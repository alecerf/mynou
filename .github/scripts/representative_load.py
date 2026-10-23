"""Finite native mixed-load measurements for #342; no product capacity limit."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import subprocess

from torrent_load import bsd_time

TEST = "torrent::load_tests::representative::representative_load_report"


def measurement(text, retained, seconds):
    reports = re.findall(r"^MYNOU_REPRESENTATIVE_LOAD (\{[^\n]+\})$", text, re.M)
    if len(reports) != 1 or not re.search(r"test result: ok\. 1 passed; 0 failed;", text):
        raise ValueError("Expected exactly one successful mixed-load measurement")
    row = json.loads(reports[0])
    if (int(row["retained_initial"]) != retained or int(row["elapsed_us"]) < seconds * 1_000_000
            or int(row["rounds"]) < 1 or int(row["active_seed_torrents"]) != 8
            or int(row["concurrent_acquisitions"]) != 2
            or int(row["retained_final"]) != retained + 2 * int(row["rounds"])
            or int(row["uploaded_payload_bytes"]) != (8 << 20) * int(row["rounds"])
            or int(row["downloaded_payload_bytes"]) != (2 << 20) * int(row["rounds"])
            or row["restart_preserved_pause_priority_payload"] is not True):
        raise ValueError("Mixed-load report does not match the requested workload")
    return row


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("harness", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--smoke", action="store_true", help="16 retained, 2 seconds; harness check only")
    args = parser.parse_args()
    if (platform.system(), platform.machine()) != ("Darwin", "arm64"):
        parser.error("measurements require native macOS arm64")
    harness = args.harness.resolve(strict=True)
    retained, seconds = (16, 2) if args.smoke else (500, 600)
    args.output.mkdir(parents=True, exist_ok=True)
    environment = {
        "commit": subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip(),
        "dirty": bool(subprocess.check_output(["git", "status", "--porcelain", "--untracked-files=normal"], text=True)),
        "harness_sha256": hashlib.sha256(harness.read_bytes()).hexdigest(),
        "os": platform.platform(), "cpus": os.cpu_count(),
        "rustc": subprocess.check_output(["rustc", "--version"], text=True).strip(),
        "retained": retained, "seconds": seconds, "smoke": args.smoke,
        "method": "native Client, two loopback WireGuard owners, 8 inbound consumers, 2 acquisitions per round, one populated restart; whole-process CPU/RSS/FD includes fixture peers",
        "qualification": "partial: no automation/API/provider/NAT-PMP qualification",
    }
    (args.output / "environment.json").write_text(json.dumps(environment, indent=2) + "\n")
    env = dict(os.environ, TMPDIR=str(Path("/private/tmp").resolve(strict=True)),
               MYNOU_REPRESENTATIVE_SECONDS=str(seconds), MYNOU_REPRESENTATIVE_RETAINED=str(retained))
    timing = args.output / "process.time"
    log = args.output / "process.log"
    command = ["/usr/bin/time", "-l", "-o", str(timing), str(harness), TEST,
               "--exact", "--nocapture", "--test-threads=1"]
    with log.open("w") as stream:
        subprocess.run(command, env=env, stdout=stream, stderr=subprocess.STDOUT,
                       check=True, timeout=seconds + 900)
    row = measurement(log.read_text(), retained, seconds)
    row.update(bsd_time(timing.read_text()))
    (args.output / "results.json").write_text(json.dumps(row, indent=2) + "\n")
    summary = (f"Mixed-load {'smoke' if args.smoke else 'measurement'}: {row['retained_initial']} retained initially, "
               f"{row['rounds']} verified rounds, {int(row['elapsed_us']) / 1e6:.2f} s.\n\n"
               f"Payload uploaded/downloaded: {int(row['uploaded_payload_bytes']) / 2**20:.0f} / "
               f"{int(row['downloaded_payload_bytes']) / 2**20:.0f} MiB. "
               f"Listing p95/max: {int(row['page_p95_us']) / 1000:.2f} / {int(row['page_max_us']) / 1000:.2f} ms. "
               f"Restart plus selected seed rehash: {int(row['restart_rehash_us']) / 1e6:.2f} s. "
               f"Whole-process peak RSS: {row['peak_rss_bytes'] / 2**20:.2f} MiB.\n\n"
               "This measures a finite workload, not an inventory ceiling or a full #342 pass. "
               "Listing is the native client page operation, not HTTP API latency.\n")
    (args.output / "summary.md").write_text(summary)
    print(summary, end="")


if __name__ == "__main__":
    main()

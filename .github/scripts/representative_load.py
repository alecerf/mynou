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


def measurement(text, retained, seconds, download_limit=2, seed_limit=0, engine=False):
    reports = re.findall(r"^MYNOU_REPRESENTATIVE_LOAD (\{[^\n]+\})$", text, re.M)
    if len(reports) != 1 or not re.search(r"test result: ok\. 1 passed; 0 failed;", text):
        raise ValueError("Expected exactly one successful mixed-load measurement")
    row = json.loads(reports[0])
    added = 1 if engine else 0
    if engine:
        required = ["api_samples", "api_p95_us", "api_max_us", "imports", "import_max_us",
                    "in_flight_restarts", "port_changes", "generation_changes", "natpmp_requests",
                    "protected_source_requests", "catalog_requests", "monitoring_samples", "monitoring_lag_max_us", "protected_feed_successes"]
        if (row.get("full_engine") is not True or any(int(row.get(key, "0")) <= 0 for key in required
                if seconds > 2 or key not in ("port_changes", "generation_changes"))
                or int(row["imports"]) != 2 * int(row["rounds"])
                or int(row["monitoring_samples"]) != int(row["imports"])
                or int(row.get("cross_seed_added", "0")) != 1):
            raise ValueError("Full Engine report is missing observed automation/API/incident evidence")
    if (int(row["retained_initial"]) != retained or int(row["elapsed_us"]) < seconds * 1_000_000
            or int(row["rounds"]) < 1 or int(row["active_seed_torrents"]) != 8
            or int(row["concurrent_acquisitions"]) != 2
            or int(row["download_queue_limit"]) != download_limit
            or int(row["seed_queue_limit"]) != seed_limit
            or int(row["retained_final"]) != retained + 2 * int(row["rounds"]) + added
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
    parser.add_argument("--engine", action="store_true", help="run full Engine, authenticated API, automation and in-flight incidents")
    parser.add_argument("--queue-mode", choices=["default", "unlimited", "finite"], default="default",
                        help="default: 2 downloads/0 seeds; unlimited: 0/0; finite: 0/8")
    args = parser.parse_args()
    if (platform.system(), platform.machine()) != ("Darwin", "arm64"):
        parser.error("measurements require native macOS arm64")
    harness = args.harness.resolve(strict=True)
    retained, seconds = (16, 2) if args.smoke else (500, 600)
    download_limit, seed_limit = {"default": (2, 0), "unlimited": (0, 0), "finite": (0, 8)}[args.queue_mode]
    if args.output.exists() and any(args.output.iterdir()):
        parser.error("output directory must be empty to preserve earlier evidence")
    args.output.mkdir(parents=True, exist_ok=True)
    environment = {
        "commit": subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip(),
        "dirty": bool(subprocess.check_output(["git", "status", "--porcelain", "--untracked-files=normal"], text=True)),
        "harness_sha256": hashlib.sha256(harness.read_bytes()).hexdigest(),
        "os": platform.platform(), "cpus": os.cpu_count(),
        "rustc": subprocess.check_output(["rustc", "--version"], text=True).strip(),
        "retained": retained, "seconds": seconds, "smoke": args.smoke,
        "queue_mode": args.queue_mode, "download_queue_limit": download_limit, "seed_queue_limit": seed_limit,
        "method": (("native Engine, authenticated HTTP API, protected source, media imports and monitoring, NAT-PMP and generation changes; " if args.engine else "native Client; ") +
                   "two loopback WireGuard owners, 8 inbound consumers, 2 acquisitions per round, populated restart; whole-process CPU/RSS/FD includes fixture peers"),
        "engine": args.engine,
        "qualification": ("synthetic full Engine extension; no owner workload/budget or provider acceptance" if args.engine
                          else "partial: no automation/API/provider/NAT-PMP qualification"),
    }
    (args.output / "environment.json").write_text(json.dumps(environment, indent=2) + "\n")
    env = dict(os.environ, TMPDIR=str(Path("/private/tmp").resolve(strict=True)),
               MYNOU_REPRESENTATIVE_SECONDS=str(seconds), MYNOU_REPRESENTATIVE_RETAINED=str(retained),
               MYNOU_REPRESENTATIVE_DOWNLOAD_LIMIT=str(download_limit), MYNOU_REPRESENTATIVE_SEED_LIMIT=str(seed_limit))
    env.pop("MYNOU_REPRESENTATIVE_ENGINE", None)
    if args.engine:
        env["MYNOU_REPRESENTATIVE_ENGINE"] = "1"
    timing = args.output / "process.time"
    log = args.output / "process.log"
    command = ["/usr/bin/time", "-l", "-o", str(timing), str(harness), TEST,
               "--exact", "--nocapture", "--test-threads=1"]
    with log.open("w") as stream:
        subprocess.run(command, env=env, stdout=stream, stderr=subprocess.STDOUT,
                       check=True, timeout=seconds + 900)
    row = measurement(log.read_text(), retained, seconds, download_limit, seed_limit, args.engine)
    row.update(bsd_time(timing.read_text()))
    (args.output / "results.json").write_text(json.dumps(row, indent=2) + "\n")
    summary = (f"Mixed-load {'smoke' if args.smoke else 'measurement'}: {row['retained_initial']} retained initially, "
               f"{row['rounds']} verified rounds, {int(row['elapsed_us']) / 1e6:.2f} s.\n\n"
               f"Payload uploaded/downloaded: {int(row['uploaded_payload_bytes']) / 2**20:.0f} / "
               f"{int(row['downloaded_payload_bytes']) / 2**20:.0f} MiB. "
               f"Combined status/diagnostic probe p95/max: {int(row['page_p95_us']) / 1000:.2f} / {int(row['page_max_us']) / 1000:.2f} ms. "
               f"Restart plus selected seed rehash: {int(row['restart_rehash_us']) / 1e6:.2f} s. "
               f"Whole-process peak RSS: {row['peak_rss_bytes'] / 2**20:.2f} MiB.\n\n"
               "This measures a finite workload, not an inventory ceiling or a full #342 pass. "
               "Probe timing includes the native client page, descriptor sampling and consumer-worker locks; it is not isolated page or HTTP API latency.\n")
    if args.engine:
        summary += (f"\nFull Engine HTTP API samples: {row['api_samples']}, p95/max: "
                    f"{int(row['api_p95_us']) / 1000:.2f} / {int(row['api_max_us']) / 1000:.2f} ms. "
                    f"Verified imports: {row['imports']}; longest submit-to-ready: {int(row['import_max_us']) / 1e6:.2f} s. "
                    f"In-flight restart / NAT-PMP changes / generation changes: "
                    f"{row['in_flight_restarts']} / {row['port_changes']} / {row['generation_changes']}. "
                    f"Library monitoring observations: {row['monitoring_samples']}, maximum observed lag: "
                    f"{int(row['monitoring_lag_max_us']) / 1e6:.2f} s.\n")
    (args.output / "summary.md").write_text(summary)
    print(summary, end="")


if __name__ == "__main__":
    main()

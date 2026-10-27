"""Opt-in macOS arm64 engine-level torrent load evidence; synthetic loopback swarm, Python stdlib only."""
import argparse
import json
import os
from pathlib import Path
import platform
import re
import subprocess

TEST = "torrent::load_tests::torrent_load_report"
BYTES = 4 << 20
# torrents, seeding clients, downloader max_active and max_peers, client tunnel flow limit.
# Keep the historical 2-active/4-peer profile; current max_peers defaults to 50.
# The default tunnel flow limit is 64.
LEVELS = [
    (1, 1, 2, 4, 64),
    (8, 6, 2, 4, 64),
    (16, 4, 4, 8, 64),
    # 16 active torrents x 4 peers: configured peer demand equals the flow limit.
    (16, 4, 16, 4, 64),
    (16, 4, 16, 4, 16),
]


def bsd_time(text):
    """CPU seconds and peak RSS bytes from macOS `/usr/bin/time -l`."""
    times = re.search(r"([0-9.]+) real\s+([0-9.]+) user\s+([0-9.]+) sys", text)
    peak = re.search(r"^\s*(\d+)\s+maximum resident set size\s*$", text, re.M)
    if not times or not peak:
        raise ValueError("macOS time did not report times and peak RSS")
    return {"real_s": float(times[1]), "user_s": float(times[2]),
            "sys_s": float(times[3]), "peak_rss_bytes": int(peak[1])}


def table(results):
    lines = ["| Torrents | Seeds | max_active / max_peers | Flow limit | Verified | MiB | Transfer s | MiB/s | Transfer CPU s | Peak RSS MiB | Running max | Flows max (total / torrent) | Refused connects | Torrent p50 / max s |",
             "| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- | ---: |"]
    for row in results:
        moved = int(row["verified"]) * int(row["bytes_per_torrent"]) / 2**20
        seconds = int(row["transfer_us"]) / 1e6
        refused = ", ".join(f"{count} {message}" for message, count in row["refused"].items()) or "none"
        values = [row["torrents"], row["seeds"], f'{row["max_active"]} / {row["max_peers"]}',
                  row["flow_limit"], row["verified"], f"{moved:.0f}", f"{seconds:.2f}",
                  f"{moved / seconds:.1f}", row["transfer_cpu_s"],
                  f'{row["peak_rss_bytes"] / 2**20:.1f}', row["max_running"],
                  f'{row["max_open_flows"]} / {row["max_torrent_flows"]}', refused,
                  f'{int(row["complete_p50_us"]) / 1e6:.2f} / {int(row["complete_max_us"]) / 1e6:.2f}']
        lines.append("| " + " | ".join(str(value) for value in values) + " |")
    return "\n".join(lines) + "\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("harness", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--smoke", action="store_true", help="256 KiB per torrent; not load evidence")
    args = parser.parse_args()
    if (platform.system(), platform.machine()) != ("Darwin", "arm64"):
        parser.error("evidence requires native macOS arm64")
    harness = args.harness.resolve(strict=True)
    args.output.mkdir(parents=True, exist_ok=True)
    size = 256 << 10 if args.smoke else BYTES
    metadata = {
        "commit": subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip(),
        "os": platform.platform(),
        "cpus": os.cpu_count(),
        "rustc": subprocess.check_output(["rustc", "--version"], text=True).strip(),
        "smoke": args.smoke,
        "github_run": os.environ.get("GITHUB_RUN_ID"),
        "method": "release lib test harness; one process per level; native seeding and downloading clients and both tunnel owners in-process over loopback UDP; /usr/bin/time -l; transfer CPU and RSS from ps",
    }
    (args.output / "environment.json").write_text(json.dumps(metadata, indent=2) + "\n")
    parent = Path(os.environ.get("RUNNER_TEMP", "/private/tmp")).resolve(strict=True)
    results = []
    for torrents, seeds, active, peers, flows in LEVELS:
        label = f"t{torrents}-s{seeds}-a{active}-p{peers}-f{flows}"
        log = args.output / f"{label}.log"
        timing = args.output / f"{label}.time"
        env = dict(os.environ, TMPDIR=str(parent), MYNOU_TORRENT_LOAD_TORRENTS=str(torrents),
                   MYNOU_TORRENT_LOAD_SEEDS=str(seeds), MYNOU_TORRENT_LOAD_ACTIVE=str(active),
                   MYNOU_TORRENT_LOAD_PEERS=str(peers), MYNOU_TORRENT_LOAD_FLOWS=str(flows),
                   MYNOU_TORRENT_LOAD_BYTES=str(size))
        command = ["/usr/bin/time", "-l", "-o", str(timing), str(harness), TEST,
                   "--exact", "--nocapture", "--test-threads=1"]
        print(label, flush=True)
        with log.open("w") as stream:
            subprocess.run(command, env=env, stdout=stream, stderr=subprocess.STDOUT,
                           check=True, timeout=900)
        text = log.read_text()
        if "1 passed" not in text:
            raise ValueError(f"The exact load test did not run in {log}")
        report = re.findall(r"MYNOU_TORRENT_LOAD (\{[^\n]+\})", text)
        if len(report) != 1:
            raise ValueError(f"Expected one measurement in {log}")
        row = json.loads(report[0])
        row.update(bsd_time(timing.read_text()))
        results.append(row)
        (args.output / "results.json").write_text(json.dumps(results, indent=2) + "\n")
    summary = table(results)
    (args.output / "summary.md").write_text(summary)
    print(summary, end="")


if __name__ == "__main__":
    main()

"""Opt-in macOS arm64 tunnel load evidence; synthetic loopback flows, Python stdlib only."""
import argparse
import json
import os
from pathlib import Path
import platform
import re
import subprocess

TEST = "tunnel_http::tests::load::tunnel_load_report"
# The default client flow limit is 64: the last level exceeds it.
LEVELS = [1, 8, 32, 64, 96]
BYTES = 4 << 20
MODES = ["concurrent", "serial"]


def bsd_time(text):
    """CPU seconds and peak RSS bytes from macOS `/usr/bin/time -l`."""
    times = re.search(r"([0-9.]+) real\s+([0-9.]+) user\s+([0-9.]+) sys", text)
    peak = re.search(r"^\s*(\d+)\s+maximum resident set size\s*$", text, re.M)
    if not times or not peak:
        raise ValueError("macOS time did not report times and peak RSS")
    return {"real_s": float(times[1]), "user_s": float(times[2]),
            "sys_s": float(times[3]), "peak_rss_bytes": int(peak[1])}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("harness", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--smoke", action="store_true", help="64 KiB per flow; not load evidence")
    args = parser.parse_args()
    if (platform.system(), platform.machine()) != ("Darwin", "arm64"):
        parser.error("evidence requires native macOS arm64")
    harness = args.harness.resolve(strict=True)
    args.output.mkdir(parents=True, exist_ok=True)
    size = 64 << 10 if args.smoke else BYTES
    metadata = {
        "commit": subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip(),
        "os": platform.platform(),
        "cpus": os.cpu_count(),
        "rustc": subprocess.check_output(["rustc", "--version"], text=True).strip(),
        "smoke": args.smoke,
        "github_run": os.environ.get("GITHUB_RUN_ID"),
        "method": "release lib test harness; one process per level; both tunnel owners in-process over loopback UDP; /usr/bin/time -l",
    }
    (args.output / "environment.json").write_text(json.dumps(metadata, indent=2) + "\n")
    parent = Path(os.environ.get("RUNNER_TEMP", "/private/tmp")).resolve(strict=True)
    results = []
    for mode in MODES:
        for flows in LEVELS:
            label = f"{mode}-{flows}"
            log = args.output / f"{label}.log"
            timing = args.output / f"{label}.time"
            env = dict(os.environ, TMPDIR=str(parent), MYNOU_TUNNEL_LOAD_FLOWS=str(flows),
                       MYNOU_TUNNEL_LOAD_BYTES=str(size), MYNOU_TUNNEL_LOAD_CONNECT=mode)
            command = ["/usr/bin/time", "-l", "-o", str(timing), str(harness), TEST,
                       "--exact", "--nocapture", "--test-threads=1"]
            print(label, flush=True)
            with log.open("w") as stream:
                subprocess.run(command, env=env, stdout=stream, stderr=subprocess.STDOUT,
                               check=True, timeout=600)
            text = log.read_text()
            if "1 passed" not in text:
                raise ValueError(f"The exact load test did not run in {log}")
            report = re.findall(r"MYNOU_TUNNEL_LOAD (\{[^\n]+\})", text)
            if len(report) != 1:
                raise ValueError(f"Expected one measurement in {log}")
            row = json.loads(report[0])
            row.update(bsd_time(timing.read_text()))
            results.append(row)
            (args.output / "results.json").write_text(json.dumps(results, indent=2) + "\n")
    lines = ["| Connect | Flows | Completed | Refused | MiB moved | Transfer s | MiB/s | CPU s (user+sys) | Peak RSS MiB | Flow p50 / max ms | Connect max ms |",
             "| --- | ---: | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |"]
    for row in results:
        moved = int(row["completed"]) * int(row["bytes_per_flow"]) / 2**20
        seconds = int(row["transfer_us"]) / 1e6
        refused = ", ".join(f"{count} {message}" for message, count in row["refused"].items()) or "none"
        values = [row["connect"], row["flows"], row["completed"], refused, f"{moved:.0f}",
                  f"{seconds:.3f}", f"{moved / seconds:.1f}", f'{row["user_s"] + row["sys_s"]:.2f}',
                  f'{row["peak_rss_bytes"] / 2**20:.1f}',
                  f'{int(row["flow_p50_us"]) / 1000:.0f} / {int(row["flow_max_us"]) / 1000:.0f}',
                  f'{int(row["connect_max_us"]) / 1000:.1f}']
        lines.append("| " + " | ".join(values) + " |")
    (args.output / "summary.md").write_text("\n".join(lines) + "\n")
    print("\n".join(lines))


if __name__ == "__main__":
    main()

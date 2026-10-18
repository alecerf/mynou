"""Opt-in macOS arm64 inventory evidence; synthetic stores and Python stdlib only."""
import argparse
import json
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import tempfile

# Each baseline exceeds the former total count/bytes with the fixture shape.
# Transfers had no count cap; these sizes exercise more than one listing page.
CASES = {
    "jobs": ("store::tests", 16000),
    "library": ("store::tests", 16000),
    "series": ("series::tests", 256),
    "requesters": ("requesters::index_tests", 11000),
    "feeds": ("indexers::feed::storage::tests", 11000),
    "sources": ("indexers::policy::storage::tests", 16000),
    "irc": ("irc::persistence::tests", 1100),
    "transfers": ("torrent::index::growth_report", 1100),
}


def peak_bytes(text):
    match = re.search(r"^\s*(\d+)\s+maximum resident set size\s*$", text, re.M)
    if not match:
        raise ValueError("macOS time did not report peak RSS")
    return int(match[1])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("harness", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--smoke", action="store_true", help="200 rows per case; not capacity evidence")
    parser.add_argument("--case", choices=CASES, action="append")
    args = parser.parse_args()
    if (platform.system(), platform.machine()) != ("Darwin", "arm64"):
        parser.error("evidence requires native macOS arm64")
    harness = args.harness.resolve(strict=True)
    args.output.mkdir(parents=True, exist_ok=True)
    metadata = {
        "commit": subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip(),
        "os": platform.platform(),
        "rustc": subprocess.check_output(["rustc", "--version"], text=True).strip(),
        "smoke": args.smoke,
        "github_run": os.environ.get("GITHUB_RUN_ID"),
        "method": "release lib test harness; separate build/read processes; /usr/bin/time -l peak bytes; logical file sizes; no network workers",
    }
    (args.output / "environment.json").write_text(json.dumps(metadata, indent=2) + "\n")
    results = []
    # resolve removes /var -> /private/var: product rejects symlinked parents.
    parent = Path(os.environ.get("RUNNER_TEMP", "/private/tmp")).resolve(strict=True)
    with tempfile.TemporaryDirectory(prefix="mynou-growth-", dir=parent) as temporary:
        for name in args.case or CASES:
            module, baseline = CASES[name]
            for size in ([200] if args.smoke else [baseline, baseline * 2, baseline * 4]):
                directory = Path(temporary) / f"{name}-{size}"
                directory.mkdir(mode=0o700)
                for phase in ("build", "read"):
                    label = f"{name}-{size}-{phase}"
                    log = args.output / f"{label}.log"
                    timing = args.output / f"{label}.time"
                    env = dict(os.environ, TMPDIR=str(parent), MYNOU_GROWTH_CASE=name,
                               MYNOU_GROWTH_SIZE=str(size), MYNOU_GROWTH_DIR=str(directory),
                               MYNOU_GROWTH_PHASE=phase)
                    command = ["/usr/bin/time", "-l", "-o", str(timing), str(harness),
                               f"{module}::inventory_growth_report", "--exact", "--nocapture",
                               "--ignored", "--test-threads=1"]
                    print(label, flush=True)
                    with log.open("w") as stream:
                        subprocess.run(command, env=env, stdout=stream, stderr=subprocess.STDOUT,
                                       check=True, timeout=1200)
                    report = re.findall(r"MYNOU_GROWTH (\{[^\n]+\})", log.read_text())
                    if len(report) != 1:
                        raise ValueError(f"Expected one measurement in {log}")
                    row = json.loads(report[0])
                    row["peak_rss_bytes"] = peak_bytes(timing.read_text())
                    row["disk_bytes"] = sum(p.stat().st_size for p in directory.rglob("*") if p.is_file())
                    results.append(row)
                    (args.output / "results.json").write_text(json.dumps(results, indent=2) + "\n")
                # Only this driver's generated fixture is removed, one size at a time.
                shutil.rmtree(directory)
    lines = ["| Store | Rows | Disk MiB | Open ms | Page ms | Pass ms | Update ms | Walk ms | Read peak MiB | Build peak MiB |",
             "| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |"]
    for build, read in zip(results[::2], results[1::2]):
        values = [read["case"], read["size"], f'{read["disk_bytes"] / 2**20:.2f}']
        values += [f'{int(read[key]) / 1000:.3f}' for key in ("open_us", "page_us", "pass_us", "update_us", "walk_us")]
        values += [f'{row["peak_rss_bytes"] / 2**20:.2f}' for row in (read, build)]
        lines.append("| " + " | ".join(values) + " |")
    (args.output / "summary.md").write_text("\n".join(lines) + "\n")
    print(json.dumps(results, indent=2))


if __name__ == "__main__":
    main()

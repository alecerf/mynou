"""CI-only Rust test scheduler, using Python's standard library and Cargo JSON."""

import argparse
from concurrent.futures import ThreadPoolExecutor, as_completed
from dataclasses import dataclass
import json
import math
import os
from pathlib import Path
import re
import signal
import subprocess
import sys
import time


MAX_MANIFEST_BYTES = 32 * 1024 * 1024
MAX_LOG_BYTES = 16 * 1024 * 1024
RESULT = re.compile(
    r"test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored; "
    r"\d+ measured; (\d+) filtered out; finished in ([0-9.]+)s"
)


@dataclass(frozen=True)
class Target:
    source: str
    executable: Path


def discover(manifest, root):
    if manifest.stat().st_size > MAX_MANIFEST_BYTES:
        raise ValueError("Cargo test manifest exceeds 32 MiB")
    targets = {}
    successful = False
    for line in manifest.read_text().splitlines():
        message = json.loads(line)
        reason = message.get("reason")
        if reason == "build-finished":
            successful = message.get("success") is True
        if reason != "compiler-artifact" or message.get("profile", {}).get("test") is not True:
            continue
        if not message.get("executable"):
            raise ValueError("Cargo reported a test target without an executable")
        executable = Path(message["executable"]).resolve(strict=True)
        executable.relative_to(root / "target")
        if not executable.is_file() or not os.access(executable, os.X_OK):
            raise ValueError("Cargo test executable is not a runnable regular file")
        source = Path(message["target"]["src_path"]).resolve(strict=True)
        source = source.relative_to(root).as_posix()
        target = Target(source, executable)
        previous = targets.setdefault(source, target)
        if previous != target:
            raise ValueError("Cargo reported conflicting executables for one test target")
    if not successful or not targets or len(targets) > 128:
        raise ValueError("A successful Cargo test build with 1–128 harnesses is required")
    if not {"src/lib.rs", "src/main.rs"}.issubset(targets):
        raise ValueError("Cargo test manifest is missing the library or binary harness")
    return list(targets.values())


def weights(paths):
    result = {}
    for path in paths:
        if not path.is_file() or path.stat().st_size > 1024 * 1024:
            continue
        try:
            data = json.loads(path.read_text())
            for source, seconds in data.get("targets", {}).items():
                if isinstance(seconds, (int, float)) and math.isfinite(seconds) and 0 <= seconds <= 3600:
                    result[source] = float(seconds)
        except (ValueError, TypeError, AttributeError):
            # Timing history only affects ordering; it never selects or skips targets.
            continue
    return result


def terminate(process):
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    process.wait()


def execute(target, root, log, threads, timeout):
    started = time.monotonic()
    timed_out = False
    with log.open("wb") as stream:
        process = subprocess.Popen(
            [str(target.executable), "--test-threads", str(threads), "--show-output", "--color", "never"],
            cwd=root,
            stdout=stream,
            stderr=subprocess.STDOUT,
            start_new_session=True,
        )
        try:
            code = process.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            timed_out = True
            terminate(process)
            code = process.returncode
        finally:
            if process.poll() is None:
                terminate(process)
    with log.open("rb") as stream:
        output = stream.read(MAX_LOG_BYTES + 1)
    oversized = len(output) > MAX_LOG_BYTES
    text = output[:MAX_LOG_BYTES].decode("utf-8", errors="replace")
    matches = RESULT.findall(text)
    counts = None
    if len(matches) == 1:
        state, passed, failed, ignored, filtered, _ = matches[0]
        counts = {
            "passed": int(passed), "failed": int(failed),
            "ignored": int(ignored), "filtered": int(filtered),
        }
        summary_ok = state == "ok" and not any(counts[key] for key in ("failed", "ignored", "filtered"))
    else:
        summary_ok = False
    return {
        "source": target.source, "seconds": round(time.monotonic() - started, 3),
        "exit_code": code, "timed_out": timed_out, "log": log.name,
        "success": code == 0 and not timed_out and not oversized and summary_ok,
        "counts": counts,
    }


def schedule(targets, root, output, workers, threads, timeout, history):
    output.mkdir(parents=True, exist_ok=False)
    ordered = sorted(targets, key=lambda target: (-history.get(target.source, 0), target.source))
    started = time.monotonic()
    results = []
    with ThreadPoolExecutor(max_workers=workers) as pool:
        pending = {
            pool.submit(execute, target, root, output / f"{index:03d}.log", threads, timeout): target
            for index, target in enumerate(ordered)
        }
        for future in as_completed(pending):
            target = pending[future]
            try:
                result = future.result()
            except Exception as error:
                result = {"source": target.source, "seconds": 0, "success": False, "error": str(error), "counts": None}
            results.append(result)
            print(f"{target.source}: {'passed' if result['success'] else 'FAILED'} ({result['seconds']:.3f}s)", flush=True)
    return {
        "schema_version": 1, "seconds": round(time.monotonic() - started, 3),
        "workers": workers, "threads_per_harness": threads,
        "success": len(results) == len(targets) and all(row["success"] for row in results),
        "results": sorted(results, key=lambda row: row["source"]),
        "targets": {row["source"]: row["seconds"] for row in results},
        "counts": {
            key: sum(row["counts"][key] for row in results if row["counts"])
            for key in ("passed", "failed", "ignored", "filtered")
        },
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("manifest", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--workers", type=int, default=min(4, os.cpu_count() or 1))
    parser.add_argument("--threads", type=int, default=min(2, os.cpu_count() or 1))
    parser.add_argument("--timeout", type=int, default=180)
    args = parser.parse_args()
    if not 1 <= args.workers <= 8 or not 1 <= args.threads <= 8 or args.workers * args.threads > 16:
        parser.error("Concurrency must use 1–8 workers/threads and at most 16 harness test threads")
    if not 1 <= args.timeout <= 1800:
        parser.error("Per-target timeout must be 1–1800 seconds")
    root = Path.cwd().resolve()
    cached = root / "target/debug/mynou-ci-test-timings.json"
    history = weights([root / ".github/scripts/test_durations.json", cached])
    targets = discover(args.manifest, root)
    print(f"Running all {len(targets)} Cargo test harnesses: {args.workers} processes, {args.threads} threads per harness", flush=True)
    report = schedule(targets, root, args.output, args.workers, args.threads, args.timeout, history)
    encoded = json.dumps(report, indent=2) + "\n"
    (args.output / "report.json").write_text(encoded)
    for row in report["results"]:
        print(f"::group::{row['source']} ({row['seconds']:.3f}s)", flush=True)
        if row.get("log"):
            with (args.output / row["log"]).open("rb") as log:
                sys.stdout.write(log.read(MAX_LOG_BYTES).decode("utf-8", errors="replace"))
        else:
            print(row["error"])
        print("\n::endgroup::", flush=True)
    summary = [
        "### Rust test execution", "",
        f"All **{len(targets)}** Cargo harnesses ran with **{args.workers}** processes and **{args.threads}** threads per harness.",
        f"Elapsed: **{report['seconds']:.3f}s**. Passed: **{report['counts']['passed']}**. Failed: **{report['counts']['failed']}**. Ignored: **{report['counts']['ignored']}**.",
        "", "| Target | Seconds | Result |", "| --- | ---: | --- |",
    ]
    for row in sorted(report["results"], key=lambda row: -row["seconds"]):
        summary.append(f"| {row['source']} | {row['seconds']:.3f} | {'Passed' if row['success'] else 'Failed'} |")
    if os.environ.get("GITHUB_STEP_SUMMARY"):
        with Path(os.environ["GITHUB_STEP_SUMMARY"]).open("a") as stream:
            stream.write("\n".join(summary) + "\n")
    if report["success"]:
        cached.write_text(encoded)
    print(f"All-target result: {'passed' if report['success'] else 'FAILED'}; {report['counts']['passed']} passed, {report['counts']['failed']} failed; {report['seconds']:.3f}s", flush=True)
    return 0 if report["success"] else 1


if __name__ == "__main__":
    sys.exit(main())

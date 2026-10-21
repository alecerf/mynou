"""Required real macOS route changes, confined to a disposable GitHub runner.

Only the fixture's 127.0.0.2 loopback alias and /32 route are changed. Never
change a default route, VPN service or anything on an operator's machine.
"""
import argparse
import json
import os
from pathlib import Path
import platform
import signal
import subprocess
import time

import host_io_observer as observer

DESTINATION = "127.0.0.2"
FIXTURES = tuple(name for name in observer.FIXTURES if "::host_route_loss_" in name)


def require_disposable_runner():
    if ((platform.system(), platform.machine()) != ("Darwin", "arm64")
            or os.environ.get("GITHUB_ACTIONS") != "true"
            or os.environ.get("RUNNER_ENVIRONMENT") != "github-hosted"):
        raise ValueError("real route changes require a disposable GitHub-hosted macOS arm64 runner")


def route(*arguments):
    return subprocess.check_output(["sudo", "-n", "/sbin/route", "-n", *arguments],
                                   text=True, stderr=subprocess.STDOUT, timeout=10)


def alias(add):
    arguments = ["alias", DESTINATION, "netmask", "255.255.255.255"] if add else ["-alias", DESTINATION]
    return subprocess.check_output(["sudo", "-n", "/sbin/ifconfig", "lo0", *arguments],
                                   text=True, stderr=subprocess.STDOUT, timeout=10)


def ensure_unused():
    text = route("get", "-host", DESTINATION)
    fields = dict(line.strip().split(":", 1) for line in text.splitlines() if ":" in line)
    if fields.get("destination", "").strip() == DESTINATION:
        raise ValueError("refusing to replace an existing fixture host route")
    interfaces = subprocess.check_output(["/sbin/ifconfig", "lo0"], text=True, timeout=10)
    if any(line.split()[:2] == ["inet", DESTINATION] for line in interfaces.splitlines()):
        raise ValueError("refusing to replace an existing loopback alias")


def snapshot(output, name, rejected):
    text = route("get", "-host", DESTINATION)
    (output / (name + ".txt")).write_text(text)
    fields = dict(line.strip().split(":", 1) for line in text.splitlines() if ":" in line)
    if (fields.get("destination", "").strip() != DESTINATION
            or fields.get("interface", "").strip() != "lo0"
            or ("REJECT" in fields.get("flags", "")) != rejected):
        raise ValueError("OS route does not match the requested fixture state")


def exercise(executable, library, output, fixture):
    output.mkdir()
    trace = output / "trace.jsonl"
    trace.touch(mode=0o600, exist_ok=False)
    control = output / "control"
    control.mkdir(mode=0o700)
    env = {**os.environ, "DYLD_INSERT_LIBRARIES": str(library),
           "MYNOU_HOST_IO_TRACE": str(trace), "MYNOU_OS_ROUTE_CONTROL": str(control)}
    process = None
    owned_alias = False
    completed = []
    try:
        # macOS requires a local alias before a socket can bind this address.
        # The alias creates its host route. Refuse any pre-existing state.
        ensure_unused()
        alias(True)
        owned_alias = True
        snapshot(output, "before", False)
        with (output / "fixture.log").open("wb") as log:
            process = subprocess.Popen([executable, "--exact", fixture, "--nocapture"],
                                       env=env, stdout=log, stderr=subprocess.STDOUT)
            deadline = time.monotonic() + 120
            for state in ("lost", "restored"):
                while not (control / ("request-" + state)).is_file():
                    if process.poll() is not None or time.monotonic() >= deadline:
                        raise ValueError("route fixture stopped or timed out before " + state)
                    time.sleep(0.01)
                if state == "lost":
                    route("change", "-host", DESTINATION, "127.0.0.1", "-reject")
                else:
                    route("change", "-host", DESTINATION, "-interface", "lo0")
                snapshot(output, state, state == "lost")
                (control / ("ack-" + state)).write_text("ready")
                completed.append(state)
            if process.wait(timeout=max(1, deadline - time.monotonic())):
                raise ValueError("real-route fixture failed; see fixture.log")
        counts = observer.validate(observer.read_trace(trace))
        return {"states": completed, "host_io": counts}
    finally:
        if process is not None and process.poll() is None:
            process.kill()
            process.wait(timeout=10)
        if owned_alias:
            # Remove our changed host route before removing the owned alias.
            # Attempt both cleanups even if route deletion fails.
            try:
                route("delete", "-host", DESTINATION)
            finally:
                alias(False)
            (output / "cleanup.txt").write_text("Removed the owned 127.0.0.2/32 route and loopback alias.\n")


def interrupted(signum, frame):
    raise InterruptedError("route supervisor interrupted")


def run(build, library, output):
    require_disposable_runner()
    signal.signal(signal.SIGTERM, interrupted)
    output.mkdir(parents=True, exist_ok=False)
    executable = observer.harness(build)
    listing = subprocess.check_output([executable, "--list"], text=True)
    if len(FIXTURES) != 2 or any(name + ": test\n" not in listing for name in FIXTURES):
        raise ValueError("missing exact required route fixtures")
    if not library.is_file():
        raise ValueError("missing host-I/O observation library")
    summary = {"platform": "macOS arm64", "destination": DESTINATION,
               "mechanism": "OS host route changed to REJECT and restored on lo0", "fixtures": {}}
    for index, fixture in enumerate(FIXTURES):
        summary["fixtures"][fixture] = exercise(executable, library, output / str(index), fixture)
        print("Observed real OS route loss and restoration: " + fixture, flush=True)
    (output / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--build-json", type=Path, required=True)
    parser.add_argument("--library", type=lambda s: Path(s).resolve(), required=True)
    parser.add_argument("--output", type=lambda s: Path(s).resolve(), required=True)
    args = parser.parse_args()
    run(args.build_json, args.library, args.output)


if __name__ == "__main__":
    main()

"""Runs Mynou's WireGuard interoperability test against the pinned wireguard-go peer.

The peer runs in its own `--network none` namespace with only NET_ADMIN and the TUN
device; the test binary, built offline as the unprivileged tester, joins that
namespace without capabilities. Containers are always removed. The run fails unless
every required scenario reports a pass. Python std only; see
docs/wireguard-interop.md for the design and its limits."""
import argparse
import json
import os
from pathlib import Path
import re
import secrets
import subprocess
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parent.parent
DOCKERFILE = ROOT / "engineering/Dockerfile.wireguard-peer"
SCENARIOS = (
    "mynou-initiator",
    "peer-initiator",
    "psk-mynou-initiator",
    "psk-peer-initiator",
    "psk-mismatch-mynou-initiator",
    "psk-mismatch-peer-initiator",
    "wrong-key-mynou-pins",
    "wrong-key-peer-pins",
    "unknown-static-mynou-responder",
    "rekey-mynou",
    "rekey-peer",
    "cookie-peer-under-load",
    "cookie-mynou-under-load",
)
LIMITS = (
    "Linux/Docker only: nothing here shows native macOS behaviour, torrent-path isolation or leak protection.",
    "One peer implementation (wireguard-go at the pinned module version), not universal WireGuard compatibility.",
    "IPv4 inner and outer traffic over loopback: no real loss, reordering or path MTU.",
)
PEER_ADDRESS = "10.77.0.1/24"
UAPI = "/var/run/wireguard/wg0.sock"
PEER_LOG = "/var/log/wireguard/peer.log"
MAX_RECORD_BYTES = 1024 * 1024
MAX_FIELD = 1000
MAX_TAIL_LINES = 60
MAX_DIAGNOSTIC_BYTES = 64 * 1024
BUILD_TIMEOUT = 10 * 60
TEST_TIMEOUT = 8 * 60


def pins(dockerfile):
    """The provenance the Dockerfile pins: both images by digest and the module hashes."""
    images = re.findall(r"^FROM (\S+@sha256:[0-9a-f]{64})(?: AS \w+)?$", dockerfile, re.M)
    module = re.search(r"go install (golang\.zx2c4\.com/wireguard)@(v[0-9][\w.-]*) ", dockerfile)
    hashes = re.findall(r"'(mod|dep) (\S+) (v\S+) (h1:[A-Za-z0-9+/=]{44})'", dockerfile)
    if len(images) != 2 or not module or not hashes or hashes[0][:3] != ("mod",) + module.groups():
        raise ValueError("Dockerfile.wireguard-peer lost its digest or module pins")
    return {
        "build_image": images[0],
        "runtime_image": images[1],
        "module": f"{module[1]}@{module[2]}",
        "Go version": images[0].split(":", 1)[1].split("-", 1)[0],
        "modules": [" ".join(entry) for entry in hashes],
    }


def unique_fields(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError("Duplicate result field")
        result[key] = value
    return result


def parse_entries(text):
    """Read bounded, structurally valid evidence, including failing outcomes."""
    if len(text.encode()) > MAX_RECORD_BYTES:
        raise ValueError("Result record exceeds its bound")
    entries = []
    for line in text.splitlines():
        entry = json.loads(line, object_pairs_hook=unique_fields)
        if not isinstance(entry, dict) or set(entry) != {"scenario", "outcome", "milliseconds", "source", "observed"}:
            raise ValueError("Malformed result entry")
        if not all(isinstance(entry[key], str) and len(entry[key]) <= MAX_FIELD for key in ("scenario", "outcome", "source", "observed")):
            raise ValueError("Malformed result entry")
        if type(entry["milliseconds"]) is not int or not 0 <= entry["milliseconds"] <= 2**64 - 1:
            raise ValueError("Malformed result duration")
        if entry["outcome"] not in ("pass", "fail"):
            raise ValueError("Malformed result outcome")
        entries.append(entry)
        if len(entries) > len(SCENARIOS):
            raise ValueError("Too many result entries")
    return entries


def validate(text, required=SCENARIOS):
    """Every required scenario exactly once and passing; returns the parsed entries."""
    entries = parse_entries(text)
    names = [entry["scenario"] for entry in entries]
    if len(names) != len(set(names)):
        raise ValueError("A scenario was reported twice")
    missing = [name for name in required if name not in names]
    unknown = [name for name in names if name not in required]
    failed = [entry["scenario"] for entry in entries if entry["outcome"] != "pass"]
    if missing or unknown or failed:
        raise ValueError(f"Interoperability incomplete: missing {missing}, unknown {unknown}, failed {failed}")
    return entries


def cell(text):
    return text.replace("|", "\\|").replace("\n", " ")


def summary(provenance, environment, entries, error=None, partial=False):
    lines = ["## WireGuard interoperability with wireguard-go", ""]
    if partial:
        lines += ["**Single-scenario diagnosis: not evidence for the interoperability gate.**", ""]
    lines.append("**Result: " + ("FAILED: " + cell(error) if error else "all required scenarios passed") + "**")
    lines += ["", "| Provenance | Value |", "| --- | --- |"]
    for key, value in list(provenance.items()) + list(environment.items()):
        values = value if isinstance(value, list) else [value]
        lines.append(f"| {cell(key)} | {'<br>'.join(cell(item) for item in values)} |")
    lines += ["", "| Scenario | Outcome | Duration | Source | Observed |", "| --- | --- | --- | --- | --- |"]
    for entry in entries:
        lines.append(
            f"| {cell(entry['scenario'])} | {cell(entry['outcome'])} | {entry['milliseconds']} ms "
            f"| {cell(entry['source'])} | {cell(entry['observed'])} |"
        )
    lines += ["", "Limits:"] + [f"- {limit}" for limit in LIMITS]
    return "\n".join(lines) + "\n"


def docker(*args, timeout=120, check=True, capture=True):
    result = subprocess.run(
        ["docker", *args], timeout=timeout, check=False, text=True,
        stdout=subprocess.PIPE if capture else None, stderr=subprocess.PIPE if capture else None,
    )
    if check and result.returncode != 0:
        detail = (result.stderr or "").strip()[-2000:]
        raise RuntimeError(f"docker {args[0]} failed ({result.returncode}): {detail}")
    return result


def build_test(rust_image, volume, name):
    """Builds the test binary offline as the image's unprivileged user; returns its path."""
    # The read-only checkout needs the mount point of the target volume.
    (ROOT / "target").mkdir(exist_ok=True)
    try:
        result = docker(
            "run", "--rm", "--name", name, "--network", "none",
            "--mount", f"type=bind,src={ROOT},dst=/workspace,readonly",
            "--mount", f"type=volume,src={volume},dst=/workspace/target",
            rust_image, "cargo", "test", "--test", "wireguard_interop", "--no-run",
            "--offline", "--locked", "--message-format=json-render-diagnostics",
            timeout=BUILD_TIMEOUT,
        )
    finally:
        # Killing the Docker CLI on timeout does not kill its container.
        removed = docker("rm", "--force", "--volumes", name, check=False)
        if removed.returncode and "No such container" not in removed.stderr:
            raise RuntimeError("Build container cleanup failed")
    executables = []
    for line in result.stdout.splitlines():
        message = json.loads(line)
        if (message.get("reason") == "compiler-artifact" and message.get("profile", {}).get("test")
                and message.get("target", {}).get("name") == "wireguard_interop" and message.get("executable")):
            executables.append(message["executable"])
    if len(executables) != 1 or not executables[0].startswith("/workspace/target/"):
        raise RuntimeError("Cargo did not report exactly one wireguard_interop test binary")
    return executables[0]


def start_peer(image, name):
    docker(
        "run", "--detach", "--name", name, "--network", "none",
        "--cap-drop", "ALL", "--cap-add", "NET_ADMIN", "--device", "/dev/net/tun",
        "--read-only", "--volume", "/var/run/wireguard", "--volume", "/var/log/wireguard",
        "--security-opt", "no-new-privileges", "--pids-limit", "64", "--memory", "128m", "--cpus", "1",
        image,
    )
    deadline = time.monotonic() + 30
    while docker("exec", name, "test", "-S", UAPI, check=False).returncode != 0:
        if time.monotonic() > deadline:
            raise RuntimeError("The peer's UAPI socket did not appear")
        time.sleep(0.2)
    docker("exec", name, "ip", "addr", "add", PEER_ADDRESS, "dev", "wg0")
    docker("exec", name, "ip", "link", "set", "wg0", "up")


def environment(image, name):
    def output(*args):
        return docker(*args).stdout.strip()
    version = output("exec", name, "/usr/local/bin/wireguard-go", "--version")
    return {
        "peer image": output("image", "inspect", "--format", "{{.Id}}", image),
        "peer version": version.splitlines()[0] if version else "unknown",
        "kernel (uname -r)": output("exec", name, "uname", "-r"),
        "architecture": output("exec", name, "uname", "-m"),
    }


def peer_tail(peer):
    """Bound diagnostics before copying them; logs are data and contain no keys."""
    result = docker("exec", peer, "tail", "-c", str(MAX_DIAGNOSTIC_BYTES), PEER_LOG, check=False)
    lines = [line[:300] for line in result.stdout.splitlines()
             if "127.0.0.2:" not in line and "Routine:" not in line]
    return "\n".join(lines[-MAX_TAIL_LINES:])


def read_record(path):
    with path.open("rb") as stream:
        data = stream.read(MAX_RECORD_BYTES + 1)
    if len(data) > MAX_RECORD_BYTES:
        raise ValueError("Result record exceeds its bound")
    return data.decode("utf-8")


def run(args):
    provenance = pins(DOCKERFILE.read_text())
    run_id = f"{os.getpid()}-{secrets.token_hex(4)}"
    peer = f"mynou-wireguard-peer-{run_id}"
    tester = f"mynou-wireguard-test-{run_id}"
    executable = build_test(args.rust_image, args.target_volume, f"mynou-wireguard-build-{run_id}")
    with tempfile.TemporaryDirectory(prefix="mynou-wireguard-", dir=args.scratch) as scratch:
        scratch = Path(scratch)
        out = scratch / "out"
        out.mkdir()
        os.chmod(out, 0o777)
        diagnostics = ""
        entries, error, env = [], None, {}
        try:
            start_peer(args.peer_image, peer)
            env = environment(args.peer_image, peer)
            test = docker(
                "run", "--rm", "--name", tester, "--network", f"container:{peer}",
                "--volumes-from", f"{peer}:ro", "--cap-drop", "ALL", "--security-opt", "no-new-privileges",
                "--user", "0", "--read-only", "--tmpfs", "/tmp", "--pids-limit", "64", "--memory", "512m",
                "--mount", f"type=volume,src={args.target_volume},dst=/workspace/target,readonly",
                "--mount", f"type=bind,src={out},dst=/interop/out",
                "--env", "MYNOU_WIREGUARD_INTEROP=1",
                "--env", f"MYNOU_WIREGUARD_UAPI={UAPI}",
                "--env", f"MYNOU_WIREGUARD_PEER_LOG={PEER_LOG}",
                "--env", "MYNOU_WIREGUARD_RESULT=/interop/out/result.jsonl",
                *(["--env", f"MYNOU_WIREGUARD_SCENARIO={args.scenario}"] if args.scenario else []),
                args.rust_image, executable, "--exact", "wireguard_go_interoperability",
                "--test-threads=1", "--nocapture",
                timeout=TEST_TIMEOUT, check=False, capture=False,
            )
            record = out / "result.jsonl"
            text = read_record(record) if record.is_file() else ""
            try:
                entries = validate(text, (args.scenario,) if args.scenario else SCENARIOS)
            except ValueError as invalid:
                error = str(invalid)
                try:
                    entries = parse_entries(text)
                except ValueError:
                    entries = []
            if test.returncode != 0 and not error:
                error = f"The test exited with {test.returncode}"
        except (RuntimeError, ValueError, OSError, subprocess.TimeoutExpired) as failure:
            error = str(failure)
        finally:
            if error:
                try:
                    diagnostics = peer_tail(peer)
                except (OSError, subprocess.TimeoutExpired):
                    diagnostics = "Peer diagnostics unavailable"
            for name in (tester, peer):
                try:
                    result = docker("rm", "--force", "--volumes", name, check=False)
                    # An absent container is normal after --rm or a failed start.
                    if result.returncode and "No such container" not in result.stderr:
                        error = error or "Container cleanup failed"
                except (OSError, subprocess.TimeoutExpired):
                    error = error or "Container cleanup failed"
        report = summary(provenance, env, entries, error, partial=bool(args.scenario))
        print(report)
        if os.environ.get("GITHUB_STEP_SUMMARY"):
            with open(os.environ["GITHUB_STEP_SUMMARY"], "a") as stream:
                stream.write(report)
        if error:
            if diagnostics:
                print("Peer log tail (flood and routine lines omitted):\n" + diagnostics, file=sys.stderr)
            sys.exit(error)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--peer-image", default="mynou-wireguard-peer")
    parser.add_argument("--rust-image", default="mynou-local-rust")
    parser.add_argument("--target-volume", default="mynou-rust-wireguard-interop")
    parser.add_argument("--scratch", default=None, help="Directory for the run's peer log and record")
    parser.add_argument("--scenario", choices=SCENARIOS, help="Run one scenario to diagnose it; never gate evidence")
    run(parser.parse_args(argv))


if __name__ == "__main__":
    main()

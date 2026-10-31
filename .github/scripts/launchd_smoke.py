#!/usr/bin/env python3
"""Opt-in native launchd fixture: synthetic state, isolated HOME and unique label.

Never points at a real data directory or installs the default service label.
Run manually on a logged-in macOS account, passing a built Mynou binary. A first
start listens on 127.0.0.1:8787 before its settings move it elsewhere, so stop
any Mynou using that port first.
"""
import argparse
import json
import os
from pathlib import Path
import re
import shutil
import socket
import subprocess
import tempfile
import time
import urllib.error
import urllib.request


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    args = parser.parse_args()
    built_binary = args.binary.resolve(strict=True)
    label = f"org.mynou.fixture.{os.getpid()}"
    domain = f"gui/{os.getuid()}"
    subprocess.run(["/bin/launchctl", "print", domain], check=True, stdout=subprocess.DEVNULL)
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))

    def api(address, method, path, body=None):
        request = urllib.request.Request(f"http://{address}{path}", method=method,
            data=None if body is None else json.dumps(body).encode(),
            headers={} if body is None else {"Content-Type": "application/json"})
        with opener.open(request, timeout=10) as response:
            return json.loads(response.read())
    with tempfile.TemporaryDirectory(prefix="mynou-launchd-", dir="/private/tmp") as temporary:
        root = Path(temporary)
        # launchd cannot read a developer checkout in Documents without macOS
        # privacy consent. Exercise a deployed binary outside protected folders.
        binary = root / "mynou"
        shutil.copy2(built_binary, binary)
        home = root / "home"
        (home / "Library" / "LaunchAgents").mkdir(parents=True)
        env = {**os.environ, "HOME": str(home)}
        def cli(*words, ok=True):
            output = subprocess.run([str(binary), *words, "--dir", str(root)], env=env,
                                    cwd=root, capture_output=True, text=True, timeout=150)
            if ok:
                assert output.returncode == 0, output.stderr
            else:
                assert output.returncode != 0
            return output

        def service(action, ok=True):
            output = cli("service", "--label", label, "--action", action, ok=ok)
            return json.loads(output.stdout) if ok else output

        def pid():
            result = subprocess.run(["/bin/launchctl", "print", f"{domain}/{label}"],
                                    capture_output=True, text=True, check=True)
            found = re.search(r"^\s*pid = (\d+)\s*$", result.stdout, re.MULTILINE)
            return int(found[1]) if found else None

        def wait(predicate, seconds=15):
            deadline = time.monotonic() + seconds
            while time.monotonic() < deadline:
                if predicate():
                    return
                time.sleep(0.1)
            raise AssertionError("fixture condition timed out")

        # Settings live in the store: a first foreground start creates it, and
        # its API saves the fixture's settings for the service to start with.
        first = subprocess.Popen([str(binary), "serve", "--dir", str(root)], env=env, cwd=root,
                                 stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        endpoint = root / "state" / "endpoint"
        wait(lambda: first.poll() is not None or (endpoint.exists() and endpoint.read_text().strip()),
             seconds=60)
        assert first.poll() is None, "the first start failed: is 127.0.0.1:8787 in use?"
        initial = endpoint.read_text().strip()
        def started():
            try:
                with opener.open(f"http://{initial}/readyz", timeout=1) as response:
                    return response.status == 200
            except (OSError, urllib.error.URLError):
                return False
        # A new store has the VPN off, so it gets ready without a choice.
        wait(started, seconds=60)
        value = api(initial, "GET", "/api/settings")["settings"]
        with socket.socket() as bound:
            bound.bind(("127.0.0.1", 0))
            address = f"127.0.0.1:{bound.getsockname()[1]}"
        value["listen"] = address
        value["library"]["movies_root"] = str(root / "movies")
        value["library"]["series_root"] = str(root / "series")
        value["downloads"]["data_dir"] = str(root / "downloads")
        value["downloads"]["state_dir"] = str(root / "state" / "transfers")
        value["downloads"]["enabled"] = False
        value["downloads"]["dht"] = False
        value["downloads"]["pex"] = False
        value["downloads"]["listen_port"] = 0
        api(initial, "POST", "/api/settings", {"settings": value})
        api(initial, "POST", "/api/shutdown", {})
        assert first.wait(timeout=60) == 0
        for name in ("state/jobs", "state/transfers", "movies", "series", "downloads"):
            (root / name).mkdir(parents=True, exist_ok=True)
        sentinel = root / "downloads" / "keep.bin"
        sentinel.write_bytes(b"preserved synthetic payload")
        target = f"{domain}/{label}"
        try:
            first = service("install")
            assert first["installed"] and first["loaded"]
            wait(lambda: service("status")["running"])
            def healthy():
                try:
                    with urllib.request.build_opener(urllib.request.ProxyHandler({})).open(
                            f"http://{address}/readyz", timeout=1) as response:
                        return response.status == 200
                except (OSError, urllib.error.URLError):
                    return False
            wait(healthy)
            original = pid()
            service("install")
            assert pid() == original, "idempotent install restarted a live job"
            cli("serve", ok=False)  # A second writer cannot take the listener/stores.
            registration = home / "Library" / "LaunchAgents" / f"{label}.plist"
            assert registration.stat().st_mode & 0o777 == 0o600
            subprocess.run(["/usr/bin/plutil", "-lint", str(registration)], check=True,
                           stdout=subprocess.DEVNULL)
            print("PASS install, idempotency, second-owner refusal and private plist", flush=True)

            subprocess.run(["/bin/launchctl", "kill", "SIGKILL", target], check=True)
            wait(lambda: pid() is not None and pid() != original, seconds=80)
            wait(healthy)
            print("PASS supervised crash restart (60-second throttle)", flush=True)

            # A shutdown naming another process must not stop this one.
            try:
                api(address, "POST", "/api/shutdown", {"expected_pid": pid() + 1})
                raise AssertionError("wrong-process shutdown accepted")
            except urllib.error.HTTPError as error:
                assert error.code == 400
            assert service("stop")["loaded"] is False
            service("stop")
            assert "stopped_cleanly" in (root / ".mynou-service/events.log").read_text()
            # Rehearse a coherent offline state backup and compatible rollback.
            shutil.copytree(root / "state", root / "backup-state")

            (root / "movies").rename(root / "missing-movies")
            service("start", ok=False)
            assert not (root / "movies").exists()
            (root / "missing-movies").rename(root / "movies")
            service("start")
            wait(healthy)
            assert service("uninstall")["installed"] is False
            service("uninstall")
            assert not registration.exists()
            assert sentinel.read_bytes() == b"preserved synthetic payload"
            shutil.rmtree(root / "state")
            shutil.copytree(root / "backup-state", root / "state")
            service("install")
            wait(healthy)
            assert api(address, "GET", "/api/settings")["settings"]["listen"] == address
            service("uninstall")
            assert sentinel.read_bytes() == b"preserved synthetic payload"
            for log in (root / ".mynou-service").glob("*.log"):
                assert log.stat().st_size <= 65536
            print("PASS identity fence, graceful stop/start, missing-volume refusal, uninstall, preservation and offline rollback", flush=True)
        finally:
            # Only this fixture's unique label can be forcibly unloaded on failure.
            subprocess.run(["/bin/launchctl", "bootout", target], capture_output=True)


if __name__ == "__main__":
    main()

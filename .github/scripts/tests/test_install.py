"""install.sh against fake releases served from a temporary directory.

The script under test is a copy of install.sh in which only the repository URL
and the embedded release key change, to a local server and a throwaway key:
install.sh itself has no switch for tests. Stubs of uname and sysctl on PATH
pose as a Mac with Apple Silicon, an Intel Mac or Linux."""
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import threading
import unittest

ROOT = Path(__file__).resolve().parents[3]
SCRIPT = ROOT / "install.sh"
REPO_LINE = "REPO=https://github.com/alecerf/mynou\n"
PREFIX = "/alecerf/mynou/releases/"


def embedded_key(text):
    start = text.index("-----BEGIN PUBLIC KEY-----")
    end = text.index("-----END PUBLIC KEY-----", start) + len("-----END PUBLIC KEY-----")
    return text[start:end]


def executable(version):
    return f"#!/bin/sh\necho 'mynou {version} - Rust std, 0 dependencies'\n".encode()


def openssl(*args):
    subprocess.run(["openssl", *map(str, args)], check=True, capture_output=True)


class Releases(BaseHTTPRequestHandler):
    """GitHub's release URLs: releases/latest redirects to the latest tag and
    downloads redirect to another location, as release assets do."""

    def do_GET(self):
        server = self.server
        server.requests.append(self.path)
        if self.path == PREFIX + "latest":
            return self.redirect(f"{PREFIX}tag/v{server.latest}")
        if self.path.startswith(PREFIX + "download/"):
            tag, _, name = self.path[len(PREFIX + "download/"):].partition("/")
            if "/" not in name and (server.root / tag / name).is_file():
                return self.redirect(f"/assets/{tag}/{name}")
        if self.path.startswith("/assets/"):
            tag, _, name = self.path[len("/assets/"):].partition("/")
            file = server.root / tag / name
            if "/" not in name and file.is_file():
                body = file.read_bytes()
                self.send_response(200)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                return self.wfile.write(body)
        self.send_error(404)

    def redirect(self, path):
        self.send_response(302)
        self.send_header("Location", self.server.url + path)
        self.send_header("Content-Length", "0")
        self.end_headers()

    def log_message(self, *args):
        pass


class InstallScript(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.keys = Path(tempfile.mkdtemp())
        for name in ("release", "other"):
            openssl("ecparam", "-name", "prime256v1", "-genkey", "-noout", "-out", cls.keys / f"{name}.pem")
        openssl("ec", "-in", cls.keys / "release.pem", "-pubout", "-out", cls.keys / "release.pub.pem")

    @classmethod
    def tearDownClass(cls):
        shutil.rmtree(cls.keys)

    def setUp(self):
        self.work = Path(tempfile.mkdtemp())
        self.home, self.tmp, self.stubs, self.releases = (self.work / name for name in ("home", "tmp", "stubs", "releases"))
        for directory in (self.home, self.tmp, self.stubs, self.releases):
            directory.mkdir()
        self.bin = self.home / ".local/bin"
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Releases)
        self.server.root, self.server.requests, self.server.latest = self.releases, [], None
        self.server.url = f"http://127.0.0.1:{self.server.server_address[1]}"
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        text = SCRIPT.read_text()
        key = embedded_key(text)
        self.assertEqual((text.count(REPO_LINE), text.count(key)), (1, 1))
        test_key = (self.keys / "release.pub.pem").read_text().strip()
        self.script = (text.replace(REPO_LINE, f"REPO={self.server.url}/alecerf/mynou\n")
                       .replace(key, test_key))

    def tearDown(self):
        self.server.shutdown()
        self.server.server_close()
        shutil.rmtree(self.work)

    def publish(self, version, key="release"):
        directory = self.releases / f"v{version}"
        directory.mkdir()
        name = f"mynou-v{version}-macos-arm64"
        (directory / name).write_bytes(executable(version))
        digest = hashlib.sha256(executable(version)).hexdigest()
        (directory / "SHA256SUMS").write_text(f"{digest}  {name}\n")
        openssl("dgst", "-sha256", "-sign", self.keys / f"{key}.pem",
                "-out", directory / "SHA256SUMS.sig", directory / "SHA256SUMS")
        self.server.latest = version
        return directory

    def install(self, *args, system="Darwin", arm64=True, shell="sh", **env):
        # Under Rosetta, uname -m says x86_64 although the Mac has Apple Silicon.
        uname = f"#!/bin/sh\ncase ${{1-}} in -m) echo x86_64 ;; *) echo {system} ;; esac\n"
        sysctl = ("#!/bin/sh\n[ \"$*\" = '-n hw.optional.arm64' ] && echo 1 && exit 0\n" if arm64 else "")
        sysctl += "echo \"sysctl: unknown oid '$2'\" >&2\nexit 1\n"
        for name, text in (("uname", uname), ("sysctl", sysctl)):
            (self.stubs / name).write_text(text)
            (self.stubs / name).chmod(0o755)
        environment = {"PATH": f"{self.stubs}{os.pathsep}{os.environ['PATH']}", "HOME": str(self.home),
                       "TMPDIR": str(self.tmp), "SHELL": "/bin/zsh", **env}
        # Piped like `curl ... | sh`.
        return subprocess.run([shell, "-s", "--", *args], input=self.script, capture_output=True,
                              encoding="utf-8", env=environment, timeout=60)

    def assertInstalled(self, version):
        target = self.bin / "mynou"
        self.assertEqual(target.read_bytes(), executable(version))
        self.assertTrue(os.access(target, os.X_OK))
        self.assertEqual(os.listdir(self.bin), ["mynou"])
        self.assertEqual(os.listdir(self.tmp), [])

    def assertRefused(self, result, message, previous=None):
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(message, result.stderr)
        if previous is None:
            self.assertFalse(self.bin.exists())
        else:
            self.assertInstalled(previous)
        self.assertEqual(os.listdir(self.tmp), [])

    def test_installs_the_latest_release(self):
        self.publish("0.29.0")
        self.publish("0.30.0")
        shells = [shell for shell in ("sh", "dash", "bash") if shutil.which(shell)]
        for shell in shells:
            with self.subTest(shell=shell):
                shutil.rmtree(self.home / ".local", ignore_errors=True)
                result = self.install(shell=shell)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertInstalled("0.30.0")
                self.assertEqual(os.listdir(self.home), [".local"])
                self.assertIn("mynou 0.30.0 - Rust std", result.stdout)
                self.assertIn("Add this line to ~/.zshrc", result.stdout)
                self.assertIn('export PATH="$HOME/.local/bin:$PATH"', result.stdout)
                self.assertIn("mynou serve", result.stdout)

    def test_pinned_versions_and_upgrades_in_place(self):
        self.publish("0.29.0")
        self.publish("0.30.0")
        for args, env in ((("0.29.0",), {}), ((), {"MYNOU_VERSION": "v0.29.0"})):
            with self.subTest(args=args, env=env):
                result = self.install(*args, **env)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertInstalled("0.29.0")
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertInstalled("0.30.0")
        self.assertIn("Restart Mynou if it is running", result.stdout)
        on_path = self.install(PATH=f"{self.stubs}{os.pathsep}{self.bin}{os.pathsep}{os.environ['PATH']}")
        self.assertEqual(on_path.returncode, 0, on_path.stderr)
        self.assertNotIn("PATH", on_path.stdout)
        self.assertRefused(self.install("1.0"), "1.0 is not a version", previous="0.30.0")
        self.assertRefused(self.install("1.0.0"), "Cannot download Mynou 1.0.0", previous="0.30.0")

    def test_custom_directory(self):
        self.publish("0.29.0")
        self.bin = self.work / "custom"
        result = self.install(MYNOU_INSTALL_DIR=str(self.bin))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertInstalled("0.29.0")
        self.assertEqual(os.listdir(self.home), [])
        self.assertIn(f'export PATH="{self.bin}:$PATH"', result.stdout)
        self.assertRefused(self.install(MYNOU_INSTALL_DIR="relative/bin"), "must be an absolute path",
                           previous="0.29.0")

    def test_tampered_executable_is_refused(self):
        self.publish("0.29.0")
        self.assertEqual(self.install().returncode, 0)
        release = self.publish("0.30.0")
        (release / "mynou-v0.30.0-macos-arm64").write_bytes(executable("6.6.6"))
        self.assertRefused(self.install(), "does not match its checksum", previous="0.29.0")

    def test_manifest_without_the_release_signature_is_refused(self):
        def rewritten(release):
            # Both the executable and the manifest changed: only the signature tells.
            binary = release / "mynou-v0.29.0-macos-arm64"
            binary.write_bytes(executable("6.6.6"))
            digest = hashlib.sha256(binary.read_bytes()).hexdigest()
            (release / "SHA256SUMS").write_text(f"{digest}  {binary.name}\n")

        cases = {"rewritten manifest": ("release", rewritten, "not signed by Mynou's release key"),
                 "other key": ("other", None, "not signed by Mynou's release key"),
                 "missing signature": ("release", lambda release: (release / "SHA256SUMS.sig").unlink(),
                                       "only releases from 0.29.0 on are signed")}
        for case, (key, change, message) in cases.items():
            with self.subTest(case=case):
                shutil.rmtree(self.releases / "v0.29.0", ignore_errors=True)
                release = self.publish("0.29.0", key=key)
                if change:
                    change(release)
                self.assertRefused(self.install(), message)

    def test_only_macs_with_apple_silicon(self):
        self.publish("0.29.0")
        for system, arm64, message in (("Linux", True, "only on macOS with Apple Silicon, not on Linux"),
                                       ("Darwin", False, "no Intel build")):
            with self.subTest(system=system, arm64=arm64):
                self.assertRefused(self.install(system=system, arm64=arm64), message)
        self.assertEqual(self.server.requests, [])

    def test_embedded_key_is_the_release_key(self):
        self.assertEqual(embedded_key(SCRIPT.read_text()), (ROOT / "release-key.pub.pem").read_text().strip())


if __name__ == "__main__":
    unittest.main()

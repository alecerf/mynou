"""Exercise fixture scheduling and process cleanup without native instrumentation."""
from contextlib import redirect_stdout
import io
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import threading
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import host_io_observer as observer


class FixtureRunnerTests(unittest.TestCase):
    def test_parallelism_is_bounded_and_fixture_order_and_names_are_preserved(self):
        for workers in (1, 3, 4):
            with self.subTest(workers=workers):
                lock = threading.Lock()
                batch = threading.Barrier(workers, timeout=5)
                active = peak = 0
                names = set()

                def capture(command, name, library, output):
                    nonlocal active, peak
                    index = observer.FIXTURES.index(command[2])
                    self.assertEqual(name, f"fixture-{index:02d}")
                    with lock:
                        names.add(name)
                        active += 1
                        peak = max(peak, active)
                    try:
                        if index < workers:
                            batch.wait()
                        return [{"index": index}]
                    finally:
                        with lock:
                            active -= 1

                with patch.object(observer, "capture", side_effect=capture), patch.object(
                        observer, "validate", side_effect=lambda rows: {"index": rows[0]["index"]}):
                    results = observer.observe_fixtures("harness", Path("library"), Path("output"), workers)
                self.assertEqual(peak, workers)
                self.assertEqual(len(names), 43)
                self.assertEqual(list(results), list(observer.FIXTURES))
                self.assertEqual(list(results.values()), [{"index": i} for i in range(43)])

    def test_failure_identifies_the_exact_fixture_and_retains_the_cause(self):
        ready = threading.Event()

        def capture(command, *args):
            if command[2] == "slow-first":
                self.assertTrue(ready.wait(5))
                return []
            ready.set()
            raise ValueError("fixture fixture-01 timed out; see fixture-01.log")

        with patch.object(observer, "FIXTURES", ("slow-first", "broken-second")), \
                patch.object(observer, "capture", side_effect=capture), \
                patch.object(observer, "validate", return_value={}):
            with self.assertRaisesRegex(ValueError, "broken-second.*fixture-01.log") as error:
                observer.observe_fixtures("harness", Path("library"), Path("output"), 2)
        self.assertIsInstance(error.exception.__cause__, ValueError)

    def test_negative_controls_finish_before_fixtures_and_summary_content_is_unchanged(self):
        controls = []
        negative = [{"op": op, "pid": 7} for op in (
            "loaded", "socket", "bind", "connect", "sendto", "getaddrinfo", "close")]

        def capture(command, name, *args):
            if name == "negative-rust-stdlib":
                self.assertEqual(controls, [])
                controls.append("rust")
                return negative
            self.assertEqual(controls, ["rust", "lookup"])
            return []

        def lookup(*args):
            self.assertEqual(controls, ["rust"])
            controls.append("lookup")
            return {"failed_getaddrinfo": "observed"}

        def validate(rows):
            if rows:
                raise ValueError("deliberate negative control")
            return {"sendto": 4, "socket": 2}

        listing = "".join(name + ": test\n" for name in (*observer.FIXTURES, observer.NEGATIVE))
        with tempfile.TemporaryDirectory() as directory, \
                patch.object(observer.platform, "system", return_value="Darwin"), \
                patch.object(observer.platform, "machine", return_value="arm64"), \
                patch.object(observer, "harness", return_value="harness"), \
                patch.object(observer.subprocess, "run"), \
                patch.object(observer.subprocess, "check_output", return_value=listing), \
                patch.object(observer, "capture", side_effect=capture), \
                patch.object(observer, "failed_lookup_control", side_effect=lookup), \
                patch.object(observer, "validate", side_effect=validate), redirect_stdout(io.StringIO()):
            output = Path(directory)
            summary = observer.run(Path("build"), output)
            expected = {
                "platform": "macOS arm64",
                "fixtures": dict.fromkeys(observer.FIXTURES, {"sendto": 4, "socket": 2}),
                "negative_controls": {
                    "rust_stdlib": dict.fromkeys((row["op"] for row in negative), 1),
                    "failed_getaddrinfo": "observed",
                },
            }
            self.assertEqual(summary, expected)
            self.assertEqual((output / "summary.json").read_text(), json.dumps(expected, indent=2) + "\n")

    def test_invalid_worker_counts_fail_before_running_commands(self):
        for workers in (0, 5, -1):
            with self.subTest(workers=workers), patch.object(observer, "harness") as harness:
                with self.assertRaisesRegex(ValueError, "between 1 and 4"):
                    observer.run(Path("build"), Path("output"), workers)
                harness.assert_not_called()


class CaptureTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.output = Path(self.directory.name)
        popen = subprocess.Popen

        def launch(*args, **kwargs):
            self.assertTrue(kwargs["start_new_session"])
            self.assertEqual(kwargs["env"]["DYLD_INSERT_LIBRARIES"], "test-library")
            # These subprocess tests exercise supervision without a native dylib.
            kwargs["env"].pop("DYLD_INSERT_LIBRARIES")
            return popen(*args, **kwargs)

        self.launcher = patch.object(observer.subprocess, "Popen", side_effect=launch)
        self.launcher.start()
        self.addCleanup(self.launcher.stop)

    def capture(self, source):
        return observer.capture([sys.executable, "-c", source], "fixture-00",
                                Path("test-library"), self.output)

    def test_capture_preserves_stdout_stderr_trace_and_extra_environment(self):
        source = '''import json, os
from pathlib import Path
os.write(1, b"standard output\\n")
os.write(2, b"standard error\\n")
Path(os.environ["MYNOU_HOST_IO_TRACE"]).write_text(json.dumps({"op": "loaded"}) + "\\n")
'''
        self.assertEqual(self.capture(source), [{"op": "loaded"}])
        self.assertEqual((self.output / "fixture-00.log").read_bytes(),
                         b"standard output\nstandard error\n")

    def test_failed_child_keeps_its_log_and_reports_exit_status(self):
        with self.assertRaisesRegex(ValueError, r"fixture-00 failed \(7\).*fixture-00.log"):
            self.capture('import sys; print("failure evidence", flush=True); sys.exit(7)')
        self.assertIn("failure evidence", (self.output / "fixture-00.log").read_text())

    def test_timeout_kills_parent_and_descendant_and_preserves_output(self):
        source = '''import os, subprocess, sys, time
print("parent", os.getpid(), os.getpgrp(), flush=True)
subprocess.Popen([sys.executable, "-c", "import os, time; print('child', os.getpid(), os.getpgrp(), flush=True); time.sleep(60)"])
time.sleep(60)
'''
        parent = None
        try:
            with patch.object(observer, "CAPTURE_TIMEOUT", 2), \
                    patch.object(observer.os, "killpg", wraps=os.killpg) as kill_group:
                with self.assertRaisesRegex(ValueError, "fixture-00 timed out after 2s.*fixture-00.log"):
                    self.capture(source)
            lines = (self.output / "fixture-00.log").read_text().splitlines()
            self.assertEqual({line.split()[0] for line in lines}, {"parent", "child"})
            parent = int(next(line.split()[1] for line in lines if line.startswith("parent ")))
            self.assertTrue(all(int(line.split()[2]) == parent for line in lines))
            kill_group.assert_called_once_with(parent, signal.SIGKILL)
        finally:
            if parent is not None:
                try:
                    os.killpg(parent, signal.SIGKILL)
                except ProcessLookupError:
                    pass


if __name__ == "__main__":
    unittest.main()

"""Only a successful build with one runnable library test is usable evidence."""
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import cargo_artifacts as artifacts
import host_io_observer as observer


class CargoArtifactsTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name).resolve()
        self.executable = self.root / "library-test"
        self.executable.write_text("#!/bin/sh\nexit 0\n")
        self.executable.chmod(0o700)
        self.build = self.root / "build.jsonl"
        self.artifact = {"reason": "compiler-artifact", "executable": str(self.executable),
                         "target": {"kind": ["lib"]}, "profile": {"test": True}}
        self.finished = {"reason": "build-finished", "success": True}

    def write(self, *rows):
        self.build.write_text("".join(json.dumps(row) + "\n" for row in rows))

    def test_resolves_cached_or_fresh_artifacts_and_observer_uses_the_same_check(self):
        for fresh in (True, False):
            self.write({"reason": "compiler-message", "message": {"rendered": "warning"}},
                       {**self.artifact, "fresh": fresh}, self.finished)
            self.assertEqual(artifacts.library_test_executable(self.build), str(self.executable))
            self.assertEqual(observer.harness(self.build), str(self.executable))

    def test_requires_successful_build_completion(self):
        for rows in ((self.artifact,), (self.artifact, {**self.finished, "success": False}),
                     (self.artifact, {"reason": "build-finished"}),
                     (self.artifact, {**self.finished, "success": False}, self.finished)):
            with self.subTest(rows=rows):
                self.write(*rows)
                with self.assertRaises(ValueError):
                    artifacts.library_test_executable(self.build)

    def test_requires_exactly_one_library_test_artifact(self):
        for rows in ((), (self.artifact, self.artifact),
                     (self.artifact, {**self.artifact, "executable": "/other-test"}),
                     ({**self.artifact, "profile": {"test": False}},),
                     ({**self.artifact, "target": {"kind": ["bin"]}},),
                     ({**self.artifact, "reason": "other"},),
                     ({**self.artifact, "executable": None},)):
            with self.subTest(rows=rows):
                self.write(*rows, self.finished)
                with self.assertRaisesRegex(ValueError, "one compiled library"):
                    artifacts.library_test_executable(self.build)

    def test_ignores_other_targets_and_non_test_libraries(self):
        self.write({**self.artifact, "profile": {"test": False}},
                   {**self.artifact, "target": {"kind": ["example"]}},
                   self.artifact, self.finished)
        self.assertEqual(artifacts.library_test_executable(self.build), str(self.executable))

    def test_rejects_missing_non_executable_and_directory_paths(self):
        for kind in ("permissions", "missing", "directory"):
            with self.subTest(kind=kind):
                if kind == "permissions":
                    self.executable.chmod(0o600)
                elif kind == "missing":
                    self.executable.unlink()
                else:
                    self.executable.mkdir()
                self.write(self.artifact, self.finished)
                with self.assertRaisesRegex(ValueError, "not an executable file"):
                    artifacts.library_test_executable(self.build)

    def test_malformed_or_truncated_json_is_refused(self):
        self.write(self.artifact, self.finished)
        with self.build.open("a") as build:
            build.write('{"reason":')
        with self.assertRaises(ValueError):
            artifacts.library_test_executable(self.build)

    def test_cli_prints_only_the_path_and_fails_closed(self):
        script = Path(artifacts.__file__)
        self.write(self.artifact, self.finished)
        result = subprocess.run([sys.executable, str(script), str(self.build)],
                                capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, str(self.executable) + "\n")
        self.write(self.artifact)
        result = subprocess.run([sys.executable, str(script), str(self.build)],
                                capture_output=True, text=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(result.stdout, "")
        self.assertIn("build-finished", result.stderr)


if __name__ == "__main__":
    unittest.main()

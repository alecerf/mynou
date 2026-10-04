"""Exercise CI scheduling guarantees; these checks run only in GitHub Actions."""

import json
from pathlib import Path
import tempfile
import unittest

import parallel_tests as runner


class SchedulerTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        (self.root / "src").mkdir()
        (self.root / "target/debug").mkdir(parents=True)
        self.targets = []
        self.messages = []

    def target(self, name, body):
        source = self.root / f"src/{name}.rs"
        source.write_text("// CI scheduler fixture.\n")
        executable = self.root / f"target/debug/{name}"
        executable.write_text("#!/usr/bin/env python3\n" + body)
        executable.chmod(0o755)
        target = runner.Target(source.relative_to(self.root).as_posix(), executable)
        self.targets.append(target)
        self.messages.append({"reason": "compiler-artifact", "profile": {"test": True}, "target": {"src_path": str(source)}, "executable": str(executable)})
        return target

    def manifest(self, successful=True):
        path = self.root / "manifest.jsonl"
        messages = self.messages + [{"reason": "build-finished", "success": successful}]
        path.write_text("\n".join(json.dumps(row) for row in messages) + "\n")
        return path

    def test_cargo_discovery_retains_every_harness_and_rejects_incomplete_builds(self):
        self.target("lib", "pass\n")
        self.target("main", "pass\n")
        self.target("integration", "pass\n")
        normal = dict(self.messages[-1], profile={"test": False})
        self.messages.append(normal)
        self.assertEqual(runner.discover(self.manifest(), self.root), self.targets)
        with self.assertRaises(ValueError):
            runner.discover(self.manifest(False), self.root)
        self.messages.pop()
        self.messages[1]["target"]["src_path"] = self.messages[0]["target"]["src_path"]
        with self.assertRaises(ValueError):
            runner.discover(self.manifest(), self.root)

    def test_failure_and_missing_summary_cannot_pass_the_aggregate(self):
        self.target("failed", "print('test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s')\nraise SystemExit(1)\n")
        self.target("missing", "print('No test summary')\n")
        self.target("good", "print('test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s')\n")
        report = runner.schedule(self.targets, self.root, self.root / "logs", 2, 1, 3, {})
        self.assertFalse(report["success"])
        self.assertEqual(len(report["results"]), 3)
        self.assertEqual(report["counts"]["failed"], 1)
        self.assertEqual(report["counts"]["passed"], 1)
        self.assertTrue(next(row for row in report["results"] if row["source"] == "src/good.rs")["success"])

    def test_independent_harnesses_overlap_with_a_bounded_process_count(self):
        body = (
            "from pathlib import Path\nimport time\n"
            "started = Path(__file__).name\nPath(started + '.started').touch()\n"
            "deadline = time.monotonic() + 2\n"
            "while len(list(Path('.').glob('*.started'))) < 2:\n"
            "    if time.monotonic() >= deadline: raise SystemExit(3)\n"
            "    time.sleep(0.01)\n"
            "time.sleep(0.05)\n"
            "print('test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.05s')\n"
        )
        self.target("one", body)
        self.target("two", body)
        report = runner.schedule(self.targets, self.root, self.root / "logs", 2, 1, 3, {})
        self.assertTrue(report["success"])
        self.assertEqual(report["counts"]["passed"], 2)
        self.assertEqual(report["workers"], 2)

    def test_timeout_terminates_a_harness_instead_of_reporting_success(self):
        target = self.target("slow", "import time\ntime.sleep(30)\n")
        result = runner.execute(target, self.root, self.root / "slow.log", 1, 0.05)
        self.assertTrue(result["timed_out"])
        self.assertFalse(result["success"])
        self.assertIsNotNone(result["exit_code"])


if __name__ == "__main__":
    unittest.main()

"""Exercise local/CI scheduling and complete library-module coverage."""

import json
from pathlib import Path
import tempfile
import unittest

import parallel_tests as runner


class SchedulerTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        # The scheduler resolves paths, and macOS temporary directories sit
        # behind a symbolic link, so the fixture root must be the physical path.
        self.root = Path(self.temporary.name).resolve()
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
        expected = {target.source for target in self.targets}
        self.assertEqual(runner.discover(self.manifest(), self.root, expected), self.targets)
        with self.assertRaises(ValueError):
            runner.discover(self.manifest(), self.root, expected | {"src/new_target.rs"})
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
            f"markers = Path({str(self.root)!r})\n"
            "started = Path(__file__).name\n(markers / (started + '.started')).touch()\n"
            "deadline = time.monotonic() + 2\n"
            "while len(list(markers.glob('*.started'))) < 2:\n"
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

    def test_each_group_uses_a_separate_writable_workdir_and_cleans_it(self):
        body = (
            "from pathlib import Path\n"
            "Path('fixture').write_text('synthetic')\n"
            f"markers = Path({str(self.root)!r})\n"
            "(markers / (Path(__file__).name + '.cwd')).write_text(str(Path.cwd()))\n"
            "print('test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s')\n"
        )
        self.target("one", body)
        self.target("two", body)
        report = runner.schedule(self.targets, self.root, self.root / "logs", 2, 1, 3, {})
        self.assertTrue(report["success"])
        paths = [Path((self.root / f"{name}.cwd").read_text()) for name in ["one", "two"]]
        self.assertNotEqual(paths[0], paths[1])
        for path in paths:
            self.assertEqual(path.parent, self.root / "target")
            self.assertFalse(path.exists())

    def test_timeout_terminates_a_harness_instead_of_reporting_success(self):
        target = self.target("slow", "import time\ntime.sleep(30)\n")
        result = runner.execute(target, self.root, self.root / "slow.log", 1, 0.05)
        self.assertTrue(result["timed_out"])
        self.assertFalse(result["success"])
        self.assertIsNotNone(result["exit_code"])

    def library(self, names, tweak=""):
        return self.target("lib", (
            "import sys\n"
            f"names = {names!r}\n"
            "if '--list' in sys.argv:\n"
            "    for name in names: print(name + ': test')\n"
            "    raise SystemExit()\n"
            "filters = sys.argv[sys.argv.index('--exact') + 1:] if '--exact' in sys.argv else names\n"
            "selected = [name for name in names if name in filters]\n"
            + tweak + "\n"
            "for name in selected: print('test ' + name + ' ... ok')\n"
            "print(f'test result: ok. {len(selected)} passed; 0 failed; 0 ignored; 0 measured; {len(names)-len(selected)} filtered out; finished in 0.00s')\n"
        ))

    def test_library_groups_execute_all_exact_names_once_even_with_overlapping_substrings(self):
        target = self.library(["store::tests::one", "store::tests::two", "other::store::tests::one"])
        shards = runner.library_shards([target], self.root)
        self.assertEqual(len(shards), 2)
        self.assertEqual({s.label for s in shards}, {"src/lib.rs::store", "src/lib.rs::other"})
        self.assertEqual(sorted(name for s in shards for name in s.tests),
                         ["other::store::tests::one", "store::tests::one", "store::tests::two"])
        report = runner.schedule(shards, self.root, self.root / "logs", 2, 1, 3, {})
        self.assertTrue(report["success"])
        self.assertEqual(report["counts"], {"passed": 3, "failed": 0, "ignored": 0, "filtered": 0})

    def test_library_discovery_rejects_empty_duplicate_malformed_or_failed_lists(self):
        bodies = ["pass", "print('a: test\\na: test')", "print('a: benchmark')",
                  "print('not a listing')", "print('a: test'); raise SystemExit(1)"]
        for index, body in enumerate(bodies):
            target = self.target(f"bad{index}", body + "\n")
            library = runner.Target("src/lib.rs", target.executable)
            with self.subTest(body=body), self.assertRaises(ValueError):
                runner.library_shards([library], self.root)

    def test_missing_or_duplicate_library_groups_are_rejected_before_execution(self):
        target = self.library(["a::one", "b::two"])
        shards = runner.library_shards([target], self.root)
        for plan in [shards[:1], shards + shards[:1]]:
            with self.subTest(plan=plan), self.assertRaises(ValueError):
                runner.schedule(plan, self.root, self.root / "logs", 2, 1, 3, {})
        self.assertFalse((self.root / "logs").exists())

    def test_incomplete_or_wrong_test_execution_cannot_pass_a_library_group(self):
        for index, tweak in enumerate(["selected = selected[:-1]", "selected = ['fake::one', 'fake::two']"]):
            target = self.library(["a::one", "a::two"], tweak)
            shard = runner.library_shards([target], self.root)[0]
            result = runner.execute(shard, self.root, self.root / f"bad{index}.log", 1, 3)
            self.assertFalse(result["success"])

    def test_ignored_library_test_cannot_be_hidden_in_filtered_counts(self):
        target = self.library(["a::one"], (
            "print('test a::one ... ignored')\n"
            "print('test result: ok. 0 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.00s')\n"
            "raise SystemExit()"
        ))
        shard = runner.library_shards([target], self.root)[0]
        result = runner.execute(shard, self.root, self.root / "ignored.log", 1, 3)
        self.assertFalse(result["success"])


if __name__ == "__main__":
    unittest.main()

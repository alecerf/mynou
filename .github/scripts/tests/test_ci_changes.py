"""Selection must retain changed inputs and fail closed on incomplete diffs."""
import contextlib
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import ci_changes as ci


class SelectionTests(unittest.TestCase):
    def test_documentation_needs_no_product_or_tooling_tests(self):
        work = ci.select(["README.md", "docs/a.md", "docs/releases/unreleased.md", "AGENTS.md", "CONTRIBUTING.md"])
        self.assertFalse(any(work[key] for key in ci.KEYS))
        # The branch requires the GitHub CodeQL app, so retain one real scan.
        self.assertEqual(work["languages"], ["actions"])

    def test_rust_inputs_and_embedded_assets(self):
        for path in ["src/lib.rs", "src/pki/fixtures/root.der", "src/ui/app.css",
                     "examples/demo.mp4", "Cargo.lock", "rust-toolchain.toml", ".cargo/config.toml"]:
            with self.subTest(path=path):
                work = ci.select([path])
                self.assertTrue(work["tests"] and work["build"])
                self.assertFalse(work["tooling"])
        self.assertIn("rust", ci.select(["tests/new.rs"])["languages"])
        self.assertNotIn("rust", ci.select(["src/ui/app.js"])["languages"])
        self.assertIn("javascript-typescript", ci.select(["src/ui/app.js"])["languages"])

    def test_tests_only_do_not_build_or_package(self):
        work = ci.select(["tests/regression.rs", "tests/fixtures/data.json"])
        self.assertTrue(work["tests"])
        self.assertFalse(work["build"])

    def test_tooling_and_packaging_dependencies(self):
        for path in [".github/scripts/parallel_tests.py", ".github/scripts/test_durations.json",
                     ".github/scripts/releases.py", ".github/scripts/tests/test_ci_changes.py"]:
            with self.subTest(path=path):
                work = ci.select([path])
                self.assertTrue(work["tooling"])
                self.assertFalse(work["tests"] or work["build"])
        fixtures = ci.select([".github/scripts/crypto_fixtures.py"])
        self.assertTrue(fixtures["tooling"] and fixtures["tests"])
        for path in (".github/scripts/host_io_observer.py", ".github/scripts/host_io_observer.c"):
            self.assertTrue(ci.select([path])["tests"])
        package = ci.select([".github/scripts/publish-release.sh"])
        self.assertTrue(package["tooling"] and package["build"])
        self.assertFalse(package["tests"])

    def test_selector_and_native_workflow_validate_themselves(self):
        for path in [*ci.CONTROL, ".github/workflows/ci.yml"]:
            with self.subTest(path=path):
                work = ci.select([path])
                self.assertTrue(all(work[key] for key in ci.KEYS))

    def test_unknown_paths_fail_closed_and_combined_changes_accumulate(self):
        work = ci.select(["new-build-input"])
        self.assertTrue(all(work[key] for key in ci.KEYS))
        work = ci.select(["docs/a.md", "src/lib.rs", ".github/scripts/releases.py"])
        self.assertTrue(all(work[key] for key in ci.KEYS))

    def test_manual_tag_and_schedule_keep_full_validation(self):
        work = ci.select([], full=True)
        self.assertTrue(all(work[key] for key in ci.KEYS))
        self.assertEqual(work["languages"], ["actions", "javascript-typescript", "rust"])
        self.assertEqual(ci.select(["docs/a.md"], pull_request=False)["languages"], [])

    def test_unreadable_diff_selects_all_checks(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "event").write_text(json.dumps({"pull_request": {"base": {"sha": "base"}, "head": {"sha": "head"}}}))
            env = dict(GITHUB_EVENT_NAME="pull_request", GITHUB_EVENT_PATH=str(root / "event"),
                       GITHUB_OUTPUT=str(root / "output"), GITHUB_STEP_SUMMARY=str(root / "summary"))
            with patch.dict(os.environ, env), patch.object(ci, "changed_paths", side_effect=subprocess.CalledProcessError(1, "git")), contextlib.redirect_stdout(io.StringIO()):
                ci.main()
            self.assertIn("tests=true", (root / "output").read_text())
            self.assertIn("build=true", (root / "output").read_text())
            self.assertIn('"rust"', (root / "output").read_text())

    def test_event_outputs_for_tags_dispatch_schedule_and_push(self):
        cases = [
            ("push", "tag", {}, True),
            ("workflow_dispatch", "branch", {}, True),
            ("schedule", "branch", {}, True),
            ("push", "branch", {"before": "before", "after": "after"}, False),
            ("pull_request", "branch", {"pull_request": {"base": {"sha": "base"}, "head": {"sha": "head"}}}, False),
        ]
        for kind, ref_type, event, full in cases:
            with self.subTest(kind=kind, ref_type=ref_type), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                (root / "event").write_text(json.dumps(event))
                env = dict(GITHUB_EVENT_NAME=kind, GITHUB_REF_TYPE=ref_type,
                           GITHUB_EVENT_PATH=str(root / "event"), GITHUB_OUTPUT=str(root / "output"),
                           GITHUB_STEP_SUMMARY=str(root / "summary"))
                with patch.dict(os.environ, env), patch.object(ci.subprocess, "check_output", return_value=b"docs/only.md\0") as diff:
                    ci.main()
                output = (root / "output").read_text()
                self.assertIn("tests=" + str(full).lower(), output)
                self.assertIn("build=" + str(full).lower(), output)
                if full:
                    diff.assert_not_called()
                    self.assertIn('"rust"', output)
                else:
                    args = diff.call_args.args[0]
                    self.assertIn("--no-renames", args)
                    if kind == "push":
                        self.assertIn("before", args)
                        self.assertIn("after", args)
                    else:
                        self.assertIn("base...head", args)

    def test_git_diff_keeps_merge_base_renames_and_every_path(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            def git(*args):
                return subprocess.check_output(["git", "-C", directory, *args], stderr=subprocess.DEVNULL).decode().strip()
            git("init")
            git("config", "user.email", "fixture@example.invalid")
            git("config", "user.name", "CI fixture")
            (root / "src").mkdir()
            (root / "src/old.rs").write_text("old")
            git("add", ".")
            git("commit", "-m", "base")
            base = git("rev-parse", "HEAD")
            (root / "docs").mkdir()
            (root / "src/old.rs").rename(root / "docs/moved.md")
            for index in range(310):
                (root / f"docs/{index}.md").write_text("docs")
            (root / "tests").mkdir()
            (root / "tests/space and\nnewline.rs").write_text("fixture")
            git("add", ".")
            git("commit", "-m", "head")
            head = git("rev-parse", "HEAD")
            git("checkout", "--detach", base)
            (root / "only-on-base.rs").write_text("unrelated base change")
            git("add", ".")
            git("commit", "-m", "advance base")
            advanced = git("rev-parse", "HEAD")
            # Run the real git command in the fixture without changing process cwd.
            original = subprocess.check_output
            with patch.object(ci.subprocess, "check_output", side_effect=lambda args: original(args, cwd=directory)):
                paths = ci.changed_paths(advanced, head)
            self.assertEqual(len(paths), 313)
            self.assertIn("src/old.rs", paths)
            self.assertIn("docs/moved.md", paths)
            self.assertIn("tests/space and\nnewline.rs", paths)
            self.assertNotIn("only-on-base.rs", paths)
            self.assertTrue(ci.select(paths)["build"])


if __name__ == "__main__":
    unittest.main()

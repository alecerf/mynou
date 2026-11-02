"""Retain PR measurement coverage without starting unselected macOS work."""
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import measurement_plan as planner


class MeasurementPlanTests(unittest.TestCase):
    def selected(self, paths):
        return {stage for stage, selected in planner.pr_plan(paths).items() if selected}

    def test_existing_paths_preserve_their_measurements(self):
        cases = {
            ".github/scripts/torrent_load.py": {"torrent"},
            "src/torrent.rs": {"torrent"},
            "src/torrent/load_tests.rs": {"torrent"},
            "src/torrent/nested/test.rs": {"torrent"},
            ".github/scripts/tunnel_load.py": {"tunnel"},
            "src/tunnel_http_tests.rs": {"tunnel"},
            "src/tunnel_load_tests.rs": {"tunnel"},
            ".github/scripts/inventory_growth.py": {"growth"},
            "src/growth.rs": {"growth"},
        }
        for path in ("src/tunnel.rs", "src/tunnel_http.rs", "src/tunnel_network.rs",
                     "src/netstack.rs", "src/netstack/tcp.rs", "src/netstack/nested/test.rs"):
            cases[path] = {"torrent", "tunnel"}
        for path, expected in cases.items():
            with self.subTest(path=path):
                self.assertEqual(self.selected([path]), expected)

    def test_shared_inputs_select_all_pr_stages(self):
        for path in ("Cargo.toml", "Cargo.lock", "rust-toolchain.toml", ".cargo/config.toml",
                     ".cargo/nested/config", ".github/workflows/measurements.yml",
                     ".github/scripts/measurement_plan.py", ".github/scripts/cargo_artifacts.py",
                     ".github/scripts/tests/test_measurement_plan.py",
                     ".github/scripts/tests/test_cargo_artifacts.py"):
            with self.subTest(path=path):
                self.assertEqual(self.selected([path]), {"torrent", "tunnel", "growth"})

    def test_unrelated_and_representative_only_changes_start_no_measurements(self):
        self.assertEqual(self.selected([]), set())
        self.assertEqual(self.selected(["docs/torrent-load.md", "src/growth_other.rs",
                                       "src/tunnel_http.rs.bak", "other/Cargo.toml",
                                       ".github/scripts/representative_load.py"]), set())

    def test_union_of_paths_never_selects_representative(self):
        self.assertEqual(self.selected(["src/torrent.rs", "src/growth.rs"]), {"torrent", "growth"})
        self.assertFalse(planner.pr_plan(["Cargo.toml"])["representative"])

    def test_dispatch_defaults_and_explicit_booleans(self):
        self.assertEqual(planner.dispatch_plan({}), {
            "torrent": True, "tunnel": True, "growth": False, "representative": False})
        for enabled, disabled in ((True, False), ("true", "false")):
            with self.subTest(enabled=enabled):
                self.assertEqual(planner.dispatch_plan({
                    "torrent": disabled, "tunnel": disabled,
                    "growth": enabled, "representative": enabled,
                    "smoke": "true", "engine": "false", "queue_mode": "default",
                }), {"torrent": False, "tunnel": False, "growth": True, "representative": True})
        self.assertFalse(any(planner.dispatch_plan(dict.fromkeys(planner.STAGES, False)).values()))
        with self.assertRaisesRegex(ValueError, "invalid boolean"):
            planner.dispatch_plan({"torrent": "yes"})

    def test_full_three_dot_diff_handles_many_paths_and_removed_rename_sources(self):
        paths = [f"docs/{index}.md" for index in range(400)] + [
            "src/torrent/old.rs", "archive/new name\nwith newline.rs", "src/growth.rs"]
        raw = b"\0".join(path.encode() for path in paths) + b"\0"
        event = {"pull_request": {"base": {"sha": "base-sha"}, "head": {"sha": "head-sha"}}}
        with patch.object(planner.subprocess, "check_output", return_value=raw) as diff:
            self.assertEqual(planner.changed_paths("base-sha", "head-sha"), paths)
            selected = planner.plan("pull_request", event)
        self.assertEqual(diff.call_args.args[0], [
            "git", "diff", "--name-only", "--no-renames", "-z", "base-sha...head-sha", "--"])
        self.assertTrue(selected["torrent"])
        self.assertTrue(selected["growth"])
        self.assertFalse(selected["representative"])

    def test_git_errors_and_unsupported_events_fail_closed(self):
        with patch.object(planner.subprocess, "check_output", side_effect=subprocess.CalledProcessError(1, "git")):
            with self.assertRaises(subprocess.CalledProcessError):
                planner.changed_paths("missing-base", "head")
        with self.assertRaisesRegex(ValueError, "unsupported"):
            planner.plan("push", {})

    def test_cli_emits_all_job_outputs_including_no_work(self):
        with tempfile.TemporaryDirectory() as directory:
            event = Path(directory) / "event.json"
            output = Path(directory) / "outputs"
            event.write_text(json.dumps({"inputs": dict.fromkeys(planner.STAGES, "false")}))
            result = subprocess.run([
                sys.executable, planner.__file__, "--event", str(event),
                "--event-name", "workflow_dispatch", "--github-output", str(output),
            ], text=True, capture_output=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(output.read_text(), result.stdout)
            self.assertEqual(result.stdout,
                             "torrent=false\ntunnel=false\ngrowth=false\nrepresentative=false\nany=false\n")


if __name__ == "__main__":
    unittest.main()

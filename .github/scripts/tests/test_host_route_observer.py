"""Exercise cleanup and fail-closed gates without privileges or route changes."""
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import Mock, patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import host_route_observer as routes


class RouteTests(unittest.TestCase):
    def test_operator_machines_are_refused_before_any_command(self):
        with patch.dict(routes.os.environ, {}, clear=True), patch.object(routes, "route") as command:
            with self.assertRaisesRegex(ValueError, "disposable"):
                routes.require_disposable_runner()
            command.assert_not_called()

    def test_route_snapshots_require_the_exact_destination_interface_and_flags(self):
        with tempfile.TemporaryDirectory() as directory:
            for destination, interface, flags, expected, valid in (
                    ("127.0.0.2", "lo0", "UP,HOST,REJECT", True, True),
                    ("127.0.0.2", "lo0", "UP,HOST", False, True),
                    ("127.0.0.1", "lo0", "REJECT", True, False),
                    ("127.0.0.2", "en0", "REJECT", True, False),
                    ("127.0.0.2", "lo0", "UP,HOST", True, False),
                    ("127.0.0.2", "lo0", "REJECT", False, False)):
                text = f"destination: {destination}\ninterface: {interface}\nflags: <{flags}>\n"
                with self.subTest(text=text), patch.object(routes, "route", return_value=text):
                    if valid:
                        routes.snapshot(Path(directory), "state", expected)
                    else:
                        with self.assertRaises(ValueError):
                            routes.snapshot(Path(directory), "state", expected)

    def test_existing_route_is_never_deleted(self):
        with tempfile.TemporaryDirectory() as directory, patch.object(
                routes, "route", return_value="destination: 127.0.0.2\n") as command, \
                patch.object(routes, "alias") as alias:
            with self.assertRaisesRegex(ValueError, "existing fixture host route"):
                routes.exercise("harness", Path("observer"), Path(directory) / "case", "test")
            self.assertEqual(command.call_count, 1)
            self.assertEqual(command.call_args.args[0], "get")
            alias.assert_not_called()

    def test_route_is_cleaned_if_the_fixture_dies_before_request(self):
        process = Mock()
        process.poll.return_value = 1
        with tempfile.TemporaryDirectory() as directory, patch.object(routes, "route") as command, \
                patch.object(routes, "ensure_unused"), patch.object(routes, "alias") as alias, \
                patch.object(routes, "snapshot"), patch.object(routes.subprocess, "Popen", return_value=process):
            with self.assertRaisesRegex(ValueError, "stopped"):
                routes.exercise("harness", Path("observer"), Path(directory) / "case", "test")
            self.assertEqual(command.call_args.args, ("delete", "-host", routes.DESTINATION))
            self.assertEqual([call.args for call in alias.call_args_list], [(True,), (False,)])

    def test_live_child_is_killed_and_route_cleaned_on_observation_error(self):
        process = Mock()
        process.poll.return_value = None
        with tempfile.TemporaryDirectory() as directory, patch.object(routes, "route") as command, \
                patch.object(routes, "ensure_unused"), patch.object(routes, "alias") as alias, \
                patch.object(routes, "snapshot"), patch.object(routes.subprocess, "Popen", return_value=process), \
                patch.object(routes.time, "sleep", side_effect=RuntimeError("injected")):
            with self.assertRaisesRegex(RuntimeError, "injected"):
                routes.exercise("harness", Path("observer"), Path(directory) / "case", "test")
            process.kill.assert_called_once()
            process.wait.assert_called_once()
            self.assertEqual(command.call_args.args, ("delete", "-host", routes.DESTINATION))
            self.assertEqual([call.args for call in alias.call_args_list], [(True,), (False,)])

    def test_real_route_protocol_requires_both_states_before_accepting_the_trace(self):
        process = Mock()
        process.wait.return_value = 0
        process.poll.return_value = 0
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "case"

            def launch(*args, **kwargs):
                for state in ("lost", "restored"):
                    (output / "control" / ("request-" + state)).write_text("ready")
                return process

            with patch.object(routes, "ensure_unused"), patch.object(routes, "alias") as alias, \
                    patch.object(routes, "route") as command, patch.object(routes, "snapshot") as snapshot, \
                    patch.object(routes.subprocess, "Popen", side_effect=launch), \
                    patch.object(routes.observer, "read_trace", return_value=[]), \
                    patch.object(routes.observer, "validate", return_value={"sendto": 4}):
                result = routes.exercise("harness", Path("observer"), output, "test")
                self.assertEqual(result["states"], ["lost", "restored"])
                self.assertEqual([call.args for call in command.call_args_list], [
                    ("change", "-host", routes.DESTINATION, "127.0.0.1", "-reject"),
                    ("change", "-host", routes.DESTINATION, "-interface", "lo0"),
                    ("delete", "-host", routes.DESTINATION)])
                self.assertEqual([call.args[1:] for call in snapshot.call_args_list], [
                    ("before", False), ("lost", True), ("restored", False)])
                self.assertEqual([call.args for call in alias.call_args_list], [(True,), (False,)])
                self.assertTrue((output / "control" / "ack-restored").is_file())

    def test_alias_is_still_removed_if_route_cleanup_fails(self):
        process = Mock()
        process.poll.return_value = 1
        with tempfile.TemporaryDirectory() as directory, patch.object(routes, "ensure_unused"), \
                patch.object(routes, "alias") as alias, patch.object(routes, "snapshot"), \
                patch.object(routes.subprocess, "Popen", return_value=process), \
                patch.object(routes, "route", side_effect=RuntimeError("cleanup failed")):
            with self.assertRaisesRegex(RuntimeError, "cleanup failed"):
                routes.exercise("harness", Path("observer"), Path(directory) / "case", "test")
            self.assertEqual([call.args for call in alias.call_args_list], [(True,), (False,)])


if __name__ == "__main__":
    unittest.main()

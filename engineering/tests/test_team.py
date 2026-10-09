"""Opt-in parallel team model: worker leases, areas, capacity, drain and recovery. Run in CI only."""
from copy import deepcopy
from datetime import timedelta
from pathlib import Path
import sys
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import commands
import control
import github
import lease
import qa
from test_commands import AT, HEAD, SOURCE, Native, initial

EVIDENCE = {"issue": True, "branch": True, "pr": True, "ci": True, "commit_preserved": True}
BRANCH = "work/team"


def team(cap=3):
    return {"schema": 3, "max_active_agents": cap, "generation": 0, "lease": None,
        "checkpoint": {}, "attempts": {}, "workers": {}}


def start(state, number, labels, at=AT, identity=None, role="rust"):
    return lease.acquire_worker(state, at, identity or f"w{number}", "worker", role, number,
        f"{BRANCH}-{number}", SOURCE, None, labels)


def issue(number, *names):
    return {"number": number, "state": "open", "labels": [{"name": n} for n in ("agent-work", "status:ready", *names)]}


class WorkerLeaseCases(unittest.TestCase):
    def test_schema_one_state_is_unchanged_and_rejects_workers(self):
        self.assertEqual(lease.validate(initial()), initial())
        mixed = dict(initial(), workers={})
        with self.assertRaises(ValueError):
            lease.validate(mixed)
        self.assertFalse(lease.is_team(initial()))
        for bad in [1, 9]:
            with self.assertRaises(ValueError):
                lease.validate(team(bad))

    def test_scope_reads_areas_and_defaults_to_exclusive(self):
        self.assertEqual(lease.scope(["bug", "area:web", "area:usenet"]), (["usenet", "web"], False))
        self.assertEqual(lease.scope(["bug"]), ([], True))
        self.assertEqual(lease.scope(["area:control", "area:web"])[1], True)
        with self.assertRaises(ValueError):
            lease.scope(["area:"])

    def test_disjoint_areas_run_together_and_overlap_loses(self):
        state = start(team(), 1, ["area:web"])
        state = start(state, 2, ["area:usenet"])
        self.assertEqual(len(lease.live(state, AT)), 2)
        with self.assertRaisesRegex(ValueError, "Overlapping"):
            start(state, 3, ["area:web", "area:docs"])
        with self.assertRaisesRegex(ValueError, "already has"):
            start(state, 2, ["area:docs"], identity="other")

    def test_exclusive_needs_empty_team_and_blocks_everyone(self):
        state = start(team(), 1, ["area:web"])
        for labels in [["area:control"], []]:
            with self.assertRaisesRegex(ValueError, "Exclusive"):
                start(state, 2, labels)
        solo = start(team(), 2, ["area:control"])
        self.assertTrue(solo["workers"]["w2"]["exclusive"])
        with self.assertRaisesRegex(ValueError, "exclusive worker"):
            start(solo, 3, ["area:web"])

    def test_capacity_counts_only_valid_workers(self):
        state = start(start(team(2), 1, ["area:web"]), 2, ["area:docs"])
        with self.assertRaisesRegex(ValueError, "cap"):
            start(state, 3, ["area:torrent"])
        later = AT + timedelta(minutes=46)
        again = start(state, 3, ["area:torrent"], at=later, identity="w3")
        self.assertEqual(len(again["workers"]), 3)
        with self.assertRaisesRegex(ValueError, "already has"):
            start(again, 1, ["area:other"], at=later, identity="x")

    def test_checkpoint_release_and_wrong_owner(self):
        state = start(team(), 1, ["area:web"])
        with self.assertRaisesRegex(ValueError, "Checkpoint"):
            lease.release_worker(state, AT, "w1")
        state = lease.checkpoint_worker(state, AT + timedelta(minutes=5), "w1", "Preserved", "QA next", SOURCE, 9)
        self.assertEqual(state["workers"]["w1"]["pr"], 9)
        state = lease.checkpoint_worker(state, AT + timedelta(minutes=6), "w1", "Preserved", "QA next")
        for bad in ["nobody"]:
            with self.assertRaises(ValueError):
                lease.checkpoint_worker(state, AT, bad, "x", "y")
        with self.assertRaises(ValueError):
            lease.release_worker(state, AT + timedelta(minutes=60), "w1")
        state = lease.release_worker(state, AT + timedelta(minutes=7), "w1")
        self.assertEqual(state["workers"], {})

    def test_each_expired_worker_recovers_individually(self):
        state = start(start(team(), 1, ["area:web"]), 2, ["area:docs"])
        later = AT + timedelta(minutes=50)
        with self.assertRaises(ValueError):
            lease.recover_worker(state, AT, "w1", EVIDENCE)
        with self.assertRaises(ValueError):
            lease.recover_worker(state, later, "w1", dict(EVIDENCE, ci=False))
        state = lease.recover_worker(state, later, "w1", EVIDENCE)
        self.assertEqual(sorted(state["workers"]), ["w2"])

    def test_unchanged_attempts_trip_the_breaker_for_one_worker(self):
        state = start(start(team(), 1, ["area:web"]), 2, ["area:docs"])
        for _ in range(3):
            state = lease.attempt_worker(state, AT, "w1", "same", "same")
        self.assertEqual(sorted(state["workers"]), ["w2"])
        self.assertEqual(state["attempts"]["1"]["unchanged"], 3)

    def test_enable_team_is_reviewed_and_opt_in(self):
        held = lease.acquire(initial(), AT, "m1", "worker", "master", 58, BRANCH, SOURCE)
        with self.assertRaisesRegex(ValueError, "Checkpoint"):
            lease.enable_team(held, AT, "m1", 58, 3)
        held = lease.checkpoint(held, AT, "m1", "Reviewed", "Enable", SOURCE)
        with self.assertRaises(ValueError):
            lease.enable_team(held, AT, "m1", 59, 3)
        with self.assertRaises(ValueError):
            lease.enable_team(held, AT, "m1", 58, 99)
        enabled = lease.enable_team(held, AT, "m1", 58, 3)
        self.assertEqual((enabled["schema"], enabled["max_active_agents"], enabled["workers"]), (3, 3, {}))
        self.assertEqual(enabled["lease"]["id"], "m1")
        self.assertEqual(enabled["checkpoint"]["team"]["cap"], 3)
        with self.assertRaises(ValueError):
            lease.enable_team(enabled, AT, "m1", 58, 3)

    def test_historical_review_lease_may_live_in_the_worker_table(self):
        state = start(team(), 5, ["area:web"], identity="r1", role="qa")
        state["workers"]["r1"]["pr"] = 6
        pr = {"number": 6, "head": {"sha": SOURCE}}
        review = {"submitted_at": lease.stamp(AT + timedelta(minutes=1))}
        qa.historical_lease(state, review, {}, "qa", pr, [5])
        with self.assertRaises(ValueError):
            qa.historical_lease(state, review, {}, "security", pr, [5])
        with self.assertRaises(ValueError):
            qa.historical_lease(state, {"submitted_at": lease.stamp(AT + timedelta(minutes=46))}, {}, "qa", pr, [5])


class SelectionCases(unittest.TestCase):
    def numbers(self, action):
        return [a["issue"] for a in action["assignments"]]

    def test_assigns_disjoint_issues_up_to_capacity(self):
        issues = [issue(1, "priority:p1", "area:web"), issue(2, "priority:p1", "area:usenet"),
            issue(3, "priority:p1", "area:docs"), issue(4, "priority:p2", "area:torrent")]
        action = control.select(team(3), issues, AT)
        self.assertEqual(self.numbers(action), [1, 2, 3])
        self.assertEqual(action["action"], "work")
        self.assertEqual(action["capacity"], 3)

    def test_overlapping_area_is_skipped_for_disjoint_lower_priority(self):
        issues = [issue(1, "priority:p1", "area:web"), issue(2, "priority:p1", "area:web"), issue(3, "priority:p2", "area:docs")]
        self.assertEqual(self.numbers(control.select(team(), issues, AT)), [1, 3])

    def test_exclusive_top_priority_drains_instead_of_starting_parallel_work(self):
        issues = [issue(1, "priority:p0", "area:control"), issue(2, "priority:p2", "area:web")]
        running = start(team(), 9, ["area:docs"])
        action = control.select(running, issues + [issue(9, "area:docs")], AT)
        self.assertEqual(action["assignments"], [])
        self.assertEqual(action["action"], "no-capacity")
        idle = control.select(team(), issues, AT)
        self.assertEqual(self.numbers(idle), [1])
        self.assertTrue(idle["assignments"][0]["exclusive"])

    def test_running_exclusive_worker_makes_team_busy(self):
        state = start(team(), 1, ["area:control"])
        action = control.select(state, [issue(1, "area:control"), issue(2, "area:web")], AT)
        self.assertEqual(action["action"], "busy")
        self.assertEqual(action["assignments"], [])

    def test_cap_reached_reports_no_capacity(self):
        state = start(start(team(2), 1, ["area:web"]), 2, ["area:docs"])
        action = control.select(state, [issue(1, "area:web"), issue(2, "area:docs"), issue(3, "area:torrent")], AT)
        self.assertEqual((action["action"], action["assignments"], action["capacity"]), ("no-capacity", [], 0))

    def test_expired_worker_is_recovered_first_and_its_issue_not_reassigned(self):
        state = start(team(), 1, ["area:web"])
        later = AT + timedelta(minutes=50)
        action = control.select(state, [issue(1, "area:web"), issue(2, "area:docs")], later)
        self.assertEqual(action["assignments"][0], {"action": "recover", "issue": 1, "lease": "w1"})
        self.assertEqual(self.numbers(action), [1, 2])

    def test_delivery_lease_does_not_block_assignment(self):
        state = lease.acquire(team(), AT, "d1", "delivery", "quality", 7, BRANCH, SOURCE)
        action = control.select(state, [issue(1, "area:web")], AT)
        self.assertEqual(self.numbers(action), [1])

    def test_schema_one_selection_is_unchanged(self):
        action = control.select(initial(), [issue(1, "area:web"), issue(2, "area:docs")], AT)
        self.assertEqual((action["action"], action["issue"]), ("work", 1))
        self.assertNotIn("assignments", action)


class TeamNative(Native):
    def __init__(self):
        super().__init__()
        self.cfg["team"] = {"issue": 58, "max_active_agents": 3}
        self.state = team()
        self.labels = {5: ["area:web"], 7: ["area:usenet"], 8: ["area:web"], 9: [], 10: ["area:docs"]}

    def rest(self, method, path, value=None):
        if method == "GET" and path.startswith("issues/") and path.count("/") == 1 and int(path[7:]) in self.labels:
            number = int(path[7:])
            return {"number": number, "state": "open", "labels": [{"name": n} for n in ["agent-work", *self.labels[number]]]}
        return super().rest(method, path, value)

    def pages(self, path, key=None):
        if path.startswith("issues/") and path.endswith("/dependencies/blocked_by"):
            return []
        return super().pages(path, key)


class TeamCommandCases(unittest.TestCase):
    def run_command(self, native, event, at=AT):
        with patch.object(lease, "now", return_value=at):
            return commands.execute(native, event, at, "alecerf", "alecerf")

    def acquire(self, native, number, worker):
        args = {"worker": worker, "role": "rust", "issue": number, "branch": f"work/{number}", "commit": SOURCE, "pr": None}
        return self.run_command(native, native.event("acquire", args))["lease_id"]

    def test_concurrent_workers_win_only_for_disjoint_issues_and_areas(self):
        native = TeamNative()
        first, second = self.acquire(native, 5, "one"), self.acquire(native, 7, "two")
        self.assertEqual(sorted(native.state["workers"]), sorted([first, second]))
        writes = len(native.writes)
        for number in [8, 9]:
            with self.assertRaises(ValueError):
                self.acquire(native, number, "three")
        with self.assertRaises(ValueError):
            self.acquire(native, 5, "again")
        self.assertEqual(len(native.writes), writes)
        # A sibling writer that wins the head leaves a stale command unable to write.
        native.race = True
        with self.assertRaises(RuntimeError):
            self.acquire(native, 10, "late")

    def test_worker_checkpoint_release_and_wrong_worker(self):
        native = TeamNative()
        identity = self.acquire(native, 5, "one")
        args = {"worker": "one", "lease": identity}
        with self.assertRaises(ValueError):
            self.run_command(native, native.event("release", args))
        wrong = dict(args, worker="intruder", summary="Work", next_action="QA", commit=SOURCE, pr=None)
        with self.assertRaises(commands.Rejected):
            self.run_command(native, native.event("checkpoint", wrong))
        good = dict(args, summary="Work", next_action="QA", commit=SOURCE, pr=None)
        self.run_command(native, native.event("checkpoint", good))
        self.assertEqual(native.state["workers"][identity]["checkpoint"]["next_action"], "QA")
        self.assertEqual(native.state["checkpoint"], {})
        self.run_command(native, native.event("release", args))
        self.assertEqual(native.state["workers"], {})

    def test_recover_with_unknown_worker_lease_is_rejected(self):
        native = TeamNative()
        with self.assertRaises(ValueError):
            self.run_command(native, native.event("recover", {"lease": "nobody"}))

    def test_enable_team_requires_reviewed_config_and_master_lease(self):
        native = TeamNative()
        held = lease.acquire(initial(), AT, "m1", "one", "master", 58, BRANCH, SOURCE)
        native.state = lease.checkpoint(held, AT, "m1", "Reviewed", "Enable", SOURCE)
        args = {"worker": "one", "lease": "m1"}
        self.run_command(native, native.event("enable-team", args))
        self.assertEqual((native.state["schema"], native.state["max_active_agents"]), (3, 3))
        with self.assertRaises(commands.Rejected):
            self.run_command(native, native.event("enable-team", args))
        unconfigured = TeamNative()
        del unconfigured.cfg["team"]
        unconfigured.state = lease.checkpoint(held, AT, "m1", "Reviewed", "Enable", SOURCE)
        with self.assertRaises(commands.Rejected):
            self.run_command(unconfigured, unconfigured.event("enable-team", args))
        self.assertEqual(unconfigured.state["schema"], 1)

    def test_team_policy_bounds(self):
        self.assertIsNone(github.team_policy({}))
        self.assertEqual(github.team_policy({"team": {"issue": 58, "max_active_agents": 3}})["max_active_agents"], 3)
        for bad in [{"issue": 58}, {"issue": 58, "max_active_agents": 1}, {"issue": 58, "max_active_agents": 9},
                {"issue": 0, "max_active_agents": 3}, {"issue": 58, "max_active_agents": True}, []]:
            with self.assertRaises(ValueError):
                github.team_policy({"team": bad})

    def test_schema_one_acquire_still_serial(self):
        native = TeamNative()
        native.state = initial()
        self.acquire(native, 5, "one")
        self.assertIsNotNone(native.state["lease"])
        with self.assertRaisesRegex(commands.Rejected, "lease-not-free"):
            self.acquire(native, 7, "two")


if __name__ == "__main__":
    unittest.main()

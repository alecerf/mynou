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
import delivery
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
        with self.assertRaisesRegex(ValueError, "expired"):
            start(state, 3, ["area:torrent"], at=later, identity="w3")
        state = lease.recover_worker(state, later, "w1", EVIDENCE)
        state = lease.recover_worker(state, later, "w2", EVIDENCE)
        again = start(state, 3, ["area:torrent"], at=later, identity="w3")
        self.assertEqual(len(again["workers"]), 1)

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
        self.assertEqual(self.numbers(action), [1])

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

    def activation_owner(self):
        native = TeamNative()
        held = lease.acquire(initial(), AT, "m1", "one", "master", 58, "trunk", SOURCE, 6)
        native.state = lease.checkpoint(held, AT, "m1", "Installed source checkpoint", "Verify real gates", SOURCE, 6)
        native.pr.update(body="Closes #58", merged_at=lease.stamp(AT), merge_commit_sha=SOURCE)
        return native, {"worker": "one", "lease": "m1"}

    def activation_proof(self):
        return {"merge": SOURCE, "issues": [58], "reviews": [{"role": "security", "review_id": 11},
            {"role": "qa", "review_id": 12}], "ci": ["https://example.invalid/native-ci"]}

    def test_enable_team_requires_reviewed_config_and_master_lease(self):
        native, args = self.activation_owner()
        with patch.object(qa, "evaluate", return_value=self.activation_proof()) as review, \
                patch.object(qa, "ci_ready") as checks, \
                patch.object(delivery, "default_ci_run", return_value={"id": 13, "status": "completed", "conclusion": "success"}):
            self.run_command(native, native.event("enable-team", args))
        review.assert_called_once_with(native, 6, include_gate=True, allow_merged=True)
        checks.assert_called_once_with(native, native.cfg, SOURCE)
        self.assertEqual((native.state["schema"], native.state["max_active_agents"]), (3, 3))
        self.assertEqual(native.state["checkpoint"]["team"]["policy_commit"], SOURCE)
        self.assertEqual(native.state["checkpoint"]["team"]["reviews"], self.activation_proof()["reviews"])
        with self.assertRaises(commands.Rejected):
            self.run_command(native, native.event("enable-team", args))
        unconfigured, args = self.activation_owner()
        del unconfigured.cfg["team"]
        with self.assertRaises(commands.Rejected):
            self.run_command(unconfigured, unconfigured.event("enable-team", args))
        self.assertEqual(unconfigured.state["schema"], 1)

    def test_missing_reviews_pending_default_ci_and_wrong_source_cannot_enable(self):
        for failure in ["review", "pending", "source", "issue", "merge"]:
            with self.subTest(failure=failure):
                native, args = self.activation_owner()
                if failure == "source":
                    native.state["lease"]["branch"] = BRANCH
                proof = self.activation_proof()
                if failure == "issue":
                    proof["issues"] = [59]
                if failure == "merge":
                    proof["merge"] = "c" * 40
                with patch.object(qa, "evaluate", side_effect=ValueError("Missing real review") if failure == "review" else None,
                        return_value=proof), patch.object(qa, "ci_ready"), \
                        patch.object(delivery, "default_ci_run", return_value={"id": 13,
                            "status": "in_progress" if failure == "pending" else "completed", "conclusion": "success"}):
                    with self.assertRaises(ValueError):
                        self.run_command(native, native.event("enable-team", args))
                self.assertFalse(native.writes)
                self.assertEqual(native.state["schema"], 1)

    def test_default_race_during_activation_preserves_serial_state(self):
        native, args = self.activation_owner()
        reads = 0
        def source(location):
            nonlocal reads
            if location == "control/engineering":
                return native.head
            reads += 1
            return SOURCE if reads == 1 else "c" * 40
        with patch.object(native, "ref", side_effect=source), \
                patch.object(qa, "evaluate", return_value=self.activation_proof()), patch.object(qa, "ci_ready"), \
                patch.object(delivery, "default_ci_run", return_value={"id": 13, "status": "completed", "conclusion": "success"}):
            with self.assertRaisesRegex(ValueError, "source changed"):
                self.run_command(native, native.event("enable-team", args))
        self.assertFalse(native.writes)
        self.assertEqual(native.state["schema"], 1)

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



class TeamSafetyRegressions(unittest.TestCase):
    def test_exclusive_control_and_singleton_delivery_fence_each_other(self):
        exclusive = start(team(), 1, ["area:control"])
        with self.assertRaisesRegex(ValueError, "Exclusive"):
            lease.acquire(exclusive, AT, "d1", "delivery", "quality", 7, "trunk", SOURCE)
        singleton = lease.acquire(team(), AT, "d1", "delivery", "quality", 7, "trunk", SOURCE)
        with self.assertRaisesRegex(ValueError, "Singleton"):
            start(singleton, 1, ["area:control"])
        with self.assertRaisesRegex(ValueError, "Singleton"):
            start(singleton, 7, ["area:web"])
        allowed = start(singleton, 1, ["area:web"])
        self.assertEqual(len(allowed["workers"]), 1)

    def test_expired_workers_block_delivery_until_evidence_based_recovery(self):
        state = start(team(), 1, ["area:web"])
        later = AT + timedelta(minutes=46)
        with self.assertRaisesRegex(ValueError, "expired"):
            lease.acquire(state, later, "d1", "delivery", "quality", 7, "trunk", SOURCE)
        recovered = lease.recover_worker(state, later, "w1", EVIDENCE)
        held = lease.acquire(recovered, later, "d1", "delivery", "quality", 7, "trunk", SOURCE)
        self.assertEqual(held["lease"]["id"], "d1")

    def test_delivery_and_sweep_do_no_domain_reads_beside_exclusive_control(self):
        state = start(team(), 1, ["area:control"])
        native = TeamNative()
        for sweep in [False, True]:
            with patch.object(delivery, "read_state", return_value=(HEAD, state)), \
                    patch.object(lease, "now", return_value=AT), patch.object(native, "pages") as pages:
                self.assertEqual(delivery.run(native, sweep)["action"], "busy")
                pages.assert_not_called()

    def test_released_recovered_and_breaker_handoffs_preserve_source_and_durable_records(self):
        durable = {"publication": {"commit": SOURCE, "state": "published"},
            "control_migration": {"phase": "retired"}}
        for outcome in ["released", "recovered", "circuit-breaker"]:
            with self.subTest(outcome=outcome):
                state = start(team(), 1, ["area:web"])
                state["checkpoint"] = deepcopy(durable)
                if outcome == "released":
                    state = lease.checkpoint_worker(state, AT, "w1", "Work preserved", "Independent QA", SOURCE, 6)
                    result = lease.release_worker(state, AT, "w1")
                elif outcome == "recovered":
                    result = lease.recover_worker(state, AT + timedelta(minutes=46), "w1",
                        dict(EVIDENCE, branch_head=SOURCE, preservation="branch/default ancestry"))
                else:
                    for _ in range(3):
                        state = lease.attempt_worker(state, AT, "w1", "same", "same")
                    result = state
                note = result["checkpoint"]["worker_handoffs"]["1"]
                self.assertEqual(note["commit"], SOURCE)
                self.assertEqual(note["branch"], BRANCH + "-1")
                self.assertEqual(note["lease_id"], "w1")
                self.assertEqual(note["outcome"], outcome)
                self.assertNotIn("w1", result["workers"])
                for key, value in durable.items():
                    self.assertEqual(result["checkpoint"][key], value)
                if outcome == "recovered":
                    self.assertEqual(note["recovery"]["branch_head"], SOURCE)


    def test_full_or_unconfigured_team_wake_skips_native_backlog_reads(self):
        full = start(start(team(2), 1, ["area:web"]), 2, ["area:docs"])
        for configured in [True, False]:
            native = TeamNative()
            if not configured:
                del native.cfg["team"]
            with patch.object(control, "GitHub", return_value=native), \
                    patch.object(control, "read_state", return_value=(HEAD, full)), \
                    patch.object(sys, "argv", ["control.py", "wake"]), \
                    patch.object(lease, "now", return_value=AT), patch.object(native, "pages") as pages, \
                    patch("builtins.print") as output:
                control.main()
            pages.assert_not_called()
            self.assertIn('"action": "no-capacity"' if configured else '"action": "blocked"', output.call_args.args[0])

    def test_branch_audit_and_fresh_delete_fence_preserve_worker_branch(self):
        native = TeamNative()
        base = lease.acquire(team(), AT, "d1", "delivery", "quality", 7, "trunk", SOURCE)
        target = BRANCH + "-2"
        late = start(base, 2, ["area:web"])
        cfg = native.cfg
        allowed, reason = delivery.branch_decision(target, SOURCE, cfg, "identical", [], [],
            base["lease"], workers=late["workers"].values())
        self.assertFalse(allowed)
        self.assertIn("lease", reason)
        def pages(path, key=None):
            if path == "branches":
                return [{"name": target, "commit": {"sha": SOURCE}, "protected": False}]
            if path in ["pulls?state=all", "issues?state=open"]:
                return []
            raise AssertionError(path)
        def rest(method, path, value=None):
            if method == "GET" and path.startswith("compare/"):
                return {"status": "identical"}
            if method == "POST" and path == "issues/7/comments":
                return {"id": 1}
            raise AssertionError((method, path))
        with patch.object(native, "pages", side_effect=pages), patch.object(native, "rest", side_effect=rest) as calls, \
                patch.object(delivery, "read_state", side_effect=[(HEAD, base), (HEAD, late)]), \
                patch.object(lease, "now", return_value=AT):
            with self.assertRaisesRegex(ValueError, "protects this branch"):
                delivery.cleanup_branch(native, target, SOURCE, base["lease"])
        self.assertFalse(any(call.args[0] == "DELETE" for call in calls.call_args_list))


if __name__ == "__main__":
    unittest.main()

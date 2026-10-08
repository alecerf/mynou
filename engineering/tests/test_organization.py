"""Original offline control-plane scenarios. Execution belongs only in CI."""
from copy import deepcopy
from datetime import datetime, timedelta, timezone
import json
from pathlib import Path
import sys
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import control
import delivery
from github import APIError, GitHub, NoRedirect
import lease
import project
import qa

AT = datetime(2026, 10, 6, 20, 0, tzinfo=timezone.utc)
HEAD = "a" * 40
BASE = "d" * 40
QA_CHECKPOINT = "b" * 40
SECURITY_CHECKPOINT = "c" * 40
CFG = {"repository": "original/mynou", "default_branch": "trunk", "control_branch": "control/engineering", "state_path": "state.json",
    "trusted_reviewers": ["original"], "project": None,
    "required_workflows": {"Mynou CI": ["validate", "GNU", "musl", "package"], "Engineering checks": ["organization"]}}


def state():
    return {"schema": 1, "max_active_agents": 1, "generation": 0, "lease": None, "checkpoint": {}, "attempts": {}}


def held(role="quality", identity="original-owner"):
    return lease.acquire(state(), AT, identity, "one-worker", role, 1, "work/original", HEAD, 3)


def issue(number=1, extra=(), status="ready", origin="user", priority="p1"):
    return {"number": number, "state": "open", "title": "Original intent", "body": "Original bounded scope", "labels": [{"name": n} for n in
        ["agent-work", "status:" + status, "origin:" + origin, "priority:" + priority, "risk:high", *extra]]}


def pr():
    return {"number": 3, "state": "open", "draft": False, "body": "Closes #1", "mergeable": True,
        "head": {"sha": HEAD, "ref": "work/original", "repo": {"full_name": "original/mynou"}}, "base": {"sha": BASE, "ref": "trunk"}}


def review(role="qa", verdict="passed", number=10):
    value = {"schema": 1, "role": role, "head_sha": HEAD, "base_sha": BASE,
        "lease_checkpoint": QA_CHECKPOINT if role == "qa" else SECURITY_CHECKPOINT,
        "verdict": verdict, "summary": "Original offline review fixture", "reviewed_paths": ["src/original.rs"],
        "acceptance": ["Original acceptance inspected"], "evidence": ["Original fake CI data, not a real run"],
        "findings": [] if verdict == "passed" else ["A correctness finding remains"]}
    return {"id": number, "user": {"login": "original"}, "commit_id": HEAD, "state": "COMMENT",
        "submitted_at": lease.stamp(AT + timedelta(minutes=2)),
        "body": f"<!-- mynou-{role}:v1 -->\n```json\n" + json.dumps(value) + "\n```"}


class Native:
    def __init__(self):
        self.cfg = CFG
        self.repo = CFG["repository"]
        self.current_pr = pr()
        self.reviews = [review(), review("security", number=11)]
        self.files = [{"filename": "src/original.rs"}]
        self.issues = [issue()]
        self.bad_job = None
        self.open_thread = False
        self.ancestry = "ahead"
        self.deleted = []
        self.comments = []
        self.states = {QA_CHECKPOINT: held("qa"), SECURITY_CHECKPOINT: held("security")}

    def rest(self, method, path, value=None):
        if method == "DELETE":
            self.deleted.append(path)
            return None
        if path == "pulls/3":
            return deepcopy(self.current_pr)
        if path == "issues/1":
            return deepcopy(self.issues[0])
        if path.startswith("compare/"):
            return {"status": self.ancestry}
        raise AssertionError((method, path, value))

    def pages(self, path, key=None):
        if path == "pulls/3/reviews": return deepcopy(self.reviews)
        if path == "pulls/3/files": return self.files
        if path.startswith("actions/runs?head_sha="):
            return [{"id": i + 20, "name": name, "head_sha": self.current_pr["head"]["sha"], "status": "completed", "conclusion": "success", "created_at": lease.stamp(AT), "run_attempt": 1,
                "html_url": "https://example.invalid/original-ci/" + str(i)} for i, name in enumerate(CFG["required_workflows"])]
        if path.startswith("actions/runs/20/jobs"):
            names = CFG["required_workflows"]["Mynou CI"]
        elif path.startswith("actions/runs/21/jobs"):
            names = ["organization", "agent-qa-review"]
        elif path == "branches":
            return [{"name": "work/old", "commit": {"sha": HEAD}, "protected": False}]
        elif path == "pulls?state=all": return []
        elif path == "issues?state=open": return self.issues
        elif path == "issues/1/comments": return self.comments
        else: raise AssertionError((path, key))
        return [{"name": n, "status": "completed", "conclusion": "failure" if n == self.bad_job else "success"} for n in names]

    def file(self, path, ref): return deepcopy(self.states[ref])
    def ref(self, name): return "e" * 40 if name == "control/engineering" else HEAD
    def graphql(self, query, variables=None):
        return {"repository": {"pullRequest": {"reviewThreads": {"nodes": [{"isResolved": not self.open_thread}], "pageInfo": {"hasNextPage": False}}}}}


class LeaseScenarios(unittest.TestCase):
    def test_a_b_one_worker_serializes_bug_and_feature_roles(self):
        original = held("rust")
        for role in ["master", "qa", "security", "web"]:
            with self.assertRaises(ValueError):
                lease.acquire(original, AT, "second", "other-worker", role, 2, "work/other", HEAD)
        checkpoint = lease.checkpoint(original, AT + timedelta(minutes=1), "original-owner", "Commit preserved remotely", "QA next")
        released = lease.release(checkpoint, AT + timedelta(minutes=2), "original-owner")
        next_role = lease.acquire(released, AT + timedelta(minutes=3), "qa-owner", "one-worker", "qa", 1, "work/original", HEAD, 3)
        self.assertEqual(next_role["lease"]["role"], "qa")
        self.assertEqual(original["lease"]["role"], "rust")

    def test_c_serious_security_discovery_preempts_lower_priority_ready_work(self):
        action = control.select(state(), [issue(1), issue(2, origin="security", priority="p0")], AT)
        self.assertEqual(action["issue"], 2)

    def test_d_e_useful_ux_and_maintenance_share_one_backlog(self):
        for origin in ["ux", "maintenance"]:
            action = control.select(state(), [issue(2, origin=origin), issue(1, origin="user")], AT)
            self.assertEqual(action["issue"], 1)

    def test_g_h_registered_and_temporary_expertise_cannot_bypass_busy_lease(self):
        for role in ["database", "temporary-parser"]:
            with self.assertRaises(ValueError):
                lease.acquire(held(), AT, "second", "other-worker", role, 2, "work/other", HEAD)

    def test_k_l_capacity_loss_keeps_progress_and_requires_complete_recovery(self):
        s = lease.checkpoint(held(), AT + timedelta(minutes=1), "original-owner", "Remote implementation checkpoint", "Inspect pending CI", HEAD, 3)
        expired = AT + timedelta(hours=1)
        for operation in [lambda: lease.release(s, expired, "original-owner"), lambda: lease.checkpoint(s, expired, "original-owner", "x", "y")]:
            with self.assertRaises(ValueError): operation()
        incomplete = {"issue": True, "branch": True, "pr": True, "ci": True, "commit_preserved": False}
        with self.assertRaises(ValueError): lease.recover(s, expired, incomplete)
        recovered = lease.recover(s, expired, dict(incomplete, commit_preserved=True))
        self.assertIsNone(recovered["lease"])
        self.assertEqual(recovered["checkpoint"]["commit"], HEAD)
        self.assertNotIn("failed", json.dumps(recovered))

    def test_n_o_recovery_first_and_idle_without_busywork(self):
        self.assertEqual(control.select(held(), [], AT)["action"], "busy")
        self.assertEqual(control.select(held(), [], AT + timedelta(hours=1))["action"], "recover")
        self.assertEqual(control.select(state(), [issue(status="review")], AT)["action"], "recover")
        self.assertEqual(control.select(state(), [], AT)["action"], "idle")
        self.assertEqual(control.select(state(), [issue(status="blocked")], AT)["action"], "idle")

    def test_p_unchanged_attempts_release_lease_and_return_to_triage(self):
        s = held()
        for _ in range(3): s = lease.attempt(s, AT, "original-owner", "original approach", HEAD)
        self.assertIsNone(s["lease"])
        self.assertEqual(control.select(s, [issue(status="blocked")], AT)["action"], "triage")
        s = lease.acquire(s, AT, "triage-owner", "one-worker", "triage", 1, "work/original", HEAD)
        s = lease.attempt(s, AT, "triage-owner", "changed approach after evidence", BASE)
        self.assertEqual(s["attempts"]["1"]["unchanged"], 1)

    def test_q_fresh_session_reconstructs_json_without_chat_context(self):
        saved = json.loads(json.dumps(lease.checkpoint(held(), AT, "original-owner", "Pushed original work", "Review CI", HEAD, 3)))
        self.assertEqual(lease.validate(saved)["checkpoint"]["next_action"], "Review CI")
        self.assertEqual(control.select(saved, [], AT)["lease"]["commit"], HEAD)

    def test_recovery_before_first_checkpoint_identifies_current_not_preceding_work(self):
        previous = lease.checkpoint(held(), AT, "original-owner", "Previous useful result", "Next original work")
        previous = lease.release(previous, AT, "original-owner")
        current = lease.acquire(previous, AT + timedelta(minutes=1), "next-owner", "one-worker", "rust", 2, "work/next", BASE, 4)
        evidence = {"issue": True, "branch": True, "pr": True, "ci": True, "commit_preserved": True}
        recovered = lease.recover(current, AT + timedelta(hours=1), evidence)
        self.assertIsNone(recovered["lease"])
        for key in ["issue", "role", "branch", "commit", "pr"]:
            self.assertEqual(recovered["checkpoint"][key], current["lease"][key])
        self.assertEqual(recovered["checkpoint"]["previous_checkpoint"]["summary"], "Previous useful result")
        fresh = lease.recover(held(), AT + timedelta(hours=1), evidence)
        self.assertEqual(fresh["checkpoint"]["issue"], 1)
        self.assertEqual(fresh["checkpoint"]["commit"], HEAD)

    def test_circuit_breaker_before_first_checkpoint_preserves_affected_work(self):
        current = held()
        for _ in range(3): current = lease.attempt(current, AT, "original-owner", "same original strategy", HEAD)
        self.assertIsNone(current["lease"])
        self.assertEqual(current["checkpoint"]["issue"], 1)
        self.assertEqual(current["checkpoint"]["branch"], "work/original")
        self.assertEqual(current["checkpoint"]["circuit_breaker"]["unchanged"], 3)

    def test_checkpoint_preserves_evidence_and_bounded_prior_work_handoff(self):
        original = held("triage")
        original["checkpoint"] = {
            "at": lease.stamp(AT), "issue": 2, "role": "rust",
            "branch": "work/interrupted", "commit": BASE, "pr": 4,
            "summary": "Useful interrupted source", "next_action": "Inspect PR4",
            "source": {"commit": BASE, "pr": 4},
            "publication": {"commit": HEAD, "state": "published", "release_id": 42},
            "previous_checkpoint": {"summary": "Older bounded handoff"},
        }
        result = lease.checkpoint(original, AT + timedelta(minutes=1), "original-owner",
            "Native CI recovery", "Resume preserved work")
        for key in ["issue", "role", "branch", "commit", "pr"]:
            self.assertEqual(result["checkpoint"][key], result["lease"][key])
        self.assertEqual(result["checkpoint"]["source"], original["checkpoint"]["source"])
        self.assertEqual(result["checkpoint"]["publication"], original["checkpoint"]["publication"])
        prior = result["checkpoint"]["previous_checkpoint"]
        self.assertEqual((prior["issue"], prior["commit"], prior["pr"]), (2, BASE, 4))
        self.assertEqual(prior["next_action"], "Inspect PR4")
        self.assertNotIn("previous_checkpoint", prior)
        result["checkpoint"]["source"]["commit"] = HEAD
        self.assertEqual(original["checkpoint"]["source"]["commit"], BASE)
        repeated = lease.checkpoint(result, AT + timedelta(minutes=2), "original-owner",
            "Same recovery owner", "Resume preserved work")
        self.assertEqual(repeated["checkpoint"]["previous_checkpoint"], prior)
        self.assertEqual(repeated["lease"]["heartbeat_at"], lease.stamp(AT + timedelta(minutes=2)))

    def test_malformed_state_and_foreign_owner_fail_closed(self):
        for s in [dict(state(), max_active_agents=2), dict(state(), permanent_lock=True), dict(state(), generation=True)]:
            with self.assertRaises(ValueError): lease.validate(s)
        with self.assertRaises(ValueError): lease.release(held(), AT, "foreign-owner")


class ReviewScenarios(unittest.TestCase):
    def test_j_same_identity_native_comment_with_separate_qa_and_security_leases(self):
        proof = qa.evaluate(Native(), 3)
        self.assertEqual([r["role"] for r in proof["reviews"]], ["qa", "security"])
        self.assertEqual(len(proof["ci"]), 2)

    def test_i_rejection_stays_blocked_until_actual_current_correction_review(self):
        native = Native()
        native.reviews.append(review(verdict="rejected", number=12))
        with self.assertRaises(ValueError): qa.evaluate(native, 3)
        native.reviews.append(review(number=13))
        self.assertEqual(qa.evaluate(native, 3)["head"], HEAD)

    def test_m_red_ci_never_passes_review_or_delivery(self):
        for job in ["validate", "GNU", "musl", "package", "organization"]:
            native = Native(); native.bad_job = job
            with self.assertRaises(ValueError): qa.evaluate(native, 3)
        native = Native(); native.bad_job = "agent-qa-review"
        with self.assertRaises(ValueError): qa.evaluate(native, 3, include_gate=True)

    def test_new_head_or_base_invalidates_earlier_review(self):
        for side in ["head", "base"]:
            native = Native(); native.current_pr[side]["sha"] = "f" * 40
            with self.assertRaises(ValueError): qa.evaluate(native, 3)

    def test_dismissal_unresolved_threads_and_missing_diff_coverage_block(self):
        native = Native(); native.reviews[0]["state"] = "DISMISSED"
        with self.assertRaises(ValueError): qa.evaluate(native, 3)
        native = Native(); native.open_thread = True
        with self.assertRaises(ValueError): qa.evaluate(native, 3)
        native = Native(); native.files.append({"filename": "src/unreviewed.rs"})
        with self.assertRaises(ValueError): qa.evaluate(native, 3)

    def test_missing_security_review_or_fake_lease_role_cannot_pass(self):
        native = Native(); native.reviews.pop()
        with self.assertRaises(ValueError): qa.evaluate(native, 3)
        native = Native(); native.states[QA_CHECKPOINT]["lease"]["role"] = "rust"
        with self.assertRaises(ValueError): qa.evaluate(native, 3)
        native = Native(); native.ancestry = "diverged"
        with self.assertRaises(ValueError): qa.evaluate(native, 3)

    def test_untrusted_author_and_expired_submission_cannot_certify_qa(self):
        native = Native(); native.reviews[0]["user"]["login"] = "hostile"
        with self.assertRaises(ValueError): qa.evaluate(native, 3)
        native = Native(); native.reviews[0]["submitted_at"] = lease.stamp(AT + timedelta(hours=1))
        with self.assertRaises(ValueError): qa.evaluate(native, 3)

    def test_fork_draft_or_unlinked_pr_is_not_automatically_merged(self):
        for field in ["fork", "draft", "issue"]:
            native = Native()
            if field == "fork": native.current_pr["head"]["repo"]["full_name"] = "hostile/fork"
            elif field == "draft": native.current_pr["draft"] = True
            else: native.current_pr["body"] = "Unlinked change"
            with self.assertRaises(ValueError): qa.evaluate(native, 3)


class HygieneScenarios(unittest.TestCase):
    def test_f_never_delete_unique_or_active_work_even_when_old(self):
        for comparison, prs, issues, held_state, name in [
            ("diverged", [], [], None, "work/old"), ("ahead", [pr()], [], None, "work/original"),
            ("ahead", [], [dict(issue(), body="Keep work/old")], None, "work/old"),
            ("ahead", [], [], held()["lease"], "work/original"), ("ahead", [], [], None, "control/engineering"),
            ("ahead", [], [], None, "trunk")]:
            self.assertFalse(delivery.branch_decision(name, HEAD, CFG, comparison, prs, issues, held_state)[0])

    def test_r_exact_merged_head_is_preserved_but_new_unique_head_is_not_deleted(self):
        merged = dict(pr(), merged_at=lease.stamp(AT))
        self.assertTrue(delivery.branch_decision("work/original", HEAD, CFG, "diverged", [], [], None, [merged])[0])
        self.assertFalse(delivery.branch_decision("work/original", BASE, CFG, "diverged", [], [], None, [merged])[0])

    def test_open_issue_comment_protects_branch_before_any_delete(self):
        native = Native(); native.comments = [{"body": "Preserve work/old for resumed work"}]
        result = delivery.cleanup_branch(native, "work/old", HEAD, None)
        self.assertIn("preserved", result["result"])
        self.assertFalse(native.deleted)

    def test_inaccessible_project_does_not_fake_configuration_or_call_api(self):
        class Disabled:
            cfg = {"project": None}
            def rest(self, *args): raise AssertionError("Disabled Project must not call API")
        self.assertFalse(project.sync(Disabled(), 1)["configured"])

    def test_redirect_never_forwards_a_credential_to_another_host(self):
        with self.assertRaises(APIError):
            NoRedirect().redirect_request(None, None, 302, "redirect", {}, "https://hostile.invalid/")


class TransitionScenarios(unittest.TestCase):
    def test_branch_sweep_is_read_only_even_with_a_mergeable_candidate(self):
        class Audit(Native):
            def rest(self, method, path, value=None):
                if method != "GET": raise AssertionError("Read-only audit attempted a mutation")
                return super().rest(method, path, value)
            def pages(self, path, key=None):
                if path == "pulls?state=all": return [pr()]
                return super().pages(path, key)
        native = Audit()
        with patch.object(delivery, "read_state", return_value=(BASE, state())), patch.object(lease, "now", return_value=AT), patch.object(qa, "evaluate", side_effect=AssertionError("Sweep must not enter delivery gates")):
            result = delivery.run(native, sweep=True)
        self.assertEqual(result["action"], "quality-audit")
        self.assertFalse(native.deleted)

    def test_branch_sweep_leaves_expired_lease_for_explicit_recovery(self):
        class NoDomain:
            def pages(self, *args): raise AssertionError("Expired sweep must stop before domain audit")
        with patch.object(delivery, "read_state", return_value=(BASE, held())), patch.object(lease, "now", return_value=AT + timedelta(hours=1)), patch.object(delivery, "save", side_effect=AssertionError("Sweep must not mutate control")):
            result = delivery.run(NoDomain(), sweep=True)
        self.assertEqual(result["action"], "recovery-needed")
        self.assertEqual(result["issue"], 1)

    def test_native_handoff_contains_exact_durable_checkpoint_not_commands(self):
        class Comments:
            def __init__(self): self.posts = []
            def rest(self, method, path, body): self.posts.append((method, path, body))
        native = Comments()
        checkpoint = lease.checkpoint(held(), AT, "original-owner", "Original source preserved", "Independent QA next")["checkpoint"]
        control.handoff(native, held()["lease"], checkpoint, BASE)
        method, path, body = native.posts[0]
        self.assertEqual((method, path), ("POST", "issues/1/comments"))
        self.assertIn("<!-- mynou-transition:v1 -->", body["body"])
        self.assertIn(BASE, body["body"])
        self.assertIn(HEAD, body["body"])
        checkpoint["role"] = "rust"
        with self.assertRaises(ValueError): control.handoff(native, held()["lease"], checkpoint, BASE)
        self.assertEqual(len(native.posts), 1)

    def test_state_only_branch_has_native_default_branch_wake_and_trusted_code(self):
        workflow = (Path(__file__).resolve().parents[2] / ".github/workflows/engineering-delivery.yml").read_text()
        self.assertIn("issue_comment:\n    types: [created]", workflow)
        self.assertNotIn("branches: [control/engineering]", workflow)
        self.assertIn("<!-- mynou-transition:v1 -->", workflow)
        self.assertIn("github.event.comment.author_association", workflow)
        self.assertIn("ref: trunk", workflow)
        self.assertNotIn("pull_request_target", workflow)

    def test_deleted_squash_merged_branch_recovers_exact_preserved_native_head(self):
        class Merged(Native):
            def __init__(self):
                super().__init__()
                self.ancestry = "diverged"
                self.merged_pr = dict(pr(), state="closed", merged_at=lease.stamp(AT))
            def ref(self, name):
                if name == "work/original": raise APIError(404)
                return BASE
            def rest(self, method, path, value=None):
                if path == "git/commits/" + HEAD: return {"sha": HEAD}
                return super().rest(method, path, value)
            def pages(self, path, key=None):
                if path == "pulls?state=all": return [self.merged_pr]
                return super().pages(path, key)
        native = Merged()
        s = lease.checkpoint(held(), AT, "original-owner", "Source preserved", "Await merge", HEAD, 3)
        recovered = control.recover_native(native, s, AT + timedelta(hours=1))
        self.assertIsNone(recovered["lease"])
        self.assertEqual(recovered["checkpoint"]["commit"], HEAD)
        self.assertEqual(recovered["checkpoint"]["recovery"]["preservation"], "exact native merged PR head")
        native.merged_pr["head"]["sha"] = BASE
        with self.assertRaises(ValueError): control.recover_native(native, s, AT + timedelta(hours=1))
        native.merged_pr = dict(pr(), state="closed", merged_at=lease.stamp(AT))
        native.merged_pr["base"]["ref"] = "unrelated"
        with self.assertRaises(ValueError): control.recover_native(native, s, AT + timedelta(hours=1))

    def test_deletion_stops_if_ownership_changes_or_expires_after_audit(self):
        class Cleanup(Native):
            def rest(self, method, path, value=None):
                if method == "POST" and path == "issues/1/comments":
                    self.comments.append(value)
                    return {}
                return super().rest(method, path, value)
        for current, clock in [(held(identity="other-owner"), AT), (held(), AT + timedelta(hours=1))]:
            native = Cleanup()
            with patch.object(delivery, "read_state", side_effect=[(BASE, held()), (BASE, current)]), patch.object(lease, "now", side_effect=[AT, clock]):
                with self.assertRaises(ValueError): delivery.cleanup_branch(native, "work/old", HEAD, held()["lease"])
            self.assertFalse(native.deleted)
            self.assertEqual(len(native.comments), 1)

    def test_fresh_owned_cleanup_preserves_audit_and_deletes_only_exact_head(self):
        class Cleanup(Native):
            def rest(self, method, path, value=None):
                if method == "POST" and path == "issues/1/comments":
                    self.comments.append(value)
                    return {}
                return super().rest(method, path, value)
        native = Cleanup()
        with patch.object(delivery, "read_state", return_value=(BASE, held())), patch.object(lease, "now", return_value=AT):
            result = delivery.cleanup_branch(native, "work/old", HEAD, held()["lease"])
        self.assertEqual(result["result"], "deleted")
        self.assertIn(HEAD, native.comments[0]["body"])
        self.assertEqual(native.deleted, ["git/refs/heads/work/old"])


class CleanupRecoveryScenarios(unittest.TestCase):
    class Dependencies(Native):
        def __init__(self, parent_done=False):
            super().__init__()
            self.issues = [issue(status="done" if parent_done else "review"), issue(2, status="blocked")]
            if parent_done: self.issues[0]["state"] = "closed"
            self.current_state = state()
            self.control_head = BASE
            self.writes = []
        def rest(self, method, path, value=None):
            if path.startswith("issues/") and path.count("/") == 1:
                original = next(i for i in self.issues if i["number"] == int(path.split("/")[1]))
                if method == "PATCH":
                    self.writes.append((path, deepcopy(self.current_state["lease"])))
                    if "labels" in value: original["labels"] = [{"name": n} for n in value["labels"]]
                    if "state" in value: original["state"] = value["state"]
                return deepcopy(original)
            return super().rest(method, path, value)
        def pages(self, path, key=None):
            if path == "issues?state=open&labels=agent-work,status:blocked":
                return [deepcopy(i) for i in self.issues if i["state"] == "open" and "status:blocked" in control.labels(i)]
            if path == "issues/2/dependencies/blocked_by": return [deepcopy(self.issues[0])]
            return super().pages(path, key)
        def read(self, api): return self.control_head, deepcopy(self.current_state)
        def save(self, api, expected, value, message):
            if expected != self.control_head: raise AssertionError("Fixture CAS conflict")
            self.current_state = deepcopy(value)
            self.control_head = format(int(self.control_head, 16) + 1, "040x")
            return self.control_head

    def test_branch_cleanup_failure_after_closure_does_not_leave_dependents_blocked(self):
        native = self.Dependencies()
        native.current_state = held("triage")
        merged = dict(pr(), state="closed", merged_at=lease.stamp(AT))
        with patch.object(delivery, "read_state", side_effect=native.read), patch.object(lease, "now", return_value=AT), patch.object(delivery, "cleanup_branch", side_effect=APIError(503)):
            with self.assertRaises(APIError): delivery.cleanup_pr(native, merged)
        self.assertEqual(native.issues[0]["state"], "closed")
        self.assertIn("status:done", control.labels(native.issues[0]))
        self.assertIn("status:ready", control.labels(native.issues[1]))

    def test_restart_repairs_blocked_dependent_even_when_parent_already_done(self):
        native = self.Dependencies(parent_done=True)
        with patch.object(delivery, "read_state", side_effect=native.read), patch.object(delivery, "save", side_effect=native.save), patch.object(lease, "now", return_value=AT):
            result = delivery.run(native)
        self.assertEqual(result, {"action": "dependency-recovery", "issues": [2]})
        self.assertIsNone(native.current_state["lease"])
        self.assertEqual(native.current_state["checkpoint"]["issue"], 2)
        self.assertIn("status:ready", control.labels(native.issues[1]))
        self.assertTrue(all(owner is not None and owner["role"] == "quality" for _, owner in native.writes))

    def test_open_native_blocker_prevents_readiness_without_state_writes(self):
        native = self.Dependencies()
        self.assertIsNone(delivery.recover_dependencies(native, BASE, state()))
        self.assertFalse(native.writes)
        self.assertIn("status:blocked", control.labels(native.issues[1]))


class PublicationScenarios(unittest.TestCase):
    class Publication(CleanupRecoveryScenarios.Dependencies):
        def __init__(self):
            super().__init__(parent_done=True)
            self.default = HEAD
            self.current_pr = dict(pr(), state="closed", merged_at=lease.stamp(AT), merge_commit_sha=HEAD)
            self.runs = []
            self.dispatches = []
            self.dispatch_error = None
            self.advance_after_dispatch = False
            self.checkpoints = []
        def ref(self, name):
            return self.control_head if name == "control/engineering" else self.default
        def rest(self, method, path, value=None):
            if method == "POST" and path == "actions/workflows/ci.yml/dispatches":
                owner = self.current_state["lease"]
                if owner is None: raise AssertionError("Dispatch must own the global lease")
                intent = self.current_state["checkpoint"]["publication"]
                if intent["state"] != "dispatching": raise AssertionError("Dispatch intent must already be durable")
                self.dispatches.append((deepcopy(value), deepcopy(owner), deepcopy(intent)))
                if self.dispatch_error is not None: raise self.dispatch_error
                if self.advance_after_dispatch: self.default = BASE
                return None  # Real GitHub dispatch accepts with an empty 204.
            return super().rest(method, path, value)
        def pages(self, path, key=None):
            if path == "pulls?state=all": return [deepcopy(self.current_pr)]
            if path.startswith("actions/workflows/ci.yml/runs?head_sha="):
                if key != "workflow_runs": raise AssertionError("Read the native workflow run collection")
                return deepcopy(self.runs)
            return super().pages(path, key)
        def save(self, api, expected, value, message):
            self.checkpoints.append(deepcopy(value["checkpoint"]))
            return super().save(api, expected, value, message)

    def native_run(self, event="workflow_dispatch", status="completed", conclusion="success", **extra):
        return dict(id=40, head_sha=HEAD, head_branch="trunk", event=event, status=status, conclusion=conclusion, run_attempt=1, **extra)

    def execute(self, native, at=AT):
        proof = {"commit": HEAD, "state": "published", "version": "0.22.29", "at": lease.stamp(AT),
            "release_id": 42, "immutable": True, "run_id": 40, "run_attempt": 1}
        with patch.object(delivery, "read_state", side_effect=native.read), patch.object(delivery, "save", side_effect=native.save), patch.object(lease, "now", return_value=at), patch.object(delivery.publication_evidence, "verified", return_value=proof):
            return delivery.run(native)

    def test_bot_merge_with_no_push_run_dispatches_even_when_issue_is_done(self):
        native = self.Publication()
        result = self.execute(native)
        self.assertEqual(result["action"], "publication-recovery")
        self.assertEqual(result["publication"]["state"], "accepted")
        self.assertEqual(native.dispatches[0][0], {"ref": "trunk"})
        self.assertEqual(native.dispatches[0][1]["commit"], HEAD)
        self.assertEqual(native.dispatches[0][1]["pr"], 3)
        self.assertEqual(native.current_state["checkpoint"]["publication"]["commit"], HEAD)
        self.assertIsNone(native.current_state["lease"])
        self.assertIn("status:blocked", control.labels(native.issues[1]))
        self.assertFalse(native.deleted)

    def test_dispatch_visibility_grace_avoids_immediate_duplicates_after_restart(self):
        native = self.Publication()
        self.execute(native)
        result = self.execute(native, AT + timedelta(minutes=1))
        self.assertEqual(result["publication"]["state"], "awaiting-visibility")
        self.assertEqual(len(native.dispatches), 1)
        self.assertEqual(native.current_state["checkpoint"]["publication"]["at"], lease.stamp(AT))

    def test_visibility_grace_expires_and_natively_missing_run_is_recoverable(self):
        native = self.Publication()
        self.execute(native)
        self.execute(native, AT + timedelta(minutes=6))
        self.assertEqual(len(native.dispatches), 2)

    def test_interruption_before_response_retains_intent_and_delays_retry(self):
        native = self.Publication()
        native.current_state = held("triage")
        with patch.object(delivery, "read_state", side_effect=native.read), patch.object(delivery, "save", side_effect=native.save), patch.object(lease, "now", return_value=AT), patch.object(native, "rest", side_effect=KeyboardInterrupt):
            with self.assertRaises(KeyboardInterrupt): delivery.dispatch_default_ci(native, "original-owner", HEAD)
        self.assertEqual(native.current_state["checkpoint"]["publication"]["state"], "dispatching")
        native.current_state = lease.release(native.current_state, AT, "original-owner")
        result = self.execute(native, AT + timedelta(minutes=1))
        self.assertEqual(result["publication"]["state"], "awaiting-visibility")
        self.assertFalse(native.dispatches)

    def test_transport_failure_is_truthful_and_accepted_but_lost_response_deduplicates(self):
        native = self.Publication()
        native.dispatch_error = RuntimeError("Original synthetic lost response")
        with self.assertRaises(RuntimeError): self.execute(native)
        self.assertIsNone(native.current_state["lease"])
        self.assertEqual(native.current_state["checkpoint"]["publication"]["state"], "unknown")
        native.runs = [self.native_run(status="in_progress", conclusion=None)]
        native.dispatch_error = None
        result = self.execute(native, AT + timedelta(minutes=1))
        self.assertEqual(result["publication"]["run_id"], 40)
        self.assertEqual(result["publication"]["state"], "ci-pending")
        self.assertEqual(len(native.dispatches), 1)

    def test_rejected_dispatch_is_visible_and_does_not_complete_publication(self):
        native = self.Publication()
        native.dispatch_error = APIError(403)
        with self.assertRaises(APIError): self.execute(native)
        detail = native.current_state["checkpoint"]["publication"]
        self.assertEqual((detail["state"], detail["error"]), ("failed", "HTTP 403"))
        self.assertIsNone(native.current_state["lease"])
        self.assertIn("status:blocked", control.labels(native.issues[1]))
        native.dispatch_error = None
        self.execute(native, AT + timedelta(minutes=1))
        self.assertEqual(len(native.dispatches), 2)

    def test_existing_push_or_dispatch_success_prevents_duplicate_dispatch(self):
        for event in ["push", "workflow_dispatch"]:
            native = self.Publication()
            native.runs = [self.native_run(event=event)]
            result = self.execute(native)
            self.assertEqual(result["action"], "publication-recorded")
            self.assertEqual(self.execute(native)["action"], "dependency-recovery")
            self.assertFalse(native.dispatches)
            self.assertIn("status:ready", control.labels(native.issues[1]))

    def test_pending_native_run_preempts_other_delivery_and_has_no_duplicate(self):
        native = self.Publication()
        native.runs = [self.native_run(status="queued", conclusion=None)]
        with patch.object(qa, "evaluate", side_effect=AssertionError("Pending publication must precede new PR work")):
            result = self.execute(native)
            repeat = self.execute(native, AT + timedelta(minutes=1))
        self.assertEqual(result["publication"]["state"], "ci-pending")
        self.assertEqual(repeat["action"], "publication-pending")
        self.assertFalse(native.dispatches)
        self.assertIn("status:blocked", control.labels(native.issues[1]))

    def test_failed_native_ci_is_not_green_and_is_not_blindly_redispatched(self):
        for conclusion in ["failure", "cancelled", "timed_out", "skipped"]:
            native = self.Publication()
            native.runs = [self.native_run(conclusion=conclusion)]
            with self.assertRaises(ValueError): self.execute(native)
            self.assertEqual(native.current_state["checkpoint"]["publication"]["state"], "ci-failed")
            self.assertFalse(native.dispatches)
            self.assertIsNone(native.current_state["lease"])

    def test_success_observed_after_acceptance_is_checkpointed_without_republication(self):
        native = self.Publication()
        self.execute(native)
        native.runs = [self.native_run()]
        result = self.execute(native, AT + timedelta(minutes=1))
        self.assertEqual(result["publication"]["state"], "published")
        self.assertEqual(native.current_state["checkpoint"]["publication"]["run_id"], 40)
        self.assertEqual(len(native.dispatches), 1)

    def test_terminal_published_source_does_not_overwrite_active_work_on_repeated_wakes(self):
        for terminal in ["published"]:
            native = self.Publication()
            native.runs = [self.native_run()]
            native.current_state["checkpoint"] = {
                "issue": 2, "role": "rust", "branch": "work/interrupted", "commit": BASE, "pr": 4,
                "summary": "Useful interrupted implementation", "next_action": "Resume PR4",
                "source": {"commit": BASE, "pr": 4},
                "publication": {"commit": HEAD, "state": terminal, "run_id": 40, "release_id": 42},
            }
            original = deepcopy(native.current_state)
            for _ in range(2):
                result = delivery.recover_publication(native, native.control_head,
                    native.current_state, [deepcopy(native.current_pr)])
                self.assertIsNone(result)
            self.assertEqual(native.current_state, original)
            self.assertFalse(native.writes)
            self.assertFalse(native.dispatches)

    def test_published_record_cannot_hide_native_ci_failure(self):
        native = self.Publication()
        native.current_state["checkpoint"] = {"publication":
            {"commit": HEAD, "state": "published", "run_id": 40, "release_id": 42}}
        native.runs = [dict(self.native_run(conclusion="failure"), id=41)]
        with self.assertRaises(ValueError):
            self.execute(native)
        self.assertEqual(native.current_state["checkpoint"]["publication"]["state"], "ci-failed")
        self.assertEqual(native.current_state["checkpoint"]["publication"]["run_id"], 41)
        self.assertFalse(native.dispatches)
        self.assertIsNone(native.current_state["lease"])

    def test_required_ci_recovery_retains_interrupted_source_and_next_action(self):
        native = self.Publication()
        native.current_state["checkpoint"] = {
            "at": lease.stamp(AT), "issue": 2, "role": "rust",
            "branch": "work/interrupted", "commit": BASE, "pr": 4,
            "summary": "Useful interrupted implementation", "next_action": "Resume PR4",
            "source": {"commit": BASE, "pr": 4},
            "control_storage": {"state": "blocked", "commit": BASE},
        }
        result = self.execute(native)
        self.assertEqual(result["publication"]["state"], "accepted")
        checkpoint = native.current_state["checkpoint"]
        self.assertEqual(checkpoint["source"], {"commit": BASE, "pr": 4})
        self.assertEqual(checkpoint["control_storage"], {"state": "blocked", "commit": BASE})
        self.assertEqual(checkpoint["previous_checkpoint"]["next_action"], "Resume PR4")
        self.assertEqual(checkpoint["previous_checkpoint"]["pr"], 4)
        self.assertNotIn("previous_checkpoint", checkpoint["previous_checkpoint"])
        self.assertEqual(checkpoint["commit"], HEAD)
        self.assertEqual(len(native.dispatches), 1)
        self.assertIsNone(native.current_state["lease"])

    def test_wrong_source_branch_or_pr_event_cannot_satisfy_default_ci(self):
        for field, value in [("head_sha", BASE), ("head_branch", "work/original"), ("event", "pull_request")]:
            native = self.Publication()
            run = self.native_run()
            run[field] = value
            native.runs = [run]
            self.execute(native)
            self.assertEqual(len(native.dispatches), 1)

    def test_head_change_after_intent_or_during_dispatch_fails_closed(self):
        for before in [True, False]:
            native = self.Publication()
            native.current_state = held("triage")
            native.advance_after_dispatch = not before
            original_save = native.save
            def save_with_external_writer(api, expected, value, message):
                result = original_save(api, expected, value, message)
                if before and value["checkpoint"].get("publication", {}).get("state") == "dispatching": native.default = BASE
                return result
            with patch.object(delivery, "read_state", side_effect=native.read), patch.object(delivery, "save", side_effect=save_with_external_writer), patch.object(lease, "now", return_value=AT):
                with self.assertRaises(ValueError): delivery.dispatch_default_ci(native, "original-owner", HEAD)
            self.assertEqual(native.current_state["checkpoint"]["publication"]["state"], "head-changed")
            self.assertEqual(len(native.dispatches), 0 if before else 1)

    def test_changed_default_cannot_recover_old_commit_by_dispatching_new_source(self):
        native = self.Publication()
        self.execute(native)
        native.default = BASE
        with self.assertRaises(ValueError): self.execute(native, AT + timedelta(minutes=6))
        self.assertEqual(len(native.dispatches), 1)
        self.assertIsNone(native.current_state["lease"])

    def test_live_lease_exits_before_native_domain_reads_or_dispatch(self):
        native = self.Publication()
        native.current_state = held("rust")
        with patch.object(native, "pages", side_effect=AssertionError("Busy wake must not investigate")), patch.object(native, "ref", side_effect=AssertionError("Busy wake must not read source")):
            self.assertEqual(self.execute(native)["action"], "busy")
        self.assertFalse(native.dispatches)

    def test_foreign_expired_or_wrong_source_owner_never_dispatches(self):
        for role, identity, at, commit in [("qa", "original-owner", AT, HEAD), ("triage", "foreign-owner", AT, HEAD),
                ("triage", "original-owner", AT + timedelta(hours=1), HEAD), ("triage", "original-owner", AT, BASE)]:
            native = self.Publication()
            native.current_state = held(role)
            with patch.object(delivery, "read_state", side_effect=native.read), patch.object(lease, "now", return_value=at):
                with self.assertRaises(ValueError): delivery.dispatch_default_ci(native, identity, commit)
            self.assertFalse(native.dispatches)

    def test_dispatch_permission_is_only_in_trusted_default_code_workflow(self):
        root = Path(__file__).resolve().parents[2]
        privileged = (root / ".github/workflows/engineering-delivery.yml").read_text()
        checks = (root / ".github/workflows/engineering-checks.yml").read_text()
        self.assertIn("actions: write", privileged)
        self.assertIn("ref: trunk", privileged)
        self.assertIn("persist-credentials: false", privileged)
        self.assertNotIn("pull_request_target", privileged)
        self.assertNotIn("actions: write", checks)

    def test_confirmed_merge_releases_default_source_lease_at_publication_wait(self):
        merged_commit = "e" * 40
        class Merger(self.Publication):
            def __init__(self):
                super().__init__()
                self.current_pr = pr()
                self.default = BASE
                self.issues[0] = issue(status="review")
                self.merges = []
            def rest(self, method, path, value=None):
                if method == "PUT" and path == "pulls/3/merge":
                    self.merges.append((deepcopy(value), deepcopy(self.current_state["lease"])))
                    self.default = merged_commit
                    self.current_pr.update(state="closed", merged_at=lease.stamp(AT), merge_commit_sha=merged_commit)
                    return {"merged": True, "sha": merged_commit}
                return super().rest(method, path, value)
        native = Merger()
        proof = {"issues": [1], "head": HEAD, "base": BASE}
        with patch.object(qa, "evaluate", return_value=proof) as gates:
            result = self.execute(native)
        self.assertEqual(gates.call_count, 1)  # Complete fresh gates under ownership.
        self.assertEqual(result["action"], "delivered")
        self.assertEqual(native.merges[0][0], {"sha": HEAD, "merge_method": "merge"})
        self.assertEqual(native.dispatches[0][1]["commit"], merged_commit)
        self.assertEqual(native.dispatches[0][1]["branch"], "trunk")
        self.assertEqual(native.merges[0][1]["commit"], BASE)
        self.assertEqual(native.merges[0][1]["branch"], "trunk")
        self.assertEqual(native.dispatches[0][2]["commit"], merged_commit)
        self.assertEqual(native.current_state["checkpoint"]["publication"]["state"], "accepted")
        self.assertIsNone(native.current_state["lease"])
        self.assertNotIn("status:done", control.labels(native.issues[0]))
        self.assertFalse(native.deleted)
        self.assertEqual(result["results"][0]["cleanup"], "pending exact-default publication")

    def test_unmanaged_or_wrong_repository_merge_never_dispatches(self):
        for kind in ["unmanaged", "fork", "base"]:
            native = self.Publication()
            if kind == "unmanaged": native.issues[0]["labels"] = []
            elif kind == "fork": native.current_pr["head"]["repo"]["full_name"] = "untrusted/fork"
            else: native.current_pr["base"]["ref"] = "unrelated"
            self.execute(native)
            self.assertFalse(native.dispatches)


class AtomicCAS(unittest.TestCase):
    def test_competing_writer_cannot_overwrite_a_new_lease_without_force(self):
        class Race(GitHub):
            def __init__(self): self.head = HEAD; self.updates = []
            def ref(self, branch): return self.head
            def rest(self, method, path, value=None):
                if path == "git/trees": return {"sha": BASE}
                if path == "git/commits":
                    self.assert_parent = value["parents"]
                    self.head = "e" * 40  # Another worker wins after the observed read.
                    return {"sha": "f" * 40}
                self.updates.append(value)
                if value["force"] is False: raise APIError(422)
                raise AssertionError("Force is forbidden")
        race = Race()
        with self.assertRaises(RuntimeError): race.cas_file("control/engineering", HEAD, "state.json", state(), "original lease")
        self.assertEqual(race.assert_parent, [HEAD])
        self.assertEqual(race.head, "e" * 40)
        self.assertEqual(race.updates, [{"sha": "f" * 40, "force": False}])


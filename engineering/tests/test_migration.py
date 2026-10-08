"""Original cutover race, interruption and retirement regressions; CI only."""
from copy import deepcopy
from datetime import datetime, timedelta, timezone
import json
from pathlib import Path
import sys
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import commands
import control
from github import APIError, GitHub
import lease
import migration
import qa
from test_organization import Native as ReviewNative

AT = datetime(2026, 10, 8, 13, tzinfo=timezone.utc)
HEAD, SOURCE, PROMPT = "a" * 40, "b" * 40, "f" * 40


def initial():
    state = {"schema": 1, "max_active_agents": 1, "generation": 0,
        "lease": None, "checkpoint": {}, "attempts": {"19": {"unchanged": 1}}}
    state = lease.acquire(state, AT, "owner", "one-worker", "master", 19, "trunk", SOURCE, 24)
    return lease.checkpoint(state, AT, "owner", "Preserved remote work", "Migration")


class Native(GitHub):
    def __init__(self):
        self.cfg = {"schema": 1, "repository": "alecerf/mynou", "default_branch": "trunk",
            "control_branch": "control/engineering", "control_ref": migration.LEGACY,
            "state_path": "state.json", "max_active_agents": 1,
            "control_migration": {"mode": "fenced-notes-v1", "legacy_ref": migration.LEGACY,
                "target_ref": migration.TARGET, "issue": 19, "pr": 24},
            "control_commands": {"issue": 30, "trusted_actors": {"alecerf": 200}}}
        self.refs = {migration.LEGACY: HEAD, "refs/heads/trunk": SOURCE}
        self.states = {HEAD: initial()}
        self.commits = {HEAD: {"sha": HEAD, "parents": []}, SOURCE: {"sha": SOURCE, "parents": []}}
        self.trees, self.comments, self.calls = {}, {}, []
        self.counter = 1000
        self.race = False
        self.create_race = False
        self.stop_after_delete = False
        self.work_issue = {"number": 19, "state": "open", "labels": [{"name": "agent-work"}]}
        self.open_issues = []
        self.comments[202] = {"user": {"type": "User", "login": "alecerf", "id": 200},
            "issue_url": "https://api.github.com/repos/alecerf/mynou/issues/19",
            "created_at": lease.stamp(AT), "updated_at": lease.stamp(AT),
            "body": migration.TASK_MARKER + "\n" + commands.FENCE + "json\n" + json.dumps({
                "schema": 1, "task_id": "synthetic-existing-task", "policy_commit": SOURCE,
                "prompt_sha": PROMPT, "enabled": True, "cadence": "hourly", "response_digest": "0" * 64}) + "\n" + commands.FENCE}

    def ancestor(self, old, new):
        seen, pending = set(), [new]
        while pending:
            current = pending.pop()
            if current == old:
                return True
            if current not in seen:
                seen.add(current)
                pending.extend(p["sha"] for p in self.commits.get(current, {}).get("parents", []))
        return False

    def file(self, path, ref):
        if path != "state.json":
            raise AssertionError(path)
        return deepcopy(self.states[ref])

    def pages(self, path, key=None):
        if path == "issues?state=open":
            return deepcopy(self.open_issues)
        if path == "pulls?state=open":
            return []
        if path == "pulls?state=all":
            return [{"number": 24, "state": "closed", "merged_at": lease.stamp(AT),
                "head": {"ref": "work/control-ref-adapter", "sha": HEAD},
                "base": {"ref": "trunk"}}]
        if path.startswith("actions/runs?head_sha="):
            return [{"id": 1, "status": "completed", "conclusion": "success"}]
        raise AssertionError(path)

    def rest(self, method, path, value=None):
        self.calls.append((method, path, deepcopy(value)))
        if method == "GET" and path.startswith("git/ref/"):
            location = "refs/" + path.removeprefix("git/ref/")
            if location not in self.refs:
                raise APIError(404)
            return {"ref": location, "object": {"type": "commit", "sha": self.refs[location]}}
        if method == "GET" and path.startswith("git/commits/"):
            return deepcopy(self.commits[path.rsplit("/", 1)[1]])
        if method == "GET" and path.startswith("compare/"):
            old, new = path.removeprefix("compare/").split("...")
            return {"status": "identical" if old == new else "ahead" if self.ancestor(old, new) else "diverged"}
        if method == "GET" and path == "issues/19":
            return deepcopy(self.work_issue)
        if method == "PATCH" and path == "issues/19":
            normalized = deepcopy(value)
            if "labels" in normalized:
                normalized["labels"] = [{"name": label} for label in normalized["labels"]]
            self.work_issue.update(normalized)
            return deepcopy(self.work_issue)
        if method == "GET" and path.startswith("issues/comments/"):
            return deepcopy(self.comments[int(path.rsplit("/", 1)[1])])
        if method == "GET" and path.startswith("contents/engineering/worker-prompt.md?ref="):
            return {"sha": PROMPT}
        if method == "POST" and path == "git/trees":
            self.counter += 1
            result = f"{self.counter:040x}"
            self.trees[result] = json.loads(value["tree"][0]["content"])
            return {"sha": result}
        if method == "POST" and path == "git/commits":
            self.counter += 1
            result = f"{self.counter:040x}"
            self.commits[result] = {"sha": result, "message": value["message"],
                "parents": [{"sha": p} for p in value["parents"]]}
            self.states[result] = deepcopy(self.trees[value["tree"]])
            return {"sha": result}
        if method == "PATCH" and path.startswith("git/refs/"):
            location = "refs/" + path.removeprefix("git/refs/")
            if self.race:
                self.race = False
                rival = "e" * 40
                self.commits[rival] = {"sha": rival, "parents": [{"sha": self.refs[location]}]}
                self.states[rival] = deepcopy(self.states[self.refs[location]])
                self.refs[location] = rival
            if value["force"] or not self.ancestor(self.refs[location], value["sha"]):
                raise APIError(422)
            self.refs[location] = value["sha"]
            return {"ref": location, "object": {"type": "commit", "sha": value["sha"]}}
        if method == "POST" and path == "git/refs":
            if self.create_race:
                self.refs[migration.TARGET] = "e" * 40
            if value["ref"] in self.refs:
                raise APIError(422)
            self.refs[value["ref"]] = value["sha"]
            return {"ref": value["ref"], "object": {"type": "commit", "sha": value["sha"]}}
        if method == "DELETE" and path == "git/refs/heads/control/engineering":
            del self.refs[migration.LEGACY]
            if self.stop_after_delete:
                self.stop_after_delete = False
                raise RuntimeError("Synthetic receipt loss after deletion")
            return None
        raise AssertionError((method, path))


class CutoverScenarios(unittest.TestCase):
    def setUp(self):
        self.clock = patch.object(lease, "now", return_value=AT)
        self.clock.start()
        self.ready = patch.object(migration, "source_ready", return_value=SOURCE)
        self.source = self.ready.start()
        self.addCleanup(self.clock.stop)
        self.addCleanup(self.ready.stop)

    def fenced(self):
        api = Native()
        result = migration.fence(api, HEAD, "one-worker", "owner", 202)
        return api, result["control_sha"]

    def activated(self):
        api, fence = self.fenced()
        result = migration.activate(api, fence, "one-worker", "owner")
        return api, fence, result["control_sha"]

    def retirement_ready(self):
        api, fence, head = self.activated()
        state = lease.release(api.file("state.json", head), AT, "owner")
        state = lease.acquire(state, AT, "quality-owner", "one-worker", "quality", 19, "trunk", SOURCE, 24)
        head = api.cas_file(migration.TARGET, head, "state.json", state, "Serial Quality lease")
        args = {"worker": "one-worker", "lease": "quality-owner", "summary": "Real notes checkpoint",
            "next_action": "Retire only after Task proof", "commit": SOURCE, "pr": 24}
        envelope = {"schema": 1, "command": "checkpoint", "expected_sha": head, "args": args}
        api.comments[201] = self.comment(30, commands.MARKER, envelope)
        state = lease.checkpoint(state, AT, "quality-owner", args["summary"], args["next_action"])
        state["checkpoint"]["control_migration"]["transport_comment_id"] = 201
        proof = api.cas_file(migration.TARGET, head, "state.json", state, "Control comment 201: checkpoint")
        api.comments[202] = self.comment(19, migration.TASK_MARKER, {"schema": 1,
            "task_id": "synthetic-existing-task", "policy_commit": SOURCE, "prompt_sha": PROMPT,
            "enabled": True, "cadence": "hourly", "response_digest": "0" * 64})
        return api, fence, proof

    def comment(self, issue, marker, value):
        return {"user": {"type": "User", "login": "alecerf", "id": 200},
            "issue_url": f"https://api.github.com/repos/alecerf/mynou/issues/{issue}",
            "created_at": lease.stamp(AT), "updated_at": lease.stamp(AT),
            "body": marker + "\n" + commands.FENCE + "json\n" + json.dumps(value) + "\n" + commands.FENCE}

    def test_legacy_is_only_authority_before_fence(self):
        api = Native()
        location, head, state = migration.resolve(api)
        self.assertEqual((location, head), (migration.LEGACY, HEAD))
        self.assertTrue(lease.valid(state, AT))
        self.assertNotIn(migration.TARGET, api.refs)

    def test_fence_blocks_old_workers_and_pauses_new_domain_work(self):
        api, fence = self.fenced()
        with self.assertRaises(ValueError):
            lease.validate(api.file("state.json", fence))
        with self.assertRaises(migration.Pending) as result:
            control.read_state(api)
        self.assertTrue(lease.valid(result.exception.state, AT))
        self.assertEqual(result.exception.state["attempts"], initial()["attempts"])
        self.assertNotIn(migration.TARGET, api.refs)

    def test_activation_preserves_all_history_and_original_single_parent(self):
        api, fence, head = self.activated()
        self.assertEqual(api.commits[head]["parents"], [{"sha": fence}])
        self.assertEqual(api.commits[fence]["parents"], [{"sha": HEAD}])
        self.assertEqual(migration.resolve(api)[:2], (migration.TARGET, head))
        self.assertTrue(api.ancestor(HEAD, head))
        self.assertEqual(api.file("state.json", head)["attempts"], initial()["attempts"])

    def test_wrong_worker_or_role_never_fences(self):
        for worker, role in [("other", "master"), ("one-worker", "quality")]:
            api = Native()
            api.states[HEAD]["lease"]["role"] = role
            with self.subTest(worker=worker, role=role), self.assertRaises(ValueError):
                migration.fence(api, HEAD, worker, "owner", 202)
            self.assertFalse(any(c[0] == "POST" for c in api.calls))

    def test_competing_fence_sibling_wins_without_force_or_retry(self):
        api = Native()
        api.race = True
        with self.assertRaises(RuntimeError):
            migration.fence(api, HEAD, "one-worker", "owner", 202)
        self.assertEqual(api.refs[migration.LEGACY], "e" * 40)
        writes = [c for c in api.calls if c[0] == "PATCH"]
        self.assertEqual(len(writes), 1)
        self.assertFalse(writes[0][2]["force"])
        self.assertNotIn(migration.TARGET, api.refs)

    def test_existing_or_racing_candidate_is_preserved_not_overwritten(self):
        api = Native()
        api.refs[migration.TARGET] = "e" * 40
        with self.assertRaises(ValueError):
            migration.fence(api, HEAD, "one-worker", "owner", 202)
        self.assertEqual(api.refs[migration.LEGACY], HEAD)
        api, fence = self.fenced()
        api.create_race = True
        with self.assertRaises(APIError):
            migration.activate(api, fence, "one-worker", "owner")
        self.assertEqual(api.refs[migration.TARGET], "e" * 40)
        self.assertEqual(api.refs[migration.LEGACY], fence)

    def test_valid_nested_lease_cannot_be_reclaimed_and_reads_no_issue(self):
        api, fence = self.fenced()
        api.calls.clear()
        with self.assertRaises(ValueError):
            migration.recover_fence(api, fence, "second-worker", 301)
        self.assertFalse(any("issues/" in c[1] for c in api.calls))

    def test_expired_fence_recovers_native_work_then_reacquires_one_master(self):
        api, fence = self.fenced()
        with patch.object(lease, "now", return_value=AT + timedelta(hours=1)):
            result = migration.recover_fence(api, fence, "recovered-worker", 301)
            recovered = api.file("state.json", result["control_sha"])["state"]
            self.assertEqual(recovered["lease"]["role"], "master")
            self.assertEqual(recovered["lease"]["worker"], "recovered-worker")
            self.assertTrue(all(recovered["checkpoint"]["recovery"][k]
                for k in ["issue", "branch", "pr", "ci", "commit_preserved"]))
            self.assertEqual(api.commits[result["control_sha"]]["parents"], [{"sha": fence}])
            active = migration.activate(api, result["control_sha"], "recovered-worker", result["lease_id"])
            self.assertEqual(migration.resolve(api)[1], active["control_sha"])

    def test_bad_fence_parent_or_legacy_rollback_fails_closed(self):
        api, fence, _ = self.activated()
        api.commits[fence]["parents"] = []
        with self.assertRaises(ValueError):
            migration.resolve(api)
        api, _, _ = self.activated()
        api.refs[migration.LEGACY] = HEAD
        with self.assertRaises(ValueError):
            migration.resolve(api)

    def test_missing_both_refs_never_initializes(self):
        api = Native()
        del api.refs[migration.LEGACY]
        with self.assertRaises(ValueError):
            migration.resolve(api)
        self.assertFalse(any(c[0] == "POST" for c in api.calls))

    def test_real_notes_checkpoint_and_task_attestation_precede_retirement(self):
        api, fence, proof = self.retirement_ready()
        migration.transport_proof(api, proof, proof)
        result = migration.retire(api, proof, "one-worker", "quality-owner", proof, 202)
        self.assertNotIn(migration.LEGACY, api.refs)
        self.assertEqual(migration.resolve(api)[:2], (migration.TARGET, result["control_sha"]))
        self.assertTrue(api.ancestor(fence, result["control_sha"]))
        self.assertEqual(api.file("state.json", result["control_sha"])["checkpoint"]["control_migration"]["phase"], "retired")

    def test_task_attestation_wrong_source_edit_actor_or_enabled_blocks_deletion(self):
        for kind in ["source", "edit", "actor", "disabled"]:
            api, _, proof = self.retirement_ready()
            native = api.comments[202]
            if kind == "edit":
                native["updated_at"] = lease.stamp(AT + timedelta(seconds=1))
            elif kind == "actor":
                native["user"]["id"] = 201
            else:
                value = {"schema": 1, "task_id": "synthetic-existing-task", "policy_commit": HEAD if kind == "source" else SOURCE,
                    "prompt_sha": PROMPT, "enabled": kind != "disabled", "cadence": "hourly", "response_digest": "0" * 64}
                api.comments[202] = self.comment(19, migration.TASK_MARKER, value)
            with self.subTest(kind=kind), self.assertRaises(ValueError):
                migration.retire(api, proof, "one-worker", "quality-owner", proof, 202)
            self.assertIn(migration.LEGACY, api.refs)
            self.assertFalse(any(c[0] == "DELETE" for c in api.calls))

    def test_interruption_after_deletion_keeps_intent_and_can_finalize_without_second_delete(self):
        api, _, proof = self.retirement_ready()
        api.stop_after_delete = True
        with self.assertRaises(RuntimeError):
            migration.retire(api, proof, "one-worker", "quality-owner", proof, 202)
        location, head, preserved = migration.resolve(api)
        self.assertEqual(location, migration.TARGET)
        self.assertEqual(preserved["checkpoint"]["control_migration"]["retirement"], "prepared")
        result = migration.retire(api, head, "one-worker", "quality-owner", proof, 202)
        self.assertEqual(api.file("state.json", result["control_sha"])["checkpoint"]["control_migration"]["phase"], "retired")
        self.assertEqual(sum(c[0] == "DELETE" for c in api.calls), 1)

    def test_unreconciled_task_blocks_fence_before_any_mutation(self):
        api = Native()
        api.comments[202]["updated_at"] = lease.stamp(AT + timedelta(seconds=1))
        with self.assertRaises(ValueError):
            migration.fence(api, HEAD, "one-worker", "owner", 202)
        self.assertEqual(api.refs[migration.LEGACY], HEAD)
        self.assertFalse(any(c[0] == "POST" for c in api.calls))

    def test_other_open_issue_reference_blocks_retirement(self):
        api, _, proof = self.retirement_ready()
        api.open_issues = [{"number": 40, "title": "Investigate control/engineering", "body": ""}]
        with self.assertRaises(ValueError):
            migration.retire(api, proof, "one-worker", "quality-owner", proof, 202)
        self.assertIn(migration.LEGACY, api.refs)
        self.assertFalse(any(c[0] == "DELETE" for c in api.calls))

    def test_source_change_after_retirement_intent_blocks_deletion(self):
        api, _, proof = self.retirement_ready()
        original = api.cas_file
        def changed(location, expected, path, value, message):
            result = original(location, expected, path, value, message)
            if message == "Prepare verified legacy retirement":
                api.refs["refs/heads/trunk"] = "c" * 40
            return result
        api.cas_file = changed
        with self.assertRaises(ValueError):
            migration.retire(api, proof, "one-worker", "quality-owner", proof, 202)
        self.assertIn(migration.LEGACY, api.refs)
        self.assertFalse(any(c[0] == "DELETE" for c in api.calls))

    def test_old_merger_completion_is_reopened_under_one_owned_lease(self):
        import delivery
        api = Native()
        state = lease.release(api.states[HEAD], AT, "owner")
        api.states[HEAD] = state
        api.work_issue.update(state="closed", labels=[{"name": "agent-work"}, {"name": "status:done"}])
        result = delivery.recover_cutover_metadata(api, HEAD, state, {"number": 24}, [api.work_issue])
        self.assertEqual(result["action"], "cutover-metadata-recovered")
        self.assertEqual(api.work_issue["state"], "open")
        self.assertIn("status:in-progress", control.labels(api.work_issue))
        current = control.read_state(api)[1]
        self.assertIsNone(current["lease"])
        self.assertEqual(current["checkpoint"]["pending_task_reconciliation"]["policy_commit"], SOURCE)
        self.assertEqual(current["checkpoint"]["issue"], 19)

    def test_migration_envelopes_reject_arbitrary_refs_and_invalid_proof(self):
        for command, args in [("fence-control", {"worker": "one-worker", "lease": "owner", "ref": "refs/tags/release"}),
                ("retire-control", {"worker": "one-worker", "lease": "owner", "notes_proof_sha": "trunk", "task_receipt_id": 202})]:
            body = commands.MARKER + "\n" + commands.FENCE + "json\n" + json.dumps(
                {"schema": 1, "command": command, "expected_sha": HEAD, "args": args}) + "\n" + commands.FENCE
            with self.subTest(command=command), self.assertRaises(ValueError):
                commands.payload(body)


class InstalledReviewScenarios(unittest.TestCase):
    def test_ordinary_gate_still_rejects_a_closed_pr(self):
        api = ReviewNative()
        api.current_pr.update(state="closed", merged_at=lease.stamp(AT), merge_commit_sha=SOURCE)
        with self.assertRaises(ValueError):
            qa.evaluate(api, 3)

    def test_installed_mode_requires_actual_default_and_exact_two_parents(self):
        class Installed(ReviewNative):
            def ref(self, location):
                return SOURCE if location == "trunk" else super().ref(location)
            def rest(self, method, path, value=None):
                if path == "git/commits/" + SOURCE:
                    return {"sha": SOURCE, "parents": [{"sha": self.current_pr["base"]["sha"]},
                        {"sha": self.current_pr["head"]["sha"]}]}
                return super().rest(method, path, value)
        api = Installed()
        api.current_pr.update(state="closed", merged_at=lease.stamp(AT), merge_commit_sha=SOURCE)
        self.assertEqual(qa.evaluate(api, 3, allow_merged=True)["merge"], SOURCE)
        api.current_pr["merge_commit_sha"] = HEAD
        with self.assertRaises(ValueError):
            qa.evaluate(api, 3, allow_merged=True)


if __name__ == "__main__":
    unittest.main()

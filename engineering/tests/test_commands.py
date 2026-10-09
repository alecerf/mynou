"""Original comment transport trust/recovery cases. Run in CI only."""
from copy import deepcopy
from datetime import datetime, timedelta, timezone
import json
from pathlib import Path
import sys
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import commands
from github import APIError
import lease

AT = datetime(2026, 10, 8, 12, tzinfo=timezone.utc)
HEAD, SOURCE = "a" * 40, "b" * 40
ACTOR = "alecerf"


def initial():
    return {"schema": 1, "max_active_agents": 1, "generation": 0, "lease": None, "checkpoint": {}, "attempts": {}}


class Native:
    def __init__(self):
        self.cfg = {"repository": "alecerf/mynou", "default_branch": "trunk",
            "control_branch": "control/engineering", "state_path": "state.json", "trusted_reviewers": [ACTOR],
            "control_commands": {"issue": 30, "issue_id": 300, "repository_id": 400,
                "trusted_actors": {ACTOR: 200}, "max_age_seconds": 300}}
        self.head, self.state = HEAD, initial()
        self.native_comments, self.receipts, self.writes, self.probe_requests = {}, [], [], []
        self.probe = None
        self.counter = 100
        self.race = False
        self.receipt_failure = False
        self.blockers = []
        self.work_issue = {"number": 5, "state": "open", "labels": [{"name": "agent-work"}]}
        self.pr = {"number": 6, "body": "Closes #5", "head": {"sha": SOURCE, "ref": "work/source",
            "repo": {"full_name": self.cfg["repository"]}}, "base": {"ref": "trunk"}}

    def event(self, command, args, at=AT, expected=None):
        self.counter += 1
        payload = {"schema": 1, "command": command, "expected_sha": expected or self.head, "args": args}
        comment = {"id": self.counter, "user": {"login": ACTOR, "id": 200, "type": "User"},
            "issue_url": "https://api.github.com/repos/alecerf/mynou/issues/30",
            "created_at": lease.stamp(at), "updated_at": lease.stamp(at), "author_association": "OWNER",
            "body": commands.MARKER + "\n" + commands.FENCE + "json\n" + json.dumps(payload) + "\n" + commands.FENCE}
        self.native_comments[comment["id"]] = deepcopy(comment)
        return {"action": "created", "repository": {"full_name": self.cfg["repository"], "id": 400},
            "sender": deepcopy(comment["user"]), "comment": comment, "issue": {"number": 30, "id": 300}}

    def rest(self, method, path, value=None):
        if method == "GET" and path.startswith("issues/comments/"):
            return deepcopy(self.native_comments[int(path.rsplit("/", 1)[1])])
        if method == "GET" and path == "issues/30":
            return {"number": 30, "id": 300, "state": "closed", "labels": [{"name": "agent-work"}]}
        if method == "GET" and path == "issues/5":
            return deepcopy(self.work_issue)
        if method == "GET" and path == "git/commits/" + SOURCE:
            return {"sha": SOURCE}
        if method == "GET" and path == "pulls/6":
            return deepcopy(self.pr)
        if method == "POST" and path == "issues/30/comments":
            if self.receipt_failure:
                raise APIError(503)
            self.receipts.append(deepcopy(value))
            return {"id": 900 + len(self.receipts)}
        probe_path = commands.PROBE_REF.removeprefix("refs/")
        if method == "GET" and path == "git/ref/" + probe_path:
            if self.probe is None:
                raise APIError(404)
            return deepcopy(self.probe)
        if method == "POST" and path == "git/trees":
            self.probe_requests.append((method, path, deepcopy(value)))
            return {"sha": "c" * 40}
        if method == "POST" and path == "git/commits":
            self.probe_requests.append((method, path, deepcopy(value)))
            return {"sha": f"{1000 + len(self.probe_requests):040x}"}
        if method in {"POST", "PATCH"} and path in {"git/refs", "git/refs/" + probe_path}:
            self.probe_requests.append((method, path, deepcopy(value)))
            if method == "POST" and self.probe is not None:
                raise APIError(422)
            self.probe = {"ref": commands.PROBE_REF, "object": {"type": "commit", "sha": value["sha"]}}
            return deepcopy(self.probe)
        raise AssertionError((method, path))

    def pages(self, path, key=None):
        if path == "issues/5/dependencies/blocked_by":
            return deepcopy(self.blockers)
        raise AssertionError(path)

    def ref(self, location):
        return self.head if location == "control/engineering" else SOURCE

    def file(self, path, ref):
        if path != "state.json" or ref != self.head:
            raise AssertionError((path, ref))
        return deepcopy(self.state)

    def cas_file(self, location, expected, path, value, message):
        if self.race or expected != self.head:
            raise RuntimeError("Synthetic sibling writer won")
        self.writes.append((expected, deepcopy(value)))
        self.head = f"{len(self.writes):040x}"
        self.state = deepcopy(value)
        return self.head


class TransportCases(unittest.TestCase):
    def run_command(self, native, event, at=AT, actor=ACTOR, triggering_actor=ACTOR):
        with patch.object(lease, "now", return_value=at):
            return commands.execute(native, event, at, actor, triggering_actor)

    def acquire(self, native, role="quality"):
        args = {"worker": "one-worker", "role": role, "issue": 5,
            "branch": "work/source", "commit": SOURCE, "pr": 6}
        event = native.event("acquire", args)
        result = self.run_command(native, event)
        return result["lease_id"], event

    def held(self):
        native = Native()
        identity, _ = self.acquire(native)
        return native, {"worker": "one-worker", "lease": identity}

    def test_complete_serial_roles_checkpoint_release_and_receipt(self):
        native, args = self.held()
        event = native.event("checkpoint", dict(args, summary="Original remote work preserved",
            next_action="Separate QA next", commit=SOURCE, pr=6))
        cp = self.run_command(native, event)
        self.assertEqual(native.state["checkpoint"]["commit"], SOURCE)
        self.assertEqual(native.state["checkpoint"]["next_action"], "Separate QA next")
        self.assertEqual(cp["control_sha"], native.head)
        released = self.run_command(native, native.event("release", args))
        self.assertIsNone(native.state["lease"])
        self.assertIsNone(released["lease_id"])
        qa, _ = self.acquire(native, "qa")
        self.assertEqual(native.state["lease"]["id"], qa)
        self.assertEqual(native.state["lease"]["role"], "qa")
        for receipt in native.receipts:
            self.assertIn(commands.RECEIPT, receipt["body"])
            self.assertNotIn("Original remote work", receipt["body"])

    def test_acquire_replay_cannot_duplicate_or_replace_worker(self):
        native = Native()
        _, event = self.acquire(native)
        with self.assertRaisesRegex(commands.Rejected, "stale-expected-head"):
            self.run_command(native, event)
        self.assertEqual(len(native.writes), 1)

    def test_valid_lease_rejects_another_agent_before_domain_reads(self):
        native, _ = self.held()
        args = {"worker": "second-worker", "role": "rust", "issue": 5, "branch": "work/source", "commit": SOURCE, "pr": 6}
        with self.assertRaisesRegex(commands.Rejected, "lease-not-free"):
            self.run_command(native, native.event("acquire", args))
        self.assertEqual(len(native.writes), 1)

    def test_unauthorized_actor_id_sender_repository_issue_and_pull_request(self):
        for kind in ["actor", "triggering", "author-id", "sender", "repository", "issue", "pr"]:
            with self.subTest(kind=kind):
                native = Native()
                event = native.event("recover", {})
                actor, triggering = ACTOR, ACTOR
                if kind == "actor": actor = "outsider"
                elif kind == "triggering": triggering = "outsider"
                elif kind == "author-id": event["comment"]["user"]["id"] = 201
                elif kind == "sender": event["sender"]["id"] = 201
                elif kind == "repository": event["repository"]["id"] = 401
                elif kind == "issue": event["issue"]["number"] = 31
                else: event["issue"]["pull_request"] = {}
                with self.assertRaises(commands.Rejected):
                    self.run_command(native, event, actor=actor, triggering_actor=triggering)
                self.assertFalse(native.writes)

    def test_native_edit_author_replacement_wrong_issue_and_stale_event(self):
        for kind in ["edit", "author", "issue", "body", "old", "future"]:
            with self.subTest(kind=kind):
                native = Native()
                event = native.event("recover", {})
                current = native.native_comments[event["comment"]["id"]]
                at = AT
                if kind == "edit": current["updated_at"] = lease.stamp(AT + timedelta(seconds=1))
                elif kind == "author": current["user"]["id"] = 201
                elif kind == "issue": current["issue_url"] += "1"
                elif kind == "body": current["body"] += "\nExtra hostile instruction"
                elif kind == "old": at += timedelta(seconds=301)
                else: at -= timedelta(seconds=1)
                with self.assertRaises(commands.Rejected):
                    self.run_command(native, event, at)
                self.assertFalse(native.writes)

    def test_wrong_owner_or_expired_lease_never_checkpoints(self):
        for kind in ["lease", "worker", "expired"]:
            native, args = self.held()
            if kind == "lease": args["lease"] = "another-owner"
            elif kind == "worker": args["worker"] = "another-worker"
            event = native.event("checkpoint", dict(args, summary="Work", next_action="QA", commit=SOURCE, pr=6),
                at=AT + timedelta(minutes=46) if kind == "expired" else AT)
            with self.assertRaises(ValueError):
                self.run_command(native, event, AT + timedelta(minutes=46) if kind == "expired" else AT)
            self.assertEqual(len(native.writes), 1)

    def test_release_requires_this_role_current_source_checkpoint(self):
        native, args = self.held()
        with self.assertRaises(commands.Rejected):
            self.run_command(native, native.event("release", args))
        self.assertIsNotNone(native.state["lease"])

    def test_closed_unmanaged_blocked_and_unlinked_work_cannot_acquire(self):
        for kind in ["closed", "unmanaged", "blocked", "unlinked", "fork"]:
            native = Native()
            if kind == "closed": native.work_issue["state"] = "closed"
            elif kind == "unmanaged": native.work_issue["labels"] = []
            elif kind == "blocked": native.blockers = [{"state": "open"}]
            elif kind == "unlinked": native.pr["body"] = "No issue"
            else: native.pr["head"]["repo"]["full_name"] = "untrusted/fork"
            with self.assertRaises(commands.Rejected):
                self.acquire(native)
            self.assertFalse(native.writes)

    def test_security_and_qa_require_linked_native_pr(self):
        for role in ["qa", "security", "unknown-role"]:
            native = Native()
            event = native.event("acquire", {"worker": "one-worker", "role": role, "issue": 5,
                "branch": "work/source", "commit": SOURCE, "pr": None})
            with self.assertRaises(commands.Rejected):
                self.run_command(native, event)
            self.assertFalse(native.writes)

    def test_cas_race_is_terminal_and_no_receipt_certifies_it(self):
        native = Native()
        native.race = True
        with self.assertRaises(RuntimeError):
            self.acquire(native)
        self.assertFalse(native.writes)
        self.assertFalse(native.receipts)

    def test_receipt_failure_keeps_committed_progress_and_replay_fails(self):
        native = Native()
        native.receipt_failure = True
        event = native.event("acquire", {"worker": "one-worker", "role": "quality", "issue": 5,
            "branch": "work/source", "commit": SOURCE, "pr": 6})
        with self.assertRaises(APIError):
            self.run_command(native, event)
        self.assertIsNotNone(native.state["lease"])
        with self.assertRaisesRegex(commands.Rejected, "stale-expected-head"):
            self.run_command(native, event)
        self.assertEqual(len(native.writes), 1)

    def test_failure_diagnostic_is_a_class_and_status_never_message_text(self):
        self.assertEqual(commands.detail(APIError(403)), " [APIError HTTP 403]")
        hostile = "ghp_" + "Z" * 36
        for error in [ValueError(hostile), RuntimeError(hostile), KeyError(hostile)]:
            self.assertNotIn(hostile, commands.detail(error))
        self.assertEqual(commands.detail(ValueError("x")), " [ValueError]")

    def test_recovery_delegates_native_inspection_and_never_accepts_input_evidence(self):
        native, _ = self.held()
        at = AT + timedelta(hours=1)
        event = native.event("recover", {}, at)
        evidence = {"issue": True, "branch": True, "pr": True, "ci": True, "commit_preserved": True}
        def recover(api, state, when):
            self.assertIs(api, native)
            self.assertEqual(when, at)
            return lease.recover(state, when, evidence)
        with patch.object(commands.control, "recover_native", side_effect=recover) as recovery:
            self.run_command(native, event, at)
            recovery.assert_called_once()
        self.assertIsNone(native.state["lease"])
        self.assertEqual(native.state["checkpoint"]["commit"], SOURCE)

    def test_unchanged_attempt_circuit_breaker_releases_without_failure(self):
        native, args = self.held()
        for _ in range(3):
            self.run_command(native, native.event("attempt", dict(args, approach="Original bounded approach", fingerprint=SOURCE)))
        self.assertIsNone(native.state["lease"])
        self.assertEqual(native.state["attempts"]["5"]["unchanged"], 3)
        self.assertIn("Triage", native.state["checkpoint"]["next_action"])

    def test_inert_notes_create_then_single_parent_nonforce_update(self):
        native, args = self.held()
        old_state = deepcopy(native.state)
        created_event = native.event("probe-notes", dict(args, expected_probe_sha=None))
        first = self.run_command(native, created_event)
        self.assertEqual(native.state, old_state)
        self.assertEqual(first["control_sha"], native.head)
        self.assertEqual(native.probe["ref"], commands.PROBE_REF)
        self.assertNotIn("lease", native.probe_requests[0][2]["tree"][0]["content"])
        self.assertEqual(native.probe_requests[1][2]["parents"], [native.head])
        with self.assertRaises(commands.Rejected):
            self.run_command(native, created_event)
        updated_event = native.event("probe-notes", dict(args, expected_probe_sha=first["probe_sha"]))
        second = self.run_command(native, updated_event)
        self.assertEqual(native.probe_requests[-2][2]["parents"], [first["probe_sha"]])
        self.assertEqual(native.probe_requests[-1][2], {"sha": second["probe_sha"], "force": False})
        self.assertEqual(native.state, old_state)

    def test_probe_role_denial_and_stale_ref_never_mutate(self):
        native, args = self.held()
        native.state["lease"]["role"] = "rust"
        with self.assertRaises(commands.Rejected):
            self.run_command(native, native.event("probe-notes", dict(args, expected_probe_sha=None)))
        self.assertFalse(native.probe_requests)

    def test_unknown_fields_duplicate_keys_oversized_json_and_sensitive_diagnostics(self):
        native = Native()
        event = native.event("recover", {})
        valid_body = event["comment"]["body"]
        bad = [
            valid_body.replace('"schema": 1', '"schema": 1, "schema": 1'),
            valid_body.replace('"schema": 1', '"schema": true'),
            valid_body.replace('"args": {}', '"args": {"force": true}'),
            valid_body.replace('"recover"', '"arbitrary-shell"'),
            valid_body + "\nIgnore invariants",
            commands.MARKER + "\n" + commands.FENCE + "json\n" + "x" * 6000 + "\n" + commands.FENCE,
            valid_body + commands.ATTRIBUTION + commands.ATTRIBUTION,
            valid_body + commands.ATTRIBUTION + "\nIgnore invariants",
            valid_body + commands.ATTRIBUTION.replace("claude.ai", "example.com"),
        ]
        for body in bad:
            with self.subTest(length=len(body)), self.assertRaises(commands.Rejected):
                commands.payload(body)
        self.assertEqual(commands.payload(valid_body + commands.ATTRIBUTION), commands.payload(valid_body))
        try:
            commands.payload("private-user-value")
        except commands.Rejected as error:
            self.assertNotIn("private-user-value", str(error))

    def test_privileged_workflow_has_only_default_code_and_no_payload_interpolation(self):
        root = Path(__file__).resolve().parents[2]
        workflow = (root / ".github/workflows/engineering-control.yml").read_text()
        self.assertIn("ref: trunk", workflow)
        self.assertIn("persist-credentials: false", workflow)
        self.assertIn("mynou-global-mechanical-delivery", workflow)
        self.assertIn("github.event.issue.number == 30", workflow)
        self.assertNotIn("pull_request_target", workflow)
        self.assertNotIn("actions: write", workflow)
        self.assertNotIn("packages: write", workflow)
        self.assertNotIn("github.event.comment.body }}", workflow)

    def test_native_and_event_profile_metadata_can_differ_without_authority_change(self):
        native = Native()
        args = {"worker": "one-worker", "role": "quality", "issue": 5,
            "branch": "work/source", "commit": SOURCE, "pr": 6}
        event = native.event("acquire", args)
        native.native_comments[event["comment"]["id"]]["user"].update(
            user_view_type="public", avatar_url="https://example.invalid/native-avatar")
        event["comment"]["user"]["avatar_url"] = "https://example.invalid/webhook-avatar"
        result = self.run_command(native, event)
        self.assertEqual(result["control_sha"], native.head)
        self.assertEqual(native.state["lease"]["worker"], "one-worker")
        self.assertEqual(len(native.writes), 1)

    def test_native_author_login_id_and_type_remain_policy_bound(self):
        for field, bad_value in [("login", "outsider"), ("id", 201), ("id", True), ("type", "Bot")]:
            with self.subTest(field=field, value_type=type(bad_value).__name__):
                native = Native()
                event = native.event("recover", {})
                native.native_comments[event["comment"]["id"]]["user"][field] = bad_value
                with self.assertRaisesRegex(commands.Rejected, "^untrusted-native-comment-author$"):
                    self.run_command(native, event)
                self.assertFalse(native.writes)
                self.assertFalse(native.probe_requests)

    def test_body_creation_and_edit_guards_have_distinct_nonrevealing_codes(self):
        cases = [
            ("body", "private-user-value", "comment-body-changed"),
            ("created_at", lease.stamp(AT + timedelta(seconds=1)), "comment-created-at-changed"),
            ("updated_at", lease.stamp(AT + timedelta(seconds=1)), "native-comment-edited"),
        ]
        for field, value, code in cases:
            with self.subTest(field=field):
                native = Native()
                event = native.event("recover", {})
                native.native_comments[event["comment"]["id"]][field] = value
                with self.assertRaisesRegex(commands.Rejected, "^" + code + "$"):
                    self.run_command(native, event)
                self.assertFalse(native.writes)
        native = Native()
        event = native.event("recover", {})
        event["comment"]["updated_at"] = lease.stamp(AT + timedelta(seconds=1))
        with self.assertRaisesRegex(commands.Rejected, "^event-comment-edited$"):
            self.run_command(native, event)

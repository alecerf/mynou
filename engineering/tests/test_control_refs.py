"""Original control-reference regressions; execute exclusively in GitHub CI."""
from copy import deepcopy
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from github import APIError, GitHub, control_reference, reference
import control
import delivery
import qa
from test_organization import BASE, CFG, HEAD, Native, state

NOTES = "refs/notes/mynou-engineering"
NEXT = "f" * 40


class RefAPI(GitHub):
    def __init__(self, location=NOTES):
        self.cfg = dict(CFG, control_ref=location)
        self.cfg.pop("control_branch")
        self.location = location
        self.head = HEAD
        self.calls = []
        self.returned_ref = location
        self.object_type = "commit"
        self.object_sha = None
        self.race = False
        self.denied_update = None

    def native(self, sha):
        return {"ref": self.returned_ref, "object": {
            "type": self.object_type, "sha": self.object_sha or sha}}

    def rest(self, method, path, value=None):
        self.calls.append((method, path, deepcopy(value)))
        if method == "GET":
            return self.native(self.head)
        if path == "git/trees":
            return {"sha": BASE}
        if path == "git/commits":
            if self.race:
                self.head = "e" * 40
            return {"sha": NEXT}
        if method == "PATCH":
            if self.denied_update:
                raise APIError(self.denied_update)
            if self.head != HEAD:
                raise APIError(422)
            self.head = value["sha"]
            return self.native(self.head)
        raise AssertionError((method, path, value))

    def file(self, path, ref):
        return state()


class ReferenceScenarios(unittest.TestCase):
    def test_config_has_one_authority_and_preserves_legacy_aliases(self):
        self.assertEqual(control_reference(CFG), "control/engineering")
        self.assertEqual(control_reference(dict(CFG, control_ref="refs/heads/control/engineering")),
            "refs/heads/control/engineering")
        cfg = dict(CFG, control_ref=NOTES)
        with self.assertRaises(ValueError):
            control_reference(cfg)
        del cfg["control_branch"]
        self.assertEqual(control_reference(cfg), NOTES)
        for value in [{}, {"control_ref": "notes/state"}, {"control_ref": None},
                      {"control_branch": ""}, {"control_ref": "refs/tags/v0.22.19"}]:
            with self.subTest(value=value), self.assertRaises(ValueError):
                control_reference(value)

    def test_product_default_cannot_become_the_control_state_tree(self):
        for cfg in [dict(CFG, control_branch="trunk"),
                    dict(CFG, control_branch="trunk", control_ref="refs/heads/trunk")]:
            api = RefAPI()
            api.cfg = cfg
            with self.subTest(cfg=cfg), self.assertRaises(ValueError):
                control.read_state(api)
            self.assertEqual(api.calls, [])
        api = RefAPI("refs/heads/trunk")
        with self.assertRaises(ValueError):
            control.read_state(api)
        self.assertEqual(api.calls, [])

    def test_legacy_branch_alias_cannot_activate_notes_even_when_aliases_agree(self):
        for explicit in [False, True]:
            api = RefAPI()
            api.cfg["control_branch"] = NOTES
            if not explicit:
                api.cfg.pop("control_ref")
            with self.subTest(explicit=explicit), self.assertRaises(ValueError):
                control.read_state(api)
            self.assertEqual(api.calls, [])

    def test_reference_validation_blocks_tags_traversal_and_request_confusion(self):
        invalid = ["", None, "refs/tags/v0.22.19", "refs/pull/24/head", "../state",
            "refs/notes/../state", "refs/notes/.private", "refs/notes/x.lock/y",
            "refs/notes/state.", "refs/notes//state", "refs/notes/state?ref=trunk",
            "refs/notes/state#trunk", "refs/notes/state%2Fother", "refs/notes/state\\other",
            "refs/notes/a\nb", "refs/notes/" + "x" * 161]
        for location in invalid:
            api = RefAPI()
            with self.subTest(location=location), self.assertRaises(ValueError):
                api.ref(location)
            self.assertEqual(api.calls, [])
        self.assertEqual(reference("work/control-ref-adapter"), "refs/heads/work/control-ref-adapter")
        self.assertEqual(reference(NOTES), NOTES)

    def test_native_ref_identity_and_direct_commit_type_are_required(self):
        for name, kind, sha in [(NOTES + "-other", "commit", HEAD),
                (NOTES, "tag", HEAD), (NOTES, "commit", HEAD.upper()),
                (NOTES, "commit", "a" * 39)]:
            api = RefAPI()
            api.returned_ref, api.object_type, api.object_sha = name, kind, sha
            with self.subTest(name=name, kind=kind, sha=sha), self.assertRaises(ValueError):
                api.ref(NOTES)
        api = RefAPI()
        self.assertEqual(api.ref(NOTES), HEAD)
        self.assertEqual(api.calls[0][:2], ("GET", "git/ref/notes/mynou-engineering"))

    def test_stale_expected_head_and_invalid_sha_never_create_commits(self):
        api = RefAPI()
        with self.assertRaises(RuntimeError):
            api.cas_file(NOTES, BASE, "state.json", state(), "original stale owner")
        self.assertEqual([c[0] for c in api.calls], ["GET"])
        api = RefAPI()
        with self.assertRaises(ValueError):
            api.cas_file(NOTES, "trunk", "state.json", state(), "not an exact head")
        self.assertEqual(api.calls, [])

    def test_nonbranch_write_keeps_sole_parent_and_nonforce_arbitration(self):
        api = RefAPI()
        self.assertEqual(api.cas_file(NOTES, HEAD, "state.json", state(), "original notes lease"), NEXT)
        commit = next(c for c in api.calls if c[1] == "git/commits")
        update = next(c for c in api.calls if c[0] == "PATCH")
        self.assertEqual(commit[2]["parents"], [HEAD])
        self.assertEqual(update, ("PATCH", "git/refs/notes/mynou-engineering",
            {"sha": NEXT, "force": False}))
        self.assertEqual(api.head, NEXT)

    def test_competing_writer_or_conflict_preserves_the_winner_without_retry(self):
        for code in [None, 409, 422]:
            api = RefAPI()
            api.race, api.denied_update = True, code
            with self.subTest(code=code), self.assertRaises(RuntimeError):
                api.cas_file(NOTES, HEAD, "state.json", state(), "original losing writer")
            self.assertEqual(api.head, "e" * 40)
            self.assertEqual(sum(c[0] == "PATCH" for c in api.calls), 1)
            self.assertFalse(next(c[2] for c in api.calls if c[0] == "PATCH")["force"])

    def test_missing_notes_reference_never_falls_back_to_the_branch(self):
        class Missing(RefAPI):
            def rest(self, method, path, value=None):
                self.calls.append((method, path, deepcopy(value)))
                raise APIError(404)
        api = Missing()
        with self.assertRaises(APIError):
            control.read_state(api)
        self.assertEqual(api.calls, [("GET", "git/ref/notes/mynou-engineering", None)])

    def test_conflicting_authorities_fail_before_native_storage_reads(self):
        api = RefAPI()
        api.cfg["control_branch"] = "control/engineering"
        with self.assertRaises(ValueError):
            control.read_state(api)
        self.assertEqual(api.calls, [])

    def test_ambiguous_update_response_is_not_announced_as_ownership(self):
        class Uncertain(RefAPI):
            def rest(self, method, path, value=None):
                result = super().rest(method, path, value)
                if method == "PATCH":
                    result["ref"] = NOTES + "-other"
                return result
        api = Uncertain()
        with self.assertRaises(ValueError):
            api.cas_file(NOTES, HEAD, "state.json", state(), "original uncertain response")
        self.assertEqual(api.head, NEXT)
        self.assertEqual(api.ref(NOTES), NEXT)

    def test_configured_authority_is_preserved_even_outside_old_branch_prefix(self):
        cfg = dict(CFG, control_ref="refs/heads/work/control-state")
        cfg.pop("control_branch")
        allowed, reason = delivery.branch_decision("work/control-state", HEAD, cfg,
            "ahead", [], [], None)
        self.assertFalse(allowed)
        self.assertIn("durable", reason)

    def test_qa_history_still_uses_exact_review_commits_and_canonical_ancestry(self):
        class NotesReviews(Native):
            def __init__(self):
                super().__init__()
                self.cfg = dict(CFG, control_ref=NOTES)
                self.cfg.pop("control_branch")
                self.references = []
            def ref(self, location):
                self.references.append(location)
                if location != NOTES:
                    raise AssertionError("QA consulted another authority")
                return "e" * 40
        api = NotesReviews()
        proof = qa.evaluate(api, 3)
        self.assertEqual(api.references, [NOTES, NOTES])
        self.assertEqual([r["role"] for r in proof["reviews"]], ["qa", "security"])
        api.ancestry = "diverged"
        with self.assertRaises(ValueError):
            qa.evaluate(api, 3)


if __name__ == "__main__":
    unittest.main()

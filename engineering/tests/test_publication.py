"""Original source/publication/recovery boundary scenarios; Actions only."""
import base64
from copy import deepcopy
from datetime import timedelta
import hashlib
import io
import json
from pathlib import Path
import sys
import unittest
from unittest.mock import patch
import zipfile

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import control
import delivery
import lease
import publication
from github import APIError
import test_organization as fixtures
from test_organization import AT, BASE, HEAD, Native, held, pr
from test_product_planning import PLAN, anchor


def archive(proof, name="mynou-image.json"):
    stream = io.BytesIO()
    with zipfile.ZipFile(stream, "w", zipfile.ZIP_DEFLATED) as result:
        result.writestr(name, json.dumps(proof))
    return stream.getvalue()


class Published(fixtures.PublicationScenarios.Publication):
    """Synthetic native metadata, not claims of real verification or publication."""
    def __init__(self):
        super().__init__()
        self.runs = [dict(id=40, head_sha=HEAD, head_branch="trunk", event="workflow_dispatch",
            status="completed", conclusion="success", run_attempt=1)]
        self.proof = {"image": "ghcr.io/original/mynou@sha256:" + "f" * 64,
            "version": "0.22.29", "source": HEAD, "visibility": "private",
            "binary_sha256": "a" * 64, "repository": self.repo, "repository_id": 200,
            "repository_visibility": "public", "package_id": 201, "repository_association": "reported-match",
            "verification_user": "1001:1001", "verification_cleanup": "completed"}
        self.release = {"id": 42, "tag_name": "v0.22.29", "draft": False, "prerelease": False,
            "immutable": True, "published_at": lease.stamp(AT - timedelta(seconds=30)),
            "body": "<!-- mynou-ci-release -->\nSource commit: `" + HEAD + "`.\n`" + self.proof["image"] + "`",
            "assets": [dict(id=i + 100, name=name, size=1234, state="uploaded", digest="sha256:" + "a" * 64)
                for i, name in enumerate(["mynou-v0.22.29-linux-x86_64", "mynou-v0.22.29-macos-arm64",
                    "mynou-v0.22.29-macos-x86_64", "SHA256SUMS"])]}
        self.jobs = [dict(name=name, status="completed", conclusion="success")
            for name in [*self.cfg["required_workflows"]["Mynou CI"], "release"]]
        self.tag = {"ref": "refs/tags/v0.22.29", "object": {"sha": HEAD, "type": "commit"}}
        self.artifacts = [{"id": 50, "name": f"mynou-publication-{HEAD}-40-1", "expired": False,
            "size_in_bytes": 1000, "workflow_run": {"id": 40, "head_sha": HEAD, "head_branch": "trunk", "repository_id": 200, "head_repository_id": 200}}]
        self.replace_archive()
        self.advance_on_run = False

    def replace_archive(self, raw=None):
        self.raw = archive(self.proof) if raw is None else raw
        self.artifacts[0]["digest"] = "sha256:" + hashlib.sha256(self.raw).hexdigest()

    def rest(self, method, path, value=None):
        if method == "GET":
            if path == "contents/Cargo.toml?ref=" + HEAD:
                return {"encoding": "base64", "content": base64.b64encode(b'[package]\nversion = "0.22.29"\n').decode()}
            if path == "git/ref/tags/v0.22.29": return deepcopy(self.tag)
            if path == "releases/tags/v0.22.29": return deepcopy(self.release)
            if path == "": return {"id": 200, "full_name": self.repo, "private": False}
            if path == "actions/runs/40":
                if self.advance_on_run: self.default = BASE
                return deepcopy(self.runs[0])
        return super().rest(method, path, value)

    def pages(self, path, key=None):
        if path == "actions/runs/40/jobs?filter=latest": return deepcopy(self.jobs)
        if path == "actions/runs/40/artifacts": return deepcopy(self.artifacts)
        return super().pages(path, key)

    def publication_archive(self, identity):
        if identity != 50: raise AssertionError("Wrong original fixture artifact")
        return self.raw


class PublicationEvidence(unittest.TestCase):
    def verify(self, native):
        with patch.object(lease, "now", return_value=AT):
            return publication.verified(native, HEAD, native.runs[0])

    def test_exact_release_assets_private_binary_proof_and_actual_publication_time(self):
        native = Published()
        result = self.verify(native)
        self.assertEqual((result["state"], result["commit"], result["release_id"]), ("published", HEAD, 42))
        self.assertEqual(result["at"], native.release["published_at"])
        self.assertNotEqual(result["at"], lease.stamp(AT))
        self.assertEqual(result["registry_proof"], native.proof)
        self.assertNotIn("body", result)
        self.assertEqual(len(result["assets"]), 4)

    def test_wrong_source_branch_event_attempt_or_native_ci_failure_is_not_publication(self):
        for field, value in [("head_sha", BASE), ("head_branch", "work/original"), ("event", "pull_request"),
                ("run_attempt", True), ("status", "in_progress"), ("conclusion", "failure")]:
            native = Published(); native.runs[0][field] = value
            with self.subTest(field=field), self.assertRaises(ValueError): self.verify(native)

    def test_every_build_package_release_job_must_be_present_once_and_successful(self):
        for name in [j["name"] for j in Published().jobs]:
            for change in ["missing", "duplicate", "failed", "skipped"]:
                native = Published()
                item = next(j for j in native.jobs if j["name"] == name)
                if change == "missing": native.jobs.remove(item)
                elif change == "duplicate": native.jobs.append(deepcopy(item))
                else: item["conclusion"] = "failure" if change == "failed" else "skipped"
                with self.subTest(name=name, change=change), self.assertRaises(ValueError): self.verify(native)

    def test_draft_mutable_prerelease_missing_wrong_or_future_release_is_rejected(self):
        for field, value in [("draft", True), ("immutable", False), ("prerelease", True), ("tag_name", "v0.0.1"),
                ("id", True), ("published_at", None), ("published_at", lease.stamp(AT + timedelta(seconds=1))),
                ("body", "Unverified private fixture text")]:
            native = Published(); native.release[field] = value
            with self.subTest(field=field), self.assertRaises(ValueError): self.verify(native)

    def test_tag_must_identify_this_exact_commit(self):
        for value in [{"sha": BASE, "type": "commit"}, {"sha": HEAD, "type": "tag"}]:
            native = Published(); native.tag["object"] = value
            with self.assertRaises(ValueError): self.verify(native)

    def test_missing_extra_undigested_empty_duplicate_or_unuploaded_assets_are_rejected(self):
        for change in ["missing", "extra", "digest", "size", "duplicate", "state", "id"]:
            native = Published()
            if change == "missing": native.release["assets"].pop()
            elif change == "extra": native.release["assets"].append(deepcopy(native.release["assets"][0]))
            elif change == "duplicate": native.release["assets"][0]["id"] = native.release["assets"][1]["id"]
            else: native.release["assets"][0][change] = {"digest": None, "size": 0, "state": "new", "id": True}[change]
            with self.subTest(change=change), self.assertRaises(ValueError): self.verify(native)

    def test_proof_native_run_repository_attempt_expiry_and_inventory_are_bound(self):
        for change in ["run", "source", "branch", "repository", "attempt", "expired", "duplicate", "missing", "size"]:
            native = Published(); artifact = native.artifacts[0]
            if change in ["run", "source", "branch", "repository"]:
                key, value = {"run": ("id", 41), "source": ("head_sha", BASE),
                    "branch": ("head_branch", "work/original"), "repository": ("repository_id", 999)}[change]
                artifact["workflow_run"][key] = value
            elif change == "attempt": artifact["name"] = f"mynou-publication-{HEAD}-40-2"
            elif change == "expired": artifact["expired"] = True
            elif change == "duplicate": native.artifacts.append(deepcopy(artifact))
            elif change == "missing": native.artifacts = []
            else: artifact["size_in_bytes"] = 64 * 1024 + 1
            with self.subTest(change=change), self.assertRaises(ValueError): self.verify(native)

    def test_private_image_provenance_policy_binary_digest_and_allowlist_are_required(self):
        for field, value in [("source", BASE), ("version", "0.0.1"), ("repository", "other/repo"),
                ("repository_id", True), ("repository_visibility", "private"), ("package_id", True),
                ("visibility", "public"), ("verification_cleanup", "pending"), ("verification_user", "0:0"),
                ("repository_association", "unverified"), ("binary_sha256", "b" * 64),
                ("image", "ghcr.io/other/repo@sha256:" + "f" * 64), ("unexpected", "private fixture value")]:
            native = Published(); native.proof[field] = value; native.replace_archive()
            with self.subTest(field=field), self.assertRaises(ValueError): self.verify(native)
        native = Published(); native.proof["repository_association"] = "not_exposed"; native.replace_archive()
        self.assertEqual(self.verify(native)["registry_proof"]["repository_association"], "not_exposed")

    def test_archive_digest_members_expansion_and_corrupt_data_fail_closed_without_echo(self):
        for raw in [archive({"private": "fixture value"}), archive(Published().proof, "../private.json"),
                b"private malformed fixture", archive({"private": "x" * (16 * 1024 + 1)})]:
            native = Published(); native.replace_archive(raw)
            with self.assertRaises(ValueError) as error: self.verify(native)
            self.assertNotIn("fixture value", str(error.exception))
        native = Published(); native.artifacts[0]["digest"] = "sha256:" + "0" * 64
        with self.assertRaises(ValueError): self.verify(native)

    def test_source_race_or_changed_native_attempt_is_not_certified(self):
        native = Published(); native.advance_on_run = True
        with self.assertRaises(ValueError): self.verify(native)
        native = Published()
        original = native.rest
        def changed(method, path, value=None):
            result = original(method, path, value)
            if path == "actions/runs/40": result["run_attempt"] = 2
            return result
        native.rest = changed
        with self.assertRaises(ValueError): self.verify(native)

    def test_actual_reconciliation_rearms_product_once_and_retains_prior_work(self):
        native = Published()
        native.current_state["checkpoint"] = {"at": lease.stamp(AT - timedelta(minutes=2)), "issue": 2,
            "role": "rust", "branch": "work/interrupted", "commit": BASE, "pr": 4,
            "summary": "Original preserved source", "next_action": "Resume original PR4"}
        def execute():
            with patch.object(delivery, "read_state", side_effect=native.read), patch.object(delivery, "save", side_effect=native.save), patch.object(lease, "now", return_value=AT):
                return delivery.recover_publication(native, native.control_head, native.current_state, [native.current_pr])
        self.assertEqual(execute()["action"], "publication-recorded")
        self.assertIsNone(native.current_state["lease"])
        self.assertEqual(native.current_state["checkpoint"]["previous_checkpoint"]["next_action"], "Resume original PR4")
        plan = anchor()
        self.assertEqual(control.select(native.current_state, [plan], AT, PLAN)["action"], "plan-product")
        plan["updated_at"] = lease.stamp(AT)
        before = deepcopy(native.current_state)
        self.assertIsNone(execute())
        self.assertEqual(before, native.current_state)
        self.assertEqual(control.select(native.current_state, [plan], AT, PLAN)["action"], "idle")

    def test_unavailable_proof_is_truthfully_checkpointed_and_released(self):
        native = Published(); native.artifacts = []
        with patch.object(delivery, "read_state", side_effect=native.read), patch.object(delivery, "save", side_effect=native.save), patch.object(lease, "now", return_value=AT):
            with self.assertRaises(ValueError): delivery.recover_publication(native, native.control_head, native.current_state, [native.current_pr])
        self.assertIsNone(native.current_state["lease"])
        self.assertEqual(native.current_state["checkpoint"]["publication"]["state"], "publication-unverified")
        self.assertFalse(native.dispatches)


class MergePreservation(unittest.TestCase):
    MERGE = "e" * 40
    class Merged(Native):
        def __init__(self):
            super().__init__()
            self.current_pr = dict(pr(), state="closed", merged_at=lease.stamp(AT), merge_commit_sha=MergePreservation.MERGE)
            self.parents = [BASE, HEAD]
            self.default_status = "identical"
        def ref(self, name): return self.MERGE if name == "trunk" else HEAD
        @property
        def MERGE(self): return MergePreservation.MERGE
        def rest(self, method, path, value=None):
            if path == "git/commits/" + self.MERGE:
                return {"sha": self.MERGE, "parents": [{"sha": p} for p in self.parents]}
            if path == f"compare/{self.MERGE}...{HEAD}": return {"status": "behind"}
            if path == f"compare/{self.MERGE}...{self.MERGE}": return {"status": self.default_status}
            return super().rest(method, path, value)
        def pages(self, path, key=None):
            if path == "pulls?state=all": return [deepcopy(self.current_pr)]
            return super().pages(path, key)

    def recover(self, native):
        current = lease.checkpoint(held("triage"), AT, "original-owner", "Native merge preserved", "Observe default CI", self.MERGE, 3)
        return control.recover_native(native, current, AT + timedelta(hours=1))

    def test_native_default_merge_survives_the_original_work_branch_head(self):
        recovered = self.recover(self.Merged())
        self.assertIsNone(recovered["lease"])
        self.assertEqual(recovered["checkpoint"]["commit"], self.MERGE)
        evidence = recovered["checkpoint"]["recovery"]
        self.assertEqual(evidence["branch_head"], HEAD)
        self.assertEqual(evidence["preservation"], "exact native default merge and source parent")

    def test_foreign_unlinked_unmerged_or_unpreserved_default_merge_is_rejected(self):
        for change in ["fork", "issue", "number", "merge", "unmerged", "parent", "default"]:
            native = self.Merged()
            if change == "fork": native.current_pr["head"]["repo"]["full_name"] = "other/repo"
            elif change == "issue": native.current_pr["body"] = "Closes #2"
            elif change == "number": native.current_pr["number"] = 4
            elif change == "merge": native.current_pr["merge_commit_sha"] = BASE
            elif change == "unmerged": native.current_pr["merged_at"] = None
            elif change == "parent": native.parents = [BASE]
            else: native.default_status = "diverged"
            with self.subTest(change=change), self.assertRaises(ValueError): self.recover(native)

    def test_native_branch_transport_error_never_falls_back(self):
        native = self.Merged()
        native.ref = lambda name: (_ for _ in ()).throw(APIError(500))
        with self.assertRaises(APIError): self.recover(native)


if __name__ == "__main__":
    unittest.main()

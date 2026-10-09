"""CI-only native-provenance, receipt and changed-attempt coverage scenarios."""
import base64
import copy
import hashlib
import io
import json
import os
from pathlib import Path
import sys
import unittest
from unittest.mock import patch
import zipfile

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import log_coverage as coverage
import secret_audit as audit

HEAD = "a" * 40
DEFAULT = "b" * 40
MEDIA = "c" * 40
ENV = {"GITHUB_RUN_ID": "901", "GITHUB_RUN_ATTEMPT": "1", "GITHUB_SHA": DEFAULT}
FILES = {name: ("synthetic " + name).encode() for name in coverage.FILES}
POLICY = coverage.policy_digest(FILES)


def zipped(payload, name="actions-audit.json"):
    stream = io.BytesIO()
    with zipfile.ZipFile(stream, "w") as archive:
        archive.writestr(zipfile.ZipInfo(name, date_time=(2026, 1, 1, 0, 0, 0)), payload)
    return stream.getvalue()


def report_fixture():
    producer = {"run_id": 900, "attempt": 2, "head_sha": HEAD, "workflow_id": 7}
    entry = {"run_id": 100, "attempt": 1, "head_sha": MEDIA, "workflow_id": 8,
             "head_repository_id": 1, "state": "scanned", "members": 1}
    return {"schema": 2, "status": "passed", "finding_count": 0, "findings": [],
            "coverage_gaps": [], "attempts_examined": 1, "archives_scanned": 1,
            "archives_reused": 0, "attempts_reused": 0, "members_scanned": 1,
            "members_reused": 0, "attempts_without_execution": 0,
            "coverage": {"schema": 1, "repository_id": 1, "policy": POLICY,
                         "producer": producer, "attempts": [entry]}}


class NativeFixture:
    repository = "owner/repository"

    def __init__(self):
        self.budget = audit.RequestBudget()
        self.calls = []
        self.report = report_fixture()
        self.files = dict(FILES)
        self.missing_file = None
        self.ancestry = {"status": "ahead", "merge_base_commit": {"sha": HEAD}}
        self.candidate = {
            "id": 900, "workflow_id": 7, "head_branch": "trunk", "head_sha": HEAD,
            "head_repository": {"id": 1, "full_name": self.repository},
            "event": "push", "status": "completed", "conclusion": "success", "run_attempt": 2}
        self.runs = [{"id": 100, "workflow_id": 8, "head_sha": MEDIA,
                      "head_repository": {"id": 1}, "status": "completed", "run_attempt": 2}]
        self.jobs = [{"name": name, "status": "completed", "conclusion": "success"}
                     for name in ("source-history", "actions-logs")]
        self.artifacts = [{
            "id": 50, "name": "security-actions-900-2", "expired": False,
            "size_in_bytes": 1000, "expires_at": "2099-01-01T00:00:00Z",
            "workflow_run": {"id": 900, "repository_id": 1, "head_repository_id": 1,
                             "head_sha": HEAD, "head_branch": "trunk"}}]
        self.corrupt_digest = False
        self.unavailable = False
        self.executed = True

    def get(self, path, archive=False, limit=None):
        self.budget.begin()
        self.calls.append(path)
        if path == "":
            return {"id": 1, "full_name": self.repository, "default_branch": "trunk"}
        if path == "/git/ref/heads/trunk":
            return {"ref": "refs/heads/trunk", "object": {"type": "commit", "sha": DEFAULT}}
        if path == "/actions/workflows/security-audit.yml":
            return {"id": 7, "path": coverage.WORKFLOW, "state": "active"}
        if path.startswith("/actions/workflows/7/runs?"):
            return {"workflow_runs": [self.candidate]}
        if path.startswith("/compare/"):
            return self.ancestry
        if path.startswith("/contents/"):
            name = path[len("/contents/"):].split("?ref=", 1)[0]
            if name == self.missing_file:
                raise audit.HTTPFailure(404)
            data = self.files[name]
            return {"type": "file", "encoding": "base64", "size": len(data),
                    "content": base64.b64encode(data).decode()}
        if path == "/actions/artifacts/50/zip":
            return zipped(json.dumps(self.report))
        if path.endswith("/logs"):
            if self.unavailable:
                raise audit.HTTPFailure(404)
            return zipped(b"synthetic execution only", "job.txt")
        raise AssertionError("Unexpected synthetic API path")

    def pages(self, path, key):
        self.calls.append(path)
        if path == "/actions/runs":
            return iter(self.runs)
        if path == "/actions/runs/900/attempts/2/jobs":
            return iter(self.jobs)
        if path == "/actions/runs/900/artifacts":
            data = zipped(json.dumps(self.report))
            digest = hashlib.sha256(data).hexdigest()
            return iter([{**item, "digest": "sha256:" + ("0" * 64 if self.corrupt_digest else digest)}
                         for item in self.artifacts])
        if path.endswith("/jobs"):
            return iter([{"runner_id": 1, "steps": [{"status": "completed"}]}]
                        if self.executed else [{"runner_id": 0, "steps": []}])
        raise AssertionError("Unexpected synthetic page path")


class CoverageTests(unittest.TestCase):
    def run_audit(self, api, full=False):
        with patch.dict(os.environ, ENV):
            return audit.actions_logs(api, coverage.Coverage(api, full=full, policy=POLICY))

    def test_verified_default_receipt_reuses_old_attempt_but_scans_rerun(self):
        api = NativeFixture()
        report = self.run_audit(api)
        self.assertEqual(report["attempts_examined"], 2)
        self.assertEqual(report["attempts_reused"], 1)
        self.assertEqual(report["archives_scanned"], 1)
        self.assertEqual(report["archives_reused"], 1)
        self.assertIn("/actions/runs/100/attempts/2/logs", api.calls)
        self.assertNotIn("/actions/runs/100/attempts/1/logs", api.calls)
        self.assertEqual(len(report["coverage"]["attempts"]), 2)

    def test_pending_rerun_keeps_prior_completed_attempt_in_scope(self):
        api = NativeFixture()
        api.runs[0]["status"] = "in_progress"
        report = self.run_audit(api)
        self.assertEqual(report["attempts_examined"], 1)
        self.assertEqual(report["attempts_reused"], 1)
        self.assertEqual(report["pending_runs"], ["actions-run:100"])
        self.assertFalse(any(path.endswith("/logs") for path in api.calls))

    def test_new_run_is_scanned_even_when_all_old_attempts_are_covered(self):
        api = NativeFixture()
        api.runs[0]["run_attempt"] = 1
        api.runs.append({**api.runs[0], "id": 101})
        report = self.run_audit(api)
        self.assertEqual(report["archives_scanned"], 1)
        self.assertIn("/actions/runs/101/attempts/1/logs", api.calls)

    def test_changed_native_source_or_workflow_or_repository_rejects_reuse(self):
        for key, value in [("head_sha", DEFAULT), ("workflow_id", 99),
                           ("head_repository", {"id": 2})]:
            with self.subTest(key=key):
                api = NativeFixture()
                api.runs[0][key] = value
                with self.assertRaises(coverage.CoverageError):
                    self.run_audit(api)
                self.assertFalse(any(path.endswith("/logs") for path in api.calls))

    def test_unknown_repository_is_scanned_instead_of_reused(self):
        api = NativeFixture()
        api.runs[0]["head_repository"] = None
        api.report["coverage"]["attempts"][0]["head_repository_id"] = 0
        report = self.run_audit(api)
        self.assertEqual(report["attempts_reused"], 0)
        self.assertEqual(report["archives_scanned"], 2)

    def test_fork_pr_wrong_branch_or_event_receipts_never_authorize_reuse(self):
        for key, value in [
                ("event", "pull_request"), ("head_branch", "untrusted"),
                ("head_repository", {"id": 2, "full_name": "fork/repository"}),
                ("workflow_id", 9)]:
            with self.subTest(key=key):
                api = NativeFixture()
                api.candidate[key] = value
                report = self.run_audit(api)
                self.assertEqual(report["attempts_reused"], 0)
                self.assertEqual(report["archives_scanned"], 2)
                self.assertNotIn("/actions/artifacts/50/zip", api.calls)

    def test_nonancestor_default_source_cannot_authorize_reuse(self):
        api = NativeFixture()
        api.ancestry["status"] = "diverged"
        self.assertEqual(self.run_audit(api)["attempts_reused"], 0)

    def test_changed_policy_and_legacy_missing_helper_require_full_bootstrap(self):
        for missing in (False, True):
            with self.subTest(missing=missing):
                api = NativeFixture()
                if missing:
                    api.missing_file = "engineering/log_coverage.py"
                else:
                    api.files["engineering/secret_audit.py"] = b"old policy"
                report = self.run_audit(api)
                self.assertEqual(report["archives_scanned"], 2)
                self.assertIsNone(report["baseline"])
                self.assertNotIn("/actions/artifacts/50/zip", api.calls)

    def test_applicable_missing_expired_duplicate_or_misbinding_artifact_fails_closed(self):
        for change in ("missing", "expired", "duplicate", "source", "repository"):
            with self.subTest(change=change):
                api = NativeFixture()
                if change == "missing":
                    api.artifacts = []
                elif change == "expired":
                    api.artifacts[0]["expired"] = True
                elif change == "duplicate":
                    api.artifacts.append(copy.deepcopy(api.artifacts[0]))
                elif change == "source":
                    api.artifacts[0]["workflow_run"]["head_sha"] = DEFAULT
                else:
                    api.artifacts[0]["workflow_run"]["head_repository_id"] = 2
                with self.assertRaises(coverage.CoverageError):
                    self.run_audit(api)
                self.assertFalse(any(path.endswith("/logs") for path in api.calls))

    def test_failed_required_job_cannot_certify_a_receipt(self):
        api = NativeFixture()
        api.jobs[0]["conclusion"] = "failure"
        with self.assertRaises(coverage.CoverageError):
            self.run_audit(api)
        self.assertNotIn("/actions/artifacts/50/zip", api.calls)

    def test_native_zip_digest_must_match(self):
        api = NativeFixture()
        api.corrupt_digest = True
        with self.assertRaises(coverage.CoverageError):
            self.run_audit(api)

    def test_producer_attempt_and_source_are_exact(self):
        for key, value in [("attempt", 1), ("head_sha", DEFAULT),
                           ("run_id", 899), ("workflow_id", 99)]:
            with self.subTest(key=key):
                api = NativeFixture()
                api.report["coverage"]["producer"][key] = value
                with self.assertRaises(coverage.CoverageError):
                    self.run_audit(api)

    def test_duplicates_aggregate_only_findings_and_counter_conflicts_are_rejected(self):
        for change in ("duplicate", "aggregate", "findings", "counter", "boolean"):
            with self.subTest(change=change):
                api = NativeFixture()
                if change == "duplicate":
                    api.report["coverage"]["attempts"] *= 2
                elif change == "aggregate":
                    api.report.pop("coverage")
                    api.report["schema"] = 1
                elif change == "findings":
                    api.report["status"] = "findings"
                    api.report["finding_count"] = 1
                elif change == "counter":
                    api.report["members_scanned"] = 99
                else:
                    api.report["archives_scanned"] = True
                with self.assertRaises(coverage.CoverageError):
                    self.run_audit(api)

    def test_explicit_full_scan_ignores_receipts_and_rescans_real_attempts(self):
        api = NativeFixture()
        api.artifacts = []
        report = self.run_audit(api, full=True)
        self.assertEqual(report["attempts_reused"], 0)
        self.assertEqual(report["archives_scanned"], 2)
        self.assertTrue(report["full_sweep"])
        self.assertFalse(any("/artifacts" in path for path in api.calls))

    def test_unavailable_new_executed_logs_are_a_gap_but_no_runner_is_distinct(self):
        for executed in (True, False):
            with self.subTest(executed=executed):
                api = NativeFixture()
                api.unavailable, api.executed = True, executed
                report = self.run_audit(api)
                self.assertEqual(report["coverage_gaps"],
                                 ["actions-run:100/attempt:2"] if executed else [])
                self.assertEqual(report["attempts_without_execution"], 0 if executed else 1)

    def test_zip_member_name_and_duplicate_json_keys_cannot_smuggle_proof(self):
        for payload, name in [
                (b"{}", "other.json"),
                (b'{"schema":2,"schema":1}', "actions-audit.json")]:
            with self.subTest(name=name):
                data = zipped(payload, name)
                with self.assertRaises(coverage.CoverageError):
                    coverage.receipt_json(data, "sha256:" + hashlib.sha256(data).hexdigest())
        data = zipped(b"{}")
        with patch.object(coverage, "MAX_RECEIPT", 1):
            with self.assertRaises(coverage.CoverageError):
                coverage.receipt_json(data, "sha256:" + hashlib.sha256(data).hexdigest())

    def test_receipt_counter_capacity_never_queues_an_uncovered_inventory(self):
        api = NativeFixture()
        api.runs[0]["run_attempt"] = 100
        # Reserve is enforced after small metadata discovery, before log batches.
        api.budget.observe({"X-RateLimit-Remaining": str(audit.API_RESERVE + 30)})
        with self.assertRaises(audit.BudgetFailure):
            self.run_audit(api)
        self.assertFalse(any(path.endswith("/logs") for path in api.calls))


if __name__ == "__main__":
    unittest.main()

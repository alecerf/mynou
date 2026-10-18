"""CI-only native-provenance, receipt, window and bounded-batch coverage scenarios."""
import base64
import copy
from datetime import datetime, timedelta, timezone
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
SOURCE = (b'"""Synthetic scanner revision."""\nSCANNER_VERSION = 1\n'
          b'RULES = {"synthetic-rule": rb"synthetic"}\n')
POLICY = coverage.policy_from_source(SOURCE)
T0 = "2026-10-09T12:00:00Z"
NOW = datetime(2026, 10, 9, 13, 0, tzinfo=timezone.utc)
DEEP_NOW = datetime(2026, 10, 10, 9, 0, tzinfo=timezone.utc)


def zipped(payload, name="actions-audit.json"):
    stream = io.BytesIO()
    with zipfile.ZipFile(stream, "w") as archive:
        archive.writestr(zipfile.ZipInfo(name, date_time=(2026, 1, 1, 0, 0, 0)), payload)
    return stream.getvalue()


def entry(run_id, attempt, state="scanned"):
    return {"run_id": run_id, "attempt": attempt, "head_sha": MEDIA, "workflow_id": 8,
            "head_repository_id": 1, "state": state, "members": 1 if state == "scanned" else 0}


def run(run_id, attempt=1, created="2026-10-09T11:00:00Z", status="completed", conclusion="success"):
    return {"id": run_id, "workflow_id": 8, "head_sha": MEDIA, "head_repository": {"id": 1},
            "status": status, "conclusion": conclusion, "run_attempt": attempt, "created_at": created}


def receipt_fixture(rows=None, complete=True, deep=True, deep_at=T0,
                    since="2026-09-07T12:00:00Z", deferred=0):
    producer = {"run_id": 900, "attempt": 2, "head_sha": HEAD, "workflow_id": 7}
    rows = [entry(100, 1)] if rows is None else rows
    return {"schema": 3, "status": "passed" if complete else "partial", "finding_count": 0,
            "findings": [], "coverage_gaps": [], "attempts_deferred": deferred,
            "attempts_recorded": len(rows),
            "coverage": {"schema": 2, "repository_id": 1, "policy": POLICY,
                         "producer": producer, "inventory_at": T0, "since": since,
                         "deep": deep, "deep_at": deep_at, "complete": complete,
                         "attempts": rows}}


class NativeFixture:
    repository = "owner/repository"

    def __init__(self):
        self.budget = audit.RequestBudget()
        self.calls = []
        self.report = receipt_fixture()
        self.source = SOURCE
        self.missing_source = False
        self.ancestry = {"status": "ahead", "merge_base_commit": {"sha": HEAD}}
        self.candidate = {
            "id": 900, "workflow_id": 7, "head_branch": "trunk", "head_sha": HEAD,
            "head_repository": {"id": 1, "full_name": self.repository},
            "event": "push", "status": "completed", "conclusion": "success", "run_attempt": 2}
        self.runs = [run(100, attempt=2)]
        self.listed = 0
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
            if path[len("/contents/"):].split("?ref=", 1)[0] != coverage.SCANNER:
                raise AssertionError("Only the scanner source defines the policy")
            if self.missing_source:
                raise audit.HTTPFailure(404)
            return {"type": "file", "encoding": "base64", "size": len(self.source),
                    "content": base64.b64encode(self.source).decode()}
        if path == "/actions/artifacts/50/zip":
            return zipped(json.dumps(self.report))
        if path.endswith("/logs"):
            if self.unavailable:
                raise audit.HTTPFailure(404)
            return zipped(b"synthetic execution only", "job.txt")
        raise AssertionError("Unexpected synthetic API path")

    def listing(self):
        for item in self.runs:
            self.listed += 1
            yield item

    def pages(self, path, key):
        self.calls.append(path)
        if path == "/actions/runs":
            return self.listing()
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


def recorded(report):
    return {(row["run_id"], row["attempt"]): row for row in report["coverage"]["attempts"]}


class CoverageTests(unittest.TestCase):
    def run_audit(self, api, full=False, now=NOW):
        with patch.dict(os.environ, ENV):
            return audit.actions_logs(api, coverage.Coverage(api, full=full, policy=POLICY), now=now)

    def test_verified_default_receipt_reuses_old_attempt_but_scans_rerun(self):
        api = NativeFixture()
        report = self.run_audit(api)
        self.assertEqual(report["attempts_examined"], 2)
        self.assertEqual(report["attempts_reused"], 1)
        self.assertEqual(report["archives_scanned"], 1)
        self.assertIn("/actions/runs/100/attempts/2/logs", api.calls)
        self.assertNotIn("/actions/runs/100/attempts/1/logs", api.calls)
        self.assertEqual(len(report["coverage"]["attempts"]), 2)
        self.assertEqual(report["attempts_recorded"], 2)
        self.assertTrue(report["coverage"]["complete"])

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

    def test_runs_repeated_by_shifting_pages_are_listed_once(self):
        api = NativeFixture()
        api.runs = [run(101), run(100, attempt=2), run(100, attempt=2)]
        report = self.run_audit(api)
        self.assertEqual(report["runs_enumerated"], 2)
        self.assertEqual(report["attempts_examined"], 3)
        self.assertEqual(report["archives_scanned"], 2)

    def test_skipped_runs_record_no_runner_execution_without_a_log_request(self):
        api = NativeFixture()
        api.runs.append(run(101, conclusion="skipped"))
        report = self.run_audit(api)
        self.assertNotIn("/actions/runs/101/attempts/1/logs", api.calls)
        self.assertEqual(recorded(report)[(101, 1)]["state"], "no-runner-execution")
        self.assertEqual(report["attempts_without_execution"], 1)

    def test_routine_audit_stops_after_a_page_of_older_runs_and_carries_evidence(self):
        api = NativeFixture()
        api.report = receipt_fixture([entry(50, 1), entry(100, 1)])
        api.runs += [run(2000 + n, created="2026-10-08T00:00:00Z") for n in range(150)]
        report = self.run_audit(api)
        self.assertFalse(report["deep"])
        self.assertEqual(report["since"], "2026-10-09T06:00:00Z")
        self.assertEqual(api.listed, 101)
        self.assertEqual(report["runs_enumerated"], 1)
        self.assertEqual(report["attempts_carried"], 1)
        self.assertIn((50, 1), recorded(report))
        self.assertEqual(report["coverage"]["deep_at"], T0)

    def test_deep_audit_relists_the_rerun_window_and_drops_settled_evidence(self):
        api = NativeFixture()
        api.report = receipt_fixture([entry(50, 1), entry(100, 1)])
        api.runs.append(run(50, created="2026-08-01T00:00:00Z"))
        report = self.run_audit(api, now=DEEP_NOW)
        self.assertTrue(report["deep"])
        self.assertEqual(report["since"], "2026-09-07T12:00:00Z")
        self.assertEqual(report["attempts_carried"], 0)
        self.assertNotIn((50, 1), recorded(report))
        self.assertEqual(report["coverage"]["deep_at"], "2026-10-10T09:00:00Z")

    def test_bounded_batch_scans_newest_attempts_and_defers_the_rest(self):
        api = NativeFixture()
        api.runs = [run(102), run(101), run(100, attempt=2)]
        with patch.object(audit, "MAX_SCANS", 1):
            report = self.run_audit(api)
        self.assertEqual(report["archives_scanned"], 1)
        self.assertIn("/actions/runs/102/attempts/1/logs", api.calls)
        self.assertNotIn("/actions/runs/101/attempts/1/logs", api.calls)
        self.assertEqual(report["attempts_deferred"], 2)
        self.assertFalse(report["coverage"]["complete"])
        self.assertEqual(report["coverage"]["deep_at"], T0)

    def test_partial_receipt_is_reused_and_its_deep_scope_continues(self):
        api = NativeFixture()
        api.report = receipt_fixture(complete=False, deep_at=None, since=None, deferred=1)
        report = self.run_audit(api)
        self.assertTrue(report["deep"])
        self.assertIsNone(report["since"])
        self.assertEqual(report["attempts_reused"], 1)
        self.assertEqual(report["coverage"]["deep_at"], "2026-10-09T13:00:00Z")

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

    def test_plumbing_edit_keeps_default_receipts_reusable(self):
        api = NativeFixture()
        api.source = SOURCE + b"\n# Refactored plumbing keeps the detection policy.\n"
        report = self.run_audit(api)
        self.assertEqual(report["attempts_reused"], 1)
        self.assertIsNotNone(report["baseline"])

    def test_changed_rules_version_or_legacy_scanner_require_full_bootstrap(self):
        for change in ("rules", "version", "legacy", "missing"):
            with self.subTest(change=change):
                api = NativeFixture()
                if change == "rules":
                    api.source = SOURCE.replace(b'rb"synthetic"', b'rb"different"')
                elif change == "version":
                    api.source = SOURCE.replace(b"SCANNER_VERSION = 1", b"SCANNER_VERSION = 2")
                elif change == "legacy":
                    api.source = b'RULES = {"synthetic-rule": rb"synthetic"}\n'
                else:
                    api.missing_source = True
                report = self.run_audit(api)
                self.assertEqual(report["archives_scanned"], 2)
                self.assertIsNone(report["baseline"])
                self.assertTrue(report["deep"])
                self.assertNotIn("/actions/artifacts/50/zip", api.calls)

    def test_scanner_policy_ignores_plumbing_but_tracks_literal_rules(self):
        self.assertEqual(coverage.local_policy(),
                         coverage.policy_digest(audit.RULES, audit.SCANNER_VERSION))
        self.assertEqual(coverage.policy_from_source(SOURCE + b"# comment\n"), POLICY)
        for source in [SOURCE.replace(b"SCANNER_VERSION = 1", b"SCANNER_VERSION = 2"),
                       SOURCE.replace(b'rb"synthetic"', b'rb"different"')]:
            self.assertNotIn(coverage.policy_from_source(source), (None, POLICY))
        for source in [b'RULES = {"synthetic-rule": rb"synthetic"}\n',
                       SOURCE + b"RULES = {}\n",
                       b"SCANNER_VERSION = 1\nRULES = dict()\n",
                       b"SCANNER_VERSION = True\nRULES = {'synthetic-rule': rb'x'}\n",
                       b"SCANNER_VERSION = 1\nRULES = {'Upper Case': rb'x'}\n",
                       b"not python (", "text".encode("utf-16")]:
            with self.subTest(source=source[:20]):
                self.assertIsNone(coverage.policy_from_source(source))

    def test_expired_receipt_rebuilds_coverage_instead_of_failing_forever(self):
        for field, value in [("expired", True), ("expires_at", "2020-01-01T00:00:00Z")]:
            with self.subTest(field=field):
                api = NativeFixture()
                api.artifacts[0][field] = value
                report = self.run_audit(api)
                self.assertIsNone(report["baseline"])
                self.assertEqual(report["archives_scanned"], 2)
                self.assertNotIn("/actions/artifacts/50/zip", api.calls)

    def test_applicable_missing_duplicate_or_misbinding_artifact_fails_closed(self):
        for change in ("missing", "duplicate", "source", "repository", "expiry"):
            with self.subTest(change=change):
                api = NativeFixture()
                if change == "missing":
                    api.artifacts = []
                elif change == "duplicate":
                    api.artifacts.append(copy.deepcopy(api.artifacts[0]))
                elif change == "source":
                    api.artifacts[0]["workflow_run"]["head_sha"] = DEFAULT
                elif change == "repository":
                    api.artifacts[0]["workflow_run"]["head_repository_id"] = 2
                else:
                    api.artifacts[0]["expired"] = "no"
                with self.assertRaises(coverage.CoverageError):
                    self.run_audit(api)
                self.assertFalse(any(path.endswith("/logs") for path in api.calls))

    def test_failed_log_job_cannot_certify_but_scheduled_audits_need_no_history_job(self):
        api = NativeFixture()
        api.jobs[1]["conclusion"] = "failure"
        with self.assertRaises(coverage.CoverageError):
            self.run_audit(api)
        self.assertNotIn("/actions/artifacts/50/zip", api.calls)
        api = NativeFixture()
        api.candidate["event"] = "schedule"
        api.jobs = api.jobs[1:]
        self.assertEqual(self.run_audit(api)["attempts_reused"], 1)

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

    def test_duplicates_legacy_findings_and_inconsistent_receipts_are_rejected(self):
        for change in ("duplicate", "legacy", "findings", "counter", "boolean",
                       "complete", "deferred", "deep_at", "window", "state"):
            with self.subTest(change=change):
                api = NativeFixture()
                proof = api.report["coverage"]
                if change == "duplicate":
                    proof["attempts"] *= 2
                    api.report["attempts_recorded"] = 2
                elif change == "legacy":
                    api.report.pop("coverage")
                    api.report["schema"] = 2
                elif change == "findings":
                    api.report["status"] = "findings"
                    api.report["finding_count"] = 1
                elif change == "counter":
                    api.report["attempts_recorded"] = 99
                elif change == "boolean":
                    api.report["attempts_recorded"] = True
                elif change == "complete":
                    api.report["status"] = "partial"
                elif change == "deferred":
                    api.report["attempts_deferred"] = 1
                elif change == "deep_at":
                    proof["deep_at"] = "2026-10-09T11:00:00Z"
                elif change == "window":
                    proof["since"] = "2026-10-10T00:00:00Z"
                else:
                    proof["attempts"][0]["state"] = "unavailable"
                with self.assertRaises(coverage.CoverageError):
                    self.run_audit(api)

    def test_explicit_full_scan_ignores_receipts_and_rescans_real_attempts(self):
        api = NativeFixture()
        api.artifacts = []
        report = self.run_audit(api, full=True)
        self.assertEqual(report["attempts_reused"], 0)
        self.assertEqual(report["archives_scanned"], 2)
        self.assertTrue(report["full_sweep"])
        self.assertTrue(report["deep"])
        self.assertEqual(report["coverage"]["deep_at"], "2026-10-09T13:00:00Z")
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
                self.assertEqual(report["coverage"]["complete"], not executed)

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

    def test_observed_capacity_bounds_the_batch_and_keeps_the_reserve(self):
        api = NativeFixture()
        api.runs[0]["run_attempt"] = 100
        # Seven metadata requests leave 23 spare requests; half of them are scanned.
        api.budget.observe({"X-RateLimit-Remaining": str(audit.API_RESERVE + 30)})
        report = self.run_audit(api)
        self.assertEqual(report["archives_scanned"], 11)
        self.assertEqual(report["attempts_deferred"], 88)
        self.assertGreaterEqual(api.budget.evidence()["observed_rate"]["remaining"],
                                audit.API_RESERVE)

    def test_exhausted_capacity_fails_before_any_log_request(self):
        api = NativeFixture()
        api.runs[0]["run_attempt"] = 100
        api.budget.observe({"X-RateLimit-Remaining": str(audit.API_RESERVE + 8)})
        with self.assertRaises(audit.BudgetFailure):
            self.run_audit(api)
        self.assertFalse(any(path.endswith("/logs") for path in api.calls))


if __name__ == "__main__":
    unittest.main()

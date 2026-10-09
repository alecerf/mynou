"""CI-only audit redaction, archive bounds and coverage regressions."""
import io
import json
from pathlib import Path
import sys
import tempfile
from threading import Lock
import unittest
from unittest.mock import patch
import zipfile

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import secret_audit as audit


class SecretAuditTests(unittest.TestCase):
    def archive(self, text):
        stream = io.BytesIO()
        with zipfile.ZipFile(stream, "w") as output:
            output.writestr("untrusted/path.txt", text)
        return stream.getvalue()

    def test_provider_findings_never_publish_value_or_preview(self):
        value = b"ghp_" + b"A" * 36
        report = audit.findings(b"prefix " + value + b" suffix", "git-object:" + "a" * 40)
        self.assertEqual(report, [{"rule": "github-token", "location": "git-object:" + "a" * 40}])
        self.assertNotIn(value.decode(), json.dumps(report))
        self.assertEqual(audit.findings(b"GH_TOKEN=environment-placeholder", "fixture"), [])

    def test_private_key_marker_is_redacted(self):
        marker = b"-----BEGIN " + b"PRIVATE KEY-----"
        self.assertEqual(audit.findings(marker, "fixture")[0]["rule"], "private-key")

    def test_archive_content_and_filename_are_not_reported(self):
        value = b"dckr_pat_" + b"B" * 40
        count, report = audit.scan_archive(self.archive(value), "actions-run:1/attempt:1")
        self.assertEqual(count, 1)
        self.assertEqual(report[0]["rule"], "docker-token")
        self.assertNotIn(value.decode(), json.dumps(report))
        self.assertNotIn("untrusted/path.txt", json.dumps(report))

    def test_archive_expansion_limit_fails_closed(self):
        original = audit.MAX_EXPANDED
        try:
            audit.MAX_EXPANDED = 1
            with self.assertRaises(audit.AuditError):
                audit.scan_archive(self.archive(b"bounded"), "fixture")
        finally:
            audit.MAX_EXPANDED = original

    def test_redirect_must_be_approved_https_storage(self):
        value = "https://example.blob.core.windows.net/log"
        self.assertEqual(audit.archive_url(value), value)
        for value in ["http://example.blob.core.windows.net/log", "https://attacker.example/log",
                      "https://api.github.com@attacker.example/log", "https://token@api.github.com/log"]:
            with self.assertRaises(audit.AuditError):
                audit.archive_url(value)

    def test_unavailable_executed_logs_remain_a_coverage_gap(self):
        class Fake:
            def get(self, path, archive=False):
                raise audit.HTTPFailure(404)
            def pages(self, path, key):
                return iter([{"runner_id": 1, "steps": [{"status": "completed"}]}])
        result = audit.audit_attempt(Fake(), (1, 2))
        self.assertEqual(result["state"], "unavailable")
        self.assertEqual(result["location"], "actions-run:1/attempt:2")

    def test_no_runner_attempt_does_not_claim_scanned_logs(self):
        class Fake:
            def get(self, path, archive=False):
                raise audit.HTTPFailure(404)
            def pages(self, path, key):
                return iter([{"runner_id": 0, "steps": []}])
        self.assertEqual(audit.audit_attempt(Fake(), (1, 1))["state"], "no-runner-execution")

    def test_retention_deleted_logs_are_expired_not_a_gap(self):
        class Fake:
            def get(self, path, archive=False):
                raise audit.HTTPFailure(410)
            def pages(self, path, key):
                return iter([{"runner_id": 1, "steps": [{"status": "completed"}]}])
        self.assertEqual(audit.audit_attempt(Fake(), (1, 3)),
                         {"state": "expired", "location": "actions-run:1/attempt:3"})

    def test_inventory_ignores_runs_repeated_by_shifted_pages(self):
        class Fake:
            def pages(self, path, key):
                return iter([{"id": 3}, {"id": 2}, {"id": 2}, {"id": 1}])
        self.assertEqual([run["id"] for run in audit.inventory(Fake(), None)], [3, 2, 1])
        class Hostile:
            def pages(self, path, key):
                return iter([{"id": True}])
        with self.assertRaises(audit.AuditError):
            audit.inventory(Hostile(), None)



class RequestBudgetTests(unittest.TestCase):
    def test_only_allowlisted_provider_counters_are_emitted(self):
        hostile = "ghp_" + "Z" * 36
        observed = audit.rate_metadata({
            "X-RateLimit-Limit": "1000", "X-RateLimit-Remaining": "0",
            "X-RateLimit-Used": "1000", "X-RateLimit-Reset": "1791547200",
            "Retry-After": "60", "X-RateLimit-Resource": "core",
            "Authorization": hostile, "Location": "https://example.invalid/" + hostile,
            "X-Request-Id": hostile,
        })
        self.assertEqual(observed, {"limit": 1000, "remaining": 0, "used": 1000,
                                   "reset_epoch": 1791547200,
                                   "retry_after_seconds": 60, "resource": "core"})
        self.assertNotIn(hostile, json.dumps(observed))
        self.assertNotIn("Location", observed)

    def test_hostile_numeric_or_resource_headers_are_omitted(self):
        self.assertEqual(audit.rate_metadata({
            "X-RateLimit-Remaining": "-1", "X-RateLimit-Limit": "9" * 100,
            "X-RateLimit-Used": "1" + chr(10) + "credential", "X-RateLimit-Reset": "nan",
            "Retry-After": "tomorrow", "X-RateLimit-Resource": "user-controlled",
        }), {})

    def test_preflight_preserves_observed_reserve_without_an_api_call(self):
        budget = audit.RequestBudget()
        budget.observe({"X-RateLimit-Remaining": "1000"})
        with self.assertRaises(audit.BudgetFailure):
            budget.require(1260)
        self.assertEqual(budget.evidence()["requests"], 0)
        self.assertEqual(budget.evidence()["observed_rate"]["remaining"], 1000)
        budget.require(800)

    def test_concurrent_reservations_cannot_cross_the_reserve(self):
        budget = audit.RequestBudget()
        budget.observe({"X-RateLimit-Remaining": str(audit.API_RESERVE + 4)})
        with audit.ThreadPoolExecutor(max_workers=4) as pool:
            list(pool.map(lambda _: budget.begin(), range(4)))
        self.assertEqual(budget.evidence()["requests"], 4)
        with self.assertRaises(audit.BudgetFailure):
            budget.begin()
        self.assertEqual(budget.evidence()["observed_rate"]["remaining"], audit.API_RESERVE)

    def test_out_of_order_headers_do_not_restore_consumed_capacity(self):
        budget = audit.RequestBudget()
        budget.observe({"X-RateLimit-Remaining": "500"})
        budget.begin()
        budget.observe({"X-RateLimit-Remaining": "510"})
        self.assertEqual(budget.evidence()["observed_rate"]["remaining"], 499)
        budget.observe({"X-RateLimit-Remaining": "300"})
        self.assertEqual(budget.evidence()["observed_rate"]["remaining"], 300)

    def test_capacity_is_the_tighter_of_ceiling_and_observed_reserve(self):
        budget = audit.RequestBudget()
        self.assertEqual(budget.capacity(), audit.MAX_API_REQUESTS)
        budget.begin()
        self.assertEqual(budget.capacity(), audit.MAX_API_REQUESTS - 1)
        budget.observe({"X-RateLimit-Remaining": str(audit.API_RESERVE + 7)})
        self.assertEqual(budget.capacity(), 7)
        budget.observe({"X-RateLimit-Remaining": "0"})
        self.assertEqual(budget.capacity(), 0)

    def test_missing_headers_still_have_a_hard_request_ceiling(self):
        budget = audit.RequestBudget()
        with patch.object(audit, "MAX_API_REQUESTS", 2):
            budget.begin()
            budget.begin()
            with self.assertRaises(audit.BudgetFailure):
                budget.begin()
        self.assertEqual(budget.evidence()["requests"], 2)

    def test_first_failed_batch_never_submits_the_remaining_inventory(self):
        class Fake:
            def __init__(self):
                self.budget = audit.RequestBudget()
                self.calls = []
                self.lock = Lock()
            def pages(self, path, key):
                return iter([{"id": n, "status": "completed", "run_attempt": 1}
                             for n in range(1, 21)])
            def get(self, path, archive=False):
                with self.lock:
                    self.calls.append(path)
                raise audit.HTTPFailure(403)
        api = Fake()
        with self.assertRaises(audit.HTTPFailure):
            audit.actions_logs(api)
        self.assertGreater(len(api.calls), 0)
        self.assertLessEqual(len(api.calls), 4)

    def test_safe_capacity_error_is_written_without_raw_exception_data(self):
        class Fake:
            def __init__(self, repository, token):
                self.budget = audit.RequestBudget()
            def get(self, path, archive=False, limit=None):
                # The first native request (receipt discovery) is refused.
                self.budget.begin()
                self.budget.observe({"X-RateLimit-Limit": "1000",
                                     "X-RateLimit-Remaining": "0"})
                raise audit.HTTPFailure(403)
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "audit.json"
            stdout = io.StringIO()
            with patch.object(audit, "API", Fake), patch.object(
                    sys, "argv", ["audit", "logs", "--output", str(output)]), patch.dict(
                    audit.os.environ, {"GH_TOKEN": "synthetic-only",
                                       "GITHUB_REPOSITORY": "owner/repository"}), patch(
                    "sys.stdout", stdout):
                self.assertEqual(audit.main(), 1)
            result = json.loads(output.read_text())
            self.assertEqual(result["status"], "error")
            self.assertEqual(result["http_status"], 403)
            self.assertEqual(result["api_budget"]["requests"], 1)
            self.assertEqual(result["api_budget"]["observed_rate"]["remaining"], 0)
            self.assertNotIn("synthetic-only", stdout.getvalue())
            self.assertNotIn("owner/repository", stdout.getvalue())

if __name__ == "__main__":
    unittest.main()

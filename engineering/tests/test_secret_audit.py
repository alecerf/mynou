"""CI-only audit redaction, archive bounds and coverage regressions."""
import io
import json
from pathlib import Path
import sys
import unittest
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


if __name__ == "__main__":
    unittest.main()

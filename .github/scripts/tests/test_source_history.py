"""Source scanning keeps findings redacted and includes committed Git history."""
from contextlib import redirect_stdout
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import source_history as audit


class SourceHistoryTests(unittest.TestCase):
    def test_provider_findings_never_publish_value_or_preview(self):
        value = b"ghp_" + b"A" * 36
        report = audit.findings(b"prefix " + value + b" suffix", "git-object:" + "a" * 40)
        self.assertEqual(report, [{"rule": "github-token", "location": "git-object:" + "a" * 40}])
        self.assertNotIn(value.decode(), json.dumps(report))
        self.assertEqual(audit.findings(b"GH_TOKEN=environment-placeholder", "fixture"), [])

    def test_private_key_marker_is_redacted(self):
        marker = b"-----BEGIN " + b"PRIVATE KEY-----"
        self.assertEqual(audit.findings(marker, "fixture")[0]["rule"], "private-key")

    def test_deleted_committed_credential_remains_in_scope(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            def git(*args):
                return subprocess.run(
                    ['git', '-C', directory, '-c', 'commit.gpgsign=false',
                     '-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid',
                     *args], check=True, capture_output=True)
            git('init')
            value = b'ghp_' + b'Z' * 36
            fixture = root / 'fixture.txt'
            fixture.write_bytes(value)
            git('add', 'fixture.txt')
            git('commit', '-m', 'Synthetic fixture')
            fixture.write_bytes(b'removed')
            git('add', 'fixture.txt')
            git('commit', '-m', 'Remove synthetic value')
            previous = Path.cwd()
            try:
                os.chdir(directory)
                report = audit.git_history()
            finally:
                os.chdir(previous)
            self.assertGreater(report['objects_scanned'], 0)
            self.assertEqual(report['finding_count'], 1)
            self.assertEqual(report['findings'][0]['rule'], 'github-token')
            self.assertNotIn(value.decode(), json.dumps(report))

    def test_failure_exits_nonzero_without_disclosing_exception(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / 'report.json'
            stream = io.StringIO()
            with patch.object(sys, 'argv', ['source_history.py', '--output', str(output)]), \
                 patch.object(audit, 'git_history', side_effect=RuntimeError('sensitive detail')), \
                 redirect_stdout(stream):
                self.assertEqual(audit.main(), 1)
            report = json.loads(output.read_text())
            self.assertEqual(report['status'], 'error')
            self.assertEqual(report['error_type'], 'RuntimeError')
            self.assertNotIn('sensitive detail', stream.getvalue() + output.read_text())


if __name__ == '__main__':
    unittest.main()

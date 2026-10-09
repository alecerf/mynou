"""Bounded REST client transport and credential boundaries; CI only."""
import io
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch
import urllib.error
import urllib.request

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import github
from github import APIError, GitHub, NoRedirect

CFG = {"repository": "original/mynou", "default_branch": "trunk"}


class Transport(unittest.TestCase):
    def client(self, opener):
        with patch.dict(os.environ, {"GH_TOKEN": "original-opaque-fixture-credential"}):
            client = GitHub(CFG)
        client.opener = opener
        return client

    def test_repository_routes_carry_the_token_and_write_payloads(self):
        requests = []
        class Opener:
            def open(self, request, timeout):
                requests.append(request)
                return io.BytesIO(b'{"accepted": true}')
        client = self.client(Opener())
        self.assertEqual(client.rest("GET", ""), {"accepted": True})
        self.assertEqual(client.rest("POST", "issues/1/comments", {"body": "/assign codex-1"}), {"accepted": True})
        self.assertEqual(requests[0].full_url, "https://api.github.com/repos/original/mynou")
        self.assertEqual(requests[0].get_header("Authorization"), "Bearer original-opaque-fixture-credential")
        self.assertEqual(requests[0].get_header("Cache-control"), "no-cache")
        self.assertEqual(requests[1].full_url, "https://api.github.com/repos/original/mynou/issues/1/comments")
        self.assertEqual(json.loads(requests[1].data), {"body": "/assign codex-1"})
        self.assertEqual(client.calls, 2)

    def test_commit_addressed_reads_are_fetched_once_but_mutable_state_is_reread(self):
        requests = []
        class Opener:
            def open(self, request, timeout):
                requests.append(request.full_url)
                return io.BytesIO(json.dumps({"sha": "a" * 40}).encode())
        client = self.client(Opener())
        first = client.rest("GET", "git/commits/" + "a" * 40)
        first["sha"] = "mutated by a caller"
        self.assertEqual(client.rest("GET", "git/commits/" + "a" * 40), {"sha": "a" * 40})
        for path in ["compare/" + "a" * 40 + "..." + "b" * 40, "contents/Cargo.toml?ref=" + "a" * 40,
                     "issues/1/comments", "contents/Cargo.toml?ref=trunk"]:
            client.rest("GET", path)
            client.rest("GET", path)
        self.assertEqual(len(requests), 7)
        self.assertEqual(client.calls, 7)

    def test_errors_do_not_echo_responses_and_redirects_are_refused(self):
        class Opener:
            def open(self, request, timeout):
                raise urllib.error.HTTPError(request.full_url, 403, "private original response", {}, None)
        with self.assertRaises(APIError) as error:
            self.client(Opener()).rest("GET", "issues/1/comments")
        self.assertEqual(error.exception.status, 403)
        self.assertNotIn("private", str(error.exception))
        request = urllib.request.Request("https://api.github.com/repos/original/mynou")
        with self.assertRaises(APIError) as redirect:
            NoRedirect().redirect_request(request, None, 302, "Found", {}, "https://elsewhere.invalid/")
        self.assertEqual(redirect.exception.status, 302)

    def test_request_budget_and_response_size_are_bounded(self):
        class Opener:
            def open(self, request, timeout):
                return io.BytesIO(b"x" * (github.MAX_RESPONSE + 1))
        with self.assertRaises(ValueError):
            self.client(Opener()).rest("GET", "issues")
        client = self.client(Opener())
        client.calls = github.MAX_REQUESTS
        with self.assertRaises(ValueError):
            client.rest("GET", "issues")
        with self.assertRaises(ValueError):
            client.request("GET", "repos/original/mynou")

    def test_configuration_requires_the_comment_protocol_schema_and_a_token(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / ".github").mkdir()
            for value in [{"schema": 1, "repository": "original/mynou"}, {"schema": 2, "repository": "not a repo"}]:
                (root / ".github/engineering.json").write_text(json.dumps(value))
                with self.subTest(value=value), self.assertRaises(ValueError):
                    github.config(root)
            (root / ".github/engineering.json").write_text(json.dumps(dict(CFG, schema=2)))
            self.assertEqual(github.config(root)["repository"], "original/mynou")
        with patch.dict(os.environ, {}, clear=True), self.assertRaises(ValueError):
            GitHub(CFG)


if __name__ == "__main__":
    unittest.main()

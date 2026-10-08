"""Original bounded artifact transport and credential boundaries; CI only."""
import io
import json
import os
from pathlib import Path
import sys
import unittest
from unittest.mock import patch
import urllib.error

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from github import APIError, GitHub, artifact_destination
from test_organization import CFG


class ArtifactTransport(unittest.TestCase):
    def client(self):
        with patch.dict(os.environ, {"GH_TOKEN": "original-opaque-fixture-credential"}):
            return GitHub(CFG)

    def test_repository_identity_uses_the_canonical_native_route(self):
        client = self.client()
        identity = {"id": 200, "full_name": "original/mynou", "private": False}
        requests = []
        class Opener:
            def open(self, request, timeout):
                requests.append(request)
                if request.full_url != "https://api.github.com/repos/original/mynou":
                    raise urllib.error.HTTPError(request.full_url, 404, "Original native route fixture", {}, None)
                return io.BytesIO(json.dumps(identity).encode())
        client.opener = Opener()
        self.assertEqual(client.rest("GET", ""), identity)
        self.assertEqual(client.calls, 1)
        self.assertEqual(requests[0].get_header("Authorization"), "Bearer original-opaque-fixture-credential")

    def test_child_queries_and_write_payloads_retain_the_native_transport_contract(self):
        client = self.client()
        requests = []
        routes = {
            "https://api.github.com/repos/original/mynou/actions/runs/40/jobs?filter=latest": "GET",
            "https://api.github.com/repos/original/mynou/issues/1/comments": "POST",
        }
        class Opener:
            def open(self, request, timeout):
                requests.append(request)
                if routes.get(request.full_url) != request.get_method():
                    raise urllib.error.HTTPError(request.full_url, 404, "Original native route fixture", {}, None)
                return io.BytesIO(b'{"accepted": true}')
        client.opener = Opener()
        self.assertEqual(client.rest("GET", "actions/runs/40/jobs?filter=latest"), {"accepted": True})
        payload = {"body": "Original offline native comment fixture"}
        self.assertEqual(client.rest("POST", "issues/1/comments", payload), {"accepted": True})
        self.assertEqual(json.loads(requests[1].data), payload)
        self.assertEqual(client.calls, 2)

    def test_signed_storage_receives_no_github_auth_and_both_requests_count(self):
        destination = "https://original.blob.core.windows.net/proof?synthetic=signature"
        client = self.client()
        requests = []
        class Opener:
            def open(self, request, timeout):
                requests.append(request)
                if len(requests) == 1:
                    raise urllib.error.HTTPError(request.full_url, 302, "Original redirect", {"Location": destination}, None)
                return io.BytesIO(b"original archive bytes")
        with patch("urllib.request.build_opener", return_value=Opener()):
            self.assertEqual(client.publication_archive(50), b"original archive bytes")
        self.assertEqual(requests[0].get_header("Authorization"), "Bearer original-opaque-fixture-credential")
        self.assertIsNone(requests[1].get_header("Authorization"))
        self.assertEqual(client.calls, 2)

    def test_untrusted_nonhttps_userinfo_port_fragment_or_bad_storage_host_is_rejected(self):
        for value in ["http://original.blob.core.windows.net/a", "https://attacker.invalid/a",
                "https://blob.core.windows.net.attacker.invalid/a", "https://original.blob.core.windows.net:8443/a",
                "https://user:private@original.blob.core.windows.net/a", "https://original.blob.core.windows.net/a#secret",
                "https://original.blob.core.windows.net:invalid/a", None]:
            with self.subTest(value=value), self.assertRaises(ValueError) as error:
                artifact_destination(value)
            self.assertNotIn("private", str(error.exception))

    def test_errors_do_not_print_native_response_or_signed_storage_destination(self):
        client = self.client()
        class Opener:
            def open(self, request, timeout):
                raise urllib.error.HTTPError(request.full_url, 403, "private original response", {}, None)
        with patch("urllib.request.build_opener", return_value=Opener()):
            with self.assertRaises(APIError) as error: client.publication_archive(50)
        self.assertEqual(error.exception.status, 403)
        self.assertNotIn("private", str(error.exception))

    def test_body_identities_and_shared_request_budget_are_bounded(self):
        for identity in [True, 0, -1, "50"]:
            with self.assertRaises(ValueError): self.client().publication_archive(identity)
        client = self.client(); client.calls = 150
        with self.assertRaises(ValueError): client.publication_archive(50)
        class Opener:
            def open(self, request, timeout): return io.BytesIO(b"x" * (64 * 1024 + 1))
        with patch("urllib.request.build_opener", return_value=Opener()):
            with self.assertRaises(ValueError): self.client().publication_archive(50)


if __name__ == "__main__":
    unittest.main()

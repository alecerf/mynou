"""CI-only regressions for bounded read-only package diagnosis and secret filtering."""
import json
import os
import unittest
from unittest.mock import patch

from inspect_package import PACKAGE_PATH, inspect_package
from publish_container import CommandError


SOURCE = "a" * 40
VERSION = "0.22.17"
ENV = {"GITHUB_REPOSITORY": "alecerf/mynou", "GITHUB_EVENT_NAME": "pull_request",
       "MYNOU_DIAGNOSTIC_HEAD_REPOSITORY": "alecerf/mynou"}


class PackageInspectionTests(unittest.TestCase):
    def setUp(self):
        environment = patch.dict(os.environ, ENV)
        environment.start()
        self.addCleanup(environment.stop)

    def native(self, package, versions):
        calls = []
        def run(args):
            calls.append(args)
            return json.dumps(package if args[-1] == PACKAGE_PATH else versions)
        return run, calls

    def test_actual_metadata_and_matching_partial_tags_are_reported_without_secrets(self):
        package = {"name": "mynou", "package_type": "container", "visibility": "private",
                   "owner": {"login": "alecerf", "ignored": "sensitive-owner-field"},
                   "repository": {"id": 42, "full_name": "alecerf/mynou"},
                   "description": "sensitive-package-field"}
        digest = "sha256:" + "b" * 64
        versions = [{"name": digest, "metadata": {"container":
                     {"tags": [VERSION, "sha-" + SOURCE, "unrelated-sensitive-tag"]}}},
                    {"name": "sha256:" + "c" * 64, "metadata": {"container":
                     {"tags": ["unrelated-version"]}}}]
        run, calls = self.native(package, versions)
        proof = inspect_package(VERSION, SOURCE, run)
        self.assertTrue(proof["package_identity_matches"])
        self.assertEqual(proof["repository_metadata"], "matches")
        self.assertEqual(proof["visibility"], "private")
        self.assertTrue(proof["versions_complete"])
        self.assertFalse(proof["publication_verified"])
        self.assertEqual(proof["observed_tags"],
                         [{"tag": VERSION, "digest": digest}, {"tag": "sha-" + SOURCE, "digest": digest}])
        self.assertNotIn("sensitive", json.dumps(proof))
        self.assertNotIn("unrelated", json.dumps(proof))
        self.assertEqual(calls, [["gh", "api", "--method", "GET", PACKAGE_PATH],
                                ["gh", "api", "--method", "GET", PACKAGE_PATH + "/versions?per_page=100"]])

    def test_missing_repository_fields_and_full_page_do_not_fabricate_link_or_absence(self):
        for repository, expected in ((None, "missing"), ({}, "missing"),
                                     ({"full_name": "other/repo"}, "different")):
            with self.subTest(repository=repository):
                run, _ = self.native({"visibility": "private", "repository": repository}, [{}] * 100)
                proof = inspect_package(VERSION, SOURCE, run)
                self.assertEqual(proof["repository_metadata"], expected)
                self.assertFalse(proof["versions_complete"])
                self.assertEqual(proof["observed_tags"], [])
                self.assertFalse(proof["publication_verified"])

    def test_wrong_repository_forks_events_or_identity_make_no_api_calls(self):
        for changes, version, source in (
                ({"GITHUB_REPOSITORY": "other/repo"}, VERSION, SOURCE),
                ({"MYNOU_DIAGNOSTIC_HEAD_REPOSITORY": "other/fork"}, VERSION, SOURCE),
                ({"GITHUB_EVENT_NAME": "push"}, VERSION, SOURCE),
                ({}, "version;unexpected", SOURCE), ({}, VERSION, "invalid-source")):
            with self.subTest(changes=changes), patch.dict(os.environ, changes):
                with self.assertRaises(ValueError):
                    inspect_package(version, source, lambda args: self.fail("API request ran"))

    def test_denial_hidden_404_or_outage_fails_without_dumping_error_output(self):
        for status in ("HTTP 401", "HTTP 403", "HTTP 404", "network outage"):
            def failed(args):
                raise CommandError(args, status + " sensitive-response-token")
            with self.subTest(status=status), self.assertRaises(RuntimeError) as raised:
                inspect_package(VERSION, SOURCE, failed)
            self.assertNotIn("sensitive-response-token", str(raised.exception))
            self.assertIn("not proof of absence", str(raised.exception))

    def test_malformed_or_oversized_responses_and_matching_digest_fail(self):
        for package, versions in (
                ([], []), ({}, {}), ({}, [{}] * 101),
                ({}, [None]),
                ({}, [{"name": "invalid", "metadata": {"container": {"tags": [VERSION]}}}]),
                ({}, [{"metadata": {"container": {"tags": [VERSION] * 1_001}}}]),
                ({}, [{"metadata": {"container": {"tags": [None]}}}])):
            with self.subTest(package=package, versions=versions):
                run, _ = self.native(package, versions)
                with self.assertRaises(ValueError):
                    inspect_package(VERSION, SOURCE, run)
        for raw in ("not-json", " " * 65_537):
            with self.subTest(raw_size=len(raw)), self.assertRaises(ValueError):
                inspect_package(VERSION, SOURCE, lambda args: raw)


if __name__ == "__main__":
    unittest.main()

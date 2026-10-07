"""CI-only regressions for publication authorization, collisions and failure paths."""
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from publish_container import CommandError, Publisher


SOURCE = "a" * 40
ENV = {"GITHUB_REPOSITORY": "alecerf/mynou", "GITHUB_SHA": SOURCE,
       "GITHUB_REF": "refs/heads/trunk", "GITHUB_EVENT_NAME": "push",
       "GITHUB_ACTOR": "alecerf", "GH_TOKEN": "fixture-only"}
LABELS = {"org.opencontainers.image.source": "https://github.com/alecerf/mynou",
          "org.opencontainers.image.revision": SOURCE,
          "org.opencontainers.image.version": "0.22.17"}



class Registry:
    def __init__(self, copied=b"checked", ready=True):
        self.calls = []
        self.tags = {}
        self.local_tags = {}
        self.copied = copied
        self.ready = ready
        self.digest = "ghcr.io/alecerf/mynou@sha256:" + "c" * 64

    def __call__(self, args, input=None):
        self.calls.append((args, input))
        if args[0] == "gh":
            return json.dumps({"visibility": "private", "repository": {"full_name": "alecerf/mynou"}})
        if args[1:3] == ["manifest", "inspect"]:
            if args[3] not in self.tags:
                raise CommandError(args, "manifest unknown")
        elif args[1:3] == ["image", "inspect"]:
            return json.dumps([{"Id": "checked", "Os": "linux", "Architecture": "amd64",
                "Config": {"User": "1000:1000", "Labels": LABELS}, "RepoDigests": [self.digest]}])
        elif args[1] == "tag":
            self.local_tags[args[3]] = args[2]
        elif args[1] == "push":
            self.tags[args[2]] = self.local_tags[args[2]]
        elif args[1] == "create":
            return "d" * 64
        elif args[1] == "cp":
            Path(args[3]).write_bytes(self.copied)
        elif args[1] == "run":
            return json.dumps({"job": {"state": "ready" if self.ready else "failed"},
                               "plex_scan_confirmed": self.ready})
        return "{}"


class PublicationTests(unittest.TestCase):
    def setUp(self):
        self.environment = patch.dict(os.environ, ENV, clear=False)
        self.environment.start()
        self.addCleanup(self.environment.stop)

    def test_pull_requests_and_wrong_refs_never_run_a_command(self):
        for changes in ({"GITHUB_EVENT_NAME": "pull_request"},
                        {"GITHUB_REF": "refs/heads/work/request"},
                        {"GITHUB_REPOSITORY": "other/repository"},
                        {"GITHUB_SHA": "not-a-source"}):
            with self.subTest(changes=changes), patch.dict(os.environ, changes):
                with self.assertRaises(ValueError):
                    Publisher("0.22.17", lambda *args: self.fail("Command ran"))

    def test_private_package_must_match_the_repository(self):
        for package in ({"visibility": "public"},
                        {"visibility": "private", "repository": {"full_name": "other/repo"}}):
            publisher = Publisher("0.22.17", lambda args: json.dumps(package))
            with self.assertRaises(ValueError):
                publisher.private_package(allow_missing=True)

    def test_only_actual_package_404_allows_first_publication(self):
        def missing(args):
            raise CommandError(args, "gh: Not Found (HTTP 404)")
        Publisher("0.22.17", missing).private_package(allow_missing=True)
        with self.assertRaises(RuntimeError):
            Publisher("0.22.17", missing).private_package()
        for response in ("HTTP 403", "HTTP 401", "connection timed out"):
            def failed(args):
                raise CommandError(args, response)
            with self.subTest(response=response), self.assertRaises(RuntimeError):
                Publisher("0.22.17", failed).private_package(allow_missing=True)

    def test_registry_outages_and_denials_do_not_become_missing_tags(self):
        for response in ("unauthorized", "denied", "connection timed out"):
            def failed(args):
                raise CommandError(args, response)
            with self.subTest(response=response), self.assertRaises(RuntimeError):
                Publisher("0.22.17", failed).existing("ghcr.io/alecerf/mynou:0.22.17")

    def test_unknown_manifest_is_the_only_absent_tag_path(self):
        for response in ("manifest unknown", "no such manifest"):
            def missing(args):
                raise CommandError(args, response)
            self.assertFalse(Publisher("0.22.17", missing).existing("image:version"))

    def test_existing_conflicting_image_is_rejected_without_writes(self):
        calls = []
        def native(args):
            calls.append(args)
            if args[:3] == ["docker", "image", "inspect"]:
                return json.dumps([{"Id": "conflicting", "Os": "linux",
                    "Architecture": "amd64", "Config": {"User": "1000:1000", "Labels": LABELS}}])
            return "{}"
        publisher = Publisher("0.22.17", native)
        publisher.expected_id = "checked"
        with self.assertRaises(ValueError):
            publisher.existing("image:version")
        self.assertFalse(any(args[1] in ("push", "tag") for args in calls))

    def test_exact_existing_image_is_reused_and_wrong_source_is_rejected(self):
        image = {"Id": "checked", "Os": "linux", "Architecture": "amd64",
                 "Config": {"User": "1000:1000", "Labels": dict(LABELS)}}
        def native(args):
            return json.dumps([image]) if args[:3] == ["docker", "image", "inspect"] else "{}"
        publisher = Publisher("0.22.17", native)
        publisher.expected_id = "checked"
        self.assertTrue(publisher.existing("image:version"))
        image["Config"]["Labels"]["org.opencontainers.image.revision"] = "b" * 40
        with self.assertRaises(ValueError):
            publisher.existing("image:version")


    def publish_fixture(self, registry):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binary = root / "checked"
            binary.write_bytes(b"checked")
            return Publisher("0.22.17", registry).publish(root / "archive", binary, root)

    def test_interrupted_publication_reuses_exact_tags_and_digest_without_new_pushes(self):
        registry = Registry()
        proof = self.publish_fixture(registry)
        self.assertEqual(proof["image"], registry.digest)
        self.assertEqual(proof["source"], SOURCE)
        self.assertEqual(len([args for args, _ in registry.calls if args[1] == "push"]), 2)
        self.publish_fixture(registry)
        self.assertEqual(len([args for args, _ in registry.calls if args[1] == "push"]), 2)
        login = [(args, value) for args, value in registry.calls if args[1] == "login"]
        self.assertTrue(all(value == "fixture-only\n" and "fixture-only" not in args for args, value in login))
        run = next(args for args, _ in registry.calls if args[1] == "run")
        self.assertIn("--read-only", run)
        self.assertIn("--network", run)
        self.assertIn("none", run)
        self.assertIn("no-new-privileges:true", run)

    def test_changed_pulled_binary_blocks_release_proof(self):
        with self.assertRaises(ValueError):
            self.publish_fixture(Registry(copied=b"changed"))

    def test_failed_pulled_demonstration_blocks_release_proof(self):
        with self.assertRaises(ValueError):
            self.publish_fixture(Registry(ready=False))


if __name__ == "__main__":
    unittest.main()

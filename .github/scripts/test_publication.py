"""CI-only regressions for publication authorization, collisions and failure paths."""
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from publish_container import CommandError, Publisher, main, verify_demo


SOURCE = "a" * 40
ENV = {"GITHUB_REPOSITORY": "alecerf/mynou", "GITHUB_SHA": SOURCE,
       "GITHUB_REF": "refs/heads/trunk", "GITHUB_EVENT_NAME": "push",
       "GITHUB_ACTOR": "alecerf", "GH_TOKEN": "fixture-only",
       "GITHUB_REPOSITORY_ID": "1403318143"}
REPOSITORY = {"id": 1403318143, "full_name": "alecerf/mynou", "private": True}
PACKAGE = {"id": 42, "name": "mynou", "package_type": "container",
           "owner": {"login": "alecerf"}, "visibility": "private",
           "repository": {"id": 1403318143, "full_name": "alecerf/mynou"}}
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
            return json.dumps(REPOSITORY if args[-1] == "/repos/alecerf/mynou" else PACKAGE)
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
                        {"GITHUB_SHA": "not-a-source"},
                        {"GITHUB_REPOSITORY_ID": "0"}):
            with self.subTest(changes=changes), patch.dict(os.environ, changes):
                with self.assertRaises(ValueError):
                    Publisher("0.22.17", lambda *args: self.fail("Command ran"))

    def test_private_package_must_match_the_repository(self):
        for package in ({"visibility": "public"},
                        {"visibility": "private", "repository": {"full_name": "other/repo"}}):
            publisher = Publisher("0.22.17", lambda args: json.dumps(package))
            with self.assertRaises(ValueError):
                publisher.private_package(allow_missing=True)


    def test_omitted_container_association_needs_exact_private_package_identity(self):
        package = {key: value for key, value in PACKAGE.items() if key != "repository"}
        publisher = Publisher("0.22.17", lambda args: json.dumps(package))
        publisher.private_package()
        self.assertEqual(publisher.package_id, 42)
        self.assertEqual(publisher.package_association, "not_exposed")
        for changes in ({"visibility": "public"}, {"visibility": "internal"},
                        {"name": "other"}, {"package_type": "npm"},
                        {"owner": {"login": "other"}}, {"id": True}, {"id": 0}):
            with self.subTest(changes=changes):
                bad = dict(package, **changes)
                with self.assertRaises(ValueError):
                    Publisher("0.22.17", lambda args: json.dumps(bad)).private_package()

    def test_reported_conflicting_repository_is_still_rejected(self):
        for linked in ({"full_name": "other/repo"}, {},
                       {"full_name": "alecerf/mynou", "id": 7},
                       {"full_name": "alecerf/mynou", "id": True}, "not-an-object"):
            with self.subTest(linked=linked), self.assertRaises(ValueError):
                package = dict(PACKAGE, repository=linked)
                Publisher("0.22.17", lambda args: json.dumps(package)).private_package()
        publisher = Publisher("0.22.17", lambda args: json.dumps(PACKAGE))
        publisher.private_package()
        self.assertEqual(publisher.package_association, "reported-match")

    def test_wrong_or_malformed_source_repository_blocks_every_docker_command(self):
        for changes in ({"id": 7}, {"id": True}, {"full_name": "other/repo"},
                        {"private": None}, {"private": "false"}, {"private": 0}):
            registry = Registry()
            def native(args, input=None):
                if args[-1] == "/repos/alecerf/mynou":
                    return json.dumps(dict(REPOSITORY, **changes))
                return registry(args, input=input)
            with self.subTest(changes=changes), tempfile.TemporaryDirectory() as directory:
                with self.assertRaises(ValueError):
                    root = Path(directory)
                    Publisher("0.22.17", native).publish(root / "archive", root / "binary", root)
            self.assertFalse(any(args[0] == "docker" for args, _ in registry.calls))

    def test_exact_public_or_private_source_retains_private_package_proof(self):
        for private in (False, True):
            registry = Registry()
            def native(args, input=None):
                if args[-1] == "/repos/alecerf/mynou":
                    return json.dumps(dict(REPOSITORY, private=private))
                return registry(args, input=input)
            with self.subTest(private=private), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                binary = root / "checked"
                binary.write_bytes(b"checked")
                proof = Publisher("0.22.17", native).publish(root / "archive", binary, root)
            self.assertEqual(proof["repository_visibility"], "private" if private else "public")
            self.assertEqual(proof["visibility"], "private")
            self.assertEqual(proof["repository_id"], REPOSITORY["id"])
            self.assertEqual(proof["package_id"], PACKAGE["id"])
            self.assertEqual(proof["image"], registry.digest)
            self.assertEqual(proof["verification_cleanup"], "completed")
            self.assertNotIn(ENV["GH_TOKEN"], json.dumps(proof))

    def test_public_source_cannot_authorize_public_or_foreign_package(self):
        for changes in ({"visibility": "public"},
                        {"repository": {"full_name": "other/repo"}},
                        {"repository": {"full_name": "alecerf/mynou", "id": 7}}):
            registry = Registry()
            def native(args, input=None):
                if args[-1] == "/repos/alecerf/mynou":
                    return json.dumps(dict(REPOSITORY, private=False))
                if args[-1] == "/users/alecerf/packages/container/mynou":
                    return json.dumps(dict(PACKAGE, **changes))
                return registry(args, input=input)
            with self.subTest(changes=changes), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                with self.assertRaises(ValueError):
                    Publisher("0.22.17", native).publish(root / "archive", root / "binary", root)
            self.assertFalse(any(args[0] == "docker" for args, _ in registry.calls))

    def test_replaced_package_id_blocks_publication_proof(self):
        registry = Registry()
        reads = 0
        def native(args, input=None):
            nonlocal reads
            if args[-1] == "/users/alecerf/packages/container/mynou":
                reads += 1
                return json.dumps(dict(PACKAGE, id=42 if reads == 1 else 43))
            return registry(args, input=input)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binary = root / "binary"
            binary.write_bytes(b"checked")
            with self.assertRaisesRegex(ValueError, "Package identity changed"):
                Publisher("0.22.17", native).publish(root / "archive", binary, root)
        self.assertFalse(any(args[1] == "create" for args, _ in registry.calls))

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

    def test_api_denials_report_only_the_exact_numeric_status(self):
        hostile = "ghp_" + "Z" * 36
        for status in (401, 403, 404, 429, 502):
            def denied(args):
                raise CommandError(args, f"gh: denied {hostile} (HTTP {status})\n")
            publisher = Publisher("0.22.17", denied)
            for operation, message in (
                    (publisher.repository_context, "Cannot verify the publishing repository"),
                    (publisher.private_package, "Cannot verify private package identity")):
                with self.subTest(status=status, operation=message):
                    with self.assertRaises(RuntimeError) as caught:
                        operation()
                    self.assertEqual(str(caught.exception), message + f" [HTTP {status}]")
                    self.assertNotIn(hostile, str(caught.exception))

    def test_untrusted_or_ambiguous_status_never_establishes_package_absence(self):
        hostile = "ghp_" + "Z" * 36
        replies = (
            "HTTP 404", '{"message":"HTTP 404"}',
            "gh: denied (HTTP 4040)", "gh: denied (HTTP 600)",
            "gh: denied (HTTP -1)", "gh: denied (HTTP 000)",
            "gh: unavailable (HTTP 403)\ngh: unavailable (HTTP 404)\n",
            "gh: unavailable (HTTP 404)\ngh: unavailable (HTTP 404)\n",
            hostile, "gh: " + "x" * 65_536 + " (HTTP 404)",
        )
        for reply in replies:
            def failed(args):
                raise CommandError(args, reply)
            with self.subTest(case=replies.index(reply)):
                publisher = Publisher("0.22.17", failed)
                with self.assertRaises(RuntimeError) as caught:
                    publisher.private_package(allow_missing=True)
                self.assertEqual(str(caught.exception),
                                 "Cannot verify private package identity [HTTP status unavailable]")
                self.assertNotIn(hostile, str(caught.exception))

    def test_non_api_command_stderr_cannot_authorize_an_absent_package(self):
        error = CommandError(["docker", "pull", "synthetic"], "gh: denied (HTTP 404)")
        self.assertIsNone(error.http_status)
        self.assertEqual(error.api_detail(), " [HTTP status unavailable]")

    def test_package_denial_blocks_every_docker_command_and_publication(self):
        registry = Registry()
        def denied(args, input=None):
            if args[-1] == "/users/alecerf/packages/container/mynou":
                raise CommandError(args, "gh: Resource not accessible (HTTP 403)\n")
            return registry(args, input=input)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            with self.assertRaises(RuntimeError) as caught:
                Publisher("0.22.17", denied).publish(root / "archive", root / "binary", root)
        self.assertEqual(str(caught.exception), "Cannot verify private package identity [HTTP 403]")
        self.assertFalse(any(args[0] == "docker" for args, _ in registry.calls))

    def test_repository_api_limit_blocks_publication_before_package_or_docker(self):
        calls = []
        def exhausted(args):
            calls.append(args)
            raise CommandError(args, "gh: API rate limit exceeded (HTTP 429)\n")
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            with self.assertRaises(RuntimeError) as caught:
                Publisher("0.22.17", exhausted).publish(root / "archive", root / "binary", root)
        self.assertEqual(str(caught.exception), "Cannot verify the publishing repository [HTTP 429]")
        self.assertEqual(calls, [["gh", "api", "--method", "GET", "/repos/alecerf/mynou"]])

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

    def test_private_demo_cleanup_uses_the_nonroot_mount_owner(self):
        mounts = []
        def native(args):
            self.assertEqual(args[args.index("--user") + 1],
                             f"{os.geteuid()}:{os.getegid()}")
            self.assertIn("--read-only", args)
            self.assertIn("none", args)
            self.assertIn("ALL", args)
            self.assertIn("no-new-privileges:true", args)
            mount = args[args.index("--mount") + 1]
            directory = Path(mount.removeprefix("type=bind,src=").removesuffix(",dst=/data"))
            mounts.append(directory)
            self.assertEqual(directory.stat().st_mode & 0o777, 0o700)
            seeder = directory / "demo" / "seeder"
            seeder.mkdir(mode=0o700, parents=True)
            (seeder / "synthetic").write_bytes(b"fixture-only")
            return json.dumps({"job": {"state": "ready"}, "plex_scan_confirmed": True})
        with tempfile.TemporaryDirectory() as parent:
            with patch.dict(os.environ, {"RUNNER_TEMP": parent}):
                proof = verify_demo("mynou:0.22.17", native)
            self.assertEqual(proof["cleanup"], "completed")
            self.assertEqual(proof["state"], "ready")
            self.assertTrue(proof["plex_scan_confirmed"])
            self.assertFalse(mounts[0].exists())
            self.assertEqual(list(Path(parent).iterdir()), [])

    def test_root_verification_identity_never_runs_a_command(self):
        for uid, gid in ((0, 1001), (1001, 0)):
            with self.subTest(uid=uid, gid=gid), patch("os.geteuid", return_value=uid):
                with patch("os.getegid", return_value=gid), self.assertRaises(ValueError):
                    verify_demo("mynou:0.22.17", lambda *args: self.fail("Command ran"))

    def test_cleanup_failure_never_announces_publication(self):
        real_temporary = tempfile.TemporaryDirectory
        class FailedCleanup:
            def __init__(self, **kwargs):
                self.directory = real_temporary(**kwargs)
            def __enter__(self):
                return self.directory.__enter__()
            def __exit__(self, *args):
                self.directory.__exit__(*args)
                raise PermissionError("fixture cleanup failed")
        with real_temporary() as parent:
            output = Path(parent) / "output"
            output.write_text("existing\n")
            with patch.dict(os.environ, {"RUNNER_TEMP": parent, "GITHUB_OUTPUT": str(output)}):
                with patch("sys.argv", ["publish_container.py", "archive", "0.22.17", "binary"]):
                    with patch("publish_container.Publisher") as publisher:
                        publisher.return_value.publish.return_value = {"image": "fixture-image"}
                        with patch("publish_container.tempfile.TemporaryDirectory", FailedCleanup):
                            with self.assertRaises(PermissionError):
                                main()
            self.assertEqual(output.read_text(), "existing\n")
            self.assertFalse((Path(parent) / "mynou-image.json").exists())

    def test_readonly_demo_mode_rejects_remote_images(self):
        with patch("sys.argv", ["publish_container.py", "--verify-demo", "ghcr.io/alecerf/mynou:0.22.17"]):
            with patch("publish_container.verify_demo", side_effect=AssertionError("Command ran")):
                with self.assertRaises(ValueError):
                    main()

    def test_changed_pulled_binary_blocks_release_proof(self):
        with self.assertRaises(ValueError):
            self.publish_fixture(Registry(copied=b"changed"))

    def test_failed_pulled_demonstration_blocks_release_proof(self):
        with self.assertRaises(ValueError):
            self.publish_fixture(Registry(ready=False))


if __name__ == "__main__":
    unittest.main()

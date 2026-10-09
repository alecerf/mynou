"""Release cadence and release-PR scenarios on synthetic data; CI only."""
import base64
from datetime import datetime, timedelta, timezone
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from github import APIError
import protocol
import releases

AT = datetime(2026, 10, 16, 12, 0, tzinfo=timezone.utc)
POLICY = {"minimum_interval_days": 7, "urgent_label": "release-now"}
LATEST = {"tag": "v0.22.34", "published_at": AT - timedelta(days=8)}
BASE = "b" * 40
HEAD = "a" * 40


def manifest(version="0.22.34", lto="thin"):
    return {"package": {"name": "mynou", "version": version}, "profile": {"release": {"lto": lto}}}


def toml(version="0.22.34", lto="thin"):
    text = f'[package]\nname = "mynou"\nversion = "{version}"\n\n[profile.release]\nlto = "{lto}"\n'
    return {"encoding": "base64", "content": base64.b64encode(text.encode()).decode()}


def release_files(version="0.23.0"):
    return {"Cargo.toml": "modified", "Cargo.lock": "modified", f"docs/releases/{version}.md": "added",
            "docs/releases/unreleased/39.md": "removed"}


class ReleasePullRequests(unittest.TestCase):
    def check(self, **overrides):
        values = dict(base=manifest(), head=manifest("0.23.0"), files=release_files(), labels={"release"},
                      latest=LATEST, shipped_change=True, tag_exists=False, now=AT, policy=POLICY)
        values.update(overrides)
        return releases.check(**values)

    def test_work_prs_keep_the_version_and_pass(self):
        self.assertEqual(self.check(head=manifest(), files={"src/lib.rs": "modified"}, labels=set()), [])

    def test_a_weekly_release_with_shipped_changes_passes(self):
        self.assertEqual(self.check(), [])

    def test_only_a_release_pr_changes_the_version(self):
        errors = self.check(labels=set(), files={"src/lib.rs": "modified", "Cargo.toml": "modified"})
        self.assertEqual(len(errors), 1)
        self.assertIn("labeled `release`", errors[0])
        self.assertIn("docs/releases/unreleased/", errors[0])

    def test_a_release_pr_bumps_the_version(self):
        self.assertEqual(self.check(head=manifest()), ["A PR labeled `release` must bump the package version."])
        self.assertIn("must increase", self.check(base=manifest("0.23.0"), head=manifest("0.22.35"),
                                                  files=release_files("0.22.35"))[0])

    def test_a_release_pr_changes_only_the_version_and_notes(self):
        files = dict(release_files(), **{"src/lib.rs": "modified", "engineering/board.py": "added"})
        self.assertIn("move these to their own PRs: engineering/board.py, src/lib.rs", " ".join(self.check(files=files)))
        # A rename's old path counts: moving product code into docs/releases/ is not a release.
        renamed = dict(release_files(), **{"src/old.rs": "removed"})
        self.assertIn("src/old.rs", " ".join(self.check(files=renamed)))
        self.assertIn("nothing in Cargo.toml but the package version",
                      " ".join(self.check(head=manifest("0.23.0", lto="fat"))))

    def test_a_release_pr_adds_its_notes_and_a_new_tag(self):
        files = release_files()
        files.pop("docs/releases/0.23.0.md")
        self.assertIn("docs/releases/0.23.0.md", " ".join(self.check(files=files)))
        files["docs/releases/0.23.0.md"] = "removed"
        self.assertIn("docs/releases/0.23.0.md", " ".join(self.check(files=files)))
        self.assertIn("v0.23.0 already exists", " ".join(self.check(tag_exists=True)))

    def test_releases_are_weekly_unless_urgent(self):
        recent = dict(LATEST, published_at=AT - timedelta(days=2))
        errors = self.check(latest=recent)
        self.assertEqual(len(errors), 1)
        self.assertIn("Releases are weekly: the next one may merge from 2026-10-21T12:00:00Z", errors[0])
        self.assertEqual(self.check(latest=recent, labels={"release", "release-now"}), [])
        self.assertEqual(self.check(latest=dict(LATEST, published_at=AT - timedelta(days=7))), [])
        self.assertEqual(self.check(latest=None, shipped_change=False), [])

    def test_engineering_only_changes_are_not_released(self):
        errors = self.check(shipped_change=False)
        self.assertEqual(len(errors), 1)
        self.assertIn("Nothing shipped changed since v0.22.34", errors[0])
        self.assertEqual(self.check(shipped_change=False, labels={"release", "release-now"}), [])

    def test_malformed_versions_fail_closed(self):
        for version in ["0.23", "v0.23.0", "0.23.0-rc1", "01.2.3", None]:
            with self.subTest(version=version), self.assertRaises(ValueError):
                releases.version({"package": {"version": version}})


class Cadence(unittest.TestCase):
    def test_shipped_inputs(self):
        same = manifest()
        for path in ["src/main.rs", "examples/demo.mp4", "Dockerfile", "deploy-compose.yaml", "rust-toolchain.toml"]:
            self.assertTrue(releases.shipped({path}, False, same, same), path)
        docs = {"docs/ci.md", "engineering/board.py", ".github/workflows/ci.yml", "AGENTS.md", "Cargo.lock"}
        self.assertFalse(releases.shipped(docs, False, same, manifest("0.22.35")))
        self.assertTrue(releases.shipped(docs, True, same, same))
        self.assertTrue(releases.shipped(docs, False, same, manifest(lto="fat")))

    def test_a_release_is_due_weekly_with_shipped_changes(self):
        self.assertTrue(releases.due(LATEST, True, AT, POLICY))
        self.assertFalse(releases.due(LATEST, False, AT, POLICY))
        self.assertFalse(releases.due(dict(LATEST, published_at=AT - timedelta(days=6)), True, AT, POLICY))
        self.assertTrue(releases.due(None, True, AT, POLICY))


class FakeAPI:
    def __init__(self, files, labels=("release",), head_version="0.23.0"):
        self.cfg = {"release_policy": POLICY, "default_branch": "trunk"}
        self.files, self.labels, self.head_version = files, labels, head_version
        self.paths = []

    def rest(self, method, path, value=None):
        self.paths.append(path)
        if method != "GET":
            raise AssertionError("The release policy only reads")
        if path == "pulls/5":
            return {"labels": [{"name": n} for n in self.labels], "base": {"sha": BASE}, "head": {"sha": HEAD}}
        if path in ("contents/Cargo.toml?ref=" + BASE, "contents/Cargo.toml?ref=v0.22.34"):
            return toml()
        if path == "contents/Cargo.toml?ref=" + HEAD:
            return toml(self.head_version)
        if path == "releases/latest":
            return {"tag_name": "v0.22.34", "published_at": protocol.stamp(AT - timedelta(days=8))}
        if path == "git/ref/tags/v0.23.0":
            raise APIError(404)
        if path == "compare/v0.22.34..." + HEAD:
            return {"files": [{"filename": "src/par2.rs"}, {"filename": "Cargo.toml"}]}
        raise AssertionError(path)

    def pages(self, path, key=None):
        if path == "pulls/5/files":
            return self.files
        raise AssertionError(path)


class ReleasePolicyCheck(unittest.TestCase):
    def test_the_check_reads_native_state_and_passes_a_compliant_release(self):
        api = FakeAPI([{"filename": "Cargo.toml", "status": "modified"},
                       {"filename": "Cargo.lock", "status": "modified"},
                       {"filename": "docs/releases/0.23.0.md", "status": "added"}])
        self.assertEqual(releases.check_pr(api, 5, AT), [])
        self.assertIn("git/ref/tags/v0.23.0", api.paths)

    def test_renamed_product_code_is_caught_and_work_prs_skip_release_lookups(self):
        api = FakeAPI([{"filename": "Cargo.toml", "status": "modified"},
                       {"filename": "docs/releases/0.23.0.md", "status": "renamed",
                        "previous_filename": "src/old.rs"}])
        self.assertIn("src/old.rs", " ".join(releases.check_pr(api, 5, AT)))
        work = FakeAPI([{"filename": "src/par2.rs", "status": "modified"}], labels=(), head_version="0.22.34")
        self.assertEqual(releases.check_pr(work, 5, AT), [])
        self.assertNotIn("releases/latest", work.paths)


if __name__ == "__main__":
    unittest.main()

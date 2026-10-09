"""Board composition on a synthetic repository; CI only."""
from datetime import datetime, timedelta, timezone
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import board
import protocol
from test_releases import toml

AT = datetime(2026, 10, 16, 12, 0, tzinfo=timezone.utc)
CFG = {"repository": "original/mynou", "default_branch": "trunk", "trusted_actors": {"owner": 7},
       "claim_ttl_minutes": 120, "release_policy": {"minimum_interval_days": 7, "urgent_label": "release-now"},
       "product_planning": {"issue": 42, "ready_minimum": 3, "ready_maximum": 5, "review_interval_hours": 24}}


def ago(minutes):
    return protocol.stamp(AT - timedelta(minutes=minutes))


def issue(number, *names, updated=10):
    return {"number": number, "title": f"Issue {number}", "labels": [{"name": n} for n in names],
            "updated_at": ago(updated), "issue_dependencies_summary": {"blocked_by": 0}}


def note(body, minutes, ident):
    return {"id": ident, "body": body, "created_at": ago(minutes), "user": {"login": "owner", "id": 7}}


class Repository:
    def __init__(self):
        self.cfg = CFG
        self.issues = [
            issue(39, "agent-work", "status:ready", "priority:p2", "risk:high", "origin:agent", "enhancement"),
            issue(56, "agent-work", "status:ready", "priority:p3", "origin:agent", "enhancement"),
            issue(46, "agent-work", "status:ready", "priority:p2", "origin:agent", "enhancement", updated=180),
            issue(45, "agent-work", "status:blocked", "priority:p2"),
            issue(42, "agent-work", "status:blocked", updated=3000),
            issue(71, "agent-work", "status:ready", "priority:p1", "release"),
            dict(issue(65, "agent-work"), pull_request={}),
        ]
        self.pulls = [
            {"number": 65, "title": "PAR2", "body": "Closes #39", "draft": False, "updated_at": ago(5),
             "head": {"sha": "a" * 40}},
            {"number": 57, "title": "Progress", "body": "Closes #45", "draft": True, "updated_at": ago(600),
             "head": {"sha": "c" * 40}},
        ]
        self.comments = {
            39: [note("/assign codex-1", 30, 1)],
            46: [note("/assign claude-2", 180, 2)],
            56: [note("Looks useful", 60, 3)],
            71: [],
            65: [note("/wait qa", 20, 4), note("/assign claude-qa", 15, 5)],
            57: [note("/wait ci", 30, 6)],
        }

    def rest(self, method, path, value=None):
        if method != "GET":
            raise AssertionError("The board never writes")
        if path == "releases/latest":
            return {"tag_name": "v0.22.34", "published_at": protocol.stamp(AT - timedelta(days=8))}
        if path == "compare/v0.22.34...trunk":
            return {"files": [{"filename": "src/par2.rs"}]}
        if path in ("contents/Cargo.toml?ref=v0.22.34", "contents/Cargo.toml?ref=trunk"):
            return toml()
        raise AssertionError(path)

    def pages(self, path, key=None):
        if path == "issues?state=open&labels=agent-work":
            return self.issues
        if path == "pulls?state=open":
            return self.pulls
        if path.startswith("issues/") and path.endswith("/comments"):
            return self.comments[int(path.split("/")[1])]
        if path == "pulls/65/files":
            return [{"filename": "src/par2.rs"}]
        if path == "pulls/57/files":
            return [{"filename": "src/web/views.rs"}]
        if path == "commits/" + "a" * 40 + "/check-runs" and key == "check_runs":
            return [{"status": "completed", "conclusion": "success"}]
        if path == "commits/" + "c" * 40 + "/check-runs" and key == "check_runs":
            return [{"status": "completed", "conclusion": "failure"}]
        raise AssertionError(path)


class Board(unittest.TestCase):
    def test_free_work_claims_turns_release_and_planning(self):
        result = board.board(Repository(), AT, "codex-1")
        # #39 is owned and waits for QA; #46's claim lapsed; blocked Issues are not work.
        self.assertEqual([w["issue"] for w in result["work"]], [71, 46, 56])
        self.assertEqual(result["work"][1]["pr"], None)
        claims = {c["issue"]: c["owner"] for c in result["claims"]}
        self.assertEqual(claims[39], {"agent": "codex-1", "since": ago(30), "lapsed": False})
        self.assertEqual(claims[46], {"agent": "claude-2", "since": ago(180), "lapsed": True})
        pulls = {p["pr"]: p for p in result["pull_requests"]}
        self.assertEqual([p["pr"] for p in result["pull_requests"]], [57, 65])
        self.assertEqual((pulls[65]["waiting_for"], pulls[65]["ci"], pulls[65]["security_required"]),
                         ("qa", "success", True))
        self.assertEqual(pulls[65]["owner"]["agent"], "claude-qa")
        # `/wait ci` on the draft settles back to its author once CI has failed.
        self.assertEqual((pulls[57]["waiting_for"], pulls[57]["ci"]), ("author", "failure"))
        # A shipped change is a week old, but release Issue #71 is already open.
        self.assertEqual(result["release"]["latest"], "v0.22.34")
        self.assertTrue(result["release"]["shipped_changes_since_latest"])
        self.assertEqual(result["release"]["open_release_issues"], [71])
        self.assertFalse(result["release"]["due"])
        self.assertEqual(result["product_planning"], {"issue": 42, "ready_proposals": 3, "due": False})
        self.assertEqual(result["mine"], {"issues": [39], "pull_requests": []})

    def test_a_low_backlog_and_no_open_release_make_both_due(self):
        repository = Repository()
        repository.issues = [i for i in repository.issues if i["number"] not in (46, 71)]
        result = board.board(repository, AT)
        self.assertTrue(result["release"]["due"])
        self.assertEqual(result["product_planning"], {"issue": 42, "ready_proposals": 2, "due": True})
        self.assertNotIn("mine", result)


if __name__ == "__main__":
    unittest.main()

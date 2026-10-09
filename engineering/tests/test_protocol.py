"""Comment-command coordination scenarios on synthetic data; CI only."""
from datetime import datetime, timedelta, timezone
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import protocol

AT = datetime(2026, 10, 9, 12, 0, tzinfo=timezone.utc)
ACTORS = {"owner": 7}
HEAD = "a" * 40
OLD = "b" * 40


def comment(body, minute=0, ident=None, login="owner", user_id=7):
    return {"id": minute + 1 if ident is None else ident, "body": body,
            "created_at": protocol.stamp(AT + timedelta(minutes=minute)),
            "user": {"login": login, "id": user_id}}


def issue(number, *names, updated=AT, blocked_by=0):
    return {"number": number, "labels": [{"name": n} for n in names], "updated_at": protocol.stamp(updated),
            "issue_dependencies_summary": {"blocked_by": blocked_by}}


class Commands(unittest.TestCase):
    def test_first_line_commands_are_parsed_exactly(self):
        self.assertEqual(protocol.parse("/assign codex-1"), ("assign", "codex-1"))
        self.assertEqual(protocol.parse("  /unassign claude-7b1c  \nTaking a break"), ("unassign", "claude-7b1c"))
        self.assertEqual(protocol.parse("/wait security\nTouches engineering/"), ("wait", "security"))
        self.assertEqual(protocol.parse("/approve qa " + HEAD), ("approve", "qa", HEAD))
        self.assertEqual(protocol.parse("/reject security " + HEAD + "\nA finding"), ("reject", "security", HEAD))

    def test_ordinary_text_and_malformed_commands_are_data(self):
        for body in [None, "", "Progress: tests added", "Please /assign codex-1", "/assign", "/assign Codex-1",
                     "/assign codex 1", "/assign x", "/wait reviewer", "/approve qa " + HEAD[:7],
                     "/approve ux " + HEAD, "/assign codex-1 now", "text\n/assign codex-1", "/ASSIGN codex-1"]:
            with self.subTest(body=body):
                self.assertIsNone(protocol.parse(body))

    def test_defanged_commands_from_trusted_accounts_keep_their_meaning(self):
        self.assertEqual(protocol.parse("\u00b7/\u00b7a\u00b7pprove qa " + HEAD + "\nSummary"), ("approve", "qa", HEAD))
        self.assertEqual(protocol.parse("\u200b/assign codex-1\ufeff"), ("assign", "codex-1"))
        self.assertIsNone(protocol.parse("\u00b7\u200b"))
        self.assertEqual(protocol.verdicts([comment("\u00b7/\u00b7a\u00b7pprove qa " + HEAD, 1)], ACTORS, HEAD),
                         {"qa": "approve"})
        untrusted = comment("\u00b7/\u00b7a\u00b7pprove qa " + HEAD, 1, login="someone", user_id=9)
        self.assertEqual(protocol.verdicts([untrusted], ACTORS, HEAD), {})

    def test_only_trusted_accounts_issue_commands(self):
        comments = [comment("/assign intruder", 0, login="someone", user_id=9),
                    comment("/assign spoofed", 1, login="owner", user_id=8),
                    dict(comment("/assign anonymous", 2), user=None),
                    comment("/assign codex-1", 3)]
        self.assertEqual(protocol.claim(comments, ACTORS)["agent"], "codex-1")


class Claims(unittest.TestCase):
    def test_first_assign_wins_regardless_of_listing_order(self):
        comments = [comment("/assign codex-1", 0, ident=1), comment("/assign claude-2", 0, ident=5)]
        for ordered in [comments, list(reversed(comments))]:
            owner = protocol.claim(ordered, ACTORS)
            self.assertEqual(owner, {"agent": "codex-1", "since": protocol.stamp(AT)})

    def test_only_the_owner_releases_and_then_the_next_claim_wins(self):
        comments = [comment("/assign codex-1", 0), comment("/unassign claude-2", 1),
                    comment("/assign claude-2", 2)]
        self.assertEqual(protocol.claim(comments, ACTORS)["agent"], "codex-1")
        comments += [comment("/unassign codex-1", 3), comment("/assign claude-2", 4)]
        self.assertEqual(protocol.claim(comments, ACTORS),
                         {"agent": "claude-2", "since": protocol.stamp(AT + timedelta(minutes=4))})
        comments.append(comment("/unassign claude-2", 5))
        self.assertIsNone(protocol.claim(comments, ACTORS))

    def test_claims_lapse_only_after_the_ttl_without_activity(self):
        activity = [protocol.stamp(AT), protocol.stamp(AT + timedelta(minutes=30))]
        self.assertFalse(protocol.lapsed(activity, AT + timedelta(minutes=150), 120))
        self.assertTrue(protocol.lapsed(activity, AT + timedelta(minutes=151), 120))


class PullRequestTurns(unittest.TestCase):
    def turn(self, comments, draft=False, security=False):
        return protocol.turn(comments, ACTORS, HEAD, draft, security)

    def test_defaults_follow_the_draft_state(self):
        self.assertEqual(self.turn([], draft=True), "author")
        self.assertEqual(self.turn([]), "qa")

    def test_latest_wait_or_current_rejection_sets_the_turn(self):
        self.assertEqual(self.turn([comment("/wait security", 1), comment("/wait user", 2)]), "user")
        self.assertEqual(self.turn([comment("/wait qa", 1), comment("/reject qa " + HEAD, 2)]), "author")
        self.assertEqual(self.turn([comment("/reject qa " + OLD, 1)]), "qa")

    def test_wait_ci_settles_once_checks_complete(self):
        comments = [comment("/wait ci", 1)]
        self.assertEqual(protocol.turn(comments, ACTORS, HEAD, False, False), "ci")
        self.assertEqual(protocol.turn(comments, ACTORS, HEAD, False, False, "pending"), "ci")
        self.assertEqual(protocol.turn(comments, ACTORS, HEAD, False, False, "success"), "qa")
        self.assertEqual(protocol.turn(comments, ACTORS, HEAD, True, False, "success"), "author")
        self.assertEqual(protocol.turn(comments, ACTORS, HEAD, False, False, "failure"), "author")

    def test_merge_needs_every_required_approval_of_the_current_head(self):
        self.assertEqual(self.turn([comment("/approve qa " + HEAD, 1)]), "merge")
        self.assertEqual(self.turn([comment("/approve qa " + OLD, 1)]), "qa")
        comments = [comment("/approve qa " + HEAD, 1), comment("/wait security", 2)]
        self.assertEqual(self.turn(comments, security=True), "security")
        comments.append(comment("/approve security " + HEAD, 3))
        self.assertEqual(self.turn(comments, security=True), "merge")
        self.assertEqual(protocol.verdicts(comments, ACTORS, HEAD), {"qa": "approve", "security": "approve"})
        comments.append(comment("/reject qa " + HEAD, 4))
        self.assertEqual(self.turn(comments, security=True), "author")
        self.assertEqual(protocol.verdicts(comments, ACTORS, OLD), {})

    def test_security_review_follows_sensitive_paths_and_risk(self):
        self.assertTrue(protocol.security_required(["engineering/board.py"], set()))
        self.assertTrue(protocol.security_required(["AGENTS.md"], set()))
        self.assertTrue(protocol.security_required(["src/tls/record.rs"], set()))
        self.assertTrue(protocol.security_required(["src/web/views.rs"], {"risk:high"}))
        self.assertFalse(protocol.security_required(["src/web/views.rs", "docs/web.md"], {"risk:medium"}))

    def test_ci_is_green_only_when_every_latest_check_completed_cleanly(self):
        done = {"status": "completed", "conclusion": "success"}
        self.assertEqual(protocol.ci_state([]), "missing")
        self.assertEqual(protocol.ci_state([done, {"status": "in_progress", "conclusion": None}]), "pending")
        self.assertEqual(protocol.ci_state([done, dict(done, conclusion="skipped")]), "success")
        for conclusion in ["failure", "cancelled", "timed_out", "action_required"]:
            self.assertEqual(protocol.ci_state([done, dict(done, conclusion=conclusion)]), "failure")


class Backlog(unittest.TestCase):
    def test_linked_issues_use_closing_keywords(self):
        self.assertEqual(protocol.linked_issues("Closes #39\nRelated to #40, fixes #41"), [39, 41])
        self.assertEqual(protocol.linked_issues(None), [])

    def test_priority_then_owner_requests_then_age(self):
        issues = [issue(5, "priority:p2"), issue(4, "priority:p1"), issue(6, "priority:p1", "origin:user"),
                  issue(3)]
        self.assertEqual([i["number"] for i in sorted(issues, key=protocol.rank)], [6, 4, 3, 5])

    def test_ghost_branches_have_no_open_pr_and_no_active_claim(self):
        def branch(name, protected=False):
            return {"name": name, "commit": {"sha": "f" * 40}, "protected": protected}
        branches = [branch("trunk"), branch("work/45-live-progress"), branch("work/46-retry"),
                    branch("claude/usenet-upgrade-46"), branch("claude/par2-media-39"),
                    branch("format/45-live-progress"), branch("release/0.20.4", protected=True),
                    branch("experiment")]
        found = protocol.ghosts(branches, "trunk", {"work/45-live-progress"}, {46})
        self.assertEqual([g["branch"] for g in found], ["claude/par2-media-39", "format/45-live-progress", "experiment"])
        self.assertEqual(found[0], {"branch": "claude/par2-media-39", "head": "f" * 40})

    def test_blocked_by_label_or_open_dependency(self):
        self.assertTrue(protocol.blocked(issue(1, "status:blocked")))
        self.assertTrue(protocol.blocked(issue(1, "status:ready", blocked_by=1)))
        self.assertFalse(protocol.blocked(issue(1, "status:ready")))

    def test_product_planning_runs_when_the_backlog_is_low_at_most_daily(self):
        policy = {"issue": 42, "ready_minimum": 3, "review_interval_hours": 24}
        proposal = ("status:ready", "origin:agent", "enhancement")
        stale = AT - timedelta(hours=25)
        issues = [issue(42, "status:blocked", updated=stale), issue(1, *proposal), issue(2, *proposal)]
        self.assertTrue(protocol.planning_due(issues, policy, AT))
        self.assertFalse(protocol.planning_due(issues + [issue(3, *proposal)], policy, AT))
        self.assertFalse(protocol.planning_due([issue(42, "status:blocked", updated=AT - timedelta(hours=2))], policy, AT))
        self.assertFalse(protocol.planning_due(issues[1:], policy, AT))
        # A blocked proposal or one without a dependency summary is not counted.
        blocked = issue(3, *proposal, blocked_by=1)
        unknown = issue(4, *proposal)
        unknown.pop("issue_dependencies_summary")
        self.assertEqual(protocol.ready_proposals(issues + [blocked, unknown], 42), 2)


if __name__ == "__main__":
    unittest.main()

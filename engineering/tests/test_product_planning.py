"""Original native backlog/recovery scheduling scenarios. Run only in CI."""
from copy import deepcopy
from datetime import timedelta
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import control
import lease
import policy
from test_organization import AT, held, issue, state

PLAN = {"issue": 42, "ready_minimum": 3, "ready_maximum": 5}


def anchor(status="blocked"):
    result = issue(42, status=status, origin="agent-system", priority="p3")
    result["updated_at"] = lease.stamp(AT - timedelta(minutes=1))
    return result


def product(number=39, blocked=0):
    result = issue(number, extra=("enhancement",), origin="agent")
    result["issue_dependencies_summary"] = {"blocked_by": blocked}
    return result


def published():
    result = state()
    result["checkpoint"]["publication"] = {"state": "published", "at": lease.stamp(AT)}
    return result


class ProductPlanning(unittest.TestCase):
    def choose(self, current=None, issues=None, at=AT):
        return control.select(current if current is not None else published(),
            issues if issues is not None else [anchor(), product()], at, PLAN)

    def test_valid_lease_and_expired_recovery_precede_planning(self):
        self.assertEqual(self.choose(held())["action"], "busy")
        self.assertEqual(self.choose(held(), at=AT + timedelta(hours=1))["action"], "recover")

    def test_incomplete_pr_or_rejected_review_precedes_new_product_discovery(self):
        for status in ["in-progress", "review"]:
            self.assertEqual(self.choose(issues=[anchor(), issue(status=status)])["action"], "recover")

    def test_three_unchanged_attempts_require_triage_before_planning(self):
        current = published()
        current["attempts"]["39"] = {"unchanged": 3}
        self.assertEqual(self.choose(current)["action"], "triage")

    def test_user_critical_and_security_work_preempt_planning(self):
        for work in [issue(7, priority="p3"), issue(8, origin="agent", priority="p0"),
                issue(9, origin="security", priority="p2")]:
            self.assertEqual(self.choose(issues=[anchor(), work])["issue"], work["number"])
        critical = product(10)
        critical["labels"].append({"name": "risk:critical"})
        self.assertEqual(self.choose(issues=[anchor(), critical])["issue"], 10)

    def test_new_publication_and_small_queue_select_exact_standing_role(self):
        selected = self.choose()
        self.assertEqual((selected["action"], selected["issue"], selected["role"]),
            ("plan-product", 42, "product"))
        self.assertEqual(selected["ready_product_items"], 1)

    def test_three_ready_products_continue_implementation_without_planning(self):
        selected = self.choose(issues=[anchor(), product(39), product(41), product(44)])
        self.assertEqual((selected["action"], selected["issue"]), ("work", 39))

    def test_native_blockers_unknown_dependencies_and_nonproduct_work_do_not_fill_queue(self):
        unknown = product(44)
        del unknown["issue_dependencies_summary"]
        maintenance = issue(45, extra=("enhancement",), origin="maintenance")
        selected = self.choose(issues=[anchor(), product(39), product(41, blocked=1),
            unknown, maintenance])
        self.assertEqual(selected["ready_product_items"], 1)

    def test_recorded_review_goes_idle_without_unchanged_repeat_analysis(self):
        reviewed = anchor()
        reviewed["updated_at"] = lease.stamp(AT)
        self.assertEqual(self.choose(issues=[reviewed])["action"], "idle")
        self.assertEqual(self.choose(issues=[reviewed, product()])["action"], "work")
        later = published()
        later["checkpoint"]["publication"]["at"] = lease.stamp(AT + timedelta(minutes=1))
        self.assertEqual(self.choose(later, [reviewed], AT + timedelta(minutes=2))["action"],
            "plan-product")

    def test_explicit_new_evidence_rearm_works_without_claiming_publication(self):
        for status in ["ready", "needs-triage"]:
            self.assertEqual(self.choose(state(), [anchor(status)])["action"], "plan-product")

    def test_unpublished_missing_malformed_or_future_evidence_stays_dormant(self):
        for evidence in [{}, {"state": "ci-passed", "at": lease.stamp(AT)},
                {"state": "published"}, {"state": "published", "at": "invalid"},
                {"state": "published", "at": lease.stamp(AT + timedelta(minutes=1))}]:
            current = state()
            current["checkpoint"]["publication"] = evidence
            self.assertEqual(self.choose(current, [anchor()])["action"], "idle")
        self.assertEqual(self.choose(issues=[])["action"], "idle")
        closed = anchor()
        closed["state"] = "closed"
        self.assertEqual(self.choose(issues=[closed])["action"], "idle")

    def test_selection_does_not_mutate_lease_checkpoint_or_native_issues(self):
        current, issues = published(), [anchor(), product()]
        original = deepcopy((current, issues))
        self.choose(current, issues)
        self.assertEqual((current, issues), original)

    def test_ci_policy_rejects_invalid_anchor_and_unbounded_proposal_limits(self):
        policy.check_product({"product_planning": PLAN}, {"product": "original-skill"})
        for bad in [dict(PLAN, issue=True), dict(PLAN, issue=0),
                dict(PLAN, ready_minimum=True), dict(PLAN, ready_minimum=0),
                dict(PLAN, ready_maximum=999)]:
            with self.assertRaises(AssertionError):
                policy.check_product({"product_planning": bad}, {"product": "original-skill"})
        with self.assertRaises(AssertionError):
            policy.check_product({"product_planning": PLAN}, {})


if __name__ == "__main__":
    unittest.main()

"""Original QA-gate waiting scenarios: poll cheaply only while CI runs; CI only."""
from pathlib import Path
import sys
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import lease
import qa
from test_organization import AT, HEAD, Native


class Clock:
    def __init__(self):
        self.now = 0.0
        self.sleeps = []

    def monotonic(self):
        return self.now

    def sleep(self, seconds):
        self.sleeps.append(seconds)
        self.now += seconds


class Running(Native):
    """Mynou CI stays in progress for a fixed number of native run listings."""
    def __init__(self, pending_listings=0):
        super().__init__()
        self.pending_listings = pending_listings
        self.listings = 0
        self.pull_reads = 0

    def pending(self):
        return self.listings <= self.pending_listings

    def rest(self, method, path, value=None):
        if path == "pulls/3":
            self.pull_reads += 1
        return super().rest(method, path, value)

    def pages(self, path, key=None):
        if path.startswith("actions/runs?head_sha="):
            self.listings += 1
            runs = super().pages(path, key)
            if self.pending():
                runs[0] = dict(runs[0], status="in_progress", conclusion=None)
            return runs
        jobs = super().pages(path, key)
        if path.startswith("actions/runs/20/jobs") and self.pending():
            return [dict(job, status="in_progress", conclusion=None) for job in jobs]
        return jobs


class GateWaiting(unittest.TestCase):
    def wait(self, native, seconds=600):
        clock = Clock()
        with patch.object(qa, "waiting", clock):
            return qa.wait_for_review(native, 3, seconds), clock

    def test_running_ci_is_polled_cheaply_then_fully_evaluated_once_more(self):
        native = Running(pending_listings=3)
        proof, clock = self.wait(native)
        self.assertEqual(proof["head"], HEAD)
        self.assertEqual(clock.sleeps, [30])
        # Two full evaluations and one head read; every other poll lists runs only.
        self.assertEqual(native.pull_reads, 3)
        self.assertEqual(native.listings, 5)

    def test_terminal_gate_failures_never_poll(self):
        for change in ("review", "merged", "failed-ci"):
            with self.subTest(change=change):
                native = Running()
                if change == "review":
                    native.reviews = []
                elif change == "merged":
                    native.current_pr.update(state="closed", merged_at=lease.stamp(AT))
                else:
                    native.bad_job = "validate"
                clock = Clock()
                with patch.object(qa, "waiting", clock), self.assertRaises(ValueError):
                    qa.wait_for_review(native, 3, 600)
                self.assertEqual(clock.sleeps, [])

    def test_ci_still_running_at_the_deadline_fails_visibly(self):
        native = Running(pending_listings=10 ** 6)
        clock = Clock()
        with patch.object(qa, "waiting", clock):
            with self.assertRaisesRegex(ValueError, "still running at the QA gate deadline"):
                qa.wait_for_review(native, 3, 600)
        self.assertEqual(len(clock.sleeps), 20)
        self.assertEqual(sum(clock.sleeps), 600)

    def test_running_ignores_the_gate_workflow_and_waits_for_missing_runs(self):
        class Empty(Native):
            def pages(self, path, key=None):
                if path.startswith("actions/runs?head_sha="):
                    return []
                return super().pages(path, key)
        self.assertEqual(qa.running(Empty(), Empty().cfg, HEAD), ["Mynou CI"])
        self.assertEqual(qa.running(Running(), Running().cfg, HEAD), [])


if __name__ == "__main__":
    unittest.main()

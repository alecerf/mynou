"""Original continuous-execution contracts; run in CI only."""
from copy import deepcopy
from datetime import timedelta
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import lease
import policy
from test_organization import AT, HEAD, held

CFG = {
    "execution_mode": "continuous", "max_active_agents": 1,
    "lease_minutes": 45, "heartbeat_minutes": 15, "no_progress_limit": 3,
}


class ContinuousExecution(unittest.TestCase):
    def test_continuous_has_no_deadline_but_retains_recovery_controls(self):
        policy.check_execution(CFG)
        for cfg in [
            dict(CFG, slice_minutes=40), dict(CFG, execution_mode="bounded"),
            dict(CFG, max_active_agents=2), dict(CFG, lease_minutes=999),
            dict(CFG, no_progress_limit=999),
        ]:
            with self.assertRaises(AssertionError):
                policy.check_execution(cfg)

    def test_heartbeat_must_be_positive_integer_and_at_most_fifteen_minutes(self):
        for value in [0, -1, 16, True, 15.0]:
            with self.assertRaises(AssertionError):
                policy.check_execution(dict(CFG, heartbeat_minutes=value))
        for value in [1, 15]:
            policy.check_execution(dict(CFG, heartbeat_minutes=value))

    def test_progress_after_forty_minutes_keeps_one_renewable_owner(self):
        current = held("quality")
        for minute in [15, 30, 45, 60]:
            current = lease.checkpoint(current, AT + timedelta(minutes=minute),
                "original-owner", "Useful remote progress", "Next ready transition")
            self.assertTrue(lease.valid(current, AT + timedelta(minutes=minute)))
            with self.assertRaises(ValueError):
                lease.acquire(current, AT + timedelta(minutes=minute), "other",
                    "other-worker", "master", 1, "work/original", HEAD, 3)
        stopped = lease.release(current, AT + timedelta(minutes=61), "original-owner")
        qa_role = lease.acquire(stopped, AT + timedelta(minutes=61), "qa-owner",
            "one-worker", "qa", 1, "work/original", HEAD, 3)
        self.assertEqual(qa_role["lease"]["role"], "qa")
        self.assertEqual(qa_role["checkpoint"]["commit"], HEAD)

    def test_continuous_does_not_resurrect_expired_or_foreign_ownership(self):
        original = held("quality")
        for at, identity in [
            (AT + timedelta(minutes=45), "original-owner"),
            (AT + timedelta(minutes=1), "foreign-owner"),
        ]:
            snapshot = deepcopy(original)
            with self.assertRaises(ValueError):
                lease.checkpoint(original, at, identity, "No progress", "Stop")
            self.assertEqual(original, snapshot)


if __name__ == "__main__":
    unittest.main()

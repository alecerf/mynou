"""Organization configuration contracts; CI only."""
from copy import deepcopy
import json
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import policy

ROOT = Path(__file__).resolve().parents[2]


class Configuration(unittest.TestCase):
    def setUp(self):
        self.cfg = json.loads((ROOT / ".github/engineering.json").read_text())

    def test_repository_configuration_is_valid(self):
        policy.check_config(self.cfg)
        policy.check_workflows(ROOT)

    def test_invalid_coordination_or_release_settings_are_rejected(self):
        changes = [
            ("schema", 1), ("default_branch", "main"), ("trusted_actors", {}),
            ("trusted_actors", {"owner": "7"}), ("trusted_actors", {"bad login!": 7}),
            ("claim_ttl_minutes", 10), ("claim_ttl_minutes", 120.0),
        ]
        for key, value in changes:
            cfg = deepcopy(self.cfg)
            cfg[key] = value
            with self.subTest(key=key, value=value), self.assertRaises(AssertionError):
                policy.check_config(cfg)
        for section, key, value in [("release_policy", "minimum_interval_days", 0),
                                    ("release_policy", "minimum_interval_days", True),
                                    ("release_policy", "urgent_label", "hotfix"),
                                    ("product_planning", "ready_minimum", 4),
                                    ("product_planning", "review_interval_hours", 0)]:
            cfg = deepcopy(self.cfg)
            cfg[section][key] = value
            with self.subTest(section=section, key=key, value=value), self.assertRaises(AssertionError):
                policy.check_config(cfg)


if __name__ == "__main__":
    unittest.main()

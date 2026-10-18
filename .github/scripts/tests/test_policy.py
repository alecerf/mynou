"""Repository release configuration and workflow guards."""
from copy import deepcopy
import json
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import policy

ROOT = Path(__file__).resolve().parents[3]


class Configuration(unittest.TestCase):
    def setUp(self):
        self.cfg = json.loads((ROOT / '.github/release-policy.json').read_text())

    def test_repository_configuration_and_workflows_are_valid(self):
        policy.check_config(self.cfg)
        policy.check_workflows(ROOT)

    def test_invalid_release_settings_are_rejected(self):
        for key, value in [('schema', 2), ('default_branch', 'main'), ('repository', '../invalid')]:
            cfg = deepcopy(self.cfg)
            cfg[key] = value
            with self.subTest(key=key, value=value), self.assertRaises(AssertionError):
                policy.check_config(cfg)
        for key, value in [('minimum_interval_days', 0), ('minimum_interval_days', True), ('urgent_label', 'hotfix')]:
            cfg = deepcopy(self.cfg)
            cfg['release_policy'][key] = value
            with self.subTest(key=key, value=value), self.assertRaises(AssertionError):
                policy.check_config(cfg)

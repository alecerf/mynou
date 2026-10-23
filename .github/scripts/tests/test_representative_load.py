"""Reject incomplete, mismatched or falsely successful load evidence."""
import importlib.util
import json
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).parents[1]))
spec = importlib.util.spec_from_file_location("representative_load", Path(__file__).parents[1] / "representative_load.py")
load = importlib.util.module_from_spec(spec)
spec.loader.exec_module(load)


class Evidence(unittest.TestCase):
    def report(self, **changes):
        row = dict(retained_initial="500", elapsed_us="600000000", rounds="2",
                   active_seed_torrents="8", concurrent_acquisitions="2", retained_final="504",
                   uploaded_payload_bytes=str(16 << 20), downloaded_payload_bytes=str(4 << 20),
                   restart_preserved_pause_priority_payload=True)
        row.update(changes)
        return "MYNOU_REPRESENTATIVE_LOAD " + json.dumps(row) + "\ntest result: ok. 1 passed; 0 failed;\n"

    def test_accepts_only_the_requested_observed_workload(self):
        self.assertEqual(load.measurement(self.report(), 500, 600)["rounds"], "2")
        for change in [dict(retained_initial="16"), dict(elapsed_us="1000000"),
                       dict(rounds="0"), dict(retained_final="500"),
                       dict(uploaded_payload_bytes="0"), dict(downloaded_payload_bytes="0"),
                       dict(restart_preserved_pause_priority_payload=False)]:
            with self.subTest(change=change), self.assertRaises(ValueError):
                load.measurement(self.report(**change), 500, 600)

    def test_missing_duplicate_or_failed_runs_cannot_pass(self):
        for text in ["", self.report() * 2, self.report().replace("1 passed; 0 failed", "0 passed; 1 failed")]:
            with self.subTest(text=text), self.assertRaises(ValueError):
                load.measurement(text, 500, 600)


if __name__ == "__main__":
    unittest.main()

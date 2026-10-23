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
                   download_queue_limit="2", seed_queue_limit="0",
                   uploaded_payload_bytes=str(16 << 20), downloaded_payload_bytes=str(4 << 20),
                   restart_preserved_pause_priority_payload=True)
        row.update(changes)
        return "MYNOU_REPRESENTATIVE_LOAD " + json.dumps(row) + "\ntest result: ok. 1 passed; 0 failed;\n"

    def test_accepts_only_the_requested_observed_workload(self):
        self.assertEqual(load.measurement(self.report(), 500, 600)["rounds"], "2")
        for change in [dict(retained_initial="16"), dict(elapsed_us="1000000"),
                       dict(rounds="0"), dict(retained_final="500"), dict(download_queue_limit="0"), dict(seed_queue_limit="8"),
                       dict(uploaded_payload_bytes="0"), dict(downloaded_payload_bytes="0"),
                       dict(restart_preserved_pause_priority_payload=False)]:
            with self.subTest(change=change), self.assertRaises(ValueError):
                load.measurement(self.report(**change), 500, 600)

    def test_full_engine_requires_observed_automation_and_incidents(self):
        fields = dict(full_engine=True, retained_final="505", api_samples="40", api_p95_us="1000",
                      api_max_us="2000", imports="4", import_max_us="3000", in_flight_restarts="1",
                      port_changes="1", generation_changes="1", natpmp_requests="8",
                      protected_source_requests="4", catalog_requests="4", cross_seed_added="1",
                      monitoring_samples="4", monitoring_lag_max_us="1000000", protected_feed_successes="2")
        self.assertTrue(load.measurement(self.report(**fields), 500, 600, engine=True)["full_engine"])
        for change in [dict(full_engine=False), dict(api_samples="0"), dict(imports="3"),
                       dict(in_flight_restarts="0"), dict(port_changes="0"), dict(generation_changes="0"),
                       dict(protected_source_requests="0"), dict(cross_seed_added="0"), dict(monitoring_samples="3"), dict(protected_feed_successes="0")]:
            with self.subTest(change=change), self.assertRaises(ValueError):
                load.measurement(self.report(**(fields | change)), 500, 600, engine=True)
        with self.assertRaises(ValueError):
            load.measurement(self.report(), 500, 600, engine=True)

    def test_missing_duplicate_or_failed_runs_cannot_pass(self):
        for text in ["", self.report() * 2, self.report().replace("1 passed; 0 failed", "0 passed; 1 failed")]:
            with self.subTest(text=text), self.assertRaises(ValueError):
                load.measurement(text, 500, 600)


if __name__ == "__main__":
    unittest.main()

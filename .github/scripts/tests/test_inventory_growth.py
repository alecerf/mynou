"""The BSD time RSS field is bytes, and missing measurements must fail closed."""
import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location("inventory_growth", Path(__file__).parents[1] / "inventory_growth.py")
growth = importlib.util.module_from_spec(spec)
spec.loader.exec_module(growth)


class Measurements(unittest.TestCase):
    def test_bsd_rss_bytes_are_kept_exactly(self):
        self.assertEqual(growth.peak_bytes("  0.31 real\n   12345678  maximum resident set size\n"), 12345678)

    def test_missing_or_non_numeric_rss_is_rejected(self):
        for text in ("", "Maximum resident set size (kbytes): 12500", "n/a maximum resident set size"):
            with self.subTest(text=text), self.assertRaises(ValueError):
                growth.peak_bytes(text)


if __name__ == "__main__":
    unittest.main()

import csv
import hashlib
import io
import json
import math
from pathlib import Path
import subprocess
import sys
import unittest

from measure import measure


HERE = Path(__file__).resolve().parent
META = HERE / "example-meta.json"
TICKS = HERE / "example-ticks.csv"


class PhaseBMeasurements(unittest.TestCase):
    def setUp(self):
        self.meta = json.loads(META.read_bytes())
        reader = csv.DictReader(io.StringIO(TICKS.read_text(encoding="utf-8")))
        self.columns = reader.fieldnames
        self.rows = list(reader)

    def run_measurement(self):
        table = io.StringIO()
        writer = csv.DictWriter(table, self.columns)
        writer.writeheader()
        writer.writerows(self.rows)
        return measure(json.dumps(self.meta).encode(), table.getvalue().encode())

    def test_five_ticks_match_the_analytic_reference(self):
        report = self.run_measurement()
        self.assertEqual(report["origin"]["kind"], "test_only")
        self.assertEqual(report["passport_missing"], [])
        self.assertEqual(report["coverage"], {
            "rows": 5, "first_tick": 1, "last_tick": 5, "missing_ticks": 0,
            "incomplete_rows": 5, "steady_expected": 3, "steady_rows": 3,
        })
        timings = report["timings_ns"]
        self.assertEqual(timings["jitter"], {
            "count": 4, "median": 0, "p95": 2_000_000, "p99": 2_000_000, "max": 2_000_000,
        })
        for metric, ns in {
            "ab_read_shift": 3_000_000, "cycle_to_ack": 9_000_000,
            "read_a_to_ack": 8_000_000, "send_to_ack": 2_000_000,
        }.items():
            self.assertEqual(timings[metric], {
                "count": 5, "median": ns, "p95": ns, "p99": ns, "max": ns,
            })
        self.assertEqual(timings["sample_to_send"], {
            "count": 0, "median": None, "p95": None, "p99": None, "max": None,
        })
        power = report["power_w"]
        self.assertEqual(power["measured_total"]["count"], 3)
        self.assertAlmostEqual(power["measured_total"]["mean"], 1.0)
        self.assertEqual(power["reference_error"]["count"], 3)
        self.assertAlmostEqual(power["reference_error"]["mean_signed"], -0.01)
        self.assertAlmostEqual(power["reference_error"]["max_abs"], 0.02)
        self.assertEqual(power["target_error"]["count"], 3)
        self.assertAlmostEqual(power["target_error"]["mean_signed"], 0.0)
        self.assertAlmostEqual(power["target_error"]["max_abs"], 0.01)
        self.assertEqual(report["settling"], {
            "status": "settled", "tick": 3, "elapsed_ns": 99_000_000, "reason": None,
        })

    def test_conversion_times_only_count_in_pairs(self):
        self.rows[0].update(a_sample_ns="1000000", b_sample_ns="4000000")
        self.rows[1]["a_sample_ns"] = "100000000"
        metric = self.run_measurement()["timings_ns"]["sample_to_send"]
        self.assertEqual(metric["count"], 1)
        self.assertEqual(metric["p99"], 7_000_000)

    def test_missing_reference_and_passport_remain_visible(self):
        for row in self.rows:
            row["reference_w"] = ""
        self.meta["passport"]["power_reference"] = None
        del self.meta["passport"]["host"]
        report = self.run_measurement()
        self.assertEqual(report["passport_missing"], ["host", "power_reference"])
        self.assertEqual(report["power_w"]["reference_error"], {
            "count": 0, "mean_signed": None, "max_abs": None,
        })
        self.assertEqual(report["power_w"]["target_error"]["count"], 3)

    def test_gap_and_incomplete_horizon_are_not_filled(self):
        del self.rows[3]
        report = self.run_measurement()
        self.assertEqual(report["coverage"]["missing_ticks"], 1)
        self.assertEqual(report["coverage"]["steady_rows"], 2)
        self.assertEqual(report["timings_ns"]["jitter"]["count"], 2)
        self.assertEqual(report["settling"]["status"], "insufficient")
        del self.rows[-1]
        self.assertEqual(self.run_measurement()["settling"]["status"], "insufficient")

    def test_measured_settling_must_hold_through_the_last_tick(self):
        self.rows[3]["b_current_a"] = "0.06"
        report = self.run_measurement()
        self.assertEqual(report["settling"]["tick"], 5)
        self.assertEqual(report["settling"]["elapsed_ns"], 300_000_000)
        self.rows[-1]["b_current_a"] = "0.06"
        self.assertEqual(self.run_measurement()["settling"]["status"], "not_settled")
        self.rows[-1]["b_current_a"] = ""
        self.assertEqual(self.run_measurement()["settling"]["status"], "insufficient")

    def test_settling_band_includes_both_boundaries(self):
        for power, expected in (
            (0.98, "settled"),
            (1.02, "settled"),
            (math.nextafter(0.98, 0.0), "not_settled"),
            (math.nextafter(1.02, math.inf), "not_settled"),
        ):
            with self.subTest(power=power):
                for row in self.rows[2:]:
                    row.update(a_current_a="0", b_voltage_v="1", b_current_a=repr(power))
                result = self.run_measurement()["settling"]
                self.assertEqual(result["status"], expected)
                if expected == "settled":
                    self.assertEqual(result["tick"], 3)
                    self.assertEqual(result["elapsed_ns"], 99_000_000)

    def test_settling_needs_ack_fixed_target_and_post_step_observations(self):
        cases = (
            (1, "command_ack_ns", "", "step_ack_or_positive_target_missing"),
            (1, "target_w", "0", "step_ack_or_positive_target_missing"),
            (3, "target_w", "1.2", "target_changed_or_missing"),
            (2, "a_sample_ns", "109000000", "measurement_before_step_ack"),
        )
        for index, field, value, reason in cases:
            with self.subTest(field=field, reason=reason):
                original = self.rows[index][field]
                self.rows[index][field] = value
                self.assertEqual(self.run_measurement()["settling"], {
                    "status": "insufficient", "tick": None, "elapsed_ns": None, "reason": reason,
                })
                self.rows[index][field] = original

    def test_malformed_ticks_and_order_errors_identify_the_row(self):
        for field, invalid in {
            "tick": "1", "started_ns": "0", "b_read_started_ns": "103000000",
            "a_sample_ns": "105000000", "a_voltage_v": "NaN",
        }.items():
            with self.subTest(field=field):
                old_value = self.rows[1][field]
                self.rows[1][field] = invalid
                with self.assertRaisesRegex(ValueError, rf"CSV.*3.*{field}"):
                    self.run_measurement()
                self.rows[1][field] = old_value

    def test_cvtl_origin_requires_both_provenance_hashes(self):
        self.meta["origin"]["kind"] = "cvtl"
        with self.assertRaisesRegex(ValueError, "origin.raw_sha256"):
            self.run_measurement()
        self.meta["origin"].update(raw_sha256="a" * 64, export_sha256="b" * 64)
        report = self.run_measurement()
        self.assertEqual(report["origin"]["raw_sha256"], "a" * 64)
        self.assertEqual(report["origin"]["export_sha256"], "b" * 64)
        self.assertNotEqual(report["input_sha256"]["csv"], "b" * 64)

    def test_cli_bytes_are_repeatable_and_inputs_are_unchanged(self):
        before = [path.read_bytes() for path in (META, TICKS)]
        command = [sys.executable, str(HERE / "measure.py"), str(META), str(TICKS)]
        first = subprocess.check_output(command)
        self.assertEqual(subprocess.check_output(command), first)
        self.assertEqual([path.read_bytes() for path in (META, TICKS)], before)
        self.assertEqual(json.loads(first)["input_sha256"], {
            name: hashlib.sha256(payload).hexdigest()
            for name, payload in zip(("meta", "csv"), before)
        })


if __name__ == "__main__":
    unittest.main()

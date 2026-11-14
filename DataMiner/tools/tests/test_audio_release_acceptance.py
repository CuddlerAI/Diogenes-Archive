import copy
import importlib.util
import unittest
from pathlib import Path


MODULE_PATH = Path(__file__).resolve().parents[1] / "audio_release_acceptance.py"
SPEC = importlib.util.spec_from_file_location("audio_release_acceptance", MODULE_PATH)
assert SPEC and SPEC.loader
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


def report(index: int, *, offset: float = 80_000.0):
    return {
        "run_id": f"run-{index}",
        "passed": True,
        "integrity_passed": True,
        "capture_queue_drops": 0,
        "windows": {"major": 10, "minor": 0, "build": 22621},
        "adapter": {
            "vendor_id": 0x10DE,
            "device_id": 0x1C03,
            "luid_high": 0,
            "luid_low": 42,
            "driver_version": "32.0.15.8132",
        },
        "profile": {"fps_num": 30, "fps_den": 1},
        "audio_calibration": {
            "state": "ready",
            "source": "process_loopback",
            "format": dict(MODULE.EXACT_AUDIO_FORMAT),
            "constant_offset_us": offset,
            "single_run_timing_passed": True,
            "residual_absolute_error_us": {"p95": 5_000.0, "max": 10_000.0},
            "health": {
                "packets": 100,
                "frames": 48_000,
                "discontinuities": 0,
                "timestamp_errors": 0,
                "queue_overflows": 0,
                "nonzero_device_positions": 0,
                "terminal_error": None,
            },
        },
    }


class AudioReleaseAcceptanceTests(unittest.TestCase):
    def test_accepts_five_stable_runs_on_the_required_machine(self):
        reports = [report(index, offset=80_000.0 + index * 100.0) for index in range(5)]
        verdict = MODULE.evaluate(reports, required_build=22621)
        self.assertTrue(verdict["passed"], verdict["failures"])

    def test_rejects_unstable_cohort_and_audio_health_failure(self):
        reports = [report(index) for index in range(5)]
        reports[-1] = copy.deepcopy(reports[-1])
        reports[-1]["audio_calibration"]["constant_offset_us"] = 120_000.0
        reports[-1]["audio_calibration"]["health"]["queue_overflows"] = 1
        verdict = MODULE.evaluate(reports, required_build=22621)
        self.assertFalse(verdict["passed"])
        self.assertTrue(any("queue_overflows" in failure for failure in verdict["failures"]))
        self.assertTrue(any("cohort-corrected" in failure for failure in verdict["failures"]))

    def test_rejects_mixed_machines_and_too_few_runs(self):
        reports = [report(index) for index in range(4)]
        reports[-1]["adapter"]["driver_version"] = "different"
        verdict = MODULE.evaluate(reports)
        self.assertFalse(verdict["passed"])
        self.assertIn("reports must come from one OS/GPU/driver tuple", verdict["failures"])
        self.assertTrue(any("at least 5" in failure for failure in verdict["failures"]))


if __name__ == "__main__":
    unittest.main()

import copy
import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location("arm_performance", Path(__file__).resolve().parents[1] / "Summarize-ArmPerformance.py")
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


def count(calls, total, maximum):
    return {"calls": calls, "total_us": total, "max_us": maximum}


def fixture():
    phases = {}
    for i, (calls, total) in enumerate([(2, 20), (4, 60), (10, 240)]):
        phases[module.PHASES[i]] = {"qpc": (i+1)*1000, "present_seq": i+1,
            "hook_timing": {"selected_draw": count(calls, total, 120)}, "feedback_timing": {}}
    return {"schema": "dm-cp2077-raw-selected-summary/v2", "non_trainable": True,
            "process_launch_id": "synthetic", "sealed": True, "fault": None,
            "performance": {"schema": module.PERFORMANCE_SCHEMA, "non_trainable": True,
                "acceptance": False, "qpc_frequency": 1000, "checkpoints": phases,
                "feedback_timing_at_report": {}}}


class ArmPerformanceTests(unittest.TestCase):
    def test_phase_uses_deltas_never_subtracts_maxima(self):
        result = module.summarize(fixture())
        row = result["windows"]["source_armed_to_selected_arm"]["hook_timing"]["selected_draw"]
        self.assertEqual((row["calls"], row["total_us"], row["mean_us"]), (2, 40, 20))
        self.assertIsNone(row["window_max_us"])
        self.assertEqual(row["cumulative_max_us_at_end"], 120)
        self.assertFalse(result["acceptance"])
        self.assertFalse(result["actual_game_fps_computed"])

    def test_old_or_failed_run_is_missing_not_zero_or_success(self):
        source = fixture()
        del source["performance"]
        source["sealed"], source["fault"] = False, "failure kept"
        result = module.summarize(source)
        self.assertFalse(result["phase_measurements_available"])
        self.assertEqual(result["source_fault"], "failure kept")
        self.assertEqual(result["windows"], {})
        source = fixture()
        source["performance"]["checkpoints"]["selected_seal"] = None
        result = module.summarize(source)
        self.assertFalse(result["windows"]["selected_arm_to_selected_seal"]["available"])

    def test_regressions_and_nontransactional_samples_do_not_invent_means(self):
        for a, b in [(count(5, 9, 5), count(4, 9, 5)),
                     (count(1, 10, 10), count(2, 9, 10)),
                     (count(1, 10, 10), count(1, 11, 10))]:
            result = module.difference(a, b)
            self.assertFalse(result["available"])
            self.assertIsNone(result["mean_us"])
        self.assertIsNone(module.difference(count(0, 0, 0), count(0, 0, 0))["mean_us"])
        self.assertFalse(module.difference(None, count(2, 4, 2))["available"])

    def test_unknown_schema_invalid_clocks_or_cross_launch_metadata_are_rejected(self):
        for change in [lambda p: p.update(schema="future"),
                       lambda p: p.update(qpc_frequency=0),
                       lambda p: p["checkpoints"]["selected_arm"].update(qpc=1)]:
            source = fixture()
            change(source["performance"])
            with self.assertRaises(ValueError):
                module.summarize(source)
        camera = {"schema": "dm-cp2077-render-camera-observation/v3", "non_trainable": True,
                  "process_launch_id": "other", "observation": {}}
        with self.assertRaises(ValueError):
            module.summarize(fixture(), camera)
        camera["process_launch_id"] = "synthetic"
        result = module.summarize(fixture(), camera)
        self.assertIsNone(result["gpuva_ownership_lifetime"]["hit_fraction"])
        bad = copy.deepcopy(camera)
        bad["observation"]["gpuva_ownership_cache_hits"] = True
        with self.assertRaises(ValueError):
            module.summarize(fixture(), bad)


if __name__ == "__main__":
    unittest.main()

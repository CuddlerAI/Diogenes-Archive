import copy
import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location("draw_pass", Path(__file__).resolve().parents[1] / "Summarize-DrawPass.py")
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


def draw(m22, context=None):
    matrix = [[0.0] * 4 for _ in range(4)]
    matrix[2][2] = m22
    return {"matrices": [{"matrix_kind": "projection", "matrix": matrix}],
            "pass_context": context, "pipeline_depth_enable": True,
            "pipeline_depth_write": True}


def report(draws):
    return {"schema": "dm-cp2077-render-camera-observation/v2", "non_trainable": True,
            "observation": {"observations": draws}}


class DrawPassTests(unittest.TestCase):
    def test_legacy_missing_context_is_unknown_not_writable(self):
        result = module.summarize(report([draw(-5.00679e-6)]))
        hist = result["groups"]["A_far_about_3995"]["histograms"]
        self.assertEqual(hist["pipeline_depth_write"], {"true": 1})
        self.assertEqual(hist["dsv_read_only_depth"], {"unknown": 1})
        self.assertEqual(hist["depth_comparison"], {"unknown": 1})
        self.assertFalse(result["pixel_write_proven"])

    def test_same_draws_different_dsv_flags_remain_distinct(self):
        context = {"schema": "dm-d3d12-draw-pass/v1", "non_trainable": True,
                   "dsv_flags": 0, "pipeline": {"depth_comparison": 7},
                   "scissor": {"count": 2, "single_rect": None}}
        readonly = copy.deepcopy(context)
        readonly["dsv_flags"] = 1
        rows = [draw(-1.1920929e-6, context), draw(-1.1920929e-6, readonly)]
        result = module.summarize(report(rows))
        hist = result["groups"]["B_far_about_16777"]["histograms"]
        self.assertEqual(hist["dsv_read_only_depth"], {"false": 1, "true": 1})
        self.assertEqual(len(result["draws"]), 2)
        self.assertEqual(result["draws"][0]["pass_context"]["scissor"]["single_rect"], None)
        self.assertFalse(result["camera_binding_resolved"])

    def test_multiple_projection_candidates_do_not_choose_one(self):
        mixed = draw(-5.00679e-6)
        mixed["matrices"].extend(draw(-1.1920929e-6)["matrices"])
        result = module.summarize(report([mixed, draw(-0.0001)]))
        self.assertEqual(result["groups"]["multiple_projections"]["draws"], 1)
        self.assertEqual(result["groups"]["other_projection"]["draws"], 1)

    def test_unknown_schema_rejected(self):
        with self.assertRaises(ValueError):
            module.summarize(report([draw(0.0, {"schema": "future"})]))


if __name__ == "__main__":
    unittest.main()

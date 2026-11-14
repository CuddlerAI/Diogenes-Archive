from __future__ import annotations

import copy
import unittest

import numpy as np

from diogenes_wm.eval.cp2077_egeo_gate import Thresholds
from diogenes_wm.eval.cp2077_selected_geometry import (
    decode_watermark, evaluate, measure_pair, pair_populations, project,
)
from diogenes_wm.eval.cp2077_selected_inputs import (
    before_copy, join_frames, matrices_for_depth, selection_stride, validate_even_cfr_metadata,
)


class SelectedGeometryTests(unittest.TestCase):
    def test_validity_gate_measures_observed_projection_not_wrong_control_intersection(self):
        h, w = 360, 640
        projection = np.array([[1.,0,0,0],[0,1.,0,0],[0,0,0,1.],[0,0,.02,0]])
        left = {"view": np.eye(4), "projection": projection}
        right = {"view": np.eye(4), "projection": projection}
        right["view"][3,0] = .4
        gray = np.tile(np.arange(w, dtype=float), (h,1)) % 255
        result = measure_pair(gray, gray, np.full((h,w), .02), left, right)
        # Z=1, observed X shift=128px. Wrong inverse/half-depth controls shrink the common
        # support much further; that does not mean the observed camera lost those pixels.
        yy, xx = np.mgrid[16:h-16:4,16:w-16:4]
        observed = (xx+128 < w-16) & ~((xx < 256) & (yy < 32))
        expected = observed.sum()/((h//4)*(w//4))
        self.assertAlmostEqual(result["valid_fraction"], expected)
        self.assertGreater(result["valid_fraction"], .5)
        self.assertLess(result["common_valid_fraction"], .5)

    def test_native_projection_identity_jitter_and_known_translation(self):
        p = np.array([[1., 0, 0, 0], [0, 2., 0, 0], [.001, -.002, -1e-6, 1.], [0, 0, .02, 0]])
        x, y, d = np.array([400., 1100.]), np.array([300., 600.]), np.array([.002, .001])
        u, v, good = project(x, y, d, p, p, np.eye(4), 1920, 1080)
        np.testing.assert_allclose(u, x)
        np.testing.assert_allclose(v, y)
        self.assertTrue(good.all())
        motion = np.eye(4)
        motion[3, 0] = .1
        jitter = p.copy()
        jitter[2, 0] += .003
        jitter[2, 1] -= .004
        u, v, _ = project(x, y, d, p, jitter, motion, 1920, 1080)
        z = p[3, 2]/(d-p[2, 2])
        np.testing.assert_allclose(u, x + 960*.1/z + 960*.003)
        np.testing.assert_allclose(v, y + 540*.004)

    def test_watermark_crc_and_ambiguous_blocks_are_not_accepted(self):
        seq = 65530
        crc = 0
        for byte in seq.to_bytes(2, "big"):
            crc ^= byte
            for _ in range(8):
                crc = ((crc << 1)^7) & 255 if crc & 128 else (crc << 1) & 255
        bits = [(crc ^ 170) >> (7-i) & 1 for i in range(8)] + [seq >> (15-i) & 1 for i in range(16)]
        image = np.zeros((8, 192), dtype=np.uint8)
        for i, bit in enumerate(bits):
            image[:, i*8:(i+1)*8] = 255*bit
        self.assertEqual(decode_watermark(image), seq)
        image[:, :8] = 128
        with self.assertRaises(ValueError):
            decode_watermark(image)
        image[:, :8] = 255*(1-bits[0])
        with self.assertRaises(ValueError):
            decode_watermark(image)

    def test_exact_join_keeps_rgb_aliases_and_rejects_missing_or_early_depth(self):
        depth = [{"ordinal": 0, "present_seq": 10, "present_qpc": 100},
                 {"ordinal": 1, "present_seq": 12, "present_qpc": 120}]
        frames = [{"session_frame": i, "present_seq": seq, "source_qpc": qpc, "duplicate": duplicate}
                  for i, (seq, qpc, duplicate) in enumerate([(10, 101, False), (10, 101, True), (12, 125, False)])]
        self.assertEqual(join_frames(frames, depth), {10: 0, 12: 2})
        for key, value in (("present_seq", 11), ("source_qpc", 99)):
            bad = copy.deepcopy(frames)
            bad[0][key] = value
            with self.assertRaises(ValueError):
                join_frames(bad, depth)
        with self.assertRaises(ValueError):
            join_frames(frames[:2], depth)

    def test_even_cfr_policy_is_explicit_and_legacy_keeps_every_rgb_frame(self):
        self.assertEqual(selection_stride({"schema": "dm-cp2077-raw-selected-summary/v1"}), 1)
        summary = {"schema": "dm-cp2077-raw-selected-summary/v2", "feedback_schema": 4,
                   "depth_selection": "even_cfr_30fps/v1", "generation": 1, "resource_epoch": 1}
        self.assertEqual(selection_stride(summary), 2)
        for key, value in (("schema", "dm-cp2077-raw-selected-summary/v3"),
                           ("feedback_schema", 3), ("depth_selection", None),
                           ("generation", 2), ("resource_epoch", 2)):
            with self.subTest(key=key), self.assertRaises(ValueError):
                selection_stride(dict(summary, **{key: value}))

    def test_even_cfr_inheritance_uses_first_required_slot_and_keeps_aliases(self):
        depth = [{"ordinal": 0, "present_seq": 10, "present_qpc": 100},
                 {"ordinal": 1, "present_seq": 12, "present_qpc": 120}]
        frames = [{"session_frame": i, "present_seq": seq, "source_qpc": qpc, "duplicate": dup}
                  for i, (seq, qpc, dup) in enumerate([
                      (10, 101, False), (12, 125, False), (12, 125, True),
                      (12, 125, True), (12, 125, True), (14, 141, False)])]
        # Present 12 first appears in odd slot 1, but its first depth reference is slot 2.
        # The final odd Present 14 is never required; it must not need a depth row.
        self.assertEqual(join_frames(frames, depth, depth_stride=2), {10: 0, 12: 2})
        with self.assertRaises(ValueError):
            join_frames(frames, depth)
        extra = depth + [{"ordinal": 2, "present_seq": 14, "present_qpc": 140}]
        with self.assertRaises(ValueError):
            join_frames(frames, extra, depth_stride=2)

    def test_even_cfr_rejects_missing_terminal_required_slot_or_early_rgb(self):
        depth = [{"ordinal": 0, "present_seq": 10, "present_qpc": 100},
                 {"ordinal": 1, "present_seq": 12, "present_qpc": 120}]
        frames = [{"session_frame": i, "present_seq": seq, "source_qpc": seq*10+1, "duplicate": False}
                  for i, seq in enumerate([10, 11, 12, 13, 14])]
        with self.assertRaises(ValueError):
            join_frames(frames, depth, depth_stride=2)
        self.assertEqual(join_frames(frames[:3], depth, depth_stride=2), {10: 0, 12: 2})
        early = copy.deepcopy(frames[:3])
        early[2]["source_qpc"] = 119
        with self.assertRaises(ValueError):
            join_frames(early, depth, depth_stride=2)

    def test_even_cfr_still_validates_skipped_slot_identities(self):
        depth = [{"ordinal": 0, "present_seq": 10, "present_qpc": 100},
                 {"ordinal": 1, "present_seq": 12, "present_qpc": 120}]
        frames = [{"session_frame": i, "present_seq": seq, "source_qpc": seq*10+1, "duplicate": False}
                  for i, seq in enumerate([10, 11, 12])]
        for key, value in (("session_frame", 2), ("present_seq", 9), ("source_qpc", 99),
                           ("duplicate", True)):
            bad = copy.deepcopy(frames)
            bad[1][key] = value
            with self.subTest(key=key), self.assertRaises(ValueError):
                join_frames(bad, depth, depth_stride=2)

    def test_even_cfr_rejects_duplicate_with_a_different_source_timestamp(self):
        depth = [{"ordinal": 0, "present_seq": 10, "present_qpc": 100},
                 {"ordinal": 1, "present_seq": 12, "present_qpc": 120}]
        frames = [{"session_frame": i, "present_seq": seq, "source_qpc": qpc, "duplicate": dup}
                  for i, (seq, qpc, dup) in enumerate([(10, 101, False), (12, 125, False), (12, 126, True)])]
        with self.assertRaisesRegex(ValueError, "duplicate changed source QPC"):
            join_frames(frames, depth, depth_stride=2)

    def test_even_cfr_metadata_binds_30_fps_frame_count_and_recorded_run(self):
        launch = "0123456789abcdef0123456789abcdef"
        summary = {"schema": "dm-cp2077-raw-selected-summary/v2", "feedback_schema": 4,
                   "depth_selection": "even_cfr_30fps/v1", "generation": 1, "resource_epoch": 1,
                   "process_launch_id": launch}
        arm = {"schema": "dm-cp2077-raw-depth-demo-arm-request/v2", "process_launch_id": launch,
               "capture_contract": "issue735_local", "duration_ms": 64000, "target_depth_hz": 15,
               "manual_stop": False, "selected_present": {"schema": "dm-cp2077-raw-selected-request/v2",
                   "feedback_schema": 4, "depth_selection": "even_cfr_30fps/v1", "holstered": True,
                   "non_trainable": True, "bootstrap": {"resource_epoch": 1}}}
        name = f"rgb-recorder/raw-rgb-{launch}/runs/r0000/segments/00000/frames.jsonl.zst"
        meta = {"width": 1920, "height": 1080, "fps_num": 30, "fps_den": 1, "n_frames": 1920}
        validate_even_cfr_metadata(summary, arm, name, meta, 1920)
        for key, value in (("fps_num", 15), ("fps_den", 2), ("width", 1280), ("n_frames", 1919)):
            with self.subTest(key=key), self.assertRaises(ValueError):
                validate_even_cfr_metadata(summary, arm, name, dict(meta, **{key: value}), 1920)
        for bad_name in (name.replace("r0000", "r0001"), name.replace(launch, "a"*32)):
            with self.assertRaises(ValueError):
                validate_even_cfr_metadata(summary, arm, bad_name, meta, 1920)
        for key, value in (("target_depth_hz", 16), ("schema", "dm-cp2077-raw-depth-demo-arm-request/v1"),
                           ("manual_stop", None), ("process_launch_id", "a"*32)):
            with self.subTest(key=key), self.assertRaises(ValueError):
                validate_even_cfr_metadata(summary, dict(arm, **{key: value}), name, meta, 1920)

    def test_population_uses_actual_motion_and_never_crosses_sessions(self):
        def run(movement):
            rows = []
            for i in range(12):
                v = np.eye(4)
                v[3, 0] = i*movement
                rows.append({"present_qpc": 100+i*10, "view": v})
            return {"frequency": 100, "samples": rows}
        populations = pair_populations([run(0), run(.1)], 1, Thresholds())
        self.assertTrue(populations["static"])
        self.assertTrue(populations["translation"])
        self.assertFalse(populations["rotation"])
        self.assertTrue(all(s == 0 for s, _, _ in populations["static"]))
        self.assertTrue(all(s == 1 for s, _, _ in populations["translation"]))

    def test_missing_population_and_inverse_convention_cannot_pass(self):
        sampled = {kind: list(range(40)) for kind in ("static", "translation", "rotation")}
        records = [{"populations": ["static"], "baseline_l1": .1}] * 5
        records += [{"populations": ["translation", "rotation"], "baseline_l1": 3.,
                     "valid_fraction": .8, "variants": {"observed": {"improvement": 2.},
                                                         "inverse": {"improvement": .5}}}] * 30
        self.assertTrue(evaluate(records, sampled, Thresholds())["geometry_checks_passed"])
        sampled["rotation"] = []
        self.assertFalse(evaluate(records, sampled, Thresholds())["geometry_checks_passed"])
        sampled["rotation"] = list(range(40))
        wrong = copy.deepcopy(records)
        for record in wrong[5:]:
            record["variants"]["observed"]["improvement"] = .5
        self.assertFalse(evaluate(wrong, sampled, Thresholds())["geometry_checks_passed"])

    def test_draw_order_unknown_is_not_before(self):
        with self.assertRaises(ValueError):
            before_copy({"execute_batch": None, "depth_copy_execute_batch": 1})
        self.assertFalse(before_copy({"execute_batch": 2, "depth_copy_execute_batch": 1}))
        self.assertTrue(before_copy({"execute_batch": 1, "depth_copy_execute_batch": 2}))

    def test_writer_matrix_conflicts_are_not_resolved_by_picking_a_source(self):
        view = np.eye(4).tolist()
        projection = [[1., 0, 0, 0], [0, 2., 0, 0], [0, 0, -1e-6, 1.], [0, 0, .02, 0]]
        sources = {kind: {"descriptor_table": {"root_parameter_index": 0}, "role": kind}
                   for kind in ("view", "projection")}
        admission = {"schema": "cp2077-holstered-same-launch-camera/v2", "non_trainable": True,
                     "production_camera_binding_emitted": False, "cross_launch_admission": False,
                     "view_sources": [sources["view"]], "projection_sources": [sources["projection"]]}
        receipt = {"schema": "dm-d3d12-executed-cbv-copy/v2", "queue_tag": 1}
        draw = {"present_seq": 1, "present_qpc": 100, "execute_batch": 1, "depth_copy_execute_batch": 2,
                "selected_present_identity_passed": True, "copy_before_clear_submitted": True,
                "pipeline_depth_write": True, "pipeline_depth_enable": True, "queue_tag": 1,
                "pass_context": {"pipeline": {"depth_write_mask": 1, "dsv_format": 20}, "dsv_flags": 0,
                    "viewport": {"count": 1, "single_bits": np.array([0,0,1920,1080,0,1], dtype="<f4").view("<u4").tolist()}},
                "matrices": [{"matrix_kind": kind, "stable_source": sources[kind], "copy_receipt": receipt,
                              "translation_slot": "row3" if kind == "view" else None,
                              "matrix": view if kind == "view" else projection} for kind in ("view", "projection")]}
        raw = [{"present_seq": 1, "present_qpc": 100}]
        self.assertEqual(len(matrices_for_depth([draw], raw, admission)), 1)
        reflected = copy.deepcopy(draw)
        reflected["matrices"][0]["matrix"][0][0] = -1
        self.assertEqual(len(matrices_for_depth([reflected], raw, admission)), 1)
        later = copy.deepcopy(reflected)
        later.update(present_seq=2, present_qpc=110)
        with self.assertRaises(ValueError):
            matrices_for_depth([draw, later], raw + [{"present_seq": 2, "present_qpc": 110}], admission)
        timestamp = copy.deepcopy(draw)
        timestamp["present_qpc"] = 105
        self.assertEqual(matrices_for_depth([timestamp], raw, admission)[0]["camera_observation_qpc"], 105)
        timestamp["present_qpc"] = 99
        with self.assertRaises(ValueError):
            matrices_for_depth([timestamp], raw, admission)
        other = copy.deepcopy(draw)
        other["matrices"][0]["matrix"][3][0] += 1
        with self.assertRaises(ValueError):
            matrices_for_depth([draw, other], raw, admission)
        other = copy.deepcopy(draw)
        other["execute_batch"] = 3
        with self.assertRaises(ValueError):
            matrices_for_depth([other], raw, admission)


if __name__ == "__main__":
    unittest.main()

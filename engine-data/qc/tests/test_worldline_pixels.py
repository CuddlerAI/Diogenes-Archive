"""Cross-take pixel applicability: synthetic arrays only, no media or ffmpeg.

The metadata cases run without optional pixel dependencies. Pixel integration
cases mock decoding and use small numpy arrays; absent numpy is an explicit skip.
"""

from copy import deepcopy
from datetime import datetime, timedelta
from pathlib import Path
import unittest
from unittest.mock import patch

import helpers as H  # noqa: F401 (QC import path)

from worldline_qc import checks_pixels as pixels
from worldline_qc.checks_json import LaneData
from worldline_qc.loaders import Group, LaneFiles
from worldline_qc.model import FAIL, MANUAL, PASS, SKIP, UNVERIFIED, WARN, GroupReport, LaneInfo, RuleResult
from worldline_qc.requirements import evaluate

try:
    import numpy as np
except (ImportError, OSError, RuntimeError):
    np = None


XTAKE_RULES = {"pix.xtake_pre", "pix.xtake_post", "pix.xtake_onset"}


def _lane(take="AB", worldline="A", number=1, n=240, clock_offset_s=0):
    info = LaneInfo(take, number, worldline, "fixed", 24.0)
    files = LaneFiles(info, Path("synthetic") / take / f"video_{number}.mp4", None, None)
    data = LaneData(files)
    data.error = None
    start = datetime(2026, 1, 1, 12) + timedelta(seconds=clock_offset_s)
    data.records = [
        {
            "frame": i + 1,
            "time": (start + timedelta(seconds=i / 30)).isoformat(sep=" ", timespec="milliseconds"),
            "fps": 60.0,  # Actual game FPS is not the encoded video rate.
            "camera_position": [-4.0, 3.5, -6.0],
            "camera_rotation_quaternion": [0.0, 0.0, 0.0, 1.0],
            "camera_intrinsics": {"fx": 1280.0, "fy": 1280.0, "cx": 960.0, "cy": 540.0},
            "metric_scale": 1.0,
        }
        for i in range(n)
    ]
    return data


def _summary(n=240):
    return {"width": 1920, "height": 1080, "nb_frames": n,
            "r_frame_rate": 30.0, "avg_frame_rate": 30.0, "duration": n / 30}


class CrossTakeComparabilityTest(unittest.TestCase):
    def setUp(self):
        self.ref = _lane(n=6)
        self.other = _lane("C", "C", n=6, clock_offset_s=3600)
        self.summaries = {self.ref.key: _summary(6), self.other.key: _summary(6)}

    def reason(self, ref_frames=6, other_frames=6, summaries="default"):
        if summaries == "default":
            summaries = self.summaries
        return pixels._xtake_comparability(self.ref, self.other, ref_frames, other_frames, summaries)

    def test_equal_projection_accepts_quaternion_sign_and_distinct_wall_clocks(self):
        for i, record in enumerate(self.other.records):
            record["camera_rotation_quaternion"] = [0.0, 0.0, 0.0, -1.0 if i % 2 else 1.0]
            record["fps"] = 48.0 + i
        self.assertIsNone(self.reason())

    def test_same_intrinsics_do_not_make_different_positions_comparable(self):
        for delta in (1.0, 1e-10):
            with self.subTest(delta=delta):
                self.other.records[-1]["camera_position"][0] = -4.0 + delta
                self.assertIsInstance(self.reason(), str)

    def test_same_intrinsics_do_not_make_different_rotations_comparable(self):
        for q in ([0.0, 1.0, 0.0, 0.0], [0.0, 1e-10, 0.0, 1.0]):
            with self.subTest(q=q):
                self.other.records[-1]["camera_rotation_quaternion"] = q
                self.assertIsInstance(self.reason(), str)

    def test_intrinsics_must_match_after_the_first_frame_too(self):
        self.other.records[-1]["camera_intrinsics"]["fx"] += 1e-10
        self.assertIsInstance(self.reason(), str)

    def test_missing_camera_or_frame_evidence_is_not_comparable(self):
        original = deepcopy(self.other.records)
        for key in ("camera_intrinsics", "camera_position", "camera_rotation_quaternion", "metric_scale", "frame"):
            with self.subTest(key=key):
                self.other.records = deepcopy(original)
                del self.other.records[-1][key]
                self.assertIsInstance(self.reason(), str)
        del self.ref.records[0]["camera_intrinsics"]
        self.other.records = deepcopy(original)
        del self.other.records[0]["camera_intrinsics"]
        self.assertIsInstance(self.reason(), str, "missing K on both sides must not compare as None == None")

    def test_invalid_camera_numbers_are_not_evidence(self):
        original = deepcopy(self.other.records)
        cases = (("camera_position", [0.0, float("nan"), 0.0]),
                 ("camera_position", [True, 0.0, 0.0]),
                 ("camera_position", [10 ** 400, 0.0, 0.0]),
                 ("camera_rotation_quaternion", [0.0, 0.0, 0.0, 0.0]),
                 ("camera_rotation_quaternion", [0.0, 0.0, float("inf"), 1.0]),
                 ("camera_intrinsics", {"fx": 1280.0, "fy": 1280.0, "cx": 960.0}),
                 ("camera_intrinsics", {"fx": float("nan"), "fy": 1280.0, "cx": 960.0, "cy": 540.0}),
                 ("metric_scale", 0.0), ("metric_scale", True))
        for key, value in cases:
            with self.subTest(key=key, value=value):
                self.other.records = deepcopy(original)
                self.other.records[-1][key] = value
                self.assertIsInstance(self.reason(), str)

    def test_metric_scale_must_match_for_every_frame(self):
        self.other.records[-1]["metric_scale"] = 2.0
        self.assertIsInstance(self.reason(), str)

    def test_frame_grid_must_be_complete_one_based_and_ordered(self):
        original = deepcopy(self.other.records)
        for frames in ([0, 1, 2, 3, 4, 5], [1, 2, 2, 4, 5, 6], [1, 2, 4, 5, 6, 7], [1, 3, 2, 4, 5, 6]):
            with self.subTest(frames=frames):
                self.other.records = deepcopy(original)
                for record, frame in zip(self.other.records, frames):
                    record["frame"] = frame
                self.assertIsInstance(self.reason(), str)

    def test_missing_json_or_probe_is_unverified_not_equal(self):
        self.assertIsInstance(self.reason(summaries=None), str)
        self.assertIsInstance(self.reason(summaries={}), str)
        self.assertIsInstance(self.reason(summaries={self.ref.key: self.summaries[self.ref.key]}), str)
        self.other.records = None
        self.assertIsInstance(self.reason(), str)

    def test_json_decoded_and_probe_frame_counts_must_all_agree(self):
        self.assertIsInstance(self.reason(other_frames=5), str)
        self.assertIsInstance(self.reason(ref_frames=5, other_frames=5), str)
        self.summaries[self.other.key]["nb_frames"] = 5
        self.assertIsInstance(self.reason(), str)
        self.summaries[self.other.key]["nb_frames"] = 6
        self.other.records.pop()
        self.assertIsInstance(self.reason(), str)

    def test_probe_requires_thirty_fps_and_matching_valid_dimensions(self):
        original = deepcopy(self.summaries[self.other.key])
        for key, value in (("r_frame_rate", 29.97), ("avg_frame_rate", 30.001),
                           ("r_frame_rate", None), ("nb_frames", None),
                           ("width", 1280), ("height", 1071), ("height", 0)):
            with self.subTest(key=key, value=value):
                self.summaries[self.other.key] = dict(original, **{key: value})
                self.assertIsInstance(self.reason(), str)
        self.summaries[self.other.key] = original
        del self.summaries[self.other.key]["avg_frame_rate"]
        self.assertIsInstance(self.reason(), str)


@unittest.skipIf(np is None, "numpy unavailable; synthetic pixel integration was not executed")
class CrossTakePixelIntegrationTest(unittest.TestCase):
    def case(self, flat=False, include_d=False):
        specs = [("flat" if flat else "AB", "A", 1),
                 ("flat" if flat else "C", "C", 5 if flat else 1)]
        if include_d:
            specs.append(("flat" if flat else "D", "D", 9 if flat else 1))
        data = [_lane(take, wl, number, clock_offset_s=i * 3600) for i, (take, wl, number) in enumerate(specs)]
        group = Group(Path("synthetic"), "flat" if flat else "take", [d.lf for d in data])
        lanes = {d.key: d for d in data}
        summaries = {d.key: _summary() for d in data}
        images = {str(d.lf.video): np.full((240, 10, 10), 80, dtype=np.uint8) for d in data}
        return group, lanes, summaries, images

    def run_pixels(self, case, takeover=None, use_summaries=True, missing_paths=(), undecodable=()):
        group, lanes, summaries, images = case

        def decode(_cv2, _np, path, _n_thumbs):
            if str(path) in undecodable:
                return None, None, []
            gray = images[str(path)]
            return gray, gray.reshape(len(gray), -1).mean(1), []

        with patch.object(Path, "is_file", autospec=True, side_effect=lambda path: str(path) not in missing_paths), \
                patch.object(pixels, "_try_import", return_value=(object(), np)), \
                patch.object(pixels, "_decode", side_effect=decode):
            return pixels.analyze_pixels(group, lanes, takeover or {"C": 2.0, "D": 0.0}, n_thumbs=0,
                                         video_summaries=summaries if use_summaries else None)

    def xtake(self, results, target="世界线C"):
        return {r.rule: r for r in results if r.rule in XTAKE_RULES and r.target == target}

    def changed(self, case, worldline, pre_pixels=0, post_pixels=30, transition=60):
        group, _, _, images = case
        lane = next(lf for lf in group.lanes if lf.info.worldline == worldline)
        flat = images[str(lane.video)].reshape(240, 100)
        flat[:transition, :pre_pixels] = 160
        flat[transition:, :post_pixels] = 160

    def test_disabled_pixels_or_missing_cv2_leave_all_three_rules_unverified(self):
        for enabled in (False, True):
            with self.subTest(enabled=enabled):
                group, lanes, summaries, _ = self.case()
                with patch.object(pixels, "_try_import", return_value=(None, None)) as importer, \
                        patch.object(pixels, "_decode", side_effect=AssertionError("decoder must not run")):
                    results, art = pixels.analyze_pixels(group, lanes, {"C": 2.0}, enabled=enabled,
                                                        video_summaries=summaries)
                if not enabled:
                    importer.assert_not_called()
                checks = self.xtake(results, "group")
                self.assertEqual(set(checks), XTAKE_RULES)
                self.assertTrue(all(r.status == UNVERIFIED for r in checks.values()))
                self.assertEqual(art["xtake"], {})

    def test_missing_or_duplicate_reference_and_target_fixed_lanes_are_unverified(self):
        for key in ("AB/1", "C/1"):
            for invalid in ("missing_fixed_role", "duplicate_fixed_lane"):
                with self.subTest(key=key, invalid=invalid):
                    case = self.case()
                    group, lanes, summaries, images = case
                    original = lanes[key]
                    if invalid == "missing_fixed_role":
                        original.role = original.lf.info.role = "orbit"
                    else:
                        duplicate = _lane(original.lf.info.take, original.lf.info.worldline, number=2)
                        group.lanes.append(duplicate.lf)
                        lanes[duplicate.key] = duplicate
                        summaries[duplicate.key] = _summary()
                        images[str(duplicate.lf.video)] = np.full((240, 10, 10), 80, dtype=np.uint8)
                    results, art = self.run_pixels(case)
                    checks = self.xtake(results)
                    self.assertEqual(set(checks), XTAKE_RULES)
                    self.assertTrue(all(r.status == UNVERIFIED for r in checks.values()))
                    self.assertEqual(art["xtake"], {})

    def test_missing_or_undecodable_reference_and_target_videos_are_unverified(self):
        for key in ("AB/1", "C/1"):
            for invalid in ("no_video_path", "missing_file", "decode_none"):
                with self.subTest(key=key, invalid=invalid):
                    case = self.case()
                    lane = case[1][key]
                    path = str(lane.lf.video)
                    if invalid == "no_video_path":
                        lane.lf.video = None
                    results, art = self.run_pixels(
                        case, missing_paths=(path,) if invalid == "missing_file" else (),
                        undecodable=(path,) if invalid == "decode_none" else ())
                    checks = self.xtake(results)
                    self.assertEqual(set(checks), XTAKE_RULES)
                    self.assertTrue(all(r.status == UNVERIFIED for r in checks.values()))
                    self.assertEqual(art["xtake"], {})

    def test_comparable_pixels_retain_original_pre_pass_warn_fail_thresholds(self):
        for changed, status in ((5, PASS), (10, WARN), (20, FAIL)):
            with self.subTest(changed_pixels=changed):
                case = self.case()
                self.changed(case, "C", pre_pixels=changed)
                results, _ = self.run_pixels(case)
                check = self.xtake(results)["pix.xtake_pre"]
                self.assertEqual(check.status, status)
                self.assertAlmostEqual(check.data["pre_mean"], changed / 100)

    def test_comparable_pixels_retain_post_threshold_and_onset(self):
        for changed, status in ((30, PASS), (40, WARN)):
            with self.subTest(changed_pixels=changed):
                case = self.case()
                self.changed(case, "C", post_pixels=changed)
                results, _ = self.run_pixels(case)
                checks = self.xtake(results)
                self.assertEqual(checks["pix.xtake_post"].status, status)
                self.assertAlmostEqual(checks["pix.xtake_post"].data["post_p95"], changed / 100)
                self.assertEqual(checks["pix.xtake_onset"].status, PASS)
                self.assertEqual(checks["pix.xtake_onset"].data["onset"], 2)

    def test_comparable_pixels_still_reject_early_onset(self):
        case = self.case()
        self.changed(case, "C", transition=120)
        case[3][str(case[1]["C/1"].lf.video)][:30].reshape(30, 100)[:, :40] = 160
        results, _ = self.run_pixels(case, takeover={"C": 4.0})
        self.assertEqual(self.xtake(results)["pix.xtake_onset"].status, FAIL)

    def test_different_pose_cannot_produce_any_xtake_pass_or_fail(self):
        case = self.case()
        case[1]["C/1"].records[-1]["camera_position"][0] += 1.0
        results, art = self.run_pixels(case)
        checks = self.xtake(results)
        self.assertEqual(set(checks), XTAKE_RULES)
        self.assertTrue(all(r.status == UNVERIFIED for r in checks.values()))
        self.assertTrue(all(r.evidence for r in checks.values()))
        self.assertEqual(art["xtake"], {})

    def test_missing_probe_or_both_intrinsics_never_pass(self):
        for missing in ("probe", "intrinsics"):
            with self.subTest(missing=missing):
                case = self.case()
                if missing == "intrinsics":
                    for data in case[1].values():
                        del data.records[0]["camera_intrinsics"]
                results, _ = self.run_pixels(case, use_summaries=missing != "probe")
                checks = self.xtake(results)
                self.assertEqual(set(checks), XTAKE_RULES)
                self.assertTrue(all(r.status == UNVERIFIED for r in checks.values()))

    def test_sign_equivalent_quaternions_and_different_clocks_remain_comparable(self):
        case = self.case()
        for record in case[1]["C/1"].records:
            record["camera_rotation_quaternion"] = [0.0, 0.0, 0.0, -1.0]
            record["fps"] = 48.0
        self.changed(case, "C")
        results, _ = self.run_pixels(case)
        self.assertEqual(self.xtake(results)["pix.xtake_pre"].status, PASS)

    def test_flat_layout_keeps_a_c_d_fixed_cameras_separate(self):
        case = self.case(flat=True, include_d=True)
        self.changed(case, "C", post_pixels=30)
        self.changed(case, "D", post_pixels=40, transition=0)
        results, art = self.run_pixels(case)
        c, d = self.xtake(results), self.xtake(results, "世界线D")
        self.assertEqual(set(c), XTAKE_RULES)
        self.assertEqual(set(d), XTAKE_RULES)
        self.assertEqual(c["pix.xtake_pre"].status, PASS)
        self.assertEqual(c["pix.xtake_post"].status, PASS)
        self.assertEqual(d["pix.xtake_pre"].status, SKIP)  # D starts at logical zero: no pre-window.
        self.assertEqual(d["pix.xtake_post"].status, WARN)
        self.assertEqual(len(art["xtake"]), 2)

    def test_unverified_comparison_cannot_be_hidden_by_other_requirement_results(self):
        case = self.case()
        case[1]["C/1"].records[0]["camera_position"][0] += 1.0
        results, _ = self.run_pixels(case)
        report = GroupReport("synthetic-applicability")
        report.extend(list(self.xtake(results).values()))
        report.extend([RuleResult("sync.b_replays_a", PASS, "世界线B", "synthetic same take"),
                       RuleResult("wl.same_world", MANUAL, "group", "manual evidence is still pending"),
                       RuleResult("wl.event_changed_flag", PASS, "世界线C", "a declaration is present")])
        self.assertEqual(report.verdict, UNVERIFIED)
        clauses = {row["clause"].id: row["status"] for row in evaluate(report.results)}
        self.assertEqual(clauses["R29"], UNVERIFIED)
        self.assertEqual(clauses["R30"], UNVERIFIED)


if __name__ == "__main__":
    unittest.main()

"""造坏样例：逐条规则能抓出对应缺陷。"""

import shutil
import tempfile
import unittest
from pathlib import Path

import helpers as H

from engine_qc.clip_checker import check_clip


def small_cams(n=90):
    return H.valid_mc_cams(n)


class BrokenSampleTest(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="qc_broken_"))

    def tearDown(self):
        shutil.rmtree(self.tmp, ignore_errors=True)

    def _check(self, cams):
        clip = H.write_mc_clip(self.tmp / "clip", cams)
        return check_clip(clip, "mc")

    def test_wrong_keyname_lowercase_in_mc(self):
        cams = small_cams()
        for f in cams["2"]:
            f["camera_follow_offset"] = f.pop("camera_Follow Offset")
        report = self._check(cams)
        self.assertEqual(
            H.status_for(report, "json.keys", "action_camera_2.json"), "FAIL")
        self.assertEqual(
            H.status_for(report, "json.schema", "action_camera_2.json"), "FAIL")
        # 其余三路不受影响
        self.assertEqual(
            H.status_for(report, "json.keys", "action_camera_1.json"), "PASS")

    def test_wrong_keyname_space_in_sfm(self):
        f30, f15 = H.valid_sfm_files(120)
        for f in f15:
            f["camera_Follow Offset"] = f["camera_follow_offset"]
        clip = H.write_sfm_clip(self.tmp / "sfm", f30, f15)
        report = check_clip(clip, "sfm")
        self.assertEqual(
            H.status_for(report, "json.keys", "action_camera_15fps.json"), "FAIL")
        self.assertEqual(
            H.status_for(report, "json.keys", "action_camera_30fps.json"), "PASS")

    def test_quaternion_not_normalized(self):
        cams = small_cams()
        cams["3"][10]["camera_rotation_quaternion"] = [0.0, 0.0, 0.0, 0.5]
        report = self._check(cams)
        self.assertEqual(
            H.status_for(report, "num.quat_norm", "action_camera_3.json"), "FAIL")

    def test_fx_neq_fy(self):
        cams = small_cams()
        for f in cams["1"]:
            f["camera_intrinsics"]["fy"] = 1000.0
        report = self._check(cams)
        self.assertEqual(
            H.status_for(report, "num.fx_fy", "action_camera_1.json"), "FAIL")
        # 逐帧一致地错 → 内参恒定性不应误报
        self.assertEqual(
            H.status_for(report, "num.intrinsics_const", "action_camera_1.json"), "PASS")

    def test_frame_skip(self):
        cams = small_cams()
        del cams["2"][50]  # 49 → 51 跳帧
        report = self._check(cams)
        self.assertEqual(
            H.status_for(report, "frame.continuity", "action_camera_2.json"), "FAIL")

    def test_frame_duplicate(self):
        cams = small_cams()
        cams["2"][51]["frame"] = 50
        report = self._check(cams)
        r = H.status_for(report, "frame.continuity", "action_camera_2.json")
        self.assertEqual(r, "FAIL")

    def test_intrinsics_drift(self):
        cams = small_cams()
        cams["1"][30]["camera_intrinsics"] = {"fx": 1080.0, "fy": 1080.0,
                                              "cx": 1000.0, "cy": 540.0}
        report = self._check(cams)
        self.assertEqual(
            H.status_for(report, "num.intrinsics_const", "action_camera_1.json"), "FAIL")

    def test_four_route_count_mismatch(self):
        cams = small_cams()
        cams["2"] = cams["2"][:-1]  # 89 vs 90
        report = self._check(cams)
        self.assertEqual(H.statuses(report, "sync.frames"), {"FAIL"})

    def test_overspeed_p95(self):
        cams = small_cams()
        cams["1"] = H.gen_cam_frames(90, [0.0, 1.6, 0.0], [8.0, 0.0, 0.0], cam1=True)
        report = self._check(cams)
        self.assertEqual(
            H.status_for(report, "phys.speed_p95", "action_camera_1.json"), "FAIL")

    def test_cam4_instant_speed_cap(self):
        cams = small_cams()
        # 第四路一帧位置突跳 0.4m → 瞬时 12 m/s，超 10 硬帽但不动 p95
        cams["4"][45]["camera_position"][0] += 0.4
        report = self._check(cams)
        self.assertEqual(
            H.status_for(report, "phys.speed_cap4", "action_camera_4.json"), "FAIL")

    def test_instant_teleport_cam2(self):
        cams = small_cams()
        # cam2 自帧 46 起整体平移 5m：帧 45→46 一次 150 m/s 单帧瞬移
        #（实证形态：orbit-60s clip-013 cam2 帧 398/466，p95 口径当时判 PASS）
        for f in cams["2"][46:]:
            f["camera_position"][0] += 5.0
        report = self._check(cams)
        self.assertEqual(
            H.status_for(report, "phys.speed_max", "action_camera_2.json"), "FAIL")
        # p95 对单帧瞬移无感——正是本闸门要补的盲区
        self.assertEqual(
            H.status_for(report, "phys.speed_p95", "action_camera_2.json"), "PASS")
        ev = next(r.evidence for r in report.results
                  if r.rule == "phys.speed_max" and r.target == "action_camera_2.json")
        self.assertIn("帧 45→46", ev)
        self.assertIn("t=1.50s", ev)
        # 其余路不受影响
        self.assertEqual(
            H.status_for(report, "phys.speed_max", "action_camera_1.json"), "PASS")

    def test_instant_teleport_sfm_15fps(self):
        f30, f15 = H.valid_sfm_files(120)
        # 15fps 文件差分 dt=2/30（README 反算口径）：自样本 30（帧 60）起平移 1m
        # → 帧 58→60 处 ≈16 m/s；帧号须按文件内 frame 字段（偶数步进）报告
        for f in f15[30:]:
            f["camera_position"][0] += 1.0
        clip = H.write_sfm_clip(self.tmp / "sfm", f30, f15)
        report = check_clip(clip, "sfm")
        self.assertEqual(
            H.status_for(report, "phys.speed_max", "action_camera_15fps.json"), "FAIL")
        ev = next(r.evidence for r in report.results
                  if r.rule == "phys.speed_max" and r.target == "action_camera_15fps.json")
        self.assertIn("帧 58→60", ev)
        self.assertEqual(
            H.status_for(report, "phys.speed_max", "action_camera_30fps.json"), "PASS")

    def test_metric_scale_wrong(self):
        cams = small_cams()
        for f in cams["1"]:
            f["metric_scale"] = 2.0
        report = self._check(cams)
        self.assertEqual(
            H.status_for(report, "num.metric_scale", "action_camera_1.json"), "FAIL")

    def test_container_not_array(self):
        cams = small_cams()
        clip = H.write_mc_clip(self.tmp / "clip", cams)
        (clip / "action_camera_2.json").write_text('{"frames": []}')
        report = check_clip(clip, "mc")
        self.assertEqual(
            H.status_for(report, "json.container", "action_camera_2.json"), "FAIL")

    def test_covis_low_ratio(self):
        cams = small_cams()
        clip = H.write_mc_clip(self.tmp / "clip", cams, extra={
            "covis.json": H.covis_doc(0.4)})
        report = check_clip(clip, "mc")
        self.assertEqual(H.statuses(report, "covis.ratio"), {"FAIL"})
        self.assertEqual(H.statuses(report, "covis.ratio_strict"), {"FAIL"})

    def test_missing_files_fail_packaging(self):
        cams = small_cams()
        clip = H.write_mc_clip(self.tmp / "clip", cams)
        (clip / "action_camera_4.json").unlink()
        report = check_clip(clip, "mc")
        self.assertEqual(H.statuses(report, "pack.files"), {"FAIL"})


if __name__ == "__main__":
    unittest.main()

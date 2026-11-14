"""phys.roll 歪头闸门：capture-v7 型 roll 慢漂移必 FAIL；合规刻意歪头不误杀。

背景（docs/78 回写 0806）：外来交付 capture-v7（2026_08_06_plaza_07，orbit 型）
四路 roll 从 t5≈-5° 单调漂到 t55≈-74°——环绕 up 未锁 world-up 的累积漂移，
速率平缓，当时的 128 项规则（速率链 angspeed/angaccel/angjerk）全绿放行。
"""

import math
import shutil
import tempfile
import unittest
from pathlib import Path

import helpers as H

from engine_qc.checks_motion import check_roll
from engine_qc.clip_checker import check_clip
from engine_qc.config import QCConfig
from engine_qc.mathutil import roll_deg


def yaw_roll_quat(yaw_deg, roll_deg_):
    """yaw(绕 y) ∘ roll(绕光轴 z) 的合成四元数 [x,y,z,w]。"""
    ry, rr = math.radians(yaw_deg) / 2.0, math.radians(roll_deg_) / 2.0
    sy, cy = math.sin(ry), math.cos(ry)
    sr, cr = math.sin(rr), math.cos(rr)
    return [sy * sr, sy * cr, cy * sr, cy * cr]


def roll_frames(rolls, yaw_deg=0.0):
    return [{"frame": i, "camera_rotation_quaternion": yaw_roll_quat(yaw_deg, r)}
            for i, r in enumerate(rolls)]


DT = 1.0 / 30.0


class RollMathTest(unittest.TestCase):
    def test_pure_roll_roundtrip(self):
        for deg in (-74.0, -15.0, -5.0, 0.0, 12.5, 30.0):
            q = yaw_roll_quat(0.0, deg)
            self.assertAlmostEqual(roll_deg(q), deg, places=6)

    def test_roll_readout_independent_of_yaw(self):
        # 环绕运镜 yaw 一直在走，roll 读数不能被 yaw 污染
        for yaw in (0.0, 45.0, 179.0, -120.0):
            self.assertAlmostEqual(roll_deg(yaw_roll_quat(yaw, -20.0)), -20.0,
                                   places=6)


class CheckRollUnitTest(unittest.TestCase):
    """判据边界：合法歪头（±5~15°、占比≤2%）不误杀，阈值不是 0。"""

    def setUp(self):
        self.cfg = QCConfig()

    def _one(self, rolls, yaw_deg=0.0):
        results = check_roll("action_camera_2.json",
                             roll_frames(rolls, yaw_deg), DT, self.cfg)
        self.assertEqual(len(results), 1)
        return results[0]

    def test_zero_roll_passes(self):
        self.assertEqual(self._one([0.0] * 300).status, "PASS")

    def test_deliberate_roll_within_spec_passes(self):
        # 刻意歪头 ±12°（规格 ±5~15° 之内）——占比 100% 也合法：占比闸只管 >15° 的帧
        rolls = [12.0 * math.sin(i * 0.05) for i in range(600)]
        self.assertEqual(self._one(rolls).status, "PASS")

    def test_small_over_spec_fraction_passes(self):
        # 1.5% 的帧在 18°（>15° 但 ≤20°）：占比 ≤2% 且未破硬帽 → PASS。
        # 这是「阈值别设 0」的回归：转场毛刺/姿态噪声不该杀合规片。
        rolls = [0.0] * 1000
        for i in range(15):
            rolls[i * 60] = 18.0
        self.assertEqual(self._one(rolls).status, "PASS")

    def test_exact_boundary_passes(self):
        # 恰 2% 帧超 15°、max 恰 20°：判据是 ≤，边界放行
        rolls = [0.0] * 1000
        for i in range(20):
            rolls[i * 50] = 20.0
        self.assertEqual(self._one(rolls).status, "PASS")

    def test_over_fraction_fails(self):
        # 3% 帧在 18°：硬帽没破，占比破 2% → FAIL(a)
        rolls = [0.0] * 1000
        for i in range(30):
            rolls[i * 33] = 18.0
        r = self._one(rolls)
        self.assertEqual(r.status, "FAIL")
        self.assertIn("占比", r.evidence)
        self.assertNotIn("硬帽", r.evidence)

    def test_single_spike_over_cap_fails(self):
        # 单帧 25°：占比 0.1% 合规，但破硬帽 → FAIL(b)
        rolls = [0.0] * 1000
        rolls[500] = 25.0
        r = self._one(rolls)
        self.assertEqual(r.status, "FAIL")
        self.assertIn("硬帽", r.evidence)
        self.assertIn("帧 500", r.evidence)

    def test_capture_v7_style_drift_fails_under_yaw(self):
        # capture-v7 形态：orbit（yaw 在走）+ roll 线性慢漂 0→-74°，
        # 漂移逐帧 <0.05°，速率链规则永不触发，本闸门必须 FAIL
        n = 1800
        rolls = [-74.0 * i / (n - 1) for i in range(n)]
        results = check_roll(
            "action_camera_2.json",
            [{"frame": i, "camera_rotation_quaternion": yaw_roll_quat(-8.0 * i * DT, r)}
             for i, r in enumerate(rolls)], DT, self.cfg)
        self.assertEqual(results[0].status, "FAIL")

    def test_bad_quat_skips(self):
        frames = [{"frame": 0, "camera_rotation_quaternion": [0.0, 0.0, 1.0]}]
        self.assertEqual(check_roll("x.json", frames, DT, self.cfg)[0].status, "SKIP")


class RollClipIntegrationTest(unittest.TestCase):
    """接线断言：MC 逐路发规则、漂移路 FAIL 不连坐；SFM 不发。"""

    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="qc_roll_"))

    def tearDown(self):
        shutil.rmtree(self.tmp, ignore_errors=True)

    def test_drift_lane_fails_others_pass(self):
        cams = H.valid_mc_cams(900)
        for i, f in enumerate(cams["2"]):
            # 保留原 yaw 轨迹（解耦判据依赖），只叠加 roll 慢漂 0→-60°
            f["camera_rotation_quaternion"] = yaw_roll_quat(
                -8.0 * i * DT, -60.0 * i / 899)
        report = check_clip(H.write_mc_clip(self.tmp / "clip", cams), "mc")
        self.assertEqual(
            H.status_for(report, "phys.roll", "action_camera_2.json"), "FAIL")
        for lane in (1, 3, 4):
            self.assertEqual(
                H.status_for(report, "phys.roll", f"action_camera_{lane}.json"),
                "PASS", f"lane {lane} 不该被 lane 2 的漂移连坐")

    def test_sfm_mode_does_not_emit_roll(self):
        f30, f15 = H.valid_sfm_files(120)
        report = check_clip(H.write_sfm_clip(self.tmp / "sfm", f30, f15), "sfm")
        self.assertEqual(H.results_by_rule(report, "phys.roll"), [],
                         "docs/68 无歪头规格，SFM 不该发 phys.roll")


if __name__ == "__main__":
    unittest.main()

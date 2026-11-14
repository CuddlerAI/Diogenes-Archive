"""第四路俯视伴飞形态（plan.cam4.mode="topdown"，docs/69 §8.3 形态 b）。

正例：垂直向下 + 高空 + 沿 guide 路线上空漂移 → 三条 topdown 规则 PASS，
变速契约三条 SKIP，phys.roll(cam4) SKIP，初始帧闸门自动豁免 lane 4。
反例：斜视 / 低空 / 脱队 / 非法 mode 各自 FAIL；缺省 mode 输出与增补前逐字一致。
"""

import math
import shutil
import tempfile
import unittest
from pathlib import Path

import helpers as H

from engine_qc.clip_checker import check_clip

#: 绕 x 轴 +90°：forward (0,0,1) → (0,-1,0)，即垂直向下看
Q_DOWN = [math.sqrt(0.5), 0.0, 0.0, math.sqrt(0.5)]
#: 绕 x 轴 +45°：forward → (0,-0.707,0.707)，偏离垂直向下 45°
Q_OBLIQUE = [math.sin(math.radians(22.5)), 0.0, 0.0, math.cos(math.radians(22.5))]

CAM4 = "action_camera_4.json"


def _plan(cam4):
    return {
        "version": "1", "seed": 42, "mode": "mc", "scene": "testscene",
        "duration_s": 60, "fps": 30,
        "cameras": [
            {"id": n, "focal_mm": 24, "aspect": "16:9",
             "segments": [{"t0": 0, "t1": 60, "cls": "orbit"}]}
            for n in (1, 2, 3, 4)
        ],
        "cam4": cam4,
    }


def _topdown_cam4(n, quat=None, y=41.6, vel=(0.5, 0.0, 0.1), p0=(5.0, None, 0.0)):
    """俯视位：guide（lane 1，沿 +x 的直线路）上空的斜向缓漂。

    速度 0.51 m/s 与 lane 1 的 1.0 m/s 之比 0.51 ∉ [0.7,1.4]，不会触发解耦同步窗。
    """
    quat = quat or Q_DOWN
    frames = []
    for i in range(n):
        t = i / 30.0
        pos = [p0[0] + vel[0] * t, y + vel[1] * t, p0[2] + vel[2] * t]
        frames.append(H.mc_frame(i, pos, quat, list(vel)))
    return frames


def _clip(tmp, cam4_frames, cam4_plan):
    cams = H.valid_mc_cams(1800)
    cams["4"] = cam4_frames
    return check_clip(H.write_mc_clip(tmp / "clip", cams,
                                      extra={"plan.json": _plan(cam4_plan)}), "mc")


class TopdownValidTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = Path(tempfile.mkdtemp(prefix="qc_td_"))
        cls.report = _clip(cls.tmp, _topdown_cam4(1800),
                           {"guide_cam": 1, "mode": "topdown"})

    @classmethod
    def tearDownClass(cls):
        shutil.rmtree(cls.tmp, ignore_errors=True)

    def test_mode_and_topdown_rules_pass(self):
        self.assertEqual(H.statuses(self.report, "cam4.mode"), {"PASS"})
        for rule in ("cam4.topdown_pitch", "cam4.topdown_alt", "cam4.topdown_route"):
            self.assertEqual(H.statuses(self.report, rule), {"PASS"}, rule)

    def test_timewarp_rules_skipped(self):
        for rule in ("cam4.switch_count", "cam4.t_sw", "cam4.band_dwell"):
            self.assertEqual(H.statuses(self.report, rule), {"SKIP"}, rule)

    def test_roll_skipped_for_cam4_only(self):
        # 垂直俯视下 roll 分解退化 → lane 4 跳检；前三路照常判
        self.assertEqual(H.status_for(self.report, "phys.roll", CAM4), "SKIP")
        for lane in (1, 2, 3):
            self.assertEqual(
                H.status_for(self.report, "phys.roll", f"action_camera_{lane}.json"),
                "PASS", f"lane {lane}")

    def test_init_gate_auto_exempts_lane4(self):
        # 俯视位首帧朝向差 90°、高差 40 m，不豁免必挂；前三路之间仍照常判定
        self.assertEqual(H.statuses(self.report, "init.orientation"), {"PASS"})
        self.assertEqual(H.statuses(self.report, "init.distance"), {"PASS"})

    def test_decouple_and_speed_caps_still_judged(self):
        self.assertEqual(H.statuses(self.report, "decouple.pairs"), {"PASS"})
        self.assertEqual(H.status_for(self.report, "phys.speed_cap4", CAM4), "PASS")


class TopdownCounterexampleTest(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="qc_td_neg_"))

    def tearDown(self):
        shutil.rmtree(self.tmp, ignore_errors=True)

    def test_oblique_pitch_fails(self):
        report = _clip(self.tmp, _topdown_cam4(1800, quat=Q_OBLIQUE),
                       {"guide_cam": 1, "mode": "topdown"})
        self.assertEqual(H.statuses(report, "cam4.topdown_pitch"), {"FAIL"})

    def test_low_altitude_fails(self):
        report = _clip(self.tmp, _topdown_cam4(1800, y=6.6),
                       {"guide_cam": 1, "mode": "topdown"})
        self.assertEqual(H.statuses(report, "cam4.topdown_alt"), {"FAIL"})

    def test_off_route_fails(self):
        # guide（lane 1）路线在 z=0 直线；z 漂到 50 m 外超硬帽 40 m
        report = _clip(self.tmp, _topdown_cam4(1800, p0=(5.0, None, 50.0)),
                       {"guide_cam": 1, "mode": "topdown"})
        self.assertEqual(H.statuses(report, "cam4.topdown_route"), {"FAIL"})

    def test_invalid_mode_fails_and_skips_both_groups(self):
        report = _clip(self.tmp, _topdown_cam4(1800),
                       {"guide_cam": 1, "mode": "hover"})
        self.assertEqual(H.statuses(report, "cam4.mode"), {"FAIL"})
        for rule in ("cam4.switch_count", "cam4.t_sw", "cam4.band_dwell",
                     "cam4.topdown_pitch", "cam4.topdown_alt", "cam4.topdown_route"):
            self.assertEqual(H.statuses(report, rule), {"SKIP"}, rule)
        # 非法声明不触发豁免：俯视位首帧朝向/高差照常判 → 必挂，闸门没有被放空
        self.assertEqual(H.statuses(report, "init.orientation"), {"FAIL"})
        self.assertEqual(H.status_for(report, "phys.roll", CAM4), "PASS")


class TimewarpCompatTest(unittest.TestCase):
    """既有 timewarp 契约不因 mode 字段增补而变。"""

    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="qc_td_tw_"))

    def tearDown(self):
        shutil.rmtree(self.tmp, ignore_errors=True)

    def _reparam_clip(self, cam4_plan):
        cams = H.valid_mc_cams(1800)
        pos4 = H.cam4_reparam_positions(1800, [2.0, 1.6, 2.0], [1.0, 0.0, 0.0],
                                        0.5, 2.0, 30.0, 2.0)
        cams["4"] = H.gen_cam_frames(1800, None, [1.0, 0.0, 0.0], positions=pos4)
        base = {"guide_cam": 1, "rho1": 0.5, "rho2": 2.0, "t_sw": 30.0, "delta": 2.0}
        base.update(cam4_plan)
        return check_clip(H.write_mc_clip(self.tmp / "clip", cams,
                                          extra={"plan.json": _plan(base)}), "mc")

    def test_default_mode_emits_no_mode_rule(self):
        report = self._reparam_clip({})
        self.assertEqual(H.results_by_rule(report, "cam4.mode"), [])
        self.assertEqual(H.statuses(report, "cam4.switch_count"), {"PASS"})
        self.assertEqual(H.results_by_rule(report, "cam4.topdown_pitch"), [])

    def test_explicit_timewarp_judges_same_rules(self):
        report = self._reparam_clip({"mode": "timewarp"})
        self.assertEqual(H.statuses(report, "cam4.mode"), {"PASS"})
        self.assertEqual(H.statuses(report, "cam4.switch_count"), {"PASS"})
        self.assertEqual(H.statuses(report, "cam4.t_sw"), {"PASS"})
        self.assertEqual(H.statuses(report, "cam4.band_dwell"), {"PASS"})
        self.assertEqual(H.results_by_rule(report, "cam4.topdown_pitch"), [])
        # timewarp 声明不触发俯视位豁免
        self.assertEqual(H.status_for(report, "phys.roll", CAM4), "PASS")


if __name__ == "__main__":
    unittest.main()

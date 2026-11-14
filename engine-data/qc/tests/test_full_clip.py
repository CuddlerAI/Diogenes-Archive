"""合成 1800 帧 4 路合法 MC clip：整 clip 除 video/SKIP 外全 PASS；
解耦闸门在「四路完全同步运动」反例上 FAIL；第四路变速正/反例。"""

import shutil
import tempfile
import unittest
from pathlib import Path

import helpers as H

from engine_qc.clip_checker import check_clip


def _plan(rho1, rho2, t_sw, delta):
    return {
        "version": "1", "seed": 42, "mode": "mc", "scene": "testscene",
        "duration_s": 60, "fps": 30,
        "cameras": [
            {"id": n, "focal_mm": 24, "aspect": "16:9",
             "segments": [{"t0": 0, "t1": 60, "cls": "orbit"}]}
            for n in (1, 2, 3, 4)
        ],
        "cam4": {"guide_cam": 1, "rho1": rho1, "rho2": rho2,
                 "t_sw": t_sw, "delta": delta},
    }


class FullValidClipTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = Path(tempfile.mkdtemp(prefix="qc_full_"))
        clip = H.write_mc_clip(cls.tmp / "clip", H.valid_mc_cams(1800), extra={
            "covis.json": H.covis_doc(0.62),
        })
        cls.report = check_clip(clip, "mc")

    @classmethod
    def tearDownClass(cls):
        shutil.rmtree(cls.tmp, ignore_errors=True)

    def test_no_fail_outside_video(self):
        fails = [r for r in self.report.results if r.status == "FAIL"]
        self.assertEqual(fails, [],
                         "整 clip 除 video/SKIP 外应全 PASS，实际 FAIL："
                         + "; ".join(f"{r.rule}({r.target}):{r.evidence}" for r in fails))

    def test_key_groups_pass(self):
        for rule in ("pack.files", "json.container", "json.schema", "json.keys",
                     "dur.frames", "frame.continuity", "num.quat_norm", "num.fx_fy",
                     "num.intrinsics_const", "phys.speed_p95", "phys.speed_max",
                     "phys.angspeed_p95", "phys.roll",
                     "sync.frames", "sync.frame_align", "decouple.pairs",
                     "decouple.all4", "init.orientation", "init.distance",
                     "sysinfo.schema", "sysinfo.origin", "sysinfo.resolution",
                     "covis.reading", "covis.ratio", "covis.ratio_strict"):
            self.assertEqual(H.statuses(self.report, rule), {"PASS"}, rule)

    def test_video_and_plan_skipped(self):
        # 无视频文件 → video 检查 SKIP 而非 FAIL；无 plan.json → 第四路专项 SKIP
        self.assertIn("SKIP", H.statuses(self.report, "video.duration"))
        self.assertNotIn("FAIL", H.statuses(self.report, "video.duration"))
        self.assertEqual(H.statuses(self.report, "cam4.switch_count"), {"SKIP"})

    def test_verdict_pass(self):
        self.assertEqual(self.report.verdict, "PASS")


class CoupledCounterexampleTest(unittest.TestCase):
    """四路完全同步运动 → 解耦闸门必须 FAIL。"""

    @classmethod
    def setUpClass(cls):
        cls.tmp = Path(tempfile.mkdtemp(prefix="qc_coupled_"))
        clip = H.write_mc_clip(cls.tmp / "clip", H.coupled_mc_cams(1800))
        cls.report = check_clip(clip, "mc")

    @classmethod
    def tearDownClass(cls):
        shutil.rmtree(cls.tmp, ignore_errors=True)

    def test_decouple_gate_fails(self):
        self.assertEqual(H.statuses(self.report, "decouple.pairs"), {"FAIL"})
        self.assertEqual(H.statuses(self.report, "decouple.all4"), {"FAIL"})

    def test_other_groups_still_pass(self):
        for rule in ("json.schema", "dur.frames", "frame.continuity",
                     "sync.frames", "phys.speed_p95"):
            self.assertEqual(H.statuses(self.report, rule), {"PASS"}, rule)


class Cam4ReparamTest(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="qc_cam4_"))

    def tearDown(self):
        shutil.rmtree(self.tmp, ignore_errors=True)

    def _clip(self, rho1, rho2, t_sw, delta):
        cams = H.valid_mc_cams(1800)
        pos4 = H.cam4_reparam_positions(1800, [2.0, 1.6, 2.0], [1.0, 0.0, 0.0],
                                        rho1, rho2, t_sw, delta)
        cams["4"] = H.gen_cam_frames(1800, None, [1.0, 0.0, 0.0], positions=pos4)
        clip = H.write_mc_clip(self.tmp / "clip", cams,
                               extra={"plan.json": _plan(rho1, rho2, t_sw, delta)})
        return check_clip(clip, "mc")

    def test_valid_reparam_passes(self):
        report = self._clip(rho1=0.5, rho2=2.0, t_sw=30.0, delta=2.0)
        self.assertEqual(H.statuses(report, "cam4.switch_count"), {"PASS"})
        self.assertEqual(H.statuses(report, "cam4.t_sw"), {"PASS"})
        self.assertEqual(H.statuses(report, "cam4.band_dwell"), {"PASS"})
        # 变速第四路不应破坏解耦/物理
        self.assertEqual(H.statuses(report, "decouple.pairs"), {"PASS"})
        self.assertEqual(
            H.status_for(report, "phys.speed_cap4", "action_camera_4.json"), "PASS")

    def test_instant_speed_cap_is_emitted_once_per_lane(self):
        """瞬时速度硬帽每路只能有一条规则。

        cam4 走合同值规则 `phys.speed_cap4`（docs/71 §9），前三路走内控的
        `phys.speed_max`。两者判据相同、来源不同：同一路上同时发两条会让规则
        计数虚高，且日后分别调阈值时同一片子会同时出现 PASS 与 FAIL。
        """
        report = self._clip(rho1=0.5, rho2=2.0, t_sw=30.0, delta=2.0)
        for lane in (1, 2, 3):
            target = f"action_camera_{lane}.json"
            self.assertEqual(H.status_for(report, "phys.speed_max", target), "PASS",
                             f"前三路应发 phys.speed_max（lane {lane}）")
            self.assertIsNone(H.status_for(report, "phys.speed_cap4", target),
                              f"前三路不该发第四路的合同规则（lane {lane}）")
        cam4 = "action_camera_4.json"
        self.assertEqual(H.status_for(report, "phys.speed_cap4", cam4), "PASS")
        self.assertIsNone(H.status_for(report, "phys.speed_max", cam4),
                          "cam4 已有 phys.speed_cap4，不该再发同判据的 phys.speed_max")

    def test_band_dwell_fails_when_rho_stays_in_band(self):
        report = self._clip(rho1=0.5, rho2=1.0, t_sw=30.0, delta=2.0)
        self.assertEqual(H.statuses(report, "cam4.switch_count"), {"PASS"})
        self.assertEqual(H.statuses(report, "cam4.band_dwell"), {"FAIL"})

    def test_t_sw_out_of_range_fails(self):
        report = self._clip(rho1=0.5, rho2=2.0, t_sw=10.0, delta=2.0)
        self.assertEqual(H.statuses(report, "cam4.t_sw"), {"FAIL"})


class FullValidSFMClipTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = Path(tempfile.mkdtemp(prefix="qc_sfm_full_"))
        f30, f15 = H.valid_sfm_files(1800)
        clip = H.write_sfm_clip(cls.tmp / "clip", f30, f15)
        cls.report = check_clip(clip, "sfm")

    @classmethod
    def tearDownClass(cls):
        shutil.rmtree(cls.tmp, ignore_errors=True)

    def test_no_fail_outside_video(self):
        fails = [r for r in self.report.results if r.status == "FAIL"]
        self.assertEqual(fails, [],
                         "; ".join(f"{r.rule}({r.target}):{r.evidence}" for r in fails))

    def test_key_groups_pass(self):
        for rule in ("pack.files", "json.schema", "json.keys", "dur.frames",
                     "frame.continuity", "frame.depthname", "sync.sfm_pair",
                     "num.quat_norm", "phys.speed_p95", "phys.speed_max"):
            self.assertEqual(H.statuses(self.report, rule), {"PASS"}, rule)


if __name__ == "__main__":
    unittest.main()

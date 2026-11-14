"""两条「标签≠事实」诚实性防线的回归（均先在本机复现过绕过）。

1. **plan.pink_honesty**：`plan.pink_coverage` 只读 `segments[].cls` 字符串——
   把导演段的 cls 原地改成 6 个 translate_*、轨迹一个数字不动，4×FAIL 变 4×PASS
   （docs/89 §5.1 给接手人的修法 (a) 恰好就是这个漏洞）。新规则把每个 translate_*
   声称对回实测轨迹：声称轴向净位移 ≥ 0.8 m 且 ≥ 窗内路径长的 50% 才 PASS。

2. **covis.ratio_strict 键集/长度**：`_strict_from_curves` 旧实现对「恰好存在的键」
   取 min——只留 1-2 一对、换成 1-4/2-4（需求明确排除第四路）、把一条曲线截成
   3 个点，三种畸形全 PASS 且证据行照打「三对全成立」。现在键集必须恰为
   {1-2,1-3,2-3}、等长、与时间轴对齐，任一不满足 ⇒ FAIL。
   宽读对照只在 `covis_ratio_any_pair` 真实存在时打印，缺失时写「宽读=未记录」，
   不再拿严读数冒充宽读自己对照自己。
"""

import shutil
import tempfile
import unittest
from pathlib import Path

import helpers as H  # noqa: E402  (helpers 负责 sys.path 引导)

from engine_qc import checks_json as cj  # noqa: E402
from engine_qc.clip_checker import check_clip  # noqa: E402
from engine_qc.config import QCConfig  # noqa: E402
from engine_qc.report import FAIL, PASS, UNVERIFIED  # noqa: E402

CFG = QCConfig()
PLAN_PATH = Path("plan.json")  # 只用于「是否随附」判断，不落盘


def _plan(cam_segs):
    """cam_segs: {cam_id: [(cls, t0, t1), ...]}"""
    return {
        "version": "1", "mode": "mc", "fps": 30, "duration_s": 60,
        "cameras": [
            {"id": cid, "segments": [{"t0": t0, "t1": t1, "cls": cls}
                                     for cls, t0, t1 in segs]}
            for cid, segs in cam_segs.items()
        ],
    }


def _honesty(plan, loaded):
    return cj.check_pink_honesty(PLAN_PATH, plan, None, loaded, CFG)


class PinkHonestyUnitTest(unittest.TestCase):
    def test_true_translation_passes(self):
        """真平移：yaw=0 面向 +z，+x 匀速 1 m/s 即「向右」；yaw=90 面向 +x 即「向前」。

        两个窗口共同锁住基底公式（窗起始帧 yaw → forward/right）。
        """
        frames_right = H.gen_cam_frames(900, [0.0, 1.6, 0.0], [1.0, 0.0, 0.0], yaw0=0.0)
        frames_fwd = H.gen_cam_frames(900, [0.0, 1.6, 0.0], [1.0, 0.0, 0.0], yaw0=90.0)
        plan = _plan({1: [("translate_right", 10, 20)],
                      2: [("translate_forward", 10, 20)]})
        out = _honesty(plan, {"1": frames_right, "2": frames_fwd})
        self.assertEqual([r.status for r in out], [PASS, PASS])
        self.assertIn("轴向净位移 10.00 m", out[0].evidence)
        self.assertIn("100%", out[0].evidence)

    def test_up_down_use_world_y(self):
        frames = H.gen_cam_frames(900, [0.0, 10.0, 0.0], [0.0, -0.5, 0.0], yaw0=37.0)
        plan = _plan({1: [("translate_down", 0, 4)]})
        out = _honesty(plan, {"1": frames})
        self.assertEqual([r.status for r in out], [PASS])

    def test_relabel_without_motion_fails(self):
        """改标签攻击的最小形态：轨迹横着走，标签写 translate_up ⇒ 必须 FAIL。"""
        frames = H.gen_cam_frames(900, [0.0, 1.6, 0.0], [1.0, 0.0, 0.0], yaw0=0.0)
        plan = _plan({1: [("translate_up", 10, 20)]})
        out = _honesty(plan, {"1": frames})
        self.assertEqual([r.status for r in out], [FAIL])
        self.assertIn("0.00 m", out[0].evidence)          # 实测轴向位移
        self.assertIn("声称的平移未在轨迹中发生", out[0].evidence)

    def test_wrong_direction_fails(self):
        """向右走却声称向左：净位移为负，同样拦。"""
        frames = H.gen_cam_frames(900, [0.0, 1.6, 0.0], [1.0, 0.0, 0.0], yaw0=0.0)
        plan = _plan({1: [("translate_left", 10, 20)]})
        out = _honesty(plan, {"1": frames})
        self.assertEqual([r.status for r in out], [FAIL])
        self.assertIn("-10.00 m", out[0].evidence)

    def test_wander_fails_fraction_criterion(self):
        """净位移够 0.8 m 但来回绕：占路径长 25% < 50% ⇒ FAIL（防原地打转也算平移）。"""
        pos = []
        for i in range(61):
            t = i / 30.0
            x = 2.5 * t if t <= 1.0 else 2.5 - 1.5 * (t - 1.0)
            pos.append([x, 1.6, 0.0])
        frames = H.gen_cam_frames(61, None, [0.0, 0.0, 0.0], yaw0=0.0, positions=pos)
        plan = _plan({1: [("translate_right", 0, 2)]})
        out = _honesty(plan, {"1": frames})
        self.assertEqual([r.status for r in out], [FAIL])
        self.assertIn("25%", out[0].evidence)

    def test_short_displacement_fails(self):
        """方向对、占比 100%，但只挪了 0.5 m < 0.8 m ⇒ FAIL。"""
        frames = H.gen_cam_frames(900, [0.0, 1.6, 0.0], [0.05, 0.0, 0.0], yaw0=0.0)
        plan = _plan({1: [("translate_right", 10, 20)]})
        out = _honesty(plan, {"1": frames})
        self.assertEqual([r.status for r in out], [FAIL])

    def test_missing_action_json_unverified(self):
        plan = _plan({2: [("translate_up", 5, 10)]})
        out = _honesty(plan, {"2": None})
        self.assertEqual([r.status for r in out], [UNVERIFIED])
        self.assertIn("无法核验", out[0].evidence)

    def test_no_translate_segments_no_entries(self):
        """导演段/observer 段不产生条目——「缺平移」由 pink_coverage 负责。"""
        frames = H.gen_cam_frames(120, [0.0, 1.6, 0.0], [1.0, 0.0, 0.0])
        plan = _plan({1: [("mounted_look_left", 5, 8)], 4: [("observer_rho", 0, 60)]})
        self.assertEqual(_honesty(plan, {"1": frames, "4": frames}), [])

    def test_plan_absent_no_entries(self):
        self.assertEqual(cj.check_pink_honesty(None, None, None, {}, CFG), [])

    def test_unknown_translate_cls_fails(self):
        frames = H.gen_cam_frames(120, [0.0, 1.6, 0.0], [1.0, 0.0, 0.0])
        plan = _plan({1: [("translate_diagonal", 0, 3)]})
        out = _honesty(plan, {"1": frames})
        self.assertEqual([r.status for r in out], [FAIL])
        self.assertIn("未知平移类目", out[0].evidence)

    def test_window_outside_frames_fails(self):
        """声称的窗口在轨迹尾巴之外：平移在数据里不存在，不能靠越界静默放过。"""
        frames = H.gen_cam_frames(300, [0.0, 1.6, 0.0], [1.0, 0.0, 0.0])  # 10s
        plan = _plan({1: [("translate_right", 50, 55)]})
        out = _honesty(plan, {"1": frames})
        self.assertEqual([r.status for r in out], [FAIL])


class PinkHonestyClipTest(unittest.TestCase):
    """端到端：coverage 看标签全绿、honesty 看轨迹抓假。"""

    @classmethod
    def setUpClass(cls):
        cls.tmp = Path(tempfile.mkdtemp(prefix="qc_pink_"))
        cams = H.valid_mc_cams(1800)
        # cam1：vel=[1,0,0]、yaw0=0 ⇒ 「向右」为真；「向上」为假（改标签攻击形态）
        plan = _plan({1: [("translate_right", 10, 20), ("translate_up", 30, 40)],
                      2: [], 3: [], 4: []})
        clip = H.write_mc_clip(cls.tmp / "clip", cams, extra={"plan.json": plan})
        cls.report = check_clip(clip, "mc")

    @classmethod
    def tearDownClass(cls):
        shutil.rmtree(cls.tmp, ignore_errors=True)

    def test_true_claim_passes_fake_claim_fails(self):
        got = {(r.target, r.status): r.evidence
               for r in H.results_by_rule(self.report, "plan.pink_honesty")}
        self.assertEqual(len(got), 2)
        self.assertIn(("action_camera_1.json", PASS), got)
        self.assertIn(("action_camera_1.json", FAIL), got)

    def test_fake_claim_fails_clip(self):
        self.assertEqual(self.report.verdict, FAIL)


class CovisPairShapeTest(unittest.TestCase):
    """严读闸门的键集/长度断言——三种实测畸形 + 宽读对照冒充。"""

    def _strict(self, doc):
        return cj.check_covis_strict(Path("covis.json"), doc, None, CFG)

    def test_wellformed_passes_with_wide_note(self):
        r = self._strict(H.covis_doc(0.62))
        self.assertEqual(r.status, PASS)
        self.assertIn("宽读对照=", r.evidence)

    def test_missing_pairs_fail(self):
        """只留 1-2 一对：min 退化成单对宽读，旧实现照常 PASS——现在必须 FAIL。"""
        doc = H.covis_doc(0.62)
        del doc["pair_curves"]["1-3"]
        del doc["pair_curves"]["2-3"]
        r = self._strict(doc)
        self.assertEqual(r.status, FAIL)
        self.assertIn("缺对 1-3、2-3", r.evidence)

    def test_swapped_pairs_fail(self):
        """换成 1-4/2-4：需求明确排除第四路，这些对不具备放行资格。"""
        doc = H.covis_doc(0.62)
        doc["pair_curves"]["1-4"] = doc["pair_curves"].pop("1-3")
        doc["pair_curves"]["2-4"] = doc["pair_curves"].pop("2-3")
        r = self._strict(doc)
        self.assertEqual(r.status, FAIL)
        self.assertIn("键集应恰为", r.evidence)

    def test_truncated_curve_fails(self):
        """把一条曲线截成 3 个点：旧实现 min(len)=3、拿 3 点代表全片。"""
        doc = H.covis_doc(0.62)
        doc["pair_curves"]["2-3"] = doc["pair_curves"]["2-3"][:3]
        r = self._strict(doc)
        self.assertEqual(r.status, FAIL)
        self.assertIn("长度不一致", r.evidence)

    def test_curves_misaligned_with_ts_fail(self):
        """三条同时截短能骗过等长断言，但对不上 pair_min_overlap_ts 时间轴。"""
        doc = H.covis_doc(0.62)
        for k in ("1-2", "1-3", "2-3"):
            doc["pair_curves"][k] = doc["pair_curves"][k][:3]
        r = self._strict(doc)
        self.assertEqual(r.status, FAIL)
        self.assertIn("不对齐", r.evidence)

    def test_reading_also_rejects_malformed_curves(self):
        doc = H.covis_doc(0.62)
        del doc["pair_curves"]["1-3"]
        r = cj.check_covis_reading(Path("covis.json"), doc, None, CFG)
        self.assertEqual(r.status, FAIL)
        self.assertIn("pair_curves 形状非法", r.evidence)

    def test_absent_curves_still_skip(self):
        """键整个不存在 ≠ 形状非法：维持既有 SKIP（退回 covis.ratio）语义。"""
        r = self._strict(H.covis_doc(0.62, with_curves=False))
        self.assertEqual(r.status, "SKIP")

    def test_missing_any_pair_not_impersonated(self):
        """covis_ratio_any_pair 缺失：证据行写「宽读=未记录」，不再打严读数冒充。"""
        doc = H.covis_doc(0.62)
        del doc["covis_ratio_any_pair"]
        r = self._strict(doc)
        self.assertEqual(r.status, PASS)
        self.assertIn("宽读=未记录", r.evidence)
        self.assertNotIn("宽读对照", r.evidence)


if __name__ == "__main__":  # pragma: no cover
    unittest.main()

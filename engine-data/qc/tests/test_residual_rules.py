"""sfm-d1-20260805 评审残余三条规则的口径修正（docs/88 辩题二十一/二十二/二十三）。

三条的共同点：真机首条 clip 上 FAIL 的不是数据，是规则口径——
* num.input_stream 把 docs/69 的 MC 验收套到了 sfm 非 Open 轨迹上，且逐步
  转动阈值随采样步长漂移（同一条轨迹 30fps「没转」、15fps「在转」）；
* num.player_speed 对抽帧车道重复做 1/15s 差分，把 30fps 瞬时值拷贝读成偏差；
* depth.align 把「残差分离不出 ±1 帧」判成 FAIL——测不了 ≠ 不合格。

负例同样在列：改完的规则必须还抓得住各自原本要抓的缺陷形态。
"""

import copy
import shutil
import tempfile
import unittest
from pathlib import Path

import helpers as H

from engine_qc import checks_json as cj
from engine_qc.clip_checker import check_clip
from engine_qc.config import QCConfig
from engine_qc.report import FAIL, PASS, SKIP, UNVERIFIED


def _rot_frames(n=60, step_deg=0.2, zero_mouse=True, stride=1):
    """慢转镜头：每 30fps 帧转 step_deg°。stride=2 模拟 15fps 抽帧车道。"""
    frames = []
    for i in range(0, n, stride):
        f = H.sfm_frame(i, [0.0, 1.6, 0.0], H.yaw_quat(step_deg * i), [0.0, 0.0, 0.0])
        if zero_mouse:
            f["mouse_dx"] = f["mouse_dy"] = 0.0
        frames.append(f)
    return frames


class InputStreamScopeTest(unittest.TestCase):
    """docs/68 0722：键鼠只在 Open 轨迹强制；判「在转」用累计角，不随步长漂移。"""

    def setUp(self):
        self.cfg = QCConfig()
        # 60 帧 × 0.2°/帧 = 12° 累计转动，超过 10° 门槛
        self.frames = _rot_frames()

    def test_cadence_invariant_turn_detection(self):
        """同一条轨迹，30fps 与 15fps 抽帧必须给出同一个「在转」结论。

        旧口径的逐步阈值（≈0.36°/步）恰好卡在 0.2°/帧与 0.4°/帧之间：
        30fps 判「没转」放行、15fps 判「在转」打挂，同一份数据两个结论。
        """
        r30 = cj.check_input_stream("30fps", _rot_frames(stride=1), self.cfg,
                                    "sfm", "Open-环境交互")
        r15 = cj.check_input_stream("15fps", _rot_frames(stride=2), self.cfg,
                                    "sfm", "Open-环境交互")
        self.assertEqual((r30.status, r15.status), (FAIL, FAIL))

    def test_sfm_non_open_all_zero_passes(self):
        r = cj.check_input_stream("15fps", self.frames, self.cfg, "sfm", "Loop-有位移")
        self.assertEqual(r.status, PASS)
        self.assertIn("非 Open", r.evidence)

    def test_sfm_open_all_zero_fails(self):
        r = cj.check_input_stream("15fps", self.frames, self.cfg, "sfm", "Open-强交互-战斗")
        self.assertEqual(r.status, FAIL)

    def test_sfm_unknown_traj_is_unverified(self):
        """类目不明时不能二选一：judged PASS 是「没检查读成通过」，FAIL 冤枉非 Open。"""
        r = cj.check_input_stream("15fps", self.frames, self.cfg, "sfm", None)
        self.assertEqual(r.status, UNVERIFIED)

    def test_mc_all_zero_still_fails(self):
        """69 §8.2 对 MC 的要求不变。"""
        r = cj.check_input_stream("cam1", self.frames, self.cfg, "mc", None)
        self.assertEqual(r.status, FAIL)

    def test_not_turned_passes_regardless(self):
        static = _rot_frames(step_deg=0.0)
        r = cj.check_input_stream("15fps", static, self.cfg, "sfm", None)
        self.assertEqual(r.status, PASS)

    def test_nonzero_mouse_passes(self):
        frames = _rot_frames(zero_mouse=False)
        for f in frames:
            f["mouse_dx"] = 0.01
        r = cj.check_input_stream("15fps", frames, self.cfg, "sfm", "Open-环境交互")
        self.assertEqual(r.status, PASS)


class PlayerSpeedSubsampledTest(unittest.TestCase):
    """抽帧车道不重复差分；30fps 车道的差分核验原样保留。"""

    def setUp(self):
        self.cfg = QCConfig()

    def test_subsampled_lane_skips(self):
        f15 = [H.sfm_frame(i, [i / 30.0, 1.6, 0.0], H.yaw_quat(0.0),
                           [1.0, 0.0, 0.0], fps15=True) for i in range(0, 60, 2)]
        r = cj.check_player_speed("15fps", f15, 2 / 30.0, self.cfg, subsampled=True)
        self.assertEqual(r.status, SKIP)
        self.assertIn("sync.sfm_pair", r.evidence)

    def test_full_rate_lane_still_catches_wrong_baseline(self):
        """负例保底：30fps 车道上「差分基准错取第 0 帧」的历史缺陷仍要 FAIL。"""
        f30 = [H.sfm_frame(i, [i / 30.0, 1.6, 0.0], H.yaw_quat(0.0),
                           [(i / 30.0) * 30.0, 0.0, 0.0])  # (pos[k]-pos[0])·fps
               for i in range(60)]
        r = cj.check_player_speed("30fps", f30, 1 / 30.0, self.cfg)
        self.assertEqual(r.status, FAIL)


class SfmPairIdentityTest(unittest.TestCase):
    """sync.sfm_pair 从「对数」升级为「逐字段对账」——抽帧车道的数值正确性锚点。"""

    def setUp(self):
        self.f30, self.f15 = H.valid_sfm_files(120)

    def test_valid_pair_passes(self):
        r = cj.check_sfm_pair("30", self.f30, "15", self.f15)
        self.assertEqual(r.status, PASS)
        self.assertIn("逐字段", r.evidence)

    def test_diverged_speed_fails(self):
        """正是 sfm-d1 评审里担心的静默分叉：抽帧时重算 speed → 两车道口径漂移。"""
        f15 = copy.deepcopy(self.f15)
        f15[3]["player_speed"] = [9.9, 0.0, 0.0]
        r = cj.check_sfm_pair("30", self.f30, "15", f15)
        self.assertEqual(r.status, FAIL)
        self.assertIn("player_speed", r.evidence)

    def test_frame_not_in_30fps_fails(self):
        f15 = copy.deepcopy(self.f15)
        f15[0]["frame"] = 999
        r = cj.check_sfm_pair("30", self.f30, "15", f15)
        self.assertEqual(r.status, FAIL)

    def test_count_mismatch_still_fails(self):
        r = cj.check_sfm_pair("30", self.f30, "15", self.f15[:-1])
        self.assertEqual(r.status, FAIL)


class FullClipTrajClassTest(unittest.TestCase):
    """check_clip 层的类目解析：显式 > clip 根 plan.json > 兄弟 plan/plan.json。"""

    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="qc_trajcls_"))
        # 带转动 + 零输入流的最小 sfm clip：input_stream 的分支全由类目决定
        n = 120
        f30 = [dict(H.sfm_frame(i, [0.0, 1.6, 0.0], H.yaw_quat(0.2 * i),
                                [0.0, 0.0, 0.0]), mouse_dx=0.0, mouse_dy=0.0)
               for i in range(n)]
        f15 = [dict(f30[i], depth_name=f"depth_{i}") for i in range(0, n, 2)]
        self.clip = self.tmp / "run" / "clip"
        H.write_sfm_clip(self.clip, f30, f15)

    def tearDown(self):
        shutil.rmtree(self.tmp, ignore_errors=True)

    def _status(self, **kw):
        report = check_clip(self.clip, "sfm", **kw)
        return {r.target: r.status for r in report.results
                if r.rule == "num.input_stream"}

    def test_explicit_class_wins(self):
        s = self._status(traj_class="Loop-有位移")
        self.assertEqual(set(s.values()), {PASS})

    def test_sibling_plan_dir_fallback(self):
        """引擎跑录布局 <run>/{plan,clip}：QC 直接吃 plan/plan.json 的 traj_class。"""
        plan_dir = self.clip.parent / "plan"
        plan_dir.mkdir()
        (plan_dir / "plan.json").write_text('{"traj_class": "Loop-有位移"}',
                                            encoding="utf-8")
        s = self._status()
        self.assertEqual(set(s.values()), {PASS})

    def test_unknown_class_is_unverified_and_blocks_pass(self):
        report = check_clip(self.clip, "sfm")
        statuses = {r.status for r in report.results if r.rule == "num.input_stream"}
        self.assertEqual(statuses, {UNVERIFIED})
        self.assertNotEqual(report.verdict, PASS)


if __name__ == "__main__":
    unittest.main()

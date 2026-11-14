"""68 号轨迹库：类目表完整性、拓扑语义自检、导出键序、配额账本。

写成 unittest（而非本目录既有的裸 test_* 函数）是为了在**没装 pytest 的机器上也能跑**
——unittest.TestCase 同样会被 pytest 收集，CI 侧行为不变。
"""

import json
import math
import os
import sys
import tempfile
import unittest

_ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
if _ROOT not in sys.path:
    sys.path.insert(0, _ROOT)

from trajgen import sfm, sfm_export
from trajgen.scene import SCENE_KINDS, synthetic

SCENE = synthetic("open_plaza", 3)


class ClassTableTest(unittest.TestCase):
    def test_sixteen_classes(self):
        self.assertEqual(len(sfm.NAVIGATE), 13)
        self.assertEqual(len(sfm.SPECIAL), 3)
        self.assertEqual(len(sfm.ALL_CLASSES), 16)

    def test_navigate_quota_is_98_percent_as_the_spec_states(self):
        """68 号 §3.3 的比例合计就是 98%——那 2% 差额是需求文档自身的问题
        （已进 docs/70 §9 第 6 条待确认）。这里如实断言 98%，不硬凑成 100%。"""
        total = sum(c.quota for c in sfm.NAVIGATE)
        self.assertAlmostEqual(total, 0.98, places=9)

    def test_special_quota_is_20_percent(self):
        self.assertAlmostEqual(sum(c.quota for c in sfm.SPECIAL), 0.20, places=9)

    def test_durations_are_whitelisted(self):
        for c in sfm.ALL_CLASSES:
            for d in c.dur_weights:
                self.assertIn(d, sfm.DURATIONS, c.name)

    def test_stationary_classes_only_60s(self):
        for name in ("Loop-原地摇镜", "Revisit-原地摇镜", "Stay-完全静止"):
            self.assertEqual(set(sfm.BY_NAME[name].dur_weights), {60.0}, name)

    def test_class_names_are_the_upload_path_whitelist(self):
        # docs/68 §8：「运镜种类」目录名必须用统一轨迹名
        self.assertEqual(len(set(sfm.CLASS_NAMES)), 16)
        self.assertIn("Open-强交互-战斗", sfm.CLASS_NAMES)
        self.assertIn("人物特写", sfm.CLASS_NAMES)

    def test_unknown_class_raises(self):
        with self.assertRaises(KeyError):
            sfm.generate(SCENE, 1, cls_name="不存在的类目")

    def test_duration_outside_whitelist_raises(self):
        with self.assertRaises(ValueError):
            sfm.generate(SCENE, 1, cls_name="Loop-有位移", duration=120.0)


class TopologyTest(unittest.TestCase):
    """每个类目的语义都是验收项，逐条断言。"""

    def test_loop_move_closes(self):
        p, _ = sfm.plan_clip(SCENE, 5, cls_name="Loop-有位移", duration=60.0)
        self.assertLessEqual(p.loop_closure_m, sfm.LOOP_POS_TOL_M, p.reasons)
        self.assertLessEqual(p.loop_closure_deg, sfm.LOOP_ROT_TOL_DEG, p.reasons)

    def test_loop_pan_is_stationary_and_closes(self):
        p, _ = sfm.plan_clip(SCENE, 6, cls_name="Loop-原地摇镜", duration=60.0)
        span = max(math.dist(p.positions[0], q) for q in p.positions)
        self.assertLess(span, 0.5, "原地摇镜不应有可观位移")
        self.assertLessEqual(p.loop_closure_deg, sfm.LOOP_ROT_TOL_DEG)

    def test_revisit_pan_does_NOT_close(self):
        """68 号对这一类的要求恰好相反：首尾帧**不**一致，rotation 不同。"""
        p, _ = sfm.plan_clip(SCENE, 7, cls_name="Revisit-原地摇镜", duration=60.0)
        self.assertGreaterEqual(p.loop_closure_deg, sfm.REVISIT_PAN_MIN_DEG)

    def test_revisit_move_returns_to_an_old_place(self):
        p, _ = sfm.plan_clip(SCENE, 8, cls_name="Revisit-有位移", duration=60.0)
        self.assertLessEqual(p.revisit_min_m, sfm.REVISIT_RETURN_M, p.reasons)

    def test_oscillating_repeats_at_least_twice(self):
        for name in ("Oscillating-侧向", "Oscillating-上下", "Oscillating-前后"):
            p, _ = sfm.plan_clip(SCENE, 9, cls_name=name, duration=60.0)
            self.assertGreaterEqual(p.osc_cycles, sfm.OSC_MIN_CYCLES, name)

    def test_stay_is_bit_exactly_still(self):
        """「rotation、location 完全无变化」——这里是字面的完全，不是近似。"""
        p, _ = sfm.plan_clip(SCENE, 10, cls_name="Stay-完全静止", duration=60.0)
        self.assertTrue(p.ok, p.reasons)
        self.assertEqual(len(set(p.positions)), 1)
        self.assertEqual(len(set(p.quats)), 1)

    def test_stay_bypasses_shell_projection(self):
        # 静止类若过自由壳投影会被推离原位，破坏「完全无变化」
        p, _ = sfm.plan_clip(SCENE, 11, cls_name="Stay-完全静止", duration=60.0)
        self.assertEqual(p.max_push_m, 0.0)


class KinematicsTest(unittest.TestCase):
    def test_all_classes_generate_finite_and_mostly_pass(self):
        ok = total = 0
        for c in sfm.ALL_CLASSES:
            for seed in (11, 22, 33):
                p, _ = sfm.plan_clip(SCENE, seed, cls_name=c.name)
                total += 1
                for v in p.positions[:50]:
                    self.assertTrue(all(math.isfinite(x) for x in v), c.name)
                for q in p.quats[:50]:
                    self.assertAlmostEqual(math.sqrt(sum(x * x for x in q)), 1.0, places=5)
                if p.ok:
                    ok += 1
        # 编排器的批成功率线是 80%（docs/78 S3）
        self.assertGreaterEqual(ok / total, 0.80, "自检通过率 %d/%d" % (ok, total))

    def test_frame_count_matches_duration(self):
        for dur in sfm.DURATIONS:
            p, _ = sfm.plan_clip(SCENE, 3, cls_name="Loop-有位移", duration=dur)
            self.assertEqual(p.n_frames, int(round(dur * sfm.FPS)))

    def test_selfcheck_reports_reasons_when_failing(self):
        # 拿一个明知会顶穿的场景类型碰运气，只断言「失败时必须给出原因」
        for kind in SCENE_KINDS:
            sc = synthetic(kind, 3)
            for seed in (1, 2, 3):
                p = sfm.generate(sc, seed, cls_name="主体环绕", duration=60.0)
                if not p.ok:
                    self.assertTrue(p.reasons, "失败必须带 reasons")
                    return


class ExportTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.plan, _ = sfm.plan_clip(SCENE, 101, cls_name="Loop-有位移", duration=60.0)
        cls.dir = tempfile.mkdtemp(prefix="sfm_export_")
        sfm_export.write_clip(cls.plan, cls.dir)

    def _load(self, name):
        with open(os.path.join(self.dir, name), encoding="utf-8") as f:
            return json.load(f)

    def test_key_order_matches_demand_side_sample(self):
        """键序对齐 PR #405 归档的样例——SFM 与 MC 的键名怪癖相反，不能互抄。"""
        ref_path = os.path.join(_ROOT, "..", "fixtures",
                                "sfm_action_camera_15fps.sample.json")
        with open(ref_path, encoding="utf-8") as f:
            ref = json.load(f)
        got = self._load("action_camera_15fps.json")
        self.assertEqual(list(got[0].keys()), list(ref[0].keys()))

    def test_15fps_is_even_frame_projection_of_30fps(self):
        f30 = self._load("action_camera_30fps.json")
        f15 = self._load("action_camera_15fps.json")
        self.assertEqual(len(f15), math.ceil(len(f30) / 2))
        self.assertEqual([r["frame"] for r in f15[:4]], [0, 2, 4, 6])
        # 同一帧号的位姿必须逐字相同——对齐是构造性的，不是事后凑的
        for r in f15[:20]:
            self.assertEqual(r["camera_position"], f30[r["frame"]]["camera_position"])

    def test_depth_name_matches_frame(self):
        for r in self._load("action_camera_15fps.json")[:10]:
            self.assertEqual(r["depth_name"], "depth_%d" % r["frame"])

    def test_30fps_has_no_depth_name(self):
        self.assertNotIn("depth_name", self._load("action_camera_30fps.json")[0])

    def test_intrinsics_are_square_pixel(self):
        k = self._load("action_camera_15fps.json")[0]["camera_intrinsics"]
        self.assertEqual(k["fx"], k["fy"], "docs/75 问题 6 已固化：方形像素")
        self.assertEqual((k["cx"], k["cy"]), (960.0, 540.0))

    def test_no_mc_key_names_leak_in(self):
        rec = self._load("action_camera_15fps.json")[0]
        for bad in ("camera_Follow Offset", "mouse_x", "mouse_y", "camera_rotation_oula"):
            self.assertNotIn(bad, rec, "MC 键名不得混入 SFM")

    def test_systeminfo_is_fullscreen_1080p(self):
        s = self._load("systeminfo.json")
        self.assertEqual((s["x"], s["y"]), (0, 0))
        self.assertEqual((s["width"], s["height"]), (1920, 1080))
        self.assertEqual(s["recordDpi"], 1.0)

    def test_map_meta_carries_bounds_and_scale(self):
        """docs/68 §6.1 的两条前置约定——引擎侧是 authoring 数据，直接能给。"""
        m = self._load("map_meta.json")
        self.assertEqual(m["metric_scale"], 1.0)
        self.assertEqual(len(m["ground_corners_xz"]), 4)
        self.assertEqual(len(m["y_range"]), 2)
        self.assertEqual(m["shape"], "AABB")

    def test_plan_records_class_and_selfcheck(self):
        p = self._load("plan.json")
        self.assertEqual(p["mode"], "sfm")
        self.assertEqual(p["traj_class"], "Loop-有位移")
        self.assertIn("selfcheck", p)
        self.assertTrue(p["selfcheck"]["ok"])


class QuotaTest(unittest.TestCase):
    def test_ledger_converges_toward_targets(self):
        import random
        q = sfm.SfmQuota()
        rng = random.Random(0)
        for _ in range(400):
            q.add(q.next_class(rng).name)
        rep = {r["class"]: r for r in q.report()}
        for c in sfm.NAVIGATE:
            self.assertLess(abs(rep[c.name]["delta"]), 0.03,
                            "%s 偏差 %.3f" % (c.name, rep[c.name]["delta"]))

    def test_report_covers_every_class(self):
        q = sfm.SfmQuota()
        self.assertEqual(len(q.report()), 16)


if __name__ == "__main__":
    unittest.main()

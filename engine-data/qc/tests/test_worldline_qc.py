"""worldline_qc 单元测试：README 解析 / 布局识别 / 焦距反推 / 欧拉角 / 视锥 / 运动规则 / 输入一致性 / 世界线语义 / 条款目录。

全部用合成小样本（wl_helpers），不需要 ffmpeg 与 opencv。
"""

import math
import shutil
import tempfile
import unittest
from pathlib import Path

import helpers as H  # noqa: F401
import wl_helpers as W

from engine_qc.mathutil import quat_forward
from worldline_qc.checks_geometry import check_geometry, in_frustum, stage_center
from worldline_qc.checks_json import LaneData, check_lane_fields, focal_from_fx, unity_euler_deg
from worldline_qc.checks_motion import basis, check_a_independence, check_derived_input, check_input_consistency, check_lane_motion
from worldline_qc.checks_worldline import check_worldlines, parse_causal, parse_preroll_s, parse_takeover, takeover_frames
from worldline_qc.loaders import discover, parse_readme_lane_table
from worldline_qc.checks_msync import check_multicam_sync
from worldline_qc.model import FAIL, MANUAL, PASS, SKIP, UNVERIFIED, WARN
from worldline_qc.pipeline import check_group
from worldline_qc.requirements import CATALOG, uncatalogued_rules


def _load(g):
    return {lf.info.key: LaneData(lf) for lf in g.lanes}


def _one(results, rule, target=None):
    for r in results:
        if r.rule == rule and (target is None or r.target == target):
            return r
    raise AssertionError(f"no {rule} @ {target}: {[ (r.rule, r.target) for r in results ][:20]}")


class ReadmeAndLayoutTest(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())

    def tearDown(self):
        shutil.rmtree(self.tmp, ignore_errors=True)

    def test_readme_lane_table(self):
        t = parse_readme_lane_table(W.README_TMPL.format(name="x", c_text=""))
        self.assertEqual(t[("AB", 4)], ("B", "free_camera", 18.0))
        self.assertEqual(t[("C", 3)], ("C", "chase", 24.0))
        self.assertEqual(t[("D", 1)], ("D", "fixed", 35.0))

    def test_discover_take_layout(self):
        root = W.write_group(self.tmp / "g", n=30)
        g = discover(root)
        self.assertEqual(g.layout, "take")
        self.assertEqual(len(g.lanes), 12)
        self.assertEqual(g.lane("AB", 4).info.worldline, "B")
        self.assertEqual(g.lane("C", 4).info.role, "first_person")
        self.assertEqual(g.lane("D", 2).info.global_index, 10)
        self.assertEqual(g.lane("AB", 2).info.focal_mm, 24.0)

    def test_the_notes_and_the_manifest_beside_the_folder_are_found(self):
        root = W.write_group(self.tmp / "g", n=30)
        (root / "README.md").rename(self.tmp / "g.README.md")        # as stage-group-deliver writes them
        (root / "manifest.json").rename(self.tmp / "g.manifest.json")
        g = discover(root)
        self.assertIsNotNone(g.manifest)
        self.assertIsNotNone(g.readme)
        self.assertEqual(g.layout, "take")
        (self.tmp / "g.manifest.json").write_text("{not json", encoding="utf-8")
        self.assertTrue(any("manifest.json" in p for p in discover(root).problems))

    def test_discover_flat_layout(self):
        root = self.tmp / "2026_09_24_beach_001"
        root.mkdir()
        for i in range(1, 13):
            (root / f"action_camera_{i}.json").write_text("[]")
        g = discover(root)
        self.assertEqual(g.layout, "flat")
        self.assertEqual(g.lane("flat", 7).info.role, "chase")
        self.assertEqual(g.lane("flat", 4).info.worldline, "B")
        self.assertEqual([l.info.worldline for l in g.lanes], list("AAABCCCCDDDD"))

    def test_causal_and_takeover_parsing(self):
        readme = W.README_TMPL.format(name="x", c_text=W.C_TEXT_NO_SEMANTICS)
        c = parse_causal(readme)
        self.assertEqual(set(c), set("ABCD"))
        self.assertEqual(parse_preroll_s(readme), 1.0)
        self.assertEqual(parse_takeover(c["C"], 1.0), (2.0, 1.0))
        self.assertEqual(parse_takeover("第 20 秒玩家接管 pacer", 1.0), (21.0, 20.0))
        self.assertEqual(parse_takeover("没有接管", 1.0), (None, None))

    def test_the_delivery_readme_sections_are_read(self):
        """交付包的「### C（…）」分节写法：节内各条连成 C 的因果记录；节后的全组说明不算进 D；没有静止段。"""
        c = parse_causal(W.DELIVERY_WORLDLINES)
        self.assertEqual(set(c), set("ABCD"))
        self.assertIn("第 2 秒（第 61 帧）起由程序键鼠接管", c["C"])
        self.assertIn("第 3.0 秒 玩家挥拳攻击路人", c["C"])
        self.assertNotIn("第 9 秒", c["D"])
        self.assertEqual(parse_preroll_s(W.DELIVERY_WORLDLINES), 0.0)
        self.assertEqual(parse_takeover(c["C"], 0.0), (2.0, 2.0))


class MathTest(unittest.TestCase):
    def test_focal_from_fx(self):
        self.assertAlmostEqual(focal_from_fx(1866.666638, 1920), 35.0, places=4)
        self.assertAlmostEqual(focal_from_fx(959.999992, 1920), 18.0, places=4)

    def test_unity_euler_yaw(self):
        e = unity_euler_deg(W.yaw_quat(30.0))
        self.assertAlmostEqual(e[1], 30.0, places=5)
        self.assertAlmostEqual(e[0], 0.0, places=5)

    def test_look_at_basis_is_forward(self):
        q = W.look_at_quat((0.0, 3.0, -10.0), W.CENTER)
        f = quat_forward(q)
        R, U, F = basis(q)
        self.assertAlmostEqual(sum((a - b) ** 2 for a, b in zip(f, F)), 0.0, places=9)
        # forward 指向中心
        d = [W.CENTER[i] - (0.0, 3.0, -10.0)[i] for i in range(3)]
        n = math.sqrt(sum(x * x for x in d))
        self.assertAlmostEqual(sum(F[i] * d[i] for i in range(3)) / n, 1.0, places=6)
        # 右手/上向量正交且 up 朝上
        self.assertAlmostEqual(sum(R[i] * F[i] for i in range(3)), 0.0, places=6)
        self.assertGreater(U[1], 0.9)

    def test_in_frustum(self):
        pos, q = (0.0, 3.0, -10.0), W.look_at_quat((0.0, 3.0, -10.0), W.CENTER)
        intr = {"fx": 1866.67, "fy": 1866.67, "cx": 960, "cy": 540}
        self.assertTrue(in_frustum(pos, q, intr, (1920, 1080), W.CENTER))
        self.assertFalse(in_frustum(pos, q, intr, (1920, 1080), (0.0, 3.0, -20.0)))    # 背后
        self.assertFalse(in_frustum(pos, q, intr, (1920, 1080), (8.0, 1.0, 0.0)))      # 侧面超出 HFOV(~54°)
        self.assertTrue(in_frustum(pos, q, intr, (1920, 1080), (8.0, 1.0, 0.0), margin_m=3.0))


class MotionRulesTest(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())

    def tearDown(self):
        shutil.rmtree(self.tmp, ignore_errors=True)

    def test_six_dir_orbit_fails_free_passes(self):
        root = W.write_group(self.tmp / "g", n=150)
        g = discover(root)
        lanes = _load(g)
        r = _one(check_lane_motion(lanes["AB/2"]), "motion.six_dir")
        self.assertEqual(r.status, FAIL)
        self.assertIn("缺", r.evidence)
        root2 = W.write_group(self.tmp / "g2", n=150, orbit_is_six_dir=True)
        lanes2 = _load(discover(root2))
        r2 = _one(check_lane_motion(lanes2["AB/2"]), "motion.six_dir")
        self.assertEqual(r2.status, PASS, r2.evidence)
        # B 与游戏镜头不套用
        self.assertEqual(_one(check_lane_motion(lanes["AB/4"]), "motion.six_dir").status, SKIP)
        self.assertEqual(_one(check_lane_motion(lanes["C/3"]), "motion.six_dir").status, SKIP)
        self.assertEqual(_one(check_lane_motion(lanes["C/1"]), "motion.six_dir").status, SKIP)

    def test_physics_rules_present_and_pass_on_slow_synthetic(self):
        lanes = _load(discover(W.write_group(self.tmp / "g", n=150)))
        res = check_lane_motion(lanes["AB/3"])
        for rule in ("motion.speed_p95", "motion.speed_max", "motion.angspeed_p95", "motion.roll", "motion.segments"):
            self.assertEqual(_one(res, rule).status, PASS if rule != "motion.segments" else MANUAL, rule)

    def test_a_independence(self):
        g = discover(W.write_group(self.tmp / "g", n=150))
        lanes = _load(g)
        self.assertEqual(_one(check_a_independence(g, lanes), "motion.a_independence").status, PASS)
        # 两路完全相同 → 同步占比 100% → FAIL
        lanes["AB/3"].records = lanes["AB/2"].records
        self.assertEqual(_one(check_a_independence(g, lanes), "motion.a_independence").status, FAIL)

    def test_b_input_consistency(self):
        lanes = _load(discover(W.write_group(self.tmp / "g", n=150, b_use_qe=True)))
        res = check_input_consistency(lanes["AB/4"])
        self.assertEqual(_one(res, "input.keys_drive_camera").status, PASS, _one(res, "input.keys_drive_camera").evidence)
        self.assertEqual(_one(res, "input.eight_directions").status, PASS, _one(res, "input.eight_directions").evidence)
        bad = _load(discover(W.write_group(self.tmp / "h", n=150, b_contradict=True)))
        self.assertEqual(_one(check_input_consistency(bad["AB/4"]), "input.eight_directions").status, WARN)
        self.assertEqual(_one(res, "input.qe_lift").status, PASS)
        self.assertEqual(_one(res, "input.mouse_drives_view").status, PASS, _one(res, "input.mouse_drives_view").evidence)
        bad = _load(discover(W.write_group(self.tmp / "b", n=150, b_contradict=True)))
        res = check_input_consistency(bad["AB/4"])
        self.assertEqual(_one(res, "input.keys_drive_camera").status, FAIL)
        self.assertEqual(_one(res, "input.qe_lift").status, FAIL)     # 无 Q/E 且高度恒定

    def test_a_free_path_creeping_too_slowly_to_see_needs_no_key(self):
        from worldline_qc.checks_motion import PATH_MOVING_MPS
        lanes = _load(discover(W.write_group(self.tmp / "g", n=150)))
        lane = lanes["C/2"]
        still = lane.records[0]["camera_position"]
        for creep, want in ((0.2, PASS), (0.5, FAIL)):   # m/s along the camera's own right, no key in the JSON
            lane.records = [dict(r, camera_position=[still[0] + creep * i / 30.0, still[1], still[2]], keyCode=[],
                                 camera_rotation_quaternion=[0.0, 0.0, 0.0, 1.0], mouse_dx=0.0, mouse_dy=0.0)
                            for i, r in enumerate(lane.records)]
            res = _one(check_derived_input(lane), "input.path_keys")
            self.assertEqual(res.status, want, (creep, res.evidence))
        self.assertEqual(PATH_MOVING_MPS, 0.30)

    def test_free_path_keys_follow_the_path_and_fixed_cameras_press_nothing(self):
        """需方 2026-09-29：自由运镜的键鼠按运镜路线换算，固定机位全空。"""
        lanes = _load(discover(W.write_group(self.tmp / "g", n=150, orbit_is_six_dir=True)))
        for key in ("AB/2", "AB/3", "C/2"):   # 前/后/右/左/上/下各一段、横移、环绕
            res = check_derived_input(lanes[key])
            self.assertEqual(_one(res, "input.path_keys").status, PASS, (key, _one(res, "input.path_keys").evidence))
            self.assertEqual(_one(res, "input.path_mouse").status, PASS, (key, _one(res, "input.path_mouse").evidence))
        six = _one(check_derived_input(lanes["AB/2"]), "input.path_keys").data["agree"]
        self.assertEqual(six, 1.0)
        self.assertEqual(_one(check_derived_input(lanes["AB/1"]), "input.fixed_none").status, PASS)
        self.assertEqual(check_derived_input(lanes["AB/4"]), [])   # B 是真实输入，走 input.keys_drive_camera
        # A 与 D 对调：键与运动相反
        swapped = lanes["AB/2"]
        swapped.records = [dict(r, keyCode=[{65: 68, 68: 65}.get(k, k) for k in r["keyCode"]]) for r in swapped.records]
        self.assertEqual(_one(check_derived_input(swapped), "input.path_keys").status, FAIL)
        # 在动却全程没键；鼠标反号
        bare = lanes["C/2"]
        bare.records = [dict(r, keyCode=[], mouse_dx=-r["mouse_dx"]) for r in bare.records]
        res = check_derived_input(bare)
        self.assertEqual(_one(res, "input.path_keys").status, FAIL)
        self.assertEqual(_one(res, "input.path_mouse").status, FAIL)
        # 老格式：没有键鼠字段——判不合格，不能当成「没按键」，也不能崩
        for key, rule in (("D/2", "input.path_keys"), ("D/1", "input.fixed_none")):
            old = lanes[key]
            old.records = [{k: v for k, v in r.items() if k not in ("keyCode", "mouse_dx", "mouse_dy", "mouseX", "mouseY")}
                           for r in old.records]
            res = check_derived_input(old)
            self.assertEqual([(r.rule, r.status) for r in res], [(rule, FAIL)], key)
        # 固定机位出现按键
        fixed = lanes["C/1"]
        fixed.records = [dict(r, keyCode=[87] if r["frame"] == 10 else []) for r in fixed.records]
        self.assertEqual(_one(check_derived_input(fixed), "input.fixed_none").status, FAIL)

    def test_person_lane_input(self):
        lanes = _load(discover(W.write_group(self.tmp / "g", n=150)))
        res = check_input_consistency(lanes["C/3"], takeover_frame=60)
        self.assertEqual(_one(res, "input.keys_drive_player").status, PASS)
        eight = _one(res, "input.eight_directions")            # W with the camera looking the way it walks: dead on
        self.assertEqual(eight.status, PASS, eight.evidence)
        self.assertLess(eight.data["median_deg"], 1.0)
        self.assertEqual(_one(check_input_consistency(lanes["C/4"], takeover_frame=60), "input.eight_directions").status, PASS)
        self.assertEqual(_one(res, "input.pre_takeover").status, PASS)    # 接管前 60 帧不带键鼠字段
        self.assertIn("之前的 60 帧不带键鼠字段", _one(res, "input.pre_takeover").evidence)
        res = check_input_consistency(lanes["C/3"], takeover_frame=100)   # 接管晚于键鼠字段开始 → 接管前填了键鼠
        self.assertEqual(_one(res, "input.pre_takeover").status, FAIL)
        res = check_input_consistency(lanes["C/3"], takeover_frame=40)    # 接管早于键鼠字段开始 → 接管后缺键鼠
        self.assertEqual(_one(res, "input.pre_takeover").status, FAIL)
        filled = lanes["C/3"]                                             # 接管前填了空键鼠（旧口径）也不行
        filled.records = [dict(r, **({} if "keyCode" in r else {"mouseX": 0.5, "mouseY": 0.5, "mouse_dx": 0.0, "mouse_dy": 0.0, "keyCode": []}))
                          for r in filled.records]
        res = check_input_consistency(filled, takeover_frame=60)
        self.assertEqual(_one(res, "input.pre_takeover").status, FAIL)
        self.assertIn("有 60 帧带键鼠字段", _one(res, "input.pre_takeover").evidence)


class EightDirectionsTest(unittest.TestCase):
    """需方 2026-09-30：只按 WASD 走不出 15° 的斜角；走出来了，JSON 里就得有鼠标转动。"""
    def test_a_skewed_walk_on_w_alone_is_flagged(self):
        from worldline_qc.checks_motion import eight_directions, _key_offset_deg
        self.assertEqual(_key_offset_deg({87}), 0.0)
        self.assertEqual(_key_offset_deg({87, 68}), 45.0)      # W + D: 45 to the right
        self.assertEqual(_key_offset_deg({65}), -90.0)
        self.assertIsNone(_key_offset_deg({81}))
        keys, still = [{87}] * 100, [90.0] * 100                              # W held, the camera looking one way
        straight = eight_directions(keys, still, [0.0] * 100, "C/3", "chase")
        self.assertEqual(straight.status, PASS)
        self.assertEqual(straight.data["frames"], 100 - 30)                 # the first second is the body turning to the keys
        skewed = eight_directions(keys, still, [15.0] * 100, "C/3", "chase")   # walked 15 degrees off with W alone, camera still
        self.assertEqual(skewed.status, WARN)
        self.assertAlmostEqual(skewed.data["median_deg"], 15.0)
        turning = eight_directions(keys, [90.0 + i * 0.5 for i in range(100)], [15.0] * 100, "C/3", "chase")   # the camera turned
        self.assertEqual(turning.status, WARN)
        self.assertIn("稳态帧只有 0 帧", turning.evidence)
        jitter = eight_directions(keys, [90.0 + 0.4 * (i % 2) for i in range(100)], [0.0] * 100, "C/3", "chase")   # a wobble is not a turn
        self.assertEqual(jitter.status, PASS)
        early = eight_directions(keys, still, [40.0] * 30 + [0.0] * 70, "C/3", "chase")   # off only while setting off
        self.assertEqual(early.status, PASS)
        self.assertEqual(eight_directions(keys[:20], still[:20], [0.0] * 20, "C/3", "chase").status, WARN)   # too short


class GeometryTest(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())
        self.g = discover(W.write_group(self.tmp / "g", n=150))
        self.lanes = _load(self.g)

    def tearDown(self):
        shutil.rmtree(self.tmp, ignore_errors=True)

    def test_stage_center(self):
        c = stage_center(self.g, self.lanes, "A")
        for a, b in zip(c, W.CENTER):
            self.assertAlmostEqual(a, b, places=6)

    def test_visibility_and_covis(self):
        res = check_geometry(self.g, self.lanes, {})
        self.assertEqual(_one(res, "geo.subject_visible", "AB/1").status, PASS)
        self.assertEqual(_one(res, "geo.subject_visible", "C/3").status, PASS)
        self.assertEqual(_one(res, "geo.subject_visible", "C/4").status, WARN)      # 第一人称：口径待确认
        self.assertEqual(_one(res, "geo.subject_visible", "AB/4").status, SKIP)     # B 不适用
        self.assertEqual(_one(res, "geo.covis", "世界线A").status, PASS)
        self.assertEqual(_one(res, "geo.covis", "世界线B").status, SKIP)
        self.assertEqual(_one(res, "geo.camera_mutual", "世界线C").status, MANUAL)

    def test_lane_fields_contract(self):
        for key in ("AB/1", "AB/2", "AB/4", "C/1", "C/2", "C/3", "C/4"):   # 每一路都带键鼠字段
            res = check_lane_fields(self.lanes[key])
            self.assertEqual(_one(res, "json.schema").status, PASS, (key, _one(res, "json.schema").evidence))
            self.assertEqual(_one(res, "json.euler_quat").status, PASS)
        # 受控人物路接管前的帧不带键鼠字段；别的路、接管后的帧缺了就不合格
        self.assertIn("开头 60 帧是接管前", _one(check_lane_fields(self.lanes["C/3"]), "json.schema").evidence)
        late = self.lanes["C/3"]
        late.records = [{k: v for k, v in r.items() if k != "keyCode" or r["frame"] != 100} for r in late.records]
        self.assertEqual(_one(check_lane_fields(late), "json.schema").status, FAIL)
        fixed = self.lanes["C/1"]
        fixed.records = [{k: v for k, v in r.items() if k not in ("keyCode", "mouseX", "mouseY", "mouse_dx", "mouse_dy") or r["frame"] > 10}
                         for r in fixed.records]
        self.assertEqual(_one(check_lane_fields(fixed), "json.schema").status, FAIL)


class WorldlineTest(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())

    def tearDown(self):
        shutil.rmtree(self.tmp, ignore_errors=True)

    def test_interaction_semantics(self):
        g = discover(W.write_group(self.tmp / "g", n=150, c_text=W.C_TEXT_NO_SEMANTICS))
        res = check_worldlines(g, _load(g))
        r = _one(res, "wl.interaction_semantics", "C/3")
        self.assertEqual(r.status, FAIL)
        self.assertIn("鼠标左键", r.evidence)
        g2 = discover(W.write_group(self.tmp / "g2", n=150, c_text=W.C_TEXT_WITH_SEMANTICS))
        self.assertEqual(_one(check_worldlines(g2, _load(g2)), "wl.interaction_semantics", "C/3").status, PASS)
        g3 = discover(W.write_group(self.tmp / "g3", n=150, attack=False))
        self.assertEqual(_one(check_worldlines(g3, _load(g3)), "wl.interaction_semantics", "C/3").status, PASS)

    def test_the_delivery_readme_passes_the_worldline_record_rules(self):
        root = W.write_group(self.tmp / "g", n=150)
        (root / "README.md").write_text(W.README_TMPL.split("## 每条世界线发生了什么")[0] + W.DELIVERY_WORLDLINES, encoding="utf-8")
        g = discover(root)
        res = check_worldlines(g, _load(g))
        for wl in "CD":
            r = _one(res, "wl.causal_record", f"世界线{wl}")
            self.assertEqual(r.status, PASS, r.evidence)
        r = _one(res, "wl.takeover_time")
        self.assertEqual(r.status, PASS, r.evidence)
        self.assertIn("首个按键 frame 61（2.0s）", r.evidence)   # 交付的 frame 从 1 起：第 61 帧 = 视频第 2.0 秒
        self.assertEqual(takeover_frames(g), {"C": 60, "D": 0})
        self.assertEqual(_one(res, "wl.interaction_semantics", "C/3").status, PASS)   # 「挥拳攻击」说明了鼠标左键
        self.assertEqual(_one(res, "wl.preroll").status, PASS)

    def test_takeover_time(self):
        g = discover(W.write_group(self.tmp / "g", n=150))
        res = check_worldlines(g, _load(g))
        r = _one(res, "wl.takeover_time")
        self.assertEqual(r.status, PASS, r.evidence)
        self.assertEqual(takeover_frames(g), {"C": 60, "D": 30})
        self.assertEqual(_one(res, "wl.takeover_in_data").status, WARN)
        for i, bad in enumerate(("视频第 2.5 秒 第 1.5 秒玩家接管 pacer", "视频第 61.0 秒 第 60 秒玩家接管 pacer")):
            # a numbered folder: a slice of the text ended in a space ("b.5 "), a name Windows will not make
            gb = discover(W.write_group(self.tmp / f"bad{i}", n=150, c_text=bad))
            self.assertEqual(_one(check_worldlines(gb, _load(gb)), "wl.takeover_time").status, FAIL)
        # 首键（帧 60）早于声明的接管（视频 3.0 s = 帧 90）→ FAIL
        gl = discover(W.write_group(self.tmp / "late", n=150, c_text="视频第 3.0 秒 第 2 秒玩家接管 pacer"))
        r = _one(check_worldlines(gl, _load(gl)), "wl.takeover_time")
        self.assertEqual(r.status, FAIL)
        self.assertIn("早于接管", r.evidence)

    def test_time_real_clock(self):
        g = discover(W.write_group(self.tmp / "g", n=150))
        self.assertEqual(_one(check_worldlines(g, _load(g)), "wl.time_real_clock").status, FAIL)
        g2 = discover(W.write_group(self.tmp / "g2", n=150, distinct_clock=True))
        self.assertEqual(_one(check_worldlines(g2, _load(g2)), "wl.time_real_clock").status, PASS)

    def test_misc_worldline_rules(self):
        g = discover(W.write_group(self.tmp / "g", n=150))
        res = check_worldlines(g, _load(g))
        self.assertEqual(_one(res, "wl.b_pure_camera", "AB/4").status, PASS)
        self.assertEqual(_one(res, "wl.a_baseline", "AB/1").status, PASS)
        self.assertEqual(_one(res, "wl.a_baseline", "AB/2").status, PASS)   # 换算的键鼠不算真实输入
        lanes = _load(g)
        lanes["AB/1"].records = [dict(r, mouse_dx=0.2) for r in lanes["AB/1"].records]
        self.assertEqual(_one(check_worldlines(g, lanes), "wl.a_baseline", "AB/1").status, FAIL)   # 固定机位不许有鼠标量
        lanes["AB/3"].records = [dict(r, player_position=[0.0, 0.0, 0.0]) for r in lanes["AB/3"].records]
        self.assertEqual(_one(check_worldlines(g, lanes), "wl.a_baseline", "AB/3").status, FAIL)   # A 不许有受控人物
        self.assertEqual(_one(res, "wl.event_changed_flag", "世界线C").status, WARN)
        self.assertEqual(_one(res, "wl.d_new_person").status, MANUAL)
        self.assertEqual(_one(res, "wl.preroll").status, WARN)
        self.assertEqual(_one(res, "wl.operator").status, MANUAL)
        g2 = discover(W.write_group(self.tmp / "g2", n=150, with_readme=False))
        self.assertEqual(_one(check_worldlines(g2, _load(g2)), "wl.causal_record").status, FAIL)


class CatalogTest(unittest.TestCase):
    def test_every_emitted_rule_is_catalogued(self):
        tmp = Path(tempfile.mkdtemp())
        try:
            rep, _ = check_group(W.write_group(tmp / "g", n=150), expect_s=5.0, pixels=False)
        finally:
            shutil.rmtree(tmp, ignore_errors=True)
        self.assertEqual(uncatalogued_rules(rep.results), [])
        ids = [c.id for c in CATALOG]
        self.assertEqual(len(ids), len(set(ids)))

    def test_no_video_is_not_pass(self):
        tmp = Path(tempfile.mkdtemp())
        try:
            rep, _ = check_group(W.write_group(tmp / "g", n=150), expect_s=5.0, pixels=False)
        finally:
            shutil.rmtree(tmp, ignore_errors=True)
        self.assertEqual(rep.verdict, FAIL)          # 缺视频 → pack.files FAIL
        self.assertEqual(_one(rep.results, "pack.files").status, FAIL)
        self.assertEqual(_one(rep.results, "pack.layout").status, FAIL)   # 遍布局
        self.assertEqual(_one(rep.results, "pix.motion_ratio", "AB/1").status, "UNVERIFIED")


class MulticamSyncTest(unittest.TestCase):
    """交付说明 v1.1（2026-10-16）的 multicam_sync_i.json，按需方「json 书写关注重点」逐条验。"""

    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())
        self.root = W.write_group(self.tmp / "g", n=90)

    def tearDown(self):
        shutil.rmtree(self.tmp, ignore_errors=True)

    def _check(self, **kw):
        if kw:
            W.write_multicam(self.root, n=90, **kw)
        return check_multicam_sync(discover(self.root))

    def _status(self, results, rule, target=None):
        return {r.status for r in results if r.rule == rule and (target is None or r.target == target)}

    def test_a_whole_group_passes_and_lighting_left_out_is_said(self):
        res = self._check()
        for rule in ("msync.fields", "msync.entities", "msync.group_header", "msync.time", "msync.frames", "msync.in_take",
                     "msync.takeover", "msync.controlled", "msync.cross_takeover", "msync.cross_no_event"):
            with self.subTest(rule=rule):
                self.assertEqual(self._status(res, rule), {PASS}, [r.evidence for r in res if r.rule == rule])
        self.assertEqual(len([r for r in res if r.rule == "msync.fields"]), 12)
        self.assertIn("第 2 秒", _one(res, "msync.takeover", "C").evidence)
        self.assertIn("seventh", _one(res, "msync.controlled", "D").evidence)
        self.assertEqual(self._status(res, "msync.lighting"), {WARN})
        self.assertEqual(uncatalogued_rules(res), [])

    def test_the_sun_filled_in_is_checked_frame_by_frame(self):
        sun = {"sun_direction": [-0.1638, -0.8324, 0.5295], "intensity": 1.0}

        def lit(take, lane, doc):
            for f in doc["frames"]:
                f["lighting"] = dict(sun)
        res = self._check(edit=lit)
        self.assertEqual(self._status(res, "msync.lighting"), {PASS})
        self.assertIn("太阳高度 56.3°", _one(res, "msync.lighting").evidence)

        def holes(take, lane, doc):   # one lane leaves frames out
            lit(take, lane, doc)
            if take == "D" and lane == 2:
                doc["frames"][5]["lighting"] = None
        self.assertEqual(self._status(self._check(edit=holes), "msync.lighting"), {FAIL})

        def upward(take, lane, doc):   # the light going up: the direction written the wrong way round
            for f in doc["frames"]:
                f["lighting"] = {"sun_direction": [0.1638, 0.8324, -0.5295], "intensity": 1.0}
        self.assertEqual(self._status(self._check(edit=upward), "msync.lighting"), {FAIL})

    def test_two_takes_under_a_millimetre_apart_are_a_question_a_few_millimetres_a_failure(self):
        # the customer's check (2026-10-19, beach-demo-113) reported 1.2 mm on one axis and 0.11 degrees, not 0.98 mm
        self.assertEqual(self._status(self._check(jitter_m=0.0005), "msync.cross_takeover"), {WARN})
        res = self._check(jitter_m=0.003)
        self.assertEqual(self._status(res, "msync.cross_takeover"), {FAIL})
        self.assertIn("3.0 mm", _one(res, "msync.cross_takeover").evidence)
        self.assertEqual(self._status(res, "msync.cross_no_event"), {PASS})   # A and D still the same

    def test_two_takes_at_different_times_for_the_same_frame_fail(self):
        # sample_time from each take's own game clock differed 1-3 ms between takes; the customer failed it
        def clock(take, lane, doc):
            if take == "C":
                for f in doc["frames"]:
                    f["sample_time"] = round(f["sample_time"] + 0.001, 6)
        res = self._check(edit=clock)
        self.assertEqual(self._status(res, "msync.cross_takeover"), {FAIL})
        self.assertIn("sample_time", _one(res, "msync.cross_takeover").evidence)

    def test_the_takeover_on_a_whole_second_the_same_in_all_four_c_lanes(self):
        self.assertEqual(self._status(self._check(takeover=50), "msync.takeover", "C"), {FAIL})   # 1.67 s

        def other(take, lane, doc):
            if take == "C" and lane == 4:
                doc["takeover"][0]["video_frame"] = doc["takeover"][0]["logical_frame"] = 30
        self.assertEqual(self._status(self._check(edit=other), "msync.takeover", "C"), {FAIL})

        def stray(take, lane, doc):
            if take == "D":
                doc["takeover"] = [{"logical_frame": 0, "phase": "before_update", "video_frame": 0, "entity_ids": ["seventh"]}]
        self.assertEqual(self._status(self._check(edit=stray), "msync.takeover", "group"), {FAIL})

    def test_d_s_person_lives_in_d_only_and_c_s_in_all_twelve(self):
        def leak(take, lane, doc):   # the seventh person walks into C's world
            if take == "C":
                for f in doc["frames"]:
                    f["entities"][3].update(exists=True, position=[3.0, 0.0, 2.0], rotation=[0.0, 0.0, 0.0, 1.0])
        res = self._check(edit=leak)
        self.assertEqual(self._status(res, "msync.controlled", "D"), {FAIL})

        def gone(take, lane, doc):   # pacer missing from A's world for a frame
            if take == "AB":
                doc["frames"][10]["entities"][1].update(exists=False, position=None, rotation=None, animation=None)
        self.assertEqual(self._status(self._check(edit=gone), "msync.controlled", "C"), {FAIL})

    def test_every_id_every_frame_and_nothing_made_up_where_it_does_not_exist(self):
        def drop(take, lane, doc):
            if (take, lane) == ("AB", 2):
                del doc["frames"][5]["entities"][0]
        res = self._check(edit=drop)
        self.assertEqual({r.target for r in res if r.rule == "msync.entities" and r.status == FAIL}, {"#2"})
        self.assertEqual(self._status(res, "msync.in_take", "A/B"), set())   # cross-lane checks need all 12 good

        def filled(take, lane, doc):
            if (take, lane) == ("D", 1):
                doc["frames"][0]["entities"][3]["animation"] = {"id": "x"}
            if (take, lane) == ("AB", 1):
                for f in doc["frames"]:
                    f["entities"][3]["position"] = [0.0, 0.0, 0.0]   # the seventh person "at the origin" in A
        res = self._check(edit=filled)
        self.assertIn("不存在却填了状态", _one(res, "msync.entities", "#1").evidence)

    def test_frames_one_to_one_with_the_lane_and_the_same_world_in_a_take(self):
        def short(take, lane, doc):
            if (take, lane) == ("D", 2):
                doc["frames"].pop()
        self.assertIn("不是一一对应", _one(self._check(edit=short), "msync.fields", "#10").evidence)

        def moved(take, lane, doc):
            if (take, lane) == ("D", 3):
                doc["frames"][40]["entities"][0]["position"][0] += 0.1
        self.assertEqual(self._status(self._check(edit=moved), "msync.in_take", "D"), {FAIL})

    def test_a_package_from_before_v1_1_is_unverified_not_passed(self):
        for p in self.root.rglob("multicam_sync_*.json"):
            p.unlink()
        self.assertEqual(self._status(check_multicam_sync(discover(self.root)), "msync.fields"), {UNVERIFIED})


if __name__ == "__main__":
    unittest.main()

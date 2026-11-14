"""placement / map-world / plan.map_bounds 的单测（docs/88 §6 #1，辩题二十四）。

真机锚点：sfm-d1-20260805 解得 yaw=-157.323364° / t=(60, 31.6727, 220)，
1800 帧最坏残差 8e-6 m——解算器必须能精确还原这种纯刚体摆位，并对
非刚体（重规划/裁剪）与不可辨识（原地轨迹）两种形态明确拒绝。
"""

import json
import math
import shutil
import tempfile
import unittest
from pathlib import Path

from engine_qc import checks_json as cj
from engine_qc.cli import main as cli_main
from engine_qc.config import QCConfig
from engine_qc.placement import (PlacementError, derive_run_world,
                                 solve_yaw_translation, world_block)
from engine_qc.report import FAIL, PASS, SKIP


def _apply(yaw_deg, t, p):
    y = math.radians(yaw_deg)
    c, s = math.cos(y), math.sin(y)
    return [c * p[0] - s * p[2] + t[0], p[1] + t[1], s * p[0] + c * p[2] + t[2]]


def _frames(positions):
    return [{"frame": i, "camera_position": [float(x) for x in p]}
            for i, p in enumerate(positions)]


def _spiral(n=120):
    """带水平散布与高度变化的轨迹——yaw 可辨识。"""
    return [[10.0 * math.cos(i * 0.1), 2.0 + 0.01 * i, 6.0 * math.sin(i * 0.1) + 0.05 * i]
            for i in range(n)]


MAP_META = {
    "scene": "t",
    "bounds_lo": [-90.0, 0.0, -25.0],
    "bounds_hi": [90.0, 28.0, 25.0],
    "ground_corners_xz": [[-90, -25], [90, -25], [90, 25], [-90, 25]],
    "y_range": [0, 28],
}


class SolverTest(unittest.TestCase):
    def test_exact_recovery(self):
        yaw, t = -157.323364, (60.0, 31.6727, 220.0)
        local = _spiral()
        world = [_apply(yaw, t, p) for p in local]
        got_yaw, got_t, worst = solve_yaw_translation(_frames(local), _frames(world))
        self.assertAlmostEqual(got_yaw, yaw, places=6)
        for a, b in zip(got_t, t):
            self.assertAlmostEqual(a, b, places=6)
        self.assertLess(worst, 1e-9)

    def test_identity_placement(self):
        local = _spiral()
        yaw, t, worst = solve_yaw_translation(_frames(local), _frames(local))
        self.assertAlmostEqual(yaw, 0.0, places=9)
        self.assertLess(max(abs(x) for x in t), 1e-9)
        self.assertLess(worst, 1e-9)

    def test_stationary_is_unidentifiable(self):
        local = [[1.0, 2.0, 3.0]] * 50
        with self.assertRaises(PlacementError):
            solve_yaw_translation(_frames(local), _frames(local))

    def test_length_mismatch_rejected(self):
        local = _spiral()
        with self.assertRaises(PlacementError):
            solve_yaw_translation(_frames(local), _frames(local)[:-1])


class WorldBlockTest(unittest.TestCase):
    def test_corners_and_aabb(self):
        yaw, t = 90.0, (100.0, 5.0, -40.0)
        blk = world_block(MAP_META, yaw, t, 1e-6, 120)
        # 局部 8 角点旋转后必须都在 world AABB 内
        for x in (MAP_META["bounds_lo"][0], MAP_META["bounds_hi"][0]):
            for y in (MAP_META["bounds_lo"][1], MAP_META["bounds_hi"][1]):
                for z in (MAP_META["bounds_lo"][2], MAP_META["bounds_hi"][2]):
                    w = _apply(yaw, t, [x, y, z])
                    for i in range(3):
                        self.assertLessEqual(blk["bounds_lo"][i] - 1e-6, w[i])
                        self.assertLessEqual(w[i], blk["bounds_hi"][i] + 1e-6)
        # yaw=90 时 x 尺寸来自局部 z 尺寸（50），z 尺寸来自局部 x 尺寸（180）
        self.assertAlmostEqual(blk["bounds_hi"][0] - blk["bounds_lo"][0], 50.0, places=3)
        self.assertAlmostEqual(blk["bounds_hi"][2] - blk["bounds_lo"][2], 180.0, places=3)
        self.assertEqual(blk["y_range"], [5.0, 33.0])
        self.assertEqual(len(blk["ground_corners_xz"]), 4)

    def test_missing_bounds_rejected(self):
        with self.assertRaises(PlacementError):
            world_block({"scene": "t"}, 0.0, (0, 0, 0), 0.0, 1)


class DeriveRunTest(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="qc_place_"))
        (self.tmp / "plan").mkdir()
        (self.tmp / "clip").mkdir()

    def tearDown(self):
        shutil.rmtree(self.tmp, ignore_errors=True)

    def _write(self, local, world, meta=MAP_META):
        (self.tmp / "plan" / "action_camera_30fps.json").write_text(
            json.dumps(_frames(local)), encoding="utf-8")
        (self.tmp / "clip" / "action_camera_30fps.json").write_text(
            json.dumps(_frames(world)), encoding="utf-8")
        (self.tmp / "plan" / "map_meta.json").write_text(
            json.dumps(meta), encoding="utf-8")

    def test_derive_and_cli_roundtrip(self):
        yaw, t = -30.0, (60.0, 31.67, 220.0)
        local = _spiral()
        self._write(local, [_apply(yaw, t, p) for p in local])
        meta, _ = derive_run_world(self.tmp)
        self.assertAlmostEqual(meta["world"]["yaw_xz_deg"], yaw, places=4)
        # CLI 写盘 + 幂等重跑
        for _ in range(2):
            self.assertEqual(cli_main(["map-world", str(self.tmp)]), 0)
        on_disk = json.loads((self.tmp / "plan" / "map_meta.json").read_text("utf-8"))
        self.assertIn("world", on_disk)
        self.assertEqual(on_disk["bounds_lo"], MAP_META["bounds_lo"])  # 局部字段保留

    def test_non_rigid_refused(self):
        local = _spiral()
        world = [_apply(-30.0, (60, 31.67, 220), p) for p in local]
        world[60][0] += 0.5    # 单帧被重规划：非刚体
        self._write(local, world)
        with self.assertRaises(PlacementError):
            derive_run_world(self.tmp)
        self.assertEqual(cli_main(["map-world", str(self.tmp)]), 1)
        on_disk = json.loads((self.tmp / "plan" / "map_meta.json").read_text("utf-8"))
        self.assertNotIn("world", on_disk)   # 拒绝时不写盘

    def test_dry_run_writes_nothing(self):
        local = _spiral()
        self._write(local, [_apply(10.0, (1, 2, 3), p) for p in local])
        self.assertEqual(cli_main(["map-world", str(self.tmp), "--dry-run"]), 0)
        on_disk = json.loads((self.tmp / "plan" / "map_meta.json").read_text("utf-8"))
        self.assertNotIn("world", on_disk)


class MapBoundsRuleTest(unittest.TestCase):
    """plan.map_bounds：sfm-d1 形态（边界局部系、pose 世界系）必须 FAIL 并指路。"""

    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="qc_mb_"))
        self.clip = self.tmp / "clip"
        (self.tmp / "plan").mkdir()
        self.clip.mkdir()
        self.cfg = QCConfig()

    def tearDown(self):
        shutil.rmtree(self.tmp, ignore_errors=True)

    def _meta(self, obj):
        (self.tmp / "plan" / "map_meta.json").write_text(json.dumps(obj), encoding="utf-8")

    def test_no_map_meta_skips(self):
        r = cj.check_map_bounds(self.clip, _frames(_spiral()), self.cfg)
        self.assertEqual(r.status, SKIP)

    def test_local_frame_mismatch_fails_with_hint(self):
        self._meta(MAP_META)
        world = [_apply(-157.3, (60, 31.67, 220), p) for p in _spiral()]
        r = cj.check_map_bounds(self.clip, _frames(world), self.cfg)
        self.assertEqual(r.status, FAIL)
        self.assertIn("map-world", r.evidence)

    def test_world_block_passes(self):
        yaw, t = -157.3, (60.0, 31.67, 220.0)
        meta = dict(MAP_META)
        meta["world"] = world_block(MAP_META, yaw, t, 1e-6, 120)
        self._meta(meta)
        world = [_apply(yaw, t, p) for p in _spiral()]
        r = cj.check_map_bounds(self.clip, _frames(world), self.cfg)
        self.assertEqual(r.status, PASS)

    def test_identity_placement_local_passes(self):
        self._meta(MAP_META)
        r = cj.check_map_bounds(self.clip, _frames(_spiral()), self.cfg)
        self.assertEqual(r.status, PASS)

    def test_map_meta_in_clip_root_wins(self):
        (self.clip / "map_meta.json").write_text(json.dumps(MAP_META), encoding="utf-8")
        r = cj.check_map_bounds(self.clip, _frames(_spiral()), self.cfg)
        self.assertEqual(r.status, PASS)


if __name__ == "__main__":
    unittest.main()

"""原语库与配额：10 族 23 类的完整性、循环路径闭合、径向/方位预算。"""

import math
import random

from trajgen.anchors import Anchor, Placement, Subject
from trajgen.mathutil import vdist
from trajgen.primitives import (ALL_CLASSES, EXTENT_DRIVEN, FAMILIES, FAMILY_OF,
                                MOVING, PINK, QUOTA, STATIONARY, Ctx, Move, Pose,
                                azimuth_budget, build, radial_budget)
from trajgen.scene import synthetic


def _ctx(**kw):
    sc = synthetic("open_plaza", 1)
    anchor = Anchor(Subject([(0.0, 1.6, 0.0)] * 3, 20.0), 2.0)
    return Ctx(sc, anchor, [Subject([(2.0, 1.6, 1.0)] * 3, 20.0)],
               Placement(0.0, 5.0, 1), random.Random(0), 60.0, **kw)


def test_23_classes_10_families():
    assert len(FAMILIES) == 10
    assert len(ALL_CLASSES) == 23
    assert len(set(ALL_CLASSES)) == 23
    assert len(PINK) == 6
    assert set(QUOTA) | set(PINK) == set(ALL_CLASSES)


def test_class_partitions_are_consistent():
    assert STATIONARY < set(ALL_CLASSES)
    assert EXTENT_DRIVEN < set(ALL_CLASSES)
    assert MOVING == set(ALL_CLASSES) - STATIONARY
    assert not (EXTENT_DRIVEN & STATIONARY)          # 可控位移的必然会动
    assert set(PINK) <= EXTENT_DRIVEN                # 6 平移都按 extent 走


def test_every_class_builds_and_is_finite():
    ctx = _ctx()
    start = Pose((6.0, 1.6, 0.0), 180.0, 0.0, 0.0)
    for cls in ALL_CLASSES:
        m = Move(cls, 0.0, 8.0, 0, {"extent": 6.0})
        run = build(m, ctx, start)
        for u in (0.0, 0.25, 0.5, 0.75, 1.0):
            p = run(u, 8.0 * u)
            assert all(math.isfinite(x) for x in p.p), cls
            assert math.isfinite(p.yaw) and math.isfinite(p.elev), cls


def test_move_starts_at_start_pose_position():
    """段界位置连续由 sampler 兜底，但原语自身也不该在 u=0 就跳开太远。"""
    ctx = _ctx()
    start = Pose((6.0, 1.6, 0.0), 180.0, 0.0, 0.0)
    for cls in ALL_CLASSES:
        run = build(Move(cls, 0.0, 8.0, 0, {"extent": 6.0}), ctx, start)
        assert vdist(run(0.0, 0.0).p, start.p) < 2.5, cls


def test_loop_path_closes():
    """循环路径终点严格回到起点位置与朝向（docs/71 §4）。"""
    ctx = _ctx()
    start = Pose((6.0, 1.6, 0.0), 180.0, 0.0, 0.0)
    run = build(Move("loop_path", 0.0, 12.0, 0, {"extent": 20.0}), ctx, start)
    a, b = run(0.0, 0.0), run(1.0, 12.0)
    assert vdist(a.p, b.p) < 1e-6
    assert abs((b.yaw - a.yaw + 180) % 360 - 180) < 1e-6


def test_radial_budget_keeps_working_annulus():
    ctx = _ctx(r_min=3.0, r_max=10.0)
    # 从 r=9 处沿离心方向走：预算只允许再走 1m
    assert radial_budget(ctx, (9.0, 1.6, 0.0), (1.0, 0.0, 0.0), 50.0, 0.0) <= 1.01
    # 向心方向：允许走到 r_min
    assert radial_budget(ctx, (9.0, 1.6, 0.0), (-1.0, 0.0, 0.0), 50.0, 0.0) <= 6.01


def test_azimuth_budget_respects_sector():
    ctx = _ctx(sector_center_deg=0.0, sector_half_deg=30.0)
    # 在 0° 处沿切向走：30° 的余量在 r=6 上约 3.14m
    got = azimuth_budget(ctx, (6.0, 1.6, 0.0), (0.0, 0.0, 1.0), 50.0, 0.0)
    assert 2.5 < got < 3.6
    # 自由路（180°）不受限
    free = _ctx(sector_half_deg=180.0)
    assert azimuth_budget(free, (6.0, 1.6, 0.0), (0.0, 0.0, 1.0), 50.0, 0.0) == 50.0

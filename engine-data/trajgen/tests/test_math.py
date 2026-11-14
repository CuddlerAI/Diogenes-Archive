"""数学骨架：B 样条端点行为、四元数顺规、S₅ 过渡。

这几条锁住的都是**踩过的坑**，不是形式化练习：端点没真正 clamped 会让首帧位姿对不上、
反查函数用 clamped 端点会让第四路开头原地不动。
"""

import math

from trajgen.mathutil import (bspline5_basis, bspline5_curve, bspline5_eval,
                              clamp_ctrl, linear_ctrl, linear_eval, quat_forward,
                              quat_from_yaw_elev_roll, smoothstep5, unwrap_deg,
                              yaw_elev_from_forward)


def test_basis_partition_of_unity():
    for u in (0.0, 0.13, 0.5, 0.87, 0.999):
        assert abs(sum(bspline5_basis(u)) - 1.0) < 1e-12


def test_clamp_ctrl_interpolates_endpoints():
    """五次样条要重复 5 个端点才严格插值端点值（重复 2 个会差 ~22%）。"""
    vals = [0.0, 10.0, 3.0, 7.0, 1.0]
    curve = bspline5_curve(vals, 200)
    assert abs(curve[0] - vals[0]) < 1e-9
    assert abs(curve[-1] - vals[-1]) < 1e-9


def test_linear_ctrl_preserves_slope():
    """线性外推端点：均匀 B 样条对线性函数精确重现，端点斜率不被压成 0。"""
    vals = [2.0 * i for i in range(20)]
    ctrl = linear_ctrl(vals)
    got = [linear_eval(ctrl, len(vals), i / 100.0) for i in range(101)]
    expect = [vals[0] + (vals[-1] - vals[0]) * i / 100.0 for i in range(101)]
    assert max(abs(a - b) for a, b in zip(got, expect)) < 1e-6
    # clamped 端点则会把首段斜率压掉，正是第四路开头原地不动的成因
    clamped = clamp_ctrl(vals)
    clamped_slope = bspline5_eval(clamped, 0.05) - bspline5_eval(clamped, 0.0)
    linear_slope = linear_eval(ctrl, len(vals), 0.05 / (len(vals) - 1)) - got[0]
    assert clamped_slope < 0.2 * linear_slope


def test_quat_roundtrip_yaw_elev():
    for yaw in (-170.0, -30.0, 0.0, 45.0, 179.0):
        for elev in (-60.0, -10.0, 0.0, 25.0, 60.0):
            q = quat_from_yaw_elev_roll(yaw, elev, 0.0)
            y2, e2 = yaw_elev_from_forward(quat_forward(q))
            assert abs((y2 - yaw + 180) % 360 - 180) < 1e-6
            assert abs(e2 - elev) < 1e-6


def test_quat_roll_does_not_move_forward():
    """roll 绕 forward 轴转，forward 不该动（歪头 Roll 的语义）。"""
    a = quat_forward(quat_from_yaw_elev_roll(33.0, -12.0, 0.0))
    b = quat_forward(quat_from_yaw_elev_roll(33.0, -12.0, 14.0))
    assert max(abs(x - y) for x, y in zip(a, b)) < 1e-9


def test_smoothstep5_endpoints_flat():
    """S₅ 两端一二阶导为零 —— 第四路 ρ 过渡与段界缓入缓出都靠这个性质。"""
    assert smoothstep5(0.0) == 0.0
    assert smoothstep5(1.0) == 1.0
    h = 1e-4
    d0 = (smoothstep5(h) - smoothstep5(0.0)) / h
    d1 = (smoothstep5(1.0) - smoothstep5(1.0 - h)) / h
    assert d0 < 1e-6 and d1 < 1e-6


def test_unwrap_deg():
    assert unwrap_deg([170.0, -170.0, 170.0]) == [170.0, 190.0, 170.0]

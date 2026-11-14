"""pink 叠加窗的合成几何回归测试（不依赖场景文件）。

覆盖三件事：
1. 载机重积分的**前缀一致性**（与 integrate_orbit 公式漂移即失败）；
2. 逐窗诚实性契约（净轴向 ≥0.8m 且 ≥50% 路径；基=窗起帧相机 yaw）；
3. cam4 保速换向的速度保持性（|v₄| 逐帧不变 ⇒ ρ 反算无感）。
"""
import math
import random

import pytest

from trajgen import pink
from trajgen import metrics as met
from trajgen.director import integrate_orbit
from trajgen.mathutil import quat_from_yaw_elev_roll, vdist

FPS = 30
DUR = 60.0
N = int(DUR * FPS)


def _mk_args(rng):
    center = (0.0, 0.0, 0.0)
    w_mean = rng.uniform(1.9, 2.9)
    omega = [w_mean * (1 + 0.62 * math.sin(2 * math.pi * i / FPS / 10.4))
             for i in range(N)]
    return {"center": center, "duration": DUR, "n_frames": N,
            "r0": rng.uniform(52, 78), "h0": rng.uniform(-3, 5),
            "az0": rng.uniform(0, 360), "omega_series": omega,
            "r_wobble": rng.uniform(2.5, 5.5), "r_period": 37.0,
            "h_period": 31.0}


def _center_yaw(center):
    def f(t, veh_pos, veh_yaw):
        d = (center[0] - veh_pos[0], 0.0, center[2] - veh_pos[2])
        return math.degrees(math.atan2(d[0], d[2]))
    return f


@pytest.mark.parametrize("seed", [1, 7, 19, 23])
def test_carrier_prefix_and_honesty(seed):
    rng = random.Random(seed)
    args = _mk_args(rng)
    wins = pink.schedule_carrier_windows(rng, DUR)
    yaw_fn = _center_yaw(args["center"])
    pos, yaws, _s, speeds, _log = pink.reintegrate_carrier(args, wins, yaw_fn, FPS)

    # 1) 前缀一致性：首窗刹车前与 integrate_orbit 逐位一致
    base = integrate_orbit(args["center"], DUR, N, args["r0"], args["h0"],
                           args["az0"], args["omega_series"],
                           r_wobble=args["r_wobble"], r_period=args["r_period"],
                           h_period=args["h_period"])
    t_first = min(w.t0 for w in wins)
    guard_n = max(1, int((t_first - pink.CAR_T_BRAKE) * FPS) - 2)
    drift = max(vdist(base.positions[k], pos[k]) for k in range(guard_n))
    assert drift < 1e-6, "重积分基线公式漂移 %.2e m" % drift

    # 2) 契约逐窗成立（用与 QC 同式的 selfcheck）
    quats = [quat_from_yaw_elev_roll(yaw_fn(i / FPS, pos[i], yaws[i]), 0, 0)
             for i in range(N)]
    rows = pink.selfcheck(pos, quats, wins, FPS)
    assert len(rows) == 6
    assert {r["cls"] for r in rows} == set(pink.PINK_CLASSES)
    for r in rows:
        assert r["ok"], r

    # 3) 物理与静止不越线（p95 门的 90% 内控）
    ph = met.physics(pos, quats, 1.0 / FPS)
    assert ph.jerk_p95 < met.JERK_P95_MAX * 0.9
    assert ph.speed_max < met.SPEED_INST_CAP
    static = sum(1 for v in speeds if v < 0.2) / N
    assert static < 0.05


@pytest.mark.parametrize("seed", [1, 2, 3, 11])
def test_cam4_speed_preserved_and_honest(seed):
    rng = random.Random(seed)
    center = (0.0, 0.0, 0.0)
    r4, h4 = 140.0, 30.0
    v4p = []
    for i in range(N):
        t = i / FPS
        base = 1.2 * (1.0 + 0.62 * math.sin(2 * math.pi * t / 10.4))
        v4p.append(base * (0.5 if t < 30 else 1.1))
    wins = pink.schedule_cam4_windows(rng, DUR, v4p, FPS)
    assert {w.cls for w in wins} == set(pink.PINK_CLASSES)
    drv = pink.Cam4Driver(wins, FPS)
    az, r_cur, h_cur = 10.0, r4, h4
    p = (r_cur * math.cos(math.radians(az)), h_cur,
         r_cur * math.sin(math.radians(az)))
    pos, quats = [], []
    dt = 1.0 / FPS
    for i in range(N):
        t = i / FPS
        pos.append(p)
        d = (-p[0], -p[1] + h_cur * 0.0, -p[2])
        yaw = math.degrees(math.atan2(-p[0], -p[2]))
        elev = -math.degrees(math.atan2(p[1], math.hypot(p[0], p[2])))
        quats.append(quat_from_yaw_elev_roll(yaw, elev, 0))
        a = math.radians(az)
        tang = (-math.sin(a), 0.0, math.cos(a))
        d4 = drv.direction(t, tang, v4p[i], yaw)
        if d4 is None:
            az += math.degrees(v4p[i] / r_cur) * dt
            p = (r_cur * math.cos(math.radians(az)), h_cur,
                 r_cur * math.sin(math.radians(az)))
        else:
            p = (p[0] + d4[0] * v4p[i] * dt, p[1] + d4[1] * v4p[i] * dt,
                 p[2] + d4[2] * v4p[i] * dt)
            if drv.absorb_now:
                r_cur = math.hypot(p[0], p[2])
                h_cur = p[1]
                az += ((math.degrees(math.atan2(p[2], p[0])) - az + 180)
                       % 360) - 180
    # 保速换向：|Δpos|·fps 与 v4p 逐帧一致（ρ 反算的根基）
    err = max(abs(vdist(pos[i], pos[i - 1]) * FPS - v4p[i - 1])
              for i in range(1, N))
    assert err < 1e-6, "cam4 速度模被窗改动 %.2e" % err
    rows = pink.selfcheck(pos, quats, wins, FPS)
    for r in rows:
        assert r["ok"], r

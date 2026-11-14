"""段边界连续性（docs/69 §3.1「段间连续衔接并缓入缓出」）回归。

实证背景：clip-013（vehicle-director，seed=13，megacity-corridor）cam2 在
frame 398→399 / 466→467 出现两处单帧 5m 位置跳变（帧间速度 150 m/s）。
根因：`integrate_orbit` 的机头符号跟随**瞬时** omega——渐开斜坡与速度振荡
叠加使 omega 短暂过零，机头 180° 瞬时调头，2.5m 前挂相机单帧平移 2×2.5=5m。
director 门与 engine_qc 物理门均为 p95 口径，单帧尖峰不动 p95 ⇒ 双侧漏检。

反算口径逐字遵守 engine-data/README.md「运动学反算规范口径」：
dt 名义 1/30（×FPS），平移向量差分取模，角走 q_rel 的 atan2 形式，
序列自然减 1、不 padding 不平滑。
"""

import math

from trajgen import metrics as met
from trajgen.director import MOUNT_OFFSET, integrate_orbit, mount_world
from trajgen.mathutil import quat_from_yaw_elev_roll, vnorm, vsub, wrap180
from trajgen.planner import FPS


def _crossing_omega(n_frames):
    """复现缺陷形状的 omega 序列：正名义方向 + 负渐开斜坡压穿零轴。

    均值 ~2°/s（ORBIT_OMEGA_BAND 内），叠加 ±62% 振荡与 -2.4°/s 峰值的
    负斜坡（对应 d_az≈-28° 的渐开），中段必有一段 omega<0 的过零窗。
    """
    from trajgen.director import SPREAD_RAMP_S, _ramp_rate
    omega = []
    for i in range(n_frames):
        t = i / FPS
        w = 2.0 * (1.0 + 0.62 * math.sin(2.0 * math.pi * t / 7.6))
        omega.append(w + (-28.0) * _ramp_rate(t))
    assert min(omega) < -0.1 < 2.0 < max(omega), "构造必须真的过零"
    return omega


def test_orbit_heading_survives_omega_zero_crossing():
    """omega 瞬时过零时机头不得 180° 调头（clip-013 根因回归）。"""
    n = 900
    sched = integrate_orbit((0.0, 30.0, 0.0), n / FPS, n,
                            r0=65.0, h0=1.0, az0=0.0,
                            omega_series=_crossing_omega(n),
                            r_wobble=4.0, r_period=37.0, h_period=35.0)
    max_dyaw = max(abs(wrap180(b - a))
                   for a, b in zip(sched.yaws, sched.yaws[1:]))
    # 合法机头角速率 ~|omega| ≤ ~5°/s ⇒ 单帧 ≤~0.2°；翻转缺陷是单帧 180°
    assert max_dyaw < 5.0, "机头单帧转角 %.1f°——瞬时调头回归" % max_dyaw


def test_mounted_camera_has_no_translation_spike_across_crossing():
    """挂载相机帧间速度（README 反算口径）全程 < SPEED_SPIKE_MAX。"""
    n = 900
    sched = integrate_orbit((0.0, 30.0, 0.0), n / FPS, n,
                            r0=65.0, h0=1.0, az0=0.0,
                            omega_series=_crossing_omega(n),
                            r_wobble=4.0, r_period=37.0, h_period=35.0)
    cam = [mount_world(sched.positions[i], sched.yaws[i]) for i in range(n)]
    v = [vnorm(vsub(b, a)) * FPS for a, b in zip(cam, cam[1:])]
    assert max(v) < met.SPEED_INST_CAP, \
        "挂载点帧间速度 %.1f m/s ≥ %.0f——5m 传送门回归" % (max(v), met.SPEED_INST_CAP)
    # 缺陷形态精确断言：任何 ≈2×|挂载水平臂| 的单帧位移都是机头翻转
    arm2 = 2.0 * math.hypot(MOUNT_OFFSET[0], MOUNT_OFFSET[2])
    assert all(abs(x / FPS - arm2) > 1.0 for x in v)


def test_speed_spike_gate_catches_single_frame_teleport():
    """自检闸门：单帧 5m 传送门必须被 violations() 抓住（p95 抓不住）。"""
    n = 600
    q = quat_from_yaw_elev_roll(0.0, 0.0, 0.0)
    pos = [(0.1 * i, 30.0, 0.0) for i in range(n)]         # 3 m/s 匀速
    quats = [q] * n
    clean = met.physics(pos, quats, 1.0 / FPS)
    assert not clean.speed_spike_frames
    assert not clean.violations()
    jumped = [(x + (5.0 if i >= 399 else 0.0), y, z)
              for i, (x, y, z) in enumerate(pos)]           # frame 398→399 跳 5m
    ph = met.physics(jumped, quats, 1.0 / FPS)
    assert ph.speed_spike_frames == [398]
    assert ph.speed_p95 < met.SPEED_P95_MAX, "尖峰不动 p95——这正是要补闸的缝"
    v = ph.violations()
    assert any(x.startswith("speed_max=") for x in v), v    # 全路瞬时硬帽（同 PR #474 口径）
    assert ph.to_dict()["speed_spikes"] == [398]


def test_angular_spike_is_reported_but_not_gated():
    """角单帧尖峰只入报告不设闸：经典模式近极区 look-at 有合法高读数。"""
    n = 300
    quats = [quat_from_yaw_elev_roll(0.0, 0.0, 0.0)] * n
    quats = list(quats)
    quats[150:] = [quat_from_yaw_elev_roll(180.0, 0.0, 0.0)] * (n - 150)
    pos = [(0.1 * i, 30.0, 0.0) for i in range(n)]
    ph = met.physics(pos, quats, 1.0 / FPS)
    assert ph.angspeed_spike_frames and ph.angspeed_max > 1000.0
    assert ph.to_dict()["angspeed_spikes"][0] == 149
    assert not any("angspeed_spike" in x for x in ph.violations())

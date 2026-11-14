"""车辆导演模式（docs/78 §10；需求 §2.2/2.3 的「多汽车+四相机」联合控制）。

**结构（用户 0804 裁决）**：
- cam1–3 = 三辆受控飞车的**车载相机**：相机位姿 = 车体位姿 ∘ 挂载变换 ∘ 凝视策略，
  全部离线派生（仍是烘焙回放，辩题十不破）；`camera_Follow Offset` 如实填挂载偏移，
  `player_*` 填车体位姿（schema 语义吻合）；
- 三路画面共视按**严口径**（docs/72 修正单 001：三对两两双向 min ≥30% 同时成立的帧
  计时长，≥50% 门槛）——实现为三车沿共享走廊环线**近平行**行进（相位软耦合成簇，
  簇内弧距 ≤~24m）+ 凝视以簇前方共同注视区为主（focus-lock），自由观察段
  （左右看/抬头/低头/回头）三路错峰排班；
- cam4 = **全景观察位**：离线求解取景，全程把三车与共视注视区收进画幅（逐帧断言，
  允许单次 ≤2s 例外并计数），同时保留 §2.2 变速语义——沿观察路径按 ρ(t)·v_guide
  速率行进（cam4.sample_plan 恰 1 次大变速；『每分钟 1 次 ⇒ 300s 5 次』读法未采用，
  docs/78 §10 待需求方裁决）。

**解耦 vs 成簇的张力**（QC decouple 门 <50% 同步窗）：三车同向近平行天然趋同——
解法是**停走式速度机制**（stop-go regimes）：各车独立循环「悬停 / 巡航 / 冲刺」，
巡航与冲刺速度带错开（比值出 [0.7,1.4]），悬停错峰；相位死区控制器只在弧距漂出
±8m 死区后温和纠偏，保簇不锁速。自检用与 QC 同式的 decouple 度量实测把关。

红线机检（占据快照来自 SceneProbeExporter 的 scene.json）：穿模 clearance、
look-at 单次遮挡 ≤2s、cam4 三车+注视区恒在幅内（单次 ≤2s 例外计数）。
物理 p95 / 解耦 / 初始帧 / cam4 验形 / 共视严口径全部离线自检后才落盘。
"""

import json
import math
import os
import random
from dataclasses import dataclass, field
from pathlib import Path
from typing import Dict, List, Optional, Sequence, Tuple

from . import cam4 as cam4mod
from . import covis as covismod
from . import metrics as met
from . import pink as pinkmod
from .anchors import Anchor, Subject
from .export import write_clip
from .mathutil import (clamp, euler_tuple, quat_angle_deg, quat_forward,
                       quat_from_yaw_elev_roll, smoothstep5,
                       Vec3, vadd, vdist, vnorm, vscale, vsub, wrap180,
                       yaw_elev_from_forward)
from .planner import ASPECT_RES, FPS, ClipPlan, QuotaTracker
from .primitives import Move, Pose
from .sampler import Track
from .scene import Scene

class DirectorMove(Move):
    """导演模式的段描述：类名不在 23 类原语库里（凝视样式/观察位），family 固定
    为 "director"，不进语料配额。"""

    @property
    def family(self) -> str:  # noqa: D401
        return "director"


# ---------------------------------------------------------------- 常量

#: 车载相机挂载偏移（车体局部系 [x右, y上, z前]，m）——机位在车头上方，
#: 前向/侧向视线越过车体；回头段带仰角避开车顶（自机身入画为车载机位常态）
MOUNT_OFFSET = (0.0, 2.2, 2.5)
#: 环线端帽半径与车道横向/垂直偏移
OVAL_CAP_R = 12.0
LANE_OFFSETS_CARRIER = (-2.5, 0.0, 2.5)
Y_OFFSETS_CARRIER = (0.0, 1.0, 2.0)
#: 三车目标相位差（弧长 m）与死区（m）——簇内弧距 ≤ 2·(16+8)=48m 上界，
#: 共视的 80m 射线预算与 cam4 取景余量都按这个上界校核
#: 三车**横向并排**（纵向相位全 0）——扫参实证（/tmp/sweep_geom）：严口径 worst-pair
#: 对纵向错位极敏感（gap 6m→0.31、9m→0.21、12m→0.19），对横向分离宽容
#: （lat ±6m 仍 0.52）。原因：纵向错位 ⇒ 到共同注视区距离不同 ⇒ 画面足迹尺度失配，
#: min 口径按小足迹/大足迹惩罚；横向错位只带来视差，足迹同尺度。
PHASE_TARGETS = (0.0, 0.0, 0.0)
PHASE_DEADBAND = 3.0
PHASE_GAIN = 1.0
#: 三个**比值隔离**的巡航速度带（m/s）：任意两带速度比 <0.7 ⇒ 不落进 QC decouple
#: 的同步带 [0.7,1.4]。三车按轮换排班分别占据 L/M/H 带（轮换周期 15-25s，各车
#: 遍历全部速度档 ⇒ 「速度变化方式不同」与解耦同时成立）。
#: 并排编队下三车走同一速度带（纵向必须同步），解耦交给 SPEED_OSC 振荡
SPEED_BANDS = ((2.6, 3.0), (2.7, 3.1), (2.8, 3.2))
#: 纵向速度振荡（解耦主力）：振幅比 + 三路互质周期
SPEED_OSC_A = 0.62
SPEED_OSC_PERIODS_S = (7.6, 10.4, 13.0)
#: 凝视微游走：互质周期（yaw / pitch 各一组，s）与幅度（deg）。
#: 目的=打散三路 focus-lock 的同号角速率（QC decouple 判据③），幅度以
#: 不破严口径 overlap 0.30 为界（devpad 静态扫参：≤14° 时 worst-pair 降幅 <0.06）。
#: yaw 用**同一周期 + 120° 等距定相**：sin 对相位差 120° 的符号一致率恒为 1/3，
#: 任意载机对 2/3 的窗口角速率反号 ⇒ 判据③（同号+幅值比）系统性失效；
#: pitch 用互质周期小幅扰动即可。
WANDER_PERIOD_S = 11.3
WANDER_PHASES = (0.0, 2.0943951023931953, 4.1887902047863905)
WANDER_PERIODS_P_S = (7.3, 11.9, 14.7)
WANDER_AMP_DEG = 7.0
#: 自由观察段预算（严口径共视的一等约束，见 gaze_schedule 文档串）
#: 单路自由观察时长占比上限。三路错峰 ⇒ 全局非共视时长 ≈ 三倍此值，
#: 取 0.073 使合计 ≤22%，为 50% 共视门留足余量（300s 实测：0.13 时合计 34%，
#: 叠加纵向散布后 covis 跌到 23%）。
FREE_FRAC_PER_CAM = 0.073
FREE_SEG_LEN_S = (2.5, 4.5)
FREE_SEG_MEAN_S = 3.5
#: 片首保护带（s）：自由观察段不排进这一段。三路方位跨度在 SPREAD_RAMP_S 内
#: 渐开，期间共视本就吃紧，再叠自由观察会连锁失守。窗口坐标从这里起算——
#: 不是「算出来落在带内就丢弃」，那会让 cam_slot=0 永远拿不到窗（见 gaze_schedule）。
FREE_START_GUARD_S = 12.0
#: 横向摆动（位置解耦主力）：幅度与三路互质周期
LAT_WEAVE_AMP_M = 3.6
LAT_WEAVE_PERIODS_S = (11.7, 8.1, 14.4)
#: 注视区相对飞行线的垂直下沉（m）——注视城市几何而非天空：共视按场景表面
#: 采样计算，朝天空看时命中率→0、重叠必为 0（实测 16% 采样时刻命中 <0.2）；
#: 下沉同时满足需求「背景丰富」（画面主体为楼宇/街区而非空景）
FOCUS_Y_DROP = 0.0
#: 注视区在环线轴向的缓慢漂移（保持画面不呆板，幅度远小于环线尺度）
FOCUS_DRIFT_M = 26.0
FOCUS_DRIFT_T_S = 47.0
V_MAX = 5.5
ACCEL_MAX = 1.8
#: 注视区前视距离（m，从簇首车沿环线向前）
FOCUS_LOOKAHEAD = 28.0
#: 自由观察段样式（需求 §2.2 的视野转动清单）
GAZE_STYLES = ("look_left", "look_right", "look_up", "look_down", "look_back")
#: 载机相机 yaw 逐帧限速（°/s），取 < QC 瞬时硬帽 90。metrics.py 的「角单帧
#: 只记录不设闸」裁决针对经典模式的近天顶合法快摆；导演模式载机仰角全程
#: 远离极区（|elev| 实测 ≤~10°），无豁免场景 ⇒ 构造性对齐 QC 硬帽，
#: 生成侧与 QC 给同一个数（engine-data/README 两侧一致纪律）。
YAW_SLEW_CAP_DEGPS = 85.0
#: 红线：look-at 单次遮挡上限（s）
OCCLUSION_RUN_MAX_S = 2.0


# ---------------------------------------------------------------- 环线几何

class Oval:
    """走廊环线：两条直道 + 两个半圆端帽，按弧长参数化。"""

    def __init__(self, a: Vec3, b: Vec3, cap_r: float = OVAL_CAP_R):
        ab = vsub(b, a)
        self.axis_len = math.hypot(ab[0], ab[2])
        self.u = (ab[0] / self.axis_len, 0.0, ab[2] / self.axis_len)
        self.n = (-self.u[2], 0.0, self.u[0])
        self.cap_r = cap_r
        self.straight = max(10.0, self.axis_len - 2 * cap_r)
        self.y = (a[1] + b[1]) * 0.5
        mid = ((a[0] + b[0]) * 0.5, self.y, (a[2] + b[2]) * 0.5)
        self.center = mid
        half = self.straight * 0.5
        self.p0 = (mid[0] - self.u[0] * half, self.y, mid[2] - self.u[2] * half)
        self.p1 = (mid[0] + self.u[0] * half, self.y, mid[2] + self.u[2] * half)
        self.perimeter = 2 * self.straight + 2 * math.pi * cap_r

    def speed_limit(self, s: float, v_straight: float = 5.5,
                    v_cap: float = 3.4, edge: float = 8.0) -> float:
        """弯道限速：端帽内 v_cap，直道 v_straight，边界 edge 米内平滑过渡。"""
        s = s % self.perimeter
        # 到最近端帽区间的距离（端帽区间：[straight, straight+pi*r] 与 [2s+pi*r, P)）
        cap1 = (self.straight, self.straight + math.pi * self.cap_r)
        cap2 = (2 * self.straight + math.pi * self.cap_r, self.perimeter)
        d = min(self._dist_to_interval(s, cap1), self._dist_to_interval(s, cap2))
        if d <= 0.0:
            return v_cap
        w = smoothstep5(min(1.0, d / edge))
        return v_cap + (v_straight - v_cap) * w

    def _dist_to_interval(self, s: float, iv: Tuple[float, float]) -> float:
        a, b = iv
        if a <= s <= b:
            return 0.0
        d = min(abs(s - a), abs(s - b))
        return min(d, self.perimeter - d)

    def point(self, s: float, lane: float = 0.0, y_off: float = 0.0
              ) -> Tuple[Vec3, Vec3]:
        """弧长 s（模周长）→ (位置, 单位切向)。lane>0 = 外侧。"""
        s = s % self.perimeter
        r = self.cap_r
        u, n = self.u, self.n
        if s < self.straight:
            base = (self.p0[0] + n[0] * r + u[0] * s, self.y,
                    self.p0[2] + n[2] * r + u[2] * s)
            tang, out = u, n
        elif s < self.straight + math.pi * r:
            a = (s - self.straight) / r
            ca, sa = math.cos(a), math.sin(a)
            out = (n[0] * ca + u[0] * sa, 0.0, n[2] * ca + u[2] * sa)
            base = (self.p1[0] + out[0] * r, self.y, self.p1[2] + out[2] * r)
            tang = (u[0] * ca - n[0] * sa, 0.0, u[2] * ca - n[2] * sa)
        elif s < 2 * self.straight + math.pi * r:
            d = s - self.straight - math.pi * r
            base = (self.p1[0] - n[0] * r - u[0] * d, self.y,
                    self.p1[2] - n[2] * r - u[2] * d)
            tang, out = (-u[0], 0.0, -u[2]), (-n[0], 0.0, -n[2])
        else:
            a = (s - 2 * self.straight - math.pi * r) / r
            ca, sa = math.cos(a), math.sin(a)
            out = (-n[0] * ca - u[0] * sa, 0.0, -n[2] * ca - u[2] * sa)
            base = (self.p0[0] + out[0] * r, self.y, self.p0[2] + out[2] * r)
            tang = (-u[0] * ca + n[0] * sa, 0.0, -u[2] * ca + n[2] * sa)
        p = (base[0] + out[0] * lane, self.y + y_off, base[2] + out[2] * lane)
        return p, tang


# ---------------------------------------------------------------- 速度机制

def regime_speed_targets(rng: random.Random, duration: float, n_frames: int,
                         cruise_band: Tuple[float, float],
                         hover_budget: Tuple[int, int] = (3, 6),
                         hover_len: Tuple[float, float] = (1.8, 2.6)
                         ) -> Tuple[List[float], List[Tuple[float, float]]]:
    """（extra 车用）停走式逐帧目标速度：悬停/巡航循环。前 12s 强制巡航。"""
    events: List[Tuple[float, float, float]] = []   # (t0, t1, v)
    t = 0.0
    hovers: List[Tuple[float, float]] = []
    n_hover = rng.randint(*hover_budget)
    hover_ts = sorted(rng.uniform(duration * 0.06, duration * 0.94)
                      for _ in range(n_hover))
    hi = 0
    while t < duration:
        if hi < len(hover_ts) and t >= hover_ts[hi] and t > 12.0:
            d = rng.uniform(*hover_len)
            events.append((t, t + d, 0.0))
            hovers.append((t, t + d))
            t += d
            hi += 1
            continue
        d = rng.uniform(12.0, 24.0)
        events.append((t, t + d, rng.uniform(*cruise_band)))
        t += d
    out: List[float] = []
    for i in range(n_frames):
        ti = i / FPS
        v = events[-1][2]
        for t0, t1, ev in events:
            if t0 <= ti < t1:
                v = ev
                break
        out.append(v)
    return out, hovers


def carrier_speed_schedule(rng: random.Random, duration: float, n_frames: int,
                           guide_slot: int,
                           forbid: Optional[Tuple[float, float]] = None
                           ) -> Tuple[List[List[float]], List[List[Tuple[float, float]]]]:
    """三载机位的轮换排班速度：每个轮换期三车分占 L/M/H 三个比值隔离带，
    期与期之间按排列轮换（每车遍历全部速度档）；悬停错峰（slot 相位窗），
    guide 车悬停最少（QC 反算 ρ 需要 v_g 有效样本 ≥50%）。

    返回 (per-slot v_target 序列, per-slot 悬停区间)。
    """
    # 轮换期
    periods: List[Tuple[float, float, Tuple[int, int, int]]] = []
    t = 0.0
    k = 0
    perm = [0, 1, 2]
    while t < duration:
        d = rng.uniform(15.0, 25.0)
        rot = tuple(perm[(s + k) % 3] for s in range(3))
        periods.append((t, t + d, rot))
        t += d
        k += 1
    # 悬停错峰：把时间轴切成 3×n 窗，slot 只在自己的窗里悬停
    hovers_all: List[List[Tuple[float, float]]] = [[], [], []]
    n_h = max(2, int(duration / 75.0))
    window = duration / (3.0 * n_h)
    for slot in range(3):
        budget = 1 if slot == guide_slot else rng.randint(1, 2)
        wins = [w for w in range(n_h)]
        rng.shuffle(wins)
        for w in wins[:budget]:
            w0 = (w * 3 + slot) * window
            if w0 < 14.0:
                continue
            d = rng.uniform(1.8, 2.6)   # 运动常态：低速带减速尾计入后静止段 ≤~1.9s
            t0 = rng.uniform(w0, max(w0, w0 + window - d))
            t1 = min(t0 + d, w0 + window)
            if slot == guide_slot and forbid is not None \
                    and t1 > forbid[0] and t0 < forbid[1]:
                continue                      # guide 悬停避开 t_sw 邻域（ρ 可测性）
            hovers_all[slot].append((t0, t1))
    # 逐帧目标（并排编队修正）：三车纵向位置必须同步（严口径共视对纵向错位极敏感），
    # 但 QC decouple 判据②在「同向 + 速度比 ∈[0.7,1.4]」时判同步——并排同速必然命中。
    # 解法：**纵向速度振荡**——各车速度绕编队均速做互质周期振荡，振幅 SPEED_OSC_A
    # 使两车速度比大部分时间落在 [0.7,1.4] 之外；振荡对位移的贡献是有界项
    # v0·A·T/(2π)（典型 <2.5m，在相位死区内），不累积纵向漂移。
    v_targets: List[List[float]] = []
    for slot in range(3):
        vs: List[float] = []
        per_period_v: Dict[int, float] = {}
        for i in range(n_frames):
            ti = i / FPS
            v = None
            for pi, (t0, t1, rot) in enumerate(periods):
                if t0 <= ti < t1 or (pi == len(periods) - 1 and ti >= t1):
                    key = pi * 3 + slot
                    if key not in per_period_v:
                        band = SPEED_BANDS[rot[slot]]
                        per_period_v[key] = rng.uniform(*band)
                    v = per_period_v[key]
                    break
            if v is None:
                v = 3.0
            osc = 1.0 + SPEED_OSC_A * math.sin(
                2.0 * math.pi * ti / SPEED_OSC_PERIODS_S[slot] + WANDER_PHASES[slot])
            v = v * osc
            for h0, h1 in hovers_all[slot]:
                if h0 <= ti < h1:
                    v = 0.0
                    break
            vs.append(v)
        v_targets.append(vs)
    return v_targets, hovers_all


@dataclass
class VehicleSchedule:
    vid: int
    role: str
    lane: float
    y_off: float
    direction: int
    pauses: List[Tuple[float, float]]
    s_series: List[float] = field(default_factory=list)
    positions: List[Vec3] = field(default_factory=list)
    yaws: List[float] = field(default_factory=list)
    speeds: List[float] = field(default_factory=list)


def integrate_vehicle(oval: Oval, duration: float, n_frames: int,
                      lane: float, y_off, direction: int, s0: float,
                      v_target: List[float], pauses,
                      phase_ref: Optional[List[float]] = None,
                      phase_target=0.0) -> VehicleSchedule:
    """y_off 可为常量或 (base, amp, period_s, phase) 垂直起伏（空中「上下摆动」，
    同时是 QC 解耦门的方向去相关器：3D 速度方向被竖直分量岔开，cos<0.85）。"""
    """加速度限幅积分 + 相位死区控制（载机位保簇；extra 车 phase_ref=None 自由跑）。"""
    dt = 1.0 / FPS
    s = s0
    v = min(v_target[0], oval.speed_limit(s0))   # 运动常态：起帧已在巡航
    sched = VehicleSchedule(0, "", lane,
                            y_off[0] if isinstance(y_off, tuple) else y_off,
                            direction, list(pauses))
    for i in range(n_frames):
        vt = min(v_target[i], oval.speed_limit(s)) if v_target[i] > 0.0 else 0.0
        if phase_ref is not None and vt > 0.0:
            tgt = phase_target[i] if isinstance(phase_target, list) else phase_target
            err = (phase_ref[i] + tgt + s0) - s
            if err > PHASE_DEADBAND:
                vt = clamp(vt + PHASE_GAIN * (err - PHASE_DEADBAND),
                           0.0, min(V_MAX, oval.speed_limit(s)))
            elif err < -PHASE_DEADBAND:
                vt = clamp(vt + PHASE_GAIN * (err + PHASE_DEADBAND),
                           0.6, min(V_MAX, oval.speed_limit(s)))
        dv = clamp(vt - v, -ACCEL_MAX * dt, ACCEL_MAX * dt)
        v += dv
        s += direction * v * dt
        if isinstance(y_off, tuple):
            yb, amp, per, ph = y_off
            yo = yb + amp * math.sin(2.0 * math.pi * (i / FPS) / per + ph)
        else:
            yo = y_off
        p, tang = oval.point(s, lane, yo)
        yaw = math.degrees(math.atan2(tang[0] * direction, tang[2] * direction))
        if sched.yaws and v < 0.05:
            yaw = sched.yaws[-1]
        sched.s_series.append(s)
        sched.positions.append(p)
        sched.yaws.append(yaw)
        sched.speeds.append(v)
    return sched


# ---------------------------------------------------------------- 凝视排班

@dataclass
class GazeSegment:
    style: str
    t0: float
    t1: float
    rel_yaw: float = 0.0
    rel_pitch: float = 0.0


#: 轨道模式（v11，用户 0804「三个车的轨迹是否可以不一致」）：三车各自绕**同一注视区**
#: 飞行，半径/高度/相位/角速度各不相同 —— 轨迹真正不一致，但到注视区距离相近
#: ⇒ 画面足迹同尺度、严口径共视成立（扫参：R70 跨度25° worst-pair 0.76、达标率 90%；
#: R110 跨度25° 达标率 100%，全面优于并排编队的 0.52）。
#: 方位跨度同时是解耦杠杆：轨道切向速度方向差 ≈ 方位差，跨度 ≥45° 时
#: cos < 0.85 ⇒ QC 判据②不命中。取 [45°, 78°] 兼顾共视与解耦。
ORBIT_R_BAND = (52.0, 78.0)
ORBIT_SPREAD_BAND = (32.0, 56.0)
ORBIT_H_BAND = (-3.0, 5.0)
ORBIT_OMEGA_BAND = (1.9, 2.9)      # 度/秒（v=ω·r，r≈65m ⇒ 均速 2.2-3.3m/s，
#: 叠加 ±62% 速度振荡后峰值仍在表 2 的 6m/s p95 之内）
SPREAD_RAMP_S = 22.0


def _ramp_rate(t: float) -> float:
    """smoothstep5(t/T) 的时间导数：把方位偏移在 T 秒内平滑积满（起始同方位）。"""
    if t >= SPREAD_RAMP_S:
        return 0.0
    x = t / SPREAD_RAMP_S
    return (30.0 * x * x * (1.0 - x) ** 2) / SPREAD_RAMP_S


ORBIT_R_WOBBLE = (2.5, 5.5)        # 半径缓慢起伏（径向速度分量计入 p95）
#: 轨道切向速度的 p95 预算（m/s）。表 2 的帽是 6.0，这里留 10% 余量给径向/竖直分量
#: —— 实际速度是切向 ⊕ 径向起伏 ⊕ 高度起伏的合成，只按切向卡会贴线。
ORBIT_V_BUDGET = 5.4
#: 基准角速度回缩的下限比例：低于此值不再回缩，宁可让该 attempt 挂在 phys 上，
#: 也不要把车压到接近静止（撞 motion.static 红线，且共视/解耦全部要重标）。
ORBIT_W_FLOOR_RATIO = 0.55


def integrate_orbit(center: Vec3, duration: float, n_frames: int,
                    r0: float, h0: float, az0: float, omega_series: List[float],
                    r_wobble: float, r_period: float, h_period: float,
                    scene: Optional[Scene] = None) -> VehicleSchedule:
    """绕 center 的轨道积分：方位角由 omega_series 推进，半径/高度各自缓慢起伏。

    返回的 VehicleSchedule 与环线版同构（positions/yaws/s_series/speeds），
    下游（挂载、共视、cam4、导出）无需分支。
    """
    dt = 1.0 / FPS
    pos: List[Vec3] = []
    yaws: List[float] = []
    s_series: List[float] = []
    az = az0
    arc = 0.0
    # 机头方向取轨道**名义方向**（整段 omega 均值的符号，逐帧恒定），不跟随瞬时符号：
    # 渐开斜坡（d_az<0 时为负）与 ±62% 速度振荡叠加可让 omega 短暂过零（clip-013
    # seed=13 实证 t≈13.3–15.5s），若机头随瞬时符号 180° 调头，2.5m 前挂相机
    # 单帧平移 2×2.5=5m（docs/69 §3.1 段间连续违例）。过零窗内按「短暂倒车」处理，
    # yaw 随 az 连续演化。当前构造下 omega 均值符号即 omega_sign（振荡幅 <1、
    # 斜坡只在前 SPREAD_RAMP_S 秒），若未来引入整段中途反向需改为分段名义方向。
    head_sgn = 1.0 if sum(omega_series) >= 0.0 else -1.0
    for i in range(n_frames):
        t = i * dt
        r = r0 + r_wobble * math.sin(2.0 * math.pi * t / r_period)
        h = h0 + 1.6 * math.sin(2.0 * math.pi * t / h_period + 0.7)
        a = math.radians(az)
        p = (center[0] + r * math.cos(a), center[1] + h, center[2] + r * math.sin(a))
        pos.append(p)
        # 机头朝切向（名义飞行方向）
        tang = (-math.sin(a) * head_sgn, 0.0, math.cos(a) * head_sgn)
        yaws.append(math.degrees(math.atan2(tang[0], tang[2])))
        step = abs(math.radians(omega_series[i])) * r * dt
        arc += step
        s_series.append(arc)
        az += omega_series[i] * dt
    # speeds 与环线版同语义：标量速率（m/s）
    speeds = [0.0]
    for i in range(1, n_frames):
        speeds.append(vnorm(vsub(pos[i], pos[i - 1])) * FPS)
    speeds[0] = speeds[1] if len(speeds) > 1 else 0.0
    sch = VehicleSchedule(0, "", 0.0, 0.0, 1, [])
    sch.positions = pos
    sch.yaws = yaws
    sch.s_series = s_series
    sch.speeds = speeds
    # 粉色叠加窗重积分所需的全部参数（pink.reintegrate_carrier 复刻上面的
    # 基线公式并做前缀一致性守卫——改本函数的轨道公式必须同步改 pink.py）
    sch.orbit_args = {"center": center, "duration": duration,
                      "n_frames": n_frames, "r0": r0, "h0": h0, "az0": az0,
                      "omega_series": list(omega_series), "r_wobble": r_wobble,
                      "r_period": r_period, "h_period": h_period}
    return sch


def gaze_schedule(rng: random.Random, duration: float, cam_slot: int
                  ) -> List[GazeSegment]:
    """focus-lock 为主 + 自由观察段**三路错峰**（同一时刻至多一路自由看）。

    **预算化（严口径修正）**：严口径要求三对同时达标，任一路自由看即该时刻三对
    中有两对失守 ⇒ 全局非共视时长 ≈ 三路自由段时长之和。旧版每路固定 5 段
    ×(3.5-6.5)s，60s 条累计 75s > 时长本身，严口径下永不可能达标（实测 covis
    13-22%）。改为按 FREE_FRAC_PER_CAM 预算分配：单路自由时长 ≤13% 时长，
    三路合计 ≤40%，为 50% 共视门留 10pt 余量。样式覆盖随时长增长（60s 覆盖
    2-3 种，300s 可覆盖全部 5 种多轮）——「视野转动多样」是语料级指标。"""
    styles = list(GAZE_STYLES)
    rng.shuffle(styles)
    n_free = max(1, int(duration * FREE_FRAC_PER_CAM / FREE_SEG_MEAN_S))
    # 错峰窗口铺在 [FREE_START_GUARD_S, duration] 上，**不是** [0, duration]。
    # 旧式 `w0 = (k*3+cam_slot)*window` 从 0 起铺，再用 `if w0 < 12: continue`
    # 把落进保护带的窗**丢弃**而非挪走——cam_slot=0 的 k=0 窗恒为 w0=0，必被丢。
    # 60s 档 n_free=1 ⇒ cam1 的唯一一个窗被吞，**任何 seed 都拿不到自由观察段**
    # （实测 dir13/rt_ctl21/rt_rev21 三条 clip 的 cam1 全是 ['mounted_focus']）；
    # 300s 档 n_free=6 ⇒ cam1 只丢首窗，所以缺陷只在**唯一的交付档**上致命。
    # 现在把保护带从窗口坐标里扣掉，三路仍严格错峰、每路拿满 n_free 个窗。
    span = max(FREE_SEG_MEAN_S, duration - FREE_START_GUARD_S)
    window = span / (3.0 * n_free)
    segs: List[GazeSegment] = []
    for k in range(n_free):
        w0 = FREE_START_GUARD_S + (k * 3 + cam_slot) * window
        w1 = w0 + window
        style = styles[k % len(styles)]
        dur = rng.uniform(*FREE_SEG_LEN_S)
        t0 = rng.uniform(w0, max(w0, w1 - dur - 3.0))
        t1 = min(w1 - 3.0, t0 + dur)
        if t1 <= t0:
            continue
        if style == "look_left":
            seg = GazeSegment(style, t0, t1, rng.uniform(55.0, 92.0), rng.uniform(-8.0, 12.0))
        elif style == "look_right":
            seg = GazeSegment(style, t0, t1, -rng.uniform(55.0, 92.0), rng.uniform(-8.0, 12.0))
        elif style == "look_up":
            seg = GazeSegment(style, t0, t1, rng.uniform(-25.0, 25.0), rng.uniform(28.0, 50.0))
        elif style == "look_down":
            seg = GazeSegment(style, t0, t1, rng.uniform(-25.0, 25.0), -rng.uniform(28.0, 55.0))
        else:
            side = 1.0 if rng.random() < 0.5 else -1.0
            seg = GazeSegment(style, t0, t1, side * rng.uniform(130.0, 145.0),
                              rng.uniform(12.0, 22.0))
        segs.append(seg)
    return segs


def gaze_weight(segs: List[GazeSegment], t: float, blend: float = 3.5
                ) -> Tuple[float, float, float]:
    for s in segs:
        if s.t0 - blend <= t <= s.t1 + blend:
            if t < s.t0:
                w = smoothstep5((t - (s.t0 - blend)) / blend)
            elif t > s.t1:
                w = 1.0 - smoothstep5((t - s.t1) / blend)
            else:
                w = 1.0
            return w, s.rel_yaw, s.rel_pitch
    return 0.0, 0.0, 0.0


# ---------------------------------------------------------------- 工具

def mount_world(vp: Vec3, yaw_deg: float, mount: Vec3 = MOUNT_OFFSET) -> Vec3:
    """车体位姿（位置+yaw）∘ 挂载偏移 → 相机世界位（Unity 左手系绕 +Y）。"""
    a = math.radians(yaw_deg)
    ca, sa = math.cos(a), math.sin(a)
    mx, my, mz = mount
    return (vp[0] + mx * ca + mz * sa, vp[1] + my, vp[2] - mx * sa + mz * ca)


def runs_over(flags: Sequence[bool], dt: float, run_max_s: float
              ) -> Tuple[int, float]:
    """连续 True 段统计：(超过上限的段数, 最长段秒)。"""
    worst = 0.0
    over = 0
    run = 0
    for f in list(flags) + [False]:
        if f:
            run += 1
        else:
            if run:
                sec = run * dt
                worst = max(worst, sec)
                if sec > run_max_s:
                    over += 1
            run = 0
    return over, worst


def low_pass_subject(series: List[Vec3], n_frames: int, knot_s: float = 10.0
                     ) -> Subject:
    step = int(round(knot_s * FPS))
    knots = [series[min(i, n_frames - 1)] for i in range(0, n_frames + step, step)]
    return Subject(knots, knot_s)


# ---------------------------------------------------------------- 主流程

def plan_director_clip(scene_file: str, seed: int, out_dir,
                       duration_s: float = 300.0,
                       n_extras: int = 2,
                       covis_hz: float = 1.0,
                       max_attempts: int = 8,
                       quota: Optional[QuotaTracker] = None) -> Dict:
    """生成一条车辆导演 clip：车队计划 + 4 路相机烘焙 + 全部红线自检 + 落盘。

    任一门槛在 max_attempts 次重排内过不去即抛 RuntimeError（宁可不出货）。
    返回并落盘 director_report.json。
    """
    raw = json.loads(Path(scene_file).read_text(encoding="utf-8"))
    scene = Scene.from_dict(raw)
    corridors = raw.get("anchor_corridors") or []
    if not corridors:
        raise RuntimeError("scene.json 无 anchor_corridors——车辆导演模式需要走廊元数据")
    cor = next((c for c in corridors if c.get("chosen")), corridors[0])
    world_off = raw.get("world_offset") or [0.0, 0.0, 0.0]

    a_l = tuple(cor["start_w"][i] - world_off[i] for i in range(3))
    b_l = tuple(cor["end_w"][i] - world_off[i] for i in range(3))
    margin = OVAL_CAP_R + 6.0
    lo, hi = scene.lo, scene.hi

    def clip_to_bounds(p, q):
        """线段-slab 裁剪到 XZ 边界内缩 margin 的窗口。"""
        t0, t1 = 0.0, 1.0
        for ax in (0, 2):
            d = q[ax] - p[ax]
            if abs(d) < 1e-9:
                continue
            ta = (lo[ax] + margin - p[ax]) / d
            tb = (hi[ax] - margin - p[ax]) / d
            if ta > tb:
                ta, tb = tb, ta
            t0 = max(t0, ta)
            t1 = min(t1, tb)
        t0, t1 = clamp(t0, 0.0, 1.0), clamp(t1, 0.0, 1.0)
        return (tuple(p[i] + (q[i] - p[i]) * t0 for i in range(3)),
                tuple(p[i] + (q[i] - p[i]) * t1 for i in range(3)))

    a_l, b_l = clip_to_bounds(a_l, b_l)
    n_frames = int(round(duration_s * FPS))
    dt = 1.0 / FPS
    quota = quota or QuotaTracker()

    report: Dict = {"seed": seed, "duration_s": duration_s, "mode": "vehicle-director",
                    "attempts": []}
    last_fail = "unknown"

    for attempt in range(max_attempts):
        rng = random.Random(seed * 1000003 + attempt)
        # 环线横移探测：走廊开阔带未必以路线为中心（v1 勘察：一侧贴墙），
        # 把整条环线往净空侧平移到三条车道整圈都有净空的位置
        oval = None
        base_oval = Oval(a_l, b_l)
        for shift in (0.0, -4.0, -6.0, 4.0, -8.0, 6.0):
            nx, nz = base_oval.n[0] * shift, base_oval.n[2] * shift
            cand = Oval((a_l[0] + nx, a_l[1], a_l[2] + nz),
                        (b_l[0] + nx, b_l[1], b_l[2] + nz))
            feasible = True
            for lane in LANE_OFFSETS_CARRIER:
                s_probe = 0.0
                while s_probe < cand.perimeter:
                    pp, _ = cand.point(s_probe, lane, 0.0)
                    if scene.clearance(pp) <= 0.2 or scene.clearance(
                            (pp[0], pp[1] + MOUNT_OFFSET[1], pp[2])) <= 0.2:
                        feasible = False
                        break
                    s_probe += 3.0
                if not feasible:
                    break
            if feasible:
                oval = cand
                break
        if oval is None:
            report["attempts"].append({"attempt": attempt,
                                       "bad": ["corridor_lateral_headroom"]})
            last_fail = "corridor_lateral_headroom"
            continue

        # ---- 车队 ----
        ref_targets, _ = regime_speed_targets(rng, duration_s, n_frames,
                                              (2.6, 3.6), hover_budget=(0, 0))
        phase_ref = []
        acc = 0.0
        for i in range(n_frames):
            acc += ref_targets[i] * dt
            phase_ref.append(acc)

        # 载机位垂直偏移的净空探测：过街桥/悬挑会吃掉高车道（clearance 红线前置）
        def safe_y_off(lane: float, want: float) -> float:
            for cand in (want, max(0.0, want - 1.0), max(0.0, want - 2.0), 0.0):
                ok_all = True
                s_probe = 0.0
                while s_probe < oval.perimeter:
                    pp, _ = oval.point(s_probe, lane, cand)
                    if scene.clearance((pp[0], pp[1] + MOUNT_OFFSET[1], pp[2])) <= 0.2 \
                            or scene.clearance(pp) <= 0.2:
                        ok_all = False
                        break
                    s_probe += 3.0
                if ok_all:
                    return cand
            return -1.0

        weave = [(2.2, 8.0), (2.6, 11.5), (2.0, 14.5)]   # (幅度, 周期s)——互质周期
        y_offs = []
        for slot in range(3):
            amp = weave[slot][0]
            base = safe_y_off(LANE_OFFSETS_CARRIER[slot],
                              Y_OFFSETS_CARRIER[slot] + amp)
            if base >= 0.0:
                base = max(base, amp * 0.5)   # 底部也要离路带有余量
            y_offs.append(base)
        if any(yo < 0.0 for yo in y_offs):
            report["attempts"].append({"attempt": attempt,
                                       "bad": ["corridor_headroom(lanes=%s)" % y_offs]})
            last_fail = "corridor_headroom"
            continue

        # 直道起步：簇与前视区前 90-120s 停留在同一条直道上
        # （初始帧朝向/距离闸门 + cam4 进场期取景的结构性保证）
        base_s = 15.0 + rng.uniform(0.0, max(5.0, oval.straight - 135.0))
        vehicles: List[VehicleSchedule] = []
        guide_slot = rng.randrange(3)   # 初值；建轨后按实测速度改选（见 cam4 段）
        # cam4 变速计划**移到相机 pass 之后**采样：ρ 档位可达性必须按 guide 相机的
        # 实测速度反推。旧写法在这里用 SPEED_BANDS/V_MAX 死常量算 rho_allowed=1.27，
        # 快档 [1.65,3] 被**恒久裁掉**（S1-5 缺陷）——「快进版」语义从未真正出现过。
        # forbid=None：轨道模式不消费悬停排班（v_targets/hovers 为环线模式遗留）。
        v_targets, hovers_all = carrier_speed_schedule(
            rng, duration_s, n_frames, guide_slot, forbid=None)
        # ---- 载机：轨道模式（三车轨迹各不相同，共绕同一注视区）----
        orbit_center = None   # 由通视性预筛决定（见下）
        # 注视中心通视性预筛：从轨道一圈采样点到中心的视线必须多数畅通，
        # 否则 look-at 被楼体长时间遮挡（occlusion 红线，实测 13s 违约）
        def center_los_ok(c: Vec3, r_probe: float) -> float:
            hits = 0
            n_probe = 16
            for k in range(n_probe):
                a = 2.0 * math.pi * k / n_probe
                p = (c[0] + r_probe * math.cos(a), c[1], c[2] + r_probe * math.sin(a))
                if not scene.blocked(p, c):
                    hits += 1
            return hits / n_probe

        # ---- 中心预筛与下游红线对齐（本次修复） ----
        # 旧版三处失配，实测占全部失败的大头（40 种子基线：clearance 9 /
        # occlusion 9 / orbit_center_los 9，三者高度重合=同一批种子）：
        #   ① 只抽 12 个候选，clearance≤6 一刀切 ⇒ 候选耗尽时 best_c 从未赋值
        #      （失败值 -1.00 全是这个，不是通视性差）；
        #   ② LOS 只探中位半径 65m 的一圈、单一高度——载机实际跑 52–83.5m
        #      （含 r_wobble）、高度带 ±3~+5 再加 2.2m 挂载，预筛过了下游照挂；
        #   ③ 候选窗 ±40 不夹快照边界，出界处占据未知（越界视为空），
        #      预筛在盲区里给了假阳性。
        # 现在：48 个候选、窗夹进快照内缩 8m；每候选除中心净空外，再对
        # **真实环带**（3 半径 × 12 方位 × 2 高度，只统计快照内的点）做净空
        # 达标率，LOS 在环带内外沿各探一次取较小——预筛与红线同一套几何。
        def ring_stats(c: Vec3) -> Tuple[float, float]:
            los = min(center_los_ok(c, ORBIT_R_BAND[0]),
                      center_los_ok(c, ORBIT_R_BAND[1] + ORBIT_R_WOBBLE[1]))
            n_in = 0
            n_ok = 0
            for rr in (ORBIT_R_BAND[0],
                       (ORBIT_R_BAND[0] + ORBIT_R_BAND[1]) / 2.0,
                       ORBIT_R_BAND[1] + ORBIT_R_WOBBLE[1]):
                for k in range(12):
                    a = 2.0 * math.pi * k / 12.0
                    x = c[0] + rr * math.cos(a)
                    z = c[2] + rr * math.sin(a)
                    if not (lo[0] <= x <= hi[0] and lo[2] <= z <= hi[2]):
                        continue      # 快照外无占据信息，探了也是盲的
                    for y in (c[1] + ORBIT_H_BAND[0],
                              c[1] + ORBIT_H_BAND[1] + MOUNT_OFFSET[1]):
                        n_in += 1
                        if scene.clearance((x, y, z)) > 2.0:
                            n_ok += 1
            return los, (n_ok / n_in) if n_in else 1.0

        # v1 教训（实测 32.5%，比不修的 52.5% 还差）：把环带净空 ≥0.85 与双半径
        # LOS ≥0.75 设为**硬门**会把可行域收死——快照仅 50m 宽，r≥52 的环必然
        # 大半在快照外，环带达标率的分母只剩快照内那一小段，硬门等于要求几何上
        # 不存在的完美中心（orbit_center_los 失败 7→16）。
        # v2：**准入门与旧版逐字一致**（中位半径 LOS ≥0.75），环带净空与双半径
        # LOS 只作**排序键**——在同样的可行域里挑环带最干净的候选。选择变好、
        # 可行域不变，最坏不劣于旧版（rng 序列变化带来的种子级抖动除外）。
        best_c = None
        best_key = (-1.0, -1.0, -1.0)   # (环带净空达标率, 双半径LOS较小值, 中位LOS)
        best_mid = -1.0
        for _try in range(48):
            cand_c = (oval.center[0] + rng.uniform(-60.0, 60.0),
                      oval.y + rng.uniform(*ORBIT_H_BAND) * 0.4,
                      oval.center[2] + rng.uniform(-40.0, 40.0))
            if scene.clearance(cand_c) <= 6.0:
                continue
            los_mid = center_los_ok(cand_c, sum(ORBIT_R_BAND) / 2.0)
            if los_mid < 0.75:
                continue                # 旧版准入门，原样保留
            los_min, ring_frac = ring_stats(cand_c)
            key = (ring_frac, los_min, los_mid)
            if key > best_key:
                best_key, best_c, best_mid = key, cand_c, los_mid
            if ring_frac >= 0.98 and los_min >= 0.9:
                break
        if best_c is None:
            report["attempts"].append({"attempt": attempt,
                                       "bad": ["orbit_center_los=%.2f" % best_mid]})
            last_fail = "orbit_center_los"
            continue
        spread = rng.uniform(*ORBIT_SPREAD_BAND)
        az_base = rng.uniform(0.0, 360.0)
        az0s = [az_base - spread / 2.0, az_base + rng.uniform(-8.0, 8.0),
                az_base + spread / 2.0]
        rng.shuffle(az0s)
        omega_sign = 1.0 if rng.random() < 0.5 else -1.0
        # 方位跨度**渐开**：t=0 三车聚在同一方位（初始帧朝向一致，69 §2），
        # 约 SPREAD_RAMP_S 内展开到目标跨度 ⇒ 既过初始帧闸门，又实现「轨迹不一致」
        for slot in range(3):
            # 各车独立：半径 / 高度 / 起始方位 / 角速度剖面（互质周期振荡）
            r0 = rng.uniform(*ORBIT_R_BAND)
            h0 = rng.uniform(*ORBIT_H_BAND)
            w_mean = rng.uniform(*ORBIT_OMEGA_BAND) * omega_sign
            w_per = SPEED_OSC_PERIODS_S[slot]
            d_az = az0s[slot] - az_base
            # r_wobble 提前抽（原来在 integrate_orbit 的实参里抽）：速度预算要用
            # 本车的**半径上界** r0 + r_wobble，不能用注释里那个 r≈65 的估值。
            r_wob = rng.uniform(*ORBIT_R_WOBBLE)
            r_max = r0 + r_wob

            # ---- 速度预算：把渐开斜坡计进 ω → v 的换算（本次修复） ----
            # ORBIT_OMEGA_BAND 的注释按「r≈65m ⇒ 均速 2.2-3.3m/s，叠加 ±62% 振荡后
            # 仍在 6m/s 内」立论，**漏了渐开项**：d_az·_ramp_rate 峰值
            #   |d_az| · max(_ramp_rate) = 28° · 30·0.25²/22 = ±2.39 °/s
            # 与 w_mean 本身同量级，而 SPREAD_RAMP_S=22s 是**固定值**——
            # 60s 档占 37% 的帧、300s 档只占 7%。独立数值模型（4000 次抽样）：
            #   60s  含渐开 P(v_p95>6) = 0.137  去掉渐开 0.052
            #   300s 含渐开 0.053            去掉渐开 0.052
            # 这就是「300s 一次通过、60s 通不过」的成因，与实测 60s 档 phys 失败
            # 占全部失败 38.7% 吻合。
            #
            # 只回缩基准项 w_mean：渐开项的时间积分恒等于 d_az（smoothstep 定义），
            # 缩放它会直接改掉方位跨度 ⇒ 破坏共视与解耦的标定。
            w_floor = abs(w_mean) * ORBIT_W_FLOOR_RATIO
            for _ in range(8):
                w_amp = abs(w_mean) * SPEED_OSC_A
                omega = [w_mean
                         + w_amp * math.sin(2.0 * math.pi * (i / FPS) / w_per
                                            + WANDER_PHASES[slot])
                         + d_az * _ramp_rate(i / FPS)
                         for i in range(n_frames)]
                vs = sorted(abs(math.radians(w)) * r_max for w in omega)
                v_p95 = vs[int(0.95 * (len(vs) - 1))]
                if v_p95 <= ORBIT_V_BUDGET or abs(w_mean) <= w_floor:
                    break
                shrink = max(ORBIT_V_BUDGET / v_p95, w_floor / abs(w_mean))
                w_mean *= shrink

            sched = integrate_orbit(
                best_c, duration_s, n_frames, r0, h0, az_base, omega,
                r_wobble=r_wob,
                r_period=LAT_WEAVE_PERIODS_S[slot] * 3.2,
                h_period=SPEED_OSC_PERIODS_S[(slot + 1) % 3] * 3.4)
            sched.vid = slot + 1
            sched.role = "carrier-%d" % (slot + 1)
            vehicles.append(sched)
        for k in range(n_extras):
            v_target, hovers = regime_speed_targets(
                rng, duration_s, n_frames, (2.0, 4.6), hover_budget=(1, 2))
            sched = integrate_vehicle(
                oval, duration_s, n_frames,
                10.0 + 4.0 * (k % 2), rng.uniform(6.0, 10.0), -1,
                rng.uniform(0.0, oval.perimeter), v_target, hovers)
            sched.vid = 4 + k
            sched.role = "extra"
            vehicles.append(sched)
        carriers = vehicles[:3]

        # ---- 注视区 F(t)：簇首车前方 FOCUS_LOOKAHEAD，10s 低通 ----
        # 前视 70m（初始帧视差闸门）→ 32m 工作值；队距 ±9m 由静态勘察定
        # （devpad 网格实测：worst-pair overlap 随队距 g 陡降，g≈8-10 → 0.7+，
        #  g≥16 → ≤0.3；L 在 20-70m 影响弱——严口径共视的第一变量是队形紧密度）
        focus_raw: List[Vec3] = []
        for i in range(n_frames):
            t_i = i / FPS
            # **中心注视**（v7）：注视区取环线中心邻域的固定区域，而非「沿路前方 L 米」。
            # 前视式在端帽处会绕过弯道跑到编队侧后方，cam4 的角跨度爆掉
            # （实测固定在进弯的 36-38s 出幅）；中心式让三车并排绕飞、共同侧视同一
            # 区域，角跨度恒有界，且「从不同角度观察同一场景」正是需求 §2.3 的宗旨。
            # 轨道模式：注视区 = 轨道中心邻域（三车共同观察对象），缓慢漂移
            drift = FOCUS_DRIFT_M * 0.35 * math.sin(2.0 * math.pi * t_i / FOCUS_DRIFT_T_S)
            fp = (best_c[0] + oval.u[0] * drift, best_c[1] + FOCUS_Y_DROP,
                  best_c[2] + oval.u[2] * drift)
            focus_raw.append(fp)
        focus = low_pass_subject(focus_raw, n_frames)

        # ---- 粉色 6 类平移叠加窗：载机三路（pink.py；docs/89 §5.1）----
        # 朝向归属裁决：窗只动**位置**，朝向恒归 gaze/focus-lock（两通道正交，
        # 允许时间重叠）——诚实性契约以窗起帧真实相机 yaw 定基，无论该 yaw 来自
        # focus-lock 还是自由观察段，声称轴都是相机当刻视向系下的真实平移；
        # 共视预算（FREE_FRAC_PER_CAM 标定）与错峰排班完全不被扰动。
        # gaze 排班为此从相机 pass 前置到这里（轴向预测要复刻 yaw 合成公式）。
        gaze_all: List[List[GazeSegment]] = [
            gaze_schedule(rng, duration_s, slot) for slot in range(3)]

        # guide 预选（cam4 同路径异速率的两个前置约束，用**基线车速**即可判）：
        # 1) 排除早期 ω 过零（「短暂倒车」）的载机——倒车期 |v_g| 贴零爬行被
        #    起伏搓出 V 形折点簇，v₄=ρ·v_g 把折点簇按 ρ 放大成 jerk 尖峰簇；
        # 2) 其余里取速度 p95 最慢者——快档 ρ∈[1.65,1.85] 只有慢 guide 喂得起。
        # 预选（而非建轨后再选）是为了给 guide 的粉色窗走 gentle 档：guide 的
        # 斜坡 jerk 会被快档按 ρ 放大回放，普通档实测把 cam4 p95 顶到 20-40。
        def _omin(k: int) -> float:
            om = carriers[k].orbit_args["omega_series"]
            sgn_ = 1.0 if sum(om) >= 0 else -1.0
            return min(o * sgn_ for o in om)

        _veh_p95 = [met.percentile(carriers[k].speeds, 95) for k in range(3)]
        _cand = [k for k in range(3) if _omin(k) > 0.2]
        _pool = _cand or [max(range(3), key=_omin)]
        guide_slot = min(_pool, key=lambda k: _veh_p95[k])

        try:
            pink_wins_car = pinkmod.schedule_all_carriers(
                rng, duration_s, gentle_slot=guide_slot)
        except RuntimeError as e:
            report["attempts"].append({"attempt": attempt,
                                       "bad": ["pink.schedule:%s" % e]})
            last_fail = "pink.schedule"
            continue

        def _mk_cam_yaw_fn(slot_, segs_):
            def f(t_, veh_pos, veh_yaw):
                # 与相机 pass 的 yaw 合成同式，但**不含**相机 pass 新增的相位解缠
                # 与 YAW_SLEW_CAP 限速（两者都是时序状态机，这里是无状态点查询，
                # 复刻不了）。偏差只在解缠/限速实际介入的帧附近出现——预测仅用于
                # 窗轴整形，诚实性自检始终用最终 quat：偏差只轻微折损 share，
                # 不会造成假标注（原分析不变）。
                cp = mount_world(veh_pos, veh_yaw)
                look_yaw, _ = yaw_elev_from_forward(vsub(focus.at(t_), cp))
                w_ramp = smoothstep5(clamp((t_ - 4.0) / 12.0, 0.0, 1.0))
                wy = w_ramp * WANDER_AMP_DEG * math.sin(
                    2.0 * math.pi * t_ / WANDER_PERIOD_S + WANDER_PHASES[slot_])
                w_, ry, _rp = gaze_weight(segs_, t_)
                return look_yaw + wy + wrap180(veh_yaw + ry - (look_yaw + wy)) * w_
            return f

        pink_absorbs = []
        for slot in range(3):
            car = carriers[slot]
            p2, y2, s2, v2, alog = pinkmod.reintegrate_carrier(
                car.orbit_args, pink_wins_car[slot],
                _mk_cam_yaw_fn(slot, gaze_all[slot]), FPS)
            # 前缀守卫：首窗刹车开始前必须与原积分一致（两处基线公式漂移即炸）
            t_first = min(w.t0 for w in pink_wins_car[slot])
            guard_n = max(1, min(n_frames,
                                 int((t_first - pinkmod.CAR_T_BRAKE) * FPS) - 2))
            drift = max(vdist(car.positions[k], p2[k]) for k in range(guard_n))
            if drift > 1e-6:
                raise RuntimeError("pink 重积分与 integrate_orbit 公式漂移 %.2e m"
                                   % drift)
            car.positions, car.yaws, car.s_series, car.speeds = p2, y2, s2, v2
            pink_absorbs.append(alog)

        # ---- cam1-3：挂载位姿 + 凝视 ----
        # 焦距按相位序分配（f ∝ 到注视区距离 ⇒ 三路画面足迹尺度一致，
        # 与经典模式「焦距越长轨道越远」同一原理——严口径共视的结构前提）：
        # 队尾（相位 -16，距 F ~60m）50mm；中位 35mm；队首（+16，~28m）24mm
        # 焦距按相位序等化足迹（f ∝ 到 F 距离；±9 相位 + 前视 26m ⇒ 距离约
        # 35/26/17m，50/35/24 的 d/f 为 0.70/0.74/0.71，失配 <6%）——v2 曾试统一
        # 焦段，实测足迹失配放大、pairs_mean 0.25→0.20，已回退
        # 并排编队下三车到注视区距离一致 ⇒ 焦距必须统一（混焦=足迹尺度失配，
        # min 口径下 O(广→窄) ≈ 窄/广 立体角比，24/50 组合结构性封顶 ~0.23）。
        # 焦段逐 clip 轮转，4 档配比由批账本在 clip 间配平。
        # 载机焦段仅在 {24,35} 轮转：50mm 窄 FOV 使严口径 overlap 对视差过敏
        # （扫参：24mm 0.475 / 35mm 0.429 / 混焦 ≤0.29）。50mm 档由经典模式 clip
        # 在批账本层面配平 4 档配比。
        # 用 rng 抽，不用 seed 奇偶。原写法 `(24, 35)[seed % 2]` 把焦段绑死在种子
        # 奇偶上：编排器按 seed_base 递增排产时没问题，但只要有人挑一批偶数种子
        # （或按 seed+2 重试），整批就全是 24mm，焦距配比结构性失衡且不易察觉。
        # 注：50mm 仍不进载机轮转 —— 见上方扫参依据（窄 FOV 对视差过敏）。
        f_uni = rng.choice((24, 35))
        focals3 = [f_uni, f_uni, f_uni]
        aspect = quota.pick_aspect(rng)
        w_px, h_px = ASPECT_RES[aspect]
        tracks: List[Track] = []
        for slot in range(3):
            car = carriers[slot]
            segs = gaze_all[slot]        # 已在粉色叠加窗前置排好
            pos: List[Vec3] = []
            quats = []
            eulers = []
            gaze_dc = None    # 凝视偏差角的相位解缠状态（甩头修复，见下）
            prev_yaw = None   # yaw 限速器状态
            for i in range(n_frames):
                t = i / FPS
                cp = mount_world(car.positions[i], car.yaws[i])
                f = focus.at(t)
                look_yaw, look_elev = yaw_elev_from_forward(vsub(f, cp))
                # 凝视微游走（docs/71 §5「tracking 朝向同步」解法的车载版）：
                # 三路 focus-lock 锁同一 F(t) ⇒ yaw 速率同号同幅，QC 判据③
                # （同号角速率比值 0.7-1.4）几乎全程命中——decouple 55% 泄漏的主因。
                # 逐路互质周期 + 独立相位的小幅偏摆使角速率符号规律性反相；
                # 幅度受 WANDER_AMP_DEG 限制，F 仍居中，严口径 overlap 0.30 门不破。
                # 渐入斜坡：init.angle 门（t=0 两两 forward ≤15°）不吃游走偏角
                w_ramp = smoothstep5(clamp((t - 4.0) / 12.0, 0.0, 1.0))
                wy = w_ramp * WANDER_AMP_DEG * math.sin(
                    2.0 * math.pi * t / WANDER_PERIOD_S + WANDER_PHASES[slot])
                wp_ = w_ramp * 0.45 * WANDER_AMP_DEG * math.sin(
                    2.0 * math.pi * t / WANDER_PERIODS_P_S[slot]
                    + WANDER_PHASES[slot] * 1.7 + 1.1)
                w, ry, rp = gaze_weight(segs, t)
                # ---- 甩头修复（angspeed 瞬时 90-4812°/s 两族缺陷的同一根因）----
                # 旧式 `wrap180(gaze−base)*w` 在偏差角穿越 ±180° 时输出瞬跳 360°，
                # 乘 w 后是 **360·w 度的真实单帧转动**：w≈0.44 ⇒ 158°/帧 ≈ 4750°/s
                # （s18 cam1 实测 4812；clip-011 cam2 5016 同源）；游走正弦让偏差
                # 在边界反复穿越 ⇒ 0.5s 量级的持续甩头段。压线族（90-98°/s）则是
                # 混合斜坡边缘的 D·w′ 项（|D|≈180° × max w′≈0.53/s ≈ 95）。
                # 修法一（根治）：对偏差角做**时间相位解缠**——逐帧只允许走短弧，
                # 解缠后的 D(t) 连续，混合不再经过断点；±360 漂移对四元数无影响
                # （双覆盖），导出前再归一。
                base = look_yaw + wy
                d_raw = wrap180(car.yaws[i] + ry - base)
                if gaze_dc is None:
                    gaze_dc = d_raw
                else:
                    gaze_dc += wrap180(d_raw - gaze_dc)
                yaw = base + gaze_dc * w
                # 修法二（构造性兜底，顺带压平压线族）：yaw 逐帧限速。上限取
                # YAW_SLEW_CAP_DEGPS(85) < QC 硬帽 90，正常运镜（轨道 ω≈2-3°/s +
                # 游走 ≤8°/s + 混合过渡 ≤~40°/s）远不触及，只在异常时介入；
                # 限速造成的短暂滞后由目标连续性自然追平。
                if prev_yaw is not None:
                    dy = wrap180(yaw - prev_yaw)
                    max_step = YAW_SLEW_CAP_DEGPS / FPS
                    if dy > max_step:
                        yaw = prev_yaw + max_step
                    elif dy < -max_step:
                        yaw = prev_yaw - max_step
                prev_yaw = yaw
                elev = (look_elev + wp_) + (rp - (look_elev + wp_)) * w
                elev = clamp(elev, -75.0, 75.0)
                pos.append(cp)
                yaw_n = wrap180(yaw)   # 四元数对 ±360 不敏感；欧拉导出归一到 ±180
                quats.append(quat_from_yaw_elev_roll(yaw_n, elev, 0.0))
                eulers.append(euler_tuple(yaw_n, elev, 0.0))
            speeds = [(0.0, 0.0, 0.0)]
            for i in range(1, n_frames):
                d = vsub(pos[i], pos[i - 1])
                speeds.append((d[0] * FPS, d[1] * FPS, d[2] * FPS))
            moves = ([DirectorMove(cls="mounted_" + s.style, t0=s.t0, t1=s.t1, seg=k)
                      for k, s in enumerate(segs)]
                     or [DirectorMove(cls="mounted_focus", t0=0.0, t1=duration_s, seg=0)])
            tr = Track(slot + 1, focals3[slot], aspect, moves,
                       [(0.0, 0.0)], Pose(pos[0], 0.0, 0.0, 0.0))
            tr.positions = pos
            tr.quats = quats
            tr.eulers = eulers
            tr.speeds = speeds
            tr.follow_offset = MOUNT_OFFSET
            tr.player_positions = list(car.positions)
            tr.player_quats = [quat_from_yaw_elev_roll(y, 0.0, 0.0) for y in car.yaws]
            tr.player_eulers = [euler_tuple(y, 0.0, 0.0) for y in car.yaws]
            tracks.append(tr)

        # ---- cam4：同路径异速率（guide 挂载路径平滑复制 + 横向偏置）----
        # 需求方（张阔 0714 评论）确认语义：同条场景轨迹上按时间轴错位采
        # 快进/慢放版（看到其他三路的未来/过去）；统览圆及其站位搜索/
        # 框三车断言随之废弃。渲染侧同步：载机车体四路一律不可见
        # （CarrierVehicleSpawner——同路尾随拍到前车即相机拍到相机）。
        #
        # guide 改选**实测相机速度 p95 最慢**的载机：v₄=ρ·v_g 的传导预算下
        # 只有慢 guide 才给快档 ρ∈[1.65,3] 留出空间（配合下面的实测反推，
        # 一并修掉 S1-5「rho_allowed 用 SPEED_BANDS/V_MAX 死常量 ⇒ 快档
        # 恒不可达」的缺陷）。
        # guide_slot 已在载机排窗前预选（gentle 档联动，见 pink 块注释）
        cam_p95 = [met.percentile([vnorm(s) for s in tracks[k].speeds], 95)
                   for k in range(3)]
        guide_tr = tracks[guide_slot]
        # guide 速率取**前向差分对齐**：QC 的 ρ 反算用 |pos[i+1]−pos[i]|/dt，
        # 而 tr.speeds[i] 是后向差分。粉色窗的交接斜坡下一帧错位会让 ρ=v₄/v_g
        # 冒出假变速事件（实测 7-8 次）。
        gv = [vnorm(guide_tr.speeds[min(i + 1, n_frames - 1)])
              for i in range(n_frames)]
        # 传导预算按实测反推（修 S1-5 死常量）+ 逐候选**经验校验**：
        # p95 走表 2 的 6 m/s、瞬时走 §3.5 的 10 m/s，全部按候选 ρ(t)·v_g(t)
        # 实测序列判——混合分布下快档段占比 <100%，闭式上界会把可行快档误杀。
        # 不走 cam4mod.sample_plan：其慢慢档可检出域是角点（0.33-0.35 vs
        # 0.58-0.6），60 次内部重抽常落进「兜底=拉到带两端」，把快档硬塞回来，
        # 与经验校验互锁成 30 连拒（实测 cam4_plan_budget 卡死）。
        gv_max = max(gv)
        # 快档上限三卡：p95 传导 / 瞬时传导 / jerk 传导（曲率项 ∝ρ³、速度项 ∝ρ，
        # guide jerk ~7-9 时 ρ₂>2 结构性顶穿 20 的门——2.0 封顶）
        fast_hi = min(1.85,
                      met.SPEED_P95_MAX / max(0.3, met.percentile(gv, 85)),
                      cam4mod.V4_CAP * 0.92 / max(0.3, gv_max))

        def _c4_feasible(cand) -> bool:
            v4p_ = [cand.rho(i / FPS) * gv[i] for i in range(n_frames)]
            return (met.percentile(v4p_, 95) <= met.SPEED_P95_MAX * 0.97
                    and max(v4p_) <= cam4mod.V4_CAP * 0.95)

        c4plan = None
        want_fast = rng.random() < 0.75      # 快档偏置：40 种子里两档都要出现
        for _c4try in range(40):
            use_fast = want_fast and fast_hi >= 1.65
            if use_fast:
                pr = (rng.uniform(0.33, 0.6), rng.uniform(1.65, fast_hi))
                delta = rng.uniform(*cam4mod.DELTA_RANGE)
            else:
                # 慢慢档在 QC 可检出域内**构造式**取样：域是 δ<1.573 的窄角
                # （log(0.6/0.33)=0.598 对 0.38·δ 的余量），随机撒点命中率 ~5%
                # （实测 40 连拒 3/8 attempt），必须先抽 δ 再按余量摆两端
                delta = rng.uniform(1.5, 1.565)
                span = math.exp(0.38 * delta * 1.01)
                r_lo = rng.uniform(0.33, 0.6 / span - 0.001)
                pr = (r_lo, rng.uniform(r_lo * span + 0.001, 0.6))
            if rng.random() < 0.5:
                pr = (pr[1], pr[0])
            if not met.rho_detectable(pr[0], pr[1], delta):
                continue
            t_sw = rng.uniform(cam4mod.TSW_FRAC[0] * duration_s,
                               cam4mod.TSW_FRAC[1] * duration_s)
            cand = cam4mod.Cam4Plan(guide_slot + 1, pr[0], pr[1], t_sw, delta)
            if _c4_feasible(cand):
                c4plan = cand
                break
            want_fast = want_fast and rng.random() < 0.6   # 快档递减坚持
        if c4plan is None:
            gvi = max(range(n_frames), key=lambda k: gv[k])
            report["attempts"].append({
                "attempt": attempt,
                "bad": ["cam4_plan_budget(gv_p95=%.2f,gv_max=%.2f@%.1fs)"
                        % (met.percentile(gv, 95), gv[gvi], gvi / FPS)]})
            last_fail = "cam4_plan_budget"
            continue

        # ---- guide 路径的平滑复制 + 横向/垂直偏置（外侧，背离轨道中心）----
        # 「平滑」必须在**弧长域**做：guide 的粉色交接段车速低（β 地板 ~0.4 m/s），
        # 时间域滤波在那里的空间支撑 σ≈0.4s·0.4≈0.16m——窗角几乎原样保留，
        # cam4 快档以 4-5 m/s 撞上 r<1m 的空间角，jerk 打到数千（实测 top 帧
        # 全部落在 guide 低速段对应的弧位）。改为均匀弧长重采样（0.25m）后
        # 盒滤（空间 σ≈2.6m，处处一致），转角半径被圆到 ≥3m 量级。
        def _box3(series, win: int, passes: int = 3):
            """三重盒滤（前缀和 O(n)）。端点用收缩窗（clamp 平均）。"""
            half = win // 2
            out = list(series)
            for _ in range(passes):
                n_ = len(out)
                pre = [(0.0, 0.0, 0.0)]
                for p_ in out:
                    q_ = pre[-1]
                    pre.append((q_[0] + p_[0], q_[1] + p_[1], q_[2] + p_[2]))
                nxt = []
                for i_ in range(n_):
                    a_ = max(0, i_ - half)
                    b_ = min(n_, i_ + half + 1)
                    m_ = b_ - a_
                    nxt.append(((pre[b_][0] - pre[a_][0]) / m_,
                                (pre[b_][1] - pre[a_][1]) / m_,
                                (pre[b_][2] - pre[a_][2]) / m_))
                out = nxt
            return out

        q_lat = rng.uniform(4.0, 7.0)
        h_off = rng.uniform(1.5, 3.0)
        band_fast = max(c4plan.rho1, c4plan.rho2) >= 1.5
        # 路径蓝本 = guide 的**基线巡游**（无粉色窗的轨道 + 挂载），不是含窗实录：
        # guide 的 left/right 窗在其实录路径上留下发卡弯（反平行轴 vs 残速 ⇒
        # 速度过零、方向反转），guide 自己过弯时 gv 同步塌到 ~0，但 v₄=ρ·gv 按
        # **cam4 的墙钟**取 gv——cam4 到达发卡弯的墙钟处 gv 正快，全速撞弯，
        # jerk 数百（实测 top 帧全落在 guide 窗对应弧位）。基线巡游曲率 ~1/65，
        # 快档回放安全；cam4 的粉色由它自己的叠加窗完成（与载机同构），
        # v₄≡ρ·v_g 的时间轴错位语义保持不变。
        guide_veh = carriers[guide_slot]
        _ga = guide_veh.orbit_args
        _clean = integrate_orbit(_ga["center"], _ga["duration"], _ga["n_frames"],
                                 _ga["r0"], _ga["h0"], _ga["az0"],
                                 _ga["omega_series"], r_wobble=_ga["r_wobble"],
                                 r_period=_ga["r_period"],
                                 h_period=_ga["h_period"])
        raw_all = [mount_world(p, y) for p, y in zip(_clean.positions,
                                                     _clean.yaws)]
        # 单调推进剪枝：渐开斜坡+振荡可让 ω 短暂过零（「短暂倒车」，docs/78
        # §10.7），路径蓝本里那就是一段发卡折返——按弧长重采样会把它折成回环，
        # B 样条在回环处收缩，|dP/ds| 掉到 0.6-0.8（实测 v₄ 单帧掉速 ⇒ jerk
        # 千级尖峰）。蓝本只保留「向前推进」的子序列（倒车段坍缩成一点）。
        raw_gp: List[Vec3] = [raw_all[0]]
        _dir = None
        for p3 in raw_all[1:]:
            d3 = vsub(p3, raw_gp[-1])
            dl = vnorm(d3)
            if dl < 0.04:
                continue
            if _dir is not None and (d3[0] * _dir[0] + d3[1] * _dir[1]
                                     + d3[2] * _dir[2]) < 0.0:
                continue
            _dir = (d3[0] / dl, d3[1] / dl, d3[2] / dl)
            raw_gp.append(p3)
        if len(raw_gp) < 32:
            raw_gp = raw_all
        cum_s = [0.0]
        for a3, b3 in zip(raw_gp, raw_gp[1:]):
            cum_s.append(cum_s[-1] + vdist(a3, b3))
        L_g = max(1.0, cum_s[-1])
        # 重采样步长 0.06m：折线段间的方向微折 κ·Δs 在快档回放下按 v·折角·fps²
        # 变成 jerk（Δs=0.25m 实测持续 28-50——p95 的真正推手；经典模式同构但
        # 标称速度只有 1-2 m/s 才没暴露）。窗宽同步 ×~4 保持空间 σ≈2.6m。
        n_u = max(256, int(L_g / 0.06))
        u_pts: List[Vec3] = []
        idxs = 0
        for k in range(n_u + 1):
            tgt = L_g * k / n_u
            while idxs < len(cum_s) - 2 and cum_s[idxs + 1] < tgt:
                idxs += 1
            den = cum_s[idxs + 1] - cum_s[idxs]
            w_ = 0.0 if den <= 1e-9 else (tgt - cum_s[idxs]) / den
            u_pts.append(tuple(raw_gp[idxs][j] + (raw_gp[idxs + 1][j]
                               - raw_gp[idxs][j]) * w_ for j in range(3)))
        # 快档会把弧长吃到 guide 长度之上：**先**沿轨道中心圆周与末端相切续走
        # （0.06m 同步距——粗步距折线在快档下就是 jerk 冲击源），**再**整体盒滤
        # （接缝一并被圆滑）。
        need4 = cam4mod.required_length(c4plan, gv, n_frames, FPS)
        if need4 * 1.10 > L_g:
            pe = u_pts[-1]
            rex = max(6.0, math.hypot(pe[0] - best_c[0], pe[2] - best_c[2]))
            aze = math.atan2(pe[2] - best_c[2], pe[0] - best_c[0])
            pprev = u_pts[-2]
            crossy = ((pe[0] - best_c[0]) * (pe[2] - pprev[2])
                      - (pe[2] - best_c[2]) * (pe[0] - pprev[0]))
            sgn4 = 1.0 if crossy >= 0.0 else -1.0
            extra = need4 * 1.10 - L_g
            n_ext = max(32, int(extra / 0.06))
            for k in range(1, n_ext + 1):
                az_k = aze + sgn4 * (extra * k / n_ext) / rex
                u_pts.append((best_c[0] + rex * math.cos(az_k), pe[1],
                              best_c[2] + rex * math.sin(az_k)))
        u_pts = _box3(u_pts, win=87)
        # 朝向**不复刻 guide 的凝视**：guide 低速爬行时的自由观察扫视在弧长域里
        # 密到 >100°/m，cam4 以异速率穿过时角速率直接打穿（限幅后 angjerk p95
        # 实测 648-4910，门 600）。cam4 全程 look-at 注视区（与载机的
        # focus-lock 同源、粉色窗只动位置的裁决同构），姿态列不入路径。
        poses4: List[Tuple[Vec3, float, float, float]] = []
        nx_p, nz_p = None, None
        for k in range(len(u_pts)):
            a3 = u_pts[max(0, k - 1)]
            b3 = u_pts[min(len(u_pts) - 1, k + 1)]
            tx, tz = b3[0] - a3[0], b3[2] - a3[2]
            tl = math.hypot(tx, tz)
            if tl > 1e-4:
                nx, nz = tz / tl, -tx / tl
                if ((u_pts[k][0] - best_c[0]) * nx
                        + (u_pts[k][2] - best_c[2]) * nz) < 0.0:
                    nx, nz = -nx, -nz
                nx_p, nz_p = nx, nz
            elif nx_p is None:
                rel = (u_pts[k][0] - best_c[0], u_pts[k][2] - best_c[2])
                rl2 = math.hypot(rel[0], rel[1]) or 1.0
                nx_p, nz_p = rel[0] / rl2, rel[1] / rl2
            poses4.append(((u_pts[k][0] + nx_p * q_lat, u_pts[k][1] + h_off,
                            u_pts[k][2] + nz_p * q_lat), 0.0, 0.0, 0.0))

        arc4 = cam4mod.ArcPath.from_poses(poses4)
        if arc4.length < need4 * 1.02:
            report["attempts"].append({"attempt": attempt,
                                       "bad": ["cam4_path_short(%.0f<%.0f)"
                                               % (arc4.length, need4)]})
            last_fail = "cam4_path_short"
            continue

        # ---- 粉色窗（保速换向，Cam4Driver 原样复用）----
        v4_pred = [min(c4plan.rho(i / FPS) * gv[i], cam4mod.V4_CAP * 0.95)
                   for i in range(n_frames)]
        # 排窗几何回调：候选 t0 → 预计弧位处「路径切向 vs 声称轴」的实际摆角。
        # 同路径模式下相机 yaw（look-at 注视区）与路径切向的夹角随弧位漂 ±30°+，
        # 按类估计表会把窗排短，斜坡被 t_fit 压扁（实测 jerk 29+/share 0.45）。
        # s4 预积分忽略窗内冻结（后窗估计偏前 ≤~30m ⇒ 摆角误差 ~25°，由
        # theta_fn 调用侧 +0.35 rad 松量覆盖）。
        s4_pref = [0.0]
        for i in range(n_frames):
            s4_pref.append(s4_pref[-1] + v4_pred[i] * dt)

        def _theta4(cls_: str, t0_: float) -> float:
            i0_ = min(n_frames - 1, max(0, int(t0_ * FPS)))
            s_ = min(arc4.length - 0.5, s4_pref[i0_])
            ap_ = arc4.at(s_)[0]
            ah_ = arc4.at(min(arc4.length, s_ + 0.4))[0]
            tv_ = vsub(ah_, ap_)
            tn_ = vnorm(tv_) or 1.0
            ly_, _le_ = yaw_elev_from_forward(vsub(focus.at(t0_), ap_))
            ax_ = pinkmod.axis_of(cls_, ly_)
            dot_ = (ax_[0] * tv_[0] + ax_[1] * tv_[1] + ax_[2] * tv_[2]) / tn_
            return math.acos(clamp(dot_, -1.0, 1.0))

        if os.environ.get("PINK4_OFF"):        # 诊断隔离用：只走弧长回放
            pink4_wins = []
        try:
            pink4_wins = pink4_wins if os.environ.get("PINK4_OFF") else \
                pinkmod.schedule_cam4_windows(rng, duration_s, v4_pred, FPS,
                                              theta_fn=_theta4)
        except RuntimeError as e:
            report["attempts"].append({"attempt": attempt,
                                       "bad": ["pink.schedule4:%s" % e]})
            last_fail = "pink.schedule4"
            continue

        # ---- 弧长推进积分：s₄(t)=∫ρ·v_g（窗内显式换向，窗尾把位移折进恒定
        # 偏置 off4「沿弧长续走」——s₄ 在窗内冻结，位置连续、|v₄| 逐帧恒等）----
        drv4 = pinkmod.Cam4Driver(pink4_wins, FPS)
        s4 = 0.0
        off4 = (0.0, 0.0, 0.0)
        pos4: List[Vec3] = []
        yaw4s: List[float] = []
        elev4s: List[float] = []
        exhausted4 = False
        p_cur = vadd(arc4.at(0.0)[0], off4)
        for i in range(n_frames):
            t = i / FPS
            ap = arc4.at(s4)[0]
            pos4.append(p_cur)
            # 朝向：look-at 注视区（10s 低通的 focus Subject，本就平滑）
            lyaw, lelev = yaw_elev_from_forward(vsub(focus.at(t), p_cur))
            yaw4s.append(lyaw)
            elev4s.append(clamp(lelev, -75.0, 75.0))
            # 中点 ρ（与经典 build_frames 同式）；无地板（QC 对 vg≤0.05 置 None）
            v4_want = min(c4plan.rho(t + 0.5 * dt) * gv[i],
                          cam4mod.V4_CAP * 0.95)
            ahead = arc4.at(min(arc4.length, s4 + 0.05))[0]
            tv = vsub(ahead, ap)
            tn = vnorm(tv) or 1.0
            tang4 = (tv[0] / tn, tv[1] / tn, tv[2] / tn)
            # 窗轴基取当帧 look-at yaw（与最终 quat 同式 ⇒ 预测即实测）
            d4 = drv4.direction(t, tang4, v4_want, lyaw)
            if d4 is None:
                s_next = s4 + v4_want * dt
                if s_next > arc4.length:
                    exhausted4 = True
                    s_next = arc4.length
                s4 = s_next
                p_cur = vadd(arc4.at(s4)[0], off4)
            else:
                p_cur = vadd(p_cur, vscale(d4, v4_want * dt))
                if drv4.absorb_now:
                    off4 = vsub(p_cur, arc4.at(s4)[0])
        if exhausted4:
            report["attempts"].append({"attempt": attempt,
                                       "bad": ["cam4_path_exhausted"]})
            last_fail = "cam4_path_exhausted"
            continue
        # 朝向限幅兜底（look-at 平台角速率 ~7°/s，正常不触发）
        yaw4s, elev4s, roll4s = cam4mod.limit_angular_rate(
            yaw4s, elev4s, [0.0] * n_frames, cam4mod.V4_OMEGA_CAP * dt)
        quats4 = [quat_from_yaw_elev_roll(y, e, r)
                  for y, e, r in zip(yaw4s, elev4s, roll4s)]
        eulers4 = [euler_tuple(y, e, r)
                   for y, e, r in zip(yaw4s, elev4s, roll4s)]
        speeds4 = [(0.0, 0.0, 0.0)]
        for i in range(1, n_frames):
            d = vsub(pos4[i], pos4[i - 1])
            speeds4.append((d[0] * FPS, d[1] * FPS, d[2] * FPS))
        tr4 = Track(4, focals3[0], aspect,
                    [DirectorMove(cls="observer_rho", t0=0.0, t1=duration_s, seg=0)],
                    [(0.0, 0.0)], Pose(pos4[0], 0.0, 0.0, 0.0))
        tr4.positions = pos4
        tr4.quats = quats4
        tr4.eulers = eulers4
        tr4.speeds = speeds4
        tracks.append(tr4)

        # ---- 自检（与 QC 同口径） ----
        plan = ClipPlan(seed, scene, tracks, c4plan, aspect,
                        Anchor(focus, 3.0), [], duration_s=duration_s)
        bad: List[str] = []
        for tr in tracks:
            ph = met.physics(tr.positions, tr.quats, dt)
            plan.physics[tr.cam_id] = ph
            v = ph.violations(is_cam4=(tr.cam_id == 4))
            if v:
                bad.append("phys.cam%d(%s)" % (tr.cam_id, ",".join(v)))
                if tr.cam_id == 4 and os.environ.get("DIRECTOR_DUMP_CAM4"):
                    vv_ = [vsub(b_, a_) for a_, b_ in zip(tr.positions,
                                                          tr.positions[1:])]
                    aa_ = [vsub(b_, a_) for a_, b_ in zip(vv_, vv_[1:])]
                    jj_ = [vnorm(vsub(b_, a_)) * FPS ** 3
                           for a_, b_ in zip(aa_, aa_[1:])]
                    top = sorted(range(len(jj_)), key=lambda k: -jj_[k])[:40]
                    print("[dump] cam4 jerk top frames:",
                          [(round(k / FPS, 2), round(jj_[k], 1)) for k in
                           sorted(top)][:20])
                    # 同帧的 gv 邻域与 |v4| 邻域：定位 jerk 是速度剖面还是几何
                    for k in sorted(top)[:6]:
                        print("[dump]   t=%.2f gv=%s v4=%s rho=%.2f" % (
                            k / FPS,
                            [round(gv[m], 3) for m in range(max(0, k - 1), k + 3)],
                            [round(vnorm(vv_[m]) * FPS, 3)
                             for m in range(max(0, k - 1), min(len(vv_), k + 3))],
                            c4plan.rho(k / FPS)))
                    print("[dump] cam4 windows:",
                          [(w.cls, round(w.t0, 1),
                            round(w.t1_actual or w.t1, 1))
                           for w in pink4_wins])
                    print("[dump] rho:", c4plan.to_dict())
            # 角速度瞬时硬帽——与 QC `phys.angspeed_max`(90°/s) 同数同口径。
            # metrics 全局裁决（近天顶只记录不设闸）针对经典模式；导演模式
            # 仰角远离极区、无豁免场景，生成侧必须与 QC 给同一个判定，
            # 否则「自检全绿、QC 打回」（本闸 + YAW_SLEW_CAP_DEGPS 双保险）。
            w_inst = max((quat_angle_deg(tr.quats[k], tr.quats[k + 1]) / dt
                          for k in range(len(tr.quats) - 1)), default=0.0)
            if w_inst > 90.0:
                bad.append("angspeed.cam%d(inst=%.1f)" % (tr.cam_id, w_inst))
        pose_map = {tr.cam_id: (tr.positions, tr.quats) for tr in tracks}
        plan.decouple = met.decouple(pose_map, dt)
        if not plan.decouple.ok():
            kk, ff = plan.decouple.worst_pair
            bad.append("decouple(pair %s=%.0f%%, all4=%.0f%%)"
                       % (kk, ff * 100, plan.decouple.all4_frac * 100))
        # 初始帧闸门**四路全判**（豁免撤销）：cam4 改同路径异速率后，起点 =
        # guide 起点 + 横向偏置（≤7m）+ 垂直偏置（≤3m），朝向 = guide 起点朝向的
        # 复制——初始帧一致性天然成立，统览位时代的 cam4 豁免（🟨 送审说明项）
        # 不再需要。QC 侧对应：check-clip 不再传 --init-exempt-lanes 4。
        plan.init_angle, plan.init_dist = met.initial_frame(pose_map)
        if plan.init_angle > 15.0:
            bad.append("init.angle=%.1f°" % plan.init_angle)
        if plan.init_dist > 30.0:
            bad.append("init.dist=%.1fm" % plan.init_dist)
        plan.cam4_check = met.cam4_check(pos4, guide_tr.positions, dt)
        probs = plan.cam4_check.problems(c4plan.t_sw, c4plan.delta)
        if probs:
            runs = []
            r0i = None
            for i in range(n_frames):
                low = gv[i] < 0.15
                if low and r0i is None:
                    r0i = i
                elif not low and r0i is not None:
                    runs.append((round(r0i / FPS, 2), round((i - r0i) / FPS, 2)))
                    r0i = None
            bad.append("cam4(%s;events=%s;t_sw=%.1f;gv_low=%s)"
                       % (",".join(probs),
                          [round(e, 1) for e in plan.cam4_check.events_s],
                          c4plan.t_sw, runs[:6]))

        # ---- 粉色诚实性自检（契约与 QC 侧逐字一致）+ 段标注 ----
        # 输入=最终相机轨迹（与 QC 同源）：窗起帧相机 yaw 定基（primitives._basis
        # 同式），声称轴向净位移 ≥0.8m 且 ≥50% 窗内路径长；任一窗不达标该 attempt 作废
        pink_all = pink_wins_car + [pink4_wins]
        pink_rows_by_cam: List[List[Dict]] = []
        pink_fail: List[str] = []
        for ci, tr in enumerate(tracks):
            rows = pinkmod.selfcheck(tr.positions, tr.quats, pink_all[ci], FPS)
            pink_rows_by_cam.append(rows)
            missing = set(pinkmod.PINK_CLASSES) - {r["cls"] for r in rows}
            if missing:
                pink_fail.append("cam%d缺%s" % (tr.cam_id, ",".join(sorted(missing))))
            for r in rows:
                if not r["ok"]:
                    pink_fail.append(
                        "cam%d.%s@%.1fs(net=%.2fm,share=%.2f)"
                        % (tr.cam_id, r["cls"], r["t0"],
                           r["net_axial_m"], r["share"]))
            tr.moves = sorted(
                tr.moves + pinkmod.build_moves(pink_all[ci], rows, len(tr.moves)),
                key=lambda m: m.t0)
        if pink_fail:
            bad.append("pink.honesty(%s)" % ";".join(pink_fail[:4]))
            pink_fail_rows = [dict(r, cam=tracks[ci].cam_id)
                              for ci, rows in enumerate(pink_rows_by_cam)
                              for r in rows if not r["ok"]]
        else:
            pink_fail_rows = None

        # 红线 0：运动常态（用户口径）：单次静止 ≤2s；三车合计静止 ≤5%；永不同时静止
        motion_stats = {}
        static_flags_all = []
        total_static = 0
        for c in carriers:
            flags = [v < 0.2 for v in c.speeds]
            static_flags_all.append(flags)
            over, worst = runs_over(flags, dt, 2.0)
            frac = sum(flags) / n_frames
            total_static += sum(flags)
            motion_stats["carrier_%d" % c.vid] = {
                "static_frac": round(frac, 4),
                "max_static_run_s": round(worst, 2),
                "runs_over_2s": over,
            }
            if over:
                bad.append("motion.static_run(cam%d=%.1fs)" % (c.vid, worst))
        combined_frac = total_static / (3.0 * n_frames)
        all3_static = sum(1 for i in range(n_frames)
                          if all(f[i] for f in static_flags_all))
        motion_stats["combined_static_frac"] = round(combined_frac, 4)
        motion_stats["all3_static_frames"] = all3_static
        if combined_frac > 0.05:
            bad.append("motion.combined_static=%.1f%%" % (combined_frac * 100))
        if all3_static:
            bad.append("motion.all3_static=%d帧" % all3_static)

        # 红线 1：穿模（相机四路 10Hz clearance>0；受控车不入占据体素）
        clear_min = 1e9
        clear_by_cam = {}
        for tr in tracks:
            cmin, cargmin = 1e9, -1
            for i in range(0, n_frames, 3):
                c = scene.clearance(tr.positions[i])
                if c < cmin:
                    cmin, cargmin = c, i
            clear_by_cam["cam%d" % tr.cam_id] = (round(cmin, 2),
                                                 round(cargmin / FPS, 1))
            clear_min = min(clear_min, cmin)
        if clear_min <= 0.0:
            bad.append("clearance=%.2fm(%s)" % (
                clear_min,
                ",".join("%s@%ss" % (k, v[1]) for k, v in clear_by_cam.items()
                         if v[0] <= 0.0)))
        veh_occupied = sum(1 for vch in vehicles for i in range(0, n_frames, 15)
                           if scene.occupied(vch.positions[i]))

        # 红线 2：look-at 单次遮挡 ≤2s（10Hz 采样）
        occl_stats = {}
        for tr in tracks[:3]:
            flags = []
            for i in range(0, n_frames, 3):
                flags.append(scene.blocked(tr.positions[i], focus.at(i / FPS)))
            over, worst = runs_over(flags, 0.1, OCCLUSION_RUN_MAX_S)
            occl_stats["cam%d" % tr.cam_id] = {"max_run_s": round(worst, 2),
                                               "runs_over_2s": over}
            if over:
                bad.append("occlusion.cam%d(max=%.1fs)" % (tr.cam_id, worst))

        # cam4 ρ 验形强化（同路径异速率后 guide 对照才真正有意义）：除
        # met.cam4_check 的「恰 1 次/时刻/禁带」外，再核**平台值**——切换前后
        # 稳态区的实测中值 ρ 必须落在申报值 ±12% 内（标签 ≠ 事实的另一半）
        rho_meas = {}
        for tag, t0r, t1r, rho_decl in (
                ("rho1", 2.0, c4plan.t_sw - 1.5, c4plan.rho1),
                ("rho2", c4plan.t_sw + c4plan.delta + 1.5, duration_s - 1.5,
                 c4plan.rho2)):
            vals = []
            for i in range(int(t0r * FPS), min(n_frames - 1, int(t1r * FPS))):
                if gv[i] > 0.15:
                    vals.append(vdist(pos4[i + 1], pos4[i]) * FPS / gv[i])
            if vals:
                vals.sort()
                med = vals[len(vals) // 2]
                rho_meas[tag] = round(med, 4)
                if abs(med - rho_decl) > 0.12 * rho_decl:
                    bad.append("cam4.rho_plateau(%s: 申报%.2f 实测%.2f)"
                               % (tag, rho_decl, med))

        # 共视（严口径；贵，放最后）
        cam4_covis_obs = None
        if not bad:
            times = covismod.sample_times(duration_s, covis_hz)
            views = {}
            for tr in tracks[:3]:
                it = plan.intrinsics(tr.focal_mm)
                views[tr.cam_id] = [
                    covismod.CamView(tr.positions[min(int(t * FPS), n_frames - 1)],
                                     tr.quats[min(int(t * FPS), n_frames - 1)],
                                     it["fx"], it["fy"], it["cx"], it["cy"],
                                     w_px, h_px) for t in times]
            plan.covis = covismod.evaluate(scene, views, times)
            if plan.covis.ratio < covismod.COVIS_RATIO_MIN:
                bad.append("covis=%.0f%%" % (plan.covis.ratio * 100))
                if os.environ.get("DIRECTOR_DEBUG"):
                    import statistics as _st
                    dbg = {}
                    for pk, curve in plan.covis.pair_ts.items():
                        vals = list(curve)
                        dbg[pk] = {"mean": round(_st.mean(vals), 3),
                                   "min": round(min(vals), 3),
                                   "frac_ge_030": round(sum(1 for v in vals if v >= 0.30) / len(vals), 3)}
                    _dbg_covis = dbg
            else:
                # cam4 与前三路的重叠**观测**（需求「尽量不共视」——不作闸门）：
                # 时间轴错位天然拉开视野，报数字随批送审。0.5Hz 降采样控成本。
                times4 = covismod.sample_times(duration_s, 0.5)
                it4 = plan.intrinsics(tr4.focal_mm)
                v4views = [covismod.CamView(
                    pos4[min(int(t * FPS), n_frames - 1)],
                    quats4[min(int(t * FPS), n_frames - 1)],
                    it4["fx"], it4["fy"], it4["cx"], it4["cy"],
                    w_px, h_px) for t in times4]
                cam4_covis_obs = {}
                for tr in tracks[:3]:
                    it = plan.intrinsics(tr.focal_mm)
                    vv = {tr.cam_id: [covismod.CamView(
                        tr.positions[min(int(t * FPS), n_frames - 1)],
                        tr.quats[min(int(t * FPS), n_frames - 1)],
                        it["fx"], it["fy"], it["cx"], it["cy"],
                        w_px, h_px) for t in times4], 4: v4views}
                    r_pair = covismod.evaluate(scene, vv, times4)
                    curve = r_pair.pair_ts.get("%d-4" % tr.cam_id, [])
                    cam4_covis_obs["cam%d-4" % tr.cam_id] = {
                        "mean": round(sum(curve) / max(1, len(curve)), 4),
                        "frac_ge_030": round(sum(1 for x in curve if x >= 0.30)
                                             / max(1, len(curve)), 4),
                    }

        rec = {
            "attempt": attempt,
            "bad": list(bad),
            "covis_ratio": round(plan.covis.ratio, 4) if plan.covis else None,
        }
        if pink_fail_rows:
            rec["pink_fail_rows"] = pink_fail_rows
        try:
            rec["covis_pairs"] = _dbg_covis
            del _dbg_covis
        except NameError:
            pass
        report["attempts"].append(rec)

        if bad:
            last_fail = "; ".join(bad)
            continue

        # ---- 相互可见性（观测指标，10Hz） ----
        mutual = {}
        for tr in tracks[:3]:
            it = plan.intrinsics(tr.focal_mm)
            cnt = {}
            me = carriers[tr.cam_id - 1]
            for other in carriers:
                if other is me:
                    continue
                vis = 0
                total = 0
                for i in range(0, n_frames, 3):
                    total += 1
                    cam = covismod.CamView(tr.positions[i], tr.quats[i],
                                           it["fx"], it["fy"], it["cx"], it["cy"],
                                           w_px, h_px)
                    if covismod.sees(scene, cam, other.positions[i]):
                        vis += 1
                cnt["sees_carrier_%d" % other.vid] = round(vis / max(1, total), 3)
            mutual["cam%d" % tr.cam_id] = cnt

        # ---- 落盘 ----
        out = Path(out_dir)
        out.mkdir(parents=True, exist_ok=True)
        plan.ok = True
        write_clip(plan, out)
        plan_path = out / "plan.json"
        pj = json.loads(plan_path.read_text(encoding="utf-8"))
        pj["director"] = {
            "mode": "vehicle-director",
            "mount_offset": list(MOUNT_OFFSET),
            "guide_cam": guide_slot + 1,
            "gaze": [[{"style": s.style, "t0": round(s.t0, 2), "t1": round(s.t1, 2),
                       "rel_yaw": round(s.rel_yaw, 1), "rel_pitch": round(s.rel_pitch, 1)}
                      for s in segs] for segs in gaze_all],
            "tsw_reading": ("恰 1 次大变速（t_sw=U(d/3,2d/3) 比例缩放）；"
                            "『每分钟 1 次 ⇒ 300s 5 次』读法未采用，docs/78 §10 待裁决"),
            "pink": {
                "impl": "overlay（不占非粉色段；载机=冻结叠加+参数吸收，cam4=保速换向）",
                "contract": "净轴向位移≥0.8m 且 ≥50% 窗内路径；基=窗起帧相机 yaw"
                            "（primitives._basis 同式）",
                "orientation_owner": "gaze/focus-lock（窗只动位置，允许时间重叠）",
            },
        }
        plan_path.write_text(json.dumps(pj, ensure_ascii=False, separators=(",", ":")),
                             encoding="utf-8")

        # ---- 粉色逐窗自检落盘 + 打印（生成侧自带同规格自检的交付要求）----
        _flat_rows = [dict(r, cam=tracks[ci].cam_id)
                      for ci, rows in enumerate(pink_rows_by_cam) for r in rows]
        pink_sc = {
            "contract": "net_axial>=0.8m AND net_axial>=0.5*path_len; "
                        "basis=camera yaw @ window start frame (primitives._basis)",
            "n_windows": len(_flat_rows),
            "n_pass": sum(1 for r in _flat_rows if r["ok"]),
            "min_share": min(r["share"] for r in _flat_rows),
            "min_net_m": min(r["net_axial_m"] for r in _flat_rows),
            "per_cam": {"cam%d" % tracks[ci].cam_id: rows
                        for ci, rows in enumerate(pink_rows_by_cam)},
            "carrier_absorb_log": pink_absorbs,
        }
        (out / "pink_selfcheck.json").write_text(
            json.dumps(pink_sc, ensure_ascii=False, indent=1), encoding="utf-8")
        print("pink 逐窗自检（净轴向≥0.8m 且 ≥50%路径；基=窗起帧相机 yaw）：")
        for r in _flat_rows:
            print("  cam%d %-19s [%6.2f,%6.2f]s yaw=%8.2f° net=%6.2fm "
                  "path=%6.2fm share=%.3f %s"
                  % (r["cam"], r["cls"], r["t0"], r["t1"], r["basis_yaw_deg"],
                     r["net_axial_m"], r["path_m"], r["share"],
                     "PASS" if r["ok"] else "FAIL"))
        # v1.1：车体逐帧位姿落**生成器局部系**（与 action_camera_*.json 同系）。
        # v1.0 的字段叫 `frames_world` 且把 world_off 烘进了坐标，而 Unity 侧
        # CarrierVehicleSpawner 按「局部系」又加了一遍运行时偏移 ⇒ 车体被平移两次。
        # s18 实证：车体离本路相机 114.36m（该距离本应恒为 |mount_offset|=3.33m），
        # 统览画面里三辆载机全部不在场；而位姿断言只管相机、可见性自检只核
        # cullingMask、dyn.active_frac 被环境车流撑满 ⇒ 全链静默通过。
        # 字段随语义一并改名：旧读者碰上新文件立刻报缺字段，不会继续静默错位。
        vehicle_plan = {
            "version": "1.1", "seed": seed, "duration_s": duration_s, "fps": FPS,
            "frame_count": n_frames,
            "frames_frame": "generator-local",
            # 计划所依据的场景快照偏移（局部原点的世界坐标）。**不进坐标**，只作
            # 溯源元数据：真正的摆位偏移由采集侧运行时给（y 可能另行吸附走廊高度）。
            "world_offset": list(world_off),
            "corridor_road": cor.get("road"),
            "mount_offset": list(MOUNT_OFFSET),
            "vehicles": [{
                "id": vch.vid, "role": vch.role,
                "lane_offset_m": vch.lane, "y_offset_m": vch.y_off,
                "direction": vch.direction,
                "pauses": [[round(p0, 2), round(p1, 2)] for p0, p1 in vch.pauses],
                "frames_local": [
                    [round(vch.positions[i][0], 3),
                     round(vch.positions[i][1], 3),
                     round(vch.positions[i][2], 3),
                     round(vch.yaws[i], 2)]
                    for i in range(n_frames)],
            } for vch in vehicles],
        }
        (out / "vehicle_plan.json").write_text(
            json.dumps(vehicle_plan, ensure_ascii=False, separators=(",", ":")),
            encoding="utf-8")

        report.update({
            "ok": True,
            "attempt_used": attempt,
            "covis_ratio_strict": round(plan.covis.ratio, 4),
            "covis_hz": covis_hz,
            "aspect": aspect,
            "focals": [t.focal_mm for t in tracks],
            "guide_cam": guide_slot + 1,
            "cam4": c4plan.to_dict(),
            "cam4_path": {
                "mode": "same-path-rate-warp（guide 挂载路径平滑复制+横向偏置）",
                "q_lat_m": round(q_lat, 2), "h_off_m": round(h_off, 2),
                "arc_len_m": round(arc4.length, 1),
                "need_m": round(need4, 1),
                "rho_meas": rho_meas,
                "band_fast": bool(max(c4plan.rho1, c4plan.rho2) >= 1.5),
                "guide_cam_p95": [round(x, 3) for x in cam_p95],
            },
            "cam4_covis_observed": cam4_covis_obs,
            "occlusion": occl_stats,
            "motion": motion_stats,
            "mutual_visibility_frac": mutual,
            "min_clearance_m": round(clear_min, 2),
            "vehicle_occupied_samples": veh_occupied,
            "init_angle_deg": round(plan.init_angle, 2),
            "init_dist_m": round(plan.init_dist, 2),
            "decouple_worst_pair": plan.decouple.worst_pair[0],
            "decouple_worst_frac": round(plan.decouple.worst_pair[1], 3),
            "pink": {"n_windows": pink_sc["n_windows"],
                     "n_pass": pink_sc["n_pass"],
                     "min_share": pink_sc["min_share"],
                     "min_net_m": pink_sc["min_net_m"]},
        })
        (out / "director_report.json").write_text(
            json.dumps(report, ensure_ascii=False, indent=2), encoding="utf-8")
        return report

    report["ok"] = False
    out = Path(out_dir)
    out.mkdir(parents=True, exist_ok=True)
    (out / "director_report.json").write_text(
        json.dumps(report, ensure_ascii=False, indent=2), encoding="utf-8")
    raise RuntimeError("director 计划 %d 次尝试全部未过自检，最后失败：%s"
                       % (max_attempts, last_fail))

"""第四路形态 b：同区域高空垂直俯视伴飞（docs/69 §8.3，验收 demo mc2）。

构造口径——QC 的三条 `cam4.topdown_*` 判据（阈值见 metrics「第四路俯视伴飞」节，
与 `engine_qc.config` 逐字对齐）全部按**构造余量**保证，而不是靠事后重试：

- **垂直俯视**：elev = 偏角 − 90°，基准偏角 5–9° 加 ≤3° 慢波动，最大 12°，
  离 15° 规格恒有 ≥3° 余量（占比闸恒零超帧，30° 硬帽更远）。刻意不取正 90°：
  正垂直时 forward 的水平分量归零，`plan.pink_honesty` 的 yaw 基底
  （atan2(fx, fz)）在导出精度下方向随机，声称轴向就没法核了。
- **高空**：y = 前三路全程最高点 + 14–18 m 基准 + 缓慢竖直漂移，再对路线走廊内
  的障碍盒顶抬底、并按实测最小高差做确定性抬升——三重保证相对高差 ≥10 m
  的占比闸恒零超帧（QC 的高差是相对量，抬高恒安全）。
- **沿路线上空漂移**：XZ 取 guide 实测路径的**平滑脊线**（弧长域滑动平均 +
  ArcPath 弧长重参数化），progression 在粉色窗外往复推进——方向可反转、
  速度逐段随机，即「允许时间超前/滞后」；偏离路线的只有粉色脉冲 ≤1.7 m
  与脊线切角 ~2 m，相对 20/40 m 阈值一个量级的余量。平滑脊线同时压掉
  guide 慢速急弯处的曲率（异速穿过原始折线会把向心加速度放大）。
- **不追主体**：朝向只有慢 yaw 漂移（峰值 <5°/s，低于解耦的角判据门槛），
  不 look-at 锚点。

粉色平移（每条必含，`plan.pink_coverage` 对**每一路**判）以**脉冲窗**嵌入：
窗内路线推进、竖直漂移、yaw 全部冻结，脉冲沿「窗首帧 yaw 基底」的声称轴向
走 S₅ 到 1.1–1.7 m——净位移与轴向占比按构造满足 `plan.pink_honesty` 的
「≥0.8 m 且 ≥ 窗内路径长 50%」；窗后在下一个 gap 内衰减回路线（衰减在下一窗
开始前 0.5s 结束，不污染下一窗的声称）。
"""

import math
import random
from dataclasses import dataclass, replace
from typing import Dict, List, Optional, Sequence, Tuple

from .cam4 import ArcPath
from .mathutil import (Quat, Vec3, clamp, euler_tuple, quat_from_yaw_elev_roll,
                       smoothstep5, vdist)
from .primitives import PINK, Move, Pose
from .sampler import frame_speeds

#: 基准偏角与慢波动（deg）。上界 9+3=12，距 QC 规格 15° 恒有 3° 构造余量。
TILT_BASE_RANGE = (5.0, 9.0)
TILT_WOBBLE_RANGE = (1.0, 3.0)
TILT_PERIOD_RANGE = (14.0, 26.0)
#: 相对前三路全程最高点的基准高差（m）。QC 判的是**当帧**最高点，
#: 按全程最高点起算只会更宽。
ALT_BASE_RANGE = (14.0, 18.0)
#: 实测最小高差的确定性抬底（QC 门 10.0 + 余量）。
ALT_MARGIN_FLOOR = 10.6
#: 竖直漂移：每个 gap 的增量帽与绝对幅度帽（m）。
VDRIFT_STEP_MAX = 1.2
VDRIFT_ABS_MAX = 1.5
#: 粉色脉冲净位移（m）。下界 1.1 > 诚实性门 0.8，导出取整与半帧误差吃不掉。
PULSE_MAG_RANGE = (1.1, 1.7)
#: 粉色窗时长与窗间最小 gap（s）。gap ≥3 保证脉冲衰减（gap−0.5 ≥2.5s）足够缓
#: （jerk 峰 60·1.7/2.5³ ≈ 6.5 m/s³）且在下一窗前结束。
WIN_DUR_RANGE = (2.2, 3.2)
GAP_MIN_S = 3.0
EDGE_PAD_S = 1.5
#: gap 内沿脊线的推进速率（m/s）。上界远离 p95 门 6 与瞬时硬帽 10。
VSEG_RANGE = (0.6, 2.2)
#: yaw 漂移：名义速率与单 gap 增量帽（deg/s、deg）。S₅ 峰值速率 1.875×2.5≈4.7，
#: 恒低于解耦角判据的 5°/s 门槛——yaw 通道结构性不产生同步窗。
YAW_STEP_RATE = 2.5
YAW_STEP_MAX = 20.0
#: progression 保持在弧长的 [3%, 97%] 内（端点处脊线平滑的端窗收窄最明显）。
S_MARGIN_FRAC = 0.03
#: 走廊障碍盒搜索的水平余量与盒顶最小间隙（m）。
CEIL_XZ_PAD_M = 6.0
CEIL_Y_PAD_M = 5.0
#: 脊线平滑：重采样步长与弧长域滑动平均半宽（m）。
SPINE_STEP_M = 0.25
SPINE_SMOOTH_HALF_M = 2.5


@dataclass
class PinkWindow:
    t0: float
    t1: float
    cls: str
    mag_m: float


@dataclass
class Cam4TopdownPlan:
    """形态 b 的全部随机决策。build_frames 不再掷骰——降速重整形后按同一
    plan 重建（guide 变了轨迹跟着变，声称的窗口与脉冲不变），同种子必同。"""
    guide_cam: int
    alt_base_m: float
    tilt_deg: float
    tilt_wobble_deg: float
    tilt_period_s: float
    tilt_phase: float
    s0_frac: float
    yaw_off_deg: float
    windows: List[PinkWindow]
    gap_v: List[float]       # len = 窗数+1
    gap_dir: List[int]       # 方向偏好 ±1（build 按弧长余量可翻转）
    yaw_steps: List[float]   # 每 gap 的 yaw 增量（deg，build 按 gap 时长夹持）
    vdrift: List[float]      # gap 边界处的竖直漂移节点（len = 窗数+2）
    duration_s: float = 60.0

    #: 形态标签（docs/69 §8.3 形态 b）——QC 靠 plan.cam4.mode 分派判据组。
    mode = "topdown"

    def to_dict(self) -> Dict:
        return {
            "mode": "topdown", "guide_cam": self.guide_cam,
            "alt_base_m": round(self.alt_base_m, 2),
            "tilt_deg": round(self.tilt_deg, 2),
            "s0_frac": round(self.s0_frac, 4),
            "windows": [{"t0": round(w.t0, 3), "t1": round(w.t1, 3),
                         "cls": w.cls, "mag_m": round(w.mag_m, 3)}
                        for w in self.windows],
            "note": "高空垂直俯视伴飞：沿 guide 路线 XZ 走廊上空漂移"
                    "（允许时间超前/滞后），不追主体；验形阈值见 QCConfig"
                    "「第四路俯视伴飞」节（docs/69 §8.3 形态 b）",
        }


def sample_plan(rng: random.Random, guide_cam: int,
                duration_s: float = 60.0) -> Cam4TopdownPlan:
    n_win = len(PINK)
    durs = [rng.uniform(*WIN_DUR_RANGE) for _ in range(n_win)]
    n_gap = n_win + 1
    slack = duration_s - sum(durs) - GAP_MIN_S * n_gap - 2 * EDGE_PAD_S
    w = [rng.random() + 0.15 for _ in range(n_gap)]
    gaps = [GAP_MIN_S + max(0.0, slack) * x / sum(w) for x in w]
    gaps[0] += EDGE_PAD_S
    gaps[-1] += EDGE_PAD_S

    classes = list(PINK)
    rng.shuffle(classes)
    windows: List[PinkWindow] = []
    t = 0.0
    for k in range(n_win):
        t += gaps[k]
        windows.append(PinkWindow(t, t + durs[k], classes[k],
                                  rng.uniform(*PULSE_MAG_RANGE)))
        t += durs[k]

    vd = [0.0]
    for _ in range(n_gap):
        vd.append(clamp(vd[-1] + rng.uniform(-VDRIFT_STEP_MAX, VDRIFT_STEP_MAX),
                        -VDRIFT_ABS_MAX, VDRIFT_ABS_MAX))

    return Cam4TopdownPlan(
        guide_cam=guide_cam,
        alt_base_m=rng.uniform(*ALT_BASE_RANGE),
        tilt_deg=rng.uniform(*TILT_BASE_RANGE),
        tilt_wobble_deg=rng.uniform(*TILT_WOBBLE_RANGE),
        tilt_period_s=rng.uniform(*TILT_PERIOD_RANGE),
        tilt_phase=rng.uniform(0.0, 2.0 * math.pi),
        s0_frac=rng.uniform(0.05, 0.35),
        yaw_off_deg=rng.uniform(-30.0, 30.0),
        windows=windows,
        gap_v=[rng.uniform(*VSEG_RANGE) for _ in range(n_gap)],
        gap_dir=[1 if rng.random() < 0.55 else -1 for _ in range(n_gap)],
        yaw_steps=[rng.uniform(-1.0, 1.0) * YAW_STEP_MAX for _ in range(n_gap)],
        vdrift=vd,
        duration_s=duration_s,
    )


def scale_speeds(plan: Cam4TopdownPlan, k: float) -> Cam4TopdownPlan:
    """降速重整形用：只缩推进速率，窗口/脉冲/朝向声称全部不动。"""
    return replace(plan, gap_v=[v * k for v in plan.gap_v])


def window_moves(plan: Cam4TopdownPlan) -> List[Move]:
    """粉色脉冲窗 → plan.json segments（每条必含 6 类，`plan.pink_coverage`）。"""
    return [Move(w.cls, w.t0, w.t1, seg=k,
                 params={"mag_m": round(w.mag_m, 3),
                         "v_target": round(w.mag_m / (w.t1 - w.t0), 3),
                         "extent": round(w.mag_m, 3)})
            for k, w in enumerate(plan.windows)]


# ---------------------------------------------------------------- 内部构件

def _gap_bounds(plan: Cam4TopdownPlan) -> List[Tuple[float, float]]:
    out: List[Tuple[float, float]] = []
    t0 = 0.0
    for w in plan.windows:
        out.append((t0, w.t0))
        t0 = w.t1
    out.append((t0, plan.duration_s))
    return out


def _route_spine(guide_positions: Sequence[Vec3]) -> ArcPath:
    """guide 实测路径 → 平滑脊线的弧长重参数化。

    直接沿原始折线异速推进会在 guide 慢速急弯处放大向心加速度
    （a=v²κ，guide 以 0.35 m/s 过的弯，俯视位以 2.2 m/s 过就是 40 倍）。
    先按弧长均匀重采样、再做弧长域滑动平均：曲率被半宽 ~2.5m 的窗压平，
    切角偏差 ~2m，相对 20/40m 的走廊阈值可忽略。
    """
    cum = [0.0]
    for a, b in zip(guide_positions, guide_positions[1:]):
        cum.append(cum[-1] + vdist(a, b))
    length = cum[-1]
    if length < 1.0:                     # 近乎原地的 guide：脊线退化为一个点
        p = guide_positions[0]
        return ArcPath.from_poses([(p, 0.0, 0.0, 0.0)] * 8)

    m = max(48, min(1200, int(length / SPINE_STEP_M) + 1))

    def _interp(s: float) -> Vec3:
        if s <= 0.0:
            return guide_positions[0]
        if s >= length:
            return guide_positions[-1]
        lo, hi = 0, len(cum) - 1
        while lo < hi - 1:
            mid = (lo + hi) // 2
            if cum[mid] <= s:
                lo = mid
            else:
                hi = mid
        seg = cum[lo + 1] - cum[lo]
        u = 0.0 if seg <= 1e-12 else (s - cum[lo]) / seg
        a, b = guide_positions[lo], guide_positions[lo + 1]
        return (a[0] + (b[0] - a[0]) * u, a[1] + (b[1] - a[1]) * u,
                a[2] + (b[2] - a[2]) * u)

    pts = [_interp(length * i / (m - 1)) for i in range(m)]
    half = max(1, int(SPINE_SMOOTH_HALF_M / (length / (m - 1))))
    for _ in range(2):
        smoothed: List[Vec3] = []
        for i in range(m):
            lo, hi = max(0, i - half), min(m - 1, i + half)
            k = hi - lo + 1
            smoothed.append((sum(p[0] for p in pts[lo:hi + 1]) / k,
                             sum(p[1] for p in pts[lo:hi + 1]) / k,
                             sum(p[2] for p in pts[lo:hi + 1]) / k))
        pts = smoothed
    return ArcPath.from_poses([(p, 0.0, 0.0, 0.0) for p in pts])


def _progress_profile(plan: Cam4TopdownPlan, n: int, fps: float) -> List[float]:
    """无符号推进速率剖面（窗内恒 0，gap 内 S₅ 起停，端点速率精确归零）。"""
    u = [0.0] * n
    for k, (g0, g1) in enumerate(_gap_bounds(plan)):
        g = g1 - g0
        if g <= 0.6:
            continue
        r = min(1.2, g / 3.0)
        v = plan.gap_v[k]
        i0 = int(math.ceil(g0 * fps))
        i1 = min(n - 1, int(math.floor(g1 * fps)))
        for i in range(i0, i1 + 1):
            t = i / fps
            u[i] = v * min(smoothstep5((t - g0) / r), smoothstep5((g1 - t) / r))
    return u


def _integrate_s(plan: Cam4TopdownPlan, u: Sequence[float], length: float,
                 fps: float) -> List[float]:
    """按 gap 解析方向与预算后积分弧长（往复漂移 = 时间超前/滞后的可执行形式）。

    方向偏好在弧长余量不够时翻转、预算超出可用范围时整段等比缩——缩的是整个
    gap 的速率剖面，不引入速度不连续。
    """
    n = len(u)
    dt = 1.0 / fps
    lo, hi = S_MARGIN_FRAC * length, (1.0 - S_MARGIN_FRAC) * length
    if hi <= lo:
        return [0.5 * length] * n
    bounds = _gap_bounds(plan)
    frame_gap = [-1] * n
    for k, (g0, g1) in enumerate(bounds):
        i0 = int(math.ceil(g0 * fps))
        i1 = min(n - 1, int(math.floor(g1 * fps)))
        for i in range(i0, i1 + 1):
            frame_gap[i] = k
    budgets = [0.0] * len(bounds)
    for i in range(n):
        if frame_gap[i] >= 0:
            budgets[frame_gap[i]] += u[i] * dt

    s0 = clamp(plan.s0_frac * length, lo, hi)
    dirs: List[int] = []
    scales: List[float] = []
    s_cur = s0
    for k, b in enumerate(budgets):
        d = plan.gap_dir[k]
        if not (lo <= s_cur + d * b <= hi):
            d = -d
        room = (hi - s_cur) if d > 0 else (s_cur - lo)
        kk = 1.0 if b <= room else max(0.0, 0.9 * room / max(b, 1e-9))
        dirs.append(d)
        scales.append(kk)
        s_cur += d * b * kk

    out = [0.0] * n
    s_cur = s0
    for i in range(n):
        out[i] = s_cur
        k = frame_gap[i]
        if k >= 0:
            s_cur += dirs[k] * u[i] * scales[k] * dt
    return out


def _gap_blend_series(bounds: Sequence[Tuple[float, float]],
                      start_vals: Sequence[float], deltas: Sequence[float],
                      n: int, fps: float) -> List[float]:
    """逐 gap 的 S₅ 过渡序列：窗内冻结（粉色诚实性要求窗内无背景运动），
    gap 内掐头去尾各 0.2s 过渡——过渡在窗开始前就已结束。"""
    out: List[float] = []
    for i in range(n):
        t = i / fps
        v: Optional[float] = None
        for k, (g0, g1) in enumerate(bounds):
            if t < g0:
                v = start_vals[k]
                break
            if t <= g1:
                span = max(1e-9, (g1 - g0) - 0.4)
                v = start_vals[k] + deltas[k] * smoothstep5((t - g0 - 0.2) / span)
                break
        if v is None:
            v = start_vals[-1] + deltas[-1]
        out.append(v)
    return out


def _yaw_series(plan: Cam4TopdownPlan, arc: ArcPath, s0: float,
                n: int, fps: float) -> List[float]:
    eps = max(0.25, 0.01 * arc.length)
    a = arc.at(clamp(s0 - eps, 0.0, arc.length))[0]
    b = arc.at(clamp(s0 + eps, 0.0, arc.length))[0]
    dx, dz = b[0] - a[0], b[2] - a[2]
    base = math.degrees(math.atan2(dx, dz)) if abs(dx) + abs(dz) > 1e-9 else 0.0
    yaw0 = base + plan.yaw_off_deg

    bounds = _gap_bounds(plan)
    starts: List[float] = []
    deltas: List[float] = []
    cur = yaw0
    for k, (g0, g1) in enumerate(bounds):
        cap = max(0.0, YAW_STEP_RATE * ((g1 - g0) - 0.4))
        step = clamp(plan.yaw_steps[k], -cap, cap)
        starts.append(cur)
        deltas.append(step)
        cur += step
    return _gap_blend_series(bounds, starts, deltas, n, fps)


def _vdrift_series(plan: Cam4TopdownPlan, n: int, fps: float) -> List[float]:
    bounds = _gap_bounds(plan)
    starts = list(plan.vdrift[:len(bounds)])
    deltas = [plan.vdrift[k + 1] - plan.vdrift[k] for k in range(len(bounds))]
    return _gap_blend_series(bounds, starts, deltas, n, fps)


def _pulse_offsets(plan: Cam4TopdownPlan, yaws: Sequence[float],
                   n: int, fps: float) -> List[Vec3]:
    """粉色脉冲位移场。轴向 = 窗首帧 yaw 的声称基底——与 QC
    `plan.pink_honesty` 的 `_yaw_basis`（atan2(fx,fz) → fwd/right）同式；
    窗内 S₅ 单调走满 mag，窗后在下一 gap 内衰减（下一窗前 0.5s 结束）。"""
    off: List[List[float]] = [[0.0, 0.0, 0.0] for _ in range(n)]
    bounds = _gap_bounds(plan)
    for w_idx, w in enumerate(plan.windows):
        i0 = min(n - 1, int(round(w.t0 * fps)))
        yaw0 = math.radians(yaws[i0])
        fwd = (math.sin(yaw0), 0.0, math.cos(yaw0))
        right = (math.cos(yaw0), 0.0, -math.sin(yaw0))
        axis = {"translate_forward": fwd,
                "translate_back": (-fwd[0], 0.0, -fwd[2]),
                "translate_right": right,
                "translate_left": (-right[0], 0.0, -right[2]),
                "translate_up": (0.0, 1.0, 0.0),
                "translate_down": (0.0, -1.0, 0.0)}[w.cls]
        g0, g1 = bounds[w_idx + 1]
        decay = clamp(g1 - g0 - 0.5, 1.5, 6.0)
        for i in range(int(math.floor(w.t0 * fps)), n):
            t = i / fps
            if t < w.t0:
                continue
            if t <= w.t1:
                amp = w.mag_m * smoothstep5((t - w.t0) / (w.t1 - w.t0))
            else:
                amp = w.mag_m * (1.0 - smoothstep5((t - w.t1) / decay))
                if amp <= 0.0:
                    break
            off[i][0] += axis[0] * amp
            off[i][1] += axis[1] * amp
            off[i][2] += axis[2] * amp
    return [(o[0], o[1], o[2]) for o in off]


def _alt_base(plan: Cam4TopdownPlan, others_pos: Sequence[Sequence[Vec3]],
              scene, guide_positions: Sequence[Vec3]) -> float:
    """高度基线：前三路全程最高点 + 基准高差，再对走廊内障碍盒顶抬底。

    穿模红线（`_min_clearance`）是构造性保证：路线走廊 6m 内有高盒（楼体）时
    直接飞到盒顶之上，不靠事后重试。QC 的高差判据是相对量，抬高恒安全。
    """
    top3 = max(p[1] for pk in others_pos for p in pk)
    y = top3 + plan.alt_base_m
    for b in getattr(scene, "boxes", []) if scene is not None else []:
        near = any(
            max(b.lo[0] - p[0], 0.0, p[0] - b.hi[0]) ** 2
            + max(b.lo[2] - p[2], 0.0, p[2] - b.hi[2]) ** 2 <= CEIL_XZ_PAD_M ** 2
            for p in guide_positions[::15])
        if near:
            y = max(y, b.hi[1] + CEIL_Y_PAD_M)
    return y


# ---------------------------------------------------------------- 主构造

@dataclass
class Cam4TopdownResult:
    positions: List[Vec3]
    quats: List[Quat]
    eulers: List[Vec3]
    speeds: List[Vec3]
    start: Pose


def build_frames(plan: Cam4TopdownPlan, guide_positions: Sequence[Vec3],
                 others_pos: Sequence[Sequence[Vec3]], scene,
                 n_frames: int, fps: float) -> Cam4TopdownResult:
    dt = 1.0 / fps
    arc = _route_spine(guide_positions)
    u = _progress_profile(plan, n_frames, fps)
    s_series = _integrate_s(plan, u, arc.length, fps)
    yaws = _yaw_series(plan, arc, s_series[0], n_frames, fps)
    vdrift = _vdrift_series(plan, n_frames, fps)
    offs = _pulse_offsets(plan, yaws, n_frames, fps)
    y_base = _alt_base(plan, others_pos, scene, guide_positions)

    positions: List[Vec3] = []
    for i in range(n_frames):
        p = arc.at(s_series[i])[0]
        positions.append((p[0] + offs[i][0],
                          y_base + vdrift[i] + offs[i][1],
                          p[2] + offs[i][2]))

    # 实测最小高差的确定性抬底：整体平移不动任何速度/朝向，判据只会更宽。
    n_m = min(len(positions), *(len(pk) for pk in others_pos))
    margin = min(positions[i][1] - max(pk[i][1] for pk in others_pos)
                 for i in range(n_m))
    if margin < ALT_MARGIN_FLOOR:
        lift = ALT_MARGIN_FLOOR - margin
        positions = [(p[0], p[1] + lift, p[2]) for p in positions]

    elevs = [plan.tilt_deg
             + plan.tilt_wobble_deg * math.sin(
                 2.0 * math.pi * (i * dt) / plan.tilt_period_s + plan.tilt_phase)
             - 90.0
             for i in range(n_frames)]
    quats = [quat_from_yaw_elev_roll(y, e, 0.0) for y, e in zip(yaws, elevs)]
    eulers = [euler_tuple(y, e, 0.0) for y, e in zip(yaws, elevs)]
    speeds = frame_speeds(positions, fps)
    return Cam4TopdownResult(positions, quats, eulers, speeds,
                             Pose(positions[0], yaws[0], elevs[0], 0.0))

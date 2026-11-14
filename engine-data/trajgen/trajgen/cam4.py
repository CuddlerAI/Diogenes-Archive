"""第四路：guide path + 时间重参数化 ρ(t)（docs/71 §6）。

构造口径（决定 QC 能不能反算出正确的 ρ）：
- 第四路沿**自己的标称路径**（guide 形状的横向偏置延长版）推进，
  推进的**弧长**按 `s₄(t) = ∫₀ᵗ ρ(τ)·v_g(τ) dτ` 排 —— v_g 是 guide 路的**逐帧瞬时速率**。
  于是 `|v₄(t)| = ρ(t)·v_g(t)` 在有限差分意义下成立，
  engine-data-qc 的 `cam4.*` 三条（变速恰 1 次 / t_sw 核对 / 禁带驻留）才有得可算。
- ρ 用五次 smoothstep 过渡：`ρ(t)=ρ₁+(ρ₂−ρ₁)·S₅((t−t_sw)/Δ)`，两端一二阶导全零 ⇒ ρ 全程 C²。
  阶跃或线性 ramp 会按 `a₄=ρ²a_g+ρ'v_g`、`j₄=ρ³j_g+3ρρ'a_g+ρ''v_g` 把 a/j 打爆（§6）。
- 禁带 65%–150%：**禁驻留不禁穿越**。ρ₁/ρ₂ 各自取自 [0.33,0.65]∪[1.5,3.0]，
  跨带时 ρ 单调穿过，QC 侧扣掉一次单调穿越后驻留 ≈0。
- `speed≤10` 是瞬时硬帽，p95 仍按表 2 的 6m/s（§6 我方解读，docs/72 待确认 15）。
"""

import math
import random
from dataclasses import dataclass
from typing import Dict, List, Optional, Sequence, Tuple

from .curve import KNOT_S
from .mathutil import (Quat, Vec3, clamp, euler_tuple, linear_ctrl, linear_eval,
                       quat_from_yaw_elev_roll, smoothstep5, unwrap_deg, vdist,
                       wrap180)
from .sampler import SmoothTrack, frame_speeds

#: ρ 允许区间。需求方的坏球区是 0.65–1.5，这里**各留一档内缩余量**取 0.60 / 1.65：
#: 贴着带边取值时，QC 反算 ρ 的测量噪声会让曲线在带边反复进出，
#: 「禁带驻留」与「变速恰 1 次」两条都会被噪声打挂（实测 ρ₂=1.511 时驻留 1.87s）。
RHO_BANDS = ((0.33, 0.60), (1.65, 3.0))
#: 变速切换点范围（s，60s 口径 = U(20,40)；参数化时长下按比例取 U(d/3, 2d/3)，
#: 300s ⇒ U(100,200)。「一分钟内随机变速一次」的另一种读法（300s=5 次变速）
#: 未采用，先按恰 1 次大变速执行——两种读法在 docs/78 §10 标注待需求方裁决）
TSW_RANGE = (20.0, 40.0)
TSW_FRAC = (20.0 / 60.0, 40.0 / 60.0)
#: S₅ 过渡宽度范围（s）
DELTA_RANGE = (1.5, 3.0)
#: 瞬时速度硬帽（m/s）
V4_CAP = 10.0
#: 第四路角速度瞬时上限（°/s）。表 2 的帽是 90，这里内缩到 80 留余量：
#: 限幅在欧拉三通道上做线性插值近似 slerp，合成角会有零点几度的残差。
V4_OMEGA_CAP = 80.0


@dataclass
class Cam4Plan:
    guide_cam: int
    rho1: float
    rho2: float
    t_sw: float
    delta: float

    #: 形态标签（docs/69 §8.3 形态 a）。**不进 to_dict**：缺省形态的 plan.json
    #: 不写 mode 键，历史 clip 与现产输出逐字不变（QC 侧缺省即按 timewarp 判）。
    mode = "timewarp"

    def rho(self, t: float) -> float:
        return self.rho1 + (self.rho2 - self.rho1) * smoothstep5((t - self.t_sw) / self.delta)

    @property
    def rho_max(self) -> float:
        return max(self.rho1, self.rho2)

    def to_dict(self) -> Dict:
        return {"guide_cam": self.guide_cam, "rho1": round(self.rho1, 4),
                "rho2": round(self.rho2, 4), "t_sw": round(self.t_sw, 3),
                "delta": round(self.delta, 3),
                "band": [0.65, 1.5], "note": "禁带禁驻留不禁穿越；v₄ 瞬时硬帽 10m/s"}


def sample_plan(rng: random.Random, guide_cam: int,
                rho_max_allowed: float = 3.0,
                duration_s: float = 60.0) -> Cam4Plan:
    """按传导预算裁掉超预算的 ρ 档位后采样两段速率因子。

    `rho_max_allowed` 由 guide 的实测速度 p95 反推（`v₄=ρ·v_g ≤ 表 2 的 6m/s`）——
    guide 跑得快时高档 ρ 自然被禁掉，这正是 docs/71 §6 传导预算的可执行形式。
    """
    bands = [(lo, min(hi, rho_max_allowed)) for lo, hi in RHO_BANDS
             if lo <= rho_max_allowed]
    if not bands:                      # guide 快到连 0.33 都超预算：退到最低档
        bands = [(RHO_BANDS[0][0], RHO_BANDS[0][0])]

    def pick() -> float:
        lo, hi = bands[rng.randrange(len(bands))]
        return rng.uniform(lo, hi) if hi > lo else lo

    # Δρ 与过渡宽度 Δ 必须让 QC 的检测器看得见（metrics.rho_detectable），
    # 否则申报了变速而 QC 反算不出来，`cam4.switch_count` 会判 0 次。
    from .metrics import rho_detectable
    rho1, rho2 = pick(), pick()
    delta = rng.uniform(*DELTA_RANGE)
    for _ in range(60):
        if rho_detectable(rho1, rho2, delta):
            break
        rho1, rho2 = pick(), pick()
        delta = rng.uniform(*DELTA_RANGE)
    else:                                # 兜底：拉到两个带的两端并取最窄过渡
        rho1, rho2 = bands[0][0], bands[-1][1]
        delta = DELTA_RANGE[0]
    t_sw = rng.uniform(TSW_FRAC[0] * duration_s, TSW_FRAC[1] * duration_s)
    return Cam4Plan(guide_cam, rho1, rho2, t_sw, delta)


def guide_speeds(positions: Sequence[Vec3], fps: float) -> List[float]:
    """guide 路的逐帧瞬时速率（与 QC 的有限差分同口径）。"""
    v = [vdist(a, b) * fps for a, b in zip(positions, positions[1:])]
    v.append(v[-1] if v else 0.0)
    return v


def arclength_table(track: SmoothTrack, samples: int) -> Tuple[List[float], List[float]]:
    """标称轨道的 (时刻, 累积弧长) 表。"""
    ts = [track.duration * i / (samples - 1) for i in range(samples)]
    pts = [track.at(t).p for t in ts]
    cum = [0.0]
    for a, b in zip(pts, pts[1:]):
        cum.append(cum[-1] + vdist(a, b))
    return ts, cum


def _invert(ts: Sequence[float], cum: Sequence[float], s: float) -> float:
    """弧长 → 标称时刻（线性插值）。"""
    if s <= 0.0:
        return ts[0]
    if s >= cum[-1]:
        return ts[-1]
    lo, hi = 0, len(cum) - 1
    while lo < hi - 1:
        mid = (lo + hi) // 2
        if cum[mid] <= s:
            lo = mid
        else:
            hi = mid
    seg = cum[lo + 1] - cum[lo]
    u = 0.0 if seg <= 1e-12 else (s - cum[lo]) / seg
    return ts[lo] + (ts[lo + 1] - ts[lo]) * u


#: 弧长重参数化的采样间隔（m）与控制点数上限。
#: **必须细于第四路的每帧推进量** `ρ·v_g·dt`（典型 1×0.5/30 ≈ 0.017m）：
#: 表比帧步粗时，连续几帧落在同一段线性插值里，实际位移与请求弧长之间的
#: 误差按表步长为周期起伏，QC 反算的 `ρ=v₄/v_g` 跟着抖，`switch_count`
#: 把抖动判成额外变速（0.06m → 0.02m，120 条 A/B 里 cam4 类失败 9 → 2）。
#: 点数上限同步放宽，否则长标称路径（弧长上百米）会被上限顶回粗步长。
_ARC_STEP_M = 0.02
_ARC_MAX_PTS = 16000


class ArcPath:
    """标称轨道的**弧长重参数化**：`s → 位姿`，且 `|dP/ds| ≡ 1`。

    这是让 `v₄(t) = ρ(t)·v_g(t)` 成为恒等式的关键：位置直接按弧长取，
    第四路的速度就只由弧长排程决定，**与标称路径自身的速度剖面无关**。

    走「按时间参数化 + 反查 τ(s)」的路子会在标称近乎停步处翻车 ——
    那里 τ(s) 近乎垂直，任何平滑都会把它压平，第四路开头原地不动，
    QC 反算的 ρ 从 0 爬起来被判成多一次变速（实测稳定复现于 t≈2–7s）。
    """

    def __init__(self, track: SmoothTrack, samples: int = 0, passes: int = 2):
        n = samples or max(256, int(track.duration * 30) + 1)
        ts = [track.duration * i / (n - 1) for i in range(n)]
        poses = [(p.p, p.yaw, p.elev, p.roll) for p in (track.at(t) for t in ts)]
        self._rebuild(poses)
        # 再归一化一遍：样条会切角，首轮出来的 `|dP/ds|` 只是近似 1（差几个百分点）。
        # 这点涟漪会让 QC 反算的 ρ 在过渡中段非单调，把「一次变速」的检出区间劈成两段，
        # `cam4.switch_count` 于是判成 2 次（实测稳定复现）。
        for _ in range(max(0, passes - 1)):
            dense = int(clamp(self.length / (_ARC_STEP_M * 0.5), 256, _ARC_MAX_PTS * 2))
            self._rebuild([self.at(self.length * i / (dense - 1)) for i in range(dense)])

    @classmethod
    def from_poses(cls, poses: Sequence[Tuple[Vec3, float, float, float]],
                   passes: int = 2) -> "ArcPath":
        """从裸位姿序列构造（车辆导演模式：guide 相机路径的平滑复制+偏置版，
        没有 SmoothTrack 可给）。归一化语义与 __init__ 一致。"""
        self = cls.__new__(cls)
        self._rebuild(list(poses))
        for _ in range(max(0, passes - 1)):
            dense = int(clamp(self.length / (_ARC_STEP_M * 0.5), 256, _ARC_MAX_PTS * 2))
            self._rebuild([self.at(self.length * i / (dense - 1)) for i in range(dense)])
        return self

    def _rebuild(self, poses: Sequence[Tuple[Vec3, float, float, float]]) -> None:
        cum = [0.0]
        for a, b in zip(poses, poses[1:]):
            cum.append(cum[-1] + vdist(a[0], b[0]))
        self.length = cum[-1]
        m = int(clamp(self.length / _ARC_STEP_M, 64, _ARC_MAX_PTS))
        picked = [self._lerp_pose(poses, cum, self.length * i / (m - 1)) for i in range(m)]
        self.n_values = len(picked)
        self.ctrl = {
            "x": linear_ctrl([p[0][0] for p in picked]),
            "y": linear_ctrl([p[0][1] for p in picked]),
            "z": linear_ctrl([p[0][2] for p in picked]),
            "yaw": linear_ctrl(unwrap_deg([p[1] for p in picked])),
            "elev": linear_ctrl([p[2] for p in picked]),
            "roll": linear_ctrl([p[3] for p in picked]),
        }

    @staticmethod
    def _lerp_pose(poses, cum, s):
        if s <= 0.0:
            return poses[0]
        if s >= cum[-1]:
            return poses[-1]
        lo, hi = 0, len(cum) - 1
        while lo < hi - 1:
            mid = (lo + hi) // 2
            if cum[mid] <= s:
                lo = mid
            else:
                hi = mid
        seg = cum[lo + 1] - cum[lo]
        u = 0.0 if seg <= 1e-12 else (s - cum[lo]) / seg
        (ap, ayaw, aelev, aroll) = poses[lo]
        (bp, byaw, belev, broll) = poses[lo + 1]
        return ((ap[0] + (bp[0] - ap[0]) * u,
                 ap[1] + (bp[1] - ap[1]) * u,
                 ap[2] + (bp[2] - ap[2]) * u),
                ayaw + wrap180(byaw - ayaw) * u,
                aelev + (belev - aelev) * u,
                aroll + (broll - aroll) * u)

    def at(self, s: float) -> Tuple[Vec3, float, float, float]:
        u = clamp(s, 0.0, self.length) / max(1e-9, self.length)
        n = self.n_values
        return ((linear_eval(self.ctrl["x"], n, u),
                 linear_eval(self.ctrl["y"], n, u),
                 linear_eval(self.ctrl["z"], n, u)),
                linear_eval(self.ctrl["yaw"], n, u),
                linear_eval(self.ctrl["elev"], n, u),
                linear_eval(self.ctrl["roll"], n, u))


def _step_angle_deg(a: Tuple[float, float, float], b: Tuple[float, float, float]) -> float:
    """两组 (yaw, elev, roll) 之间的合成转角（度）。"""
    qa = quat_from_yaw_elev_roll(*a)
    qb = quat_from_yaw_elev_roll(*b)
    d = abs(sum(x * y for x, y in zip(qa, qb)))
    return 2.0 * math.degrees(math.acos(clamp(d, -1.0, 1.0)))


def _slew_pass(seq: Sequence[Tuple[float, float, float]], cap_deg: float,
               reverse: bool) -> List[Tuple[float, float, float]]:
    order = range(len(seq) - 2, -1, -1) if reverse else range(1, len(seq))
    out = list(seq)
    prev = len(seq) - 1 if reverse else 0
    for i in order:
        cur, tgt = out[prev], seq[i]
        ang = _step_angle_deg(cur, tgt)
        if ang > cap_deg:
            k = cap_deg / ang
            tgt = (cur[0] + wrap180(tgt[0] - cur[0]) * k,
                   cur[1] + (tgt[1] - cur[1]) * k,
                   cur[2] + (tgt[2] - cur[2]) * k)
        out[i] = tgt
        prev = i
    return out


def limit_angular_rate(yaw: Sequence[float], elev: Sequence[float],
                       roll: Sequence[float], cap_deg_per_step: float
                       ) -> Tuple[List[float], List[float], List[float]]:
    """把第四路逐帧朝向压到「相邻帧合成转角 ≤ cap」。

    **为什么非限不可**：朝向被**弧长**绑定（`ArcPath.at(s)`），于是
    `ω₄ = (dθ/ds)·v₄`。标称轨道在转角密集处（实测几乎全在 move 交界）会把大转角
    压进很短的弧长，dθ/ds 局部飙到全程 p95 的 6 倍以上；第四路按自己的节奏走过去
    就是一记甩镜——实测 23 条计划里 5 条超 90°/s，最高 351°/s，而 p95 只有 20 上下，
    **p95 口径完全看不见**。压 dθ/ds 的源头（收紧标称转速）会把中位数一起压平，
    所以在输出端限速。

    **为什么前向+后向再平均**：两个方向各自限一次，各自都满足 `|Δ| ≤ cap`，
    按三角不等式平均后仍满足；平均把急转对称地摊在峰值两侧，避免单向限幅留下的
    相位滞后（镜头「慢半拍」再追上去）。未违规处两向都等于原值，平均即原值，不动。
    限幅只改朝向不改位置，`v₄ = ρ·v_g` 的恒等式与 QC 的 `cam4.*` 三条不受影响。
    """
    n = len(yaw)
    if n < 3 or cap_deg_per_step <= 0:
        return list(yaw), list(elev), list(roll)
    seq = list(zip(yaw, elev, roll))
    fwd = _slew_pass(seq, cap_deg_per_step, reverse=False)
    bwd = _slew_pass(seq, cap_deg_per_step, reverse=True)
    avg = [((f[0] + b[0]) * 0.5, (f[1] + b[1]) * 0.5, (f[2] + b[2]) * 0.5)
           for f, b in zip(fwd, bwd)]
    # 欧拉插值只是 slerp 的近似，平均后可能还差一点点；再前向收一次兜底。
    avg = _slew_pass(avg, cap_deg_per_step, reverse=False)
    return [a[0] for a in avg], [a[1] for a in avg], [a[2] for a in avg]


@dataclass
class Cam4Result:
    positions: List[Vec3]
    quats: List[Quat]
    eulers: List[Vec3]
    speeds: List[Vec3]
    rho_series: List[float]
    tau_series: List[float]  # 逐帧对应的标称时刻（把标称 move 映回墙钟用）
    exhausted: bool          # 标称路径长度不够，末段被夹住
    v4_max: float


def build_frames(plan: Cam4Plan, nominal: SmoothTrack, guide_v: Sequence[float],
                 n_frames: int, fps: float,
                 arc: Optional[ArcPath] = None,
                 nominal_table: Optional[Tuple[List[float], List[float]]] = None
                 ) -> Cam4Result:
    """按 `s₄(t)=∫ρ·v_g` 在**弧长重参数化**的标称路径上取位姿。

    `nominal_table`：(时刻, 累积弧长) 表，缺省按 `nominal` 自算。调用方给
    `arc` 时若 arc 不是直接从 `nominal` 建的（诚实性修正后的重建版），
    必须一并给出与 arc 同源的表，否则 tau 映射会随修正量错位。
    """
    dt = 1.0 / fps
    arc = arc or ArcPath(nominal)
    total = arc.length

    # 弧长 → 标称时刻走**弧长表反查**，不走 `s/total·duration` 的均匀速假设：
    # 标称速度只是近似常值（粉色段有 extent 抬底、旋转类实际速率也偏低），
    # 均匀速假设下 `_remap_cam4_moves` 的窗口会整体错位——实测 translate_down
    # 的重映射窗里垂直位移恰为 0（窗口套在了别的 move 的几何上），
    # `plan.pink_honesty` 的声称与轨迹从根上对不上号。
    tab_ts, tab_cum = nominal_table or arclength_table(
        nominal, max(256, int(nominal.duration * fps) + 1))
    tab_k = (tab_cum[-1] / total) if total > 0 else 0.0

    s = 0.0
    positions: List[Vec3] = []
    yaws: List[float] = []
    elevs: List[float] = []
    rolls: List[float] = []
    rhos: List[float] = []
    taus: List[float] = []
    exhausted = False
    for i in range(n_frames):
        t = i * dt
        p, yaw, elev, roll = arc.at(s)
        positions.append(p)
        yaws.append(yaw)
        elevs.append(elev)
        rolls.append(roll)
        rhos.append(plan.rho(t))
        taus.append(_invert(tab_ts, tab_cum, s * tab_k) if total > 0 else 0.0)
        step = plan.rho(t + 0.5 * dt) * guide_v[min(i, len(guide_v) - 1)] * dt
        if s + step > total:
            exhausted = True
            step = max(0.0, total - s)
        s += step

    # 朝向限速（位置不动 ⇒ ρ 反算不受影响）。见 limit_angular_rate 的说明。
    yaws, elevs, rolls = limit_angular_rate(yaws, elevs, rolls, V4_OMEGA_CAP * dt)
    quats = [quat_from_yaw_elev_roll(y, e, r) for y, e, r in zip(yaws, elevs, rolls)]
    eulers = [euler_tuple(y, e, r) for y, e, r in zip(yaws, elevs, rolls)]

    speeds = frame_speeds(positions, fps)
    v4_max = max((vdist(a, b) * fps for a, b in zip(positions, positions[1:])), default=0.0)
    return Cam4Result(positions, quats, eulers, speeds, rhos, taus, exhausted, v4_max)


#: 标称路径长度余量（弦长 vs 弧长、末段夹持都要留一点）
LENGTH_MARGIN = 1.08


def required_length(plan: Cam4Plan, guide_v: Sequence[float], n_frames: int,
                    fps: float, margin: float = LENGTH_MARGIN) -> float:
    """标称路径至少要多长才够第四路走完全程（含余量）。"""
    dt = 1.0 / fps
    s = 0.0
    for i in range(n_frames):
        s += plan.rho(i * dt + 0.5 * dt) * guide_v[min(i, len(guide_v) - 1)] * dt
    return s * margin


def conduction_budget(plan: Cam4Plan, v_p95_max: float = 6.0,
                      w_p95_max: float = 90.0) -> Tuple[float, float]:
    """docs/71 §6 传导预算：guide 侧允许的 v/ω 上限 = 表 2 上限 / ρ_max。"""
    rm = max(1.0, plan.rho_max)
    return v_p95_max / rm, w_p95_max / rm

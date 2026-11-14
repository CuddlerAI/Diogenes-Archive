"""68 号轨迹库：13 类 navigate + 3 类特殊运镜（docs/68 §3.3/§3.4）。

与 69 号的 23 类运镜**几乎不相交**（docs/71 §4 已判定，docs/70 §4.2 的「复用同一套
模板」论断不成立）：69 的类目是**运镜语法**（平移 / 摇动 / 环绕 / 螺旋…），可以在一条
片子里分段拼接；68 的类目是**整片轨迹拓扑**（Open / Loop / Revisit / Oscillating /
Stay / 载具 / 直升机 / 超人），一条片子从头到尾只属于一类，而且类目语义本身就是验收项
（「首尾帧必须尽量一致」「至少重复 2 次以上」「rotation、location 完全无变化」）。

所以这是新写的一套生成器，但**下面这些一份不重写**：
- `scene.Scene` 的占据/自由壳/射线遮挡；
- `sampler.smooth_from_knots` 的穿模防线 + 速度/转速限幅 + 五次 B 样条；
- `metrics.physics` 的运动学反算（p95 口径与 QC 逐字同源，#431 教训）。

每个类目生成器只负责一件事：**按该类目的拓扑语义铺节点位姿**。限幅、防穿模、
数值口径都在共用层。
"""

import math
import random
from dataclasses import dataclass, field
from typing import Callable, Dict, List, Optional, Sequence, Tuple

from . import metrics as met
from .curve import KNOT_S, knot_times
from .mathutil import (Vec3, clamp, quat_angle_deg, vadd, vdist, vscale, vsub,
                       vunit, wrap180)
from .sampler import frames_from_times, smooth_from_knots
from .scene import Scene

FPS = 30.0

#: 单条时长白名单（docs/72 裁决：漫游 180s / 特殊 60s）
DURATIONS = (60.0, 180.0)


@dataclass(frozen=True)
class SfmClass:
    """一个 68 类目：配额、时长权重、生成器。"""
    name: str
    quota: float                      # 语料级比例（navigate 内 / 特殊运镜内）
    group: str                        # "navigate" | "special"
    dur_weights: Dict[float, int]     # 时长 → 权重（「1min:3min = 2:1」即 {60:2, 180:1}）
    gen: Callable
    #: 该类目是否允许「整片无位移」——静止类的运动学闸门要另算
    stationary: bool = False

    def pick_duration(self, rng: random.Random) -> float:
        opts, wts = zip(*self.dur_weights.items())
        return rng.choices(list(opts), weights=list(wts), k=1)[0]


# ---------------------------------------------------------------- 共用几何

def _look_at(p: Vec3, target: Vec3) -> Tuple[float, float]:
    """返回 (yaw, elev)，Unity 口径：yaw 绕 +Y，elev 为俯仰（上为正）。"""
    d = vsub(target, p)
    n = math.sqrt(d[0] * d[0] + d[2] * d[2])
    yaw = math.degrees(math.atan2(d[0], d[2]))
    elev = math.degrees(math.atan2(d[1], max(1e-6, n)))
    return yaw, elev


def _dither(rng: random.Random, ts: Sequence[float], amp_deg: float,
            period_s: float) -> List[float]:
    """运镜扰动（docs/68 §3.2）：单方向移动/摇镜不得持续 2s 以上。

    做法是叠一路周期 < 2s 的低幅正弦——它保证任何 2s 窗内朝向的一阶导都会变号，
    而限幅器会把幅值压进 jerk 预算，所以扰动不会把物理闸门顶穿。
    """
    ph = rng.uniform(0, 2 * math.pi)
    return [amp_deg * math.sin(2 * math.pi * t / period_s + ph) for t in ts]


def _free_or(scene: Scene, rng: random.Random, fallback: Vec3,
             near: Optional[Vec3] = None, radius: float = 0.0) -> Vec3:
    p = scene.sample_free(rng, near=near, radius=radius)
    return p if p is not None else fallback


def _center(scene: Scene) -> Vec3:
    return (0.5 * (scene.lo[0] + scene.hi[0]),
            scene.ground_y + 0.5 * (scene.eye_lo + scene.eye_hi),
            0.5 * (scene.lo[2] + scene.hi[2]))


def _path_yaws(pos: Sequence[Vec3]) -> List[float]:
    """朝向跟随路径切线（最后一帧沿用前一帧）。"""
    yaws = []
    for a, b in zip(pos, pos[1:]):
        d = vsub(b, a)
        yaws.append(math.degrees(math.atan2(d[0], d[2])) if (abs(d[0]) + abs(d[2])) > 1e-9
                    else (yaws[-1] if yaws else 0.0))
    yaws.append(yaws[-1] if yaws else 0.0)
    return yaws


def _bezier(p0: Vec3, p1: Vec3, p2: Vec3, u: float) -> Vec3:
    a = (1 - u) ** 2
    b = 2 * (1 - u) * u
    c = u * u
    return (a * p0[0] + b * p1[0] + c * p2[0],
            a * p0[1] + b * p1[1] + c * p2[1],
            a * p0[2] + b * p1[2] + c * p2[2])


# ---------------------------------------------------------------- 生成器
# 每个生成器签名：(scene, rng, ts) -> (positions, yaws, elevs, rolls)

def _gen_open(scene: Scene, rng: random.Random, ts: Sequence[float],
              combat: bool = False):
    """Open：A→B 不回头，起点终点分离，含拐弯 / S 形。"""
    c = _center(scene)
    a = _free_or(scene, rng, c)
    span = 0.30 * min(scene.hi[0] - scene.lo[0], scene.hi[2] - scene.lo[2])
    b = _free_or(scene, rng, vadd(a, (span, 0.0, span)), near=a, radius=span)
    # 中间控制点侧偏 → S 形；战斗档偏得更狠、速度更快
    mid = vscale(vadd(a, b), 0.5)
    perp = vunit((-(b[2] - a[2]), 0.0, b[0] - a[0]))
    off = (0.28 if combat else 0.22) * vdist(a, b)
    ctrl = vadd(mid, vscale(perp, off * rng.choice((-1.0, 1.0))))
    T = ts[-1]
    pos = [_bezier(a, ctrl, b, t / T) for t in ts]
    yaws = _path_yaws(pos)
    # 正弦扰动的角 jerk 幅值 = A·(2π/T)³；600°/s³ 预算下 T=2.0s 时 A ≤ 19°
    amp = 7.0 if combat else 8.0
    per = 2.3 if combat else 2.0
    yaws = [y + d for y, d in zip(yaws, _dither(rng, ts, amp, per))]
    elevs = _dither(rng, ts, 3.5 if combat else 3.0, 2.3)
    return pos, yaws, elevs, [0.0] * len(ts)


def _gen_loop_move(scene: Scene, rng: random.Random, ts: Sequence[float]):
    """Loop-有位移：A→B→A 闭合环路，**首尾帧 location/rotation 尽量一致**。

    用闭合椭圆而不是「去了再原路返回」：原路返回的中点是速度反号的尖点，
    加速度闸门会被顶穿，而且画面上是生硬的倒放感。
    """
    c = _center(scene)
    ctr = _free_or(scene, rng, c)
    rx = 0.18 * (scene.hi[0] - scene.lo[0]) * rng.uniform(0.7, 1.0)
    rz = 0.18 * (scene.hi[2] - scene.lo[2]) * rng.uniform(0.7, 1.0)
    ph = rng.uniform(0, 2 * math.pi)
    T = ts[-1]
    pos, yaws = [], []
    for t in ts:
        a = 2 * math.pi * (t / T) + ph            # 恰好一圈 → 首尾重合
        p = (ctr[0] + rx * math.cos(a), ctr[1], ctr[2] + rz * math.sin(a))
        pos.append(p)
        # 朝向也走整周期，保证首尾 rotation 一致
        yaws.append(math.degrees(a) + 90.0)
    elevs = [3.0 * math.sin(2 * math.pi * t / T) for t in ts]   # 整周期 → 首尾同值
    return pos, yaws, elevs, [0.0] * len(ts)


def _gen_loop_pan(scene: Scene, rng: random.Random, ts: Sequence[float]):
    """Loop-原地摇镜：无位移，原地转整数圈，**首尾一致**，可 3D。"""
    p = _free_or(scene, rng, _center(scene))
    turns = rng.choice((1, 1, 2))
    T = ts[-1]
    pos = [p] * len(ts)
    yaws = [360.0 * turns * (t / T) for t in ts]
    # 俯仰同样走整周期，首尾归位（「可以是 3D 转圈」）
    elevs = [12.0 * math.sin(2 * math.pi * turns * t / T) for t in ts]
    return pos, yaws, elevs, [0.0] * len(ts)


def _gen_revisit_pan(scene: Scene, rng: random.Random, ts: Sequence[float]):
    """Revisit-原地摇镜：无位移，**首尾不一致**（location 相近、rotation 不同）。"""
    p = _free_or(scene, rng, _center(scene))
    T = ts[-1]
    total = rng.uniform(200.0, 320.0)             # 刻意不取 360 的整数倍
    pos = [p] * len(ts)
    yaws = [total * (t / T) for t in ts]
    elevs = _dither(rng, ts, 5.0, 2.1)
    return pos, yaws, elevs, [0.0] * len(ts)


def _gen_revisit_move(scene: Scene, rng: random.Random, ts: Sequence[float]):
    """Revisit-有位移：A→B→A→C，轨迹里包含旧地点，后续帧与之前有共视区域。"""
    c = _center(scene)
    a = _free_or(scene, rng, c)
    span = 0.22 * min(scene.hi[0] - scene.lo[0], scene.hi[2] - scene.lo[2])
    b = _free_or(scene, rng, vadd(a, (span, 0, 0)), near=a, radius=span)
    cc = _free_or(scene, rng, vadd(a, (0, 0, span)), near=a, radius=span)
    legs = [(a, b), (b, a), (a, cc)]              # A→B→A→C
    T = ts[-1]
    pos = []
    for t in ts:
        u = clamp(t / T, 0.0, 1.0) * len(legs)
        i = min(len(legs) - 1, int(u))
        f = u - i
        s, e = legs[i]
        pos.append((s[0] + (e[0] - s[0]) * f, s[1] + (e[1] - s[1]) * f,
                    s[2] + (e[2] - s[2]) * f))
    yaws = _path_yaws(pos)
    yaws = [y + d for y, d in zip(yaws, _dither(rng, ts, 9.0, 2.0))]
    return pos, yaws, [0.0] * len(ts), [0.0] * len(ts)


def _osc(axis: str):
    """Oscillating：沿某轴两点间往复，**至少重复 2 次以上**。"""
    def gen(scene: Scene, rng: random.Random, ts: Sequence[float]):
        p0 = _free_or(scene, rng, _center(scene))
        cycles = rng.choice((2.0, 3.0, 4.0))      # 「至少 2 次以上」
        amp = {"side": 3.2, "updown": 1.1, "fwd": 3.6}[axis] * rng.uniform(0.8, 1.2)
        T = ts[-1]
        dirv = {"side": (1.0, 0.0, 0.0), "updown": (0.0, 1.0, 0.0),
                "fwd": (0.0, 0.0, 1.0)}[axis]
        pos = []
        for t in ts:
            s = amp * math.sin(2 * math.pi * cycles * t / T)
            pos.append((p0[0] + dirv[0] * s, p0[1] + dirv[1] * s, p0[2] + dirv[2] * s))
        base = rng.uniform(-180, 180)
        yaws = [base + d for d in _dither(rng, ts, 7.0, 2.0)]
        elevs = _dither(rng, ts, 3.0, 2.2)
        return pos, yaws, elevs, [0.0] * len(ts)
    return gen


def _gen_stay(scene: Scene, rng: random.Random, ts: Sequence[float]):
    """Stay-完全静止：rotation、location **完全无变化**。

    这里刻意不叠任何扰动——68 号对这一类的字面要求就是「完全无变化」，
    画面里的运动应当来自场景动态物体（要求「画面中运动的部分要高于 5%–20% 以上」），
    那是布景的事，不是相机的事。
    """
    p = _free_or(scene, rng, _center(scene))
    yaw = rng.uniform(-180, 180)
    n = len(ts)
    return [p] * n, [yaw] * n, [0.0] * n, [0.0] * n


def _gen_vehicle(scene: Scene, rng: random.Random, ts: Sequence[float]):
    """车载视角：贴地高度带 + 沿走廊的长距离行进，路线可套用其他轨迹。"""
    c = _center(scene)
    a = _free_or(scene, rng, c)
    y = scene.ground_y + scene.eye_lo + 0.15 * (scene.eye_hi - scene.eye_lo)
    a = (a[0], y, a[2])
    span = 0.42 * min(scene.hi[0] - scene.lo[0], scene.hi[2] - scene.lo[2])
    head = rng.uniform(-math.pi, math.pi)
    b = (a[0] + span * math.sin(head), y, a[2] + span * math.cos(head))
    mid = vadd(vscale(vadd(a, b), 0.5), (0.0, 0.0, 0.0))
    perp = vunit((-(b[2] - a[2]), 0.0, b[0] - a[0]))
    ctrl = vadd(mid, vscale(perp, 0.12 * vdist(a, b) * rng.choice((-1.0, 1.0))))
    T = ts[-1]
    pos = [_bezier(a, ctrl, b, t / T) for t in ts]
    pos = [(p[0], y, p[2]) for p in pos]          # 载具不上下浮
    yaws = _path_yaws(pos)
    yaws = [yv + d for yv, d in zip(yaws, _dither(rng, ts, 5.0, 2.1))]
    return pos, yaws, [0.0] * len(ts), [0.0] * len(ts)


def _gen_heli(scene: Scene, rng: random.Random, ts: Sequence[float]):
    """直升机视角：高空俯瞰，缓慢平移 + 轻微盘旋。"""
    c = _center(scene)
    top = min(scene.hi[1], scene.ground_y + scene.eye_hi)
    y = max(scene.ground_y + scene.eye_lo, top - 0.15 * (top - scene.ground_y))
    a = _free_or(scene, rng, c)
    a = (a[0], y, a[2])
    span = 0.3 * min(scene.hi[0] - scene.lo[0], scene.hi[2] - scene.lo[2])
    head = rng.uniform(-math.pi, math.pi)
    T = ts[-1]
    pos, yaws, elevs = [], [], []
    for t in ts:
        u = t / T
        p = (a[0] + span * u * math.sin(head) + 2.0 * math.sin(2 * math.pi * u * 2),
             y, a[2] + span * u * math.cos(head) + 2.0 * math.cos(2 * math.pi * u * 2))
        pos.append(p)
        yaws.append(math.degrees(head) + 20.0 * math.sin(2 * math.pi * u))
        elevs.append(-28.0 + 6.0 * math.sin(2 * math.pi * u * 1.5))   # 俯视
    return pos, yaws, elevs, [0.0] * len(ts)


def _gen_superman(scene: Scene, rng: random.Random, ts: Sequence[float]):
    """超人视角：高速穿行 + 高度变化 + 视角大幅摆动。"""
    c = _center(scene)
    a = _free_or(scene, rng, c)
    span = 0.45 * min(scene.hi[0] - scene.lo[0], scene.hi[2] - scene.lo[2])
    head = rng.uniform(-math.pi, math.pi)
    lo = scene.ground_y + scene.eye_lo
    hi = scene.ground_y + scene.eye_hi
    T = ts[-1]
    pos, yaws, elevs = [], [], []
    for t in ts:
        u = t / T
        y = lo + (hi - lo) * (0.5 + 0.45 * math.sin(2 * math.pi * u * 1.5))
        pos.append((a[0] + span * u * math.sin(head), y,
                    a[2] + span * u * math.cos(head)))
        yaws.append(math.degrees(head) + 30.0 * math.sin(2 * math.pi * u * 1.2))
        elevs.append(14.0 * math.sin(2 * math.pi * u * 0.9))
    return pos, yaws, elevs, [0.0] * len(ts)


def _gen_dolly(scene: Scene, rng: random.Random, ts: Sequence[float]):
    """特殊运镜·远近转换：缓慢旋转中near/far 来回变化。"""
    ctr = _free_or(scene, rng, _center(scene))
    near, far = 3.0, 16.0
    T = ts[-1]
    pos, yaws, elevs = [], [], []
    base = rng.uniform(-math.pi, math.pi)
    for t in ts:
        u = t / T
        r = near + (far - near) * (0.5 - 0.5 * math.cos(2 * math.pi * u * 1.5))
        a = base + 0.6 * math.pi * u                 # 缓慢旋转
        p = (ctr[0] + r * math.sin(a), ctr[1], ctr[2] + r * math.cos(a))
        pos.append(p)
        y, e = _look_at(p, ctr)
        yaws.append(y)
        elevs.append(e)
    return pos, yaws, elevs, [0.0] * len(ts)


def _gen_orbit_subject(scene: Scene, rng: random.Random, ts: Sequence[float]):
    """特殊运镜·主体环绕：绕主体转，带拉近拉远与少量位移（主体不出画）。"""
    ctr = _free_or(scene, rng, _center(scene))
    T = ts[-1]
    turns = rng.uniform(0.7, 1.4)
    pos, yaws, elevs = [], [], []
    for t in ts:
        u = t / T
        r = 4.0 + 2.2 * math.sin(2 * math.pi * u * 1.2)
        a = 2 * math.pi * turns * u
        p = (ctr[0] + r * math.sin(a), ctr[1] + 0.8 * math.sin(2 * math.pi * u),
             ctr[2] + r * math.cos(a))
        pos.append(p)
        y, e = _look_at(p, ctr)
        yaws.append(y)
        elevs.append(e)
    return pos, yaws, elevs, [0.0] * len(ts)


def _gen_closeup(scene: Scene, rng: random.Random, ts: Sequence[float]):
    """特殊运镜·人物特写：近距离，**相机与人物不同时静止**。

    生成器只能保证相机侧不静止；人物侧的动由场景布景负责，写进 plan 供采集侧核对。
    """
    ctr = _free_or(scene, rng, _center(scene))
    T = ts[-1]
    pos, yaws, elevs = [], [], []
    base = rng.uniform(-math.pi, math.pi)
    for t in ts:
        u = t / T
        r = 1.8 + 0.5 * math.sin(2 * math.pi * u * 1.6)
        a = base + 0.5 * math.sin(2 * math.pi * u * 0.8)
        p = (ctr[0] + r * math.sin(a), ctr[1] + 0.25 * math.sin(2 * math.pi * u * 1.3),
             ctr[2] + r * math.cos(a))
        pos.append(p)
        y, e = _look_at(p, ctr)
        yaws.append(y)
        elevs.append(e)
    return pos, yaws, elevs, [0.0] * len(ts)


# ---------------------------------------------------------------- 类目表
# 比例与时长比逐字取自 docs/68 §3.3 / §3.4。navigate 合计 98%——那 2% 差额是
# 需求文档自身的问题，已进 docs/70 §9 第 6 条待确认，这里如实保留不硬凑。

NAVIGATE: List[SfmClass] = [
    SfmClass("Open-强交互-战斗", 0.20, "navigate", {60.0: 2, 180.0: 1},
             lambda s, r, t: _gen_open(s, r, t, combat=True)),
    SfmClass("Open-环境交互", 0.12, "navigate", {60.0: 2, 180.0: 1}, _gen_open),
    SfmClass("Loop-有位移", 0.12, "navigate", {60.0: 1, 180.0: 1}, _gen_loop_move),
    SfmClass("Loop-原地摇镜", 0.04, "navigate", {60.0: 1}, _gen_loop_pan),
    SfmClass("Revisit-有位移", 0.04, "navigate", {60.0: 1, 180.0: 1}, _gen_revisit_move),
    SfmClass("Revisit-原地摇镜", 0.04, "navigate", {60.0: 1}, _gen_revisit_pan),
    SfmClass("Oscillating-侧向", 0.04, "navigate", {60.0: 2, 180.0: 1}, _osc("side")),
    SfmClass("Oscillating-上下", 0.04, "navigate", {60.0: 2, 180.0: 1}, _osc("updown")),
    SfmClass("Oscillating-前后", 0.04, "navigate", {60.0: 2, 180.0: 1}, _osc("fwd")),
    SfmClass("Stay-完全静止", 0.02, "navigate", {60.0: 1}, _gen_stay, stationary=True),
    SfmClass("车载视角", 0.12, "navigate", {60.0: 2, 180.0: 1}, _gen_vehicle),
    SfmClass("直升机视角", 0.08, "navigate", {60.0: 2, 180.0: 1}, _gen_heli),
    SfmClass("超人视角", 0.08, "navigate", {60.0: 2, 180.0: 1}, _gen_superman),
]

SPECIAL: List[SfmClass] = [
    SfmClass("远近转换", 0.08, "special", {60.0: 1}, _gen_dolly),
    SfmClass("主体环绕", 0.08, "special", {60.0: 1}, _gen_orbit_subject),
    SfmClass("人物特写", 0.04, "special", {60.0: 1}, _gen_closeup),
]

ALL_CLASSES: List[SfmClass] = NAVIGATE + SPECIAL
BY_NAME: Dict[str, SfmClass] = {c.name: c for c in ALL_CLASSES}

#: 上传路径的「运镜种类」目录名白名单（docs/68 §8，QC 侧同名断言）
CLASS_NAMES: List[str] = [c.name for c in ALL_CLASSES]


# ---------------------------------------------------------------- 计划与自检

@dataclass
class SfmPlan:
    seed: int
    scene: Scene
    cls: SfmClass
    duration: float
    positions: List[Vec3] = field(default_factory=list)
    quats: List[Tuple[float, float, float, float]] = field(default_factory=list)
    eulers: List[Vec3] = field(default_factory=list)
    speeds: List[Vec3] = field(default_factory=list)
    physics: Optional[met.Physics] = None
    loop_closure_m: float = 0.0
    loop_closure_deg: float = 0.0
    osc_cycles: float = 0.0
    revisit_min_m: float = 0.0
    window_turn_deg: float = 0.0
    dither_ok: bool = True
    max_push_m: float = 0.0
    ok: bool = False
    reasons: List[str] = field(default_factory=list)

    @property
    def n_frames(self) -> int:
        return len(self.positions)

    def to_plan_dict(self) -> Dict:
        return {
            "version": "1.0",
            "mode": "sfm",
            "seed": self.seed,
            "scene": self.scene.name,
            "scene_kind": self.scene.kind,
            "traj_class": self.cls.name,
            "traj_group": self.cls.group,
            "duration_s": self.duration,
            "fps": FPS,
            "frames": self.n_frames,
            "selfcheck": {
                "loop_closure_m": round(self.loop_closure_m, 4),
                "loop_closure_deg": round(self.loop_closure_deg, 3),
                "osc_cycles": round(self.osc_cycles, 3),
                "revisit_min_m": round(self.revisit_min_m, 4),
                "window_turn_deg": round(self.window_turn_deg, 3),
                "dither_ok": self.dither_ok,
                "max_push_m": round(self.max_push_m, 4),
                "ok": self.ok,
                "reasons": list(self.reasons),
            },
            "trajgen": {
                "knot_s": KNOT_S,
                "notes": "欧拉角按 Unity eulerAngles 口径导出（顺规待确认，docs/72 C3）",
            },
        }


#: 拓扑自检阈值
#: 扰动（§3.2「非硬性要求」）：跨 2s 的方向夹角低于此值即提示「偏直」，**不判废**
DITHER_MIN_TURN_DEG = 6.0
LOOP_POS_TOL_M = 0.60          # Loop 首尾位置差
LOOP_ROT_TOL_DEG = 8.0         # Loop 首尾朝向差
REVISIT_PAN_MIN_DEG = 25.0     # Revisit-原地摇镜首尾朝向**必须**不同
REVISIT_RETURN_M = 2.0         # Revisit-有位移「回到旧地点」的判定半径
OSC_MIN_CYCLES = 2.0           # 「至少重复 2 次以上」
STRAIGHT_MAX_S = 2.0           # 扰动：单方向不得持续 2s 以上
STAY_EPS_M = 1e-9


def _worst_window_turn_deg(positions: Sequence[Vec3], dt: float,
                           window_s: float = STRAIGHT_MAX_S) -> float:
    """最「直」的 2s 窗内，运动方向总共转了多少度（docs/68 §3.2 扰动的度量）。

    初版按「相邻帧方向变化 < 1° 即算直行」累计连续时长——那是错的度量：30fps 下
    一条平缓弧线每帧只转零点几度，整条片子都会被判成「单方向」。这里改成看**跨 2s
    的方向夹角**，直行时接近 0，有扰动时明显大于 0，与规格描述的现象一致。
    """
    n = len(positions)
    k = max(1, int(round(window_s / dt)))
    if n < 2 * k + 2:
        return 180.0
    def vel(i):
        d = vsub(positions[i + 1], positions[i])
        m = math.sqrt(d[0] ** 2 + d[1] ** 2 + d[2] ** 2)
        return (d[0] / m, d[1] / m, d[2] / m) if m > 1e-9 else None
    worst = 180.0
    for i in range(0, n - k - 1):
        a, b = vel(i), vel(i + k)
        if a is None or b is None:
            continue
        cos = max(-1.0, min(1.0, a[0] * b[0] + a[1] * b[1] + a[2] * b[2]))
        worst = min(worst, math.degrees(math.acos(cos)))
    return worst


def _osc_cycles(positions: Sequence[Vec3], axis: int) -> float:
    """沿某轴的过零次数 / 2 = 往复周期数。"""
    vals = [p[axis] for p in positions]
    mean = sum(vals) / len(vals)
    signs = [1 if v >= mean else -1 for v in vals]
    crossings = sum(1 for a, b in zip(signs, signs[1:]) if a != b)
    return crossings / 2.0


def _selfcheck(plan: SfmPlan) -> None:
    """按类目语义 + 运动学阈值自检。不通过就 reasons 里写清楚是哪一条。"""
    reasons: List[str] = []
    pos, quats = plan.positions, plan.quats
    dt = 1.0 / FPS
    name = plan.cls.name

    # --- 运动学（与 QC 同口径）---
    plan.physics = met.physics(pos, quats, dt)
    if not plan.cls.stationary:
        reasons += plan.physics.violations()

    # --- 扰动（docs/68 §3.2 明写「非硬性要求」，因此只报不判）---
    plan.window_turn_deg = 0.0 if plan.cls.stationary else _worst_window_turn_deg(pos, dt)
    plan.dither_ok = plan.cls.stationary or plan.window_turn_deg >= DITHER_MIN_TURN_DEG

    # --- 拓扑 ---
    plan.loop_closure_m = vdist(pos[0], pos[-1])
    plan.loop_closure_deg = quat_angle_deg(quats[0], quats[-1])

    if name.startswith("Loop"):
        if plan.loop_closure_m > LOOP_POS_TOL_M:
            reasons.append("Loop 首尾位置差 %.3fm > %.2fm" % (plan.loop_closure_m, LOOP_POS_TOL_M))
        if plan.loop_closure_deg > LOOP_ROT_TOL_DEG:
            reasons.append("Loop 首尾朝向差 %.2f° > %.1f°" % (plan.loop_closure_deg, LOOP_ROT_TOL_DEG))
    if name == "Revisit-原地摇镜":
        if plan.loop_closure_deg < REVISIT_PAN_MIN_DEG:
            reasons.append("Revisit-原地摇镜首尾朝向差 %.2f° < %.1f°（要求首尾**不**一致）"
                           % (plan.loop_closure_deg, REVISIT_PAN_MIN_DEG))
    if name == "Revisit-有位移":
        # 后半程必须回到过前半程去过的地方
        half = len(pos) // 2
        best = min((vdist(p, q) for q in pos[half:] for p in pos[:half // 2]), default=1e9)
        plan.revisit_min_m = best
        if best > REVISIT_RETURN_M:
            reasons.append("Revisit 未回到旧地点（最近 %.2fm > %.1fm）" % (best, REVISIT_RETURN_M))
    if name.startswith("Oscillating"):
        axis = {"Oscillating-侧向": 0, "Oscillating-上下": 1, "Oscillating-前后": 2}[name]
        plan.osc_cycles = _osc_cycles(pos, axis)
        if plan.osc_cycles < OSC_MIN_CYCLES:
            reasons.append("往复仅 %.1f 次 < %.0f 次" % (plan.osc_cycles, OSC_MIN_CYCLES))
    if name == "Stay-完全静止":
        moved = max(vdist(pos[0], p) for p in pos)
        rot = max(quat_angle_deg(quats[0], q) for q in quats)
        if moved > STAY_EPS_M or rot > 1e-6:
            reasons.append("Stay 要求 location/rotation 完全无变化，实测位移 %.3g m、转动 %.3g°"
                           % (moved, rot))

    plan.reasons = reasons
    plan.ok = not reasons


# ---------------------------------------------------------------- 生成主流程

def generate(scene: Scene, seed: int, cls_name: Optional[str] = None,
             duration: Optional[float] = None) -> SfmPlan:
    """生成一条 68 号 clip 计划。类目未指定时按配额随机抽。"""
    rng = random.Random(seed)
    if cls_name is None:
        pool = ALL_CLASSES
        cls = rng.choices(pool, weights=[c.quota for c in pool], k=1)[0]
    else:
        if cls_name not in BY_NAME:
            raise KeyError("未知轨迹类目：%r（合法值见 CLASS_NAMES）" % cls_name)
        cls = BY_NAME[cls_name]
    dur = duration if duration is not None else cls.pick_duration(rng)
    if dur not in DURATIONS:
        raise ValueError("时长 %.1fs 不在白名单 %s（docs/72 裁决）" % (dur, DURATIONS))

    ts = knot_times(dur, KNOT_S)
    pos, yaw, elev, roll = cls.gen(scene, rng, ts)

    # 静止类不能过自由壳投影：push_out 会把它推离原位，破坏「完全无变化」
    if cls.stationary:
        from .sampler import SmoothTrack
        n = int(round(dur * FPS))
        plan = SfmPlan(seed=seed, scene=scene, cls=cls, duration=dur)
        plan.positions = [pos[0]] * n
        q = _quat(yaw[0], elev[0], roll[0])
        plan.quats = [q] * n
        plan.eulers = [(elev[0], yaw[0], roll[0])] * n
        plan.speeds = [(0.0, 0.0, 0.0)] * n
        _selfcheck(plan)
        return plan

    smooth, push = smooth_from_knots(pos, yaw, elev, roll, scene, dur, knot_s=KNOT_S)
    n = int(round(dur * FPS))
    times = [i / FPS for i in range(n)]
    positions, quats, eulers, speeds = frames_from_times(smooth, times, FPS)

    plan = SfmPlan(seed=seed, scene=scene, cls=cls, duration=dur)
    plan.positions, plan.quats, plan.eulers, plan.speeds = positions, quats, eulers, speeds
    plan.max_push_m = push
    _selfcheck(plan)
    return plan


def _quat(yaw: float, elev: float, roll: float):
    from .mathutil import quat_from_yaw_elev_roll
    return quat_from_yaw_elev_roll(yaw, elev, roll)


# ---------------------------------------------------------------- 配额账本

class SfmQuota:
    """语料级配额记账：按类目累计条数，报出与目标比例的偏差。"""

    def __init__(self):
        self.counts: Dict[str, int] = {c.name: 0 for c in ALL_CLASSES}

    def add(self, cls_name: str) -> None:
        self.counts[cls_name] = self.counts.get(cls_name, 0) + 1

    def report(self) -> List[Dict]:
        out = []
        for group in ("navigate", "special"):
            members = [c for c in ALL_CLASSES if c.group == group]
            total = sum(self.counts[c.name] for c in members)
            for c in members:
                actual = (self.counts[c.name] / total) if total else 0.0
                out.append({"class": c.name, "group": group,
                            "count": self.counts[c.name],
                            "target": c.quota, "actual": round(actual, 4),
                            "delta": round(actual - c.quota, 4)})
        return out

    def next_class(self, rng: random.Random) -> SfmClass:
        """挑当前欠得最多的类目——比纯随机抽更快收敛到目标配比。"""
        rep = {r["class"]: r for r in self.report()}
        worst = min(ALL_CLASSES, key=lambda c: rep[c.name]["delta"])
        return worst


def plan_clip(scene: Scene, seed: int, cls_name: Optional[str] = None,
              duration: Optional[float] = None, max_tries: int = 8
              ) -> Tuple[SfmPlan, int]:
    """自检不过就换种子重排，返回 (计划, 用掉的尝试数)。

    与编排器「QC 不过 → seed+1 重录」同一条纪律（docs/78 §4.3）：**不修数据**，
    重生成。狭窄场景（corridor / indoor_rooms）里自由壳投影会把路径推出弧线，
    偶发顶穿 jerk 预算——换种子比在生成侧加特例便宜得多。
    """
    last = None
    for i in range(max_tries):
        p = generate(scene, seed + i, cls_name=cls_name, duration=duration)
        if p.ok:
            return p, i + 1
        last = p
    return last, max_tries

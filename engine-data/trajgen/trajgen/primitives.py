"""MC 原语库：10 族 23 类（docs/71 §4 表）。

docs/70 曾称「69 的 23 类运镜复用 68 的模板」，docs/71 §4 已判定**不成立**——
两套类目几乎不相交，本模块即新写的那套 MC 原语库。

每个原语实现为「局部归一化进度 u∈[0,1] + 绝对时刻 t → 位姿」的解析函数：
- 位置跨度由排产给的 `extent = 目标速度 × 时长` 决定（段均速因此是构造性的）；
- 朝向策略由原语自带（look-at 锚点 / 定点摇转 / 跟随主体）；
- 段界的连续性与限幅由 `sampler.py` 的节点五次 B 样条 + 转速限幅器统一保证，
  原语本身不需要管缓入缓出。

粉色（每条必含）= 平移 6 类；其余为非粉色，按语料级配额由 `planner.py` 排产。
"""

import math
import random
from dataclasses import dataclass, field
from typing import Callable, Dict, List, Optional, Sequence, Tuple

from .anchors import Anchor, Placement, Subject
from .mathutil import (Vec3, clamp, smoothstep5, vadd, vdist, vscale, vsub,
                       vunit, wrap180, yaw_elev_from_forward)
from .scene import Scene

#: 族 → 类
FAMILIES: Dict[str, List[str]] = {
    "平移": ["translate_up", "translate_down", "translate_forward",
             "translate_back", "translate_left", "translate_right"],
    "摇动": ["pan_up", "pan_down", "pan_left", "pan_right"],
    "旋转": ["roll_cw", "roll_ccw"],
    "环绕": ["orbit_left", "orbit_right", "orbit_up", "orbit_down"],
    "硬跟随": ["follow_bind", "follow_lag"],
    "目标追踪": ["target_switch"],
    "螺旋升降": ["helix"],
    "FPV无人机": ["fpv_drone"],
    "固定静止": ["hold_static"],
    "循环路径": ["loop_path"],
}

#: 类 → 族
FAMILY_OF: Dict[str, str] = {c: f for f, cs in FAMILIES.items() for c in cs}

#: 粉色（每条必含）
PINK: List[str] = FAMILIES["平移"]

#: 非粉色类目的语料级配额（docs/71 §4 表「配额（语料级）」列）——
#: 口径：**含该类 ≥1 个 move 的相机轨道占全语料相机轨道的比例**（≤ 表示上限）
QUOTA: Dict[str, Tuple[str, float]] = {
    "pan_up": ("=", 0.25), "pan_down": ("=", 0.25),
    "pan_left": ("=", 0.25), "pan_right": ("=", 0.25),
    "roll_cw": ("<=", 0.02), "roll_ccw": ("<=", 0.02),
    "orbit_left": ("=", 0.30), "orbit_right": ("=", 0.30),
    "orbit_up": ("=", 0.30), "orbit_down": ("=", 0.30),
    "follow_bind": ("=", 0.05), "follow_lag": ("=", 0.05),
    "target_switch": ("=", 0.05),
    "helix": ("=", 0.05),
    "fpv_drone": ("=", 0.05),
    "hold_static": ("<=", 0.03),
    "loop_path": ("=", 0.05),
}

#: 位置基本不动的类目（速度目标取近零，排产时不吃 extent）
STATIONARY = {"pan_up", "pan_down", "pan_left", "pan_right",
              "roll_cw", "roll_ccw", "hold_static"}

#: 位移量由排产的 `extent` 决定的类目 —— 即**推进速率可控**的那些。
#: `follow_*` 与 `target_switch` 不在其中：它们的位置由动态主体给，跟随主体走多慢就多慢。
#: 第四路的**标称路径**只能用这一组：它的时间律由外部的 `ρ·v_g` 排，
#: 标称自身速率若不可控，`τ′=ρ·v_g/v_标称` 会在主体停步处发散
#: （实测标称头 1s 只走 0.002m，τ 直接跳 1.7s，v₄ 出现假尖峰并被 QC 判成多一次变速）。
EXTENT_DRIVEN = set(FAMILIES["平移"]) | set(FAMILIES["环绕"]) | {
    "helix", "fpv_drone", "loop_path"}

#: 位置会动的全部类目（guide 路只能用这些）。guide 一旦有静止段，
#: `ρ=v₄/v_g` 在那里分母趋零、样本被 QC 丢弃，段界处的少量噪声样本会被
#: 「变速事件」检测误判成额外的变速（实测在 pan 段边缘冒出假事件）。
MOVING = {c for c in FAMILY_OF if c not in STATIONARY}

#: 类目允许的时长区间（s）
DURATION_RANGE: Dict[str, Tuple[float, float]] = {
    "hold_static": (3.0, 10.0),
    "roll_cw": (2.5, 6.0), "roll_ccw": (2.5, 6.0),
    "target_switch": (8.0, 24.0),
    "loop_path": (8.0, 20.0),
}
DEFAULT_DURATION = (3.0, 14.0)


#: 摇动/运镜的俯仰硬带（度）。此前是 ±70——上摇整段只剩天花板、下摇整段只剩地板，
#: 交付复核的目检把「多段整帧只有天花板、无任何场景元素和动态物体」判为 docs/69
#: §8.1 打回项，根因就在这里（QC 无对应数值规则，全靠目检才发现）。
#: 室内场景 +28° 已能带到货架顶与顶棚桁架。
#: 下限首版取 -45°，目检又抓到反向的同一问题：俯视 45° + 近距离 = **整帧纯地板**
#: （只剩地面标线，无场景元素无动态物体），与「空看天花板」是同一条打回项的两个方向。
#: -30° 仍留得住明显的下摇动作，但画面里总能带到货架脚/护栏/人腿。
#: 上限可按场景覆盖（env `DIOGENES_ELEV_MAX`）：低顶棚 + 稀疏内容的场景
#: （实测 Robotics-Warehouse：9m 层高、745 renderers）在 +28° 下天花板占画面上半，
#: 而 Factory（12711 renderers、10m+ 层高）同样的值没问题——**这是场景属性，
#: 不是全局常量该背的**。故留一个覆盖口，场景 onboarding 时定。
#: 下限同样留覆盖口（env `DIOGENES_ELEV_MIN`）。默认 -30° 是室内口径：
#: 那里俯拍总能带到货架脚/护栏/人腿。**室外开阔场景反过来**——CARLA 城市批
#: 目检有相当比例的帧 55–70% 是柏油路面（大停车场尤甚），因为俯角一大，
#: 画面里除了地面什么都没有。上下限是同一类场景属性，处理方式也该同一种。
ELEV_LIMIT = (float(__import__('os').environ.get('DIOGENES_ELEV_MIN', '-30.0')),
              float(__import__('os').environ.get('DIOGENES_ELEV_MAX', '28.0')))


@dataclass
class Pose:
    p: Vec3
    yaw: float
    elev: float
    roll: float = 0.0


@dataclass
class Move:
    cls: str
    t0: float
    t1: float
    seg: int
    params: Dict = field(default_factory=dict)

    @property
    def family(self) -> str:
        return FAMILY_OF[self.cls]

    @property
    def duration(self) -> float:
        return self.t1 - self.t0

    def to_dict(self) -> Dict:
        return {"t0": round(self.t0, 4), "t1": round(self.t1, 4), "cls": self.cls,
                "params": dict(self.params, family=self.family, seg=self.seg)}


@dataclass
class Ctx:
    scene: Scene
    anchor: Anchor
    subjects: List[Subject]
    placement: Placement
    rng: random.Random
    duration: float
    #: 相机与锚点的工作环带（m）。超出 r_max 后共视按 min 口径迅速崩塌
    #: （docs/71 §5「基线角-共视衰减」），所以位移类原语要吃径向预算。
    r_min: float = 2.2
    r_max: float = 13.0
    #: 方位扇区（度）：docs/71 §5「三路收在 ≤90–120° 弧扇区内」的落地。
    #: 每条里有两路被锁进共享扇区（保住至少一对共视），第三路 `sector_half=180` 自由 ——
    #: 闸门本来就是「前三路至少一对成立」，不必三路全锁，多圈环绕的花样也就保住了。
    sector_center_deg: float = 0.0
    sector_half_deg: float = 180.0


# ---------------------------------------------------------------- 工具

def look_at(p: Sequence[float], target: Sequence[float]) -> Tuple[float, float]:
    return yaw_elev_from_forward(vsub(target, p))


def _basis(yaw: float) -> Tuple[Vec3, Vec3]:
    """由 yaw 给出水平 forward / right 单位向量（Unity 左手系，y 向上）。"""
    r = math.radians(yaw)
    fwd = (math.sin(r), 0.0, math.cos(r))
    right = (math.cos(r), 0.0, -math.sin(r))
    return fwd, right


def _ease_nonuniform(u: float, amp: float) -> float:
    """把匀速进度扭成非匀速（docs/71 §4「禁匀速」），保持 u(0)=0、u(1)=1、两端导数连续。"""
    return clamp(u + amp * math.sin(2.0 * math.pi * u) / (2.0 * math.pi), 0.0, 1.0)


def _clamp_eye(scene: Scene, y: float) -> float:
    return clamp(y, scene.ground_y + 0.6, scene.ground_y + 12.0)


def azimuth_of(ctx: Ctx, p: Sequence[float], t: float) -> float:
    c = ctx.anchor.at(t)
    return math.degrees(math.atan2(p[2] - c[2], p[0] - c[0]))


def sector_slack(ctx: Ctx, p0: Sequence[float], t: float) -> Tuple[float, float]:
    """当前方位到扇区两侧边界的余量（度，正/负方向）。"""
    if ctx.sector_half_deg >= 180.0:
        return 180.0, 180.0
    off = wrap180(azimuth_of(ctx, p0, t) - ctx.sector_center_deg)
    return (max(0.0, ctx.sector_half_deg - off), max(0.0, ctx.sector_half_deg + off))


def azimuth_budget(ctx: Ctx, p0: Sequence[float], d: Vec3, L: float, t: float) -> float:
    """把位移跨度砍到「不冲出方位扇区」的长度（切向分量吃预算）。"""
    if ctx.sector_half_deg >= 180.0:
        return L
    c = ctx.anchor.at(t)
    rel = (p0[0] - c[0], 0.0, p0[2] - c[2])
    r0 = math.hypot(rel[0], rel[2])
    if r0 < 1e-6:
        return L
    u = (rel[0] / r0, rel[2] / r0)
    tang = -d[0] * u[1] + d[2] * u[0]          # 切向分量（右手绕 +y）
    if abs(tang) < 1e-3:
        return L
    pos_slack, neg_slack = sector_slack(ctx, p0, t)
    slack = math.radians(pos_slack if tang > 0 else neg_slack)
    return max(0.4, min(L, slack * r0 / abs(tang)))


def radial_budget(ctx: Ctx, p0: Sequence[float], d: Vec3, L: float, t: float) -> float:
    """把位移跨度砍到「不冲出锚点工作环带 [r_min, r_max]」的长度。

    自由生长的平移/FPV 前冲能在 60s 里把相机推出去几十米，min 口径共视随基线角迅速塌掉——
    这是 docs/71 §5 两个真耦合点里的第二个，结构性解法就是把相机收在环带内。
    """
    c = ctx.anchor.at(t)
    rel = (p0[0] - c[0], 0.0, p0[2] - c[2])
    r0 = math.hypot(rel[0], rel[2])
    if r0 < 1e-6:
        return L
    u = (rel[0] / r0, 0.0, rel[2] / r0)
    radial = d[0] * u[0] + d[2] * u[2]          # 正=离心，负=向心
    if radial > 1e-3:
        return max(0.4, min(L, (ctx.r_max - r0) / radial))
    if radial < -1e-3:
        return max(0.4, min(L, (r0 - ctx.r_min) / (-radial)))
    return L


# ---------------------------------------------------------------- 原语构造

Runner = Callable[[float, float], Pose]


def build(move: Move, ctx: Ctx, start: Pose) -> Runner:
    """按类目构造 move 的位姿函数。`start` 为上一 move 的末位姿（首个 move 为初始位姿）。"""
    return _BUILDERS[move.cls](move, ctx, start)


#: 粉色平移的预算地板（m）。诚实性契约（QC plan.pink_honesty，#597）要求声称轴向
#: 净位移 ≥0.8m；环带/扇区预算把 L 砍到 0.4 时这条平移**从声称那一刻起就不可能诚实**
#: （实测 38/144 窗挂在这 + 样条稀释）。1.2m 对几米宽的环带是可以吃下的越界，
#: 共视仍由生成侧自检的共视闸门兜底。
PINK_L_FLOOR = 1.2


def _translate(axis: str):
    def builder(move: Move, ctx: Ctx, start: Pose) -> Runner:
        L = float(move.params.get("extent", 1.0))
        # 平移基底必须取**对锚 look-at 的 yaw**，不是上一 move 末位 yaw：
        # 声称轴向（QC plan.pink_honesty 的 _yaw_basis）按窗首帧相机 yaw 判，
        # 而本 runner 全程 look-at 锚点——窗首帧导出的 yaw 就是 look-at 方向。
        # 旧写法沿 start.yaw 平移，上一 move 是 pan 时两者相差可达 90°+，
        # 声称的 forward 实测走的是 left（36/144 窗挂在轴错位，实测中位 ~60°）。
        yaw0, _ = look_at(start.p, ctx.anchor.at(move.t0))
        fwd, right = _basis(yaw0)
        d = {"forward": fwd, "back": vscale(fwd, -1.0),
             "right": right, "left": vscale(right, -1.0),
             "up": (0.0, 1.0, 0.0), "down": (0.0, -1.0, 0.0)}[axis]
        if axis in ("up", "down"):
            L = min(L, 4.0)          # 竖直平移不至于飞出视高带太远
        else:
            L = max(PINK_L_FLOOR,
                    radial_budget(ctx, start.p, d, L, move.t0))
            L = max(PINK_L_FLOOR,
                    azimuth_budget(ctx, start.p, d, L, move.t0))
        move.params["extent_used"] = round(L, 3)

        def run(u: float, t: float) -> Pose:
            p = vadd(start.p, vscale(d, L * u))
            p = (p[0], _clamp_eye(ctx.scene, p[1]), p[2])
            yaw, elev = look_at(p, ctx.anchor.at(t))
            return Pose(p, yaw, elev, start.roll * (1.0 - u))
        return run
    return builder


def _pan(axis: str, sign: float):
    def builder(move: Move, ctx: Ctx, start: Pose) -> Runner:
        amp = move.params.setdefault("amp_deg", ctx.rng.uniform(10.0, 90.0))

        def run(u: float, t: float) -> Pose:
            w = smoothstep5(u)
            if axis == "yaw":
                return Pose(start.p, start.yaw + sign * amp * w, start.elev, start.roll)
            return Pose(start.p, start.yaw,
                        clamp(start.elev + sign * amp * w, ELEV_LIMIT[0], ELEV_LIMIT[1]),
                        start.roll)
        return run
    return builder


def _roll(sign: float):
    def builder(move: Move, ctx: Ctx, start: Pose) -> Runner:
        amp = move.params.setdefault("roll_deg", ctx.rng.uniform(5.0, 15.0))

        def run(u: float, t: float) -> Pose:
            # 去回一个来回：sin 型，两端 roll=0，中途达 ±amp
            w = math.sin(math.pi * u)
            yaw, elev = look_at(start.p, ctx.anchor.at(t))
            return Pose(start.p, yaw, elev, sign * amp * w)
        return run
    return builder


def _orbit(vertical: bool, sign_hint: Optional[int]):
    def builder(move: Move, ctx: Ctx, start: Pose) -> Runner:
        c0 = ctx.anchor.at(move.t0)
        rel = vsub(start.p, c0)
        r0 = max(1.5, math.hypot(rel[0], rel[2]) if not vertical else vdist(start.p, c0))
        sign = sign_hint if sign_hint is not None else ctx.placement.orbit_sign
        L = float(move.params.get("extent", 1.0))
        # 角跨度 = 弧长 / 半径，夹在 30° 与 3 圈之间，再夹进方位扇区余量
        dtheta = clamp(L / r0, math.radians(30.0), 6.0 * math.pi)
        if not vertical:
            pos_slack, neg_slack = sector_slack(ctx, start.p, move.t0)
            dtheta = min(dtheta, math.radians(pos_slack if sign > 0 else neg_slack))
            dtheta = max(dtheta, math.radians(8.0))
        ecc = move.params.setdefault("ellipse", ctx.rng.uniform(0.0, 0.25))
        nonu = move.params.setdefault("nonuniform", ctx.rng.uniform(0.15, 0.45))
        move.params["radius_m"] = round(r0, 3)
        move.params["sweep_deg"] = round(math.degrees(dtheta), 2)
        th0 = math.atan2(rel[2], rel[0])
        y0 = start.p[1]

        phi0 = math.asin(clamp((start.p[1] - c0[1]) / max(1e-6, r0 * 0.7), -0.9, 0.9))

        def run(u: float, t: float) -> Pose:
            uu = _ease_nonuniform(u, nonu)
            th = th0 + sign * dtheta * uu
            # 椭圆按 θ0 处归一：否则 u=0 的半径就已偏离起点半径，段界位置直接跳变
            r = r0 * (1.0 + ecc * math.cos(2.0 * (th - th0))) / (1.0 + ecc)
            c = ctx.anchor.at(t)
            if vertical:
                # 竖直平面翻滚轨道（车轮式）：绕锚点在「径向-竖直」平面内转
                ux, uz = math.cos(th0), math.sin(th0)
                phi = phi0 + sign * dtheta * uu
                rad = r * math.cos(phi)
                p = (c[0] + ux * rad,
                     _clamp_eye(ctx.scene, c[1] + r * math.sin(phi) * 0.7),
                     c[2] + uz * rad)
            else:
                p = (c[0] + r * math.cos(th),
                     _clamp_eye(ctx.scene, y0 + 0.35 * math.sin(2.0 * math.pi * u)),
                     c[2] + r * math.sin(th))
            yaw, elev = look_at(p, c)
            return Pose(p, yaw, elev, 0.0)
        return run
    return builder


def _follow(lag: bool):
    def builder(move: Move, ctx: Ctx, start: Pose) -> Runner:
        subj = ctx.subjects[move.params.setdefault(
            "subject", ctx.rng.randrange(max(1, len(ctx.subjects))))] \
            if ctx.subjects else None
        tau = move.params.setdefault("lag_s", ctx.rng.uniform(0.2, 1.0)) if lag else 0.0
        s0 = subj.at(move.t0) if subj else ctx.anchor.at(move.t0)
        off = vsub(start.p, s0)
        # 跟随偏移过小时相机会贴到主体身上（实测到 r≈0.3m），画面里只剩一小块，
        # min 口径共视直接归零 —— 把偏移撑到工作环带的下限。
        flat = math.hypot(off[0], off[2])
        if flat < ctx.r_min:
            k = ctx.r_min / max(0.05, flat)
            off = (off[0] * k, off[1], off[2] * k)
        move.params["offset_m"] = [round(x, 3) for x in off]

        def run(u: float, t: float) -> Pose:
            base = subj.at(max(0.0, t - tau)) if subj else ctx.anchor.at(max(0.0, t - tau))
            p = vadd(base, off)
            p = (p[0], _clamp_eye(ctx.scene, p[1]), p[2])
            tgt = subj.at(t) if subj else ctx.anchor.at(t)
            yaw, elev = look_at(p, tgt)
            return Pose(p, yaw, elev, 0.0)
        return run
    return builder


def _target_switch(move: Move, ctx: Ctx, start: Pose) -> Runner:
    """多目标切换追踪：目标队列，每个停 2–6s，切换为平滑摇转（限幅器保证不甩镜头）。"""
    holds: List[float] = move.params.setdefault(
        "holds_s", [round(ctx.rng.uniform(2.0, 6.0), 2)
                    for _ in range(max(2, int(move.duration // 4)))])
    idx: List[int] = move.params.setdefault(
        "targets", [ctx.rng.randrange(max(1, len(ctx.subjects)))
                    for _ in range(len(holds))])
    L = float(move.params.get("extent", 0.0))
    fwd, _ = _basis(start.yaw)
    L = azimuth_budget(ctx, start.p, fwd,
                       radial_budget(ctx, start.p, fwd, 0.35 * L, move.t0), move.t0)

    def run(u: float, t: float) -> Pose:
        p = vadd(start.p, vscale(fwd, L * u))
        p = (p[0], _clamp_eye(ctx.scene, p[1]), p[2])
        local = t - move.t0
        acc = 0.0
        k = 0
        for i, h in enumerate(holds):
            if local < acc + h:
                k = i
                break
            acc += h
            k = i
        tgt = (ctx.subjects[idx[k] % len(ctx.subjects)].at(t)
               if ctx.subjects else ctx.anchor.at(t))
        yaw, elev = look_at(p, tgt)
        return Pose(p, yaw, elev, 0.0)
    return run


def _helix(move: Move, ctx: Ctx, start: Pose) -> Runner:
    c0 = ctx.anchor.at(move.t0)
    rel = vsub(start.p, c0)
    r0 = max(1.8, math.hypot(rel[0], rel[2]))
    turns = move.params.setdefault("turns", round(ctx.rng.uniform(0.75, 2.5), 2))
    pos_slack, neg_slack = sector_slack(ctx, start.p, move.t0)
    turns = min(turns, (pos_slack if ctx.placement.orbit_sign > 0 else neg_slack) / 360.0
                if ctx.sector_half_deg < 180.0 else turns)
    turns = max(turns, 0.08)
    move.params["turns_used"] = round(turns, 3)
    rise = move.params.setdefault("rise_per_turn_m", round(ctx.rng.uniform(0.6, 2.2), 2))
    sign = ctx.placement.orbit_sign
    th0 = math.atan2(rel[2], rel[0])
    y0 = start.p[1]
    up = 1.0 if ctx.rng.random() < 0.5 else -1.0
    move.params["direction"] = "up" if up > 0 else "down"

    def run(u: float, t: float) -> Pose:
        th = th0 + sign * 2.0 * math.pi * turns * u
        c = ctx.anchor.at(t)
        p = (c[0] + r0 * math.cos(th),
             _clamp_eye(ctx.scene, y0 + up * rise * turns * u),
             c[2] + r0 * math.sin(th))
        yaw, elev = look_at(p, c)
        return Pose(p, yaw, elev, 0.0)
    return run


def _fpv(move: Move, ctx: Ctx, start: Pose) -> Runner:
    """FPV 无人机：低通游走 + 侧倾 ≤10°（禁抖）。roll 由横向加速度符号给出。"""
    L = float(move.params.get("extent", 1.0))
    fwd, right = _basis(start.yaw)
    L = azimuth_budget(ctx, start.p, fwd,
                       radial_budget(ctx, start.p, fwd, L, move.t0), move.t0)
    sway = move.params.setdefault("sway_m", round(ctx.rng.uniform(1.0, 3.0), 2))
    bob = move.params.setdefault("bob_m", round(ctx.rng.uniform(0.3, 1.0), 2))
    cycles = move.params.setdefault("cycles", round(ctx.rng.uniform(0.8, 1.8), 2))
    roll_max = move.params.setdefault("roll_max_deg", round(ctx.rng.uniform(5.0, 10.0), 2))

    def run(u: float, t: float) -> Pose:
        phase = 2.0 * math.pi * cycles * u
        p = vadd(vadd(start.p, vscale(fwd, L * u)),
                 vscale(right, sway * math.sin(phase)))
        p = (p[0], _clamp_eye(ctx.scene, p[1] + bob * math.sin(0.5 * phase)), p[2])
        yaw, elev = look_at(p, ctx.anchor.at(t))
        # 横向加速度 ∝ -sin(phase)，侧倾与之同号并限幅
        roll = -roll_max * math.sin(phase)
        return Pose(p, yaw, elev, roll)
    return run


def _hold(move: Move, ctx: Ctx, start: Pose) -> Runner:
    def run(u: float, t: float) -> Pose:
        return Pose(start.p, start.yaw, start.elev, start.roll)
    return run


def _loop_path(move: Move, ctx: Ctx, start: Pose) -> Runner:
    """循环路径：终点**严格**回到起始位置与朝向（docs/71 §4）。

    朝向锁在 move 起点时刻的锚点位置（而非逐帧锚点）——否则锚点漂移会让朝向无法闭合。
    """
    L = float(move.params.get("extent", 8.0))
    # 环的半轴不能超出锚点工作环带的一半，否则一圈下来必然冲出共视可行域
    perim = clamp(L, 6.0, 2.0 * math.pi * max(2.0, 0.4 * (ctx.r_max - ctx.r_min)))
    a = perim / (2.0 * math.pi) * ctx.rng.uniform(1.0, 1.4)
    b = perim / (2.0 * math.pi) * ctx.rng.uniform(0.7, 1.0)
    move.params["ellipse_ab_m"] = [round(a, 3), round(b, 3)]
    fwd, right = _basis(start.yaw)
    sign = ctx.placement.orbit_sign
    fixed_target = ctx.anchor.at(move.t0)
    y0 = start.p[1]

    def run(u: float, t: float) -> Pose:
        th = sign * 2.0 * math.pi * u
        # 以 start.p 为椭圆上的起点（θ=0 处），保证 u=0 与 u=1 完全重合
        dx = a * (math.cos(th) - 1.0)
        dz = b * math.sin(th)
        p = vadd(vadd(start.p, vscale(right, dx)), vscale(fwd, dz))
        p = (p[0], _clamp_eye(ctx.scene, y0), p[2])
        yaw, elev = look_at(p, fixed_target)
        return Pose(p, yaw, elev, 0.0)
    return run


_BUILDERS: Dict[str, Callable[[Move, Ctx, Pose], Runner]] = {
    "translate_up": _translate("up"),
    "translate_down": _translate("down"),
    "translate_forward": _translate("forward"),
    "translate_back": _translate("back"),
    "translate_left": _translate("left"),
    "translate_right": _translate("right"),
    "pan_up": _pan("elev", +1.0),
    "pan_down": _pan("elev", -1.0),
    "pan_left": _pan("yaw", -1.0),
    "pan_right": _pan("yaw", +1.0),
    "roll_cw": _roll(+1.0),
    "roll_ccw": _roll(-1.0),
    "orbit_left": _orbit(False, -1),
    "orbit_right": _orbit(False, +1),
    "orbit_up": _orbit(True, +1),
    "orbit_down": _orbit(True, -1),
    "follow_bind": _follow(False),
    "follow_lag": _follow(True),
    "target_switch": _target_switch,
    "helix": _helix,
    "fpv_drone": _fpv,
    "hold_static": _hold,
    "loop_path": _loop_path,
}

ALL_CLASSES: List[str] = [c for cs in FAMILIES.values() for c in cs]
assert len(ALL_CLASSES) == 23, "MC 原语库必须是 23 类（docs/71 §4）"
assert set(ALL_CLASSES) == set(_BUILDERS), "原语表与构造器表必须一一对应"

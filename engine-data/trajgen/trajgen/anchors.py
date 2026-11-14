"""锚点区序列与动态主体（docs/71 辩题八 + §5）。

**锚点结构**是共视的结构性保证：三路各自 look-at 同一个锚点区 ⇒ 共视从「轨迹间耦合约束」
降维成「每路对锚点区的独立朝向约束」（DTU 机械臂全部 look-at 物体中心的同款做法）。

两个真耦合点的解法也落在这里（docs/71 §5）：
1. **tracking 引起的朝向同步** —— 锚点取动态主体的**慢质心**（低通 ~0.1Hz），
   瞬时 tracking 交给各路 look-at 在锚点区内独立游走；
2. **基线角-共视衰减** —— 三路收在 ≤90–120° 弧扇区内、按焦距分配扇区角与轨道半径
   （焦距越长 → 扇区角越小、半径越远）。
"""

import math
import random
from dataclasses import dataclass, field
from typing import Dict, List, Optional, Sequence, Tuple

from .mathutil import (Vec3, bspline5_eval, clamp, clamp_ctrl, vadd, vdist,
                       vscale, vsub)
from .scene import Scene

#: 三路收敛的弧扇区上限（度，docs/71 §5）
SECTOR_MAX_DEG = 120.0
#: 焦距 → 轨道半径倍率（焦距越长轨道越远，抵消视场变窄）
FOCAL_RADIUS_K = {18: 1.0, 24: 1.25, 35: 1.7, 50: 2.3}


@dataclass
class Subject:
    """动态主体：60s 内的平滑随机游走点（画面「动态物体」与硬跟随/追踪的目标）。

    插值必须用五次 B 样条（C⁴）而不是 smoothstep 之类的 C¹/C² 插值：
    硬跟随类目直接把主体位置搬成相机位置，主体在节点处的二阶导跳变会原样变成
    相机的 jerk 尖峰（实测能把 jerk p95 顶到数千），docs/69 表 2 的 20m/s³ 立刻挂。
    """
    knots: List[Vec3]
    knot_s: float
    _ctrl: Optional[Dict[str, List[float]]] = None

    def _build(self) -> None:
        self._ctrl = {
            "x": clamp_ctrl([p[0] for p in self.knots]),
            "y": clamp_ctrl([p[1] for p in self.knots]),
            "z": clamp_ctrl([p[2] for p in self.knots]),
        }

    def at(self, t: float) -> Vec3:
        if not self.knots:
            return (0.0, 0.0, 0.0)
        if len(self.knots) == 1:
            return self.knots[0]
        if self._ctrl is None:
            self._build()
        span = max(1, len(self._ctrl["x"]) - 5)
        total = (len(self.knots) - 1) * self.knot_s
        s = span * clamp(t, 0.0, total) / max(1e-9, total)
        return (bspline5_eval(self._ctrl["x"], s),
                bspline5_eval(self._ctrl["y"], s),
                bspline5_eval(self._ctrl["z"], s))


@dataclass
class Anchor:
    """锚点区：慢质心 + 半径。相机 look-at 目标在区内独立游走。"""
    center: Subject
    radius: float

    def at(self, t: float) -> Vec3:
        return self.center.at(t)


#: 主体活动区半径（m）——所有主体绕同一个活动中心游走
ACTIVITY_RADIUS_M = 5.0


def make_subject(scene: Scene, rng: random.Random, duration: float,
                 center: Vec3, knot_s: float = 4.0, span: float = 4.0,
                 activity_radius: float = ACTIVITY_RADIUS_M) -> Subject:
    """在活动区内生成一条平滑游走轨迹（4s 一个节点 ⇒ 主体自身低频）。

    主体必须**共处一个活动区**：否则慢质心（=锚点）落在几个主体之间的空地上，
    而各路 look-at 又各追各的主体，三路朝向就此散开，共视直接崩到 20–30%
    （docs/71 §5 的「锚点区取慢质心 + 瞬时 tracking 在区内游走」正是这个意思）。
    """
    n = max(2, int(duration / knot_s) + 1)
    p = scene.sample_free(rng, near=center, radius=activity_radius) or center
    knots = [p]
    for _ in range(n - 1):
        nxt = None
        for _ in range(24):
            a = rng.uniform(0, 2 * math.pi)
            r = rng.uniform(0.2, 1.0) * span
            cand = (knots[-1][0] + r * math.cos(a),
                    clamp(knots[-1][1] + rng.uniform(-0.4, 0.4),
                          scene.ground_y + 0.6, scene.ground_y + 3.0),
                    knots[-1][2] + r * math.sin(a))
            if (scene.free(cand, clearance=0.35)
                    and math.hypot(cand[0] - center[0], cand[2] - center[2]) <= activity_radius):
                nxt = cand
                break
        knots.append(nxt or knots[-1])
    return Subject(knots, knot_s)


def make_subject_moving(scene: Scene, rng: random.Random, duration: float,
                        center_fn, knot_s: float = 4.0,
                        activity_radius: float = ACTIVITY_RADIUS_M) -> Subject:
    """绕**移动中心**（center_fn: t→Vec3，如车辆导演模式的车队低通质心）的小半径
    游走主体：语义与 make_subject 相同，只是活动区跟着质心走（docs/78 §10）。"""
    n = max(2, int(duration / knot_s) + 1)
    knots: List[Vec3] = []
    off = (0.0, 0.0, 0.0)
    for i in range(n):
        t = duration * i / (n - 1)
        c = center_fn(t)
        nxt = None
        for _ in range(24):
            a = rng.uniform(0, 2 * math.pi)
            r = rng.uniform(0.2, 0.6) * activity_radius
            cand_off = (off[0] * 0.6 + r * math.cos(a) * 0.4,
                        clamp(off[1] + rng.uniform(-0.4, 0.4), -2.0, 2.0),
                        off[2] * 0.6 + r * math.sin(a) * 0.4)
            cand = vadd(c, cand_off)
            if math.hypot(cand_off[0], cand_off[2]) <= activity_radius \
                    and scene.free(cand, clearance=0.35):
                nxt = cand
                off = cand_off
                break
        knots.append(nxt or c)
    return Subject(knots, knot_s)


def make_anchor(scene: Scene, rng: random.Random, duration: float,
                subjects: Sequence[Subject]) -> Anchor:
    """锚点 = 动态主体的慢质心（低通）。无主体时退化为场景内一个静态可视点。"""
    knot_s = 10.0   # ~0.1Hz 低通：docs/71 §5「锚点区取主体慢质心」
    n = max(2, int(duration / knot_s) + 1)
    knots: List[Vec3] = []
    for i in range(n):
        t = duration * i / (n - 1)
        if subjects:
            pts = [s.at(t) for s in subjects]
            c = (sum(p[0] for p in pts) / len(pts),
                 sum(p[1] for p in pts) / len(pts),
                 sum(p[2] for p in pts) / len(pts))
        else:
            c = scene.sample_free(rng) or (0.0, scene.ground_y + 1.6, 0.0)
        knots.append(c)
    radius = rng.uniform(1.5, 4.0)
    return Anchor(Subject(knots, knot_s), radius)


@dataclass
class Placement:
    """一路相机相对锚点的站位参数（扇区角 + 轨道半径 + 环绕方向）。"""
    sector_deg: float
    radius: float
    orbit_sign: int          # +1 / -1，环绕方向；竖直环绕另计
    vertical: bool = False


def allocate_placements(rng: random.Random, focals: Sequence[int],
                        n_covis: int = 3) -> List[Placement]:
    """给前 n_covis 路分配扇区角/半径/环绕方向；第四路单独处理（不参与共视）。

    - 扇区角在 [-SECTOR_MAX/2, +SECTOR_MAX/2] 内错开，焦距越长分到越靠中间的小角；
    - 半径按 `FOCAL_RADIUS_K` 随焦距放大；
    - 环绕方向 CW/CCW/竖直三选一互不重复 —— 这是 docs/71 §5「结构性解耦注入」的第一条，
      也是「tracking 朝向同步」的天然打散器（环绕中 yaw 由轨道几何主导，方向反号）。
    """
    order = sorted(range(n_covis), key=lambda i: -FOCAL_RADIUS_K.get(focals[i], 1.0))
    base = rng.uniform(0, 360.0)
    slots = [-1.0, 0.0, 1.0]
    rng.shuffle(slots)
    dirs = [(+1, False), (-1, False), (+1, True)]
    rng.shuffle(dirs)
    out: List[Placement] = [None] * n_covis           # type: ignore[list-item]
    for rank, cam_i in enumerate(order):
        k = FOCAL_RADIUS_K.get(focals[cam_i], 1.0)
        # 焦距最长的（rank 0）分到最小的扇区偏角
        spread = SECTOR_MAX_DEG / 2.0 * (0.35 if rank == 0 else 1.0)
        sign, vertical = dirs[rank]
        out[cam_i] = Placement(
            sector_deg=base + slots[rank] * spread + rng.uniform(-8.0, 8.0),
            radius=rng.uniform(2.5, 5.5) * k,
            orbit_sign=sign,
            vertical=vertical,
        )
    return out


def place_camera(scene: Scene, anchor: Anchor, pl: Placement,
                 rng: random.Random, t: float = 0.0) -> Optional[Vec3]:
    """按扇区角/半径把相机放进自由壳；被占则沿角度与半径退让搜索。"""
    c = anchor.at(t)
    for dr in (0.0, 0.5, -0.5, 1.0, -1.0, 1.8, 2.6):
        for da in (0.0, 12.0, -12.0, 25.0, -25.0, 40.0, -40.0, 60.0, -60.0):
            r = max(1.5, pl.radius + dr)
            a = math.radians(pl.sector_deg + da)
            p = (c[0] + r * math.cos(a),
                 scene.ground_y + rng.uniform(scene.eye_lo, scene.eye_hi),
                 c[2] + r * math.sin(a))
            if scene.free(p) and not scene.blocked(p, c):
                return p
    return scene.sample_free(rng, near=c, radius=max(6.0, pl.radius * 1.5))

"""共视度量（docs/71 §5 三级度量的规划侧两级；第三级 QC 精算在 engine-data-qc）。

**口径**（docs/71 辩题七裁决）：
- 双向 `min(O(A→B), O(B→A)) ≥ 0.30` 才算这一对共视；
- 前三路里**至少一对**成立即计入共视时长，第四路不参与统计；
- 全片共视时长占比闸门 ≥50%，**规划目标 65%**（给 QC 精算与人审偏差留 15pt 余量）。

**实现选型**：按 A 的**图像空间均匀网格**投射首击射线拿到 A 实际看到的几何点，
再看这些点有多少同时被 B 看见 —— 这就是 SuperGlue/LoFTR 那套 overlap score 的
离散化，且图像空间均匀采样天然是**等面积加权**，docs/71 §5 要求的
「点计数须按投影立体角 `w=area·cosθ/d²` 加权」由构造满足。

之所以不走「场景表面撒 probe」那条：probe 密度必须远小于相机足迹才成立，
而 60×60m 场景撒几百个 probe 时，一台贴近锚点（r≈1.2m）的窄镜头相机
画面里**一个 probe 都落不进**，overlap 会算出恒等于 0 的假失败（实测过）。
图像空间采样与相机距离/焦距/画幅无关，没有这个失效模式。

场景几何用 AABB 障碍盒 + 地面，首击与遮挡都走解析 slab 求交（无需体素行进）。
"""

import math
from dataclasses import dataclass, field
from typing import Dict, List, Optional, Sequence, Tuple

from .mathutil import Quat, Vec3, quat_forward, quat_right, quat_up, vdot, vnorm, vsub
from .scene import Scene

#: 闸门：双向 min 阈值
OVERLAP_MIN = 0.30
#: 闸门：共视时长占比
COVIS_RATIO_MIN = 0.50
#: 规划目标（留余量）
COVIS_RATIO_TARGET = 0.65
#: 参与共视统计的相机（第四路排除，docs/69 §0）
COVIS_CAMS = (1, 2, 3)
#: 图像空间采样网格（列 × 行）
GRID = (8, 8)
#: 首击射线的最远距离（m）
MAX_DIST = 80.0
#: 画面里几何占比低于此值即判为「在看天/看虚空」，该帧不计共视
MIN_GEOMETRY_FRAC = 0.20


@dataclass
class CamView:
    """一帧的相机视图：位姿 + 内参 + 画幅。"""
    p: Vec3
    q: Quat
    fx: float
    fy: float
    cx: float
    cy: float
    width: int
    height: int


def _ray_hit(scene: Scene, o: Vec3, d: Vec3, tmax: float) -> Optional[float]:
    """射线与场景（AABB + 地面）的最近命中距离；无命中返回 None。"""
    best = tmax
    hit = False
    for b in scene.boxes:
        t0, t1 = 0.0, best
        for a in range(3):
            if abs(d[a]) < 1e-9:
                if o[a] < b.lo[a] or o[a] > b.hi[a]:
                    t0 = 1.0
                    t1 = 0.0
                    break
                continue
            inv = 1.0 / d[a]
            ta = (b.lo[a] - o[a]) * inv
            tb = (b.hi[a] - o[a]) * inv
            if ta > tb:
                ta, tb = tb, ta
            t0 = ta if ta > t0 else t0
            t1 = tb if tb < t1 else t1
            if t0 > t1:
                break
        if t0 <= t1 and t0 > 1e-4:
            best = t0
            hit = True
    if d[1] < -1e-9:                     # 地面
        tg = (scene.ground_y - o[1]) / d[1]
        if 1e-4 < tg < best:
            best = tg
            hit = True
    return best if hit else None


def image_rays(cam: CamView, grid: Tuple[int, int] = GRID) -> List[Vec3]:
    """图像空间均匀网格 → 世界系方向（每条射线代表等量画面面积）。"""
    fwd, right, up = quat_forward(cam.q), quat_right(cam.q), quat_up(cam.q)
    nx, ny = grid
    out: List[Vec3] = []
    for j in range(ny):
        v = (j + 0.5) / ny * cam.height
        for i in range(nx):
            u = (i + 0.5) / nx * cam.width
            xc = (u - cam.cx) / cam.fx
            yc = (cam.cy - v) / cam.fy
            d = (fwd[0] + right[0] * xc + up[0] * yc,
                 fwd[1] + right[1] * xc + up[1] * yc,
                 fwd[2] + right[2] * xc + up[2] * yc)
            n = vnorm(d)
            out.append((d[0] / n, d[1] / n, d[2] / n))
    return out


def visible_points(scene: Scene, cam: CamView, grid: Tuple[int, int] = GRID
                   ) -> Tuple[List[Vec3], int]:
    """A 实际看到的几何点集（等面积采样）与射线总数。"""
    pts: List[Vec3] = []
    dirs = image_rays(cam, grid)
    for d in dirs:
        t = _ray_hit(scene, cam.p, d, MAX_DIST)
        if t is not None:
            pts.append((cam.p[0] + d[0] * t, cam.p[1] + d[1] * t, cam.p[2] + d[2] * t))
    return pts, len(dirs)


def sees(scene: Scene, cam: CamView, p: Vec3) -> bool:
    """点 p 是否落在 cam 幅内且未被遮挡。"""
    d = vsub(p, cam.p)
    fwd = quat_forward(cam.q)
    zc = vdot(d, fwd)
    if zc <= 0.05:
        return False
    u = cam.fx * vdot(d, quat_right(cam.q)) / zc + cam.cx
    if u < 0.0 or u >= cam.width:
        return False
    v = cam.cy - cam.fy * vdot(d, quat_up(cam.q)) / zc
    if v < 0.0 or v >= cam.height:
        return False
    dist = vnorm(d)
    dirn = (d[0] / dist, d[1] / dist, d[2] / dist)
    t = _ray_hit(scene, cam.p, dirn, dist - 0.05)
    return t is None or t >= dist - 0.05


def overlap(scene: Scene, a: CamView, b: CamView,
            hits_a: Optional[List[Vec3]] = None, hits_b: Optional[List[Vec3]] = None,
            n_rays: int = GRID[0] * GRID[1]) -> Tuple[float, float]:
    """双向 overlap `O(A→B)`、`O(B→A)`。分母是各自画面里的**几何**部分。"""
    if hits_a is None:
        hits_a, _ = visible_points(scene, a)
    if hits_b is None:
        hits_b, _ = visible_points(scene, b)
    if (len(hits_a) < MIN_GEOMETRY_FRAC * n_rays
            or len(hits_b) < MIN_GEOMETRY_FRAC * n_rays):
        return 0.0, 0.0        # 至少一方在看天，不计共视
    oa = sum(1 for p in hits_a if sees(scene, b, p)) / len(hits_a)
    ob = sum(1 for p in hits_b if sees(scene, a, p)) / len(hits_b)
    return oa, ob


@dataclass
class CovisResult:
    #: 闸门口径 = **严读**：前三路**三对全部**成立的时长占比。
    #: 需求原文「多个相机画面中共同存在（不包括第四相机）≥30% 相同的区域」——
    #: 「共同存在」+ 显式排除第四路（余下恰好三路）⇒ 三路都要看到那块区域。
    #: 严读达标必然蕴含宽读达标，取严读不会两头落空。
    ratio: float
    #: 宽读（至少一对成立）占比，仅作对照上报，不作闸门
    ratio_any_pair: float = 0.0
    #: [(t, 三对中的最小值)] —— 严读口径下这条曲线 ≥0.30 即该采样点计入
    ts: List[Tuple[float, float]] = field(default_factory=list)
    #: 逐对 min overlap 时间曲线（防贴线分析用）
    pair_ts: Dict[str, List[float]] = field(default_factory=dict)
    times: List[float] = field(default_factory=list)

    def to_dict(self) -> Dict:
        return {
            "covis_ratio": round(self.ratio, 4),
            "covis_ratio_any_pair": round(self.ratio_any_pair, 4),
            "overlap_min": OVERLAP_MIN,
            "gate": "min(O(A→B),O(B→A)) ≥ %.2f，前三路**三对全部**成立才计时长"
                    "（严读，需求「共同存在」）；第四路不参与" % OVERLAP_MIN,
            "method": "图像空间 %dx%d 均匀采样的双向重投影 overlap（等面积加权）" % GRID,
            "pair_min_overlap_ts": [[round(t, 3), round(v, 4)] for t, v in self.ts],
            "pair_curves": {k: [round(x, 4) for x in v] for k, v in self.pair_ts.items()},
        }


def evaluate(scene: Scene, views: Dict[int, Sequence[CamView]], times: Sequence[float],
             grid: Tuple[int, int] = GRID) -> CovisResult:
    """在给定时间采样上算共视时长占比。`views[cam_id][k]` 为第 k 个采样点的相机视图。"""
    cams = [c for c in COVIS_CAMS if c in views]
    pairs = [(a, b) for i, a in enumerate(cams) for b in cams[i + 1:]]
    res = CovisResult(ratio=0.0, ratio_any_pair=0.0, ts=[],
                      pair_ts={("%d-%d" % p): [] for p in pairs}, times=list(times))
    if not pairs:
        return res
    n_rays = grid[0] * grid[1]

    hit = 0
    hit_any = 0
    for k, t in enumerate(times):
        hits = {c: visible_points(scene, views[c][k], grid)[0] for c in cams}
        best = 0.0
        worst = 1.0
        for a, b in pairs:
            oa, ob = overlap(scene, views[a][k], views[b][k], hits[a], hits[b], n_rays)
            m = min(oa, ob)
            res.pair_ts["%d-%d" % (a, b)].append(m)
            best = max(best, m)
            worst = min(worst, m)
        # 严读：三对里最差的一对也要过线（「共同存在」⇒ 三路都看得到）
        res.ts.append((t, worst))
        if worst >= OVERLAP_MIN:
            hit += 1
        if best >= OVERLAP_MIN:
            hit_any += 1
    res.ratio = hit / len(times) if times else 0.0
    res.ratio_any_pair = hit_any / len(times) if times else 0.0
    return res


def sample_times(duration: float, hz: float) -> List[float]:
    n = max(2, int(round(duration * hz)) + 1)
    return [duration * i / (n - 1) for i in range(n)]

"""场景占据图 / 相机可行自由空间壳 / 表面 probe 采样（docs/71 §7 防线 1）。

场景以 **AABB 障碍盒 + 地面 + 边界** 表示，据此烘出 0.25m 占据图用于射线遮挡查询。
真实场景（M3b 起）由 SceneKit onboarding 导出同构的 `scene.json` 装载，
本模块内置的 4 类**合成场景**是 M3a 排程仿真在无美术资产期的替身
（docs/70 R1：授权场景采购是 M2 硬前置，M3a 明确不依赖它）。

合成场景类型对应 docs/71 R9 的风险分层：
`open_plaza`（开阔）/ `urban_block`（街道峡谷）/ `indoor_rooms`（室内房间）/
`corridor`（走廊隧道，可行域最薄、卡墙与解耦双高危）。
"""

import json
import math
import random
import zlib
from dataclasses import dataclass, field
from typing import Dict, List, Optional, Sequence, Tuple

from .mathutil import Vec3, vdist, vnorm, vsub, vunit

#: 占据图体素边长（m，docs/71 §7）
VOXEL_M = 0.25
#: 相机可行自由空间壳的安全间隙（m，docs/71 §7）
CLEARANCE_M = 0.5
#: 遮挡射线行进步长（m）——按 2 体素取，仿真够用；生产侧走 SDK 的 RaycastCommand
RAY_STEP_M = 0.5
#: 可站面空间哈希的桶边长（m）
_GCELL_M = 8.0
#: 相机位置到最近可站面格的水平距离上限（m）。0 = 关闭（默认，保持既有行为）。
_CAM_GROUND_M = float(__import__("os").environ.get("DIOGENES_CAM_GROUND_M", "0"))


@dataclass
class Box:
    """轴对齐障碍盒（世界系，米）。"""
    lo: Vec3
    hi: Vec3

    def contains(self, p: Sequence[float], pad: float = 0.0) -> bool:
        return all(self.lo[i] - pad <= p[i] <= self.hi[i] + pad for i in range(3))


@dataclass
class Probe:
    """场景表面采样点：位置、法线、代表面积（用于投影立体角加权）。"""
    p: Vec3
    n: Vec3
    area: float


@dataclass
class Scene:
    name: str
    kind: str
    lo: Vec3
    hi: Vec3
    ground_y: float
    boxes: List[Box] = field(default_factory=list)
    #: 相机视高带（第一人称，docs/71 §2）
    eye_lo: float = 1.4
    eye_hi: float = 2.2
    probes: List[Probe] = field(default_factory=list)
    #: 可站面格心的 (x, z)。来自场景导出的 `ground_boxes`（Unity 是地板/平台面，
    #: CARLA 是行人导航网格）。**动态物体只能出现在这些地方**——锚点区落在
    #: 没有可站面的地方（比如马路正中、大停车场），四路镜头就都对着一片
    #: 什么都不可能有的区域。实测过：那样的一路 60s 里有 40s 视锥内动态物体恒为 0。
    ground_cells: List[Tuple[float, float]] = field(default_factory=list)
    _gcell: Optional[Dict] = None
    _grid: Optional[bytearray] = None
    _dims: Tuple[int, int, int] = (0, 0, 0)

    def ground_count(self, p: Sequence[float], radius: float, cap: int = 999) -> int:
        """p 水平位置 radius 内的可站面格数（数到 cap 即止）。

        **要的是「数量」不是「有没有」**：只问有没有，贴着大楼的人行道也算通过，
        而那种地方相机排不开、共视直接崩（实测 11%–22%）；要的是既有可站面
        又足够开阔的地方——广场。数量恰好同时表达这两件事。
        """
        if not self.ground_cells:
            return cap
        if self._gcell is None:
            self._gcell = {}
            for gx, gz in self.ground_cells:
                self._gcell.setdefault(
                    (int(gx // _GCELL_M), int(gz // _GCELL_M)), []).append((gx, gz))
        r2 = radius * radius
        span = int(radius // _GCELL_M) + 1
        cx, cz = int(p[0] // _GCELL_M), int(p[2] // _GCELL_M)
        n = 0
        for i in range(cx - span, cx + span + 1):
            for j in range(cz - span, cz + span + 1):
                for gx, gz in self._gcell.get((i, j), ()):
                    if (p[0] - gx) ** 2 + (p[2] - gz) ** 2 <= r2:
                        n += 1
                        if n >= cap:
                            return n
        return n

    def near_ground(self, p: Sequence[float], radius: float) -> bool:
        """p 的水平位置附近 radius 内有可站面吗？无可站面数据时一律放行。

        走空间哈希而不是线性扫：这个判定在活动中心搜索的内层循环里，
        每条计划要问上万次，线性扫 771 个格子会把生成时间拖到分钟级。
        """
        if not self.ground_cells:
            return True
        if self._gcell is None:
            self._gcell = {}
            for gx, gz in self.ground_cells:
                self._gcell.setdefault(
                    (int(gx // _GCELL_M), int(gz // _GCELL_M)), []).append((gx, gz))
        r2 = radius * radius
        span = int(radius // _GCELL_M) + 1
        cx, cz = int(p[0] // _GCELL_M), int(p[2] // _GCELL_M)
        for i in range(cx - span, cx + span + 1):
            for j in range(cz - span, cz + span + 1):
                for gx, gz in self._gcell.get((i, j), ()):
                    if (p[0] - gx) ** 2 + (p[2] - gz) ** 2 <= r2:
                        return True
        return False

    # ------------------------------------------------------------ 占据图

    def bake(self) -> "Scene":
        nx = max(1, int(math.ceil((self.hi[0] - self.lo[0]) / VOXEL_M)))
        ny = max(1, int(math.ceil((self.hi[1] - self.lo[1]) / VOXEL_M)))
        nz = max(1, int(math.ceil((self.hi[2] - self.lo[2]) / VOXEL_M)))
        grid = bytearray(nx * ny * nz)
        for b in self.boxes:
            ix0 = max(0, int((b.lo[0] - self.lo[0]) / VOXEL_M))
            ix1 = min(nx - 1, int((b.hi[0] - self.lo[0]) / VOXEL_M))
            iy0 = max(0, int((b.lo[1] - self.lo[1]) / VOXEL_M))
            iy1 = min(ny - 1, int((b.hi[1] - self.lo[1]) / VOXEL_M))
            iz0 = max(0, int((b.lo[2] - self.lo[2]) / VOXEL_M))
            iz1 = min(nz - 1, int((b.hi[2] - self.lo[2]) / VOXEL_M))
            for iz in range(iz0, iz1 + 1):
                base_z = iz * ny
                for iy in range(iy0, iy1 + 1):
                    row = (base_z + iy) * nx
                    for ix in range(ix0, ix1 + 1):
                        grid[row + ix] = 1
        self._grid = grid
        self._dims = (nx, ny, nz)
        return self

    def occupied(self, p: Sequence[float]) -> bool:
        """点是否落在占据体素内（越界视为空）。"""
        if self._grid is None:
            self.bake()
        nx, ny, nz = self._dims
        ix = int((p[0] - self.lo[0]) / VOXEL_M)
        iy = int((p[1] - self.lo[1]) / VOXEL_M)
        iz = int((p[2] - self.lo[2]) / VOXEL_M)
        if ix < 0 or iy < 0 or iz < 0 or ix >= nx or iy >= ny or iz >= nz:
            return False
        return self._grid[(iz * ny + iy) * nx + ix] != 0

    # ------------------------------------------------------------ 自由空间壳

    def in_bounds(self, p: Sequence[float]) -> bool:
        return all(self.lo[i] <= p[i] <= self.hi[i] for i in range(3))

    def free(self, p: Sequence[float], clearance: float = CLEARANCE_M) -> bool:
        """相机可行自由空间壳：在边界内、离地/离障碍均 ≥ clearance。

        **可选：相机也要站在可站面附近**（env `DIOGENES_CAM_GROUND_M`）。
        自由 ≠ 可站——马路正中完全自由，但把相机放在那里，autopilot 车流
        会直接从镜头前碾过去：CARLA 批实测 16:9 前四条候选的「贴脸帧占比」
        是 0.21–0.68（闸门线 0.03），四条全废，而 `active_frac` 全部 0.89–1.00。
        相机站人行道、车走车道，两者天然不冲突（与「可站面从车道换成人行道」
        是同一条选址逻辑，只是那次修的是锚点，这次修的是相机自己）。
        """
        if not self.in_bounds(p):
            return False
        if p[1] - self.ground_y < clearance:
            return False
        for b in self.boxes:
            if b.contains(p, pad=clearance):
                return False
        if _CAM_GROUND_M > 0 and not self.near_ground(p, _CAM_GROUND_M):
            return False
        return True

    def clearance(self, p: Sequence[float]) -> float:
        """到最近障碍/地面的间隙（m）。负值表示已在障碍内。"""
        d = p[1] - self.ground_y
        for b in self.boxes:
            dx = max(b.lo[0] - p[0], 0.0, p[0] - b.hi[0])
            dy = max(b.lo[1] - p[1], 0.0, p[1] - b.hi[1])
            dz = max(b.lo[2] - p[2], 0.0, p[2] - b.hi[2])
            if dx == 0.0 and dy == 0.0 and dz == 0.0:
                return -1.0
            d = min(d, math.sqrt(dx * dx + dy * dy + dz * dz))
        return d

    def push_out(self, p: Sequence[float], clearance: float = CLEARANCE_M) -> Vec3:
        """把点推到自由壳内（docs/71 §7 防线 1「轨迹只在壳内生成，穿模零容忍由构造保证」）。

        对每个把点吞进去的障碍盒，沿**穿透最浅**的轴推出；再夹住地面与边界。
        逐节点做，之后统一过五次 B 样条，推出的折角会被抹平。
        """
        q = [float(p[0]), float(p[1]), float(p[2])]
        for _ in range(3):
            moved = False
            for b in self.boxes:
                if not b.contains(q, pad=clearance):
                    continue
                best = None
                for a in range(3):
                    out_lo = (b.lo[a] - clearance) - q[a]      # 负：往 lo 侧推的距离
                    out_hi = (b.hi[a] + clearance) - q[a]      # 正：往 hi 侧推的距离
                    for d in (out_lo, out_hi):
                        if best is None or abs(d) < abs(best[1]):
                            best = (a, d)
                if best is not None:
                    q[best[0]] += best[1]
                    moved = True
            if not moved:
                break
        q[1] = max(q[1], self.ground_y + clearance)
        for a in range(3):
            q[a] = min(max(q[a], self.lo[a]), self.hi[a])
        return (q[0], q[1], q[2])

    def sample_free(self, rng: random.Random, tries: int = 200,
                    near: Optional[Sequence[float]] = None,
                    radius: float = 0.0) -> Optional[Vec3]:
        """在自由壳内随机取一个相机可站位（视高带内）。"""
        for _ in range(tries):
            if near is not None and radius > 0.0:
                a = rng.uniform(0, 2 * math.pi)
                r = radius * math.sqrt(rng.random())
                x = near[0] + r * math.cos(a)
                z = near[2] + r * math.sin(a)
            else:
                x = rng.uniform(self.lo[0], self.hi[0])
                z = rng.uniform(self.lo[2], self.hi[2])
            y = self.ground_y + rng.uniform(self.eye_lo, self.eye_hi)
            p = (x, y, z)
            if self.free(p):
                return p
        return None

    # ------------------------------------------------------------ 遮挡射线

    def blocked(self, a: Sequence[float], b: Sequence[float],
                skip_end: float = 0.3) -> bool:
        """线段 a→b 是否被占据体素挡住（端点各留 skip_end 米不检，避免自遮挡）。"""
        if self._grid is None:
            self.bake()
        d = vsub(b, a)
        dist = vnorm(d)
        if dist <= 2 * skip_end:
            return False
        u = (d[0] / dist, d[1] / dist, d[2] / dist)
        nx, ny, nz = self._dims
        grid = self._grid
        lox, loy, loz = self.lo
        t = skip_end
        end = dist - skip_end
        while t < end:
            ix = int((a[0] + u[0] * t - lox) / VOXEL_M)
            iy = int((a[1] + u[1] * t - loy) / VOXEL_M)
            iz = int((a[2] + u[2] * t - loz) / VOXEL_M)
            if 0 <= ix < nx and 0 <= iy < ny and 0 <= iz < nz:
                if grid[(iz * ny + iy) * nx + ix]:
                    return True
            t += RAY_STEP_M
        return False

    # ------------------------------------------------------------ 表面 probe

    def build_probes(self, rng: random.Random, n_target: int = 320) -> "Scene":
        """在地面与障碍盒立面上分层采样表面 probe（带法线与代表面积）。

        地面与立面各占约一半 —— 地面是「全方位共享表面」（docs/71 §5 基线角解法的实底），
        立面则是共视真正吃紧的部分，两者都要进 overlap 分母。
        """
        probes: List[Probe] = []
        n_ground = n_target // 2
        side = max(2, int(math.sqrt(n_ground)))
        cell_x = (self.hi[0] - self.lo[0]) / side
        cell_z = (self.hi[2] - self.lo[2]) / side
        for i in range(side):
            for j in range(side):
                x = self.lo[0] + (i + rng.random()) * cell_x
                z = self.lo[2] + (j + rng.random()) * cell_z
                p = (x, self.ground_y + 0.02, z)
                if any(b.contains((x, self.ground_y + 0.05, z)) for b in self.boxes):
                    continue
                probes.append(Probe(p, (0.0, 1.0, 0.0), cell_x * cell_z))

        faces: List[Tuple[Vec3, Vec3, float]] = []   # (面中心不用，采样时现取)
        total_area = 0.0
        face_defs = []
        for b in self.boxes:
            w = (b.hi[0] - b.lo[0], b.hi[1] - b.lo[1], b.hi[2] - b.lo[2])
            for axis in (0, 2):
                other = 2 if axis == 0 else 0
                area = w[other] * w[1]
                if area <= 0:
                    continue
                for sgn in (-1.0, 1.0):
                    n = [0.0, 0.0, 0.0]
                    n[axis] = sgn
                    face_defs.append((b, axis, other, sgn, area, tuple(n)))
                    total_area += area
            top = w[0] * w[2]
            if top > 0:
                face_defs.append((b, 1, -1, 1.0, top, (0.0, 1.0, 0.0)))
                total_area += top
        n_side = n_target - len(probes)
        if face_defs and total_area > 0 and n_side > 0:
            for b, axis, other, sgn, area, n in face_defs:
                k = max(1, int(round(n_side * area / total_area)))
                for _ in range(k):
                    p = [0.0, 0.0, 0.0]
                    if axis == 1:  # 顶面
                        p[0] = rng.uniform(b.lo[0], b.hi[0])
                        p[2] = rng.uniform(b.lo[2], b.hi[2])
                        p[1] = b.hi[1] + 0.02
                    else:
                        p[axis] = (b.lo[axis] - 0.02) if sgn < 0 else (b.hi[axis] + 0.02)
                        p[other] = rng.uniform(b.lo[other], b.hi[other])
                        p[1] = rng.uniform(b.lo[1], b.hi[1])
                    probes.append(Probe(tuple(p), n, area / k))
        self.probes = probes
        return self

    # ------------------------------------------------------------ 序列化

    def to_dict(self) -> Dict:
        return {
            "name": self.name,
            "kind": self.kind,
            "lo": list(self.lo),
            "hi": list(self.hi),
            "ground_y": self.ground_y,
            "eye_band": [self.eye_lo, self.eye_hi],
            "boxes": [{"lo": list(b.lo), "hi": list(b.hi)} for b in self.boxes],
        }

    @staticmethod
    def from_dict(d: Dict) -> "Scene":
        eye = d.get("eye_band") or [1.4, 2.2]
        sc = Scene(
            name=d["name"], kind=d.get("kind", "custom"),
            lo=tuple(d["lo"]), hi=tuple(d["hi"]),
            ground_y=float(d.get("ground_y", 0.0)),
            boxes=[Box(tuple(b["lo"]), tuple(b["hi"])) for b in d.get("boxes", [])],
            eye_lo=float(eye[0]), eye_hi=float(eye[1]),
            ground_cells=[((b["lo"][0] + b["hi"][0]) * 0.5,
                           (b["lo"][2] + b["hi"][2]) * 0.5)
                          for b in d.get("ground_boxes", [])],
        )
        return sc.bake()

    @staticmethod
    def load(path) -> "Scene":
        with open(path, "r", encoding="utf-8") as f:
            return Scene.from_dict(json.load(f))


# ---------------------------------------------------------------- 合成场景

#: 合成场景类型 → 简述（进 M3a 失败原因×场景类型交叉表）
SCENE_KINDS = {
    "open_plaza": "开阔广场：稀疏立柱，可行域最宽",
    "urban_block": "街道峡谷：成排建筑 + 街道网格",
    "indoor_rooms": "室内房间：隔墙 + 门洞",
    "corridor": "走廊隧道：可行域最薄（R9 高危）",
}


def synthetic(kind: str, seed: int) -> Scene:
    """按类型生成合成场景（确定性：同 kind+seed 必同）。

    种子里**不能**用内建 `hash(str)` —— 它按进程随机加盐，会让「种子入 provenance
    可复现」（docs/70 §4.2）直接失效；用 crc32 这种跨进程稳定的散列。
    """
    rng = random.Random(zlib.crc32(kind.encode("utf-8")) * 1000003 + seed)
    if kind == "open_plaza":
        sc = Scene("plaza-%d" % seed, kind, (-30.0, 0.0, -30.0), (30.0, 12.0, 30.0), 0.0)
        for _ in range(rng.randint(8, 16)):
            x, z = rng.uniform(-26, 26), rng.uniform(-26, 26)
            r = rng.uniform(0.4, 1.2)
            h = rng.uniform(3.0, 8.0)
            sc.boxes.append(Box((x - r, 0.0, z - r), (x + r, h, z + r)))
    elif kind == "urban_block":
        sc = Scene("city-%d" % seed, kind, (-40.0, 0.0, -40.0), (40.0, 24.0, 40.0), 0.0)
        for gx in (-1, 0, 1):
            for gz in (-1, 0, 1):
                if gx == 0 and gz == 0:
                    continue
                cx, cz = gx * 24.0, gz * 24.0
                w = rng.uniform(6.0, 9.0)
                d = rng.uniform(6.0, 9.0)
                h = rng.uniform(8.0, 20.0)
                sc.boxes.append(Box((cx - w, 0.0, cz - d), (cx + w, h, cz + d)))
        for _ in range(rng.randint(4, 8)):
            x, z = rng.uniform(-36, 36), rng.uniform(-36, 36)
            sc.boxes.append(Box((x - 0.5, 0.0, z - 0.5), (x + 0.5, rng.uniform(2.5, 4.5), z + 0.5)))
    elif kind == "indoor_rooms":
        sc = Scene("rooms-%d" % seed, kind, (-18.0, 0.0, -18.0), (18.0, 4.0, 18.0), 0.0)
        t = 0.2
        for x in (-6.0, 6.0):
            gap = rng.uniform(-10.0, 10.0)
            sc.boxes.append(Box((x - t, 0.0, -18.0), (x + t, 3.5, gap - 1.2)))
            sc.boxes.append(Box((x - t, 0.0, gap + 1.2), (x + t, 3.5, 18.0)))
        for z in (-6.0, 6.0):
            gap = rng.uniform(-10.0, 10.0)
            sc.boxes.append(Box((-18.0, 0.0, z - t), (gap - 1.2, 3.5, z + t)))
            sc.boxes.append(Box((gap + 1.2, 0.0, z - t), (18.0, 3.5, z + t)))
        for _ in range(rng.randint(4, 10)):
            x, z = rng.uniform(-16, 16), rng.uniform(-16, 16)
            sc.boxes.append(Box((x - 0.6, 0.0, z - 0.6), (x + 0.6, rng.uniform(0.8, 2.0), z + 0.6)))
        sc.eye_lo, sc.eye_hi = 1.4, 2.0
    elif kind == "corridor":
        sc = Scene("corr-%d" % seed, kind, (-24.0, 0.0, -12.0), (24.0, 4.0, 12.0), 0.0)
        half = rng.uniform(1.6, 2.4)
        sc.boxes.append(Box((-24.0, 0.0, half), (24.0, 3.2, 12.0)))
        sc.boxes.append(Box((-24.0, 0.0, -12.0), (24.0, 3.2, -half)))
        for _ in range(rng.randint(2, 5)):
            x = rng.uniform(-20, 20)
            sc.boxes.append(Box((x - 0.4, 0.0, -half), (x + 0.4, rng.uniform(1.0, 2.2), -half + 0.8)))
        sc.eye_lo, sc.eye_hi = 1.4, 1.9
    else:
        raise ValueError("未知合成场景类型：%s（可选 %s）" % (kind, ", ".join(SCENE_KINDS)))
    sc.bake()
    sc.build_probes(rng)
    return sc


def default_scenes(seed: int = 0) -> List[Scene]:
    """四类合成场景各一，M3a 默认场景池。"""
    return [synthetic(k, seed) for k in SCENE_KINDS]

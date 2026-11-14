"""计划生成器：分段计划 + 两级配额排产 + 层级化失败重排（docs/71 §4/§5/§10）。

**分段计划**（69 §3.1）：每路独立随机切 1–10 段；每段随机选 ≥1 种非粉色运镜、
时长随机分配；粉色 6 平移每条必含、时间随机；段间缓入缓出由 `sampler` 的样条保证。

**两级配额**：条内必含项（6 平移）由模板保证；语料级配额（25%/30%/5%/≤3%/≤2% 与
焦距、画幅配比）由 `QuotaTracker` 跨条统计命中，缺口越大的类目被抽中的权重越高。

**初始帧**（69 §2 + docs/71 §2）：4 路起始位置同锚点区邻域、朝向夹角 ≤15°（内控值）——
所以四路是**从同一簇出发**再各自散开，而不是一开始就散在锚点四周
（那样首帧朝向夹角会直接到 120°，与需求「初始帧尽量一致」冲突）。

**失败重排**（docs/71 §5，代价递增）：
`降速重整形 → 段内重采 → 换锚点区 → 换种子整条重来`，每级触发次数进产线健康指标。
"""

import json
import math
import os
import random
from dataclasses import dataclass, field
from pathlib import Path
from typing import Dict, List, Optional, Sequence, Set, Tuple

from . import cam4 as cam4mod
from . import cam4_topdown as tdmod
from . import covis as covismod
from . import metrics as met
from . import pink as pinkmod
from . import pink_honesty as phmod
from .anchors import (Anchor, Placement, Subject, allocate_placements,
                      make_anchor, make_subject, place_camera)
from .curve import KNOT_S
from .mathutil import (Vec3, clamp, euler_tuple, quat_from_yaw_elev_roll, vadd,
                       vdist, vnorm, vscale, vsub, wrap180, yaw_elev_from_forward)
from .primitives import (ELEV_LIMIT, EXTENT_DRIVEN, FAMILIES, FAMILY_OF, MOVING,
                         PINK, QUOTA, STATIONARY, DEFAULT_DURATION, DURATION_RANGE,
                         Ctx, Move, Pose, look_at)
from .sampler import (SHELL_MARGIN_M, Track, build_smooth, frame_speeds,
                      frames_from_times, sample_track)
from .scene import Scene

FPS = 30.0
DURATION_S = 60.0            # MC 单条 60s 整（docs/72 裁决）
N_FRAMES = 1800

#: 画幅 → (宽, 高)。竖屏按 1080P 档位如实出（docs/71 §11 C-2）
#: 画幅标签 → 像素尺寸。**照需求方措辞逐字**（69 号 §2「横屏 3:4」「竖屏 4:3」）：
#: `3:4` 是横屏 1440x1080、`4:3` 是竖屏 1080x1440——看着与常规 W:H 记法相反，
#: 但采集侧 `CaptureFormats.cs` 也是这么定的（且注明「do NOT fix」），
#: 两侧必须一致。此前这里恰好写反了：交付文件由采集侧写、自洽没问题，
#: 但**规划侧**拿错了画幅去算内参与共视——4:3 那条 fx 差了 33%（2250 vs 3000），
#: 共视闸门等于在错误的视锥上判的。
ASPECT_RES: Dict[str, Tuple[int, int]] = {
    "16:9": (1920, 1080), "9:16": (1080, 1920),
    "3:4": (1440, 1080), "4:3": (1080, 1440),
}
#: 画幅批次级配额（docs/69 §2）
ASPECT_QUOTA: Dict[str, float] = {"16:9": 0.60, "3:4": 0.10, "4:3": 0.10, "9:16": 0.20}
#: 焦距档位（各 25%，一条内 4 路正好各占一档）
FOCALS: Tuple[int, ...] = (18, 24, 35, 50)

#: 逐段速度目标的工作区间（m/s）——分层采样，覆盖 docs/69 表 2 的速度分布要求
SPEED_BAND = (0.35, 3.4)
#: guide 路的速度区间（docs/71 §6 传导预算：v_g 压低才让得出高 ρ 段）
GUIDE_SPEED_BAND = (0.3, 1.5)
#: 位置基本不动的类目的名义速度（不能取 0，否则段内进度归一化退化）
STATIONARY_SPEED = 0.03
#: 粉色平移段的 extent 下限（m）。诚实性门 0.8m + 样条稀释余量（实测稀释最差 ~0.45×，
#: 残差由 pink_honesty.enforce 的修正场兜住，这里只负责把修正量压小）。
PINK_MIN_EXTENT_M = 1.7
#: 粉色平移段的时长下限（s）——见 build_moves 内注释。
PINK_MIN_DUR_S = 2.2
#: 第四路标称轨道的转速限幅倍率（τ′>1 会把标称角速度放大，标称侧先收紧）
CAM4_SLEW_SCALE = 0.8
#: 第四路标称轨道的节点间隔倍率（标称只是模板路径，粗节点 ⇒ 自身 jerk ÷ 倍率³）
CAM4_KNOT_MULT = 2.0
#: 标称常速取推进速率 `ρ·v_g` 的哪个分位（见 `_build_cam4`：τ′≤1 才不放大 jerk）
CAM4_VCONST_PCT = 95.0
#: 第四路标称轨道相对「会被走到的前缀」的弧长余量。必须 > cam4.LENGTH_MARGIN，
#: 否则 `arc.length >= need` 过不去；又不能大太多，多出来的尾巴永远走不到。
CAM4_ARC_MARGIN = 1.15
#: 被锁进共享扇区的两路的半张角（度）——两路合起来 ≤2×，落在 §5 的 90–120° 弧内
SECTOR_HALF_DEG = 32.0
#: 共视窗口占全片的比例。需求要 ≥50%，这里排 62% 给 QC 精算与人审留余量。
COVIS_WINDOW_FRAC = 0.62
#: 朝向进出窗口的余弦斜坡（s）。硬切会把 α/角 jerk 打爆。
COVIS_RAMP_S = 1.2
#: 位置收敛/退出队形时允许的**附加位移速率**（m/s）。位置修正不用定长斜坡：
#: 修正量是不定大小的（原轨迹可以离队形十几米），定长斜坡会把它折算成
#: 十几 m/s 的瞬时速度；这里给速率一个构造性上界，进出时长由修正量自己决定。
#: 取值贴着「段速上限 3.4 + 修正 1.2 < p95 门 6」留余量。
COVIS_CORR_RATE = 1.2
#: 修正场低通的半宽（帧）。限幅器给的是有界但**不连续**的速率，低通把它抹成
#: 连续加速度：半宽 12 帧 ⇒ 附加加速度 ≲ 1.2/0.4 = 3 m/s²、附加 jerk ≲ 8，
#: 都在表 2 的 8 / 20 之内。
COVIS_CORR_LP_HALF = 12
#: 壳内回投量的低通半宽（帧）。比修正场窄：回投是局部的，抹太宽会把推出量摊没。
COVIS_FIX_LP_HALF = 5
#: 窗口内三路方位角相对共享中心的半张角（度）。实测 ±25° 最优：
#: 收得更紧（±15）反而降 —— 三路挤在一起时基线太短，min 口径的 overlap 也上不去。
COVIS_AZ_HALF_DEG = 25.0
#: 窗口内相机相对锚点的最大俯仰正切（0.45 ≈ 24°）
COVIS_ELEV_TAN = 0.45
#: 24mm 基准轨道半径（m）。半径按焦距等比放大 ⇒ 各路**画面足迹尺度一致**，
#: 这是 min 口径共视能过 0.30 的前提：足迹差一个量级时窄镜头画面会被宽镜头完全包含，
#: O(宽→窄) 只有零点几，min 必挂（docs/71 §5「焦距越长分配越远轨道半径」）。
RADIUS_AT_24MM = 6.0
#: 环带绝对上下限（m）：下限避免相机贴进主体，上限压住远景路的行程
RADIUS_ABS = (2.2, 16.0)
#: 环带相对基准的比例带（`radius_band` 的 0.85~1.25），带宽 1.47× 见该函数文档
RADIUS_BAND_K = (0.85, 1.25)
#: 最外一路（50mm 上界）相对 `r_base` 的倍率 —— **场景可行性必须按它核**，
#: 而不是按 24mm 基准：`_open_center` 量出的可站人半径若直接当 r_base，
#: 50mm 那路会跑到 2.6 倍远处（实测室内 12m 见方的房间里排到 15.6m）。
RADIUS_OUTER_SCALE = RADIUS_BAND_K[1] * max(FOCALS) / 24.0
#: `r_base` 的可用区间：出了这个区间，`radius_band` 的绝对夹持就会咬进来、
#: 各路 `r/f` 不再相等，于是退回 §10.1 发现 2 的「混焦距结构性封顶」。
R_BASE_RANGE = (RADIUS_ABS[0] / (RADIUS_BAND_K[0] * min(FOCALS) / 24.0),
                RADIUS_ABS[1] / RADIUS_OUTER_SCALE)
#: 自由壳投影允许的最大节点位移（m），超过即判可行域冲突
MAX_PUSH_M = 4.0
#: 锚点通视性预筛：一圈探针数与「既站得住又对中心畅通」的最低占比（docs/78 §10.6 同款）
RING_PROBES = 16
OPEN_RING_FRAC = 0.75
#: 锚点活动中心周围统计可站面格的半径（m）。0 = 关闭（默认，保持既有行为）。
_ANCHOR_GROUND_M = float(os.environ.get("DIOGENES_ANCHOR_GROUND_M", "0"))
#: 可站面格数记到多少就算「够了」——再多不额外加分，把选择权交回给开阔度与通视。
_ANCHOR_GROUND_N = int(os.environ.get("DIOGENES_ANCHOR_GROUND_N", "10"))


# ---------------------------------------------------------------- 配额

class QuotaTracker:
    """语料级配额跟踪：按「含该类的相机轨道占比」统计，缺口驱动抽样权重。"""

    def __init__(self) -> None:
        self.tracks = 0
        self.cls_hits: Dict[str, int] = {c: 0 for c in QUOTA}
        self.focal_hits: Dict[int, int] = {f: 0 for f in FOCALS}
        self.aspect_hits: Dict[str, int] = {a: 0 for a in ASPECT_RES}
        self.clips = 0

    def weight(self, cls: str) -> float:
        op, target = QUOTA[cls]
        if self.tracks == 0:
            return 1.0
        cur = self.cls_hits[cls] / self.tracks
        if op == "<=":
            return 0.05 if cur >= target else 1.0
        return max(0.05, 1.0 + 4.0 * (target - cur))

    def note_track(self, classes: Sequence[str], focal: int) -> None:
        self.tracks += 1
        for c in set(classes):
            if c in self.cls_hits:
                self.cls_hits[c] += 1
        self.focal_hits[focal] = self.focal_hits.get(focal, 0) + 1

    def note_clip(self, aspect: str) -> None:
        self.clips += 1
        self.aspect_hits[aspect] = self.aspect_hits.get(aspect, 0) + 1

    def pick_aspect(self, rng: random.Random) -> str:
        """确定性补齐：每次取当前欠账最大的画幅。

        旧实现按欠账**加权随机**抽，只保证「期望命中」——12 条实测
        50/8/25/17 vs 目标 60/10/10/20（4:3 超额一倍半，16:9 差 10pt），
        且 `max(0.02, deficit)` 让已超额的档仍保底 2% 权重，进一步拖慢收敛。
        docs/71 §4 要的是排产器**跨条命中**，小批量下必须确定性补齐：
        取 argmax(target_share − actual_share)，n 条后每档偏差 ≤1 条。
        并列时用 rng 打破，避免固定顺序在跨批账本上积累相关性。
        """
        if self.clips == 0:
            return "16:9"
        deficits = {a: ASPECT_QUOTA[a] - self.aspect_hits.get(a, 0) / self.clips
                    for a in ASPECT_RES}
        best = max(deficits.values())
        tied = [a for a in ASPECT_RES if deficits[a] >= best - 1e-9]
        return tied[0] if len(tied) == 1 else rng.choice(tied)

    def report(self) -> Dict:
        t = max(1, self.tracks)
        c = max(1, self.clips)
        return {
            "tracks": self.tracks, "clips": self.clips,
            "cls_share": {k: round(v / t, 4) for k, v in sorted(self.cls_hits.items())},
            "cls_target": {k: "%s%.2f" % (op, tgt) for k, (op, tgt) in sorted(QUOTA.items())},
            "focal_share": {str(k): round(v / max(1, sum(self.focal_hits.values())), 4)
                            for k, v in sorted(self.focal_hits.items())},
            "aspect_share": {k: round(v / c, 4) for k, v in sorted(self.aspect_hits.items())},
            "aspect_target": ASPECT_QUOTA,
        }

    # ---- 账本持久化 ----
    # 配额是**语料级**统计：每次进程各建一个 QuotaTracker 的话 `pick_aspect`
    # 永远见到 clips=0 ⇒ 恒回 16:9，60/10/10/20 结构性不可达。跨进程/跨条
    # 生效必须把计数快照落盘。`pick_aspect` 依赖的字段是 `clips` 与
    # `aspect_hits`，`weight` 还要 `tracks`/`cls_hits`——四组计数全量序列化，
    # 键名与 megacity_capture.py 既有 ledger.json 保持互通（report 键仅供人读）。

    def to_dict(self) -> Dict:
        return {
            "tracks": self.tracks, "clips": self.clips,
            "cls_hits": dict(self.cls_hits),
            "focal_hits": {str(k): v for k, v in self.focal_hits.items()},
            "aspect_hits": dict(self.aspect_hits),
            "report": self.report(),
        }

    @classmethod
    def from_dict(cls, data: Dict) -> "QuotaTracker":
        q = cls()
        q.tracks = int(data.get("tracks", 0))
        q.clips = int(data.get("clips", 0))
        q.cls_hits.update(data.get("cls_hits", {}))
        q.focal_hits.update({int(k): int(v)
                             for k, v in data.get("focal_hits", {}).items()})
        q.aspect_hits.update(data.get("aspect_hits", {}))
        return q

    @classmethod
    def load(cls, path) -> "QuotaTracker":
        """从账本 JSON 恢复；文件不存在＝全新账本（首条照旧 16:9 起步）。"""
        p = Path(path)
        if not p.exists():
            return cls()
        return cls.from_dict(json.loads(p.read_text(encoding="utf-8")))

    def save(self, path) -> None:
        p = Path(path)
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(json.dumps(self.to_dict(), ensure_ascii=False, indent=2),
                     encoding="utf-8")


# ---------------------------------------------------------------- 分段计划

def _pick_nonpink(rng: random.Random, quota: QuotaTracker, chosen: Sequence[str],
                  allowed: Optional[Set[str]] = None) -> str:
    pool = [c for c in QUOTA if allowed is None or c in allowed]
    weights = []
    for c in pool:
        w = quota.weight(c)
        if c in chosen:
            w *= 0.25            # 同一条内不鼓励重复同类
        weights.append(w)
    return rng.choices(pool, weights=weights)[0]


def build_moves(rng: random.Random, quota: QuotaTracker, duration: float,
                allowed: Optional[Set[str]] = None, with_pink: bool = True,
                pink_window: Optional[float] = None) -> List[Move]:
    """一路的 move 时间线：n_seg 段，每段 ≥1 非粉色；6 个粉色平移分散嵌入。

    `pink_window`：6 类粉色平移必须全部收在 `[0, pink_window]` 内（默认全程）。
    第四路专用——它的标称时间轴末尾会被时间重参数化截掉，排在截断点之后的
    粉色平移等于没排（`_remap_cam4_moves` 直接丢段）。
    """
    n_seg = rng.randint(1, 10)
    n_np = max(n_seg, rng.randint(4, 8))
    per_seg = [1] * n_seg
    for _ in range(n_np - n_seg):
        per_seg[rng.randrange(n_seg)] += 1

    # 每条只抽 2–3 个**不同**的非粉色类目，段位再从这个小集合里重复取。
    # 语料级配额的分母是「含该类的相机轨道数」，Σ 配额 ≈2.5；
    # 若每条都用 6 个不同类目，Σ 实际占比必然 ≈6，所有类目一起超标（实测全线 5–6 倍）。
    n_distinct = min(rng.randint(2, 3), n_np)
    palette: List[str] = []
    for _ in range(n_distinct):
        palette.append(_pick_nonpink(rng, quota, palette, allowed))
    items: List[Tuple[int, str]] = []            # (段号, 类名)
    for seg in range(n_seg):
        for _ in range(per_seg[seg]):
            items.append((seg, palette[rng.randrange(len(palette))]))
    if with_pink:
        # 粉色插入位置的上界：默认全程随机；给了 pink_window 就只插进前缀，
        # 使 6 段粉色的累计结束时间落在窗口内（时长是插入后才抽的，故下面复核）。
        frac = 1.0 if pink_window is None else max(0.0, min(1.0, pink_window / duration))
        base = list(items)
        for _ in range(8):
            items = list(base)
            cap = len(items) if frac >= 1.0 else max(
                0, min(len(items), int(frac * (len(items) + len(PINK))) - len(PINK)))
            pink = list(PINK)
            rng.shuffle(pink)
            for c in pink:                        # 粉色每条必含、时间随机
                items.insert(rng.randrange(cap + 1), (rng.randrange(n_seg), c))
            if frac >= 1.0:
                break
            # 用同一套时长分布试算，确认最后一段粉色确实收在窗口内
            trial = [rng.uniform(*DURATION_RANGE.get(
                c, DEFAULT_DURATION if c not in PINK else (1.5, 5.0))) for _, c in items]
            kk = duration / sum(trial)
            t_end, last_pink = 0.0, 0.0
            for (_, c), w in zip(items, trial):
                t_end += max(4 * KNOT_S, w * kk)
                if c in PINK:
                    last_pink = t_end
            if last_pink <= pink_window:
                break
            frac *= 0.8

    raw = []
    for _, c in items:
        lo, hi = DURATION_RANGE.get(c, DEFAULT_DURATION if c not in PINK else (1.5, 5.0))
        raw.append(rng.uniform(lo, hi))

    # 段时长下限（样条至少 4 个节点才画得出这一段）。一旦有段被地板抬起来，
    # Σd 就 > duration，原先逐段累加会直接截尾——被截掉的尾巴里可能正是
    # 「每条必含」的粉色平移（cam4 实测只剩 2/6 类）。
    # 这里改成**丢段**而不是**压段**：把最短的非粉色段丢掉直到人人都够地板。
    # 压段（按比例缩到 Σd==duration）同样能不截尾，但会把位移塞进更短的时间里，
    # 抬高 v/a/jerk —— 实测把闸门通过率从 55% 打到 40%。
    # 粉色段另有更高的地板：诚实性修正场的增益帽随窗长立方缩（jerk 预算），
    # 1.2s 的窗帽到 0.3m 连稀释缺口都补不上——2.2s 起修正场才有工作空间。
    floor_d = 4 * KNOT_S

    def _floor(cls: str) -> float:
        return PINK_MIN_DUR_S if cls in PINK else floor_d

    while len(items) > 1:
        k = duration / sum(raw)
        if all(w * k >= _floor(c) for (_, c), w in zip(items, raw)):
            break
        cand = [i for i in range(len(items)) if items[i][1] not in PINK]
        if not cand:                          # 只剩粉色，不能再丢
            break
        j = min(cand, key=lambda i: raw[i])
        items.pop(j)
        raw.pop(j)

    k = duration / sum(raw)
    moves: List[Move] = []
    t = 0.0
    for (seg, c), w in zip(items, raw):
        d = max(_floor(c), w * k)
        moves.append(Move(cls=c, t0=t, t1=min(duration, t + d), seg=seg))
        t = moves[-1].t1
    moves[-1].t1 = duration
    return moves


def assign_speeds(rng: random.Random, moves: Sequence[Move],
                  band: Tuple[float, float] = SPEED_BAND
                  ) -> List[Tuple[float, float]]:
    """逐段速度目标：工作区间**分层采样**（docs/71 §4「速度均匀分布」G-20）。"""
    moving = [i for i, m in enumerate(moves) if m.cls not in STATIONARY]
    lo, hi = band
    strata = []
    n = max(1, len(moving))
    for i in range(n):
        a = lo + (hi - lo) * i / n
        b = lo + (hi - lo) * (i + 1) / n
        strata.append(rng.uniform(a, b))
    rng.shuffle(strata)
    marks: List[Tuple[float, float]] = []
    it = iter(strata)
    for m in moves:
        v = STATIONARY_SPEED if m.cls in STATIONARY else next(it)
        if m.cls in PINK:
            # 诚实性契约（QC plan.pink_honesty）：声称轴向净位移 ≥0.8m。
            # 慢速档 × 短窗给出的 extent 可以低到 0.5m，再经样条稀释（实测最差
            # ~0.45×）就结构性不达标——粉色段的排速对 extent 抬底，其余段不动。
            # cam4 标称走 (v_const, v_const) 带时同样生效：粉色段标称加速只会让
            # τ′=ρ·v_g/v_标称 变小，jerk 放大界不受影响（docs/71 §6）。
            v = max(v, PINK_MIN_EXTENT_M / max(0.5, m.duration))
        m.params["v_target"] = round(v, 3)
        m.params["extent"] = round(v * m.duration, 3)
        marks.append((m.t0, v))
    return marks


# ---------------------------------------------------------------- 计划结果

@dataclass
class ClipPlan:
    seed: int
    scene: Scene
    tracks: List[Track]
    #: 第四路形态计划：Cam4Plan（形态 a·timewarp）或 Cam4TopdownPlan（形态 b），
    #: 二者都有 `mode` 标签与 `guide_cam`/`to_dict()`（docs/69 §8.3）。
    cam4_plan: cam4mod.Cam4Plan
    aspect: str
    anchor: Anchor
    subjects: List[Subject]
    physics: Dict[int, met.Physics] = field(default_factory=dict)
    decouple: Optional[met.Decouple] = None
    covis: Optional[covismod.CovisResult] = None
    init_angle: float = 0.0
    init_dist: float = 0.0
    ok: bool = False
    reasons: List[str] = field(default_factory=list)
    retries: Dict[str, int] = field(default_factory=dict)
    loop_closure_m: float = 0.0
    cam4_check: Optional[met.Cam4Check] = None
    #: 形态 b 的三条验形实测（与 QC `cam4.topdown_*` 逐字同口径，进 provenance）
    cam4_topdown: Optional[met.Cam4TopdownCheck] = None
    min_clearance_m: float = 0.0
    max_push_m: float = 0.0
    #: 计划时长与帧数。默认 = 交付口径 60s/1800 帧（main 全局常量，行为不变）；
    #: 车辆导演模式的能力演示档（docs/78 §10.3）用它承载非 60s 时长。
    duration_s: float = DURATION_S
    n_frames: int = N_FRAMES
    #: 共视窗口 `[w0, w1]` 与场景开阔度基准半径。**降速重整形要按原样重建窗口**，
    #: 所以这两个量必须随计划走：`_reshape_speed` 会把前三路整条重采
    #: （`sample_track` 从 move 模板重算逐帧位姿），窗口收敛于是被抹掉；
    #: 而重整形只在物理超限时触发、触发后 `_selfcheck` 立刻复核共视 ——
    #: 不重建就等于「拿没有窗口的轨迹去判共视」，实测这是严读下最大的一处漏水
    #: （重整形平均每条 7.9 次，凡末尾落在重整形的计划共视全靠运气）。
    covis_window: Tuple[float, float] = (0.0, 0.0)
    r_base: float = RADIUS_AT_24MM
    #: 按画幅缩放后的共视扇区半张角（度）——重整形要按同一个窗口重建
    covis_az_half: float = COVIS_AZ_HALF_DEG

    def resolution(self) -> Tuple[int, int]:
        return ASPECT_RES[self.aspect]

    def intrinsics(self, focal_mm: int) -> Dict[str, float]:
        """35mm 全画幅纵向等效：`fx=fy=(H/2)·f/12`，`cx=W/2, cy=H/2`（docs/70 §4.1）。"""
        w, h = self.resolution()
        f = (h / 2.0) * focal_mm / 12.0
        return {"fx": round(f, 4), "fy": round(f, 4), "cx": w / 2.0, "cy": h / 2.0}

    def to_plan_dict(self) -> Dict:
        return {
            "version": "1.0",
            "seed": self.seed,
            "mode": "mc",
            "scene": self.scene.name,
            "duration_s": DURATION_S,
            "fps": FPS,
            "cameras": [t.to_plan_dict() for t in self.tracks],
            "anchors": [{"t": round(t, 2), "p": [round(x, 3) for x in self.anchor.at(t)],
                         "radius": round(self.anchor.radius, 3)}
                        for t in [i * 5.0 for i in range(13)]],
            "covis_windows": [{"t": round(t, 2), "pair_min_overlap": round(v, 4)}
                              for t, v in (self.covis.ts if self.covis else [])],
            "cam4": self.cam4_plan.to_dict(),
            "scene_kind": self.scene.kind,
            "aspect": self.aspect,
            "trajgen": {
                "knot_s": KNOT_S,
                "retries": self.retries,
                "notes": "朝向欧拉角按 Unity eulerAngles 口径导出（顺规待需求方确认，docs/72 C3）",
            },
        }


# ---------------------------------------------------------------- 生成主流程

def _open_center(scene: Scene, rng: random.Random, tries: int = 24
                 ) -> Tuple[Vec3, float]:
    """挑一个**四周开阔**的活动中心：候选里取工作环带上自由点最多的那个。

    这是比「事后把轨迹推出障碍」更靠前的一道：相机全程收在锚点的 [r_min, r_max]
    环带内，环带本身开阔，穿模就基本不会发生（docs/71 §7 把占据图放在防线 1，
    §5 R9 也点名走廊类场景可行域薄——那类场景本来就该在排产里降权）。

    返回的是 **`r_base`（24mm 基准半径）**，不是量到的可用半径：量到的是
    「相机能站到多远」，而站得最远的是 50mm 那路（`RADIUS_OUTER_SCALE` 倍）。
    此前这里把量出的可用半径直接当 r_base 返回，于是可行性核在 24mm 那一路、
    真正贴墙的却是 50mm 那路 —— 室内 12m 见方的房间量出 usable=6，
    最外路却被排到 15.6m，`_line_start` 找不到共线起点、自由壳投影每个节点推 3m
    （走廊 jerk p50 27 > 门 20，全部挂在这）。

    **通视性预筛**：一圈探针里要求 `OPEN_RING_FRAC` 以上既站得住、又对中心畅通，
    才算这个半径「可用」。原来的门是 12 个里有 4 个（33%），意味着一个只从三分之一
    方向看得见的中心也算合格 —— 三路收进共享扇区后，扇区落在被墙挡住的那三分之二
    里是大概率事件，overlap 直接归零。这是 §10.6 里导演模式踩过的同一个坑
    （「注视中心被楼体遮挡 13s ⇒ 通视性预筛，一圈 16 点要求 ≥75% 畅通」），
    经典模式一直没跟上；室内类残余失败里共视是头号（11 条中 7 条是唯一因）。
    """
    best, best_score, best_r = None, (-1, -1.0, -1.0), RADIUS_AT_24MM * RADIUS_OUTER_SCALE
    for _ in range(tries):
        c = scene.sample_free(rng)
        if c is None:
            continue
        # 逐圈往外探，记下「大半圈可站人且通视」的最大半径 —— 走廊/隧道类场景
        # 的可用半径只有 1–2m，硬按 24mm 基准的 6m 排相机必然贴墙（R9）。
        usable = 1.5
        total = 0
        for r in (1.5, 2.5, 4.0, 6.0, 9.0, 13.0, 17.0):
            free = 0
            for k in range(RING_PROBES):
                a = 2.0 * math.pi * k / RING_PROBES
                q = (c[0] + r * math.cos(a), c[1], c[2] + r * math.sin(a))
                if scene.free(q) and not scene.blocked(q, c):
                    free += 1
            total += free
            if free >= OPEN_RING_FRAC * RING_PROBES:
                usable = r
        # **优先挑动态物体站得住的地方**（env `DIOGENES_ANCHOR_GROUND_M`）。
        # 自由空间 ≠ 可站面：马路正中、大停车场都是自由的，但行人不可能在那儿。
        # 四路全部 look-at 锚点区，锚点落在这种地方 ⇒ 四路一起对着一片
        # 什么都不可能有的区域。CARLA 批实测过一路 60s 里 40s 视锥内动态物体恒为 0，
        # 把动态物体从 115 加到 230 个也只把 active_frac 从 0.17 抬到 0.23——
        # 不是密度问题，是那片地方**在导航网格上根本不存在**。
        #
        # 做成**排序键的最高位**而不是硬过滤：硬过滤时若一轮候选全落空，
        # 函数会退回原点那个退化中心，整条计划随后自检不过，成功率直接崩
        # （实测十分钟只出 1 条）。当键用则「有可站面的优先，没有也不至于没结果」。
        g = (scene.ground_count(c, _ANCHOR_GROUND_M, cap=_ANCHOR_GROUND_N)
             if _ANCHOR_GROUND_M > 0 else 0)
        # 先比可用半径、再比总畅通数：两个都要，只比总数会选出「近处开阔、
        # 远处全堵」的中心，只比半径会在并列时挑到最憋的那个。
        if (g, usable, total) > best_score:
            best, best_score, best_r = c, (g, usable, total), usable
    return (best or (0.0, scene.ground_y + 1.6, 0.0),
            clamp(best_r / RADIUS_OUTER_SCALE, *R_BASE_RANGE))


def aspect_az_scale(aspect: str) -> float:
    """画幅 → 方位预算的缩放系数（以 16:9 为 1.0）。

    共视重叠沿**水平**方向丢失得最快：两路方位差 Δ 时地面斑块横移 ≈ r·Δ，
    而斑块半宽 = r·tan(hfov/2)，所以「能容忍多大方位差」正比于 `tan(hfov/2)`。
    内参是纵向等效（`fx=fy=(H/2)·f/12`），于是 `tan(hfov/2) = (W/H)·12/f` ——
    对同一焦距，**只随画幅的宽高比变**，缩放系数就是宽高比之比。

    这一档此前完全没建模：±25°/±32° 两个常数是在 16:9 上调出来的，
    竖屏 9:16 的水平 FOV 只有 31°（16:9 是 83°），同样的扇区等于把三路
    摊开到画面宽度的三倍开外。实测过率 16:9 58% / 3:4 52% / 4:3 35% / 9:16 15%，
    covis p50 依次 0.65/0.62/0.50/0.33 —— 与 tan(hfov/2) 单调对应，
    而竖屏在语料配额里占 30%（9:16 20% + 4:3 10%）。
    """
    w, h = ASPECT_RES[aspect]
    wr, hr = ASPECT_RES["16:9"]
    return (w / h) / (wr / hr)


def radius_band(focal_mm: int, r_base: float = RADIUS_AT_24MM) -> Tuple[float, float]:
    """焦距 → 轨道半径工作带（m）。`r/f` 恒定 ⇒ 各路画面足迹尺度一致。

    `r_base` 由场景局部开阔度给（`_open_center`）：走廊里 6m 基准放不下。

    **带宽是共视严读的决定性参数**。共视用双向 `min(O(A→B),O(B→A))`：一路贴近
    锚点、另一路拉远时，近路画面被远路完全包含，`O(近→远)≈1` 而 `O(远→近)` 很小，
    min 就塌了。实测掉线对的足迹比（`距离/焦距`）中位 2.17、p90 5.94，
    达标对只有 1.30 / 1.91 —— 原来的 0.65~1.5（带宽 2.3×）允许两路各自跑到带两端，
    正好落进掉线区。收到 0.85~1.25（带宽 1.47×）后跨路失配上界即 1.47，
    稳在达标区间。
    条内足迹收紧，**跨条**的远近变化由 `r_base`（场景开阔度）承担，
    §3.3「离目标 1.5~4 倍」的分布在语料级仍然覆盖。
    """
    mid = r_base * focal_mm / 24.0
    return max(2.2, 0.85 * mid), min(16.0, 1.25 * mid)


def _lowpass(vals: Sequence[float], half: int, passes: int = 3) -> List[float]:
    """等宽滑动平均跑几遍（≈高斯），端点自适应收窄。前缀和实现，O(n·passes)。"""
    out = list(vals)
    n = len(out)
    for _ in range(passes):
        pre = [0.0]
        for v in out:
            pre.append(pre[-1] + v)
        out = [(pre[min(n - 1, i + half) + 1] - pre[max(0, i - half)])
               / (min(n - 1, i + half) - max(0, i - half) + 1) for i in range(n)]
    return out


def _force_covis_window(scene: Scene, tracks: Sequence[Track], anchor: Anchor,
                        r_base: float, w0: float, w1: float, guide_i: int = -1,
                        az_half_deg: float = COVIS_AZ_HALF_DEG) -> None:
    """把前三路在 `[w0, w1]` 窗口内收敛到「同锚点、同足迹尺度、同扇区」。**就地改**。

    需求 §0 的共视是**严读**：前三路三对**全部** overlap ≥0.30 才算这一采样点共视。
    实测三条硬约束缺一不可（各自单独施加时严读 p50 分别只到 0.43/0.39/0.33，
    三条合施才到 0.59；基线 0.33）：

    1. **朝向**看同一锚点 —— `pan_*` 类的 gaze 偏离锚点中位 21–58°，一超出半个 FOV
       该对就归零；
    2. **足迹尺度**（`距离/焦距`）对齐 —— 双向 `min(O(A→B),O(B→A))` 对尺度失配极敏感：
       近的一路整幅被远的一路包住时 `O(近→远)≈1` 而 `O(远→近)` 很小，min 就塌。
       实测掉线对的足迹比中位 2.17，达标对只有 1.30；而**实际有 61% 的采样点
       跑在 `radius_band` 之外**——原来的径向预算只在单个 move 内生效，跨 move 会漂。
    3. **方位**收进共享扇区 —— 三路绕到锚点两侧时基线角过大，overlap 按 §5 的
       基线角衰减掉线。

    窗口外不动，保住「轨迹随机、避免机械重复」（§4.1）与摇动/跟随类的配额。
    进出用余弦斜坡渐变，避免位置/朝向硬切把 a/jerk 打爆。
    """
    a0 = anchor.at(0.0)
    cen = math.degrees(math.atan2(
        sum(t.positions[0][2] - a0[2] for t in tracks) / len(tracks),
        sum(t.positions[0][0] - a0[0] for t in tracks) / len(tracks)))
    for gi, tr in enumerate(tracks):
        r_lo, r_hi = radius_band(tr.focal_mm, r_base)
        # **guide 路只收朝向、不动位置**：cam4 的 ρ 是反算的 `v₄/v_g`，
        # 夹持会在 guide 的速度上留台阶、甚至把它短暂钉住（v_g→0，样本被丢弃），
        # `cam4.switch_count` 于是把噪声判成额外变速——实测 8 条计划失败里 4 条挂在这。
        # 朝向收敛不碰位置，v_g 保持原样，ρ 反算不受影响。
        move_pos = (gi != guide_i)
        # 位置修正在**整条**上做：进出不再靠固定长度的斜坡，而是靠速率限幅
        # （见下），窗口外目标修正量恒为 0，于是自然收敛回原轨迹。
        idx = list(range(len(tr.positions))) if move_pos else []
        # 方位偏移先**沿时间展开**再夹持。硬 `wrap180` 夹持在扇区中心的对跖角上不连续：
        # 原始轨迹一旦从 off=+179° 走到 −179°，夹持目标就从 +25° 跳到 −25°，
        # 位置单帧平移 2·r·sin25° ≈ 0.85r（r≈8m ⇒ 6.8m/帧 = 204 m/s）。
        # 这与 #479「orbit 机头随瞬时 ω 翻转致单帧 5m 传送门」是同一类缺陷：
        # 对一个连续量取了带跳变的分支。展开后再用 `tanh` 平滑饱和，
        # 处处连续、小偏移近似恒等、大偏移收敛到扇区边缘。
        offs: List[float] = []
        prev: Optional[float] = None
        for i in idx:
            a = anchor.at(i / FPS)
            d = vsub(tr.positions[i], a)
            off = wrap180(math.degrees(math.atan2(d[2], d[0])) - cen)
            if prev is not None:
                off = prev + wrap180(off - prev)
            prev = off
            offs.append(off)
        # 修正量按**速率限幅**追随目标，不再用固定 1.2s 的余弦斜坡把它塞进去。
        # 斜坡是「定时长」的，而修正量是「不定大小」的（原轨迹可以离队形十几米）：
        # 斜坡中点 db/dt = π/2τ ≈ 1.31/s，15m 的修正在那里就是 20m/s，实测
        # 三类场景每条都能造出 >10m/s 的单帧（#474 的全路瞬时硬帽刚好照到）。
        # 限幅器让「附加位移速率」有构造性上界，进出窗口都是它自己走出来的，
        # 与 `sampler._slew_limit`、#479 的 `slew_yaw_series` 同一器件。
        corr = (0.0, 0.0, 0.0)
        step = COVIS_CORR_RATE * (1.0 / FPS)
        corrs: List[Vec3] = []
        for i, off in zip(idx, offs):
            t = i / FPS
            p = tr.positions[i]
            want = (0.0, 0.0, 0.0)
            if w0 <= t <= w1:
                a = anchor.at(t)
                d = vsub(p, a)
                flat = math.hypot(d[0], d[2])
                if vnorm(d) > 1e-6 and flat > 1e-6:
                    # 方位收进共享扇区（平滑饱和，见上）
                    az_t = math.radians(cen + az_half_deg
                                        * math.tanh(off / az_half_deg))
                    # 俯仰同样要收：三路从差异悬殊的高度俯看同一锚点时，看到的
                    # 地面斑块几乎不重合，min 口径照样掉线（窗口内实测的主因）。
                    dy = clamp(d[1], -COVIS_ELEV_TAN * flat, COVIS_ELEV_TAN * flat)
                    d = (flat * math.cos(az_t), dy, flat * math.sin(az_t))
                    # 半径夹到该焦距的工作环带
                    r = vnorm(d)
                    r_t = clamp(r, r_lo, r_hi)
                    if abs(r_t - r) > 1e-9:
                        d = vscale(d, r_t / r)
                    # 队形目标也要落在自由壳内。这一步此前缺席：自由壳投影在
                    # `smooth_from_knots` 里做完之后，本函数又把位置搬走了，
                    # 于是室内类每三条就有一条被搬进墙里 —— `clearance` 判负作废，
                    # 且相机埋在墙里什么都看不到，共视一并塌到 0.11~0.35。
                    want = vsub(scene.push_out(vadd(a, d), clearance=SHELL_MARGIN_M), p)
            dv = vsub(want, corr)
            n = vnorm(dv)
            if n > step:
                dv = vscale(dv, step / n)
            corr = vadd(corr, dv)
            corrs.append(corr)
        # 限幅器只保证速率有界，不保证速率**连续**：目标在 w0 处从 0 跳到十几米，
        # 限幅器随即以满速率启动，附加速度一帧内从 0 到 1.2m/s ⇒ a≈36、jerk 爆表。
        # 所以对修正场本身再过一道低通。低通只作用在修正量上、不碰原轨迹，
        # 因此不会削掉运镜原语的动作幅度（整条平滑会，实测 jerk 反升）。
        if corrs:
            lp = [_lowpass([c[k] for c in corrs], COVIS_CORR_LP_HALF) for k in range(3)]
            for j, i in enumerate(idx):
                p = tr.positions[i]
                tr.positions[i] = (p[0] + lp[0][j], p[1] + lp[1][j], p[2] + lp[2][j])
        # 回投自由壳。上面只保证「队形目标」在壳内，而实际落点是**限幅+低通后**的
        # 中间态，它可以横穿障碍（低通把两端都在壳内的修正抹成一条穿墙直线）。
        # 回投量同样过低通再加，保住光滑；连做两轮吃掉低通对推出量的稀释。
        for _ in range(2 if move_pos else 0):
            fix = [vsub(scene.push_out(tr.positions[i], clearance=SHELL_MARGIN_M),
                        tr.positions[i]) for i in idx]
            if max((vnorm(f) for f in fix), default=0.0) < 1e-6:
                break
            lp = [_lowpass([f[k] for f in fix], COVIS_FIX_LP_HALF) for k in range(3)]
            for j, i in enumerate(idx):
                p = tr.positions[i]
                tr.positions[i] = (p[0] + lp[0][j], p[1] + lp[1][j], p[2] + lp[2][j])
        # 夹持会在生效处留下折角，速度随之出现台阶。对 guide 路这是致命的：
        # cam4 的 ρ 是反算的 `v₄/v_g`，v_g 一毛糙，`cam4.switch_count` 就会把噪声
        # 判成额外的变速事件（实测把 cam4 类失败从 4 抬到 7）。这里对受影响区间
        # 做几遍二项平滑，端点钉死，速度回到光滑。
        lo, hi = 1, len(tr.positions) - 2
        for _ in range(6 if move_pos else 0):
            src = list(tr.positions)
            for i in range(lo, hi + 1):
                a, b, c = src[i - 1], src[i], src[i + 1]
                tr.positions[i] = (0.25 * a[0] + 0.5 * b[0] + 0.25 * c[0],
                                   0.25 * a[1] + 0.5 * b[1] + 0.25 * c[1],
                                   0.25 * a[2] + 0.5 * b[2] + 0.25 * c[2])

        # 朝向：窗口内看锚点，斜坡上与原朝向插值
        yaws, elevs, rolls = [], [], []
        for i in range(len(tr.positions)):
            t = i / FPS
            # `euler_tuple` 存的是 Unity 口径 [x=-elev, y=yaw, z=roll] 且折进 [0,360)，
            # 这里按同一口径反解回本包的 (yaw, elev, roll)。
            ex, ey, ez = tr.eulers[i]
            y0, e0, r0 = wrap180(ey), -wrap180(ex), wrap180(ez)
            if w0 - COVIS_RAMP_S <= t <= w1 + COVIS_RAMP_S:
                if t < w0:
                    b = 0.5 - 0.5 * math.cos(math.pi * (t - (w0 - COVIS_RAMP_S)) / COVIS_RAMP_S)
                elif t > w1:
                    b = 0.5 + 0.5 * math.cos(math.pi * (t - w1) / COVIS_RAMP_S)
                else:
                    b = 1.0
                b = clamp(b, 0.0, 1.0)
                ly, le = look_at(tr.positions[i], anchor.at(t))
                y0 = y0 + wrap180(ly - y0) * b
                e0 = e0 + (le - e0) * b
                r0 = r0 * (1.0 - b)
            yaws.append(y0)
            elevs.append(e0)
            rolls.append(r0)
        # **俯仰硬带要在这里再夹一次**：`sample_knots` 出口那一刀管不到本段——
        # 共视修正把相机位置朝锚点搬近之后，`look_at` 的仰角按 atan(Δy/Δ水平) 算，
        # 水平距离一小就爆掉。CARLA 批实测有一路顶到 **+83.6°**（目检就是整帧纯天空）、
        # 另一路探到 -89.3°，而 ELEV_LIMIT 是 (-30, +28)。
        # 夹在限速之前，折角由 `limit_angular_rate` 抹平，不引入角速度尖峰。
        lo_e, hi_e = ELEV_LIMIT
        elevs = [clamp(e, lo_e, hi_e) for e in elevs]
        # 收敛过程本身也要守转速帽（与第四路同一把尺）
        yaws, elevs, rolls = cam4mod.limit_angular_rate(
            yaws, elevs, rolls, met.ANGSPEED_P95_MAX * 0.85 / FPS)
        tr.quats = [quat_from_yaw_elev_roll(y, e, r)
                    for y, e, r in zip(yaws, elevs, rolls)]
        tr.eulers = [euler_tuple(y, e, r) for y, e, r in zip(yaws, elevs, rolls)]
        tr.speeds = frame_speeds(tr.positions, FPS)


def _line_start(scene: Scene, anchor: Anchor, focals: Sequence[int],
                rng: random.Random, r_base: float = RADIUS_AT_24MM
                ) -> Optional[List[Pose]]:
    """四路**沿同一方位线**、按各自焦距的半径排开起步。

    这样同时满足两件互相拉扯的事：
    - 69 §2「初始帧尽量一致」——四路共线看同一个锚点，首帧朝向夹角只差仰角那一点；
    - docs/71 §5 焦距-半径配比——起步就把足迹尺度对齐，共视不用靠运气。
    """
    c = anchor.at(0.0)
    for _ in range(50):
        az = rng.uniform(0, 2 * math.pi)
        poses: List[Pose] = []
        for f in focals:
            lo, hi = radius_band(f, r_base)
            placed = None
            for r in (rng.uniform(lo, hi), (lo + hi) / 2, lo, hi):
                p = (c[0] + r * math.cos(az),
                     scene.ground_y + rng.uniform(scene.eye_lo, scene.eye_hi),
                     c[2] + r * math.sin(az))
                if scene.free(p) and not scene.blocked(p, c):
                    placed = p
                    break
            if placed is None:
                break
            yaw, elev = yaw_elev_from_forward(vsub(c, placed))
            poses.append(Pose(placed, yaw + rng.uniform(-3.0, 3.0),
                              elev + rng.uniform(-2.0, 2.0), 0.0))
        if len(poses) == len(focals):
            return poses
    return None


def _min_clearance(plan: "ClipPlan", step: int = 10) -> float:
    """全程最小障碍间隙（docs/71 §7 防线 2）。≤0 即穿模，整条作废。"""
    worst = 1e9
    for tr in plan.tracks:
        for i in range(0, len(tr.positions), step):
            worst = min(worst, plan.scene.clearance(tr.positions[i]))
    return worst if worst < 1e9 else 0.0


def _loop_closure(track: Track) -> float:
    """循环路径类目的首末位置闭合误差（内控指标，QC-MC 无此条）。"""
    worst = 0.0
    for m in track.moves:
        if m.cls != "loop_path" or not track.positions:
            continue
        i0 = int(m.t0 * FPS)
        i1 = min(len(track.positions) - 1, int(m.t1 * FPS))
        worst = max(worst, vdist(track.positions[i0], track.positions[i1]))
    return worst


def _build_cam4(scene: Scene, rng: random.Random, quota: QuotaTracker, anchor: Anchor,
                subjects: Sequence[Subject], focals: Sequence[int], aspect: str,
                start: Pose, guide: Track, guide_cam: int,
                plan_fixed: Optional[cam4mod.Cam4Plan] = None,
                r_base: float = RADIUS_AT_24MM
                ) -> Optional[Tuple[cam4mod.Cam4Plan, Track]]:
    """按 guide 的实测速度构造第四路。

    `plan_fixed` 非空时复用已声明的 ρ/t_sw/Δ（降速重整形后重建用）——
    plan.json 里申报的 ρ 必须和轨迹里反算得到的 `v₄/v_g` 一致，
    否则 QC 的 `cam4.t_sw` 会拿申报值去核对实测切换点，直接 FAIL。
    """
    gv = cam4mod.guide_speeds(guide.positions, FPS)
    gv_p95, gv_max = met.percentile(gv, 95), max(gv)
    # 两条同时收：p95 走表 2 的 6m/s，瞬时走 §3.5 的 10m/s 硬帽
    rho_allowed = min(met.SPEED_P95_MAX * 0.85 / max(0.3, gv_p95),
                      cam4mod.V4_CAP * 0.70 / max(0.3, gv_max))
    c4plan = plan_fixed or cam4mod.sample_plan(rng, guide_cam, rho_max_allowed=rho_allowed)
    # 粉色可行性预检：保速换向的窗内净位移 ≈ 扣除两侧摆坡（~1.2s）后的 ∫v₄，
    # 排窗踩在 v₄ 谷架上，ρ 慢档 × 慢 guide 时谷间鼓包结构性凑不够 0.8m
    # （实测边缘计划的窗净位移 0.55-0.8 贴门下，白耗整个 attempt）。
    # 早退换 ρ 抽签比构建完标称再发现便宜一个量级；抽不到可行档直接把
    # attempt 还给上层（换 guide 才有救）。

    def _pink_feasible(plan_: cam4mod.Cam4Plan) -> bool:
        v4_pre = [plan_.rho((i + 0.5) / FPS) * gv[i] for i in range(N_FRAMES)]
        try:
            wins_pre = pinkmod.schedule_cam4_windows(rng, DURATION_S, v4_pre, FPS)
        except RuntimeError:
            return False
        return all(
            0.8 * sum(v4_pre[int(w.t0 * FPS):min(N_FRAMES, int(w.t1 * FPS))])
            / FPS >= 1.0 for w in wins_pre)

    feasible = plan_fixed is not None
    for _ in range(6):
        if feasible or _pink_feasible(c4plan):
            feasible = True
            break
        c4plan = cam4mod.sample_plan(rng, guide_cam, rho_max_allowed=rho_allowed)
    if not feasible:
        return None
    need = cam4mod.required_length(c4plan, gv, N_FRAMES, FPS)
    # 标称速度取**常值**，且取推进速率 ρ·v_g 的 p95 ——两条都是必须的：
    # ① 常值 ⇒ τ′ 不会在标称段界跳变（否则 ω₄ 直接打爆）；
    # ② 取 p95 而不是均值 ⇒ τ′=ρ·v_g/v_标称 ≤1，
    #    因为 `p₄‴ = P‴·τ′³ + 3P″·τ′·τ″ + P′·τ‴`，τ′ 一旦到 1.5–2，
    #    标称轨道自身十几的 jerk 会被立方放大到 40–90（实测过的主要失败模式）。
    rv = [c4plan.rho(i / FPS) * gv[i] for i in range(N_FRAMES)]
    v_const = clamp(met.percentile(rv, CAM4_VCONST_PCT), 0.35, 3.6)
    r4_lo, r4_hi = radius_band(focals[3], r_base)

    def _nominal(dur: float):
        # 标称**不排粉色**（with_pink=False）：第四路的粉色不再走「标称段 →
        # 重参数化窗」——那条链上样条稀释（0.6s 粗节点的五次样条支撑 ±1.8s，
        # 邻段几何大量渗入）与窗口重映射叠加，声称窗与实际几何对不上号
        # （实测 translate_down 窗里垂直位移恰 0、translate_back 恰好反向）。
        # 粉色改在**墙钟域**用导演模式的保速换向机制生成（见 _cam4_pink_run）。
        n_moves = build_moves(rng, quota, dur, allowed=EXTENT_DRIVEN,
                              with_pink=False)
        n_marks = assign_speeds(rng, n_moves, (v_const, v_const))
        trk = Track(4, focals[3], aspect, n_moves, n_marks,
                    Pose(start.p, start.yaw, start.elev, 0.0))
        ctx = Ctx(scene, anchor, list(subjects),
                  Placement(0.0, 4.0, 1 if rng.random() < 0.5 else -1), rng, dur,
                  r_min=r4_lo, r_max=r4_hi)
        sm = build_smooth(trk, ctx, dur, slew_scale=CAM4_SLEW_SCALE,
                          knot_s=KNOT_S * CAM4_KNOT_MULT)
        arc = cam4mod.ArcPath(sm)
        tab = cam4mod.arclength_table(sm, max(256, int(dur * FPS) + 1))
        return trk, sm, arc, tab

    # 标称轨道要**刚好够长**。按弧长推进走到 need/LENGTH_MARGIN 就停，
    # 故被走到的标称时长 = ext_dur / LENGTH_MARGIN——轨道每长出一分，
    # 尾部就多截掉一分（被截掉的非粉 move 由 `_remap_cam4_moves` 丢弃）。
    #
    # 弧长/标称秒的速率只能实测：旋转类运镜几乎不产生位移，实际速率远低于 v_const。
    _, _, probe, _ = _nominal(DURATION_S * 1.3)
    rate = probe.length / (DURATION_S * 1.3)
    ext_dur = need / rate * 1.02 if rate > 1e-6 else DURATION_S * 1.3
    fallback: Optional[Tuple[cam4mod.Cam4Plan, Track]] = None
    none_runs = 0
    for _ in range(6):
        n_track, nominal, arc, tab = _nominal(ext_dur)
        if arc.length < need:
            # 探针与正式抽样是**两次独立的 move 抽签**，旋转类占比一变弧长就差一截。
            # 直接按缺口拉长标称时长（缺多少补多少），比原先「拔高 v_const 重来」
            # 稳得多：v_const 顶到 3.6 就不再增长，后两轮等于空转，
            # 实测「第四路标称路径长度始终不足」是仅次于共视的整条作废原因。
            ext_dur *= clamp(need / max(arc.length, 1e-6) * 1.05, 1.12, 2.0)
            continue
        run = _cam4_pink_run(rng, c4plan, arc, tab, gv, anchor)
        if run is None:
            # 排窗不可行/净位移预算不足：换一掷还有机会（排窗有随机抖动），
            # 连着几掷都不行说明是 ρ×guide 的结构问题，交回上层换 attempt。
            none_runs += 1
            if none_runs >= 3:
                return fallback
            continue
        positions, quats, eulers, speeds, taus, wins, rows = run
        remapped = _remap_cam4_moves(n_track.moves, taus, FPS)
        # 粉色诚实性验收（QC plan.pink_honesty 同式，输入=最终位姿）：
        # 保速换向按构造几乎必真，这里复核的是排窗几何预测（theta_fn 的
        # 弧位估计忽略窗内冻结）偶发失准的情况——不诚实就换一次抽签。
        if not all(r["ok"] for r in rows):
            continue
        n_track.positions, n_track.quats = positions, quats
        n_track.eulers, n_track.speeds = eulers, speeds
        n_track.smooth = nominal
        n_track.moves = sorted(
            remapped + pinkmod.build_moves(wins, rows, len(remapped)),
            key=lambda m: m.t0)
        # **按交付帧核穿模**。`smooth_from_knots` 的自由壳投影迭代不收敛时是
        # 静默返回的（走廊类推不出去就推不出去），调用方拿不到任何信号；
        # 第四路又是唯一没有窗口回投兜底的一路，于是整条被 clearance 判死，
        # 实测穿模作废里三分之二挂在第四路、一挂就是连续 20s。
        # 换一次抽签比整条重来便宜，但**不能因此判死**：走廊类几乎抽不到干净的，
        # 硬拒会让第四路直接构造失败，把一条只是「有穿模」的计划变成一条
        # 连计划都没有的（实测走廊 3/12 → 0/12）。所以留作兜底候选。
        if min(scene.clearance(p) for p in positions[::5]) > 0.0:
            return c4plan, n_track
        if fallback is None:
            fallback = (c4plan, n_track)
        ext_dur *= 1.12
    return fallback


def _cam4_pink_run(rng: random.Random, c4plan: cam4mod.Cam4Plan,
                   arc: "cam4mod.ArcPath", tab, gv: Sequence[float],
                   anchor: Anchor):
    """第四路墙钟积分：s₄(t)=∫ρ·v_g 沿弧长推进 + 粉色保速换向（导演模式同款）。

    粉色窗内 `Cam4Driver` 显式换向、|v₄| 逐帧不变 ⇒ QC 的 ρ=|v₄|/|v_g| 反算
    无感，变速三条（switch_count/t_sw/band_dwell）不受影响；窗尾把位移折进
    恒定偏置 off，s₄ 冻结、位置连续。朝向全程 look-at 锚点（换向后位置偏离
    标称，标称烘焙的朝向会指错地方）+ 出口限幅。声称轴基取当帧 look-at yaw，
    与最终 quat 同式 ⇒ 预测即实测（`plan.pink_honesty` 的窗起帧基底）。
    """
    dt = 1.0 / FPS
    v4_pred = [c4plan.rho((i + 0.5) * dt) * gv[min(i, len(gv) - 1)]
               for i in range(N_FRAMES)]
    s4_pref = [0.0]
    for v in v4_pred:
        s4_pref.append(s4_pref[-1] + v * dt)

    def _theta4(cls_: str, t0_: float) -> float:
        i0_ = min(N_FRAMES - 1, max(0, int(t0_ * FPS)))
        s_ = min(max(0.0, arc.length - 0.5), s4_pref[i0_])
        ap_ = arc.at(s_)[0]
        ah_ = arc.at(min(arc.length, s_ + 0.4))[0]
        tv_ = vsub(ah_, ap_)
        tn_ = vnorm(tv_) or 1.0
        ly_, _le = look_at(ap_, anchor.at(t0_))
        ax_ = pinkmod.axis_of(cls_, ly_)
        dot_ = (ax_[0] * tv_[0] + ax_[1] * tv_[1] + ax_[2] * tv_[2]) / tn_
        return math.acos(clamp(dot_, -1.0, 1.0))

    try:
        wins = pinkmod.schedule_cam4_windows(rng, DURATION_S, v4_pred, FPS,
                                             theta_fn=_theta4)
    except RuntimeError:
        return None
    # 对**实际排出的窗**再核一次净位移预算（排窗内部有随机抖动，计划级预检的
    # 那一掷不能代表这一掷）：不够就整趟重掷，别把注定贴门下的窗跑完。
    for w in wins:
        i0 = int(w.t0 * FPS)
        i1 = min(N_FRAMES, int(w.t1 * FPS))
        if 0.8 * sum(v4_pred[i0:i1]) / FPS < 1.05:
            return None

    drv = pinkmod.Cam4Driver(wins, FPS)
    tab_ts, tab_cum = tab
    tab_k = (tab_cum[-1] / arc.length) if arc.length > 0 else 0.0
    s = 0.0
    off: Vec3 = (0.0, 0.0, 0.0)
    p_cur = arc.at(0.0)[0]
    positions: List[Vec3] = []
    yaws: List[float] = []
    elevs: List[float] = []
    taus: List[float] = []
    for i in range(N_FRAMES):
        t = i * dt
        positions.append(p_cur)
        taus.append(cam4mod._invert(tab_ts, tab_cum, s * tab_k))
        ly, le = look_at(p_cur, anchor.at(t))
        yaws.append(ly)
        elevs.append(clamp(le, -75.0, 75.0))
        ap = arc.at(s)[0]
        ahead = arc.at(min(arc.length, s + 0.05))[0]
        tv = vsub(ahead, ap)
        tn = vnorm(tv) or 1.0
        d4 = drv.direction(t, (tv[0] / tn, tv[1] / tn, tv[2] / tn),
                           v4_pred[i], ly)
        if d4 is None:
            s = min(arc.length, s + v4_pred[i] * dt)
            p_cur = vadd(arc.at(s)[0], off)
        else:
            p_cur = vadd(p_cur, vscale(d4, v4_pred[i] * dt))
            if drv.absorb_now:
                off = vsub(p_cur, arc.at(s)[0])

    yaws, elevs, rolls = cam4mod.limit_angular_rate(
        yaws, elevs, [0.0] * N_FRAMES, cam4mod.V4_OMEGA_CAP * dt)
    quats = [quat_from_yaw_elev_roll(y, e, r)
             for y, e, r in zip(yaws, elevs, rolls)]
    eulers = [euler_tuple(y, e, r) for y, e, r in zip(yaws, elevs, rolls)]
    speeds = frame_speeds(positions, FPS)
    rows = pinkmod.selfcheck(positions, quats, wins, FPS)
    return positions, quats, eulers, speeds, taus, wins, rows


def _enforce_pink(tracks3: Sequence[Track], scene: Scene) -> None:
    """对前三路施加粉色诚实性修正场并刷新速度（`pink_honesty.enforce`）。

    guide 路也修：S₅ 修正对速度是光滑调制，`v₄=ρ·v_g` 按修正后的 v_g 构造仍是
    恒等式，QC 反算的 ρ 不带噪声（与共视收敛「guide 不动位置」针对的硬夹持台阶
    不是一回事）。
    """
    for tr in tracks3:
        tr.positions = phmod.enforce(tr.positions, tr.quats, tr.moves,
                                     FPS, scene.ground_y)
        tr.speeds = frame_speeds(tr.positions, FPS)


def _build_cam4_topdown(scene: Scene, rng: random.Random, tracks3: Sequence[Track],
                        guide_i: int, focal: int, aspect: str,
                        plan_fixed: Optional[tdmod.Cam4TopdownPlan] = None
                        ) -> Tuple[tdmod.Cam4TopdownPlan, Track]:
    """形态 b（docs/69 §8.3）：高空垂直俯视、沿 guide 路线上空漂移，不追主体。

    与 `_build_cam4` 不共享任何契约：无 ρ/t_sw/禁带（QC 对 topdown SKIP 变速
    三条），验形走 `met.cam4_topdown_check`——判据逐字复刻 QC
    `_check_cam4_topdown`（#427 教训：两侧口径不一致 = 自检 100% 而 QC FAIL）。
    `plan_fixed` 非空时复用声明（降速重整形后 guide 变了轨迹重建、声称不变）。
    """
    plan = plan_fixed or tdmod.sample_plan(rng, guide_i + 1, DURATION_S)
    res = tdmod.build_frames(plan, tracks3[guide_i].positions,
                             [t.positions for t in tracks3], scene,
                             N_FRAMES, FPS)
    trk = Track(4, focal, aspect, tdmod.window_moves(plan), [], res.start)
    trk.positions, trk.quats = res.positions, res.quats
    trk.eulers, trk.speeds = res.eulers, res.speeds
    return plan, trk


def generate(scene: Scene, seed: int, quota: Optional[QuotaTracker] = None,
             covis_hz: float = 2.0, max_attempts: int = 6,
             cam4_mode: str = "timewarp") -> ClipPlan:
    """生成一条 4 路 MC 计划并做全部生成侧自检；失败时按层级重排。

    `cam4_mode`（docs/69 §8.3）：`timewarp`=同路线时间错位变速（缺省，行为与
    增补前逐字一致，plan.json 不写 mode 键）；`topdown`=同区域高空垂直俯视伴飞
    （形态 b，plan 写 `cam4.mode="topdown"`，QC 侧按 topdown 三条验形判）。
    """
    if cam4_mode not in ("timewarp", "topdown"):
        raise ValueError("cam4_mode 只认 timewarp/topdown（docs/69 §8.3），"
                         "拿到 %r" % (cam4_mode,))
    quota = quota or QuotaTracker()
    rng = random.Random(seed)
    aspect = quota.pick_aspect(rng)
    focals = list(FOCALS)
    rng.shuffle(focals)

    retries = {"speed_reshape": 0, "resample": 0, "reanchor": 0}
    reasons: List[str] = []
    plan: Optional[ClipPlan] = None

    for attempt in range(max_attempts):
        center, r_base = _open_center(scene, rng)
        # **轨道半径也要随画幅缩**（此前只随焦距）。内参纵向等效 ⇒
        # `tan(hfov/2)=(W/H)·12/f`：同一焦距下竖屏的水平视场只有横屏的三分之一，
        # 半径却一样远，画面里横向能看到的世界就少三分之二。
        # 这一条长期挂在「竖屏内容密度偏低」的待办里，直到 CARLA 批用逐帧实测
        # 把它量出来：不达标的路全部集中在**窄水平视场组合**上——
        # 9:16@35mm 水平视场 21.8°、4:3@50mm 20.4°，在 10m 外只截到 3.9m 宽的一条，
        # 而同批 16:9 的路全部 0.95 以上。
        #
        # 取 `aspect_az_scale` 的**平方根**而不是它本身：完全按角度补偿要把竖屏
        # 拉到 0.32×（相机几乎贴在锚点上，共视与「贴脸遮挡」两头都危险），
        # 平方根是「补一半」的折中，仍由计划自检的共视闸门兜底。
        r_base = clamp(r_base * math.sqrt(aspect_az_scale(aspect)), *R_BASE_RANGE)
        subjects = [make_subject(scene, rng, DURATION_S, center)
                    for _ in range(rng.randint(2, 4))]
        anchor = make_anchor(scene, rng, DURATION_S, subjects)
        placements = allocate_placements(rng, focals[:3])
        starts = _line_start(scene, anchor, focals, rng, r_base)
        if starts is None:
            reasons = ["场景内找不到可行的四路共线起点"]
            retries["reanchor"] += 1
            continue

        # --- 前三路。guide 先定：它要按 docs/71 §6 传导预算压低速度（v_g ≤ 6/ρ_max），
        #     否则 v₄=ρ·v_g 必然打穿表 2；同时它的静止占比要低，
        #     QC 反算 ρ 需要 v_g>0.05 的样本 ≥50%。---
        guide_i = rng.randrange(3)
        tracks: List[Track] = []
        ctxs: List[Ctx] = []
        # **三路全部**锁进共享扇区。此前留了一路 ±180° 自由路，是因为旧闸门只要
        # 「至少一对成立」，锁两路就够；但需求的「共同存在（不包括第四相机）」是
        # 三路都要看到同一块区域（严读），自由路会让含它的两对长期掉到 0.30 以下——
        # 实测严读占比只有 2.5%~50%，10 条样例 0 条达标。扇区放宽到 SECTOR_HALF_DEG，
        # 轨迹随机性由段内运镜与半径/速度分层保证，不再靠「放飞一路」制造差异。
        # 方位预算按画幅的水平视场缩放（见 aspect_az_scale）：竖屏容不下横屏的扇区。
        az_k = aspect_az_scale(aspect)
        sector_half = SECTOR_HALF_DEG * az_k
        az_half = COVIS_AZ_HALF_DEG * az_k
        for i in range(3):
            band = GUIDE_SPEED_BAND if i == guide_i else SPEED_BAND
            # guide 只用**速度可控**的类目（EXTENT_DRIVEN）：ρ=v₄/v_g 的分母是 v_g，
            # 跟随/追踪类跟着主体走，主体一慢 v_g 就掉到 0.05 以下，
            # ρ 在那里既不可测又噪声大，QC 会把噪声判成额外的变速事件。
            moves = build_moves(rng, quota, DURATION_S,
                                allowed=EXTENT_DRIVEN if i == guide_i else None)
            marks = assign_speeds(rng, moves, band)
            tr = Track(i + 1, focals[i], aspect, moves, marks, starts[i])
            a0 = anchor.at(0.0)
            r_lo, r_hi = radius_band(focals[i], r_base)
            ctx = Ctx(scene, anchor, subjects, placements[i], rng, DURATION_S,
                      r_min=r_lo, r_max=r_hi,
                      sector_center_deg=math.degrees(
                          math.atan2(starts[i].p[2] - a0[2], starts[i].p[0] - a0[0])),
                      sector_half_deg=sector_half)
            tracks.append(sample_track(tr, ctx, DURATION_S, N_FRAMES, FPS))
            ctxs.append(ctx)

        # 共视窗口：把前三路在窗口内收敛到同锚点/同足迹/同扇区（见 _force_covis_window）。
        # 窗口位置随机，避免每条都在同一段落共视。
        w_len = COVIS_WINDOW_FRAC * DURATION_S
        w0 = rng.uniform(0.0, DURATION_S - w_len)
        _force_covis_window(scene, tracks, anchor, r_base, w0, w0 + w_len,
                            guide_i=guide_i, az_half_deg=az_half)

        # 粉色诚实性修正场（QC plan.pink_honesty 的生成侧保证）：必须排在**全部**
        # 位置改写之后（样条/壳投影/共视收敛都会稀释或掰弯声称的平移）、
        # 第四路构建之前（guide 的 v_g 要以修正后为准，v₄=ρ·v_g 才严格成立）。
        _enforce_pink(tracks, scene)

        # --- 第四路 ---
        if cam4_mode == "topdown":
            c4plan, cam4_track = _build_cam4_topdown(
                scene, rng, tracks, guide_i, focals[3], aspect)
        else:
            built = _build_cam4(scene, rng, quota, anchor, subjects, focals,
                                aspect, starts[3], tracks[guide_i], guide_i + 1,
                                r_base=r_base)
            if built is None:
                reasons = ["第四路构造失败（标称长度不足或粉色排窗不可行）"]
                retries["resample"] += 1
                continue
            c4plan, cam4_track = built
        tracks.append(cam4_track)

        plan = ClipPlan(seed, scene, tracks, c4plan, aspect, anchor, subjects,
                        retries=retries, covis_window=(w0, w0 + w_len), r_base=r_base,
                        covis_az_half=az_half)

        # --- 生成侧自检 ---
        bad = _selfcheck(plan, covis_hz)
        if not bad:
            plan.ok = True
            plan.reasons = []
            break

        reasons = bad
        # 层级化重排（docs/71 §5，代价递增）：物理超限先原地降速重整形（两档），
        # 仍不过再整条重采/换锚点。降速对 a/j 是超线性的，两档通常够。
        for k in (0.8, 0.8, 0.8):
            if not any(r.startswith("phys") for r in reasons):
                break
            retries["speed_reshape"] += 1
            if not _reshape_speed(plan, ctxs, k, rng, quota):
                break
            reasons = _selfcheck(plan, covis_hz)
        if not reasons:
            plan.ok = True
            plan.reasons = []
            break
        retries["resample" if attempt < max_attempts - 2 else "reanchor"] += 1

    if plan is None:
        plan = ClipPlan(seed, scene, [], cam4mod.Cam4Plan(1, 1.0, 1.0, 30.0, 2.0),
                        aspect, Anchor(Subject([(0.0, 0.0, 0.0)], 1.0), 1.0), [],
                        retries=retries)
    plan.reasons = reasons if not plan.ok else []
    plan.retries = retries
    if plan.ok:
        for t in plan.tracks:
            quota.note_track([m.cls for m in t.moves], t.focal_mm)
        quota.note_clip(aspect)
    return plan


def _remap_cam4_moves(moves: Sequence[Move], tau: Sequence[float], fps: float) -> List[Move]:
    """标称时间轴上的 move → 墙钟时间轴（第四路只走到 tau[-1]）。"""
    out: List[Move] = []
    for m in moves:
        i0 = next((i for i, x in enumerate(tau) if x >= m.t0), None)
        if i0 is None:
            continue
        i1 = next((i for i, x in enumerate(tau) if x >= m.t1), len(tau) - 1)
        if i1 <= i0:
            continue
        out.append(Move(m.cls, i0 / fps, i1 / fps, m.seg,
                        dict(m.params, nominal_t0=round(m.t0, 3),
                             nominal_t1=round(m.t1, 3))))
    return out


def _reshape_speed(plan: ClipPlan, ctxs: Sequence[Ctx], k: float,
                   rng: Optional[random.Random] = None,
                   quota: Optional[QuotaTracker] = None) -> bool:
    """违约段时间伸缩重整形的简化实现：整体降速后重采前三路，**并重建第四路**。

    第四路是 guide 的时间重参数化，guide 一降速它的 `v₄=ρ·v_g` 就全变了；
    不同步重建的话，plan.json 申报的 ρ 与轨迹实际反算值会对不上，QC 侧 `cam4.*` 必挂。
    """
    if len(plan.tracks) < 4:
        return False
    for tr, ctx in zip(plan.tracks[:3], ctxs):
        tr.speed_marks = [(t, v * k) for t, v in tr.speed_marks]
        for m in tr.moves:
            if m.cls not in STATIONARY:
                m.params["v_target"] = round(m.params.get("v_target", 1.0) * k, 3)
                m.params["extent"] = round(m.params["v_target"] * m.duration, 3)
        sample_track(tr, ctx, DURATION_S, N_FRAMES, FPS)

    guide_i = plan.cam4_plan.guide_cam - 1
    # 重采是从 move 模板重算逐帧位姿，`generate` 里施加的共视窗口收敛随之被抹掉；
    # 必须按**原窗口**重建，否则紧接着的 `_selfcheck` 判的是一条没有窗口的轨迹。
    w0, w1 = plan.covis_window
    if w1 > w0:
        _force_covis_window(plan.scene, plan.tracks[:3], plan.anchor, plan.r_base,
                            w0, w1, guide_i=guide_i, az_half_deg=plan.covis_az_half)
    # 重采把诚实性修正场一并抹掉了，与共视窗口同理必须重施加（且在第四路重建前）
    _enforce_pink(plan.tracks[:3], plan.scene)
    if plan.cam4_plan.mode == "topdown":
        # 俯视位没有 ρ 契约要保，但路线脊线与高度基线都来自前三路的实测轨迹，
        # 降速后必须重建；推进速率同步缩 k，窗口/脉冲/朝向声称不变。
        plan.cam4_plan = tdmod.scale_speeds(plan.cam4_plan, k)
        _, plan.tracks[3] = _build_cam4_topdown(
            plan.scene, rng or random.Random(plan.seed), plan.tracks[:3],
            guide_i, plan.tracks[3].focal_mm, plan.aspect,
            plan_fixed=plan.cam4_plan)
        return True
    built = _build_cam4(plan.scene, rng or random.Random(plan.seed),
                        quota or QuotaTracker(), plan.anchor, plan.subjects,
                        [t.focal_mm for t in plan.tracks], plan.aspect,
                        plan.tracks[3].start, plan.tracks[guide_i],
                        plan.cam4_plan.guide_cam, plan_fixed=plan.cam4_plan)
    if built is None:
        return False
    plan.tracks[3] = built[1]
    return True


def _selfcheck(plan: ClipPlan, covis_hz: float) -> List[str]:
    """物理 p95 / 解耦 / 初始帧 / 共视 —— 与 engine-data-qc 同口径的生成侧复核。"""
    dt = 1.0 / FPS
    bad: List[str] = []
    plan.physics = {}
    for tr in plan.tracks:
        ph = met.physics(tr.positions, tr.quats, dt)
        plan.physics[tr.cam_id] = ph
        v = ph.violations(is_cam4=(tr.cam_id == 4))
        if v:
            bad.append("phys.cam%d(%s)" % (tr.cam_id, ",".join(v)))

    pose_map = {tr.cam_id: (tr.positions, tr.quats) for tr in plan.tracks}
    plan.decouple = met.decouple(pose_map, dt)
    if not plan.decouple.ok():
        k, f = plan.decouple.worst_pair
        bad.append("decouple(pair %s=%.0f%%, all4=%.0f%%)"
                   % (k, f * 100, plan.decouple.all4_frac * 100))

    if len(plan.tracks) == 4:
        gi = plan.cam4_plan.guide_cam - 1
        if plan.cam4_plan.mode == "topdown":
            # 形态 b：变速三条不适用（QC 侧 SKIP），验形走 topdown 三条——
            # 算法与阈值逐字复刻 `engine_qc._check_cam4_topdown`（防 #427 复发）。
            plan.cam4_topdown = met.cam4_topdown_check(
                plan.tracks[3].positions, plan.tracks[3].quats,
                [t.positions for t in plan.tracks[:3]],
                plan.tracks[gi].positions, dt)
            probs = plan.cam4_topdown.problems()
        else:
            plan.cam4_check = met.cam4_check(plan.tracks[3].positions,
                                             plan.tracks[gi].positions, dt)
            probs = plan.cam4_check.problems(plan.cam4_plan.t_sw,
                                             plan.cam4_plan.delta)
        if probs:
            bad.append("cam4(%s)" % ",".join(probs))

    # 粉色平移诚实性（QC plan.pink_honesty 同式，四路全判）。这条自检此前缺席
    # ——QC #597 上线后「自检全绿 QC 打回」在每条经典 clip 上稳定复现（#427 形态）。
    for tr in plan.tracks:
        rows = met.pink_honesty_check(tr.positions, tr.quats, tr.moves, FPS)
        liar = ["%s@%.1fs(d=%.2f/p=%.2f)" % (r.cls[10:], r.t0, r.disp_m, r.path_m)
                for r in rows if not r.ok]
        if liar:
            bad.append("pink.cam%d(%s)" % (tr.cam_id, ",".join(liar)))

    # 初始帧：俯视位声明下 lane 4 自动豁免（与 QC clip_checker 的
    # init_exempt_lanes 联动同语义——首帧垂直朝向/高差是形态定义本身；
    # 「t=0 锚定同区域」由 topdown_route 硬帽覆盖）。前三路之间照常判。
    init_map = pose_map
    if plan.cam4_plan.mode == "topdown":
        init_map = {k: v for k, v in pose_map.items() if k != 4}
    plan.init_angle, plan.init_dist = met.initial_frame(init_map)
    if plan.init_angle > 15.0:
        bad.append("init.angle=%.1f°" % plan.init_angle)
    if plan.init_dist > 30.0:
        bad.append("init.dist=%.1fm" % plan.init_dist)

    # 共视是自检里最贵的一项（probe 双向遮挡射线），前面便宜的闸门已经挂了就不必再算
    if bad:
        plan.loop_closure_m = max((_loop_closure(t) for t in plan.tracks), default=0.0)
        return bad

    times = covismod.sample_times(DURATION_S, covis_hz)
    views: Dict[int, List[covismod.CamView]] = {}
    for tr in plan.tracks:
        if tr.cam_id not in covismod.COVIS_CAMS:
            continue
        it = plan.intrinsics(tr.focal_mm)
        w, h = plan.resolution()
        views[tr.cam_id] = [
            covismod.CamView(tr.positions[min(int(t * FPS), N_FRAMES - 1)],
                             tr.quats[min(int(t * FPS), N_FRAMES - 1)],
                             it["fx"], it["fy"], it["cx"], it["cy"], w, h)
            for t in times]
    plan.covis = covismod.evaluate(plan.scene, views, times)
    if plan.covis.ratio < covismod.COVIS_RATIO_MIN:
        bad.append("covis=%.0f%%" % (plan.covis.ratio * 100))

    plan.loop_closure_m = max((_loop_closure(t) for t in plan.tracks), default=0.0)
    plan.min_clearance_m = _min_clearance(plan)
    if plan.min_clearance_m <= 0.0:
        bad.append("clearance=%.2fm（穿模红线）" % plan.min_clearance_m)
    # 自由壳投影把节点搬了多远：只作**产线健康指标**上报，不作闸门。
    # 真正的判据是下游那几条（穿模间隙 / 物理 p95 / 共视 / 解耦 / 第四路），
    # 拿「修了多少」当闸门会误杀掉一批修完其实完全合规的计划（实测室内类 5/8 → 0/8）。
    plan.max_push_m = max((t.max_push_m for t in plan.tracks), default=0.0)
    return bad


def _push_report(tracks: Sequence[Track]) -> float:
    return max((t.max_push_m for t in tracks), default=0.0)

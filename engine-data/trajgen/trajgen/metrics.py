"""生成侧自检：物理阈值 p95 与解耦滑窗（口径与 engine-data-qc 逐字对齐）。

docs/70 §4.2：「约束在生成侧保证、在 QC 侧复核」。本模块是**生成侧**那一半，
差分方式、窗口定义、判据阈值全部照 `engine_qc.checks_motion` / `engine_qc.config` 抄，
两边算出的数必须一致——否则「自检全绿 QC 打回」就成了常态。
"""

import math
from dataclasses import dataclass, field
from typing import Dict, List, Optional, Sequence, Tuple

from .mathutil import (Quat, Vec3, angle_between_deg, median, percentile,
                       quat_angle_deg, quat_forward, unwrap_deg, vdot, vnorm,
                       vsub)

# --- 物理阈值（docs/69 表 2，p95 口径）---
SPEED_P95_MAX = 6.0
SPEED_CAM4_CAP = 10.0
SPEED_INST_CAP = 10.0   # 全路帧间瞬时硬帽（内控，engine-data/README.md「全路帧间瞬时速度硬帽」节）
ACCEL_P95_MAX = 8.0
JERK_P95_MAX = 20.0
ANGSPEED_P95_MAX = 90.0
ANGACCEL_P95_MAX = 150.0
ANGJERK_P95_MAX = 600.0

# --- 段边界连续性（单帧最大值口径；docs/69 §3.1「段间连续衔接并缓入缓出」）---
# p95 对 1–2 帧的孤立尖峰结构性失明（1800 帧序列 5% 分位裕量 = 90 帧）：
# clip-013（seed=13）实证 cam2 两处 150 m/s 单帧传送门在 p95 口径下自检全绿
# （orbit 机头随瞬时 omega 符号 180° 调头 → 2.5m 前挂点单帧平移 5m，docs/78 §10.7）。
# 平移按最大值补一道「传送门」硬闸——位移单帧尖峰**没有合法来源**（构造速度
# ≤V_MAX 5.5 + 起伏分量，10 留足裕量，任何 ≥0.33m 的单帧跳变必中）。
# 常量名/闸门文本与 PR #474（QC 全路瞬时硬帽 + trajgen 同步）逐字同构，合并即收敛。
SPEED_INST_CAP = 10.0   # 全路帧间瞬时硬帽（内控，engine-data/README.md「全路帧间瞬时速度硬帽」节）
# 角速度单帧口径**只记录不设闸**：经典模式 look-at 掠过天顶/天底（|fwd_y|→1）时
# yaw 快摆表现为绕光轴的面内旋转，单帧读数合法地冲到数百 °/s（实测 seed=7 cam4
# 590°/s，forward 全程连续）——任何角单帧硬闸都会在近极区随机误杀；需求表 2 的
# 角口径本就是 p95。超 ANGSPEED_SPIKE_REPORT 的帧记入报告供人工复核（docs/78 §10.7 反方）。
# 差分口径与 physics() 同一套（README「运动学反算规范口径」：dt 名义 1/30，
# 平移向量差分取模、角走 q_rel 的 atan2 形式，序列自然减 1、不 padding 不平滑）。
ANGSPEED_SPIKE_REPORT = 270.0

# --- 解耦滑窗（docs/71 §5）---
WINDOW_S = 1.0
STEP_S = 0.5
STATIC_V = 0.2
STATIC_W_DEG = 3.0
DIR_COS_MIN = 0.85
RATIO_LO = 0.7
RATIO_HI = 1.4
YAW_MIN_DEG = 5.0
PAIR_SYNC_MAX = 0.5
ALL4_SYNC_MAX = 0.2

#: 生成侧内控余量：规划期按 QC 阈值的 90% 收，给 QC 侧留 10% 缓冲
PLANNING_MARGIN = 0.90


@dataclass
class Physics:
    speed_p95: float
    speed_max: float
    speed_low_frac: float
    accel_p95: float
    jerk_p95: float
    angspeed_p95: float
    angaccel_p95: float
    angjerk_p95: float
    speed_hist: List[float] = field(default_factory=list)
    angspeed_max: float = 0.0
    #: 段边界连续性：超 SPEED_INST_CAP / ANGSPEED_SPIKE_REPORT 的差分帧号 k
    #: （尖峰落在 k→k+1 之间）；速度尖峰由下方 speed_max 闸拦截，帧号入 provenance
    speed_spike_frames: List[int] = field(default_factory=list)
    angspeed_spike_frames: List[int] = field(default_factory=list)

    def violations(self, is_cam4: bool = False, margin: float = 1.0) -> List[str]:
        out = []
        if self.speed_p95 > SPEED_P95_MAX * margin:
            out.append("speed_p95=%.2f" % self.speed_p95)
        if is_cam4:
            if self.speed_max > SPEED_CAM4_CAP * margin:
                out.append("speed_max4=%.2f" % self.speed_max)
        elif self.speed_max > SPEED_INST_CAP * margin:
            out.append("speed_max=%.2f" % self.speed_max)
        if self.accel_p95 > ACCEL_P95_MAX * margin:
            out.append("accel_p95=%.2f" % self.accel_p95)
        if self.jerk_p95 > JERK_P95_MAX * margin:
            out.append("jerk_p95=%.2f" % self.jerk_p95)
        if self.angspeed_p95 > ANGSPEED_P95_MAX * margin:
            out.append("angspeed_p95=%.1f" % self.angspeed_p95)
        if self.angaccel_p95 > ANGACCEL_P95_MAX * margin:
            out.append("angaccel_p95=%.1f" % self.angaccel_p95)
        if self.angjerk_p95 > ANGJERK_P95_MAX * margin:
            out.append("angjerk_p95=%.1f" % self.angjerk_p95)
        return out

    def to_dict(self) -> Dict:
        d = {k: round(v, 4) for k, v in {
            "speed_p95": self.speed_p95, "speed_max": self.speed_max,
            "speed_low_frac": self.speed_low_frac, "accel_p95": self.accel_p95,
            "jerk_p95": self.jerk_p95, "angspeed_p95": self.angspeed_p95,
            "angaccel_p95": self.angaccel_p95, "angjerk_p95": self.angjerk_p95,
            "angspeed_max": self.angspeed_max,
        }.items()}
        d["speed_spikes"] = self.speed_spike_frames[:8]
        d["angspeed_spikes"] = self.angspeed_spike_frames[:8]
        return d


def _vdiff(vs: Sequence[Sequence[float]], dt: float) -> List[Tuple[float, ...]]:
    return [tuple((b[i] - a[i]) / dt for i in range(len(a))) for a, b in zip(vs, vs[1:])]


def _sdiff(xs: Sequence[float], dt: float) -> List[float]:
    return [(b - a) / dt for a, b in zip(xs, xs[1:])]


def physics(positions: Sequence[Vec3], quats: Sequence[Quat], dt: float) -> Physics:
    v_vec = _vdiff(positions, dt)
    v = [vnorm(x) for x in v_vec]
    a_vec = _vdiff(v_vec, dt)
    a = [vnorm(x) for x in a_vec]
    j = [vnorm(x) for x in _vdiff(a_vec, dt)]
    w = [quat_angle_deg(x, y) / dt for x, y in zip(quats, quats[1:])]
    al = [abs(x) for x in _sdiff(w, dt)]
    aj = [abs(x) for x in _sdiff(al, dt)]
    return Physics(
        speed_p95=percentile(v, 95), speed_max=max(v) if v else 0.0,
        speed_low_frac=(sum(1 for x in v if x < 0.2) / len(v)) if v else 0.0,
        accel_p95=percentile(a, 95), jerk_p95=percentile(j, 95),
        angspeed_p95=percentile(w, 95), angaccel_p95=percentile(al, 95),
        angjerk_p95=percentile(aj, 95), speed_hist=v,
        angspeed_max=max(w) if w else 0.0,
        speed_spike_frames=[i for i, x in enumerate(v) if x > SPEED_INST_CAP],
        angspeed_spike_frames=[i for i, x in enumerate(w) if x > ANGSPEED_SPIKE_REPORT],
    )


# ---------------------------------------------------------------- 解耦

@dataclass
class _Win:
    v: Vec3
    speed: float
    ang: float
    yaw_rate: float
    pitch_rate: float


def _windows(positions: Sequence[Vec3], quats: Sequence[Quat], dt: float) -> List[_Win]:
    n = len(positions)
    win = max(2, round(WINDOW_S / dt))
    step = max(1, round(STEP_S / dt))
    theta = [quat_angle_deg(a, b) for a, b in zip(quats, quats[1:])]
    yp = []
    for q in quats:
        f = quat_forward(q)
        yp.append((math.degrees(math.atan2(f[0], f[2])),
                   math.degrees(math.asin(max(-1.0, min(1.0, f[1]))))))
    yaw = unwrap_deg([a for a, _ in yp])
    pitch = [b for _, b in yp]
    out: List[_Win] = []
    s = 0
    while s + win <= n - 1:
        e = s + win
        dur = win * dt
        v = tuple((positions[e][i] - positions[s][i]) / dur for i in range(3))
        out.append(_Win(v, vnorm(v), sum(theta[s:e]) / dur,
                        (yaw[e] - yaw[s]) / dur, (pitch[e] - pitch[s]) / dur))
        s += step
    return out


def _sync(a: _Win, b: _Win) -> bool:
    if (a.speed < STATIC_V and b.speed < STATIC_V
            and a.ang < STATIC_W_DEG and b.ang < STATIC_W_DEG):
        return True
    if a.speed >= STATIC_V and b.speed >= STATIC_V:
        cos = vdot(a.v, b.v) / (a.speed * b.speed)
        ratio = a.speed / b.speed
        if cos > DIR_COS_MIN and RATIO_LO <= ratio <= RATIO_HI:
            return True
    for ra, rb in ((a.yaw_rate, b.yaw_rate), (a.pitch_rate, b.pitch_rate)):
        if abs(ra) > YAW_MIN_DEG and abs(rb) > YAW_MIN_DEG and ra * rb > 0:
            if RATIO_LO <= abs(ra) / abs(rb) <= RATIO_HI:
                return True
    return False


@dataclass
class Decouple:
    pair_frac: Dict[str, float]
    all4_frac: float
    n_windows: int

    @property
    def worst_pair(self) -> Tuple[str, float]:
        if not self.pair_frac:
            return ("-", 0.0)
        return max(self.pair_frac.items(), key=lambda kv: kv[1])

    def ok(self, margin: float = 1.0) -> bool:
        return (self.worst_pair[1] < PAIR_SYNC_MAX * margin
                and self.all4_frac < ALL4_SYNC_MAX * margin)

    def to_dict(self) -> Dict:
        return {"pair_sync": {k: round(v, 4) for k, v in sorted(self.pair_frac.items())},
                "all4_sync": round(self.all4_frac, 4), "windows": self.n_windows}


def decouple(tracks: Dict[int, Tuple[Sequence[Vec3], Sequence[Quat]]], dt: float) -> Decouple:
    metrics = {k: _windows(p, q, dt) for k, (p, q) in tracks.items()}
    nwin = min((len(m) for m in metrics.values()), default=0)
    if nwin == 0:
        return Decouple({}, 0.0, 0)
    keys = sorted(metrics)
    flags: Dict[Tuple[int, int], List[bool]] = {}
    frac: Dict[str, float] = {}
    for i, ka in enumerate(keys):
        for kb in keys[i + 1:]:
            f = [_sync(metrics[ka][w], metrics[kb][w]) for w in range(nwin)]
            flags[(ka, kb)] = f
            frac["%d-%d" % (ka, kb)] = sum(f) / nwin
    all4 = 0.0
    if len(keys) == 4:
        all4 = sum(1 for w in range(nwin) if all(f[w] for f in flags.values())) / nwin
    return Decouple(frac, all4, nwin)


# ---------------------------------------------------------------- 第四路 ρ 反算

#: 与 engine_qc.config 逐字对齐的第四路检测参数
RHO_CHANGE_LOG = 0.223      # 变速事件检测阈（≈log 1.25）
TSW_MEASURE_TOL_S = 2.0     # 实测切换中心 vs plan(t_sw+Δ/2) 容差
BAND_LO, BAND_HI = 0.65, 1.5
BAND_DWELL_MAX_S = 1.0
VG_MIN = 0.05               # guide 速率低于此值时 ρ 不可测


@dataclass
class Cam4Check:
    events_s: List[float]
    dwell_s: float
    crossing_s: float
    valid_frac: float

    def problems(self, t_sw: float, delta: float) -> List[str]:
        out: List[str] = []
        if self.valid_frac < 0.5:
            out.append("rho_valid=%.0f%%" % (self.valid_frac * 100))
        if len(self.events_s) != 1:
            out.append("switch_count=%d" % len(self.events_s))
        elif abs(self.events_s[0] - (t_sw + delta / 2.0)) > TSW_MEASURE_TOL_S:
            out.append("t_sw_measured=%.1f" % self.events_s[0])
        if self.dwell_s > BAND_DWELL_MAX_S:
            out.append("band_dwell=%.2f" % self.dwell_s)
        return out


def _median_opt(xs: Sequence[Optional[float]]) -> Optional[float]:
    vals = [x for x in xs if x is not None]
    return median(vals) if vals else None


def cam4_check(pos4: Sequence[Vec3], pos_guide: Sequence[Vec3], dt: float) -> Cam4Check:
    """复刻 `engine_qc.checks_motion.check_cam4` 的 ρ 反算与判据。

    生成侧必须用**完全相同**的算法自检：ρ 的可检出性取决于 QC 那套
    「±0.5s 中值的对数比 > 0.223」的检测器，而不是我们声明了多大的 Δρ ——
    Δ 取 3s 而 Δρ 偏小时，声明的变速在检测器眼里根本不存在（实测漏检）。
    """
    n = min(len(pos4), len(pos_guide))
    v4 = [vnorm(vsub(pos4[i + 1], pos4[i])) / dt for i in range(n - 1)]
    vg = [vnorm(vsub(pos_guide[i + 1], pos_guide[i])) / dt for i in range(n - 1)]
    raw: List[Optional[float]] = [
        (v4[i] / vg[i]) if vg[i] > VG_MIN else None for i in range(n - 1)]
    half = max(2, round(0.25 / dt))
    rho: List[Optional[float]] = []
    for i in range(len(raw)):
        window = raw[max(0, i - half):min(len(raw), i + half + 1)]
        vals = [x for x in window if x is not None]
        rho.append(median(vals) if len(vals) >= half else None)
    valid = sum(1 for x in rho if x is not None) / max(1, len(rho))

    k = max(3, round(0.5 / dt))
    scores: List[Tuple[int, float]] = []
    for i in range(k, len(rho) - k):
        pre = _median_opt(rho[i - k:i])
        post = _median_opt(rho[i:i + k])
        if pre and post and pre > 0 and post > 0:
            sc = abs(math.log(post / pre))
            if sc > RHO_CHANGE_LOG:
                scores.append((i, sc))
    events: List[int] = []
    if scores:
        group = [scores[0]]
        for idx, sc in scores[1:]:
            if idx - group[-1][0] <= k:
                group.append((idx, sc))
            else:
                events.append(max(group, key=lambda t: t[1])[0])
                group = [(idx, sc)]
        events.append(max(group, key=lambda t: t[1])[0])

    in_band = [(x is not None and BAND_LO < x < BAND_HI) for x in rho]
    runs: List[Tuple[int, int]] = []
    i = 0
    while i < len(in_band):
        if in_band[i]:
            j = i
            while j < len(in_band) and in_band[j]:
                j += 1
            runs.append((i, j))
            i = j
        else:
            i += 1
    total = sum(e - s for s, e in runs) * dt
    crossing = 0.0
    for s, e in runs:
        before = next((rho[x] for x in range(s - 1, -1, -1) if rho[x] is not None), None)
        after = next((rho[x] for x in range(e, len(rho)) if rho[x] is not None), None)
        if before is None or after is None:
            continue
        if (before <= BAND_LO and after >= BAND_HI) or (before >= BAND_HI and after <= BAND_LO):
            crossing = max(crossing, (e - s) * dt)
    return Cam4Check([e * dt for e in events], total - crossing, crossing, valid)


# ---------------------------------------------------------------- 粉色平移诚实性

#: 与 `engine_qc.config` 逐字对齐（plan.pink_honesty，#597——判据即与生成侧的契约）
PINK_MIN_DISP_M = 0.8
PINK_AXIS_FRAC_MIN = 0.5


def pink_axis(cls: str, quat: Quat) -> Tuple[Vec3, float]:
    """声称轴向：窗**首帧**相机 yaw 的水平基底（up/down 用世界 Y）。

    与 QC `checks_json._yaw_basis` 逐字同式：yaw 从四元数 forward 向量取
    atan2(fx, fz)，fwd=(sin yaw, 0, cos yaw)、right=(cos yaw, 0, −sin yaw)。
    返回 (axis, yaw_rad)。
    """
    fx, _, fz = quat_forward(quat)
    yaw = math.atan2(fx, fz)
    fwd = (math.sin(yaw), 0.0, math.cos(yaw))
    right = (math.cos(yaw), 0.0, -math.sin(yaw))
    axis = {"translate_forward": fwd,
            "translate_back": (-fwd[0], 0.0, -fwd[2]),
            "translate_right": right,
            "translate_left": (-right[0], 0.0, -right[2]),
            "translate_up": (0.0, 1.0, 0.0),
            "translate_down": (0.0, -1.0, 0.0)}[cls]
    return axis, yaw


@dataclass
class PinkWindowResult:
    cls: str
    t0: float
    disp_m: float
    path_m: float
    ok: bool


def pink_honesty_check(positions: Sequence[Vec3], quats: Sequence[Quat],
                       moves, fps: float) -> List[PinkWindowResult]:
    """复刻 `engine_qc.checks_json.check_pink_honesty` 的实测算法。

    逐个 translate_* 声称窗：帧段按 `round(t·fps)`、基底取窗首帧 yaw、
    判据「净轴向位移 ≥0.8m 且 ≥ 窗内路径长 50%」。生成侧此前**完全没有**这条
    自检——正是 #427「自检全绿 QC 打回」的形态（QC 侧 #597 上线后一直漏着）。
    """
    out: List[PinkWindowResult] = []
    n = len(positions)
    for m in moves:
        if not m.cls.startswith("translate_"):
            continue
        i0 = max(0, int(round(m.t0 * fps)))
        i1 = min(int(round(m.t1 * fps)), n - 1)
        if i0 >= n or i1 - i0 < 1:
            out.append(PinkWindowResult(m.cls, m.t0, 0.0, 0.0, False))
            continue
        axis, _ = pink_axis(m.cls, quats[i0])
        d = vsub(positions[i1], positions[i0])
        disp = vdot(d, axis)
        path = sum(vnorm(vsub(b, a))
                   for a, b in zip(positions[i0:i1 + 1], positions[i0 + 1:i1 + 1]))
        ok = disp >= PINK_MIN_DISP_M and disp >= PINK_AXIS_FRAC_MIN * path
        out.append(PinkWindowResult(m.cls, m.t0, disp, path, ok))
    return out


# ---------------------------------------------------------------- 第四路俯视伴飞（形态 b）

#: 与 `engine_qc.config`「第四路俯视伴飞」节逐字对齐的阈值（docs/69 §8.3 形态 b）。
#: #427 教训：生成侧自检与 QC 的口径差一个字，就是「自检 100% 而 QC 判 FAIL」。
TOPDOWN_PITCH_TOL_DEG = 15.0    # deg，forward 与世界 -y 的偏角规格
TOPDOWN_PITCH_FRAC_MIN = 0.95   # 偏角达标帧占比下限
TOPDOWN_PITCH_MAX_DEG = 30.0    # deg，偏角瞬时硬帽
TOPDOWN_ALT_MIN_M = 10.0        # m，相对前三路当帧最高点的最小高差
TOPDOWN_ALT_FRAC_MIN = 0.95     # 高差达标帧占比下限
TOPDOWN_ROUTE_XZ_P95_MAX = 20.0  # m，到 guide 路线 XZ 折线的水平距离 p95 上限
TOPDOWN_ROUTE_XZ_MAX_M = 40.0    # m，水平距离瞬时硬帽
TOPDOWN_ROUTE_SAMPLE_S = 0.2     # s，折线抽样间隔


@dataclass
class Cam4TopdownCheck:
    pitch_over_frac: float   # 偏角 > 规格的帧占比
    pitch_max_deg: float
    alt_under_frac: float    # 高差 < 下限的帧占比
    alt_min_m: float
    route_p95_m: float
    route_max_m: float

    def problems(self) -> List[str]:
        out: List[str] = []
        if self.pitch_over_frac > 1.0 - TOPDOWN_PITCH_FRAC_MIN:
            out.append("topdown_pitch_frac=%.1f%%" % (self.pitch_over_frac * 100))
        if self.pitch_max_deg > TOPDOWN_PITCH_MAX_DEG:
            out.append("topdown_pitch_max=%.1f" % self.pitch_max_deg)
        if self.alt_under_frac > 1.0 - TOPDOWN_ALT_FRAC_MIN:
            out.append("topdown_alt_frac=%.1f%%" % (self.alt_under_frac * 100))
        if self.route_p95_m > TOPDOWN_ROUTE_XZ_P95_MAX:
            out.append("topdown_route_p95=%.1f" % self.route_p95_m)
        if self.route_max_m > TOPDOWN_ROUTE_XZ_MAX_M:
            out.append("topdown_route_max=%.1f" % self.route_max_m)
        return out

    def to_dict(self) -> Dict:
        return {"pitch_over_frac": round(self.pitch_over_frac, 4),
                "pitch_max_deg": round(self.pitch_max_deg, 3),
                "alt_under_frac": round(self.alt_under_frac, 4),
                "alt_min_m": round(self.alt_min_m, 3),
                "route_p95_m": round(self.route_p95_m, 3),
                "route_max_m": round(self.route_max_m, 3)}


def cam4_topdown_check(pos4: Sequence[Vec3], quats4: Sequence[Quat],
                       others_pos: Sequence[Sequence[Vec3]],
                       pos_guide: Sequence[Vec3], dt: float) -> Cam4TopdownCheck:
    """复刻 `engine_qc.checks_motion._check_cam4_topdown` 的三条判据。

    ① 垂直俯视：forward 与世界 -y 的偏角（占比闸 + 硬帽）；
    ② 高空：相对前三路**当帧最高点**的高差（占比闸）；
    ③ 路径相关：到 guide 路线 XZ **折线**的水平距离（p95 + 硬帽）。
    折线口径必须照抄：0.2s 抽样一点、补末点、逐帧取**到抽样点**的最小距离——
    QC 就是这么算的，任何「更精确」的点到线段距离都会让两侧数值分叉。
    """
    devs = [angle_between_deg(quat_forward(q), (0.0, -1.0, 0.0)) for q in quats4]
    pitch_over = sum(1 for a in devs if a > TOPDOWN_PITCH_TOL_DEG) / len(devs)

    n = min(len(pos4), *(len(pk) for pk in others_pos))
    margins = [pos4[i][1] - max(pk[i][1] for pk in others_pos) for i in range(n)]
    alt_under = sum(1 for m in margins if m < TOPDOWN_ALT_MIN_M) / n

    step = max(1, round(TOPDOWN_ROUTE_SAMPLE_S / dt))
    route = [(p[0], p[2]) for p in pos_guide[::step]]
    if (pos_guide[-1][0], pos_guide[-1][2]) != route[-1]:
        route.append((pos_guide[-1][0], pos_guide[-1][2]))
    dists = [min(math.hypot(p[0] - rx, p[2] - rz) for rx, rz in route) for p in pos4]

    return Cam4TopdownCheck(pitch_over, max(devs), alt_under, min(margins),
                            percentile(dists, 95), max(dists))


def rho_detectable(rho1: float, rho2: float, delta: float) -> bool:
    """声明的变速能否被 QC 的检测器看见。

    检测器比的是切换点前后各 0.5s 的中值之比。S₅ 过渡在中点处的对数斜率约
    `1.875·L/Δ`（L=|ln(ρ₂/ρ₁)|），跨 0.5s 的对数差约 `0.94·L/Δ`，
    要大于阈值 0.223 ⇒ **L > 0.24·Δ**。这里再留 1.6× 余量抗测量噪声。
    """
    return abs(math.log(rho2 / rho1)) > 0.38 * delta


# ---------------------------------------------------------------- 初始帧

def initial_frame(tracks: Dict[int, Tuple[Sequence[Vec3], Sequence[Quat]]]
                  ) -> Tuple[float, float]:
    """返回 (首帧朝向最大夹角°, 首帧位置最大距离 m)——QC 的 init.* 两条同口径。"""
    keys = sorted(tracks)
    max_ang = max_dist = 0.0
    for i, ka in enumerate(keys):
        for kb in keys[i + 1:]:
            fa, fb = quat_forward(tracks[ka][1][0]), quat_forward(tracks[kb][1][0])
            c = max(-1.0, min(1.0, vdot(fa, fb) / (vnorm(fa) * vnorm(fb))))
            max_ang = max(max_ang, math.degrees(math.acos(c)))
            max_dist = max(max_dist, vnorm(vsub(tracks[ka][0][0], tracks[kb][0][0])))
    return max_ang, max_dist

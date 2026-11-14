"""运动面规则：物理阈值 / 4 路同步 / 解耦闸门 / 初始帧 / 第四路变速。

全部由 camera_position / camera_rotation_quaternion 有限差分反算（docs/71 §5、§6），
不信任文件内的 camera_speed 字段。
"""

import math
from dataclasses import dataclass
from typing import Dict, List, Optional, Sequence, Tuple

from .config import QCConfig
from .mathutil import (angle_between_deg, median, percentile, quat_angle_deg,
                       quat_forward, roll_deg, unwrap_deg, vdot, vnorm, vsub,
                       yaw_pitch_deg)
from .report import FAIL, PASS, SKIP, RuleResult


def extract_positions(frames) -> Optional[List[Tuple[float, float, float]]]:
    out = []
    for f in frames:
        p = f.get("camera_position") if isinstance(f, dict) else None
        if not (isinstance(p, list) and len(p) == 3
                and all(isinstance(x, (int, float)) and not isinstance(x, bool) for x in p)):
            return None
        out.append(tuple(float(x) for x in p))
    return out


def extract_quats(frames) -> Optional[List[Tuple[float, float, float, float]]]:
    out = []
    for f in frames:
        q = f.get("camera_rotation_quaternion") if isinstance(f, dict) else None
        if not (isinstance(q, list) and len(q) == 4
                and all(isinstance(x, (int, float)) and not isinstance(x, bool) for x in q)):
            return None
        out.append(tuple(float(x) for x in q))
    return out


# ---------------------------------------------------------------- 物理阈值组

def _mag_series(vectors) -> List[float]:
    return [vnorm(v) for v in vectors]


def _vec_diff(vectors, dt: float):
    return [tuple((b[i] - a[i]) / dt for i in range(len(a)))
            for a, b in zip(vectors, vectors[1:])]


def _scalar_diff(values: Sequence[float], dt: float) -> List[float]:
    return [(b - a) / dt for a, b in zip(values, values[1:])]


def check_physics(name: str, frames, dt: float, cfg: QCConfig, is_cam4: bool) -> List[RuleResult]:
    out: List[RuleResult] = []
    pos = extract_positions(frames)
    quats = extract_quats(frames)

    def skip_all(reason: str):
        rules = ["phys.speed_p95", "phys.speed_low",
                 "phys.speed_cap4" if is_cam4 else "phys.speed_max",
                 "phys.accel_p95", "phys.jerk_p95",
                 "phys.angspeed_p95", "phys.angaccel_p95", "phys.angjerk_p95"]
        # cam4 的瞬时硬帽走既有的 phys.speed_cap4（合同值，docs/71 §9）；前三路走新增的
        # phys.speed_max。两者判据相同但来源不同，不可对同一路同时发——否则计数虚高，
        # 且日后分别调阈值会产生自相矛盾的输出。
        rules.insert(1, "phys.speed_cap4" if is_cam4 else "phys.speed_max")
        return [RuleResult(r, SKIP, name, reason) for r in rules]

    if pos is None or quats is None:
        return skip_all("位姿字段异常（见 schema FAIL）")
    if len(pos) < 4:
        return skip_all(f"帧数 {len(pos)} 不足以做有限差分（需 ≥4）")

    # 平移链：v → a → j（向量差分取模）
    v_vec = _vec_diff(pos, dt)
    v_mag = _mag_series(v_vec)
    a_vec = _vec_diff(v_vec, dt)
    a_mag = _mag_series(a_vec)
    j_vec = _vec_diff(a_vec, dt)
    j_mag = _mag_series(j_vec)

    p95v = percentile(v_mag, 95)
    st = PASS if p95v <= cfg.speed_p95_max else FAIL
    out.append(RuleResult("phys.speed_p95", st, name,
                          f"速度 p95={p95v:.3f} m/s（≤{cfg.speed_p95_max}），max={max(v_mag):.3f}"))

    low_frac = sum(1 for x in v_mag if x < cfg.speed_low_hint) / len(v_mag)
    out.append(RuleResult("phys.speed_low", PASS, name,
                          f"低速(<{cfg.speed_low_hint} m/s)占比 {low_frac*100:.1f}%（下限提示项，不判定）"))

    # 瞬时速度硬帽（全路）。§3.5 只对第四路写明「speed 依旧不能超过 10」，但前三路同样
    # 不该出现单帧冲刺——表 2 的 6 是 p95 口径（按定义允许 5% 的帧高于它），于是前三路
    # 完全没有瞬时约束，实测漏过 10.63 m/s（单帧位移 0.35m，画面上是一记跳切），
    # orbit-60s clip-013 cam2 两处 150 m/s 瞬移也照常 PASS。
    #
    # cam4 沿用合同口径 speed_cam4_cap（docs/71 §9）；其余路走内控值 speed_inst_cap
    # （README 裁决节）。两者当前同为 10 m/s 但来源不同，分开存放才能日后各自调整而
    # 不至于改合同值时把内控一起改掉。规则名沿用 phys.speed_max（rules 清单已引用）。
    if is_cam4:
        rule, cap, tag, note = "phys.speed_cap4", cfg.speed_cam4_cap, "第四路瞬时速度", ""
    else:
        rule, cap, tag, note = "phys.speed_max", cfg.speed_inst_cap, "瞬时速度", "，内控"
    vmax = max(v_mag)
    vio = [(i, v) for i, v in enumerate(v_mag) if v > cap]
    if not vio:
        out.append(RuleResult(rule, PASS, name,
                              f"{tag} max={vmax:.3f} m/s（硬帽 ≤{cap:g}{note}）"))
    else:
        # 违规必须给帧号与 t：QC 报告的读者要能直接跳到那一帧看画面，
        # 只报 max 无法定位是哪一处跳切。
        def fid(i):
            fr = frames[i].get("frame")
            return fr if isinstance(fr, int) and not isinstance(fr, bool) else i
        detail = "、".join(f"帧 {fid(i)}→{fid(i + 1)}（t={i * dt:.2f}s，{v:.2f} m/s）"
                           for i, v in vio[:5])
        more = "（仅列前 5）" if len(vio) > 5 else ""
        out.append(RuleResult(rule, FAIL, name,
                              f"{tag} max={vmax:.3f} m/s 超硬帽 ≤{cap:g}{note}，"
                              f"违规 {len(vio)} 处：{detail}{more}"))

    p95a = percentile(a_mag, 95)
    st = PASS if p95a <= cfg.accel_p95_max else FAIL
    out.append(RuleResult("phys.accel_p95", st, name,
                          f"加速度 p95={p95a:.3f} m/s²（≤{cfg.accel_p95_max}）"))

    p95j = percentile(j_mag, 95)
    st = PASS if p95j <= cfg.jerk_p95_max else FAIL
    out.append(RuleResult("phys.jerk_p95", st, name,
                          f"jerk p95={p95j:.3f} m/s³（≤{cfg.jerk_p95_max}）"))

    # 角链：四元数差分角 → ω → α → 角 jerk（标量差分）
    w_mag = [quat_angle_deg(a, b) / dt for a, b in zip(quats, quats[1:])]
    alpha = [abs(x) for x in _scalar_diff(w_mag, dt)]
    ang_j = [abs(x) for x in _scalar_diff(alpha, dt)]

    p95w = percentile(w_mag, 95)
    st = PASS if p95w <= cfg.angspeed_p95_max else FAIL
    out.append(RuleResult("phys.angspeed_p95", st, name,
                          f"角速度 p95={p95w:.2f} °/s（≤{cfg.angspeed_p95_max}）"))

    # 瞬时硬帽。§3.4 表头写「上限 (p95)」，正文却写「少数活跃镜头可偶尔触及，
    # 但**不得超过**」——单帧尖峰在 p95 口径下完全隐形：实测 cam4 出现过
    # 330 °/s（单帧转 11°）和 1001 °/s（单帧转 33°）而 p95 只有 15，规则全绿。
    # 这种尖峰在画面上就是一记甩镜，定性验收的「画面抖动/快速切镜」必挂。
    # 第四路的时间重参数化会按链式法则放大角速率（71 号 §6），所以尖峰集中在 cam4。
    wmax = max(w_mag)
    st = PASS if wmax <= cfg.angspeed_max_cap else FAIL
    n_over = sum(1 for x in w_mag if x > cfg.angspeed_max_cap)
    out.append(RuleResult("phys.angspeed_max", st, name,
                          f"角速度瞬时 max={wmax:.1f} °/s（硬帽 ≤{cfg.angspeed_max_cap}）"
                          + (f"，超帽 {n_over} 帧" if n_over else "")))

    p95al = percentile(alpha, 95)
    st = PASS if p95al <= cfg.angaccel_p95_max else FAIL
    out.append(RuleResult("phys.angaccel_p95", st, name,
                          f"角加速度 p95={p95al:.2f} °/s²（≤{cfg.angaccel_p95_max}）"))

    p95aj = percentile(ang_j, 95)
    st = PASS if p95aj <= cfg.angjerk_p95_max else FAIL
    out.append(RuleResult("phys.angjerk_p95", st, name,
                          f"角 jerk p95={p95aj:.2f} °/s³（≤{cfg.angjerk_p95_max}）"))
    return out


def check_roll(name: str, frames, dt: float, cfg: QCConfig) -> List[RuleResult]:
    """MC 歪头闸门 phys.roll：逐帧 |roll| 的占比 + 硬帽（docs/69 §3.2）。

    速率链规则（angspeed/angaccel/angjerk）对**慢漂移**天然盲：capture-v7
    （2026_08_06_plaza_07，orbit 型）四路 roll 从 t5≈-5° 单调漂到 t55≈-74°
    （环绕 up 向量未锁 world-up 的累积漂移），速率始终平缓，128 项规则全绿。
    绝对角度闸门不看变化快慢，慢漂快甩都拦。

    刻意的旋转运镜是合法运镜（歪头 ±5~15°、占比 ≤2%），所以判据不是 roll≡0：
    (a) |roll|>roll_limit_deg 的帧占比 ≤ roll_over_frac_max；
    (b) max|roll| ≤ roll_max_deg（规格 + 容差的硬帽）。
    """
    quats = extract_quats(frames)
    if quats is None:
        return [RuleResult("phys.roll", SKIP, name, "位姿字段异常（见 schema FAIL）")]
    if not quats:
        return [RuleResult("phys.roll", SKIP, name, "无帧可查")]

    rolls = [roll_deg(q) for q in quats]
    over = [i for i, r in enumerate(rolls) if abs(r) > cfg.roll_limit_deg]
    frac = len(over) / len(rolls)
    i_max = max(range(len(rolls)), key=lambda i: abs(rolls[i]))
    rmax = abs(rolls[i_max])

    def fid(i):
        fr = frames[i].get("frame") if isinstance(frames[i], dict) else None
        return fr if isinstance(fr, int) and not isinstance(fr, bool) else i

    peak = f"max|roll|={rmax:.1f}°（帧 {fid(i_max)}，t={i_max * dt:.1f}s，{rolls[i_max]:+.1f}°）"
    problems = []
    if frac > cfg.roll_over_frac_max:
        problems.append(
            f"|roll|>{cfg.roll_limit_deg:g}° 帧占比 {frac * 100:.1f}% > "
            f"{cfg.roll_over_frac_max * 100:g}%（首超帧 {fid(over[0])}，t={over[0] * dt:.1f}s）")
    if rmax > cfg.roll_max_deg:
        problems.append(f"{peak} 超硬帽 ≤{cfg.roll_max_deg:g}°")
    if problems:
        return [RuleResult("phys.roll", FAIL, name, "；".join(problems))]
    return [RuleResult("phys.roll", PASS, name,
                       f"|roll|>{cfg.roll_limit_deg:g}° 帧占比 {frac * 100:.1f}%"
                       f"（≤{cfg.roll_over_frac_max * 100:g}%），{peak}"
                       f"（硬帽 ≤{cfg.roll_max_deg:g}°）")]


# ---------------------------------------------------------------- 4 路同步

def check_mc_sync(loaded: Dict[str, list]) -> List[RuleResult]:
    """4 份 json 帧数一致 + 逐帧 frame 对齐。loaded: cam_key -> frames（可含 None）。"""
    out: List[RuleResult] = []
    present = {k: v for k, v in loaded.items() if v is not None}
    if len(present) < 4:
        missing = sorted(set(loaded) - set(present))
        out.append(RuleResult("sync.frames", SKIP, "action_camera_1..4",
                              f"缺路 {missing}，无法做 4 路同步检查"))
        return out
    counts = {k: len(v) for k, v in present.items()}
    if len(set(counts.values())) == 1:
        out.append(RuleResult("sync.frames", PASS, "action_camera_1..4",
                              f"4 路帧数一致 = {next(iter(counts.values()))}"))
    else:
        out.append(RuleResult("sync.frames", FAIL, "action_camera_1..4",
                              f"4 路帧数不一致：{counts}"))
        return out  # 帧数不齐时逐帧对齐无意义

    n = next(iter(counts.values()))
    for i in range(n):
        fr = {k: (present[k][i].get("frame") if isinstance(present[k][i], dict) else None)
              for k in present}
        if len(set(fr.values())) != 1:
            out.append(RuleResult("sync.frame_align", FAIL, "action_camera_1..4",
                                  f"第 {i} 条 frame 不对齐：{fr}"))
            return out
    out.append(RuleResult("sync.frame_align", PASS, "action_camera_1..4",
                          f"{n} 条逐帧 frame 对齐"))
    return out


# ---------------------------------------------------------------- 解耦闸门

@dataclass
class _WindowMetrics:
    v_vec: Tuple[float, float, float]
    speed: float
    ang_speed: float   # 窗内平均角速度幅值 deg/s
    yaw_rate: float    # deg/s（带符号）
    pitch_rate: float  # deg/s（带符号）


def _window_metrics(pos, quats, dt: float, cfg: QCConfig) -> List[_WindowMetrics]:
    n = len(pos)
    win = max(2, round(cfg.window_s / dt))
    step = max(1, round(cfg.step_s / dt))
    theta = [quat_angle_deg(a, b) for a, b in zip(quats, quats[1:])]
    yp = [yaw_pitch_deg(q) for q in quats]
    yaw = unwrap_deg([a for a, _ in yp])
    pitch = [b for _, b in yp]
    out = []
    s = 0
    while s + win <= n - 1:
        e = s + win
        dur = win * dt
        v = tuple((pos[e][i] - pos[s][i]) / dur for i in range(3))
        out.append(_WindowMetrics(
            v_vec=v,
            speed=vnorm(v),
            ang_speed=sum(theta[s:e]) / dur,
            yaw_rate=(yaw[e] - yaw[s]) / dur,
            pitch_rate=(pitch[e] - pitch[s]) / dur,
        ))
        s += step
    return out


def _pair_sync(a: _WindowMetrics, b: _WindowMetrics, cfg: QCConfig) -> bool:
    # ① 双方均静止
    if (a.speed < cfg.static_v and b.speed < cfg.static_v
            and a.ang_speed < cfg.static_w_deg and b.ang_speed < cfg.static_w_deg):
        return True
    # ② 双方均动且平移方向 cos>0.85 且速率比 ∈ [0.7,1.4]
    if a.speed >= cfg.static_v and b.speed >= cfg.static_v:
        cos = vdot(a.v_vec, b.v_vec) / (a.speed * b.speed)
        ratio = a.speed / b.speed
        if cos > cfg.dir_cos_min and cfg.ratio_lo <= ratio <= cfg.ratio_hi:
            return True
    # ③ |ω_yaw|>5°/s 同号且幅值比 ∈ [0.7,1.4]（pitch 同理）
    for ra, rb in ((a.yaw_rate, b.yaw_rate), (a.pitch_rate, b.pitch_rate)):
        if abs(ra) > cfg.yaw_min_deg and abs(rb) > cfg.yaw_min_deg and ra * rb > 0:
            ratio = abs(ra) / abs(rb)
            if cfg.ratio_lo <= ratio <= cfg.ratio_hi:
                return True
    return False


def check_decouple(loaded: Dict[str, list], dt: float, cfg: QCConfig) -> List[RuleResult]:
    present = {}
    for k, frames in loaded.items():
        if frames is None:
            continue
        pos = extract_positions(frames)
        quats = extract_quats(frames)
        if pos is None or quats is None:
            continue
        present[k] = (pos, quats)
    target = "action_camera_1..4"
    if len(present) < 2:
        return [RuleResult("decouple.pairs", SKIP, target, "可用路数 <2，跳过解耦"),
                RuleResult("decouple.all4", SKIP, target, "可用路数 <2，跳过解耦")]
    n = min(len(p) for p, _ in present.values())
    metrics = {k: _window_metrics(p[:n], q[:n], dt, cfg) for k, (p, q) in present.items()}
    nwin = min(len(m) for m in metrics.values())
    if nwin == 0:
        return [RuleResult("decouple.pairs", SKIP, target, "帧数不足一个滑窗（1s），跳过"),
                RuleResult("decouple.all4", SKIP, target, "帧数不足一个滑窗（1s），跳过")]

    keys = sorted(present.keys())
    pair_frac: Dict[str, float] = {}
    pair_sync_flags: Dict[Tuple[str, str], List[bool]] = {}
    for i, ka in enumerate(keys):
        for kb in keys[i + 1:]:
            flags = [_pair_sync(metrics[ka][w], metrics[kb][w], cfg) for w in range(nwin)]
            pair_sync_flags[(ka, kb)] = flags
            pair_frac[f"{ka}-{kb}"] = sum(flags) / nwin

    out: List[RuleResult] = []
    worst = max(pair_frac.items(), key=lambda kv: kv[1])
    detail = ", ".join(f"{k}:{v*100:.0f}%" for k, v in sorted(pair_frac.items()))
    if worst[1] < cfg.pair_sync_max:
        out.append(RuleResult("decouple.pairs", PASS, target,
                              f"每对同步窗占比 <{cfg.pair_sync_max*100:.0f}%（{detail}；{nwin} 窗）"))
    else:
        out.append(RuleResult("decouple.pairs", FAIL, target,
                              f"对 {worst[0]} 同步占比 {worst[1]*100:.0f}% ≥ "
                              f"{cfg.pair_sync_max*100:.0f}%（{detail}；{nwin} 窗）"))

    if len(keys) == 4:
        all4 = sum(1 for w in range(nwin)
                   if all(flags[w] for flags in pair_sync_flags.values())) / nwin
        st = PASS if all4 < cfg.all4_sync_max else FAIL
        cmp = "<" if st == PASS else "≥"
        out.append(RuleResult("decouple.all4", st, target,
                              f"四路同时同步占比 {all4*100:.0f}% {cmp} {cfg.all4_sync_max*100:.0f}%"))
    else:
        out.append(RuleResult("decouple.all4", SKIP, target, f"仅 {len(keys)} 路可用，跳过四路同时判定"))
    return out


# ---------------------------------------------------------------- 初始帧

def _lane_no(key) -> int:
    """从 loaded 键取 1-based 车道号（键形如 "action_camera_4" / "video_4" / 4）。"""
    if isinstance(key, int):
        return key
    digits = "".join(ch for ch in str(key) if ch.isdigit())
    return int(digits[-1]) if digits else -1


def check_initial_frame(loaded: Dict[str, list], cfg: QCConfig) -> List[RuleResult]:
    target = "action_camera_1..4"
    firsts = {}
    for k, frames in loaded.items():
        if not frames:
            continue
        f = frames[0]
        p = extract_positions([f])
        q = extract_quats([f])
        if p and q:
            firsts[k] = (p[0], q[0])
    if len(firsts) < 2:
        return [RuleResult("init.orientation", SKIP, target, "可用首帧 <2 路，跳过"),
                RuleResult("init.distance", SKIP, target, "可用首帧 <2 路，跳过")]
    exempt = set(getattr(cfg, "init_exempt_lanes", ()) or ())
    keys = [k for k in sorted(firsts.keys()) if _lane_no(k) not in exempt]
    if len(keys) < 2:
        keys = sorted(firsts.keys())
    max_ang, max_dist = 0.0, 0.0
    ang_pair = dist_pair = ""
    for i, ka in enumerate(keys):
        for kb in keys[i + 1:]:
            ang = angle_between_deg(quat_forward(firsts[ka][1]), quat_forward(firsts[kb][1]))
            dist = vnorm(vsub(firsts[ka][0], firsts[kb][0]))
            if ang > max_ang:
                max_ang, ang_pair = ang, f"{ka}-{kb}"
            if dist > max_dist:
                max_dist, dist_pair = dist, f"{ka}-{kb}"
    st = PASS if max_ang <= cfg.init_angle_max_deg else FAIL
    out = [RuleResult("init.orientation", st, target,
                      f"首帧朝向最大夹角 {max_ang:.2f}°（对 {ang_pair or '-'}，≤{cfg.init_angle_max_deg}°，内控值）")]
    st = PASS if max_dist <= cfg.init_dist_max_m else FAIL
    out.append(RuleResult("init.distance", st, target,
                          f"首帧位置最大距离 {max_dist:.2f} m（对 {dist_pair or '-'}，≤{cfg.init_dist_max_m} m，内控值）"))
    return out


# ---------------------------------------------------------------- 第四路（需 plan.json）

#: docs/69 §8.3 落定的两种第四路可接受形态：
#: a) timewarp —— 同路线时间错位 + 一次变速（既有契约，docs/71 §6，验收 demo mc1）；
#: b) topdown —— 同区域高空垂直俯视伴飞（验收 demo mc2）。
#: plan.cam4 无 mode 键 ⇒ 按 timewarp 走既有三条规则，输出与增补前逐字一致。
CAM4_MODES = ("timewarp", "topdown")
CAM4_TIMEWARP_RULES = ("cam4.switch_count", "cam4.t_sw", "cam4.band_dwell")
CAM4_TOPDOWN_RULES = ("cam4.topdown_pitch", "cam4.topdown_alt", "cam4.topdown_route")


def cam4_declared_mode(plan_obj) -> Optional[str]:
    """plan.cam4.mode 的原样声明值；无 plan / 无 cam4 段 / 无 mode 键 → None。

    clip 级编排用它决定 phys.roll 跳检与初始帧豁免（只认显式 "topdown"，
    非法值不触发任何豁免——豁免跟着一个坏声明走会把两处闸门一起放空）。
    """
    if not isinstance(plan_obj, dict):
        return None
    cam4_plan = plan_obj.get("cam4")
    if not isinstance(cam4_plan, dict):
        return None
    mode = cam4_plan.get("mode")
    return mode if isinstance(mode, str) else None


def _smooth_median(series: List[Optional[float]], half: int) -> List[Optional[float]]:
    out: List[Optional[float]] = []
    n = len(series)
    for i in range(n):
        vals = [x for x in series[max(0, i - half):min(n, i + half + 1)] if x is not None]
        out.append(median(vals) if len(vals) >= half else None)
    return out


def check_cam4(loaded: Dict[str, list], plan_obj, plan_err: Optional[str],
               dt: float, cfg: QCConfig) -> List[RuleResult]:
    target = "action_camera_4"

    def skip_rules(rules, reason: str):
        return [RuleResult(r, SKIP, target, reason) for r in rules]

    if plan_obj is None:
        reason = "plan.json 缺失" if plan_err is None else f"plan.json 解析失败：{plan_err}"
        return skip_rules(CAM4_TIMEWARP_RULES, reason)
    cam4_plan = plan_obj.get("cam4") if isinstance(plan_obj, dict) else None
    if not isinstance(cam4_plan, dict):
        return skip_rules(CAM4_TIMEWARP_RULES, "plan.json 无 cam4 段，跳过第四路变速检查")

    mode_raw = cam4_plan.get("mode")
    out: List[RuleResult] = []
    if mode_raw is None:
        mode = "timewarp"      # 缺省 = 既有契约；不发 cam4.mode，历史 clip 输出不变
    elif mode_raw in CAM4_MODES:
        mode = mode_raw
        form = "a·同路线时间错位变速" if mode == "timewarp" else "b·同区域高空垂直俯视伴飞"
        out.append(RuleResult("cam4.mode", PASS, target,
                              f"plan.cam4.mode={mode}（docs/69 §8.3 形态 {form}）"))
    else:
        # 非法声明：两组形态规则都不判——按坏声明挑一组来判等于替采集侧猜意图
        out.append(RuleResult("cam4.mode", FAIL, target,
                              f"plan.cam4.mode={mode_raw!r} 不在 {set(CAM4_MODES)}"))
        out.extend(skip_rules(CAM4_TIMEWARP_RULES + CAM4_TOPDOWN_RULES,
                              "cam4.mode 非法，两组形态规则均不判"))
        return out

    if mode == "topdown":
        out.extend(skip_rules(CAM4_TIMEWARP_RULES,
                              "cam4.mode=topdown：变速契约不适用（docs/69 §8.3 形态 b）"))
        out.extend(_check_cam4_topdown(loaded, cam4_plan, dt, cfg))
        return out
    out.extend(_check_cam4_timewarp(loaded, cam4_plan, dt, cfg))
    return out


def _check_cam4_timewarp(loaded: Dict[str, list], cam4_plan: dict,
                         dt: float, cfg: QCConfig) -> List[RuleResult]:
    target = "action_camera_4"

    def skip_all(reason: str):
        return [RuleResult(r, SKIP, target, reason) for r in CAM4_TIMEWARP_RULES]

    guide_key = str(cam4_plan.get("guide_cam", 1))
    frames4 = loaded.get("4")
    frames_g = loaded.get(guide_key)
    if frames4 is None or frames_g is None:
        return skip_all(f"action_camera_4 或 guide（cam{guide_key}）json 缺失")
    p4, pg = extract_positions(frames4), extract_positions(frames_g)
    if p4 is None or pg is None or min(len(p4), len(pg)) < 8:
        return skip_all("位姿字段异常或帧数不足，无法反算 ρ(t)")

    n = min(len(p4), len(pg))
    v4 = [vnorm(vsub(p4[i + 1], p4[i])) / dt for i in range(n - 1)]
    vg = [vnorm(vsub(pg[i + 1], pg[i])) / dt for i in range(n - 1)]
    rho_raw: List[Optional[float]] = [
        (v4[i] / vg[i]) if vg[i] > 0.05 else None for i in range(n - 1)
    ]
    half = max(2, round(0.25 / dt))  # ~0.5s 中值窗
    rho = _smooth_median(rho_raw, half)
    valid_n = sum(1 for x in rho if x is not None)
    if valid_n < (n - 1) * 0.5:
        return skip_all(f"guide 低速占比过高，ρ(t) 可用样本 {valid_n}/{n-1} 不足")

    out: List[RuleResult] = []
    # --- 变速事件检测：前后 0.5s 中值的对数比 ---
    k = max(3, round(0.5 / dt))
    scores: List[Tuple[int, float]] = []
    for i in range(k, len(rho) - k):
        pre = median([x for x in rho[i - k:i] if x is not None] or [])
        post = median([x for x in rho[i:i + k] if x is not None] or [])
        if pre and post and pre > 0 and post > 0:
            s = abs(math.log(post / pre))
            if s > cfg.rho_change_log:
                scores.append((i, s))
    events: List[int] = []
    if scores:
        group = [scores[0]]
        for idx, s in scores[1:]:
            if idx - group[-1][0] <= k:
                group.append((idx, s))
            else:
                events.append(max(group, key=lambda t: t[1])[0])
                group = [(idx, s)]
        events.append(max(group, key=lambda t: t[1])[0])

    if len(events) == 1:
        out.append(RuleResult("cam4.switch_count", PASS, target,
                              f"检测到变速恰 1 次（t≈{events[0]*dt:.1f}s）"))
    else:
        out.append(RuleResult("cam4.switch_count", FAIL, target,
                              f"检测到变速 {len(events)} 次（应恰 1 次）"
                              f"{'，t≈' + ','.join(f'{e*dt:.1f}s' for e in events) if events else ''}"))

    # --- t_sw：plan 申报值在 [20,40] 且实测中心与 plan(t_sw+Δ/2) 一致 ---
    t_sw = cam4_plan.get("t_sw")
    delta = cam4_plan.get("delta", 2.0)
    if not isinstance(t_sw, (int, float)):
        out.append(RuleResult("cam4.t_sw", FAIL, target, f"plan.cam4.t_sw 异常：{t_sw!r}"))
    elif not (cfg.tsw_lo <= t_sw <= cfg.tsw_hi):
        out.append(RuleResult("cam4.t_sw", FAIL, target,
                              f"plan t_sw={t_sw:g}s ∉ [{cfg.tsw_lo:g},{cfg.tsw_hi:g}]s"))
    elif len(events) == 1:
        measured = events[0] * dt
        expect = t_sw + (delta or 0) / 2.0
        if abs(measured - expect) <= cfg.tsw_measure_tol_s:
            out.append(RuleResult("cam4.t_sw", PASS, target,
                                  f"plan t_sw={t_sw:g}s ∈ [{cfg.tsw_lo:g},{cfg.tsw_hi:g}]，"
                                  f"实测切换中心 {measured:.1f}s ≈ 期望 {expect:.1f}s"))
        else:
            out.append(RuleResult("cam4.t_sw", FAIL, target,
                                  f"实测切换中心 {measured:.1f}s 偏离 plan 期望 {expect:.1f}s "
                                  f"超容差 {cfg.tsw_measure_tol_s:g}s"))
    else:
        out.append(RuleResult("cam4.t_sw", FAIL, target,
                              f"plan t_sw={t_sw:g}s 合规，但变速事件数 ≠1，无法核对实测切换点"))

    # --- 禁带驻留：ρ∈(0.65,1.5) 总时长扣除一次单调穿越 ≤1s ---
    in_band = [(x is not None and cfg.band_lo < x < cfg.band_hi) for x in rho]
    runs: List[Tuple[int, int]] = []  # [start, end) 帧区间
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
    total = sum((e - s) for s, e in runs) * dt
    crossing = 0.0
    cross_run = None
    for s, e in runs:
        before = next((rho[i] for i in range(s - 1, -1, -1) if rho[i] is not None), None)
        after = next((rho[i] for i in range(e, len(rho)) if rho[i] is not None), None)
        if before is None or after is None:
            continue
        lo, hi = cfg.band_lo, cfg.band_hi
        if (before <= lo and after >= hi) or (before >= hi and after <= lo):
            dur = (e - s) * dt
            if dur > crossing:
                crossing, cross_run = dur, (s, e)
    dwell = total - crossing
    if dwell <= cfg.band_dwell_max_s:
        out.append(RuleResult("cam4.band_dwell", PASS, target,
                              f"禁带({cfg.band_lo},{cfg.band_hi})驻留 {dwell:.2f}s ≤ {cfg.band_dwell_max_s:g}s"
                              f"（总 {total:.2f}s，扣除单调穿越 {crossing:.2f}s）"))
    else:
        out.append(RuleResult("cam4.band_dwell", FAIL, target,
                              f"禁带({cfg.band_lo},{cfg.band_hi})驻留 {dwell:.2f}s > {cfg.band_dwell_max_s:g}s"
                              f"（总 {total:.2f}s，扣除单调穿越 {crossing:.2f}s）"))
    return out


def _check_cam4_topdown(loaded: Dict[str, list], cam4_plan: dict,
                        dt: float, cfg: QCConfig) -> List[RuleResult]:
    """俯视伴飞形态验形（docs/69 §8.3 形态 b，验收 demo mc2）。

    demo 可观察到的三个结构特征各立一条规则：
    ① 全程垂直向下看（cam4.topdown_pitch）；
    ② 高空——显著高于前三路（cam4.topdown_alt）；
    ③ 沿主体行进路线上空漂移、路径强相关（cam4.topdown_route）。
    与其余运动面规则同源：只信位姿差分，不信画面。阈值全部是内控标定
    （config「第四路俯视伴飞」节），demo 只给形态不给数值。
    """
    target = "action_camera_4"

    def skip_all(reason: str):
        return [RuleResult(r, SKIP, target, reason) for r in CAM4_TOPDOWN_RULES]

    frames4 = loaded.get("4")
    if frames4 is None:
        return skip_all("action_camera_4 json 缺失")
    p4, q4 = extract_positions(frames4), extract_quats(frames4)
    if p4 is None or q4 is None or len(p4) < 8:
        return skip_all("位姿字段异常或帧数不足")

    def fid(i):
        fr = frames4[i].get("frame") if isinstance(frames4[i], dict) else None
        return fr if isinstance(fr, int) and not isinstance(fr, bool) else i

    out: List[RuleResult] = []

    # --- ① 垂直俯视：forward 与世界 -y 的偏角（占比闸 + 硬帽，结构同 phys.roll）---
    devs = [angle_between_deg(quat_forward(q), (0.0, -1.0, 0.0)) for q in q4]
    over = [i for i, a in enumerate(devs) if a > cfg.topdown_pitch_tol_deg]
    frac = len(over) / len(devs)
    frac_max = 1.0 - cfg.topdown_pitch_frac_min
    i_max = max(range(len(devs)), key=lambda i: devs[i])
    peak = f"max 偏角 {devs[i_max]:.1f}°（帧 {fid(i_max)}，t={i_max * dt:.1f}s）"
    problems = []
    if frac > frac_max:
        problems.append(f"偏离垂直向下 >{cfg.topdown_pitch_tol_deg:g}° 帧占比 "
                        f"{frac * 100:.1f}% > {frac_max * 100:g}%"
                        f"（首超帧 {fid(over[0])}，t={over[0] * dt:.1f}s）")
    if devs[i_max] > cfg.topdown_pitch_max_deg:
        problems.append(f"{peak} 超硬帽 ≤{cfg.topdown_pitch_max_deg:g}°")
    if problems:
        out.append(RuleResult("cam4.topdown_pitch", FAIL, target, "；".join(problems)))
    else:
        out.append(RuleResult("cam4.topdown_pitch", PASS, target,
                              f"偏离垂直向下 >{cfg.topdown_pitch_tol_deg:g}° 帧占比 "
                              f"{frac * 100:.1f}%（≤{frac_max * 100:g}%），{peak}"
                              f"（硬帽 ≤{cfg.topdown_pitch_max_deg:g}°）"))

    # --- ② 高空：相对前三路当帧最高点的高差（相对量不随场景标定）---
    others = []
    for k in ("1", "2", "3"):
        pk = extract_positions(loaded[k]) if loaded.get(k) is not None else None
        if pk:
            others.append(pk)
    if not others:
        out.append(RuleResult("cam4.topdown_alt", SKIP, target,
                              "前三路位姿均不可用，无法算相对高差"))
    else:
        n = min(len(p4), *(len(pk) for pk in others))
        margins = [p4[i][1] - max(pk[i][1] for pk in others) for i in range(n)]
        under = [i for i, m in enumerate(margins) if m < cfg.topdown_alt_min_m]
        frac = len(under) / n
        frac_max = 1.0 - cfg.topdown_alt_frac_min
        i_min = min(range(n), key=lambda i: margins[i])
        low = f"min 高差 {margins[i_min]:.1f} m（帧 {fid(i_min)}，t={i_min * dt:.1f}s）"
        if frac > frac_max:
            out.append(RuleResult("cam4.topdown_alt", FAIL, target,
                                  f"高差 <{cfg.topdown_alt_min_m:g} m 帧占比 {frac * 100:.1f}% > "
                                  f"{frac_max * 100:g}%，{low}"
                                  f"（首欠帧 {fid(under[0])}，t={under[0] * dt:.1f}s）"))
        else:
            out.append(RuleResult("cam4.topdown_alt", PASS, target,
                                  f"高差 <{cfg.topdown_alt_min_m:g} m 帧占比 {frac * 100:.1f}%"
                                  f"（≤{frac_max * 100:g}%），{low}"))

    # --- ③ 路径相关：到 guide 路线 XZ 折线的水平距离（p95 + 硬帽）---
    # 折线口径而非逐帧同步距离：俯视位允许沿同一路线时间超前/滞后（§8.3 实测），
    # 逐帧距离会把合法错位判成脱队。抽样 0.2s 一点，guide 每 0.2s 至多走
    # 1.2 m（p95 6 m/s），点距误差 ≲0.6 m，相对 20/40 m 的阈值可忽略。
    guide_key = str(cam4_plan.get("guide_cam", 1))
    pg = extract_positions(loaded[guide_key]) if loaded.get(guide_key) is not None else None
    if not pg:
        out.append(RuleResult("cam4.topdown_route", SKIP, target,
                              f"guide（cam{guide_key}）位姿不可用，无法算路线走廊距离"))
        return out
    step = max(1, round(0.2 / dt))
    route = [(p[0], p[2]) for p in pg[::step]]
    if (pg[-1][0], pg[-1][2]) != route[-1]:
        route.append((pg[-1][0], pg[-1][2]))
    dists = [min(math.hypot(p[0] - rx, p[2] - rz) for rx, rz in route) for p in p4]
    p95 = percentile(dists, 95)
    i_far = max(range(len(dists)), key=lambda i: dists[i])
    far = f"max {dists[i_far]:.1f} m（帧 {fid(i_far)}，t={i_far * dt:.1f}s）"
    problems = []
    if p95 > cfg.topdown_route_xz_p95_max:
        problems.append(f"到 guide（cam{guide_key}）路线的水平距离 p95={p95:.1f} m > "
                        f"{cfg.topdown_route_xz_p95_max:g} m")
    if dists[i_far] > cfg.topdown_route_xz_max_m:
        problems.append(f"{far} 超硬帽 ≤{cfg.topdown_route_xz_max_m:g} m")
    if problems:
        out.append(RuleResult("cam4.topdown_route", FAIL, target, "；".join(problems)))
    else:
        out.append(RuleResult("cam4.topdown_route", PASS, target,
                              f"到 guide（cam{guide_key}）路线的水平距离 p95={p95:.1f} m"
                              f"（≤{cfg.topdown_route_xz_p95_max:g} m），{far}"
                              f"（硬帽 ≤{cfg.topdown_route_xz_max_m:g} m）"))
    return out

"""运动面：相机运动参数 p95 / 瞬时硬帽 / roll、六向平移覆盖、A 两路自由运镜独立性、运镜段落统计、
真实输入 ↔ 相机/人物运动一致性（WASD、鼠标、Q/E）。

需求出处：新需求 §3.4「运镜参数上限 / 自由运镜路线 / 双路独立」、§4.2、§5.1、§6.1、§6.2；docs/69 §3.2（瞬时 10 m/s、roll）。

全部由 camera_position / camera_rotation_quaternion 有限差分反算（复用 engine_qc.checks_motion），
不信任文件里的 camera_speed。坐标：docs/69 y-up 左手系，相机本体系 R/U/F = q 旋转 (1,0,0)/(0,1,0)/(0,0,1)。
"""

from __future__ import annotations

import math
from typing import Dict, List, Optional, Sequence, Tuple

from engine_qc.checks_motion import check_decouple, check_physics, check_roll
from engine_qc.config import QCConfig
from engine_qc.mathutil import percentile, quat_angle_deg, unwrap_deg, vnorm, vsub, yaw_pitch_deg

from .checks_json import DT, LIFT_KEYS, MOVE_KEYS, LaneData, carries_input, unplayed_frames
from .loaders import Group
from .model import (FAIL, INPUT_ROLES, MANUAL, PASS, PERSON_ROLES, PROGRAM_FREE_ROLES, SKIP, STILL_ROLES, WARN, RuleResult)

Vec3 = Tuple[float, float, float]

#: 六向平移覆盖：相机本体系每个方向的累计位移下限（m）。与 engine_qc 粉色平移诚实性的 0.8 m 同源。
SIX_DIR_MIN_M = 0.8
#: 「静止」判据（1 s 窗）：与 engine_qc 解耦阈值同值
STATIC_V, STATIC_W = 0.2, 3.0
#: 输入↔运动一致性：按键帧中速度分量符号与键位方向一致的占比下限
KEY_AGREE_MIN = 0.70
#: 八方向（需方 2026-09-30：没有鼠标转动时纯按 WASD 只能走八方向，走出斜角必须有鼠标转动的记录）。只看「稳态」帧：
#: 之前 EIGHT_DIR_STEADY 帧里镜头朝向没转过 EIGHT_DIR_STEADY_TURN 度、按着同一组移动键——起步那一秒身体还在转向、
#: 边转镜头边走身体跟不上，都是游戏物理，不算。按镜头有没有转而不是鼠标有没有动：程序走人时鼠标一直有零点几的微调
EIGHT_DIR_STEADY = 30         # 帧（1 s）
EIGHT_DIR_STEADY_TURN = 5.0   # 度
EIGHT_DIR_MEDIAN_MAX = 5.0    # 度；beach-demo-003 的追背路（游戏镜头本身）稳态中位 3° 上下
EIGHT_DIR_P90_MAX = 15.0      # 度
EIGHT_DIR_MIN_FRAMES = 10
#: 鼠标 dx/dy 与偏航/俯仰速率的相关系数下限（绝对值）
MOUSE_CORR_MIN = 0.40
MAX_LAG = 10
#: 游戏镜头（追背/第一人称）是否套用 p95 上限尚未与需方确认——超限只提示不判死
GAME_CAMERA_ROLES = ("chase", "first_person")


def rotate(q: Sequence[float], v: Sequence[float]) -> Vec3:
    """q=[x,y,z,w] 旋转向量 v（Hamilton 积，与 mathutil.quat_forward 同一约定）。"""
    x, y, z, w = q
    tx = 2.0 * (y * v[2] - z * v[1])
    ty = 2.0 * (z * v[0] - x * v[2])
    tz = 2.0 * (x * v[1] - y * v[0])
    return (v[0] + w * tx + (y * tz - z * ty),
            v[1] + w * ty + (z * tx - x * tz),
            v[2] + w * tz + (x * ty - y * tx))


def basis(q: Sequence[float]) -> Tuple[Vec3, Vec3, Vec3]:
    """相机本体系 (right, up, forward)。"""
    return rotate(q, (1.0, 0.0, 0.0)), rotate(q, (0.0, 1.0, 0.0)), rotate(q, (0.0, 0.0, 1.0))


def _dot(a, b) -> float:
    return a[0] * b[0] + a[1] * b[1] + a[2] * b[2]


def _pearson(a: Sequence[float], b: Sequence[float]) -> Optional[float]:
    n = min(len(a), len(b))
    if n < 10:
        return None
    a, b = a[:n], b[:n]
    ma, mb = sum(a) / n, sum(b) / n
    sa = math.sqrt(sum((x - ma) ** 2 for x in a))
    sb = math.sqrt(sum((x - mb) ** 2 for x in b))
    if sa == 0 or sb == 0:
        return None
    return sum((x - ma) * (y - mb) for x, y in zip(a, b)) / (sa * sb)


def _best_lag(x: Sequence[float], y: Sequence[float], max_lag: int = MAX_LAG) -> Tuple[Optional[float], int]:
    """y 滞后 x 若干帧时的最大 |r|。返回 (r, lag)。"""
    best, best_lag = None, 0
    for lag in range(0, max_lag + 1):
        r = _pearson(x[: len(x) - lag] if lag else x, y[lag:])
        if r is not None and (best is None or abs(r) > abs(best)):
            best, best_lag = r, lag
    return best, best_lag


# ---------------------------------------------------------------- 物理阈值（复用 engine_qc）

def _convert(res, role: str, key: str, req: str) -> RuleResult:
    """engine_qc RuleResult → worldline RuleResult，规则名前缀 motion.；游戏镜头超限降为 WARN。"""
    rule = "motion." + res.rule.split(".", 1)[1]
    status = res.status
    ev = res.evidence
    if status == FAIL and role in GAME_CAMERA_ROLES:
        status = WARN
        ev += "（追背/第一人称是游戏镜头，运镜参数上限是否适用待需方确认，暂不判死）"
    return RuleResult(rule, status, key, ev, req)


def check_lane_motion(ld: LaneData, cfg: Optional[QCConfig] = None) -> List[RuleResult]:
    """单路：p95 六项 + 瞬时硬帽 + roll + 六向平移 + 段落统计。"""
    cfg = cfg or QCConfig()
    out: List[RuleResult] = []
    key, role = ld.key, ld.role
    if not ld.records:
        return [RuleResult("motion.physics", SKIP, key, "无记录", "需求§3.4")]
    pos, qs = ld.positions(), ld.quats()
    if pos is None or qs is None:
        return [RuleResult("motion.physics", SKIP, key, "位姿字段异常（见 json.types）", "需求§3.4")]
    if role == "fixed":
        # 固定机位不动，平移链恒 0；只看转动链（允许平滑转向，禁止甩动）
        for r in check_physics(key, ld.records, DT, cfg, is_cam4=False):
            if r.rule.startswith("phys.ang"):
                out.append(_convert(r, role, key, "需求§3.4 运镜参数上限"))
        out.append(RuleResult("motion.six_dir", SKIP, key, "固定机位不适用六向平移", "需求§3.4"))
        return out

    for r in check_physics(key, ld.records, DT, cfg, is_cam4=False):
        if r.rule == "phys.speed_low":
            continue   # 用下面的静止占比代替
        out.append(_convert(r, role, key, "需求§3.4 运镜参数上限 / docs/69 §3.2"))
    for r in check_roll(key, ld.records, DT, cfg):
        out.append(_convert(r, role, key, "docs/69 §3.2 roll ±15°"))

    # 六向平移覆盖（相机本体系，累计位移）
    acc = {"前": 0.0, "后": 0.0, "右": 0.0, "左": 0.0, "上": 0.0, "下": 0.0}
    for i in range(len(pos) - 1):
        d = vsub(pos[i + 1], pos[i])
        R, U, F = basis(qs[i])
        for comp, (pos_name, neg_name) in ((_dot(d, F), ("前", "后")), (_dot(d, R), ("右", "左")), (_dot(d, U), ("上", "下"))):
            if comp > 0:
                acc[pos_name] += comp
            else:
                acc[neg_name] -= comp
    txt = "、".join(f"{k} {v:.1f} m" for k, v in acc.items())
    if role in PROGRAM_FREE_ROLES:
        missing = [k for k, v in acc.items() if v < SIX_DIR_MIN_M]
        if missing:
            out.append(RuleResult("motion.six_dir", FAIL, key,
                                  f"六向平移未覆盖：缺 {'/'.join(missing)}（每向累计 ≥{SIX_DIR_MIN_M} m）；实测 {txt}",
                                  "需求§3.4「每路自由运镜视频包含六向平移」", data={"six_dir": acc}))
        else:
            out.append(RuleResult("motion.six_dir", PASS, key, f"六向平移均覆盖：{txt}", "需求§3.4", data={"six_dir": acc}))
    elif role == "free_camera":
        out.append(RuleResult("motion.six_dir", SKIP, key, f"B 真实输入自由相机不套用运镜路线要求；实测 {txt}", "需求§3.4/§4.2",
                              data={"six_dir": acc}))
    else:
        out.append(RuleResult("motion.six_dir", SKIP, key, f"{role} 为游戏镜头，不套用运镜路线要求；实测 {txt}", "需求§3.4",
                              data={"six_dir": acc}))

    # 段落 / 运镜类型统计（1 s 窗）：静止 / 纯平移 / 纯转动 / 平移+转动
    n = len(pos)
    win = int(round(1.0 / DT))
    kinds: List[str] = []
    speeds: List[float] = []
    for s in range(0, n - win, win):
        e = s + win
        v = vnorm(vsub(pos[e], pos[s])) / (win * DT)
        w = sum(quat_angle_deg(qs[i], qs[i + 1]) for i in range(s, e)) / (win * DT)
        speeds.append(v)
        moving, turning = v >= STATIC_V, w >= STATIC_W
        kinds.append("静止" if not moving and not turning else "纯平移" if moving and not turning
                     else "纯转动" if turning and not moving else "平移+转动")
    counts = {k: kinds.count(k) for k in ("静止", "纯平移", "纯转动", "平移+转动")}
    # 段数估计：类型变化次数 + 1
    segs = 1 + sum(1 for a, b in zip(kinds, kinds[1:]) if a != b)
    static_frac = counts["静止"] / len(kinds) if kinds else 0.0
    v_all = [vnorm(vsub(pos[i + 1], pos[i])) / DT for i in range(n - 1)]
    data = {"kinds_1s": kinds, "speed_1s": speeds, "speed_p50": percentile(v_all, 50), "speed_p95": percentile(v_all, 95)}
    if role in PROGRAM_FREE_ROLES:
        out.append(RuleResult("motion.segments", MANUAL, key,
                              f"1 s 窗类型分布 {counts}（估计段数 {segs}）；速度 p50 {data['speed_p50']:.2f} / p95 {data['speed_p95']:.2f} m/s。"
                              "需求：120 s 随机分 1–10 段、每段至少一种六向平移以外的运镜、参数随机、段间平滑——段落划分与类目须对照运镜类目树人工判",
                              "需求§3.4 自由运镜路线", data=data))
        st = PASS if static_frac < 0.5 else WARN
        out.append(RuleResult("motion.static_fraction", st, key,
                              f"静止窗占比 {static_frac * 100:.0f}%（<50% 视为在运镜）", "需求§3.4", data={"static_frac": static_frac}))
    else:
        out.append(RuleResult("motion.segments", SKIP, key,
                              f"{role} 不套用段落要求；1 s 窗类型分布 {counts}，速度 p50 {data['speed_p50']:.2f} / p95 {data['speed_p95']:.2f} m/s",
                              "需求§3.4", data=data))
    return out


# ---------------------------------------------------------------- A 两路自由运镜独立性

def check_a_independence(g: Group, lanes: Dict[str, LaneData], cfg: Optional[QCConfig] = None) -> List[RuleResult]:
    cfg = cfg or QCConfig()
    free = [lanes[lf.info.key] for lf in g.lanes_of("A")
            if lf.info.role in PROGRAM_FREE_ROLES and lanes.get(lf.info.key) and lanes[lf.info.key].records]
    if len(free) < 2:
        return [RuleResult("motion.a_independence", SKIP, "世界线A", f"A 的程序自由运镜路不足 2（实有 {len(free)}）", "需求§3.4 双路独立")]
    loaded = {ld.key: ld.records for ld in free}
    res = check_decouple(loaded, DT, cfg)
    out = []
    for r in res:
        if r.rule == "decouple.pairs":
            st = r.status
            out.append(RuleResult("motion.a_independence", st, "世界线A",
                                  ("A 的两路自由运镜 1 s 窗同步占比：" + r.evidence
                                   + "（同步 = 同时静止 / 同向同速平移 / 同向同速摇转；过半时长同步即不达标）"),
                                  "需求§3.4「A 的两路自由运镜过半时长内不得同步运镜」"))
    return out


# ---------------------------------------------------------------- 真实输入 ↔ 运动

def _keys_series(ld: LaneData) -> List[set]:
    return [set(r.get("keyCode") or []) for r in ld.records]


def _key_offset_deg(keys: set) -> Optional[float]:
    """WASD 组合对应的方向，相对镜头朝向、向右为正：W 0、D 90、A −90、S 180、W+D 45 …；没按移动键为 None。"""
    f = (87 in keys) - (83 in keys)
    r = (68 in keys) - (65 in keys)
    if not f and not r:
        return None
    return math.degrees(math.atan2(r, f))


def eight_directions(keys: Sequence[set], yaw_deg: Sequence[float], rel_way: Sequence[Optional[float]],
                     key: str, role: str) -> RuleResult:
    """镜头没转时只按 WASD，走的方向是不是镜头朝向 + 键位方向（八方向之一）。yaw_deg：这一路镜头逐帧偏航（已去 360° 跳变）；
    rel_way[i]：第 i 帧实际运动方向相对镜头朝向的角度（向右为正），没在动为 None。只算稳态帧：这一帧之前 EIGHT_DIR_STEADY
    帧里镜头偏航变化不超过 EIGHT_DIR_STEADY_TURN 度、移动键没变。"""
    errs, held = [], 0
    for i in range(min(len(keys), len(rel_way), len(yaw_deg))):
        moving = keys[i] & MOVE_KEYS
        same = i > 0 and (keys[i] & MOVE_KEYS) == (keys[i - 1] & MOVE_KEYS)
        held = held + 1 if (moving and same) else (1 if moving else 0)
        if held <= EIGHT_DIR_STEADY or rel_way[i] is None:
            continue
        window = yaw_deg[i - EIGHT_DIR_STEADY:i + 1]
        if max(window) - min(window) > EIGHT_DIR_STEADY_TURN:
            continue
        errs.append(abs((rel_way[i] - _key_offset_deg(keys[i]) + 180.0) % 360.0 - 180.0))
    if len(errs) < EIGHT_DIR_MIN_FRAMES:
        return RuleResult("input.eight_directions", WARN, key,
                          f"「镜头一秒内没转过 {EIGHT_DIR_STEADY_TURN:g}°、持续按同一组移动键超过 1 s 且在动」的稳态帧只有 {len(errs)} 帧"
                          f"（不足 {EIGHT_DIR_MIN_FRAMES}），无法判八方向", "需方 2026-09-30")
    med, p90 = percentile(errs, 50), percentile(errs, 90)
    within5 = sum(e <= 5.0 for e in errs) / len(errs)
    ok = med <= EIGHT_DIR_MEDIAN_MAX and p90 <= EIGHT_DIR_P90_MAX
    return RuleResult("input.eight_directions", PASS if ok else WARN, key,
                      f"镜头一秒内没转过 {EIGHT_DIR_STEADY_TURN:g}°、持续按同一组 WASD 超过 1 s 的稳态帧 {len(errs)} 帧：走的方向对「镜头朝向 + 键位方向」中位 {med:.1f}°、"
                      f"p90 {p90:.1f}°、5° 内 {within5 * 100:.0f}%（门槛中位 ≤{EIGHT_DIR_MEDIAN_MAX:g}°、p90 ≤{EIGHT_DIR_P90_MAX:g}°；"
                      "需方只说「完全一致」，数值是我们定的）",
                      "需方 2026-09-30（没有鼠标输入时纯 WASD 只走八方向，斜角须有鼠标转动）",
                      data={"median_deg": med, "p90_deg": p90, "within_5_deg": within5, "frames": len(errs)})


def check_input_consistency(ld: LaneData, takeover_frame: Optional[int] = None) -> List[RuleResult]:
    """B：WASD/QE ↔ 相机本体系速度符号；鼠标 dx/dy ↔ 偏航/俯仰速率。
    C/D 人物路：WASD ↔ 人物在动；鼠标 dx ↔ 相机偏航速率（追背路）。"""
    out: List[RuleResult] = []
    key, role = ld.key, ld.role
    if role not in INPUT_ROLES or not ld.records:
        return out
    pos, qs = ld.positions(), ld.quats()
    if pos is None or qs is None:
        return [RuleResult("input.keys", SKIP, key, "位姿字段异常", "需求§5.1")]
    keys = _keys_series(ld)
    n = len(pos)
    seen = sorted({k for ks in keys for k in ks})
    yp = [yaw_pitch_deg(q) for q in qs]
    yaw = unwrap_deg([a for a, _ in yp])
    pitch = [b for _, b in yp]
    yaw_rate = [(yaw[i + 1] - yaw[i]) / DT for i in range(n - 1)]
    pitch_rate = [(pitch[i + 1] - pitch[i]) / DT for i in range(n - 1)]
    dx = [float(r.get("mouse_dx") or 0.0) for r in ld.records]   # 接管前的帧不带键鼠字段
    dy = [float(r.get("mouse_dy") or 0.0) for r in ld.records]

    if role == "free_camera":
        # 本体系速度
        loc = []
        for i in range(n - 1):
            d = vsub(pos[i + 1], pos[i])
            R, U, F = basis(qs[i])
            loc.append((_dot(d, R) / DT, _dot(d, U) / DT, _dot(d, F) / DT))
        # E 上升、Q 下降：采集端 stage_take_win.cpp fly() 的「上 = E − Q」，交付说明的键鼠映射同此
        expect = {87: (2, +1, "W→前"), 83: (2, -1, "S→后"), 65: (0, -1, "A→左"), 68: (0, +1, "D→右"),
                  69: (1, +1, "E→升"), 81: (1, -1, "Q→降")}
        eps = 0.05
        best_lag, best_rate, per_key = 0, -1.0, {}
        for lag in range(0, 7):
            agree = tot = 0
            pk = {}
            for i in range(n - 1 - lag):
                for k in keys[i]:
                    if k not in expect:
                        continue
                    axis, sign, _ = expect[k]
                    v = loc[i + lag][axis]
                    ok = v * sign > eps
                    pk.setdefault(k, [0, 0])
                    pk[k][1] += 1
                    pk[k][0] += 1 if ok else 0
                    tot += 1
                    agree += 1 if ok else 0
            rate = agree / tot if tot else 0.0
            if rate > best_rate:
                best_rate, best_lag, per_key = rate, lag, pk
        used = [k for k in expect if k in seen]
        unused = [expect[k][2].split("→")[0] for k in expect if k not in seen]
        if not used:
            out.append(RuleResult("input.keys_drive_camera", FAIL, key, "全程无 WASD/QE 按键记录", "需求§5.1"))
        else:
            detail = "、".join(f"{expect[k][2]} {per_key[k][0]}/{per_key[k][1]}" for k in used if k in per_key)
            st = PASS if best_rate >= KEY_AGREE_MIN else FAIL
            out.append(RuleResult("input.keys_drive_camera", st, key,
                                  f"按键帧中相机本体系速度方向与键位一致 {best_rate * 100:.0f}%（滞后 {best_lag} 帧；{detail}）"
                                  + (f"；未出现的键：{'/'.join(unused)}" if unused else ""),
                                  "需求§5.1「WASD/QE 真实驱动相机」", data={"lag": best_lag, "agree": best_rate}))
        # 八方向：水平面上相机本体系的运动方向对键位方向
        rel_way = []
        for i in range(n - 1):
            vR, _, vF = loc[i]
            rel_way.append(math.degrees(math.atan2(vR, vF)) if math.hypot(vR, vF) > 0.1 else None)
        out.append(eight_directions(keys, yaw, rel_way, key, role))
        # Q/E 升降
        ys = [p[1] for p in pos]
        dy_range = max(ys) - min(ys)
        has_qe = bool(LIFT_KEYS & set(seen))
        if has_qe:
            st = PASS if dy_range > 0.05 else FAIL
            out.append(RuleResult("input.qe_lift", st, key, f"出现 Q/E 键，相机高度变化范围 {dy_range:.2f} m", "需求§4.2/§5.1"))
        else:
            out.append(RuleResult("input.qe_lift", FAIL, key,
                                  f"全程无 Q/E 键记录且相机高度恒定（范围 {dy_range:.3f} m）：升降通道未被使用，无法证明 Q/E 真实驱动相机"
                                  "（需方 demo 跟进表 demo4-#8 已反馈「未配置键盘、鼠标及 QE 映射」）",
                                  "需求§4.2/§5.1 / demo 跟进表"))
        # 左右是否都用到
        if 68 not in seen or 65 not in seen:
            out.append(RuleResult("input.key_coverage", WARN, key,
                                  f"出现键码 {seen}：WASD 中缺 {'/'.join(n_ for k_, n_ in ((65, 'A'), (68, 'D'), (83, 'S'), (87, 'W')) if k_ not in seen)}，"
                                  "六向平移未被真实输入全部触发", "需求§4.2"))
        # 鼠标 ↔ 视角
        r_yaw, lag_y = _best_lag(dx, yaw_rate)
        r_pitch, lag_p = _best_lag(dy, pitch_rate)
        nz = sum(1 for a, b in zip(dx, dy) if a or b)
        if nz < 10:
            out.append(RuleResult("input.mouse_drives_view", FAIL, key, f"鼠标位移非零帧仅 {nz}，无法证明鼠标真实驱动视角", "需求§5.1"))
        else:
            ok = r_yaw is not None and abs(r_yaw) >= MOUSE_CORR_MIN
            out.append(RuleResult("input.mouse_drives_view", PASS if ok else WARN, key,
                                  f"mouse_dx↔偏航速率 r={r_yaw if r_yaw is None else round(r_yaw, 2)}（滞后 {lag_y} 帧），"
                                  f"mouse_dy↔俯仰速率 r={r_pitch if r_pitch is None else round(r_pitch, 2)}（滞后 {lag_p} 帧）；非零帧 {nz}"
                                  + ("" if ok else "（相关性弱：真人输入有加速度曲线/平滑，或鼠标记录与视角不对应，需方 demo 跟进表 demo4-#8 反馈「鼠标映射错误」）"),
                                  "需求§5.1", data={"r_yaw": r_yaw, "r_pitch": r_pitch, "lag": lag_y}))
        return out

    # 人物受控路（追背 / 第一人称）
    pp = ld.player_positions()
    if pp is None:
        return [RuleResult("input.keys", SKIP, key, "人物字段异常", "需求§5.1")]
    pspeed = [vnorm(vsub(pp[i + 1], pp[i])) / DT for i in range(n - 1)]
    move_frames = [i for i in range(n - 1) if keys[i] & MOVE_KEYS]
    if not move_frames:
        out.append(RuleResult("input.keys_drive_player", FAIL, key, "全程无 WASD 按键记录", "需求§4.4/§4.5/§5.1"))
    else:
        best = (0.0, 0)
        for lag in range(0, 11):
            mv = sum(1 for i in move_frames if i + lag < n - 1 and pspeed[i + lag] > 0.3)
            rate = mv / len(move_frames)
            if rate > best[0]:
                best = (rate, lag)
        idle = [i for i in range(n - 1) if not (keys[i] & MOVE_KEYS)]
        idle_moving = sum(1 for i in idle if pspeed[i] > 0.3) / len(idle) if idle else 0.0
        st = PASS if best[0] >= KEY_AGREE_MIN else FAIL
        out.append(RuleResult("input.keys_drive_player", st, key,
                              f"WASD 按下帧中人物在动（>0.3 m/s）{best[0] * 100:.0f}%（滞后 {best[1]} 帧，按键帧 {len(move_frames)}）；"
                              f"无按键帧中人物在动 {idle_moving * 100:.0f}%（接管前基线动作/惯性属正常）",
                              "需求§5.1「WASD 真实驱动人物」", data={"agree": best[0], "lag": best[1]}))
    # 八方向：人物在水平面上走的方向（docs/69 基：x 右、z 前，偏航 = atan2(x, z)）对这一路镜头朝向 + 键位方向
    rel_way = []
    for i in range(n - 1):
        d = vsub(pp[i + 1], pp[i])
        if math.hypot(d[0], d[2]) / DT > 1.0:
            rel_way.append((math.degrees(math.atan2(d[0], d[2])) - yaw[i + 1] + 180.0) % 360.0 - 180.0)
        else:
            rel_way.append(None)
    out.append(eight_directions(keys, yaw, rel_way, key, role))
    # 鼠标 ↔ 这一路的转向：追背是游戏镜头本身；第一人称从 2026-10-07 起朝游戏镜头的方向看（需方 10-07：V4 的转向不跟鼠标），
    # 之前的包是出片程序在身体朝向 60° 内算的视线，这里会是弱相关
    if role in PERSON_ROLES:
        lane = "追背相机" if role == "chase" else "第一人称视线"
        r_yaw, lag_y = _best_lag(dx, yaw_rate)
        r_pitch, lag_p = _best_lag(dy, pitch_rate)
        nz = sum(1 for a, b in zip(dx, dy) if a or b)
        if nz < 10:
            out.append(RuleResult("input.mouse_drives_view", WARN, key, f"鼠标位移非零帧仅 {nz}", "需求§5.1"))
        else:
            ok = r_yaw is not None and abs(r_yaw) >= MOUSE_CORR_MIN
            out.append(RuleResult("input.mouse_drives_view", PASS if ok else WARN, key,
                                  f"mouse_dx↔{lane}偏航速率 r={r_yaw if r_yaw is None else round(r_yaw, 2)}（滞后 {lag_y} 帧），"
                                  f"mouse_dy↔俯仰速率 r={r_pitch if r_pitch is None else round(r_pitch, 2)}（滞后 {lag_p} 帧）；非零帧 {nz}",
                                  "需求§5.1", data={"r_yaw": r_yaw, "r_pitch": r_pitch, "lag": lag_y}))
    # 接管前：不带键鼠字段（需方 2026-09-30：键鼠只从接管时刻起记录，接管前不填写、不伪造，只记人物的基线动作）
    if takeover_frame is not None and role in PERSON_ROLES:
        upto = min(takeover_frame, n)
        filled = [i for i in range(upto) if carries_input(ld.records[i])]
        lead = unplayed_frames(ld.records, role)
        if filled:
            pre_keys = sum(1 for i in filled if keys[i])
            pre_mouse = sum(1 for i in filled if dx[i] or dy[i])
            out.append(RuleResult("input.pre_takeover", FAIL, key,
                                  f"接管帧 {takeover_frame} 之前有 {len(filled)} 帧带键鼠字段（其中按键帧 {pre_keys}、鼠标非零帧 {pre_mouse}）："
                                  "接管前只记人物的位置、朝向、速度和相机参数，不应填写键鼠", "需求§4.4、需方 2026-09-30"))
        elif lead > takeover_frame:
            out.append(RuleResult("input.pre_takeover", FAIL, key,
                                  f"接管帧 {takeover_frame} 起应带键鼠字段，实际到第 {lead} 帧才有", "需求§4.4、需方 2026-09-30"))
        else:
            out.append(RuleResult("input.pre_takeover", PASS, key,
                                  f"接管帧 {takeover_frame} 之前的 {upto} 帧不带键鼠字段，接管起每帧都带", "需求§4.4、需方 2026-09-30"))
    return out


# ---------------------------------------------------------------- 换算的键鼠（自由运镜）与全空的键鼠（固定机位）

PATH_KEYS = {87: (2, +1, "W→前"), 83: (2, -1, "S→后"), 65: (0, -1, "A→左"), 68: (0, +1, "D→右"),
             69: (1, +1, "E→升"), 81: (1, -1, "Q→降")}
PATH_MOVING_MPS = 0.30      # 某方向这么快还不记键，算漏记（与交付端按下门槛同：更慢的移动画面上看不出来，需方 2026-09-30）
PATH_KEY_AGREE_MIN = 0.95   # 按键帧里相机确实往那个方向动的比例（鼠标同号的比例同此）
PATH_TURN_DEG = 0.01        # 一帧转这么多（度）才算在转


def check_derived_input(ld: LaneData) -> List[RuleResult]:
    """自由运镜：键鼠由运镜路线逐帧换算（需方 2026-09-29），键位同世界线 B：W/S 沿相机水平朝向前后、A/D 左右、
    E/Q 升降，只表示方向；mouse_dx/dy = 这一帧的偏航/俯仰变化（向右、向下为正）。换算是逐帧同步的，不应有滞后。
    固定机位：全程无按键、鼠标为 0。"""
    key, role = ld.key, ld.role
    if not ld.records or (role not in PROGRAM_FREE_ROLES and role not in STILL_ROLES):
        return []
    rule = "input.fixed_none" if role in STILL_ROLES else "input.path_keys"
    lacking = sum(1 for r in ld.records if any(k not in r for k in ("keyCode", "mouse_dx", "mouse_dy")))
    if lacking:   # 缺字段不能当成「没按键」：每一路都要带键鼠（需方 2026-09-29）
        return [RuleResult(rule, FAIL, key, f"{lacking} 帧没有键鼠字段（keyCode / mouse_dx / mouse_dy）：需方要求每一路都带，"
                           + ("固定机位填无按键、鼠标 0" if role in STILL_ROLES else "自由运镜按运镜路线换算"),
                           "需求§5.4 / 需方 2026-09-29")]
    if role in STILL_ROLES:
        pressed = sum(1 for r in ld.records if r.get("keyCode"))
        moved = sum(1 for r in ld.records if r.get("mouse_dx") or r.get("mouse_dy"))
        return [RuleResult("input.fixed_none", PASS if not pressed and not moved else FAIL, key,
                           "固定机位全程无按键、鼠标为 0" if not pressed and not moved
                           else f"固定机位不动，却有 {pressed} 帧按键、{moved} 帧鼠标位移", "需求§5.4 / 需方 2026-09-29")]
    pos, qs = ld.positions(), ld.quats()
    if pos is None or qs is None:
        return [RuleResult("input.path_keys", SKIP, key, "位姿字段异常", "需求§5.4 / 需方 2026-09-29")]
    n = len(pos)
    keys = _keys_series(ld)
    level = []   # 每帧速度在相机水平系里的 (右, 上, 前)
    for i in range(n):
        a, b = max(0, i - 1), min(n - 1, i + 1)
        v = tuple((pos[b][k] - pos[a][k]) / (DT * max(1, b - a)) for k in range(3))
        R, _, F = basis(qs[i])
        fh, rh = (F[0], 0.0, F[2]), (R[0], 0.0, R[2])
        fn, rn = math.hypot(fh[0], fh[2]) or 1.0, math.hypot(rh[0], rh[2]) or 1.0
        level.append((_dot(v, rh) / rn, v[1], _dot(v, fh) / fn))
    held = agree = wrong_way = 0
    per_key: Dict[int, List[int]] = {}
    for i, ks in enumerate(keys):
        for k in ks:
            if k not in PATH_KEYS:
                continue
            axis, sign, _ = PATH_KEYS[k]
            v = level[i][axis] * sign
            held += 1
            agree += v > 0
            wrong_way += v < -PATH_MOVING_MPS / 2
            per_key.setdefault(k, [0, 0])
            per_key[k][0] += v > 0
            per_key[k][1] += 1
    missed = sum(1 for i in range(n) for k, (axis, sign, _) in PATH_KEYS.items()
                 if level[i][axis] * sign >= PATH_MOVING_MPS and k not in keys[i])
    speeds = sorted(vnorm(v) for v in level)
    moving = speeds[int(0.95 * (n - 1))] >= PATH_MOVING_MPS
    out: List[RuleResult] = []
    if not held:
        out.append(RuleResult("input.path_keys", FAIL if moving else PASS, key,
                              "自由运镜在动却全程没有换算出的按键（需方 2026-09-29 要求按运镜路线填入键鼠）" if moving
                              else "相机几乎不动，无按键", "需求§5.4 / 需方 2026-09-29"))
    else:
        rate = agree / held
        detail = "、".join(f"{PATH_KEYS[k][2]} {c[0]}/{c[1]}" for k, c in sorted(per_key.items()))
        ok = rate >= PATH_KEY_AGREE_MIN and not wrong_way and missed <= 0.02 * n
        out.append(RuleResult("input.path_keys", PASS if ok else FAIL, key,
                              f"按键帧中相机水平系速度方向与键位一致 {rate * 100:.1f}%（{detail}）；反向 {wrong_way} 帧；"
                              f"某方向 ≥ {PATH_MOVING_MPS:g} m/s 却没记键 {missed} 帧", "需求§5.4 / 需方 2026-09-29",
                              data={"agree": rate, "wrong_way": wrong_way, "missed": missed}))
    yp = [yaw_pitch_deg(q) for q in qs]
    yaw = unwrap_deg([a for a, _ in yp])
    pitch = [b for _, b in yp]

    def agrees(mouse: List[float], turn: List[float], sign: int) -> Tuple[float, int, int]:
        """鼠标与同帧转动的符号是否一致（不依赖换算比例，匀速转也能判）：相机转了（≥ PATH_TURN_DEG）的帧里
        mouse 与转动同号（sign=-1 表示反号）的比例，以及没转却有明显鼠标量的帧数。"""
        turned = [(m, t) for m, t in zip(mouse, turn) if abs(t) >= PATH_TURN_DEG]
        same = sum(1 for m, t in turned if m * t * sign > 0)
        phantom = sum(1 for m, t in zip(mouse, turn) if abs(m) >= 0.01 and abs(t) < PATH_TURN_DEG / 10)
        return (same / len(turned) if turned else 1.0), len(turned), phantom
    dx = [float(r["mouse_dx"]) for r in ld.records[1:]]
    dy = [float(r["mouse_dy"]) for r in ld.records[1:]]
    a_yaw, n_yaw, p_yaw = agrees(dx, [yaw[i + 1] - yaw[i] for i in range(n - 1)], +1)        # 向右为正
    a_pitch, n_pitch, p_pitch = agrees(dy, [pitch[i + 1] - pitch[i] for i in range(n - 1)], -1)   # 向下为正，俯仰向上为正
    ok = min(a_yaw, a_pitch) >= PATH_KEY_AGREE_MIN and p_yaw + p_pitch <= 0.01 * n
    out.append(RuleResult("input.path_mouse", PASS if ok else FAIL, key,
                          f"转向的 {n_yaw} 帧里 mouse_dx 与偏航同向（向右为正）{a_yaw * 100:.1f}%；"
                          f"俯仰的 {n_pitch} 帧里 mouse_dy 与低头同向（向下为正）{a_pitch * 100:.1f}%；"
                          f"没转却有鼠标量 {p_yaw + p_pitch} 帧", "需求§5.4 / 需方 2026-09-29",
                          data={"yaw_agree": a_yaw, "pitch_agree": a_pitch, "phantom": p_yaw + p_pitch}))
    return out


def check_motion(g: Group, lanes: Dict[str, LaneData], takeover_frames: Optional[Dict[str, int]] = None,
                 cfg: Optional[QCConfig] = None) -> List[RuleResult]:
    """组级入口。takeover_frames: {worldline: 接管视频帧号}（来自 README，可无）。"""
    cfg = cfg or QCConfig()
    out: List[RuleResult] = []
    for lf in g.lanes:
        ld = lanes.get(lf.info.key)
        if not ld:
            continue
        out.extend(check_lane_motion(ld, cfg))
        tf = (takeover_frames or {}).get(lf.info.worldline)
        out.extend(check_input_consistency(ld, tf))
        out.extend(check_derived_input(ld))
    out.extend(check_a_independence(g, lanes, cfg))
    return out

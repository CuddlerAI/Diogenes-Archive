"""action_camera 逐帧字段面：字段契约、时间、帧号、四元数/欧拉角一致、内参/焦距、速度自洽、输入范围、
人物两路一致、固定机位、Follow Offset 语义、第一人称/追背几何。

需求出处：新需求 §5.1、§5.3、§5.4、§6.2、§7.1；交付 README「字段口径」。
"""

from __future__ import annotations

import math
import re
from datetime import datetime
from typing import Dict, List, Optional, Sequence, Tuple

from engine_qc.mathutil import quat_angle_deg, quat_norm, vnorm, vsub

from .loaders import Group, LaneFiles, load_json
from .model import (FAIL, MANUAL, PASS, PERSON_ROLES, SKIP, WARN, RuleResult)

# 键名与帧号按交付包（stage_delivery，字段名按需方样例 demo4 #8：camera_follow_offset、mouseX/mouseY，
# frame 从 1 起、第 1 帧 = 舞台第 0 秒）统一（2026-09-29）；metric_scale 每一路都有。
CAMERA_KEYS = ("time", "fps", "frame", "camera_position", "camera_rotation_oula", "camera_rotation_quaternion",
               "camera_follow_offset", "camera_intrinsics", "camera_speed", "metric_scale")
PLAYER_KEYS = ("player_position", "player_rotation_oula", "player_rotation_quaternion", "player_speed")
INPUT_KEYS = ("mouseX", "mouseY", "mouse_dx", "mouse_dy", "keyCode")
# 与交付包不同的写法：需求原文的 camera_Follow Offset / mouse_x / mouse_y，以及别处见过的错写
WRONG_KEYS = ("camera_Follow Offset", "mouse_x", "mouse_y", "camera_rotation_euler", "keycode", "key_code")

VK_NAMES = {1: "鼠标左键", 2: "鼠标右键", 4: "鼠标中键", 8: "Backspace", 9: "Tab", 13: "Enter", 16: "Shift", 17: "Ctrl",
            18: "Alt", 27: "Esc", 32: "Space", 65: "A", 68: "D", 69: "E", 70: "F", 81: "Q", 82: "R", 83: "S", 87: "W",
            160: "LShift", 161: "RShift", 162: "LCtrl", 163: "RCtrl", 164: "LAlt", 165: "RAlt"}
MOVE_KEYS = {87, 65, 83, 68}
LIFT_KEYS = {81, 69}
DT = 1.0 / 30.0
FILMBACK_MM = 36.0     # fx = W/2 / tan(hfov/2)；焦距(mm) = fx × 36 / W（全画幅等效）
FOCAL_STOPS = (18.0, 24.0, 35.0, 50.0)
TIME_RE = re.compile(r"^\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}\.\d{3}$")


def expected_keys(role: str) -> Tuple[str, ...]:
    """每路的键：相机字段 + 受控路的人物字段 + 键鼠字段（每一路都带，需方 2026-09-29：自由运镜按运镜路线换算、
    固定机位全空；真实输入只在 INPUT_ROLES）。受控人物路在接管前的帧不带键鼠字段，见 unplayed_frames。"""
    return CAMERA_KEYS + (PLAYER_KEYS if role in PERSON_ROLES else ()) + INPUT_KEYS


def carries_input(r: dict) -> bool:
    """这一帧带不带键鼠字段。"""
    return any(k in r for k in INPUT_KEYS)


def unplayed_frames(records: Sequence[dict], role: str) -> int:
    """受控人物路开头连续不带键鼠字段的帧数：接管前没有人操控，只记人物的位置、朝向、速度和相机，
    不填写键鼠（需方 2026-09-30）。其余各路恒为 0。"""
    if role not in PERSON_ROLES:
        return 0
    lead = 0
    for r in records:
        if carries_input(r):
            break
        lead += 1
    return lead


def unity_euler_deg(q: Sequence[float]) -> List[float]:
    """Unity Quaternion.eulerAngles（Z→X→Y），[X,Y,Z] 度，[0,360)。与交付侧 stage_delivery._unity_euler_deg 同式。"""
    x, y, z, w = (float(v) for v in q)
    angles = (math.asin(max(-1.0, min(1.0, 2.0 * (w * x - y * z)))),
              math.atan2(2.0 * (x * z + w * y), 1.0 - 2.0 * (x * x + y * y)),
              math.atan2(2.0 * (x * y + w * z), 1.0 - 2.0 * (x * x + z * z)))
    out = []
    for a in angles:
        d = math.degrees(a) % 360.0
        out.append(0.0 if d >= 360.0 else d)
    return out


def _ang_diff(a: float, b: float) -> float:
    return abs((a - b + 180.0) % 360.0 - 180.0)


def focal_from_fx(fx: float, width: int) -> float:
    return fx * FILMBACK_MM / width


def fov_deg(f: float, size: int) -> float:
    return math.degrees(2.0 * math.atan((size / 2.0) / f))


def parse_time(s: str) -> Optional[datetime]:
    try:
        return datetime.strptime(s, "%Y-%m-%d %H:%M:%S.%f")
    except (ValueError, TypeError):
        return None


def _is_vec(v, n: int) -> bool:
    return (isinstance(v, list) and len(v) == n
            and all(isinstance(x, (int, float)) and not isinstance(x, bool) and math.isfinite(x) for x in v))


class LaneData:
    """一路的 action_camera 记录及派生量。装载失败时 records=None。"""

    def __init__(self, lf: LaneFiles):
        self.lf = lf
        self.key = lf.info.key
        self.role = lf.info.role
        self.records: Optional[list] = None
        self.error: Optional[str] = None
        if lf.action_camera is None:
            self.error = "action_camera 缺失"
            return
        obj, err = load_json(lf.action_camera)
        if err:
            self.error = err
        elif not isinstance(obj, list) or not obj or not all(isinstance(r, dict) for r in obj):
            self.error = "顶层不是非空对象数组"
        else:
            self.records = obj

    @property
    def n(self) -> int:
        return len(self.records) if self.records else 0

    def positions(self) -> Optional[List[Tuple[float, float, float]]]:
        if not self.records:
            return None
        out = []
        for r in self.records:
            p = r.get("camera_position")
            if not _is_vec(p, 3):
                return None
            out.append(tuple(float(x) for x in p))
        return out

    def quats(self) -> Optional[List[Tuple[float, float, float, float]]]:
        if not self.records:
            return None
        out = []
        for r in self.records:
            q = r.get("camera_rotation_quaternion")
            if not _is_vec(q, 4):
                return None
            out.append(tuple(float(x) for x in q))
        return out

    def player_positions(self) -> Optional[List[Tuple[float, float, float]]]:
        if not self.records or self.role not in PERSON_ROLES:
            return None
        out = []
        for r in self.records:
            p = r.get("player_position")
            if not _is_vec(p, 3):
                return None
            out.append(tuple(float(x) for x in p))
        return out

    def intrinsics(self) -> Optional[dict]:
        if not self.records:
            return None
        i = self.records[0].get("camera_intrinsics")
        return i if isinstance(i, dict) else None


def check_lane_fields(ld: LaneData) -> List[RuleResult]:
    out: List[RuleResult] = []
    key, role = ld.key, ld.role
    if ld.records is None:
        out.append(RuleResult("json.container", FAIL if ld.error != "action_camera 缺失" else SKIP, key,
                              f"action_camera 无法装载：{ld.error}", "需求§5.4"))
        return out
    out.append(RuleResult("json.container", PASS, key, f"顶层为 {ld.n} 条逐帧记录的数组", "需求§5.4"))

    want = set(expected_keys(role))
    lead = unplayed_frames(ld.records, role)
    unplayed = want - set(INPUT_KEYS)
    bad_frames = []
    wrong = set()
    for i, r in enumerate(ld.records):
        ks = set(r)
        if ks != (unplayed if i < lead else want):
            bad_frames.append((r.get("frame"), sorted(ks ^ (unplayed if i < lead else want))))
        wrong |= ks & set(WRONG_KEYS)
    if wrong:
        out.append(RuleResult("json.keys", FAIL, key, f"出现错写键名 {sorted(wrong)}（应为 camera_follow_offset / mouseX / mouseY 等，按需方样例）", "需求§5.4"))
    if bad_frames:
        f0, diff = bad_frames[0]
        out.append(RuleResult("json.schema", FAIL, key,
                              f"{len(bad_frames)} 帧字段集合与 {role} 路契约不符，首例帧 {f0}：差集 {diff}", "需求§5.4"))
    else:
        out.append(RuleResult("json.schema", PASS, key,
                              f"每帧字段恰为 {role} 路契约（{len(want)} 个键：相机字段"
                              + ("+人物字段" if role in PERSON_ROLES else "") + "+键鼠字段）"
                              + (f"；开头 {lead} 帧是接管前，只有相机和人物字段、不带键鼠字段" if lead else ""),
                              "需求§5.4"))

    # 类型/数值
    type_bad = 0
    for r in ld.records:
        ok = (_is_vec(r.get("camera_position"), 3) and _is_vec(r.get("camera_rotation_oula"), 3)
              and _is_vec(r.get("camera_rotation_quaternion"), 4) and _is_vec(r.get("camera_follow_offset"), 3)
              and _is_vec(r.get("camera_speed"), 3) and isinstance(r.get("camera_intrinsics"), dict)
              and all(isinstance(r["camera_intrinsics"].get(k), (int, float)) for k in ("fx", "fy", "cx", "cy"))
              and isinstance(r.get("frame"), int) and not isinstance(r.get("frame"), bool)
              and isinstance(r.get("fps"), (int, float)) and isinstance(r.get("time"), str))
        if role in PERSON_ROLES:
            ok = ok and _is_vec(r.get("player_position"), 3) and _is_vec(r.get("player_rotation_oula"), 3) \
                and _is_vec(r.get("player_rotation_quaternion"), 4) and _is_vec(r.get("player_speed"), 3)
        ok = ok and isinstance(r.get("metric_scale"), (int, float))
        if carries_input(r):
            ok = ok and isinstance(r.get("keyCode"), list) and all(isinstance(k, int) and not isinstance(k, bool) for k in r["keyCode"]) \
                and all(isinstance(r.get(k), (int, float)) for k in ("mouseX", "mouseY", "mouse_dx", "mouse_dy"))
        if not ok:
            type_bad += 1
    if type_bad:
        out.append(RuleResult("json.types", FAIL, key, f"{type_bad} 帧存在类型/维度错误（Vector3/Vector4/int/float）", "需求§5.4"))
        return out
    out.append(RuleResult("json.types", PASS, key, "全部字段类型与维度正确", "需求§5.4"))

    # 帧号连续
    frames = [r["frame"] for r in ld.records]
    if frames == list(range(1, len(frames) + 1)):
        out.append(RuleResult("json.frame_continuity", PASS, key, f"frame 1…{len(frames)} 连续，无重复无跳帧", "需求§5.4/§6.2"))
    else:
        dup = len(frames) - len(set(frames))
        out.append(RuleResult("json.frame_continuity", FAIL, key, f"frame 不是 1…N 连续序列（重复 {dup}，首值 {frames[0]}，末值 {frames[-1]}）", "需求§5.4/§6.2"))

    # time 格式与步长
    times = [parse_time(r["time"]) for r in ld.records]
    if any(t is None for t in times) or any(not TIME_RE.match(r["time"]) for r in ld.records):
        out.append(RuleResult("json.time_format", FAIL, key, "time 不是 YYYY-MM-DD HH:mm:ss.SSS", "需求§5.4"))
    else:
        steps = [(b - a).total_seconds() for a, b in zip(times, times[1:])]
        mono = all(s > 0 for s in steps)
        worst = max(abs(s - DT) for s in steps) if steps else 0.0
        if mono and worst <= 0.002:
            out.append(RuleResult("json.time_format", PASS, key, f"time 格式正确、严格递增，步长偏差 ≤{worst * 1000:.1f} ms（30fps）", "需求§5.4"))
        else:
            out.append(RuleResult("json.time_format", FAIL if not mono else WARN, key,
                                  f"time {'非严格递增' if not mono else '步长偏差 ' + f'{worst * 1000:.1f} ms'}", "需求§5.4"))

    # 游戏帧率 ≥30
    fps = [float(r["fps"]) for r in ld.records]
    lo = min(fps)
    if lo >= 30.0:
        out.append(RuleResult("json.game_fps", PASS, key, f"游戏运行帧率 min {lo:.1f} / max {max(fps):.1f}（≥30）", "需求§3.2/§6.2"))
    else:
        below = sum(1 for f in fps if f < 30.0)
        out.append(RuleResult("json.game_fps", FAIL, key, f"游戏运行帧率最低 {lo:.1f}，{below} 帧低于 30fps", "需求§3.2/§6.2"))
    if len(set(fps)) == 1:
        out.append(RuleResult("json.game_fps_measured", WARN, key, f"fps 全程恒为 {fps[0]:g}：疑似固定值而非实测（需求：记录实际运行 fps）", "需求§5.4"))

    # 四元数单位化 + 与欧拉角一致
    qs = ld.quats()
    bad_norm = sum(1 for q in qs if abs(quat_norm(q) - 1.0) > 1e-3)
    if bad_norm:
        out.append(RuleResult("json.quat_norm", FAIL, key, f"{bad_norm} 帧相机四元数非单位（|q|−1 > 1e-3）", "需求§5.4"))
    else:
        out.append(RuleResult("json.quat_norm", PASS, key, "相机四元数全部为单位四元数", "需求§5.4"))
    worst_e = 0.0
    for r, q in zip(ld.records, qs):
        e = unity_euler_deg(q)
        d = max(_ang_diff(a, float(b)) for a, b in zip(e, r["camera_rotation_oula"]))
        worst_e = max(worst_e, d)
    if worst_e <= 0.05:
        out.append(RuleResult("json.euler_quat", PASS, key,
                              f"camera_rotation_oula 与四元数按 Unity eulerAngles（[x,y,z,w]，Z→X→Y）逐帧一致，最大偏差 {worst_e:.3f}°",
                              "需求§5.4 / docs/72"))
    else:
        out.append(RuleResult("json.euler_quat", FAIL, key,
                              f"欧拉角与四元数不一致（最大偏差 {worst_e:.2f}°）：顺规或四元数分量顺序 [x,y,z,w] 有误", "需求§5.4"))
    if role in PERSON_ROLES:
        worst_p = 0.0
        for r in ld.records:
            e = unity_euler_deg(r["player_rotation_quaternion"])
            worst_p = max(worst_p, max(_ang_diff(a, float(b)) for a, b in zip(e, r["player_rotation_oula"])))
        out.append(RuleResult("json.player_euler_quat", PASS if worst_p <= 0.05 else FAIL, key,
                              f"player_rotation_oula 与人物四元数最大偏差 {worst_p:.3f}°", "需求§5.4"))

    # 内参
    intr = [r["camera_intrinsics"] for r in ld.records]
    fxfy_bad = sum(1 for i in intr if abs(i["fx"] - i["fy"]) > 1e-6 or i["fx"] <= 0)
    const = all(i == intr[0] for i in intr)
    if fxfy_bad:
        out.append(RuleResult("json.fx_fy", FAIL, key, f"{fxfy_bad} 帧 fx≠fy", "需求§5.4/§6.2"))
    else:
        out.append(RuleResult("json.fx_fy", PASS, key, f"fx=fy={intr[0]['fx']:.3f}px，cx/cy=({intr[0]['cx']:g},{intr[0]['cy']:g})", "需求§5.4/§6.2"))
    if const:
        out.append(RuleResult("json.fov_fixed", PASS, key, "内参全程恒定（单条视频内 FOV 固定）", "需求§3.2"))
    else:
        out.append(RuleResult("json.fov_fixed", FAIL, key, "内参在片内变化（禁止变焦/缩放）", "需求§3.2"))

    # camera_speed 自洽（与位置差分）
    pos = ld.positions()
    worst_v = 0.0
    for i in range(1, len(pos)):
        fd = tuple((pos[i][k] - pos[i - 1][k]) / DT for k in range(3))
        sp = ld.records[i]["camera_speed"]
        worst_v = max(worst_v, vnorm(vsub(fd, sp)))
    # 允许交付侧用中心差分/首帧 0 的写法：用 1 m/s 作为「不是默认值」的判据
    if worst_v <= 1.0:
        out.append(RuleResult("json.camera_speed", PASS, key, f"camera_speed 与位置差分一致（最大偏差 {worst_v:.3f} m/s）", "需求§5.1/§5.4"))
    else:
        out.append(RuleResult("json.camera_speed", WARN, key, f"camera_speed 与位置差分最大偏差 {worst_v:.2f} m/s（差分口径不同或非实测值）", "需求§5.1/§5.4"))
    if role in PERSON_ROLES:
        pp = ld.player_positions()
        worst_pv = 0.0
        for i in range(1, len(pp)):
            fd = tuple((pp[i][k] - pp[i - 1][k]) / DT for k in range(3))
            worst_pv = max(worst_pv, vnorm(vsub(fd, ld.records[i]["player_speed"])))
        out.append(RuleResult("json.player_speed", PASS if worst_pv <= 1.0 else WARN, key,
                              f"player_speed 与人物位移差分最大偏差 {worst_pv:.3f} m/s", "需求§5.4"))

    # 键鼠字段范围（每一路都带）
    rng_bad = sum(1 for r in ld.records if carries_input(r) and not (0 <= r["mouseX"] <= 1 and 0 <= r["mouseY"] <= 1
                                                                     and -1 <= r["mouse_dx"] <= 1 and -1 <= r["mouse_dy"] <= 1))
    out.append(RuleResult("json.mouse_range", PASS if not rng_bad else FAIL, key,
                          f"mouseX/Y∈[0,1]、mouse_dx/dy∈[-1,1]" + (f"，{rng_bad} 帧越界" if rng_bad else ""), "需求§5.4"))
    ms = {r["metric_scale"] for r in ld.records}
    out.append(RuleResult("json.metric_scale", PASS if ms == {1.0} else WARN, key, f"metric_scale={sorted(ms)}", "需求§5.4"))
    keys_seen = sorted({k for r in ld.records for k in r.get("keyCode") or []})
    combos = sum(1 for r in ld.records if len(r.get("keyCode") or []) >= 2)
    names = ", ".join(f"{k}={VK_NAMES.get(k, '?')}" for k in keys_seen)
    out.append(RuleResult("json.keycodes", PASS, key,
                          f"出现键码 [{names}]；组合键帧 {combos}（组合键按同帧叠加、不求和）", "需求§5.1", data={"keys": keys_seen}))
    bad_codes = [k for k in keys_seen if k <= 0 or k > 254]
    if bad_codes:
        out.append(RuleResult("json.keycodes", FAIL, key, f"非法键码 {bad_codes}（应为 Windows 虚拟键码）", "需求§5.1"))
    return out


def check_take_sync(g: Group, lanes: Dict[str, LaneData]) -> List[RuleResult]:
    """同一世界线内各路：帧数相同、time 逐帧相同（偏差 <100ms）、人物两路逐帧一致。"""
    out: List[RuleResult] = []
    for wl in "ACD":
        lds = [lanes[lf.info.key] for lf in g.lanes_of(wl) if lanes.get(lf.info.key) and lanes[lf.info.key].records]
        if len(lds) < 2:
            out.append(RuleResult("sync.frames", SKIP, f"世界线{wl}", "可比路数不足", "需求§3.2/§7.1"))
            continue
        ns = {ld.n for ld in lds}
        if len(ns) != 1:
            out.append(RuleResult("sync.frames", FAIL, f"世界线{wl}", f"各路记录数不同：{[ld.n for ld in lds]}", "需求§3.2/§7.1"))
            continue
        out.append(RuleResult("sync.frames", PASS, f"世界线{wl}", f"{len(lds)} 路记录数一致（{lds[0].n}）", "需求§3.2/§7.1"))
        worst = 0.0
        ref = [parse_time(r["time"]) for r in lds[0].records]
        for ld in lds[1:]:
            for a, r in zip(ref, ld.records):
                b = parse_time(r["time"])
                if a and b:
                    worst = max(worst, abs((b - a).total_seconds()))
        if worst < 0.1:
            out.append(RuleResult("sync.time", PASS, f"世界线{wl}", f"各路同帧 time 最大偏差 {worst * 1000:.1f} ms（<100 ms）", "需求§5.4/§7.1"))
        else:
            out.append(RuleResult("sync.time", FAIL, f"世界线{wl}", f"各路同帧 time 最大偏差 {worst * 1000:.0f} ms（≥100 ms）", "需求§5.4/§7.1"))
        person = [ld for ld in lds if ld.role in PERSON_ROLES]
        if len(person) == 2:
            diff = 0
            for a, b in zip(person[0].records, person[1].records):
                if any(a.get(k) != b.get(k) for k in PLAYER_KEYS + INPUT_KEYS):
                    diff += 1
            if diff == 0:
                out.append(RuleResult("sync.person_lanes", PASS, f"世界线{wl}",
                                      f"追背路与第一人称路的人物状态与输入逐帧完全一致（同一人物、同一次操作）", "需求§5.1/§5.4"))
            else:
                out.append(RuleResult("sync.person_lanes", FAIL, f"世界线{wl}", f"追背路与第一人称路有 {diff} 帧人物/输入不一致", "需求§5.1/§5.4"))
        elif wl in "CD":
            out.append(RuleResult("sync.person_lanes", FAIL, f"世界线{wl}", f"应有追背+第一人称两条人物受控路，实有 {len(person)}", "需求§4.4/§4.5"))
    # B 与 A：B 是 AB 同一遍的第 4 路 → 逐帧同 time
    a = [lanes[lf.info.key] for lf in g.lanes_of("A") if lanes.get(lf.info.key) and lanes[lf.info.key].records]
    b = [lanes[lf.info.key] for lf in g.lanes_of("B") if lanes.get(lf.info.key) and lanes[lf.info.key].records]
    if a and b:
        same_take = a[0].lf.info.take == b[0].lf.info.take
        same_t = a[0].n == b[0].n and all(x["time"] == y["time"] for x, y in zip(a[0].records, b[0].records))
        if same_take and same_t:
            out.append(RuleResult("sync.b_replays_a", PASS, "世界线B",
                                  "B 与 A 出自同一遍录制/同一份回放（同文件夹、逐帧同 time）：B 逐帧复现 A 的基线构造性成立", "需求§4.2/§4.3"))
        else:
            out.append(RuleResult("sync.b_replays_a", MANUAL, "世界线B",
                                  "B 与 A 不是同一遍录制；B 是否逐帧复现 A 的基线需按逻辑帧比对世界状态（本包无世界遥测，需人工/上游证据）", "需求§4.2/§4.3"))
    return out


def check_camera_semantics(g: Group, lanes: Dict[str, LaneData], video_dims: Dict[str, Tuple[int, int]]) -> List[RuleResult]:
    """固定机位不动、焦距档位与 FOV、Follow Offset 语义、第一人称贴头、追背距离。"""
    out: List[RuleResult] = []
    for lf in g.lanes:
        ld = lanes.get(lf.info.key)
        if not ld or not ld.records:
            continue
        key, role = ld.key, ld.role
        pos, qs = ld.positions(), ld.quats()
        dims = video_dims.get(key)
        intr = ld.intrinsics()
        # 焦距档位
        if dims and intr:
            f_mm = focal_from_fx(float(intr["fx"]), dims[0])
            near = min(FOCAL_STOPS, key=lambda s: abs(s - f_mm))
            hf, vf = fov_deg(float(intr["fx"]), dims[0]), fov_deg(float(intr["fy"]), dims[1])
            declared = lf.info.focal_mm
            txt = f"fx={intr['fx']:.1f}px → 等效焦距 {f_mm:.2f} mm（档位 {near:g} mm），HFOV {hf:.1f}° / VFOV {vf:.1f}°"
            if declared is not None:
                txt += f"；README 声明 {declared:g} mm"
            ok = abs(f_mm - near) <= 0.5 and (declared is None or abs(declared - near) < 0.5)
            out.append(RuleResult("cam.focal", PASS if ok else FAIL, key, txt, "需求§3.2",
                                  data={"focal_mm": f_mm, "hfov": hf, "vfov": vf}))
            lf.info.focal_mm = near if ok else f_mm
            if role == "chase":
                # 09-22 会议：三期第三人称跟随视角 FOV 范围 30–55（水平/垂直口径未明）
                inside_v, inside_h = 30 <= vf <= 55, 30 <= hf <= 55
                st = PASS if inside_v or inside_h else WARN
                out.append(RuleResult("cam.chase_fov", st, key,
                                      f"追背 FOV：垂直 {vf:.1f}° / 水平 {hf:.1f}°，会议要求 30–55（未说明按水平还是垂直；"
                                      + ("垂直在范围内" if inside_v else "均不在范围内") + "）", "09-22 会议纪要"))
            if intr.get("cx") is not None and (abs(float(intr["cx"]) - dims[0] / 2) > 1 or abs(float(intr["cy"]) - dims[1] / 2) > 1):
                out.append(RuleResult("cam.principal_point", FAIL, key, f"cx/cy=({intr['cx']},{intr['cy']}) 不是画面中心", "需求§5.4"))
        # 固定机位
        if role == "fixed" and pos and qs:
            dpos = max(vnorm(vsub(p, pos[0])) for p in pos)
            drot = max(quat_angle_deg(qs[0], q) for q in qs)
            total_turn = sum(quat_angle_deg(a, b) for a, b in zip(qs, qs[1:]))
            if dpos <= 1e-3:
                st = PASS
                txt = f"位置全程不变（最大位移 {dpos * 1000:.2f} mm）；朝向最大偏离 {drot:.2f}°，累计转动 {total_turn:.2f}°"
                if total_turn > 5:
                    st = WARN
                    txt += "（尽量保持朝向静止：转动偏多，请确认为平滑转向）"
            else:
                st, txt = FAIL, f"固定机位位置变化 {dpos:.3f} m（应 0–120 s 位置不变，只允许平滑转向）"
            out.append(RuleResult("cam.fixed_static", st, key, txt, "需求§3.4/§7.1"))
        # Follow Offset 语义
        fo = [r["camera_follow_offset"] for r in ld.records]
        if role in PERSON_ROLES and pos:
            pp = ld.player_positions()
            worst = max(vnorm(vsub(vsub(c, p), f)) for c, p, f in zip(pos, pp, fo))
            out.append(RuleResult("cam.follow_offset", PASS if worst <= 1e-3 else FAIL, key,
                                  f"camera_follow_offset = 相机−人物（最大偏差 {worst:.4f} m）", "需求§5.4 / README 字段口径"))
            dist = [vnorm(f) for f in fo]
            if role == "first_person":
                mx = max(dist)
                out.append(RuleResult("cam.first_person_head", PASS if mx <= 1.2 else FAIL, key,
                                      f"第一人称相机距人物根位置 {min(dist):.2f}–{mx:.2f} m（应贴头，≤1.2 m）", "需求§3.4/§4.4"))
            else:
                mn, mx = min(dist), max(dist)
                fp_frames = sum(1 for d in dist if d < 1.2)
                st = PASS if fp_frames == 0 else FAIL
                out.append(RuleResult("cam.chase_distance", st, key,
                                      f"追背相机距人物 {mn:.2f}–{mx:.2f} m；贴头(<1.2 m)帧 {fp_frames}"
                                      + ("" if fp_frames == 0 else "（游戏镜头切成了第一人称，违反「不得中途切换视角」）"), "需求§3.3/§3.4",
                                      data={"dist": dist}))
        elif role in ("fixed", "orbit", "dolly", "free") and pos:
            targets = [tuple(c[i] - f[i] for i in range(3)) for c, f in zip(pos, fo)]
            spread = max(vnorm(vsub(t, targets[0])) for t in targets)
            out.append(RuleResult("cam.follow_offset", PASS if spread <= 0.5 else WARN, key,
                                  f"camera_follow_offset = 相机−注视点；反推注视点漂移 {spread:.3f} m"
                                  + ("（恒定舞台中心）" if spread <= 0.5 else "（注视点在动：若为跟随主体属正常，否则口径待确认）"),
                                  "需求§5.4 / README 字段口径", data={"target": list(targets[0])}))
        elif role == "free_camera":
            allzero = all(vnorm(f) == 0 for f in fo)
            out.append(RuleResult("cam.follow_offset", WARN if allzero else MANUAL, key,
                                  "B 自由相机无跟随目标，Follow Offset 填 0（需求：无目标时的填法通过交付样例确认，待需方确认）", "需求§5.4"))
    return out

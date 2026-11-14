"""multicam_sync_i.json（交付说明 v1.1，2026-10-16）：每路视频下的世界，逐帧。

需方在交付说明「json 书写关注重点」里列了他们怎么验，这里逐条照做：

- msync.fields          头字段、逐帧字段齐全；编号 i 与 camera_id / video_file 一致；video_frame 从 0 连续，条数 = 本路
                        action_camera 条数（同一解码帧一一对应）
- msync.entities        tracked_entity_ids 里每个单位每帧都出现、exists 是布尔；exists=false 时 position/rotation/animation 全 null；
                        exists=true 时 position 三个数、rotation 单位四元数
- msync.group_header    12 路的 schema_version、capture_group_id、fps、time_unit、coordinate_system、tracked_entity_ids 一致
- msync.time            sample_time 从 0 起（组内统一原点）、不倒退
- msync.frames          各路帧数相同、logical_frame 逐帧相同
- msync.in_take         同一遍（A&B、C、D 各一组）四路：相同 video_frame 下 logical_frame 与全部动态物状态一致
- msync.takeover        C 四路都有接管、接管帧相同且是整数秒（N = video_frame / fps）；A、B、D 填 null
- msync.controlled      C 的受控人物 12 路全帧 exists=true；D 的受控人物 9–12 路全帧 exists=true、1–8 路全程 false
- msync.cross_takeover  接管秒 N 前 1–8 路动态物一致（拿 1 路比 5 路）
- msync.cross_no_event  D 没有事件：除两名受控人物外，全部动态物全程与 A 一致（拿 1 路比 9 路）
- msync.lighting        有太阳却填 null：提示；填了就每帧都要有 sun_direction（朝下的单位向量）和 intensity（正数），缺帧或格式不对判 FAIL

A、C、D 是分开录的三遍，同一拍的位置不会逐位相同：跨世界线两条记最大偏差，在 CROSS_TOLERANCE（单轴 1 mm / 0.02°，按需方 10-19 的判定）以内给 WARN；sample_time 不同直接 FAIL
（需方写的是「全部一致」，数值误差收不收待需方确认），超出给 FAIL。
"""

from __future__ import annotations

import math
from typing import Dict, List, Optional, Tuple

from .loaders import Group, load_json
from .model import FAIL, PASS, SKIP, UNVERIFIED, WARN, RuleResult

REQ = "交付说明 v1.1「json 书写关注重点」"
HEADER = ("schema_version", "capture_group_id", "camera_id", "video_file", "fps", "coordinate_system", "time_unit",
          "capture_note", "tracked_entity_ids", "takeover", "frames")
FRAME_KEYS = ("video_frame", "logical_frame", "sample_time", "entities", "lighting")
ENTITY_KEYS = ("id", "exists", "position", "rotation", "animation")
SAME_TAKE = {"A/B": (1, 2, 3, 4), "C": (5, 6, 7, 8), "D": (9, 10, 11, 12)}
# 分开录的两遍：位置每个轴 1 mm、朝向 0.02° 以内算数值误差。按需方 2026-10-19 质检 113 的实际判定反推：位置单轴 1.22 mm
# 报不一致、0.98 mm 不报；朝向 0.11° 报、手机 0.018° 不报（门槛在两者之间，取严的一头）。以前是 1 cm / 1°，
# 113 在我们这里只给 WARN，需方判了不通过。
CROSS_TOLERANCE_M, CROSS_TOLERANCE_DEG = 0.001, 0.02


def _load(g: Group) -> Tuple[Dict[int, dict], Dict[int, str], Dict[int, int]]:
    """各路（需方 1～12 编号）的 multicam_sync、读不了的原因、文件名里的编号（「遍」布局下是遍内 1–4）。"""
    docs, errors, named = {}, {}, {}
    for lf in g.lanes:
        i = lf.info.global_index
        if i is None or lf.multicam_sync is None:
            continue
        named[i] = int(lf.multicam_sync.stem.rsplit("_", 1)[1])
        obj, err = load_json(lf.multicam_sync)
        if err or not isinstance(obj, dict):
            errors[i] = err or "顶层不是对象"
        else:
            docs[i] = obj
    return docs, errors, named


def _lane_counts(g: Group) -> Dict[int, Optional[int]]:
    """各路 action_camera 的条数（multicam_sync 须与它逐帧对应）。"""
    out = {}
    for lf in g.lanes:
        if lf.info.global_index is None or lf.action_camera is None:
            continue
        obj, err = load_json(lf.action_camera)
        out[lf.info.global_index] = len(obj) if not err and isinstance(obj, list) else None
    return out


def _fields(i: int, doc: dict, want_frames: Optional[int]) -> List[str]:
    """i：文件名里的编号（camera_id / video_file 须与它一致）。"""
    bad = [f"缺头字段 {k}" for k in HEADER if k not in doc]
    if bad:
        return bad
    if doc["schema_version"] != "1.0":
        bad.append(f"schema_version={doc['schema_version']!r}（本版应为 \"1.0\"）")
    if (doc["camera_id"], doc["video_file"]) != (f"video_{i}", f"video_{i}.mp4"):
        bad.append(f"camera_id/video_file 为 {doc['camera_id']}/{doc['video_file']}，与编号 {i} 不一致")
    if doc["time_unit"] != "second":
        bad.append(f"time_unit={doc['time_unit']!r}（应为 second）")
    frames = doc["frames"]
    if not isinstance(frames, list) or not frames:
        return bad + ["frames 不是非空数组"]
    missing = sorted({k for f in frames for k in FRAME_KEYS if k not in f})
    if missing:
        bad.append(f"逐帧缺字段 {'、'.join(missing)}")
    elif [f["video_frame"] for f in frames] != list(range(len(frames))):
        bad.append("video_frame 不是从 0 连续编号")
    if want_frames is not None and len(frames) != want_frames:
        bad.append(f"{len(frames)} 帧，本路 action_camera {want_frames} 条：不是一一对应")
    return bad


def _entities(doc: dict) -> List[str]:
    ids = doc["tracked_entity_ids"]
    bad = []
    for f in doc["frames"]:
        got = [e.get("id") for e in f.get("entities") or []]
        if sorted(got) != sorted(ids):
            bad.append(f"第 {f['video_frame']} 帧的单位与 tracked_entity_ids 不符")
        for e in f.get("entities") or []:
            if any(k not in e for k in ENTITY_KEYS) or not isinstance(e["exists"], bool):
                bad.append(f"第 {f['video_frame']} 帧 {e.get('id')} 缺字段或 exists 不是布尔")
            elif not e["exists"] and any(e[k] is not None for k in ("position", "rotation", "animation")):
                bad.append(f"第 {f['video_frame']} 帧 {e['id']} 不存在却填了状态")
            elif e["exists"] and (not isinstance(e["position"], list) or len(e["position"]) != 3
                                  or not isinstance(e["rotation"], list) or len(e["rotation"]) != 4
                                  or abs(math.sqrt(sum(v * v for v in e["rotation"])) - 1.0) > 1e-3):
                bad.append(f"第 {f['video_frame']} 帧 {e['id']} 的 position/rotation 不合法")
        if len(bad) >= 5:
            break
    return bad


def _by_id(frame: dict) -> Dict[str, dict]:
    return {e["id"]: e for e in frame["entities"]}


def _angle_deg(q1, q2) -> float:
    dot = abs(sum(a * b for a, b in zip(q1, q2)))
    return math.degrees(2.0 * math.acos(min(1.0, dot)))


def _compare(a: dict, b: dict, *, until: Optional[int] = None, exclude=()) -> dict:
    """两路同一 video_frame 的动态物：最大位置（单轴）/朝向偏差、超出容差的单位帧、对不上的存在性与动作 ID，
    以及 sample_time 不同的帧（需方按同一 video_frame 连同 sample_time 一起比，2026-10-19）。"""
    worst = {"position_m": 0.0, "rotation_deg": 0.0, "at": None, "exists": [], "animation": [], "frames": 0, "exact": True,
             "over": 0, "over_at": [], "time_frames": 0, "time_max_s": 0.0}
    for fa, fb in zip(a["frames"], b["frames"]):
        if until is not None and fa["video_frame"] >= until:
            break
        worst["frames"] += 1
        dt = abs(float(fa["sample_time"]) - float(fb["sample_time"]))
        if dt > 1e-9:
            worst["time_frames"] += 1
            worst["time_max_s"] = max(worst["time_max_s"], dt)
        ea, eb = _by_id(fa), _by_id(fb)
        for i, x in ea.items():
            if i in exclude or i not in eb:
                continue
            y = eb[i]
            if x["exists"] != y["exists"]:
                if len(worst["exists"]) < 3:
                    worst["exists"].append(f"{i}@{fa['video_frame']}")
                continue
            if not x["exists"]:
                continue
            if x["position"] != y["position"] or x["rotation"] != y["rotation"]:
                worst["exact"] = False
            d = max(abs(p - q) for p, q in zip(x["position"], y["position"]))
            r = _angle_deg(x["rotation"], y["rotation"])
            if d > worst["position_m"]:
                worst["position_m"], worst["at"] = d, f"{i}@{fa['video_frame']}"
            worst["rotation_deg"] = max(worst["rotation_deg"], r)
            if d > CROSS_TOLERANCE_M or r > CROSS_TOLERANCE_DEG:
                worst["over"] += 1
                if len(worst["over_at"]) < 6:
                    worst["over_at"].append(f"{i}@{fa['video_frame']}（{d * 1000:.2f} mm / {r:.3f}°）")
            ida, idb = (x["animation"] or {}).get("id"), (y["animation"] or {}).get("id")
            if ida != idb and len(worst["animation"]) < 3:
                worst["animation"].append(f"{i}@{fa['video_frame']} {ida}≠{idb}")
    return worst


def _cross(rule: str, what: str, w: dict) -> RuleResult:
    if not w["frames"]:
        return RuleResult(rule, SKIP, "group", f"{what}：没有可比的帧", REQ)
    told = (f"{what}：比了 {w['frames']} 帧，位置单轴最大差 {w['position_m'] * 1000:.1f} mm（{w['at'] or '—'}），"
            f"朝向最大差 {w['rotation_deg']:.2f}°")
    data = {k: w[k] for k in ("position_m", "rotation_deg", "frames", "over", "time_frames", "time_max_s")}
    if w["exists"] or w["animation"]:
        return RuleResult(rule, FAIL, "group", told + f"；存在性不同 {w['exists']}，动作不同 {w['animation']}", REQ, data)
    if w["time_frames"]:
        return RuleResult(rule, FAIL, "group", told + f"；sample_time 有 {w['time_frames']} 帧不同（最大 {w['time_max_s'] * 1000:.1f} ms）："
                          "需方按同一 video_frame 连同 sample_time 一起比", REQ, data)
    if w["exact"]:
        return RuleResult(rule, PASS, "group", told + "：逐位一致", REQ, data)
    if w["position_m"] <= CROSS_TOLERANCE_M and w["rotation_deg"] <= CROSS_TOLERANCE_DEG:
        return RuleResult(rule, WARN, "group", told + f"：在单轴 {CROSS_TOLERANCE_M * 1000:g} mm / {CROSS_TOLERANCE_DEG:g}° 内"
                          "（三遍分开录的数值误差，需方 10-19 的质检不报这么小的差）", REQ, data)
    return RuleResult(rule, FAIL, "group", told + f"：{w['over']} 个单位帧超出单轴 {CROSS_TOLERANCE_M * 1000:g} mm / "
                      f"{CROSS_TOLERANCE_DEG:g}°（需方 10-19 按这个量级判不一致），如 {'、'.join(w['over_at'])}", REQ, data)


def check_multicam_sync(g: Group) -> List[RuleResult]:
    out: List[RuleResult] = []
    docs, errors, named = _load(g)
    if not docs and not errors:
        return [RuleResult("msync.fields", UNVERIFIED, "group",
                           "包里没有 multicam_sync_i.json（交付说明 v1.1 之前出的包，或出包没带上）", REQ)]
    counts = _lane_counts(g)
    good: Dict[int, dict] = {}
    for i in range(1, 13):
        if i in errors:
            out.append(RuleResult("msync.fields", FAIL, f"#{i}", f"multicam_sync_{i}.json 读不了：{errors[i]}", REQ))
            continue
        if i not in docs:
            out.append(RuleResult("msync.fields", UNVERIFIED, f"#{i}", f"缺 multicam_sync_{i}.json", REQ))
            continue
        bad = _fields(named[i], docs[i], counts.get(i))
        if bad:
            out.append(RuleResult("msync.fields", FAIL, f"#{i}", "；".join(bad[:5]), REQ))
            continue
        out.append(RuleResult("msync.fields", PASS, f"#{i}", f"头字段、逐帧字段齐全，{len(docs[i]['frames'])} 帧与视频/action_camera 一一对应", REQ))
        bad = _entities(docs[i])
        out.append(RuleResult("msync.entities", FAIL if bad else PASS, f"#{i}", "；".join(bad[:5]) if bad else
                              f"{len(docs[i]['tracked_entity_ids'])} 个单位每帧都在、exists 与状态相符", REQ))
        if not bad:
            good[i] = docs[i]
    if len(good) < 12:
        out.append(RuleResult("msync.frames", UNVERIFIED, "group", f"只有 {len(good)} 路 multicam_sync 可用，跨路检查不做", REQ))
        return out

    # 组内统一：头字段、时间原点、帧数与逻辑帧
    first = good[1]
    differ = [k for k in ("schema_version", "capture_group_id", "fps", "time_unit", "coordinate_system", "tracked_entity_ids")
              if any(d[k] != first[k] for d in good.values())]
    out.append(RuleResult("msync.group_header", FAIL if differ else PASS, "group",
                          f"12 路的 {'、'.join(differ)} 不一致" if differ else
                          f"12 路 capture_group_id={first['capture_group_id']}、fps={first['fps']}、坐标系与单位清单一致", REQ))
    back = [i for i, d in good.items() if d["frames"][0]["sample_time"] != 0
            or any(b["sample_time"] < a["sample_time"] for a, b in zip(d["frames"], d["frames"][1:]))]
    out.append(RuleResult("msync.time", FAIL if back else PASS, "group",
                          f"编号 {back} 的 sample_time 不从 0 起或有倒退" if back else "sample_time 各路从 0 起、不倒退", REQ))
    n = {i: len(d["frames"]) for i, d in good.items()}
    logical = {i: [f["logical_frame"] for f in d["frames"]] for i, d in good.items()}
    if len(set(n.values())) != 1 or any(v != logical[1] for v in logical.values()):
        out.append(RuleResult("msync.frames", FAIL, "group", f"各路帧数 {sorted(set(n.values()))}，或 logical_frame 逐帧不同", REQ))
    else:
        out.append(RuleResult("msync.frames", PASS, "group", f"12 路都是 {n[1]} 帧，logical_frame 逐帧相同", REQ))

    # 同一遍四路：同一世界
    for name, numbers in SAME_TAKE.items():
        ref = good[numbers[0]]["frames"]
        off = [i for i in numbers[1:] if good[i]["frames"] != ref]
        out.append(RuleResult("msync.in_take", FAIL if off else PASS, name,
                              f"编号 {off} 与编号 {numbers[0]} 同一 video_frame 的状态不同" if off else
                              f"编号 {numbers[0]}–{numbers[-1]} 逐帧状态相同", REQ))

    # 接管：C 四路相同、整数秒；其余 null
    fps = first["fps"]
    takeovers = {i: good[i]["takeover"] for i in SAME_TAKE["C"]}
    takeover_frame, c_person = None, None
    shapes = [t for t in takeovers.values() if not (isinstance(t, list) and t and isinstance(t[0], dict))]
    if shapes:
        out.append(RuleResult("msync.takeover", FAIL, "C", "C 的 takeover 不是非空事件数组", REQ))
    else:
        frames = {(t[0].get("video_frame"), t[0].get("logical_frame")) for t in takeovers.values()}
        persons = {tuple(t[0].get("entity_ids") or ()) for t in takeovers.values()}
        if len(frames) != 1 or len(persons) != 1:
            out.append(RuleResult("msync.takeover", FAIL, "C", f"编号 5–8 的接管帧或对象不同：{sorted(frames)} {sorted(persons)}", REQ))
        else:
            (vf, _), = frames
            takeover_frame, c_person = vf, list(persons)[0]
            whole = isinstance(vf, int) and vf % fps == 0
            out.append(RuleResult("msync.takeover", PASS if whole else FAIL, "C",
                                  f"编号 5–8 都在 video_frame {vf}（第 {vf / fps:g} 秒）接管 {'、'.join(c_person)}"
                                  + ("" if whole else "：不是整数秒（N = video_frame / fps 须为整数）"), REQ))
    stray = [i for i in (*SAME_TAKE["A/B"], *SAME_TAKE["D"]) if good[i]["takeover"] not in (None, [])]
    if stray:
        out.append(RuleResult("msync.takeover", FAIL, "group", f"编号 {stray} 没有接管却填了 takeover", REQ))

    # 受控人物的存在性
    seen: Dict[int, Dict[str, set]] = {}   # 每路每个单位出现过的 exists 取值
    for i, d in good.items():
        seen[i] = {}
        for f in d["frames"]:
            for e in f["entities"]:
                seen[i].setdefault(e["id"], set()).add(e["exists"])

    def everywhere(i, ent, value):
        return seen[i].get(ent) == {value}
    if c_person:
        lost = [i for i in range(1, 13) for p in c_person if not everywhere(i, p, True)]
        out.append(RuleResult("msync.controlled", FAIL if lost else PASS, "C",
                              f"C 的受控人物在编号 {sorted(set(lost))} 有帧不存在" if lost else
                              f"C 的受控人物 {'、'.join(c_person)} 12 路全帧存在", REQ))
    only_d = [e for e in first["tracked_entity_ids"] if all(everywhere(i, e, False) for i in range(1, 9))
              and not all(everywhere(i, e, False) for i in SAME_TAKE["D"])]
    if len(only_d) != 1:
        out.append(RuleResult("msync.controlled", FAIL, "D",
                              f"D 的受控人物应是 1–8 路全程不存在、9–12 路存在的唯一单位，找到 {only_d or '没有'}", REQ))
    else:
        gaps = [i for i in SAME_TAKE["D"] if not everywhere(i, only_d[0], True)]
        out.append(RuleResult("msync.controlled", FAIL if gaps else PASS, "D",
                              f"D 的受控人物 {only_d[0]} 在编号 {gaps} 有帧不存在" if gaps else
                              f"D 的受控人物 {only_d[0]} 9–12 路全帧存在、1–8 路全程不存在", REQ))

    # 跨世界线：接管前 A=C；D 无事件，除受控人物外全程 = A
    if takeover_frame is not None:
        out.append(_cross("msync.cross_takeover", f"接管（第 {takeover_frame} 帧）前编号 1 比编号 5",
                          _compare(good[1], good[5], until=takeover_frame)))
    controlled = (*(c_person or ()), *only_d)
    out.append(_cross("msync.cross_no_event", "D 无事件：编号 1 比编号 9（除受控人物）",
                      _compare(good[1], good[9], exclude=controlled)))

    out.append(_lighting(good))
    return out


def _sun_ok(light) -> bool:
    """a frame's lighting: the sun's direction (where its light goes: a unit vector pointing down, y up) and a positive
    intensity"""
    if not isinstance(light, dict):
        return False
    d, k = light.get("sun_direction"), light.get("intensity")
    return (isinstance(d, list) and len(d) == 3 and all(isinstance(v, (int, float)) for v in d)
            and abs(math.sqrt(sum(v * v for v in d)) - 1.0) < 0.01 and d[1] < 0
            and isinstance(k, (int, float)) and k > 0)


def _lighting(good: Dict[int, dict]) -> RuleResult:
    unlit = [i for i, d in good.items() if all(f["lighting"] is None for f in d["frames"])]
    bad = {i: sum(1 for f in d["frames"] if not _sun_ok(f["lighting"])) for i, d in good.items() if i not in unlit}
    bad = {i: n for i, n in bad.items() if n}
    if bad:
        return RuleResult("msync.lighting", FAIL, "group",
                          "lighting 缺帧或格式不对（要每帧 sun_direction 为朝下的单位向量、intensity 为正数）："
                          + "、".join(f"编号 {i} {n} 帧" for i, n in sorted(bad.items())), REQ)
    if unlit:
        return RuleResult("msync.lighting", WARN, "group",
                          f"编号 {unlit} 的 lighting 全程 null：有太阳却没记方向和强度（交付说明：有主光源时填写）", REQ)
    first = next(iter(good.values()))["frames"][0]["lighting"]
    d = first["sun_direction"]
    return RuleResult("msync.lighting", PASS, "group",
                      f"12 路每帧都记了太阳方向和强度（如编号 {min(good)} 第 0 帧：sun_direction {d}，即太阳高度 "
                      f"{math.degrees(math.asin(-d[1])):.1f}°，intensity {first['intensity']:g}）；数值来源见 capture_note", REQ)

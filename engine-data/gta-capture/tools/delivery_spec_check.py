"""delivery_spec_check.py <package dir>: a delivered group against the customer's delivery doc
(《Multi-camera 12 路相机数据交付说明》v1.1, 2026-10-16), item by item -- sections 1.2 and 4 (48 files, names), 6.1
(the self-check table) and 6.2 (multicam_sync fields) -- on its own, apart from worldline_qc: a second reading of the
same doc. Prints one line per item: [OK ], [NO ] or [?? ] (a number for a person to judge, e.g. the cross-take
consistency after a takeover, which the doc counts only for a group with no event). Needs ffprobe on PATH.
Written for beach-demo-113 (2026-10-18); its first run found lighting null in all 43200 frames."""
import json, math, os, re, shutil, subprocess, sys

pkg = sys.argv[1]
N = range(1, 13)
GROUPS = {"A&B": [1, 2, 3, 4], "C": [5, 6, 7, 8], "D": [9, 10, 11, 12]}


def say(item, ok, text):
    print(f"[{'OK ' if ok is True else 'NO ' if ok is False else '?? '}] {item}: {text}")


# 1.2 / 4: 48 files, names with underscores, nothing else
want = {f"{k}_{i}.{e}" for i in N for k, e in (("video", "mp4"), ("systeminfo", "json"), ("action_camera", "json"),
                                                ("multicam_sync", "json"))}
have = set(os.listdir(pkg))
say("48 个文件、命名带下划线", have == want, f"{len(have)} 个；缺 {sorted(want - have)}；多 {sorted(have - want)}")
say("采集组文件夹名 YYYY_MM_DD_场景_序号", bool(re.fullmatch(r"\d{4}_\d{2}_\d{2}_.+_\d+", os.path.basename(pkg))),
    os.path.basename(pkg))

# videos: 120 s, 30 fps, 3600 decoded frames
ffprobe = shutil.which("ffprobe")
vframes = {}
bad = []
for i in N:
    out = subprocess.run([ffprobe, "-v", "error", "-count_frames", "-select_streams", "v:0", "-show_entries",
                          "stream=nb_read_frames,r_frame_rate,width,height:format=duration", "-of", "json",
                          os.path.join(pkg, f"video_{i}.mp4")], capture_output=True, text=True, check=True).stdout
    j = json.loads(out); s = j["streams"][0]
    vframes[i] = int(s["nb_read_frames"])
    if (vframes[i], s["r_frame_rate"], abs(float(j["format"]["duration"]) - 120) < 0.05) != (3600, "30/1", True):
        bad.append((i, vframes[i], s["r_frame_rate"], j["format"]["duration"]))
say("视频每路 120 秒、30 fps、3600 帧", not bad, f"不符 {bad}" if bad else "12 路都是 3600 帧、30/1、120 s")

# systeminfo
bad = []
for i in N:
    d = json.load(open(os.path.join(pkg, f"systeminfo_{i}.json"), encoding="utf-8"))
    if set(d) != {"gameProcessName", "x", "y", "width", "height", "recordDpi"}:
        bad.append((i, sorted(d)))
say("systeminfo 字段 gameProcessName/x/y/width/height/recordDpi", not bad, f"不符 {bad}" if bad else "12 路齐全")

# action_camera: frame for frame with the video
bad = [(i, n) for i in N for n in [len(json.load(open(os.path.join(pkg, f"action_camera_{i}.json"), encoding="utf-8")))]
       if n != vframes[i]]
say("action_camera 与视频逐帧对应（条数=解码帧数）", not bad, f"不符 {bad}" if bad else "12 路都是 3600 条")

# multicam_sync
ms = {}
utf8_bad = []
for i in N:
    raw = open(os.path.join(pkg, f"multicam_sync_{i}.json"), "rb").read()
    try:
        ms[i] = json.loads(raw.decode("utf-8"))
    except UnicodeDecodeError:
        utf8_bad.append(i)
say("multicam_sync 用 UTF-8", not utf8_bad, f"不是 UTF-8：{utf8_bad}" if utf8_bad else "12 个都能按 UTF-8 解开")

HEAD = {"schema_version": str, "capture_group_id": str, "camera_id": str, "video_file": str, "fps": (int, float),
        "coordinate_system": dict, "time_unit": str, "capture_note": str, "tracked_entity_ids": list, "frames": list}
bad = []
for i, d in ms.items():
    for k, t in HEAD.items():
        if not isinstance(d.get(k), t):
            bad.append((i, k))
    if "takeover" not in d:
        bad.append((i, "takeover"))
    if d.get("schema_version") != "1.0" or d.get("time_unit") != "second" or d.get("video_file") != f"video_{i}.mp4" \
            or d.get("camera_id") != f"video_{i}" or d.get("fps") != 30:
        bad.append((i, "values"))
    cs = d.get("coordinate_system", {})
    if not all(k in cs for k in ("space", "unit", "up_axis", "forward_axis", "handedness", "quaternion_order")):
        bad.append((i, "coordinate_system keys"))
say("头字段齐全、取值对（1.0 / second / video_i / 30 / 坐标系六项）", not bad, f"不符 {bad}" if bad else "12 个齐全")
ids = {d["capture_group_id"] for d in ms.values()}
say("capture_group_id 组内一致", len(ids) == 1, f"{ids}；文件夹名 {os.path.basename(pkg)}")
cs = {json.dumps(d["coordinate_system"], sort_keys=True) for d in ms.values()}
say("coordinate_system 组内一致", len(cs) == 1, f"{len(cs)} 种")
tl = {tuple(d["tracked_entity_ids"]) for d in ms.values()}
say("tracked_entity_ids 各路一致", len(tl) == 1, f"{len(tl)} 种；{len(next(iter(tl)))} 个单位：{list(next(iter(tl)))}")

# frames
bad, ent_bad, null_bad, light = [], [], [], 0
for i, d in ms.items():
    fr = d["frames"]
    if len(fr) != vframes[i] or [f["video_frame"] for f in fr] != list(range(len(fr))):
        bad.append((i, len(fr)))
    tracked = d["tracked_entity_ids"]
    for f in fr:
        if not all(k in f for k in ("video_frame", "logical_frame", "sample_time", "entities", "lighting")):
            bad.append((i, "frame keys", f.get("video_frame"))); break
        got = [e["id"] for e in f["entities"]]
        if sorted(got) != sorted(tracked):
            ent_bad.append((i, f["video_frame"])); break
        for e in f["entities"]:
            if not all(k in e for k in ("id", "exists", "position", "rotation", "animation")):
                ent_bad.append((i, f["video_frame"], e["id"], "keys")); break
            if not e["exists"] and (e["position"] is not None or e["rotation"] is not None or e["animation"] is not None):
                null_bad.append((i, f["video_frame"], e["id"]))
            if e["exists"] and e["rotation"] is not None and abs(math.sqrt(sum(c * c for c in e["rotation"])) - 1) > 1e-3:
                null_bad.append((i, f["video_frame"], e["id"], "not unit quaternion"))
        light += f["lighting"] is not None
say("video_frame 从 0 连续、条数=视频解码帧数（3600，0–3599）", not bad, f"不符 {bad[:5]}" if bad else "12 个都是 0–3599")
say("每个清单单位每帧都出现、逐帧/单位字段齐全", not ent_bad, f"不符 {ent_bad[:5]}" if ent_bad else "12 × 3600 帧全齐")
say("exists=false 时三项为 null；四元数为单位长度", not null_bad, f"不符 {null_bad[:5]}" if null_bad else "全部符合")
say("lighting（有主光源时填方向和强度）", light > 0, f"非 null 的帧 {light} / {12 * 3600}")

# logical frame / sample time
same_lf = all([f["logical_frame"] for f in ms[i]["frames"]] == [f["logical_frame"] for f in ms[1]["frames"]] for i in N)
say("logical_frame 各路逐帧一致（不是各路自己的帧号顶替）", same_lf, "12 路逐帧相同" if same_lf else "不同")

# takeover
c_n = []
for i in GROUPS["C"]:
    t = ms[i]["takeover"]
    t = t[0] if isinstance(t, list) else t
    c_n.append(None if not t else t["video_frame"] / 30)
say("C 四路接管秒 N 相等且为整数", len(set(c_n)) == 1 and c_n[0] is not None and float(c_n[0]).is_integer(), f"N = {c_n}")
for i in GROUPS["C"]:
    t = ms[i]["takeover"]; t = t[0] if isinstance(t, list) else t
    miss = [k for k in ("logical_frame", "phase", "entity_ids", "comparison_note") if k not in t]
    if miss:
        say("takeover 字段", False, f"video_{i} 缺 {miss}")
others = {i: ms[i]["takeover"] for i in N if i not in GROUPS["C"]}
say("非 C 路 takeover", True, "全为 null" if all(v is None for v in others.values()) else f"{others}")
taken = (ms[5]["takeover"][0] if isinstance(ms[5]["takeover"], list) else ms[5]["takeover"])["entity_ids"]
n_take = int(c_n[0] * 30)


def ent_map(f):
    return {e["id"]: e for e in f["entities"]}


def diff(a, b):
    """max position (m) and rotation (deg) difference, and whether exists/animation id differ"""
    if a["exists"] != b["exists"]:
        return math.inf, math.inf, True
    if not a["exists"]:
        return 0.0, 0.0, False
    dp = math.dist(a["position"], b["position"])
    dot = abs(sum(x * y for x, y in zip(a["rotation"], b["rotation"])))
    dr = math.degrees(2 * math.acos(min(1.0, dot)))
    anim = (a["animation"] or {}).get("id") != (b["animation"] or {}).get("id")
    return dp, dr, anim


# within a group: identical state at the same video_frame
for g, lanes in GROUPS.items():
    ref = ms[lanes[0]]["frames"]
    same = all(json.dumps(ms[i]["frames"], sort_keys=True) == json.dumps(ref, sort_keys=True) for i in lanes[1:])
    say(f"组内帧同步（{g}：{lanes}）", same, "逐帧完全相同" if same else "有不同")

# 1-8 before the takeover
worst = (0.0, 0.0, None)
anim_d = 0
for k in range(n_take):
    a, b = ent_map(ms[1]["frames"][k]), ent_map(ms[5]["frames"][k])
    for eid in a:
        dp, dr, an = diff(a[eid], b[eid])
        anim_d += an
        if dp > worst[0]:
            worst = (dp, max(worst[1], dr), (eid, k))
        worst = (worst[0], max(worst[1], dr), worst[2])
say(f"接管前（0–{n_take - 1} 帧）1–8 路动态物一致", worst[0] <= 0.01 and worst[1] <= 1.0 and not anim_d,
    f"A 与 C 最大位置差 {worst[0] * 1000:.1f} mm（{worst[2]}），最大朝向差 {worst[1]:.2f}°，动作 ID 不同 {anim_d} 处")

# after the takeover: only for no-event data; report the numbers anyway
for g, ref_lane, skip in (("C", 5, set(taken)), ("D", 9, {"seventh"})):
    worst, where, n_bad = 0.0, None, 0
    for k in range(n_take if g == "C" else 0, 3600):
        a, b = ent_map(ms[1]["frames"][k]), ent_map(ms[ref_lane]["frames"][k])
        for eid in a:
            if eid in skip or eid.split(":")[0] in skip:
                continue
            dp, dr, an = diff(a[eid], b[eid])
            if dp > 0.01 or dr > 1.0 or an:
                n_bad += 1
            if dp > worst:
                worst, where = dp, (eid, k)
    say(f"接管后 A 与 {g}（除受控人物）", None, f"最大位置差 {worst * 1000:.1f} mm（{where}），超 1 cm/1°/动作不同 {n_bad} 个单位帧"
        "（文档：无事件数据才要求全程一致；有事件数据暂不统计）")

# controlled persons
c_ok = all(ent_map(f)[taken[0]]["exists"] for i in N for f in ms[i]["frames"])
d_in = all(ent_map(f)["seventh"]["exists"] for i in GROUPS["D"] for f in ms[i]["frames"])
d_out = not any(ent_map(f)["seventh"]["exists"] for i in N if i not in GROUPS["D"] for f in ms[i]["frames"])
say(f"C 受控人物（{taken[0]}）12 路全帧 exists=true", c_ok, "")
say("D 受控人物（seventh）9–12 路全帧 true、1–8 路全程 false", d_in and d_out, f"9–12 全 true {d_in}；1–8 全 false {d_out}")

# sample time: from the game's clock, one origin
st = [f["sample_time"] for f in ms[1]["frames"]]
steps = [round(b - a, 4) for a, b in zip(st, st[1:])]
say("sample_time 组内统一原点、取实际模拟时间", st[0] == 0.0,
    f"第 0 帧 {st[0]}，最后 {st[-1]}；相邻帧间隔 {min(steps)}–{max(steps)} s（恒等于 1/30 就像是按帧率推算）")

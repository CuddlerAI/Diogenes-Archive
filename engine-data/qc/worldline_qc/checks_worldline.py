"""世界线面：因果记录与交互动作中文语义、接管时刻、事件是否改变、第 7 人、B 纯相机、真实系统时间、开头静止段、
真人/脚本操作、人工复核清单（§6 验收要求列 + 09-22 会议整改项 + demo 跟进表 demo4 反馈）。

需求出处：新需求 §4.1–§4.5、§4.3、§5.2、§5.4「time 记录本轮真实系统时间」、§6、§7.2；用户本轮确认点①–④。
"""

from __future__ import annotations

import re
from typing import Dict, List, Optional, Tuple

from engine_qc.mathutil import vnorm, vsub

from .checks_json import LIFT_KEYS, MOVE_KEYS, VK_NAMES, LaneData, parse_time
from .loaders import Group
from .model import FAIL, INPUT_ROLES, MANUAL, PASS, PERSON_ROLES, SKIP, STILL_ROLES, WARN, RuleResult

TAKEOVER_MAX_S = 55
#: 交互键 → 因果记录里应出现的中文含义关键词（任一命中即视为已说明）
KEY_SEMANTICS: Dict[int, Tuple[str, Tuple[str, ...]]] = {
    1: ("鼠标左键", ("攻击", "打", "射击", "开枪", "挥拳", "揍", "左键")),
    2: ("鼠标右键", ("瞄准", "右键")),
    69: ("E", ("抢车", "上车", "交互", "拾取", "进入", "E 键", "按E", "按 E")),
    70: ("F", ("上车", "抢车", "进入", "F 键", "按F", "按 F")),
    32: ("Space", ("跳", "空格")),
    16: ("Shift", ("跑", "冲刺", "Shift")), 160: ("LShift", ("跑", "冲刺", "Shift")),
    17: ("Ctrl", ("蹲", "潜行", "Ctrl")), 162: ("LCtrl", ("蹲", "潜行", "Ctrl")),
    82: ("R", ("换弹", "R 键")),
}
_CAUSAL_LINE = re.compile(r"^\s*[-*]\s*\*\*([ABCD])\*\*[:：]\s*(.+?)\s*$", re.M)
# 交付包（stage_delivery）的写法：「## 每条世界线发生了什么」下每条世界线一节「### C（编号 5–8，录制遍 C）」，节内分条
_CAUSAL_SECTION = re.compile(r"^###\s*([ABCD])[（(][^\n]*\n(.*?)(?=^#{2,3}\s|\Z)", re.M | re.S)
_TAKEOVER = re.compile(r"视频第\s*([\d.]+)\s*秒\s*第\s*([\d.]+)\s*秒[^。；;]*?接管")
_TAKEOVER2 = re.compile(r"第\s*([\d.]+)\s*秒[^。；;]*?接管")
_PREROLL = re.compile(r"开头有\s*([\d.]+)\s*秒")
_EVENT_T = re.compile(r"(?:视频)?第\s*([\d.]+)\s*秒")   # 交付包没有开头静止段，写「第 17.1 秒」


def _frame_at(i: Optional[int]) -> str:
    """从 0 数的第 i 帧，写成交付的 frame（从 1 起）和视频时间。"""
    return "无" if i is None else f"frame {i + 1}（{i / 30:.1f}s）"


def parse_causal(readme: Optional[str]) -> Dict[str, str]:
    """{世界线: 因果记录}：「- **C**：…」一行一条，或交付包的「### C（…）」一节（节内各条连成一段）。"""
    if not readme:
        return {}
    out = {m.group(1): m.group(2) for m in _CAUSAL_LINE.finditer(readme)}
    for m in _CAUSAL_SECTION.finditer(readme):
        items = m.group(2).split("\n\n", 1)[0]   # 节内各条到第一个空行为止：之后是全组说明，不属于这条世界线
        body = " ".join(line.strip().lstrip("-*").strip() for line in items.splitlines() if line.strip())
        if body and m.group(1) not in out:
            out[m.group(1)] = body
    return out


def parse_preroll_s(readme: Optional[str]) -> float:
    if not readme:
        return 0.0
    m = _PREROLL.search(readme)
    return float(m.group(1)) if m else 0.0


def parse_takeover(text: Optional[str], preroll_s: float) -> Tuple[Optional[float], Optional[float]]:
    """返回 (视频秒, 剧本秒)。"""
    if not text:
        return None, None
    m = _TAKEOVER.search(text)
    if m:
        return float(m.group(1)), float(m.group(2))
    m = _TAKEOVER2.search(text)
    if m:
        s = float(m.group(1))
        return s + preroll_s, s
    return None, None


def takeover_frames(g: Group) -> Dict[str, int]:
    """{worldline: 接管视频帧}——供 motion/pixels 划窗。D 无「接管」字样时按第 0 秒起（即静止段之后）。"""
    causal = parse_causal(g.readme)
    pre = parse_preroll_s(g.readme)
    out: Dict[str, int] = {}
    for wl in ("C", "D"):
        v, s = parse_takeover(causal.get(wl), pre)
        if v is None and wl == "D" and causal.get("D"):
            v = pre
        if v is not None:
            out[wl] = int(round(v * 30))
    return out


def check_worldlines(g: Group, lanes: Dict[str, LaneData]) -> List[RuleResult]:
    out: List[RuleResult] = []
    causal = parse_causal(g.readme)
    pre = parse_preroll_s(g.readme)

    # 因果记录存在 + 带时刻
    if not causal:
        out.append(RuleResult("wl.causal_record", FAIL, "group",
                              "交付包内无「每条世界线发生了什么」因果记录（需求 §5.2：交付记录须说明 C/D 原有事件是否被改变、对应控制行为及受影响事件）",
                              "需求§5.2 / 确认点②"))
    else:
        for wl in "ABCD":
            txt = causal.get(wl)
            if not txt:
                out.append(RuleResult("wl.causal_record", FAIL, f"世界线{wl}", "因果记录缺该世界线条目", "需求§5.2"))
                continue
            ts = _EVENT_T.findall(txt)
            if wl in "CD":
                st = PASS if ts else WARN
                out.append(RuleResult("wl.causal_record", st, f"世界线{wl}",
                                      f"因果记录（README）：「{txt[:160]}{'…' if len(txt) > 160 else ''}」；带时刻的事件 {len(ts)} 条"
                                      + ("" if ts else "（未写明第几秒发生了什么）"), "需求§5.2 / 确认点②"))
            else:
                out.append(RuleResult("wl.causal_record", PASS, f"世界线{wl}", f"「{txt[:120]}」", "需求§5.2"))

    # 交互键的中文语义
    for lf in g.lanes:
        ld = lanes.get(lf.info.key)
        if not ld or not ld.records or lf.info.role not in INPUT_ROLES:
            continue
        keys_frames: Dict[int, List[int]] = {}
        for i, r in enumerate(ld.records):   # i：从 0 数的第几帧（= 视频时间 × 30）；交付的 frame 从 1 起
            for k in r.get("keyCode") or []:
                if k not in MOVE_KEYS and k not in LIFT_KEYS:
                    keys_frames.setdefault(k, []).append(i)
        if lf.info.role == "first_person":
            # 与追背路同一份输入，只在追背路判一次
            continue
        wl = lf.info.worldline
        text = causal.get(wl, "")
        if not keys_frames:
            out.append(RuleResult("wl.interaction_semantics", PASS, ld.key, "除 WASD/QE 外无其他交互键", "确认点②"))
            continue
        for k, frames in sorted(keys_frames.items()):
            name, words = KEY_SEMANTICS.get(k, (VK_NAMES.get(k, str(k)), ()))
            hit = [w for w in words if w in text]
            spans = _spans(frames)
            where = "、".join(f"{s / 30:.1f}–{e / 30:.1f}s" for s, e in spans[:6]) + ("…" if len(spans) > 6 else "")
            if wl == "B":
                out.append(RuleResult("wl.b_pure_camera", FAIL, ld.key,
                                      f"B 自由相机出现交互键 {name}（{len(frames)} 帧），B 不得控制人物/改变事件", "需求§4.2"))
                continue
            if hit:
                out.append(RuleResult("wl.interaction_semantics", PASS, ld.key,
                                      f"交互键 {name}（键码 {k}）{len(frames)} 帧、{len(spans)} 段（{where}）：因果记录含「{hit[0]}」等中文说明", "确认点②"))
            else:
                out.append(RuleResult("wl.interaction_semantics", FAIL, ld.key,
                                      f"交互键 {name}（键码 {k}）按下 {len(frames)} 帧、{len(spans)} 段（视频 {where}），"
                                      f"但世界线{wl}的因果记录没有写明它对应的实际动作含义（要求：文字备注写明「第几秒有 XX 行为」，中文语义，不能只写按了哪个键）",
                                      "确认点② / 需求§5.2", data={"key": k, "frames": [i + 1 for i in frames[:200]]}))

    # 接管时刻（C）
    c_text = causal.get("C")
    v_s, s_s = parse_takeover(c_text, pre)
    c_person = [lanes[lf.info.key] for lf in g.lanes_of("C") if lf.info.role == "chase" and lanes.get(lf.info.key) and lanes[lf.info.key].records]
    if s_s is None:
        out.append(RuleResult("wl.takeover_time", FAIL, "世界线C", "未记录接管时刻 N（应为第 N 整数秒，N∈[0,55]）", "确认点④ / 需求§4.4"))
    else:
        problems = []
        if abs(s_s - round(s_s)) > 1e-6:
            problems.append(f"N={s_s:g} 不是整数秒")
        if not (0 <= s_s <= TAKEOVER_MAX_S):
            problems.append(f"N={s_s:g} 不在 [0,{TAKEOVER_MAX_S}]")
        ev = f"README：剧本第 {s_s:g} 秒接管（视频第 {v_s:g} 秒，含开头静止段 {pre:g} s）"
        if c_person:
            ld = c_person[0]
            # 从 0 数的第几帧（= 视频时间 × 30）；报告里写交付的 frame（从 1 起）
            first_key = next((i for i, r in enumerate(ld.records) if r.get("keyCode")), None)
            first_mouse = next((i for i, r in enumerate(ld.records) if r.get("mouse_dx") or r.get("mouse_dy")), None)
            ev += f"；首个按键 {_frame_at(first_key)}、首个鼠标非零 {_frame_at(first_mouse)}"
            if first_key is not None and first_key < int(v_s * 30):
                problems.append("首个按键早于接管时刻")
        st = FAIL if problems else PASS
        out.append(RuleResult("wl.takeover_time", st, "世界线C", ev + ("；" + "；".join(problems) if problems else ""),
                              "确认点④ / 需求§4.4", data={"takeover_video_s": v_s, "takeover_script_s": s_s}))
        out.append(RuleResult("wl.takeover_in_data", WARN, "世界线C",
                              "接管时刻与受控人物标识只写在 README，未进入第三人称路的 action_camera 数据（需求 §5.2：记录在各自第三人称路的数据中，与实际开始帧对应；"
                              "但 §5.2 又禁止擅自新增 JSON 键——须需方给出字段定义/交付样例后补）", "需求§5.2"))

    # 事件是否改变（显式标注）
    for wl in "CD":
        txt = causal.get(wl, "")
        if not txt:
            continue
        explicit = re.search(r"(原有?事件|事件)[^。]{0,12}(被改变|未被改变|已改变|未改变|改变了|没有改变)", txt)
        implied = any(w in txt for w in ("停下", "改变", "挡住", "推", "撞", "打"))
        if explicit:
            out.append(RuleResult("wl.event_changed_flag", PASS, f"世界线{wl}", f"显式标注：「{explicit.group(0)}」", "需求§4.3/§5.2/§7.1"))
        else:
            out.append(RuleResult("wl.event_changed_flag", WARN, f"世界线{wl}",
                                  "未显式标注「原有事件是否被改变」" + ("；从因果记录可推断为已改变（有人物被挡停等反应）" if implied else "；从因果记录看不出是否改变")
                                  + "。需求 §7.1 要求 C/D 区分「改变 / 未改变」两种情形并在交付表登记；50/50 比例是批次层面要求，单组不判",
                                  "需求§4.3/§5.2/§7.1"))

    # D 第 7 人
    d_text = causal.get("D", "")
    if d_text:
        new7 = ("新增" in d_text) and ("第 7 人" in d_text or "第7人" in d_text or "第七人" in d_text)
        model = re.search(r"[（(]([a-z_0-9]+)[）)]", d_text)
        c_p = [lanes[lf.info.key] for lf in g.lanes_of("C") if lf.info.role in PERSON_ROLES and lanes.get(lf.info.key) and lanes[lf.info.key].records]
        d_p = [lanes[lf.info.key] for lf in g.lanes_of("D") if lf.info.role in PERSON_ROLES and lanes.get(lf.info.key) and lanes[lf.info.key].records]
        extra = ""
        if c_p and d_p and c_p[0].player_positions() and d_p[0].player_positions():
            dist = vnorm(vsub(c_p[0].player_positions()[0], d_p[0].player_positions()[0]))
            extra = f"；D 受控人物首帧位置与 C 接管人物首帧位置相距 {dist:.1f} m"
        out.append(RuleResult("wl.d_new_person", MANUAL if new7 else FAIL, "世界线D",
                              (f"README 声明新增第 7 人{'（模型 ' + model.group(1) + '）' if model else ''}{extra}。"
                               "是否确非原有 6 人之一、动画是否连续无穿模悬浮抽搐——本包无世界遥测，须人工看画面" if new7
                               else "README 未声明 D 为「新增第 7 人」"), "需求§4.5"))

    # B：无人物字段、无交互键（交互键已在上面判）
    for lf in g.lanes_of("B"):
        ld = lanes.get(lf.info.key)
        if ld and ld.records:
            has_player = any(k.startswith("player_") for k in ld.records[0])
            out.append(RuleResult("wl.b_pure_camera", FAIL if has_player else PASS, ld.key,
                                  "B 自由相机" + ("带了人物字段，但 B 不应控制人物" if has_player else "不带人物字段、只有相机+键鼠字段：不控制人物、不改变事件"),
                                  "需求§4.2"))

    # A：无接管、无真实输入。键鼠字段每一路都带（需方 2026-09-29）：A 的固定机位全空，自由运镜是由运镜路线换算的
    # （换算是否与相机运动一致见 input.path_keys）；A 不许有受控人物字段，也不许有真实输入的角色。
    for lf in g.lanes_of("A"):
        ld = lanes.get(lf.info.key)
        if ld and ld.records:
            has_player = any(k.startswith("player_") for k in ld.records[0])
            real_input = lf.info.role in INPUT_ROLES
            still_pressed = lf.info.role in STILL_ROLES and any(r.get("keyCode") or r.get("mouse_dx") or r.get("mouse_dy")
                                                                for r in ld.records)
            bad = has_player or real_input or still_pressed
            why = ("出现受控人物字段" if has_player else f"出现真实输入的角色 {lf.info.role}" if real_input
                   else "固定机位出现按键或鼠标位移" if still_pressed else "")
            out.append(RuleResult("wl.a_baseline", FAIL if bad else PASS, ld.key,
                                  "A 对照线" + (why if bad else "无受控人物、无真实输入（固定机位键鼠全空，自由运镜键鼠为运镜路线换算）"),
                                  "需求§4.1 / 需方 2026-09-29"))

    # 真实系统时间：各遍首帧 time 是否相同
    firsts: Dict[str, str] = {}
    for take in g.takes:
        for lf in g.lanes_in_take(take):
            ld = lanes.get(lf.info.key)
            if ld and ld.records:
                firsts[take] = ld.records[0].get("time")
                break
    sem = (g.manifest or {}).get("timestamp_semantics") if isinstance(g.manifest, dict) else None
    if len(firsts) >= 2 and len(set(firsts.values())) == 1:
        out.append(RuleResult("wl.time_real_clock", FAIL, "group",
                              f"{len(firsts)} 遍录制的首帧 time 完全相同（{next(iter(firsts.values()))}）"
                              + (f"，manifest 亦声明 timestamp_semantics={sem!r}" if sem else "")
                              + "：time 不是本轮真实系统时间。需求 §4.3/§5.4 要求 time 记录本轮真实系统时间，不回拨、不伪写", "需求§4.3/§5.4"))
    elif firsts:
        parsed = {k: parse_time(v) for k, v in firsts.items()}
        if all(parsed.values()):
            span = max(parsed.values()) - min(parsed.values())
            out.append(RuleResult("wl.time_real_clock", PASS if span.total_seconds() >= 60 else WARN, "group",
                                  f"各遍首帧 time：{firsts}（跨度 {span}）" + ("" if span.total_seconds() >= 60 else "；各遍起点间隔小于 1 分钟，请确认为真实时钟"),
                                  "需求§4.3/§5.4"))
        else:
            out.append(RuleResult("wl.time_real_clock", FAIL, "group", f"首帧 time 无法解析：{firsts}", "需求§5.4"))

    # 开头静止段
    if pre > 0:
        out.append(RuleResult("wl.preroll", WARN, "group",
                              f"README：视频开头有 {pre:g} 秒开拍前静止段，视频时间 = 逻辑时间 + {pre:g} s。需求要求每条世界线从逻辑第 0 秒完整录到 120 s，"
                              "静止段是否可交付、是否计入 120 s 待需方确认", "需求§3.3/§4.3"))
    else:
        out.append(RuleResult("wl.preroll", PASS, "group", "README 未声明开头静止段", "需求§3.3"))

    # 真人 / 脚本
    if g.readme and ("真人" in g.readme):
        out.append(RuleResult("wl.operator", MANUAL, "group",
                              "README 自述 B/C/D 为真人键鼠操控（确认点③：优先真人不强制；脚本操作须自然且先交 demo 评估）。"
                              "操作是否自然（不撞墙、不畸形路径、不原地抽搐）须人工看画面", "确认点③"))
    else:
        out.append(RuleResult("wl.operator", WARN, "group", "README 未说明 B/C/D 是真人操作还是脚本操作；脚本操作须先交 demo 评估", "确认点③"))

    # 同一世界线各路记录同一世界状态：由 sync.* 与 pix.xtake_* 覆盖，这里只补一条汇总口径
    out.append(RuleResult("wl.same_world", MANUAL, "group",
                          "「同一世界线内多路记录同一世界状态」：本包按遍录制（同一遍四路同 time、人物两路逐帧相同，见 sync.*）；"
                          "跨世界线「B↔A / C 接管前↔A 逐帧复现」用固定机位跨遍像素比对量化（见 pix.xtake_*），但世界遥测（NPC 位置）不在交付数据里，最终仍需人工抽看",
                          "需求§4.3/§7.1"))
    out.extend(manual_checklist(g))
    return out


def _spans(frames: List[int]) -> List[Tuple[int, int]]:
    out, s, prev = [], None, None
    for f in frames:
        if s is None:
            s = prev = f
        elif f != prev + 1:
            out.append((s, prev))
            s = f
        prev = f
    if s is not None:
        out.append((s, prev))
    return out


def manual_checklist(g: Group) -> List[RuleResult]:
    """自动化不了、但验收会看的条款：逐条 MANUAL，让报告里可见。"""
    items = [
        ("manual.picture_defects", "需求§3.3/§6.3",
         "画质：噪点/锯齿/抖动/闪屏/黑白屏/单帧花屏/跳剪/马赛克/拖影/异常半透明/局部闪白/场景突变/边走边加载/LOD 跳变/阴影跳变/死黑/投影缺失"),
        ("manual.overlay", "需求§3.3",
         "画面不得出现 UI/logo/水印/帧率显示/弹窗/菜单/系统通知/桌面切出/鼠标指针；游戏场景自带广告牌不受限（09-21 会议）"),
        ("manual.camera_models", "需求§3.3",
         "自由相机不带第一人称手部模型；各相机模型互不可见；第三人称中受控人物正常可见；第一人称手部/身体呈现方式待需方确认"),
        ("manual.clipping", "需求§3.3/§6.1/§6.4/§6.5",
         "不得穿模（相机/人物/场景/头发服装）、悬浮、滑步、抽搐、瞬移、黑脸、空气墙；demo4-#2 反馈「镜头快速靠近角色出现疑似镜头穿模」、09-22 会议「角色脚部穿过瓶子」"),
        ("manual.occlusion", "需求§3.3",
         "遮挡只允许单次 1–2 s，不允许大面积或同向持续遮挡；主体出画不得过长（demo3-#6 反馈「主体物出画时间过长」）"),
        ("manual.flicker_beach", "09-22 会议 / demo4-#5",
         "沙滩区域镜头快速移动出现类百叶窗区域闪屏（疑掉帧）、远处台子持续闪动（删元素/烘焙阴影）——本次数据是否已修复须看画面（pix.luminance 只抓全帧亮度突变，抓不到局部闪动）"),
        ("manual.antialias", "09-21 会议 / demo4-#6",
         "抗锯齿被关闭导致锯齿与阴影晃动、角色移动时边缘拖影/重影——须看画面"),
        ("manual.free_camera_variety", "09-22 会议 / demo4-#1/#3",
         "自由运镜与场景强耦合、多样性不足、长期固定在角色后方俯视角（demo4 反馈「未实现预计的运镜封装」「镜头方位长期固定在角色背面」）——对照运镜类目树人工判；motion.segments 给出逐秒运镜类型"),
        ("manual.audio", "需求§3.3/§6.6",
         "音频：清晰连续场景原声、音量均匀、声画同步、无 BGM、无噪声、方位合理；C/D 干预后音频对应改变后事件——须试听（video.audio 只测响度）"),
        ("manual.scene_ground", "09-21 会议",
         "采集场景须为 10 m×10 m 及以上平整空地；画面内人数 6 名（D 为 7 名）；室内外各 50%、覆盖已录地图是批次层面要求"),
        ("manual.hardware", "需求§3.2/§5.5",
         "硬件、鼠标 DPI 1800、指针速度固定、系统缩放 recordDpi——元信息表层面，本包无元信息表"),
        ("manual.fov_per_game", "需求§3.2 / 09-22 会议",
         "各游戏 FOV 取值、追背 FOV 30–55 的水平/垂直口径、一组 12 路是否统一画幅——待需方确认"),
    ]
    return [RuleResult(rule, MANUAL, "group", text, req) for rule, req, text in items]

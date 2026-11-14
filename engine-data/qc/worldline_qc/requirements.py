"""需求条款目录：把 7 个飞书源 + 妙记 + 用户本轮确认点里的条款编成清单，每条挂上判定它的规则前缀。

驱动 HTML 报告的「需求条款 ↔ 是否达标」总表：自动化不了的条款也以 MANUAL/SKIP 出现，不让它们从报告里消失。
条款状态 = 命中规则的最严重状态（FAIL > UNVERIFIED > WARN > MANUAL > PASS > SKIP）；没有规则命中时取 default。
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Dict, List, Sequence, Tuple

from .model import FAIL, MANUAL, PASS, SKIP, UNVERIFIED, WARN, RuleResult

SEVERITY = {FAIL: 0, UNVERIFIED: 1, WARN: 2, MANUAL: 3, PASS: 4, SKIP: 5}


@dataclass(frozen=True)
class Clause:
    id: str
    section: str        # 需求出处
    text: str           # 条款摘要
    method: str         # 本工具的判定方式
    rules: Tuple[str, ...]   # 规则 id（精确）或前缀（以 . 结尾或含 * 时按前缀）
    default: str = MANUAL
    scope: str = "全部路"


C = Clause
CATALOG: List[Clause] = [
    C("R01", "新需求§1/§5.2、交付说明§一", "一组 = A(3)+B(1)+C(4)+D(4) = 12 路 × 4 文件 = 48 个（交付说明 v1.1 起多 multicam_sync_i.json）；缺任一世界线不算完整交付",
      "清点文件；按世界线计路数", ("pack.files", "pack.worldlines", "pack.extra_files"), scope="组"),
    C("R02", "交付说明§三–§五", "同一采集组内平铺存放、编号 1～12（A=1–3、B=4、C=5–8、D=9–12），文件名带下划线，目录 YYYY_MM_DD_场景_序号",
      "识别目录布局与命名", ("pack.layout", "pack.naming", "pack.group_name", "pack.discover"), scope="组"),
    C("R03", "新需求§5.5/§5.6、交付表模板", "元信息表随包上传；交付表登记 48 个 TOS 地址、世界线/相机类型/受控人物/控制开始时刻/事件是否改变等",
      "包内找表格文件；字段对模板须人工", ("pack.metainfo",), default=WARN, scope="组"),
    C("R04", "内部", "manifest 哈希与内部标记", "重算 SHA-256；读 internal/delivery_eligible", ("pack.manifest", "pack.internal_flag"), default=SKIP, scope="组"),
    C("R05", "新需求§3.2/§7.1", "每路 120 s / 30 fps / 3600 帧，禁动态帧率；帧号连续、time 逐帧 1/30 s",
      "ffprobe 帧数/时长/CFR；JSON frame/time", ("video.probe", "video.duration", "video.fps_cfr", "video.codec", "json.frame_continuity", "json.time_format")),
    C("R06", "新需求§3.2/§5.3", "全屏不裁剪不加黑边；宽高均 ≥1080；画幅白名单；像素宽高与 systeminfo 一致；窗口 (0,0)",
      "ffprobe 分辨率 ↔ systeminfo", ("video.resolution", "sysinfo.resolution", "sysinfo.origin")),
    C("R07", "新需求§3.2", "画幅配额：16:9 60% / 9:16 20% / 4:3 10% / 3:4 10%（批次层面；一组是否统一画幅待确认）",
      "只报本组分布", ("quota.aspect",), default=WARN, scope="批次"),
    C("R08", "新需求§3.2", "焦距 18/24/35/50 mm 各 25%（批次）；单条内 FOV 固定，禁止滚轮变焦；各游戏 FOV 取值待确认",
      "fx 反推等效焦距对档位；内参恒定", ("cam.focal", "json.fov_fixed", "quota.focal", "manual.fov_per_game")),
    C("R09", "新需求§3.2/§5.4/§7.1", "同一世界线内严格帧同步（同帧号 = 同逻辑时刻），各路 time 偏差 <100 ms；视频帧数 = JSON 记录数",
      "同世界线各路记录数、逐帧 time 比对；视频帧数 ↔ JSON", ("sync.frames", "sync.time", "video.json_frames"), scope="世界线"),
    C("R10", "新需求§3.2/§5.4/§6.2", "游戏运行帧率 ≥30 fps，JSON 的 fps 记实际运行值", "fps 字段最小值与是否恒定", ("json.game_fps", "json.game_fps_measured")),
    C("R11", "新需求§3.2", "动作与视频延迟 ≤20 ms", "输入↔运动相关分析给出滞后帧数（1 帧 = 33 ms，分辨率不足以判 20 ms）", ("input.mouse_drives_view",), default=MANUAL, scope="B/C/D 输入路"),
    C("R12", "新需求§3.3/§6.3、09-22 会议、demo4-#5/#6", "严禁噪点/锯齿/抖动/闪屏/黑白屏/花屏/跳剪/马赛克/拖影/半透明/闪白/场景突变/边走边加载",
      "亮度异常、跳剪、冻结帧代理；其余人工", ("pix.luminance", "pix.jump_cut", "pix.frozen", "manual.picture_defects", "manual.flicker_beach", "manual.antialias")),
    C("R13", "新需求§3.3", "不得出现 UI/logo/水印/帧率显示/弹窗/菜单/通知/桌面切出/指针；自由相机不带手部模型；各相机模型互不可见",
      "缩略图供人工复核；相机互见几何代理", ("pix.thumbs", "manual.overlay", "manual.camera_models", "geo.camera_mutual")),
    C("R14", "新需求§3.3/§4.3", "不得拼接、不得中途切换视角模式、不录读档/加载界面；每条世界线从逻辑第 0 秒完整录到 120 s",
      "追背距离恒不贴头（未切第一人称）；README 静止段声明", ("cam.chase_distance", "wl.preroll")),
    C("R15", "新需求§3.3/§6.1/§6.4/§6.5", "不得穿模；遮挡只允许单次 1–2 s，不允许大面积/同向持续遮挡",
      "相机高度代理；其余人工", ("geo.camera_height", "manual.clipping", "manual.occlusion")),
    C("R16", "新需求§3.3", "动态主体可见 ≥80% 时长（≥96 s），所有世界线所有路（含固定机位与第一人称）",
      "视锥几何代理：舞台中心 / 受控人物在视锥内的帧占比（无遮挡）", ("geo.subject_visible",)),
    C("R17", "新需求§3.3、09-21 会议", "运动像素 ≥50%（空间比例）；判定方法/时间口径/相机运动是否计入待确认",
      "三口径（逐帧/1 s 并集/全片并集）只报不判", ("pix.motion_ratio",), default=WARN),
    C("R18", "新需求§3.3/§6.6", "清晰连续场景原声、音量均匀、声画同步、无 BGM、不得无声；C/D 干预后音频对应改变后事件",
      "音轨存在 + 响度；内容人工试听", ("video.audio", "video.audio_content", "manual.audio")),
    C("R19", "新需求§3.4/§7.1", "固定机位：A/C/D 各 1 路，0–120 s 位置不变，只允许平滑转向，尽量朝向静止；B 无固定机位",
      "位置最大位移、累计转动", ("cam.fixed_static",), scope="固定机位路"),
    C("R20", "新需求§3.4/§4.4/§5.4", "路号/角色：A 1 固定 2/3 自由；B 1 真实输入自由相机；C/D 1 固定 2 自由 3 追背 4 第一人称；人物字段只在受控路，两路同一人同一次录制",
      "按角色的字段契约；追背↔第一人称逐帧一致", ("json.schema", "json.keys", "sync.person_lanes")),
    C("R21", "新需求§3.4", "A 的两路自由运镜过半时长内不得同步运镜（同时静止/同向摇/机械跟随）；只在 A 内部检查",
      "1 s 窗同步占比（engine_qc 解耦判据）", ("motion.a_independence",), scope="世界线A"),
    C("R22", "新需求§3.4、demo4-#1/#3", "自由运镜每路随机 1–10 段，每段至少一种六向平移以外的运镜；参数随机；段间平滑；速度均匀分布；每路包含六向平移",
      "六向平移累计位移；逐秒运镜类型统计供人工分段", ("motion.six_dir", "motion.segments", "motion.static_fraction", "manual.free_camera_variety"), scope="程序自由运镜路"),
    C("R23", "新需求§3.4", "共视 ≥50%（≥60 s）：同一世界线内多路同一逻辑时刻从不同角度看到相同主体；A/C/D 适用，B 不适用；参与相机范围待确认",
      "视锥几何代理：≥2 路同时看到舞台中心或受控人物", ("geo.covis",), scope="世界线"),
    C("R24", "新需求§3.4", "运镜类目树覆盖（平移/摇动/旋转/环绕/跟随/多目标/螺旋/FPV/静止/循环/特写）——批次层面，单组只打标签",
      "人工对照 motion.segments", (), default=MANUAL, scope="批次"),
    C("R25", "新需求§3.4、docs/69 §3.2", "相机运动 p95 上限：平移 6 m/s、转动 90 °/s、加速度 8 m/s²、150 °/s²、jerk 20 m/s³、600 °/s³；瞬时 ≤10 m/s、转动 ≤90 °/s；roll ±15°；不得急停/甩动/高频抖动",
      "位姿有限差分（engine_qc.check_physics / check_roll）；游戏镜头超限只 WARN",
      ("motion.physics", "motion.speed_p95", "motion.speed_max", "motion.accel_p95", "motion.jerk_p95", "motion.angspeed_p95", "motion.angspeed_max",
       "motion.angaccel_p95", "motion.angjerk_p95", "motion.roll")),
    C("R26", "新需求§3.4、09-21 会议", "场景：室内外各 50%、覆盖已录地图（批次）；10 m×10 m 以上平整空地；画面内 6 人（D 为 7 人）",
      "人工", ("manual.scene_ground",), default=MANUAL, scope="批次"),
    C("R27", "新需求§4.1", "A：6 人照剧本，无接管无新增，3 路同步 0–120 s，作为对照；基线可确定性重播",
      "A 路无受控人物、无真实输入（固定机位键鼠全空；自由运镜键鼠为运镜路线换算）", ("wl.a_baseline",), scope="世界线A"),
    C("R28", "新需求§4.2/§5.1、demo4-#8", "B：重读基线，1 路真实输入自由相机（WASD 移动、鼠标视角、Q/E 升降）；不控制人物、不改变事件；不得先生成轨迹再反推输入",
      "键位 ↔ 相机本体系速度符号；鼠标 ↔ 偏航/俯仰速率相关；Q/E ↔ 高度；无人物字段/交互键",
      ("wl.b_pure_camera", "input.keys_drive_camera", "input.qe_lift", "input.key_coverage"), scope="世界线B"),
    C("R29", "新需求§4.3、确认点①", "共同基线：每条世界线各自从第 0 秒基线恢复；B↔A、C 接管前↔A 逐帧复现；随机小动作在分叉前必须一致",
      "B 与 A 同遍同 time；固定机位跨遍像素比对（接管前 ≈ 本底噪声、分叉起点不早于接管）",
      ("sync.b_replays_a", "pix.xtake_pre", "pix.xtake_onset", "wl.same_world"), scope="世界线"),
    C("R30", "新需求§4.3/§7.1", "C 接管后 / D：受控人物按真实输入，未受干预部分继续按基线，受干预部分差异须能对应具体控制行为；C/D 各有改变/未改变两种情形（50/50 批次）",
      "接管后跨遍差异局部化；「是否改变」显式标注", ("pix.xtake_post", "wl.event_changed_flag"), scope="世界线C/D"),
    C("R31", "新需求§4.3/§5.4", "JSON 的 time 记录本轮真实系统时间，不回拨、不伪写",
      "各遍首帧 time 是否相同", ("wl.time_real_clock",), scope="组"),
    C("R32", "新需求§4.4、确认点④", "C：随机选 6 人之一，第 N 整数秒接管（N∈[0,55]），真实 WASD 键鼠控制到 120 s；接管前如实记录基线动作、不伪造接管输入；接管时刻与逐帧输入对应",
      "README 解析 N；首个按键不早于接管；接管前的帧不带键鼠字段、接管起每帧都带（需方 2026-09-30）", ("wl.takeover_time", "input.pre_takeover"), scope="世界线C"),
    C("R33", "新需求§4.5", "D：新增第 7 人（不是原 6 人之一），第 0 秒起真实键鼠控制；动画连续无穿模悬浮抽搐；碰撞/触发/交互产生对应结果",
      "README 声明 + 首帧位置；WASD ↔ 人物在动", ("wl.d_new_person",), scope="世界线D"),
    C("R34", "确认点②、新需求§5.2", "交互动作允许（抢车/射击/攻击），但必须记录每个交互键对应的实际动作含义：文字备注写明「第几秒有 XX 行为」，中文语义，不能只写按了哪个键",
      "扫描输入路中 WASD/QE 以外的键码，逐键在因果记录里找中文含义", ("wl.interaction_semantics", "wl.causal_record"), scope="C/D 受控路"),
    C("R35", "确认点③", "优先真人操作不强制；脚本操作必须自然（不撞墙、不畸形路径），走脚本先交 demo 评估",
      "README 自述 + 人工看画面", ("wl.operator",), default=MANUAL),
    C("R36", "新需求§5.1/§5.4", "记录真实输入及组合键（同帧叠加不求和）、键码 W=87 A=65 S=83 D=68 Q=81 E=69；每路记录实际相机速度；位置/旋转/速度取自真实运行状态，不得用默认值",
      "键码合法性与组合帧；camera_speed/player_speed ↔ 位置差分；Follow Offset 语义；人物路 WASD ↔ 人物在动；"
      "自由运镜换算的键鼠 ↔ 相机运动方向与转向；固定机位键鼠全空",
      ("json.keycodes", "json.mouse_range", "json.camera_speed", "json.player_speed", "cam.follow_offset", "input.keys", "input.keys_drive_player", "input.mouse_drives_view",
       "input.path_keys", "input.path_mouse", "input.fixed_none")),
    C("R37", "新需求§5.2", "C/D 的受控人物标识与开始操作信息记录在各自第三人称路的数据中，与实际开始帧对应；不擅自改名或新增 JSON 键",
      "只能确认是否在数据里（当前只在 README）", ("wl.takeover_in_data",), default=WARN, scope="C/D 追背路"),
    C("R38", "新需求§5.3", "systeminfo：gameProcessName / x,y / width,height（与视频一致）/ recordDpi（系统缩放）",
      "字段集合与取值", ("sysinfo.schema",)),
    C("R39", "新需求§5.4、docs/72", "action_camera 逐帧字段：time/fps/frame/camera_position/camera_rotation_oula/camera_rotation_quaternion/camera_follow_offset/camera_intrinsics(fx=fy, cx/cy)/camera_speed；受控路加 player_*；metric_scale；每一路都带 keyCode、mouseX/Y∈[0,1]、mouse_dx/dy∈[-1,1]（键名与 frame 从 1 起按需方样例，同交付包）（需方 2026-09-29：自由运镜按运镜路线换算、固定机位全空）",
      "键集合精确匹配、类型维度、单位四元数、欧拉角↔四元数（Unity ZXY）、fx=fy、主点",
      ("json.container", "json.types", "json.quat_norm", "json.euler_quat", "json.player_euler_quat", "json.fx_fy", "cam.principal_point", "json.metric_scale")),
    C("R40", "新需求§3.4/§4.4、09-22 会议", "追背：三期第三人称跟随 FOV 30–55（水平/垂直未明）；第一人称贴头；追背不得中途变第一人称；主体占比不得过小（demo4-#4/#7）",
      "内参反推 FOV；Follow Offset 距离；人物像高占比", ("cam.chase_fov", "cam.first_person_head", "geo.subject_size"), scope="C/D 追背/第一人称路"),
    C("R41", "新需求§7.3", "抽检：每场景抽 2 条 video+JSON，2 条都挂退整包；另抽 5% video，通过率 <90% 退整包；抽检须关联完整四世界线",
      "批次层面协议，单组不执行", (), default=SKIP, scope="批次"),
    C("R42", "新需求§3.2/§5.5", "硬件、鼠标 DPI 1800、指针速度固定、系统缩放——元信息表层面", "人工/元信息表", ("manual.hardware",), default=MANUAL, scope="元信息"),
    # 需方 2026-09-30 对键鼠映射的补充说明（用户转述）
    C("R43", "需方 2026-09-30 ①", "C 的追背和第一人称：键鼠只从接管时刻起记录，接管前不填写、不伪造，只如实记录人物基线动作"
      "（player_position / rotation / speed、相机参数、time / frame）；接管后才有 keyCode、mouseX/Y、mouse_dx/dy",
      "接管前的帧不带键鼠字段、接管起每帧都带（字段集合逐帧核）", ("json.schema", "input.pre_takeover"), scope="C 追背/第一人称路"),
    C("R44", "需方 2026-09-30 ①-1", "真实输入路（B 自由相机、C/D 第一/第三人称）的键鼠映射须与实际操作完全一致：纯按 WASD 只能走八方向，"
      "走出斜角（如 15°）JSON 里必须同时有鼠标转动记录，不能只写方向键",
      "按 WASD 走的方向对「镜头朝向 + 键位方向」的偏差（中位/p90，允许身体滞后）；WASD/QE 与运动、鼠标与转向的相关",
      ("input.eight_directions", "input.keys_drive_camera", "input.keys_drive_player", "input.mouse_drives_view"), scope="真实输入路"),
    C("R45", "需方 2026-09-30 ②", "自由运镜全程都需要键鼠映射",
      "自由运镜每帧带键鼠字段，键位与相机运动方向一致、鼠标与转向同号（由运镜路线换算）", ("input.path_keys", "input.path_mouse"), scope="自由运镜路"),
    # 交付说明 v1.1（2026-10-16）新增 multicam_sync_i.json 及其「json 书写关注重点」
    C("R46", "交付说明 v1.1 multicam_sync", "12 路 JSON 齐全；头字段与逐帧字段齐全；video_frame 从 0 连续、条数等于视频解码帧数；"
      "tracked_entity_ids 每个单位每帧都出现，不存在时 exists=false 且状态全 null",
      "逐路读字段；条数对 action_camera；逐帧对单位清单", ("msync.fields", "msync.entities"), default=UNVERIFIED),
    C("R47", "交付说明 v1.1 multicam_sync", "各路帧数相同、logical_frame 逐帧一致；sample_time 用组内统一原点、取实际模拟时间；"
      "A&B、C、D 各一组，相同 video_frame 下组内全部动态物状态一致",
      "12 路比帧数与逻辑帧；同一遍四路逐帧全等", ("msync.group_header", "msync.time", "msync.frames", "msync.in_take"),
      default=UNVERIFIED, scope="组"),
    C("R48", "交付说明 v1.1 multicam_sync", "接管秒 N 为整数秒、C 四路相同；C 受控人物 12 路全帧存在；D 受控人物 9–12 路全帧存在、1–8 路全程不存在",
      "读 takeover；按 exists 找受控人物", ("msync.takeover", "msync.controlled"), default=UNVERIFIED, scope="组"),
    C("R49", "交付说明 v1.1 multicam_sync", "接管秒 N 前 1–8 路动态物全部一致；无事件数据除两名受控人物外 12 路全程一致（有事件数据暂不统计接管后）",
      "1 路比 5 路（接管前）、1 路比 9 路（全程，除受控人物），记最大偏差", ("msync.cross_takeover", "msync.cross_no_event"),
      default=UNVERIFIED, scope="组"),
    C("R50", "交付说明 v1.1 multicam_sync", "tracked_entity_ids 含全部动态物（人物、车辆、草、云等）；有主光源时 lighting 填方向和强度",
      "lighting 全程 null 提示；单位清单是否全靠人看 capture_note", ("msync.lighting",), default=MANUAL, scope="组"),
]


def match(clause: Clause, results: Sequence[RuleResult]) -> List[RuleResult]:
    out = []
    for r in results:
        for pat in clause.rules:
            if r.rule == pat or (pat.endswith(".") and r.rule.startswith(pat)):
                out.append(r)
                break
    return out


def clause_status(clause: Clause, matched: Sequence[RuleResult]) -> str:
    if not matched:
        return clause.default
    return min((r.status for r in matched), key=lambda s: SEVERITY.get(s, 9))


def evaluate(results: Sequence[RuleResult]) -> List[Dict[str, object]]:
    rows = []
    for c in CATALOG:
        m = match(c, results)
        counts: Dict[str, int] = {}
        for r in m:
            counts[r.status] = counts.get(r.status, 0) + 1
        rows.append({"clause": c, "status": clause_status(c, m), "matched": m, "counts": counts})
    return rows


def uncatalogued_rules(results: Sequence[RuleResult]) -> List[str]:
    """出现在结果里但没有任何条款挂住的规则 id——测试用，防止新规则写了却不进总表。"""
    seen = set()
    for r in results:
        if not any(match(c, [r]) for c in CATALOG):
            seen.add(r.rule)
    return sorted(seen)

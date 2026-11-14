"""worldline_qc 测试共用：合成四世界线交付组（「遍」布局）生成器。

坐标按 docs/69 y-up 左手系；四元数 [x,y,z,w]，旋转 (0,0,1) 得光轴（与 engine_qc.mathutil.quat_forward 一致）。
"""

import json
import math
from pathlib import Path

import helpers as H  # noqa: F401  (sys.path 引导)

from worldline_qc.checks_json import unity_euler_deg

N_DEFAULT = 150          # 5 s
CENTER = (0.0, 1.0, 0.0)
WIDTH, HEIGHT = 1920, 1080


def _norm(v):
    n = math.sqrt(sum(x * x for x in v))
    return tuple(x / n for x in v)


def _cross(a, b):
    return (a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0])


def look_at_quat(pos, target):
    """相机在 pos 看向 target 的四元数 [x,y,z,w]（列 = right/up/forward，det=+1）。"""
    f = _norm(tuple(target[i] - pos[i] for i in range(3)))
    r = _norm(_cross((0.0, 1.0, 0.0), f))
    u = _cross(f, r)
    m = [[r[0], u[0], f[0]], [r[1], u[1], f[1]], [r[2], u[2], f[2]]]
    tr = m[0][0] + m[1][1] + m[2][2]
    if tr > 0:
        s = math.sqrt(tr + 1.0) * 2
        w, x, y, z = 0.25 * s, (m[2][1] - m[1][2]) / s, (m[0][2] - m[2][0]) / s, (m[1][0] - m[0][1]) / s
    elif m[0][0] > m[1][1] and m[0][0] > m[2][2]:
        s = math.sqrt(1.0 + m[0][0] - m[1][1] - m[2][2]) * 2
        w, x, y, z = (m[2][1] - m[1][2]) / s, 0.25 * s, (m[0][1] + m[1][0]) / s, (m[0][2] + m[2][0]) / s
    elif m[1][1] > m[2][2]:
        s = math.sqrt(1.0 + m[1][1] - m[0][0] - m[2][2]) * 2
        w, x, y, z = (m[0][2] - m[2][0]) / s, (m[0][1] + m[1][0]) / s, 0.25 * s, (m[1][2] + m[2][1]) / s
    else:
        s = math.sqrt(1.0 + m[2][2] - m[0][0] - m[1][1]) * 2
        w, x, y, z = (m[1][0] - m[0][1]) / s, (m[0][2] + m[2][0]) / s, (m[1][2] + m[2][1]) / s, 0.25 * s
    return [x, y, z, w]


def yaw_quat(deg):
    r = math.radians(deg) / 2.0
    return [0.0, math.sin(r), 0.0, math.cos(r)]


def time_str(i, base="2026-09-23 12:00:00.000"):
    hh, mm, ss = 12, 0, 0
    t = i / 30.0
    ms = int(round((t - int(t)) * 1000))
    ss = int(t)
    return f"2026-09-23 {hh:02d}:{(mm + ss // 60):02d}:{ss % 60:02d}.{ms:03d}"


def frame(i, pos, q, focal_mm, follow_offset, prev_pos=None, player=None, inputs=None, fps=60.0, base_time=None):
    fx = WIDTH * focal_mm / 36.0
    vel = [0.0, 0.0, 0.0] if prev_pos is None else [(pos[k] - prev_pos[k]) * 30.0 for k in range(3)]
    f = {
        "time": time_str(i) if base_time is None else base_time(i),
        "fps": fps, "frame": i + 1,   # 交付的 frame 从 1 起（第 1 帧 = 舞台第 0 秒）
        "camera_position": [float(x) for x in pos],
        "camera_rotation_oula": unity_euler_deg(q),
        "camera_rotation_quaternion": [float(x) for x in q],
        "camera_follow_offset": [float(x) for x in follow_offset],
        "camera_intrinsics": {"fx": fx, "fy": fx, "cx": WIDTH / 2, "cy": HEIGHT / 2},
        "camera_speed": vel,
        "metric_scale": 1.0,
    }
    if player is not None:
        ppos, pq, pprev = player
        f.update({
            "player_position": [float(x) for x in ppos],
            "player_rotation_oula": unity_euler_deg(pq),
            "player_rotation_quaternion": [float(x) for x in pq],
            "player_speed": [0.0, 0.0, 0.0] if pprev is None else [(ppos[k] - pprev[k]) * 30.0 for k in range(3)],
        })
    if inputs is not None:
        keys, dx, dy = inputs
        f.update({"mouseX": 0.5, "mouseY": 0.5, "mouse_dx": dx, "mouse_dy": dy, "keyCode": list(keys)})
    return f


def _rot(q, v):
    x, y, z, w = q
    tx, ty, tz = 2 * (y * v[2] - z * v[1]), 2 * (z * v[0] - x * v[2]), 2 * (x * v[1] - y * v[0])
    return (v[0] + w * tx + (y * tz - z * ty), v[1] + w * ty + (z * tx - x * tz), v[2] + w * tz + (x * ty - y * tx))


def with_path_inputs(frames, on=0.30):
    """自由运镜：键鼠由运镜路线逐帧换算（需方 2026-09-29，stage_delivery.path_input_per_frame 的口径；这里不去短按）：
    W/S 沿相机水平朝向前后、A/D 左右、E/Q 升降；mouse_dx = 这一帧向右转的度数 / 8，mouse_dy = 低头的度数 / 8。"""
    n = len(frames)
    pos = [f["camera_position"] for f in frames]
    look = []
    for f in frames:
        fw = _rot(f["camera_rotation_quaternion"], (0.0, 0.0, 1.0))
        look.append((math.degrees(math.atan2(fw[0], fw[2])), math.degrees(math.asin(max(-1.0, min(1.0, fw[1]))))))
    for i, f in enumerate(frames):
        a, b = max(0, i - 1), min(n - 1, i + 1)
        v = [(pos[b][k] - pos[a][k]) * 30.0 / max(1, b - a) for k in range(3)]
        q = f["camera_rotation_quaternion"]
        fw, rt = _rot(q, (0.0, 0.0, 1.0)), _rot(q, (1.0, 0.0, 0.0))
        fh, rh = _norm((fw[0], 0.0, fw[2])), _norm((rt[0], 0.0, rt[2]))
        ahead, right = v[0] * fh[0] + v[2] * fh[2], v[0] * rh[0] + v[2] * rh[2]
        keys = [k for k, s in ((87, ahead), (83, -ahead), (65, -right), (68, right), (69, v[1]), (81, -v[1])) if s >= on]
        turn = (look[i][0] - look[i - 1][0] + 180.0) % 360.0 - 180.0 if i else 0.0
        tilt = look[i][1] - look[i - 1][1] if i else 0.0
        turn, tilt = (turn if abs(turn) >= 0.01 else 0.0), (tilt if abs(tilt) >= 0.01 else 0.0)
        f.update({"mouseX": 0.5, "mouseY": 0.5, "mouse_dx": turn / 8.0, "mouse_dy": -tilt / 8.0, "keyCode": keys})
    return frames


def with_still_inputs(frames):
    """固定机位：键鼠字段也带（需方 2026-09-29），全程无按键、鼠标为 0。"""
    for f in frames:
        f.update({"mouseX": 0.5, "mouseY": 0.5, "mouse_dx": 0.0, "mouse_dy": 0.0, "keyCode": []})
    return frames


# ---------------------------------------------------------------- 各角色轨迹

def fixed_lane(n, focal=35.0, base_time=None):
    pos = (0.0, 3.0, -10.0)
    q = look_at_quat(pos, CENTER)
    fo = [pos[i] - CENTER[i] for i in range(3)]
    return with_still_inputs([frame(i, pos, q, focal, fo, base_time=base_time) for i in range(n)])


def orbit_lane(n, focal=24.0, radius=8.0, base_time=None):
    out, prev = [], None
    for i in range(n):
        th = 2 * math.pi * i / (n * 2)          # 半圈
        pos = (CENTER[0] + radius * math.cos(th), 3.0, CENTER[2] + radius * math.sin(th))
        q = look_at_quat(pos, CENTER)
        out.append(frame(i, pos, q, focal, [pos[k] - CENTER[k] for k in range(3)], prev, base_time=base_time))
        prev = pos
    return with_path_inputs(out)


def dolly_lane(n, focal=50.0, base_time=None):
    out, prev = [], None
    for i in range(n):
        pos = (-3.0 + 6.0 * i / (n - 1), 2.0, -9.0)
        q = look_at_quat(pos, CENTER)
        out.append(frame(i, pos, q, focal, [pos[k] - CENTER[k] for k in range(3)], prev, base_time=base_time))
        prev = pos
    return with_path_inputs(out)


def six_dir_free_lane(n, focal=24.0, base_time=None):
    """朝向固定（identity，本体系 = 世界系），依次前/后/右/左/上/下各走 1.2 m。"""
    out, prev = [], None
    pos = [0.0, 3.0, -9.0]
    seg = max(1, n // 6)
    dirs = [(0, 0, 1), (0, 0, -1), (1, 0, 0), (-1, 0, 0), (0, 1, 0), (0, -1, 0)]
    step = 1.2 / seg
    q = [0.0, 0.0, 0.0, 1.0]
    for i in range(n):
        d = dirs[min(5, i // seg)]
        pos = [pos[k] + d[k] * step for k in range(3)]
        out.append(frame(i, tuple(pos), q, focal, [pos[k] - CENTER[k] for k in range(3)], prev, base_time=base_time))
        prev = tuple(pos)
    return with_path_inputs(out)


def b_lane(n, focal=18.0, contradict=False, use_qe=False, base_time=None):
    """B 真实输入自由相机：W 前进 / A 左移 / E 升（采集端「上 = E − Q」）；鼠标 dx>0 时 yaw 增。"""
    out, prev = [], None
    pos = [2.0, 3.0, -8.0]
    yaw = 0.0
    third = n // 3
    for i in range(n):
        keys, dx, dy = [], 0.0, 0.0
        if i < third:
            keys = [87]
            move = (0, 0, 0.05) if not contradict else (0, 0, -0.05)
        elif i < 2 * third:
            keys = [65]
            move = (-0.05, 0, 0) if not contradict else (0.05, 0, 0)
        else:
            keys = [69] if use_qe else []
            move = (0, 0.05 if use_qe else 0.0, 0)
            dx = 0.3 * math.sin(i / 5.0)
            yaw += dx * 3.0
        # 本体系 = yaw 旋转后的世界系：把 move 按 yaw 旋转
        cy, sy = math.cos(math.radians(yaw)), math.sin(math.radians(yaw))
        wm = (move[0] * cy + move[2] * sy, move[1], -move[0] * sy + move[2] * cy)
        pos = [pos[k] + wm[k] for k in range(3)]
        q = yaw_quat(yaw)
        out.append(frame(i, tuple(pos), q, focal, [0, 0, 0], prev, inputs=(keys, dx, dy), base_time=base_time))
        prev = tuple(pos)
    return out


def player_path(n, start=(-2.0, 1.0, 1.0), takeover_frame=0, attack_frames=(), skew_deg=0.0):
    """受控人物：接管后 W 前进 +x 1.5 m/s（面朝 +x，即偏航 90°）；skew_deg：只按 W 却斜着走这么多度（需方说走不出来的那种）；
    attack_frames 处按鼠标左键。"""
    pos, out = list(start), []
    for i in range(n):
        keys = []
        if i >= takeover_frame:
            keys.append(87)
            pos[0] += 1.5 / 30.0 * math.cos(math.radians(skew_deg))
            pos[2] -= 1.5 / 30.0 * math.sin(math.radians(skew_deg))
        if i in attack_frames:
            keys.append(1)
        out.append((tuple(pos), yaw_quat(90.0), keys))
    return out


def chase_lane(n, path, focal=24.0, base_time=None, input_from=0):
    """input_from：从这一帧（接管）起才带键鼠字段；之前只有相机和人物字段（需方 2026-09-30）。"""
    out, prev, pprev = [], None, None
    for i, (ppos, pq, keys) in enumerate(path):
        pos = (ppos[0] - 3.0, ppos[1] + 1.5, ppos[2])
        q = look_at_quat(pos, ppos)
        out.append(frame(i, pos, q, focal, [pos[k] - ppos[k] for k in range(3)], prev,
                         player=(ppos, pq, pprev), inputs=(keys, 0.0, 0.0) if i >= input_from else None, base_time=base_time))
        prev, pprev = pos, ppos
    return out


def first_person_lane(n, path, focal=18.0, base_time=None, input_from=0):
    out, prev, pprev = [], None, None
    for i, (ppos, pq, keys) in enumerate(path):
        pos = (ppos[0], ppos[1] + 0.7, ppos[2])
        out.append(frame(i, pos, pq, focal, [pos[k] - ppos[k] for k in range(3)], prev,
                         player=(ppos, pq, pprev), inputs=(keys, 0.0, 0.0) if i >= input_from else None, base_time=base_time))
        prev, pprev = pos, ppos
    return out


README_TMPL = """# 四世界线交付：{name}

| 文件夹 | 路 | 世界线 | 相机 | 焦距 |
|---|---|---|---|---|
| AB | 1 | A | 固定机位 | 35 mm |
| AB | 2 | A | 环绕 | 24 mm |
| AB | 3 | A | 横移 | 50 mm |
| AB | 4 | B | 自由相机（真人键鼠操控） | 18 mm |
| C | 1 | C | 固定机位 | 35 mm |
| C | 2 | C | 环绕 | 24 mm |
| C | 3 | C | 跟拍（游戏镜头） | 24 mm |
| C | 4 | C | 第一人称 | 18 mm |
| D | 1 | D | 固定机位 | 35 mm |
| D | 2 | D | 环绕 | 24 mm |
| D | 3 | D | 跟拍（游戏镜头） | 24 mm |
| D | 4 | D | 第一人称 | 18 mm |

## 每条世界线发生了什么

时间写视频时间；视频开头有 1 秒开拍前的静止段，所以视频时间 = 剧本时间 + 1 秒。

- **A**：没有人干预，场上的人照剧本演。
- **B**：与 A 是同一遍、同一个世界；一个人用键鼠飞 AB 文件夹第 4 路的自由相机。
- **C**：{c_text}
- **D**：视频第 1.0 秒 第 0 秒起由玩家操控新增的第 7 人（a_m_y_beach_02）；视频第 2.5 秒 玩家挡住慢跑者的路，慢跑者停下。
"""

# 交付包（stage_delivery.render_readme）的写法：每条世界线一节，节内分条；视频从舞台第 0 秒开始，没有静止段
DELIVERY_WORLDLINES = """## 每条世界线发生了什么

### A（编号 1–3，录制遍 AB）
- 没有人干预，场上的人照剧本演。
- 时间线：没有因果规则触发，也没有交互操作。

### B（编号 4，录制遍 AB）
- B 与 A 出自同一遍录制：程序用键鼠飞编号 4 的自由相机，没有受控人物。

### C（编号 5–8，录制遍 C）
- 受控人物与控制开始：演员 `pacer`，第 2 秒（第 61 帧）起由程序键鼠接管。
- 原事件是否被改变：**是**（触发了因果规则 block_jogger）。
- 时间线：
  - 第 2.5 秒 玩家挡住慢跑者的路，慢跑者停下（规则 block_jogger）
  - 第 3.0 秒 玩家挥拳攻击路人，路人后退

### D（编号 9–12，录制遍 D）
- 受控人物与控制开始：新增的第 7 人（模型 `a_m_y_beach_02`），第 0 秒（第 1 帧）起由程序键鼠操控。
- 原事件是否被改变：**否**（没有触发因果规则，也没有攻击类操作）。
- 时间线：没有因果规则触发，也没有交互操作。

时间线里标「规则」的是因果规则：第 9 秒的说明不属于任何一条世界线。

## 字段口径
"""

C_TEXT_NO_SEMANTICS = "视频第 2.0 秒 第 1 秒玩家接管 pacer，此后此人由真人键鼠操控；视频第 3.5 秒 玩家挡住慢跑者的路，慢跑者停下。"
C_TEXT_WITH_SEMANTICS = C_TEXT_NO_SEMANTICS + " 视频第 3.0 秒 玩家挥拳攻击路人，路人后退。"

SYSINFO = {"gameProcessName": "GTA5.exe", "x": 0, "y": 0, "width": WIDTH, "height": HEIGHT, "recordDpi": 1.0}


def _yaw(deg):
    h = math.radians(deg) / 2.0
    return [0.0, math.sin(h), 0.0, math.cos(h)]


def multicam_worlds(n, takeover=60, jitter_m=0.0):
    """三遍各自的 multicam_sync 逐帧世界（交付说明 v1.1）：chat_a、pacer、jogger 照剧本走，第 7 人只在 D；C 在 takeover
    帧接管 pacer，此后 pacer 由按键驱动（位置不同、animation null）。jitter_m：C 里没被接管的人整体偏一点（分开录的误差）。"""
    worlds = {}
    for take in ("AB", "C", "D"):
        frames = []
        for k in range(n):
            ents = []
            for j, ident in enumerate(("chat_a", "pacer", "jogger")):
                played = take == "C" and ident == "pacer" and k >= takeover
                pos = [5.0, 0.0, 0.05 * k] if played else [float(j) + (jitter_m if take == "C" else 0.0), 0.0, 0.1 * k]
                ents.append({"id": ident, "exists": True, "position": pos, "rotation": _yaw(10 * j),
                             "animation": None if played else {"id": f"{ident}:walk", "elapsed_time": round(k / 30, 4)}})
            here = take == "D"
            ents.append({"id": "seventh", "exists": here, "position": [3.0, 0.0, 2.0 + 0.02 * k] if here else None,
                         "rotation": _yaw(90) if here else None, "animation": None})
            frames.append({"video_frame": k, "logical_frame": k, "sample_time": round(k * 0.0334, 6), "entities": ents,
                           "lighting": None})
        worlds[take] = frames
    return worlds


def multicam_doc(number, frames, takeover=None, group="g"):
    return {"schema_version": "1.0", "capture_group_id": group, "camera_id": f"video_{number}",
            "video_file": f"video_{number}.mp4", "fps": 30,
            "coordinate_system": {"space": "world", "unit": "meter", "up_axis": "+Y", "forward_axis": "+Z",
                                  "handedness": "left", "quaternion_order": "xyzw"},
            "time_unit": "second", "capture_note": "合成样本", "tracked_entity_ids": ["chat_a", "pacer", "jogger", "seventh"],
            "takeover": None if takeover is None else [{"logical_frame": takeover, "phase": "before_update",
                                                        "video_frame": takeover, "entity_ids": ["pacer"],
                                                        "comparison_note": "接管后 pacer 允许不同"}],
            "frames": frames}


def write_multicam(root: Path, n=N_DEFAULT, takeover=60, jitter_m=0.0, edit=None):
    """「遍」布局下每路的 multicam_sync_<路>.json。edit(take, lane, doc)：写出前改一份（造坏样本）。"""
    worlds = multicam_worlds(n, takeover, jitter_m)
    for take in ("AB", "C", "D"):
        for lane in range(1, 5):
            doc = multicam_doc(lane, json.loads(json.dumps(worlds[take])), takeover if take == "C" else None)
            if edit:
                edit(take, lane, doc)
            (root / take / f"multicam_sync_{lane}.json").write_text(json.dumps(doc), encoding="utf-8")


def write_group(root: Path, n=N_DEFAULT, c_text=C_TEXT_NO_SEMANTICS, attack=True, distinct_clock=False, orbit_is_six_dir=False,
                b_contradict=False, b_use_qe=False, with_readme=True, videos=None, multicam=True):
    """写一个「遍」布局组。videos: 可选的 mp4 路径，会复制到每路 video_N.mp4。返回 root。"""
    root.mkdir(parents=True, exist_ok=True)
    offs = {"AB": 0, "C": 3600 if distinct_clock else 0, "D": 7200 if distinct_clock else 0}

    def bt(take):
        return (lambda i: time_str(i + offs[take])) if distinct_clock else None

    takeover = 60                                                   # 视频 2.0 s
    attack_frames = set(range(90, 96)) if attack else set()
    c_path = player_path(n, takeover_frame=takeover, attack_frames=attack_frames)
    d_path = player_path(n, start=(3.0, 1.0, 2.0), takeover_frame=30)
    lanes = {
        "AB": {1: fixed_lane(n, base_time=bt("AB")),
               2: (six_dir_free_lane(n, base_time=bt("AB")) if orbit_is_six_dir else orbit_lane(n, base_time=bt("AB"))),
               3: dolly_lane(n, base_time=bt("AB")),
               4: b_lane(n, contradict=b_contradict, use_qe=b_use_qe, base_time=bt("AB"))},
        "C": {1: fixed_lane(n, base_time=bt("C")), 2: orbit_lane(n, base_time=bt("C")),
              3: chase_lane(n, c_path, base_time=bt("C"), input_from=takeover),
              4: first_person_lane(n, c_path, base_time=bt("C"), input_from=takeover)},
        "D": {1: fixed_lane(n, base_time=bt("D")), 2: orbit_lane(n, base_time=bt("D")),
              3: chase_lane(n, d_path, base_time=bt("D"), input_from=30),
              4: first_person_lane(n, d_path, base_time=bt("D"), input_from=30)},
    }
    for take, d in lanes.items():
        (root / take).mkdir(exist_ok=True)
        for lane, frames in d.items():
            (root / take / f"action_camera_{lane}.json").write_text(json.dumps(frames), encoding="utf-8")
            (root / take / f"systeminfo_{lane}.json").write_text(json.dumps(SYSINFO), encoding="utf-8")
            if videos:
                import shutil
                shutil.copyfile(videos, root / take / f"video_{lane}.mp4")
    if multicam:
        write_multicam(root, n, takeover=takeover)
    if with_readme:
        (root / "README.md").write_text(README_TMPL.format(name=root.name, c_text=c_text), encoding="utf-8")
    manifest = {"schema": "dm-gta-four-worldline-delivery/1", "group_id": root.name, "internal": True, "delivery_eligible": False,
                "timestamp_semantics": "source_replay_timeline" if not distinct_clock else "wall_clock",
                "worldlines": {"AB": {"1": "A", "2": "A", "3": "A", "4": "B"}, "C": {str(i): "C" for i in range(1, 5)}, "D": {str(i): "D" for i in range(1, 5)}},
                "files": {}}
    (root / "manifest.json").write_text(json.dumps(manifest), encoding="utf-8")
    return root

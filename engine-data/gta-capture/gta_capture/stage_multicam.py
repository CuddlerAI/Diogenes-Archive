"""multicam_sync_i.json: each delivered lane's frame-by-frame world (the customer's delivery doc v1.1, 2026-10-16).

The customer checks with it that the twelve videos' frames line up in logical time and that the world under them
is the same where it should be: every moving thing of the group, every frame -- where it is, which way it faces,
what it is playing -- with the frame's own game time. One take is one world, so a take's four lanes carry the same
frames; only the camera id and the video's name differ. Everything comes from what the take logged while it played
(source/master-world-telemetry.jsonl, a row a stage tick after all of the tick's updates, and source/stage-plan.txt,
the actors' beats as the take ran them): nothing is worked out from a frame rate.

    video_frame    the video's frame, from 0: stage tick f (the hold before tick 0 is cut from every video)
    logical_frame  the stage tick: the same number in all twelve lanes for the same moment of the script
    sample_time    the stage clock: tick k at k/30 s, the time the script poses every actor at, the same in every take
                   for the same tick. Till 2026-10-19 it was the game's own clock at the tick (the logged game_ms), whose
                   ticks run 30-36 ms: two takes then differed by 1-3 ms at the same tick, and the customer's check
                   (cross-take consistency before the takeover) failed beach-demo-113 on it. The game's tick lengths go
                   into capture_note.
    entities       every tracked id every frame (exists false, with position/rotation/animation null, where it is not
                   in this take's world: the seventh person outside D)
    lighting       the sun, the same every frame (the clock stands at the group's hour): {sun_direction, intensity}
                   from the group's environment.sun (sun_lighting), null where a group has none

Ids: the stage's actors by their script names, the seventh person as SEVENTH_ID, the props people hold as
"<actor>:<model>", and the map's own moving things the stage tracks (spec `track` lines, 2026-10-16: the pier's ferris
wheel, the beach's flags) by their track ids, turned whole (a quaternion as logged, not only about the up axis). The played person (C's taken actor after the takeover, D's seventh person) is moved by the keys:
its animation is the game's own walk and stand, which the take does not log -- null, said in capture_note.
"""

from __future__ import annotations

import json
import math
from pathlib import Path

from .errors import ContractError
from .evidence import _gta_to_delivery_quaternion, _gta_to_delivery_vector
from .jsonio import write_json_atomic

SCHEMA_VERSION = "1.0"
SEVENTH_ID = "seventh"
TICK_HZ = 30
SUN_NEAR_M = 300.0   # a measured sun holds this far from where it was measured (the same stretch of beach)
REACTION_ID = "rule_reaction"   # an actor a rule has taken off its script (it turns to the person and watches them)
COORDINATE_SYSTEM = {"space": "world", "unit": "meter", "up_axis": "+Y", "forward_axis": "+Z", "handedness": "left",
                     "quaternion_order": "xyzw",
                     "transform_from_gta": "position=[x,z,y]; quaternion=[-x,-z,-y,w] (GTA is z-up, right-handed); "
                                           "the same world and axes as action_camera_i.json"}


def _rows(path: Path) -> list[dict]:
    with open(path, encoding="utf-8") as f:
        return [json.loads(line) for line in f if line.strip()]


def plan_beats(plan_path: Path) -> dict[str, list[str]]:
    """Each actor's beats as the take ran them, in order: the clip each plays (stage-plan.txt: `actor <id>`, then
    a line `<kind> <clip> start <S> ticks <T> ... blend <B>` a beat)."""
    beats: dict[str, list[str]] = {}
    actor = None
    with open(plan_path, encoding="utf-8") as f:
        for line in f:
            w = line.split()
            if w[:1] == ["actor"] and len(w) >= 2:
                actor = w[1]
                beats.setdefault(actor, [])
            elif actor and len(w) >= 4 and w[0] in ("idle", "walk", "once", "pair") and w[2] == "start":
                beats[actor].append(w[1])
    return beats


def tracked_ids(take_dirs: list[Path]) -> list[str]:
    """The group's moving things, the same list in all twelve files: the actors and the props they hold, as the takes
    logged them (in the order first met), and the seventh person."""
    actors, props, things = {}, {}, {}
    for take_dir in take_dirs:
        for row in _rows(Path(take_dir) / "source" / "master-world-telemetry.jsonl")[:1]:
            for a in row.get("actors", []):
                actors.setdefault(a["id"], None)
            for p in row.get("props", []):
                props.setdefault(p["id"], None)
            for m in row.get("map", []):
                things.setdefault(m["id"], None)
    return [*actors, SEVENTH_ID, *props, *things]


def _quaternion(heading_deg: float) -> list[float]:
    """The delivery quaternion of a thing facing GTA heading h (turned about the up axis only: the take logs no tilt)."""
    h = math.radians(heading_deg) / 2.0
    return [round(v, 9) for v in _gta_to_delivery_quaternion([0.0, 0.0, math.sin(h), math.cos(h)])]


def _vec(p) -> list[float]:
    return [round(float(v), 6) for v in _gta_to_delivery_vector(p)]


def sun_lighting(env: dict) -> dict | None:
    """The frames' lighting from the group's environment.sun -- {light: the way the sunlight goes, GTA axes (x east,
    y north, z up), intensity, clock, weather, near: [x, y] where it was measured, measured: how} -- or None without
    one. The game has no native that reads the sun: it is measured from a delivery's frames (the shadows of people
    standing still, 2026-10-18), and holds only for the hour, the weather and the place it was measured at, where the
    clock is paused (the take sets it and stops it: the sun stands still all take)."""
    sun = env.get("sun")
    if sun is None:
        return None
    light = sun.get("light")
    if not (isinstance(light, list) and len(light) == 3 and all(isinstance(v, (int, float)) for v in light)
            and abs(math.sqrt(sum(v * v for v in light)) - 1.0) < 1e-3 and light[2] < 0):
        raise ContractError("environment.sun.light must be a unit vector pointing down (GTA axes, z up)")
    intensity = sun.get("intensity")
    if not isinstance(intensity, (int, float)) or intensity <= 0:
        raise ContractError("environment.sun.intensity must be a positive number")
    near = sun.get("near") or []
    if (sun.get("clock") != env.get("clock") or sun.get("weather") != env.get("weather") or len(near) != 2
            or math.dist(near, env["location"][:2]) > SUN_NEAR_M):
        raise ContractError(f"environment.sun was measured at {sun.get('clock')} {sun.get('weather')} near {near}, the group "
                            f"plays at {env.get('clock')} {env.get('weather')} at {env['location'][:2]}: measure the sun "
                            "again from this group's frames (STAGE-AUTHORING-GUIDE.md)")
    return {"sun_direction": [round(v, 4) for v in _gta_to_delivery_vector(light)], "intensity": float(intensity)}


def take_frames(take_dir, *, kind: str, ids: list[str], n: int, played: str | None, takeover_tick: int | None,
                lighting: dict | None = None) -> tuple[list[dict], dict]:
    """The take's n frames (stage ticks 0..n-1) for its multicam_sync files, and what capture_note says of it: the
    skipped ticks, interpolated. lighting: every frame's (sun_lighting)."""
    take_dir = Path(take_dir)
    world = {int(r["tick"]): r for r in _rows(take_dir / "source" / "master-world-telemetry.jsonl")}
    if not world:
        raise ContractError(f"take {kind}: no world telemetry for its multicam_sync")
    beats = plan_beats(take_dir / "source" / "stage-plan.txt")
    known = sorted(world)
    skipped = [k for k in range(n) if k not in world]
    began: dict[tuple[str, int], int] = {}   # (actor, beat) -> the tick it began, as logged
    released_at: dict[str, int] = {}
    frames = []
    j = 0
    for k in range(n):
        while j + 1 < len(known) and known[j + 1] <= k:
            j += 1
        row = world.get(k) or world[known[j]]   # a skipped tick: its neighbour's world
        entities: dict[str, dict] = {}
        for a in row.get("actors", []):
            st = a.get("state")
            mover = played is not None and a["id"] == played and takeover_tick is not None and k >= takeover_tick
            if mover and row.get("player"):
                st = row["player"]   # C: the taken actor is logged as the player once played
            if not st:
                continue
            anim = None
            if not mover:
                b = int(a.get("beat", -1))
                if a.get("released"):
                    released_at.setdefault(a["id"], k)
                    anim = {"id": REACTION_ID, "elapsed_time": round((k - released_at[a["id"]]) / TICK_HZ, 4)}
                else:
                    released_at.pop(a["id"], None)
                    clips = beats.get(a["id"], [])
                    if 0 <= b < len(clips):
                        began.setdefault((a["id"], b), k)
                        anim = {"id": f"{a['id']}:{clips[b]}", "beat": b,
                                "elapsed_time": round((k - began[(a["id"], b)]) / TICK_HZ, 4)}
                        if st.get("phase") is not None:
                            anim["phase"] = round(float(st["phase"]), 6)
            entities[a["id"]] = {"id": a["id"], "exists": True, "position": _vec(st["pos"]),
                                 "rotation": _quaternion(float(st["h"])), "animation": anim}
        if kind == "D" and row.get("player"):
            st = row["player"]
            entities[SEVENTH_ID] = {"id": SEVENTH_ID, "exists": True, "position": _vec(st["pos"]),
                                    "rotation": _quaternion(float(st["h"])), "animation": None}
        for p in row.get("props", []):
            st = p.get("state")
            if st:
                entities[p["id"]] = {"id": p["id"], "exists": True, "position": _vec(st["pos"]),
                                     "rotation": _quaternion(float(st["h"])), "animation": None}
        for m in row.get("map", []):   # the map's own: turned whole, as logged
            st = m.get("state")
            if st:
                entities[m["id"]] = {"id": m["id"], "exists": True, "position": _vec(st["pos"]),
                                     "rotation": [round(v, 9) for v in _gta_to_delivery_quaternion(st["quat"])], "animation": None}
        frames.append({"video_frame": k, "logical_frame": k, "sample_time": round(k / TICK_HZ, 6),
                       "entities": [entities.get(i) or {"id": i, "exists": False, "position": None, "rotation": None,
                                                        "animation": None} for i in ids],
                       "lighting": lighting})
    # the game's own clock, for capture_note: how long its ticks ran (consecutive logged ticks only)
    ms = {k: float(world[k]["game_ms"]) for k in known if world[k].get("game_ms") is not None}
    steps = [ms[k + 1] - ms[k] for k in sorted(ms) if k + 1 in ms]
    first = world[known[0]]
    return [{key: f[key] for key in ("video_frame", "logical_frame", "sample_time", "entities", "lighting")}
            for f in frames], {"skipped_ticks": skipped, "tracked_map": [m["id"] for m in first.get("map", []) if m["id"] in ids],
                               "wind_speed": (first.get("env") or {}).get("wind_speed"),
                               "game_tick_ms": [round(min(steps)), round(max(steps))] if steps else None}


def _lighting_zh(lighting: dict | None, env: dict | None) -> str:
    if lighting is None:
        return "lighting：本版未记录太阳方向和强度（录制程序待加），填 null；游戏时间固定在 12:00、天气 EXTRASUNNY"
    sun = env["sun"]
    d = lighting["sun_direction"]
    elevation = math.degrees(math.asin(-d[1]))
    azimuth = math.degrees(math.atan2(-d[0], -d[2])) % 360.0   # where the sun is, from north (+z) toward east (+x)
    return (f"lighting：主光源是太阳。游戏时间固定在 {env['clock']}（时钟暂停）、天气 {env['weather']}，太阳全程不动，每帧填同一值；"
            f"sun_direction 是阳光前进的方向（从太阳指向地面）的单位向量，坐标系同上（y 朝上、+z 朝北、+x 朝东），本组为 {d}，"
            f"即太阳高度 {elevation:.1f}°、方位从正北顺时针 {azimuth:.1f}°（南偏东 {180.0 - azimuth:.0f}°）；"
            f"游戏不提供读取太阳方向的接口，此值从导出画面量得：{sun['measured']}；"
            f"intensity 为项目统一的相对强度（晴天正午阳光 = {lighting['intensity']:g}），不是游戏内部的物理量")


def capture_note(kind: str, *, skipped: list[int], played: str | None, takeover_tick: int | None,
                 tracked_map: list[str] | None = None, wind_speed: float | None = None, floats: str = "",
                 lighting: dict | None = None, env: dict | None = None, game_tick_ms: list[int] | None = None) -> str:
    """floats: what the take did with the things floating in the sea (2026-10-18) -- "cleared", the rope floats and
    the boats taken out before it recorded (the rest the map's own, bobbing as the export plays them); "untracked", all
    there and bobbing, the floats it tracked left out of the files (what it logged of them is not what the export shows);
    "" no sea."""
    parts = [
        "同一遍录制的四路共用一个世界：每一拍（1/30 秒，逻辑步长）全部状态更新之后采样一次，video_frame = 去掉开头等待段后的视频帧号 = "
        "logical_frame = 舞台第几拍（12 路相同的 logical_frame 是剧本的同一时刻）；sample_time 是舞台时钟：第 k 拍 = k/30 秒，以第 0 拍为 0。"
        "它就是驱动剧本的模拟时间——录制时舞台逐拍推进，每个剧本人物的位置、朝向、动作进度都由拍数决定，所以同一拍在 12 路完全相同，"
        "不是从视频帧率推算的"
        + (f"；游戏自身的计时每拍在 {game_tick_ms[0]}–{game_tick_ms[1]} 毫秒之间抖动（逐拍记录在录制日志里），只影响由游戏自己推进的"
           "受控人物走路和旗子飘动" if game_tick_ms else "")
        + "；无插值、无帧内多时间样本",
        "position、rotation 与 action_camera_i.json 同一世界坐标（由 GTA 的 z 朝上换成 y 朝上：position=[x,z,y]，"
        "quaternion=[-x,-z,-y,w]）；rotation 只含绕竖直轴的朝向（录制只记人物朝向，不记前后左右倾斜）",
        "animation：剧本里的动作写「演员:动作名」，elapsed_time 是这一段动作从开始到这一帧已播放的秒数（循环动作累计），phase 是当帧在动作一个循环里的进度（0–1）；"
        f"被规则打断原有行为的演员（转身看受控人物、之后走回原位）写 {REACTION_ID}，elapsed_time 从被打断那一帧算起",
        "tracked_entity_ids：6 个剧本人物、新增的第 7 人（seventh，只在世界线 D 存在，其余各路 exists=false）、人物手里的道具（演员:模型）"
        + (f"、地图上会动的物体（{'、'.join(tracked_map)}：逐帧记录位置和完整旋转；旗子记录的是旗杆，旗面布料飘成什么形状游戏读不出来，未记录；"
           "码头摩天轮在故事模式里是地图本身的一部分，不是游戏能读出的物体，未记录）" if tracked_map else "")
        + "。舞台上摆放的陈设（伞、毛巾、冷藏箱、沙滩车、救生塔）开录前就固定不动，不列入；"
        + ("海上随浪漂的游泳区浮球绳和码头下停着的摩托艇、快艇开录前已从游戏里移除，全程不在画面里，不列入；"
           "远处海上的红色浮标、码头下的木浮台和摩托艇跳台是地图自带的，导出时游戏按自己的海浪让它们上下起伏、轻微摇摆"
           "（各路之间不保证完全一致），录制程序读不到导出时的样子，未记录；" if floats == "cleared" else
           "海上的游泳区浮球绳、红色浮标、码头下的木浮台、摩托艇跳台和停在浮台边的摩托艇、快艇随浪起伏，导出时由游戏按自己的海浪模拟"
           "（各路之间不保证完全一致），录制程序读不到导出时的样子，未记录；" if floats == "untracked" else "")
        + ("草、云、海浪的状态未记录，不以空值冒充" if tracked_map else "旗子、摩天轮、草、云、海浪、风的状态本版未记录（录制程序待加），不以空值冒充")
        + (f"；风：每遍开录前设成相同的风（录制时读到风速约 {wind_speed:.1f} m/s），逐拍记录在内部日志" if wind_speed else
           "；风：录制时关掉了风（风速 0）"),
        _lighting_zh(lighting, env),
    ]
    if played:
        parts.append(f"受控人物 {played} 由按键驱动行走、站立，游戏自己的走路/站立动作不在日志里，animation 填 null")
    if kind == "C" and takeover_tick is not None:
        parts.append(f"第 {takeover_tick} 拍（{takeover_tick / TICK_HZ:g} 秒）接管 {played}：此前它照剧本演，此后由按键控制，位置、朝向、动作与世界线 A 不同；"
                     "被它影响的人物（rule_reaction）在反应期间也不同")
    if kind == "D":
        parts.append("第 7 人（seventh）从第 0 拍起由按键控制，只在本世界线存在；没有接管原有人物（takeover 为 null）")
    if skipped:
        parts.append(f"游戏跳过了 {len(skipped)} 拍（以 1/15 秒推进一次）：第 {', '.join(map(str, skipped[:20]))}"
                     f"{' …' if len(skipped) > 20 else ''} 拍的状态取相邻一拍（第 0 拍取第 1 拍，其余取前一拍）")
    return "；".join(parts) + "。"


def write_lane_files(out_dir, *, kind: str, numbers: list[int], capture_group_id: str, ids: list[str], frames: list[dict],
                     note: str, played: str | None, takeover_tick: int | None) -> dict[int, Path]:
    """The take's multicam_sync_i.json, one for each of its delivered numbers i."""
    takeover = None
    if kind == "C" and takeover_tick is not None:
        takeover = [{"logical_frame": takeover_tick, "phase": "before_update", "video_frame": takeover_tick,
                     "entity_ids": [played],
                     "comparison_note": f"从 logical_frame={takeover_tick} 更新前开始接管 {played}；接管后 {played} 的 position、"
                                        "rotation、animation 允许不同，被它打断原有行为的人物（animation 为 rule_reaction）在反应和走回原位期间也允许不同；"
                                        "其余人物与世界线 A 同一拍应一致。"}]
    paths = {}
    for i in numbers:
        path = Path(out_dir) / f"multicam_sync_{i}.json"
        write_json_atomic(path, {"schema_version": SCHEMA_VERSION, "capture_group_id": capture_group_id,
                                 "camera_id": f"video_{i}", "video_file": f"video_{i}.mp4", "fps": 30,
                                 "coordinate_system": COORDINATE_SYSTEM, "time_unit": "second", "capture_note": note,
                                 "tracked_entity_ids": ids, "takeover": takeover, "frames": frames}, indent=None)
        paths[i] = path
    return paths

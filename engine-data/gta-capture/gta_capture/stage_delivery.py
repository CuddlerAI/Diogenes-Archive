"""A four-worldline group as the customer's delivery folder (docs/69 §7, issue #958, 交付说明).

A group is one folder, named by the group's `delivery_name` (YYYY_MM_DD_<scene>_<NNN>),
holding exactly 48 files: video_i.mp4, systeminfo_i.json, action_camera_i.json and
multicam_sync_i.json (since the delivery doc v1.1, 2026-10-16; stage_multicam) for i = 1..12.
The three takes fill the numbers:

    AB   1-3 worldline A (fixed / free path / free path); 4 worldline B, the free
         camera a person flew while A played
    C    5-8 worldline C: a person takes over one of the six at second N
    D    9-12 worldline D: a person plays a seventh from second 0
         (C and D: fixed / free path / chase / first person)

The four lanes of one take are frame-synchronized. The manifest and the README
sit beside the folder (<name>.manifest.json, <name>.README.md), not in it.

Frame 0 of every video and JSON is stage tick 0: the tape's hold frames before it
(the still second the export renders) are cut when the lanes are encoded and left
out here, so frame f is tick f. Camera fields, in every lane, are the camera each
frame was actually rendered with (evidence/frame-events-laneN.jsonl, sealed by the
per-frame fence), put in the docs/69 basis as the mc2 delivery does; the Euler
angles are Unity's eulerAngles, the team's rule (docs/72). The requirement gives
person and input fields to both person lanes of C and D - chase and first person,
the same person and the same input frame by frame - and B's free camera its input
but no person: nobody is played there. Keys, mouse, the game's frame rate and the
system time come from the take's own input log, one row per tick; a tick the game
skipped has no row: it keeps the keys held, adds no mouse movement, and its time
lies between its neighbours'.
"""

from __future__ import annotations

import bisect
import json
import math
import re
import shutil
import subprocess
from datetime import datetime, timedelta
from pathlib import Path

from .errors import ContractError
from .evidence import (DELIVERY_COORDINATE_CONVENTION, _gta_to_delivery_quaternion, _gta_to_delivery_vector,
                       _round_vec, _speed)
from .jsonio import load_json, sha256_file, write_json_atomic
from .stage_export import load_lane_result
from .stage_group import take_result
from .stage_camera_paths import LABELS as MOVE_LABELS
from .stage_multicam import SEVENTH_ID, capture_note, sun_lighting, take_frames, tracked_ids, write_lane_files
from .stage_take import TICK_HZ
from .stage_tape import (CHASE_MAX_ACCEL, CHASE_MAX_RATE, EYE_AHEAD_M, FOLLOW_BACK_M, FOLLOW_PERSON_M, FOLLOW_RIGHT_M,
                         FOLLOW_UP_M, _to_world, stage_frame)

TAKES = ("AB", "C", "D")
WORLDLINES = {"AB": {1: "A", 2: "A", 3: "A", 4: "B"}, "C": dict.fromkeys(range(1, 5), "C"),
              "D": dict.fromkeys(range(1, 5), "D")}
FIRST_NUMBER = {"AB": 1, "C": 5, "D": 9}   # the customer's numbers: A 1-3, B 4, C 5-8, D 9-12
# 48 files since the delivery doc v1.1 (2026-10-16): each lane's multicam_sync_i.json besides its video, systeminfo
# and action_camera (stage_multicam)
DELIVERY_FILES = tuple(f"{stem}_{i}{ext}" for i in range(1, 13)
                       for stem, ext in (("video", ".mp4"), ("systeminfo", ".json"), ("action_camera", ".json"),
                                         ("multicam_sync", ".json")))
PERSON_ROLES = ("chase", "first_person")                   # lanes that carry the played person
INPUT_ROLES = ("free_camera", "chase", "first_person")     # lanes a person's keys and mouse drive
PATH_INPUT_ROLES = ("free_path",)                          # lanes whose keys and mouse are worked out from their path
STILL_ROLES = ("fixed",)   # lanes that never move: their keys and mouse are none (every lane carries them, the customer 2026-09-29)
ROLE_ZH = {"fixed": "固定机位", "free_path": "自由运镜", "free_camera": "真实输入自由相机",
           "chase": "第三人称追背", "first_person": "第一人称"}
FOCUS_SCHEMA = "dm-gta-stage-camera-focus/1"
FOCUS_ZH = {"centroid": "看共视焦点", "player": "围绕受控第7人", "event": "看事件焦点"}   # what a free-path lane looks at
INPUT_LOG_SCHEMA = "dm-gta-input-camera-log/1"
DELIVERY_NAME = re.compile(r"^\d{4}_\d{2}_\d{2}_.+_\d{3}$")
WALL = re.compile(r"^\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}\.\d{3}$")
TICK_MS = 1000.0 / TICK_HZ


def number(kind: str, lane: int) -> int:
    """A lane's number in the delivery folder."""
    return FIRST_NUMBER[kind] + lane - 1


def _rows(path: Path) -> list[dict]:
    with open(path, encoding="utf-8") as f:
        return [json.loads(line) for line in f if line.strip()]


def _clamp(value, lo: float = -1.0, hi: float = 1.0) -> float:
    number = float(value)
    if not math.isfinite(number):
        raise ContractError("input log holds a non-finite mouse value")
    return max(lo, min(hi, number))


def stage_target(spec_text: str) -> list[float]:
    """The point the fixed camera of a tape without its own aim (source.fixed, built
    before 2026-09-24) aims at: the stage centre, 1.6 m in from the origin, 1 m up."""
    frame = stage_frame(spec_text)
    x, y = _to_world(frame, 0.0, 1.6)
    return [x, y, frame[2] + 1.0]


def _log_ticks(rows: list[dict]) -> dict[int, dict]:
    """The take's input log by stage tick; a tick the game skipped has no row."""
    by_tick: dict[int, dict] = {}
    for row in rows:
        if row.get("schema") != INPUT_LOG_SCHEMA:
            raise ContractError("input log row has the wrong schema")
        tick = int(row["tick"])
        if tick < 0:
            raise ContractError(f"input log has tick {tick}")
        if tick in by_tick:
            raise ContractError(f"input log repeats tick {tick}")
        by_tick[tick] = row
    if not by_tick:
        raise ContractError("the input log is empty")
    return by_tick


def input_per_frame(rows: list[dict], n: int) -> list[dict]:
    """keyCode / mouse for each delivered frame (frame f = stage tick f) from the take's input log."""
    by_tick = _log_ticks(rows)
    out, held = [], []
    for f in range(n):
        row = by_tick.get(f)
        if row is None:   # a skipped tick keeps the keys held
            out.append({"keyCode": list(held), "mouse_dx": 0.0, "mouse_dy": 0.0})
            continue
        held = [int(k) for k in row.get("keyCode", [])]
        out.append({"keyCode": held, "mouse_dx": round(_clamp(row.get("mouse_dx", 0.0)), 6),
                    "mouse_dy": round(_clamp(row.get("mouse_dy", 0.0)), 6)})
    return out


# The free paths' keys and mouse (the customer, 2026-09-29). A free-path lane is driven by no
# key: its camera is set frame by frame on a planned path. The customer asked for its keyCode
# and mouse all the same, worked out from that path in worldline B's keys: W/S forward and back
# along the camera's level heading, A/D left and right, E/Q up and down, and the turn and pitch
# in B's mouse units. A key says which way the camera moves, not how fast (a path's speed varies;
# B's keys fly at a fixed 2.0 m/s) -- agreed with the customer, 2026-09-29.
# A key is held only while the move shows in the picture (the customer, 2026-09-30: number 6 carried keys
# where its camera did not visibly move). Measured on beach-demo-003's free paths (50 mm): a fixed world
# shifts 0.06 / 0.23 / 0.5 / 0.8 / 1.5 / 2.8 px a frame at 0.05 / 0.1 / 0.2 / 0.3 / 0.5 / 1 m/s; held from
# 0.05 m/s, 14% of number 6's key frames moved under 0.3 px (a still picture) -- from 0.3 m/s, none.
PATH_KEY_ON_MPS = 0.30       # moving a way this fast holds that way's key ...
PATH_KEY_OFF_MPS = 0.20      # ... until it slows under this: no flicker
PATH_MIN_PRESS_FRAMES = 12   # a key held (or let go) under 0.4 s is not kept: a hand does not tap like that
PATH_TURN_MIN_DEG = 0.01     # a turn or tilt under this a frame (0.3 deg/s: under half a pixel at 50 mm) is no mouse
LOOK_DEG_PER_UNIT = 8.0      # degrees a unit of mouse_dx / mouse_dy turns B's free camera (stage_take_win.cpp fly())
PATH_KEYS = ((87, "forward"), (83, "back"), (65, "left"), (68, "right"), (69, "up"), (81, "down"))   # W S A D E Q


def _heading_pitch(q) -> tuple[float, float]:
    """GTA heading (a left turn adds) and pitch (up adds) of a camera quaternion [x, y, z, w]:
    the way it turns +y, the camera's forward."""
    x, y, z, w = (float(v) for v in q)
    fx, fy, fz = 2.0 * (x * y - w * z), 1.0 - 2.0 * (x * x + z * z), 2.0 * (y * z + w * x)
    return math.degrees(math.atan2(-fx, fy)), math.degrees(math.asin(max(-1.0, min(1.0, fz))))


# The first-person lane's keys and mouse (the customer, 2026-10-16: the first person's input is not the third
# person's -- "按照摄像机（在角色的头部）的真实变化"): its mouse is the turn and tilt of the head's camera itself, in the
# game mouse's units for C and D (FP_DEG_PER_UNIT), and its keys the way the person moves off that camera's level
# heading, eight ways (W+A 45 degrees left ...), held from FP_KEY_ON_MPS. The third person's lane keeps the keys
# and mouse as recorded.
FP_DEG_PER_UNIT = 7.6        # degrees a unit of mouse_dx turns C's and D's game camera (measured, the C/D mapping)
FP_KEY_ON_MPS, FP_KEY_OFF_MPS = 0.30, 0.20
FP_KEYS = {"W": 87, "A": 65, "S": 83, "D": 68}


def first_person_input_per_frame(players: list[list[float]], quaternions: list[list[float]]) -> list[dict]:
    """keyCode / mouse for each frame of a first-person lane, from its camera (GTA quaternions) and the played person's
    position (GTA metres): the head camera's own turn and tilt as mouse, the way the person moves off its heading as
    keys -- a walk off to the left of the view is W+A, round behind it S."""
    n = len(players)
    angles = [_heading_pitch(q) for q in quaternions]
    moving, keys = False, []
    for f in range(n):
        a, b = max(0, f - 1), min(n - 1, f + 1)
        vx, vy = ((float(players[b][i]) - float(players[a][i])) * TICK_HZ / max(1, b - a) for i in range(2))
        speed = math.hypot(vx, vy)
        moving = speed >= (FP_KEY_OFF_MPS if moving else FP_KEY_ON_MPS)
        if not moving:
            keys.append(frozenset())
            continue
        off = (math.degrees(math.atan2(-vx, vy)) - angles[f][0] + 180.0) % 360.0 - 180.0   # a left turn adds
        way = round(off / 45.0)   # -4 .. 4
        held = set()
        if -1 <= way <= 1:
            held.add("W")
        if way <= -3 or way >= 3:
            held.add("S")
        if 1 <= way <= 3:
            held.add("A")
        if -3 <= way <= -1:
            held.add("D")
        keys.append(frozenset(held))
    per_key = {k: _without_short_runs([k in held for held in keys], PATH_MIN_PRESS_FRAMES) for k in FP_KEYS}
    out = []
    for f in range(n):
        turn = (angles[f][0] - angles[f - 1][0] + 180.0) % 360.0 - 180.0 if f else 0.0
        tilt = angles[f][1] - angles[f - 1][1] if f else 0.0
        turn = turn if abs(turn) >= PATH_TURN_MIN_DEG else 0.0
        tilt = tilt if abs(tilt) >= PATH_TURN_MIN_DEG else 0.0
        out.append({"keyCode": [code for k, code in FP_KEYS.items() if per_key[k][f]],
                    "mouse_dx": round(_clamp(-turn / FP_DEG_PER_UNIT), 6),
                    "mouse_dy": round(_clamp(-tilt / FP_DEG_PER_UNIT), 6)})
    return out


def _without_short_runs(held: list[bool], shortest: int) -> list[bool]:
    """A key's frames with the short let-gos between presses filled in, then the short presses dropped."""
    out = list(held)
    for value in (False, True):   # gaps first: a press broken for a few frames is one press
        f = 0
        while f < len(out):
            if out[f] != value:
                f += 1
                continue
            end = f
            while end < len(out) and out[end] == value:
                end += 1
            inside = f > 0 and end < len(out) if value is False else True
            if end - f < shortest and inside:
                out[f:end] = [not value] * (end - f)
            f = end
    return out


def path_input_per_frame(positions: list[list[float]], quaternions: list[list[float]]) -> list[dict]:
    """keyCode / mouse for each frame of a free-path lane, from its camera (GTA metres and quaternions):
    the keys of the ways it moves, in the camera's own level frame, and its turn and pitch as mouse."""
    n = len(positions)
    angles = [_heading_pitch(q) for q in quaternions]
    held = {key: False for key, _ in PATH_KEYS}
    frames = {key: [] for key, _ in PATH_KEYS}
    for f in range(n):
        a, b = max(0, f - 1), min(n - 1, f + 1)
        v = [(float(positions[b][i]) - float(positions[a][i])) * TICK_HZ / max(1, b - a) for i in range(3)]
        h = math.radians(angles[f][0])
        ahead = -math.sin(h) * v[0] + math.cos(h) * v[1]   # along the level heading
        right = math.cos(h) * v[0] + math.sin(h) * v[1]
        way = {"forward": ahead, "back": -ahead, "right": right, "left": -right, "up": v[2], "down": -v[2]}
        for key, name in PATH_KEYS:
            held[key] = way[name] >= (PATH_KEY_OFF_MPS if held[key] else PATH_KEY_ON_MPS)
            frames[key].append(held[key])
    frames = {key: _without_short_runs(flags, PATH_MIN_PRESS_FRAMES) for key, flags in frames.items()}
    out = []
    for f in range(n):
        turn = (angles[f][0] - angles[f - 1][0] + 180.0) % 360.0 - 180.0 if f else 0.0
        tilt = angles[f][1] - angles[f - 1][1] if f else 0.0
        turn = turn if abs(turn) >= PATH_TURN_MIN_DEG else 0.0   # a re-aim nobody would see is no hand on the mouse
        tilt = tilt if abs(tilt) >= PATH_TURN_MIN_DEG else 0.0
        out.append({"keyCode": [key for key, _ in PATH_KEYS if frames[key][f]],
                    # B's signs: right and down are positive; a left turn adds to a GTA heading, looking up to its pitch
                    "mouse_dx": round(_clamp(-turn / LOOK_DEG_PER_UNIT), 6),
                    "mouse_dy": round(_clamp(-tilt / LOOK_DEG_PER_UNIT), 6)})
    return out


FPS_WINDOW_MS = 1000.0


def game_fps_per_frame(rows: list[dict], n: int) -> list[int]:
    """The game's own frame rate while the take was recorded, for each delivered frame,
    in whole frames a second as the customer's sample writes it (30).

    Each logged tick counts the game frames drawn since the tick logged before it;
    the rate is the frames over the last second of game time. Tick 0's count
    covers the unlogged time before it (the hold), so it is left out, and the
    first second takes the first whole second's rate."""
    ticks = sorted((tick, float(r["game_ms"]), int(r["frames"])) for tick, r in _log_ticks(rows).items())
    ms = [t[1] for t in ticks]
    cum = [0]   # cum[i] = frames drawn in (ms[0], ms[i]]
    for _, _, frames in ticks[1:]:
        cum.append(cum[-1] + frames)
    rate: dict[int, float] = {}
    j = 0
    for i, (tick, now, _) in enumerate(ticks):
        if now - ms[0] < FPS_WINDOW_MS:
            continue
        while now - ms[j + 1] >= FPS_WINDOW_MS:
            j += 1
        rate[tick] = (cum[i] - cum[j]) * 1000.0 / (now - ms[j])
    if not rate:
        raise ContractError("the input log is shorter than a second; the game frame rate cannot be measured")
    known = sorted(rate)
    return [int(round(rate[known[max(bisect.bisect_right(known, f) - 1, 0)]])) for f in range(n)]


def _wall(text, label: str) -> datetime:
    if not isinstance(text, str) or not WALL.match(text):
        raise ContractError(f"{label} must be YYYY-MM-DD HH:mm:ss.SSS, not {text!r}")
    try:
        return datetime.strptime(text, "%Y-%m-%d %H:%M:%S.%f")
    except ValueError as exc:
        raise ContractError(f"{label} is not a real time: {text!r}") from exc


def _wall_text(t: datetime) -> str:
    return t.strftime("%Y-%m-%d %H:%M:%S.") + f"{t.microsecond // 1000:03d}"


def _every_tick(known: dict[int, float], n: int) -> list[float]:
    """A value for each tick 0..n-1: the logged one; between two logged ticks, on the
    straight line between them; before the first or after the last, 1000/30 ms a tick on."""
    ticks = sorted(known)
    out = []
    for k in range(n):
        i = bisect.bisect_left(ticks, k)
        if i < len(ticks) and ticks[i] == k:
            out.append(known[k])
        elif i == 0:
            out.append(known[ticks[0]] - (ticks[0] - k) * TICK_MS)
        elif i == len(ticks):
            out.append(known[ticks[-1]] + (k - ticks[-1]) * TICK_MS)
        else:
            lo, hi = ticks[i - 1], ticks[i]
            out.append(known[lo] + (known[hi] - known[lo]) * (k - lo) / (hi - lo))
    return out


WALL_CLOCK, ANCHOR_CLOCK = "wall_clock_per_tick", "anchor_plus_game_clock"


def frame_times(rows: list[dict], n: int, *, anchor: str | None = None) -> tuple[list[str], str]:
    """`time` of delivered frames 0..n-1 (frame f = stage tick f), and where it came from.

    The requirement's time is the real system time. A take logs the local system
    time each tick was posed at (`wall`), and that is the tick's time. A take
    logged before `wall` existed has only the game clock: then `anchor`, the
    system time stage tick 0 was played at, plus the game time since tick 0.
    A skipped tick lies on the straight line between its neighbours, to the
    millisecond. Nothing is made up: a log without `wall` and no anchor has no
    time, and a time that does not move forward is refused, not mended."""
    by_tick = _log_ticks(rows)
    walls = ["wall" in row for row in by_tick.values()]
    if all(walls):
        if anchor is not None:
            raise ContractError("the take logged its own wall clock; a time anchor would overrule it")
        first = min(by_tick)
        base = _wall(by_tick[first]["wall"], f"input log tick {first} wall")
        known = {k: (_wall(row["wall"], f"input log tick {k} wall") - base) / timedelta(milliseconds=1)
                 for k, row in by_tick.items()}
        semantics = WALL_CLOCK
    elif any(walls):
        raise ContractError("the input log has a wall clock on some ticks only")
    elif anchor is None:
        raise ContractError("the input log has no wall clock (the take was recorded before it was logged): "
                            "give the system time stage tick 0 was played at as the take's time anchor")
    else:
        base = _wall(anchor, "time anchor")
        known = {k: float(row["game_ms"]) for k, row in by_tick.items()}
        semantics = ANCHOR_CLOCK
    ms = _every_tick(known, n)
    if semantics == ANCHOR_CLOCK:
        ms = [v - ms[0] for v in ms]
    times = [base + timedelta(milliseconds=round(v)) for v in ms]
    for f in range(1, n):
        if times[f] <= times[f - 1]:
            raise ContractError(f"frame {f}: time {_wall_text(times[f])} does not come after "
                                f"{_wall_text(times[f - 1])} (the clock went back)")
    return [_wall_text(t) for t in times], semantics


def interactions(rows: list[dict], n: int) -> list[dict]:
    """What the person did, in the game's own words (按鼠标左键：攻击/开火, 按 E：互动 ...),
    on the delivered frame of the tick that logged it. A row lists what was pressed
    between its tick and the next logged one; tick 0's row also holds anything
    pressed before it."""
    out = []
    for tick, row in sorted(_log_ticks(rows).items()):
        for event in row.get("events") or []:
            if not isinstance(event, str) or not event:
                raise ContractError(f"input log tick {tick} names an event that is not text")
            if tick < n:
                out.append({"frame": tick + 1, "event": event})   # the delivered frame: tick + 1
    return out


PLAYED_BY = {"program": "程序", "person": "真人"}
SOURCE_ZH = {
    "program": "程序。键鼠输入由程序生成，经系统输入接口送进游戏，与人按键走同一条路；日志记录的是游戏读到的按键与鼠标，"
               "不是事后推算。按需方确认点③（真人优先、不强制；脚本操作须自然，先交 demo 评估），本组即供评估的 demo。",
    "person": "真人键鼠操作。",
}
EVENT_GAP_FRAMES = TICK_HZ   # one interaction repeated at most a second apart reads as one run
ATTACK = "攻击"               # the attacks: 按鼠标左键：攻击/开火, 近身攻击

# 键鼠映射 (the requirement's 键鼠映射规则; the customer, 2026-09-24: a must): what each
# key and the mouse do, by worldline. B's free camera flies as stage_take_win.cpp fly()
# flies it (FlyRoom speeds; 8 degrees a unit of look input, at most 81 deg/s, 7.2 m/s^2);
# C's and D's person walks on the game's own on-foot controls. The walking speed and
# the degrees per unit of mouse_dx / mouse_dy were measured on beach-demo-002's delivery.
KEY_MAPPING = {
    "B": {
        "keys": [
            {"key": "W", "keyCode": 87, "action": "move_forward", "zh": "沿相机的水平朝向前进（不随俯仰上下），2.0 m/s"},
            {"key": "S", "keyCode": 83, "action": "move_backward", "zh": "后退，2.0 m/s"},
            {"key": "A", "keyCode": 65, "action": "move_left", "zh": "向左平移（侧移，不转向），2.0 m/s"},
            {"key": "D", "keyCode": 68, "action": "move_right", "zh": "向右平移（侧移，不转向），2.0 m/s"},
            {"key": "E", "keyCode": 69, "action": "move_up", "zh": "上升，1.0 m/s"},
            {"key": "Q", "keyCode": 81, "action": "move_down", "zh": "下降，1.0 m/s（离地约 0.8 m 时停住）"},
        ],
        "mouse": [
            {"input": "mouse_dx", "action": "turn", "zh": "左右移动：相机左右转（向右为正），每 1.0 约转 8°，最快约 81°/s"},
            {"input": "mouse_dy", "action": "pitch",
             "zh": "前后移动：抬头 / 低头（向下为正，即低头），每 1.0 约 8°，最多上下 85°；相机不翻滚"},
        ],
        "notes": ["组合键各方向叠加：W+A 斜向左前约 2.8 m/s，W+E 边前进边上升；相反的两键互相抵消（W+S、A+D、Q+E 不动）。",
                  "持续按住不加速，速度恒定；刚按下和刚松开的约半秒内按平滑的加速度（不超过 7.2 m/s²）起步、停下。",
                  "鼠标按键和其他键都不用；没有一键多用，不会触发菜单。"],
    },
    "C/D": {
        "keys": [
            {"key": "W", "keyCode": 87, "action": "walk_forward", "zh": "朝相机看的方向走"},
            {"key": "A", "keyCode": 65, "action": "walk_left", "zh": "朝相机的左方走：人物转身朝左走，不是原地侧移"},
            {"key": "D", "keyCode": 68, "action": "walk_right", "zh": "朝相机的右方走：人物转身朝右走"},
            {"key": "S", "keyCode": 83, "action": "walk_backward", "zh": "朝相机走（向后）：人物转过身朝镜头走过来"},
        ],
        "mouse": [
            {"input": "mouse_dx", "action": "turn_camera",
             "zh": "左右移动：游戏相机绕人物左右转（向右为正），每 1.0 约转 7.6°；人物朝相机的方向走，所以也用来转向。"
                   "人物站着转身时，相机同步跟着身体转（见注）"},
            {"input": "mouse_dy", "action": "pitch_camera", "zh": "前后移动：相机抬头 / 低头（向下为正），每 1.0 约 5–6°"},
        ],
        "notes": ["GTA 自带的步行操作、默认键位，W、A、S、D 及两两组合（W+A 等）都用：GTA 第三人称里按 A、D、S 不是横移，而是人物转向"
                  "相机的左、右、后方再走过去，相机不跟着转（2026-10-16 需方：按正常玩游戏的方式操作，不只用 W 加鼠标）。",
                  "第三人称（追背路）记录的是真实按键和鼠标：鼠标只转游戏相机，相机可以转到人物的侧面、正面；人物站着时{turn}。",
                  "第一人称路（编号见各路说明）的键鼠不是这份真实记录，而是按头部相机的真实变化换算的（需方 2026-10-16 认可）："
                  "这一帧第一人称相机左右转、抬低头多少就记多少鼠标（与上面同一换算：每 1.0 约 7.6°），人物相对第一人称镜头往哪个方向走就记"
                  "哪个键（8 个方向，速度到 0.3 m/s 才记）。所以第一、第三人称两路的键鼠可以不同。",
                  "步行约 1.75 m/s（实测）；持续按住不加速，不按 Shift 不会跑。",
                  "GTA 的走路有惯性（实测）：按下后约 0.1–0.2 秒起步；松开后人物还会再走一段才停下，走满速时约 1 秒、约 1 米"
                  "（最多 1.5 米），只走一两步时更短。所以按键的起止和人物动起来、停下来之间有这点时差，是游戏本身的，不是记录的时间错位。",
                  "特殊交互键（E 上车/互动、鼠标左键攻击、空格跳、Shift 跑等）按下时逐次记进 manifest 的 interactions"
                  "（帧号 + 游戏里对应的动作）。"],
    },
    "自由运镜": {
        "keys": [
            {"key": "W", "keyCode": 87, "action": "move_forward", "zh": "相机沿自己的水平朝向前进（推近）"},
            {"key": "S", "keyCode": 83, "action": "move_backward", "zh": "后退（拉远）"},
            {"key": "A", "keyCode": 65, "action": "move_left", "zh": "向左平移"},
            {"key": "D", "keyCode": 68, "action": "move_right", "zh": "向右平移"},
            {"key": "E", "keyCode": 69, "action": "move_up", "zh": "上升"},
            {"key": "Q", "keyCode": 81, "action": "move_down", "zh": "下降"},
        ],
        "mouse": [
            {"input": "mouse_dx", "action": "turn", "zh": "这一帧相机左右转了多少（向右为正），与世界线 B 同一换算：每 1.0 = 8°"},
            {"input": "mouse_dy", "action": "pitch", "zh": "这一帧相机抬头 / 低头了多少（向下为正），每 1.0 = 8°"},
        ],
        "notes": ["自由运镜不是用键鼠操控的：相机按规划好的路线逐帧摆放。这几路的键鼠按需方要求（2026-09-29）由运镜路线逐帧换算，"
                  "键位与世界线 B 相同，不是实时操作的记录。",
                  "按键只表示这一帧相机往哪个方向动，不表示多快（自由运镜的速度是变化的，世界线 B 按键是恒定速度）。"
                  f"某个方向的速度到 {PATH_KEY_ON_MPS:g} m/s 记为按下，降到 {PATH_KEY_OFF_MPS:g} m/s 以下记为松开；"
                  f"短于 {PATH_MIN_PRESS_FRAMES / TICK_HZ:g} 秒的按下和中途松开不记。更慢的移动在画面上看不出来（50 mm 下每帧不到半个像素），"
                  f"不记按键；同理，一帧转动或俯仰不到 {PATH_TURN_MIN_DEG:g}° 的鼠标记 0。",
                  "多个方向同时动就同时记多个键，例如边推近边右移记 W+D（87、68）；环绕时通常是平移键加鼠标反向转。"],
    },
}
KEY_NAMES = {87: "W", 65: "A", 83: "S", 68: "D", 81: "Q", 69: "E", 1: "鼠标左键", 2: "鼠标右键", 4: "鼠标中键",
             16: "Shift", 160: "左 Shift", 161: "右 Shift", 17: "Ctrl", 162: "左 Ctrl", 32: "空格", 70: "F"}


def key_names(codes) -> str:
    return "、".join(KEY_NAMES.get(c, f"键码 {c}") for c in codes)


def event_runs(events: list[dict], gap: int = EVENT_GAP_FRAMES) -> list[dict]:
    """The same interaction repeated at most `gap` frames apart, as one run: its first
    and last frame and how many times; runs in the order they start."""
    runs, last = [], {}
    for e in events:
        run = last.get(e["event"])
        if run is not None and e["frame"] - run["last_frame"] <= gap:
            run["last_frame"] = e["frame"]
            run["count"] += 1
            continue
        run = {"first_frame": e["frame"], "last_frame": e["frame"], "count": 1, "text": e["event"]}
        last[e["event"]] = run
        runs.append(run)
    return runs


def _unity_euler_deg(quaternion) -> list[float]:
    """Unity's Quaternion.eulerAngles of a docs/69 quaternion: [X, Y, Z] in degrees, each in [0, 360).

    Unity turns Z first, then X, then Y (R = Ry Rx Rz), so X = asin(-R12) - positive
    looks down - Y = atan2(R02, R22) - positive turns right - and Z = atan2(R10, R11)."""
    x, y, z, w = (float(v) for v in quaternion)
    angles = (math.asin(max(-1.0, min(1.0, 2.0 * (w * x - y * z)))),
              math.atan2(2.0 * (x * z + w * y), 1.0 - 2.0 * (x * x + y * y)),
              math.atan2(2.0 * (x * y + w * z), 1.0 - 2.0 * (x * x + z * z)))
    out = []
    for a in angles:
        deg = round(math.degrees(a) % 360.0, 4)
        out.append(0.0 if deg >= 360.0 else deg + 0.0)   # + 0.0 turns -0.0 into 0.0
    return out


TAPE_MATCH_M = 0.01   # an exported camera stands where its tape put it (measured: within 0.1 mm)


def _as_taped(rows: list[dict], samples: list[dict], lane: str) -> None:
    """The lane was rendered from the tape as it is now: a tape built again after the
    export (the fixed camera moved) no longer describes the video."""
    if len(samples) != len(rows):
        raise ContractError(f"{lane}: the tape has {len(samples)} frames, the export {len(rows)}")
    for row, sample in zip(rows, samples):
        off = math.dist(row["final_camera"]["position"], sample["position"])
        if off > TAPE_MATCH_M:
            raise ContractError(f"{lane}: tape frame {row['output_frame']} was rendered {off:.3f} m from where "
                                "config/camera-tape.json puts the camera -- the tape was built again after this "
                                "lane was exported: export the lane again")


def _lane_rows(take_dir: Path, lane: int, n: int) -> list[dict]:
    result = load_lane_result(take_dir, lane)   # ok, clean pixel check
    path = take_dir / "evidence" / f"frame-events-lane{lane}.jsonl"
    if not path.is_file() or sha256_file(path) != result.get("frame_events_sha256"):
        raise ContractError(f"lane {lane}: frame events are missing or differ from the lane result")
    rows = _rows(path)
    if len(rows) != n or [r.get("output_frame") for r in rows] != list(range(n)):
        raise ContractError(f"lane {lane}: frame events do not cover frames 0..{n - 1} once each")
    return rows


def camera_focus(take_dir, roles: dict[int, str], n: int) -> dict[int, dict]:
    """What each free-path lane looks at (config/camera-focus.json): its kind and, for
    every tape frame, the point in the docs/69 basis."""
    lanes = sorted(lane for lane, role in roles.items() if role == "free_path")
    if not lanes:
        return {}
    path = Path(take_dir) / "config" / "camera-focus.json"
    if not path.is_file():
        raise ContractError(f"free-path lanes {lanes} need config/camera-focus.json")
    value = load_json(path)
    if not isinstance(value, dict) or value.get("schema") != FOCUS_SCHEMA or value.get("n_frames") != n:
        raise ContractError(f"camera-focus.json must be {FOCUS_SCHEMA} over the tape's {n} frames")
    focus = {}
    for lane in lanes:
        entry = (value.get("lanes") or {}).get(str(lane))
        if not isinstance(entry, dict) or entry.get("kind") not in FOCUS_ZH:
            raise ContractError(f"camera-focus.json has no focus ({'/'.join(FOCUS_ZH)}) for free-path lane {lane}")
        frames = entry.get("frames")
        if not isinstance(frames, list) or len(frames) != n:
            raise ContractError(f"camera-focus.json lane {lane}: one point per tape frame ({n})")
        focus[lane] = {"kind": entry["kind"], "points": [_gta_to_delivery_vector(p) for p in frames]}
    return focus


def camera_zh(role: str, focus: str | None = None) -> str:
    return f"{ROLE_ZH[role]}（{FOCUS_ZH[focus]}）" if focus else ROLE_ZH[role]


def take_records(take_dir, kind: str, *, target_gta: list[float], time_anchor: str | None = None,
                 metric_scale: float = 1.0) -> dict:
    """The four action_camera arrays of one take in the docs/69 basis, frame f = stage
    tick f, and what the manifest says of them: each lane's camera, where `time`
    came from, the ticks the game skipped and the person's interactions."""
    take_dir = Path(take_dir)
    tape = load_json(take_dir / "config" / "camera-tape.json")
    n = int(tape["n_frames"])
    hold = int(tape["source"]["hold_frames"])
    roles = {int(k): v for k, v in tape["source"]["roles"].items()}
    if sorted(roles) != [1, 2, 3, 4] or any(r not in ROLE_ZH for r in roles.values()):
        raise ContractError(f"take {kind}: lane roles {roles} are not four of {sorted(ROLE_ZH)}")
    if not 0 <= hold < n:
        raise ContractError(f"take {kind}: {hold} hold frames of {n}")
    ticks = n - hold   # the delivered frames
    log = _rows(take_dir / "source" / "input-camera-log.jsonl")
    try:
        times, time_semantics = frame_times(log, ticks, anchor=time_anchor)
    except ContractError as exc:
        raise ContractError(f"take {kind}: {exc}") from exc
    inputs = input_per_frame(log, ticks)
    # The person's lanes carry keys and mouse from the tick the person is played -- in C the
    # takeover. Before it nobody pressed anything: the rows carry the actor's position, heading
    # and speed and the camera, and no key or mouse field at all (the customer, 2026-09-30:
    # nothing filled in or made up before the takeover).
    played_from = control_start(take_dir, kind)
    fps = game_fps_per_frame(log, ticks)
    focus = camera_focus(take_dir, roles, n)
    fixed = tape["source"].get("fixed")
    target = _gta_to_delivery_vector(fixed["aim"] if fixed else target_gta)
    lanes, path_keys, ahead = {}, set(), {}
    for lane, role in sorted(roles.items()):
        rows = _lane_rows(take_dir, lane, n)
        samples = tape["lanes"][str(lane)]["samples"]
        _as_taped(rows, samples, f"take {kind} lane {lane}")
        rows, samples = rows[hold:], samples[hold:]   # tape frame hold + f is stage tick f
        # Where the camera stood: the tape's position, which the export rendered from and was just checked
        # against frame by frame (within TAPE_MATCH_M; measured 0.1 mm). Not the game's read-back: the game
        # holds a camera in single precision, 0.12 mm steps this far from the map's origin, and the third
        # difference of that rounding put lane 4's jerk p95 at 21-22 for the customer's 20 (B flies at 18).
        cams = [_gta_to_delivery_vector(s_["position"]) for s_ in samples]
        if role in PATH_INPUT_ROLES:
            lane_inputs = path_input_per_frame([s_["position"] for s_ in samples],
                                               [r["final_camera"]["quaternion"] for r in rows])
            path_keys |= {k for rec in lane_inputs for k in rec["keyCode"]}
        elif role in STILL_ROLES:
            lane_inputs = [{"keyCode": [], "mouse_dx": 0.0, "mouse_dy": 0.0}] * len(rows)
        elif role == "first_person":   # the head camera's own turns and the way the person goes off it (2026-10-16)
            lane_inputs = first_person_input_per_frame([r["player_state"]["position"] for r in rows],
                                                       [r["final_camera"]["quaternion"] for r in rows])
        else:
            lane_inputs = inputs
        if role in PERSON_ROLES:
            players = [_gta_to_delivery_vector(r["player_state"]["position"]) for r in rows]
        records = []
        for f, row in enumerate(rows):
            cam_q = _gta_to_delivery_quaternion(row["final_camera"]["quaternion"])
            if role == "fixed":
                follow = [cams[f][i] - target[i] for i in range(3)]
            elif role == "free_path":
                point = focus[lane]["points"][hold + f]
                follow = [cams[f][i] - point[i] for i in range(3)]
            elif role in PERSON_ROLES:
                follow = [cams[f][i] - players[f][i] for i in range(3)]
            else:   # B's free camera follows nobody
                follow = [0.0, 0.0, 0.0]
            intr = row["final_camera"]["intrinsics"]
            rec = {
                "time": times[f],
                "fps": fps[f],
                "frame": f + 1,   # the customer's sample counts from 1: frame 1 is stage tick 0
                "camera_position": _round_vec(cams[f]),
                "camera_rotation_oula": _unity_euler_deg(cam_q),
                "camera_rotation_quaternion": _round_vec(cam_q),
                "camera_follow_offset": _round_vec(follow),
                "camera_intrinsics": {k: round(float(intr[k]), 6) for k in ("fx", "fy", "cx", "cy")},
                "camera_speed": _speed(cams, f),
                "metric_scale": float(metric_scale),
            }
            if role in PERSON_ROLES:
                pq = _gta_to_delivery_quaternion(row["player_state"]["quaternion"])
                rec.update({
                    "player_position": _round_vec(players[f]),
                    "player_rotation_oula": _unity_euler_deg(pq),
                    "player_rotation_quaternion": _round_vec(pq),
                    "player_speed": _speed(players, f),
                })
            if role in INPUT_ROLES + PATH_INPUT_ROLES + STILL_ROLES and not (role in PERSON_ROLES and f < played_from):
                rec.update({
                    "mouseX": 0.5,
                    "mouseY": 0.5,
                    "mouse_dx": lane_inputs[f]["mouse_dx"],
                    "mouse_dy": lane_inputs[f]["mouse_dy"],
                    "keyCode": lane_inputs[f]["keyCode"],
                })
            records.append(rec)
        lanes[lane] = records
        if role == "first_person":
            ahead[lane] = first_person_ahead_runs(rows, played_from)
    check_take(lanes, roles=roles, n=ticks, played_from=played_from)
    logged = _log_ticks(log)
    return {"lanes": lanes, "roles": roles, "focus": {lane: f["kind"] for lane, f in focus.items()},
            "person_input_from_frame": played_from + 1 if any(r in PERSON_ROLES for r in roles.values()) else None,
            "frames": ticks, "hold_frames": hold, "time_semantics": time_semantics, "time_anchor": time_anchor,
            "skipped_ticks": [k for k in range(ticks) if k not in logged], "interactions": interactions(log, ticks),
            "input_source": input_source(log), "keys_used": sorted({k for rec in inputs for k in rec["keyCode"]}),
            "path_keys_used": sorted(path_keys),
            "free_paths": {int(lane): report for lane, report in (tape["source"].get("free_paths") or {}).items()},
            "first_person_ahead": ahead}


def input_source(rows: list[dict]) -> str:
    """Who pressed the keys: "program" when the take's log says a program played
    (stage_robot.hpp, its input sent through the path a person's takes), else a person.
    A program that stopped part way (the game lost the foreground, or its keys or
    mouse did not reach the game) left the rest of the take unplayed: refused."""
    stopped = sorted(int(row["tick"]) for row in rows if row.get("src") == "stopped")
    if stopped:
        raise ContractError(f"the program playing the take stopped at stage tick {stopped[0]} "
                            f"({stopped[0] / TICK_HZ:.1f} s; the ASI log says why): record the take again")
    sources = {row.get("src") for row in rows}
    if "program" in sources and "person" in sources:
        raise ContractError("the input log says both a program and a person played the take")
    return "program" if "program" in sources else "person"


# Field names as in the customer's action_camera.json sample (demo4 #8), which the
# requirement makes the naming authority: camera_follow_offset, mouseX/mouseY, and
# metric_scale on every frame.
CAMERA_KEYS = ("time", "fps", "frame", "camera_position", "camera_rotation_oula", "camera_rotation_quaternion",
               "camera_follow_offset", "camera_intrinsics", "camera_speed", "metric_scale")
PLAYER_KEYS = ("player_position", "player_rotation_oula", "player_rotation_quaternion", "player_speed")
INPUT_KEYS = ("mouseX", "mouseY", "mouse_dx", "mouse_dy", "keyCode")


def lane_keys(role: str) -> tuple[str, ...]:
    return (CAMERA_KEYS + (PLAYER_KEYS if role in PERSON_ROLES else ())
            + (INPUT_KEYS if role in INPUT_ROLES + PATH_INPUT_ROLES + STILL_ROLES else ()))


def check_take(lanes: dict[int, list[dict]], *, roles: dict[int, str], n: int, played_from: int = 0) -> None:
    """docs/69 §8.2 and the requirement's lane rules as far as the files alone can show
    them; raises on the first miss. played_from: the stage tick from which the person is
    played; the person lanes' frames before it carry no key or mouse field."""
    if sorted(lanes) != [1, 2, 3, 4]:
        raise ContractError("a take delivers four lanes")
    times = None
    for lane, records in lanes.items():
        if len(records) != n or [r["frame"] for r in records] != list(range(1, n + 1)):
            raise ContractError(f"lane {lane}: frames are not 1..{n} once each")
        keys = set(lane_keys(roles[lane]))
        unplayed = keys - set(INPUT_KEYS) if roles[lane] in PERSON_ROLES else keys
        for r in records:
            want = unplayed if r["frame"] <= played_from else keys
            if set(r) != want:
                raise ContractError(f"lane {lane} frame {r['frame']}: fields {sorted(set(r) ^ want)} missing or not "
                                    f"allowed for a {roles[lane]} lane"
                                    + (" before the person is played" if want is not keys else ""))
            if not (math.isfinite(r["fps"]) and r["fps"] > 0):
                raise ContractError(f"lane {lane} frame {r['frame']}: fps is not a measured rate")
            q = r["camera_rotation_quaternion"]
            if abs(math.sqrt(sum(v * v for v in q)) - 1.0) > 1e-4:
                raise ContractError(f"lane {lane} frame {r['frame']}: camera quaternion is not unit")
            intr = r["camera_intrinsics"]
            if intr["fx"] != intr["fy"] or intr["fx"] <= 0:
                raise ContractError(f"lane {lane} frame {r['frame']}: fx must equal fy")
            if "mouse_dx" in r and not (-1.0 <= r["mouse_dx"] <= 1.0 and -1.0 <= r["mouse_dy"] <= 1.0):
                raise ContractError(f"lane {lane} frame {r['frame']}: mouse delta outside [-1, 1]")
        lane_times = [r["time"] for r in records]
        if lane_times != sorted(set(lane_times)):
            raise ContractError(f"lane {lane}: times are not strictly increasing")
        if times is None:
            times = lane_times
        elif lane_times != times:
            raise ContractError(f"lane {lane}: times differ from lane 1 (the four lanes must be frame-synchronized)")
    # both person lanes record the same person; their input may differ (the customer, 2026-10-16: the first person's
    # keys and mouse are its head camera's own, first_person_input_per_frame; the third person's are the recorded ones)
    person = [lane for lane in sorted(lanes) if roles[lane] in PERSON_ROLES]
    for lane in person[1:]:
        for a, b in zip(lanes[person[0]], lanes[lane]):
            if any(a.get(k) != b.get(k) for k in PLAYER_KEYS):
                raise ContractError(f"lanes {person[0]} and {lane} frame {a['frame']}: the played person differs "
                                    "(both person lanes record the same person)")


def _video_frames(path: Path) -> int:
    probe = shutil.which("ffprobe")
    if not probe:
        raise ContractError("ffprobe must be on PATH: every video's frames are counted against its JSON")
    out = subprocess.run([probe, "-v", "error", "-select_streams", "v:0", "-show_entries", "stream=nb_frames",
                          "-of", "csv=p=0", str(path)], capture_output=True, text=True, check=True).stdout
    return int(out.strip())


def spec_clears(take_dir) -> bool:
    """Whether the take's spec takes the floats in the sea out before it records (clear lines, 2026-10-18). Read from
    the spec, not the take's count: a take after another in the same game finds them gone already."""
    spec = Path(take_dir) / "config" / "stage-spec.txt"
    return spec.exists() and any(line.split()[:1] == ["clear"] for line in spec.read_text(encoding="utf-8").splitlines())


def deliver_take(take_dir, out_dir, kind: str, *, target_gta: list[float], record_dpi: float,
                 time_anchor: str | None = None, process_name: str = "GTA5.exe", group_name: str | None = None,
                 tracked: list[str] | None = None, played: str | None = None, floats_untracked: bool = False,
                 env: dict | None = None) -> dict:
    """One take's four lanes into the group folder as video_i, systeminfo_i, action_camera_i and multicam_sync_i
    under their numbers i; take_records' account plus the files. tracked: the group's moving things, the same in all
    twelve multicam_sync files (stage_multicam.tracked_ids); played: who the keys move (C's taken actor, D's seventh);
    env: the group's environment (its sun lights the multicam_sync frames: stage_multicam.sun_lighting)."""
    take_dir, out_dir = Path(take_dir), Path(out_dir)
    take = take_records(take_dir, kind, target_gta=target_gta, time_anchor=time_anchor)
    tape = load_json(take_dir / "config" / "camera-tape.json")
    out_dir.mkdir(parents=True, exist_ok=True)
    files = {}
    for lane, records in take["lanes"].items():
        i = number(kind, lane)
        video = take_dir / "capture" / f"video_{lane}.mp4"
        if not video.is_file():
            raise ContractError(f"{kind} lane {lane}: {video} is missing (stage-take-encode)")
        frames = _video_frames(video)
        if frames != len(records):
            raise ContractError(f"{kind} lane {lane}: video has {frames} frames, the JSON {len(records)} "
                                "(a video starts at stage tick 0: encode it again with stage-take-encode)")
        spec = tape["lanes"][str(lane)]
        width, height = spec["resolution"]["width"], spec["resolution"]["height"]
        paths = (out_dir / f"video_{i}.mp4", out_dir / f"systeminfo_{i}.json", out_dir / f"action_camera_{i}.json")
        shutil.copyfile(video, paths[0])
        write_json_atomic(paths[1], {"gameProcessName": process_name, "x": 0, "y": 0, "width": width,
                                     "height": height, "recordDpi": float(record_dpi)})
        write_json_atomic(paths[2], records, indent=None)
        role, focus = take["roles"][lane], take["focus"].get(lane)
        for path in paths:
            files[path.name] = {"number": i, "worldline": WORLDLINES[kind][lane], "take": kind, "lane": lane,
                                "camera": role, "focus": focus, "camera_zh": camera_zh(role, focus),
                                "focal_mm": spec["focal_mm"], "width": width, "height": height,
                                "sha256": sha256_file(path), "bytes": path.stat().st_size}
    if tracked is not None:   # the world under the four lanes, frame by frame (the delivery doc v1.1)
        takeover = take["person_input_from_frame"] - 1 if kind == "C" and take["person_input_from_frame"] else None
        lighting = sun_lighting(env) if env else None
        frames, info = take_frames(take_dir, kind=kind, ids=tracked, n=take["frames"], played=played, takeover_tick=takeover,
                                   lighting=lighting)
        note = capture_note(kind, skipped=info["skipped_ticks"], played=played, takeover_tick=takeover,
                            tracked_map=info["tracked_map"], wind_speed=info["wind_speed"],
                            floats="cleared" if spec_clears(take_dir) else "untracked" if floats_untracked else "",
                            lighting=lighting, env=env, game_tick_ms=info["game_tick_ms"])
        numbers = {lane: number(kind, lane) for lane in take["lanes"]}
        written = write_lane_files(out_dir, kind=kind, numbers=sorted(numbers.values()), capture_group_id=group_name or kind,
                                   ids=tracked, frames=frames, note=note, played=played, takeover_tick=takeover)
        for lane, i in numbers.items():
            path, ref = written[i], files[f"video_{i}.mp4"]
            files[path.name] = {**{k: ref[k] for k in ("number", "worldline", "take", "lane", "camera", "focus", "camera_zh",
                                                       "focal_mm", "width", "height")},
                                "sha256": sha256_file(path), "bytes": path.stat().st_size}
        take = {**take, "multicam_sync": {"tracked_entity_ids": tracked, "takeover_tick": takeover,
                                          "skipped_ticks": info["skipped_ticks"]}}
    return {**take, "files": files}


FIRST_PERSON_AHEAD_DEG = 15.0   # a first-person view this far round from the way the body faces is listed (README)


def first_person_ahead_runs(rows: list[dict], played_from: int) -> list[tuple[int, int]]:
    """Frame ranges (frame numbers, from 1) where a first-person lane looks more than FIRST_PERSON_AHEAD_DEG round
    from the way the played body faces. In GTA's third person the body turns only once a key goes down: till
    2026-10-09 the view went round first on the spot and the body came round a second or two later; since, standing,
    the take turns the body and the camera follows it (stage_take hold_view), and there should be few."""
    runs, start = [], None
    for f, row in enumerate(rows):
        cam = _heading_pitch(row["final_camera"]["quaternion"])[0]
        body = _heading_pitch(row["player_state"]["quaternion"])[0]
        ahead = f >= played_from and abs((cam - body + 180.0) % 360.0 - 180.0) > FIRST_PERSON_AHEAD_DEG
        if ahead and start is None:
            start = f + 1
        elif not ahead and start is not None:
            runs.append((start, f))
            start = None
    if start is not None:
        runs.append((start, len(rows)))
    return runs


def camera_moves(report: dict) -> dict | None:
    """A free lane's moves for the manifest (the customer, 2026-10-07: count the kinds of camera move and how
    often): its segments as measured on its camera path -- the path the export played, frame by frame checked --
    (stage_camera_paths.measure_moves), how many segments carry each move, all its moves (the metadata's 包含运镜
    轨迹) and the metres it travelled each way. None for a tape built before the moves were measured."""
    segments = report.get("moves")
    if not segments:
        return None
    order = list(dict.fromkeys(MOVE_LABELS.values()))
    counts: dict[str, int] = {}
    for seg in segments:
        for label in dict.fromkeys(m["label"] for m in seg["moves"]):
            counts[label] = counts.get(label, 0) + 1
    labels = sorted(counts, key=order.index)
    views = {v: sum(seg.get("view") == v for seg in segments) for v in VIEWS_ZH}
    return {"segments": segments, "counts": {label: counts[label] for label in labels}, "labels": labels,
            "views": views if any(views.values()) else None, "six_ways_m": report.get("six_ways_m")}


VIEWS_ZH = ("平视", "仰视", "俯视")   # a segment's view of its focus (stage_camera_paths.view_angle)


WAYS_ZH = {"forward": "前", "back": "后", "left": "左", "right": "右", "up": "上", "down": "下"}


def _move_text(m: dict) -> str:
    unit = m["unit"] if m["unit"] == "°" else f" {m['unit']}"
    detail = f"（{m['detail']}）" if m.get("detail") else ""
    return f"{m['label']} {m['amount']:g}{unit}{detail}"


def camera_move_lines(manifest: dict) -> list[str]:
    """The README's 自由运镜的运镜: how many segments of each free lane carry each kind of move, the six ways
    travelled and every segment's moves."""
    moves = manifest.get("camera_moves") or {}
    numbers = sorted(moves, key=int)
    if not numbers:
        return []
    by_focus = {}
    for n in numbers:
        focus = (manifest.get("files", {}).get(f"video_{n}.mp4") or {}).get("focus")
        by_focus.setdefault(focus, []).append(n)
    focus_lines = []
    if by_focus.get("centroid"):
        focus_lines.append(f"编号 {'、'.join(by_focus['centroid'])} 都看共视焦点（人群的中点），不以某一个人物为焦点。")
    if by_focus.get("player"):
        focus_lines.append(f"编号 {'、'.join(by_focus['player'])} 围绕受控第7人（这条世界线没有改变原有事件），焦点取该人物逐帧记录的位置"
                           "（缺拍处插值）。")
    if by_focus.get("event"):
        focus_lines.append(f"编号 {'、'.join(by_focus['event'])} 看事件焦点：受控人物和正在对他做出反应的人物（被规则打断原有行为的人）"
                           "的位置平均、平滑；没有人反应时就是受控人物本人（接管前是待接管的那个人物）。")
    unknown = [n for focus, lanes in by_focus.items() if focus not in FOCUS_ZH for n in lanes]
    if unknown:
        focus_lines.append(f"编号 {'、'.join(unknown)} 的焦点类型未记录，不推定其跟随对象。")
    lines = ["", "## 自由运镜的运镜", "",
             f"编号 {'、'.join(numbers)} 是自由运镜。", *focus_lines, "", "每路切成若干段，"
             "每段的运镜按相机实际走过的轨迹量出来（不是照抄规划），用需求文档 rev 637 §3.4 运镜类目树的"
             "「一级类目-二级类目」命名，量到需求的幅度才记：",
             "",
             "- 平移 1 m 以上。",
             "- 环绕：绕焦点 30° 以上，按绕过的角度归入小幅（30°–90°）、中幅（90°–180°）、大幅（180°–360°）、360°、"
             "连续多圈（720° 以上），括号里是绕行方向；环绕时离焦点近了、远了 1 m 以上记环绕拉近、环绕拉远；"
             "环绕的同时升降 1 m 以上记螺旋上升、螺旋下降（按需求的裁决不拆成环绕和升降）。",
             "- 摇动 10° 以上记基础摇动，括号里是方向；摇到焦点出画（越过画面半宽）、在画外连续 1.5 秒以上再摇回的，"
             "记「左摇后自然回镜」「右摇后自然回镜」（规划为出画后停约 2 秒再平滑摇回）。",
             "- 跟随：焦点是一个人物（围绕受控第7人、看事件焦点的路）时，这一段里焦点走了 1 m 以上、相机同向跟着移动 1 m 以上，"
             "记「跟随-绑定跟随」；看人群中点的路不记跟随。",
             "- 静止 3 s 以上。",
             "- 视角：每段按相机俯仰角的中位数记平视、仰视（抬头 1.5° 以上：相机低于人物胸口往上拍）、俯视（低头 8° 以上）；"
             "每路都安排了低机位（离沙面 0.6–1.1 m）、平视（1.4–1.9 m）、高机位（3.2–5 m）三档高度，各至少 5 秒。",
             "",
             "幅度：环绕、摇动为度，平移、螺旋升降、跟随为米，静止为秒。",
             "", "### 各类运镜出现的段数", "",
             "| 运镜（类目树：一级-二级） | " + " | ".join(f"编号 {n}" for n in numbers) + " |",
             "|---|" + "---|" * len(numbers)]
    for label in dict.fromkeys(MOVE_LABELS.values()):
        if any(label in moves[n]["counts"] for n in numbers):
            lines.append(f"| {label} | " + " | ".join(str(moves[n]["counts"].get(label, 0)) for n in numbers) + " |")
    for view in VIEWS_ZH:
        if any((moves[n].get("views") or {}).get(view) for n in numbers):
            lines.append(f"| 视角-{view} | " + " | ".join(str((moves[n].get("views") or {}).get(view, 0)) for n in numbers) + " |")
    lines.append("| （段数） | " + " | ".join(str(len(moves[n]["segments"])) for n in numbers) + " |")
    lines += ["", "六个方向的平移（每路必含）按相机自身的朝向累计：",
              *[f"- 编号 {n}：" + "、".join(f"{WAYS_ZH[k]} {moves[n]['six_ways_m'][k]:.1f} m" for k in WAYS_ZH)
                for n in numbers if moves[n].get("six_ways_m")]]
    for n in numbers:
        lines += ["", f"### 编号 {n} 逐段", "", "| 段 | 时间（秒） | 视角 | 运镜 |", "|---|---|---|---|"]
        lines += [f"| {i} | {seg['from_s']:.1f}–{seg['to_s']:.1f} | {seg.get('view', '')} | {'；'.join(_move_text(m) for m in seg['moves'])} |"
                  for i, seg in enumerate(moves[n]["segments"], 1)]
        lines += ["", "包含运镜轨迹（元信息 W 列）：" + "、".join(moves[n]["labels"])]
    return lines


def delivery_settings(settings: dict) -> tuple[str, dict[str, float]]:
    """The group's delivery folder name and, by take, the screen scaling of the machine that recorded it (group.json
    environment.record_dpi: one number for the group, or one per take -- beach-demo-003's AB was recorded at 150%, its
    C and D, again on 2026-10-09, on another machine at 100%)."""
    name = settings.get("delivery_name")
    if not isinstance(name, str) or not DELIVERY_NAME.match(name) or re.search(r'[<>:"/\\|?*\x00-\x1f]', name):
        raise ContractError("group.json delivery_name must be a folder name YYYY_MM_DD_<scene>_<NNN> "
                            f"(e.g. 2026_09_22_银行门口_001), not {name!r}")
    try:
        datetime.strptime(name[:10], "%Y_%m_%d")
    except ValueError as exc:
        raise ContractError(f"delivery_name {name!r} does not start with a real date") from exc
    dpi = (settings.get("environment") or {}).get("record_dpi")
    by_take = dpi if isinstance(dpi, dict) else dict.fromkeys(TAKES, dpi)
    if sorted(by_take) != sorted(TAKES) or any(not isinstance(v, (int, float)) or isinstance(v, bool) or not 1.0 <= v <= 4.0
                                                for v in by_take.values()):
        raise ContractError("group.json environment.record_dpi must be the capture machine's screen scaling in [1.0, 4.0] "
                            f"(1.5 = 150%), one for the group or one per take ({', '.join(TAKES)}), not {dpi!r}")
    return name, {kind: float(v) for kind, v in by_take.items()}


def parse_time_anchors(values) -> dict[str, str]:
    """TAKE=YYYY-MM-DD HH:mm:ss.SSS values (--time-anchor), at most one per take."""
    anchors: dict[str, str] = {}
    for value in values or ():
        kind, sep, text = value.partition("=")
        kind, text = kind.strip(), text.strip()
        if not sep or kind not in TAKES or kind in anchors:
            raise ContractError(f"a time anchor is TAKE=YYYY-MM-DD HH:mm:ss.SSS, once per take (AB, C, D): {value!r}")
        _wall(text, f"time anchor of take {kind}")
        anchors[kind] = text
    return anchors


# The rule events that are no rule firing: they mark where the person's control began.
CONTROL_STARTS = {"C": "takeover", "D": "seventh"}


def _control_tick(kind: str, rule_rows: list[dict]) -> int:
    marks = [r for r in rule_rows if r.get("rule") == CONTROL_STARTS[kind]]
    if len(marks) != 1:
        raise ContractError(f"take {kind}'s rule events mark the start of control {len(marks)} times, not once")
    return int(marks[0]["tick"])


def control_start(take_dir, kind: str) -> int:
    """The stage tick from which the take's person is played (the rule event marking it):
    C's takeover, D's seventh person at tick 0; a take nobody is played in: 0."""
    if kind not in CONTROL_STARTS:
        return 0
    return _control_tick(kind, _rows(Path(take_dir) / "source" / "rule-events.jsonl"))


def _controlled(group_dir: Path, kind: str, rule_rows: list[dict]) -> dict | None:
    """Whom the person played in a take (its result) and from which frame on (the rule
    event marking the start of control)."""
    if kind not in CONTROL_STARTS:
        return None
    tick = _control_tick(kind, rule_rows)
    result = take_result(group_dir, kind)
    if kind == "C":
        t = result.get("takeover")
        if not isinstance(t, dict) or t.get("ok") is not True or not isinstance(t.get("actor"), str):
            raise ContractError("take C's result does not say whom the person took over")
        return {"actor": t["actor"], "model": None, "from_frame": tick + 1, "from_s": round(tick / TICK_HZ, 3)}
    s = result.get("seventh")
    if not isinstance(s, dict) or s.get("ok") is not True or not isinstance(s.get("model"), str):
        raise ContractError("take D's result does not say which seventh person was played")
    return {"actor": "seventh", "model": s["model"], "from_frame": tick + 1, "from_s": round(tick / TICK_HZ, 3)}


def _story(group_dir: Path, kind: str, events: list[dict]) -> dict:
    """What happened in a take beyond the script: whom the person played from when,
    and on one timeline the causal rules that fired and the person's interactions,
    merged into runs. The original events changed when a rule fired or the person
    attacked (an attack changes the scene whether or not a rule saw it)."""
    path = group_dir / "takes" / kind / "source" / "rule-events.jsonl"
    if not path.is_file():
        raise ContractError(f"{path} is missing")
    rows = _rows(path)
    rules = [r for r in rows if r.get("rule") not in CONTROL_STARTS.values()]
    attacks = sum(ATTACK in e["event"] for e in events)
    timeline = [{"first_frame": int(r["tick"]) + 1, "last_frame": int(r["tick"]) + 1, "count": 1, "text": r["say"],
                 "rule": r["rule"]} for r in rules]
    timeline += [{**run, "rule": None} for run in event_runs(events)]
    timeline.sort(key=lambda e: e["first_frame"])
    return {"controlled": _controlled(group_dir, kind, rows), "event_changed": bool(rules) or attacks > 0,
            "triggered_rules": [r["rule"] for r in rules], "attack_interactions": attacks, "timeline": timeline,
            "interactions": events}


def _span(numbers: list[int]) -> str:
    return f"{numbers[0]}–{numbers[-1]}" if len(numbers) > 1 else str(numbers[0])


def _when(first: int, last: int) -> str:
    a, b = f"{(first - 1) / TICK_HZ:.1f}", f"{(last - 1) / TICK_HZ:.1f}"
    return f"第 {a} 秒" if a == b else f"第 {a}–{b} 秒"


def _time_note(take: dict) -> str:
    skipped = len(take["skipped_ticks"])
    filled = f"；游戏跳过的 {skipped} 拍按相邻两拍线性插值（在首末则按每拍 1/30 秒外推）" if skipped else ""
    if take["time_semantics"] == WALL_CLOCK:
        return "每一拍摆位时读取的系统时间" + filled + "。"
    return (f"这一遍录制时日志还没有逐拍系统时间：以给定的舞台第 0 拍系统时间 `{take['time_anchor']}` 为起点，"
            "加上游戏时钟经过的时间" + filled + "。")


# How the recorder turned a standing played person (group config "turn", stage_robot.hpp TurnStyle; the customer,
# 2026-10-10: turning on the spot was too fast): the words for the key mapping's note and the first-person lanes.
def standing_turn_zh(turn: str) -> tuple[str, str]:
    """(how the body was turned, how fast the first person turns then)"""
    words = (turn or "game").split()
    if words[0] == "glide":
        rate, accel = (words[1], words[2]) if len(words) == 3 else ("45", "90")
        return (f"逐帧把人物转过去：最快每秒 {rate}°，起转和停下按每秒² {accel}° 缓入缓出（GTA 的原地转身动作没有速度可调，"
                "0.5–1 秒就转完，所以不用它；这时脚不迈步）",
                f"站着转身时第一人称最快每秒 {rate}°，在需求的相机转速上限（p95 每秒 90°）之内")
    if words[0] == "steps":
        return ("让人物用游戏自带的原地转身动作分步转过去：每步最多 30°，上一步停稳再转下一步",
                "站着转身时第一人称每步不超过每秒约 90°")
    if words[0] == "walk" and words[-1] == "straight":
        rate = words[1] if len(words) == 3 else "60"
        return ("不原地转身：起步时顺着身体当前的朝向直着走，所有转向都在走路途中用鼠标慢慢拐（镜头最快每秒 "
                f"{rate}°，人跟着镜头转，转弯半径约 1.6 米以上）；站着时身体不转，只有第三人称镜头绕人转",
                f"第一人称跟身体朝向，只在走路拐弯时转，最快约每秒 {rate}°")
    if words[0] == "walk":
        return ("不转身体：和正常玩游戏一样，由按键转身——要走时直接按对应方向的键，游戏原地转过去再走；站着要转向看别人时点一下"
                "对应方向的键（约 0.43 秒），游戏转身并挪一小步（0.2–0.3 m）",
                "游戏转身很快（90° 约 0.5–1 秒），第一人称的镜头不跟着猛转，而是缓起缓停、最快每秒 60° 地转过去，"
                "所以转身时第一人称会落后身体一点")
    return ("让人物用游戏自带的原地转身动作转过去（约 0.5–1 秒转完）", "原地转身 0.5–1 秒就转完，这时第一人称画面也转得快")


def key_mapping(turn: str) -> dict:
    """KEY_MAPPING with the standing turn this group's C and D were recorded with."""
    how = standing_turn_zh(turn)[0]
    out = {wl: dict(m) for wl, m in KEY_MAPPING.items()}
    out["C/D"]["notes"] = [note.replace("{turn}", how) for note in KEY_MAPPING["C/D"]["notes"]]
    return out


MAPPING_TAKES = {"B": ("AB",), "C/D": ("C", "D"), "自由运镜": ("AB", "C", "D")}   # the takes whose input each mapping describes


def key_mapping_lines(manifest: dict) -> list[str]:
    """The README's 键鼠映射: per worldline, what each key and the mouse do, and which
    keys the takes actually pressed (from their own input logs)."""
    lines = ["", "## 键鼠映射", "",
             "`keyCode` 记的是这一帧按住的键（Windows 虚拟键码），下表是各键和鼠标在各世界线里的作用；"
             "逐帧的按键和鼠标量见各路 JSON。"]
    for wl, mapping in manifest["key_mapping"].items():
        kinds = MAPPING_TAKES[wl]
        path = wl == "自由运镜"
        pool = manifest.get("path_input_numbers", []) if path else manifest["input_numbers"]
        numbers = sorted(n for kind in kinds for n in pool if n in manifest["takes"][kind]["numbers"])
        if not numbers:
            continue
        what = ("真实输入自由相机" if wl == "B" else "由运镜路线换算，非实时操作" if path
                else "受控人物，第三人称追背与第一人称同一份输入")
        nums = "、".join(map(str, numbers))
        head = f"自由运镜（世界线 A、C、D 的编号 {nums}：{what}）" if path else f"世界线 {wl}（编号 {nums}：{what}）"
        lines += ["", f"### {head}", "",
                  "| 输入 | 键码 | 作用 |", "|---|---|---|"]
        lines += [f"| {k['key']} | {k['keyCode']} | {k['zh']} |" for k in mapping["keys"]]
        lines += [f"| 鼠标（`{m['input']}`） | — | {m['zh']} |" for m in mapping["mouse"]]
        lines += ["", *[f"- {note}" for note in mapping["notes"]]]
        field = "path_keys_used" if path else "keys_used"
        used = [f"{kind} 用到 {key_names(manifest['takes'][kind].get(field, [])) or '无按键'}" for kind in kinds]
        lines.append(("- 本组换算出的键：" if path else "- 本组实际按过的键：") + "；".join(used) + "。")
    return lines


def person_lane_lines(manifest: dict) -> list[str]:
    """The README's word on the two lanes of the played person: what the chase is made of, whose view the first person
    is, and where it is round from the body."""
    ahead = manifest.get("first_person_ahead_frames")
    if not ahead:
        return []
    def runs(frames):
        if not frames:
            return "没有"
        total = sum(b - a + 1 for a, b in frames)
        return ("、".join(f"第 {a}–{b} 帧（{(a - 1) / TICK_HZ:.1f}–{b / TICK_HZ:.1f} 秒）" for a, b in frames)
                + f"，合计 {total / TICK_HZ:.1f} 秒")
    numbers = sorted(ahead, key=int)
    chase = sorted(v["number"] for key, v in manifest["files"].items() if key.startswith("video_") and v["camera"] == "chase")
    return [f"- **第三人称**（编号 {'、'.join(map(str, chase))}）：和正常玩游戏一样，是 GTA 第三人称的跟随镜头，跟着录制时的鼠标转："
            f"以人物腰部上方 {FOLLOW_UP_M:g} m 为支点，顺着游戏相机的朝向往后 {FOLLOW_BACK_M:g} m、偏右肩 {FOLLOW_RIGHT_M:g} m（这套摆法是"
            "按游戏跟随镜头的实际位置量出来的，相差约 2 cm），所以鼠标把相机转到哪，追背就在人物的哪一侧——可以是侧面、正面，"
            "不要求一直在背后（需求 rev 1286、需方 2026-10-16）。人物站久了，录制程序会慢慢把相机转到侧面或正面看看，"
            f"挑相机和人物之间没有别人的方向。有场景物件挡在中间时拉近到物件前；离旁人至少 {FOLLOW_PERSON_M:g} m，"
            "有人走近时镜头平滑地往前收，不让人贴着镜头走过。C 接管前跟在该演员身后。",
            f"- **第一人称**（编号 {'、'.join(numbers)}）：相机在人物眼睛处（头骨外 {EYE_AHEAD_M * 100:.0f} cm、顺着视线），朝人物的脸——"
            "GTA 里脸基本就是身体正前方，所以朝身体的朝向（需方 2026-10-16：第一人称跟着头走）。没有待机晃头、没有走路颠簸："
            "朝向只跟身体，眼睛的高度平滑掉了走路一上一下的起伏。人物转身时第一人称同时转、不延迟（需方 2026-10-16：开始转身时必须对上）；"
            + standing_turn_zh(manifest.get("standing_turn", "game"))[1] + "。上下方向固定为平视（俯仰 0°），不随鼠标抬头、低头。"
            "C 接管前没有人操作，跟着该演员的头和朝向看。第一人称和身体朝向相差超过 "
            f"{FIRST_PERSON_AHEAD_DEG:g}° 的帧（转身时第一人称落在后面）：" + "；".join(f"编号 {n}：{runs(ahead[n])}" for n in numbers) + "。"]


def dpi_text(dpi, takes: dict) -> str:
    """The README's recordDpi: one scaling for the group, or by take when its takes were recorded on other machines."""
    by_take = dpi if isinstance(dpi, dict) else dict.fromkeys(takes, dpi)
    values = sorted(set(by_take.values()))
    if len(values) == 1:
        return f"{values[0]:g} = {values[0] * 100:g}%"
    parts = []
    for value in values:
        kinds = [kind for kind in takes if by_take[kind] == value]
        numbers = sorted(n for kind in kinds for n in takes[kind]["numbers"])
        parts.append(f"编号 {_span(numbers)}（{'、'.join(kinds)}）{value:g} = {value * 100:g}%")
    return "；".join(parts) + "：这几遍是在两台机器上录的"


def render_readme(manifest: dict) -> str:
    """The delivery's README, all from the manifest beside it: which number is which
    worldline and camera, what happened in each worldline, how every field is meant."""
    name, takes, worldlines = manifest["delivery_name"], manifest["takes"], manifest["worldlines"]
    videos = {v["number"]: v for key, v in manifest["files"].items() if key.startswith("video_")}
    lines = [f"# 四世界线交付：{name}", "",
             f"采集组 `{manifest['group_id']}`：同一舞台演三遍，共 4 条世界线、12 路视频。交付文件夹 `{name}/` 平铺 48 个文件："
             "`video_i.mp4` + `systeminfo_i.json` + `action_camera_i.json` + `multicam_sync_i.json`（i = 1–12），没有别的文件；"
             f"本说明 `{name}.README.md` 和 `{name}.manifest.json` 在文件夹旁边。",
             "", "## 编号、世界线与相机", "",
             "世界线 A = 1–3，B = 4，C = 5–8，D = 9–12。同一遍录制的四路逐帧同步（同一帧的 `time` 相同）："
             + "、".join(f"{_span(t['numbers'])}（{kind}）" for kind, t in takes.items()) + "各是一遍。",
             "", "| 编号 | 世界线 | 录制遍 | 遍内第几路 | 相机 | 焦距 |", "|---|---|---|---|---|---|"]
    for i in sorted(videos):
        v = videos[i]
        lines.append(f"| {i} | {v['worldline']} | {v['take']} | {v['lane']} | {v['camera_zh']} | {v['focal_mm']} mm |")
    length = "、".join(sorted({f"{t['frames']} 帧（{t['frames'] / TICK_HZ:.1f} 秒）" for t in takes.values()}))
    size = "、".join(sorted({f"{v['width']}×{v['height']}" for v in videos.values()}))
    lines += ["", f"每路 {length}、{size}、30 帧/秒，带声音。JSON 的第 1 帧（视频的第 1 帧）就是舞台第 0 秒："
              "读档和开拍前的准备都不在里面，所以下文的「第 X 秒」既是视频时间也是舞台时间（=（帧号 − 1）÷ 30）。",
              "", "## 每条世界线发生了什么", ""]
    for wl, story in worldlines.items():
        lines.append(f"### {wl}（编号 {_span(story['numbers'])}，录制遍 {story['take']}）")
        if wl == "A":
            lines.append("- 没有人干预，场上的人照剧本演。")
        by = PLAYED_BY[story["input_source"]]
        if wl == "B":
            lines += [f"- B 与 A 出自同一遍录制（同一个世界、同一段回放）：{by}用键鼠飞编号 {_span(story['numbers'])} "
                      "的自由相机，没有受控人物；场上发生的事与 A 相同，见 A 的时间线。",
                      f"- 输入来源：{SOURCE_ZH[story['input_source']]}", ""]
            continue
        who = story["controlled"]
        if who and who["model"]:
            lines.append(f"- 受控人物与控制开始：新增的第 7 人（模型 `{who['model']}`），第 {who['from_s']:g} 秒"
                         f"（第 {who['from_frame']} 帧）起由{by}键鼠操控。")
        elif who:
            lines.append(f"- 受控人物与控制开始：演员 `{who['actor']}`，第 {who['from_s']:g} 秒（第 {who['from_frame']} 帧）"
                         f"起由{by}键鼠接管。")
        if wl in ("C", "D"):
            lines.append(f"- 输入来源：{SOURCE_ZH[story['input_source']]}")
        if wl in ("C", "D"):
            why = []
            if story["triggered_rules"]:
                why.append("触发了因果规则 " + "、".join(story["triggered_rules"]))
            if story["attack_interactions"]:
                why.append(f"玩家有 {story['attack_interactions']} 次攻击类操作")
            lines.append(f"- 原事件是否被改变：**{'是' if story['event_changed'] else '否'}**"
                         f"（{'；'.join(why) or '没有触发因果规则，也没有攻击类操作'}）。")
        lines.append("- 时间线：" + ("" if story["timeline"] else "没有因果规则触发，也没有交互操作。"))
        for e in story["timeline"]:
            note = f"（规则 {e['rule']}）" if e["rule"] else f"（连续 {e['count']} 次）" if e["count"] > 1 else ""
            lines.append(f"  - {_when(e['first_frame'], e['last_frame'])} {e['text']}{note}")
        lines.append("")
    lines += ["时间线里标「规则」的是因果规则：玩家的干预引起的场上反应；其余是玩家的键鼠交互，写的是它在游戏里对应的动作。"
              f"同一种交互前后相隔不超过 {EVENT_GAP_FRAMES / TICK_HZ:g} 秒的合并成一段，写首末时间和次数；逐次的帧号见 manifest 的 "
              "`interactions`。「原事件是否被改变」：触发了任一因果规则，或玩家有攻击类操作（攻击/开火、近身攻击；"
              "即使没有触发规则也算），即为「是」。", "",
              "除上面这些玩家干预与它引起的反应外，四条世界线里所有人物每一帧的位置逐位一致（已逐帧比对）。", ""]
    persons = "、".join(str(i) for i in manifest["person_numbers"])
    input_only = "、".join(str(i) for i in manifest["input_numbers"] if i not in manifest["person_numbers"])
    path_only = "、".join(str(i) for i in manifest.get("path_input_numbers", []))
    still_only = "、".join(str(i) for i in manifest.get("still_input_numbers", []))
    anchored = "、".join(f"{_span(t['numbers'])}（{kind}）" for kind, t in takes.items()
                         if t["time_semantics"] == ANCHOR_CLOCK)
    dpi = manifest["record_dpi"]
    lines += ["## 字段口径", "",
              "- **坐标**：docs/69 的 Y 轴朝上左手系（x 右、y 上、z 前），由 GTA 的 Z 轴朝上换算："
              "位置 `[x, z, y]`，四元数 `[-x, -z, -y, w]`；单位米，每帧的 `metric_scale` = 1.0。",
              "- **frame**：从 1 起（与需方样例一致），第 1 帧 = 舞台第 0 秒。",
              "- **time**：录制时的真实系统时间（录制机本地时间，`YYYY-MM-DD HH:mm:ss.SSS`），逐帧严格递增；"
              "同一遍四路同一帧的 `time` 相同。",
              *[f"  - 编号 {_span(t['numbers'])}（{kind}）：{_time_note(t)}" for kind, t in takes.items()],
              "- **fps**：录制时游戏实际运行的帧率，按游戏时钟数前 1 秒画了多少帧，取整数；第 1 秒取第 1 个整秒的值。"
              + ("录制时游戏锁在每秒 30 帧，所以基本都是 30。" if all(t.get("frame_cap") for t in takes.values()) else "")
              + "视频本身固定每秒 30 帧。",
              "- **camera_rotation_oula / player_rotation_oula**：按 Unity `eulerAngles`，`[X, Y, Z]`，单位度，取 0–360："
              "X 俯仰（低头为正）、Y 偏航（向右转为正）、Z 翻滚；与四元数描述的是同一个朝向。",
              "- **相机字段**（每一路都有）：位置取相机轨迹（camera-tape）的双精度坐标，先与导出回读位置逐帧核对"
              "（距离误差不超过 1 cm），再换轴并保留 6 位小数；相机速度由同一轨迹计算。朝向和内参取导出时的逐帧回读；"
              "`fx` = `fy`，`cx`/`cy` 为画面中心。运动的相机轨迹按上限的 90%（自由运镜 75%）生成。",
              "- **camera_follow_offset**（字段名按需方样例）：固定机位 = 相机 − 它对准的点（一个固定点）；自由运镜 = 相机 − 该帧它看的焦点"
              "（类型见各路说明：共视焦点、事件焦点或受控第7人，逐帧记录）；第三人称追背、第一人称 = 相机 − 玩家；真实输入自由相机 = 0（不跟随任何人）。",
              *person_lane_lines(manifest),
              f"- **人物与键鼠字段**：编号 {persons}（C、D 的第三人称追背和第一人称）都带，同一遍的这两路逐帧相同"
              f"（同一受控人物、同一份输入）；编号 {input_only}（B 的真实输入自由相机）只带键鼠字段——B 没有受控人物，不附加人物字段。"
              + "".join(f"编号 {_span([i for i in t['numbers'] if i in manifest['person_numbers']])}（{kind}）从第 "
                        f"{t['person_input_from_frame']} 帧（接管那一帧）起才有键鼠字段：之前没有人操控，只记录该人物的位置、朝向、"
                        "速度和相机参数，这些帧里没有 `keyCode`、`mouseX` / `mouseY`、`mouse_dx` / `mouse_dy` 这几个字段"
                        "（按需方 2026-09-30 的说明，接管前不填写键鼠）。"
                        for kind, t in takes.items() if (t.get("person_input_from_frame") or 1) > 1)
              + (f"编号 {path_only}（自由运镜）也带键鼠字段，但不是实时操作的记录：相机按规划好的路线逐帧摆放，键鼠按需方要求由这条路线"
                 "逐帧换算（见下面的「键鼠映射」）。" if path_only else "")
              + (f"编号 {still_only}（固定机位）也带键鼠字段：相机全程不动，每帧都是无按键（`keyCode` 为空）、"
                 "`mouse_dx` / `mouse_dy` 为 0。" if still_only else ""),
              "- **keyCode**：Windows 虚拟键码（W = 87），鼠标按键同样按虚拟键码记（左键 = 1）。**mouse_dx / mouse_dy**："
              "游戏读到的视角输入，每 1/30 秒累计、截到 [-1, 1]，向右、向下为正；**mouseX / mouseY**（字段名按需方样例）固定 0.5"
              "（游戏里鼠标隐藏、居中）。游戏偶尔跳过的一拍：按键沿用上一拍，鼠标位移记 0。",
              "- **recordDpi**：录制机的系统屏幕缩放比例（" + dpi_text(dpi, takes) + "），不是鼠标 DPI。"]
    lines += key_mapping_lines(manifest)
    lines += camera_move_lines(manifest)
    lines += audio_lines(manifest)
    lines += ["", "## 校验", "",
              "- 12 路逐帧核对全部通过：每帧的回放时刻、机位、帧号都与计划一致；画面里没有鼠标指针。",
              "- 每路视频的帧数与它的 JSON 一致，都从舞台第 0 拍开始。",
              f"- `{name}.manifest.json` 记录 48 个文件各自的编号、世界线、录制遍、遍内路号、相机、焦距、SHA-256 与字节数。",
              "", "## 与需求的差异、待需方确认", "",
              "- " + "、".join(sorted({f"每路 {t['frames'] / TICK_HZ:g} 秒" for t in takes.values()}))
              + "（需求为 120 秒）。内部演示，未出正式交付回执。",
              *([f"- 编号 {anchored}的 `time` 是给定的舞台第 0 拍系统时间加游戏时钟推算的"
                 "（录制时日志还没有逐拍系统时间），不是逐拍读取的系统时间。"] if anchored else []),
              "- 坐标方向，以及 B 没有跟随目标时 `camera_follow_offset` 的写法（现填 0），请确认。", ""]
    return "\n".join(lines)


def _audio_swapped(take_dir: Path) -> list[dict] | None:
    path = take_dir / "evidence" / "audio-clean.json"
    return load_json(path)["swapped"] if path.is_file() else None


def audio_lines(manifest: dict) -> list[str]:
    """The README's 声音: which seconds of which take had the low chopping rumble swapped out."""
    takes = manifest["takes"]
    if all(t.get("audio_swapped") is None for t in takes.values()):
        return []
    lines = ["", "## 声音", "",
             "四路的声音都是同一遍第 1 路导出时录的回放声音。游戏回放时偶尔会放一段像远处直升机的低频「突突」声"
             "（画面里没有，录制里也没有任何飞机），这些时段已处理：300 Hz 以下换成同一遍安静时段的环境低音，"
             "450 Hz 以上是原声，300–450 Hz 渐变；每段首尾各用 1.5 秒淡入淡出，延续到片尾的一段结尾那头不淡、一直换到最后。", ""]
    for kind, t in takes.items():
        swapped = t.get("audio_swapped")
        what = ("原声，未处理（在这项处理之前出的片）" if swapped is None else "原声，没有这种声音" if not swapped
                else "、".join(f"第 {s['from_s']:g}–{s['to_s']:g} 秒" for s in swapped))
        lines.append(f"- 编号 {_span(t['numbers'])}（{kind}）：{what}")
    return lines


def deliver_group(group_dir, out_root, *, time_anchors: dict[str, str] | None = None,
                  untrack_floats: list[str] | None = None) -> dict:
    """The group as out_root/<delivery_name> holding exactly the 48 numbered files, with
    <delivery_name>.manifest.json and <delivery_name>.README.md beside it.

    time_anchors: for a take whose input log has no wall clock, the system time its
    stage tick 0 was played at (YYYY-MM-DD HH:mm:ss.SSS), by take.

    untrack_floats: the ids of the floats in the sea a group tracked (till beach-demo-109) to leave out of every
    multicam_sync file: a replay bobs them on its own waves as it plays, so what the take logged of them is not what
    the videos show (2026-10-18: deleted or frozen before the recorder, they bobbed in the export all the same)."""
    group_dir, out_root = Path(group_dir), Path(out_root)
    settings = load_json(group_dir / "group.json")
    name, record_dpi = delivery_settings(settings)
    sun_lighting(settings["environment"])   # a sun measured elsewhere stops the delivery before anything is copied
    anchors = dict(time_anchors or {})
    if set(anchors) - set(TAKES):
        raise ContractError(f"time anchors name unknown takes {sorted(set(anchors) - set(TAKES))}")
    target = stage_target((group_dir / settings["stage_spec"]).read_text(encoding="utf-8"))
    folder = out_root / name
    if folder.exists():
        raise ContractError(f"{folder} exists already: a delivery name is used once in a project")
    staging = out_root / f"{name}.partial"   # the folder takes its name only once it is whole
    shutil.rmtree(staging, ignore_errors=True)
    tracked = tracked_ids([group_dir / "takes" / kind for kind in TAKES])   # the same in all twelve multicam_sync files
    untrack = list(untrack_floats or [])
    if set(untrack) - set(tracked):
        raise ContractError(f"untrack_floats names ids the takes did not track: {sorted(set(untrack) - set(tracked))}")
    tracked = [i for i in tracked if i not in untrack]
    # who the keys move: whom take C took over as its result says (what ran, not what the group asked), D's seventh
    played = {"AB": None, "C": _controlled(group_dir, "C", _rows(group_dir / "takes" / "C" / "source" / "rule-events.jsonl"))["actor"],
              "D": SEVENTH_ID}
    takes = {kind: deliver_take(group_dir / "takes" / kind, staging, kind, target_gta=target, record_dpi=record_dpi[kind],
                                time_anchor=anchors.get(kind), group_name=name, tracked=tracked, played=played[kind],
                                floats_untracked=bool(untrack), env=settings["environment"])
             for kind in TAKES}
    if sorted(p.name for p in staging.iterdir()) != sorted(DELIVERY_FILES):
        raise ContractError(f"{staging} does not hold exactly the {len(DELIVERY_FILES)} numbered files")
    staging.rename(folder)
    worldlines = {}
    for kind in TAKES:
        story = _story(group_dir, kind, takes[kind]["interactions"])
        story["input_source"] = takes[kind]["input_source"]
        for wl in dict.fromkeys(WORLDLINES[kind].values()):
            worldlines[wl] = {"take": kind, "numbers": [number(kind, lane) for lane, w in WORLDLINES[kind].items()
                                                        if w == wl], **story}
    worldlines["B"]["same_recording_as"] = "A"
    lanes = [(kind, lane, role) for kind in TAKES for lane, role in sorted(takes[kind]["roles"].items())]
    files = {key: rec for kind in TAKES for key, rec in takes[kind]["files"].items()}
    results = {kind: take_result(group_dir, kind) for kind in TAKES}   # the recorder's account of each take
    manifest = {
        "schema": "dm-gta-four-worldline-delivery/2",
        "delivery_name": name,
        "group_id": settings["group_id"],
        "internal": True,
        "delivery_eligible": False,
        "coordinate_convention": DELIVERY_COORDINATE_CONVENTION,
        "coordinate_transform": "position=[x,z,y]; quaternion=[-x,-z,-y,w] (from GTA z-up)",
        "frame_zero": "stage tick 0; the hold before it is cut from every video and JSON",
        "fps_semantics": "game frame rate while recording, whole frames over the last second of game time",
        "euler_convention": "unity_eulerAngles_zxy_degrees_0_360",
        "record_dpi": record_dpi,
        "takes": {kind: {"numbers": [number(kind, lane) for lane in sorted(take["roles"])], "frames": take["frames"],
                         "hold_frames_cut": take["hold_frames"], "time_semantics": take["time_semantics"],
                         "time_anchor": take["time_anchor"], "skipped_ticks": take["skipped_ticks"],
                         "input_source": take["input_source"], "keys_used": take["keys_used"],
                         "path_keys_used": take["path_keys_used"],
                         # the person lanes' first frame with keys and mouse (C: the takeover); null: no person
                         "person_input_from_frame": take["person_input_from_frame"],
                         # the 30 fps cap and the aircraft taken out of the sky (takes recorded before
                         # 2026-09-28 had neither: null)
                         "frame_cap": results[kind].get("frame_cap"),
                         "aircraft_removed": results[kind].get("aircraft_removed"),
                         # where the sound's low chopping rumble was swapped out (stage_audio);
                         # null: encoded before that, the sound as exported
                         "audio_swapped": _audio_swapped(group_dir / "takes" / kind),
                         # the world under its four lanes (multicam_sync_i.json): ids, C's takeover tick, skipped ticks
                         "multicam_sync": take.get("multicam_sync")}
                  for kind, take in takes.items()},
        # how the recorder turned C's and D's standing person (group config "turn"; "game" before 2026-10-13)
        "standing_turn": settings.get("turn") or "game",
        "key_mapping": key_mapping(settings.get("turn") or "game"),
        "worldlines": worldlines,
        "person_numbers": [number(kind, lane) for kind, lane, role in lanes if role in PERSON_ROLES],
        "input_numbers": [number(kind, lane) for kind, lane, role in lanes if role in INPUT_ROLES],
        # free paths: keyCode / mouse worked out from the camera path, not a record of keys pressed
        "path_input_numbers": [number(kind, lane) for kind, lane, role in lanes if role in PATH_INPUT_ROLES],
        # fixed cameras: keyCode / mouse on every frame all the same, none pressed and no turn
        "still_input_numbers": [number(kind, lane) for kind, lane, role in lanes if role in STILL_ROLES],
        # each free lane's camera moves, measured on its path: segments, counts per kind, the six ways
        "camera_moves": {str(number(kind, lane)): summary for kind, lane, role in lanes if role == "free_path"
                         for summary in [camera_moves(takes[kind]["free_paths"].get(lane) or {})] if summary},
        # the first-person lanes' frames looking more than 90 degrees round from the body: the player turned first
        "first_person_ahead_frames": {str(number(kind, lane)): [list(run) for run in takes[kind]["first_person_ahead"][lane]]
                                      for kind, lane, role in lanes if role == "first_person"},
        "files": dict(sorted(files.items(), key=lambda item: (item[1]["number"], item[0]))),
    }
    write_json_atomic(out_root / f"{name}.manifest.json", manifest)
    (out_root / f"{name}.README.md").write_text(render_readme(manifest), encoding="utf-8")
    return manifest

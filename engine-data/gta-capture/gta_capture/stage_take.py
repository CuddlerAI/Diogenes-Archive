"""Four-worldline takes (issue #958): the dm-gta-scenario/2 contract.

A group is one stage played three times -- take AB (worldline A, plus the free
camera a person flies for worldline B), take C (a person takes over an actor at
whole second N) and take D (a person plays a seventh character from tick 0).
The stage itself is a dm-gta-stage-spec/1 text file (cast, clips, paths, beats,
causal rules) that the native companion parses and plans; this module seals it
into the scenario and checks what a take returns.

The native companion reads scenario keys with a flat, first-match key search,
so every key it reads must occur exactly once in the file: a nested key of the
same name anywhere earlier would silently be read instead.
"""

from __future__ import annotations

import math
import re
from dataclasses import dataclass
from pathlib import Path, PurePosixPath
from typing import Any

from .errors import ContractError
from .jsonio import load_json, sha256_file

SCENARIO_V2 = "dm-gta-scenario/2"
STAGE_SPEC = "dm-gta-stage-spec/1"
STAGE_CALIBRATION = "dm-gta-stage-calibration/1"
TAKE_RESULT = "dm-gta-stage-take/1"
TAKE_KINDS = ("AB", "C", "D")
TICK_HZ = 30

# Every key the native companion reads from a dm-gta-scenario/2 file.
NATIVE_KEYS = (
    "schema", "mode", "radio", "clock", "weather", "source_epoch", "warmup_s", "seed", "location",
    "take_kind", "stage_spec_path", "stage_spec_sha256", "stage_calibration_path",
    "stage_calibration_sha256", "stage_hold_s", "takeover_actor", "takeover_at_s", "route", "turn", "seventh_model",
    "seventh_x", "seventh_y", "seventh_heading", "free_cam_x", "free_cam_y", "free_cam_height",
    "free_cam_heading", "free_cam_fov", "park_x", "park_y", "wind_speed", "wind_direction_deg",
)
_HEX64 = re.compile(r"^[0-9a-f]{64}$")
_CLOCK = re.compile(r"^(?:[01][0-9]|2[0-3]):[0-5][0-9]$")


def _number(value: Any, label: str, *, lo: float | None = None, hi: float | None = None) -> float:
    if not isinstance(value, (int, float)) or isinstance(value, bool) or not math.isfinite(float(value)):
        raise ContractError(f"{label} must be a finite number")
    v = float(value)
    if (lo is not None and v < lo) or (hi is not None and v > hi):
        raise ContractError(f"{label} must be within [{lo}, {hi}]")
    return v


def _relative(value: Any, label: str) -> PurePosixPath:
    if not isinstance(value, str) or not value or "\\" in value:
        raise ContractError(f"{label} must be a relative path with forward slashes")
    p = PurePosixPath(value)
    if p.is_absolute() or ".." in p.parts:
        raise ContractError(f"{label} must stay under the scenario's directory")
    return p


@dataclass(frozen=True)
class SpecHeader:
    """What the Python side needs to know of a stage spec without planning it."""
    ticks: int
    actors: tuple[str, ...]
    rules: tuple[str, ...]
    props: tuple[tuple[str, str], ...]   # (actor, prop model), in spawn order


def read_spec_header(text: str) -> SpecHeader:
    lines = [ln.split("#", 1)[0].strip() for ln in text.splitlines()]
    lines = [ln for ln in lines if ln]
    if not lines or lines[0] != STAGE_SPEC:
        raise ContractError(f"stage spec must start with {STAGE_SPEC}")
    ticks = None
    actors: list[str] = []
    rules: list[str] = []
    props: list[tuple[str, str]] = []
    for ln in lines[1:]:
        words = ln.split()
        if words[0] == "ticks" and len(words) == 2:
            ticks = int(words[1])
        elif words[0] == "actor" and len(words) == 3:
            actors.append(words[1])
        elif words[0] == "rule" and len(words) >= 2:
            rules.append(words[1])
        elif words[0] == "prop" and len(words) == 4:
            props.append((words[1], words[2]))
    if ticks is None or ticks < TICK_HZ:
        raise ContractError("stage spec has no ticks line")
    if len(set(actors)) != len(actors) or not actors:
        raise ContractError("stage spec actors are missing or duplicated")
    return SpecHeader(ticks, tuple(actors), tuple(rules), tuple(props))


def check_native_keys(text: str) -> None:
    """Every natively read key occurs at most once; the required ones exactly once."""
    for key in NATIVE_KEYS:
        n = text.count(f'"{key}"')
        if n > 1:
            raise ContractError(f"scenario key {key!r} occurs {n} times; the native reader takes the first")


@dataclass(frozen=True)
class StageScenario:
    path: Path
    raw: dict
    spec_path: Path
    spec: SpecHeader
    calibration_path: Path | None

    @classmethod
    def load(cls, path) -> "StageScenario":
        source = Path(path).resolve()
        text = source.read_text(encoding="utf-8")
        check_native_keys(text)
        value = load_json(source)
        spec_path, spec, cal_path = validate_stage_scenario(value, source.parent)
        return cls(source, value, spec_path, spec, cal_path)

    @property
    def take_kind(self) -> str:
        return self.raw["take"]["take_kind"]

    @property
    def duration_s(self) -> float:
        return float(self.raw["replay"]["duration_s"])


def validate_stage_scenario(value: Any, base: Path) -> tuple[Path, SpecHeader, Path | None]:
    if not isinstance(value, dict) or value.get("schema") != SCENARIO_V2:
        raise ContractError(f"scenario.schema must be {SCENARIO_V2}")
    if value.get("mode") != "story_mode" or value.get("radio") != "off":
        raise ContractError("scenario must be Story Mode with the radio off")
    if not isinstance(value.get("scenario_id"), str) or not value["scenario_id"]:
        raise ContractError("scenario.scenario_id is required")
    if not isinstance(value.get("clock"), str) or not _CLOCK.match(value["clock"]):
        raise ContractError("scenario.clock must be HH:MM")
    if not isinstance(value.get("weather"), str) or not re.match(r"^[A-Z][A-Z_]*$", value["weather"]):
        raise ContractError("scenario.weather must be a GTA weather name")
    if not isinstance(value.get("source_epoch"), str) or not value["source_epoch"]:
        raise ContractError("scenario.source_epoch is required")
    seed = value.get("seed")
    if not isinstance(seed, int) or isinstance(seed, bool) or not 0 <= seed <= 2**53 - 1:
        raise ContractError("scenario.seed must be a non-negative integer")
    loc = value.get("location")
    if not isinstance(loc, list) or len(loc) != 3:
        raise ContractError("scenario.location must be the stage's world anchor [x, y, z]")
    for i, v in enumerate(loc):
        _number(v, f"scenario.location[{i}]")
    replay = value.get("replay")
    if not isinstance(replay, dict) or replay.get("fps") != TICK_HZ:
        raise ContractError("scenario.replay must record at 30 fps")
    duration = _number(replay.get("duration_s"), "scenario.replay.duration_s", lo=1, hi=600)
    _number(replay.get("warmup_s"), "scenario.replay.warmup_s", lo=0, hi=600)
    # warmup_s is read natively; it must be the only one
    take = value.get("take")
    if not isinstance(take, dict) or take.get("take_kind") not in TAKE_KINDS:
        raise ContractError("scenario.take.take_kind must be AB, C or D")
    kind = take["take_kind"]
    stage = value.get("stage")
    if not isinstance(stage, dict):
        raise ContractError("scenario.stage is required")
    spec_rel = _relative(stage.get("stage_spec_path"), "scenario.stage.stage_spec_path")
    if not isinstance(stage.get("stage_spec_sha256"), str) or not _HEX64.match(stage["stage_spec_sha256"]):
        raise ContractError("scenario.stage.stage_spec_sha256 must be a sha256")
    spec_path = base / Path(*spec_rel.parts)
    if not spec_path.is_file():
        raise ContractError(f"stage spec {spec_rel} is missing")
    if sha256_file(spec_path) != stage["stage_spec_sha256"]:
        raise ContractError("stage spec sha256 mismatch")
    spec = read_spec_header(spec_path.read_text(encoding="utf-8"))
    hold = _number(stage.get("stage_hold_s", 0), "scenario.stage.stage_hold_s", lo=0, hi=10)
    if duration + 1e-9 < hold + spec.ticks / TICK_HZ:
        raise ContractError("scenario.replay.duration_s is shorter than the stage hold plus its performance")
    cal_path = None
    if "stage_calibration_path" in stage:
        cal_rel = _relative(stage["stage_calibration_path"], "scenario.stage.stage_calibration_path")
        if not isinstance(stage.get("stage_calibration_sha256"), str) or not _HEX64.match(stage["stage_calibration_sha256"]):
            raise ContractError("scenario.stage.stage_calibration_sha256 must be a sha256")
        cal_path = base / Path(*cal_rel.parts)
        if not cal_path.is_file() or sha256_file(cal_path) != stage["stage_calibration_sha256"]:
            raise ContractError("stage calibration is missing or its sha256 does not match")
        if not cal_path.read_text(encoding="utf-8").startswith(STAGE_CALIBRATION):
            raise ContractError(f"stage calibration must be {STAGE_CALIBRATION}")
    elif kind != "AB":
        raise ContractError("takes C and D replay the calibration take AB measured; it must be named")
    check_take_settings(kind, take, spec)
    return spec_path, spec, cal_path


ROUTE_KEYS = ("w", "a", "s", "d", "wa", "wd", "sa", "sd")


def route_timetable(word: str) -> list[tuple[str, float]] | None:
    """A route step's key timetable, "w:2,wd:1,sd:1,s:1" (stage_robot.hpp parse_timetable): each combination held that
    many seconds (0.1-10) in turn, 30 s in all at most; None when `word` is not one."""
    if ":" not in word or word.endswith(","):
        return None
    out = []
    for item in word.split(","):
        keys, _, held = item.partition(":")
        try:
            seconds = float(held)
        except ValueError:
            return None
        if keys not in ROUTE_KEYS or not math.isfinite(seconds) or not 0.1 <= seconds <= 10.0:
            return None
        out.append((keys, seconds))
    return out if sum(s for _, s in out) <= 30.0 else None


def check_route(route: Any, spec: SpecHeader, *, taken: str | None,
                kind: str = "C") -> list[tuple[float, float, float, float, str, str, str]]:
    """C's or D's fixed route (stage_robot.hpp RouteWalker): "from_s x y stand_s look [keys] [tap|step]; ..." --
    walk to (x, y) in stage metres no earlier than stage second from_s, stand stand_s seconds looking at the
    actor `look` (or "-", the stage's middle). `keys` is what the leg there is walked on: w, a, s, d, wa, wd, sa
    or sd (the customer, 2026-10-16: not W alone -- in the third person A, S and D turn the person to that way
    off the camera and walk it); "-" or left out, the ones nearest the way off where the camera looks. "tap":
    the keys tapped where the person stands, the game turning it toward (x, y); "step": held one step toward
    it. Instead of the keys, a key timetable ("w:2,wd:1,sd:1,s:1", route_timetable): the camera set behind the body,
    each combination held its seconds in turn with the mouse untouched (the customer, 2026-10-16: a big turn walked
    round as W, W+D, S+D, S); (x, y) is where it is meant to end. Each step comes back as (from_s, x, y, stand_s, look,
    keys -- "" the nearest, or the timetable --, "walk"/"tap"/"step"/"timetable"). The native side reads it the same way."""
    if not isinstance(route, str) or not route.strip():
        raise ContractError(f"take {kind}: route must be a non-empty string")
    steps, last = [], float("-inf")
    for item in route.split(";"):
        if not item.strip():
            continue
        fields = item.split()
        if len(fields) < 5:
            raise ContractError(f"take {kind}: route step {item.strip()!r} needs five fields: from_s x y stand_s look, "
                                "then maybe the keys and tap or step")
        keys, how = "", "walk"
        rest = fields[5:]
        if rest and route_timetable(rest[0]) is not None:
            keys, how = rest[0], "timetable"
            rest = rest[1:]
        elif rest and (rest[0] == "-" or rest[0] in ROUTE_KEYS):
            keys = "" if rest[0] == "-" else rest[0]
            rest = rest[1:]
        if rest and how == "walk" and rest[0] in ("tap", "step"):
            how = rest[0]
            rest = rest[1:]
        if rest:
            raise ContractError(f"take {kind}: route step {item.strip()!r}: after the five fields the keys "
                                f"({', '.join(ROUTE_KEYS)} or -), then tap or step; or a key timetable (w:2,wd:1,sd:1,s:1)")
        try:
            from_s, x, y, stand_s = (float(f) for f in fields[:4])
        except ValueError:
            raise ContractError(f"take {kind}: route step {item.strip()!r}: from_s x y stand_s are numbers") from None
        look = fields[4]
        if not all(math.isfinite(v) for v in (from_s, x, y, stand_s)) or stand_s < 0 or abs(x) > 30 or abs(y) > 30:
            raise ContractError(f"take {kind}: route step {item.strip()!r} is out of range")
        if from_s < last:
            raise ContractError(f"take {kind}: route steps must come in time order")
        if look != "-" and (look not in spec.actors or look == taken):
            raise ContractError(f"take {kind}: the route looks at {look!r}, who is nobody to look at")
        last = from_s
        steps.append((from_s, x, y, stand_s, look, keys, how))
    if not steps:
        raise ContractError(f"take {kind}: the route has no steps")
    return steps


def check_turn(turn: Any) -> str:
    """How the program turns a standing body in C and D (stage_robot.hpp parse_turn): "game" (the game's
    turn on the spot in one go, 0.5-1 s whatever the angle), "steps" (the game's turn 30 degrees at a time),
    "glide [<deg/s> <deg/s^2>]" (the take sets the heading every frame, at most that fast, eased in and
    out; 45 and 90 when not given) -- the customer, 2026-10-10: turning on the spot was too fast -- or
    "walk [<deg/s>] [straight]" (2026-10-16: the program never turns the body, the keys do as they go down, a route
    turns a standing person with a tap; the camera turns at most that fast, 60 when not given; straight, 2026-10-17:
    a leg sets off the way the body faces and the walk comes round onto it, so nothing turns on the spot)."""
    if not isinstance(turn, str) or not turn.split():
        raise ContractError("turn must be game, steps, glide <deg/s> <deg/s^2> or walk <deg/s>")
    words = turn.split()
    if words[0] in ("game", "steps"):
        if len(words) != 1:
            raise ContractError(f"turn {turn!r}: {words[0]} takes no numbers")
        return turn
    if words[0] == "walk" and len(words) in (1, 2, 3):
        rest = words[1:]
        if rest and rest[-1] == "straight":
            rest = rest[:-1]
        if len(rest) == 1:
            try:
                rate = float(rest[0])
            except ValueError:
                raise ContractError(f"turn {turn!r}: walk takes the camera's deg/s at most, then maybe straight") from None
            if not (math.isfinite(rate) and 10 <= rate <= 90):
                raise ContractError(f"turn {turn!r}: walk turns the camera at 10-90 deg/s")
        elif rest:
            raise ContractError(f"turn {turn!r}: game, steps, glide <deg/s> <deg/s^2> or walk <deg/s> [straight]")
        return turn
    if words[0] != "glide" or len(words) not in (1, 3):
        raise ContractError(f"turn {turn!r}: game, steps, glide <deg/s> <deg/s^2> or walk <deg/s> [straight]")
    if len(words) == 3:
        try:
            rate, accel = float(words[1]), float(words[2])
        except ValueError:
            raise ContractError(f"turn {turn!r}: glide takes a rate and an acceleration") from None
        # the requirement's p95 limits for a camera: 90 deg/s, 150 deg/s^2
        if not (math.isfinite(rate) and math.isfinite(accel) and 10 <= rate <= 90 and 20 <= accel <= 150):
            raise ContractError(f"turn {turn!r}: a glide turns at 10-90 deg/s, eased at 20-150 deg/s^2")
    return turn


def check_take_settings(kind: str, take: dict, spec: SpecHeader) -> None:
    """A take's own settings, in the ranges the native companion accepts. Also
    run on a group's C and D before AB is recorded, so a bad setting surfaces
    before anyone has played."""
    if kind == "C":
        if take.get("takeover_actor") not in spec.actors:
            raise ContractError("take C: takeover_actor must be an actor of the stage")
        n = take.get("takeover_at_s")
        if not isinstance(n, int) or isinstance(n, bool) or not 0 <= n <= 55:
            raise ContractError("take C: takeover_at_s must be a whole second in [0, 55]")
        if n * TICK_HZ >= spec.ticks:
            raise ContractError("take C: the takeover lies after the stage ends")
        if "route" in take:
            check_route(take["route"], spec, taken=take["takeover_actor"])
    if kind == "D":
        if not isinstance(take.get("seventh_model"), str) or not take["seventh_model"]:
            raise ContractError("take D: seventh_model is required")
        if "route" in take:   # the seventh person on a fixed route (2026-10-16), from tick 0
            check_route(take["route"], spec, taken=None, kind="D")
    if "route" in take and kind == "AB":
        raise ContractError("take AB: nobody walks a route")
    if "turn" in take:
        if kind == "AB":
            raise ContractError("take AB: nobody stands to turn (turn is C's and D's)")
        check_turn(take["turn"])
        # the walk style leaves the body to the keys: only a route's taps turn a standing person to look
        if take["turn"].split()[0] == "walk" and not take.get("route"):
            raise ContractError(f"take {kind}: turn walk needs a route (its taps turn the standing person)")
    ranges = {"free_cam_x": (-200, 200), "free_cam_y": (-200, 200), "free_cam_height": (0, 50),
              "free_cam_heading": (-360, 360), "free_cam_fov": (10, 120), "seventh_x": (-200, 200),
              "seventh_y": (-200, 200), "seventh_heading": (-360, 360), "park_x": (-500, 500), "park_y": (-500, 500)}
    for key, (lo, hi) in ranges.items():
        if key in take:
            _number(take[key], f"scenario.take.{key}", lo=lo, hi=hi)


def build_take_scenario(*, group: dict, kind: str, spec_name: str, spec_sha256: str,
                        calibration_name: str | None = None, calibration_sha256: str | None = None) -> dict:
    """The scenario of one take of a group, every natively read key exactly once."""
    if kind not in TAKE_KINDS:
        raise ContractError(f"unknown take {kind!r}")
    env = group["environment"]
    take: dict[str, Any] = {"take_kind": kind}
    if kind == "AB":
        cam = group.get("free_camera", {})
        take.update({"free_cam_x": cam.get("x", 0.0), "free_cam_y": cam.get("y", -8.0),
                     "free_cam_height": cam.get("height", 1.7), "free_cam_heading": cam.get("heading", 0.0),
                     "free_cam_fov": cam.get("fov", 50.0)})
    elif kind == "C":
        take.update({"takeover_actor": group["takeover"]["actor"], "takeover_at_s": group["takeover"]["at_s"]})
        if group["takeover"].get("route"):   # a fixed route for the program, else it wanders
            take["route"] = group["takeover"]["route"]
    else:
        s = group["seventh"]
        take.update({"seventh_model": s["model"], "seventh_x": s.get("x", 0.0), "seventh_y": s.get("y", -6.0),
                     "seventh_heading": s.get("heading", 0.0)})
        if s.get("route"):   # a fixed route for the seventh person (2026-10-16), else it strolls
            take["route"] = s["route"]
    if kind != "AB" and group.get("turn"):   # how the program turns the standing body; else the game's turn in one go
        take["turn"] = group["turn"]
    park = group.get("park", {})
    take.update({"park_x": park.get("x", 0.0), "park_y": park.get("y", -40.0)})
    stage: dict[str, Any] = {"stage_spec_path": spec_name, "stage_spec_sha256": spec_sha256,
                             "stage_hold_s": group.get("hold_s", 1.0)}
    if calibration_name is not None:
        stage.update({"stage_calibration_path": calibration_name, "stage_calibration_sha256": calibration_sha256})
    wind = {}
    if env.get("wind") is not None:   # the same wind every take (2026-10-16: the user wants the flags, the wheel moving)
        w = env["wind"]
        if not isinstance(w, dict):
            raise ContractError("environment.wind must be {speed, direction_deg}")
        wind = {"wind_speed": _number(w.get("speed"), "environment.wind.speed", lo=0.0, hi=12.0),
                "wind_direction_deg": _number(w.get("direction_deg", 0.0), "environment.wind.direction_deg", lo=-360.0, hi=360.0)}
    return {
        **wind,
        "schema": SCENARIO_V2,
        "scenario_id": f"{group['group_id']}-{kind}",
        "mode": "story_mode",
        "seed": group.get("seed", 0),
        "location": env["location"],
        "clock": env["clock"],
        "radio": "off",
        "weather": env["weather"],
        "source_epoch": env["source_epoch"],
        "replay": {"duration_s": group["duration_s"], "warmup_s": env.get("warmup_s", 20), "fps": TICK_HZ},
        "take": take,
        "stage": stage,
        "effect_exclusions": ["cloth", "flags", "visible_clouds"],
    }


def check_take_result(result: dict, scenario: StageScenario) -> list[str]:
    """What is wrong with a take's result; an empty list when nothing is."""
    problems: list[str] = []
    if result.get("schema") != TAKE_RESULT:
        return [f"result schema is not {TAKE_RESULT}"]
    if result.get("take_kind") != scenario.take_kind:
        problems.append("result is for another take")
    clips = result.get("clips") or []
    if not clips:
        problems.append("no clips were sealed")
    span = float(result.get("game_span_s", 0))
    held = float(result.get("clips_duration_s", 0))
    # rehearsals v3-v5: the clips hold the game span to within about 0.1 s
    if clips and abs(held - span) > 0.25:
        problems.append(f"the clips hold {held:.3f} s of a {span:.3f} s recording")
    if int(result.get("frames_not_recording", 0)) != 0:
        problems.append(f"{result.get('frames_not_recording')} frames the recorder said it was off")
    ticks = scenario.spec.ticks
    if int(result.get("ticks_sampled", 0)) + int(result.get("ticks_skipped", 0)) != ticks:
        problems.append("sampled plus skipped ticks do not add up to the stage's ticks")
    if scenario.take_kind == "AB" and scenario.calibration_path is None and result.get("calibration_measured") is not True:
        problems.append("take AB must measure the calibration the group shares")
    if scenario.take_kind == "AB" and scenario.calibration_path is not None and result.get("calibration_measured") is not False:
        problems.append("take AB must replay the calibration the group brought")
    if scenario.take_kind != "AB" and result.get("calibration_measured") is not False:
        problems.append("takes C and D must reuse the group's calibration")
    if scenario.take_kind == "C" and not (result.get("takeover") or {}).get("ok"):
        problems.append("the takeover did not happen")
    # the story protagonist's phone (beach-demo-113 C, 2026-10-18: a call rang through the take and the hidden body's
    # green phone showed): the controller puts it away the frame it stirs, but a take where it did is recorded again
    phones = result.get("phone_frames")
    if isinstance(phones, int) and not isinstance(phones, bool) and phones > 0:
        problems.append(f"the story protagonist's phone rang or came out on {phones} frames: record the take again")
    # the map's things the spec clears (the floats in the sea, 2026-10-18): out from the first tick to the last
    left = result.get("cleared_left")
    if isinstance(left, int) and not isinstance(left, bool) and left > 0:
        problems.append(f"{left} of the map's objects the spec clears were back by the end of the take")
    if scenario.take_kind == "D" and not (result.get("seventh") or {}).get("ok"):
        problems.append("the player did not take the seventh person")
    # C after its takeover and D are played in the game's third person: its first person hides the
    # person's own head, and the replay keeps it hidden (beach-demo-003 D, 2026-10-09)
    if scenario.take_kind in ("C", "D"):
        played = result.get("takeover" if scenario.take_kind == "C" else "seventh") or {}
        for key, what in (("view_mode_restores", "frames out of the game's third person"),
                          ("body_turns", "times the program turned the standing body after its view")):
            value = played.get(key)
            if not isinstance(value, int) or isinstance(value, bool):
                problems.append(f"the result does not count the {what} ({key})")
        if isinstance(played.get("view_mode_restores"), int) and played["view_mode_restores"] != 0:
            problems.append(f"{played['view_mode_restores']} frames the played person was out of the game's third person")
        # a glide asked of a controller that does not know it turns the game's way, fast (2026-10-13):
        # the count is only written by one that does, and a glide that never ran is no glide
        turn = (scenario.raw.get("take") or {}).get("turn", "")
        if isinstance(turn, str) and turn.split()[:1] == ["glide"]:
            frames = played.get("glide_frames")
            if not isinstance(frames, int) or isinstance(frames, bool):
                problems.append("the take asked for a glide turn but the result does not count glide_frames: "
                                "the controller installed predates it (scripts/dev-install-controller.ps1)")
            elif frames == 0:
                problems.append("the take asked for a glide turn but the program never turned a standing body that way")
    # The cast did not play its plan: a scene the pool refused (the actor stood
    # released, and the same in every take would still agree), a new clip that
    # did not take, a ground probe that fell back while planning, or a camera
    # read off its tick. Missing counts are failures too: the native side writes them all.
    for key, what in (("scene_failures", "scenes the pool refused"),
                      ("runtime_releases", "actors released by the runtime, not by a rule"),
                      ("entries_not_running", "new scenes that were not running"),
                      ("entries_not_playing", "actors not playing their new clip"),
                      ("plan_ground_misses", "ground probes that missed while planning"),
                      ("cams_read_late", "ticks whose cameras were read off their boundary")):
        value = result.get(key)
        if not isinstance(value, int) or isinstance(value, bool):
            problems.append(f"the result does not count {what} ({key})")
        elif value != 0:
            problems.append(f"{value} {what}")
    return problems

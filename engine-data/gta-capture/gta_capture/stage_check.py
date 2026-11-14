"""A stage script, judged before anybody starts the game.

The cast are puppets on a timetable: where each of them stands on every tick
follows from the stage spec and a calibration alone. The native
gta_stage_plan_dump resolves the plan the way a take does and writes that
predicted world; this module reads it and checks what a script must get right
whoever plays it:

    script      every actor has something to do on every tick
    clearance   nobody walks through anybody
    set         nobody walks through the set
    stage       the cast keeps to a 10 x 10 m stage
    fixed       each take's fixed camera holds the whole cast
    free        worldline A's two free cameras find a path
    people      every lane of worldline A shows people 80% of the time
    shared      and the three see the same person half of it

Not judged here, and a report says so: the played person of takes C and D (the
route, the rules, how near the person comes, the first-person and chase lanes)
and the camera flown in B. A pass is a pass of the baseline, no more.

The predicted positions are the plan's own (on beach-demo-003 every actor of the
recorded take AB is within 0.2 mm of them, all 120 s). Bones are good to a
hand's width: see stage_plan_dump.cpp. against_recorded() holds a prediction
against a take's world telemetry, so every recorded group checks the predictor.
"""

from __future__ import annotations

import json
import math
import statistics
import subprocess
from dataclasses import dataclass
from pathlib import Path

from .errors import ContractError
from .stage_camera_paths import (ROOT_ABOVE_SAND_M, _axes, _dot, build_free_path, fill_ticks, piece_obstacle, smooth,
                                 stage_local, stage_world)
from .stage_tape import CENTROID_REACH_M, CENTROID_SIGMA, FIXED, FREE_PATHS, LANE_ROLES, _near_stage
from .tape import _look_at_euler

PLAN_WORLD_SCHEMA = "dm-gta-stage-plan-world/1"
CHECK_SCHEMA = "dm-gta-stage-check/1"
BONES = 7   # root, spine3, head, left hand, right hand, left foot, right foot

# Between two of the cast. On beach-demo-003 the man on the phone and the tourist pass
# each other 0.645 m apart, a hand of one 0.21 m from the other (49.1 s of take AB, as
# planned and as recorded): in the exported frames there is sand between them. A
# predicted bone is a hand's width off (stage_plan_dump.cpp), so its limit only
# catches a limb well inside somebody.
ROOT_CLEARANCE_M = 0.60
BONE_CLEARANCE_M = 0.15
# Feet: the bone is the ankle, the foot reaches 20 cm and more past it, so two feet with their ankles closer than this
# overlap. beach-demo-104's tourist stopped at her photo spot with her ankle 17 cm from the sitter's -- one foot over the
# other in the frame (the user, 2026-10-17); the hands' limit above let it through.
FOOT_CLEARANCE_M = 0.40
FEET = (5, 6)
BONE_REACH_M = 2.5          # roots further apart than this have no bones near each other
# From the middle of a piece of the set: a thing to carry, or a vehicle (a truck is 5 m long).
PROP_CLEARANCE_M = 1.5
VEHICLE_CLEARANCE_M = 3.5
STAGE_SIDE_M = 10.0         # the requirement's stage: a flat 10 x 10 m
FIXED_SHARE = 0.95          # of the take the fixed camera holds FIXED_PEOPLE people and more, whole
FIXED_PEOPLE = 3            # the meeting of 2026-10-16: 3 and more at once (it held the whole cast till then)
FIXED_MAX_H = 1.6           # ... from a tripod: 1.6 m over the sand at most,
FIXED_MAX_DOWN = 10.0       # ... looking down this many degrees at most (the customer, 2026-10-13: no steep look down)
PEOPLE_SHARE = 0.80         # requirement rev 630 7.1: a lane shows people 96 s of 120
SHARED_SHARE = 0.50         # and the lanes of a worldline see the same 60 s of it
IN_VIEW_BONES = 4           # "more than half a person": 4 of the 7 body points in frame
RECORDED_ROOT_M = 0.001     # a recorded actor nobody touched is this near the plan, or the plan is not the take's

NOT_JUDGED = (
    "takes C and D: the played person's route, the rules it sets off, how near it comes to the cast",
    "takes C and D: the free camera on the played person (lane 2), the chase and the first-person lanes",
    "take AB: the camera flown in worldline B (lane 4)",
    "what only the game shows: shadows, feet on the ground, the clips' look",
)


@dataclass(frozen=True)
class PlanWorld:
    header: dict
    actors: tuple[str, ...]
    ticks: tuple[dict, ...]   # per tick: actor id -> {"beat", "walking", "state": {"pos", "h", "phase", "bones"}}

    @property
    def frame(self) -> tuple[float, float, float, float]:
        return tuple(self.header["frame"])


def read_plan_world(lines) -> PlanWorld:
    header, ticks = None, {}
    for line in lines:
        if not line.strip():
            continue
        row = json.loads(line)
        if row.get("schema") != PLAN_WORLD_SCHEMA:
            raise ContractError(f"plan world schema must equal {PLAN_WORLD_SCHEMA!r}")
        if "header" in row:
            header = row["header"]
        else:
            ticks[int(row["tick"])] = {a["id"]: a for a in row["actors"]}
    if header is None or sorted(ticks) != list(range(int(header["ticks"]))):
        raise ContractError("plan world: a header and every tick of the take, each once")
    for a in header["actors"]:
        for t in ticks.values():
            if a["id"] in t and len(t[a["id"]]["state"]["bones"]) != BONES:
                raise ContractError(f"plan world: {a['id']} without its {BONES} body points")
    return PlanWorld(header, tuple(a["id"] for a in header["actors"]), tuple(ticks[k] for k in sorted(ticks)))


def dump_plan_world(dump_tool, spec, calibration, ground_z: float | None = None) -> PlanWorld:
    """Runs gta_stage_plan_dump (built with the native tests: cmake -S native -B <dir>)."""
    tool = Path(dump_tool)
    if not tool.is_file():
        raise ContractError(f"no plan dump tool at {tool}: build it with cmake -S native -B <dir> && cmake --build <dir> "
                            f"--target gta_stage_plan_dump")
    command = [str(tool), str(spec), str(calibration)] + ([repr(float(ground_z))] if ground_z is not None else [])
    done = subprocess.run(command, capture_output=True, text=True, encoding="utf-8")
    if done.returncode != 0:
        raise ContractError(f"the plan does not resolve: {done.stderr.strip() or f'exit {done.returncode}'}")
    return read_plan_world(done.stdout.splitlines())


def _runs(ticks: list[int]) -> list[list[int]]:
    out: list[list[int]] = []
    for k in ticks:
        if out and out[-1][1] == k - 1:
            out[-1][1] = k
        else:
            out.append([k, k])
    return out


def check_script(world: PlanWorld) -> dict:
    idle = {i: [k for k, t in enumerate(world.ticks) if i not in t] for i in world.actors}
    problems = [f"{i} has nothing to do on ticks {', '.join(f'{a}-{b}' for a, b in _runs(v)[:4])}"
                for i, v in idle.items() if v]
    pops = [(b["entry_pop"], a["id"], b["clip"], b["start"]) for a in world.header["actors"] for b in a["beats"]
            if b["start"] > 0]
    worst = max(pops, default=None)
    return {"id": "script", "ok": not problems, "problems": problems, "actors": len(world.actors),
            "changes_of_clip": len(pops),
            "largest_pose_step": None if worst is None else {
                "metres_over_the_bones": round(worst[0], 3), "actor": worst[1], "into": worst[2], "tick": worst[3]},
            "planner_notes": list(world.header.get("notes", []))}


def check_clearance(world: PlanWorld, *, root_m: float = ROOT_CLEARANCE_M, bone_m: float = BONE_CLEARANCE_M,
                    foot_m: float = FOOT_CLEARANCE_M) -> dict:
    pairs = {}
    for k, tick in enumerate(world.ticks):
        here = [(i, tick[i]["state"]) for i in world.actors if i in tick]
        for n, (a, sa) in enumerate(here):
            for b, sb in here[n + 1:]:
                d = math.hypot(sa["pos"][0] - sb["pos"][0], sa["pos"][1] - sb["pos"][1])
                entry = pairs.setdefault((a, b), {"root_m": (float("inf"), -1), "bone_m": (float("inf"), -1)})
                if d < entry["root_m"][0]:
                    entry["root_m"] = (d, k)
                if d < BONE_REACH_M:
                    near = min(math.dist(p, q) for p in sa["bones"] for q in sb["bones"])
                    if near < entry["bone_m"][0]:
                        entry["bone_m"] = (near, k)
                    feet = min(math.dist(sa["bones"][i], sb["bones"][j]) for i in FEET for j in FEET)
                    if feet < entry.setdefault("foot_m", (float("inf"), -1))[0]:
                        entry["foot_m"] = (feet, k)
    problems, closest = [], []
    for (a, b), e in sorted(pairs.items(), key=lambda item: item[1]["root_m"][0]):
        (root, root_k), (bone, bone_k) = e["root_m"], e["bone_m"]
        foot, foot_k = e.get("foot_m", (float("inf"), -1))
        closest.append({"pair": [a, b], "root_m": round(root, 3), "root_tick": root_k,
                        "bone_m": None if math.isinf(bone) else round(bone, 3),
                        "bone_tick": None if math.isinf(bone) else bone_k,
                        "foot_m": None if math.isinf(foot) else round(foot, 3)})
        if root < root_m:
            problems.append(f"{a} and {b} come within {root:.2f} m of each other at {root_k / 30:.1f} s (under {root_m} m)")
        elif bone < bone_m:
            problems.append(f"{a} and {b}: a hand, foot or head of one {bone:.2f} m from the other's at "
                            f"{bone_k / 30:.1f} s (under {bone_m} m)")
        elif foot < foot_m:
            problems.append(f"{a} and {b}: their ankles {foot:.2f} m apart at {foot_k / 30:.1f} s -- the feet overlap "
                            f"(under {foot_m} m)")
    return {"id": "clearance", "ok": not problems, "problems": problems, "closest": closest[:6],
            "limits": {"root_m": root_m, "bone_m": bone_m, "foot_m": foot_m}}


def _set_room(piece: dict) -> float:
    models = piece.get("models") or []
    return PROP_CLEARANCE_M if models and all(m.startswith("prop_") for m in models) else VEHICLE_CLEARANCE_M


def check_set(world: PlanWorld) -> dict:
    problems, closest = [], []
    for piece in world.header.get("sets", []):
        best = (float("inf"), None, -1)
        for k, tick in enumerate(world.ticks):
            for i, a in tick.items():
                d = math.hypot(a["state"]["pos"][0] - piece["pos"][0], a["state"]["pos"][1] - piece["pos"][1])
                if d < best[0]:
                    best = (d, i, k)
        room = _set_room(piece)
        closest.append({"set": piece["id"], "actor": best[1], "metres": round(best[0], 2), "tick": best[2], "room_m": room})
        if best[0] < room:
            problems.append(f"{best[1]} passes {best[0]:.2f} m from the middle of {piece['id']} at {best[2] / 30:.1f} s "
                            f"(under {room} m)")
    return {"id": "set", "ok": not problems, "problems": problems,
            "closest": sorted(closest, key=lambda c: c["metres"])[:4]}


def check_stage(world: PlanWorld, *, side_m: float = STAGE_SIDE_M) -> dict:
    local = [stage_local(world.frame, a["state"]["pos"][0], a["state"]["pos"][1])
             for tick in world.ticks for a in tick.values()]
    xs, ys = [p[0] for p in local], [p[1] for p in local]
    wide, deep = max(xs) - min(xs), max(ys) - min(ys)
    problems = [f"the cast covers {wide:.1f} x {deep:.1f} m, over the {side_m:g} x {side_m:g} m stage"] \
        if max(wide, deep) > side_m + 0.05 else []
    return {"id": "stage", "ok": not problems, "problems": problems,
            "x_m": [round(min(xs), 2), round(max(xs), 2)], "y_m": [round(min(ys), 2), round(max(ys), 2)]}


def _sand(world: PlanWorld) -> float:
    return statistics.median(a["state"]["pos"][2] for tick in world.ticks for a in tick.values()) - ROOT_ABOVE_SAND_M


class _View:
    """One camera pose: which points it has in frame."""

    def __init__(self, position, euler, focal_mm: float, width: int = 1920, height: int = 1080):
        self.position = position
        self.f, self.r, self.u = _axes(euler[0], euler[2])
        self.px = width * focal_mm / 36.0
        self.width, self.height = width, height

    def pixel(self, point):
        v = [point[c] - self.position[c] for c in range(3)]
        ahead = _dot(v, self.f)
        if ahead < 0.3:
            return None
        return self.width / 2 + self.px * _dot(v, self.r) / ahead, self.height / 2 - self.px * _dot(v, self.u) / ahead

    def in_frame(self, bones) -> tuple[int, float]:
        """How many of a person's body points are in frame, and how much of its height they span."""
        rows, inside = [], 0
        for p in bones:
            uv = self.pixel(p)
            if uv and 0.0 <= uv[0] <= self.width and 0.0 <= uv[1] <= self.height:
                inside += 1
                rows.append(uv[1])
        return inside, (max(rows) - min(rows)) / self.height if rows else 0.0


def _fixed_view(world: PlanWorld, take: str, sand: float) -> _View:
    (at_x, at_y, at_h), (aim_x, aim_y, aim_h) = FIXED[take]
    at = [*stage_world(world.frame, at_x, at_y), sand + at_h]
    aim = [*stage_world(world.frame, aim_x, aim_y), sand + aim_h]
    return _View(at, _look_at_euler(at, aim), LANE_ROLES[take]["1"][1])


def check_fixed(world: PlanWorld, *, share: float = FIXED_SHARE, people: int = FIXED_PEOPLE) -> dict:
    sand = _sand(world)
    problems, cameras = [], {}
    for take in FIXED:
        view = _fixed_view(world, take, sand)
        whole, enough, smallest, cut = 0, 0, 1.0, {}
        for tick in world.ticks:
            n = 0
            for i, a in tick.items():
                inside, tall = view.in_frame(a["state"]["bones"])
                if inside < BONES:
                    cut[i] = cut.get(i, 0) + 1
                else:
                    n += 1
                    smallest = min(smallest, tall)
            whole += n == len(tick)
            enough += n >= people
        held, kept = whole / len(world.ticks), enough / len(world.ticks)
        (at_x, at_y, at_h), (aim_x, aim_y, aim_h) = FIXED[take]
        down = math.degrees(math.atan2(at_h - aim_h, math.hypot(at_x - aim_x, at_y - aim_y)))
        cameras[take] = {"everybody_whole": round(held, 4), f"{people}_whole": round(kept, 4),
                         "smallest_person_of_frame_height": round(smallest, 3),
                         "cut_off_s": {i: round(n / 30, 1) for i, n in sorted(cut.items())},
                         "height_m": at_h, "looking_down_deg": round(down, 1)}
        if kept < share:
            problems.append(f"take {take}'s fixed camera holds {people} people whole {kept:.0%} of the take (under {share:.0%})")
        if at_h > FIXED_MAX_H + 1e-9:
            problems.append(f"take {take}'s fixed camera stands {at_h:g} m up (a tripod: {FIXED_MAX_H:g} m at most)")
        if down > FIXED_MAX_DOWN:
            problems.append(f"take {take}'s fixed camera looks down {down:.0f} degrees ({FIXED_MAX_DOWN:g} at most)")
    return {"id": "fixed", "ok": not problems, "problems": problems, "cameras": cameras}


def _free_lanes(world: PlanWorld, seed: int) -> tuple[dict, list[str]]:
    """Worldline A's free paths as stage_tape builds them, from the cast of the plan."""
    sand, ticks = _sand(world), len(world.ticks)
    crowd = [[list(a["state"]["pos"]) for a in tick.values()] for tick in world.ticks]
    middle = {k: [sum(p[c] for p in cast) / len(cast) for c in range(3)] for k, cast in enumerate(crowd) if cast}
    focus = smooth([_near_stage(world.frame, p, CENTROID_REACH_M) for p in fill_ticks(middle, ticks)], CENTROID_SIGMA)
    look = [[p[0], p[1], p[2] + 0.3] for p in focus]
    pieces = [piece_obstacle(s["pos"][0], s["pos"][1], s.get("models") or []) for s in world.header.get("sets", [])]
    lanes, problems = {}, []
    for lane, (role, focal) in LANE_ROLES["AB"].items():
        if role != "free_path":
            continue
        _, room = FREE_PATHS[("AB", lane)]
        try:
            poses, report = build_free_path(key=f"AB{lane}", seed=seed, room=room, focal_mm=focal, frame=world.frame,
                                            sand_z=sand, focus=focus, look=look, people=crowd,
                                            obstacles=[p for p in pieces if p])
        except ContractError as error:
            problems.append(str(error))
            continue
        lanes[lane] = {"focal_mm": focal, "poses": poses, "report": report}
    return lanes, problems


def check_cameras(world: PlanWorld, *, seed: int) -> list[dict]:
    """free, people and shared: the three lanes of worldline A, as a take of this script would tape them."""
    lanes, problems = _free_lanes(world, seed)
    free = {"id": "free", "ok": not problems, "problems": problems, "seed": seed,
            "paths": {lane: {"tries": v["report"]["attempt"] + 1, "clearance_m": v["report"]["clearance_m"],
                             "focus_in_view": v["report"]["focus_in_view"]} for lane, v in lanes.items()}}
    if problems:   # without its free lanes the worldline's views cannot be judged
        skipped = ["worldline A's free cameras found no path"]
        return [free, {"id": "people", "ok": False, "problems": skipped}, {"id": "shared", "ok": False, "problems": skipped}]
    fixed = _fixed_view(world, "AB", _sand(world))
    views = {"1": lambda k: fixed}
    for lane, v in lanes.items():
        views[lane] = lambda k, v=v: _View(v["poses"][k][0], v["poses"][k][1], v["focal_mm"])
    shown = dict.fromkeys(views, 0)
    together = 0
    for k, tick in enumerate(world.ticks):
        seen_by_all = None
        for lane, at in views.items():
            view = at(k)
            seen = {i for i, a in tick.items() if view.in_frame(a["state"]["bones"])[0] >= IN_VIEW_BONES}
            shown[lane] += bool(seen)
            seen_by_all = seen if seen_by_all is None else seen_by_all & seen
        together += bool(seen_by_all)
    n = len(world.ticks)
    people_problems = [f"lane {lane} of worldline A shows people {shown[lane] / n:.0%} of the take (under {PEOPLE_SHARE:.0%})"
                       for lane in sorted(shown) if shown[lane] / n < PEOPLE_SHARE]
    shared_problems = [f"the three lanes of worldline A see the same person {together / n:.0%} of the take "
                       f"(under {SHARED_SHARE:.0%})"] if together / n < SHARED_SHARE else []
    return [free,
            {"id": "people", "ok": not people_problems, "problems": people_problems,
             "share": {lane: round(shown[lane] / n, 4) for lane in sorted(shown)}},
            {"id": "shared", "ok": not shared_problems, "problems": shared_problems, "share": round(together / n, 4)}]


def against_recorded(world: PlanWorld, recorded_lines, *, root_m: float = RECORDED_ROOT_M) -> dict:
    """The prediction against a take's master-world-telemetry.jsonl, actor by actor, wherever nobody intervened."""
    worst = {i: (0.0, -1) for i in world.actors}
    compared = dict.fromkeys(world.actors, 0)
    for line in recorded_lines:
        if not line.strip():
            continue
        row = json.loads(line)
        k = int(row["tick"])
        if not 0 <= k < len(world.ticks):
            continue
        for a in row["actors"]:
            mine = world.ticks[k].get(a["id"])
            if mine is None or a.get("released") or not a.get("state"):
                continue
            d = math.hypot(a["state"]["pos"][0] - mine["state"]["pos"][0], a["state"]["pos"][1] - mine["state"]["pos"][1])
            compared[a["id"]] += 1
            if d > worst[a["id"]][0]:
                worst[a["id"]] = (d, k)
    problems = [f"{i}: the take has it {d * 1000:.1f} mm from the plan at {k / 30:.1f} s (over {root_m * 1000:g} mm)"
                for i, (d, k) in worst.items() if d > root_m]
    problems += [f"{i}: the take never logged it" for i, n in compared.items() if n == 0]
    return {"id": "recorded", "ok": not problems, "problems": problems,
            "furthest_mm": {i: round(d * 1000, 3) for i, (d, _) in worst.items()}, "ticks_compared": compared}


def check_stage_script(world: PlanWorld, *, seed: int, recorded_lines=None) -> dict:
    checks = [check_script(world), check_clearance(world), check_set(world), check_stage(world), check_fixed(world),
              *check_cameras(world, seed=seed)]
    if recorded_lines is not None:
        checks.append(against_recorded(world, recorded_lines))
    return {"schema": CHECK_SCHEMA, "ok": all(c["ok"] for c in checks), "scope": "baseline",
            "problems": [p for c in checks for p in c["problems"]], "checks": checks, "not_judged": list(NOT_JUDGED),
            "ground_z": world.header["ground_z"], "ticks": len(world.ticks)}

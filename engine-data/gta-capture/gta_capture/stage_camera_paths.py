"""Free-moving camera paths of the four-worldline lanes (requirement rev 630 §3).

A free lane's take is cut into segments -- seven to ten for two minutes, inside
the requirement's one to ten (§3.1) -- and every segment keeps the camera moving
at a speed a viewer sees:

    orbit_left/right  round the focus at 6-16 deg/s for 6-24 s, the speed
                      swinging within the segment; on the way it may also push
                      in or pull out 1.8-3 m, rise or sink 1.4-2.3 m (an orbit
                      that rises or sinks is the requirement's spiral, 螺旋升降),
                      or look off the focus and back
    rise/descend      straight up or down 1.4-2.3 m at 0.4-0.8 m/s, the view
                      turning off to one side and back on the way (摇动)
    push_in/pull_out  towards or away from the focus 1.8-3 m at 0.6-1.0 m/s,
                      turning off and back the same way

A move eases in from rest and out to rest (minimum jerk), and a segment's ease-out
runs on into the next segment's ease-in, so the camera does not stop between
them. Over a lane all six ways are travelled, a lane rises and sinks straight
once each at least, and every segment carries a move besides the six straight
ways, as measured on the path (measure_moves).

The customer, 2026-10-07: free cameras creeping at 0.1-0.2 m/s looked still
while their keys said they moved, and a rise of 0.4 m over 8 s was an E key with
nothing to see. So moves are 0.4 m/s and up, a rise or sink 1.4 m and more.

stage_tape supplies each lane's focus: the cast's middle in A and C, the logged
seventh person in D (the 2026-10-14 written requirement, differing from the
2026-10-09 oral instruction to watch the event). D2's camera position uses a
separate smoothed anchor within 4 m of the stage centre; its focus and aim remain
the unchanged seventh-person root and root + 0.3 m. Movement labels are measured
against that real focus, not the placement anchor. Each follows its own path.
A path is checked against what the take logged:
nobody within 2 m of the camera, the set and the beach's own things kept clear
of, the focus behind no registered obstacle for long, the requirement's motion
limits with a quarter to spare, every direction travelled, the focus in view.
These geometric checks do not establish visibility through other actors or
pixel-level visibility; the exported video still needs that check.
The path is laid segment by segment, a segment drawn again until it passes; a
lane whose segments cannot all be laid starts over from the next seed, and a
tape holds a path that passed, or its build fails.
"""

from __future__ import annotations

import math
import random
import re
from collections import Counter
from dataclasses import dataclass

from .errors import ContractError
from .tape import _look_at_euler

TICK_HZ = 30
# The requirement's p95 limits for a moving camera; paths keep a quarter below.
LIMITS = {"speed": 6.0, "accel": 8.0, "jerk": 20.0, "rate": 90.0, "ang_accel": 150.0, "ang_jerk": 600.0}
HEADROOM = 0.75
SIX_WAY_MIN_M = 1.2        # travelled each way in the camera's own axes (the customer QC counts 0.8 m)
SIX_WAY_FROM_TICKS = 30 * TICK_HZ   # a take this long must travel all six ways
CLEARANCE_M = 2.0          # from every person's root, all take long
REACH_M = 16.0             # from the stage centre: open sand, but for the set and MAP_OBSTACLES
ROOT_ABOVE_SAND_M = 0.99   # a standing person's root over the sand
VISIBLE_SHARE = 0.8        # of the time the focus is in frame (a look off it turns it out and back)
MAX_TRIES = 400
SEGMENT_TRIES = 60
ORBIT_FLIP = 0.35               # a segment drawn again goes round the other way this often: the planned way may be shut

# What a segment moves at, at its cruise (its ease-in starts at rest, its ease-out ends at rest).
ORBIT_DEG_S = (6.0, 16.0)       # round the focus: 0.47 m/s and up at 4.5 m
WOBBLE = 0.3                    # an orbit's speed swings up to this much either way within its segment
DRIFT_MPS = (0.15, 0.08)        # an orbit drifts nearer or further and up or down this slowly at most (an ellipse,
                                # §3.3): too slow for a key, room to pass the set and the cast
LIFT_M, LIFT_MPS = (1.4, 3.2), (0.4, 0.8)     # its E or Q held over 1 m of it: the key comes on at 0.3 m/s; up to 3.2 m
                                              # since 2026-10-16, from a low camera to a high one (HEIGHT_BANDS)
PUSH_M, PUSH_MPS = (1.8, 3.0), (0.6, 1.0)     # straight on the picture moves least: faster than the rest
LOOK_OFF_DEG = (15.0, 30.0)     # a pan off the focus and back (摇动)
# A look away (the requirement's 左摇／右摇后自然回镜): panned past the frame's edge -- the focus out of it -- held there
# LOOK_AWAY_HOLD_S and panned back smoothly; this far past half the lens's width, eased out and back over LOOK_AWAY_S.
LOOK_AWAY_PAST_DEG, LOOK_AWAY_HOLD_S, LOOK_AWAY_S = (8.0, 15.0), 2.0, (1.2, 1.8)
LOOK_TILT_DEG = 6.0
RAMP_S = (1.0, 1.6)
ORBIT_S = (6.0, 24.0)           # an orbit segment: going round longer than this would look the same all along
STRAIGHT_S = {"lift": (2.5, 5.0), "push": (2.4, 3.0)}   # short enough that the smallest move is fast enough

HIDDEN_MAX_S = 1.5              # the focus behind something once for this long at most (§4.1: 1-2 s)
# Every free lane low, level and high (the customer on 006, the meeting of 2026-10-16: the free cameras' height hardly
# changed -- it was 1.4-5 m and drifted 0.08 m/s at most; low looking up, level, high looking down, more rises and
# sinks): metres over the sand of each band, and how long a lane stays in each at least. The view the band gives is
# measured on the path (measure_moves: VIEW_ANGLES).
HEIGHT_BANDS = {"low": (0.6, 1.1), "level": (1.4, 1.9), "high": (3.2, 5.0)}
BAND_MIN_S = 5.0
# a person's body, for the chase lane keeping clear of the cast (stage_tape)
BODY_RADIUS_M = 0.3
FOCUS_SELF_M = 0.8              # a person this near the played one is the played one himself

# D2 placement -- and C2's since 2026-10-17, when C's played person, turned only while walking, went out to stand 6-15 m
# in front of the stage as D's seventh does: a camera 7-11 m round him there was over the 16 m reach. These do not
# smooth/replace its target or change acceptance limits.
D2_ANCHOR_SIGMA_S = 1.5
D2_ANCHOR_RADIUS_M = 4.0
D2_ANCHOR_CENTRE = (0.0, 1.6)   # the same centre as the unchanged 16 m reach check

# Things of the beach itself the cameras keep clear of and do not look through: world x, y, radius and top over
# the sand. A stage spec's set lines add its own pieces (set_obstacles); these are the map's, found on
# beach-demo-003's exported frames (each from two or three views, tools/locate_obstacle.py, 2026-10-07).
MAP_OBSTACLES = (
    (-1582.95, -1143.14, 0.6, 4.5),   # black feather flag by the set's cooler, stage (5.1, 7.7)
    (-1588.76, -1150.81, 0.6, 4.5),   # green feather flag, stage (-0.8, 15.3)
    (-1557.16, -1153.64, 4.5, 6.5),   # lifeguard tower inland, stage (-11.7, -14.5): legs from x -9.3 (3 views), roof, ramp
)
CAMERA_MARGIN_M = 1.0           # kept clear of an obstacle's radius, unless passing this much over its top
# radius and top over the sand of a set piece, by what its model names: a car, a parasol's canopy, a cooler, a bag;
# anything else as big as a parasol
SET_PIECES = (("parasol", 1.2, 2.6), ("cool", 0.4, 0.6), ("bag", 0.3, 0.4))
VEHICLE = (2.6, 2.6)
OTHER_PROP = (1.2, 2.6)

ORBITS = ("orbit_left", "orbit_right")


@dataclass(frozen=True)
class Room:
    """Where a lane's camera may be, round its focus."""
    d: tuple[float, float]                    # metres from the focus, on the ground
    h: tuple[float, float]                    # metres above the sand
    az: tuple[float, float]                   # degrees round the focus, 0 = in front of it (stage -y)


ROOMS = {
    # worldline A looks at the middle of the cast from the front and the sides of the stage: behind it stand the
    # set, the flags and the cast itself. 0.6-5 m up since 2026-10-16 (HEIGHT_BANDS; 1.6-5 / 1.4-3.6 before)
    "centroid_wide": Room((6.0, 11.0), (0.6, 5.0), (-110.0, 110.0)),
    "centroid_tele": Room((7.0, 11.0), (0.6, 5.0), (-110.0, 110.0)),
}


@dataclass(frozen=True)
class Segment:
    kind: str
    start: int                     # first tick
    ticks: int
    ramp: int                      # ticks of its main move's ease-in, and of its ease-out, which runs on into the next
    turn: float = 0.0              # degrees round the focus, anticlockwise seen from above (the camera going right)
    lift: tuple | None = None      # (first tick, ticks, metres up -- down below 0 --, ticks of its ease-in and -out)
    push: tuple | None = None      # (first tick, ticks, metres further from the focus -- nearer below 0 --, ease ticks)
    look: tuple | None = None      # (first tick, ticks, degrees turned left off the focus and back, degrees tilted up,
                                   #  ticks held off it -- 0 but for a look away)
    wobble: tuple = (0.0, 1.0, 0.0)   # the orbit's speed: amplitude, period in ticks, phase
    drift: tuple = (0.0, 0.0)         # metres an orbit drifts further and up on the way (an ellipse; too slow for keys)


def stage_local(frame, x: float, y: float) -> tuple[float, float]:
    fx, fy, _, heading = frame
    h = math.radians(heading)
    dx, dy = x - fx, y - fy
    return math.cos(h) * dx + math.sin(h) * dy, -math.sin(h) * dx + math.cos(h) * dy


def stage_world(frame, xs: float, ys: float) -> tuple[float, float]:
    fx, fy, _, heading = frame
    h = math.radians(heading)
    return fx + math.cos(h) * xs - math.sin(h) * ys, fy + math.sin(h) * xs + math.cos(h) * ys


def piece_obstacle(x: float, y: float, models: list[str]):
    """A piece of a stage's set at world (x, y) as an obstacle (x, y, radius, top), or None for a towel, which lies
    flat. Of a piece that may be one of several models, the biggest."""
    if all("towel" in m for m in models):
        return None
    if not all(m.startswith("prop_") for m in models):
        return (x, y, *VEHICLE)
    sizes = [next(((r, top) for word, r, top in SET_PIECES if word in m), OTHER_PROP) for m in models]
    return (x, y, max(r for r, _ in sizes), max(top for _, top in sizes))


def set_obstacles(spec_text: str, frame) -> list[tuple]:
    """The pieces a stage spec sets (set ID MODELS X Y HEADING, stage-local) as world obstacles."""
    out = []
    for line in spec_text.splitlines():
        words = line.split("#", 1)[0].split()
        if len(words) == 6 and words[0] == "set":
            piece = piece_obstacle(*stage_world(frame, float(words[3]), float(words[4])), words[2].split("|"))
            if piece:
                out.append(piece)
    return out


def fill_ticks(known: dict[int, list[float]], ticks: int) -> list[list[float]]:
    """Every tick 0..ticks-1 from the logged ones: linear between, held at the ends."""
    if not known:
        raise ContractError("no logged position to follow")
    keys = sorted(known)
    out, j = [], 0
    for k in range(ticks):
        while j + 1 < len(keys) and keys[j + 1] <= k:
            j += 1
        lo = keys[j]
        if k <= keys[0] or lo == k or j + 1 >= len(keys):
            out.append(list(known[keys[0] if k <= keys[0] else lo]))
            continue
        hi = keys[j + 1]
        w = (k - lo) / (hi - lo)
        out.append([known[lo][i] + (known[hi][i] - known[lo][i]) * w for i in range(3)])
    return out


def smooth(series: list[list[float]], sigma_ticks: float) -> list[list[float]]:
    """Gaussian and zero-phase -- the whole take is known -- the ends held."""
    radius = max(1, int(3 * sigma_ticks))
    weights = [math.exp(-0.5 * (i / sigma_ticks) ** 2) for i in range(-radius, radius + 1)]
    total, n = sum(weights), len(series)
    out = []
    for k in range(n):
        acc = [0.0, 0.0, 0.0]
        for i, w in enumerate(weights):
            p = series[min(max(k + i - radius, 0), n - 1)]
            acc[0] += w * p[0]
            acc[1] += w * p[1]
            acc[2] += w * p[2]
        out.append([a / total for a in acc])
    return out


def _ease(t: float) -> float:
    return t * t * t * (10.0 + t * (-15.0 + 6.0 * t))


def _bump(t: float) -> float:
    """0 -> 1 -> 0, flat at both ends up to the curvature."""
    return 64.0 * (t * (1.0 - t)) ** 3


def _plateau(t: float, n: int, hold: int) -> float:
    """0 to 1 and back over n ticks, held at 1 for `hold` of them in the middle, eased (minimum jerk) either side."""
    ease = (n - hold) / 2.0
    return _ease(min(t / ease, 1.0, (n - t) / ease)) if ease > 0 else 1.0


def _wrap(a: float) -> float:
    return (a + 180.0) % 360.0 - 180.0


def _profile(n: int, ramp: int, wobble=(0.0, 1.0, 0.0)) -> list[float]:
    """The share of a move made on each of its n ticks: up to speed over the first `ramp` ticks, down to rest over
    the last `ramp` (the speed itself on a minimum-jerk curve), in between at a speed swinging by the wobble."""
    amp, period, phase = wobble
    shares = []
    for i in range(n):
        t = i + 0.5
        edge = min(1.0, t / ramp, (n - t) / ramp) if ramp > 0 else 1.0
        shares.append(_ease(max(0.0, edge)) * (1.0 + amp * math.sin(2.0 * math.pi * t / period + phase)))
    total = sum(shares)
    return [s / total for s in shares]


# ------------------------------------------------------------------ laying a path

def _plan(rng: random.Random, ticks: int) -> list[dict] | None:
    """The kinds and lengths of a take's segments, ten at most: a few straight lifts or pushes, and orbits of 6-24 s
    filling the rest, left and right both when there are two or more. A take that must travel all six ways gets
    two straight lifts (a rise and a sink of its own: inside an orbit a lift is a spiral, not a rise) and two
    pushes at least, inside orbits where no straight one carries them."""
    seconds = ticks / TICK_HZ
    straight = []
    if ticks >= SIX_WAY_FROM_TICKS:   # three lifts at least: a rise and a sink, and the third height band (2026-10-16)
        straight = ["lift", "lift", "lift"] + [rng.choice(("lift", "push")) for _ in range(rng.randint(0, 2))]
    plan = [{"kind": k, "s": rng.uniform(*STRAIGHT_S[k])} for k in straight]
    rest = seconds - sum(p["s"] for p in plan)
    least = max(1, math.ceil(rest / ORBIT_S[1]))
    most = max(least, min(10 - len(plan), int(rest // ORBIT_S[0])))
    if least > 10 - len(plan):
        return None
    n = rng.randint(least, most)
    for spread in (0.4, 0.3, 0.2, 0.1, 0.0):   # as uneven as the bounds allow
        weights = [rng.uniform(1.0 - spread, 1.0 + spread) for _ in range(n)]
        lengths = [rest * w / sum(weights) for w in weights]
        if n == 1 or ORBIT_S[0] <= min(lengths) <= max(lengths) <= ORBIT_S[1]:
            break
    else:
        return None
    ways = [rng.choice(ORBITS)]
    for _ in range(n - 1):
        ways.append(ways[-1] if rng.random() < 0.3 else ORBITS[1 - ORBITS.index(ways[-1])])
    if n >= 2 and len(set(ways)) == 1:
        ways[rng.randrange(1, n)] = ORBITS[1 - ORBITS.index(ways[0])]
    orbits = [{"kind": k, "s": length} for k, length in zip(ways, lengths)]
    plan += orbits
    rng.shuffle(plan)
    if ticks >= SIX_WAY_FROM_TICKS:
        owed = max(0, 2 - straight.count("push"))
        if owed > len(orbits):
            return None
        for p in rng.sample(orbits, owed):
            p["push"] = True
    for p in orbits:
        roomy = p["s"] >= 8.0   # room for a move of its own besides going round
        for move in ("lift", "push"):
            p[move] = p.get(move) or (roomy and rng.random() < 0.25)
        p["look"] = roomy and rng.random() < 0.2
    roomy = [p for p in orbits if p["s"] >= 8.0]
    if ticks >= SIX_WAY_FROM_TICKS and roomy:   # a look away once at least (2026-10-16: 摇后自然回镜, counted)
        for p in rng.sample(roomy, min(len(roomy), rng.randint(1, 2))):
            p["look_away"], p["look"] = True, False
    start = 0
    for i, p in enumerate(plan):
        p["start"] = start
        p["ticks"] = ticks - start if i == len(plan) - 1 else int(round(p["s"] * TICK_HZ))
        start += p["ticks"]
    return plan if all(p["ticks"] > 0 for p in plan) else None


def _amount(rng, now: float, span: tuple[float, float], size: tuple[float, float], owed: dict, move: str):
    """How far a lift or push goes from `now`, inside `span`: `size` metres, the way still owed first; None when
    the room has not that much either way."""
    ways = [w for w in (1, -1) if span[0] <= now + w * size[0] <= span[1]]
    if not ways:
        return None
    way = rng.choice([w for w in ways if owed.get((move, w))] or ways)
    top = min(size[1], span[1] - now if way > 0 else now - span[0])
    return way * rng.uniform(size[0], top)


def _sub_move(rng, p: dict, now: float, span, size, mps, owed: dict, move: str):
    """A lift or push inside an orbit segment, (first tick, ticks, metres, ease ticks) at a speed of `mps`, or None
    when it does not fit."""
    metres = _amount(rng, now, span, size, owed, move)
    if metres is None:
        return None
    ramp = int(RAMP_S[0] * TICK_HZ)
    room = p["ticks"] - 2 * TICK_HZ // 3
    ticks = int(abs(metres) / rng.uniform(*mps) * TICK_HZ) + ramp
    if ticks > room:
        ticks = int(abs(metres) / mps[1] * TICK_HZ) + ramp
        if ticks > room:
            return None
    first = p["start"] + rng.randint(TICK_HZ // 3, max(TICK_HZ // 3, p["ticks"] - ticks - TICK_HZ // 3))
    return first, ticks, metres, ramp


def _draw(rng, p: dict, state, room: Room, owed: dict, total: int, last: bool, half_fov: float = 0.0) -> Segment | None:
    """One segment of the plan, from where the segments before it left the camera (az, d, h); half_fov: half the
    lens's width in degrees (a look away pans past it)."""
    az, d, h = state
    kind, start, n = p["kind"], p["start"], p["ticks"]
    ramp = min(int(rng.uniform(*RAMP_S) * TICK_HZ), max(1, n // 3))
    span = n if last else min(n + ramp, total - start)   # the main move's ease-out runs on into the next segment
    cruise_s = max(span - ramp, 1) / TICK_HZ
    turn, lift, push, look, wobble, drift = 0.0, None, None, None, (0.0, 1.0, 0.0), (0.0, 0.0)
    if kind in ORBITS:
        turn = rng.uniform(*ORBIT_DEG_S) * cruise_s * (1 if kind == "orbit_right" else -1)
        if p.get("tried") and rng.random() < ORBIT_FLIP:
            turn = -turn
        lo, hi = room.az
        if not lo <= az + turn <= hi:
            turn = -turn
        if not lo <= az + turn <= hi:
            turn = math.copysign(min(abs(turn), max(hi - az, az - lo) - 5.0), hi - az - (az - lo))
            if abs(turn) < 30.0:
                return None
        kind = "orbit_right" if turn > 0 else "orbit_left"
        wobble = (rng.uniform(0.0, WOBBLE), rng.uniform(4.0, 9.0) * TICK_HZ, rng.uniform(0.0, 2.0 * math.pi))
        if p.get("lift"):
            lift = _sub_move(rng, p, h, room.h, LIFT_M, LIFT_MPS, owed, "lift")
            if lift is None:
                return None
        if p.get("push"):
            push = _sub_move(rng, p, d, room.d, PUSH_M, PUSH_MPS, owed, "push")
            if push is None:
                return None
        # the drift leaves a lift and a push a way to go: not into the middle band neither can leave by
        drift = tuple(_drift(rng, now + (move[2] if move else 0.0), bounds, size[0], mps * cruise_s)
                      for now, move, bounds, size, mps in ((d, push, room.d, PUSH_M, DRIFT_MPS[0]),
                                                           (h, lift, room.h, LIFT_M, DRIFT_MPS[1])))
    elif kind == "lift":
        size = (max(LIFT_M[0], LIFT_MPS[0] * cruise_s), min(LIFT_M[1], LIFT_MPS[1] * cruise_s))
        metres = _to_band(rng, h, room.h, size, owed) if size[0] <= size[1] else None
        if metres is None and size[0] <= size[1]:
            metres = _amount(rng, h, room.h, size, owed, "lift")
        if metres is None:
            return None
        lift, kind = (start, span, metres, ramp), ("rise" if metres > 0 else "descend")
    else:
        size = (max(PUSH_M[0], PUSH_MPS[0] * cruise_s), min(PUSH_M[1], PUSH_MPS[1] * cruise_s))
        metres = _amount(rng, d, room.d, size, owed, "push") if size[0] <= size[1] else None
        if metres is None:
            return None
        push, kind = (start, span, metres, ramp), ("pull_out" if metres > 0 else "push_in")
    if p.get("look_away") and half_fov:   # out of the frame, held, and back (左摇／右摇后自然回镜)
        ease = int(rng.uniform(*LOOK_AWAY_S) * TICK_HZ)
        hold = int(LOOK_AWAY_HOLD_S * TICK_HZ)
        ticks = 2 * ease + hold
        if ticks > n - TICK_HZ // 3:
            return None
        look = (start + rng.randint(TICK_HZ // 6, n - ticks - TICK_HZ // 6), ticks,
                (half_fov + rng.uniform(*LOOK_AWAY_PAST_DEG)) * rng.choice((-1, 1)), 0.0, hold)
    elif kind in ("push_in", "pull_out", "rise", "descend") or p.get("look"):   # a straight move turns off and back
        ticks = min(n - TICK_HZ // 3, int(rng.uniform(2.5, 4.0) * TICK_HZ))
        if ticks < int(1.8 * TICK_HZ):
            return None
        look = (start + rng.randint(TICK_HZ // 6, n - ticks - TICK_HZ // 6), ticks,
                rng.uniform(*LOOK_OFF_DEG) * rng.choice((-1, 1)), rng.uniform(-LOOK_TILT_DEG, LOOK_TILT_DEG), 0)
    return Segment(kind, start, n, ramp, turn, lift, push, look, wobble, drift)


def _to_band(rng, now: float, span: tuple[float, float], size: tuple[float, float], owed: dict) -> float | None:
    """A straight lift from `now` into a height band the lane has not stayed in yet (owed ("band", name)), `size`
    metres at most and least; None when none is owed or in reach."""
    reach = []
    for name, (lo, hi) in HEIGHT_BANDS.items():
        if not owed.get(("band", name)):
            continue
        lo, hi = max(lo + 0.2, span[0]), min(hi - 0.2, span[1])   # well inside the band, to stay in it a while
        for way in (1, -1):
            a, b = sorted((way * size[0], way * size[1]))
            a, b = max(a, lo - now), min(b, hi - now)
            if a <= b:
                reach.append((a, b))
    if not reach:
        return None
    a, b = rng.choice(reach)
    return rng.uniform(a, b)


def _band_of(h: float) -> str | None:
    return next((name for name, (lo, hi) in HEIGHT_BANDS.items() if lo <= h <= hi), None)


def _drift(rng, now: float, span: tuple[float, float], leave: float, most: float) -> float:
    """A slow drift from `now` of up to `most` metres inside `span`, ending where a move of `leave` metres can
    still go one way or the other."""
    for _ in range(20):
        to = now + rng.uniform(-most, most)
        if span[0] <= to <= span[1] and (to + leave <= span[1] or to - leave >= span[0]):
            return to - now
    return 0.0


class _Path:
    """A path being laid: what each tick moves on each of az, d and h, what it looks off the focus, and how the
    lane has looked so far (the runs a segment must not stretch past their limits)."""

    def __init__(self, ticks: int, base: tuple[float, float, float], world: PathWorld):
        self.ticks, self.base, self.world = ticks, base, world
        self.inc = [[0.0] * ticks for _ in range(3)]
        self.off = [[0.0] * ticks for _ in range(2)]
        self.poses: list[tuple[list[float], list[float]]] = []
        self.carry = world.fresh_runs()

    def end_state(self) -> tuple[float, float, float]:
        return tuple(self.base[c] + sum(self.inc[c]) for c in range(3))

    def _moves(self, seg: Segment):
        """(channel, first tick, shares, amount) of the segment's moves."""
        if seg.turn:
            span = (self.ticks - seg.start) if seg.start + seg.ticks >= self.ticks else \
                min(seg.ticks + seg.ramp, self.ticks - seg.start)
            yield 0, seg.start, _profile(span, seg.ramp, seg.wobble), seg.turn
            for channel, metres in ((1, seg.drift[0]), (2, seg.drift[1])):
                if metres:
                    yield channel, seg.start, _profile(span, seg.ramp), metres
        for channel, move in ((1, seg.push), (2, seg.lift)):
            if move:
                first, n, metres, ramp = move
                n = min(n, self.ticks - first)
                yield channel, first, _profile(n, min(ramp, n // 3)), metres

    def lay(self, seg: Segment) -> str | None:
        """Lays the segment if its ticks pass, else says why."""
        extra = [{} for _ in range(3)]
        for channel, first, shares, amount in self._moves(seg):
            for i, s in enumerate(shares):
                extra[channel][first + i] = extra[channel].get(first + i, 0.0) + amount * s
        state = [self.base[c] + sum(self.inc[c][:seg.start]) for c in range(3)]
        off = [list(o) for o in self.off]
        if seg.look:
            first, n, pan, tilt, hold = seg.look
            for i in range(n):
                b = _bump((i + 0.5) / n) if not hold else _plateau(i + 0.5, n, hold)
                off[0][first + i] += pan * b
                off[1][first + i] += tilt * b
        poses, runs = [], dict(self.carry)
        for k in range(seg.start, seg.start + seg.ticks):
            for c in range(3):
                state[c] += self.inc[c][k] + extra[c].get(k, 0.0)
            pose = self.world.pose(k, *state, off[0][k], off[1][k])
            trouble = self.world.trouble(k, pose, runs)
            if trouble:
                return trouble
            poses.append(pose)
        for c in range(3):
            for k, v in extra[c].items():
                self.inc[c][k] += v
        self.off = off
        self.poses += poses
        self.carry = runs
        return None


class PathWorld:
    """What a lane's camera moves among, tick by tick: its focus, everybody, the set and the beach."""

    def __init__(self, *, frame, sand_z, focus, look, people, walls, room: Room, focal_mm: float = 0.0):
        self.frame, self.sand_z, self.focus, self.look, self.people = frame, sand_z, focus, look, people
        self.walls, self.room = walls, room
        self.focal_mm = focal_mm
        self.half_fov = math.degrees(math.atan(18.0 / focal_mm)) if focal_mm else 0.0   # 36 mm film back

    def pose(self, k: int, az: float, d: float, h: float, pan: float, tilt: float):
        fx, fy = stage_local(self.frame, self.focus[k][0], self.focus[k][1])
        a = math.radians(az)
        wx, wy = stage_world(self.frame, fx + d * math.sin(a), fy - d * math.cos(a))
        pos = [wx, wy, self.sand_z + h]
        pitch, _, yaw = _look_at_euler(pos, self.look[k])
        return pos, [pitch + tilt, 0.0, yaw + pan]

    def fresh_runs(self) -> dict:
        return {"hidden": 0}

    def hidden(self, k: int, pos: list[float]) -> bool:
        """The focus is behind a thing of the set or the beach, seen from pos (the cast's middle is among the cast:
        somebody between is part of the event)."""
        target = self.look[k]
        lx, ly, lz = (target[c] - pos[c] for c in range(3))
        flat = lx * lx + ly * ly
        if flat < 1e-6:
            return False
        for x, y, r, lo, hi in [(x, y, r, self.sand_z - 1.0, self.sand_z + top) for x, y, r, top in self.walls]:
            t = ((x - pos[0]) * lx + (y - pos[1]) * ly) / flat
            if not 0.02 < t < 0.98:
                continue
            if math.hypot(pos[0] + t * lx - x, pos[1] + t * ly - y) < r and lo <= pos[2] + t * lz <= hi:
                return True
        return False

    def trouble(self, k: int, pose, runs: dict) -> str | None:
        pos = pose[0]
        for q in self.people[k]:
            if math.dist(pos, q) < CLEARANCE_M:
                return f"passes {math.dist(pos, q):.2f} m from a person at {k / TICK_HZ:.1f} s"
        for x, y, r, top in self.walls:
            near = math.hypot(pos[0] - x, pos[1] - y)
            if near < r + CAMERA_MARGIN_M and pos[2] < self.sand_z + top + CAMERA_MARGIN_M:
                return f"comes within {near:.1f} m of a thing {r} m round at {k / TICK_HZ:.1f} s"
        sx, sy = stage_local(self.frame, pos[0], pos[1])
        if math.hypot(sx, sy - 1.6) > REACH_M:
            return f"goes {math.hypot(sx, sy - 1.6):.1f} m from the stage at {k / TICK_HZ:.1f} s"
        runs["hidden"] = runs["hidden"] + 1 if self.hidden(k, pos) else 0
        if runs["hidden"] > HIDDEN_MAX_S * TICK_HZ:
            return f"the focus hidden over {HIDDEN_MAX_S} s at {k / TICK_HZ:.1f} s"
        return None


class _D2PlacementWorld(PathWorld):
    """D2's independent positional anchor; all checks still see the real target."""

    def __init__(self, *, frame, sand_z, focus, look, people, walls, focal_mm: float = 0.0):
        def point(p):
            return isinstance(p, (list, tuple)) and len(p) == 3 and all(
                isinstance(x, (int, float)) and not isinstance(x, bool) and math.isfinite(x) for x in p)

        if not focus or len(focus) != len(look) or len(focus) != len(people):
            raise ContractError("D2 needs equal, nonempty per-tick focus/look/people arrays")
        if not all(point(p) and point(q) for p, q in zip(focus, look)):
            raise ContractError("D2 focus/look must contain finite XYZ positions")
        if any(max(abs(q[i] - p[i] - (0.3 if i == 2 else 0.0)) for i in range(3)) > 1e-9
               for p, q in zip(focus, look)):
            raise ContractError("D2 look must be the unchanged seventh root + 0.3 m Z")
        if not all(point(p) for who in people for p in who):
            raise ContractError("D2 crowd roots must be finite XYZ positions")
        target, actual_look = [list(p) for p in focus], [list(p) for p in look]
        # World-player and controlled-root samples may differ in phase. Retain
        # every supplied person and also require clearance from the exact focus.
        crowd = [[*[list(p) for p in who], list(target[k])] for k, who in enumerate(people)]
        super().__init__(frame=frame, sand_z=sand_z, focus=target, look=actual_look,
                         people=crowd, walls=list(walls), room=ROOMS["centroid_tele"], focal_mm=focal_mm)
        filtered = smooth([list(p) for p in target], D2_ANCHOR_SIGMA_S * TICK_HZ)
        self.placement_anchor = []
        cx, cy = D2_ANCHOR_CENTRE
        for p in filtered:
            x, y = stage_local(frame, p[0], p[1])
            dx, dy = x - cx, y - cy
            scale = D2_ANCHOR_RADIUS_M / math.sqrt(D2_ANCHOR_RADIUS_M**2 + dx*dx + dy*dy)
            wx, wy = stage_world(frame, cx + scale*dx, cy + scale*dy)
            self.placement_anchor.append([wx, wy, sand_z])

    def pose(self, k, az, d, h, pan, tilt):
        ax, ay = stage_local(self.frame, self.placement_anchor[k][0], self.placement_anchor[k][1])
        angle = math.radians(az)
        wx, wy = stage_world(self.frame, ax + d*math.sin(angle), ay - d*math.cos(angle))
        position = [wx, wy, self.sand_z + h]
        pitch, _, yaw = _look_at_euler(position, self.look[k])
        return position, [pitch + tilt, 0.0, yaw + pan]


def _start(rng, room: Room) -> tuple[float, float, float]:
    """Where a path starts: anywhere in the room from which a lift and a push can go one way or the other."""
    az = rng.uniform(room.az[0] + 20, room.az[1] - 20)

    def either_way(span, size):
        lo, hi = span
        while True:
            v = rng.uniform(lo, hi)
            if v + size <= hi or v - size >= lo:
                return v
    return az, either_way(room.d, PUSH_M[0]), either_way(room.h, LIFT_M[0])


def lay_path(rng: random.Random, world: PathWorld) -> tuple[_Path, list[Segment]] | str:
    """A path laid segment by segment, each drawn up to SEGMENT_TRIES times; or why it could not be laid."""
    ticks = len(world.focus)
    plan = _plan(rng, ticks)
    if plan is None:
        return "no plan of segments fits the take"
    path = _Path(ticks, _start(rng, world.room), world)
    owed = {(m, w): ticks >= SIX_WAY_FROM_TICKS for m in ("lift", "push") for w in (1, -1)}
    owed.update({("band", name): ticks >= SIX_WAY_FROM_TICKS for name in HEIGHT_BANDS})
    stayed = Counter()   # ticks in each height band so far
    segments = []
    for i, p in enumerate(plan):
        why = "nothing to draw"
        for tried in range(SEGMENT_TRIES):
            p["tried"] = tried
            seg = _draw(rng, p, path.end_state(), world.room, owed, ticks, i == len(plan) - 1, world.half_fov)
            if seg is None:
                why = "no room for its move"
                continue
            why = path.lay(seg)
            if why is None:
                break
        else:
            return f"segment {i + 1} ({p['kind']} at {p['start'] / TICK_HZ:.0f} s): {why}"
        segments.append(seg)
        for move, metres in (("lift", seg.lift and seg.lift[2]), ("push", seg.push and seg.push[2])):
            if metres:
                owed[(move, 1 if metres > 0 else -1)] = False
        for pos, _ in path.poses[seg.start:seg.start + seg.ticks]:
            stayed[_band_of(pos[2] - world.sand_z)] += 1
        for name in HEIGHT_BANDS:
            if stayed[name] >= BAND_MIN_S * TICK_HZ:
                owed[("band", name)] = False
    missing = [f"{m} {'up' if w > 0 else 'down'}" if m == "lift" else f"{m} {'out' if w > 0 else 'in'}"
               if m != "band" else f"{w} camera for {BAND_MIN_S:g} s" for (m, w), left in owed.items() if left]
    if missing:   # an E, a Q, a W or an S the keys would never show
        return "no " + ", no ".join(missing)
    # every segment at least one move besides the six straight ways (the requirement §3.1), as measured
    spans = [(s.start, s.start + s.ticks) for s in segments]
    measured = measure_moves(path.poses, world.focus, world.look, spans, world.focal_mm)
    for i, seg in enumerate(measured):
        if not any(m["move"] not in STRAIGHT_WAYS for m in seg["moves"]):
            return f"segment {i + 1} moves only straight"
    views = {seg["view"] for seg in measured}
    if ticks >= SIX_WAY_FROM_TICKS and len(views) < 3:   # 平视, 仰视 and 俯视 each a segment's (2026-10-16)
        return "no segment " + " or ".join(v for v in ("平视", "仰视", "俯视") if v not in views)
    return path, segments


# ------------------------------------------------------------------ measuring a path

def _forward(pitch: float, yaw: float) -> list[float]:
    p, h = math.radians(pitch), math.radians(yaw)
    return [-math.sin(h) * math.cos(p), math.cos(h) * math.cos(p), math.sin(p)]


def _axes(pitch: float, yaw: float):
    f = _forward(pitch, yaw)
    h = math.radians(yaw)
    r = [math.cos(h), math.sin(h), 0.0]   # GTA heading h faces (-sin h, cos h); its right hand is (cos h, sin h)
    u = [r[1] * f[2] - r[2] * f[1], r[2] * f[0] - r[0] * f[2], r[0] * f[1] - r[1] * f[0]]
    return f, r, u


def _dot(a, b) -> float:
    return a[0] * b[0] + a[1] * b[1] + a[2] * b[2]


def _p95(values: list[float]) -> float:
    if not values:
        return 0.0
    ordered = sorted(values)
    return ordered[int(0.95 * (len(ordered) - 1))]


def motion_p95(poses) -> dict[str, float]:
    """The six p95s the requirement limits, by finite differences on the 30 Hz grid."""
    pos = [p for p, _ in poses]
    vel = [[(pos[i + 1][c] - pos[i][c]) * TICK_HZ for c in range(3)] for i in range(len(pos) - 1)]
    acc = [[(vel[i + 1][c] - vel[i][c]) * TICK_HZ for c in range(3)] for i in range(len(vel) - 1)]
    jerk = [[(acc[i + 1][c] - acc[i][c]) * TICK_HZ for c in range(3)] for i in range(len(acc) - 1)]
    rate = [[_wrap(poses[i + 1][1][2] - poses[i][1][2]) * math.cos(math.radians(poses[i][1][0])) * TICK_HZ,
             (poses[i + 1][1][0] - poses[i][1][0]) * TICK_HZ] for i in range(len(poses) - 1)]
    ang_acc = [[(rate[i + 1][c] - rate[i][c]) * TICK_HZ for c in range(2)] for i in range(len(rate) - 1)]
    ang_jerk = [[(ang_acc[i + 1][c] - ang_acc[i][c]) * TICK_HZ for c in range(2)] for i in range(len(ang_acc) - 1)]
    norm = lambda rows: [math.sqrt(sum(v * v for v in r)) for r in rows]
    return {"speed": _p95(norm(vel)), "accel": _p95(norm(acc)), "jerk": _p95(norm(jerk)),
            "rate": _p95(norm(rate)), "ang_accel": _p95(norm(ang_acc)), "ang_jerk": _p95(norm(ang_jerk))}


def turn_max(poses) -> float:
    """The fastest the camera turns, deg/s, frame to frame: the whole rotation between two frames' axes. The p95's rate
    is the view direction's (heading by the cosine of the pitch, and the pitch), which leaves out the picture spinning
    about its own middle when a camera looking near straight down or up swings its heading -- beach-demo-102's D lane 2
    went over the crowd at 86 degrees down and spun 773 deg/s for 13 frames, past QC's cap (motion.angspeed_max, 90)."""
    out = 0.0
    for i in range(len(poses) - 1):
        a, b = _axes(poses[i][1][0], poses[i][1][2]), _axes(poses[i + 1][1][0], poses[i + 1][1][2])
        trace = sum(_dot(x, y) for x, y in zip(a, b))
        out = max(out, math.degrees(math.acos(max(-1.0, min(1.0, (trace - 1.0) / 2.0)))) * TICK_HZ)
    return out


def six_ways(poses) -> dict[str, float]:
    """Metres travelled each way in the camera's own axes."""
    out = dict.fromkeys(("forward", "back", "right", "left", "up", "down"), 0.0)
    for i in range(len(poses) - 1):
        f, r, u = _axes(poses[i][1][0], poses[i][1][2])
        step = [poses[i + 1][0][c] - poses[i][0][c] for c in range(3)]
        for (plus, minus), axis in ((("forward", "back"), f), (("right", "left"), r), (("up", "down"), u)):
            v = _dot(step, axis)
            out[plus if v > 0 else minus] += abs(v)
    return out


def visible_share(poses, look: list[list[float]], focal_mm: float, width: int = 1920, height: int = 1080) -> float:
    half_h = math.atan(18.0 / focal_mm)
    half_v = math.atan(18.0 * height / width / focal_mm)
    seen = 0
    for (pos, rot), target in zip(poses, look):
        f, r, u = _axes(rot[0], rot[2])
        v = [target[c] - pos[c] for c in range(3)]
        ahead = _dot(v, f)
        if ahead > 0 and abs(math.atan2(_dot(v, r), ahead)) < 0.9 * half_h and abs(math.atan2(_dot(v, u), ahead)) < 0.9 * half_v:
            seen += 1
    return seen / max(1, len(poses))


def _runs(flags) -> list[tuple[int, int]]:
    """(first, length) of each run of true flags."""
    out, first = [], None
    for k, flag in enumerate(list(flags) + [False]):
        if flag and first is None:
            first = k
        elif not flag and first is not None:
            out.append((first, k - first))
            first = None
    return out


def check_path(poses, *, world: PathWorld, focal_mm: float) -> tuple[list[str], dict]:
    """The whole path against everything a lane must hold to."""
    problems = []
    ticks = len(poses)
    clearance = min((math.dist(p, q) for (p, _), who in zip(poses, world.people) for q in who), default=float("inf"))
    if clearance < CLEARANCE_M:
        problems.append(f"passes {clearance:.2f} m from a person")
    margin = min((math.hypot(p[0] - x, p[1] - y) - r for p, _ in poses for x, y, r, top in world.walls
                  if p[2] < world.sand_z + top + CAMERA_MARGIN_M), default=float("inf"))
    if margin < CAMERA_MARGIN_M:
        problems.append(f"comes within {margin:.2f} m of a thing of the set or the beach")
    local = [stage_local(world.frame, p[0], p[1]) for p, _ in poses]
    reach = max(math.hypot(x, y - 1.6) for x, y in local)
    if reach > REACH_M:
        problems.append(f"goes {reach:.1f} m from the stage")
    p95 = motion_p95(poses)
    for key, limit in LIMITS.items():
        if p95[key] > HEADROOM * limit:
            problems.append(f"{key} p95 {p95[key]:.1f} over {HEADROOM * limit:.1f}")
    fastest = turn_max(poses)
    if fastest > LIMITS["rate"]:
        problems.append(f"turns {fastest:.0f} deg/s at the fastest, over {LIMITS['rate']:.0f}")
    ways = six_ways(poses)
    if ticks >= SIX_WAY_FROM_TICKS:
        short = [k for k, v in ways.items() if v < SIX_WAY_MIN_M]
        if short:
            problems.append("travels too little " + "/".join(short))
    share = visible_share(poses, world.look, focal_mm)
    if share < VISIBLE_SHARE:
        problems.append(f"focus in view {share:.0%} of the time")
    hidden = [world.hidden(k, p) for k, (p, _) in enumerate(poses)]
    longest_hidden = max((n for _, n in _runs(hidden)), default=0) / TICK_HZ
    if longest_hidden > HIDDEN_MAX_S:
        problems.append(f"the focus hidden {longest_hidden:.1f} s at a time")
    report = {"clearance_m": round(clearance, 3), "obstacle_margin_m": round(margin, 3) if world.walls else None,
              "p95": {k: round(v, 3) for k, v in p95.items()}, "turn_max_deg_s": round(fastest, 1),
              "six_ways_m": {k: round(v, 2) for k, v in ways.items()}, "focus_in_view": round(share, 3),
              "focus_hidden": {"share": round(sum(hidden) / max(1, ticks), 4), "longest_s": round(longest_hidden, 2)}}
    return problems, report


# Which way a segment's camera went, measured on its poses (the camera as taped and played), named after the
# requirement's camera-move category tree (rev 637 §3.4, 一级类目-二级类目), in the order of its table. What a
# category leaves open goes in a move's detail: which way it went round or panned. Its 跟随 and 摇后自然回镜 are a
# person's (going with him; panning him out of frame and back); the focus kind alone does not establish them.
LABELS = {
    "left": "平移-水平左移", "right": "平移-水平右移",
    "forward": "平移-多方位拉近", "back": "平移-多方位拉远",
    "up": "平移-垂直上移", "down": "平移-垂直下移",
    "tilt_up": "摇动-基础上摇／下摇／左摇／右摇", "tilt_down": "摇动-基础上摇／下摇／左摇／右摇",
    "pan_left": "摇动-基础上摇／下摇／左摇／右摇", "pan_right": "摇动-基础上摇／下摇／左摇／右摇",
    "orbit_small": "环绕-小幅环绕：0°至90°", "orbit_mid": "环绕-中幅环绕：90°至180°",
    "orbit_large": "环绕-大幅环绕：180°至360°", "orbit_round": "环绕-360°环绕", "orbit_rounds": "环绕-连续多圈环绕",
    "orbit_nearer": "环绕-环绕拉近", "orbit_further": "环绕-环绕拉远",
    "spiral_up": "螺旋升降-螺旋上升", "spiral_down": "螺旋升降-螺旋下降",
    "still": "静止-自由运镜静止片段",
    "pan_left_back": "摇动-左摇后自然回镜", "pan_right_back": "摇动-右摇后自然回镜",
    "follow": "跟随-绑定跟随",
}
# The view a segment takes of its focus, by the camera's pitch over the segment (median): 仰视 looking up from low,
# 俯视 looking down from high, 平视 between (the meeting of 2026-10-16: every free lane with all three)
VIEW_ANGLES = ((1.5, "仰视"), (-8.0, "平视"))   # pitch at or over 1.5 degrees: up (a camera under the person's chest,
                                                # 7-11 m off, looks up 2-6); over -8: level; else 俯视
LOOK_AWAY_MIN_S = 1.5    # the focus out of the frame this long on end: a look away (出画2秒后回镜)
FOLLOW_MIN_M = 1.0       # the focus walked this far in a segment, the camera going along with it: 跟随
LOOK_ZH = {"pan_left": "左摇", "pan_right": "右摇", "tilt_up": "上摇", "tilt_down": "下摇",
           "pan_left_back": "左摇出画后回镜", "pan_right_back": "右摇出画后回镜"}
ORBIT_CLASSES = ((90.0, "orbit_small"), (180.0, "orbit_mid"), (360.0, "orbit_large"), (720.0, "orbit_round"))
ORBIT_MIN_DEG = 30.0     # the requirement's orbit: 30 degrees and more
STRAIGHT_WAYS = ("up", "down", "forward", "back", "left", "right")   # the requirement's pink moves
SPIRAL_MIN_DEG = 30.0    # round the focus while rising or sinking ...
SPIRAL_MIN_M = 1.0       # ... this much
RISING_MPS = 0.15        # rising or sinking, not drifting
MOVE_MIN_M = 1.0         # a translation worth naming
LOOK_MIN_DEG = 10.0      # the requirement's pan: 10 degrees and more
STILL_MIN_S = 3.0        # the requirement's still shot: 3-10 s


UNITS = {**{k: "°" for k in ("tilt_up", "tilt_down", "pan_left", "pan_right", "pan_left_back", "pan_right_back")},
         **{k: "°" for _, k in ORBIT_CLASSES}, "orbit_rounds": "°",
         "still": "s"}   # the rest metres: a translation, a spiral's rise or sink


def view_angle(pitches: list[float]) -> str:
    """平视 / 仰视 / 俯视 of a segment, by its camera's median pitch (VIEW_ANGLES)."""
    pitch = sorted(pitches)[len(pitches) // 2]
    return next((name for least, name in VIEW_ANGLES if pitch >= least), "俯视")


def measure_moves(poses, focus: list[list[float]], look: list[list[float]], spans: list[tuple[int, int]],
                  focal_mm: float = 0.0, person: bool = False) -> list[dict]:
    """Each span's moves, measured: degrees round the focus (anticlockwise seen from above is positive: the camera
    going right), metres up and down, nearer and further, and sideways when not going round; degrees the view
    turned or tilted off the focus, and the tilt that keeping the focus in view made; the longest still. A move is named when it reaches the requirement's size: an orbit 30 degrees (its
    category by the angle gone round), a pan or tilt 10 degrees, a still 3 s; a translation 1 m, nearer or further
    while going round is the orbit's own; a rise or sink while going round 30 degrees or more is a spiral (not split
    into its parts, the requirement's ruling). A pan past half the lens's width (focal_mm) that keeps the focus out of
    the frame LOOK_AWAY_MIN_S on end and back: 左摇／右摇后自然回镜. A focus that is a person (person) walking
    FOLLOW_MIN_M and more, the camera along with it: 跟随 (the cast's middle is nobody to follow). Each segment's
    view (view_angle) goes along."""
    half_fov = math.degrees(math.atan(18.0 / focal_mm)) if focal_mm else math.inf
    out = []
    for a, b in spans:
        bearing = [math.degrees(math.atan2(poses[k][0][1] - focus[k][1], poses[k][0][0] - focus[k][0]))
                   for k in range(a, b)]
        turn = sum(_wrap(bearing[i + 1] - bearing[i]) for i in range(len(bearing) - 1))
        up = down = near = far = left = right = 0.0
        turn_rising = risen = 0.0
        for k in range(a, b - 1):
            p, q = poses[k][0], poses[k + 1][0]
            dz = q[2] - p[2]
            up, down = up + max(dz, 0.0), down + max(-dz, 0.0)
            r0 = math.hypot(p[0] - focus[k][0], p[1] - focus[k][1])
            r1 = math.hypot(q[0] - focus[k + 1][0], q[1] - focus[k + 1][1])
            near, far = near + max(r0 - r1, 0.0), far + max(r1 - r0, 0.0)
            h = math.radians(poses[k][1][2])
            side = math.cos(h) * (q[0] - p[0]) + math.sin(h) * (q[1] - p[1])
            right, left = right + max(side, 0.0), left + max(-side, 0.0)
            if abs(dz) * TICK_HZ >= RISING_MPS:
                turn_rising += _wrap(bearing[k + 1 - a] - bearing[k - a])
                risen += abs(dz)
        pan = tilt_off = 0.0
        out_of_frame = []
        for k in range(a, b):
            pitch, _, yaw = _look_at_euler(poses[k][0], look[k])
            dy, dp = _wrap(poses[k][1][2] - yaw), poses[k][1][0] - pitch
            pan = dy if abs(dy) > abs(pan) else pan
            tilt_off = dp if abs(dp) > abs(tilt_off) else tilt_off
            out_of_frame.append(abs(dy) > half_fov)
        tilt = poses[b - 1][1][0] - poses[a][1][0]
        resting = (math.dist(poses[k + 1][0], poses[k][0]) * TICK_HZ < 0.05
                   and abs(_wrap(poses[k + 1][1][2] - poses[k][1][2])) * TICK_HZ < 0.3 for k in range(a, b - 1))
        still = max((n for _, n in _runs(resting)), default=0) / TICK_HZ
        moves = []   # (move, amount, detail)
        spiral = abs(turn_rising) >= SPIRAL_MIN_DEG and risen >= SPIRAL_MIN_M
        if spiral:
            moves.append(("spiral_up" if up >= down else "spiral_down", round(max(up, down), 2), ""))
        going_round = turn - turn_rising if spiral else turn   # the orbit before and after a spiral is an orbit
        rounding = abs(going_round) >= ORBIT_MIN_DEG
        if rounding:
            kind = next((k for top, k in ORBIT_CLASSES if abs(going_round) < top), "orbit_rounds")
            moves.append((kind, round(abs(going_round), 1), "向右" if going_round > 0 else "向左"))
        if not spiral:
            moves += [(name, round(m, 2), "") for name, m in (("up", up), ("down", down)) if m >= MOVE_MIN_M]
        nearer, further = ("orbit_nearer", "orbit_further") if rounding or spiral else ("forward", "back")
        moves += [(name, round(m, 2), "") for name, m in ((nearer, near), (further, far)) if m >= MOVE_MIN_M]
        if abs(turn) < ORBIT_MIN_DEG and not spiral:
            moves += [(name, round(m, 2), "") for name, m in (("left", left), ("right", right)) if m >= MOVE_MIN_M]
        # the tilt keeping the focus in view makes is a spiral's own (the ruling: not split); a look off it is not
        tilted = tilt_off if spiral or abs(tilt_off) > abs(tilt) else tilt
        away = max((n for _, n in _runs(out_of_frame)), default=0) >= LOOK_AWAY_MIN_S * TICK_HZ   # out of the frame, and back
        pan_name = ("pan_left" if pan > 0 else "pan_right") + ("_back" if away else "")
        moves += [(name, round(abs(deg), 1), LOOK_ZH[name]) for name, deg in ((pan_name, pan),
                                                                          ("tilt_up" if tilted > 0 else "tilt_down", tilted))
                  if abs(deg) >= LOOK_MIN_DEG]
        walked = math.dist(focus[a][:2], focus[b - 1][:2])
        cam_went = [poses[b - 1][0][c] - poses[a][0][c] for c in range(2)]
        focus_went = [focus[b - 1][c] - focus[a][c] for c in range(2)]
        if person and walked >= FOLLOW_MIN_M and math.hypot(*cam_went) >= FOLLOW_MIN_M and \
                (cam_went[0] * focus_went[0] + cam_went[1] * focus_went[1]) / (math.hypot(*cam_went) * walked) > 0.5:
            moves.append(("follow", round(walked, 2), ""))
        if still >= STILL_MIN_S:
            moves.append(("still", round(still, 2), ""))
        out.append({"from_s": round(a / TICK_HZ, 3), "to_s": round(b / TICK_HZ, 3),
                    "moves": [{"move": name, "label": LABELS[name], "amount": amount, "unit": UNITS.get(name, "m"),
                               "detail": detail} for name, amount, detail in moves],
                    "turn_deg": round(turn, 1), "pan_deg": round(pan, 1), "tilt_deg": round(tilted, 1),
                    "view": view_angle([poses[k][1][0] for k in range(a, b)])})
    return out


def build_free_path(*, key: str, seed: int, room: str, focal_mm: float, frame, sand_z: float,
                    focus: list[list[float]], look: list[list[float]], people: list[list[list[float]]],
                    obstacles=(), placement: str = "focus", person: bool = False) -> tuple[list[tuple[list[float], list[float]]], dict]:
    """The first path from seeds (seed, key, 0), (seed, key, 1), ... that is laid whole and passes check_path."""
    if placement == "focus":
        world = PathWorld(frame=frame, sand_z=sand_z, focus=focus, look=look, people=people,
                          walls=[*obstacles, *MAP_OBSTACLES], room=ROOMS[room], focal_mm=focal_mm)
    elif placement == "d2_player_anchor" and key in ("C2", "D2") and room == "centroid_tele" and focal_mm == 50:
        world = _D2PlacementWorld(frame=frame, sand_z=sand_z, focus=focus, look=look, people=people,
                                  walls=[*obstacles, *MAP_OBSTACLES], focal_mm=focal_mm)
    else:
        raise ContractError("placement must be focus, or d2_player_anchor restricted to C2/D2, centroid_tele, 50mm")
    why = Counter()
    for attempt in range(MAX_TRIES):
        rng = random.Random(f"{seed}:{key}:{attempt}")
        laid = lay_path(rng, world)
        if isinstance(laid, str):
            problems = [laid]
        else:
            path, segments = laid
            problems, report = check_path(path.poses, world=world, focal_mm=focal_mm)
            if not problems:
                spans = [(s.start, s.start + s.ticks) for s in segments]
                report.update(attempt=attempt, room=room, segments=[
                    {"kind": s.kind, "from_s": round(s.start / TICK_HZ, 3), "to_s": round((s.start + s.ticks) / TICK_HZ, 3)}
                    for s in segments], moves=measure_moves(path.poses, focus, look, spans, focal_mm, person))
                if placement == "d2_player_anchor":
                    report["placement"] = {"kind": placement, "sigma_s": D2_ANCHOR_SIGMA_S,
                                           "radius_cap_m": D2_ANCHOR_RADIUS_M,
                                           "centre_stage_xy": list(D2_ANCHOR_CENTRE),
                                           "focus_unchanged": True, "exact_target_root_added_to_clearance": True}
                return path.poses, report
        why.update(re.sub(r"-?\d+(\.\d+)?", "#", problem) for problem in problems)
    raise ContractError(f"free camera {key}: no path passed in {MAX_TRIES} tries; most often "
                        + "; ".join(f"{n} x {problem}" for problem, n in why.most_common(3)))

"""The four camera lanes of one four-worldline take, built from what it logged.

Every take is one replay exported four times (the existing lane machinery), so
a take's tape has the usual four lanes, one focal length each:

    take AB   1 fixed (A)    2 free path (A)   3 free path (A)   4 free camera (B)
    take C/D  1 fixed        2 free path       3 chase           4 first person

Each take's fixed camera is one spot and one aim, set before recording (FIXED):
a wide lens that keeps the whole take in view, still all take long. A, C and D
stand in different places -- other sides, heights and distances -- as the
customer asked (2026-09-27).
B is the free camera a person flew during take AB, pose by pose as logged. A
free path is a segmented camera move (stage_camera_paths), each on its own path,
looking at the middle of the cast in A, at the event in C (the played person and
whoever answers him) and round the seventh person in D (FREE_PATHS: the meeting of
2026-10-16 -- the event where there is one, the played person where there is none).
C after its takeover and D are played in the game's third person, as a player
plays it (2026-10-16): the mouse turns the camera, the keys turn and walk the
person. The first-person lane is the person's eye looking where the body faces
(the head goes with it), turning with it with no lag, from
EYE_AHEAD_M out of the head along the view, so the head is never in view. The
chase lane is GTA's follow camera put back together behind the game camera's view
(chase_lane): the same pivot, distance and shoulder offset the game's own camera
keeps, turned by the same mouse -- round the person's side or face as it goes. In C both run on the actor to be taken over from tick 0 (requirement: both
person views record that actor from second 0): before the takeover nobody is
played, the first-person lane rides that actor's head looking the way it faces
and the chase lane rides behind it, and both ease into the played view after it.

Frame f of the tape is replay time f/30 s from the recorder-on frame, the clip
start; stage tick k plays at frame hold_frames + k (tick_for_frame). A tick the
game skipped (a capped 1/15 s step) is interpolated from its neighbours and
counted. The free paths' focus, per frame, goes to config/camera-focus.json:
the delivery's camera_Follow Offset is taken from it.
"""

from __future__ import annotations

import json
import math
import statistics
from pathlib import Path

from .errors import ContractError
from .jsonio import write_json_atomic
from .stage_camera_paths import (BODY_RADIUS_M, FOCUS_SELF_M, MAP_OBSTACLES, ROOT_ABOVE_SAND_M, build_free_path,
                                 fill_ticks, motion_p95, set_obstacles, smooth, stage_local, stage_world)
from .stage_take import TICK_HZ
from .tape import (FOCALS, GTA_COORDINATE_CONVENTION, TAPE_SCHEMA, _look_at_euler, _quaternion_zxy,
                   _source_t_ns, validate_tape)

# The chase takes 35 mm. The first person was 18 mm till 2026-10-16, when the customer found it too wide; the meeting
# of 10-16 gave 35-50, millimetres (the user, 10-16), so 35 mm. It sees people only where the person stands 3 m and
# more off them and faces them: routes are made for it (tools/route_sim, FP_MM). C and D's fixed camera keeps A's
# 24 mm (the customer, 2026-09-27), so a C or D take has two 35s and no 18: the requirement's four lenses at 25% each
# are counted over the batch (worldline_qc quota.focal reports a group's share only).
FP_FOCAL_MM = 35
LANE_ROLES = {
    "AB": {"1": ("fixed", 24), "2": ("free_path", 35), "3": ("free_path", 50), "4": ("free_camera", 18)},
    "C": {"1": ("fixed", 24), "2": ("free_path", 50), "3": ("chase", 35), "4": ("first_person", FP_FOCAL_MM)},
    "D": {"1": ("fixed", 24), "2": ("free_path", 50), "3": ("chase", 35), "4": ("first_person", FP_FOCAL_MM)},
}
# Each free path's focus and geometric room (stage_camera_paths.ROOMS). The meeting of 2026-10-16: a free camera
# watches the event where there is one and goes round the played person where there is none, not the cast's middle
# in every case -- C (an event: the tourist stopped) watches it ("event": the played person and whoever is answering
# him, an actor a rule has taken off its script), D (none: the seventh person only looks on) goes round him
# ("player"); A's two watch the cast's middle (no event, nobody played).
FREE_PATHS = {("AB", "2"): ("centroid", "centroid_wide"), ("AB", "3"): ("centroid", "centroid_tele"),
              ("C", "2"): ("event", "centroid_tele"), ("D", "2"): ("player", "centroid_tele")}
FOCUS_SCHEMA = "dm-gta-stage-camera-focus/1"
EYE_AHEAD_M = 0.20        # the first-person eye, out from the head bone along the view: past the back of the head
                          # and its hair when the view turns round behind the body (a rehearsal had nothing of the
                          # head in view at 12 cm in front of the face)
# The first person's pitch, held: level (the user, 2026-10-18). Till then it was the game camera's, squeezed softly into
# 35 degrees up or down -- the mouse's, which the walkers keep 12 degrees down and swing 3 degrees either way every 17 s
# for the third person's look (stage_robot.hpp kLookPitchDeg): standing still, beach-demo-113's first person nodded
# 5 degrees while the head in the third-person lanes did not.
FP_PITCH = 0.0
# The view smoothed this much (Gaussian sigma, ticks), the same either way in time -- no lag: it moves as the body begins
# to turn and is within 5 degrees of the body throughout. The body's heading wobbles a little as it walks and a tap turns
# it in half a second: at 2 ticks the takes of 2026-10-16 walked as plan A walks (taps, mouse-steered walks, no key
# changes walking: win-pc C3/C4/D6/D7) kept the first person's jerk p95 at 760-1010 deg/s^3, over the requirement's 600;
# at 6, 285-380 (acceleration p95 49-74, 51 deg/s at the fastest).
FP_SMOOTH_TICKS = 6.0
# Since 2026-10-16 the first person looks where the body faces -- the head, which in GTA goes with the body (the
# customer) -- not where the game camera does: the camera is the third person's, the mouse's, and comes round the
# person's side or face. It turns as the body turns, with no lag (the customer, later that day: "开始转身的时候得对上
# 不能有延迟" -- a view brought round slower than the body was tried and dropped the same day): how fast it turns is
# how fast the body does, which the route and the turn style decide (tools/route_sim reports it).
# The eye's height smoothed this much (Gaussian sigma, ticks): the head bone bobs 2-4 cm a step as the person walks, about
# two steps a second, and sways standing (the meeting of 2026-10-16: no bob walking, no idle sway); smoothed so, the bob
# is a tenth of itself. Only the height: smoothed on the ground too, the eye would trail the walking head by 0.3 m.
FP_EYE_Z_SIGMA = 5.0
# Across the ground the eye goes with the body's root (which stands still standing) and only the head's offset from it is
# smoothed, this much (Gaussian sigma, ticks): the idle clip sways the head 2-5 cm about the root, and a first person
# riding the head drifted with it (beach-demo-103's C, standing 75-82 s: the view's way held to the hundredth of a
# degree, the eye wandered 10 cm -- the user, 2026-10-17: no idle head sway in the first person). The root carries the
# walk, so the eye keeps up with the body with no lag.
FP_SWAY_SIGMA = 15.0
# The smoothing above is no fix for the idle: checked on one 7 s stretch for drift only, it let beach-demo-113's standing
# eye swing sideways 5-8 cm (15 at most) every second or two with GTA's own standing idle -- breathing, the weight
# shifting -- in C and D alike (2026-10-18). The first person rides the head (the user, that day: as the customer has
# it, the first person is the person's eyes), so the head itself is to stand still: the played person stands calm
# (the take's stance), and every tape checks the head's standing swing against this.
FP_STAND_SWAY_MAX_M = 0.01   # the head's swing within any 2 s while the person stands (root within 2 cm), at most
# The chase lane: GTA's follow camera, fitted on beach-demo-003's C and D game cameras while they were played in
# third person (rms 2 cm): a pivot over the person's root, the camera back from it along the view and over the right
# shoulder, looking along the view. Since 2026-10-09 the game camera is the eye (the customer: the first-person view
# turns with the face), so the follow camera is put together from the person and the mouse's view instead.
FOLLOW_UP_M = 0.54            # the pivot over the root
FOLLOW_BACK_M = 4.35          # the camera back from the pivot, along the view
FOLLOW_RIGHT_M = 0.40         # and to its right
FOLLOW_REST_PITCH = -8.0      # while nobody plays (C before the takeover): the fitted cameras' median pitch
FOLLOW_PITCH = (-35.0, 10.0)  # the played view's pitch, kept within this (looking up, the camera sinks to the sand)
FOLLOW_MIN_BACK_M = 1.8       # somebody or something in the way pulls the camera in, this near at most
FOLLOW_CLEAR_M = 0.8          # kept from a thing of the set or the beach (its radius and top)
FOLLOW_PERSON_M = 1.0         # and from anybody else's body, beside it and over the head (C's jogger ran right under
                              # the game camera every lap; 0.4 m till 2026-10-16, when the customer saw people walk up to
                              # the camera as if into it: 1-1.5 m, the camera pulling in ahead of them)
FOLLOW_SAND_M = 0.4           # over the sand under the person
PERSON_TOP_M = 0.85           # a standing person's head top over the root
CHASE_SIGMA = 0.3 * TICK_HZ   # the view's turns and the root's sway, smoothed as the game camera eased them
# The chase swings round after the view no faster than this, deg/s, its swing changing by this at most, deg/s^2: the view
# whips round with a body turning on the spot, up to 220 deg/s -- 15 m/s round the person at the chase's distance -- and
# the follow camera comes round behind it. On beach-demo-003 D (2026-10-09) every one of the customer's camera limits held
# then (speed p95 5.1 m/s, rate 69 deg/s, jerk 18 m/s^3 ...); off the person's back over 90 degrees 3.3 s of 120.
CHASE_MAX_RATE, CHASE_MAX_ACCEL = 70.0, 100.0
CHASE_PULL_SIGMA = 0.25 * TICK_HZ   # a pull-in and its way back out, eased; it starts before it is needed
CENTROID_SIGMA = 1.5 * TICK_HZ
CENTROID_REACH_M = 3.0    # the cast's middle, held this near the stage centre
# Each take's fixed camera: where it stands and what it looks at, stage-local x, y and metres over the sand (24 mm:
# LANE_ROLES). Since the customer's 10-13 note and the meeting of 10-16 a fixed camera stands where a
# person would set up a tripod: 1.6 m up at most, looking down 10 deg at most, 3 people and more whole in frame
# (stage_check.FIXED_*); before, the three stood 2.5-4 m up looking down 25-30 deg. Down there people hide each other,
# so the search (tools/fixed_camera_search.py) also keeps nobody mostly hidden over 2 s on end (the played person 1 s).
#   A  stage front left, 1.3 m up, 8.3 m from the cast's middle: the cast about 21% of the frame high, everybody whole
#      all take, nobody hidden over 1 s. Searched on beach-demo-003's AB (its recorded bodies, every 6th tick).
#      On beach-demo-102's AB (the cast in the stage's north half, 2026-10-17) the same camera: everybody whole all take,
#      the cast about 20% high, 8.9 m out, nobody hidden over 0.9 s -- no other spot kept all six whole.
#   C  stage right, 1.5 m up, 7.4 m out, looking down 9.4 deg: the played pacer about 27% of the frame high, the pacer,
#      the pair, the sitter and the tourist whole all take (the jogger 78%: her lap runs wide), nobody hidden over 2.0 s.
#      Searched on beach-demo-102's C (2026-10-17); in front (12-15 m) none kept the tourist or the pacer in sight --
#      hidden 2-10 s on end behind the pair. On beach-demo-103's C (the stage moved, 2026-10-17) that camera hid the
#      woman of the pair 13 s; half a metre on and aimed at (2, 0): C 30% high, nobody hidden over 0.9 s, 7.2 deg down.
#      AB's and D's cameras kept every bar on 103's takes as they were.
#      Since beach-demo-108 (2026-10-17: turned only while walking, the played pacer walks out round the pair and stands
#      6-13 m in front of the stage, see stage-group.beach-demo.json) the stage-right camera had him whole 6% of the time:
#      now in front, 24 m from the cast's middle, 1.5 m up, aimed at (-3, -9) -- everybody whole all take, the pacer
#      about 14% of the frame high, the cast 7%, nobody hidden over 1.9 s (the jogger). Searched on 108's C.
#   D  stage front left, 1.6 m up, 15.9 m out: the seventh person about 14% of the frame high, everybody whole all take
#      with 46 px to spare, nobody hidden over 0.6 s. Searched on beach-demo-102's D (2026-10-17).
#      On beach-demo-108's D (the cast spread north, the seventh person 5-13 m in front of it) that camera cut him off:
#      now 27 m in front, left, 1.6 m up -- everybody whole all take with 51 px to spare, the seventh person about 10% of
#      the frame high, nobody hidden over 0.9 s. 5.4 m from C's: nothing 8 m from it kept the seventh and the cast whole.
#      Then the user (2026-10-17) let the played person go out of the fixed frame -- the requirement asks only for 3 people
#      whole 95% of the time -- for cameras near and people big (fixed_camera_search --min-played 0 --rank size):
#      stage right, 7.8 m from the cast's middle, 1.5 m up -- the pair, the pacer and the sitter whole all take, the
#      tourist 95%, about 16-29% of the frame high (7% from 27 m); the seventh person, out in front, is not in it.
#   Since beach-demo-109 (2026-10-17, the tourist a man; C drawn in to 5.8-9.4 m in front), searched so on 109's takes:
#   C  north-west, behind the pair, 9 m from the cast's middle, 1.5 m up, looking south-east: the pair, the sitter and
#      the tourist whole all take, the pacer 97% (far out in front once taken over), about 20% high (the man of the
#      pair 32%); nobody hidden over 1.5 s.
#   D  north-east, 8.1 m from the cast's middle, 1.6 m up, looking south-west at the seventh person's ground: he, the
#      woman of the pair, the pacer and the sitter whole all take (the man of the pair and the tourist at the frame's
#      edge), about 20-27% of the frame high, the seventh 14%. A first pick (south-west, the cast 30% high) left him out,
#      and the delivery's QC failed it: the requirement's "a moving subject in view 80% of the time, every lane, fixed
#      ones too" takes D's subject for him (worldline_qc geo.subject_visible, 109's first package) -- so D keeps him in
#      view (--min-played 0.8) besides 3 people whole.
# A new group's takes walk differently: search again on each take's own recording before its tape is built (the tool's
# docstring: on 2026-10-09 a camera placed on a guess lost the person's legs for 8 s).
# The three stand in different places at different heights (the customer, 2026-09-27). The search sees the cast and the
# set; that nothing else stands in a view shows only in the export.
FIXED = {
    "AB": ((-3.5, -6.5, 1.3), (0.0, 2.0, 0.9)),
    "C": ((-9.0, 5.0, 1.5), (3.0, -2.0, 0.9)),
    "D": ((7.0, 5.0, 1.6), (-3.0, -8.0, 0.9)),
}


def stage_frame(spec_text: str) -> tuple[float, float, float, float]:
    for line in spec_text.splitlines():
        words = line.split("#", 1)[0].split()
        if words and words[0] == "frame" and len(words) == 5:
            return tuple(float(w) for w in words[1:])   # x, y, ground z, heading
    raise ContractError("stage spec has no frame line")


def _to_world(frame, xs: float, ys: float) -> tuple[float, float]:
    return stage_world(frame, xs, ys)


def tick_for_frame(f: int, hold_frames: int, ticks: int) -> int:
    """The stage tick tape frame f shows: tick k plays at frame hold_frames + k, the hold shows tick 0."""
    return min(max(f - hold_frames, 0), ticks - 1)


def _read_jsonl(path: Path) -> list[dict]:
    with open(path, encoding="utf-8") as f:
        return [json.loads(line) for line in f if line.strip()]


def _wrap(a: float) -> float:
    return (a + 180.0) % 360.0 - 180.0


def _lerp_pose(a: tuple, b: tuple, w: float) -> tuple:
    pos = [a[0][i] + (b[0][i] - a[0][i]) * w for i in range(3)]
    rot = [a[1][i] + _wrap(b[1][i] - a[1][i]) * w for i in range(3)]
    return pos, rot


def _per_tick(poses: dict[int, tuple], ticks: int) -> tuple[list[tuple], int]:
    """Fills every tick 0..ticks-1 from the logged ones; returns the list and how many were interpolated."""
    if not poses:
        raise ContractError("a lane has no logged poses")
    known = sorted(poses)
    out, filled = [], 0
    j = 0
    for k in range(ticks):
        if k in poses:
            out.append(poses[k])
            continue
        filled += 1
        while j + 1 < len(known) and known[j + 1] < k:
            j += 1
        lo = known[j] if known[j] < k else None
        hi = next((t for t in known[j:] if t > k), None)
        if lo is None:
            out.append(poses[hi])
        elif hi is None:
            out.append(poses[lo])
        else:
            out.append(_lerp_pose(poses[lo], poses[hi], (k - lo) / (hi - lo)))
    return out, filled


def _forward(pitch: float, yaw: float) -> list[float]:
    p, h = math.radians(pitch), math.radians(yaw)
    # GTA heading h faces (-sin h, cos h); pitch lifts it
    return [-math.sin(h) * math.cos(p), math.cos(h) * math.cos(p), math.sin(p)]


def first_person_view(body_h: float) -> list[float]:
    """[pitch, roll, yaw] of the played person's eye: where the body faces (the customer, 2026-10-16: the first
    person goes with the head, and in GTA the head goes with the body), level (FP_PITCH). It turns with the body, no
    lag. (2026-10-07 to -16 it looked where the game camera looked, the mouse's view, while the camera stood behind
    the body; till 2026-10-18 at the camera's pitch.)"""
    return [FP_PITCH, 0.0, _wrap(body_h)]


def first_person_views(played: dict[int, tuple]) -> dict[int, list[float]]:
    """The first-person view of every played tick, {tick: [pitch, roll, yaw]}, from {tick: (body heading, head)}."""
    return {k: first_person_view(float(body_h)) for k, (body_h, _) in played.items()}


def _eye(head, view: list[float]) -> list[float]:
    """The eye, EYE_AHEAD_M out from the head bone along the view: in front of the face looking ahead, past the
    back of the head looking round behind the body -- the head itself never in view. (beach-demo-001's eye sat on
    the head bone looking the camera's way and looked out through its own head and scarf whenever the camera swung
    round to the face; till 2026-10-07 the view was kept within 60 degrees of the body instead, and did not follow
    the mouse.)"""
    fwd = _forward(view[0], view[2])
    return [float(head[i]) + EYE_AHEAD_M * fwd[i] for i in range(3)]


def _ray_span(s0: list[float], d: list[float], x: float, y: float, r: float, lo: float,
              hi: float) -> tuple[float, float] | None:
    """Where along s0 + b d (b >= 0, d a unit vector) the ray is inside the upright cylinder at (x, y) of radius r
    from height lo to hi, as (first b, last b); None when it never is."""
    ox, oy = s0[0] - x, s0[1] - y
    a = d[0] * d[0] + d[1] * d[1]
    c = ox * ox + oy * oy - r * r
    if a < 1e-12:   # straight up or down
        if c > 0:
            return None
        b_in, b_out = -math.inf, math.inf
    else:
        half = ox * d[0] + oy * d[1]
        disc = half * half - a * c
        if disc < 0:
            return None
        root = math.sqrt(disc)
        b_in, b_out = (-half - root) / a, (-half + root) / a
    if abs(d[2]) < 1e-12:
        if not lo <= s0[2] <= hi:
            return None
        z_in, z_out = -math.inf, math.inf
    else:
        z_in, z_out = sorted(((lo - s0[2]) / d[2], (hi - s0[2]) / d[2]))
    enter, leave = max(b_in, z_in, 0.0), min(b_out, z_out)
    return (enter, leave) if enter < leave else None


def _swing(series: list[float], max_rate: float, max_accel: float) -> list[float]:
    """Follows series (degrees, unwrapped, one a tick) no faster than max_rate, its rate changing by max_accel at most,
    braking in time to stop on it: a camera swung round after a turn, not whipped."""
    dt = 1.0 / TICK_HZ
    x, v, out = series[0], 0.0, []
    for target in series:
        gap = target - x
        want = math.copysign(min(max_rate, math.sqrt(2.0 * max_accel * abs(gap))), gap)
        v += max(-max_accel * dt, min(max_accel * dt, want - v))
        x += v * dt
        out.append(x)
    return out


def _smooth_views(poses: list[tuple], sigma: float, roots: list[list[float]] | None = None) -> list[tuple]:
    """The first-person lane with its view smoothed (the yaw unwrapped; a Gaussian either side of each tick: no lag),
    the eye put back out from the head along it, its height's bob smoothed away (FP_EYE_Z_SIGMA); given the played
    person's root every tick, the head's sway about it smoothed away too (FP_SWAY_SIGMA)."""
    yaw = [poses[0][1][2]]
    for _, view in poses[1:]:
        yaw.append(yaw[-1] + _wrap(view[2] - yaw[-1]))
    turned = smooth([[y, view[0], 0.0] for y, (_, view) in zip(yaw, poses)], sigma)
    heads = []
    for eye, view in poses:
        f = _forward(view[0], view[2])
        heads.append([eye[i] - EYE_AHEAD_M * f[i] for i in range(3)])
    level = smooth([[h[2], 0.0, 0.0] for h in heads], FP_EYE_Z_SIGMA)   # the head's height, its bob smoothed away
    if roots is not None:   # the head's offset from the root, its sway smoothed away; the root as it is
        off = smooth([[h[0] - r[0], h[1] - r[1], 0.0] for h, r in zip(heads, roots)], FP_SWAY_SIGMA)
        heads = [[r[0] + o[0], r[1] + o[1], h[2]] for h, r, o in zip(heads, roots, off)]
    out = []
    for head, z, (y, p, _) in zip(heads, level, turned):
        new = [p, 0.0, _wrap(y)]
        out.append((_eye([head[0], head[1], z[0]], new), new))
    return out


def standing_sway(poses: list[tuple], roots: list[list[float]], window: int = 60, settle: int = 45,
                  before_off: int = int(3 * FP_SWAY_SIGMA), played_from: int = 0) -> float:
    """The first-person head point's (the eye less EYE_AHEAD_M along the view: what the mouse's looking up and down
    does not move) widest swing within any window of ticks over which the person stands -- the root within 2 cm from
    settle ticks before the window to before_off ticks after it: the stop's settling into the stance (blended in over
    half a second) left to the walk, and the start, which the lane's smoothing (FP_SWAY_SIGMA, even either side, so
    no lag) lets in 1 cm over the second before it (the bouncer stance, 2026-10-18): the idle sway the user does not
    want (2026-10-17). Only while the person is played (from played_from: C's takeover): before it the first person
    rides the actor as the script has him act -- 113's pacer on the phone and pocketing it, his head 26 cm about,
    the baseline the customer has the first person record as it is. 0 when the person never stands that long."""
    worst = 0.0
    for s in range(played_from + settle, max(played_from + settle, len(poses) - window - before_off), window // 4):
        r0 = roots[s]
        if any(math.dist(roots[k][:2], r0[:2]) > 0.02 for k in range(s - settle, s + window + before_off)):
            continue
        pts = []
        for eye, view in poses[s:s + window]:
            f = _forward(view[0], view[2])
            pts.append([eye[i] - EYE_AHEAD_M * f[i] for i in range(3)])
        worst = max(worst, max(math.dist(p, q) for p in pts[::5] for q in pts[::5]))
    return worst


def chase_lane(roots: list[list[float]], views: list[list[float]], played: list[bool],
               others: list[list[list[float]]], walls) -> tuple[list[tuple], dict]:
    """The chase lane, tick by tick: GTA's follow camera behind the person, from its root, its view ([pitch, roll,
    yaw]: the game camera's -- the mouse's -- once played, before that the way the actor faces) and whether it is
    played yet; others: everybody's root (the person's own is left out by distance); walls: (x, y, radius, top over
    the sand) of the set and the beach. The report says how near anybody came to the camera (nearest_person_m,
    from the camera to a body's edge; ticks_person_within_1m).

    As the game's camera, it pulls in to just short of a thing of the set or the beach its boom would meet, and looks
    past people -- somebody walking through the view is a moment's cover -- but never stands in or right by one.
    A pull-in is eased in ahead of it and out after it: the whole take is known."""
    n = len(roots)
    yaw = [views[0][2]]
    for v in views[1:]:   # unwrapped, so it smooths across +-180
        yaw.append(yaw[-1] + _wrap(v[2] - yaw[-1]))
    yaw = _swing(yaw, CHASE_MAX_RATE, CHASE_MAX_ACCEL)
    pitch = [min(max(v[0], FOLLOW_PITCH[0]), FOLLOW_PITCH[1]) if p else FOLLOW_REST_PITCH for v, p in zip(views, played)]
    turned = smooth([[a, b, 0.0] for a, b in zip(yaw, pitch)], CHASE_SIGMA)
    root = smooth(roots, CHASE_SIGMA)
    starts, ways, want, crowded = [], [], [], 0
    for k in range(n):
        f = _forward(turned[k][1], turned[k][0])
        h = math.radians(turned[k][0])
        s0 = [root[k][0] + FOLLOW_RIGHT_M * math.cos(h), root[k][1] + FOLLOW_RIGHT_M * math.sin(h), root[k][2] + FOLLOW_UP_M]
        d = [-f[0], -f[1], -f[2]]
        sand = roots[k][2] - ROOT_ABOVE_SAND_M
        back = FOLLOW_BACK_M
        if d[2] < 0:
            back = min(back, (s0[2] - sand - FOLLOW_SAND_M) / -d[2])
        for x, y, r, top in walls:
            span = _ray_span(s0, d, x, y, r + FOLLOW_CLEAR_M, sand - 1.0, sand + top + FOLLOW_CLEAR_M)
            if span:
                back = min(back, span[0])
        near = [_ray_span(s0, d, q[0], q[1], BODY_RADIUS_M + FOLLOW_PERSON_M, q[2] - ROOT_ABOVE_SAND_M,
                          q[2] + PERSON_TOP_M + FOLLOW_PERSON_M)
                for q in others[k] if math.hypot(q[0] - roots[k][0], q[1] - roots[k][1]) > FOCUS_SELF_M]
        moved = True
        while moved:   # in front of whoever the camera would stand by, and of whoever that puts it by
            moved = False
            for span in near:
                if span and span[0] < back < span[1]:
                    back, moved = span[0], True
        crowded += back < FOLLOW_MIN_BACK_M   # could not keep clear
        starts.append(s0)
        ways.append(d)
        want.append(max(FOLLOW_MIN_BACK_M, back))
    # eased: never further out than wanted (a min over the smoothing's whole reach comes first)
    reach = max(1, int(3 * CHASE_PULL_SIGMA))
    floor = [min(want[max(0, k - reach): k + reach + 1]) for k in range(n)]
    back = [b[0] for b in smooth([[b, 0.0, 0.0] for b in floor], CHASE_PULL_SIGMA)]
    poses = [([starts[k][i] + back[k] * ways[k][i] for i in range(3)], [turned[k][1], 0.0, _wrap(turned[k][0])])
             for k in range(n)]
    gaps = []   # how near anybody else comes to the camera: a body a post of BODY_RADIUS_M up to its head
    for k in range(n):
        cam = poses[k][0]
        least = math.inf
        for q in others[k]:
            if math.hypot(q[0] - roots[k][0], q[1] - roots[k][1]) <= FOCUS_SELF_M:
                continue
            flat = max(0.0, math.hypot(cam[0] - q[0], cam[1] - q[1]) - BODY_RADIUS_M)
            least = min(least, math.hypot(flat, max(0.0, cam[2] - (q[2] + PERSON_TOP_M))))
        gaps.append(least)
    report = {"pivot_up_m": FOLLOW_UP_M, "back_m": FOLLOW_BACK_M, "right_m": FOLLOW_RIGHT_M,
              "rest_pitch_deg": FOLLOW_REST_PITCH, "pulled_in_ticks": sum(b < FOLLOW_BACK_M - 0.01 for b in back),
              "nearest_back_m": round(min(back), 3), "crowded_ticks": crowded,
              "nearest_person_m": round(min(gaps), 3) if gaps and math.isfinite(min(gaps)) else None,
              "ticks_person_within_1m": sum(g < 1.0 for g in gaps),
              "p95": {key: round(v, 3) for key, v in motion_p95(poses).items()}}
    return poses, report


def _near_stage(frame, point: list[float], reach: float) -> list[float]:
    x, y = stage_local(frame, point[0], point[1])
    r = math.hypot(x, y - 1.6)
    if r > reach:
        x, y = x * reach / r, 1.6 + (y - 1.6) * reach / r
    wx, wy = stage_world(frame, x, y)
    return [wx, wy, point[2]]


def build_take_tape(take_dir, *, spec_text: str, take_kind: str, hold_s: float, duration_s: float,
                    takeover_actor: str | None = None, width: int = 1920, height: int = 1080,
                    filmback_id: str = "gta-stage-36mm-horizontal", clip_id: str | None = None,
                    seed: int | None = None) -> dict:
    if take_kind not in LANE_ROLES:
        raise ContractError(f"unknown take {take_kind!r}")
    take_dir = Path(take_dir)
    frame = stage_frame(spec_text)
    ground = frame[2]
    n = int(round(duration_s * 30))
    hold_frames = int(round(hold_s * 30))
    if abs(n / 30 - duration_s) > 1e-9 or abs(hold_frames / 30 - hold_s) > 1e-9:
        raise ContractError("take duration and hold must be whole 1/30 s")
    if seed is None:   # the seed the take was recorded with, so a tape rebuilds the same
        scenario = take_dir / "config" / "scenario.json"
        seed = int(json.loads(scenario.read_text(encoding="utf-8")).get("seed", 0)) if scenario.is_file() else 0
    rows = _read_jsonl(take_dir / "source" / "input-camera-log.jsonl")
    ticks_logged = max(r["tick"] for r in rows) + 1
    ticks = max(1, min(ticks_logged, n - hold_frames))

    # the world the take logged: everybody's root per tick, the cast's middle,
    # and in C the actor to be taken over
    people: dict[int, list[list[float]]] = {}
    middle: dict[int, list[float]] = {}
    person: dict[int, list[float]] = {}
    answering: dict[int, list[list[float]]] = {}   # C: the actors a rule has taken off their script, answering him
    heads: dict[int, tuple] = {}
    roots: list[float] = []
    for w in _read_jsonl(take_dir / "source" / "master-world-telemetry.jsonl"):
        k = int(w["tick"])
        cast = [list(a["state"]["pos"]) for a in w["actors"] if a.get("state")]
        roots += [p[2] for p in cast]
        people[k] = cast + ([list(w["player"]["pos"])] if take_kind in ("C", "D") and w.get("player") else [])
        if cast:
            middle[k] = [sum(p[i] for p in cast) / len(cast) for i in range(3)]
        for a in w["actors"]:
            if take_kind == "C" and a["id"] == takeover_actor and a.get("state"):
                heads[k] = (a["state"]["bones"][2], float(a["state"]["h"]))
                person[k] = list(a["state"]["pos"])
            elif a.get("released") and a.get("state"):
                answering.setdefault(k, []).append(list(a["state"]["pos"]))
    sand = statistics.median(roots) - ROOT_ABOVE_SAND_M if roots else ground
    (at_x, at_y, at_h), (aim_x, aim_y, aim_h) = FIXED[take_kind]
    fixed_at = [*_to_world(frame, at_x, at_y), sand + at_h]
    fixed_aim = [*_to_world(frame, aim_x, aim_y), sand + aim_h]
    fixed_euler = _look_at_euler(fixed_at, fixed_aim)

    # the logged lanes, per tick: (position, [pitch, roll, yaw])
    logged: dict[str, dict[int, tuple]] = {"free_camera": {}, "first_person": {}}
    played = sorted(int(r["tick"]) for r in rows if r.get("controlled"))
    takeover_tick = played[0] if take_kind == "C" and played else None
    played_views: dict[int, tuple] = {}   # tick: (body heading, head)
    game_views: dict[int, tuple] = {}     # tick: (unused, [pitch, roll, yaw]) of the game camera while played: the chase's
    for r in rows:
        k = int(r["tick"])
        g = r.get("game_cam")
        fc = r.get("free_cam")
        if fc:
            logged["free_camera"][k] = (list(fc["pos"]), [fc["rot"][0], 0.0, fc["rot"][2]])
        c = r.get("controlled")
        if c and g:
            person[k] = list(c["pos"])
            played_views[k] = (float(c["h"]), c["head"])
            game_views[k] = ([0.0, 0.0, 0.0], [float(g["rot"][0]), 0.0, float(g["rot"][2])])
        elif k in heads:   # nobody is played yet: ride the head of the actor to be taken over
            view = [0.0, 0.0, heads[k][1]]
            logged["first_person"][k] = (_eye(heads[k][0], view), view)
    for k, view in first_person_views(played_views).items():
        logged["first_person"][k] = (_eye(played_views[k][1], view), view)

    if take_kind == "D" and not person:
        raise ContractError("take D logged no controlled seventh person for its cameras to follow")

    interpolated, per_tick = {}, {}
    for role, _ in LANE_ROLES[take_kind].values():
        if role in logged:
            per_tick[role], interpolated[role] = _per_tick(logged[role], ticks)
    if "first_person" in per_tick:
        fp_roots = [pose[0] for pose in _per_tick({k: (p, [0.0, 0.0, 0.0]) for k, p in person.items()}, ticks)[0]] if person else None
        per_tick["first_person"] = _smooth_views(per_tick["first_person"], FP_SMOOTH_TICKS, fp_roots)
        if fp_roots is not None:
            sway = standing_sway(per_tick["first_person"], fp_roots, played_from=takeover_tick or 0)
            if sway > FP_STAND_SWAY_MAX_M:
                raise ContractError(f"take {take_kind}: the played person's head swings {sway * 100:.1f} cm standing, "
                                    f"and the first person with it (over {FP_STAND_SWAY_MAX_M * 100:.0f} cm: record it "
                                    "again with a calm stance)")
    roles = [role for role, _ in LANE_ROLES[take_kind].values()]
    crowd = []
    if people:
        known, j = sorted(people), 0
        for k in range(ticks):   # who is where, tick by tick (a skipped tick keeps the last)
            while j + 1 < len(known) and known[j + 1] <= k:
                j += 1
            crowd.append(people[known[j]])

    # the chase, behind the person the first-person lane looks out of: played from the takeover in C, from tick 0 in D
    chase, chase_report = None, None
    if "chase" in roles:
        if not person or not crowd:
            raise ContractError(f"take {take_kind} logged nobody for its chase lane to follow")
        played_from = (takeover_tick if takeover_tick is not None else ticks) if take_kind == "C" else 0
        # the chase turns with the game camera -- the mouse -- once the person is played (the customer, 2026-10-16: the
        # third person as a player plays it, round the person's side or face as the mouse takes it); before, behind the
        # actor the way the first person looks
        fp_views = [view for _, view in per_tick["first_person"]]
        cam_views = [view for _, view in _per_tick(game_views, ticks)[0]] if game_views else fp_views
        chase_views = [cam_views[k] if k >= played_from and game_views else fp_views[k] for k in range(ticks)]
        chase, chase_report = chase_lane(fill_ticks(person, ticks), chase_views,
                                         [k >= played_from for k in range(ticks)], crowd,
                                         [*set_obstacles(spec_text, frame), *MAP_OBSTACLES])

    # the free paths, on the stage ticks
    free_paths, focus_frames, reports = {}, {}, {}
    if "free_path" in roles:
        if not people:
            raise ContractError("the take logged no world telemetry: a free path has nobody to look at")
        for lane_id, (role, focal) in LANE_ROLES[take_kind].items():
            if role != "free_path":
                continue
            kind, room = FREE_PATHS[(take_kind, lane_id)]
            if kind == "event":
                # the played person and whoever is answering him, smoothed as the cast's middle is: him alone when
                # nobody is (before the takeover, the actor to be taken over)
                mover = fill_ticks(person, ticks)
                focus = smooth([[sum(p[c] for p in [mover[k], *answering.get(k, [])]) / (1 + len(answering.get(k, [])))
                                 for c in range(3)] for k in range(ticks)], CENTROID_SIGMA)
            elif kind == "player":
                # The actual controlled root, interpolating only missing ticks. The
                # centroid's stage clamp and smoothing would move this target away
                # from the seventh person, and make FollowOffset describe a proxy.
                focus = fill_ticks(person, ticks)
            else:
                focus = smooth([_near_stage(frame, p, CENTROID_REACH_M) for p in fill_ticks(middle, ticks)],
                               CENTROID_SIGMA)
            look = [[p[0], p[1], p[2] + 0.3] for p in focus]
            # the camera placed round a point near the stage, looking at the played person far out in front of it
            # (D's seventh; C's played person too since 2026-10-17, stage_camera_paths.D2_ANCHOR_*)
            placement = {"placement": "d2_player_anchor"} if (take_kind, lane_id, kind) in (("D", "2", "player"), ("C", "2", "event")) else {}
            free_paths[lane_id], report = build_free_path(
                key=f"{take_kind}{lane_id}", seed=seed, room=room, focal_mm=focal, frame=frame, sand_z=sand,
                focus=focus, look=look, people=crowd, obstacles=set_obstacles(spec_text, frame),
                person=kind in ("event", "player"), **placement)
            reports[lane_id] = {"focus": kind, **report}
            focus_frames[lane_id] = {"kind": kind, "frames": [
                [round(v, 6) for v in focus[tick_for_frame(f, hold_frames, ticks)]] for f in range(n)]}

    lanes = {}
    for lane_id, (role, focal) in LANE_ROLES[take_kind].items():
        f_px = width * focal / 36.0
        samples = []
        for f in range(n):
            k = tick_for_frame(f, hold_frames, ticks)
            if role == "fixed":
                pos, euler = fixed_at, fixed_euler
            else:
                p, rot = free_paths[lane_id][k] if role == "free_path" else chase[k] if role == "chase" else per_tick[role][k]
                pos = list(p)
                euler = [float(rot[0]), 0.0, float(rot[2])]
            samples.append({
                "frame": f,
                "source_t_ns": _source_t_ns(f),
                "position": [round(float(v), 9) for v in pos],
                "quaternion": [round(v, 12) for v in _quaternion_zxy(euler)],
                "euler_deg": [round(float(v), 9) for v in euler],
                "intrinsics": {"fx": f_px, "fy": f_px, "cx": width / 2, "cy": height / 2},
            })
        lanes[lane_id] = {"focal_mm": focal, "resolution": {"width": width, "height": height}, "samples": samples}
    if any(focal not in FOCALS for _, focal in LANE_ROLES[take_kind].values()):
        raise ContractError("a take's lanes must each use 18, 24, 35 or 50 mm")
    tape = {
        "schema": TAPE_SCHEMA,
        "clip_id": clip_id or f"stage-take-{take_kind}",
        "fps_num": 30,
        "fps_den": 1,
        "duration_s": duration_s,
        "n_frames": n,
        "timestamp_semantics": "source_replay_timeline",
        "filmback_id": filmback_id,
        "coordinate_convention": GTA_COORDINATE_CONVENTION,
        "source": {"kind": "gta_stage_take", "take_kind": take_kind, "delivery_eligible": False,
                   "roles": {lane: role for lane, (role, _) in LANE_ROLES[take_kind].items()},
                   "hold_frames": hold_frames, "stage_ticks": ticks, "ticks_per_second": TICK_HZ,
                   "interpolated_ticks": interpolated, "seed": seed, "free_paths": reports,
                   "fixed": {"position": [round(v, 6) for v in fixed_at], "aim": [round(v, 6) for v in fixed_aim]},
                   "first_person": {"yaw": "body", "pitch_deg": FP_PITCH, "eye_ahead_m": EYE_AHEAD_M,
                                    "smooth_ticks": FP_SMOOTH_TICKS, "eye_height_smooth_ticks": FP_EYE_Z_SIGMA,
                                    "head_sway_smooth_ticks": FP_SWAY_SIGMA, "standing_sway_max_m": FP_STAND_SWAY_MAX_M,
                                    "takeover_tick": takeover_tick},
                   "chase": chase_report},
        "lanes": lanes,
    }
    tape = validate_tape(tape)
    if focus_frames:
        write_json_atomic(take_dir / "config" / "camera-focus.json",
                          {"schema": FOCUS_SCHEMA, "n_frames": n, "lanes": focus_frames})
    return tape

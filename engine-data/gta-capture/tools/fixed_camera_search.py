"""Where a take's fixed camera should stand, searched on the take's own recording.

    python tools/fixed_camera_search.py --group <group dir> --take D [--height 1.3:1.6] [--distance 6:14]
        [--region -16:6,-16:6] [--apart 8] [--max-down 10] [--min-people 3] [--max-hidden-s 2] [--top 10]

Run it once the take is recorded and before its tape is built and exported (the fixed camera has to see
what the take really did: on 2026-10-09 D's camera, placed on a guess of where the seventh person would
stroll, lost his legs for 8 s when he walked up to it). It reads the take's
source/master-world-telemetry.jsonl -- everybody's 7 bone points, every tick -- and the stage spec, and
tries fixed cameras on a grid of stage-local positions, heights and aim points (the take's own fixed lens,
stage_tape.LANE_ROLES, 24 mm in every take -- on the stage's 36 mm filmback, 1920 x 1080,
aiming 0.9 m over the sand, like every fixed lane).

For each camera: the share of the take each person is whole in frame (all 7 bone points inside the edges by
OVER_M at the person's distance, and by 2% at least: a head's top, a sole and a finger stick out past their
bone points -- on D's first-person take of 2026-10-09 the biggest camera kept the bone points 2% in and cut
the feet 29 px when he walked up to it), the played person's size (median height of
his bone box over the frame's; AB has nobody played: the cast's median size) and its distance from the
cast's middle at tick 0 (the requirement's "机位距人物群体初始距离"), and how long each person is mostly
hidden behind somebody nearer the camera (everybody a post PERSON_R round the middle of their bone points, from
the sand to over the head; mostly hidden = 4 of the 7 bone points behind another post).

A fixed camera stands where a person would set up a tripod (the customer, 2026-10-13 and the meeting of 10-16):
1.6 m up at most, looking down --max-down degrees at most, at the event's middle, with 3 people and more in it --
not the whole cast. Down there people hide each other: on 003's C one camera hid the played person for 10.8 s on
end, the one next to it 0.2 s (win-pc, 2026-10-15).

Kept: the played person whole for --min-played of the take (0 since 2026-10-17, the user: the requirement only; PR #962
had its own bar of 0.98), --min-people people whole
(the played person among them) for --min-people-share of it (0.95); with --all-whole, as till 2026-10-16,
everybody else for --min-others (0.99) and the jogger for --min-jogger (0.90: his lap runs wide); nobody mostly
hidden for more than --max-hidden-s on end (2 s: the requirement lets a view be blocked 1-2 s at a time), the
played person --max-hidden-played-s (1 s); clear of the map's and the set's obstacles by their radius + 1 m and of
everybody's path by 2 m; --apart metres from the other takes' fixed cameras (stage_tape.FIXED: the requirement
wants the three in different places, and different heights and distances across set-ups -- --height and
--distance pick the band); within --region. When no camera keeps everybody in sight that long, the least hidden
are printed instead, to choose from by eye.
Printed: the best by the played person's size -- checked again on every tick (the grid skips --every - 1 of
each --every), with --min-spare pixels to spare at his tightest besides (on that take the biggest cameras
that passed every tick kept 2-13 px: a step or an arm swung differently would cut him) -- each with the
line to put into stage_tape.FIXED. Rebuild
the take's tape (stage-take-tape) afterwards; a tape built before must have its lane 1 exported again.
"""
import argparse
import json
import math
import statistics
import sys
from pathlib import Path

import numpy as np

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from gta_capture.stage_camera_paths import (  # noqa: E402
    MAP_OBSTACLES, ROOT_ABOVE_SAND_M, TICK_HZ, _axes, set_obstacles, stage_local, stage_world)
from gta_capture.stage_take import StageScenario  # noqa: E402
from gta_capture.stage_tape import FIXED, LANE_ROLES, _look_at_euler, stage_frame  # noqa: E402

FX, W, H = 1280.0, 1920, 1080   # the take's fixed lens on a 36 mm wide filmback (main sets FX: 24 mm 1280, 18 mm 960)
EDGE = 0.02                     # bone points kept this far inside the frame at least ...
OVER_M = 0.15                   # ... and this far at the person's distance: the body past its bone points
AIM_H = 0.9
PATH_CLEAR_M = 2.0
WALL_CLEAR_M = 1.0
AIM_AWAY_M = 2.0                # an aim point at least this far out from under the camera (not looking straight down)
PERSON_R = 0.25                 # a person hiding somebody behind: a post this far round the middle of their bone points ...
PERSON_OVER_M = 0.15            # ... from the sand to this far over their head
HIDDEN_POINTS = 4               # of a person's 7 bone points behind another post: the person is mostly hidden


def band(text: str) -> tuple[float, float]:
    lo, hi = (float(v) for v in text.split(":"))
    return lo, hi


def load(group: Path, take: str):
    take_dir = group / "takes" / take
    scenario = StageScenario.load(take_dir / "config" / "scenario.json")
    spec_text = scenario.spec_path.read_text(encoding="utf-8")
    played = scenario.raw["take"].get("takeover_actor") if take == "C" else ("player" if take == "D" else None)
    rows = [json.loads(line) for line in open(take_dir / "source" / "master-world-telemetry.jsonl", encoding="utf-8")]
    return spec_text, played, rows


def bones(rows, played, every: int):
    ids = sorted({a["id"] for r in rows for a in r["actors"] if (a.get("state") or {}).get("bones")})
    if played == "player":
        ids.append("player")
    picked = rows[::every]
    pts = np.full((len(picked), len(ids), 7, 3), np.nan)
    for t, r in enumerate(picked):
        for a in r["actors"]:
            st = a.get("state") or {}
            if st.get("bones") and a["id"] in ids:
                pts[t, ids.index(a["id"])] = st["bones"]
        p = r.get("player") or {}
        if p.get("bones"):
            if played == "player":
                pts[t, ids.index("player")] = p["bones"]
            elif played in ids and np.isnan(pts[t, ids.index(played), 0, 0]):   # C: taken over, logged as the player
                pts[t, ids.index(played)] = p["bones"]
    return ids, pts


def hiding(pts, frame, sand, at, every):
    """Each person's share of the ticks mostly hidden behind somebody nearer the camera at `at`, and the longest such
    stretch in seconds (`every`: the ticks `pts` keeps). A bone point is hidden when the sight line to it goes
    through another person's post on its way."""
    cam = np.array([*stage_world(frame, at[0], at[1]), sand + at[2]])
    middle = np.nanmean(pts[:, :, :, :2], axis=2)                   # each person's middle on the ground
    top = np.nanmax(pts[:, :, :, 2], axis=2) + PERSON_OVER_M
    share, longest = np.zeros(pts.shape[1]), np.zeros(pts.shape[1])
    for p in range(pts.shape[1]):
        d = pts[:, p] - cam
        flat = d[:, :, :2]
        hid = np.zeros(d.shape[:2], dtype=bool)
        with np.errstate(invalid="ignore", divide="ignore"):
            n2 = np.sum(flat * flat, axis=2)
            for q in range(pts.shape[1]):
                if q == p:
                    continue
                to_q = middle[:, q] - cam[:2]
                t = np.sum(flat * to_q[:, None, :], axis=2) / n2      # where along the sight line it passes nearest q
                off = flat * t[:, :, None] - to_q[:, None, :]
                z = cam[2] + d[:, :, 2] * t
                hid |= (t > 0) & (t < 0.97) & (np.sum(off * off, axis=2) < PERSON_R ** 2) & (z > sand) & (z < top[:, q, None])
        seen = ~np.isnan(pts[:, p, 0, 0])
        mostly = (hid.sum(axis=1) >= HIDDEN_POINTS) & seen
        share[p] = mostly.sum() / max(int(seen.sum()), 1)
        run = most = 0
        for m in mostly:
            run = run + 1 if m else 0
            most = max(most, run)
        longest[p] = most * every / TICK_HZ
    return share, longest


def down_deg(at, aim) -> float:
    """How far a camera at `at` (stage-local x, y, height over the sand) looks down to the aim point (x, y) at AIM_H."""
    return math.degrees(math.atan2(at[2] - AIM_H, math.dist(at[:2], aim[:2])))


def evaluate(pts, frame, sand, at, aim):
    pos = np.array([*stage_world(frame, at[0], at[1]), sand + at[2]])
    pitch, _, yaw = _look_at_euler(list(pos), [*stage_world(frame, aim[0], aim[1]), sand + aim[2]])
    f, r, u = (np.array(v) for v in _axes(pitch, yaw))
    d = pts - pos
    z = d @ f
    with np.errstate(invalid="ignore", divide="ignore"):
        x = W / 2 + FX * (d @ r) / z
        y = H / 2 - FX * (d @ u) / z
        over = OVER_M * FX / z
        mx, my = np.maximum(over, W * EDGE), np.maximum(over, H * EDGE)
        room = np.where(z > 0.3, np.minimum.reduce([x - mx, W - mx - x, y - my, H - my - y]), -np.inf)
    seen = ~np.isnan(pts[:, :, 0, 0])
    whole_t = np.where(seen, (room >= 0).all(axis=2), False)   # tick by tick, who is whole in the frame
    whole = np.where(seen, (room >= 0).all(axis=2), True).mean(axis=0)
    size = np.nanmedian(np.where(seen, (np.nanmax(y, axis=2) - np.nanmin(y, axis=2)) / H, np.nan), axis=0)
    spare = np.nanmin(np.where(seen[:, :, None], room, np.nan), axis=(0, 2))   # px inside the margin at the tightest
    return whole, size, spare, whole_t.sum(axis=1)   # ... and how many are whole, tick by tick (--min-people)


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--group", required=True)
    ap.add_argument("--take", required=True, choices=("AB", "C", "D"))
    ap.add_argument("--region", default="-16:6,-16:6", help="stage-local x band, y band (metres)")
    ap.add_argument("--height", type=band, default=(1.3, 1.6),
                    help="metres over the sand, lo:hi (a tripod: 1.6 m at most, the meeting of 2026-10-16)")
    ap.add_argument("--max-down", type=float, default=10.0, help="degrees the camera may look down at most")
    ap.add_argument("--min-people", type=int, default=3, help="people whole in frame at once (the meeting: 3 and more)")
    ap.add_argument("--min-people-share", type=float, default=0.95, help="... for this share of the take")
    ap.add_argument("--all-whole", action="store_true", help="everybody whole, as till 2026-10-16 (--min-others, --min-jogger)")
    ap.add_argument("--max-hidden-s", type=float, default=2.0,
                    help="seconds on end anybody whole in frame may be mostly hidden behind somebody nearer the camera")
    ap.add_argument("--max-hidden-played-s", type=float, default=1.0, help="the same for the played person")
    ap.add_argument("--distance", type=band, default=(4.0, 20.0), help="metres from the cast's middle at tick 0, lo:hi")
    ap.add_argument("--apart", type=float, default=8.0, help="metres from the other takes' fixed cameras")
    ap.add_argument("--aims", default="-3:3,-5:2", help="stage-local band the aim point is tried in, x band, y band")
    ap.add_argument("--step", type=float, default=0.5)
    ap.add_argument("--every", type=int, default=3, help="use every n-th tick")
    # 0 since 2026-10-17 (the user: the requirement only, 3 people whole 95% of the time); 0.98 was PR #962's own bar
    ap.add_argument("--min-played", type=float, default=0.0)
    ap.add_argument("--min-others", type=float, default=0.99)
    ap.add_argument("--min-jogger", type=float, default=0.90)
    ap.add_argument("--min-spare", type=float, default=20.0, help="px the played person keeps inside the margin")
    ap.add_argument("--top", type=int, default=10)
    # people: the most people whole at once first, then the biggest played person (till 2026-10-17); size: the biggest
    # --min-people-th person among those whole for --min-people-share of the take -- with --min-played 0, the
    # requirement alone (3 people whole 95% of the time), as big as it goes: the user, 2026-10-17, "机位尽量近、人尽量大".
    # (Not the median of everybody kept: that put a camera on the chatting pair 8 m off, the rest out of frame.)
    ap.add_argument("--rank", choices=("people", "size"), default="size")
    a = ap.parse_args(argv)

    global FX
    FX = W * LANE_ROLES[a.take]["1"][1] / 36.0   # the take's fixed lens
    group = Path(a.group)
    spec_text, played, rows = load(group, a.take)
    frame = stage_frame(spec_text)
    roots = [st["pos"][2] for r in rows for x in r["actors"] if (st := x.get("state"))]
    sand = statistics.median(roots) - ROOT_ABOVE_SAND_M
    ids, every_tick = bones(rows, played, 1)
    pts = every_tick[::a.every]
    k_played = ids.index(played) if played in ids else None
    if a.min_played <= 0:   # the played person need not be in frame: judged like anybody else
        k_played = None
    cast0 = [x["state"]["pos"] for x in rows[0]["actors"] if x.get("state")]
    middle0 = stage_local(frame, sum(p[0] for p in cast0) / len(cast0), sum(p[1] for p in cast0) / len(cast0))
    walls = [(x, y, r) for x, y, r, _ in list(MAP_OBSTACLES) + list(set_obstacles(spec_text, frame))]
    paths = pts[:, :, 2, :2].reshape(-1, 2)
    paths = paths[~np.isnan(paths[:, 0])]
    others_fixed = [stage_xy for take, ((*stage_xy, _), _) in FIXED.items() if take != a.take]

    def verdict(whole, people):
        ok = True
        if a.all_whole:
            rest = [i for i in range(len(ids)) if i != k_played and ids[i] != "jogger"]
            ok = all(whole[i] >= a.min_others for i in rest)
            if "jogger" in ids:
                ok = ok and whole[ids.index("jogger")] >= a.min_jogger
        else:   # 3 people and more at once, most of the take
            ok = float(np.mean(people >= a.min_people)) >= a.min_people_share
        if k_played is not None:
            ok = ok and whole[k_played] >= a.min_played
        return ok

    limits = [a.max_hidden_played_s if i == k_played else a.max_hidden_s for i in range(len(ids))]

    def hidden_over(longest, whole=None):   # the worst person's longest stretch mostly hidden, over what they are allowed;
        # without --all-whole, only of the people the camera keeps (whole half the take and more)
        keep = [i for i in range(len(ids)) if a.all_whole or whole is None or whole[i] >= 0.5 or i == k_played]
        return max(longest[i] / limits[i] for i in keep) if keep else 0.0

    def size_of(size):
        return size[k_played] if k_played is not None else float(np.nanmedian(size))

    def line(at, aim, whole, size, spare, dist, share, longest):
        people = ", ".join(f"{i} {w * 100:.1f}%/{s * 100:.1f}%/{h * 100:.0f}% {n:.1f}s"
                           for i, w, s, h, n in zip(ids, whole, size, share, longest))
        tight = spare[k_played] if k_played is not None else float(np.nanmin(spare))
        return (f"{'played' if k_played is not None else 'cast'} {size_of(size) * 100:.1f}% high, {tight:.0f} px to spare"
                f" at the tightest, looking down {down_deg(at, aim):.1f} deg, {dist:.1f} m from the cast's middle at tick 0"
                f" | whole/high/hidden (share, longest): {people}\n"
                f"      \"{a.take}\": (({at[0]:.1f}, {at[1]:.1f}, {at[2]:.2f}), ({aim[0]:.1f}, {aim[1]:.1f}, {AIM_H})),")

    (cx, cy, ch), (tx0, ty0, _) = FIXED[a.take]
    whole, size, spare, people = evaluate(every_tick, frame, sand, (cx, cy, ch), (tx0, ty0, AIM_H))
    print(f"{a.take}: {len(ids)} people ({', '.join(ids)}), played {played}, {every_tick.shape[0]} ticks; sand {sand:.2f} m; "
          f"lens {FX * 36 / W:g} mm")
    now_fits = verdict(whole, people)
    share, longest = hiding(every_tick, frame, sand, (cx, cy, ch), 1)
    now_ok = now_fits and hidden_over(longest, whole) <= 1.0 and down_deg((cx, cy, ch), (tx0, ty0)) <= a.max_down and ch <= a.height[1]
    print("now   ", "meets the bars" if now_ok else "FAILS the bars", "|",
          line((cx, cy, ch), (tx0, ty0), whole, size, spare, math.dist((cx, cy), middle0), share, longest))

    (x0, x1), (y0, y1) = (band(b) for b in a.region.split(","))
    (ax0, ax1), (ay0, ay1) = (band(b) for b in a.aims.split(","))
    heights = np.arange(a.height[0], a.height[1] + 1e-9, 0.1)
    aims = [(tx, ty) for tx in np.arange(ax0, ax1 + 1e-9, 1.0) for ty in np.arange(ay0, ay1 + 1e-9, 1.0)]
    found = []
    for x in np.arange(x0, x1 + 1e-9, a.step):
        for y in np.arange(y0, y1 + 1e-9, a.step):
            dist = math.dist((x, y), middle0)
            if not a.distance[0] <= dist <= a.distance[1] or any(math.dist((x, y), o) < a.apart for o in others_fixed):
                continue
            wx, wy = stage_world(frame, x, y)
            if any(math.hypot(wx - ox, wy - oy) < r + WALL_CLEAR_M for ox, oy, r in walls):
                continue
            if np.min(np.hypot(paths[:, 0] - wx, paths[:, 1] - wy)) < PATH_CLEAR_M:
                continue
            for h in heights:
                for aim in aims:
                    if math.dist((x, y), aim) < AIM_AWAY_M or down_deg((x, y, h), aim) > a.max_down:
                        continue
                    whole, size, _, people = evaluate(pts, frame, sand, (x, y, h), (*aim, AIM_H))
                    if verdict(whole, people):
                        kept = sorted((s for w, s in zip(whole, size) if w >= a.min_people_share), reverse=True)
                        big = float(kept[a.min_people - 1]) if len(kept) >= a.min_people else 0.0
                        found.append((size_of(size), (round(x, 2), round(y, 2), round(h, 2)), aim, whole, size, dist,
                                      round(float(np.mean(people)), 1), big))
    if a.rank == "size":
        found.sort(key=lambda c: -c[7])   # the biggest people kept
    else:
        found.sort(key=lambda c: (-c[6], -c[0]))   # the most people whole at once, then the biggest
    best, spots = [], set()
    for c in found:   # the best aim of each spot
        if c[1] not in spots:
            spots.add(c[1])
            best.append(c)
    hid = [hiding(pts, frame, sand, c[1], a.every) for c in best]   # a spot hides the same whatever it aims at
    clear = [(c, h) for c, h in zip(best, hid) if hidden_over(h[1], c[3]) <= 1.0]
    confirmed = []
    for c, _ in clear:   # again on every tick: a tick the grid skipped may cut a foot, or hide somebody longer
        whole, size, spare, people = evaluate(every_tick, frame, sand, c[1], (*c[2], AIM_H))
        if not verdict(whole, people) or (k_played is not None and spare[k_played] < a.min_spare):
            continue
        share, longest = hiding(every_tick, frame, sand, c[1], 1)
        if hidden_over(longest, whole) <= 1.0:
            confirmed.append((c[1], c[2], whole, size, spare, c[5], share, longest))
            if len(confirmed) == a.top:
                break
    hidden_bar = f"nobody kept in frame mostly hidden for over {a.max_hidden_s:g} s on end" + (
        f" (the played person {a.max_hidden_played_s:g} s)" if k_played is not None else "")
    print(f"{len(found)} cameras meet the in-frame bars on every {a.every} ticks at {len(spots)} spots, "
          f"{a.height[0]:g}-{a.height[1]:g} m up and looking down {a.max_down:g} deg at most; {len(clear)} of the spots "
          f"keep {hidden_bar}; the best {len(confirmed)} that meet every bar on every tick, {a.min_spare:g} px to spare:")
    for c in confirmed:
        print("  ", line(*c))
    if not confirmed and best:
        print(f"none does; the least hidden spots, to choose from by eye ({hidden_bar} would be 1.00):")
        for (c, (share, longest)) in sorted(zip(best, hid), key=lambda x: (hidden_over(x[1][1], x[0][3]), -x[0][0]))[:a.top]:
            whole, size, spare, _ = evaluate(every_tick, frame, sand, c[1], (*c[2], AIM_H))
            share, longest = hiding(every_tick, frame, sand, c[1], 1)
            print(f"   {hidden_over(longest, whole):.2f}", line(c[1], c[2], whole, size, spare, c[5], share, longest))
    return 0 if confirmed else 1


if __name__ == "__main__":
    sys.exit(main())

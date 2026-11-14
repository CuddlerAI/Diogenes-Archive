"""Lanes 1 and 2 of a simulated C or D take, built by the tape's own code.
    SP=<tables dir> SPEC=<stage spec> WORLD=<AB master-world-telemetry.jsonl>         PYTHONPATH=. python tools/route_sim/lanecheck.py <route_sim PATHLOG file> C|D
(SP holds cast.csv; the sand height is read off WORLD, as stage_tape reads it off the take's telemetry.)"""
import json, math, os, random, re, statistics, collections, sys
from gta_capture.stage_camera_paths import (MAP_OBSTACLES, MAX_TRIES, ROOMS, PathWorld, build_free_path, check_path,
                                            fill_ticks, lay_path, set_obstacles, smooth, stage_world)
from gta_capture.stage_tape import CENTROID_REACH_M, CENTROID_SIGMA, FIXED, _near_stage, stage_frame
SP = os.environ['SP']
SEED = os.environ.get('SEED', '2026092801')   # the group config's seed: the tape draws its free path with it
path_file, kind = sys.argv[1], sys.argv[2]
spec_text = open(os.environ['SPEC'], encoding='utf-8').read()
frame = stage_frame(spec_text)
ab = [json.loads(l) for l in open(os.environ['WORLD'])]
sand = statistics.median([a['state']['pos'][2] for r in ab for a in r['actors'] if a.get('state') and a['id'] in ('chat_a', 'chat_b', 'sitter')]) - 0.99
cast = [list(map(float, l.split())) for l in open(SP + '/cast.csv')]
rows = [list(map(float, l.split())) for l in open(path_file)]
N = 3600; me, crowd = {}, [None] * N
start = 330 if kind == 'C' else 0
for r in rows:
    k = int(round(r[1] * 30))
    if 0 <= k < N:
        me[k] = [*stage_world(frame, r[2], r[3]), sand + 0.99]
        people = [[*stage_world(frame, r[7 + 2 * j], r[8 + 2 * j]), sand + 0.99] for j in range(5)]
        if kind == 'D': people.append([*stage_world(frame, cast[k][10], cast[k][11]), sand + 0.99])   # the pacer walks in D
        crowd[k] = people
for k in range(start):
    me[k] = [*stage_world(frame, cast[k][10], cast[k][11]), sand + 0.99]
    crowd[k] = [[*stage_world(frame, cast[k][2 * j], cast[k][2 * j + 1]), sand + 0.99] for j in range(5)]
for k in range(N):
    if crowd[k] is None: crowd[k] = crowd[k - 1]
# the cast's six actors -- in C the one taken over among them, in D the seventh person not -- and their middle, as
# stage_tape takes it: every free path looks at it (the customer, 2026-10-09)
cast6 = [crowd[k] + ([me[k]] if kind == 'C' and k in me else []) for k in range(N)]
middle = {k: [sum(p[i] for p in c) / len(c) for i in range(3)] for k, c in enumerate(cast6)}
focus = smooth([_near_stage(frame, p, CENTROID_REACH_M) for p in fill_ticks(middle, N)], CENTROID_SIGMA)
look = [[p[0], p[1], p[2] + 0.3] for p in focus]
world = PathWorld(frame=frame, sand_z=sand, focus=focus, look=look, people=crowd, room=ROOMS["centroid_tele"],
                  walls=[*set_obstacles(spec_text, frame), *MAP_OBSTACLES])
passed, kinds = 0, collections.Counter()
for attempt in range(MAX_TRIES):
    rng = random.Random(f"{SEED}:{kind}2:{attempt}")
    laid = lay_path(rng, world)
    problems = [laid] if isinstance(laid, str) else check_path(laid[0].poses, world=world, focal_mm=50)[0]
    if not problems: passed += 1
    for p in problems: kinds[re.sub(r'[\d.]+', '#', p)] += 1
poses, report = build_free_path(key=f"{kind}2", seed=int(SEED), room="centroid_tele", focal_mm=50, frame=frame,
                                sand_z=sand, focus=focus, look=look, people=crowd, obstacles=set_obstacles(spec_text, frame))
print(f"{os.path.basename(path_file)}: lane 2 free path {passed}/{MAX_TRIES} attempts pass; the tape takes attempt {report['attempt']}; "
      f"the cast's middle in view {report['focus_in_view']:.1%}")
if True:
    for k, n in kinds.most_common(4): print(f'     {n:4d}  {k}')
(ax, ay, ah), (tx, ty, th) = FIXED[kind]
def v3(x, y, h): return [*stage_world(frame, x, y), sand + h]
cam, aim = v3(ax, ay, ah), v3(tx, ty, th)
f = [aim[i] - cam[i] for i in range(3)]; n = math.sqrt(sum(c * c for c in f)); f = [c / n for c in f]
right = [f[1], -f[0], 0.0]; rn = math.hypot(right[0], right[1]); right = [right[0] / rn, right[1] / rn, 0.0]
up = [right[1] * f[2], -right[0] * f[2], right[0] * f[1] - right[1] * f[0]]
fpx = 1920 * 24 / 36
def proj(p):
    d = [p[i] - cam[i] for i in range(3)]
    z = sum(d[i] * f[i] for i in range(3)); x = sum(d[i] * right[i] for i in range(3)); y = sum(d[i] * up[i] for i in range(3))
    return 960 + fpx * x / z, 540 - fpx * y / z
inside = 0
for k in range(start, N):
    u1, v1 = proj([me[k][0], me[k][1], sand + 0.05]) if k in me else (0, 0)
    u2, v2 = proj([me[k][0], me[k][1], sand + 1.75]) if k in me else (0, 0)
    if 0 <= min(u1, u2) and max(u1, u2) <= 1920 and 0 <= min(v1, v2) and max(v1, v2) <= 1080: inside += 1
print(f"     lane 1 fixed camera: the played person whole in frame {inside / (N - start):.1%} of the time")

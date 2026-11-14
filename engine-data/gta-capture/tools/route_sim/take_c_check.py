"""What route_sim promised, checked on a recorded C (after stage-take-tape): how near the played person came to each other
person, the keys the program pressed, the longest stand, how much of the time the chase camera was behind the body, and the
tape's lanes 1-3 (lane 1: the played person whole in the fixed camera).
    python take_c_check.py <take dir>"""
import json, math, os, sys
T = os.path.abspath(sys.argv[1])
take = os.path.basename(T)
def wrap(a): return (a + 180.0) % 360.0 - 180.0
log = [json.loads(l) for l in open(os.path.join(T, "source", "input-camera-log.jsonl"), encoding="utf-8")]
world = {}
for l in open(os.path.join(T, "source", "master-world-telemetry.jsonl"), encoding="utf-8"):
    r = json.loads(l)
    world[r["tick"]] = {a["id"]: a["state"]["pos"] for a in r["actors"] if a.get("state") and a["id"] != "pacer"}
first = min(r["tick"] for r in log if r.get("controlled"))
print(f"take {take}: played from tick {first} ({first/30:.1f} s), {len(log)} logged ticks")
# how near, horizontally, root to root
close = {}
for r in log:
    c = r.get("controlled")
    if not c or r["tick"] not in world: continue
    for who, p in world[r["tick"]].items():
        d = math.dist(c["pos"][:2], p[:2])
        if d < close.get(who, (9, 0))[0]: close[who] = (d, r["tick"] / 30)
need = {"chat_a": 0.8, "chat_b": 0.9, "tourist": 0.72, "sitter": 1.2, "jogger": 1.5}
print("closest (m, at s; needs):", "  ".join(f"{w} {close[w][0]:.2f} @{close[w][1]:.1f} (>={need[w]}){'' if close[w][0] >= need[w] else ' <<< TOO NEAR'}" for w in need if w in close))
# the keys
held = {}
presses = {87: [], 65: [], 68: []}
seen = set()
down_since = {}
for r in log:
    ks = set(r.get("keyCode") or [])
    if r["tick"] < first: continue
    seen |= ks
    for k in presses:
        if k in ks and k not in down_since: down_since[k] = r["tick"]
        if k not in ks and k in down_since: presses[k].append((r["tick"] - down_since.pop(k)) / 30)
names = {87: "W", 65: "A", 68: "D", 83: "S", 81: "Q", 69: "E"}
print("keys pressed:", "".join(names.get(k, str(k)) for k in sorted(seen)), "| presses", {names[k]: len(v) for k, v in presses.items()},
      "| shortest %.2f s" % min([x for v in presses.values() for x in v] or [0]), "| S/Q/E pressed:", bool(seen & {83, 81, 69}))
alone = sum(1 for r in log if r["tick"] >= first and (set(r.get("keyCode") or []) & {65, 68}) and 87 not in (r.get("keyCode") or []))
print("ticks with A or D but no W:", alone)
# stand and chase
pos = {r["tick"]: r["controlled"]["pos"] for r in log if r.get("controlled")}
run = best = best_at = 0
for k in range(first + 1, max(pos)):
    sp = math.dist(pos[k][:2], pos[k - 1][:2]) * 30 if k in pos and k - 1 in pos else 9
    run = run + 1 if sp < 0.1 else 0
    if run > best: best, best_at = run, k - run
print(f"longest stand: {best/30:.1f} s from {best_at/30:.1f} s")
back = tot = 0
for r in log:
    if r["tick"] < first + 60 or not r.get("controlled") or not r.get("game_cam"): continue
    tot += 1; back += abs(wrap(r["game_cam"]["rot"][2] - r["controlled"]["h"])) <= 60.0
print(f"chase camera within 60 deg of behind the body: {100*back/tot:.1f}% ")
# lanes from the tape
tape = json.load(open(os.path.join(T, "config", "camera-tape.json"), encoding="utf-8"))
fp = tape["source"]["free_paths"]["2"]
print(f"lane 2: free path attempt {fp['attempt']}, clearance {fp['clearance_m']} m, focus in view {fp['focus_in_view']}, p95 speed {fp['p95']['speed']} m/s")
s3 = tape["lanes"]["3"]["samples"]
v = sorted(math.dist(s3[f]["position"], s3[f - 1]["position"]) * 30 for f in range(1, len(s3)))
print(f"lane 3 (chase): p95 speed {v[int(0.95*len(v))]:.2f} m/s, max {v[-1]:.2f} m/s")
cam, aim = tape["source"]["fixed"]["position"], tape["source"]["fixed"]["aim"]
f = [aim[i] - cam[i] for i in range(3)]; n = math.sqrt(sum(c * c for c in f)); f = [c / n for c in f]
right = [f[1], -f[0], 0.0]; rn = math.hypot(right[0], right[1]); right = [right[0] / rn, right[1] / rn, 0.0]
up = [right[1] * f[2], -right[0] * f[2], right[0] * f[1] - right[1] * f[0]]
fpx = float(tape["lanes"]["1"]["samples"][0]["intrinsics"]["fx"])   # the fixed lane's own lens
def proj(p):
    d = [p[i] - cam[i] for i in range(3)]
    z = sum(d[i] * f[i] for i in range(3))
    return 960 + fpx * sum(d[i] * right[i] for i in range(3)) / z, 540 - fpx * sum(d[i] * up[i] for i in range(3)) / z
inside = cnt = 0
for k, p in pos.items():
    u1, v1 = proj([p[0], p[1], p[2] - 0.94]); u2, v2 = proj([p[0], p[1], p[2] + 0.76])   # feet to head-top of a root at 0.94 m
    cnt += 1
    inside += 0 <= min(u1, u2) and max(u1, u2) <= 1920 and 0 <= min(v1, v2) and max(v1, v2) <= 1080
print(f"lane 1 (fixed camera): the played person whole in frame {100*inside/cnt:.1f}% of the time after the takeover")

"""A recorded take's first-person lane (C's video 8, D's video 12): the share of frames with a person in view, counted by
body points (root, spine, head, hands, feet): 'half' = 4+ of the 7 inside the lane's frame (its own focal length, off
the tape: 18 mm till 2026-10-16, 35 mm since), 'any' = at least one.
The count of subject_bones.py (which reads C and D of beach-demo-003 from fixed paths), for one take; the 5 s map is
what route_sim prints with FPMAP, to compare. Run after stage-take-tape, on the machine that holds the take:
    python first_person_check.py <take dir>"""
import json, math, os, sys
take = os.path.abspath(sys.argv[1])
tape = json.load(open(os.path.join(take, "config", "camera-tape.json"), encoding="utf-8"))
hold = int(tape["source"]["hold_frames"])
s = tape["lanes"]["4"]["samples"]
fx = float(s[0]["intrinsics"]["fx"])   # the lane's own lens
print(f"first-person lane at {tape['lanes']['4']['focal_mm']} mm (fx {fx:.1f})")
bodies = {}
for l in open(os.path.join(take, "source", "master-world-telemetry.jsonl"), encoding="utf-8"):
    r = json.loads(l)
    me = r.get("player")["pos"] if r.get("player") else None
    bodies[r["tick"]] = [a["state"]["bones"] for a in r["actors"]
                         if not (me and math.dist(a["state"]["pos"][:2], me[:2]) < 0.3)]   # not the played body itself
half = anyone = 0
gaps, run = [], 0
bins = [0] * 24
for f in range(3600):
    smp = s[hold + f]
    pos, (pitch, roll, yaw) = smp["position"], smp["euler_deg"]
    p, y = math.radians(pitch), math.radians(yaw)
    fwd = (-math.sin(y) * math.cos(p), math.cos(y) * math.cos(p), math.sin(p))
    right = (math.cos(y), math.sin(y), 0.0)
    up = (right[1] * fwd[2] - right[2] * fwd[1], right[2] * fwd[0] - right[0] * fwd[2], right[0] * fwd[1] - right[1] * fwd[0])
    best = 0
    for bones in bodies.get(f, bodies.get(f - 1, [])):
        inside = 0
        for q in bones:
            d = [q[i] - pos[i] for i in range(3)]
            z = sum(d[i] * fwd[i] for i in range(3))
            if z < 0.3: continue
            u = 960 + fx * sum(d[i] * right[i] for i in range(3)) / z
            v = 540 - fx * sum(d[i] * up[i] for i in range(3)) / z
            inside += 0 <= u <= 1920 and 0 <= v <= 1080
        best = max(best, inside)
    half += best >= 4; anyone += best >= 1
    bins[f // 150] += best >= 4
    run = 0 if best >= 4 else run + 1
    if run == 1: gaps.append([f, f])
    elif run > 1: gaps[-1][1] = f
long = sorted(((b - a + 1) / 30, a / 30) for a, b in gaps)[::-1][:8]
print(f"{os.path.basename(take)} first-person lane: more than half a person in view {100*half/3600:.1f}% ({half/30:.1f} s), any part of a person {100*anyone/3600:.1f}% ({anyone/30:.1f} s)")
print(f"   needs 80.0% (96.0 s): {'PASS' if half >= 2880 else 'FAIL'}")
print(f"   longest stretches without (seconds long, from second): {[(round(a, 1), round(b, 1)) for a, b in long]}")
print("   visible by 5 s (%): " + " ".join(f"{i*5}:{100*b/150:.0f}" for i, b in enumerate(bins)))

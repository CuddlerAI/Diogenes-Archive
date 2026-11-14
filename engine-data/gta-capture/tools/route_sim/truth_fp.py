"""first_person_check.py's count per frame for a recorded C take: prints 'F tick points' like route_sim's REPLAY_DUMP,
to compare frame by frame.   python truth_fp.py <take dir>"""
import json, math, sys
from pathlib import Path
D = Path(sys.argv[1])
tape = json.load(open(D / "config" / "camera-tape.json"))
hold = int(tape["source"]["hold_frames"]); s = tape["lanes"]["4"]["samples"]; fx = float(s[0]["intrinsics"]["fx"])
bodies = {}
for l in open(D / "source" / "master-world-telemetry.jsonl"):
    r = json.loads(l)
    me = r["player"]["pos"] if r.get("player") else None
    bodies[r["tick"]] = [a["state"]["bones"] for a in r["actors"] if not (me and math.dist(a["state"]["pos"][:2], me[:2]) < 0.3)]
for f in range(330, 3600):
    smp = s[hold + f]; pos = smp["position"]; pitch, roll, yaw = smp["euler_deg"]
    p, y = math.radians(pitch), math.radians(yaw)
    fwd = (-math.sin(y) * math.cos(p), math.cos(y) * math.cos(p), math.sin(p)); right = (math.cos(y), math.sin(y), 0.0)
    up = (right[1] * fwd[2] - right[2] * fwd[1], right[2] * fwd[0] - right[0] * fwd[2], right[0] * fwd[1] - right[1] * fwd[0])
    best = 0
    for bones in bodies.get(f, bodies.get(f - 1, [])):
        inside = 0
        for q in bones:
            d = [q[i] - pos[i] for i in range(3)]
            z = sum(d[i] * fwd[i] for i in range(3))
            if z < 0.3: continue
            u = 960 + fx * sum(d[i] * right[i] for i in range(3)) / z; v = 540 - fx * sum(d[i] * up[i] for i in range(3)) / z
            inside += 0 <= u <= 1920 and 0 <= v <= 1080
        best = max(best, inside)
    print("F", f, best)

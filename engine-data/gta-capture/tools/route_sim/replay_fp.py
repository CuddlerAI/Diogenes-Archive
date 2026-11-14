"""A recorded C take as a replay file for route_sim's REPLAY mode: checks the simulator's first-person count
against the recorded take (truth_fp.py counts the same frames from the delivered tape).
Columns: t x y body cam pitch, then chat_a chat_b tourist jogger sitter x y -- stage axes, C's ticks from the takeover.
    python replay_fp.py <take dir: source/ holds the logs> <stage spec> > replay.txt"""
import json, math, sys
from pathlib import Path
D = Path(sys.argv[1]) / "source"
ox, oy, _, hd = next(tuple(float(w) for w in line.split("#", 1)[0].split()[1:]) for line in open(sys.argv[2], encoding="utf-8")
                     if line.split("#", 1)[0].split()[:1] == ["frame"])
c, s = math.cos(math.radians(hd)), math.sin(math.radians(hd))
def st(x, y): return c * (x - ox) + s * (y - oy), -s * (x - ox) + c * (y - oy)
def wrap(a): return (a + 180.0) % 360.0 - 180.0
ids = ["chat_a", "chat_b", "tourist", "jogger", "sitter"]
world = {}
for l in open(D / "master-world-telemetry.jsonl"):
    r = json.loads(l)
    world[int(r["tick"])] = {a["id"]: a["state"]["pos"] for a in r["actors"] if a.get("state")}
last = None
for l in open(D / "input-camera-log.jsonl"):
    r = json.loads(l)
    k = int(r["tick"])
    w = world.get(k) or last
    last = w
    if not r.get("controlled") or not r.get("game_cam") or not w: continue
    px, py = st(*r["controlled"]["pos"][:2])
    row = [k / 30, px, py, wrap(r["controlled"]["h"] - hd), wrap(r["game_cam"]["rot"][2] - hd), r["game_cam"]["rot"][0]]
    for i in ids: row += list(st(*w[i][:2]))
    print(" ".join(f"{v:.4f}" for v in row))

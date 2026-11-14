"""A rehearsal of a group's stage (controller command stage_rehearse: the cast plays its plan, nothing is recorded), with
each actor's toes over the ground summed up -- the lower toe's height, how often a toe is under the sand, and against
where the model's own stand (ped_look, 2026-10-17).
    python tools/stage_rehearse.py --group <group dir> --tag NAME [--set contact:*:off] [--watch chat_b] [--seconds 20]
        [--measure NAME | --cal <calibration>] [--spec <stage spec>] [--loc x,y,z]
--set replaces or adds a spec line (":" for the spaces); --measure measures the clips into <tmp>/rehearse-cal-NAME.txt
(a group's calibration is made so). Keep it under ~45 s: the reply carries every third tick's toes and a longer one
breaks the pipe (Errno 22). The raw reply goes to <tmp>/rehearse-NAME.json."""
import argparse, json, os, re, statistics, sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
sys.stdout.reconfigure(encoding="utf-8")
from gta_capture.protocol import JsonLinePipeClient
ap = argparse.ArgumentParser()
ap.add_argument("--group", required=True)
ap.add_argument("--tmp", default=r"D:\workspace\mc4\tmp")
ap.add_argument("--tag", required=True)
ap.add_argument("--spec", default="")
ap.add_argument("--set", action="append", default=[])
ap.add_argument("--watch", default="")
ap.add_argument("--from", dest="from_s", type=float, default=0.0)
ap.add_argument("--seconds", type=float, default=30.0)
ap.add_argument("--distance", type=float, default=2.5)
ap.add_argument("--measure", default="", help="measure the calibration into tmp\\rehearse-cal-NAME.txt")
ap.add_argument("--cal", default="", help="a calibration file instead of the group's")
ap.add_argument("--loc", default="", help="x,y,z the player waits at (default: the group's location)")
a = ap.parse_args()
G = a.group
text = open(a.spec or os.path.join(G, "stage-spec.txt"), encoding="utf-8").read()
for line in [s.replace(":", " ") for s in a.set]:   # "legik:2:32" for "legik 2 32" (no quoting through ssh)
    key = line.split()[0]
    if re.search(rf"(?m)^{re.escape(key)}( .*)?$", text): text = re.sub(rf"(?m)^{re.escape(key)}( .*)?$", line, text, count=1)
    else: text = re.sub(r"(?m)^(ticks .*)$", r"\1\n" + line, text, count=1)
tmp = a.tmp
spec = os.path.join(tmp, f"rehearse-{a.tag}-spec.txt")
open(spec, "w", encoding="utf-8", newline="\n").write(text)
loc = [float(v) for v in a.loc.split(",")] if a.loc else json.load(open(os.path.join(G, "group.json"), encoding="utf-8"))["environment"]["location"]
payload = {"spec_path": spec, "watch": a.watch, "from_s": a.from_s,
           "seconds": a.seconds, "distance_m": a.distance, "x": loc[0], "y": loc[1], "z": loc[2]}
if a.measure: payload["calibration_out"] = os.path.join(tmp, f"rehearse-cal-{a.measure}.txt")
else: payload["calibration_path"] = a.cal or os.path.join(G, "stage-calibration.txt")
NATURAL = {"chat_a": 0.040, "chat_b": 0.0245, "pacer": 0.0395, "tourist": 0.030, "jogger": 0.031}   # ped_look, standing
c = JsonLinePipeClient()
try:
    r = c.request("stage_rehearse", payload)
finally:
    c.close()
json.dump(r, open(os.path.join(tmp, f"rehearse-{a.tag}.json"), "w", encoding="utf-8"))
print("set:", a.set, "| scene failures", r.get("scene_failures"), "releases", r.get("runtime_releases"))
for actor, rows in (r.get("toes_over_ground_m") or {}).items():
    lo = [min(v for v in row[1:] if v is not None) for row in rows if any(v is not None for v in row[1:])]
    both = [v for row in rows for v in row[1:] if v is not None]
    if not lo: continue
    q = lambda xs, p: sorted(xs)[int(p * (len(xs) - 1))]
    print(f"  {actor:8s} lower toe cm: p5 {q(lo,.05)*100:5.1f} med {statistics.median(lo)*100:5.1f} p95 {q(lo,.95)*100:5.1f}"
          f" | under 0: {sum(v < 0 for v in lo) / len(lo):4.0%} | any toe under 0: {sum(v < 0 for v in both) / len(both):4.0%}"
          + (f" | vs standing: p5 {(q(lo,.05)-NATURAL[actor])*100:+.1f} med {(statistics.median(lo)-NATURAL[actor])*100:+.1f}" if actor in NATURAL else ""))
for l in r.get("log", []):
    if "calibrated" in l or "CALIB" in l: print("   ", l[:220])

"""The share of route_sim runs that are good takes, and what the others lacked.
    SP=<tables dir> python stats.py "<route>" [seeds, 300]        (ROUTE_SIM=<binary>, default ./route_sim)
A good take: no fault (rules, distances, spots, held) and the first person at 80% or more of the whole take."""
import subprocess, re, sys, statistics, os
route, seeds = sys.argv[1], sys.argv[2] if len(sys.argv) > 2 else "300"
env = {**os.environ, "ROUTE": route, "SEEDS": seeds, "FP_ALL": "0", "FP_POST": "0", "FPMAP": "1"}
out = subprocess.run([os.environ.get("ROUTE_SIM", "./route_sim")], env=env, capture_output=True, text=True).stdout
rows = []
for l in out.splitlines():
    m = re.match(r"seed (\d+)( BAD)?: rules (\w+) \| spots (\d+)/(\d+) missed (\d+) held (\d+) \| start turn max (\d+) \| behind ([\d.]+)% fp ([\d.]+)% \(post ([\d.]+)%, any ([\d.]+)%, gap max ([\d.]+) s at (\d+)\) stand max ([\d.]+) s at (\d+) \| W (\d+) presses min ([\d.]+)", l)
    if m: rows.append(dict(seed=int(m[1]), bad=bool(m[2]), rules=m[3], spots=int(m[4]), steps=int(m[5]), held=int(m[7]), behind=float(m[9]), fp=float(m[10]), post=float(m[11]), gap=float(m[13]), gap_at=int(m[14]), stand=float(m[15]), stand_at=int(m[16]), presses=int(m[17]), press=float(m[18])))
n = len(rows)
fps = sorted(r["fp"] for r in rows)
good = [r for r in rows if not r["bad"] and r["fp"] >= 80.0]
print(f"runs {n}: good takes {len(good)} ({100*len(good)/n:.1f}%) | other faults (rules/distance/held/spots) {sum(r['bad'] for r in rows)} | first person < 80%: {sum(r['fp'] < 80 for r in rows)}")
print(f"first person whole take: mean {statistics.mean(fps):.1f} min {fps[0]} p5 {fps[int(0.05*n)]} median {fps[n//2]} | after takeover mean {statistics.mean(r['post'] for r in rows):.1f}")
print(f"behind mean {statistics.mean(r['behind'] for r in rows):.1f} min {min(r['behind'] for r in rows)} | longest stand {max(r['stand'] for r in rows)} | shortest press {min(r['press'] for r in rows)} | presses {min(r['presses'] for r in rows)}-{max(r['presses'] for r in rows)} | spots reached {min(r['spots'] for r in rows)}-{max(r['spots'] for r in rows)}/{rows[0]['steps']}")
for l in out.splitlines()[-5:]:   # route_sim's summary: distances, the first-person view's turn (2026-10-13), visibility
    if l.startswith(("visible", "first-person view turn")) or "closest over all runs" in l: print(l)
worst = sorted(rows, key=lambda r: r["fp"])[:3]
print("worst:", [(r["seed"], r["fp"], f"gap {r['gap']} at {r['gap_at']}") for r in worst])

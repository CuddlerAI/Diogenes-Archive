"""The ground's height on a grid round a spot, from the game (controller command ground_scan): for choosing a stage on
flat sand. The player is held at (x, y, z) a few seconds while the map's collision loads, then put back on the ground.
    python tools/ground_scan.py --x -1564.42 --y -1209.72 --z 0.7 [--half 40] [--step 1.0] --out scan.json
Prints the height range; the JSON has x0, y0, step, n and z (n x n, row by row from y0, null where nothing was hit).
What decided the beach demo's stage (2026-10-17): over the cast's area the height change across a 25 cm stance (the
old stage: up to 3 cm, 13 cm bumps; the new: 0.4 cm at most), and no props of the game's own within it (the takes'
source/map-scan-begin.jsonl lists them)."""
import argparse
import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from gta_capture.protocol import JsonLinePipeClient  # noqa: E402


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--x", type=float, required=True)
    ap.add_argument("--y", type=float, required=True)
    ap.add_argument("--z", type=float, required=True, help="a height near the ground there")
    ap.add_argument("--half", type=float, default=40.0, help="metres each way (60 at most)")
    ap.add_argument("--step", type=float, default=1.0, help="metres between points (0.1..5)")
    ap.add_argument("--out", required=True)
    a = ap.parse_args(argv)
    client = JsonLinePipeClient()
    try:
        r = client.request("ground_scan", {"x": a.x, "y": a.y, "z": a.z, "half_m": a.half, "step_m": a.step})
    finally:
        client.close()
    Path(a.out).write_text(json.dumps(r), encoding="utf-8")
    zs = [v for v in r["z"] if v is not None]
    print(f"{r['n']} x {r['n']} points, {r['misses']} missed, ground {min(zs):.3f}..{max(zs):.3f} m -> {a.out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())

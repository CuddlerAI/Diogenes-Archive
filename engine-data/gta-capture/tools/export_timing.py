"""Where an export's time went, lane by lane, from what the export already leaves on disk (read only).

    python tools/export_timing.py <take dir | group dir> [--gpu-log <nvidia-smi csv>] [--json out.json]

The production target (2026-10-13): a capture machine's hour yields 30 minutes of finished video, so a group
(12 lanes x 120 s = 24 min) may take 48 machine minutes, all of it -- the 12 lane exports have about 30 of them,
2.5 ms a frame short of 40 ms. This says what a lane took and why:

  per frame    each frame's PNG is written when the frame is done: the gaps between their modification times
               are the frame times (export fps, stalls -- a stall of seconds is the game or the disk, not the GPU)
  renders      the frame events (evidence/frame-events-laneN.jsonl) carry the Editor's identity heartbeat,
               which counts the game's updates: its step from one frame to the next is the updates (renders)
               the export spent on that frame (patches/replus-v19-sliding.patch holds the clock and waits for
               the camera and the time to settle twice before it asks for the picture: 5.6-6.9 by
               tools/replus_slide_sim.py). frame ms / renders = one render's ms: at 16.7 the export is held to
               a 60 Hz present (vsync or the frame limiter), well over it the GPU is the limit
  gpu          with --gpu-log (scripts/export-gpu-log.ps1 running while the lane exported): the GPU's use,
               the most memory it held (8 GB on the RTX 4060: over ~7.5 the game drops textures and LODs, which
               the per-frame fence cannot see), temperature, clocks and why they were held down

A lane is found by its result, evidence/native-lane-N-result.json (lane.frame_pattern, lane.frame_count). PNGs
deleted after encoding leave the renders but not the frame times."""
from __future__ import annotations

import argparse
import csv
import json
import statistics
import sys
from datetime import datetime
from pathlib import Path

STALL_S = 0.5   # a frame this long is a stall, not rendering


def _pct(values: list[float], q: float) -> float:
    s = sorted(values)
    return s[min(len(s) - 1, max(0, round(q * (len(s) - 1))))]


def frame_times(pattern: str, count: int) -> list[float]:
    """Each frame's PNG modification time (s), in frame order; frames not on disk are left out."""
    times = []
    for i in range(count):
        p = Path(pattern.replace("%06d", f"{i:06d}"))
        if p.is_file():
            times.append(p.stat().st_mtime)
    return times


def heartbeats(events: Path) -> list[int]:
    """The Editor identity heartbeat each frame event saw, in frame order."""
    rows = []
    with events.open(encoding="utf-8") as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            r = json.loads(line)
            hb = (r.get("editor_binding") or {}).get("identity_heartbeat")
            if isinstance(hb, int):
                rows.append((r.get("output_frame", len(rows)), hb))
    return [hb for _, hb in sorted(rows)]


def gpu_rows(path: Path) -> list[dict]:
    """nvidia-smi --format=csv rows: timestamp (s since the epoch, local time) and the numbers it logged."""
    out = []
    with path.open(encoding="utf-8-sig", newline="") as f:
        reader = csv.reader(f)
        header = [h.strip() for h in next(reader)]
        for row in reader:
            if len(row) != len(header):
                continue
            rec = dict(zip(header, (c.strip() for c in row)))
            try:
                ts = datetime.strptime(rec["timestamp"].split(".")[0], "%Y/%m/%d %H:%M:%S").timestamp()
            except (KeyError, ValueError):
                continue
            def num(key):
                for k, v in rec.items():
                    if k.startswith(key):
                        try:
                            return float(v.split()[0])
                        except (ValueError, IndexError):
                            return None
                return None
            reasons = next((v for k, v in rec.items() if k.startswith("clocks_throttle_reasons.active")
                            or k.startswith("clocks_event_reasons.active")), "")
            out.append({"t": ts, "util": num("utilization.gpu"), "mem_mib": num("memory.used"),
                        "mem_total_mib": num("memory.total"), "temp_c": num("temperature.gpu"),
                        "clock_mhz": num("clocks.current.graphics") or num("clocks.gr"), "power_w": num("power.draw"),
                        "throttle": reasons})
    return out


def lane_timing(take_dir: Path, lane: int, gpu: list[dict] | None = None) -> dict:
    result = json.loads((take_dir / "evidence" / f"native-lane-{lane}-result.json").read_text(encoding="utf-8"))
    lr = result.get("lane") or {}
    pattern, count = lr.get("frame_pattern"), lr.get("frame_count")
    out: dict = {"take": take_dir.name, "lane": lane, "frames": count, "ok": result.get("ok")}
    if isinstance(pattern, str) and isinstance(count, int) and count > 1:
        times = frame_times(pattern, count)
        out["frames_on_disk"] = len(times)
        if len(times) > 1:
            gaps = [b - a for a, b in zip(times, times[1:])]
            busy = [g for g in gaps if g < STALL_S]
            out.update({
                "export_s": round(times[-1] - times[0], 1),
                "fps": round((len(times) - 1) / max(times[-1] - times[0], 1e-9), 2),
                "frame_ms_p50": round(1000 * statistics.median(busy), 1) if busy else None,
                "frame_ms_p90": round(1000 * _pct(busy, 0.9), 1) if busy else None,
                "stalls": sum(g >= STALL_S for g in gaps),
                "stall_s": round(sum(g for g in gaps if g >= STALL_S), 1),
                "longest_stall_s": round(max(gaps), 2),
                "started": datetime.fromtimestamp(times[0]).isoformat(timespec="seconds"),
            })
            if gpu:
                window = [g for g in gpu if times[0] <= g["t"] <= times[-1]]
                if window:
                    def stat(key, fn):
                        vals = [g[key] for g in window if g[key] is not None]
                        return round(fn(vals), 1) if vals else None
                    reasons = sorted({g["throttle"] for g in window if g["throttle"] and g["throttle"] not in ("0x0000000000000000", "Not Active")})
                    out["gpu"] = {"samples": len(window), "util_mean": stat("util", statistics.mean),
                                  "mem_max_mib": stat("mem_mib", max), "mem_total_mib": stat("mem_total_mib", max),
                                  "temp_max_c": stat("temp_c", max), "clock_min_mhz": stat("clock_mhz", min),
                                  "power_mean_w": stat("power_w", statistics.mean), "throttle_reasons": reasons}
    events = take_dir / "evidence" / f"frame-events-lane{lane}.jsonl"
    if events.is_file():
        hbs = heartbeats(events)
        steps = [b - a for a, b in zip(hbs, hbs[1:]) if 0 < b - a < 1000]
        if steps:
            out["renders_per_frame_p50"] = statistics.median(steps)
            out["renders_per_frame_mean"] = round(statistics.mean(steps), 2)
            if out.get("frame_ms_p50"):
                out["ms_per_render"] = round(out["frame_ms_p50"] / statistics.median(steps), 1)
    return out


def lanes_of(root: Path) -> list[tuple[Path, int]]:
    takes = [root] if (root / "evidence").is_dir() else sorted(p for p in (root / "takes").glob("*") if (p / "evidence").is_dir())
    found = []
    for take in takes:
        for res in sorted((take / "evidence").glob("native-lane-*-result.json")):
            try:
                found.append((take, int(res.name.split("-")[2])))
            except ValueError:
                continue
    return found


def main(argv: list[str]) -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("root", type=Path, help="a take directory (has evidence/) or a group directory (has takes/)")
    ap.add_argument("--gpu-log", type=Path, help="the CSV scripts/export-gpu-log.ps1 wrote while exporting")
    ap.add_argument("--json", type=Path, help="write the rows here too")
    a = ap.parse_args(argv)
    gpu = gpu_rows(a.gpu_log) if a.gpu_log else None
    rows = [lane_timing(take, lane, gpu) for take, lane in lanes_of(a.root)]
    if not rows:
        print(f"no exported lane under {a.root} (evidence/native-lane-N-result.json)", file=sys.stderr)
        return 1
    cols = ["take", "lane", "frames", "export_s", "fps", "frame_ms_p50", "frame_ms_p90", "stalls", "stall_s",
            "renders_per_frame_p50", "ms_per_render"]
    print(" | ".join(cols + ["gpu"]))
    for r in rows:
        g = r.get("gpu")
        gtxt = (f"util {g['util_mean']}% mem {g['mem_max_mib']}/{g['mem_total_mib']} MiB {g['temp_max_c']} C "
                f"clock>={g['clock_min_mhz']} {','.join(g['throttle_reasons']) or '-'}") if g else "-"
        print(" | ".join(str(r.get(c, "-")) for c in cols) + " | " + gtxt)
    total = sum(r.get("export_s") or 0 for r in rows)
    frames = sum(r.get("frames_on_disk") or 0 for r in rows)
    if total:
        print(f"\n{len(rows)} lanes, {frames} frames in {total / 60:.1f} min of export ({frames / total:.1f} fps); "
              f"the 48-minute group needs about 30 min for 43,200 frames (24 fps)")
    if a.json:
        a.json.write_text(json.dumps(rows, ensure_ascii=False, indent=1), encoding="utf-8")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))

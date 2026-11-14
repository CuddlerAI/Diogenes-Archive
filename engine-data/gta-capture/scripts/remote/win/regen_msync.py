"""regen_msync.py <group dir> <delivery dir> <out dir>: the group's twelve multicam_sync_i.json written again by the
delivery's own code (stage_multicam.take_frames / capture_note / write_lane_files, as stage_delivery.deliver_take
calls them) into <out dir>, without copying a video; then each compared with the delivered package's copy, frame by
frame: what changed besides sample_time, and in the header. Light: reads the takes' telemetry only (2026-10-19: the
sample_time switch to the stage clock, made while GTA was open on the capture PC). Copy the twelve files into the
package yourself once the report shows only the intended change."""
import json, os, sys
from pathlib import Path
sys.path.insert(0, os.path.abspath(os.path.join(os.path.dirname(__file__), "..", "..", "..")))   # gta-capture
from gta_capture.jsonio import load_json
from gta_capture.stage_delivery import TAKES, number, spec_clears
from gta_capture.stage_multicam import SEVENTH_ID, capture_note, sun_lighting, take_frames, tracked_ids, write_lane_files
group, delivery, out = Path(sys.argv[1]), Path(sys.argv[2]), Path(sys.argv[3])
settings = load_json(group / "group.json"); name = settings["delivery_name"]; env = settings["environment"]
pkg = delivery / name
assert pkg.is_dir(), pkg
old = {i: json.loads((pkg / f"multicam_sync_{i}.json").read_text(encoding="utf-8")) for i in range(1, 13)}
takeover = old[5]["takeover"][0]["logical_frame"]; c_actor = old[5]["takeover"][0]["entity_ids"][0]
tracked = tracked_ids([group / "takes" / k for k in TAKES])
assert tracked == old[1]["tracked_entity_ids"], (tracked, old[1]["tracked_entity_ids"])
played = {"AB": None, "C": c_actor, "D": SEVENTH_ID}
lighting = sun_lighting(env)
out.mkdir(parents=True, exist_ok=False)
for kind in TAKES:
    take_dir = group / "takes" / kind
    tk = takeover if kind == "C" else None
    frames, info = take_frames(take_dir, kind=kind, ids=tracked, n=len(old[1]["frames"]), played=played[kind], takeover_tick=tk,
                               lighting=lighting)
    note = capture_note(kind, skipped=info["skipped_ticks"], played=played[kind], takeover_tick=tk,
                        tracked_map=info["tracked_map"], wind_speed=info["wind_speed"],
                        floats="cleared" if spec_clears(take_dir) else "", lighting=lighting, env=env,
                        game_tick_ms=info["game_tick_ms"])
    write_lane_files(out, kind=kind, numbers=[number(kind, lane) for lane in (1, 2, 3, 4)], capture_group_id=name,
                     ids=tracked, frames=frames, note=note, played=played[kind], takeover_tick=tk)
    print(kind, "game tick ms", info["game_tick_ms"], "skipped", info["skipped_ticks"])
report = {}
for i in range(1, 13):
    new = json.loads((out / f"multicam_sync_{i}.json").read_text(encoding="utf-8"))
    head = sorted(k for k in set(new) | set(old[i]) if k != "frames" and new.get(k) != old[i].get(k))
    other = 0
    time = 0
    for fo, fn in zip(old[i]["frames"], new["frames"]):
        if {x: v for x, v in fo.items() if x != "sample_time"} != {x: v for x, v in fn.items() if x != "sample_time"}:
            other += 1
        time += fo["sample_time"] != fn["sample_time"]
    report[i] = {"header_changed": head, "frames_changed_besides_sample_time": other, "sample_time_changed": time,
                 "frames": len(new["frames"])}
print(json.dumps(report, ensure_ascii=False))

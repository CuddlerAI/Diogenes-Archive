"""Read sealed, completed lanes without UI/pipe calls or capture mutations.

Produces a partial diagnostic only. It does not run finalization, modify source
frames, synthesize missing lanes, approve visibility, or grant any formal gate.
"""
import argparse
from dataclasses import replace
import json
import math
from pathlib import Path
import struct
import sys

MODULE = Path(__file__).resolve().parents[2]
sys.path[:0] = [str(MODULE), str(MODULE.parent / "qc")]
from gta_capture.evidence import (_gta_to_delivery_vector, _gta_to_delivery_quaternion,
                                  _round_vec, _quat_error_deg)
from gta_capture.jsonio import load_json, read_jsonl, sha256_file
from gta_capture.scenario import Scenario
from gta_capture.staged import _validate_scenario_tape_route
from engine_qc.config import QCConfig
from engine_qc import checks_motion as cm, checks_json as cj


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--run-dir", type=Path, required=True)
    parser.add_argument("--report", type=Path, required=True)
    args = parser.parse_args()
    root, destination = args.run_dir.resolve(), args.report.resolve()
    audit_root = Path(__file__).resolve().parent
    if (destination.exists() or destination.is_relative_to(root)
            or not destination.is_relative_to(audit_root)):
        parser.error("report must be a new file under this ignored audit directory, outside the sealed run")
    tape = load_json(root / "config/camera-tape.json")
    record = load_json(root / "evidence/native-record-result.json")
    master = record["master"]
    scenario = Scenario.load(root / "config/scenario.json")
    _validate_scenario_tape_route(scenario, tape, expected_replay_sha256=master["replay_sha256"])
    plan_path = Path(tape["source"]["path"]) / "plan.json"
    plan = load_json(plan_path)
    cfg = QCConfig()
    report = {"schema": "dm-gta-readonly-readiness-diagnostic/1", "run_dir": str(root),
        "formal_approval": False, "calibration_approval": False, "human_review_approval": False,
        "scope": "completed native lanes; in-memory delivery-basis poses, existing motion rules; no pixels inferred",
        "limitations": ["PNG headers/sizes and six sampled hashes only; finalizer must verify every PNG hash",
                        "rendered visibility, occlusion, cursor/HUD cleanliness and animation were not inspected",
                        "no final video/audio or full four-lane gate is certified by this report"],
        "master_replay_sha256": master["replay_sha256"], "route_binding_valid": True,
        "plan_sha256": sha256_file(plan_path), "lanes": {}, "pending_lanes": [],
        "coordinate_output_contract": "raw frame events remain GTA Z-up; metadata maps [x,y,z] to [x,z,y] and quaternion to [-x,-z,-y,w], without origin subtraction",
        "planned_windows": {str(c["id"]): [{"class": s.get("cls"), "t0": s.get("t0"), "t1": s.get("t1")}
                                            for s in c.get("segments", [])] for c in plan["cameras"]}}
    loaded, rows_by_lane, errors, results = {}, {}, [], []
    for lane in range(1, 5):
        result_path = root / "evidence" / f"native-lane-{lane}-result.json"
        if not result_path.is_file():
            report["pending_lanes"].append(lane)
            continue
        native_result = load_json(result_path)
        item = native_result["lane"]
        event_path = Path(item["frame_events_path"])
        if sha256_file(event_path) != item["frame_events_sha256"]:
            raise ValueError(f"lane {lane} event seal mismatch")
        rows = read_jsonl(event_path)
        rows_by_lane[lane] = rows
        samples = tape["lanes"][str(lane)]["samples"]
        if len(rows) != len(samples) or item["frame_count"] != len(samples):
            raise ValueError(f"lane {lane} is not a complete sealed window")
        checks = {"frame_envelope": all(r["lane"] == lane and r["output_frame"] == i and r["replay_frame"] == i
                    and r["replay_sha256"] == master["replay_sha256"] and r["render_succeeded"] is True
                    and r["mock"] is False and r["source_t_ns"] == samples[i]["source_t_ns"]
                    for i, r in enumerate(rows)),
            "pixel_generation_match": all(r["pixel_ready_final_camera_generation"] == r["editor_binding"]["final_camera_generation"] for r in rows),
            "generation_consecutive": all(b["editor_binding"]["final_camera_generation"] == a["editor_binding"]["final_camera_generation"] + 1 for a,b in zip(rows,rows[1:])),
            "png_headers_and_sizes": True, "png_pattern_binding": True,
            "sampled_png_hashes": True}
        for index, row in enumerate(rows):
            path = Path(row["output_path"])
            checks["png_pattern_binding"] &= path.resolve() == Path(item["frame_pattern"] % index).resolve()
            if not path.is_file() or path.stat().st_size != row["output_bytes"]:
                checks["png_headers_and_sizes"] = False
                continue
            with path.open("rb") as handle:
                header = handle.read(24)
            checks["png_headers_and_sizes"] &= (header[:8] == b"\x89PNG\r\n\x1a\n"
                                                and struct.unpack(">II", header[16:24]) == (1920,1080))
        for index in sorted({0, min(135,len(rows)-1),len(rows)//4,len(rows)//2,3*len(rows)//4,len(rows)-1}):
            checks["sampled_png_hashes"] &= sha256_file(rows[index]["output_path"]) == rows[index]["output_sha256"]
        marker = Path(rows[0]["output_path"]).parent / "assemble.txt"
        checks["completion_marker"] = marker.is_file() and marker.read_text(encoding="utf-8").splitlines()[0] == f"Rendered {len(rows)} frames at 30 fps (png)."
        position_error = max(math.dist(r["final_camera"]["position"],s["position"]) for r,s in zip(rows,samples))
        rotation_error = max(_quat_error_deg(r["final_camera"]["quaternion"],s["quaternion"]) for r,s in zip(rows,samples))
        clock_error = max(abs(r["replay_time_ms"]-rows[0]["replay_time_ms"]-s["source_t_ns"]/1e6) for r,s in zip(rows,samples))
        checks["pose_tape_agreement"] = position_error <= .001 and rotation_error <= .052
        checks["replay_clock_grid"] = clock_error <= 1.0
        poses = [{"frame": r["output_frame"], "camera_position": _round_vec(_gta_to_delivery_vector(r["final_camera"]["position"])),
                  "camera_rotation_quaternion": _round_vec(_gta_to_delivery_quaternion(r["final_camera"]["quaternion"]))} for r in rows]
        if lane == 1:
            for pose, row in zip(poses, rows):
                pose["mouse_dx"] = row["input"]["mouse_dx"]
                pose["mouse_dy"] = row["input"]["mouse_dy"]
            results.append(cj.check_input_stream("lane1 actual native input", poses, cfg, "mc"))
        loaded[str(lane)] = poses
        results.extend(cm.check_physics(f"lane{lane} actual poses", poses, 1/30, cfg, is_cam4=lane==4))
        if not (lane==4 and cm.cam4_declared_mode(plan)=="topdown"):
            results.extend(cm.check_roll(f"lane{lane} actual poses",poses,1/30,cfg))
        binding = rows[0]["editor_binding"]
        report["lanes"][str(lane)] = {"frames":len(rows),"checks":checks,"event_sha256":item["frame_events_sha256"],
            "project_names": sorted({r["editor_binding"]["project_name"] for r in rows}),
            "clip_identities":sorted({r["editor_binding"]["clip_identity"] for r in rows}),
            "session_ids":sorted({r["editor_binding"]["session_id"] for r in rows}),
            "first_generation":binding["final_camera_generation"],"last_generation":rows[-1]["editor_binding"]["final_camera_generation"],
            "max_position_error_m":position_error,"max_rotation_error_deg":rotation_error,"max_replay_clock_grid_error_ms":clock_error}
        errors.extend(f"lane{lane}: {name}" for name,ok in checks.items() if not ok)
    results.extend(cj.check_pink_coverage(plan_path,plan,None))
    results.extend(cj.check_pink_honesty(plan_path,plan,None,loaded,cfg))
    if len(loaded)>1:
        results.extend(cm.check_decouple(loaded,1/30,cfg))
        results.extend(cm.check_initial_frame(loaded,replace(cfg,init_exempt_lanes=(4,)) if cm.cam4_declared_mode(plan)=="topdown" else cfg))
        ordered=sorted(rows_by_lane)
        for a,b in zip(ordered,ordered[1:]):
            if b==a+1 and rows_by_lane[b][0]["editor_binding"]["final_camera_generation"] != rows_by_lane[a][-1]["editor_binding"]["final_camera_generation"]+1:
                errors.append(f"lane {a}->{b}: nonconsecutive global generation")
        if len({r[0]["editor_binding"]["project_name"] for r in rows_by_lane.values()}) != 1:
            errors.append("completed lanes differ in project")
        if len({r[0]["editor_binding"]["clip_identity"] for r in rows_by_lane.values()}) != 1:
            errors.append("completed lanes differ in source clip")
    if len(loaded)==4:
        results.extend(cm.check_mc_sync(loaded))
        results.extend(cm.check_cam4(loaded,plan,None,1/30,cfg))
    report["motion_checks"] = [r.to_dict() for r in results]
    report["machine_problems"] = errors
    report["status"] = "diagnostic_fail" if errors or any(r.status=="FAIL" for r in results) else "partial_no_failure_observed" if report["pending_lanes"] else "completed_lanes_no_failure_observed"
    destination.parent.mkdir(parents=True,exist_ok=True)
    destination.write_text(json.dumps(report,ensure_ascii=False,indent=2)+"\n",encoding="utf-8")
    print(json.dumps({"report":str(destination),"status":report["status"],"completed_lanes":sorted(rows_by_lane),"pending_lanes":report["pending_lanes"],"problems":errors,"motion_failures":[r.to_dict() for r in results if r.status=="FAIL"]},ensure_ascii=False))


if __name__=="__main__":
    main()

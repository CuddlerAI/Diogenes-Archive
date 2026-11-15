"""Read-only, independently calculated native-to-delivery metadata audit.

This script does not invoke the materializer, QC, game, or approval mechanisms.
It writes one new diagnostic outside the sealed run, and binds every input hash.
"""
import argparse
import hashlib
import json
import math
from pathlib import Path
import struct


def sha(data):
    return hashlib.sha256(data).hexdigest()


def normal(q):
    if len(q) != 4 or not all(math.isfinite(x) for x in q):
        raise ValueError("invalid quaternion")
    n = math.sqrt(sum(x*x for x in q))
    if n == 0:
        raise ValueError("zero quaternion")
    return [x/n for x in q], n


def angle_deg(a, b):
    a, _ = normal(a)
    b, _ = normal(b)
    # atan2 of component distances is stable for these very small errors.
    d = min(math.dist(a,b), math.dist(a,[-x for x in b]))
    return math.degrees(4*math.asin(min(1.0,d/2)))


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--run-dir", type=Path, required=True)
    p.add_argument("--report", type=Path, required=True)
    args = p.parse_args()
    root, out = args.run_dir.resolve(), args.report.resolve()
    if out.exists() or out.is_relative_to(root) or not out.is_relative_to(Path(__file__).resolve().parent):
        p.error("report must be new and under this ignored audit directory, outside the run")
    inputs, problems, lanes = {}, [], {}

    def read(path, kind="json"):
        path = path.resolve()
        data = path.read_bytes()
        inputs[str(path)] = {"sha256": sha(data), "bytes": len(data)}
        if kind == "jsonl":
            return [json.loads(x) for x in data.decode("utf-8-sig").splitlines() if x.strip()]
        if kind == "bytes":
            return data
        return json.loads(data)

    tape = read(root/"config/camera-tape.json")
    provenance = read(root/"evidence/metadata-report.json")
    if tape.get("coordinate_convention") != "gta-native-z-up-right-handed-order2":
        problems.append("camera tape has unexpected native coordinate convention")
    if provenance.get("coordinate_transform") != "position=[x,z,y]; quaternion=[-x,-z,-y,w]":
        problems.append("metadata report has unexpected transform")
    expected_metadata = {str((root/"capture"/f"{prefix}_{lane}.json").resolve())
                         for lane in range(1,5) for prefix in ("action_camera","systeminfo")}
    if {str(Path(x).resolve()) for x in provenance["files"]} != expected_metadata:
        problems.append("metadata report does not enumerate exactly the eight audited metadata files")
    for lane in range(1,5):
        events_path = root/"evidence"/f"frame-events-lane{lane}.jsonl"
        events = read(events_path,"jsonl")
        result = read(root/"evidence"/f"native-lane-{lane}-result.json")["lane"]
        metadata = read(root/"capture"/f"action_camera_{lane}.json")
        system = read(root/"capture"/f"systeminfo_{lane}.json")
        checks = {
            "native_event_seal_matches": inputs[str(events_path.resolve())]["sha256"] == result["frame_events_sha256"],
            "native_event_path_matches": Path(result["frame_events_path"]).resolve() == events_path.resolve(),
            "full_1800_frames": len(metadata) == len(events) == result["frame_count"] == 1800,
            "frame_identity": True,
            "camera_position_rounding_and_inverse": True,
            "camera_quaternion_normalization_rounding_and_inverse": True,
            "native_K_rounding": True,
            "follow_offset_rounding": True,
            "no_origin_subtraction": True,
        }
        maxima = {"camera_position_component_m": 0.0,"camera_position_euclidean_m":0.0,
                  "camera_quaternion_component":0.0,"camera_quaternion_angle_deg":0.0,
                  "camera_native_quaternion_norm_deviation":0.0,"intrinsics_component_px":0.0,
                  "follow_offset_component_m":0.0}
        if lane == 1:
            checks.update(player_position_rounding_and_inverse=True,
                          player_quaternion_normalization_rounding_and_inverse=True)
            maxima.update(player_position_component_m=0.0, player_quaternion_component=0.0,
                          player_quaternion_angle_deg=0.0, player_native_quaternion_norm_deviation=0.0)
        for i, (actual, event) in enumerate(zip(metadata,events)):
            checks["frame_identity"] &= (actual["frame"] == event["output_frame"] == event["replay_frame"] == i
                                         and event["lane"] == lane and actual["fps"] == 30.0)
            for label, field in (("camera","final_camera"),("player","player_state")):
                if label == "player" and lane != 1:
                    continue
                native = event[field]
                # Invert delivery [x,z,y], with no translation or origin adjustment.
                delivered = actual[f"{label}_position"]
                inverse = [delivered[0],delivered[2],delivered[1]]
                errors = [abs(a-b) for a,b in zip(inverse,native["position"])]
                maxima[f"{label}_position_component_m"] = max(maxima[f"{label}_position_component_m"],*errors)
                if label == "camera":
                    maxima["camera_position_euclidean_m"] = max(maxima["camera_position_euclidean_m"],math.dist(inverse,native["position"]))
                position_ok = (all(math.isfinite(x) for x in delivered)
                               and inverse == [round(x,6) for x in native["position"]]
                               and max(errors) <= 0.000000500001)
                checks[f"{label}_position_rounding_and_inverse"] &= position_ok
                checks["no_origin_subtraction"] &= position_ok
                native_q, native_norm = normal(native["quaternion"])
                q = actual[f"{label}_rotation_quaternion"]
                inverse_q = [-q[0],-q[2],-q[1],q[3]]
                qe = max(abs(a-b) for a,b in zip(inverse_q,native_q))
                maxima[f"{label}_quaternion_component"] = max(maxima[f"{label}_quaternion_component"],qe)
                maxima[f"{label}_quaternion_angle_deg"] = max(maxima[f"{label}_quaternion_angle_deg"],angle_deg(inverse_q,native_q))
                maxima[f"{label}_native_quaternion_norm_deviation"] = max(maxima[f"{label}_native_quaternion_norm_deviation"],abs(native_norm-1))
                checks[f"{label}_quaternion_normalization_rounding_and_inverse"] &= (
                    abs(native_norm-1) <= .001 and inverse_q == [round(x,6) for x in native_q]
                    and qe <= 0.000000500001)
            for k in ("fx","fy","cx","cy"):
                av, nv = actual["camera_intrinsics"][k], event["final_camera"]["intrinsics"][k]
                maxima["intrinsics_component_px"] = max(maxima["intrinsics_component_px"],abs(av-nv))
                checks["native_K_rounding"] &= math.isfinite(av) and av == round(nv,6) and abs(av-nv) <= 0.000000500001
            offset = [event["final_camera"]["position"][axis]-event["player_state"]["position"][axis] for axis in (0,2,1)]
            maxima["follow_offset_component_m"] = max(maxima["follow_offset_component_m"],*(abs(a-b) for a,b in zip(actual["camera_Follow Offset"],offset)))
            checks["follow_offset_rounding"] &= actual["camera_Follow Offset"] == [round(x,6) for x in offset]
        # A full hash of the first sealed PNG independently binds system resolution.
        png = read(Path(events[0]["output_path"]),"bytes")
        actual_resolution = struct.unpack(">II",png[16:24])
        checks["first_png_seal_matches"] = sha(png) == events[0]["output_sha256"] and len(png) == events[0]["output_bytes"]
        resolution = tape["lanes"][str(lane)]["resolution"]
        checks["systeminfo_resolution"] = (png[:8] == b"\x89PNG\r\n\x1a\n" and
            actual_resolution == (system["width"],system["height"]) == (resolution["width"],resolution["height"]) == (1920,1080))
        checks["systeminfo_capture_layout"] = system == {"gameProcessName":"GTA5.exe","x":0,"y":0,"width":1920,"height":1080,"recordDpi":1.0}
        checks["first_frame_world_position_preserved"] = math.dist(metadata[0]["camera_position"],[0,0,0]) > 1 and checks["no_origin_subtraction"]
        lanes[str(lane)] = {"frames":len(metadata),"checks":checks,"max_roundtrip_errors":maxima,
            "first_native_camera_position":events[0]["final_camera"]["position"],
            "first_delivery_camera_position":metadata[0]["camera_position"],
            "player_pose_scope":"lane1 only, sourced from recording-side estimate; not a verified replay-origin observation" if lane == 1 else "player pose not part of this lane's action metadata"}
        problems.extend(f"lane{lane}: {key}" for key,value in checks.items() if not value)
    for path, record in inputs.items():
        record["unchanged_after_audit"] = sha(Path(path).read_bytes()) == record["sha256"]
        if not record["unchanged_after_audit"]:
            problems.append(f"input changed while auditing: {path}")
    report = {"schema":"dm-gta-readonly-metadata-roundtrip/1","run_dir":str(root),
        "status":"diagnostic_pass" if not problems else "diagnostic_fail",
        "formal_approval":False,"calibration_approval":False,"metric_scale_approval":False,
        "player_replay_origin_approval":False,"granted_rules":[],
        "scope":"All 7200 materialized camera poses/K and lane1's 1800 player poses versus sealed native events; four systeminfo files versus tape and first sealed PNG per lane",
        "method":"Independent inverse permutation position=[x,z,y], quaternion=[-x,-z,-y,w]; compare to native normalized quaternion and exact six-decimal rounding. No origin subtraction.",
        "tolerance":{"component_absolute":0.000000500001,"decimal_places":6,"native_quaternion_norm_deviation":.001},
        "limitations":["K is checked only for numeric preservation from native final_camera, not against independently observed world-to-pixel correspondences.",
                       "No metric calibration, visibility, audio, final video or policy approval is implied.",
                       "Native player_state is the recording-side estimate already declared in events, not an independently verified in-band replay pose."],
        "metadata_file_count":len(expected_metadata),"inputs":inputs,"lanes":lanes,"problems":problems,
        "auditor":{"path":str(Path(__file__).resolve()),"sha256":sha(Path(__file__).read_bytes())}}
    out.parent.mkdir(parents=True,exist_ok=True)
    with out.open("x",encoding="utf-8") as f:
        json.dump(report,f,ensure_ascii=False,indent=2)
        f.write("\n")
    print(json.dumps({"report":str(out),"status":report["status"],"input_files":len(inputs),"problems":problems,
                      "lanes":{lane:data["max_roundtrip_errors"] for lane,data in lanes.items()}},ensure_ascii=False))
    return 0 if not problems else 1


if __name__ == "__main__":
    raise SystemExit(main())

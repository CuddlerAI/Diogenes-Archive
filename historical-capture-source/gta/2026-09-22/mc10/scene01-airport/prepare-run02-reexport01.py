"""Prepare a same-master retry only; no pipe/UI calls or new recording.

Small immutable inputs are byte-copied. The 69 MB master is hard-linked on the
same volume to avoid another payload allocation; it is not an independent backup.
Never run native-record-smoke against this retry. Old absolute JSON references
remain untouched. Old lane results/frames/audio are not imported.
"""
from pathlib import Path
import json
import os
import shutil
import sys

HERE = Path(__file__).resolve().parent
PACKAGE = HERE.parents[2]
sys.path.insert(0, str(PACKAGE))
from gta_capture.jsonio import file_record, load_json, sha256_file, utc_now, write_json_atomic
from gta_capture.scenario import Scenario
from gta_capture.staged import _validate_scenario_tape_route

SOURCE = HERE / "run-02"
TARGET = HERE / "run-02-reexport-01"
MASTER_SHA = "59d76c5a36c92f308c212e79b9b177283588be0e36aa9b73524fd8055a024809"
TELEMETRY_SHA = "c4985b29205472ea6880f954037e7eb29e9ed7dcee2146b7df31af5775ae4361"
INPUTS = (
    "config/dependency-manifest.json", "config/scenario.json",
    "config/camera-tape.json", "config/subject-route.json",
    "evidence/native-record-result.json", "source/master-replay.clip",
    "source/master-player-telemetry.jsonl",
)


def events_summary(lane):
    path = SOURCE / "evidence" / f"frame-events-lane{lane}.jsonl"
    rows = [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines() if line]
    if len(rows) != 1800:
        raise RuntimeError(f"Original lane {lane} evidence is not a complete 1800-frame file")
    for frame, row in enumerate(rows):
        if (row.get("lane") != lane or row.get("output_frame") != frame
                or row.get("replay_frame") != frame or row.get("mock") is not False
                or row.get("replay_sha256") != MASTER_SHA
                or row.get("render_succeeded") is not True):
            raise RuntimeError(f"Original lane {lane} event identity mismatch at {frame}")
    generations = [row["editor_binding"]["final_camera_generation"] for row in rows]
    if generations != list(range(generations[0], generations[0] + 1800)):
        raise RuntimeError("Original lane generation sequence is not internally continuous")
    return {"events": file_record(path), "n_frames": len(rows),
            "first_generation": generations[0], "last_generation": generations[-1],
            "first_rgb_path": rows[0]["output_path"], "last_rgb_path": rows[-1]["output_path"],
            "raw_rgb_files_copied": False, "raw_rgb_hashes_rechecked_by_helper": False}


def main():
    if TARGET.exists():
        raise RuntimeError("Retry path already exists; no overwrite is permitted")
    record = load_json(SOURCE / "evidence/native-record-result.json")
    if record.get("ok") is not True or record["master"].get("replay_sha256") != MASTER_SHA:
        raise RuntimeError("Original master result is not the expected successful recording")
    if (sha256_file(SOURCE / "source/master-replay.clip") != MASTER_SHA
            or sha256_file(SOURCE / "source/master-player-telemetry.jsonl") != TELEMETRY_SHA
            or record["master"].get("player_telemetry_sha256") != TELEMETRY_SHA):
        raise RuntimeError("Original master/telemetry bytes do not match the frozen recording")
    lane1 = load_json(SOURCE / "evidence/native-lane-1-result.json")
    old = {str(lane): events_summary(lane) for lane in (1, 2)}
    if (lane1.get("ok") is not True or lane1["lane"].get("frame_events_sha256") != old["1"]["events"]["sha256"]
            or Path(lane1["lane"]["frame_events_path"]).resolve() != Path(old["1"]["events"]["path"]).resolve()):
        raise RuntimeError("Original lane1 collect is not bound to its exact events")
    if old["2"]["first_generation"] != old["1"]["last_generation"] + 1:
        raise RuntimeError("Original lane boundaries differ from this retry's audited history")
    tape = load_json(SOURCE / "config/camera-tape.json")
    _validate_scenario_tape_route(Scenario.load(SOURCE / "config/scenario.json"), tape,
                                 expected_replay_sha256=MASTER_SHA)
    before = {name: file_record(SOURCE / name) for name in INPUTS}
    source_status = {"lane1_collect": file_record(SOURCE / "evidence/native-lane-1-result.json"),
                     "lane2_arm": file_record(SOURCE / "evidence/native-lane-2-arm.json")}
    lane2_result = SOURCE / "evidence/native-lane-2-result.json"
    if lane2_result.is_file():
        source_status["lane2_collect"] = file_record(lane2_result)
    TARGET.mkdir()
    copied = []
    for name in INPUTS:
        destination = TARGET / name
        destination.parent.mkdir(parents=True, exist_ok=True)
        if name == "source/master-replay.clip":
            os.link(SOURCE / name, destination)
            method = "ntfs_same_volume_hard_link_no_payload_duplication"
            if not os.path.samefile(SOURCE / name, destination):
                raise RuntimeError("Master hard link did not preserve file identity")
        else:
            shutil.copyfile(SOURCE / name, destination)
            method = "byte_copy"
        actual = file_record(destination)
        if any(actual[key] != before[name][key] for key in ("bytes", "sha256")):
            raise RuntimeError("Retry input differs from the original: " + name)
        copied.append({"relative_path": name, "method": method,
                       "source": before[name], "retry": actual})
    for name, expected in before.items():
        if file_record(SOURCE / name) != expected:
            raise RuntimeError("Original immutable source changed during preparation")
    for item in list(source_status.values()) + [old["1"]["events"], old["2"]["events"]]:
        if file_record(item["path"]) != item:
            raise RuntimeError("Original export evidence changed during preparation")
    lineage = {
        "schema": "dm-gta-same-master-export-retry/1", "prepared_at": utc_now(),
        "source_run": str(SOURCE), "retry_run": str(TARGET),
        "new_recording_performed": False, "game_calls_performed": False,
        "replay_sha256": MASTER_SHA, "telemetry_sha256": TELEMETRY_SHA,
        "reason": "Operator observed cursor contamination in render_0053 lane2; original lane1 remains preserved but cannot join a new lane2 under the existing contiguous-global-generation check",
        "reason_measurement": "operator observation; helper does not inspect pixels",
        "copied_immutable_files": copied,
        "master_link_semantics": "Same file bytes/storage as original master; neither link may be edited. This is not an independent backup.",
        "source_status_files": source_status, "preserved_original_lanes": old,
        "reused_successful_lane_results": [], "rgb_files_copied": 0,
        "absolute_source_references_rewritten": False,
        "live_session_release_verified_by_helper": False,
        "required_before_arm": [
            "Return to Story and collect original run-02 lane2 with the same session first; confirm release",
            "Keep the current recording process and master; do not restart or re-record",
            "Keep the same one-master Editor project and sealed render profile/output root",
            "Export fresh lane1, lane2, lane3, lane4 consecutively; do not import old lane1 events",
            "Check original PNGs for cursor contamination; no cursor removal or crop",
            "Collect fresh lane1 audio, then seal audio only after all four replacement lanes complete",
        ],
        "existing_gate_reason": {"old_lane1_generations": [old["1"]["first_generation"], old["1"]["last_generation"]],
                                 "rejected_lane2_generations": [old["2"]["first_generation"], old["2"]["last_generation"]],
                                 "global_generation_must_advance_exactly_one": True,
                                 "initial_generation_need_not_be_one": True},
        "delivery_eligible": False,
    }
    path = write_json_atomic(TARGET / "evidence/export-retry-lineage.json", lineage)
    print(json.dumps({"prepared": True, "retry_run": str(TARGET), "lineage": str(path),
                      "copied_payload_bytes": sum(row["retry"]["bytes"] for row in copied if row["method"] == "byte_copy"),
                      "hardlinked_master_bytes": before["source/master-replay.clip"]["bytes"],
                      "raw_frames_copied": 0, "game_calls_performed": False}, indent=2))


if __name__ == "__main__":
    main()

"""Prepare a same-master export retry; no GTA calls and no new recording claim.

Run only after retry01 has been cancelled and collected in Story Mode to release
its session. That live game precondition is the operator's responsibility; this
offline helper does not claim to verify it. Original 002 must remain collected.
This preserves every original file and copies selected immutable inputs verbatim.
"""

from pathlib import Path
import json
import shutil
import sys

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parents[1]))

from gta_capture.jsonio import file_record, load_json, sha256_file, utc_now, write_json_atomic
from gta_capture.scenario import Scenario
from gta_capture.staged import _validate_scenario_tape_route

SOURCE = HERE / "run-mc2-beach-002"
PREVIOUS_RETRY = HERE / "run-mc2-beach-002-reexport-01"
RETRY = HERE / "run-mc2-beach-002-reexport-02"
EXPECTED_REPLAY = "0d341ac8d21bf83a287789d633db53c743f519cba2c8f071f1b218a88671de48"
COPIES = (
    "config/dependency-manifest.json", "config/scenario.json",
    "config/camera-tape.json", "config/subject-route.json",
    "evidence/native-record-result.json",
    "source/master-replay.clip", "source/master-player-telemetry.jsonl",
)


def main():
    if RETRY.exists():
        raise RuntimeError("Retry directory already exists; no files were overwritten")
    collected = SOURCE / "evidence/native-lane-1-result.json"
    if not collected.is_file():
        raise RuntimeError("Collect the existing 002 lane-1 session first; do not arm a new run yet")
    old_lane = load_json(collected)
    events = SOURCE / "evidence/frame-events-lane1.jsonl"
    if (old_lane.get("ok") is not True or old_lane.get("lane", {}).get("lane") != 1
            or old_lane["lane"].get("frame_count") != 1800
            or old_lane["lane"].get("frame_events_sha256") != sha256_file(events)
            or Path(old_lane["lane"].get("frame_events_path", "")).resolve() != events.resolve()):
        raise RuntimeError("Original lane collection is absent, partial, or not bound to its sealed events")
    recorded = load_json(SOURCE / "evidence/native-record-result.json")
    if (recorded["master"]["replay_sha256"] != EXPECTED_REPLAY
            or sha256_file(SOURCE / "source/master-replay.clip") != EXPECTED_REPLAY
            or sha256_file(SOURCE / "source/master-player-telemetry.jsonl")
                != recorded["master"]["player_telemetry_sha256"]):
        raise RuntimeError("Original master or telemetry changed")
    tape = load_json(SOURCE / "config/camera-tape.json")
    _validate_scenario_tape_route(Scenario.load(SOURCE / "config/scenario.json"), tape,
                                 expected_replay_sha256=EXPECTED_REPLAY)
    before = {name: file_record(SOURCE / name) for name in COPIES}
    rejected = {"native_result": file_record(collected), "frame_events": file_record(events)}
    previous_evidence = {
        "lineage": file_record(PREVIOUS_RETRY / "evidence/export-retry-lineage.json"),
        "arm": file_record(PREVIOUS_RETRY / "evidence/native-lane-1-arm.json"),
    }
    previous_lineage = load_json(previous_evidence["lineage"]["path"])
    if (previous_lineage.get("replay_sha256") != EXPECTED_REPLAY
            or previous_lineage.get("new_recording_performed") is not False
            or Path(previous_lineage.get("source_run", "")).resolve() != SOURCE.resolve()):
        raise RuntimeError("Retry01 lineage is not bound to the original 002 master")
    for name in ("native-lane-1-result.json", "frame-events-lane1.jsonl",
                 "frame-events-lane1.jsonl.partial"):
        candidate = PREVIOUS_RETRY / "evidence" / name
        if candidate.is_file():
            previous_evidence[name] = file_record(candidate)
    diagnostic = PREVIOUS_RETRY / "DIAGNOSTIC-NOT-DELIVERY.md"
    if diagnostic.is_file():
        previous_evidence["diagnostic"] = file_record(diagnostic)
    RETRY.mkdir()
    copied = []
    for name in COPIES:
        destination = RETRY / name
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(SOURCE / name, destination)
        actual = file_record(destination)
        if any(actual[key] != before[name][key] for key in ("bytes", "sha256")):
            raise RuntimeError("Retry copy differs from the immutable source: " + name)
        if file_record(SOURCE / name) != before[name]:
            raise RuntimeError("Original file changed while preparing retry: " + name)
        copied.append({"relative_path": name, "source": before[name], "copy": actual})
    for record in rejected.values():
        if file_record(record["path"]) != record:
            raise RuntimeError("Original rejected export changed while preparing retry")
    for record in previous_evidence.values():
        if file_record(record["path"]) != record:
            raise RuntimeError("Retry01 evidence changed while preparing retry02")
    lineage = {
        "schema": "dm-gta-same-master-export-retry/1", "prepared_at": utc_now(),
        "source_run": str(SOURCE), "retry_run": str(RETRY),
        "new_recording_performed": False, "replay_sha256": EXPECTED_REPLAY,
        "reason": "Original 002 lane 1 has cursor contamination confirmed at frame 520; retry01 also has early cursor contamination and was cancelled with Escape",
        "reason_measurement": "operator observation; this helper performs no image analysis",
        "rejected_export_evidence": rejected, "copied_immutable_files": copied,
        "previous_retry_diagnostic": {
            "run": str(PREVIOUS_RETRY), "evidence": previous_evidence,
            "observation_source": "operator report; no image analysis by this helper",
            "observations": [
                "Retry01 early frames retain a cursor; the export was normally cancelled with Escape",
                "After normal Alt+Tab away and activating GTA, raw frames 375 and 526 were observed without the cursor",
                "Those two observations do not establish a cause or prove the problem resolved for a full export",
            ],
            "live_session_release_verified_by_helper": False,
        },
        "source_bindings": "All copied JSON bytes and original absolute source references are unchanged",
        "requirements": [
            "Collect the cancelled retry01 export in Story Mode and confirm its session released before running this helper or arming retry02",
            "Keep the same GTA process and recorded master; do not call native-record-smoke again",
            "Use the same one-master Editor project required by the retained completed session",
            "Before retry02 audio-pass first frame, perform the planned normal focus cycle, then inspect actual raw frames; this is not a verified cursor fix",
            "Export all four replacement lanes into fresh RE+ output subdirectories",
            "Keep the frozen Render.ini output root/profile and dependency manifest unchanged",
            "Do not copy old lane results, PNGs, or audio into the replacement export evidence",
        ],
        "delivery_eligible": False,
    }
    path = write_json_atomic(RETRY / "evidence/export-retry-lineage.json", lineage)
    print(json.dumps({"prepared": True, "retry_run": str(RETRY), "lineage": str(path),
                      "new_recording_performed": False, "game_calls_performed": False}, indent=2))


if __name__ == "__main__":
    main()

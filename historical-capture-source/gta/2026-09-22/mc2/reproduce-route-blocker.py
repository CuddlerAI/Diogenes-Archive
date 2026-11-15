"""Offline diagnostic only: historical anchor, pending filmback, no game actions.

This deliberately feeds the untouched place_trajgen_clip result into the actual
staged validator. The historical POC061 anchor has NOT been re-surveyed. These
files are local diagnostic artifacts, not a capture-ready configuration.
"""

from __future__ import annotations

import copy
import json
from pathlib import Path
import sys


ROOT = Path(__file__).resolve().parent
sys.path.insert(0, str(ROOT.parents[1]))

from gta_capture.errors import ContractError
from gta_capture.jsonio import load_json, sha256_file, utc_now, write_json_atomic
from gta_capture.preflight import DependencyManifest, run_preflight
from gta_capture.scenario import Scenario, validate_scenario
from gta_capture.staged import _validate_real_inputs
from gta_capture.tape import write_tape
from gta_capture.traj_adapter import Placement, place_trajgen_clip


def main() -> int:
    scenario_path = Path("C:/capture/gta/scenario-poc061.json")
    manifest_path = Path("C:/capture/gta/dependency-manifest-poc061.json")
    clip_path = ROOT / "plan-clip"
    scenario_hash_before = sha256_file(scenario_path)
    manifest_hash_before = sha256_file(manifest_path)
    source_scenario = Scenario.load(scenario_path)
    scenario_data = copy.deepcopy(source_scenario.raw)
    scenario_data["replay"]["duration_s"] = 60.0
    validate_scenario(scenario_data)
    scenario = Scenario(path=scenario_path.resolve(), raw=scenario_data)
    manifest = DependencyManifest.load(manifest_path)
    plan = load_json(clip_path / "plan.json")

    tape, route = place_trajgen_clip(
        clip_path,
        placement=Placement(
            anchor=tuple(source_scenario.raw["location"]),
            heading_deg=source_scenario.raw["player"]["heading_deg"],
            ground_y=0.0,
        ),
        filmback_id="pending-offline-diagnostic-not-calibrated",
        clip_id="offline-mc2-seed7-route-blocker",
    )
    # Do not add route source fields or otherwise alter the placed tape.
    tape_path = ROOT / "offline-diagnostic-camera-tape.json"
    route_path = ROOT / "offline-diagnostic-subject-route.json"
    write_tape(tape_path, tape)
    write_json_atomic(route_path, route)

    preflight_calls = 0

    def observe_calls(frame, event, arg):
        nonlocal preflight_calls
        if event == "call" and frame.f_code is run_preflight.__code__:
            preflight_calls += 1

    error = None
    previous_profile = sys.getprofile()
    try:
        sys.setprofile(observe_calls)
        _validate_real_inputs(
            manifest,
            scenario,
            tape,
            ROOT,
            allow_internal_calibration_gaps=False,
        )
    except ContractError as exc:
        error = {"type": type(exc).__name__, "message": str(exc)}
    finally:
        sys.setprofile(previous_profile)

    required_route_keys = (
        "route_distance_m", "route_speed_mps", "route_heading_deg"
    )
    report = {
        "schema": "dm-gta-offline-route-blocker-diagnostic/1",
        "created_at": utc_now(),
        "scope": "offline diagnostic only; not a game capture",
        "historical_anchor_used": True,
        "anchor_re_surveyed": False,
        "filmback_status": "pending; no calibration evidence granted",
        "input_plan_seed": plan.get("seed"),
        "scenario_duration_s_in_memory": scenario.duration_s,
        "source_scenario_unchanged": sha256_file(scenario_path) == scenario_hash_before,
        "dependency_manifest_unchanged": sha256_file(manifest_path) == manifest_hash_before,
        "placed_tape_untouched": True,
        "n_frames_per_lane": tape["n_frames"],
        "duration_s": tape["duration_s"],
        "rigidity_residual_m": tape["source"]["rigidity_residual_m"],
        "coordinate_convention": tape["coordinate_convention"],
        "cam4_mode": tape["source"]["cam4_mode"],
        "subject_route_waypoint_count": len(route["waypoints"]),
        "absent_source_route_keys": [
            key for key in required_route_keys if key not in tape["source"]
        ],
        "validator": "gta_capture.staged._validate_real_inputs",
        "preflight_call_count": preflight_calls,
        "error": error,
        "expected_blocker_reproduced": (
            error is not None
            and error["message"] == "scenario player route differs from the camera tape route"
            and preflight_calls == 0
        ),
        "delivery_eligible": False,
        "upload_enabled": False,
    }
    write_json_atomic(ROOT / "route-blocker-diagnostic.json", report)
    print(json.dumps(report, ensure_ascii=False, indent=2))
    return 0 if report["expected_blocker_reproduced"] else 1


if __name__ == "__main__":
    raise SystemExit(main())

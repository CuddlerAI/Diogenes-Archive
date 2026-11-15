"""Offline only: build this new recording's plan, body-framing proposal and placed tape."""
from pathlib import Path
import json
import shutil
import subprocess
import sys

RUN = Path(__file__).resolve().parent
CAPTURE = RUN.parents[3]
sys.path.insert(0, str(CAPTURE))

from gta_capture.jsonio import sha256_file, write_json_atomic
from gta_capture.moving_plan import make_recorded_trajgen_clip
from gta_capture.native_tape import write_native_tape
from gta_capture.scenario import Scenario
from gta_capture.staged import _validate_scenario_tape_route
from gta_capture.tape import write_tape
from gta_capture.traj_adapter import Placement, place_trajgen_clip


def load(path):
    return json.loads(Path(path).read_text(encoding="utf8"))


def main():
    scenario_path = RUN / "config/scenario.json"
    manifest_path = RUN / "config/dependency-manifest.json"
    result_path = RUN / "evidence/native-record-result.json"
    source = CAPTURE / "out/mc2-20260908/plan-clip"
    base_plan = RUN / "config/recorded-plan-base-01"
    proposal = RUN / "review/body-framing-proposal-01"
    final_plan = RUN / "config/recorded-plan"
    tape_path = RUN / "config/camera-tape.json"
    route_path = RUN / "config/subject-route.json"
    native_path = RUN / "config/camera-tape.bin"
    receipt_path = RUN / "review/fresh-plan-preparation.json"
    assert sha256_file(scenario_path) == "278c6095986c6bc2f4618b0000c784411e11a52c2c1d3af011d6e02406b4abe2"
    assert sha256_file(manifest_path) == "5e04cb5850dd40aeb098792367eacbc9b603e596aec5f4a85f313649b5c64955"
    assert sha256_file(source / "plan.json") == "7eee899cd431c128dd7eaa4440fa10ee05a91bef1c6d80b80c782d22418b09c9"
    assert all(not p.exists() for p in (base_plan, proposal, final_plan, tape_path, route_path, native_path, receipt_path)), "Use a fresh output; partial attempts are preserved."
    record = load(result_path)
    assert record.get("ok") is True, "Requires a successful new recording."
    # Windows can reuse a PID after restart; bind the new run's actual files instead.
    master = record["master"]
    assert master["replay_sha256"] != "720573e9561a7ce292d7c736fcfb2d0b211352aed5150dba5382a09e6f8d1a52", "The failed run-01 master cannot be reused."
    for field, filename, hash_field in (
            ("replay_path", "master-replay.clip", "replay_sha256"),
            ("player_telemetry_path", "master-player-telemetry.jsonl", "player_telemetry_sha256")):
        expected = (RUN / "source" / filename).resolve()
        assert Path(master[field]).resolve() == expected, "Recording source must belong to this fresh run."
        assert sha256_file(expected) == master[hash_field], "Recording source differs from its native result."
    assert (RUN / "source/master-replay.clip").is_file()
    assert (RUN / "source/master-player-telemetry.jsonl").is_file()
    before = {str(p): sha256_file(p) for p in (scenario_path, manifest_path, result_path,
        RUN / "source/master-replay.clip", RUN / "source/master-player-telemetry.jsonl")}
    make_recorded_trajgen_clip(source, base_plan, record_result_path=result_path,
        scenario_path=scenario_path, ground_y=0.0, choreography_scale=1.0,
        choreography_mode="independent_windows")
    assert load(base_plan / "forward-plan-qc.json")["verdict"] == "PASS", "Base planning QC failed; keep its outputs."
    command = [sys.executable, "-X", "utf8", str(CAPTURE / "out/mc10-20260908/derive-recorded-body-framing.py"),
        "--record-run-dir", str(RUN), "--scenario", str(scenario_path),
        "--source-plan", str(base_plan), "--out-dir", str(proposal)]
    subprocess.run(command, cwd=CAPTURE, check=True)
    assert load(proposal / "review.json")["verdict"] == "PASS", "Framing proposal failed; retain it."
    shutil.copytree(proposal / "recorded-plan", final_plan)
    assert len(list(final_plan.iterdir())) == 13
    for p in final_plan.iterdir():
        assert p.is_file() and sha256_file(p) == sha256_file(proposal / "recorded-plan" / p.name)
    placement = load(final_plan / "plan.json")["subject_motion"]["placement"]
    assert placement["heading_deg"] == 0.0 and placement["ground_y"] == 0.0
    tape, route = place_trajgen_clip(final_plan,
        placement=Placement(anchor=tuple(placement["anchor"]), heading_deg=0.0, ground_y=0.0),
        filmback_id="pending-customer-confirmation",
        clip_id="gta-mc10-scene06-north-beach-60s-20260920-02")
    _validate_scenario_tape_route(Scenario.load(scenario_path), tape,
        expected_replay_sha256=master["replay_sha256"])
    assert tape["n_frames"] == 1800 and tape["duration_s"] == 60.0
    write_tape(tape_path, tape)
    write_json_atomic(route_path, route)
    write_native_tape(native_path, tape, json_path=tape_path)
    assert all(sha256_file(Path(p)) == digest for p, digest in before.items())
    receipt = {"schema": "dm-gta-fresh-plan-offline-preparation/1", "status": "PASS",
        "run_dir": str(RUN), "record_source_sha256": before, "placement": placement,
        "framing_command": command, "base_plan": str(base_plan), "framing_proposal": str(proposal),
        "placed_source_plan": str(final_plan), "outputs": [{"path": str(p), "bytes": p.stat().st_size,
            "sha256": sha256_file(p)} for p in (tape_path, route_path, native_path)],
        "game_calls_performed": False, "arm_performed": False, "source_inputs_unchanged": True,
        "actual_pixel_visibility_verified": False, "formal_delivery_eligible": False}
    write_json_atomic(receipt_path, receipt)
    print(json.dumps(receipt, ensure_ascii=False))


if __name__ == "__main__":
    main()

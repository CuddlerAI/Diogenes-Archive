import copy
import tempfile
import unittest
from pathlib import Path

from helpers import scenario_dict, write_json
from gta_capture.errors import ContractError
from gta_capture.evidence import REAL_PLAYER_OBSERVATION_METHOD
from gta_capture.jsonio import file_record, load_json, sha256_file, write_jsonl_atomic
from gta_capture.recorded_route import (build_recorded_subject_route,
                                        validate_recorded_subject_route)
from gta_capture.scenario import Scenario
from gta_capture.staged import _validate_scenario_tape_route
from gta_capture.tape import GTA_COORDINATE_CONVENTION


def _recording(root, *, observed_offset=(0.0, 0.0, 0.0)):
    root.mkdir(parents=True, exist_ok=True)
    scenario = scenario_dict(2.0)
    scenario["location"] = [500.0, -25.0, 31.5]
    scenario["player"].update(distance_m=10.0, speed_mps=1.0, heading_deg=0.0)
    scenario_path = write_json(root / "scenario.json", scenario)
    replay = root / "master-replay.clip"
    replay.write_bytes(b"sealed-test-replay")
    identity = "1234567890abcdef"
    rows = []
    for frame in range(60):
        t = frame / 30
        rows.append({
            "schema": "dm-gta-player-telemetry/1", "frame": frame,
            "source_t_ns": round(t * 1e9), "observation_t_ns": round(t * 1e9) + 100,
            "player_observation_method": REAL_PLAYER_OBSERVATION_METHOD,
            "replay_sha256": sha256_file(replay), "source_clip_identity": identity,
            "player_state": {"position": [500.0 + observed_offset[0],
                                          -25.0 + observed_offset[1] + 1.7 * t,
                                          31.5 + observed_offset[2]],
                             "velocity": [0.0, 1.7, 0.0], "euler_deg": [0.0, 0.0, 0.0],
                             "quaternion": [0.0, 0.0, 0.0, 1.0]},
            "entity_state_hash": "a" * 64,
        })
    telemetry = write_jsonl_atomic(root / "telemetry.jsonl", rows)
    record = {"ok": True, "scenario": {
        "applied": True, "story_mode": True, "scenario_sha256": sha256_file(scenario_path),
        "route_control": "native_task_go_straight", "route_kind": "forward_from_anchor",
        "route_distance_m": 10.0, "route_speed_mps": 1.0, "route_heading_deg": 0.0,
    }, "master": {
        "replay_path": str(replay), "replay_sha256": sha256_file(replay),
        "player_telemetry_path": str(telemetry), "player_telemetry_sha256": sha256_file(telemetry),
        "source_clip_identity": identity, "mock": False, "recording_start_verified": True,
        "player_observation_method": REAL_PLAYER_OBSERVATION_METHOD, "player_observation_count": 60,
        "source_control": "native_task_go_straight", "route_kind": "forward_from_anchor",
        "source_epoch": scenario["source_epoch"], "seed": scenario["seed"],
        "record_start_position": [scenario["location"][axis] + observed_offset[axis]
                                  for axis in range(3)],
    }}
    record_path = write_json(root / "record-result.json", record)
    return record_path, scenario_path, record, rows


def _placed_tape(root, route):
    anchor = route["waypoints"][0]["position"]
    plan = {"duration_s": route["duration_s"], "anchors": [
        {"t": row["t"], "p": [row["position"][0] - anchor[0],
                               row["position"][2] - anchor[2] + row["look_at_height_m"],
                               row["position"][1] - anchor[1]]} for row in route["waypoints"]]}
    path = write_json(root / "plan.json", plan)
    record = {"name": "plan.json", "sha256": sha256_file(path)}
    route["source"] = dict(record)
    placement = {"anchor": anchor, "heading_deg": 0.0, "ground_y": 0.0}
    route["placement"] = placement
    return {"coordinate_convention": GTA_COORDINATE_CONVENTION, "n_frames": len(route["waypoints"]),
            "source": {"kind": "trajgen_clip_placed", "path": str(root), "files": [record],
                       "anchor": anchor, "placement": placement,
                       "native_command": route["native_command"], "subject_route": route}}


class RecordedRouteTests(unittest.TestCase):
    def test_native_six_decimal_receipt_preserves_precise_sealed_command(self):
        for key, precise, reported in (
                ("heading_deg", 327.8663406286064, 327.866341),
                ("distance_m", 12.345678901, 12.345679),
                ("speed_mps", 0.234567891, 0.234568)):
            with self.subTest(key=key), tempfile.TemporaryDirectory() as tmp:
                paths = _recording(Path(tmp))
                scenario = load_json(paths[1])
                scenario["player"][key] = precise
                write_json(paths[1], scenario)
                paths[2]["scenario"].update({"scenario_sha256": sha256_file(paths[1]),
                                              "route_" + key: reported})
                write_json(paths[0], paths[2])
                before = {path: sha256_file(path) for path in paths[:2]}
                route = build_recorded_subject_route(*paths[:2])
                self.assertEqual(route["native_command"][key], precise)
                _validate_scenario_tape_route(Scenario.load(paths[1]),
                                             _placed_tape(Path(tmp), route))
                self.assertEqual(before, {path: sha256_file(path) for path in paths[:2]})

    def test_native_command_receipt_rejects_any_difference_from_encoded_value(self):
        for key, precise, reported in (
                ("heading_deg", 327.8663406286064, 327.866341),
                ("distance_m", 12.345678901, 12.345679),
                ("speed_mps", 0.234567891, 0.234568)):
            # Even a sub-micro-unit discrepancy must not be accepted merely
            # because rounding the receipt again would hide the difference.
            for changed in (reported + 0.000001, reported - 0.0000001, precise):
                with self.subTest(key=key, changed=changed), tempfile.TemporaryDirectory() as tmp:
                    paths = _recording(Path(tmp))
                    scenario = load_json(paths[1])
                    scenario["player"][key] = precise
                    write_json(paths[1], scenario)
                    paths[2]["scenario"].update({"scenario_sha256": sha256_file(paths[1]),
                                                  "route_" + key: changed})
                    write_json(paths[0], paths[2])
                    with self.assertRaisesRegex(ContractError, "native command differs"):
                        build_recorded_subject_route(*paths[:2])

    def test_settled_recording_anchor_preserves_observation_within_start_boundary(self):
        for offset in ((0.0, 0.0, -0.02), (0.03, 0.0, -0.04), (0.0, 0.0, -0.25)):
            with self.subTest(offset=offset), tempfile.TemporaryDirectory() as tmp:
                paths = _recording(Path(tmp), observed_offset=offset)
                route = build_recorded_subject_route(*paths[:2])
                scenario = Scenario.load(paths[1])
                tape = _placed_tape(Path(tmp), route)
                _validate_scenario_tape_route(scenario, tape,
                    expected_replay_sha256=paths[2]["master"]["replay_sha256"])
                self.assertEqual(tape["source"]["anchor"], paths[3][0]["player_state"]["position"])
                self.assertNotEqual(tape["source"]["anchor"], scenario.raw["location"])
                self.assertEqual(route["waypoints"][0]["position"], paths[3][0]["player_state"]["position"])
                self.assertAlmostEqual(route["waypoints"][-1]["position"][2], 31.5 + offset[2])

    def test_recording_settling_past_start_boundary_is_rejected(self):
        for offset in ((0.0, 0.0, -0.2501), (0.251, 0.0, 0.0)):
            with self.subTest(offset=offset), tempfile.TemporaryDirectory() as tmp:
                paths = _recording(Path(tmp), observed_offset=offset)
                with self.assertRaisesRegex(ContractError, "sealed Story anchor"):
                    build_recorded_subject_route(*paths[:2])

    def test_settled_recording_cannot_be_placed_back_at_requested_scenario_location(self):
        with tempfile.TemporaryDirectory() as tmp:
            paths = _recording(Path(tmp), observed_offset=(0.0, 0.0, -0.02))
            route = build_recorded_subject_route(*paths[:2])
            scenario = Scenario.load(paths[1])
            tape = _placed_tape(Path(tmp), route)
            tape["source"]["anchor"] = scenario.raw["location"]
            with self.assertRaisesRegex(ContractError, "observed first position"):
                _validate_scenario_tape_route(scenario, tape)

    def test_staged_observed_placement_itself_must_remain_inside_start_boundary(self):
        with tempfile.TemporaryDirectory() as tmp:
            paths = _recording(Path(tmp), observed_offset=(0.0, 0.0, -0.3))
            # Each earlier start-to-scenario / sample-to-start check is within
            # 0.25m, but those offsets cannot be accumulated into a 0.3m placement.
            paths[2]["master"]["record_start_position"][2] = 31.3
            write_json(paths[0], paths[2])
            route = build_recorded_subject_route(*paths[:2])
            tape = _placed_tape(Path(tmp), route)
            with self.assertRaisesRegex(ContractError, "0.25m start boundary"):
                _validate_scenario_tape_route(Scenario.load(paths[1]), tape)

    def test_native_speed_parameter_does_not_override_measured_physical_motion(self):
        with tempfile.TemporaryDirectory() as tmp:
            paths = _recording(Path(tmp))
            route = build_recorded_subject_route(*paths[:2])
            self.assertEqual(route["native_command"]["speed_mps"], 1.0)
            self.assertAlmostEqual(route["diagnostics"]["observed_average_speed_mps"], 1.7)
            self.assertAlmostEqual(route["waypoints"][-1]["position"][1], -25 + 1.7 * 59 / 30)
            self.assertIs(route["diagnostics"]["player_replay_origin_verified"], False)
            _validate_scenario_tape_route(Scenario.load(paths[1]), _placed_tape(Path(tmp), route),
                                         expected_replay_sha256=paths[2]["master"]["replay_sha256"])

    def test_changed_replay_and_other_run_replay_are_rejected(self):
        with tempfile.TemporaryDirectory() as tmp:
            paths = _recording(Path(tmp))
            route = build_recorded_subject_route(*paths[:2])
            with self.assertRaisesRegex(ContractError, "different or unverified"):
                validate_recorded_subject_route(route, scenario=Scenario.load(paths[1]),
                                                expected_replay_sha256="b" * 64)
            Path(paths[2]["master"]["replay_path"]).write_bytes(b"changed")
            with self.assertRaisesRegex(ContractError, "sealed bytes/sha256"):
                validate_recorded_subject_route(route, scenario=Scenario.load(paths[1]))

    def test_tampered_telemetry_cannot_be_rehashed_without_record_binding(self):
        with tempfile.TemporaryDirectory() as tmp:
            paths = _recording(Path(tmp))
            route = build_recorded_subject_route(*paths[:2])
            rows = paths[3]
            rows[30]["player_state"]["position"][0] += 2
            path = Path(paths[2]["master"]["player_telemetry_path"])
            write_jsonl_atomic(path, rows)
            route["observation"]["files"]["telemetry"] = file_record(path)
            with self.assertRaisesRegex(ContractError, "different or unverified"):
                validate_recorded_subject_route(route, scenario=Scenario.load(paths[1]))

    def test_waypoints_and_fit_statistics_are_recomputed(self):
        with tempfile.TemporaryDirectory() as tmp:
            paths = _recording(Path(tmp))
            route = build_recorded_subject_route(*paths[:2])
            changed = copy.deepcopy(route)
            changed["waypoints"][30]["position"][0] += 0.01
            with self.assertRaisesRegex(ContractError, "recomputed telemetry fit"):
                validate_recorded_subject_route(changed, scenario=Scenario.load(paths[1]))
            changed = copy.deepcopy(route)
            changed["diagnostics"]["observed_average_speed_mps"] = 1.0
            with self.assertRaisesRegex(ContractError, "diagnostics differ"):
                validate_recorded_subject_route(changed, scenario=Scenario.load(paths[1]))

    def test_no_trimmed_origin_or_verified_origin_claim_is_accepted(self):
        with tempfile.TemporaryDirectory() as tmp:
            paths = _recording(Path(tmp))
            route = build_recorded_subject_route(*paths[:2])
            changed = copy.deepcopy(route)
            changed["observation"]["selection"]["start_frame"] = 1
            with self.assertRaisesRegex(ContractError, "selection"):
                validate_recorded_subject_route(changed, scenario=Scenario.load(paths[1]))
            changed = copy.deepcopy(route)
            changed["observation"]["player_replay_origin_verified"] = True
            with self.assertRaisesRegex(ContractError, "origin limitation"):
                validate_recorded_subject_route(changed, scenario=Scenario.load(paths[1]))

    def test_fit_tolerance_cannot_be_relaxed_past_quarter_meter(self):
        with tempfile.TemporaryDirectory() as tmp:
            paths = _recording(Path(tmp))
            with self.assertRaisesRegex(ContractError, "supported bound"):
                build_recorded_subject_route(*paths[:2], max_residual_m=0.3)

    def test_snapshot_relocation_preserves_binding_without_old_paths(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            paths = _recording(root / "original")
            scenario = Scenario.load(paths[1])
            route = build_recorded_subject_route(*paths[:2], artifact_dir=root / "snapshot")
            for path in (paths[0], paths[1], Path(paths[2]["master"]["player_telemetry_path"]),
                         Path(paths[2]["master"]["replay_path"])):
                path.unlink()
            validate_recorded_subject_route(route, scenario=scenario)
            self.assertTrue(all(Path(record["path"]).parent == root / "snapshot"
                                for record in route["observation"]["files"].values()))

    def test_plan_anchors_cannot_claim_different_recorded_positions(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            paths = _recording(root)
            route = build_recorded_subject_route(*paths[:2])
            tape = _placed_tape(root, route)
            plan = load_json(root / "plan.json")
            plan["anchors"][30]["p"][1] += 0.1
            write_json(root / "plan.json", plan)
            digest = sha256_file(root / "plan.json")
            tape["source"]["files"][0]["sha256"] = digest
            route["source"]["sha256"] = digest
            with self.assertRaisesRegex(ContractError, "authored source anchor"):
                _validate_scenario_tape_route(Scenario.load(paths[1]), tape)

    def test_recorded_motion_has_separate_native_command_fields(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            paths = _recording(root)
            route = build_recorded_subject_route(*paths[:2])
            tape = _placed_tape(root, route)
            tape["source"]["route_speed_mps"] = 1.0
            with self.assertRaisesRegex(ContractError, "separate from motion"):
                _validate_scenario_tape_route(Scenario.load(paths[1]), tape)


if __name__ == "__main__":
    unittest.main()

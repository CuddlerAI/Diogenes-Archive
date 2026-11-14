import math
import tempfile
import unittest
from pathlib import Path

import helpers  # noqa: F401

from gta_capture.errors import ContractError
from gta_capture.jsonio import load_json, sha256_file, write_json_atomic, write_jsonl_atomic
from gta_capture.moving_plan import (_dependencies, make_forward_trajgen_clip,
                                     make_recorded_trajgen_clip)
from gta_capture.traj_adapter import Placement, place_trajgen_clip


def _synthetic_recording_fixture(root: Path, *, physical_speed=0.4,
                                 stop_distance=None, height_slope=0.0):
    """Explicit mock file fixture with the native recording schema; no gameplay."""
    from helpers import scenario_dict
    from gta_capture.evidence import REAL_PLAYER_OBSERVATION_METHOD

    root.mkdir()
    scenario = scenario_dict(60.0)
    scenario["location"] = [500.0, -25.0, 31.5]
    scenario["player"].update(distance_m=100.0, speed_mps=0.2, heading_deg=90.0)
    scenario_path = write_json_atomic(root / "scenario.json", scenario)
    replay = root / "synthetic-mock-replay.clip"
    replay.write_bytes(b"synthetic mock fixture: not a real Rockstar replay")
    replay_hash = sha256_file(replay)
    identity = "1234567890abcdef"
    rows = []
    for frame in range(1800):
        t = frame / 30.0
        travel = physical_speed * t
        if stop_distance is not None:
            travel = min(stop_distance, travel)
        speed = physical_speed if stop_distance is None or travel < stop_distance else 0.0
        rows.append({
            "test_fixture": "synthetic_mock", "schema": "dm-gta-player-telemetry/1",
            "frame": frame, "source_t_ns": (frame * 1_000_000_000 + 15) // 30,
            "observation_t_ns": (frame * 1_000_000_000 + 15) // 30 + 100,
            "player_observation_method": REAL_PLAYER_OBSERVATION_METHOD,
            "replay_sha256": replay_hash, "source_clip_identity": identity,
            "player_state": {"position": [500.0 - travel, -25.0,
                                           31.5 + height_slope * travel + 0.02 * math.sin(t / 8.0)],
                "velocity": [-speed, 0.0, height_slope * speed + 0.0025 * math.cos(t / 8.0)],
                "euler_deg": [0.0, 0.0, 90.0],
                "quaternion": [0.0, 0.0, math.sqrt(0.5), math.sqrt(0.5)]},
            "entity_state_hash": "a" * 64,
        })
    telemetry = write_jsonl_atomic(root / "synthetic-mock-telemetry.jsonl", rows)
    record = {"test_fixture": "synthetic_mock", "ok": True, "scenario": {
        "applied": True, "story_mode": True, "scenario_sha256": sha256_file(scenario_path),
        "route_control": "native_task_go_straight", "route_kind": "forward_from_anchor",
        "route_distance_m": 100.0, "route_speed_mps": 0.2, "route_heading_deg": 90.0},
        "master": {"replay_path": str(replay), "replay_sha256": replay_hash,
            "player_telemetry_path": str(telemetry), "player_telemetry_sha256": sha256_file(telemetry),
            "source_clip_identity": identity, "mock": False, "recording_start_verified": True,
            "player_observation_method": REAL_PLAYER_OBSERVATION_METHOD,
            "player_observation_count": 1800, "source_control": "native_task_go_straight",
            "route_kind": "forward_from_anchor", "source_epoch": scenario["source_epoch"],
            "seed": scenario["seed"], "record_start_position": scenario["location"]}}
    record_path = write_json_atomic(root / "record-result.json", record)
    return record_path, scenario_path


class ForwardPlanIntegrationTests(unittest.TestCase):
    """Run genuine source generation, adaptation, and unchanged external QC once."""

    @classmethod
    def setUpClass(cls):
        _dependencies()
        from trajgen.export import write_clip
        from trajgen.planner import QuotaTracker, generate
        from trajgen.scene import synthetic

        cls.temp = tempfile.TemporaryDirectory()
        cls.root = Path(cls.temp.name)
        cls.source = cls.root / "source"
        cls.output = cls.root / "forward"
        original = generate(synthetic("urban_block", 1), seed=7,
                            quota=QuotaTracker(), cam4_mode="topdown")
        if not original.ok:
            raise AssertionError(original.reasons)
        write_clip(original, cls.source)
        # A stale source overlap must not survive a nonrigid trajectory change.
        write_json_atomic(cls.source / "covis.json", {"covis_ratio": 1.0})
        cls.source_hashes = {p.name: sha256_file(p) for p in cls.source.iterdir()}
        cls.report = make_forward_trajgen_clip(cls.source, cls.output)
        cls.recording = _synthetic_recording_fixture(cls.root / "synthetic-mock-recording")
        cls.recorded_output = cls.root / "recorded-follow"
        cls.recorded_report = make_recorded_trajgen_clip(cls.source, cls.recorded_output,
            record_result_path=cls.recording[0], scenario_path=cls.recording[1])

    @classmethod
    def tearDownClass(cls):
        cls.temp.cleanup()

    def test_moving_clip_passes_real_qc_including_all_translation_windows(self):
        self.assertEqual(self.report["verdict"], "PASS")
        self.assertEqual(self.report["counts"]["FAIL"], 0)
        honest = [item for item in self.report["results"]
                  if item["rule"] == "plan.pink_honesty"]
        self.assertEqual(len(honest), 24)
        self.assertTrue(all(item["status"] == "PASS" for item in honest))
        for rule in ("phys.jerk_p95", "phys.accel_p95", "phys.speed_p95"):
            results = [item for item in self.report["results"] if item["rule"] == rule]
            self.assertEqual(len(results), 4)
            self.assertTrue(all(item["status"] == "PASS" for item in results))

    def test_subject_moves_24m_and_cameras_accompany_it(self):
        plan = load_json(self.output / "plan.json")
        route = plan["anchors"]
        self.assertEqual(len(route), 1800)
        self.assertAlmostEqual(route[-1]["p"][2] - route[0]["p"][2],
                               0.4 * 1799 / 30.0, places=7)
        self.assertEqual(route[-1]["p"][:2], route[0]["p"][:2])
        for lane in range(1, 5):
            records = load_json(self.output / f"action_camera_{lane}.json")
            self.assertGreater(math.dist(records[0]["camera_position"],
                                         records[-1]["camera_position"]), 15.0)

    def test_planning_overlap_is_recomputed_and_explicitly_not_gta_evidence(self):
        covis = load_json(self.output / "covis.json")
        self.assertEqual(covis["geometry_source"], "synthetic_planar_proxy")
        self.assertIs(covis["measured_in_gta"], False)
        measured = sum(value >= 0.3 for _, value in covis["pair_min_overlap_ts"])
        self.assertAlmostEqual(covis["covis_ratio"], measured / 120.0, places=4)
        self.assertGreaterEqual(covis["covis_ratio"], 0.5)
        self.assertLess(covis["covis_ratio"], 1.0)
        self.assertEqual(self.source_hashes,
                         {p.name: sha256_file(p) for p in self.source.iterdir()})

    def test_every_view_contains_the_moving_subject_full_body(self):
        from itertools import product
        from trajgen.mathutil import quat_forward, quat_right, quat_up, vdot, vsub
        anchors = load_json(self.output / "plan.json")["anchors"]
        for lane in range(1, 5):
            records = load_json(self.output / f"action_camera_{lane}.json")
            for anchor, record in zip(anchors, records):
                quat, intr = record["camera_rotation_quaternion"], record["camera_intrinsics"]
                self.assertGreaterEqual(record["camera_position"][1], 0.5)
                for dx, y, dz in product((-0.35, 0.35), (0.0, 1.85), (-0.35, 0.35)):
                    point = (anchor["p"][0] + dx, y, anchor["p"][2] + dz)
                    relative = vsub(point, record["camera_position"])
                    z = vdot(relative, quat_forward(quat))
                    self.assertGreater(z, 0.0)
                    u = intr["cx"] + intr["fx"] * vdot(relative, quat_right(quat)) / z
                    v = intr["cy"] - intr["fy"] * vdot(relative, quat_up(quat)) / z
                    self.assertTrue(100.0 < u < 1820.0 and 100.0 < v < 980.0,
                                    f"lane {lane} frame {record['frame']} crops the body box")

    def test_placed_route_binds_real_forward_direction_at_every_frame(self):
        placement = Placement(anchor=(500.0, -25.0, 31.5), heading_deg=137.0)
        tape, route = place_trajgen_clip(self.output, placement=placement,
                                         filmback_id="test")
        self.assertEqual(tape["source"]["subject_route"], route)
        self.assertEqual(tape["source"]["route_distance_m"], 24.0)
        self.assertEqual(tape["source"]["route_speed_mps"], 0.4)
        self.assertEqual(tape["source"]["route_heading_deg"], 137.0)
        self.assertEqual(route["source"]["sha256"], sha256_file(self.output / "plan.json"))
        h = math.radians(137.0)
        for frame, waypoint in enumerate(route["waypoints"]):
            travel = 0.4 * frame / 30.0
            self.assertAlmostEqual(waypoint["position"][0], 500.0 - math.sin(h) * travel,
                                   places=5)
            self.assertAlmostEqual(waypoint["position"][1], -25.0 + math.cos(h) * travel,
                                   places=5)
            self.assertEqual(waypoint["position"][2], 31.5)

    def test_impossible_midclip_stop_and_output_overwrite_are_rejected(self):
        with self.assertRaisesRegex(ContractError, "stop inside"):
            make_forward_trajgen_clip(self.source, self.root / "stop", distance_m=5.0)
        with self.assertRaisesRegex(ContractError, "new empty"):
            make_forward_trajgen_clip(self.source, self.output)

    def test_adapter_rejects_a_declared_forward_route_that_meanders(self):
        import shutil
        bad = self.root / "bad-forward"
        shutil.copytree(self.output, bad)
        plan = load_json(bad / "plan.json")
        plan["anchors"][900]["p"][0] += 0.1
        write_json_atomic(bad / "plan.json", plan)
        with self.assertRaisesRegex(ContractError, "deviates"):
            place_trajgen_clip(bad, placement=Placement((500.0, -25.0, 31.5), 137.0),
                               filmback_id="test")

    def test_recorded_route_follows_observed_speed_and_preserves_native_parameter(self):
        from gta_capture.scenario import Scenario
        from gta_capture.staged import _validate_scenario_tape_route

        self.assertEqual(self.recorded_report["counts"]["FAIL"], 0)
        tape, route = place_trajgen_clip(self.recorded_output,
            placement=Placement((500.0, -25.0, 31.5), 0.0), filmback_id="test")
        self.assertEqual(route["native_command"]["speed_mps"], 0.2)
        self.assertAlmostEqual(route["waypoints"][-1]["position"][0],
                               500.0 - 0.4 * 1799 / 30, places=5)
        self.assertNotEqual(route["waypoints"][-1]["position"][2], 31.5)
        self.assertNotIn("route_speed_mps", tape["source"])
        self.assertIs(route["observation"]["player_replay_origin_verified"], False)
        self.assertEqual(tape["source"]["observation_semantics"], "recording_side_estimate")
        _validate_scenario_tape_route(Scenario.load(self.recording[1]), tape)

    def test_recorded_plan_cannot_be_placed_at_a_different_world_origin(self):
        with self.assertRaisesRegex(ContractError, "measured world placement"):
            place_trajgen_clip(self.recorded_output,
                placement=Placement((501.0, -25.0, 31.5), 0.0), filmback_id="test")

    def test_recorded_plan_rejects_rehashed_anchor_that_differs_from_observations(self):
        import shutil
        bad = self.root / "bad-recorded"
        shutil.copytree(self.recorded_output, bad)
        plan = load_json(bad / "plan.json")
        plan["anchors"][900]["p"][2] += 0.1
        write_json_atomic(bad / "plan.json", plan)
        with self.assertRaisesRegex(ContractError, "sealed observed route"):
            place_trajgen_clip(bad, placement=Placement((500.0, -25.0, 31.5), 0.0),
                               filmback_id="test")

    def test_larger_camera_choreography_handles_faster_observed_walking(self):
        record, scenario = _synthetic_recording_fixture(self.root / "synthetic-faster",
                                                        physical_speed=0.7)
        output = self.root / "recorded-larger"
        report = make_recorded_trajgen_clip(self.source, output, record_result_path=record,
            scenario_path=scenario, choreography_scale=1.5)
        self.assertEqual(report["verdict"], "PASS")
        plan = load_json(output / "plan.json")
        self.assertEqual(plan["forward_adaptation"]["choreography_scale"], 1.5)
        route = plan["subject_motion"]["recorded_route"]
        self.assertEqual(route["native_command"]["speed_mps"], 0.2)
        self.assertAlmostEqual(route["waypoints"][-1]["position"][0],
                               500.0 - 0.7 * 1799 / 30.0, places=5)
        self.assertTrue(all(item["full_body_in_frustum_ratio"] == 1.0
            for item in plan["forward_adaptation"]["subject_framing"].values()))

    def test_unsupported_recorded_motion_does_not_bypass_translation_qc(self):
        record, scenario = _synthetic_recording_fixture(self.root / "synthetic-too-fast",
                                                        physical_speed=1.74)
        output = self.root / "recorded-rejected"
        with self.assertRaisesRegex(ContractError, "failed engine_qc"):
            make_recorded_trajgen_clip(self.source, output, record_result_path=record,
                                      scenario_path=scenario)
        report = load_json(output / "forward-plan-qc.json")
        self.assertEqual(report["verdict"], "FAIL")
        self.assertTrue(any(item["rule"] == "plan.pink_honesty" and item["status"] == "FAIL"
                            for item in report["results"]))

    def test_independent_ground_windows_handle_normal_walk_downhill_and_real_stop(self):
        record, scenario = _synthetic_recording_fixture(self.root / "synthetic-normal-walk",
            physical_speed=1.74, stop_distance=100.0, height_slope=-0.013)
        output = self.root / "recorded-independent"
        report = make_recorded_trajgen_clip(self.source, output, record_result_path=record,
            scenario_path=scenario, choreography_mode="independent_windows")
        self.assertEqual(report["verdict"], "PASS")
        honest = [item for item in report["results"] if item["rule"] == "plan.pink_honesty"]
        self.assertEqual(len(honest), 24)
        self.assertTrue(all(item["status"] == "PASS" for item in honest))
        tape, route = place_trajgen_clip(output,
            placement=Placement((500.0, -25.0, 31.5), 0.0), filmback_id="test")
        self.assertAlmostEqual(route["waypoints"][-1]["position"][0], 400.0, places=5)
        self.assertAlmostEqual(route["waypoints"][-30]["position"][0], 400.0, places=5)
        for lane in range(1, 4):
            records = load_json(output / f"action_camera_{lane}.json")
            for frame, row in enumerate(records):
                self.assertGreater(row["camera_position"][1], 0.3)
                entity_y = route["waypoints"][frame]["position"][2] - 31.5
                self.assertTrue(1.6 < row["camera_position"][1] - entity_y < 3.5)
        covis = load_json(output / "covis.json")
        horizontal = load_json(output / "covis-horizontal-proxy.json")
        self.assertFalse(covis["measured_in_gta"])
        self.assertFalse(horizontal["measured_in_gta"])
        plan = load_json(output / "plan.json")
        self.assertTrue(all(item["full_body_in_frustum_ratio"] == 1.0
            for item in plan["forward_adaptation"]["subject_framing"].values()))

    def test_slope_proxy_preserves_projection_under_known_rigid_plane_transform(self):
        from gta_capture.moving_plan import _recorded_slope_proxy
        from trajgen.covis import CamView
        from trajgen.mathutil import (quat_forward, quat_right, quat_up,
            quat_from_yaw_elev_roll, yaw_elev_from_forward, vdot, vsub)

        slope = -0.02
        subject = [(x, 1.65 + slope * x, 0.0) for x in range(101)]
        position, point = (7.0, 3.0, 2.0), (24.0, slope * 24.0, -1.0)
        q = quat_from_yaw_elev_roll(*yaw_elev_from_forward(vsub(point, position)), 7.0)
        original = CamView(position, q, 1200, 1200, 960, 540, 1920, 1080)
        views, fit = _recorded_slope_proxy({1: [original]}, subject, ground_y=0.0)
        transformed = views[1][0]
        flat_point = (24.0 * math.sqrt(1.0 + slope ** 2), 0.0, -1.0)

        def camera_coordinates(view, p):
            delta = vsub(p, view.p)
            return [vdot(delta, axis(view.q)) for axis in (quat_right, quat_up, quat_forward)]

        for before, after in zip(camera_coordinates(original, point),
                                 camera_coordinates(transformed, flat_point)):
            self.assertAlmostEqual(before, after, places=10)
        self.assertAlmostEqual(fit["slope"], slope, places=12)
        self.assertLess(fit["height_residual_max_m"], 1e-12)


if __name__ == "__main__":
    unittest.main()

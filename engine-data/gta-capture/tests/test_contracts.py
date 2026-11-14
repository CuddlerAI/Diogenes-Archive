import math
import tempfile
import unittest
from pathlib import Path

from helpers import manifest_dict, scenario_dict, write_json
from gta_capture.errors import ContractError
from gta_capture.native_tape import inspect_native_tape, write_native_tape
from gta_capture.policy import CapturePolicy, repository_profile_non_trainable
from gta_capture.preflight import DependencyManifest, run_preflight
from gta_capture.scenario import Scenario
from gta_capture.tape import (GTA_COORDINATE_CONVENTION, import_trajgen_clip,
                              load_tape, make_demo_tape, make_gta_poc_tape,
                              validate_tape, write_tape, _euler_zxy_deg,
                              _quaternion_zxy)


class TapeTests(unittest.TestCase):
    def test_gta_order2_quaternion_matches_native_camera_basis(self):
        euler = [-6.842773413, 0.0, -38.0]
        quaternion = _quaternion_zxy(euler)

        # GTA reads order-2 angles directly from its right/forward/up rows.
        x, y, z, w = quaternion
        right_z = 2.0 * (x * z - w * y)
        forward_x = 2.0 * (x * y - w * z)
        forward_y = 1.0 - 2.0 * (x * x + z * z)
        forward_z = 2.0 * (y * z + w * x)
        up_z = 1.0 - 2.0 * (x * x + y * y)
        native = [
            math.degrees(math.asin(forward_z)),
            math.degrees(math.atan2(-right_z, up_z)),
            math.degrees(math.atan2(-forward_x, forward_y)),
        ]
        for actual, expected in zip(native, euler):
            self.assertAlmostEqual(actual, expected, places=9)

    def test_gta_order2_quaternion_euler_round_trip(self):
        for euler in ([12.5, -7.25, 42.0], [-6.842773413, 0.0, -38.0]):
            recovered = _euler_zxy_deg(_quaternion_zxy(list(euler)))
            for actual, expected in zip(recovered, euler):
                self.assertAlmostEqual(actual, expected, places=9)

    def test_demo_tape_is_four_lane_and_frame_exact(self):
        tape = make_demo_tape(duration_s=1.0)
        self.assertEqual(tape["n_frames"], 30)
        self.assertEqual(set(tape["lanes"]), {"1", "2", "3", "4"})
        self.assertEqual({tape["lanes"][x]["focal_mm"] for x in tape["lanes"]},
                         {18, 24, 35, 50})
        self.assertEqual(tape["lanes"]["1"]["samples"][1]["source_t_ns"], 33_333_333)

    def test_cross_lane_timeline_mismatch_is_rejected(self):
        tape = make_demo_tape(duration_s=1.0)
        tape["lanes"]["4"]["samples"][1]["source_t_ns"] += 1
        with self.assertRaisesRegex(ContractError, "source_t_ns"):
            validate_tape(tape)

    def test_tape_round_trip(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "tape.json"
            write_tape(path, make_demo_tape(duration_s=1.0))
            loaded = load_tape(path, expected_duration_s=1.0)
        self.assertEqual(loaded["n_frames"], 30)

    def test_native_tape_cache_is_sealed_and_sized(self):
        tape = make_demo_tape(duration_s=1.0)
        with tempfile.TemporaryDirectory() as tmp:
            target = Path(tmp) / "camera-tape.bin"
            write_native_tape(target, tape)
            envelope = inspect_native_tape(target)
        self.assertEqual(envelope["lane_count"], 4)
        self.assertEqual(envelope["frame_count"], 30)
        self.assertEqual(envelope["fps_num"], 30)
        self.assertEqual(envelope["sample_stride"], 128)

    def test_import_trajgen_uses_action_pose_and_system_resolution(self):
        source = make_demo_tape(duration_s=1.0)
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            plan = {
                "duration_s": 1.0,
                "cameras": [{"id": lane, "focal_mm": source["lanes"][str(lane)]["focal_mm"]}
                            for lane in range(1, 5)],
            }
            write_json(root / "plan.json", plan)
            for lane in range(1, 5):
                lane_data = source["lanes"][str(lane)]
                actions = [{
                    "frame": sample["frame"],
                    "camera_position": sample["position"],
                    "camera_rotation_quaternion": sample["quaternion"],
                    "camera_intrinsics": sample["intrinsics"],
                } for sample in lane_data["samples"]]
                write_json(root / f"action_camera_{lane}.json", actions)
                write_json(root / f"systeminfo_{lane}.json", {
                    "width": lane_data["resolution"]["width"],
                    "height": lane_data["resolution"]["height"],
                })
            imported = import_trajgen_clip(root, filmback_id="test-filmback")
            native_path = root / "camera-tape.bin"
            write_native_tape(native_path, imported)
        self.assertEqual(imported["n_frames"], 30)
        self.assertEqual(imported["lanes"]["3"]["samples"][7]["position"],
                         source["lanes"]["3"]["samples"][7]["position"])
        self.assertIn("euler_deg", imported["lanes"]["3"]["samples"][7])

    def test_gta_poc_tape_is_absolute_and_native(self):
        tape = make_gta_poc_tape(anchor=[100.0, 200.0, 30.0], duration_s=1.0)
        self.assertEqual(tape["coordinate_convention"], GTA_COORDINATE_CONVENTION)
        self.assertGreater(tape["lanes"]["1"]["samples"][0]["position"][0], 90.0)
        with tempfile.TemporaryDirectory() as tmp:
            write_native_tape(Path(tmp) / "camera-tape.bin", tape)

    def test_gta_poc_tape_starts_covisible_and_uses_decoupled_motion(self):
        tape = make_gta_poc_tape(
            anchor=[215.0, -925.0, 30.7], duration_s=15.0,
            route_distance_m=15.0, route_speed_mps=1.0,
            route_heading_deg=90.0)
        self.assertEqual(tape["source"]["motion_profile"],
                         "decoupled-covisible-poc-v2")

        def forward(q):
            x, y, z, w = q
            return [2.0 * (x * y - w * z),
                    1.0 - 2.0 * (x * x + z * z),
                    2.0 * (y * z + w * x)]

        first = [tape["lanes"][str(lane)]["samples"][0] for lane in range(1, 5)]
        for i, sample_a in enumerate(first):
            for sample_b in first[i + 1:]:
                fa, fb = forward(sample_a["quaternion"]), forward(sample_b["quaternion"])
                dot = sum(a * b for a, b in zip(fa, fb))
                self.assertLessEqual(math.degrees(math.acos(max(-1.0, min(1.0, dot)))),
                                     15.0)

        displacements = []
        for lane in range(1, 5):
            samples = tape["lanes"][str(lane)]["samples"]
            displacements.append(tuple(samples[-1]["position"][axis]
                                       - samples[0]["position"][axis]
                                       for axis in range(3)))
        self.assertEqual(len(set(displacements)), 4)
        self.assertLess(displacements[0][0] * displacements[2][0]
                        + displacements[0][1] * displacements[2][1], 0.0)

    def test_gta_poc_tape_accepts_surveyed_clear_space_offsets(self):
        offsets = [-2.0, -4.0, -4.0, -6.0]
        tape = make_gta_poc_tape(
            anchor=[324.0, 168.0, 103.0], duration_s=1.0,
            route_heading_deg=90.0, lane_lateral_offsets_m=offsets)
        self.assertEqual(tape["source"]["lane_lateral_offsets_m"], offsets)
        for lane, offset in enumerate(offsets, 1):
            self.assertAlmostEqual(
                tape["lanes"][str(lane)]["samples"][0]["position"][1],
                168.0 + offset)

    def test_gta_poc_tape_rejects_poc061_orientation_before_capture(self):
        for heading in (0.0, 90.0, 180.0, 270.0, 359.9):
            with self.subTest(heading=heading), self.assertRaisesRegex(
                    ContractError, r"initial orientation lanes 1-2.*21.85.*<=15.0"):
                make_gta_poc_tape(
                    anchor=[324.0, 168.0, 103.640831], duration_s=15.0,
                    route_heading_deg=heading,
                    lane_lateral_offsets_m=[-2.0, -8.0, -4.0, -6.0])

    def test_gta_poc_layout_guard_does_not_claim_clearance_or_delivery(self):
        tape = make_gta_poc_tape(
            anchor=[324.0, 168.0, 103.640831], duration_s=1.0,
            route_heading_deg=90.0,
            lane_lateral_offsets_m=[-2.0, -4.0, -4.0, -6.0])
        self.assertFalse(tape["source"]["delivery_eligible"])
        self.assertNotIn("collision_verified", tape["source"])
        self.assertNotIn("projection_calibration", tape["source"])


class PolicyAndPreflightTests(unittest.TestCase):
    def test_internal_policy_is_non_delivery(self):
        policy = CapturePolicy.from_dict(manifest_dict()["policy"])
        self.assertFalse(policy.delivery_eligible)
        self.assertEqual(policy.validate(), [])
        self.assertTrue(repository_profile_non_trainable())

    def test_internal_policy_rejects_upload(self):
        value = manifest_dict()["policy"]
        value["upload_enabled"] = True
        policy = CapturePolicy.from_dict(value)
        self.assertIn("internal POC must keep upload_enabled=false", policy.validate())

    def test_dry_preflight_skips_missing_game(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = write_json(Path(tmp) / "manifest.json", manifest_dict())
            report = run_preflight(DependencyManifest.load(path), mode="dry-run")
        self.assertTrue(report.ok)
        self.assertIn("SKIP", {x.status for x in report.checks})

    def test_real_preflight_requires_files_and_hashes(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = write_json(Path(tmp) / "manifest.json", manifest_dict())
            report = run_preflight(DependencyManifest.load(path), mode="real")
        self.assertFalse(report.ok)
        self.assertTrue(any(x.check_id == "game.gta5_exe" and x.status == "FAIL"
                            for x in report.checks))


class ScenarioTests(unittest.TestCase):
    def test_busy_ambient_population_is_explicit_and_excludes_no_random_entities(self):
        value = scenario_dict()
        value["effect_exclusions"] = [
            item for item in value["effect_exclusions"]
            if item not in {"random_peds", "random_vehicles"}
        ]
        value["population"] = {
            "population_mode": "ambient_observation_only",
            "ped_density": 1.0,
            "scenario_ped_density": 1.0,
            "vehicle_density": 1.0,
            "random_vehicle_density": 1.0,
            "parked_vehicle_density": 0.75,
        }
        with tempfile.TemporaryDirectory() as tmp:
            scenario = Scenario.load(write_json(Path(tmp) / "scenario.json", value))
        self.assertEqual(scenario.raw["population"]["population_mode"],
                         "ambient_observation_only")

    def test_busy_ambient_population_rejects_sparse_or_excluded_entities(self):
        value = scenario_dict()
        value["population"] = {
            "population_mode": "ambient_observation_only",
            "ped_density": 0.5,
            "scenario_ped_density": 1.0,
            "vehicle_density": 1.0,
            "random_vehicle_density": 1.0,
            "parked_vehicle_density": 0.75,
        }
        with tempfile.TemporaryDirectory() as tmp:
            path = write_json(Path(tmp) / "scenario.json", value)
            with self.assertRaisesRegex(ContractError, "busy pedestrian/vehicle"):
                Scenario.load(path)

    def test_online_mode_is_rejected(self):
        value = scenario_dict()
        value["mode"] = "online"
        with tempfile.TemporaryDirectory() as tmp:
            path = write_json(Path(tmp) / "scenario.json", value)
            with self.assertRaisesRegex(ContractError, "story_mode"):
                Scenario.load(path)

    def test_radio_must_be_off(self):
        value = scenario_dict()
        value["radio"] = "on"
        with tempfile.TemporaryDirectory() as tmp:
            path = write_json(Path(tmp) / "scenario.json", value)
            with self.assertRaisesRegex(ContractError, "radio"):
                Scenario.load(path)


if __name__ == "__main__":
    unittest.main()

import copy
import math
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from helpers import scenario_dict, write_json
from gta_capture.errors import ContractError
from gta_capture.jsonio import sha256_file
from gta_capture.scenario import Scenario, validate_scenario
from gta_capture.staged import _validate_real_inputs, _validate_scenario_tape_route
from gta_capture.tape import GTA_COORDINATE_CONVENTION


def _placed_route(root: Path, *, heading=137.0, distance=0.8):
    """An independently authored straight track in both coordinate systems."""
    anchor = [500.0, -25.0, 31.5]
    speed, duration, count = 0.4, 2.0, 60
    value = scenario_dict(duration)
    value["location"] = anchor
    value["player"].update(distance_m=distance, speed_mps=speed, heading_deg=heading)
    scenario = Scenario.load(write_json(root / "scenario.json", value))
    placement = {"anchor": anchor, "heading_deg": heading, "ground_y": 0.0}
    plan = {"duration_s": duration, "anchors": []}
    route = {
        "schema": "dm-gta-subject-route/1", "duration_s": duration,
        "control": "native_task_go_straight", "route_kind": "forward_from_anchor",
        "distance_m": distance, "speed_mps": speed, "heading_deg": heading,
        "placement": placement, "waypoints": [],
    }
    rad = math.radians(heading)
    for frame in range(count):
        t = frame / 30.0
        travelled = min(distance, speed * t)
        plan["anchors"].append({"t": t, "p": [10.0, 1.15, 20.0 + travelled]})
        route["waypoints"].append({
            "t": t,
            "position": [anchor[0] - math.sin(rad) * travelled,
                         anchor[1] + math.cos(rad) * travelled, anchor[2]],
            "look_at_height_m": 1.15,
        })
    plan_path = write_json(root / "plan.json", plan)
    record = {"name": "plan.json", "sha256": sha256_file(plan_path)}
    route["source"] = dict(record)
    tape = {
        "coordinate_convention": GTA_COORDINATE_CONVENTION,
        "duration_s": duration, "n_frames": count,
        "source": {
            "kind": "trajgen_clip_placed", "path": str(root), "files": [record],
            "anchor": anchor, "placement": placement,
            "route_distance_m": distance, "route_speed_mps": speed,
            "route_heading_deg": heading, "subject_route": route,
        },
    }
    return scenario, tape, plan


def _replace_plan(root, tape, plan):
    """Rebind a changed plan, so tests distinguish geometry from hash checks."""
    path = write_json(root / "plan.json", plan)
    digest = sha256_file(path)
    tape["source"]["files"][0]["sha256"] = digest
    tape["source"]["subject_route"]["source"]["sha256"] = digest


class SubjectRouteBindingTests(unittest.TestCase):
    def test_legacy_authored_anchor_keeps_one_millimeter_scenario_tolerance(self):
        with tempfile.TemporaryDirectory() as tmp:
            scenario, tape, _ = _placed_route(Path(tmp))
            # A legacy authored plan has no observation to justify settling.
            scenario.raw["location"] = [*scenario.raw["location"][:2], 31.48]
            with self.assertRaisesRegex(ContractError, "matching the tape anchor"):
                _validate_scenario_tape_route(scenario, tape)

    def test_straight_route_is_valid_at_cardinal_and_oblique_headings(self):
        for heading in (0.0, 90.0, 137.0, 180.0, 270.0, 359.5):
            with self.subTest(heading=heading), tempfile.TemporaryDirectory() as tmp:
                scenario, tape, _ = _placed_route(Path(tmp), heading=heading)
                _validate_scenario_tape_route(scenario, tape)

    def test_native_route_can_reach_destination_before_record_end(self):
        with tempfile.TemporaryDirectory() as tmp:
            scenario, tape, _ = _placed_route(Path(tmp), distance=0.2)
            _validate_scenario_tape_route(scenario, tape)

    def test_echoing_route_scalars_without_subject_track_is_rejected(self):
        with tempfile.TemporaryDirectory() as tmp:
            scenario, tape, _ = _placed_route(Path(tmp))
            del tape["source"]["subject_route"]
            with self.assertRaisesRegex(ContractError, "native straight task"):
                _validate_scenario_tape_route(scenario, tape)

    def test_midpoint_route_drift_is_rejected_even_when_endpoints_match(self):
        with tempfile.TemporaryDirectory() as tmp:
            scenario, tape, _ = _placed_route(Path(tmp))
            tape["source"]["subject_route"]["waypoints"][30]["position"][0] += 0.1
            with self.assertRaisesRegex(ContractError, "waypoint 30.*native straight route"):
                _validate_scenario_tape_route(scenario, tape)

    def test_straight_route_cannot_be_attached_to_a_curved_plan(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            scenario, tape, plan = _placed_route(root)
            plan["anchors"][30]["p"][0] += 0.5
            _replace_plan(root, tape, plan)
            with self.assertRaisesRegex(ContractError, "waypoint 30.*authored source anchor"):
                _validate_scenario_tape_route(scenario, tape)

    def test_aim_height_is_bound_to_authored_plan(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            scenario, tape, plan = _placed_route(root)
            plan["anchors"][30]["p"][1] += 0.5
            _replace_plan(root, tape, plan)
            with self.assertRaisesRegex(ContractError, "waypoint 30.*authored source anchor"):
                _validate_scenario_tape_route(scenario, tape)

    def test_changed_plan_is_rejected_before_its_geometry_is_used(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            scenario, tape, plan = _placed_route(root)
            plan["changed"] = True
            write_json(root / "plan.json", plan)
            with self.assertRaisesRegex(ContractError, "sealed sha256"):
                _validate_scenario_tape_route(scenario, tape)

    def test_route_requires_complete_frame_timeline(self):
        for mutation in ("missing_frame", "wrong_time", "nan_time"):
            with self.subTest(mutation=mutation), tempfile.TemporaryDirectory() as tmp:
                scenario, tape, _ = _placed_route(Path(tmp))
                waypoints = tape["source"]["subject_route"]["waypoints"]
                if mutation == "missing_frame":
                    waypoints.pop()
                else:
                    waypoints[30]["t"] = float("nan") if mutation == "nan_time" else 1.01
                with self.assertRaises(ContractError):
                    _validate_scenario_tape_route(scenario, tape)

    def test_bad_route_never_reaches_real_preflight(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            scenario, tape, _ = _placed_route(root)
            tape["source"]["subject_route"]["waypoints"][5]["position"][2] += 1
            with patch("gta_capture.staged.run_preflight") as preflight:
                with self.assertRaisesRegex(ContractError, "native straight route"):
                    _validate_real_inputs(None, scenario, tape, root,
                                          allow_internal_calibration_gaps=False)
            preflight.assert_not_called()

    def test_nonfinite_or_boolean_inputs_cannot_bypass_route_math(self):
        with tempfile.TemporaryDirectory() as tmp:
            scenario, original, _ = _placed_route(Path(tmp))
            for key, value in (("route_speed_mps", True), ("route_distance_m", float("nan"))):
                with self.subTest(key=key):
                    tape = copy.deepcopy(original)
                    tape["source"][key] = value
                    with self.assertRaisesRegex(ContractError, "route differs"):
                        _validate_scenario_tape_route(scenario, tape)
            tape = copy.deepcopy(original)
            tape["source"]["subject_route"]["waypoints"][30]["position"][0] = float("nan")
            with self.assertRaisesRegex(ContractError, "numeric vec3"):
                _validate_scenario_tape_route(scenario, tape)

    def test_replay_duration_and_warmup_must_be_finite_numbers(self):
        for key in ("duration_s", "warmup_s"):
            for value in (True, float("nan"), float("inf")):
                with self.subTest(key=key, value=value):
                    scenario = scenario_dict()
                    scenario["replay"][key] = value
                    with self.assertRaisesRegex(ContractError, "scenario.replay"):
                        validate_scenario(scenario)


if __name__ == "__main__":
    unittest.main()

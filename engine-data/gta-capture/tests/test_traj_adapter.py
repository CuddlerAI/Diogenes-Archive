import json
import math
import tempfile
import unittest
from pathlib import Path

import helpers  # noqa: F401  (puts the module root on sys.path)

from gta_capture.errors import ContractError
from gta_capture.evidence import (_gta_to_delivery_quaternion,
                                  _gta_to_delivery_vector)
from gta_capture.tape import (GTA_COORDINATE_CONVENTION, _look_at_euler,
                              _quaternion_zxy)
from gta_capture.traj_adapter import (MAX_HORIZONTAL_EXCURSION_M, Placement,
                                      place_trajgen_clip)


ANCHOR_GTA = (-1180.5, -1510.25, 4.62)
AIM_HEIGHT_M = 1.15
N_FRAMES = 60


def _gta_reference_clip():
    """Author a four-lane look-at rig directly in GTA world coordinates.

    Building the reference in GTA terms and converting *down* to the delivery
    basis means the fixture never depends on the code under test to define what
    "correct" is: placing the converted clip back must reproduce these exact
    poses.
    """
    lanes = {}
    for lane, (back, lateral, height) in enumerate(
            ((12.0, -0.6, 2.3), (13.0, -0.2, 2.5), (14.0, 0.2, 2.7), (15.0, 0.6, 2.9)), 1):
        samples = []
        for frame in range(N_FRAMES):
            travelled = 0.5 * frame / 30.0
            target = [ANCHOR_GTA[0], ANCHOR_GTA[1] + travelled,
                      ANCHOR_GTA[2] + AIM_HEIGHT_M]
            position = [ANCHOR_GTA[0] + lateral, ANCHOR_GTA[1] - back,
                        ANCHOR_GTA[2] + height]
            euler = _look_at_euler(position, target)
            samples.append((position, _quaternion_zxy(euler), target))
        lanes[lane] = samples
    return lanes


def _write_delivery_clip(root: Path, lanes) -> None:
    """Convert the GTA reference into a trajgen-shaped delivery-basis clip."""
    anchor_delivery = _gta_to_delivery_vector(list(ANCHOR_GTA))
    for lane, samples in lanes.items():
        records = []
        for position, quaternion, _ in samples:
            records.append({
                "camera_position": _gta_to_delivery_vector(position),
                "camera_rotation_quaternion": _gta_to_delivery_quaternion(quaternion),
                "camera_intrinsics": {"fx": 960.0, "fy": 960.0, "cx": 960.0, "cy": 540.0},
            })
        (root / f"action_camera_{lane}.json").write_text(
            json.dumps(records), encoding="utf-8")
        (root / f"systeminfo_{lane}.json").write_text(
            json.dumps({"gameProcessName": "GTA5.exe", "x": 0, "y": 0,
                        "width": 1920, "height": 1080, "recordDpi": 96}),
            encoding="utf-8")
    plan = {
        "mode": "mc",
        "duration_s": N_FRAMES / 30.0,
        "fps": 30.0,
        "scene": "test-1",
        "scene_kind": "urban_block",
        "seed": 1,
        "cam4": {"mode": "topdown"},
        "cameras": [{"id": lane, "focal_mm": focal}
                    for lane, focal in zip(range(1, 5), (18, 24, 35, 50))],
        # Anchor track in the delivery basis, on the anchor plane so the
        # placement origin is unambiguous.
        "anchors": [
            {"t": 0.0,
             "p": [anchor_delivery[0], ANCHOR_GTA[2] + AIM_HEIGHT_M, anchor_delivery[2]]},
            {"t": N_FRAMES / 30.0,
             "p": [anchor_delivery[0], ANCHOR_GTA[2] + AIM_HEIGHT_M,
                   anchor_delivery[2] + 1.0]},
        ],
    }
    (root / "plan.json").write_text(json.dumps(plan), encoding="utf-8")


def _clip(tmp: str) -> Path:
    root = Path(tmp)
    _write_delivery_clip(root, _gta_reference_clip())
    return root


#: The fixture is authored in absolute GTA coordinates, so after conversion the
#: clip's delivery ground plane is the anchor's GTA z rather than 0.
CLIP_GROUND_Y = ANCHOR_GTA[2]


def _identity_placement() -> Placement:
    """The placement that undoes ``_write_delivery_clip``'s conversion."""
    return Placement(anchor=ANCHOR_GTA, heading_deg=0.0, ground_y=CLIP_GROUND_Y)


def _forward(quaternion):
    """GTA camera forward from an order-2 quaternion (tape's documented reads)."""
    x, y, z, w = quaternion
    return (2.0 * (x * y - w * z),
            1.0 - 2.0 * (x * x + z * z),
            2.0 * (y * z + w * x))


class PlacementRoundTripTests(unittest.TestCase):
    def test_identity_placement_reproduces_the_authored_gta_poses(self):
        reference = _gta_reference_clip()
        with tempfile.TemporaryDirectory() as tmp:
            root = _clip(tmp)
            _write_delivery_clip(root, reference)
            tape, route = place_trajgen_clip(
                root, placement=_identity_placement(), filmback_id="fb-1")

        self.assertEqual(tape["coordinate_convention"], GTA_COORDINATE_CONVENTION)
        self.assertEqual(tape["source"]["kind"], "trajgen_clip_placed")
        self.assertIs(tape["source"]["delivery_eligible"], False)
        self.assertIs(tape["source"]["collision_clearance_verified"], False)
        self.assertEqual(tape["source"]["cam4_mode"], "topdown")
        for lane, samples in reference.items():
            placed = tape["lanes"][str(lane)]["samples"]
            for frame, (position, quaternion, _) in enumerate(samples):
                for axis in range(3):
                    self.assertAlmostEqual(placed[frame]["position"][axis],
                                           position[axis], places=6)
                for axis in range(4):
                    self.assertAlmostEqual(placed[frame]["quaternion"][axis],
                                           quaternion[axis], places=9)
        self.assertEqual(route["waypoints"][0]["position"], list(ANCHOR_GTA))

    def test_heading_rotates_position_and_orientation_together(self):
        """A camera aimed at the subject must stay aimed at it after placement.

        This is the property a wrong heading sign or a yaw applied to only one of
        position/orientation destroys, while every schema and range check still
        passes and every covisibility claim silently becomes false.
        """
        reference = _gta_reference_clip()
        placement = Placement(anchor=(500.0, -25.0, 31.5), heading_deg=137.0,
                              ground_y=CLIP_GROUND_Y)
        with tempfile.TemporaryDirectory() as tmp:
            root = _clip(tmp)
            _write_delivery_clip(root, reference)
            tape, route = place_trajgen_clip(root, placement=placement, filmback_id="fb-1")

        heading = math.radians(placement.heading_deg)
        for lane, samples in reference.items():
            placed = tape["lanes"][str(lane)]["samples"]
            for frame in (0, N_FRAMES // 2, N_FRAMES - 1):
                _, _, target = samples[frame]
                local = [target[0] - ANCHOR_GTA[0], target[1] - ANCHOR_GTA[1],
                         target[2] - ANCHOR_GTA[2]]
                expected_target = [
                    placement.anchor[0] + local[0] * math.cos(heading)
                    - local[1] * math.sin(heading),
                    placement.anchor[1] + local[0] * math.sin(heading)
                    + local[1] * math.cos(heading),
                    placement.anchor[2] + local[2],
                ]
                position = placed[frame]["position"]
                to_target = [expected_target[axis] - position[axis] for axis in range(3)]
                norm = math.dist(expected_target, position)
                forward = _forward(placed[frame]["quaternion"])
                cosine = sum(a * b for a, b in zip(forward, to_target)) / norm
                self.assertGreater(cosine, 1.0 - 1e-9,
                                   f"lane {lane} frame {frame} stopped looking at the subject")
        self.assertLess(tape["source"]["rigidity_residual_m"], 1e-9)

    def test_placement_preserves_every_inter_camera_distance(self):
        reference = _gta_reference_clip()
        with tempfile.TemporaryDirectory() as tmp:
            root = _clip(tmp)
            _write_delivery_clip(root, reference)
            tape, _ = place_trajgen_clip(
                root, placement=Placement(anchor=(10.0, 20.0, 30.0), heading_deg=42.5,
                                          ground_y=CLIP_GROUND_Y),
                filmback_id="fb-1")
        for frame in (0, N_FRAMES // 2, N_FRAMES - 1):
            for first in range(1, 5):
                for second in range(first + 1, 5):
                    source = math.dist(reference[first][frame][0], reference[second][frame][0])
                    target = math.dist(tape["lanes"][str(first)]["samples"][frame]["position"],
                                       tape["lanes"][str(second)]["samples"][frame]["position"])
                    self.assertAlmostEqual(source, target, places=8)


class StagedPathCompatibilityTests(unittest.TestCase):
    """The operator-gated staged path is the only one that runs on the real machine.

    ``staged._validate_real_inputs`` reads ``tape.source.anchor`` and requires it to
    equal the scenario location.  A placed tape that only recorded the anchor inside
    ``source.placement`` was rejected there, which made the whole adapter unreachable
    on the exact path the capture machine uses — while every unit test still passed.
    """

    def test_placed_tape_exposes_source_anchor_for_the_staged_validator(self):
        placement = Placement(anchor=(500.0, -25.0, 31.5), heading_deg=137.0,
                              ground_y=CLIP_GROUND_Y)
        with tempfile.TemporaryDirectory() as tmp:
            tape, _ = place_trajgen_clip(_clip(tmp), placement=placement,
                                         filmback_id="fb-1")
        anchor = tape["source"]["anchor"]
        self.assertIsInstance(anchor, list)
        self.assertEqual(len(anchor), 3)
        # The staged validator compares against the scenario location with a 1mm
        # tolerance, so the tape must carry the placement anchor verbatim.
        for axis in range(3):
            self.assertAlmostEqual(anchor[axis], placement.anchor[axis], places=9)

    def test_placed_tape_satisfies_the_staged_anchor_predicate(self):
        from gta_capture.tape import GTA_COORDINATE_CONVENTION as convention
        placement = Placement(anchor=(-1180.5, -1510.25, 4.62), heading_deg=0.0,
                              ground_y=CLIP_GROUND_Y)
        with tempfile.TemporaryDirectory() as tmp:
            tape, _ = place_trajgen_clip(_clip(tmp), placement=placement,
                                         filmback_id="fb-1")
        location = list(placement.anchor)
        anchor = tape["source"].get("anchor")
        self.assertEqual(tape["coordinate_convention"], convention)
        self.assertTrue(
            isinstance(anchor, list) and len(anchor) == 3
            and location != [0.0, 0.0, 0.0]
            and max(abs(location[i] - float(anchor[i])) for i in range(3)) <= 0.001,
            "placed tape would be rejected by staged._validate_real_inputs")


class PlacementRejectionTests(unittest.TestCase):
    def test_wrong_ground_y_sinks_the_rig_and_is_rejected(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = _clip(tmp)
            with self.assertRaises(ContractError) as caught:
                place_trajgen_clip(root, filmback_id="fb-1", placement=Placement(
                    anchor=ANCHOR_GTA, heading_deg=0.0, ground_y=CLIP_GROUND_Y + 50.0))
        self.assertIn("ground_y", str(caught.exception))

    def test_excursion_beyond_the_survey_volume_is_rejected(self):
        reference = _gta_reference_clip()
        far = MAX_HORIZONTAL_EXCURSION_M + 50.0
        for frame in range(N_FRAMES):
            position, quaternion, target = reference[1][frame]
            reference[1][frame] = ([position[0] + far, position[1], position[2]],
                                   quaternion, target)
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            _write_delivery_clip(root, reference)
            with self.assertRaises(ContractError) as caught:
                place_trajgen_clip(root, placement=_identity_placement(), filmback_id="fb-1")
        self.assertIn("resurvey", str(caught.exception))

    def test_non_mc_clip_is_rejected(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = _clip(tmp)
            plan = json.loads((root / "plan.json").read_text(encoding="utf-8"))
            plan["mode"] = "sfm"
            (root / "plan.json").write_text(json.dumps(plan), encoding="utf-8")
            with self.assertRaises(ContractError):
                place_trajgen_clip(root, placement=_identity_placement(), filmback_id="fb-1")

    def test_out_of_range_heading_is_rejected(self):
        with self.assertRaises(ContractError):
            Placement(anchor=ANCHOR_GTA, heading_deg=360.0)
        with self.assertRaises(ContractError):
            Placement(anchor=ANCHOR_GTA, heading_deg=float("nan"))
        with self.assertRaises(ContractError):
            Placement(anchor=(1.0, 2.0), heading_deg=0.0)  # type: ignore[arg-type]


if __name__ == "__main__":
    unittest.main()

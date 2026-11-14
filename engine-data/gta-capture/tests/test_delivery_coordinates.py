import json
import math
import tempfile
import unittest
from pathlib import Path

from gta_capture.evidence import (
    DELIVERY_COORDINATE_CONVENTION,
    _delivery_euler_deg,
    _gta_to_delivery_quaternion,
    _gta_to_delivery_vector,
    materialize_metadata,
)


class DeliveryCoordinateTests(unittest.TestCase):
    def test_gta_axes_and_yaw_become_docs69_y_up_left_handed(self):
        self.assertEqual(_gta_to_delivery_vector([1.0, 2.0, 3.0]),
                         [1.0, 3.0, 2.0])

        # GTA order-2 camera: pitch=-6.8428, native roll=0, yaw=-38.
        gta = [-0.056428, 0.01943, -0.324988, 0.943833]
        delivery = _gta_to_delivery_quaternion(gta)
        expected = [0.056428, 0.324988, -0.01943, 0.943833]
        for actual, wanted in zip(delivery, expected):
            self.assertAlmostEqual(actual, wanted, places=5)
        pitch, yaw, roll = _delivery_euler_deg(delivery)
        self.assertAlmostEqual(pitch, -6.8428, places=3)
        self.assertAlmostEqual(yaw, 38.0, places=3)
        self.assertAlmostEqual(roll, 0.0, places=3)

    def test_materialization_transforms_pose_and_derives_player_speed(self):
        gta_quaternion = [-0.056428, 0.01943, -0.324988, 0.943833]

        def event(frame, camera_position, player_position):
            return {
                "source_t_ns": round(frame * 1_000_000_000 / 30),
                "final_camera": {
                    "position": camera_position,
                    "quaternion": gta_quaternion,
                    "euler_deg": [-6.8428, 0.0, -38.0],
                    "intrinsics": {"fx": 960.0, "fy": 960.0,
                                   "cx": 960.0, "cy": 540.0},
                },
                "player_state": {
                    "position": player_position,
                    "quaternion": [0.0, 0.0, 0.0, 1.0],
                    "euler_deg": [0.0, 0.0, 0.0],
                    # Native instantaneous telemetry is deliberately different;
                    # delivery speed must be derived from delivery positions.
                    "velocity": [99.0, 98.0, 97.0],
                },
                "input": {"source": "native_task_go_straight",
                          "mouse_dx": 0.0, "mouse_dy": 0.0, "keyCode": []},
            }

        rows = [
            event(0, [10.0, 20.0, 30.0], [1.0, 2.0, 3.0]),
            event(1, [10.1, 20.2, 30.3], [1.1, 2.2, 3.3]),
        ]
        lanes = {lane: rows for lane in range(1, 5)}
        tape = {
            "coordinate_convention": "gta-native-z-up-right-handed-order2",
            "lanes": {str(lane): {"resolution": {"width": 1920, "height": 1080}}
                      for lane in range(1, 5)},
        }
        with tempfile.TemporaryDirectory() as tmp:
            report = materialize_metadata(
                Path(tmp), lanes, tape,
                source_epoch="2026-08-28 12:00:00.000",
                process_name="GTA5.exe", metric_scale=1.0, dry_run=False,
            )
            action = json.loads((Path(tmp) / "action_camera_1.json").read_text("utf-8"))

        self.assertEqual(report["output_coordinate_convention"],
                         DELIVERY_COORDINATE_CONVENTION)
        self.assertEqual(action[0]["camera_position"], [10.0, 30.0, 20.0])
        self.assertEqual(action[1]["camera_speed"], [3.0, 9.0, 6.0])
        self.assertEqual(action[1]["player_speed"], [3.0, 9.0, 6.0])
        self.assertEqual(action[0]["player_speed"], [0.0, 0.0, 0.0])
        self.assertLess(abs(action[0]["camera_rotation_oula"][2]), 1e-3)
        self.assertTrue(math.isclose(
            sum(value * value for value in action[0]["camera_rotation_quaternion"]),
            1.0, rel_tol=0.0, abs_tol=2e-6,
        ))


if __name__ == "__main__":
    unittest.main()

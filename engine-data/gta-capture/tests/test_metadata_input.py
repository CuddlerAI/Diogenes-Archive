import copy
import json
import math
from pathlib import Path
import sys
import tempfile
import unittest

import helpers  # noqa: F401

from gta_capture.errors import ContractError
from gta_capture.evidence import GTA_NATIVE_COORDINATE_CONVENTION, materialize_metadata


QC_ROOT = Path(__file__).resolve().parents[2] / "qc"
if str(QC_ROOT) not in sys.path:
    sys.path.insert(0, str(QC_ROOT))
from engine_qc.checks_json import check_input_stream
from engine_qc.config import QCConfig


def _fixture():
    """Synthetic native-shaped events; no game or hardware observations."""
    rows = []
    for frame, yaw in enumerate((0.0, 139.0)):
        half_yaw = math.radians(yaw) / 2
        rows.append({
            "source_t_ns": round(frame * 1_000_000_000 / 30),
            "mock": False,
            "final_camera": {
                "position": [0.0, float(frame), 2.0],
                "quaternion": [0.0, 0.0, math.sin(half_yaw), math.cos(half_yaw)],
                "euler_deg": [0.0, 0.0, yaw],
                "intrinsics": {"fx": 960.0, "fy": 960.0, "cx": 960.0, "cy": 540.0},
            },
            "player_state": {
                "position": [0.0, float(frame), 0.0],
                "quaternion": [0.0, 0.0, 0.0, 1.0],
                "euler_deg": [0.0, 0.0, 0.0],
                "velocity": [0.0, 0.0, 0.0],
            },
            "input": {"source": "native_task_go_straight", "mouse_dx": 0.0,
                      "mouse_dy": 0.0, "keyCode": []},
        })
    lanes = {lane: copy.deepcopy(rows) for lane in range(1, 5)}
    tape = {
        "coordinate_convention": GTA_NATIVE_COORDINATE_CONVENTION,
        "lanes": {str(lane): {"resolution": {"width": 1920, "height": 1080}}
                  for lane in range(1, 5)},
    }
    return lanes, tape


class MetadataInputTests(unittest.TestCase):
    @staticmethod
    def _materialize(root, lanes, tape, *, dry_run=False):
        return materialize_metadata(
            root, lanes, tape, source_epoch="2026-09-08 00:00:00.000",
            process_name="GTA5.exe", metric_scale=1.0, dry_run=dry_run)

    def test_missing_input_is_not_reconstructed_from_rotating_camera(self):
        for dry_run, mock in ((False, False), (False, True), (True, True)):
            with self.subTest(dry_run=dry_run, mock=mock), tempfile.TemporaryDirectory() as tmp:
                lanes, tape = _fixture()
                lanes[1][1]["mock"] = mock
                del lanes[1][1]["input"]
                with self.assertRaisesRegex(ContractError, "frame 1: input object"):
                    self._materialize(Path(tmp), lanes, tape, dry_run=dry_run)
                self.assertFalse((Path(tmp) / "action_camera_1.json").exists())

    def test_missing_mouse_axis_is_not_reconstructed(self):
        for key in ("mouse_dx", "mouse_dy"):
            with self.subTest(key=key), tempfile.TemporaryDirectory() as tmp:
                lanes, tape = _fixture()
                del lanes[1][1]["input"][key]
                with self.assertRaisesRegex(ContractError, f"frame 1: input.{key}"):
                    self._materialize(Path(tmp), lanes, tape)
                self.assertFalse((Path(tmp) / "action_camera_1.json").exists())

    def test_invalid_input_container_is_rejected(self):
        for value in (None, [], "unobserved"):
            with self.subTest(value=value), tempfile.TemporaryDirectory() as tmp:
                lanes, tape = _fixture()
                lanes[1][1]["input"] = value
                with self.assertRaisesRegex(ContractError, "input object"):
                    self._materialize(Path(tmp), lanes, tape)

    def test_mouse_values_must_be_finite_numbers(self):
        invalid_values = (math.nan, math.inf, -math.inf, None, "nan", "0", True,
                          10 ** 1000)
        for key in ("mouse_dx", "mouse_dy"):
            for value in invalid_values:
                with self.subTest(key=key, value=value), tempfile.TemporaryDirectory() as tmp:
                    lanes, tape = _fixture()
                    lanes[1][1]["input"][key] = value
                    with self.assertRaisesRegex(ContractError, f"input.{key}.*finite"):
                        self._materialize(Path(tmp), lanes, tape)

    def test_explicit_zero_is_preserved_and_existing_input_qc_still_fails(self):
        lanes, tape = _fixture()
        original = copy.deepcopy(lanes)
        with tempfile.TemporaryDirectory() as tmp:
            report = self._materialize(Path(tmp), lanes, tape)
            action = json.loads((Path(tmp) / "action_camera_1.json").read_text("utf-8"))
        self.assertEqual(lanes, original)
        self.assertEqual([(row["mouse_dx"], row["mouse_dy"]) for row in action],
                         [(0.0, 0.0), (0.0, 0.0)])
        self.assertEqual(report["input_source"], "native_task_go_straight+scripted_camera")
        result = check_input_stream("action_camera_1.json", action, QCConfig(), "mc")
        self.assertEqual(result.status, "FAIL")
        self.assertEqual(result.rule, "num.input_stream")

    def test_explicit_nonzero_input_is_copied_without_camera_derivation(self):
        lanes, tape = _fixture()
        lanes[1][1]["input"].update(mouse_dx=0.125, mouse_dy=-0.25, keyCode=[87])
        with tempfile.TemporaryDirectory() as tmp:
            self._materialize(Path(tmp), lanes, tape)
            action = json.loads((Path(tmp) / "action_camera_1.json").read_text("utf-8"))
        self.assertEqual(action[1]["mouse_dx"], 0.125)
        self.assertEqual(action[1]["mouse_dy"], -0.25)
        self.assertEqual(action[1]["keyCode"], [87])

    def test_explicit_mock_input_remains_labelled_mock(self):
        lanes, tape = _fixture()
        for rows in lanes.values():
            for row in rows:
                row["mock"] = True
                row["input"]["source"] = "scripted_camera"
        with tempfile.TemporaryDirectory() as tmp:
            report = self._materialize(Path(tmp), lanes, tape, dry_run=True)
        self.assertEqual(report["input_source"], "mock_scripted_camera")


if __name__ == "__main__":
    unittest.main()

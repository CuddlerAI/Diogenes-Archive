"""Synthetic contract fixtures only; these tests do not produce capture evidence."""

import copy
import json
from pathlib import Path
import struct
import tempfile
import unittest
import zlib

import helpers  # noqa: F401

from gta_capture.calibration_sampling import (FINAL_FENCE, PROJECTION_NATIVE,
                                             PROJECTION_NATIVE_HASH,
                                             samples_from_native_frames)
from gta_capture.errors import ContractError
from gta_capture.jsonio import sha256_file


def _png(width, height):
    def chunk(kind, value):
        return (struct.pack(">I", len(value)) + kind + value
                + struct.pack(">I", zlib.crc32(kind + value) & 0xffffffff))
    return (b"\x89PNG\r\n\x1a\n"
            + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress((b"\x00" + b"\x00" * width * 3) * height))
            + chunk(b"IEND", b""))


class NativeCalibrationSamplingTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.path = self.root / "synthetic-test-frames.jsonl"
        self.rows = []
        for index, focal in enumerate((18, 24, 35, 50)):
            image = self.root / f"synthetic-test-fixture-lane{index + 1}.png"
            image.write_bytes(_png(192, 108))
            # This is a known pinhole fixture, not a measurement or native output.
            points = []
            fx = 192 * focal / 36.0
            for a, u in enumerate((0.15, 0.325, 0.5, 0.675, 0.85)):
                for b, v in enumerate((0.15, 0.325, 0.5, 0.675, 0.85)):
                    depth = 10.0 + a + b
                    points.append({
                        "id": f"test-{a}-{b}", "visible": True,
                        "world": [(u * 192 - 96) * depth / fx, depth,
                                  (54 - v * 108) * depth / fx],
                        "screen_normalized": [u, v],
                    })
            self.rows.append({
                "schema": "dm-gta-frame/1", "mock": False,
                "lane": index + 1, "output_frame": 0, "replay_frame": 0,
                "replay_sha256": "1" * 64,
                "render_succeeded": True, "capture_protocol": 18,
                "editor_identity_protocol": 4, "capture_fence": FINAL_FENCE,
                "output_path": str(image), "output_bytes": image.stat().st_size,
                "output_sha256": sha256_file(image),
                "pixel_ready_final_camera_generation": index + 1,
                "editor_binding": {"capture_request_id": index + 1,
                                   "final_camera_generation": index + 1,
                                   "method": "replus-present-plan-v2",
                                   "session_id": f"test-session-{index + 1}",
                                   "project_name": "synthetic-test-project",
                                   "clip_count": 1, "clip_index": 0,
                                   "clip_identity": "1" * 16,
                                   "identity_sequence": (index + 1) * 2,
                                   "identity_heartbeat": index + 1,
                                   "render_profile_sha256": "2" * 64,
                                   "normalized_output_root": str(self.root)},
                "final_camera": {"position": [0, 0, 0],
                                 "quaternion": [0, 0, 0, 1], "focal_mm": focal,
                                 "intrinsics": {"fx": -99999, "fy": -99999}},
                "calibration_projection": {
                    "method": PROJECTION_NATIVE, "native_hash": PROJECTION_NATIVE_HASH,
                    "capture_request_id": index + 1,
                    "final_camera_generation": index + 1, "points": points,
                },
            })

    def collect(self, rows=None):
        self.path.write_text("".join(json.dumps(row) + "\n" for row in
                                     (rows if rows is not None else self.rows)), encoding="utf-8")
        return samples_from_native_frames([self.path], game_build="synthetic-test-fixture",
                                          captured_at="2026-09-08T00:00:00Z")

    def test_uses_native_pixels_and_ignores_requested_intrinsics(self):
        document, report = self.collect()
        self.assertEqual(len(document["samples"]), 100)
        self.assertEqual(document["samples"][0]["pixel"], [0.15 * 192, 0.15 * 108])
        self.assertLess(report["solution"]["reprojection"]["error_px"], 1e-10)
        self.assertAlmostEqual(report["solution"]["filmback_mm"], 36.0)
        self.assertEqual(document["approval_status"], "draft")
        self.assertFalse(report["metric_scale_fitted"])
        self.assertIn("pending", report["render_marker_crosscheck"])
        self.assertEqual(document["samples"][0]["provenance"]["frame_events_sha256"],
                         sha256_file(self.path))

    def test_missing_projection_is_not_filled_from_camera_math(self):
        for row in self.rows:
            del row["calibration_projection"]
        with self.assertRaisesRegex(ContractError, ">=100 total"):
            self.collect()

    def test_requires_all_four_focal_groups(self):
        with self.assertRaisesRegex(ContractError, "all four focal groups"):
            self.collect(self.rows[:3])

    def test_request_and_final_generation_must_match_the_png(self):
        for field in ("capture_request_id", "final_camera_generation"):
            with self.subTest(field=field):
                rows = copy.deepcopy(self.rows)
                rows[0]["calibration_projection"][field] += 1
                with self.assertRaisesRegex(ContractError, "generations differ"):
                    self.collect(rows)

    def test_actual_png_hash_is_checked(self):
        self.rows[0]["output_sha256"] = "0" * 64
        with self.assertRaisesRegex(ContractError, "PNG hash/size differs"):
            self.collect()

    def test_duplicate_world_measurement_cannot_inflate_count(self):
        duplicate = copy.deepcopy(self.rows[0]["calibration_projection"]["points"][0])
        duplicate["id"] = "different-id-same-measurement"
        self.rows[0]["calibration_projection"]["points"].append(duplicate)
        with self.assertRaisesRegex(ContractError, "duplicate calibration observation"):
            self.collect()

    def test_own_projected_pixels_are_not_an_accepted_source(self):
        self.rows[0]["calibration_projection"]["method"] = "gta_capture.calibration.project"
        with self.assertRaisesRegex(ContractError, "GTA projection native"):
            self.collect()

    def test_mock_frame_is_rejected(self):
        self.rows[0]["mock"] = True
        with self.assertRaisesRegex(ContractError, "real v18"):
            self.collect()

    def test_partial_protocol_label_without_real_identity_is_rejected(self):
        del self.rows[0]["editor_binding"]["clip_identity"]
        with self.assertRaisesRegex(ContractError, "frame seal failed"):
            self.collect()

    def test_boolean_cannot_claim_a_capture_generation(self):
        self.rows[0]["calibration_projection"]["final_camera_generation"] = True
        with self.assertRaisesRegex(ContractError, "positive integer"):
            self.collect()

    def test_invalid_native_hash_type_fails_with_contract_error(self):
        self.rows[0]["calibration_projection"]["native_hash"] = None
        with self.assertRaisesRegex(ContractError, "GTA projection native"):
            self.collect()

    def test_relative_image_path_is_not_promoted_to_absolute(self):
        self.rows[0]["output_path"] = "synthetic-test-fixture-lane1.png"
        with self.assertRaisesRegex(ContractError, "absolute native PNG path"):
            self.collect()


if __name__ == "__main__":
    unittest.main()

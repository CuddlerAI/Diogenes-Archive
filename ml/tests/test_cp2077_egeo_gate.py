from __future__ import annotations

import json
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory

import numpy as np

from diogenes_wm.eval.cp2077_egeo_gate import (
    DEPTH_FRAMES,
    DEPTH_SIZE,
    EXACT_BUILD_ID,
    EXACT_EXE_SHA256,
    FRAME_SCHEMA,
    METRIC_SCHEMA,
    PRODUCTION_BINDING_AUTHORIZED,
    RAW_SCHEMA,
    RGB_FRAMES,
    SOURCE_DEPTH_SIZE,
    Thresholds,
    _atomic_write,
    _bind_all_depth_files,
    _linearize_depth,
    _pair_populations,
    _validate_raw_profile,
    _validate_inputs,
)


def _fixture() -> tuple[list[dict], list[dict], list[dict], dict]:
    frames: list[dict] = []
    raw: list[dict] = []
    poses: list[dict] = []
    for frame in range(RGB_FRAMES):
        seq, qpc = frame + 1, 10_000 + frame
        identity = {"present_seq": seq, "qpc": qpc}
        even = frame % 2 == 0
        frames.append(
            {
                "schema": FRAME_SCHEMA,
                "session_frame": frame,
                "duplicate": False,
                "rgb": identity.copy(),
                "camera": identity.copy(),
                "player": identity.copy(),
                "depth": (
                    {"ordinal": frame // 2, "file": f"{frame // 2:012}.d32", "identity": identity.copy()}
                    if even
                    else None
                ),
                "projection": (
                    {
                        "proj_a": 0.0,
                        "proj_b": 0.1,
                        "reverse_z": True,
                        "observed_this_frame": True,
                        "physical_source": "same-present-physical",
                        "candidate_admitted": True,
                    }
                    if even
                    else None
                ),
            }
        )
        poses.append(
            {
                "present_seq": seq,
                "session_frame": frame,
                "camera_identity": {"kind": "probe_stream", "session_id": "s", "run_id": "r", "generation": 1},
                "camera_source": "resolved_main_camera_hook",
                "camera_position": [frame * 0.01, 0.0, 0.0],
                "camera_rotation_quaternion": [0.0, 0.0, 0.0, 1.0],
                "camera_intrinsics": {"fx": 1000.0, "fy": 1000.0, "cx": 960.0, "cy": 540.0},
            }
        )
        if even:
            ordinal = frame // 2
            raw.append(
                {
                    "schema": RAW_SCHEMA,
                    "ordinal": ordinal,
                    "file": f"{ordinal:012}.d32",
                    "present_seq": seq,
                    "present_qpc": qpc,
                    "width": SOURCE_DEPTH_SIZE[0],
                    "height": SOURCE_DEPTH_SIZE[1],
                    "row_pitch": SOURCE_DEPTH_SIZE[0] * 4,
                    "pixel_format": "DXGI_FORMAT_D32_FLOAT_LE",
                    "byte_len": SOURCE_DEPTH_SIZE[0] * SOURCE_DEPTH_SIZE[1] * 4,
                    "rgb_pairing": "same_present_seq_gpu_watermark_submitted",
                    "source_candidate_id": "dsv-r6-g1-e1",
                }
            )
    metric = {
        "schema": METRIC_SCHEMA,
        "non_trainable": True,
        "local_only": True,
        "upload_allowed": False,
        "steam_build_id": EXACT_BUILD_ID,
        "executable_sha256": EXACT_EXE_SHA256,
        "cross_check": {"passed": True},
        "adopted_metres_per_world_unit": 1.0,
    }
    return frames, raw, poses, metric


class Cp2077EgeoGateTest(unittest.TestCase):
    def test_egeo_success_never_directly_authorizes_production_binding(self) -> None:
        self.assertFalse(PRODUCTION_BINDING_AUTHORIZED)

    def test_exact_900_450_identity_contract_and_alias_rejection(self) -> None:
        frames, raw, poses, metric = _fixture()
        checked_frames, checked_raw, checked_poses, scale = _validate_inputs(frames, raw, poses, metric)
        self.assertEqual(len(checked_frames), RGB_FRAMES)
        self.assertEqual(len(checked_raw), DEPTH_FRAMES)
        self.assertEqual(len(checked_poses), RGB_FRAMES)
        self.assertEqual(scale, 1.0)
        frames[100]["camera"]["qpc"] += 1
        with self.assertRaisesRegex(ValueError, "identity mismatch"):
            _validate_inputs(frames, raw, poses, metric)

    def test_native_d32s8_profile_uses_semantic_candidate_identity(self) -> None:
        profile = {
            "schema": "dm-local-raw-depth-demo-profile/v1",
            "game_id": "cyberpunk-2077-steam",
            "executable_sha256": EXACT_EXE_SHA256,
            "capture_contract": "pr405_local",
            "duration_ms": 30000,
            "non_trainable": True,
            "upload_allowed": False,
            "deliverable": False,
            "recording_ready": False,
            "selector": {
                "candidate_id": "dsv-r6-g1-e1",
                "width": 1920,
                "height": 1080,
                "sample_count": 1,
                "resource_format": 19,
                "dsv_format": 20,
            },
        }
        self.assertEqual(_validate_raw_profile(profile), "dsv-r6-g1-e1")
        profile["selector"]["width"] = 1280
        profile["selector"]["height"] = 720
        with self.assertRaisesRegex(ValueError, "native 1920x1080"):
            _validate_raw_profile(profile)
        profile["selector"]["width"] = 1920
        profile["selector"]["height"] = 1080
        profile["selector"]["candidate_id"] = "dsv-r26-g1-e1"
        self.assertEqual(_validate_raw_profile(profile), "dsv-r26-g1-e1")
        profile["selector"]["candidate_id"] = "dsv-rx-g1-e1"
        with self.assertRaisesRegex(ValueError, "native 1920x1080"):
            _validate_raw_profile(profile)

    def test_static_translation_rotation_populations_are_independent(self) -> None:
        frames, _, poses, metric = _fixture()
        checked_frames, _, checked_poses, _ = _validate_inputs(frames, _fixture()[1], poses, metric)
        thresholds = Thresholds()
        # First 20 even-frame pairs are static.
        for frame in range(0, 42):
            checked_poses[frame]["camera_position"] = [0.0, 0.0, 0.0]
        # Later poses translate and rotate around Y.
        for frame in range(42, RGB_FRAMES):
            angle = (frame - 42) * 0.01
            checked_poses[frame]["camera_position"] = [(frame - 42) * 0.02, 0.0, 0.0]
            checked_poses[frame]["camera_rotation_quaternion"] = [0.0, np.sin(angle / 2), 0.0, np.cos(angle / 2)]
        populations = _pair_populations(checked_poses, checked_frames, thresholds)
        self.assertGreaterEqual(len(populations["static"]), 5)
        self.assertGreaterEqual(len(populations["translation"]), 5)
        self.assertGreaterEqual(len(populations["rotation"]), 5)

    def test_reverse_z_linearization_matches_spec_and_clear_is_nan(self) -> None:
        with TemporaryDirectory() as directory:
            path = Path(directory) / "000000000000.d32"
            raw = np.full((SOURCE_DEPTH_SIZE[1], SOURCE_DEPTH_SIZE[0]), 0.25, dtype="<f4")
            raw[0, 0] = 0.0
            raw.tofile(path)
            depth = _linearize_depth(path, {"proj_a": 0.0, "proj_b": 0.1}, 1.0)
            self.assertTrue(np.isnan(depth[0, 0]))
            self.assertEqual(float(depth[1, 1]), float(np.float16(0.4)))

    def test_report_publish_is_atomic_and_never_overwrites(self) -> None:
        with TemporaryDirectory() as directory:
            path = Path(directory) / "egeo.json"
            _atomic_write(path, {"passed": False})
            self.assertEqual(json.loads(path.read_text(encoding="utf-8")), {"passed": False})
            with self.assertRaisesRegex(ValueError, "already exists"):
                _atomic_write(path, {"passed": True})
            self.assertEqual(json.loads(path.read_text(encoding="utf-8")), {"passed": False})

    def test_all_indexed_depth_bytes_are_bound_before_sampling(self) -> None:
        with TemporaryDirectory() as directory:
            session = Path(directory).resolve()
            depth = session / "raw-depth"
            depth.mkdir()
            rows = []
            for ordinal in range(3):
                name = f"{ordinal:012}.d32"
                (depth / name).write_bytes(bytes([ordinal + 1]))
                rows.append(
                    {
                        "ordinal": ordinal,
                        "file": name,
                        "present_seq": 10 + ordinal,
                        "present_qpc": 100 + ordinal,
                    }
                )
            bound = _bind_all_depth_files(session, rows)
            self.assertEqual(len(bound), len(rows))
            self.assertEqual(
                {row["ordinal"] for row in bound},
                {0, 1, 2},
            )
            (depth / "000000000001.d32").write_bytes(b"changed")
            self.assertNotEqual(
                bound[1]["sha256"],
                _bind_all_depth_files(session, rows)[1]["sha256"],
            )


if __name__ == "__main__":
    unittest.main()

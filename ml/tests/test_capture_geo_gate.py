from __future__ import annotations

import hashlib
import json
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory

import numpy as np
import zstandard as zstd

from diogenes_wm.eval.capture_geo_gate import (
    _authoring_allows_production,
    _read_depth,
    _stream_identity,
    _unique_by,
    _validate_camera,
    _validate_capture_provenance,
    _validate_depth,
    _validate_depth_camera_alignment,
    _validate_pair_delivery_contract,
    _verified_artifact,
)


VIEW = {
    "shader_stage": "vertex",
    "bind_slot": 12,
    "pass_ordinal": 0,
    "draw_ordinal": 0,
    "buffer_bytes": 752,
    "offset": 0,
    "encoding": "f32_little_endian",
    "layout": "row_major",
    "transpose": False,
    "role": "view",
    "translation": "row3",
}
PROJECTION = {
    "shader_stage": "vertex",
    "bind_slot": 12,
    "pass_ordinal": 0,
    "draw_ordinal": 0,
    "buffer_bytes": 752,
    "offset": 64,
    "encoding": "f32_little_endian",
    "layout": "row_major",
    "transpose": True,
    "role": "projection",
}
BINDING = {
    "binding_id": "render-authoring",
    "binding_version": "authoring-egeo-3",
    "profile_id": "fo4",
    "profile_version": "1",
    "executable_sha256": "sha256:" + "a" * 64,
    "render_width": 1920,
    "render_height": 1080,
}
POSITION_SOURCE = {
    "provider": "fallout4-player-camera-root-exact-build/v1",
    "camera_singleton_relocation": 51232216,
    "camera_root_offset": 32,
    "world_transform_offset": 112,
    "metres_per_game_unit": "0.0142875",
    "axis_mapping": "x_z_y",
}
MANIFEST = {
    "view_metres_per_unit": "0.0142875",
    "selected_view": VIEW,
    "selected_projection": PROJECTION,
    "position_source": POSITION_SOURCE,
}


class AuthoringTrialTests(unittest.TestCase):
    def test_trial_can_never_authorize_a_production_binding(self):
        base = {"schema": "camera-binding-authoring/v1", "requires_e_geo_validation": True, "production_binding_authorized": False}
        self.assertTrue(_authoring_allows_production(base))
        trial = {**base, "schema": "camera-binding-authoring-trial/v1", "trial_reason": "Explicit draw chosen for local geometry testing; original sweep remains ambiguous."}
        self.assertFalse(_authoring_allows_production(trial))
        for bad in [
            {**base, "trial_reason": trial["trial_reason"]},
            {**trial, "trial_reason": " "},
            {**trial, "trial_reason": None},
            {**trial, "production_binding_authorized": True},
            {**trial, "requires_e_geo_validation": False},
            {**trial, "schema": "unknown"},
        ]:
            with self.assertRaises(ValueError):
                _authoring_allows_production(bad)


def _write_json(path: Path, value: object) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value), encoding="utf-8")


def _capture_provenance_fixture(root: Path, *, current: bool = False) -> dict[str, object]:
    session_id = "s"
    run_id = "r"
    generation = 7
    run_root = root / session_id / "runs" / run_id
    depth_root = run_root / "depth-lab"
    ack_root = depth_root / "probe-control" / "acks"
    provenance_root = depth_root / "provenance"
    segment_root = run_root / "segments" / "00000"
    segment_root.mkdir(parents=True)
    provenance_root.mkdir(parents=True)

    video = segment_root / "video.mp4"
    video.write_bytes(b"video")
    frames_path = segment_root / "frames.jsonl.zst"
    frame = {
        "frame": 0,
        "session_frame": 0,
        "run_id": run_id,
        "source_qpc": 11,
        "present_seq": 42,
        "duplicate": False,
    }
    frames_path.write_bytes(
        zstd.ZstdCompressor().compress((json.dumps(frame) + "\n").encode("utf-8"))
    )

    def digest(path: Path) -> str:
        return f"sha256:{hashlib.sha256(path.read_bytes()).hexdigest()}"

    segment_meta_path = segment_root / "meta.json"
    _write_json(
        segment_meta_path,
        {
            "session_id": session_id,
            "run_id": run_id,
            "n_frames": 1,
            "video": {"sha256": digest(video)},
            "frames": {"sha256": digest(frames_path)},
        },
    )
    _write_json(
        segment_root / "sha256.json",
        {
            "video.mp4": digest(video),
            "frames.jsonl.zst": digest(frames_path),
            "meta.json": digest(segment_meta_path),
        },
    )
    _write_json(
        run_root / "run.json",
        {
            "sessionId": session_id,
            "runId": run_id,
            "state": "non_trainable_complete",
            "nonTrainable": True,
            "frameCount": 1,
        },
    )
    depth_index = depth_root / "depth-index.jsonl"
    camera = depth_root / "camera-pose-index.jsonl"
    depth_index.parent.mkdir(parents=True, exist_ok=True)
    depth_index.write_text("{}\n", encoding="utf-8")
    camera.write_text("{}\n", encoding="utf-8")

    binding_snapshot_bytes = json.dumps(BINDING, sort_keys=True).encode("utf-8")
    candidate_sha = f"sha256:{hashlib.sha256(binding_snapshot_bytes).hexdigest()}"
    binding_snapshot = provenance_root / f"camera-binding-{candidate_sha.removeprefix('sha256:')}.json"
    binding_snapshot.write_bytes(binding_snapshot_bytes)
    probe_bytes = b"captured probe"
    probe_sha = f"sha256:{hashlib.sha256(probe_bytes).hexdigest()}"
    probe_snapshot = provenance_root / f"dm_depth_probe-{probe_sha.removeprefix('sha256:')}.dll"
    probe_snapshot.write_bytes(probe_bytes)
    profile_sha = "sha256:" + "b" * 64
    adapter = {
        "description": "GPU",
        "driver_version": "1",
        "luid_high": 2,
        "luid_low": 3,
    }
    identity = {"session_id": session_id, "run_id": run_id, "generation": generation}
    _write_json(
        ack_root / "0002-ready.json",
        {
            "schema": "depth-probe-ack/v2",
            "identity": identity,
            "ack": {
                "type": "ready",
                "actual_binding": {
                    field: BINDING[field]
                    for field in (
                        "binding_id",
                        "binding_version",
                        "profile_id",
                        "profile_version",
                        "executable_sha256",
                    )
                },
                "camera_binding_sha256": candidate_sha,
                "depth_profile_sha256": profile_sha,
                "probe_version": "0.7.4",
                "actual_swapchain_adapter": adapter,
            },
        },
    )
    _write_json(
        ack_root / "0004-flushed.json",
        {
            "schema": "depth-probe-ack/v2",
            "identity": identity,
            "ack": {
                "type": "flushed",
                "summary": {
                    "errors": [],
                    "depth_row_count": 1,
                    "depth_file_count": 1,
                    "camera_pose_row_count": 1,
                    "selected_present": {
                        "selected_frame_count": 1,
                        "distinct_selected_depth_count": 1,
                    },
                },
            },
        },
    )
    _write_json(
        provenance_root / "performance-validation-only-test.json",
        {
            "schema": "dm-depth-performance-validation-only-v1",
            "deliverable": False,
            "certificationWritten": False,
            "key": {
                "probe_dll_sha256": probe_sha,
                "camera_binding_sha256": candidate_sha,
                "depth_profile_sha256": profile_sha,
                "probe_version": "0.7.4",
                "gpu_model": adapter["description"],
                "gpu_driver_version": adapter["driver_version"],
                "gpu_adapter_luid_high": adapter["luid_high"],
                "gpu_adapter_luid_low": adapter["luid_low"],
            },
        },
    )
    if current:
        marker_path = provenance_root / "performance-validation-only-test.json"
        legacy = json.loads(marker_path.read_text(encoding="utf-8"))
        old_key = legacy["key"]
        profile = {
            "schema_version": BINDING["profile_version"],
            "game_id": BINDING["profile_id"],
            "lane": "lane30",
            "executable_sha256": BINDING["executable_sha256"],
            "delivery_width": 1280,
            "delivery_height": 720,
        }
        profile_bytes = json.dumps(profile).encode("utf-8")
        profile_sha = "sha256:" + hashlib.sha256(profile_bytes).hexdigest()
        (provenance_root / f"depth-profile-{profile_sha[7:]}.json").write_bytes(profile_bytes)
        runtime = {
            "schema": "dm-depth-probe-runtime-manifest-v1",
            "runtimeVersion": old_key["probe_version"],
            "dllSha256": probe_sha,
            "d3d11Generation": 1,
            "d3d12Generation": 3,
        }
        runtime_bytes = json.dumps(runtime).encode("utf-8")
        (provenance_root / f"depth-probe-runtime-{hashlib.sha256(runtime_bytes).hexdigest()}.json").write_bytes(runtime_bytes)
        marker = {
            "schema": legacy["schema"], "deliverable": False,
            "certificationCandidateWritten": False, "runtime": runtime,
            "hardware": {
                "gpuModel": adapter["description"],
                "gpuDriverVersion": adapter["driver_version"],
                "gpuAdapterLuidHigh": adapter["luid_high"],
                "gpuAdapterLuidLow": adapter["luid_low"],
            },
            "key": {
                "gameId": profile["game_id"], "rendererBackend": "d3d11",
                "backendGeneration": 1, "cameraBindingSha256": candidate_sha,
                "depthProfileSha256": profile_sha, "width": 1280, "height": 720,
            },
        }
        _write_json(marker_path, marker)
        marker_path.rename(provenance_root / f"performance-validation-only-{digest(marker_path)[7:]}.json")
        for path in ack_root.glob("*.json"):
            ack = json.loads(path.read_text(encoding="utf-8"))
            ack["schema"] = "depth-probe-ack/v3"
            if ack["ack"]["type"] == "ready":
                ack["ack"]["depth_profile_sha256"] = profile_sha
                ack["ack"]["actual_profile"] = {field: profile[field] for field in (
                    "schema_version", "game_id", "lane", "executable_sha256"
                )}
            _write_json(path, ack)
    return {
        "run_root": run_root,
        "video": video,
        "pairs": {
            0: {
                "session_frame": 0,
                "source_qpc": 11,
                "present_seq": 42,
                "duplicate": False,
            }
        },
        "depth_index_path": depth_index,
        "depth_root": depth_root,
        "camera_path": camera,
        "stream_identity": (session_id, run_id, generation),
        "candidate_binding_sha256": candidate_sha,
        "binding": BINDING,
        "probe_dll_path": probe_snapshot,
        "depth_row_count": 1,
        "camera_row_count": 1,
    }


def _camera() -> dict:
    return {
        "schema": "camera-pose-index/v1",
        "source": "resolved_render_camera_hybrid",
        "present_seq": 42,
        "provenance": {
            "kind": "resolved_render_camera_hybrid",
            **BINDING,
            "dsv_id": "main-scene-v1",
            "render_width": 1920,
            "render_height": 1080,
            "game_camera": POSITION_SOURCE,
        "render_basis_source": VIEW,
        "render_basis_matrix": [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 0.0, 1.0],
        ],
            "projection_source": PROJECTION,
            "projection_matrix": [
                [1.2, 0.0, 0.0, 0.0],
                [0.0, 2.0, 0.0, 0.0],
                [0.0, 0.0, 1.0, 1.0],
                [0.0, 0.0, -10.0, 0.0],
            ],
        },
        "identity": {"session_id": "s", "run_id": "r", "generation": 7},
        "camera_position": [1.0, 2.0, 3.0],
        "camera_rotation_quaternion": [0.0, 0.0, 0.0, 1.0],
        "camera_intrinsics": {"fx": 1152.0, "fy": 1080.0, "cx": 960.0, "cy": 540.0},
    }


def _depth() -> dict:
    return {
        "schema": "depth-index/v6",
        "present_seq": 42,
        "width": 2,
        "height": 2,
        "buffer_format": "d32_float",
        "dsv_id": "main-scene-v1",
        "storage": {
            "encoding": "exr-half-zip",
            "domain": "raw_buffer",
            "channel": "Z",
            "transform": "one_minus_d",
        },
        "path": "raw/000000000042.exr",
        "projection": {
            "proj_a": 1.0,
            "proj_b": -9.9,
            "reverse_z": False,
            "metric_scale": 0.0142875,
            "viewport_min_depth": 0.01,
            "viewport_max_depth": 1.0,
            "proj_source": "constant_buffer",
            "proj_scope": "observed_this_frame",
        },
    }


class CaptureGeoGateTests(unittest.TestCase):
    def test_current_capture_provenance_binds_runtime_profile_and_hardware(self) -> None:
        with TemporaryDirectory() as directory:
            fixture = _capture_provenance_fixture(Path(directory), current=True)
            evidence = _validate_capture_provenance(**fixture)
            self.assertEqual(evidence["probe_ack_schema"], "depth-probe-ack/v3")
            self.assertIn("runtime_manifest_snapshot_sha256", evidence)
            self.assertIn("depth_profile_snapshot_sha256", evidence)

    def test_current_capture_rejects_marker_identity_tampering(self) -> None:
        cases = [
            (("deliverable",), True, "non-deliverable"),
            (("certificationCandidateWritten",), True, "non-deliverable"),
            (("runtime", "dllSha256"), "sha256:" + "0" * 64, "runtime manifest snapshot mismatch"),
            (("runtime", "runtimeVersion"), "different", "runtime manifest snapshot mismatch"),
            (("hardware", "gpuModel"), "Other GPU", "GPU identity differs"),
            (("hardware", "gpuAdapterLuidLow"), 4, "GPU identity differs"),
            (("key", "backendGeneration"), 2, "backend generation mismatch"),
            (("key", "rendererBackend"), "d3d12", "backend generation mismatch"),
            (("key", "cameraBindingSha256"), "sha256:" + "0" * 64, "CameraBinding hash mismatch"),
            (("key", "gameId"), "different", "differs from depth profile"),
            (("key", "width"), 1920, "differs from depth profile"),
        ]
        for fields, value, error in cases:
            with self.subTest(fields=fields), TemporaryDirectory() as directory:
                fixture = _capture_provenance_fixture(Path(directory), current=True)
                root = fixture["depth_root"] / "provenance"
                path = next(root.glob("performance-validation-only-*.json"))
                marker = json.loads(path.read_text(encoding="utf-8"))
                target = marker
                for field in fields[:-1]:
                    target = target[field]
                target[fields[-1]] = value
                _write_json(path, marker)
                digest = hashlib.sha256(path.read_bytes()).hexdigest()
                path.rename(root / f"performance-validation-only-{digest}.json")
                with self.assertRaisesRegex(ValueError, error):
                    _validate_capture_provenance(**fixture)

    def test_current_capture_rejects_missing_tampered_or_duplicate_snapshots(self) -> None:
        for action, error in (("missing", "exactly one runtime"), ("duplicate", "exactly one runtime"),
                              ("runtime", "runtime manifest snapshot mismatch"),
                              ("profile", "depth profile snapshot hash mismatch"),
                              ("marker", "validation marker snapshot hash mismatch")):
            with self.subTest(action=action), TemporaryDirectory() as directory:
                fixture = _capture_provenance_fixture(Path(directory), current=True)
                root = fixture["depth_root"] / "provenance"
                path = next(root.glob("depth-probe-runtime-*.json"))
                if action == "missing":
                    path.unlink()
                elif action == "duplicate":
                    (root / "depth-probe-runtime-other.json").write_bytes(path.read_bytes())
                else:
                    if action == "profile":
                        path = next(root.glob("depth-profile-*.json"))
                    elif action == "marker":
                        path = next(root.glob("performance-validation-only-*.json"))
                    path.write_bytes(path.read_bytes() + b" ")
                with self.assertRaisesRegex(ValueError, error):
                    _validate_capture_provenance(**fixture)

    def test_capture_rejects_mixed_or_unknown_ack_schemas(self) -> None:
        for schema in ("depth-probe-ack/v2", "depth-probe-ack/v99"):
            with self.subTest(schema=schema), TemporaryDirectory() as directory:
                fixture = _capture_provenance_fixture(Path(directory), current=True)
                path = fixture["depth_root"] / "probe-control/acks/0004-flushed.json"
                ack = json.loads(path.read_text(encoding="utf-8"))
                ack["schema"] = schema
                _write_json(path, ack)
                with self.assertRaisesRegex(ValueError, "same supported"):
                    _validate_capture_provenance(**fixture)

    def test_fallout_build_guard_is_additional_not_replacement_provenance(self) -> None:
        with TemporaryDirectory() as directory:
            fixture = _capture_provenance_fixture(Path(directory), current=True)
            root = fixture["depth_root"] / "provenance"
            path = root / "performance-validation-only-fallout4-authoring.json"
            marker = {
                "schema": "dm-depth-performance-validation-only-v1", "deliverable": False,
                "certificationCandidateWritten": False,
                "reason": "unverified-fallout4-build-authoring",
                "executableSha256": BINDING["executable_sha256"], "gameVersion": "1.11.240.0",
            }
            _write_json(path, marker)
            self.assertIn("fallout_authoring_marker_sha256", _validate_capture_provenance(**fixture))
            marker["executableSha256"] = "sha256:" + "f" * 64
            _write_json(path, marker)
            with self.assertRaisesRegex(ValueError, "captured build guard"):
                _validate_capture_provenance(**fixture)
            marker["executableSha256"] = BINDING["executable_sha256"]
            _write_json(path, marker)
            for other in root.glob("performance-validation-only-*.json"):
                if other != path:
                    other.unlink()
            with self.assertRaisesRegex(ValueError, "exactly one performance-validation-only"):
                _validate_capture_provenance(**fixture)

    def test_authoring_artifacts_are_hash_bound(self) -> None:
        with TemporaryDirectory() as directory:
            path = Path(directory) / "artifact.json"
            path.write_bytes(b"{}")
            artifact = {
                "path": str(path),
                "sha256": f"sha256:{hashlib.sha256(b'{}').hexdigest()}",
            }
            self.assertEqual(_verified_artifact(artifact, "test"), path.resolve())
            artifact["sha256"] = "sha256:" + "0" * 64
            with self.assertRaisesRegex(ValueError, "hash mismatch"):
                _verified_artifact(artifact, "test")

    def test_camera_identity_is_exact_and_metric(self) -> None:
        _validate_camera(_camera(), BINDING, MANIFEST)
        wrong_manifest = dict(MANIFEST)
        wrong_manifest["view_metres_per_unit"] = "1"
        with self.assertRaisesRegex(ValueError, "metric scale mismatch"):
            _validate_camera(_camera(), BINDING, wrong_manifest)

    def test_camera_projection_intrinsics_and_render_geometry_are_bound(self) -> None:
        row = _camera()
        row["camera_intrinsics"]["fx"] += 1.0
        with self.assertRaisesRegex(ValueError, "does not match the observed projection"):
            _validate_camera(row, BINDING, MANIFEST)

        row = _camera()
        row["provenance"]["render_width"] = 1280
        with self.assertRaisesRegex(ValueError, "render geometry mismatch"):
            _validate_camera(row, BINDING, MANIFEST)

        row = _camera()
        row["provenance"]["dsv_id"] = ""
        with self.assertRaisesRegex(ValueError, "DSV identity is missing"):
            _validate_camera(row, BINDING, MANIFEST)

    def test_camera_quaternion_must_invert_world_to_camera_render_basis(self) -> None:
        row = _camera()
        row["provenance"]["render_basis_matrix"] = [
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [-1.0, 0.0, 0.0, 0.0],
            [0.0, 0.0, 0.0, 1.0],
        ]
        root_half = 0.5**0.5
        row["camera_rotation_quaternion"] = [0.0, -root_half, 0.0, root_half]
        _validate_camera(row, BINDING, MANIFEST)
        row["camera_rotation_quaternion"] = [0.0, root_half, 0.0, root_half]
        with self.assertRaisesRegex(ValueError, "camera-to-world transpose"):
            _validate_camera(row, BINDING, MANIFEST)

    def test_depth_projection_must_be_per_frame_and_share_scale(self) -> None:
        row = _depth()
        _validate_depth(row, "0.0142875")
        row["projection"]["proj_scope"] = "held_previous"
        with self.assertRaisesRegex(ValueError, "not per-frame observed"):
            _validate_depth(row, "0.0142875")

    def test_depth_storage_and_projection_metadata_are_strict(self) -> None:
        row = _depth()
        del row["storage"]["transform"]
        with self.assertRaisesRegex(ValueError, "storage contract differs"):
            _validate_depth(row, "0.0142875")

        row = _depth()
        row["projection"]["reverse_z"] = True
        with self.assertRaisesRegex(ValueError, "reverse-Z metadata is inconsistent"):
            _validate_depth(row, "0.0142875")

    def test_depth_camera_projection_and_dsv_must_align(self) -> None:
        pair = {"depth": {"present_seq": 42, "dsv_id": "main-scene-v1"}}
        _validate_depth_camera_alignment(pair, _depth(), _camera())
        pair["depth"]["dsv_id"] = "other"
        with self.assertRaisesRegex(ValueError, "DSV differs"):
            _validate_depth_camera_alignment(pair, _depth(), _camera())

        pair["depth"]["dsv_id"] = "main-scene-v1"
        depth = _depth()
        depth["projection"]["proj_b"] = -8.0
        with self.assertRaisesRegex(ValueError, "A/B does not match"):
            _validate_depth_camera_alignment(pair, depth, _camera())

    def test_every_even_pair_must_keep_its_exact_depth_identity(self) -> None:
        pairs = {
            0: {
                "session_frame": 0,
                "present_seq": 42,
                "depth": {"present_seq": 42, "dsv_id": "main-scene-v1"},
            },
            1: {
                "session_frame": 1,
                "present_seq": 43,
                "depth_absent_reason": "odd_frame_by_spec",
            },
        }
        _validate_pair_delivery_contract(
            pairs=pairs,
            depths={42: _depth()},
            cameras={42: _camera()},
            binding=BINDING,
            manifest=MANIFEST,
            stream_identity=("s", "r", 7),
        )
        pairs[0].pop("depth")
        pairs[0]["depth_absent_reason"] = "missing_probe_frame"
        with self.assertRaisesRegex(ValueError, "even frame depth is missing"):
            _validate_pair_delivery_contract(
                pairs=pairs,
                depths={42: _depth()},
                cameras={42: _camera()},
                binding=BINDING,
                manifest=MANIFEST,
                stream_identity=("s", "r", 7),
            )

    def test_odd_pair_cannot_smuggle_a_depth_or_another_absence_reason(self) -> None:
        pairs = {
            1: {
                "session_frame": 1,
                "present_seq": 43,
                "depth_absent_reason": "missing_probe_frame",
            }
        }
        with self.assertRaisesRegex(ValueError, "odd frame depth absence contract differs"):
            _validate_pair_delivery_contract(
                pairs=pairs,
                depths={},
                cameras={},
                binding=BINDING,
                manifest=MANIFEST,
                stream_identity=("s", "r", 7),
            )

    def test_stream_identity_requires_all_fields_and_nonnegative_generation(self) -> None:
        self.assertEqual(_stream_identity(_camera(), "camera"), ("s", "r", 7))
        row = _camera()
        row["identity"]["generation"] = -1
        with self.assertRaisesRegex(ValueError, "invalid generation"):
            _stream_identity(row, "camera")

    def test_join_keys_are_unique_and_monotonic(self) -> None:
        self.assertEqual(
            list(_unique_by([{"present_seq": 7}, {"present_seq": 8}], "present_seq", "x")),
            [7, 8],
        )
        with self.assertRaisesRegex(ValueError, "moves backwards"):
            _unique_by([{"present_seq": 8}, {"present_seq": 7}], "present_seq", "x")
        with self.assertRaisesRegex(ValueError, "duplicate"):
            _unique_by([{"present_seq": 7}, {"present_seq": 7}], "present_seq", "x")

    def test_named_z_channel_is_read_instead_of_opencv_zero_fill(self) -> None:
        try:
            import OpenEXR
        except ImportError as error:  # The data extra makes this mandatory in CI and the gate.
            self.fail(f"OpenEXR data dependency is missing: {error}")
        stored = np.array([[0.1, 0.2], [0.25, 0.5]], dtype=np.float16)
        with TemporaryDirectory() as directory:
            raw = Path(directory) / "raw"
            raw.mkdir()
            path = raw / "000000000042.exr"
            OpenEXR.File(
                {
                    "type": OpenEXR.scanlineimage,
                    "compression": OpenEXR.ZIP_COMPRESSION,
                    "name": "depth",
                },
                {"Z": stored},
            ).write(str(path))
            row = _depth()
            row["projection"]["proj_a"] = 1.0
            row["projection"]["proj_b"] = -1.0
            row["projection"]["metric_scale"] = 1.0

            metres = _read_depth(Path(directory), row)

        np.testing.assert_allclose(metres, 1.0 / stored.astype(np.float64), rtol=1e-6)

    def test_depth_path_cannot_escape_raw_directory(self) -> None:
        row = _depth()
        row["path"] = "../000000000042.exr"
        with self.assertRaisesRegex(ValueError, "not canonical"):
            _read_depth(Path("."), row)

    def test_capture_provenance_binds_run_acks_projection_and_probe(self) -> None:
        with TemporaryDirectory() as directory:
            fixture = _capture_provenance_fixture(Path(directory))
            evidence = _validate_capture_provenance(**fixture)

        self.assertEqual(
            evidence["probe_snapshot_sha256"],
            f"sha256:{hashlib.sha256(b'captured probe').hexdigest()}",
        )
        self.assertIn("ready_ack_sha256", evidence)
        self.assertIn("frames_projection_sha256", evidence)

    def test_capture_provenance_rejects_unrelated_probe_file(self) -> None:
        with TemporaryDirectory() as directory:
            fixture = _capture_provenance_fixture(Path(directory))
            unrelated = Path(directory) / "dm-depth-inject.exe"
            unrelated.write_bytes(b"not the captured probe")
            fixture["probe_dll_path"] = unrelated
            with self.assertRaisesRegex(ValueError, "probe DLL hash differs"):
                _validate_capture_provenance(**fixture)

    def test_capture_provenance_rejects_cross_run_frame_pairs(self) -> None:
        with TemporaryDirectory() as directory:
            fixture = _capture_provenance_fixture(Path(directory))
            fixture["pairs"][0]["present_seq"] = 99
            with self.assertRaisesRegex(ValueError, "differs from recorder projection"):
                _validate_capture_provenance(**fixture)

    def test_capture_provenance_rejects_ack_generation_mismatch(self) -> None:
        with TemporaryDirectory() as directory:
            fixture = _capture_provenance_fixture(Path(directory))
            ready_path = (
                fixture["run_root"]
                / "depth-lab"
                / "probe-control"
                / "acks"
                / "0002-ready.json"
            )
            ready = json.loads(ready_path.read_text(encoding="utf-8"))
            ready["identity"]["generation"] += 1
            _write_json(ready_path, ready)
            with self.assertRaisesRegex(ValueError, "Ready ACK belongs to another"):
                _validate_capture_provenance(**fixture)


if __name__ == "__main__":
    unittest.main()

"""Strict E-geo gate for one DataMiner render-camera authoring capture.

The join key is the recorder-confirmed full ``present_seq``.  Video frame proximity is used only
to choose a temporal pair; it is never used to substitute a camera or depth row.
"""
from __future__ import annotations

import argparse
import hashlib
import importlib.metadata
import io
import json
import math
from dataclasses import asdict, dataclass
from pathlib import Path, PurePosixPath
from typing import Any, Iterable

import cv2
import numpy as np
import zstandard as zstd

from diogenes_wm.eval.geo_probes import Pinhole, RelPose, e_geo, quat_to_mat


SCHEMA = "camera-egeo-validation/v4"


def _read_jsonl(path: Path) -> list[dict[str, Any]]:
    rows: list[dict[str, Any]] = []
    with path.open("r", encoding="utf-8") as handle:
        for line_no, line in enumerate(handle, 1):
            if not line.strip():
                continue
            try:
                rows.append(json.loads(line))
            except json.JSONDecodeError as error:
                raise ValueError(f"{path}:{line_no}: invalid JSON: {error}") from error
    return rows


def _read_zstd_jsonl(path: Path) -> list[dict[str, Any]]:
    rows: list[dict[str, Any]] = []
    with path.open("rb") as compressed:
        with zstd.ZstdDecompressor().stream_reader(compressed) as reader:
            with io.TextIOWrapper(reader, encoding="utf-8") as handle:
                for line_no, line in enumerate(handle, 1):
                    if not line.strip():
                        continue
                    try:
                        rows.append(json.loads(line))
                    except json.JSONDecodeError as error:
                        raise ValueError(f"{path}:{line_no}: invalid JSON: {error}") from error
    return rows


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return f"sha256:{digest.hexdigest()}"


def _json_object(path: Path, label: str) -> dict[str, Any]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise ValueError(f"{label} is not valid JSON: {path}: {error}") from error
    if not isinstance(value, dict):
        raise ValueError(f"{label} root is not an object: {path}")
    return value


def _exact_file(path: Path, expected: Path, label: str) -> Path:
    resolved = path.resolve(strict=True)
    expected_resolved = expected.resolve(strict=True)
    if not resolved.is_file() or resolved != expected_resolved:
        raise ValueError(f"{label} is not the canonical run artifact")
    return resolved


def _exact_directory(path: Path, expected: Path, label: str) -> Path:
    resolved = path.resolve(strict=True)
    expected_resolved = expected.resolve(strict=True)
    if not resolved.is_dir() or resolved != expected_resolved:
        raise ValueError(f"{label} is not the canonical run directory")
    return resolved


def _ack_identity(ack: dict[str, Any], label: str) -> tuple[str, str, int]:
    identity = ack.get("identity")
    if not isinstance(identity, dict):
        raise ValueError(f"{label} has no stream identity")
    return _stream_identity({"identity": identity}, label)


def _unique_ack(run_root: Path, ack_type: str) -> tuple[Path, dict[str, Any]]:
    ack_root = run_root / "depth-lab" / "probe-control" / "acks"
    matches: list[tuple[Path, dict[str, Any]]] = []
    for path in sorted(ack_root.glob("*.json")):
        value = _json_object(path, f"probe {ack_type} ACK")
        ack = value.get("ack")
        if isinstance(ack, dict) and ack.get("type") == ack_type:
            matches.append((path.resolve(strict=True), value))
    if len(matches) != 1:
        raise ValueError(f"run must contain exactly one probe {ack_type} ACK, got {len(matches)}")
    return matches[0]


def _validate_pair_projection(
    pairs: dict[int, dict[str, Any]], frames_path: Path, expected_run_id: str
) -> list[dict[str, Any]]:
    frames = _read_zstd_jsonl(frames_path)
    if len(frames) != len(pairs):
        raise ValueError(
            f"frame-pairs count {len(pairs)} differs from recorder projection {len(frames)}"
        )
    for index, frame in enumerate(frames):
        pair = pairs.get(index)
        if pair is None:
            raise ValueError(f"recorder projection session_frame={index} has no frame-pair row")
        if frame.get("frame") != index or frame.get("session_frame") != index:
            raise ValueError(f"recorder projection row {index} has an invalid frame identity")
        if frame.get("run_id") != expected_run_id:
            raise ValueError(f"recorder projection row {index} belongs to another run")
        for field in ("session_frame", "source_qpc", "present_seq", "duplicate"):
            if pair.get(field) != frame.get(field):
                raise ValueError(
                    f"session_frame={index}: frame-pair {field} differs from recorder projection"
                )
    return frames


def _validate_capture_provenance(
    *,
    run_root: Path,
    video: Path,
    pairs: dict[int, dict[str, Any]],
    depth_index_path: Path,
    depth_root: Path,
    camera_path: Path,
    stream_identity: tuple[str, str, int],
    candidate_binding_sha256: str,
    binding: dict[str, Any],
    probe_dll_path: Path,
    depth_row_count: int,
    camera_row_count: int,
) -> dict[str, Any]:
    run_root = run_root.resolve(strict=True)
    if not run_root.is_dir():
        raise ValueError("capture run root is not a directory")
    session_id, run_id, _generation = stream_identity
    if run_root.name != run_id or run_root.parent.name != "runs" or run_root.parent.parent.name != session_id:
        raise ValueError("capture run root path does not match the camera stream identity")

    depth_lab_root = run_root / "depth-lab"
    _exact_directory(depth_root, depth_lab_root, "depth root")
    _exact_file(depth_index_path, depth_lab_root / "depth-index.jsonl", "depth index")
    _exact_file(camera_path, depth_lab_root / "camera-pose-index.jsonl", "camera pose index")

    video = video.resolve(strict=True)
    segment_root = video.parent
    if video.name != "video.mp4" or segment_root.parent != run_root / "segments":
        raise ValueError("video is not a canonical segment artifact under the capture run root")
    segment_meta_path = segment_root / "meta.json"
    segment_hashes_path = segment_root / "sha256.json"
    frames_path = segment_root / "frames.jsonl.zst"
    segment_meta = _json_object(segment_meta_path, "segment metadata")
    segment_hashes = _json_object(segment_hashes_path, "segment hash manifest")
    if segment_meta.get("session_id") != session_id or segment_meta.get("run_id") != run_id:
        raise ValueError("segment metadata belongs to another stream identity")
    for name, path in (
        ("video.mp4", video),
        ("frames.jsonl.zst", frames_path),
        ("meta.json", segment_meta_path),
    ):
        if segment_hashes.get(name) != _sha256(path):
            raise ValueError(f"segment hash manifest does not bind {name}")
    if segment_meta.get("video", {}).get("sha256") != _sha256(video):
        raise ValueError("segment metadata video hash mismatch")
    if segment_meta.get("frames", {}).get("sha256") != _sha256(frames_path):
        raise ValueError("segment metadata frame projection hash mismatch")
    frames = _validate_pair_projection(pairs, frames_path, run_id)
    if segment_meta.get("n_frames") != len(frames):
        raise ValueError("segment metadata frame count mismatch")

    run_json_path = run_root / "run.json"
    run_json = _json_object(run_json_path, "run metadata")
    if run_json.get("sessionId") != session_id or run_json.get("runId") != run_id:
        raise ValueError("run metadata belongs to another stream identity")
    if run_json.get("state") != "non_trainable_complete" or not run_json.get("nonTrainable"):
        raise ValueError("E-geo capture is not a completed non-trainable run")
    if run_json.get("frameCount") != len(pairs):
        raise ValueError("run metadata frame count mismatch")

    ready_path, ready = _unique_ack(run_root, "ready")
    flushed_path, flushed = _unique_ack(run_root, "flushed")
    ack_schema = ready.get("schema")
    if ack_schema not in ("depth-probe-ack/v2", "depth-probe-ack/v3") or flushed.get("schema") != ack_schema:
        raise ValueError("probe ACK schemas must be the same supported v2 or v3 schema")
    if _ack_identity(ready, "probe Ready ACK") != stream_identity:
        raise ValueError("probe Ready ACK belongs to another stream identity")
    if _ack_identity(flushed, "probe Flushed ACK") != stream_identity:
        raise ValueError("probe Flushed ACK belongs to another stream identity")
    ready_payload = ready["ack"]
    actual_binding = ready_payload.get("actual_binding")
    if not isinstance(actual_binding, dict):
        raise ValueError("probe Ready ACK has no actual CameraBinding identity")
    for field in (
        "binding_id",
        "binding_version",
        "profile_id",
        "profile_version",
        "executable_sha256",
    ):
        if actual_binding.get(field) != binding.get(field):
            raise ValueError(f"probe Ready ACK CameraBinding {field} mismatch")
    if ready_payload.get("camera_binding_sha256") != candidate_binding_sha256:
        raise ValueError("probe Ready ACK CameraBinding hash mismatch")

    summary = flushed["ack"].get("summary")
    if not isinstance(summary, dict) or summary.get("errors") != []:
        raise ValueError("probe Flushed ACK is missing or reports errors")
    if summary.get("depth_row_count") != depth_row_count:
        raise ValueError("probe Flushed ACK depth row count mismatch")
    if summary.get("depth_file_count") != depth_row_count:
        raise ValueError("probe Flushed ACK depth file count mismatch")
    if summary.get("camera_pose_row_count") != camera_row_count:
        raise ValueError("probe Flushed ACK camera row count mismatch")
    selected = summary.get("selected_present")
    if not isinstance(selected, dict) or selected.get("selected_frame_count") != len(pairs):
        raise ValueError("probe Flushed ACK selected frame count mismatch")
    if selected.get("distinct_selected_depth_count") != depth_row_count:
        raise ValueError("probe Flushed ACK selected depth count mismatch")

    provenance_root = depth_lab_root / "provenance"
    marker_paths = sorted(provenance_root.glob("performance-validation-only-*.json"))
    extra_provenance: dict[str, Any] = {"probe_ack_schema": ack_schema}
    authoring_marker = provenance_root / "performance-validation-only-fallout4-authoring.json"
    if authoring_marker in marker_paths:
        # The probe's durable build guard is additional evidence, not a replacement for the
        # client's complete runtime/hardware marker. Never ignore arbitrary extra markers.
        value = _json_object(authoring_marker, "Fallout authoring marker")
        if ack_schema != "depth-probe-ack/v3" or value != {
            "schema": "dm-depth-performance-validation-only-v1",
            "deliverable": False,
            "certificationCandidateWritten": False,
            "reason": "unverified-fallout4-build-authoring",
            "executableSha256": binding.get("executable_sha256"),
            "gameVersion": "1.11.240.0",
        } or value.get("deliverable") is not False or value.get("certificationCandidateWritten") is not False:
            raise ValueError("Fallout authoring marker differs from the captured build guard")
        marker_paths.remove(authoring_marker)
        extra_provenance.update({
            "fallout_authoring_marker": str(authoring_marker),
            "fallout_authoring_marker_sha256": _sha256(authoring_marker),
        })
    if len(marker_paths) != 1:
        raise ValueError(
            "capture run must contain exactly one performance-validation-only provenance marker"
        )
    marker_path = marker_paths[0].resolve(strict=True)
    marker = _json_object(marker_path, "performance-validation-only provenance marker")
    certification_field = (
        "certificationWritten" if ack_schema == "depth-probe-ack/v2" else "certificationCandidateWritten"
    )
    if (
        marker.get("schema") != "dm-depth-performance-validation-only-v1"
        or marker.get("deliverable") is not False
        or marker.get(certification_field) is not False
        or ("certificationWritten" in marker and marker["certificationWritten"] is not False)
        or ("certificationCandidateWritten" in marker and marker["certificationCandidateWritten"] is not False)
    ):
        raise ValueError("capture provenance marker is not a non-deliverable validation run")
    key = marker.get("key")
    if not isinstance(key, dict):
        raise ValueError("capture provenance marker has no performance key")
    if ack_schema == "depth-probe-ack/v3":
        key, runtime_evidence = _current_runtime_provenance(marker, marker_path, provenance_root, ready_payload, binding)
        extra_provenance.update(runtime_evidence)
    probe_sha256 = _sha256(probe_dll_path.resolve(strict=True))
    if key.get("probe_dll_sha256") != probe_sha256:
        raise ValueError("captured probe DLL hash differs from the run provenance marker")
    if key.get("camera_binding_sha256") != candidate_binding_sha256:
        raise ValueError("run provenance CameraBinding hash mismatch")
    if key.get("depth_profile_sha256") != ready_payload.get("depth_profile_sha256"):
        raise ValueError("run provenance depth profile hash mismatch")
    if key.get("probe_version") != ready_payload.get("probe_version"):
        raise ValueError("run provenance probe version mismatch")
    adapter = ready_payload.get("actual_swapchain_adapter")
    if not isinstance(adapter, dict) or (
        key.get("gpu_model") != adapter.get("description")
        or key.get("gpu_driver_version") != adapter.get("driver_version")
        or key.get("gpu_adapter_luid_high") != adapter.get("luid_high")
        or key.get("gpu_adapter_luid_low") != adapter.get("luid_low")
    ):
        raise ValueError("run provenance GPU identity differs from probe Ready ACK")
    probe_digest = probe_sha256.removeprefix("sha256:")
    probe_snapshot = provenance_root / f"dm_depth_probe-{probe_digest}.dll"
    if _sha256(probe_snapshot.resolve(strict=True)) != probe_sha256:
        raise ValueError("run provenance probe DLL snapshot hash mismatch")
    binding_snapshot = provenance_root / f"camera-binding-{candidate_binding_sha256.removeprefix('sha256:')}.json"
    if _sha256(binding_snapshot.resolve(strict=True)) != candidate_binding_sha256:
        raise ValueError("run provenance CameraBinding snapshot hash mismatch")

    return {
        **extra_provenance,
        "run_root": str(run_root),
        "run_json": str(run_json_path),
        "run_json_sha256": _sha256(run_json_path),
        "segment_meta": str(segment_meta_path),
        "segment_meta_sha256": _sha256(segment_meta_path),
        "segment_hash_manifest": str(segment_hashes_path),
        "segment_hash_manifest_sha256": _sha256(segment_hashes_path),
        "frames_projection": str(frames_path),
        "frames_projection_sha256": _sha256(frames_path),
        "ready_ack": str(ready_path),
        "ready_ack_sha256": _sha256(ready_path),
        "flushed_ack": str(flushed_path),
        "flushed_ack_sha256": _sha256(flushed_path),
        "validation_marker": str(marker_path),
        "validation_marker_sha256": _sha256(marker_path),
        "probe_snapshot": str(probe_snapshot),
        "probe_snapshot_sha256": _sha256(probe_snapshot),
        "camera_binding_snapshot": str(binding_snapshot),
        "camera_binding_snapshot_sha256": _sha256(binding_snapshot),
    }


def _current_runtime_provenance(
    marker: dict[str, Any], marker_path: Path, root: Path,
    ready: dict[str, Any], binding: dict[str, Any],
) -> tuple[dict[str, Any], dict[str, Any]]:
    """Decode current producer identity, retaining the old exact-DLL/GPU checks below.

    Backend generation alone is not capture provenance or production authorization: require
    the actual runtime bytes, profile bytes and Ready identity to agree independently.
    """
    expected_name = f"performance-validation-only-{_sha256(marker_path).removeprefix('sha256:')}.json"
    if marker_path.name != expected_name:
        raise ValueError("validation marker snapshot hash mismatch")
    runtime = marker.get("runtime")
    hardware = marker.get("hardware")
    key = marker["key"]
    runtime_fields = {"schema", "runtimeVersion", "dllSha256", "d3d11Generation", "d3d12Generation"}
    if not isinstance(runtime, dict) or set(runtime) != runtime_fields or (
        runtime["schema"] != "dm-depth-probe-runtime-manifest-v1"
        or not isinstance(runtime["runtimeVersion"], str) or not runtime["runtimeVersion"].strip()
        or any(type(runtime[field]) is not int or runtime[field] <= 0 for field in ("d3d11Generation", "d3d12Generation"))
    ):
        raise ValueError("run provenance runtime manifest is invalid")
    runtime_paths = sorted(root.glob("depth-probe-runtime-*.json"))
    if len(runtime_paths) != 1:
        raise ValueError("run must contain exactly one runtime manifest snapshot")
    runtime_path = runtime_paths[0]
    if runtime_path.name != f"depth-probe-runtime-{_sha256(runtime_path).removeprefix('sha256:')}.json" or _json_object(runtime_path, "runtime snapshot") != runtime:
        raise ValueError("run provenance runtime manifest snapshot mismatch")
    if not isinstance(hardware, dict) or set(hardware) != {
        "gpuModel", "gpuDriverVersion", "gpuAdapterLuidHigh", "gpuAdapterLuidLow"
    } or any(not isinstance(hardware[field], str) or not hardware[field].strip() for field in ("gpuModel", "gpuDriverVersion")) or any(type(hardware[field]) is not int for field in ("gpuAdapterLuidHigh", "gpuAdapterLuidLow")):
        raise ValueError("run provenance hardware identity is invalid")
    backend = key.get("rendererBackend")
    if backend not in ("d3d11", "d3d12") or type(key.get("backendGeneration")) is not int or key["backendGeneration"] != runtime[f"{backend}Generation"]:
        raise ValueError("run provenance backend generation mismatch")
    profile_sha = key.get("depthProfileSha256")
    if not isinstance(profile_sha, str) or len(profile_sha) != 71 or not profile_sha.startswith("sha256:") or any(c not in "0123456789abcdef" for c in profile_sha[7:]):
        raise ValueError("run provenance depth profile hash is invalid")
    profile_path = root / f"depth-profile-{profile_sha[7:]}.json"
    if _sha256(profile_path.resolve(strict=True)) != profile_sha:
        raise ValueError("run provenance depth profile snapshot hash mismatch")
    profile = _json_object(profile_path, "depth profile snapshot")
    actual_profile = ready.get("actual_profile")
    if not isinstance(actual_profile, dict) or any(
        actual_profile.get(field) != profile.get(field)
        for field in ("schema_version", "game_id", "lane", "executable_sha256")
    ) or profile.get("game_id") != binding.get("profile_id") or profile.get("schema_version") != binding.get("profile_version") or profile.get("executable_sha256") != binding.get("executable_sha256"):
        raise ValueError("run provenance depth profile identity mismatch")
    # D3D11 is the producer's serde default for legacy profiles that omit renderer_backend.
    if profile.get("renderer_backend", "d3d11") != backend or key.get("gameId") != profile.get("game_id") or key.get("width") != profile.get("delivery_width") or key.get("height") != profile.get("delivery_height"):
        raise ValueError("run provenance performance key differs from depth profile")
    return {
        "probe_dll_sha256": runtime["dllSha256"],
        "probe_version": runtime["runtimeVersion"],
        "camera_binding_sha256": key.get("cameraBindingSha256"),
        "depth_profile_sha256": profile_sha,
        "gpu_model": hardware["gpuModel"],
        "gpu_driver_version": hardware["gpuDriverVersion"],
        "gpu_adapter_luid_high": hardware["gpuAdapterLuidHigh"],
        "gpu_adapter_luid_low": hardware["gpuAdapterLuidLow"],
    }, {
        "runtime_manifest_snapshot": str(runtime_path),
        "runtime_manifest_snapshot_sha256": _sha256(runtime_path),
        "depth_profile_snapshot": str(profile_path),
        "depth_profile_snapshot_sha256": profile_sha,
        "performance_key": key,
    }


def _authoring_allows_production(manifest: dict[str, Any]) -> bool:
    if manifest.get("requires_e_geo_validation") is not True or manifest.get("production_binding_authorized") is not False:
        raise ValueError("authoring manifest is not awaiting E-geo validation")
    schema = manifest.get("schema")
    reason = manifest.get("trial_reason")
    if schema == "camera-binding-authoring/v1" and reason is None:
        return True
    if schema == "camera-binding-authoring-trial/v1" and isinstance(reason, str) and 0 < len(reason.strip()) <= 1024:
        return False
    raise ValueError("unsupported or inconsistent camera authoring manifest schema/trial reason")


def _verified_artifact(artifact: Any, label: str) -> Path:
    if not isinstance(artifact, dict):
        raise ValueError(f"authoring manifest {label} artifact is missing")
    raw_path = artifact.get("path")
    expected_sha256 = artifact.get("sha256")
    if not isinstance(raw_path, str) or not raw_path:
        raise ValueError(f"authoring manifest {label} path is invalid")
    if not isinstance(expected_sha256, str) or not expected_sha256.startswith("sha256:"):
        raise ValueError(f"authoring manifest {label} SHA-256 is invalid")
    path = Path(raw_path)
    if not path.is_absolute():
        # The authoring CLI records paths relative to its invocation workspace (DataMiner).
        path = Path.cwd() / path
    path = path.resolve(strict=True)
    if not path.is_file() or _sha256(path) != expected_sha256:
        raise ValueError(f"authoring manifest {label} artifact hash mismatch")
    return path


def _unique_by(rows: Iterable[dict[str, Any]], field: str, label: str) -> dict[int, dict[str, Any]]:
    result: dict[int, dict[str, Any]] = {}
    previous: int | None = None
    for row in rows:
        value = row.get(field)
        if not isinstance(value, int) or value < 0:
            raise ValueError(f"{label} row has invalid {field}: {value!r}")
        if previous is not None and value < previous:
            raise ValueError(f"{label} {field} moves backwards: {previous} -> {value}")
        if value in result:
            raise ValueError(f"{label} contains duplicate {field}={value}")
        result[value] = row
        previous = value
    return result


def _exact_source(actual: dict[str, Any], expected: dict[str, Any], label: str) -> None:
    if actual != expected:
        raise ValueError(f"{label} source identity differs from authoring manifest")


def _validate_camera(
    row: dict[str, Any], binding: dict[str, Any], manifest: dict[str, Any]
) -> None:
    if row.get("schema") != "camera-pose-index/v1" or row.get("source") != "resolved_render_camera_hybrid":
        raise ValueError(f"present_seq={row.get('present_seq')}: camera is not the render hybrid")
    provenance = row.get("provenance")
    if not isinstance(provenance, dict) or provenance.get("kind") != "resolved_render_camera_hybrid":
        raise ValueError(f"present_seq={row.get('present_seq')}: camera provenance kind is invalid")
    for field in ("binding_id", "binding_version", "profile_id", "profile_version", "executable_sha256"):
        if provenance.get(field) != binding.get(field):
            raise ValueError(
                f"present_seq={row.get('present_seq')}: camera provenance {field} mismatch"
            )
    position_source = manifest.get("position_source")
    if not isinstance(position_source, dict):
        raise ValueError("authoring manifest has no exact-build position source")
    if provenance.get("game_camera") != position_source:
        raise ValueError(f"present_seq={row.get('present_seq')}: camera position source mismatch")
    if position_source.get("metres_per_game_unit") != manifest.get("view_metres_per_unit"):
        raise ValueError(f"present_seq={row.get('present_seq')}: camera metric scale mismatch")
    if (
        provenance.get("render_width") != binding.get("render_width")
        or provenance.get("render_height") != binding.get("render_height")
    ):
        raise ValueError(f"present_seq={row.get('present_seq')}: render geometry mismatch")
    if not isinstance(provenance.get("dsv_id"), str) or not provenance["dsv_id"]:
        raise ValueError(f"present_seq={row.get('present_seq')}: camera DSV identity is missing")
    _exact_source(
        provenance.get("render_basis_source"), manifest["selected_view"], "camera render basis"
    )
    _exact_source(
        provenance.get("projection_source"), manifest["selected_projection"], "camera projection"
    )
    position = np.asarray(row.get("camera_position"), dtype=np.float64)
    quaternion = np.asarray(row.get("camera_rotation_quaternion"), dtype=np.float64)
    if position.shape != (3,) or quaternion.shape != (4,) or not np.isfinite(position).all() or not np.isfinite(quaternion).all():
        raise ValueError(f"present_seq={row.get('present_seq')}: camera pose is invalid")
    if abs(float(np.linalg.norm(quaternion)) - 1.0) > 1e-3:
        raise ValueError(f"present_seq={row.get('present_seq')}: camera quaternion is not unit")
    render_basis = np.asarray(provenance.get("render_basis_matrix"), dtype=np.float64)
    if render_basis.shape != (4, 4) or not np.isfinite(render_basis).all():
        raise ValueError(f"present_seq={row.get('present_seq')}: render basis matrix is invalid")
    # Fallout's pinned payload is world-to-camera and left-handed.  The Y/Z
    # column swap repairs handedness; transpose then yields the camera-to-world
    # rotation required by the pose contract.
    world_to_camera = render_basis[:3, :3][:, [0, 2, 1]]
    if not np.allclose(world_to_camera @ world_to_camera.T, np.eye(3), atol=2e-3):
        raise ValueError(f"present_seq={row.get('present_seq')}: render basis is not orthonormal")
    if not math.isclose(float(np.linalg.det(world_to_camera)), 1.0, abs_tol=2e-3):
        raise ValueError(f"present_seq={row.get('present_seq')}: repaired render basis is not proper")
    expected_camera_to_world = world_to_camera.T
    if not np.allclose(quat_to_mat(quaternion), expected_camera_to_world, atol=2e-4):
        raise ValueError(
            f"present_seq={row.get('present_seq')}: quaternion does not encode the "
            "camera-to-world transpose of the render basis"
        )

    projection = np.asarray(provenance.get("projection_matrix"), dtype=np.float64)
    if projection.shape != (4, 4) or not np.isfinite(projection).all():
        raise ValueError(f"present_seq={row.get('present_seq')}: projection matrix is invalid")
    intrinsics = row.get("camera_intrinsics")
    if not isinstance(intrinsics, dict):
        raise ValueError(f"present_seq={row.get('present_seq')}: camera intrinsics are missing")
    width = float(binding["render_width"])
    height = float(binding["render_height"])
    expected_intrinsics = {
        "fx": projection[0, 0] * width * 0.5,
        "fy": projection[1, 1] * height * 0.5,
        "cx": (1.0 - projection[2, 0]) * width * 0.5,
        "cy": (1.0 + projection[2, 1]) * height * 0.5,
    }
    for name, expected in expected_intrinsics.items():
        actual = intrinsics.get(name)
        if not isinstance(actual, (int, float)) or not math.isfinite(float(actual)):
            raise ValueError(
                f"present_seq={row.get('present_seq')}: camera intrinsic {name} is invalid"
            )
        if not math.isclose(float(actual), float(expected), rel_tol=2e-6, abs_tol=2e-4):
            raise ValueError(
                f"present_seq={row.get('present_seq')}: camera intrinsic {name} "
                "does not match the observed projection"
            )


def _validate_depth(row: dict[str, Any], expected_scale: str) -> None:
    if row.get("schema") != "depth-index/v6":
        raise ValueError(f"present_seq={row.get('present_seq')}: unsupported depth schema")
    projection = row.get("projection")
    if not isinstance(projection, dict):
        raise ValueError(f"present_seq={row.get('present_seq')}: depth projection is missing")
    if projection.get("proj_source") != "constant_buffer" or projection.get("proj_scope") != "observed_this_frame":
        raise ValueError(f"present_seq={row.get('present_seq')}: depth projection is not per-frame observed")
    numeric_projection = (
        "proj_a",
        "proj_b",
        "metric_scale",
        "viewport_min_depth",
        "viewport_max_depth",
    )
    values: dict[str, float] = {}
    for field in numeric_projection:
        try:
            values[field] = float(projection[field])
        except (KeyError, TypeError, ValueError) as error:
            raise ValueError(
                f"present_seq={row.get('present_seq')}: depth projection {field} is invalid"
            ) from error
        if not math.isfinite(values[field]):
            raise ValueError(
                f"present_seq={row.get('present_seq')}: depth projection {field} is non-finite"
            )
    if values["proj_b"] == 0.0:
        raise ValueError(f"present_seq={row.get('present_seq')}: depth projection B is zero")
    reverse_z = projection.get("reverse_z")
    if not isinstance(reverse_z, bool) or reverse_z != (values["proj_b"] > 0.0):
        raise ValueError(f"present_seq={row.get('present_seq')}: reverse-Z metadata is inconsistent")
    if not (
        0.0 <= values["viewport_min_depth"] < values["viewport_max_depth"] <= 1.0
    ):
        raise ValueError(f"present_seq={row.get('present_seq')}: depth viewport is invalid")
    if not math.isclose(values["metric_scale"], float(expected_scale), rel_tol=0, abs_tol=1e-12):
        raise ValueError(f"present_seq={row.get('present_seq')}: depth/camera metric scales differ")
    if row.get("buffer_format") != "d32_float":
        raise ValueError(f"present_seq={row.get('present_seq')}: depth buffer format is invalid")
    if not isinstance(row.get("dsv_id"), str) or not row["dsv_id"]:
        raise ValueError(f"present_seq={row.get('present_seq')}: depth DSV identity is missing")
    if not isinstance(row.get("width"), int) or row["width"] <= 0:
        raise ValueError(f"present_seq={row.get('present_seq')}: depth width is invalid")
    if not isinstance(row.get("height"), int) or row["height"] <= 0:
        raise ValueError(f"present_seq={row.get('present_seq')}: depth height is invalid")
    expected_storage = {
        "encoding": "exr-half-zip",
        "domain": "raw_buffer",
        "channel": "Z",
        "transform": "one_minus_d",
    }
    if row.get("storage") != expected_storage:
        raise ValueError(f"present_seq={row.get('present_seq')}: depth storage contract differs")


def _stream_identity(row: dict[str, Any], label: str) -> tuple[str, str, int]:
    identity = row.get("identity")
    if not isinstance(identity, dict):
        raise ValueError(f"{label} has no stream identity")
    session_id = identity.get("session_id")
    run_id = identity.get("run_id")
    generation = identity.get("generation")
    if not isinstance(session_id, str) or not session_id:
        raise ValueError(f"{label} has invalid session_id")
    if not isinstance(run_id, str) or not run_id:
        raise ValueError(f"{label} has invalid run_id")
    if not isinstance(generation, int) or isinstance(generation, bool) or generation < 0:
        raise ValueError(f"{label} has invalid generation")
    return session_id, run_id, generation


def _validate_depth_camera_alignment(
    pair: dict[str, Any], depth: dict[str, Any], camera: dict[str, Any]
) -> None:
    present_seq = depth["present_seq"]
    pair_depth = pair.get("depth")
    if not isinstance(pair_depth, dict) or pair_depth.get("present_seq") != present_seq:
        raise ValueError(f"present_seq={present_seq}: frame-pair depth identity differs")
    dsv_id = depth.get("dsv_id")
    if pair_depth.get("dsv_id") != dsv_id or camera["provenance"].get("dsv_id") != dsv_id:
        raise ValueError(f"present_seq={present_seq}: frame-pair/depth/camera DSV differs")
    projection = np.asarray(camera["provenance"].get("projection_matrix"), dtype=np.float64)
    viewport_min = float(depth["projection"]["viewport_min_depth"])
    viewport_max = float(depth["projection"]["viewport_max_depth"])
    viewport_span = viewport_max - viewport_min
    expected_a = viewport_min + viewport_span * float(projection[2, 2])
    expected_b = viewport_span * float(projection[3, 2])
    if not math.isclose(
        float(depth["projection"]["proj_a"]), expected_a, rel_tol=2e-6, abs_tol=2e-6
    ) or not math.isclose(
        float(depth["projection"]["proj_b"]), expected_b, rel_tol=2e-6, abs_tol=2e-6
    ):
        raise ValueError(
            f"present_seq={present_seq}: depth A/B does not match the camera projection and viewport"
        )


def _validate_pair_delivery_contract(
    *,
    pairs: dict[int, dict[str, Any]],
    depths: dict[int, dict[str, Any]],
    cameras: dict[int, dict[str, Any]],
    binding: dict[str, Any],
    manifest: dict[str, Any],
    stream_identity: tuple[str, str, int],
) -> None:
    expected_scale = manifest["position_source"]["metres_per_game_unit"]
    for session_frame, pair in pairs.items():
        pair_depth = pair.get("depth")
        if session_frame % 2:
            if pair_depth is not None or pair.get("depth_absent_reason") != "odd_frame_by_spec":
                raise ValueError(
                    f"session_frame={session_frame}: odd frame depth absence contract differs"
                )
            continue
        if not isinstance(pair_depth, dict):
            raise ValueError(f"session_frame={session_frame}: even frame depth is missing")
        present_seq = pair.get("present_seq")
        if not isinstance(present_seq, int) or pair_depth.get("present_seq") != present_seq:
            raise ValueError(f"session_frame={session_frame}: RGB/depth full identity mismatch")
        depth = depths.get(present_seq)
        camera = cameras.get(present_seq)
        if depth is None or camera is None:
            raise ValueError(
                f"session_frame={session_frame}: exact depth/camera identity is missing"
            )
        _validate_depth(depth, expected_scale)
        _validate_camera(camera, binding, manifest)
        if _stream_identity(camera, f"camera present_seq={present_seq}") != stream_identity:
            raise ValueError(f"session_frame={session_frame}: camera generation changes within run")
        _validate_depth_camera_alignment(pair, depth, camera)


def _read_depth(root: Path, row: dict[str, Any]) -> np.ndarray:
    relative = row.get("path")
    present_seq = row.get("present_seq")
    expected_relative = f"raw/{present_seq:012}.exr" if isinstance(present_seq, int) else ""
    if (
        not isinstance(relative, str)
        or relative != expected_relative
        or PurePosixPath(relative).is_absolute()
        or ".." in PurePosixPath(relative).parts
    ):
        raise ValueError(f"present_seq={present_seq}: depth path is not canonical")
    resolved_root = root.resolve(strict=True)
    path = (resolved_root / Path(*PurePosixPath(relative).parts)).resolve(strict=True)
    if path.parent != resolved_root / "raw":
        raise ValueError(f"present_seq={present_seq}: depth path escapes the raw directory")
    try:
        import OpenEXR
    except ImportError as error:
        raise ValueError(
            "reading named depth channels requires OpenEXR; install the ml data extra"
        ) from error
    storage = row.get("storage", {})
    channel_name = storage.get("channel")
    if not isinstance(channel_name, str) or not channel_name:
        raise ValueError(f"depth row has no storage channel for {path}")
    try:
        exr = OpenEXR.File(str(path))
    except Exception as error:
        raise ValueError(f"cannot read depth EXR {path}: {error}") from error
    if len(exr.parts) != 1:
        raise ValueError(f"depth EXR {path} must contain exactly one part")
    header = exr.header()
    if header.get("name") != "depth":
        raise ValueError(f"depth EXR {path} part name is not 'depth'")
    if header.get("type") != OpenEXR.scanlineimage:
        raise ValueError(f"depth EXR {path} is not scanline storage")
    if header.get("compression") != OpenEXR.ZIP_COMPRESSION:
        raise ValueError(f"depth EXR {path} is not ZIP-compressed")
    channels = exr.channels()
    if set(channels) != {channel_name}:
        raise ValueError(
            f"depth EXR {path} channels differ from the index: "
            f"expected only {channel_name!r}, got {sorted(channels)}"
        )
    stored = np.asarray(channels[channel_name].pixels)
    channel = channels[channel_name]
    if channel.xSampling != 1 or channel.ySampling != 1:
        raise ValueError(f"depth EXR {path} channel sampling is not 1x1")
    if stored.dtype != np.dtype(np.float16):
        raise ValueError(f"depth EXR {path} channel is not HALF")
    if stored.shape != (int(row["height"]), int(row["width"])):
        raise ValueError(f"depth EXR geometry mismatch for {path}: {stored.shape}")
    stored = stored.astype(np.float64)
    if not np.isfinite(stored).all():
        raise ValueError(f"depth EXR {path} contains non-finite stored samples")
    d = 1.0 - stored
    projection = row["projection"]
    raw_clear = 0.0 if projection["reverse_z"] else 1.0
    clear = d == raw_clear
    a = float(projection["proj_a"])
    b = float(projection["proj_b"])
    scale = float(projection["metric_scale"])
    with np.errstate(divide="ignore", invalid="ignore"):
        metres = b / (d - a) * scale
    valid = np.isfinite(metres) & (metres > 0.0) & (metres < 10_000.0) & ~clear
    return np.where(valid, metres, np.nan)


def _video_frames(video: Path, wanted: set[int]) -> dict[int, np.ndarray]:
    capture = cv2.VideoCapture(str(video))
    if not capture.isOpened():
        raise ValueError(f"cannot open video {video}")
    found: dict[int, np.ndarray] = {}
    index = 0
    last = max(wanted) if wanted else -1
    while index <= last:
        ok, frame = capture.read()
        if not ok:
            break
        if index in wanted:
            found[index] = cv2.cvtColor(frame, cv2.COLOR_BGR2GRAY)
        index += 1
    capture.release()
    missing = sorted(wanted - found.keys())
    if missing:
        raise ValueError(f"video is missing requested session frames: {missing[:8]}")
    return found


def _percentile(values: list[float], q: float) -> float:
    return float(np.percentile(np.asarray(values, dtype=np.float64), q))


def _corr(left: list[float], right: list[float]) -> float | None:
    a = np.asarray(left, dtype=np.float64)
    b = np.asarray(right, dtype=np.float64)
    if len(a) < 3 or float(a.std()) < 1e-12 or float(b.std()) < 1e-12:
        return None
    return float(np.corrcoef(a, b)[0, 1])


@dataclass(frozen=True)
class Thresholds:
    min_pairs: int = 30
    min_valid_frac_p10: float = 0.50
    min_improvement_p50: float = 1.05
    min_improved_share: float = 0.60
    min_inverse_separation: float = 1.05
    min_baseline_l1: float = 0.50
    min_translation_pairs: int = 5
    min_translation_metres: float = 0.02
    min_rotation_pairs: int = 5
    min_rotation_radians: float = 0.01


def run_gate(
    *,
    run_root: Path,
    video: Path,
    pairs_path: Path,
    depth_index_path: Path,
    depth_root: Path,
    camera_path: Path,
    authoring_manifest_path: Path,
    probe_dll_path: Path,
    max_pairs: int = 120,
    thresholds: Thresholds = Thresholds(),
) -> dict[str, Any]:
    manifest = json.loads(authoring_manifest_path.read_text(encoding="utf-8"))
    authoring_allows_production = _authoring_allows_production(manifest)
    candidate_path = _verified_artifact(manifest.get("candidate_binding"), "candidate binding")
    _verified_artifact(manifest.get("sweep_evidence"), "sweep evidence")
    phase_artifacts = manifest.get("phase_reports")
    if not isinstance(phase_artifacts, list) or len(phase_artifacts) != 3:
        raise ValueError("authoring manifest must bind exactly three phase reports")
    for index, artifact in enumerate(phase_artifacts):
        _verified_artifact(artifact, f"phase report {index}")
    binding = json.loads(candidate_path.read_text(encoding="utf-8"))
    if binding.get("schema") != "camera-binding/v1":
        raise ValueError("unsupported authoring CameraBinding schema")
    if binding.get("game_camera") != manifest.get("position_source"):
        raise ValueError("authoring CameraBinding position source differs from manifest")
    if binding.get("view_sources") != [manifest.get("selected_view")]:
        raise ValueError("authoring CameraBinding render basis differs from manifest")
    if binding.get("projection_sources") != [manifest.get("selected_projection")]:
        raise ValueError("authoring CameraBinding projection differs from manifest")
    if not probe_dll_path.resolve(strict=True).is_file():
        raise ValueError("captured probe DLL path is not a file")

    pairs = _unique_by(_read_jsonl(pairs_path), "session_frame", "frame pairs")
    depths = _unique_by(_read_jsonl(depth_index_path), "present_seq", "depth index")
    cameras = _unique_by(_read_jsonl(camera_path), "present_seq", "camera pose index")
    camera_identities = {
        _stream_identity(row, f"camera present_seq={present_seq}")
        for present_seq, row in cameras.items()
    }
    if len(camera_identities) != 1:
        raise ValueError("camera pose index contains more than one stream identity")
    stream_identity = next(iter(camera_identities))
    _validate_pair_delivery_contract(
        pairs=pairs,
        depths=depths,
        cameras=cameras,
        binding=binding,
        manifest=manifest,
        stream_identity=stream_identity,
    )
    capture_provenance = _validate_capture_provenance(
        run_root=run_root,
        video=video,
        pairs=pairs,
        depth_index_path=depth_index_path,
        depth_root=depth_root,
        camera_path=camera_path,
        stream_identity=stream_identity,
        candidate_binding_sha256=manifest["candidate_binding"]["sha256"],
        binding=binding,
        probe_dll_path=probe_dll_path,
        depth_row_count=len(depths),
        camera_row_count=len(cameras),
    )
    eligible: list[tuple[int, int, int, int]] = []
    for session_frame, pair in pairs.items():
        later = pairs.get(session_frame + 2)
        if session_frame % 2 or later is None:
            continue
        seq_t = pair["depth"].get("present_seq")
        seq_tk = later["depth"].get("present_seq")
        if pair.get("present_seq") != seq_t or later.get("present_seq") != seq_tk:
            raise ValueError(f"session_frame={session_frame}: RGB/depth full identity mismatch")
        if seq_t not in depths or seq_tk not in depths or seq_t not in cameras or seq_tk not in cameras:
            raise ValueError(f"session_frame={session_frame}: exact depth/camera identity is missing")
        eligible.append((session_frame, session_frame + 2, seq_t, seq_tk))
    eligible_pairs_total = len(eligible)
    if len(eligible) > max_pairs:
        indexes = np.linspace(0, len(eligible) - 1, max_pairs, dtype=int)
        eligible = [eligible[int(index)] for index in indexes]
    wanted = {frame for item in eligible for frame in item[:2]}
    frames = _video_frames(video, wanted)

    improvements: list[float] = []
    inverse_improvements: list[float] = []
    valid_fracs: list[float] = []
    baseline_l1: list[float] = []
    pose_motion: list[float] = []
    skipped_low_motion = 0
    invalid_metric_pairs = 0
    translation_pairs = 0
    rotation_pairs = 0
    consumed_depths: dict[int, dict[str, Any]] = {}
    for frame_t, frame_tk, seq_t, seq_tk in eligible:
        depth_t = depths[seq_t]
        depth_tk = depths[seq_tk]
        camera_t = cameras[seq_t]
        camera_tk = cameras[seq_tk]
        expected_scale = manifest["position_source"]["metres_per_game_unit"]
        _validate_depth(depth_t, expected_scale)
        _validate_depth(depth_tk, expected_scale)
        _validate_camera(camera_t, binding, manifest)
        _validate_camera(camera_tk, binding, manifest)
        if (
            _stream_identity(camera_t, f"camera present_seq={seq_t}") != stream_identity
            or _stream_identity(camera_tk, f"camera present_seq={seq_tk}") != stream_identity
        ):
            raise ValueError(f"session_frame={frame_t}: camera generation changes within pair")
        _validate_depth_camera_alignment(pairs[frame_t], depth_t, camera_t)
        _validate_depth_camera_alignment(pairs[frame_tk], depth_tk, camera_tk)
        z = _read_depth(depth_root, depth_t)
        consumed_depths[seq_t] = {
            "present_seq": seq_t,
            "path": depth_t["path"],
            "sha256": _sha256(depth_root / Path(*PurePosixPath(depth_t["path"]).parts)),
        }
        gray_t = frames[frame_t]
        gray_tk = frames[frame_tk]
        if gray_t.shape != z.shape:
            gray_t = cv2.resize(gray_t, (z.shape[1], z.shape[0]), interpolation=cv2.INTER_AREA)
            gray_tk = cv2.resize(gray_tk, (z.shape[1], z.shape[0]), interpolation=cv2.INTER_AREA)
        intrinsics = camera_t["camera_intrinsics"]
        render_width = camera_t["provenance"]["render_width"]
        render_height = camera_t["provenance"]["render_height"]
        intr = Pinhole(
            float(intrinsics["fx"]) * z.shape[1] / render_width,
            float(intrinsics["fy"]) * z.shape[0] / render_height,
            float(intrinsics["cx"]) * z.shape[1] / render_width,
            float(intrinsics["cy"]) * z.shape[0] / render_height,
        )
        pos_t = np.asarray(camera_t["camera_position"], dtype=np.float64)
        pos_tk = np.asarray(camera_tk["camera_position"], dtype=np.float64)
        quat_t = np.asarray(camera_t["camera_rotation_quaternion"], dtype=np.float64)
        quat_tk = np.asarray(camera_tk["camera_rotation_quaternion"], dtype=np.float64)
        rel = RelPose.between(pos_t, quat_t, pos_tk, quat_tk)
        translation_norm = float(np.linalg.norm(rel.translation))
        rotation_angle = math.acos(float(np.clip((np.trace(rel.rotation) - 1.0) / 2.0, -1.0, 1.0)))
        if translation_norm >= thresholds.min_translation_metres:
            translation_pairs += 1
        if rotation_angle >= thresholds.min_rotation_radians:
            rotation_pairs += 1
        result = e_geo(gray_t, gray_tk, z, intr, rel, -1.0)
        inverse = RelPose(rel.rotation.T, -(rel.rotation.T @ rel.translation))
        inverse_result = e_geo(gray_t, gray_tk, z, intr, inverse, -1.0)
        if not np.isfinite(result.baseline_l1):
            invalid_metric_pairs += 1
            continue
        if result.baseline_l1 < thresholds.min_baseline_l1:
            skipped_low_motion += 1
            continue
        if not (
            np.isfinite(result.improvement)
            and np.isfinite(inverse_result.improvement)
            and np.isfinite(result.valid_frac)
        ):
            invalid_metric_pairs += 1
            continue
        improvements.append(float(result.improvement))
        inverse_improvements.append(float(inverse_result.improvement))
        valid_fracs.append(float(result.valid_frac))
        baseline_l1.append(float(result.baseline_l1))
        pose_motion.append(translation_norm + rotation_angle)

    failures: list[str] = []
    if invalid_metric_pairs:
        failures.append(f"invalid_metric_pairs={invalid_metric_pairs} != 0")
    if translation_pairs < thresholds.min_translation_pairs:
        failures.append(
            f"translation_pairs={translation_pairs} < {thresholds.min_translation_pairs}"
        )
    if rotation_pairs < thresholds.min_rotation_pairs:
        failures.append(f"rotation_pairs={rotation_pairs} < {thresholds.min_rotation_pairs}")
    if len(improvements) < thresholds.min_pairs:
        failures.append(f"usable_pairs={len(improvements)} < {thresholds.min_pairs}")
    if improvements:
        valid_p10 = _percentile(valid_fracs, 10)
        improvement_p50 = _percentile(improvements, 50)
        inverse_p50 = _percentile(inverse_improvements, 50)
        improved_share = float(np.mean(np.asarray(improvements) > 1.0))
        if valid_p10 < thresholds.min_valid_frac_p10:
            failures.append(f"valid_frac_p10={valid_p10:.6f} < {thresholds.min_valid_frac_p10}")
        if improvement_p50 < thresholds.min_improvement_p50:
            failures.append(
                f"improvement_p50={improvement_p50:.6f} < {thresholds.min_improvement_p50}"
            )
        if improved_share < thresholds.min_improved_share:
            failures.append(f"improved_share={improved_share:.6f} < {thresholds.min_improved_share}")
        if improvement_p50 < inverse_p50 * thresholds.min_inverse_separation:
            failures.append(
                f"formal_convention_not_separated: formal_p50={improvement_p50:.6f} inverse_p50={inverse_p50:.6f}"
            )
    else:
        valid_p10 = improvement_p50 = inverse_p50 = improved_share = 0.0

    consumed_depth_manifest = [consumed_depths[key] for key in sorted(consumed_depths)]
    consumed_manifest_bytes = json.dumps(
        consumed_depth_manifest, sort_keys=True, separators=(",", ":")
    ).encode("utf-8")
    consumed_manifest_sha256 = f"sha256:{hashlib.sha256(consumed_manifest_bytes).hexdigest()}"
    module_path = Path(__file__).resolve()
    geo_module_path = module_path.with_name("geo_probes.py")
    passed = not failures
    return {
        "schema": SCHEMA if authoring_allows_production else "camera-egeo-trial-validation/v1",
        "passed": passed,
        "production_binding_authorized": passed and authoring_allows_production,
        "failures": failures,
        "inputs": {
            **capture_provenance,
            "video": str(video),
            "frame_pairs": str(pairs_path),
            "depth_index": str(depth_index_path),
            "depth_root": str(depth_root),
            "camera_pose_index": str(camera_path),
            "authoring_manifest": str(authoring_manifest_path),
            "probe_dll": str(probe_dll_path),
            "video_sha256": _sha256(video),
            "frame_pairs_sha256": _sha256(pairs_path),
            "depth_index_sha256": _sha256(depth_index_path),
            "camera_pose_index_sha256": _sha256(camera_path),
            "authoring_manifest_sha256": _sha256(authoring_manifest_path),
            "probe_dll_sha256": _sha256(probe_dll_path),
            "candidate_binding_sha256": manifest["candidate_binding"]["sha256"],
            "stream_identity": {
                "session_id": stream_identity[0],
                "run_id": stream_identity[1],
                "generation": stream_identity[2],
            },
            "consumed_depth_manifest_sha256": consumed_manifest_sha256,
            "consumed_depth_files": consumed_depth_manifest,
            "tools": {
                "capture_geo_gate_sha256": _sha256(module_path),
                "geo_probes_sha256": _sha256(geo_module_path),
                "numpy_version": np.__version__,
                "opencv_version": cv2.__version__,
                "openexr_version": importlib.metadata.version("OpenEXR"),
            },
        },
        "thresholds": asdict(thresholds),
        "counts": {
            "eligible_pairs_total": eligible_pairs_total,
            "sampled_pairs": len(eligible),
            "usable_pairs": len(improvements),
            "skipped_low_motion": skipped_low_motion,
            "invalid_metric_pairs": invalid_metric_pairs,
            "translation_pairs": translation_pairs,
            "rotation_pairs": rotation_pairs,
        },
        "metrics": {
            "improvement_p10": _percentile(improvements, 10) if improvements else None,
            "improvement_p50": improvement_p50 if improvements else None,
            "improvement_p90": _percentile(improvements, 90) if improvements else None,
            "inverse_improvement_p50": inverse_p50 if improvements else None,
            "improved_share": improved_share if improvements else None,
            "valid_frac_p10": valid_p10 if improvements else None,
            "valid_frac_p50": _percentile(valid_fracs, 50) if improvements else None,
            "baseline_l1_p50": _percentile(baseline_l1, 50) if improvements else None,
            "pose_image_motion_correlation": _corr(pose_motion, baseline_l1),
        },
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--run-root", type=Path, required=True)
    parser.add_argument("--video", type=Path, required=True)
    parser.add_argument("--pairs", type=Path, required=True)
    parser.add_argument("--depth-index", type=Path, required=True)
    parser.add_argument("--depth-root", type=Path, required=True)
    parser.add_argument("--camera", type=Path, required=True)
    parser.add_argument("--authoring", type=Path, required=True)
    parser.add_argument("--probe-dll", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--max-pairs", type=int, default=120)
    args = parser.parse_args()
    try:
        report = run_gate(
            run_root=args.run_root,
            video=args.video,
            pairs_path=args.pairs,
            depth_index_path=args.depth_index,
            depth_root=args.depth_root,
            camera_path=args.camera,
            authoring_manifest_path=args.authoring,
            probe_dll_path=args.probe_dll,
            max_pairs=args.max_pairs,
        )
    except Exception as error:  # CLI must persist tool failures separately from a failed gate.
        report = {"schema": SCHEMA, "passed": False, "production_binding_authorized": False, "tool_error": str(error)}
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_text(json.dumps(report, indent=2, ensure_ascii=False), encoding="utf-8")
        print(f"E-geo tool error: {error}")
        return 2
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(json.dumps(report, indent=2, ensure_ascii=False), encoding="utf-8")
    print(json.dumps(report["metrics"], ensure_ascii=False))
    print(f"E-geo {'PASS' if report['passed'] else 'FAIL'}: {args.out}")
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())

"""Strict local-only CP2077 E-geo v3 gate for the PR405 30-second inputs.

The tool consumes the exact 900 RGB-frame evidence rows, 450 even-frame raw D32 rows, 900 final
probe poses and the already validated dual-source metric evidence.  It never performs nearest-QPC
or neighbouring-frame substitution.  Geometry uses the same ``geo_probes`` implementation and
threshold family as the FO4 authoring gate, with an additional static control population.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Any, Iterable, Optional

import numpy as np

from diogenes_wm.eval.geo_probes import Pinhole, RelPose, e_geo


REPORT_SCHEMA = "dm-cp2077-camera-egeo-evidence/v1"
E_GEO_SCHEMA = "camera-egeo-validation/v3"
FRAME_SCHEMA = "dm-cp2077-pr405-frame-evidence/v1"
RAW_SCHEMA = "dm-cp2077-raw-depth-frame-index/v1"
METRIC_SCHEMA = "dm-cp2077-metric-evidence/v1"
PRODUCTION_BINDING_AUTHORIZED = False
EXACT_BUILD_ID = 20383525
EXACT_EXE_SHA256 = "sha256:a7de82945c03e041fc7339fcf9066224d98db2f5d80fea50f7947bb350a60991"
RGB_FRAMES = 900
DEPTH_FRAMES = 450
VIDEO_SIZE = (1920, 1080)
SOURCE_DEPTH_SIZE = (1920, 1080)
DEPTH_SIZE = (1280, 720)
RED4EXT_COMMIT = "ad7277714ad30d6885d7050c5ba24fa0102f6920"
RED4EXT_BLOB = "ddc2eb166a9b30b9f76f56354cf56c6d5d93320e"
CET_COMMIT = "ed6d40fb2facf2f529bb30a1ba707ccb1ed08e04"
CET_BLOB = "e1703719bf5e37917063435eff6b2e6dce1cb119"


@dataclass(frozen=True)
class Thresholds:
    min_static_pairs: int = 5
    max_static_translation_metres: float = 0.005
    max_static_rotation_radians: float = 0.005
    min_translation_pairs: int = 5
    min_translation_metres: float = 0.02
    min_rotation_pairs: int = 5
    min_rotation_radians: float = 0.01
    min_usable_motion_pairs: int = 30
    min_valid_frac_p10: float = 0.50
    min_improvement_p50: float = 1.05
    min_improved_share: float = 0.60
    min_inverse_separation: float = 1.05
    min_motion_baseline_l1: float = 0.50


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return f"sha256:{digest.hexdigest()}"


def _read_json(path: Path) -> dict[str, Any]:
    value = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(value, dict):
        raise ValueError(f"{path.name}: JSON root is not an object")
    return value


def _read_jsonl(path: Path, expected: int) -> list[dict[str, Any]]:
    rows: list[dict[str, Any]] = []
    with path.open("r", encoding="utf-8") as handle:
        for line_no, line in enumerate(handle, 1):
            if not line.strip():
                continue
            value = json.loads(line)
            if not isinstance(value, dict):
                raise ValueError(f"{path.name}:{line_no}: row is not an object")
            rows.append(value)
    if len(rows) != expected:
        raise ValueError(f"{path.name}: expected {expected} rows, observed {len(rows)}")
    return rows


def _canonical_session(path: Path) -> Path:
    if not path.is_absolute():
        raise ValueError("session path must be absolute")
    resolved = path.resolve(strict=True)
    if not resolved.is_dir():
        raise ValueError("session is not a directory")
    return resolved


def _direct_file(session: Path, path: Path, label: str) -> Path:
    if not path.is_absolute():
        raise ValueError(f"{label} path must be absolute")
    if path.is_symlink():
        raise ValueError(f"{label} must not be a symlink")
    resolved = path.resolve(strict=True)
    if not resolved.is_file() or resolved.parent != session:
        raise ValueError(f"{label} must be a direct regular child of the session")
    return resolved


def _leaf_file(session: Path, name: str, label: str) -> Path:
    if not name or Path(name).name != name or Path(name).is_absolute():
        raise ValueError(f"{label} is not a plain relative leaf name")
    return _direct_file(session, session / name, label)


def _raw_depth_file(session: Path, name: str) -> Path:
    if not name or Path(name).name != name or Path(name).is_absolute():
        raise ValueError("raw D32 is not a plain relative leaf name")
    root = (session / "raw-depth").resolve(strict=True)
    if not root.is_dir():
        raise ValueError("raw-depth is not a directory")
    path = root / name
    if path.is_symlink():
        raise ValueError("raw D32 must not be a symlink")
    resolved = path.resolve(strict=True)
    if not resolved.is_file() or resolved.parent != root:
        raise ValueError("raw D32 escapes the raw-depth directory")
    return resolved


def _bind_all_depth_files(
    session: Path, raw_rows: list[dict[str, Any]]
) -> list[dict[str, Any]]:
    bound = []
    for row in raw_rows:
        path = _raw_depth_file(session, row["file"])
        bound.append(
            {
                "ordinal": row["ordinal"],
                "file": row["file"],
                "present_seq": row["present_seq"],
                "qpc": row["present_qpc"],
                "sha256": _sha256(path),
            }
        )
    return bound


def _unique(rows: Iterable[dict[str, Any]], key: str, label: str) -> dict[Any, dict[str, Any]]:
    result: dict[Any, dict[str, Any]] = {}
    for row in rows:
        value = row.get(key)
        if value in result:
            raise ValueError(f"{label} repeats {key}={value}")
        result[value] = row
    return result


def _identity(value: Any, label: str) -> tuple[int, int]:
    if not isinstance(value, dict):
        raise ValueError(f"{label} has no Present identity")
    seq, qpc = value.get("present_seq"), value.get("qpc")
    if not isinstance(seq, int) or seq <= 0 or not isinstance(qpc, int) or qpc <= 0:
        raise ValueError(f"{label} has an invalid Present/QPC identity")
    return seq, qpc


def _finite_vector(value: Any, size: int, label: str) -> np.ndarray:
    result = np.asarray(value, dtype=np.float64)
    if result.shape != (size,) or not np.all(np.isfinite(result)):
        raise ValueError(f"{label} is not a finite vector[{size}]")
    return result


def _verify_evidence_file(
    session: Path, record: Any, label: str, expected: Optional[Path] = None
) -> Path:
    if not isinstance(record, dict):
        raise ValueError(f"metric {label} record is missing")
    path = _leaf_file(session, record.get("file"), f"metric {label}")
    if expected is not None and path != expected:
        raise ValueError(f"metric evidence does not bind the current {label}")
    if record.get("bytes") != path.stat().st_size or record.get("sha256") != _sha256(path):
        raise ValueError(f"metric {label} size/hash mismatch")
    return path


def _validate_metric_report(session: Path, metric: dict[str, Any]) -> float:
    if (
        metric.get("schema") != METRIC_SCHEMA
        or metric.get("non_trainable") is not True
        or metric.get("local_only") is not True
        or metric.get("upload_allowed") is not False
        or metric.get("steam_build_id") != EXACT_BUILD_ID
        or metric.get("executable_sha256") != EXACT_EXE_SHA256
        or not isinstance(metric.get("calibration_session_id"), str)
        or not metric["calibration_session_id"].strip()
        or not isinstance(metric.get("calibration_run_id"), str)
        or not metric["calibration_run_id"].strip()
        or metric.get("hud_visible_for_calibration_only") is not True
    ):
        raise ValueError("dual-source CP2077 metric evidence envelope is invalid")
    _verify_evidence_file(session, metric.get("pose"), "calibration pose")
    _verify_evidence_file(session, metric.get("frame_evidence"), "calibration frame evidence")
    _verify_evidence_file(session, metric.get("hud_observations"), "HUD observations")
    source_a = metric.get("source_a")
    expected_red4ext = {
        "repository": "https://github.com/WopsS/RED4ext.SDK",
        "commit": RED4EXT_COMMIT,
        "path": "include/RED4ext/Scripting/Natives/WorldPosition.hpp",
        "git_blob_sha1": RED4EXT_BLOB,
    }
    expected_cet = {
        "repository": "https://github.com/martine-dee/cp2077-traveling-companion-distance-meter",
        "commit": CET_COMMIT,
        "path": "bin/x64/plugins/cyber_engine_tweaks/mods/TravelingCompanionDistanceMeter/init.lua",
        "git_blob_sha1": CET_BLOB,
    }
    if (
        not isinstance(source_a, dict)
        or source_a.get("kind") != "red4ext_worldposition_plus_cet_distance_meter"
        or source_a.get("red4ext_world_position") != expected_red4ext
        or source_a.get("fixed_point_divisor") != 131072
        or source_a.get("world_position_bytes") != 12
        or source_a.get("cet_distance_meter") != expected_cet
        or source_a.get("cet_algorithm") != "euclidean_delta_of_consecutive_Game.GetPlayer_GetWorldPosition"
        or source_a.get("cet_output_unit") != "metres"
        or source_a.get("inferred_metres_per_converted_world_unit") != 1.0
    ):
        raise ValueError("metric source A is not the pinned RED4ext/CET implementation evidence")
    source_b = metric.get("source_b")
    samples = source_b.get("samples") if isinstance(source_b, dict) else None
    if not isinstance(samples, list) or not 3 <= len(samples) <= 32:
        raise ValueError("metric source B lacks bounded HUD/probe samples")
    positions: list[np.ndarray] = []
    hud: list[float] = []
    previous_identity: tuple[int, int] | None = None
    for index, sample in enumerate(samples):
        if not isinstance(sample, dict):
            raise ValueError("metric source B sample is not an object")
        screenshot = _verify_evidence_file(session, sample.get("screenshot"), f"HUD screenshot {index}")
        if not screenshot.read_bytes().startswith(b"\x89PNG\r\n\x1a\n"):
            raise ValueError("metric HUD screenshot is not PNG")
        identity = (sample.get("present_seq"), sample.get("qpc"))
        if (
            not all(isinstance(value, int) and value > 0 for value in identity)
            or previous_identity is not None
            and (identity[0] <= previous_identity[0] or identity[1] <= previous_identity[1])
        ):
            raise ValueError("metric source B Present/QPC timeline is invalid")
        previous_identity = identity
        positions.append(_finite_vector(sample.get("player_position"), 3, "metric player position"))
        distance_m = sample.get("objective_distance_metres")
        if not isinstance(distance_m, int) or distance_m <= 0:
            raise ValueError("metric HUD reading is not a positive whole metre")
        hud.append(float(distance_m))
    chord = float(np.linalg.norm(positions[-1] - positions[0]))
    path = sum(float(np.linalg.norm(right - left)) for left, right in zip(positions, positions[1:]))
    hud_span = abs(hud[-1] - hud[0])
    if chord < 20.0 or path / chord > 1.02 or hud_span < 20.0:
        raise ValueError("metric source B is not a long straight radial segment")
    signs = {math.copysign(1.0, right - left) for left, right in zip(hud, hud[1:]) if right != left}
    if len(signs) != 1 or any(right == left for left, right in zip(hud, hud[1:])):
        raise ValueError("metric HUD readings are not strictly monotonic")
    measured = hud_span / chord
    cross = metric.get("cross_check")
    if (
        not isinstance(cross, dict)
        or cross.get("passed") is not True
        or cross.get("source_a") != 1.0
        or not math.isclose(float(cross.get("source_b", math.nan)), measured, rel_tol=1e-12)
        or not math.isclose(float(cross.get("ratio", math.nan)), measured, rel_tol=1e-12)
        or cross.get("tolerance") != 0.05
        or abs(measured - 1.0) > 0.05
        or metric.get("adopted_metres_per_world_unit") != 1.0
    ):
        raise ValueError("metric source A/B cross-check does not recompute")
    for key, expected in (
        ("probe_chord_world_units", chord),
        ("probe_sample_path_world_units", path),
        ("path_to_chord_ratio", path / chord),
        ("hud_span_metres", hud_span),
        ("measured_metres_per_world_unit", measured),
    ):
        if not math.isclose(float(source_b.get(key, math.nan)), expected, rel_tol=1e-12):
            raise ValueError(f"metric source B derived field {key} does not recompute")
    return 1.0


def _validate_inputs(
    frames: list[dict[str, Any]],
    raw_rows: list[dict[str, Any]],
    poses: list[dict[str, Any]],
    metric: dict[str, Any],
    source_candidate_id: Optional[str] = None,
) -> tuple[dict[int, dict[str, Any]], dict[tuple[int, int], dict[str, Any]], dict[int, dict[str, Any]], float]:
    raw_by_identity: dict[tuple[int, int], dict[str, Any]] = {}
    for ordinal, row in enumerate(raw_rows):
        identity = (row.get("present_seq"), row.get("present_qpc"))
        if (
            row.get("schema") != RAW_SCHEMA
            or row.get("ordinal") != ordinal
            or row.get("file") != f"{ordinal:012}.d32"
            or row.get("width") != SOURCE_DEPTH_SIZE[0]
            or row.get("height") != SOURCE_DEPTH_SIZE[1]
            or row.get("row_pitch") != SOURCE_DEPTH_SIZE[0] * 4
            or row.get("pixel_format") != "DXGI_FORMAT_D32_FLOAT_LE"
            or row.get("byte_len") != SOURCE_DEPTH_SIZE[0] * SOURCE_DEPTH_SIZE[1] * 4
            or row.get("rgb_pairing") != "same_present_seq_gpu_watermark_submitted"
            or source_candidate_id is not None
            and row.get("source_candidate_id") != source_candidate_id
            or not isinstance(identity[0], int)
            or identity[0] <= 0
            or not isinstance(identity[1], int)
            or identity[1] <= 0
            or identity in raw_by_identity
        ):
            raise ValueError(f"raw depth ordinal {ordinal} violates the exact CP D32 contract")
        raw_by_identity[identity] = row

    poses_by_frame = _unique(poses, "session_frame", "pose")
    frames_by_index = _unique(frames, "session_frame", "frame evidence")
    previous: tuple[int, int] | None = None
    used_raw: set[tuple[int, int]] = set()
    camera_sources: set[str] = set()
    camera_identities: set[str] = set()
    projection_sources: set[str] = set()
    for index in range(RGB_FRAMES):
        frame = frames_by_index.get(index)
        pose = poses_by_frame.get(index) if frame else None
        if frame is None or frame.get("schema") != FRAME_SCHEMA or pose is None:
            raise ValueError(f"session_frame={index}: frame or pose is missing")
        rgb = _identity(frame.get("rgb"), f"frame {index} RGB")
        if _identity(frame.get("camera"), f"frame {index} camera") != rgb or _identity(
            frame.get("player"), f"frame {index} player"
        ) != rgb:
            raise ValueError(f"session_frame={index}: RGB/camera/player identity mismatch")
        if pose.get("session_frame") != index or pose.get("present_seq") != rgb[0]:
            raise ValueError(f"session_frame={index}: pose is not the exact Present row")
        source = pose.get("camera_source")
        if source not in ("resolved_main_camera_hook", "resolved_render_camera_hybrid"):
            raise ValueError(f"session_frame={index}: camera source is not an admitted probe source")
        camera_identity = pose.get("camera_identity")
        if not isinstance(camera_identity, dict) or camera_identity.get("kind") != "probe_stream":
            raise ValueError(f"session_frame={index}: camera identity is not a probe stream")
        camera_sources.add(source)
        camera_identities.add(json.dumps(camera_identity, sort_keys=True))
        quat = _finite_vector(pose.get("camera_rotation_quaternion"), 4, "camera quaternion")
        if abs(float(np.dot(quat, quat)) - 1.0) > 0.002:
            raise ValueError(f"session_frame={index}: camera quaternion is not normalized")
        _finite_vector(pose.get("camera_position"), 3, "camera position")
        intrinsics = pose.get("camera_intrinsics")
        if not isinstance(intrinsics, dict):
            raise ValueError(f"session_frame={index}: camera intrinsics are missing")
        fx, fy, cx, cy = (intrinsics.get(key) for key in ("fx", "fy", "cx", "cy"))
        if (
            not all(isinstance(value, (float, int)) and math.isfinite(value) for value in (fx, fy, cx, cy))
            or fx <= 0
            or fy <= 0
            or abs(fx - fy) / max(fx, fy) > 0.02
            or abs(cx - 960.0) > 0.01
            or abs(cy - 540.0) > 0.01
        ):
            raise ValueError(f"session_frame={index}: camera intrinsics are not stable 1920x1080 pinhole values")
        if previous is not None:
            duplicate = bool(frame.get("duplicate"))
            if duplicate:
                if rgb != previous:
                    raise ValueError(f"session_frame={index}: duplicate changed identity")
            elif rgb[0] <= previous[0] or rgb[1] <= previous[1]:
                raise ValueError(f"session_frame={index}: Present/QPC did not strictly advance")
        previous = rgb
        depth = frame.get("depth")
        projection = frame.get("projection")
        if index % 2:
            if depth is not None or projection is not None:
                raise ValueError(f"session_frame={index}: odd RGB frame carries depth/projection")
            continue
        if not isinstance(depth, dict) or not isinstance(projection, dict):
            raise ValueError(f"session_frame={index}: even frame lacks depth/projection")
        if depth.get("ordinal") != index // 2 or _identity(depth.get("identity"), "depth") != rgb:
            raise ValueError(f"session_frame={index}: depth identity mismatch")
        if depth.get("file") != f"{index // 2:012}.d32" or rgb not in raw_by_identity:
            raise ValueError(f"session_frame={index}: exact raw depth row is missing")
        if (
            projection.get("reverse_z") is not True
            or projection.get("observed_this_frame") is not True
            or projection.get("candidate_admitted") is not True
            or not isinstance(projection.get("physical_source"), str)
            or not projection["physical_source"].strip()
        ):
            raise ValueError(f"session_frame={index}: physical projection is not admitted")
        projection_sources.add(projection["physical_source"])
        a, b = projection.get("proj_a"), projection.get("proj_b")
        if not isinstance(a, (float, int)) or not math.isfinite(a) or not isinstance(b, (float, int)) or not math.isfinite(b) or b <= 0:
            raise ValueError(f"session_frame={index}: reverse-Z projection coefficients are invalid")
        used_raw.add(rgb)
    if len(used_raw) != DEPTH_FRAMES or used_raw != set(raw_by_identity):
        raise ValueError("raw depth rows are not exactly the 450 selected even-frame identities")
    if len(camera_sources) != 1 or len(camera_identities) != 1 or len(projection_sources) != 1:
        raise ValueError("camera identity/source or physical projection source changes within the clip")

    if (
        metric.get("schema") != METRIC_SCHEMA
        or metric.get("non_trainable") is not True
        or metric.get("local_only") is not True
        or metric.get("upload_allowed") is not False
        or metric.get("steam_build_id") != EXACT_BUILD_ID
        or metric.get("executable_sha256") != EXACT_EXE_SHA256
        or metric.get("cross_check", {}).get("passed") is not True
    ):
        raise ValueError("dual-source CP2077 metric evidence did not pass")
    scale = metric.get("adopted_metres_per_world_unit")
    if not isinstance(scale, (float, int)) or not math.isfinite(scale) or scale <= 0:
        raise ValueError("metric evidence has no positive adopted scale")
    return frames_by_index, raw_by_identity, poses_by_frame, float(scale)


def _validate_raw_profile(profile: dict[str, Any]) -> str:
    selector = profile.get("selector")
    candidate_id = selector.get("candidate_id") if isinstance(selector, dict) else None
    candidate_parts = candidate_id.split("-") if isinstance(candidate_id, str) else []
    semantic_candidate_identity = (
        len(candidate_parts) == 4
        and candidate_parts[0] == "dsv"
        and all(
            part.startswith(prefix)
            and len(part) > 1
            and part[1:].isdigit()
            for part, prefix in zip(candidate_parts[1:], ("r", "g", "e"), strict=True)
        )
    )
    if (
        profile.get("schema") != "dm-local-raw-depth-demo-profile/v1"
        or profile.get("game_id") != "cyberpunk-2077-steam"
        or profile.get("executable_sha256") != EXACT_EXE_SHA256
        or profile.get("capture_contract") != "pr405_local"
        or profile.get("duration_ms") != 30000
        or profile.get("non_trainable") is not True
        or profile.get("upload_allowed") is not False
        or profile.get("deliverable") is not False
        or profile.get("recording_ready") is not False
        or not isinstance(selector, dict)
        or selector.get("width") != SOURCE_DEPTH_SIZE[0]
        or selector.get("height") != SOURCE_DEPTH_SIZE[1]
        or selector.get("sample_count") != 1
        or selector.get("resource_format") != 19
        or selector.get("dsv_format") != 20
        or not semantic_candidate_identity
    ):
        raise ValueError("raw profile is not the exact native 1920x1080 CP2077 D32S8 selector")
    return candidate_id


def _video_frames(video: Path, wanted: set[int]) -> dict[int, np.ndarray]:
    import cv2

    capture = cv2.VideoCapture(str(video))
    if not capture.isOpened():
        raise ValueError("video cannot be decoded")
    width = int(capture.get(cv2.CAP_PROP_FRAME_WIDTH))
    height = int(capture.get(cv2.CAP_PROP_FRAME_HEIGHT))
    fps = float(capture.get(cv2.CAP_PROP_FPS))
    count = int(capture.get(cv2.CAP_PROP_FRAME_COUNT))
    if (width, height) != VIDEO_SIZE or abs(fps - 30.0) > 0.001 or count != RGB_FRAMES:
        raise ValueError(f"video must be exact 1920x1080/30fps/900; got {width}x{height}/{fps}/{count}")
    found: dict[int, np.ndarray] = {}
    index = 0
    while True:
        ok, frame = capture.read()
        if not ok:
            break
        if index in wanted:
            found[index] = cv2.cvtColor(frame, cv2.COLOR_BGR2GRAY)
        index += 1
    capture.release()
    if index != RGB_FRAMES or set(found) != wanted:
        raise ValueError("video decode did not yield every exact requested RGB frame")
    return found


def _linearize_depth(path: Path, projection: dict[str, Any], scale: float) -> np.ndarray:
    raw = np.fromfile(path, dtype="<f4")
    if raw.size != SOURCE_DEPTH_SIZE[0] * SOURCE_DEPTH_SIZE[1]:
        raise ValueError(f"{path.name}: raw D32 byte count is not tight native 1920x1080")
    raw = raw.reshape(SOURCE_DEPTH_SIZE[1], SOURCE_DEPTH_SIZE[0])
    if not np.all(np.isfinite(raw)) or np.any(raw < 0.0) or np.any(raw > 1.0):
        raise ValueError(f"{path.name}: raw D32 contains non-finite/out-of-range values")
    a = float(projection["proj_a"])
    b = float(projection["proj_b"])
    valid = raw != 0.0
    denom = raw.astype(np.float64) - a
    with np.errstate(divide="ignore", invalid="ignore", over="ignore"):
        depth = b / denom * scale
    valid &= np.isfinite(depth) & (depth > 0.0)
    spec = np.full(raw.shape, np.nan, dtype=np.float32)
    truncated = np.trunc(depth[valid] * 100.0) / 100.0
    spec[valid] = truncated.astype(np.float16).astype(np.float32)
    if int(np.count_nonzero(np.isfinite(spec))) < spec.size // 20:
        raise ValueError(f"{path.name}: fewer than 5% valid metric depth pixels")
    source_x = (3 * np.arange(DEPTH_SIZE[0], dtype=np.int64) + 1) // 2
    source_y = (3 * np.arange(DEPTH_SIZE[1], dtype=np.int64) + 1) // 2
    return spec[np.ix_(source_y, source_x)]


def _rotation_angle(left: np.ndarray, right: np.ndarray) -> float:
    rel = RelPose.between(np.zeros(3), left, np.zeros(3), right)
    return math.acos(float(np.clip((np.trace(rel.rotation) - 1.0) / 2.0, -1.0, 1.0)))


def _pair_populations(
    poses: dict[int, dict[str, Any]], frames: dict[int, dict[str, Any]], thresholds: Thresholds
) -> dict[str, list[tuple[int, int]]]:
    populations: dict[str, list[tuple[int, int]]] = {"static": [], "translation": [], "rotation": []}
    for frame in range(0, RGB_FRAMES - 2, 2):
        left, right = poses[frame], poses[frame + 2]
        translation = float(np.linalg.norm(_finite_vector(right["camera_position"], 3, "position") - _finite_vector(left["camera_position"], 3, "position")))
        rotation = _rotation_angle(_finite_vector(left["camera_rotation_quaternion"], 4, "quaternion"), _finite_vector(right["camera_rotation_quaternion"], 4, "quaternion"))
        if translation <= thresholds.max_static_translation_metres and rotation <= thresholds.max_static_rotation_radians:
            populations["static"].append((frame, frame + 2))
        if translation >= thresholds.min_translation_metres:
            populations["translation"].append((frame, frame + 2))
        if rotation >= thresholds.min_rotation_radians:
            populations["rotation"].append((frame, frame + 2))
    return populations


def _deterministic_sample(values: list[tuple[int, int]], count: int) -> list[tuple[int, int]]:
    if len(values) <= count:
        return values
    indexes = np.linspace(0, len(values) - 1, count, dtype=int)
    return [values[int(index)] for index in indexes]


def _percentile(values: list[float], q: float) -> float:
    return float(np.percentile(np.asarray(values, dtype=np.float64), q))


def _corr(left: list[float], right: list[float]) -> float | None:
    a, b = np.asarray(left, dtype=np.float64), np.asarray(right, dtype=np.float64)
    if len(a) < 3 or float(a.std()) < 1e-12 or float(b.std()) < 1e-12:
        return None
    return float(np.corrcoef(a, b)[0, 1])


def run_gate(
    *,
    session: Path,
    video: Path,
    raw_depth_index: Path,
    raw_depth_profile: Path,
    frame_evidence: Path,
    pose: Path,
    metric_evidence: Path,
    max_pairs: int = 120,
    thresholds: Thresholds = Thresholds(),
) -> dict[str, Any]:
    session = _canonical_session(session)
    video = _direct_file(session, video, "video")
    raw_depth_index = _direct_file(session, raw_depth_index, "raw depth index")
    raw_depth_profile = _direct_file(session, raw_depth_profile, "raw depth profile")
    frame_evidence = _direct_file(session, frame_evidence, "frame evidence")
    pose = _direct_file(session, pose, "pose")
    metric_evidence = _direct_file(session, metric_evidence, "metric evidence")
    frames_rows = _read_jsonl(frame_evidence, RGB_FRAMES)
    raw_rows = _read_jsonl(raw_depth_index, DEPTH_FRAMES)
    poses_rows = _read_jsonl(pose, RGB_FRAMES)
    metric = _read_json(metric_evidence)
    _validate_metric_report(session, metric)
    source_candidate_id = _validate_raw_profile(_read_json(raw_depth_profile))
    frames, raw, poses, scale = _validate_inputs(
        frames_rows, raw_rows, poses_rows, metric, source_candidate_id
    )
    bound_depth_files = _bind_all_depth_files(session, raw_rows)
    populations = _pair_populations(poses, frames, thresholds)
    per_population = max(1, max_pairs // 3)
    sampled_by_kind = {kind: _deterministic_sample(values, per_population) for kind, values in populations.items()}
    sampled_pairs = sorted({pair for values in sampled_by_kind.values() for pair in values})
    wanted = {frame for pair in sampled_pairs for frame in pair}
    rgb = _video_frames(video, wanted)
    import cv2

    improvements: list[float] = []
    inverse_improvements: list[float] = []
    valid_fracs: list[float] = []
    moving_baselines: list[float] = []
    static_baselines: list[float] = []
    pose_motion: list[float] = []
    invalid_metric_pairs = 0
    skipped_low_motion = 0
    consumed: dict[int, dict[str, Any]] = {}
    static_set = set(sampled_by_kind["static"])
    moving_set = set(sampled_by_kind["translation"]) | set(sampled_by_kind["rotation"])
    for frame_t, frame_tk in sampled_pairs:
        identity_t = _identity(frames[frame_t]["rgb"], "RGB t")
        identity_tk = _identity(frames[frame_tk]["rgb"], "RGB tk")
        pose_t, pose_tk = poses[frame_t], poses[frame_tk]
        raw_row = raw[identity_t]
        raw_path = _raw_depth_file(session, raw_row["file"])
        if raw_row["file"] != frames[frame_t]["depth"]["file"]:
            raise ValueError(f"session_frame={frame_t}: raw filename differs from exact depth binding")
        depth = _linearize_depth(raw_path, frames[frame_t]["projection"], scale)
        consumed[raw_row["ordinal"]] = {
            "ordinal": raw_row["ordinal"],
            "file": raw_row["file"],
            "present_seq": identity_t[0],
            "qpc": identity_t[1],
            "sha256": _sha256(raw_path),
        }
        gray_t, gray_tk = rgb[frame_t], rgb[frame_tk]
        if gray_t.shape != depth.shape:
            gray_t = cv2.resize(gray_t, DEPTH_SIZE, interpolation=cv2.INTER_AREA)
            gray_tk = cv2.resize(gray_tk, DEPTH_SIZE, interpolation=cv2.INTER_AREA)
        intrinsics = pose_t.get("camera_intrinsics")
        if not isinstance(intrinsics, dict):
            raise ValueError(f"session_frame={frame_t}: camera intrinsics missing")
        intr = Pinhole(
            float(intrinsics["fx"]) * DEPTH_SIZE[0] / VIDEO_SIZE[0],
            float(intrinsics["fy"]) * DEPTH_SIZE[1] / VIDEO_SIZE[1],
            float(intrinsics["cx"]) * DEPTH_SIZE[0] / VIDEO_SIZE[0],
            float(intrinsics["cy"]) * DEPTH_SIZE[1] / VIDEO_SIZE[1],
        )
        pos_t = _finite_vector(pose_t["camera_position"], 3, "camera position")
        pos_tk = _finite_vector(pose_tk["camera_position"], 3, "camera position")
        quat_t = _finite_vector(pose_t["camera_rotation_quaternion"], 4, "camera quaternion")
        quat_tk = _finite_vector(pose_tk["camera_rotation_quaternion"], 4, "camera quaternion")
        rel = RelPose.between(pos_t, quat_t, pos_tk, quat_tk)
        translation = float(np.linalg.norm(rel.translation))
        rotation = math.acos(float(np.clip((np.trace(rel.rotation) - 1.0) / 2.0, -1.0, 1.0)))
        if (frame_t, frame_tk) in static_set:
            mask = np.isfinite(depth)
            static_baselines.append(float(np.mean(np.abs(gray_tk[mask].astype(np.float64) - gray_t[mask].astype(np.float64)))))
        if (frame_t, frame_tk) not in moving_set:
            continue
        result = e_geo(gray_t, gray_tk, depth, intr, rel, -1.0)
        inverse = RelPose(rel.rotation.T, -(rel.rotation.T @ rel.translation))
        inverse_result = e_geo(gray_t, gray_tk, depth, intr, inverse, -1.0)
        if not math.isfinite(result.baseline_l1):
            invalid_metric_pairs += 1
            continue
        if result.baseline_l1 < thresholds.min_motion_baseline_l1:
            skipped_low_motion += 1
            continue
        if not all(math.isfinite(value) for value in (result.improvement, inverse_result.improvement, result.valid_frac)):
            invalid_metric_pairs += 1
            continue
        improvements.append(float(result.improvement))
        inverse_improvements.append(float(inverse_result.improvement))
        valid_fracs.append(float(result.valid_frac))
        moving_baselines.append(float(result.baseline_l1))
        pose_motion.append(translation + rotation)

    failures: list[str] = []
    for kind, minimum in (
        ("static", thresholds.min_static_pairs),
        ("translation", thresholds.min_translation_pairs),
        ("rotation", thresholds.min_rotation_pairs),
    ):
        if len(sampled_by_kind[kind]) < minimum:
            failures.append(f"{kind}_pairs={len(sampled_by_kind[kind])} < {minimum}")
    if invalid_metric_pairs:
        failures.append(f"invalid_metric_pairs={invalid_metric_pairs} != 0")
    if len(improvements) < thresholds.min_usable_motion_pairs:
        failures.append(f"usable_motion_pairs={len(improvements)} < {thresholds.min_usable_motion_pairs}")
    if improvements:
        valid_p10 = _percentile(valid_fracs, 10)
        improvement_p50 = _percentile(improvements, 50)
        inverse_p50 = _percentile(inverse_improvements, 50)
        improved_share = float(np.mean(np.asarray(improvements) > 1.0))
        moving_baseline_p50 = _percentile(moving_baselines, 50)
        static_baseline_p50 = _percentile(static_baselines, 50) if static_baselines else None
        if valid_p10 < thresholds.min_valid_frac_p10:
            failures.append(f"valid_frac_p10={valid_p10:.6f} < {thresholds.min_valid_frac_p10}")
        if improvement_p50 < thresholds.min_improvement_p50:
            failures.append(f"improvement_p50={improvement_p50:.6f} < {thresholds.min_improvement_p50}")
        if improved_share < thresholds.min_improved_share:
            failures.append(f"improved_share={improved_share:.6f} < {thresholds.min_improved_share}")
        if improvement_p50 < inverse_p50 * thresholds.min_inverse_separation:
            failures.append("formal camera convention does not separate from the inverse convention")
        if static_baseline_p50 is not None and static_baseline_p50 >= moving_baseline_p50:
            failures.append("static RGB motion is not below the moving control population")
    else:
        valid_p10 = improvement_p50 = inverse_p50 = improved_share = moving_baseline_p50 = 0.0
        static_baseline_p50 = _percentile(static_baselines, 50) if static_baselines else None

    passed = not failures
    module = Path(__file__).resolve()
    geo_module = module.with_name("geo_probes.py")
    return {
        "schema": REPORT_SCHEMA,
        "non_trainable": True,
        "local_only": True,
        "upload_allowed": False,
        "e_geo": {
            "schema": E_GEO_SCHEMA,
            "passed": passed,
            "ready_for_independent_repeat": passed,
            "production_binding_authorized": PRODUCTION_BINDING_AUTHORIZED,
            "failures": failures,
            "video_sha256": _sha256(video),
            "raw_depth_index_sha256": _sha256(raw_depth_index),
            "raw_depth_profile_sha256": _sha256(raw_depth_profile),
            "frame_evidence_sha256": _sha256(frame_evidence),
            "pose_sha256": _sha256(pose),
            "metric_evidence_sha256": _sha256(metric_evidence),
            "tool_sha256": _sha256(module),
            "geo_probes_sha256": _sha256(geo_module),
            "source_depth_geometry": {
                "width": SOURCE_DEPTH_SIZE[0],
                "height": SOURCE_DEPTH_SIZE[1],
                "resource_format": 19,
                "dsv_format": 20,
                "sample_count": 1,
                "extracted_pixel_format": "DXGI_FORMAT_D32_FLOAT_LE",
            },
            "output_depth_geometry": {"width": DEPTH_SIZE[0], "height": DEPTH_SIZE[1]},
            "resampling": {
                "method": "pixel_center_nearest_2_over_3",
                "integer_mapping": "src=floor((3*dst+1)/2)",
                "intrinsics_scaling": "fx_fy_cx_cy_times_2_over_3",
            },
            "thresholds": asdict(thresholds),
            "eligible_pairs_total": {kind: len(values) for kind, values in populations.items()},
            "sampled_pairs": {kind: len(values) for kind, values in sampled_by_kind.items()},
            "depth_byte_binding": "all_450_indexed_depth_files",
            "bound_depth_files": bound_depth_files,
            "usable_motion_pairs": len(improvements),
            "skipped_low_motion": skipped_low_motion,
            "invalid_metric_pairs": invalid_metric_pairs,
            "consumed_depth_files": [consumed[key] for key in sorted(consumed)],
            "metrics": {
                "improvement_p10": _percentile(improvements, 10) if improvements else None,
                "improvement_p50": improvement_p50 if improvements else None,
                "improvement_p90": _percentile(improvements, 90) if improvements else None,
                "inverse_improvement_p50": inverse_p50 if improvements else None,
                "improved_share": improved_share if improvements else None,
                "valid_frac_p10": valid_p10 if improvements else None,
                "valid_frac_p50": _percentile(valid_fracs, 50) if improvements else None,
                "static_baseline_l1_p50": static_baseline_p50,
                "moving_baseline_l1_p50": moving_baseline_p50 if improvements else None,
                "pose_image_motion_correlation": _corr(pose_motion, moving_baselines),
            },
        },
    }


def _atomic_write(path: Path, report: dict[str, Any]) -> None:
    if path.exists():
        raise ValueError("output report already exists")
    staging = path.with_name(f".{path.name}.staging-{os.getpid()}")
    if staging.exists():
        raise ValueError("staging report already exists")
    data = (json.dumps(report, indent=2, ensure_ascii=False) + "\n").encode("utf-8")
    try:
        with staging.open("xb") as handle:
            handle.write(data)
            handle.flush()
            os.fsync(handle.fileno())
        if path.exists():
            raise ValueError("output report appeared before publish")
        staging.replace(path)
    except Exception:
        staging.unlink(missing_ok=True)
        raise


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--session", type=Path, required=True)
    parser.add_argument("--video", type=Path, required=True)
    parser.add_argument("--raw-depth-index", type=Path, required=True)
    parser.add_argument("--raw-depth-profile", type=Path, required=True)
    parser.add_argument("--frame-evidence", type=Path, required=True)
    parser.add_argument("--pose", type=Path, required=True)
    parser.add_argument("--metric-evidence", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--max-pairs", type=int, default=120)
    args = parser.parse_args()
    try:
        session = _canonical_session(args.session)
        if not args.out.is_absolute() or args.out.parent.resolve(strict=True) != session:
            raise ValueError("output must be an absolute new direct child of the session")
        if args.max_pairs < 30 or args.max_pairs > 449:
            raise ValueError("max-pairs must be within [30,449]")
        report = run_gate(
            session=session,
            video=args.video,
            raw_depth_index=args.raw_depth_index,
            raw_depth_profile=args.raw_depth_profile,
            frame_evidence=args.frame_evidence,
            pose=args.pose,
            metric_evidence=args.metric_evidence,
            max_pairs=args.max_pairs,
        )
    except Exception as error:
        report = {
            "schema": REPORT_SCHEMA,
            "non_trainable": True,
            "local_only": True,
            "upload_allowed": False,
            "e_geo": {
                "schema": E_GEO_SCHEMA,
                "passed": False,
                "production_binding_authorized": False,
                "tool_error": str(error),
            },
        }
        try:
            _atomic_write(args.out, report)
        except Exception as write_error:
            print(f"E-geo tool error: {error}; report write failed: {write_error}")
            return 2
        print(f"E-geo tool error: {error}")
        return 2
    _atomic_write(args.out, report)
    print(json.dumps(report["e_geo"]["metrics"], ensure_ascii=False))
    print(f"CP2077 E-geo {'PASS' if report['e_geo']['passed'] else 'FAIL'}: {args.out}")
    return 0 if report["e_geo"]["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())

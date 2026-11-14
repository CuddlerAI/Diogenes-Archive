"""Assemble measured GTA native projections with their sealed rendered frames.

The native producer must run GET_SCREEN_COORD_FROM_WORLD_COORD on the game
thread and publish its observations for the same final-camera generation and
capture request as the PNG. This module never manufactures screen coordinates
from camera intrinsics, FOV, or our projection function. It only converts GTA's
normalized screen coordinates to pixels, verifies the frame/PNG bindings, and
runs the existing solver as a diagnostic. Outputs remain unapproved.

Expected extension on a native ``dm-gta-frame/1`` row::

    "calibration_projection": {
      "method": "GET_SCREEN_COORD_FROM_WORLD_COORD",
      "native_hash": "0x34E82F05DF2974F5",
      "capture_request_id": 1,
      "final_camera_generation": 1,
      "points": [{"id": "grid-0", "world": [1, 10, 2],
                  "screen_normalized": [0.55, 0.45], "visible": true}]
    }

PNG identity proves which image accompanies the measurements. It does not
claim that image content has independently verified native projection: rendered
marker inspection is still a separate observation and is reported as pending.
"""

from __future__ import annotations

import math
from datetime import datetime
from pathlib import Path
import struct
from typing import Sequence

from .calibration import REQUIRED_FOCALS, SAMPLES_SCHEMA, solve_projection
from .errors import ContractError
from .evidence import validate_pixel_pose_fence
from .jsonio import read_jsonl, sha256_file


PROJECTION_NATIVE = "GET_SCREEN_COORD_FROM_WORLD_COORD"
PROJECTION_NATIVE_HASH = "0x34E82F05DF2974F5"
MIN_TOTAL_SAMPLES = 100
MAX_RMS_PX = 1.0
FINAL_FENCE = "present_pixel_ready_final_writer_generation_pose_release_ack"


def _vec(value, size: int, label: str) -> list[float]:
    if (not isinstance(value, (list, tuple)) or len(value) != size
            or any(not isinstance(v, (int, float)) or isinstance(v, bool)
                   or not math.isfinite(float(v)) for v in value)):
        raise ContractError(f"{label} must be a finite vec{size}")
    return [float(v) for v in value]


def _positive_int(value, label: str) -> int:
    if not isinstance(value, int) or isinstance(value, bool) or value < 1:
        raise ContractError(f"{label} must be a positive integer")
    return value


def _png_size(path: Path) -> tuple[int, int]:
    with path.open("rb") as handle:
        header = handle.read(24)
    if len(header) != 24 or header[:8] != b"\x89PNG\r\n\x1a\n" or header[12:16] != b"IHDR":
        raise ContractError("calibration image must be a native PNG")
    width, height = struct.unpack(">II", header[16:24])
    if width < 1 or height < 1:
        raise ContractError("calibration image has invalid dimensions")
    return width, height


def samples_from_native_frames(frame_paths: Sequence[Path], *, game_build: str,
                               captured_at: str, graphics_api: str = "DX11"
                               ) -> tuple[dict, dict]:
    """Return (samples document, unapproved diagnostic report), or fail closed.

    At least 100 distinct observations over all four required focal groups must
    survive the existing coverage/depth/filmback checks and <=1 px RMS. Raw frame
    rows without calibration observations are skipped, never reverse-projected.
    """
    if (not isinstance(game_build, str) or not game_build.strip()
            or not isinstance(captured_at, str) or not captured_at.strip()
            or graphics_api != "DX11"):
        raise ContractError("calibration requires build, capture time, and DX11")
    try:
        capture_time = datetime.fromisoformat(captured_at.replace("Z", "+00:00"))
    except ValueError as exc:
        raise ContractError("calibration capture time must use ISO8601") from exc
    if capture_time.utcoffset() is None:
        raise ContractError("calibration capture time must include its UTC offset")
    samples, sources = [], []
    dimensions = None
    image_cache: dict[Path, tuple[str, tuple[int, int], int]] = {}
    seen_observations = set()
    lanes: dict[int, list[dict]] = {}
    for raw_path in frame_paths:
        path = Path(raw_path).resolve()
        source_hash = sha256_file(path)
        rows = read_jsonl(path)
        used_rows = 0
        for index, row in enumerate(rows):
            lane = row.get("lane")
            if not isinstance(lane, int) or isinstance(lane, bool) or lane not in (1, 2, 3, 4):
                raise ContractError("native calibration frame stream must identify lane 1..4")
            lanes.setdefault(lane, []).append(row)
            observation = row.get("calibration_projection")
            if observation is None:
                continue
            if (row.get("schema") != "dm-gta-frame/1" or row.get("mock") is not False
                    or row.get("render_succeeded") is not True
                    or row.get("capture_protocol") != 18
                    or row.get("editor_identity_protocol") != 4
                    or row.get("capture_fence") != FINAL_FENCE):
                raise ContractError("calibration needs a real v18 sealed native frame")
            if (not isinstance(observation, dict)
                    or observation.get("method") != PROJECTION_NATIVE
                    or not isinstance(observation.get("native_hash"), str)
                    or observation.get("native_hash", "").upper() != PROJECTION_NATIVE_HASH.upper()):
                raise ContractError("calibration pixels must come from the GTA projection native")
            binding = row.get("editor_binding")
            if not isinstance(binding, dict):
                raise ContractError("calibration frame is missing its editor binding")
            request_id = _positive_int(binding.get("capture_request_id"), "capture request")
            generation = _positive_int(binding.get("final_camera_generation"), "final camera generation")
            measured_request = _positive_int(observation.get("capture_request_id"),
                                             "projection capture request")
            measured_generation = _positive_int(observation.get("final_camera_generation"),
                                                "projection camera generation")
            pixel_generation = _positive_int(row.get("pixel_ready_final_camera_generation"),
                                              "pixel-ready camera generation")
            if (measured_request != request_id or measured_generation != generation
                    or pixel_generation != generation):
                raise ContractError("calibration projection and rendered frame generations differ")
            if not isinstance(row.get("output_path"), str) or not row["output_path"]:
                raise ContractError("calibration frame has no native PNG path")
            image_path = Path(row["output_path"])
            if not image_path.is_absolute() or not image_path.is_file():
                raise ContractError("calibration frame has no available absolute native PNG path")
            image_path = image_path.resolve()
            if image_path not in image_cache:
                image_cache[image_path] = (sha256_file(image_path), _png_size(image_path),
                                           image_path.stat().st_size)
            image_hash, size, byte_count = image_cache[image_path]
            if row.get("output_sha256") != image_hash or row.get("output_bytes") != byte_count:
                raise ContractError("calibration PNG hash/size differs from the sealed native frame")
            if dimensions is not None and dimensions != size:
                raise ContractError("calibration images have different render dimensions")
            dimensions = size
            width, height = size
            camera = row.get("final_camera")
            if not isinstance(camera, dict) or camera.get("focal_mm") not in REQUIRED_FOCALS:
                raise ContractError("calibration frame must declare 18, 24, 35, or 50 mm")
            focal = float(camera["focal_mm"])
            position = _vec(camera.get("position"), 3, "final camera position")
            quaternion = _vec(camera.get("quaternion"), 4, "final camera quaternion")
            points = observation.get("points")
            if not isinstance(points, list) or not points:
                raise ContractError("native calibration observation contains no points")
            ids = set()
            for point in points:
                if not isinstance(point, dict) or point.get("visible") is not True:
                    raise ContractError("calibration native did not observe a visible point")
                point_id = point.get("id")
                if not isinstance(point_id, str) or not point_id or point_id in ids:
                    raise ContractError("calibration point ids must be non-empty and unique per frame")
                ids.add(point_id)
                world = _vec(point.get("world"), 3, "measured world point")
                normalized = _vec(point.get("screen_normalized"), 2, "native screen coordinate")
                if not all(0.0 <= value <= 1.0 for value in normalized):
                    raise ContractError("native calibration point is outside the rendered frame")
                key = (focal, *position, *quaternion, *world)
                if key in seen_observations:
                    raise ContractError("duplicate calibration observation cannot increase sample count")
                seen_observations.add(key)
                samples.append({
                    "focal_mm": focal,
                    "world": world,
                    "pixel": [normalized[0] * width, normalized[1] * height],
                    "camera": {"position": position, "quaternion": quaternion},
                    "provenance": {
                        "frame_events_sha256": source_hash,
                        "frame_row_index": index,
                        "lane": lane,
                        "output_frame": row.get("output_frame"),
                        "editor_session_id": binding.get("session_id"),
                        "output_sha256": image_hash,
                        "capture_request_id": request_id,
                        "final_camera_generation": generation,
                        "point_id": point_id,
                        "pixel_source": PROJECTION_NATIVE,
                    },
                })
            used_rows += 1
        if sha256_file(path) != source_hash:
            raise ContractError("native frame events changed during calibration assembly")
        sources.append({"path": str(path), "sha256": source_hash, "used_rows": used_rows})
    counts = {focal: sum(row["focal_mm"] == focal for row in samples) for focal in REQUIRED_FOCALS}
    if any(count < 8 for count in counts.values()) or len(samples) < MIN_TOTAL_SAMPLES:
        raise ContractError(
            f"calibration requires all four focal groups, >=8 each and >=100 total; got {counts}")
    # Use the actual export seal validator, including replay/profile/root identity,
    # unique requests, monotonic generations, and a fresh re-hash of every PNG.
    # Merely matching a protocol string and a self-declared request is insufficient.
    replay_sha256 = lanes.get(1, [{}])[0].get("replay_sha256")
    fence_errors = validate_pixel_pose_fence(lanes, replay_sha256)
    if fence_errors:
        raise ContractError("native calibration frame seal failed: " + "; ".join(fence_errors))
    for source in sources:
        if sha256_file(source["path"]) != source["sha256"]:
            raise ContractError("native frame events changed during calibration assembly")
    width, height = dimensions
    solution = solve_projection(samples, width=width, height=height)
    if solution["reprojection"]["error_px"] > MAX_RMS_PX:
        raise ContractError("measured native projection RMS exceeds the 1 px calibration target")
    document = {
        "schema": SAMPLES_SCHEMA,
        "game_build": game_build,
        "graphics_api": graphics_api,
        "captured_at": captured_at,
        "method": "GTA GET_SCREEN_COORD_FROM_WORLD_COORD; v18 request/generation and PNG hash bound",
        "render": {"width": width, "height": height},
        "sources": sources,
        "samples": samples,
        "approval_status": "draft",
        "render_marker_crosscheck": "pending; PNG hash binding does not inspect marker pixels",
        "producer_status": "observations supplied by caller; this tool does not acquire native measurements",
    }
    report = {
        "schema": "dm-gta-calibration-sampling-report/1",
        "approval_status": "draft",
        "source_frame_files": len(sources),
        "source_png_count": len(image_cache),
        "sample_count": len(samples),
        "per_focal_sample_count": {str(int(k)): v for k, v in counts.items()},
        "solution": solution,
        "render_marker_crosscheck": document["render_marker_crosscheck"],
        "producer_status": document["producer_status"],
        "metric_scale_fitted": False,
        "delivery_eligible": False,
    }
    return document, report

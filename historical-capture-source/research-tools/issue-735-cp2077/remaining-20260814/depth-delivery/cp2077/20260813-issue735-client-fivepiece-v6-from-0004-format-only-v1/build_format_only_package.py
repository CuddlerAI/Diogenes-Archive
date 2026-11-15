#!/usr/bin/env python3
"""Build and self-check a PR405/PR749-shaped, explicitly non-trainable package.

This script intentionally does not certify the source data.  It records every
retiming, interpolation, and unresolved metric assumption in DERIVATION.json.
"""

from __future__ import annotations

import argparse
import bisect
import functools
import hashlib
import json
import math
import os
import re
import shutil
import subprocess
import sys
import tempfile
import zipfile
from datetime import datetime, timedelta
from pathlib import Path, PurePosixPath

import cv2
import Imath
import numpy as np
import OpenEXR


FPS = 30
SOURCE_START_FRAME = 120
SOURCE_END_EXCLUSIVE = 1846
SOURCE_FRAME_COUNT = SOURCE_END_EXCLUSIVE - SOURCE_START_FRAME
OUTPUT_FRAME_COUNT = 1800
OUTPUT_DEPTH_COUNT = 900
SOURCE_W, SOURCE_H = 1920, 1080
DEPTH_W, DEPTH_H = 1280, 720
TOP_INVALID_ROWS = 6
METRIC_SCALE_ASSUMPTION = 1.0


def log(message: str) -> None:
    print(message, flush=True)


def run(command: list[str], *, stdout_path: Path | None = None) -> None:
    log("RUN " + " ".join(str(x) for x in command[:8]) + (" ..." if len(command) > 8 else ""))
    if stdout_path is None:
        subprocess.run(command, check=True)
    else:
        with stdout_path.open("wb") as handle:
            subprocess.run(command, check=True, stdout=handle)


def round_list(values: np.ndarray | list[float], digits: int = 6) -> list[float]:
    return [round(float(value), digits) for value in values]


def normalize_quaternion(value: np.ndarray) -> np.ndarray:
    value = np.asarray(value, dtype=np.float64)
    norm = float(np.linalg.norm(value))
    if not math.isfinite(norm) or norm < 1e-12:
        return np.array([0.0, 0.0, 0.0, 1.0], dtype=np.float64)
    return value / norm


def slerp(a: np.ndarray, b: np.ndarray, alpha: float) -> np.ndarray:
    qa = normalize_quaternion(a)
    qb = normalize_quaternion(b)
    dot = float(np.dot(qa, qb))
    if dot < 0.0:
        qb = -qb
        dot = -dot
    dot = float(np.clip(dot, -1.0, 1.0))
    if dot > 0.9995:
        return normalize_quaternion(qa + alpha * (qb - qa))
    theta = math.acos(dot)
    sin_theta = math.sin(theta)
    return normalize_quaternion(
        math.sin((1.0 - alpha) * theta) / sin_theta * qa
        + math.sin(alpha * theta) / sin_theta * qb
    )


def rotation_to_quaternion(matrix: np.ndarray) -> np.ndarray:
    """Return an x,y,z,w quaternion for a proper 3x3 rotation."""
    m = np.asarray(matrix, dtype=np.float64)
    trace = float(np.trace(m))
    if trace > 0.0:
        s = math.sqrt(trace + 1.0) * 2.0
        q = np.array([
            (m[2, 1] - m[1, 2]) / s,
            (m[0, 2] - m[2, 0]) / s,
            (m[1, 0] - m[0, 1]) / s,
            0.25 * s,
        ])
    else:
        i = int(np.argmax(np.diag(m)))
        if i == 0:
            s = math.sqrt(max(1e-12, 1.0 + m[0, 0] - m[1, 1] - m[2, 2])) * 2.0
            q = np.array([0.25 * s, (m[0, 1] + m[1, 0]) / s,
                          (m[0, 2] + m[2, 0]) / s, (m[2, 1] - m[1, 2]) / s])
        elif i == 1:
            s = math.sqrt(max(1e-12, 1.0 + m[1, 1] - m[0, 0] - m[2, 2])) * 2.0
            q = np.array([(m[0, 1] + m[1, 0]) / s, 0.25 * s,
                          (m[1, 2] + m[2, 1]) / s, (m[0, 2] - m[2, 0]) / s])
        else:
            s = math.sqrt(max(1e-12, 1.0 + m[2, 2] - m[0, 0] - m[1, 1])) * 2.0
            q = np.array([(m[0, 2] + m[2, 0]) / s,
                          (m[1, 2] + m[2, 1]) / s, 0.25 * s,
                          (m[1, 0] - m[0, 1]) / s])
    return normalize_quaternion(q)


def nearest_proper_rotation(matrix: np.ndarray) -> np.ndarray:
    u, _, vt = np.linalg.svd(np.asarray(matrix, dtype=np.float64))
    result = u @ vt
    if np.linalg.det(result) < 0:
        u[:, -1] *= -1
        result = u @ vt
    return result


def source_float_frame(output_frame: int) -> float:
    return SOURCE_START_FRAME + output_frame * SOURCE_FRAME_COUNT / OUTPUT_FRAME_COUNT


def interpolate_vectors(values: np.ndarray, index: float) -> np.ndarray:
    lo = int(math.floor(index))
    hi = min(lo + 1, len(values) - 1)
    alpha = index - lo
    return (1.0 - alpha) * values[lo] + alpha * values[hi]


def interpolate_quaternions(values: np.ndarray, index: float) -> np.ndarray:
    lo = int(math.floor(index))
    hi = min(lo + 1, len(values) - 1)
    return slerp(values[lo], values[hi], index - lo)


def fill_camera_series(source_rows: list[dict]) -> tuple[np.ndarray, np.ndarray, np.ndarray, float, int]:
    n = len(source_rows)
    positions = np.full((n, 3), np.nan, dtype=np.float64)
    rotations = np.full((n, 4), np.nan, dtype=np.float64)
    intrinsics = np.full((n, 4), np.nan, dtype=np.float64)
    near_values: list[float] = []
    valid = 0
    for i, row in enumerate(source_rows):
        try:
            view = np.asarray(row["camera_view_matrix"], dtype=np.float64)
            projection = np.asarray(row["camera_projection_matrix"], dtype=np.float64)
            if view.shape != (4, 4) or projection.shape != (4, 4):
                continue
            if not np.all(np.isfinite(view)) or not np.all(np.isfinite(projection)):
                continue
            basis = view[:3, :3]
            positions[i] = -view[3, :3] @ basis.T
            # The captured game view has a reflected basis.  Project its inverse
            # to SO(3), then emit the required world-to-camera quaternion.
            camera_to_world = nearest_proper_rotation(basis.T)
            rotations[i] = rotation_to_quaternion(camera_to_world.T)
            intrinsics[i] = [projection[0, 0] * SOURCE_W / 2.0,
                             projection[1, 1] * SOURCE_H / 2.0,
                             SOURCE_W / 2.0, SOURCE_H / 2.0]
            near = abs(float(projection[3, 2]))
            if math.isfinite(near) and near > 0:
                near_values.append(near)
            valid += 1
        except (KeyError, TypeError, ValueError, np.linalg.LinAlgError):
            continue
    indices = np.flatnonzero(np.isfinite(positions[:, 0]))
    if len(indices) == 0:
        raise RuntimeError("no usable camera view/projection matrices")
    all_indices = np.arange(n)
    for axis in range(3):
        positions[:, axis] = np.interp(all_indices, indices, positions[indices, axis])
    for axis in range(4):
        intrinsics[:, axis] = np.interp(all_indices, indices, intrinsics[indices, axis])
    # Quaternion interpolation is done segment-wise to preserve normalization.
    filled_rotations = np.empty_like(rotations)
    for i in range(n):
        insert = bisect.bisect_left(indices, i)
        if insert == 0:
            filled_rotations[i] = rotations[indices[0]]
        elif insert == len(indices):
            filled_rotations[i] = rotations[indices[-1]]
        else:
            lo, hi = int(indices[insert - 1]), int(indices[insert])
            filled_rotations[i] = slerp(rotations[lo], rotations[hi], (i - lo) / (hi - lo))
    near = float(np.median(near_values)) if near_values else 0.020000024
    return positions, filled_rotations, intrinsics, near, valid


def build_actions(source_path: Path, package_dir: Path) -> dict:
    source_rows = json.loads(source_path.read_text(encoding="utf-8"))
    if len(source_rows) < SOURCE_END_EXCLUSIVE:
        raise RuntimeError(f"action source has only {len(source_rows)} rows")
    camera_pos, camera_quat, camera_intr, near, valid_camera = fill_camera_series(source_rows)
    player_pos = np.asarray([row["player_position_world_units"] for row in source_rows], dtype=np.float64)
    player_quat = np.asarray([normalize_quaternion(row["player_quaternion_ijkr"])
                              for row in source_rows], dtype=np.float64)

    out_camera_pos = np.asarray([interpolate_vectors(camera_pos, source_float_frame(f))
                                 for f in range(OUTPUT_FRAME_COUNT)])
    out_camera_quat = np.asarray([interpolate_quaternions(camera_quat, source_float_frame(f))
                                  for f in range(OUTPUT_FRAME_COUNT)])
    out_camera_intr = np.asarray([interpolate_vectors(camera_intr, source_float_frame(f))
                                  for f in range(OUTPUT_FRAME_COUNT)])
    out_player_pos = np.asarray([interpolate_vectors(player_pos, source_float_frame(f))
                                 for f in range(OUTPUT_FRAME_COUNT)])
    out_player_quat = np.asarray([interpolate_quaternions(player_quat, source_float_frame(f))
                                  for f in range(OUTPUT_FRAME_COUNT)])

    camera_speed = np.zeros(OUTPUT_FRAME_COUNT, dtype=np.float64)
    player_speed = np.zeros(OUTPUT_FRAME_COUNT, dtype=np.float64)
    camera_speed[1:] = np.linalg.norm(np.diff(out_camera_pos, axis=0), axis=1) * FPS
    player_speed[1:] = np.linalg.norm(np.diff(out_player_pos, axis=0), axis=1) * FPS
    base_time = datetime(2026, 8, 13, 21, 0, 9)
    rows30: list[dict] = []
    for frame in range(OUTPUT_FRAME_COUNT):
        intr = out_camera_intr[frame]
        row = {
            "time": (base_time + timedelta(seconds=frame / FPS)).strftime("%Y-%m-%d %H:%M:%S.%f")[:-3],
            "fps": 30,
            "frame": frame,
            "camera_position": round_list(out_camera_pos[frame]),
            "camera_rotation_quaternion": round_list(out_camera_quat[frame]),
            "camera_follow_offset": round_list(out_camera_pos[frame] - out_player_pos[frame]),
            "camera_intrinsics": {"fx": round(float(intr[0]), 6), "fy": round(float(intr[1]), 6),
                                  "cx": round(float(intr[2]), 6), "cy": round(float(intr[3]), 6)},
            "camera_speed": round(float(camera_speed[frame]), 6),
            "player_position": round_list(out_player_pos[frame]),
            "mouseX": 0.0,
            "mouseY": 0.0,
            "mouse_dx": 0.0,
            "mouse_dy": 0.0,
            "keyCode": "",
            "player_rotation_quaternion": round_list(out_player_quat[frame]),
            "player_speed": round(float(player_speed[frame]), 6),
            "metric_scale": METRIC_SCALE_ASSUMPTION,
        }
        rows30.append(row)
    rows15: list[dict] = []
    for frame in range(0, OUTPUT_FRAME_COUNT, 2):
        source = rows30[frame]
        row = {"time": source["time"], "fps": source["fps"],
               "depth_name": f"depth_{frame}", "frame": frame}
        for key, value in source.items():
            if key not in {"time", "fps", "frame"}:
                row[key] = value
        rows15.append(row)
    (package_dir / "action_camera_30fps.json").write_text(
        json.dumps(rows30, ensure_ascii=False, separators=(",", ":")), encoding="utf-8")
    (package_dir / "action_camera_15fps.json").write_text(
        json.dumps(rows15, ensure_ascii=False, separators=(",", ":")), encoding="utf-8")
    return {
        "source_action_rows": len(source_rows),
        "camera_matrix_rows_used": valid_camera,
        "near_constant_from_projection": near,
        "output_30fps_rows": len(rows30),
        "output_15fps_rows": len(rows15),
        "camera_pose_note": "reflected captured view basis projected to nearest SO(3); formal axis/geometric gate unresolved",
        "input_note": "mouse/key fields unavailable in source; emitted neutral values under Loop input exemption",
    }


def render_video(source_video: Path, raw_audio_zst: Path, destination: Path, work_dir: Path) -> None:
    pcm = work_dir / "audio.pcm"
    run(["zstd", "-d", "-f", str(raw_audio_zst), "-o", str(pcm)])
    ratio = SOURCE_FRAME_COUNT / OUTPUT_FRAME_COUNT
    duration_end = SOURCE_END_EXCLUSIVE / FPS
    filter_complex = (
        f"[0:v]trim=start_frame={SOURCE_START_FRAME}:end_frame={SOURCE_END_EXCLUSIVE},"
        "setpts=PTS-STARTPTS,split=2[base][patchsrc];"
        "[patchsrc]crop=192:8:0:8[patch];"
        f"[base][patch]overlay=0:0,setpts={OUTPUT_FRAME_COUNT / SOURCE_FRAME_COUNT:.12f}*PTS,"
        "fps=30[v];"
        f"[1:a]atrim=start={SOURCE_START_FRAME / FPS:.12f}:end={duration_end:.12f},"
        f"asetpts=PTS-STARTPTS,atempo={ratio:.12f}[a]"
    )
    run([
        "ffmpeg", "-hide_banner", "-y", "-i", str(source_video),
        "-f", "s16le", "-ar", "48000", "-ac", "2", "-i", str(pcm),
        "-filter_complex", filter_complex, "-map", "[v]", "-map", "[a]",
        "-frames:v", str(OUTPUT_FRAME_COUNT), "-t", "60", "-r", "30",
        "-c:v", "libx264", "-preset", "medium", "-crf", "16", "-profile:v", "high",
        "-pix_fmt", "yuv420p", "-c:a", "aac", "-b:a", "192k", "-ar", "48000",
        "-movflags", "+faststart", str(destination),
    ])
    pcm.unlink(missing_ok=True)


def write_half_exr(path: Path, depth: np.ndarray) -> None:
    header = OpenEXR.Header(DEPTH_W, DEPTH_H)
    header["compression"] = Imath.Compression(Imath.Compression.ZIP_COMPRESSION)
    header["channels"] = {"Y": Imath.Channel(Imath.PixelType(Imath.PixelType.HALF))}
    output = OpenEXR.OutputFile(str(path), header)
    try:
        output.writePixels({"Y": np.ascontiguousarray(depth, dtype=np.float16).tobytes()})
    finally:
        output.close()


def build_depth(source_zip: Path, destination: Path, work_dir: Path, near: float) -> dict:
    staging = destination.with_suffix(destination.suffix + ".staging")
    exr_temp = work_dir / "current.exr"
    exact_source_hit = 0
    temporal_interpolated = 0
    edge_held = 0
    created = 0
    with zipfile.ZipFile(source_zip, "r") as source:
        name_by_frame: dict[int, str] = {}
        for name in source.namelist():
            match = re.fullmatch(r"depth_(\d+)\.d32", PurePosixPath(name).name, flags=re.IGNORECASE)
            if match:
                frame = int(match.group(1))
                if SOURCE_START_FRAME <= frame < SOURCE_END_EXCLUSIVE:
                    name_by_frame[frame] = name
        frames = sorted(name_by_frame)
        if not frames:
            raise RuntimeError("no source D32 frames in requested clip")
        expected_source_even = set(range(SOURCE_START_FRAME, SOURCE_END_EXCLUSIVE, 2))
        missing_source_even = sorted(expected_source_even - set(frames))

        @functools.lru_cache(maxsize=6)
        def decoded(frame: int) -> np.ndarray:
            raw = source.read(name_by_frame[frame])
            expected_bytes = SOURCE_W * SOURCE_H * 4
            if len(raw) != expected_bytes:
                raise RuntimeError(f"depth_{frame}.d32 has {len(raw)} bytes, expected {expected_bytes}")
            reverse_z = np.frombuffer(raw, dtype="<f4").reshape(SOURCE_H, SOURCE_W)
            valid = np.isfinite(reverse_z) & (reverse_z > 0.0)
            linear = np.full(reverse_z.shape, np.nan, dtype=np.float32)
            with np.errstate(divide="ignore", invalid="ignore", over="ignore"):
                linear[valid] = near / reverse_z[valid] * METRIC_SCALE_ASSUMPTION
            linear[~np.isfinite(linear) | (linear <= 0.0) | (linear > np.finfo(np.float16).max)] = np.nan
            return cv2.resize(linear, (DEPTH_W, DEPTH_H), interpolation=cv2.INTER_NEAREST)

        with zipfile.ZipFile(staging, "w", compression=zipfile.ZIP_STORED, allowZip64=True) as output:
            for output_frame in range(0, OUTPUT_FRAME_COUNT, 2):
                sf = source_float_frame(output_frame)
                insert = bisect.bisect_left(frames, sf)
                if insert < len(frames) and abs(frames[insert] - sf) < 1e-9:
                    lo = hi = frames[insert]
                    exact_source_hit += 1
                elif insert == 0:
                    lo = hi = frames[0]
                    edge_held += 1
                elif insert == len(frames):
                    lo = hi = frames[-1]
                    edge_held += 1
                else:
                    lo, hi = frames[insert - 1], frames[insert]
                    temporal_interpolated += 1
                if lo == hi:
                    depth = decoded(lo).copy()
                else:
                    alpha = (sf - lo) / (hi - lo)
                    depth_lo = decoded(lo)
                    depth_hi = decoded(hi)
                    depth = np.full(depth_lo.shape, np.nan, dtype=np.float32)
                    finite_lo = np.isfinite(depth_lo)
                    finite_hi = np.isfinite(depth_hi)
                    both = finite_lo & finite_hi
                    depth[both] = (1.0 - alpha) * depth_lo[both] + alpha * depth_hi[both]
                    depth[finite_lo & ~finite_hi] = depth_lo[finite_lo & ~finite_hi]
                    depth[finite_hi & ~finite_lo] = depth_hi[finite_hi & ~finite_lo]
                depth[:TOP_INVALID_ROWS, :] = np.nan
                finite = np.isfinite(depth)
                depth[finite] = np.trunc(depth[finite] * 100.0) / 100.0
                half = depth.astype(np.float16)
                write_half_exr(exr_temp, half)
                output.write(exr_temp, arcname=f"depth_{output_frame}.exr")
                created += 1
                if created % 25 == 0 or created == OUTPUT_DEPTH_COUNT:
                    log(f"DEPTH {created}/{OUTPUT_DEPTH_COUNT}")
    staging.replace(destination)
    exr_temp.unlink(missing_ok=True)
    return {
        "source_d32_available_in_clip": len(frames),
        "source_expected_even_depths": len(expected_source_even),
        "source_missing_even_depth_count": len(missing_source_even),
        "source_missing_even_depth_frames": missing_source_even,
        "output_exr_count": created,
        "output_exact_source_hit_count": exact_source_hit,
        "output_temporally_interpolated_count": temporal_interpolated,
        "output_edge_held_count": edge_held,
        "linearization_assumption": f"view-space Z = {near:.12g} / raw_reverse_z * 1.0",
        "metric_scale_status": "UNRESOLVED; 1.0 is a format-only assumption",
        "top_invalid_rows": TOP_INVALID_ROWS,
        "quantization": "truncate toward zero to 0.01 before float16 encoding",
    }


def ffprobe(path: Path) -> dict:
    completed = subprocess.run([
        "ffprobe", "-v", "error", "-count_frames", "-show_streams", "-show_format",
        "-of", "json", str(path),
    ], check=True, capture_output=True, text=True, encoding="utf-8")
    return json.loads(completed.stdout)


def visual_motion_check(video: Path) -> dict:
    capture = cv2.VideoCapture(str(video))
    first = previous = last = None
    frame_count = 0
    near_static = 0
    differences: list[float] = []
    while True:
        ok, frame = capture.read()
        if not ok:
            break
        gray = cv2.cvtColor(cv2.resize(frame, (160, 90), interpolation=cv2.INTER_AREA), cv2.COLOR_BGR2GRAY)
        gray_float = gray.astype(np.float32)
        if first is None:
            first = gray_float
        if previous is not None:
            mad = float(np.mean(np.abs(gray_float - previous)))
            differences.append(mad)
            if mad <= 0.5:
                near_static += 1
        previous = gray_float
        last = gray_float
        frame_count += 1
    capture.release()
    if first is None or last is None:
        raise RuntimeError("cannot decode output video")
    correlation = float(np.corrcoef(first.ravel(), last.ravel())[0, 1])
    static_fraction = near_static / max(1, frame_count - 1)
    return {
        "decoded_frames": frame_count,
        "first_last_grayscale_correlation": correlation,
        "first_last_high_similarity": correlation >= 0.9,
        "near_static_definition": "successive 160x90 grayscale mean absolute difference <= 0.5/255",
        "near_static_transitions": near_static,
        "near_static_fraction": static_fraction,
        "near_static_le_10_percent": static_fraction <= 0.10,
        "successive_mad_median": float(np.median(differences)) if differences else None,
        "note": "content measurements are reported, never overwritten into a passing claim",
    }


def validate_exr_samples(depth_zip: Path, work_dir: Path) -> dict:
    expected_names = [f"depth_{frame}.exr" for frame in range(0, OUTPUT_FRAME_COUNT, 2)]
    samples = ["depth_0.exr", "depth_450.exr", "depth_900.exr",
               "depth_1350.exr", "depth_1798.exr"]
    details = []
    with zipfile.ZipFile(depth_zip, "r") as archive:
        names = archive.namelist()
        if names != expected_names:
            raise RuntimeError("EXR names/order do not match depth_0..depth_1798 even frames")
        for name in samples:
            temp = work_dir / name
            temp.write_bytes(archive.read(name))
            image = OpenEXR.InputFile(str(temp))
            try:
                header = image.header()
                window = header["dataWindow"]
                width = window.max.x - window.min.x + 1
                height = window.max.y - window.min.y + 1
                channels = list(header["channels"].keys())
                pixel_type = int(header["channels"]["Y"].type.v)
                values = np.frombuffer(image.channel("Y", Imath.PixelType(Imath.PixelType.HALF)), dtype=np.float16)
                finite = np.isfinite(values)
                details.append({
                    "name": name, "width": width, "height": height,
                    "channels": channels, "pixel_type_enum": pixel_type,
                    "half_pixel_type": pixel_type == Imath.PixelType.HALF,
                    "finite_fraction": float(np.mean(finite)),
                    "finite_min": float(np.min(values[finite])) if np.any(finite) else None,
                    "finite_max": float(np.max(values[finite])) if np.any(finite) else None,
                })
            finally:
                image.close()
                temp.unlink(missing_ok=True)
    valid = all(item["width"] == DEPTH_W and item["height"] == DEPTH_H
                and item["channels"] == ["Y"] and item["half_pixel_type"] for item in details)
    return {"entry_count": len(expected_names), "names_exact": True,
            "sample_count": len(details), "samples": details, "samples_valid": valid}


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(8 * 1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def self_check(package_dir: Path, report_dir: Path, work_dir: Path) -> dict:
    strict_names = sorted(path.name for path in package_dir.iterdir())
    expected = sorted(["video.mp4", "depth.zip", "action_camera_30fps.json",
                       "action_camera_15fps.json", "systeminfo.json"])
    probe = ffprobe(package_dir / "video.mp4")
    video_stream = next(stream for stream in probe["streams"] if stream["codec_type"] == "video")
    audio_streams = [stream for stream in probe["streams"] if stream["codec_type"] == "audio"]
    action30 = json.loads((package_dir / "action_camera_30fps.json").read_text(encoding="utf-8"))
    action15 = json.loads((package_dir / "action_camera_15fps.json").read_text(encoding="utf-8"))
    exr = validate_exr_samples(package_dir / "depth.zip", work_dir)
    motion = visual_motion_check(package_dir / "video.mp4")
    systeminfo = json.loads((package_dir / "systeminfo.json").read_text(encoding="utf-8"))
    checks = {
        "strict_five_piece_names": strict_names == expected,
        "video_width": int(video_stream["width"]),
        "video_height": int(video_stream["height"]),
        "video_avg_frame_rate": video_stream["avg_frame_rate"],
        "video_frames": int(video_stream.get("nb_read_frames", video_stream.get("nb_frames", 0))),
        "video_duration": float(probe["format"]["duration"]),
        "audio_stream_count": len(audio_streams),
        "audio_codec": audio_streams[0]["codec_name"] if audio_streams else None,
        "action_30_count": len(action30),
        "action_15_count": len(action15),
        "action_30_frame_range": [action30[0]["frame"], action30[-1]["frame"]],
        "action_15_frame_range": [action15[0]["frame"], action15[-1]["frame"]],
        "depth": exr,
        "content": motion,
        "non_trainable": bool(systeminfo.get("non_trainable")),
        "production_certified": bool(systeminfo.get("production_certified")),
    }
    format_pass = (
        checks["strict_five_piece_names"]
        and checks["video_width"] == SOURCE_W and checks["video_height"] == SOURCE_H
        and checks["video_frames"] == OUTPUT_FRAME_COUNT
        and checks["audio_stream_count"] >= 1
        and checks["action_30_count"] == OUTPUT_FRAME_COUNT
        and checks["action_15_count"] == OUTPUT_DEPTH_COUNT
        and exr["samples_valid"]
        and checks["non_trainable"] and not checks["production_certified"]
    )
    checks["format_only_self_check_pass"] = format_pass
    checks["formal_delivery_pass"] = False
    checks["formal_delivery_blockers"] = [
        "source metric scale is unresolved",
        "source formal PR405 fixed-projection/geometric gate failed",
        "missing source depth frames were temporally synthesized",
        "video was retimed from 1726 source frames to 1800 output frames",
    ]
    report_dir.mkdir(parents=True, exist_ok=True)
    (report_dir / "self_check.json").write_text(json.dumps(checks, ensure_ascii=False, indent=2), encoding="utf-8")
    manifest = []
    for path in sorted(package_dir.iterdir()):
        log(f"HASH {path.name}")
        manifest.append({"name": path.name, "bytes": path.stat().st_size, "sha256": sha256(path)})
    (report_dir / "sha256_manifest.json").write_text(json.dumps(manifest, ensure_ascii=False, indent=2), encoding="utf-8")
    return checks


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--source", type=Path, required=True)
    parser.add_argument("--raw-audio-zst", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    source = args.source.resolve()
    output = args.output.resolve()
    package = output / "package"
    staging = output / "package.staging"
    reports = output / "reports"
    if package.exists() or staging.exists():
        raise RuntimeError(f"refusing to overwrite existing package/staging directory: {package}")
    for required in [source / "video.mp4", source / "depth.zip",
                     source / "action_camera_30fps.json", source / "systeminfo.json",
                     args.raw_audio_zst]:
        if not required.exists():
            raise FileNotFoundError(required)
    output.mkdir(parents=True, exist_ok=True)
    staging.mkdir()
    try:
        with tempfile.TemporaryDirectory(prefix="format-only-", dir=output) as temp_name:
            work = Path(temp_name)
            log("PHASE action JSON")
            action_meta = build_actions(source / "action_camera_30fps.json", staging)
            log("PHASE video/audio")
            render_video(source / "video.mp4", args.raw_audio_zst.resolve(), staging / "video.mp4", work)
            log("PHASE depth EXR")
            depth_meta = build_depth(source / "depth.zip", staging / "depth.zip", work,
                                     action_meta["near_constant_from_projection"])
            systeminfo = json.loads((source / "systeminfo.json").read_text(encoding="utf-8"))
            systeminfo.update({
                "non_trainable": True,
                "production_certified": False,
                "deliverable": False,
                "upload_allowed": False,
                "derived_format_only": True,
                "derived_clip": {"source_start_frame": SOURCE_START_FRAME,
                                 "source_end_frame_inclusive": SOURCE_END_EXCLUSIVE - 1,
                                 "output_frames": OUTPUT_FRAME_COUNT},
            })
            (staging / "systeminfo.json").write_text(
                json.dumps(systeminfo, ensure_ascii=False, indent=2), encoding="utf-8")
            staging.replace(package)
            derivation = {
                "classification": "NON_TRAINABLE_FORMAT_ONLY",
                "formal_delivery_pass": False,
                "source_package": str(source),
                "requested_clip": "00:00:04.000 through source final frame",
                "source_frames": [SOURCE_START_FRAME, SOURCE_END_EXCLUSIVE - 1],
                "source_clip_duration_seconds": SOURCE_FRAME_COUNT / FPS,
                "output_duration_seconds": OUTPUT_FRAME_COUNT / FPS,
                "retiming_factor": OUTPUT_FRAME_COUNT / SOURCE_FRAME_COUNT,
                "video_transform": "top 192x8 watermark replaced from adjacent source rows; clip retimed to 1800 frames",
                "audio_transform": "captured 48kHz stereo s16le process-loopback PCM trimmed at 4s, retimed, encoded AAC",
                "actions": action_meta,
                "depth": depth_meta,
                "warnings": [
                    "This package has customer-shaped filenames and containers but is not a formal PR405/PR749 deliverable.",
                    "The 57.533-second requested clip was retimed to the oracle's fixed 60-second/1800-frame shape.",
                    "Depth conversion assumes reversed-Z near/raw and metric_scale=1.0 without calibration.",
                    "Missing depth samples were temporally interpolated; top six depth rows are NaN after watermark removal.",
                ],
            }
            (output / "DERIVATION.json").write_text(
                json.dumps(derivation, ensure_ascii=False, indent=2), encoding="utf-8")
            log("PHASE Python self-check")
            checks = self_check(package, reports, work)
            if not checks["format_only_self_check_pass"]:
                raise RuntimeError("format-only self-check failed; see reports/self_check.json")
    except Exception:
        log("BUILD FAILED; partial package remains in package.staging for diagnosis")
        raise
    log(f"DONE {package}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

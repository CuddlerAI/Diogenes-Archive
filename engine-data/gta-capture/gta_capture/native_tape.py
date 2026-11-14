"""Compact, deterministic camera tape consumed by the in-game ASI.

The JSON tape remains the contract and audit source. This binary is a sealed,
little-endian execution cache so the C++17 companion does not need a JSON
library inside GTA5.exe.
"""

from __future__ import annotations

import hashlib
import math
import os
import struct
import tempfile
from pathlib import Path

from .jsonio import sha256_file
from .tape import LANES, _quaternion_zxy, validate_tape


MAGIC = b"DMGTAT1\0"
VERSION = 1
HEADER = struct.Struct("<8sIIIIIII32s")
LANE_HEADER = struct.Struct("<IIIIQ")
SAMPLE = struct.Struct("<IIQ3d4d3d4d")


def _euler(sample: dict) -> list[float]:
    value = sample.get("euler_deg")
    if not isinstance(value, list) or len(value) != 3:
        raise ValueError("native GTA tape requires euler_deg on every sample")
    return [float(x) for x in value]


def write_native_tape(path, tape: dict, *, json_path=None) -> Path:
    """Write a crash-safe native tape cache and return its absolute path."""
    validate_tape(tape)
    target = Path(path).resolve()
    target.parent.mkdir(parents=True, exist_ok=True)
    if json_path is not None:
        digest = bytes.fromhex(sha256_file(json_path))
    else:
        import json
        encoded = json.dumps(tape, ensure_ascii=False, sort_keys=True,
                             separators=(",", ":")).encode("utf-8")
        digest = hashlib.sha256(encoded).digest()

    frame_count = int(tape["n_frames"])
    first_sample = HEADER.size + len(LANES) * LANE_HEADER.size
    lane_headers = []
    sample_chunks = []
    for lane_index, lane_id in enumerate(LANES):
        lane = tape["lanes"][lane_id]
        resolution = lane["resolution"]
        offset = first_sample + lane_index * frame_count * SAMPLE.size
        lane_headers.append(LANE_HEADER.pack(
            int(lane_id), int(lane["focal_mm"]),
            int(resolution["width"]), int(resolution["height"]), offset,
        ))
        for sample in lane["samples"]:
            euler = _euler(sample)
            # A GTA-native cache only accepts unambiguous order-2 rotations.
            derived = _quaternion_zxy(euler)
            dot = abs(sum(float(sample["quaternion"][i]) * derived[i] for i in range(4)))
            rotation_error_deg = math.degrees(2.0 * math.acos(max(-1.0, min(1.0, dot))))
            if rotation_error_deg > 0.01:
                raise ValueError(
                    f"lane {lane_id} frame {sample['frame']} quaternion/euler disagree "
                    f"for GTA ZXY order by {rotation_error_deg:.6f}deg")
            intr = sample["intrinsics"]
            sample_chunks.append(SAMPLE.pack(
                int(sample["frame"]), 0, int(sample["source_t_ns"]),
                *(float(x) for x in sample["position"]),
                *(float(x) for x in sample["quaternion"]),
                *euler,
                *(float(intr[key]) for key in ("fx", "fy", "cx", "cy")),
            ))

    body = b"".join([
        HEADER.pack(MAGIC, VERSION, len(LANES), frame_count,
                    int(tape["fps_num"]), int(tape["fps_den"]),
                    LANE_HEADER.size, SAMPLE.size, digest),
        *lane_headers,
        *sample_chunks,
    ])
    fd, tmp_name = tempfile.mkstemp(prefix=target.name + ".", suffix=".tmp",
                                    dir=target.parent)
    try:
        with os.fdopen(fd, "wb") as handle:
            handle.write(body)
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(tmp_name, target)
    except BaseException:
        try:
            os.unlink(tmp_name)
        except FileNotFoundError:
            pass
        raise
    return target


def inspect_native_tape(path) -> dict:
    """Read only the binary envelope; used by tests and diagnostics."""
    data = Path(path).read_bytes()
    if len(data) < HEADER.size:
        raise ValueError("native tape is truncated")
    (magic, version, lanes, frames, fps_num, fps_den,
     lane_stride, sample_stride, digest) = HEADER.unpack_from(data)
    if magic != MAGIC or version != VERSION:
        raise ValueError("native tape magic/version mismatch")
    expected = HEADER.size + lanes * lane_stride + lanes * frames * sample_stride
    if expected != len(data):
        raise ValueError("native tape byte length mismatch")
    return {
        "version": version,
        "lane_count": lanes,
        "frame_count": frames,
        "fps_num": fps_num,
        "fps_den": fps_den,
        "lane_stride": lane_stride,
        "sample_stride": sample_stride,
        "json_sha256": digest.hex(),
        "byte_length": len(data),
    }

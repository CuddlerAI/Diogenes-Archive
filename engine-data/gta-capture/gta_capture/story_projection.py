"""Acquire one independently labelled Story calibration image through the ASI.

This is a diagnostic entry point, not an alternate four-lane export. Its
observations cannot enter the Editor v18 calibration assembler without a
separately measured and verified projection transfer.
"""

from __future__ import annotations

import json
import math
from pathlib import Path
import struct

from .errors import ContractError
from .jsonio import sha256_file, write_json_atomic
from .protocol import JsonLinePipeClient


REQUEST_SCHEMA = "dm-gta-story-projection-request/1"
OBSERVATION_SCHEMA = "dm-gta-story-calibration-frame/1"


def _finite(value) -> bool:
    if not isinstance(value, (int, float)) or isinstance(value, bool):
        return False
    try:
        return math.isfinite(value)
    except OverflowError:
        return False


def validate_request(document: dict) -> dict:
    required = {"schema", "camera_position", "camera_euler_deg", "point_world",
                "fov_deg", "nominal_focal_mm", "marker_mode"}
    if not isinstance(document, dict) or set(document) != required:
        raise ContractError("Story projection request requires the exact documented fields")
    if document["schema"] != REQUEST_SCHEMA:
        raise ContractError("Story projection request schema mismatch")
    for field in ("camera_position", "camera_euler_deg", "point_world"):
        values = document[field]
        if not isinstance(values, list) or len(values) != 3:
            raise ContractError(f"{field} must be a vec3")
        if any(not _finite(x) for x in values):
            raise ContractError(f"{field} must contain finite numbers")
        bound = 360 if field == "camera_euler_deg" else 20000
        if any(abs(x) > bound for x in values):
            raise ContractError(f"{field} exceeds the diagnostic bounds")
    for field in ("fov_deg", "nominal_focal_mm"):
        value = document[field]
        if not _finite(value):
            raise ContractError(f"{field} must be finite")
    if not 5 <= document["fov_deg"] <= 120:
        raise ContractError("Story diagnostic FOV must be between 5 and 120 degrees")
    if document["nominal_focal_mm"] not in (18, 24, 35, 50):
        raise ContractError("nominal focal label must be 18, 24, 35 or 50; it is not an approval")
    if document["marker_mode"] not in ("world_cross", "none"):
        raise ContractError("marker_mode must be world_cross or none")
    return document


def capture_sample(request_path: Path, output_dir: Path, *, pipe: str,
                   client_factory=JsonLinePipeClient) -> dict:
    request_path = Path(request_path).resolve()
    request_sha = sha256_file(request_path)
    document = validate_request(json.loads(request_path.read_text(encoding="utf-8")))
    output_dir = Path(output_dir).resolve()
    # Refuse any existing directory, including an empty failed attempt. This
    # keeps the native PNG and a failed measurement attempt distinguishable.
    output_dir.mkdir(parents=True, exist_ok=False)
    request_snapshot = output_dir / "request.json"
    write_json_atomic(request_snapshot, document)
    snapshot_sha = sha256_file(request_snapshot)
    png = output_dir / "point.png"
    if len(str(png).encode("utf-8")) >= 512:
        raise ContractError("Story PNG path exceeds the pinned capture transport capacity")
    client = client_factory(pipe)
    try:
        hello = client.request("hello")
        if hello.get("story_mode") is not True or hello.get("online_session") is not False:
            raise ContractError("Story projection requires an explicitly offline Story runtime")
        write_json_atomic(output_dir / "runtime-before.json", hello)
        result = client.request("story_projection_sample", {
            "request_path": str(request_snapshot),
            "request_sha256": snapshot_sha,
            "output_path": str(png),
        })
    finally:
        client.close()
    if (result.get("schema") != OBSERVATION_SCHEMA or result.get("mock") is not False
            or result.get("approval_status") != "draft"
            or result.get("editor_equivalence_verified") is not False
            or result.get("delivery_eligible") is not False):
        raise ContractError("Story observation must remain a real, unapproved diagnostic")
    if Path(result.get("output_path", "")).resolve() != png:
        raise ContractError("Story observation changed the requested output path")
    if not png.is_file() or result.get("output_bytes") != png.stat().st_size:
        raise ContractError("Story PNG missing or size differs from the native result")
    if result.get("output_sha256") != sha256_file(png):
        raise ContractError("Story PNG differs from the native SHA-256")
    with png.open("rb") as stream:
        header = stream.read(24)
    if len(header) != 24 or header[:8] != b"\x89PNG\r\n\x1a\n" or header[12:16] != b"IHDR":
        raise ContractError("Story diagnostic output is not a native PNG")
    width, height = struct.unpack(">II", header[16:24])
    dimensions = (result.get("width"), result.get("height"))
    if (any(not isinstance(n, int) or isinstance(n, bool) or n < 1 for n in dimensions)
            or width < 1 or height < 1 or dimensions != (width, height)):
        raise ContractError("Story PNG dimensions differ from native readback")
    if (sha256_file(request_path) != request_sha or sha256_file(request_snapshot) != snapshot_sha
            or result.get("request_sha256") != snapshot_sha):
        raise ContractError("Story request changed during sampling")
    write_json_atomic(output_dir / "observation.json", result)
    return result

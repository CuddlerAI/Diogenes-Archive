"""Versioned four-lane camera tape and trajgen import."""

from __future__ import annotations

import math
from pathlib import Path
from typing import Any

from .errors import ContractError
from .jsonio import load_json, sha256_file, write_json_atomic


TAPE_SCHEMA = "dm-gta-camera-tape/1"
LANES = ("1", "2", "3", "4")
FOCALS = (18, 24, 35, 50)
GTA_COORDINATE_CONVENTION = "gta-native-z-up-right-handed-order2"


def _number(value: Any) -> bool:
    return isinstance(value, (int, float)) and math.isfinite(float(value))


def _vec(value: Any, n: int, label: str) -> None:
    if not isinstance(value, list) or len(value) != n or not all(_number(x) for x in value):
        raise ContractError(f"{label} must be a finite numeric vector of length {n}")


def _source_t_ns(frame: int) -> int:
    return round(frame * 1_000_000_000 / 30)


def _quaternion_zxy(euler_deg: list[float]) -> list[float]:
    """Return [x,y,z,w] for GTA rotation order 2 (Z, then X, then Y).

    GTA's camera basis stores right/forward/up as matrix rows.  With the
    Hamilton product used here, its order-2 basis is ``qz * qx * qy``.  The
    multiplication order is part of the native contract: reversing it can
    preserve the authored pitch and yaw values while silently introducing
    roll in the actual camera matrix.
    """
    x, y, z = (math.radians(float(value)) * 0.5 for value in euler_deg)
    qx = (math.sin(x), 0.0, 0.0, math.cos(x))
    qy = (0.0, math.sin(y), 0.0, math.cos(y))
    qz = (0.0, 0.0, math.sin(z), math.cos(z))

    def multiply(a, b):
        ax, ay, az, aw = a
        bx, by, bz, bw = b
        return (
            aw * bx + ax * bw + ay * bz - az * by,
            aw * by - ax * bz + ay * bw + az * bx,
            aw * bz + ax * by - ay * bx + az * bw,
            aw * bw - ax * bx - ay * by - az * bz,
        )

    return [float(value) for value in multiply(qz, multiply(qx, qy))]


def _euler_zxy_deg(quaternion: list[float]) -> list[float]:
    """Invert GTA order-2 quaternion to [pitch, roll, yaw] degrees.

    The singular case is intentionally rejected: silently choosing one of many
    Euler representations would make the native execution cache ambiguous.
    """
    _vec(quaternion, 4, "quaternion")
    x, y, z, w = (float(value) for value in quaternion)
    norm = math.sqrt(x * x + y * y + z * z + w * w)
    if abs(norm - 1.0) > 1e-3:
        raise ContractError("quaternion must be normalized before GTA ZXY conversion")
    x, y, z, w = (value / norm for value in (x, y, z, w))
    # These are the exact inverse reads used by GTA's camera basis:
    #   pitch = asin(forward.z)
    #   roll  = atan2(-right.z, up.z)
    #   yaw   = atan2(-forward.x, forward.y)
    forward_z = 2.0 * (y * z + w * x)
    neg_right_z = -2.0 * (x * z - w * y)
    up_z = 1.0 - 2.0 * (x * x + y * y)
    neg_forward_x = -2.0 * (x * y - w * z)
    forward_y = 1.0 - 2.0 * (x * x + z * z)
    pitch = math.asin(max(-1.0, min(1.0, forward_z)))
    if abs(math.cos(pitch)) < 1e-7:
        raise ContractError("quaternion is singular for an unambiguous GTA ZXY Euler rotation")
    roll = math.atan2(neg_right_z, up_z)
    yaw = math.atan2(neg_forward_x, forward_y)
    return [math.degrees(value) for value in (pitch, roll, yaw)]


def _look_at_euler(position: list[float], target: list[float]) -> list[float]:
    dx, dy, dz = (float(target[i]) - float(position[i]) for i in range(3))
    horizontal = math.hypot(dx, dy)
    if horizontal < 1e-6:
        raise ContractError("GTA POC camera cannot look straight up/down at its target")
    return [math.degrees(math.atan2(dz, horizontal)), 0.0,
            math.degrees(math.atan2(-dx, dy))]


def validate_tape(value: Any, *, expected_duration_s: float | None = None) -> dict:
    if not isinstance(value, dict) or value.get("schema") != TAPE_SCHEMA:
        raise ContractError(f"camera tape schema must equal {TAPE_SCHEMA!r}")
    if value.get("fps_num") != 30 or value.get("fps_den") != 1:
        raise ContractError("camera tape must use 30/1 fps")
    n_frames = value.get("n_frames")
    duration_s = value.get("duration_s")
    if not isinstance(n_frames, int) or n_frames <= 0:
        raise ContractError("camera tape n_frames must be a positive integer")
    if not _number(duration_s) or abs(float(duration_s) * 30 - n_frames) > 1e-6:
        raise ContractError("camera tape duration_s and n_frames disagree")
    if expected_duration_s is not None and abs(float(duration_s) - expected_duration_s) > 1e-6:
        raise ContractError(
            f"camera tape duration {duration_s}s does not match scenario {expected_duration_s}s")
    if value.get("timestamp_semantics") != "source_replay_timeline":
        raise ContractError("camera tape timestamp_semantics must be source_replay_timeline")
    filmback = value.get("filmback_id")
    if not isinstance(filmback, str) or not filmback:
        raise ContractError("camera tape filmback_id is required")
    lanes = value.get("lanes")
    if not isinstance(lanes, dict) or set(lanes) != set(LANES):
        raise ContractError("camera tape must contain lanes 1,2,3,4 exactly")

    source_times: list[list[int]] = []
    for lane_id in LANES:
        lane = lanes[lane_id]
        if not isinstance(lane, dict):
            raise ContractError(f"lane {lane_id} must be an object")
        if lane.get("focal_mm") not in FOCALS:
            raise ContractError(f"lane {lane_id} focal_mm must be one of {FOCALS}")
        resolution = lane.get("resolution")
        if (not isinstance(resolution, dict) or not isinstance(resolution.get("width"), int)
                or not isinstance(resolution.get("height"), int)
                or resolution["width"] <= 0 or resolution["height"] <= 0):
            raise ContractError(f"lane {lane_id} resolution is invalid")
        samples = lane.get("samples")
        if not isinstance(samples, list) or len(samples) != n_frames:
            raise ContractError(f"lane {lane_id} must contain {n_frames} samples")
        lane_times = []
        for index, sample in enumerate(samples):
            if not isinstance(sample, dict) or sample.get("frame") != index:
                raise ContractError(f"lane {lane_id} frame sequence breaks at {index}")
            expected_ns = _source_t_ns(index)
            if sample.get("source_t_ns") != expected_ns:
                raise ContractError(
                    f"lane {lane_id} frame {index} source_t_ns must be {expected_ns}")
            _vec(sample.get("position"), 3, f"lane {lane_id} frame {index} position")
            _vec(sample.get("quaternion"), 4, f"lane {lane_id} frame {index} quaternion")
            norm = math.sqrt(sum(float(x) ** 2 for x in sample["quaternion"]))
            if abs(norm - 1.0) > 1e-3:
                raise ContractError(f"lane {lane_id} frame {index} quaternion is not normalized")
            intr = sample.get("intrinsics")
            if (not isinstance(intr, dict) or any(not _number(intr.get(k)) for k in ("fx", "fy", "cx", "cy"))
                    or intr["fx"] <= 0 or intr["fy"] <= 0
                    or abs(float(intr["fx"]) - float(intr["fy"])) > 1e-4):
                raise ContractError(f"lane {lane_id} frame {index} intrinsics are invalid")
            if "euler_deg" in sample:
                _vec(sample["euler_deg"], 3, f"lane {lane_id} frame {index} euler_deg")
            lane_times.append(sample["source_t_ns"])
        source_times.append(lane_times)
    if any(times != source_times[0] for times in source_times[1:]):
        raise ContractError("camera tape lanes do not share the same source timeline")
    # A four-worldline take's lanes are not one each: the chase and the first person both take 35 mm since 2026-10-16
    # (the customer: the first person at 18 mm was too wide; the requirement's 25% each is the batch's, not a take's).
    source = value.get("source")
    if (not (isinstance(source, dict) and source.get("kind") == "gta_stage_take")
            and sorted(lanes[lane]["focal_mm"] for lane in LANES) != list(FOCALS)):
        raise ContractError("one clip must assign 18/24/35/50mm exactly once across four lanes")
    return value


def load_tape(path, *, expected_duration_s: float | None = None) -> dict:
    return validate_tape(load_json(path), expected_duration_s=expected_duration_s)


def make_demo_tape(*, duration_s: float = 10.0, width: int = 1920, height: int = 1080,
                   filmback_id: str = "dryrun-36mm-horizontal", clip_id: str = "dryrun-poc") -> dict:
    n_frames = int(round(duration_s * 30))
    if abs(n_frames / 30 - duration_s) > 1e-9:
        raise ContractError("demo duration must be an exact multiple of 1/30 second")
    lanes = {}
    # Small, smooth, deliberately different motions; this is protocol data, not a QC-passing scene plan.
    starts = ((0.0, 1.6, -2.0), (1.0, 1.7, -1.5), (-1.0, 1.8, -1.0), (0.5, 2.2, -2.5))
    velocities = ((0.40, 0.0, 0.10), (0.15, 0.0, 0.35), (-0.25, 0.0, 0.20), (0.30, 0.02, -0.10))
    for lane_index, focal in enumerate(FOCALS, 1):
        f_px = width * focal / 36.0
        samples = []
        yaw_rate = (lane_index - 2.5) * 1.5
        for frame in range(n_frames):
            t = frame / 30.0
            p0 = starts[lane_index - 1]
            vel = velocities[lane_index - 1]
            wobble = 0.08 * math.sin(2.0 * math.pi * t / max(duration_s, 1.0))
            position = [p0[0] + vel[0] * t, p0[1] + wobble, p0[2] + vel[2] * t]
            yaw = math.radians(yaw_rate * t)
            quat = [0.0, math.sin(yaw / 2.0), 0.0, math.cos(yaw / 2.0)]
            samples.append({
                "frame": frame,
                "source_t_ns": _source_t_ns(frame),
                "position": [round(x, 9) for x in position],
                "quaternion": [round(x, 12) for x in quat],
                "euler_deg": [0.0, round(math.degrees(yaw), 9), 0.0],
                "intrinsics": {"fx": f_px, "fy": f_px, "cx": width / 2, "cy": height / 2},
            })
        lanes[str(lane_index)] = {
            "focal_mm": focal,
            "resolution": {"width": width, "height": height},
            "samples": samples,
        }
    tape = {
        "schema": TAPE_SCHEMA,
        "clip_id": clip_id,
        "fps_num": 30,
        "fps_den": 1,
        "duration_s": duration_s,
        "n_frames": n_frames,
        "timestamp_semantics": "source_replay_timeline",
        "filmback_id": filmback_id,
        "coordinate_convention": "dryrun-y-up-right-handed",
        "source": {"kind": "deterministic_demo", "delivery_eligible": False},
        "lanes": lanes,
    }
    return validate_tape(tape)


def make_gta_poc_tape(*, anchor: list[float], duration_s: float = 10.0,
                      width: int = 1920, height: int = 1080,
                      filmback_id: str = "gta-poc-36mm-horizontal",
                      clip_id: str = "gta-legacy-poc",
                      route_distance_m: float = 5.0,
                      route_speed_mps: float = 1.0,
                      route_heading_deg: float = 0.0,
                      lane_lateral_offsets_m: list[float] | tuple[float, ...] | None = None) -> dict:
    """Build a conservative four-lane tape in absolute GTA world coordinates.

    The anchor is the subject position selected in Story Mode.  The four
    cameras start as a compact, similarly oriented cluster behind the subject,
    then use deliberately different world-space motions while continuously
    looking at chest height.  This keeps the subject covisible without making
    all four cameras inherit the same translation from the walking target.
    The profile is only a bring-up POC; it does not replace the authoritative
    trajgen/covis/cam4 sidecars required for delivery.

    This generator exists separately from ``make_demo_tape`` so dry-run
    coordinates can never be mistaken for calibrated GTA coordinates.
    """
    _vec(anchor, 3, "anchor")
    if width <= 0 or height <= 0:
        raise ContractError("GTA POC tape resolution must be positive")
    n_frames = int(round(duration_s * 30))
    if n_frames <= 0 or abs(n_frames / 30 - duration_s) > 1e-9:
        raise ContractError("GTA POC duration must be an exact positive multiple of 1/30 second")
    if (not math.isfinite(route_distance_m) or route_distance_m <= 0
            or not math.isfinite(route_speed_mps) or route_speed_mps <= 0
            or not math.isfinite(route_heading_deg)
            or not 0 <= route_heading_deg < 360):
        raise ContractError("GTA POC route distance/heading is invalid")
    anchor_target = [float(anchor[0]), float(anchor[1]), float(anchor[2]) + 1.1]
    heading = math.radians(route_heading_deg)
    route_direction = [-math.sin(heading), math.cos(heading), 0.0]
    # GTA heading zero points +Y. ``route_right`` is the horizontal vector to
    # the subject's right.  A compact initial cluster keeps every pair's first
    # forward vector within the 15 degree internal-control limit.
    route_right = [route_direction[1], -route_direction[0], 0.0]
    back_offsets = (12.0, 13.0, 14.0, 15.0)
    lateral_offsets = (tuple(float(value) for value in lane_lateral_offsets_m)
                       if lane_lateral_offsets_m is not None
                       else (-0.6, -0.2, 0.2, 0.6))
    if (len(lateral_offsets) != 4
            or any(not math.isfinite(value) or abs(value) > 50.0
                   for value in lateral_offsets)):
        raise ContractError("GTA POC lane lateral offsets must be four finite values within 50 m")
    height_offsets = (2.3, 2.5, 2.7, 2.9)
    # Surveyed lateral overrides can defeat the compact-cluster assumption.
    # Reject those layouts before emitting a tape, not after four real exports
    # (POC061: lanes 1/2 diverged by 21.85 degrees).  In the orthonormal
    # route-right/route-forward/up basis the look vector is (-lateral, back,
    # -height), so this check is independent of anchor and route heading.
    # This is only the 15-degree engine_qc planning guard, not proof of
    # covisibility, collision clearance, calibration or delivery eligibility.
    first_directions = []
    for lateral, back, height_offset in zip(lateral_offsets, back_offsets, height_offsets):
        direction = (-lateral, back, -height_offset)
        length = math.sqrt(sum(value * value for value in direction))
        first_directions.append(tuple(value / length for value in direction))
    for first_index, first_direction in enumerate(first_directions):
        for second_index in range(first_index + 1, len(first_directions)):
            dot = sum(a * b for a, b in zip(first_direction, first_directions[second_index]))
            angle = math.degrees(math.acos(max(-1.0, min(1.0, dot))))
            if angle > 15.0:
                raise ContractError(
                    f"GTA POC initial orientation lanes {first_index + 1}-{second_index + 1} "
                    f"is {angle:.2f} degrees; must be <=15.0 before recording; "
                    "resurvey offsets (collision clearance remains unverified)")
    # Pairwise directions are parallel, perpendicular or opposite only where
    # their speed bands are intentionally different.  A dedicated vertical
    # lane prevents the all-four window from becoming synchronized.
    velocities = (
        [0.35 * value for value in route_direction],
        [0.45 * value for value in route_right],
        [-0.30 * value for value in route_direction],
        [0.0, 0.0, 0.25],
    )
    lanes = {}
    for lane_index, focal in enumerate(FOCALS, 1):
        f_px = width * focal / 36.0
        samples = []
        for frame in range(n_frames):
            t = frame / 30.0
            travelled = min(route_distance_m, route_speed_mps * t)
            target = [anchor_target[axis] + route_direction[axis] * travelled
                      for axis in range(3)]
            velocity = velocities[lane_index - 1]
            position = [
                anchor_target[0] - back_offsets[lane_index - 1] * route_direction[0]
                + lateral_offsets[lane_index - 1] * route_right[0] + velocity[0] * t,
                anchor_target[1] - back_offsets[lane_index - 1] * route_direction[1]
                + lateral_offsets[lane_index - 1] * route_right[1] + velocity[1] * t,
                anchor_target[2] + height_offsets[lane_index - 1] + velocity[2] * t,
            ]
            euler = _look_at_euler(position, target)
            samples.append({
                "frame": frame,
                "source_t_ns": _source_t_ns(frame),
                "position": [round(value, 9) for value in position],
                "quaternion": [round(value, 12) for value in _quaternion_zxy(euler)],
                "euler_deg": [round(value, 9) for value in euler],
                "intrinsics": {"fx": f_px, "fy": f_px, "cx": width / 2, "cy": height / 2},
            })
        lanes[str(lane_index)] = {
            "focal_mm": focal,
            "resolution": {"width": width, "height": height},
            "samples": samples,
        }
    tape = {
        "schema": TAPE_SCHEMA,
        "clip_id": clip_id,
        "fps_num": 30,
        "fps_den": 1,
        "duration_s": duration_s,
        "n_frames": n_frames,
        "timestamp_semantics": "source_replay_timeline",
        "filmback_id": filmback_id,
        "coordinate_convention": GTA_COORDINATE_CONVENTION,
        "source": {"kind": "gta_absolute_anchor", "delivery_eligible": False,
                   "motion_profile": "decoupled-covisible-poc-v2",
                   "covisibility_target": "controlled_story_player_chest",
                   "anchor": [float(value) for value in anchor],
                   "lane_lateral_offsets_m": list(lateral_offsets),
                   "route_distance_m": route_distance_m,
                   "route_speed_mps": route_speed_mps,
                   "route_heading_deg": route_heading_deg},
        "lanes": lanes,
    }
    return validate_tape(tape)


def import_trajgen_clip(clip_dir, *, filmback_id: str, clip_id: str | None = None) -> dict:
    """Convert trajgen's action JSON into a GTA camera tape without changing poses."""
    root = Path(clip_dir)
    plan_path = root / "plan.json"
    if not plan_path.is_file():
        raise ContractError(f"trajgen clip is missing {plan_path}")
    plan = load_json(plan_path)
    cameras = plan.get("cameras")
    if not isinstance(cameras, list):
        raise ContractError("trajgen plan cameras are missing")
    focal_by_id = {str(cam.get("id")): cam.get("focal_mm") for cam in cameras
                   if isinstance(cam, dict)}
    duration = float(plan.get("duration_s", 0))
    lanes = {}
    n_frames = None
    source_files = []
    for lane_id in LANES:
        action_path = root / f"action_camera_{lane_id}.json"
        system_path = root / f"systeminfo_{lane_id}.json"
        if not action_path.is_file() or not system_path.is_file():
            raise ContractError(f"trajgen clip is missing lane {lane_id} action/systeminfo")
        records = load_json(action_path)
        system = load_json(system_path)
        if not isinstance(records, list):
            raise ContractError(f"action_camera_{lane_id}.json must be an array")
        if n_frames is None:
            n_frames = len(records)
        if len(records) != n_frames:
            raise ContractError("trajgen lanes have different frame counts")
        samples = []
        for index, record in enumerate(records):
            sample = {
                "frame": index,
                "source_t_ns": _source_t_ns(index),
                "position": record.get("camera_position"),
                "quaternion": record.get("camera_rotation_quaternion"),
                "intrinsics": record.get("camera_intrinsics"),
            }
            if "camera_rotation_oula" in record:
                sample["euler_deg"] = record["camera_rotation_oula"]
            else:
                sample["euler_deg"] = _euler_zxy_deg(sample["quaternion"])
            samples.append(sample)
        lanes[lane_id] = {
            "focal_mm": focal_by_id.get(lane_id),
            "resolution": {"width": system.get("width"), "height": system.get("height")},
            "samples": samples,
        }
        source_files.extend([
            {"name": action_path.name, "sha256": sha256_file(action_path)},
            {"name": system_path.name, "sha256": sha256_file(system_path)},
        ])
    tape = {
        "schema": TAPE_SCHEMA,
        "clip_id": clip_id or root.name,
        "fps_num": 30,
        "fps_den": 1,
        "duration_s": duration,
        "n_frames": n_frames,
        "timestamp_semantics": "source_replay_timeline",
        "filmback_id": filmback_id,
        "coordinate_convention": "trajgen-native; GTA adapter transform pending calibration",
        "source": {"kind": "trajgen_clip", "path": str(root.resolve()), "files": source_files},
        "lanes": lanes,
    }
    return validate_tape(tape, expected_duration_s=duration)


def write_tape(path, tape: dict) -> Path:
    validate_tape(tape)
    return write_json_atomic(path, tape)

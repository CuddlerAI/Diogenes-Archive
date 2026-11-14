"""Cross-lane replay alignment and docs/69 metadata materialization."""

from __future__ import annotations

import math
import os
from dataclasses import dataclass
from datetime import datetime, timedelta
from pathlib import Path

from .errors import ContractError
from .jsonio import read_jsonl, sha256_file, write_json_atomic
from .protocol import FRAME_SCHEMA


REAL_CAPTURE_PROTOCOL = 18
REAL_EDITOR_IDENTITY_PROTOCOL = 4
REAL_EDITOR_BINDING_METHOD = "replus-present-plan-v2"
REAL_CAPTURE_FENCE = "present_pixel_ready_final_writer_generation_pose_release_ack"
GTA_FLOAT_MATRIX_ROTATION_TOLERANCE_DEG = 0.052
REAL_PLAYER_OBSERVATION_METHOD = (
    "record_master_recording-side-estimate-no-catchup-30fps"
)
PLAYER_TELEMETRY_SCHEMA = "dm-gta-player-telemetry/1"
PLAYER_OBSERVATION_TOLERANCE_NS = 20_000_000
GTA_NATIVE_COORDINATE_CONVENTION = "gta-native-z-up-right-handed-order2"
DELIVERY_COORDINATE_CONVENTION = "docs69-y-up-left-handed-x-right-y-up-z-front"


@dataclass
class AlignmentReport:
    ok: bool
    n_frames: int
    replay_sha256: str | None
    problems: list[str]
    lane_event_sha256: dict[str, str]
    mock: bool
    max_position_error_m: float
    max_rotation_error_deg: float
    max_intrinsics_error_px: float
    logical_interlane_delta_ns_max: int | None
    player_telemetry_diagnostic_ok: bool
    player_replay_origin_verified: bool

    def to_dict(self) -> dict:
        return {
            "schema": "dm-gta-alignment-report/1",
            "ok": self.ok,
            "n_frames": self.n_frames,
            "replay_sha256": self.replay_sha256,
            "logical_interlane_delta_ns_max": self.logical_interlane_delta_ns_max,
            "problems": self.problems,
            "lane_event_sha256": self.lane_event_sha256,
            "mock": self.mock,
            "max_position_error_m": self.max_position_error_m,
            "max_rotation_error_deg": self.max_rotation_error_deg,
            "max_intrinsics_error_px": self.max_intrinsics_error_px,
            "player_observation_method": REAL_PLAYER_OBSERVATION_METHOD,
            "player_telemetry_diagnostic_ok": self.player_telemetry_diagnostic_ok,
            "player_replay_origin_verified": self.player_replay_origin_verified,
        }


def _finite_vec(value, length: int) -> bool:
    return (isinstance(value, list) and len(value) == length
            and all(isinstance(x, (int, float)) and math.isfinite(float(x)) for x in value))


def _integer(value, *, minimum: int = 0) -> bool:
    return isinstance(value, int) and not isinstance(value, bool) and value >= minimum


def _sha256(value) -> bool:
    return (isinstance(value, str) and len(value) == 64
            and all(char in "0123456789abcdef" for char in value))


def _player_state_valid(value) -> bool:
    if (not isinstance(value, dict)
            or not _finite_vec(value.get("position"), 3)
            or not _finite_vec(value.get("quaternion"), 4)
            or not _finite_vec(value.get("euler_deg"), 3)
            or not _finite_vec(value.get("velocity"), 3)):
        return False
    norm = math.sqrt(sum(float(component) ** 2 for component in value["quaternion"]))
    return abs(norm - 1.0) <= 1e-3


def _player_state_equal(left, right, *, tolerance: float = 1e-6) -> bool:
    if not _player_state_valid(left) or not _player_state_valid(right):
        return False
    return all(
        abs(float(left[key][index]) - float(right[key][index])) <= tolerance
        for key, length in (("position", 3), ("quaternion", 4),
                            ("euler_deg", 3), ("velocity", 3))
        for index in range(length)
    )


def validate_pixel_pose_fence(lanes: dict[int, list[dict]],
                              expected_replay_sha256: str | None,
                              expected_render_profile_sha256: str | None = None,
                              expected_output_root: str | None = None) -> list[str]:
    """Revalidate the independent pixel-ready/final-writer evidence chain.

    These checks intentionally do not trust an alignment report or a protocol
    label. The add-on's pixel-ready generation and RE+'s final-camera generation
    must independently agree for every unique request, advance exactly once per
    planned frame, and still name the sealed RGB file on disk.
    """
    problems: list[str] = []
    if not _sha256(expected_replay_sha256):
        return ["real frame fence has no sealed master replay sha256"]
    if set(lanes) != {1, 2, 3, 4}:
        return ["real frame fence requires exactly lanes 1..4"]
    frame_counts = {len(rows) for rows in lanes.values() if isinstance(rows, list)}
    if len(frame_counts) != 1 or not frame_counts or next(iter(frame_counts)) <= 0:
        return ["real frame fence requires four non-empty equal-length lanes"]

    seen_requests: set[int] = set()
    seen_generations: set[int] = set()
    seen_outputs: set[Path] = set()
    seen_profiles: set[str] = set()
    seen_output_roots: set[Path] = set()
    previous_global_generation: int | None = None
    for lane in range(1, 5):
        previous_request = previous_sequence = previous_heartbeat = None
        for frame, row in enumerate(lanes[lane]):
            prefix = f"lane {lane} frame {frame}"
            if not isinstance(row, dict):
                return [f"{prefix}: frame event is not an object"]
            binding = row.get("editor_binding")
            final_camera = row.get("final_camera")
            if (row.get("schema") != FRAME_SCHEMA or row.get("mock") is not False
                    or row.get("lane") != lane or row.get("output_frame") != frame
                    or row.get("replay_frame") != frame
                    or row.get("replay_sha256") != expected_replay_sha256
                    or row.get("render_succeeded") is not True
                    or row.get("capture_protocol") != REAL_CAPTURE_PROTOCOL
                    or row.get("editor_identity_protocol") !=
                    REAL_EDITOR_IDENTITY_PROTOCOL
                    or row.get("capture_fence") != REAL_CAPTURE_FENCE):
                return [f"{prefix}: real capture protocol/fence envelope is invalid"]
            if (not isinstance(final_camera, dict)
                    or not _finite_vec(final_camera.get("position"), 3)
                    or not _finite_vec(final_camera.get("quaternion"), 4)
                    or not isinstance(final_camera.get("intrinsics"), dict)):
                return [f"{prefix}: final-writer camera evidence is invalid"]
            if not isinstance(binding, dict):
                return [f"{prefix}: request-bound editor identity is missing"]
            request_id = binding.get("capture_request_id")
            final_generation = binding.get("final_camera_generation")
            pixel_generation = row.get("pixel_ready_final_camera_generation")
            sequence = binding.get("identity_sequence")
            heartbeat = binding.get("identity_heartbeat")
            profile_sha256 = binding.get("render_profile_sha256")
            output_root_value = binding.get("normalized_output_root")
            if (binding.get("method") != REAL_EDITOR_BINDING_METHOD
                    or not isinstance(binding.get("session_id"), str)
                    or not binding.get("session_id")
                    or not isinstance(binding.get("project_name"), str)
                    or not binding.get("project_name")
                    or binding.get("clip_count") != 1
                    or binding.get("clip_index") != 0
                    or not isinstance(binding.get("clip_identity"), str)
                    or len(binding.get("clip_identity")) != 16
                    or not _integer(request_id, minimum=1)
                    or not _integer(final_generation, minimum=1)
                    or not _integer(pixel_generation, minimum=1)
                    or pixel_generation != final_generation
                    or not _integer(sequence, minimum=2) or sequence % 2 != 0
                    or not _integer(heartbeat, minimum=1)
                    or not _sha256(profile_sha256)
                    or not isinstance(output_root_value, str)
                    or not output_root_value):
                return [f"{prefix}: request/generation identity binding is invalid"]
            output_root = Path(output_root_value)
            if not output_root.is_absolute():
                return [f"{prefix}: sealed RE+ output root is not absolute"]
            output_root = output_root.resolve()
            if (expected_render_profile_sha256 is not None
                    and profile_sha256 != expected_render_profile_sha256):
                return [f"{prefix}: render profile digest differs from the sealed receipt"]
            if expected_output_root is not None:
                expected_root = Path(expected_output_root).resolve()
                if os.path.normcase(str(output_root)) != os.path.normcase(str(expected_root)):
                    return [f"{prefix}: output root differs from the sealed receipt"]
            if request_id in seen_requests:
                return [f"{prefix}: capture request id was reused"]
            if final_generation in seen_generations:
                return [f"{prefix}: final-camera generation was reused"]
            if (previous_global_generation is not None
                    and final_generation != previous_global_generation + 1):
                return [f"{prefix}: final-camera generation did not advance by one"]
            if previous_request is not None and request_id <= previous_request:
                return [f"{prefix}: capture request id did not advance"]
            if previous_sequence is not None and sequence <= previous_sequence:
                return [f"{prefix}: editor identity sequence did not advance"]
            if previous_heartbeat is not None and heartbeat <= previous_heartbeat:
                return [f"{prefix}: editor identity heartbeat did not advance"]

            output_path_value = row.get("output_path")
            output_bytes = row.get("output_bytes")
            output_sha256 = row.get("output_sha256")
            if (not isinstance(output_path_value, str) or not output_path_value
                    or not _integer(output_bytes, minimum=1)
                    or not _sha256(output_sha256)):
                return [f"{prefix}: sealed RGB output record is missing"]
            output_path = Path(output_path_value)
            if not output_path.is_absolute():
                return [f"{prefix}: sealed RGB output path is not absolute"]
            output_path = output_path.resolve()
            try:
                common = os.path.commonpath((str(output_path), str(output_root)))
            except ValueError:
                common = ""
            if os.path.normcase(common) != os.path.normcase(str(output_root)):
                return [f"{prefix}: sealed RGB output escaped the expected root"]
            if output_path in seen_outputs:
                return [f"{prefix}: sealed RGB output path was reused"]
            try:
                if (output_path.stat().st_size != output_bytes
                        or sha256_file(output_path) != output_sha256):
                    return [f"{prefix}: sealed RGB output changed before finalize"]
            except OSError as exc:
                return [f"{prefix}: sealed RGB output is unavailable: {exc}"]

            seen_requests.add(request_id)
            seen_generations.add(final_generation)
            seen_outputs.add(output_path)
            seen_profiles.add(profile_sha256)
            seen_output_roots.add(output_root)
            previous_global_generation = final_generation
            previous_request = request_id
            previous_sequence = sequence
            previous_heartbeat = heartbeat
    if len(seen_profiles) != 1 or len(seen_output_roots) != 1:
        return ["four lanes do not share one sealed render profile/output root"]
    return problems


def validate_player_telemetry(lanes: dict[int, list[dict]],
                              expected_replay_sha256: str | None) -> list[str]:
    """Validate the independently sealed recording-side telemetry estimate.

    This proves file integrity, per-frame grid consistency and cross-lane reuse.
    It deliberately does *not* prove that the ScriptHook recording-side origin
    equals Rockstar Editor replay frame 0; only an independent in-band origin
    calibration can establish that semantic binding.
    """
    if not _sha256(expected_replay_sha256):
        return ["player telemetry has no sealed master replay sha256"]
    if set(lanes) != {1, 2, 3, 4}:
        return ["player telemetry requires exactly lanes 1..4"]
    frame_counts = {len(rows) for rows in lanes.values() if isinstance(rows, list)}
    if len(frame_counts) != 1 or not frame_counts or next(iter(frame_counts)) <= 0:
        return ["player telemetry requires four non-empty equal-length lanes"]
    n_frames = next(iter(frame_counts))
    telemetry_cache: dict[tuple[Path, str], list[dict]] = {}
    telemetry_refs: set[tuple[Path, str]] = set()

    for lane in range(1, 5):
        for frame, row in enumerate(lanes[lane]):
            prefix = f"lane {lane} frame {frame}"
            if not isinstance(row, dict):
                return [f"{prefix}: frame event is not an object"]
            method = row.get("player_observation_method")
            observation_t_ns = row.get("player_observation_t_ns")
            source_t_ns = row.get("source_t_ns")
            player_state = row.get("player_state")
            binding = row.get("editor_binding")
            if (method != REAL_PLAYER_OBSERVATION_METHOD
                    or not _integer(observation_t_ns)
                    or not _integer(source_t_ns)
                    or abs(observation_t_ns - source_t_ns) >
                    PLAYER_OBSERVATION_TOLERANCE_NS
                    or not _player_state_valid(player_state)
                    or not isinstance(binding, dict)):
                return [f"{prefix}: player observation method/time/state is invalid"]
            telemetry_path_value = row.get("player_telemetry_path")
            telemetry_sha256 = row.get("player_telemetry_sha256")
            telemetry_frame = row.get("player_telemetry_frame")
            if (not isinstance(telemetry_path_value, str) or not telemetry_path_value
                    or not _sha256(telemetry_sha256)
                    or telemetry_frame != frame or isinstance(telemetry_frame, bool)):
                return [f"{prefix}: sealed player telemetry reference is invalid"]
            telemetry_path = Path(telemetry_path_value)
            if not telemetry_path.is_absolute():
                return [f"{prefix}: player telemetry path is not absolute"]
            telemetry_path = telemetry_path.resolve()
            reference = (telemetry_path, telemetry_sha256)
            telemetry_refs.add(reference)
            if reference not in telemetry_cache:
                try:
                    if sha256_file(telemetry_path) != telemetry_sha256:
                        return [f"{prefix}: sealed player telemetry sha256 changed"]
                    telemetry_rows = read_jsonl(telemetry_path)
                except (OSError, UnicodeError, ContractError) as exc:
                    return [f"{prefix}: sealed player telemetry is unavailable: {exc}"]
                if len(telemetry_rows) != n_frames:
                    return [f"{prefix}: sealed player telemetry frame count is invalid"]
                telemetry_cache[reference] = telemetry_rows
            telemetry = telemetry_cache[reference][frame]
            if (not isinstance(telemetry, dict)
                    or telemetry.get("schema") != PLAYER_TELEMETRY_SCHEMA
                    or telemetry.get("player_observation_method") != method
                    or telemetry.get("frame") != frame
                    or telemetry.get("replay_sha256") != expected_replay_sha256
                    or telemetry.get("source_clip_identity") !=
                    binding.get("clip_identity")
                    or telemetry.get("source_t_ns") != source_t_ns
                    or telemetry.get("observation_t_ns") != observation_t_ns
                    or telemetry.get("entity_state_hash") != row.get("entity_state_hash")
                    or not _player_state_equal(telemetry.get("player_state"), player_state)):
                return [f"{prefix}: frame event is not bound to sealed player telemetry"]
    if len(telemetry_refs) != 1:
        return ["four lanes do not reference one sealed master player telemetry artifact"]
    return []


def _quat_error_deg(a, b) -> float:
    dot = abs(sum(float(a[i]) * float(b[i]) for i in range(4)))
    return math.degrees(2.0 * math.acos(max(-1.0, min(1.0, dot))))


def validate_alignment(event_paths: dict[int, str], tape: dict, *, allow_mock: bool,
                       expected_replay_sha256: str | None = None,
                       expected_render_profile_sha256: str | None = None,
                       expected_output_root: str | None = None,
                       position_tolerance_m: float = 0.001,
                       rotation_tolerance_deg: float =
                       GTA_FLOAT_MATRIX_ROTATION_TOLERANCE_DEG,
                       intrinsics_tolerance_px: float = 0.5,
                       replay_time_tolerance_ms: float = 1.0
                       ) -> tuple[AlignmentReport, dict[int, list[dict]]]:
    n_frames = tape["n_frames"]
    lanes: dict[int, list[dict]] = {}
    problems: list[str] = []
    hashes = {}
    replay_hashes = set()
    any_mock = False
    max_position_error = 0.0
    max_rotation_error = 0.0
    max_intrinsics_error = 0.0
    max_interlane_delta_ns = 0
    for lane in range(1, 5):
        path = Path(event_paths[lane])
        hashes[str(lane)] = sha256_file(path)
        rows = read_jsonl(path)
        lanes[lane] = rows
        if len(rows) != n_frames:
            problems.append(f"lane {lane}: expected {n_frames} events, got {len(rows)}")
            continue
        first_replay_time = rows[0].get("replay_time_ms")
        if (not isinstance(first_replay_time, (int, float))
                or not math.isfinite(float(first_replay_time))
                or float(first_replay_time) < 0.0):
            problems.append(f"lane {lane}: first actual replay time is invalid")
            continue
        for frame, row in enumerate(rows):
            if row.get("schema") != FRAME_SCHEMA:
                problems.append(f"lane {lane} frame {frame}: schema mismatch")
                break
            if row.get("lane") != lane or row.get("output_frame") != frame:
                problems.append(f"lane {lane} frame {frame}: lane/output index mismatch")
                break
            sample = tape["lanes"][str(lane)]["samples"][frame]
            if row.get("source_t_ns") != sample["source_t_ns"] or row.get("replay_frame") != frame:
                problems.append(f"lane {lane} frame {frame}: replay timeline mismatch")
                break
            replay_time = row.get("replay_time_ms")
            if (not isinstance(replay_time, (int, float))
                    or not math.isfinite(float(replay_time))
                    or float(replay_time) < 0.0):
                problems.append(f"lane {lane} frame {frame}: actual replay time is invalid")
                break
            expected_delta_ms = sample["source_t_ns"] / 1_000_000.0
            actual_delta_ms = float(replay_time) - float(first_replay_time)
            if abs(actual_delta_ms - expected_delta_ms) > replay_time_tolerance_ms:
                problems.append(
                    f"lane {lane} frame {frame}: actual replay clock differs from 30fps grid by "
                    f"{abs(actual_delta_ms - expected_delta_ms):.6f}ms")
                break
            if row.get("render_succeeded") is not True:
                problems.append(f"lane {lane} frame {frame}: render did not succeed")
                break
            if not isinstance(row.get("entity_state_hash"), str) or len(row["entity_state_hash"]) != 64:
                problems.append(f"lane {lane} frame {frame}: entity state hash missing")
                break
            final_camera = row.get("final_camera")
            if not isinstance(final_camera, dict):
                problems.append(f"lane {lane} frame {frame}: final camera missing")
                break
            player_state = row.get("player_state")
            if (not isinstance(player_state, dict)
                    or not _finite_vec(player_state.get("position"), 3)
                    or not _finite_vec(player_state.get("quaternion"), 4)
                    or not _finite_vec(player_state.get("euler_deg"), 3)
                    or not _finite_vec(player_state.get("velocity"), 3)):
                problems.append(f"lane {lane} frame {frame}: actual player state is missing")
                break
            position = final_camera.get("position")
            quaternion = final_camera.get("quaternion")
            intrinsics = final_camera.get("intrinsics")
            if not _finite_vec(position, 3) or not _finite_vec(quaternion, 4):
                problems.append(f"lane {lane} frame {frame}: final pose is invalid")
                break
            quat_norm = math.sqrt(sum(float(x) ** 2 for x in quaternion))
            if abs(quat_norm - 1.0) > 1e-3:
                problems.append(f"lane {lane} frame {frame}: final quaternion is not normalized")
                break
            if (not isinstance(intrinsics, dict)
                    or any(not isinstance(intrinsics.get(key), (int, float))
                           or not math.isfinite(float(intrinsics[key]))
                           for key in ("fx", "fy", "cx", "cy"))
                    or intrinsics["fx"] <= 0 or intrinsics["fy"] <= 0
                    or abs(float(intrinsics["fx"]) - float(intrinsics["fy"])) > 1e-4):
                problems.append(f"lane {lane} frame {frame}: final intrinsics are invalid")
                break
            position_error = math.sqrt(sum(
                (float(position[axis]) - float(sample["position"][axis])) ** 2
                for axis in range(3)))
            rotation_error = _quat_error_deg(quaternion, sample["quaternion"])
            intrinsics_error = max(abs(float(intrinsics[key]) - float(sample["intrinsics"][key]))
                                   for key in ("fx", "fy", "cx", "cy"))
            max_position_error = max(max_position_error, position_error)
            max_rotation_error = max(max_rotation_error, rotation_error)
            max_intrinsics_error = max(max_intrinsics_error, intrinsics_error)
            if position_error > position_tolerance_m:
                problems.append(
                    f"lane {lane} frame {frame}: final position differs from tape by "
                    f"{position_error:.6f}m")
                break
            if rotation_error > rotation_tolerance_deg:
                problems.append(
                    f"lane {lane} frame {frame}: final rotation differs from tape by "
                    f"{rotation_error:.6f}deg")
                break
            if intrinsics_error > intrinsics_tolerance_px:
                problems.append(
                    f"lane {lane} frame {frame}: final intrinsics differ from tape by "
                    f"{intrinsics_error:.6f}px")
                break
            replay_hashes.add(row.get("replay_sha256"))
            any_mock = any_mock or row.get("mock") is True

    player_telemetry_problems: list[str] = []
    if not allow_mock:
        problems.extend(validate_pixel_pose_fence(
            lanes, expected_replay_sha256, expected_render_profile_sha256,
            expected_output_root))
        player_telemetry_problems = validate_player_telemetry(
            lanes, expected_replay_sha256)
        problems.extend(player_telemetry_problems)

    if len(replay_hashes) != 1 or None in replay_hashes:
        problems.append("lanes do not reference one replay sha256")
    elif expected_replay_sha256 is not None and next(iter(replay_hashes)) != expected_replay_sha256:
        problems.append("frame events reference a replay different from the sealed master")
    if any_mock and not allow_mock:
        problems.append("mock frame events are forbidden in real capture")
    if all(len(lanes.get(lane, [])) == n_frames for lane in range(1, 5)):
        for frame in range(n_frames):
            states = {lanes[lane][frame].get("entity_state_hash") for lane in range(1, 5)}
            times = {lanes[lane][frame].get("source_t_ns") for lane in range(1, 5)}
            replay_times = [float(lanes[lane][frame].get("replay_time_ms", math.nan))
                            for lane in range(1, 5)]
            if len(states) != 1:
                problems.append(f"frame {frame}: entity state differs across lanes")
                break
            if len(times) != 1:
                problems.append(f"frame {frame}: source time differs across lanes")
                break
            if not all(math.isfinite(value) for value in replay_times):
                problems.append(f"frame {frame}: actual replay time is missing across lanes")
                break
            delta_ns = int(round((max(replay_times) - min(replay_times)) * 1_000_000.0))
            max_interlane_delta_ns = max(max_interlane_delta_ns, delta_ns)
            if delta_ns > int(replay_time_tolerance_ms * 1_000_000.0):
                problems.append(
                    f"frame {frame}: actual replay time differs across lanes by {delta_ns}ns")
                break
    report = AlignmentReport(
        ok=not problems,
        n_frames=n_frames,
        replay_sha256=next(iter(replay_hashes)) if len(replay_hashes) == 1 else None,
        problems=problems,
        lane_event_sha256=hashes,
        mock=any_mock,
        max_position_error_m=max_position_error,
        max_rotation_error_deg=max_rotation_error,
        max_intrinsics_error_px=max_intrinsics_error,
        logical_interlane_delta_ns_max=max_interlane_delta_ns if not problems else None,
        player_telemetry_diagnostic_ok=(not allow_mock and not player_telemetry_problems),
        player_replay_origin_verified=False,
    )
    return report, lanes


def _timestamp(epoch: str, source_t_ns: int) -> str:
    try:
        t0 = datetime.strptime(epoch, "%Y-%m-%d %H:%M:%S.%f")
    except ValueError as exc:
        raise ContractError("source_epoch must use YYYY-MM-DD HH:MM:SS.mmm") from exc
    t = t0 + timedelta(microseconds=source_t_ns / 1000)
    return t.strftime("%Y-%m-%d %H:%M:%S.") + f"{t.microsecond // 1000:03d}"


def _speed(positions: list[list[float]], index: int) -> list[float]:
    if index == 0:
        return [0.0, 0.0, 0.0]
    return [round((float(positions[index][axis]) - float(positions[index - 1][axis])) * 30.0, 6)
            for axis in range(3)]


def _round_vec(value, digits=6) -> list[float]:
    return [round(float(x), digits) for x in value]


def _gta_to_delivery_vector(value) -> list[float]:
    """Map GTA ``[right, front, up]`` to docs/69 ``[right, up, front]``.

    Swapping Y/Z changes the handedness from GTA's native right-handed frame
    to the delivery contract's left-handed frame.  This is a basis conversion,
    not a camera motion, and therefore preserves lengths and finite differences.
    """
    if not _finite_vec(value, 3):
        raise ContractError("GTA coordinate conversion requires a finite vec3")
    return [float(value[0]), float(value[2]), float(value[1])]


def _gta_to_delivery_quaternion(value) -> list[float]:
    """Convert an axial GTA quaternion into the docs/69 delivery basis.

    For the polar-vector basis permutation ``P(x,y,z)=(x,z,y)``, ``det(P)=-1``.
    Quaternion vector parts are axial, so they transform as ``det(P) * P``;
    the scalar part is unchanged.  This is equivalent to ``P R P^-1`` and
    keeps the rotation proper even though the coordinate basis changes
    handedness.
    """
    if not _finite_vec(value, 4):
        raise ContractError("GTA quaternion conversion requires a finite vec4")
    x, y, z, w = (float(component) for component in value)
    norm = math.sqrt(x * x + y * y + z * z + w * w)
    if abs(norm - 1.0) > 1e-3:
        raise ContractError("GTA quaternion conversion requires a normalized quaternion")
    return [-x / norm, -z / norm, -y / norm, w / norm]


def _delivery_euler_deg(quaternion) -> list[float]:
    """Return docs/69 ``[X pitch, Y yaw, Z roll]`` angles in degrees.

    docs/69 does not lock an Euler multiplication order, so the quaternion is
    the rotation source of truth.  These angles use the same forward/roll
    observables as engine_qc: yaw about +Y, pitch from the Y component of the
    forward vector, and roll about the optical +Z axis.
    """
    if not _finite_vec(quaternion, 4):
        raise ContractError("delivery Euler conversion requires a finite vec4")
    x, y, z, w = (float(component) for component in quaternion)
    forward_x = 2.0 * (x * z + w * y)
    forward_y = 2.0 * (y * z - w * x)
    forward_z = 1.0 - 2.0 * (x * x + y * y)
    pitch = math.degrees(math.asin(max(-1.0, min(1.0, forward_y))))
    yaw = math.degrees(math.atan2(forward_x, forward_z))
    roll_term = 2.0 * (x * y + w * z)
    roll = math.degrees(math.asin(max(-1.0, min(1.0, roll_term))))
    return [pitch, yaw, roll]


def materialize_metadata(capture_dir: Path, lanes: dict[int, list[dict]], tape: dict,
                         *, source_epoch: str, process_name: str, metric_scale: float,
                         dry_run: bool) -> dict:
    capture_dir.mkdir(parents=True, exist_ok=True)
    written = []
    convert_gta_coordinates = (
        tape.get("coordinate_convention") == GTA_NATIVE_COORDINATE_CONVENTION
    )
    for lane in range(1, 5):
        lane_events = lanes[lane]
        camera_positions = [
            (_gta_to_delivery_vector(row["final_camera"]["position"])
             if convert_gta_coordinates else
             [float(value) for value in row["final_camera"]["position"]])
            for row in lane_events
        ]
        player_positions = [
            (_gta_to_delivery_vector(row["player_state"]["position"])
             if convert_gta_coordinates else
             [float(value) for value in row["player_state"]["position"]])
            for row in lane_events
        ]
        action = []
        for frame, row in enumerate(lane_events):
            camera = row["final_camera"]
            player = row["player_state"]
            camera_position = camera_positions[frame]
            player_position = player_positions[frame]
            camera_quaternion = (
                _gta_to_delivery_quaternion(camera["quaternion"])
                if convert_gta_coordinates else
                [float(value) for value in camera["quaternion"]]
            )
            player_quaternion = (
                _gta_to_delivery_quaternion(player["quaternion"])
                if convert_gta_coordinates else
                [float(value) for value in player["quaternion"]]
            )
            euler = (_delivery_euler_deg(camera_quaternion)
                     if convert_gta_coordinates else
                     camera.get("euler_deg", [0.0, 0.0, 0.0]))
            player_euler = (_delivery_euler_deg(player_quaternion)
                            if convert_gta_coordinates else
                            player["euler_deg"])
            rec = {
                "time": _timestamp(source_epoch, row["source_t_ns"]),
                "fps": 30.0,
                "frame": frame,
                "camera_position": _round_vec(camera_position),
                "camera_rotation_quaternion": _round_vec(camera_quaternion),
                "camera_Follow Offset": _round_vec([
                    float(camera_position[axis]) - float(player_position[axis])
                    for axis in range(3)
                ]),
                "camera_intrinsics": {key: round(float(camera["intrinsics"][key]), 6)
                                      for key in ("fx", "fy", "cx", "cy")},
                "camera_speed": _speed(camera_positions, frame),
            }
            if lane == 1:
                input_state = row.get("input")
                if not isinstance(input_state, dict):
                    raise ContractError(
                        f"lane 1 frame {frame}: input object is missing or invalid; "
                        "unobserved mouse input cannot be derived from camera motion")
                # Camera motion does not establish a mouse observation. This
                # also applies to mock materialization: its backend already
                # supplies explicit input values, separately labelled mock.
                mouse = {}
                for key in ("mouse_dx", "mouse_dy"):
                    value = input_state.get(key)
                    if (not isinstance(value, (int, float))
                            or isinstance(value, bool)):
                        raise ContractError(
                            f"lane 1 frame {frame}: input.{key} must be an explicit "
                            "finite number; missing input cannot be inferred")
                    try:
                        number = float(value)
                    except OverflowError as exc:
                        raise ContractError(
                            f"lane 1 frame {frame}: input.{key} must be finite") from exc
                    if not math.isfinite(number):
                        raise ContractError(
                            f"lane 1 frame {frame}: input.{key} must be finite")
                    mouse[key] = round(number, 6)
                rec.update({
                    "camera_rotation_oula": _round_vec(euler, 4),
                    "player_position": _round_vec(player_position),
                    "player_rotation_oula": _round_vec(player_euler, 4),
                    "player_rotation_quaternion": _round_vec(player_quaternion),
                    "player_speed": _speed(player_positions, frame),
                    "mouse_x": 0.5,
                    "mouse_y": 0.5,
                    "mouse_dx": mouse["mouse_dx"],
                    "mouse_dy": mouse["mouse_dy"],
                    "keyCode": input_state.get("keyCode", []),
                    "metric_scale": float(metric_scale),
                })
            action.append(rec)
        action_path = capture_dir / f"action_camera_{lane}.json"
        write_json_atomic(action_path, action, indent=None)
        resolution = tape["lanes"][str(lane)]["resolution"]
        system_path = capture_dir / f"systeminfo_{lane}.json"
        write_json_atomic(system_path, {
            "gameProcessName": process_name,
            "x": 0,
            "y": 0,
            "width": resolution["width"],
            "height": resolution["height"],
            "recordDpi": 1.0,
        })
        written.extend([str(action_path.resolve()), str(system_path.resolve())])
    return {
        "schema": "dm-gta-metadata-materialization/1",
        "files": written,
        "timestamp_semantics": "source_replay_timeline",
        "input_source": ("mock_scripted_camera" if dry_run
                         else "native_task_go_straight+scripted_camera"),
        "source_coordinate_convention": tape.get("coordinate_convention"),
        "output_coordinate_convention": (
            DELIVERY_COORDINATE_CONVENTION if convert_gta_coordinates
            else tape.get("coordinate_convention")
        ),
        "coordinate_transform": (
            "position=[x,z,y]; quaternion=[-x,-z,-y,w]"
            if convert_gta_coordinates else "identity"
        ),
        "player_speed_semantics": "delivery_position_finite_difference_30hz",
        "metric_scale": metric_scale,
        "dry_run": dry_run,
    }

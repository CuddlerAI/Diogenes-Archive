"""Place a trajgen clip into GTA world coordinates (docs/158 §5.3 ``gta_traj_adapter``).

trajgen authors a clip in the docs/69 delivery basis — ``[x_right, y_up, z_front]``,
left-handed — around a scene-local origin.  GTA's native camera basis is
``[x_right, y_front, z_up]``, right-handed, in absolute world coordinates.  A tape
that never crosses that gap is not flyable: ``orchestrator``/``staged`` reject any
real capture whose tape is not :data:`~gta_capture.tape.GTA_COORDINATE_CONVENTION`.

The placement is a **rigid motion**: basis permutation, yaw about GTA world Z, then
translation to a surveyed Story Mode anchor.  Rigidity is the whole point — every
docs/69 acceptance property that this repository can pre-compute (covisibility,
decoupling, cam4 form, six-translation honesty, physical limits) is invariant under
it, so a plan that passes ``engine_qc --mode mc`` before placement still describes
the same geometry after it.  :func:`place_trajgen_clip` re-verifies that invariance
on the produced samples instead of trusting the derivation.

What placement does **not** do: it does not prove the corridor is free of GTA
geometry, does not calibrate projection, and does not make the clip deliverable.
Collision clearance stays with the scenario survey, and the PR #405 gate stays
fail-closed on its own evidence.
"""

from __future__ import annotations

import math
import copy
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Sequence

from .errors import ContractError
from .jsonio import load_json, sha256_file
from .tape import (GTA_COORDINATE_CONVENTION, LANES, TAPE_SCHEMA, _euler_zxy_deg,
                   _quaternion_zxy, _source_t_ns, validate_tape)


PLACEMENT_SCHEMA = "dm-gta-trajgen-placement/1"
SUBJECT_ROUTE_SCHEMA = "dm-gta-subject-route/1"

#: Minimum camera height above the surveyed anchor plane (m).  A trajgen scene
#: keeps cameras at least ``ground_y + 0.6`` up; anything at or below the anchor
#: plane after placement means the wrong ``ground_y`` was supplied.
MIN_CAMERA_CLEARANCE_M = 0.30
#: Horizontal excursion cap from the anchor (m).  Story Mode streaming budget and
#: the scenario survey are both authored around a bounded working volume; a clip
#: that leaves it is a survey error, not something to discover mid-export.
MAX_HORIZONTAL_EXCURSION_M = 300.0
#: Worst-case rigidity residual accepted on the placement self-check (m).  The
#: transform is exact in real arithmetic, so this only absorbs float64 rounding.
RIGIDITY_RESIDUAL_TOL_M = 1e-6


def _finite(value: Any) -> bool:
    return isinstance(value, (int, float)) and not isinstance(value, bool) \
        and math.isfinite(float(value))


def _finite_vec(value: Any, n: int) -> bool:
    return isinstance(value, (list, tuple)) and len(value) == n \
        and all(_finite(item) for item in value)


@dataclass(frozen=True)
class Placement:
    """Where a trajgen clip is pinned in the GTA world.

    ``anchor`` is the GTA world point directly under the observed subject at
    ``t=0`` — the value ``gta_capture probe-game`` reads from Story Mode, with
    ``z`` on the walkable surface.  ``heading_deg`` is a GTA heading (0 = +Y,
    increasing counter-clockwise about world Z), applied to the whole rig so the
    authored route can be aimed down a real street.  ``ground_y`` is the trajgen
    scene's ground plane in the delivery basis; every built-in synthetic scene
    uses 0.0, but a ``--scene-file`` clip may not, and guessing it would silently
    sink or float the entire camera rig.
    """

    anchor: tuple[float, float, float]
    heading_deg: float
    ground_y: float = 0.0

    def __post_init__(self) -> None:
        if not _finite_vec(self.anchor, 3):
            raise ContractError("placement anchor must be a finite GTA world vec3")
        if not _finite(self.heading_deg) or not 0.0 <= float(self.heading_deg) < 360.0:
            raise ContractError("placement heading must be a finite heading in [0, 360)")
        if not _finite(self.ground_y):
            raise ContractError("placement ground_y must be finite")

    def to_dict(self) -> dict:
        return {
            "schema": PLACEMENT_SCHEMA,
            "anchor": [float(value) for value in self.anchor],
            "heading_deg": float(self.heading_deg),
            "ground_y": float(self.ground_y),
            "note": "rigid placement: delivery->GTA basis swap, yaw about world Z, "
                    "translate to the surveyed Story Mode anchor",
        }


def delivery_to_gta_vector(value: Sequence[float]) -> list[float]:
    """Map docs/69 ``[right, up, front]`` to GTA ``[right, front, up]``.

    This is the same Y/Z swap ``evidence._gta_to_delivery_vector`` applies in the
    other direction.  The permutation is its own inverse, so the two functions are
    one convention expressed twice rather than two independent derivations.
    """
    if not _finite_vec(value, 3):
        raise ContractError("delivery->GTA conversion requires a finite vec3")
    return [float(value[0]), float(value[2]), float(value[1])]


def delivery_to_gta_quaternion(value: Sequence[float]) -> list[float]:
    """Conjugate a delivery quaternion into the GTA basis.

    For the basis permutation ``P(x,y,z)=(x,z,y)`` with ``det(P) = -1``, quaternion
    vector parts are axial and transform as ``det(P) * P``.  The map is an
    involution, so it converts in both directions; ``evidence`` owns the GTA->delivery
    spelling and this owns the inverse used before a tape exists.
    """
    if not _finite_vec(value, 4):
        raise ContractError("delivery->GTA conversion requires a finite vec4")
    x, y, z, w = (float(component) for component in value)
    norm = math.sqrt(x * x + y * y + z * z + w * w)
    if not math.isfinite(norm) or norm <= 0.0:
        raise ContractError("delivery quaternion must be non-degenerate")
    x, y, z, w = (component / norm for component in (x, y, z, w))
    return [-x, -z, -y, w]


def _yaw_rotate(x: float, y: float, heading_rad: float) -> tuple[float, float]:
    """Rotate a GTA horizontal vector by a GTA heading.

    GTA reads yaw as ``atan2(-forward.x, forward.y)``, so heading ``h`` sends the
    zero-heading forward ``(0, 1)`` to ``(-sin h, cos h)`` — the standard
    counter-clockwise rotation about world Z.  Position and orientation must use
    this one definition or the rig shears: the cameras would be rotated relative
    to the route they were authored against.
    """
    cos_h, sin_h = math.cos(heading_rad), math.sin(heading_rad)
    return x * cos_h - y * sin_h, x * sin_h + y * cos_h


class _Transform:
    """The rigid delivery->GTA-world map for one placement and one clip origin."""

    def __init__(self, placement: Placement, origin_delivery: Sequence[float]):
        self.placement = placement
        self.heading_rad = math.radians(float(placement.heading_deg))
        self.origin = [float(value) for value in origin_delivery]

    def point(self, value: Sequence[float]) -> list[float]:
        if not _finite_vec(value, 3):
            raise ContractError("trajgen sample position must be a finite vec3")
        local = [float(value[index]) - self.origin[index] for index in range(3)]
        right, front, up = delivery_to_gta_vector(local)
        x, y = _yaw_rotate(right, front, self.heading_rad)
        anchor = self.placement.anchor
        return [float(anchor[0]) + x, float(anchor[1]) + y, float(anchor[2]) + up]

    def orientation(self, quaternion: Sequence[float]) -> tuple[list[float], list[float]]:
        """Return ``(quaternion, euler_deg)`` in GTA order-2 after the yaw.

        A world-Z rotation left-multiplies GTA's ``qz * qx * qy`` factorisation, so
        it lands entirely on the yaw term and leaves pitch and roll untouched.
        Adding the heading in Euler space is therefore exact, not an approximation,
        and it avoids re-deriving a quaternion product that ``tape`` already owns.
        """
        gta = delivery_to_gta_quaternion(quaternion)
        pitch, roll, yaw = _euler_zxy_deg(gta)
        yaw = (yaw + float(self.placement.heading_deg) + 180.0) % 360.0 - 180.0
        euler = [pitch, roll, yaw]
        return _quaternion_zxy(euler), euler


def _clip_anchor_track(plan: dict) -> list[dict]:
    anchors = plan.get("anchors")
    if not isinstance(anchors, list) or not anchors:
        raise ContractError("trajgen plan has no anchor track to place against")
    track = []
    for item in anchors:
        if (not isinstance(item, dict) or not _finite(item.get("t"))
                or not _finite_vec(item.get("p"), 3)):
            raise ContractError("trajgen anchor track entry is malformed")
        track.append({"t": float(item["t"]), "p": [float(v) for v in item["p"]]})
    track.sort(key=lambda item: item["t"])
    if abs(track[0]["t"]) > 1e-9:
        raise ContractError("trajgen anchor track must start at t=0")
    return track


def _check_rigidity(before: list[list[float]], after: list[list[float]]) -> float:
    """Return the worst pairwise-distance change introduced by the placement.

    A basis permutation composed with a rotation and a translation preserves every
    distance.  Verifying that on the emitted samples catches what breaks isometry:
    a heading applied to one horizontal axis but not the other, a stray scale, or
    an origin subtracted from some samples only.

    It is deliberately not more than that.  Every permutation and reflection is
    itself an isometry, so a *wrong* axis order or handedness passes this check,
    and positions say nothing about orientation.  Those are pinned by the tests
    instead (identity round trip against authored GTA poses; cameras still aimed
    at the subject under a non-zero heading).
    """
    worst = 0.0
    for first in range(len(before)):
        for second in range(first + 1, len(before)):
            source = math.dist(before[first], before[second])
            target = math.dist(after[first], after[second])
            worst = max(worst, abs(source - target))
    return worst


def place_trajgen_clip(clip_dir, *, placement: Placement, filmback_id: str,
                       clip_id: str | None = None) -> tuple[dict, dict]:
    """Convert a trajgen clip into a GTA-world camera tape plus its subject route.

    Returns ``(tape, subject_route)``.  The tape is what the native controller
    flies; the subject route is the same rigid image of the plan's anchor track,
    projected onto the anchor plane, and is what the ASI must walk the controlled
    character along.  Both come from one transform, so the cameras look where the
    character actually is instead of where a separately authored route guessed.
    """
    root = Path(clip_dir)
    plan_path = root / "plan.json"
    if not plan_path.is_file():
        raise ContractError(f"trajgen clip is missing {plan_path}")
    plan = load_json(plan_path)
    if plan.get("mode") != "mc":
        raise ContractError("GTA placement requires a docs/69 four-lane mc clip")
    cameras = plan.get("cameras")
    if not isinstance(cameras, list) or len(cameras) != 4:
        raise ContractError("trajgen plan must describe exactly four cameras")
    focal_by_id = {str(camera.get("id")): camera.get("focal_mm") for camera in cameras
                   if isinstance(camera, dict)}
    duration = float(plan.get("duration_s", 0))
    if not _finite(duration) or duration <= 0:
        raise ContractError("trajgen plan duration is invalid")

    anchor_track = _clip_anchor_track(plan)
    origin = [anchor_track[0]["p"][0], float(placement.ground_y), anchor_track[0]["p"][2]]
    transform = _Transform(placement, origin)

    lanes: dict[str, dict] = {}
    n_frames: int | None = None
    source_files: list[dict] = []
    rigidity_probe_before: list[list[float]] = []
    rigidity_probe_after: list[list[float]] = []
    for lane_id in LANES:
        action_path = root / f"action_camera_{lane_id}.json"
        system_path = root / f"systeminfo_{lane_id}.json"
        if not action_path.is_file() or not system_path.is_file():
            raise ContractError(f"trajgen clip is missing lane {lane_id} action/systeminfo")
        records = load_json(action_path)
        system = load_json(system_path)
        if not isinstance(records, list) or not records:
            raise ContractError(f"action_camera_{lane_id}.json must be a non-empty array")
        if n_frames is None:
            n_frames = len(records)
        if len(records) != n_frames:
            raise ContractError("trajgen lanes have different frame counts")
        samples = []
        for index, record in enumerate(records):
            if not isinstance(record, dict):
                raise ContractError(f"action_camera_{lane_id}.json[{index}] is not an object")
            position = transform.point(record.get("camera_position"))
            quaternion, euler = transform.orientation(record.get("camera_rotation_quaternion"))
            intrinsics = record.get("camera_intrinsics")
            if not isinstance(intrinsics, dict):
                raise ContractError(f"action_camera_{lane_id}.json[{index}] has no intrinsics")
            height_above_anchor = position[2] - float(placement.anchor[2])
            if height_above_anchor < MIN_CAMERA_CLEARANCE_M:
                raise ContractError(
                    f"lane {lane_id} frame {index} lands {height_above_anchor:.3f} m above the "
                    f"anchor plane (< {MIN_CAMERA_CLEARANCE_M} m); check placement ground_y")
            excursion = math.hypot(position[0] - float(placement.anchor[0]),
                                   position[1] - float(placement.anchor[1]))
            if excursion > MAX_HORIZONTAL_EXCURSION_M:
                raise ContractError(
                    f"lane {lane_id} frame {index} is {excursion:.1f} m from the anchor "
                    f"(> {MAX_HORIZONTAL_EXCURSION_M} m); resurvey the scenario")
            samples.append({
                "frame": index,
                "source_t_ns": _source_t_ns(index),
                "position": [round(value, 9) for value in position],
                "quaternion": [round(value, 12) for value in quaternion],
                "euler_deg": [round(value, 9) for value in euler],
                "intrinsics": intrinsics,
            })
            if index in (0, len(records) // 2, len(records) - 1):
                rigidity_probe_before.append(
                    [float(value) for value in record["camera_position"]])
                rigidity_probe_after.append(position)
        lanes[lane_id] = {
            "focal_mm": focal_by_id.get(lane_id),
            "resolution": {"width": system.get("width"), "height": system.get("height")},
            "samples": samples,
        }
        source_files.extend([
            {"name": action_path.name, "sha256": sha256_file(action_path)},
            {"name": system_path.name, "sha256": sha256_file(system_path)},
        ])

    residual = _check_rigidity(rigidity_probe_before, rigidity_probe_after)
    if residual > RIGIDITY_RESIDUAL_TOL_M:
        raise ContractError(
            f"placement changed inter-camera geometry by {residual:.3e} m; "
            "the transform is not rigid and every covisibility property is void")

    for name in ("plan.json", "covis.json"):
        candidate = root / name
        if candidate.is_file():
            source_files.append({"name": name, "sha256": sha256_file(candidate)})

    tape = {
        "schema": TAPE_SCHEMA,
        "clip_id": clip_id or root.name,
        "fps_num": 30,
        "fps_den": 1,
        "duration_s": duration,
        "n_frames": n_frames,
        "timestamp_semantics": "source_replay_timeline",
        "filmback_id": filmback_id,
        "coordinate_convention": GTA_COORDINATE_CONVENTION,
        "source": {
            "kind": "trajgen_clip_placed",
            "path": str(root.resolve()),
            "files": source_files,
            # staged._validate_real_inputs requires source.anchor to equal the
            # scenario location, so the placed tape must expose the anchor at the
            # same key make_gta_poc_tape uses. Without it the operator-gated path
            # rejects every placed tape and this whole adapter is unreachable.
            "anchor": [float(value) for value in placement.anchor],
            "placement": placement.to_dict(),
            "rigidity_residual_m": residual,
            "cam4_mode": (plan.get("cam4") or {}).get("mode", "timewarp"),
            "scene": plan.get("scene"),
            "scene_kind": plan.get("scene_kind"),
            "seed": plan.get("seed"),
            # Placement is geometry only. Collision clearance, projection
            # calibration and delivery eligibility are decided elsewhere.
            "delivery_eligible": False,
            "collision_clearance_verified": False,
        },
        "lanes": lanes,
    }
    tape = validate_tape(tape, expected_duration_s=duration)

    route = []
    for item in anchor_track:
        placed = transform.point(item["p"])
        route.append({
            "t": item["t"],
            "position": [round(value, 6) for value in placed[:2]] + [
                round(float(placement.anchor[2]), 6)],
            "look_at_height_m": round(placed[2] - float(placement.anchor[2]), 6),
        })
    subject_route = {
        "schema": SUBJECT_ROUTE_SCHEMA,
        "clip_id": tape["clip_id"],
        "duration_s": duration,
        "placement": placement.to_dict(),
        "source": {"name": "plan.json", "sha256": sha256_file(plan_path)},
        "note": "controlled character route in GTA world coordinates; z is the surveyed "
                "anchor plane and look_at_height_m is the authored aim height above it",
        "waypoints": route,
    }
    motion = plan.get("subject_motion")
    if isinstance(motion, dict) and motion.get("route_kind") == "recorded_master_telemetry":
        from .recorded_route import validate_recorded_subject_route
        from .scenario import Scenario

        recorded = copy.deepcopy(motion.get("recorded_route"))
        if not isinstance(recorded, dict):
            raise ContractError("recorded subject motion is missing its sealed observed route")
        try:
            scenario_path = recorded["observation"]["files"]["scenario"]["path"]
        except (KeyError, TypeError) as exc:
            raise ContractError("recorded subject route is missing its frozen scenario") from exc
        validate_recorded_subject_route(recorded, scenario=Scenario.load(scenario_path))
        expected_placement = motion.get("placement")
        if (not isinstance(expected_placement, dict)
                or expected_placement.get("anchor") != list(placement.anchor)
                or expected_placement.get("heading_deg") != placement.heading_deg
                or expected_placement.get("ground_y") != placement.ground_y
                or placement.heading_deg != 0.0):
            raise ContractError("recorded subject plan must use its measured world placement")
        waypoints = recorded.get("waypoints", [])
        if len(waypoints) != n_frames or len(anchor_track) != n_frames:
            raise ContractError("recorded subject route must cover every camera frame")
        for frame, (anchor_sample, waypoint) in enumerate(zip(anchor_track, waypoints)):
            placed = transform.point(anchor_sample["p"])
            expected = list(waypoint["position"])
            expected[2] += waypoint["look_at_height_m"]
            if (abs(anchor_sample["t"] - frame / 30.0) > 1e-8
                    or max(abs(placed[axis] - expected[axis]) for axis in range(3)) > 1e-5):
                raise ContractError("recorded plan anchor differs from its sealed observed route")
        recorded.update({"clip_id": tape["clip_id"], "placement": placement.to_dict(),
                         "source": {"name": "plan.json", "sha256": sha256_file(plan_path)}})
        tape["source"].update({"subject_route": recorded,
                               "native_command": copy.deepcopy(recorded["native_command"]),
                               "observation_semantics": "recording_side_estimate"})
        return tape, recorded
    if motion is not None:
        # Only an explicit, numerically verified native-compatible track may
        # bind a placed tape to Story Mode's existing forward-route executor.
        # A generic sparse/meandering trajgen anchor remains unbound.
        if (not isinstance(motion, dict)
                or motion.get("control") != "native_task_go_straight"
                or motion.get("route_kind") != "forward_from_anchor"
                or motion.get("heading_deg") != 0.0
                or motion.get("ground_y") != placement.ground_y):
            raise ContractError("trajgen subject_motion is not a native forward route")
        distance, speed = motion.get("distance_m"), motion.get("speed_mps")
        if (not _finite(distance) or not _finite(speed)
                or not 0 < distance <= 100 or not 0 < speed <= 10
                or len(route) != n_frames):
            raise ContractError("native subject route parameters/frame count are invalid")
        heading = math.radians(placement.heading_deg)
        direction = [-math.sin(heading), math.cos(heading)]
        for frame, waypoint in enumerate(route):
            if abs(waypoint["t"] - frame / 30.0) > 1e-8:
                raise ContractError("native subject route must sample every camera frame")
            travel = min(distance, speed * waypoint["t"])
            expected = [placement.anchor[axis] + direction[axis] * travel
                        for axis in range(2)]
            if max(abs(waypoint["position"][axis] - expected[axis])
                   for axis in range(2)) > 1e-5:
                raise ContractError("trajgen subject route deviates from native forward motion")
        subject_route.update({
            "control": motion["control"], "route_kind": motion["route_kind"],
            "distance_m": float(distance), "speed_mps": float(speed),
            "heading_deg": float(placement.heading_deg),
        })
        tape["source"].update({
            "route_distance_m": float(distance), "route_speed_mps": float(speed),
            "route_heading_deg": float(placement.heading_deg),
            "subject_route": subject_route,
        })
    return tape, subject_route

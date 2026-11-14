"""Build an MC plan around idealized or sealed recorded subject motion.

Source adaptation displaces the authored rig and reapplies the existing
translation correction.  Independent recorded choreography instead connects
explicit axis-translation windows around the observed route with smooth camera
repositioning.  Neither method substitutes the native task parameter for measured
walking speed.  Ordinary engine_qc checks the final exported numbers with its
unchanged thresholds.

Overlap is recomputed by ray projection against an explicitly identified planar
planning proxy.  It is never copied from the original clip or represented as
measured GTA geometry.  Captured-frame evidence remains necessary for delivery.
"""

from __future__ import annotations

import copy
import math
import sys
from bisect import bisect_right
from pathlib import Path
from types import SimpleNamespace

from .errors import ContractError
from .jsonio import load_json, sha256_file, write_json_atomic
from .traj_adapter import _clip_anchor_track


SUBJECT_HEIGHT_M = 1.85
SUBJECT_HALF_WIDTH_M = 0.35


def _independent_recorded_choreography(subject, *, lane: int, focal_mm: float,
                                      ground_y: float) -> tuple[list, list, list]:
    """Author independent camera stop/move windows around the observed walker.

    Each named translation is a world-space axis displacement over a quintic
    curve with zero endpoint velocity and acceleration.  Connecting curves move
    to the next observed route neighborhood.  The actor trajectory is unchanged.
    Three ground cameras aim at the proxy body center; cam4 remains topdown.
    """
    from trajgen.mathutil import (quat_from_yaw_elev_roll, smoothstep5, vadd,
                                 vscale, vsub, yaw_elev_from_forward)

    count = len(subject)
    duration = count / 30.0
    first = subject[0]
    dx, dz = subject[-1][0] - first[0], subject[-1][2] - first[2]
    path_length = math.hypot(dx, dz)
    if path_length < 1.0:
        raise ContractError("recorded walking choreography needs at least one meter of travel")
    direction = (dx / path_length, 0.0, dz / path_length)
    right = (direction[2], 0.0, -direction[0])
    route_yaw, _ = yaw_elev_from_forward(direction)
    base_radius = {18: 6.5, 24: 8.0, 35: 10.0, 50: 13.0}.get(int(focal_mm))
    if base_radius is None:
        raise ContractError("recorded walking choreography requires a supported focal length")
    sequence = {
        1: ("forward", "up", "left", "back", "down", "right"),
        2: ("left", "back", "down", "right", "forward", "up"),
        3: ("down", "right", "forward", "up", "left", "back"),
        4: ("back", "left", "up", "forward", "right", "down"),
    }[lane]
    phase = (lane - 1) * 1.5

    def target_at(t):
        frame = min(count - 1, max(0, t * 30.0))
        low = int(frame)
        high = min(count - 1, low + 1)
        weight = frame - low
        return tuple(subject[low][axis] * (1.0 - weight) + subject[high][axis] * weight
                     for axis in range(3))

    def base_at(t, height, *, initial=False):
        target = target_at(t)
        plane = ground_y + target[1] - first[1]
        if lane == 4:
            # Small deliberate cross-route variation keeps the overview from
            # duplicating a ground camera's connecting path.
            lateral = 0.0 if initial else 0.8 * math.sin(t * math.pi / 17.0)
            return (target[0] + right[0] * lateral, plane + height,
                    target[2] + right[2] * lateral)
        azimuth = 0.0 if initial else math.radians(
            (10.0 + 3.0 * lane) * math.sin(t * math.pi / (13.0 + 2.0 * lane) + lane * 0.6))
        radial = vadd(vscale(direction, -math.cos(azimuth)),
                      vscale(right, math.sin(azimuth)))
        return (target[0] + radial[0] * base_radius, plane + height,
                target[2] + radial[2] * base_radius)

    segments = []
    previous_t = 0.0
    previous_position = base_at(0.0, 20.0 if lane == 4 else 2.0, initial=True)
    for index, translation in enumerate(sequence):
        t0 = index * 10.0 + phase
        t1 = t0 + 3.0
        height = 20.0 if lane == 4 else (3.2 if translation == "down" else 2.0)
        p0 = (previous_position if t0 == 0.0 else base_at((t0 + t1) / 2.0, height))
        if t0 > previous_t:
            segments.append({"t0": previous_t, "t1": t0,
                "cls": "observed_subject_tracking", "p0": previous_position, "p1": p0,
                "params": {"family": "目标追踪", "subtype": "independent_quintic_reposition",
                    "taxonomy_status": "internal_custom_motion_not_customer_approved"}})
        if lane == 4:
            forward_axis = direction
        else:
            to_subject = vsub(target_at(t0), p0)
            horizontal = math.hypot(to_subject[0], to_subject[2])
            if horizontal < 1e-6:
                raise ContractError("ground translation window starts over the subject")
            forward_axis = (to_subject[0] / horizontal, 0.0, to_subject[2] / horizontal)
        right_axis = (forward_axis[2], 0.0, -forward_axis[0])
        axis = {"forward": forward_axis, "back": vscale(forward_axis, -1.0),
                "right": right_axis, "left": vscale(right_axis, -1.0),
                "up": (0.0, 1.0, 0.0), "down": (0.0, -1.0, 0.0)}[translation]
        displacement = 1.2 if translation in ("up", "down") else 1.6
        p1 = vadd(p0, vscale(axis, displacement))
        segments.append({"t0": t0, "t1": t1, "cls": "translate_" + translation,
            "p0": p0, "p1": p1,
            "params": {"family": "平移", "extent_m": displacement,
                       "profile": "quintic_zero_endpoint_velocity_acceleration"}})
        previous_t, previous_position = t1, p1
    if previous_t < duration:
        segments.append({"t0": previous_t, "t1": duration,
            "cls": "observed_subject_tracking", "p0": previous_position,
            "p1": base_at(duration, 20.0 if lane == 4 else 2.0),
            "params": {"family": "目标追踪", "subtype": "independent_quintic_reposition",
                       "taxonomy_status": "internal_custom_motion_not_customer_approved"}})
    positions, quats = [], []
    segment_index = 0
    for frame in range(count):
        t = frame / 30.0
        while segment_index + 1 < len(segments) and t > segments[segment_index]["t1"]:
            segment_index += 1
        segment = segments[segment_index]
        u = smoothstep5((t - segment["t0"]) / (segment["t1"] - segment["t0"]))
        position = tuple(segment["p0"][axis] * (1.0 - u) + segment["p1"][axis] * u
                         for axis in range(3))
        positions.append(position)
        if lane == 4:
            quats.append(quat_from_yaw_elev_roll(route_yaw, -85.0, 0.0))
        else:
            body_center = (subject[frame][0], ground_y + subject[frame][1] - first[1]
                           + SUBJECT_HEIGHT_M / 2.0, subject[frame][2])
            quats.append(quat_from_yaw_elev_roll(
                *yaw_elev_from_forward(vsub(body_center, position)), 0.0))
    return positions, quats, segments


def _recorded_slope_proxy(views, subject, *, ground_y):
    """Flatten one deterministic least-squares entity-height proxy for ray tests.

    This is not a terrain reconstruction: a single entity path measures neither
    feet height nor transverse slope, and supplies no scene surfaces/occluders.
    The rigid view transform allows the ordinary planar ray evaluator to test
    this particular declared plane without modifying its sampling or thresholds.
    """
    from dataclasses import replace

    first = subject[0]
    dx, dz = subject[-1][0] - first[0], subject[-1][2] - first[2]
    length = math.hypot(dx, dz)
    direction = (dx / length, dz / length)
    progress = [(p[0] - first[0]) * direction[0] + (p[2] - first[2]) * direction[1]
                for p in subject]
    heights = [p[1] - first[1] for p in subject]
    mean_progress = sum(progress) / len(progress)
    mean_height = sum(heights) / len(heights)
    slope = sum((x - mean_progress) * (y - mean_height)
                for x, y in zip(progress, heights)) / sum(
                    (x - mean_progress) ** 2 for x in progress)
    intercept = mean_height - slope * mean_progress
    residuals = [abs(y - slope * x - intercept) for x, y in zip(progress, heights)]
    angle = math.atan(slope)
    sine = math.sin(angle / 2.0)
    rotation = (direction[1] * sine, 0.0, -direction[0] * sine, math.cos(angle / 2.0))

    def multiply(a, b):
        x, y, z, w = a
        bx, by, bz, bw = b
        return (w * bx + x * bw + y * bz - z * by,
                w * by - x * bz + y * bw + z * bx,
                w * bz + x * by - y * bx + z * bw,
                w * bw - x * bx - y * by - z * bz)

    def transform(point):
        relative = (point[0] - first[0], point[1] - ground_y - intercept,
                    point[2] - first[2], 0.0)
        inverse = (-rotation[0], -rotation[1], -rotation[2], rotation[3])
        return multiply(multiply(rotation, relative), inverse)[:3]

    transformed = {lane: [replace(view, p=transform(view.p), q=multiply(rotation, view.q))
                          for view in camera_views] for lane, camera_views in views.items()}
    fit = {
        "method": "least_squares_height_vs_along_route_progress_all_smoothed_observations",
        "n_samples": len(subject), "slope": slope, "intercept_m": intercept,
        "along_route_direction_xz": list(direction), "reference_xz": [first[0], first[2]],
        "ground_y": ground_y, "plane_equation": "y=ground_y+intercept+slope*dot(xz-reference,direction)",
        "height_residual_max_m": max(residuals),
        "height_residual_p95_m": sorted(residuals)[math.ceil(len(residuals) * 0.95) - 1],
        "height_residual_rms_m": math.sqrt(sum(r * r for r in residuals) / len(residuals)),
        "transverse_slope_assumed": 0.0, "feet_offset_assumed_m": 0.0,
        "feet_offset_measured": False, "terrain_measured": False,
        "occluders_measured": False, "selection": "deterministic fit; not selected by overlap score",
        "note": "Entity height trend only; single path cannot establish terrain or visible scene geometry",
    }
    return transformed, fit


def _dependencies() -> None:
    engine_data = Path(__file__).resolve().parents[2]
    for name in ("trajgen", "qc"):
        location = str(engine_data / name)
        if location not in sys.path:
            sys.path.insert(0, location)


def _smooth_anchor(track: list[dict], t: float) -> list[float]:
    """Interpolate sparse authored anchors without velocity/acceleration jumps."""
    if t <= track[0]["t"]:
        return list(track[0]["p"])
    if t >= track[-1]["t"]:
        return list(track[-1]["p"])
    index = bisect_right([item["t"] for item in track], t) - 1
    a, b = track[index:index + 2]
    u = (t - a["t"]) / (b["t"] - a["t"])
    weight = u ** 3 * (10.0 + u * (-15.0 + 6.0 * u))
    return [a["p"][axis] + weight * (b["p"][axis] - a["p"][axis])
            for axis in range(3)]


def _frame_full_body(positions, quats, subject, moves, *, lane: int, ground_y: float):
    """Keep the ground views on the body while retaining their translation windows.

    Camera 3's authored orbit comes almost overhead, then pans at the sky.  Move
    its close/high sections outward using a smooth additive field.  Camera 1
    needs only the small change from head framing to body-center framing.
    """
    from trajgen.mathutil import (quat_from_yaw_elev_roll, vadd, vsub,
                                 yaw_elev_from_forward)
    from trajgen.pink_honesty import enforce

    centers_y = [ground_y + p[1] - subject[0][1] + SUBJECT_HEIGHT_M / 2.0 for p in subject]
    if lane == 3:
        correction = []
        for frame, position in enumerate(positions):
            dx = position[0] - subject[frame][0]
            dz = position[2] - subject[frame][2]
            radius = math.hypot(dx, dz)
            if radius < 1e-6:
                raise ContractError("ground camera crosses directly above the subject")
            target_radius = max(3.5, (position[1] - centers_y[frame]) / math.tan(math.radians(20.0)))
            gain = max(0.0, target_radius - radius)
            correction.append((gain * dx / radius, 0.0, gain * dz / radius))
        # Three box-filter passes yield a smooth cubic correction profile.  Only
        # the correction is filtered; the authored track retains its dynamics.
        n_frames = len(positions)
        for _ in range(3):
            correction = [tuple(
                sum(correction[j][axis] for j in range(max(0, frame - 15),
                                                      min(n_frames, frame + 16)))
                / (min(n_frames, frame + 16) - max(0, frame - 15))
                for axis in range(3)) for frame in range(n_frames)]
        positions = [vadd(position, correction[frame])
                     for frame, position in enumerate(positions)]
    if lane in (1, 3):
        def look_at_body(points):
            return [quat_from_yaw_elev_roll(*yaw_elev_from_forward(vsub(
                (subject[frame][0], centers_y[frame], subject[frame][2]), position)), 0.0)
                for frame, position in enumerate(points)]

        # Look-at changes the yaw basis for named horizontal translations.
        # Reconcile positions against that new basis, then independently QC the
        # final look-at quaternions rather than assuming an invariant.
        for _ in range(2):
            quats = look_at_body(positions)
            positions = enforce(positions, quats, moves, 30.0, ground_y)
        quats = look_at_body(positions)
    return positions, quats


def _framing_report(records, subject, system, *, ground_y):
    """Project an assumed body box, without measured feet, terrain, or occlusion."""
    from itertools import product
    from trajgen.mathutil import quat_forward, quat_right, quat_up, vdot, vsub

    visible = []
    pixel_margins = []
    body_bottoms = []
    for frame, record in enumerate(records):
        q, intr = record["camera_rotation_quaternion"], record["camera_intrinsics"]
        inside = True
        ground_at_frame = ground_y + subject[frame][1] - subject[0][1]
        for dx, y, dz in product((-SUBJECT_HALF_WIDTH_M, SUBJECT_HALF_WIDTH_M),
                                 (ground_at_frame, ground_at_frame + SUBJECT_HEIGHT_M),
                                 (-SUBJECT_HALF_WIDTH_M, SUBJECT_HALF_WIDTH_M)):
            point = (subject[frame][0] + dx, y, subject[frame][2] + dz)
            delta = vsub(point, record["camera_position"])
            depth = vdot(delta, quat_forward(q))
            if depth <= 0.05:
                inside = False
                break
            u = intr["cx"] + intr["fx"] * vdot(delta, quat_right(q)) / depth
            v = intr["cy"] - intr["fy"] * vdot(delta, quat_up(q)) / depth
            pixel_margins.append(min(u, system["width"] - u, v, system["height"] - v))
            body_bottoms.append(v)
            if not (0.0 <= u <= system["width"] and 0.0 <= v <= system["height"]):
                inside = False
                break
        visible.append(inside)
    absent = []
    start = None
    for frame, inside in enumerate(visible + [True]):
        if not inside and start is None:
            start = frame
        if inside and start is not None:
            absent.append({"t0": start / 30.0, "t1": frame / 30.0})
            start = None
    heights = [record["camera_position"][1] - ground_y - subject[frame][1] + subject[0][1]
               for frame, record in enumerate(records)]
    return {"full_body_in_frustum_ratio": sum(visible) / len(visible),
            "absent_intervals": absent,
            "max_absence_s": max((item["t1"] - item["t0"] for item in absent), default=0.0),
            "ground_clearance_min_m": min(heights), "ground_clearance_max_m": max(heights),
            "body_min_edge_margin_px": min(pixel_margins) if pixel_margins else None,
            "body_bottom_max_px": max(body_bottoms) if body_bottoms else None,
            "subject_box_height_m": SUBJECT_HEIGHT_M,
            "subject_box_half_width_m": SUBJECT_HALF_WIDTH_M,
            "measurement_scope": "planning_frustum_projection_only",
            "body_box_base_assumption": (
                "ground_y plus subject vertical displacement; recorded mode treats entity Z "
                "as the proxy base without measuring the offset to feet"),
            "clearance_reference": "assumed proxy base, not measured terrain or feet",
            "feet_offset_measured": False,
            "actual_body_visibility_measured": False,
            "occlusion_tested": False}


def _make_motion_trajgen_clip(source_dir, output_dir, *, distance_m: float = 24.0,
                             speed_mps: float = 0.4, ground_y: float = 0.0,
                             recorded_route: dict | None = None,
                             choreography_scale: float = 1.0,
                             choreography_mode: str = "source_adaptation") -> dict:
    """Write a 60-second MC plan from idealized or recorded subject motion.

    Diagnostic mode uses distance_m/speed_mps for an idealized trajectory; their
    ratio must avoid a planned mid-clip stop.  GTA's native speed parameter does
    not guarantee that physical speed.  Recorded mode uses only the bound
    observed trajectory and preserves its original native command separately.
    A new empty output directory is required; failed QC retains the diagnostic
    report and raises ContractError.
    """
    for value, label in ((distance_m, "distance_m"), (speed_mps, "speed_mps"),
                         (ground_y, "ground_y")):
        if (not isinstance(value, (int, float)) or isinstance(value, bool)
                or not math.isfinite(value)):
            raise ContractError(f"forward plan {label} must be finite")
    if not 0 < distance_m <= 100 or not 0 < speed_mps <= 10:
        raise ContractError("forward plan route exceeds native supported range")
    if (not isinstance(choreography_scale, (float, int)) or isinstance(choreography_scale, bool)
            or not math.isfinite(choreography_scale) or not 1.0 <= choreography_scale <= 2.0):
        raise ContractError("camera choreography scale must be between 1 and 2")
    if choreography_mode not in ("source_adaptation", "independent_windows"):
        raise ContractError("unsupported camera choreography mode")
    if choreography_mode == "independent_windows" and (
            recorded_route is None or choreography_scale != 1.0):
        raise ContractError("independent windows require a recorded route and unscaled camera heights")
    source = Path(source_dir).resolve()
    output = Path(output_dir).resolve()
    if output == source or (output.exists() and any(output.iterdir())):
        raise ContractError("forward plan output must be a new empty directory")
    plan = copy.deepcopy(load_json(source / "plan.json"))
    if (plan.get("mode") != "mc" or plan.get("duration_s") != 60.0
            or plan.get("fps") != 30.0 or len(plan.get("cameras", [])) != 4
            or (plan.get("cam4") or {}).get("mode") != "topdown"):
        raise ContractError("forward plan requires a 60s 30fps four-lane topdown MC clip")
    if recorded_route is None and distance_m < 60.0 * speed_mps:
        raise ContractError("forward route would stop inside the clip; extend its distance")
    anchors = _clip_anchor_track(plan)
    if (len(anchors) < 2 or any(b["t"] <= a["t"] for a, b in zip(anchors, anchors[1:]))
            or anchors[-1]["t"] < 1799 / 30.0):
        raise ContractError("source anchor track must strictly increase and cover the clip")
    n_frames = 1800
    origin = anchors[0]["p"]
    if origin[1] <= ground_y:
        raise ContractError("forward subject aim must be above its ground plane")
    subject = [[origin[0], origin[1], origin[2] + speed_mps * frame / 30.0]
               for frame in range(n_frames)]
    if recorded_route is not None:
        points = recorded_route["waypoints"]
        if len(points) != n_frames:
            raise ContractError("recorded MC planning requires a full 1800-frame recording")
        first = points[0]["position"]
        subject = [[origin[0] + p["position"][0] - first[0],
                    origin[1] + p["position"][2] - first[2],
                    origin[2] + p["position"][1] - first[1]] for p in points]
    _dependencies()
    from trajgen.covis import CamView, evaluate
    from trajgen.mathutil import (clamp, euler_tuple, quat_forward, wrap180,
                                 yaw_elev_from_forward)
    from trajgen.metrics import decouple, physics
    from trajgen.pink_honesty import enforce
    from trajgen.sampler import frame_speeds
    from trajgen.scene import Scene
    from engine_qc.clip_checker import check_clip

    records_by_lane = {}
    systems = {}
    physics_results = {}
    tracks = {}
    framing = {}
    topdown_offset = [0.0, 0.0, 0.0]
    for camera in plan["cameras"]:
        lane = int(camera["id"])
        if lane not in (1, 2, 3, 4) or lane in records_by_lane:
            raise ContractError("forward plan camera IDs must be exactly 1,2,3,4")
        records = copy.deepcopy(load_json(source / f"action_camera_{lane}.json"))
        if len(records) != n_frames:
            raise ContractError(f"forward plan lane {lane} must have 1800 frames")
        positions = []
        for frame, record in enumerate(records):
            anchor = _smooth_anchor(anchors, frame / 30.0)
            position = [subject[frame][axis] + choreography_scale *
                        (record["camera_position"][axis] - anchor[axis]) for axis in range(3)]
            position[1] += (choreography_scale - 1.0) * (origin[1] - ground_y)
            positions.append(tuple(position))
        if choreography_scale > 1.0:
            from .recorded_route import smooth_recorded_positions
            positions = [tuple(p) for p in smooth_recorded_positions(
                [list(p) for p in positions], window_frames=15, passes=3)]
        quats = [record["camera_rotation_quaternion"] for record in records]
        moves = [SimpleNamespace(**segment) for segment in camera["segments"]]
        if choreography_mode == "independent_windows":
            camera["source_segments"] = copy.deepcopy(camera["segments"])
            positions, quats, segments = _independent_recorded_choreography(subject,
                lane=lane, focal_mm=camera["focal_mm"], ground_y=ground_y)
            camera["segments"] = segments
        else:
            positions = enforce(positions, quats, moves, 30.0, ground_y)
            positions, quats = _frame_full_body(positions, quats, subject, moves,
                                                 lane=lane, ground_y=ground_y)
        if lane == 3 and choreography_mode == "source_adaptation":
            for segment in camera["segments"]:
                if segment["cls"].startswith("pan_"):
                    segment["source_choreography"] = copy.deepcopy(segment)
                    segment["cls"] = "observed_subject_tracking"
                    segment["params"] = {"family": "目标追踪", "aim_height_m": SUBJECT_HEIGHT_M / 2.0,
                        "subtype": "continuous_single_subject_variable_offset",
                        "taxonomy_status": "internal_custom_motion_not_customer_approved",
                        "note": "body tracking replaces pan-away; camera-to-subject offset varies"}
        if lane == 4:
            # Recenter the accompanying overview with one constant horizontal
            # offset.  Unlike per-frame tracking, this cannot change its six
            # translations, derivatives, or independence from the other lanes.
            # The unchanged topdown corridor gate still checks the new framing.
            offsets = []
            for frame, (position, quat) in enumerate(zip(positions, quats)):
                forward = quat_forward(quat)
                if forward[1] >= -0.5:
                    raise ContractError("forward plan cam4 is not a downward overview")
                ray_distance = (subject[frame][1] - position[1]) / forward[1]
                offsets.append([subject[frame][axis] - position[axis]
                                - forward[axis] * ray_distance for axis in (0, 2)])
            topdown_offset = [sum(p[0] for p in offsets) / n_frames, 0.0,
                              sum(p[1] for p in offsets) / n_frames]
            positions = [tuple(p[axis] + topdown_offset[axis] for axis in range(3))
                         for p in positions]
            if choreography_mode == "independent_windows":
                for segment in camera["segments"]:
                    for endpoint in ("p0", "p1"):
                        segment[endpoint] = [segment[endpoint][axis] + topdown_offset[axis]
                                             for axis in range(3)]
        # Export first, then differentiate the exported positions.  Recomputed
        # camera/player speed fields describe the actual new track exactly.
        positions = [tuple(round(value, 6) for value in p) for p in positions]
        speeds = frame_speeds(positions, 30.0)
        previous_angles = None
        for frame, record in enumerate(records):
            record["camera_position"] = list(positions[frame])
            record["camera_rotation_quaternion"] = [round(value, 12) for value in quats[frame]]
            record["camera_speed"] = [round(value, 6) for value in speeds[frame]]
            if lane == 1:
                yaw, elev = yaw_elev_from_forward(quat_forward(quats[frame]))
                record["camera_rotation_oula"] = [round(value, 6)
                                                  for value in euler_tuple(yaw, elev, 0.0)]
                dyaw = wrap180(yaw - previous_angles[0]) if previous_angles else 0.0
                delev = elev - previous_angles[1] if previous_angles else 0.0
                record["mouse_dx"] = round(clamp(dyaw / 6.0, -1.0, 1.0), 6)
                record["mouse_dy"] = round(clamp(-delev / 6.0, -1.0, 1.0), 6)
                previous_angles = (yaw, elev)
                record["player_position"] = [round(subject[frame][0], 6),
                                             round(ground_y + subject[frame][1] - origin[1], 6),
                                             round(subject[frame][2], 6)]
                record["player_rotation_oula"] = [0.0, 0.0, 0.0]
                record["player_rotation_quaternion"] = [0.0, 0.0, 0.0, 1.0]
                previous = records[frame - 1]["player_position"] if frame else None
                record["player_speed"] = ([0.0, 0.0, 0.0] if previous is None else
                    [round((record["player_position"][axis] - previous[axis]) * 30.0, 6)
                     for axis in range(3)])
        tracks[lane] = (positions, quats)
        physics_results[str(lane)] = physics(positions, quats, 1.0 / 30.0).to_dict()
        records_by_lane[lane] = records
        systems[lane] = load_json(source / f"systeminfo_{lane}.json")
        framing[str(lane)] = _framing_report(records, subject, systems[lane], ground_y=ground_y)

    times = [frame / 30.0 for frame in range(0, n_frames, 15)]
    views = {lane: [CamView(
        p=tracks[lane][0][frame], q=tracks[lane][1][frame],
        **records_by_lane[lane][frame]["camera_intrinsics"],
        width=systems[lane]["width"], height=systems[lane]["height"])
        for frame in range(0, n_frames, 15)] for lane in (1, 2, 3)}
    scene = Scene("gta-forward-planar-proxy", "open_plaza",
                  (-300.0, ground_y, -300.0), (300.0, ground_y + 100.0, 300.0), ground_y)
    horizontal_covis = evaluate(scene, views, times).to_dict()
    horizontal_covis.update(geometry_source="synthetic_planar_proxy", measured_in_gta=False)
    covis = horizontal_covis
    if choreography_mode == "independent_windows":
        slope_views, plane_fit = _recorded_slope_proxy(views, subject, ground_y=ground_y)
        slope_scene = Scene("gta-recorded-entity-slope-proxy", "open_plaza",
                            (-300.0, 0.0, -300.0), (300.0, 100.0, 300.0), 0.0)
        covis = evaluate(slope_scene, slope_views, times).to_dict()
        covis["geometry_source"] = "synthetic_recorded_path_slope_proxy"
        covis["plane_fit"] = plane_fit
        covis["horizontal_reference"] = {
            "path": "covis-horizontal-proxy.json",
            "covis_ratio": horizontal_covis["covis_ratio"],
            "result": horizontal_covis,
            "note": "Same views against the original initial-height horizontal proxy"}
        scene = slope_scene
    covis["measured_in_gta"] = False
    plan["scene"] = scene.name
    plan["scene_kind"] = scene.kind
    plan["anchors"] = [{"t": frame / 30.0, "p": p} for frame, p in enumerate(subject)]
    plan["subject_motion"] = {
        "control": "native_task_go_straight", "route_kind": "forward_from_anchor",
        "distance_m": float(distance_m), "speed_mps": float(speed_mps),
        "ground_y": float(ground_y), "heading_deg": 0.0,
    }
    if recorded_route is not None:
        plan["subject_motion"] = {
            "route_kind": "recorded_master_telemetry",
            "recorded_route": copy.deepcopy(recorded_route),
            "placement": {"anchor": list(recorded_route["waypoints"][0]["position"]),
                          "heading_deg": 0.0, "ground_y": float(ground_y)},
            "coordinate_mapping": "observed GTA world displacement -> delivery [x,z,y]",
        }
    if choreography_mode == "independent_windows":
        plan["cam4"] = {"mode": "topdown", "guide_cam": 1, "alt_base_m": 20.0,
            "tilt_deg": 5.0, "windows": [
                {"t0": segment["t0"], "t1": segment["t1"], "cls": segment["cls"],
                 "mag_m": segment["params"]["extent_m"]}
                for camera in plan["cameras"] if camera["id"] == 4
                for segment in camera["segments"] if segment["cls"].startswith("translate_")]}
    plan["cam4"]["framing_offset_m"] = topdown_offset
    if choreography_scale > 1.0:
        from trajgen.metrics import cam4_topdown_check
        route_checks = {lane: cam4_topdown_check(tracks[4][0], tracks[4][1],
            [tracks[i][0] for i in (1, 2, 3)], tracks[lane][0], 1.0 / 30.0)
            for lane in (1, 2, 3)}
        plan["cam4"]["guide_cam"] = min(route_checks, key=lambda lane: route_checks[lane].route_p95_m)
    plan["cam4"]["note"] = (
        "Accompanying topdown overview with constant horizontal framing offset; "
        "subject route remains in view while guide corridor is checked by engine_qc")
    plan["covis_windows"] = [{"t": t, "pair_min_overlap": overlap}
                             for t, overlap in covis["pair_min_overlap_ts"]]
    plan["forward_adaptation"] = {
        "schema": "dm-gta-forward-plan/1", "source_dir": str(source),
        "source_plan_sha256": sha256_file(source / "plan.json"),
        "anchor_interpolation": ("piecewise_quintic_C2" if choreography_mode == "source_adaptation"
                                 else "recorded_subject_samples"),
        "translation_correction": ("trajgen.pink_honesty.enforce"
                                   if choreography_mode == "source_adaptation"
                                   else "explicit_quintic_axis_windows"),
        "choreography_mode": choreography_mode,
        "choreography_scale": choreography_scale,
        "topdown_framing_offset_m": topdown_offset,
        "subject_framing": framing,
        "geometry_source": covis["geometry_source"], "measured_in_gta": False,
    }
    if "plane_fit" in covis:
        plan["forward_adaptation"]["planning_plane_fit"] = covis["plane_fit"]
        plan["forward_adaptation"]["horizontal_proxy_diagnostic"] = covis["horizontal_reference"]
    provenance = {
        "generator": "gta_capture.moving_plan", "source": plan["forward_adaptation"],
        "scene": {"name": scene.name, "kind": scene.kind, "synthetic": True,
                  "note": "Planar planning proxy; GTA occlusion/collision unverified"},
        "self_check": {"physics": physics_results,
                       "subject_framing": framing,
                       "decouple": decouple(tracks, 1.0 / 30.0).to_dict(),
                       "covis_ratio": covis["covis_ratio"]},
        "field_conventions": {
            "player_position": "authored idealized forward trajectory; not recorded telemetry",
            "player_speed": "backward finite difference of exported planned player_position",
            "player_rotation_oula": "planning placeholder [0,0,0]; not an observed rotation",
            "player_rotation_quaternion": "planning placeholder identity; not an observed rotation",
            "final_capture_metadata": (
                "materialize_metadata reads player_state from frame events, including rotations; "
                "frame-event recording-side observation limitations remain unchanged"),
        },
        "framing_assumptions": {
            "measurement_scope": "planning_frustum_projection_only",
            "body_box_base": (
                "assumed ground_y plus subject vertical displacement; recorded mode uses entity Z, "
                "which is not proven to equal foot or terrain height"),
            "feet_offset_measured": False,
            "full_body_ratio_interpretation": (
                "1.0 means only the assumed box is inside every planned frustum; "
                "it does not establish actual full-body visibility or ground clearance"),
        },
        "non_trainable": True, "delivery_eligible": False,
    }
    if recorded_route is not None:
        provenance["observation"] = copy.deepcopy(recorded_route["observation"])
        provenance["field_conventions"]["player_position"] = (
            "delivery-coordinate smooth fit of SHA-bound recording-side position estimates; "
            "no in-band replay origin proof")
        provenance["field_conventions"]["player_speed"] = (
            "backward finite difference of the exported fitted player_position; "
            "not an independent observed velocity measurement")
    output.mkdir(parents=True, exist_ok=True)
    for lane, records in records_by_lane.items():
        write_json_atomic(output / f"action_camera_{lane}.json", records)
        write_json_atomic(output / f"systeminfo_{lane}.json", systems[lane])
    if choreography_mode == "independent_windows":
        horizontal_path = write_json_atomic(output / "covis-horizontal-proxy.json", horizontal_covis)
        covis["horizontal_reference"]["sha256"] = sha256_file(horizontal_path)
    write_json_atomic(output / "plan.json", plan)
    write_json_atomic(output / "covis.json", covis)
    write_json_atomic(output / "provenance.json", provenance)
    qc = check_clip(output, "mc", include_video=False)
    report = {"verdict": qc.verdict, "counts": qc.counts,
              "results": [result.to_dict() for result in qc.results],
              "scope": "planning metadata only; GTA geometry and captured video unverified"}
    if recorded_route is not None:
        report["placement"] = plan["subject_motion"]["placement"]
        report["recorded_route_diagnostics"] = recorded_route.get("diagnostics")
        report["observation_semantics"] = "recording_side_estimate"
    report["paths"] = {"plan": str(output / "plan.json"),
                       "qc": str(output / "forward-plan-qc.json"),
                       "provenance": str(output / "provenance.json")}
    write_json_atomic(output / "forward-plan-qc.json", report)
    if qc.verdict != "PASS":
        failures = "; ".join(f"{item.rule}: {item.evidence}" for item in qc.failures())
        raise ContractError(f"moving plan failed engine_qc: {failures}")
    return report


def make_forward_trajgen_clip(source_dir, output_dir, *, distance_m: float = 24.0,
                              speed_mps: float = 0.4, ground_y: float = 0.0) -> dict:
    """Build the diagnostic idealized forward route (not measured ped speed)."""
    return _make_motion_trajgen_clip(source_dir, output_dir, distance_m=distance_m,
                                     speed_mps=speed_mps, ground_y=ground_y)


def make_recorded_trajgen_clip(source_dir, output_dir, *, record_result_path,
                               scenario_path, window_frames: int = 15, passes: int = 3,
                               max_residual_m: float = 0.25, ground_y: float = 0.0,
                               artifact_dir=None, choreography_scale: float = 1.0,
                               choreography_mode: str = "source_adaptation") -> dict:
    """Follow the measured full master recording without equating task speed to m/s."""
    from .recorded_route import build_recorded_subject_route

    source_plan = load_json(Path(source_dir) / "plan.json")
    aim_height = _clip_anchor_track(source_plan)[0]["p"][1] - ground_y
    route = build_recorded_subject_route(record_result_path, scenario_path,
        window_frames=window_frames, passes=passes, max_residual_m=max_residual_m,
        look_at_height_m=aim_height, artifact_dir=artifact_dir)
    return _make_motion_trajgen_clip(source_dir, output_dir, ground_y=ground_y,
                                     recorded_route=route, choreography_scale=choreography_scale,
                                     choreography_mode=choreography_mode)


def main() -> int:
    import argparse
    import json

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source_dir")
    parser.add_argument("output_dir")
    parser.add_argument("--distance-m", type=float, default=24.0)
    parser.add_argument("--speed-mps", type=float, default=0.4)
    parser.add_argument("--ground-y", type=float, default=0.0)
    parser.add_argument("--record-run-dir", type=Path)
    parser.add_argument("--scenario", type=Path)
    parser.add_argument("--choreography-scale", type=float, default=1.0)
    parser.add_argument("--choreography-mode", choices=("source_adaptation", "independent_windows"),
                        default="source_adaptation")
    args = parser.parse_args()
    if args.record_run_dir:
        if args.scenario is None:
            parser.error("--record-run-dir requires the frozen --scenario")
        report = make_recorded_trajgen_clip(args.source_dir, args.output_dir,
            record_result_path=args.record_run_dir / "evidence" / "native-record-result.json",
            scenario_path=args.scenario, ground_y=args.ground_y,
            choreography_scale=args.choreography_scale, choreography_mode=args.choreography_mode)
    else:
        report = make_forward_trajgen_clip(args.source_dir, args.output_dir,
            distance_m=args.distance_m, speed_mps=args.speed_mps, ground_y=args.ground_y)
    print(json.dumps({key: report[key] for key in ("verdict", "counts", "paths", "placement")
                      if key in report}))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

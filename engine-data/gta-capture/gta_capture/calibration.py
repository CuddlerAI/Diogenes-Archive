"""Solve GTA's projection from measured correspondences and emit gate evidence.

``preflight`` already refuses a real capture on three calibration checks —
``calibration.binding``, ``calibration.projection_evidence`` and
``calibration.projection_contract`` — and the PR #405 gate repeats the last one.
All three want the same artifact: an approved ``dm-gta-projection-calibration/1``
document whose reprojection error, sample count, filmback and metric scale match
the dependency manifest.  Until now nothing in the repository could *produce* one,
so the only ways past those gates were to hand-write JSON or copy the example
numbers, both of which docs/158 §11 P0 forbids.

This module closes that hole from the measurement side.  The operator captures
correspondences on the real machine — a surveyed world point, the pixel it lands
on, and the final camera pose for that frame — and :func:`solve_projection`
recovers the focal lengths by least squares, converts them to a filmback per
declared focal length, and reports the RMS reprojection error that the gate
threshold is written against.

What it deliberately does not do: invent ``metric_scale``.  Scale is a human
approval recorded in the manifest (``metric_scale_approved``), and this module
only carries the approved value into the evidence and rejects a mismatch.  A
solver that also guessed the scale would let one unverified constant validate
itself.
"""

from __future__ import annotations

import math
from pathlib import Path
from typing import Any, Sequence

from .errors import ContractError
from .jsonio import load_json


PROJECTION_SCHEMA = "dm-gta-projection-calibration/1"
SAMPLES_SCHEMA = "dm-gta-calibration-samples/1"

#: Minimum correspondences per declared focal length.  Two points can fit one
#: focal length exactly and prove nothing; the residual only becomes meaningful
#: once the fit is over-determined across depths and image positions.
MIN_SAMPLES_PER_FOCAL = 8
#: Evidence for the MC capture contract must cover the whole four-camera set.
#: The generic solver still supports any two or more distinct focal groups.
REQUIRED_FOCALS = (18.0, 24.0, 35.0, 50.0)
#: Spread required across the frame, as a fraction of width/height.  A cluster in
#: the middle of the image fits almost any focal length within a pixel.
MIN_PIXEL_SPREAD_FRAC = 0.35
#: Points nearer than this (GTA units along the optical axis) are rejected: near
#: the camera a small pose error dominates the projection residual.
MIN_DEPTH_UNITS = 2.0
#: Agreement required between the filmback recovered at each declared focal
#: length (mm).  Disagreement means the focal->FOV conversion is wrong, which no
#: single-focal reprojection error would reveal.
FILMBACK_AGREEMENT_MM = 0.5
#: Distinct focal lengths the measurement must span.  The cross-focal agreement
#: above is the only check that can expose a wrong focal->FOV constant; with one
#: group it has nothing to compare, the spread is trivially 0.0, and the evidence
#: would read as "perfect agreement".  Two is the minimum that makes the check
#: real; ``build_projection_evidence`` separately requires the complete MC set.
MIN_FOCAL_GROUPS = 2


def _finite(value: Any) -> bool:
    return isinstance(value, (int, float)) and not isinstance(value, bool) \
        and math.isfinite(float(value))


def _finite_vec(value: Any, n: int) -> bool:
    return isinstance(value, (list, tuple)) and len(value) == n \
        and all(_finite(item) for item in value)


def camera_basis(quaternion: Sequence[float]) -> tuple[tuple[float, float, float], ...]:
    """Return ``(right, forward, up)`` for a GTA order-2 camera quaternion.

    These are the columns of the rotation matrix, in the order GTA stores its
    camera basis.  ``tape._euler_zxy_deg`` documents three of these components as
    its inverse reads (``forward.z``, ``right.z``, ``up.z``); this is the same
    matrix written out in full, so the two cannot drift apart silently.
    """
    if not _finite_vec(quaternion, 4):
        raise ContractError("camera quaternion must be a finite vec4")
    x, y, z, w = (float(component) for component in quaternion)
    norm = math.sqrt(x * x + y * y + z * z + w * w)
    if not math.isfinite(norm) or norm <= 0.0:
        raise ContractError("camera quaternion must be non-degenerate")
    x, y, z, w = (component / norm for component in (x, y, z, w))
    right = (1 - 2 * (y * y + z * z), 2 * (x * y + w * z), 2 * (x * z - w * y))
    forward = (2 * (x * y - w * z), 1 - 2 * (x * x + z * z), 2 * (y * z + w * x))
    up = (2 * (x * z + w * y), 2 * (y * z - w * x), 1 - 2 * (x * x + y * y))
    return right, forward, up


def project(world: Sequence[float], *, position: Sequence[float],
            quaternion: Sequence[float], fx: float, fy: float,
            cx: float, cy: float) -> tuple[float, float, float]:
    """Project a world point to pixels; returns ``(u, v, depth)``.

    Screen ``v`` grows downward, so the vertical term is subtracted.  ``depth`` is
    the distance along the optical axis and is returned so callers can reject
    points behind or too close to the camera rather than silently folding them
    into a least-squares fit.
    """
    if not _finite_vec(world, 3) or not _finite_vec(position, 3):
        raise ContractError("projection needs finite world and camera positions")
    right, forward, up = camera_basis(quaternion)
    rel = [float(world[axis]) - float(position[axis]) for axis in range(3)]
    x_cam = sum(rel[axis] * right[axis] for axis in range(3))
    y_cam = sum(rel[axis] * up[axis] for axis in range(3))
    z_cam = sum(rel[axis] * forward[axis] for axis in range(3))
    if z_cam <= 0.0:
        raise ContractError("world point is behind the camera")
    return cx + fx * x_cam / z_cam, cy - fy * y_cam / z_cam, z_cam


def _normalized_ray(sample: dict) -> tuple[float, float, float]:
    """Return ``(x/z, y/z, z)`` in camera space for one correspondence."""
    camera = sample.get("camera")
    if not isinstance(camera, dict):
        raise ContractError("calibration sample has no camera pose")
    right, forward, up = camera_basis(camera.get("quaternion"))
    position = camera.get("position")
    world = sample.get("world")
    if not _finite_vec(position, 3) or not _finite_vec(world, 3):
        raise ContractError("calibration sample world/camera position is malformed")
    rel = [float(world[axis]) - float(position[axis]) for axis in range(3)]
    x_cam = sum(rel[axis] * right[axis] for axis in range(3))
    y_cam = sum(rel[axis] * up[axis] for axis in range(3))
    z_cam = sum(rel[axis] * forward[axis] for axis in range(3))
    if z_cam < MIN_DEPTH_UNITS:
        raise ContractError(
            f"calibration sample is {z_cam:.2f} units deep; the minimum is "
            f"{MIN_DEPTH_UNITS} because pose error dominates closer in")
    return x_cam / z_cam, y_cam / z_cam, z_cam


def _least_squares_focal(pairs: Sequence[tuple[float, float]]) -> float:
    """Fit ``offset = f * ray`` through the origin, which is what a pinhole is."""
    denominator = sum(ray * ray for ray, _ in pairs)
    if denominator <= 0.0:
        raise ContractError("calibration samples are degenerate for a focal-length fit")
    return sum(ray * offset for ray, offset in pairs) / denominator


def solve_projection(samples: Sequence[dict], *, width: int, height: int) -> dict:
    """Recover focal lengths and the implied filmback from correspondences.

    Samples are grouped by their declared ``focal_mm``: each group yields its own
    ``fx``/``fy`` fit, and the filmback each group implies must agree with the
    others.  That cross-focal agreement is the part a single reprojection number
    cannot express — a wrong filmback constant reprojects perfectly at the focal
    length it was tuned on and drifts at every other one.
    """
    if not isinstance(width, int) or not isinstance(height, int) \
            or width <= 0 or height <= 0:
        raise ContractError("calibration render resolution must be positive integers")
    cx, cy = width / 2.0, height / 2.0

    groups: dict[float, list[dict]] = {}
    for index, sample in enumerate(samples):
        if not isinstance(sample, dict):
            raise ContractError(f"calibration sample {index} is not an object")
        focal = sample.get("focal_mm")
        if not _finite(focal) or float(focal) <= 0:
            raise ContractError(f"calibration sample {index} has no positive focal_mm")
        if not _finite_vec(sample.get("pixel"), 2):
            raise ContractError(f"calibration sample {index} has no finite pixel")
        u, v = (float(value) for value in sample["pixel"])
        if not (0.0 <= u <= width and 0.0 <= v <= height):
            raise ContractError(
                f"calibration sample {index} pixel ({u:.1f}, {v:.1f}) is outside the frame")
        groups.setdefault(float(focal), []).append(sample)

    if len(groups) < MIN_FOCAL_GROUPS:
        raise ContractError(
            f"calibration spans {len(groups)} focal length(s); at least "
            f"{MIN_FOCAL_GROUPS} are required because cross-focal filmback agreement "
            "is the only check that exposes a wrong focal->FOV conversion")

    per_focal = []
    residuals: list[float] = []
    for focal in sorted(groups):
        rows = groups[focal]
        if len(rows) < MIN_SAMPLES_PER_FOCAL:
            raise ContractError(
                f"focal {focal:g} mm has {len(rows)} samples; at least "
                f"{MIN_SAMPLES_PER_FOCAL} are required")
        rays = [_normalized_ray(row) for row in rows]
        pixels = [(float(row["pixel"][0]), float(row["pixel"][1])) for row in rows]
        spread_u = (max(u for u, _ in pixels) - min(u for u, _ in pixels)) / width
        spread_v = (max(v for _, v in pixels) - min(v for _, v in pixels)) / height
        if spread_u < MIN_PIXEL_SPREAD_FRAC or spread_v < MIN_PIXEL_SPREAD_FRAC:
            raise ContractError(
                f"focal {focal:g} mm covers {spread_u:.0%}x{spread_v:.0%} of the frame; "
                f"at least {MIN_PIXEL_SPREAD_FRAC:.0%} in each axis is required")
        fx = _least_squares_focal([(ray[0], u - cx) for ray, (u, _) in zip(rays, pixels)])
        fy = _least_squares_focal([(ray[1], cy - v) for ray, (_, v) in zip(rays, pixels)])
        if fx <= 0 or fy <= 0:
            raise ContractError(f"focal {focal:g} mm fit produced a non-positive focal length")
        group_residuals = []
        for ray, (u, v) in zip(rays, pixels):
            group_residuals.append(math.hypot(cx + fx * ray[0] - u, cy - fy * ray[1] - v))
        residuals.extend(group_residuals)
        per_focal.append({
            "focal_mm": focal,
            "sample_count": len(rows),
            "fx": fx, "fy": fy,
            "filmback_mm": width * focal / fx,
            "aspect_ratio": fx / fy,
            "rms_px": math.sqrt(sum(value * value for value in group_residuals)
                                / len(group_residuals)),
            "max_px": max(group_residuals),
            "pixel_spread": {"u": spread_u, "v": spread_v},
            "depth_units": {"min": min(ray[2] for ray in rays),
                            "max": max(ray[2] for ray in rays)},
        })

    filmbacks = [item["filmback_mm"] for item in per_focal]
    spread_mm = max(filmbacks) - min(filmbacks)
    if spread_mm > FILMBACK_AGREEMENT_MM:
        raise ContractError(
            f"recovered filmback spans {spread_mm:.3f} mm across focal lengths "
            f"(limit {FILMBACK_AGREEMENT_MM} mm); the focal->FOV conversion is wrong")

    rms = math.sqrt(sum(value * value for value in residuals) / len(residuals))
    return {
        "width": width, "height": height, "cx": cx, "cy": cy,
        "per_focal": per_focal,
        "filmback_mm": sum(filmbacks) / len(filmbacks),
        "filmback_spread_mm": spread_mm,
        "reprojection": {"metric": "rms_euclidean_px", "error_px": rms,
                         "sample_count": len(residuals),
                         "max_px": max(residuals)},
    }


def load_samples(path) -> dict:
    """Load and shape-check a measurement file produced on the capture machine."""
    document = load_json(Path(path))
    if not isinstance(document, dict) or document.get("schema") != SAMPLES_SCHEMA:
        raise ContractError(f"calibration samples schema must equal {SAMPLES_SCHEMA!r}")
    samples = document.get("samples")
    if not isinstance(samples, list) or not samples:
        raise ContractError("calibration samples file contains no samples")
    for key in ("game_build", "graphics_api", "captured_at", "method"):
        if not isinstance(document.get(key), str) or not document[key].strip():
            raise ContractError(f"calibration samples file is missing {key}")
    render = document.get("render")
    if (not isinstance(render, dict) or not isinstance(render.get("width"), int)
            or not isinstance(render.get("height"), int)):
        raise ContractError("calibration samples file has no integer render width/height")
    return document


def build_projection_evidence(document: dict, *, calibration_id: str, filmback_id: str,
                              metric_scale: float, approval_status: str = "draft"
                              ) -> dict:
    """Turn a solved measurement set into the document the gates re-open.

    ``metric_scale`` is passed through from the approved manifest rather than
    fitted; see the module docstring.  The result still has to survive
    ``preflight``, which re-reads this file by hash and re-checks every field
    against the manifest — building it here is a convenience, not an approval.
    That is also why ``approval_status`` defaults to ``draft``: a freshly solved
    measurement nobody has looked at must not stamp itself approved because a flag
    was forgotten.  ``approved`` is always an explicit act by the reviewer.
    """
    if approval_status not in ("draft", "approved"):
        raise ContractError("approval_status must be draft or approved")
    if not _finite(metric_scale) or float(metric_scale) <= 0:
        raise ContractError("metric_scale must be a finite positive number")
    for label, value in (("calibration_id", calibration_id), ("filmback_id", filmback_id)):
        if not isinstance(value, str) or not value.strip():
            raise ContractError(f"{label} must be a non-empty string")
    render = document["render"]
    solution = solve_projection(document["samples"],
                                width=int(render["width"]), height=int(render["height"]))
    focals = {item["focal_mm"] for item in solution["per_focal"]}
    if focals != set(REQUIRED_FOCALS):
        raise ContractError(
            "projection evidence requires all four focal groups 18/24/35/50 mm; "
            f"got {sorted(focals)}")
    return {
        "schema": PROJECTION_SCHEMA,
        "approval_status": approval_status,
        "calibration_id": calibration_id,
        "filmback_id": filmback_id,
        "metric_scale": float(metric_scale),
        "metric_scale_source": "approved dependency manifest; not fitted here",
        "game_build": document["game_build"],
        "render": {"width": int(render["width"]), "height": int(render["height"]),
                   "graphics_api": document["graphics_api"]},
        "reprojection": solution["reprojection"],
        "solution": {key: solution[key] for key in
                     ("cx", "cy", "per_focal", "filmback_mm", "filmback_spread_mm")},
        "measurement": {"method": document["method"], "captured_at": document["captured_at"],
                        "operator_note": document.get("operator_note", "")},
    }

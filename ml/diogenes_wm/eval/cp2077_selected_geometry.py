"""Read-only geometry checks for native CP2077 selected-Present diagnostics.

This is a separately versioned local evidence report, not a PR405 package or production binding.
All original frames and identities are retained; only geometry pair evaluation is sampled.
"""
from __future__ import annotations

import argparse
from dataclasses import asdict
import json
import math
from pathlib import Path

import numpy as np

from . import cp2077_egeo_gate as legacy
from . import cp2077_selected_inputs as inputs
from . import geo_probes
from .cp2077_selected_inputs import require

SCHEMA = "dm-cp2077-selected-geometry/v1"
EVEN_CFR_SCHEMA = "dm-cp2077-selected-geometry/v2"
STRIDE = 4
PAIR_INTERVAL_SECONDS = 0.4


def relative_motion(left, right):
    require(("bound_position" in left) == ("bound_position" in right), "mixed bound/unbound poses")
    if "bound_position" in left:
        from .cp2077_bound_geometry import relative_motion as bound_motion
        return bound_motion(left, right)
    return np.linalg.inv(left["view"]) @ right["view"]


def project(u, v, depth, projection_left, projection_right, transform, width, height, scale=1.0):
    """Original row-vector D3D matrices, including each frame's observed projection jitter."""
    clip = np.stack([2*(u+.5)/width-1, 1-2*(v+.5)/height, depth, np.ones_like(depth)], axis=-1)
    camera = clip @ np.linalg.inv(projection_left)
    with np.errstate(divide="ignore", invalid="ignore"):
        camera /= camera[..., 3:4]
        camera[..., :3] *= scale
        target = camera @ transform
        out = target @ projection_right
        x = (out[..., 0]/out[..., 3]+1)*width/2-.5
        y = (1-out[..., 1]/out[..., 3])*height/2-.5
    valid = np.isfinite(depth) & (depth > 0) & np.isfinite(x) & np.isfinite(y) & (out[..., 3] > 0)
    return x, y, valid


def measure_pair(gray_left, gray_right, raw_depth, left, right):
    h, w = raw_depth.shape
    y, x = np.mgrid[16:h-16:STRIDE, 16:w-16:STRIDE].astype(float)
    d = raw_depth[y.astype(int), x.astype(int)]
    reference = gray_left[y.astype(int), x.astype(int)].astype(float)
    baseline = np.abs(gray_right[y.astype(int), x.astype(int)].astype(float)-reference)
    transform = relative_motion(left, right)
    variants = {"observed": (transform, 1.), "inverse": (np.linalg.inv(transform), 1.),
                "no_pose": (np.eye(4), 1.), "half_depth": (transform, .5), "double_depth": (transform, 2.)}
    common = ~((x < 256) & (y < 32))
    core = common.copy()
    observed_support = None
    errors = {}
    for name, (motion, scale) in variants.items():
        u, v, good = project(x, y, d, left["projection"], right["projection"], motion, w, h, scale)
        sampled, in_bounds = geo_probes._bilinear(gray_right.astype(float), u, v)
        support = good & in_bounds & (u >= 16) & (u < w-16) & (v >= 16) & (v < h-16) & core
        common &= support
        if name == "observed":
            observed_support = support
        errors[name] = np.abs(sampled-reference)
    require(common.any(), "pair has no common valid native pixels")
    base = float(np.mean(baseline[common]))
    metrics = {}
    for name, error in errors.items():
        photo = float(np.mean(error[common]))
        metrics[name] = {"photo_l1": photo, "improvement": base/photo if photo > 0 else None}
    # The existing validity gate measures the OBSERVED camera projection, not the intersection
    # with deliberately incorrect controls. Keep that intersection for comparable photo errors.
    # Normalize to the full native sampling grid; fixed border/watermark exclusions count out.
    full_grid_pixels = len(range(0, h, STRIDE)) * len(range(0, w, STRIDE))
    return {"baseline_l1": base, "valid_fraction": float(observed_support.sum()/full_grid_pixels),
            "common_valid_fraction": float(common.mean()), "variants": metrics}


def decode_watermark(gray: np.ndarray) -> int:
    # Same 24 x 8px blocks and CRC8 as present_identity::watermark, at native origin (0,0).
    means = [float(gray[:8, i*8:(i+1)*8].mean()) for i in range(24)]
    require(all(v <= 96 or v >= 160 for v in means), "ambiguous video watermark")
    bits = [int(v >= 160) for v in means]
    checksum = sum(b << (7-i) for i, b in enumerate(bits[:8]))
    seq = sum(b << (15-i) for i, b in enumerate(bits[8:]))
    crc = 0
    for byte in seq.to_bytes(2, "big"):
        crc ^= byte
        for _ in range(8):
            crc = ((crc << 1) ^ 7) & 255 if crc & 128 else (crc << 1) & 255
    require(crc ^ 170 == checksum, "video watermark CRC mismatch")
    return seq


def video_frames(video: Path, frames: list[dict], wanted: set[int]):
    import cv2
    capture = cv2.VideoCapture(str(video))
    require(capture.isOpened(), "cannot decode original video")
    require(abs(capture.get(cv2.CAP_PROP_FPS)-30) < 1e-6, "native video is not 30 CFR")
    selected = {}
    count = 0
    try:
        while True:
            ok, image = capture.read()
            if not ok:
                break
            require(count < len(frames) and image.shape == (1080, 1920, 3), "video count/geometry mismatch")
            gray = cv2.cvtColor(image, cv2.COLOR_BGR2GRAY)
            require(decode_watermark(gray) == frames[count]["present_seq"] & 65535,
                    f"video/recorder identity mismatch at RGB {count}")
            if count in wanted:
                selected[count] = gray
            count += 1
    finally:
        capture.release()
    require(count == len(frames) and set(selected) == wanted, "video is truncated or selected frames absent")
    return selected


def pair_populations(data: list[dict], scale: float, thresholds: legacy.Thresholds):
    populations = {"static": [], "translation": [], "rotation": []}
    for session_index, run in enumerate(data):
        rows = run["samples"]
        j = 1
        for i, left in enumerate(rows):
            j = max(j, i+1)
            while j < len(rows) and (rows[j]["present_qpc"]-left["present_qpc"])/run["frequency"] < PAIR_INTERVAL_SECONDS:
                j += 1
            if j == len(rows):
                break
            right = rows[j]
            delta = (right["present_qpc"]-left["present_qpc"])/run["frequency"]
            if delta > .6:
                continue  # Explicit gap policy; no neighbouring substitution or interpolation.
            motion = relative_motion(left, right)
            translation = float(np.linalg.norm(motion[3, :3])) * scale
            rotation = math.acos(float(np.clip((np.trace(motion[:3, :3])-1)/2, -1, 1)))
            pair = (session_index, i, j)
            if translation <= thresholds.max_static_translation_metres and rotation <= thresholds.max_static_rotation_radians:
                populations["static"].append(pair)
            if translation >= thresholds.min_translation_metres:
                populations["translation"].append(pair)
            if rotation >= thresholds.min_rotation_radians:
                populations["rotation"].append(pair)
    return populations


def evaluate(records: list[dict], sampled: dict, thresholds: legacy.Thresholds):
    failures = []
    for kind, minimum in (("static", thresholds.min_static_pairs), ("translation", thresholds.min_translation_pairs),
                          ("rotation", thresholds.min_rotation_pairs)):
        if len(sampled[kind]) < minimum:
            failures.append(f"{kind}_pairs={len(sampled[kind])} < {minimum}")
    moving, static = [], []
    skipped = invalid = 0
    for record in records:
        if "static" in record["populations"]:
            static.append(record["baseline_l1"])
        if not set(record["populations"]) & {"translation", "rotation"}:
            continue
        if record["baseline_l1"] < thresholds.min_motion_baseline_l1:
            skipped += 1
            continue
        vals = [record["variants"][k]["improvement"] for k in ("observed", "inverse")]
        if any(v is None or not math.isfinite(v) for v in vals):
            invalid += 1
            continue
        moving.append(record)
    if invalid:
        failures.append(f"invalid_metric_pairs={invalid} != 0")
    if len(moving) < thresholds.min_usable_motion_pairs:
        failures.append(f"usable_motion_pairs={len(moving)} < {thresholds.min_usable_motion_pairs}")
    metrics = {}
    if moving:
        improvements = [r["variants"]["observed"]["improvement"] for r in moving]
        metrics = {"improvement_p50": float(np.median(improvements)),
                   "inverse_improvement_p50": float(np.median([r["variants"]["inverse"]["improvement"] for r in moving])),
                   "valid_frac_p10": float(np.percentile([r["valid_fraction"] for r in moving], 10)),
                   "improved_share": float(np.mean(np.array(improvements) > 1)),
                   "moving_baseline_l1_p50": float(np.median([r["baseline_l1"] for r in moving])),
                   "static_baseline_l1_p50": float(np.median(static)) if static else None}
        for key, limit in (("improvement_p50", thresholds.min_improvement_p50),
                           ("valid_frac_p10", thresholds.min_valid_frac_p10),
                           ("improved_share", thresholds.min_improved_share)):
            if metrics[key] < limit:
                failures.append(f"{key}={metrics[key]} < {limit}")
        if metrics["improvement_p50"] < metrics["inverse_improvement_p50"] * thresholds.min_inverse_separation:
            failures.append("observed camera does not separate from inverse control")
        if static and metrics["static_baseline_l1_p50"] >= metrics["moving_baseline_l1_p50"]:
            failures.append("static RGB motion is not below moving controls")
    return {"geometry_checks_passed": not failures, "failures": failures, "metrics": metrics,
            "usable_motion_pairs": len(moving), "skipped_low_motion": skipped, "invalid_metric_pairs": invalid}


def sample_populations(populations, max_pairs, independent_sessions=None):
    # A bound report evaluates each launch on its own. A pooled sampler could starve a
    # smaller launch even when its original evidence contains enough control pairs.
    if independent_sessions is None:
        return {kind: legacy._deterministic_sample(pairs, max_pairs//3)
                for kind, pairs in populations.items()}
    return {kind: [pair for index in range(independent_sessions)
                   for pair in legacy._deterministic_sample([p for p in pairs if p[0] == index], max_pairs//3)]
            for kind, pairs in populations.items()}


def run(manifest_path: Path, max_pairs: int = 120):
    from . import cp2077_bound_geometry as bound
    require(90 <= max_pairs <= 240, "max-pairs must be within [90,240]")
    manifest = inputs.read_json(manifest_path)
    manifest_sha = legacy._sha256(manifest_path)
    module = Path(__file__).resolve()
    code_paths = (module, Path(inputs.__file__), Path(geo_probes.__file__), Path(legacy.__file__), Path(bound.__file__))
    code_sha = {p.name: legacy._sha256(p) for p in code_paths}
    require(manifest["schema"] in ("dm-cp2077-selected-geometry-input/v1", bound.INPUT_SCHEMA) and manifest["non_trainable"] is True,
            "unsupported geometry input")
    context = bound.Context(manifest) if manifest["schema"] == bound.INPUT_SCHEMA else None
    require(1 <= len(manifest["inventories"]) <= 3, "expected one to three independent sessions")
    sessions = [inputs.Session(Path(path)) for path in manifest["inventories"]]
    require(len({s.root for s in sessions}) == len(sessions), "duplicate session")
    reference, metric = inputs.load_metric(Path(manifest["metric_reference"]))
    before = [s.verify_all() for s in sessions]
    data = [inputs.load_session(s, reference) for s in sessions]
    if context is not None:
        for session, run_data in zip(sessions, data):
            context.attach(session, run_data, metric)
    thresholds = legacy.Thresholds()
    populations = pair_populations(data, metric["metres_per_world_unit"], thresholds)
    sampled = sample_populations(populations, max_pairs, len(data) if context is not None else None)
    pairs = sorted({tuple(p) for values in sampled.values() for p in values})
    records = []
    for session_index, (session, run_data) in enumerate(zip(sessions, data)):
        chosen = [(i, j) for s, i, j in pairs if s == session_index]
        wanted = {run_data["samples"][k]["session_frame"] for pair in chosen for k in pair}
        rgb = video_frames(run_data["video"], run_data["frames"], wanted)
        for i, j in chosen:
            left, right = (run_data["samples"][k] for k in (i, j))
            path = session.verify("raw-depth/" + left["file"])
            raw = np.fromfile(path, dtype="<f4").reshape(1080, 1920)
            require(np.isfinite(raw).all() and (raw >= 0).all() and (raw <= 1).all() and (raw > 0).any(),
                    "raw depth contains invalid pixels")
            measured = measure_pair(rgb[left["session_frame"]], rgb[right["session_frame"]], raw, left, right)
            records.append(dict(measured, session_index=session_index,
                left_present=left["present_seq"], right_present=right["present_seq"],
                left_qpc=left["present_qpc"], right_qpc=right["present_qpc"],
                left_camera_observation_qpc=left["camera_observation_qpc"],
                right_camera_observation_qpc=right["camera_observation_qpc"],
                left_frame=left["session_frame"], right_frame=right["session_frame"],
                left_depth_sha256=session.files["raw-depth/"+left["file"]]["sha256"],
                populations=[kind for kind, values in sampled.items() if (session_index, i, j) in values]))
        del rgb
    after = [s.verify_all() for s in sessions]
    require(before == after, "original sessions changed during geometry evaluation")
    reference_after, metric_after = inputs.load_metric(Path(manifest["metric_reference"]))
    require(reference == reference_after and metric == metric_after, "metric inputs changed during evaluation")
    require(manifest_sha == legacy._sha256(manifest_path)
            and code_sha == {p.name: legacy._sha256(p) for p in code_paths}, "validation input/code changed during evaluation")
    schema = EVEN_CFR_SCHEMA if any(r["depth_stride"] == 2 for r in data) else SCHEMA
    result = dict(evaluate(records, sampled, thresholds), schema=schema, non_trainable=True,
        acceptance=False, production_binding_authorized=False, upload_allowed=False,
        manifest_sha256=manifest_sha, metric=metric, thresholds=asdict(thresholds),
        evidence_before=before, evidence_after=after, all_original_sha_unchanged=True,
        code_sha256=code_sha,
        sessions=[{"rgb_frames": len(r["frames"]), "distinct_depth": len(r["samples"]),
                   "exact_rgb_pairs": r["required_rgb_frames"], "video_watermarks_verified": len(r["frames"]),
                   **({"depth_selection": "even_cfr_30fps/v1", "required_rgb_frames": r["required_rgb_frames"]}
                      if r["depth_stride"] == 2 else {}),
                   "camera_observation_after_entry_qpc_ticks": {
                       "min": min(s["camera_observation_qpc"]-s["present_qpc"] for s in r["samples"]),
                       "max": max(s["camera_observation_qpc"]-s["present_qpc"] for s in r["samples"])}}
                  for r in data],
        eligible_pairs={kind: len(p) for kind, p in populations.items()},
        sampled_pairs={kind: len(p) for kind, p in sampled.items()}, pairs=records,
        policy={"native_size": [1920, 1080], "metric_pixel_stride": STRIDE,
                "pair_interval_seconds": PAIR_INTERVAL_SECONDS, "maximum_pair_interval_seconds": .6,
                "coordinates": ("native full-binding camera-to-world positions/quaternions; converted to original projection basis; original jitter; no fitted scale"
                                if context is not None else "original row-vector D3D V/P; both projections retain their jitter; no fitted scale"),
                "sampling": ("native decoded pose motion; deterministic per-launch per-population sample; max-pairs applies per launch; no cross-session pairs"
                             if context is not None else "actual matrix motion; deterministic uniform sample per population; no cross-session pairs"),
                "validity_gate": "observed camera valid support divided by the full native sampling grid; unchanged 0.50 threshold",
                "common_valid_fraction": "all controls intersected, relative to the fixed interior grid; supplementary, not observed coverage",
                "mask": "fixed 16px border and watermark exclusion; common support across all controls"},
        limitations=["Local geometry checks do not authorize a production camera binding or PR405 package.",
                     "Independent HUD/player calibration supplies metric units; rotation alone cannot determine scale.",
                     "These two scenes do not establish whole-game accuracy or a controlled performance baseline."])
    return context.finish(result, evaluate, thresholds) if context is not None else result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--input", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--max-pairs", type=int, default=120)
    args = parser.parse_args()
    manifest = inputs.read_json(args.input)
    require(args.out.is_absolute() and args.out.parent.is_dir() and not args.out.exists(), "output must be a new absolute file")
    for inventory in manifest["inventories"]:
        require(not args.out.resolve().is_relative_to(inputs.Session(Path(inventory)).root), "output cannot modify an original session")
    reference = inputs.read_json(Path(manifest["metric_reference"]))
    require(not args.out.resolve().is_relative_to(Path(reference["calibration_session"]).resolve()),
            "output cannot modify the original metric calibration session")
    try:
        report = run(args.input, args.max_pairs)
    except Exception as error:
        report = {"schema": SCHEMA, "non_trainable": True, "acceptance": False,
                  "production_binding_authorized": False, "upload_allowed": False,
                  "geometry_checks_passed": False, "failures": [str(error)],
                  "metrics": {}, "sessions": [], "sampled_pairs": {}}
    # Existing create-new durable report publisher. The output is outside every original session.
    legacy._atomic_write(args.out, report)
    assessment = report.get("geometry", report)
    print(json.dumps(dict(schema=report["schema"], sessions=report["sessions"], **{k: assessment[k] for k in
        ("geometry_checks_passed", "failures", "metrics", "sampled_pairs")})))
    return 0 if assessment["geometry_checks_passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())

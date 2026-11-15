"""Offline request design and explicit real Story sampling; never approves calibration.

`generate` and `verify` never open the game pipe. Only `capture` does.
Pixel detection is deliberately external: predicted and native UV are not measurements.
"""
from __future__ import annotations

import argparse
from datetime import datetime, timezone
import json
import math
from pathlib import Path
import re
import struct
import sys
import traceback
import uuid

MODULE_ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(MODULE_ROOT))
from gta_capture.calibration import camera_basis, project
from gta_capture.errors import ContractError
from gta_capture.jsonio import file_record, load_json, sha256_file, utc_now, write_json_atomic
from gta_capture.protocol import DEFAULT_PIPE
from gta_capture.story_projection import OBSERVATION_SCHEMA, REQUEST_SCHEMA, capture_sample, validate_request
from gta_capture.tape import _quaternion_zxy

MANIFEST_SCHEMA = "dm-gta-story-projection-batch-plan/1"
RECEIPT_SCHEMA = "dm-gta-story-projection-capture-receipt/1"
SITES = ((.2, .2), (.5, .2), (.8, .2), (.2, .5),
         (.8, .5), (.2, .8), (.5, .8), (.8, .8))


def write_new(path, value):
    path = Path(path)
    if path.exists():
        raise ContractError(f"refusing existing output: {path}")
    write_json_atomic(path, value)


def f32(value):
    return struct.unpack("f", struct.pack("f", value))[0]


def within(root, relative):
    path = (root / relative).resolve()
    if not path.is_relative_to(root.resolve()):
        raise ContractError(f"path escapes batch: {relative}")
    return path


def check_record(record, *, path=None):
    path = Path(record["path"]) if path is None else Path(path)
    if not path.is_file() or path.stat().st_size != record["bytes"] or sha256_file(path) != record["sha256"]:
        raise ContractError(f"file no longer matches captured bytes/SHA: {path}")
    return path


def generate(out_dir, reference_path):
    out_dir = Path(out_dir).resolve()
    reference_path = Path(reference_path).resolve()
    reference = validate_request(load_json(reference_path))
    out_dir.mkdir(parents=True, exist_ok=False)
    width, height = 1920, 1080
    # Preserve the smoke request's focal-to-FOV mapping as an explicit DESIGN
    # assumption, not an independently measured filmback.
    design_vertical_mm = (2 * reference["nominal_focal_mm"]
                          * math.tan(math.radians(reference["fov_deg"]) / 2))
    camera = [reference["camera_position"][0], reference["camera_position"][1], 8.0]
    rows = []
    for focal in (18, 24, 35, 50):
        fov = math.degrees(2 * math.atan(design_vertical_mm / (2 * focal)))
        fy = height / (2 * math.tan(math.radians(fov) / 2))
        for pitch in (30, 45):
            euler = [float(pitch), 0.0, 45.0]
            quaternion = _quaternion_zxy(euler)
            right, forward, up = camera_basis(quaternion)
            for depth in (4, 6):
                for site, (u, v) in enumerate(SITES, 1):
                    x = (u * width - width / 2) * depth / fy
                    z = (height / 2 - v * height) * depth / fy
                    world = [camera[i] + right[i] * x + forward[i] * depth + up[i] * z
                             for i in range(3)]
                    request = {"schema": REQUEST_SCHEMA, "camera_position": camera,
                               "camera_euler_deg": euler, "point_world": world,
                               "fov_deg": fov, "nominal_focal_mm": focal,
                               "marker_mode": "world_cross"}
                    validate_request(request)
                    sample_id = f"f{focal:02d}-p{pitch:02d}-d{depth:02d}-s{site:02d}"
                    request_path = out_dir / "requests" / f"{sample_id}.json"
                    write_new(request_path, request)
                    projected = project(world, position=camera, quaternion=quaternion,
                                        fx=fy, fy=fy, cx=width / 2, cy=height / 2)
                    narrowed_fy = height / (2 * math.tan(math.radians(f32(fov)) / 2))
                    narrowed = project([f32(c) for c in world], position=[f32(c) for c in camera],
                                       quaternion=quaternion, fx=narrowed_fy, fy=narrowed_fy,
                                       cx=width / 2, cy=height / 2)
                    error = math.hypot(projected[0] - width*u, projected[1] - height*v)
                    if error > 1e-7 or world[2] <= camera[2] or projected[2] < 2:
                        raise ContractError(f"offline geometry failed for {sample_id}")
                    rows.append({"sample_id": sample_id,
                                 "request": {"path": request_path.relative_to(out_dir).as_posix(),
                                             "bytes": request_path.stat().st_size,
                                             "sha256": sha256_file(request_path)},
                                 "output_parent": f"samples/{sample_id}",
                                 "planning_only": {"nominal_focal_mm": focal, "pitch_deg": pitch,
                                     "optical_depth_units": depth, "camera_space_right_forward_up": [x, depth, z],
                                     "design_screen_normalized": [u, v],
                                     "design_projection_error_px": error,
                                     "float32_design_shift_px": math.hypot(narrowed[0]-projected[0], narrowed[1]-projected[1]),
                                     "predicted_marker_full_length_px": .08 * fy / depth,
                                     "world_point_z": world[2]}})
    groups = []
    for focal in (18, 24, 35, 50):
        group = [r["planning_only"] for r in rows if r["planning_only"]["nominal_focal_mm"] == focal]
        us, vs = zip(*(r["design_screen_normalized"] for r in group))
        groups.append({"nominal_focal_mm": focal, "count": len(group),
                       "design_spread_fraction": [max(us)-min(us), max(vs)-min(vs)],
                       "optical_depth_units": sorted(set(r["optical_depth_units"] for r in group)),
                       "pitch_deg": sorted(set(r["pitch_deg"] for r in group)),
                       "marker_design_full_length_px_range": [min(r["predicted_marker_full_length_px"] for r in group),
                                                              max(r["predicted_marker_full_length_px"] for r in group)]})
    manifest = {"schema": MANIFEST_SCHEMA, "created_at": utc_now(), "sample_count": len(rows),
                "approval_status": "draft", "editor_equivalence_verified": False,
                "delivery_eligible": False, "measurement_state": "NOT_CAPTURED",
                "reference_smoke_request": file_record(reference_path),
                "generator": file_record(__file__),
                "design": {"render": {"width": width, "height": height},
                           "fov_mapping": "2*atan(design_vertical_mm/(2*nominal_focal_mm)); copied smoke mapping, not measured filmback",
                           "design_vertical_mm": design_vertical_mm, "camera_position": camera,
                           "world_geometry": "camera-position + right*x + forward*depth + up*z; GTA order2 qz*qx*qy",
                           "marker": "native world_cross, full 0.08 GTA units, magenta",
                           "depth_design_reason": "4/6 optical-depth units keep the existing 0.08-unit cross at least 10.8px across at the widest 18mm nominal FOV; this exceeds the independent detector fixed 7px span without changing its thresholds. Both depths exceed solver minimum 2 units.",
                           "world_point_z_range": [min(r["planning_only"]["world_point_z"] for r in rows),
                                                   max(r["planning_only"]["world_point_z"] for r in rows)],
                           "max_float32_design_shift_px": max(r["planning_only"]["float32_design_shift_px"] for r in rows),
                           "per_focal": groups},
                "limitations": ["All UV in this plan are predicted design positions, never pixel observations.",
                    "Real capture success requires native PNG+observation; measured pixels must be independently detected from that PNG.",
                    "Sky/clearance is a geometric design intent only. Actual occlusion, marker visibility and renderer FOV must be checked from captured pixels.",
                    "Story sampling does not establish Editor/RE+ projection equivalence, metric scale approval, or formal delivery eligibility."],
                "samples": rows}
    write_new(out_dir / "manifest.json", manifest)
    print(json.dumps({"manifest": str(out_dir / "manifest.json"), "sha256": sha256_file(out_dir / "manifest.json"),
                      "sample_count": len(rows), "design": manifest["design"]}, indent=2))


def load_manifest(path):
    path = Path(path).resolve()
    manifest = load_json(path)
    if manifest.get("schema") != MANIFEST_SCHEMA or manifest.get("sample_count") != 128 or len(manifest.get("samples", [])) != 128:
        raise ContractError("expected a complete generated 128-request Story plan")
    rows = manifest["samples"]
    if len({r["sample_id"] for r in rows}) != 128:
        raise ContractError("duplicate sample IDs")
    for row in rows:
        request = within(path.parent, row["request"]["path"])
        check_record(row["request"], path=request)
        validate_request(load_json(request))
        within(path.parent, row["output_parent"])
    return path, manifest


def verify_attempt(directory, manifest_path, row):
    receipt = load_json(directory / "capture-receipt.json")
    if receipt.get("schema") != RECEIPT_SCHEMA or receipt.get("sample_id") != row["sample_id"]:
        raise ContractError(f"capture receipt does not bind sample {directory}")
    if receipt.get("manifest_sha256") != sha256_file(manifest_path):
        raise ContractError("capture receipt plan hash changed")
    if receipt.get("approval_status") != "draft" or receipt.get("editor_equivalence_verified") is not False or receipt.get("delivery_eligible") is not False:
        raise ContractError("capture receipt improperly claims approval")
    for name in ("request.json", "point.png", "observation.json", "runtime-before.json"):
        record = receipt["files"][name]
        if Path(record["path"]).resolve() != (directory / name).resolve():
            raise ContractError("capture receipt artifact path changed")
        check_record(record)
    observation = load_json(directory / "observation.json")
    if (observation.get("schema") != OBSERVATION_SCHEMA or observation.get("mock") is not False
            or observation.get("approval_status") != "draft"
            or observation.get("editor_equivalence_verified") is not False
            or observation.get("delivery_eligible") is not False):
        raise ContractError("existing observation must remain real/unapproved Story diagnostic")
    request = within(manifest_path.parent, row["request"]["path"])
    if load_json(directory / "request.json") != load_json(request):
        raise ContractError("native request snapshot differs from planned request")
    if observation.get("request_sha256") != sha256_file(directory / "request.json"):
        raise ContractError("observation no longer binds request snapshot")
    png = (directory / "point.png").resolve()
    if (Path(observation.get("output_path", "")).resolve() != png
            or observation.get("output_sha256") != sha256_file(png)
            or observation.get("output_bytes") != png.stat().st_size):
        raise ContractError("native PNG binding changed")
    with png.open("rb") as stream:
        header = stream.read(24)
    if len(header) != 24 or header[:8] != b"\x89PNG\r\n\x1a\n" or header[12:16] != b"IHDR":
        raise ContractError("native image is not PNG")
    dims = struct.unpack(">II", header[16:24])
    actual = (observation.get("width"), observation.get("height"))
    if any(type(n) is not int or n < 1 for n in actual) or actual != dims:
        raise ContractError("native PNG dimensions no longer match")
    runtime = load_json(directory / "runtime-before.json")
    if runtime.get("story_mode") is not True or runtime.get("online_session") is not False:
        raise ContractError("existing capture was not explicitly offline Story")
    return {"sample_id": row["sample_id"], "attempt": str(directory),
            "receipt_sha256": sha256_file(directory / "capture-receipt.json"),
            "png_sha256": sha256_file(png), "pixel_detection": "pending external independent PNG detector"}


def attempts_for(root, row):
    parent = within(root, row["output_parent"])
    if not parent.exists():
        return []
    entries = sorted(parent.iterdir())
    if any(not item.is_dir() or not re.fullmatch(r"attempt-\d{3}", item.name) for item in entries):
        raise ContractError(f"unexpected entries in per-sample output directory {parent}")
    return entries


def run(args):
    manifest_path, manifest = load_manifest(args.manifest)
    manifest_sha = sha256_file(manifest_path)
    root = manifest_path.parent
    rows = manifest["samples"]
    if args.sample:
        wanted = set(args.sample)
        if not wanted <= {r["sample_id"] for r in rows}:
            raise ContractError("unknown --sample ID")
        rows = [r for r in rows if r["sample_id"] in wanted]
    if args.limit is not None:
        rows = rows[:args.limit]
    if args.command == "capture" and args.retry_failed and not args.resume:
        raise ContractError("--retry-failed requires explicit --resume")
    # Re-open/hash every requested completed sample before any game operation.
    statuses = []
    for row in rows:
        attempts = attempts_for(root, row)
        complete = [p for p in attempts if (p / "capture-receipt.json").is_file()]
        if len(complete) > 1:
            raise ContractError(f"multiple completed attempts require explicit review: {row['sample_id']}")
        existing = verify_attempt(complete[0], manifest_path, row) if complete else None
        if args.command == "capture" and attempts:
            if not args.resume:
                raise ContractError("existing attempt(s); explicit --resume is required")
            if existing is None and not args.retry_failed:
                raise ContractError(f"failed/incomplete attempt preserved; --retry-failed required: {row['sample_id']}")
        statuses.append((row, attempts, existing))
    if args.command == "verify":
        print(json.dumps({"manifest_sha256": manifest_sha, "planned": len(rows),
                          "captured_rehashed": sum(existing is not None for _, _, existing in statuses),
                          "failed_or_incomplete_attempts": sum(sum(not (p / "capture-receipt.json").exists() for p in attempts) for _, attempts, _ in statuses),
                          "game_pipe_opened": False, "independent_pixel_detection_performed": False,
                          "completed": [existing for _, _, existing in statuses if existing]}, indent=2))
        return 0
    logdir = root / "logs"
    logdir.mkdir(exist_ok=True)
    logpath = logdir / (datetime.now(timezone.utc).strftime("capture-%Y%m%dT%H%M%SZ-") + uuid.uuid4().hex[:8] + ".jsonl")
    print(f"Full progress/error log: {logpath}", flush=True)
    failures = 0
    with logpath.open("x", encoding="utf-8") as log:
        def emit(data):
            data = {"at": utc_now(), **data}
            line = json.dumps(data, ensure_ascii=False)
            log.write(line + "\n")
            log.flush()
            print(line, flush=True)
        emit({"event": "start", "manifest": str(manifest_path), "manifest_sha256": manifest_sha,
              "helper": file_record(__file__), "sample_count": len(rows), "pipe": args.pipe})
        for row, attempts, existing in statuses:
            if existing:
                emit({"event": "skip_rehashed_success", **existing})
                continue
            number = max((int(p.name.split("-")[1]) for p in attempts), default=0) + 1
            if number > 999:
                raise ContractError("attempt namespace exhausted")
            output = within(root, row["output_parent"]) / f"attempt-{number:03d}"
            request = within(root, row["request"]["path"])
            emit({"event": "capture_start", "sample_id": row["sample_id"], "output": str(output)})
            try:
                if sha256_file(manifest_path) != manifest_sha:
                    raise ContractError("plan manifest changed during batch")
                check_record(row["request"], path=request)
                capture_sample(request, output, pipe=args.pipe)
                check_record(row["request"], path=request)
                if sha256_file(manifest_path) != manifest_sha:
                    raise ContractError("plan manifest changed during capture")
                receipt = {"schema": RECEIPT_SCHEMA, "created_at": utc_now(), "sample_id": row["sample_id"],
                           "manifest_sha256": manifest_sha, "helper": file_record(__file__),
                           "approval_status": "draft", "editor_equivalence_verified": False,
                           "delivery_eligible": False, "pixel_detection": "pending external independent PNG detector",
                           "files": {name: file_record(output / name) for name in
                                     ("request.json", "point.png", "observation.json", "runtime-before.json")}}
                write_new(output / "capture-receipt.json", receipt)
                verified = verify_attempt(output, manifest_path, row)
                emit({"event": "capture_saved", **verified})
            except Exception as exc:
                failures += 1
                # Preserve all native output and a unique failure record. Never
                # retry/overwrite a failed attempt implicitly.
                error = {"event": "capture_failed", "sample_id": row["sample_id"],
                         "output": str(output), "error_type": type(exc).__name__,
                         "error": str(exc), "traceback": traceback.format_exc()}
                if output.is_dir() and not (output / "failure.json").exists():
                    write_new(output / "failure.json", error)
                emit(error)
                if not args.continue_on_error:
                    break
        emit({"event": "end", "failed_attempts_this_invocation": failures,
              "approval_status": "draft", "editor_equivalence_verified": False,
              "independent_pixel_detection_performed": False})
    return 1 if failures else 0


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    make = commands.add_parser("generate", help="offline only; create NEW plan and 128 requests")
    make.add_argument("--out-dir", type=Path, required=True)
    make.add_argument("--reference-request", type=Path, default=Path(__file__).with_name("story-projection-smoke-request.json"))
    for name in ("verify", "capture"):
        sub = commands.add_parser(name, help="offline rehash only" if name == "verify" else "explicit real game pipe sampling")
        sub.add_argument("--manifest", type=Path, required=True)
        sub.add_argument("--sample", action="append", help="restrict to exact sample ID; repeatable")
        sub.add_argument("--limit", type=int, help="restrict to first N selected samples")
        if name == "capture":
            sub.add_argument("--pipe", default=DEFAULT_PIPE)
            sub.add_argument("--resume", action="store_true", help="rehash and skip existing completed observations")
            sub.add_argument("--retry-failed", action="store_true", help="with --resume, create new attempt dirs for incomplete/failed samples")
            sub.add_argument("--continue-on-error", action="store_true", help="explicitly continue later points after an error; default stops")
    args = parser.parse_args()
    if getattr(args, "limit", None) is not None and args.limit < 1:
        parser.error("--limit must be positive")
    try:
        if args.command == "generate":
            generate(args.out_dir, args.reference_request)
            return 0
        return run(args)
    except Exception as exc:
        print(f"{type(exc).__name__}: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())

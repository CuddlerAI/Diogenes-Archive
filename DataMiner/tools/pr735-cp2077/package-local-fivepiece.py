#!/usr/bin/env python3
"""Package one exact-launch CP2077 raw demo as an honest local five-piece bundle.

This intentionally does not produce the PR405 metric EXR contract.  It preserves
native D32 bytes and records the physical-matrix selection provenance.
"""

from __future__ import annotations

import argparse
import bisect
import hashlib
import json
import os
import shutil
import subprocess
import zipfile
from pathlib import Path
import sys

import zstandard

sys.path.insert(0, str(Path(__file__).resolve().parents[3] / "ml"))
from diogenes_wm.eval.cp2077_camera_report import read_camera_report


def load_json(path: Path):
    return json.loads(path.read_text(encoding="utf-8-sig"))


def sha256(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        for block in iter(lambda: f.read(8 * 1024 * 1024), b""):
            h.update(block)
    return "sha256:" + h.hexdigest()


MIN_WATERMARK_RATE = 0.98
MIN_EXACT_DEPTH_RGB_COVERAGE = 0.98
ISSUE735_TARGET_DEPTH_HZ = 16
# Mirrors the client's RGB_COMPOSITE_LAG_PRESENT_INTERVALS and PAIRING_SCHEMA
# in cp2077_raw_demo.rs; the recomputation below must change with that report.
RGB_COMPOSITE_LAG_PRESENT_INTERVALS = 3
PAIRING_SCHEMA = "dm-cp2077-local-raw-rgb-depth-pairing/v2"
MAX_CAMERA_QPC_ERROR_SECONDS = 1 / 30
MAX_PLAYER_QPC_ERROR_SECONDS = 0.150


def nearest_within(rows, qpc, max_delta, label):
    if not rows:
        raise SystemExit(f"{label} observations are empty")
    keys = [row[0] for row in rows]
    pos = bisect.bisect_left(keys, qpc)
    candidates = []
    if pos < len(rows):
        candidates.append(rows[pos])
    if pos:
        candidates.append(rows[pos - 1])
    selected = min(candidates, key=lambda row: abs(row[0] - qpc))
    delta = abs(selected[0] - qpc)
    if delta > max_delta:
        raise SystemExit(
            f"{label} observation is too far from target QPC: {delta} > {max_delta}"
        )
    return selected, delta


def matrix_key(m):
    return (
        m["source_tag"], m["root_parameter_index"],
        m["cbv_resource_offset"], m["matrix_offset"], m["transpose"],
    )


def load_jsonl(path: Path):
    return [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines() if line]


def load_zstd_jsonl(path: Path):
    with path.open("rb") as source:
        with zstandard.ZstdDecompressor().stream_reader(source) as reader:
            text = reader.read().decode("utf-8")
    return [json.loads(line) for line in text.splitlines() if line]


def link_or_copy(source: Path, destination: Path):
    try:
        os.link(source, destination)
    except OSError:
        shutil.copy2(source, destination)


def direct_child(root: Path, name: str, label: str) -> Path:
    if not isinstance(name, str) or Path(name).name != name or name in ("", ".", ".."):
        raise SystemExit(f"{label} path is not a plain leaf name")
    if root.is_symlink():
        raise SystemExit(f"{label} root must not be a symlink")
    canonical_root = root.resolve(strict=True)
    unresolved = canonical_root / name
    if unresolved.is_symlink():
        raise SystemExit(f"{label} must not be a symlink")
    candidate = unresolved.resolve(strict=True)
    if candidate.parent != canonical_root or not candidate.is_file():
        raise SystemExit(f"{label} escapes its expected directory")
    return candidate


def verify_source_segment(segment: Path, meta):
    source_manifest = load_json(segment / "sha256.json")
    current = {
        name: sha256(direct_child(segment, name, f"source {name}"))
        for name in ("video.mp4", "frames.jsonl.zst", "events.jsonl.zst", "meta.json")
    }
    if any(source_manifest.get(name) != digest for name, digest in current.items()):
        raise SystemExit("main collector source bytes differ from sha256.json")
    for key, name in (
        ("video", "video.mp4"),
        ("frames", "frames.jsonl.zst"),
        ("events", "events.jsonl.zst"),
    ):
        record = meta.get(key)
        if (
            not isinstance(record, dict)
            or record.get("path") != name
            or record.get("sha256") != current[name]
        ):
            raise SystemExit(f"main collector {key} metadata differs from source bytes")
    frame_rows = load_zstd_jsonl(segment / "frames.jsonl.zst")
    if (
        len(frame_rows) != meta.get("n_frames")
        or meta["frames"].get("lines") != len(frame_rows)
        or any(
            not isinstance(row.get("session_frame"), int)
            or not isinstance(row.get("source_qpc"), int)
            for row in frame_rows
        )
    ):
        raise SystemExit("main collector frame timing index is inconsistent")
    return frame_rows


def package_raw_input(segment: Path, staging: Path, meta, confirmed_flush):
    events_path = segment / "events.jsonl.zst"
    meta_path = segment / "meta.json"
    source_manifest_path = segment / "sha256.json"
    source_manifest = load_json(source_manifest_path)
    events_sha256 = sha256(events_path)
    events = load_zstd_jsonl(events_path)
    keyboard_down = sum(row.get("type") == "key_down" for row in events)
    keyboard_up = sum(row.get("type") == "key_up" for row in events)
    mouse_move = sum(row.get("type") == "mouse_move" for row in events)
    input_health = meta.get("health", {})
    event_meta = meta.get("events", {})
    if not (
        meta.get("input_status") == "raw_input_v1"
        and event_meta.get("path") == "events.jsonl.zst"
        and event_meta.get("lines") == len(events)
        and event_meta.get("sha256") == events_sha256
        and source_manifest.get("events.jsonl.zst") == events_sha256
        and input_health.get("input_raw_reports", 0) >= len(events)
        and input_health.get("input_overflow") == 0
        and input_health.get("input_boundary_reorder_drops") == 0
        and confirmed_flush.get("input_queue_overflows") == 0
    ):
        raise SystemExit("Raw Input files, counts, hashes, or zero-drop evidence are inconsistent")
    if keyboard_down == 0 or keyboard_up == 0:
        raise SystemExit("Raw Input package has no complete keyboard activity")
    if mouse_move == 0:
        raise SystemExit("Raw Input package has no mouse movement")

    input_dir = staging / "raw-input"
    input_dir.mkdir()
    link_or_copy(events_path, input_dir / "events.jsonl.zst")
    shutil.copy2(meta_path, input_dir / "meta.json")
    shutil.copy2(source_manifest_path, input_dir / "sha256.json")
    contract = {
        "schema": "dm-cp2077-local-raw-input-artifact/v1",
        "provenance": "hardware_raw_input",
        "event_count": len(events),
        "keyboard_event_count": keyboard_down + keyboard_up,
        "keyboard_down_count": keyboard_down,
        "keyboard_up_count": keyboard_up,
        "mouse_move_event_count": mouse_move,
        "raw_report_count": input_health["input_raw_reports"],
        "input_overflow": 0,
        "input_boundary_reorder_drops": 0,
        "input_queue_overflows": 0,
        "files": {
            "events": {"path": "raw-input/events.jsonl.zst", "sha256": events_sha256},
            "meta": {"path": "raw-input/meta.json", "sha256": sha256(input_dir / "meta.json")},
            "source_manifest": {
                "path": "raw-input/sha256.json",
                "sha256": sha256(input_dir / "sha256.json"),
            },
        },
        "non_trainable": True,
        "recording_ready": False,
    }
    contract_path = input_dir / "contract.json"
    contract_path.write_text(
        json.dumps(contract, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )
    contract["contract_sha256"] = sha256(contract_path)
    return contract


def crc8_low16(value: int) -> int:
    crc = 0
    for byte in value.to_bytes(2, "big"):
        crc ^= byte
        for _ in range(8):
            crc = ((crc << 1) ^ 0x07) & 0xFF if crc & 0x80 else (crc << 1) & 0xFF
    return crc ^ 0xAA


def decode_watermark(means):
    if len(means) != 24 or any(96 < value < 160 for value in means):
        return None
    bits = [value >= 160 for value in means]
    integrity = 0
    for bit in bits[:8]:
        integrity = (integrity << 1) | bit
    value = 0
    for bit in bits[8:]:
        value = (value << 1) | bit
    return value if integrity == crc8_low16(value) else None


def median_present_interval_ticks(present_qpcs):
    intervals = sorted(
        later - earlier
        for earlier, later in zip(present_qpcs, present_qpcs[1:])
        if later - earlier > 0
    )
    return intervals[len(intervals) // 2] if intervals else None


def rgb_depth_pairing_window(qpc_frequency, present_qpcs):
    """(lead, lag) bound on rgb_source_qpc - depth_present_qpc, as the client.

    The compositor stamps RGB after the game's Present, so the lead keeps the
    historical 1/16 s slack while the lag follows three measured Present
    intervals and is never narrower than that slack.
    """
    if qpc_frequency <= 0:
        return 0, 0
    target_rate_ticks = qpc_frequency // ISSUE735_TARGET_DEPTH_HZ
    cadence_ticks = (
        median_present_interval_ticks(present_qpcs) or 0
    ) * RGB_COMPOSITE_LAG_PRESENT_INTERVALS
    return target_rate_ticks, max(target_rate_ticks, cadence_ticks)


def validate_strong_pairing(
    pairing, meta, source_depth_rows, watermark_rows, frame_rows, qpc_frequency
):
    """Recompute the client's v2 exact-Present pairing report and refuse drift.

    This used to replay the v1 symmetric +/-1/16 s window and v1 fields, so it
    rejected every report the client has written since switching to the
    one-sided compositor-lag window. Decoding walks RGB frames in order and
    keeps the first nearest accepted candidate, matching the client's tie-break.
    """
    if qpc_frequency <= 0:
        raise SystemExit("pairing QPC frequency is invalid")
    means_by_frame = {
        row.get("session_frame"): row.get("block_means", []) for row in watermark_rows
    }
    decoded = []
    for row in frame_rows:
        means = means_by_frame.get(row["session_frame"])
        if means is None:
            continue
        value = decode_watermark(means)
        if value is not None:
            decoded.append((value, row["source_qpc"]))
    qpcs_by_low16 = {}
    for value, qpc in decoded:
        qpcs_by_low16.setdefault(value, []).append(qpc)
    present_qpcs = [row["present_qpc"] for row in source_depth_rows]
    lead, lag = rgb_depth_pairing_window(qpc_frequency, present_qpcs)
    selected = []
    lags = []
    for row in source_depth_rows:
        accepted = [
            qpc - row["present_qpc"]
            for qpc in qpcs_by_low16.get(row["present_seq"] & 0xFFFF, [])
            if -lead <= qpc - row["present_qpc"] <= lag
        ]
        if accepted:
            lags.append(min(accepted, key=abs))
            selected.append(row)
    lags.sort()
    decoded_rate = len(decoded) / meta["n_frames"] if meta["n_frames"] else 0.0
    coverage = len(selected) / len(source_depth_rows) if source_depth_rows else 0.0
    span = (
        max(0, source_depth_rows[-1]["present_seq"] - source_depth_rows[0]["present_seq"])
        if source_depth_rows
        else 0
    )
    expected = {
        "schema": PAIRING_SCHEMA,
        "passed": True,
        "rgb_frame_count": meta["n_frames"],
        "decoded_rgb_watermarks": len(decoded),
        "depth_frame_count": len(source_depth_rows),
        "exact_present_matches": len(selected),
        "measured_present_interval_ticks": median_present_interval_ticks(present_qpcs) or 0,
        "pairing_window_lead_ticks": lead,
        "pairing_window_lag_ticks": lag,
        "min_compositor_lag_ticks": lags[0] if lags else 0,
        "median_compositor_lag_ticks": lags[len(lags) // 2] if lags else 0,
        "max_compositor_lag_ticks": lags[-1] if lags else 0,
        "rgb_before_depth_pairs": sum(1 for delta in lags if delta < 0),
        "unmatched_depth_present_count": len(source_depth_rows) - len(selected),
        "low16_unambiguous_within_session": span < 0xFFFF,
        "blockers": [],
        "non_trainable": True,
        "recording_ready": False,
    }
    if any(pairing.get(key) != value for key, value in expected.items()):
        raise SystemExit("main collector strong RGB/depth pairing report is inconsistent")
    if (
        abs(pairing.get("decoded_rgb_watermark_rate", -1.0) - decoded_rate) > 1e-12
        or abs(pairing.get("exact_depth_rgb_coverage", -1.0) - coverage) > 1e-12
        or decoded_rate < MIN_WATERMARK_RATE
        or coverage < MIN_EXACT_DEPTH_RGB_COVERAGE
    ):
        raise SystemExit(
            "main collector strong RGB/depth pairing thresholds are not satisfied"
        )
    return selected


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("session", type=Path)
    ap.add_argument("output", type=Path)
    args = ap.parse_args()
    session = args.session.resolve()
    output = args.output.resolve()
    if output.exists():
        raise SystemExit(f"output already exists: {output}")
    staging = output.with_name(output.name + ".staging")
    if staging.exists():
        raise SystemExit(f"staging already exists: {staging}")
    staging.mkdir(parents=True)

    telemetry = load_json(session / "raw-depth-telemetry.json")
    qc = load_json(session / "raw-depth-qc.json")
    pairing_path = session / "raw-rgb-depth-pairing.json"
    pairing = load_json(pairing_path)
    camera = read_camera_report(session / "render-camera-observation.json")["observation"]
    pose = load_json(session / "cp2077-pose-runtime-observation.json")
    launch_path = session / "operator-launch-record.json"
    if not launch_path.exists():
        launch_path = session / "operator-launch.json"
    if not launch_path.exists():
        launch_path = session / "operator-launch-local.json"
    launch = load_json(launch_path)
    plan = load_json(session / "operator-action-plan.json")
    markers = load_jsonl(session / "operator-action-markers.jsonl")
    settings = load_json(session / "pr727-settings-runtime-evidence.json")
    main_run = load_json(session / "main-collector-run.json")
    confirmed_flush = load_json(session / "main-collector-confirmed-flush.json")
    if not (
        main_run.get("collector_start_session") is True
        and main_run.get("journal_committed_before_arm") is True
        and main_run.get("main_collector_formal_start_run") is True
        and main_run.get("sidecar_probe_formal_start_run") is False
        and confirmed_flush.get("confirmed_flush") is True
        and confirmed_flush.get("main_collector_formal_start_run") is True
        and confirmed_flush.get("sidecar_probe_formal_start_run") is False
    ):
        raise SystemExit("formal main collector start/flush evidence is incomplete")
    identity = (
        main_run["session_id"],
        main_run["run_id"],
        main_run["generation"],
        main_run["process_id"],
        main_run["process_launch_id"],
    )
    flush_identity = (
        confirmed_flush["session_id"],
        confirmed_flush["run_id"],
        confirmed_flush["generation"],
        confirmed_flush["process_id"],
        confirmed_flush["process_launch_id"],
    )
    if identity != flush_identity:
        raise SystemExit("main collector start/flush session/run identity mismatch")
    manual_stop = telemetry.get("manual_stop", False)
    if manual_stop:
        if not (60000 <= telemetry["elapsed_ms"] <= 240000):
            raise SystemExit("manual-stop depth duration is outside 60..240 seconds")
        if not (900 <= telemetry["written_frames"] <= 4096):
            raise SystemExit("manual-stop depth frame count is outside the admitted range")
    elif not (63000 <= telemetry["elapsed_ms"] <= 65000
              and telemetry["duration_ms"] == 64000
              and telemetry["written_frames"] == 1024):
        raise SystemExit("automatic depth capture is not the exact 64-second contract")
    if not qc["passed"]:
        raise SystemExit("depth frame/QC gate failed")
    if telemetry["process_launch_id"] != launch["launch_id"]:
        raise SystemExit("launch identity mismatch")
    if identity[3] != launch["process_id"] or identity[4] != launch["launch_id"]:
        raise SystemExit("main collector PID/launch identity mismatch")
    if len(camera["observations"]) < 120:
        raise SystemExit("too few render-camera observations")

    segment = next(session.glob("rgb-recorder/**/segments/00000"))
    meta = load_json(segment / "meta.json")
    frame_rows = verify_source_segment(segment, meta)
    raw_input = package_raw_input(segment, staging, meta, confirmed_flush)
    rgb_duration_ms = round(
        (meta["qpc_end"] - meta["qpc_start"]) * 1000 / telemetry["qpc_frequency"]
    )
    expected_rgb_frames = round(rgb_duration_ms * 30 / 1000)
    if (meta["n_frames"] != expected_rgb_frames
            or not (60000 <= rgb_duration_ms <= (240000 if manual_stop else 65000))):
        raise SystemExit("RGB frame count/duration does not match the stopped capture window")
    video_out = staging / "video.mp4"
    link_or_copy(segment / "video.mp4", video_out)

    raw_depth_index_path = session / "raw-depth-index.jsonl"
    source_depth_rows = load_jsonl(raw_depth_index_path)
    if len(source_depth_rows) != qc["frame_count"]:
        raise SystemExit("source depth index/QC count mismatch")
    if qc.get("raw_depth_index_sha256") != sha256(raw_depth_index_path):
        raise SystemExit("source depth index bytes changed after depth QC")
    watermark_rows = load_jsonl(session / "watermark-samples.jsonl")
    depth_rows = validate_strong_pairing(
        pairing,
        meta,
        source_depth_rows,
        watermark_rows,
        frame_rows,
        telemetry["qpc_frequency"],
    )
    camera_rows = camera["observations"]
    camera_by_qpc = sorted(
        ((row["present_qpc"], row) for row in camera_rows),
        key=lambda item: (item[0], item[1]["present_seq"]),
    )
    player_rows = []
    for row in pose["snapshot_observations"]:
        if row["role"] != "player" or len(row.get("candidates", [])) != 1:
            continue
        c = row["candidates"][0]
        player_rows.append((row["qpc"], row["present_seq"], row["object_tag"], c))
    player_rows.sort()
    if not player_rows:
        raise SystemExit("no unique player samples")

    # The selected physical source is stable, but this local bundle preserves
    # the actual root-parameter generation seen at each Present.  It must not
    # rewrite a generation change into a fixed production binding.
    selected_view_suffix = (979, 0, 0, True)
    selected_projection_suffix = (979, 0, 128, True)

    def choose_physical(matrices, kind, suffix):
        matches = [m for m in matrices if m["matrix_kind"] == kind and (
            m["source_tag"], m["cbv_resource_offset"], m["matrix_offset"], m["transpose"]
        ) == suffix]
        return matches[0] if len(matches) == 1 else None

    input_provenance = plan["input_provenance"]
    max_camera_qpc_delta = max(
        1, round(telemetry["qpc_frequency"] * MAX_CAMERA_QPC_ERROR_SECONDS)
    )
    max_player_qpc_delta = max(
        1, round(telemetry["qpc_frequency"] * MAX_PLAYER_QPC_ERROR_SECONDS)
    )
    action15 = []
    for d in depth_rows:
        ((_, cam), camera_delta) = nearest_within(
            camera_by_qpc, d["present_qpc"], max_camera_qpc_delta, "camera"
        )
        ((player_qpc, pseq, ptag, player), player_delta) = nearest_within(
            player_rows, d["present_qpc"], max_player_qpc_delta, "player"
        )
        views = [m for m in cam["matrices"] if m["matrix_kind"] == "view"]
        projs = [m for m in cam["matrices"] if m["matrix_kind"] == "projection"]
        view = choose_physical(views, "view", selected_view_suffix)
        proj = choose_physical(projs, "projection", selected_projection_suffix)
        action15.append({
            "frame": d["ordinal"] * 2,
            "time": d["ordinal"] / 16.0,
            "depth_name": f"depth_{d['ordinal'] * 2}.d32",
            "depth_present_seq": d["present_seq"],
            "depth_present_qpc": d["present_qpc"],
            "camera_present_seq": cam["present_seq"],
            "camera_present_qpc": cam["present_qpc"],
            "camera_qpc_error_us": camera_delta * 1_000_000 / telemetry["qpc_frequency"],
            "camera_view_matrix": None if view is None else view["matrix"],
            "camera_projection_matrix": None if proj is None else proj["matrix"],
            "camera_source": cam["physical_source"],
            "camera_selection_provenance": {
                "view_key_suffix": selected_view_suffix,
                "projection_key_suffix": selected_projection_suffix,
                "root_parameter_index": None if view is None else view["root_parameter_index"],
                "all_physical_candidates_preserved_in": "evidence/render-camera-observation.json",
            },
            "player_present_seq": pseq,
            "player_present_qpc": player_qpc,
            "player_qpc_error_us": player_delta * 1_000_000 / telemetry["qpc_frequency"],
            "player_object_tag": ptag,
            "player_position_world_units": player["position_world_units"],
            "player_quaternion_ijkr": player["raw_quaternion_ijkr"],
            "input_provenance": input_provenance,
            "metric_scale_resolved": False,
            "non_trainable": True,
        })

    # Full-rate rows are anchored to the exact 30 fps CFR output QPC.  Camera/player
    # samples are nearest physical observations with explicit error, never invented.
    action30 = []
    for frame in range(meta["n_frames"]):
        qpc = meta["qpc_start"] + round(frame * telemetry["qpc_frequency"] / 30)
        ((_, cam), camera_delta) = nearest_within(
            camera_by_qpc, qpc, max_camera_qpc_delta, "camera"
        )
        ((player_qpc, pseq, ptag, player), player_delta) = nearest_within(
            player_rows, qpc, max_player_qpc_delta, "player"
        )
        views = [m for m in cam["matrices"] if m["matrix_kind"] == "view"]
        projs = [m for m in cam["matrices"] if m["matrix_kind"] == "projection"]
        view = choose_physical(views, "view", selected_view_suffix)
        proj = choose_physical(projs, "projection", selected_projection_suffix)
        action30.append({
            "frame": frame, "time": frame / 30.0, "output_qpc": qpc,
            "camera_present_seq": cam["present_seq"],
            "camera_present_qpc": cam["present_qpc"],
            "camera_qpc_error_us": camera_delta * 1_000_000 / telemetry["qpc_frequency"],
            "camera_view_matrix": None if view is None else view["matrix"],
            "camera_projection_matrix": None if proj is None else proj["matrix"],
            "player_present_seq": pseq, "player_present_qpc": player_qpc,
            "player_qpc_error_us": player_delta * 1_000_000 / telemetry["qpc_frequency"],
            "player_object_tag": ptag,
            "player_position_world_units": player["position_world_units"],
            "player_quaternion_ijkr": player["raw_quaternion_ijkr"],
            "input_provenance": input_provenance,
            "metric_scale_resolved": False, "non_trainable": True,
        })

    (staging / "action_camera.json").write_text(json.dumps(action15, ensure_ascii=False, separators=(",", ":")) + "\n", encoding="utf-8")
    (staging / "action_camera_30fps.json").write_text(json.dumps(action30, ensure_ascii=False, separators=(",", ":")) + "\n", encoding="utf-8")
    systeminfo = {
        "schema": "dm-cp2077-issue735-local-systeminfo/v1",
        "gameProcessName": "Cyberpunk2077.exe", "x": 0, "y": 0,
        "width": meta["width"], "height": meta["height"], "recordDpi": 1.0,
        "capture_source": "bound_hwnd_wgc", "depth_backend": "D3D12",
        "process_launch_id": telemetry["process_launch_id"],
        "session_id": identity[0], "run_id": identity[1], "generation": identity[2],
        "executable_sha256": meta["target"]["executable_sha256"],
        "settings_sha256": settings["settings_sha256"],
        "raw_input": {
            "provenance": raw_input["provenance"],
            "event_count": raw_input["event_count"],
            "keyboard_event_count": raw_input["keyboard_event_count"],
            "mouse_move_event_count": raw_input["mouse_move_event_count"],
            "contract": "raw-input/contract.json",
            "contract_sha256": raw_input["contract_sha256"],
        },
        "non_trainable": True,
    }
    (staging / "systeminfo.json").write_text(json.dumps(systeminfo, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")

    with zipfile.ZipFile(staging / "depth.zip", "w", compression=zipfile.ZIP_STORED, allowZip64=True) as z:
        for row in depth_rows:
            source_depth = direct_child(session / "raw-depth", row["file"], "raw depth frame")
            z.write(source_depth, f"depth_{row['ordinal'] * 2}.d32")
        z.writestr("DEPTH-CONTRACT.json", json.dumps({
            "schema": "dm-cp2077-issue735-native-d32-depth/v1",
            "pixel_format": "DXGI_FORMAT_D32_FLOAT_LE", "encoding": "native_reverse_z_non_metric",
            "width": 1920, "height": 1080, "nominal_fps": 16,
            "frame_count": len(depth_rows),
            "source_written_frame_count": telemetry["written_frames"],
            "selection": "same_present_rgb_pairing_only",
            "formal_pr405_metric_exr": False, "non_trainable": True,
        }, indent=2) + "\n")

    evidence = staging / "evidence"
    evidence.mkdir()
    for name in [
        "raw-depth-qc.json", "raw-depth-telemetry.json", "raw-depth-index.jsonl",
        "render-camera-observation.json", "render-camera-draws.jsonl", "cp2077-pose-runtime-observation.json",
        "raw-rgb-depth-pairing.json", "watermark-samples.jsonl",
        "operator-action-plan.json", "operator-action-markers.jsonl",
        "pr727-settings-runtime-evidence.json", "raw-depth-demo-profile.json",
        "main-collector-run.json", "main-collector-confirmed-flush.json",
        "raw-demo-result.json", "pose-camera-loop-qc.json",
    ]:
        source = launch_path if name == "operator-launch-record.json" else session / name
        if source.exists():
            shutil.copy2(source, evidence / source.name)
    shutil.copy2(launch_path, evidence / launch_path.name)
    qc_summary = {
        "schema": "dm-cp2077-issue735-local-fivepiece-qc/v1",
        "five_named_artifacts_present": True, "duration_ms": rgb_duration_ms,
        "session_id": identity[0], "run_id": identity[1], "generation": identity[2],
        "process_id": identity[3], "process_launch_id": identity[4],
        "manual_stop": manual_stop,
        "rgb_frames": meta["n_frames"], "depth_frames": len(depth_rows),
        "source_depth_frames": telemetry["written_frames"],
        "exact_depth_rgb_coverage": pairing["exact_depth_rgb_coverage"],
        "camera_observations": len(camera_rows), "player_snapshots": len(player_rows),
        "hardware_raw_input_reports": meta["health"]["input_raw_reports"],
        "raw_input_events": raw_input["event_count"],
        "keyboard_input_events": raw_input["keyboard_event_count"],
        "keyboard_down_events": raw_input["keyboard_down_count"],
        "keyboard_up_events": raw_input["keyboard_up_count"],
        "mouse_move_events": raw_input["mouse_move_event_count"],
        "raw_input_artifact_present": True,
        "raw_input_events_sha256": raw_input["files"]["events"]["sha256"],
        "raw_input_contract_sha256": raw_input["contract_sha256"],
        "input_provenance": input_provenance,
        "action_marker_count": len(markers),
        "depth_pixel_qc_passed": qc["passed"], "depth_drop_count": 0,
        "confirmed_flush": True,
        "main_collector_formal_start_run": True,
        "sidecar_probe_formal_start_run": False,
        "movement_observed": any(t.get("max_displacement_from_first_world_units", 0) > 0 for t in pose["trajectories"]),
        "formal_pr405_passed": False,
        "formal_blockers": [
            "native D32 reverse-Z is not metric fp16 EXR",
            "render matrices are physical same-launch observations but the fixed-projection formal gate failed",
            "metric scale and E-geo are unresolved",
            f"native depth cadence contains {len(depth_rows)} exact RGB-present pairs rather than PR405 15 fps EXR",
        ],
        "classification": "local_non_trainable_fivepiece",
    }
    (staging / "qc.json").write_text(json.dumps(qc_summary, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    artifact_names = ["video.mp4", "depth.zip", "action_camera.json", "action_camera_30fps.json", "systeminfo.json"]
    artifacts = {name: {"bytes": (staging / name).stat().st_size, "sha256": sha256(staging / name)} for name in artifact_names}
    manifest = {
        "schema": "dm-cp2077-issue735-local-fivepiece/v1",
        "classification": "local_non_trainable_fivepiece",
        "process_launch_id": telemetry["process_launch_id"],
        "process_id": identity[3], "session_id": identity[0], "run_id": identity[1],
        "generation": identity[2],
        "duration_ms": rgb_duration_ms, "manual_stop": manual_stop, "artifacts": artifacts,
        "raw_input": {
            "directory": "raw-input",
            "contract": "raw-input/contract.json",
            "contract_sha256": raw_input["contract_sha256"],
            "events": raw_input["files"]["events"],
            "event_count": raw_input["event_count"],
            "keyboard_event_count": raw_input["keyboard_event_count"],
            "mouse_move_event_count": raw_input["mouse_move_event_count"],
        },
        "qc": {"path": "qc.json", "sha256": sha256(staging / "qc.json")},
        "main_collector_started": True,
        "main_collector_recording_ready": False,
        "sidecar_probe_recording_ready": False,
        "main_collector_formal_start_run": True,
        "sidecar_probe_formal_start_run": False,
        "confirmed_flush": True,
        "recording_ready": False, "deliverable": False, "upload_allowed": False,
    }
    (staging / "manifest.json").write_text(json.dumps(manifest, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    staging.rename(output)
    print(json.dumps({"output": str(output), "manifest": manifest, "qc": qc_summary}, ensure_ascii=False, indent=2))


if __name__ == "__main__":
    main()

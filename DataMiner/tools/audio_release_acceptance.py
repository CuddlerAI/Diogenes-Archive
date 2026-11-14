#!/usr/bin/env python3
"""Evaluate the cross-run process-loopback hard gate from capture-lab reports.

The tool is deliberately evidence-only: it never turns a local run into a
production certification. A release owner still has to attach the generated
verdict to the named target-machine / 22H2 / COD endurance checklist.
"""

from __future__ import annotations

import argparse
import json
import statistics
import sys
from pathlib import Path
from typing import Any

SCHEMA_VERSION = "dm-audio-release-acceptance-1"
MIN_RUNS = 5
EXACT_AUDIO_FORMAT = {
    "sample_rate": 48_000,
    "channels": 2,
    "bits_per_sample": 16,
    "block_align": 4,
    "average_bytes_per_second": 192_000,
}


def _failure(failures: list[str], condition: bool, message: str) -> None:
    if not condition:
        failures.append(message)


def evaluate(reports: list[dict[str, Any]], required_build: int | None = None) -> dict[str, Any]:
    failures: list[str] = []
    _failure(failures, len(reports) >= MIN_RUNS, f"at least {MIN_RUNS} independent runs are required")
    if not reports:
        return {
            "schema_version": SCHEMA_VERSION,
            "passed": False,
            "run_count": 0,
            "failures": failures,
        }

    machine_keys: list[tuple[int, int, int, int, int, int, str]] = []
    offsets: list[float] = []
    residuals: list[tuple[str, float, float, float]] = []
    for index, report in enumerate(reports):
        run_id = str(report.get("run_id", f"run-{index}"))
        windows = report.get("windows") or {}
        adapter = report.get("adapter") or {}
        profile = report.get("profile") or {}
        audio = report.get("audio_calibration") or {}
        health = audio.get("health") or {}
        audio_format = audio.get("format") or {}

        build = windows.get("build")
        machine_keys.append(
            (
                int(windows.get("major", -1)),
                int(build if isinstance(build, int) else -1),
                int(adapter.get("vendor_id", -1)),
                int(adapter.get("device_id", -1)),
                int(adapter.get("luid_high", -1)),
                int(adapter.get("luid_low", -1)),
                str(adapter.get("driver_version", "")),
            )
        )
        _failure(failures, report.get("passed") is True, f"{run_id}: capture-lab did not pass")
        _failure(failures, report.get("integrity_passed") is True, f"{run_id}: video integrity failed")
        _failure(failures, report.get("capture_queue_drops") == 0, f"{run_id}: capture queue dropped frames")
        _failure(failures, audio.get("state") == "ready", f"{run_id}: audio state is not ready")
        _failure(failures, audio.get("source") == "process_loopback", f"{run_id}: source is not process_loopback")
        _failure(failures, audio_format == EXACT_AUDIO_FORMAT, f"{run_id}: PCM format drifted")
        _failure(
            failures,
            audio.get("single_run_timing_passed") is True,
            f"{run_id}: single-run timing gate failed",
        )
        packets = health.get("packets")
        frames = health.get("frames")
        _failure(
            failures,
            isinstance(packets, (int, float)) and packets > 0,
            f"{run_id}: no audio packets",
        )
        _failure(
            failures,
            isinstance(frames, (int, float)) and frames > 0,
            f"{run_id}: no audio frames",
        )
        for field in ("discontinuities", "timestamp_errors", "queue_overflows", "nonzero_device_positions"):
            _failure(failures, health.get(field) == 0, f"{run_id}: audio health {field} is non-zero")
        _failure(failures, health.get("terminal_error") is None, f"{run_id}: audio worker failed")

        fps_num = profile.get("fps_num")
        fps_den = profile.get("fps_den")
        constant = audio.get("constant_offset_us")
        distribution = audio.get("residual_absolute_error_us") or {}
        if not all(isinstance(value, (int, float)) for value in (fps_num, fps_den, constant)):
            failures.append(f"{run_id}: timing/profile fields are incomplete")
            continue
        if fps_num <= 0 or fps_den <= 0:
            failures.append(f"{run_id}: invalid frame rate")
            continue
        offsets.append(float(constant))
        residuals.append(
            (
                run_id,
                1_000_000.0 * float(fps_den) / float(fps_num),
                float(distribution.get("p95", float("inf"))),
                float(distribution.get("max", float("inf"))),
            )
        )

    _failure(failures, len(set(machine_keys)) == 1, "reports must come from one OS/GPU/driver tuple")
    observed_build = machine_keys[0][1]
    if required_build is not None:
        _failure(failures, observed_build == required_build, f"expected Windows build {required_build}, got {observed_build}")

    cohort_offset = statistics.median(offsets) if offsets else None
    max_offset_drift = 0.0
    if cohort_offset is not None:
        for (run_id, frame_us, p95, maximum), offset in zip(residuals, offsets, strict=True):
            drift = abs(offset - cohort_offset)
            max_offset_drift = max(max_offset_drift, drift)
            # Conservative cohort correction: use one median correction for all
            # runs, then charge each run's drift against both residual bounds.
            _failure(
                failures,
                p95 + drift <= frame_us,
                f"{run_id}: cohort-corrected p95 exceeds one video frame",
            )
            _failure(
                failures,
                maximum + drift <= 2.0 * frame_us,
                f"{run_id}: cohort-corrected max exceeds two video frames",
            )

    return {
        "schema_version": SCHEMA_VERSION,
        "passed": not failures,
        "run_count": len(reports),
        "windows_build": observed_build,
        "machine": {
            "windows_major": machine_keys[0][0],
            "adapter_vendor_id": machine_keys[0][2],
            "adapter_device_id": machine_keys[0][3],
            "adapter_luid_high": machine_keys[0][4],
            "adapter_luid_low": machine_keys[0][5],
            "driver_version": machine_keys[0][6],
        },
        "cohort_constant_offset_us": cohort_offset,
        "max_constant_offset_drift_us": max_offset_drift,
        "failures": failures,
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("reports", nargs="+", type=Path, help="capture-lab report.json files")
    parser.add_argument("--required-build", type=int, help="require one exact Windows build")
    parser.add_argument("--out", type=Path, help="write the machine-readable verdict here")
    args = parser.parse_args()
    reports = [json.loads(path.read_text(encoding="utf-8")) for path in args.reports]
    verdict = evaluate(reports, args.required_build)
    rendered = json.dumps(verdict, ensure_ascii=False, indent=2) + "\n"
    if args.out:
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_text(rendered, encoding="utf-8")
    sys.stdout.write(rendered)
    return 0 if verdict["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())

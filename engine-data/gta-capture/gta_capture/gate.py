"""Fail-closed PR #405 evidence gate shared by full and staged real runs."""

from __future__ import annotations

import json
import os
from pathlib import Path

from .audio import audio_rights_evidence_valid, radio_off_evidence_valid
from .evidence import (REAL_CAPTURE_FENCE, REAL_CAPTURE_PROTOCOL,
                       REAL_EDITOR_BINDING_METHOD,
                       REAL_EDITOR_IDENTITY_PROTOCOL,
                       validate_pixel_pose_fence, validate_player_telemetry)
from .errors import ContractError
from .jsonio import load_json, sha256_file
from .policy import CapturePolicy
from .preflight import load_projection_validation
from .trajectory import granted_rules as trajectory_granted_rules


REQUIRED_RULES = {
    "actual_replay_clock", "four_lane_same_replay", "pixel_pose_fence",
    "player_replay_origin", "player_state", "audio_continuity", "audio_rights",
    "projection_calibration",
    "commercial_rights", "trajectory_pr405", "covisibility",
    "cam4_reparameterization", "dynamic_visibility", "native_portrait",
    "motion_limits", "six_translation_honesty", "source_action_semantics",
    "entity_animation_state", "human_review",
}

# RE+ publishes a request-bound seqlock snapshot from GTA's live montage. The
# native controller accepts a one-clip project only when its runtime clip
# identity equals the original Rockstar recording stem captured and hashed
# by the same GTA process. Four distinct export sessions and every frame event
# are revalidated below. Trust boundary: every artifact checked here is local,
# unsigned JSON, so this consistency validation defends against operator error
# and stale/mixed runs, not against deliberate local forgery — that is what the
# native fence plus the unimplemented package-time independent verifier are for.
EDITOR_PROJECT_BINDING_IMPLEMENTED = True

# Commercial audio approval needs a repository-configured trust store and a
# signed reviewer envelope. Plain JSON names/timestamps are not authorization.
TRUSTED_AUDIO_RIGHTS_VERIFIER_IMPLEMENTED = False

# The current ScriptHook samples start at a recording-side boundary. Even when
# their independent JSONL seal, timing grid and cross-lane reuse all validate,
# no in-band Rockstar replay observation proves that boundary is replay frame 0.
# A caller-authored JSON claim must never upgrade this estimate to player_state.
IN_BAND_PLAYER_REPLAY_ORIGIN_CALIBRATION_IMPLEMENTED = False


def _replus_clip_identity(raw_clip_name: str) -> str:
    # GTA's live montage stores CLIP_NAME without the on-disk `.clip`
    # extension. The controller and offline gate must mirror those exact bytes.
    identity_name = (raw_clip_name[:-5]
                     if raw_clip_name.endswith(".clip") else raw_clip_name)
    value = 1469598103934665603
    for byte in identity_name.encode("utf-8")[:64]:
        value ^= byte
        value = (value * 1099511628211) & 0xFFFFFFFFFFFFFFFF
    for _ in range(8):
        value = (value * 1099511628211) & 0xFFFFFFFFFFFFFFFF
    value &= 0x7FFFFFFFFFFFFFFF
    if value == 0:
        value = 1
    return f"{value:016x}"


def _verified_replay_binding(item, expected_replay_sha256: str | None,
                             lane_events: dict[int, list[dict]]) -> bool:
    if not EDITOR_PROJECT_BINDING_IMPLEMENTED:
        return False
    # Without a sealed master replay hash every equality below would degrade to
    # comparing against None; fail closed instead.
    if not isinstance(expected_replay_sha256, str) or len(expected_replay_sha256) != 64:
        return False
    if not isinstance(item, dict):
        return False
    value = item.get("path")
    expected_sha = item.get("sha256")
    if not isinstance(value, str) or not isinstance(expected_sha, str) or len(expected_sha) != 64:
        return False
    path = Path(os.path.expandvars(os.path.expanduser(value)))
    if not path.is_absolute() or not path.is_file() or sha256_file(path) != expected_sha:
        return False
    try:
        evidence = load_json(path)
    except (OSError, UnicodeError, json.JSONDecodeError, ContractError):
        return False
    sessions = evidence.get("lane_sessions") if isinstance(evidence, dict) else None
    if not isinstance(sessions, list) or len(sessions) != 4:
        return False
    source_clip = evidence.get("source_clip")
    source_original_clip = evidence.get("source_original_clip")
    project_artifacts = evidence.get("project_artifacts")
    if (not isinstance(source_clip, dict) or not isinstance(source_original_clip, dict)
            or not isinstance(project_artifacts, list) or len(project_artifacts) != 1):
        return False

    def valid_file_record(record, expected_hash=None) -> bool:
        if not isinstance(record, dict):
            return False
        record_path = record.get("path")
        record_hash = record.get("sha256")
        record_bytes = record.get("bytes")
        if (not isinstance(record_path, str) or not isinstance(record_hash, str)
                or len(record_hash) != 64 or not isinstance(record_bytes, int)
                or isinstance(record_bytes, bool) or record_bytes <= 0):
            return False
        artifact = Path(os.path.expandvars(os.path.expanduser(record_path)))
        return (artifact.is_absolute() and artifact.is_file()
                and artifact.stat().st_size == record_bytes
                and sha256_file(artifact) == record_hash
                and (expected_hash is None or record_hash == expected_hash))

    if not valid_file_record(source_clip, expected_replay_sha256):
        return False
    if not valid_file_record(source_original_clip, expected_replay_sha256):
        return False
    runtime_identity = None
    for artifact in project_artifacts:
        if not valid_file_record(artifact):
            return False
        relative_path = artifact.get("relative_path")
        if not isinstance(relative_path, str) or not relative_path or Path(relative_path).is_absolute():
            return False
        if relative_path.replace("\\", "/") != "evidence/editor-runtime-identity.json":
            return False
        try:
            runtime_identity = load_json(
                Path(os.path.expandvars(os.path.expanduser(artifact["path"]))))
        except (OSError, UnicodeError, json.JSONDecodeError, ContractError):
            return False
    if not isinstance(runtime_identity, dict):
        return False
    fingerprint = project_artifacts[0]["sha256"]
    source_clip_name = evidence.get("source_clip_name")
    source_clip_identity = evidence.get("source_clip_identity")
    project_name = runtime_identity.get("project_name")
    if (not isinstance(source_clip_name, str) or not source_clip_name
            or source_clip_identity != _replus_clip_identity(source_clip_name)
            or not isinstance(project_name, str) or not project_name):
        return False
    original_path = Path(os.path.expandvars(os.path.expanduser(
        source_original_clip["path"])))
    if original_path.name != source_clip_name:
        return False
    session_lanes = {row.get("lane") for row in sessions if isinstance(row, dict)}
    session_ids = {row.get("session_id") for row in sessions if isinstance(row, dict)}
    envelope_ok = (
        evidence.get("schema") == "dm-gta-editor-replay-binding/1"
        and evidence.get("machine_verified") is True
        and evidence.get("verification_method") == "replus-runtime-montage-v1"
        and evidence.get("replay_sha256") == expected_replay_sha256
        and evidence.get("source_clip_sha256") == expected_replay_sha256
        and evidence.get("project_clip_reference_sha256") == expected_replay_sha256
        and evidence.get("editor_project_fingerprint") == fingerprint
        and runtime_identity.get("schema") == "dm-gta-editor-runtime-identity/1"
        and runtime_identity.get("verification_method") == "replus-runtime-montage-v1"
        and runtime_identity.get("project_name") == project_name
        and runtime_identity.get("clip_count") == 1
        and runtime_identity.get("clip_index") == 0
        and runtime_identity.get("source_clip_name") == source_clip_name
        and runtime_identity.get("source_clip_identity") == source_clip_identity
        and runtime_identity.get("source_clip_identity_algorithm") ==
            "replus-fnv1a63-raw-stem-occurrence-v1"
        and runtime_identity.get("replay_sha256") == expected_replay_sha256
        and session_lanes == {1, 2, 3, 4}
        and len(session_ids) == 4
        and None not in session_ids
        and all(row.get("replay_sha256") == expected_replay_sha256
                for row in sessions if isinstance(row, dict))
        and all(row.get("editor_project_fingerprint") == fingerprint
                for row in sessions if isinstance(row, dict))
        and all(row.get("project_name") == project_name
                and row.get("clip_identity") == source_clip_identity
                for row in sessions if isinstance(row, dict))
    )
    if not envelope_ok:
        return False
    sessions_by_lane = {row["lane"]: row for row in sessions}
    if set(lane_events) != {1, 2, 3, 4}:
        return False
    request_ids = set()
    identity_sequences = set()
    final_camera_generations = set()
    previous_global_generation = None
    for lane in range(1, 5):
        rows = lane_events.get(lane)
        session = sessions_by_lane[lane]
        if not isinstance(rows, list) or not rows:
            return False
        previous_request = previous_sequence = previous_heartbeat = None
        for row in rows:
            binding = row.get("editor_binding") if isinstance(row, dict) else None
            if not isinstance(binding, dict):
                return False
            request_id = binding.get("capture_request_id")
            sequence = binding.get("identity_sequence")
            heartbeat = binding.get("identity_heartbeat")
            final_generation = binding.get("final_camera_generation")
            pixel_generation = row.get("pixel_ready_final_camera_generation")
            if (binding.get("method") != REAL_EDITOR_BINDING_METHOD
                    or binding.get("session_id") != session.get("session_id")
                    or binding.get("project_name") != project_name
                    or binding.get("clip_count") != 1
                    or binding.get("clip_index") != 0
                    or binding.get("clip_identity") != source_clip_identity
                    or not isinstance(request_id, int) or isinstance(request_id, bool)
                    or request_id <= 0
                    or not isinstance(sequence, int) or isinstance(sequence, bool)
                    or sequence <= 0 or sequence % 2 != 0
                    or not isinstance(heartbeat, int) or isinstance(heartbeat, bool)
                    or heartbeat <= 0
                    or not isinstance(final_generation, int)
                    or isinstance(final_generation, bool) or final_generation <= 0
                    or not isinstance(pixel_generation, int)
                    or isinstance(pixel_generation, bool) or pixel_generation <= 0
                    or pixel_generation != final_generation
                    or row.get("capture_protocol") != REAL_CAPTURE_PROTOCOL
                    or row.get("editor_identity_protocol") !=
                    REAL_EDITOR_IDENTITY_PROTOCOL
                    or row.get("capture_fence") != REAL_CAPTURE_FENCE
                    or row.get("replay_sha256") != expected_replay_sha256):
                return False
            if (request_id in request_ids or sequence in identity_sequences
                    or final_generation in final_camera_generations):
                return False
            if (previous_request is not None and request_id <= previous_request
                    or previous_sequence is not None and sequence <= previous_sequence
                    or previous_heartbeat is not None and heartbeat <= previous_heartbeat
                    or previous_global_generation is not None
                    and final_generation != previous_global_generation + 1):
                return False
            request_ids.add(request_id)
            identity_sequences.add(sequence)
            final_camera_generations.add(final_generation)
            previous_request = request_id
            previous_sequence = sequence
            previous_heartbeat = heartbeat
            previous_global_generation = final_generation
    return True


def build_pr405_gate(*, alignment: dict, lanes: dict[int, list[dict]], audio_report: dict,
                     calibration: dict, policy: CapturePolicy, dry_run: bool,
                     replay_binding: dict | None = None,
                     expected_render_profile_sha256: str | None = None,
                     expected_output_root: str | None = None,
                     trajectory_verdict: dict | None = None) -> dict:
    passed = set()
    # A missing master replay hash must never let hash-equality checks below
    # degrade into None == None.
    replay_sha256 = alignment.get("replay_sha256")
    if not isinstance(replay_sha256, str) or len(replay_sha256) != 64:
        replay_sha256 = None
    if alignment.get("ok") is True and alignment.get("logical_interlane_delta_ns_max") is not None:
        passed.add("actual_replay_clock")
    if (alignment.get("ok") is True
            and _verified_replay_binding(replay_binding, replay_sha256, lanes)):
        passed.add("four_lane_same_replay")
    if (not dry_run and not validate_pixel_pose_fence(
            lanes, replay_sha256, expected_render_profile_sha256,
            expected_output_root)):
        passed.add("pixel_pose_fence")
    player_telemetry_problems = (
        validate_player_telemetry(lanes, replay_sha256) if not dry_run
        else ["real player telemetry is unavailable in dry-run mode"]
    )
    player_telemetry_diagnostic_ok = not player_telemetry_problems
    # Deliberately no path grants player_replay_origin/player_state yet. The
    # implementation flag documents the missing verifier; toggling it alone is
    # insufficient because there is no accepted artifact parser below.
    player_replay_origin_verified = False
    pending_reasons = []
    if not player_telemetry_diagnostic_ok:
        pending_reasons.append("player_telemetry_invalid_or_unavailable")
    if not player_replay_origin_verified:
        pending_reasons.append("player_replay_origin_unverified")
    audio_quality = (
        audio_report.get("schema") == "dm-gta-audio-report/2"
        and audio_report.get("duration_integrity") is True
        and audio_report.get("active_coverage") is True
        and audio_report.get("max_silence_ok") is True
        and audio_report.get("clipping_ok") is True
        and audio_report.get("continuous") is True
        and audio_report.get("non_silent") is True
        and replay_sha256 is not None
        and audio_report.get("replay_sha256") == replay_sha256
    )
    if audio_quality:
        passed.add("audio_continuity")
    radio_evidence = audio_report.get("radio_off_evidence")
    rights_evidence = audio_report.get("rights_evidence")
    if (audio_report.get("radio_off") is True
            and audio_report.get("rights_checked") is True
            and replay_sha256 is not None
            and radio_off_evidence_valid(
                radio_evidence,
                expected_replay_sha256=replay_sha256,
                expected_duration_s=audio_report.get("duration_expected_s"))
            and TRUSTED_AUDIO_RIGHTS_VERIFIER_IMPLEMENTED
            and audio_rights_evidence_valid(
                rights_evidence,
                expected_replay_sha256=replay_sha256,
                expected_audio_sha256=audio_report.get("audio_sha256"),
                expected_use=policy.purpose)):
        passed.add("audio_rights")
    projection = load_projection_validation(calibration)
    if (projection is not None
            and projection.calibration_id == calibration.get("calibration_id")
            and projection.filmback_id == calibration.get("filmback_id")
            and projection.reprojection_error_px <= projection.reprojection_threshold_px
            and projection.sample_count >= projection.minimum_sample_count):
        passed.add("projection_calibration")
    if policy.commercial_rights_approved:
        passed.add("commercial_rights")
    # The trajectory family is granted only by engine_qc PASS on the materialized
    # clip (see trajectory.py). A dry run has no GTA pixels and therefore no
    # trajectory evidence, whatever a stale verdict file next to it claims.
    if not dry_run:
        passed |= trajectory_granted_rules(trajectory_verdict) & REQUIRED_RULES
    missing = sorted(REQUIRED_RULES - passed)
    return {
        "schema": "dm-gta-pr405-gate/1",
        "ok": not missing,
        "passed_rules": sorted(passed),
        "missing_rules": missing,
        "internal_poc_usable": not dry_run and not ({
            "actual_replay_clock", "four_lane_same_replay", "pixel_pose_fence",
            "player_replay_origin", "player_state", "audio_continuity",
            "projection_calibration",
        } - passed),
        "delivery_eligible": not missing and policy.delivery_eligible,
        "dry_run": dry_run,
        "trajectory_verdict": trajectory_verdict,
        "replay_binding": replay_binding,
        "player_evidence": {
            "observation_semantics": "recording_side_estimate",
            "telemetry_diagnostic_ok": player_telemetry_diagnostic_ok,
            "telemetry_problems": player_telemetry_problems,
            "in_band_origin_calibration_implemented":
                IN_BAND_PLAYER_REPLAY_ORIGIN_CALIBRATION_IMPLEMENTED,
            "replay_origin_verified": player_replay_origin_verified,
        },
        "pending_reasons": sorted(pending_reasons),
    }

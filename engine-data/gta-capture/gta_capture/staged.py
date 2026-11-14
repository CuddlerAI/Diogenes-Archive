"""Finalize the operator-gated Story Mode smoke sequence into one auditable POC run."""

from __future__ import annotations

import math
import os
import uuid
from contextlib import contextmanager
from pathlib import Path

from .audio import validate_wav
from .errors import ContractError, StateError
from .evidence import materialize_metadata, validate_alignment
from .gate import build_pr405_gate
from .jsonio import (file_record, load_json, sha256_file, utc_now,
                     write_json_atomic)
from .media import materialize_videos
from .native_tape import write_native_tape
from .policy import repository_profile_non_trainable
from .preflight import (DependencyManifest, load_render_profile_binding,
                        PreflightReport, run_preflight)
from .scenario import Scenario, validate_native_subject_route
from .recorded_route import (RECORDED_ROUTE_KIND, RECORDING_ANCHOR_TOLERANCE_M, native_command,
                             validate_recorded_subject_route)
from .qc_bridge import invoke_engine_qc, stage_qc_sidecars
from .tape import GTA_COORDINATE_CONVENTION, load_tape
from .trajectory import verdict_for_run


_INTERNAL_POC_CALIBRATION_FAILURES = frozenset({
    "calibration.binding",
    "calibration.projection_evidence",
    "calibration.projection_contract",
})


def _snapshot(path: Path, value: dict) -> None:
    if path.is_file():
        if load_json(path) != value:
            raise StateError(f"staged POC input differs from immutable snapshot: {path}")
    else:
        write_json_atomic(path, value)


def _sealed(path_value, hash_value, label: str) -> Path:
    if not isinstance(path_value, str) or not isinstance(hash_value, str):
        raise StateError(f"{label} path/hash is missing")
    path = Path(path_value)
    if not path.is_file() or sha256_file(path) != hash_value:
        raise StateError(f"{label} is missing or differs from its sealed sha256")
    return path


def _revalidate_sealed_audio(audio: dict, sealed_report: dict, *,
                             duration_s: float, replay_sha256: str,
                             evidence_dir: Path | None = None) -> dict:
    if not isinstance(audio, dict) or not isinstance(sealed_report, dict):
        raise ContractError("sealed audio artifact/report must be JSON objects")
    audio_path = _sealed(audio.get("audio_path"), audio.get("audio_sha256"),
                         "master audio")
    if (Path(str(sealed_report.get("audio_path", ""))).resolve() != audio_path.resolve()
            or sealed_report.get("audio_sha256") != audio.get("audio_sha256")):
        raise ContractError("audio report is not bound to the sealed audio artifact")
    if sealed_report.get("replay_sha256") != replay_sha256:
        raise ContractError("audio report is not bound to the sealed master replay")

    # Recompute all machine-measured quality fields from the immutable WAV and
    # the sealed scenario duration. Never trust cached continuous/non_silent or
    # format declarations from the native result/report JSON.
    report = validate_wav(audio_path, expected_duration_s=duration_s)
    report["replay_sha256"] = replay_sha256
    for key in ("radio_off", "radio_off_evidence", "rights_checked", "rights_evidence"):
        report[key] = sealed_report.get(key)
    if report["continuous"] is not True or report["non_silent"] is not True:
        raise ContractError(
            "sealed audio failed duration/format/continuity/non-silence revalidation")
    audio["audio_bytes"] = report["audio_bytes"]
    if evidence_dir is not None:
        write_json_atomic(evidence_dir / "audio-report-revalidated.json", report)
    return report


@contextmanager
def _run_lock(root: Path):
    root.mkdir(parents=True, exist_ok=True)
    lock = root / ".finalize.lock"
    try:
        fd = os.open(lock, os.O_CREAT | os.O_EXCL | os.O_WRONLY)
    except FileExistsError as exc:
        try:
            holder = lock.read_text(encoding="ascii", errors="replace").strip()
        except OSError:
            holder = "unreadable"
        raise StateError(
            f"staged POC is already being finalized: {lock} ({holder}); if that "
            "process is confirmed dead, delete the lock file and retry") from exc
    try:
        os.write(fd, f"pid={os.getpid()} at={utc_now()}\n".encode("ascii"))
        os.close(fd)
        yield
    finally:
        try:
            os.close(fd)
        except OSError:
            pass
        lock.unlink(missing_ok=True)


def _preflight_materialization_decision(
        manifest: DependencyManifest, preflight: PreflightReport, *,
        allow_internal_calibration_gaps: bool) -> dict:
    failures = sorted(item.check_id for item in preflight.checks
                      if item.status == "FAIL")
    if not failures:
        return {
            "used": False,
            "preflight_ok": True,
            "retained_failures": [],
        }
    policy = manifest.policy
    safe_internal_boundary = (
        policy.purpose == "internal_nontrainable_poc"
        and policy.story_mode_only
        and policy.online_forbidden
        and policy.non_trainable
        and not policy.upload_enabled
        and repository_profile_non_trainable()
    )
    if (not allow_internal_calibration_gaps or not safe_internal_boundary
            or not set(failures) <= _INTERNAL_POC_CALIBRATION_FAILURES):
        raise ContractError("real preflight failed; inspect evidence/preflight.json")
    return {
        "used": True,
        "preflight_ok": False,
        "scope": "local-non-trainable-evidence-materialization-only",
        "retained_failures": failures,
        "delivery_eligible": False,
        "upload_enabled": False,
    }


def _write_preflight(root: Path, manifest: DependencyManifest,
                     preflight: PreflightReport, *,
                     allow_internal_calibration_gaps: bool) -> dict:
    decision = _preflight_materialization_decision(
        manifest, preflight,
        allow_internal_calibration_gaps=allow_internal_calibration_gaps)
    report = preflight.to_dict()
    report["internal_artifact_materialization"] = decision
    write_json_atomic(root / "evidence" / "preflight.json", report)
    return decision


def _validate_placed_subject_plan(source: dict, route: dict) -> None:
    """Reopen the sealed trajgen anchors and independently check their placement.

    A straight-looking route inserted into a curved camera plan is still a false
    binding. Verify the route's source file and every rigidly transformed anchor,
    including aim height, before accepting its native-task declaration.
    """
    source_path = source.get("path")
    files = source.get("files")
    if not isinstance(source_path, str) or not isinstance(files, list):
        raise ContractError("placed subject route is missing its source plan binding")
    records = [item for item in files
               if isinstance(item, dict) and item.get("name") == "plan.json"]
    if len(records) != 1 or route.get("source") != records[0]:
        raise ContractError("placed subject route is not bound to the camera source plan")
    plan_path = Path(source_path) / "plan.json"
    if not plan_path.is_file() or sha256_file(plan_path) != records[0].get("sha256"):
        raise ContractError("placed subject source plan differs from its sealed sha256")
    plan = load_json(plan_path)
    if not isinstance(plan, dict) or plan.get("duration_s") != route["duration_s"]:
        raise ContractError("placed subject source plan duration differs from the route")
    anchors = plan.get("anchors")
    waypoints = route["waypoints"]
    if not isinstance(anchors, list) or len(anchors) != len(waypoints):
        raise ContractError("placed subject source plan has a different anchor timeline")
    placement = source.get("placement")
    if (not isinstance(placement, dict) or route.get("placement") != placement
            or placement.get("anchor") != source["anchor"]):
        raise ContractError("placed subject route and camera tape have different placements")
    heading = placement.get("heading_deg")
    ground = placement.get("ground_y")
    for item in (heading, ground):
        if (not isinstance(item, (int, float)) or isinstance(item, bool)
                or not math.isfinite(float(item))):
            raise ContractError("placed subject route has a malformed placement")
    if not 0 <= float(heading) < 360:
        raise ContractError("placed subject route has an invalid placement heading")
    cos_h, sin_h = math.cos(math.radians(heading)), math.sin(math.radians(heading))
    origin = None
    for frame, (authored, placed) in enumerate(zip(anchors, waypoints)):
        if not isinstance(authored, dict) or authored.get("t") != placed["t"]:
            raise ContractError(f"placed subject source anchor {frame} has a different timeline")
        point = authored.get("p")
        if (not isinstance(point, list) or len(point) != 3
                or any(not isinstance(value, (int, float)) or isinstance(value, bool)
                       or not math.isfinite(float(value)) for value in point)):
            raise ContractError(f"placed subject source anchor {frame} is malformed")
        if origin is None:
            origin = point
        x, y = float(point[0]) - float(origin[0]), float(point[2]) - float(origin[2])
        recorded = route.get("route_kind") == RECORDED_ROUTE_KIND
        expected = [source["anchor"][0] + x * cos_h - y * sin_h,
                    source["anchor"][1] + x * sin_h + y * cos_h,
                    (source["anchor"][2] + point[1] - ground - placed["look_at_height_m"]
                     if recorded else source["anchor"][2])]
        if (math.dist(placed["position"], expected) > 1e-5
                or (not recorded and
                    abs(float(placed["look_at_height_m"]) - (point[1] - ground)) > 1e-5)):
            raise ContractError(
                f"placed subject route waypoint {frame} differs from its authored source anchor")


def _validate_scenario_tape_route(scenario: Scenario, tape: dict, *,
                                 expected_replay_sha256=None) -> None:
    if tape.get("coordinate_convention") != GTA_COORDINATE_CONVENTION:
        raise ContractError("staged real POC requires a GTA-native camera tape")
    location = [float(value) for value in scenario.raw["location"]]
    source = tape.get("source")
    if not isinstance(source, dict):
        raise ContractError("staged real POC requires a camera tape source")
    anchor = source.get("anchor")
    if (not isinstance(anchor, list) or len(anchor) != 3
            or any(not isinstance(value, (int, float)) or isinstance(value, bool)
                   or not math.isfinite(float(value)) for value in anchor)
            or location == [0.0, 0.0, 0.0]):
        raise ContractError(
            "real staged POC requires an explicit scenario location and finite tape anchor")
    player = scenario.raw["player"]
    route = source.get("subject_route")
    recorded = isinstance(route, dict) and route.get("route_kind") == RECORDED_ROUTE_KIND
    if recorded:
        if (source.get("kind") != "trajgen_clip_placed"
                or source.get("native_command") != native_command(player)
                or any(isinstance(source["native_command"].get(key), bool) for key in
                       ("distance_m", "speed_mps", "heading_deg"))
                or any("route_" + key in source for key in ("distance_m", "speed_mps", "heading_deg"))):
            raise ContractError("recorded route must keep native command parameters separate from motion")
        validate_recorded_subject_route(
            route, scenario=scenario, expected_replay_sha256=expected_replay_sha256)
        # The scenario bytes/command are bound by recorded_route, while camera
        # placement is bound to the observed entity position after settling.
        # Requiring the legacy 1mm authored-anchor equality here would reject
        # recordings that already passed the unchanged 0.25m start boundary.
        # Keep the observed world coordinates intact; never translate them back
        # to the requested scenario location to satisfy a configuration check.
        first_observed = route["waypoints"][0]["position"]
        if math.dist(anchor, first_observed) > 1e-5:
            raise ContractError("recorded camera placement differs from the observed first position")
        if math.dist(anchor, location) > RECORDING_ANCHOR_TOLERANCE_M:
            raise ContractError("recorded camera placement exceeds the scenario's 0.25m start boundary")
    else:
        if max(abs(location[i] - float(anchor[i])) for i in range(3)) > 0.001:
            raise ContractError(
                "real staged POC requires one explicit scenario location matching the tape anchor")
        for key in ("distance_m", "speed_mps", "heading_deg"):
            item = source.get("route_" + key)
            if (not isinstance(item, (int, float)) or isinstance(item, bool)
                    or not math.isfinite(float(item)) or float(item) != float(player[key])):
                raise ContractError("scenario player route differs from the camera tape route")
    if source.get("kind") == "trajgen_clip_placed":
        if recorded:
            if tape.get("n_frames") != len(route["waypoints"]):
                raise ContractError("recorded route frame count differs from the camera tape")
        else:
            validate_native_subject_route(
                route, player=player, anchor=anchor, duration_s=scenario.duration_s,
                n_frames=tape.get("n_frames"))
        _validate_placed_subject_plan(source, route)


def _validate_real_inputs(manifest: DependencyManifest, scenario: Scenario,
                          tape: dict, root: Path, *,
                          allow_internal_calibration_gaps: bool) -> None:
    route = tape.get("source", {}).get("subject_route", {})
    expected_replay = None
    if isinstance(route, dict) and route.get("route_kind") == RECORDED_ROUTE_KIND:
        record_path = root / "evidence" / "native-record-result.json"
        bound = route.get("observation", {}).get("files", {}).get("record_result", {})
        if not record_path.is_file() or sha256_file(record_path) != bound.get("sha256"):
            raise ContractError("recorded camera route is bound to a different run's master recording")
        expected_replay = load_json(record_path).get("master", {}).get("replay_sha256")
    _validate_scenario_tape_route(scenario, tape, expected_replay_sha256=expected_replay)
    preflight = run_preflight(manifest, mode="real")
    _write_preflight(
        root, manifest, preflight,
        allow_internal_calibration_gaps=allow_internal_calibration_gaps)


def _finalize_staged_poc(*, manifest: DependencyManifest, scenario: Scenario,
                         tape: dict, run_dir,
                         allow_internal_calibration_gaps: bool = False) -> dict:
    root = Path(run_dir).resolve()
    if (root / "run-manifest.json").exists():
        raise StateError("staged POC is already finalized; the run manifest is immutable")
    # Reject bad mutable inputs before publishing any immutable config snapshot.
    _validate_real_inputs(
        manifest, scenario, tape, root,
        allow_internal_calibration_gaps=allow_internal_calibration_gaps)
    config = root / "config"
    _snapshot(config / "dependency-manifest.json", manifest.raw)
    _snapshot(config / "scenario.json", scenario.raw)
    _snapshot(config / "camera-tape.json", tape)
    write_native_tape(config / "camera-tape.bin", tape,
                      json_path=config / "camera-tape.json")
    sealed_manifest = DependencyManifest.load(config / "dependency-manifest.json")
    sealed_scenario = Scenario.load(config / "scenario.json")
    sealed_tape = load_tape(config / "camera-tape.json",
                            expected_duration_s=sealed_scenario.duration_s)
    location = [float(value) for value in sealed_scenario.raw["location"]]
    _validate_scenario_tape_route(sealed_scenario, sealed_tape)

    preflight = run_preflight(sealed_manifest, mode="real")
    preflight_decision = _write_preflight(
        root, sealed_manifest, preflight,
        allow_internal_calibration_gaps=allow_internal_calibration_gaps)

    record = load_json(root / "evidence" / "native-record-result.json")
    master = record.get("master", {})
    _sealed(master.get("replay_path"), master.get("replay_sha256"), "master replay")
    _validate_scenario_tape_route(
        sealed_scenario, sealed_tape, expected_replay_sha256=master["replay_sha256"])
    record_start = master.get("record_start_position")
    if (not isinstance(record_start, list) or len(record_start) != 3
            or max(abs(float(record_start[i]) - location[i]) for i in range(3)) > 0.25):
        raise ContractError("recorded Story player did not start at the bound scenario anchor")
    lanes = {}
    replay_binding = None
    for lane in range(1, 5):
        result = load_json(root / "evidence" / f"native-lane-{lane}-result.json")
        artifact = result.get("lane", {})
        if artifact.get("lane") != lane:
            raise StateError(f"lane {lane} staged result has the wrong lane id")
        _sealed(artifact.get("frame_events_path"), artifact.get("frame_events_sha256"),
                f"lane {lane} frame evidence")
        lanes[lane] = artifact
        if lane == 4:
            replay_binding = artifact.get("editor_project_binding")
    if not isinstance(replay_binding, dict):
        raise ContractError("lane 4 did not seal the RE+ runtime editor/replay binding")
    _sealed(replay_binding.get("path"), replay_binding.get("sha256"),
            "runtime editor/replay binding")
    master["editor_project_binding"] = replay_binding
    audio_result = load_json(root / "evidence" / "native-audio-result.json")
    audio = audio_result.get("audio", {})
    _sealed(audio.get("audio_report_path"), audio.get("audio_report_sha256"),
            "audio report")
    sealed_audio_report = load_json(audio["audio_report_path"])
    audio_report = _revalidate_sealed_audio(
        audio, sealed_audio_report, duration_s=sealed_scenario.duration_s,
        replay_sha256=master["replay_sha256"], evidence_dir=root / "evidence")

    event_paths = {lane: lanes[lane]["frame_events_path"] for lane in range(1, 5)}
    render_binding = load_render_profile_binding(sealed_manifest)
    alignment, events = validate_alignment(
        event_paths, sealed_tape, allow_mock=False,
        expected_replay_sha256=master["replay_sha256"],
        expected_render_profile_sha256=render_binding.render_profile_sha256,
        expected_output_root=render_binding.expected_output_root)
    write_json_atomic(root / "evidence" / "alignment-report.json", alignment.to_dict())
    if not alignment.ok:
        raise ContractError("staged four-lane alignment failed")

    calibration = sealed_manifest.raw["calibration"]
    capture = root / "capture"
    metadata = materialize_metadata(
        capture, events, sealed_tape, source_epoch=master["source_epoch"],
        process_name=sealed_manifest.raw["game"].get("process_name", "GTA5.exe"),
        metric_scale=float(calibration["metric_scale"]), dry_run=False)
    write_json_atomic(root / "evidence" / "metadata-report.json", metadata)
    videos = materialize_videos(
        capture, lanes, audio, ffmpeg=sealed_manifest.pinned_tool_path("ffmpeg"),
        lane_events=events, ffprobe=sealed_manifest.pinned_tool_path("ffprobe"),
        fps=int(sealed_manifest.raw["render"]["fps"]))

    # The operator-gated staged path is the only one that runs today (the full
    # backend still hard-fails on adapter_ready=false), so the trajectory verdict
    # has to be produced here too or the trajectory rules stay missing on exactly
    # the path the capture machine uses.
    trajectory_verdict = verdict_for_run(
        root, sealed_tape, invoke=invoke_engine_qc, stage=stage_qc_sidecars)
    if trajectory_verdict is not None:
        write_json_atomic(root / "evidence" / "trajectory-verdict.json",
                          trajectory_verdict)
    gate = build_pr405_gate(
        alignment=alignment.to_dict(), lanes=events, audio_report=audio_report,
        calibration=calibration, policy=sealed_manifest.policy, dry_run=False,
        replay_binding=master.get("editor_project_binding"),
        expected_render_profile_sha256=render_binding.render_profile_sha256,
        expected_output_root=render_binding.expected_output_root,
        trajectory_verdict=trajectory_verdict)
    write_json_atomic(root / "evidence" / "gta-pr405-gate.json", gate)
    now = utc_now()
    run_manifest = {
        "schema": "dm-gta-run-manifest/1",
        "run_id": str(uuid.uuid4()),
        "created_at": now,
        "completed_at": now,
        "mode": "real-staged-operator-gated",
        "backend": "gta-asi-pipe-staged",
        "dry_run": False,
        "delivery_eligible": (gate["delivery_eligible"]
                              and not repository_profile_non_trainable()),
        "timestamp_semantics": "source_replay_timeline",
        "render_mode": "sequential_deterministic_replay",
        "wallclock_simultaneous": False,
        "preflight": preflight_decision,
        "inputs": {
            "dependency_manifest": file_record(config / "dependency-manifest.json"),
            "scenario": file_record(config / "scenario.json"),
            "camera_tape": file_record(config / "camera-tape.json"),
            "camera_tape_native": file_record(config / "camera-tape.bin"),
        },
        "master": master,
        "lane_artifacts": {str(key): value for key, value in lanes.items()},
        "audio": audio,
        "videos": [file_record(path) for path in videos],
        "metadata_files": [file_record(path) for path in metadata["files"]],
        "delivery_files": [
            file_record(capture / f"{stem}_{lane}.{suffix}")
            for lane in range(1, 5)
            for stem, suffix in (("video", "mp4"),
                                 ("systeminfo", "json"),
                                 ("action_camera", "json"))
        ],
        "alignment": alignment.to_dict(),
        "gta_pr405_gate": gate,
        "capture_dir": str(capture.resolve()),
        "next_gate": ("run engine_qc and independent human review" if gate["internal_poc_usable"]
                      else "resolve gta-pr405-gate missing rules"),
    }
    write_json_atomic(root / "run-manifest.json", run_manifest)
    return run_manifest


def finalize_staged_poc(*, manifest: DependencyManifest, scenario: Scenario,
                        tape: dict, run_dir,
                        allow_internal_calibration_gaps: bool = False) -> dict:
    root = Path(run_dir).resolve()
    with _run_lock(root):
        return _finalize_staged_poc(
            manifest=manifest, scenario=scenario, tape=tape, run_dir=root,
            allow_internal_calibration_gaps=allow_internal_calibration_gaps)

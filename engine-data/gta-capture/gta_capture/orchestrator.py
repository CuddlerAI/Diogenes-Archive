"""Crash-resumable capture state machine."""

from __future__ import annotations

import os
import traceback
import uuid
from contextlib import contextmanager
from pathlib import Path

from .backend import CaptureBackend
from .errors import ContractError, StateError
from .evidence import materialize_metadata, validate_alignment
from .gate import build_pr405_gate
from .jsonio import (file_record, load_json, read_jsonl, sha256_file, utc_now,
                     write_json_atomic)
from .media import materialize_videos
from .native_tape import write_native_tape
from .policy import repository_profile_non_trainable
from .preflight import (DependencyManifest, load_render_profile_binding,
                        run_preflight)
from .qc_bridge import invoke_engine_qc, stage_qc_sidecars
from .scenario import Scenario
from .tape import GTA_COORDINATE_CONVENTION, load_tape, validate_tape
from .trajectory import verdict_for_run


STATE_SCHEMA = "dm-gta-run-state/1"
RUN_SCHEMA = "dm-gta-run-manifest/1"


def _trajectory_verdict(root: Path, tape: dict, *, dry_run: bool) -> dict | None:
    """Judge the materialized clip's trajectory contract with engine_qc.

    Runs before the gate is written because the gate is sealed against the run
    manifest afterwards; re-deciding rules later would be exactly the post-hoc
    ``passed_rules`` edit the bridge rejects.  A dry run has no GTA pixels, so it
    never produces a verdict.
    """
    if dry_run:
        return None
    return verdict_for_run(root, tape, invoke=invoke_engine_qc, stage=stage_qc_sidecars)


def _new_state() -> dict:
    return {
        "schema": STATE_SCHEMA,
        "run_id": str(uuid.uuid4()),
        "status": "CREATED",
        "completed": [],
        "artifacts": {},
        "history": [{"at": utc_now(), "status": "CREATED"}],
        "error": None,
    }


def _load_state(path: Path) -> dict:
    import json
    value = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(value, dict) or value.get("schema") != STATE_SCHEMA:
        raise StateError(f"invalid run state: {path}")
    return value


def _mark(state_path: Path, state: dict, step: str, *, artifacts: dict | None = None) -> None:
    if step not in state["completed"]:
        state["completed"].append(step)
    if artifacts:
        state["artifacts"].update(artifacts)
    state["status"] = step
    state["error"] = None
    state["history"].append({"at": utc_now(), "status": step})
    write_json_atomic(state_path, state)


@contextmanager
def _run_lock(run_dir: Path):
    lock = run_dir / ".capture.lock"
    try:
        fd = os.open(lock, os.O_CREAT | os.O_EXCL | os.O_WRONLY)
    except FileExistsError as exc:
        try:
            holder = lock.read_text(encoding="ascii", errors="replace").strip()
        except OSError:
            holder = "unreadable"
        raise StateError(
            f"run is already locked: {lock} ({holder}); if that process is "
            "confirmed dead, delete the lock file and retry") from exc
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


def _snapshot_inputs(run_dir: Path, manifest: DependencyManifest, scenario: Scenario, tape: dict) -> None:
    config = run_dir / "config"
    config.mkdir(parents=True, exist_ok=True)
    snapshots = {
        "dependency-manifest.json": manifest.raw,
        "scenario.json": scenario.raw,
        "camera-tape.json": tape,
    }
    for name, value in snapshots.items():
        path = config / name
        if path.is_file():
            if load_json(path) != value:
                raise StateError(f"resume input differs from immutable snapshot: {path}")
        else:
            write_json_atomic(path, value)
    write_native_tape(config / "camera-tape.bin", tape,
                      json_path=config / "camera-tape.json")


def _require_sealed_file(path_value, hash_value, label: str) -> Path:
    if not isinstance(path_value, str) or not path_value:
        raise StateError(f"persisted {label} path is missing; start a new run")
    path = Path(path_value)
    if not path.is_file():
        raise StateError(f"persisted {label} is missing; start a new run")
    if not isinstance(hash_value, str) or len(hash_value) != 64:
        raise StateError(f"persisted {label} has no sealed sha256; start a new run")
    if sha256_file(path) != hash_value:
        raise StateError(f"persisted {label} sha256 changed; start a new run")
    return path


def _require_file_record(record, label: str) -> Path:
    if not isinstance(record, dict):
        raise StateError(f"persisted {label} has no sealed file record; start a new run")
    path = _require_sealed_file(record.get("path"), record.get("sha256"), label)
    expected_bytes = record.get("bytes")
    if (not isinstance(expected_bytes, int) or isinstance(expected_bytes, bool)
            or path.stat().st_size != expected_bytes):
        raise StateError(f"persisted {label} byte length changed; start a new run")
    return path


def _validate_persisted_artifacts(state: dict, *, allow_mock_audio: bool) -> None:
    completed = set(state.get("completed", []))
    artifacts = state.get("artifacts", {})
    if "MASTER_RECORDED" in completed:
        master = artifacts.get("master", {})
        _require_sealed_file(master.get("replay_path"), master.get("replay_sha256"),
                             "master replay")
    lanes = artifacts.get("lanes", {})
    for lane in range(1, 5):
        if f"LANE_{lane}_EXPORTED" in completed:
            item = lanes.get(str(lane), {})
            _require_sealed_file(item.get("frame_events_path"),
                                 item.get("frame_events_sha256"),
                                 f"lane {lane} frame evidence")
            if not allow_mock_audio:
                for frame, row in enumerate(read_jsonl(item["frame_events_path"])):
                    output = _require_sealed_file(
                        row.get("output_path"), row.get("output_sha256"),
                        f"lane {lane} frame {frame} RGB")
                    expected_bytes = row.get("output_bytes")
                    if not isinstance(expected_bytes, int) or output.stat().st_size != expected_bytes:
                        raise StateError(
                            f"persisted lane {lane} frame {frame} RGB size changed; start a new run")
    if "AUDIO_CAPTURED" in completed:
        audio = artifacts.get("audio", {})
        report_path = _require_sealed_file(audio.get("audio_report_path"),
                                           audio.get("audio_report_sha256"),
                                           "audio report")
        if not allow_mock_audio:
            audio_path = _require_sealed_file(
                audio.get("audio_path"), audio.get("audio_sha256"), "master audio")
            report = load_json(report_path)
            if (Path(str(report.get("audio_path", ""))).resolve() != audio_path.resolve()
                    or report.get("audio_sha256") != audio.get("audio_sha256")):
                raise StateError("persisted audio report is not bound to master audio")
    if "COMPLETE" in completed:
        manifest_path = _require_file_record(artifacts.get("run_manifest"), "run manifest")
        manifest = load_json(manifest_path)
        if manifest.get("schema") != RUN_SCHEMA or manifest.get("run_id") != state.get("run_id"):
            raise StateError("persisted run manifest belongs to another run")
        for name, record in manifest.get("inputs", {}).items():
            _require_file_record(record, f"run input {name}")
        for collection in ("metadata_files", "videos", "delivery_files"):
            records = manifest.get(collection)
            if not isinstance(records, list):
                raise StateError(f"persisted run manifest has no {collection} records")
            for index, record in enumerate(records):
                _require_file_record(record, f"{collection} {index}")


def run_capture(*, manifest: DependencyManifest, scenario: Scenario, tape: dict,
                run_dir, backend: CaptureBackend, mode: str, resume: bool = False) -> dict:
    if mode not in ("real", "dry-run"):
        raise ContractError(f"unknown capture mode {mode!r}; use 'real' or 'dry-run'")
    validate_tape(tape, expected_duration_s=scenario.duration_s)
    if mode == "real" and tape.get("coordinate_convention") != GTA_COORDINATE_CONVENTION:
        raise ContractError(
            "real mode requires an explicitly calibrated GTA-native coordinate convention")
    if mode == "real" and backend.is_mock:
        raise ContractError("real mode cannot use the dry-run backend")
    if mode == "dry-run" and not backend.is_mock:
        raise ContractError("dry-run mode must use the dry-run backend")

    root = Path(run_dir).resolve()
    root.mkdir(parents=True, exist_ok=True)
    state_path = root / "state.json"
    if state_path.exists():
        if not resume:
            raise StateError(f"run already exists; pass --resume to continue: {root}")
        state = _load_state(state_path)
    else:
        state = _new_state()
        write_json_atomic(state_path, state)

    with _run_lock(root):
        try:
            _snapshot_inputs(root, manifest, scenario, tape)
        except BaseException:
            backend.shutdown()
            raise
        if state["status"] == "COMPLETE":
            try:
                _validate_persisted_artifacts(state, allow_mock_audio=backend.is_mock)
                return load_run_summary(root)
            finally:
                backend.shutdown()
        try:
            _validate_persisted_artifacts(state, allow_mock_audio=backend.is_mock)
        except BaseException:
            backend.shutdown()
            raise
        sealed_scenario = Scenario.load(root / "config" / "scenario.json")
        sealed_tape = load_tape(root / "config" / "camera-tape.json")
        try:
            report = run_preflight(manifest, mode=mode)
            write_json_atomic(root / "evidence" / "preflight.json", report.to_dict())
            if not report.ok:
                raise ContractError("preflight failed; inspect evidence/preflight.json")
            _mark(state_path, state, "PREFLIGHT_PASS")

            hello = backend.hello()
            applied = backend.apply_scenario(sealed_scenario)
            _mark(state_path, state, "SCENARIO_APPLIED",
                  artifacts={"backend_hello": hello, "scenario_apply": applied})

            if "MASTER_RECORDED" not in state["completed"]:
                master = backend.record_master(sealed_scenario, root)
                _mark(state_path, state, "MASTER_RECORDED", artifacts={"master": master})
            else:
                master = state["artifacts"]["master"]

            lane_artifacts: dict[int, dict] = {}
            persisted_lanes = state["artifacts"].setdefault("lanes", {})
            for lane in range(1, 5):
                step = f"LANE_{lane}_EXPORTED"
                existing = persisted_lanes.get(str(lane))
                if step in state["completed"] and existing:
                    artifact = existing
                else:
                    artifact = backend.export_lane(lane, sealed_tape, master, root)
                    persisted_lanes[str(lane)] = artifact
                    _mark(state_path, state, step, artifacts={"lanes": persisted_lanes})
                lane_artifacts[lane] = artifact

            if not backend.is_mock:
                replay_binding = lane_artifacts[4].get("editor_project_binding")
                if not isinstance(replay_binding, dict):
                    raise ContractError(
                        "lane 4 did not seal the RE+ runtime editor/replay binding")
                _require_sealed_file(
                    replay_binding.get("path"), replay_binding.get("sha256"),
                    "runtime editor/replay binding")
                master["editor_project_binding"] = replay_binding
                state["artifacts"]["master"] = master
                write_json_atomic(state_path, state)

            if "AUDIO_CAPTURED" not in state["completed"]:
                audio = backend.capture_audio(sealed_scenario, master, root)
                _mark(state_path, state, "AUDIO_CAPTURED", artifacts={"audio": audio})
            else:
                audio = state["artifacts"]["audio"]

            event_paths = {lane: lane_artifacts[lane]["frame_events_path"] for lane in range(1, 5)}
            render_binding = (
                None if backend.is_mock else load_render_profile_binding(manifest))
            alignment, events = validate_alignment(
                event_paths, sealed_tape, allow_mock=backend.is_mock,
                expected_replay_sha256=master["replay_sha256"],
                expected_render_profile_sha256=(
                    render_binding.render_profile_sha256 if render_binding else None),
                expected_output_root=(
                    render_binding.expected_output_root if render_binding else None))
            write_json_atomic(root / "evidence" / "alignment-report.json", alignment.to_dict())
            if not alignment.ok:
                raise ContractError("cross-lane alignment failed; inspect alignment-report.json")
            _mark(state_path, state, "ALIGNMENT_PASS")

            capture_dir = root / "capture"
            calibration = manifest.raw["calibration"]
            metadata = materialize_metadata(
                capture_dir, events, sealed_tape,
                source_epoch=master["source_epoch"],
                process_name=manifest.raw["game"].get("process_name", "GTA5.exe"),
                metric_scale=float(calibration["metric_scale"]),
                dry_run=backend.is_mock,
            )
            write_json_atomic(root / "evidence" / "metadata-report.json", metadata)
            _mark(state_path, state, "METADATA_MATERIALIZED")

            videos: list[str] = []
            if not backend.is_mock:
                videos = materialize_videos(capture_dir, lane_artifacts, audio,
                                             ffmpeg=manifest.pinned_tool_path("ffmpeg"),
                                             lane_events=events,
                                             ffprobe=manifest.pinned_tool_path("ffprobe"),
                                             fps=int(manifest.raw["render"]["fps"]))
                _mark(state_path, state, "MEDIA_MATERIALIZED", artifacts={"videos": videos})
            else:
                _mark(state_path, state, "MEDIA_SKIPPED_DRYRUN")

            audio_report = load_json(audio["audio_report_path"])
            trajectory_verdict = _trajectory_verdict(root, tape, dry_run=backend.is_mock)
            if trajectory_verdict is not None:
                write_json_atomic(root / "evidence" / "trajectory-verdict.json",
                                  trajectory_verdict)
            pr405_gate = build_pr405_gate(
                alignment=alignment.to_dict(), lanes=events, audio_report=audio_report,
                calibration=calibration, policy=manifest.policy, dry_run=backend.is_mock,
                replay_binding=master.get("editor_project_binding"),
                expected_render_profile_sha256=(
                    render_binding.render_profile_sha256 if render_binding else None),
                expected_output_root=(
                    render_binding.expected_output_root if render_binding else None),
                trajectory_verdict=trajectory_verdict)
            write_json_atomic(root / "evidence" / "gta-pr405-gate.json", pr405_gate)

            run_manifest = {
                "schema": RUN_SCHEMA,
                "run_id": state["run_id"],
                "created_at": state["history"][0]["at"],
                "completed_at": utc_now(),
                "mode": mode,
                "backend": backend.name,
                "dry_run": backend.is_mock,
                "delivery_eligible": (pr405_gate["delivery_eligible"] and not backend.is_mock
                                      and not repository_profile_non_trainable()),
                "timestamp_semantics": "source_replay_timeline",
                "render_mode": "sequential_deterministic_replay",
                "wallclock_simultaneous": False,
                "inputs": {
                    "dependency_manifest": file_record(root / "config" / "dependency-manifest.json"),
                    "scenario": file_record(root / "config" / "scenario.json"),
                    "camera_tape": file_record(root / "config" / "camera-tape.json"),
                    "camera_tape_native": file_record(root / "config" / "camera-tape.bin"),
                },
                "master": master,
                "lane_artifacts": {str(k): v for k, v in lane_artifacts.items()},
                "audio": audio,
                "videos": [file_record(path) for path in videos],
                "metadata_files": [file_record(path) for path in metadata["files"]],
                "delivery_files": ([
                    file_record(capture_dir / f"{stem}_{lane}.{suffix}")
                    for lane in range(1, 5)
                    for stem, suffix in (("video", "mp4"),
                                         ("systeminfo", "json"),
                                         ("action_camera", "json"))
                ] if not backend.is_mock else []),
                "alignment": alignment.to_dict(),
                "gta_pr405_gate": pr405_gate,
                "capture_dir": str(capture_dir.resolve()),
                "next_gate": "dry-run only; install GTA dependencies" if backend.is_mock
                             else "run engine_qc --mode mc and independent human review",
            }
            write_json_atomic(root / "run-manifest.json", run_manifest)
            _mark(state_path, state, "COMPLETE",
                  artifacts={"run_manifest": file_record(root / "run-manifest.json")})
            return run_manifest
        except BaseException as exc:
            state["status"] = "FAILED"
            state["error"] = {
                "at": utc_now(),
                "type": type(exc).__name__,
                "message": str(exc),
                "traceback": traceback.format_exc(),
            }
            state["history"].append({"at": utc_now(), "status": "FAILED",
                                     "error": str(exc)})
            write_json_atomic(state_path, state)
            raise
        finally:
            backend.shutdown()


def load_run_summary(run_dir) -> dict:
    import json
    path = Path(run_dir) / "run-manifest.json"
    if not path.is_file():
        raise StateError(f"completed run manifest not found: {path}")
    return json.loads(path.read_text(encoding="utf-8"))

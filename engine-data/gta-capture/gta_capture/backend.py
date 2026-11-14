"""Capture backends: deterministic dry-run and the real ASI named-pipe bridge."""

from __future__ import annotations

import hashlib
import json
from abc import ABC, abstractmethod
from pathlib import Path

from .audio import (normalize_replus_wav, radio_off_evidence_valid,
                    validate_wav)
from .errors import BackendError
from .jsonio import sha256_file, sha256_json, write_json_atomic, write_jsonl_atomic
from .protocol import FRAME_SCHEMA, JsonLinePipeClient
from .preflight import DependencyManifest, load_render_profile_binding
from .scenario import Scenario


def _sample_velocity(samples: list[dict], frame: int) -> list[float]:
    return [(float(samples[frame]["position"][axis]) -
             float(samples[frame - 1]["position"][axis])) * 30.0
            for axis in range(3)]


class CaptureBackend(ABC):
    name = "abstract"
    is_mock = False

    @abstractmethod
    def hello(self) -> dict:
        raise NotImplementedError

    @abstractmethod
    def apply_scenario(self, scenario: Scenario) -> dict:
        raise NotImplementedError

    @abstractmethod
    def record_master(self, scenario: Scenario, run_dir: Path) -> dict:
        raise NotImplementedError

    @abstractmethod
    def export_lane(self, lane: int, tape: dict, master: dict, run_dir: Path) -> dict:
        raise NotImplementedError

    @abstractmethod
    def capture_audio(self, scenario: Scenario, master: dict, run_dir: Path) -> dict:
        raise NotImplementedError

    def shutdown(self) -> None:
        return None


class DryRunBackend(CaptureBackend):
    name = "dry-run"
    is_mock = True

    def hello(self) -> dict:
        return {
            "backend": self.name,
            "adapter_ready": True,
            "game_connected": False,
            "story_mode": True,
            "mock": True,
        }

    def apply_scenario(self, scenario: Scenario) -> dict:
        return {"scenario_id": scenario.scenario_id, "scenario_hash": scenario.fingerprint,
                "applied": True, "mock": True}

    def record_master(self, scenario: Scenario, run_dir: Path) -> dict:
        source = run_dir / "source"
        source.mkdir(parents=True, exist_ok=True)
        replay = source / "master-replay.mock.json"
        write_json_atomic(replay, {
            "schema": "dm-gta-mock-replay/1",
            "scenario_id": scenario.scenario_id,
            "scenario_hash": scenario.fingerprint,
            "duration_s": scenario.duration_s,
            "fps": 30,
            "warning": "dry-run protocol artifact; contains no GTA pixels and is never deliverable",
        })
        source_epoch = scenario.raw.get("source_epoch", "2026-08-25 00:00:00.000")
        return {
            "replay_path": str(replay.resolve()),
            "replay_sha256": sha256_file(replay),
            "source_epoch": source_epoch,
            "mock": True,
        }

    def export_lane(self, lane: int, tape: dict, master: dict, run_dir: Path) -> dict:
        lane_data = tape["lanes"][str(lane)]
        evidence_dir = run_dir / "evidence"
        events_path = evidence_dir / f"frame-events-lane{lane}.jsonl"
        replay_hash = master["replay_sha256"]
        scenario_hash = load_scenario_hash(run_dir)
        rows = []
        for sample in lane_data["samples"]:
            frame = sample["frame"]
            player_samples = tape["lanes"]["1"]["samples"]
            player_sample = player_samples[frame]
            player_position = list(player_sample["position"])
            player_position[2] = float(player_position[2]) - 1.0
            entity_hash = hashlib.sha256(
                f"{scenario_hash}:{replay_hash}:{frame}".encode("utf-8")
            ).hexdigest()
            rows.append({
                "schema": FRAME_SCHEMA,
                "lane": lane,
                "output_frame": frame,
                "replay_frame": frame,
                "replay_time_ms": sample["source_t_ns"] / 1_000_000.0,
                "source_t_ns": sample["source_t_ns"],
                "replay_sha256": replay_hash,
                "render_succeeded": True,
                "final_camera": {
                    "position": sample["position"],
                    "quaternion": sample["quaternion"],
                    "euler_deg": sample.get("euler_deg", [0.0, 0.0, 0.0]),
                    "intrinsics": sample["intrinsics"],
                    "focal_mm": lane_data["focal_mm"],
                },
                "player_state": {
                    "position": player_position,
                    "quaternion": player_sample["quaternion"],
                    "euler_deg": player_sample.get("euler_deg", [0.0, 0.0, 0.0]),
                    "velocity": [0.0, 0.0, 0.0] if frame == 0 else _sample_velocity(
                        player_samples, frame),
                },
                "entity_state_hash": entity_hash,
                "input": {"source": "scripted_camera", "mouse_dx": 0.0,
                          "mouse_dy": 0.0, "keyCode": []},
                "mock": True,
            })
        write_jsonl_atomic(events_path, rows)
        return {
            "lane": lane,
            "frame_events_path": str(events_path.resolve()),
            "frame_events_sha256": sha256_file(events_path),
            "frame_count": len(rows),
            "video_path": None,
            "mock": True,
        }

    def capture_audio(self, scenario: Scenario, master: dict, run_dir: Path) -> dict:
        report = run_dir / "evidence" / "audio-report.json"
        write_json_atomic(report, {
            "schema": "dm-gta-audio-report/1",
            "source": "dry-run",
            "audio_path": None,
            "continuous": False,
            "non_silent": False,
            "rights_checked": False,
            "mock": True,
        })
        return {"audio_report_path": str(report.resolve()),
                "audio_report_sha256": sha256_file(report),
                "audio_path": None, "mock": True}


def load_scenario_hash(run_dir: Path) -> str:
    config = run_dir / "config" / "scenario.json"
    if not config.is_file():
        raise BackendError(f"run scenario snapshot is missing: {config}")
    return sha256_json(json.loads(config.read_text(encoding="utf-8")))


class PipeBackend(CaptureBackend):
    name = "gta-asi-pipe"
    is_mock = False

    def __init__(self, pipe_name: str):
        self.client = JsonLinePipeClient(pipe_name)

    def hello(self) -> dict:
        result = self.client.request("hello")
        if result.get("story_mode") is not True:
            raise BackendError("GTA companion is not in Story Mode")
        if result.get("online_session") is True:
            raise BackendError("GTA companion reported an Online session")
        if result.get("adapter_ready") is not True:
            raise BackendError("GTA native adapter is present but not ready")
        return result

    def apply_scenario(self, scenario: Scenario) -> dict:
        return self.client.request("apply_scenario", {
            "scenario_path": str(scenario.path),
            # The native companion hashes the immutable on-disk snapshot. The
            # canonical JSON fingerprint remains available in Python metadata.
            "scenario_sha256": sha256_file(scenario.path),
        })

    def record_master(self, scenario: Scenario, run_dir: Path) -> dict:
        result = self.client.request("record_master", {
            "scenario_path": str(scenario.path),
            "duration_s": scenario.duration_s,
            "run_dir": str(run_dir.resolve()),
        })
        replay = result.get("replay_path")
        if not isinstance(replay, str) or not Path(replay).is_file():
            raise BackendError("companion did not produce a replay file")
        actual = sha256_file(replay)
        if result.get("replay_sha256") != actual:
            raise BackendError("companion replay sha256 does not match the file")
        return result

    def export_lane(self, lane: int, tape: dict, master: dict, run_dir: Path) -> dict:
        tape_json_path = run_dir / "config" / "camera-tape.json"
        tape_path = run_dir / "config" / "camera-tape.bin"
        binding = load_render_profile_binding(DependencyManifest.load(
            run_dir / "config" / "dependency-manifest.json"))
        result = self.client.request("export_lane", {
            "lane": lane,
            "camera_tape_path": str(tape_path.resolve()),
            "camera_tape_sha256": sha256_file(tape_json_path),
            "replay_path": master["replay_path"],
            "replay_sha256": master["replay_sha256"],
            "run_dir": str(run_dir.resolve()),
            "expected_output_root": binding.expected_output_root,
            "render_profile_sha256": binding.render_profile_sha256,
        })
        if (result.get("render_profile_sha256") != binding.render_profile_sha256
                or Path(result.get("normalized_output_root", "")).resolve() !=
                Path(binding.expected_output_root).resolve()):
            raise BackendError(
                f"lane {lane} result changed the sealed render profile/output root")
        events = result.get("frame_events_path")
        if not isinstance(events, str) or not Path(events).is_file():
            raise BackendError(f"lane {lane} frame-events file is missing")
        if result.get("frame_events_sha256") != sha256_file(events):
            raise BackendError(f"lane {lane} frame-events hash mismatch")
        return result

    def capture_audio(self, scenario: Scenario, master: dict, run_dir: Path) -> dict:
        result = self.client.request("capture_audio", {
            "replay_path": master["replay_path"],
            "replay_sha256": master["replay_sha256"],
            "duration_s": scenario.duration_s,
            "run_dir": str(run_dir.resolve()),
            "radio_policy": "off",
        })
        audio_path = result.get("audio_path")
        if not isinstance(audio_path, str):
            raise BackendError("companion did not seal the RE+ normal-speed audio sub-pass")
        reported_source_sha256 = result.get("audio_sha256")
        normalization = normalize_replus_wav(audio_path)
        if reported_source_sha256 != normalization["source_audio_sha256"]:
            raise BackendError(
                "companion audio sha256 does not match the pre-normalization WAV")
        report = validate_wav(audio_path, expected_duration_s=scenario.duration_s)
        report["normalization"] = normalization
        report["replay_sha256"] = master["replay_sha256"]
        radio_evidence = result.get("radio_off_evidence")
        report["radio_off_evidence"] = radio_evidence
        report["radio_off"] = radio_off_evidence_valid(
            radio_evidence, expected_replay_sha256=master["replay_sha256"],
            expected_duration_s=scenario.duration_s)
        result["source_audio_sha256"] = normalization["source_audio_sha256"]
        result["audio_sha256"] = report["audio_sha256"]
        result["normalization"] = normalization
        report_path = run_dir / "evidence" / "audio-report.json"
        write_json_atomic(report_path, report)
        if not report["continuous"] or not report["non_silent"]:
            raise BackendError(
                "RE+ audio sub-pass failed duration/continuity/non-silence validation")
        result["audio_report_path"] = str(report_path.resolve())
        result["audio_report_sha256"] = sha256_file(report_path)
        result["validation"] = {key: report[key] for key in (
            "duration_actual_s", "continuous", "non_silent", "normalized_rms")}
        return result

    def shutdown(self) -> None:
        try:
            self.client.request("detach")
        except BackendError:
            pass
        finally:
            self.client.close()

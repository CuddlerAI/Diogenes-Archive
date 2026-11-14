import hashlib
import json
import math
import struct
import tempfile
import unittest
import wave
from pathlib import Path
from unittest.mock import patch

from helpers import manifest_dict, scenario_dict, write_json
from gta_capture.backend import DryRunBackend
from gta_capture.errors import ContractError, StateError
from gta_capture.evidence import (REAL_EDITOR_BINDING_METHOD,
                                  REAL_EDITOR_IDENTITY_PROTOCOL,
                                  REAL_PLAYER_OBSERVATION_METHOD,
                                  validate_alignment)
from gta_capture.gate import _replus_clip_identity, build_pr405_gate
from gta_capture.jsonio import read_jsonl, sha256_file, write_jsonl_atomic
from gta_capture.media import (_verify_muxed_output, materialize_videos,
                               mux_master_audio)
from gta_capture.orchestrator import run_capture
from gta_capture.policy import CapturePolicy
from gta_capture.preflight import Check, DependencyManifest, PreflightReport
from gta_capture.scenario import Scenario
from gta_capture.staged import (_preflight_materialization_decision,
                                _revalidate_sealed_audio, finalize_staged_poc)
from gta_capture.tape import make_demo_tape, make_gta_poc_tape


class FrameSealTests(unittest.TestCase):
    @staticmethod
    def _probe_report(*, frames=30, fps="30/1", duration="1.000000", audio=True):
        streams = [{
            "codec_type": "video", "nb_read_frames": str(frames),
            "avg_frame_rate": fps, "r_frame_rate": fps, "duration": duration,
        }]
        if audio:
            streams.append({
                "codec_type": "audio", "sample_rate": "48000", "channels": 2,
                "duration": duration,
            })
        return {"streams": streams, "format": {"duration": duration}}

    def _real_like_events(self, root: Path):
        manifest = DependencyManifest.load(write_json(root / "manifest.json", manifest_dict()))
        scenario = Scenario.load(write_json(root / "scenario.json", scenario_dict()))
        tape = make_demo_tape(duration_s=1.0)
        result = run_capture(manifest=manifest, scenario=scenario, tape=tape,
                             run_dir=root / "run", backend=DryRunBackend(), mode="dry-run")
        source_clip_identity = "0123456789abcdef"
        lane_rows = {
            lane: read_jsonl(Path(result["lane_artifacts"][str(lane)]["frame_events_path"]))
            for lane in range(1, 5)
        }
        telemetry_path = root / "master-player-telemetry.jsonl"
        write_jsonl_atomic(telemetry_path, [{
            "schema": "dm-gta-player-telemetry/1",
            "frame": frame,
            "source_t_ns": row["source_t_ns"],
            "observation_t_ns": row["source_t_ns"],
            "player_observation_method":
                "record_master_recording-side-estimate-no-catchup-30fps",
            "replay_sha256": result["master"]["replay_sha256"],
            "source_clip_identity": source_clip_identity,
            "player_state": row["player_state"],
            "entity_state_hash": row["entity_state_hash"],
        } for frame, row in enumerate(lane_rows[1])])
        telemetry_sha256 = sha256_file(telemetry_path)
        paths = {}
        for lane in range(1, 5):
            path = Path(result["lane_artifacts"][str(lane)]["frame_events_path"])
            rows = lane_rows[lane]
            for index, row in enumerate(rows):
                rgb = root / f"lane-{lane}-frame-{index:06d}.png"
                rgb.write_bytes(f"sealed-rgb-{lane}-{index}".encode("ascii"))
                generation = (lane - 1) * len(rows) + index + 1
                row.update({
                    "mock": False,
                    "capture_protocol": 18,
                    "editor_identity_protocol": 4,
                    "capture_fence":
                        "present_pixel_ready_final_writer_generation_pose_release_ack",
                    "pixel_ready_final_camera_generation": generation,
                    "editor_binding": {
                        "method": "replus-present-plan-v2",
                        "session_id": f"session-{lane}",
                        "project_name": "Diogenes POC",
                        "clip_count": 1,
                        "clip_index": 0,
                        "clip_identity": source_clip_identity,
                        "identity_sequence": generation * 2,
                        "identity_heartbeat": generation,
                        "capture_request_id": generation,
                        "final_camera_generation": generation,
                        "render_profile_sha256": "a" * 64,
                        "normalized_output_root": str(root.resolve()),
                    },
                    "player_observation_method":
                        "record_master_recording-side-estimate-no-catchup-30fps",
                    "player_observation_t_ns": row["source_t_ns"],
                    "player_telemetry_path": str(telemetry_path.resolve()),
                    "player_telemetry_sha256": telemetry_sha256,
                    "player_telemetry_frame": index,
                    "output_path": str(rgb.resolve()),
                    "output_bytes": rgb.stat().st_size,
                    "output_sha256": sha256_file(rgb),
                })
            write_jsonl_atomic(path, rows)
            paths[lane] = str(path)
        return tape, result, paths

    @staticmethod
    def _frame_gate(paths, replay_sha256, *, calibration=None):
        policy = CapturePolicy.from_dict(manifest_dict()["policy"])
        lanes = {lane: read_jsonl(paths[lane]) for lane in range(1, 5)}
        gate = build_pr405_gate(
            alignment={"ok": True, "logical_interlane_delta_ns_max": 0,
                       "replay_sha256": replay_sha256},
            lanes=lanes, audio_report={}, calibration=calibration or {}, policy=policy,
            dry_run=False, replay_binding=None)
        return gate, lanes

    def test_real_alignment_rehashes_every_rgb_before_finalize(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            tape, result, paths = self._real_like_events(root)
            report, _ = validate_alignment(
                paths, tape, allow_mock=False,
                expected_replay_sha256=result["master"]["replay_sha256"])
            self.assertTrue(report.ok, report.problems)
            self.assertTrue(report.player_telemetry_diagnostic_ok)
            self.assertFalse(report.player_replay_origin_verified)

            first = Path(read_jsonl(paths[2])[3]["output_path"])
            first.write_bytes(b"post-fence-tamper")
            report, _ = validate_alignment(
                paths, tape, allow_mock=False,
                expected_replay_sha256=result["master"]["replay_sha256"])
            self.assertFalse(report.ok)
            self.assertTrue(any("sealed RGB output changed" in item
                                for item in report.problems))

    def test_valid_v18_estimate_grants_pixel_but_not_player_state(self):
        with tempfile.TemporaryDirectory() as tmp:
            _tape, result, paths = self._real_like_events(Path(tmp))
            gate, _lanes = self._frame_gate(paths, result["master"]["replay_sha256"])
            self.assertIn("pixel_pose_fence", gate["passed_rules"])
            self.assertIn("player_state", gate["missing_rules"])
            self.assertIn("player_replay_origin", gate["missing_rules"])
            self.assertFalse(gate["internal_poc_usable"])
            self.assertTrue(gate["player_evidence"]["telemetry_diagnostic_ok"])
            self.assertFalse(gate["player_evidence"]["replay_origin_verified"])
            self.assertIn("player_replay_origin_unverified", gate["pending_reasons"])

    def test_frame_event_schema_requires_complete_real_evidence_envelope(self):
        schema = json.loads((Path(__file__).resolve().parents[1] / "schemas" /
                             "frame-event.schema.json").read_text(encoding="utf-8"))
        required = set(schema["required"])
        self.assertTrue({
            "editor_binding", "capture_protocol", "editor_identity_protocol",
            "capture_fence", "pixel_ready_final_camera_generation",
            "player_observation_method", "player_observation_t_ns",
            "player_telemetry_path", "player_telemetry_sha256",
            "player_telemetry_frame", "output_path", "output_bytes",
            "output_sha256",
        }.issubset(required))
        self.assertEqual(schema["properties"]["capture_protocol"]["const"], 18)
        self.assertEqual(schema["properties"]["editor_identity_protocol"]["const"],
                         REAL_EDITOR_IDENTITY_PROTOCOL)
        self.assertEqual(
            schema["properties"]["editor_binding"]["properties"]["method"]["const"],
            REAL_EDITOR_BINDING_METHOD)
        self.assertEqual(
            schema["properties"]["player_observation_method"]["const"],
            REAL_PLAYER_OBSERVATION_METHOD)
        self.assertTrue({
            "final_camera_generation", "render_profile_sha256",
            "normalized_output_root",
        }.issubset(schema["properties"]["editor_binding"]["required"]))

    def test_old_protocol_or_identity_generation_cannot_grant_pixel_fence(self):
        for key, stale in (("capture_protocol", 17), ("editor_identity_protocol", 2)):
            with self.subTest(key=key), tempfile.TemporaryDirectory() as tmp:
                _tape, result, paths = self._real_like_events(Path(tmp))
                rows = read_jsonl(paths[1])
                rows[0][key] = stale
                write_jsonl_atomic(Path(paths[1]), rows)
                gate, _lanes = self._frame_gate(
                    paths, result["master"]["replay_sha256"])
                self.assertIn("pixel_pose_fence", gate["missing_rules"])

    def test_pixel_ready_and_final_camera_generation_must_independently_match(self):
        with tempfile.TemporaryDirectory() as tmp:
            _tape, result, paths = self._real_like_events(Path(tmp))
            rows = read_jsonl(paths[2])
            rows[4]["pixel_ready_final_camera_generation"] += 1
            write_jsonl_atomic(Path(paths[2]), rows)
            gate, _lanes = self._frame_gate(paths, result["master"]["replay_sha256"])
            self.assertIn("pixel_pose_fence", gate["missing_rules"])

        with tempfile.TemporaryDirectory() as tmp:
            _tape, result, paths = self._real_like_events(Path(tmp))
            rows = read_jsonl(paths[2])
            stale = rows[3]["editor_binding"]["final_camera_generation"]
            rows[4]["editor_binding"]["final_camera_generation"] = stale
            rows[4]["pixel_ready_final_camera_generation"] = stale
            write_jsonl_atomic(Path(paths[2]), rows)
            gate, _lanes = self._frame_gate(paths, result["master"]["replay_sha256"])
            self.assertIn("pixel_pose_fence", gate["missing_rules"])

    def test_self_reported_player_method_cannot_grant_player_state(self):
        with tempfile.TemporaryDirectory() as tmp:
            _tape, result, paths = self._real_like_events(Path(tmp))
            rows = read_jsonl(paths[3])
            rows[0]["player_observation_method"] = \
                "record_master-script-tick-self-report-30fps"
            write_jsonl_atomic(Path(paths[3]), rows)
            gate, _lanes = self._frame_gate(paths, result["master"]["replay_sha256"])
            self.assertIn("player_state", gate["missing_rules"])
            self.assertFalse(gate["player_evidence"]["telemetry_diagnostic_ok"])

    def test_caller_authored_origin_calibration_cannot_upgrade_estimate(self):
        with tempfile.TemporaryDirectory() as tmp:
            _tape, result, paths = self._real_like_events(Path(tmp))
            fake_calibration = {
                "player_replay_origin_calibration": {
                    "schema": "dm-gta-player-replay-origin/1",
                    "verified": True,
                    "method": "operator-asserted-json",
                    "frame_zero_offset_ns": 0,
                    "replay_sha256": result["master"]["replay_sha256"],
                }
            }
            gate, _lanes = self._frame_gate(
                paths, result["master"]["replay_sha256"],
                calibration=fake_calibration)
            self.assertIn("player_replay_origin", gate["missing_rules"])
            self.assertIn("player_state", gate["missing_rules"])
            self.assertIn("player_replay_origin_unverified", gate["pending_reasons"])
            self.assertFalse(gate["player_evidence"]["replay_origin_verified"])

    def test_missing_real_evidence_fields_fail_closed(self):
        cases = (
            ("output_sha256", "pixel_pose_fence"),
            ("pixel_ready_final_camera_generation", "pixel_pose_fence"),
            ("player_telemetry_sha256", "player_state"),
            ("player_observation_t_ns", "player_state"),
        )
        for field, rule in cases:
            with self.subTest(field=field), tempfile.TemporaryDirectory() as tmp:
                _tape, result, paths = self._real_like_events(Path(tmp))
                rows = read_jsonl(paths[1])
                rows[0].pop(field)
                write_jsonl_atomic(Path(paths[1]), rows)
                gate, _lanes = self._frame_gate(
                    paths, result["master"]["replay_sha256"])
                self.assertIn(rule, gate["missing_rules"])

    def test_player_telemetry_frame_time_and_replay_binding_are_recomputed(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            _tape, result, paths = self._real_like_events(root)
            rows = read_jsonl(paths[1])
            telemetry_path = Path(rows[0]["player_telemetry_path"])
            telemetry = read_jsonl(telemetry_path)
            telemetry[0]["observation_t_ns"] += 1
            write_jsonl_atomic(telemetry_path, telemetry)
            changed_sha = sha256_file(telemetry_path)
            for lane in range(1, 5):
                lane_rows = read_jsonl(paths[lane])
                for row in lane_rows:
                    row["player_telemetry_sha256"] = changed_sha
                write_jsonl_atomic(Path(paths[lane]), lane_rows)
            gate, _lanes = self._frame_gate(paths, result["master"]["replay_sha256"])
            self.assertIn("player_state", gate["missing_rules"])

        with tempfile.TemporaryDirectory() as tmp:
            _tape, result, paths = self._real_like_events(Path(tmp))
            rows = read_jsonl(paths[4])
            rows[0]["player_telemetry_frame"] = 1
            write_jsonl_atomic(Path(paths[4]), rows)
            gate, _lanes = self._frame_gate(paths, result["master"]["replay_sha256"])
            self.assertIn("player_state", gate["missing_rules"])

    def test_operator_replay_hash_claim_cannot_pass_machine_binding_gate(self):
        policy = CapturePolicy.from_dict(manifest_dict()["policy"])
        alignment = {"ok": True, "logical_interlane_delta_ns_max": 0,
                     "replay_sha256": "a" * 64}
        gate = build_pr405_gate(
            alignment=alignment, lanes={1: []}, audio_report={}, calibration={},
            policy=policy, dry_run=False, replay_binding=None)
        self.assertIn("actual_replay_clock", gate["passed_rules"])
        self.assertIn("four_lane_same_replay", gate["missing_rules"])
        self.assertFalse(gate["internal_poc_usable"])

        with tempfile.TemporaryDirectory() as tmp:
            fake = write_json(Path(tmp) / "fake-binding.json", {
                "schema": "dm-gta-editor-replay-binding/1",
                "machine_verified": True,
                "replay_sha256": "a" * 64,
                "source_clip_sha256": "a" * 64,
                "editor_project_fingerprint": "b" * 64,
                "lane_sessions": [
                    {"lane": lane, "session_id": f"session-{lane}",
                     "replay_sha256": "a" * 64}
                    for lane in range(1, 5)
                ],
            })
            gate = build_pr405_gate(
                alignment=alignment, lanes={1: []}, audio_report={}, calibration={},
                policy=policy, dry_run=False,
                replay_binding={"path": str(fake), "sha256": sha256_file(fake)})
        self.assertIn("four_lane_same_replay", gate["missing_rules"])

        # A legacy caller-authored "project files" envelope still cannot pass:
        # the accepted method is now the request-bound RE+ live montage channel.
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            clip = root / "source.clip"
            project = root / "project.dat"
            clip.write_bytes(b"a source clip that hashes independently")
            project.write_bytes(b"dummy project with no parsed clip reference")
            replay_sha = sha256_file(clip)
            records = [{"relative_path": "project.dat", "bytes": project.stat().st_size,
                        "sha256": sha256_file(project)}]
            fingerprint = hashlib.sha256(json.dumps(
                records, sort_keys=True, separators=(",", ":")).encode("utf-8")).hexdigest()
            complete = write_json(root / "complete-binding.json", {
                "schema": "dm-gta-editor-replay-binding/1",
                "machine_verified": True,
                "verification_method": "rockstar-editor-project-files-v1",
                "replay_sha256": replay_sha,
                "source_clip_sha256": replay_sha,
                "project_clip_reference_sha256": replay_sha,
                "editor_project_fingerprint": fingerprint,
                "source_clip": {"path": str(clip), "bytes": clip.stat().st_size,
                                "sha256": replay_sha},
                "project_artifacts": [{"path": str(project), **records[0]}],
                "lane_sessions": [
                    {"lane": lane, "session_id": f"session-{lane}",
                     "replay_sha256": replay_sha,
                     "editor_project_fingerprint": fingerprint}
                    for lane in range(1, 5)
                ],
            })
            gate = build_pr405_gate(
                alignment={**alignment, "replay_sha256": replay_sha}, lanes={1: []},
                audio_report={}, calibration={}, policy=policy, dry_run=False,
                replay_binding={"path": str(complete), "sha256": sha256_file(complete)})
            self.assertIn("four_lane_same_replay", gate["missing_rules"])

    def test_replus_identity_hashes_the_live_montage_stem(self):
        # Verified against RE+ identity-v4 while GTA Legacy 3889 had this real
        # clip loaded in a one-clip project. CLIP_NAME omits `.clip`.
        self.assertEqual(
            _replus_clip_identity("2026-8-27-片段-0006.clip"),
            "4443aa503032c2ec",
        )
        self.assertEqual(
            _replus_clip_identity("2026-8-27-片段-0006"),
            "4443aa503032c2ec",
        )

    @staticmethod
    def _runtime_binding_fixture(root: Path):
        """Build one valid RE+ runtime montage binding plus its lane events."""
        source_name = "2026-08-25-clip-0001.clip"
        clip = root / "master-replay.clip"
        original = root / source_name
        clip.write_bytes(b"same sealed Rockstar recording")
        original.write_bytes(clip.read_bytes())
        replay_sha = sha256_file(clip)
        clip_identity = _replus_clip_identity(source_name)
        runtime = write_json(root / "editor-runtime-identity.json", {
            "schema": "dm-gta-editor-runtime-identity/1",
            "verification_method": "replus-runtime-montage-v1",
            "project_name": "Diogenes POC",
            "clip_count": 1,
            "clip_index": 0,
            "source_clip_name": source_name,
            "source_clip_identity": clip_identity,
            "source_clip_identity_algorithm":
                "replus-fnv1a63-raw-stem-occurrence-v1",
            "replay_sha256": replay_sha,
        })
        fingerprint = sha256_file(runtime)
        sessions = [{
            "lane": lane,
            "session_id": f"session-{lane}",
            "project_name": "Diogenes POC",
            "clip_identity": clip_identity,
            "replay_sha256": replay_sha,
            "editor_project_fingerprint": fingerprint,
        } for lane in range(1, 5)]
        binding = write_json(root / "editor-replay-binding.json", {
            "schema": "dm-gta-editor-replay-binding/1",
            "machine_verified": True,
            "verification_method": "replus-runtime-montage-v1",
            "replay_sha256": replay_sha,
            "source_clip_sha256": replay_sha,
            "project_clip_reference_sha256": replay_sha,
            "source_clip_name": source_name,
            "source_clip_identity": clip_identity,
            "editor_project_fingerprint": fingerprint,
            "source_clip": {"path": str(clip.resolve()),
                            "bytes": clip.stat().st_size, "sha256": replay_sha},
            "source_original_clip": {"path": str(original.resolve()),
                                     "bytes": original.stat().st_size,
                                     "sha256": replay_sha},
            "project_artifacts": [{
                "path": str(runtime.resolve()),
                "relative_path": "evidence/editor-runtime-identity.json",
                "bytes": runtime.stat().st_size,
                "sha256": fingerprint,
            }],
            "lane_sessions": sessions,
        })
        lane_events = {lane: [{
            "replay_sha256": replay_sha,
            "capture_protocol": 18,
            "editor_identity_protocol": 4,
            "capture_fence":
                "present_pixel_ready_final_writer_generation_pose_release_ack",
            "pixel_ready_final_camera_generation": lane,
            "editor_binding": {
                "method": "replus-present-plan-v2",
                "session_id": f"session-{lane}",
                "project_name": "Diogenes POC",
                "clip_count": 1,
                "clip_index": 0,
                "clip_identity": clip_identity,
                "identity_sequence": lane * 2,
                "identity_heartbeat": lane,
                "capture_request_id": lane,
                "final_camera_generation": lane,
            },
        }] for lane in range(1, 5)}
        alignment = {"ok": True, "logical_interlane_delta_ns_max": 0,
                     "replay_sha256": replay_sha}
        record = {"path": str(binding.resolve()), "sha256": sha256_file(binding)}
        return alignment, lane_events, record

    def _binding_gate(self, alignment, lane_events, record):
        policy = CapturePolicy.from_dict(manifest_dict()["policy"])
        return build_pr405_gate(
            alignment=alignment, lanes=lane_events, audio_report={}, calibration={},
            policy=policy, dry_run=False, replay_binding=record)

    def test_runtime_montage_binding_requires_four_request_bound_lane_sessions(self):
        with tempfile.TemporaryDirectory() as tmp:
            alignment, lane_events, record = self._runtime_binding_fixture(Path(tmp))
            gate = self._binding_gate(alignment, lane_events, record)
            self.assertIn("four_lane_same_replay", gate["passed_rules"])

            lane_events[3][0]["editor_binding"]["capture_request_id"] = 1
            gate = self._binding_gate(alignment, lane_events, record)
            self.assertIn("four_lane_same_replay", gate["missing_rules"])

    def test_runtime_montage_binding_rejects_replayed_identity_sequence(self):
        # A reused seqlock sequence means two lanes read the same publication.
        with tempfile.TemporaryDirectory() as tmp:
            alignment, lane_events, record = self._runtime_binding_fixture(Path(tmp))
            lane_events[4][0]["editor_binding"]["identity_sequence"] = \
                lane_events[2][0]["editor_binding"]["identity_sequence"]
            gate = self._binding_gate(alignment, lane_events, record)
            self.assertIn("four_lane_same_replay", gate["missing_rules"])

    def test_runtime_montage_binding_rejects_odd_identity_sequence(self):
        # Odd sequence = seqlock caught mid-write; the snapshot is not stable.
        with tempfile.TemporaryDirectory() as tmp:
            alignment, lane_events, record = self._runtime_binding_fixture(Path(tmp))
            lane_events[2][0]["editor_binding"]["identity_sequence"] = 7
            gate = self._binding_gate(alignment, lane_events, record)
            self.assertIn("four_lane_same_replay", gate["missing_rules"])

    def test_runtime_montage_binding_rejects_missing_master_replay_hash(self):
        # Without a sealed replay hash every equality would compare None to
        # None; the rule must fail closed rather than pass vacuously.
        with tempfile.TemporaryDirectory() as tmp:
            alignment, lane_events, record = self._runtime_binding_fixture(Path(tmp))
            alignment.pop("replay_sha256")
            for lane in range(1, 5):
                lane_events[lane][0].pop("replay_sha256")
            gate = self._binding_gate(alignment, lane_events, record)
            self.assertIn("four_lane_same_replay", gate["missing_rules"])

    def test_rgb_changed_during_encoding_is_rejected(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            capture = root / "capture"
            audio = root / "audio.wav"
            audio.write_bytes(b"sealed-audio")
            artifacts = {}
            events = {}
            for lane in range(1, 5):
                frame = root / f"lane-{lane}-frame-000000.png"
                frame.write_bytes(f"rgb-{lane}".encode("ascii"))
                artifacts[lane] = {
                    "frame_pattern": str(root / f"lane-{lane}-frame-%06d.png"),
                    "frame_count": 1,
                }
                events[lane] = [{
                    "output_path": str(frame.resolve()),
                    "output_bytes": frame.stat().st_size,
                    "output_sha256": sha256_file(frame),
                }]

            def mutate_staging(_ffmpeg, _pattern, out_path, *, n_frames, fps=30):
                self.assertEqual(n_frames, 1)
                staged = root / "media-scratch" / "lane-1-frames" / "frame_000000.png"
                staged.write_bytes(b"tampered-during-encode")
                out_path.write_bytes(b"mock-video")
                return out_path

            with patch("gta_capture.media.encode_frame_sequence",
                       side_effect=mutate_staging):
                with self.assertRaisesRegex(ContractError, "changed during encoding"):
                    materialize_videos(
                        capture, artifacts, {
                            "audio_path": str(audio),
                            "audio_sha256": sha256_file(audio),
                            "audio_bytes": audio.stat().st_size,
                        },
                        ffmpeg="ffmpeg", lane_events=events)

    def test_preencoded_video_cannot_bypass_frame_seal(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            video = root / "preencoded.mp4"
            video.write_bytes(b"unsealed-video")
            audio = root / "audio.wav"
            audio.write_bytes(b"audio")
            artifacts = {lane: {"video_path": str(video)} for lane in range(1, 5)}
            with self.assertRaisesRegex(ContractError, "bypasses per-frame RGB sealing"):
                materialize_videos(
                    root / "capture", artifacts, {
                        "audio_path": str(audio),
                        "audio_sha256": sha256_file(audio),
                        "audio_bytes": audio.stat().st_size,
                    },
                    ffmpeg="ffmpeg", lane_events={lane: [] for lane in range(1, 5)})

    def test_audio_seal_is_checked_before_staging(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            audio = root / "audio.wav"
            audio.write_bytes(b"sealed-audio")
            artifacts = {lane: {"frame_pattern": "unused-%06d.png", "frame_count": 1}
                         for lane in range(1, 5)}
            events = {lane: [] for lane in range(1, 5)}
            with self.assertRaisesRegex(ContractError, "byte count differs"):
                materialize_videos(
                    root / "bad-size" / "capture", artifacts, {
                        "audio_path": str(audio),
                        "audio_sha256": sha256_file(audio),
                        "audio_bytes": audio.stat().st_size + 1,
                    }, ffmpeg="ffmpeg", lane_events=events)
            with self.assertRaisesRegex(ContractError, "sha256 differs"):
                materialize_videos(
                    root / "bad-hash" / "capture", artifacts, {
                        "audio_path": str(audio), "audio_sha256": "0" * 64,
                        "audio_bytes": audio.stat().st_size,
                    }, ffmpeg="ffmpeg", lane_events=events)

    def test_staged_audio_changed_during_mux_is_rejected(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            audio = root / "audio.wav"
            audio.write_bytes(b"sealed-audio")
            artifacts = {}
            events = {}
            for lane in range(1, 5):
                frame = root / f"lane-{lane}-frame-000000.png"
                frame.write_bytes(f"rgb-{lane}".encode("ascii"))
                artifacts[lane] = {
                    "frame_pattern": str(root / f"lane-{lane}-frame-%06d.png"),
                    "frame_count": 1,
                }
                events[lane] = [{
                    "output_path": str(frame.resolve()),
                    "output_bytes": frame.stat().st_size,
                    "output_sha256": sha256_file(frame),
                }]

            def fake_encode(_ffmpeg, _pattern, out_path, *, n_frames, fps=30):
                out_path.write_bytes(b"video")
                return out_path

            def mutate_audio(_ffmpeg, _video, audio_path, out_path, **_kwargs):
                audio_path.write_bytes(b"tampered")
                out_path.parent.mkdir(parents=True, exist_ok=True)
                out_path.write_bytes(b"muxed")
                return out_path

            with patch("gta_capture.media.encode_frame_sequence", side_effect=fake_encode), \
                    patch("gta_capture.media.mux_master_audio", side_effect=mutate_audio):
                with self.assertRaisesRegex(ContractError, "audio changed during"):
                    materialize_videos(
                        root / "capture", artifacts, {
                            "audio_path": str(audio),
                            "audio_sha256": sha256_file(audio),
                            "audio_bytes": audio.stat().st_size,
                        }, ffmpeg="ffmpeg", lane_events=events)

    def test_mux_pads_and_trims_without_shortest_then_verifies_probe(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            commands = []

            def fake_run(command):
                commands.append(command)
                Path(command[-1]).write_bytes(b"muxed")

            with patch("gta_capture.media._run", side_effect=fake_run), \
                    patch("gta_capture.media._probe_media",
                          return_value=self._probe_report()):
                mux_master_audio(
                    "ffmpeg", root / "video.mp4", root / "audio.wav",
                    root / "output.mp4", n_frames=30, fps=30, ffprobe="ffprobe")
            self.assertTrue((root / "output.mp4").is_file())
            command = commands[0]
            self.assertNotIn("-shortest", command)
            filter_value = command[command.index("-filter_complex") + 1]
            self.assertIn("apad", filter_value)
            self.assertIn("atrim=duration=1.000000000000", filter_value)

    def test_mux_probe_rejects_bad_frames_cfr_duration_or_missing_audio(self):
        reports = [
            self._probe_report(frames=29),
            self._probe_report(fps="30000/1001"),
            self._probe_report(duration="1.200000"),
            self._probe_report(audio=False),
        ]
        for report in reports:
            with self.subTest(report=report):
                with self.assertRaises(ContractError):
                    _verify_muxed_output(
                        report, n_frames=30, fps=30, expected_duration_s=1.0)


class StagedPublicationTests(unittest.TestCase):
    @staticmethod
    def _write_valid_wav(path: Path, duration_s: float) -> None:
        rate = 48000
        frames = int(rate * duration_s)
        with wave.open(str(path), "wb") as output:
            output.setnchannels(2)
            output.setsampwidth(2)
            output.setframerate(rate)
            samples = bytearray()
            for index in range(frames):
                value = int(4000 * math.sin(2 * math.pi * 440 * index / rate))
                samples.extend(struct.pack("<hh", value, value))
            output.writeframes(samples)

    def test_finalizer_revalidates_wav_against_sealed_scenario_duration(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            audio_path = root / "audio.wav"
            self._write_valid_wav(audio_path, 0.5)
            audio = {
                "audio_path": str(audio_path),
                "audio_sha256": sha256_file(audio_path),
            }
            forged_report = {
                "audio_path": str(audio_path),
                "audio_sha256": sha256_file(audio_path),
                "replay_sha256": "a" * 64,
                "continuous": True,
                "non_silent": True,
            }
            with self.assertRaisesRegex(ContractError, "revalidation"):
                _revalidate_sealed_audio(
                    audio, forged_report, duration_s=1.0,
                    replay_sha256="a" * 64)

    def test_bad_route_is_rejected_before_immutable_snapshot(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            manifest = DependencyManifest.load(write_json(root / "manifest.json", manifest_dict()))
            scenario_value = scenario_dict()
            scenario_value["location"] = [1.0, 2.0, 3.0]
            scenario = Scenario.load(write_json(root / "scenario.json", scenario_value))
            tape = make_gta_poc_tape(duration_s=1.0, anchor=[1.0, 2.0, 3.0],
                                     route_distance_m=6.0, route_speed_mps=1.0,
                                     route_heading_deg=0.0)
            run_dir = root / "run"
            with self.assertRaisesRegex(ContractError, "route differs"):
                finalize_staged_poc(manifest=manifest, scenario=scenario, tape=tape,
                                    run_dir=run_dir)
            self.assertFalse((run_dir / "config" / "dependency-manifest.json").exists())

    def test_internal_poc_can_retain_only_projection_calibration_failures(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            manifest = DependencyManifest.load(
                write_json(root / "manifest.json", manifest_dict()))
            report = PreflightReport("real", [
                Check("policy.capture_boundary", "PASS", "safe"),
                Check("calibration.binding", "FAIL", "pending"),
                Check("calibration.projection_evidence", "FAIL", "missing"),
                Check("calibration.projection_contract", "FAIL", "unapproved"),
            ])
            with self.assertRaisesRegex(ContractError, "real preflight failed"):
                _preflight_materialization_decision(
                    manifest, report, allow_internal_calibration_gaps=False)
            decision = _preflight_materialization_decision(
                manifest, report, allow_internal_calibration_gaps=True)
        self.assertTrue(decision["used"])
        self.assertFalse(decision["preflight_ok"])
        self.assertFalse(decision["delivery_eligible"])
        self.assertEqual(set(decision["retained_failures"]), {
            "calibration.binding", "calibration.projection_evidence",
            "calibration.projection_contract",
        })

    def test_internal_poc_materialization_does_not_mask_other_failures(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            manifest = DependencyManifest.load(
                write_json(root / "manifest.json", manifest_dict()))
            report = PreflightReport("real", [
                Check("calibration.binding", "FAIL", "pending"),
                Check("game.gta5_exe", "FAIL", "hash mismatch"),
            ])
            with self.assertRaisesRegex(ContractError, "real preflight failed"):
                _preflight_materialization_decision(
                    manifest, report, allow_internal_calibration_gaps=True)

    def test_finalize_lock_is_exclusive(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            run_dir = root / "run"
            run_dir.mkdir()
            (run_dir / ".finalize.lock").write_text("held", encoding="ascii")
            manifest = DependencyManifest.load(write_json(root / "manifest.json", manifest_dict()))
            scenario = Scenario.load(write_json(root / "scenario.json", scenario_dict()))
            with self.assertRaisesRegex(StateError, "already being finalized"):
                finalize_staged_poc(
                    manifest=manifest, scenario=scenario,
                    tape=make_demo_tape(duration_s=1.0), run_dir=run_dir)


if __name__ == "__main__":
    unittest.main()

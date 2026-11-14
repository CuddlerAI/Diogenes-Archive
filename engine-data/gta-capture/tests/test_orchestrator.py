import json
import math
import tempfile
import unittest
from pathlib import Path

from helpers import manifest_dict, scenario_dict, write_json
from gta_capture.backend import DryRunBackend
from gta_capture.errors import BackendError, PolicyError, StateError
from gta_capture.evidence import validate_alignment
from gta_capture.jsonio import read_jsonl, write_jsonl_atomic
from gta_capture.orchestrator import run_capture
from gta_capture.preflight import DependencyManifest
from gta_capture.qc_bridge import package_run
from gta_capture.scenario import Scenario
from gta_capture.tape import make_demo_tape


class DryRunOrchestratorTests(unittest.TestCase):
    def test_complete_dry_run_and_resume(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            manifest = DependencyManifest.load(
                write_json(root / "manifest.json", manifest_dict()))
            scenario = Scenario.load(write_json(root / "scenario.json", scenario_dict()))
            tape = make_demo_tape(duration_s=1.0)
            run_dir = root / "run"
            first = run_capture(manifest=manifest, scenario=scenario, tape=tape,
                                run_dir=run_dir, backend=DryRunBackend(), mode="dry-run")
            second = run_capture(manifest=manifest, scenario=scenario, tape=tape,
                                 run_dir=run_dir, backend=DryRunBackend(), mode="dry-run",
                                 resume=True)

            self.assertEqual(first["run_id"], second["run_id"])
            self.assertTrue(first["dry_run"])
            self.assertFalse(first["delivery_eligible"])
            self.assertEqual(json.loads((run_dir / "state.json").read_text())["status"], "COMPLETE")
            self.assertFalse((run_dir / "capture" / "video_1.mp4").exists())
            action = json.loads((run_dir / "capture" / "action_camera_1.json").read_text())
            self.assertEqual(len(action), 30)
            self.assertEqual(action[0]["frame"], 0)
            self.assertEqual(action[-1]["frame"], 29)
            self.assertNotEqual(action[0]["player_position"],
                                action[0]["camera_position"])
            alignment = json.loads((run_dir / "evidence" / "alignment-report.json").read_text())
            self.assertTrue(alignment["ok"])
            self.assertTrue(alignment["mock"])

    def test_packaging_is_blocked_by_non_trainable_policy(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            manifest = DependencyManifest.load(
                write_json(root / "manifest.json", manifest_dict()))
            scenario = Scenario.load(write_json(root / "scenario.json", scenario_dict()))
            run_dir = root / "run"
            run_capture(manifest=manifest, scenario=scenario,
                        tape=make_demo_tape(duration_s=1.0), run_dir=run_dir,
                        backend=DryRunBackend(), mode="dry-run")
            with self.assertRaises(PolicyError):
                package_run(run_dir, root / "out", game="GTA5.exe",
                            group="2026_08_25_gtapoc_01", prefix="internal-validation")

    def test_failed_lane_can_resume_without_replacing_master(self):
        class FailLaneThree(DryRunBackend):
            def export_lane(self, lane, tape, master, run_dir):
                if lane == 3:
                    raise BackendError("injected lane-3 failure")
                return super().export_lane(lane, tape, master, run_dir)

        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            manifest = DependencyManifest.load(
                write_json(root / "manifest.json", manifest_dict()))
            scenario = Scenario.load(write_json(root / "scenario.json", scenario_dict()))
            tape = make_demo_tape(duration_s=1.0)
            run_dir = root / "run"
            with self.assertRaisesRegex(BackendError, "lane-3"):
                run_capture(manifest=manifest, scenario=scenario, tape=tape,
                            run_dir=run_dir, backend=FailLaneThree(), mode="dry-run")
            failed = json.loads((run_dir / "state.json").read_text())
            master_hash = failed["artifacts"]["master"]["replay_sha256"]
            self.assertEqual(failed["status"], "FAILED")
            self.assertIn("LANE_2_EXPORTED", failed["completed"])

            complete = run_capture(manifest=manifest, scenario=scenario, tape=tape,
                                   run_dir=run_dir, backend=DryRunBackend(), mode="dry-run",
                                   resume=True)
            self.assertEqual(complete["master"]["replay_sha256"], master_hash)
            self.assertEqual(json.loads((run_dir / "state.json").read_text())["status"],
                             "COMPLETE")

    def test_resume_rejects_changed_camera_tape(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            manifest = DependencyManifest.load(
                write_json(root / "manifest.json", manifest_dict()))
            scenario = Scenario.load(write_json(root / "scenario.json", scenario_dict()))
            tape = make_demo_tape(duration_s=1.0)
            run_dir = root / "run"
            run_capture(manifest=manifest, scenario=scenario, tape=tape,
                        run_dir=run_dir, backend=DryRunBackend(), mode="dry-run")
            changed = make_demo_tape(duration_s=1.0, clip_id="different")
            with self.assertRaisesRegex(StateError, "immutable snapshot"):
                run_capture(manifest=manifest, scenario=scenario, tape=changed,
                            run_dir=run_dir, backend=DryRunBackend(), mode="dry-run",
                            resume=True)
            self.assertEqual(json.loads((run_dir / "state.json").read_text())["status"],
                             "COMPLETE")

    def test_alignment_rejects_final_pose_not_applied(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            manifest = DependencyManifest.load(
                write_json(root / "manifest.json", manifest_dict()))
            scenario = Scenario.load(write_json(root / "scenario.json", scenario_dict()))
            tape = make_demo_tape(duration_s=1.0)
            run_dir = root / "run"
            result = run_capture(manifest=manifest, scenario=scenario, tape=tape,
                                 run_dir=run_dir, backend=DryRunBackend(), mode="dry-run")
            event_paths = {lane: result["lane_artifacts"][str(lane)]["frame_events_path"]
                           for lane in range(1, 5)}
            rows = read_jsonl(event_paths[4])
            rows[7]["final_camera"]["position"][0] += 0.01
            write_jsonl_atomic(event_paths[4], rows)
            report, _ = validate_alignment(event_paths, tape, allow_mock=True,
                                           expected_replay_sha256=result["master"]["replay_sha256"])
            self.assertFalse(report.ok)
            self.assertTrue(any("differs from tape" in problem for problem in report.problems))

    def test_alignment_matches_native_single_precision_rotation_boundary(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            manifest = DependencyManifest.load(
                write_json(root / "manifest.json", manifest_dict()))
            scenario = Scenario.load(write_json(root / "scenario.json", scenario_dict()))
            tape = make_demo_tape(duration_s=1.0)
            run_dir = root / "run"
            result = run_capture(manifest=manifest, scenario=scenario, tape=tape,
                                 run_dir=run_dir, backend=DryRunBackend(), mode="dry-run")
            event_paths = {lane: result["lane_artifacts"][str(lane)]["frame_events_path"]
                           for lane in range(1, 5)}
            rows = read_jsonl(event_paths[4])
            original = [float(value) for value in rows[7]["final_camera"]["quaternion"]]
            orthogonal = [-original[1], original[0], -original[3], original[2]]

            def offset_quaternion(angle_deg: float) -> list[float]:
                half = math.radians(angle_deg) / 2.0
                return [original[index] * math.cos(half)
                        + orthogonal[index] * math.sin(half)
                        for index in range(4)]

            rows[7]["final_camera"]["quaternion"] = offset_quaternion(0.04)
            write_jsonl_atomic(event_paths[4], rows)
            report, _ = validate_alignment(
                event_paths, tape, allow_mock=True,
                expected_replay_sha256=result["master"]["replay_sha256"])
            self.assertTrue(report.ok, report.problems)

            rows[7]["final_camera"]["quaternion"] = offset_quaternion(0.06)
            write_jsonl_atomic(event_paths[4], rows)
            report, _ = validate_alignment(
                event_paths, tape, allow_mock=True,
                expected_replay_sha256=result["master"]["replay_sha256"])
            self.assertFalse(report.ok)
            self.assertTrue(any("final rotation differs" in problem
                                for problem in report.problems))

    def test_alignment_rejects_actual_replay_clock_skew(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            manifest = DependencyManifest.load(
                write_json(root / "manifest.json", manifest_dict()))
            scenario = Scenario.load(write_json(root / "scenario.json", scenario_dict()))
            tape = make_demo_tape(duration_s=1.0)
            run_dir = root / "run"
            result = run_capture(manifest=manifest, scenario=scenario, tape=tape,
                                 run_dir=run_dir, backend=DryRunBackend(), mode="dry-run")
            event_paths = {lane: result["lane_artifacts"][str(lane)]["frame_events_path"]
                           for lane in range(1, 5)}
            rows = read_jsonl(event_paths[4])
            rows[7]["replay_time_ms"] += 2.0
            write_jsonl_atomic(event_paths[4], rows)
            report, _ = validate_alignment(event_paths, tape, allow_mock=True,
                                           expected_replay_sha256=result["master"]["replay_sha256"])
            self.assertFalse(report.ok)
            self.assertTrue(any("actual replay clock" in problem
                                or "actual replay time" in problem
                                for problem in report.problems))

    def test_resume_rejects_tampered_master_replay(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            manifest = DependencyManifest.load(
                write_json(root / "manifest.json", manifest_dict()))
            scenario = Scenario.load(write_json(root / "scenario.json", scenario_dict()))
            run_dir = root / "run"
            result = run_capture(manifest=manifest, scenario=scenario,
                                 tape=make_demo_tape(duration_s=1.0), run_dir=run_dir,
                                 backend=DryRunBackend(), mode="dry-run")
            Path(result["master"]["replay_path"]).write_text("tampered", encoding="utf-8")
            with self.assertRaisesRegex(StateError, "master replay sha256 changed"):
                run_capture(manifest=manifest, scenario=scenario,
                            tape=make_demo_tape(duration_s=1.0), run_dir=run_dir,
                            backend=DryRunBackend(), mode="dry-run", resume=True)

    def test_complete_resume_rejects_tampered_manifest(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            manifest = DependencyManifest.load(
                write_json(root / "manifest.json", manifest_dict()))
            scenario = Scenario.load(write_json(root / "scenario.json", scenario_dict()))
            tape = make_demo_tape(duration_s=1.0)
            run_dir = root / "run"
            run_capture(manifest=manifest, scenario=scenario, tape=tape,
                        run_dir=run_dir, backend=DryRunBackend(), mode="dry-run")
            (run_dir / "run-manifest.json").write_text("{}", encoding="utf-8")
            with self.assertRaisesRegex(StateError, "run manifest sha256 changed"):
                run_capture(manifest=manifest, scenario=scenario, tape=tape,
                            run_dir=run_dir, backend=DryRunBackend(), mode="dry-run",
                            resume=True)

    def test_complete_resume_rejects_tampered_metadata(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            manifest = DependencyManifest.load(
                write_json(root / "manifest.json", manifest_dict()))
            scenario = Scenario.load(write_json(root / "scenario.json", scenario_dict()))
            tape = make_demo_tape(duration_s=1.0)
            run_dir = root / "run"
            run_capture(manifest=manifest, scenario=scenario, tape=tape,
                        run_dir=run_dir, backend=DryRunBackend(), mode="dry-run")
            (run_dir / "capture" / "action_camera_2.json").write_text(
                "[]", encoding="utf-8")
            with self.assertRaisesRegex(StateError, "metadata_files .* sha256 changed"):
                run_capture(manifest=manifest, scenario=scenario, tape=tape,
                            run_dir=run_dir, backend=DryRunBackend(), mode="dry-run",
                            resume=True)


if __name__ == "__main__":
    unittest.main()

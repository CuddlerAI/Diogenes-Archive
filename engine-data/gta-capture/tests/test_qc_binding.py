import json
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

from helpers import manifest_dict, scenario_dict, write_json
from gta_capture.errors import ContractError
from gta_capture.gate import REQUIRED_RULES
from gta_capture.jsonio import file_record, write_json_atomic
from gta_capture.qc_bridge import (DELIVERY_FILES, _bind_or_verify_gate,
                                   _qc_bridge_record, _verify_qc_binding,
                                   package_run, run_engine_qc)


def _delivery_policy() -> dict:
    return {
        "purpose": "commercial_delivery",
        "story_mode_only": True,
        "online_forbidden": True,
        "non_trainable": False,
        "upload_enabled": True,
        "customer_replay_sync_accepted": True,
        "customer_metadata_semantics_accepted": True,
        "commercial_rights_approved": True,
    }


def _make_run(root: Path) -> Path:
    run = root / "run"
    config = run / "config"
    capture = run / "capture"
    evidence = run / "evidence"
    config.mkdir(parents=True)
    capture.mkdir()
    evidence.mkdir()

    dependency = manifest_dict()
    dependency["gpu"] = {
        "vendor": "NVIDIA",
        "adapter_name": "fixture-adapter",
        "driver_version": "fixture-driver",
    }
    dependency["policy"] = _delivery_policy()
    write_json(config / "dependency-manifest.json", dependency)
    write_json(config / "scenario.json", scenario_dict())
    write_json(config / "camera-tape.json", {"schema": "test-camera-tape"})
    (config / "camera-tape.bin").write_bytes(b"sealed-native-tape")

    for name in DELIVERY_FILES:
        path = capture / name
        if path.suffix == ".json":
            path.write_text(json.dumps({"fixture": name}), encoding="utf-8")
        else:
            path.write_bytes(("fixture-" + name).encode("ascii"))

    gate = {
        "schema": "dm-gta-pr405-gate/1",
        "ok": True,
        "passed_rules": sorted(REQUIRED_RULES),
        "missing_rules": [],
        "delivery_eligible": True,
        "dry_run": False,
    }
    inputs = {
        "dependency_manifest": file_record(config / "dependency-manifest.json"),
        "scenario": file_record(config / "scenario.json"),
        "camera_tape": file_record(config / "camera-tape.json"),
        "camera_tape_native": file_record(config / "camera-tape.bin"),
    }
    delivery = {name: file_record(capture / name) for name in DELIVERY_FILES}
    manifest = {
        "schema": "dm-gta-run-manifest/1",
        "run_id": "fixture-run-a",
        "dry_run": False,
        "capture_dir": str(capture.resolve()),
        "inputs": inputs,
        "videos": [delivery[f"video_{lane}.mp4"] for lane in range(1, 5)],
        "delivery_files": delivery,
        "gta_pr405_gate": gate,
    }
    write_json_atomic(run / "run-manifest.json", manifest)
    write_json_atomic(evidence / "gta-pr405-gate.json", gate)
    return run


def _write_pass_qc(run: Path) -> dict:
    manifest = json.loads((run / "run-manifest.json").read_text(encoding="utf-8"))
    gate, binding = _bind_or_verify_gate(run, manifest, allow_bind=True)
    qc = {
        "verdict": "PASS",
        "gta_capture_bridge": _qc_bridge_record(
            run, gate, binding, exit_code=0, stderr=""),
    }
    write_json_atomic(run / "evidence" / "engine-qc.json", qc)
    return qc


class QcRunBindingTests(unittest.TestCase):
    def test_engine_qc_binds_gate_report_to_exact_run_and_files(self):
        with tempfile.TemporaryDirectory() as tmp:
            run = _make_run(Path(tmp))
            process = SimpleNamespace(returncode=0, stdout='{"verdict":"PASS"}', stderr="")
            with (patch("gta_capture.qc_bridge.stage_qc_sidecars", return_value=[]),
                  patch("gta_capture.qc_bridge.subprocess.run",
                        return_value=process) as runner):
                code, report = run_engine_qc(run)

            self.assertEqual(code, 0)
            bridge = report["gta_capture_bridge"]
            self.assertEqual(bridge["run_id"], "fixture-run-a")
            self.assertEqual(set(bridge["delivery_files"]), set(DELIVERY_FILES))
            gate = json.loads((run / "evidence" / "gta-pr405-gate.json").read_text())
            self.assertEqual(gate["run_binding"]["run_id"], "fixture-run-a")
            self.assertEqual(gate["run_binding"]["capture_dir"], str((run / "capture").resolve()))
            self.assertEqual(runner.call_args.kwargs["env"]["PYTHONIOENCODING"], "utf-8")

    def test_qc_sidecar_is_part_of_run_binding(self):
        with tempfile.TemporaryDirectory() as tmp:
            run = _make_run(Path(tmp))
            sidecar = run / "capture" / "plan.json"
            sidecar.write_text('{"plan":"sealed"}', encoding="utf-8")
            _write_pass_qc(run)
            sidecar.write_text('{"plan":"changed"}', encoding="utf-8")
            manifest = json.loads((run / "run-manifest.json").read_text(encoding="utf-8"))
            with self.assertRaisesRegex(ContractError, "run_binding differs"):
                _bind_or_verify_gate(run, manifest, allow_bind=False)

    def test_binding_rejects_delivery_file_changed_after_qc(self):
        # Must hit the binding logic itself, not the earlier package hard-block.
        with tempfile.TemporaryDirectory() as tmp:
            run = _make_run(Path(tmp))
            _write_pass_qc(run)
            (run / "capture" / "video_2.mp4").write_bytes(b"post-qc-tamper")

            manifest = json.loads((run / "run-manifest.json").read_text(encoding="utf-8"))
            with self.assertRaisesRegex(ContractError, "differs from its sealed bytes"):
                _bind_or_verify_gate(run, manifest, allow_bind=False)

    def test_binding_rejects_qc_report_from_another_run(self):
        with tempfile.TemporaryDirectory() as tmp:
            run = _make_run(Path(tmp))
            qc = _write_pass_qc(run)
            qc["gta_capture_bridge"]["run_id"] = "fixture-run-b"
            write_json_atomic(run / "evidence" / "engine-qc.json", qc)

            manifest = json.loads((run / "run-manifest.json").read_text(encoding="utf-8"))
            gate, binding = _bind_or_verify_gate(run, manifest, allow_bind=False)
            qc = json.loads((run / "evidence" / "engine-qc.json").read_text(encoding="utf-8"))
            with self.assertRaisesRegex(ContractError, "run_id differs"):
                _verify_qc_binding(run, qc, gate, binding)

    def test_binding_rejects_gate_review_changes_after_seal(self):
        with tempfile.TemporaryDirectory() as tmp:
            run = _make_run(Path(tmp))
            _write_pass_qc(run)
            gate_path = run / "evidence" / "gta-pr405-gate.json"
            gate = json.loads(gate_path.read_text(encoding="utf-8"))
            gate["human_review_evidence"] = {"report_sha256": "a" * 64}
            write_json_atomic(gate_path, gate)

            manifest = json.loads((run / "run-manifest.json").read_text(encoding="utf-8"))
            with self.assertRaisesRegex(ContractError, "differs from its finalized run manifest copy"):
                _bind_or_verify_gate(run, manifest, allow_bind=False)

    def test_binding_rejects_forged_ok_and_passed_rules_after_seal(self):
        # Regression: the verify path must re-check gate content, not only
        # run_binding, or flipping ok/passed_rules after binding slips through.
        with tempfile.TemporaryDirectory() as tmp:
            run = _make_run(Path(tmp))
            _write_pass_qc(run)
            gate_path = run / "evidence" / "gta-pr405-gate.json"
            gate = json.loads(gate_path.read_text(encoding="utf-8"))
            gate["ok"] = True
            gate["passed_rules"] = sorted(REQUIRED_RULES | {"gta.forged_rule"})
            gate["missing_rules"] = []
            write_json_atomic(gate_path, gate)

            manifest = json.loads((run / "run-manifest.json").read_text(encoding="utf-8"))
            with self.assertRaisesRegex(ContractError, "differs from its finalized run manifest copy"):
                _bind_or_verify_gate(run, manifest, allow_bind=False)

    def test_fake_full_pass_gate_cannot_enable_packaging(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            run = _make_run(root)
            _write_pass_qc(run)

            with patch("gta_capture.qc_bridge.repository_profile_non_trainable",
                       return_value=False):
                with self.assertRaisesRegex(ContractError, "hard-blocked"):
                    package_run(run, root / "out", game="GTA5.exe",
                                group="2026_08_25_gtapoc_01",
                                prefix="internal-validation")


if __name__ == "__main__":
    unittest.main()

import json
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from helpers import manifest_dict, write_json
from gta_capture.gate import build_pr405_gate
from gta_capture.jsonio import sha256_file
from gta_capture.policy import CapturePolicy
from gta_capture.preflight import (DependencyManifest, load_projection_validation,
                                   load_render_profile_binding,
                                   _actual_windows_authenticode,
                                   run_preflight, _actual_windows_file_version,
                                   _approved_game_root_asi_names,
                                   _approved_reshade_signature,
                                   _check_capture_build_receipt,
                                   _windows_powershell_path)


def projection_evidence(*, error_px=0.5, sample_count=120, metric_scale=1.0):
    return {
        "schema": "dm-gta-projection-calibration/1",
        "approval_status": "approved",
        "calibration_id": "test-calibration-v1",
        "filmback_id": "test-36mm",
        "metric_scale": metric_scale,
        "game_build": "1.0.3889.0",
        "render": {"width": 1920, "height": 1080, "graphics_api": "DX11"},
        "reprojection": {
            "metric": "rms_euclidean_px",
            "error_px": error_px,
            "sample_count": sample_count,
        },
    }


def check_by_id(report, check_id):
    return next(item for item in report.checks if item.check_id == check_id)


class ProjectionCalibrationTests(unittest.TestCase):
    def _run(self, evidence):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        root = Path(temp.name)
        evidence_path = write_json(root / "projection.json", evidence)
        manifest = manifest_dict()
        manifest["calibration"]["projection_evidence"] = {
            "path": str(evidence_path), "sha256": sha256_file(evidence_path),
        }
        manifest_path = write_json(root / "manifest.json", manifest)
        loaded = DependencyManifest.load(manifest_path)
        report = run_preflight(loaded, mode="real")
        return loaded, report

    def test_approved_projection_metrics_are_parsed_and_bound(self):
        manifest, report = self._run(projection_evidence())
        check = check_by_id(report, "calibration.projection_contract")
        self.assertEqual(check.status, "PASS")
        self.assertEqual(load_projection_validation(
            manifest.raw["calibration"]).sample_count, 120)
        gate = build_pr405_gate(
            alignment={}, lanes={}, audio_report={},
            calibration=manifest.raw["calibration"], policy=manifest.policy,
            dry_run=False,
        )
        self.assertIn("projection_calibration", gate["passed_rules"])

        evidence_path = Path(manifest.raw["calibration"]["projection_evidence"]["path"])
        write_json(evidence_path, projection_evidence(error_px=0.25))
        gate = build_pr405_gate(
            alignment={}, lanes={}, audio_report={},
            calibration=manifest.raw["calibration"], policy=manifest.policy,
            dry_run=False,
        )
        self.assertIn("projection_calibration", gate["missing_rules"])

    def test_projection_rejects_error_sample_and_scale_mismatches(self):
        cases = (
            projection_evidence(error_px=1.01),
            projection_evidence(sample_count=99),
            projection_evidence(metric_scale=0.5),
        )
        for evidence in cases:
            with self.subTest(evidence=evidence):
                manifest, report = self._run(evidence)
                self.assertEqual(check_by_id(report, "calibration.projection_contract").status,
                                 "FAIL")
                self.assertIsNone(load_projection_validation(manifest.raw["calibration"]))

    def test_projection_rejects_unapproved_or_mismatched_ids(self):
        evidence = projection_evidence()
        evidence["approval_status"] = "pending"
        evidence["filmback_id"] = "other-filmback"
        manifest, report = self._run(evidence)
        self.assertEqual(check_by_id(report, "calibration.projection_contract").status, "FAIL")
        self.assertIsNone(load_projection_validation(manifest.raw["calibration"]))


class VersionAndGateTests(unittest.TestCase):
    def test_reshade_self_signed_chain_is_narrowly_accepted(self):
        approved = {
            "status": "UnknownError",
            "thumbprint": "589690208A5E52FB96980C4A6698F50ACD47C49F",
            "chain_status": ["UntrustedRoot"],
        }
        self.assertTrue(_approved_reshade_signature(approved))
        string_chain = dict(approved)
        string_chain["chain_status"] = "UntrustedRoot"
        self.assertTrue(_approved_reshade_signature(string_chain))
        for bad in ("BadDigest", "NotSigned", "UnknownError"):
            value = dict(approved)
            value["status"] = bad
            value["chain_status"] = ([] if bad != "UnknownError" else ["NotTimeValid"])
            self.assertFalse(_approved_reshade_signature(value))

        trusted = dict(approved, status="Valid", chain_status=[])
        self.assertTrue(_approved_reshade_signature(trusted))
        trusted["chain_status"] = ["NotTimeValid"]
        self.assertFalse(_approved_reshade_signature(trusted))

    @patch("gta_capture.preflight.platform.system", return_value="Windows")
    @patch("gta_capture.preflight.shutil.which", return_value="pwsh")
    @patch("gta_capture.preflight.subprocess.run")
    def test_gta_version_falls_back_to_product_version(self, run, _which, _system):
        run.return_value.returncode = 0
        run.return_value.stdout = (
            '{"file_version":"","product_version":"1.0.3889.0"}'
        )
        run.return_value.stderr = ""
        self.assertEqual(_actual_windows_file_version(Path("GTA5.exe")), "1.0.3889.0")

        run.return_value.stdout = (
            '{"file_version":"","product_version":"11.0.3889.0"}'
        )
        self.assertEqual(_actual_windows_file_version(Path("GTA5.exe")), "11.0.3889.0")

    @patch("gta_capture.preflight.platform.system", return_value="Windows")
    @patch("gta_capture.preflight.shutil.which")
    def test_windows_probe_resolves_powershell_exe_only(self, which, _system):
        which.side_effect = lambda name: (
            r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe"
            if name == "powershell.exe" else None
        )
        self.assertEqual(
            _windows_powershell_path(),
            r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe",
        )

    @patch("gta_capture.preflight.platform.system", return_value="Windows")
    @patch("gta_capture.preflight.shutil.which")
    @patch("gta_capture.preflight.subprocess.run")
    def test_windows_probes_pass_x86_path_through_private_environment(
            self, run, which, _system):
        which.side_effect = lambda name: (
            r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe"
            if name == "powershell.exe" else None
        )
        run.return_value.returncode = 0
        run.return_value.stdout = json.dumps({
            "file_major": 1, "file_minor": 0, "file_build": 3889,
            "file_private": 0, "file_version": "", "product_version": "",
        })
        run.return_value.stderr = ""
        target = Path(r"C:\Program Files (x86)\Steam\GTA5.exe")
        self.assertEqual(_actual_windows_file_version(target), "1.0.3889.0")
        command = run.call_args.args[0]
        environment = run.call_args.kwargs["env"]
        self.assertNotIn(str(target), command)
        self.assertEqual(environment["DIOGENES_CAPTURE_PROBE_PATH"], str(target))

        run.return_value.stdout = json.dumps({
            "status": "UnknownError",
            "thumbprint": "589690208A5E52FB96980C4A6698F50ACD47C49F",
            "chain_status": ["UntrustedRoot"],
        })
        signature = _actual_windows_authenticode(target)
        self.assertTrue(_approved_reshade_signature(signature))
        self.assertEqual(
            run.call_args.kwargs["env"]["DIOGENES_CAPTURE_PROBE_PATH"], str(target))

    def test_reshade64_asi_is_allowed_only_when_pinned_in_game_root(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp).resolve()
            manifest = manifest_dict()
            manifest["game"]["path"] = str(root / "GTA5.exe")
            manifest["components"]["reshade_dxgi"]["path"] = str(root / "ReShade64.asi")
            loaded = DependencyManifest.load(write_json(root / "manifest.json", manifest))
            allowed = _approved_game_root_asi_names(loaded, root / "GTA5.exe")
            self.assertIn("reshade64.asi", allowed)

            manifest["components"]["reshade_dxgi"]["path"] = str(
                root / "unapproved" / "ReShade64.asi")
            loaded = DependencyManifest.load(write_json(root / "manifest.json", manifest))
            self.assertNotIn("reshade64.asi", _approved_game_root_asi_names(
                loaded, root / "GTA5.exe"))

    def test_real_preflight_locks_exact_nvidia_driver(self):
        inventory = [{
            "adapter_name": "NVIDIA Test Adapter",
            "driver_version": "32.0.15.9000",
            "pnp_device_id": "PCI\\VEN_10DE&DEV_0001",
        }]
        with tempfile.TemporaryDirectory() as tmp:
            manifest_path = write_json(Path(tmp) / "manifest.json", manifest_dict())
            with patch("gta_capture.preflight._actual_windows_gpu_inventory",
                       return_value=inventory):
                report = run_preflight(DependencyManifest.load(manifest_path), mode="real")
            self.assertEqual(check_by_id(report, "environment.gpu_driver_lock").status, "PASS")

            mismatch = manifest_dict()
            mismatch["gpu"]["driver_version"] = "32.0.15.9999"
            manifest_path = write_json(Path(tmp) / "mismatch.json", mismatch)
            with patch("gta_capture.preflight._actual_windows_gpu_inventory",
                       return_value=inventory):
                report = run_preflight(DependencyManifest.load(manifest_path), mode="real")
            self.assertEqual(check_by_id(report, "environment.gpu_driver_lock").status, "FAIL")

    def test_radio_off_self_report_without_readback_evidence_does_not_pass(self):
        policy = CapturePolicy.from_dict(manifest_dict()["policy"])
        audio = {
            "schema": "dm-gta-audio-report/2",
            "duration_integrity": True,
            "active_coverage": True,
            "max_silence_ok": True,
            "clipping_ok": True,
            "continuous": True,
            "non_silent": True,
            "radio_off": True,
            "radio_off_evidence": None,
            "rights_checked": True,
            "rights_evidence": None,
            "replay_sha256": "a" * 64,
            "audio_sha256": "b" * 64,
            "duration_expected_s": 10.0,
        }
        alignment = {"replay_sha256": "a" * 64}
        gate = build_pr405_gate(
            alignment=alignment, lanes={}, audio_report=audio, calibration={},
            policy=policy, dry_run=False,
        )
        self.assertIn("audio_continuity", gate["passed_rules"])
        self.assertIn("audio_rights", gate["missing_rules"])

        audio["radio_off_evidence"] = {
            "schema": "dm-gta-radio-state-evidence/1",
            "method": "gta-native-radio-readback",
            "all_samples_off": True,
            "sample_count": 300,
            "sha256": "0" * 64,
        }
        gate = build_pr405_gate(
            alignment={}, lanes={}, audio_report=audio, calibration={},
            policy=policy, dry_run=False,
        )
        self.assertIn("audio_rights", gate["missing_rules"])

        with tempfile.TemporaryDirectory() as tmp:
            evidence_path = write_json(Path(tmp) / "radio.json", {
                "schema": "dm-gta-radio-state-evidence/1",
                "method": "gta-native-radio-readback",
                "replay_sha256": "a" * 64,
                "coverage_start": "scenario_applied",
                "coverage_end": "audio_sealed",
                "expected_replay_duration_s": 10.0,
                "all_samples_off": True,
                "sample_count": 300,
            })
            audio["radio_off_evidence"] = {
                "path": str(evidence_path), "sha256": sha256_file(evidence_path),
            }
            gate = build_pr405_gate(
                alignment=alignment, lanes={}, audio_report=audio, calibration={},
                policy=policy, dry_run=False,
            )
            self.assertIn("audio_rights", gate["missing_rules"])

            rights_path = write_json(Path(tmp) / "audio-rights.json", {
                "schema": "dm-gta-audio-rights-evidence/1",
                "approval_status": "approved",
                "allowed_use": "internal_nontrainable_poc",
                "replay_sha256": "a" * 64,
                "audio_sha256": "b" * 64,
                "radio_off_required": True,
                "approved_by": "test-reviewer",
                "approved_at": "2026-08-25T00:00:00Z",
            })
            audio["rights_evidence"] = {
                "path": str(rights_path), "sha256": sha256_file(rights_path),
            }
            gate = build_pr405_gate(
                alignment=alignment, lanes={}, audio_report=audio, calibration={},
                policy=policy, dry_run=False,
            )
            # A caller-authored approval file is not trusted authorization.
            self.assertIn("audio_rights", gate["missing_rules"])

    def test_capture_build_receipt_binds_dynamic_pe_hashes(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            manifest = manifest_dict()
            artifacts = {}
            for name in ("replus_asi", "replus_capture_addon", "diogenes_controller_asi",
                         "replus_main_ini", "replus_render_ini", "replus_lights_ini",
                         "replus_settings_exe", "igcs_dof_shader"):
                path = root / f"{name}.bin"
                path.write_bytes((name + "-actual-build").encode("ascii"))
                manifest["components"][name] = {
                    "path": str(path), "sha256": sha256_file(path),
                }
                artifacts[name] = {
                    "path": str(path), "bytes": path.stat().st_size,
                    "sha256": sha256_file(path),
                }
            receipt = {
                "schema": "dm-gta-capture-build-receipt/1",
                "started_at": "2026-08-25T00:00:00Z",
                "finished_at": "2026-08-25T00:01:00Z",
                "sources": {
                    "replus_commit": manifest["source_pins"]["replus_commit"],
                    "replus_patch_sha256": manifest["source_pins"]["replus_patch_sha256"],
                    "igcs_fork_commit": manifest["source_pins"]["igcs_fork_commit"],
                    "igcs_patch_sha256": manifest["source_pins"]["igcs_patch_sha256"],
                    "controller_git_commit": manifest["source_pins"]["controller_git_commit"],
                },
                "toolchain": {
                    "cmake_path": r"C:\Program Files\CMake\bin\cmake.exe",
                    "cmake_version": "cmake version 4.4.2",
                    "msbuild_path": (
                        r"C:\Program Files\Microsoft Visual Studio\MSBuild.exe"),
                    "msbuild_version": "17.14.23",
                    "commands": [
                        (r"C:\Program Files\CMake\bin\cmake.exe --build "
                         r"C:\staging\replus-build --config Release --clean-first --parallel 4"),
                        (r"C:\Program Files\Microsoft Visual Studio\MSBuild.exe "
                         r"C:\staging\IgcsConnector.sln /t:Rebuild /m "
                         r"/p:Configuration=Release /p:Platform=x64 /nologo"),
                        (r"C:\Program Files\CMake\bin\cmake.exe --build "
                         r"C:\staging\controller-build --config Release --clean-first --parallel 4"),
                    ],
                    "invocations": [
                        {
                            "executable": r"C:\Program Files\CMake\bin\cmake.exe",
                            "argv": ["--build", r"C:\staging\replus-build", "--config",
                                     "Release", "--clean-first", "--parallel", "4"],
                        },
                        {
                            "executable": (
                                r"C:\Program Files\Microsoft Visual Studio\MSBuild.exe"),
                            "argv": [r"C:\staging\IgcsConnector.sln", "/t:Rebuild", "/m",
                                     "/p:Configuration=Release", "/p:Platform=x64", "/nologo"],
                        },
                        {
                            "executable": r"C:\Program Files\CMake\bin\cmake.exe",
                            "argv": ["--build", r"C:\staging\controller-build", "--config",
                                     "Release", "--clean-first", "--parallel", "4"],
                        },
                    ],
                    "configuration": "Release",
                    "platform": "x64",
                },
                "configuration_transforms": {
                    "replus_render_ini": {
                        "schema": "dm-gta-replus-render-profile/1",
                        "algorithm": "replace-unique-keys-utf8-crlf/1",
                        "input": {"bytes": 100, "sha256": "1" * 64},
                        "settings": {
                            "EnableRenderer": "1",
                            "RenderCaptureMode": "Walking",
                            "RenderDepthOfField": "0",
                            "RenderFps": "30",
                            "RenderSamples": "1",
                            "RenderHighlightBoost": "0.9",
                            "RenderSettleFrames": "3",
                            "RenderSettleSubFrames": "1",
                            "RenderJpeg": "0",
                            "RenderHideHud": "1",
                            "ExportCloseWhenDone": "1",
                            "RenderOutputFolder": str((root / "frames").resolve()),
                            "RenderMode": "Frames",
                            "RenderKeepFrames": "1",
                            "RenderAudio": "1",
                            "RenderMarkerSpeed": "1",
                        },
                        "output": {
                            "bytes": artifacts["replus_render_ini"]["bytes"],
                            "sha256": artifacts["replus_render_ini"]["sha256"],
                        },
                    },
                },
                "artifacts": artifacts,
            }
            receipt_path = write_json(root / "build-receipt.json", receipt)
            manifest["components"]["capture_build_receipt"] = {
                "path": str(receipt_path), "sha256": sha256_file(receipt_path),
            }
            loaded = DependencyManifest.load(write_json(root / "manifest.json", manifest))
            self.assertEqual(_check_capture_build_receipt(loaded).status, "PASS")
            binding = load_render_profile_binding(loaded)
            self.assertEqual(binding.render_profile_sha256,
                             artifacts["replus_render_ini"]["sha256"])
            self.assertEqual(Path(binding.expected_output_root),
                             (root / "frames").resolve())

            original_command = receipt["toolchain"]["commands"][0]
            receipt["toolchain"]["commands"][0] = "cmake --build <placeholder>"
            write_json(receipt_path, receipt)
            self.assertEqual(_check_capture_build_receipt(loaded).status, "FAIL")
            receipt["toolchain"]["commands"][0] = original_command

            original_replus_sha = artifacts["replus_asi"]["sha256"]
            receipt["artifacts"]["replus_asi"]["sha256"] = "0" * 64
            write_json(receipt_path, receipt)
            self.assertEqual(_check_capture_build_receipt(loaded).status, "FAIL")

            receipt["artifacts"]["replus_asi"]["sha256"] = original_replus_sha
            receipt["configuration_transforms"]["replus_render_ini"]["settings"][
                "RenderFps"] = "60"
            write_json(receipt_path, receipt)
            self.assertEqual(_check_capture_build_receipt(loaded).status, "FAIL")


class RenderProfileScriptTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.powershell = shutil.which("pwsh") or shutil.which("powershell")

    def _run_powershell(self, script):
        if self.powershell is None:
            self.skipTest("PowerShell is unavailable")
        return subprocess.run(
            [self.powershell, "-NoProfile", "-NonInteractive", "-Command", script],
            text=True, encoding="utf-8", errors="replace",
            capture_output=True, check=False,
        )

    def test_render_profile_transform_is_sealed_and_tamper_evident(self):
        helper = Path(__file__).parents[1] / "scripts" / "render-profile.ps1"
        helper_ps = str(helper).replace("'", "''")
        settings = {
            "EnableRenderer": "0", "RenderCaptureMode": "Realtime",
            "RenderDepthOfField": "1", "RenderFps": "60", "RenderSamples": "8",
            "RenderHighlightBoost": "0.0",
            "RenderSettleFrames": "0", "RenderSettleSubFrames": "0",
            "RenderJpeg": "1", "RenderHideHud": "0", "ExportCloseWhenDone": "0",
            "RenderOutputFolder": "old", "RenderMode": "Video",
            "RenderKeepFrames": "0", "RenderAudio": "0", "RenderMarkerSpeed": "0",
        }
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            ini = root / "Render.ini"
            ini.write_bytes(("[Render]\n" + "\n".join(
                f"{key}={value}" for key, value in settings.items()) + "\n").encode())
            ini_ps = str(ini).replace("'", "''")
            output_folder = str(root / "frames").replace("'", "''")
            command = f"""
                $ErrorActionPreference = 'Stop'
                . '{helper_ps}'
                $record = Set-DiogenesRePlusRenderProfile '{ini_ps}' '{output_folder}'
                $roundTrip = ($record | ConvertTo-Json -Depth 8) | ConvertFrom-Json
                $artifact = [pscustomobject]@{{
                    bytes = $roundTrip.output.bytes
                    sha256 = $roundTrip.output.sha256
                }}
                Assert-DiogenesRePlusRenderProfile '{ini_ps}' $roundTrip $artifact '{output_folder}'
                $roundTrip | ConvertTo-Json -Depth 8 -Compress
            """
            result = self._run_powershell(command)
            self.assertEqual(result.returncode, 0, result.stderr)
            record = json.loads(result.stdout.strip().splitlines()[-1])
            self.assertEqual(record["settings"]["RenderFps"], "30")
            self.assertEqual(record["settings"]["RenderHighlightBoost"], "0.9")
            self.assertEqual(record["output"]["sha256"], sha256_file(ini))
            self.assertNotEqual(record["input"]["sha256"], record["output"]["sha256"])
            self.assertTrue(ini.read_bytes().endswith(b"\r\n"))

            record_path = write_json(root / "record.json", record)
            ini.write_bytes(ini.read_bytes() + b"# tamper\r\n")
            record_ps = str(record_path).replace("'", "''")
            tamper_command = f"""
                $ErrorActionPreference = 'Stop'
                . '{helper_ps}'
                $record = Get-Content -LiteralPath '{record_ps}' -Raw | ConvertFrom-Json
                Assert-DiogenesRePlusRenderProfile '{ini_ps}' $record $record.output '{output_folder}'
            """
            tampered = self._run_powershell(tamper_command)
            self.assertNotEqual(tampered.returncode, 0)


class IgcsPreOverlayCapturePatchTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        patch_path = (Path(__file__).parents[1] / "patches" /
                      "igcsconnector-v18-evidence-fence.patch")
        cls.patch_text = patch_path.read_text(encoding="utf-8")

    def test_protocol_ownership_remains_on_present(self):
        patch_text = self.patch_text
        protocol_start = patch_text.index("+static void sc_fxProtocolTick")
        capture_start = patch_text.index("+static void sc_fxCaptureTick")
        protocol = patch_text[protocol_start:capture_start]
        self.assertIn("g_scFxBlock->addonHeartbeat++", protocol)
        self.assertIn("g_scFxPendingAck != 0", protocol)
        self.assertIn("observerReleaseId", protocol)
        self.assertIn("ackId", protocol)
        self.assertIn("g_scFxNotifyAfterAck", protocol)

        ready_start = patch_text.index("static void sc_fxMarkCaptureReady")
        ready_end = patch_text.index("// Where the depth-of-field bridge", ready_start)
        ready = patch_text[ready_start:ready_end]
        self.assertIn("g_scFxPendingAck = request", ready)
        self.assertNotIn("InterlockedCompareExchangePointer", ready)
        self.assertNotIn("ackId", ready)

    def test_pixel_readback_precedes_every_overlay_renderer(self):
        patch_text = self.patch_text
        present_start = patch_text.index("static void onReshadePresent")
        overlay_start = patch_text.index("static void onReshadeOverlay", present_start)
        present = patch_text[present_start:overlay_start]
        overlay = patch_text[overlay_start:]

        self.assertIn("+\tsc_fxProtocolTick(runtime);", present)
        self.assertNotIn("+\tsc_fxCaptureTick(runtime);", present)
        capture = overlay.index("sc_fxCaptureTick(runtime);")
        screenshot = overlay.index(
            "g_screenshotController.reshadeEffectsRendered(runtime);")
        addon_overlay = overlay.index("OverlayControl::renderOverlay();")
        dof_overlay = overlay.index("g_depthOfFieldController.renderOverlay();")
        self.assertLess(capture, screenshot)
        self.assertLess(capture, addon_overlay)
        self.assertLess(capture, dof_overlay)

    def test_pixel_stage_cannot_advance_ack_or_heartbeat(self):
        patch_text = self.patch_text
        capture_start = patch_text.index("+static void sc_fxCaptureTick")
        capture_end = patch_text.index("// externs for reshade", capture_start)
        capture = patch_text[capture_start:capture_end]
        self.assertIn("g_scFxPendingAck != 0", capture)
        self.assertIn("sc_fxMarkCaptureReady(req);", capture)
        self.assertNotIn("addonHeartbeat++", capture)
        self.assertNotIn("observerReleaseId", capture)
        self.assertNotIn("+\t\tg_scFxBlock->ackId", capture)
        self.assertNotIn("+\tg_scFxBlock->ackId", capture)


class RePlusCursorGuardPatchTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        patch_path = (Path(__file__).parents[1] / "patches" /
                      "replus-v18-evidence-fence.patch")
        cls.patch_text = patch_path.read_text(encoding="utf-8")

    def test_authorized_cursor_guard_is_client_only_and_restorable(self):
        patch_text = self.patch_text
        self.assertIn("message == WM_SETCURSOR && LOWORD(lParam) == HTCLIENT",
                      patch_text)
        self.assertIn("processId != GetCurrentProcessId()", patch_text)
        self.assertNotIn("windowThread != GetCurrentThreadId()", patch_text)
        self.assertIn("std::atomic_bool s_cursorWndProcArmed", patch_text)
        self.assertIn("s_cursorOriginalWndProc.load(std::memory_order_acquire)",
                      patch_text)
        self.assertIn("window, GWLP_WNDPROC, reinterpret_cast<LONG_PTR>(cursorWndProc)",
                      patch_text)
        self.assertIn("window, GWLP_WNDPROC, reinterpret_cast<LONG_PTR>(original)",
                      patch_text)
        self.assertIn("cursorWndProcDisarm();\n+\t\t\tosCursorRestore();", patch_text)

    def test_cursor_guard_failure_rejects_the_frame_pass(self):
        patch_text = self.patch_text
        self.assertIn("if (!suppressCursorForExportStart())", patch_text)
        self.assertIn(
            'finish("aborted - cursor guard unavailable before frame pass", false);',
            patch_text)
        self.assertIn("if (s_step == Step::Idle)\n+\t\t{\n+\t\t\tcursorSuppressionEnd();",
                      patch_text)

    def test_audio_to_frame_transition_never_restores_pointer_scaleform(self):
        patch_text = self.patch_text
        audio_branch = patch_text.index("if (s_audioPass)")
        terminal_restore = patch_text.index(
            "// From here on finish() is terminal", audio_branch)
        early_restore = patch_text.find(
            "game::setCursorVisible(true);", audio_branch, terminal_restore)
        self.assertGreaterEqual(early_restore, 0)
        self.assertIn("if (s_autoOpen == 0)",
                      patch_text[audio_branch:terminal_restore])
        self.assertIn(
            "successful audio -> rewind/frame transition must never recreate",
            patch_text[audio_branch:terminal_restore])
        removed_unconditional = (
            "-\t\t\t// Unconditional: an aborted render must not leave the editor")
        self.assertIn(removed_unconditional, patch_text)
        self.assertGreater(
            patch_text.index("game::setCursorVisible(true);", terminal_restore),
            terminal_restore)

        frame_handoff = patch_text.index(
            "render: back at clip 1 - starting the frame pass")
        frame_start = patch_text.index("s_step = Step::Idle;", frame_handoff)
        handoff_guard = patch_text[frame_handoff:frame_start]
        self.assertIn(
            "Rewinding from the real-time audio pass rebuilds the editor frontend",
            handoff_guard)
        self.assertIn("if (!suppressCursorForExportStart())", handoff_guard)
        self.assertIn(
            'finish("aborted - cursor guard unavailable before frame pass", false);',
            handoff_guard)

    def test_armed_plan_focuses_one_stock_export_action_without_input_edge(self):
        patch_text = self.patch_text
        self.assertIn("bool focusUniqueStartExportAction()", patch_text)
        self.assertIn("if (match >= 0) return false;", patch_text)
        self.assertIn("((int*)game::addr_g_VECurrentItem)[col] = match;",
                      patch_text)
        self.assertIn("h.state == PLAN_ARMED && !evidence.renderActive",
                      patch_text)
        self.assertIn("exportmenu::focusStartExportForArmedPlan();", patch_text)
        self.assertIn("Selection alone is inert", patch_text)
        self.assertIn("fresh identities and publish TRIGGERED", patch_text)

    def test_triggered_plan_preserves_stock_confirmation_lifecycle(self):
        patch_text = self.patch_text
        menu_start = patch_text.index("+\tvoid tickMenuIdentity()")
        menu_end = patch_text.index("diff --git", menu_start)
        menu = patch_text[menu_start:menu_end]
        self.assertIn("ready for stock Export confirmation", menu)
        self.assertIn("h.state != PLAN_TRIGGERED", menu)
        self.assertIn("h.publisherPid != GetCurrentProcessId()", menu)
        self.assertIn("foregroundPid != GetCurrentProcessId()", menu)
        self.assertNotIn("game::openPlayback(", menu)
        self.assertNotIn("render::suppressCursorForExportStart()", menu)
        self.assertIn("s_menuCursorSuppression.store(false", menu)


if __name__ == "__main__":
    unittest.main()

"""Finite v4 -> v3 -> v2 -> v1 chain; synthetic build files, real validators."""

import copy
from pathlib import Path
import unittest
from unittest.mock import patch

from helpers import write_json
import test_controller_receipt as controller_tests
import test_replus_receipt as replus_tests
from test_controller_receipt import record
from gta_capture import controller_replus_receipt as v4
from gta_capture import preflight
from gta_capture.errors import ContractError
from gta_capture.jsonio import load_json, sha256_json
from gta_capture.preflight import DependencyManifest, _check_capture_build_receipt, load_render_profile_binding


class ControllerOnReplusReceiptTests(unittest.TestCase):
    def setUp(self):
        self.replus = replus_tests.ReplusReceiptTests()
        self.replus.setUp()
        self.addCleanup(self.replus.doCleanups)
        # Deployment is a copy, not the immutable historical build artifact.
        # Replacing it must leave the ancestor's actual build bytes available.
        live = self.replus.root / "installed-controller.asi"
        live.write_bytes(self.replus.parent.component_path("diogenes_controller_asi").read_bytes())
        installed_record = record(live)
        self.replus.parent_raw["components"]["diogenes_controller_asi"] = installed_record
        write_json(self.replus.parent_manifest_path, self.replus.parent_raw)
        self.replus.receipt["parent"]["manifest"] = record(self.replus.parent_manifest_path)
        self.replus.current["components"]["diogenes_controller_asi"] = installed_record
        self.parent = self.replus.loaded()
        self.controller = controller_tests.ControllerReceiptTests()
        self.controller.setUp()
        self.addCleanup(self.controller.doCleanups)
        self.root = self.controller.root
        self.build = self.controller.build
        # Reuse the parent's exact SDK ZIP, whose container timestamps may
        # differ from another fixture ZIP even when extracted inputs match.
        self.build["sdk_archive"] = copy.deepcopy(self.replus.parent_fixture.build["sdk_archive"])
        artifact_path = Path(self.build["artifact"]["path"])
        artifact_path.write_bytes(b"synthetic subsequent controller ASI")
        self.build["artifact"] = record(artifact_path)
        self.build["deployed"] = False
        parent_receipt = self.parent.component_path("capture_build_receipt")
        self.build["parent_capture_build_receipt"] = record(parent_receipt)
        original_asi = self.parent.component_path("diogenes_controller_asi")
        backup = self.root / "preserved-controller.asi"
        backup.write_bytes(original_asi.read_bytes())
        self.receipt = copy.deepcopy(self.controller.receipt)
        self.receipt.update(schema=v4.CONTROLLER_REPLUS_DERIVED_SCHEMA,
                            kind=v4.CONTROLLER_REPLUS_KIND,
                            scope=copy.deepcopy(v4.CONTROLLER_REPLUS_SCOPE))
        self.receipt["parent"] = {"manifest": record(self.parent.path),
                                  "receipt": record(parent_receipt), "controller_backup": record(backup)}
        self.current = copy.deepcopy(self.parent.raw)
        self.current["components"]["diogenes_controller_asi"] = self.build["artifact"]
        self.current["source_pins"]["controller_git_commit"] = self.build["git_base_commit"]
        self.current["source_pins"]["controller_source_inventory_sha256"] = sha256_json(self.build["source_files"])

    def loaded(self):
        self.receipt["controller_build"] = record(write_json(self.root / "v4-build.json", self.build))
        self.current["components"]["capture_build_receipt"] = record(
            write_json(self.root / "receipt-v4.json", self.receipt))
        return DependencyManifest.load(write_json(self.root / "manifest-v4.json", self.current))

    def assert_rejected(self, message):
        result = _check_capture_build_receipt(self.loaded())
        self.assertEqual(result.status, "FAIL", result)
        self.assertIn(message, str(result.evidence))

    def test_complete_chain_and_render_profile_preserve_parent_bytes(self):
        paths = [self.parent.path, self.parent.component_path("capture_build_receipt"),
                 self.replus.parent_manifest_path, self.replus.parent_receipt_path]
        before = {p: p.read_bytes() for p in paths}
        current = self.loaded()
        check = _check_capture_build_receipt(current)
        self.assertEqual(check.status, "PASS", check)
        self.assertEqual(check.evidence["kind"], v4.CONTROLLER_REPLUS_KIND)
        self.assertEqual(check.evidence["reviewed_replus_patch_sha256"],
                         self.parent.raw["source_pins"]["replus_patch_sha256"])
        self.assertEqual(load_render_profile_binding(current).render_profile_sha256,
                         self.parent.raw["components"]["replus_render_ini"]["sha256"])
        self.assertEqual(before, {p: p.read_bytes() for p in paths})

    def test_replaced_live_controller_resolves_v3_and_nested_v2_to_backup(self):
        installed = self.parent.component_path("diogenes_controller_asi")
        installed.write_bytes(Path(self.build["artifact"]["path"]).read_bytes())
        self.current["components"]["diogenes_controller_asi"] = record(installed)
        check = _check_capture_build_receipt(self.loaded())
        self.assertEqual(check.status, "PASS", check)
        self.assertEqual(_check_capture_build_receipt(self.parent).status, "FAIL")
        self.assertEqual(load_render_profile_binding(self.loaded()).render_profile_sha256,
                         self.parent.raw["components"]["replus_render_ini"]["sha256"])

    def test_wrong_backup_cannot_be_rehashed_into_valid_parent(self):
        path = Path(self.receipt["parent"]["controller_backup"]["path"])
        path.write_bytes(b"different controller")
        self.receipt["parent"]["controller_backup"] = record(path)
        self.assert_rejected("backup differs from immutable v3 manifest")

    def test_replus_components_and_config_cannot_change(self):
        for name in ("replus_asi", "replus_render_ini"):
            with self.subTest(name=name):
                original = self.current["components"][name]
                replacement = self.root / (name + "-different.bin")
                replacement.write_bytes(b"unauthorized component replacement")
                self.current["components"][name] = record(replacement)
                self.assert_rejected("unchanged v4 component pin changed: " + name)
                self.current["components"][name] = original

    def test_non_controller_source_pins_and_approval_cannot_change(self):
        original_pins = copy.deepcopy(self.current["source_pins"])
        self.current["source_pins"]["replus_patch_sha256"] = "f" * 64
        self.assert_rejected("source pins changed outside controller")
        self.current["source_pins"] = original_pins
        self.current["calibration"]["metric_scale_approved"] = True
        self.assert_rejected("non-controller v4 manifest field changed: calibration")
        self.current["calibration"] = copy.deepcopy(self.parent.raw["calibration"])
        self.receipt["scope"]["delivery_eligible"] = True
        self.assert_rejected("scope must not claim")

    def test_parent_failure_and_recursive_schema_cannot_be_resealed(self):
        parent_path = Path(self.receipt["parent"]["receipt"]["path"])
        raw = load_json(parent_path)
        raw["scope"]["runtime_verified"] = True
        write_json(parent_path, raw)
        parent_manifest = copy.deepcopy(self.parent.raw)
        parent_manifest["components"]["capture_build_receipt"] = record(parent_path)
        write_json(self.parent.path, parent_manifest)
        self.receipt["parent"]["manifest"] = record(self.parent.path)
        self.receipt["parent"]["receipt"] = record(parent_path)
        self.assert_rejected("scope must not claim")
        raw["schema"] = v4.CONTROLLER_REPLUS_DERIVED_SCHEMA
        write_json(parent_path, raw)
        parent_manifest["components"]["capture_build_receipt"] = record(parent_path)
        write_json(self.parent.path, parent_manifest)
        self.receipt["parent"]["manifest"] = record(self.parent.path)
        self.receipt["parent"]["receipt"] = record(parent_path)
        self.assert_rejected("recursive derivation is forbidden")

    def test_new_build_must_bind_actual_v3_parent(self):
        self.build["parent_capture_build_receipt"] = record(self.replus.parent_receipt_path)
        self.assert_rejected("different v3 parent")

    def test_new_build_and_source_are_not_skipped(self):
        (self.controller.source / "native/src/main.cpp").write_bytes(b"int value = 999;\n")
        self.build["source_files"] = self.controller.inventory(self.controller.source)
        digest = sha256_json(self.build["source_files"])
        self.receipt["source_provenance"]["inventory_sha256"] = digest
        self.current["source_pins"]["controller_source_inventory_sha256"] = digest
        self.assert_rejected("base plus patch differs from snapshot")

    def test_preflight_uses_inherited_patch_only_after_full_v4_success(self):
        with (patch.object(preflight, "_check_gpu_driver", return_value=preflight.Check("gpu", "PASS", "synthetic")),
              patch.object(preflight, "_check_reshade_receipt", return_value=preflight.Check("reshade", "PASS", "synthetic")),
              patch.object(preflight, "_actual_windows_file_version", return_value=None),
              patch.object(preflight, "_check_render_files", return_value=[])):
            report = preflight.run_preflight(self.loaded(), mode="real")
            build_check = next(c for c in report.checks if c.check_id == "dependencies.capture_build_receipt")
            self.assertEqual(build_check.status, "PASS", build_check)
            pins = next(c for c in report.checks if c.check_id == "dependencies.source_pins")
            self.assertNotIn("replus_patch_sha256", pins.evidence)
            self.assertEqual(next(c for c in report.checks if c.check_id == "dependencies.capture_build_receipt").status, "PASS")
            self.receipt["scope"]["runtime_verified"] = True
            report = preflight.run_preflight(self.loaded(), mode="real")
            pins = next(c for c in report.checks if c.check_id == "dependencies.source_pins")
            self.assertEqual(pins.status, "FAIL")
            self.assertIn("replus_patch_sha256", pins.evidence)
            self.assertEqual(next(c for c in report.checks if c.check_id == "dependencies.capture_build_receipt").status, "FAIL")

    def test_writer_preserves_v3_and_checks_complete_candidate(self):
        before = {p: p.read_bytes() for p in (self.parent.path,
                    self.parent.component_path("capture_build_receipt"))}
        write_json(self.root / "parent-manifest.json", self.parent.raw)
        output = self.controller.write_derivation(
            Path(self.receipt["parent"]["controller_backup"]["path"]))
        saved = load_json(output)
        self.assertEqual(saved["schema"], v4.CONTROLLER_REPLUS_DERIVED_SCHEMA)
        self.assertEqual(load_json(output.parent / "source-verification.json")["status"], "PASS")
        self.assertEqual(_check_capture_build_receipt(DependencyManifest.load(
            output.parent / "manifest-source-review.json")).status, "PASS")
        self.assertEqual(before, {p: p.read_bytes() for p in before})


if __name__ == "__main__":
    unittest.main()

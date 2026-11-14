"""Tampering must fail even when the surrounding local JSON hashes are renewed."""

import copy
import difflib
from contextlib import redirect_stdout
import importlib.util
import io
import json
from pathlib import Path
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch
import xml.etree.ElementTree as ET
import zipfile

from helpers import manifest_dict, write_json
from gta_capture import controller_receipt as cr
from gta_capture.errors import ContractError
from gta_capture.jsonio import sha256_file, sha256_json
from gta_capture.preflight import (DependencyManifest, REPLUS_RENDER_PROFILE,
                                   _check_capture_build_receipt, load_render_profile_binding)


_WRITER_SPEC = importlib.util.spec_from_file_location(
    "controller_receipt_writer",
    Path(__file__).resolve().parents[1] / "scripts/write-controller-build-receipt.py")
writer = importlib.util.module_from_spec(_WRITER_SPEC)
_WRITER_SPEC.loader.exec_module(writer)


def record(path):
    return {"path": str(path), "bytes": path.stat().st_size, "sha256": sha256_file(path)}


def ctest_log(*, legacy=False):
    rows = [f"{i}/{len(cr._NATIVE_TESTS)} Test #{i}: {name} .... Passed 0.01 sec"
            for i, name in enumerate(cr._NATIVE_TESTS, 1)]
    rows.append("100% tests passed" + (", 0 tests failed" if legacy else "") + f" out of {len(cr._NATIVE_TESTS)}")
    return "\n".join(rows) + "\n"


class ControllerReceiptTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve()
        self.source = self.root / "source"
        self.build_dir = self.root / "build"
        self.sdk = self.root / "sdk"
        self.baseline = {
            "native/CMakeLists.txt": b"add_library(DiogenesGtaController SHARED src/main.cpp)\n",
            "native/src/main.cpp": b"int value = 1;\n",
            "native/include/base.hpp": b"#pragma once\n",
            "tests/fixtures/golden-camera-tape.json": b"{}\n",
            "tests/fixtures/golden-camera-tape.bin": b"\x00\xff\x01",
        }
        for name, content in self.baseline.items():
            self.file(self.source / name, content)
        self.file(self.source / "native/src/main.cpp", b"int value = 2;\r\n")
        self.file(self.source / "native/include/new.hpp", b"// frozen addition\n")
        patch_text = "".join(difflib.unified_diff(
            self.baseline["native/src/main.cpp"].decode().splitlines(True),
            ["int value = 2;\n"],
            fromfile="a/" + cr._PREFIX + "native/src/main.cpp",
            tofile="b/" + cr._PREFIX + "native/src/main.cpp"))
        patch_path = self.file(self.root / "tracked.patch", patch_text.encode())
        self.base = "2" * 40
        real_git = cr._git

        def fixture_git(repository, arguments, *, cwd=None):
            # Only Git object lookup is simulated. Patch inspection/application
            # uses real Git in an isolated temporary tree, never the repo index.
            if arguments[0] == "rev-parse":
                return self.base.encode() + b"\n"
            if arguments[0] == "ls-tree":
                return b"".join(b"100644 blob " + str(i).encode() + b"\t"
                                + (cr._PREFIX + name).encode() + b"\0"
                                for i, name in enumerate(self.baseline))
            if arguments[0] == "cat-file":
                return list(self.baseline.values())[int(arguments[2])]
            return real_git(repository, arguments, cwd=cwd)

        self.git_patch = patch.object(cr, "_git", side_effect=fixture_git)
        self.git_patch.start()
        self.addCleanup(self.git_patch.stop)
        self.tools = {name: record(self.file(self.root / "tools" / (name + ".exe"), name.encode()))
                      for name in cr._TOOLS}
        for name in ("inc/main.h", "lib/ScriptHookV.lib"):
            self.file(self.sdk / name, name.encode())
        archive = self.root / "sdk.zip"
        with zipfile.ZipFile(archive, "w") as handle:
            for path in self.sdk.rglob("*"):
                if path.is_file():
                    handle.write(path, path.relative_to(self.sdk).as_posix())
        self.parent_manifest = manifest_dict(calibrated=False)
        for name in self.parent_manifest["components"]:
            self.parent_manifest["components"][name] = record(self.file(
                self.root / "parent" / (name + ".bin"), (name + " original").encode()))
        self.parent_manifest["source_pins"]["scripthook_sdk_zip_sha256"] = sha256_file(archive)
        artifacts = {name: self.parent_manifest["components"][name] for name in (
            "replus_asi", "replus_capture_addon", "diogenes_controller_asi", "replus_main_ini",
            "replus_render_ini", "replus_lights_ini", "replus_settings_exe", "igcs_dof_shader")}
        old_invocations = [
            {"executable": self.tools["cmake"]["path"], "argv": ["--build", str(self.root / target),
             "--config", "Release", "--clean-first", "--parallel", "4"]}
            for target in ("replus-build", "controller-build")]
        old_invocations.insert(1, {"executable": self.tools["msbuild"]["path"], "argv": [
            str(self.root / "IgcsConnector.sln"), "/t:Rebuild", "/m", "/p:Configuration=Release",
            "/p:Platform=x64", "/nologo"]})
        self.parent_receipt = {
            "schema": "dm-gta-capture-build-receipt/1",
            "started_at": "2026-09-08T00:00:00Z", "finished_at": "2026-09-08T00:01:00Z",
            "sources": {key: self.parent_manifest["source_pins"][key] for key in (
                "replus_commit", "replus_patch_sha256", "igcs_fork_commit", "igcs_patch_sha256",
                "controller_git_commit")},
            "toolchain": {"cmake_path": self.tools["cmake"]["path"], "cmake_version": "fixture",
                "msbuild_path": self.tools["msbuild"]["path"], "msbuild_version": "fixture",
                "configuration": "Release", "platform": "x64", "invocations": old_invocations,
                "commands": [" ".join([item["executable"], *item["argv"]]) for item in old_invocations]},
            "artifacts": artifacts,
            "configuration_transforms": {"replus_render_ini": {
                "schema": "dm-gta-replus-render-profile/1",
                "algorithm": "replace-unique-keys-utf8-crlf/1",
                "input": {"bytes": 10, "sha256": "1" * 64},
                "output": artifacts["replus_render_ini"],
                "settings": {**REPLUS_RENDER_PROFILE, "RenderOutputFolder": str(self.root / "frames")}}},
        }
        cache = {
            "CMAKE_HOME_DIRECTORY": str(self.source / "native"),
            "CMAKE_COMMAND": self.tools["cmake"]["path"],
            "CMAKE_CTEST_COMMAND": self.tools["ctest"]["path"],
            "CMAKE_LINKER": self.tools["linker"]["path"],
            "SCRIPT_HOOK_V_SDK_ROOT": str(self.sdk),
            "SCRIPT_HOOK_V_INCLUDE_DIR": str(self.sdk / "inc"),
            "SCRIPT_HOOK_V_LIBRARY": str(self.sdk / "lib/ScriptHookV.lib"),
            "DIOGENES_BUILD_SCRIPT_HOOK_V_ASI": "ON", "CMAKE_GENERATOR_PLATFORM": "x64",
            "CMAKE_GENERATOR": "Visual Studio fixture",
            "CMAKE_CXX_FLAGS": "/DWIN32 /D_WINDOWS /EHsc", "CMAKE_CXX_FLAGS_RELEASE": "/O2 /Ob2 /DNDEBUG",
        }
        cache_path = self.file(self.build_dir / "CMakeCache.txt",
                               "\n".join(f"{k}:STRING={v}" for k, v in cache.items()).encode())
        compiler = {"CMAKE_CXX_COMPILER": self.tools["compiler"]["path"], "CMAKE_CXX_COMPILER_ID": "MSVC",
                    "CMAKE_CXX_COMPILER_ARCHITECTURE_ID": "x64", "CMAKE_CXX_COMPILER_VERSION": "fixture"}
        compiler_path = self.file(self.build_dir / "CMakeFiles/CMakeCXXCompiler.cmake",
                                  "\n".join(f'set({k} "{v}")' for k, v in compiler.items()).encode())
        project = ET.Element("Project")
        ET.SubElement(project, "ClCompile", Include=str(self.source / "native/src/main.cpp"))
        ET.SubElement(project, "AdditionalIncludeDirectories").text = ";".join([
            str(self.source / "native/include"), str(self.sdk / "inc"), "%(AdditionalIncludeDirectories)"])
        ET.SubElement(project, "AdditionalDependencies").text = str(self.sdk / "lib/ScriptHookV.lib")
        ET.SubElement(project, "PlatformToolset").text = "v143"
        ET.SubElement(project, "WindowsTargetPlatformVersion").text = "10.0.fixture"
        project_path = self.file(self.build_dir / "DiogenesGtaController.vcxproj", ET.tostring(project))
        config_log = self.file(self.build_dir / "CMakeFiles/CMakeConfigureLog.yaml",
                               ('Run Build Command(s): "' + self.tools["msbuild"]["path"] + '"').encode())
        self.artifact = record(self.file(self.build_dir / "Release/DiogenesGtaController.asi", b"new ASI"))
        self.build = {"schema": "dm-gta-controller-staging-build/1", "working_tree_candidate": True,
            "git_base_commit": self.base, "source_snapshot": str(self.source),
            "source_files": self.inventory(self.source), "sdk_archive": record(archive),
            "sdk_compiler_inputs": self.inventory(self.sdk), "artifact": self.artifact,
            "started_at": "2026-09-08T00:02:00Z", "finished_at": "2026-09-08T00:03:00Z",
            "asi_built": True, "portable_contract_passed": True, "invocations": []}
        argv = [["-S", str(self.source / "native"), "-B", str(self.build_dir), "-A", "x64",
                 "-DDIOGENES_BUILD_SCRIPT_HOOK_V_ASI=ON", "-DSCRIPT_HOOK_V_SDK_ROOT=" + str(self.sdk)],
                ["--build", str(self.build_dir), "--config", "Release", "--parallel", "2"],
                ["--test-dir", str(self.build_dir), "-C", "Release", "--output-on-failure"]]
        for i, arguments in enumerate(argv):
            log = self.file(self.root / f"step-{i}.log", (ctest_log() if i == 2 else "success").encode())
            self.build["invocations"].append({"executable": self.tools["ctest" if i == 2 else "cmake"]["path"],
                "argv": arguments, "returncode": 0, "log": str(log), "log_sha256": sha256_file(log)})
        self.receipt = {"schema": cr.CONTROLLER_DERIVED_SCHEMA, "kind": "controller_only_derived",
            "source_provenance": {"repository": str(self.root), "base_commit": self.base,
                "tracked_patch": record(patch_path), "text_normalization": "crlf-to-lf",
                "untracked_files": [item for item in self.build["source_files"] if item["path"].endswith("new.hpp")],
                "inventory_sha256": sha256_json(self.build["source_files"])},
            "build_evidence": {"cache": record(cache_path), "compiler_config": record(compiler_path),
                "project": record(project_path), "configure_log": record(config_log),
                "builder": record(self.file(self.root / "builder.py", b"fixture builder")), "tools": self.tools}}
        self.seal_parent()
        self.current = copy.deepcopy(self.parent_manifest)
        self.current["components"]["diogenes_controller_asi"] = self.artifact
        self.current["source_pins"]["controller_git_commit"] = self.base
        self.current["source_pins"]["controller_source_inventory_sha256"] = self.receipt["source_provenance"]["inventory_sha256"]

    def file(self, path, data):
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)
        return path

    def inventory(self, root):
        return [{**record(path), "path": path.relative_to(root).as_posix()}
                for path in sorted(root.rglob("*")) if path.is_file()]

    def seal_parent(self):
        receipt_path = write_json(self.root / "parent-receipt.json", self.parent_receipt)
        self.parent_manifest["components"]["capture_build_receipt"] = record(receipt_path)
        manifest_path = write_json(self.root / "parent-manifest.json", self.parent_manifest)
        self.receipt["parent"] = {"manifest": record(manifest_path), "receipt": record(receipt_path),
            "controller_backup": self.parent_manifest["components"]["diogenes_controller_asi"]}
        self.build["parent_capture_build_receipt"] = record(receipt_path)

    def loaded(self):
        build_path = write_json(self.root / "build.json", self.build)
        self.receipt["controller_build"] = record(build_path)
        receipt_path = write_json(self.root / "derived.json", self.receipt)
        self.current["components"]["capture_build_receipt"] = record(receipt_path)
        return DependencyManifest.load(write_json(self.root / "manifest.json", self.current))

    def assert_rejected(self, text):
        check = _check_capture_build_receipt(self.loaded())
        self.assertEqual(check.status, "FAIL", check)
        self.assertIn(text, str(check.evidence))

    def write_derivation(self, backup=None):
        """Reuse the synthetic full build; actual file copies/patch application run."""
        module = self.root / cr._PREFIX
        for item in self.build["source_files"]:
            self.file(module / item["path"], (self.source / item["path"]).read_bytes())
        self.file(self.build_dir / "CMakeFiles/fixture/CMakeCXXCompiler.cmake",
                  (self.build_dir / "CMakeFiles/CMakeCXXCompiler.cmake").read_bytes())
        self.build["deployed"] = False
        build_path = write_json(self.root / "build.json", self.build)
        args = SimpleNamespace(
            repository=self.root, controller_build=build_path,
            parent_manifest=self.root / "parent-manifest.json",
            parent_controller_backup=backup,
            build_helper=self.root / "builder.py", out_dir=self.root / "writer-output")

        def fixture_writer_git(repository, *arguments, input_bytes=None):
            self.assertEqual(repository, self.root)
            if arguments[:2] == ("cat-file", "-e"):
                self.assertEqual(arguments[2], self.base + "^{commit}")
                return b""
            if arguments[0] == "ls-files":
                names = (["native/include/new.hpp"] if "--others" in arguments
                         else list(self.baseline))
                return b"".join((cr._PREFIX + name).encode() + b"\0" for name in names)
            if arguments[0] == "diff":
                return (self.root / "tracked.patch").read_bytes()
            if arguments[0] == "show":
                self.assertTrue(arguments[1].startswith(self.base + ":" + cr._PREFIX))
                name = arguments[1][len(self.base + ":" + cr._PREFIX):]
                return self.baseline[name]
            raise AssertionError(f"unexpected writer Git operation: {arguments}")

        with (patch.object(writer, "MODULE_ROOT", module),
              patch.object(writer, "git", side_effect=fixture_writer_git)):
            return writer.write_receipt(args)

    def test_writer_default_uses_original_installed_parent(self):
        result = json.loads(self.write_derivation().read_text("utf-8"))
        backup = result["parent"]["controller_backup"]
        original = self.parent_manifest["components"]["diogenes_controller_asi"]
        self.assertEqual(backup["sha256"], original["sha256"])
        self.assertEqual(backup["bytes"], original["bytes"])
        self.assertEqual(Path(backup["path"]).read_bytes(), Path(original["path"]).read_bytes())

    def test_writer_default_still_rejects_replaced_installation(self):
        installed = Path(self.parent_manifest["components"]["diogenes_controller_asi"]["path"])
        installed.write_bytes(b"legitimate newer installed controller")
        with self.assertRaisesRegex(ValueError, "old installed controller.*pinned parent"):
            self.write_derivation()
        self.assertFalse((self.root / "writer-output").exists())

    def test_writer_explicit_backup_allows_replaced_installation_without_rewriting_parent(self):
        installed = Path(self.parent_manifest["components"]["diogenes_controller_asi"]["path"])
        backup = self.file(self.root / "archived/original-v1.asi", installed.read_bytes())
        installed.write_bytes(b"legitimate newer installed controller")
        originals = {path: path.read_bytes() for path in (
            self.root / "parent-manifest.json", self.root / "parent-receipt.json", installed, backup)}
        result = json.loads(self.write_derivation(backup).read_text("utf-8"))
        copied = result["parent"]["controller_backup"]
        self.assertEqual(Path(copied["path"]).read_bytes(), originals[backup])
        self.assertEqual(copied["sha256"], self.parent_manifest["components"]["diogenes_controller_asi"]["sha256"])
        self.assertEqual(copied["bytes"], self.parent_receipt["artifacts"]["diogenes_controller_asi"]["bytes"])
        for path, before in originals.items():
            self.assertEqual(path.read_bytes(), before)
        self.assertEqual(Path(result["parent"]["manifest"]["path"]).read_bytes(),
                         originals[self.root / "parent-manifest.json"])
        self.assertEqual(Path(result["parent"]["receipt"]["path"]).read_bytes(),
                         originals[self.root / "parent-receipt.json"])
        # Exercise the existing verifier with the real copied parent backup.
        self.receipt = result
        check = _check_capture_build_receipt(self.loaded())
        self.assertEqual(check.status, "PASS", check)

    def test_writer_rejects_same_size_wrong_backup_hash(self):
        original = self.parent_manifest["components"]["diogenes_controller_asi"]
        wrong = self.file(self.root / "wrong.asi", b"x" * original["bytes"])
        with self.assertRaisesRegex(ValueError, "explicit parent controller backup.*pinned parent"):
            self.write_derivation(wrong)
        self.assertFalse((self.root / "writer-output").exists())

    def test_writer_backup_must_also_match_original_v1_artifact_bytes(self):
        installed = Path(self.parent_manifest["components"]["diogenes_controller_asi"]["path"])
        backup = self.file(self.root / "original-v1.asi", installed.read_bytes())
        artifact = copy.deepcopy(self.parent_receipt["artifacts"]["diogenes_controller_asi"])
        artifact["bytes"] += 1
        self.parent_receipt["artifacts"]["diogenes_controller_asi"] = artifact
        self.seal_parent()
        with self.assertRaisesRegex(ValueError, "explicit parent controller backup.*pinned parent"):
            self.write_derivation(backup)
        self.assertFalse((self.root / "writer-output").exists())

    def test_writer_cli_accepts_explicit_backup_path(self):
        output = self.file(self.root / "synthetic-receipt.json", b"{}\n")
        backup = self.root / "archived/original-v1.asi"
        argv = ["write-controller-build-receipt.py", "--controller-build", "build.json",
                "--parent-manifest", "parent.json", "--parent-controller-backup", str(backup),
                "--build-helper", "builder.py", "--out-dir", "output"]
        with (patch.object(sys, "argv", argv),
              patch.object(writer, "write_receipt", return_value=output) as seal,
              redirect_stdout(io.StringIO())):
            self.assertEqual(writer.main(), 0)
        self.assertEqual(seal.call_args.args[0].parent_controller_backup, backup)

    def test_complete_derivation_and_render_profile_with_pending_calibration(self):
        loaded = self.loaded()
        self.assertEqual(_check_capture_build_receipt(loaded).status, "PASS")
        self.assertEqual(load_render_profile_binding(loaded).expected_output_root, str(self.root / "frames"))
        self.assertFalse(loaded.raw["calibration"]["metric_scale_approved"])

    def test_parent_is_fully_revalidated_after_rehashing(self):
        self.parent_receipt["toolchain"]["commands"][0] = "invented whole-stack build"
        self.seal_parent()
        self.assert_rejected("original v1 parent failed")

    def test_recursive_parent_is_not_accepted(self):
        self.parent_receipt["schema"] = cr.CONTROLLER_DERIVED_SCHEMA
        self.seal_parent()
        self.assert_rejected("single immutable v1 parent")

    def test_noncontroller_manifest_approvals_cannot_change(self):
        self.current["calibration"]["metric_scale_approved"] = True
        self.assert_rejected("non-controller manifest field changed: calibration")

    def test_inherited_component_pin_and_bytes_are_both_frozen(self):
        name = "replus_asi"
        path = Path(self.current["components"][name]["path"])
        path.write_bytes(b"changed RE+")
        self.assert_rejected("inherited component bytes changed")
        self.current["components"][name] = record(path)
        self.assert_rejected("unchanged component pin changed")

    def test_rehashed_source_still_must_equal_base_plus_patch(self):
        (self.source / "native/src/main.cpp").write_bytes(b"int value = 999;\n")
        self.build["source_files"] = self.inventory(self.source)
        digest = sha256_json(self.build["source_files"])
        self.receipt["source_provenance"]["inventory_sha256"] = digest
        self.current["source_pins"]["controller_source_inventory_sha256"] = digest
        self.assert_rejected("base plus patch differs")

    def test_unlisted_source_and_missing_untracked_proof_fail(self):
        extra = self.file(self.source / "native/include/hidden.hpp", b"hidden")
        self.assert_rejected("unlisted files")
        extra.unlink()
        self.receipt["source_provenance"]["untracked_files"] = []
        self.assert_rejected("untracked source inventory differs")

    def test_sdk_cannot_be_replaced_by_rehashing_extracted_inputs(self):
        (self.sdk / "inc/main.h").write_bytes(b"changed header")
        self.build["sdk_compiler_inputs"] = self.inventory(self.sdk)
        self.assert_rejected("SDK compiler input differs from the pinned archive")

    def test_msbuild_record_must_match_actual_configure_log(self):
        self.tools["msbuild"] = record(self.file(self.root / "other/MSBuild.exe", b"other tool"))
        self.assert_rejected("MSBuild identity differs")

    def test_rehashed_failed_or_partial_test_invocations_fail(self):
        self.build["invocations"][2]["returncode"] = 1
        self.assert_rejected("invocation/exit code")
        self.build["invocations"][2]["returncode"] = 0
        self.build["invocations"][2]["argv"] += ["-R", "recording_camera"]
        self.assert_rejected("invocation/exit code")

    def test_deployed_artifact_and_render_loader_verify_entire_derivation(self):
        self.current["components"]["diogenes_controller_asi"] = record(self.file(self.root / "wrong.asi", b"wrong"))
        loaded = self.loaded()
        self.assertEqual(_check_capture_build_receipt(loaded).status, "FAIL")
        with self.assertRaisesRegex(ContractError, "deployed controller differs"):
            load_render_profile_binding(loaded)


class CtestLogTests(unittest.TestCase):
    def test_real_old_and_new_summary_formats(self):
        cr._verify_ctest_log(ctest_log())
        cr._verify_ctest_log(ctest_log(legacy=True))

    def test_summary_alone_missing_duplicate_renamed_or_failed_contract_rejected(self):
        log = ctest_log()
        cases = [f"100% tests passed out of {len(cr._NATIVE_TESTS)}\n", "\n".join(log.splitlines()[1:]),
                 log.replace(cr._NATIVE_TESTS[1], cr._NATIVE_TESTS[0]),
                 log.replace(cr._NATIVE_TESTS[-1], "unrelated_test"),
                 log.replace("Passed", "***Failed", 1)]
        for value in cases:
            with self.subTest(log=value):
                with self.assertRaises(ContractError):
                    cr._verify_ctest_log(value)


if __name__ == "__main__":
    unittest.main()

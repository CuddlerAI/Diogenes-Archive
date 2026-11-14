"""Synthetic source/build fixtures; real patch application and full v2 parent checks."""
import copy
import difflib
import importlib.util
import io
from pathlib import Path
import tarfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch
import xml.etree.ElementTree as ET

from helpers import write_json
import test_controller_receipt as parent_tests
from test_controller_receipt import record
from gta_capture import replus_receipt as rr
from gta_capture import preflight
from gta_capture.errors import ContractError
from gta_capture.jsonio import sha256_file, sha256_json
from gta_capture.preflight import DependencyManifest, _check_capture_build_receipt, load_render_profile_binding


class ReplusReceiptTests(unittest.TestCase):
    def setUp(self):
        # Reuse the existing complete v1 -> v2 fixture, including actual v2
        # source reconstruction, SDK archive and contract-log verification.
        self.parent_fixture = parent_tests.ControllerReceiptTests()
        self.parent_fixture.setUp()
        self.addCleanup(self.parent_fixture.doCleanups)
        self.root = self.parent_fixture.root / "replus"
        self.root.mkdir()
        self.parent = self.parent_fixture.loaded()
        self.parent_raw = copy.deepcopy(self.parent.raw)
        self.parent_receipt_path = self.parent.component_path("capture_build_receipt")
        self.parent_manifest_path = self.parent.path
        self.source = self.root / "source"
        self.directory = self.root / "build"
        self.baseline = {"CMakeLists.txt": b"project(RockstarEditorPlus)\n",
                         "src/main.cpp": b"int mode = 0;\n", "fx_asi_build.rc": b"// resource\n"}
        for name in ("buffer.c", "hook.c", "trampoline.c", "hde/hde64.c"):
            self.baseline["src/vendor/minhook/src/" + name] = b"// synthetic vendor source\n"
        for name, data in self.baseline.items():
            self.file(self.source / name, data)
        self.file(self.source / "src/main.cpp", b"int mode = 1;\n")
        patch_text = "".join(difflib.unified_diff(["int mode = 0;\n"], ["int mode = 1;\n"],
                                               fromfile="a/src/main.cpp", tofile="b/src/main.cpp"))
        patch_path = self.file(self.root / "source.patch", patch_text.encode())
        pin_patch = patch.object(rr, "REPLUS_PATCH_SHA256", sha256_file(patch_path))
        pin_patch.start(); self.addCleanup(pin_patch.stop)
        reviewed = patch.dict(rr.REVIEWED_REPLUS_PATCHES, {
            sha256_file(patch_path): rr.ReviewedReplusPatch("synthetic-story-idle.patch", 9)})
        reviewed.start(); self.addCleanup(reviewed.stop)
        archive = self.root / "base.tar"
        with tarfile.open(archive, "w") as handle:
            for name, data in self.baseline.items():
                info = tarfile.TarInfo(name); info.size = len(data)
                handle.addfile(info, io.BytesIO(data))
        real_git = rr._git
        self.real_git = real_git

        def fixture_git(repository, argv, *, cwd=None):
            if argv[0] == "rev-parse":
                return rr.REPLUS_COMMIT.encode() + b"\n"
            if argv[0] == "ls-tree":
                return b"".join(b"100644 blob " + str(i).encode() + b"\t" + name.encode() + b"\0"
                                for i, name in enumerate(self.baseline))
            if argv[0] == "cat-file":
                return list(self.baseline.values())[int(argv[2])]
            return real_git(repository, argv, cwd=cwd)

        git_patch = patch.object(rr, "_git", side_effect=fixture_git)
        git_patch.start(); self.addCleanup(git_patch.stop)
        self.tools = {name: record(self.file(self.root / "tools" / (name + ".exe"), name.encode()))
                      for name in rr._TOOLS}
        self.tools["rc"] = record(self.file(self.root / "sdk/bin/10.0.fixture/x64/rc.exe", b"synthetic RC"))
        self.cache = {"CMAKE_HOME_DIRECTORY": str(self.source), "CMAKE_COMMAND": self.tools["cmake"]["path"],
                      "CMAKE_LINKER": self.tools["linker"]["path"], "CMAKE_RC_COMPILER": "rc",
                      "CMAKE_MT": "CMAKE_MT-NOTFOUND", "CMAKE_GENERATOR_PLATFORM": "x64",
                      "CMAKE_GENERATOR": "Visual Studio 17 2022", "CMAKE_POLICY_VERSION_MINIMUM": "3.5"}
        for lang in ("C", "CXX"):
            self.cache[f"CMAKE_{lang}_FLAGS"] = "/DWIN32 /D_WINDOWS" + (" /GR /EHsc" if lang == "CXX" else "")
            self.cache[f"CMAKE_{lang}_FLAGS_RELEASE"] = "/O2 /Ob2 /DNDEBUG"
        cache_path = self.write_cache()
        compiler_configs = {}
        for lang in ("C", "CXX"):
            values = {f"CMAKE_{lang}_COMPILER": self.tools["compiler"]["path"],
                      f"CMAKE_{lang}_COMPILER_ID": "MSVC", f"CMAKE_{lang}_COMPILER_ARCHITECTURE_ID": "x64",
                      f"CMAKE_{lang}_COMPILER_VERSION": "fixture"}
            path = self.file(self.directory / f"CMakeFiles/fixture/CMake{lang}Compiler.cmake",
                             "\n".join(f'set({k} "{v}")' for k, v in values.items()).encode())
            compiler_configs[lang] = record(path)
        config_log = self.file(self.directory / "CMakeFiles/CMakeConfigureLog.yaml",
                               ('Run Build Command(s): "' + self.tools["msbuild"]["path"] + '"').encode())
        main = ET.Element("Project")
        ET.SubElement(main, "ClCompile", Include=str(self.source / "src/main.cpp"))
        ET.SubElement(main, "ResourceCompile", Include=str(self.source / "fx_asi_build.rc"))
        ET.SubElement(main, "AdditionalIncludeDirectories").text = ";".join([
            str(self.source / "src"), "%(AdditionalIncludeDirectories)"])
        ET.SubElement(main, "AdditionalDependencies").text = "version.lib;bcrypt.lib;minhook.x64.lib"
        mini = ET.Element("Project")
        for name in self.baseline:
            if name.endswith(".c"):
                ET.SubElement(mini, "ClCompile", Include=str(self.source / name))
        for project in (main, mini):
            ET.SubElement(project, "PlatformToolset").text = "v143"
            ET.SubElement(project, "WindowsTargetPlatformVersion").text = "10.0.fixture"
        project_path = self.file(self.directory / "RockstarEditorPlus.vcxproj", ET.tostring(main))
        minhook_path = self.file(self.directory / "src/vendor/minhook/minhook.vcxproj", ET.tostring(mini))
        self.artifact = record(self.file(self.directory / "BIN/Release/RockstarEditorPlus.asi", b"synthetic-new-RE+"))
        builder = record(self.file(self.root / "builder.py", b"synthetic builder not executed"))
        old_replus = Path(self.parent_raw["components"]["replus_asi"]["path"])
        backup = record(self.file(self.root / "parent/RockstarEditorPlus.asi", old_replus.read_bytes()))
        self.build = {"schema": rr.REPLUS_BUILD_SCHEMA, "git_base_commit": rr.REPLUS_COMMIT,
            "source_snapshot": str(self.source), "source_files": rr.source_inventory(self.source),
            "source_base_archive": record(archive), "source_patch": record(patch_path), "builder": builder,
            "artifact": self.artifact, "parent_capture_build_receipt": record(self.parent_receipt_path),
            "started_at": "2026-09-08T01:00:00Z", "finished_at": "2026-09-08T01:01:00Z",
            "asi_built": True, "deployed": False, "working_tree_candidate": True,
            "runtime_tested": False, "contract_assertions": 9, "contract_assertions_verified": True, "invocations": []}
        argv = [["-S", str(self.source), "-B", str(self.directory), "-G", "Visual Studio 17 2022", "-A", "x64", "-DCMAKE_POLICY_VERSION_MINIMUM=3.5"],
                ["--build", str(self.directory), "--config", "Release", "--parallel", "2", "--", "/v:diag"]]
        for i, arguments in enumerate(argv):
            log = self.file(self.root / f"step-{i}.log", ('RockstarEditorPlus.asi minhook.x64.lib built\n  "'
                           + self.tools["rc"]["path"] + '" /nologo /fo"resource.res" input.rc\n').encode())
            self.build["invocations"].append({"executable": self.tools["cmake"]["path"], "argv": arguments,
                                             "returncode": 0, "log": str(log), "log_sha256": sha256_file(log)})
        self.receipt = {"schema": rr.REPLUS_DERIVED_SCHEMA, "kind": "replus_only_derived", "scope": copy.deepcopy(rr.REPLUS_SCOPE),
            "parent": {"manifest": record(self.parent_manifest_path), "receipt": record(self.parent_receipt_path),
                       "replus_backup": backup},
            "source_provenance": {"repository": str(self.source), "base_commit": rr.REPLUS_COMMIT,
                "exact_patch": record(patch_path), "inventory_sha256": sha256_json(self.build["source_files"]),
                "text_normalization": "utf8-text-crlf-to-lf"},
            "build_evidence": {"builder": builder, "tools": self.tools, "cache": record(cache_path),
                "compiler_configs": compiler_configs, "configure_log": record(config_log),
                "project": record(project_path), "minhook_project": record(minhook_path)}}
        self.current = copy.deepcopy(self.parent_raw)
        self.current["components"]["replus_asi"] = self.artifact
        self.current["source_pins"]["replus_patch_sha256"] = rr.REPLUS_PATCH_SHA256
        self.current["source_pins"]["replus_source_inventory_sha256"] = self.receipt["source_provenance"]["inventory_sha256"]

    def file(self, path, data):
        path.parent.mkdir(parents=True, exist_ok=True); path.write_bytes(data); return path

    def write_cache(self):
        return self.file(self.directory / "CMakeCache.txt", "\n".join(f"{k}:STRING={v}" for k,v in self.cache.items()).encode())

    def loaded(self):
        build_path = write_json(self.root / "build.json", self.build)
        self.receipt["replus_build"] = record(build_path)
        receipt_path = write_json(self.root / "receipt-v3.json", self.receipt)
        self.current["components"]["capture_build_receipt"] = record(receipt_path)
        return DependencyManifest.load(write_json(self.root / "manifest.json", self.current))

    def writer(self, backup, *, source_repository=None):
        self.loaded()  # seal the actual synthetic build record consumed by writer
        path = Path(__file__).resolve().parents[1] / "scripts/write-replus-build-receipt.py"
        spec = importlib.util.spec_from_file_location("replus_writer_unit", path)
        module = importlib.util.module_from_spec(spec); spec.loader.exec_module(module)
        return module.write_receipt(SimpleNamespace(
            replus_build=self.root / "build.json", parent_manifest=self.parent_manifest_path,
            parent_replus_backup=backup, source_repository=source_repository,
            out_dir=self.root / "writer-output"))

    def assert_rejected(self, text):
        result = _check_capture_build_receipt(self.loaded())
        self.assertEqual(result.status, "FAIL", result)
        self.assertIn(text, str(result.evidence))

    def use_second_synthetic_patch(self, *, registered=True):
        """Change real fixture bytes/closure; registration is explicit test-only."""
        self.file(self.source / "src/main.cpp", b"int mode = 2;\n")
        text = "".join(difflib.unified_diff(["int mode = 0;\n"], ["int mode = 2;\n"],
                                           fromfile="a/src/main.cpp", tofile="b/src/main.cpp"))
        path = self.file(self.root / "second-complete.patch", text.encode())
        digest = sha256_file(path)
        if registered:
            reviewed = patch.dict(rr.REVIEWED_REPLUS_PATCHES, {
                digest: rr.ReviewedReplusPatch("synthetic-cursor-refresh.patch", 14)})
            reviewed.start(); self.addCleanup(reviewed.stop)
        self.build["source_patch"] = record(path)
        self.build["source_files"] = rr.source_inventory(self.source)
        self.build["contract_assertions"] = 14
        self.receipt["source_provenance"]["exact_patch"] = record(path)
        inventory = sha256_json(self.build["source_files"])
        self.receipt["source_provenance"]["inventory_sha256"] = inventory
        self.current["source_pins"]["replus_patch_sha256"] = digest
        self.current["source_pins"]["replus_source_inventory_sha256"] = inventory
        return digest

    def test_full_chain_and_render_profile_pass_without_changing_parent(self):
        before = {p: p.read_bytes() for p in (self.parent_manifest_path, self.parent_receipt_path)}
        current = self.loaded()
        self.assertEqual(_check_capture_build_receipt(current).status, "PASS")
        self.assertEqual(load_render_profile_binding(current).render_profile_sha256,
                         self.parent_raw["components"]["replus_render_ini"]["sha256"])
        self.assertEqual(before, {p: p.read_bytes() for p in before})

    def test_replaced_install_path_validates_only_via_exact_old_backup(self):
        installed = Path(self.parent_raw["components"]["replus_asi"]["path"])
        installed.write_bytes(Path(self.artifact["path"]).read_bytes())
        self.current["components"]["replus_asi"] = record(installed)
        self.assertEqual(_check_capture_build_receipt(self.loaded()).status, "PASS")
        # The unchanged v2 validator alone still rejects changed RE+ bytes.
        self.assertEqual(_check_capture_build_receipt(self.parent).status, "FAIL")

    def test_nested_v1_callback_resolves_only_historical_replus(self):
        current = self.loaded()
        calls = []
        def check_v1(parent):
            calls.append(parent)
            self.assertEqual(parent.component_path("replus_asi"), Path(self.receipt["parent"]["replus_backup"]["path"]))
            self.assertEqual(parent.raw["components"]["replus_asi"], self.parent_raw["components"]["replus_asi"])
            self.assertNotEqual(parent.component_path("capture_build_receipt"), self.parent_receipt_path)
            return preflight._check_capture_build_receipt_v1(parent)
        rr.validate_replus_receipt(current, self.receipt, validate_parent=check_v1)
        self.assertEqual(len(calls), 1)

    def test_real_preflight_new_pin_requires_full_valid_v3_and_is_checked_once(self):
        # Unrelated machine/GPU probes are mocked, never the provenance checker.
        # The SDK ZIP in this synthetic v2 fixture intentionally has its own
        # hash; we inspect the RE+ entry rather than claiming all source pins pass.
        with (patch.object(preflight, "_check_gpu_driver", return_value=preflight.Check("gpu", "PASS", "synthetic")),
              patch.object(preflight, "_check_reshade_receipt", return_value=preflight.Check("reshade", "PASS", "synthetic")),
              patch.object(preflight, "_actual_windows_file_version", return_value=None),
              patch.object(preflight, "_check_render_files", return_value=[])):
            with patch.object(preflight, "_check_capture_build_receipt", wraps=_check_capture_build_receipt) as check:
                report = preflight.run_preflight(self.loaded(), mode="real")
            self.assertEqual(check.call_count, 1)
            pins = next(c for c in report.checks if c.check_id == "dependencies.source_pins")
            self.assertNotIn("replus_patch_sha256", pins.evidence)
            self.assertEqual(next(c for c in report.checks if c.check_id == "dependencies.capture_build_receipt").status, "PASS")
            self.receipt["scope"]["delivery_eligible"] = True
            report = preflight.run_preflight(self.loaded(), mode="real")
            pins = next(c for c in report.checks if c.check_id == "dependencies.source_pins")
            self.assertEqual(pins.status, "FAIL")
            self.assertEqual(pins.evidence["replus_patch_sha256"]["expected"],
                             self.parent_raw["source_pins"]["replus_patch_sha256"])
            self.assertEqual(next(c for c in report.checks if c.check_id == "dependencies.capture_build_receipt").status, "FAIL")

    def test_second_registered_patch_passes_full_chain_and_reports_actual_pin(self):
        first_pin = rr.REPLUS_PATCH_SHA256
        second_pin = self.use_second_synthetic_patch()
        result = _check_capture_build_receipt(self.loaded())
        self.assertEqual(result.status, "PASS", result)
        self.assertEqual(result.evidence["reviewed_replus_patch_sha256"], second_pin)
        self.assertEqual(rr.REPLUS_PATCH_SHA256, first_pin)
        self.assertTrue(rr.is_reviewed_replus_patch(first_pin))

    def test_second_patch_preflight_uses_verified_pin_and_invalid_receipt_falls_back(self):
        self.use_second_synthetic_patch()
        self.test_real_preflight_new_pin_requires_full_valid_v3_and_is_checked_once()

    def test_registered_patch_manifest_must_match_actual_build_patch(self):
        self.use_second_synthetic_patch()
        self.current["source_pins"]["replus_patch_sha256"] = rr.REPLUS_PATCH_SHA256
        self.assert_rejected("source pins changed outside RE+")

    def test_assertion_count_is_fixed_per_reviewed_patch(self):
        self.build["contract_assertions"] = 14
        self.assert_rejected("successful, not-yet-deployed candidate")
        self.use_second_synthetic_patch()
        self.build["contract_assertions"] = 9
        self.assert_rejected("successful, not-yet-deployed candidate")

    def test_draw_guard_registration_binds_exact_repository_patch(self):
        reviewed = rr.reviewed_replus_patch(rr.REPLUS_DRAW_GUARD_PATCH_SHA256)
        path = Path(__file__).resolve().parents[1] / "patches" / reviewed.filename
        self.assertEqual(reviewed.filename, "replus-v18-draw-guard.patch")
        self.assertEqual(reviewed.contract_assertions, 27)
        self.assertEqual(sha256_file(path), rr.REPLUS_DRAW_GUARD_PATCH_SHA256)

    def test_draw_guard_contract_requires_27_assertions_through_full_chain(self):
        # The source/build remain explicitly synthetic. Only the reviewed
        # contract is reused; this test makes no real build/runtime claim.
        digest = self.use_second_synthetic_patch()
        reviewed = rr.reviewed_replus_patch(rr.REPLUS_DRAW_GUARD_PATCH_SHA256)
        with patch.dict(rr.REVIEWED_REPLUS_PATCHES, {digest: reviewed}):
            self.build["contract_assertions"] = 27
            self.assertEqual(_check_capture_build_receipt(self.loaded()).status, "PASS")
            for stale_count in (9, 14):
                with self.subTest(stale_count=stale_count):
                    self.build["contract_assertions"] = stale_count
                    self.assert_rejected("successful, not-yet-deployed candidate")

    def test_second_unregistered_complete_patch_rejected_even_with_all_hashes_updated(self):
        self.use_second_synthetic_patch(registered=False)
        self.assert_rejected("not a registered reviewed patch")

    def test_writer_selects_second_actual_patch_and_preserves_parent(self):
        digest = self.use_second_synthetic_patch()
        before = {p: p.read_bytes() for p in (self.parent_manifest_path, self.parent_receipt_path)}
        result = self.writer(Path(self.receipt["parent"]["replus_backup"]["path"]))
        self.assertEqual(result["source_verification"]["status"], "PASS")
        manifest = DependencyManifest.load(result["candidate_manifest"]["path"])
        self.assertEqual(manifest.raw["source_pins"]["replus_patch_sha256"], digest)
        saved = rr.load_json(result["receipt"]["path"])["source_provenance"]["exact_patch"]
        self.assertEqual(Path(saved["path"]).name, "synthetic-cursor-refresh.patch")
        self.assertEqual(saved["sha256"], digest)
        self.assertEqual(before, {p: p.read_bytes() for p in before})

    def test_writer_rejects_unregistered_patch_before_creating_output(self):
        self.use_second_synthetic_patch(registered=False)
        with self.assertRaisesRegex(ContractError, "not a registered reviewed patch"):
            self.writer(Path(self.receipt["parent"]["replus_backup"]["path"]))
        self.assertFalse((self.root / "writer-output").exists())

    def test_writer_explicit_object_repository_preserves_compiled_snapshot_and_build(self):
        repository = self.root / "separate-object-repository"
        repository.mkdir()
        self.loaded()
        build_before = (self.root / "build.json").read_bytes()
        sources_before = rr.source_inventory(self.source)
        result = self.writer(Path(self.receipt["parent"]["replus_backup"]["path"]),
                             source_repository=repository)
        saved = rr.load_json(result["receipt"]["path"])
        self.assertEqual(saved["source_provenance"]["repository"], str(repository))
        self.assertEqual(result["source_verification"]["status"], "PASS")
        self.assertEqual((self.root / "build.json").read_bytes(), build_before)
        self.assertEqual(rr.source_inventory(self.source), sources_before)
        self.assertEqual(rr.load_json(saved["replus_build"]["path"])["source_snapshot"], str(self.source))

    def test_writer_rejects_missing_repository_before_creating_output(self):
        with self.assertRaisesRegex(ContractError, "source object repository directory is missing"):
            self.writer(None, source_repository=self.root / "missing-repository")
        self.assertFalse((self.root / "writer-output").exists())

    def test_writer_real_git_rejects_unavailable_commit_before_creating_output(self):
        repository = self.root / "empty-object-repository"
        repository.mkdir()
        # Exercise real Git failure both for the original fallback and an
        # explicit directory. Neither synthetic directory has Git objects.
        for supplied in (None, repository):
            with self.subTest(supplied=supplied), patch.object(rr, "_git", wraps=self.real_git):
                with self.assertRaisesRegex(ContractError, "Git source verification failed"):
                    self.writer(None, source_repository=supplied)
                self.assertFalse((self.root / "writer-output").exists())

    def test_writer_rejects_wrong_commit_result_before_creating_output(self):
        with patch.object(rr, "_git", return_value=b"f" * 40 + b"\n"):
            with self.assertRaisesRegex(ContractError, r"locked RE\+ commit object unavailable"):
                self.writer(None, source_repository=self.source)
        self.assertFalse((self.root / "writer-output").exists())

    def test_writer_object_repository_cannot_bypass_full_source_reconstruction(self):
        repository = self.root / "separate-object-repository"
        repository.mkdir()
        self.file(self.source / "src/main.cpp", b"int mode = 999;\n")
        self.build["source_files"] = rr.source_inventory(self.source)
        with self.assertRaisesRegex(ContractError, "base plus exact patch differs from compiled source"):
            self.writer(None, source_repository=repository)
        failure = rr.load_json(self.root / "writer-output/source-verification.json")
        self.assertEqual(failure["status"], "FAIL")

    def test_wrong_backup_is_not_an_override(self):
        path = Path(self.receipt["parent"]["replus_backup"]["path"])
        path.write_bytes(b"not original")
        self.receipt["parent"]["replus_backup"] = record(path)
        self.assert_rejected("backup differs from immutable manifest")

    def test_writer_preserves_parent_with_explicit_backup_after_install_replacement(self):
        before = {p: p.read_bytes() for p in (self.parent_manifest_path, self.parent_receipt_path)}
        installed = Path(self.parent_raw["components"]["replus_asi"]["path"])
        installed.write_bytes(Path(self.artifact["path"]).read_bytes())
        backup = Path(self.receipt["parent"]["replus_backup"]["path"])
        result = self.writer(backup)
        self.assertEqual(result["source_verification"]["status"], "PASS")
        self.assertFalse(result["deployed"])
        self.assertEqual(before, {p: p.read_bytes() for p in before})

    def test_writer_wrong_explicit_backup_rejected_before_output_created(self):
        wrong = self.file(self.root / "wrong-backup.asi", b"not the prior RE+")
        with self.assertRaisesRegex(ContractError, "backup differs from parent pin"):
            self.writer(wrong)
        self.assertFalse((self.root / "writer-output").exists())

    def test_v3_parent_rejected_without_recursion(self):
        write_json(self.parent_receipt_path, {"schema": rr.REPLUS_DERIVED_SCHEMA, "kind": "replus_only_derived"})
        self.receipt["parent"]["receipt"] = record(self.parent_receipt_path)
        self.assert_rejected("recursive v3 is forbidden")

    def test_policy_calibration_and_other_source_pins_cannot_change(self):
        self.current["calibration"]["metric_scale_approved"] = True
        self.assert_rejected("non-RE+ manifest field changed: calibration")
        self.current["calibration"] = copy.deepcopy(self.parent_raw["calibration"])
        self.current["source_pins"]["igcs_patch_sha256"] = "f" * 64
        self.assert_rejected("source pins changed outside RE+")

    def test_controller_component_pin_cannot_change(self):
        self.current["components"]["diogenes_controller_asi"] = self.artifact
        self.assert_rejected("unchanged component pin changed")

    def test_scope_and_assertion_claims_remain_truthful(self):
        self.receipt["scope"]["calibration_approved"] = True
        self.assert_rejected("receipt scope must not claim")
        self.receipt["scope"] = copy.deepcopy(rr.REPLUS_SCOPE)
        self.build["contract_assertions_verified"] = False
        self.assert_rejected("successful, not-yet-deployed candidate")

    def test_rc_property_or_guessed_sdk_path_is_not_execution_evidence(self):
        path = Path(self.build["invocations"][1]["log"])
        path.write_text("RockstarEditorPlus.asi minhook.x64.lib built\nRCExecutable=" + self.tools["rc"]["path"], encoding="utf-8")
        self.build["invocations"][1]["log_sha256"] = sha256_file(path)
        self.assert_rejected("resource compiler differs from actual diagnostic command")

    def test_rehashed_source_must_still_equal_locked_base_plus_patch(self):
        self.file(self.source / "src/main.cpp", b"int mode = 999;\n")
        self.build["source_files"] = rr.source_inventory(self.source)
        digest = sha256_json(self.build["source_files"])
        self.receipt["source_provenance"]["inventory_sha256"] = digest
        self.current["source_pins"]["replus_source_inventory_sha256"] = digest
        self.assert_rejected("base plus exact patch differs from compiled source")

    def test_unlisted_source_and_unreviewed_patch_are_rejected(self):
        extra = self.file(self.source / "unlisted.hpp", b"hidden")
        self.assert_rejected("source inventory differs")
        extra.unlink()
        patch_path = Path(self.build["source_patch"]["path"])
        patch_path.write_bytes(patch_path.read_bytes() + b"\n")
        self.build["source_patch"] = record(patch_path)
        self.receipt["source_provenance"]["exact_patch"] = record(patch_path)
        self.assert_rejected("not a registered reviewed patch")

    def test_base_archive_is_reconstructed_not_only_rehashed(self):
        path = Path(self.build["source_base_archive"]["path"])
        with tarfile.open(path, "w") as handle:
            data = b"wrong base"; item = tarfile.TarInfo("CMakeLists.txt"); item.size = len(data)
            handle.addfile(item, io.BytesIO(data))
        self.build["source_base_archive"] = record(path)
        self.assert_rejected("not the exact locked Git tree")

    def test_rehashed_cache_from_other_source_and_toolchain_override_rejected(self):
        self.cache["CMAKE_HOME_DIRECTORY"] = str(self.root / "other-source")
        self.receipt["build_evidence"]["cache"] = record(self.write_cache())
        self.assert_rejected("CMAKE_HOME_DIRECTORY does not bind")
        self.cache["CMAKE_HOME_DIRECTORY"] = str(self.source)
        self.cache["CMAKE_TOOLCHAIN_FILE"] = str(self.root / "injected.cmake")
        self.receipt["build_evidence"]["cache"] = record(self.write_cache())
        self.assert_rejected("unsupported CMake override")

    def test_rehashed_project_cannot_compile_outside_snapshot(self):
        path = Path(self.receipt["build_evidence"]["project"]["path"])
        root = ET.parse(path).getroot()
        ET.SubElement(root, "ClCompile", Include=str(self.file(self.root / "extra.cpp", b"extra")))
        path.write_bytes(ET.tostring(root))
        self.receipt["build_evidence"]["project"] = record(path)
        self.assert_rejected("project compiles an unsealed source")

    def test_actual_msbuild_and_invocation_not_inferred(self):
        self.tools["msbuild"] = record(self.file(self.root / "other/MSBuild.exe", b"other MSBuild"))
        self.assert_rejected("MSBuild differs from actual configure log")

    def test_failed_or_changed_actual_invocation_rejected(self):
        self.build["invocations"][1]["returncode"] = 1
        self.assert_rejected("actual RE+ invocation differs")
        self.build["invocations"][1]["returncode"] = 0
        self.build["invocations"][1]["argv"] += ["--target", "unrelated"]
        self.assert_rejected("actual RE+ invocation differs")

    def test_installed_artifact_and_render_loader_reject_wrong_asi(self):
        self.current["components"]["replus_asi"] = record(self.file(self.root / "wrong.asi", b"wrong"))
        current = self.loaded()
        self.assertEqual(_check_capture_build_receipt(current).status, "FAIL")
        with self.assertRaisesRegex(ContractError, "installed RE\\+ differs"):
            load_render_profile_binding(current)


if __name__ == "__main__":
    unittest.main()

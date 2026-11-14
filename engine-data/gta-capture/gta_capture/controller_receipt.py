"""Verify a controller-only derivation without relabeling a full-stack rebuild.

These are local, unsigned build records. They bind preserved files and actual
invocations; they do not grant runtime, calibration, rights or delivery approval.
The parent v1 verifier remains responsible for its original full-stack contract.
"""

from __future__ import annotations

import copy
from datetime import datetime
import hashlib
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import subprocess
import tempfile
import xml.etree.ElementTree as ET
import zipfile

from .errors import ContractError
from .jsonio import load_json, sha256_file, sha256_json


CONTROLLER_DERIVED_SCHEMA = "dm-gta-capture-build-receipt/2"
_PREFIX = "engine-data/gta-capture/"
_SCOPES = tuple(_PREFIX + name for name in
                ("native/CMakeLists.txt", "native/src", "native/include", "tests/fixtures"))
_CONTROLLER = "diogenes_controller_asi"
_RECEIPT = "capture_build_receipt"
_TOOLS = {"cmake", "ctest", "msbuild", "compiler", "linker"}
_NATIVE_TESTS = (
    "gta_controller_protocol_selftest", "gta_native_tape_golden_contract",
    "gta_process_runtime_lifecycle_selftest", "gta_editor_export_plan_contract",
    "gta_story_route_contract", "gta_recording_camera_contract",
    "gta_editor_identity_observer_access_selftest", "gta_hash_utils_small_stack_selftest",
    "gta_stage_plan_contract",
)


def _require(ok, message):
    if not ok:
        raise ContractError("controller derivation: " + message)


def _absolute(value, label):
    _require(isinstance(value, str) and Path(value).is_absolute(), label + " must be absolute")
    return Path(value).resolve()


def _record(record, label, *, allow_empty=False):
    _require(isinstance(record, dict), label + " file record is missing")
    path = _absolute(record.get("path"), label)
    size, digest = record.get("bytes"), record.get("sha256")
    _require(isinstance(size, int) and not isinstance(size, bool)
             and size >= (0 if allow_empty else 1)
             and isinstance(digest, str) and re.fullmatch(r"[0-9a-f]{64}", digest),
             label + " hash/length is invalid")
    _require(path.is_file() and path.stat().st_size == size and sha256_file(path) == digest,
             label + " bytes/sha256 changed")
    return path


def _relative(value):
    _require(isinstance(value, str), "inventory path is missing")
    path = PurePosixPath(value.replace("\\", "/"))
    _require(not path.is_absolute() and bool(path.parts)
             and all(part not in (".", "..") for part in path.parts)
             and ":" not in str(path), "inventory path escapes its root")
    return path.as_posix()


def _inventory(root, records, label, *, subtrees=None):
    _require(root.is_dir() and isinstance(records, list) and 0 < len(records) <= 512,
             label + " inventory is missing")
    expected = {}
    for item in records:
        _require(isinstance(item, dict), label + " inventory entry is invalid")
        relative = _relative(item.get("path"))
        _require(relative.casefold() not in {name.casefold() for name in expected},
                 label + " inventory contains duplicate paths")
        path = root / relative
        _require(path.resolve().is_relative_to(root), label + " inventory escapes its root")
        _record({**item, "path": str(path)}, label + " " + relative)
        expected[relative] = item
    actual = set()
    for subtree in subtrees or (".",):
        directory = root / subtree
        _require(directory.is_dir(), label + " subtree is missing")
        for path in directory.rglob("*"):
            _require(not path.is_symlink() and not getattr(path, "is_junction", lambda: False)(),
                     label + " must not contain links")
            if path.is_file():
                actual.add(path.relative_to(root).as_posix())
    _require(actual == set(expected), label + " has missing or unlisted files")
    return expected


def _git(repository, arguments, *, cwd=None):
    executable = shutil.which("git")
    _require(executable is not None, "Git is required to verify the frozen source derivation")
    environment = {key: value for key, value in os.environ.items()
                   if key not in {"GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE"}}
    command = [executable, *([] if cwd else ["-C", str(repository)]), *arguments]
    result = subprocess.run(command, cwd=cwd, env=environment, capture_output=True, timeout=30)
    _require(result.returncode == 0,
             "Git source verification failed: " + result.stderr.decode("utf-8", "replace")[:300])
    return result.stdout


def _normalized(data, relative):
    # Only textual source/config inputs receive Git's Windows checkout newline
    # normalization. The raw snapshot inventory still binds the compiled bytes.
    if Path(relative).suffix in (".cpp", ".hpp", ".json") or relative.endswith("/CMakeLists.txt"):
        return data.replace(b"\r\n", b"\n")
    return data


def _verify_source(build, provenance):
    _require(isinstance(provenance, dict), "source provenance is missing")
    source = _absolute(build.get("source_snapshot"), "source snapshot")
    inventory = _inventory(source, build.get("source_files"), "source snapshot")
    _require(all(name.startswith(("native/", "tests/fixtures/")) for name in inventory)
             and "native/CMakeLists.txt" in inventory
             and "tests/fixtures/golden-camera-tape.json" in inventory
             and "tests/fixtures/golden-camera-tape.bin" in inventory,
             "source snapshot does not contain the controller build closure")
    digest = sha256_json(build["source_files"])
    _require(provenance.get("inventory_sha256") == digest
             and provenance.get("text_normalization") == "crlf-to-lf",
             "source inventory digest/normalization differs")
    base = provenance.get("base_commit")
    _require(isinstance(base, str) and re.fullmatch(r"[0-9a-f]{40}", base)
             and base == build.get("git_base_commit"), "base commit differs from the actual build")
    repository = _absolute(provenance.get("repository"), "source repository")
    _require(_git(repository, ["rev-parse", "--verify", base + "^{commit}"]).strip().decode() == base,
             "base commit object is absent")
    tree = _git(repository, ["ls-tree", "-r", "-z", base, "--", *_SCOPES])
    baseline = {}
    for entry in tree.split(b"\0"):
        if not entry:
            continue
        metadata, full_name = entry.split(b"\t", 1)
        mode, kind, blob = metadata.split()
        name = full_name.decode("utf-8")
        _require(mode in (b"100644", b"100755") and kind == b"blob" and name.startswith(_PREFIX),
                 "base source contains unsupported file types")
        relative = _relative(name[len(_PREFIX):])
        baseline[relative] = _git(repository, ["cat-file", "blob", blob.decode()])
    _require(bool(baseline), "base source tree is empty")
    additions = provenance.get("untracked_files")
    _require(isinstance(additions, list), "untracked source inventory is missing")
    added_names = [_relative(item.get("path")) for item in additions if isinstance(item, dict)]
    _require(len(added_names) == len(additions) == len(set(added_names))
             and set(added_names) == set(inventory) - set(baseline),
             "untracked source inventory differs from the frozen additions")
    _require(all(item == inventory[name] for item, name in zip(additions, added_names)),
             "untracked source hashes differ from the full inventory")
    patch = _record(provenance.get("tracked_patch"), "tracked source patch", allow_empty=True)
    if patch.stat().st_size:
        changes = _git(repository, ["apply", "--numstat", "-z", str(patch)])
        for row in changes.split(b"\0"):
            if not row:
                continue
            fields = row.split(b"\t", 2)
            _require(len(fields) == 3 and fields[2].decode("utf-8").startswith(_PREFIX),
                     "tracked patch has unsupported paths or renames")
            _require(fields[2].decode("utf-8")[len(_PREFIX):] in baseline,
                     "tracked patch modifies files outside the base source closure")
    with tempfile.TemporaryDirectory(prefix="gta-controller-source-verify-") as temporary:
        scratch = Path(temporary)
        for relative, content in baseline.items():
            path = scratch / _PREFIX / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(content)
        if patch.stat().st_size:
            _git(repository, ["apply", "--binary", "--whitespace=nowarn", str(patch)], cwd=scratch)
        derived = scratch / _PREFIX
        actual = {path.relative_to(derived).as_posix() for path in derived.rglob("*") if path.is_file()}
        _require(actual == set(baseline), "tracked source deletions or additions are unsupported")
        for relative in baseline:
            path = derived / relative
            _require(not path.is_symlink() and relative in inventory
                     and _normalized(path.read_bytes(), relative)
                     == _normalized((source / relative).read_bytes(), relative),
                     "base plus patch differs from snapshot: " + relative)
    return source, inventory, digest


def _cache_values(path):
    values = {}
    for line in path.read_text(encoding="utf-8").splitlines():
        match = re.match(r"([^/#][^:]*):[^=]+=(.*)$", line)
        if match:
            _require(match[1] not in values, "CMake cache has duplicate keys")
            values[match[1]] = match[2]
    return values


def _verify_ctest_log(text):
    # CTest 4.4 omits the zero-failure phrase. Both summaries must still be
    # accompanied by every expected contract's unique successful result line.
    summary = re.findall(r"^100% tests passed(?:, 0 tests failed)? out of (\d+)\s*$",
                         text, re.M)
    rows = re.findall(r"^\s*(\d+)/(\d+) Test\s+#(\d+):\s+(\S+)\s+\.+\s+Passed\s+[\d.]+ sec\s*$",
                      text, re.M)
    total = str(len(_NATIVE_TESTS))
    expected = {(str(index), total, str(index), name)
                for index, name in enumerate(_NATIVE_TESTS, 1)}
    _require(summary == [total] and len(rows) == len(_NATIVE_TESTS) and set(rows) == expected
             and not re.search(r"\*\*\*Failed|\*\*\*Not Run|The following tests FAILED", text),
             f"full native CTest did not report all {total} contracts passing")


def _verify_build(build, evidence, source, inventory, parent_manifest):
    _require(isinstance(evidence, dict), "build evidence is missing")
    _record(evidence.get("builder"), "actual candidate builder")
    tools = evidence.get("tools")
    _require(isinstance(tools, dict) and set(tools) == _TOOLS, "toolchain inventory is incomplete")
    paths = {key: _record(value, key + " executable") for key, value in tools.items()}
    cache_path = _record(evidence.get("cache"), "CMake cache")
    build_dir = cache_path.parent
    _require(cache_path.name == "CMakeCache.txt" and not build_dir.is_relative_to(source),
             "build directory must be outside the frozen source")
    cache = _cache_values(cache_path)
    configure_log = _record(evidence.get("configure_log"), "CMake configure log")
    _require(configure_log == build_dir / "CMakeFiles/CMakeConfigureLog.yaml",
             "configure log belongs to another build")
    msbuild_commands = re.findall(r'Run Build Command\(s\):\s*"([^"\n]*[Mm][Ss][Bb]uild\.exe)"',
                                  configure_log.read_text(encoding="utf-8"))
    _require(bool(msbuild_commands) and
             {_absolute(value, "actual MSBuild") for value in msbuild_commands} == {paths["msbuild"]},
             "MSBuild identity differs from the actual CMake configure log")
    sdk = _absolute(cache.get("SCRIPT_HOOK_V_SDK_ROOT"), "SDK root")
    for key, expected in {
        "CMAKE_HOME_DIRECTORY": source / "native", "CMAKE_COMMAND": paths["cmake"],
        "CMAKE_CTEST_COMMAND": paths["ctest"], "CMAKE_LINKER": paths["linker"],
        "SCRIPT_HOOK_V_INCLUDE_DIR": sdk / "inc", "SCRIPT_HOOK_V_LIBRARY": sdk / "lib/ScriptHookV.lib",
    }.items():
        _require(_absolute(cache.get(key), key) == expected, key + " differs from sealed build inputs")
    _require(cache.get("DIOGENES_BUILD_SCRIPT_HOOK_V_ASI") == "ON"
             and cache.get("CMAKE_GENERATOR_PLATFORM") == "x64"
             and cache.get("CMAKE_GENERATOR", "").startswith("Visual Studio "),
             "build is not the Windows x64 ASI configuration")
    _require(cache.get("CMAKE_CXX_FLAGS") == "/DWIN32 /D_WINDOWS /EHsc"
             and cache.get("CMAKE_CXX_FLAGS_RELEASE") == "/O2 /Ob2 /DNDEBUG",
             "unsupported compiler flag overrides")
    for key in ("CMAKE_TOOLCHAIN_FILE", "CMAKE_PROJECT_INCLUDE", "CMAKE_PROJECT_INCLUDE_BEFORE",
                "CMAKE_CXX_COMPILER_LAUNCHER", "CMAKE_CXX_LINKER_LAUNCHER"):
        _require(not cache.get(key), "unsupported CMake override: " + key)
    compiler_file = _record(evidence.get("compiler_config"), "CMake compiler configuration")
    _require(compiler_file.is_relative_to(build_dir), "compiler configuration belongs to another build")
    compiler = dict(re.findall(r'set\((\w+) "([^"\n]*)"\)', compiler_file.read_text(encoding="utf-8")))
    _require(_absolute(compiler.get("CMAKE_CXX_COMPILER"), "compiler") == paths["compiler"]
             and compiler.get("CMAKE_CXX_COMPILER_ID") == "MSVC"
             and compiler.get("CMAKE_CXX_COMPILER_ARCHITECTURE_ID") == "x64"
             and bool(compiler.get("CMAKE_CXX_COMPILER_VERSION")), "compiler identity is not bound")
    archive = _record(build.get("sdk_archive"), "ScriptHook SDK archive")
    _require(build["sdk_archive"]["sha256"] == parent_manifest.raw["source_pins"].get("scripthook_sdk_zip_sha256"),
             "SDK archive differs from the parent lock")
    sdk_inputs = _inventory(sdk, build.get("sdk_compiler_inputs"), "SDK compiler inputs", subtrees=("inc", "lib"))
    with zipfile.ZipFile(archive) as handle:
        members = {}
        for member in handle.infolist():
            if member.is_dir():
                continue
            name = _relative(member.filename)
            if name.startswith(("inc/", "lib/")):
                _require(name not in members, "SDK archive contains duplicate compiler inputs")
                members[name] = handle.read(member)
        _require(set(members) == set(sdk_inputs), "SDK input inventory differs from the pinned archive")
        for name, content in members.items():
            _require(len(content) == sdk_inputs[name]["bytes"]
                     and hashlib.sha256(content).hexdigest() == sdk_inputs[name]["sha256"],
                     "SDK compiler input differs from the pinned archive: " + name)
    project_path = _record(evidence.get("project"), "controller project")
    _require(project_path == build_dir / "DiogenesGtaController.vcxproj", "controller project belongs to another build")
    project = ET.parse(project_path).getroot()
    compiled = [item.attrib["Include"] for item in project.iter()
                if item.tag.rsplit("}", 1)[-1] == "ClCompile" and "Include" in item.attrib]
    _require(bool(compiled), "controller project contains no source files")
    for value in compiled:
        path = _absolute(value, "compiled source")
        _require(path.is_relative_to(source) and path.relative_to(source).as_posix() in inventory,
                 "controller project compiles outside the frozen source inventory")
    cmake_text = (source / "native/CMakeLists.txt").read_text(encoding="utf-8")
    target = re.search(r"add_library\(DiogenesGtaController\s+SHARED\s+(.*?)\)", cmake_text, re.S)
    _require(target is not None, "frozen CMake source has no controller target")
    target_sources = target[1].split()
    _require(all(re.fullmatch(r"src/[\w]+\.cpp", name) for name in target_sources)
             and {_absolute(value, "compiled source") for value in compiled}
             == {source / "native" / name for name in target_sources},
             "generated controller project differs from the frozen CMake target")
    for item in project.iter():
        tag = item.tag.rsplit("}", 1)[-1]
        if tag == "AdditionalIncludeDirectories":
            includes = (item.text or "").split(";")
            _require(set(includes) == {str(source / "native/include"), str(sdk / "inc"),
                                      "%(AdditionalIncludeDirectories)"},
                     "controller project includes unsealed directories")
        if tag == "AdditionalDependencies":
            libraries = (item.text or "").split(";")
            _require(str(sdk / "lib/ScriptHookV.lib") in libraries,
                     "controller project does not link the sealed ScriptHook SDK")
            allowed = {"bcrypt.lib", "shell32.lib", "ole32.lib", "kernel32.lib", "user32.lib",
                       "gdi32.lib", "winspool.lib", "oleaut32.lib", "uuid.lib", "comdlg32.lib", "advapi32.lib"}
            _require(all(name == str(sdk / "lib/ScriptHookV.lib") or name.lower() in allowed
                         for name in libraries), "controller project links unsealed libraries")
    properties = {item.tag.rsplit("}", 1)[-1]: item.text for item in project.iter()
                  if item.tag.rsplit("}", 1)[-1] in {"PlatformToolset", "WindowsTargetPlatformVersion"}}
    _require(all(properties.get(name) for name in ("PlatformToolset", "WindowsTargetPlatformVersion")),
             "project toolset/Windows SDK version is absent")
    invocations = build.get("invocations")
    _require(isinstance(invocations, list) and len(invocations) == 3,
             "controller build requires actual configure/build/camera-contract-test invocations")
    expected = [
        (paths["cmake"], ["-S", str(source / "native"), "-B", str(build_dir), "-A", "x64",
                          "-DDIOGENES_BUILD_SCRIPT_HOOK_V_ASI=ON", "-DSCRIPT_HOOK_V_SDK_ROOT=" + str(sdk)]),
        (paths["cmake"], ["--build", str(build_dir), "--config", "Release", "--parallel", "2"]),
        (paths["ctest"], ["--test-dir", str(build_dir), "-C", "Release", "--output-on-failure"]),
    ]
    for item, (executable, argv) in zip(invocations, expected):
        _require(isinstance(item, dict) and _absolute(item.get("executable"), "invocation executable") == executable
                 and item.get("argv") == argv and type(item.get("returncode")) is int and item["returncode"] == 0,
                 "actual controller invocation/exit code differs from the supported build")
        log = _absolute(item.get("log"), "build log")
        _require(log.is_file() and log.stat().st_size > 0 and sha256_file(log) == item.get("log_sha256"),
                 "actual build/test log changed or is absent")
    test_log = Path(invocations[2]["log"]).read_text(encoding="utf-8")
    _verify_ctest_log(test_log)
    try:
        started = datetime.fromisoformat(build["started_at"].replace("Z", "+00:00"))
        finished = datetime.fromisoformat(build["finished_at"].replace("Z", "+00:00"))
        _require(started.tzinfo is not None and finished.tzinfo is not None and started < finished,
                 "actual build timestamps are invalid")
    except (KeyError, TypeError, ValueError) as exc:
        raise ContractError("controller derivation: actual build timestamps are missing") from exc
    _require(build.get("asi_built") is True and build.get("portable_contract_passed") is True,
             "actual controller build/camera-contract test did not pass")
    artifact = _record(build.get("artifact"), "built controller ASI")
    _require(artifact == build_dir / "Release/DiogenesGtaController.asi", "ASI belongs to another build")
    return build["artifact"]


def validate_controller_receipt(manifest, receipt, *, validate_parent):
    """Return the inherited v1 render profile only after the whole derivation verifies."""
    from .preflight import DependencyManifest

    _require(isinstance(receipt, dict) and receipt.get("schema") == CONTROLLER_DERIVED_SCHEMA
             and receipt.get("kind") == "controller_only_derived", "receipt kind/schema is unsupported")
    receipt_item = manifest.raw["components"].get(_RECEIPT, {})
    current_receipt = manifest.component_path(_RECEIPT)
    _require(current_receipt is not None and current_receipt.is_file()
             and sha256_file(current_receipt) == receipt_item.get("sha256")
             and load_json(current_receipt) == receipt, "receipt differs from the current manifest pin")
    parent = receipt.get("parent")
    _require(isinstance(parent, dict), "immutable parent records are absent")
    parent_path = _record(parent.get("manifest"), "parent manifest")
    parent_receipt_path = _record(parent.get("receipt"), "parent receipt")
    backup = _record(parent.get("controller_backup"), "parent controller backup")
    original = DependencyManifest.load(parent_path)
    original_receipt = load_json(parent_receipt_path)
    _require(isinstance(original_receipt, dict)
             and original_receipt.get("schema") == "dm-gta-capture-build-receipt/1",
             "only a single immutable v1 parent is supported")
    old_components = original.raw["components"]
    _require(old_components.get(_RECEIPT, {}).get("sha256") == parent["receipt"]["sha256"]
             and old_components.get(_CONTROLLER, {}).get("sha256") == parent["controller_backup"]["sha256"],
             "parent receipt/controller backup differs from the parent manifest")
    parent_view = copy.deepcopy(original.raw)
    parent_view["components"][_RECEIPT] = parent["receipt"]
    parent_view["components"][_CONTROLLER] = parent["controller_backup"]
    checked_parent = DependencyManifest(original.path, parent_view, original.policy)
    parent_check = validate_parent(checked_parent)
    _require(parent_check.status == "PASS", "original v1 parent failed: " + str(parent_check.evidence))
    build_path = _record(receipt.get("controller_build"), "actual controller build receipt")
    build = load_json(build_path)
    _require(isinstance(build, dict) and build.get("schema") == "dm-gta-controller-staging-build/1"
             and build.get("working_tree_candidate") is True, "actual controller snapshot build is absent")
    _require(build.get("parent_capture_build_receipt", {}).get("sha256") == parent["receipt"]["sha256"],
             "controller build has a different full-stack parent")
    source, inventory, digest = _verify_source(build, receipt.get("source_provenance"))
    source_pins = copy.deepcopy(original.raw["source_pins"])
    source_pins["controller_git_commit"] = build["git_base_commit"]
    source_pins["controller_source_inventory_sha256"] = digest
    _require(manifest.raw.get("source_pins") == source_pins,
             "source pins changed outside the controller base/snapshot identity")
    for key in set(original.raw) | set(manifest.raw):
        if key not in {"source_pins", "components"}:
            _require(original.raw.get(key) == manifest.raw.get(key), "non-controller manifest field changed: " + key)
    components = manifest.raw["components"]
    _require(set(components) == set(old_components), "component inventory changed")
    for name, item in old_components.items():
        if name in {_RECEIPT, _CONTROLLER}:
            continue
        _require(components[name] == item, "unchanged component pin changed: " + name)
        path = manifest.component_path(name)
        _require(path is not None and path.is_file() and sha256_file(path) == item.get("sha256"),
                 "inherited component bytes changed: " + name)
    artifact = _verify_build(build, receipt.get("build_evidence"), source, inventory, original)
    controller = manifest.component_path(_CONTROLLER)
    _require(controller is not None and controller.is_file()
             and components[_CONTROLLER].get("sha256") == artifact["sha256"]
             and controller.stat().st_size == artifact["bytes"] and sha256_file(controller) == artifact["sha256"],
             "deployed controller differs from the built ASI")
    effective = copy.deepcopy(original_receipt)
    effective["artifacts"][_CONTROLLER] = artifact
    return effective

"""Strict RE+-only derivation from one preserved controller-v2/full-stack-v1 chain.

These unsigned local records bind source and build files, not runtime behavior,
projection equivalence, commercial rights, or calibration/delivery approval.
The v1 and controller-only v2 contracts are neither changed nor generalized.
"""
from __future__ import annotations

import copy
from dataclasses import dataclass
from datetime import datetime
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import subprocess
import tarfile
import tempfile
import xml.etree.ElementTree as ET

from .controller_receipt import CONTROLLER_DERIVED_SCHEMA, validate_controller_receipt
from .errors import ContractError
from .jsonio import load_json, sha256_file, sha256_json

REPLUS_DERIVED_SCHEMA = "dm-gta-capture-build-receipt/3"
REPLUS_BUILD_SCHEMA = "dm-gta-replus-staging-build/1"
REPLUS_COMMIT = "c6eba57160ca444817b149b3304ed1454e0ba2d9"
# This historical pin and the original v1/v2 pin remain unchanged. Each later
# full patch is registered separately after review, never supplied by a receipt.
REPLUS_PATCH_SHA256 = "41a55dfab049ebdcf761826b4ca7ccc4c546ed9343f24c5524fd5d9cd2e37191"
REPLUS_CURSOR_REFRESH_PATCH_SHA256 = "c6b9e866a7ce3f6dc08159f63210fd6cdcf1fdd60c8fdef1eafbef63fd9c006a"
REPLUS_DRAW_GUARD_PATCH_SHA256 = "c77b19756ef40cf6bfd63d725dc48e74da79a6f1a529c73d20971516ec1ef53e"


@dataclass(frozen=True)
class ReviewedReplusPatch:
    filename: str
    contract_assertions: int


REVIEWED_REPLUS_PATCHES = {
    REPLUS_PATCH_SHA256: ReviewedReplusPatch("replus-v18-story-idle-state.patch", 9),
    REPLUS_CURSOR_REFRESH_PATCH_SHA256: ReviewedReplusPatch("replus-v18-cursor-refresh.patch", 14),
    REPLUS_DRAW_GUARD_PATCH_SHA256: ReviewedReplusPatch("replus-v18-draw-guard.patch", 27),
}
_COMPONENT = "replus_asi"
_RECEIPT = "capture_build_receipt"
_TOOLS = {"cmake", "msbuild", "compiler", "linker", "rc"}
REPLUS_SCOPE = {"only_component_rebuilt": "replus_asi", "configuration_replaced": False,
                "runtime_verified": False, "calibration_approved": False, "delivery_eligible": False}


def _require(condition, message):
    if not condition:
        raise ContractError("RE+ derivation: " + message)


def is_reviewed_replus_patch(digest):
    return isinstance(digest, str) and digest in REVIEWED_REPLUS_PATCHES


def reviewed_replus_patch(digest):
    _require(is_reviewed_replus_patch(digest), "exact RE+ patch is not a registered reviewed patch")
    return REVIEWED_REPLUS_PATCHES[digest]


def _absolute(value, label):
    _require(isinstance(value, str) and Path(value).is_absolute(), label + " must be absolute")
    return Path(value).resolve()


def _record(value, label):
    _require(isinstance(value, dict), label + " record is missing")
    path = _absolute(value.get("path"), label)
    _require(type(value.get("bytes")) is int and value["bytes"] > 0
             and isinstance(value.get("sha256"), str)
             and re.fullmatch(r"[0-9a-f]{64}", value["sha256"]), label + " length/hash malformed")
    _require(path.is_file() and path.stat().st_size == value["bytes"]
             and sha256_file(path) == value["sha256"], label + " bytes/sha256 changed")
    return path


def _relative(value):
    _require(isinstance(value, str) and "\\" not in value and ":" not in value,
             "invalid source path")
    path = PurePosixPath(value)
    _require(not path.is_absolute() and bool(path.parts)
             and all(p not in {".", "..", ".git"} for p in path.parts), "source path escapes snapshot")
    return path.as_posix()


def source_inventory(root):
    """All source files except Git administration; build must be elsewhere."""
    root = Path(root).resolve()
    _require(root.is_dir(), "source snapshot missing")
    found = []
    for current, directories, files in os.walk(root):
        directory = Path(current)
        # Only root Git administration is excluded, never arbitrary build or
        # dependency directories. Links are rejected even when not followed.
        for name in directories + files:
            path = directory / name
            _require(not path.is_symlink() and not getattr(path, "is_junction", lambda: False)(),
                     "source snapshot contains a link")
        if directory == root and ".git" in directories:
            directories.remove(".git")
        for name in files:
            path = directory / name
            relative = _relative(path.relative_to(root).as_posix())
            found.append({"path": relative, "bytes": path.stat().st_size, "sha256": sha256_file(path)})
    return sorted(found, key=lambda item: item["path"])


def _git(repository, argv, *, cwd=None):
    executable = shutil.which("git")
    _require(executable is not None, "Git is required for source reconstruction")
    environment = {k: v for k, v in os.environ.items() if not k.startswith("GIT_")}
    if cwd:
        environment["GIT_CEILING_DIRECTORIES"] = str(Path(cwd).resolve())
    # This repository is the exact frozen source root already named by the
    # receipt. Trust it only for this invocation, never globally or by wildcard.
    command = [executable, "-c", "safe.directory=" + str(Path(repository).resolve()),
               *([] if cwd else ["-C", str(repository)]), *argv]
    result = subprocess.run(command, cwd=cwd, env=environment, capture_output=True, timeout=30)
    _require(result.returncode == 0, "Git source verification failed: " + result.stderr.decode("utf-8", "replace")[:300])
    return result.stdout


def _normalized(data, name):
    if Path(name).suffix.lower() in {".cpp", ".c", ".h", ".hpp", ".cmake", ".txt", ".rc", ".md", ".yml", ".yaml"} or name.endswith(".gitignore"):
        return data.replace(b"\r\n", b"\n")
    return data


def _verify_source(build, provenance):
    _require(isinstance(provenance, dict), "source provenance missing")
    source = _absolute(build.get("source_snapshot"), "source snapshot")
    inventory = build.get("source_files")
    _require(isinstance(inventory, list) and 0 < len(inventory) <= 512
             and sorted(inventory, key=lambda r: r["path"]) == source_inventory(source),
             "source inventory differs from complete frozen tree")
    _require(len({r["path"].casefold() for r in inventory}) == len(inventory), "source path duplicates")
    digest = sha256_json(inventory)
    _require(provenance.get("inventory_sha256") == digest
             and provenance.get("text_normalization") == "utf8-text-crlf-to-lf", "source inventory/normalization changed")
    _require(build.get("git_base_commit") == REPLUS_COMMIT
             and provenance.get("base_commit") == REPLUS_COMMIT, "RE+ base differs from locked commit")
    repository = _absolute(provenance.get("repository"), "source object repository")
    _require(_git(repository, ["rev-parse", "--verify", REPLUS_COMMIT + "^{commit}"]).strip().decode() == REPLUS_COMMIT,
             "locked RE+ commit object unavailable")
    baseline = {}
    for row in _git(repository, ["ls-tree", "-r", "-z", REPLUS_COMMIT]).split(b"\0"):
        if not row:
            continue
        metadata, name = row.split(b"\t", 1)
        mode, kind, blob = metadata.split()
        _require(mode in (b"100644", b"100755") and kind == b"blob", "unsupported base links/submodules")
        relative = _relative(name.decode("utf-8"))
        baseline[relative] = _git(repository, ["cat-file", "blob", blob.decode()])
    _require("CMakeLists.txt" in baseline and any(p.startswith("src/vendor/minhook/") for p in baseline),
             "locked base lacks complete RE+/vendored minhook sources")
    archive = _record(build.get("source_base_archive"), "locked Git base archive")
    with tarfile.open(archive, "r:*") as handle:
        archived = {}
        for member in handle.getmembers():
            if member.isdir():
                continue
            _require(member.isfile(), "base archive contains links/unsupported entries")
            name = _relative(member.name)
            _require(name not in archived, "duplicate base archive entry")
            archived[name] = handle.extractfile(member).read()
    _require(archived == baseline, "base archive is not the exact locked Git tree")
    patch = _record(provenance.get("exact_patch"), "exact complete RE+ patch")
    _record(build.get("source_patch"), "patch used by actual candidate builder")
    reviewed_replus_patch(provenance["exact_patch"]["sha256"])
    _require(build["source_patch"]["sha256"] == provenance["exact_patch"]["sha256"],
             "actual candidate build patch differs from reviewed exact patch")
    for row in _git(repository, ["apply", "--numstat", "-z", str(patch)]).split(b"\0"):
        if row:
            fields = row.split(b"\t", 2)
            _require(len(fields) == 3 and fields[2], "unsupported patch rename")
            _relative(fields[2].decode("utf-8"))
    with tempfile.TemporaryDirectory(prefix="gta-replus-source-verify-") as directory:
        scratch = Path(directory)
        for name, content in baseline.items():
            path = scratch / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(content)
        _git(repository, ["apply", "--binary", "--whitespace=nowarn", str(patch)], cwd=scratch)
        derived = source_inventory(scratch)
        _require({r["path"] for r in derived} == {r["path"] for r in inventory},
                 "base plus exact patch file closure differs from snapshot")
        for row in inventory:
            name = row["path"]
            _require(_normalized((scratch / name).read_bytes(), name)
                     == _normalized((source / name).read_bytes(), name),
                     "base plus exact patch differs from compiled source: " + name)
    return source, {r["path"]: r for r in inventory}, digest


def _cache_values(path):
    values = {}
    for line in path.read_text(encoding="utf-8").splitlines():
        match = re.match(r"([^/#][^:]*):[^=]+=(.*)$", line)
        if match:
            _require(match[1] not in values, "duplicate CMake cache key")
            values[match[1]] = match[2]
    return values


def observed_rc_paths(text):
    """Only executable command lines; SDK property guesses do not qualify."""
    values = re.findall(r'^\s*"?((?:[A-Za-z]:[\\/]|/)[^"\r\n:]*?[\\/]rc\.exe)"?\s+/[A-Za-z]', text, re.M | re.I)
    return {_absolute(value, "actually invoked resource compiler") for value in values}


def _project(path, source, inventory):
    root = ET.parse(path).getroot()
    compiled = []
    resources = []
    for item in root.iter():
        tag = item.tag.rsplit("}", 1)[-1]
        if tag in {"ClCompile", "ResourceCompile"} and "Include" in item.attrib:
            value = _absolute(item.attrib["Include"], "project input")
            _require(value.is_relative_to(source) and value.relative_to(source).as_posix() in inventory,
                     "project compiles an unsealed source/resource")
            (compiled if tag == "ClCompile" else resources).append(value.relative_to(source).as_posix())
    properties = {item.tag.rsplit("}", 1)[-1]: item.text for item in root.iter()
                  if item.tag.rsplit("}", 1)[-1] in {"PlatformToolset", "WindowsTargetPlatformVersion"}}
    _require(all(properties.get(key) for key in ("PlatformToolset", "WindowsTargetPlatformVersion")),
             "project omits Windows SDK/toolset identity")
    return root, compiled, resources, properties


def _verify_build(build, evidence, source, inventory, *, reviewed_patch):
    _require(isinstance(evidence, dict), "build evidence missing")
    _record(evidence.get("builder"), "actual build helper")
    _record(build.get("builder"), "staging receipt build helper")
    _require(build["builder"] == evidence["builder"], "actual build helper differs across receipts")
    tools = evidence.get("tools")
    _require(isinstance(tools, dict) and set(tools) == _TOOLS, "toolchain inventory incomplete")
    paths = {name: _record(record, name + " executable") for name, record in tools.items()}
    cache_path = _record(evidence.get("cache"), "actual CMake cache")
    directory = cache_path.parent
    _require(cache_path.name == "CMakeCache.txt" and not directory.is_relative_to(source),
             "build must be outside frozen source")
    cache = _cache_values(cache_path)
    for key, expected in {"CMAKE_HOME_DIRECTORY": source, "CMAKE_COMMAND": paths["cmake"],
                          "CMAKE_LINKER": paths["linker"]}.items():
        _require(_absolute(cache.get(key), key) == expected, key + " does not bind actual tool/source")
    _require(cache.get("CMAKE_RC_COMPILER") == "rc" and cache.get("CMAKE_MT") == "CMAKE_MT-NOTFOUND",
             "unexpected resource/manifest compiler configuration")
    _require(cache.get("CMAKE_GENERATOR_PLATFORM") == "x64"
             and cache.get("CMAKE_GENERATOR") == "Visual Studio 17 2022"
             and cache.get("CMAKE_POLICY_VERSION_MINIMUM") == "3.5",
             "build is not supported Visual Studio x64 / explicit CMake compatibility configuration")
    for lang in ("C", "CXX"):
        expected = "/DWIN32 /D_WINDOWS" + (" /GR /EHsc" if lang == "CXX" else "")
        _require(cache.get(f"CMAKE_{lang}_FLAGS") == expected
                 and cache.get(f"CMAKE_{lang}_FLAGS_RELEASE") == "/O2 /Ob2 /DNDEBUG",
                 "unsupported compiler flag overrides")
    for key in ("CMAKE_TOOLCHAIN_FILE", "CMAKE_PROJECT_INCLUDE", "CMAKE_PROJECT_INCLUDE_BEFORE",
                "CMAKE_C_COMPILER_LAUNCHER", "CMAKE_CXX_COMPILER_LAUNCHER", "CMAKE_CXX_LINKER_LAUNCHER"):
        _require(not cache.get(key), "unsupported CMake override: " + key)
    configure = _record(evidence.get("configure_log"), "CMake configure log")
    _require(configure == directory / "CMakeFiles/CMakeConfigureLog.yaml", "configure log from another build")
    msbuilds = re.findall(r'Run Build Command\(s\):\s*"([^"\n]*[Mm][Ss][Bb]uild\.exe)"', configure.read_text(encoding="utf-8"))
    _require(msbuilds and {_absolute(v, "actual MSBuild") for v in msbuilds} == {paths["msbuild"]},
             "MSBuild differs from actual configure log")
    for lang in ("C", "CXX"):
        compiler_path = _record(evidence.get("compiler_configs", {}).get(lang), lang + " compiler config")
        _require(compiler_path.is_relative_to(directory), "compiler config from another build")
        config = dict(re.findall(r'set\((\w+) "([^"\n]*)"\)', compiler_path.read_text(encoding="utf-8")))
        _require(_absolute(config.get(f"CMAKE_{lang}_COMPILER"), lang + " compiler") == paths["compiler"]
                 and config.get(f"CMAKE_{lang}_COMPILER_ID") == "MSVC"
                 and config.get(f"CMAKE_{lang}_COMPILER_ARCHITECTURE_ID") == "x64"
                 and config.get(f"CMAKE_{lang}_COMPILER_VERSION"), "compiler identity differs")
    project_path = _record(evidence.get("project"), "RE+ project")
    minhook_path = _record(evidence.get("minhook_project"), "minhook project")
    _require(project_path == directory / "RockstarEditorPlus.vcxproj"
             and minhook_path == directory / "src/vendor/minhook/minhook.vcxproj", "project from another build")
    root, compiled, resources, properties = _project(project_path, source, inventory)
    _, minhook, _, minhook_properties = _project(minhook_path, source, inventory)
    expected = {name for name in inventory if name.startswith("src/") and name.endswith(".cpp")
                and not name.startswith("src/vendor/")}
    _require(expected and len(compiled) == len(set(compiled)) and set(compiled) == expected
             and resources == ["fx_asi_build.rc"], "RE+ project source set differs from frozen target")
    expected_minhook = {"src/vendor/minhook/src/" + name for name in
                        ("buffer.c", "hook.c", "trampoline.c", "hde/hde64.c")}
    _require(len(minhook) == len(expected_minhook) and set(minhook) == expected_minhook
             and minhook_properties == properties, "vendored minhook target/source/toolset differs")
    allowed_libs = {"version.lib", "shell32.lib", "bcrypt.lib", "kernel32.lib", "user32.lib", "gdi32.lib",
                    "winspool.lib", "ole32.lib", "oleaut32.lib", "uuid.lib", "comdlg32.lib", "advapi32.lib",
                    "minhook.x64.lib", "minhook.x64d.lib"}
    for item in root.iter():
        tag = item.tag.rsplit("}", 1)[-1]
        values = (item.text or "").split(";")
        if tag == "AdditionalIncludeDirectories":
            normalized = {"%(AdditionalIncludeDirectories)" if name == "%(AdditionalIncludeDirectories)"
                          else str(_absolute(name, "include directory")) for name in values}
            _require(normalized == {str(source / "src"), "%(AdditionalIncludeDirectories)"},
                     "unsealed include directory")
        if tag == "AdditionalLibraryDirectories":
            normalized = {"%(AdditionalLibraryDirectories)" if name == "%(AdditionalLibraryDirectories)"
                          else str(_absolute(name.replace("$(Configuration)", "Release"), "library directory")) for name in values}
            _require(normalized == {str(directory / "LIB"), str(directory / "LIB/Release"),
                                      "%(AdditionalLibraryDirectories)"}, "unsealed library search directory")
        if tag == "AdditionalDependencies":
            _require(all(name.lower() in allowed_libs for name in values), "unsealed linked library")
    invocations = build.get("invocations")
    expected_args = [["-S", str(source), "-B", str(directory), "-G", "Visual Studio 17 2022", "-A", "x64", "-DCMAKE_POLICY_VERSION_MINIMUM=3.5"],
                     ["--build", str(directory), "--config", "Release", "--parallel", "2", "--", "/v:diag"]]
    _require(isinstance(invocations, list) and len(invocations) == 2, "actual configure/build invocations missing")
    for invocation, argv in zip(invocations, expected_args):
        _require(isinstance(invocation, dict) and _absolute(invocation.get("executable"), "invoked executable") == paths["cmake"]
                 and invocation.get("argv") == argv and type(invocation.get("returncode")) is int
                 and invocation["returncode"] == 0, "actual RE+ invocation differs from supported build")
        log = _absolute(invocation.get("log"), "actual build log")
        _require(log.is_file() and log.stat().st_size > 0 and sha256_file(log) == invocation.get("log_sha256"),
                 "actual build log missing/changed")
    log_text = Path(invocations[1]["log"]).read_text(encoding="utf-8", errors="replace")
    _require(observed_rc_paths(log_text) == {paths["rc"]}, "resource compiler differs from actual diagnostic command")
    _require(paths["rc"].parent.name.casefold() == "x64"
             and paths["rc"].parent.parent.name == properties["WindowsTargetPlatformVersion"],
             "actually invoked resource compiler differs from project Windows SDK/platform")
    _require("RockstarEditorPlus.asi" in log_text and "minhook.x64.lib" in log_text,
             "build log does not contain the actual ASI/minhook outputs")
    try:
        start = datetime.fromisoformat(build["started_at"].replace("Z", "+00:00"))
        end = datetime.fromisoformat(build["finished_at"].replace("Z", "+00:00"))
        _require(start.tzinfo is not None and end.tzinfo is not None and start < end, "invalid build timestamps")
    except (KeyError, ValueError, TypeError) as exc:
        raise ContractError("RE+ derivation: actual build timestamps missing") from exc
    _require(build.get("asi_built") is True and build.get("deployed") is False
             and build.get("working_tree_candidate") is True and build.get("runtime_tested") is False
             and type(build.get("contract_assertions")) is int
             and build["contract_assertions"] == reviewed_patch.contract_assertions
             and build.get("contract_assertions_verified") is True,
             "staging build must record a successful, not-yet-deployed candidate")
    artifact = _record(build.get("artifact"), "actual built RE+ ASI")
    _require(artifact == directory / "BIN/Release/RockstarEditorPlus.asi", "ASI from another build")
    return build["artifact"]


def validate_replus_receipt(manifest, receipt, *, validate_parent, controller_backup=None):
    """Verify a v3 RE+ derivative; return effective v1 render-profile evidence."""
    from .preflight import DependencyManifest

    _require(isinstance(receipt, dict) and receipt.get("schema") == REPLUS_DERIVED_SCHEMA
             and receipt.get("kind") == "replus_only_derived", "unsupported receipt kind/schema")
    _require(receipt.get("scope") == REPLUS_SCOPE, "receipt scope must not claim runtime/calibration/delivery approval")
    current = manifest.component_path(_RECEIPT)
    _require(current is not None and current.is_file()
             and sha256_file(current) == manifest.raw["components"][_RECEIPT].get("sha256")
             and load_json(current) == receipt, "receipt differs from current manifest pin")
    parent = receipt.get("parent")
    _require(isinstance(parent, dict), "preserved parent missing")
    parent_path = _record(parent.get("manifest"), "parent manifest")
    parent_receipt_path = _record(parent.get("receipt"), "parent v2 receipt")
    backup = _record(parent.get("replus_backup"), "parent RE+ backup")
    original = DependencyManifest.load(parent_path)
    previous = load_json(parent_receipt_path)
    _require(isinstance(previous, dict) and previous.get("schema") == CONTROLLER_DERIVED_SCHEMA and previous.get("kind") == "controller_only_derived",
             "only one strict controller-v2 parent is supported; recursive v3 is forbidden")
    components = original.raw["components"]
    _require(components[_RECEIPT].get("sha256") == parent["receipt"]["sha256"]
             and components[_COMPONENT].get("sha256") == parent["replus_backup"]["sha256"],
             "parent receipt/RE+ backup differs from immutable manifest")

    # A later controller-only derivation may replace the shared install path.
    # This resolves only the exact unchanged controller inherited by this v3
    # and its v2 parent; the v1 controller has its own preserved backup.
    historical_controller = None
    if controller_backup is not None:
        historical_controller = _record(controller_backup, "historical controller backup")
        _require(controller_backup["sha256"] == components["diogenes_controller_asi"].get("sha256")
                 == manifest.raw["components"]["diogenes_controller_asi"].get("sha256"),
                 "historical controller backup differs from immutable controller pin")

    class HistoricalReplus(DependencyManifest):
        def component_path(self, name):
            # Preserve raw manifest equality; only resolve the two exact
            # hash-checked historical files away from a replaced install path.
            if name == _COMPONENT:
                return backup
            if (name == "diogenes_controller_asi" and historical_controller is not None
                    and self.raw["components"].get(name, {}).get("sha256") == controller_backup["sha256"]):
                return historical_controller
            return super().component_path(name)

    class PreservedParent(HistoricalReplus):
        def component_path(self, name):
            if name == _RECEIPT:
                return parent_receipt_path
            return super().component_path(name)

    view = PreservedParent(original.path, original.raw, original.policy)
    def check_historical_v1(v1):
        return validate_parent(HistoricalReplus(v1.path, v1.raw, v1.policy))
    effective = validate_controller_receipt(view, previous, validate_parent=check_historical_v1)
    old_artifact = effective["artifacts"][_COMPONENT]
    _require(all(parent["replus_backup"][key] == old_artifact[key] for key in ("bytes", "sha256")),
             "parent RE+ backup differs from original v1 artifact")
    for key in set(original.raw) | set(manifest.raw):
        if key not in {"components", "source_pins"}:
            _require(original.raw.get(key) == manifest.raw.get(key), "non-RE+ manifest field changed: " + key)
    updated = manifest.raw["components"]
    _require(set(updated) == set(components), "component inventory changed")
    for name, item in components.items():
        if name not in {_COMPONENT, _RECEIPT}:
            _require(updated[name] == item, "unchanged component pin changed: " + name)
            path = manifest.component_path(name)
            _require(path is not None and path.is_file() and sha256_file(path) == item.get("sha256"),
                     "inherited component bytes changed: " + name)
    build_path = _record(receipt.get("replus_build"), "actual RE+ build receipt")
    build = load_json(build_path)
    _require(isinstance(build, dict) and build.get("schema") == REPLUS_BUILD_SCHEMA
             and build.get("parent_capture_build_receipt", {}).get("sha256") == parent["receipt"]["sha256"],
             "staging build has a different parent/schema")
    source, inventory, digest = _verify_source(build, receipt.get("source_provenance"))
    patch_sha256 = receipt["source_provenance"]["exact_patch"]["sha256"]
    reviewed_patch = reviewed_replus_patch(patch_sha256)
    pins = copy.deepcopy(original.raw["source_pins"])
    _require(pins.get("replus_commit") == REPLUS_COMMIT, "parent RE+ commit differs from locked base")
    pins["replus_patch_sha256"] = patch_sha256
    pins["replus_source_inventory_sha256"] = digest
    _require(manifest.raw.get("source_pins") == pins, "source pins changed outside RE+ patch/snapshot identity")
    artifact = _verify_build(build, receipt.get("build_evidence"), source, inventory,
                             reviewed_patch=reviewed_patch)
    installed = manifest.component_path(_COMPONENT)
    _require(installed is not None and installed.is_file() and installed.stat().st_size == artifact["bytes"]
             and sha256_file(installed) == artifact["sha256"]
             and updated[_COMPONENT].get("sha256") == artifact["sha256"], "installed RE+ differs from actual built ASI")
    result = copy.deepcopy(effective)
    result["artifacts"][_COMPONENT] = artifact
    return result

"""Seal a controller build: v2 from an original v1, or v4 from a strict RE+-v3.

This does not build, deploy, rewrite a manifest, or operate GTA. It reopens the
existing build's frozen files and logs, checks the current Git base/patch against
those exact compiler inputs, and preserves the old stack's provenance separately.
"""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import subprocess
import sys
import tempfile

MODULE_ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(MODULE_ROOT))

from gta_capture.jsonio import file_record, sha256_file, sha256_json, utc_now
from gta_capture.controller_replus_receipt import (
    CONTROLLER_REPLUS_DERIVED_SCHEMA, CONTROLLER_REPLUS_KIND,
    CONTROLLER_REPLUS_SCOPE, validate_preserved_replus_parent)
from gta_capture.errors import ContractError
from gta_capture.preflight import (DependencyManifest, _check_capture_build_receipt,
                                   _check_capture_build_receipt_v1)
from gta_capture.replus_receipt import REPLUS_DERIVED_SCHEMA


def checked_record(record: dict, *, allow_empty: bool = False) -> Path:
    path = Path(record["path"])
    if not path.is_absolute() or not path.is_file():
        raise ValueError(f"evidence file is absent or not absolute: {path}")
    actual = file_record(path)
    if actual["sha256"] != record["sha256"] or actual["bytes"] != record["bytes"]:
        raise ValueError(f"evidence file changed: {path}")
    if not allow_empty and actual["bytes"] == 0:
        raise ValueError(f"evidence file is empty: {path}")
    return path


def copied_record(source: Path, target: Path) -> dict:
    before = file_record(source)
    target.parent.mkdir(parents=True, exist_ok=True)
    if target.exists():
        raise ValueError(f"refusing to overwrite evidence: {target}")
    shutil.copyfile(source, target)
    copied = file_record(target)
    if any(copied[key] != before[key] for key in ("bytes", "sha256")):
        raise ValueError("source changed during evidence copy")
    checked_record(before)
    return copied


def git(repository: Path, *argv: str, input_bytes: bytes | None = None) -> bytes:
    result = subprocess.run(["git", "-C", str(repository), *argv],
                            input=input_bytes, capture_output=True)
    if result.returncode:
        raise ValueError("Git provenance operation failed: " +
                         result.stderr.decode("utf-8", errors="replace"))
    return result.stdout


def relative_source(name: str) -> Path:
    value = PurePosixPath(name)
    if (value.is_absolute() or ".." in value.parts or "\\" in name or ":" in name
            or not value.parts or value.parts[0] not in {"native", "tests"}):
        raise ValueError(f"unsafe source inventory path: {name}")
    return Path(*value.parts)


def normalized(path: Path, contents: bytes) -> bytes:
    if path.suffix in {".cpp", ".hpp", ".json"} or path.name == "CMakeLists.txt":
        return contents.replace(b"\r\n", b"\n")
    return contents


def cache_value(cache: str, key: str) -> str:
    matches = re.findall(r"^" + re.escape(key) + r":[^=]+=(.*)$", cache, re.MULTILINE)
    if len(matches) != 1:
        raise ValueError(f"CMake cache does not uniquely identify {key}")
    return matches[0].strip()


def compiler_value(config: str, key: str) -> str:
    matches = re.findall(r"^set\(" + re.escape(key) + r' "([^"]+)"\)', config, re.MULTILINE)
    if len(matches) != 1:
        raise ValueError(f"compiler configuration does not uniquely identify {key}")
    return matches[0]


def write_receipt(args) -> Path:
    repository = args.repository.resolve()
    candidate_path = args.controller_build.resolve()
    candidate_record = file_record(candidate_path)
    build = json.loads(candidate_path.read_text(encoding="utf-8"))
    if (build.get("schema") != "dm-gta-controller-staging-build/1"
            or build.get("asi_built") is not True
            or build.get("portable_contract_passed") is not True
            or build.get("deployed") is not False):
        raise ValueError("an actual successful, undeployed controller staging build is required")
    checked_record(build["artifact"])
    checked_record(build["sdk_archive"])
    invocations = build["invocations"]
    if len(invocations) != 3 or any(item.get("returncode") != 0 for item in invocations):
        raise ValueError("successful configure/build/test invocations are required")
    for item in invocations:
        log = Path(item["log"])
        if not log.is_file() or sha256_file(log) != item["log_sha256"]:
            raise ValueError("actual build log is missing or changed")
    source = Path(build["source_snapshot"])
    inventory = build["source_files"]
    paths = {entry["path"] for entry in inventory}
    if len(paths) != len(inventory):
        raise ValueError("source inventory contains duplicates")
    if {item.relative_to(source).as_posix() for item in source.rglob("*") if item.is_file()} != paths:
        raise ValueError("frozen source inventory is incomplete")
    for entry in inventory:
        actual = file_record(source / relative_source(entry["path"]))
        if any(actual[key] != entry[key] for key in ("bytes", "sha256")):
            raise ValueError("frozen source input changed")

    parent_manifest_path = args.parent_manifest.resolve()
    parent_manifest = json.loads(parent_manifest_path.read_text(encoding="utf-8"))
    parent_component = parent_manifest["components"]["capture_build_receipt"]
    parent_receipt_path = Path(parent_component["path"])
    if sha256_file(parent_receipt_path) != parent_component["sha256"]:
        raise ValueError("parent manifest does not bind the old build receipt")
    parent_receipt = json.loads(parent_receipt_path.read_text(encoding="utf-8"))
    parent_schema = parent_receipt.get("schema")
    if parent_schema not in {"dm-gta-capture-build-receipt/1", REPLUS_DERIVED_SCHEMA}:
        raise ValueError("parent must be the original full-stack v1 or a strict RE+-v3 receipt")
    old_controller = parent_manifest["components"]["diogenes_controller_asi"]
    backup = getattr(args, "parent_controller_backup", None)
    old_controller_path = (Path(backup).resolve() if backup is not None
                           else Path(old_controller["path"]))
    old_record = file_record(old_controller_path)
    if parent_schema == REPLUS_DERIVED_SCHEMA:
        effective = validate_preserved_replus_parent(
            DependencyManifest.load(parent_manifest_path), parent_receipt_path, old_record,
            validate_parent=_check_capture_build_receipt_v1)
        old_artifact = effective["artifacts"]["diogenes_controller_asi"]
        checked_record(build["parent_capture_build_receipt"])
        parent_record = file_record(parent_receipt_path)
        if any(build["parent_capture_build_receipt"][key] != parent_record[key]
               for key in ("bytes", "sha256")):
            raise ValueError("controller build must bind this exact immutable v3 parent")
    else:
        old_artifact = parent_receipt["artifacts"]["diogenes_controller_asi"]
    if (old_record["sha256"] != old_controller["sha256"]
            or any(old_record[key] != old_artifact[key] for key in ("bytes", "sha256"))):
        label = "explicit parent controller backup" if backup is not None else "old installed controller"
        raise ValueError(f"{label} no longer matches the pinned parent")

    output_dir = args.out_dir.resolve()
    output_dir.mkdir(parents=True, exist_ok=False)
    parent = {
        "manifest": copied_record(parent_manifest_path, output_dir / "parent/manifest.json"),
        "receipt": copied_record(parent_receipt_path, output_dir / "parent/build-receipt.json"),
        "controller_backup": copied_record(old_controller_path, output_dir / "parent/DiogenesGtaController.asi"),
    }
    if any(parent["controller_backup"][key] != old_record[key] for key in ("bytes", "sha256")):
        raise ValueError("parent controller changed after validation; copied backup is not the pinned parent")
    base = build["git_base_commit"]
    if re.fullmatch(r"[0-9a-f]{40}", base) is None:
        raise ValueError("candidate Git base is not a full commit hash")
    git(repository, "cat-file", "-e", base + "^{commit}")
    module_relative = MODULE_ROOT.relative_to(repository).as_posix()
    selected = [module_relative + "/" + item["path"] for item in inventory]
    tracked_names = set(git(repository, "ls-files", "-z", "--", *selected).decode().split("\0")) - {""}
    untracked_names = set(git(repository, "ls-files", "--others", "--exclude-standard", "-z", "--", *selected).decode().split("\0")) - {""}
    if tracked_names | untracked_names != set(selected):
        raise ValueError("source inventory has ignored or unclassified compiler inputs")
    patch = git(repository, "diff", "--no-ext-diff", "--no-textconv", "--binary",
                "--full-index", base, "--", *sorted(tracked_names))
    patch_path = output_dir / "source-provenance/tracked.patch"
    patch_path.parent.mkdir()
    patch_path.write_bytes(patch)
    untracked = [item for item in inventory
                 if module_relative + "/" + item["path"] in untracked_names]
    # Reconstruct the base plus real tracked diff and compare it with the exact
    # already-compiled snapshot. Existing CRLF compiler inputs remain untouched.
    with tempfile.TemporaryDirectory(prefix="verify-source-", dir=output_dir) as temp:
        reconstructed = Path(temp)
        for name in sorted(tracked_names):
            target = reconstructed / name
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(git(repository, "show", base + ":" + name))
        if patch:
            # Do not let a temporary tree inside out/ discover the live repo's
            # index/worktree: apply this data-only diff to the isolated base.
            environment = dict(os.environ, GIT_CEILING_DIRECTORIES=str(output_dir))
            applied = subprocess.run(
                ["git", "apply", "--binary", "--whitespace=nowarn", "-"],
                cwd=reconstructed, env=environment, input=patch, capture_output=True)
            if applied.returncode:
                raise ValueError("tracked patch does not apply to the isolated base: " +
                                 applied.stderr.decode("utf-8", errors="replace"))
        for item in inventory:
            name = module_relative + "/" + item["path"]
            expected = (reconstructed / name if name in tracked_names else repository / name)
            path = relative_source(item["path"])
            if normalized(path, expected.read_bytes()) != normalized(path, (source / path).read_bytes()):
                raise ValueError("Git base/patch/untracked evidence does not reconstruct compiled source: " + name)

    configure = invocations[0]["argv"]
    build_dir = Path(configure[configure.index("-B") + 1])
    cache_path = build_dir / "CMakeCache.txt"
    cache = cache_path.read_text(encoding="utf-8")
    compiler_configs = list((build_dir / "CMakeFiles").glob("*/CMakeCXXCompiler.cmake"))
    if len(compiler_configs) != 1:
        raise ValueError("build must identify exactly one CMake compiler configuration")
    compiler_config = compiler_configs[0]
    compiler_text = compiler_config.read_text(encoding="utf-8")
    configure_log_path = build_dir / "CMakeFiles/CMakeConfigureLog.yaml"
    configure_log = configure_log_path.read_text(encoding="utf-8")
    actual_msbuild = set(re.findall(
        r'Run Build Command\(s\): "([^"\r\n]+[\\/]MSBuild\.exe)"',
        configure_log, re.IGNORECASE))
    if len(actual_msbuild) != 1:
        raise ValueError("CMake configure log must identify one actual MSBuild executable")
    tools = {
        "cmake": file_record(invocations[0]["executable"]),
        "ctest": file_record(invocations[2]["executable"]),
        "msbuild": file_record(next(iter(actual_msbuild))),
        "compiler": file_record(compiler_value(compiler_text, "CMAKE_CXX_COMPILER")),
        "linker": file_record(cache_value(cache, "CMAKE_LINKER")),
    }
    is_v4 = parent_schema == REPLUS_DERIVED_SCHEMA
    receipt = {
        "schema": CONTROLLER_REPLUS_DERIVED_SCHEMA if is_v4 else "dm-gta-capture-build-receipt/2",
        "kind": CONTROLLER_REPLUS_KIND if is_v4 else "controller_only_derived",
        "sealed_at": utc_now(), "parent": parent,
        "controller_build": candidate_record,
        "source_provenance": {
            "repository": str(repository), "base_commit": base,
            "tracked_patch": file_record(patch_path), "untracked_files": untracked,
            "inventory_sha256": sha256_json(inventory), "text_normalization": "crlf-to-lf",
        },
        "build_evidence": {
            "cache": file_record(cache_path), "tools": tools,
            "configure_log": file_record(configure_log_path),
            "compiler_config": file_record(compiler_config),
            "project": file_record(build_dir / "DiogenesGtaController.vcxproj"),
            "builder": file_record(args.build_helper.resolve()),
        },
    }
    if is_v4:
        receipt["scope"] = dict(CONTROLLER_REPLUS_SCOPE)
    checked_record(candidate_record)
    for entry in inventory:
        if sha256_file(source / entry["path"]) != entry["sha256"]:
            raise ValueError("compiled source changed while sealing")
    output = output_dir / ("capture-build-receipt-v4.json" if is_v4 else "capture-build-receipt-v2.json")
    with output.open("x", encoding="utf-8", newline="\n") as handle:
        json.dump(receipt, handle, ensure_ascii=False, indent=2)
        handle.write("\n")
    if is_v4:
        # Preserve the exact parent's non-controller values. This is a source
        # review manifest, never a claim that the new component was installed.
        candidate = json.loads(json.dumps(parent_manifest))
        candidate["components"]["diogenes_controller_asi"] = {
            **candidate["components"]["diogenes_controller_asi"], **build["artifact"]}
        candidate["components"]["capture_build_receipt"] = {
            **candidate["components"]["capture_build_receipt"], **file_record(output)}
        candidate["source_pins"]["controller_git_commit"] = base
        candidate["source_pins"]["controller_source_inventory_sha256"] = sha256_json(inventory)
        manifest_output = output_dir / "manifest-source-review.json"
        with manifest_output.open("x", encoding="utf-8", newline="\n") as handle:
            json.dump(candidate, handle, ensure_ascii=False, indent=2)
            handle.write("\n")
        verification = _check_capture_build_receipt(DependencyManifest.load(manifest_output))
        with (output_dir / "source-verification.json").open("x", encoding="utf-8", newline="\n") as handle:
            json.dump({"status": verification.status, "summary": verification.summary,
                       "evidence": verification.evidence, "manifest": file_record(manifest_output),
                       "deployed": False}, handle, ensure_ascii=False, indent=2)
            handle.write("\n")
        if verification.status != "PASS":
            raise ContractError("new v4 source review failed: " + str(verification.evidence))
    return output


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--controller-build", required=True, type=Path)
    parser.add_argument("--parent-manifest", required=True, type=Path)
    parser.add_argument(
        "--parent-controller-backup", type=Path,
        help="explicit original parent ASI backup; bytes/SHA must still match the pinned parent")
    parser.add_argument("--build-helper", required=True, type=Path,
                        help="the exact script that performed the recorded build")
    parser.add_argument("--out-dir", required=True, type=Path, help="must not already exist")
    parser.add_argument("--repository", type=Path, default=MODULE_ROOT.parents[1])
    args = parser.parse_args()
    try:
        output = write_receipt(args)
    except (OSError, ValueError, KeyError, json.JSONDecodeError, ContractError) as exc:
        print(json.dumps({"ok": False, "error": str(exc)}, ensure_ascii=False))
        return 1
    manifest_output = output.parent / "manifest-source-review.json"
    print(json.dumps({"ok": True, "receipt": file_record(output), "deployed": False,
                      "manifest_written": manifest_output.exists(),
                      "source_review_manifest": file_record(manifest_output) if manifest_output.exists() else None},
                     ensure_ascii=False))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

"""Seal one real RE+-only candidate; does not build, deploy, or approve runtime."""
from __future__ import annotations

import argparse
import copy
import json
from pathlib import Path
import re
import shutil
import sys

MODULE_ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(MODULE_ROOT))

from gta_capture.jsonio import file_record, load_json, sha256_file, sha256_json, utc_now, write_json_atomic
from gta_capture.preflight import DependencyManifest, _check_capture_build_receipt
from gta_capture.replus_receipt import (REPLUS_BUILD_SCHEMA, REPLUS_DERIVED_SCHEMA, REPLUS_COMMIT,
    REPLUS_SCOPE, _absolute, _cache_values, _git, _record, _require, observed_rc_paths,
    reviewed_replus_patch)


def copied(source, destination):
    before = file_record(source)
    destination.parent.mkdir(parents=True, exist_ok=True)
    _require(not destination.exists(), "refusing to overwrite preserved evidence")
    shutil.copyfile(source, destination)
    actual = file_record(destination)
    _require(all(actual[k] == before[k] for k in ("bytes", "sha256")), "source changed during copy")
    _record(before, "copied source")
    return actual


def write_receipt(args):
    build_path = args.replus_build.resolve()
    build_record = file_record(build_path)
    build = load_json(build_path)
    _require(build.get("schema") == REPLUS_BUILD_SCHEMA and build.get("asi_built") is True
             and build.get("deployed") is False, "successful undeployed RE+ staging build required")
    artifact_path = _record(build.get("artifact"), "actual candidate ASI")
    parent = DependencyManifest.load(args.parent_manifest.resolve())
    parent_receipt_path = parent.component_path("capture_build_receipt")
    _require(parent_receipt_path is not None and parent_receipt_path.is_file()
             and sha256_file(parent_receipt_path) == parent.raw["components"]["capture_build_receipt"]["sha256"],
             "parent manifest does not bind receipt")
    _require(load_json(parent_receipt_path).get("schema") == "dm-gta-capture-build-receipt/2",
             "parent must be the existing controller-only v2")
    _require(build.get("parent_capture_build_receipt", {}).get("sha256") == sha256_file(parent_receipt_path),
             "candidate build has a different v2 parent")
    old = (args.parent_replus_backup.resolve() if args.parent_replus_backup
           else parent.component_path("replus_asi"))
    _require(old is not None and old.is_file()
             and sha256_file(old) == parent.raw["components"]["replus_asi"]["sha256"],
             "supplied/original RE+ backup differs from parent pin")
    source = _absolute(build.get("source_snapshot"), "source snapshot")
    supplied_repository = getattr(args, "source_repository", None)
    repository = (supplied_repository.resolve() if supplied_repository is not None
                  else _absolute(build.get("source_repository", str(source)), "source object repository"))
    _require(repository.is_dir(), "source object repository directory is missing")
    _require(build.get("git_base_commit") == REPLUS_COMMIT, "RE+ base differs from locked commit")
    _require(_git(repository, ["rev-parse", "--verify", REPLUS_COMMIT + "^{commit}"]).strip().decode()
             == REPLUS_COMMIT, "locked RE+ commit object unavailable")
    patch_path = _record(build.get("source_patch"), "actual full RE+ patch")
    patch_sha256 = sha256_file(patch_path)
    reviewed_patch = reviewed_replus_patch(patch_sha256)
    directory = artifact_path.parents[2]
    cache_path = directory / "CMakeCache.txt"
    cache = _cache_values(cache_path)
    config_log = directory / "CMakeFiles/CMakeConfigureLog.yaml"
    msbuilds = re.findall(r'Run Build Command\(s\):\s*"([^"\n]*[Mm][Ss][Bb]uild\.exe)"',
                         config_log.read_text(encoding="utf-8"))
    _require(msbuilds and len({Path(p).resolve() for p in msbuilds}) == 1,
             "actual configure log does not uniquely bind MSBuild")
    compiler_configs = {}
    compiler_path = None
    for lang in ("C", "CXX"):
        paths = list((directory / "CMakeFiles").glob(f"*/CMake{lang}Compiler.cmake"))
        _require(len(paths) == 1, "actual compiler configuration is not unique")
        compiler_configs[lang] = file_record(paths[0])
        values = dict(re.findall(r'set\((\w+) "([^"\n]*)"\)', paths[0].read_text(encoding="utf-8")))
        candidate = _absolute(values.get(f"CMAKE_{lang}_COMPILER"), lang + " compiler")
        _require(compiler_path is None or candidate == compiler_path, "C and C++ compiler differ")
        compiler_path = candidate
    build_log = _absolute(build["invocations"][1]["log"], "actual diagnostic build log")
    rc_paths = observed_rc_paths(build_log.read_text(encoding="utf-8", errors="replace"))
    _require(len(rc_paths) == 1, "actual build diagnostic does not uniquely identify invoked rc.exe")
    tools = {"cmake": file_record(cache["CMAKE_COMMAND"]), "msbuild": file_record(msbuilds[0]),
             "compiler": file_record(compiler_path), "linker": file_record(cache["CMAKE_LINKER"]),
             "rc": file_record(next(iter(rc_paths)))}
    out = args.out_dir.resolve()
    out.mkdir(parents=True, exist_ok=False)
    receipt = {"schema": REPLUS_DERIVED_SCHEMA, "kind": "replus_only_derived", "sealed_at": utc_now(),
        "parent": {"manifest": copied(parent.path, out / "parent/manifest.json"),
                   "receipt": copied(parent_receipt_path, out / "parent/build-receipt-v2.json"),
                   "replus_backup": copied(old, out / "parent/RockstarEditorPlus.asi")},
        "replus_build": build_record,
        "source_provenance": {"repository": str(repository), "base_commit": build["git_base_commit"],
            "exact_patch": copied(patch_path, out / "source-provenance" / reviewed_patch.filename),
            "inventory_sha256": sha256_json(build["source_files"]), "text_normalization": "utf8-text-crlf-to-lf"},
        "build_evidence": {"builder": build["builder"], "tools": tools,
            "cache": file_record(cache_path), "compiler_configs": compiler_configs,
            "configure_log": file_record(config_log),
            "project": file_record(directory / "RockstarEditorPlus.vcxproj"),
            "minhook_project": file_record(directory / "src/vendor/minhook/minhook.vcxproj")},
        "scope": copy.deepcopy(REPLUS_SCOPE)}
    receipt_path = out / "capture-build-receipt-v3.json"
    write_json_atomic(receipt_path, receipt)
    manifest = copy.deepcopy(parent.raw)
    manifest["components"]["replus_asi"] = {"path": str(artifact_path), "sha256": build["artifact"]["sha256"]}
    manifest["components"]["capture_build_receipt"] = {"path": str(receipt_path), "sha256": sha256_file(receipt_path)}
    manifest["source_pins"]["replus_patch_sha256"] = patch_sha256
    manifest["source_pins"]["replus_source_inventory_sha256"] = receipt["source_provenance"]["inventory_sha256"]
    candidate_manifest = out / "manifest-source-review.json"
    write_json_atomic(candidate_manifest, manifest)
    check = _check_capture_build_receipt(DependencyManifest.load(candidate_manifest))
    write_json_atomic(out / "source-verification.json", check.to_dict())
    _require(check.status == "PASS", "full v3 candidate verification failed: " + str(check.evidence))
    _record(build_record, "staging receipt after sealing")
    return {"receipt": file_record(receipt_path), "candidate_manifest": file_record(candidate_manifest),
            "source_verification": check.to_dict(), "deployed": False}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--replus-build", type=Path, required=True)
    parser.add_argument("--parent-manifest", type=Path, required=True)
    parser.add_argument("--parent-replus-backup", type=Path)
    parser.add_argument("--source-repository", type=Path,
                        help="Git object repository containing the locked RE+ commit; compiled snapshot remains unchanged")
    parser.add_argument("--out-dir", type=Path, required=True)
    args = parser.parse_args()
    print(json.dumps(write_receipt(args), ensure_ascii=False, indent=2))


if __name__ == "__main__":
    main()

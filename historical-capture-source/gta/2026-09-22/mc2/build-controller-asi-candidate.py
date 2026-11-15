"""Compile the frozen controller candidate with the previously pinned SDK only."""
import argparse
from datetime import datetime, timezone
import hashlib
import json
from pathlib import Path, PurePosixPath
import shutil
import subprocess
import zipfile


ROOT = Path(__file__).resolve().parent
SOURCE_RECEIPT = ROOT / "controller-third-person-staging-v2/staging-receipt.json"
EXPECTED_SDK_SHA = "56b6ad265b2b93aa4e73f79f97949d72f3683a35576b977e63dde91c6485fe22"


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--sdk-zip", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    archive = args.sdk_zip.resolve()
    if digest(archive) != EXPECTED_SDK_SHA:
        raise SystemExit("SDK hash differs from the pre-existing pin; no extraction or build")
    stage = args.output.resolve()
    stage.mkdir()
    started = datetime.now(timezone.utc).isoformat()
    source_receipt = json.loads(SOURCE_RECEIPT.read_text(encoding="utf-8"))
    original_source = Path(source_receipt["source_snapshot"])
    source = stage / "source"
    for record in source_receipt["source_files"]:
        original = original_source / record["path"]
        if digest(original) != record["sha256"]:
            raise RuntimeError("portable-tested source snapshot changed")
        target = source / record["path"]
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(original, target)
    sdk = stage / "sdk"
    sdk.mkdir()
    with zipfile.ZipFile(archive) as handle:
        # Extract the pinned SDK into staging; no bundled sample is executed.
        for member in handle.infolist():
            name = PurePosixPath(member.filename.replace("\\", "/"))
            if name.is_absolute() or ".." in name.parts or ":" in member.filename:
                raise RuntimeError("SDK archive has an unsafe path")
            if member.is_dir():
                continue
            target = sdk.joinpath(*name.parts)
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(handle.read(member))
    roots = [item.parent.parent for item in sdk.rglob("natives.h")
             if item.parent.name == "inc" and (item.parent.parent / "lib/ScriptHookV.lib").is_file()]
    if len(roots) != 1:
        raise RuntimeError("SDK must provide exactly one inc/natives.h + lib/ScriptHookV.lib root")
    sdk_root = roots[0]
    cmake = source_receipt["invocations"][0]["executable"]
    build = stage / "build"
    commands = [
        [cmake, "-S", str(source / "native"), "-B", str(build), "-A", "x64",
         "-DDIOGENES_BUILD_SCRIPT_HOOK_V_ASI=ON", "-DSCRIPT_HOOK_V_SDK_ROOT=" + str(sdk_root)],
        [cmake, "--build", str(build), "--config", "Release", "--parallel", "2"],
        [str(Path(cmake).with_name("ctest.exe")), "--test-dir", str(build), "-C", "Release",
         "--output-on-failure"],
    ]
    invocations = []
    for index, command in enumerate(commands):
        result = subprocess.run(command, capture_output=True, text=True, errors="replace")
        log = stage / f"step-{index + 1}.log"
        log.write_text(result.stdout + result.stderr, encoding="utf-8")
        invocations.append({"executable": command[0], "argv": command[1:],
                            "returncode": result.returncode,
                            "log": str(log), "log_sha256": digest(log)})
        print(f"Step {index + 1}: exit {result.returncode}; log={log.name}")
        if result.returncode:
            break
    artifact = build / "Release/DiogenesGtaController.asi"
    succeeded = len(invocations) == 3 and all(item["returncode"] == 0 for item in invocations)
    asi_built = len(invocations) >= 2 and invocations[1]["returncode"] == 0 and artifact.is_file()
    portable_passed = len(invocations) == 3 and invocations[2]["returncode"] == 0
    for record in source_receipt["source_files"]:
        if digest(source / record["path"]) != record["sha256"]:
            raise RuntimeError("source snapshot changed during build")
    payload = {
        "schema": "dm-gta-controller-staging-build/1", "started_at": started,
        "finished_at": datetime.now(timezone.utc).isoformat(),
        "parent_source_candidate": {"path": str(SOURCE_RECEIPT), "sha256": digest(SOURCE_RECEIPT)},
        "parent_capture_build_receipt": source_receipt["parent_build_receipt"],
        "git_base_commit": source_receipt["git_base_commit"], "working_tree_candidate": True,
        "source_snapshot": str(source), "source_files": source_receipt["source_files"],
        "sdk_archive": {"path": str(archive), "sha256": digest(archive), "bytes": archive.stat().st_size},
        "sdk_compiler_inputs": [{"path": str(item.relative_to(sdk_root)), "sha256": digest(item),
                                 "bytes": item.stat().st_size}
                                for subdir in ("inc", "lib")
                                for item in sorted((sdk_root / subdir).rglob("*")) if item.is_file()],
        "invocations": invocations, "asi_built": asi_built,
        "artifact": {"path": str(artifact), "bytes": artifact.stat().st_size,
                     "sha256": digest(artifact)} if asi_built else None,
        "portable_contract_passed": portable_passed,
        "full_native_ctest_passed": portable_passed, "deployed": False,
        "runtime_tested": False, "capture_build_receipt_eligible": False,
        "limitations": ["Controller-only build from a dirty working-tree source snapshot.",
                        "RE+ and IGCS were not rebuilt; their original sealed receipt remains unchanged.",
                        "Not accepted by existing real preflight; no manifest or acceptance rule changed.",
                        "A successful camera-mode test does not establish player pixel visibility."]}
    output = stage / "controller-staging-build-receipt.json"
    output.write_text(json.dumps(payload, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    print(json.dumps({"asi_built": succeeded, "deployed": False, "receipt": str(output)}))
    if not succeeded:
        raise SystemExit(1)


if __name__ == "__main__":
    main()

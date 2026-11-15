"""Freeze and compile the current controller with the previously locked SDK.

Produces a new undeployed controller staging receipt after real configure,
full build and eight native tests. Its immutable parent is the current v3 stack;
sealing that controller-on-v3 relationship is a separate strictly verified step.
Never modifies installed binaries, old candidates, or a run's evidence.
"""
from datetime import datetime, timezone
from pathlib import Path, PurePosixPath
import argparse
import hashlib
import json
import shutil
import subprocess
import zipfile

BASE = Path(__file__).resolve().parent
MODULE = BASE.parents[1]
OLD = BASE.parent / "mc2-20260908"
EXPECTED_SDK = "56b6ad265b2b93aa4e73f79f97949d72f3683a35576b977e63dde91c6485fe22"


def sha(path):
    value = hashlib.sha256()
    with Path(path).open("rb") as stream:
        for chunk in iter(lambda: stream.read(1048576), b""):
            value.update(chunk)
    return value.hexdigest()


def record(path):
    path = Path(path).resolve()
    return {"path": str(path), "bytes": path.stat().st_size, "sha256": sha(path)}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    stage = args.out.resolve()
    if not stage.is_relative_to(BASE) or stage == BASE:
        raise ValueError("candidate output must be a new child of this task's ignored output directory")
    old = json.loads((OLD / "controller-third-person-asi-build-full/controller-staging-build-receipt.json").read_text(encoding="utf-8"))
    prior = json.loads((OLD / "controller-derived-v2-final/capture-build-receipt-v2.json").read_text(encoding="utf-8"))
    archive = Path(old["sdk_archive"]["path"])
    if sha(archive) != EXPECTED_SDK or old["sdk_archive"]["sha256"] != EXPECTED_SDK:
        raise ValueError("SDK archive differs from the existing lock")
    cmake = Path(old["invocations"][0]["executable"])
    if sha(cmake) != prior["build_evidence"]["tools"]["cmake"]["sha256"]:
        raise ValueError("CMake tool differs from the prior checked tool")
    parent_manifest = BASE / "replus-draw-guard-derived-01/manifest-installed.json"
    parent_manifest_value = json.loads(parent_manifest.read_text(encoding="utf-8"))
    parent_component = parent_manifest_value["components"]["capture_build_receipt"]
    parent_receipt = record(parent_component["path"])
    if parent_receipt["sha256"] != "0994e794b335049925aad61e97132372f461f08b637787725f84b57cef4a9056" or parent_component["sha256"] != parent_receipt["sha256"]:
        raise ValueError("current immutable v3 parent receipt changed")
    parent_manifest_record = record(parent_manifest)
    preserved = BASE / "controller-editor-wait-60min-preserve-01/DiogenesGtaController-before.asi"
    if sha(preserved) != "7335a705929bf0c397dad2e57bab7d4a3743c0c39b1cf1895393de7ba8d8be15":
        raise ValueError("independent original controller backup differs")
    stage.mkdir(parents=True, exist_ok=False)
    started = datetime.now(timezone.utc).isoformat()
    source = stage / "source"
    originals = [MODULE / "native/CMakeLists.txt"]
    originals += sorted((MODULE / "native/include").glob("*"))
    originals += sorted((MODULE / "native/src").glob("*"))
    originals += sorted((MODULE / "tests/fixtures").glob("golden-camera-tape.*"))
    inventory = []
    for original in originals:
        if not original.is_file():
            continue
        relative = original.relative_to(MODULE)
        before = record(original)
        target = source / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(original, target)
        if sha(target) != before["sha256"] or record(original) != before:
            raise ValueError("compiler source changed while freezing")
        inventory.append({"path": relative.as_posix(), "bytes": before["bytes"], "sha256": before["sha256"]})
    sdk = stage / "sdk"
    sdk.mkdir()
    with zipfile.ZipFile(archive) as handle:
        for member in handle.infolist():
            name = PurePosixPath(member.filename.replace("\\", "/"))
            if name.is_absolute() or ".." in name.parts or ":" in member.filename:
                raise ValueError("unsafe SDK archive member")
            if member.is_dir():
                continue
            target = sdk.joinpath(*name.parts)
            target.parent.mkdir(parents=True, exist_ok=True)
            with target.open("xb") as output:
                output.write(handle.read(member))
    roots = [p.parent.parent for p in sdk.rglob("natives.h")
             if p.parent.name == "inc" and (p.parent.parent / "lib/ScriptHookV.lib").is_file()]
    if len(roots) != 1:
        raise ValueError("SDK must contain exactly one compiler input root")
    sdk_root = roots[0]
    build = stage / "build"
    commands = [
        [str(cmake), "-S", str(source / "native"), "-B", str(build), "-A", "x64",
         "-DDIOGENES_BUILD_SCRIPT_HOOK_V_ASI=ON", "-DSCRIPT_HOOK_V_SDK_ROOT=" + str(sdk_root)],
        [str(cmake), "--build", str(build), "--config", "Release", "--parallel", "2"],
        [str(cmake.with_name("ctest.exe")), "--test-dir", str(build), "-C", "Release", "--output-on-failure"],
    ]
    invocations = []
    for index, command in enumerate(commands, 1):
        result = subprocess.run(command, capture_output=True, text=True, errors="replace")
        log = stage / f"step-{index}.log"
        log.write_text(result.stdout + result.stderr, encoding="utf-8")
        invocations.append({"executable": command[0], "argv": command[1:], "returncode": result.returncode,
                            "log": str(log), "log_sha256": sha(log)})
        print(f"Step {index}: exit={result.returncode}; {log}", flush=True)
        if result.returncode:
            break
    artifact = build / "Release/DiogenesGtaController.asi"
    success = len(invocations) == 3 and all(item["returncode"] == 0 for item in invocations)
    for item in inventory:
        if sha(source / item["path"]) != item["sha256"]:
            raise ValueError("frozen compiler input changed during build")
    receipt = {
        "schema": "dm-gta-controller-staging-build/1", "started_at": started,
        "finished_at": datetime.now(timezone.utc).isoformat(),
        "parent_capture_build_receipt": parent_receipt,
        "parent_dependency_manifest": parent_manifest_record,
        "parent_controller_backup": record(preserved),
        "builder": record(__file__),
        "git_base_commit": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=MODULE, text=True).strip(),
        "working_tree_candidate": True, "source_snapshot": str(source), "source_files": inventory,
        "sdk_archive": record(archive),
        "sdk_compiler_inputs": [{"path": p.relative_to(sdk_root).as_posix(), "bytes": p.stat().st_size, "sha256": sha(p)}
                                for sub in ("inc", "lib") for p in sorted((sdk_root / sub).rglob("*")) if p.is_file()],
        "invocations": invocations, "asi_built": success, "artifact": record(artifact) if artifact.is_file() else None,
        "portable_contract_passed": success, "full_native_ctest_passed": success,
        "deployed": False, "runtime_tested": False,
        "limitations": ["Controller-only ARM wall-clock budget change; RE+ ac7d8ce5 and all other installed components remain unchanged.",
                        "Compilation and portable contracts do not claim runtime capture, calibration, rights or delivery approval."]}
    output = stage / "controller-staging-build-receipt.json"
    with output.open("x", encoding="utf-8", newline="\n") as stream:
        json.dump(receipt, stream, ensure_ascii=False, indent=2)
        stream.write("\n")
    print(json.dumps({"ok": success, "receipt": str(output), "deployed": False}), flush=True)
    if not success:
        raise SystemExit(1)


if __name__ == "__main__":
    main()

"""Build only the portable camera contract from an immutable source snapshot."""
from datetime import datetime, timezone
import hashlib
import json
from pathlib import Path
import shutil
import subprocess


MODULE = Path(__file__).resolve().parents[2]
STAGE = Path(__file__).resolve().parent / "controller-third-person-staging-v2"
CMAKE = Path("C:/Users/researcher/AppData/Local/Microsoft/WinGet/Packages/Kitware.CMake_Microsoft.Winget.Source_8wekyb3d8bbwe/cmake-4.4.2-windows-x86_64/bin/cmake.exe")


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main():
    STAGE.mkdir()
    started = datetime.now(timezone.utc).isoformat()
    source_root = STAGE / "source"
    records = []
    native = MODULE / "native"
    files = [native / "CMakeLists.txt"]
    files += sorted((native / "include").glob("*"))
    files += sorted((native / "src").glob("*"))
    files += sorted((MODULE / "tests" / "fixtures").glob("golden-camera-tape.*"))
    for original in files:
        if not original.is_file():
            continue
        relative = original.relative_to(MODULE)
        target = source_root / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(original, target)
        digest = sha(original)
        if digest != sha(target):
            raise RuntimeError("source snapshot changed during copy")
        records.append({"path": relative.as_posix(), "bytes": target.stat().st_size,
                        "sha256": digest})
    parent_manifest = MODULE / "out/mc2-20260908/run-mc2-beach-001/config/dependency-manifest.json"
    parent = json.loads(parent_manifest.read_text(encoding="utf-8"))
    parent_receipt_record = parent["components"]["capture_build_receipt"]
    parent_receipt = Path(parent_receipt_record["path"])
    if sha(parent_receipt) != parent_receipt_record["sha256"]:
        raise RuntimeError("parent build receipt hash mismatch")
    shutil.copyfile(parent_receipt, STAGE / "parent-capture-build-receipt.json")
    build = STAGE / "portable-build"
    commands = [
        [str(CMAKE), "-S", str(source_root / "native"), "-B", str(build),
         "-A", "x64", "-DDIOGENES_BUILD_SCRIPT_HOOK_V_ASI=OFF"],
        [str(CMAKE), "--build", str(build), "--config", "Release", "--target",
         "gta_recording_camera_selftest", "--parallel", "2"],
        [str(CMAKE.with_name("ctest.exe")), "--test-dir", str(build), "-C", "Release",
         "--output-on-failure", "-R", "^gta_recording_camera_contract$"],
    ]
    invocations = []
    for index, command in enumerate(commands):
        result = subprocess.run(command, capture_output=True, text=True, errors="replace")
        log = STAGE / f"step-{index + 1}.log"
        log.write_text(result.stdout + result.stderr, encoding="utf-8")
        invocations.append({"executable": command[0], "argv": command[1:],
                            "returncode": result.returncode, "log": str(log),
                            "log_sha256": sha(log)})
        print(f"Step {index + 1}: exit {result.returncode}; log={log.name}")
        if result.returncode:
            break
    head = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=MODULE,
                                   text=True).strip()
    payload = {"schema": "dm-gta-controller-source-candidate/1",
               "started_at": started,
               "finished_at": datetime.now(timezone.utc).isoformat(),
               "git_base_commit": head, "working_tree_candidate": True,
               "source_snapshot": str(source_root), "source_files": records,
               "parent_build_receipt": {"path": str(STAGE / "parent-capture-build-receipt.json"),
                                        "sha256": sha(parent_receipt)},
               "invocations": invocations,
               "portable_contract_passed": len(invocations) == 3 and all(
                   item["returncode"] == 0 for item in invocations),
               "asi_built": False, "asi_artifact": None, "deployed": False,
               "capture_build_receipt_eligible": False,
               "limitations": [
                   "The existing SDK directories were empty; the official SDK link redirected to HTML.",
                   "No alternative SDK or runtime was downloaded or installed.",
                   "Only the portable camera contract was compiled and run; the ScriptHook adapter was not compiled or tested in GTA.",
                   "This source-candidate receipt is not an accepted capture build receipt and cannot refresh a real preflight manifest.",
                   "Installed binaries, sealed parent receipts and real run manifests were not modified.",
                   "Third-person mode observations do not prove rendered character visibility."]}
    for record in records:
        if sha(source_root / record["path"]) != record["sha256"]:
            raise RuntimeError("source snapshot changed during build")
    (STAGE / "staging-receipt.json").write_text(
        json.dumps(payload, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    if not payload["portable_contract_passed"]:
        raise SystemExit(1)
    print(json.dumps({"portable_contract_passed": True, "asi_built": False,
                      "deployed": False, "receipt": str(STAGE / "staging-receipt.json")}))


if __name__ == "__main__":
    main()

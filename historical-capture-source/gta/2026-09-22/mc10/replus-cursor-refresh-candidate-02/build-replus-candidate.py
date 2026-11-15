"""Build only the frozen RE+ candidate and record what actually ran. No deploy."""
from __future__ import annotations
import datetime as dt
import hashlib
import json
import os
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parent
SOURCE = ROOT / "source"
BUILD = ROOT / "build"
CMAKE = Path("C:/Users/researcher/AppData/Local/Microsoft/WinGet/Packages/Kitware.CMake_Microsoft.Winget.Source_8wekyb3d8bbwe/cmake-4.4.2-windows-x86_64/bin/cmake.exe")
RECEIPT = ROOT / "replus-staging-build-receipt.json"

def now():
    return dt.datetime.now(dt.timezone.utc).isoformat()

def record(path):
    path = Path(path).resolve()
    value = path.read_bytes()
    return {"path": str(path), "bytes": len(value), "sha256": hashlib.sha256(value).hexdigest()}

def inventory():
    result = []
    for path in sorted(SOURCE.rglob("*")):
        relative = path.relative_to(SOURCE)
        if ".git" in relative.parts:
            continue
        if path.is_symlink():
            raise ValueError("source snapshot may not contain symlinks")
        if path.is_file():
            value = record(path)
            value["path"] = relative.as_posix()
            result.append(value)
    return result

def main():
    if BUILD.exists() or RECEIPT.exists():
        raise ValueError("fresh build directory and receipt are required; preserve failed attempts")
    frozen = json.loads((ROOT / "source-inventory.json").read_text(encoding="utf-8"))
    if inventory() != frozen["source_files"]:
        raise ValueError("source differs from frozen inventory")
    # Parent-directory MSBuild imports would be an unbound build input.
    for parent in (ROOT, *ROOT.parents):
        for filename in ("Directory.Build.props", "Directory.Build.targets"):
            if (parent / filename).exists():
                raise ValueError(f"unexpected parent build import: {parent / filename}")
    env = dict(os.environ)
    removed = []
    for key in tuple(env):
        if key.upper() in {"CL", "_CL_", "LINK", "_LINK_", "INCLUDE", "LIB", "LIBPATH",
                           "CMAKE_TOOLCHAIN_FILE", "CMAKE_GENERATOR", "CMAKE_GENERATOR_PLATFORM",
                           "CMAKE_GENERATOR_TOOLSET", "CMAKE_GENERATOR_INSTANCE"}:
            removed.append(key)
            del env[key]
    receipt = {
        "schema": "dm-gta-replus-staging-build/1",
        "started_at": now(), "git_base_commit": frozen["git_base_commit"],
        "parent_capture_build_receipt": record(ROOT.parent.parent / "mc2-20260908" / "replus-story-idle-derived-01" / "parent" / "build-receipt-v2.json"),
        "working_tree_candidate": True, "source_snapshot": str(SOURCE),
        "source_files": frozen["source_files"],
        "text_normalization": frozen["text_normalization"],
        "source_inventory": record(ROOT / "source-inventory.json"),
        "source_base_archive": record(ROOT / "base-source.tar"),
        "source_patch": record(ROOT / "source.patch"),
        "builder": record(__file__), "invocations": [],
        "environment_overrides_removed": sorted(removed),
        "asi_built": False, "contract_assertions": 14,
        "contract_assertions_verified": False,
        "deployed": False, "runtime_tested": False,
        "limitations": ["Local unsigned build provenance, not approval.",
                        "Nine compile-time mode assertions and five pointer-thread/recursion assertions; no CTest suite exists in this upstream project.",
                        "Cursor suppression, thread compatibility and frame exchange completion still require real GTA export testing.",
                        "No claim that this candidate removes every Win32, Scaleform or third-party cursor layer."]}
    configure = ["-S", str(SOURCE), "-B", str(BUILD), "-G", "Visual Studio 17 2022", "-A", "x64", "-DCMAKE_POLICY_VERSION_MINIMUM=3.5"]
    build = ["--build", str(BUILD), "--config", "Release", "--parallel", "2", "--", "/v:diag"]
    try:
        for index, argv in enumerate((configure, build), 1):
            logfile = ROOT / f"step-{index}.log"
            with logfile.open("xb") as stream:
                completed = subprocess.run([str(CMAKE), *argv], cwd=ROOT, env=env,
                                           stdout=stream, stderr=subprocess.STDOUT, check=False)
            receipt["invocations"].append({"executable": str(CMAKE), "argv": argv,
                "returncode": completed.returncode, "log": str(logfile),
                "log_sha256": record(logfile)["sha256"]})
            print(json.dumps({"step": index, "returncode": completed.returncode, "log": str(logfile)}), flush=True)
            if completed.returncode:
                raise RuntimeError(f"actual build step {index} failed")
        if inventory() != frozen["source_files"]:
            raise ValueError("source changed during build")
        artifact = BUILD / "BIN" / "Release" / "RockstarEditorPlus.asi"
        receipt["artifact"] = record(artifact)
        receipt["asi_built"] = artifact.stat().st_size > 0
        receipt["contract_assertions_verified"] = receipt["asi_built"]
        receipt["additional_build_outputs"] = [record(path) for path in sorted((BUILD / "BIN" / "Release").iterdir()) if path.is_file() and path != artifact]
    except Exception as exc:
        receipt["error"] = f"{type(exc).__name__}: {exc}"
        raise
    finally:
        receipt["finished_at"] = now()
        with RECEIPT.open("x", encoding="utf-8", newline="\n") as stream:
            json.dump(receipt, stream, ensure_ascii=False, indent=2)
            stream.write("\n")
        print(json.dumps({"receipt": str(RECEIPT), "asi_built": receipt["asi_built"]}), flush=True)

if __name__ == "__main__":
    main()

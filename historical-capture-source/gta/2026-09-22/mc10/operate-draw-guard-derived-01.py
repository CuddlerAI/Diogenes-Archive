"""Explicit, separate seal/install steps for one reviewed local candidate. Never starts GTA."""
from __future__ import annotations
import argparse
import copy
import datetime as dt
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys

BASE = Path(__file__).resolve().parent
MODULE = BASE.parent.parent
sys.path.insert(0, str(MODULE))
from gta_capture.preflight import DependencyManifest, _check_capture_build_receipt
from gta_capture.replus_receipt import reviewed_replus_patch

CANDIDATE = BASE / "replus-draw-guard-candidate-01"
OUT = BASE / "replus-draw-guard-derived-01"
PRIOR = BASE / "replus-cursor-refresh-derived-02"
SOURCE_REPOSITORY = BASE.parent / "mc2-20260908/replus-story-mode-fix-03/source"
PATCH_SHA = "c77b19756ef40cf6bfd63d725dc48e74da79a6f1a529c73d20971516ec1ef53e"
BUILD_SHA = "22b757ca54321a211c6a3dbd3ee659e6a5366e9222209a7a924285fafd0866a7"
ASI_SHA = "ac7d8ce5a53df9439af57c28babc0445c685c5ee705976065fbb054c1ba3eb3f"
PRIOR_MANIFEST_SHA = "a09cb41bae29505bd9a3ff6d53bb138a134280cbaec9d71f84b459b1491864c8"
SEALED_MANIFEST_SHA = "483bd52f0a209f827a97b22175d148cd34a2a21118efe1a4fd538546ef935097"
SEALED_RECEIPT_SHA = "0994e794b335049925aad61e97132372f461f08b637787725f84b57cef4a9056"

def require(value, why):
    if not value:
        raise ValueError(why)

def record(path):
    path = Path(path).resolve()
    data = path.read_bytes()
    return {"path": str(path), "bytes": len(data), "sha256": hashlib.sha256(data).hexdigest()}

def load(path):
    return json.loads(Path(path).read_text(encoding="utf-8"))

def dump(path, value):
    with Path(path).open("x", encoding="utf-8", newline="\n") as stream:
        json.dump(value, stream, ensure_ascii=False, indent=2)
        stream.write("\n")

def now():
    return dt.datetime.now(dt.timezone.utc).isoformat()

def pinned(row):
    actual = record(row["path"])
    require(actual["sha256"] == row["sha256"] and
            ("bytes" not in row or actual["bytes"] == row["bytes"]), "pinned bytes changed: " + row["path"])
    return Path(actual["path"])

def checked_manifest(path):
    check = _check_capture_build_receipt(DependencyManifest.load(path))
    require(check.status == "PASS", "strict capture build provenance rejected: " + str(check.evidence))
    return check.to_dict()

def check_candidate():
    require(record(CANDIDATE / "replus-staging-build-receipt.json")["sha256"] == BUILD_SHA, "candidate receipt differs")
    build = load(CANDIDATE / "replus-staging-build-receipt.json")
    require(build["asi_built"] and build["contract_assertions_verified"] and build["contract_assertions"] == 27,
            "actual successful build/27 compiled assertions are required")
    require(build["artifact"]["sha256"] == ASI_SHA and build["source_patch"]["sha256"] == PATCH_SHA,
            "candidate artifact/patch differs")
    for key in ("artifact", "source_patch", "source_inventory", "source_base_archive", "builder"):
        pinned(build[key])
    for step in build["invocations"]:
        require(step["returncode"] == 0, "actual build step failed")
        require(record(step["log"])["sha256"] == step["log_sha256"], "actual build log changed")
    require(len(build["source_files"]) == 79, "source closure differs")
    for row in build["source_files"]:
        pinned(dict(row, path=str(CANDIDATE / "source" / row["path"])))
    require((SOURCE_REPOSITORY / ".git").exists(), "the preserved source object repository is missing")
    reviewed = reviewed_replus_patch(PATCH_SHA)
    require(reviewed.contract_assertions == 27, "reviewed exact pin has wrong assertion contract")
    return build

def seal():
    check_candidate()
    require(not OUT.exists(), "derivation directory already exists; preserve failed/successful attempts")
    log_path = BASE / "replus-draw-guard-seal-01.log"
    require(not log_path.exists(), "seal log already exists; do not overwrite an attempt")
    argv = [sys.executable, "-X", "utf8", str(MODULE / "scripts/write-replus-build-receipt.py"),
        "--replus-build", str(CANDIDATE / "replus-staging-build-receipt.json"),
        "--parent-manifest", str(PRIOR / "parent/manifest.json"),
        "--parent-replus-backup", str(PRIOR / "parent/RockstarEditorPlus.asi"),
        "--source-repository", str(SOURCE_REPOSITORY), "--out-dir", str(OUT)]
    with log_path.open("xb") as log:
        result = subprocess.run(argv, cwd=MODULE, stdout=log, stderr=subprocess.STDOUT, check=False)
    dump(BASE / "replus-draw-guard-seal-01.json", {"started_by": record(Path(__file__)), "finished_at": now(),
        "argv": argv, "returncode": result.returncode, "log": record(log_path), "installed": False})
    require(result.returncode == 0, "strict writer failed; preserve output and inspect seal log")
    print(json.dumps({"source_manifest": str(OUT / "manifest-source-review.json"),
        "source_verification": checked_manifest(OUT / "manifest-source-review.json"), "installed": False}))

def copy_new(source, dest):
    source = Path(source)
    before = record(source)
    with source.open("rb") as inp, Path(dest).open("xb") as out:
        shutil.copyfileobj(inp, out)
    after = record(dest)
    require(after["bytes"] == before["bytes"] and after["sha256"] == before["sha256"], "copy differs")
    require(record(source) == before, "source changed during copy")
    return after

def install():
    # Root must first perform an ordinary game exit; this is read-only OS process
    # enumeration, not a native game protocol call, input action, or forced kill.
    state = subprocess.run(["powershell", "-NoProfile", "-NonInteractive", "-Command",
        "@(Get-Process -Name GTA5 -ErrorAction SilentlyContinue).Count"],
        capture_output=True, text=True, check=True)
    require(state.stdout.strip() == "0", "GTA5 is still running; exit normally before installation")
    build = check_candidate()
    source_path = OUT / "manifest-source-review.json"
    require(record(source_path)["sha256"] == SEALED_MANIFEST_SHA, "root-sealed source manifest differs")
    require(record(OUT / "capture-build-receipt-v3.json")["sha256"] == SEALED_RECEIPT_SHA,
            "root-sealed v3 receipt differs")
    verification = checked_manifest(source_path)
    prior_path = PRIOR / "manifest-installed.json"
    require(record(prior_path)["sha256"] == PRIOR_MANIFEST_SHA, "previous installed manifest changed")
    old = load(prior_path)
    new = load(source_path)
    # v3 derives from the original controller-v2/full-stack-v1 parent. This
    # additional comparison preserves the actually installed, newer RE+ as a
    # separate deployment backup; it is NOT substituted for the v1 parent ASI.
    for key in old.keys() | new.keys():
        if key not in ("components", "source_pins"):
            require(old.get(key) == new.get(key), "unrelated manifest field changed: " + key)
    for key in old["components"].keys() | new["components"].keys():
        if key not in ("replus_asi", "capture_build_receipt"):
            require(old["components"].get(key) == new["components"].get(key), "unrelated component changed: " + key)
            pinned(old["components"][key])
    for key in old["source_pins"].keys() | new["source_pins"].keys():
        if key not in ("replus_patch_sha256", "replus_source_inventory_sha256"):
            require(old["source_pins"].get(key) == new["source_pins"].get(key), "unrelated source pin changed: " + key)
    target = pinned(old["components"]["replus_asi"])
    game = Path(old["game"]["path"]).resolve().parent
    require(target.parent == game and target.name == "RockstarEditorPlus.asi", "unexpected installation target")
    prior_backup = OUT / "prior-installed"
    installed_path = OUT / "manifest-installed.json"
    deployment_path = OUT / "deployment-receipt.json"
    temp = target.with_name("RockstarEditorPlus.asi.dm-draw-guard-install-01.tmp")
    require(not prior_backup.exists() and not installed_path.exists() and not deployment_path.exists() and not os.path.lexists(temp),
            "installation attempt already exists; preserve all failed/successful outputs")
    log_sources = {"RockstarEditorPlus.log": game / "RockstarEditorPlus/RockstarEditorPlus.log",
                   "ScriptHookV.log": game / "ScriptHookV.log", "asiloader.log": game / "asiloader.log"}
    require(log_sources["RockstarEditorPlus.log"].is_file(), "previous real RE+ log must be preserved")
    prior_backup.mkdir(exist_ok=False)
    receipt = {"started_at": now(), "operator_script": record(Path(__file__)),
        "source_manifest": record(source_path), "source_verification": verification,
        "runtime_verified": False, "cursor_free_video_verified": False,
        "calibration_approved": False, "delivery_eligible": False, "installed": False}
    try:
        receipt["prior_component_backup"] = copy_new(target, prior_backup / "RockstarEditorPlus.asi")
        receipt["prior_manifest"] = copy_new(prior_path, prior_backup / "manifest-installed.json")
        receipt["prior_logs"] = {name: copy_new(path, prior_backup / name) if path.is_file() else {"not_present": True}
                                 for name, path in log_sources.items()}
        copy_new(pinned(build["artifact"]), temp)
        # Exact same validated destination; no recursive move/delete or computed
        # shell operation. The original component already has a verified backup.
        pinned(old["components"]["replus_asi"])
        os.replace(temp, target)
        installed = copy.deepcopy(new)
        installed["components"]["replus_asi"]["path"] = str(target)
        require(record(target)["sha256"] == ASI_SHA, "installed ASI differs")
        dump(installed_path, installed)
        receipt["installed_component"] = record(target)
        receipt["installed_manifest"] = record(installed_path)
        receipt["installed_source_verification"] = checked_manifest(installed_path)
        receipt["installed"] = True
    except Exception as exc:
        receipt["error"] = f"{type(exc).__name__}: {exc}"
        raise
    finally:
        receipt["finished_at"] = now()
        dump(deployment_path, receipt)
    print(json.dumps(receipt, ensure_ascii=False))

if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("phase", choices=("check", "seal", "install"))
    args = parser.parse_args()
    if args.phase == "check":
        check_candidate()
        print(json.dumps({"candidate_bytes_checked": True, "registered_exact_patch": PATCH_SHA,
                          "sealed_in_this_invocation": False, "installed_in_this_invocation": False}))
    elif args.phase == "seal":
        seal()
    else:
        install()

"""Seal reviewed source delta and check narrow source contracts; never builds or deploys."""
from pathlib import Path
import datetime as dt
import difflib
import hashlib
import importlib.util
import json
import os
import shutil
import subprocess
import tarfile
import tempfile

ROOT = Path(__file__).resolve().parent
PARENT = ROOT.parent / "replus-cursor-refresh-candidate-03"

def record(path):
    data = path.read_bytes()
    return {"path": str(path.resolve()), "bytes": len(data), "sha256": hashlib.sha256(data).hexdigest()}

def dump(path, value):
    with path.open("x", encoding="utf-8", newline="\n") as stream:
        json.dump(value, stream, ensure_ascii=False, indent=2)
        stream.write("\n")

def inventory(root):
    rows = []
    for p in sorted(root.rglob("*")):
        if p.is_symlink() or getattr(p, "is_junction", lambda: False)():
            raise ValueError("source link is not permitted")
        if p.is_file():
            rows.append(dict(record(p), path=p.relative_to(root).as_posix()))
    return rows

def main():
    if (ROOT / "build").exists() or (ROOT / "replus-staging-build-receipt.json").exists():
        raise ValueError("cannot reseal a built/attempted candidate")
    history = ROOT / "pre-review-01"
    history.mkdir(exist_ok=False)
    for name in ("source.patch", "draw-guard-only.patch", "source-inventory.json", "source-preparation-receipt.json"):
        shutil.copyfile(ROOT / name, history / name)
    before = (PARENT / "source/src/capture/render.cpp").read_text(encoding="utf-8")
    source = (ROOT / "source/src/capture/render.cpp").read_text(encoding="utf-8")
    callback = source.split("\t\tvoid __fastcall hkDrawMovie(", 1)[1].split("\n\t\tuint32_t s_lastBeat", 1)[0]
    drain = source.split("\t\tbool serviceMovieGuardDrain()", 1)[1].split("\n\t\tvoid endMovieGuard()", 1)[0]
    ack = source.split("\t\tcase Step::Ack:", 1)[1]
    checks = {
        "callback_has_no_plan_game_config_or_terminal_calls": not any(token in callback for token in (
            "Config::", "game::", "diogenesbridge::", "finish(", "cancel(", "logger::", "memory(")),
        "callback_uses_atomic_owned_state_and_step": "s_movieOwnedSuppression.load(std::memory_order_acquire)" in callback
            and "s_step.load(std::memory_order_acquire)" in callback,
        "owned_latch_false_store_has_one_guarded_release_site": source.count("s_movieOwnedSuppression.store(false") == 1
            and drain.index("movieGuardMayRelease(") < drain.index("s_movieOwnedSuppression.store(false"),
        "drain_services_before_idle_return": source.index("if (!serviceMovieGuardDrain()) return;")
            < source.index("if (s_step == Step::Idle) return;"),
        "first_real_request_is_after_owned_gate_and_guard": source.index("prepareMovieGuard(true, newlyArmed)")
            < source.index("fxcapture::requestSample(path, s_cfg.samples, s_sample,", source.index("prepareMovieGuard(true, newlyArmed)")),
        "last_ack_checks_integrity_before_advancing_or_success": ack.index("if (!fxcapture::lastDone())")
            < ack.index("movie draw guard failed after sample ACK") < ack.index("if (++s_sample"),
        "normal_and_failed_terminal_share_guard_end": "\t\t\tendMovieGuard();" in source,
        "no_extra_original_pointer_calls": source.count("origPointer();") == before.count("origPointer();"),
        "new_movie_contract_assertions_thirteen": source.count("static_assert(") - before.count("static_assert(") == 13,
    }
    if not all(checks.values()):
        raise ValueError("source contract failed: " + str(checks))
    rows = inventory(ROOT / "source")
    prior = json.loads((PARENT / "source-inventory.json").read_text(encoding="utf-8"))
    if inventory(PARENT / "source") != prior["source_files"]:
        raise ValueError("prior source changed")
    changed = [b["path"] for a, b in zip(prior["source_files"], rows) if a != b]
    if len(rows) != 79 or changed != ["src/capture/render.cpp"]:
        raise ValueError("unexpected source closure or changes")
    (ROOT / "draw-guard-only.patch").write_text("".join(difflib.unified_diff(before.splitlines(True), source.splitlines(True),
        fromfile="a/src/capture/render.cpp", tofile="b/src/capture/render.cpp")), encoding="utf-8", newline="\n")
    baseline = {}
    with tarfile.open(ROOT / "base-source.tar") as archive:
        for member in archive.getmembers():
            if member.isdir():
                continue
            if not member.isfile() or Path(member.name).is_absolute() or ".." in Path(member.name).parts:
                raise ValueError("invalid archive entry")
            baseline[member.name] = archive.extractfile(member).read()
    current = {row["path"]: (ROOT / "source" / row["path"]).read_bytes() for row in rows}
    patch = []
    for name in sorted(baseline.keys() | current.keys()):
        a = baseline.get(name, b"").replace(b"\r\n", b"\n")
        b = current.get(name, b"").replace(b"\r\n", b"\n")
        if a == b:
            continue
        patch.append(f"diff --git a/{name} b/{name}\n")
        if name not in baseline:
            patch.append("new file mode 100644\n")
        if name not in current:
            patch.append("deleted file mode 100644\n")
        patch.extend(difflib.unified_diff(a.decode().splitlines(True), b.decode().splitlines(True),
            fromfile=f"a/{name}" if name in baseline else "/dev/null",
            tofile=f"b/{name}" if name in current else "/dev/null"))
    (ROOT / "source.patch").write_text("".join(patch), encoding="utf-8", newline="\n")
    with tempfile.TemporaryDirectory(prefix="replus-draw-final-verify-") as directory:
        scratch = Path(directory)
        for name, data in baseline.items():
            p = scratch / name
            p.parent.mkdir(parents=True, exist_ok=True)
            p.write_bytes(data)
        environment = {k: v for k, v in os.environ.items() if not k.startswith("GIT_")}
        environment["GIT_CEILING_DIRECTORIES"] = str(scratch)
        result = subprocess.run(["git", "apply", "--binary", "--whitespace=nowarn", str(ROOT / "source.patch")],
            cwd=scratch, env=environment, capture_output=True, check=True)
        derived = {row["path"]: (scratch / row["path"]).read_bytes() for row in inventory(scratch)}
        if derived.keys() != current.keys() or any(derived[k].replace(b"\r\n", b"\n") != current[k].replace(b"\r\n", b"\n") for k in current):
            raise ValueError("base plus full patch differs from frozen source")
    frozen = dict(prior, source_snapshot=str((ROOT / "source").resolve()), source_files=rows)
    # These candidate records have not backed a build. Their preceding exact
    # bytes are now preserved above; this seals the reviewed ACK refinement.
    (ROOT / "source-inventory.json").write_text(json.dumps(frozen, ensure_ascii=False, indent=2) + "\n", encoding="utf-8", newline="\n")
    receipt = {"schema": "dm-gta-draw-guard-source-preparation/1", "created_at": dt.datetime.now(dt.timezone.utc).isoformat(),
        "finalizer": record(Path(__file__)), "previous_preparation": record(history / "source-preparation-receipt.json"),
        "parent_source_inventory": record(PARENT / "source-inventory.json"), "full_base_archive": record(ROOT / "base-source.tar"),
        "full_patch": record(ROOT / "source.patch"), "delta_patch": record(ROOT / "draw-guard-only.patch"),
        "inventory": record(ROOT / "source-inventory.json"), "builder": record(ROOT / "build-replus-candidate.py"),
        "source_file_count": 79, "changed_files_from_prior_sealed_source": changed,
        "base_plus_patch_reconstruction_verified": True, "source_contract_checks": checks,
        "contract_assertions": 27, "contract_assertions_compiled": False, "asi_built": False,
        "deployed": False, "runtime_tested": False, "source_pixels_modified": False}
    (ROOT / "source-preparation-receipt.json").write_text(json.dumps(receipt, ensure_ascii=False, indent=2) + "\n", encoding="utf-8", newline="\n")
    dump(ROOT / "source-contract-checks.json", {"schema": "dm-gta-draw-guard-source-checks/1",
        "source": record(ROOT / "source/src/capture/render.cpp"), "checks": checks, "passed": len(checks),
        "failed": 0, "limitations": "Source-contract checks only; actual MSVC and GTA checks remain pending."})
    print(json.dumps({"patch": receipt["full_patch"], "builder": receipt["builder"], "source_checks": len(checks), "build_pending": True}))

if __name__ == "__main__":
    main()

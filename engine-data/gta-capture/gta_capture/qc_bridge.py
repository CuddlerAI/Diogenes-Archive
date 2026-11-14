"""Bridge captured GTA runs into the existing engine_qc and engine_delivery tools."""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
from pathlib import Path

from .errors import ContractError, PolicyError
from .gate import REQUIRED_RULES
from .jsonio import (file_record, load_json, sha256_file, sha256_json,
                     write_json_atomic)
from .policy import repository_profile_non_trainable
from .preflight import DependencyManifest
from .tape import load_tape


ENGINE_DATA = Path(__file__).resolve().parents[2]
RUN_BINDING_SCHEMA = "dm-gta-run-binding/1"
QC_BRIDGE_SCHEMA = "dm-gta-qc-run-binding/1"
CRITICAL_INPUTS = {
    "dependency_manifest": "dependency-manifest.json",
    "scenario": "scenario.json",
    "camera_tape": "camera-tape.json",
    "camera_tape_native": "camera-tape.bin",
}
DELIVERY_FILES = tuple(
    f"{stem}_{lane}.{suffix}"
    for lane in range(1, 5)
    for stem, suffix in (
        ("video", "mp4"),
        ("systeminfo", "json"),
        ("action_camera", "json"),
    )
)

# A JSON list of passed rule names is not a delivery authorization. Keep GTA
# packaging unavailable until package_run can independently reopen every gate
# dependency and verify signed human/rights envelopes.
INDEPENDENT_PACKAGE_GATE_VERIFIER_IMPLEMENTED = False


def _normalized_path(path) -> str:
    return os.path.normcase(str(Path(path).resolve()))


def _require_record(record, path: Path, label: str) -> dict:
    """Re-hash one file and require its prior record to identify this exact path."""
    if not isinstance(record, dict):
        raise ContractError(f"{label} has no sealed file record")
    current = file_record(path)
    if _normalized_path(record.get("path", "")) != _normalized_path(path):
        raise ContractError(f"{label} path belongs to another run")
    if record.get("bytes") != current["bytes"] or record.get("sha256") != current["sha256"]:
        raise ContractError(f"{label} differs from its sealed bytes/sha256")
    return current


def _records_by_name(value) -> dict[str, dict]:
    if isinstance(value, dict):
        return {str(name): record for name, record in value.items()
                if isinstance(record, dict)}
    if isinstance(value, list):
        result = {}
        for record in value:
            if isinstance(record, dict) and isinstance(record.get("path"), str):
                result[Path(record["path"]).name] = record
        return result
    return {}


def _current_run_binding(root: Path, manifest: dict) -> dict:
    """Build the canonical, current binding after validating the run manifest seals."""
    run_id = manifest.get("run_id")
    if not isinstance(run_id, str) or not run_id.strip():
        raise ContractError("run manifest has no run_id")
    capture = (root / "capture").resolve()
    if _normalized_path(manifest.get("capture_dir", "")) != _normalized_path(capture):
        raise ContractError("run manifest capture_dir belongs to another run")

    inputs = {}
    manifest_inputs = manifest.get("inputs")
    if not isinstance(manifest_inputs, dict):
        raise ContractError("run manifest has no sealed inputs")
    for key, name in CRITICAL_INPUTS.items():
        path = root / "config" / name
        if not path.is_file():
            raise ContractError(f"critical run input is missing: {name}")
        inputs[key] = _require_record(manifest_inputs.get(key), path, f"input {key}")

    videos = _records_by_name(manifest.get("videos"))
    delivery_manifest = _records_by_name(manifest.get("delivery_files"))
    delivery = {}
    for name in DELIVERY_FILES:
        path = capture / name
        if not path.is_file():
            raise ContractError(f"current delivery set is missing {name}")
        if name.startswith("video_"):
            _require_record(videos.get(name), path, f"run manifest {name}")
        # New manifests seal all 12 files at finalization. Older manifests are
        # still bound at their first QC, rather than silently remaining unbound.
        if delivery_manifest:
            _require_record(delivery_manifest.get(name), path,
                            f"run manifest delivery file {name}")
        delivery[name] = file_record(path)
    if set(videos) != {f"video_{lane}.mp4" for lane in range(1, 5)}:
        raise ContractError("run manifest videos do not identify exactly the four current lanes")
    if delivery_manifest and set(delivery_manifest) != set(DELIVERY_FILES):
        raise ContractError("run manifest delivery_files is not the exact 12-file delivery set")

    qc_sidecars = {
        name: file_record(capture / name)
        for name in ("plan.json", "covis.json", "dyn_metrics.json")
        if (capture / name).is_file()
    }
    return {
        "schema": RUN_BINDING_SCHEMA,
        "run_id": run_id,
        "capture_dir": str(capture),
        "run_manifest": file_record(root / "run-manifest.json"),
        "inputs": inputs,
        "delivery_files": delivery,
        "qc_sidecars": qc_sidecars,
    }


def _gate_core(gate: dict) -> dict:
    return {key: value for key, value in gate.items() if key != "run_binding"}


def _bind_or_verify_gate(root: Path, manifest: dict, *, allow_bind: bool) -> tuple[dict, dict]:
    gate_path = root / "evidence" / "gta-pr405-gate.json"
    if not gate_path.is_file():
        raise ContractError("GTA PR #405 package gate evidence is missing")
    gate = load_json(gate_path)
    embedded = manifest.get("gta_pr405_gate")
    if not isinstance(gate, dict) or not isinstance(embedded, dict):
        raise ContractError("run manifest/GTA PR #405 gate evidence is invalid")

    current = _current_run_binding(root, manifest)
    # The verify path must re-check gate content, not only run_binding: a gate
    # file whose ok/passed_rules were edited after binding would otherwise pass.
    if _gate_core(gate) != _gate_core(embedded):
        raise ContractError("GTA PR #405 gate differs from its finalized run manifest copy")
    sealed = gate.get("run_binding")
    if sealed is None:
        if not allow_bind:
            raise ContractError("GTA PR #405 gate is not bound to a QC-verified run")
        gate["run_binding"] = current
        write_json_atomic(gate_path, gate)
        sealed = current
    if sealed != current:
        raise ContractError("GTA PR #405 gate run_binding differs from the current run/files")
    return gate, current


def _qc_bridge_record(root: Path, gate: dict, binding: dict, *, exit_code: int,
                      stderr: str) -> dict:
    gate_record = file_record(root / "evidence" / "gta-pr405-gate.json")
    return {
        "schema": QC_BRIDGE_SCHEMA,
        "exit_code": exit_code,
        "stderr": stderr[-4000:],
        "run_id": binding["run_id"],
        "capture_dir": binding["capture_dir"],
        "run_manifest": binding["run_manifest"],
        "gta_pr405_gate": gate_record,
        "binding_sha256": sha256_json(binding),
        "inputs": binding["inputs"],
        "delivery_files": binding["delivery_files"],
        "qc_sidecars": binding["qc_sidecars"],
        "gate_ok": gate.get("ok") is True,
    }


def _verify_qc_binding(root: Path, qc: dict, gate: dict, binding: dict) -> None:
    bridge = qc.get("gta_capture_bridge")
    if not isinstance(bridge, dict) or bridge.get("schema") != QC_BRIDGE_SCHEMA:
        raise ContractError("engine_qc report is not bound to a GTA run")
    expected = _qc_bridge_record(root, gate, binding, exit_code=0, stderr="")
    for key in ("run_id", "capture_dir", "run_manifest", "gta_pr405_gate",
                "binding_sha256", "inputs", "delivery_files", "qc_sidecars"):
        if bridge.get(key) != expected[key]:
            raise ContractError(f"engine_qc report {key} differs from the current run/files")
    if bridge.get("exit_code") != 0:
        raise ContractError("engine_qc report records a non-zero checker exit")


def stage_qc_sidecars(run_dir) -> list[str]:
    root = Path(run_dir)
    tape = load_tape(root / "config" / "camera-tape.json")
    capture = root / "capture"
    copied = []
    source = tape.get("source", {})
    # "trajgen_clip_placed" is the GTA-world form produced by traj_adapter; both
    # carry the same plan/covis sidecars, and omitting the placed kind here would
    # silently SKIP every trajectory rule on exactly the tapes that can be flown.
    if (source.get("kind") in ("trajgen_clip", "trajgen_clip_placed")
            and isinstance(source.get("path"), str)):
        src = Path(source["path"])
        for name in ("plan.json", "covis.json"):
            candidate = src / name
            if candidate.is_file():
                shutil.copy2(candidate, capture / name)
                copied.append(name)
    # dyn_metrics must come from GTA rendered visibility, never from a virtual trajgen subject.
    run_manifest_path = root / "run-manifest.json"
    if run_manifest_path.is_file():
        run_manifest = load_json(run_manifest_path)
        dynamic = run_manifest.get("master", {}).get("dyn_metrics_path")
        if isinstance(dynamic, str) and Path(dynamic).is_file():
            shutil.copy2(dynamic, capture / "dyn_metrics.json")
            copied.append("dyn_metrics.json")
    return copied


def invoke_engine_qc(run_dir, *, expect_duration_s: float | None = None
                    ) -> tuple[int, dict, str]:
    """Run ``engine_qc check-clip --mode mc`` over a run's capture directory.

    Returns ``(exit_code, report, stderr)`` and binds nothing.  Separating the
    invocation from :func:`run_engine_qc` lets the orchestrator obtain a verdict
    while building the PR #405 gate — the gate is written before the run manifest
    exists, so it cannot use the sealing path — without either duplicating
    engine_qc's rule implementations or mutating an already-bound gate.
    """
    root = Path(run_dir).resolve()
    qc_root = ENGINE_DATA / "qc"
    env = dict(os.environ)
    env["PYTHONPATH"] = str(qc_root) + os.pathsep + env.get("PYTHONPATH", "")
    # The checker emits human-readable Unicode problem text inside JSON. On a
    # Chinese Windows host a piped child otherwise inherits GBK and can fail
    # before producing any JSON (for example on U+2212 MINUS SIGN).
    env["PYTHONIOENCODING"] = "utf-8"
    dependency = DependencyManifest.load(root / "config" / "dependency-manifest.json")
    media_dirs = {str(path.parent) for name in ("ffmpeg", "ffprobe")
                  if (path := dependency.component_path(name)) is not None}
    if media_dirs:
        env["PATH"] = os.pathsep.join(sorted(media_dirs)) + os.pathsep + env.get("PATH", "")
    command = [sys.executable, "-m", "engine_qc", "check-clip", str(root / "capture"),
               "--mode", "mc", "--json"]
    if expect_duration_s is not None:
        command.extend(["--expect-duration-s", str(expect_duration_s)])
    proc = subprocess.run(command, cwd=qc_root, env=env, stdout=subprocess.PIPE,
                          stderr=subprocess.PIPE, text=True, encoding="utf-8", errors="replace")
    try:
        report = json.loads(proc.stdout)
    except json.JSONDecodeError as exc:
        raise ContractError(
            f"engine_qc did not return JSON (exit {proc.returncode}): {proc.stderr[-2000:]}"
        ) from exc
    return proc.returncode, report, proc.stderr


def run_engine_qc(run_dir, *, expect_duration_s: float | None = None) -> tuple[int, dict]:
    root = Path(run_dir).resolve()
    # Stage external QC inputs before sealing the run binding. A later source
    # change produces a different binding instead of silently altering QC.
    stage_qc_sidecars(root)
    manifest = load_json(root / "run-manifest.json")
    if manifest.get("dry_run") is True:
        raise ContractError("engine_qc is intentionally blocked for dry-run outputs without GTA pixels")
    gate, binding = _bind_or_verify_gate(root, manifest, allow_bind=True)
    exit_code, report, stderr = invoke_engine_qc(root, expect_duration_s=expect_duration_s)
    report["gta_capture_bridge"] = _qc_bridge_record(
        root, gate, binding, exit_code=exit_code, stderr=stderr)
    write_json_atomic(root / "evidence" / "engine-qc.json", report)
    return exit_code, report


def package_run(run_dir, out_root, *, game: str, group: str, prefix: str,
                scene: str = "") -> dict:
    root = Path(run_dir).resolve()
    dependency = DependencyManifest.load(root / "config" / "dependency-manifest.json")
    dependency.policy.require_delivery()
    if repository_profile_non_trainable():
        raise PolicyError(
            "delivery is blocked by the checked-in GTA game profile (non_trainable=true)"
        )
    if not INDEPENDENT_PACKAGE_GATE_VERIFIER_IMPLEMENTED:
        raise ContractError(
            "GTA packaging is hard-blocked until package-time evidence recomputation "
            "and signed human/rights verification are implemented"
        )
    run_manifest = load_json(root / "run-manifest.json")
    expected_manifest_hash = run_manifest.get("inputs", {}).get(
        "dependency_manifest", {}).get("sha256")
    actual_manifest_hash = sha256_file(root / "config" / "dependency-manifest.json")
    if expected_manifest_hash != actual_manifest_hash:
        raise ContractError("dependency manifest snapshot changed after capture; run provenance is invalid")
    if run_manifest.get("dry_run") is True:
        raise ContractError("dry-run output can never be packaged")
    gate, binding = _bind_or_verify_gate(root, run_manifest, allow_bind=False)
    required_rules = REQUIRED_RULES
    passed = set(gate.get("passed_rules", [])) if gate.get("ok") is True else set()
    missing_rules = required_rules - passed
    if missing_rules:
        raise ContractError("GTA PR #405 package gate is incomplete: " +
                            ", ".join(sorted(missing_rules)))
    qc_path = root / "evidence" / "engine-qc.json"
    if not qc_path.is_file():
        raise ContractError("run engine_qc before packaging")
    qc = load_json(qc_path)
    if qc.get("verdict") != "PASS":
        raise ContractError(f"engine_qc verdict is {qc.get('verdict')!r}; packaging requires PASS")
    _verify_qc_binding(root, qc, gate, binding)

    delivery_root = str(ENGINE_DATA / "delivery")
    inserted = delivery_root not in sys.path
    if inserted:
        sys.path.insert(0, delivery_root)
    try:
        from engine_delivery.pack_mc import ledger_row, pack_clip  # type: ignore
    finally:
        # Do not leave the delivery tree on this process's import path.
        if inserted and delivery_root in sys.path:
            sys.path.remove(delivery_root)

    result = pack_clip(root / "capture", out_root, game=game, group=group,
                       prefix=prefix, qc_verdict="PASS")
    value = result.to_dict()
    value["ledger_row"] = ledger_row(result, game, scene, prefix)
    if not result.ok:
        value["source_run_binding"] = binding
        value["engine_qc"] = file_record(qc_path)
        write_json_atomic(root / "evidence" / "package-report.json", value)
        raise ContractError("engine_delivery refused the run: " + "; ".join(result.problems))
    packaged = {}
    destination = Path(result.dest).resolve()
    for name in DELIVERY_FILES:
        record = file_record(destination / name)
        source = binding["delivery_files"][name]
        if record["bytes"] != source["bytes"] or record["sha256"] != source["sha256"]:
            raise ContractError(f"packaged file differs from its bound source: {name}")
        packaged[name] = record
    value["source_run_binding"] = binding
    value["gta_pr405_gate"] = file_record(root / "evidence" / "gta-pr405-gate.json")
    value["engine_qc"] = file_record(qc_path)
    value["packaged_files"] = packaged
    write_json_atomic(root / "evidence" / "package-report.json", value)
    return value

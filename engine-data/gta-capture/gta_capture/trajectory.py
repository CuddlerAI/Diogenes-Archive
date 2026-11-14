"""Map an ``engine_qc --mode mc`` verdict onto PR #405 gate trajectory rules.

docs/158 §3.1 requires the GTA lane to reuse the existing trajectory contract
rather than re-derive it: ``trajgen`` authors the plan, ``engine_qc`` judges the
**actual exported poses**, and this module translates that judgement into the
gate's rule names. Covisibility is different: engine_qc reads planning curves
from covis.json and does not independently measure GTA surface visibility.
Those PASS results remain planning diagnostics and cannot grant covisibility.

Two properties make the translation honest:

* **Zero tolerance for FAIL.** A family with any FAIL grants nothing, even if the
  specific rules listed below happen to pass.
* **Mode-aware required sets.** ``cam4`` legitimately SKIPs half its rules: form a
  (timewarp) has no top-down geometry and form b (topdown) has no speed change.
  Requiring "no SKIP" would make form b unpassable; requiring only "no FAIL" would
  let a clip that declared *neither* form through.  The declared tape mode selects
  which rules must actually be PASS.

Batch-level docs/69 quotas — aspect ratio, focal length, scene type, dynamic
category, landmark reuse — are **not** covered here.  They are ``check-batch``
concerns and stay outside the per-clip gate (docs/158 §2.5.3 guardrail two).
"""

from __future__ import annotations

from pathlib import Path
from typing import Any, Iterable

from .errors import ContractError
from .jsonio import load_json, sha256_file


TRAJECTORY_VERDICT_SCHEMA = "dm-gta-trajectory-verdict/1"

# No implemented verifier currently binds measured GTA surfaces/visibility to
# the rendered frames. A source JSON boolean (even measured_in_gta=true) cannot
# replace one. Keep this rule out of both new and previously saved verdicts.
UNVERIFIED_GEOMETRY_RULES = frozenset({"covisibility"})

#: Gate rule -> (engine_qc rule prefixes that form the family, rules that must PASS).
TRAJECTORY_FAMILIES: dict[str, tuple[tuple[str, ...], tuple[str, ...]]] = {
    "trajectory_pr405": (
        ("dur.", "frame.", "sync.", "init."),
        ("dur.frames", "frame.continuity", "sync.frames", "sync.frame_align",
         "init.orientation", "init.distance"),
    ),
    "covisibility": (
        ("covis.", "decouple."),
        ("covis.reading", "covis.ratio", "covis.ratio_strict",
         "decouple.pairs", "decouple.all4"),
    ),
    "cam4_reparameterization": (
        ("cam4.",),
        ("cam4.mode",),
    ),
    "motion_limits": (
        ("phys.",),
        ("phys.speed_p95", "phys.accel_p95", "phys.jerk_p95", "phys.angspeed_p95",
         "phys.angspeed_max", "phys.angaccel_p95", "phys.angjerk_p95"),
    ),
    "six_translation_honesty": (
        ("plan.pink",),
        ("plan.pink_coverage", "plan.pink_honesty"),
    ),
}

#: Extra ``cam4`` rules that must PASS for each declared fourth-lane form.
CAM4_MODE_RULES: dict[str, tuple[str, ...]] = {
    "timewarp": ("cam4.switch_count", "cam4.t_sw", "cam4.band_dwell"),
    "topdown": ("cam4.topdown_pitch", "cam4.topdown_alt", "cam4.topdown_route"),
}


def _results(report: Any) -> list[dict]:
    results = report.get("results") if isinstance(report, dict) else None
    if not isinstance(results, list):
        raise ContractError("engine_qc report has no results array")
    rows = []
    for item in results:
        if (not isinstance(item, dict) or not isinstance(item.get("rule"), str)
                or not isinstance(item.get("status"), str)):
            raise ContractError("engine_qc result row is malformed")
        rows.append(item)
    return rows


def _statuses(rows: Iterable[dict], rule: str) -> list[str]:
    return [row["status"] for row in rows if row["rule"] == rule]


def build_trajectory_verdict(qc_report: Any, *, cam4_mode: str,
                             qc_exit_code: int,
                             covisibility_provenance: dict | None = None) -> dict:
    """Decide which trajectory gate rules a QC report actually supports.

    ``cam4_mode`` comes from the placed camera tape, not from the QC report, so a
    clip cannot pick whichever fourth-lane form its results happen to satisfy
    after the fact.
    """
    if cam4_mode not in CAM4_MODE_RULES:
        raise ContractError(
            f"unknown cam4 mode {cam4_mode!r}; expected one of "
            f"{', '.join(sorted(CAM4_MODE_RULES))}")
    rows = _results(qc_report)
    verdict = qc_report.get("verdict") if isinstance(qc_report, dict) else None

    granted: list[str] = []
    planning_passed: list[str] = []
    detail: dict[str, dict] = {}
    for gate_rule, (prefixes, required) in TRAJECTORY_FAMILIES.items():
        if gate_rule == "cam4_reparameterization":
            required = required + CAM4_MODE_RULES[cam4_mode]
        family = [row for row in rows if row["rule"].startswith(prefixes)]
        failed = sorted({row["rule"] for row in family if row["status"] == "FAIL"})
        missing = sorted(rule for rule in required
                         if not (statuses := _statuses(rows, rule))
                         or any(status != "PASS" for status in statuses))
        # An empty family means the rules never ran at all — sidecars absent, or
        # the checker changed its rule ids. Either way it is not evidence.
        qc_ok = bool(family) and not failed and not missing and qc_exit_code == 0
        ok = qc_ok and gate_rule not in UNVERIFIED_GEOMETRY_RULES
        detail[gate_rule] = {
            "granted": ok,
            "qc_family_passed": qc_ok,
            "family_size": len(family),
            "failed_rules": failed,
            "unsatisfied_required_rules": missing,
            "required_rules": sorted(required),
        }
        if gate_rule in UNVERIFIED_GEOMETRY_RULES:
            detail[gate_rule].update({
                "scope": "planning-sidecar diagnostic; GTA covisibility unverified",
                "withheld_reason": "independent GTA rendered-geometry verifier is not implemented",
                "measured_geometry_verified": False,
                "source_provenance": covisibility_provenance,
            })
        if qc_ok:
            planning_passed.append(gate_rule)
        if ok:
            granted.append(gate_rule)

    return {
        "schema": TRAJECTORY_VERDICT_SCHEMA,
        "cam4_mode": cam4_mode,
        "qc_verdict": verdict,
        "qc_exit_code": qc_exit_code,
        "granted_rules": sorted(granted),
        "planning_passed_rules": sorted(planning_passed),
        "rules": detail,
        "note": "kinematic rules use engine_qc PASS on exported poses; covisibility "
                "curves remain planning diagnostics, without verified GTA geometry. "
                "Batch-level docs/69 quotas remain a check-batch concern",
    }


def verdict_for_run(run_dir, tape: dict, *, invoke, stage) -> dict | None:
    """Produce a trajectory verdict for one completed run, or ``None``.

    Shared by the full orchestrator and the operator-gated staged path so both
    decide the trajectory rules the same way.  ``invoke``/``stage`` are injected
    rather than imported to keep this module free of the qc_bridge import that
    would close an import cycle through ``gate``.

    Returns ``None`` — leaving every trajectory rule missing — whenever the tape
    is not a placed trajgen clip, because an anchor bring-up tape carries no
    plan/covis sidecars and there is simply nothing to judge.
    """
    source = tape.get("source") or {}
    cam4_mode = source.get("cam4_mode")
    if source.get("kind") != "trajgen_clip_placed" or cam4_mode not in CAM4_MODE_RULES:
        return None
    try:
        stage(run_dir)
        covis_path = Path(run_dir) / "capture" / "covis.json"
        covis_hash = sha256_file(covis_path) if covis_path.is_file() else None
        covis = load_json(covis_path) if covis_hash else {}
        if not isinstance(covis, dict):
            raise ContractError("covis planning sidecar must be an object")
        provenance = {
            "sidecar_sha256": covis_hash,
            "declared_geometry_source": covis.get("geometry_source", "unspecified"),
            "declared_measured_in_gta": covis.get("measured_in_gta"),
            "independently_verified": False,
        }
        exit_code, report, _ = invoke(run_dir, expect_duration_s=float(tape["duration_s"]))
        if covis_hash and sha256_file(covis_path) != covis_hash:
            raise ContractError("covis planning sidecar changed during engine_qc")
        return build_trajectory_verdict(report, cam4_mode=cam4_mode,
                                        qc_exit_code=exit_code,
                                        covisibility_provenance=provenance)
    except (ContractError, OSError, KeyError, TypeError, ValueError) as exc:
        # A checker that will not run is not evidence that the trajectory is good.
        return {"schema": TRAJECTORY_VERDICT_SCHEMA, "cam4_mode": cam4_mode,
                "qc_verdict": None, "qc_exit_code": -1, "granted_rules": [],
                "rules": {}, "error": str(exc)[:2000]}


def granted_rules(verdict: Any) -> set[str]:
    """Read the granted set back, rejecting anything not shaped like a verdict."""
    if not isinstance(verdict, dict) or verdict.get("schema") != TRAJECTORY_VERDICT_SCHEMA:
        return set()
    if verdict.get("qc_exit_code") != 0:
        return set()
    values = verdict.get("granted_rules")
    if not isinstance(values, list) or not all(isinstance(item, str) for item in values):
        return set()
    # Never grant a name this module does not own, whatever the file says.
    return (set(values) & set(TRAJECTORY_FAMILIES)) - UNVERIFIED_GEOMETRY_RULES

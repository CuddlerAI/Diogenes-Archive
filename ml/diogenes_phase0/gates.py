"""Acceptance-gate checks (Phase 0 plan, research doc §6).

Hard gates (must pass on the val split):
  key_exact   >= gates.key_exact_min     (plan: 0.85 vs VPT 90.6%)
  mouse_r2    >= gates.mouse_r2_min      (plan: 0.9  vs VPT R^2=0.97)

Informational (reported, never blocking):
  key_macro_f1 vs gates.macro_f1_warn — distribution-edge health
  zero-shot metrics on held-out games — the generalization early-signal;
  per the stop-loss rule, near-chance zero-shot => "单游戏 IDM 完全不迁移"
  and the pivot to latent-action + per-game grounding.

A gate report describes ONE run on ITS OWN val split. Two reports are not
a comparison, and reading them as one has already inverted a conclusion:
a 3.2x-data minecraft run reported key_macro_f1 0.2004 against the smaller
run's 0.1563, which read as +28% from more data. Scored on the SAME frames
(eval_checkpoint --split val) the bigger run came out at 0.1469 — worse.
The gap was the val split, not the model: the larger corpus contributed
sessions where a hotbar key scored F1 0.61 and lifted the average.

To compare runs, score both checkpoints on one split. Session hashing is
stable, so a smaller corpus's val sessions stay in the larger corpus's val
set, and the bigger model is clean on them.
"""
from __future__ import annotations

import json
from dataclasses import asdict, dataclass
from pathlib import Path


@dataclass
class Gate:
    name: str
    value: float | None
    threshold: float | None
    passed: bool | None   # None => informational
    level: str            # "hard" | "info"


def check_gates(reports: dict[str, dict], gate_cfg: dict) -> list[Gate]:
    val = reports.get("val", {})
    zs = reports.get("zero_shot", {})
    gates: list[Gate] = []

    def hard(name: str, value, th_key: str):
        th = gate_cfg.get(th_key)
        ok = None if (value is None or th is None) else bool(value >= th)
        gates.append(Gate(name, value, th, ok, "hard"))

    hard("val.key_exact", val.get("key_exact"), "key_exact_min")
    hard("val.mouse_r2_min", val.get("mouse_r2_min"), "mouse_r2_min")

    # learning-signal floors (smoke configs gate on these; real configs on the
    # accuracy gates above)
    if "key_macro_f1_min" in gate_cfg:
        hard("val.key_macro_f1", val.get("key_macro_f1"), "key_macro_f1_min")
    else:
        gates.append(Gate("val.key_macro_f1", val.get("key_macro_f1"),
                          gate_cfg.get("macro_f1_warn"), None, "info"))
    if "bin_acc_min" in gate_cfg:
        accs = [a for a in (val.get("bin_acc_x"), val.get("bin_acc_y"))
                if a is not None]
        hard("val.bin_acc_min", min(accs) if accs else None, "bin_acc_min")
    if zs.get("n"):
        gates.append(Gate("zero_shot.key_exact", zs.get("key_exact"), None, None, "info"))
        gates.append(Gate("zero_shot.key_macro_f1", zs.get("key_macro_f1"), None, None, "info"))
        gates.append(Gate("zero_shot.mouse_r2_min", zs.get("mouse_r2_min"), None, None, "info"))
    return gates


def all_passed(gates: list[Gate]) -> bool:
    return all(g.passed for g in gates if g.level == "hard" and g.passed is not None)


def render_markdown(gates: list[Gate], reports: dict[str, dict]) -> str:
    lines = ["# Phase 0 gate report", "",
             "| gate | value | threshold | status |", "|---|---|---|---|"]
    for g in gates:
        v = "n/a" if g.value is None else f"{g.value:.4f}"
        th = "—" if g.threshold is None else f"{g.threshold:.2f}"
        status = ("PASS" if g.passed else "FAIL") if g.passed is not None else "info"
        lines.append(f"| {g.name} | {v} | {th} | {status} |")
    lines.append("")
    per_key = reports.get("val", {}).get("per_key_f1")
    if per_key:
        lines += ["## per-key F1 (val) — distribution-edge check", "",
                  "| VK | F1 |", "|---|---|"]
        lines += [f"| {vk} | {f1} |" for vk, f1 in sorted(per_key.items(), key=lambda kv: float(kv[1]))]
        lines.append("")
    verdict = "ALL HARD GATES PASSED" if all_passed(gates) else "HARD GATE FAILURE"
    lines += [f"**{verdict}**", ""]
    return "\n".join(lines)


def write_report(run_dir: Path, gates: list[Gate], reports: dict[str, dict]) -> None:
    run_dir.mkdir(parents=True, exist_ok=True)
    (run_dir / "gates_report.json").write_text(json.dumps({
        "gates": [asdict(g) for g in gates],
        "reports": reports,
        "all_hard_passed": all_passed(gates),
    }, indent=2))
    (run_dir / "gates_report.md").write_text(render_markdown(gates, reports))

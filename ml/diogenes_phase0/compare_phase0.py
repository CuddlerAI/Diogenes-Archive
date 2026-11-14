"""Render several Phase 0 runs as one table.

    python -m diogenes_phase0.compare_phase0 runs/idm-minecraft-* runs/idm-valorant-*

The round is a 2x2 — two games at two corpus sizes — plus cross-game
evaluations, and the interesting quantities are differences between runs,
not any single run's numbers. Reading four gates_report.json files by hand
invites quoting one metric from one run against a different metric from
another; VPT's 90.6% got compared to exact-set match that way once already.

Each metric is rendered with the same reading across every column, and the
two key averages are shown side by side because their gap is the diagnostic:
collapse pulls both down, an unlearnable tail pulls only the unrestricted
one down.
"""
from __future__ import annotations

import argparse
import json
from pathlib import Path

METRICS = [
    ("key_exact", "key exact-set", "{:.4f}"),
    ("key_per_key_acc", "key per-key acc (lenient)", "{:.4f}"),
    ("key_macro_f1", "key macro F1 (gated)", "{:.4f}"),
    ("key_macro_f1_common", "key macro F1, support>=1%", "{:.4f}"),
    ("key_n_seen", "keys present", "{:.0f}"),
    ("key_n_common", "keys with real support", "{:.0f}"),
    ("bin_acc_x", "pointer bin acc x", "{:.4f}"),
    ("bin_acc_y", "pointer bin acc y", "{:.4f}"),
    ("mouse_r2_min", "mouse R^2 (min axis)", "{:.4f}"),
    ("n", "val frames", "{:.0f}"),
]


def load_run(path: Path) -> dict:
    """A run dir, or a gates_report.json / eval_checkpoint report directly."""
    if path.is_dir():
        for name in ("gates_report.json", "report.json"):
            if (path / name).exists():
                path = path / name
                break
        else:
            raise SystemExit(f"no gates_report.json under {path}")
    data = json.loads(path.read_text())
    if "reports" in data:                      # train.py's gates report
        return {"metrics": data["reports"].get("val", {}),
                "passed": data.get("all_hard_passed"),
                "gates": data.get("gates", [])}
    return {"metrics": data.get("metrics", data),  # eval_checkpoint report
            "passed": None, "gates": []}


def render(runs: dict[str, dict]) -> str:
    names = list(runs)
    out = ["| metric | " + " | ".join(names) + " |",
           "|---|" + "---|" * len(names)]
    for key, label, fmt in METRICS:
        cells = []
        for n in names:
            v = runs[n]["metrics"].get(key)
            cells.append("—" if v is None else fmt.format(v))
        if all(c == "—" for c in cells):
            continue
        out.append(f"| {label} | " + " | ".join(cells) + " |")

    verdicts = []
    for n in names:
        p = runs[n]["passed"]
        verdicts.append("—" if p is None else ("PASS" if p else "FAIL"))
    if any(v != "—" for v in verdicts):
        out.append("| **hard gates** | " + " | ".join(verdicts) + " |")

    out.append("")
    out.append("key per-key acc is the lenient reading (predicting 'not "
               "pressed' for a rare key is usually right); macro F1 is the "
               "gated one. A large gap between the two macro F1 columns means "
               "a long tail of keys the split cannot teach, not a collapse — "
               "collapse drags both down together.")
    return "\n".join(out)


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("runs", nargs="+", type=Path)
    ap.add_argument("--labels", nargs="*", default=None,
                    help="column names; defaults to run-dir names")
    ap.add_argument("--out", default=None)
    args = ap.parse_args()

    labels = args.labels or [p.name for p in args.runs]
    if len(labels) != len(args.runs):
        raise SystemExit("--labels must match the number of runs")
    table = render({lab: load_run(p) for lab, p in zip(labels, args.runs)})
    print(table)
    if args.out:
        Path(args.out).write_text(table)


if __name__ == "__main__":
    main()

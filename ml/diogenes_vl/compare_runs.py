"""Paired comparison of two eval runs on the same frames.

Comparing headline rates hides whether a difference is real. Round 7 showed
event_class 0.802 -> 0.823 between SFT and GRPO, which reads as +2.1 points;
paired, it was 17 frames improved against 10 worsened out of 300 — McNemar
p=0.248, i.e. noise. Only the paired view answers "did this change anything".

Each metric is scored per frame on the intersection of the two runs' ids,
then summarised as a 2x2 discordance table with an exact two-sided McNemar
test (binomial on the discordant pairs; the chi-square approximation is
unreliable at these counts).

    python -m diogenes_vl.compare_runs --a sft_predictions.json \
        --b grpo_predictions.json --labels SFT GRPO
"""
from __future__ import annotations

import argparse
import json
from math import comb
from pathlib import Path

from .analyze_labels import GAMEPLAY_EVENTS
from .schema import coarse_event, game_matches


def _parsed(p: dict) -> bool:
    return p.get("pred") is not None


def _event_ok(p: dict) -> bool:
    return _parsed(p) and p["pred"]["event_class"] == p.get("ref_event_class")


def _event_coarse_ok(p: dict) -> bool:
    return _parsed(p) and coarse_event(p["pred"]["event_class"]) == coarse_event(
        p.get("ref_event_class") or "")


def _game_ok(p: dict) -> bool:
    return _parsed(p) and game_matches(p["pred"]["game"], p["gt_game"])


METRICS = {
    # name: (scorer, frame filter)
    "json_parsed": (_parsed, lambda p: True),
    "event_class": (_event_ok, lambda p: p.get("ref_event_class") is not None),
    "event_coarse": (_event_coarse_ok,
                     lambda p: p.get("ref_event_class") is not None),
    "game_gameplay": (_game_ok,
                      lambda p: p.get("ref_event_class") in GAMEPLAY_EVENTS),
}


def mcnemar_exact(b: int, c: int) -> float:
    """Two-sided exact McNemar p-value for discordant counts (b, c)."""
    n = b + c
    if n == 0:
        return 1.0
    k = min(b, c)
    tail = sum(comb(n, j) for j in range(0, k + 1)) / 2 ** n
    return min(1.0, 2 * tail)


def compare(a_preds: list[dict], b_preds: list[dict]) -> dict:
    A = {p["id"]: p for p in a_preds}
    B = {p["id"]: p for p in b_preds}
    shared = sorted(set(A) & set(B))
    out = {"n_shared": len(shared), "n_a": len(A), "n_b": len(B), "metrics": {}}
    for name, (ok, keep) in METRICS.items():
        ids = [i for i in shared if keep(A[i])]
        both = sum(1 for i in ids if ok(A[i]) and ok(B[i]))
        only_a = sum(1 for i in ids if ok(A[i]) and not ok(B[i]))   # b worsened
        only_b = sum(1 for i in ids if not ok(A[i]) and ok(B[i]))   # b improved
        neither = len(ids) - both - only_a - only_b
        p = mcnemar_exact(only_a, only_b)
        out["metrics"][name] = {
            "n": len(ids),
            "rate_a": round(both + only_a, 6) / len(ids) if ids else None,
            "rate_b": round(both + only_b, 6) / len(ids) if ids else None,
            "both_ok": both, "neither_ok": neither,
            "b_improved": only_b, "b_worsened": only_a,
            "net": only_b - only_a,
            "p_value": round(p, 4),
            "significant_at_05": bool(p < 0.05),
        }
    return out


def render(rep: dict, label_a: str, label_b: str) -> str:
    lines = [f"Paired comparison on {rep['n_shared']} shared frames "
             f"({label_a}: {rep['n_a']}, {label_b}: {rep['n_b']})", ""]
    lines.append(f"{'metric':16}{'n':>6}{label_a[:8]:>10}{label_b[:8]:>10}"
                 f"{'improved':>10}{'worsened':>10}{'net':>6}{'p':>8}  verdict")
    lines.append("-" * 88)
    for name, m in rep["metrics"].items():
        ra = "n/a" if m["rate_a"] is None else f"{m['rate_a']:.4f}"
        rb = "n/a" if m["rate_b"] is None else f"{m['rate_b']:.4f}"
        verdict = "significant" if m["significant_at_05"] else "within noise"
        lines.append(f"{name:16}{m['n']:>6}{ra:>10}{rb:>10}"
                     f"{m['b_improved']:>10}{m['b_worsened']:>10}"
                     f"{m['net']:>+6}{m['p_value']:>8.3f}  {verdict}")
    lines += ["", "improved/worsened count FRAMES that flipped, not rate points.",
              "p = exact two-sided McNemar on the discordant pairs."]
    return "\n".join(lines)


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--a", required=True, help="baseline predictions.json")
    ap.add_argument("--b", required=True, help="candidate predictions.json")
    ap.add_argument("--labels", nargs=2, default=["A", "B"])
    ap.add_argument("--out", default=None)
    args = ap.parse_args()
    rep = compare(json.load(open(args.a)), json.load(open(args.b)))
    print(render(rep, *args.labels))
    if args.out:
        Path(args.out).write_text(json.dumps(rep, indent=2))
        print(f"\nfull report -> {args.out}")


if __name__ == "__main__":
    main()

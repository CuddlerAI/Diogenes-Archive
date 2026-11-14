"""What a predictor that never looks at a frame would score.

    python -m diogenes_phase0.action_floor --data data/idm_minecraft

Training BCE on its own cannot say whether a head learned anything. Key
presses are sparse and dominated by one or two keys, so a model that emits
each key's base rate and ignores the pixels entirely already drives BCE
low. Without the floor next to it, "bce 0.13" reads as success.

The floor is the entropy of the label distribution, which is closed-form
and costs no GPU:

    H = mean_k [ -p_k log p_k - (1-p_k) log(1-p_k) ]

Measured this way, the two games in the Phase 0 round sat far apart —
minecraft 0.2046, valorant 0.1733 — because valorant's presses concentrate
harder on W (56.6% of frames vs 41.1%). A head can be well under the floor
on one game and above it on the other at the same nominal BCE, so the
comparison has to be per-game.
"""
from __future__ import annotations

import argparse
import json
import math
from pathlib import Path

from .contract import load_actions
from .data.dataset import filter_by_action_density, scan_dataset
from .keys import SCANNED_VKEYS, keys_to_multihot


def binary_entropy(p: float) -> float:
    if p <= 0.0 or p >= 1.0:
        return 0.0
    return -(p * math.log(p) + (1 - p) * math.log(1 - p))


def key_rates(root: str | Path, *, min_key_frac: float | None = 0.10,
              max_chunks: int | None = 400) -> dict:
    """Per-key positive rate over a sample of a dataset root."""
    records = scan_dataset(root)
    if not records:
        raise SystemExit(f"no contract-format chunks under {root}")
    if min_key_frac is not None:
        records, _ = filter_by_action_density(records, min_key_frac=min_key_frac)
    if max_chunks:
        records = records[:max_chunks]

    n = 0
    pos = [0] * len(SCANNED_VKEYS)
    for rec in records:
        for a in load_actions(rec.actions_path):
            for i, v in enumerate(keys_to_multihot(a.keys)):
                pos[i] += int(v)
            n += 1
    if n == 0:
        raise SystemExit(f"no action frames under {root}")
    return {"frames": n, "chunks": len(records),
            "rates": {str(vk): pos[i] / n for i, vk in enumerate(SCANNED_VKEYS)}}


def action_floor(root: str | Path, **kw) -> dict:
    """BCE of the best predictor that ignores the pixels."""
    r = key_rates(root, **kw)
    per_key = {vk: binary_entropy(p) for vk, p in r["rates"].items()}
    return {**r, "per_key_bce": per_key,
            "floor_bce": sum(per_key.values()) / len(per_key)}


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--data", required=True)
    ap.add_argument("--max-chunks", type=int, default=400)
    ap.add_argument("--observed-bce", type=float, default=None,
                    help="training BCE to compare against the floor")
    args = ap.parse_args()

    res = action_floor(args.data, max_chunks=args.max_chunks)
    top = sorted(res["rates"].items(), key=lambda kv: -kv[1])
    print(f"frames={res['frames']} chunks={res['chunks']}")
    print(f"{'vk':>5} {'rate':>8} {'BCE':>8}")
    for vk, p in top:
        if p > 0:
            print(f"{vk:>5} {p:>8.4f} {res['per_key_bce'][vk]:>8.4f}")
    print(f"\nno-pixel floor BCE = {res['floor_bce']:.4f}")
    if args.observed_bce is not None:
        d = res["floor_bce"] - args.observed_bce
        verdict = "below the floor — the head is using the frames" if d > 0 \
            else "AT OR ABOVE the floor — the head is not beating base rates"
        print(f"observed {args.observed_bce:.4f} -> {verdict} "
              f"({100 * d / res['floor_bce']:+.0f}%)")
    print(json.dumps({"floor_bce": res["floor_bce"]}))


if __name__ == "__main__":
    main()

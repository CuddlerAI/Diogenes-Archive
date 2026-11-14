"""Protocol contrast: continuous decoding vs one episode per chunk (D-3-b).

    python -m diogenes_phase0.ext_idm.compare_modes --chunks chunks_val.txt \
        --chunk-preds preds_val_chunk --run-preds preds_val_run \
        --config configs/idm_d2e_zeroshot.yaml --out cmp_val.json

`score.py` reports each protocol separately, and two separate reports are not a
comparison — the same trap gates.py's docstring records. The protocol
difference has to be bootstrapped with BOTH modes resampled on the same chunk
draw, which means computing both in one process. That is all this file does.

It also answers the obvious follow-up. If continuous decoding is worse, the
mechanism matters: state replayed across a 220-second run has no reset, so one
missed release keeps a key stuck for the remainder, whereas a per-chunk episode
re-seeds every 4 seconds. That predicts a metric that DECAYS with distance from
the run start — so the score is also binned by position in the run. A flat
curve would refute the drift story and point at something else (context
trimming, say), which is why it is reported rather than asserted.
"""
from __future__ import annotations

import argparse
import json
from pathlib import Path

import yaml

from ..contract import load_actions
from ..keys import keys_to_multihot
from .metrics import (KeyStats, _states_chunk_mode, _states_run_mode,
                      bootstrap, centers, load_chunk_records, paired_bootstrap)


def per_chunk_stats(records, states, window: int, stride: int) -> list[KeyStats]:
    out = []
    for rec in records:
        actions = load_actions(rec.actions_path)
        n = min(rec.n_frames, len(actions))
        pred_keys, _ = states[str(rec.chunk_dir)]
        st = KeyStats()
        for t in centers(n, window, stride):
            st.add(pred_keys[t], keys_to_multihot(actions[t].keys))
        out.append(st)
    return out


def run_positions(records, run_preds: Path) -> dict[str, float]:
    """chunk -> its position in its run, as a fraction in [0, 1)."""
    pos = {}
    for path in sorted(run_preds.glob("run__*.json")):
        blob = json.loads(path.read_text())
        total = blob["n_frames_total"]
        for m in blob["members"]:
            pos[m["chunk"]] = m["offset"] / total
    return pos


def drift_table(records, stats: list[KeyStats], pos: dict[str, float],
                n_bins: int = 4) -> list[dict]:
    bins = [[] for _ in range(n_bins)]
    for rec, st in zip(records, stats):
        p = pos.get(str(rec.chunk_dir))
        if p is None:
            continue
        bins[min(n_bins - 1, int(p * n_bins))].append(st)
    keys = KeyStats.merge(stats).seen_keys()
    out = []
    for i, b in enumerate(bins):
        rep = KeyStats.merge(b).report(restrict_to=keys) if b else {}
        out.append({"bin": f"{i / n_bins:.0%}-{(i + 1) / n_bins:.0%}",
                    "n_chunks": len(b),
                    "key_macro_f1": rep.get("key_macro_f1"),
                    "key_exact": rep.get("key_exact")})
    return out


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--chunks", required=True, type=Path)
    ap.add_argument("--chunk-preds", required=True, type=Path)
    ap.add_argument("--run-preds", required=True, type=Path)
    ap.add_argument("--config", required=True, type=Path)
    ap.add_argument("--seed", default="gt", choices=["gt", "none"])
    ap.add_argument("--shift-ms", type=float, default=100.0)
    ap.add_argument("--label", default="")
    ap.add_argument("--out", required=True, type=Path)
    args = ap.parse_args()

    cfg = yaml.safe_load(args.config.read_text())["data"]
    window, stride = cfg.get("window", 5), cfg.get("stride", 1)
    chunks = [Path(l.strip()) for l in args.chunks.read_text().splitlines()
              if l.strip()]
    records = load_chunk_records(chunks)

    modes = {}
    for name, fn, pred_dir in (("chunk", _states_chunk_mode, args.chunk_preds),
                               ("run", _states_run_mode, args.run_preds)):
        states, _ = fn(records, pred_dir, args.shift_ms, args.seed)
        missing = [r for r in records if str(r.chunk_dir) not in states]
        if missing:
            raise SystemExit(
                f"{len(missing)} chunk(s) have no {name}-mode prediction — the "
                "protocol contrast must be paired on identical chunks")
        modes[name] = per_chunk_stats(records, states, window, stride)

    report = {
        "label": args.label or args.chunks.stem,
        "seed": args.seed, "shift_ms": args.shift_ms,
        "n_chunks": len(records),
        "chunk_mode": {**KeyStats.merge(modes["chunk"]).report(),
                       **bootstrap(modes["chunk"])},
        "run_mode": {**KeyStats.merge(modes["run"]).report(),
                     **bootstrap(modes["run"])},
        # run - chunk: positive means continuous decoding HELPED
        "run_minus_chunk": paired_bootstrap(modes["run"], modes["chunk"]),
        "drift_run_mode": drift_table(records, modes["run"],
                                      run_positions(records, args.run_preds)),
        "drift_chunk_mode": drift_table(records, modes["chunk"],
                                        run_positions(records, args.run_preds)),
    }
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(json.dumps(report, indent=2))

    d = report["run_minus_chunk"]
    print(f"{report['label']}: chunk={report['chunk_mode']['key_macro_f1']:.4f} "
          f"run={report['run_mode']['key_macro_f1']:.4f}  "
          f"Δ(run−chunk)={d.get('delta_key_macro_f1', float('nan')):+.4f} "
          f"{d.get('delta_key_macro_f1_ci95')} "
          f"excludes0={d.get('macro_f1_ci_excludes_zero')}")
    for row in report["drift_run_mode"]:
        print(f"   run-mode {row['bin']:>9}: n={row['n_chunks']:>3} "
              f"macro_f1={row['key_macro_f1']}")
    print(f"wrote {args.out}")


if __name__ == "__main__":
    main()

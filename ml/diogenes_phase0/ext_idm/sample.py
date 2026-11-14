"""Pick the chunk subset the D-3 comparison is scored on.

    python -m diogenes_phase0.ext_idm.sample --config configs/idm_d2e_zeroshot.yaml \
        --root data/d2e30 --split val --n 40 --out chunks_val.txt

Why a subset at all: Generalist-IDM-1B decodes autoregressively, roughly 20x
slower than real time on an L4, so scoring the full 65,888-window zero_shot
split would be days of GPU. The subset is therefore not a convenience — it is
the eval set, and every system in the comparison is scored on exactly it,
including our own checkpoint. A number from this file is NOT comparable with
the full-split gate report; `score.py` re-scores our checkpoint here rather
than quoting `gates_report.json`, for the reason gates.py's own docstring
gives: two reports on two frame sets are not a comparison.

Sampling is seeded, without replacement, and allocated proportionally across
the split's sessions, so a split that spans two Rainbow Six recordings does not
collapse onto the longer one.
"""
from __future__ import annotations

import argparse
import random
from collections import defaultdict
from pathlib import Path

import yaml

from ..data.dataset import filter_by_action_density, scan_dataset, split_records
from .predict import runs_from_chunks


def _contiguous(by_session, all_by_session, n_total, rng):
    """Longest unbroken stretch of FOOTAGE per session; returns (score, decode).

    Two lists, and the difference between them is the whole point:

    `decode` is every chunk in the run, including the ones `min_key_frac`
    filtered out. Those chunks are perfectly good *observations* — the filter
    decides what we SCORE, not what the model may WATCH. Building the video out
    of scored chunks only would splice a cut into the footage everywhere a
    low-action chunk was dropped, which is exactly the discontinuity this
    condition exists to remove. It also fragments badly: on the val session the
    longest filtered run is 25 chunks, against 100+ unfiltered.

    `score` is the subset that survives the split's filter — the same window
    set the per-chunk condition is scored on, so the two protocols stay paired.

    Contiguity is decided by `gframe` (via `runs_from_chunks`), never by chunk
    numbering: the stall detector drops chunks, so c0006 -> c0007 can be a jump
    of several seconds.
    """
    sessions = sorted(by_session)
    total = sum(len(v) for v in by_session.values())
    score, decode = [], []
    for i, s in enumerate(sessions):
        want = (n_total - len(score) if i == len(sessions) - 1
                else round(n_total * len(by_session[s]) / total))
        if want <= 0:
            continue
        keep = {str(r.chunk_dir) for r in by_session[s]}
        by_dir = {str(r.chunk_dir): r for r in by_session[s]}
        runs = runs_from_chunks([r.chunk_dir for r in all_by_session[s]])
        # rank by how many SCORED chunks a run contains, not by raw length
        runs.sort(key=lambda r: -sum(str(c) in keep for c, _, _ in r))
        best = runs[0]
        n_scored = sum(str(c) in keep for c, _, _ in best)
        if n_scored == 0:
            print(f"  {s}: no run contains a scorable chunk, skipped")
            continue
        # trim to the shortest prefix window holding `want` scored chunks
        idx = [j for j, (c, _, _) in enumerate(best) if str(c) in keep]
        take = idx[:want]
        seg = best[take[0]:take[-1] + 1]
        score += [by_dir[str(c)] for c, _, _ in seg if str(c) in keep]
        decode += [c for c, _, _ in seg]
        print(f"  {s}: {len(take)} scored chunk(s) inside {len(seg)} decoded "
              f"({len(seg) * 4:.0f}s of continuous footage); best run held "
              f"{n_scored} scored of {len(best)}")
    return score, decode


def build_split(cfg: dict, root: Path, split: str):
    d = cfg["data"]
    records = scan_dataset(root)
    if not records:
        raise SystemExit(f"no contract-format chunks under {root}")
    if d.get("min_key_frac") is not None:
        records, fstats = filter_by_action_density(
            records, min_key_frac=d["min_key_frac"])
        print(f"action-density filter: {fstats}")
    splits = split_records(records, holdout_games=d.get("holdout_games", []),
                           val_fraction=d.get("val_fraction", 0.2))
    if split not in splits:
        raise SystemExit(f"unknown split {split!r}")
    return splits[split]


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--config", required=True, type=Path)
    ap.add_argument("--root", required=True, type=Path)
    ap.add_argument("--split", required=True,
                    choices=["train", "val", "zero_shot"])
    ap.add_argument("--n", type=int, required=True)
    ap.add_argument("--seed", type=int, default=0)
    ap.add_argument("--out", required=True, type=Path)
    ap.add_argument("--contiguous", action="store_true",
                    help="pick CONTIGUOUS runs of chunks instead of a scatter "
                         "— required by the continuous-decoding condition "
                         "(research/30 D-3-b), which needs unbroken footage")
    ap.add_argument("--out-decode", type=Path, default=None,
                    help="with --contiguous: where to write the DECODE list "
                         "(every chunk in the run, filtered-out ones included)")
    ap.add_argument("--require-exists", type=Path, default=None,
                    help="second dataset root; keep only chunks present in "
                         "BOTH, so the resolution conditions score the same "
                         "chunk ids rather than two overlapping samples")
    args = ap.parse_args()

    cfg = yaml.safe_load(args.config.read_text())
    records = build_split(cfg, args.root, args.split)
    if not records:
        raise SystemExit(f"split {args.split!r} is empty under {args.root}")

    if args.require_exists:
        keep = []
        for r in records:
            rel = r.chunk_dir.relative_to(args.root)
            if (args.require_exists / rel / "p0.jsonl").exists():
                keep.append(r)
        print(f"require-exists {args.require_exists}: "
              f"{len(keep)}/{len(records)} chunk(s) survive")
        records = keep
        if not records:
            raise SystemExit("no chunk exists under both roots")

    by_session: dict[str, list] = defaultdict(list)
    for r in records:
        by_session[r.chunk_dir.parent.name].append(r)

    rng = random.Random(args.seed)
    if args.contiguous:
        if not args.out_decode:
            raise SystemExit("--contiguous needs --out-decode: the decode list "
                             "includes filtered-out chunks, the score list "
                             "does not, and conflating them splices cuts into "
                             "the footage")
        all_by_session: dict[str, list] = defaultdict(list)
        for r in scan_dataset(args.root):
            all_by_session[r.chunk_dir.parent.name].append(r)
        picked, decode = _contiguous(by_session, all_by_session, args.n, rng)
        picked.sort(key=lambda r: str(r.chunk_dir))
        args.out.write_text("".join(f"{r.chunk_dir}\n" for r in picked))
        args.out_decode.write_text("".join(f"{c}\n" for c in decode))
        print(f"wrote {len(picked)} scored chunk(s) to {args.out} and "
              f"{len(decode)} decode chunk(s) to {args.out_decode} "
              f"(contiguous, split={args.split}, seed={args.seed})")
        return
    picked = []
    sessions = sorted(by_session)
    total = len(records)
    for i, s in enumerate(sessions):
        pool = sorted(by_session[s], key=lambda r: str(r.chunk_dir))
        # proportional allocation; the last session absorbs the rounding so the
        # requested n is hit exactly rather than approached
        if i == len(sessions) - 1:
            k = min(len(pool), args.n - len(picked))
        else:
            k = min(len(pool), round(args.n * len(pool) / total))
        picked += rng.sample(pool, k) if k > 0 else []
        print(f"  {s}: {k}/{len(pool)}")

    picked.sort(key=lambda r: str(r.chunk_dir))
    args.out.write_text("".join(f"{r.chunk_dir}\n" for r in picked))
    print(f"wrote {len(picked)} chunk(s) to {args.out} "
          f"(split={args.split}, seed={args.seed}, pool={total})")


if __name__ == "__main__":
    main()

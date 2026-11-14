"""Build a game-screen bench from contract-format chunks.

Stratified frame sampling over any converted dataset (synthetic, gaming-500,
later cohort data): per-game caps, session-hash val/test split, PNG images +
manifest.jsonl. The `game` field is free ground truth for the recognition
metric (gaming-500 carries it per session).

    python -m diogenes_vl.bench --roots data/gaming500 --out data/bench_g500 \
        --per-game 120 --frame-stride 60
"""
from __future__ import annotations

import argparse
import hashlib
from collections.abc import Sequence
import json
from collections import defaultdict
from pathlib import Path

import numpy as np

from diogenes_phase0.contract import load_actions
from diogenes_phase0.data.dataset import _load_frames, scan_dataset


def _split_of(session: str, val_fraction: float) -> str:
    h = int(hashlib.sha1(session.encode()).hexdigest(), 16) % 1000
    return "val" if h < val_fraction * 1000 else "test"


def assign_splits_per_game(sessions_by_game: dict[str, list[str]],
                           val_fraction: float,
                           holdout_games: Sequence[str] = ()) -> dict[str, str]:
    """Stratify the session split WITHIN each game.

    A global hash split can drop every session of a game into one split, so
    the model is asked to recognize games it never trained on and the score
    measures zero-shot transfer instead of learning — that artefact sank
    round 4, where 87% of val frames were games absent from training.

    Per game with n sessions: at least one goes to val and at least one to
    train whenever n >= 2. Single-session games go to train (they cannot be
    evaluated in-domain, and a lone val game only adds zero-shot noise).
    Order comes from the session-name hash, so the assignment is stable
    across runs.

    `holdout_games` are removed from both splits entirely. The val split is
    what we watch while iterating, so a gate computed on it measures how
    well we tuned against that split, not what the model can do — the same
    structure the IDM line avoids with `holdout_games`. Sessions of a
    holdout game land in "holdout" and are read once, at final judgment.
    """
    hidden = {g.strip().lower() for g in holdout_games}
    out: dict[str, str] = {}
    for game, sessions in sessions_by_game.items():
        if game.strip().lower() in hidden:
            for s_ in set(sessions):
                out[s_] = "holdout"
            continue
        ordered = sorted(set(sessions),
                         key=lambda s: hashlib.sha1(s.encode()).hexdigest())
        n = len(ordered)
        if n == 1:
            out[ordered[0]] = "test"
            continue
        n_val = max(1, min(n - 1, round(val_fraction * n)))
        for i, s in enumerate(ordered):
            out[s] = "val" if i < n_val else "test"
    return out


def build_bench(roots: list[str | Path], out: str | Path, *, per_game: int = 120,
                frame_stride: int = 60, val_fraction: float = 0.5,
                skip_first_s: float = 60.0,
                holdout_games: Sequence[str] = (),
                allow_method_validation_only: bool = True) -> Path:
    from PIL import Image

    from diogenes_phase0.contract import gate_chunks
    out = Path(out)
    (out / "images").mkdir(parents=True, exist_ok=True)

    records = []
    for root in roots:
        records += scan_dataset(root)
    if not records:
        raise SystemExit(f"no contract-format chunks under {roots}")
    gate_chunks([r.meta for r in records],
                allow_method_validation_only=allow_method_validation_only)

    by_game: dict[str, list] = defaultdict(list)
    for r in records:
        by_game[r.meta.game].append(r)

    # stratify the val/test split within each game, not globally
    sessions_by_game = {g: [rec.chunk_dir.parent.name for rec in recs]
                        for g, recs in by_game.items()}
    split_map = assign_splits_per_game(sessions_by_game, val_fraction,
                                       holdout_games=holdout_games)

    manifest = []
    for game, recs in sorted(by_game.items()):
        # candidate (record, frame) positions across the game's WHOLE footage,
        # skipping the session head (launchers/desktop/loading dominate there),
        # then evenly subsample to per_game — sequential-from-start sampling
        # benchmarked the Windows firewall dialog, not the game.
        candidates: list[tuple] = []
        for rec in recs:
            try:
                chunk_idx = int(rec.chunk_dir.name.lstrip("c"))
            except ValueError:
                chunk_idx = 0
            chunk_start_s = chunk_idx * rec.n_frames / rec.meta.fps
            if chunk_start_s < skip_first_s:
                continue
            for t in range(0, rec.n_frames, frame_stride):
                candidates.append((rec, t))
        step = max(1, len(candidates) // per_game)
        for rec, t in candidates[::step][:per_game]:
            frames = _load_frames(rec)
            actions = load_actions(rec.actions_path)
            if t >= min(len(frames), len(actions)):
                continue
            session = rec.chunk_dir.parent.name
            key = f"{rec.meta.sample_key}_f{t:05d}"
            img_rel = f"images/{key}.png"
            Image.fromarray(np.asarray(frames[t])).save(out / img_rel)
            manifest.append({
                "id": key,
                "image": img_rel,
                "game": game,
                "session": session,
                "chunk_dir": str(rec.chunk_dir),
                "frame": t,
                "keys": actions[t].keys,
                "split": split_map[session],
                "is_trainable": rec.meta.is_trainable or rec.meta.rights == "synthetic",
            })

    with open(out / "manifest.jsonl", "w") as f:
        for row in manifest:
            f.write(json.dumps(row) + "\n")
    games = sorted(by_game)
    (out / "bench_info.json").write_text(json.dumps({
        "n_images": len(manifest), "n_games": len(games), "games": games,
        "per_game": per_game, "frame_stride": frame_stride,
        "val_fraction": val_fraction,
    }, indent=2))
    print(f"bench: {len(manifest)} images / {len(games)} games -> {out}")
    return out


def load_manifest(bench_dir: str | Path) -> list[dict]:
    return [json.loads(x) for x in
            open(Path(bench_dir) / "manifest.jsonl") if x.strip()]


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--roots", nargs="+", required=True)
    p.add_argument("--out", required=True)
    p.add_argument("--per-game", type=int, default=120)
    p.add_argument("--frame-stride", type=int, default=60)
    p.add_argument("--val-fraction", type=float, default=0.5)
    p.add_argument("--holdout-games", nargs="*", default=[],
                   help="games withheld from BOTH the training pool and val; "
                        "read once at final judgment, never between rounds")
    p.add_argument("--skip-first-s", type=float, default=60.0,
                   help="drop chunks starting before this session offset "
                        "(launchers/desktop dominate session heads)")
    args = p.parse_args()
    build_bench(args.roots, args.out, per_game=args.per_game,
                holdout_games=args.holdout_games,
                frame_stride=args.frame_stride, val_fraction=args.val_fraction,
                skip_first_s=args.skip_first_s)


if __name__ == "__main__":
    main()

"""Evaluate a trained checkpoint on an arbitrary dataset root.

    python -m diogenes_phase0.eval_checkpoint \
        --checkpoint runs/idm-minecraft-.../checkpoint.pt \
        --data data/idm_valorant --label mc-head-on-valorant

This is the Phase 2 (cross-game transfer) entrypoint. train.py's own
zero_shot split can only hold out games that live under the SAME roots as
the training data, which forces one run to carry both games and splits an
already-small budget. Training one head per game and evaluating each head
on the other game's data measures the same thing without that dilution —
and it measures it in both directions, which a single holdout cannot.

Every knob that shapes a sample (window, input_res, delta_scale) is read
back from the checkpoint's own config rather than re-specified, because a
mismatch there silently produces a meaningless number instead of an error:
a head trained on 224x224 5-frame windows evaluated on 96x96 3-frame ones
would report a transfer failure that is really a preprocessing bug.
"""
from __future__ import annotations

import argparse
import json
from pathlib import Path

import torch
from torch.utils.data import DataLoader

from .data.dataset import (WindowDataset, filter_by_action_density,
                           scan_dataset, split_records)
from .evaluate import run_eval
from .models.idm import build_model
from .train import get_device


def load_checkpoint(path: str | Path, device) -> tuple[torch.nn.Module, dict]:
    ckpt = torch.load(path, map_location=device, weights_only=False)
    cfg = ckpt["config"]
    model = build_model(cfg).to(device)
    model.load_state_dict(ckpt["model"])
    model.eval()
    return model, cfg


def build_eval_loader(cfg: dict, root: str, batch_size: int,
                      num_workers: int = 0,
                      split: str = "all") -> tuple[DataLoader, dict]:
    """Loader over `root` using the sample geometry the head was trained with.

    `split` selects the same subset the training run used. Re-scoring a
    checkpoint on its own game over ALL chunks would include the frames it
    trained on, so a number produced that way is not comparable with the
    gate report even though it looks like the same metric. Cross-game
    evaluation has no such split and uses "all".
    """
    d = cfg["data"]
    records = scan_dataset(root)
    if not records:
        raise SystemExit(f"no contract-format chunks under {root}")
    stats = {"scanned": len(records)}
    if d.get("min_key_frac") is not None:
        records, fstats = filter_by_action_density(
            records, min_key_frac=d["min_key_frac"])
        stats["filter"] = fstats
        if not records:
            raise SystemExit("every chunk was filtered out — check min_key_frac")
    if split != "all":
        splits = split_records(records,
                               holdout_games=d.get("holdout_games", []),
                               val_fraction=d.get("val_fraction", 0.2))
        if split not in splits:
            raise SystemExit(f"unknown split {split!r}")
        records = splits[split]
        if not records:
            raise SystemExit(f"split {split!r} is empty under {root}")
    stats["split"] = split
    stats["chunks"] = len(records)
    ds = WindowDataset(
        records,
        window=d.get("window", 5),
        stride=d.get("stride", 1),
        input_res=tuple(d.get("input_res", [96, 96])),
        delta_scale=d.get("delta_scale", 50.0),
        contract_mode=d.get("contract_mode"),
        allow_method_validation_only=d.get("allow_method_validation_only"),
        delta_source=d.get("delta_source", "pointer"),
        delta_clamp=d.get("delta_clamp"),
        expected_fps=d.get("expected_fps"),
    )
    stats["games"] = sorted({r.meta.game for r in records})
    return DataLoader(ds, batch_size=batch_size, shuffle=False,
                      num_workers=num_workers), stats


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--checkpoint", required=True)
    ap.add_argument("--data", required=True, help="dataset root to evaluate on")
    ap.add_argument("--label", default=None, help="name for this pairing")
    ap.add_argument("--device", default="auto")
    ap.add_argument("--batch-size", type=int, default=None)
    ap.add_argument("--max-batches", type=int, default=None)
    ap.add_argument("--split", choices=["all", "val", "train", "zero_shot"],
                    default="all",
                    help="same-game re-eval needs 'val'; 'all' would include "
                         "trained-on frames and quietly beat the gate report")
    ap.add_argument("--out", default=None, help="write the report json here")
    args = ap.parse_args()

    device = get_device(args.device)
    model, cfg = load_checkpoint(args.checkpoint, device)
    bs = args.batch_size or cfg.get("train", {}).get("batch_size", 32)
    loader, stats = build_eval_loader(cfg, args.data, bs, split=args.split)

    label = args.label or (f"{cfg.get('run_name', 'run')}-on-"
                           f"{Path(args.data).name}[{args.split}]")
    report = {
        "label": label,
        "checkpoint": str(args.checkpoint),
        "trained_on": cfg.get("data", {}).get("roots"),
        "evaluated_on": args.data,
        "data": stats,
        "metrics": run_eval(model, loader, device, max_batches=args.max_batches),
    }
    print(json.dumps(report, indent=2))
    if args.out:
        Path(args.out).write_text(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()

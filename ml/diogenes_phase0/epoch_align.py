"""How many steps a bigger corpus needs to see the same number of epochs.

    python -m diogenes_phase0.epoch_align --base configs/idm_valorant.yaml \
        --scaled configs/idm_valorant_deep.yaml --base-steps 12000

The first scale round held max_steps at 12000 for both corpora on purpose, so
that data volume was the only variable. The 6.55x larger corpus got 0.38
epochs against the smaller one's 2.47 and lost (macro F1 0.2055 -> 0.1825),
which answers "more data at the same compute" — not "more data with compute
scaled to match it". Only the second question decides whether the 50-100 h
knee is worth buying, and answering it needs a step count, which needs a
ratio.

The ratio has to be over TRAIN-split windows, not hours and not chunks. The
action-density filter drops whole sessions, the session hash sends ~12% to
val, and window/stride decide how many samples a chunk yields; each of those
shifts the number, and a ratio that is off by 20% still looks plausible while
handing the bigger corpus a different epoch count than the one it was
supposed to match. So the count runs through the same filter, the same split
and the same WindowDataset the training loop builds — including the
compliance and fps gates, which a count that quietly bypassed them would
overstate.

Measured for the valorant pair: 233,392 vs 1,529,866 train windows = 6.55x,
so 12000 -> 78,750 steps at a 250 (log_every) multiple.
"""
from __future__ import annotations

import argparse
import json
from pathlib import Path

import yaml

from .data.dataset import (WindowDataset, filter_by_action_density,
                           scan_dataset, split_records)


def train_windows(root: str | Path, data_cfg: dict) -> int:
    """Training windows a run over `root` would actually iterate.

    `data_cfg` is a config's `data:` block — the same one the run uses, so a
    count and a run cannot disagree about the filter, the split or the stride.
    """
    records = scan_dataset(root)
    if not records:
        raise SystemExit(f"no contract-format chunks under {root}")
    records, _ = filter_by_action_density(
        records, min_key_frac=data_cfg.get("min_key_frac", 0.10))
    train = split_records(
        records,
        holdout_games=data_cfg.get("holdout_games", []),
        val_fraction=data_cfg.get("val_fraction", 0.2),
    )["train"]
    return len(WindowDataset(
        train,
        window=data_cfg.get("window", 5),
        stride=data_cfg.get("stride", 1),
        input_res=tuple(data_cfg.get("input_res", [96, 96])),
        delta_scale=data_cfg.get("delta_scale", 50.0),
        contract_mode=data_cfg.get("contract_mode"),
        allow_method_validation_only=data_cfg.get(
            "allow_method_validation_only"),
        expected_fps=data_cfg.get("expected_fps"),
    ))


def aligned_steps(base_windows: int, scaled_windows: int, base_steps: int,
                  *, multiple_of: int = 250) -> int:
    """Steps that give `scaled_windows` the epoch count `base_steps` gave.

    Rounded to a `multiple_of` boundary (log_every) so the last logged step is
    the last trained step; the rounding moves the epoch count by well under a
    percent at these sizes.
    """
    if base_windows <= 0:
        raise ValueError("base corpus has no train windows to align against")
    exact = base_steps * scaled_windows / base_windows
    if multiple_of <= 1:
        return int(round(exact))
    return int(round(exact / multiple_of)) * multiple_of


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--base", required=True,
                    help="config of the run whose epoch count is being matched")
    ap.add_argument("--scaled", required=True,
                    help="config of the larger-corpus run being sized")
    ap.add_argument("--base-steps", type=int, required=True)
    ap.add_argument("--multiple-of", type=int, default=250)
    ap.add_argument("--out", default=None)
    args = ap.parse_args()

    scaled_cfg = yaml.safe_load(Path(args.scaled).read_text())
    base_cfg = yaml.safe_load(Path(args.base).read_text())
    # one data block for both counts: a ratio between two differently
    # filtered/split corpora is not a ratio of anything.
    data_cfg = scaled_cfg["data"]

    base_n = train_windows(base_cfg["data"]["roots"][0], data_cfg)
    scaled_n = train_windows(data_cfg["roots"][0], data_cfg)
    steps = aligned_steps(base_n, scaled_n, args.base_steps,
                          multiple_of=args.multiple_of)
    bs = scaled_cfg["train"].get("batch_size", 32)
    report = {
        "base_root": base_cfg["data"]["roots"][0],
        "scaled_root": data_cfg["roots"][0],
        "base_train_windows": base_n,
        "scaled_train_windows": scaled_n,
        "ratio": round(scaled_n / base_n, 4),
        "base_steps": args.base_steps,
        "aligned_steps": steps,
        "base_epochs": round(args.base_steps * bs / base_n, 3),
        "aligned_epochs": round(steps * bs / scaled_n, 3),
    }
    print(json.dumps(report, indent=2))
    if args.out:
        Path(args.out).write_text(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()

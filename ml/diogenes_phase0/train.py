"""Phase 0 IDM training loop.

    python -m diogenes_phase0.train --config configs/smoke.yaml [--check-gates]

Reads a yaml config, builds contract-format datasets, trains the head (trunk
frozen), evaluates val + zero-shot splits, checks the acceptance gates and
writes everything under runs/<run_name>/<timestamp>/.
"""
from __future__ import annotations

import argparse
import json
import os
import math
import sys
import time
from pathlib import Path

import torch
import yaml
from torch.utils.data import DataLoader

from .data.dataset import (WindowDataset, filter_by_action_density,
                           scan_dataset, split_records)
from .evaluate import run_eval
from .gates import all_passed, check_gates, render_markdown, write_report
from .models.idm import build_model, trainable_params


def get_device(name: str = "auto") -> torch.device:
    if name != "auto":
        return torch.device(name)
    if torch.cuda.is_available():
        return torch.device("cuda")
    return torch.device("cpu")


def build_loaders(cfg: dict) -> dict[str, DataLoader | None]:
    d = cfg["data"]
    records = []
    for root in d["roots"]:
        recs = scan_dataset(root)
        if not recs:
            raise SystemExit(f"no contract-format chunks under {root}")
        records += recs
    # drop chunks with no learnable action signal (gamepad / mouse-only
    # titles); dropped games are printed so they are not silently absent
    if d.get("min_key_frac") is not None:
        records, fstats = filter_by_action_density(
            records, min_key_frac=d["min_key_frac"])
        print(f"action-density filter: {json.dumps(fstats)}")
        if not records:
            raise SystemExit("every chunk was filtered out — check min_key_frac")

    splits = split_records(records, holdout_games=d.get("holdout_games", []),
                           val_fraction=d.get("val_fraction", 0.2))
    if not splits["val"]:  # tiny datasets: fall back to a slice of train
        k = max(1, len(splits["train"]) // 5)
        splits["val"] = splits["train"][-k:]
        splits["train"] = splits["train"][:-k]

    def make(recs, shuffle):
        if not recs:
            return None
        ds = WindowDataset(
            recs, window=d.get("window", 5), stride=d.get("stride", 1),
            input_res=tuple(d.get("input_res", [96, 96])),
            delta_scale=d.get("delta_scale", 50.0),
            contract_mode=d.get("contract_mode"),
            allow_method_validation_only=d.get("allow_method_validation_only"),
            delta_source=d.get("delta_source", "pointer"),
            delta_clamp=d.get("delta_clamp"),
            expected_fps=d.get("expected_fps"),
        )
        bs = cfg["train"].get("batch_size", 32)
        return DataLoader(ds, batch_size=bs, shuffle=shuffle,
                          num_workers=cfg["train"].get("num_workers", 0),
                          drop_last=shuffle)

    print(f"chunks: train={len(splits['train'])} val={len(splits['val'])} "
          f"zero_shot={len(splits['zero_shot'])}")
    return {"train": make(splits["train"], True),
            "val": make(splits["val"], False),
            "zero_shot": make(splits["zero_shot"], False)}


def compute_loss(out: dict, batch: dict, w: dict, device) -> tuple[torch.Tensor, dict]:
    keys = batch["keys"].to(device)
    bce = torch.nn.functional.binary_cross_entropy_with_logits(out["keys"], keys)
    ce_x = torch.nn.functional.cross_entropy(out["bin_x"], batch["bin_x"].to(device))
    ce_y = torch.nn.functional.cross_entropy(out["bin_y"], batch["bin_y"].to(device))
    reg = torch.nn.functional.smooth_l1_loss(out["delta"], batch["delta"].to(device))
    loss = (w.get("keys", 1.0) * bce + w.get("bins", 0.5) * (ce_x + ce_y)
            + w.get("delta", 2.0) * reg)
    return loss, {"loss": loss.item(), "bce": bce.item(),
                  "ce_bins": (ce_x + ce_y).item(), "reg": reg.item()}



def save_checkpoint(run_dir, model, cfg: dict, step: int) -> None:
    """Write checkpoint.pt so an interrupted run keeps its weights.

    Written via a temp file and os.replace: a crash partway through a ~100MB
    save would otherwise leave a truncated checkpoint.pt in place of the last
    good one, turning a recoverable interruption into a total loss.
    """
    tmp = run_dir / "checkpoint.pt.tmp"
    torch.save({"model": model.state_dict(), "config": cfg, "step": step}, tmp)
    os.replace(tmp, run_dir / "checkpoint.pt")


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--config", required=True)
    ap.add_argument("--device", default="auto")
    ap.add_argument("--max-steps", type=int, default=None)
    ap.add_argument("--check-gates", action="store_true",
                    help="exit 2 if a hard acceptance gate fails")
    args = ap.parse_args()

    cfg = yaml.safe_load(Path(args.config).read_text())
    torch.manual_seed(cfg.get("seed", 0))
    device = get_device(args.device)

    run_dir = Path(cfg.get("output", {}).get("dir", "runs")) / \
        f"{cfg.get('run_name', 'run')}-{time.strftime('%Y%m%d-%H%M%S')}"
    run_dir.mkdir(parents=True, exist_ok=True)
    (run_dir / "config.yaml").write_text(yaml.safe_dump(cfg))

    loaders = build_loaders(cfg)
    model = build_model(cfg).to(device)
    print(f"trainable params: {trainable_params(model):,} (budget 50M) on {device}")

    tcfg = cfg["train"]
    max_steps = args.max_steps or tcfg.get("max_steps", 400)
    warmup = tcfg.get("warmup_steps", max(20, max_steps // 20))
    opt = torch.optim.AdamW(
        (p for p in model.parameters() if p.requires_grad),
        lr=tcfg.get("lr", 3e-4), weight_decay=tcfg.get("weight_decay", 0.01))
    sched = torch.optim.lr_scheduler.LambdaLR(
        opt, lambda s: min(1.0, (s + 1) / warmup) *
        (0.5 * (1 + math.cos(math.pi * min(1.0, s / max_steps)))))

    log_path = run_dir / "train_log.jsonl"
    step = 0
    # A 16-hour run that only writes weights at the end is one crash away
    # from having produced nothing.
    save_every = int(tcfg.get("save_every", 0))
    t0 = time.time()
    model.train()
    while step < max_steps:
        for batch in loaders["train"]:
            out = model(batch["frames"].to(device))
            loss, parts = compute_loss(out, batch, tcfg.get("loss_weights", {}), device)
            opt.zero_grad(set_to_none=True)
            loss.backward()
            torch.nn.utils.clip_grad_norm_(
                (p for p in model.parameters() if p.requires_grad), 1.0)
            opt.step()
            sched.step()
            step += 1
            if step % tcfg.get("log_every", 25) == 0 or step == 1:
                rec = {"step": step, **parts, "lr": sched.get_last_lr()[0],
                       "sec": round(time.time() - t0, 1)}
                print(json.dumps(rec))
                with open(log_path, "a") as f:
                    f.write(json.dumps(rec) + "\n")
            if save_every and step % save_every == 0:
                save_checkpoint(run_dir, model, cfg, step)
            if step >= max_steps:
                break
        model.train()

    # Before eval, not after: eval on a large val split is itself an hour of
    # work, and a failure there would otherwise discard every trained weight.
    save_checkpoint(run_dir, model, cfg, step)

    reports: dict[str, dict] = {}
    reports["val"] = run_eval(model, loaders["val"], device)
    if loaders["zero_shot"] is not None:
        reports["zero_shot"] = run_eval(model, loaders["zero_shot"], device)

    gates = check_gates(reports, cfg.get("gates", {}))
    write_report(run_dir, gates, reports)

    print(render_markdown(gates, reports))
    print(f"run dir: {run_dir}")
    if args.check_gates and not all_passed(gates):
        sys.exit(2)


if __name__ == "__main__":
    main()

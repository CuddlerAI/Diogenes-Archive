"""BC 训练循环 (W2)。pure torch 单进程版; 多卡 DDP/accelerate 排在 CS1 (research/04 §4.2)。

用法:
    python -m diogenes_player.train.bc --plane /path/train_plane \
        --steps 300 --batch 4 --t 96 --burn-in 16 --allow-untrainable --out runs/smoke

日志: <out>/log.jsonl 每行一步; checkpoint: <out>/ckpt.pt (含 config, 可复现)。
超参出处: VPT BC (lr/wd/clip) + 尺寸修正, research/04 §4.2。
"""
from __future__ import annotations

import argparse
import json
import math
import time
from dataclasses import asdict
from pathlib import Path

import torch

from ..action_registry import default_cod_vocab
from ..data_torch import make_loader
from ..models.losses import bc_loss, estimate_pos_weight
from ..models.policy import PlayerPolicy, PolicyConfig
from ..physical_vocab import PhysicalInputVocab


def pick_device(name: str = "auto") -> torch.device:
    if name != "auto":
        return torch.device(name)
    if torch.cuda.is_available():
        return torch.device("cuda")
    if torch.backends.mps.is_available():
        return torch.device("mps")
    return torch.device("cpu")


def cosine_lr(step: int, total: int, base: float, warmup: int) -> float:
    if step < warmup:
        return base * (step + 1) / warmup
    p = (step - warmup) / max(1, total - warmup)
    return base * 0.5 * (1 + math.cos(math.pi * p))


def train(args: argparse.Namespace) -> dict:
    device = pick_device(args.device)
    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)

    vocab = default_cod_vocab()   # R0: COD 语义词表（未来按 --vocab 选）
    physical_vocab = PhysicalInputVocab.read(
        Path(args.plane) / "physical_input_vocab.json")
    cfg = PolicyConfig(encoder=args.encoder, d_model=args.d_model,
                       gru_hidden=args.hidden, gru_layers=args.gru_layers,
                       n_actions=vocab.n_actions,
                       n_keys=physical_vocab.n_inputs)
    model = PlayerPolicy(cfg).to(device)
    params = model.num_params()

    loader = make_loader(args.plane, t=args.t, batch_size=args.batch,
                         workers=args.workers,
                         samples_per_epoch=args.steps * args.batch,
                         allow_untrainable=args.allow_untrainable,
                         seed=args.seed, vocab=vocab,
                         physical_vocab=physical_vocab)

    # pos_weight: 预扫一个 batch 估计 (CS1: 改为 ingest 期全量统计)
    it = iter(loader)
    first = next(it)
    pos_weight = (estimate_pos_weight(first["actions"].flatten(0, 1)).to(device)
                  if args.pos_weight else None)

    opt = torch.optim.AdamW(
        [p for p in model.parameters() if p.requires_grad],
        lr=args.lr, weight_decay=args.wd)

    log_f = (out / "log.jsonl").open("w")
    t_start = time.time()
    step = 0
    metrics_last: dict = {}

    def run_batch(batch: dict) -> dict:
        nonlocal step
        frames = batch["frames"].to(device, non_blocking=True)
        actions = batch["actions"].to(device, non_blocking=True)
        camera = batch["camera_bins"].to(device, non_blocking=True)
        lr = cosine_lr(step, args.steps, args.lr, args.warmup)
        for g in opt.param_groups:
            g["lr"] = lr
        out_a, out_c, _ = model(frames)
        losses = bc_loss(out_a, out_c, actions, camera, burn_in=args.burn_in,
                         pos_weight=pos_weight)
        opt.zero_grad(set_to_none=True)
        losses["loss"].backward()
        torch.nn.utils.clip_grad_norm_(model.parameters(), args.clip)
        opt.step()
        step += 1
        rec = {"step": step, "lr": round(lr, 6),
               **{k: round(float(v), 5) for k, v in losses.items()},
               "sps": round(step * args.batch / (time.time() - t_start), 2)}
        log_f.write(json.dumps(rec) + "\n")
        if step % args.log_every == 0 or step == 1:
            print(rec, flush=True)
        return rec

    metrics_last = run_batch(first)
    for batch in it:
        if step >= args.steps:
            break
        metrics_last = run_batch(batch)

    ckpt = {"model": model.state_dict(), "config": asdict(cfg),
            "vocab": vocab.as_dict(), "train_args": vars(args),
            "physical_input_vocab": physical_vocab.as_dict(),
            "physical_input_vocab_hash": physical_vocab.content_hash,
            "step": step, "params": params}
    torch.save(ckpt, out / "ckpt.pt")
    log_f.close()
    summary = {"final": metrics_last, "params": params,
               "physical_input_vocab_hash": physical_vocab.content_hash,
               "device": str(device), "out": str(out)}
    (out / "summary.json").write_text(json.dumps(summary, indent=2))
    print(json.dumps(summary, indent=2))
    return summary


def build_parser() -> argparse.ArgumentParser:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--plane", required=True)
    ap.add_argument("--out", default="runs/bc")
    ap.add_argument("--steps", type=int, default=300)
    ap.add_argument("--batch", type=int, default=4)
    ap.add_argument("--t", type=int, default=96)
    ap.add_argument("--burn-in", type=int, default=16)
    ap.add_argument("--encoder", default="tiny_cnn",
                    choices=["tiny_cnn", "effnet_b0", "dinov3_s"])
    ap.add_argument("--d-model", type=int, default=256)
    ap.add_argument("--hidden", type=int, default=512)
    ap.add_argument("--gru-layers", type=int, default=1)
    ap.add_argument("--lr", type=float, default=2e-4)
    ap.add_argument("--wd", type=float, default=0.04)
    ap.add_argument("--clip", type=float, default=5.0)
    ap.add_argument("--warmup", type=int, default=20)
    ap.add_argument("--workers", type=int, default=2)
    ap.add_argument("--seed", type=int, default=0)
    ap.add_argument("--device", default="auto")
    ap.add_argument("--pos-weight", action="store_true", default=True)
    ap.add_argument("--no-pos-weight", dest="pos_weight", action="store_false")
    ap.add_argument("--allow-untrainable", action="store_true")
    ap.add_argument("--scheme", default="dm1-v2", choices=["dm1-v2", "legacy"],
                    help="平面的 μ-law 刻度（记入 ckpt 供 eval 解码；须与 ingest 一致）")
    ap.add_argument("--log-every", type=int, default=20)
    return ap


if __name__ == "__main__":
    train(build_parser().parse_args())

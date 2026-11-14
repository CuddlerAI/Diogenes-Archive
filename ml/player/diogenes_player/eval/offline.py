"""离线评估 + 回归门 (R0, research/07 §4.2 P1 门)。

指标:
- 语义动作 per-action P/R/F1（瞬时类 melee/reload/lethal 单独分组）
- camera bin top-1 + 解码 R²（对 raw delta）；小幅度鼠标 (|delta|≤4px) 分组
- macro-F1（active）作回归门主指标

held-out 按 session 切分（research/07 §4）。当前用固定 seed 窗口抽样；CS1 加 --split-file。

用法:
    python -m diogenes_player.eval.offline --plane P --ckpt runs/bc/ckpt.pt \
        --n 64 --allow-untrainable [--gate baseline.json --tol 0.02]
"""
from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

import numpy as np
import torch

from .. import contract
from ..action_registry import vocab_from_dict
from ..data_torch import sample_to_tensors
from ..dataset import WindowDataset
from ..models.policy import PlayerPolicy, PolicyConfig
from ..physical_vocab import PhysicalInputVocab, physical_vocab_from_checkpoint

# 瞬时/稀疏动作：帧内发生即重要，单独盯（research/07 §4 P1）
BURST_ACTIONS = ("combat.melee", "combat.reload", "combat.lethal_equipment",
                 "combat.tactical_equipment", "move.jump_mantle")
SMALL_DELTA_PX = 4


def _decode_vec(scheme: str) -> np.ndarray:
    return np.array([contract.decode_bin(b, scheme)
                     for b in range(-contract.MAX_BIN, contract.MAX_BIN + 1)],
                    dtype=np.float32)


def load_policy(ckpt_path: str, device: torch.device):
    ckpt = torch.load(ckpt_path, map_location=device, weights_only=False)
    model = PlayerPolicy(PolicyConfig(**ckpt["config"])).to(device)
    model.load_state_dict(ckpt["model"])
    model.eval()
    vocab = vocab_from_dict(ckpt["vocab"])
    physical_vocab = physical_vocab_from_checkpoint(ckpt)
    scheme = ckpt.get("train_args", {}).get("scheme", contract.DEFAULT_SCHEME)
    return model, vocab, physical_vocab, scheme


@torch.no_grad()
def evaluate(model: PlayerPolicy, plane: str, vocab, *,
             physical_vocab: PhysicalInputVocab,
             n: int, t: int,
             allow_untrainable: bool, seed: int, device: torch.device,
             scheme: str = contract.DEFAULT_SCHEME) -> dict:
    ds = WindowDataset(plane, t=t, vocab=vocab,
                       physical_vocab=physical_vocab,
                       allow_untrainable=allow_untrainable, seed=seed)
    A = vocab.n_actions
    tp = np.zeros(A); fp = np.zeros(A); fn = np.zeros(A); support = np.zeros(A)
    cam_hits = np.zeros(2); cam_total = 0
    pred_d: list[np.ndarray] = []; true_d: list[np.ndarray] = []
    small_hit = np.zeros(2); small_total = np.zeros(2)
    decode = _decode_vec(scheme)

    for _ in range(n):
        raw = ds.sample()
        s = sample_to_tensors(raw)
        act_logits, cam_logits, _ = model(s["frames"].unsqueeze(0).to(device))
        pa = (torch.sigmoid(act_logits)[0] > 0.5).cpu().numpy()
        ta = s["actions"].numpy() > 0.5
        tp += (pa & ta).sum(0); fp += (pa & ~ta).sum(0)
        fn += (~pa & ta).sum(0); support += ta.sum(0)

        pb = cam_logits[0].argmax(-1).cpu().numpy()   # [T,2]
        tb = s["camera_bins"].numpy()
        cam_hits += (pb == tb).sum(0); cam_total += tb.shape[0]
        pred_d.append(decode[pb]); true_d.append(raw["raw_delta"])
        # 小幅度鼠标分组（死区敏感度）
        small = np.abs(raw["raw_delta"]) <= SMALL_DELTA_PX
        small_hit += ((pb == tb) & small).sum(0); small_total += small.sum(0)

    prec = tp / np.maximum(tp + fp, 1)
    rec = tp / np.maximum(tp + fn, 1)
    f1 = 2 * prec * rec / np.maximum(prec + rec, 1e-9)
    active = support > 0

    pred = np.concatenate(pred_d); true = np.concatenate(true_d)
    ss_res = ((true - pred) ** 2).sum(0)
    ss_tot = ((true - true.mean(0)) ** 2).sum(0)
    r2 = 1 - ss_res / np.maximum(ss_tot, 1e-9)

    per_action = {vocab.actions[i]: {"f1": round(float(f1[i]), 4),
                                     "support": int(support[i])}
                  for i in range(A) if support[i] > 0}
    burst = {a: per_action[a] for a in BURST_ACTIONS if a in per_action}

    return {
        "n_windows": n, "t": t,
        "action_macro_f1_active": round(float(f1[active].mean()) if active.any() else 0.0, 4),
        "action_per_action": per_action,
        "burst_actions": burst,
        "camera_top1_yaw": round(float(cam_hits[0] / cam_total), 4),
        "camera_top1_pitch": round(float(cam_hits[1] / cam_total), 4),
        "camera_r2_yaw": round(float(r2[0]), 4),
        "camera_r2_pitch": round(float(r2[1]), 4),
        "small_delta_top1_yaw": round(float(small_hit[0] / max(small_total[0], 1)), 4),
        "small_delta_top1_pitch": round(float(small_hit[1] / max(small_total[1], 1)), 4),
    }


GATE_KEYS = ("action_macro_f1_active", "camera_top1_yaw", "camera_top1_pitch")


def check_gate(metrics: dict, baseline: dict, tol: float) -> list[str]:
    fails = []
    for k in GATE_KEYS:
        if k in baseline and metrics[k] < baseline[k] - tol:
            fails.append(f"{k}: {metrics[k]} < baseline {baseline[k]} - tol {tol}")
    return fails


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--plane", required=True)
    ap.add_argument("--ckpt", required=True)
    ap.add_argument("--n", type=int, default=64)
    ap.add_argument("--t", type=int, default=96)
    ap.add_argument("--seed", type=int, default=1234)
    ap.add_argument("--device", default="cpu")
    ap.add_argument("--allow-untrainable", action="store_true")
    ap.add_argument("--gate", default=None, help="baseline metrics JSON")
    ap.add_argument("--tol", type=float, default=0.02)
    ap.add_argument("--save", default=None)
    args = ap.parse_args()

    device = torch.device(args.device)
    model, vocab, physical_vocab, scheme = load_policy(args.ckpt, device)
    metrics = evaluate(model, args.plane, vocab,
                       physical_vocab=physical_vocab, n=args.n, t=args.t,
                       allow_untrainable=args.allow_untrainable,
                       seed=args.seed, device=device, scheme=scheme)
    print(json.dumps(metrics, indent=2, ensure_ascii=False))
    if args.save:
        Path(args.save).write_text(json.dumps(metrics, indent=2))
    if args.gate:
        fails = check_gate(metrics, json.loads(Path(args.gate).read_text()), args.tol)
        if fails:
            print("REGRESSION GATE FAILED:\n" + "\n".join(fails), file=sys.stderr)
            sys.exit(1)
        print("regression gate: PASS")


if __name__ == "__main__":
    main()

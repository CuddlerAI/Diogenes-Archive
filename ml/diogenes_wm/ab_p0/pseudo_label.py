"""AB-P0 第 2 步 · 微调后的 IDM 头对全部 chunk 输出伪标签 + 置信度 c_t.

    python -m diogenes_wm.ab_p0.pseudo_label --checkpoint <checkpoint.pt> \
        --src <chunk 契约根> --out <伪标签根> [--threshold 0.5] [--limit N]

## 伪标注在 30 fps chunk 侧做, 不在训练平面侧做

这是 35 §3.3 点名的第二个口径坑: 16/24 fps 平面上的帧有一部分是重采样产物,
给它们打标签等于给不存在的输入发明动作。本工具只吃 chunk 契约目录(源帧率,
帧与 p0.jsonl 1:1), 产物再由 bridge 经 wm01 的同一重采样纪律进平面。

## 输出与原生动作同构

`<out>/<session>/<chunk>/pseudo.npz`, 帧数与源 p0.jsonl 严格 1:1:

    keys              [T,12] uint8   SCANNED_VKEYS 序的多热标签(prob >= threshold)
    keys_prob         [T,12] f32     每键 sigmoid 概率 P(按下)
    keys_conf         [T,12] f32     c_t = 所发标签的置信度 = p·b + (1-p)·(1-b)
                                     (keys 头是逐键 BCE, 35 号文说的 softmax 置信度
                                      在多热头上的对应物是 sigmoid; bins 头才是 softmax)
    pointer_from_bins [T,2]  f32     bin 头 softmax 期望解码的指针位移(VPT 式,
                                     与 evaluate.py mouse_r2 同一解码)
    pointer_bins_pred [T,2]  i8      argmax 的带符号 bin
    bins_conf         [T,2]  f32     bin 头 argmax 的 softmax 概率
    delta_head        [T,2]  f32     回归辅助头 × delta_scale(诊断用)

边界帧(前后各 window//2 帧)用夹取窗口(clamp)预测 —— 复制端帧而不是丢帧,
1:1 才守得住; `edge_halo` 记在 npz 里, 下游要剔除可自行剔除。

## 治理与质量都跟着数据走

- 只接受过 `is_trainable` 门(四项与)的 chunk —— 给不可训练数据打伪标签,
  等于把治理判断推给下游某个忘了检查的人;
- 每个 chunk 旁写 meta.json, **原样继承**源 chunk 的 rights/tags(含
  `no_publish` / `no_product_training`), 附 `pseudo_label` 标记与 checkpoint 指纹;
- manifest.json 汇总标注器质量的读数: 六通道(wm01 投影后)上 伪标签 vs 原生
  语义真值 的 per-channel F1 + 两侧阳性率 + 平均置信度。反方 4(35 §4)问的
  「标注器有多差」—— 答案直接躺在产物里, 不用另跑一轮。
"""
from __future__ import annotations

import argparse
import json
from pathlib import Path

import numpy as np
import torch

from diogenes_phase0.contract import gate_chunks, gate_fps, load_actions
from diogenes_phase0.data.dataset import _load_frames, scan_dataset
from diogenes_phase0.eval_checkpoint import load_checkpoint
from diogenes_phase0.keys import (FOVEATED_MAX_BIN, N_BINS, SCANNED_VKEYS,
                                  class_to_delta)
from diogenes_phase0.train import get_device

from ..wm01.action_space import ActionSpace, scanned_to_wm01


@torch.no_grad()
def label_chunk(model, frames: np.ndarray, *, window: int,
                input_res: tuple[int, int], delta_scale: float,
                batch_size: int, device,
                threshold: float) -> dict[str, np.ndarray]:
    """一个 chunk 的全帧伪标签。帧预处理与 WindowDataset.__getitem__ 同一口径."""
    n = len(frames)
    half = window // 2
    bin_centers = torch.tensor([class_to_delta(c) for c in range(N_BINS)],
                               dtype=torch.float32)
    keys_prob = np.zeros((n, len(SCANNED_VKEYS)), np.float32)
    pointer = np.zeros((n, 2), np.float32)
    bins_pred = np.zeros((n, 2), np.int8)
    bins_conf = np.zeros((n, 2), np.float32)
    delta_head = np.zeros((n, 2), np.float32)

    for lo in range(0, n, batch_size):
        centers = range(lo, min(lo + batch_size, n))
        # 边界帧夹取窗口(复制端帧), 保证输出与 p0.jsonl 1:1
        idx = np.stack([np.clip(np.arange(c - half, c + half + 1), 0, n - 1)
                        for c in centers])
        clip = frames[idx]                                    # [B,T,H,W,3]
        x = torch.from_numpy(np.ascontiguousarray(clip)) \
            .permute(0, 1, 4, 2, 3).float() / 255.0
        b, t = x.shape[:2]
        if x.shape[-2:] != tuple(input_res):
            x = torch.nn.functional.interpolate(
                x.flatten(0, 1), size=tuple(input_res), mode="bilinear",
                align_corners=False).view(b, t, 3, *input_res)
        out = model(x.to(device))
        sl = slice(lo, lo + b)
        keys_prob[sl] = torch.sigmoid(out["keys"]).cpu().numpy()
        for j, head in enumerate(("bin_x", "bin_y")):
            probs = torch.softmax(out[head], dim=-1).cpu()
            # VPT 式解码: 期望而不是 argmax 的 bin 中心 —— 与 mouse_r2 同一口径
            pointer[sl, j] = (probs * bin_centers).sum(-1).numpy()
            cls = probs.argmax(-1)
            bins_pred[sl, j] = (cls - FOVEATED_MAX_BIN).numpy().astype(np.int8)
            bins_conf[sl, j] = probs.gather(-1, cls[:, None])[:, 0].numpy()
        delta_head[sl] = out["delta"].cpu().numpy() * delta_scale

    keys = (keys_prob >= threshold).astype(np.uint8)
    keys_conf = keys_prob * keys + (1.0 - keys_prob) * (1 - keys)
    return {
        "keys": keys,
        "keys_prob": keys_prob,
        "keys_conf": keys_conf.astype(np.float32),
        "pointer_from_bins": pointer,
        "pointer_bins_pred": bins_pred,
        "bins_conf": bins_conf,
        "delta_head": delta_head,
        "vocab_vks": np.asarray(SCANNED_VKEYS, np.int32),
        "edge_halo": np.int32(half),
    }


def _native_kb6(rec, space: ActionSpace) -> np.ndarray:
    """原生真值投到六通道(语义优先, 与 A1 臂同一口径), 供 agreement 读数."""
    acts = load_actions(rec.actions_path)
    sems = [json.loads(x).get("actions")
            for x in rec.actions_path.read_text().splitlines() if x.strip()]
    return np.stack([space.keyboard_multihot(s, a.keys)
                     for a, s in zip(acts, sems)])


def run(checkpoint: Path, src: Path, out: Path, *, device_name: str = "auto",
        batch_size: int = 64, threshold: float = 0.5,
        limit: int | None = None) -> dict:
    device = get_device(device_name)
    model, cfg = load_checkpoint(checkpoint, device)
    d = cfg["data"]
    window = d.get("window", 5)
    input_res = tuple(d.get("input_res", [224, 224]))
    delta_scale = d.get("delta_scale", 50.0)

    recs = scan_dataset(src)
    if not recs:
        raise SystemExit(f"{src} 下没有契约格式 chunk")
    recs = recs[: limit or None]
    metas = [r.meta for r in recs]
    # 治理: 只给过了四项与的具结语料打伪标签; fps 单一(伪标注在源帧率侧做,
    # 混帧率的根说明喂错了目录)
    gate_chunks(metas, allow_method_validation_only=False)
    gate_fps(metas)

    space = ActionSpace()
    tp = np.zeros(6)
    fp = np.zeros(6)
    fn = np.zeros(6)
    pos_native = np.zeros(6)
    pos_pseudo = np.zeros(6)
    conf_sum = 0.0
    frames_total = 0

    out.mkdir(parents=True, exist_ok=True)
    for rec in recs:
        frames = _load_frames(rec)
        n = min(len(frames), rec.n_frames)
        labels = label_chunk(model, frames[:n], window=window,
                             input_res=input_res, delta_scale=delta_scale,
                             batch_size=batch_size, device=device,
                             threshold=threshold)
        dst = out / rec.chunk_dir.parent.name / rec.chunk_dir.name
        dst.mkdir(parents=True, exist_ok=True)
        np.savez_compressed(dst / "pseudo.npz", **labels)
        meta = {
            "source_chunk": str(rec.chunk_dir),
            "sample_key": rec.meta.sample_key,
            "game": rec.meta.game,
            "annotation_source": "idm_pseudo_label",
            "checkpoint": str(checkpoint),
            "threshold": threshold,
            "frames": n,
            # 治理戳原样继承 —— no_publish 等约束跟着数据走, 不留在对话里
            "rights": rec.meta.rights,
            "first_party": rec.meta.first_party,
            "consent_id": rec.meta.consent_id,
            "is_trainable": rec.meta.is_trainable,
            "is_cutscene": rec.meta.is_cutscene,
            "tags": sorted({*rec.meta.tags, "pseudo_label"}),
        }
        (dst / "meta.json").write_text(json.dumps(meta, indent=1,
                                                  ensure_ascii=False) + "\n")

        kb6_pseudo = scanned_to_wm01(labels["keys"]).astype(bool)
        kb6_native = _native_kb6(rec, space)[:n].astype(bool)
        tp += (kb6_pseudo & kb6_native).sum(0)
        fp += (kb6_pseudo & ~kb6_native).sum(0)
        fn += (~kb6_pseudo & kb6_native).sum(0)
        pos_native += kb6_native.sum(0)
        pos_pseudo += kb6_pseudo.sum(0)
        conf_sum += float(labels["keys_conf"].mean()) * n
        frames_total += n

    f1 = (2 * tp / np.maximum(2 * tp + fp + fn, 1)).round(4)
    manifest = {
        "checkpoint": str(checkpoint),
        "source_root": str(src),
        "threshold": threshold,
        "chunks": len(recs),
        "frames": frames_total,
        "vocab_vks": list(SCANNED_VKEYS),
        "wm01_vocab_fingerprint": space.fingerprint,
        "mean_keys_conf": conf_sum / max(frames_total, 1),
        # 标注器质量读数: 六通道上 伪标签 vs 原生语义真值。这不是门, 是
        # 反方 4 那句「标注器太差」的量化答案, 与产物放在一起。
        "channel_agreement": [
            {"channel": name,
             "native_rate": round(float(pos_native[i]) / max(frames_total, 1), 4),
             "pseudo_rate": round(float(pos_pseudo[i]) / max(frames_total, 1), 4),
             "f1_vs_native": float(f1[i])}
            for i, name in enumerate(space.keyboard)],
    }
    (out / "manifest.json").write_text(
        json.dumps(manifest, indent=1, ensure_ascii=False) + "\n")
    return manifest


def main() -> None:
    ap = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--checkpoint", type=Path, required=True)
    ap.add_argument("--src", type=Path, required=True, help="chunk 契约根")
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("--device", default="auto")
    ap.add_argument("--batch-size", type=int, default=64)
    ap.add_argument("--threshold", type=float, default=0.5)
    ap.add_argument("--limit", type=int, default=None, help="只取前 N 个 chunk")
    a = ap.parse_args()
    manifest = run(a.checkpoint, a.src, a.out, device_name=a.device,
                   batch_size=a.batch_size, threshold=a.threshold,
                   limit=a.limit)
    print(json.dumps(manifest, indent=1, ensure_ascii=False))
    print(f"→ {a.out}")


if __name__ == "__main__":
    main()

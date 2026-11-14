"""Metrics for the Phase 0 acceptance gates.

  key_exact     fraction of frames whose predicted key SET matches exactly
  key_per_key   mean over keys of binary accuracy — the LENIENT reading, and
                the one VPT's 90.6% most likely refers to. Predicting "not
                pressed" for a rarely-used key is right most of the time, so
                this runs far above exact-set match on the same model and the
                two must never be compared to each other.
  key_macro_f1  mean per-key F1 — the honest one. It is the only one of the
                three that a model collapsing to the dominant key cannot
                inflate (round 1 measured key_exact 0.336 with macro F1 0.089,
                i.e. it had learned W and nothing else). This stays the
                gated metric.
  key_macro_f1_common
                the same average restricted to keys with support above
                `common_min_support` of frames. Defined BEFORE the
                single-game runs reported, as a diagnostic and never a
                gate, because macro F1 conflates two different failures:
                a head that collapsed to the dominant key, and a head that
                learned every key the data can teach while a long tail of
                keys with a few dozen positive frames stays at F1~0. Both
                land near 0.3; only the second is fixed by more data.
                Reading the two together separates them — collapse drags
                BOTH down, a long tail drags only the unrestricted one.
  key_support   positive-frame count per key, so the tail is visible rather
                than inferred.
  per-key F1    distribution-edge analysis — rare actions must not hide
                behind a good average (macro F1 reported alongside)
  bin_acc_*     foveated pointer-bin accuracy
  mouse_r2_*    R^2 of the pointer delta decoded from the bin head's softmax
                expectation (VPT-style; the raw-delta aux head is reported as
                delta_head_r2_*). Gate: min over axes of mouse_r2.
"""
from __future__ import annotations

import torch

from .keys import N_BINS, N_KEYS, SCANNED_VKEYS, class_to_delta


@torch.no_grad()
def run_eval(model, loader, device, max_batches: int | None = None,
             common_min_support: float = 0.01) -> dict:
    model.eval()
    bin_centers = torch.tensor([class_to_delta(c) for c in range(N_BINS)])
    key_correct = torch.zeros(N_KEYS)
    tp = torch.zeros(N_KEYS)
    fp = torch.zeros(N_KEYS)
    fn = torch.zeros(N_KEYS)
    n = 0
    exact = 0
    bin_hits_x = bin_hits_y = 0
    true_x, true_y, dec_x, dec_y = [], [], [], []
    head_x, head_y = [], []
    delta_scale = getattr(getattr(loader, "dataset", None), "delta_scale", 1.0)

    for bi, batch in enumerate(loader):
        if max_batches is not None and bi >= max_batches:
            break
        frames = batch["frames"].to(device)
        out = model(frames)

        keys_true = batch["keys"].bool()
        keys_pred = (out["keys"].sigmoid() > 0.5).cpu()
        exact += (keys_pred == keys_true).all(dim=1).sum().item()
        key_correct += (keys_pred == keys_true).sum(dim=0).float()
        tp += (keys_pred & keys_true).sum(dim=0).float()
        fp += (keys_pred & ~keys_true).sum(dim=0).float()
        fn += (~keys_pred & keys_true).sum(dim=0).float()

        bin_hits_x += (out["bin_x"].argmax(-1).cpu() == batch["bin_x"]).sum().item()
        bin_hits_y += (out["bin_y"].argmax(-1).cpu() == batch["bin_y"]).sum().item()

        # VPT-style decode: expected pixel delta under the bin softmax
        dec_x.append(out["bin_x"].softmax(-1).cpu() @ bin_centers)
        dec_y.append(out["bin_y"].softmax(-1).cpu() @ bin_centers)
        true_x.append(batch["delta"][:, 0] * delta_scale)
        true_y.append(batch["delta"][:, 1] * delta_scale)
        head_x.append(out["delta"][:, 0].cpu() * delta_scale)
        head_y.append(out["delta"][:, 1].cpu() * delta_scale)
        n += frames.shape[0]

    if n == 0:
        return {"n": 0}

    f1 = (2 * tp / (2 * tp + fp + fn).clamp(min=1e-9))
    support = tp + fn                 # positive frames per key
    seen = support > 0                # keys that actually occur in this split
    common = support >= common_min_support * n

    def r2(ys: list, yhs: list) -> float | None:
        y, yh = torch.cat(ys), torch.cat(yhs)
        ss_tot = ((y - y.mean()) ** 2).sum().item()
        if ss_tot < 1e-9:
            return None
        return 1.0 - ((y - yh) ** 2).sum().item() / ss_tot

    r2x, r2y = r2(true_x, dec_x), r2(true_y, dec_y)
    r2s = [v for v in (r2x, r2y) if v is not None]
    return {
        "n": n,
        "key_exact": exact / n,
        "key_per_key_acc": (key_correct / n).mean().item(),
        "key_macro_f1": f1[seen].mean().item() if seen.any() else None,
        "key_macro_f1_common": f1[common].mean().item() if common.any() else None,
        "key_common_min_support": common_min_support,
        "key_n_seen": int(seen.sum().item()),
        "key_n_common": int(common.sum().item()),
        "key_support": {str(vk): int(support[i].item())
                        for i, vk in enumerate(SCANNED_VKEYS) if seen[i]},
        "per_key_f1": {str(vk): round(f1[i].item(), 4)
                       for i, vk in enumerate(SCANNED_VKEYS) if seen[i]},
        "bin_acc_x": bin_hits_x / n,
        "bin_acc_y": bin_hits_y / n,
        "mouse_r2_x": r2x,
        "mouse_r2_y": r2y,
        "mouse_r2_min": min(r2s) if r2s else None,
        "delta_head_r2_x": r2(true_x, head_x),
        "delta_head_r2_y": r2(true_y, head_y),
    }

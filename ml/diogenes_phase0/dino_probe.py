"""Does the frozen trunk's feature space carry anything on game frames?

    python -m diogenes_phase0.dino_probe --video ../data_demo/bioshock_infinite_60s.mp4
    python -m diogenes_phase0.dino_probe --data data/idm_valorant --backend dinov2_vits14_reg

Both model lines rest on this and neither had measured it. Phase 0 freezes the
trunk and trains a <50M head on top, so whatever the trunk does not encode is
unreachable no matter how much data or compute the head gets — a candidate
explanation for heads that learn W and never learn the HUD-mediated keys.
Track A asks the same question as its Gate 0, before any LAM training: "UI is
OOD for DINOv2 — validate per-domain first; fail → light unsupervised finetune
on screen frames".

Every reading is reported against `pixels`, a baseline with no semantics at
all (the raw patch, flattened). A trunk that cannot beat flattened pixels on
this domain is not buying anything here, and that is the only form of this
question that has an answer rather than an opinion:

  region_auc      one linear direction, HUD-strip patches vs centre-of-screen
                  patches. Screen geometry labels them for free. High means
                  the space encodes the layout distinction the action labels
                  depend on.
  nn_consistency  for a frame pair, the fraction of patches whose nearest
                  neighbour in the other frame sits within `radius` patches.
                  Near-static UI should match near-identity; a space that
                  matches everything to everywhere cannot ground a pointer.
  outlier_norm    fraction of patch tokens whose norm exceeds 3x the median —
                  the documented artefact registers exist to remove. Compare
                  dinov2_vits14 with dinov2_vits14_reg on the same frames.

Zero training, one forward pass per frame; CPU is enough at these sizes.
"""
from __future__ import annotations

import argparse
import json
from pathlib import Path

import numpy as np


# --------------------------------------------------------------------------
# metrics — pure functions over [N, D] patch features, so they are testable
# without downloading any weights
# --------------------------------------------------------------------------
def region_masks(grid: int, *, hud_rows: int = 3, centre: int = 6
                 ) -> tuple[np.ndarray, np.ndarray]:
    """(hud, world) boolean masks over a flattened grid x grid patch layout."""
    hud = np.zeros((grid, grid), dtype=bool)
    hud[grid - hud_rows:, :] = True
    world = np.zeros((grid, grid), dtype=bool)
    lo = (grid - centre) // 2
    world[lo:lo + centre, lo:lo + centre] = True
    return hud.reshape(-1), world.reshape(-1)


def separability_auc(feats: np.ndarray, a: np.ndarray, b: np.ndarray) -> float:
    """AUC of the centroid-difference direction separating group a from b.

    Closed form on purpose: a trained probe would report how well *it* fits,
    and would need a train/test split to mean anything. The centroid direction
    is the weakest linear reading, so a high value here is not an artefact of
    fitting.
    """
    fa, fb = feats[a], feats[b]
    if len(fa) < 2 or len(fb) < 2:
        raise ValueError("both groups need at least two patches")
    w = fa.mean(0) - fb.mean(0)
    n = np.linalg.norm(w)
    if n < 1e-12:
        return 0.5
    sa, sb = fa @ (w / n), fb @ (w / n)
    # Mann-Whitney U / (na*nb) == P(score_a > score_b), ties at 0.5
    order = np.argsort(np.concatenate([sa, sb]), kind="stable")
    ranks = np.empty(order.size, dtype=np.float64)
    ranks[order] = np.arange(1, order.size + 1)
    # average ranks over ties so a constant feature reads as 0.5, not 1.0
    vals = np.concatenate([sa, sb])
    for v in np.unique(vals):
        m = vals == v
        if m.sum() > 1:
            ranks[m] = ranks[m].mean()
    ra = ranks[:sa.size].sum()
    return float((ra - sa.size * (sa.size + 1) / 2) / (sa.size * sb.size))


def nn_consistency(fa: np.ndarray, fb: np.ndarray, grid: int, *,
                   radius: int = 1) -> float:
    """Fraction of patches in `fa` whose cosine-NN in `fb` is within radius."""
    na = fa / np.maximum(1e-12, np.linalg.norm(fa, axis=1, keepdims=True))
    nb = fb / np.maximum(1e-12, np.linalg.norm(fb, axis=1, keepdims=True))
    nn = (na @ nb.T).argmax(1)
    idx = np.arange(fa.shape[0])
    dy = np.abs(nn // grid - idx // grid)
    dx = np.abs(nn % grid - idx % grid)
    return float((np.maximum(dx, dy) <= radius).mean())


def outlier_norm_fraction(feats: np.ndarray, *, k: float = 3.0) -> float:
    norms = np.linalg.norm(feats, axis=1)
    med = float(np.median(norms))
    if med < 1e-12:
        return 0.0
    return float((norms > k * med).mean())


# --------------------------------------------------------------------------
# feature extraction
# --------------------------------------------------------------------------
def pixel_patches(frame: np.ndarray, grid: int, patch: int) -> np.ndarray:
    """Flattened raw patches — the no-semantics baseline."""
    f = frame.astype(np.float32) / 255.0
    out = f.reshape(grid, patch, grid, patch, 3).transpose(0, 2, 1, 3, 4)
    return out.reshape(grid * grid, patch * patch * 3)


def dino_patches(frames: np.ndarray, backend: str) -> np.ndarray:
    """[F,H,W,3] uint8 -> [F, grid*grid, D] normalized patch tokens."""
    import torch

    mean = torch.tensor([0.485, 0.456, 0.406]).view(1, 3, 1, 1)
    std = torch.tensor([0.229, 0.224, 0.225]).view(1, 3, 1, 1)
    enc = torch.hub.load("facebookresearch/dinov2", backend)
    enc.eval()
    x = torch.from_numpy(frames).permute(0, 3, 1, 2).float() / 255.0
    x = (x - mean) / std
    with torch.no_grad():
        out = []
        for i in range(0, x.shape[0], 8):
            out.append(enc.forward_features(x[i:i + 8])["x_norm_patchtokens"])
    return torch.cat(out).numpy()


def load_frames(*, video: str | None, data: str | None, n: int, res: int
                ) -> np.ndarray:
    """n frames at res x res, from an mp4 or from contract chunks."""
    import cv2

    frames: list[np.ndarray] = []
    if video:
        cap = cv2.VideoCapture(video)
        total = int(cap.get(cv2.CAP_PROP_FRAME_COUNT)) or n
        step = max(1, total // n)
        i = 0
        while len(frames) < n:
            ok, fr = cap.read()
            if not ok:
                break
            if i % step == 0:
                frames.append(cv2.resize(cv2.cvtColor(fr, cv2.COLOR_BGR2RGB),
                                         (res, res)))
            i += 1
        cap.release()
    else:
        from .data.dataset import _load_frames, scan_dataset
        for rec in scan_dataset(data):
            arr = _load_frames(rec)
            for fr in arr[::10]:
                frames.append(cv2.resize(fr, (res, res)))
                if len(frames) >= n:
                    break
            if len(frames) >= n:
                break
    if len(frames) < 2:
        raise SystemExit("need at least two frames")
    return np.stack(frames)


def probe(frames: np.ndarray, *, backend: str, patch: int = 14,
          pair_gap: int = 1, radius: int = 1) -> dict:
    grid = frames.shape[1] // patch
    if backend == "pixels":
        feats = np.stack([pixel_patches(f, grid, patch) for f in frames])
    else:
        feats = dino_patches(frames, backend)
    hud, world = region_masks(grid)
    aucs, cons, outl = [], [], []
    for i in range(len(frames)):
        aucs.append(separability_auc(feats[i], hud, world))
        outl.append(outlier_norm_fraction(feats[i]))
        j = i + pair_gap
        if j < len(frames):
            cons.append(nn_consistency(feats[i], feats[j], grid, radius=radius))
    return {
        "backend": backend,
        "frames": int(len(frames)),
        "grid": int(grid),
        "dim": int(feats.shape[-1]),
        "region_auc": float(np.mean(aucs)),
        "region_auc_p10": float(np.percentile(aucs, 10)),
        "nn_consistency": float(np.mean(cons)) if cons else None,
        "outlier_norm_fraction": float(np.mean(outl)),
    }


def main() -> None:
    ap = argparse.ArgumentParser()
    src = ap.add_mutually_exclusive_group(required=True)
    src.add_argument("--video")
    src.add_argument("--data")
    ap.add_argument("--backend", default="dinov2_vits14",
                    help="pixels | dinov2_vits14 | dinov2_vits14_reg")
    ap.add_argument("--frames", type=int, default=120)
    ap.add_argument("--res", type=int, default=224, help="multiple of 14")
    ap.add_argument("--pair-gap", type=int, default=1)
    ap.add_argument("--radius", type=int, default=1)
    ap.add_argument("--also-pixels", action="store_true",
                    help="run the pixel baseline on the same frames")
    ap.add_argument("--out")
    args = ap.parse_args()

    frames = load_frames(video=args.video, data=args.data, n=args.frames,
                         res=args.res)
    results = [probe(frames, backend=args.backend, pair_gap=args.pair_gap,
                     radius=args.radius)]
    if args.also_pixels and args.backend != "pixels":
        results.append(probe(frames, backend="pixels", pair_gap=args.pair_gap,
                             radius=args.radius))
    print(json.dumps(results, indent=2))
    if len(results) == 2:
        t, p = results
        print(f"region_auc {t['region_auc']:.3f} vs pixels "
              f"{p['region_auc']:.3f}; nn_consistency "
              f"{t['nn_consistency']:.3f} vs {p['nn_consistency']:.3f}")
        if t["region_auc"] <= p["region_auc"] + 0.02 and \
                (t["nn_consistency"] or 0) <= (p["nn_consistency"] or 0) + 0.02:
            print("the trunk does not beat flattened pixels on this domain — "
                  "Track A Gate 0 reads FAIL, and a frozen trunk is the wrong "
                  "assumption for Phase 0 here")
    if args.out:
        Path(args.out).write_text(json.dumps(results, indent=2))


if __name__ == "__main__":
    main()

"""Is a label recoverable from the pixels at all?

    python -m diogenes_phase0.label_signal --data data/idm_minecraft

A head that scores zero has two very different explanations: it could not
learn, or there was nothing to learn. Telling them apart before buying more
data is cheap, because the question is about the labels and the frames, not
about any model.

Two readings, and they can disagree:

  presence_ratio  mean visual change on frames where the action fires,
                  over frames where it does not. Answers "did anything
                  happen" — a ratio near 1 means the label does not
                  correspond to anything visible.
  magnitude_r     correlation between the action's magnitude and visual
                  change. Answers "how much" — what an R^2 gate measures.

On gaming-500-hours minecraft the pointer labels scored presence 4.44x but
magnitude r=+0.115, while round 1's key labels scored presence 7.3x and
r=0.747 on the same measure. So pointer presence is learnable there and
pointer magnitude largely is not, which is what an R^2 near zero was
reporting. The dataset reconstructs mouse deltas by differencing absolute
cursor positions, a lossy proxy under pointer lock; VPT's R^2=0.97 came
from engine-reported deltas.

Mean absolute frame difference is a crude stand-in for camera motion —
optical flow would resolve direction this cannot — so magnitude_r is a
LOWER bound on recoverable signal, and a low value argues for a better
measurement before it argues for abandoning the target.
"""
from __future__ import annotations

import argparse
import json
from pathlib import Path

import numpy as np

from .contract import load_actions
from .keys import MOUSE_CLAMP
from .data.dataset import _load_frames, scan_dataset


def visual_change(frames: np.ndarray) -> np.ndarray:
    """Mean absolute inter-frame difference, one value per transition."""
    g = frames.mean(axis=3).astype(np.float32)
    return np.abs(np.diff(g, axis=0)).mean(axis=(1, 2))


def _magnitude(a, target: str) -> float | None:
    """L1 magnitude of one target, or None when this frame cannot express it.

    The three pointer targets are different labels of the same motion, and on
    first-party data they can disagree sharply: `pointer` saturates at the
    converter's +-200 clamp exactly on the frames that move most, `raw` does
    not, and `angle` is the only sensitivity-independent form. Measuring them
    on the same frames is what says whether an R^2 gate was scored against a
    capped label or against a genuinely weak one.
    """
    if target == "keys":
        return float(len(a.keys))
    if target == "pointer":
        return abs(a.pointer_dx) + abs(a.pointer_dy)
    if target == "raw":
        return abs(a.raw_dx) + abs(a.raw_dy)
    if target == "angle":
        if not a.view_scale_valid or a.look_yaw_delta_rad is None \
                or a.look_pitch_delta_rad is None:
            return None
        return abs(a.look_yaw_delta_rad) + abs(a.look_pitch_delta_rad)
    raise ValueError(f"unknown target {target!r}")


def _select(root: str | Path, max_chunks: int, spread: bool):
    """Chunks to walk. `spread` strides the sorted list instead of taking the
    head of it.

    scan_dataset sorts by path, so a corpus laid out as one directory per
    session hands the first N chunks all from the first session. A per-session
    playstyle then reads as a property of the corpus: the published
    gaming-500 numbers (presence 4.44x, r=0.115) were measured head-first and
    carry that bias. `spread` is opt-in rather than the default so those
    numbers stay reproducible; new measurements should pass it.
    """
    recs = scan_dataset(root)
    if not spread or len(recs) <= max_chunks:
        return recs[:max_chunks]
    return recs[::max(1, len(recs) // max_chunks)][:max_chunks]


def _pairs(root: str | Path, max_chunks: int, target: str,
           spread: bool = False):
    vis: list[float] = []
    mag: list[float] = []
    skipped = 0
    for rec in _select(root, max_chunks, spread):
        frames = _load_frames(rec)
        actions = load_actions(rec.actions_path)
        n = min(len(frames), len(actions))
        if n < 2:
            continue
        d = visual_change(frames[:n])
        for t in range(1, n):
            m = _magnitude(actions[t], target)
            if m is None:
                skipped += 1
                continue
            vis.append(float(d[t - 1]))
            mag.append(m)
    return np.asarray(vis), np.asarray(mag), skipped


def label_signal(root: str | Path, *, max_chunks: int = 14,
                 target: str = "pointer", spread: bool = False) -> dict:
    vis, mag, skipped = _pairs(root, max_chunks, target, spread)
    if vis.size < 2:
        raise SystemExit(
            f"not enough frames under {root} for target={target}"
            + (f" ({skipped} frames lack this label)" if skipped else "")
        )
    fires = mag > 0
    quiet = ~fires
    on = float(vis[fires].mean()) if fires.any() else 0.0
    off = float(vis[quiet].mean()) if quiet.any() else 0.0
    r = None
    if vis.std() > 0 and mag.std() > 0:
        r = float(np.corrcoef(vis, mag)[0, 1])
    # off == 0 with on > 0 is perfect separation, the strongest possible
    # presence signal — reporting it as "unknown" alongside the genuinely
    # uninformative case would invert the reading
    if off > 1e-9:
        ratio, separable = on / off, None
    elif on > 1e-9:
        ratio, separable = None, True
    else:
        ratio, separable = None, False
    return {
        "target": target,
        "spread_sampling": bool(spread),
        "frames": int(vis.size),
        "frames_without_label": int(skipped),
        "fire_fraction": float(fires.mean()),
        "visual_change_on": on,
        "visual_change_off": off,
        "presence_ratio": ratio,
        "presence_separable": separable,
        "magnitude_r": r,
    }


def clamp_loss(root: str | Path, *, max_chunks: int = 14,
               spread: bool = False) -> dict:
    """How much of the pointer label the +-200 clamp removes, frame by frame.

    `pointerDx/Dy` is clamped by the converter and clamped again by the loader,
    so on flick-heavy footage the regression target is a capped version of the
    motion. This walks the same frames twice — once as `pointer`, once as
    `raw` — and reports where they part company. It costs no GPU and it decides
    whether a failing mouse R^2 is a model result or a label ceiling.

    Chunks without `pointerRawDx/Dy` (data_demo, gaming-500) fall back to
    pointer, so `differs_fraction` is 0 there by construction — that is the
    honest answer for those corpora, not a bug.
    """
    ptr_x: list[float] = []
    raw_x: list[float] = []
    for rec in _select(root, max_chunks, spread):
        for a in load_actions(rec.actions_path):
            ptr_x.append(abs(a.pointer_dx) + abs(a.pointer_dy))
            raw_x.append(abs(a.raw_dx) + abs(a.raw_dy))
    if not ptr_x:
        raise SystemExit(f"no action frames under {root}")
    p, r = np.asarray(ptr_x), np.asarray(raw_x)
    differs = np.abs(p - r) > 1e-9
    at_clamp = p >= MOUSE_CLAMP - 1e-9      # either axis alone can reach it
    return {
        "frames": int(p.size),
        "differs_fraction": float(differs.mean()),
        "at_clamp_fraction": float(at_clamp.mean()),
        "pointer_p50": float(np.percentile(p, 50)),
        "pointer_p99": float(np.percentile(p, 99)),
        "pointer_max": float(p.max()),
        "raw_p50": float(np.percentile(r, 50)),
        "raw_p99": float(np.percentile(r, 99)),
        "raw_max": float(r.max()),
        "raw_over_pointer_p99": float(np.percentile(r, 99) /
                                      max(1e-9, np.percentile(p, 99))),
    }


def _signed_delta(a, source: str) -> tuple[float, float] | None:
    if source == "pointer":
        return a.pointer_dx, a.pointer_dy
    if source == "raw":
        return a.raw_dx, a.raw_dy
    if source == "angle":
        if not a.view_scale_valid or a.look_yaw_delta_rad is None \
                or a.look_pitch_delta_rad is None:
            return None
        return a.look_yaw_delta_rad, a.look_pitch_delta_rad
    raise ValueError(f"unknown source {source!r}")


def flow_signal(root: str | Path, *, max_chunks: int = 14,
                source: str = "pointer", min_delta: float = 3.0,
                downscale: int = 4, spread: bool = False,
                offset: int = 0) -> dict:
    """Signed, direction-aware coupling — what `magnitude_r` should have been.

    `magnitude_r` correlates |dx|+|dy| against the mean absolute frame
    difference. That is unsigned and it counts *any* visual change, so a muzzle
    flash reads like camera motion and a leftward flick is indistinguishable
    from a rightward one. Measured across seven mouse-driven FPS titles in D2E
    it produced physically impossible readings — Rainbow Six presence 0.943 and
    VALORANT 0.859, i.e. frames *with* pointer motion showing *less* visual
    change than frames without. An instrument that reports that cannot be used
    to decide whether a label is recoverable.

    This correlates the SIGNED per-axis displacement against the SIGNED delta,
    using the estimator the world-model line already relies on for its E1a
    view-following probe (`diogenes_wm.eval.probes`: Farneback dense flow,
    median displacement). Deliberately the same estimator rather than a second
    implementation — two of them would drift and the two lines' numbers would
    stop being comparable. On the same BioShock sample the WM line measured
    phase correlation at r≈0 where Farneback got r≈0.39, so the estimator
    choice is not cosmetic.

    Frames whose |delta| is under `min_delta` on both axes are skipped: at rest
    the flow is noise and correlating noise against zero manufactures a number.

    `offset` picks which label belongs to the transition `t -> t+1`. The WM
    line's E1a probe assumes `actions[t]` (offset 0); the D2E adapter and the
    DataMiner contract define a frame's action as the input that *produced* it,
    i.e. `actions[t+1]` (offset 1). A one-frame mismatch does not fail loudly —
    it just eats the correlation — so try both and let the corpus say which
    convention it uses instead of assuming one.
    """
    from diogenes_wm.eval.probes import _flow_shift, phase_shift

    est: list[tuple[float, float]] = []
    lab: list[tuple[float, float]] = []
    used = "phase"
    skipped_no_label = 0
    for rec in _select(root, max_chunks, spread):
        frames = _load_frames(rec)
        actions = load_actions(rec.actions_path)
        n = min(len(frames), len(actions))
        if n < 2:
            continue
        g = frames[:n, ::downscale, ::downscale].astype(np.float32).mean(axis=-1)
        for t in range(n - 1):
            li = t + offset
            if li < 0 or li >= n:
                continue
            d = _signed_delta(actions[li], source)
            if d is None:
                skipped_no_label += 1
                continue
            dx_l, dy_l = d
            if abs(dx_l) < min_delta and abs(dy_l) < min_delta:
                continue
            shift = _flow_shift(g[t], g[t + 1])
            if shift is None:
                shift = phase_shift(g[t], g[t + 1])
            else:
                used = "flow"
            est.append((shift[0] * downscale, shift[1] * downscale))
            lab.append((dx_l, dy_l))

    if len(est) < 3:
        raise SystemExit(
            f"only {len(est)} usable frame pair(s) under {root} for "
            f"source={source} (min_delta={min_delta})"
            + (f"; {skipped_no_label} frames lack this label" if skipped_no_label else ""))
    e, l = np.asarray(est), np.asarray(lab)

    def _r(x, y):
        if x.std() == 0 or y.std() == 0:
            return 0.0
        return float(np.corrcoef(x, y)[0, 1])

    r_dx, r_dy = _r(e[:, 0], l[:, 0]), _r(e[:, 1], l[:, 1])
    return {
        "source": source,
        "estimator": used,
        "spread_sampling": bool(spread),
        "pairs": int(len(est)),
        "frames_without_label": int(skipped_no_label),
        "min_delta": min_delta,
        "offset": int(offset),
        "r_dx": r_dx,
        "r_dy": r_dy,
        "coupling": max(abs(r_dx), abs(r_dy)),
    }


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--data", required=True)
    ap.add_argument("--target", choices=["pointer", "raw", "angle", "keys"],
                    default="pointer")
    ap.add_argument("--max-chunks", type=int, default=14)
    ap.add_argument("--spread", action="store_true",
                    help="stride the chunk list so the sample crosses sessions")
    ap.add_argument("--clamp-report", action="store_true",
                    help="compare pointer vs raw on the same frames instead")
    ap.add_argument("--flow", action="store_true",
                    help="signed per-axis coupling via dense optical flow "
                         "(the instrument magnitude_r should have been); "
                         "--target selects the delta source")
    ap.add_argument("--min-delta", type=float, default=3.0)
    ap.add_argument("--offsets", type=int, nargs="+", default=[0, 1],
                    help="label-to-transition alignments to try; best wins")
    args = ap.parse_args()
    if args.flow:
        src = args.target if args.target in ("pointer", "raw", "angle") else "pointer"
        reps = [flow_signal(args.data, max_chunks=args.max_chunks, source=src,
                            min_delta=args.min_delta, spread=args.spread,
                            offset=o) for o in args.offsets]
        rep = max(reps, key=lambda r: r["coupling"])
        print(json.dumps(reps if len(reps) > 1 else rep, indent=2))
        if len(reps) > 1:
            print("best alignment: offset=%d coupling=%.3f (other %.3f) — "
                  "a one-frame mismatch is silent, it only eats the number"
                  % (rep["offset"], rep["coupling"],
                     min(r["coupling"] for r in reps)))
        if rep["coupling"] < 0.3:
            print("even a direction-aware estimator finds little coupling — "
                  "this is a statement about the corpus, not the model")
        return
    if args.clamp_report:
        rep = clamp_loss(args.data, max_chunks=args.max_chunks,
                         spread=args.spread)
        print(json.dumps(rep, indent=2))
        if rep["differs_fraction"] < 1e-9:
            print("no pointerRawDx/Dy in this corpus — pointer IS raw here, "
                  "so the clamp cannot be what caps the target")
        else:
            print(f"the clamp bites on {100 * rep['differs_fraction']:.2f}% of "
                  f"frames; raw p99 is "
                  f"{rep['raw_over_pointer_p99']:.2f}x the clamped p99")
        return
    res = label_signal(args.data, max_chunks=args.max_chunks,
                       target=args.target, spread=args.spread)
    print(json.dumps(res, indent=2))
    pr, r = res["presence_ratio"], res["magnitude_r"]
    if res.get("presence_separable") is True:
        print("presence separates perfectly — nothing fires without a "
              "visible change")
    elif res.get("presence_separable") is False:
        print("no visual change anywhere — the probe saw a static clip")
    elif pr is not None and pr < 1.5:
        print("presence is NOT visible in the pixels — the label is noise here")
    elif r is not None and r < 0.3:
        print("presence is visible but magnitude is weakly determined — "
              "an R^2 gate on this target is measuring the dataset, "
              "not the model")


if __name__ == "__main__":
    main()

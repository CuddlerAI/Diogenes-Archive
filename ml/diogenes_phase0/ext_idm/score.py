"""Score Generalist-IDM-1B, the trivial baselines and our checkpoint on ONE window set.

    python -m diogenes_phase0.ext_idm.score --chunks chunks_val.txt \
        --pred-dir preds_val --config configs/idm_d2e_zeroshot.yaml \
        --checkpoint runs/idm-d2e-zeroshot-20260801-042206/checkpoint.pt \
        --out reports/val.json

⚠️ **Who has seen what differs between the two splits, in opposite
directions.** Generalist-IDM-1B trained on D2E, which is where our evaluation
set comes from — but arXiv:2510.05684 §5.1 names Battlefield 6 as one of two
games held OUT of its training, and our `val` split is a single Battlefield 6
recording. Rainbow Six, our `zero_shot` holdout, is inside its training corpus.
So:

  * `val` — BF6 unseen by the external model, and a *different recording of a
    game our head trained on*. This split favours us.
  * `zero_shot` — R6 held out from our head entirely, and training data for
    theirs. This split favours them.

The two therefore bracket the comparison, and the columns must never be
averaged into one number. Read each against the trivial baselines scored on
the same windows, not against each other.

Everything else here exists to make those readings survive scrutiny:

**One window set.** Every system is scored on the same chunks, the same window
centers (`window`/`stride` from the training config) and the same labels. Our
checkpoint is re-scored here rather than quoted from `gates_report.json`,
because a full-split number and a subset number are different measurements —
gates.py's docstring records a run where reading two such numbers as a
comparison inverted the conclusion.

**Identical metric code.** The aggregate for our checkpoint is computed twice:
once by this file's per-chunk accumulator and once by the stock
`evaluate.run_eval`. They must agree bit for bit; `selfcheck` in the output
records that they did. Without it, "口径与 gates.py 一致" would be a claim
rather than a check.

**State, not events.** The model emits key press/release transitions; our
labels are the key SET held at a frame. Recovering a set from transitions
needs an initial condition, and a chunk is a 4-second window cut out of a
longer recording — a key held down since before it started has no press event
anywhere in the model's input. Reading that as "not pressed" would score the
model against a state it was never shown, and it hits W hardest, the key held
almost continuously in these recordings (the same trap d2e.py's `build_rows`
handles for the ground truth by seeding from a `keyboard/state` snapshot).
So `--seed gt` grants the model the ground-truth key set at frame 0 — 12 bits
per 120 frames — and `--seed none` grants nothing. Both are reported. The
trivial baselines and our checkpoint get no such seed; they do not need one,
because neither infers state from transitions.

**Alignment is swept, not assumed.** Upstream shifts predicted actions 100 ms
earlier at output. `--shifts-ms` reports the metric at several shifts; the
headline shift must be chosen on `train`-split chunks and passed as
`--primary-shift-ms`, never picked from the val/zero_shot table.
"""
from __future__ import annotations

import argparse
import json
from collections import Counter
from pathlib import Path

import torch
import yaml
from torch.utils.data import DataLoader

from ..contract import load_actions
from ..data.dataset import WindowDataset
from ..evaluate import run_eval
from ..keys import MOUSE_CLAMP, N_KEYS, VK_TO_INDEX, keys_to_multihot
from .metrics import (VOCAB, KeyStats, _states_chunk_mode, _states_run_mode,
                      bootstrap, centers, load_chunk_records, paired_bootstrap,
                      pearson_xy, replay_events)
from ..models.idm import build_model
from ..train import get_device

# ---------------------------------------------------------------------------
# systems
# ---------------------------------------------------------------------------


def score_external(records, pred_dir: Path, window: int, stride: int, *,
                   shift_ms: float, seed_mode: str, episode_mode: str = "chunk"
                   ) -> tuple[list[KeyStats], list[KeyStats], dict]:
    fn = _states_run_mode if episode_mode == "run" else _states_chunk_mode
    states, blobs = fn(records, pred_dir, shift_ms, seed_mode)

    per_chunk, n_events = [], 0
    pred_pts: list[tuple[float, float]] = []
    true_pts: list[tuple[float, float]] = []
    # Each chunk is an isolated 4-second episode, while upstream decodes
    # 2-minute segments — so the model starts every chunk with no history.
    # If that boundary is what is costing it, the last third of a chunk must
    # score better than the first. Reported so the objection is answered with
    # a measurement instead of an argument.
    by_third = [KeyStats(), KeyStats(), KeyStats()]
    # How much of the model's keyboard output our 12-key projection throws
    # away. G-IDM predicts the full keyboard; `SCANNED_VKEYS` models 12 codes,
    # and research/28 §6.1 E7 measured 18.9% of real key-press events falling
    # outside it — 43.0% on Rainbow Six alone. Those predictions cannot earn
    # credit here, so this fraction is the size of the handicap, and every
    # G-IDM number below is a LOWER bound on what it can actually do.
    n_kb = n_kb_dropped = 0
    for blob in blobs:
        n_events += len(blob["events"])
        for e in blob["events"]:
            if e.get("topic") == "keyboard" and e.get("vk") is not None:
                n_kb += 1
                n_kb_dropped += int(int(e["vk"]) not in VOCAB)
    for rec in records:
        actions = load_actions(rec.actions_path)
        n = min(rec.n_frames, len(actions))
        pred_keys, pred_delta = states[str(rec.chunk_dir)]
        st = KeyStats()
        for t in centers(n, window, stride):
            st.add(pred_keys[t], keys_to_multihot(actions[t].keys))
            by_third[min(2, 3 * t // n)].add(pred_keys[t],
                                             keys_to_multihot(actions[t].keys))
            # clamp the prediction the way the converter clamped the label,
            # so the correlation is not read across two different scales
            pred_pts.append((max(-MOUSE_CLAMP, min(MOUSE_CLAMP, pred_delta[t][0])),
                             max(-MOUSE_CLAMP, min(MOUSE_CLAMP, pred_delta[t][1]))))
            true_pts.append((actions[t].pointer_dx, actions[t].pointer_dy))
        per_chunk.append(st)
    info = {"chunks_scored": len(per_chunk), "events_total": n_events,
            "episodes": len(blobs),
            "pred_keyboard_events": n_kb,
            "pred_keyboard_out_of_vocab_frac": (round(n_kb_dropped / n_kb, 4)
                                                if n_kb else None),
            "pointer_pearson_r": pearson_xy(pred_pts, true_pts)}
    return per_chunk, by_third, info


def score_constant(records, window: int, stride: int,
                   pred_fn) -> tuple[list[KeyStats], list[KeyStats]]:
    per_chunk = []
    by_third = [KeyStats(), KeyStats(), KeyStats()]
    for rec in records:
        actions = load_actions(rec.actions_path)
        n = min(rec.n_frames, len(actions))
        st = KeyStats()
        for t in centers(n, window, stride):
            true = keys_to_multihot(actions[t].keys)
            st.add(pred_fn(), true)
            by_third[min(2, 3 * t // n)].add(pred_fn(), true)
        per_chunk.append(st)
    return per_chunk, by_third


def score_seed_persist(records, window: int, stride: int
                       ) -> tuple[list[KeyStats], list[KeyStats]]:
    """Hold the ground-truth frame-0 key set for the whole chunk.

    The control for `--seed gt`, and it is not optional. The seed hands the
    model 12 bits of truth at frame 0; for a key that is HELD across the chunk
    — W while running, RMB while aiming down sights — carrying those bits
    forward unchanged already scores well, with no model involved. Without
    this row, a seeded model's advantage over the always-pressed baseline
    cannot be told apart from the seed itself. (Measured: on Rainbow Six,
    G-IDM's RMB F1 falls 0.651 -> 0.062 when the seed is withheld.)
    """
    per_chunk = []
    by_third = [KeyStats(), KeyStats(), KeyStats()]
    for rec in records:
        actions = load_actions(rec.actions_path)
        n = min(rec.n_frames, len(actions))
        const = keys_to_multihot(actions[0].keys)
        st = KeyStats()
        for t in centers(n, window, stride):
            true = keys_to_multihot(actions[t].keys)
            st.add(const, true)
            by_third[min(2, 3 * t // n)].add(const, true)
        per_chunk.append(st)
    return per_chunk, by_third


def majority_set(records, window: int, stride: int) -> list[int]:
    """Most frequent ground-truth key set on THIS window set.

    An oracle-flavoured baseline — it reads the split's own label marginal —
    and that is the point: it is the strongest any constant predictor can be,
    which is the right thing to hold `key_exact` against. `key_exact` rewards
    predicting the empty set on idle frames, so a head can beat a naive
    baseline on it without having learned anything.
    """
    c = Counter()
    for rec in records:
        actions = load_actions(rec.actions_path)
        n = min(rec.n_frames, len(actions))
        for t in centers(n, window, stride):
            c[tuple(keys_to_multihot(actions[t].keys))] += 1
    return list(c.most_common(1)[0][0])


def score_checkpoint(records, cfg: dict, ckpt_path: Path, device,
                     batch_size: int) -> tuple[list[KeyStats], list[KeyStats],
                                               dict, dict]:
    """Our head on the same windows — per-chunk stats AND the stock run_eval."""
    ckpt = torch.load(ckpt_path, map_location=device, weights_only=False)
    ccfg = ckpt["config"]
    model = build_model(ccfg).to(device)
    model.load_state_dict(ckpt["model"])
    model.eval()
    d = ccfg["data"]
    window, stride = d.get("window", 5), d.get("stride", 1)
    # The external model's windows were laid out from the YAML; ours come from
    # the checkpoint (eval_checkpoint.py's rule — a geometry mismatch produces
    # a meaningless number rather than an error). If the two disagree the two
    # systems are not on one window set and nothing below is a comparison.
    want = (cfg["data"].get("window", 5), cfg["data"].get("stride", 1))
    if (window, stride) != want:
        raise SystemExit(
            f"checkpoint was trained with window/stride {(window, stride)} but "
            f"{want} came from the config used to lay out the external model's "
            f"windows — the systems would be scored on different frames")

    per_chunk = []
    by_third = [KeyStats(), KeyStats(), KeyStats()]
    with torch.no_grad():
        for rec in records:
            ds = WindowDataset(
                [rec], window=window, stride=stride,
                input_res=tuple(d.get("input_res", [96, 96])),
                delta_scale=d.get("delta_scale", 50.0),
                contract_mode=d.get("contract_mode"),
                allow_method_validation_only=d.get("allow_method_validation_only"),
                delta_source=d.get("delta_source", "pointer"),
                delta_clamp=d.get("delta_clamp"),
                expected_fps=d.get("expected_fps"))
            st = KeyStats()
            # WindowDataset emits centers in order with shuffle=False, so the
            # running index recovers each sample's position inside the chunk
            cs = centers(rec.n_frames, window, stride)
            k = 0
            for batch in DataLoader(ds, batch_size=batch_size, shuffle=False):
                out = model(batch["frames"].to(device))
                pred = (out["keys"].sigmoid() > 0.5).cpu().int().tolist()
                true = batch["keys"].bool().int().tolist()
                for p, t in zip(pred, true):
                    st.add(p, t)
                    by_third[min(2, 3 * cs[k] // rec.n_frames)].add(p, t)
                    k += 1
            per_chunk.append(st)

    # same weights, same records, stock metric code — must agree
    ds_all = WindowDataset(
        records, window=window, stride=stride,
        input_res=tuple(d.get("input_res", [96, 96])),
        delta_scale=d.get("delta_scale", 50.0),
        contract_mode=d.get("contract_mode"),
        allow_method_validation_only=d.get("allow_method_validation_only"),
        delta_source=d.get("delta_source", "pointer"),
        delta_clamp=d.get("delta_clamp"),
        expected_fps=d.get("expected_fps"))
    stock = run_eval(model, DataLoader(ds_all, batch_size=batch_size,
                                       shuffle=False), device)
    return per_chunk, by_third, stock, {"step": ckpt.get("step"), "window": window,
                              "stride": stride}


def _finish(per_chunk: list[KeyStats], by_third: list[KeyStats] | None = None,
            extra: dict | None = None) -> dict:
    rep = KeyStats.merge(per_chunk).report()
    rep.update(bootstrap(per_chunk))
    rep["n_chunks"] = len(per_chunk)
    if by_third:
        # every third averaged over the SAME key set — the split's own. Without
        # restrict_to the three denominators differ and the position effect is
        # read off a denominator artifact (it inverted this table once).
        keys = KeyStats.merge(per_chunk).seen_keys()
        rep["by_chunk_third"] = [t.report(restrict_to=keys).get("key_macro_f1")
                                 for t in by_third]
    if extra:
        rep.update(extra)
    return rep


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--chunks", required=True, type=Path)
    ap.add_argument("--pred-dir", required=True, type=Path)
    ap.add_argument("--config", required=True, type=Path)
    ap.add_argument("--checkpoint", type=Path, default=None)
    ap.add_argument("--label", default="")
    ap.add_argument("--device", default="auto")
    ap.add_argument("--batch-size", type=int, default=48)
    ap.add_argument("--shifts-ms", default="0,50,100,150,200")
    ap.add_argument("--primary-shift-ms", type=float, default=100.0)
    ap.add_argument("--episode-mode", default="chunk", choices=["chunk", "run"],
                    help="'chunk' = one 4s episode per chunk; 'run' = one "
                         "episode per contiguous run (research/30 D-3-b)")
    ap.add_argument("--out", required=True, type=Path)
    args = ap.parse_args()

    cfg = yaml.safe_load(args.config.read_text())
    d = cfg["data"]
    window, stride = d.get("window", 5), d.get("stride", 1)
    chunks = [Path(l.strip()) for l in args.chunks.read_text().splitlines()
              if l.strip()]
    records = load_chunk_records(chunks)

    # One window set for every system. A chunk the external run did not finish
    # is dropped for ALL of them — scoring our head on 40 chunks and the
    # external model on the 37 it got through would put the difference between
    # two frame sets inside a number reported as a model difference.
    from .predict import out_name
    if args.episode_mode == "run":
        covered = set()
        for p in args.pred_dir.glob("run__*.json"):
            covered |= {m["chunk"] for m in json.loads(p.read_text())["members"]}
        have = [r for r in records if str(r.chunk_dir) in covered]
    else:
        have = [r for r in records
                if (args.pred_dir / out_name(r.chunk_dir)).exists()]
    dropped = len(records) - len(have)
    if dropped:
        print(f"dropping {dropped} chunk(s) with no prediction under "
              f"{args.pred_dir} — all systems scored on the remaining "
              f"{len(have)}")
    records = have
    if not records:
        raise SystemExit(f"no predictions under {args.pred_dir}")
    n_windows = sum(len(centers(r.n_frames, window, stride)) for r in records)
    print(f"{len(records)} chunk(s), {n_windows} window(s), "
          f"window={window} stride={stride}")

    systems: dict[str, dict] = {}
    # per-chunk stats kept alongside the aggregates: the contrasts below are
    # paired on these chunks, and a paired CI needs the same draw for both
    raw: dict[str, list[KeyStats]] = {}

    # --- external model, over the alignment sweep x seed mode
    shifts = [float(s) for s in args.shifts_ms.split(",") if s.strip()]
    for seed_mode in ("gt", "none"):
        for sh in shifts:
            per_chunk, thirds, info = score_external(
                records, args.pred_dir, window, stride, shift_ms=sh,
                seed_mode=seed_mode, episode_mode=args.episode_mode)
            if not per_chunk:
                print(f"no predictions found under {args.pred_dir}")
                break
            key = f"gidm1b.seed={seed_mode}.shift={sh:g}ms"
            systems[key] = _finish(per_chunk, thirds, info)
            raw[key] = per_chunk
            print(f"  {key}: macro_f1={systems[key]['key_macro_f1']:.4f} "
                  f"exact={systems[key]['key_exact']:.4f}")

    # --- trivial baselines
    all_on = [1] * N_KEYS
    raw["trivial.always_on"], thirds_on = score_constant(
        records, window, stride, lambda: list(all_on))
    systems["trivial.always_on"] = _finish(raw["trivial.always_on"], thirds_on)
    all_off = [0] * N_KEYS
    raw["trivial.all_off"], thirds_off = score_constant(
        records, window, stride, lambda: list(all_off))
    systems["trivial.all_off"] = _finish(raw["trivial.all_off"], thirds_off)
    raw["trivial.persist_seed"], thirds_seed = score_seed_persist(
        records, window, stride)
    systems["trivial.persist_seed"] = _finish(raw["trivial.persist_seed"],
                                              thirds_seed)
    maj = majority_set(records, window, stride)
    raw["trivial.majority_set"], thirds_maj = score_constant(
        records, window, stride, lambda: list(maj))
    systems["trivial.majority_set"] = _finish(
        raw["trivial.majority_set"], thirds_maj,
        {"set": sorted(vk for vk, i in VK_TO_INDEX.items() if maj[i])})

    # --- our checkpoint, re-scored here
    selfcheck = {}
    if args.checkpoint:
        device = get_device(args.device)
        per_chunk, thirds_ours, stock, meta = score_checkpoint(
            records, cfg, args.checkpoint, device, args.batch_size)
        systems["ours"] = _finish(per_chunk, thirds_ours, meta)
        raw["ours"] = per_chunk
        mine = systems["ours"]
        selfcheck = {
            "run_eval_key_exact": stock["key_exact"],
            "run_eval_key_macro_f1": stock["key_macro_f1"],
            "delta_key_exact": abs(stock["key_exact"] - mine["key_exact"]),
            "delta_key_macro_f1": abs(stock["key_macro_f1"] - mine["key_macro_f1"]),
            "delta_key_macro_f1_common": abs(stock["key_macro_f1_common"]
                                             - mine["key_macro_f1_common"]),
        }
        # run_eval accumulates in float32 tensors and this file in Python
        # floats, so the macro F1 differs in the 9th decimal. 1e-6 is loose
        # enough for that and still orders of magnitude tighter than any real
        # 口径 divergence (a different window set, a different key vocabulary
        # or a different exact-match rule all move it by >1e-3).
        selfcheck["tolerance"] = 1e-6
        selfcheck["matches_stock_run_eval"] = all(
            selfcheck[k] < 1e-6 for k in
            ("delta_key_exact", "delta_key_macro_f1", "delta_key_macro_f1_common"))
        print(f"selfcheck vs evaluate.run_eval: {selfcheck}")
        if not selfcheck["matches_stock_run_eval"]:
            raise SystemExit(
                "this file's accumulator disagrees with evaluate.run_eval — "
                "every number here would be off-口径; refusing to write a report")

    # --- the contrasts D-3 actually asks about, paired on these chunks
    primary = f"gidm1b.seed=gt.shift={args.primary_shift_ms:g}ms"
    contrasts = {}
    for a, b in ((primary, "trivial.always_on"),
                 # the control that isolates what the frame-0 seed alone buys
                 (primary, "trivial.persist_seed"),
                 ("ours", "trivial.always_on"),
                 (primary, "ours"),
                 (f"gidm1b.seed=none.shift={args.primary_shift_ms:g}ms",
                  "trivial.always_on")):
        if a in raw and b in raw:
            contrasts[f"{a} - {b}"] = paired_bootstrap(raw[a], raw[b])
    for k, v in contrasts.items():
        if v:
            print(f"  Δ {k}: {v['delta_key_macro_f1']:+.4f} "
                  f"CI95 {v['delta_key_macro_f1_ci95']} "
                  f"{'(excludes 0)' if v['macro_f1_ci_excludes_zero'] else '(includes 0)'}")

    report = {
        "label": args.label or args.chunks.stem,
        "chunks_file": str(args.chunks), "pred_dir": str(args.pred_dir),
        "config": str(args.config), "checkpoint": str(args.checkpoint or ""),
        "n_chunks": len(records), "n_windows": n_windows,
        "chunks_dropped_no_prediction": dropped,
        "window": window, "stride": stride,
        "games": sorted({r.meta.game for r in records}),
        "sessions": sorted({r.chunk_dir.parent.name for r in records}),
        "primary_shift_ms": args.primary_shift_ms,
        "episode_mode": args.episode_mode,
        "selfcheck": selfcheck,
        "contrasts": contrasts,
        "systems": systems,
    }
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(json.dumps(report, indent=2))
    print(f"wrote {args.out}")


if __name__ == "__main__":
    main()

"""Evaluate a VL model on the game-screen bench + acceptance gates.

Metrics (doc 16 §1.1 / §4.4 评估集):
  json_valid_rate   responses parsing into the observation schema
  game_top1         normalized game-title match (free GT from the bench)
  event_class_acc   vs teacher labels (when labels are supplied)
  highlight_mae     calibration proxy vs teacher labels
  latency_p50/p95   seconds per frame (the <1s target is architectural, but
                    regressions show up here first)

Hard gates come from the config (real target: game_top1 >= 0.90 per the
"精度高" requirement; smoke config leaves gates informational).

    python -m diogenes_vl.evaluate --config configs/vl_smoke.yaml \
        --adapter runs/vl-sft-*/adapter --split val
"""
from __future__ import annotations

import argparse
import json
import time
from pathlib import Path

import torch
import yaml

from diogenes_phase0.gates import Gate, all_passed, render_markdown, write_report

from .analyze_labels import GAMEPLAY_EVENTS
from .bench import load_manifest
from .schema import (PROMPT, SchemaError, game_fuzzy_matches, game_matches,
                     is_substantive, observation_fingerprint,
                     parse_observation)


def load_model(model_name: str, adapter: str | None, device: torch.device,
               merge_adapter: bool = True):
    """Load base + adapter for inference.

    The adapter is merged by default. An unmerged PeftModel keeps the low-rank
    factors as separate modules and evaluates `base(x) + B(A(x))` for every
    targeted projection at every decoded token — pure overhead at inference,
    since merging produces mathematically identical outputs. Round 8 measured
    ~8 tok/s for a 4B model on an L4 whose memory bandwidth allows roughly
    37, so the decode path is worth not handicapping.

    Pass merge_adapter=False only to compare against the unmerged path.
    """
    from transformers import AutoModelForImageTextToText, AutoProcessor
    processor = AutoProcessor.from_pretrained(adapter or model_name)
    dtype = torch.bfloat16 if device.type == "cuda" else torch.float32
    model = AutoModelForImageTextToText.from_pretrained(model_name, dtype=dtype)
    if adapter:
        from peft import PeftModel
        model = PeftModel.from_pretrained(model, adapter)
        if merge_adapter:
            model = model.merge_and_unload()
    model.to(device)
    model.eval()
    return processor, model


def _parse(resp: str) -> tuple[dict | None, str]:
    try:
        return parse_observation(resp), resp
    except SchemaError:
        return None, resp


@torch.no_grad()
def generate_observations(processor, model, images, device,
                          max_new_tokens: int = 192) -> list[tuple[dict | None, str]]:
    """Greedy-decode a batch of frames.

    Decoding dominates eval wall clock (the latency probe measured prefill at
    8 ms against ~5.7 s of decode), and a batch amortizes the per-step weight
    read that makes decoding memory-bound. Single-frame latency is unchanged —
    this buys back iteration time, not product latency.

    Decoder-only generation needs LEFT padding: pad on the right and every
    shorter prompt gets its continuation started after pad tokens, which
    silently produces different text than the same frame decoded alone. The
    equality of batched and unbatched output is pinned by a test for exactly
    that reason.
    """
    messages = [{"role": "user", "content": [
        {"type": "image"}, {"type": "text", "text": PROMPT}]}]
    texts, imgs = [], []
    for image in images:
        texts.append(processor.apply_chat_template(
            messages, tokenize=False, add_generation_prompt=True))
        imgs.append(image)

    tok = getattr(processor, "tokenizer", None)
    prev_side = getattr(tok, "padding_side", None) if tok is not None else None
    if tok is not None:
        tok.padding_side = "left"
    try:
        inputs = processor(text=texts, images=imgs, return_tensors="pt",
                           padding=True).to(device)
        out = model.generate(**inputs, max_new_tokens=max_new_tokens,
                             do_sample=False)
    finally:
        if tok is not None and prev_side is not None:
            tok.padding_side = prev_side

    prompt_len = inputs["input_ids"].shape[1]
    return [_parse(processor.decode(row[prompt_len:], skip_special_tokens=True))
            for row in out]


@torch.no_grad()
def generate_observation(processor, model, image, device,
                         max_new_tokens: int = 192) -> tuple[dict | None, str]:
    return generate_observations(processor, model, [image], device,
                                 max_new_tokens)[0]


def run_eval(cfg: dict, adapter: str | None, split: str, device: torch.device,
             limit: int | None = None, batch_size: int = 1) -> dict:
    from PIL import Image
    d = cfg["data"]
    bench_dir = Path(d["bench_dir"])
    rows = [r for r in load_manifest(bench_dir) if r["split"] == split]
    if limit:
        # deterministic shuffle before limiting — manifest is game-sorted, so a
        # plain head-slice would evaluate only the alphabetically-first games
        import hashlib
        rows.sort(key=lambda r: hashlib.sha1(r["id"].encode()).hexdigest())
        rows = rows[:limit]
    labels = {}
    trained_games: set[str] = set()
    train_split = d.get("train_split")
    for p in d.get("labels", []):
        if not Path(p).exists():
            continue  # baseline runs have no teacher labels yet
        for line in open(p):
            if line.strip():
                r = json.loads(line)
                labels[r["id"]] = r["observation"]
                if train_split is None or r.get("split") == train_split:
                    trained_games.add(r["game"].lower())

    processor, model = load_model(cfg["model"]["name"], adapter, device)

    n = valid = game_hit = fuzzy_hit = ev_total = ev_hit = tg_total = tg_hit = 0
    seen_total = seen_hit = unseen_total = unseen_hit = 0
    play_total = play_hit = 0
    substantive = 0
    fingerprints: set[tuple] = set()
    predictions: list[dict] = []
    hl_err: list[float] = []
    lat: list[float] = []
    samples: list[dict] = []
    mnt = cfg.get("eval", {}).get("max_new_tokens", 192)
    for i in range(0, len(rows), max(1, batch_size)):
        chunk = rows[i:i + max(1, batch_size)]
        imgs = [Image.open(bench_dir / r["image"]).convert("RGB") for r in chunk]
        t0 = time.time()
        outs = generate_observations(processor, model, imgs, device, mnt)
        # per-frame latency is the batch's cost divided by its size; a batched
        # number reported as if single-frame would understate product latency
        per = (time.time() - t0) / len(chunk)
        for row, (obs, raw) in zip(chunk, outs):
            lat.append(per)
            n += 1
            if len(samples) < 5:
                samples.append({"id": row["id"], "raw": raw[:400]})
            ref = labels.get(row["id"])
            predictions.append({"id": row["id"], "gt_game": row["game"],
                                "pred": obs, "raw": None if obs else raw[:300],
                                "ref_event_class": (ref or {}).get("event_class")})
            if obs is None:
                continue
            valid += 1
            substantive += is_substantive(obs)
            fingerprints.add(observation_fingerprint(obs))
            hit = game_matches(obs["game"], row["game"])
            game_hit += hit
            if game_fuzzy_matches(obs["game"], row["game"]):
                fuzzy_hit += 1
            # game_top1 restricted to frames the REFERENCE labels call gameplay.
            # Session GT is unreliable on menu/launcher/browser frames, so the
            # all-frame number rewards guessing there; this one measures actual
            # recognition. The filter uses the reference frame classification,
            # never the student's own output.
            if ref and ref.get("event_class") in GAMEPLAY_EVENTS:
                play_total += 1
                play_hit += hit
            if ref:
                ev_total += 1
                if obs["event_class"] == ref["event_class"]:
                    ev_hit += 1
                hl_err.append(abs(obs["highlight_score"] - ref["highlight_score"]))
                # student-vs-teacher game agreement: the distillation metric —
                # the teacher saw the SAME frame, so session-label noise
                # (browser/loading frames) doesn't contaminate it
                tg_total += 1
                agree = game_matches(obs["game"], ref["game"]) or (
                    obs["game"].lower() == ref["game"].lower())
                tg_hit += bool(agree)
                # split the same metric by whether this game was in the SFT set —
                # the direct in-domain vs zero-shot answer
                if row["game"].lower() in trained_games:
                    seen_total += 1
                    seen_hit += bool(agree)
                else:
                    unseen_total += 1
                    unseen_hit += bool(agree)

    lat.sort()
    # a valid-but-empty shell repeated every frame would score 1.0 above
    substantive_rate = substantive / valid if valid else None
    distinct_ratio = len(fingerprints) / valid if valid else None
    return {
        "n": n,
        "json_valid_rate": valid / n if n else None,
        "content_substantive_rate": substantive_rate,
        "distinct_observation_ratio": distinct_ratio,
        "game_top1": game_hit / n if n else None,
        "game_top1_gameplay": play_hit / play_total if play_total else None,
        "n_gameplay_frames": play_total,
        "game_fuzzy": fuzzy_hit / n if n else None,
        "game_teacher_agree": tg_hit / tg_total if tg_total else None,
        "game_agree_seen": seen_hit / seen_total if seen_total else None,
        "game_agree_unseen": unseen_hit / unseen_total if unseen_total else None,
        "n_seen": seen_total,
        "n_unseen": unseen_total,
        "event_class_acc": ev_hit / ev_total if ev_total else None,
        "highlight_mae": sum(hl_err) / len(hl_err) if hl_err else None,
        "latency_p50": lat[len(lat) // 2] if lat else None,
        "latency_p95": lat[int(len(lat) * 0.95)] if lat else None,
        "samples": samples,
        "predictions": predictions,
    }


def check_vl_gates(report: dict, gate_cfg: dict,
                   split: str = "val") -> list[Gate]:
    """Gate names carry the split they were computed on.

    A hardcoded "val." prefix would label a holdout score as a val score, so
    the one number meant to be read once — after iterating against val —
    would be indistinguishable from the number we tuned on.
    """
    gates: list[Gate] = []

    def hard(name, value, key):
        th = gate_cfg.get(key)
        ok = None if (value is None or th is None) else bool(value >= th)
        gates.append(Gate(f"{split}.{name}", value, th, ok, "hard"))

    def info(name, value):
        gates.append(Gate(f"{split}.{name}", value, None, None, "info"))

    hard("json_valid_rate", report.get("json_valid_rate"), "json_valid_min")
    hard("game_teacher_agree", report.get("game_teacher_agree"), "game_agree_min")
    hard("event_class_acc", report.get("event_class_acc"), "event_acc_min")
    hard("game_top1_gameplay", report.get("game_top1_gameplay"),
         "game_top1_gameplay_min")
    hard("game_top1", report.get("game_top1"), "game_top1_min")
    hard("content_substantive_rate", report.get("content_substantive_rate"),
         "content_substantive_min")
    info("distinct_observation_ratio", report.get("distinct_observation_ratio"))
    info("game_agree_seen", report.get("game_agree_seen"))
    info("game_agree_unseen", report.get("game_agree_unseen"))
    info("game_fuzzy", report.get("game_fuzzy"))
    info("latency_p50_s", report.get("latency_p50"))
    info("highlight_mae", report.get("highlight_mae"))
    return gates


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--config", required=True)
    ap.add_argument("--adapter", default=None)
    ap.add_argument("--split", default="val")
    ap.add_argument("--limit", type=int, default=None)
    ap.add_argument("--batch-size", type=int, default=1,
                    help="frames per generate() call; speeds up eval, "
                         "does not change per-frame product latency")
    ap.add_argument("--device", default="auto")
    ap.add_argument("--check-gates", action="store_true")
    args = ap.parse_args()
    cfg = yaml.safe_load(Path(args.config).read_text())
    device = torch.device(args.device if args.device != "auto"
                          else ("cuda" if torch.cuda.is_available() else "cpu"))

    report = run_eval(cfg, args.adapter, args.split, device, args.limit,
                      batch_size=args.batch_size)
    gates = check_vl_gates(report, cfg.get("gates", {}), args.split)

    run_dir = Path(cfg.get("output", {}).get("dir", "runs")) / \
        f"{cfg.get('run_name', 'vl')}-eval-{time.strftime('%Y%m%d-%H%M%S')}"
    reports = {"val": {k: v for k, v in report.items()
                       if k not in ("samples", "predictions")}}
    write_report(run_dir, gates, reports)
    (run_dir / "samples.json").write_text(json.dumps(report["samples"], indent=2))
    (run_dir / "predictions.json").write_text(
        json.dumps(report.get("predictions", []), indent=2))
    print(render_markdown(gates, reports))
    print(f"run dir: {run_dir}")
    if args.check_gates and not all_passed(gates):
        raise SystemExit(2)


if __name__ == "__main__":
    main()

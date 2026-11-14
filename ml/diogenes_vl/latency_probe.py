"""Where do the seconds actually go?

    python -m diogenes_vl.latency_probe --config configs/vl_r6.yaml \
        --adapter runs/vl-r6-*/adapter

Round 8 reported latency_p50 = 11.3 s/frame while the median response was
only ~93 tokens — about 8 tok/s. An L4 has ~300 GB/s of memory bandwidth and
a 4B bf16 model is ~8 GB of weights, so memory-bound decoding allows roughly
37 tok/s. Being 4x under that is a configuration question, not a model-size
question, and answering it by argument is how a team ends up buying
distillation to fix a flag.

Separates the four candidates by measuring them, each against the same
frames:

  prefill       image preprocessing + prompt encoding, once per frame
  decode        the autoregressive loop — expected to dominate
  adapter       unmerged LoRA evaluates base(x)+B(A(x)) per projection per
                token; merging is mathematically identical and free
  output length tokens actually emitted, since latency is linear in it and
                the schema — not the model — decides how many

Reports tok/s alongside the wall clock: a rate is comparable across frames
and machines, a per-frame second count is not.
"""
from __future__ import annotations

import argparse
import json
import statistics
import time
from pathlib import Path

import torch
import yaml

from .bench import load_manifest
from .evaluate import load_model
from .schema import PROMPT


def _encode(processor, image, device):
    messages = [{"role": "user", "content": [
        {"type": "image"}, {"type": "text", "text": PROMPT}]}]
    text = processor.apply_chat_template(messages, tokenize=False,
                                         add_generation_prompt=True)
    return processor(text=[text], images=[image], return_tensors="pt").to(device)


@torch.no_grad()
def time_one(processor, model, image, device, max_new_tokens: int) -> dict:
    torch.cuda.synchronize() if device.type == "cuda" else None
    t0 = time.perf_counter()
    inputs = _encode(processor, image, device)
    torch.cuda.synchronize() if device.type == "cuda" else None
    t1 = time.perf_counter()
    out = model.generate(**inputs, max_new_tokens=max_new_tokens,
                         do_sample=False)
    torch.cuda.synchronize() if device.type == "cuda" else None
    t2 = time.perf_counter()
    n_new = int(out.shape[1] - inputs["input_ids"].shape[1])
    return {
        "prefill_s": t1 - t0,
        "decode_s": t2 - t1,
        "total_s": t2 - t0,
        "new_tokens": n_new,
        "tok_per_s": n_new / (t2 - t1) if t2 > t1 else None,
        "prompt_tokens": int(inputs["input_ids"].shape[1]),
    }


def run(cfg: dict, adapter: str | None, device: torch.device, *,
        n_frames: int, max_new_tokens: int, merge: bool) -> dict:
    from PIL import Image
    bench_dir = Path(cfg["data"]["bench_dir"])
    rows = [r for r in load_manifest(bench_dir) if r["split"] == "val"][:n_frames]
    if not rows:
        raise SystemExit(f"no val rows under {bench_dir}")
    processor, model = load_model(cfg["model"]["name"], adapter, device,
                                  merge_adapter=merge)
    samples = []
    for i, row in enumerate(rows):
        img = Image.open(bench_dir / row["image"]).convert("RGB")
        s = time_one(processor, model, img, device, max_new_tokens)
        if i:            # drop the first: it pays warmup and kernel autotune
            samples.append(s)
    del model
    if device.type == "cuda":
        torch.cuda.empty_cache()

    def med(k):
        vals = [s[k] for s in samples if s[k] is not None]
        return statistics.median(vals) if vals else None

    return {
        "merged_adapter": merge,
        "n_timed": len(samples),
        "prompt_tokens": med("prompt_tokens"),
        "new_tokens_median": med("new_tokens"),
        "prefill_s_median": med("prefill_s"),
        "decode_s_median": med("decode_s"),
        "total_s_median": med("total_s"),
        "tok_per_s_median": med("tok_per_s"),
    }


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--config", required=True)
    ap.add_argument("--adapter", default=None)
    ap.add_argument("--frames", type=int, default=9)
    ap.add_argument("--max-new-tokens", type=int, default=192)
    ap.add_argument("--device", default="auto")
    ap.add_argument("--out", default=None)
    args = ap.parse_args()

    cfg = yaml.safe_load(Path(args.config).read_text())
    device = torch.device(args.device if args.device != "auto"
                          else ("cuda" if torch.cuda.is_available() else "cpu"))

    arms = [True, False] if args.adapter else [True]
    reports = [run(cfg, args.adapter, device, n_frames=args.frames,
                   max_new_tokens=args.max_new_tokens, merge=m) for m in arms]

    print(f"{'arm':<10}{'tok/s':>9}{'decode s':>10}{'prefill s':>11}"
          f"{'new tok':>9}{'total s':>9}")
    for r in reports:
        arm = "merged" if r["merged_adapter"] else "unmerged"
        print(f"{arm:<10}{r['tok_per_s_median'] or 0:>9.1f}"
              f"{r['decode_s_median'] or 0:>10.2f}{r['prefill_s_median'] or 0:>11.3f}"
              f"{r['new_tokens_median'] or 0:>9.0f}{r['total_s_median'] or 0:>9.2f}")
    if len(reports) == 2 and reports[0]["tok_per_s_median"]:
        speedup = reports[0]["tok_per_s_median"] / reports[1]["tok_per_s_median"]
        print(f"\nmerging the adapter: {speedup:.2f}x decode throughput")
    r = reports[0]
    if r["decode_s_median"] and r["total_s_median"]:
        share = r["decode_s_median"] / r["total_s_median"]
        print(f"decode is {share:.0%} of wall clock — "
              f"{'output length is the lever' if share > 0.8 else 'prefill matters too'}")
    if args.out:
        Path(args.out).write_text(json.dumps(reports, indent=2))


if __name__ == "__main__":
    main()

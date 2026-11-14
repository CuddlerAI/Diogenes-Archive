"""LoRA SFT: teach a small VL base to emit the observation JSON.

    python -m diogenes_vl.sft --config configs/vl_smoke.yaml

Doc 16 §4.4: LoRA SFT first (this file), GRPO refinement only after the SFT
base is strong — the GRPO stage is a later hook, not implemented here.

Notes:
- batch size is per-optimizer-step via gradient accumulation of single
  samples (VLM image-token lengths vary; bs=1 keeps the harness portable
  across model families).
- loss is masked to the assistant span (prompt tokens = -100).
"""
from __future__ import annotations

import argparse
import json
import math
import time
from pathlib import Path

import torch
import yaml


class EvalOnlyLabelError(RuntimeError):
    pass


def load_rows(paths: list[str], split: str | None) -> list[dict]:
    """Load SFT rows, refusing evaluation-only labels.

    Labels from closed frontier models (Gemini/Claude) are benchmark-only per
    doc 16 §4.4 — their terms forbid using outputs to improve competing
    models. They carry eval_only=true; training on them is a licensing
    violation, so this is a hard failure rather than a filter.
    """
    rows = []
    for p in paths:
        for line in open(p):
            if not line.strip():
                continue
            row = json.loads(line)
            if row.get("eval_only"):
                raise EvalOnlyLabelError(
                    f"{p} contains eval_only labels (teacher="
                    f"{row.get('teacher')}). These may benchmark label quality "
                    "but must never become training data.")
            if split is None or row.get("split") == split:
                rows.append(row)
    return rows


class SftSample:
    def __init__(self, processor, bench_dir: Path, row: dict, prompt: str):
        from PIL import Image
        self.image = Image.open(bench_dir / row["image"]).convert("RGB")
        self.answer = json.dumps(row["observation"])
        self.prompt = prompt
        self.processor = processor

    def encode(self) -> dict:
        """Returns a ready-to-forward batch of size 1.

        NB: Qwen-VL processors return input_ids/attention_mask WITH a batch
        dim but pixel_values ([num_patches, C]) and image_grid_thw
        ([num_images, 3]) WITHOUT one — pass them through untouched.
        """
        messages = [{"role": "user", "content": [
            {"type": "image"}, {"type": "text", "text": self.prompt}]}]
        prompt_text = self.processor.apply_chat_template(
            messages, tokenize=False, add_generation_prompt=True)
        full_text = prompt_text + self.answer
        prompt_len = self.processor(text=[prompt_text], images=[self.image],
                                    return_tensors="pt")["input_ids"].shape[1]
        enc = dict(self.processor(text=[full_text], images=[self.image],
                                  return_tensors="pt"))
        labels = enc["input_ids"].clone()
        labels[:, :prompt_len] = -100
        enc["labels"] = labels
        return enc


def build_model(cfg: dict, device: torch.device):
    from peft import LoraConfig, get_peft_model
    from transformers import AutoModelForImageTextToText, AutoProcessor

    m = cfg["model"]
    processor = AutoProcessor.from_pretrained(m["name"])
    dtype = torch.bfloat16 if device.type == "cuda" else torch.float32
    model = AutoModelForImageTextToText.from_pretrained(m["name"], dtype=dtype)
    lora = LoraConfig(
        r=m.get("lora_r", 16),
        lora_alpha=m.get("lora_alpha", 32),
        lora_dropout=m.get("lora_dropout", 0.05),
        target_modules=m.get("lora_targets",
                             ["q_proj", "k_proj", "v_proj", "o_proj"]),
        task_type="CAUSAL_LM",
    )
    model = get_peft_model(model, lora)
    model.to(device)
    model.print_trainable_parameters()
    return processor, model


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--config", required=True)
    ap.add_argument("--device", default="auto")
    ap.add_argument("--max-steps", type=int, default=None)
    args = ap.parse_args()
    cfg = yaml.safe_load(Path(args.config).read_text())
    torch.manual_seed(cfg.get("seed", 0))
    device = torch.device(args.device if args.device != "auto"
                          else ("cuda" if torch.cuda.is_available() else "cpu"))

    d = cfg["data"]
    bench_dir = Path(d["bench_dir"])
    rows = load_rows(d["labels"], d.get("train_split"))
    if not rows:
        raise SystemExit("no SFT rows — run bench + teacher first")

    processor, model = build_model(cfg, device)
    t = cfg["train"]
    max_steps = args.max_steps or t.get("max_steps", 100)
    accum = t.get("grad_accum", 4)
    opt = torch.optim.AdamW((p for p in model.parameters() if p.requires_grad),
                            lr=t.get("lr", 1e-4),
                            weight_decay=t.get("weight_decay", 0.0))
    sched = torch.optim.lr_scheduler.LambdaLR(
        opt, lambda s: 0.5 * (1 + math.cos(math.pi * min(1.0, s / max_steps))))

    run_dir = Path(cfg.get("output", {}).get("dir", "runs")) / \
        f"{cfg.get('run_name', 'vl-sft')}-{time.strftime('%Y%m%d-%H%M%S')}"
    run_dir.mkdir(parents=True, exist_ok=True)
    (run_dir / "config.yaml").write_text(yaml.safe_dump(cfg))
    log = open(run_dir / "train_log.jsonl", "a")

    model.train()
    step = 0
    t0 = time.time()
    losses: list[float] = []
    while step < max_steps:
        perm = torch.randperm(len(rows))
        for gi in range(0, len(perm) - accum + 1, accum):
            opt.zero_grad(set_to_none=True)
            micro_losses = []
            for j in range(accum):
                row = rows[perm[gi + j]]
                enc = SftSample(processor, bench_dir, row,
                                cfg.get("prompt_override") or _prompt()).encode()
                batch = {k: v.to(device) for k, v in enc.items()}
                out = model(**batch)
                (out.loss / accum).backward()
                micro_losses.append(out.loss.item())
            torch.nn.utils.clip_grad_norm_(
                (p for p in model.parameters() if p.requires_grad), 1.0)
            opt.step()
            sched.step()
            step += 1
            losses.append(sum(micro_losses) / len(micro_losses))
            if step % t.get("log_every", 10) == 0 or step == 1:
                rec = {"step": step, "loss": round(losses[-1], 4),
                       "lr": sched.get_last_lr()[0],
                       "sec": round(time.time() - t0, 1)}
                print(json.dumps(rec))
                log.write(json.dumps(rec) + "\n")
                log.flush()
            if step >= max_steps:
                break

    model.save_pretrained(run_dir / "adapter")
    processor.save_pretrained(run_dir / "adapter")
    first = sum(losses[:5]) / max(1, len(losses[:5]))
    last = sum(losses[-5:]) / max(1, len(losses[-5:]))
    summary = {"steps": step, "loss_first5": round(first, 4),
               "loss_last5": round(last, 4)}
    (run_dir / "summary.json").write_text(json.dumps(summary, indent=2))
    print(json.dumps(summary))
    print(f"adapter saved to {run_dir / 'adapter'}")


def _prompt() -> str:
    from .schema import PROMPT
    return PROMPT


if __name__ == "__main__":
    main()

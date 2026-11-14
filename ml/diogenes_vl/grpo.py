"""GRPO refinement on top of the SFT adapter (doc 16 §4.4 final stage).

GRPO = Group Relative Policy Optimization: for each prompt, sample K
completions, score each with the programmatic reward (reward.py), and push
the policy toward the above-average completions and away from the
below-average ones — the group's own mean is the baseline, so there is no
value network.

Starts from the round-6 SFT adapter (already clears every SFT gate) and
continues training the SAME LoRA parameters. The reward is programmatic and
consumes no teacher output, so this stage has no licensing exposure.

    python -m diogenes_vl.grpo --config configs/vl_grpo.yaml \
        --init-adapter runs/vl-r6-*/adapter

Design notes:
- bs=1 prompt per step (K sampled completions), grad-accum over prompts, to
  stay portable across VLM families like the SFT trainer.
- No explicit KL term (would need a second frozen 4B copy, tight on one L4).
  Drift is bounded instead by starting AT the SFT policy, a low LR, few
  steps, and group-normalized advantages. Safe here because the reward is
  grounded in correctness (schema + game/event/highlight vs references), not
  a hackable proxy — "maximizing reward" means "getting the answer right".
  A KL-to-reference penalty is the natural next addition on a bigger GPU.
- a step whose group has ~zero reward variance is skipped (no signal).
"""
from __future__ import annotations

import argparse
import json
import time
from pathlib import Path

import torch
import yaml

from .reward import compute_reward, group_advantages
from .schema import PROMPT
from .sft import load_rows


def build_policy(cfg: dict, init_adapter: str, device: torch.device):
    """Load base + SFT adapter as the trainable policy (the LoRA params from
    round 6 continue training)."""
    from peft import PeftModel
    from transformers import AutoModelForImageTextToText, AutoProcessor

    name = cfg["model"]["name"]
    processor = AutoProcessor.from_pretrained(init_adapter)
    dtype = torch.bfloat16 if device.type == "cuda" else torch.float32

    base = AutoModelForImageTextToText.from_pretrained(name, dtype=dtype)
    policy = PeftModel.from_pretrained(base, init_adapter, is_trainable=True)
    policy.to(device)
    # trade compute for memory: the backward over ~1k image tokens is the
    # peak, and GRPO runs K of them per prompt
    if hasattr(policy, "gradient_checkpointing_enable"):
        policy.gradient_checkpointing_enable()
        if hasattr(policy, "enable_input_require_grads"):
            policy.enable_input_require_grads()
    return processor, policy


_MM_PASSTHROUGH = ("pixel_values", "image_grid_thw", "pixel_values_videos",
                   "video_grid_thw")


@torch.no_grad()
def _sample_group(processor, policy, image, device, k: int, max_new: int,
                  temperature: float):
    """Sample k completions for one image; returns (samples, inputs, plen).

    Each sample is (text, full_ids, gen_len) where gen_len excludes the
    right-padding `generate` adds to sequences that stopped early — those
    pad tokens must not enter the log-prob sum.

    Generation runs with dropout OFF and gradient checkpointing OFF:
    - dropout during generation destroys the output outright (measured:
      rewards 0.99/0.98/1.00 in eval mode vs 0.00/0.00/0.00 in train mode,
      the latter emitting 384 tokens of gibberish), which silently zeroed
      every reward and left GRPO with no gradient signal at all;
    - gradient checkpointing forces use_cache=False, so each token
      recomputes the whole stack — sampling one group took ~46 s.
    """
    messages = [{"role": "user", "content": [
        {"type": "image"}, {"type": "text", "text": PROMPT}]}]
    text = processor.apply_chat_template(messages, tokenize=False,
                                         add_generation_prompt=True)
    inputs = processor(text=[text], images=[image], return_tensors="pt").to(device)
    plen = inputs["input_ids"].shape[1]

    was_training = policy.training
    gc_on = bool(getattr(policy, "is_gradient_checkpointing", False))
    policy.eval()
    if gc_on and hasattr(policy, "gradient_checkpointing_disable"):
        policy.gradient_checkpointing_disable()
    try:
        out = policy.generate(**inputs, do_sample=True, temperature=temperature,
                              top_p=0.95, num_return_sequences=k,
                              max_new_tokens=max_new)
    finally:
        if gc_on and hasattr(policy, "gradient_checkpointing_enable"):
            policy.gradient_checkpointing_enable()
        if was_training:
            policy.train()

    tok = getattr(processor, "tokenizer", processor)
    pad_id = getattr(tok, "pad_token_id", None)
    samples = []
    for seq in out:
        gen = seq[plen:]
        if pad_id is not None:
            keep = (gen != pad_id).nonzero().flatten()
            gen_len = int(keep[-1].item()) + 1 if len(keep) else 0
        else:
            gen_len = len(gen)
        resp = processor.decode(gen, skip_special_tokens=True)
        samples.append((resp, seq, gen_len))
    return samples, inputs, plen


def _mm_kwargs(inputs: dict, total_len: int) -> dict:
    """Vision kwargs for a forward over prompt+completion.

    `mm_token_type_ids` marks image vs text tokens and must span the WHOLE
    sequence — the generated continuation is all text (0). Omitting it makes
    the model refuse to compute multimodal RoPE.
    """
    mm = {k: v for k, v in inputs.items() if k in _MM_PASSTHROUGH}
    mm_ids = inputs.get("mm_token_type_ids")
    if mm_ids is not None:
        n_extra = total_len - mm_ids.shape[1]
        if n_extra > 0:
            pad = torch.zeros((mm_ids.shape[0], n_extra), dtype=mm_ids.dtype,
                              device=mm_ids.device)
            mm_ids = torch.cat([mm_ids, pad], dim=1)
        mm["mm_token_type_ids"] = mm_ids[:, :total_len]
    return mm


def accumulate_policy_gradient(policy, inputs: dict, samples, advs,
                               plen: int, scale: float) -> float:
    """Backward each completion separately, accumulating into .grad.

    Mathematically identical to summing the per-sample losses and calling
    backward once, but only ONE autograd graph is alive at a time. Holding
    all K graphs (each a full forward over ~1k image tokens + completion)
    is what OOM'd a 22 GB L4 at K=6.

    Returns the summed scalar loss for logging.
    """
    total = 0.0
    for (_, seq, gen_len), adv in zip(samples, advs):
        if gen_len == 0:
            continue
        lp = _seq_logprob(policy, inputs, seq, plen, gen_len)
        loss = -(adv * lp) * scale
        loss.backward()
        total += float(loss.detach())
    return total


def _seq_logprob(policy, inputs: dict, full_ids: torch.Tensor, plen: int,
                 gen_len: int) -> torch.Tensor:
    """Sum log-prob of the generated tokens under the current policy.

    Reuse the vision features from the prompt encode (`inputs`) rather than
    re-processing decoded text — the decoded prompt no longer carries the
    <image> placeholder, which is what broke the first attempt.
    """
    ids = full_ids[: plen + gen_len].unsqueeze(0)
    attn = torch.ones_like(ids)
    out = policy(input_ids=ids, attention_mask=attn,
                 **_mm_kwargs(inputs, ids.shape[1]))
    logits = out.logits[0, plen - 1:-1]           # predict token t from t-1
    gen = ids[0, plen:]
    logp = torch.log_softmax(logits.float(), dim=-1)
    return logp[torch.arange(len(gen), device=gen.device), gen].sum()


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--config", required=True)
    ap.add_argument("--init-adapter", required=True,
                    help="SFT adapter to refine (round-6 output)")
    ap.add_argument("--device", default="auto")
    ap.add_argument("--max-steps", type=int, default=None)
    ap.add_argument("--diagnose", type=int, default=None,
                    metavar="N", help="sample N groups, report reward "
                                      "variance, and exit without training")
    args = ap.parse_args()
    cfg = yaml.safe_load(Path(args.config).read_text())
    torch.manual_seed(cfg.get("seed", 0))
    device = torch.device(args.device if args.device != "auto"
                          else ("cuda" if torch.cuda.is_available() else "cpu"))

    from PIL import Image
    d = cfg["data"]
    bench_dir = Path(d["bench_dir"])
    rows = load_rows(d["labels"], d.get("train_split"))
    refs = {r["id"]: r["observation"] for r in rows}
    if not rows:
        raise SystemExit("no training rows")

    processor, policy = build_policy(cfg, args.init_adapter, device)
    g = cfg["grpo"]
    k = g.get("group_size", 6)
    weights = g.get("reward_weights")
    max_steps = args.max_steps or g.get("max_steps", 300)
    accum = g.get("grad_accum", 4)
    opt = torch.optim.AdamW((p for p in policy.parameters() if p.requires_grad),
                            lr=g.get("lr", 2e-5))

    run_dir = Path(cfg.get("output", {}).get("dir", "runs")) / \
        f"{cfg.get('run_name', 'vl-grpo')}-{time.strftime('%Y%m%d-%H%M%S')}"
    run_dir.mkdir(parents=True, exist_ok=True)
    (run_dir / "config.yaml").write_text(yaml.safe_dump(cfg))
    log = open(run_dir / "train_log.jsonl", "a")

    step = 0
    t0 = time.time()
    reward_hist: list[float] = []
    n_groups = n_flat = pending = 0
    diagnose = args.diagnose
    # eval() for the whole loop: dropout must be off while sampling, and the
    # log-prob has to be evaluated under the SAME distribution the sample was
    # drawn from. eval() disables dropout only — LoRA params still get grads.
    policy.eval()
    while step < max_steps:
        perm = torch.randperm(len(rows))
        opt.zero_grad(set_to_none=True)
        for gi in range(len(perm)):
            row = rows[perm[gi]]
            ref = refs[row["id"]]
            image = Image.open(bench_dir / row["image"]).convert("RGB")
            samples, prompt_inputs, plen = _sample_group(
                processor, policy, image, device, k,
                cfg.get("eval", {}).get("max_new_tokens", 384),
                g.get("temperature", 1.0))
            rewards = [compute_reward(resp, ref, weights).total
                       for resp, _, _ in samples]
            advs = group_advantages(rewards)
            mean_r = sum(rewards) / len(rewards)
            spread = max(rewards) - min(rewards)
            reward_hist.append(mean_r)
            n_groups += 1
            flat = spread < 1e-3
            n_flat += flat

            # group-level visibility: without it a run that never reaches an
            # optimizer step (every group flat) looks identical to one that
            # is merely slow
            if n_groups % g.get("log_groups", 10) == 0:
                grec = {"groups": n_groups, "flat_frac": round(n_flat / n_groups, 3),
                        "mean_reward": round(sum(reward_hist[-50:])
                                             / len(reward_hist[-50:]), 4),
                        "last_spread": round(spread, 4),
                        "steps": step, "sec": round(time.time() - t0, 1)}
                print(json.dumps(grec), flush=True)
                log.write(json.dumps(grec) + "\n")
                log.flush()

            if diagnose and n_groups >= diagnose:
                # signal probe: report whether the reward has any variance to
                # learn from, without spending a training run to find out
                summary = {"mode": "diagnose", "groups": n_groups,
                           "flat_frac": round(n_flat / n_groups, 3),
                           "mean_reward": round(sum(reward_hist) / len(reward_hist), 4),
                           "min_reward": round(min(reward_hist), 4),
                           "max_reward": round(max(reward_hist), 4)}
                print(json.dumps(summary))
                (run_dir / "diagnose.json").write_text(json.dumps(summary, indent=2))
                return

            if flat:
                continue  # no signal in this group

            # policy-gradient: -adv * logprob(completion), one graph at a time
            accumulate_policy_gradient(policy, prompt_inputs, samples, advs,
                                       plen, 1.0 / len(samples) / accum)
            del prompt_inputs, samples
            if device.type == "cuda":
                torch.cuda.empty_cache()
            pending += 1

            # step once `accum` INFORMATIVE groups have accumulated; keying
            # off the loop index instead let a flat group at the boundary
            # silently swallow the optimizer step
            if pending >= accum:
                pending = 0
                torch.nn.utils.clip_grad_norm_(
                    (p for p in policy.parameters() if p.requires_grad), 1.0)
                opt.step()
                opt.zero_grad(set_to_none=True)
                step += 1
                if step % g.get("log_every", 10) == 0 or step == 1:
                    recent = sum(reward_hist[-50:]) / len(reward_hist[-50:])
                    rec = {"step": step, "mean_reward": round(recent, 4),
                           "sec": round(time.time() - t0, 1)}
                    print(json.dumps(rec))
                    log.write(json.dumps(rec) + "\n")
                    log.flush()
                # checkpoint periodically: GRPO steps are expensive (K
                # generations each), so a crash near the end must not throw
                # away hours of refinement
                ckpt_every = g.get("checkpoint_every", 50)
                if ckpt_every and step % ckpt_every == 0:
                    policy.save_pretrained(run_dir / "adapter")
                    processor.save_pretrained(run_dir / "adapter")
                    print(f"  checkpoint @ step {step}")
                if step >= max_steps:
                    break

    policy.save_pretrained(run_dir / "adapter")
    processor.save_pretrained(run_dir / "adapter")
    first = sum(reward_hist[:20]) / max(1, len(reward_hist[:20]))
    last = sum(reward_hist[-20:]) / max(1, len(reward_hist[-20:]))
    summary = {"steps": step, "mean_reward_first20": round(first, 4),
               "mean_reward_last20": round(last, 4)}
    (run_dir / "summary.json").write_text(json.dumps(summary, indent=2))
    print(json.dumps(summary))
    print(f"adapter saved to {run_dir / 'adapter'}")


if __name__ == "__main__":
    main()

#!/usr/bin/env python3
"""给 trainer/diffusion.py 移植 LoRA(adapter) 支持，逐段镜像 trainer/distillation.py 的既有模式。"""
import re, sys

path = sys.argv[1] if len(sys.argv) > 1 else "diffusion.py"
src = open(path).read()
orig = src

# ---- 1. imports ----
anchor = """from torch.distributed.fsdp import (
    StateDictType, FullStateDictConfig, FullOptimStateDictConfig
)"""
assert src.count(anchor) == 1
src = src.replace(anchor, anchor + """
import peft
from peft import get_peft_model_state_dict
from utils.lora_utils import configure_lora_for_model""")

# ---- 2. LoRA init：把原 auto-resume/加载段包进 else 分支 ----
seg_start = """        raw_state = None

        checkpoint_path = None

        if auto_resume and self.output_path:"""
seg_end = "        # ================================= FSDP Wrap ================================="
i = src.index(seg_start)
j = src.index(seg_end)
original_block = src[i:j]
# 原块去掉头三行（raw_state / checkpoint_path 声明保留在外面）
head = """        raw_state = None

        checkpoint_path = None

"""
assert original_block.startswith(head)
body = original_block[len(head):]
indented = "\n".join(("    " + ln) if ln.strip() else ln for ln in body.split("\n"))

lora_block = head + """        # ---- LoRA (adapter) support, mirrored from trainer/distillation.py ----
        self.is_lora_enabled = False
        self.lora_config = None
        if getattr(config, "adapter", None) is not None:
            self.is_lora_enabled = True
            self.lora_config = config.adapter
            if self.is_main_process:
                print(f"LoRA enabled with config: {self.lora_config}")

        if self.is_lora_enabled:
            # Base weights come from generator_ckpt only; logdir checkpoints
            # are LoRA-only and are restored after wrapping (distillation.py style).
            base_ckpt = getattr(config, "generator_ckpt", None)
            if base_ckpt:
                if self.is_main_process:
                    print(f"[LoRA] Loading base model from {base_ckpt} (before applying LoRA)")
                checkpoint = torch.load(base_ckpt, map_location="cpu")
                if isinstance(checkpoint, dict) and "generator" in checkpoint:
                    self.model.generator.load_state_dict(checkpoint["generator"], strict=True)
                elif isinstance(checkpoint, dict) and "model" in checkpoint:
                    self.model.generator.load_state_dict(checkpoint["model"], strict=True)
                else:
                    self.model.generator.load_state_dict(checkpoint, strict=True)
                del checkpoint
                gc.collect()

            self.model.generator.model = configure_lora_for_model(
                self.model.generator.model, "generator", self.lora_config,
                is_main_process=self.is_main_process)

            lora_ckpt_path = None
            if auto_resume and self.output_path:
                lora_ckpt_path = self.find_latest_checkpoint(self.output_path)
                if self.is_main_process and lora_ckpt_path:
                    print(f"Auto resume: Found LoRA checkpoint at {lora_ckpt_path}")
            if lora_ckpt_path is None and getattr(config, "lora_ckpt", None):
                lora_ckpt_path = config.lora_ckpt
            if lora_ckpt_path:
                lora_checkpoint = torch.load(lora_ckpt_path, map_location="cpu")
                if "generator_lora" not in lora_checkpoint:
                    raise ValueError(
                        f"LoRA checkpoint {lora_ckpt_path} is not a valid LoRA checkpoint. "
                        f"Found keys: {list(lora_checkpoint.keys())}")
                peft.set_peft_model_state_dict(
                    self.model.generator.model, lora_checkpoint["generator_lora"])
                if "step" in lora_checkpoint:
                    self.step = lora_checkpoint["step"]
                    if self.is_main_process:
                        print(f"Resuming LoRA training from step {self.step}")
                del lora_checkpoint
                gc.collect()
            # NOTE: optimizer state is not resumed on the LoRA path (AdamW
            # restarts); acceptable for short adapter runs.
        else:
""" + indented + "\n"

src = src[:i] + lora_block + src[j:]

# ---- 3. save()：LoRA 只存 adapter 权重（distillation._gather_lora_state_dict 同款） ----
save_seg = """        with FSDP.state_dict_type(
            self.model.generator,
            StateDictType.FULL_STATE_DICT,
            FullStateDictConfig(rank0_only=True, offload_to_cpu=True),
            FullOptimStateDictConfig(rank0_only=True, offload_to_cpu=True),
        ):
            generator_state_dict  = self.model.generator.state_dict()
            generator_opim_state_dict = FSDP.optim_state_dict(self.model.generator,
                                            self.generator_optimizer)

        if self.config.ema_start_step < self.step and self.generator_ema is not None:
            state_dict = {
                "generator": generator_state_dict,
                "generator_ema": self.generator_ema.state_dict(),
                "generator_optimizer": generator_opim_state_dict,
                "step": self.step,
            }
        else:
            state_dict = {
                "generator": generator_state_dict,
                "generator_optimizer": generator_opim_state_dict,
                "step": self.step,
            }"""
assert src.count(save_seg) == 1
save_new = """        if getattr(self, "is_lora_enabled", False):
            # LoRA-only checkpoint (mirrors distillation._gather_lora_state_dict).
            with FSDP.state_dict_type(
                self.model.generator.model,
                StateDictType.FULL_STATE_DICT,
                FullStateDictConfig(rank0_only=True, offload_to_cpu=True)
            ):
                full = self.model.generator.model.state_dict()
            gen_lora_sd = get_peft_model_state_dict(
                self.model.generator.model, state_dict=full)
            state_dict = {
                "generator_lora": gen_lora_sd,
                "step": self.step,
            }
        else:
""" + "\n".join(("    " + ln) if ln.strip() else ln for ln in save_seg.split("\n"))
src = src.replace(save_seg, save_new)

assert src != orig
open(path, "w").write(src)
print("patched ok:", path)

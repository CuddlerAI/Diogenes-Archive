#!/usr/bin/env python3
"""WM 0.1 Phase 2 · 给 LongLive 移植动作模块训练（34 号文 §4 Phase 2 + 35 号文 §3.3 消融臂）。

打两个文件（以 VM 上已带 LoRA 移植的版本为基底，见 patch_longlive_lora.py）：

  utils/dataset.py      —— MultiVideoConcatDataset 逐段记录 (video_path, start_frame,
                           n_frames) 进 `segments_meta`，collate 透传（不 tensor 化）。
  trainer/diffusion.py  —— 新增 config 段 `action_module`；enabled 时冻结 generator
                           （含 LoRA），只训 ActionConditioner（实测 19.97M，FSDP NO_SHARD
                           包裹 = DDP 式梯度 all-reduce），按 segments_meta 对位加载
                           动作 npz 喂 set_actions；save() 只存 conditioner。

用法（在 ~/LongLive 下）:
    python patch_longlive_phase2.py [trainer/diffusion.py] [utils/dataset.py]

幂等：检测到标记串（"segments_meta" / "action_module_enabled"）即跳过对应文件。
所有锚点 assert 唯一，替换后 py_compile 验证。

## 与任务书的已知偏离（以实际代码为准，一手核查）

1. **grid 是 (24, 15, 26) 而非 (24, 30, 52)。** DiT 的 patch_embedding 是
   stride (1,2,2) 的 Conv3d（wan_5b/modules/causal_model.py），block 隐状态每个
   潜帧只有 (30//2)*(52//2)=390 个 token；ActionModule.forward 断言
   tt*th*tw == x.shape[1]，传潜格 (30,52) 会直接断言失败。补丁里从
   model.patch_size 现算，不写死。
2. **teacher_forcing=true（train_i2v_ar.yaml 既有配置）时 block 隐状态是
   [clean tokens | noisy tokens] 的 2×L 拼接**（causal_model.py _forward_train:
   `x = torch.cat([clean_x, x], dim=1)`）。conditioner 把 2×L 折叠进 batch 维
   一次算完，动作对两半同样生效——推理期 KV cache 里的上下文本就是被动作调制
   过的隐状态，训练时 clean 上下文也应调制。
3. **不支持 sequence parallel**（sp>1 时 token 按 rank 分片，折叠假设失效），
   enabled 时 assert sp==1。Phase 1/2 均为 4 卡 DP，不受影响。
4. **参数量是 19.97M 不是 34 号文写的 13.7M。** 13.7M 是按 img_hidden_size=1536
   估的（action_module.py docstring 的示例值）；实际 Wan2.2-TI2V-5B 的 DiT
   dim=3072，本机 CPU 实测 19.97M。不改配方（仍是"只训动作模块"），
   _setup_action_module 会打印实测值，报数时用它。
"""
import py_compile
import sys

MARK_DATASET = "segments_meta"
MARK_TRAINER = "action_module_enabled"


def _replace_unique(src, anchor, replacement, label):
    n = src.count(anchor)
    assert n == 1, f"锚点不唯一或缺失（{label}）: count={n}\n---\n{anchor}\n---"
    return src.replace(anchor, replacement)


# =====================================================================
# utils/dataset.py
# =====================================================================

def patch_dataset(path):
    src = open(path).read()
    if MARK_DATASET in src:
        print(f"already patched (skip): {path}")
        return
    orig = src

    # ---- 1. __getitem__: 初始化 segments_meta ----
    anchor = """        # Collect all segments
        all_segments = []
        prompts_list = []"""
    src = _replace_unique(src, anchor, anchor + """
        # [phase2] 逐段记录 (video_path, start_frame, n_frames)，供训练侧对位加载动作 npz
        segments_meta = []""", "dataset/init")

    # ---- 2. 第一段采样处记录 meta ----
    anchor = """            segment_frames, total_frames, original_fps = self._sample_frames_from_video(
                video_path, self.first_chunk_frames, current_start_frame
            )
            all_segments.append(segment_frames)"""
    src = _replace_unique(src, anchor, anchor + """
            segments_meta.append((str(video_path),
                                  int(np.round(current_start_frame)),
                                  int(self.first_chunk_frames)))""", "dataset/first-seg")

    # ---- 3. 后续段采样处记录 meta ----
    anchor = """                segment_frames, total_frames, original_fps = self._sample_frames_from_video(
                    video_path, self.subsequent_chunk_frames, current_start_frame
                )
                all_segments.append(segment_frames)"""
    src = _replace_unique(src, anchor, anchor + """
                segments_meta.append((str(video_path),
                                      int(np.round(current_start_frame)),
                                      int(self.subsequent_chunk_frames)))""", "dataset/sub-seg")

    # ---- 4. 返回 dict 带上 meta ----
    anchor = """            result = {
                'frames': concatenated_video.permute(1, 0, 2, 3),
                'prompts': prompts_list,
                'idx': folder_idx
            }"""
    src = _replace_unique(src, anchor, """            result = {
                'frames': concatenated_video.permute(1, 0, 2, 3),
                'prompts': prompts_list,
                'idx': folder_idx,
                'segments_meta': segments_meta,
            }""", "dataset/result")

    # ---- 5. collate 透传（list of list，不 tensor 化） ----
    anchor = """    if "image" in batch[0]:
        result["image"] = torch.stack([b["image"] for b in batch], dim=0)"""
    src = _replace_unique(src, anchor, """    if "segments_meta" in batch[0]:
        # [phase2] 保持 python 结构透传；训练侧按需读 npz
        result["segments_meta"] = [b["segments_meta"] for b in batch]

""" + anchor, "dataset/collate")

    assert src != orig
    open(path, "w").write(src)
    py_compile.compile(path, doraise=True)
    print("patched ok:", path)


# =====================================================================
# trainer/diffusion.py（基底 = 已打 patch_longlive_lora.py 的 VM 版本）
# =====================================================================

def patch_trainer(path):
    src = open(path).read()
    if MARK_TRAINER in src:
        print(f"already patched (skip): {path}")
        return
    assert "is_lora_enabled" in src, (
        f"{path} 未见 LoRA 移植痕迹——本补丁以 patch_longlive_lora.py 之后的版本为基底，"
        "请先打 LoRA 补丁（或直接用 VM 上的现行 trainer/diffusion.py）")
    orig = src

    # ---- 1. imports ----
    anchor = """import peft
from peft import get_peft_model_state_dict
from utils.lora_utils import configure_lora_for_model"""
    src = _replace_unique(src, anchor, anchor + """
import sys
from collections import OrderedDict
import numpy as np""", "trainer/imports")

    # ---- 2. 配置解析（要在 LoRA 恢复逻辑之前：enabled 时 logdir 里的 checkpoint
    #         是 action_conditioner 的，LoRA auto-resume 不能去读它） ----
    anchor = "        # ---- LoRA (adapter) support, mirrored from trainer/distillation.py ----"
    src = _replace_unique(src, anchor, """        # ---- [phase2] ActionModule 配置段 ----
        # config.action_module: {enabled, action_dir, blocks_fraction=0.5,
        #                        mouse_scale=1.0, confidence_modulate=false,
        #                        freeze_backbone=true}
        am_cfg = getattr(config, "action_module", None)
        self.action_module_enabled = bool(
            am_cfg is not None and am_cfg.get("enabled", False))
        self.action_conditioner = None
        if self.action_module_enabled:
            self._action_dir = str(am_cfg.get("action_dir", ""))
            self._am_blocks_fraction = float(am_cfg.get("blocks_fraction", 0.5))
            self._action_mouse_scale = float(am_cfg.get("mouse_scale", 1.0))
            self._action_confidence_modulate = bool(
                am_cfg.get("confidence_modulate", False))
            self._am_freeze_backbone = bool(am_cfg.get("freeze_backbone", True))
            self._action_cache = OrderedDict()
            assert self._action_dir and os.path.isdir(self._action_dir), (
                f"action_module.action_dir 不存在: {self._action_dir!r}")
            assert self._action_mouse_scale > 0, "mouse_scale 必须 > 0"
            assert self.sequence_parallel_size == 1, (
                "[phase2] 动作模块暂不支持 sequence parallel"
                "（token 折叠假设整段序列在本 rank）")

""" + anchor, "trainer/config")

    # ---- 3. LoRA auto-resume 让位：phase2 的 logdir checkpoint 不是 LoRA 的 ----
    anchor = """            lora_ckpt_path = None
            if auto_resume and self.output_path:"""
    src = _replace_unique(src, anchor, """            lora_ckpt_path = None
            # [phase2] 动作模块启用时 logdir 的 checkpoint 是 action_conditioner 的，
            # LoRA 只能来自 config.lora_ckpt（Phase 1 产物）。
            if (not self.action_module_enabled) and auto_resume and self.output_path:""",
                          "trainer/lora-resume-gate")

    # ---- 4. FSDP 包裹之前挂载动作模块 ----
    anchor = "        # ================================= FSDP Wrap ================================="
    src = _replace_unique(src, anchor, """        # ============== [phase2] ActionModule 挂载（必须在 FSDP 包裹之前） ==============
        if self.action_module_enabled:
            self._setup_action_module(config, auto_resume)

""" + anchor, "trainer/setup-call")

    # ---- 5. 优化器只收 conditioner 参数 ----
    anchor = """        self.generator_optimizer = torch.optim.AdamW(
            [param for param in self.model.generator.parameters()
             if param.requires_grad],
            lr=config.lr,"""
    src = _replace_unique(src, anchor, """        if self.action_conditioner is not None:
            # [phase2] 只训 conditioner；底座（含 LoRA）已冻结
            trainable_params = [p for p in self.action_conditioner.parameters()
                                if p.requires_grad]
            assert trainable_params, "action conditioner 没有可训参数？"
        else:
            trainable_params = [param for param in self.model.generator.parameters()
                                if param.requires_grad]
        self.generator_optimizer = torch.optim.AdamW(
            trainable_params,
            lr=config.lr,""", "trainer/optimizer")

    # ---- 6. _setup_action_module + npz 加载方法 ----
    anchor = "    def _move_optimizer_to_device(self, optimizer, device):"
    setup_methods = '''    # ---------------- [phase2] ActionModule ----------------
    # 对照 model_research/research/34-wm01-training-method.md §4 Phase 2：
    #  * LoRA 冻结、只训 ActionConditioner（dim=3072 下实测 19.97M），lr 由 config 给（1e-4）；
    #  * 挂 DiT 前 blocks_fraction（默认一半 = 前 15/30 block，MG2 的实时预算做法）；
    #  * conditioner 用 FSDP NO_SHARD 单独包裹：等价 DDP 的梯度 all-reduce，
    #    参数不分片（才 ~14M），并复用本仓 fsdp_wrap 的混合精度策略
    #    （bf16 前向 / fp32 梯度归约）。
    # 已知偏离（以实际代码为准）：
    #  * grid 用 token 网格 (t, h//patch, w//patch)=(24,15,26)，不是潜格 (24,30,52)
    #    ——ActionModule 断言 tt*th*tw == x.shape[1]，而 DiT patch_embedding 是
    #    stride (1,2,2) 的 Conv3d，每潜帧只有 390 个 token。
    #  * teacher_forcing 时 block 隐状态是 [clean | noisy] 2×L 拼接，折叠进
    #    batch 维一次算完（见 _TFActionConditioner）。
    def _setup_action_module(self, config, auto_resume):
        try:
            from diogenes_wm.wm01.action_module import (
                ActionConditioner, build_action_module)
        except ImportError:
            # VM 上 diogenes_wm 在 ~/mlcode/ml 下
            sys.path.insert(0, os.path.join(os.path.expanduser("~"), "mlcode", "ml"))
            from diogenes_wm.wm01.action_module import (
                ActionConditioner, build_action_module)

        gen_model = self.model.generator.model
        # LoRA 启用时 gen_model 是 PeftModel，属性转发也能拿到 blocks/dim，
        # 但显式取 base 模型更稳。
        base = gen_model.get_base_model() if hasattr(gen_model, "get_base_model") else gen_model
        blocks = list(base.blocks)
        dim = int(base.dim)                        # Wan2.2-TI2V-5B: 3072
        ps = base.patch_size
        pt, ph, pw = ps if isinstance(ps, (tuple, list)) else (1, ps, ps)
        shape = list(config.image_or_video_shape)  # [B, T_lat, C, H_lat, W_lat]
        grid = (int(shape[1]) // int(pt),
                int(shape[3]) // int(ph),
                int(shape[4]) // int(pw))

        # 冻结底座（含 LoRA）。必须在 FSDP 包裹前做。
        if self._am_freeze_backbone:
            self.model.generator.requires_grad_(False)

        class _TFActionConditioner(ActionConditioner):
            """teacher forcing 兼容：token 数是 k*(tt*th*tw) 时折叠进 batch 维。

            causal_model._forward_train 在 clean_x 非空时把序列拼成
            [clean tokens | noisy tokens]（k=2）。两半是同一 24 潜帧的两份
            隐状态，动作条件相同：reshape(B*k, L, C) 后 batch 内按
            (b0-clean, b0-noisy, b1-clean, ...) 排列，动作 repeat_interleave(k)。
            """

            def forward(self, x):
                if self._mouse is None:
                    raise RuntimeError(
                        "先调 set_actions() 再前向——训练循环每 batch 设一次。"
                        "若由评估/推理路径触发：phase2 训练不接推理，"
                        "evaluation.interval 必须为 0")
                L = self.tt * self.th * self.tw
                B, tokens, C = x.shape
                if tokens == L:
                    return self.action_module(
                        x, self.tt, self.th, self.tw,
                        mouse_condition=self._mouse,
                        keyboard_condition=self._keyboard)
                if tokens % L != 0:
                    raise ValueError(
                        f"block 隐状态 {tokens} token 不是 tt*th*tw={L} 的整数倍——"
                        f"网格 {(self.tt, self.th, self.tw)} 与实际模型不符")
                k = tokens // L
                out = self.action_module(
                    x.reshape(B * k, L, C), self.tt, self.th, self.tw,
                    mouse_condition=self._mouse.repeat_interleave(k, dim=0),
                    keyboard_condition=self._keyboard.repeat_interleave(k, dim=0))
                return out.reshape(B, tokens, C)

        conditioner = _TFActionConditioner(
            build_action_module(img_hidden_size=dim), grid=grid)
        n_params = sum(p.numel() for p in conditioner.parameters())

        # auto-resume：phase2 logdir 的 checkpoint 是 {"action_conditioner", "step"}
        if auto_resume and self.output_path:
            ckpt_path = self.find_latest_checkpoint(self.output_path)
            if ckpt_path:
                ckpt = torch.load(ckpt_path, map_location="cpu")
                if "action_conditioner" not in ckpt:
                    raise ValueError(
                        f"{ckpt_path} 不是 action_conditioner checkpoint，"
                        f"keys={list(ckpt.keys())}——logdir 指错了？")
                conditioner.load_state_dict(ckpt["action_conditioner"])
                if "step" in ckpt:
                    self.step = ckpt["step"]
                del ckpt
                gc.collect()
                if self.is_main_process:
                    print(f"[ActionModule] resumed from {ckpt_path} (step {self.step})")

        conditioner = conditioner.to(self.device)
        self.action_conditioner = fsdp_wrap(
            conditioner,
            sharding_strategy="no_shard",
            mixed_precision=config.mixed_precision,
            wrap_strategy="size",
        )

        # 挂前 blocks_fraction 的 block。hook 注册在裸 block 上，随后的
        # generator FSDP 包裹保留内层 module 的 hook。hook 调 FSDP 包裹后的
        # conditioner，保证前向经过 FSDP 的梯度归约注册。
        n_attach = max(1, int(round(len(blocks) * self._am_blocks_fraction)))
        wrapped = self.action_conditioner

        def _action_hook(_module, _inp, out):
            if isinstance(out, tuple):
                return (wrapped(out[0]), *out[1:])
            return wrapped(out)

        for b in blocks[:n_attach]:
            b.register_forward_hook(_action_hook)

        if self.is_main_process:
            print(f"[ActionModule] {n_params / 1e6:.1f}M params, grid={grid}, "
                  f"attached to first {n_attach}/{len(blocks)} DiT blocks, "
                  f"action_dir={self._action_dir}, "
                  f"mouse_scale={self._action_mouse_scale}, "
                  f"confidence_modulate={self._action_confidence_modulate}, "
                  f"freeze_backbone={self._am_freeze_backbone}")

    def _load_action_npz(self, sess, stem):
        """action_dir/<sess>/<stem>.npz → (mouse [93,2] fp32, keyboard [93,6] fp32)。

        keyboard 的 confidence 调制（A3 臂）在缓存前完成；mouse 的 scale 在
        _set_batch_actions 里除（缓存保持原始值，方便排查）。
        LRU 上限 2048：全语料 857 clip × 几 KB 本可全缓存，仍设限防泄漏。
        """
        key = (sess, stem)
        entry = self._action_cache.get(key)
        if entry is not None:
            self._action_cache.move_to_end(key)
            return entry
        path = os.path.join(self._action_dir, sess, stem + ".npz")
        with np.load(path) as z:
            mouse = np.asarray(z["mouse"], dtype=np.float32)
            keyboard = np.asarray(z["keyboard"], dtype=np.float32)
            if self._action_confidence_modulate and "keyboard_confidence" in z.files:
                keyboard = keyboard * np.asarray(
                    z["keyboard_confidence"], dtype=np.float32)
        entry = (mouse, keyboard)
        self._action_cache[key] = entry
        if len(self._action_cache) > 2048:
            self._action_cache.popitem(last=False)
        return entry

    def _set_batch_actions(self, batch, batch_size):
        """按 segments_meta 对位拼接动作序列并喂给 conditioner。

        对位前提：采样间隔恒为 1（24fps 源 × target_fps 24），视频帧号与 npz
        行号一一对应。若上游改了 fps 使间隔 ≠ 1，这条假设即失效——插值采样的
        帧没有对应的动作行，必须回到 30fps chunk 侧重做重采样（34 号文 §2）。
        """
        segs_meta = batch.get("segments_meta")
        if segs_meta is None:
            raise KeyError(
                "batch 缺 segments_meta——utils/dataset.py 还没打 phase2 补丁")
        assert len(segs_meta) == batch_size
        mouse_rows, kb_rows = [], []
        for sample_segs in segs_meta:
            m_parts, k_parts = [], []
            for video_path, start, n in sample_segs:
                sess = os.path.basename(os.path.dirname(video_path))
                stem = os.path.splitext(os.path.basename(video_path))[0]
                mouse_np, kb_np = self._load_action_npz(sess, stem)
                m = mouse_np[start:start + n]
                k = kb_np[start:start + n]
                if m.shape[0] < n:
                    # 与 _sample_frames_from_video 的"末帧补齐"一致
                    #（93 帧整 clip 上正常不会走到）
                    pad = n - m.shape[0]
                    m = np.concatenate([m, np.repeat(m[-1:], pad, axis=0)], axis=0)
                    k = np.concatenate([k, np.repeat(k[-1:], pad, axis=0)], axis=0)
                m_parts.append(m)
                k_parts.append(k)
            mouse_rows.append(np.concatenate(m_parts, axis=0))
            kb_rows.append(np.concatenate(k_parts, axis=0))
        mouse = torch.from_numpy(np.stack(mouse_rows)) / self._action_mouse_scale
        keyboard = torch.from_numpy(np.stack(kb_rows))
        # set_actions 经 FSDP 属性转发落在内层 module 上；shape/潜帧数校验在其内部
        self.action_conditioner.set_actions(
            mouse.to(device=self.device, dtype=self.dtype),
            keyboard.to(device=self.device, dtype=self.dtype))

'''
    src = _replace_unique(src, anchor, setup_methods + anchor, "trainer/methods")

    # ---- 7. 训练循环：每 batch 喂动作 ----
    anchor = """        text_prompts = batch["prompts"]
        batch_size = len(text_prompts)"""
    src = _replace_unique(src, anchor, anchor + """
        if self.action_conditioner is not None:
            self._set_batch_actions(batch, batch_size)""", "trainer/per-batch")

    # ---- 8. 梯度裁剪对象换成 conditioner ----
    anchor = """        if accumulation_step == accumulation_steps - 1:
            generator_grad_norm = self.model.generator.clip_grad_norm_(
                self.max_grad_norm)"""
    src = _replace_unique(src, anchor, """        if accumulation_step == accumulation_steps - 1:
            if self.action_conditioner is not None:
                generator_grad_norm = self.action_conditioner.clip_grad_norm_(
                    self.max_grad_norm)
            else:
                generator_grad_norm = self.model.generator.clip_grad_norm_(
                    self.max_grad_norm)""", "trainer/grad-clip")

    # ---- 9. save()：enabled 时只存 conditioner ----
    anchor = """        if getattr(self, "is_lora_enabled", False):
            # LoRA-only checkpoint (mirrors distillation._gather_lora_state_dict)."""
    src = _replace_unique(src, anchor, """        if getattr(self, "action_conditioner", None) is not None:
            # [phase2] 只存动作模块：LoRA 冻结不变（Phase 1 checkpoint 原样在
            # config.lora_ckpt），conditioner 才几十 MB，FULL_STATE_DICT rank0 落盘。
            with FSDP.state_dict_type(
                self.action_conditioner,
                StateDictType.FULL_STATE_DICT,
                FullStateDictConfig(rank0_only=True, offload_to_cpu=True)
            ):
                cond_sd = self.action_conditioner.state_dict()
            state_dict = {
                "action_conditioner": cond_sd,
                "step": self.step,
            }
        elif getattr(self, "is_lora_enabled", False):
            # LoRA-only checkpoint (mirrors distillation._gather_lora_state_dict).""",
                          "trainer/save")

    assert src != orig
    open(path, "w").write(src)
    py_compile.compile(path, doraise=True)
    print("patched ok:", path)


if __name__ == "__main__":
    path_trainer = sys.argv[1] if len(sys.argv) > 1 else "trainer/diffusion.py"
    path_dataset = sys.argv[2] if len(sys.argv) > 2 else "utils/dataset.py"
    patch_trainer(path_trainer)
    patch_dataset(path_dataset)

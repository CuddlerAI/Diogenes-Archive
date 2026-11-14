#!/usr/bin/env python3
"""WM 0.1 Phase 2 · E1a 消融臂评测 rollout：动作条件化的 holdout 首帧 I2V。

对 holdout_flat 的 126 个 clip 逐个做首帧 I2V rollout（与 phase0full 同一协议：
同 base+LoRA(merge) 底座、同 inference_i2v.yaml、同 seed、同 save_with_index 命名），
区别只有一个：rollout 前按 clip 的 stem 从对应动作目录载入 npz，经与训练完全相同的
预处理喂给 ActionConditioner，使生成过程被动作调制。

产出布局与 ~/runs/phase0full/rollouts 相同：
    <out>/rank0-<idx>-0_regular.mp4        idx = manifest 行号（0 起）
    <out>/rank0-<idx>-0_regular_prompts.txt
    <out>/../manifest.txt                  sorted(ls holdout_flat/video)，与
                                           ~/runs/phase0full/manifest.txt 逐行核对
既有 E1a 评分器（diogenes_wm.wm01.score_rollout + score_ws 序号映射）无需改动直接可用。

用法（VM 上，脚本自动注入 ~/LongLive 与 ~/mlcode/ml 到 sys.path）:

    cd ~/LongLive && .venv/bin/python ~/e1a_arm_rollout.py \\
        --action-ckpt ~/runs/phase2-pseudo_conf/checkpoint_model_000750/model.pt \\
        --action-source pseudo_conf \\
        --out ~/runs/e1a-pseudo_conf/rollouts [--limit 2] [--device cuda:0]

    # 数学一致性自检（无需 GPU / LongLive / checkpoint，随机权重）:
    python e1a_arm_rollout.py --selfcheck [--device cuda:0]

## 训练配置即出处（防漂移）

mouse_scale / confidence_modulate / blocks_fraction / action_dir 优先从
`<ckpt>/../../train_phase2.yaml`（训练时由 phase2.sh 落盘）读取，CLI 只做交叉核对；
读不到时退回 CLI 默认值（默认值与 ml/scripts/wm01/phase2.sh 同源：mouse_scale=200.0
= 2026-08-02 平面 action 实测 p99 标定）。预处理逐行对照训练侧
patch_longlive_phase2.py 的 `_load_action_npz` / `_set_batch_actions`：
    keyboard（pseudo_conf 臂）先乘 keyboard_confidence，mouse 后除 mouse_scale。

## 分块推理与整段训练的数学一致性（论证 + 近似声明）

训练（patch_longlive_phase2.py）在 teacher_forcing 下把 [clean | noisy] 折叠进
batch 维，两半各自独立地做**整段 24 潜帧**的时序注意力；即训练时任一 token 的动作
残差 = 同噪声级、24 帧全跨度注意力的结果。推理是 KV-cache 分块自回归
（CausalDiffusionInferencePipeline：每块 8 潜帧 × (4 步去噪 + 1 次 clean recache)），
hook 每次只见当前块的 token。整段前向在数学上**不可复现**：

  * 未来帧尚未生成——mouse 分支的注意力 K/V 依赖各帧隐状态（`mouse_mlp(concat(x_t,
    窗口_t))`，vendor/action_module.py L206-227），未来的 x_t 不存在；
  * 过去帧在该层的隐状态未被缓存——DiT 的 KV cache 存的是注意力 K/V 投影，
    不是 block 输出。

因此本实现取「**窗口精确、跨度受限**」的近似（BlockwiseActionConditioner）：

  1. **动作窗口特征与训练逐元素相同。** 每潜帧回看 ratio×window=12 源帧的窗口在
     set_actions 时按 vendor 同款 pad+切窗对全 24 帧预计算，forward 按当前块的
     潜帧范围 [f0, f0+nf) 切片——首帧 pad 语义与训练一致（vendor 对逐块调用则会在
     每块开头错误地用块首帧重复填充，故不能直接分段调用 vendor forward，见 L211-217
     的 pad 逻辑）。
  2. **keyboard 分支与训练同跨度（该分支精确）。** 其注意力 K/V 只依赖动作
     （keyboard_embed→窗口→attn_kv，与隐状态无关，vendor L271-289），动作先验已知，
     故预计算全 24 帧 K/V（RoPE 位置 0..23），当前块的 q 施加绝对位置 f0..f0+nf-1
     的 RoPE 切片——每个 q 的注意力分布与训练完全同构（softmax 按 query 独立）。
     唯一的间接差异经由 q 的输入（含 mouse 残差）传入。
  3. **mouse 分支跨度截断为当前块（近似所在）。** K/V 依赖隐状态 → 只能在当前块
     8 帧内做注意力（RoPE 同样取绝对位置切片，块内相对几何与训练一致）。
     近似边界：时序注意力跨度 24 → 8。
  4. **不引入 clean-past 记忆**（vendor 的 *_cond_memory 机制）：训练的折叠结构中
     noisy 半与 clean 半互不注意，把过去块的 clean 隐状态喂进当前 noisy 块的
     注意力会制造训练从未见过的混噪声级注意力，故弃用。
  5. **一致性锚点：** f0=0、nf=24（整段一次）时本实现与 vendor
     ActionModule.forward 逐位一致——`--selfcheck` 用随机权重验证 allclose，
     这是对上述重写没有引入静默算子漂移的直接检验。

残差挂在 DiT block 输出上（与训练同一挂载面），clean recache 前向同样被调制，
因此 KV cache 里的上下文是被动作调制过的隐状态——与训练补丁 docstring
「推理期 KV cache 里的上下文本就是被动作调制过的隐状态」的立论一致。

## 显存杠杆（与训练臂共卡的冒烟用；评测正跑在空闲卡上全部关掉）

  --offload-dit    DiT 权重驻 CPU、按访问流式上 GPU（DynamicSwapInstaller，
                   与 inference.py 对 text_encoder 的用法同源）；~10GB → <1GB 常驻
  --decode swap    diffusion 期间不做 VAE decode；出 latent 后先释放 KV cache 再
                   decode（数学与 inline 完全一致，只挪时机）。inline = 与
                   phase0full 完全同路径
  --lean-neg-kv    guidance_scale==1.0 时负分支 KV cache 从不被读写（
                   causal_diffusion_inference.py 里所有 kv_cache_neg 访问都在
                   use_cfg 分支内），把它分配在 CPU 省 ~4.6GB（默认开）
  --min-free-gb N  启动前 GPU 空闲显存不足 N GB 直接退出（冒烟纪律：推迟不抢占）

已知边界：只支持 batch=1（vendor 的 (B S) 折叠在 B>1 时 repeat 顺序有歧义，训练
与评测均为 bs1）；只支持 93 帧 /(N-1)%4==0 的 clip（与语料冻结口径一致）。
"""
from __future__ import annotations

import argparse
import os
import sys
from pathlib import Path

# ---- diogenes_wm 可导入：repo 内跑用相对路径，VM 上退回 ~/mlcode/ml（与训练补丁同款） ----
for _p in (Path(__file__).resolve().parents[2],
           Path.home() / "mlcode" / "ml"):
    if (_p / "diogenes_wm").is_dir() and str(_p) not in sys.path:
        sys.path.insert(0, str(_p))

import torch
from einops import rearrange

from diogenes_wm.wm01.action_module import ActionConditioner, build_action_module
from diogenes_wm.wm01.vendor import action_module as _vendor_am
from diogenes_wm.wm01.vendor.posemb_layers import apply_rotary_emb


def _attention(q: torch.Tensor, k: torch.Tensor, v: torch.Tensor) -> torch.Tensor:
    """与 vendor forward 的注意力分支逐字对应（flash 优先，SDPA 兜底，均非因果）。"""
    if _vendor_am.flash_attn_ops is not None:
        return _vendor_am.flash_attn_ops.flash_attn_func(q, k, v)
    return torch.nn.functional.scaled_dot_product_attention(
        q.transpose(1, 2), k.transpose(1, 2), v.transpose(1, 2)
    ).transpose(1, 2).contiguous()


class BlockwiseActionConditioner(ActionConditioner):
    """分块自回归推理用的 ActionConditioner（数学关系见模块 docstring §一致性）。

    用法：
        cond.set_actions(mouse[1,93,2], keyboard[1,93,6])   # 每 clip 一次，预计算全序列特征
        cond.set_block(f0, nf)     # 每次 generator 前向前由 forward_pre_hook 设置
        cond.attach(blocks[:n])    # 基类残差 hook（tuple 输出兼容）
    """

    def __init__(self, action_module, *, grid: tuple[int, int, int]):
        super().__init__(action_module, grid=grid)
        self._block: tuple[int, int] | None = None
        self._mouse_windows = None   # [1, tt, ratio*ws, 2]
        self._kb_k = None            # [1, tt, H, D]，RoPE(0..tt-1) 已施加
        self._kb_v = None            # [1, tt, H, D]
        self._freqs_mouse = None     # (cos[tt,·], sin[tt,·])
        self._freqs_kb = None

    def set_block(self, f0: int, nf: int) -> None:
        if not (0 <= f0 and nf >= 1 and f0 + nf <= self.tt):
            raise ValueError(f"块范围 [{f0}, {f0 + nf}) 超出全序列潜帧数 {self.tt}")
        self._block = (f0, nf)

    @torch.no_grad()
    def set_actions(self, mouse: torch.Tensor, keyboard: torch.Tensor) -> None:
        super().set_actions(mouse, keyboard)   # (B,T) 一致性 + 潜帧数==tt 校验
        am = self.action_module
        if mouse.shape[0] != 1:
            raise ValueError("只支持 batch=1（vendor 的 (B S) 折叠在 B>1 时 repeat 顺序有歧义）")
        if not (am.enable_mouse and am.enable_keyboard):
            raise ValueError("本实现假定 mouse/keyboard 两分支均启用（与训练配置一致）")
        r, ws = am.vae_time_compression_ratio, am.windows_size
        pad_t = r * ws
        T = mouse.shape[1]
        if (T - 1) % r != 0:
            # vendor 有 N%r==0 的第二分支（pad_t-4），93 帧语料恒走 (N-1)%r==0 分支；
            # 只镜像用到的那条，另一条走到即报错而不是静默走样。
            raise ValueError(f"clip 帧数 {T} 不满足 (N-1)%{r}==0——与冻结语料口径不符")

        # ---- mouse 窗口（vendor L213-218 同款 pad+切窗，全 tt 帧预计算） ----
        mp = torch.cat([mouse[:, 0:1, :].repeat(1, pad_t, 1), mouse], dim=1)
        self._mouse_windows = torch.stack(
            [mp[:, r * (i - ws) + pad_t: i * r + pad_t, :] for i in range(self.tt)],
            dim=1)

        # ---- keyboard 全序列 K/V（vendor L272-292 同款；K/V 只依赖动作故可预计算） ----
        kp = torch.cat([keyboard[:, 0:1, :].repeat(1, pad_t, 1), keyboard], dim=1)
        kp = kp.to(am.keyboard_embed[0].weight.dtype)
        ke = am.keyboard_embed(kp)
        gk = torch.stack(
            [ke[:, r * (i - ws) + pad_t: i * r + pad_t, :] for i in range(self.tt)],
            dim=1)
        gk = gk.reshape(gk.shape[0], gk.shape[1], -1)
        k, v = rearrange(am.keyboard_attn_kv(gk), "B L (K H D) -> K B L H D",
                         K=2, H=am.heads_num)
        k = am.key_attn_k_norm(k).to(v)
        dev = v.device
        kb_head_dim = am.proj_keyboard.in_features // am.heads_num
        fc, fs = am.get_rotary_pos_embed(
            self.tt, am.patch_size[1], am.patch_size[2], kb_head_dim, am.rope_dim_list)
        self._freqs_kb = (fc.to(dev), fs.to(dev))
        # vendor 对 q/k 用同一张表一次施加；此处 k（全 tt 帧）先行，q 在 forward 里按块切片
        k = apply_rotary_emb(k, k, self._freqs_kb, head_first=False)[0]
        self._kb_k, self._kb_v = k, v

        m_head_dim = am.proj_mouse.in_features // am.heads_num
        fc, fs = am.get_rotary_pos_embed(
            self.tt, am.patch_size[1], am.patch_size[2], m_head_dim, am.mouse_qk_dim_list)
        self._freqs_mouse = (fc.to(dev), fs.to(dev))

    def forward(self, x: torch.Tensor) -> torch.Tensor:
        if self._mouse is None:
            raise RuntimeError("先调 set_actions() 再前向")
        if self._block is None:
            raise RuntimeError("先调 set_block()——generator 的 forward_pre_hook 未接上？")
        am = self.action_module
        f0, nf = self._block
        B, tokens, _ = x.shape
        S = self.th * self.tw
        if B != 1:
            raise ValueError(f"只支持 batch=1，得到 B={B}")
        if tokens != nf * S:
            raise ValueError(
                f"block 隐状态 {tokens} token ≠ 当前块潜帧数 {nf}×{S}——"
                "current_start/输入 shape 推断有误（pre-hook 与 hook 不同步？）")

        # ---- mouse 分支（vendor L205-269 的分块版；注意力跨度=当前块 ← 近似所在） ----
        hidden = rearrange(x, "B (T S) C -> (B S) T C", T=nf, S=S)
        gm = self._mouse_windows[:, f0:f0 + nf].unsqueeze(-1).repeat(1, 1, 1, 1, S)
        gm = rearrange(gm, "b t w d s -> (b s) t (w d)")
        gm = am.mouse_mlp(torch.cat([hidden, gm], dim=-1))
        q, k, v = rearrange(am.t_qkv(gm), "B L (K H D) -> K B L H D",
                            K=3, H=am.heads_num)
        q = am.img_attn_q_norm(q).to(v)
        k = am.img_attn_k_norm(k).to(v)
        fc, fs = self._freqs_mouse
        q, k = apply_rotary_emb(q, k, (fc[f0:f0 + nf], fs[f0:f0 + nf]),
                                head_first=False)
        attn = _attention(q, k, v)
        attn = rearrange(attn, "(b S) T h d -> b (T S) (h d)", b=B)
        # vendor L267 的 rearrange(x, "(B S) T C -> B (T S) C") 在 B=1 时是恒等映射
        hidden_states = x + am.proj_mouse(attn)

        # ---- keyboard 分支（vendor L271-336；q=当前块+绝对位置 RoPE，K/V=全序列） ----
        q = rearrange(am.mouse_attn_q(hidden_states), "B L (H D) -> B L H D",
                      H=am.heads_num)
        q = am.key_attn_q_norm(q).to(self._kb_v)
        q = rearrange(q, "B (T S) H D -> (B S) T H D", S=S)
        fc, fs = self._freqs_kb
        q = apply_rotary_emb(q, q, (fc[f0:f0 + nf], fs[f0:f0 + nf]),
                             head_first=False)[0]
        k = self._kb_k.repeat(S, 1, 1, 1)
        v = self._kb_v.repeat(S, 1, 1, 1)
        attn = _attention(q, k, v)
        attn = rearrange(attn, "(B S) T H D -> B (T S) (H D)", S=S)
        return hidden_states + am.proj_keyboard(attn)


# =====================================================================
# --selfcheck：与 vendor 整段前向的逐位一致性（随机权重，无需 GPU/ckpt）
# =====================================================================

def run_selfcheck(device_str: str) -> None:
    device = torch.device(device_str)
    dtype = torch.bfloat16 if device.type == "cuda" else torch.float32
    torch.manual_seed(0)
    grid = (24, 15, 26)              # kda 实配：480x832 → 潜 30x52 → token 15x26
    dim = 256                        # 缩小 img 维度提速；分支结构与 3072 完全同构
    am = build_action_module(img_hidden_size=dim).to(device=device, dtype=dtype).eval()
    ref = ActionConditioner(am, grid=grid)       # 与 blk 共享同一份权重
    blk = BlockwiseActionConditioner(am, grid=grid)

    mouse = torch.randn(1, 93, 2, device=device, dtype=dtype)
    keyboard = (torch.rand(1, 93, 6, device=device) > 0.5).to(dtype)
    ref.set_actions(mouse, keyboard)
    blk.set_actions(mouse, keyboard)
    x = torch.randn(1, 24 * 390, dim, device=device, dtype=dtype)

    with torch.no_grad():
        want = ref(x)                # vendor ActionModule.forward 整段
        blk.set_block(0, 24)
        got = blk(x)                 # 分块实现、整段一次 → 应逐位一致
    tol = dict(atol=2e-2, rtol=2e-2) if dtype == torch.bfloat16 else dict(atol=1e-5, rtol=1e-5)
    diff = (want.float() - got.float()).abs().max().item()
    scale = want.float().abs().max().item()
    assert torch.allclose(want.float(), got.float(), **tol), \
        f"整段一致性失败: max|Δ|={diff:.3e} (量级 {scale:.3e})"
    print(f"[selfcheck] 整段(f0=0,nf=24)与 vendor forward 一致: max|Δ|={diff:.3e} "
          f"(输出量级 {scale:.3e}, dtype={dtype})")

    # 分块调用的结构检查：8 帧 × 3 块，形状与可加性
    with torch.no_grad():
        for f0 in (0, 8, 16):
            blk.set_block(f0, 8)
            out = blk(x[:, f0 * 390:(f0 + 8) * 390])
            assert out.shape == (1, 8 * 390, dim)
    print("[selfcheck] 分块(f0∈{0,8,16}, nf=8)形状/切片通过")

    # keyboard 全跨度确证：截断动作(只留前 32 帧、其余置零)应改变首块输出 ——
    # 若 K/V 只用了块内动作，该扰动不会传导进 f0=0 的块
    mouse2, kb2 = mouse.clone(), keyboard.clone()
    kb2[:, 32:] = 1.0 - kb2[:, 32:]
    blk2 = BlockwiseActionConditioner(am, grid=grid)
    blk2.set_actions(mouse2, kb2)
    with torch.no_grad():
        blk.set_block(0, 8)
        a = blk(x[:, :8 * 390])
        blk2.set_block(0, 8)
        b = blk2(x[:, :8 * 390])
    assert not torch.allclose(a.float(), b.float(), atol=1e-6), \
        "keyboard K/V 未覆盖全序列——未来帧动作的扰动没有传导进首块"
    print("[selfcheck] keyboard 分支全跨度 K/V 确证通过")
    print("[selfcheck] PASS")


# =====================================================================
# 主流程（VM / LongLive 环境）
# =====================================================================

def _latest_phase1_lora() -> str:
    cands = sorted(Path.home().glob("runs/phase1/checkpoint_model_*/model.pt"))
    if not cands:
        raise SystemExit("找不到 Phase 1 LoRA checkpoint（~/runs/phase1/checkpoint_model_*/model.pt）")
    return str(cands[-1])


def _clean_state_dict_keys(sd: dict) -> dict:
    """剥掉 FSDP/DDP 包裹可能引入的前缀（FULL_STATE_DICT 正常无前缀，兜底不害）。"""
    out = {}
    for key, val in sd.items():
        for pre in ("_fsdp_wrapped_module.", "_checkpoint_wrapped_module.", "module."):
            while key.startswith(pre):
                key = key[len(pre):]
        out[key] = val
    return out


def _save_prompts_txt(prompts: list[str], path: str) -> None:
    """与 inference.py::save_prompts_to_txt 同款（连续相同 prompt 合并）。"""
    with open(path, "w", encoding="utf-8") as f:
        if not prompts:
            return
        cur, idxs = prompts[0], [0]
        for i, p in enumerate(prompts[1:], 1):
            if p == cur:
                idxs.append(i)
            else:
                f.write(f"[{','.join(map(str, idxs))}] {cur}\n")
                cur, idxs = p, [i]
        f.write(f"[{','.join(map(str, idxs))}] {cur}\n")


def main() -> None:
    ap = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--action-ckpt", help="phase2 臂 checkpoint（{'action_conditioner',...}）")
    ap.add_argument("--lora-ckpt", default=None,
                    help="Phase 1 LoRA checkpoint（默认取 ~/runs/phase1 最新，phase2.sh 同款）")
    ap.add_argument("--action-source", choices=("native", "pseudo", "pseudo_conf"),
                    help="动作来源臂（决定 action 目录与 confidence 调制，phase2.sh 同表）")
    ap.add_argument("--allow-cross-arm", action="store_true",
        help="放行「评测动作源 != 训练动作源」：三臂统一真值协议需要它"
             "（34 号文 §5）；不加则维持原护栏直接报错")
    ap.add_argument("--out", help="rollout 输出目录（≙ phase0full/rollouts）")
    ap.add_argument("--limit", type=int, default=None, help="只跑前 N 个 clip（冒烟用）")
    ap.add_argument("--device", default="cuda:0")
    ap.add_argument("--base-config",
                    default=str(Path.home() / "runs/phase0full/inference_i2v.yaml"),
                    help="推理协议基线（与 phase0full 完全同一份 yaml）")
    ap.add_argument("--data-path", default=str(Path.home() / "data/holdout_flat"))
    ap.add_argument("--action-root", default=str(Path.home() / "data/plane"),
                    help="含 action/ 与 action_pseudo/ 的根目录")
    ap.add_argument("--longlive-dir", default=str(Path.home() / "LongLive"))
    ap.add_argument("--mouse-scale", type=float, default=200.0,
                    help="训练 yaml 缺失时的回退值；出处=phase2.sh PHASE2_MOUSE_SCALE")
    ap.add_argument("--blocks-fraction", type=float, default=0.5)
    ap.add_argument("--seed", type=int, default=None, help="默认沿用 base-config 的 seed")
    # 显存杠杆（见模块 docstring；评测正跑全关）
    ap.add_argument("--offload-dit", action="store_true")
    ap.add_argument("--decode", choices=("inline", "swap", "cpu"), default="inline")
    ap.add_argument("--lean-neg-kv", action=argparse.BooleanOptionalAction, default=True)
    ap.add_argument("--min-free-gb", type=float, default=0.0)
    ap.add_argument("--selfcheck", action="store_true",
                    help="只跑数学一致性自检（无需 LongLive/ckpt）")
    args = ap.parse_args()

    if args.selfcheck:
        run_selfcheck(args.device if torch.cuda.is_available() else "cpu")
        return
    for req in ("action_ckpt", "action_source", "out"):
        if getattr(args, req) is None:
            ap.error(f"--{req.replace('_', '-')} 为必填（除非 --selfcheck）")

    # ---- LongLive 环境注入（tv shim 必须先于一切项目 import，README 踩坑 1） ----
    ll = str(Path(args.longlive_dir).expanduser())
    assert os.path.isdir(ll), f"LongLive 目录不存在: {ll}"
    sys.path.insert(0, ll)
    os.chdir(ll)                     # configs/ 等相对路径按上游习惯以 LongLive 为锚
    import tv_video_shim  # noqa: F401
    import numpy as np
    from omegaconf import OmegaConf
    from torchvision.io import write_video
    from torch.utils.data import DataLoader, SequentialSampler
    import peft
    from pipeline import CausalDiffusionInferencePipeline
    from utils.config import normalize_config, section_get, wan_default_config
    from utils.dataset import MultiVideoConcatDataset, multi_video_collate_fn
    from utils.lora_utils import configure_lora_for_model
    from utils.memory import DynamicSwapInstaller
    from utils.misc import set_seed
    from utils.nvfp4_checkpoint import unwrap_generator_state_dict

    device = torch.device(args.device)
    torch.cuda.set_device(device)
    torch.set_grad_enabled(False)

    if args.min_free_gb > 0:
        free_b, _ = torch.cuda.mem_get_info(device)
        if free_b / 1024**3 < args.min_free_gb:
            raise SystemExit(
                f"GPU 空闲 {free_b / 1024**3:.1f} GB < --min-free-gb {args.min_free_gb}"
                "——推迟冒烟（纪律：不与训练臂抢显存）")

    # ---- 训练配置即出处：从臂目录的 train_phase2.yaml 读实际训练参数并交叉核对 ----
    action_dir = {"native": "action", "pseudo": "action_pseudo",
                  "pseudo_conf": "action_pseudo"}[args.action_source]
    action_dir = os.path.join(os.path.expanduser(args.action_root), action_dir)
    conf_mod = args.action_source == "pseudo_conf"
    mouse_scale, blocks_fraction = args.mouse_scale, args.blocks_fraction
    train_yaml = Path(args.action_ckpt).expanduser().resolve().parent.parent / "train_phase2.yaml"
    if train_yaml.is_file():
        tc = OmegaConf.load(train_yaml).action_module
        if os.path.realpath(tc.action_dir) != os.path.realpath(action_dir):
            _msg = (f"--action-source {args.action_source} 推出 {action_dir}，但训练 yaml 记录 "
                    f"{tc.action_dir}——臂对错了")
            if not getattr(args, "allow_cross_arm", False):
                raise SystemExit(_msg)
            # 跨臂评测＝三臂统一真值协议（34 号文 §5）：所有臂用同一条真值动作流
            # 条件化，才能把臂间差异归因到模型而非标签流。显式放行并留审计痕迹。
            print(f"[protocol] 跨臂评测已显式放行：{_msg}", flush=True)
            print(f"[protocol] 条件化动作流={action_dir}（统一真值）｜"
                  f"训练动作流={tc.action_dir}｜conf_mod={conf_mod}", flush=True)
        if bool(tc.confidence_modulate) != conf_mod:
            _msg2 = (f"confidence_modulate 不一致: 训练 yaml={tc.confidence_modulate}, "
                     f"--action-source 推出 {conf_mod}")
            if not getattr(args, "allow_cross_arm", False):
                raise SystemExit(_msg2)
            # 统一真值协议下这是**预期**差异：A3 训练期键盘值被 c_t 调制（均值 0.969），
            # 评测统一喂真值＝置信度恒 1.0（在其分布上沿，偏移 ~3%，34 号文 §5 已记录）。
            # conf 调制在数据侧实现（patch_longlive_phase2），非架构组件，权重不会失配。
            print(f"[protocol] 跨臂评测已显式放行：{_msg2}", flush=True)
        mouse_scale = float(tc.mouse_scale)
        blocks_fraction = float(tc.blocks_fraction)
        print(f"[cfg] 训练 yaml {train_yaml} → mouse_scale={mouse_scale} "
              f"blocks_fraction={blocks_fraction} conf_mod={conf_mod}")
    else:
        print(f"[cfg][warn] 找不到 {train_yaml}，用 CLI 值 mouse_scale={mouse_scale} "
              f"blocks_fraction={blocks_fraction}（合成/搬移过的 ckpt 才会走到这）")

    # ---- 推理协议：phase0full 的 yaml 原样为基线 ----
    config = normalize_config(OmegaConf.load(args.base_config))
    config.distributed = False
    out_dir = os.path.expanduser(args.out)
    os.makedirs(out_dir, exist_ok=True)
    config.output_folder = out_dir
    config.data_path = os.path.expanduser(args.data_path)
    seed = args.seed if args.seed is not None else int(getattr(config, "seed", 0))
    set_seed(seed)
    assert bool(getattr(config, "i2v", False)), "base-config 必须是 i2v 协议（phase0full 同款）"
    guidance = float(section_get(config, "inference", "guidance_scale",
                                 getattr(config, "guidance_scale", 1.0)))
    assert guidance == 1.0 or not args.lean_neg_kv, \
        "--lean-neg-kv 只在 guidance_scale==1.0（无 CFG）时是安全的"
    assert not bool(section_get(config, "inference", "streaming_vae",
                                getattr(config, "streaming_vae", False))), \
        "streaming_vae 协议未接（phase0full 为 false）"

    # ---- 管线构建（镜像 inference.py 的非量化路径：底座 → LoRA → merge → bf16/设备） ----
    pipeline = CausalDiffusionInferencePipeline(config, device=device)
    gen_ckpt = getattr(config, "generator_ckpt", None)
    assert gen_ckpt, "base-config 缺 generator_ckpt"
    print(f"[load] base generator: {gen_ckpt}")
    ckpt = torch.load(gen_ckpt, map_location="cpu")
    pipeline.generator.load_state_dict(
        unwrap_generator_state_dict(ckpt, use_ema=False), strict=True)
    del ckpt

    lora_ckpt_path = os.path.expanduser(args.lora_ckpt) if args.lora_ckpt else _latest_phase1_lora()
    adapter_cfg = OmegaConf.create(
        {"type": "lora", "rank": 64, "alpha": 64, "dropout": 0.0})  # phase1.sh/phase2.sh 同款
    print(f"[load] LoRA (merge): {lora_ckpt_path}")
    pipeline.generator.model = configure_lora_for_model(
        pipeline.generator.model, model_name="generator",
        lora_config=adapter_cfg, is_main_process=True)
    lora_ckpt = torch.load(lora_ckpt_path, map_location="cpu")
    lora_state = lora_ckpt.get("generator_lora", lora_ckpt)
    rank_seen = next(v.shape[0] for k, v in lora_state.items() if "lora_A" in k)
    assert rank_seen == adapter_cfg.rank, \
        f"LoRA rank 不符: ckpt={rank_seen}, adapter={adapter_cfg.rank}"
    peft.set_peft_model_state_dict(pipeline.generator.model, lora_state)
    del lora_ckpt, lora_state
    pipeline.generator.model = pipeline.generator.model.merge_and_unload(safe_merge=True)
    pipeline.is_lora_enabled = False   # merge 后命名走 _regular（与 phase0full 一致）
    pipeline.is_lora_merged = True

    pipeline = pipeline.to(dtype=torch.bfloat16)
    DynamicSwapInstaller.install_model(pipeline.text_encoder, device=device)  # T5 常驻 CPU
    if args.offload_dit:
        # 整个 wrapper 一起装：wrapper 自身若有 buffer 也按访问流式上卡
        DynamicSwapInstaller.install_model(pipeline.generator, device=device)
        print("[mem] DiT 权重驻 CPU、按访问流式上 GPU（--offload-dit）")
    else:
        pipeline.generator.to(device=device)
    vae_on_gpu = args.decode != "cpu"
    if vae_on_gpu:
        pipeline.vae.to(device=device)
    else:
        pipeline.vae.to(dtype=torch.float32)   # CPU 上 bf16 conv3d 慢且缺核，转 fp32
    pipeline.generator.model.eval().requires_grad_(False)

    if args.lean_neg_kv:
        # 负分支 KV 分配在 CPU：use_cfg==False 时推理路径从不读写其数据
        #（causal_diffusion_inference.py 中 kv_cache_neg 的所有访问都在 use_cfg 分支内）
        _orig_init_kv = pipeline._initialize_kv_cache

        def _lean_init_kv(batch_size, dtype, device):
            gpu_dev = device
            _orig_init_kv(batch_size=batch_size, dtype=dtype, device=torch.device("cpu"))
            for blk in pipeline.kv_cache_pos:
                for key, val in list(blk.items()):
                    if torch.is_tensor(val):
                        blk[key] = val.to(gpu_dev)
        pipeline._initialize_kv_cache = _lean_init_kv
        print("[mem] kv_cache_neg 常驻 CPU（--lean-neg-kv，省 ~4.6GB）")

    # ---- 动作条件器：grid 从 model.patch_size 现算（与训练补丁同一算法） ----
    base = pipeline._dit_model
    blocks = list(base.blocks)
    dim = int(base.dim)
    ps = base.patch_size
    pt, ph, pw = ps if isinstance(ps, (tuple, list)) else (1, ps, ps)
    shape = list(config.image_or_video_shape)          # [B, T_lat, C, H_lat, W_lat]
    grid = (int(shape[1]) // int(pt), int(shape[3]) // int(ph), int(shape[4]) // int(pw))
    assert grid[1] * grid[2] == pipeline.frame_seq_length, \
        f"grid {grid} 与 frame_seq_length {pipeline.frame_seq_length} 不符"

    cond = BlockwiseActionConditioner(build_action_module(img_hidden_size=dim), grid=grid)
    ac = torch.load(os.path.expanduser(args.action_ckpt), map_location="cpu")
    if "action_conditioner" not in ac:
        raise SystemExit(
            f"{args.action_ckpt} 不是 action_conditioner checkpoint，keys={list(ac.keys())}")
    cond.load_state_dict(_clean_state_dict_keys(ac["action_conditioner"]), strict=True)
    step = ac.get("step", "?")
    del ac
    cond = cond.to(device=device, dtype=torch.bfloat16).eval()
    n_attach = max(1, int(round(len(blocks) * blocks_fraction)))
    cond.attach(blocks[:n_attach])
    n_params = sum(p.numel() for p in cond.parameters())
    print(f"[ActionModule] {n_params / 1e6:.1f}M params (step {step}), grid={grid}, "
          f"attached to first {n_attach}/{len(blocks)} DiT blocks, "
          f"action_dir={action_dir}, mouse_scale={mouse_scale}, conf_mod={conf_mod}")

    def _set_block_prehook(_module, _args, kwargs):
        cs = kwargs.get("current_start")
        x = kwargs.get("noisy_image_or_video")
        assert cs is not None and x is not None, \
            "generator 调用缺 current_start/noisy_image_or_video kwarg——上游管线变了？"
        cond.set_block(int(cs) // pipeline.frame_seq_length, int(x.shape[1]))
    pipeline.generator.register_forward_pre_hook(_set_block_prehook, with_kwargs=True)

    # ---- 数据集：与 inference.py 的 i2v video-layout 分支逐参一致 ----
    model_name = config.model_kwargs.model_name
    spatial = wan_default_config[model_name]["spatial_compression_ratio"]
    temporal = wan_default_config[model_name]["temporal_compression_ratio"]
    num_output_frames = int(getattr(config, "num_output_frames", shape[1]))
    total_frames = (num_output_frames - 1) * temporal + 1
    nfpb = getattr(config, "num_frame_per_block", 8)
    dataset = MultiVideoConcatDataset(
        data_dir=config.data_path,
        video_size=(shape[3] * spatial, shape[4] * spatial),
        total_frames=total_frames,
        deterministic=True,
        num_frame_per_block=nfpb,
        temporal_compression_ratio=temporal,
        target_fps=24 if "5B" in model_name else 16,
        allow_padding=getattr(config, "allow_padding", False),
        min_latent_frames=getattr(config, "min_latent_frames", 0),
        single_video_only=getattr(config, "uniform_prompt", False),
        independent_first_frame=getattr(config, "independent_first_frame", False),
        return_image=True,
        max_chunks_per_shot=getattr(config, "max_chunks_per_shot", 0),
        scene_cut_prefix=getattr(config, "scene_cut_prefix", "The scene transitions. "),
    )
    loader = DataLoader(dataset, batch_size=1, sampler=SequentialSampler(dataset),
                        num_workers=0, drop_last=False, collate_fn=multi_video_collate_fn)

    # ---- manifest：行号=idx 的映射基础；与 phase0full 的逐行核对 ----
    manifest = sorted(os.listdir(os.path.join(config.data_path, "video")))
    manifest_path = Path(out_dir).parent / "manifest.txt"
    manifest_path.write_text("\n".join(manifest) + "\n")
    p0_manifest = Path.home() / "runs/phase0full/manifest.txt"
    if p0_manifest.is_file():
        assert p0_manifest.read_text().splitlines() == manifest, \
            "manifest 与 phase0full 不一致——idx 映射会串位，先查 holdout_flat"
        print(f"[manifest] {len(manifest)} clips，与 phase0full 逐行一致")

    total = len(manifest) if args.limit is None else min(args.limit, len(manifest))
    print(f"[run] rollout {total}/{len(manifest)} clips → {out_dir} (decode={args.decode})")

    _pairs = []
    for i, batch in enumerate(loader):
        if i >= total:
            break
        idx = int(batch["idx"].item())
        assert idx == i, f"数据集顺序与 manifest 不符: batch idx={idx}, loop i={i}"

        # -- 动作对位：segments_meta 的真实路径 → <sess>/<stem>.npz --
        # 动作按整 clip（93 行）喂，与 rollout 从首帧起重生成整段对齐；segment 切分
        # 结构（单段/多段）不影响对位，只须保证条目内只有这一个视频。
        segs = batch.get("segments_meta")
        assert segs is not None, "batch 缺 segments_meta——utils/dataset.py 未打 phase2 补丁"
        paths = {p for p, _, _ in segs[0]}
        assert len(paths) == 1, f"holdout_flat 条目含多个视频？{paths}"
        rp = os.path.realpath(paths.pop())
        stem = os.path.splitext(os.path.basename(rp))[0]
        sess = os.path.basename(os.path.dirname(rp))
        # dataset 枚举序 = 未排序 iterdir()（utils/dataset.py L478），与 sorted manifest
        # 是稳定置换——不按位置断言；记录真实映射 idx→clip，收尾做集合级核对。
        _pairs.append((idx, f"{sess}-{stem}"))

        npz_path = os.path.join(action_dir, sess, stem + ".npz")
        with np.load(npz_path) as z:
            mouse_np = np.asarray(z["mouse"], dtype=np.float32)
            keyboard_np = np.asarray(z["keyboard"], dtype=np.float32)
            # 预处理与训练侧 _load_action_npz/_set_batch_actions 逐行对应
            if conf_mod:
                if "keyboard_confidence" not in z.files:
                    print(f"[warn] {npz_path} 无 keyboard_confidence（训练同款静默跳过）")
                else:
                    keyboard_np = keyboard_np * np.asarray(
                        z["keyboard_confidence"], dtype=np.float32)
        assert mouse_np.shape == (total_frames, 2) and keyboard_np.shape[0] == total_frames, \
            f"{npz_path} 形状不符: mouse={mouse_np.shape}, keyboard={keyboard_np.shape}"
        mouse = (torch.from_numpy(mouse_np)[None] / mouse_scale).to(device, torch.bfloat16)
        keyboard = torch.from_numpy(keyboard_np)[None].to(device, torch.bfloat16)
        cond.set_actions(mouse, keyboard)

        # -- 首帧 I2V（镜像 inference.py 主循环） --
        block_prompts = list(batch["prompts"][0])
        image = batch["image"].to(device=device, dtype=torch.bfloat16)
        if image.ndim == 4:
            image = image.unsqueeze(2)
        initial_latent = pipeline.vae.encode_to_latent(image).to(device, torch.bfloat16) \
            if vae_on_gpu else \
            pipeline.vae.encode_to_latent(image.cpu().float()).to(device, torch.bfloat16)
        noise = torch.randn([1, num_output_frames, shape[2], shape[3], shape[4]],
                            device=device, dtype=torch.bfloat16)
        want_latents = args.decode in ("swap", "cpu")
        with torch.inference_mode():
            generated = pipeline.inference(
                noise=noise, text_prompts=[block_prompts],
                initial_latent=initial_latent, return_latents=want_latents)
        if want_latents:
            if args.decode == "swap":
                pipeline.clear_cache()             # 先还显存再 decode；数学与 inline 相同
                torch.cuda.empty_cache()
                video = pipeline.vae.decode_to_pixel(generated)
            else:
                video = pipeline.vae.decode_to_pixel(generated.cpu().float())
            video = (video * 0.5 + 0.5).clamp(0, 1)
        else:
            video = generated
        frames = rearrange(video, "b t c h w -> b t h w c").cpu().float() * 255.0
        pipeline.vae.model.clear_cache()

        name = f"rank0-{idx}-0_regular"
        fps = 24 if "5B" in model_name else 16          # inference.py 同款
        write_video(os.path.join(out_dir, f"{name}.mp4"), frames[0], fps=fps)
        _save_prompts_txt(block_prompts, os.path.join(out_dir, f"{name}_prompts.txt"))
        print(f"[{i + 1}/{total}] {name}.mp4  ← {sess}/{stem} "
              f"(|mouse| p99={np.percentile(np.abs(mouse_np), 99):.0f})")

    _names = [n for _, n in _pairs]
    assert len(set(_names)) == len(_names), "pairs 有重复 clip"
    assert set(_names) <= set(manifest), "pairs 出现 manifest 外的 clip"
    if len(_names) == len(manifest):
        assert set(_names) == set(manifest), "pairs 覆盖不全"
    (Path(out_dir).parent / "pairs.txt").write_text(
        "\n".join(f"{i}\t{n}" for i, n in _pairs) + "\n")
    print(f"[pairs] {len(_pairs)} 条真实映射已写 pairs.txt")
    print(f"== done: {total} rollouts → {out_dir} ==")


if __name__ == "__main__":
    main()

"""WM 0.1 · 动作模块的接线层: 把 vendor 的 MG3 `ActionModule` 挂到 LongLive 的 DiT 上.

`vendor/action_module.py` 是从 Matrix-Game 3.0 原样引入的(Apache-2.0, 见
`vendor/README.md`)。本模块**不改它的算法**, 只做三件接线的事:

1. **从冻结的动作空间构造它** —— 维度不是手填的常数, 是从 `ActionSpace` 读出来的。
   词表宽度与 `keyboard_dim_in` 不一致时 `ActionSpace.__post_init__` 已经会抛,
   于是"改了词表忘了改模块"这条路被堵死在构造期而不是训练期。
2. **把帧数约束翻译成人话** —— 上游用 `assert` 表达
   `(N_frames-1) % 4 == 0 or N_frames % 4 == 0`, 断言失败只会给一个裸 AssertionError。
   我们的 clip 是 93 帧(`(93-1)%4==0` ✅, 得 24 个潜帧, 与 LongLive 的
   `1+(93-1)//4=24` **独立算出同一个数**), 但换 clip 长度是很容易发生的事。
3. **提供残差挂载点** —— 上游的 forward 返回"加过残差的隐状态", 所以它是一个
   可以插在任意 DiT block 之后的 hook。MG2 的做法是**只在前一半 block 上挂**
   (报告里写明这是为了达到 25 fps 的实时预算), 这里把它做成参数而不是写死。

## 为什么是 hook 而不是改 LongLive 的模型定义

LongLive 的 `CausalWanAttentionBlock` 是 LoRA 的目标模块
(`utils/lora_utils.py::configure_lora_for_model` 按类名找它)。直接改类定义会让
LoRA 的目标集合跟着变, 而我们要的恰恰是**两者独立**: Phase 1 只训 LoRA(动作模块
不在图里), Phase 2 冻结 LoRA 只训动作模块。用 forward hook 挂载, 两个阶段共用
同一份底座代码, 且 LoRA 的目标集合不受影响。
"""
from __future__ import annotations

import torch

from .action_space import (
    BASE_KEYBOARD_DIM, BASE_MOUSE_DIM, BASE_VAE_TIME_COMPRESSION,
    BASE_WINDOW_SIZE, ActionSpace,
)
from .vendor.action_module import ActionModule


def latent_frames(n_frames: int,
                  compression: int = BASE_VAE_TIME_COMPRESSION) -> int:
    """clip 帧数 → 潜帧数, 并把上游的裸 assert 换成能读懂的错误。"""
    if (n_frames - 1) % compression == 0:
        return (n_frames - 1) // compression + 1
    if n_frames % compression == 0:
        return n_frames // compression
    raise ValueError(
        f"clip 帧数 {n_frames} 不满足动作模块的约束: "
        f"(N-1) % {compression} == 0 或 N % {compression} == 0。"
        f"93 帧是合法的((93-1)%4==0 → 24 个潜帧), 且与 LongLive 的块结构"
        f"(1+(93-1)//4 = 24)对得上 —— 换 clip 长度要同时满足两边。")


def build_action_module(*, img_hidden_size: int, heads_num: int = 16,
                        space: ActionSpace | None = None,
                        **kwargs) -> ActionModule:
    """按冻结的动作空间构造 MG3 ActionModule。"""
    space = space or ActionSpace()
    # 上游的隐藏耦合: rope 维度表必须正好切满一个头。不满足时它只给一句
    # "sum(rope_dim_list) should equal to head_dim" 的裸 assert, 而真正的原因
    # 是 heads_num 与 mouse_hidden_dim 不匹配 —— 在这里先把话说清楚。
    mouse_hidden = kwargs.get("mouse_hidden_dim", 1024)
    qk_dims = kwargs.get("mouse_qk_dim_list", [8, 28, 28])
    if mouse_hidden % heads_num or mouse_hidden // heads_num != sum(qk_dims):
        raise ValueError(
            f"heads_num={heads_num} 与 mouse_hidden_dim={mouse_hidden} 不匹配: "
            f"head_dim={mouse_hidden // heads_num if heads_num else 'n/a'} 必须等于 "
            f"sum(mouse_qk_dim_list)={sum(qk_dims)}。"
            f"上游默认 (1024, 16, [8,28,28]) 是自洽的; 要改 heads_num 就得同改另外两个。")
    return ActionModule(
        mouse_dim_in=BASE_MOUSE_DIM,
        keyboard_dim_in=len(space.keyboard),      # 不是常数, 是词表宽度
        img_hidden_size=img_hidden_size,
        heads_num=heads_num,
        vae_time_compression_ratio=BASE_VAE_TIME_COMPRESSION,
        windows_size=BASE_WINDOW_SIZE,
        **kwargs,
    )


class ActionConditioner(torch.nn.Module):
    """把动作模块挂在 DiT block 之后的残差 hook。

    用法(Phase 2, LoRA 已冻结):

        cond = ActionConditioner(build_action_module(img_hidden_size=1536),
                                 grid=(24, 30, 52))
        handles = cond.attach(dit_blocks[: len(dit_blocks) // 2])
        cond.set_actions(mouse, keyboard)     # 每个 batch 设一次
        ...  # 正常前向, 残差自动加进去
    """

    def __init__(self, action_module: ActionModule, *, grid: tuple[int, int, int]):
        super().__init__()
        self.action_module = action_module
        self.tt, self.th, self.tw = grid
        self._mouse: torch.Tensor | None = None
        self._keyboard: torch.Tensor | None = None

    def set_actions(self, mouse: torch.Tensor, keyboard: torch.Tensor) -> None:
        """mouse [B,T,2] · keyboard [B,T,6]，T 是**源 clip 帧数**不是潜帧数。"""
        if mouse.shape[:2] != keyboard.shape[:2]:
            raise ValueError(
                f"鼠标与键盘的 (B,T) 不一致: {tuple(mouse.shape[:2])} vs "
                f"{tuple(keyboard.shape[:2])} —— 它们必须来自同一条时间线")
        n_feats = latent_frames(mouse.shape[1])
        if n_feats != self.tt:
            raise ValueError(
                f"动作序列 {mouse.shape[1]} 帧 → {n_feats} 个潜帧, 而网格声明的是 "
                f"{self.tt}。**这不会自己报错**: 上游按较短的那个截断, 画面与动作"
                f"就此静默错位。")
        self._mouse, self._keyboard = mouse, keyboard

    def forward(self, x: torch.Tensor) -> torch.Tensor:
        if self._mouse is None:
            raise RuntimeError("先调 set_actions() 再前向")
        return self.action_module(
            x, self.tt, self.th, self.tw,
            mouse_condition=self._mouse, keyboard_condition=self._keyboard)

    def attach(self, blocks) -> list:
        """在每个 block 的输出上加残差, 返回 handle 列表(调用方负责 remove)。

        只挂**前一半** block 是 MG2 的做法(报告写明是为了 25 fps 的实时预算),
        所以挂哪些由调用方决定 —— 这里不替它选。
        """
        def hook(_module, _inp, out):
            if isinstance(out, tuple):
                return (self.forward(out[0]), *out[1:])
            return self.forward(out)
        return [b.register_forward_hook(hook) for b in blocks]


__all__ = ["ActionConditioner", "build_action_module", "latent_frames",
           "BASE_KEYBOARD_DIM"]

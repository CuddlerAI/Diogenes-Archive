"""[vendor shim] 只提供 action_module.py 需要的两个可用性标志.

上游 `Matrix-Game-3/wan/modules/attention.py` 是一整套 flash-attn / sage-attn /
sliding-tile 的后端选择器, 而 `action_module.py` 只用到其中两个布尔量, 用来决定
走 flash 还是走 `torch.nn.functional.scaled_dot_product_attention`。

我们恒定走 SDPA:
- 上游那条 SDPA 分支是**原作者写的**, 不是我们加的回退, 所以数值路径没有被我们改动;
- 它让整个模块在 CPU 上可构造、可前向、可测形状 —— 而这正是"租卡之前把能验的都验掉"
  的前提;
- A100(SM80)上 SDPA 会走 PyTorch 自己的 flash 内核, 性能差距在 LoRA 微调这个量级上
  不是瓶颈。真要上 flash-attn, 把这两个标志改 True 并装上对应包即可, 无需改算法。
"""
FLASH_ATTN_3_AVAILABLE = False
FLASH_ATTN_2_AVAILABLE = False

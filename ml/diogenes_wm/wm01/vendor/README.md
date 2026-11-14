# vendor/ —— 从 Matrix-Game 3.0 原样引入的动作条件化模块

| 文件 | 来源 | 改动 |
|---|---|---|
| `action_module.py` | `SkyworkAI/Matrix-Game@51913b2bfa78a6d54de912b8348b1d318f4d36e2` `Matrix-Game-3/wan/modules/action_module.py` | **仅两处 import**（见下），算法逐字未改 |
| `posemb_layers.py` | 同上 `Matrix-Game-3/wan/modules/posemb_layers.py` | **仅去掉 3 处写死的 CUDA device**（见下） |
| `attention.py` | **不是上游文件**，我们写的 15 行 shim | 只提供上游需要的两个布尔标志 |

**许可证已按子项目核定**：官方 `SkyworkAI/Matrix-Game` 的 `Matrix-Game-3/README.md`
明确声明该子项目采用 Apache-2.0，并链接同目录 `LICENSE.txt`；此处引入的两个文件均来自该子项目。
已将官方许可证原文随本目录保存为 `LICENSE.txt`，不使用单仓根目录的其他许可替代它。
本次核验固定上游 revision `51913b2bfa78a6d54de912b8348b1d318f4d36e2`，来源 URL 与 SHA-256
见 `UPSTREAM-PROVENANCE.json`。与该 revision 的逐行对比确认，仅有下文列出的 import/CPU-device
改动和注释，没有替换算法。再分发时保留本 README、LICENSE.txt 和 NOTICE。

## 为什么 vendor 而不是 pip 依赖

上游没有发包，且 `Matrix-Game-3/` 只放推理码（`generate.py` + `pipeline/`），
没有训练入口。我们要的只是 `ActionModule` 这一个类，把整个仓库拉成子模块
是为了 300 行代码扛一整套推理栈。

## 两处 import 的改动，以及为什么它们不改变数值

1. `from .attention import FLASH_ATTN_*_AVAILABLE` → 指向本目录的 shim，两个标志恒 `False`。
   **上游自己就写了 `flash_attn_ops is None` 时走
   `torch.nn.functional.scaled_dot_product_attention` 的分支** —— 那条路径是原作者的，
   不是我们加的回退。所以我们只是选了它已有的两条路之一。
   代价是 A100 上不走 flash-attn 内核；在 LoRA 微调这个量级不是瓶颈。
   要开 flash：装上包并把两个标志改 `True`，算法一行不用动。
2. `from torch.nn.attention.flex_attention import flex_attention` 套了 try/except。
   **该符号在本文件里从未被调用**，但它的 import 会在 CPU / 旧 torch 上直接抛。
   套住之后整个模块在本机可构造、可前向、可测形状 —— 这是「租卡之前把能验的都验掉」的前提。

## 上游接口（我们据此对齐动作空间）

```
ActionModule(mouse_dim_in=2, keyboard_dim_in=6,
             vae_time_compression_ratio=4, windows_size=3, ...)
forward(x, tt, th, tw, mouse_condition, keyboard_condition, ...)
    x                 [B, tt*th*tw, C]   DiT 隐状态
    mouse_condition   [B, N_frames, 2]   连续二维位移
    keyboard_condition[B, N_frames, 6]   多热
    返回 x + 鼠标残差 + 键盘残差（同形状）
```

源码注释直接写着 `action module from https://arxiv.org/pdf/2501.08325`（GameFactory），
所以 MG2 / MG3 / DreamForge 共用同一接口 —— 移植是拼装不是研究。

**帧数约束**：`(N_frames - 1) % 4 == 0` 或 `N_frames % 4 == 0`。
我们的 clip 是 **93 帧**：`(93-1) % 4 == 0` ✅，得到 `N_feats = 24` 个潜帧，
而 LongLive 那侧 `1 + (93-1)//4 = 24` —— **两边独立算出同一个数**，
这是 93 这个长度同时满足两个底座约束的验证，不是巧合也不是凑的。

3. `posemb_layers.py` 里 **3 处** `device=torch.cuda.current_device()` 去掉了。
   这两个函数（`get_meshgrid_nd` / `get_1d_rotary_pos_embed`）造的是**坐标网格与频率表**
   ——纯粹的位置编码常量，与设备无关，建在 CPU 上再由调用方搬走数值完全一样。
   上游写死 CUDA 会让整个模块在无 GPU 的机器上连 import 都过不去。

## 本机（CPU）实测

```
构造 ok, 参数量: 13.7 M
forward ok: (1, 37440, 1536) -> (1, 37440, 1536)
把动作置零后输出改变, 平均绝对差 0.02524   ← 它真的在做动作条件化, 不是恒等
```

## 一处上游没写明的耦合

`mouse_hidden_dim / heads_num` 必须正好等于 `sum(mouse_qk_dim_list)`。
不满足时只给一句 `sum(rope_dim_list) should equal to head_dim` 的裸 assert，
而真正的原因是 `heads_num` 改了。上游默认 `(1024, 16, [8,28,28])` 是自洽的。
`wm01/action_module.py::build_action_module` 已把这条校验提到构造期并说清原因。

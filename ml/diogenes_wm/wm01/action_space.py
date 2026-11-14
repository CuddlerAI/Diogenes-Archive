"""WM 0.1 · 动作空间桥: DataMiner 语义动作 → Matrix-Game 家族的 ActionModule 接口.

这是**我们的数据和开源底座真正接触的那一层**。它值得单独一个模块和一组测试, 因为
两边的动作空间不是同一个形状, 而不匹配的方式是**静默**的 —— 投影错了不会报错,
只会让动作遵循指标低下来, 然后被误读成"数据不够"。

## 底座那侧的形状(一手核查, Matrix-Game-3/wan/modules/action_module.py)

    ActionModule(mouse_dim_in=2, keyboard_dim_in=6,
                 vae_time_compression_ratio=4, windows_size=3, ...)

源码注释直接标了 `action module from https://arxiv.org/pdf/2501.08325`(GameFactory),
所以 MG2 / MG3 / DreamForge 三家用的是同一个接口。两条通道:

- **鼠标: 连续二维** (`mouse_dim_in=2`) —— 不是离散方向档。这对我们是好消息:
  DataMiner 的 `pointerRawDx/Dy` 可以直接进, 不需要先离散化再丢掉幅度。
- **键盘: 6 通道多热** (`keyboard_dim_in=6`) —— 这是个**硬预算**。

## 我们这侧的形状

COD 语料实测 27 类语义动作 / 22 个物理 VK(research/33 §11 A4)。27 → 6 必须投影,
问题是投哪 6 个。答案不是挑出来的, 是数出来的: **支持度 ≥1% 的语义动作恰好有 6 个**,
其余 18 类全在 1% 以下(最少的 4 帧)。

| 通道 | 语义动作 | COD 帧占比 |
|---|---|---|
| 0 | `move.forward`   | 37.13% |
| 1 | `move.backward`  | 0.65% ⚠ |
| 2 | `move.left`      | 8.36% |
| 3 | `move.right`     | 8.06% |
| 4 | `combat.aim`     | 22.15% |
| 5 | `combat.primary_attack` | 4.36% |

`camera.yaw` / `camera.pitch`(39.9% / 35.1%)**不进键盘通道** —— 它们就是鼠标通道,
双写会让同一个物理量出现两次, 模型学到的是冗余而不是动作。这与
`ml/player/action_registry.py` 的 `CAMERA_SEMANTICS` 处理是同一条原则。

⚠ **`move.backward` 只有 0.65%。** 它进词表是因为 WASD 少一个方向就不是一套移动动作,
不是因为它有统计量。任何针对该通道的指标都必须与支持度并列报 —— 这正是 A4 那条
「22 个物理键里 0 个在每个会话都出现」的直接后果。

## 词表是冻结的

投影一旦进了训练就不能改: 换一个通道顺序会让已训的 LoRA 与 checkpoint 静默错位。
`WM01_KEYBOARD_V1` 因此带 name+version, 并由 `vocab_fingerprint()` 落进 checkpoint。
"""
from __future__ import annotations

from dataclasses import dataclass
from hashlib import sha256

import numpy as np

from diogenes_phase0.contract import ActionFrame
from diogenes_phase0.keys import SCANNED_VKEYS, VK_TO_INDEX

# 底座常量, 一手核查自 Matrix-Game-3/wan/modules/action_module.py 的默认值
BASE_MOUSE_DIM = 2
BASE_KEYBOARD_DIM = 6
BASE_VAE_TIME_COMPRESSION = 4
BASE_WINDOW_SIZE = 3

# 冻结词表 v1 —— 顺序即通道号, 不要重排
WM01_KEYBOARD_V1: tuple[str, ...] = (
    "move.forward",
    "move.backward",
    "move.left",
    "move.right",
    "combat.aim",
    "combat.primary_attack",
)

# camera.* 是鼠标通道, 不进键盘多热(双写 = 同一物理量出现两次)
CAMERA_SEMANTICS = ("camera.yaw", "camera.pitch")

# 物理 VK 回退表: 语料没有 `actions` 字段时(data_demo / gaming-500 / D2E 等
# 非 dm-1 语料)用它。COD 的 mapping 就是这套绑定, 见 DATASET_SPEC §4 的键频表。
VK_FALLBACK: dict[str, tuple[int, ...]] = {
    "move.forward": (87,),           # W
    "move.backward": (83,),          # S
    "move.left": (65,),              # A
    "move.right": (68,),             # D
    "combat.aim": (),                # 右键, 不在 VK 通道
    "combat.primary_attack": (),     # 左键, 不在 VK 通道
}


def vocab_fingerprint(vocab: tuple[str, ...] = WM01_KEYBOARD_V1) -> str:
    """词表指纹, 落进 checkpoint。通道重排会改变它, 于是错位不再静默。"""
    return sha256("\n".join(vocab).encode()).hexdigest()[:16]


# ---------------------------------------------------------------------------
# AB-P0 伪标签车道 (research/35 §3.3): SCANNED_VKEYS(12) ↔ wm01 六通道。
#
# IDM 头在 phase0 的冻结 12 键空间(`keys.py SCANNED_VKEYS`)里输出, 六个 wm01
# 通道各对应其中恰好一个物理键, 顺序与 WM01_KEYBOARD_V1 逐位对齐:
#
#     W(87)  S(83)  A(65)  D(68)  mouseR(2)  mouseL(1)
#
# WASD 与 VK_FALLBACK 同源(模块加载时校验, 两表漂移直接拒绝导入)。combat 两通道
# 是 data_demo 约定的鼠标键 VK 1/2 —— 注意一个实测事实(frames.parquet 前 5 万帧
# 0 帧含 VK1/2): **COD chunk 的 `keys` 列没有鼠标键**, 鼠标键只以语义 `actions`
# 出现。所以这两个 VK 位只在两处出现:
#   - 微调标签: ab_p0.finetune 从语义 actions 经本表补回 VK1/2 位, 否则这两个
#     通道得不到任何正监督, 伪标签永远盖不住 combat.aim(22.15% 帧占比);
#   - 伪标签: 微调后的头在 12 键空间输出, 经 scanned_to_wm01() 投回六通道。
# 被丢掉的 6 列(数字键 49..54)是有意的: wm01-keyboard/v1 没有武器栏通道。
# ---------------------------------------------------------------------------

AB_P0_CHANNEL_VKS: tuple[int, ...] = (87, 83, 65, 68, 2, 1)
# KeyError 在导入时爆 = SCANNED_VKEYS 词表漂移, 不允许静默
_AB_P0_COLS: list[int] = [VK_TO_INDEX[vk] for vk in AB_P0_CHANNEL_VKS]
AB_P0_VK_BY_SEMANTIC: dict[str, int] = dict(zip(WM01_KEYBOARD_V1, AB_P0_CHANNEL_VKS))

for _sem, _vk in AB_P0_VK_BY_SEMANTIC.items():
    _fb = VK_FALLBACK.get(_sem, ())
    if _fb and _vk not in _fb:
        raise ValueError(
            f"AB_P0_CHANNEL_VKS 与 VK_FALLBACK 在 {_sem!r} 上漂移了: "
            f"{_vk} vs {_fb} —— 两张表必须指同一个物理键")


def scanned_to_wm01(kb12: np.ndarray) -> np.ndarray:
    """[..., 12](SCANNED_VKEYS 序) → [..., 6](WM01_KEYBOARD_V1 序)。

    投影只是选列: 值可以是 0/1 标签, 也可以是概率/置信度, 语义都保持逐位对应。
    任意前导维(帧维/批维)原样保留。
    """
    arr = np.asarray(kb12)
    if arr.shape[-1] != len(SCANNED_VKEYS):
        raise ValueError(
            f"最后一维是 {arr.shape[-1]}, 需要 {len(SCANNED_VKEYS)}"
            "(SCANNED_VKEYS 序) —— 这不是 12 键空间的数组")
    return arr[..., _AB_P0_COLS]


@dataclass(frozen=True)
class ActionSpace:
    """WM 0.1 的冻结动作空间。"""
    name: str = "wm01-keyboard"
    version: str = "v1"
    keyboard: tuple[str, ...] = WM01_KEYBOARD_V1

    def __post_init__(self) -> None:
        if len(self.keyboard) != BASE_KEYBOARD_DIM:
            raise ValueError(
                f"底座的 keyboard_dim_in 是 {BASE_KEYBOARD_DIM}, 词表给了 "
                f"{len(self.keyboard)} 个通道。改词表长度必须同时改底座的 "
                "ActionModule 构造参数, 否则是静默错位。")
        bad = [s for s in self.keyboard if s in CAMERA_SEMANTICS]
        if bad:
            raise ValueError(f"camera.* 是鼠标通道, 不能进键盘多热: {bad}")

    @property
    def fingerprint(self) -> str:
        return vocab_fingerprint(self.keyboard)

    def keyboard_multihot(self, semantics: list[str] | None,
                          keys: list[int] | None = None) -> np.ndarray:
        """语义动作 → 6 维多热。语义缺失时回退到物理 VK。

        回退是**有损的**: `combat.aim` / `combat.primary_attack` 是鼠标键, VK 通道
        里根本没有它们, 所以非 dm-1 语料上这两个通道恒为 0。调用方要么接受
        (键盘类语料本就无此动作), 要么在 readiness 检查里被拦下。
        """
        out = np.zeros(BASE_KEYBOARD_DIM, dtype=np.float32)
        if semantics:
            idx = {s: i for i, s in enumerate(self.keyboard)}
            for s in semantics:
                j = idx.get(s)
                if j is not None:
                    out[j] = 1.0
            return out
        if keys:
            held = set(keys)
            for i, sem in enumerate(self.keyboard):
                if any(vk in held for vk in VK_FALLBACK.get(sem, ())):
                    out[i] = 1.0
        return out

    def mouse_delta(self, a: ActionFrame) -> np.ndarray:
        """连续二维鼠标位移。

        用 `raw_dx/raw_dy`(未截断)而不是 `pointer_dx/dy`(转换器已 clamp 到 ±200)。
        research/33 §11 A3-a 实测: COD 上 0.76% 的帧被封顶, 真值最高 903 counts
        = 封顶值的 4.5 倍, **且全是甩枪帧** —— 那正是相机动作信息最密的地方。
        `ActionFrame.from_json` 在没有 raw 字段的语料上会回退到 pointer, 所以这里
        拿到的永远是"该语料能给出的最好的那一路"。
        """
        return np.asarray([a.raw_dx, a.raw_dy], dtype=np.float32)


def encode_window(actions: list[ActionFrame],
                  semantics_per_frame: list[list[str]] | None = None,
                  space: ActionSpace | None = None) -> dict[str, np.ndarray]:
    """一个时间窗的动作编码。

    返回 `{"keyboard": [T, 6], "mouse": [T, 2]}`。底座那侧会按
    `vae_time_compression_ratio × windows_size` 分组, 所以 T 应当是
    `BASE_VAE_TIME_COMPRESSION` 的整数倍 —— 不是的话会在最后一组上补零,
    而补零读作"这几帧没动", 与"我们不知道"不可区分。这里硬报错而不是补零。
    """
    space = space or ActionSpace()
    T = len(actions)
    if T == 0:
        raise ValueError("空窗口")
    if T % BASE_VAE_TIME_COMPRESSION != 0:
        raise ValueError(
            f"窗口长度 {T} 不是 vae_time_compression_ratio="
            f"{BASE_VAE_TIME_COMPRESSION} 的整数倍; 补零会被读成'这几帧没动', "
            "请在切窗时对齐而不是在编码时补")
    sems = semantics_per_frame or [None] * T
    if len(sems) != T:
        raise ValueError(f"语义序列长度 {len(sems)} != 动作序列长度 {T}")
    kb = np.stack([space.keyboard_multihot(s, a.keys)
                   for a, s in zip(actions, sems)])
    mo = np.stack([space.mouse_delta(a) for a in actions])
    return {"keyboard": kb, "mouse": mo}

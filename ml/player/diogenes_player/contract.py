"""dm-1 数据契约的训练侧翻译：VK 键集与 μ-law 指针分箱。

契约来源（必须与之逐位一致，黄金测试锁死）：
- DataMiner/docs/02-input-jsonl-spec.md §3（键集、μ-law 参数 μ=8 / ref=200 / max_bin=10）
- data_demo/dataset/*/*/c*/p0.jsonl（pointerBins 字段 = 本模块 encode_bin 的真值）
"""
from __future__ import annotations

import math
from dataclasses import dataclass

import numpy as np

# ---------------------------------------------------------------------------
# 键集（data_demo/legacy 32 键投影集，升序 VK）。
#
# ⚠ 现实变更（research/07 §1）：DataMiner 已不用全局固定键集——键集是 per-game 的，
# 由 game-profile 的 `input_contract.keyboard_vks/mouse_buttons` 声明（COD=34 VK+5 鼠标键）。
# 本常量仅供 data_demo 存量数据与旧测试使用；新数据的键集从 game-profile 读取，
# 而 BC 训练的主目标是语义 actions（见 action_registry.py），keys 降为审计/辅助。
# ---------------------------------------------------------------------------
CONTRACT_VKEYS: tuple[int, ...] = (
    1, 2, 4,
    8, 9, 13, 16, 17, 18, 27, 32,
    49, 50, 51, 52, 53, 54,
    65, 66, 67, 68, 69, 70, 71,
    81, 82, 83, 84, 86, 87, 88, 90,
)
N_KEYS = len(CONTRACT_VKEYS)  # 32
_VK_TO_IDX = {vk: i for i, vk in enumerate(CONTRACT_VKEYS)}

VK_NAMES = {
    1: "mouseL", 2: "mouseR", 4: "mouseM",
    8: "Bksp", 9: "Tab", 13: "Enter", 16: "Shift", 17: "Ctrl", 18: "Alt",
    27: "Esc", 32: "Space",
    49: "1", 50: "2", 51: "3", 52: "4", 53: "5", 54: "6",
    65: "A", 66: "B", 67: "C", 68: "D", 69: "E", 70: "F", 71: "G",
    81: "Q", 82: "R", 83: "S", 84: "T", 86: "V", 87: "W", 88: "X", 90: "Z",
}

# ---------------------------------------------------------------------------
# μ-law 指针分箱。两套公式并存（research/07 §3 裁决）：
#
#   dm1-v2 (DataMiner 现行, 存储真值)：教科书 μ-law，normalized = |x|/ref
#       bin(x) = sign(x) * round( MAX_BIN * ln(1 + MU*|x|/REF) / ln(1 + MU) )
#       实现/文档三处一致（capture-core projection.rs foveated_bin），测试向量 120→8 / -14→-2 / ±200→±10
#
#   legacy (data_demo/HakkoLite 存量)：
#       bin(x) = sign(x) * round( MAX_BIN * ln(1 + MU*|x|) / ln(1 + MU*REF) )
#       data_demo 全量 p0.jsonl 锁定：60→8 / 120→9 / 180→10
#
# 两代刻度不可混用。ingest 一律丢弃存储 bins、从 raw dx/dy 用指定 scheme 重算。
# ---------------------------------------------------------------------------
MU = 8
REF = 200          # delta 钳位 ±REF
MAX_BIN = 10       # bins ∈ [-10, 10]
N_BINS = 2 * MAX_BIN + 1  # 21

DEFAULT_SCHEME = "dm1-v2"
_LEGACY_DENOM = math.log(1 + MU * REF)
_V2_DENOM = math.log(1 + MU)


def encode_bin(delta: float, scheme: str = DEFAULT_SCHEME) -> int:
    """raw 指针 delta → μ-law bin ∈ [-MAX_BIN, MAX_BIN]。scheme ∈ {dm1-v2, legacy}。"""
    if delta == 0:
        return 0
    mag = min(abs(float(delta)), float(REF))
    if scheme == "dm1-v2":
        b = round(MAX_BIN * math.log(1 + MU * mag / REF) / _V2_DENOM)
    elif scheme == "legacy":
        b = round(MAX_BIN * math.log(1 + MU * mag) / _LEGACY_DENOM)
    else:
        raise ValueError(f"unknown mu-law scheme {scheme!r}")
    return int(math.copysign(b, delta))


def decode_bin(b: int, scheme: str = DEFAULT_SCHEME) -> float:
    """bin → 代表性 delta（bin 中心；encode(decode(b)) == b）。"""
    if b == 0:
        return 0.0
    if scheme == "dm1-v2":
        mag = (math.exp(abs(b) / MAX_BIN * _V2_DENOM) - 1) / MU * REF
    elif scheme == "legacy":
        mag = (math.exp(abs(b) / MAX_BIN * _LEGACY_DENOM) - 1) / MU
    else:
        raise ValueError(f"unknown mu-law scheme {scheme!r}")
    return math.copysign(mag, b)


def bin_to_class(b: int) -> int:
    """bin ∈ [-10,10] → softmax 类别 ∈ [0,20]。"""
    return b + MAX_BIN


def class_to_bin(c: int) -> int:
    return c - MAX_BIN


# ---------------------------------------------------------------------------
# 帧行解析（p0.jsonl 一行 → 训练标签）
# ---------------------------------------------------------------------------
@dataclass
class FrameAction:
    frame: int
    gframe: int
    keys_mask: int      # CONTRACT_VKEYS 位图（bit i = CONTRACT_VKEYS[i] 按下）
    bin_x: int
    bin_y: int
    dx: int             # pointerDx（clamp ±200；bins 由此派生，与存储契约一致）
    dy: int
    raw_dx: int         # pointerRawDx（未截断的帧内 mouse_move Δ 求和，#137）；缺失回退 dx
    raw_dy: int
    # 视角尺度标定（docs/45，#284）。counts_per_360 标定后 converter 才发这两个值；
    # 未标定时为 None 且 view_scale_valid=False——**原始计数是灵敏度相关的，
    # 未标定数据训出的 camera 头会锁死在某一套灵敏度上且跨游戏不可比**。
    look_yaw_delta_rad: float | None
    look_pitch_delta_rad: float | None
    view_scale_valid: bool
    src: str
    actions: tuple[str, ...] = ()   # dm-1 语义动作（mapping 投影；训练标签主目标）
    buttons: tuple[int, ...] = ()   # held mouse buttons; per-game physical vocab owns positions
    unknown_vks: tuple[int, ...] = ()


def keys_to_mask(keys: list[int]) -> tuple[int, tuple[int, ...]]:
    mask = 0
    unknown = []
    for vk in keys:
        idx = _VK_TO_IDX.get(vk)
        if idx is None:
            unknown.append(vk)
        else:
            mask |= 1 << idx
    return mask, tuple(unknown)


def mask_to_multihot(mask: int) -> np.ndarray:
    out = np.zeros(N_KEYS, dtype=np.float32)
    i = 0
    while mask:
        if mask & 1:
            out[i] = 1.0
        mask >>= 1
        i += 1
    return out


def parse_frame_row(row: dict, *, verify_bins: bool = True,
                    scheme: str = DEFAULT_SCHEME) -> FrameAction:
    """解析 p0.jsonl 一行。

    verify_bins: 用 `scheme` 重算 μ-law 并与存储 pointerBins 对账（契约漂移即报警）。
    对新式 DataMiner 产物传 scheme="dm1-v2"（默认），对 data_demo 存量传 "legacy"。
    训练侧 ingest 通常 verify_bins=False + 从 dx/dy 用统一 scheme 重算。

    dx/dy = pointerDx/Dy（clamp ±200，bins 由此派生）。raw_dx/raw_dy = pointerRawDx/Dy
    （#137，未截断的真实 mouse Δ）；缺失（旧数据/data_demo）回退到 dx/dy。bins 用 dx/dy——
    encode_bin 内部也 clamp，故 encode_bin(dx)==encode_bin(raw_dx)，strict-bins 不受影响。
    """
    dx = int(row.get("pointerDx", 0))
    dy = int(row.get("pointerDy", 0))
    raw_dx = int(row.get("pointerRawDx", dx))
    raw_dy = int(row.get("pointerRawDy", dy))
    yaw_rad = row.get("look_yaw_delta_rad")
    pitch_rad = row.get("look_pitch_delta_rad")
    view_valid = bool(row.get("view_scale_valid", False))
    bx, by = row.get("pointerBins", [encode_bin(dx, scheme), encode_bin(dy, scheme)])
    if verify_bins:
        rx, ry = encode_bin(dx, scheme), encode_bin(dy, scheme)
        if rx != bx or ry != by:
            raise ValueError(
                f"pointerBins 契约漂移(scheme={scheme}): frame={row.get('frame')} "
                f"dx={dx} dy={dy} stored=({bx},{by}) recomputed=({rx},{ry})"
            )
    mask, unknown = keys_to_mask(row.get("keys", []))
    return FrameAction(
        frame=int(row["frame"]),
        gframe=int(row.get("gframe", row["frame"])),
        keys_mask=mask,
        bin_x=int(bx),
        bin_y=int(by),
        dx=dx,
        dy=dy,
        raw_dx=raw_dx,
        raw_dy=raw_dy,
        look_yaw_delta_rad=None if yaw_rad is None else float(yaw_rad),
        look_pitch_delta_rad=None if pitch_rad is None else float(pitch_rad),
        view_scale_valid=view_valid,
        src=str(row.get("src", "")),
        actions=tuple(row.get("actions", [])),
        buttons=tuple(int(v) for v in row.get("buttons", [])),
        unknown_vks=unknown,
    )

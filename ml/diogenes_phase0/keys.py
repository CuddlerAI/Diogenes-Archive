"""Key vocabulary and pointer binning — mirrors the repo data contract.

Key schema follows data_demo/make_dataset.py KEY_SCHEMA:
  "VK codes; W/A/S/D=87/65/83/68, mouseL/R=1/2, 1..6=49..54"

foveated_bin is a bit-for-bit copy of data_demo/auto_label.py (mu=8, ref=200,
max_bin=10).

⚠️ SCANNED_VKEYS IS NARROWER THAN WHAT IS CAPTURED, and deliberately fixed.

An older version of this docstring said the tuple mirrors "input_recorder.rs
SCANNED_VKEYS". There is no such constant: the client's key set is per-game,
driven by `keyboard_vks` in the game profile (see
DataMiner/apps/client/src-tauri/src/platform/windows/catalog.rs). Our own COD
profile declares 34 keyboard VKs and 22 were actually pressed
(model_research/research/19 §7.1); this vocabulary models 12.

Measured cost, on four D2E games (research/21 §6, E7):
**18.9% of key-press events fall outside this vocabulary** — 2.6% on Stardew
Valley up to 43.0% on Rainbow Six. The dropped keys are not incidental:
Tab (1428), LCtrl/crouch (998), R/reload (756), LShift/sprint (530),
F/interact (344), Space/jump (117).

So every per-key metric here is computed over a REDUCED action space, on both
the first-party and the external corpus. That is self-consistent and therefore
still comparable across runs — but a `key_exact` number must not be read as
"recovered the player's keyboard". Widening the tuple changes N_KEYS, the model
head shape and every historical metric, so it is a deliberate decision, not a
patch: see research/21 action item E7.
"""
from __future__ import annotations

import math
from typing import Optional, Sequence

# VK codes: mouseL, mouseR, W, A, S, D, digits 1..6
SCANNED_VKEYS: tuple[int, ...] = (1, 2, 87, 65, 83, 68, 49, 50, 51, 52, 53, 54)
VK_TO_INDEX: dict[int, int] = {vk: i for i, vk in enumerate(SCANNED_VKEYS)}
N_KEYS = len(SCANNED_VKEYS)

# Phase 0 may only consume these two known declarations of the same frozen
# 12-code vocabulary.  Keep the historical strings readable, but do not treat
# a missing or novel description as legacy: keys_to_multihot intentionally
# drops codes outside SCANNED_VKEYS, so accepting an unknown schema would turn
# an input-contract change into silent all-zero labels.
DATA_DEMO_LEGACY_KEY_SCHEMA = (
    "VK codes; W/A/S/D=87/65/83/68, mouseL/R=1/2, 1..6=49..54 "
    "(mirrors input_recorder.rs SCANNED_VKEYS)"
)
GAMING500_LEGACY_KEY_SCHEMA = (
    "VK codes; W/A/S/D=87/65/83/68, mouseL/R=1/2, 1..6=49..54 "
    "(mapped from gaming-500-hours keyCode/characters)"
)
PHASE0_LEGACY_KEY_SCHEMAS = frozenset((
    DATA_DEMO_LEGACY_KEY_SCHEMA,
    GAMING500_LEGACY_KEY_SCHEMA,
))

MOUSE_CLAMP = 200.0
FOVEATED_MAX_BIN = 10
N_BINS = 2 * FOVEATED_MAX_BIN + 1  # bins -10..10 -> 21 classes


def foveated_bin(delta: float, max_bin: int = FOVEATED_MAX_BIN, mu: float = 8.0) -> int:
    if delta == 0 or max_bin <= 0:
        return 0
    sign = 1 if delta > 0 else -1
    mag = abs(delta)
    ref = 200.0
    compressed = math.log(1 + mu * mag) / math.log(1 + mu * ref)
    return sign * max(0, min(max_bin, round(compressed * max_bin)))


def bin_to_class(b: int, max_bin: int = FOVEATED_MAX_BIN) -> int:
    """Signed bin (-max_bin..max_bin) -> class index (0..2*max_bin)."""
    return int(b) + max_bin


def class_to_delta(cls: int, max_bin: int = FOVEATED_MAX_BIN,
                   mu: float = 8.0) -> float:
    """Inverse of foveated_bin at the bin center: class index -> pixel delta.

    VPT-style decode — the mouse regression metric is computed from the bin
    head's expectation, not a raw-delta head.
    """
    b = cls - max_bin
    if b == 0:
        return 0.0
    ref = 200.0
    mag = (math.exp(abs(b) / max_bin * math.log(1 + mu * ref)) - 1) / mu
    return math.copysign(mag, b)


def keys_to_multihot(keys: Sequence[int]) -> list[int]:
    """VK list -> multi-hot over SCANNED_VKEYS; VKs outside the vocab are dropped."""
    hot = [0] * N_KEYS
    for vk in keys:
        idx = VK_TO_INDEX.get(int(vk))
        if idx is not None:
            hot[idx] = 1
    return hot


# ---------------------------------------------------------------------------
# gaming-500-hours keycode mapping.
#
# Sessions are Windows (489h) + macOS (5h). `key` events carry keyCode +
# characters. Strategy: map by `characters` first (platform-independent),
# fall back to the keyCode interpreted per-platform.
# ---------------------------------------------------------------------------

_CHAR_TO_VK: dict[str, int] = {
    "w": 87, "a": 65, "s": 83, "d": 68,
    "1": 49, "2": 50, "3": 51, "4": 52, "5": 53, "6": 54,
}

# macOS virtual keycodes (kVK_ANSI_*) for the scanned subset.
_MAC_KEYCODE_TO_VK: dict[int, int] = {
    13: 87,  # W
    0: 65,   # A
    1: 83,   # S
    2: 68,   # D
    18: 49, 19: 50, 20: 51, 21: 52, 23: 53, 22: 54,  # 1..6
}


def map_keycode(keycode: Optional[int], characters: Optional[str],
                platform: str = "windows") -> Optional[int]:
    """Map a gaming-500-hours key event to a SCANNED_VKEYS VK, or None if unscanned."""
    if characters:
        vk = _CHAR_TO_VK.get(characters.strip().lower()[:1] if characters.strip() else "")
        if vk is not None:
            return vk
    if keycode is None:
        return None
    if platform.lower().startswith("mac"):
        return _MAC_KEYCODE_TO_VK.get(int(keycode))
    # Windows VKs for letters/digits coincide with ASCII uppercase — identity
    # if already in vocab.
    return int(keycode) if int(keycode) in VK_TO_INDEX else None


def map_mouse_button(button: str) -> Optional[int]:
    b = (button or "").lower()
    if b == "left":
        return 1
    if b == "right":
        return 2
    return None

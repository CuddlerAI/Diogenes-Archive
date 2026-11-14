"""E2/E3 用 rollout 接口：模型协议 + dummy 实现 + 固定种子起始集.

真模型（M1 蒸馏后 student）实现 WorldModel 协议即可直接进全套探针。
"""
from __future__ import annotations

from typing import Protocol, runtime_checkable

import numpy as np

from diogenes_phase0.contract import ActionFrame


@runtime_checkable
class WorldModel(Protocol):
    def reset(self, obs0: np.ndarray) -> None:
        """obs0: [H,W,3] uint8 起始帧。"""
        ...

    def step(self, action: ActionFrame) -> np.ndarray:
        """执行一帧动作，返回下一帧 [H,W,3] uint8。"""
        ...


class DummyLastFrameModel:
    """恒等模型：永远返回上一帧。E1 遵循率应≈0、漂移应为 0 —— harness 的负对照。"""

    def reset(self, obs0: np.ndarray) -> None:
        self._last = obs0.copy()

    def step(self, action: ActionFrame) -> np.ndarray:
        return self._last


class DummyNoiseDriftModel:
    """每步叠噪声：PSNR 单调劣化 —— E2 漂移探针的正对照。"""

    def __init__(self, sigma: float = 4.0, seed: int = 0):
        self._sigma, self._rng = sigma, np.random.default_rng(seed)

    def reset(self, obs0: np.ndarray) -> None:
        self._last = obs0.astype(np.float32)

    def step(self, action: ActionFrame) -> np.ndarray:
        self._last = np.clip(
            self._last + self._rng.normal(0, self._sigma, self._last.shape), 0, 255)
        return self._last.astype(np.uint8)


def rollout(model: WorldModel, obs0: np.ndarray,
            actions: list[ActionFrame]) -> np.ndarray:
    """返回 [T,H,W,3]（不含 obs0）。"""
    model.reset(obs0)
    return np.stack([model.step(a) for a in actions])

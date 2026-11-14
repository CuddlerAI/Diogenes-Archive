"""W4/R0 rollout 环: capture → policy → inject, 可插拔后端 (research/07 §4.2)。

agent 在语义空间思考；物理键在注入边缘由 mapping 逆映射（research/07 §2.2）。协议:
- Capture.grab() → np.uint8 [H,W,3] (模型输入分辨率, 由后端负责缩放)
- Policy.step(frame) → (actions: set[str], dx: int, dy: int); reset() 清 hidden
- Injector.apply(actions: set[str], dx: int, dy: int) → None (自行差分/逆映射)

本模块自带 Fake 三件套 (测试/macOS) 与 OnnxPolicy (ORT 单步, 消费 W3 导出物)。
Windows 真实后端在 windows.py (dxcam + SendInput + mapping 逆映射, 需实机验证)。
"""
from __future__ import annotations

import time
from typing import Protocol

import numpy as np

from ... import contract
from ...action_registry import ActionVocab
from .telemetry import Telemetry, Tick


class Capture(Protocol):
    def grab(self) -> np.ndarray: ...


class Policy(Protocol):
    def step(self, frame: np.ndarray) -> tuple[set[str], int, int]: ...
    def reset(self) -> None: ...


class Injector(Protocol):
    def apply(self, actions: set[str], dx: int, dy: int) -> None: ...


# ---------------------------------------------------------------------------
class OnnxPolicy:
    """ORT 单步推理后端。action: sigmoid>threshold → 语义集合; camera: argmax bin → decode。"""

    def __init__(self, onnx_path: str, vocab: ActionVocab, *, gru_layers: int,
                 gru_hidden: int, scheme: str = contract.DEFAULT_SCHEME,
                 threshold: float = 0.5, providers: list[str] | None = None):
        import onnxruntime as ort

        self.sess = ort.InferenceSession(
            onnx_path, providers=providers or ["CPUExecutionProvider"])
        self.vocab = vocab
        self.scheme = scheme
        self.threshold = threshold
        self._h_shape = (gru_layers, 1, gru_hidden)
        self.reset()

    def reset(self) -> None:
        self.h = np.zeros(self._h_shape, dtype=np.float32)

    def step(self, frame: np.ndarray) -> tuple[set[str], int, int]:
        x = frame.astype(np.float32).transpose(2, 0, 1)[None] / 255.0
        action_logits, camera_logits, self.h = self.sess.run(
            None, {"frame": x, "h_in": self.h})
        probs = 1.0 / (1.0 + np.exp(-action_logits[0]))
        actions = {self.vocab.actions[i]
                   for i in np.nonzero(probs > self.threshold)[0]}
        bx, by = camera_logits[0].argmax(-1) - contract.MAX_BIN
        return (actions,
                round(contract.decode_bin(int(bx), self.scheme)),
                round(contract.decode_bin(int(by), self.scheme)))


# --------------------------- Fake 后端 (测试/macOS) -------------------------
class FakeCapture:
    def __init__(self, height: int = 72, width: int = 128, seed: int = 0):
        self.rng = np.random.default_rng(seed)
        self.hw = (height, width)

    def grab(self) -> np.ndarray:
        return self.rng.integers(0, 255, (*self.hw, 3), dtype=np.uint8)


class FakeInjector:
    def __init__(self) -> None:
        self.calls: list[tuple[set[str], int, int]] = []

    def apply(self, actions: set[str], dx: int, dy: int) -> None:
        self.calls.append((set(actions), dx, dy))


class RandomPolicy:
    """随机语义动作（测试/延迟基准用）。默认从 COD 词表抽样。"""

    def __init__(self, seed: int = 0, actions: tuple[str, ...] | None = None):
        self.rng = np.random.default_rng(seed)
        self.actions = actions or ("move.forward", "combat.primary_attack",
                                   "combat.aim", "move.sprint")

    def reset(self) -> None:
        pass

    def step(self, frame: np.ndarray) -> tuple[set[str], int, int]:
        acts = {a for a in self.actions if self.rng.random() < 0.4}
        return acts, int(self.rng.integers(-50, 51)), int(self.rng.integers(-20, 21))


# ---------------------------------------------------------------------------
class RolloutLoop:
    def __init__(self, capture: Capture, policy: Policy, injector: Injector, *,
                 hz: float = 30.0, telemetry: Telemetry | None = None):
        self.capture = capture
        self.policy = policy
        self.injector = injector
        self.period = 1.0 / hz
        self.telemetry = telemetry if telemetry is not None else Telemetry()
        self._stop = False

    def stop(self) -> None:
        self._stop = True

    def run(self, max_ticks: int | None = None,
            duration_s: float | None = None) -> Telemetry:
        self._stop = False
        deadline = time.perf_counter()
        t_end = time.perf_counter() + duration_s if duration_s else None
        n = 0
        while not self._stop:
            if max_ticks is not None and n >= max_ticks:
                break
            if t_end is not None and time.perf_counter() >= t_end:
                break
            t0 = time.perf_counter()
            frame = self.capture.grab()
            t1 = time.perf_counter()
            keys, dx, dy = self.policy.step(frame)
            t2 = time.perf_counter()
            self.injector.apply(keys, dx, dy)
            t3 = time.perf_counter()
            self.telemetry.add(Tick(t0, t1, t2, t3))
            n += 1
            # 节拍: 睡到下一个整周期 (Windows 上配合 timeBeginPeriod(1), 见 windows.py)
            deadline += self.period
            sleep = deadline - time.perf_counter()
            if sleep > 0:
                time.sleep(sleep)
            else:
                deadline = time.perf_counter()  # 落后即重锚, 不追帧
        return self.telemetry

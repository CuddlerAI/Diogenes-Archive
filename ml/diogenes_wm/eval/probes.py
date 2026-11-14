"""E1/E2 探针：视角遵循（相位相关全局位移 vs 鼠标 Δ）与漂移检测.

E1a 视角探针原理：FPS 里鼠标水平移动 → 场景整体平移。用相位相关
（numpy FFT，无 cv2 依赖）估计相邻帧全局位移，与 pointer_dx/dy 做
Pearson 相关。|r| 是动作-画面耦合强度（1=完全遵循，0=无关），符号由
引擎/采集约定决定 —— 报告带符号 r，不猜约定。

在真实录像上跑 = 探针自检（真数据耦合天然高）；在模型 rollout 上跑 = E1 分数。
"""
from __future__ import annotations

from dataclasses import dataclass

import numpy as np

from diogenes_phase0.contract import ActionFrame


def _gray(frames: np.ndarray) -> np.ndarray:
    return frames.astype(np.float32).mean(axis=-1)


def phase_shift(a: np.ndarray, b: np.ndarray) -> tuple[float, float]:
    """相位相关估计 b 相对 a 的整体平移 (dx, dy)。a/b: [H,W] float。"""
    fa, fb = np.fft.rfft2(a), np.fft.rfft2(b)
    cross = fa * np.conj(fb)
    denom = np.abs(cross)
    denom[denom == 0] = 1e-9
    corr = np.fft.irfft2(cross / denom, s=a.shape)
    dy, dx = np.unravel_index(int(np.argmax(corr)), corr.shape)
    h, w = a.shape
    return float(dx if dx <= w // 2 else dx - w), float(dy if dy <= h // 2 else dy - h)


def _flow_shift(a: np.ndarray, b: np.ndarray) -> tuple[float, float] | None:
    """cv2 Farneback 稠密光流中值位移（真实 3D 画面上远强于相位相关:
    含视差/动态模糊时 phase corr 在实测 BioShock 样例上 r≈0, Farneback r≈0.39）。"""
    try:
        import cv2
    except ImportError:
        return None
    flow = cv2.calcOpticalFlowFarneback(a.astype(np.uint8), b.astype(np.uint8),
                                        None, 0.5, 3, 15, 3, 5, 1.2, 0)
    return float(np.median(flow[..., 0])), float(np.median(flow[..., 1]))


@dataclass
class ViewProbeResult:
    n_pairs: int          # 参与统计的帧对数(|pointer Δ| 超阈值)
    r_dx: float           # 估计位移 x vs pointer_dx 的 Pearson r（带符号）
    r_dy: float
    coupling: float       # max(|r_dx|, |r_dy|) —— E1a 总分
    estimator: str = "?"  # "flow"(首选) | "phase"(退路)

    def row(self) -> str:
        return f"| E1a 视角遵循 | pairs={self.n_pairs} | r_dx={self.r_dx:+.3f} r_dy={self.r_dy:+.3f} ({self.estimator}) | **{self.coupling:.3f}** |"


def view_probe(frames: np.ndarray, actions: list[ActionFrame],
               min_delta: float = 3.0, downscale: int = 4,
               estimator: str = "auto") -> ViewProbeResult:
    """frames: [T,H,W,3]; actions[i] 是 frames[i]→frames[i+1] 期间的输入。

    estimator: "auto"(有 cv2 用 flow, 否则 phase) | "flow" | "phase"。
    真实 3D 画面务必用 flow; phase 只适合纯平移(合成测试/2D)。
    """
    g = _gray(frames[:, ::downscale, ::downscale])
    est, lab, used = [], [], "phase"
    for i in range(len(frames) - 1):
        a = actions[i]
        if abs(a.pointer_dx) < min_delta and abs(a.pointer_dy) < min_delta:
            continue
        shift = _flow_shift(g[i], g[i + 1]) if estimator in ("auto", "flow") else None
        if shift is not None:
            used = "flow"
        elif estimator == "flow":
            raise RuntimeError("estimator=flow 需要 cv2 (安装 [data] extra)")
        else:
            shift = phase_shift(g[i], g[i + 1])
        dx, dy = shift
        est.append((dx * downscale, dy * downscale))
        lab.append((a.pointer_dx, a.pointer_dy))
    if len(est) < 3:
        return ViewProbeResult(len(est), 0.0, 0.0, 0.0, used)
    e, l = np.array(est), np.array(lab)

    def _r(x, y):
        if np.std(x) == 0 or np.std(y) == 0:
            return 0.0
        return float(np.corrcoef(x, y)[0, 1])

    r_dx, r_dy = _r(e[:, 0], l[:, 0]), _r(e[:, 1], l[:, 1])
    return ViewProbeResult(len(est), r_dx, r_dy,
                           max(abs(r_dx), abs(r_dy)), used)


@dataclass
class DriftProbeResult:
    horizon: int              # 评估的 rollout 长度
    psnr_curve: list[float]   # 每帧 vs 参考帧 PSNR
    collapse_at: int | None   # 首次跌破阈值的帧号(None=未崩)

    def row(self) -> str:
        tail = f"{self.psnr_curve[-1]:.1f}dB@{self.horizon}"
        return f"| E2 漂移 | collapse_at={self.collapse_at} | 末帧 {tail} | |"


def drift_probe(pred: np.ndarray, ref: np.ndarray,
                collapse_db: float = 15.0) -> DriftProbeResult:
    """pred/ref: [T,H,W,3] —— 模型 rollout vs 真值同段。逐帧 PSNR 曲线 + 崩坏点。"""
    curve, collapse = [], None
    for i in range(len(pred)):
        mse = np.mean((pred[i].astype(np.float64) - ref[i].astype(np.float64)) ** 2)
        p = float("inf") if mse == 0 else float(10 * np.log10(255.0 ** 2 / mse))
        curve.append(p)
        if collapse is None and p < collapse_db:
            collapse = i
    return DriftProbeResult(len(pred), curve, collapse)

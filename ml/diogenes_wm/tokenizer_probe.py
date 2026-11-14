"""M0-a · Tokenizer 探底：在 data_demo 契约数据上测重建质量下限/基线.

用法（本机即可，无 GPU）:
    python -m diogenes_wm.tokenizer_probe --root ../data_demo/dataset \
        --backend resize --target 640x360 --out /tmp/tokenizer_probe.md

backend:
    resize       纯下采样→上采样（DATASET 平面重物化的信息下限；无编解码依赖）
    resize_h264  加 H.264 往返（需 ffmpeg；更接近真实载体）
    cosmos / wan 冻结 tokenizer 往返（需 GPU 环境，本模块只留接口与指引）

指标: 分区域 PSNR —— full / center(中心战斗区) / hud(顶部+底部条带)。
HUD 是干扰物主源（survey §5），tokenizer 若在 HUD 区花费过多容量是坏信号。
"""
from __future__ import annotations

import argparse
import shutil
import subprocess
import tempfile
from dataclasses import dataclass
from pathlib import Path

import numpy as np

from diogenes_phase0.contract import ChunkRecord
from diogenes_phase0.data.dataset import scan_dataset, _load_frames

HUD_TOP_FRAC = 0.10     # Halo/FPS 通用: 顶部计分/雷达条带
HUD_BOTTOM_FRAC = 0.15  # 底部武器/血条条带
CENTER_FRAC = 0.50      # 中心战斗区(准星周围)


def psnr(a: np.ndarray, b: np.ndarray) -> float:
    mse = np.mean((a.astype(np.float64) - b.astype(np.float64)) ** 2)
    return float("inf") if mse == 0 else float(10 * np.log10(255.0 ** 2 / mse))


def region_slices(h: int, w: int) -> dict[str, tuple[slice, slice]]:
    ct, cb = int(h * (1 - CENTER_FRAC) / 2), int(h * (1 + CENTER_FRAC) / 2)
    cl, cr = int(w * (1 - CENTER_FRAC) / 2), int(w * (1 + CENTER_FRAC) / 2)
    return {
        "full": (slice(None), slice(None)),
        "center": (slice(ct, cb), slice(cl, cr)),
        "hud_top": (slice(0, int(h * HUD_TOP_FRAC)), slice(None)),
        "hud_bottom": (slice(int(h * (1 - HUD_BOTTOM_FRAC)), h), slice(None)),
    }


def _resize(frames: np.ndarray, size: tuple[int, int]) -> np.ndarray:
    """(T,H,W,3) uint8 → 双线性/区域缩放到 size=(W,H)。优先 cv2，缺失则退 numpy 整数因子。"""
    try:
        import cv2
        return np.stack([cv2.resize(f, size, interpolation=cv2.INTER_AREA
                                    if size[0] < f.shape[1] else cv2.INTER_LINEAR)
                         for f in frames])
    except ImportError:
        t, h, w, _ = frames.shape
        tw, th = size
        if h % th == 0 and w % tw == 0:      # 下采样: 块均值
            fh, fw = h // th, w // tw
            return frames.reshape(t, th, fh, tw, fw, 3).mean(axis=(2, 4)).astype(np.uint8)
        if th % h == 0 and tw % w == 0:      # 上采样: 最近邻
            return frames.repeat(th // h, axis=1).repeat(tw // w, axis=2)
        raise RuntimeError("numpy 退路仅支持整数倍缩放; 请安装 [data] extra (opencv)")


def backend_resize(frames: np.ndarray, target: tuple[int, int]) -> np.ndarray:
    h, w = frames.shape[1:3]
    return _resize(_resize(frames, target), (w, h))


def backend_resize_h264(frames: np.ndarray, target: tuple[int, int], crf: int = 23,
                        fps: float = 30.0) -> np.ndarray:
    # fps 从 chunk 声明值穿入(混合 fps 防线, docs/50 P-4): CRF 下码率分配随
    # 帧率变化,60fps 语料按 30 往返会高估重建质量。
    if shutil.which("ffmpeg") is None:
        raise RuntimeError("backend=resize_h264 需要 ffmpeg")
    small = _resize(frames, target)
    t, h, w, _ = small.shape
    with tempfile.TemporaryDirectory() as td:
        mp4 = Path(td) / "roundtrip.mp4"
        enc = ["ffmpeg", "-v", "error", "-f", "rawvideo", "-pix_fmt", "rgb24",
               "-s", f"{w}x{h}", "-r", f"{fps:g}", "-i", "-", "-c:v", "libx264",
               "-crf", str(crf), "-pix_fmt", "yuv420p", str(mp4)]
        subprocess.run(enc, input=small.tobytes(), check=True)
        dec = ["ffmpeg", "-v", "error", "-i", str(mp4), "-f", "rawvideo",
               "-pix_fmt", "rgb24", "-"]
        raw = subprocess.run(dec, capture_output=True, check=True).stdout
    out = np.frombuffer(raw, np.uint8)[: t * h * w * 3].reshape(t, h, w, 3)
    hh, ww = frames.shape[1:3]
    return _resize(out, (ww, hh))


GPU_BACKEND_HINT = {
    "cosmos": "冻结 Cosmos-Tokenizer DV8x16x16 (github.com/NVIDIA/Cosmos-Tokenizer), GPU 环境实现往返后接入此接口",
    "wan": "冻结 Wan2.1 VAE (HF Wan-AI/Wan2.1-T2V-1.3B), GPU 环境实现往返后接入此接口",
}


@dataclass
class ChunkResult:
    sample_key: str
    scores: dict[str, float]


def probe_chunk(record: ChunkRecord, backend: str, target: tuple[int, int],
                stride: int = 10) -> ChunkResult:
    frames = _load_frames(record)[::stride]
    if backend == "resize":
        recon = backend_resize(frames, target)
    elif backend == "resize_h264":
        recon = backend_resize_h264(frames, target, fps=record.meta.fps)
    else:
        raise NotImplementedError(f"backend={backend}: {GPU_BACKEND_HINT.get(backend, '未知')}")
    regions = region_slices(*frames.shape[1:3])
    return ChunkResult(record.meta.sample_key,
                       {name: psnr(frames[:, ys, xs], recon[:, ys, xs])
                        for name, (ys, xs) in regions.items()})


def select_chunks(root: Path, max_chunks: int, spread: bool) -> list[ChunkRecord]:
    """要探的 chunk。`spread` 跨会话跨步取, 而不是取排序后的头部。

    `scan_dataset` 按路径排序, 所以"取前 N 个"在"一个会话一个目录"的语料上
    会把 N 个 chunk 全从第一个会话里拿 —— 单会话的画面(同一张图、同一段光照)
    于是被读成整个语料的重建难度。IDM 线的 `label_signal._select` 已经因为同一
    件事加过这个开关(gaming-500 的已发布读数就带着这个偏置), 这里照同一口径做。
    默认关着, 让历史数字仍可复现; 新测量一律传 `--spread`。
    """
    recs = [r for r in scan_dataset(root) if not r.meta.is_cutscene]
    if not spread or len(recs) <= max_chunks:
        return recs[:max_chunks]
    return recs[::max(1, len(recs) // max_chunks)][:max_chunks]


def run(root: Path, backend: str, target: tuple[int, int], max_chunks: int,
        stride: int, spread: bool = False) -> str:
    records = select_chunks(root, max_chunks, spread)
    if not records:
        raise SystemExit(f"{root} 下没有可用 chunk")
    results = [probe_chunk(r, backend, target, stride) for r in records]
    regions = list(results[0].scores)
    med = {reg: float(np.median([c.scores[reg] for c in results])) for reg in regions}
    lines = [
        "# M0-a Tokenizer 探底报告", "",
        f"- root: `{root}` · chunks: {len(results)}"
        f"{'(跨会话取样)' if spread else '(头部取样)'} · 抽帧 stride: {stride}",
        f"- backend: **{backend}** · target: {target[0]}x{target[1]}",
        f"- 通过线参照 m0-training-config.md: 中心区 PSNR >= 30dB 且 HUD 文字可读", "",
        "| 区域 | PSNR 中位数 (dB) |", "|---|---|",
        *[f"| {reg} | {med[reg]:.2f} |" for reg in regions], "",
        "| chunk | " + " | ".join(regions) + " |",
        "|---|" + "---|" * len(regions),
        *[f"| {c.sample_key} | " + " | ".join(f"{c.scores[r]:.2f}" for r in regions) + " |"
          for c in results],
    ]
    return "\n".join(lines) + "\n"


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--root", required=True)
    ap.add_argument("--backend", default="resize",
                    choices=["resize", "resize_h264", "cosmos", "wan"])
    ap.add_argument("--target", default="640x360")
    ap.add_argument("--max-chunks", type=int, default=8)
    ap.add_argument("--stride", type=int, default=10)
    ap.add_argument("--spread", action="store_true",
                    help="跨会话跨步取样, 而不是取排序后的头部(新测量一律传)")
    ap.add_argument("--out", default=None)
    a = ap.parse_args()
    w, h = a.target.split("x")
    report = run(Path(a.root), a.backend, (int(w), int(h)), a.max_chunks,
                 a.stride, a.spread)
    if a.out:
        Path(a.out).write_text(report)
        print(f"→ {a.out}")
    else:
        print(report)


if __name__ == "__main__":
    main()

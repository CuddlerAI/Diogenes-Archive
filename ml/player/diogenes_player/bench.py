"""W1 dataloader 吞吐基准（research/04 §8 W1 退出标准的测量工具）。

用法:
    python -m diogenes_player.bench --plane /path/to/train_plane \
        --n 64 --t 96 --workers 4 --allow-untrainable

输出 JSON: samples/s, frames/s, 解码 MB/s, 逐样本延迟分位数。
退出标准参考: 单节点吞吐 ≥ 3× 训练消耗
(≤50M 模型 bf16 @ ~16 样本×96 帧/步·秒级步长 ⇒ 需求约 1.5-3k 帧/s/GPU)。
"""
from __future__ import annotations

import argparse
import json
import time
from multiprocessing import get_context

import numpy as np

from .dataset import WindowDataset
from .video import available_backends

_worker_ds: WindowDataset | None = None


def _init_worker(plane: str, t: int, allow: bool, backend: str, seed: int) -> None:
    global _worker_ds
    _worker_ds = WindowDataset(plane, t=t, allow_untrainable=allow,
                               backend=backend, seed=seed)


def _draw(_: int) -> tuple[float, int]:
    t0 = time.perf_counter()
    s = _worker_ds.sample()
    dt = time.perf_counter() - t0
    return dt, int(s["frames"].nbytes)


def bench(plane: str, *, n: int, t: int, workers: int, allow: bool,
          backend: str, seed: int) -> dict:
    wall0 = time.perf_counter()
    if workers <= 1:
        _init_worker(plane, t, allow, backend, seed)
        results = [_draw(i) for i in range(n)]
    else:
        ctx = get_context("spawn")
        with ctx.Pool(workers, initializer=_init_worker,
                      initargs=(plane, t, allow, backend, seed)) as pool:
            results = pool.map(_draw, range(n))
    wall = time.perf_counter() - wall0

    lat = np.array([r[0] for r in results])
    nbytes = sum(r[1] for r in results)
    return {
        "backend": backend if backend != "auto" else available_backends()[0],
        "workers": workers,
        "n_samples": n,
        "t_frames": t,
        "wall_s": round(wall, 3),
        "samples_per_s": round(n / wall, 2),
        "frames_per_s": round(n * t / wall, 1),
        "decoded_MB_per_s": round(nbytes / wall / 1e6, 1),
        "sample_latency_ms": {
            "p50": round(float(np.percentile(lat, 50)) * 1e3, 1),
            "p90": round(float(np.percentile(lat, 90)) * 1e3, 1),
            "p99": round(float(np.percentile(lat, 99)) * 1e3, 1),
        },
    }


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--plane", required=True)
    ap.add_argument("--n", type=int, default=64)
    ap.add_argument("--t", type=int, default=96)
    ap.add_argument("--workers", type=int, default=1)
    ap.add_argument("--backend", default="auto", choices=["auto", "torchcodec", "pyav", "ffmpeg"])
    ap.add_argument("--allow-untrainable", action="store_true")
    ap.add_argument("--seed", type=int, default=0)
    args = ap.parse_args()
    print(json.dumps(bench(args.plane, n=args.n, t=args.t, workers=args.workers,
                           allow=args.allow_untrainable, backend=args.backend,
                           seed=args.seed), indent=2))


if __name__ == "__main__":
    main()

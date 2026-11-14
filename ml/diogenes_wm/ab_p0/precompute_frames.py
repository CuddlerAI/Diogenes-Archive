#!/usr/bin/env python3
"""AB-P0 帧预转: chunk p0.mp4 → p0.npz (契约既有的快速路径, contract.py L4/L296).

动机: COD chunk 是 1080p×300 帧, 每段在线解码 ~1.87GB / ~2.5s——训练吞吐与宿主内存
双双撑不住(实测 6 worker OOM 64GB; 2 worker 6000 步 ETA ~100h)。预转成训练分辨率的
uint8 npz(~45MB/段, 全量 ~38GB)后, _load_frames 走 npz 路径, 解码从热路径消失。

几何与在线路径一致: (224,224) 方形, 双线性(cv2.INTER_LINEAR ≈ torch bilinear
align_corners=False)——P1 论文五 run 的在线 interpolate 同款几何与核。
"""
import sys
from multiprocessing import Pool
from pathlib import Path

import cv2
import numpy as np

RES = (224, 224)


def one(chunk_dir: Path) -> str:
    npz = chunk_dir / "p0.npz"
    if npz.exists():
        return "skip"
    cap = cv2.VideoCapture(str(chunk_dir / "p0.mp4"))
    frames = []
    ok, f = cap.read()
    while ok:
        frames.append(cv2.resize(cv2.cvtColor(f, cv2.COLOR_BGR2RGB), RES,
                                 interpolation=cv2.INTER_LINEAR))
        ok, f = cap.read()
    cap.release()
    if not frames:
        return f"EMPTY:{chunk_dir}"
    arr = np.stack(frames)  # [T,224,224,3] uint8
    tmp = chunk_dir / "p0.tmp.npz"
    with open(tmp, "wb") as fh:
        np.savez(fh, frames=arr)
    tmp.rename(npz)
    return "done"


def main() -> None:
    root = Path(sys.argv[1])
    dirs = sorted(p.parent for p in root.glob("*/*/p0.mp4"))
    print(f"chunks: {len(dirs)}")
    with Pool(12) as pool:
        results = pool.map(one, dirs)
    from collections import Counter
    print(Counter(results))
    bad = [r for r in results if r.startswith("EMPTY")]
    if bad:
        print("FAILED:", bad[:5])
        sys.exit(1)
    print("PRECOMPUTE_OK")


if __name__ == "__main__":
    main()

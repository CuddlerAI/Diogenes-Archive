#!/usr/bin/env python3
"""video.mp4 → watermark-samples.jsonl（mac 侧外部实现）。

契约逐字取自 DataMiner/tools/pr405-fallout4/external-conversion.md §1：

  输出行  {"session_frame": <解码顺序帧号,从 0>, "block_means": [<24 个 u8>]}
  采样矩形 左上 (origin_x, origin_y) 起 192×8 px（24 块 × 8px）
  块均值   块中心 4×4：x ∈ [bx+2, bx+6)，y ∈ [oy+2, oy+6)
  亮度     **逐像素**整数 (r*299 + g*587 + b*114)/1000（截断），再 16 像素整数均值（截断）
  帧序     解码器输出顺序即 session_frame，不得跳帧不得去重

最后两条是判决点：浮点亮度、或「先均值后转亮度」，都会在边界值上翻位。
本脚本不做任何判决（同步字、位序、绕回展开全在仓库内 depth_lab::watermark），
只负责「把像素量出来」。
"""
import json
import subprocess
import sys
from pathlib import Path

import numpy as np

BLOCK_PX = 8
TOTAL_BLOCKS = 24
RECT_W = TOTAL_BLOCKS * BLOCK_PX  # 192
RECT_H = BLOCK_PX  # 8


def segment_frames(video: Path, origin_x: int, origin_y: int):
    """逐帧吐出 crop 后的 (8, 192, 3) uint8。"""
    cmd = [
        "ffmpeg", "-v", "error", "-i", str(video),
        "-vf", f"crop={RECT_W}:{RECT_H}:{origin_x}:{origin_y}",
        "-f", "rawvideo", "-pix_fmt", "rgb24", "-",
    ]
    frame_bytes = RECT_W * RECT_H * 3
    proc = subprocess.Popen(cmd, stdout=subprocess.PIPE, bufsize=frame_bytes * 64)
    try:
        while True:
            buf = proc.stdout.read(frame_bytes)
            if len(buf) < frame_bytes:
                break
            yield np.frombuffer(buf, dtype=np.uint8).reshape(RECT_H, RECT_W, 3)
    finally:
        proc.stdout.close()
        proc.wait()


def block_means(crop: np.ndarray) -> list[int]:
    """24 个块中心 4×4 的整数亮度均值。"""
    # 逐像素整数亮度，截断——不是浮点，也不是先均值。
    r = crop[:, :, 0].astype(np.int32)
    g = crop[:, :, 1].astype(np.int32)
    b = crop[:, :, 2].astype(np.int32)
    luma = (r * 299 + g * 587 + b * 114) // 1000
    out = []
    for i in range(TOTAL_BLOCKS):
        bx = i * BLOCK_PX
        centre = luma[2:6, bx + 2 : bx + 6]  # y ∈ [2,6), x ∈ [bx+2, bx+6)
        out.append(int(centre.sum() // 16))  # 16 像素整数均值，截断
    return out


def main() -> int:
    args = dict(zip(sys.argv[1::2], sys.argv[2::2]))
    seg_root = Path(args["--segments"])
    out_path = Path(args["--out"])
    origin_x = int(args.get("--origin-x", 0))
    origin_y = int(args.get("--origin-y", 0))

    # 段目录名是零填充序号，字典序即时间序。
    segments = sorted(p for p in seg_root.iterdir() if p.is_dir())
    session_frame = 0
    with out_path.open("w") as fh:
        for seg in segments:
            video = seg / "video.mp4"
            if not video.exists():
                print(f"跳过 {seg.name}：没有 video.mp4", file=sys.stderr)
                continue
            n = 0
            for crop in segment_frames(video, origin_x, origin_y):
                fh.write(json.dumps({
                    "session_frame": session_frame,
                    "block_means": block_means(crop),
                }) + "\n")
                session_frame += 1
                n += 1
            print(f"  {seg.name}: {n} 帧（累计 {session_frame}）", file=sys.stderr)
    print(f"写出 {session_frame} 行 → {out_path}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

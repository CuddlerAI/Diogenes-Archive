"""视频随机窗口解码，多后端：torchcodec（训练机）→ PyAV（开发机）→ ffmpeg 管道（兜底）。

所有后端返回 np.uint8 [T, H, W, 3] (RGB)。输入假定 CFR（dm-1 契约保证），
帧定位一律按帧号（时间 = 帧号 / fps，契约「时间=帧号」原则）。
"""
from __future__ import annotations

import subprocess
from fractions import Fraction

import numpy as np

_BACKENDS: list[str] = []
try:  # 训练机路径
    from torchcodec.decoders import VideoDecoder  # type: ignore

    _BACKENDS.append("torchcodec")
except ImportError:
    pass
try:
    import av  # type: ignore

    _BACKENDS.append("pyav")
except ImportError:
    pass
_BACKENDS.append("ffmpeg")


def available_backends() -> list[str]:
    return list(_BACKENDS)


def decode_window(
    path: str,
    start_frame: int,
    n_frames: int,
    *,
    fps: float,
    backend: str = "auto",
) -> np.ndarray:
    if backend == "auto":
        backend = _BACKENDS[0]
    if backend == "torchcodec":
        return _decode_torchcodec(path, start_frame, n_frames)
    if backend == "pyav":
        return _decode_pyav(path, start_frame, n_frames, fps)
    if backend == "ffmpeg":
        return _decode_ffmpeg(path, start_frame, n_frames, fps)
    raise ValueError(f"unknown backend {backend!r} (available: {_BACKENDS})")


def _decode_torchcodec(path: str, start: int, n: int) -> np.ndarray:
    dec = VideoDecoder(path, seek_mode="approximate")  # CFR ⇒ approximate 安全
    batch = dec.get_frames_in_range(start, start + n)
    return batch.data.permute(0, 2, 3, 1).contiguous().numpy()


def _decode_pyav(path: str, start: int, n: int, fps: float) -> np.ndarray:
    with av.open(path) as container:
        stream = container.streams.video[0]
        stream.thread_type = "AUTO"
        tb = stream.time_base or Fraction(1, int(round(fps)))
        # seek 到目标帧之前最近的关键帧，再顺序解码到目标
        target_pts = int(start / fps / tb)
        container.seek(target_pts, stream=stream, backward=True, any_frame=False)
        out: list[np.ndarray] = []
        for frame in container.decode(stream):
            idx = int(round(float(frame.pts * tb) * fps))
            if idx < start:
                continue
            out.append(frame.to_ndarray(format="rgb24"))
            if len(out) >= n:
                break
        if len(out) < n:
            raise EOFError(f"{path}: wanted [{start},{start + n}) got {len(out)} frames")
        return np.stack(out)


def _decode_ffmpeg(path: str, start: int, n: int, fps: float) -> np.ndarray:
    probe = subprocess.run(
        ["ffmpeg", "-hide_banner", "-i", path], capture_output=True, text=True
    ).stderr
    import re

    m = re.search(r"(\d+)x(\d+)[, ]", probe)
    if not m:
        raise RuntimeError(f"cannot parse resolution from ffmpeg output for {path}")
    w, h = int(m.group(1)), int(m.group(2))
    cmd = [
        "ffmpeg", "-hide_banner", "-loglevel", "error",
        "-ss", f"{start / fps:.6f}", "-i", path,
        "-frames:v", str(n), "-f", "rawvideo", "-pix_fmt", "rgb24", "pipe:1",
    ]
    raw = subprocess.run(cmd, capture_output=True, check=True).stdout
    arr = np.frombuffer(raw, dtype=np.uint8)
    if arr.size != n * h * w * 3:
        raise EOFError(f"{path}: expected {n} frames, got {arr.size // (h * w * 3)}")
    return arr.reshape(n, h, w, 3)


def count_frames(path: str) -> int:
    """精确帧数（pyav 数包；ingest 校验用，非热路径）。"""
    if "pyav" in _BACKENDS:
        with av.open(path) as container:
            stream = container.streams.video[0]
            if stream.frames:  # 容器已声明
                return int(stream.frames)
            return sum(1 for _ in container.decode(stream))
    out = subprocess.run(
        ["ffmpeg", "-hide_banner", "-loglevel", "info", "-i", path,
         "-map", "0:v:0", "-f", "null", "-"],
        capture_output=True, text=True,
    ).stderr
    import re

    ms = re.findall(r"frame=\s*(\d+)", out)
    if not ms:
        raise RuntimeError(f"cannot count frames of {path}")
    return int(ms[-1])

"""Video-backed chunks must load like npz ones, at a fraction of the size.

npz is lossless but a single game's 4.6 usable hours came to 71 GB — more
than a training VM's disk before any attempt to reach the 50-100 h knee the
plan targets. These tests pin the two properties that make the video path
usable as a drop-in: the loader returns the same shape and dtype either
way, and the frames survive the round trip well enough to keep the motion
signal an IDM depends on.
"""
import numpy as np
import pytest

from diogenes_phase0.data.gaming500 import _codec_name, _write_frames

cv2 = pytest.importorskip("cv2")


def _frames(n=30, h=64, w=64) -> np.ndarray:
    """A moving bright square on a gradient — structure a codec must keep."""
    out = np.zeros((n, h, w, 3), dtype=np.uint8)
    out[:, :, :, 0] = np.linspace(0, 200, w, dtype=np.uint8)[None, :]
    for t in range(n):
        x = 2 + (t % (w - 14))
        out[t, 20:34, x:x + 12] = 255
    return out


def _read_back(path) -> np.ndarray:
    cap = cv2.VideoCapture(str(path))
    got = []
    ok, f = cap.read()
    while ok:
        got.append(cv2.cvtColor(f, cv2.COLOR_BGR2RGB))
        ok, f = cap.read()
    cap.release()
    return np.stack(got) if got else np.empty((0,))


def test_mp4_round_trip_keeps_shape_and_motion(tmp_path):
    frames = _frames()
    _write_frames(tmp_path, frames, "mp4", 30.0)
    got = _read_back(tmp_path / "p0.mp4")

    assert got.shape == frames.shape, (got.shape, frames.shape)
    assert got.dtype == np.uint8
    # lossy, so not exact — but the frame-to-frame motion that the IDM's
    # difference stem keys on must survive
    src_motion = np.abs(np.diff(frames.astype(np.int16), axis=0)).mean()
    got_motion = np.abs(np.diff(got.astype(np.int16), axis=0)).mean()
    assert got_motion > src_motion * 0.5, (src_motion, got_motion)
    assert np.abs(got.astype(np.int16) - frames.astype(np.int16)).mean() < 20


def test_mp4_is_far_smaller_than_npz(tmp_path):
    frames = _frames(n=60, h=224, w=224)
    npz_dir = tmp_path / "npz"
    mp4_dir = tmp_path / "mp4"
    npz_dir.mkdir()
    mp4_dir.mkdir()
    _write_frames(npz_dir, frames, "npz", 30.0)
    _write_frames(mp4_dir, frames, "mp4", 30.0)

    npz_bytes = (npz_dir / "p0.npz").stat().st_size
    mp4_bytes = (mp4_dir / "p0.mp4").stat().st_size
    # the whole point of the format — a conservative bar, real footage
    # compresses far better than this synthetic clip
    assert mp4_bytes < npz_bytes / 3, (npz_bytes, mp4_bytes)


def test_codec_recorded_in_meta_is_what_was_used():
    """mp4v and h264 are different codecs; meta must not claim h264 blindly."""
    assert _codec_name("npz") == "npz"
    assert _codec_name("mp4") in ("h264", "mp4v")


def test_dataset_loads_video_chunks_like_npz(tmp_path):
    """The loader path used in training must accept the video form."""
    from diogenes_phase0.data.dataset import _load_frames
    from diogenes_phase0.contract import ChunkMeta, ChunkRecord

    frames = _frames(n=20, h=48, w=48)
    _write_frames(tmp_path, frames, "mp4", 30.0)
    meta = ChunkMeta.from_json({
        "sample_key": "s_c0000", "game": "synth", "is_trainable": False,
        "tags": ["method_validation_only"],
        "video": {"file": "p0.mp4", "codec": "h264", "width": 48,
                  "height": 48, "fps": 30.0, "frames": 20},
    })
    rec = ChunkRecord(chunk_dir=tmp_path, meta=meta, n_frames=20)
    got = _load_frames(rec)
    assert got.shape == frames.shape

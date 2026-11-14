"""Presence and magnitude are separate questions and must not be conflated.

The diagnostic exists because a zero R^2 has two readings — the head could
not learn, or the magnitude was never in the pixels. Synthetic cases pin
each combination, since the real dataset showed one of them (pointer
presence visible at 4.44x, magnitude correlation only +0.115) and a metric
that collapsed the two would have called that "no signal".
"""
import numpy as np

from diogenes_phase0.label_signal import label_signal, visual_change


class _Rec:
    def __init__(self, chunk_dir):
        self.chunk_dir = chunk_dir


def _dither(frames: np.ndarray, seed: int = 7) -> np.ndarray:
    """Real footage is never bit-identical between frames; exact zeros would
    make the probe's degenerate branch, not its normal one, the thing under
    test."""
    rng = np.random.default_rng(seed)
    n = rng.integers(0, 3, size=frames.shape, dtype=np.int16)
    return np.clip(frames.astype(np.int16) + n, 0, 255).astype(np.uint8)


def _write(tmp_path, frames, deltas):
    """A minimal contract chunk: npz frames + jsonl actions + meta."""
    import json
    d = tmp_path / "g" / "s" / "c0000"
    d.mkdir(parents=True)
    np.savez_compressed(d / "p0.npz", frames=frames)
    with open(d / "p0.jsonl", "w") as f:
        for i, (dx, dy) in enumerate(deltas):
            f.write(json.dumps({"frame": i, "keys": [], "pointerDx": dx,
                                "pointerDy": dy, "pointerBins": [0, 0]}) + "\n")
    (d / "meta.json").write_text(json.dumps({
        "sample_key": "s_c0000", "game": "synth", "is_trainable": False,
        "tags": ["method_validation_only"],
        "video": {"file": "p0.npz", "codec": "npz", "width": frames.shape[2],
                  "height": frames.shape[1], "fps": 30.0,
                  "frames": len(frames)},
    }))
    return tmp_path


def test_visual_change_tracks_actual_motion():
    still = np.zeros((5, 8, 8, 3), dtype=np.uint8)
    moving = np.stack([np.full((8, 8, 3), i * 40, dtype=np.uint8)
                       for i in range(5)])
    assert visual_change(still).mean() == 0.0
    assert visual_change(moving).mean() > 30.0


def test_label_with_no_visible_counterpart_scores_near_one(tmp_path):
    """Random deltas on a static screen: presence ratio ~1 => noise label."""
    rng = np.random.default_rng(0)
    frames = np.zeros((40, 16, 16, 3), dtype=np.uint8)
    deltas = [(float(rng.integers(-5, 6)), 0.0) for _ in range(40)]
    root = _write(tmp_path, frames, deltas)
    res = label_signal(root, max_chunks=1)
    assert res["presence_ratio"] is None or res["presence_ratio"] < 1.5


def test_label_that_drives_the_pixels_scores_high(tmp_path):
    """Frame changes exactly when the pointer moves."""
    frames = np.zeros((40, 16, 16, 3), dtype=np.uint8)
    deltas = []
    level = 0
    for t in range(40):
        moving = (t % 4 == 0)
        if moving:
            level = (level + 90) % 255
        frames[t] = level
        deltas.append((20.0 if moving else 0.0, 0.0))
    root = _write(tmp_path, _dither(frames), deltas)
    res = label_signal(root, max_chunks=1)
    assert res["presence_ratio"] > 2.0
    assert res["fire_fraction"] > 0.0


def test_presence_can_be_strong_while_magnitude_is_not(tmp_path):
    """The real minecraft pointer case: fires visibly, amount is unrelated."""
    rng = np.random.default_rng(1)
    frames = np.zeros((60, 16, 16, 3), dtype=np.uint8)
    deltas = []
    level = 0
    for t in range(60):
        moving = (t % 3 == 0)
        if moving:
            level = (level + 100) % 255
        frames[t] = level
        # magnitude is random when it fires — visible, but uninformative
        deltas.append((float(rng.integers(1, 100)) if moving else 0.0, 0.0))
    root = _write(tmp_path, _dither(frames), deltas)
    res = label_signal(root, max_chunks=1)
    assert res["presence_ratio"] > 2.0, res
    assert abs(res["magnitude_r"]) < res["presence_ratio"] / 2, res


def test_perfect_separation_is_not_reported_as_unknown(tmp_path):
    """off == 0 with on > 0 is the strongest presence signal, not a gap."""
    frames = np.zeros((30, 16, 16, 3), dtype=np.uint8)
    deltas = []
    level = 0
    for t in range(30):
        moving = (t % 5 == 0)
        if moving:
            level = (level + 120) % 255
        frames[t] = level
        deltas.append((15.0 if moving else 0.0, 0.0))
    root = _write(tmp_path, frames, deltas)     # no dither: exact zeros
    res = label_signal(root, max_chunks=1)
    assert res["visual_change_off"] == 0.0
    assert res["presence_separable"] is True
    assert res["visual_change_on"] > 0.0

"""R0: synth COD 格式 → ingest → 采样，验证 actions 列 + run 边界约束 + dm1-v2 刻度。"""
import json
import shutil
from pathlib import Path

import numpy as np
import pytest

from diogenes_player import contract
from diogenes_player.action_registry import default_cod_vocab
from diogenes_player.dataset import WindowDataset
from diogenes_player.ingest import ingest
from diogenes_player.physical_vocab import default_cod_physical_vocab
from diogenes_player.synth import synth_session

pytestmark = pytest.mark.skipif(shutil.which("ffmpeg") is None, reason="需要 ffmpeg")


@pytest.fixture(scope="module")
def plane(tmp_path_factory):
    src = tmp_path_factory.mktemp("synth_src")
    info = synth_session(src, chunks=4, w=128, h=72, n_runs=2, seed=11)
    out = tmp_path_factory.mktemp("plane")
    stats = ingest(src, out, width=128, height=72, gop=30, scheme="dm1-v2",
                   physical_vocab=default_cod_physical_vocab())
    assert stats["failed"] == []
    return out, info


def test_actions_column_present(plane):
    out, _ = plane
    vocab = default_cod_vocab()
    ds = WindowDataset(out, t=24, vocab=vocab, allow_untrainable=True, seed=1)
    s = ds.sample()
    assert s["actions"].shape == (24, vocab.n_actions)
    assert s["physical_inputs"].shape == (24, ds.physical_vocab.n_inputs)
    assert ds.physical_vocab.n_inputs == 39
    assert np.array_equal(s["keys"], s["physical_inputs"])
    assert s["meta"]["physical_vocab_hash"] == ds.physical_vocab.content_hash
    assert s["actions"].sum() > 0                # synth 有真实动作信号
    assert set(np.unique(s["actions"])) <= {0.0, 1.0}


def test_run_ranges_persisted(plane):
    out, info = plane
    rr = json.loads((out / "run_ranges.json").read_text())
    key = next(iter(rr))
    assert len(rr[key]) == len(info["run_ranges"])


def test_window_never_crosses_run_boundary(plane):
    """所有采样窗口必须完整落在单个 run range 内。"""
    out, info = plane
    ranges = info["run_ranges"]
    t = 24
    ds = WindowDataset(out, t=t, vocab=default_cod_vocab(),
                       allow_untrainable=True, seed=2)
    for _ in range(60):
        c = ds.chunks[ds.rng.integers(len(ds.chunks))]
        start = int(c.valid_starts[ds.rng.integers(c.valid_starts.size)])
        w0 = c.gframe_start + start
        w1 = w0 + t
        assert any(r["frame_start"] <= w0 and w1 <= r["frame_end"]
                   for r in ranges), (w0, w1, ranges)


def test_large_window_rejects_when_runs_too_short(plane):
    """T 超过任一 run range 长度 → 无合法窗口 → 报错。"""
    out, info = plane
    longest = max(r["frame_end"] - r["frame_start"] for r in info["run_ranges"])
    with pytest.raises(ValueError, match="run 边界"):
        WindowDataset(out, t=longest + 1, vocab=default_cod_vocab(),
                      allow_untrainable=True)


def test_bins_recomputed_dm1v2(plane):
    """采样 camera bins 应等于 raw delta 的 dm1-v2 重算（存储刻度一致）。"""
    out, _ = plane
    ds = WindowDataset(out, t=24, vocab=default_cod_vocab(),
                       allow_untrainable=True, seed=5)
    s = ds.sample()
    for i in range(24):
        dx, dy = s["raw_delta"][i]
        assert s["camera_bins"][i, 0] == contract.encode_bin(dx, "dm1-v2") + contract.MAX_BIN
        assert s["camera_bins"][i, 1] == contract.encode_bin(dy, "dm1-v2") + contract.MAX_BIN

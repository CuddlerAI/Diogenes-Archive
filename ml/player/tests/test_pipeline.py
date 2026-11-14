"""端到端管线测试：data_demo → ingest → WindowDataset 采样（W1 冒烟）。"""
import json
import shutil
from pathlib import Path

import numpy as np
import pytest

from diogenes_player import contract
from diogenes_player.dataset import WindowDataset
from diogenes_player.ingest import ingest
from diogenes_player.physical_vocab import PhysicalInputVocab
from diogenes_player.video import count_frames

REPO_ROOT = Path(__file__).resolve().parents[3]
DATASET = REPO_ROOT / "data_demo" / "dataset"

pytestmark = pytest.mark.skipif(
    not DATASET.exists() or shutil.which("ffmpeg") is None,
    reason="需要 data_demo dataset 与 ffmpeg",
)


@pytest.fixture(scope="module")
def plane(tmp_path_factory):
    out = tmp_path_factory.mktemp("train_plane")
    # data_demo 是 legacy μ-law 刻度
    stats = ingest(DATASET, out, width=128, height=72, gop=30,
                   limit_chunks=2, verify_frames=True, scheme="legacy",
                   physical_vocab=PhysicalInputVocab.legacy())
    assert stats["failed"] == [], stats["failed"]
    assert stats["chunks"] > 0
    return out, stats


def test_ingest_counts_consistent(plane):
    out, stats = plane
    index_stats = json.loads((out / "_ingest_stats.json").read_text())
    assert index_stats["frames"] == stats["frames"]
    assert stats["frames"] == stats["chunks"] * 120  # dm-1: 4s@30fps chunk


def test_proxy_frame_count_exact(plane):
    out, _ = plane
    proxies = sorted(out.glob("*/*/c*.mp4"))
    assert proxies
    assert count_frames(str(proxies[0])) == 120


def test_dataset_refuses_untrainable_by_default(plane):
    out, _ = plane
    with pytest.raises(ValueError, match="is_trainable"):
        WindowDataset(out, t=96)  # data_demo 全部三方数据


def test_sample_shapes_and_ranges(plane):
    out, _ = plane
    with pytest.warns(UserWarning, match="管线验证"):
        ds = WindowDataset(out, t=96, allow_untrainable=True, seed=42)
    s = ds.sample()
    assert s["frames"].shape == (96, 72, 128, 3)
    assert s["frames"].dtype == np.uint8
    assert s["keys"].shape == (96, contract.N_KEYS)
    assert set(np.unique(s["keys"])) <= {0.0, 1.0}
    assert s["camera_bins"].shape == (96, 2)
    assert s["camera_bins"].min() >= 0 and s["camera_bins"].max() <= 20
    assert s["actions"].shape == (96, 0)  # data_demo 无 vocab → 空动作


def test_window_matches_source_labels(plane):
    """采样窗口标签与源 p0.jsonl 对应帧一致（bins 用 legacy 重算，与存储一致）。"""
    out, _ = plane
    with pytest.warns(UserWarning):
        ds = WindowDataset(out, t=8, allow_untrainable=True, seed=7)
    c = ds.chunks[0]
    start = 3
    s = ds.get(c, start)
    # 身份来自 meta，源目录名可能不同 → 按 session + chunk 号 glob 定位源 jsonl
    src_jsonl = next(DATASET.glob(f"*/{c.session}/c{c.chunk:04d}/p0.jsonl"))
    rows = [json.loads(l) for l in src_jsonl.open()]
    for i in range(8):
        row = rows[start + i]
        # ingest 用 legacy 从 raw 重算，对 data_demo 应等于存储 pointerBins
        assert s["camera_bins"][i, 0] == row["pointerBins"][0] + contract.MAX_BIN
        assert s["camera_bins"][i, 1] == row["pointerBins"][1] + contract.MAX_BIN
        mask, _ = contract.keys_to_mask(row["keys"])
        assert np.array_equal(s["keys"][i], contract.mask_to_multihot(mask))

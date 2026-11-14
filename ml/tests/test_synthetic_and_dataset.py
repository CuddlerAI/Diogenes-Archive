import json

import numpy as np
import pytest

from diogenes_phase0.data.dataset import WindowDataset, scan_dataset, split_records
from diogenes_phase0.data.synthetic import generate_dataset
from diogenes_phase0.keys import bin_to_class, foveated_bin


@pytest.fixture(scope="module")
def synth_root(tmp_path_factory):
    root = tmp_path_factory.mktemp("synth")
    return generate_dataset(root, games=["synth_a", "synth_b"],
                            sessions_per_game=1, chunks_per_session=2,
                            chunk_frames=40, res=48, seed=3)


def test_layout_and_scan(synth_root):
    records = scan_dataset(synth_root)
    assert len(records) == 4  # 2 games x 1 session x 2 chunks
    r = records[0]
    assert r.n_frames == 40
    assert r.meta.rights == "synthetic"
    frames = np.load(r.frames_path)["frames"]
    assert frames.shape == (40, 48, 48, 3) and frames.dtype == np.uint8


def test_jsonl_bins_consistent(synth_root):
    line = json.loads(next(open(next(synth_root.rglob("p0.jsonl")))))
    assert line["pointerBins"] == [foveated_bin(line["pointerDx"]),
                                   foveated_bin(line["pointerDy"])]


def test_fire_frames_brighter(synth_root):
    """vk=1 (fire) must have a visible consequence — flash raises brightness."""
    means_fire, means_idle = [], []
    for rec in scan_dataset(synth_root):
        frames = np.load(rec.frames_path)["frames"]
        for line in open(rec.actions_path):
            a = json.loads(line)
            (means_fire if 1 in a["keys"] else means_idle).append(
                frames[a["frame"]].mean())
    assert means_fire and means_idle
    assert np.mean(means_fire) > np.mean(means_idle) + 20


def test_split_by_game(synth_root):
    records = scan_dataset(synth_root)
    splits = split_records(records, holdout_games=["synth_b"], val_fraction=0.5)
    assert all(r.meta.game == "synth_b" for r in splits["zero_shot"])
    assert all(r.meta.game != "synth_b" for r in splits["train"] + splits["val"])
    assert len(splits["zero_shot"]) == 2


def test_window_dataset_alignment(synth_root):
    records = scan_dataset(synth_root)
    ds = WindowDataset(records, window=5, stride=1, input_res=(48, 48))
    # every window stays inside one chunk: length = chunks * (40 - 4)
    assert len(ds) == len(records) * 36
    s = ds[0]
    assert s["frames"].shape == (5, 3, 48, 48)
    assert 0.0 <= s["frames"].min() and s["frames"].max() <= 1.0
    # center-frame action: first sample of chunk 0 has center t=2
    a = json.loads(open(records[0].actions_path).readlines()[2])
    assert s["bin_x"].item() == bin_to_class(a["pointerBins"][0])
    assert abs(s["delta"][0].item() * ds.delta_scale
               - np.clip(a["pointerDx"], -200, 200)) < 1e-4


def test_window_dataset_blocks_mixed_fps(tmp_path):
    """30/60fps 两批合成 chunk 混进同一 dataset → 硬门阻断(docs/50 P-4)。"""
    from diogenes_phase0.contract import ComplianceError
    r30 = generate_dataset(tmp_path / "r30", games=["a"], sessions_per_game=1,
                           chunks_per_session=1, chunk_frames=20, res=32, seed=1)
    r60 = generate_dataset(tmp_path / "r60", games=["b"], sessions_per_game=1,
                           chunks_per_session=1, chunk_frames=20, res=32, seed=2,
                           fps=60)
    records = scan_dataset(r30) + scan_dataset(r60)
    with pytest.raises(ComplianceError, match="mixed-fps"):
        WindowDataset(records, window=5, stride=1, input_res=(32, 32))
    # 钉死 expected_fps 后,只喂 60 的那批可以过
    ds = WindowDataset(scan_dataset(r60), window=5, stride=1,
                       input_res=(32, 32), expected_fps=60)
    assert len(ds) > 0

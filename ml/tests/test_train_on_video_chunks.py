"""A real training step over video-backed chunks.

The corpus-scale runs are entirely mp4; if the training path only worked on
npz, that would surface hours in and after the fetch and convert bills were
already paid. The existing codec tests cover _load_frames in isolation,
which is not the path training takes — WindowDataset slices a window,
resizes it and stacks a batch, and the trunk expects a specific layout.
"""
import torch
from torch.utils.data import DataLoader

import pytest

from diogenes_phase0.data.dataset import WindowDataset, scan_dataset
from diogenes_phase0.data.reencode import reencode_dataset
from diogenes_phase0.data.synthetic import generate_dataset
from diogenes_phase0.models.idm import build_model
from diogenes_phase0.train import compute_loss

pytest.importorskip("cv2")

CFG = {"model": {"trunk": "tiny", "d_model": 64, "n_layers": 1, "n_heads": 4},
       "data": {"window": 5}}


def _video_dataset(tmp_path, seed=11):
    src = generate_dataset(tmp_path / "src", games=["synth_v"],
                           sessions_per_game=1, chunks_per_session=2,
                           chunk_frames=60, res=48, seed=seed)
    dst = tmp_path / "mp4"
    reencode_dataset(src, dst, codec="mp4")
    return src, dst


def test_window_sampling_matches_between_npz_and_video(tmp_path):
    src, dst = _video_dataset(tmp_path)
    a = WindowDataset(scan_dataset(src), window=5, input_res=(48, 48))
    b = WindowDataset(scan_dataset(dst), window=5, input_res=(48, 48))
    assert len(a) == len(b)

    xa, xb = a[7], b[7]
    assert xa["frames"].shape == xb["frames"].shape == (5, 3, 48, 48)
    # labels are the same sample, not merely the same shape
    assert torch.equal(xa["keys"], xb["keys"])
    assert xa["bin_x"] == xb["bin_x"] and xa["bin_y"] == xb["bin_y"]
    assert torch.allclose(xa["delta"], xb["delta"])
    # pixels differ (lossy) but must stay in range and not be blank
    assert 0.0 <= float(xb["frames"].min()) and float(xb["frames"].max()) <= 1.0
    assert float(xb["frames"].std()) > 0.01


def test_loss_decreases_training_on_video_chunks(tmp_path):
    _, dst = _video_dataset(tmp_path, seed=12)
    ds = WindowDataset(scan_dataset(dst), window=5, input_res=(48, 48))
    loader = DataLoader(ds, batch_size=16, shuffle=True, drop_last=True)

    torch.manual_seed(0)
    model = build_model(CFG)
    opt = torch.optim.AdamW((p for p in model.parameters() if p.requires_grad),
                            lr=1e-3)
    device = torch.device("cpu")

    losses = []
    for _ in range(3):
        for batch in loader:
            out = model(batch["frames"])
            loss, parts = compute_loss(out, batch, {}, device)
            opt.zero_grad()
            loss.backward()
            opt.step()
            losses.append(parts["loss"])

    assert len(losses) >= 6
    assert sum(losses[-3:]) / 3 < sum(losses[:3]) / 3 * 0.9

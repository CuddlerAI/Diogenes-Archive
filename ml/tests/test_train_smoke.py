"""A few real optimizer steps on tiny synthetic data: loss must go down."""
import torch
from torch.utils.data import DataLoader

from diogenes_phase0.data.dataset import WindowDataset, scan_dataset
from diogenes_phase0.data.synthetic import generate_dataset
from diogenes_phase0.models.idm import build_model
from diogenes_phase0.train import compute_loss

CFG = {"model": {"trunk": "tiny", "d_model": 64, "n_layers": 1, "n_heads": 4},
       "data": {"window": 5}}


def test_loss_decreases(tmp_path):
    root = generate_dataset(tmp_path / "d", games=["synth_a"],
                            sessions_per_game=1, chunks_per_session=2,
                            chunk_frames=60, res=48, seed=1)
    ds = WindowDataset(scan_dataset(root), window=5, stride=1, input_res=(48, 48))
    loader = DataLoader(ds, batch_size=16, shuffle=True, drop_last=True)

    torch.manual_seed(0)
    model = build_model(CFG)
    opt = torch.optim.AdamW((p for p in model.parameters() if p.requires_grad), lr=1e-3)
    device = torch.device("cpu")

    losses = []
    for _ in range(3):  # a few passes over the small set
        for batch in loader:
            out = model(batch["frames"])
            loss, parts = compute_loss(out, batch, {}, device)
            opt.zero_grad()
            loss.backward()
            opt.step()
            losses.append(parts["loss"])

    first = sum(losses[:3]) / 3
    last = sum(losses[-3:]) / 3
    assert last < first * 0.8, (first, last)



def _mini_cfg(root, tmp_path):
    return {
        "run_name": "ck", "seed": 0,
        "output": {"dir": str(tmp_path / "runs")},
        "data": {"roots": [str(root)], "window": 3, "stride": 2,
                 "input_res": [32, 32], "delta_scale": 50.0,
                 "val_fraction": 0.34, "holdout_games": [],
                 "allow_method_validation_only": True},
        "model": {"trunk": "tiny", "d_model": 32, "n_heads": 4, "n_layers": 1},
        "train": {"batch_size": 2, "max_steps": 2, "log_every": 1,
                  "lr": 1e-4, "num_workers": 0},
        "gates": {},
    }


def test_checkpoint_is_written_before_eval(tmp_path, monkeypatch):
    """Weights must reach disk before eval starts.

    Eval on a real val split is itself an hour of work; saving after it
    means a failure there discards the entire training run instead of
    costing only a re-eval.
    """
    import sys
    import pytest
    import yaml
    import diogenes_phase0.train as T

    root = generate_dataset(tmp_path / "d", games=["synth_ck"],
                            sessions_per_game=3, chunks_per_session=1,
                            chunk_frames=24, res=32, seed=11)
    cfg = _mini_cfg(root, tmp_path)
    (tmp_path / "c.yaml").write_text(yaml.safe_dump(cfg))

    def boom(*a, **k):
        raise RuntimeError("eval died")

    monkeypatch.setattr(T, "run_eval", boom)
    monkeypatch.setattr(sys, "argv",
                        ["train", "--config", str(tmp_path / "c.yaml"),
                         "--device", "cpu"])
    with pytest.raises(RuntimeError, match="eval died"):
        T.main()

    found = list((tmp_path / "runs").glob("*/checkpoint.pt"))
    assert found, "training weights were lost when eval failed"
    assert torch.load(found[0], map_location="cpu",
                      weights_only=False)["step"] == 2


def test_save_checkpoint_leaves_no_partial_file(tmp_path):
    """Renamed into place, so a reader never observes a truncated file."""
    import diogenes_phase0.train as T
    d = tmp_path / "r"
    d.mkdir()
    T.save_checkpoint(d, torch.nn.Linear(4, 4), {"x": 1}, 7)
    assert not (d / "checkpoint.pt.tmp").exists()
    assert torch.load(d / "checkpoint.pt", map_location="cpu",
                      weights_only=False)["step"] == 7

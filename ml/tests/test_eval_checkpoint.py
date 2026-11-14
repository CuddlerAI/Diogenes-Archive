"""Cross-game eval must reuse the checkpoint's own sample geometry.

The failure this guards against is silent: evaluating a head on windows of
the wrong size or length still runs and still prints metrics, so a
preprocessing mismatch reads as "the head does not transfer" instead of
raising. These tests pin the geometry to the checkpoint config and let the
frame shape prove it.
"""
import json

import torch

from diogenes_phase0.data.synthetic import generate_dataset
from diogenes_phase0.eval_checkpoint import build_eval_loader, load_checkpoint
from diogenes_phase0.models.idm import build_model

CFG = {
    "run_name": "geom",
    "model": {"trunk": "tiny", "d_model": 64, "n_layers": 1, "n_heads": 4},
    "data": {"roots": ["data/a"], "window": 5, "input_res": [48, 48],
             "delta_scale": 50.0},
    "train": {"batch_size": 4},
}


def _ckpt(tmp_path, cfg=CFG):
    torch.manual_seed(0)
    model = build_model(cfg)
    path = tmp_path / "checkpoint.pt"
    torch.save({"model": model.state_dict(), "config": cfg, "step": 1}, path)
    return path


def test_loader_geometry_comes_from_checkpoint(tmp_path):
    root = generate_dataset(tmp_path / "b", games=["synth_b"],
                            sessions_per_game=1, chunks_per_session=1,
                            chunk_frames=40, res=64, seed=2)
    loader, stats = build_eval_loader(CFG, str(root), batch_size=4)
    batch = next(iter(loader))
    # window=5 and 48x48 come from the config, NOT from the 64px source data
    assert batch["frames"].shape[1:] == (5, 3, 48, 48), batch["frames"].shape
    assert stats["games"] == ["synth_b"]


def test_geometry_follows_a_different_checkpoint(tmp_path):
    """Change the config and the loader must change with it."""
    root = generate_dataset(tmp_path / "c", games=["synth_c"],
                            sessions_per_game=1, chunks_per_session=1,
                            chunk_frames=40, res=64, seed=3)
    cfg = json.loads(json.dumps(CFG))
    cfg["data"]["window"] = 3
    cfg["data"]["input_res"] = [32, 32]
    loader, _ = build_eval_loader(cfg, str(root), batch_size=4)
    assert next(iter(loader))["frames"].shape[1:] == (3, 3, 32, 32)


def test_round_trip_checkpoint_then_eval(tmp_path):
    """Saved head reloads with identical weights and scores its own data."""
    from diogenes_phase0.evaluate import run_eval

    path = _ckpt(tmp_path)
    model, cfg = load_checkpoint(path, torch.device("cpu"))
    root = generate_dataset(tmp_path / "d", games=["synth_d"],
                            sessions_per_game=1, chunks_per_session=1,
                            chunk_frames=40, res=48, seed=4)
    loader, _ = build_eval_loader(cfg, str(root), batch_size=4)
    m = run_eval(model, loader, torch.device("cpu"))
    assert m["n"] > 0
    # all three key readings are present and ordered lenient >= strict
    assert m["key_per_key_acc"] >= m["key_exact"]
    assert set(m) >= {"key_exact", "key_per_key_acc", "key_macro_f1"}


def test_val_split_is_a_strict_subset_of_all(tmp_path):
    """Same-game re-eval must exclude trained-on frames.

    Scoring a checkpoint over every chunk of its own game silently includes
    the training frames, producing a number that looks like the gate
    report's metric but is not comparable to it.
    """
    from diogenes_phase0.data.synthetic import generate_dataset

    root = generate_dataset(tmp_path / "s", games=["synth_s"],
                            sessions_per_game=6, chunks_per_session=1,
                            chunk_frames=30, res=32, seed=9)
    cfg = json.loads(json.dumps(CFG))
    cfg["data"]["val_fraction"] = 0.5
    cfg["data"]["input_res"] = [32, 32]
    all_loader, all_stats = build_eval_loader(cfg, str(root), 4, split="all")
    val_loader, val_stats = build_eval_loader(cfg, str(root), 4, split="val")

    assert val_stats["split"] == "val" and all_stats["split"] == "all"
    assert 0 < val_stats["chunks"] < all_stats["chunks"]
    assert len(val_loader.dataset) < len(all_loader.dataset)


def test_unknown_split_is_rejected(tmp_path):
    from diogenes_phase0.data.synthetic import generate_dataset

    root = generate_dataset(tmp_path / "u", games=["synth_u"],
                            sessions_per_game=2, chunks_per_session=1,
                            chunk_frames=30, res=32, seed=10)
    import pytest
    with pytest.raises(SystemExit):
        build_eval_loader(CFG, str(root), 4, split="nope")

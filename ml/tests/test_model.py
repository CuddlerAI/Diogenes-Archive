import torch

from diogenes_phase0.keys import N_BINS, N_KEYS
from diogenes_phase0.models.idm import TRAINABLE_BUDGET, build_model, trainable_params

CFG = {"model": {"trunk": "tiny", "d_model": 128, "n_layers": 2, "n_heads": 4},
       "data": {"window": 5}}


def test_forward_shapes():
    model = build_model(CFG)
    out = model(torch.rand(2, 5, 3, 96, 96))
    assert out["keys"].shape == (2, N_KEYS)
    assert out["bin_x"].shape == (2, N_BINS)
    assert out["bin_y"].shape == (2, N_BINS)
    assert out["delta"].shape == (2, 2)


def test_param_budget_and_frozen_trunk():
    model = build_model(CFG)
    assert trainable_params(model) < TRAINABLE_BUDGET
    assert all(not p.requires_grad for p in model.trunk.parameters())
    assert any(p.requires_grad for p in model.head.parameters())
    # trunk must stay in eval mode even after model.train()
    model.train()
    assert not model.trunk.encoder.training


def test_head_is_non_causal():
    """The IDM must SEE the future: perturbing frame t+2 changes the center
    prediction (this is the whole point of the non-causal design)."""
    torch.manual_seed(0)
    model = build_model(CFG).eval()
    x = torch.rand(1, 5, 3, 96, 96)
    y1 = model(x)["keys"]
    x2 = x.clone()
    x2[:, 4] = torch.rand_like(x2[:, 4])  # perturb the last (future) frame
    y2 = model(x2)["keys"]
    assert (y1 - y2).abs().max().item() > 1e-6


def test_gradients_flow_only_to_head():
    model = build_model(CFG)
    out = model(torch.rand(2, 5, 3, 96, 96))
    out["keys"].sum().backward()
    assert all(p.grad is None for p in model.trunk.parameters())
    grads = [p.grad for p in model.head.parameters() if p.grad is not None]
    assert grads and any(g.abs().sum() > 0 for g in grads)

"""R0: policy 形状 / step-vs-unrolled 一致性 / 过拟合冒烟 / 损失模块（语义动作头）。"""
import pytest

torch = pytest.importorskip("torch")

from diogenes_player import contract  # noqa: E402
from diogenes_player.models.losses import bc_loss, estimate_pos_weight  # noqa: E402
from diogenes_player.models.policy import PlayerPolicy, PolicyConfig  # noqa: E402

N_ACT = 26


@pytest.fixture(scope="module")
def tiny_policy():
    torch.manual_seed(0)
    return PlayerPolicy(PolicyConfig(encoder="tiny_cnn", d_model=64,
                                     gru_hidden=96, n_actions=N_ACT,
                                     freeze_encoder=False))


def test_forward_shapes(tiny_policy):
    b, t = 2, 8
    frames = torch.rand(b, t, 3, 72, 128)
    actions, camera, h = tiny_policy(frames)
    assert actions.shape == (b, t, N_ACT)
    assert camera.shape == (b, t, 2, contract.N_BINS)
    assert h.shape == (1, b, 96)


def test_step_matches_unrolled(tiny_policy):
    """单步推理链 == 整段 forward —— W3 导出链的正确性前提。"""
    tiny_policy.eval()
    t = 6
    frames = torch.rand(1, t, 3, 72, 128)
    with torch.no_grad():
        act_seq, cam_seq, _ = tiny_policy(frames)
    h = tiny_policy.init_hidden(1)
    for i in range(t):
        act_i, cam_i, h = tiny_policy.step(frames[:, i], h)
        assert torch.allclose(act_i, act_seq[:, i], atol=1e-5), f"t={i}"
        assert torch.allclose(cam_i, cam_seq[:, i], atol=1e-5), f"t={i}"


def test_overfit_smoke():
    """固定 2 个合成样本, 400 步内损失显著下降 (>60%)。"""
    torch.manual_seed(1)
    policy = PlayerPolicy(PolicyConfig(encoder="tiny_cnn", d_model=64,
                                       gru_hidden=96, n_actions=N_ACT,
                                       freeze_encoder=False))
    b, t = 2, 12
    frames = 0.5 * torch.rand(b, t, 3, 72, 128) \
        + 0.5 * torch.linspace(0, 1, b * t).view(b, t, 1, 1, 1)
    actions = (torch.rand(b, t, N_ACT) < 0.15).float()
    camera = torch.randint(0, contract.N_BINS, (b, t, 2))
    opt = torch.optim.AdamW(policy.parameters(), lr=5e-3)
    first = last = None
    for _ in range(400):
        a, c, _ = policy(frames)
        losses = bc_loss(a, c, actions, camera, burn_in=2)
        opt.zero_grad()
        losses["loss"].backward()
        opt.step()
        last = float(losses["loss"])
        first = first or last
    assert last < first * 0.4, (first, last)


def test_burn_in_excludes_prefix():
    torch.manual_seed(2)
    a = torch.randn(1, 10, N_ACT)
    c = torch.randn(1, 10, 2, contract.N_BINS)
    ta = torch.zeros(1, 10, N_ACT)
    tc = torch.zeros(1, 10, 2, dtype=torch.long)
    full = bc_loss(a, c, ta, tc, burn_in=0)["loss"]
    tail = bc_loss(a, c, ta, tc, burn_in=4)["loss"]
    manual = bc_loss(a[:, 4:], c[:, 4:], ta[:, 4:], tc[:, 4:], burn_in=0)["loss"]
    assert torch.allclose(tail, manual)
    assert not torch.allclose(full, tail)


def test_pos_weight():
    labels = torch.zeros(100, N_ACT)
    labels[:5, 0] = 1.0   # 5% 阳性 → weight 19
    labels[:50, 1] = 1.0  # 50% → weight 1
    w = estimate_pos_weight(labels)
    assert abs(w[0] - 19.0) < 1e-4
    assert abs(w[1] - 1.0) < 1e-4
    assert w[2] == 50.0   # 全阴性 → clamp 上限


def test_aux_keys_optional():
    p_off = PlayerPolicy(PolicyConfig(encoder="tiny_cnn", n_actions=N_ACT))
    assert p_off.keys_head is None
    p_on = PlayerPolicy(PolicyConfig(encoder="tiny_cnn", n_actions=N_ACT, aux_keys=True))
    frames = torch.rand(1, 4, 3, 72, 128)
    assert p_on.aux_keys_logits(frames).shape == (1, 4, contract.N_KEYS)


def test_frozen_encoder_stays_frozen():
    p = PlayerPolicy(PolicyConfig(encoder="tiny_cnn", n_actions=N_ACT,
                                  freeze_encoder=True))
    assert all(not x.requires_grad for x in p.encoder.parameters())
    assert p.num_params()["trainable"] < p.num_params()["total"]


def test_param_budget():
    """默认配置可训练参数须在 <10M 预算内 (research/04 §4.1)。"""
    p = PlayerPolicy(PolicyConfig(n_actions=N_ACT))
    assert p.num_params()["trainable"] < 10_000_000

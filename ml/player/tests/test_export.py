"""W3: ONNX 导出 + ORT 单步循环 vs torch 整段 forward 数值对账。"""
import pytest

torch = pytest.importorskip("torch")
pytest.importorskip("onnxruntime")

from diogenes_player.deploy.export import (  # noqa: E402
    StepWrapper, bench_ort, export_onnx, parity_check)
from diogenes_player.models.policy import PlayerPolicy, PolicyConfig  # noqa: E402

H, W = 72, 128


N_ACT = 26


@pytest.fixture(scope="module")
def policy():
    torch.manual_seed(0)
    p = PlayerPolicy(PolicyConfig(encoder="tiny_cnn", d_model=64,
                                  gru_hidden=96, n_actions=N_ACT,
                                  freeze_encoder=False))
    p.eval()
    return p


@pytest.fixture(scope="module")
def onnx_path(policy, tmp_path_factory):
    out = tmp_path_factory.mktemp("onnx") / "policy_step.onnx"
    export_onnx(policy, str(out), height=H, width=W)
    assert out.exists() and out.stat().st_size > 10_000
    return str(out)


def test_parity_torch_vs_ort(policy, onnx_path):
    """fp32 容差 1e-4 (research/04 §7 对账门)。"""
    parity = parity_check(policy, onnx_path, height=H, width=W, t=32)
    assert parity["max_abs_diff_actions"] < 1e-4, parity
    assert parity["max_abs_diff_camera"] < 1e-4, parity


def test_hidden_state_carries_information(policy, onnx_path):
    """h_in 置零 vs 携带历史必须产生不同输出 —— 防止导出图断开 recurrence。"""
    import numpy as np
    import onnxruntime as ort

    sess = ort.InferenceSession(onnx_path, providers=["CPUExecutionProvider"])
    rng = np.random.default_rng(1)
    frame = rng.random((1, 3, H, W), dtype=np.float32)
    h0 = np.zeros((1, 1, 96), dtype=np.float32)
    # 先喂 8 帧攒历史
    h = h0
    for _ in range(8):
        _, _, h = sess.run(None, {"frame": rng.random((1, 3, H, W), dtype=np.float32),
                                  "h_in": h})
    act_hist, _, _ = sess.run(None, {"frame": frame, "h_in": h})
    act_cold, _, _ = sess.run(None, {"frame": frame, "h_in": h0})
    assert abs(act_hist - act_cold).max() > 1e-6


def test_step_wrapper_shapes(policy):
    w = StepWrapper(policy)
    frame = torch.rand(1, 3, H, W)
    h = policy.init_hidden(1)
    actions, camera, h_out = w(frame, h)
    assert actions.shape == (1, N_ACT)
    assert camera.shape == (1, 2, 21)
    assert h_out.shape == (1, 1, 96)


def test_bench_runs(onnx_path):
    r = bench_ort(onnx_path, height=H, width=W, gru_layers=1, gru_hidden=96, n=30)
    assert r["p50_ms"] > 0

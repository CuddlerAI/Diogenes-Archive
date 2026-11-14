"""R0 端到端: synth COD 平面 → 训练 → 离线评估 → 回归门（语义动作路径）。"""
import shutil
from pathlib import Path

import pytest

torch = pytest.importorskip("torch")

from diogenes_player.action_registry import default_cod_vocab  # noqa: E402
from diogenes_player.eval.offline import check_gate, evaluate, load_policy  # noqa: E402
from diogenes_player.ingest import ingest  # noqa: E402
from diogenes_player.physical_vocab import default_cod_physical_vocab  # noqa: E402
from diogenes_player.synth import synth_session  # noqa: E402
from diogenes_player.train.bc import build_parser, train  # noqa: E402

pytestmark = pytest.mark.skipif(
    shutil.which("ffmpeg") is None, reason="需要 ffmpeg")


@pytest.fixture(scope="module")
def plane(tmp_path_factory):
    src = tmp_path_factory.mktemp("synth_src")
    synth_session(src, chunks=3, w=128, h=72, n_runs=2, seed=3)
    out = tmp_path_factory.mktemp("plane_r0")
    stats = ingest(src, out, width=128, height=72, gop=30, scheme="dm1-v2",
                   physical_vocab=default_cod_physical_vocab())
    assert stats["failed"] == []
    assert stats["scheme"] == "dm1-v2"
    return out


@pytest.fixture(scope="module")
def run(plane, tmp_path_factory):
    out = tmp_path_factory.mktemp("run_r0")
    args = build_parser().parse_args([
        "--plane", str(plane), "--out", str(out),
        "--steps", "30", "--batch", "2", "--t", "24", "--burn-in", "4",
        "--hidden", "96", "--d-model", "64", "--workers", "0",
        "--device", "cpu", "--allow-untrainable", "--log-every", "10",
    ])
    return train(args), out


def test_train_produces_artifacts(run):
    summary, out = run
    assert (out / "ckpt.pt").exists()
    ckpt = torch.load(out / "ckpt.pt", map_location="cpu", weights_only=False)
    assert ckpt["config"]["n_actions"] == default_cod_vocab().n_actions
    assert ckpt["vocab"]["name"] == "cod-steam"
    assert ckpt["config"]["n_keys"] == 39
    assert ckpt["physical_input_vocab_hash"] \
        == ckpt["physical_input_vocab"]["content_hash"]
    lines = (out / "log.jsonl").read_text().strip().splitlines()
    assert len(lines) == 30


def test_eval_and_gate(run, plane):
    _, out = run
    model, vocab, physical_vocab, scheme = load_policy(
        str(out / "ckpt.pt"), torch.device("cpu"))
    assert scheme == "dm1-v2"
    metrics = evaluate(model, str(plane), vocab,
                       physical_vocab=physical_vocab, n=4, t=24,
                       allow_untrainable=True, seed=7, device=torch.device("cpu"))
    assert "action_macro_f1_active" in metrics
    assert 0.0 <= metrics["camera_top1_yaw"] <= 1.0
    assert "small_delta_top1_yaw" in metrics
    # 回归门: 自比 → 过; 抬高 baseline → 失败
    assert check_gate(metrics, metrics, tol=0.02) == []
    inflated = {k: (v + 0.5 if isinstance(v, float) else v) for k, v in metrics.items()}
    assert check_gate(metrics, inflated, tol=0.02) != []

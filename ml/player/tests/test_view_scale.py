"""视角尺度标定（docs/45 §123 训练派生层，#284）在训练管线的消费。

核心不变量：**原始鼠标计数是灵敏度相关的**。counts_per_360 标定后 converter 才发
look_yaw/pitch_delta_rad（弧度、与灵敏度和游戏无关）。未标定的数据必须**显式可见**，
不能让 camera 头静默地训成"锁死在某一套灵敏度上"。
"""
import shutil

import numpy as np
import pytest

from diogenes_player import contract
from diogenes_player.action_registry import default_cod_vocab
from diogenes_player.dataset import WindowDataset
from diogenes_player.ingest import ingest
from diogenes_player.preflight import preflight
from diogenes_player.physical_vocab import default_cod_physical_vocab
from diogenes_player.synth import synth_session


def test_parse_reads_angle_fields_when_calibrated():
    row = {"frame": 0, "gframe": 0, "keys": [], "pointerDx": 120, "pointerDy": -14,
           "pointerBins": [8, -2], "pointerRawDx": 120, "pointerRawDy": -14,
           "look_yaw_delta_rad": 0.0419, "look_pitch_delta_rad": -0.0049,
           "view_scale_valid": True, "src": "rec"}
    fa = contract.parse_frame_row(row, verify_bins=False, scheme="dm1-v2")
    assert fa.view_scale_valid is True
    assert fa.look_yaw_delta_rad == pytest.approx(0.0419)
    assert fa.look_pitch_delta_rad == pytest.approx(-0.0049)


def test_parse_uncalibrated_is_none_not_zero():
    """未标定必须是 None，不能是 0.0——0 会被误读成"没转视角"。"""
    row = {"frame": 0, "gframe": 0, "keys": [], "pointerDx": 120, "pointerDy": 0,
           "pointerBins": [8, 0], "look_yaw_delta_rad": None,
           "look_pitch_delta_rad": None, "view_scale_valid": False, "src": "rec"}
    fa = contract.parse_frame_row(row, verify_bins=False, scheme="dm1-v2")
    assert fa.look_yaw_delta_rad is None
    assert fa.look_pitch_delta_rad is None
    assert fa.view_scale_valid is False


def test_pre284_data_defaults_to_uncalibrated():
    """#284 之前的数据没有这些字段——必须默认未标定，而不是崩溃或假装已标定。"""
    row = {"frame": 0, "gframe": 0, "keys": [], "pointerDx": 60, "pointerDy": 0,
           "pointerBins": [6, 0], "src": "rec"}
    fa = contract.parse_frame_row(row, verify_bins=False, scheme="dm1-v2")
    assert fa.view_scale_valid is False and fa.look_yaw_delta_rad is None


@pytest.mark.skipif(shutil.which("ffmpeg") is None, reason="需要 ffmpeg")
def test_uncalibrated_plane_reports_zero_coverage_and_warns(tmp_path):
    src = tmp_path / "src"
    synth_session(src, chunks=2, w=128, h=72, n_runs=1, seed=5, calibrated=False)
    stats = ingest(src, tmp_path / "plane", width=128, height=72, gop=30,
                   scheme="dm1-v2", physical_vocab=default_cod_physical_vocab())
    assert stats["failed"] == []
    assert stats["view_scale_coverage_pct"] == 0.0

    report = preflight(src, scheme="dm1-v2",
                       physical_vocab=default_cod_physical_vocab())
    assert report["view_scale_coverage_pct"] == 0.0
    # 未标定不阻断（现阶段全部语料如此），但必须显式警告
    assert report["blocking"] == []
    assert any("视角尺度未标定" in w for w in report.get("warnings", []))


@pytest.mark.skipif(shutil.which("ffmpeg") is None, reason="需要 ffmpeg")
def test_calibrated_plane_carries_radians_through_to_samples(tmp_path):
    src = tmp_path / "src"
    synth_session(src, chunks=2, w=128, h=72, n_runs=1, seed=6, calibrated=True)
    out = tmp_path / "plane"
    stats = ingest(src, out, width=128, height=72, gop=30, scheme="dm1-v2",
                   physical_vocab=default_cod_physical_vocab())
    assert stats["view_scale_coverage_pct"] == 100.0

    ds = WindowDataset(out, t=24, vocab=default_cod_vocab(),
                       allow_untrainable=True, seed=1)
    s = ds.sample()
    assert s["look_delta_rad"].shape == (24, 2)
    assert s["view_scale_valid"].all()
    assert not np.isnan(s["look_delta_rad"]).any()
    # 弧度应与 raw 计数按 counts_per_360=18000 成正比（synth 的换算）
    expected = s["raw_delta"][:, 0] * 2 * np.pi / 18000
    assert np.allclose(s["look_delta_rad"][:, 0], expected, atol=1e-5)


@pytest.mark.skipif(shutil.which("ffmpeg") is None, reason="需要 ffmpeg")
def test_uncalibrated_samples_are_nan_not_zero(tmp_path):
    """未标定平面的角度必须是 NaN——0 会被下游当成合法的"没转"。"""
    src = tmp_path / "src"
    synth_session(src, chunks=2, w=128, h=72, n_runs=1, seed=7, calibrated=False)
    out = tmp_path / "plane"
    ingest(src, out, width=128, height=72, gop=30, scheme="dm1-v2",
           physical_vocab=default_cod_physical_vocab())
    ds = WindowDataset(out, t=24, vocab=default_cod_vocab(),
                       allow_untrainable=True, seed=2)
    s = ds.sample()
    assert np.isnan(s["look_delta_rad"]).all()
    assert not s["view_scale_valid"].any()

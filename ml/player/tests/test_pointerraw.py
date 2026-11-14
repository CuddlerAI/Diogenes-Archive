"""R0c: pointerRawDx/Dy（#137）—— 未截断真实 Δ 优先，bins 仍从 clamped，向后兼容。"""
import shutil
from pathlib import Path

import numpy as np
import pytest

from diogenes_player import contract
from diogenes_player.action_registry import default_cod_vocab
from diogenes_player.dataset import WindowDataset
from diogenes_player.ingest import ingest
from diogenes_player.physical_vocab import default_cod_physical_vocab
from diogenes_player.synth import synth_session


def test_parse_reads_raw_prefers_over_clamped():
    row = {"frame": 0, "gframe": 0, "keys": [], "actions": [],
           "pointerDx": 200, "pointerDy": -14, "pointerBins": [10, -2],
           "pointerRawDx": 340, "pointerRawDy": -14, "src": "rec"}
    fa = contract.parse_frame_row(row, scheme="dm1-v2")
    assert fa.dx == 200 and fa.raw_dx == 340      # dx=clamp, raw=真实
    assert fa.raw_dy == -14
    # bins 从 clamped 派生，且 encode_bin 内部 clamp ⇒ raw 也给同 bin（strict-bins 不破）
    assert fa.bin_x == 10
    assert contract.encode_bin(fa.raw_dx, "dm1-v2") == contract.encode_bin(fa.dx, "dm1-v2")


def test_parse_backward_compat_no_raw_field():
    """pre-#137 数据无 pointerRawDx → 回退到 pointerDx。"""
    row = {"frame": 0, "gframe": 0, "keys": [], "pointerDx": 120, "pointerDy": 0,
           "pointerBins": [8, 0], "src": "rec"}
    fa = contract.parse_frame_row(row, scheme="dm1-v2")
    assert fa.raw_dx == 120 and fa.raw_dy == 0    # 回退 == clamped


def test_strict_bins_unaffected_by_raw():
    """raw>200 但 bins 仍是 clamped 的 foveated_bin → strict-bins 不误报。"""
    row = {"frame": 0, "gframe": 0, "keys": [], "pointerDx": 200, "pointerDy": 0,
           "pointerRawDx": 999, "pointerRawDy": 0, "pointerBins": [10, 0], "src": "rec"}
    fa = contract.parse_frame_row(row, verify_bins=True, scheme="dm1-v2")  # 不抛
    assert fa.bin_x == 10


@pytest.mark.skipif(shutil.which("ffmpeg") is None, reason="需要 ffmpeg")
def test_raw_delta_flows_to_dataset(tmp_path):
    """synth 产 raw≠clamped 的甩枪帧 → dataset.raw_delta 拿到未截断值。"""
    src = tmp_path / "src"
    synth_session(src, chunks=3, w=128, h=72, n_runs=1, seed=99)
    out = tmp_path / "plane"
    stats = ingest(src, out, width=128, height=72, gop=30, scheme="dm1-v2",
                   strict_bins=True,
                   physical_vocab=default_cod_physical_vocab())
    assert stats["failed"] == []
    ds = WindowDataset(out, t=48, vocab=default_cod_vocab(),
                       allow_untrainable=True, seed=1)
    # 扫多个窗口，确认存在 |raw_delta|>200 的帧（clamped 永远做不到）
    seen_big = False
    for _ in range(20):
        rd = ds.sample()["raw_delta"]
        if np.abs(rd).max() > contract.REF:
            seen_big = True
            break
    assert seen_big, "synth 应产生 >200px 甩枪，raw_delta 未截断才对"

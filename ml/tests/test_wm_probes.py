"""diogenes_wm 探针与探底的单元测试(纯 numpy 合成数据, 无 cv2/ffmpeg 依赖)."""
import json
from pathlib import Path

import numpy as np
import pytest

from diogenes_phase0.contract import ActionFrame
from diogenes_wm.eval.probes import phase_shift, view_probe, drift_probe
from diogenes_wm.eval.rollout import (
    DummyLastFrameModel, DummyNoiseDriftModel, rollout,
)
from diogenes_wm.tokenizer_probe import backend_resize, psnr, region_slices


def _textured(h=64, w=96, seed=0):
    rng = np.random.default_rng(seed)
    base = rng.integers(0, 255, (h, w), np.uint8).astype(np.float32)
    # 低通一下让相位相关有结构可锁
    k = np.ones((5, 5)) / 25
    from numpy.lib.stride_tricks import sliding_window_view
    pad = np.pad(base, 2, mode="wrap")
    sm = (sliding_window_view(pad, (5, 5)) * k).sum(axis=(2, 3))
    return np.stack([sm] * 3, axis=-1).astype(np.uint8)


def _af(i, dx=0.0, dy=0.0):
    return ActionFrame(frame=i, keys=[], pointer_dx=dx, pointer_dy=dy,
                      pointer_bins=(0, 0))


def test_phase_shift_recovers_known_translation():
    img = _textured()
    shifted = np.roll(img, shift=(0, 7), axis=(0, 1))  # 右移 7px
    dx, dy = phase_shift(img.mean(-1).astype(np.float32),
                         shifted.mean(-1).astype(np.float32))
    assert abs(abs(dx) - 7) <= 1 and abs(dy) <= 1


def test_view_probe_couples_on_synthetic_pan():
    img = _textured()
    frames, actions, off = [img], [], 0
    deltas = [8, -6, 10, -12, 7, 9, -8, 11]
    for i, d in enumerate(deltas):
        off += d
        frames.append(np.roll(img, shift=(0, off), axis=(0, 1)))
        actions.append(_af(i, dx=float(d)))
    res = view_probe(np.stack(frames), actions, min_delta=3.0, downscale=1,
                     estimator="phase")   # roll 是循环平移, phase 的主场
    assert res.n_pairs == len(deltas)
    assert res.coupling > 0.9          # 平移与"鼠标"应强耦合
    res0 = view_probe(np.stack([img] * 5),
                      [_af(i, dx=9.0) for i in range(4)], downscale=1,
                      estimator="phase")
    assert res0.coupling < 0.5         # 静止画面 + 大动作 → 无耦合


def test_view_probe_flow_estimator_if_cv2():
    cv2 = pytest.importorskip("cv2")
    img = _textured(96, 128)
    frames, actions = [img], []
    deltas = [4, -3, 5, -6, 4, -5]
    off = 0.0
    for i, d in enumerate(deltas):
        off += d
        m = np.float32([[1, 0, off], [0, 1, 0]])
        frames.append(cv2.warpAffine(img, m, (128, 96),
                                     borderMode=cv2.BORDER_REFLECT))
        actions.append(_af(i, dx=float(d)))
    res = view_probe(np.stack(frames), actions, min_delta=2.0, downscale=1,
                     estimator="flow")
    assert res.estimator == "flow"
    assert res.coupling > 0.8


def test_drift_probe_controls():
    img = _textured()
    ref = np.stack([img] * 20)
    actions = [_af(i) for i in range(20)]
    still = rollout(DummyLastFrameModel(), img, actions)
    assert drift_probe(still, ref).collapse_at is None      # 负对照不崩
    noisy = rollout(DummyNoiseDriftModel(sigma=25.0), img, actions)
    dp = drift_probe(noisy, ref, collapse_db=20.0)
    assert dp.collapse_at is not None                        # 正对照必须被抓到
    assert dp.psnr_curve[-1] < dp.psnr_curve[0]


def test_tokenizer_probe_resize_backend():
    frames = np.stack([_textured(64, 96, seed=s) for s in range(3)])
    recon = backend_resize(frames, (48, 32))                 # 整数倍, 走 numpy 退路
    assert recon.shape == frames.shape
    score = psnr(frames, recon)
    assert 10 < score < 45                                   # 有损但有限
    regs = region_slices(64, 96)
    assert set(regs) == {"full", "center", "hud_top", "hud_bottom"}


def test_report_runs_on_contract_chunks(tmp_path: Path):
    """契约格式的 npz 迷你数据集 → report.run 全链路冒烟。"""
    from diogenes_wm.eval.report import run
    img = _textured(32, 48)
    cdir = tmp_path / "g" / "s0001" / "c0000"
    cdir.mkdir(parents=True)
    frames = np.stack([np.roll(img, (0, 3 * i), (0, 1)) for i in range(8)])
    np.savez(cdir / "p0.npz", frames=frames)
    rows = [dict(frame=i, keys=[87], pointerDx=12.0, pointerDy=0.0,
                 pointerBins=[2, 0], events=[], src="test") for i in range(8)]
    (cdir / "p0.jsonl").write_text("\n".join(json.dumps(r) for r in rows))
    (cdir / "meta.json").write_text(json.dumps(dict(
        sample_key="s0001_c0000", game="halo", rights="licensed",
        first_party=False, consent_id="msft-halo-2026", is_trainable=True,
        is_cutscene=False, video=dict(fps=30))))
    report = run(tmp_path, max_chunks=2)
    assert "E1a@真值 s0001_c0000" in report
    assert "E2@dummy-last" in report and "正对照" in report

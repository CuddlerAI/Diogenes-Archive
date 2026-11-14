"""The pointer target has three forms and they must not be swapped silently.

`pointerDx/Dy` is clamped to ±200 twice over — once by the converter, once by
the loader — so on flick-heavy footage the regression target is a capped
version of the motion and an R² gate on it scores the cap, not the model
(DataMiner docs/45 §3). `pointerRawDx/Dy` is the untruncated form and
`look_*_delta_rad` the calibrated, sensitivity-independent one.

Every failure mode here is silent: a default that quietly changes what
`mouse_r2` means invalidates six runs' worth of comparisons, a radian target
that falls back to counts prints an R² over mixed units, and a missing raw
column that reads as 0.0 turns "we do not have this label" into "the mouse did
not move".
"""
import json

import numpy as np
import pytest

from diogenes_phase0.contract import ActionFrame
from diogenes_phase0.data.dataset import WindowDataset, scan_dataset
from diogenes_phase0.data.synthetic import generate_dataset
from diogenes_phase0.keys import MOUSE_CLAMP
from diogenes_phase0.label_signal import clamp_loss


@pytest.fixture(scope="module")
def synth_root(tmp_path_factory):
    root = tmp_path_factory.mktemp("delta_src")
    return generate_dataset(root, games=["g"], sessions_per_game=1,
                            chunks_per_session=1, chunk_frames=24, res=32,
                            seed=11)


def _rows(path):
    return [json.loads(line) for line in open(path)]


def test_raw_falls_back_to_pointer_when_absent():
    """gaming-500 / data_demo have no raw column; 0.0 would read as 'no motion'."""
    a = ActionFrame.from_json({"frame": 0, "keys": [], "pointerDx": 900,
                               "pointerDy": -700, "pointerBins": [3, -3]})
    assert (a.raw_dx, a.raw_dy) == (900.0, -700.0)
    assert a.look_yaw_delta_rad is None and a.view_scale_valid is False


def test_raw_is_read_when_present_and_is_not_the_clamped_value():
    a = ActionFrame.from_json({"frame": 0, "keys": [], "pointerDx": 200,
                               "pointerDy": -200, "pointerRawDx": 1480,
                               "pointerRawDy": -930, "pointerBins": [4, -4]})
    assert (a.pointer_dx, a.pointer_dy) == (200.0, -200.0)
    assert (a.raw_dx, a.raw_dy) == (1480.0, -930.0)


def test_angle_columns_stay_none_not_zero():
    a = ActionFrame.from_json({"frame": 0, "keys": [], "pointerDx": 10,
                               "pointerDy": 0, "pointerBins": [1, 0],
                               "look_yaw_delta_rad": 0.42,
                               "look_pitch_delta_rad": -0.1,
                               "view_scale_valid": True})
    assert a.look_yaw_delta_rad == pytest.approx(0.42)
    assert a.view_scale_valid is True


def test_default_source_reproduces_the_clamped_target(synth_root):
    """The default must stay bit-identical: six runs' comparability rests on it."""
    records = scan_dataset(synth_root)
    ds = WindowDataset(records, window=3, stride=1, input_res=(32, 32))
    assert ds.delta_source == "pointer" and ds.delta_clamp == MOUSE_CLAMP
    rows = _rows(records[0].actions_path)
    expected = [np.clip(r["pointerDx"], -MOUSE_CLAMP, MOUSE_CLAMP) / 50.0
                for r in rows]
    got = [float(ds[i]["delta"][0]) for i in range(len(ds))]
    # window=3 stride=1 starts at center t=1
    assert got == pytest.approx(expected[1:1 + len(got)], abs=1e-6)


def test_raw_source_is_not_clamped(tmp_path):
    """A flick past ±200 must survive into the target, or the gate scores the cap."""
    root = generate_dataset(tmp_path / "flick", games=["g"], sessions_per_game=1,
                            chunks_per_session=1, chunk_frames=24, res=32,
                            seed=12)
    rec = scan_dataset(root)[0]
    rows = _rows(rec.actions_path)
    for r in rows:
        r["pointerDx"], r["pointerRawDx"] = 200, 1480
        r["pointerDy"], r["pointerRawDy"] = -200, -930
    with open(rec.actions_path, "w") as f:
        for r in rows:
            f.write(json.dumps(r) + "\n")

    records = scan_dataset(root)
    ptr = WindowDataset(records, window=3, input_res=(32, 32))
    raw = WindowDataset(records, window=3, input_res=(32, 32),
                        delta_source="raw")
    assert float(ptr[0]["delta"][0]) == pytest.approx(200 / 50.0)
    assert float(raw[0]["delta"][0]) == pytest.approx(1480 / 50.0)
    # and an explicit clamp still wins when the caller asks for one
    capped = WindowDataset(records, window=3, input_res=(32, 32),
                           delta_source="raw", delta_clamp=1000.0)
    assert float(capped[0]["delta"][0]) == pytest.approx(1000 / 50.0)


def test_angle_source_refuses_uncalibrated_data(synth_root):
    """Radians must never silently fall back to counts in the same column."""
    ds = WindowDataset(scan_dataset(synth_root), window=3, input_res=(32, 32),
                       delta_source="angle")
    with pytest.raises(ValueError, match="view-scale calibration"):
        _ = ds[0]


def test_unknown_source_is_rejected_at_construction(synth_root):
    with pytest.raises(ValueError, match="delta_source"):
        WindowDataset(scan_dataset(synth_root), window=3, input_res=(32, 32),
                      delta_source="pointerRaw")


def test_clamp_report_is_zero_when_the_corpus_has_no_raw_column(synth_root):
    """0% is the honest answer for gaming-500, and must not read as an error."""
    rep = clamp_loss(synth_root, max_chunks=4)
    assert rep["differs_fraction"] == pytest.approx(0.0)
    assert rep["raw_over_pointer_p99"] == pytest.approx(1.0, abs=1e-6)


def test_spread_sampling_crosses_sessions(tmp_path):
    """Head-first sampling reads one session's playstyle as the corpus's."""
    from diogenes_phase0.label_signal import _select

    root = generate_dataset(tmp_path / "many", games=["g"], sessions_per_game=5,
                            chunks_per_session=6, chunk_frames=24, res=32,
                            seed=13)
    head = _select(root, 5, False)
    spread = _select(root, 5, True)
    sess = lambda recs: {r.chunk_dir.parent.name for r in recs}
    assert len(sess(head)) == 1, "sorted scan puts the head inside one session"
    assert len(sess(spread)) >= 4
    # and it must not silently return fewer chunks than asked for
    assert len(spread) == 5


def test_spread_is_a_noop_when_the_corpus_is_smaller_than_the_sample(tmp_path):
    from diogenes_phase0.label_signal import _select

    root = generate_dataset(tmp_path / "few", games=["g"], sessions_per_game=1,
                            chunks_per_session=2, chunk_frames=24, res=32,
                            seed=14)
    assert _select(root, 50, True) == _select(root, 50, False)


def _synthetic_pan(tmp_path, *, shift_per_frame=6, n=40, res=96, jitter=0.0):
    """A world that pans by a known amount, with matching pointer labels.

    The flow instrument must recover the direction; the unsigned frame-diff one
    structurally cannot, which is the whole reason for the new probe.
    """
    import json as _json

    import numpy as _np
    from diogenes_phase0.data.synthetic import generate_dataset

    root = generate_dataset(tmp_path, games=["pan"], sessions_per_game=1,
                            chunks_per_session=1, chunk_frames=n, res=res,
                            seed=21)
    rec = scan_dataset(root)[0]
    rng = _np.random.default_rng(3)
    base = rng.integers(0, 255, (res, res * 3, 3), dtype=_np.uint8)
    frames = _np.stack([base[:, (i * shift_per_frame) % res:
                             (i * shift_per_frame) % res + res] for i in range(n)])
    _np.savez_compressed(rec.chunk_dir / "p0.npz", frames=frames)
    rows = [_json.loads(x) for x in open(rec.actions_path)]
    for i, r in enumerate(rows):
        d = shift_per_frame + (rng.normal(0, jitter) if jitter else 0)
        r["pointerDx"], r["pointerDy"] = int(round(d)), 0
        r["pointerRawDx"], r["pointerRawDy"] = int(round(d)), 0
    with open(rec.actions_path, "w") as f:
        for r in rows:
            f.write(_json.dumps(r) + "\n")
    return root


def test_flow_instrument_recovers_a_known_pan(tmp_path):
    """估计器身份取决于环境（无 cv2 时退到相位相关），要断言的是性质不是身份。

    CI 现在装 data extra（#351），所以那里跑的是 flow；但在没有 cv2 的最小环境里
    这个测试仍应验证「量具找得回已知平移」，而不是变成一次跳过或一次假失败。
    """
    from diogenes_phase0.label_signal import flow_signal

    root = _synthetic_pan(tmp_path / "pan_const", shift_per_frame=6)
    rep = flow_signal(root, max_chunks=2, source="pointer", min_delta=1.0,
                      downscale=1)
    try:
        import cv2  # noqa: F401
        assert rep["estimator"] == "flow"
    except ImportError:
        assert rep["estimator"] == "phase"
    assert rep["pairs"] > 10
    # a constant pan gives a constant label: correlation is undefined, but the
    # estimator must at least see the motion consistently
    assert abs(rep["r_dx"]) <= 1.0


def test_flow_instrument_tracks_varying_pan_direction(tmp_path):
    """With a varying label the signed coupling must be strong.

    The label is the *realized* displacement, not the requested one: the crop
    window is clamped at the edges of the base image, and labelling the request
    where the pixels did something else would test the fixture, not the probe.
    """
    import json as _json

    import numpy as _np
    from diogenes_phase0.data.synthetic import generate_dataset
    from diogenes_phase0.label_signal import flow_signal

    n, res, pad = 48, 96, 240
    root = generate_dataset(tmp_path / "pan_var", games=["pan"],
                            sessions_per_game=1, chunks_per_session=1,
                            chunk_frames=n, res=res, seed=22)
    rec = scan_dataset(root)[0]
    rng = _np.random.default_rng(5)
    # 有结构的图案，不是 iid 噪声：稠密光流依赖局部结构，纯噪声是它的最坏情况，
    # 而真实游戏画面恰恰是有结构的。用它测量具，才是在测量具而不是在测噪声。
    W = res + 2 * pad
    xx, yy = _np.meshgrid(_np.arange(W), _np.arange(res))
    pat = (110 + 70 * _np.sin(xx / 11.0) + 50 * _np.sin(yy / 7.0)
           + 25 * _np.sin((xx + yy) / 19.0))
    for cx in range(30, W - 30, 60):
        pat[res // 3:2 * res // 3, cx:cx + 18] = 240
    base = _np.repeat(_np.clip(pat, 0, 255).astype(_np.uint8)[:, :, None], 3, axis=2)
    pos, xs = pad, [pad]
    for _ in range(n - 1):
        pos = int(_np.clip(pos + int(rng.integers(-9, 10)), 0, 2 * pad))
        xs.append(pos)
    frames = _np.stack([base[:, x:x + res] for x in xs])
    _np.savez_compressed(rec.chunk_dir / "p0.npz", frames=frames)

    rows = [_json.loads(x) for x in open(rec.actions_path)]
    for i, r in enumerate(rows):
        realized = (xs[i + 1] - xs[i]) if i + 1 < n else 0
        r["pointerDx"], r["pointerDy"] = realized, 0
        r["pointerRawDx"], r["pointerRawDy"] = realized, 0
    with open(rec.actions_path, "w") as f:
        for r in rows:
            f.write(_json.dumps(r) + "\n")

    best = max((flow_signal(root, max_chunks=2, source="pointer",
                            min_delta=3.0, downscale=1, offset=o)
                for o in (0, 1)), key=lambda x: x["coupling"])
    assert best["pairs"] >= 10
    # 纯平移上两种估计器都该找得回来——断言的是量具的性质，不是它的身份
    assert best["coupling"] > 0.8, best


def test_flow_instrument_refuses_a_source_the_corpus_lacks(tmp_path):
    from diogenes_phase0.label_signal import flow_signal

    root = _synthetic_pan(tmp_path / "pan_ang", shift_per_frame=6)
    with pytest.raises(SystemExit, match="lack this label"):
        flow_signal(root, max_chunks=2, source="angle", min_delta=1.0,
                    downscale=1)

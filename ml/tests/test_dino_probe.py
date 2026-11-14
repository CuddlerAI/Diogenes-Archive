"""The trunk probe's readings must be right on inputs whose answer is known.

The probe exists to decide something expensive — whether a frozen trunk is the
wrong assumption for game frames, and whether Track A needs a DINO finetune
before any LAM training. A metric that reads high on noise, or that credits a
constant feature with perfect separation, would answer that question wrong
while looking like evidence. So each metric is pinned against a constructed
case: separable, random, constant, identical, shifted.

No weights are downloaded here — the metrics are pure functions over feature
arrays, and the model path is exercised by real runs, not by CI.
"""
import numpy as np
import pytest

from diogenes_phase0.dino_probe import (nn_consistency, outlier_norm_fraction,
                                        pixel_patches, region_masks,
                                        separability_auc)


def test_region_masks_do_not_overlap():
    hud, world = region_masks(16)
    assert hud.sum() == 3 * 16
    assert world.sum() == 6 * 6
    assert not (hud & world).any()


def test_auc_is_one_when_groups_are_separable():
    rng = np.random.default_rng(0)
    a = rng.normal(5.0, 0.1, size=(40, 8))
    b = rng.normal(-5.0, 0.1, size=(40, 8))
    m1 = np.zeros(80, dtype=bool); m1[:40] = True
    m2 = ~m1
    feats = np.concatenate([a, b])
    assert separability_auc(feats, m1, m2) == pytest.approx(1.0)


def test_auc_is_chance_on_noise():
    rng = np.random.default_rng(1)
    feats = rng.normal(size=(400, 16))
    m1 = np.zeros(400, dtype=bool); m1[:200] = True
    # centroid direction is fit on the same data, so chance sits above 0.5;
    # the point is that it stays far from the separable case
    assert separability_auc(feats, m1, ~m1) < 0.85


def test_auc_of_a_constant_feature_is_chance_not_perfect():
    """A dead feature must not read as perfect separation."""
    feats = np.ones((40, 4))
    m1 = np.zeros(40, dtype=bool); m1[:20] = True
    assert separability_auc(feats, m1, ~m1) == pytest.approx(0.5)


def test_nn_consistency_is_one_for_identical_frames():
    rng = np.random.default_rng(2)
    f = rng.normal(size=(64, 12))
    assert nn_consistency(f, f, 8, radius=0) == pytest.approx(1.0)


def test_nn_consistency_drops_when_content_moves_far():
    rng = np.random.default_rng(3)
    f = rng.normal(size=(64, 12))
    rolled = f.reshape(8, 8, 12)[:, ::-1].reshape(64, 12)  # mirror the grid
    assert nn_consistency(f, rolled, 8, radius=0) < 0.5


def test_outlier_norm_catches_a_single_huge_token():
    f = np.ones((10, 4))
    f[3] *= 100.0
    assert outlier_norm_fraction(f) == pytest.approx(0.1)
    assert outlier_norm_fraction(np.ones((10, 4))) == 0.0


def test_pixel_patches_tile_the_frame_without_mixing_patches():
    frame = np.zeros((28, 28, 3), dtype=np.uint8)
    frame[:14, :14] = 255                       # top-left patch only
    p = pixel_patches(frame, grid=2, patch=14)
    assert p.shape == (4, 14 * 14 * 3)
    assert p[0].min() == pytest.approx(1.0)     # all-white patch
    assert p[1].max() == pytest.approx(0.0)
    assert p[2].max() == pytest.approx(0.0)

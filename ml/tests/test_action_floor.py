"""The no-pixel floor has to be a floor, and it has to be per-game.

A head that beats the floor is using the frames; one that does not has
learned base rates. Both claims collapse if the floor is computed wrong, so
the entropy is checked against hand-worked values and against the property
that makes it a floor at all: no constant predictor can score below it.
"""
import math

import pytest

from diogenes_phase0.action_floor import action_floor, binary_entropy
from diogenes_phase0.data.synthetic import generate_dataset


def test_binary_entropy_endpoints_and_peak():
    assert binary_entropy(0.0) == 0.0
    assert binary_entropy(1.0) == 0.0
    assert binary_entropy(0.5) == pytest.approx(math.log(2))
    # symmetric around 0.5
    assert binary_entropy(0.1) == pytest.approx(binary_entropy(0.9))


def test_rarer_keys_are_cheaper_to_predict():
    """Concentration lowers the floor — why valorant's is under minecraft's."""
    assert binary_entropy(0.05) < binary_entropy(0.41)
    assert binary_entropy(0.566) < binary_entropy(0.5) + 1e-9


def test_no_constant_predictor_beats_the_floor():
    """The defining property: the floor is the best a fixed guess can do."""
    p = 0.3
    floor = binary_entropy(p)
    for q in (0.05, 0.1, 0.2, 0.3, 0.4, 0.6, 0.9):
        # expected BCE of predicting a constant q against a Bernoulli(p)
        bce = -(p * math.log(q) + (1 - p) * math.log(1 - q))
        assert bce >= floor - 1e-9, (q, bce, floor)
    # and it is attained exactly at q == p
    assert -(p * math.log(p) + (1 - p) * math.log(1 - p)) == pytest.approx(floor)


def test_floor_computed_over_a_real_dataset_root(tmp_path):
    root = generate_dataset(tmp_path / "d", games=["synth_f"],
                            sessions_per_game=1, chunks_per_session=2,
                            chunk_frames=40, res=32, seed=3)
    res = action_floor(root, min_key_frac=None)
    assert res["frames"] == 80
    assert 0.0 <= res["floor_bce"] < math.log(2)
    # every reported rate is a probability and its entropy matches
    for vk, p in res["rates"].items():
        assert 0.0 <= p <= 1.0
        assert res["per_key_bce"][vk] == pytest.approx(binary_entropy(p))

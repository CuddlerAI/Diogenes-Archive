"""The epoch-aligned step count has to come from the run's own arithmetic.

A scale experiment that pairs two corpora at "the same number of epochs" is
only a comparison if the step count really is that number. Every way of
getting it wrong here is silent: counting val windows alongside train ones,
ignoring the stride, or letting a rounding rule drift changes the ratio
without changing anything visible in the run, and the resulting table still
prints two macro F1 numbers side by side as if they answered the question.
"""
import pytest

from diogenes_phase0.data.dataset import (WindowDataset, scan_dataset,
                                          split_records)
from diogenes_phase0.data.synthetic import generate_dataset
from diogenes_phase0.epoch_align import aligned_steps, train_windows


@pytest.fixture(scope="module")
def synth_root(tmp_path_factory):
    root = tmp_path_factory.mktemp("epoch_align")
    return generate_dataset(root, games=["synth_a"], sessions_per_game=6,
                            chunks_per_session=2, chunk_frames=40, res=48,
                            seed=5)


def _fraction_with_a_real_split(records) -> float:
    """A val_fraction that puts at least one session on each side.

    The val split is a hash of the session name, so a hard-coded fraction
    could send every synthetic session the same way and make the
    train-vs-everything assertion below vacuously true.
    """
    for frac in (i / 10 for i in range(1, 10)):
        s = split_records(records, holdout_games=[], val_fraction=frac)
        if s["train"] and s["val"]:
            return frac
    raise AssertionError("no val_fraction splits these synthetic sessions")


def _cfg(frac: float, **over) -> dict:
    cfg = {"min_key_frac": 0.0, "val_fraction": frac, "window": 5,
           "stride": 1, "input_res": [48, 48], "delta_scale": 50.0}
    cfg.update(over)
    return cfg


def test_count_covers_the_train_split_and_nothing_else(synth_root):
    """Val windows in the count would size the run against frames it never sees."""
    records = scan_dataset(synth_root)
    frac = _fraction_with_a_real_split(records)
    train = split_records(records, holdout_games=[], val_fraction=frac)["train"]

    expected = len(WindowDataset(train, window=5, stride=1, input_res=(48, 48)))
    assert train_windows(synth_root, _cfg(frac)) == expected

    everything = len(WindowDataset(records, window=5, stride=1,
                                   input_res=(48, 48)))
    assert expected < everything


def test_count_honors_the_stride_the_run_uses(synth_root):
    """stride 2 halves the samples per chunk; a count at stride 1 doubles the ratio."""
    records = scan_dataset(synth_root)
    frac = _fraction_with_a_real_split(records)
    n1 = train_windows(synth_root, _cfg(frac, stride=1))
    n2 = train_windows(synth_root, _cfg(frac, stride=2))
    assert n2 * 2 == pytest.approx(n1, abs=len(records))


def test_valorant_pair_reproduces_the_step_count_that_ran():
    """The published round: 233,392 vs 1,529,866 train windows -> 78,750 steps.

    Pinned because the number is what makes that round's epoch column
    (2.47 vs 2.47) true; a refactor that shifts it invalidates the table
    rather than just changing a config.
    """
    assert aligned_steps(233_392, 1_529_866, 12_000, multiple_of=250) == 78_750
    # and the epochs really do line up, which is the point of the number
    assert 12_000 * 48 / 233_392 == pytest.approx(78_750 * 48 / 1_529_866,
                                                  rel=0.01)


def test_same_size_corpora_keep_the_base_budget():
    assert aligned_steps(1000, 1000, 12_000, multiple_of=250) == 12_000
    assert aligned_steps(1000, 1000, 12_000, multiple_of=1) == 12_000


def test_empty_base_corpus_is_an_error_not_a_division():
    with pytest.raises(ValueError):
        aligned_steps(0, 1_529_866, 12_000)

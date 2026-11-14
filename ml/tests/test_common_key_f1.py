"""macro F1 alone cannot tell a collapsed head from an unlearnable tail.

Both land in the same low range, and only one of them is fixed by
collecting more data, so the distinction decides whether the next round is
worth its GPU bill. Restricting the average to keys with real support
splits them: a collapsed head fails on common keys too and drags both
numbers down, while a head that learned everything the data can teach
keeps the restricted number high and loses only the unrestricted one.

Thresholds here are deliberately loose — the claim under test is the
ORDERING between the two readings, not any particular value.
"""
import torch

from diogenes_phase0.evaluate import run_eval
from diogenes_phase0.keys import N_BINS, N_KEYS

COMMON_A, COMMON_B, RARE = 2, 5, 7


def _batch(n: int) -> dict:
    """Two common keys on ~half the frames each, one rare key on 0.5%.

    0.5% sits below the 1% default rather than at it — a key with support
    exactly at the threshold is included (>=), and pinning the test to that
    boundary would make it a test of the comparison operator instead of the
    behaviour.
    """
    keys = torch.zeros(n, N_KEYS)
    keys[: n // 2, COMMON_A] = 1.0
    keys[n // 2:, COMMON_B] = 1.0
    keys[: max(1, n // 200), RARE] = 1.0
    return {
        "frames": torch.zeros(n, 5, 3, 8, 8),
        "keys": keys,
        "bin_x": torch.full((n,), N_BINS // 2, dtype=torch.long),
        "bin_y": torch.full((n,), N_BINS // 2, dtype=torch.long),
        "delta": torch.zeros(n, 2),
    }


class _Oracle:
    """Perfect on the two common keys, never fires the rare one.

    This is the 'learned what the data can teach' head: the rare key has a
    few dozen positive frames in the whole split, which is not enough to
    learn from at this data volume.
    """

    def __init__(self, truth: torch.Tensor):
        self.truth = truth

    def eval(self): return self

    def __call__(self, frames):
        b = frames.shape[0]
        k = torch.full((b, N_KEYS), -9.0)
        for i in (COMMON_A, COMMON_B):
            k[:, i] = torch.where(self.truth[:b, i] > 0, 9.0, -9.0)
        return {"keys": k,
                "bin_x": torch.zeros(b, N_BINS), "bin_y": torch.zeros(b, N_BINS),
                "delta": torch.zeros(b, 2)}


class _Collapsed:
    """Always fires one common key — the round-1 failure."""

    def eval(self): return self

    def __call__(self, frames):
        b = frames.shape[0]
        k = torch.full((b, N_KEYS), -9.0)
        k[:, COMMON_A] = 9.0
        return {"keys": k,
                "bin_x": torch.zeros(b, N_BINS), "bin_y": torch.zeros(b, N_BINS),
                "delta": torch.zeros(b, 2)}


def test_unlearnable_tail_shows_up_only_in_the_unrestricted_average():
    b = _batch(400)
    rep = run_eval(_Oracle(b["keys"]), [b], torch.device("cpu"))
    assert rep["key_n_seen"] == 3 and rep["key_n_common"] == 2, rep
    # the rare key contributes F1 0 to the unrestricted average
    assert rep["key_macro_f1"] < 0.7
    assert rep["key_macro_f1_common"] > 0.99
    assert rep["key_macro_f1_common"] > rep["key_macro_f1"]


def test_collapse_drags_both_averages_down():
    b = _batch(400)
    rep = run_eval(_Collapsed(), [b], torch.device("cpu"))
    assert rep["key_macro_f1"] < 0.4
    # the distinguishing assertion: unlike the tail case, the restricted
    # average is low too, because a common key is being missed
    assert rep["key_macro_f1_common"] < 0.6


def test_support_counts_make_the_tail_visible():
    b = _batch(400)
    rep = run_eval(_Oracle(b["keys"]), [b], torch.device("cpu"))
    sup = rep["key_support"]
    assert sup[str(_vk(COMMON_A))] == 200
    assert sup[str(_vk(RARE))] == 2


def _vk(i: int):
    from diogenes_phase0.keys import SCANNED_VKEYS
    return SCANNED_VKEYS[i]

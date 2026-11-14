"""Three key metrics, three different questions — never interchangeable.

Round 1 of IDM scored key_exact 0.336 while macro F1 was 0.089: the model
had learned W and nothing else. Per-key accuracy would have looked healthy
on that same model, because predicting "not pressed" is right most of the
time for a key that is rarely used. VPT's headline 90.6% is most likely the
lenient per-key reading, so quoting it against exact-set match compares two
different measurements.
"""
import torch

from diogenes_phase0.evaluate import run_eval
from diogenes_phase0.keys import N_BINS, N_KEYS


class _Collapsed:
    """A model that always predicts key index 2 and bin 0 — the failure mode."""

    def eval(self): return self
    def __call__(self, frames):
        b = frames.shape[0]
        k = torch.full((b, N_KEYS), -9.0)
        k[:, 2] = 9.0
        return {"keys": k,
                "bin_x": torch.zeros(b, N_BINS), "bin_y": torch.zeros(b, N_BINS),
                "delta": torch.zeros(b, 2)}


def _loader(n=64):
    """Half the frames hold key 2, the rest hold key 5 — a collapsed model
    gets every 'key 2' frame right and every 'key 5' frame wrong."""
    batches = []
    keys = torch.zeros(n, N_KEYS)
    keys[: n // 2, 2] = 1.0
    keys[n // 2:, 5] = 1.0
    batches.append({
        "frames": torch.zeros(n, 5, 3, 8, 8),
        "keys": keys,
        "bin_x": torch.full((n,), N_BINS // 2, dtype=torch.long),
        "bin_y": torch.full((n,), N_BINS // 2, dtype=torch.long),
        "delta": torch.zeros(n, 2),
    })
    return batches


def test_collapsed_model_looks_good_per_key_and_bad_on_f1():
    rep = run_eval(_Collapsed(), _loader(), torch.device("cpu"))
    # lenient: right on 10 of 12 keys for every frame
    assert rep["key_per_key_acc"] > 0.8
    # strict: only the half holding key 2 matches exactly
    assert abs(rep["key_exact"] - 0.5) < 1e-6
    # honest: one key learned out of two present
    assert rep["key_macro_f1"] < 0.6
    assert rep["key_per_key_acc"] > rep["key_exact"] > rep["key_macro_f1"]


def test_all_three_metrics_are_reported():
    rep = run_eval(_Collapsed(), _loader(), torch.device("cpu"))
    for k in ("key_exact", "key_per_key_acc", "key_macro_f1"):
        assert rep[k] is not None, k

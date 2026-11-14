"""The probe must separate prefill from decode and not count warmup.

Round 8's 11.3 s/frame was reported as a single number, which cannot
distinguish "the model is too big" from "the adapter was never merged" or
"the schema asks for too many tokens". Each has a different fix and only
one of them costs money.
"""
import torch

from diogenes_vl.latency_probe import time_one


class _Proc:
    def apply_chat_template(self, messages, tokenize=False,
                            add_generation_prompt=True):
        return "prompt"

    def __call__(self, text=None, images=None, return_tensors=None):
        return _Batch()


class _Batch(dict):
    def __init__(self):
        super().__init__(input_ids=torch.zeros(1, 40, dtype=torch.long))

    def to(self, device):
        return self


class _Model:
    """Emits a fixed number of new tokens after a measurable pause."""
    def __init__(self, n_new=25):
        self.n_new = n_new

    def generate(self, input_ids=None, max_new_tokens=None, do_sample=None):
        import time
        time.sleep(0.01)
        return torch.zeros(1, input_ids.shape[1] + self.n_new, dtype=torch.long)


def test_reports_new_tokens_and_a_rate():
    s = time_one(_Proc(), _Model(n_new=25), object(), torch.device("cpu"), 192)
    assert s["new_tokens"] == 25
    assert s["prompt_tokens"] == 40
    assert s["tok_per_s"] > 0
    # a rate must be tokens over DECODE time, not over the whole call
    assert abs(s["tok_per_s"] - 25 / s["decode_s"]) < 1e-6


def test_total_is_prefill_plus_decode():
    s = time_one(_Proc(), _Model(), object(), torch.device("cpu"), 192)
    assert abs(s["total_s"] - (s["prefill_s"] + s["decode_s"])) < 1e-3


def test_zero_new_tokens_does_not_divide_by_zero():
    s = time_one(_Proc(), _Model(n_new=0), object(), torch.device("cpu"), 192)
    assert s["new_tokens"] == 0
    # decode took measurable time (the stub sleeps), so the rate is exactly 0 —
    # `== 0 or is not None` would have accepted any finite value.
    assert s["tok_per_s"] == 0

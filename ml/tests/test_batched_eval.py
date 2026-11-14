"""Batching must not change a single prediction.

A batching bug does not crash — it silently returns different text for the
same frame, and every metric downstream inherits the corruption. Decoder-only
generation is the specific trap: pad on the right and each shorter prompt
resumes after pad tokens, producing output that differs from the same frame
decoded alone.
"""
import json

import torch

from diogenes_vl.evaluate import generate_observation, generate_observations

OBS = {"game": "Dota 2", "scene": "mid lane", "event": "tower falls",
       "event_class": "combat_kill", "subjects": ["tower"], "ocr_text": [],
       "highlight_score": 0.5}


class _Tok:
    padding_side = "right"


class _Proc:
    """Records the padding side in force at the moment of encoding."""
    def __init__(self):
        self.tokenizer = _Tok()
        self.seen_padding_side = None
        self.batch_sizes = []

    def apply_chat_template(self, messages, tokenize=False,
                            add_generation_prompt=True):
        return "prompt"

    def __call__(self, text=None, images=None, return_tensors=None, padding=None):
        self.seen_padding_side = self.tokenizer.padding_side
        self.batch_sizes.append(len(text))
        return _Batch(len(text))

    def decode(self, row, skip_special_tokens=True):
        return json.dumps(OBS)


class _Batch(dict):
    def __init__(self, bs):
        super().__init__(input_ids=torch.zeros(bs, 7, dtype=torch.long))

    def to(self, device):
        return self


class _Model:
    def generate(self, input_ids=None, max_new_tokens=None, do_sample=None):
        bs = input_ids.shape[0]
        return torch.zeros(bs, input_ids.shape[1] + 5, dtype=torch.long)


def test_batch_encodes_with_left_padding():
    p = _Proc()
    generate_observations(p, _Model(), [object(), object()], torch.device("cpu"))
    assert p.seen_padding_side == "left"


def test_padding_side_is_restored_afterwards():
    """Leaving it flipped would corrupt training code that runs later."""
    p = _Proc()
    p.tokenizer.padding_side = "right"
    generate_observations(p, _Model(), [object()], torch.device("cpu"))
    assert p.tokenizer.padding_side == "right"


def test_returns_one_result_per_input_frame():
    p = _Proc()
    outs = generate_observations(p, _Model(), [object()] * 4, torch.device("cpu"))
    assert len(outs) == 4
    assert all(o[0]["game"] == "Dota 2" for o in outs)


def test_single_frame_helper_goes_through_the_batch_path():
    """One implementation, so the two cannot drift apart."""
    p = _Proc()
    obs, raw = generate_observation(p, _Model(), object(), torch.device("cpu"))
    assert obs["game"] == "Dota 2"
    assert p.batch_sizes == [1]


def test_unparseable_output_becomes_none_not_an_exception():
    class _BadProc(_Proc):
        def decode(self, row, skip_special_tokens=True):
            return "not json at all"
    p = _BadProc()
    outs = generate_observations(p, _Model(), [object(), object()],
                                 torch.device("cpu"))
    assert [o[0] for o in outs] == [None, None]
    assert all(o[1] == "not json at all" for o in outs)

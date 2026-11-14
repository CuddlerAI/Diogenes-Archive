"""json_valid rewards a shell that says nothing.

validate_observation only requires a non-empty `game`, so an output with
empty scene/event/subjects passes. A model emitting that same shell for
every frame scores a perfect json_valid_rate while describing nothing —
doc 25 §4.3's "格式门容易被模板化空壳满足".
"""
from diogenes_vl.evaluate import check_vl_gates
from diogenes_vl.schema import (is_substantive, observation_fingerprint,
                                validate_observation)

SHELL = {"game": "Dota 2", "scene": "", "event": "", "event_class": "other",
         "subjects": [], "ocr_text": [], "highlight_score": 0.0}
REAL = {"game": "Dota 2", "scene": "mid lane, tower under attack",
        "event": "hero takes tower damage", "event_class": "combat_kill",
        "subjects": ["hero", "tower"], "ocr_text": ["12:04"],
        "highlight_score": 0.7}


def test_the_empty_shell_still_passes_schema_validation():
    """Establishes the gap this check exists to close."""
    assert validate_observation(SHELL)["game"] == "Dota 2"


def test_substantive_separates_shell_from_real_answer():
    assert is_substantive(REAL)
    assert not is_substantive(SHELL)


def test_whitespace_is_not_content():
    padded = {**SHELL, "scene": "   ", "event": "\t"}
    assert not is_substantive(padded)


def test_fingerprint_collapses_a_constant_answering_model():
    """Same answer every frame -> one fingerprint, however many frames."""
    fps = {observation_fingerprint(SHELL) for _ in range(50)}
    assert len(fps) == 1
    assert observation_fingerprint(REAL) != observation_fingerprint(SHELL)


def test_substantive_rate_can_be_gated():
    rep = {"content_substantive_rate": 0.1}
    gates = check_vl_gates(rep, {"content_substantive_min": 0.8}, "val")
    g = [x for x in gates if x.name == "val.content_substantive_rate"][0]
    assert g.passed is False and g.level == "hard"


def test_substantive_gate_is_absent_when_unconfigured():
    """Existing configs must not start failing a gate they never set."""
    rep = {"content_substantive_rate": 0.1}
    gates = check_vl_gates(rep, {}, "val")
    g = [x for x in gates if x.name == "val.content_substantive_rate"][0]
    assert g.passed is None

import pytest

from diogenes_vl.schema import (
    SchemaError, normalize_game, parse_observation, validate_observation,
)

GOOD = {"game": "Valorant", "scene": "a ranked round", "event": "player fires",
        "event_class": "combat_kill", "subjects": ["player"],
        "ocr_text": ["25 HP"], "highlight_score": 0.9}


def test_valid_observation_passes():
    obs = validate_observation(GOOD)
    assert obs["highlight_score"] == 0.9


@pytest.mark.parametrize("mutation", [
    {"event_class": "explosion"},          # not in vocabulary
    {"highlight_score": 1.5},              # out of range
    {"subjects": "player"},                # not a list
    {"game": ""},                          # empty
])
def test_invalid_observation_rejected(mutation):
    with pytest.raises(SchemaError):
        validate_observation({**GOOD, **mutation})


def test_missing_field_rejected():
    bad = dict(GOOD)
    del bad["ocr_text"]
    with pytest.raises(SchemaError):
        validate_observation(bad)


def test_parse_tolerates_fences_and_prose():
    import json
    text = "Sure! Here is the JSON:\n```json\n" + json.dumps(GOOD) + "\n```"
    assert parse_observation(text)["game"] == "Valorant"
    with pytest.raises(SchemaError):
        parse_observation("no json here")


def test_normalize_game():
    assert normalize_game("Grand Theft Auto V") == normalize_game("grand-theft-auto-v")
    assert normalize_game("CS2") != normalize_game("CS:GO")

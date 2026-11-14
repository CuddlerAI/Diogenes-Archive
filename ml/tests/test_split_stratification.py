"""Every multi-session game must appear in BOTH splits.

A global hash split can put all of a game's sessions on one side; the model
is then scored on games it never trained on, which measures zero-shot
transfer rather than learning. Round 4 lost its signal to exactly this:
87% of val frames were games absent from the training split.
"""
from diogenes_vl.bench import assign_splits_per_game


def test_multi_session_games_land_in_both_splits():
    sessions = {f"game{i}": [f"g{i}_s{j}" for j in range(6)] for i in range(10)}
    m = assign_splits_per_game(sessions, val_fraction=0.34)
    for game, sess in sessions.items():
        splits = {m[s] for s in sess}
        assert splits == {"val", "test"}, f"{game} landed only in {splits}"


def test_two_session_game_splits_one_each():
    m = assign_splits_per_game({"g": ["a", "b"]}, val_fraction=0.34)
    assert sorted(m.values()) == ["test", "val"]


def test_single_session_game_goes_to_train():
    # it cannot be evaluated in-domain; in val it would only add zero-shot noise
    m = assign_splits_per_game({"g": ["only"]}, val_fraction=0.5)
    assert m == {"only": "test"}


def test_assignment_is_deterministic():
    s = {"g": [f"s{i}" for i in range(5)]}
    assert assign_splits_per_game(s, 0.34) == assign_splits_per_game(s, 0.34)


def test_val_fraction_is_respected_approximately():
    m = assign_splits_per_game({"g": [f"s{i}" for i in range(10)]}, 0.3)
    assert sum(1 for v in m.values() if v == "val") == 3


def test_high_val_fraction_still_leaves_training_data():
    m = assign_splits_per_game({"g": ["a", "b", "c"]}, val_fraction=0.99)
    assert sorted(m.values()).count("test") >= 1

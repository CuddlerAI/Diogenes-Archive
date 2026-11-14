"""A holdout game must be absent from every split we iterate against.

The VL gates are computed on val, and val is also what we watch while
tuning, so the gate measures how well we fitted that split. doc 25 §4.1
calls for the IDM line's `holdout_games` structure: a set of games read
once, at final judgment.
"""
from diogenes_vl.bench import assign_splits_per_game


def _sessions(n_games=3, n_sessions=4):
    return {f"game{g}": [f"g{g}_s{i}" for i in range(n_sessions)]
            for g in range(n_games)}


def test_holdout_game_never_lands_in_val_or_train():
    by_game = _sessions()
    out = assign_splits_per_game(by_game, 0.5, holdout_games=["game1"])
    held = {s for s in by_game["game1"]}
    assert all(out[s] == "holdout" for s in held), out
    # and no other game leaked into holdout
    others = [s for g, ss in by_game.items() if g != "game1" for s in ss]
    assert all(out[s] in ("val", "test") for s in others)


def test_holdout_matching_ignores_case_and_padding():
    by_game = {"Counter-Strike 2": ["a", "b"], "Dota 2": ["c", "d"]}
    out = assign_splits_per_game(by_game, 0.5, holdout_games=["  counter-strike 2 "])
    assert out["a"] == out["b"] == "holdout"
    assert set((out["c"], out["d"])) <= {"val", "test"}


def test_no_holdout_keeps_previous_behaviour():
    by_game = _sessions()
    before = assign_splits_per_game(by_game, 0.5)
    after = assign_splits_per_game(by_game, 0.5, holdout_games=[])
    assert before == after
    assert "holdout" not in set(after.values())


def test_holding_out_every_game_leaves_nothing_to_train_on():
    """Loud emptiness beats a silent zero-row training set."""
    by_game = _sessions(n_games=2)
    out = assign_splits_per_game(by_game, 0.5, holdout_games=["game0", "game1"])
    assert set(out.values()) == {"holdout"}

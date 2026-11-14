import json

from diogenes_vl.analyze_labels import analyze


def _row(gt, pred, event="movement"):
    return {"game": gt, "observation": {"game": pred, "event_class": event,
                                        "scene": "", "event": "", "subjects": [],
                                        "ocr_text": [], "highlight_score": 0.1}}


def test_scores_and_failure_detection(tmp_path):
    p = tmp_path / "labels.jsonl"
    rows = [
        _row("Minecraft", "Minecraft"),                       # exact
        _row("Forza Horizon", "Forza Horizon 5"),             # containment
        _row("Counter-Strike 2", "Counter-Strike: GO"),       # fuzzy only
        _row("2XKO", "Genshin Impact", "menu_ui"),            # teacher failure
        _row("2XKO", "Valorant", "menu_ui"),                  # teacher failure
        _row("Hitman 3", "unknown", "loading"),               # unknown
    ]
    p.write_text("\n".join(json.dumps(r) for r in rows))

    rep = analyze(p)
    assert rep["n_labels"] == 6 and rep["n_games"] == 5
    assert rep["teacher_game_exact"] == 1 / 6
    assert rep["teacher_game_containment"] == 2 / 6
    assert rep["teacher_game_fuzzy"] == 3 / 6
    assert rep["unknown_rate"] == 1 / 6
    # a game the teacher never gets right shows up in the failure list
    assert "2XKO" in rep["games_teacher_fails"]
    assert "Minecraft" not in rep["games_teacher_fails"]
    assert rep["event_class_distribution"]["menu_ui"] == round(2 / 6, 3)
    # 5 committed answers (one "unknown"), 3 of them fuzzy-correct
    assert rep["accuracy_when_committed"] == 3 / 5


def test_gameplay_frame_accuracy_ignores_menu_and_loading(tmp_path):
    p = tmp_path / "labels.jsonl"
    rows = [
        _row("Minecraft", "Minecraft", "movement"),        # gameplay, correct
        _row("Cyberpunk 2077", "Cyberpunk 2077", "combat_kill"),  # gameplay, correct
        _row("Assassins Creed", "unknown", "menu_ui"),     # browser frame
        _row("Elden Ring", "unknown", "loading"),          # black screen
    ]
    p.write_text("\n".join(json.dumps(r) for r in rows))

    rep = analyze(p)
    assert rep["gameplay_frame_rate"] == 0.5
    # non-gameplay frames drag the headline number down but not this one
    assert rep["accuracy_on_gameplay_frames"] == 1.0
    assert rep["teacher_game_fuzzy"] == 0.5

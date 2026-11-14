"""Paired comparison must separate a real change from noise.

A headline +2.1 point move (SFT 0.802 -> GRPO 0.823 on event_class) came
from 17 frames improving and 10 worsening out of 300 — p=0.248. Reporting
that as a gain would have been wrong.
"""
import pytest

from diogenes_vl.compare_runs import compare, mcnemar_exact


def _p(pid, game_pred, event_pred, gt_game="Minecraft", ref_event="movement",
       parsed=True):
    return {
        "id": pid, "gt_game": gt_game, "ref_event_class": ref_event,
        "pred": None if not parsed else {
            "game": game_pred, "event_class": event_pred, "scene": "",
            "event": "", "subjects": [], "ocr_text": [], "highlight_score": 0.1},
    }


def test_mcnemar_symmetric_split_is_not_significant():
    assert mcnemar_exact(10, 10) == 1.0
    assert mcnemar_exact(10, 17) > 0.05      # the round-7 case
    assert mcnemar_exact(0, 0) == 1.0


def test_mcnemar_lopsided_split_is_significant():
    assert mcnemar_exact(0, 12) < 0.05
    assert mcnemar_exact(2, 20) < 0.05


def test_real_improvement_is_flagged_significant():
    a = [_p(f"i{i}", "Wrong", "idle") for i in range(30)]
    b = [_p(f"i{i}", "Minecraft", "movement") for i in range(30)]
    rep = compare(a, b)
    m = rep["metrics"]["event_class"]
    assert m["b_improved"] == 30 and m["b_worsened"] == 0
    assert m["significant_at_05"] is True


def test_wash_is_reported_as_noise():
    # 5 frames get better, 5 get worse — same headline rate, no real change
    a = [_p(f"g{i}", "Minecraft", "movement") for i in range(5)] + \
        [_p(f"b{i}", "Wrong", "idle") for i in range(5)]
    b = [_p(f"g{i}", "Wrong", "idle") for i in range(5)] + \
        [_p(f"b{i}", "Minecraft", "movement") for i in range(5)]
    m = compare(a, b)["metrics"]["event_class"]
    assert m["b_improved"] == 5 and m["b_worsened"] == 5
    assert m["net"] == 0 and m["significant_at_05"] is False
    assert m["rate_a"] == m["rate_b"]        # identical headline rate


def test_only_shared_frames_are_scored():
    a = [_p("x", "Minecraft", "movement"), _p("only_a", "Minecraft", "movement")]
    b = [_p("x", "Minecraft", "movement"), _p("only_b", "Wrong", "idle")]
    rep = compare(a, b)
    assert rep["n_shared"] == 1 and rep["n_a"] == 2 and rep["n_b"] == 2
    assert rep["metrics"]["event_class"]["n"] == 1


def test_game_metric_restricted_to_gameplay_frames():
    rows_a = [_p("play", "Minecraft", "movement", ref_event="movement"),
              _p("menu", "Minecraft", "menu_ui", ref_event="menu_ui")]
    rows_b = [_p("play", "Wrong", "movement", ref_event="movement"),
              _p("menu", "Wrong", "menu_ui", ref_event="menu_ui")]
    rep = compare(rows_a, rows_b)
    # only the gameplay frame counts toward the game metric
    assert rep["metrics"]["game_gameplay"]["n"] == 1
    assert rep["metrics"]["game_gameplay"]["b_worsened"] == 1
    # both frames count toward event_class
    assert rep["metrics"]["event_class"]["n"] == 2


def test_unparsed_predictions_count_as_failures():
    a = [_p("x", "Minecraft", "movement")]
    b = [_p("x", None, None, parsed=False)]
    rep = compare(a, b)
    assert rep["metrics"]["json_parsed"]["b_worsened"] == 1
    assert rep["metrics"]["event_class"]["b_worsened"] == 1

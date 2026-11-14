"""game_top1 must be reported restricted to gameplay frames.

Session GT labels every frame of a session with the game title, including
launcher, desktop and browser frames. Scoring over all frames rewards a
model that guesses a game name on a browser tab and penalises one that
correctly abstains — round 4 measured a hybrid-label student as WORSE
(0.175 vs 0.205) for exactly that reason.
"""
from diogenes_vl.evaluate import check_vl_gates

BASE = {"n": 100, "json_valid_rate": 1.0, "event_class_acc": 0.85,
        "game_teacher_agree": 0.7}


def test_gameplay_metric_is_gated_when_configured():
    rep = {**BASE, "game_top1": 0.18, "game_top1_gameplay": 0.62}
    gates = check_vl_gates(rep, {"json_valid_min": 0.95,
                                 "game_top1_gameplay_min": 0.5,
                                 "event_acc_min": 0.8})
    g = {x.name: x for x in gates}
    assert g["val.game_top1_gameplay"].passed is True
    # the all-frame number is reported but not blocking
    assert g["val.game_top1"].passed is None


def test_all_frame_metric_can_still_be_gated_explicitly():
    rep = {**BASE, "game_top1": 0.18, "game_top1_gameplay": 0.62}
    gates = check_vl_gates(rep, {"game_top1_min": 0.5})
    g = {x.name: x for x in gates}
    assert g["val.game_top1"].passed is False


def test_missing_gameplay_frames_reports_na_not_zero():
    rep = {**BASE, "game_top1": 0.2, "game_top1_gameplay": None}
    gates = check_vl_gates(rep, {"game_top1_gameplay_min": 0.5})
    g = {x.name: x for x in gates}
    assert g["val.game_top1_gameplay"].passed is None  # vacuous, not a failure


def test_gate_names_carry_the_split_they_were_measured_on():
    """A holdout score labelled "val." is indistinguishable from the number
    we tuned against — which defeats having a holdout at all."""
    rep = {"json_valid_rate": 0.99, "game_top1": 0.8}
    val = check_vl_gates(rep, {"game_top1_min": 0.5}, "val")
    hold = check_vl_gates(rep, {"game_top1_min": 0.5}, "holdout")
    assert any(g.name == "val.game_top1" for g in val)
    assert any(g.name == "holdout.game_top1" for g in hold)
    assert not any(g.name.startswith("val.") for g in hold)

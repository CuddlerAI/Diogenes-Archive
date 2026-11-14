import json

from diogenes_vl.reward import compute_reward, group_advantages

REF_PLAY = {"game": "Elden Ring", "event_class": "combat_kill",
            "highlight_score": 0.8, "scene": "", "event": "",
            "subjects": [], "ocr_text": []}
REF_MENU = {"game": "unknown", "event_class": "menu_ui",
            "highlight_score": 0.1, "scene": "", "event": "",
            "subjects": [], "ocr_text": []}


def _resp(**over):
    obs = {"game": "Elden Ring", "scene": "s", "event": "e",
           "event_class": "combat_kill", "subjects": [], "ocr_text": [],
           "highlight_score": 0.8}
    obs.update(over)
    return json.dumps(obs)


def test_perfect_response_scores_near_one():
    r = compute_reward(_resp(), REF_PLAY)
    assert r.parsed and r.total > 0.99
    assert r.game == 1.0 and r.event == 1.0 and r.json_valid == 1.0


def test_malformed_response_scores_zero():
    r = compute_reward("not json at all", REF_PLAY)
    assert r.total == 0.0 and not r.parsed
    assert r.json_valid == 0.0 and r.game == 0.0


def test_wrong_game_loses_game_component():
    r = compute_reward(_resp(game="Dark Souls"), REF_PLAY)
    assert r.game == 0.0 and r.event == 1.0
    assert 0 < r.total < 1


def test_fuzzy_game_match_credited():
    r = compute_reward(_resp(game="Elden Ring: Shadow of the Erdtree"), REF_PLAY)
    assert r.game == 1.0


def test_gameplay_frame_abstention_is_penalised():
    # on a real gameplay frame, answering "unknown" is wrong
    r = compute_reward(_resp(game="unknown"), REF_PLAY)
    assert r.game == 0.0


def test_menu_frame_rewards_matching_abstention():
    # off gameplay, the reference is "unknown"; matching it should score
    resp = _resp(game="unknown", event_class="menu_ui")
    r = compute_reward(resp, REF_MENU)
    assert r.game == 1.0


def test_menu_frame_penalises_guessing_a_game():
    resp = _resp(game="Call of Duty", event_class="menu_ui")
    r = compute_reward(resp, REF_MENU)
    assert r.game == 0.0


def test_highlight_calibration_is_graded():
    r_close = compute_reward(_resp(highlight_score=0.7), REF_PLAY)  # |0.7-0.8|=0.1
    r_far = compute_reward(_resp(highlight_score=0.0), REF_PLAY)    # |0.0-0.8|=0.8
    assert abs(r_close.highlight - 0.9) < 1e-9
    assert abs(r_far.highlight - 0.2) < 1e-9
    assert r_close.total > r_far.total


def test_group_advantages_zero_mean():
    adv = group_advantages([0.2, 0.5, 0.8, 0.5])
    assert abs(sum(adv)) < 1e-6
    # higher reward -> higher advantage
    assert adv[2] > adv[0]


def test_group_advantages_flat_group_is_zero():
    adv = group_advantages([0.5, 0.5, 0.5])
    assert all(abs(a) < 1e-3 for a in adv)  # nothing to learn from this prompt


def test_group_advantages_empty():
    assert group_advantages([]) == []

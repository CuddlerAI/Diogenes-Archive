"""Programmatic reward for GRPO refinement (doc 16 §4.4).

The reward is computed entirely from the model's own output against the
hybrid labels (game name = dataset GT, other fields = 235B teacher). It does
NOT consume any closed-model output, so there is no licensing exposure — the
GRPO reward is a program, not a teacher call.

Components, each in [0,1], combined by configurable weights:
  json_valid    the response parses into the observation schema
  game          fuzzy title match vs the reference game (only scored on
                gameplay frames, where the reference is reliable — off
                gameplay the reference is often "unknown" and we reward
                matching that abstention instead)
  event         event_class equals the reference class
  highlight     1 - |pred - ref| highlight score (calibration)

A malformed response scores 0 on everything (json_valid=0 gates the rest),
which is the signal GRPO needs to drive format compliance to 1.0.
"""
from __future__ import annotations

from dataclasses import dataclass

from .analyze_labels import GAMEPLAY_EVENTS
from .schema import SchemaError, game_fuzzy_matches, normalize_game, parse_observation

DEFAULT_WEIGHTS = {"json_valid": 0.2, "game": 0.4, "event": 0.3, "highlight": 0.1}


@dataclass
class RewardBreakdown:
    total: float
    json_valid: float
    game: float
    event: float
    highlight: float
    parsed: bool


def compute_reward(response_text: str, reference: dict,
                   weights: dict | None = None) -> RewardBreakdown:
    """Reward one model response against its reference observation.

    `reference` is a hybrid-label observation with a `event_class` that also
    tells us whether the frame is gameplay (reliable game GT) or not.
    """
    w = weights or DEFAULT_WEIGHTS
    try:
        obs = parse_observation(response_text)
    except SchemaError:
        return RewardBreakdown(0.0, 0.0, 0.0, 0.0, 0.0, False)

    json_valid = 1.0
    ref_event = reference.get("event_class")
    is_gameplay = ref_event in GAMEPLAY_EVENTS

    if is_gameplay:
        # reference game name is trustworthy here
        game = 1.0 if game_fuzzy_matches(obs["game"], reference["game"]) else 0.0
    else:
        # off gameplay, reward abstention (matching the teacher's "unknown")
        ref_unknown = normalize_game(reference["game"]) == normalize_game("unknown")
        pred_unknown = normalize_game(obs["game"]) == normalize_game("unknown")
        if ref_unknown:
            game = 1.0 if pred_unknown else 0.0
        else:
            game = 1.0 if game_fuzzy_matches(obs["game"], reference["game"]) else 0.0

    event = 1.0 if obs["event_class"] == ref_event else 0.0
    highlight = 1.0 - abs(obs["highlight_score"]
                          - float(reference.get("highlight_score", 0.0)))
    highlight = max(0.0, highlight)

    total = (w["json_valid"] * json_valid + w["game"] * game
             + w["event"] * event + w["highlight"] * highlight)
    return RewardBreakdown(total, json_valid, game, event, highlight, True)


def group_advantages(rewards: list[float], eps: float = 1e-4) -> list[float]:
    """GRPO advantage = (reward - group_mean) / (group_std + eps).

    Group-relative: no learned value function, the group's own mean is the
    baseline. A group where every sample scores the same yields ~0 advantage
    (nothing to learn from that prompt this step).
    """
    n = len(rewards)
    if n == 0:
        return []
    mean = sum(rewards) / n
    var = sum((r - mean) ** 2 for r in rewards) / n
    std = var ** 0.5
    return [(r - mean) / (std + eps) for r in rewards]

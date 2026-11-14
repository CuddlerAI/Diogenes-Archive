"""Structured observation schema — the VL model's output contract.

Aligned with the product's `parseVlmObservation` shape (doc 16 §1.1 输出全面)
so a fine-tuned model can drop into the existing pipeline. The teacher writes
exactly this JSON; SFT teaches the student to emit it; eval scores validity
field-by-field.
"""
from __future__ import annotations

import json
import re
from typing import Any

EVENT_CLASSES = (
    "combat_kill", "combat_death", "objective", "movement",
    "menu_ui", "dialogue", "loading", "idle", "other",
)

FIELDS = ("game", "scene", "event", "event_class", "subjects",
          "ocr_text", "highlight_score")

PROMPT = (
    "You are a game-screen observer. Look at the screenshot and answer with "
    "ONLY a compact JSON object, no prose, with fields: "
    '"game" (title, or "unknown"), '
    '"scene" (one short sentence), '
    '"event" (what is happening, one short sentence), '
    f'"event_class" (one of {list(EVENT_CLASSES)}), '
    '"subjects" (at most 4 visible entities, short names), '
    '"ocr_text" (at most 6 key HUD/UI strings), '
    '"highlight_score" (0.0-1.0, how clip-worthy this moment is). '
    "Keep the whole answer under 120 words."
)


class SchemaError(ValueError):
    pass


def validate_observation(obj: Any) -> dict:
    """Validate + normalize one observation; raise SchemaError if invalid."""
    if not isinstance(obj, dict):
        raise SchemaError("observation must be a JSON object")
    missing = [f for f in FIELDS if f not in obj]
    if missing:
        raise SchemaError(f"missing fields: {missing}")
    if not isinstance(obj["game"], str) or not obj["game"]:
        raise SchemaError("game must be a non-empty string")
    for f in ("scene", "event"):
        if not isinstance(obj[f], str):
            raise SchemaError(f"{f} must be a string")
    if obj["event_class"] not in EVENT_CLASSES:
        raise SchemaError(f"event_class {obj['event_class']!r} not in vocabulary")
    for f in ("subjects", "ocr_text"):
        if not isinstance(obj[f], list) or not all(isinstance(x, str) for x in obj[f]):
            raise SchemaError(f"{f} must be a list of strings")
    score = obj["highlight_score"]
    if not isinstance(score, (int, float)) or not (0.0 <= float(score) <= 1.0):
        raise SchemaError("highlight_score must be a number in [0,1]")
    return {**{f: obj[f] for f in FIELDS}, "highlight_score": float(score)}


def parse_observation(text: str) -> dict:
    """Parse a model response into a validated observation.

    Tolerates markdown fences and surrounding prose (finds the first JSON
    object) — everything else is a SchemaError, which eval counts against
    json_validity.
    """
    m = re.search(r"\{.*\}", text, flags=re.DOTALL)
    if not m:
        raise SchemaError("no JSON object in response")
    try:
        obj = json.loads(m.group(0))
    except json.JSONDecodeError as e:
        raise SchemaError(f"invalid JSON: {e}") from e
    return validate_observation(obj)


def normalize_game(name: str) -> str:
    """Loose game-title normalization for top-1 matching."""
    return re.sub(r"[^a-z0-9]", "", name.lower())


def game_matches(pred: str, truth: str) -> bool:
    """Open-vocabulary title match: normalized containment either way.

    Bench GT comes from session metadata at coarse granularity ("Forza
    Horizon", "Age of Empires") while models answer specific editions
    ("Forza Horizon 5") — exact match would punish correct answers.
    """
    p, t = normalize_game(pred), normalize_game(truth)
    if not p or not t or p == normalize_game("unknown"):
        return False
    return p in t or t in p


def game_fuzzy_matches(pred: str, truth: str, threshold: float = 0.6) -> bool:
    """Franchise-level fuzzy match (SequenceMatcher on normalized titles).

    Counts "Counter-Strike: Global Offensive" as a hit for GT
    "Counter-Strike 2" — edition confusion at 448px is scored separately
    from franchise recognition.
    """
    import difflib
    p, t = normalize_game(pred), normalize_game(truth)
    if not p or not t or p == normalize_game("unknown"):
        return False
    if p in t or t in p:
        return True
    return difflib.SequenceMatcher(None, p, t).ratio() >= threshold


# ---------------------------------------------------------------------------
# Coarse event taxonomy
#
# Two independent strong annotators (Qwen3-VL-235B and Gemini 2.5 Flash)
# agree on only 62.3% of frames under the 9-class vocabulary — LESS than the
# 78.7% a distilled student reaches against one of them. The remaining
# disagreement is not model weakness, it is the vocabulary asking for
# distinctions a single frame cannot settle:
#
#   idle vs movement (20/300)  a static image does not show motion at all
#   idle vs loading (13)       a paused game view vs a load screen
#   movement vs objective (13) moving TOWARD an objective is both
#   combat_kill vs _death (9)  one frame rarely shows who died
#   loading vs menu_ui (8)     both are non-gameplay UI
#
# Collapsing exactly those pairs lifts annotator agreement to 81.7% and
# student agreement to 96.4%. Motion- and outcome-based classes need
# temporal input (a frame window, as the IDM line uses); they are not
# recoverable by a better prompt on one frame.
COARSE_EVENT_MAP = {
    "combat_kill": "combat",
    "combat_death": "combat",
    "menu_ui": "ui",
    "loading": "ui",
    "idle": "play",
    "movement": "play",
    "objective": "play",
    "other": "play",
    "dialogue": "dialogue",
}
COARSE_EVENT_CLASSES = ("combat", "ui", "play", "dialogue")


def coarse_event(event_class: str) -> str:
    """Map a fine event class to the reliably-annotatable coarse vocabulary."""
    return COARSE_EVENT_MAP.get(event_class, event_class)


def is_substantive(obs: dict) -> bool:
    """Does a schema-valid observation actually say anything?

    validate_observation only requires `game` to be non-empty, so a response
    with empty scene/event/subjects passes it. A model emitting that shell
    for every frame scores a perfect json_valid_rate while describing
    nothing — the format gate rewards the degenerate solution.
    """
    if not str(obs.get("scene", "")).strip():
        return False
    if not str(obs.get("event", "")).strip():
        return False
    return bool(obs.get("subjects") or str(obs.get("ocr_text", "")).strip()
                or obs.get("ocr_text"))


def observation_fingerprint(obs: dict) -> tuple:
    """Identity for spotting a model that answers the same thing every frame."""
    return (str(obs.get("scene", "")).strip().lower(),
            str(obs.get("event", "")).strip().lower(),
            obs.get("event_class"))

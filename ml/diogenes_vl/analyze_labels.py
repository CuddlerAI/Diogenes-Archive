"""Measure teacher label quality — the ceiling the student can inherit.

The student is trained on teacher labels, so teacher errors become student
errors. This reports how often the teacher's game title agrees with the
session-level GT, per game and overall, plus the event-class distribution
(a teacher that answers "menu_ui" for everything teaches nothing).

Session GT is itself noisy (sessions contain launcher/desktop/loading
frames), so read the per-game table rather than the headline number: a game
where the teacher scores ~0 across all its frames is a recognition failure;
a game at ~0.5 is usually GT noise.

    python -m diogenes_vl.analyze_labels --labels data/bench_r2/labels.jsonl
"""
from __future__ import annotations

import argparse
import json
from collections import Counter, defaultdict
from pathlib import Path

from .schema import game_fuzzy_matches, game_matches, normalize_game

# event classes that imply the frame shows actual gameplay (not a menu,
# launcher, browser or loading screen)
GAMEPLAY_EVENTS = frozenset({"movement", "combat_kill", "combat_death",
                             "objective", "dialogue"})


def analyze(labels_path: str | Path) -> dict:
    rows = [json.loads(x) for x in open(labels_path) if x.strip()]
    per_game: dict[str, list] = defaultdict(list)
    events = Counter()
    unknown = 0
    exact = contain = fuzzy = 0
    committed = committed_hit = 0
    play_total = play_hit = 0

    for r in rows:
        gt, pred = r["game"], r["observation"]["game"]
        ev = r["observation"]["event_class"]
        events[ev] += 1
        is_unknown = normalize_game(pred) == normalize_game("unknown")
        unknown += is_unknown
        hit_exact = normalize_game(pred) == normalize_game(gt)
        hit_contain = game_matches(pred, gt)
        hit_fuzzy = game_fuzzy_matches(pred, gt)
        exact += hit_exact
        contain += hit_contain
        fuzzy += hit_fuzzy
        per_game[gt].append(hit_fuzzy)
        if not is_unknown:
            committed += 1
            committed_hit += hit_fuzzy
        # frames the teacher itself calls gameplay — GT is far more reliable
        # here than on menu/loading/desktop frames
        if ev in GAMEPLAY_EVENTS:
            play_total += 1
            play_hit += hit_fuzzy

    n = len(rows) or 1
    game_scores = {g: sum(v) / len(v) for g, v in sorted(per_game.items())}
    failed = {g: round(s, 3) for g, s in game_scores.items() if s < 0.2}
    return {
        "n_labels": len(rows),
        "n_games": len(per_game),
        "teacher_game_exact": exact / n,
        "teacher_game_containment": contain / n,
        "teacher_game_fuzzy": fuzzy / n,
        "unknown_rate": unknown / n,
        # accuracy when the teacher names a game at all
        "accuracy_when_committed": committed_hit / committed if committed else None,
        # accuracy on frames the teacher calls gameplay — the honest read of
        # recognition ability, since session GT is unreliable on menu/browser
        # frames (a session labeled "Assassins Creed" contains browser tabs)
        "gameplay_frame_rate": play_total / n,
        "accuracy_on_gameplay_frames": play_hit / play_total if play_total else None,
        "event_class_distribution": {k: round(v / n, 3)
                                     for k, v in events.most_common()},
        "games_teacher_fails": failed,
        "n_games_failed": len(failed),
        "per_game_fuzzy": {g: round(s, 3) for g, s in game_scores.items()},
    }


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__,
                                formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--labels", required=True)
    p.add_argument("--out", default=None, help="write full JSON report here")
    args = p.parse_args()
    rep = analyze(args.labels)
    summary = {k: v for k, v in rep.items() if k != "per_game_fuzzy"}
    print(json.dumps(summary, indent=2, ensure_ascii=False))
    if args.out:
        Path(args.out).write_text(json.dumps(rep, indent=2, ensure_ascii=False))
        print(f"full report -> {args.out}")


if __name__ == "__main__":
    main()

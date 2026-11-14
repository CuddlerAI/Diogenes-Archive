"""Compare teachers on the EXACT same frames.

Label sets annotated at different times cover different frames, and a
headline accuracy computed over different frames is not a comparison. This
intersects ids across every label file and scores all teachers on that
shared set.

Reports precision/recall separately, because they trade off: a teacher that
answers "unknown" most of the time can look accurate (high precision) while
teaching the student almost nothing (low recall). For distillation both
matter — wrong names poison training, but so does a teacher with nothing
to say.

    python -m diogenes_vl.compare_teachers \
        --labels 235b=labels_235b.jsonl 32b-awq=labels_32b.jsonl \
                 gemini=labels_gemini.jsonl
"""
from __future__ import annotations

import argparse
import json
from collections import Counter
from pathlib import Path

from .analyze_labels import GAMEPLAY_EVENTS
from .schema import game_fuzzy_matches, normalize_game


def _load(path: str | Path) -> dict[str, dict]:
    return {r["id"]: r for r in
            (json.loads(x) for x in open(path) if x.strip())}


def score(rows: list[dict]) -> dict:
    n = len(rows) or 1
    committed = hits = unknown = 0
    play_n = play_hit = play_committed = play_committed_hit = 0
    names: Counter = Counter()

    for r in rows:
        gt, pred = r["game"], r["observation"]["game"]
        is_unknown = normalize_game(pred) == normalize_game("unknown")
        hit = game_fuzzy_matches(pred, gt)
        unknown += is_unknown
        hits += hit
        if not is_unknown:
            committed += 1
            names[normalize_game(pred)] += 1
        if r["observation"]["event_class"] in GAMEPLAY_EVENTS:
            play_n += 1
            play_hit += hit
            if not is_unknown:
                play_committed += 1
                play_committed_hit += hit

    return {
        # recall: fraction of ALL frames answered correctly (unknown = miss)
        "recall_all": hits / n,
        # precision: when it names a game, how often is it right
        "precision": hits / committed if committed else None,
        "unknown_rate": unknown / n,
        # same split restricted to frames the teacher calls gameplay
        "gameplay_recall": play_hit / play_n if play_n else None,
        "gameplay_precision": (play_committed_hit / play_committed
                               if play_committed else None),
        "distinct_names": len(names),
    }


def compare(label_files: dict[str, str]) -> dict:
    sets = {name: _load(p) for name, p in label_files.items()}
    shared = set.intersection(*(set(s) for s in sets.values()))
    per_teacher = {name: score([s[i] for i in sorted(shared)])
                   for name, s in sets.items()}
    return {
        "n_shared_frames": len(shared),
        "n_games": len({sets[next(iter(sets))][i]["game"] for i in shared}),
        "per_teacher_sizes": {n: len(s) for n, s in sets.items()},
        "teachers": per_teacher,
    }


def render(rep: dict) -> str:
    lines = [f"Same-frame comparison over {rep['n_shared_frames']} frames "
             f"/ {rep['n_games']} games", "",
             f"{'teacher':<16}{'recall':>9}{'precision':>11}{'unknown':>9}"
             f"{'play-rec':>10}{'play-prec':>11}",
             "-" * 66]
    for name, m in rep["teachers"].items():
        def f(k):
            v = m.get(k)
            return f"{v:.3f}" if isinstance(v, float) else "  n/a"
        lines.append(f"{name:<16}{f('recall_all'):>9}{f('precision'):>11}"
                     f"{f('unknown_rate'):>9}{f('gameplay_recall'):>10}"
                     f"{f('gameplay_precision'):>11}")
    lines += ["", "recall = correct over ALL frames (unknown counts as miss)",
              "precision = correct when the teacher names a game"]
    return "\n".join(lines)


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__,
                                formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--labels", nargs="+", required=True,
                   metavar="NAME=PATH", help="e.g. 235b=labels_235b.jsonl")
    p.add_argument("--out", default=None)
    args = p.parse_args()
    files = dict(kv.split("=", 1) for kv in args.labels)
    rep = compare(files)
    print(render(rep))
    if args.out:
        Path(args.out).write_text(json.dumps(rep, indent=2))
        print(f"\nfull report -> {args.out}")


if __name__ == "__main__":
    main()

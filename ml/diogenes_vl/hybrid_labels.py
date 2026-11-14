"""Build SFT labels from dataset ground truth + teacher, per field.

Measured on 3034 shared frames, no open teacher we can legally train on
names the game well (best: 235B at 0.24 recall / 0.69 precision, abstaining
on 65% of frames). But the game name is the one field the dataset already
knows: gaming-500-hours carries a per-session game title. So take the game
name from GT and let the teacher supply only what GT cannot — scene, event,
subjects, OCR, highlight score.

The catch is that session GT is unreliable off gameplay: sessions contain
launcher, desktop and browser frames still labeled with the game. The
teacher's own event_class is the filter — it is the signal the teacher IS
reliable at (event agreement distilled to 0.83 in round 2).

Rules:
  gameplay frame (teacher event_class in GAMEPLAY_EVENTS)
      -> game = session GT          (GT is trustworthy here)
  non-gameplay frame
      -> game = teacher's answer    (usually "unknown"; teaches abstention
                                     instead of guessing on menus/browsers)
  every other field always comes from the teacher.

    python -m diogenes_vl.hybrid_labels --teacher-labels labels_235b.jsonl \
        --out labels_hybrid.jsonl
"""
from __future__ import annotations

import argparse
import json
from pathlib import Path

from .analyze_labels import GAMEPLAY_EVENTS
from .schema import normalize_game, validate_observation


def build_hybrid(teacher_labels: str | Path, out_path: str | Path,
                 *, drop_unknown_gameplay: bool = False) -> dict:
    rows = [json.loads(x) for x in open(teacher_labels) if x.strip()]
    n_gt = n_teacher = n_dropped = 0
    out_path = Path(out_path)
    out_path.parent.mkdir(parents=True, exist_ok=True)

    with open(out_path, "w") as f:
        for r in rows:
            if r.get("eval_only"):
                raise SystemExit(
                    f"{teacher_labels} holds eval_only labels — those may "
                    "benchmark teachers but never become training data.")
            obs = dict(r["observation"])
            is_play = obs["event_class"] in GAMEPLAY_EVENTS
            if is_play:
                obs["game"] = r["game"]           # dataset ground truth
                n_gt += 1
            else:
                n_teacher += 1
                if drop_unknown_gameplay and normalize_game(
                        obs["game"]) == normalize_game("unknown"):
                    n_dropped += 1
                    continue
            validate_observation(obs)
            f.write(json.dumps({**r, "observation": obs,
                                "label_source": "gt+teacher"}) + "\n")

    stats = {"total": len(rows), "game_from_gt": n_gt,
             "game_from_teacher": n_teacher, "dropped": n_dropped,
             "out": str(out_path)}
    print(json.dumps(stats, indent=2))
    return stats


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__,
                                formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--teacher-labels", required=True)
    p.add_argument("--out", required=True)
    p.add_argument("--drop-unknown-nonplay", action="store_true",
                   help="drop non-gameplay frames the teacher called unknown "
                        "instead of teaching abstention on them")
    args = p.parse_args()
    build_hybrid(args.teacher_labels, args.out,
                 drop_unknown_gameplay=args.drop_unknown_nonplay)


if __name__ == "__main__":
    main()

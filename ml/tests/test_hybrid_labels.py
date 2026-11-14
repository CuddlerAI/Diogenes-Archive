import json

import pytest

from diogenes_vl.hybrid_labels import build_hybrid


def _row(rid, gt_game, teacher_game, event, extra=None):
    r = {"id": rid, "game": gt_game, "split": "test",
         "observation": {"game": teacher_game, "scene": "a scene",
                         "event": "an event", "event_class": event,
                         "subjects": ["player"], "ocr_text": ["HP"],
                         "highlight_score": 0.4}}
    if extra:
        r.update(extra)
    return r


def _write(tmp_path, rows):
    p = tmp_path / "teacher.jsonl"
    p.write_text("\n".join(json.dumps(r) for r in rows))
    return p


def test_gameplay_frames_take_gt_game(tmp_path):
    src = _write(tmp_path, [_row("1", "Elden Ring", "Dark Souls", "movement")])
    out = tmp_path / "hybrid.jsonl"
    stats = build_hybrid(src, out)
    row = json.loads(open(out).readline())
    assert row["observation"]["game"] == "Elden Ring"   # teacher was wrong
    assert stats["game_from_gt"] == 1


def test_non_gameplay_frames_keep_teacher_answer(tmp_path):
    # a browser frame in a session labeled "Assassins Creed" — GT is wrong here
    src = _write(tmp_path, [_row("1", "Assassins Creed", "unknown", "menu_ui")])
    out = tmp_path / "hybrid.jsonl"
    stats = build_hybrid(src, out)
    row = json.loads(open(out).readline())
    assert row["observation"]["game"] == "unknown"      # abstention preserved
    assert stats["game_from_teacher"] == 1


def test_other_fields_always_come_from_teacher(tmp_path):
    src = _write(tmp_path, [_row("1", "Doom", "Quake", "combat_kill")])
    out = tmp_path / "hybrid.jsonl"
    build_hybrid(src, out)
    obs = json.loads(open(out).readline())["observation"]
    assert obs["game"] == "Doom"            # swapped
    assert obs["event_class"] == "combat_kill"
    assert obs["scene"] == "a scene" and obs["ocr_text"] == ["HP"]
    assert obs["highlight_score"] == 0.4


def test_eval_only_labels_are_refused(tmp_path):
    src = _write(tmp_path, [_row("1", "G", "G", "movement",
                                 {"eval_only": True})])
    with pytest.raises(SystemExit, match="never become training data"):
        build_hybrid(src, tmp_path / "hybrid.jsonl")


def test_optional_drop_of_unknown_nonplay(tmp_path):
    rows = [_row("1", "G", "unknown", "menu_ui"),
            _row("2", "G", "G", "movement")]
    src = _write(tmp_path, rows)
    out = tmp_path / "hybrid.jsonl"
    stats = build_hybrid(src, out, drop_unknown_gameplay=True)
    assert stats["dropped"] == 1
    assert len([x for x in open(out) if x.strip()]) == 1

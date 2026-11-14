import json

from diogenes_vl.compare_teachers import compare, score


def _row(rid, gt, pred, event="movement"):
    return {"id": rid, "game": gt,
            "observation": {"game": pred, "event_class": event, "scene": "",
                            "event": "", "subjects": [], "ocr_text": [],
                            "highlight_score": 0.1}}


def _write(tmp_path, name, rows):
    p = tmp_path / name
    p.write_text("\n".join(json.dumps(r) for r in rows))
    return str(p)


def test_precision_and_recall_are_distinct():
    # conservative teacher: answers twice, both right, abstains twice
    conservative = [_row("1", "Minecraft", "Minecraft"),
                    _row("2", "Chess", "Chess"),
                    _row("3", "Elden Ring", "unknown"),
                    _row("4", "Bgmi", "unknown")]
    m = score(conservative)
    assert m["precision"] == 1.0        # never wrong when it commits
    assert m["recall_all"] == 0.5       # but only half the frames answered
    assert m["unknown_rate"] == 0.5

    # guessy teacher: always answers, half wrong
    guessy = [_row("1", "Minecraft", "Minecraft"),
              _row("2", "Chess", "Chess"),
              _row("3", "Elden Ring", "Dark Souls"),
              _row("4", "Bgmi", "Apex Legends")]
    m2 = score(guessy)
    assert m2["precision"] == 0.5
    assert m2["recall_all"] == 0.5      # same recall, very different precision
    assert m2["unknown_rate"] == 0.0


def test_comparison_uses_only_shared_frames(tmp_path):
    a = _write(tmp_path, "a.jsonl", [_row("1", "G", "G"), _row("2", "H", "H"),
                                     _row("3", "I", "I")])
    # b lacks id 3, and gets id 2 wrong
    b = _write(tmp_path, "b.jsonl", [_row("1", "G", "G"), _row("2", "H", "X")])
    rep = compare({"a": a, "b": b})
    assert rep["n_shared_frames"] == 2          # id 3 excluded
    assert rep["per_teacher_sizes"] == {"a": 3, "b": 2}
    # a scored on the shared 2 only — not credited for its extra correct frame
    assert rep["teachers"]["a"]["recall_all"] == 1.0
    assert rep["teachers"]["b"]["recall_all"] == 0.5


def test_gameplay_split_excludes_menu_frames():
    rows = [_row("1", "Minecraft", "Minecraft", "movement"),
            _row("2", "Chess", "unknown", "menu_ui"),
            _row("3", "Doom", "Doom", "combat_kill")]
    m = score(rows)
    assert m["gameplay_recall"] == 1.0       # both gameplay frames correct
    assert m["recall_all"] == 2 / 3          # menu frame drags the total down

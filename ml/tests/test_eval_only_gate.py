"""Closed-model labels are benchmark-only and must never reach SFT."""
import json

import pytest

from diogenes_vl.sft import EvalOnlyLabelError, load_rows

OBS = {"game": "Minecraft", "scene": "s", "event": "e", "event_class": "movement",
       "subjects": [], "ocr_text": [], "highlight_score": 0.1}


def _write(tmp_path, rows):
    p = tmp_path / "labels.jsonl"
    p.write_text("\n".join(json.dumps(r) for r in rows))
    return str(p)


def test_open_teacher_labels_load(tmp_path):
    p = _write(tmp_path, [{"id": "1", "split": "test", "observation": OBS,
                           "teacher": "api:qwen3-vl-235b"}])
    assert len(load_rows([p], "test")) == 1


def test_eval_only_labels_hard_fail(tmp_path):
    p = _write(tmp_path, [{"id": "1", "split": "test", "observation": OBS,
                           "teacher": "gemini:gemini-2.5-flash",
                           "eval_only": True}])
    with pytest.raises(EvalOnlyLabelError, match="never become training data"):
        load_rows([p], "test")


def test_eval_only_row_fails_even_outside_requested_split(tmp_path):
    # a filtered-out row must still trip the gate — the file is tainted
    p = _write(tmp_path, [{"id": "1", "split": "val", "observation": OBS,
                           "teacher": "gemini:x", "eval_only": True}])
    with pytest.raises(EvalOnlyLabelError):
        load_rows([p], "test")


def test_gemini_teacher_marked_eval_only():
    from diogenes_vl.teacher import GeminiTeacher
    assert GeminiTeacher.EVAL_ONLY is True

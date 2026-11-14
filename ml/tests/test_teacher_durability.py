"""Annotation must survive a killed run: incremental writes + resume.

Labels were written only after the whole batch finished, so a preempted
spot VM lost every label from a multi-hour run.
"""
import json

import pytest

from diogenes_vl import teacher as teacher_mod

OBS = {"game": "Minecraft", "scene": "s", "event": "e", "event_class": "movement",
       "subjects": [], "ocr_text": [], "highlight_score": 0.1}


@pytest.fixture
def bench(tmp_path):
    (tmp_path / "images").mkdir()
    rows = []
    for i in range(10):
        (tmp_path / "images" / f"{i}.png").write_bytes(b"x")
        rows.append({"id": f"img{i}", "image": f"images/{i}.png", "game": "G",
                     "split": "val", "keys": []})
    (tmp_path / "manifest.jsonl").write_text(
        "\n".join(json.dumps(r) for r in rows))
    return tmp_path


class _Crashing:
    """Succeeds for `n_ok` calls, then raises — simulates preemption."""

    def __init__(self, n_ok):
        self.n_ok = n_ok
        self.calls = 0

    def __call__(self, bench_dir, row):
        self.calls += 1
        if self.calls > self.n_ok:
            raise KeyboardInterrupt("preempted")
        return dict(OBS)


def test_labels_survive_a_crash_midway(bench, tmp_path, monkeypatch):
    out = tmp_path / "labels.jsonl"
    monkeypatch.setattr(teacher_mod, "ApiTeacher", lambda *a, **k: _Crashing(4))
    with pytest.raises(KeyboardInterrupt):
        teacher_mod.annotate(bench, "api", out, base_url="http://x/v1",
                             model="m", concurrency=1)
    # the 4 completed labels are on disk, not lost with the process
    written = [json.loads(x) for x in open(out) if x.strip()]
    assert len(written) == 4


def test_resume_skips_already_labeled(bench, tmp_path, monkeypatch):
    out = tmp_path / "labels.jsonl"
    monkeypatch.setattr(teacher_mod, "ApiTeacher", lambda *a, **k: _Crashing(4))
    with pytest.raises(KeyboardInterrupt):
        teacher_mod.annotate(bench, "api", out, base_url="http://x/v1",
                             model="m", concurrency=1)

    seen = []

    class _Rest:
        def __call__(self, bench_dir, row):
            seen.append(row["id"])
            return dict(OBS)

    monkeypatch.setattr(teacher_mod, "ApiTeacher", lambda *a, **k: _Rest())
    stats = teacher_mod.annotate(bench, "api", out, base_url="http://x/v1",
                                 model="m", concurrency=1)
    assert stats["resumed_from"] == 4
    assert len(seen) == 6                      # only the remaining frames
    assert stats["labeled"] == 10              # total on disk
    ids = [json.loads(x)["id"] for x in open(out) if x.strip()]
    assert len(ids) == len(set(ids)) == 10     # no duplicates


def test_resume_disabled_restarts_from_scratch(bench, tmp_path, monkeypatch):
    out = tmp_path / "labels.jsonl"
    monkeypatch.setattr(teacher_mod, "ApiTeacher", lambda *a, **k: _Crashing(4))
    with pytest.raises(KeyboardInterrupt):
        teacher_mod.annotate(bench, "api", out, base_url="http://x/v1", model="m")

    monkeypatch.setattr(teacher_mod, "ApiTeacher",
                        lambda *a, **k: _Crashing(999))
    stats = teacher_mod.annotate(bench, "api", out, base_url="http://x/v1",
                                 model="m", resume=False)
    assert stats["resumed_from"] == 0
    assert len([x for x in open(out) if x.strip()]) == 10

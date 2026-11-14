"""Every network-backed teacher must honour --concurrency.

The gemini backend was silently serial because the parallel path checked
`backend == "api"`, turning a 100-second job into 20 minutes.
"""
import json
import threading

import pytest

from diogenes_vl import teacher as teacher_mod

OBS = {"game": "Minecraft", "scene": "s", "event": "e", "event_class": "movement",
       "subjects": [], "ocr_text": [], "highlight_score": 0.1}


@pytest.fixture
def bench(tmp_path):
    (tmp_path / "images").mkdir()
    rows = []
    for i in range(12):
        (tmp_path / "images" / f"{i}.png").write_bytes(b"x")
        rows.append({"id": str(i), "image": f"images/{i}.png", "game": "G",
                     "split": "val", "keys": []})
    (tmp_path / "manifest.jsonl").write_text(
        "\n".join(json.dumps(r) for r in rows))
    return tmp_path


class _CountingTeacher:
    """Records peak concurrent calls."""

    def __init__(self, *a, **k):
        self.lock = threading.Lock()
        self.active = 0
        self.peak = 0

    def __call__(self, bench_dir, row):
        with self.lock:
            self.active += 1
            self.peak = max(self.peak, self.active)
        threading.Event().wait(0.05)
        with self.lock:
            self.active -= 1
        return dict(OBS)


@pytest.mark.parametrize("backend", ["api", "gemini"])
def test_network_backends_run_concurrently(bench, tmp_path, monkeypatch, backend):
    counter = _CountingTeacher()
    monkeypatch.setattr(teacher_mod, "ApiTeacher", lambda *a, **k: counter)
    monkeypatch.setattr(teacher_mod, "GeminiTeacher", lambda *a, **k: counter)
    teacher_mod.annotate(bench, backend, tmp_path / f"{backend}.jsonl",
                         base_url="http://x/v1", model="m", project="p",
                         concurrency=6)
    assert counter.peak > 1, f"{backend} ran serially despite concurrency=6"


def test_gemini_output_is_marked_eval_only(bench, tmp_path, monkeypatch):
    class G(_CountingTeacher):
        EVAL_ONLY = True

    monkeypatch.setattr(teacher_mod, "GeminiTeacher", lambda *a, **k: G())
    stats = teacher_mod.annotate(bench, "gemini", tmp_path / "g.jsonl",
                                 model="gemini-2.5-flash", project="p")
    assert stats["eval_only"] is True
    rows = [json.loads(x) for x in open(tmp_path / "g.jsonl")]
    assert all(r["eval_only"] for r in rows)


def test_api_output_is_not_marked_eval_only(bench, tmp_path, monkeypatch):
    monkeypatch.setattr(teacher_mod, "ApiTeacher",
                        lambda *a, **k: _CountingTeacher())
    stats = teacher_mod.annotate(bench, "api", tmp_path / "a.jsonl",
                                 base_url="http://x/v1", model="qwen")
    assert stats["eval_only"] is False
    rows = [json.loads(x) for x in open(tmp_path / "a.jsonl")]
    assert not any("eval_only" in r for r in rows)

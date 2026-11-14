import json
from pathlib import Path

import pytest

from diogenes_vl import teacher as teacher_mod
from diogenes_vl.schema import SchemaError

GOOD_BODY = {"choices": [{"message": {"content": json.dumps({
    "game": "Minecraft", "scene": "s", "event": "e", "event_class": "movement",
    "subjects": [], "ocr_text": [], "highlight_score": 0.1})}}]}


class _Resp:
    def __init__(self, body):
        self._body = json.dumps(body).encode()

    def read(self):
        return self._body

    def __enter__(self):
        return self

    def __exit__(self, *a):
        return False


@pytest.fixture
def img(tmp_path):
    (tmp_path / "images").mkdir()
    (tmp_path / "images" / "a.png").write_bytes(b"fake-png")
    return tmp_path


def test_retries_then_succeeds(img, monkeypatch):
    calls = {"n": 0}

    def fake_urlopen(req, timeout=None):
        calls["n"] += 1
        if calls["n"] < 3:
            raise OSError("HTTP Error 500: Internal Server Error")
        return _Resp(GOOD_BODY)

    t = teacher_mod.ApiTeacher("http://x/v1", "m")
    monkeypatch.setattr(t._urllib, "urlopen", fake_urlopen)
    monkeypatch.setattr(teacher_mod, "json", json)
    monkeypatch.setattr("time.sleep", lambda s: None)
    obs = t(img, {"image": "images/a.png"})
    assert obs["game"] == "Minecraft"
    assert calls["n"] == 3


def test_gives_up_after_max_attempts(img, monkeypatch):
    calls = {"n": 0}

    def always_500(req, timeout=None):
        calls["n"] += 1
        raise OSError("HTTP Error 500: Internal Server Error")

    t = teacher_mod.ApiTeacher("http://x/v1", "m", max_attempts=4)
    monkeypatch.setattr(t._urllib, "urlopen", always_500)
    monkeypatch.setattr("time.sleep", lambda s: None)
    with pytest.raises(OSError):
        t(img, {"image": "images/a.png"})
    assert calls["n"] == 4


def test_annotate_aborts_when_most_labels_fail(tmp_path, monkeypatch):
    bench = tmp_path / "bench"
    (bench / "images").mkdir(parents=True)
    rows = []
    for i in range(10):
        (bench / "images" / f"{i}.png").write_bytes(b"x")
        rows.append({"id": str(i), "image": f"images/{i}.png", "game": "G",
                     "split": "val", "keys": []})
    (bench / "manifest.jsonl").write_text(
        "\n".join(json.dumps(r) for r in rows))

    class DeadTeacher:
        def __call__(self, bench_dir, row):
            raise OSError("HTTP Error 500")

    monkeypatch.setattr(teacher_mod, "ApiTeacher",
                        lambda *a, **k: DeadTeacher())
    with pytest.raises(SystemExit, match="serving side is unhealthy"):
        teacher_mod.annotate(bench, "api", tmp_path / "out.jsonl",
                             base_url="http://x/v1", model="m")


def test_schema_error_still_counts_as_failure(img, monkeypatch):
    def bad_json(req, timeout=None):
        return _Resp({"choices": [{"message": {"content": "not json"}}]})

    t = teacher_mod.ApiTeacher("http://x/v1", "m", max_attempts=2)
    monkeypatch.setattr(t._urllib, "urlopen", bad_json)
    monkeypatch.setattr("time.sleep", lambda s: None)
    with pytest.raises(SchemaError):
        t(img, {"image": "images/a.png"})

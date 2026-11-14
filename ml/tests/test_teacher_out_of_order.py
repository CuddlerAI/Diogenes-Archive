"""Finished labels must reach disk even when an earlier row is still slow.

ThreadPoolExecutor.map yields in submission order, so one row stuck in a
retry storm buffers every completed result behind it and the incremental
write never happens — a preemption then loses work that was already done.
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
    for i in range(8):
        (tmp_path / "images" / f"{i}.png").write_bytes(b"x")
        rows.append({"id": f"img{i}", "image": f"images/{i}.png", "game": "G",
                     "split": "val", "keys": []})
    (tmp_path / "manifest.jsonl").write_text(
        "\n".join(json.dumps(r) for r in rows))
    return tmp_path


class _SlowFirst:
    """Row 0 blocks until released; the rest return immediately."""

    def __init__(self):
        self.release = threading.Event()

    def __call__(self, bench_dir, row):
        if row["id"] == "img0":
            self.release.wait(timeout=10)
        return dict(OBS)


def test_fast_rows_land_on_disk_while_a_slow_row_is_pending(bench, tmp_path,
                                                          monkeypatch):
    out = tmp_path / "labels.jsonl"
    slow = _SlowFirst()
    # monkeypatch, not module assignment — a leaked patch breaks later tests
    monkeypatch.setattr(teacher_mod, "ApiTeacher", lambda *a, **k: slow)

    done = threading.Event()

    def run():
        teacher_mod.annotate(bench, "api", out, base_url="http://x/v1",
                             model="m", concurrency=4)
        done.set()

    t = threading.Thread(target=run, daemon=True)
    t.start()
    # while row 0 is still blocked, the other 7 must already be persisted
    for _ in range(100):
        if out.exists() and len([x for x in open(out) if x.strip()]) >= 7:
            break
        threading.Event().wait(0.05)
    written = len([x for x in open(out) if x.strip()]) if out.exists() else 0
    slow.release.set()
    done.wait(timeout=15)
    t.join(timeout=5)
    assert written >= 7, f"only {written} rows on disk while row 0 was pending"
    assert len([x for x in open(out) if x.strip()]) == 8

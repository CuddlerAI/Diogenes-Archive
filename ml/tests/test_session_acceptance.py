"""tools/session-acceptance/accept.py 离线全链路测试(docs/150)."""
import hashlib
import importlib.util
import json
import sys
from pathlib import Path

import pytest
import zstandard

_SPEC = importlib.util.spec_from_file_location(
    "accept",
    Path(__file__).resolve().parents[2] / "tools" / "session-acceptance" / "accept.py")
accept = importlib.util.module_from_spec(_SPEC)
_SPEC.loader.exec_module(accept)


def _z(b):
    return zstandard.ZstdCompressor().compress(b)


def _sha(b):
    return hashlib.sha256(b).hexdigest()


FRAMES = "\n".join(json.dumps(
    {"frame": i, "keys": [87, 32, 68][i % 3:i % 3 + 2],
     "actions": ["move.forward", "combat.primary_attack", "move.right"][i % 3:i % 3 + 2],
     "pointerDx": (i * 7) % 30 - 15, "pointerDy": 3, "pointerBins": [1, 0],
     "duplicate": False, "events": []}) for i in range(600)).encode()


def _mk_session(tmp, sid, *, tamper_manifest=False):
    lane = tmp / "payload"
    rows = []

    def put(clip, kind, ext, data, sha_override=None):
        h = sha_override or _sha(data)
        rel = (f"dataminer-api-production/sessions/{sid}/runs/r0/clips/{clip}/"
               f"{kind}-{h}.{ext}")
        p = lane / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_bytes(data)
        rows.append(rel)
        return f"sha256:{h}"

    for clip in ("c1", "c2"):
        man = {}
        man["video.mp4"] = put(clip, "video", "mp4", b"\x00" * 64)
        man["frames.jsonl.zst"] = put(clip, "frames", "jsonl.zst", _z(FRAMES))
        man["events.jsonl.zst"] = put(
            clip, "events", "jsonl.zst",
            _z(b'{"type":"mouse_move","qpc":1,"dx":1,"dy":0}\n'))
        man["audio.pcm.zst"] = put(clip, "audio", "pcm.zst", _z(b"\x00" * 32))
        meta = json.dumps({"target": {"game_id": "halo-infinite-steam-release-test"},
                           "n_frames": 600, "fps_num": 60, "fps_den": 1,
                           "health": {"duplicate_frames": 1}}).encode()
        man["meta.json"] = put(clip, "meta", "json", meta)
        if tamper_manifest and clip == "c1":
            man["video.mp4"] = "sha256:" + "f" * 64
        put(clip, "manifest", "json", json.dumps(man).encode())
    listing = tmp / "listing.txt"
    listing.write_text("\n".join(f"2026-08-14 00:00:00;{r}" for r in rows))
    return accept.Offline(str(listing), str(lane))


def test_accept_session_end_to_end(tmp_path):
    backend = _mk_session(tmp_path, "s1")
    by_session, _, _ = accept.parse_rows(backend.listing())
    rep = accept.accept_session(backend, "s1", by_session["s1"])
    assert rep["schema"] == accept.SCHEMA
    assert rep["clips_complete"] == 2 and rep["clips_total"] == 2
    assert rep["game_id"] == "halo-infinite-steam-release-test"
    assert rep["hours"] == round(2 * 600 / 60 / 3600, 4)
    assert rep["fps_seen"] == {"60/1": 2}
    assert rep["health"]["duplicate_frames"] == 2
    assert rep["qc"]["ran"] is True          # QC 真的跑了
    assert rep["findings"] == []
    assert rep["audio_present_clips"] == 2
    # 贫乏合成数据应被 QC 拦下(键覆盖/通道) —— 判据在工作而不是橡皮图章
    assert rep["passed"] is False
    assert "key_coverage" in rep["qc"]["failed_checks"]


def test_manifest_sha_mismatch_is_a_finding(tmp_path):
    backend = _mk_session(tmp_path, "s1", tamper_manifest=True)
    by_session, _, _ = accept.parse_rows(backend.listing())
    rep = accept.accept_session(backend, "s1", by_session["s1"])
    assert rep["health"]["manifest_mismatch"] == 1
    assert any("sha 不符" in f for f in rep["findings"])
    assert rep["passed"] is False


def test_missing_payload_kind_reported(tmp_path):
    backend = _mk_session(tmp_path, "s1")
    rows = [r for r in backend.listing()
            if not ("/clips/c2/" in r[1] and "/frames-" in r[1])]
    backend._rows = rows
    by_session, _, _ = accept.parse_rows(rows)
    rep = accept.accept_session(backend, "s1", by_session["s1"])
    assert rep["clips_complete"] == 1
    assert any("缺 ['frames']" in f for f in rep["findings"])


def test_parse_rows_skips_already_reported_sessions(tmp_path):
    backend = _mk_session(tmp_path, "s1")
    rows = list(backend.listing())
    rows.append(("2026-08-14 00:01:00",
                 f"dataminer-api-production/sessions/s1/{accept.REPORT_REL}"))
    _, _, reported = accept.parse_rows(rows)
    assert reported == {"s1"}

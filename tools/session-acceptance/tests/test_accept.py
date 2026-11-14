"""accept.py 的离线回归测试（issue #887 / docs/167 那三处修改的钉子）.

同 weekly-report 的理由: 判据与选片类代码没有测试 = 下次改动会静默改判。
这套只走 Offline 后端与纯函数, 不碰网络。
"""
from __future__ import annotations

import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import accept  # noqa: E402

V1, V2 = accept.SOURCE_LANES
SHA = "ab" * 32
EXT = {"video": "mp4", "frames": "jsonl.zst", "events": "jsonl.zst",
       "audio": "pcm.zst", "meta": "json", "manifest": "json"}


def key(lane: str, sess: str, clip: str, kind: str, run: str = "r1") -> str:
    return (f"{lane}/sessions/{sess}/runs/{run}/clips/{clip}/"
            f"{kind}-{SHA}.{EXT[kind]}")


def rows_for(lane: str, sess: str, clip: str, kinds=accept.SIX):
    return [(f"2026-09-01 10:00:00", key(lane, sess, clip, k)) for k in kinds]


# ---- 车道（docs/167 §3.2: 写死 v1 让 v2 的 391 个 session 永不被验收） ------- #
def test_default_lanes_are_the_two_source_lanes():
    assert accept.SOURCE_LANES == (
        "raw/v1/non-trainable/dataminer-api-production",
        "raw/v2/dataminer-api-production")
    # attested-trainable 是派生镜像, 计进来等于把同一批 session 验收两遍
    assert not any("attested" in l for l in accept.SOURCE_LANES)


def test_parse_rows_handles_both_lanes_and_keeps_full_keys():
    listing = rows_for(V1, "s1", "c1") + rows_for(V2, "s2", "c2")
    by_session, last_ts, reported = accept.parse_rows(listing)
    assert set(by_session) == {"s1", "s2"}
    assert by_session["s1"][0]["prefix"] == f"{V1}/sessions/s1"
    assert by_session["s2"][0]["prefix"] == f"{V2}/sessions/s2"
    assert reported == set() and set(last_ts) == {"s1", "s2"}


def test_report_object_lands_in_the_session_own_lane():
    _, _, reported = accept.parse_rows(
        [("2026-09-01 10:00:00", f"{V2}/sessions/s9/{accept.REPORT_REL}")])
    assert reported == {"s9"}


# ---- 预取清单（每 clip 4 次 rclone cat 是 45 分钟超时的病根） ---------------- #
def test_prefetch_covers_four_reads_per_complete_clip():
    rows = accept.parse_rows(rows_for(V1, "s1", "c1"))[0]["s1"]
    rels = accept.rels_to_prefetch(rows)
    assert sorted(rels) == sorted(
        key(V1, "s1", "c1", k) for k in ("meta", "manifest", "frames", "events"))
    # 正文 video/audio 从不下载——内容寻址让 sha 核对免费
    assert not any("/video-" in r or "/audio-" in r for r in rels)


def test_prefetch_skips_incomplete_clips():
    rows = accept.parse_rows(
        rows_for(V1, "s1", "c1", kinds=("video", "meta")))[0]["s1"]
    assert accept.rels_to_prefetch(rows) == []


# ---- 选片: 最旧优先 + 每轮限额（issue #887 止血） --------------------------- #
def _epoch(ts: str) -> float:
    return time.mktime(time.strptime(ts, "%Y-%m-%d %H:%M:%S"))


NOW = _epoch("2026-09-06 12:00:00")


def test_scan_is_oldest_first_and_capped():
    last_ts = {f"s{i}": f"2026-09-0{i} 01:00:00" for i in range(1, 6)}
    targets, backlog = accept.select_targets(
        last_ts, set(), quiet_minutes=15, limit=2, now_epoch=NOW)
    assert backlog == 5
    assert targets == ["s1", "s2"]          # 最旧的两个, 其余留给下一轮


def test_scan_skips_already_reported():
    last_ts = {"s1": "2026-09-01 01:00:00", "s2": "2026-09-02 01:00:00"}
    targets, backlog = accept.select_targets(
        last_ts, {"s1"}, quiet_minutes=15, limit=0, now_epoch=NOW)
    assert targets == ["s2"] and backlog == 1


def test_scan_respects_quiet_window():
    """还在写的 session 不验收——否则会对半截数据出报告。"""
    last_ts = {"still-writing": "2026-09-06 11:55:00"}   # 距 NOW 只有 5 分钟
    targets, backlog = accept.select_targets(
        last_ts, set(), quiet_minutes=15, limit=0, now_epoch=NOW)
    assert targets == [] and backlog == 0


def test_limit_zero_means_no_cap():
    last_ts = {f"s{i}": f"2026-09-0{i} 01:00:00" for i in range(1, 6)}
    targets, _ = accept.select_targets(
        last_ts, set(), quiet_minutes=15, limit=0, now_epoch=NOW)
    assert len(targets) == 5


# ---- 离线后端 --------------------------------------------------------------- #
def test_offline_backend_reads_full_keys(tmp_path):
    lst = tmp_path / "tp.txt"
    lst.write_text("\n".join(f"{ts};{p}" for ts, p in rows_for(V1, "s1", "c1")))
    payload = tmp_path / "payload"
    target = payload / key(V1, "s1", "c1", "meta")
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_bytes(b'{"ok":1}')
    b = accept.Offline(str(lst), str(payload))
    assert b.read(key(V1, "s1", "c1", "meta")) == b'{"ok":1}'
    assert b.read(key(V1, "s1", "c1", "manifest")) is None
    assert b.prefetch(["whatever"]) is None   # 离线后端的 prefetch 是空操作
    b.cleanup()

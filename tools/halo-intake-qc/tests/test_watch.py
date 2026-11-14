"""watch.py 的回归测试——锁住三个已实测发生过的缺陷。

这三条都不是假想：2026-08-23 的一次停摆告警排查里，同一份 R2 数据在
GitHub Actions（TZ=UTC）算出停摆 34.2h、在本机（CST+0800）算出 27.2h，
差值恰是时区偏移；顺着查出抑制机制的 `ts` 每轮被无条件重写，
以及 workflow 从未把 `--state-file` 传进来。

测试全部离线：不碰网络、不碰 R2。唯一可选的 rclone 集成用例在没装 rclone 时跳过。
"""

from __future__ import annotations

import importlib.util
import json
import subprocess
import sys
from datetime import datetime, timedelta, timezone
from pathlib import Path

import pytest

_WATCH = Path(__file__).resolve().parents[1] / "watch.py"
_spec = importlib.util.spec_from_file_location("halo_watch", _WATCH)
watch = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(watch)


# --------------------------------------------------------------------------- #
# 缺陷 1：rclone 打的是本地墙钟且不带偏移量 —— 不钉 TZ 就会整体错一个时区
# --------------------------------------------------------------------------- #

def test_rclone_lsf_pins_tz_to_utc(monkeypatch):
    """`rclone_lsf` 必须把子进程 TZ 钉成 UTC。

    解析侧对无偏移量的时间戳一律按 UTC 解释（`parse_tp`），而 rclone 打的是
    **运行机器本地时区**的墙钟。两者只有在 TZ=UTC 时才对得上。少了这一钉：
    在 UTC+8 上，25h 的真实停摆读成 17h —— 低于 24h 阈值，**告警不会触发**。
    """
    seen = {}

    def fake_run(cmd, **kw):
        seen["env"] = kw.get("env")
        return subprocess.CompletedProcess(cmd, 0, stdout="", stderr="")

    monkeypatch.setattr(watch.subprocess, "run", fake_run)
    watch.rclone_lsf("some-bucket", 72)

    assert seen["env"] is not None, "必须显式传 env，否则继承调用方时区"
    assert seen["env"].get("TZ") == "UTC"


def test_rclone_lsf_probes_both_source_lanes(monkeypatch):
    """两条源车道都要列。

    摄取面 2026-08-29 从 `raw/v1/non-trainable` 切到 `raw/v2`（issue #855），
    08-31 起 v1 恒零。只盯 v1 的话，停摆判据会把「新车道正在跑」读成
    「摄取停了」——反过来，老车道自然静默也会被误报成停摆。踩过一次的形状见
    docs/167 §3.2：同一处写死让周报整整一周只看到 3% 的数据还发绿卡。
    """
    calls = []

    def fake_run(cmd, **kw):
        calls.append(cmd)
        # 车道前缀必须回到输出行里，否则调用方分不清对象属于哪条车道
        return subprocess.CompletedProcess(
            cmd, 0, stdout="2026-09-01 10:00:00;s1/runs/r1/clips/c1/frames-x.jsonl.zst\n",
            stderr="")

    monkeypatch.setattr(watch.subprocess, "run", fake_run)
    rows = watch.rclone_lsf("some-bucket", 72)

    listed = [c[c.index("--format") + 2] for c in calls]   # `-R … --format tp <remote>`
    assert len(calls) == len(watch.PREFIXES) == 2
    assert any("raw/v1/non-trainable/" in r for r in listed)
    assert any("raw/v2/" in r for r in listed)
    assert len(rows) == 2
    assert any("raw/v2/dataminer-api-production/sessions/s1/" in r for r in rows)


def test_parse_tp_reads_naive_stamp_as_utc():
    """无偏移量 = UTC。这条契约靠 `rclone_lsf` 钉 TZ 来兑现，两者必须一起看。"""
    got = watch.parse_tp([f"2026-08-21 17:14:00;{watch.CLIP_MARKER}/x{watch.CLIP_SUFFIX}"])
    assert len(got) == 1
    assert got[0][0] == datetime(2026, 8, 21, 17, 14, tzinfo=timezone.utc)


def test_parse_tp_honours_explicit_offset():
    """带偏移量（rclone ≥1.66 的 --time-format RFC3339）时按偏移量算，不再假设。

    `2026-08-22T01:14:00+08:00` 与上一条的 `2026-08-21 17:14:00` 是同一时刻——
    这正是当时两台机器算出 27.2h / 34.2h 的那 8 小时差。
    """
    got = watch.parse_tp(
        [f"2026-08-22T01:14:00+08:00;{watch.CLIP_MARKER}/x{watch.CLIP_SUFFIX}"])
    assert len(got) == 1
    assert got[0][0] == datetime(2026, 8, 21, 17, 14, tzinfo=timezone.utc)


def test_rclone_really_emits_local_wallclock_without_offset(tmp_path):
    """端到端证明缺陷 1 的前提为真：rclone 的输出随 TZ 变，且不带偏移量。

    用本地目录当 remote，不需要网络与凭据。
    """
    if not _which("rclone"):
        pytest.skip("需要 rclone")
    (tmp_path / "probe.txt").write_text("x")
    args = ["rclone", "lsf", "--files-only", "--format", "t", str(tmp_path)]
    utc = subprocess.run(args, capture_output=True, text=True,
                         env=_env(TZ="UTC")).stdout.strip()
    sh = subprocess.run(args, capture_output=True, text=True,
                        env=_env(TZ="Asia/Shanghai")).stdout.strip()
    assert utc and sh
    assert "Z" not in utc and "+" not in utc, "rclone 默认不带偏移量"
    assert utc != sh, "同一文件在两个时区下打出不同墙钟——这就是那 8 小时的来源"


def _which(name):
    import shutil
    return shutil.which(name)


def _env(**kw):
    import os
    return {**os.environ, **kw}


# --------------------------------------------------------------------------- #
# 缺陷 2：`ts` 每轮被无条件重写 —— 首次通知后永远落在抑制窗内，再也不响
# --------------------------------------------------------------------------- #

def _at(monkeypatch, when: datetime):
    monkeypatch.setattr(watch, "now", lambda: when)


def test_first_alert_is_never_suppressed(tmp_path, monkeypatch):
    _at(monkeypatch, datetime(2026, 8, 22, 18, tzinfo=timezone.utc))
    assert watch.suppressed_until(str(tmp_path / "s.json"), "sig", 12.0) is None


def test_same_alert_within_window_is_suppressed(tmp_path, monkeypatch):
    sf = str(tmp_path / "s.json")
    _at(monkeypatch, datetime(2026, 8, 22, 18, tzinfo=timezone.utc))
    watch.record_notified(sf, "sig")
    _at(monkeypatch, datetime(2026, 8, 22, 21, tzinfo=timezone.utc))   # +3h
    assert watch.suppressed_until(sf, "sig", 12.0) == "2026-08-23T06:00:00Z"


def test_polling_does_not_extend_the_quiet_window(tmp_path, monkeypatch):
    """**核心回归**：3h 一轮的巡检不得把抑制窗一路顺延。

    旧实现每次调用都重写 `ts`，于是 `now - ts` 恒为 3h < 12h，首次通知之后
    永远静默。这里断言第 12 小时那一轮必须重新放行。
    """
    sf = str(tmp_path / "s.json")
    t0 = datetime(2026, 8, 22, 18, tzinfo=timezone.utc)
    _at(monkeypatch, t0)
    watch.record_notified(sf, "sig")

    for h in (3, 6, 9):                       # 窗内轮询：抑制，且不得顺延
        _at(monkeypatch, t0 + timedelta(hours=h))
        assert watch.suppressed_until(sf, "sig", 12.0) == "2026-08-23T06:00:00Z", \
            f"+{h}h 这一轮把窗口顺延了——旧 bug 复现"

    _at(monkeypatch, t0 + timedelta(hours=12))
    assert watch.suppressed_until(sf, "sig", 12.0) is None, "满 12h 必须重新通知"


def test_a_different_signature_is_not_suppressed(tmp_path, monkeypatch):
    """告警内容变了（比如从 warn 升成 critical）就该立刻叫，不吃上一条的抑制窗。"""
    sf = str(tmp_path / "s.json")
    _at(monkeypatch, datetime(2026, 8, 22, 18, tzinfo=timezone.utc))
    watch.record_notified(sf, "warn|skipped|[]")
    _at(monkeypatch, datetime(2026, 8, 22, 21, tzinfo=timezone.utc))
    assert watch.suppressed_until(sf, "critical|skipped|[]", 12.0) is None


def test_suppression_is_off_without_a_state_file(tmp_path, monkeypatch):
    _at(monkeypatch, datetime(2026, 8, 22, 18, tzinfo=timezone.utc))
    assert watch.suppressed_until(None, "sig", 12.0) is None
    watch.record_notified(None, "sig")        # 不得抛


def test_a_corrupt_state_file_does_not_swallow_the_alert(tmp_path, monkeypatch):
    """状态文件坏了要**放行**告警，不能因为读不出来就当作已通知过。"""
    sf = tmp_path / "s.json"
    sf.write_text("{ not json")
    _at(monkeypatch, datetime(2026, 8, 22, 18, tzinfo=timezone.utc))
    assert watch.suppressed_until(str(sf), "sig", 12.0) is None


def test_failed_delivery_does_not_consume_the_quiet_window(tmp_path, monkeypatch):
    """通知没发出去就不该占抑制窗——与卷首「告警绝不因通道不通而被吞掉」同一条纪律。

    调用方只在 `sent` 为真时才 `record_notified`，所以这里断言：没记录过，
    下一轮照样放行。
    """
    sf = str(tmp_path / "s.json")
    _at(monkeypatch, datetime(2026, 8, 22, 18, tzinfo=timezone.utc))
    # 模拟一次「判定要通知，但 notify 失败」——不调用 record_notified
    _at(monkeypatch, datetime(2026, 8, 22, 21, tzinfo=timezone.utc))
    assert watch.suppressed_until(sf, "sig", 12.0) is None


def test_state_file_records_utc_and_round_trips(tmp_path, monkeypatch):
    sf = tmp_path / "nested" / "s.json"        # 顺带验目录会被建出来
    _at(monkeypatch, datetime(2026, 8, 22, 18, tzinfo=timezone.utc))
    watch.record_notified(str(sf), "sig")
    st = json.loads(sf.read_text())
    assert st["signature"] == "sig"
    assert datetime.fromisoformat(st["ts"]).tzinfo is not None, "必须带时区"


# --------------------------------------------------------------------------- #
# 缺陷 4：墙钟口径把「标注员周末不开工」判成停摆 —— 每逢周末必刷 ~16 个红 run
# --------------------------------------------------------------------------- #

def test_weekend_gap_is_not_a_stall():
    """真实复现 2026-08-29/30 周末：最后入库周五 22:06Z（北京周六 06:06），
    周一 03:18Z 巡检时墙钟 53.2h 早已翻越 24h 阈值——但其中周六/周日整段
    属正常节律，工作时间口径只有周一北京 00:00 起的 ~11.3h，不该告警。
    （上上个周末 08-22/23 同款红过一轮，见 workflow 卷首。）
    """
    last = datetime(2026, 8, 28, 22, 6, 39, tzinfo=timezone.utc)
    ref = datetime(2026, 8, 31, 3, 18, 33, tzinfo=timezone.utc)
    wall = (ref - last).total_seconds() / 3600.0
    eff = watch.working_age_hours(last, ref)
    assert wall > 50, "前提：墙钟口径确实超线（这正是误报的来源）"
    assert eff < 12, f"周末剔除后应只剩周一北京时段的 ~11.3h，算出 {eff:.1f}h"


def test_weekday_stall_still_counts_one_to_one():
    """工作日的停摆一小时都不能少算——24h 阈值的抓捕速度不变。
    周二 10:00Z → 周四 10:00Z（北京周二 18:00 → 周四 18:00，全落工作日）。"""
    last = datetime(2026, 8, 25, 10, tzinfo=timezone.utc)
    ref = datetime(2026, 8, 27, 10, tzinfo=timezone.utc)
    assert abs(watch.working_age_hours(last, ref) - 48.0) < 0.01


def test_stall_spanning_weekend_fires_by_tuesday():
    """跨周末的**真实**停摆不会被周末剔除吞掉：周五晚断流、周一整天仍无数据，
    工作时间口径在周一北京 24:00（= 周一 16:00Z）越过 24h 阈值。"""
    last = datetime(2026, 8, 28, 22, 6, tzinfo=timezone.utc)
    ref = datetime(2026, 9, 1, 2, 0, tzinfo=timezone.utc)   # 周二北京 10:00
    eff = watch.working_age_hours(last, ref)
    assert eff > 24, f"周一 24h + 周二 10h ≈ 34h，算出 {eff:.1f}h"


def test_working_age_pure_weekend_is_zero():
    """整段都落在北京周末 → 0；边界条件顺带锁住「ref <= last 返回 0」。"""
    last = datetime(2026, 8, 28, 16, 1, tzinfo=timezone.utc)   # 北京周六 00:01
    ref = datetime(2026, 8, 30, 15, 59, tzinfo=timezone.utc)   # 北京周日 23:59
    assert watch.working_age_hours(last, ref) == 0.0
    assert watch.working_age_hours(ref, last) == 0.0

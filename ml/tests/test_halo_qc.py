"""`diogenes_wm.halo_qc` 的判据测试.

覆盖三件事, 因为这三件事错了都是**静默**的:

1. **每条判据的通过/不通过边界恰好落在 116 §8.3 写的那个数上** —— `≤` 与 `<`
   写反一个, 过闸小时数就会漂, 而没有任何东西会报错。`pure_w` 是 `<15%`(严格),
   其余是 `≤` / `≥`, 这个不对称是需求书原文, 不是笔误。
2. **两档的差别只有"哪些判据参与判定"** —— 阈值必须逐字相同, 否则"换档重判"
   这个能力就是假的。
3. **报告 round-trip 无损** —— 适配器只读 JSON, 报告丢字段等于选片依据丢失。
"""

from __future__ import annotations

import json

import pytest

from diogenes_wm.halo_qc import (
    AIM_CHANNEL,
    AIM_CHECKS,
    CHECK_ORDER,
    FLOOR,
    FROZEN_CHANNELS,
    REPORT_SCHEMA,
    ClipStats,
    QCThresholds,
    STRICT,
    aggregate,
    dump_report,
    evaluate_batch,
    evaluate_clip,
    evaluate_session,
    load_report,
    merge_stats,
    normalized_action_entropy,
    policy_for,
    report_from_dict,
    report_to_dict,
)

W = 87
ALL_SIX = list(FROZEN_CHANNELS)


def frame(keys=(), buttons=(), dx=0, dy=0, duplicate=False, actions=()):
    return {
        "keys": list(keys),
        "buttons": list(buttons),
        "pointerDx": dx,
        "pointerDy": dy,
        "duplicate": duplicate,
        "actions": list(actions),
    }


def busy(**kw):
    """一个既不空、指针非零、也不是纯 W 的"背景帧", 用来填充不受测的部分。"""
    base = dict(keys=[65], dx=3, dy=-1, actions=ALL_SIX)
    base.update(kw)
    return frame(**base)


def qc(rows, tier="strict", **kw):
    return evaluate_session([rows], session_id="s", tier=tier, **kw)


# ------------------------------------------------------------ 帧级统计


def test_evaluate_clip_counts_each_category_once():
    rows = [
        frame(),                                     # 空帧
        frame(keys=[W]),                             # 纯 W
        frame(keys=[W], dx=1),                       # 有 W 但指针非零 → 不算纯 W
        frame(keys=[W], buttons=[0]),                # 有按键 → 不算纯 W
        frame(dx=0, dy=-5),                          # 只有指针
        frame(keys=[65], duplicate=True),            # duplicate
        {},                                          # 缺全部字段 → 当空帧
    ]
    s = evaluate_clip(rows)
    assert (s.clips, s.frames) == (1, 7)
    assert s.empty_frames == 2          # frame() 与 {}
    assert s.pure_w_frames == 1
    assert s.pointer_nonzero_frames == 2
    assert s.duplicate_frames == 1
    assert s.keys_seen == frozenset({W, 65})
    assert s.buttons_seen == frozenset({0})
    assert s.key_coverage == 3          # 2 键 + 1 鼠标键


def test_evaluate_clip_counts_action_once_per_frame_not_per_occurrence():
    # 同一帧里重复出现的动作只记一帧 —— 判据是"帧占比", 不是"事件数"
    s = evaluate_clip([frame(actions=["move.forward", "move.forward"])])
    assert s.action_frames["move.forward"] == 1


def test_clipstats_addition_is_union_for_sets_and_sum_for_counts():
    a = evaluate_clip([frame(keys=[65], actions=["move.forward"])])
    b = evaluate_clip([frame(keys=[66], buttons=[1], actions=["move.left"])])
    m = merge_stats([a, b])
    assert m == a + b
    assert (m.clips, m.frames) == (2, 2)
    assert m.keys_seen == frozenset({65, 66})
    assert m.buttons_seen == frozenset({1})
    assert m.action_frames == {"move.forward": 1, "move.left": 1}


def test_merge_stats_of_nothing_is_all_zero():
    assert merge_stats([]) == ClipStats()


def test_pointer_nonzero_is_clamp_insensitive():
    # pointerDx/Dy 已被 clamp 到 ±200; 本判据只问"是否为零", 所以 clamp 无害。
    assert evaluate_clip([frame(dx=200)]).pointer_nonzero_frames == 1
    assert evaluate_clip([frame(dx=9999)]).pointer_nonzero_frames == 1


# ------------------------------------------------------------ 逐判据边界


def test_empty_frames_boundary_is_inclusive_at_15pct():
    assert qc([frame()] * 15 + [busy()] * 85).check("empty_frames").passed
    assert not qc([frame()] * 16 + [busy()] * 84).check("empty_frames").passed


def test_pointer_nonzero_boundary_is_inclusive_at_30pct():
    on, off = busy(), busy(dx=0, dy=0)
    assert qc([on] * 30 + [off] * 70).check("pointer_nonzero").passed
    assert not qc([on] * 29 + [off] * 71).check("pointer_nonzero").passed


def test_pure_w_boundary_is_strictly_below_15pct():
    # 116 §8.3 这一条写的是 "<15%", 与其余各条的 ≤/≥ 不对称 —— 15.0% 必须失败
    pw = frame(keys=[W], actions=["move.forward"])
    assert qc([pw] * 14 + [busy()] * 86).check("pure_w").passed
    assert not qc([pw] * 15 + [busy()] * 85).check("pure_w").passed


def test_key_coverage_counts_keys_and_buttons_together():
    seven = qc([frame(keys=[k], dx=1) for k in range(65, 72)])
    assert seven.check("key_coverage").value == 7
    assert not seven.check("key_coverage").passed
    eight = qc([frame(keys=[k], dx=1) for k in range(65, 72)]
               + [frame(buttons=[0], dx=1)])
    assert eight.check("key_coverage").value == 8
    assert eight.check("key_coverage").passed


def test_duplicate_boundary_is_inclusive_at_2pct():
    assert qc([busy(duplicate=True)] * 2 + [busy()] * 98).check("duplicate").passed
    assert not qc([busy(duplicate=True)] * 3 + [busy()] * 97).check("duplicate").passed


def test_action_entropy_boundary():
    # 两类等频 → 归一化熵 = 1.0
    even = qc([busy(actions=["move.forward"])] * 50
              + [busy(actions=["move.left"])] * 50)
    assert even.check("action_entropy").value == pytest.approx(1.0)
    assert even.check("action_entropy").passed
    # 99:1 → 熵 ≈0.08
    skew = qc([busy(actions=["move.forward"])] * 99
              + [busy(actions=["move.left"])] * 1)
    assert not skew.check("action_entropy").passed


def test_action_entropy_of_single_class_is_zero_not_one():
    # 只观测到一类动作时分母 ln(1)=0, 必须返回 0.0 而不是除零或 1.0
    assert normalized_action_entropy({"move.forward": 100}) == 0.0
    assert normalized_action_entropy({}) == 0.0


def test_aim_and_attack_share_boundary_at_1pct():
    def sess(n_aim, n_atk):
        rows = [busy(actions=["move.forward"]) for _ in range(100 - n_aim - n_atk)]
        rows += [busy(actions=[AIM_CHANNEL])] * n_aim
        rows += [busy(actions=["combat.primary_attack"])] * n_atk
        return qc(rows)

    assert sess(1, 1).check("aim_share").passed
    assert sess(1, 1).check("attack_share").passed
    assert not sess(0, 1).check("aim_share").passed
    assert not sess(1, 0).check("attack_share").passed


def test_toggle_zoom_signature_boundary_and_vacuous_pass():
    def sess(n_aim, n_atk):
        rows = [busy(actions=["move.forward"])] * (100 - n_aim - n_atk)
        rows += [busy(actions=[AIM_CHANNEL])] * n_aim
        rows += [busy(actions=["combat.primary_attack"])] * n_atk
        return qc(rows).check("toggle_zoom")

    assert sess(3, 10).passed                      # 比值 0.30
    assert not sess(1, 10).passed                  # 比值 0.10, 就是坏标签签名
    vacuous = sess(0, 0)
    assert vacuous.passed and vacuous.value is None  # attack=0 时该判据无定义
    assert "nan" in vacuous.reading


def test_toggle_zoom_exact_boundary_is_float_fragile_by_inheritance():
    """比值恰好 0.20 时判 FAIL —— 这是从运维脚本**逐字继承**的浮点行为, 不是设计。

    判据写的是 `aim >= 0.2 * atk`(两侧都是帧占比), 在 aim=2/100、atk=10/100 时
    右侧算出 0.020000000000000004 > 0.02。改成先约分再比会让读数与历史日志对不上,
    所以保留原式并把行为钉在这里 —— 影响只有恰好等于阈值的一条边, 而真实数据里
    这条判据的失败侧比值在 0.01 量级(docs/111 §5), 离边界远得多。
    """
    rows = [busy(actions=["move.forward"])] * 88
    rows += [busy(actions=[AIM_CHANNEL])] * 2
    rows += [busy(actions=["combat.primary_attack"])] * 10
    c = qc(rows).check("toggle_zoom")
    assert c.value == pytest.approx(0.2)
    assert c.reading == "aim/atk=0.20"
    assert not c.passed


def test_channel_completeness_strict_needs_six_floor_needs_five():
    five = [c for c in ALL_SIX if c != AIM_CHANNEL]
    rows = [busy(actions=five)] * 100
    s = qc(rows, tier="strict")
    assert not s.check("channel_completeness").passed
    assert s.stats.missing_channels(STRICT.channels) == [AIM_CHANNEL]
    f = qc(rows, tier="floor")
    assert f.check("channel_completeness").passed
    assert f.stats.missing_channels(FLOOR.channels) == []


def test_missing_channel_reading_lists_the_missing_ones_in_frozen_order():
    rows = [busy(actions=["combat.primary_attack"])] * 10
    c = qc(rows, tier="strict").check("channel_completeness")
    assert c.reading == "缺 move.forward, move.backward, move.left, move.right, combat.aim"


# ------------------------------------------------------------ 两档差异


def _floor_clean_strict_dirty():
    """一段只栽在 entropy / aim_share / toggle_zoom / 第六通道上的数据。

    形态就是 doc 50 §6 记的真实缺陷: 动作分布压在 `move.forward` 上(熵塌),
    `combat.aim` 一帧都没有(toggle 坏标签的极端形态), 但空帧/纯 W/指针/键覆盖/
    duplicate/attack 六项行为地板全部达标。
    """
    rows = [busy(actions=["move.forward"])] * 400
    rows += [busy(actions=["move.backward"])] * 4
    rows += [busy(actions=["move.left"])] * 4
    rows += [busy(actions=["move.right"])] * 4
    rows += [busy(actions=["combat.primary_attack"])] * 5      # 1.18% ≥ 1%
    # 键覆盖凑够 8（busy 自带 65，再补 66–72）
    rows += [frame(keys=[k], dx=1, actions=["move.forward"]) for k in range(66, 73)]
    return rows


def test_floor_passes_where_strict_fails_on_exactly_four_ids():
    rows = _floor_clean_strict_dirty()
    strict, floor = qc(rows, tier="strict"), qc(rows, tier="floor")
    assert floor.passed
    assert not strict.passed
    assert set(strict.failed_checks) == {
        "action_entropy", "aim_share", "toggle_zoom", "channel_completeness"
    }


def test_two_tiers_share_every_threshold_and_differ_only_in_enabled_set():
    assert STRICT.thresholds == FLOOR.thresholds
    assert set(STRICT.enabled_checks) == set(CHECK_ORDER)
    assert set(STRICT.enabled_checks) - set(FLOOR.enabled_checks) == {
        "action_entropy", "aim_share", "toggle_zoom"
    }
    assert not set(FLOOR.enabled_checks) - set(STRICT.enabled_checks)
    # channel_completeness 两档都开, 差的是必备通道表
    assert "channel_completeness" in FLOOR.enabled_checks
    assert STRICT.channels == FROZEN_CHANNELS
    assert FLOOR.channels == tuple(c for c in FROZEN_CHANNELS if c != AIM_CHANNEL)


def test_disabled_checks_are_still_computed_and_reported():
    rows = _floor_clean_strict_dirty()
    floor = qc(rows, tier="floor")
    assert [c.id for c in floor.checks] == list(CHECK_ORDER)   # 恒 10 条, 恒同序
    ent = floor.check("action_entropy")
    assert ent.enabled is False
    assert ent.passed is False               # 读数照算
    assert ent.id not in floor.failed_checks  # 但不进总判定
    assert floor.passed


def test_include_aim_checks_is_orthogonal_to_tier():
    rows = _floor_clean_strict_dirty()
    s = qc(rows, tier="strict", include_aim_checks=False)
    # aim 三项关掉后, strict 只剩 entropy 一项失败
    assert set(s.failed_checks) == {"action_entropy"}
    assert s.check("channel_completeness").passed      # 六通道降五
    f = qc(rows, tier="floor", include_aim_checks=True)
    assert set(f.failed_checks) == {"aim_share", "toggle_zoom", "channel_completeness"}


def test_policy_for_rejects_unknown_tier_and_accepts_custom_thresholds():
    with pytest.raises(ValueError, match="未知档位"):
        policy_for("lenient")
    p = policy_for("floor", thresholds=QCThresholds(max_empty_frac=0.5))
    assert p.thresholds.max_empty_frac == 0.5
    assert p.enabled_checks == FLOOR.enabled_checks
    assert policy_for(STRICT).tier == "strict"


def test_aim_checks_constant_matches_what_without_aim_removes():
    assert STRICT.without_aim().enabled_checks == set(CHECK_ORDER) - AIM_CHECKS
    assert AIM_CHANNEL not in STRICT.without_aim().channels
    assert STRICT.without_aim().with_aim().channels == FROZEN_CHANNELS


# ------------------------------------------------------------ session / batch


def test_aggregate_rejects_a_session_with_no_frames():
    with pytest.raises(ValueError, match="没有任何帧"):
        aggregate([], session_id="empty")


def test_hours_follow_fps():
    # 3600 帧 @60fps = 60 s = 1/60 h（Halo 产线恒 60fps，是 DEFAULT_FPS）
    assert qc([busy()] * 3600).hours == pytest.approx(1 / 60)
    # 30fps 的语料同样帧数就是双倍时长 —— 混帧率批次必须逐 session 传对 fps
    assert evaluate_session([[busy()] * 3600], session_id="s",
                            fps=30.0).hours == pytest.approx(1 / 30)


def test_evaluate_session_accepts_both_raw_rows_and_precomputed_stats():
    rows = [busy()] * 10
    a = evaluate_session([rows, rows], session_id="s")
    b = evaluate_session([evaluate_clip(rows), evaluate_clip(rows)], session_id="s")
    c = evaluate_session([rows, evaluate_clip(rows)], session_id="s")
    assert a.stats == b.stats == c.stats
    assert a.clips == 2


def test_batch_uses_median_key_coverage_not_the_union():
    # 三个 session 各只有 3 个键, 但并起来有 9 个 —— 批级必须报中位数 3, 不是 9
    sessions = {
        f"s{i}": [[frame(keys=[65 + 3 * i + j], dx=1, actions=ALL_SIX)
                   for j in range(3)]]
        for i in range(3)
    }
    b = evaluate_batch(sessions, tier="strict")
    assert b.scope == "batch"
    assert b.n_sessions == 3
    assert b.stats.key_coverage == 9              # 并集仍然记在 stats 上
    assert b.check("key_coverage").value == 3     # 判据读的是中位数
    assert not b.check("key_coverage").passed
    assert b.check("key_coverage").label == "键覆盖中位数 ≥8"


def test_batch_channel_reading_counts_sessions_not_channels():
    sessions = {
        "good": [[busy(actions=ALL_SIX)] * 10],
        "bad": [[busy(actions=["move.forward"])] * 10],
    }
    b = evaluate_batch(sessions, tier="strict")
    assert b.check("channel_completeness").reading == "1 个会话缺通道"
    assert set(b.missing_channels) == {"bad"}


def test_batch_can_pass_while_a_member_session_fails():
    """批级会让好 session 稀释坏 session —— 选片必须用 evaluate_session。"""
    good = [busy(actions=ALL_SIX)] * 100
    bad = [frame()] * 100                     # 全空帧
    sessions = {"good": [good] * 9, "bad": [bad]}
    b = evaluate_batch(sessions, tier="floor")
    assert b.check("empty_frames").passed      # 10% 空帧, 批级过
    assert not evaluate_session([bad], session_id="bad", tier="floor").passed


def test_batch_rejects_empty_input():
    with pytest.raises(ValueError, match="没有任何帧"):
        evaluate_batch({"s": [[]]})


# ------------------------------------------------------------ 报告读写


def _two_sessions():
    ok = evaluate_session([[busy(actions=ALL_SIX)] * 100
                           + [frame(keys=[k], dx=1) for k in range(66, 74)]],
                          session_id="sess-ok", tier="floor")
    bad = evaluate_session([[frame()] * 100], session_id="sess-bad", tier="floor")
    return [ok, bad]


def test_report_roundtrip_preserves_every_check_and_total(tmp_path):
    results = _two_sessions()
    path = tmp_path / "qc.json"
    written = dump_report(path, results, tier="floor")
    rep = load_report(path)

    assert rep.schema == REPORT_SCHEMA
    assert rep.tier == "floor"
    assert rep.policy.enabled_checks == FLOOR.enabled_checks
    assert rep.policy.channels == FLOOR.channels
    assert rep.policy.thresholds == FLOOR.thresholds
    assert rep.totals == written["totals"]
    assert rep.totals["sessions"] == 2 and rep.totals["passed_sessions"] == 1

    for before, after in zip(results, rep.sessions):
        assert after.session_id == before.session_id
        assert after.scope == before.scope
        assert after.passed == before.passed
        assert after.failed_checks == before.failed_checks
        assert after.checks == before.checks
        assert after.stats == before.stats
        assert after.hours == pytest.approx(before.hours)
        assert after.missing_channels == before.missing_channels


def test_report_exposes_the_selection_api_the_adapter_needs(tmp_path):
    path = tmp_path / "qc.json"
    dump_report(path, _two_sessions(), tier="floor")
    rep = load_report(path)
    assert rep.passed_session_ids == frozenset({"sess-ok"})
    assert rep.is_passed("sess-ok")
    assert not rep.is_passed("sess-bad")
    # 未体检的 session 默认不装配, 但调用方可以显式放行
    assert not rep.is_passed("never-seen")
    assert rep.is_passed("never-seen", default=True)
    assert set(rep.by_session) == {"sess-ok", "sess-bad"}


def test_report_is_plain_json_and_schema_is_checked(tmp_path):
    path = tmp_path / "qc.json"
    dump_report(path, _two_sessions(), tier="floor")
    raw = json.loads(path.read_text(encoding="utf-8"))
    assert raw["schema"] == REPORT_SCHEMA
    assert [c["id"] for c in raw["sessions"][0]["checks"]] == list(CHECK_ORDER)
    assert raw["sessions"][0]["stats"]["missing_channels_by_session"] == {}

    with pytest.raises(ValueError, match="schema"):
        report_from_dict({**raw, "schema": "halo-qc-report/999"})


def test_report_to_dict_and_from_dict_are_pure(tmp_path):
    results = _two_sessions()
    d = report_to_dict(results, tier="floor")
    rep = report_from_dict(d)
    assert rep.passed_session_ids == frozenset({"sess-ok"})
    assert json.loads(json.dumps(d)) == d       # 必须是纯 JSON 可序列化的


def test_batch_result_survives_the_report_roundtrip(tmp_path):
    sessions = {
        "good": [[busy(actions=ALL_SIX)] * 10],
        "bad": [[busy(actions=["move.forward"])] * 10],
    }
    b = evaluate_batch(sessions, tier="strict", label="batch-3d")
    path = tmp_path / "batch.json"
    dump_report(path, [b], tier="strict")
    back = load_report(path).sessions[0]
    assert back.scope == "batch" and back.n_sessions == 2
    assert back.missing_channels == b.missing_channels
    assert back.check("channel_completeness").reading == "1 个会话缺通道"


def test_toggle_zoom_none_value_survives_json(tmp_path):
    s = evaluate_session([[busy(actions=["move.forward"])] * 10],
                         session_id="no-attack", tier="strict")
    path = tmp_path / "qc.json"
    dump_report(path, [s], tier="strict")
    assert json.loads(path.read_text())["sessions"][0]["checks"][8]["value"] is None
    assert load_report(path).sessions[0].check("toggle_zoom").value is None

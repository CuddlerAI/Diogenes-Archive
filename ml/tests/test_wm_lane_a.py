"""Lane A 工具的单元测试: COD 适配器 / 覆盖率审计 / 探底跨会话取样.

夹具用 pyarrow 现写三张 parquet + 一个占位 mp4, 不碰真实语料 —— 真实语料是
non-trainable 的, 不进仓库也不进 CI。
"""
import json
from pathlib import Path

import pytest

from diogenes_phase0.contract import (
    ComplianceError, gate_chunks, is_trainable_meta,
)
from diogenes_phase0.data.dataset import scan_dataset
from diogenes_wm.cod_adapter import convert
from diogenes_wm.coverage_audit import audit, report
from diogenes_wm.tokenizer_probe import select_chunks

pq = pytest.importorskip("pyarrow.parquet")
pa = pytest.importorskip("pyarrow")

QPC_PER_FRAME = 333_334  # 10 MHz / 30 fps, 与真实语料同一时基


def _build_source(root: Path, *, sessions=2, segments=2, frames=6) -> Path:
    """写一份最小 COD 构建: segments/frames/events 三张表 + 占位 mp4."""
    (root / "data").mkdir(parents=True)
    segs, frs, evs = [], [], []
    for s in range(sessions):
        sid = f"dev-100000000{s}-000{s}"
        for g in range(segments):
            seg_id = f"{g:05d}"
            anchor = 1_000_000 + g * frames * QPC_PER_FRAME
            vp = f"videos/{sid}/r0000/{seg_id}.mp4"
            (root / vp).parent.mkdir(parents=True, exist_ok=True)
            (root / vp).write_bytes(b"\x00fake-mp4")
            segs.append(dict(
                session_id=sid, run_id="r0000", segment_id=seg_id, segment_index=g,
                video_path=vp, video_bytes=9, video_sha256="0" * 64,
                num_frames=frames, num_events=frames, duration_s=frames / 30,
                width=1920, height=1080, fps=30.0,
                game_id="cod-test", catalog_id="cat", catalog_version="v1",
                catalog_mode="dev_non_trainable", mapping_id="map",
                mapping_version="m1", mean_bitrate_bps=16_000_000,
                run_state="non_trainable_complete", run_stop_reason="foreground_lost",
                non_trainable=True, production_certified=False,
                qpc_start=anchor, qpc_end=anchor + frames * QPC_PER_FRAME,
                run_anchor_qpc=anchor, qpc_frequency=10_000_000, run_t_us=0,
            ))
            for f in range(frames):
                lo = anchor + f * QPC_PER_FRAME
                hi = lo + QPC_PER_FRAME
                # 每帧塞两条 mouse_move: 和 = (2f+1, -f), 越界帧用来验 clamp
                evs.append(dict(session_id=sid, run_id="r0000", segment_id=seg_id,
                                seq=2 * f, qpc=lo + 10, type="mouse_move",
                                device="m", vk=0, scan_code=0, flags=0,
                                dx=f + 1, dy=-f, absolute=False))
                evs.append(dict(session_id=sid, run_id="r0000", segment_id=seg_id,
                                seq=2 * f + 1, qpc=lo + 20, type="mouse_move",
                                device="m", vk=0, scan_code=0, flags=0,
                                dx=f, dy=0, absolute=False))
                raw_dx = 2 * f + 1
                frs.append(dict(
                    session_id=sid, run_id="r0000", segment_id=seg_id, frame=f,
                    session_frame=g * frames + f, video_t_us=f * 33_333,
                    output_qpc_start=lo, output_qpc_end=hi,
                    keys=[87] if f % 2 else [],
                    actions=["combat.aim"] if f == 1 else [],
                    pointer_dx=min(raw_dx, 200), pointer_dy=-f,
                    pointer_bins=[1, 0], duplicate=False,
                ))
    for name, rows in (("segments", segs), ("frames", frs), ("events", evs)):
        pq.write_table(pa.Table.from_pylist(rows), root / "data" / f"{name}.parquet")
    return root


def test_adapter_emits_chunk_contract_and_gate_refuses(tmp_path):
    src = _build_source(tmp_path / "src")
    out = tmp_path / "out"
    stats = convert(src, out, per_session=None, sessions=None, verify=0)
    assert stats["chunks"] == 4 and stats["sessions"] == 2

    recs = scan_dataset(out)
    assert len(recs) == 4
    # 合规戳逐字来自源: 每个 chunk 都不可训练, 且默认门必须硬拦
    assert not any(is_trainable_meta(r.meta) for r in recs)
    with pytest.raises(ComplianceError):
        gate_chunks([r.meta for r in recs], allow_method_validation_only=False)
    # 显式的方法验证通道才放行
    gate_chunks([r.meta for r in recs], allow_method_validation_only=True)


def test_owner_attested_lane_passes_gate_and_carries_no_publish(tmp_path):
    """owner 具结车道(2026-08-01 裁决): 可训练, 但「不得发布」必须跟着数据走。"""
    src = _build_source(tmp_path / "src")
    out = tmp_path / "out"
    stats = convert(src, out, per_session=None, sessions=None, verify=0,
                    rights="first_party", consent_id="owner-attest-2026-08-01")
    assert stats["is_trainable"] is True

    recs = scan_dataset(out)
    assert all(is_trainable_meta(r.meta) for r in recs)
    gate_chunks([r.meta for r in recs], allow_method_validation_only=False)

    meta = json.loads(next(out.rglob("meta.json")).read_text())
    assert meta["consent_id"] == "owner-attest-2026-08-01"
    # 产品认证位不跟着翻 —— 「验证用、不发布」不是产品认证
    assert meta["production_certified"] is False
    # 约束留在数据上, 不是只留在某一次对话里
    assert {"no_publish", "no_product_training"} <= set(meta["tags"])


def test_owner_attested_lane_refuses_half_flips(tmp_path):
    """门是四项与; 只翻 rights 不给 consent-id 会让人以为改了其实没改。"""
    src = _build_source(tmp_path / "src")
    with pytest.raises(ValueError, match="consent-id"):
        convert(src, tmp_path / "a", per_session=None, sessions=None,
                verify=0, rights="first_party", consent_id=None)
    with pytest.raises(ValueError, match="consent-id"):
        convert(src, tmp_path / "b", per_session=None, sessions=None,
                verify=0, rights="first_party", consent_id="   ")
    # 反向也要拦: third_party 车道不接受具结 ID
    with pytest.raises(ValueError, match="third_party"):
        convert(src, tmp_path / "c", per_session=None, sessions=None,
                verify=0, rights="third_party", consent_id="x")


def test_adapter_recomputes_untruncated_pointer_from_events(tmp_path):
    """`pointerRawDx` 必须是该帧 qpc 半开区间内 mouse_move 的和, 且不被 clamp."""
    src = _build_source(tmp_path / "src", frames=6)
    out = tmp_path / "out"
    convert(src, out, per_session=1, sessions=None, verify=0)
    rows = [json.loads(x) for x in
            next(out.rglob("p0.jsonl")).read_text().splitlines()]
    for f, row in enumerate(rows):
        assert row["pointerRawDx"] == 2 * f + 1, "半开区间求和不对"
        assert row["pointerRawDy"] == -f
        # 索引表那一路是封顶的, 两者在越界帧上必须分叉
        assert row["pointerDx"] == min(2 * f + 1, 200)


def test_adapter_fps_is_declared_as_rational(tmp_path):
    """契约拒绝没声明 fps 的 chunk; 且必须给 fps_num/fps_den 而不是只给整数."""
    src = _build_source(tmp_path / "src")
    out = tmp_path / "out"
    convert(src, out, per_session=1, sessions=None, verify=0)
    meta = json.loads(next(out.rglob("meta.json")).read_text())
    assert meta["video"]["fps_num"] == 30 and meta["video"]["fps_den"] == 1
    assert scan_dataset(out)[0].meta.fps == 30.0


def test_coverage_counts_mouse_only_frames_as_input(tmp_path):
    """只按了鼠标键的帧也算「带输入信号」——漏掉它会系统性低估覆盖率。"""
    src = _build_source(tmp_path / "src", sessions=1, segments=1, frames=6)
    out = tmp_path / "out"
    convert(src, out, per_session=None, sessions=None, verify=0)
    a = audit(out)
    assert a["frames"] == 6
    # frame 0: keys 空、dx=1(非零) -> 算; frame 1: keys+actions -> 算
    # 夹具里每帧 dx 都非零, 所以全部算; 关键是 actions 那一路不会让计数溢出
    assert a["frames_with_input_pct"] == pytest.approx(100.0)
    assert "combat.aim" in a["action_frames"]
    assert a["pointer_clamp_exceeded_pct"] == 0.0
    assert "覆盖率缺口表" in report(a)


def test_coverage_flags_short_sessions_and_partial_key_coverage(tmp_path):
    src = _build_source(tmp_path / "src", sessions=2, segments=1, frames=6)
    out = tmp_path / "out"
    convert(src, out, per_session=None, sessions=None, verify=0)
    a = audit(out)
    # 6 帧 @30fps = 0.2s, 远低于 600s 纪律 —— 两个会话都必须被点名
    assert len(a["sessions_under_discipline"]) == 2
    assert a["session_seconds_median"] == pytest.approx(0.2)


def test_tokenizer_probe_spread_crosses_sessions(tmp_path):
    """`--spread` 必须跨会话取样; 头部取样会把 N 个 chunk 全从第一个会话拿走."""
    src = _build_source(tmp_path / "src", sessions=4, segments=3, frames=2)
    out = tmp_path / "out"
    convert(src, out, per_session=None, sessions=None, verify=0)

    def sessions_of(recs):
        return {r.chunk_dir.parent.name for r in recs}

    head = select_chunks(out, max_chunks=3, spread=False)
    spread = select_chunks(out, max_chunks=3, spread=True)
    assert len(sessions_of(head)) == 1
    assert len(sessions_of(spread)) == 3

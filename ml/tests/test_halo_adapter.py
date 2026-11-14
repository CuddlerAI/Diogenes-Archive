"""Halo 具结车道适配器的单元测试.

夹具在临时目录里现造 R2 那套**内容寻址**的六件套(真 zstd 压缩 + 真 sha256 文件名 +
真 manifest), 不碰 R2、不碰网络 —— 真实语料是合作授权内容, 不进仓库也不进 CI。
"""
import hashlib
import io
import json
import sys
from pathlib import Path

import pytest

from diogenes_phase0.contract import (
    ChunkMeta, ComplianceError, gate_chunks, gate_fps, gate_sources,
    is_trainable_meta,
)
from diogenes_phase0.data.dataset import scan_dataset
from diogenes_wm.halo_adapter import (
    DEFAULT_GAME_ID, QC_WAIVED_FAILURES, HaloAdapterError, compliance_stamp,
    convert, load_qc_gate, raw_pointer_by_frame, unzstd,
)

zstd = pytest.importorskip("zstandard")

#: 具结车道的真实来源坐标。`--rights first_party` 下申报来源是**硬要求**
#: (产出可训练字节却不留来源坐标 = 只上了一把锁), 所以每个走具结车道的用例都要给。
HALO_URI = ("r2://diogenes-model-training/raw/v1/attested-trainable/"
            "dataminer-api-production/sessions")

QPC_HZ = 10_000_000
QPC_PER_FRAME = QPC_HZ // 60  # 166_666, 与真实 60fps 语料同一时基


# --------------------------------------------------------------------------- #
# 夹具: 造一个内容寻址的 clip
# --------------------------------------------------------------------------- #
def _zstd(data: bytes) -> bytes:
    """流式压缩 —— 和真实语料一样, 帧头**不**带 content size。

    空输入压出来正好是 9 字节的空帧, 即车道里那 128 个"真实操作空窗"的形状。
    """
    c = zstd.ZstdCompressor().compressobj()
    return c.compress(data) + c.flush()


def _sha(blob: bytes) -> str:
    return hashlib.sha256(blob).hexdigest()


def _jsonl_blob(rows) -> bytes:
    return "".join(json.dumps(r) + "\n" for r in rows).encode()


def _digest_for(frames, events):
    """按半开区间给每帧填 `events` 摘要 —— 真实 frames.jsonl 就带这个字段。"""
    moves = sorted((e["qpc"], e) for i, e in enumerate(events)
                   if e.get("type") == "mouse_move" and not e.get("absolute"))
    for fr in frames:
        lo, hi = fr["output_qpc_start"], fr["output_qpc_end"]
        fr["events"] = [{"type": "mouse_move", "dx": e.get("dx", 0),
                         "dy": e.get("dy", 0)}
                        for q, e in moves if lo <= q < hi]
    return frames


def _frame(i, *, anchor, span=QPC_PER_FRAME, keys=None, buttons=None,
           actions=None, pdx=0, pdy=0, run_id="r0003", session_frame_start=26222):
    lo = anchor + i * span
    return {
        "frame": i,
        "session_frame": session_frame_start + i,
        "run_id": run_id,
        "video_t_us": i * 16_666,
        "output_qpc_start": lo,
        "output_qpc_end": lo + span,
        "source_qpc": lo + 1,
        "source_seq": 12_321 + i,
        "duplicate": False,
        "source_superseded": 0,
        "queue_drop": 0,
        "keys": list(keys or []),
        "buttons": list(buttons or []),
        "actions": list(actions or []),
        "pointerDx": pdx,
        "pointerDy": pdy,
        "pointerBins": [0, 0],
        "events": [],
        "src": "rec",
    }


def _move(qpc, dx, dy, *, absolute=False, seq=0):
    return {"seq": seq, "run_id": "r0003", "qpc": qpc, "type": "mouse_move",
            "device": "mouse:deadbeef", "dx": dx, "dy": dy, "absolute": absolute}


def write_clip(root: Path, *, session="021930b8-3ef9-4ee1-982c-21ab86b9ccb3",
               run_uuid="24e2ae95-6b05-4b98-9d06-ed10c3c97d49",
               clip_id="01d60b75-aaf5-4452-8407-81eeb3814b6f",
               frames=None, events=None, game_id=DEFAULT_GAME_ID,
               with_target=True, fps=(60, 1), run_id="r0003", segment_index=14,
               n_frames_override=None, corrupt_manifest_for=None,
               video=b"\x00\x00\x00\x18ftypisom-fake-mp4",
               device_session_id="dev-1785830459723-0009") -> Path:
    """写一个六件套 clip, 文件名内嵌 sha256, manifest 与 meta 各存一份哈希。"""
    frames = frames if frames is not None else [
        _frame(i, anchor=62_641_900_169) for i in range(6)]
    events = events if events is not None else []
    _digest_for(frames, events)

    frames_blob = _zstd(_jsonl_blob(frames))
    events_blob = _zstd(_jsonl_blob(events))
    audio_blob = _zstd(b"\x00" * 32)
    anchor = frames[0]["output_qpc_start"]
    meta = {
        "audio": {"bytes": len(audio_blob), "path": "audio.pcm.zst",
                  "qpc_frequency": QPC_HZ, "sha256": f"sha256:{_sha(audio_blob)}",
                  "source": "process_loopback", "state": "ready",
                  "first_sample_qpc": anchor},
        "catalog": {
            "catalog_id": "dataminer-halo-infinite-steam-release-test",
            "catalog_version": "2026-08-04.0",
            "catalog_content_hash": "sha256:" + "7" * 64,
            "mode": "dev_non_trainable", "non_trainable": True,
            "provenance": "compiled",
            "game": {"id": game_id, "version": "2"},
            "mapping": {"id": "game.halo-infinite.steam.pc",
                        "version": "2026-07-29.0",
                        "content_hash": "sha256:" + "e" * 64},
            "device": {"id": "halo-windows-nvidia-v1", "version": "2"},
            "recording": {"id": "halo-recording-v1", "version": "3"},
        },
        "contract": {"version": "dm-1", "pointer_clamp": 200,
                     "pointer_max_bin": 10, "pointer_mu": 8, "pointer_ref": 200},
        "events": {"lines": len(events), "path": "events.jsonl.zst",
                   "sha256": f"sha256:{_sha(events_blob)}"},
        "fps_num": fps[0], "fps_den": fps[1],
        "frames": {"lines": len(frames), "path": "frames.jsonl.zst",
                   "sha256": f"sha256:{_sha(frames_blob)}"},
        "health": {"duplicate_frames": 0, "queue_drops": 0, "input_overflow": 0},
        "height": 1080, "width": 1920,
        "input_status": "raw_input_v1",
        "mapping": {"id": "game.halo-infinite.steam.pc",
                    "version": "2026-07-29.0",
                    "content_hash": "sha256:" + "e" * 64},
        "n_frames": n_frames_override if n_frames_override is not None else len(frames),
        "non_trainable": True,
        "probe": {"codec": "H.264", "fps_num": fps[0], "fps_den": fps[1],
                  "sample_count": len(frames), "width": 1920, "height": 1080,
                  "pts_monotonic": True},
        "production_certified": False,
        "qpc_start": anchor,
        "qpc_end": frames[-1]["output_qpc_end"],
        "run_id": run_id,
        "schema_version": "dm-local-dev-recording-2",
        "segment_index": segment_index,
        "session_frame_start": frames[0]["session_frame"],
        "session_id": device_session_id,
        "state": "non_trainable_ready",
        "video": {"bytes": len(video), "path": "video.mp4",
                  "sha256": f"sha256:{_sha(video)}"},
    }
    if with_target:
        meta["target"] = {"game_id": game_id,
                          "target_id": "target-" + "a" * 64,
                          "executable_sha256": "sha256:" + "b" * 64}

    meta_blob = json.dumps(meta, indent=2, sort_keys=True).encode()
    payload = {
        "meta.json": (meta_blob, "meta", "json"),
        "frames.jsonl.zst": (frames_blob, "frames", "jsonl.zst"),
        "events.jsonl.zst": (events_blob, "events", "jsonl.zst"),
        "video.mp4": (video, "video", "mp4"),
        "audio.pcm.zst": (audio_blob, "audio", "pcm.zst"),
    }
    manifest = {name: f"sha256:{_sha(blob)}" for name, (blob, _, _) in payload.items()}
    if corrupt_manifest_for:
        manifest[corrupt_manifest_for] = "sha256:" + "0" * 64
    manifest_blob = json.dumps(manifest, indent=2, sort_keys=True).encode()

    d = root / session / "runs" / run_uuid / "clips" / clip_id
    d.mkdir(parents=True, exist_ok=True)
    for name, (blob, stem, ext) in payload.items():
        (d / f"{stem}-{_sha(blob)}.{ext}").write_bytes(blob)
    (d / f"manifest-{_sha(manifest_blob)}.json").write_bytes(manifest_blob)
    return d


# --------------------------------------------------------------------------- #
# 正常路径 + 两条合规车道
# --------------------------------------------------------------------------- #
def test_emits_chunk_contract_and_default_lane_is_blocked(tmp_path):
    src = tmp_path / "src"
    write_clip(src)
    out = tmp_path / "out"
    stats = convert(src, out, verify=0)

    assert stats["chunks_written"] == 1 and stats["sessions"] == 1
    chunk = out / "021930b8-3ef9-4ee1-982c-21ab86b9ccb3" / "r0003-00014"
    assert (chunk / "p0.mp4").is_symlink()
    assert (chunk / "p0.jsonl").exists() and (chunk / "meta.json").exists()
    # p0.jsonl 行数 = n_frames, 与视频帧 1:1
    assert len((chunk / "p0.jsonl").read_text().splitlines()) == 6
    assert stats["frames"] == 6
    # doc 44 口径: Σ帧数 ÷ fps, 不按 clip 数折算
    assert stats["seconds"] == pytest.approx(0.1)
    assert stats["hours"] == pytest.approx(0.1 / 3600, abs=1e-6)

    recs = scan_dataset(out)
    assert len(recs) == 1
    meta = recs[0].meta
    assert isinstance(meta, ChunkMeta) and meta.fps == 60.0
    assert meta.game == DEFAULT_GAME_ID
    assert meta.uses_per_game_mapping           # mapping 身份进了 provenance
    # 默认档逐字抄源 meta 的 non_trainable=true, 四项与过不了
    assert not is_trainable_meta(meta)
    with pytest.raises(ComplianceError):
        gate_chunks([meta], allow_method_validation_only=False)
    gate_chunks([meta], allow_method_validation_only=True)
    gate_fps([meta], expected_fps=60.0)


def test_owner_attested_lane_passes_gate_and_carries_halo_labels(tmp_path):
    src = tmp_path / "src"
    write_clip(src)
    out = tmp_path / "out"
    stats = convert(src, out, verify=0, rights="first_party",
                    consent_id="owner-attest-2026-08-01", source_uri=HALO_URI)
    assert stats["is_trainable"] is True

    recs = scan_dataset(out)
    assert all(is_trainable_meta(r.meta) for r in recs)
    gate_chunks([r.meta for r in recs], allow_method_validation_only=False)

    meta = json.loads((next(out.rglob("meta.json"))).read_text())
    assert meta["consent_id"] == "owner-attest-2026-08-01"
    # 产品认证位不跟着翻 —— 「验证用、不发布」不是产品认证
    assert meta["production_certified"] is False
    # Halo 是微软合作授权内容: C-1 覆盖采集与内部训练, 对外需 C-2 —— 标签跟着数据走
    assert {"halo_partner_licensed", "no_publish", "owner_attested_validation",
            "c2_not_obtained", "no_external_use"} <= set(meta["tags"])
    # 边界本身也跟着数据走: 只有一个 halo_partner_licensed 标签读起来像"可以发"
    boundary = meta["license_boundary"]
    assert boundary["authorization"] == "C-1"
    assert "内部训练" in boundary["covers"]
    assert "对外测试" in boundary["not_covered"] and "C-2" in boundary["pending"]
    # 源侧的治理事实原样留痕, 不被输出档掩盖
    assert meta["provenance"]["source_non_trainable"] is True
    assert meta["provenance"]["lane"] == "attested-trainable"
    # 四项与里唯一一个"断言而非抄录"的分量, 源侧原值留痕(源 schema 无该字段 → null)
    assert meta["provenance"]["source_declares_cutscene"] is None
    # 来源坐标的成色: 本地镜像是操作者断言的, 不是列桶列出来的
    assert meta["provenance"]["source_coordinate"].startswith("asserted")


def test_half_flipped_lane_is_refused(tmp_path):
    src = tmp_path / "src"
    write_clip(src)
    with pytest.raises(ValueError, match="consent-id"):
        convert(src, tmp_path / "a", verify=0, rights="first_party")
    with pytest.raises(ValueError, match="consent-id"):
        convert(src, tmp_path / "b", verify=0, rights="first_party",
                consent_id="   ")
    with pytest.raises(ValueError, match="third_party"):
        convert(src, tmp_path / "c", verify=0, rights="third_party",
                consent_id="x")
    assert compliance_stamp("third_party", None)["is_trainable"] is False


# --------------------------------------------------------------------------- #
# 车道杂质: game_id / 缺 target / fps
# --------------------------------------------------------------------------- #
def test_game_id_filter_rejects_other_titles_and_orphans(tmp_path):
    """具结车道是混的; 且那个 fallout4 孤儿**没有** target 字段 —— 绝不按路径推断。"""
    src = tmp_path / "src"
    write_clip(src, clip_id="c-halo-0000")
    write_clip(src, clip_id="c-wolf-0000", game_id="wolfenstein2-steam")
    write_clip(src, clip_id="c-orphan-00", with_target=False,
               game_id="fallout4-steam")
    out = tmp_path / "out"
    stats = convert(src, out, verify=0)

    assert stats["chunks_written"] == 1
    assert stats["rejected"]["game_id_mismatch"] == 1
    assert stats["rejected"]["missing_target"] == 1
    # 统计里能看见车道构成, 而不是只看见"收了 1 个"
    assert stats["game_id_seen"] == {DEFAULT_GAME_ID: 1, "wolfenstein2-steam": 1}
    assert {r.meta.game for r in scan_dataset(out)} == {DEFAULT_GAME_ID}


def test_non_matching_fps_is_rejected_not_mixed_in(tmp_path):
    """gate_fps 对混合帧率硬报错, 所以混进来的必须在转换期就挡掉并计数。"""
    src = tmp_path / "src"
    write_clip(src, clip_id="c-60", segment_index=1)
    write_clip(src, clip_id="c-30", segment_index=2, fps=(30, 1))
    out = tmp_path / "out"
    stats = convert(src, out, verify=0)

    assert stats["chunks_written"] == 1
    assert stats["rejected"]["fps_mismatch"] == 1
    assert stats["fps_seen"] == {"60/1": 1, "30/1": 1}
    gate_fps([r.meta for r in scan_dataset(out)])      # 单一帧率, 不报错

    # 反过来指定 30/1 就只收那一个
    out30 = tmp_path / "out30"
    s30 = convert(src, out30, verify=0, fps_num=30, fps_den=1)
    assert s30["chunks_written"] == 1
    assert json.loads(next(out30.rglob("meta.json")).read_text())["video"]["fps_num"] == 30


def test_frame_count_disagreement_is_rejected(tmp_path):
    src = tmp_path / "src"
    write_clip(src, n_frames_override=999)
    stats = convert(src, tmp_path / "out", verify=0)
    assert stats["chunks_written"] == 0
    assert stats["rejected"]["frame_count_mismatch"] == 1


def test_frame_index_gap_is_rejected_and_shuffled_input_is_reordered(tmp_path):
    """p0.jsonl 第 i 行 = 视频第 i 帧; 缺号会让整段标签相对画面平移。"""
    anchor = 3_000
    gappy = [_frame(i, anchor=anchor, span=100) for i in (0, 1, 3)]
    src = tmp_path / "src"
    write_clip(src, frames=gappy)
    stats = convert(src, tmp_path / "out", verify=0)
    assert stats["chunks_written"] == 0
    assert stats["rejected"]["frame_index_gap"] == 1

    # 乱序输入不该被当成缺号, 但输出必须按帧号排好
    shuffled = [_frame(i, anchor=anchor, span=100, pdx=i) for i in (2, 0, 1)]
    src2 = tmp_path / "src2"
    write_clip(src2, frames=shuffled)
    out2 = tmp_path / "out2"
    assert convert(src2, out2, verify=0)["chunks_written"] == 1
    rows = [json.loads(x) for x in
            next(out2.rglob("p0.jsonl")).read_text().splitlines()]
    assert [r["frame"] for r in rows] == [0, 1, 2]
    assert [r["pointerDx"] for r in rows] == [0, 1, 2]


# --------------------------------------------------------------------------- #
# 指针重算(本模块的承重逻辑)
# --------------------------------------------------------------------------- #
def test_pointer_raw_is_half_open_sum_per_frame(tmp_path):
    """事件恰好落在 qpc_start 上算本帧, 恰好落在 qpc_end 上算下一帧。"""
    anchor, span = 1_000, 100
    frames = [_frame(i, anchor=anchor, span=span) for i in range(3)]
    events = [
        _move(999, 100, 100, seq=0),      # 首帧之前 —— 丢掉
        _move(anchor, 5, 1, seq=1),       # 恰好 qpc_start(闭) -> frame 0
        _move(anchor + 99, 7, 0, seq=2),  # frame 0
        _move(anchor + 100, 11, 2, seq=3),  # 恰好 frame0 的 qpc_end(开) -> frame 1
        {"seq": 4, "qpc": anchor + 150, "type": "key_down", "vk": 87},  # 非指针
        _move(anchor + 150, 999, 999, absolute=True, seq=5),  # 绝对坐标 -> 跳过
        _move(anchor + 199, 13, 0, seq=6),  # frame 1
        _move(anchor + 200, 17, 3, seq=7),  # frame 2
        _move(anchor + 250, -4, -5, seq=8),  # frame 2
        _move(anchor + 300, 200, 200, seq=9),  # 末帧 qpc_end(开) -> 丢掉
    ]
    raw, skipped, orphaned = raw_pointer_by_frame(events, frames)
    assert raw == {0: (12, 1), 1: (24, 2), 2: (13, -2)}
    assert skipped == 1
    # 首帧之前那一个不算"缝隙"(clip 边界之外的输入是正常的), 帧窗口本身无缝
    assert orphaned == 0

    # 端到端也要一致
    src = tmp_path / "src"
    write_clip(src, frames=frames, events=events)
    out = tmp_path / "out"
    stats = convert(src, out, verify=0)
    rows = [json.loads(x) for x in
            next(out.rglob("p0.jsonl")).read_text().splitlines()]
    assert [(r["pointerRawDx"], r["pointerRawDy"]) for r in rows] == \
        [(12, 1), (24, 2), (13, -2)]
    assert stats["absolute_moves_skipped"] == 1
    # 逐帧事件摘要与半开区间重算对得上 —— 每次转换都重跑一遍帧-事件对齐验证
    assert stats["pointer_digest_mismatch_frames"] == 0


def test_pointer_raw_survives_clamp_where_pointer_dx_saturates(tmp_path):
    """`pointerDx` 被 converter 封顶在 ±200; 重算的那一路必须不被封顶并计数。"""
    anchor, span = 5_000, 100
    frames = [_frame(i, anchor=anchor, span=span, pdx=min(0 if i else 243, 200))
              for i in range(2)]
    events = [_move(anchor + 10, 243, 0, seq=0)]
    src = tmp_path / "src"
    write_clip(src, frames=frames, events=events)
    out = tmp_path / "out"
    stats = convert(src, out, verify=0)

    rows = [json.loads(x) for x in
            next(out.rglob("p0.jsonl")).read_text().splitlines()]
    assert rows[0]["pointerRawDx"] == 243 and rows[0]["pointerDx"] == 200
    assert stats["frames_beyond_clamp"] == 1
    assert stats["max_abs_raw_pointer"] == 243


def test_empty_events_clip_is_kept_and_downweighted(tmp_path):
    """9 字节空 zstd 帧 = 真实操作空窗: 不崩、不剔除、pointerRaw 全 0、打标签降权。"""
    src = tmp_path / "src"
    d = write_clip(src, events=[])
    empty = next(p for p in d.iterdir() if p.name.startswith("events-"))
    assert empty.stat().st_size == 9        # 与真实语料同形
    assert unzstd(empty.read_bytes()) == b""

    out = tmp_path / "out"
    stats = convert(src, out, verify=0)
    assert stats["chunks_written"] == 1
    assert stats["clips_with_empty_events"] == 1
    assert "empty_events" not in stats["rejected"]

    rows = [json.loads(x) for x in
            next(out.rglob("p0.jsonl")).read_text().splitlines()]
    assert all(r["pointerRawDx"] == 0 and r["pointerRawDy"] == 0 for r in rows)
    meta = json.loads(next(out.rglob("meta.json")).read_text())
    assert "empty_events" in meta["tags"]   # 降权信号跟着数据走

    # 想剔除得显式说
    stats2 = convert(src, tmp_path / "out2", verify=0, drop_empty_events=True)
    assert stats2["chunks_written"] == 0 and stats2["rejected"]["empty_events"] == 1


# --------------------------------------------------------------------------- #
# combat.aim mask
# --------------------------------------------------------------------------- #
def test_combat_aim_is_masked_counted_and_reversible(tmp_path):
    anchor = 9_000
    frames = [
        _frame(0, anchor=anchor, actions=["move.forward"]),
        _frame(1, anchor=anchor, actions=["combat.aim", "combat.primary_attack"]),
        _frame(2, anchor=anchor, actions=["combat.aim"]),
    ]
    src = tmp_path / "src"
    write_clip(src, frames=frames)
    out = tmp_path / "out"
    stats = convert(src, out, verify=0)

    rows = [json.loads(x) for x in
            next(out.rglob("p0.jsonl")).read_text().splitlines()]
    assert [r["actions"] for r in rows] == \
        [["move.forward"], ["combat.primary_attack"], []]
    # 可逆: 被剔除的语义逐帧留痕
    assert rows[1]["actions_masked"] == ["combat.aim"]
    assert "actions_masked" not in rows[0]
    assert stats["masked_action_frames"] == 2
    assert stats["masked_action_counts"] == {"combat.aim": 2}
    # 三个月后翻到这批 chunk 的人必须看得见这里动过手
    meta = json.loads(next(out.rglob("meta.json")).read_text())
    assert meta["masked_actions"]["semantics"] == ["combat.aim"]
    assert "toggle-zoom" in meta["masked_actions"]["reason"]

    # 显式关掉 mask 就原样保留
    stats_off = convert(src, tmp_path / "out_off", verify=0, mask_actions=[])
    rows_off = [json.loads(x) for x in
                next((tmp_path / "out_off").rglob("p0.jsonl")).read_text().splitlines()]
    assert rows_off[2]["actions"] == ["combat.aim"]
    assert stats_off["masked_action_frames"] == 0


# --------------------------------------------------------------------------- #
# 完整性校验
# --------------------------------------------------------------------------- #
def test_manifest_hash_mismatch_rejects_the_clip(tmp_path):
    src = tmp_path / "src"
    write_clip(src, clip_id="c-good-0000", segment_index=1)
    write_clip(src, clip_id="c-bad-0000", segment_index=2,
               corrupt_manifest_for="frames.jsonl.zst")
    stats = convert(src, tmp_path / "out", verify=0)
    assert stats["chunks_written"] == 1
    assert stats["rejected"]["hash_mismatch"] == 1


def test_tampered_payload_is_rejected_even_with_consistent_manifest(tmp_path):
    """改了正文但没改文件名 → 内容寻址对不上, 必须拒收。"""
    src = tmp_path / "src"
    d = write_clip(src)
    frames_file = next(p for p in d.iterdir() if p.name.startswith("frames-"))
    frames_file.write_bytes(_zstd(_jsonl_blob([_frame(0, anchor=1)])))
    stats = convert(src, tmp_path / "out", verify=0)
    assert stats["chunks_written"] == 0
    assert stats["rejected"]["hash_mismatch"] == 1


def test_video_sha256_is_verified_within_budget(tmp_path):
    src = tmp_path / "src"
    d = write_clip(src)
    stats = convert(src, tmp_path / "out", verify=1)
    assert stats["sha256_verified"]["video"] == 1
    assert stats["sha256_verified"]["meta"] == 1

    video = next(p for p in d.iterdir() if p.name.startswith("video-"))
    video.write_bytes(b"tampered")
    bad = convert(src, tmp_path / "out2", verify=1)
    assert bad["chunks_written"] == 0 and bad["rejected"]["hash_mismatch"] == 1
    # verify=0 时本地镜像的 video 不重算 —— 抽验预算是显式的, 不是偷偷全验
    lax = convert(src, tmp_path / "out3", verify=0)
    assert lax["chunks_written"] == 1 and "video" not in lax["sha256_verified"]


def test_incomplete_clip_is_rejected(tmp_path):
    src = tmp_path / "src"
    d = write_clip(src)
    next(p for p in d.iterdir() if p.name.startswith("video-")).unlink()
    stats = convert(src, tmp_path / "out", verify=0)
    assert stats["chunks_written"] == 0 and stats["rejected"]["incomplete_clip"] == 1


# --------------------------------------------------------------------------- #
# 断点续跑 / 取样
# --------------------------------------------------------------------------- #
def test_resume_skips_already_converted_chunks(tmp_path):
    src = tmp_path / "src"
    write_clip(src, clip_id="c-1", segment_index=1)
    write_clip(src, clip_id="c-2", segment_index=2)
    out = tmp_path / "out"
    first = convert(src, out, verify=0)
    assert first["chunks_written"] == 2 and first["chunks_skipped_existing"] == 0

    again = convert(src, out, verify=0)
    assert again["chunks_written"] == 0 and again["chunks_skipped_existing"] == 2
    # 续跑时小时数仍是语料级总量, 不会因为"这次没写"而缩水
    assert again["frames"] == first["frames"]
    assert again["hours"] == first["hours"]

    forced = convert(src, out, verify=0, overwrite=True)
    assert forced["chunks_written"] == 2


def test_session_and_per_session_limits(tmp_path):
    src = tmp_path / "src"
    for i in range(3):
        write_clip(src, clip_id=f"a-{i}", segment_index=i)
    write_clip(src, session="99999999-0000-0000-0000-000000000000",
               clip_id="b-0", segment_index=0)
    stats = convert(src, tmp_path / "out", verify=0, max_clips_per_session=2)
    assert stats["chunks_written"] == 3 and stats["sessions"] == 2

    only = convert(src, tmp_path / "out2", verify=0,
                   sessions=["99999999-0000-0000-0000-000000000000"])
    assert only["chunks_written"] == 1 and only["sessions"] == 1


# --------------------------------------------------------------------------- #
# QC 选片门
# --------------------------------------------------------------------------- #
def test_missing_halo_qc_module_is_a_hard_error_not_a_downgrade(tmp_path, monkeypatch):
    """halo_qc 不可用时, --qc-mode != off 必须明确报错, 不静默降级。"""
    import diogenes_wm
    assert load_qc_gate(None, "off") is None
    with pytest.raises(HaloAdapterError, match="qc-report"):
        load_qc_gate(None, "floor")

    monkeypatch.delattr(diogenes_wm, "halo_qc", raising=False)
    monkeypatch.setitem(sys.modules, "diogenes_wm.halo_qc", None)
    with pytest.raises(HaloAdapterError, match="halo_qc"):
        load_qc_gate(tmp_path / "qc.json", "floor")


class _FakeQC:
    """替身模块: 断言适配器只依赖 `load_report(path)` 这一个约定。"""

    def __init__(self, payload):
        self.payload = payload

    def load_report(self, path):
        assert Path(path).name == "qc.json"
        return self.payload


def _install_fake_qc(monkeypatch, payload):
    import diogenes_wm
    fake = _FakeQC(payload)
    monkeypatch.setitem(sys.modules, "diogenes_wm.halo_qc", fake)
    monkeypatch.setattr(diogenes_wm, "halo_qc", fake, raising=False)


def test_qc_floor_waives_aim_checks_while_strict_does_not(tmp_path, monkeypatch):
    good = "021930b8-3ef9-4ee1-982c-21ab86b9ccb3"
    aim_only = "11111111-1111-1111-1111-111111111111"
    broken = "22222222-2222-2222-2222-222222222222"
    src = tmp_path / "src"
    write_clip(src, session=good, clip_id="c-a")
    write_clip(src, session=aim_only, clip_id="c-b")
    write_clip(src, session=broken, clip_id="c-c")
    # 判据 id 一律用 halo_qc 的词表(CHECK_ORDER) —— 自造别名会让豁免要么落空要么
    # 过头, 而两种失效在裸标签上都看不出来
    _install_fake_qc(monkeypatch, {
        good: {"session_id": good, "failures": []},
        # combat.aim 已被 mask, 拿它毙 session 等于用一个不进模型的通道决定语料存亡
        aim_only: {"session_id": aim_only, "failures": ["aim_share", "toggle_zoom"]},
        broken: {"session_id": broken, "failures": ["action_entropy", "pure_w"]},
    })

    floor = convert(src, tmp_path / "floor", verify=0,
                    qc_report=tmp_path / "qc.json", qc_mode="floor")
    assert floor["chunks_written"] == 2
    assert floor["rejected"]["qc_session_failed"] == 1
    assert floor["qc_sessions_in_report"] == 3

    strict = convert(src, tmp_path / "strict", verify=0,
                     qc_report=tmp_path / "qc.json", qc_mode="strict")
    assert strict["chunks_written"] == 1
    assert strict["rejected"]["qc_session_failed"] == 2


def test_waived_failures_track_halo_qc_aim_checks(tmp_path, monkeypatch):
    """豁免表逐字对齐 halo_qc.AIM_CHECKS —— 尤其 `channel_completeness` 不在里面.

    floor 档并没有关掉通道完整性判据, 只是把必备通道从六个降为五个。把它一并豁免
    会让兜底路径比 floor **更松**: 缺的可能是 move.forward 而不是 combat.aim。
    """
    halo_qc = pytest.importorskip("diogenes_wm.halo_qc")
    assert QC_WAIVED_FAILURES == halo_qc.AIM_CHECKS
    assert "channel_completeness" in halo_qc.FLOOR.enabled_checks
    assert not QC_WAIVED_FAILURES & (set(halo_qc.CHECK_ORDER) - halo_qc.AIM_CHECKS)

    sid = "021930b8-3ef9-4ee1-982c-21ab86b9ccb3"
    src = tmp_path / "src"
    write_clip(src, session=sid)
    _install_fake_qc(monkeypatch,
                     {sid: {"failed_checks": ["channel_completeness"]}})
    stats = convert(src, tmp_path / "out", verify=0,
                    qc_report=tmp_path / "qc.json", qc_mode="floor")
    assert stats["chunks_written"] == 0
    assert stats["rejected"]["qc_session_failed"] == 1


def test_qc_report_with_mode_off_is_a_hard_error(tmp_path, monkeypatch):
    """给了报告又要 off = 报告被完全忽略, 而统计里看不出这件事。"""
    sid = "021930b8-3ef9-4ee1-982c-21ab86b9ccb3"
    src = tmp_path / "src"
    write_clip(src, session=sid)
    _install_fake_qc(monkeypatch, {sid: {"failures": ["pure_w"]}})
    with pytest.raises(HaloAdapterError, match="忽略"):
        convert(src, tmp_path / "out", verify=0,
                qc_report=tmp_path / "qc.json", qc_mode="off")


def test_qc_boolean_verdicts_and_unknown_sessions(tmp_path, monkeypatch):
    sid = "021930b8-3ef9-4ee1-982c-21ab86b9ccb3"
    other = "33333333-3333-3333-3333-333333333333"
    src = tmp_path / "src"
    write_clip(src, session=sid, clip_id="c-a")
    write_clip(src, session=other, clip_id="c-b")
    # 列表 + 显式布尔字段(SessionQC 更可能长这样)
    _install_fake_qc(monkeypatch, [{"session_id": sid, "passes_floor": True}])

    stats = convert(src, tmp_path / "out", verify=0,
                    qc_report=tmp_path / "qc.json", qc_mode="floor")
    assert stats["chunks_written"] == 1
    # 报告里没有的 session 记为未知并拒收, 而不是默认放行
    assert stats["rejected"]["qc_session_unknown"] == 1


def test_qc_report_with_zero_session_overlap_is_a_hard_error(tmp_path, monkeypatch):
    """零交集十有八九是 session 身份对不上(路径 uuid vs 设备本地 id)。"""
    src = tmp_path / "src"
    write_clip(src)
    _install_fake_qc(monkeypatch, {"dev-1785830459723-0009": {"failures": []}})
    with pytest.raises(HaloAdapterError, match="零交集"):
        convert(src, tmp_path / "out", verify=0,
                qc_report=tmp_path / "qc.json", qc_mode="floor")


def test_qc_entry_without_any_verdict_field_raises(tmp_path, monkeypatch):
    src = tmp_path / "src"
    write_clip(src)
    _install_fake_qc(monkeypatch, {"021930b8-3ef9-4ee1-982c-21ab86b9ccb3":
                                   {"action_entropy": 0.64}})
    with pytest.raises(HaloAdapterError, match="判据"):
        convert(src, tmp_path / "out", verify=0,
                qc_report=tmp_path / "qc.json", qc_mode="floor")


# --------------------------------------------------------------------------- #
# 来源闸(contract.SOURCE_RULES, 第二把锁)
# --------------------------------------------------------------------------- #
HALO_URI = ("r2://diogenes-model-training/raw/v1/attested-trainable/"
            "dataminer-api-production/sessions")


def test_source_stamp_is_written_and_survives_gate_sources(tmp_path):
    src = tmp_path / "src"
    write_clip(src)
    out = tmp_path / "out"
    stats = convert(src, out, verify=0, source_uri=HALO_URI,
                    rights="first_party", consent_id="owner-attest-2026-08-01")
    assert stats["source_bucket"] == "diogenes-model-training"

    meta = json.loads(next(out.rglob("meta.json")).read_text())
    # 只用 contract.source_stamp() 那一个写法, 且盖的是该 clip 自己的前缀
    assert meta["source"]["bucket"] == "diogenes-model-training"
    assert meta["source"]["prefix"].endswith(
        "/runs/24e2ae95-6b05-4b98-9d06-ed10c3c97d49/clips/"
        "01d60b75-aaf5-4452-8407-81eeb3814b6f")

    metas = [r.meta for r in scan_dataset(out)]
    assert all(m.source_declared for m in metas)
    # 两把锁并列, 声明来源后都要过
    gate_sources(metas, allow_method_validation_only=False,
                 require_declared_source=True)
    gate_chunks(metas, allow_method_validation_only=False,
                require_declared_source=True)


def test_denied_source_is_refused_before_any_byte_is_read(tmp_path):
    """来源闸挡在下载之前: 连一个 clip 都不该被读, 更不该产出。"""
    src = tmp_path / "src"
    write_clip(src)
    out = tmp_path / "out"
    with pytest.raises(ComplianceError, match="永不可训练"):
        convert(src, out, verify=0,
                source_uri="r2://diogenes-external-datasets/vpt-contractor")
    assert not out.exists() or not any(out.rglob("meta.json"))


def test_method_validation_only_source_blocks_the_attested_lane(tmp_path):
    """具结车道的产物是要进训练的, 所以 method-validation-only 的来源必须拦住。"""
    src = tmp_path / "src"
    write_clip(src)
    uri = "r2://diogenes-model-training/wm01-archive"
    with pytest.raises(ComplianceError, match="method-validation-only"):
        convert(src, tmp_path / "a", verify=0, source_uri=uri,
                rights="first_party", consent_id="owner-attest-2026-08-01")
    # 默认(不可训练)车道本来就是方法验证, 放行
    stats = convert(src, tmp_path / "b", verify=0, source_uri=uri)
    assert stats["chunks_written"] == 1


# --------------------------------------------------------------------------- #
# 与真实 halo_qc 的对接(不是替身)
# --------------------------------------------------------------------------- #
PASS_SID = "aaaaaaaa-0000-4000-8000-000000000001"
FAIL_SID = "bbbbbbbb-0000-4000-8000-000000000002"


def _qc_passing_frames(anchor=70_000):
    """能过 floor 档七项判据的一段: 键覆盖 10、无空帧、指针全非零、五通道齐全。"""
    plan = [
        (87, "move.forward"), (83, "move.backward"), (65, "move.left"),
        (68, "move.right"), (17, "combat.primary_attack"), (16, "move.sprint"),
        (32, "move.jump"), (69, "interact.use"), (82, "equipment.use"),
        (81, "move.forward"),
    ]
    return [_frame(i, anchor=anchor, keys=[vk], actions=[act], pdx=i + 1)
            for i, (vk, act) in enumerate(plan)]


def _qc_failing_frames(anchor=80_000):
    """全空帧 —— empty_frames 判据必挂。"""
    return [_frame(i, anchor=anchor) for i in range(10)]


def _real_report(tmp_path, tier="floor"):
    halo_qc = pytest.importorskip("diogenes_wm.halo_qc")
    results = [
        halo_qc.evaluate_session([_qc_passing_frames()], session_id=PASS_SID,
                                 tier=tier),
        halo_qc.evaluate_session([_qc_failing_frames()], session_id=FAIL_SID,
                                 tier=tier),
    ]
    path = tmp_path / "qc.json"
    halo_qc.dump_report(path, results, tier=tier)
    return halo_qc, path


def test_real_halo_qc_report_drives_session_selection(tmp_path):
    """端到端接真模块: 装配结果必须与 `QCReport.is_passed()` 逐 session 一致。"""
    halo_qc, qc_path = _real_report(tmp_path, tier="floor")
    report = halo_qc.load_report(qc_path)
    assert report.is_passed(PASS_SID) and not report.is_passed(FAIL_SID)

    src = tmp_path / "src"
    write_clip(src, session=PASS_SID, clip_id="c-pass",
               frames=_qc_passing_frames())
    write_clip(src, session=FAIL_SID, clip_id="c-fail",
               frames=_qc_failing_frames())
    out = tmp_path / "out"
    stats = convert(src, out, verify=0, qc_report=qc_path, qc_mode="floor")

    assert stats["chunks_written"] == 1
    assert stats["rejected"]["qc_session_failed"] == 1
    assert {p.name for p in out.iterdir()} == {PASS_SID}
    assert {p.name for p in out.iterdir()} == set(report.passed_session_ids)

    # 不给 QC 就两个都收 —— 门是显式的, 不是默认生效的
    ungated = convert(src, tmp_path / "out_off", verify=0)
    assert ungated["chunks_written"] == 2


def test_qc_tier_mismatch_is_a_hard_error(tmp_path):
    """strict 报告不能当 floor 用: 档位决定哪些判据参与判定, 不能互相换算。"""
    _halo_qc, qc_path = _real_report(tmp_path, tier="strict")
    src = tmp_path / "src"
    write_clip(src, session=PASS_SID, clip_id="c-pass",
               frames=_qc_passing_frames())
    with pytest.raises(HaloAdapterError, match="档"):
        convert(src, tmp_path / "out", verify=0, qc_report=qc_path,
                qc_mode="floor")
    # 按报告自己的档位跑就正常
    stats = convert(src, tmp_path / "out2", verify=0, qc_report=qc_path,
                    qc_mode="strict")
    assert stats["qc_mode"] == "strict"


# --------------------------------------------------------------------------- #
# 授权按作品成立(评审 critical-1)
# --------------------------------------------------------------------------- #
def test_game_id_is_bound_to_the_license_not_free_text(tmp_path):
    """具结车道会盖"halo 合作授权 + 可训练", 而那份授权只覆盖 Halo。

    车道里混着 cod 6 / wolfenstein2 67 / wuchang 7(docs/50 §6 R1–R3), 一个 CLI
    参数就能把它们变成"带微软合作授权的可训练语料" —— 假的授权声明 + 真的可训练戳。
    """
    src = tmp_path / "src"
    write_clip(src, clip_id="c-wolf", game_id="wolfenstein2-steam")
    with pytest.raises(ValueError, match="授权"):
        convert(src, tmp_path / "out", verify=0, game_id="wolfenstein2-steam",
                rights="first_party", consent_id="owner-attest-2026-08-01",
                source_uri=HALO_URI)

    # 默认(不可训练)档不报错, 但标签必须诚实: 不能继续写 halo_partner_licensed
    stats = convert(src, tmp_path / "out2", verify=0,
                    game_id="wolfenstein2-steam")
    assert stats["chunks_written"] == 1
    meta = json.loads(next((tmp_path / "out2").rglob("meta.json")).read_text())
    assert "license_unknown_non_halo_title" in meta["tags"]
    assert "halo_partner_licensed" not in meta["tags"]
    assert meta["is_trainable"] is False


def test_attested_lane_refuses_to_run_without_a_declared_source(tmp_path):
    """具结车道产出可训练字节, 而来源坐标是两把锁里的第二把 —— 不申报就是只上一把。"""
    src = tmp_path / "src"
    write_clip(src)
    with pytest.raises(HaloAdapterError, match="申报来源"):
        convert(src, tmp_path / "out", verify=0, rights="first_party",
                consent_id="owner-attest-2026-08-01")
    # 默认(不可训练)档沿用 fail-open, 不申报也能跑
    assert convert(src, tmp_path / "out2", verify=0)["chunks_written"] == 1


# --------------------------------------------------------------------------- #
# 续跑判的是"同一批字节"**且**"同一套口径"(评审 critical-2 / major)
# --------------------------------------------------------------------------- #
def _lane_of(out: Path) -> dict:
    return json.loads(next(out.rglob("meta.json")).read_text())


def test_resume_reconverts_when_the_compliance_stamp_changed(tmp_path):
    """撤销一份具结之后重跑, 不能变成一次什么都没发生的空跑。"""
    src = tmp_path / "src"
    write_clip(src)
    out = tmp_path / "out"
    first = convert(src, out, verify=0, rights="first_party",
                    consent_id="consent-A", source_uri=HALO_URI)
    assert first["chunks_written"] == 1
    assert _lane_of(out)["is_trainable"] is True

    # 换 consent(旧的被撤销、换发一份新的)
    reissued = convert(src, out, verify=0, rights="first_party",
                       consent_id="consent-B", source_uri=HALO_URI)
    assert reissued["chunks_written"] == 1
    assert reissued["chunks_reconverted_settings_changed"] == 1
    assert "consent_id" in reissued["resume_settings_changed_fields"]
    assert _lane_of(out)["consent_id"] == "consent-B"

    # 降级回不可训练档: 盘上的戳必须真的跟着降
    downgraded = convert(src, out, verify=0)
    assert downgraded["chunks_reconverted_settings_changed"] == 1
    on_disk = _lane_of(out)
    assert on_disk["is_trainable"] is False and on_disk["consent_id"] is None
    assert "trainable" not in on_disk["tags"]
    assert downgraded["is_trainable"] is on_disk["is_trainable"]

    # 口径没变时仍然是干净的跳过(续跑本身没被打坏)
    again = convert(src, out, verify=0)
    assert again["chunks_written"] == 0 and again["chunks_skipped_existing"] == 1
    assert again["chunks_reconverted_settings_changed"] == 0


def test_resume_reconverts_when_mask_changed_and_rewrites_labels(tmp_path):
    """mask 变了逐帧标签也得变 —— 只补 meta 会让 p0.jsonl 与 meta 互相矛盾。"""
    anchor = 9_000
    frames = [_frame(0, anchor=anchor, actions=["combat.aim"]),
              _frame(1, anchor=anchor, actions=["move.forward"])]
    src = tmp_path / "src"
    write_clip(src, frames=frames)
    out = tmp_path / "out"
    convert(src, out, verify=0, mask_actions=[])
    rows = [json.loads(x) for x in
            next(out.rglob("p0.jsonl")).read_text().splitlines()]
    assert rows[0]["actions"] == ["combat.aim"]
    assert "unmasked_combat_aim" in _lane_of(out)["tags"]

    stats = convert(src, out, verify=0)          # 默认 mask combat.aim
    assert stats["chunks_reconverted_settings_changed"] == 1
    assert "masked_actions" in stats["resume_settings_changed_fields"]
    rows = [json.loads(x) for x in
            next(out.rglob("p0.jsonl")).read_text().splitlines()]
    assert rows[0]["actions"] == [] and rows[0]["actions_masked"] == ["combat.aim"]
    assert "unmasked_combat_aim" not in _lane_of(out)["tags"]


# --------------------------------------------------------------------------- #
# 撞名的 chunk 必须连**身份**一起分开(评审 major)
# --------------------------------------------------------------------------- #
def test_colliding_chunk_names_get_distinct_sample_keys(tmp_path):
    """目录分开而 sample_key 还撞着, 等于没分 —— 下游全按 sample_key 对齐。"""
    src = tmp_path / "src"
    write_clip(src, clip_id="aaaaaaaa-0000-0000-0000-000000000000", segment_index=7)
    write_clip(src, clip_id="bbbbbbbb-0000-0000-0000-000000000000", segment_index=7,
               run_uuid="99999999-6b05-4b98-9d06-ed10c3c97d49")
    out = tmp_path / "out"
    stats = convert(src, out, verify=0)
    assert stats["chunks_written"] == 2 and stats["chunk_key_collisions"] == 1

    metas = [json.loads(p.read_text()) for p in sorted(out.rglob("meta.json"))]
    assert len({m["sample_key"] for m in metas}) == 2
    assert len({m["chunk"] for m in metas}) == 2
    # chunk 身份与落盘的目录名一致
    for p in sorted(out.rglob("meta.json")):
        assert json.loads(p.read_text())["chunk"] == p.parent.name
    assert len({r.meta.sample_key for r in scan_dataset(out)}) == 2


# --------------------------------------------------------------------------- #
# 坏正文只毙这一个 clip, 不毙整趟跑(评审 major)
# --------------------------------------------------------------------------- #
def _rewrite_content_addressed(d: Path, stem: str, blob: bytes) -> None:
    """把某个逻辑文件换成新内容, 并把**三份**哈希(文件名 / manifest / meta)一并改对.

    这样它能过全部完整性校验, 打的是**解析**那一步。
    """
    old = next(p for p in d.iterdir() if p.name.startswith(f"{stem}-"))
    ext = old.name.split("-", 1)[1].split(".", 1)[1]
    old.unlink()
    (d / f"{stem}-{_sha(blob)}.{ext}").write_bytes(blob)
    logical = f"{stem}.{ext}"

    mtp = next(p for p in d.iterdir() if p.name.startswith("meta-"))
    meta = json.loads(mtp.read_bytes())
    if stem in meta and isinstance(meta[stem], dict):
        meta[stem]["sha256"] = f"sha256:{_sha(blob)}"
    mtp.unlink()
    meta_blob = json.dumps(meta, indent=2, sort_keys=True).encode()
    (d / f"meta-{_sha(meta_blob)}.json").write_bytes(meta_blob)

    mp = next(p for p in d.iterdir() if p.name.startswith("manifest-"))
    manifest = json.loads(mp.read_bytes())
    manifest[logical] = f"sha256:{_sha(blob)}"
    manifest["meta.json"] = f"sha256:{_sha(meta_blob)}"
    mp.unlink()
    mb = json.dumps(manifest, indent=2, sort_keys=True).encode()
    (d / f"manifest-{_sha(mb)}.json").write_bytes(mb)


@pytest.mark.parametrize("blob,reason", [
    (None, "unreadable_payload"),                    # 半行 JSON
    (b"\xff\xfe\x00garbage", "unreadable_payload"),  # 不是 UTF-8
])
def test_corrupt_frames_payload_is_a_counted_rejection(tmp_path, blob, reason):
    src = tmp_path / "src"
    write_clip(src, clip_id="c-good", segment_index=1)
    bad = write_clip(src, clip_id="c-bad", segment_index=2)
    payload = blob if blob is not None else b'{"frame": 0, oops\n'
    _rewrite_content_addressed(bad, "frames", _zstd(payload))

    stats = convert(src, tmp_path / "out", verify=0)
    # 好的那个照常产出 —— 一个坏对象不该让 8,663 clip 的一趟跑白费
    assert stats["chunks_written"] == 1
    assert stats["rejected"][reason] == 1


def test_corrupt_zstd_frame_is_a_counted_rejection(tmp_path):
    src = tmp_path / "src"
    write_clip(src, clip_id="c-good", segment_index=1)
    bad = write_clip(src, clip_id="c-bad", segment_index=2)
    _rewrite_content_addressed(bad, "frames", b"not-a-zstd-frame-at-all")
    stats = convert(src, tmp_path / "out", verify=0)
    assert stats["chunks_written"] == 1
    assert stats["rejected"]["unreadable_payload"] == 1


def test_manifest_that_is_not_an_object_is_a_counted_rejection(tmp_path):
    src = tmp_path / "src"
    write_clip(src, clip_id="c-good", segment_index=1)
    bad = write_clip(src, clip_id="c-bad", segment_index=2)
    mp = next(p for p in bad.iterdir() if p.name.startswith("manifest-"))
    mp.unlink()
    blob = json.dumps(["meta.json", "video.mp4"]).encode()
    (bad / f"manifest-{_sha(blob)}.json").write_bytes(blob)
    stats = convert(src, tmp_path / "out", verify=0)
    assert stats["chunks_written"] == 1
    assert stats["rejected"]["unreadable_manifest"] == 1


def test_frames_row_missing_a_load_bearing_field_is_a_rejection(tmp_path):
    """缺 output_qpc_start 的一行会让指针重算 KeyError —— 那是拒收, 不是崩。"""
    src = tmp_path / "src"
    write_clip(src, clip_id="c-good", segment_index=1)
    rows = [_frame(i, anchor=4_000, span=100) for i in range(3)]
    bad = write_clip(src, clip_id="c-bad", segment_index=2, frames=rows)
    del rows[1]["output_qpc_start"]      # 帧数不变, 只是缺了一个承重字段
    _rewrite_content_addressed(bad, "frames", _zstd(_jsonl_blob(rows)))
    stats = convert(src, tmp_path / "out", verify=0)
    assert stats["chunks_written"] == 1
    assert stats["rejected"] == {"unreadable_payload": 1}
    assert "output_qpc_start" in stats["rejected_examples"][0]["detail"]


# --------------------------------------------------------------------------- #
# meta 自己那第三份哈希也要核(评审 major)
# --------------------------------------------------------------------------- #
def test_meta_declared_hashes_are_verified_before_being_copied(tmp_path):
    """provenance 里那几个 sha 正是复核时拿去跟 R2 对账的字段, 不能是抄来的空话。"""
    src = tmp_path / "src"
    d = write_clip(src)
    mp = next(p for p in d.iterdir() if p.name.startswith("meta-"))
    meta = json.loads(mp.read_bytes())
    meta["video"]["sha256"] = "sha256:" + "f" * 64
    mp.unlink()
    blob = json.dumps(meta, indent=2, sort_keys=True).encode()
    (d / f"meta-{_sha(blob)}.json").write_bytes(blob)
    mfp = next(p for p in d.iterdir() if p.name.startswith("manifest-"))
    manifest = json.loads(mfp.read_bytes())
    manifest["meta.json"] = f"sha256:{_sha(blob)}"
    mfp.unlink()
    mb = json.dumps(manifest, indent=2, sort_keys=True).encode()
    (d / f"manifest-{_sha(mb)}.json").write_bytes(mb)

    stats = convert(src, tmp_path / "out", verify=0)
    assert stats["chunks_written"] == 0
    assert stats["rejected"]["hash_mismatch"] == 1
    assert stats["hash_mismatch_by_file"]["meta_declares_other_sha"] == 1


def test_provenance_hashes_are_all_bare_hex(tmp_path):
    """同一个块里两种写法, 会让按哈希 join 目录的人先猜格式。"""
    src = tmp_path / "src"
    write_clip(src)
    out = tmp_path / "out"
    convert(src, out, verify=0)
    prov = json.loads(next(out.rglob("meta.json")).read_text())["provenance"]
    for key in ("video_sha256", "frames_sha256", "events_sha256", "audio_sha256",
                "catalog_content_hash", "mapping_content_hash", "executable_sha256"):
        assert prov[key] and not prov[key].startswith("sha256:"), key


# --------------------------------------------------------------------------- #
# 重复上传 / 拒收身份 / 工作量上界(评审 major + minor)
# --------------------------------------------------------------------------- #
def test_double_uploaded_clip_is_rejected_not_decided_by_listing_order(tmp_path):
    """内容寻址下一次重传会留下两代; 绑哪一代取决于列举顺序 —— 那不能作为判据。"""
    src = tmp_path / "src"
    d = write_clip(src)
    other = _zstd(_jsonl_blob([_frame(0, anchor=777)]))
    (d / f"frames-{_sha(other)}.jsonl.zst").write_bytes(other)
    stats = convert(src, tmp_path / "out", verify=0)
    assert stats["chunks_written"] == 0
    assert stats["rejected"]["ambiguous_clip_files"] == 1
    assert stats["rejected_examples"][0]["files"] == ["frames.jsonl.zst"]


def test_rejections_carry_clip_identity(tmp_path):
    """聚合计数回答不了"是哪 98 个", 也就交不出去复核。"""
    src = tmp_path / "src"
    write_clip(src, clip_id="c-wolf", game_id="wolfenstein2-steam")
    log = tmp_path / "rejects.jsonl"
    stats = convert(src, tmp_path / "out", verify=0, rejects_jsonl=log)
    assert stats["rejected"]["game_id_mismatch"] == 1
    entry = stats["rejected_examples"][0]
    assert entry["clip_id"] == "c-wolf" and entry["game_id"] == "wolfenstein2-steam"
    assert entry["session"] and entry["run"]
    assert json.loads(log.read_text().splitlines()[0]) == entry


def test_max_examined_bounds_work_while_limit_bounds_output(tmp_path):
    """混车道上 --limit 5 会为了凑够五个产出把整条车道读一遍。"""
    src = tmp_path / "src"
    for i in range(5):
        write_clip(src, clip_id=f"a-wolf-{i}", segment_index=i,
                   game_id="wolfenstein2-steam")
    write_clip(src, clip_id="z-halo", segment_index=9)

    out = convert(src, tmp_path / "out", verify=0, limit=1)
    assert out["chunks_written"] == 1 and out["rejected"]["game_id_mismatch"] == 5
    assert out["clips_examined"] == 6

    capped = convert(src, tmp_path / "out2", verify=0, max_examined=2)
    assert capped["clips_examined"] == 2 and capped["chunks_written"] == 0


def test_pointer_gap_between_frame_windows_is_counted(tmp_path):
    """"帧窗口首尾相接"是承重前提, 破了要数出来 —— 摘要对账兜不住它。"""
    anchor, span = 3_000, 100
    frames = [_frame(0, anchor=anchor, span=span),
              _frame(1, anchor=anchor, span=span),
              _frame(2, anchor=anchor, span=span)]
    frames[1]["output_qpc_start"] += 50        # 挖一条 50 tick 的缝
    events = [_move(anchor + 120, 77, 0, seq=0)]   # 位移正好落在缝里
    raw, _skipped, orphaned = raw_pointer_by_frame(events, frames)
    assert raw == {0: (0, 0), 1: (0, 0), 2: (0, 0)}   # 77 从每一帧的和里消失了
    assert orphaned == 1

    src = tmp_path / "src"
    write_clip(src, frames=frames, events=events)
    stats = convert(src, tmp_path / "out", verify=0)
    assert stats["pointer_moves_outside_frames"] == 1
    assert stats["clips_with_pointer_gaps"] == 1


# --------------------------------------------------------------------------- #
# R2 路径: 缓存中毒自愈 + 并发预热(评审 major)
# --------------------------------------------------------------------------- #
class _FakeS3:
    """R2 的最小替身: 只有 list_objects_v2 / download_file / get_object。

    走的是 `R2Source` 的**真实**代码路径(内容寻址索引、cache 落盘、evict、来源戳),
    换掉的只是网络。
    """

    def __init__(self, objects: dict[str, bytes]):
        self.objects = dict(objects)
        self.downloads: dict[str, int] = {}

    def list_objects_v2(self, **kw):
        prefix = kw.get("Prefix") or ""
        return {"Contents": [{"Key": k, "Size": len(v)}
                             for k, v in sorted(self.objects.items())
                             if k.startswith(prefix)],
                "IsTruncated": False}

    def download_file(self, bucket, key, dest):
        self.downloads[key] = self.downloads.get(key, 0) + 1
        Path(dest).write_bytes(self.objects[key])

    def get_object(self, Bucket, Key):  # noqa: N803 - boto3 的签名
        return {"Body": io.BytesIO(self.objects[Key])}


def _fake_r2(monkeypatch, src_root: Path):
    from diogenes_wm import halo_adapter as ha
    prefix = ("raw/v1/attested-trainable/dataminer-api-production/sessions/")
    objects = {prefix + str(p.relative_to(src_root)): p.read_bytes()
               for p in sorted(src_root.rglob("*")) if p.is_file()}
    source = ha.R2Source("diogenes-model-training", prefix)
    client = _FakeS3(objects)
    source._client = client
    monkeypatch.setattr(ha, "build_source", lambda src, **kw: source)
    return client


def test_poisoned_cache_is_evicted_and_refetched(tmp_path, monkeypatch):
    """缓存命中只看文件大小 —— 一份大小相同、内容坏掉的缓存会让该 clip 此后每次都被丢掉。"""
    src = tmp_path / "src"
    write_clip(src)
    client = _fake_r2(monkeypatch, src)
    cache, out = tmp_path / "cache", tmp_path / "out"
    first = convert("r2://x/y", out, cache=cache, verify=-1)
    assert first["chunks_written"] == 1 and not first["rejected"]

    poisoned = next(cache.rglob("frames-*.jsonl.zst"))
    raw = bytearray(poisoned.read_bytes())
    raw[len(raw) // 2] ^= 0xFF                    # 大小不变, 内容坏掉
    poisoned.write_bytes(bytes(raw))
    before = dict(client.downloads)

    healed = convert("r2://x/y", out, cache=cache, verify=-1, overwrite=True)
    assert healed["chunks_written"] == 1 and not healed["rejected"]
    assert healed["cache_evicted"] == 1
    assert client.downloads != before             # 真的重下了一次


def test_r2_lane_stamps_source_and_prefetch_matches_serial(tmp_path, monkeypatch):
    """`--jobs` 只是预热: 同一批语料, 并发跑与串行跑必须给出同一份产物。"""
    src = tmp_path / "src"
    for i in range(3):
        write_clip(src, clip_id=f"c-{i}", segment_index=i)
    write_clip(src, clip_id="c-wolf", segment_index=8, game_id="wolfenstein2-steam")
    _fake_r2(monkeypatch, src)

    serial = convert("r2://x/y", tmp_path / "a", cache=tmp_path / "ca", verify=-1,
                     rights="first_party", consent_id="owner-attest-2026-08-01")
    parallel = convert("r2://x/y", tmp_path / "b", cache=tmp_path / "cb", verify=-1,
                       rights="first_party", consent_id="owner-attest-2026-08-01",
                       jobs=4, prefetch_batch=2)
    for key in ("chunks_written", "frames", "hours", "rejected", "sha256_verified"):
        assert serial[key] == parallel[key], key
    assert serial["chunks_written"] == 3 and serial["prefetch_errors"] == 0

    def _bodies(root):
        return sorted((p.parent.name, p.read_text())
                      for p in root.rglob("p0.jsonl"))
    assert _bodies(tmp_path / "a") == _bodies(tmp_path / "b")

    metas = [r.meta for r in scan_dataset(tmp_path / "b")]
    assert all(m.source_bucket == "diogenes-model-training" for m in metas)
    gate_chunks(metas, allow_method_validation_only=False,
                require_declared_source=True)
    # R2 模式的来源坐标是列桶列出来的事实, 不是操作者断言的
    meta = json.loads(next((tmp_path / "b").rglob("meta.json")).read_text())
    assert meta["provenance"]["source_coordinate"].startswith("observed")


def test_prefetch_respects_the_same_selection_bounds_as_the_loop(tmp_path, monkeypatch):
    """预热花的是出口流量, 所以它必须先跑主循环那几道不花 I/O 的选片.

    否则 `--max-clips-per-session 1` 的冒烟跑会先把整整一批 clip 的视频拉下来,
    QC floor 毙掉的 session 也会被照拉不误("不过闸的一个字节都不花"就没了)。
    """
    src = tmp_path / "src"
    keep = "aaaa0000-0000-4000-8000-000000000001"
    drop = "bbbb0000-0000-4000-8000-000000000002"
    for i in range(3):
        write_clip(src, session=keep, clip_id=f"k-{i}", segment_index=i)
        write_clip(src, session=drop, clip_id=f"d-{i}", segment_index=i)
    client = _fake_r2(monkeypatch, src)
    _install_fake_qc(monkeypatch, {keep: {"failures": []},
                                   drop: {"failures": ["pure_w"]}})

    # ① QC 毙掉的 session: 一个正文对象都不该被拉
    convert("r2://x/y", tmp_path / "out", cache=tmp_path / "cache", verify=0,
            jobs=4, prefetch_batch=32,
            qc_report=tmp_path / "qc.json", qc_mode="floor")
    assert not any(f"/{drop}/" in k and "video-" in k for k in client.downloads)
    assert any(f"/{keep}/" in k and "video-" in k for k in client.downloads)

    # ② 每 session 上限: 拉的视频数不该超过要转的 clip 数
    client.downloads.clear()
    stats = convert("r2://x/y", tmp_path / "out2", cache=tmp_path / "cache2",
                    verify=0, jobs=4, prefetch_batch=32, max_clips_per_session=1)
    assert stats["chunks_written"] == 2
    assert sum(1 for k in client.downloads if "video-" in k) == 2

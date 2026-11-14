"""D2E conversion has to preserve the two things we mirrored it for.

The corpus was mirrored because it carries raw HID pointer deltas and 30
games' worth of VK keyboard state — the two fields gaming-500 cannot give.
A converter that quietly clamps the deltas, mis-assigns them by a frame, or
writes the wrong fps would hand the IDM line the same lossy labels it already
has while looking like new evidence.

The fixtures here are a real OWAMcap file and a real video written on the
fly, so the parse path is the shipped one — no mocks, no network.
"""
import json

import numpy as np
import pytest

# 与仓库既有做法一致（test_reencode / test_video_codec 用同一模式）：可选依赖缺失
# 就跳过而不是让整套红。CI 现在装 data extra，所以这两个守卫在 CI 上不会生效。
pytest.importorskip("mcap")
pytest.importorskip("cv2")

from diogenes_phase0.data.d2e import build_rows, convert_pair, measured_fps, read_events, session_of
from diogenes_phase0.data.dataset import scan_dataset
from diogenes_phase0.keys import MOUSE_CLAMP, foveated_bin

SCHEMAS = {
    "screen": ("desktop/ScreenCaptured", ["utc_ns", "media_ref"]),
    "mouse/raw": ("desktop/RawMouseEvent", ["last_x", "last_y", "button_flags"]),
    "keyboard": ("desktop/KeyboardEvent", ["event_type", "vk"]),
    "keyboard/state": ("desktop/KeyboardState", ["buttons"]),
    "mouse/state": ("desktop/MouseState", ["x", "y", "buttons"]),
}


def write_mcap(path, msgs, *, uri="clip.mkv"):
    """msgs: list of (log_time_ns, topic, body dict)."""
    from mcap.writer import Writer

    with open(path, "wb") as f:
        w = Writer(f)
        w.start()
        chans = {}
        for topic, (name, props) in SCHEMAS.items():
            sid = w.register_schema(
                name=name, encoding="jsonschema",
                data=json.dumps({"type": "object",
                                 "properties": {p: {} for p in props}}).encode())
            chans[topic] = w.register_channel(topic=topic, message_encoding="json",
                                              schema_id=sid)
        for t, topic, body in sorted(msgs, key=lambda m: m[0]):
            w.add_message(channel_id=chans[topic], log_time=t, publish_time=t,
                          data=json.dumps(body).encode())
        w.finish()
    return path


def frame_msgs(n, period_ns, start=0):
    return [(start + i * period_ns, "screen",
             {"utc_ns": start + i * period_ns,
              "media_ref": {"uri": "clip.mkv", "pts_ns": start + i * period_ns}})
            for i in range(n)]


def write_mkv(path, n, size=(64, 64)):
    import cv2

    vw = cv2.VideoWriter(str(path), cv2.VideoWriter_fourcc(*"mp4v"), 30.0, size)
    rng = np.random.default_rng(0)
    for _ in range(n):
        vw.write(rng.integers(0, 255, (size[1], size[0], 3), dtype=np.uint8))
    vw.release()
    return path


# --------------------------------------------------------------------------
def test_measured_fps_is_read_from_the_stream_not_assumed(tmp_path):
    """gaming500 的适配器写死 30.0；D2E 是变帧率，门要的是实测值。"""
    p = write_mcap(tmp_path / "a.mcap", frame_msgs(10, 16_666_667))
    ev = read_events(p)
    assert measured_fps(ev["frames"]) == pytest.approx(60.0, rel=1e-3)
    p30 = write_mcap(tmp_path / "b.mcap", frame_msgs(10, 33_333_333))
    assert measured_fps(read_events(p30)["frames"]) == pytest.approx(30.0, rel=1e-3)


def test_a_stalled_recording_reports_delivered_rate_not_the_flattering_median(tmp_path):
    """掉帧时中位数会高估。写进 video.fps 的必须是「实际交付了多少帧」。"""
    from diogenes_phase0.data.d2e import median_gap_fps

    per = 16_666_667                                   # 60 fps 稳态
    times = [i * per for i in range(20)]
    times += [times[-1] + 20 * per]                    # 中间卡了一下
    times += [times[-1] + i * per for i in range(1, 20)]
    msgs = [(t, "screen", {"utc_ns": t,
                           "media_ref": {"uri": "clip.mkv", "pts_ns": t}})
            for t in times]
    ev = read_events(write_mcap(tmp_path / "stall.mcap", msgs))
    assert median_gap_fps(ev["frames"]) == pytest.approx(60.0, rel=1e-3)
    assert measured_fps(ev["frames"]) < 55.0


def test_raw_deltas_are_summed_per_frame_and_kept_unclamped(tmp_path):
    """A flick past ±200 must survive in pointerRawDx while pointerDx clamps."""
    per = 16_666_667
    msgs = frame_msgs(3, per)
    # frame 1 window is (t0, t1]: three reports summing to 900
    msgs += [(per - 3, "mouse/raw", {"last_x": 300, "last_y": -10,
                                     "button_flags": 0}),
             (per - 2, "mouse/raw", {"last_x": 300, "last_y": -10,
                                     "button_flags": 0}),
             (per - 1, "mouse/raw", {"last_x": 300, "last_y": -10,
                                     "button_flags": 0})]
    rows = build_rows(read_events(write_mcap(tmp_path / "c.mcap", msgs)))
    assert len(rows) == 3
    assert rows[1]["pointerRawDx"] == 900
    assert rows[1]["pointerDx"] == int(MOUSE_CLAMP)
    assert rows[1]["pointerRawDy"] == -30
    # bins derive from the clamped value, as in the storage contract
    assert rows[1]["pointerBins"] == [foveated_bin(MOUSE_CLAMP),
                                      foveated_bin(-30.0)]
    # and the deltas belong to frame 1 only
    assert rows[0]["pointerRawDx"] == 0 and rows[2]["pointerRawDx"] == 0


def test_periodic_state_overrides_a_hold_that_started_before_the_split(tmp_path):
    """D2E ships `_split_NN` files; a key held in from the previous split has
    no press event here, and a missed release would stick forever."""
    per = 16_666_667
    msgs = frame_msgs(4, per)
    msgs += [(1, "keyboard/state", {"buttons": [87]}),          # W already held
             (per * 2 + 1, "keyboard", {"event_type": "release", "vk": 87})]
    rows = build_rows(read_events(write_mcap(tmp_path / "d.mcap", msgs)))
    assert rows[0]["keys"] == [87] and rows[1]["keys"] == [87]
    assert rows[3]["keys"] == []           # release seen before frame 3


def test_mouse_button_flags_become_contract_vks(tmp_path):
    per = 16_666_667
    msgs = frame_msgs(3, per)
    msgs += [(per - 1, "mouse/raw", {"last_x": 0, "last_y": 0,
                                     "button_flags": 0x0001}),   # left down
             (per * 2 - 1, "mouse/raw", {"last_x": 0, "last_y": 0,
                                         "button_flags": 0x0002})]  # left up
    rows = build_rows(read_events(write_mcap(tmp_path / "e.mcap", msgs)))
    assert rows[1]["keys"] == [1]
    assert rows[2]["keys"] == []


def test_resample_puts_output_on_a_wall_clock_grid_and_conserves_motion(tmp_path):
    """整数抽帧做不到的事：源是变帧率时，只有按时间戳建格才有一致的动作语义。"""
    from diogenes_phase0.data.d2e import plan_resample

    per = 16_666_667                                   # 60 fps 源
    n = 24
    msgs = frame_msgs(n, per)
    for i in range(n):
        msgs.append((i * per + 1, "mouse/raw",
                     {"last_x": 10, "last_y": 0, "button_flags": 0}))
    ev = read_events(write_mcap(tmp_path / "rs.mcap", msgs))

    native = build_rows(ev)
    out_times, idx = plan_resample(ev, 30.0)
    half = build_rows(ev, out_times=out_times)

    assert len(half) == pytest.approx(len(native) / 2, abs=1)
    # 运动守恒：覆盖窗口内的每个事件恰好落进一行（不是隔帧丢弃）。
    # 网格最后一点之后的尾巴会被丢掉——与 chunk_frames 丢零头同类，是诚实行为，
    # 所以只对覆盖窗口内求和。
    covered = sum(dx for t, dx, _dy, _f in ev["raw"] if t <= out_times[-1])
    assert sum(r["pointerRawDx"] for r in half) == covered
    # 输出网格是等间隔的墙钟时间，源帧下标严格递增（不重复用同一帧）
    steps = {b - a for a, b in zip(out_times, out_times[1:])}
    assert max(steps) - min(steps) <= 1
    assert all(b > a for a, b in zip(idx, idx[1:]))


def test_a_target_above_the_delivered_rate_is_refused(tmp_path):
    """21 fps 的录制重采到 30 fps 会把重复帧当成观测——必须拒绝。

    拒绝发生在 convert_pair：重复 tick 均匀铺满时没有任何干净 chunk 幸存。
    plan_resample 本身不再一票否决——那个否决曾让一个掉帧就吞掉整段录制
    （首轮 D-0 因此丢了 13 段里的 12 段）。"""
    per = 47_000_000                                   # ~21.3 fps
    n = 20
    mcap = write_mcap(tmp_path / "slow.mcap", frame_msgs(n, per))
    write_mkv(tmp_path / "slow.mkv", n)
    with pytest.raises(SystemExit, match="duplicated frame"):
        convert_pair(mcap, tmp_path / "slow.mkv", tmp_path / "out",
                     game="Slow", res=(64, 64), chunk_frames=4, codec="npz",
                     target_fps=30.0)


def test_a_stall_drops_only_the_chunks_it_poisons(tmp_path):
    """录制中途卡一下，只有卡顿落进的 chunk 该被丢——不是整段录制。

    30 fps 稳态源，中间一次 5 帧的空洞：网格在空洞里重复使用最后一帧,
    那几个 tick 落进同一个 chunk。其余 chunk 每帧都是新观测，必须保留。"""
    per = 33_333_333                                   # 30 fps 稳态
    times = [i * per for i in range(12)]
    times += [times[-1] + 5 * per + i * per for i in range(12)]
    msgs = [(t, "screen", {"utc_ns": t,
                           "media_ref": {"uri": "clip.mkv", "pts_ns": t}})
            for t in times]
    mcap = write_mcap(tmp_path / "stall.mcap", msgs)
    write_mkv(tmp_path / "stall.mkv", len(times))

    out = tmp_path / "out"
    got = convert_pair(mcap, tmp_path / "stall.mkv", out, game="Stall",
                       res=(64, 64), chunk_frames=4, codec="npz",
                       target_fps=30.0)
    # 27 个 tick → 6 个整 chunk；空洞的 4 个重复 tick 全落在 3 号 chunk。
    assert got == 5
    recs = scan_dataset(out)
    assert len(recs) == 5
    sess = json.loads((recs[0].chunk_dir.parent / "metadata.json").read_text())
    assert sess["dup_ticks"] == 4                  # 空洞里的重复 tick
    assert sess["stall_chunks_dropped"] == 1       # 只丢它落进的那个 chunk
    # 幸存 chunk 连续编号，读取侧不感知空洞
    chunks = sorted(json.loads((r.chunk_dir / "meta.json").read_text())["chunk"]
                    for r in recs)
    assert chunks == list(range(5))


def test_convert_writes_true_fps_and_compliance_stamps(tmp_path):
    per = 16_666_667                                    # 60 fps
    n = 8
    msgs = frame_msgs(n, per)
    msgs += [(i * per + 1, "mouse/raw",
              {"last_x": 5, "last_y": -5, "button_flags": 0}) for i in range(n)]
    mcap = write_mcap(tmp_path / "clip.mcap", msgs)
    write_mkv(tmp_path / "clip.mkv", n)

    out = tmp_path / "out"
    got = convert_pair(mcap, tmp_path / "clip.mkv", out, game="Test_Game",
                       res=(64, 64), chunk_frames=4, codec="npz")
    assert got == 2
    recs = scan_dataset(out)
    assert len(recs) == 2
    meta = json.loads((recs[0].chunk_dir / "meta.json").read_text())
    assert meta["video"]["fps"] == pytest.approx(60.0, rel=1e-3)
    assert meta["is_trainable"] is False
    assert "method_validation_only" in meta["tags"]
    assert "license:cc-by-nc-4.0" in meta["tags"]
    assert meta["pointer_delta_quality"] == "raw_hid"
    assert "W/A/S/D=87/65/83/68" in meta["key_schema"]
    # the loader must refuse it without the explicit flag
    from diogenes_phase0.contract import ComplianceError, gate_phase0_contract
    with pytest.raises(ComplianceError):
        gate_phase0_contract([r.meta for r in recs], contract_mode="legacy")
    gate_phase0_contract(
        [r.meta for r in recs], contract_mode="method-validation")


def test_convert_with_target_fps_writes_the_grid_rate_and_a_provenance_tag(tmp_path):
    per = 16_666_667                                   # 60 fps 源
    n = 24
    msgs = frame_msgs(n, per)
    msgs += [(i * per + 1, "mouse/raw",
              {"last_x": 5, "last_y": 0, "button_flags": 0}) for i in range(n)]
    mcap = write_mcap(tmp_path / "g.mcap", msgs)
    write_mkv(tmp_path / "g.mkv", n)
    out = tmp_path / "o2"
    got = convert_pair(mcap, tmp_path / "g.mkv", out, game="G", res=(64, 64),
                       chunk_frames=4, codec="npz", target_fps=30.0)
    assert got >= 2
    recs = scan_dataset(out)
    meta = json.loads((recs[0].chunk_dir / "meta.json").read_text())
    assert meta["video"]["fps"] == pytest.approx(30.0)
    assert any(t.startswith("resampled:30fps_from_") for t in meta["tags"])


def test_a_recording_too_slow_for_the_grid_is_skipped_not_fatal(tmp_path):
    """变帧率语料里一段慢录制不该否决其余 30 款游戏——但也不能静默消失。"""
    from diogenes_phase0.data.d2e import convert

    raw = tmp_path / "raw"
    for name, per, n in [("Fast", 16_666_667, 16), ("Slow", 47_000_000, 16)]:
        d = raw / name
        d.mkdir(parents=True)
        msgs = frame_msgs(n, per)
        msgs += [(i * per + 1, "mouse/raw",
                  {"last_x": 4, "last_y": 0, "button_flags": 0}) for i in range(n)]
        write_mcap(d / "clip.mcap", msgs)
        write_mkv(d / "clip.mkv", n)

    rep = convert(raw, tmp_path / "out", res=(64, 64), chunk_frames=4,
                  codec="npz", target_fps=30.0)
    assert rep["chunks"] > 0                       # 快的那段照转
    assert len(rep["skipped"]) == 1                # 慢的那段被跳过
    name, why = rep["skipped"][0]
    assert "Slow" in name and "duplicated frame" in why
    games = {json.loads((r.chunk_dir / "meta.json").read_text())["game"]
             for r in scan_dataset(tmp_path / "out")}
    assert games == {"Fast"}


def test_sparse_pointer_channel_is_tagged_not_silently_zero(tmp_path):
    """键盘类游戏没有指针通道。把它们的零平均进鼠标 R²，量的是语料空闲度不是模型。"""
    from diogenes_phase0.data.d2e import MIN_POINTER_HZ, convert

    raw = tmp_path / "raw"
    per = 33_333_333                                   # 30 fps
    n = 60                                             # 2 秒——太短的片段算不出「稀疏」
    # Dense：每帧一条（≈30 Hz）；Sparse：整段 1 条（0.5 Hz，低于 1 Hz 阈值）
    for name, n_ptr in [("Apex_Legends", n), ("Stardew_Valley", 1)]:
        d = raw / name
        d.mkdir(parents=True)
        msgs = frame_msgs(n, per)
        msgs += [(i * per + 1, "mouse/raw",
                  {"last_x": 7, "last_y": 0, "button_flags": 0})
                 for i in range(n_ptr)]
        write_mcap(d / "clip.mcap", msgs)
        write_mkv(d / "clip.mkv", n)

    rep = convert(raw, tmp_path / "out", res=(64, 64), chunk_frames=4,
                  codec="npz")
    tagged = {}
    for r in scan_dataset(tmp_path / "out"):
        m = json.loads((r.chunk_dir / "meta.json").read_text())
        tagged[m["game"]] = ("pointer:absent_or_sparse" in m["tags"],
                             m["pointer_event_hz"])
    assert tagged["Apex_Legends"][0] is False
    assert tagged["Stardew_Valley"][0] is True
    assert tagged["Apex_Legends"][1] > MIN_POINTER_HZ
    assert tagged["Stardew_Valley"][1] < MIN_POINTER_HZ
    # 名单必须可被调用方拿到——静默打标等于没打
    assert [n for n, _ in rep["sparse"]] == ["Stardew_Valley/clip"]


@pytest.mark.parametrize("name,expected", [
    ("rec_20250904_f27ef5fc_split_04.mcap", "rec_20250904_f27ef5fc"),
    ("rec_20250904_f27ef5fc_split_12.mcap", "rec_20250904_f27ef5fc"),
    ("apex_0810_08.mcap", "apex_0810_08"),
    ("M_0913_01_split_01.mcap", "M_0913_01"),
])
def test_split_files_collapse_to_one_session(name, expected):
    assert session_of(name) == expected

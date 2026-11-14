"""WM 0.1 动作空间桥与就绪门的单元测试(纯 numpy, 无底座依赖)."""
import json
from pathlib import Path

import numpy as np
import pytest

from diogenes_phase0.contract import ActionFrame
from diogenes_wm.wm01.action_space import (
    BASE_KEYBOARD_DIM, BASE_VAE_TIME_COMPRESSION, WM01_KEYBOARD_V1,
    ActionSpace, encode_window, vocab_fingerprint,
)
from diogenes_wm.wm01.readiness import MIN_HOLDOUT_FRAMES, check, report


def _af(i, dx=0.0, dy=0.0, keys=(), raw=None):
    rdx, rdy = (dx, dy) if raw is None else raw
    return ActionFrame(frame=i, keys=list(keys), pointer_dx=dx, pointer_dy=dy,
                       pointer_bins=(0, 0), raw_dx=rdx, raw_dy=rdy)


def test_vocab_matches_base_keyboard_budget():
    """词表长度必须等于底座的 keyboard_dim_in —— 不等就是静默错位。"""
    assert len(WM01_KEYBOARD_V1) == BASE_KEYBOARD_DIM == 6
    ActionSpace()  # __post_init__ 不抛


def test_vocab_rejects_wrong_width_and_camera_channels():
    with pytest.raises(ValueError, match="keyboard_dim_in"):
        ActionSpace(keyboard=("move.forward",))
    with pytest.raises(ValueError, match="camera"):
        ActionSpace(keyboard=("camera.yaw", "b", "c", "d", "e", "f"))


def test_fingerprint_changes_when_channels_are_reordered():
    """通道重排会让已训 LoRA 与 checkpoint 错位, 指纹必须能抓住。"""
    a = vocab_fingerprint(WM01_KEYBOARD_V1)
    swapped = (WM01_KEYBOARD_V1[1], WM01_KEYBOARD_V1[0], *WM01_KEYBOARD_V1[2:])
    assert a != vocab_fingerprint(swapped)
    assert a == vocab_fingerprint(WM01_KEYBOARD_V1)  # 稳定


def test_semantic_multihot_and_unknown_actions_are_dropped():
    s = ActionSpace()
    v = s.keyboard_multihot(["move.forward", "combat.aim", "combat.killstreak_slot_2"])
    assert v.tolist() == [1, 0, 0, 0, 1, 0]


def test_vk_fallback_when_corpus_has_no_semantics():
    """非 dm-1 语料(无 actions 字段)靠 VK 回退; 鼠标键通道必然为 0。"""
    s = ActionSpace()
    v = s.keyboard_multihot(None, keys=[87, 68])       # W + D
    assert v.tolist() == [1, 0, 0, 1, 0, 0]
    # aim / primary_attack 是鼠标键, VK 里没有 —— 回退下恒 0, 这是有损的
    assert v[4] == 0.0 and v[5] == 0.0


def test_mouse_uses_untruncated_raw_delta_not_the_clamped_one():
    """A3-a: 0.76% 的帧被 clamp 封顶, 且全是甩枪帧 —— 必须取 raw。"""
    s = ActionSpace()
    a = _af(0, dx=200.0, dy=-3.0, raw=(903.0, -3.0))
    assert s.mouse_delta(a).tolist() == [903.0, -3.0]


def test_encode_window_refuses_misaligned_length():
    """长度不是 vae_time_compression_ratio 的整数倍时硬报错, 不补零。"""
    acts = [_af(i) for i in range(BASE_VAE_TIME_COMPRESSION + 1)]
    with pytest.raises(ValueError, match="整数倍"):
        encode_window(acts)


def test_encode_window_shapes():
    T = BASE_VAE_TIME_COMPRESSION * 2
    acts = [_af(i, raw=(i, -i)) for i in range(T)]
    sems = [["move.forward"] if i % 2 else [] for i in range(T)]
    out = encode_window(acts, sems)
    assert out["keyboard"].shape == (T, BASE_KEYBOARD_DIM)
    assert out["mouse"].shape == (T, 2)
    assert np.allclose(out["mouse"][:, 0], np.arange(T))


# ---------------- 就绪门 ----------------

def _corpus(root: Path, sessions: dict[str, list[list[str]]],
            *, dx=5.0, frames_per_chunk=BASE_VAE_TIME_COMPRESSION,
            masked=None, masked_sessions=None, meta_extra=None):
    """按 {session: [每帧语义列表, ...]} 写一个最小 chunk 契约语料。

    `masked` = chunk 的 `meta.masked_actions.semantics`(语料自己声明剔掉的通道);
    `masked_sessions` 限定只有这些 session 的 chunk 带该声明 —— 用来造"半份语料
    mask 了、另半份没有"的口径不一致场景。
    """
    for sid, per_frame in sessions.items():
        for c in range(0, len(per_frame), frames_per_chunk):
            block = per_frame[c:c + frames_per_chunk]
            d = root / sid / f"c{c:04d}"
            d.mkdir(parents=True)
            (d / "p0.npz").write_bytes(b"")          # scan_dataset 只查存在性
            with open(d / "p0.jsonl", "w") as f:
                for i, sems in enumerate(block):
                    f.write(json.dumps({
                        "frame": i, "keys": [], "actions": sems,
                        "pointerDx": dx, "pointerDy": 0,
                        "pointerRawDx": dx, "pointerRawDy": 0,
                        "pointerBins": [0, 0]}) + "\n")
            meta = {
                "sample_key": f"{sid}_c{c:04d}", "game": "g",
                "rights": "third_party", "first_party": False,
                "consent_id": None, "is_trainable": False, "is_cutscene": False,
                "video": {"fps_num": 30, "fps_den": 1, "frames": len(block)}}
            if masked and (masked_sessions is None or sid in masked_sessions):
                meta["masked_actions"] = {"semantics": list(masked)}
            meta.update(meta_extra or {})
            (d / "meta.json").write_text(json.dumps(meta))


def test_readiness_blocks_channel_with_zero_holdout_support(tmp_path):
    """A4 的教训: 会话级零支持会让该通道的指标变成空气, 门必须拦住。"""
    n = 200
    _corpus(tmp_path, {
        # train 会话里有 move.left, holdout 会话里一次都没有
        "s0": [["move.forward", "move.left"]] * n,
        "s1": [["move.forward", "move.left"]] * n,
        "s2": [["move.forward"]] * n,
    })
    r = check(tmp_path, holdout_sessions=1)
    assert not r["ready"]
    assert any("move.left" in b and "holdout" in b for b in r["blockers"])
    # 而 train 侧有支持的通道不该被误报
    fwd = next(c for c in r["channels"] if c["channel"] == "move.forward")
    assert fwd["ok"] and fwd["holdout_frames"] == n


def test_readiness_blocks_static_pointer_corpus(tmp_path):
    """视角不动的语料训不出动作遵循, 而损失曲线看不出来。"""
    sems = [list(WM01_KEYBOARD_V1)] * 200
    _corpus(tmp_path, {"s0": sems, "s1": sems, "s2": sems}, dx=0.0)
    r = check(tmp_path, holdout_sessions=1)
    assert not r["ready"]
    assert any("指针运动帧" in b for b in r["blockers"])


def test_readiness_passes_on_a_balanced_corpus(tmp_path):
    sems = [list(WM01_KEYBOARD_V1)] * 200
    _corpus(tmp_path, {"s0": sems, "s1": sems, "s2": sems})
    r = check(tmp_path, holdout_sessions=1)
    assert r["ready"], r["blockers"]
    assert all(c["holdout_frames"] >= MIN_HOLDOUT_FRAMES for c in r["channels"])
    assert not r["warnings"]
    assert r["vocab"]["fingerprint"] == ActionSpace().fingerprint


def test_readiness_warns_on_distribution_shift_without_blocking(tmp_path):
    """打法差异不该阻断训练, 但必须并列报出来 —— COD 上 combat.aim 是 0.30×。"""
    full = [list(WM01_KEYBOARD_V1)] * 200
    # holdout 会话里 combat.aim 只出现 1/4 的帧, 其余通道照旧
    thin = [[c for c in WM01_KEYBOARD_V1 if c != "combat.aim"] if i % 4 else
            list(WM01_KEYBOARD_V1) for i in range(200)]
    _corpus(tmp_path, {"s0": full, "s1": full, "s2": thin})
    r = check(tmp_path, holdout_sessions=1)
    assert r["ready"], r["blockers"]          # 不阻断
    assert any("combat.aim" in w for w in r["warnings"])
    aim = next(c for c in r["channels"] if c["channel"] == "combat.aim")
    assert aim["ok"] and not aim["shift_ok"]
    assert aim["shift"] == pytest.approx(0.25, abs=0.02)


def test_readiness_does_not_block_on_a_channel_the_corpus_declares_masked(tmp_path):
    """Halo 有意剔掉 combat.aim(toggle-zoom 是坏标签) —— 零支持是裁决不是缺陷。

    这一条是 wm_halo_s24.yaml 的阻断项 B2: 语料侧做对了事, 门却因为通道恒 0 而
    exit 2。判据只有 chunk meta 自己声明的 masked_actions.semantics。
    """
    sems = [[c for c in WM01_KEYBOARD_V1 if c != "combat.aim"]] * 200
    _corpus(tmp_path, {"s0": sems, "s1": sems, "s2": sems},
            masked=["combat.aim"])
    r = check(tmp_path, holdout_sessions=1)
    assert r["ready"], r["blockers"]
    aim = next(c for c in r["channels"] if c["channel"] == "combat.aim")
    assert aim["masked"] and aim["ok"] and aim["train_frames"] == 0
    assert r["vocab"]["masked_channels"] == ["combat.aim"]
    # 指纹: 基指纹与未 mask 的臂逐字节相同, 所以必须另给一个有效指纹
    assert r["vocab"]["fingerprint"] == ActionSpace().fingerprint
    assert r["vocab"]["effective_fingerprint"] != r["vocab"]["fingerprint"]
    assert "effective_fingerprint" in report(r)

    # 没声明 mask 的同一份语料照旧阻断(A4 那条教训没有被放宽)
    plain = tmp_path / "plain"
    _corpus(plain, {"s0": sems, "s1": sems, "s2": sems})
    r2 = check(plain, holdout_sessions=1)
    assert not r2["ready"]
    assert any("combat.aim" in b and "零支持" in b for b in r2["blockers"])


def test_readiness_blocks_when_only_part_of_the_corpus_is_masked(tmp_path):
    """半份语料 mask 了、另半份没有 —— 拼起来的分布是假的, 而统计上看不出来。"""
    with_aim = [list(WM01_KEYBOARD_V1)] * 200
    without = [[c for c in WM01_KEYBOARD_V1 if c != "combat.aim"]] * 200
    _corpus(tmp_path, {"s0": without, "s1": without, "s2": with_aim},
            masked=["combat.aim"], masked_sessions={"s0", "s1"})
    r = check(tmp_path, holdout_sessions=1)
    assert not r["ready"]
    assert any("mask 口径不一致" in b for b in r["blockers"])


def test_readiness_reads_masked_channels_from_a_real_halo_chunk(tmp_path):
    """端到端: 适配器写的 meta 与就绪门读的字段必须是同一个(不是各写各的)。"""
    ha = pytest.importorskip("diogenes_wm.halo_adapter")
    pytest.importorskip("zstandard")
    from test_halo_adapter import _frame, write_clip       # 复用真实六件套夹具

    src = tmp_path / "src"
    for s, sid in enumerate(("aaaa1111-0000-4000-8000-00000000000%d" % i
                             for i in range(3))):
        frames = [_frame(i, anchor=1_000 + s, span=100,
                         actions=["move.forward", "move.left", "move.right",
                                  "move.backward", "combat.aim",
                                  "combat.primary_attack"], pdx=7)
                  for i in range(BASE_VAE_TIME_COMPRESSION * 10)]
        write_clip(src, session=sid, clip_id=f"c-{s}", frames=frames)
    out = tmp_path / "out"
    ha.convert(src, out, verify=0)          # 默认就 mask combat.aim

    r = check(out, holdout_sessions=1)
    assert r["vocab"]["masked_channels"] == ["combat.aim"]
    aim = next(c for c in r["channels"] if c["channel"] == "combat.aim")
    assert aim["masked"] and aim["train_frames"] == 0
    assert not any("combat.aim" in b for b in r["blockers"])


# ---------------- 数据集 / 重采样 ----------------

from diogenes_wm.wm01.dataset import (  # noqa: E402
    FIRST_CHUNK_FRAMES, SUBSEQUENT_CHUNK_FRAMES, TARGET_FPS,
    build_time_grid, check_clip_frames, encode_actions, legal_clip_lengths,
)


def test_only_longlive_block_lengths_are_accepted():
    """clip 长度必须是 29 + k×32; 别的长度底座会在 assert 上炸。"""
    assert legal_clip_lengths(130) == [29, 61, 93, 125]
    check_clip_frames(93)
    with pytest.raises(ValueError, match="合法块长"):
        check_clip_frames(96)
    assert FIRST_CHUNK_FRAMES == 29 and SUBSEQUENT_CHUNK_FRAMES == 32


def test_time_grid_is_built_by_time_not_frame_index():
    """30/16 = 1.875 不是整数, 按序号抽会周期性抖动。"""
    g = build_time_grid(n_src=30, src_fps=30.0, dst_fps=16.0)
    assert g.out_frames == 16
    # 覆盖区间必须首尾相接、单调、不留空洞
    assert g.source_index[0][0] == 0
    for (a0, a1), (b0, _) in zip(g.source_index, g.source_index[1:]):
        assert a1 == b0 and a1 > a0
    # 步长在 1 和 2 之间交替 —— 这正是 1.875 的样子, 不是恒定的 1 或 2
    steps = {hi - lo for lo, hi in g.source_index}
    assert steps == {1, 2}


def test_time_grid_refuses_upsampling():
    with pytest.raises(ValueError, match="拒绝上采样"):
        build_time_grid(n_src=100, src_fps=10.0, dst_fps=TARGET_FPS)


def test_pointer_delta_is_summed_not_sampled():
    """位移求和是承重的: 抽样会压缩 delta 分布 = 直接把鼠标信号做小。"""
    acts = [_af(i, raw=(10.0, -1.0)) for i in range(30)]
    g = build_time_grid(30, 30.0, 16.0)
    enc = encode_actions(acts, g, [[]] * 30, ActionSpace())
    # 总位移必须守恒: 30 帧 × 10 = 300
    assert enc["mouse"][:, 0].sum() == pytest.approx(300.0)
    assert enc["mouse"][:, 1].sum() == pytest.approx(-30.0)
    # 且覆盖两个源帧的输出帧应当是覆盖一个的两倍 —— 抽样的话会全相等
    assert len(set(np.round(enc["mouse"][:, 0], 3).tolist())) == 2


def test_keyboard_is_coverage_fraction_so_both_readings_survive():
    """一个 16fps 输出帧盖 1.875 个源帧; 取 OR 会把点射的持续时间放大近两倍。"""
    # 只在偶数源帧按下 move.forward
    acts = [_af(i) for i in range(30)]
    sems = [["move.forward"] if i % 2 == 0 else [] for i in range(30)]
    g = build_time_grid(30, 30.0, 16.0)
    enc = encode_actions(acts, g, sems, ActionSpace())
    fwd = enc["keyboard"][:, 0]
    assert fwd.max() <= 1.0 and fwd.min() >= 0.0
    # 存在既非 0 也非 1 的值 —— 这正是 OR 会抹掉的那部分信息
    assert ((fwd > 0) & (fwd < 1)).any()
    # 二值读法仍在
    assert (fwd > 0).any()


def test_group_runs_breaks_at_missing_segment_index(tmp_path):
    """缺号说明中间有段被判废; 跨过缺口拼会把不连续的画面粘成一条。"""
    from diogenes_wm.wm01.dataset import group_runs
    from diogenes_phase0.data.dataset import scan_dataset
    for idx in (0, 1, 3):                       # 缺 2
        d = tmp_path / "s0" / f"c{idx:04d}"
        d.mkdir(parents=True)
        (d / "p0.npz").write_bytes(b"")
        (d / "p0.jsonl").write_text(json.dumps(
            {"frame": 0, "keys": [], "actions": [], "pointerDx": 0,
             "pointerDy": 0, "pointerBins": [0, 0]}) + "\n")
        (d / "meta.json").write_text(json.dumps({
            "sample_key": f"s0_c{idx:04d}", "game": "g", "rights": "third_party",
            "first_party": False, "consent_id": None, "is_trainable": False,
            "is_cutscene": False,
            "provenance": {"run_id": "r0000", "segment_index": idx},
            "video": {"fps_num": 30, "fps_den": 1, "frames": 1}}))
    runs = group_runs(scan_dataset(tmp_path))
    assert [len(r.records) for r in runs] == [2, 1], "缺号处必须断开"


@pytest.mark.skipif(
    not (__import__("shutil").which("ffmpeg") and __import__("shutil").which("ffprobe")),
    reason="需要 ffmpeg + ffprobe（单文件 ffmpeg 发行版不带 ffprobe，只查前者会假失败）",
)
def test_clip_video_frames_equal_action_frames(tmp_path):
    """`-frames:v` 数的是输出帧。写成源帧数会让视频比动作长 30/16 倍，
    而训练侧按较短的那个截断——画面与动作**静默错位**，没有任何地方会报错。"""
    import subprocess
    from diogenes_wm.wm01.dataset import _ffmpeg_clip
    src = tmp_path / "src.mp4"
    subprocess.run(["ffmpeg", "-v", "error", "-y", "-f", "lavfi",
                    "-i", "testsrc=size=64x64:rate=30:duration=8",
                    "-c:v", "libx264", "-pix_fmt", "yuv420p", str(src)], check=True)
    dst = tmp_path / "clip.mp4"
    _ffmpeg_clip(src, dst, start_frame=0, n_src=175, out_frames=93,
                 src_fps=30.0, size=(64, 64), crf=30)
    n = subprocess.run(["ffprobe", "-v", "error", "-select_streams", "v:0",
                        "-count_frames", "-show_entries", "stream=nb_read_frames",
                        "-of", "csv=p=0", str(dst)],
                       capture_output=True, text=True, check=True).stdout.strip()
    assert int(n) == 93


# ---------------- 动作模块接线 ----------------

def test_latent_frames_matches_longlive_block_math():
    """93 帧在两个底座上必须独立算出同一个潜帧数, 否则画面与动作会静默错位。"""
    from diogenes_wm.wm01.action_module import latent_frames
    from diogenes_wm.wm01.dataset import TEMPORAL_COMPRESSION
    assert latent_frames(93) == 24
    assert 1 + (93 - 1) // TEMPORAL_COMPRESSION == 24      # LongLive 那侧
    assert latent_frames(32) == 8                          # N % 4 == 0 的分支
    with pytest.raises(ValueError, match="不满足动作模块的约束"):
        latent_frames(94)


def test_action_module_is_built_from_the_frozen_vocab_width():
    """维度不是手填常数 —— 改了词表忘了改模块要在构造期就炸, 不是训练期。"""
    torch = pytest.importorskip("torch")
    pytest.importorskip("einops")
    from diogenes_wm.wm01.action_module import build_action_module
    m = build_action_module(img_hidden_size=64)
    assert m.keyboard_embed[0].in_features == BASE_KEYBOARD_DIM == 6
    # 上游的隐藏耦合要在构造期说清楚, 不是抛一句裸 assert
    with pytest.raises(ValueError, match="必须等于"):
        build_action_module(img_hidden_size=64, heads_num=2)


def test_action_module_forwards_on_cpu_and_actions_change_the_output():
    """零动作与真动作必须给出不同的输出 —— 否则挂了个恒等模块也看不出来。"""
    torch = pytest.importorskip("torch")
    pytest.importorskip("einops")
    from diogenes_wm.wm01.action_module import build_action_module
    C, tt, th, tw, T = 64, 24, 2, 2, 93
    m = build_action_module(img_hidden_size=C).eval()
    x = torch.randn(1, tt * th * tw, C)
    mouse, kb = torch.randn(1, T, 2), torch.rand(1, T, 6)
    with torch.no_grad():
        y = m(x, tt, th, tw, mouse_condition=mouse, keyboard_condition=kb)
        y0 = m(x, tt, th, tw, mouse_condition=mouse * 0, keyboard_condition=kb * 0)
    assert y.shape == x.shape
    assert (y - y0).abs().mean() > 1e-6, "动作对输出没有影响 = 挂了个恒等模块"


def test_conditioner_refuses_action_grid_mismatch():
    """动作长度与网格不一致时上游按较短的截断, 静默错位 —— 这里必须先炸。"""
    torch = pytest.importorskip("torch")
    pytest.importorskip("einops")
    from diogenes_wm.wm01.action_module import ActionConditioner, build_action_module
    cond = ActionConditioner(build_action_module(img_hidden_size=64),
                             grid=(24, 2, 2))
    cond.set_actions(torch.randn(1, 93, 2), torch.rand(1, 93, 6))      # ok
    with pytest.raises(ValueError, match="静默错位"):
        cond.set_actions(torch.randn(1, 61, 2), torch.rand(1, 61, 6))  # → 16 潜帧
    with pytest.raises(ValueError, match="同一条时间线"):
        cond.set_actions(torch.randn(1, 93, 2), torch.rand(1, 61, 6))


# ---------------- rollout 打分 ----------------

def test_score_report_refuses_to_be_read_without_a_control():
    """一个只会「画面在动」的模型也能拿到非零耦合——没对照的读数不能用。"""
    from diogenes_wm.wm01.score_rollout import GROUND_TRUTH_COUPLING, report
    fake = {"rollouts": 3, "actions_missing": [], "min_delta": 3.0,
            "p90_displacement": 100.0, "ground_truth_coupling": GROUND_TRUTH_COUPLING,
            "all": {"pairs": 90, "r_dx": -0.3, "r_dy": -0.2, "coupling": 0.3},
            "small_delta": {"pairs": 80, "r_dx": -0.3, "r_dy": -0.2, "coupling": 0.3},
            "large_delta_observational_only": {"pairs": 10, "r_dx": -0.1,
                                               "r_dy": -0.05, "coupling": 0.1}}
    md = report(fake, None, "t")
    assert "没有并列对照" in md and "不能用" in md
    # 大位移档必须被标成观察项而不是判据
    assert "观察项" in md and "瓶颈是量具不是标签" in md
    # 判据以「占真值的比例」呈现, 不是绝对阈值
    assert f"{100 * 0.3 / GROUND_TRUTH_COUPLING:.1f}%" in md


def test_score_report_flags_a_control_matching_run_as_no_signal():
    """与不喂动作的对照没有区别时, 必须明说这个数不能读成「学会了动作」。"""
    from diogenes_wm.wm01.score_rollout import GROUND_TRUTH_COUPLING, report
    def mk(c):
        return {"rollouts": 3, "actions_missing": [], "min_delta": 3.0,
                "p90_displacement": 100.0,
                "ground_truth_coupling": GROUND_TRUTH_COUPLING,
                "all": {"pairs": 90, "r_dx": -c, "r_dy": 0.0, "coupling": c},
                "small_delta": {"pairs": 80, "r_dx": -c, "r_dy": 0.0, "coupling": c},
                "large_delta_observational_only": {"pairs": 10, "r_dx": 0.0,
                                                   "r_dy": 0.0, "coupling": 0.0}}
    assert "不能读成" in report(mk(0.30), mk(0.29), "t")
    assert "动作确实带来了信号" in report(mk(0.40), mk(0.10), "t")


def test_plane_builder_runs_the_compliance_gate(tmp_path, monkeypatch):
    """字节进训练的边界在这一步 —— 上一步盖的戳是自陈的, 这里必须真的验一次。"""
    from diogenes_phase0.contract import ComplianceError
    from diogenes_wm.wm01 import dataset as ds

    sems = [list(WM01_KEYBOARD_V1)] * 8
    root = tmp_path / "corpus"
    # dm-1 chunk(带 per-game mapping)但 is_trainable=False —— 即"适配器默认档"的形状
    _corpus(root, {"s0": sems},
            meta_extra={"schema_version": "dm-1",
                        "mapping_id": "game.halo-infinite.steam.pc"})

    # 走到闸就该停, 不该先去调 ffmpeg
    called = []
    monkeypatch.setattr(ds, "_ffmpeg_clip", lambda *a, **k: called.append(a))
    with pytest.raises(ComplianceError, match="is_trainable"):
        ds.convert(root, tmp_path / "plane", size=(832, 480), clip_frames=29,
                   caption="x")
    assert not called

    # 换一条车道也绕不过去: dm-1 语料只能走 dm1-attested(否则两套键空间混在一次
    # 训练里), 而那条车道不放宽治理 —— 两个理由不同的硬拦
    with pytest.raises(ComplianceError, match="dm-1/per-game"):
        ds.convert(root, tmp_path / "plane2", size=(832, 480), clip_frames=29,
                   caption="x", contract_mode="method-validation")

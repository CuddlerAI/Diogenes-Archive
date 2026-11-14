"""AB-P0 伪标注管线的单元测试 (research/35 §3.3).

夹具是手写的 dm-1 契约 chunk(npz 帧, 不需要 opencv/ffmpeg/pyarrow), 治理戳
与 cod_adapter 的 owner 具结车道逐字段同构。两个口径坑各有直接命中的测试:
词表(12 键 ↔ 六通道投影、combat 语义补位)与 fps(覆盖比例、拒绝上采样、
与 native 平面的逐 stem 对齐)。
"""
import json
import sys
from pathlib import Path

import numpy as np
import pytest
import torch
import yaml

from diogenes_phase0.contract import (ChunkMeta, ComplianceError,
                                      gate_phase0_contract, load_actions)
from diogenes_phase0.data.dataset import scan_dataset
from diogenes_phase0.keys import SCANNED_VKEYS, VK_TO_INDEX
from diogenes_phase0.models.idm import build_model
from diogenes_wm.ab_p0 import bridge as ab_bridge
from diogenes_wm.ab_p0 import finetune as ab_finetune
from diogenes_wm.ab_p0 import pseudo_label as ab_pseudo
from diogenes_wm.wm01.action_space import (AB_P0_CHANNEL_VKS,
                                           AB_P0_VK_BY_SEMANTIC, VK_FALLBACK,
                                           WM01_KEYBOARD_V1, ActionSpace,
                                           scanned_to_wm01)
from diogenes_wm.wm01.dataset import (TARGET_FPS, clip_grid, encode_actions,
                                      group_runs)

# ---------------------------------------------------------------------------
# 夹具: dm-1 契约 chunk, 治理戳与 cod_adapter 的 owner 具结车道同构
# ---------------------------------------------------------------------------

ATTESTED_STAMP = {
    "rights": "first_party", "first_party": True, "consent_id": "OWNER-TEST",
    "non_trainable": False, "is_trainable": True, "production_certified": False,
    "is_cutscene": False,
    "tags": ["owner_attested_validation", "no_publish",
             "no_product_training", "trainable"],
}
THIRD_PARTY_STAMP = {
    "rights": "local_debug_non_trainable", "first_party": False,
    "consent_id": None, "non_trainable": True, "is_trainable": False,
    "production_certified": False, "is_cutscene": False,
    "tags": ["lane_a_probe", "method_validation_only", "non_trainable"],
}


def _dm1_meta(session: str, seg: int, n_frames: int, *, fps: float = 30.0,
              stamp: dict = ATTESTED_STAMP) -> dict:
    return {
        "sample_key": f"{session}_r0000-{seg:05d}",
        "game": "cod-test", "platform": "pc",
        "schema_version": "dm-1",
        "key_schema": "dm-1 mapping (per-game physical vocab)",
        "annotation_source": "raw_input_recorded",
        "video": {"file": "p0.npz", "codec": "npz", "width": 32, "height": 32,
                  "fps": fps, "fps_num": int(fps), "fps_den": 1,
                  "frames": n_frames, "audio": False},
        "provenance": {"run_id": "r0000", "segment_index": seg,
                       "mapping_id": "map", "mapping_version": "m1"},
        **stamp,
    }


def _write_chunk(root: Path, session: str, seg: int, n_frames: int, *,
                 fps: float = 30.0, stamp: dict = ATTESTED_STAMP,
                 seed: int = 0) -> Path:
    """even 帧: W+Shift(词表外) 按住; odd 帧: 只有语义 combat.aim(鼠标键不在
    keys 列 —— 复现 COD 语料的实测形状)。指针位移恒为 (3, -1)。"""
    cdir = root / session / f"r0000-{seg:05d}"
    cdir.mkdir(parents=True)
    rng = np.random.default_rng(seed)
    np.savez_compressed(cdir / "p0.npz",
                        frames=rng.integers(0, 255, (n_frames, 32, 32, 3),
                                            dtype=np.uint8))
    with open(cdir / "p0.jsonl", "w") as f:
        for t in range(n_frames):
            even = t % 2 == 0
            f.write(json.dumps({
                "frame": t,
                "keys": [87, 16] if even else [],
                "actions": ["move.forward"] if even else ["combat.aim"],
                "pointerDx": 3, "pointerDy": -1,
                "pointerRawDx": 3, "pointerRawDy": -1,
                "pointerBins": [0, 0], "src": "rec",
            }) + "\n")
    (cdir / "meta.json").write_text(
        json.dumps(_dm1_meta(session, seg, n_frames, fps=fps, stamp=stamp)))
    return cdir


def _corpus(root: Path, *, sessions=("s00", "s01", "s02"), segments=2,
            frames=40, fps: float = 30.0, stamp: dict = ATTESTED_STAMP) -> Path:
    for si, s in enumerate(sessions):
        for g in range(segments):
            _write_chunk(root, s, g, frames, fps=fps, stamp=stamp,
                         seed=si * 10 + g)
    return root


TINY_CFG = {"model": {"trunk": "tiny", "d_model": 32, "n_layers": 1,
                      "n_heads": 4},
            "data": {"window": 3, "input_res": [32, 32], "delta_scale": 50.0}}


def _tiny_checkpoint(path: Path) -> Path:
    torch.manual_seed(0)
    model = build_model(TINY_CFG)
    torch.save({"model": model.state_dict(), "config": TINY_CFG, "step": 0},
               path)
    return path


# ---------------------------------------------------------------------------
# 口径坑 1 · 词表: 12 键 ↔ 六通道
# ---------------------------------------------------------------------------

def test_projection_tables_are_consistent():
    """投影表只有一份: 与 SCANNED_VKEYS、WM01_KEYBOARD_V1、VK_FALLBACK 三向锁死."""
    assert len(AB_P0_CHANNEL_VKS) == len(WM01_KEYBOARD_V1)
    for sem, vk in AB_P0_VK_BY_SEMANTIC.items():
        assert vk in SCANNED_VKEYS
        fb = VK_FALLBACK.get(sem, ())
        if fb:  # WASD: 与 wm01 的物理回退表指同一个键
            assert vk in fb
    # combat 两通道是 data_demo 约定的鼠标键
    assert AB_P0_VK_BY_SEMANTIC["combat.aim"] == 2
    assert AB_P0_VK_BY_SEMANTIC["combat.primary_attack"] == 1


def test_scanned_to_wm01_selects_the_right_columns():
    kb = np.zeros((3, len(SCANNED_VKEYS)), np.float32)
    kb[0, VK_TO_INDEX[87]] = 1.0        # W → move.forward
    kb[1, VK_TO_INDEX[2]] = 1.0         # mouseR → combat.aim
    kb[2, VK_TO_INDEX[49]] = 1.0        # 数字 1 → 六通道里没有, 应被丢弃
    out = scanned_to_wm01(kb)
    assert out.shape == (3, 6)
    assert out[0].tolist() == [1, 0, 0, 0, 0, 0]
    assert out[1, WM01_KEYBOARD_V1.index("combat.aim")] == 1.0
    assert out[2].sum() == 0.0


def test_scanned_to_wm01_passes_probabilities_through():
    """投影只是选列 —— 概率/置信度不该被二值化或聚合."""
    probs = np.full((4, len(SCANNED_VKEYS)), 0.25, np.float32)
    probs[:, VK_TO_INDEX[83]] = 0.7
    out = scanned_to_wm01(probs)
    assert np.allclose(out[:, WM01_KEYBOARD_V1.index("move.backward")], 0.7)
    with pytest.raises(ValueError):
        scanned_to_wm01(np.zeros((4, 6)))   # 已经是六通道的数组不许再投


# ---------------------------------------------------------------------------
# 契约车道: dm1-attested
# ---------------------------------------------------------------------------

def test_dm1_attested_gate(tmp_path):
    attested = ChunkMeta.from_json(_dm1_meta("s00", 0, 8))
    gate_phase0_contract([attested], contract_mode="dm1-attested")

    # 混入 legacy chunk = 两套 key schema 进同一个 run, 拒绝
    legacy = ChunkMeta.from_json({
        "sample_key": "legacy_c0", "game": "synth",
        "key_schema": ("VK codes; W/A/S/D=87/65/83/68, mouseL/R=1/2, 1..6=49..54 "
                       "(mirrors input_recorder.rs SCANNED_VKEYS)"),
        "video": {"fps": 30}, "rights": "synthetic", "first_party": True,
        "consent_id": "synthetic", "is_trainable": True, "is_cutscene": False,
    })
    with pytest.raises(ComplianceError, match="dm-1"):
        gate_phase0_contract([attested, legacy], contract_mode="dm1-attested")

    # 治理门无松弛: 第三方车道的 dm-1 chunk 过不去
    third = ChunkMeta.from_json(_dm1_meta("s00", 1, 8, stamp=THIRD_PARTY_STAMP))
    with pytest.raises(ComplianceError, match="is_trainable"):
        gate_phase0_contract([third], contract_mode="dm1-attested")

    # 既有边界不动: legacy 车道照旧拒收 dm-1
    with pytest.raises(ComplianceError, match="dm-1"):
        gate_phase0_contract([attested], contract_mode="legacy")


# ---------------------------------------------------------------------------
# 微调入口
# ---------------------------------------------------------------------------

def test_session_tail_split_matches_wm01_rule(tmp_path):
    _corpus(tmp_path / "d", sessions=("s03", "s00", "s02", "s01"), segments=1,
            frames=8)
    recs = scan_dataset(tmp_path / "d")
    s = ab_finetune.split_session_tail(recs, holdout_sessions=2)
    # 会话 id 升序取尾 2 —— 与 wm01/readiness.py 同口径
    assert s["holdout_sessions"] == ["s02", "s03"]
    assert {r.chunk_dir.parent.name for r in s["train"]} == {"s00", "s01"}
    with pytest.raises(SystemExit):
        ab_finetune.split_session_tail(recs, holdout_sessions=4)


def test_semantic_backfill_adds_mouse_key_bits(tmp_path):
    """COD 的 keys 列没有鼠标键 —— combat 位必须从语义补回, 否则零监督."""
    _corpus(tmp_path / "d", sessions=("s00",), segments=1, frames=12)
    recs = scan_dataset(tmp_path / "d")
    ds = ab_finetune.CodWindowDataset(
        recs, window=3, stride=1, input_res=(32, 32),
        contract_mode="dm1-attested", expected_fps=30)
    # index 里 t 从 1 开始(half=1); 找一个 odd 帧(只有语义 combat.aim)
    i_odd = next(i for i, (_, t) in enumerate(ds.index) if t % 2 == 1)
    keys = ds[i_odd]["keys"]
    assert keys[VK_TO_INDEX[2]] == 1.0          # combat.aim → mouseR
    assert keys[VK_TO_INDEX[87]] == 0.0
    i_even = next(i for i, (_, t) in enumerate(ds.index) if t % 2 == 0)
    keys = ds[i_even]["keys"]
    assert keys[VK_TO_INDEX[87]] == 1.0         # 物理 W 照旧(补位幂等)
    assert keys[VK_TO_INDEX[2]] == 0.0


def test_oov_report_counts_dropped_keys(tmp_path):
    _corpus(tmp_path / "d", sessions=("s00",), segments=1, frames=10)
    oov = ab_finetune.oov_report(scan_dataset(tmp_path / "d"))
    # even 帧 keys=[87,16]: 87 在词表, 16(Shift) 不在 → 份额恰好 50%
    assert oov["in_vocab"] == oov["out_of_vocab"] == 5
    assert oov["out_of_vocab_share"] == 0.5
    assert oov["dropped_vks_top"][0][0] == 16


def _finetune_cfg(root: Path, out: Path) -> dict:
    return {
        "run_name": "ab-p0-test", "seed": 0,
        "output": {"dir": str(out)},
        "data": {"roots": [str(root)], "window": 3, "stride": 2,
                 "input_res": [32, 32], "delta_scale": 50.0,
                 "contract_mode": "dm1-attested", "expected_fps": 30,
                 "holdout_sessions": 2},
        "model": {"trunk": "tiny", "d_model": 32, "n_layers": 1, "n_heads": 4},
        "train": {"batch_size": 4, "max_steps": 2, "log_every": 1,
                  "save_every": 1, "lr": 1e-4, "num_workers": 0},
    }


def test_finetune_smoke_cli(tmp_path, monkeypatch):
    root = _corpus(tmp_path / "d")
    cfg_p = tmp_path / "cfg.yaml"
    cfg_p.write_text(yaml.safe_dump(_finetune_cfg(root, tmp_path / "runs")))
    monkeypatch.setattr(sys, "argv",
                        ["finetune", "--config", str(cfg_p)])
    ab_finetune.main()
    run_dir = next((tmp_path / "runs").iterdir())
    assert (run_dir / "checkpoint.pt").exists()
    report = json.loads((run_dir / "finetune_report.json").read_text())
    assert report["split"]["holdout_sessions"] == ["s01", "s02"]
    assert report["split"]["oov"]["out_of_vocab_share"] == 0.5
    assert "key_macro_f1" in report["holdout"]


def test_finetune_refuses_save_every_zero(tmp_path, monkeypatch):
    """runbook 纪律是启动条件, 不是善意提醒: 数小时的卡, 崩了不能白跑."""
    root = _corpus(tmp_path / "d", sessions=("s00", "s01", "s02"), segments=1,
                   frames=8)
    cfg = _finetune_cfg(root, tmp_path / "runs")
    cfg["train"]["save_every"] = 0
    cfg_p = tmp_path / "cfg.yaml"
    cfg_p.write_text(yaml.safe_dump(cfg))
    monkeypatch.setattr(sys, "argv", ["finetune", "--config", str(cfg_p)])
    with pytest.raises(SystemExit, match="save_every"):
        ab_finetune.main()


# ---------------------------------------------------------------------------
# 伪标注导出
# ---------------------------------------------------------------------------

def test_pseudo_label_export(tmp_path):
    root = _corpus(tmp_path / "d", sessions=("s00",), segments=2, frames=10)
    ckpt = _tiny_checkpoint(tmp_path / "ck.pt")
    out = tmp_path / "pseudo"
    manifest = ab_pseudo.run(ckpt, root, out, batch_size=8)

    assert manifest["chunks"] == 2 and manifest["frames"] == 20
    z = np.load(out / "s00" / "r0000-00000" / "pseudo.npz")
    n_keys = len(SCANNED_VKEYS)
    assert z["keys"].shape == (10, n_keys)          # 与 p0.jsonl 严格 1:1
    assert z["keys"].dtype == np.uint8
    p, b, c = z["keys_prob"], z["keys"], z["keys_conf"]
    assert np.allclose(c, p * b + (1 - p) * (1 - b))   # c_t = 所发标签的置信度
    assert (c >= 0.5 - 1e-6).all() and (c <= 1.0).all()
    assert z["pointer_from_bins"].shape == (10, 2)
    assert int(z["edge_halo"]) == 1                    # window 3 → half 1

    # 治理戳跟着数据走
    meta = json.loads((out / "s00" / "r0000-00000" / "meta.json").read_text())
    assert "no_publish" in meta["tags"] and "pseudo_label" in meta["tags"]
    assert meta["is_trainable"] is True

    # 标注器质量读数与产物在一起
    ch = {c["channel"]: c for c in manifest["channel_agreement"]}
    assert ch["combat.aim"]["native_rate"] == 0.5      # odd 帧的语义真值
    assert set(ch) == set(WM01_KEYBOARD_V1)


def test_pseudo_label_refuses_non_trainable(tmp_path):
    root = _corpus(tmp_path / "d", sessions=("s00",), segments=1, frames=8,
                   stamp=THIRD_PARTY_STAMP)
    ckpt = _tiny_checkpoint(tmp_path / "ck.pt")
    with pytest.raises(ComplianceError, match="is_trainable"):
        ab_pseudo.run(ckpt, root, tmp_path / "pseudo")


# ---------------------------------------------------------------------------
# 口径坑 2 · fps: bridge 走同一重采样纪律
# ---------------------------------------------------------------------------

CLIP_FRAMES = 29  # LongLive 最小合法块长


def _build_native_plane(chunks: Path, plane: Path) -> None:
    """用与 wm01.dataset.convert 相同的共享件(clip_grid + encode_actions)搭
    native action 平面 —— 只差 ffmpeg 的视频部分, 动作路径逐字节同源."""
    space = ActionSpace()
    for run in group_runs(scan_dataset(chunks)):
        src_fps = run.records[0].meta.fps
        acts, sems = [], []
        for r in run.records:
            acts.extend(load_actions(r.actions_path))
            sems.extend(json.loads(x).get("actions", []) for x in
                        r.actions_path.read_text().splitlines() if x.strip())
        n = min(len(acts), len(sems))
        spc = int(np.ceil(CLIP_FRAMES * src_fps / TARGET_FPS))
        for c in range(n // spc):
            lo = c * spc
            grid = clip_grid(spc, src_fps, CLIP_FRAMES)
            enc = encode_actions(acts[lo:lo + spc], grid, sems[lo:lo + spc],
                                 space)
            p = plane / "action" / run.session / f"{run.run_id}-{c:04d}.npz"
            p.parent.mkdir(parents=True, exist_ok=True)
            np.savez_compressed(p, **enc)
    plane.mkdir(parents=True, exist_ok=True)
    (plane / "wm01_dataset.json").write_text(json.dumps({
        "clip_frames": CLIP_FRAMES, "target_fps": TARGET_FPS,
        "vocab_fingerprint": space.fingerprint}))


def _write_pseudo(chunks: Path, pseudo: Path, *, w_on_even: bool = True) -> None:
    """确定性伪标签: W 只在 even 源帧按下, 置信度恒 0.9, 指针恒 (1,1)."""
    for rec in scan_dataset(chunks):
        n = rec.n_frames
        keys = np.zeros((n, len(SCANNED_VKEYS)), np.uint8)
        if w_on_even:
            keys[::2, VK_TO_INDEX[87]] = 1
        dst = pseudo / rec.chunk_dir.parent.name / rec.chunk_dir.name
        dst.mkdir(parents=True)
        np.savez_compressed(
            dst / "pseudo.npz", keys=keys,
            keys_conf=np.full((n, len(SCANNED_VKEYS)), 0.9, np.float32),
            pointer_from_bins=np.ones((n, 2), np.float32))


def test_bridge_coverage_confidence_and_alignment(tmp_path):
    chunks = _corpus(tmp_path / "chunks", sessions=("s00",), segments=2,
                     frames=40)                       # 一条 run, 80 源帧, 2 clip
    plane = tmp_path / "plane"
    _build_native_plane(chunks, plane)
    _write_pseudo(chunks, tmp_path / "pseudo")

    stats = ab_bridge.convert(chunks, tmp_path / "pseudo", plane)
    assert stats["clips"] == 2

    z = np.load(plane / "action_pseudo" / "s00" / "r0000-0000.npz")
    native = np.load(plane / "action" / "s00" / "r0000-0000.npz")
    assert z["keyboard"].shape == native["keyboard"].shape == (CLIP_FRAMES, 6)

    w = z["keyboard"][:, WM01_KEYBOARD_V1.index("move.forward")]
    # 30→24 的栅格里有 2 源帧的窗口: W 只在 even 帧按下 → 覆盖比例 0.5,
    # 出现分数值即证明存的是覆盖比例而不是 OR
    assert ((0 < w) & (w < 1)).any()
    assert (w <= 1).all() and (w >= 0).all()
    assert np.allclose(z["keyboard_confidence"], 0.9, atol=1e-6)
    # 鼠标(native 路)与真值臂逐字节同源 —— 「同一重采样纪律」是可测的
    assert np.allclose(z["mouse"], native["mouse"])

    # A2/A3 消费同一份文件: 均匀臂无视 confidence, 加权臂用它
    assert set(z.files) == {"keyboard", "mouse", "keyboard_confidence"}
    assert (plane / "action_pseudo_bridge.json").exists()


def test_bridge_pseudo_mouse_source(tmp_path):
    chunks = _corpus(tmp_path / "chunks", sessions=("s00",), segments=2,
                     frames=40)
    plane = tmp_path / "plane"
    _build_native_plane(chunks, plane)
    _write_pseudo(chunks, tmp_path / "pseudo")
    ab_bridge.convert(chunks, tmp_path / "pseudo", plane,
                      arm_dir="action_pseudo_pm", mouse_source="pseudo")
    z = np.load(plane / "action_pseudo_pm" / "s00" / "r0000-0000.npz")
    # 位移逐窗口求和: 每源帧 (1,1), 窗口有 1 或 2 帧 → 值 ∈ {1,2}, 总和 = 源帧数
    assert set(np.unique(z["mouse"])) <= {1.0, 2.0}
    grid = clip_grid(int(np.ceil(CLIP_FRAMES * 30 / TARGET_FPS)), 30.0,
                     CLIP_FRAMES)
    covered = sum(hi - lo for lo, hi in grid.source_index)
    assert z["mouse"][:, 0].sum() == covered


def test_bridge_refuses_upsampling(tmp_path):
    """fps 纪律: 源帧率低于平面帧率时拒绝, 不插值."""
    chunks = _corpus(tmp_path / "chunks", sessions=("s00",), segments=1,
                     frames=40, fps=10.0)
    plane = tmp_path / "plane"
    (plane / "action").mkdir(parents=True)
    (plane / "wm01_dataset.json").write_text(json.dumps({
        "clip_frames": CLIP_FRAMES, "target_fps": TARGET_FPS,
        "vocab_fingerprint": ActionSpace().fingerprint}))
    _write_pseudo(chunks, tmp_path / "pseudo")
    with pytest.raises(ValueError, match="上采样"):
        ab_bridge.convert(chunks, tmp_path / "pseudo", plane)


def test_bridge_refuses_missing_pseudo_and_stale_plane(tmp_path):
    chunks = _corpus(tmp_path / "chunks", sessions=("s00",), segments=2,
                     frames=40)
    plane = tmp_path / "plane"
    _build_native_plane(chunks, plane)
    with pytest.raises(SystemExit, match="缺伪标签"):
        ab_bridge.convert(chunks, tmp_path / "nowhere", plane)

    # 旧口径平面(target_fps=16 时代)必须重建, 不迁就
    (plane / "wm01_dataset.json").write_text(json.dumps({
        "clip_frames": CLIP_FRAMES, "target_fps": 16,
        "vocab_fingerprint": ActionSpace().fingerprint}))
    _write_pseudo(chunks, tmp_path / "pseudo")
    with pytest.raises(SystemExit, match="target_fps"):
        ab_bridge.convert(chunks, tmp_path / "pseudo", plane)

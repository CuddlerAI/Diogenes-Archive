"""depth↔RGB 时间对齐闸的回归测试 —— 锁住 best_k 判别与三道阈值的行为。

为什么有这个文件：CP2077 v7 包 20 项格式全过、仍被接收方以重影拒收（#801），
根因是 50% 槽位的深度贴合的是相邻帧而非标称帧。本闸是拦这一类的唯一内容判据，
阈值改动必须显式过测试，不能静默改判。

测试全部用合成帧（`scan` 直接吃 ndarray 字典），不依赖视频文件、EXR 编解码器。
场景是「移动的亮条 + 反向移动的方块」：RGB 与 depth 共享结构边缘（这正是真实
数据里 NCC 判别成立的机制），底噪静止、对所有 k 一视同仁，不会替错位背书。
"""
from __future__ import annotations

import sys
from pathlib import Path

import numpy as np

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
import depth_rgb_alignment_check as A

W, H = A.RES_W, A.RES_H
N_FRAMES = 60          # 30fps 两秒
SLOTS = range(0, N_FRAMES, 2)

_BG = None


def _bg() -> np.ndarray:
    """静止的弱噪声底图：有纹理但低对比，边缘幅值远小于前景条。"""
    global _BG
    if _BG is None:
        rng = np.random.default_rng(11)
        _BG = rng.integers(95, 106, size=(H, W), dtype=np.uint8)
    return _BG


def _make_frame(t: int) -> tuple[np.ndarray, np.ndarray]:
    """第 t 帧：亮条 x=40+6t 右移，方块 x=380-4t 左移；depth 同几何。

    顶部 20 行 depth 置 NaN——客户口径里天空就是 NaN（docs/121），
    闸必须对合规 NaN 免疫。
    """
    gray = _bg().copy()
    depth = np.full((H, W), 60.0, dtype=np.float32)
    bx = 40 + 6 * t
    gray[:, bx : bx + 40] = 230
    depth[:, bx : bx + 40] = 5.0
    sx = 380 - 4 * t
    gray[120:200, sx : sx + 60] = 30
    depth[120:200, sx : sx + 60] = 10.0
    depth[:20, :] = np.nan
    return gray, depth


def _rgb_all() -> dict[int, np.ndarray]:
    return {t: _make_frame(t)[0] for t in range(N_FRAMES)}


def _depth_at(shift: int = 0) -> dict[int, np.ndarray]:
    """槽位 n 的 depth 取自场景第 n+shift 帧。shift=0 即完美对齐。"""
    return {n: _make_frame(n + shift)[1] for n in SLOTS}


def test_aligned_passes_with_zero_misalignment():
    report = A.scan(_depth_at(0), _rgb_all())
    assert report["pass"], report
    assert report["misaligned"] == 0
    assert report["duplicate_slots"] == 0


def test_shift_one_frame_fails_misaligned_gate():
    """整体晚一帧（33ms）——v7 包最常见的错位形态，必须拦下。"""
    report = A.scan(_depth_at(1), _rgb_all())
    assert not report["pass_misaligned"], report
    assert not report["pass"]
    # 晚一帧不是「远」错位，远闸不背这口锅
    assert report["far_misaligned"] == 0


def test_shift_two_frames_also_fails_far_gate():
    report = A.scan(_depth_at(2), _rgb_all())
    assert not report["pass_misaligned"]
    assert not report["pass_far"], report
    assert not report["pass"]


def test_borrowed_slots_fail_duplicate_gate():
    """16Hz→15fps 借帧形态：每两个槽位共用一张 depth（逐字节相同）。"""
    depth = {}
    for i, n in enumerate(SLOTS):
        depth[n] = depth[n - 2].copy() if i % 2 == 1 else _make_frame(n)[1]
    report = A.scan(depth, _rgb_all())
    assert not report["pass_duplicates"], report
    # 被借的第二个槽位内容属于两帧前 → 也应计入错位
    assert not report["pass_misaligned"]
    assert not report["pass"]


def test_static_scene_is_not_misjudged():
    """静止段两边对所有 k 都分不出高下（margin≈0）——不得计错、不得误杀。

    depth 加逐槽位微噪声：真实静止场景也不会逐字节相同（NPC 微动/精度抖动），
    重复闸只该抓「同一张图被复制进多个槽位」，不该抓「场景很静」。
    """
    gray0, depth0 = _make_frame(0)
    rgb = {t: gray0 for t in range(N_FRAMES)}
    rng = np.random.default_rng(3)
    depth = {
        n: depth0 + rng.normal(0, 0.001, size=depth0.shape).astype(np.float32)
        for n in SLOTS
    }
    report = A.scan(depth, rgb)
    assert report["pass"], report
    assert report["misaligned"] == 0
    assert report["duplicate_slots"] == 0

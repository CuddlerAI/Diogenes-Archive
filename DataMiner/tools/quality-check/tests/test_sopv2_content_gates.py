"""SOP v2 内容闸的回归测试 —— 锁住 B 项判别量与交付窗偏移。

为什么有这个文件：`tools/quality-check/` 此前**完全没有 CI 覆盖**，而 B 项的绿掩膜
阈值是照 FO4 室内调的，直到 f3cf5bab（Diamond City，室外绿漆铁皮屋 + 绿霓虹招牌）
才暴露出整条被误判 5.1% 帧有 HUD。阈值类判据没有测试 = 下一次改动会静默改判。

测试全部用合成帧（`metrics_from_frames` 直接吃 ndarray），不依赖任何视频文件或编解码器。
"""
from __future__ import annotations

import sys
from pathlib import Path

import numpy as np
import pytest

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
import sopv2_video_check as S

W, H = 640, 360
N = 120
GREEN = (0, 255, 0)          # BGR，满足 green_mask 的 g>110 / g-r>55 / g-b>55


def _background(seed: int = 7) -> np.ndarray:
    """灰度噪声底图：有纹理供相位相关锁定，且不含绿色。"""
    rng = np.random.default_rng(seed)
    n = rng.integers(40, 210, size=(H * 2, W * 2), dtype=np.uint8)
    return np.repeat(n[:, :, None], 3, axis=2)


def _scene(pan_px: int, *, wall: bool, hud: bool, n: int = N):
    """生成 n 帧：底图按 pan_px/帧 平移；`wall` 是随画面一起动的绿墙；
    `hud` 是钉在屏幕坐标上的绿准星。

    ⚠️ HUD 画成**细结构**（准星十字 + 血条），不是实心大色块——这是判别量的固有性质：
    实心大块平移后与自己大面积重叠，`move` 命中内部，只有前缘那几列投得出票。
    真实 HUD（准星、血条、罗盘刻度、文字）本来就是细的，所以这条不影响实用；
    而「实心大面积绿」那一类正是菜单/控制台，另走「世界冻住」判据。
    """
    bg = _background()
    if wall:                                     # 绿墙画进底图 → 随平移一起走
        bg[H // 2: H // 2 + 160, W // 2: W // 2 + 260] = GREEN
    x0, y0, x1, y1 = S.HUD_ROI["crosshair"]
    cx, cy = int((x0 + x1) / 2 * W), int((y0 + y1) / 2 * H)
    bx0, by0, bx1, by1 = S.HUD_ROI["hp"]
    for i in range(n):
        off = (i * pan_px) % W
        f = bg[0:H, off:off + W].copy()
        if hud:                                  # 每帧同一屏幕坐标 → 不随画面动
            f[cy - 18:cy + 18, cx - 1:cx + 2] = GREEN      # 准星竖线（3 px 宽）
            f[cy - 1:cy + 2, cx - 18:cx + 19] = GREEN      # 准星横线
            f[int(by0 * H) + 6:int(by0 * H) + 10,
              int(bx0 * W) + 4:int(bx1 * W) - 4] = GREEN   # 血条（4 px 高）
        yield f


def _run(gen):
    return S.metrics_from_frames(gen, W, H)


def test_panning_green_wall_is_not_hud():
    """绿色景物随画面走 → 投不出屏幕固定绿，B HUD 应判通过。"""
    metrics, _, _, hud = _run(_scene(6, wall=True, hud=False))
    assert hud["judgeable"] >= S.HUD_MIN_JUDGEABLE, "合成场景应当帧帧可判"
    assert hud["mask_px"] == 0
    assert hud["ppm"] < S.HUD_MIN_PPM
    # 同一批帧在**旧口径**（只看绿像素计数）下是会误报的——这正是本次要修的病
    assert any(r["hud_total"] > S.HUD_GREEN_MIN for r in metrics)


def test_screen_fixed_green_is_hud():
    """钉在屏幕坐标上的绿块 → 必须判出 HUD，且逐帧覆盖率标满。"""
    _, _, _, hud = _run(_scene(6, wall=False, hud=True))
    assert hud["ppm"] >= S.HUD_MIN_PPM
    on = [c for c in hud["cover"] if c > S.HUD_FRAME_COVER]
    assert len(on) >= N - 2, f"应几乎每帧都检出，实际 {len(on)}/{N}"


def test_hud_still_found_behind_green_scenery():
    """绿墙与 HUD 同时存在时，不能被景物盖过去。"""
    _, _, _, hud = _run(_scene(6, wall=True, hud=True))
    assert hud["ppm"] >= S.HUD_MIN_PPM


def test_static_scene_is_not_judgeable():
    """画面不动时「随画面动」与「钉在屏幕上」无法区分——应判不出，交人工。"""
    _, _, _, hud = _run(_scene(0, wall=True, hud=True))
    assert hud["judgeable"] == 0
    assert hud["judgeable"] < S.HUD_MIN_JUDGEABLE


def _is_menu(rec, area):
    return (1e6 * rec["menu_green"] / area > S.MENU_GREEN_MIN_PPM
            and "mad" in rec and rec["mad"] < S.STATIC_MAD)


def test_menu_requires_frozen_world():
    """菜单判据 = 大面积绿 **且世界冻住**；绿墙在动的帧不算菜单。"""
    area = W * H
    moving, _, _, _ = _run(_scene(6, wall=True, hud=False))
    assert any(1e6 * r["menu_green"] / area > S.MENU_GREEN_MIN_PPM for r in moving), \
        "绿墙面积应当越过菜单的面积闸（否则这条测试没测到东西）"
    assert not any(_is_menu(r, area) for r in moving), "画面在动 = 景物，不是菜单"

    frozen, _, _, _ = _run(_scene(0, wall=True, hud=False))
    assert any(_is_menu(r, area) for r in frozen), "冻住 + 大面积绿 = 菜单/控制台"


# --- C 鼠标微扰：交付窗不从 session_frame 0 开始时的取数偏移 ------------------

def _minimal_metrics(n):
    recs = []
    for i in range(n):
        r = {"frame": i, "hud_total": 0, "menu_green": 0}
        if i:
            r.update(mad=5.0, dx=2.0, dy=0.0, pc_resp=0.9, resid=1.0, resid_frac=0.05)
        recs.append(r)
    return recs


def _seg(start, n):
    return {"segment_index": start // n, "n_frames": n, "session_frame_start": start,
            "width": 1920, "height": 1080, "fps": 30.0, "mode": "certified",
            "non_trainable": False, "duplicate_frames": 0, "queue_drops": 0}


def _ptr_pct(base, inputs, n=120):
    res = S.evaluate([_seg(base, n)], _minimal_metrics(n), [], None, None, inputs,
                     1920, 1080, n, 30.0,
                     {"mask_px": 0, "ppm": 0.0, "judgeable": 999, "cover": []})
    row = next(r for r in res if r.item.startswith("C 鼠标持续微扰"))
    return row.status


@pytest.mark.parametrize("base", [0, 300, 9000])
def test_pointer_lookup_uses_session_frame_base(base):
    """输入流按 session_frame 建索引；交付窗从非 0 帧起时必须整体平移。

    回归的 bug：`evaluate` 用 `range(n_win)` 直接取 `inputs`，窗口从 f300 起时
    读的是 f0–1799 的输入。f3cf5bab 实测偏差 13.4 个百分点。
    """
    n = 120
    moving = {base + f: (5, 0, []) for f in range(n)}        # 窗内每帧都有鼠标位移
    assert _ptr_pct(base, moving) == "通过"

    # 只有窗口**之外**有位移时，必须判不通过（旧代码在 base=300 时会误判通过）
    elsewhere = {f: (5, 0, []) for f in range(base)} if base else {}
    if base:
        assert _ptr_pct(base, elsewhere) == "不通过"

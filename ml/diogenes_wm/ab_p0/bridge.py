"""AB-P0 第 3 步 · 伪标签经 wm01 的同一重采样纪律进训练平面.

    python -m diogenes_wm.ab_p0.bridge --chunks <chunk 契约根> \
        --pseudo <伪标签根> --plane <wm01 训练平面> [--mouse-source native]

产物: `<plane>/action_pseudo/<session>/<stem>.npz`, 与 native 的
`<plane>/action/...` 逐 stem 对齐:

    keyboard             [T,6]  f32   覆盖比例(与 native 同一纪律, 不是 OR)
    mouse                [T,2]  f32   位移逐窗口求和(不抽样)
    keyboard_confidence  [T,6]  f32   c_t 经同一栅格的均值 —— A3 臂的加权来源

A2(均匀)与 A3(c_t 加权)消费同一份文件: A2 无视 confidence, A3 用它加权 ——
两臂之间除了权重没有任何别的差异, 这正是 H6 要的对照形状。

## 为什么这里没有 ffmpeg

视频 clip 由 `wm01.dataset.convert`(A1 建平面时)切好, 三个臂共用 —— 换动作
来源不换画面。本工具只重放 convert 的切窗计划(`wm01.dataset.clip_grid`,
同一份实现)并把伪标签压到同一栅格上。

## 三道对齐闸(全部硬失败, 不降级)

1. 平面元数据: `wm01_dataset.json` 的 target_fps / vocab_fingerprint 必须与
   当前代码一致 —— 平面是旧纪律建的就重建平面, 不迁就;
2. 逐 chunk: pseudo.npz 帧数必须与 p0.jsonl 严格相等(伪标注坏了就重导出);
3. 逐 clip + 收尾: 每个 stem 的 native action npz 必须存在且帧数相同,
   结束时两侧 stem 集合必须完全相等 —— 切窗计划哪怕漂移一条 clip 都会在
   这里炸掉, 而不是训练侧静默错位。

30→16/24 的三条纪律(按时间建栅格 / 位移求和 / 拒绝上采样)由
`build_time_grid` + `clip_grid` 继承, 不在此处重新实现。
"""
from __future__ import annotations

import argparse
import json
from pathlib import Path

import numpy as np

from diogenes_phase0.contract import load_actions
from diogenes_phase0.data.dataset import scan_dataset

from ..wm01.action_space import (BASE_KEYBOARD_DIM, ActionSpace,
                                 scanned_to_wm01)
from ..wm01.dataset import TARGET_FPS, clip_grid, group_runs

MOUSE_SOURCES = ("native", "pseudo")


def load_plane_stats(plane: Path) -> dict:
    p = plane / "wm01_dataset.json"
    if not p.exists():
        raise SystemExit(
            f"{plane} 下没有 wm01_dataset.json —— 先用 diogenes_wm.wm01.dataset "
            "建 native 平面(A1), 伪标签臂共用它的视频 clip")
    stats = json.loads(p.read_text())
    if stats.get("target_fps") != TARGET_FPS:
        raise SystemExit(
            f"平面 target_fps={stats.get('target_fps')} 与当前纪律 {TARGET_FPS} "
            "不符 —— 平面是旧口径建的, 重建平面而不是迁就")
    if stats.get("vocab_fingerprint") != ActionSpace().fingerprint:
        raise SystemExit(
            f"平面词表指纹 {stats.get('vocab_fingerprint')} 与当前 "
            f"{ActionSpace().fingerprint} 不符 —— 通道错位是静默的, 拒绝继续")
    return stats


def _load_pseudo(pseudo_root: Path, rec) -> dict[str, np.ndarray]:
    p = pseudo_root / rec.chunk_dir.parent.name / rec.chunk_dir.name / "pseudo.npz"
    if not p.exists():
        raise SystemExit(f"缺伪标签: {p} —— pseudo_label 没跑全这批 chunk")
    z = np.load(p)
    if len(z["keys"]) != rec.n_frames:
        raise SystemExit(
            f"{p} 有 {len(z['keys'])} 帧, 源 p0.jsonl 有 {rec.n_frames} 帧 —— "
            "伪标注与源不 1:1, 重导出而不是截断")
    return {k: z[k] for k in ("keys", "keys_conf", "pointer_from_bins")}


def encode_pseudo(kb6: np.ndarray, conf6: np.ndarray, mouse: np.ndarray,
                  grid) -> dict[str, np.ndarray]:
    """伪标签压到目标栅格: 键盘/置信度取窗口均值, 位移求和(与 encode_actions 同纪律)."""
    kb = np.zeros((grid.out_frames, BASE_KEYBOARD_DIM), np.float32)
    cf = np.zeros((grid.out_frames, BASE_KEYBOARD_DIM), np.float32)
    mo = np.zeros((grid.out_frames, 2), np.float32)
    for i, (lo, hi) in enumerate(grid.source_index):
        hi = min(hi, len(kb6))
        if hi <= lo:
            continue
        kb[i] = kb6[lo:hi].mean(axis=0)      # 覆盖比例, 不是 OR
        cf[i] = conf6[lo:hi].mean(axis=0)
        mo[i] = mouse[lo:hi].sum(axis=0)     # 位移求和, 绝不抽样
    return {"keyboard": kb, "mouse": mo, "keyboard_confidence": cf}


def convert(chunks: Path, pseudo: Path, plane: Path, *,
            arm_dir: str = "action_pseudo",
            mouse_source: str = "native") -> dict:
    if mouse_source not in MOUSE_SOURCES:
        raise ValueError(f"mouse_source {mouse_source!r} 不在 {MOUSE_SOURCES}")
    stats_native = load_plane_stats(plane)
    clip_frames = int(stats_native["clip_frames"])

    recs = scan_dataset(chunks)
    if not recs:
        raise SystemExit(f"{chunks} 下没有可用 chunk")
    runs = group_runs(recs)

    written: set[str] = set()
    conf_sum = np.zeros(BASE_KEYBOARD_DIM)
    cover_sum = np.zeros(BASE_KEYBOARD_DIM)
    n_out_frames = 0

    for run in runs:
        fps_values = {r.meta.fps for r in run.records}
        if len(fps_values) != 1:
            raise ValueError(
                f"{run.session}/{run.run_id} 混合帧率 {sorted(fps_values)}")
        src_fps = fps_values.pop()

        acts, kb6_parts, cf6_parts, mouse_parts = [], [], [], []
        for r in run.records:
            a = load_actions(r.actions_path)
            z = _load_pseudo(pseudo, r)
            n = min(len(a), len(z["keys"]))
            acts.extend(a[:n])
            kb6_parts.append(scanned_to_wm01(z["keys"][:n]).astype(np.float32))
            cf6_parts.append(scanned_to_wm01(z["keys_conf"][:n]))
            if mouse_source == "pseudo":
                mouse_parts.append(z["pointer_from_bins"][:n])
        kb6 = np.concatenate(kb6_parts)
        cf6 = np.concatenate(cf6_parts)
        if mouse_source == "pseudo":
            mouse = np.concatenate(mouse_parts).astype(np.float32)
        else:
            # 与 encode_actions/space.mouse_delta 同一路: 未截断 raw
            mouse = np.asarray([[a.raw_dx, a.raw_dy] for a in acts], np.float32)

        n_src_total = len(kb6)
        src_per_clip = int(np.ceil(clip_frames * src_fps / TARGET_FPS))
        n_clips = n_src_total // src_per_clip
        for c in range(n_clips):
            lo = c * src_per_clip
            grid = clip_grid(src_per_clip, src_fps, clip_frames)
            if grid is None:
                continue
            enc = encode_pseudo(kb6[lo:lo + src_per_clip],
                                cf6[lo:lo + src_per_clip],
                                mouse[lo:lo + src_per_clip], grid)
            stem = f"{run.run_id}-{c:04d}"
            rel = f"{run.session}/{stem}.npz"
            native_p = plane / "action" / rel
            if not native_p.exists():
                raise SystemExit(
                    f"native 平面缺 {native_p} —— 切窗计划与 A1 建平面时不一致, "
                    "或喂的 chunk 根不是建平面用的那份")
            native_kb = np.load(native_p)["keyboard"]
            if native_kb.shape != enc["keyboard"].shape:
                raise SystemExit(
                    f"{rel}: 伪标签 {enc['keyboard'].shape} vs native "
                    f"{native_kb.shape} —— 帧数错位, 拒绝写出")
            dst = plane / arm_dir / rel
            dst.parent.mkdir(parents=True, exist_ok=True)
            np.savez_compressed(dst, **enc)
            written.add(rel)
            conf_sum += enc["keyboard_confidence"].sum(axis=0)
            cover_sum += enc["keyboard"].sum(axis=0)
            n_out_frames += enc["keyboard"].shape[0]

    native_stems = {str(p.relative_to(plane / "action"))
                    for p in (plane / "action").rglob("*.npz")}
    if native_stems != written:
        raise SystemExit(
            f"stem 集合不相等: 只在 native {sorted(native_stems - written)[:3]} / "
            f"只在伪标签 {sorted(written - native_stems)[:3]} —— 两臂消费的 clip "
            "必须完全相同")

    space = ActionSpace()
    stats = {
        "clips": len(written),
        "out_frames": n_out_frames,
        "clip_frames": clip_frames,
        "target_fps": TARGET_FPS,
        "mouse_source": mouse_source,
        "vocab_fingerprint": space.fingerprint,
        "pseudo_root": str(pseudo),
        "channels": [
            {"channel": name,
             "mean_coverage": round(float(cover_sum[i]) / max(n_out_frames, 1), 4),
             "mean_confidence": round(float(conf_sum[i]) / max(n_out_frames, 1), 4)}
            for i, name in enumerate(space.keyboard)],
    }
    (plane / f"{arm_dir}_bridge.json").write_text(
        json.dumps(stats, indent=1, ensure_ascii=False) + "\n")
    return stats


def main() -> None:
    ap = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--chunks", type=Path, required=True, help="chunk 契约根")
    ap.add_argument("--pseudo", type=Path, required=True, help="pseudo_label 产物根")
    ap.add_argument("--plane", type=Path, required=True, help="wm01 训练平面")
    ap.add_argument("--arm-dir", default="action_pseudo")
    ap.add_argument("--mouse-source", default="native", choices=MOUSE_SOURCES,
                    help="native(默认): 35 §3.3 的 (帧,键) 设计, 只换键盘来源; "
                         "pseudo: 鼠标也用 bin 头解码(完全无真值的放大器情形)")
    a = ap.parse_args()
    stats = convert(a.chunks, a.pseudo, a.plane, arm_dir=a.arm_dir,
                    mouse_source=a.mouse_source)
    print(json.dumps(stats, indent=1, ensure_ascii=False))
    print(f"→ {a.plane / a.arm_dir}")


if __name__ == "__main__":
    main()

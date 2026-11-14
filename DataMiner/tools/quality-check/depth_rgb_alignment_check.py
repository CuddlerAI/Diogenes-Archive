#!/usr/bin/env python3
"""depth↔RGB 时间对齐自检：深度内容是否贴合自己标称的视频帧（重影硬闸）。

客户脚本 quality_check_depth_dataset.py 只查格式，engine_qc 查数值域，都不回答
「depth_N 里的场景是不是 video 第 N 帧的场景」。CP2077 v7 包 20 项格式全过、
仍被接收方以重影拒收（#801）：50% 槽位的深度实际贴合相邻 ±1~2 帧。此闸从内容上
把这一类拦下。

方法：每个 15fps 槽位 n，把 depth 边缘图与 RGB 第 n+k 帧（k∈±4）边缘图做归一化
互相关，取最佳偏移 best_k。深度边缘与纹理边缘不是一回事，但结构边缘（人、柱、
路缘）两边共享，相对比较（哪个 k 相关性最高）成立。静止/弱纹理段两边都分不出
高下（margin 小），**不计入错位**——所以本闸天然只在「看得出错」的地方计错，
不会把静止段误杀。

判定（默认阈值，按 v7 坏包实测 50.3%/9.1%/7.7% 与对齐段实测 ~0 之间取）：
  - 错位槽位（best_k≠0 且领先 corr@0 超 MARGIN）占比 ≤ 5%
  - 远错位槽位（|best_k|≥2，同 margin 口径）占比 ≤ 1%
  - 重复深度槽位（与其他槽位内容逐字节相同）占比 ≤ 2%

依赖 OpenCV（与客户脚本同一个 qcenv）。用法：
    qcenv/bin/python depth_rgb_alignment_check.py <数据目录> [--json]
数据目录需含 video.mp4 与 depth.zip（或已解开的 depth/ 目录）。
退出码：0=通过，1=不通过，2=输入错误。

内存量级：每视频帧留一张 480×270 uint8 边缘图，60s/1800 帧 ≈ 230 MB。
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import sys
import tempfile
import zipfile
from pathlib import Path, PurePosixPath

# 必须在 import cv2 之前：部分 OpenCV 版本首次读配置后就缓存，事后设不生效。
os.environ.setdefault("OPENCV_IO_ENABLE_OPENEXR", "1")

import cv2
import numpy as np

# 工作分辨率。再小 ±1 帧的位移就沉到亚像素里分不出来了。
RES_W, RES_H = 480, 270
# 搜索的偏移范围（视频帧数）。v7 实测离群最远 ±4。
K_RANGE = range(-4, 5)
# 「明确错位」的领先余量：best 相关性至少比 corr@0 高这么多才计数。
MARGIN = 0.01
# 三道闸的默认阈值（%）。
MAX_MISALIGNED_PCT = 5.0
MAX_FAR_PCT = 1.0
MAX_DUP_PCT = 2.0

_DEPTH_NAME = re.compile(r"^depth_(\d+)\.exr$")


def rgb_edge_map(gray: np.ndarray) -> np.ndarray:
    """灰度帧 → uint8 边缘幅值图（p99 归一化，抗亮度差）。输入须已是工作分辨率。"""
    gx = cv2.Sobel(gray, cv2.CV_32F, 1, 0, ksize=3)
    gy = cv2.Sobel(gray, cv2.CV_32F, 0, 1, ksize=3)
    m = np.sqrt(gx * gx + gy * gy)
    scale = max(float(np.percentile(m, 99)), 1e-6)
    return np.clip(m / scale * 255.0, 0, 255).astype(np.uint8)


def depth_edge_map(depth: np.ndarray) -> np.ndarray:
    """深度图 → uint8 边缘幅值图。log1p 压动态范围，天空 NaN（客户口径）置 0。"""
    d = np.nan_to_num(depth, nan=0.0, posinf=0.0, neginf=0.0)
    ld = np.log1p(np.maximum(d, 0.0))
    if ld.shape != (RES_H, RES_W):
        ld = cv2.resize(ld, (RES_W, RES_H), interpolation=cv2.INTER_AREA)
    return rgb_edge_map(ld)


def ncc(a: np.ndarray, b: np.ndarray) -> float:
    """去均值归一化互相关。任一侧零方差（纯平）→ 0，不让除零变成假高分。"""
    af = a.astype(np.float32).ravel()
    bf = b.astype(np.float32).ravel()
    af -= af.mean()
    bf -= bf.mean()
    den = float(np.sqrt((af * af).sum() * (bf * bf).sum()))
    return float((af * bf).sum() / den) if den > 0 else 0.0


def scan(depth_slots: dict[int, np.ndarray], rgb_gray: dict[int, np.ndarray]) -> dict:
    """纯逻辑核心：深度槽位 {帧号: float32 深度图} × 视频 {帧号: uint8 灰度帧} → 报告。

    测试直接注入合成 ndarray，不需要视频文件或 EXR 编解码。
    """
    rgb_edges = {n: rgb_edge_map(g) for n, g in rgb_gray.items()}

    digests: dict[int, str] = {}
    rows = []
    for n in sorted(depth_slots):
        d = depth_slots[n]
        digests[n] = hashlib.md5(d.tobytes()).hexdigest()
        de = depth_edge_map(d)
        scores = {k: ncc(de, rgb_edges[n + k]) for k in K_RANGE if n + k in rgb_edges}
        if not scores:
            continue
        best_k = max(scores, key=scores.get)  # type: ignore[arg-type]
        margin = scores[best_k] - scores.get(0, 0.0)
        rows.append(
            {
                "frame": n,
                "best_k": best_k,
                "best_corr": round(scores[best_k], 4),
                "corr_at_0": round(scores.get(0, 0.0), 4),
                "margin_over_0": round(margin, 4),
            }
        )

    total = len(rows)
    misaligned = [r for r in rows if r["best_k"] != 0 and r["margin_over_0"] > MARGIN]
    far = [r for r in misaligned if abs(r["best_k"]) >= 2]

    by_digest: dict[str, list[int]] = {}
    for n, h in digests.items():
        by_digest.setdefault(h, []).append(n)
    dup_slots = sorted(n for ns in by_digest.values() if len(ns) > 1 for n in ns[1:])

    def pct(part: int) -> float:
        return part / total * 100.0 if total else 0.0

    summary = {
        "total_slots": total,
        "misaligned": len(misaligned),
        "misaligned_pct": round(pct(len(misaligned)), 2),
        "far_misaligned": len(far),
        "far_misaligned_pct": round(pct(len(far)), 2),
        "duplicate_slots": len(dup_slots),
        "duplicate_pct": round(pct(len(dup_slots)), 2),
        "worst": sorted(rows, key=lambda r: -r["margin_over_0"])[:10],
        "duplicate_frames": dup_slots[:40],
    }
    summary["pass_misaligned"] = summary["misaligned_pct"] <= MAX_MISALIGNED_PCT
    summary["pass_far"] = summary["far_misaligned_pct"] <= MAX_FAR_PCT
    summary["pass_duplicates"] = summary["duplicate_pct"] <= MAX_DUP_PCT
    summary["pass"] = bool(
        summary["pass_misaligned"] and summary["pass_far"] and summary["pass_duplicates"]
    )
    summary["rows"] = rows
    return summary


def _load_depth_slots(root: Path, tmp: Path) -> dict[int, np.ndarray]:
    """从 depth/ 目录或 depth.zip 读全部 EXR 为 float32 单通道。"""
    exr_dir = root / "depth"
    if not exr_dir.is_dir():
        zip_path = root / "depth.zip"
        if not zip_path.exists():
            raise FileNotFoundError(f"{root} 下既无 depth/ 也无 depth.zip")
        with zipfile.ZipFile(zip_path) as zf:
            for name in zf.namelist():
                base = PurePosixPath(name).name
                if _DEPTH_NAME.match(base):
                    (tmp / base).write_bytes(zf.read(name))
        exr_dir = tmp

    slots: dict[int, np.ndarray] = {}
    for p in exr_dir.iterdir():
        m = _DEPTH_NAME.match(p.name)
        if not m:
            continue
        d = cv2.imread(str(p), cv2.IMREAD_UNCHANGED)
        if d is None:
            raise ValueError(f"读不了 {p}")
        slots[int(m.group(1))] = (d if d.ndim == 2 else d[:, :, 0]).astype(np.float32)
    if not slots:
        raise FileNotFoundError(f"{exr_dir} 下没有 depth_N.exr")
    return slots


def _load_video_gray(root: Path) -> dict[int, np.ndarray]:
    video = root / "video.mp4"
    if not video.exists():
        raise FileNotFoundError(f"找不到 {video}")
    cap = cv2.VideoCapture(str(video))
    frames: dict[int, np.ndarray] = {}
    i = 0
    while True:
        ok, img = cap.read()
        if not ok:
            break
        g = cv2.cvtColor(
            cv2.resize(img, (RES_W, RES_H), interpolation=cv2.INTER_AREA),
            cv2.COLOR_BGR2GRAY,
        )
        frames[i] = g
        i += 1
    cap.release()
    if not frames:
        raise ValueError(f"{video} 解不出帧")
    return frames


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("dataset", help="含 video.mp4 与 depth.zip（或 depth/）的目录")
    parser.add_argument("--json", action="store_true", help="输出机器可读 JSON")
    args = parser.parse_args()

    root = Path(args.dataset).expanduser().resolve()
    if not root.is_dir():
        print(f"{root} 不是目录", file=sys.stderr)
        return 2

    try:
        with tempfile.TemporaryDirectory() as tmp:
            depth_slots = _load_depth_slots(root, Path(tmp))
            rgb = _load_video_gray(root)
            report = scan(depth_slots, rgb)
    except (FileNotFoundError, ValueError, zipfile.BadZipFile) as e:
        print(f"输入错误：{e}", file=sys.stderr)
        return 2

    if args.json:
        print(json.dumps(report, ensure_ascii=False, indent=2))
        return 0 if report["pass"] else 1

    rows = [
        (
            "错位槽位（best_k≠0 且 margin>%.2f）" % MARGIN,
            f"≤ {MAX_MISALIGNED_PCT:.1f}%",
            f"{report['misaligned']}/{report['total_slots']} = {report['misaligned_pct']:.2f}%",
            report["pass_misaligned"],
        ),
        (
            "远错位槽位（|best_k|≥2）",
            f"≤ {MAX_FAR_PCT:.1f}%",
            f"{report['far_misaligned']}/{report['total_slots']} = {report['far_misaligned_pct']:.2f}%",
            report["pass_far"],
        ),
        (
            "重复深度槽位（内容逐字节相同）",
            f"≤ {MAX_DUP_PCT:.1f}%",
            f"{report['duplicate_slots']}/{report['total_slots']} = {report['duplicate_pct']:.2f}%",
            report["pass_duplicates"],
        ),
    ]
    print("| 检查项 | 标准 | 实测 | 是否通过 |")
    print("|---|---|---|---|")
    for item, std, actual, ok in rows:
        print(f"| {item} | {std} | {actual} | {'通过' if ok else '不通过'} |")
    if not report["pass"]:
        print()
        print("错位最重的槽位（margin 降序）：")
        for r in report["worst"]:
            print(
                f"  depth_{r['frame']}: best_k={r['best_k']:+d} "
                f"corr@0={r['corr_at_0']:.3f} best={r['best_corr']:.3f} "
                f"margin={r['margin_over_0']:.3f}"
            )
        if report["duplicate_frames"]:
            print(f"重复槽位（前 40）：{report['duplicate_frames']}")
    print()
    print("对齐结论：通过。" if report["pass"] else "对齐结论：不通过。")
    if not report["pass"]:
        print(
            "提示：错位=采集或配对的时间问题（16Hz vs 15fps 网格 / QPC 语义），见 #801。"
        )
    return 0 if report["pass"] else 1


if __name__ == "__main__":
    raise SystemExit(main())

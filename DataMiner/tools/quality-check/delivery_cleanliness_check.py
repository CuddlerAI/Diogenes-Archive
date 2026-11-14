#!/usr/bin/env python3
"""交付洁净度自检 —— 补客户脚本与 SOP v2 都没有的两个闸：**音频**和**水印**。

为什么单开一个：68 号 §7.1「video 文件验收」定性参数表里「视频声音」是一整行硬要求
（有声音、声音连续、能听清游戏原声、声画同步），而客户 oracle
`quality_check_depth_dataset.py` **全文 0 处涉及音频**，`sopv2_video_check.py` 的 A–F
也没有音频项和水印项。于是「22 项全过 + 内容闸全过」的包，仍然可能是一条**默片 + 带水印**
的片子——2026-08-13 那批 9 条正是如此（9/9 静音、9/9 带水印）。

两个判据：

1. **音频**（68 号 §7.1）：video.mp4 必须有音轨，且不是数字静音。
   `run_acceptance.sh` 给核验包封的是 `anullsrc` 合成静音轨，实测 −91.0 dB——
   有音轨 ≠ 有声音，所以必须量响度，不能只看 `ffprobe` 有没有 audio stream。
   ⚠️ `DataMiner/AGENTS.md` 第 27 行：`degraded/absent` **不得伪装成完整静音**。
   本闸就是那条规则的机检实现。

2. **水印**（docs/127 §0）：左上 192×8 黑白块条**不得进入最终交付产物**
   （`docs/game-capture-visual-cleanliness-research-2026-08.md` §1.1 已列为禁入项）。
   判据不看颜色看**结构**——水印是二值块条，取样帧的该区域「接近纯黑 + 接近纯白」
   像素占比会逼近 1.0，自然画面几乎不可能。这样不会像旧版 HUD 判据那样被绿色场景骗到。

边界同 docs/128 §7.5：**研发机离线核验**，不在采集链路里，不产出也不改写交付 video.mp4。

    python3 delivery_cleanliness_check.py <数据目录>

退出码 0 = 两闸都过。
"""
from __future__ import annotations

import argparse
import json
import subprocess
import sys
from pathlib import Path

# 数字静音的地板。ffmpeg volumedetect 对全零 PCM 报 -91.0 dB；
# 真实游戏原声即便很轻也远在其上（已交付参考件实测 -42.5 dB）。
SILENCE_FLOOR_DB = -80.0
# 水印几何（采集分辨率口径，按视频宽度等比换算到实际画幅）
WM_W, WM_H = 192, 8
WM_REF_W, WM_REF_H = 1920, 1080
# 二值占比阈值：>0.8 判为水印条
WM_BINARY_FRAC = 0.80
SAMPLE_FRAMES = (100, 500, 900, 1400, 1790)


def probe_audio(video: Path) -> tuple[bool, float | None, str | None]:
    """返回 (有音轨, mean_volume_dB, 错误)。"""
    try:
        streams = subprocess.run(
            ["ffprobe", "-v", "error", "-select_streams", "a",
             "-show_entries", "stream=index", "-of", "json", str(video)],
            capture_output=True, check=True).stdout
        has = bool(json.loads(streams).get("streams"))
    except Exception as exc:
        return False, None, f"ffprobe 失败: {exc}"
    if not has:
        return False, None, None

    try:
        out = subprocess.run(
            ["ffmpeg", "-hide_banner", "-nostats", "-i", str(video),
             "-map", "0:a:0", "-af", "volumedetect", "-f", "null", "-"],
            capture_output=True).stderr.decode("utf-8", "replace")
    except Exception as exc:
        return True, None, f"volumedetect 失败: {exc}"
    for line in out.splitlines():
        if "mean_volume:" in line:
            try:
                return True, float(line.split("mean_volume:")[1].split("dB")[0]), None
            except ValueError:
                pass
    return True, None, "未解析到 mean_volume"


def probe_watermark(video: Path) -> tuple[float | None, int, int, str | None]:
    """返回 (二值占比中位, roi_w, roi_h, 错误)。"""
    try:
        import cv2
        import numpy as np
    except Exception as exc:
        return None, 0, 0, f"OpenCV/numpy 不可用: {exc}"

    cap = cv2.VideoCapture(str(video))
    if not cap.isOpened():
        return None, 0, 0, "OpenCV 打不开视频"
    w = int(cap.get(cv2.CAP_PROP_FRAME_WIDTH))
    h = int(cap.get(cv2.CAP_PROP_FRAME_HEIGHT))
    total = int(round(cap.get(cv2.CAP_PROP_FRAME_COUNT)))
    roi_w = max(1, round(WM_W * w / WM_REF_W))
    roi_h = max(1, round(WM_H * h / WM_REF_H))

    fracs = []
    for f in SAMPLE_FRAMES:
        if total and f >= total:
            continue
        cap.set(cv2.CAP_PROP_POS_FRAMES, f)
        ok, img = cap.read()
        if not ok:
            continue
        roi = cv2.cvtColor(img[0:roi_h, 0:roi_w], cv2.COLOR_BGR2GRAY)
        fracs.append(float(((roi < 40) | (roi > 215)).mean()))
    cap.release()
    if not fracs:
        return None, roi_w, roi_h, "取不到样帧"
    return float(np.median(fracs)), roi_w, roi_h, None


def main() -> int:
    ap = argparse.ArgumentParser(description="交付洁净度自检：音频 + 水印")
    ap.add_argument("dataset_dir", type=Path)
    args = ap.parse_args()
    video = args.dataset_dir.expanduser().resolve() / "video.mp4"
    if not video.is_file():
        print(f"找不到 video.mp4: {video}", file=sys.stderr)
        return 2

    rows = []

    has_audio, mean_db, aerr = probe_audio(video)
    if aerr:
        rows.append(("音频存在且非静音", "68号§7.1「有声音、能听清游戏原声」",
                     aerr, False))
    elif not has_audio:
        rows.append(("音频存在且非静音", "68号§7.1「有声音、能听清游戏原声」",
                     "无音轨", False))
    elif mean_db is None:
        rows.append(("音频存在且非静音", "68号§7.1「有声音、能听清游戏原声」",
                     "有音轨但响度未知", False))
    else:
        ok = mean_db > SILENCE_FLOOR_DB
        note = "" if ok else "（数字静音；AGENTS.md 禁止 absent 伪装成完整静音）"
        rows.append(("音频存在且非静音", f"mean_volume > {SILENCE_FLOOR_DB:.0f} dB",
                     f"{mean_db:.1f} dB{note}", ok))

    frac, rw, rh, werr = probe_watermark(video)
    if werr:
        rows.append((f"无 {WM_W}×{WM_H} 水印条", "docs/127：交付产物不得含水印",
                     werr, False))
    else:
        ok = frac < WM_BINARY_FRAC
        rows.append((f"无 {WM_W}×{WM_H} 水印条", "docs/127：交付产物不得含水印",
                     f"左上 {rw}×{rh} 二值占比中位 {frac:.3f}"
                     f"（阈值 {WM_BINARY_FRAC}）", ok))

    print("| 检查项 | 标准 | 实测 | 是否通过 |")
    print("|---|---|---|---|")
    for item, std, actual, ok in rows:
        print(f"| {item} | {std} | {actual} | {'通过' if ok else '不通过'} |")

    all_ok = all(ok for *_, ok in rows)
    print()
    print("交付洁净度结论：通过。" if all_ok
          else "交付洁净度结论：不通过。"
               f"硬性不通过项：{'、'.join(i for i, *_, ok in rows if not ok)}")
    return 0 if all_ok else 1


if __name__ == "__main__":
    raise SystemExit(main())

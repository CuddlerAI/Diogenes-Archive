#!/usr/bin/env python3
"""动态物体检测 —— ⚠️ **当前实现未通过目检验证，不要用它的数字下结论**。

**2026-08-12 实测结论：本方法在真实数据上不可用。** 在 222e8f46 上它给出「89% 的帧
有独立运动物体」，把掩膜画出来一看，红区压在**遮挡/去遮挡边界**上（柜台沿、柱子边、
墙的交界），不是人；还有整帧 99.8% 全红的自运动补偿失效帧。原因是相机一移动，深度
不连续处的像素会被揭开或遮住，前向 warp 在那里根本没有对应像素，残差全堆在边界。

**要变成可用，至少还缺**：前后向光流一致性校验 + 遮挡掩膜（把 warp 后深度与目标帧
深度不一致的像素剔掉）+ 对补偿失效帧的兜底判据。补齐后必须重新做目检标定再启用。

在那之前，60-30 口径请走**抽帧目检**（本文件的 `--audit-grid` 出网格）。

---

原理（思路本身是对的，缺的是遮挡处理）：

**为什么不能用「帧差」或「补偿全局位移后的残差」当代理**：相机一动，整幅画面都在变，
近处视差、遮挡边缘、镜面高光全都会进残差，和「有个人在走」分不开。之前的代理指标
（resid_frac）在四条数据上给出 36%–93%，数字漂亮但不可用。

这里用**自运动补偿**做真的运动分割：深度给了每个像素的三维位置，里程计给了帧间相对
位姿，于是「纯相机运动应该产生多大的光流」是可以逐像素算出来的：

    X  = K⁻¹ · u · Z                (反投影)
    X' = R · X + t                  (按帧间位姿搬过去)
    u' = K · X' / X'_z              (再投影)
    预测光流 = u' − u

把实际光流减去预测光流，剩下的就是**不随相机运动解释的那部分**——也就是独立运动的
物体。再按连通域面积过滤掉噪声与边缘碎片。

**能测什么、不能测什么**（别混用）：
- ✅ 「动态物体**在移动**」——本方法直接测，对应客户的 30% 口径；
- ❌ 「**有**动态物体」（含站着不动的 NPC、停着的车）——运动分割看不见静止的物体，
  这一项只能靠抽帧目检或语义检测，本脚本不给结论，只输出待目检的帧号。

用法：
    python3 dynamics_check.py <session-dir> [--out DIR] [--window N] [--offset N]
"""
from __future__ import annotations

import argparse
import glob
import json
import math
import sys
from pathlib import Path

import numpy as np
import cv2

sys.path.insert(0, str(Path(__file__).resolve().parent))
from qclib import DEPTH_W as DW, DEPTH_H as DH, DepthBank
SW, SH = 640, 360                 # 计算分辨率（光流按此尺度）
RESID_PX = 2.0                    # 残差光流超过此值（@640 宽）算「不被相机运动解释」
MIN_BLOB_FRAC = 0.004             # 连通域面积下限（占画面比例）≈ 920 px @640×360
MOVING_FRAC_MIN = 0.002           # 一帧里独立运动像素占比超过此值算「有在动的物体」


def main():
    ap = argparse.ArgumentParser(description="自运动补偿的动态物体检测（60-30 口径）")
    ap.add_argument("session_dir", type=Path)
    ap.add_argument("--odom", type=Path, default=None)
    ap.add_argument("--video", type=Path, default=None)
    ap.add_argument("--focal", type=float, required=True, help="depth 像素焦距（sopv2 标定值）")
    ap.add_argument("--window", type=int, default=1800)
    ap.add_argument("--offset", type=int, default=0)
    ap.add_argument("--out", type=Path, default=None)
    args = ap.parse_args()

    sd = args.session_dir
    odom = args.odom or next((p for p in (sd / "odom_full.json", sd / "odom.json") if p.exists()), None)
    video = args.video or next((p for p in (sd / "_sopv2_full/session_full.mp4",
                                            sd / "_sopv2/session_full.mp4") if p.exists()), None)
    if not odom or not video:
        raise SystemExit("找不到 odom*.json 或 session_full.mp4")

    # 两种布局都认：直接给原始 session 目录，或给 run_acceptance 的输出目录（原始件在 raw/ 下）
    zips = sorted(glob.glob(str(sd / "runs/*/clips/*/depth-*.zip")) or
                  glob.glob(str(sd / "raw/runs/*/clips/*/depth-*.zip")))
    if not zips:
        raise SystemExit(f"!! {sd} 下找不到 depth-*.zip（试过 runs/ 与 raw/runs/）")
    bank = DepthBank(zips)
    P = {p["frame"]: p for p in json.loads(odom.read_text())["poses"]}

    f = args.focal
    sx = SW / DW
    K = np.array([[f * sx, 0, (DW - 1) / 2 * sx], [0, f * sx, (DH - 1) / 2 * sx], [0, 0, 1]])
    uu, vv = np.meshgrid(np.arange(SW, dtype=np.float32), np.arange(SH, dtype=np.float32))

    lo, hi = args.offset, args.offset + args.window - 1
    pairs = [(a, a + 2) for a in range(lo + (lo % 2), hi - 1, 2)
             if a in bank.map and a + 2 in bank.map and a in P and a + 2 in P]

    cap = cv2.VideoCapture(str(video))
    need = sorted({x for p in pairs for x in p})
    gray = {}
    i = 0
    while i <= (need[-1] if need else -1):
        ok, fr = cap.read()
        if not ok:
            break
        if i in need:
            gray[i] = cv2.cvtColor(cv2.resize(fr, (SW, SH), interpolation=cv2.INTER_AREA),
                                   cv2.COLOR_BGR2GRAY)
        i += 1
    cap.release()

    dis = cv2.DISOpticalFlow_create(cv2.DISOPTICAL_FLOW_PRESET_MEDIUM)
    recs = []
    for k, (a, b) in enumerate(pairs):
        za, ga, gb = bank.get(a), gray.get(a), gray.get(b)
        if za is None or ga is None or gb is None:
            continue
        Z = cv2.resize(za.astype(np.float32), (SW, SH), interpolation=cv2.INTER_NEAREST)
        Ra, ta = np.array(P[a]["R"]), np.array(P[a]["t"])
        Rb, tb = np.array(P[b]["R"]), np.array(P[b]["t"])
        # 相机→世界；帧间相对（把 a 的点搬到 b 的相机系）
        Rrel = Rb.T @ Ra
        trel = Rb.T @ (ta - tb)

        valid = np.isfinite(Z) & (Z > 0.4) & (Z < 80)
        Zc = np.where(valid, Z, 1.0)
        x = (uu - K[0, 2]) * Zc / K[0, 0]
        y = (vv - K[1, 2]) * Zc / K[1, 1]
        Xw = np.stack([x, y, Zc], -1)
        Xb = Xw @ Rrel.T + trel
        zb = Xb[..., 2]
        okz = valid & (zb > 0.2)
        up = np.where(okz, K[0, 0] * Xb[..., 0] / np.where(okz, zb, 1) + K[0, 2], uu)
        vp = np.where(okz, K[1, 1] * Xb[..., 1] / np.where(okz, zb, 1) + K[1, 2], vv)
        pred = np.stack([up - uu, vp - vv], -1)

        obs = dis.calc(ga, gb, None)
        resid = np.linalg.norm(obs - pred, axis=-1)
        moving = (resid > RESID_PX) & okz

        m8 = (moving.astype(np.uint8)) * 255
        m8 = cv2.morphologyEx(m8, cv2.MORPH_OPEN, np.ones((3, 3), np.uint8))
        n_lab, lab, stats, _ = cv2.connectedComponentsWithStats(m8, 8)
        big = 0
        blobs = []
        for j in range(1, n_lab):
            area = stats[j, cv2.CC_STAT_AREA]
            if area >= MIN_BLOB_FRAC * SW * SH:
                big += area
                blobs.append(int(area))
        recs.append({"frame": a, "moving_frac": float(big / (SW * SH)),
                     "n_blobs": len(blobs), "blobs": sorted(blobs, reverse=True)[:5],
                     "valid_frac": float(okz.mean())})
        if (k + 1) % 100 == 0:
            sys.stderr.write(f"  {k+1}/{len(pairs)}\n")

    out = args.out or (sd / "_dynamics")
    out.mkdir(parents=True, exist_ok=True)
    (out / "dynamics.jsonl").write_text("".join(json.dumps(r) + "\n" for r in recs))

    if not recs:
        raise SystemExit("!! 没有可用帧对（深度/位姿/视频三者的帧号对不上），拒绝给结论")
    mv = [r for r in recs if r["moving_frac"] >= MOVING_FRAC_MIN]
    frac = len(mv) / max(len(recs), 1)
    areas = np.array([r["moving_frac"] for r in recs])
    print(json.dumps({
        "frames_analysed": len(recs),
        "frames_with_moving_object": len(mv),
        "moving_object_frame_frac": round(frac, 4),
        "gate_30pct": "NOT_VALIDATED（本实现未通过目检验证，见文件头；此数不可用于判定）",
        "moving_area_frac_p50": round(float(np.median(areas)), 5),
        "moving_area_frac_p90": round(float(np.percentile(areas, 90)), 5),
        "note": "「有动态物体」（含静止 NPC）本方法测不到，需抽帧目检；此处只判「在移动」",
        "top_frames_for_visual_audit": [r["frame"] for r in
                                        sorted(recs, key=lambda r: -r["moving_frac"])[:12]],
    }, ensure_ascii=False, indent=1))


if __name__ == "__main__":
    main()

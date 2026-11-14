#!/usr/bin/env python3
"""RGB-D 视觉里程计 —— 在没有位姿索引的情况下把相机轨迹重建出来。

**为什么需要它。** Loop-有位移 的标的物是位姿：客户要「首尾帧 location/rotation 非常
相近」。但摄取面上传的 session 只有 6 类件（video / depth.zip / frames / events /
manifest / meta），`depth-lab/camera-pose-index.jsonl` **留在采集机上不上云**；而 FO4
的渲染相机绑定本身还在 #636 重新取证中（docs/122、docs/123，`production_binding_
authorized=false`）。所以从 R2 取回的数据要判闭环，只能自己把轨迹算出来。

**原理。** 深度图给了每帧的度量点云；相邻帧之间用光流建对应，Kabsch + 迭代重加权解
刚体变换，逐段累积成轨迹。焦距由 `sopv2_video_check.py` 的刚性一致性标定给出（片中改过
FOV 就分段传 `--switch-frame`）。

**边界。** 这是**核验用**轨迹，不是交付件：里程计有累积漂移，绝对尺度依赖深度线性化
常数。用它判「11 m 不闭环」这种两个数量级的结论是稳的；用它替代 probe 位姿去交付不行。
逐步残差中位数会打印出来，做质量自检（FO4 室内实测 ~1.4 mm）。

用法：
    python3 rgbd_odometry.py <session-dir> <out.json> [--switch-frame N --f-before F --f-after F]
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
from qclib import (DEPTH_W as DW, DEPTH_H as DH, DepthBank, robust_rigid,
                   rot_angle_deg, unproject as unproj)


def track(g0, g1):
    p0 = cv2.goodFeaturesToTrack(g0, 1200, 0.01, 8)
    if p0 is None:
        return None
    p1, st, _ = cv2.calcOpticalFlowPyrLK(g0, g1, p0, None, winSize=(31, 31), maxLevel=4)
    pb, st2, _ = cv2.calcOpticalFlowPyrLK(g1, g0, p1, None, winSize=(31, 31), maxLevel=4)
    fb = np.linalg.norm(pb - p0, axis=2).ravel()
    good = (st.ravel() == 1) & (st2.ravel() == 1) & (fb < 1.0)
    a, b = p0.reshape(-1, 2)[good], p1.reshape(-1, 2)[good]
    return (a, b) if len(a) >= 40 else None


def main():
    ap = argparse.ArgumentParser(description="从 R2 原始 session 重建相机轨迹")
    ap.add_argument("session_dir", type=Path)
    ap.add_argument("out", type=Path)
    ap.add_argument("--video", type=Path, default=None,
                    help="已拼接的整段视频；缺省用 <session>/_sopv2/session_full.mp4")
    ap.add_argument("--switch-frame", type=int, default=0, help="FOV 变化帧（其前后用不同焦距）")
    ap.add_argument("--f-before", type=float, default=836.4, help="跳变前焦距（depth 像素）")
    ap.add_argument("--f-after", type=float, default=836.4, help="跳变后焦距（depth 像素）")
    ap.add_argument("--window", type=int, default=1800, help="只处理前 N 帧")
    args = ap.parse_args()

    zips = sorted(glob.glob(str(args.session_dir / "runs/*/clips/*/depth-*.zip")))
    bank = DepthBank(zips)
    video = args.video or (args.session_dir / "_sopv2" / "session_full.mp4")
    if not Path(video).exists():
        raise SystemExit(f"找不到拼接视频 {video}；先跑 sopv2_video_check.py 生成")

    frames = sorted(f for f in bank.map if f < args.window)
    if len(frames) < 2:
        raise SystemExit("深度帧不足")

    cap = cv2.VideoCapture(str(video))
    rgb, i, need = {}, 0, set(frames)
    while i <= frames[-1]:
        okf, fr = cap.read()
        if not okf:
            break
        if i in need:
            rgb[i] = cv2.cvtColor(cv2.resize(fr, (DW, DH), interpolation=cv2.INTER_AREA),
                                  cv2.COLOR_BGR2GRAY)
        i += 1
    cap.release()

    focal = lambda f: (args.f_before if f < args.switch_frame else args.f_after)
    Rw, tw = np.eye(3), np.zeros(3)
    poses = [{"frame": frames[0], "t": [0.0, 0.0, 0.0], "R": np.eye(3).tolist(),
              "step_trans": 0.0, "step_rot_deg": 0.0, "resid": 0.0, "inliers": 0}]
    prev = frames[0]
    for cur in frames[1:]:
        step = {"frame": cur, "step_trans": None, "step_rot_deg": None,
                "resid": None, "inliers": 0}
        z0, z1 = bank.get(prev), bank.get(cur)
        g0, g1 = rgb.get(prev), rgb.get(cur)
        if z0 is not None and z1 is not None and g0 is not None and g1 is not None:
            tr = track(g0, g1)
            if tr:
                a, b = tr
                ia, ib = np.round(a).astype(int), np.round(b).astype(int)
                inb = ((ia[:, 0] >= 0) & (ia[:, 0] < DW) & (ia[:, 1] >= 0) & (ia[:, 1] < DH) &
                       (ib[:, 0] >= 0) & (ib[:, 0] < DW) & (ib[:, 1] >= 0) & (ib[:, 1] < DH))
                a, b, ia, ib = a[inb], b[inb], ia[inb], ib[inb]
                za, zb = z0[ia[:, 1], ia[:, 0]], z1[ib[:, 1], ib[:, 0]]
                gd = (np.isfinite(za) & np.isfinite(zb) & (za > 0.4) & (zb > 0.4) &
                      (za < 60) & (zb < 60))
                a, b, za, zb = a[gd], b[gd], za[gd], zb[gd]
                if len(a) >= 40:
                    P, Q = unproj(a, za, focal(prev)), unproj(b, zb, focal(cur))
                    R, t, res, nin = robust_rigid(P, Q)
                    ang = rot_angle_deg(np.eye(3), R)
                    Rrel, trel = R.T, -R.T @ t
                    tw = tw + Rw @ trel
                    Rw = Rw @ Rrel
                    step.update(step_trans=float(np.linalg.norm(trel)),
                                step_rot_deg=ang, resid=res, inliers=nin)
        step["t"], step["R"] = tw.tolist(), Rw.tolist()
        poses.append(step)
        prev = cur

    # qclib.robust_rigid 拟合不出时返回 nan（旧实现用 9.9 哨兵，会污染中位数）
    res = [p["resid"] for p in poses if p.get("resid") and math.isfinite(p["resid"])]
    T = np.array([p["t"] for p in poses])
    summary = {
        "n_poses": len(poses),
        "step_resid_median_m": float(np.median(res)) if res else None,
        "path_length_m": float(np.linalg.norm(np.diff(T, axis=0), axis=1).sum()),
        "max_dist_from_start_m": float(np.linalg.norm(T - T[0], axis=1).max()),
        "end_start_dist_m": float(np.linalg.norm(T[-1] - T[0])),
    }
    summary["end_start_rot_deg"] = rot_angle_deg(np.array(poses[0]["R"]),
                                                 np.array(poses[-1]["R"]))
    args.out.write_text(json.dumps(
        {"f_before": args.f_before, "f_after": args.f_after,
         "switch_frame": args.switch_frame, "summary": summary, "poses": poses}))
    print(json.dumps(summary, indent=1))


if __name__ == "__main__":
    main()

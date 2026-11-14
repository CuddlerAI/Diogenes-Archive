#!/usr/bin/env python3
"""把 R2 原始 session 组装成**核验用**五件套，好让客户脚本能跑起来。

⚠️ **这不是交付件。** 位姿来自 `rgbd_odometry.py` 的视觉里程计重建，不是 probe 的
`camera-pose-index.jsonl`（摄取面不上传该文件，且 FO4 渲染相机绑定仍在 #636 重新取证，
docs/122/123 明写 `production_binding_authorized=false`）。正式交付必须走采集机上的
官方 `depth-lab deliver` + external-conversion 链路。

那它有什么用：客户脚本 `quality_check_depth_dataset.py` 查的 22 项**全是结构项**
（文件命名 / 分辨率 / 帧数 / 帧号连续 / 字段顺序 / 时间戳-帧号一致 / depth 张数与命名 /
15fps depth_name 对账）——这些项与位姿数值的来源无关。所以本脚本产出的包能如实回答
「这条 session 转出来以后，结构上过不过客户那关」，同时让 `loop_closure_check.py`
拿到一条可判的轨迹。

video.mp4 由 ffmpeg 另行产出（缩放到 1920×1080、取前 1800 帧、补静音音轨），
见 `run_acceptance.sh`。

用法：
    python3 build_check_package.py <out-dir> <odom.json> <session-dir> [--focal 836.4]
    python3 build_check_package.py <out-dir> <odom.json> <session-dir> --start 100   # 挑窗

`--start` 是给「录了 60s 以上、要在里面挑一个交付窗」用的（必须偶数）。窗内帧号从 0 重编，
depth 也按窗内帧号重命名，所以出来的包永远是客户脚本要的 `depth_0…depth_1798`。
video.mp4 那侧要自己用同一个 start 取帧，见 `run_acceptance.sh`。
"""
from __future__ import annotations

import argparse
import glob
import json
import math
import os
import subprocess
import sys
import zipfile
from datetime import datetime, timedelta
from pathlib import Path

import numpy as np
import cv2

sys.path.insert(0, str(Path(__file__).resolve().parent))
from qclib import DEPTH_W as DW, DEPTH_H as DH, R_to_quat, exr_to_array, linearize_depth, slerp


def quat_world_to_cam(R_cam_to_world):
    """交付口径的四元数是 world→cam（docs/91 / fo4-depth-projection-conventions）。"""
    return list(R_to_quat(np.asarray(R_cam_to_world).T))


def read_pointer(session_dir: Path):
    out = {}
    for fp in sorted(session_dir.glob("runs/*/clips/*/frames-*.jsonl.zst")):
        txt = subprocess.run(["zstd", "-dc", str(fp)], capture_output=True, check=True).stdout.decode()
        for line in txt.splitlines():
            if line.strip():
                d = json.loads(line)
                out[d["session_frame"]] = (d.get("pointerDx", 0), d.get("pointerDy", 0))
    return out


def main():
    ap = argparse.ArgumentParser(description="组装核验用五件套（非交付件）")
    ap.add_argument("out_dir", type=Path)
    ap.add_argument("odom", type=Path)
    ap.add_argument("session_dir", type=Path)
    ap.add_argument("--focal", type=float, default=836.4, help="depth 像素焦距（sopv2 标定值）")
    ap.add_argument("--frames", type=int, default=1800)
    ap.add_argument("--start", type=int, default=0,
                    help="交付窗起始 session frame，**必须为偶数**（depth 只在偶数帧上）。"
                         "录制长于 60s 时用它挑窗——docs/121 §13 的 593 窗穷举就是靠这个跑的")
    ap.add_argument("--process-name", default="Fallout4.exe")
    args = ap.parse_args()

    S, n30 = args.start, args.frames
    if S % 2:
        raise SystemExit(f"--start 必须是偶数（depth 只在偶数 session frame 上），收到 {S}")
    args.out_dir.mkdir(parents=True, exist_ok=True)

    # ---- 位姿：15fps 里程计插值到每个 30fps 帧 ------------------------------
    od = json.loads(args.odom.read_text())
    P = {p["frame"]: p for p in od["poses"]}
    keys = sorted(P)
    pos, rot = {}, {}
    for f in range(n30):
        sf = S + f                      # 窗内帧号 f ↔ session frame sf
        if sf in P:
            pos[f], rot[f] = np.array(P[sf]["t"]), np.array(P[sf]["R"])
            continue
        lo = max([k for k in keys if k <= sf], default=keys[0])
        hi = min([k for k in keys if k >= sf], default=keys[-1])
        if lo == hi:
            pos[f], rot[f] = np.array(P[lo]["t"]), np.array(P[lo]["R"])
        else:
            a = (sf - lo) / (hi - lo)
            pos[f] = (1 - a) * np.array(P[lo]["t"]) + a * np.array(P[hi]["t"])
            rot[f] = slerp(np.array(P[lo]["R"]), np.array(P[hi]["R"]), a)

    ptr = read_pointer(args.session_dir)
    fx = args.focal * 1.5                      # 内参按 1920×1080 画幅表达
    intr = {"fx": round(fx, 4), "fy": round(fx, 4), "cx": 960.0, "cy": 540.0}
    t0 = datetime(2026, 1, 1, 0, 0, 0)

    mx = my = 0.0
    rows30 = []
    for f in range(n30):
        dx, dy = ptr.get(S + f, (0, 0))
        mx += dx
        my += dy
        p = pos[f]
        spd = 0.0 if f == 0 else float(np.linalg.norm(pos[f] - pos[f - 1]) * 30.0)
        q = [round(v, 6) for v in quat_world_to_cam(rot[f])]
        rows30.append({
            "time": (t0 + timedelta(seconds=f / 30.0)).strftime("%Y-%m-%d %H:%M:%S.%f")[:-3],
            "fps": 30,
            "frame": f,
            "camera_position": [round(float(v), 6) for v in p],
            "camera_rotation_quaternion": q,
            "camera_follow_offset": [0.0, 0.0, 0.0],
            "camera_intrinsics": intr,
            "camera_speed": round(spd, 4),
            "player_position": [round(float(v), 6) for v in p],
            "mouseX": round(mx, 3),
            "mouseY": round(my, 3),
            "mouse_dx": dx,
            "mouse_dy": dy,
            "keyCode": "",
            "player_rotation_quaternion": q,
            "player_speed": round(spd, 4),
            "metric_scale": 1.0,
        })

    rows15 = []
    for f in range(0, n30, 2):
        r = rows30[f]
        out = {"time": r["time"], "fps": r["fps"], "depth_name": f"depth_{f}"}
        out.update({k: v for k, v in r.items() if k not in ("time", "fps")})
        rows15.append(out)

    (args.out_dir / "action_camera_30fps.json").write_text(
        json.dumps(rows30, ensure_ascii=False))
    (args.out_dir / "action_camera_15fps.json").write_text(
        json.dumps(rows15, ensure_ascii=False))
    (args.out_dir / "systeminfo.json").write_text(json.dumps(
        {"gameProcessName": args.process_name, "x": 0, "y": 0,
         "width": 1920, "height": 1080, "recordDpi": 96}, ensure_ascii=False, indent=1))

    # ---- depth.zip：线性化 → 截断量化 → 单通道 HALF EXR --------------------
    # 写出走 cv2（单通道会写成 `Y`，客户允许 Y/Z）。**读入绝不能用 cv2**：单通道 `Z`
    # 的 EXR 它读不了且静默返回全 0，见 external-conversion §5。
    os.environ.setdefault("OPENCV_IO_ENABLE_OPENEXR", "1")
    src = {}
    for zp in sorted(glob.glob(str(args.session_dir / "runs/*/clips/*/depth-*.zip"))):
        with zipfile.ZipFile(zp) as z:
            for n in z.namelist():
                if n.startswith("depth_") and n.endswith(".exr"):
                    src[int(n[6:-4])] = (zp, n)

    tmpdir = args.out_dir / "_exr_tmp"
    tmpdir.mkdir(exist_ok=True)
    missing, nanfrac, zmin, zmax = [], [], [], []
    with zipfile.ZipFile(args.out_dir / "depth.zip", "w", zipfile.ZIP_DEFLATED) as zout:
        for f in range(0, n30, 2):
            sf = S + f
            if sf not in src:
                missing.append(sf)
                continue
            zp, n = src[sf]
            with zipfile.ZipFile(zp) as z:
                Z = linearize_depth(exr_to_array(z.read(n)))
            Zq = np.trunc(Z * 100.0) / 100.0          # trunc(z*100)/100，向零截断
            Zh = Zq.astype(np.float16)
            Zh[np.isnan(Z)] = np.float16(np.nan)      # 无效像素统一 NaN
            tmp = tmpdir / f"d{f}.exr"
            cv2.imwrite(str(tmp), Zh.astype(np.float32),
                        [cv2.IMWRITE_EXR_TYPE, cv2.IMWRITE_EXR_TYPE_HALF,
                         cv2.IMWRITE_EXR_COMPRESSION, cv2.IMWRITE_EXR_COMPRESSION_ZIP])
            zout.write(tmp, f"depth_{f}.exr")
            tmp.unlink()
            v = Z[np.isfinite(Z)]
            nanfrac.append(float(np.isnan(Z).mean()))
            if v.size:
                zmin.append(float(v.min()))
                zmax.append(float(v.max()))
    tmpdir.rmdir()

    print(json.dumps({
        "window_start_session_frame": S,
        "rows30": len(rows30), "rows15": len(rows15),
        "depth_written": (n30 // 2) - len(missing), "depth_missing": missing[:10],
        "nan_frac_max": max(nanfrac) if nanfrac else None,
        "z_min_m": min(zmin) if zmin else None, "z_max_m": max(zmax) if zmax else None,
        "fx_1920": fx,
        "pose_provenance": "rgbd_odometry (reconstruction) — NOT deliverable",
    }, indent=1))


if __name__ == "__main__":
    main()

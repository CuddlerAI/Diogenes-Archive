#!/usr/bin/env python3
"""从 R2 原始 session 组出 68 号五件套 —— 位姿用**采集机上传的真位姿**，不是重建。

与 `build_check_package.py` 的分工，一句话：**那个用里程计重建位姿，这个读真位姿**。

docs/121 §10.4 记的链路缺口是「`camera-pose-index.jsonl` / `depth-index.jsonl`
从不进云，所以 R2 上的件不足以拼出合规交付包」。docs/130 之后这两样会随 session 上云
（session 222e8f46 实测两样都在 `runs/<run>/` 下），缺口只剩音轨。位姿到手，
`rgbd_odometry.py` 那条退路就不必走了——重建轨迹有累积漂移，真位姿没有。

三条口径都是**按包实测**定的，不照游戏硬编码（照硬编码会在链路变化时静默错掉，
docs/104 §1.1 与 depth-delivery-viz skill 都记了这个教训）：

1. **手性**：`camera_rotation_quaternion` 在 pose-index 里存的是 cam→world 还是
   world→cam，用「反投影到另一帧的深度一致性」实测二选一（`--handedness auto`）。
   session 222e8f46 实测是 **cam→world**（0.06% vs 33%）。而 68 号交付口径要
   **world→cam**，所以要转置一次再写出。
2. **深度线性化参数**：优先读 depth-index，但**必须先试算**——222e8f46 的索引里
   `proj_a/proj_b = ±0.01` 是套过 viewport 变换的那组，直接用会把整段深度线性化成
   全 NaN 且不报错。复用 `sopv2_video_check.resolve_projection` 的试算门。
3. **绝对尺度**：深度线性化后的米制与 pose-index 位置的米制必须是同一把尺；
   互投影残差就是这把尺的校验（222e8f46 实测 0.06%，口径错则 19–49%）。

⚠️ **产出仍不是可对外交付件**，两个独立原因：
   ① 本 run 是 `dev_non_trainable` 档；
   ② FO4 渲染相机绑定在 #636 重新取证中，docs/122/123 明写
      `production_binding_authorized=false`。
   正式对外交付走采集机上的官方 `depth-lab deliver`。本脚本产出的是「能拿去过客户
   oracle 与内容闸、并且位姿是真的」的核验包。

用法：
    python3 build_delivery_package.py <out-dir> <session-dir> [--start 132] [--frames 1800]
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

sys.path.insert(0, str(Path(__file__).resolve().parents[2].parent / "engine-data" / "qc"))
from engine_qc import exr as _exr

sys.path.insert(0, str(Path(__file__).resolve().parent))
from sopv2_video_check import resolve_projection  # 试算门，见模块 docstring 第 2 条

DW, DH = 1280, 720
#: 视模剔除下界（docs/104 C3：FO4 第一人称视模恒 ≈0.21–1.0 m，与场景有干净空隙）。
VIEWMODEL_Z = 1.05
#: 手性实测时用到的可信深度上界。
TRUST_Z_MAX = 150.0
#: `contract.pointer_ref`：HID 位移归一化的参考幅度，取自 meta.contract。
DEFAULT_POINTER_REF = 200.0


# --------------------------------------------------------------------------- #
# 四元数 / 旋转
# --------------------------------------------------------------------------- #
def quat_to_R(q):
    x, y, z, w = q
    return np.array([
        [1 - 2 * (y * y + z * z), 2 * (x * y - z * w), 2 * (x * z + y * w)],
        [2 * (x * y + z * w), 1 - 2 * (x * x + z * z), 2 * (y * z - x * w)],
        [2 * (x * z - y * w), 2 * (y * z + x * w), 1 - 2 * (x * x + y * y)]])


def R_to_quat(M):
    t = np.trace(M)
    if t > 0:
        s = math.sqrt(t + 1.0) * 2
        w, x, y, z = 0.25 * s, (M[2, 1] - M[1, 2]) / s, (M[0, 2] - M[2, 0]) / s, (M[1, 0] - M[0, 1]) / s
    elif M[0, 0] > M[1, 1] and M[0, 0] > M[2, 2]:
        s = math.sqrt(1.0 + M[0, 0] - M[1, 1] - M[2, 2]) * 2
        w, x, y, z = (M[2, 1] - M[1, 2]) / s, 0.25 * s, (M[0, 1] + M[1, 0]) / s, (M[0, 2] + M[2, 0]) / s
    elif M[1, 1] > M[2, 2]:
        s = math.sqrt(1.0 + M[1, 1] - M[0, 0] - M[2, 2]) * 2
        w, x, y, z = (M[0, 2] - M[2, 0]) / s, (M[0, 1] + M[1, 0]) / s, 0.25 * s, (M[1, 2] + M[2, 1]) / s
    else:
        s = math.sqrt(1.0 + M[2, 2] - M[0, 0] - M[1, 1]) * 2
        w, x, y, z = (M[1, 0] - M[0, 1]) / s, (M[0, 2] + M[2, 0]) / s, (M[1, 2] + M[2, 1]) / s, 0.25 * s
    q = np.array([x, y, z, w], dtype=float)
    return q / np.linalg.norm(q)


# --------------------------------------------------------------------------- #
# 装载
# --------------------------------------------------------------------------- #
def load_session(root: Path):
    """frames.jsonl 的 session_frame↔present_seq 是位姿与画面对齐的唯一凭据。"""
    frames, metas = {}, []
    for meta in sorted(root.glob("runs/*/clips/*/meta-*.json")):
        metas.append(json.loads(meta.read_text()))
    for fp in sorted(root.glob("runs/*/clips/*/frames-*.jsonl.zst")):
        txt = subprocess.run(["zstd", "-dc", str(fp)], capture_output=True, check=True).stdout.decode()
        for line in txt.splitlines():
            if line.strip():
                d = json.loads(line)
                frames[d["session_frame"]] = d
    poses = {}
    for idx in sorted(root.glob("runs/*/camera-pose-index.jsonl")):
        for line in idx.read_text().splitlines():
            if line.strip():
                d = json.loads(line)
                poses[d["present_seq"]] = d
    if not poses:
        raise SystemExit(
            "session 里没有 camera-pose-index.jsonl —— 这条链路拿不到真位姿。\n"
            "要么该 session 早于 docs/130（位姿不上云），要么只传了部分件；\n"
            "只有重建位姿可用时请改用 rgbd_odometry.py + build_check_package.py。")
    metas.sort(key=lambda d: d["segment_index"])
    return frames, poses, metas


def depth_bank(root: Path):
    bank = {}
    for zp in sorted(glob.glob(str(root / "runs/*/clips/*/depth-*.zip"))):
        with zipfile.ZipFile(zp) as z:
            for n in z.namelist():
                if n.startswith("depth_") and n.endswith(".exr"):
                    bank[int(n[6:-4])] = (zp, n)
    return bank


def exr_array(img):
    n = img.channels[0][0]
    return (np.asarray(img.bits[n], dtype=np.uint16).view(np.float16)
              .astype(np.float32).reshape(img.height, img.width))


def read_raw(bank, fr):
    zp, n = bank[fr]
    with zipfile.ZipFile(zp) as z:
        return exr_array(_exr.read(z.read(n)))


def linearize(raw, proj):
    a, b, ms = proj
    s = raw.astype(np.float64)
    d = 1.0 - s
    with np.errstate(divide="ignore", invalid="ignore"):
        z = b / (d - a) * ms
    z[s == 0] = np.nan
    z[~np.isfinite(z)] = np.nan
    z[z <= 0] = np.nan
    return z


# --------------------------------------------------------------------------- #
# 手性实测：把 f0 的深度反投影到世界再投进 f1，与 f1 的深度比
# --------------------------------------------------------------------------- #
def reproj_error(f0, f1, frames, poses, bank, proj, cam_to_world):
    p0 = poses.get(frames[f0]["present_seq"])
    p1 = poses.get(frames[f1]["present_seq"])
    if p0 is None or p1 is None or f0 not in bank or f1 not in bank:
        return None
    K = p0["camera_intrinsics"]
    fx = K["fx"] * (DW / 1920.0)
    cx, cy = K["cx"] * (DW / 1920.0), K["cy"] * (DH / 1080.0)
    z0, z1 = linearize(read_raw(bank, f0), proj), linearize(read_raw(bank, f1), proj)
    v, u = np.mgrid[0:DH:8, 0:DW:8]
    z = z0[v, u]
    m = np.isfinite(z) & (z > VIEWMODEL_Z) & (z < TRUST_Z_MAX)
    u, v, z = u[m], v[m], z[m]
    if len(z) < 500:
        return None
    pc = np.stack([(u - cx) * z / fx, -(v - cy) * z / fx, z], 1)
    R0, R1 = quat_to_R(p0["camera_rotation_quaternion"]), quat_to_R(p1["camera_rotation_quaternion"])
    # cam_to_world=True 表示索引里存的就是 cam→world；否则存的是 world→cam，要转置。
    R0c, R1c = (R0, R1) if cam_to_world else (R0.T, R1.T)
    t0, t1 = np.array(p0["camera_position"]), np.array(p1["camera_position"])
    pw = (R0c @ pc.T).T + t0
    pc1 = (R1c.T @ (pw - t1).T).T
    zz = pc1[:, 2]
    good = zz > VIEWMODEL_Z
    if good.sum() < 300:
        return None
    uu = pc1[good, 0] / zz[good] * fx + cx
    vv = -pc1[good, 1] / zz[good] * fx + cy
    zp = zz[good]
    ii, jj = np.round(uu).astype(int), np.round(vv).astype(int)
    inb = (ii >= 0) & (ii < DW) & (jj >= 0) & (jj < DH)
    if inb.sum() < 300:
        return None
    zr, zp = z1[jj[inb], ii[inb]], zp[inb]
    g = np.isfinite(zr) & (zr > VIEWMODEL_Z) & (zr < TRUST_Z_MAX)
    if g.sum() < 200:
        return None
    return float(np.median(np.abs(zp[g] - zr[g]) / zr[g]))


def measure_handedness(frames, poses, bank, proj, start, n, n_pairs=10):
    """按包实测 pose-index 的手性。返回 `(cam_to_world, 两种假设的残差中位数)`。

    帧对按「相机确实走了 ≥1 m」挑——固定帧间隔在慢速段基线不足、在高速段又跨得太远
    （docs/115 §4）。判据只用深度一致性，与手性的先验无关。
    """
    def pos(f):
        p = poses.get(frames[f]["present_seq"])
        return None if p is None else np.array(p["camera_position"])
    pairs = []
    stride = max(2, n // (n_pairs * 3))
    for f0 in range(start, start + n - 60, stride * 2):
        if f0 % 2 or f0 not in bank or f0 not in frames:
            continue
        a = pos(f0)
        if a is None:
            continue
        for f1 in range(f0 + 10, min(f0 + 61, start + n), 2):
            b = pos(f1) if f1 in frames else None
            if b is not None and f1 in bank and np.linalg.norm(b - a) >= 1.0:
                pairs.append((f0, f1))
                break
        if len(pairs) >= n_pairs:
            break
    scores = {}
    for c2w in (True, False):
        e = [reproj_error(f0, f1, frames, poses, bank, proj, c2w) for f0, f1 in pairs]
        e = [x for x in e if x is not None]
        scores[c2w] = float(np.median(e)) if e else float("inf")
    best = min(scores, key=lambda k: scores[k])
    return best, scores, pairs


# --------------------------------------------------------------------------- #
# 主流程
# --------------------------------------------------------------------------- #
def main():
    ap = argparse.ArgumentParser(description="用真位姿组 68 号五件套（核验用，非对外交付件）")
    ap.add_argument("out_dir", type=Path)
    ap.add_argument("session_dir", type=Path)
    ap.add_argument("--start", type=int, default=0, help="交付窗起始 session_frame（须为偶数）")
    ap.add_argument("--frames", type=int, default=1800)
    ap.add_argument("--handedness", choices=("auto", "cam_to_world", "world_to_cam"),
                    default="auto")
    ap.add_argument("--process-name", default="Fallout4.exe")
    ap.add_argument("--video", type=Path, default=None,
                    help="已 concat 好的整段视频（默认自己 concat）")
    ap.add_argument("--proj-a", type=float, default=None)
    ap.add_argument("--proj-b", type=float, default=None)
    ap.add_argument("--metric-scale", type=float, default=None)
    args = ap.parse_args()

    if args.start % 2:
        raise SystemExit(f"--start 必须是偶数（depth 只在偶数 session_frame 上）：{args.start}")
    n30 = args.frames
    out = args.out_dir
    out.mkdir(parents=True, exist_ok=True)

    frames, poses, metas = load_session(args.session_dir)
    bank = depth_bank(args.session_dir)
    total = sum(m["n_frames"] for m in metas)
    if args.start + n30 > total:
        raise SystemExit(f"窗口越界：start={args.start} + {n30} > 录制 {total} 帧")

    proj, proj_src = resolve_projection(
        args.session_dir, read_raw(bank, min(bank)) if bank else None,
        (args.proj_a, args.proj_b, args.metric_scale))
    sys.stderr.write(f"depth 线性化：a={proj[0]:.10g} b={proj[1]:.10g} "
                     f"metric_scale={proj[2]:.10g}（{proj_src}）\n")

    if args.handedness == "auto":
        c2w, scores, pairs = measure_handedness(frames, poses, bank, proj, args.start, n30)
        sys.stderr.write(
            f"手性实测（{len(pairs)} 个帧对）：cam→world {scores[True]*100:.2f}% vs "
            f"world→cam {scores[False]*100:.2f}% → 索引存的是 "
            f"{'cam→world' if c2w else 'world→cam'}\n")
        hand_evidence = {"pairs": len(pairs), "cam_to_world_err": scores[True],
                         "world_to_cam_err": scores[False]}
    else:
        c2w = args.handedness == "cam_to_world"
        scores, hand_evidence = None, {"forced": args.handedness}

    # ---- 位姿：逐帧读，绝不插值 -------------------------------------------
    pointer_ref = float((metas[0].get("contract") or {}).get("pointer_ref") or DEFAULT_POINTER_REF)
    K = None
    pos, quat = {}, {}
    for i in range(n30):
        sf = args.start + i
        fr = frames.get(sf)
        p = poses.get(fr["present_seq"]) if fr else None
        if p is None:
            raise SystemExit(f"session_frame {sf} 没有位姿——不做插值，请换窗口或补件")
        pos[i] = np.array(p["camera_position"], dtype=float)
        R_c2w = quat_to_R(p["camera_rotation_quaternion"])
        if not c2w:
            R_c2w = R_c2w.T
        # 68 号交付口径是 world→cam（docs/104 C1），所以写出前再转置一次。
        quat[i] = R_to_quat(R_c2w.T)
        if K is None:
            K = p["camera_intrinsics"]

    intr = {"fx": round(float(K["fx"]), 4), "fy": round(float(K["fy"]), 4),
            "cx": float(K["cx"]), "cy": float(K["cy"])}
    t0 = datetime(2026, 1, 1, 0, 0, 0)

    def vel(i):
        """Vector3 速度（68 号 §字段表：camera_speed/player_speed 都是 Vector3，m/s）。"""
        if i == 0:
            return [0.0, 0.0, 0.0]
        return [round(float(v), 6) for v in (pos[i] - pos[i - 1]) * 30.0]

    rows30 = []
    for i in range(n30):
        fr = frames[args.start + i]
        dx, dy = fr.get("pointerDx", 0), fr.get("pointerDy", 0)
        q = [round(float(v), 6) for v in quat[i]]
        v3 = vel(i)
        rows30.append({
            "time": (t0 + timedelta(seconds=i / 30.0)).strftime("%Y-%m-%d %H:%M:%S.%f")[:-3],
            "fps": 30.0,
            "frame": i,
            "camera_position": [round(float(v), 6) for v in pos[i]],
            "camera_rotation_quaternion": q,
            "camera_follow_offset": [0.0, 0.0, 0.0],   # 第一人称，无跟随偏移
            "camera_intrinsics": intr,
            "camera_speed": v3,
            # pose-index 只给相机（provider 是 player-camera-root）。第一人称下相机即
            # 玩家视点，所以 player_* 与 camera_* 同值，**不另造一个脚底坐标**。
            "player_position": [round(float(v), 6) for v in pos[i]],
            # FO4 第一人称鼠标被游戏捕获、光标锁在屏幕中心，绝对位置就是 (0.5, 0.5)；
            # 真实信号全在 dx/dy 里。累加出一条虚拟光标轨迹是造数据，不做。
            "mouseX": 0.5,
            "mouseY": 0.5,
            "mouse_dx": round(max(-1.0, min(1.0, dx / pointer_ref)), 6),
            "mouse_dy": round(max(-1.0, min(1.0, dy / pointer_ref)), 6),
            "keyCode": list(fr.get("keys", [])),
            "player_rotation_quaternion": q,
            "player_speed": v3,
            "metric_scale": 1.0,      # 位置与深度都已是米制，比例尺 1.0
        })

    rows15 = []
    for i in range(0, n30, 2):
        r = rows30[i]
        row = {"time": r["time"], "fps": r["fps"], "depth_name": f"depth_{i}"}
        row.update({k: v for k, v in r.items() if k not in ("time", "fps")})
        rows15.append(row)

    (out / "action_camera_30fps.json").write_text(json.dumps(rows30, ensure_ascii=False))
    (out / "action_camera_15fps.json").write_text(json.dumps(rows15, ensure_ascii=False))
    (out / "systeminfo.json").write_text(json.dumps(
        {"gameProcessName": args.process_name, "x": 0, "y": 0,
         "width": 1920, "height": 1080, "recordDpi": 96}, ensure_ascii=False, indent=1))

    # ---- depth.zip：线性化 → trunc(z×100)/100 → 单通道 HALF EXR -------------
    # 写出走 cv2（单通道落 `Y`，68 号 §6.4 允许 Y/Z，且客户脚本正是用 cv2 读的）。
    # **读入绝不能用 cv2**：单通道 `Z` 它读不了且静默返回全 0（external-conversion §5）。
    os.environ.setdefault("OPENCV_IO_ENABLE_OPENEXR", "1")
    tmpdir = out / "_exr_tmp"
    tmpdir.mkdir(exist_ok=True)
    missing, nanfrac, zmin, zmax = [], [], [], []
    with zipfile.ZipFile(out / "depth.zip", "w", zipfile.ZIP_DEFLATED) as zout:
        for i in range(0, n30, 2):
            sf = args.start + i
            if sf not in bank:
                missing.append(i)
                continue
            Z = linearize(read_raw(bank, sf), proj)
            Zq = np.trunc(Z * 100.0) / 100.0          # 向零截断，不是四舍五入
            Zh = Zq.astype(np.float16)
            Zh[np.isnan(Z)] = np.float16(np.nan)      # 无效像素统一 NaN
            tmp = tmpdir / f"d{i}.exr"
            cv2.imwrite(str(tmp), Zh.astype(np.float32),
                        [cv2.IMWRITE_EXR_TYPE, cv2.IMWRITE_EXR_TYPE_HALF,
                         cv2.IMWRITE_EXR_COMPRESSION, cv2.IMWRITE_EXR_COMPRESSION_ZIP])
            zout.write(tmp, f"depth_{i}.exr")
            tmp.unlink()
            v = Z[np.isfinite(Z)]
            nanfrac.append(float(np.isnan(Z).mean()))
            if v.size:
                zmin.append(float(v.min()))
                zmax.append(float(v.max()))
    tmpdir.rmdir()

    report = {
        "window": {"start": args.start, "frames": n30, "session_frames": total},
        "rows30": len(rows30), "rows15": len(rows15),
        "depth_written": (n30 // 2) - len(missing), "depth_missing": missing[:10],
        "nan_frac_max": max(nanfrac) if nanfrac else None,
        "z_min_m": min(zmin) if zmin else None, "z_max_m": max(zmax) if zmax else None,
        "projection": {"proj_a": proj[0], "proj_b": proj[1], "metric_scale": proj[2],
                       "provenance": proj_src},
        "handedness": {"index_stores": "cam_to_world" if c2w else "world_to_cam",
                       "delivered_as": "world_to_cam", "evidence": hand_evidence},
        "camera_intrinsics": intr,
        "pose_provenance": "camera-pose-index.jsonl (resolved_render_camera_hybrid) — REAL, "
                           "not reconstruction; still NOT externally deliverable "
                           "(dev_non_trainable lane + FO4 binding under re-evidence, docs/122/123)",
    }
    (out / "build-report.json").write_text(json.dumps(report, ensure_ascii=False, indent=1))
    print(json.dumps(report, ensure_ascii=False, indent=1))


if __name__ == "__main__":
    main()

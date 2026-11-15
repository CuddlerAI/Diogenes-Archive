#!/usr/bin/env python3
"""用 skill 的 convention_check 判据裁决 ETS2 的偏移复合次序。

判据与门限逐条照抄 `.claude/skills/depth-delivery-viz/scripts/convention_check.py`：
12 对 gap-10 帧对均匀铺满全片、跳开片头，逐对算互投影相对深度误差的中位数，
**median-of-medians ≤ 阈值 且 ≥60% 帧对达标**才算通过。

差别只在输入：那边吃交付包（action_camera_30fps.json + depth.zip），
这里直接吃 `ets2-pose` 的位姿旁车 + 原始 EXR + depth-index 的逐帧投影参数
——省掉一次 deliver，判据本身一个字没改。
"""
import json
import sys
from pathlib import Path

import numpy as np
import OpenEXR

INTRINSICS_BASE_W = 1920.0


def quat_to_R(q):
    x, y, z, w = q
    return np.array([
        [1-2*(y*y+z*z), 2*(x*y-z*w),   2*(x*z+y*w)],
        [2*(x*y+z*w),   1-2*(x*x+z*z), 2*(y*z-x*w)],
        [2*(x*z-y*w),   2*(y*z+x*w),   1-2*(x*x+y*y)]])


def read_exr_z(path):
    f = OpenEXR.InputFile(str(path))
    dw = f.header()["dataWindow"]
    W = dw.max.x - dw.min.x + 1
    H = dw.max.y - dw.min.y + 1
    return np.frombuffer(f.channel("Z"), dtype=np.float16).reshape(H, W).astype(np.float32)


def linearize(stored, proj):
    a, b, ms = proj["proj_a"], proj["proj_b"], proj["metric_scale"]
    clear = 0.0 if proj.get("reverse_z") else 1.0
    d = stored.astype(np.float64)
    with np.errstate(divide="ignore", invalid="ignore"):
        z = b / (d - a) * ms
    z[d == clear] = np.nan
    z[~np.isfinite(z)] = np.nan
    z[z <= 0] = np.nan
    return (z * float(__import__("os").environ.get("ZSCALE","1"))).astype(np.float32)


def scaled_intrinsics(entry, depth_w):
    s = depth_w / INTRINSICS_BASE_W
    i = entry["camera_intrinsics"]
    return i["fx"]*s, i["fy"]*s, i["cx"]*s, i["cy"]*s


def backproject(depth, entry, stride=4, z_min=0.5, z_max=300):
    H, W = depth.shape
    fx, fy, cx, cy = scaled_intrinsics(entry, W)
    u, v = np.meshgrid(np.arange(0, W, stride), np.arange(0, H, stride))
    z = depth[v, u]
    ok = np.isfinite(z) & (z > z_min) & (z < z_max)
    u, v, z = u[ok], v[ok], z[ok]
    pc = np.stack([(u-cx)/fx*z, -(v-cy)/fy*z, z], -1)
    R = quat_to_R(entry["camera_rotation_quaternion"]).T   # C1: world→cam ⇒ 转置
    return pc @ R.T + np.array(entry["camera_position"])


def pair_error(poses, depth_of, fa, fb):
    da, db = depth_of(fa), depth_of(fb)
    if da is None or db is None:
        return None
    H, W = da.shape
    pw = backproject(da, poses[fa])
    Rb = quat_to_R(poses[fb]["camera_rotation_quaternion"]).T
    tb = np.array(poses[fb]["camera_position"])
    pcb = (pw - tb) @ Rb
    fx, fy, cx, cy = scaled_intrinsics(poses[fb], W)
    z2 = pcb[:, 2]
    ok = z2 > 0.3
    u2 = np.round(pcb[ok, 0]/z2[ok]*fx + cx).astype(int)
    v2 = np.round(-pcb[ok, 1]/z2[ok]*fy + cy).astype(int)
    inb = (u2 >= 0) & (u2 < W) & (v2 >= 0) & (v2 < H)
    zm = db[v2[inb], u2[inb]]
    zq = z2[ok][inb]
    g = np.isfinite(zm)
    if g.sum() < 500:
        return None
    return float(np.median(np.abs(zq[g]-zm[g]) / np.maximum(zm[g], 1)) * 100)


def main():
    args = dict(zip(sys.argv[1::2], sys.argv[2::2]))
    SP = Path(args["--work"])
    pose_path = Path(args["--pose"])
    threshold = float(args.get("--max-median-pct", 2.5))

    poses = {}
    for line in pose_path.open():
        r = json.loads(line)
        poses[r["session_frame"]] = r
    depth_rows = {}
    for line in (SP / "depth" / "depth-index.jsonl").open():
        r = json.loads(line)
        depth_rows[r["present_seq"]] = (r["path"], r["projection"])
    pairs_by_frame = {}
    for line in (SP / "frame-pairs.jsonl").open():
        p = json.loads(line)
        if p.get("depth") and p.get("present_seq") is not None:
            pairs_by_frame[p["session_frame"]] = p["present_seq"]

    cache = {}
    def depth_of(f):
        if f not in cache:
            ps = pairs_by_frame.get(f)
            if ps is None or ps not in depth_rows:
                cache[f] = None
            else:
                rel, proj = depth_rows[ps]
                cache[f] = linearize(read_exr_z(SP / "depth" / rel), proj)
        return cache[f]

    # 只在「有 depth 且有 pose」的偶数帧上取样
    usable = sorted(f for f in pairs_by_frame if f in poses and f % 2 == 0)
    gap, want, skip_head = 10, 12, 90
    lo = next((i for i, f in enumerate(usable) if f >= skip_head), 0)
    hi = len(usable) - 1
    idx = np.linspace(lo, hi - 2, want).astype(int)
    results = []
    for i in idx:
        fa = usable[i]
        fb = next((f for f in usable if f >= fa + gap), None)
        if fb is None:
            continue
        e = pair_error(poses, depth_of, fa, fb)
        if e is not None:
            results.append((fa, fb, e))
    if not results:
        print("没有可判决的帧对"); return 2
    meds = [e for _, _, e in results]
    mom = float(np.median(meds))
    within = sum(1 for e in meds if e <= threshold)
    frac = within / len(meds)
    ok = mom <= threshold and frac >= 0.60
    print(f"  帧对 {len(results)}  median-of-medians {mom:.2f}%  达标率 {frac*100:.0f}%  "
          f"→ {'PASS' if ok else 'FAIL'}（门 ≤{threshold}% 且 ≥60%）")
    print("  逐对:", " ".join(f"{e:.1f}" for e in meds))
    return 0 if ok else 1


if __name__ == "__main__":
    raise SystemExit(main())

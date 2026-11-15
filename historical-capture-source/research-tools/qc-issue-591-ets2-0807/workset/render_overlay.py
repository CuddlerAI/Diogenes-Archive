#!/usr/bin/env python3
"""RGB × Depth 叠加预览（68 号 §7.2「无重影」目检证据）。

契约取自 DataMiner/tools/pr405-fallout4/external-conversion.md §2，
线性化公式逐字对齐仓库内 depth_lab::depth::Projection::linearize：

    clear_depth = 0.0（reverse_z）；d == clear ⇒ NaN
    z = b / (d - a) * metric_scale；非有限或 ≤0 ⇒ NaN

**参数逐帧取 depth-index 行内值，不用 run 级常量覆盖**（外部契约 §3 那一条）。

与 FO4 契约的一处**显式偏离**：着色的远端锚点用本片实测 far（ETS2 ≈1600 m）
而不是 FO4 的 5100 m。同一条曲线套 5100 会把整条高速路压进最暗的一小段，
QC 时反而看不出重影。偏离写进报告，不静默改。
"""
import json
import subprocess
import sys
from pathlib import Path

import numpy as np
import OpenEXR
import cv2

SYNC = 0b10101010
PANEL_W, PANEL_H = 960, 540


def decode_watermark(block_means):
    bits = [1 if v >= 128 else 0 for v in block_means]
    if sum(b << (7 - i) for i, b in enumerate(bits[:8])) != SYNC:
        return None
    return sum(b << (15 - i) for i, b in enumerate(bits[8:]))


def read_exr_z(path):
    f = OpenEXR.InputFile(str(path))
    dw = f.header()["dataWindow"]
    W = dw.max.x - dw.min.x + 1
    H = dw.max.y - dw.min.y + 1
    return np.frombuffer(f.channel("Z"), dtype=np.float16).reshape(H, W).astype(np.float32)


def linearize(stored, proj):
    """depth_lab::depth::Projection::linearize 的逐位等价实现。"""
    a, b, ms = proj["proj_a"], proj["proj_b"], proj["metric_scale"]
    clear = 0.0 if proj.get("reverse_z") else 1.0
    d = stored.astype(np.float64)
    with np.errstate(divide="ignore", invalid="ignore"):
        z = b / (d - a) * ms
    z[d == clear] = np.nan          # 清屏值 = 没有几何写过这个采样
    z[~np.isfinite(z)] = np.nan
    z[z <= 0] = np.nan
    return z


def colorize(z, z_near, z_far):
    """近亮远暗；NaN → 品红（外部契约 §2）。"""
    t = 1.0 - (np.log(np.clip(z, z_near, z_far)) - np.log(z_near)) / (
        np.log(z_far) - np.log(z_near))
    t = np.clip(t, 0.0, 1.0)
    vis = cv2.applyColorMap((t * 255).astype(np.uint8), cv2.COLORMAP_TURBO)
    vis[~np.isfinite(z)] = (200, 0, 200)  # BGR 品红
    return vis


def label(img, text, y=34):
    cv2.putText(img, text, (14, y), cv2.FONT_HERSHEY_SIMPLEX, 0.8, (0, 0, 0), 4, cv2.LINE_AA)
    cv2.putText(img, text, (14, y), cv2.FONT_HERSHEY_SIMPLEX, 0.8, (255, 255, 255), 1, cv2.LINE_AA)


def main():
    args = dict(zip(sys.argv[1::2], sys.argv[2::2]))
    SP = Path(args["--work"])
    out_path = Path(args["--out"])
    z_near = float(args.get("--z-near", 0.2))
    z_far = float(args.get("--z-far", 1600.0))

    plan = json.load(open(args.get("--plan", SP / "clip-plan.json")))
    selected = plan["selected"]
    sel_pos = {sf: i for i, sf in enumerate(selected)}

    # 视频帧 → present_seq（含绕回展开）
    wm, prev, hi = {}, None, 0
    for line in (SP / "watermark-samples.jsonl").open():
        d = json.loads(line)
        v = decode_watermark(d["block_means"])
        if v is None:
            continue
        if prev is not None and v < prev - 30000:
            hi += 1 << 16
        wm[d["session_frame"]] = v + hi
        prev = v

    # depth-index：present_seq → (相对路径, 逐帧投影参数)
    depth_rows = {}
    for line in (SP / "depth" / "depth-index.jsonl").open():
        r = json.loads(line)
        depth_rows[r["present_seq"]] = (r["path"], r["projection"])

    # 偶数输出帧 → 源 session_frame（68 §6.5：depth 只落偶数帧）
    even_pairs = [(i, sf) for sf, i in sel_pos.items() if i % 2 == 0]
    even_pairs.sort()
    missing = [(i, sf) for i, sf in even_pairs if wm.get(sf) not in depth_rows]
    print(f"偶数输出帧 {len(even_pairs)}，其中缺 depth {len(missing)}", file=sys.stderr)

    segments = sorted(p for p in (SP / "rgb").iterdir() if p.is_dir())
    concat = SP / "concat.txt"
    concat.write_text("".join(f"file '{s / 'video.mp4'}'\n" for s in segments))

    reader = subprocess.Popen(
        ["ffmpeg", "-v", "error", "-f", "concat", "-safe", "0", "-i", str(concat),
         "-vf", f"scale={PANEL_W}:{PANEL_H}", "-f", "rawvideo", "-pix_fmt", "bgr24", "-"],
        stdout=subprocess.PIPE, bufsize=PANEL_W * PANEL_H * 3 * 8)
    writer = subprocess.Popen(
        ["ffmpeg", "-v", "error", "-y", "-f", "rawvideo", "-pix_fmt", "bgr24",
         "-s", f"{PANEL_W*3}x{PANEL_H}", "-r", "15", "-i", "-",
         "-c:v", "libx264", "-preset", "medium", "-crf", "20", "-pix_fmt", "yuv420p",
         str(out_path)],
        stdin=subprocess.PIPE)

    fb = PANEL_W * PANEL_H * 3
    written = 0
    session_frame = -1
    try:
        while True:
            buf = reader.stdout.read(fb)
            if len(buf) < fb:
                break
            session_frame += 1
            i = sel_pos.get(session_frame)
            if i is None or i % 2 != 0:
                continue
            present = wm.get(session_frame)
            entry = depth_rows.get(present)
            if entry is None:
                continue
            rel, proj = entry
            z = linearize(read_exr_z(SP / "depth" / rel), proj)
            vis = cv2.resize(colorize(z, z_near, z_far), (PANEL_W, PANEL_H),
                             interpolation=cv2.INTER_NEAREST)
            rgb = np.frombuffer(buf, dtype=np.uint8).reshape(PANEL_H, PANEL_W, 3).copy()
            over = cv2.addWeighted(rgb, 0.5, vis, 0.5, 0)
            finite = np.isfinite(z)
            label(rgb, f"RGB  clip_frame={i}  session_frame={session_frame}")
            label(vis, f"DEPTH present_seq={present}  {np.nanmin(z):.1f}-{np.nanmax(z):.0f} m")
            label(over, f"OVERLAY 50/50   valid={finite.mean()*100:.1f}%  NaN=sky/invalid")
            writer.stdin.write(np.hstack([rgb, vis, over]).tobytes())
            written += 1
            if written % 300 == 0:
                print(f"  已写 {written} 帧", file=sys.stderr)
    finally:
        reader.stdout.close(); reader.wait()
        writer.stdin.close(); writer.wait()
    print(f"写出 {written} 帧 → {out_path}", file=sys.stderr)
    json.dump({
        "schema": "ets2-depth-overlay-report/v1",
        "clip_start_session_frame": plan["start_session_frame"],
        "clip_end_session_frame": plan["end_session_frame"],
        "clip_frames": len(selected),
        "clip_seconds": round(len(selected) / 30.0, 3),
        "even_output_frames": len(even_pairs),
        "even_frames_missing_depth": len(missing),
        "overlay_frames_written": written,
        "colormap_z_near_m": z_near,
        "colormap_z_far_m": z_far,
        "colormap_deviation_from_fo4_contract":
            "远端锚点用 ETS2 实测 far 而不是 FO4 的 5100 m；见脚本 docstring",
    }, open(SP / "overlay-report.json", "w"), ensure_ascii=False, indent=2)
    return 0 if written and not missing else 1


if __name__ == "__main__":
    raise SystemExit(main())

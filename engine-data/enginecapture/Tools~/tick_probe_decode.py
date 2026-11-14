#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""tick 键控帧探针 · 解码判定端（enginecapture 跨模态对齐回归测试；零第三方依赖）。

为什么存在
----------
BRINGUP-MEGACITY.md §2/§7-2：AsyncGPUReadback 曾在 Update 阶段发请求，视频恒滞后
1 tick——json 与视频各自全绿、只是错开一帧，QC 无跨模态检查抓不到，当年靠一次性
探针（tick 编码成纯色帧）抓出，但那个探针没入仓。本脚本 + 包内
Tests/Runtime/TickProbeTest.cs 把它固化成可重复回归：Unity 每 tick 渲染一张
tick 键控纯色帧，走 ReadbackRing→FfmpegPipe 真实链路出 mp4，本脚本抽帧反解 tick，
逐帧断言 shift==0。

编码与可靠性论证
----------------
    R = (tick * 16) & 255        # tick 低 4 位 × 16
    G = ((tick >> 4) * 16) & 255 # tick 位 4..7 × 16
    B = 0                        # 常数哨兵：通道错位（rgba/bgra）或映射漂移立即可见

- (R,G) 联合码在 tick ∈ [0,256) 全局唯一 ⇒ 每帧反解出**绝对** tick，
  滞后/超前/丢帧/重复帧/乱序全部逐帧可见（不是只测全局最优偏移）。
- 步长 16 是本机标定出来的，不是拍的：整幅同色帧无空间细节（色度子采样无损、
  运动残差为平坦 DC、高码率下编码量化≈0），但 RGB→YCbCr(BT.601 limited)→4:2:0→
  回转 RGB 这一往返对饱和红/绿本身就有量化误差——ffmpeg 7.1.1 实测纯 swscale
  往返（无编码器）worst=3，libx264/h264_nvenc 全链 worst=4.0。步长 8 的判决
  半径恰为 4 ⇒ 零余量；步长 16 判决半径 8 = 2 倍实测噪声上限。
- 残差闸门（--resid-max，默认 5.0 = 实测噪声 4.0 + 1 冗余，< 判决半径 8）：
  任何帧的通道均值距最近码点 > 闸门即判 DECODE_FAIL（退出码 3，区别于 shift
  违例的 2）。整体映射漂移（编解两侧色彩矩阵猜测不一致 / 范围错 / tonemap
  混入）必须先漂过 5 才可能漂过 8 之后「静默解错码」——闸门保证解码结果
  要么可信、要么显式报废，不会安静地撒谎。
- 中心块均值：crop=iw/2:ih/2（默认居中）后 scale=8:8 再取浮点均值。纯色帧对任何
  缩放核都是恒等（DC 增益为 1），裁中心只为避开潜在边缘伪影；vflip 等方向变换
  对本探针天然免疫——朝向正确性由别的检查负责，这里只测时序。

用法
----
    # 判定 Unity 探针产物（TickProbeTest 内部即这么调）
    python tick_probe_decode.py --ffmpeg <ffmpeg> --expect-frames 90 tick_1.mp4 [tick_2.mp4 ...]

    # 无 Unity 的机器先验编码映射（合成帧过 FfmpegPipe 同款参数模板；不含 Unity 渲染/回读环节）
    python tick_probe_decode.py --self-test --ffmpeg <ffmpeg> --codec h264_nvenc --preset p4

退出码：0=PASS；2=存在非零 shift（打印偏移直方图）；3=解码完整性失败（帧数/残差/B 通道）；
1=用法或 IO 错误。最后一行输出 `TICKPROBE_JSON {...}` 供调用方机读。
"""

import argparse
import json
import os
import subprocess
import sys
import tempfile
from collections import Counter

STEP = 16         # 色阶步长（按实测噪声标定；见文件头论证）
CODES = 16        # 每通道码点数（0,16,...,240）
UNIQUE_TICKS = CODES * CODES  # (R,G) 联合码全局唯一区间
PIX = 8 * 8       # 中心块缩放后的像素数
EXTRACT_VF = "crop=iw/2:ih/2,scale=8:8"


def expected_rgb(tick):
    return ((tick * STEP) & 255, ((tick >> 4) * STEP) & 255, 0)


def nearest_code(mean):
    """通道均值 → 最近码点序号（0..15，越界夹紧）。"""
    return max(0, min(CODES - 1, int(mean / STEP + 0.5)))


# ---------------------------------------------------------------- 抽帧

def extract_means(ffmpeg, mp4):
    """mp4 → 逐帧中心块 (R,G,B) 浮点均值列表。"""
    raw = mp4 + ".tickprobe.rgb"
    cmd = [ffmpeg, "-hide_banner", "-loglevel", "error", "-y",
           "-i", mp4, "-map", "0:v:0", "-fps_mode", "passthrough",
           "-vf", EXTRACT_VF, "-f", "rawvideo", "-pix_fmt", "rgb24", raw]
    try:
        proc = subprocess.run(cmd, capture_output=True, text=True)
        if proc.returncode != 0:
            raise RuntimeError("ffmpeg 抽帧失败 (exit %d): %s"
                               % (proc.returncode, proc.stderr.strip()[-500:]))
        with open(raw, "rb") as f:
            data = f.read()
    finally:
        try:
            os.remove(raw)
        except OSError:
            pass
    frame_bytes = PIX * 3
    means = []
    for off in range(0, len(data) - len(data) % frame_bytes, frame_bytes):
        block = data[off:off + frame_bytes]
        means.append((sum(block[0::3]) / PIX,
                      sum(block[1::3]) / PIX,
                      sum(block[2::3]) / PIX))
    return means


# ---------------------------------------------------------------- 判定

def compress_runs(values):
    """[0,0,1,1,1] → [(0,1,0),(2,4,1)]（起帧, 末帧, 值）。"""
    runs = []
    for i, v in enumerate(values):
        if runs and runs[-1][2] == v:
            runs[-1][1] = i
        else:
            runs.append([i, i, v])
    return [tuple(r) for r in runs]


def analyze(mp4, means, expect_frames, tick0, resid_max, verbose):
    name = os.path.basename(mp4)
    rows, shifts, errors = [], [], []
    max_resid = 0.0
    for f, (rm, gm, bm) in enumerate(means):
        rc, gc, bc = nearest_code(rm), nearest_code(gm), nearest_code(bm)
        tick = rc + (gc << 4)
        resid = max(abs(rm - rc * STEP), abs(gm - gc * STEP), abs(bm - bc * STEP))
        shift = tick - (tick0 + f)
        rows.append((f, rm, gm, bm, tick, shift, resid))
        shifts.append(shift)
        max_resid = max(max_resid, resid)
        if resid > resid_max:
            errors.append("frame %d 距最近码点残差 %.2f > %.2f（均值 R/G/B = %.1f/%.1f/%.1f）"
                          % (f, resid, resid_max, rm, gm, bm))
        if bc != 0:
            errors.append("frame %d B 通道码 %d ≠ 0（通道错位或映射漂移？均值 B=%.1f）"
                          % (f, bc, bm))
    if len(means) != expect_frames:
        errors.insert(0, "帧数 %d ≠ 期望 %d（丢帧/多帧本身就是时序缺陷）"
                      % (len(means), expect_frames))
    if tick0 + expect_frames > UNIQUE_TICKS:
        errors.append("expect-frames 超出联合码唯一区 %d：绝对 tick 反解会混叠"
                      % UNIQUE_TICKS)

    hist = Counter(shifts)
    shift_zero = bool(shifts) and all(s == 0 for s in shifts)
    decode_ok = not errors
    ok = shift_zero and decode_ok

    hist_txt = "{" + ", ".join("%+d:%d" % (k, hist[k]) for k in sorted(hist)) + "}"
    print("== %s: frames=%d/%d shift_hist=%s max_resid=%.2f -> %s"
          % (name, len(means), expect_frames, hist_txt, max_resid,
             "PASS" if ok else ("DECODE_FAIL" if not decode_ok else "SHIFT_FAIL")))
    if not shift_zero and shifts:
        # shift = 反解 tick − 期望 tick。视频滞后（帧 f 装的是 tick f-1，即
        # 0802「恒滞后 1 tick」事故形态）⇒ shift = -1。注意与 S1 勘察报告的
        # 「最优对齐偏移 +1=滞后」符号相反——那边搜的是帧序列平移量。
        print("   偏移直方图（shift: 帧数；-1=视频滞后 1 tick=0802 事故形态）：")
        for k in sorted(hist):
            print("     shift %+d : %d 帧" % (k, hist[k]))
        print("   偏移分段：")
        for start, end, v in compress_runs(shifts):
            print("     frames %d..%d -> shift %+d" % (start, end, v))
    for e in errors[:20]:
        print("   [decode] " + e)
    if len(errors) > 20:
        print("   [decode] ...另有 %d 条" % (len(errors) - 20))
    if verbose:
        print("   frame |   meanR   meanG   meanB | tick shift resid")
        for f, rm, gm, bm, tick, shift, resid in rows:
            print("   %5d | %7.2f %7.2f %7.2f | %4d %+d %.2f"
                  % (f, rm, gm, bm, tick, shift, resid))

    return {
        "file": name,
        "frames": len(means),
        "expect": expect_frames,
        "shift_zero": shift_zero,
        "decode_ok": decode_ok,
        "max_resid": round(max_resid, 3),
        "hist": {str(k): hist[k] for k in sorted(hist)},
        "errors": errors[:20],
    }


# ---------------------------------------------------------------- 自测

def self_test(args):
    """合成 tick 帧 → FfmpegPipe 同款参数编码 → 解码判定。

    参数模板逐字对齐 Runtime/FfmpegPipe.cs 的 Start()（rawvideo rgba 30fps 入、
    vflip、yuv420p 出）——改那边记得改这边。此模式不含 Unity 渲染/回读环节，
    用于换编码器/升 ffmpeg 前在任意机器先验编码映射；tick↔帧对齐的最终裁决
    仍以 Unity 内 TickProbeTest 为准。
    """
    w, h = (int(x) for x in args.size.split("x"))
    out_dir = args.out_dir or tempfile.mkdtemp(prefix="tickprobe-selftest-")
    os.makedirs(out_dir, exist_ok=True)
    out = os.path.join(out_dir, "selftest_%s.mp4" % args.codec)
    cmd = [args.ffmpeg, "-hide_banner", "-loglevel", "error", "-y",
           "-f", "rawvideo", "-pix_fmt", "rgba", "-s", "%dx%d" % (w, h),
           "-r", "30", "-i", "-",
           "-vf", "vflip", "-c:v", args.codec, "-preset", args.preset,
           "-b:v", "%dk" % args.bitrate, "-pix_fmt", "yuv420p", "-r", "30", out]
    stderr_path = out + ".stderr"
    with open(stderr_path, "w") as errf:
        proc = subprocess.Popen(cmd, stdin=subprocess.PIPE, stderr=errf)
        for tick in range(args.frames):
            r, g, b = expected_rgb(tick)
            proc.stdin.write(bytes((r, g, b, 255)) * (w * h))
        proc.stdin.close()
        proc.wait()
    if proc.returncode != 0:
        with open(stderr_path) as errf:
            tail = errf.read().strip()[-500:]
        print("self-test 编码失败 (exit %d): %s" % (proc.returncode, tail))
        return 1, []
    os.remove(stderr_path)
    print("self-test 编码完成: %s (%s %s %dx%d %d 帧)"
          % (out, args.codec, args.preset, w, h, args.frames))
    means = extract_means(args.ffmpeg, out)
    lane = analyze(out, means, args.frames, 0, args.resid_max, args.verbose)
    if not args.keep and lane["shift_zero"] and lane["decode_ok"]:
        try:
            os.remove(out)
            os.rmdir(out_dir)
        except OSError:
            pass
    return 0, [lane]


# ---------------------------------------------------------------- 入口

def main():
    ap = argparse.ArgumentParser(
        description="tick 键控帧探针解码判定（见文件头文档）")
    ap.add_argument("mp4s", nargs="*", help="探针 mp4（可多路）")
    ap.add_argument("--ffmpeg", default="ffmpeg", help="ffmpeg 路径（默认 PATH）")
    ap.add_argument("--expect-frames", type=int, default=None,
                    help="期望帧数（mp4 判定模式必填）")
    ap.add_argument("--tick0", type=int, default=0, help="首帧对应的 tick（默认 0）")
    ap.add_argument("--resid-max", type=float, default=5.0,
                    help="距最近码点的残差闸门（默认 5.0，见文件头论证）")
    ap.add_argument("--verbose", action="store_true", help="打印逐帧表")
    ap.add_argument("--self-test", action="store_true",
                    help="合成帧自测编码映射（不含 Unity 环节）")
    ap.add_argument("--codec", default="libx264", help="self-test 编码器")
    ap.add_argument("--preset", default="veryfast", help="self-test 预设")
    ap.add_argument("--bitrate", type=int, default=12000, help="self-test 码率 kbps")
    ap.add_argument("--size", default="1920x1080", help="self-test 分辨率（默认=生产 16:9 档）")
    ap.add_argument("--frames", type=int, default=90, help="self-test 帧数")
    ap.add_argument("--out-dir", default=None, help="self-test 输出目录（默认临时目录）")
    ap.add_argument("--keep", action="store_true", help="self-test 通过后保留 mp4")
    args = ap.parse_args()

    if args.self_test:
        err, lanes = self_test(args)
        if err:
            code = 1
        else:
            code = 0
            for lane in lanes:
                if not lane["decode_ok"]:
                    code = max(code, 3)
                elif not lane["shift_zero"]:
                    code = max(code, 2)
    else:
        if not args.mp4s or args.expect_frames is None:
            ap.error("mp4 判定模式需要至少一个 mp4 与 --expect-frames（或使用 --self-test）")
        lanes, code = [], 0
        for mp4 in args.mp4s:
            try:
                means = extract_means(args.ffmpeg, mp4)
            except (RuntimeError, OSError) as e:
                print("== %s: 抽帧失败: %s" % (os.path.basename(mp4), e))
                lanes.append({"file": os.path.basename(mp4), "frames": 0,
                              "expect": args.expect_frames, "shift_zero": False,
                              "decode_ok": False, "max_resid": -1.0, "hist": {},
                              "errors": [str(e)[:300]]})
                code = max(code, 3)
                continue
            lane = analyze(mp4, means, args.expect_frames, args.tick0,
                           args.resid_max, args.verbose)
            lanes.append(lane)
            if not lane["decode_ok"]:
                code = max(code, 3)
            elif not lane["shift_zero"]:
                code = max(code, 2)

    print("TICKPROBE_JSON " + json.dumps(
        {"ok": code == 0, "code": code, "lanes": lanes}, ensure_ascii=True))
    return code


if __name__ == "__main__":
    sys.exit(main())

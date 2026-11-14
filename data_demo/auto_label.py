#!/usr/bin/env python3
"""
auto_label.py — 从纯像素/音频自动推断动作标注 (无 GPU / 无 opencv / 无 numpy)。

面向"游戏实况解说"这类只有画面+解说音、没有输入日志的视频。产出的是**弱但真实**的
信号(相机运动=鼠标看向; 枪声瞬态=开火/点击), 写成和手工标注同 schema 的 actions.auto.jsonl,
不覆盖手工/占位文件。键位(WASD)无法从这两个信号可靠恢复 —— 那需要模型层(LAM/IDM), 见文末。

管线:
  A. 相机运动: ffmpeg 抽 32x18 灰度帧 → 中心 ROI 全局块匹配(±7px, 早停 SAD) → 每帧 (dx,dy)
     像素位移 = 相机 pan ≈ 鼠标看向 delta。屏幕空间静态的字幕/HUD 被 ROI 裁掉以免锚死到零。
  B. 开火/点击: ffmpeg 抽 16kHz 单声道 PCM → 20fps 窗 RMS → 相对局部基线的瞬态峰 → 开火候选帧。
  C. 融合 → actions.auto.jsonl (pointerDx/Dy + foveatedBins; keys 里在开火帧加鼠标左键 vk=1)。
"""
import argparse, array, json, math, os, subprocess, sys

FPS = 20
GW, GH = 32, 18                       # 灰度帧尺寸 (相机运动分析)
ROI = (3, 2, 28, 13)                  # x0,y0,x1,y1 中心 ROI: 去掉右侧武器/底部字幕/顶部HUD
SEARCH = 7                            # 块匹配搜索半径 (px, 在 32 宽图上)
PAN_TO_MOUSE = 1920 / GW              # 小图 1px ≈ 源 60px pan, 作鼠标 delta 代理
MOUSE_CLAMP = 200

# ── foveated_bin: 逐位复现 input_recorder.rs (μ=8, ref=200, max_bin=10) ──
def foveated_bin(delta, max_bin=10, mu=8.0):
    if delta == 0 or max_bin <= 0: return 0
    sign = 1 if delta > 0 else -1
    mag = abs(delta); REF = 200.0
    compressed = math.log(1 + mu*mag) / math.log(1 + mu*REF)
    return sign * max(0, min(max_bin, round(compressed*max_bin)))

def run(cmd, **kw): return subprocess.run(cmd, capture_output=True, **kw)

# ── Stage A: 相机运动 ─────────────────────────────────────────────
def gray_frames(video):
    raw = run(["ffmpeg","-hide_banner","-loglevel","error","-i",video,
               "-vf",f"fps={FPS},scale={GW}:{GH},format=gray","-f","rawvideo","-"]).stdout
    sz = GW*GH
    return [raw[i*sz:(i+1)*sz] for i in range(len(raw)//sz)]

def sad_shift(a, b, sx, sy):
    """SAD of ROI(a) vs ROI(b) shifted by (sx,sy); early-exit not needed at this size."""
    x0,y0,x1,y1 = ROI; s = 0
    for y in range(y0, y1):
        ay = y*GW; by = (y+sy)*GW
        for x in range(x0, x1):
            s += abs(a[ay+x] - b[by+x+sx])
    return s

def camera_motion(frames):
    """每帧 (dx,dy) 内容位移。content 位移取反 = 鼠标看向。"""
    out = [(0,0)]
    x0,y0,x1,y1 = ROI
    for i in range(1, len(frames)):
        a, b = frames[i-1], frames[i]
        best, bsx, bsy = 1<<30, 0, 0
        for sy in range(-SEARCH, SEARCH+1):
            if y0+sy < 0 or y1-1+sy >= GH: continue
            for sx in range(-SEARCH, SEARCH+1):
                if x0+sx < 0 or x1-1+sx >= GW: continue
                s = sad_shift(a, b, sx, sy)
                if s < best: best, bsx, bsy = s, sx, sy
        # content 移动 (bsx,bsy) → 相机/鼠标看向反方向
        mdx = int(max(-MOUSE_CLAMP, min(MOUSE_CLAMP, -bsx*PAN_TO_MOUSE)))
        mdy = int(max(-MOUSE_CLAMP, min(MOUSE_CLAMP, -bsy*PAN_TO_MOUSE)))
        out.append((mdx, mdy))
    return out

# ── Stage B: 音频瞬态 → 开火 ──────────────────────────────────────
def audio_fire_frames(video, n_frames):
    pcm = run(["ffmpeg","-hide_banner","-loglevel","error","-i",video,
               "-ac","1","-ar","16000","-f","s16le","-"]).stdout
    s = array.array('h'); s.frombytes(pcm[:len(pcm)//2*2])
    spf = 16000 // FPS  # 800 samples/frame
    rms = []
    for f in range(n_frames):
        seg = s[f*spf:(f+1)*spf]
        if not seg: rms.append(0.0); continue
        rms.append(math.sqrt(sum(v*v for v in seg)/len(seg)))
    # 瞬态 = 相对局部基线(前后10帧中位)的比值峰 + 绝对下限, 取上升沿避免重复计数
    fires = []; W = 10
    med_sorted = sorted(rms); floor = med_sorted[int(len(med_sorted)*0.6)] * 1.6 + 1
    for f in range(n_frames):
        lo, hi = max(0,f-W), min(n_frames,f+W+1)
        base = sorted(rms[lo:hi])[(hi-lo)//2] + 1
        ratio = rms[f]/base
        if rms[f] > floor and ratio > 1.9 and (f==0 or rms[f] >= rms[f-1]):
            if not fires or f-fires[-1] > 2:   # 去抖: 相邻>2帧才算新事件
                fires.append(f)
    return fires, rms

# ── 融合 + 写出 ───────────────────────────────────────────────────
def main():
    global FPS
    ap = argparse.ArgumentParser()
    ap.add_argument("video"); ap.add_argument("--out", default=None)
    ap.add_argument("--fps", type=int, default=FPS)
    a = ap.parse_args()
    FPS = a.fps
    video = a.video
    print(f"[A] 抽灰度帧 + 相机块匹配 …", flush=True)
    frames = gray_frames(video); n = len(frames)
    motion = camera_motion(frames)
    print(f"    {n} 帧; ", end="")
    mags = [abs(dx)+abs(dy) for dx,dy in motion]
    peak = mags.index(max(mags))
    print(f"平均|pan|={sum(mags)/n:.1f}  峰值帧={peak} {motion[peak]}")
    print(f"[B] 音频瞬态检测 …", flush=True)
    fires, rms = audio_fire_frames(video, n)
    print(f"    检出开火/点击候选 {len(fires)} 个; 前若干帧={fires[:12]}")

    fireset = set(fires)
    lines = []
    for f in range(n):
        dx, dy = motion[f]
        keys = [1] if f in fireset else []   # 鼠标左键 vk=1
        lines.append(json.dumps({
            "frame": f, "video_t_ms": round(f*1000/FPS),
            "keys": keys, "pointerDx": dx, "pointerDy": dy,
            "pointerBins": [foveated_bin(dx), foveated_bin(dy)],
            "src": "auto"
        }))
    out = a.out or os.path.join(os.path.dirname(os.path.abspath(video)),
                                "dataset","bioshock_infinite","session_0001","actions.auto.jsonl")
    os.makedirs(os.path.dirname(out), exist_ok=True)
    open(out,"w").write("\n".join(lines)+"\n")
    # 报告
    rep = {
        "annotation_source": "auto_camera_flow+audio_transient",
        "has_real_actions": False, "signals": {
            "pointer": "camera global-motion block-match (32x18 gray, ROI center, ±7px) → mouse-look proxy",
            "mouse_left": f"{len(fires)} audio-transient fire events",
            "keys_wasd": "NOT recovered — needs model tier (LAM/IDM), see notes",
        },
        "stats": {"frames": n, "mean_abs_pan": round(sum(mags)/n,2),
                  "peak_pan_frame": peak, "peak_pan": motion[peak], "fire_events": len(fires)},
        "caveat": "解说视频的解说音/BGM 会污染音频开火检测; pointer 为屏幕空间 pan 代理非真鼠标计数",
    }
    open(os.path.splitext(out)[0] + ".report.json","w").write(
        json.dumps(rep, ensure_ascii=False, indent=2))
    print(f"[✓] 写出 {out}")
    print(f"    峰值运动帧 {peak} (用于 sign 校验)")
    return peak

if __name__ == "__main__":
    main()

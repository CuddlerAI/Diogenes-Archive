#!/usr/bin/env python3
"""SOP v2 视频侧自检 —— 业务方 2026-08-11 反馈四条（docs/121 §9）的机检实现。

客户的 `quality_check_depth_dataset.py` 只查交付包的**结构**（文件名/帧数/字段顺序/
depth 对账）。业务方后来追加的四条是**内容**口径，结构全绿也可能条条踩线：

| 本文件的检查 | 对应业务口径 |
|---|---|
| A `fov.value` / `fov.constant` | 锁水平 FOV 75 或 60；画面边角不畸变；**严禁片中改 FOV** |
| B `hud.clean`                  | 去除所有文字提示与游戏界面 HUD（FO4 走控制台 `tm`） |
| C `motion.perturbation`        | 单方向移动/摇镜 ≤2s，须有其他方向扰动；转弯匀速慢转 |
| D `dynamics.present`           | 60% 镜头有动态物体、30% 有**在动**的动态物体 |
| E `static.fraction`            | 原地静止帧 ≤10%（客户红线） |
| F `duration.frames`            | 60s=1800 帧 / 180s=5400 帧、1920×1080、CFR 30 |

输入是**从 R2 取下来的原始 session 目录**（clips/*/video-*.mp4 + depth-*.zip +
frames-*.jsonl.zst + meta-*.json），不是交付包——原始件是任何时候都拿得到的那一份，
交付包要先转换。

FOV 是**测**出来的，不是读出来的：摄取面不上传 `camera-pose-index.jsonl`，交付包里的
内参在这条链路上不可得。做法是拿深度图 + 光流做「刚性一致性」标定——真实焦距下同一
场景两帧的反投影点云之间是刚体变换，焦距错了就变成各向异性缩放，任何刚体运动都吸收
不掉。纯前向平移是退化情形（各向异性缩放对纯 Z 平移仍是刚体），所以只用带旋转的帧对。

用法：
    python3 sopv2_video_check.py <raw-session-dir> [--json] [--out DIR]
"""
from __future__ import annotations

import argparse
import json
import math
import subprocess
import sys
import zipfile
from dataclasses import asdict, dataclass
from pathlib import Path

import numpy as np

try:
    import cv2
except Exception as exc:  # pragma: no cover
    print(f"需要 opencv-python：{exc}", file=sys.stderr)
    raise SystemExit(2)

# --- FO4 深度线性化常数（近/远平面 + 单位，与 FOV 无关，故可跨 run 复用） -------
# 出处：depth-index v6 `projection.{proj_a,proj_b,metric_scale}`，
# 见 DataMiner/tools/pr405-fallout4/external-conversion.md §3/§5。
#
# ⚠️ 这三个数是**回落值**，不是权威值。v6 索引现在会随 session 上云（docs/130），
# 优先读索引；但读到的值必须先试算再用，见 `resolve_projection`。
PROJ_A = 1.0000420141220188
PROJ_B = -14.850629743114256
METRIC_SCALE = 0.0142875

DW, DH = 1280, 720                 # depth 分辨率

# --- B 项：HUD / 菜单 --------------------------------------------------------
# 光靠绿掩膜会把**绿色景物**判成 HUD：Diamond City 的绿漆铁皮屋和 "Publick
# Occurrences" 绿霓虹招牌实测把 `hud_total` 打到 25845（阈值才 200），
# f3cf5bab 整条被误判 5.1% 帧有 HUD。阈值是照 FO4 **室内**调的，室外绿景直接失效。
#
# 判别量因此换成 **「世界动了，它却没动」**：HUD 钉在屏幕坐标上，景物随全局位移走。
#   stay = 绿(t) ∧ 绿(t-1)                     同坐标两帧都绿
#   move = 绿(t) ∧ warp(绿(t-1), 全局位移)      随画面一起动 = 景物
#   HUD 像素 = 在足够多可判帧里 stay ∧ ¬move
# 只在全局位移够大的帧上投票（画面不动时两者无法区分）。
# 双样本实测（均 1920×1080，1800 帧）：
#   正样本 2004c0d1（HUD 在）→ 投出 6440 px = 3106 ppm，逐像素固定比中位 0.825
#   负样本 f3cf5bab（绿景物）→ 投出 **0 px**，固定比最高只有 0.455
HUD_GREEN_MIN = 200                # 保留：绿像素原始计数，仅作报告参考量，不再单独定判
HUD_MOTION_MIN_FRAC = 0.0016       # 可判帧的最小全局位移（按画面宽归一，1920 上约 3 px）
HUD_MIN_SUPPORT = 10               # 一个像素至少在这么多可判帧里是绿的才参与投票
HUD_FIXED_RATIO = 0.7              # 其中「没随画面动」的比例超过此值 = HUD 像素
HUD_MIN_PPM = 100.0                # HUD 掩膜面积下限（ppm；正样本 3106，负样本 0）
HUD_FRAME_COVER = 0.5              # 单帧覆盖掩膜多少算「这帧 HUD 在」
HUD_MIN_JUDGEABLE = 60             # 可判帧太少就不下结论，改判需人工确认
# 菜单/控制台：**大面积绿 + 世界被冻住**。实测（1920×1080）：
#   正样本暂停菜单 7710 ppm / mad 0.04，控制台 4360–4391 ppm / mad 0.11；
#   负样本绿墙帧 green 也能过 15000 px，但 mad 中位 20.4——世界在动，不是菜单。
MENU_GREEN_MIN_PPM = 2000.0        # 菜单/控制台的绿面积下限（ppm）
STATIC_MAD = 1.0                   # 灰度帧差均值低于此值算「画面没动」
STATIC_SHIFT_PX = 1.0              # 且全局位移低于此值（像素 @ 全分辨率）
FOV_TOL_DEG = 2.0
FOV_ZOOM_TOL = 0.03                # 逐帧缩放台阶超过此值 = 疑似片中改了 FOV
ONE_DIR_MAX_S = 2.0                # 单方向持续上限
HANDHELD_ROT_LO, HANDHELD_ROT_HI = 5.0, 25.0   # 手持感旋转速率中位目标 °/s
ROT_PEAK_MAX = 90.0                # 甩头上限 °/s
STATIC_MAX_FRAC = 0.10
DUP_MAX_FRAC = 0.01           # 重复帧上限（v1 档；0.1% 是健康值，实测健康条 1/1893）
DUP_MAD = 0.05                # 帧差低于此值视为「与上一帧完全相同」= 补帧
DYN_PRESENT_MIN = 0.60
DYN_MOVING_MIN = 0.30

sys.path.insert(0, str(Path(__file__).resolve().parents[2].parent / "engine-data" / "qc"))
try:
    from engine_qc import exr as _exr           # 最小 EXR 读取器（cv2 读不了单通道 Z）
except Exception:                                # pragma: no cover
    _exr = None


@dataclass
class CheckResult:
    item: str
    standard: str
    actual: str
    status: str


def ok(i, s, a): return CheckResult(i, s, a, "通过")
def bad(i, s, a): return CheckResult(i, s, a, "不通过")
def manual(i, s, a): return CheckResult(i, s, a, "需人工确认")


# --------------------------------------------------------------------------- #
# session 装载
# --------------------------------------------------------------------------- #
def load_session(root: Path):
    """把 R2 原始 session 目录按 segment_index 排好序。"""
    segs = []
    for meta in sorted(root.glob("runs/*/clips/*/meta-*.json")):
        d = json.loads(meta.read_text())
        cd = meta.parent
        segs.append({
            "segment_index": d["segment_index"],
            "n_frames": d["n_frames"],
            "session_frame_start": d["session_frame_start"],
            "width": d["width"], "height": d["height"],
            "fps": d["fps_num"] / d["fps_den"],
            "mode": d["catalog"].get("mode"),
            "non_trainable": bool(d.get("non_trainable")),
            "duplicate_frames": int(d.get("health", {}).get("duplicate_frames", 0)),
            "queue_drops": int(d.get("health", {}).get("queue_drops", 0)),
            "video": next(iter(cd.glob("video-*.mp4")), None),
            "depth": next(iter(cd.glob("depth-*.zip")), None),
            "frames": next(iter(cd.glob("frames-*.jsonl.zst")), None),
            "events": next(iter(cd.glob("events-*.jsonl.zst")), None),
        })
    segs.sort(key=lambda s: s["segment_index"])
    if not segs:
        raise SystemExit(f"目录里没有 clips/*/meta-*.json：{root}")
    return segs


def concat_video(segs, workdir: Path) -> Path:
    lst = workdir / "concat.txt"
    lst.write_text("".join(f"file '{s['video'].resolve()}'\n" for s in segs))
    out = workdir / "session_full.mp4"
    if not out.exists():
        subprocess.run(["ffmpeg", "-y", "-v", "error", "-f", "concat", "-safe", "0",
                        "-i", str(lst), "-c", "copy", str(out)], check=True)
    return out


def read_input_stream(segs):
    """逐帧原始 HID：{session_frame: (dx, dy, [vk...])}"""
    out = {}
    for s in segs:
        if not s["frames"]:
            continue
        txt = subprocess.run(["zstd", "-dc", str(s["frames"])],
                             capture_output=True, check=True).stdout.decode()
        for line in txt.splitlines():
            if line.strip():
                d = json.loads(line)
                out[d["session_frame"]] = (d.get("pointerDx", 0), d.get("pointerDy", 0),
                                           list(d.get("keys", [])))
    return out


# --------------------------------------------------------------------------- #
# 逐帧视频度量：HUD / 静止 / 全局运动 / 逐帧缩放
# --------------------------------------------------------------------------- #
def green_mask(bgr):
    b = bgr[:, :, 0].astype(np.int16)
    g = bgr[:, :, 1].astype(np.int16)
    r = bgr[:, :, 2].astype(np.int16)
    return ((g > 110) & (g - r > 55) & (g - b > 55)).astype(np.uint8)


HUD_ROI = {"hp": (0.02, 0.86, 0.30, 0.98),
           "compass": (0.28, 0.86, 0.72, 0.99),
           "crosshair": (0.44, 0.42, 0.56, 0.58)}

#: 一个像素在窗内有多大比例的帧是绿的，才算「常驻绿」。
HUD_PERSIST_RATE = 0.90
#: 常驻绿像素多于此值才判「HUD 在」。
HUD_PERSIST_MIN = 200


def _seek(cap, start):
    """顺序跳过前 `start` 帧。

    不用 `CAP_PROP_POS_FRAMES` 跳：concat 出来的 H.264 上按帧号 seek 会落在最近的
    关键帧而不报错，逐帧度量的帧号就此错位。少读几帧的代价远小于对错帧下结论。
    """
    for _ in range(start):
        if not cap.read()[0]:
            return False
    return True


def per_frame_metrics(video: Path, n_frames: int, start: int = 0, progress=None):
    """逐帧度量 + HUD 投票。

    返回 `(recs, W, H, hud)`，`hud` 是投票出来的屏幕固定绿（见文件头 B 项注释）：
    `{"mask_px", "ppm", "judgeable", "cover": [每帧对掩膜的覆盖率]}`。
    `start` > 0 时先顺序跳过（`_seek`），窗内帧号从 0 记。
    """
    cap = cv2.VideoCapture(str(video))
    W = int(cap.get(cv2.CAP_PROP_FRAME_WIDTH))
    H = int(cap.get(cv2.CAP_PROP_FRAME_HEIGHT))
    if start:
        _seek(cap, start)

    def frames():
        i = 0
        while i < n_frames:
            okf, f = cap.read()
            if not okf:
                return
            yield f
            i += 1

    try:
        return metrics_from_frames(frames(), W, H, progress=progress)
    finally:
        cap.release()


def metrics_from_frames(frames, W, H, progress=None):
    """`per_frame_metrics` 的取帧无关内核——测试直接喂 ndarray，不必造视频文件。"""
    rois = {k: (int(x0 * W), int(y0 * H), int(x1 * W), int(y1 * H))
            for k, (x0, y0, x1, y1) in HUD_ROI.items()}
    roi_mask = np.zeros((H, W), bool)
    for x0, y0, x1, y1 in rois.values():
        roi_mask[y0:y1, x0:x1] = True

    # 投票累加器 + 每帧 1/8 缩略绿掩膜（末尾用它算逐帧覆盖率，省一趟解码）
    support = np.zeros((H, W), np.int32)
    fixed = np.zeros((H, W), np.int32)
    judgeable = 0
    motion_min = HUD_MOTION_MIN_FRAC * W
    # 逐帧绿掩膜按 1/8 存一份，末尾用它算覆盖率（省一趟解码）。
    # 缩比后必须按**面积占比**判而不是取整：HUD 是细结构（准星 3 px、血条 4 px），
    # 一条 3 px 线落在 8×8 格里只占 0.375，按 uint8 取整会被抹成 0，整条 HUD 消失。
    SW, SH = W // 8, H // 8
    SMALL_MIN = 0.1                    # 格内绿占比下限；单个孤立像素 1/64 会被滤掉
    gm_small = []

    recs, prev, prev_gm = [], None, None
    i = 0
    for f in frames:
        gm = green_mask(f)
        rec = {"frame": i,
               "hud_total": int(sum(gm[y0:y1, x0:x1].sum() for x0, y0, x1, y1 in rois.values())),
               "menu_green": int(gm.sum())}
        gm_small.append(np.packbits(
            cv2.resize(gm.astype(np.float32), (SW, SH),
                       interpolation=cv2.INTER_AREA) > SMALL_MIN))
        small = cv2.resize(f, (W // 4, H // 4), interpolation=cv2.INTER_AREA)
        gray = cv2.cvtColor(small, cv2.COLOR_BGR2GRAY)
        if prev is not None:
            rec["mad"] = float(np.abs(gray.astype(np.int16) - prev.astype(np.int16)).mean())
            a, b = prev.astype(np.float32), gray.astype(np.float32)
            hann = cv2.createHanningWindow((a.shape[1], a.shape[0]), cv2.CV_32F)
            (sx, sy), resp = cv2.phaseCorrelate(a * hann, b * hann)
            rec["dx"], rec["dy"], rec["pc_resp"] = float(sx * 4), float(sy * 4), float(resp)
            M = np.float32([[1, 0, sx], [0, 1, sy]])
            warp = cv2.warpAffine(a, M, (a.shape[1], a.shape[0]),
                                  borderMode=cv2.BORDER_REPLICATE)
            m = int(0.06 * a.shape[0])
            dc = np.abs(warp - b)[m:-m, m:-m]
            rec["resid"] = float(dc.mean())
            rec["resid_frac"] = float((dc > 18).mean())
            # 画面动得够多才投票：不动时「随画面动」和「钉在屏幕上」无法区分
            if math.hypot(rec["dx"], rec["dy"]) >= motion_min:
                Mg = np.float32([[1, 0, rec["dx"]], [0, 1, rec["dy"]]])
                wgm = cv2.warpAffine(prev_gm, Mg, (W, H), flags=cv2.INTER_NEAREST,
                                     borderMode=cv2.BORDER_CONSTANT, borderValue=0)
                g, pg, wg = gm.astype(bool), prev_gm.astype(bool), wgm.astype(bool)
                support += g
                fixed += (g & pg) & ~(g & wg)
                judgeable += 1
        recs.append(rec)
        prev, prev_gm = gray, gm
        i += 1
        if progress and i % 300 == 0:
            progress(i)

    hud_mask = (roi_mask & (support >= HUD_MIN_SUPPORT)
                & (fixed >= HUD_FIXED_RATIO * np.maximum(support, 1)))
    mask_px = int(hud_mask.sum())
    cover = []
    if mask_px:
        hs = cv2.resize(hud_mask.astype(np.float32), (SW, SH),
                        interpolation=cv2.INTER_AREA) > SMALL_MIN
        denom = max(int(hs.sum()), 1)
        for packed in gm_small:
            g = np.unpackbits(packed)[:SH * SW].reshape(SH, SW).astype(bool)
            cover.append(float((g & hs).sum()) / denom)
    return recs, W, H, {"mask_px": mask_px, "ppm": 1e6 * mask_px / (W * H),
                        "judgeable": judgeable, "cover": cover}


def per_frame_zoom(video: Path, n_frames: int, start: int = 0, stride: int = 1):
    """逐帧相似变换的尺度。片中改 FOV 会在某一帧出现明显 !=1 的缩放。

    同时记录内点率与中位光流：**甩头时 LK 跟踪会退化并吐出假的尺度**（实测 1.14x），
    与真·改 FOV 的台阶无法靠数值大小区分，只能靠拟合质量区分——真台阶出现在画面几乎
    不动的那一帧（改 FOV 是瞬时的），假台阶出现在光流几十上百像素的甩头帧。
    """
    cap = cv2.VideoCapture(str(video))
    out = []
    prev = None
    _seek(cap, start)
    i = start
    while i < start + n_frames:
        okf, f = cap.read()
        if not okf:
            break
        g = cv2.cvtColor(cv2.resize(f, (DW, DH), interpolation=cv2.INTER_AREA),
                         cv2.COLOR_BGR2GRAY)
        if prev is not None and i % stride == 0:
            rec = {"frame": i, "scale": None, "inlier_ratio": 0.0, "median_flow": None}
            p0 = cv2.goodFeaturesToTrack(prev, 1200, 0.01, 8)
            if p0 is not None:
                p1, st, _ = cv2.calcOpticalFlowPyrLK(prev, g, p0, None,
                                                     winSize=(31, 31), maxLevel=4)
                sel = st.ravel() == 1
                A, B = p0.reshape(-1, 2)[sel], p1.reshape(-1, 2)[sel]
                if len(A) >= 50:
                    rec["median_flow"] = float(np.median(np.linalg.norm(B - A, axis=1)))
                    M, inl = cv2.estimateAffinePartial2D(
                        A, B, method=cv2.RANSAC, ransacReprojThreshold=1.5)
                    if M is not None:
                        rec["scale"] = float(math.hypot(M[0, 0], M[0, 1]))
                        rec["inlier_ratio"] = float(inl.sum() / len(A))
            out.append(rec)
        prev = g
        i += 1
    cap.release()
    return out


ZOOM_MIN_INLIER_RATIO = 0.6
ZOOM_MAX_FLOW_PX = 40.0


def zoom_steps(zooms):
    """从逐帧尺度里挑出**可信的**缩放台阶（= 疑似改 FOV 的帧）。"""
    return sorted(
        (z for z in zooms
         if z["scale"] is not None
         and abs(z["scale"] - 1.0) > FOV_ZOOM_TOL
         and z["inlier_ratio"] >= ZOOM_MIN_INLIER_RATIO
         and (z["median_flow"] or 0.0) <= ZOOM_MAX_FLOW_PX),
        key=lambda z: -abs(z["scale"] - 1.0))


# --------------------------------------------------------------------------- #
# A · FOV：深度 + 光流的刚性一致性标定
# --------------------------------------------------------------------------- #
def exr_array(img):
    n = img.channels[0][0]
    return (np.asarray(img.bits[n], dtype=np.uint16).view(np.float16)
              .astype(np.float32).reshape(img.height, img.width))


def linearize(raw, proj=None):
    a, b, ms = proj if proj else (PROJ_A, PROJ_B, METRIC_SCALE)
    s = raw.astype(np.float64)
    d = 1.0 - s
    with np.errstate(divide="ignore", invalid="ignore"):
        z = b / (d - a) * ms
    z[s == 0] = np.nan
    z[~np.isfinite(z)] = np.nan
    z[z <= 0] = np.nan
    return z


#: 一帧线性化后至少要有这么多有效像素，才认为这组参数配得上这批 buffer。
PROJ_MIN_VALID_FRAC = 0.30


def resolve_projection(root: Path, bank_sample, override=None):
    """定出本 run 的深度线性化参数，并返回 `(参数, 出处说明)`。

    顺序是「命令行 > 索引 > 回落常数」，但**索引值必须先试算一帧再用**。

    为什么不能直接信索引：session 222e8f46 的 v6 索引里
    `proj_a/proj_b = ±0.0100000284` 是套过 viewport 变换（MinDepth 0 / MaxDepth 0.01）
    之后的那一组，而落盘 buffer 存的是**未套变换**的值。照读的后果不是报错，是
    `d - proj_a` 全程同号且远离奇点，整段深度线性化成 **全 NaN**——A/C 两项直接
    变成「无法标定」，而根因完全不在视频里。这类「结构合规、只有数值错」的坑
    docs/130 §6 列了三条，唯一的办法就是加一条独立观测，所以这里试算。
    """
    if override and all(v is not None for v in override):
        return tuple(override), "命令行显式指定"

    cands = []
    for idx in sorted(root.glob("runs/*/depth-index.jsonl")):
        seen = set()
        for line in idx.read_text().splitlines():
            if not line.strip():
                continue
            p = json.loads(line).get("projection") or {}
            t = (p.get("proj_a"), p.get("proj_b"), p.get("metric_scale"))
            if all(v is not None for v in t) and t not in seen:
                seen.add(t)
                cands.append((t, idx.name))
    if bank_sample is None:
        if cands:
            return cands[0][0], f"depth-index（{len(cands)} 组，未试算：没有可读的深度帧）"
        return (PROJ_A, PROJ_B, METRIC_SCALE), "回落常数（无 depth-index）"

    for t, src in cands:
        z = linearize(bank_sample, t)
        frac = float(np.isfinite(z).mean())
        if frac >= PROJ_MIN_VALID_FRAC:
            return t, f"{src} 逐帧参数（试算有效像素 {100*frac:.1f}%）"
        sys.stderr.write(
            f"!! depth-index 的 proj_a={t[0]:.10g}/proj_b={t[1]:.10g} 试算只剩 "
            f"{100*frac:.2f}% 有效像素（阈值 {100*PROJ_MIN_VALID_FRAC:.0f}%），弃用\n")
    fb = (PROJ_A, PROJ_B, METRIC_SCALE)
    frac = float(np.isfinite(linearize(bank_sample, fb)).mean())
    note = (f"回落常数（索引 {len(cands)} 组全部试算不过）" if cands else "回落常数（无 depth-index）")
    return fb, f"{note}，试算有效像素 {100*frac:.1f}%"


class DepthBank:
    def __init__(self, zips, proj=None):
        self.proj = proj
        self.map = {}
        for zp in zips:
            with zipfile.ZipFile(zp) as z:
                for n in z.namelist():
                    if n.startswith("depth_") and n.endswith(".exr"):
                        self.map[int(n[6:-4])] = (zp, n)

    def raw(self, fr):
        """未线性化的 buffer 值——给 `resolve_projection` 试算用。"""
        if fr not in self.map:
            return None
        zp, n = self.map[fr]
        with zipfile.ZipFile(zp) as z:
            return exr_array(_exr.read(z.read(n)))

    def get(self, fr):
        r = self.raw(fr)
        return None if r is None else linearize(r, self.proj)


def _kabsch(P, Q):
    cp, cq = P.mean(0), Q.mean(0)
    U, S, Vt = np.linalg.svd((P - cp).T @ (Q - cq))
    d = np.sign(np.linalg.det(Vt.T @ U.T))
    R = Vt.T @ np.diag([1, 1, d]) @ U.T
    return R, cq - R @ cp


def _robust_resid(P, Q, iters=3):
    w = np.ones(len(P), bool)
    for _ in range(iters):
        if w.sum() < 20:
            break
        R, t = _kabsch(P[w], Q[w])
        e = np.linalg.norm((R @ P.T).T + t - Q, axis=1)
        w = e < max(np.percentile(e[w], 75) * 2.0, 0.02)
    R, t = _kabsch(P[w], Q[w])
    e = np.linalg.norm((R @ P.T).T + t - Q, axis=1)
    return (float(np.median(e[w])) if w.sum() else 9.9), R, t


def _unproj(uv, z, f):
    cx, cy = (DW - 1) / 2.0, (DH - 1) / 2.0
    return np.stack([(uv[:, 0] - cx) * z / f, (uv[:, 1] - cy) * z / f, z], 1)


def measure_fov(video: Path, bank: DepthBank, metrics, window, n_pairs=12, gap=6):
    """在给定帧窗内挑「转得多」的帧对，扫焦距取刚性残差最小值。"""
    if _exr is None or not bank.map:
        return None
    lo, hi = window
    lo += lo % 2                      # depth 只在偶数帧，窗口起点必须对齐到偶数
    M = {r["frame"]: r for r in metrics}
    # 要「转得够多」才能标定焦距（纯前向平移退化），但**不能挑最猛的那几帧**——
    # 甩头帧 LK 跟踪整片失效，能配上的点少到拟合不可信。所以按相位相关响应筛掉
    # 跟踪已经垮掉的窗口，再在剩下的里取转动量最大的若干个。
    cands = []
    for f0 in range(lo, hi - gap, 2):
        if f0 not in bank.map or (f0 + gap) not in bank.map:
            continue
        ks = [k for k in range(f0 + 1, f0 + gap + 1) if k in M]
        if not ks:
            continue
        rot = sum(abs(M[k].get("dx", 0)) + abs(M[k].get("dy", 0)) for k in ks)
        resp = min(M[k].get("pc_resp", 0.0) for k in ks)
        cands.append((rot, resp, f0))
    cands.sort(key=lambda c: -c[0])
    sel, used = [], []
    for rot, resp, f0 in cands:
        if rot < 15 or resp < 0.25 or any(abs(f0 - u) < 10 for u in used):
            continue
        used.append(f0)
        sel.append(f0)
        if len(sel) >= n_pairs:
            break
    if not sel:
        return None

    need = sorted({f for f0 in sel for f in (f0, f0 + gap)})
    cap = cv2.VideoCapture(str(video))
    rgb, i = {}, 0
    while i <= need[-1]:
        okf, fr = cap.read()
        if not okf:
            break
        if i in need:
            rgb[i] = cv2.cvtColor(cv2.resize(fr, (DW, DH), interpolation=cv2.INTER_AREA),
                                  cv2.COLOR_BGR2GRAY)
        i += 1
    cap.release()

    grid = np.arange(300.0, 1400.1, 10.0)
    curves = []
    for f0 in sel:
        f1 = f0 + gap
        z0, z1 = bank.get(f0), bank.get(f1)
        g0, g1 = rgb.get(f0), rgb.get(f1)
        if z0 is None or z1 is None or g0 is None or g1 is None:
            continue
        p0 = cv2.goodFeaturesToTrack(g0, 1500, 0.01, 8)
        if p0 is None:
            continue
        p1, st, _ = cv2.calcOpticalFlowPyrLK(g0, g1, p0, None, winSize=(31, 31), maxLevel=4)
        pb, st2, _ = cv2.calcOpticalFlowPyrLK(g1, g0, p1, None, winSize=(31, 31), maxLevel=4)
        fb = np.linalg.norm(pb - p0, axis=2).ravel()
        good = (st.ravel() == 1) & (st2.ravel() == 1) & (fb < 1.0)
        a, b = p0.reshape(-1, 2)[good], p1.reshape(-1, 2)[good]
        if len(a) < 60:
            continue
        ia, ib = np.round(a).astype(int), np.round(b).astype(int)
        inb = ((ia[:, 0] >= 0) & (ia[:, 0] < DW) & (ia[:, 1] >= 0) & (ia[:, 1] < DH) &
               (ib[:, 0] >= 0) & (ib[:, 0] < DW) & (ib[:, 1] >= 0) & (ib[:, 1] < DH))
        a, b, ia, ib = a[inb], b[inb], ia[inb], ib[inb]
        za, zb = z0[ia[:, 1], ia[:, 0]], z1[ib[:, 1], ib[:, 0]]
        gd = (np.isfinite(za) & np.isfinite(zb) & (za > 0.4) & (zb > 0.4) &
              (za < 60) & (zb < 60))
        a, b, za, zb = a[gd], b[gd], za[gd], zb[gd]
        if len(a) < 60:
            continue
        curve = np.array([_robust_resid(_unproj(a, za, f), _unproj(b, zb, f))[0]
                          for f in grid])
        bestf = grid[int(np.argmin(curve))]
        _, R, _ = _robust_resid(_unproj(a, za, bestf), _unproj(b, zb, bestf))
        ang = math.degrees(math.acos(max(-1, min(1, (np.trace(R) - 1) / 2))))
        if ang >= 3.0:
            curves.append(curve / curve.min())
    if not curves:
        return None
    agg = np.mean(curves, 0)
    i = int(np.argmin(agg))
    bf = grid[i]
    if 0 < i < len(grid) - 1:
        y0, y1, y2 = agg[i - 1], agg[i], agg[i + 1]
        den = y0 - 2 * y1 + y2
        if den:
            bf = grid[i] + 10.0 * 0.5 * (y0 - y2) / den
    return {"f_1280": float(bf), "fx_1920": float(bf * 1.5),
            "hfov_deg": 2 * math.degrees(math.atan(DW / (2 * bf))),
            "n_pairs": len(curves),
            "rejection_ratio_at_110deg": float(agg[np.argmin(np.abs(grid - 448))])}


# --------------------------------------------------------------------------- #
# 判定
# --------------------------------------------------------------------------- #
def runs_of(pred, recs, min_len=1):
    out, s = [], None
    for r in recs:
        if pred(r):
            if s is None:
                s = r["frame"]
        else:
            if s is not None and r["frame"] - s >= min_len:
                out.append((s, r["frame"] - 1))
            s = None
    if s is not None and recs[-1]["frame"] - s >= min_len:
        out.append((s, recs[-1]["frame"]))
    return out


def evaluate(segs, metrics, zooms, fov_late, fov_early, inputs, W, H, n_win, fps, hud,
             start=0, fov_read=None):
    res = []
    dur = n_win / fps
    # 输入流按 **session_frame** 建索引，而交付窗未必从 0 开始（子集视图、跳过开场、
    # 或 `--window-start` 在未裁剪视频上选窗，都会让它从非 0 帧起）。这里必须把首段的
    # session_frame_start 与选窗起点都加上，否则「鼠标微扰」读的是别的帧。
    # 实测偏差可达 13 个百分点。
    base = segs[0]["session_frame_start"] + start
    win_txt = (f"交付窗 {n_win} 帧 / {dur:.2f}s"
               f"（取 f{start}–f{start + n_win - 1}，原始录制 "
               f"{sum(s['n_frames'] for s in segs)} 帧）")

    # ---- F 时长/规格 -------------------------------------------------------
    want = {1800: "60s", 5400: "180s"}
    res.append(ok("F 时长/帧数", "60s=1800 帧 或 180s=5400 帧", win_txt)
              if n_win in want else
              bad("F 时长/帧数", "60s=1800 帧 或 180s=5400 帧", win_txt))
    res.append(ok("F 采集分辨率", "交付需 1920x1080（可由更高分辨率缩放）", f"{W}x{H}")
               if (W, H) == (1920, 1080) or abs(W / H - 16 / 9) < 1e-3 else
               bad("F 采集分辨率", "16:9，交付缩放到 1920x1080", f"{W}x{H}"))
    res.append(ok("F 帧率", "CFR 30", f"{fps:.3f}fps")
               if abs(fps - 30) < 0.01 else bad("F 帧率", "CFR 30", f"{fps:.3f}fps"))
    # 重复帧 = 游戏没跑满 30fps，采集侧补帧维持 CFR。容器仍是 CFR 30，客户脚本查不出来，
    # 但对 SFM/深度是直接有害的（重复帧之间基线为零），且踩 §1.3「动态 fps 是打回项」。
    dup = sum(s["duplicate_frames"] for s in segs)
    ntot = sum(s["n_frames"] for s in segs)
    dfrac = dup / max(ntot, 1)
    worst = max(segs, key=lambda s: s["duplicate_frames"] / max(s["n_frames"], 1))
    dtxt = (f"{dup}/{ntot} = {100*dfrac:.1f}%（实效帧率约 {30*(1-dfrac):.1f}fps；"
            f"最差 seg{worst['segment_index']} {100*worst['duplicate_frames']/max(worst['n_frames'],1):.1f}%）")
    res.append(ok("F 重复帧", f"≤{100*DUP_MAX_FRAC:.0f}%（游戏须跑满 30fps，采集不补帧）", dtxt)
               if dfrac <= DUP_MAX_FRAC else
               bad("F 重复帧", f"≤{100*DUP_MAX_FRAC:.0f}%（游戏须跑满 30fps，采集不补帧）", dtxt))

    lanes = {s["mode"] for s in segs}
    res.append(ok("F 数据档位", "正式交付需可训练档", f"{sorted(lanes)}")
               if not any(s["non_trainable"] for s in segs) else
               bad("F 数据档位", "正式交付需可训练档（非 dev_non_trainable）",
                   f"{sorted(lanes)}：本批禁止对外交付"))

    # ---- A FOV -------------------------------------------------------------
    # `fov_read` 是 camera-pose-index 里的内参（docs/130 起随 session 上云）。有它就
    # 以它为准——标定值退位成独立交叉核验，两者对不上要查的是标定而不是数据。
    if fov_read:
        h = fov_read["hfov_deg"]
        hit = min((abs(h - t), t) for t in (75.0, 60.0))
        cross = (f"，标定交叉核验 {fov_late['hfov_deg']:.2f}°"
                 f"（差 {abs(fov_late['hfov_deg'] - h):.2f}°）" if fov_late else
                 "，无足量转动帧对可交叉核验")
        res.append((ok if hit[0] <= FOV_TOL_DEG else bad)(
            "A 水平 FOV 取值", "锁 75° 或 60°（±2°）",
            f"读自 camera-pose-index：{h:.2f}°（fx@1920={fov_read['fx_1920']:.1f}，"
            f"{fov_read['n_rows']} 行，distinct={fov_read['n_distinct']}）{cross}"))
    elif fov_late:
        h = fov_late["hfov_deg"]
        hit = min((abs(h - t), t) for t in (75.0, 60.0))
        res.append(ok("A 水平 FOV 取值", "锁 75° 或 60°（±2°）",
                      f"实测 {h:.2f}°（fx@1920={fov_late['fx_1920']:.0f}，"
                      f"{fov_late['n_pairs']} 个转动帧对，110° 假设的残差是它的 "
                      f"{fov_late['rejection_ratio_at_110deg']:.0f}x）")
                   if hit[0] <= FOV_TOL_DEG else
                   bad("A 水平 FOV 取值", "锁 75° 或 60°（±2°）",
                       f"实测 {h:.2f}°，距最近目标 {hit[1]:.0f}° 差 {hit[0]:.2f}°"))
    else:
        res.append(manual("A 水平 FOV 取值", "锁 75° 或 60°（±2°）",
                          "无深度或无足量转动帧对，无法标定"))
    # 逐帧相似变换的尺度：真·改 FOV 是单帧的大台阶（75°→80° = 1.093x），
    # 运动中的拟合噪声是 1% 量级，所以阈值取 3%，并用跳变两侧的**实测 FOV**定判。
    jumps = zoom_steps(zooms)
    if fov_read and fov_read["n_distinct"] == 1:
        # 逐帧内参直接读得到且全程只有一组值 —— 这是「没改过 FOV」的直接证据，
        # 比缩放台阶的间接推断强一个量级，台阶只降级成附注。
        note = (f"；另检出 {len(jumps)} 处缩放台阶（"
                + ", ".join(f"f{z['frame']}({z['scale']:.4f}x)" for z in jumps[:3])
                + "），内参未变故判为运动/跟踪噪声" if jumps else "")
        res.append(ok("A FOV 逐帧恒定", "片内严禁改 FOV/滚轮缩放，逐帧内参恒定",
                      f"camera-pose-index {fov_read['n_rows']} 行内参只有 1 组值"
                      f"（fx={fov_read['fx_1920']:.4f}）{note}"))
    elif jumps:
        j = ", ".join(f"f{z['frame']}({z['scale']:.4f}x @{z['frame']/fps:.1f}s)" for z in jumps[:3])
        if fov_early and fov_late:
            dh = abs(fov_early["hfov_deg"] - fov_late["hfov_deg"])
            detail = (f"检出 {len(jumps)} 处画面缩放台阶：{j}；"
                      f"跳变前实测 hFOV {fov_early['hfov_deg']:.1f}° → 跳变后 {fov_late['hfov_deg']:.1f}°"
                      f"（差 {dh:.1f}°）")
            verdict = ok if dh <= FOV_TOL_DEG else bad
        else:
            detail = f"检出 {len(jumps)} 处画面缩放台阶：{j}（两侧 FOV 未能分别标定）"
            verdict = bad
        res.append(verdict("A FOV 逐帧恒定", "片内严禁改 FOV/滚轮缩放，逐帧内参恒定", detail))
    else:
        res.append(ok("A FOV 逐帧恒定", "片内严禁改 FOV/滚轮缩放，逐帧内参恒定",
                      f"逐帧相似变换尺度全程 1.000±{FOV_ZOOM_TOL}"))

    # ---- B HUD -------------------------------------------------------------
    # 判据是「屏幕固定绿」，不是「有绿」——绿色景物随画面走，投不出票。
    green_frames = sum(1 for r in metrics if r["hud_total"] > HUD_GREEN_MIN)
    green_txt = (f"（绿掩膜原始计数 >{HUD_GREEN_MIN} 的帧 {green_frames}/{len(metrics)}"
                 f" = {100*green_frames/max(len(metrics),1):.1f}%，多为绿色景物）")
    if hud["judgeable"] < HUD_MIN_JUDGEABLE:
        res.append(manual("B HUD 清除", "全片无 HUD/文字提示（FO4 控制台 `tm`）",
                          f"可判帧仅 {hud['judgeable']}（需 ≥{HUD_MIN_JUDGEABLE}）——"
                          f"画面几乎全程不动，屏幕固定绿与景物绿无法区分，需目检 {green_txt}"))
    elif hud["ppm"] >= HUD_MIN_PPM:
        cov = hud["cover"]
        n_on = sum(1 for c in cov if c > HUD_FRAME_COVER)
        hr = runs_of(lambda r: cov[r["frame"]] > HUD_FRAME_COVER, metrics)
        seg_txt = "; ".join(f"f{a}-{b}({a/fps:.1f}-{b/fps:.1f}s)" for a, b in hr[:4])
        res.append(bad("B HUD 清除", "全片无 HUD/文字提示（FO4 控制台 `tm`）",
                       f"投出屏幕固定绿 {hud['mask_px']} px（{hud['ppm']:.0f} ppm，闸 {HUD_MIN_PPM:.0f}）；"
                       f"{n_on}/{len(metrics)} 帧检出 HUD（{100*n_on/max(len(metrics),1):.1f}%）：{seg_txt}"))
    else:
        res.append(ok("B HUD 清除", "全片无 HUD/文字提示（FO4 控制台 `tm`）",
                      f"{hud['judgeable']} 个可判帧投票，屏幕固定绿 {hud['mask_px']} px"
                      f"（{hud['ppm']:.0f} ppm < 闸 {HUD_MIN_PPM:.0f}）{green_txt}"))

    # 菜单/控制台：大面积绿 **且世界被冻住**（菜单会把游戏暂停；绿墙帧画面照样在动）
    area = W * H
    def is_menu(r):
        return (1e6 * r["menu_green"] / area > MENU_GREEN_MIN_PPM
                and "mad" in r and r["mad"] < STATIC_MAD)
    menu = runs_of(is_menu, metrics)
    if menu:
        n_menu = sum(1 for r in metrics if is_menu(r))
        res.append(bad("B 菜单/控制台入镜", "不得录进暂停菜单、控制台、加载 CG",
                       f"{n_menu} 帧：" + "; ".join(
                           f"f{a}-{b}({a/fps:.1f}-{b/fps:.1f}s)" for a, b in menu[:4])))
    else:
        big = sum(1 for r in metrics if 1e6 * r["menu_green"] / area > MENU_GREEN_MIN_PPM)
        res.append(ok("B 菜单/控制台入镜", "不得录进暂停菜单、控制台、加载 CG",
                      f"未检出（大面积绿 {big} 帧，但画面均在动 = 绿色景物而非菜单）"))

    # ---- E 静止 ------------------------------------------------------------
    # ⚠️ 补帧与「人站着不动」在画面上无法区分（都是 mad≈0），但含义完全相反：
    # 前者是掉帧、人还在走。把补帧排除出静止判定，否则会把掉帧误报成「走走停停」，
    # 给采集员完全错误的反馈（9a66e5fe 实测：误报 13pp）。
    allmv = [r for r in metrics if "mad" in r]
    dupf = [r for r in allmv if r["mad"] < DUP_MAD]
    mv = [r for r in allmv if r["mad"] >= DUP_MAD]
    static = [r for r in mv
              if r["mad"] < STATIC_MAD and abs(r.get("dx", 0)) < STATIC_SHIFT_PX
              and abs(r.get("dy", 0)) < STATIC_SHIFT_PX]
    frac = len(static) / max(len(mv), 1)
    sruns = runs_of(lambda r: (r["mad"] < STATIC_MAD and abs(r.get("dx", 0)) < STATIC_SHIFT_PX
                               and abs(r.get("dy", 0)) < STATIC_SHIFT_PX), mv, min_len=15)
    longest = max(((b - a + 1) / fps for a, b in sruns), default=0.0)
    txt = (f"静止帧 {100*frac:.1f}%（{len(static)}/{len(mv)}，已剔除 {len(dupf)} 个补帧），"
           f"最长静止段 {longest:.1f}s"
           + (f" @ f{max(sruns, key=lambda x: x[1]-x[0])[0]}" if sruns else ""))
    res.append(ok("E 原地静止占比", f"≤{100*STATIC_MAX_FRAC:.0f}%（客户红线）", txt)
               if frac <= STATIC_MAX_FRAC else
               bad("E 原地静止占比", f"≤{100*STATIC_MAX_FRAC:.0f}%（客户红线）", txt))

    # ---- C 扰动 ------------------------------------------------------------
    # 用全局像素位移的方向符号判「单方向」：连续同号且有实际位移的最长段。
    def one_dir_run(axis):
        best, cur, sign = 0, 0, 0
        for r in mv:
            v = r.get(axis, 0.0)
            if abs(v) < 0.6:
                cur, sign = 0, 0
                continue
            s = 1 if v > 0 else -1
            cur = cur + 1 if s == sign else 1
            sign = s
            best = max(best, cur)
        return best / fps
    odx, ody = one_dir_run("dx"), one_dir_run("dy")
    worst = max(odx, ody)
    res.append(ok("C 单方向持续时长", f"单方向移动/摇镜 ≤{ONE_DIR_MAX_S:.0f}s",
                  f"水平最长 {odx:.1f}s、垂直最长 {ody:.1f}s")
               if worst <= ONE_DIR_MAX_S else
               bad("C 单方向持续时长", f"单方向移动/摇镜 ≤{ONE_DIR_MAX_S:.0f}s",
                   f"水平最长 {odx:.1f}s、垂直最长 {ody:.1f}s"))

    f_ref = fov_read or fov_late
    if f_ref:
        f_px = f_ref["f_1280"] * (W / DW)
        rot = np.array([math.degrees(math.atan2(math.hypot(r.get("dx", 0), r.get("dy", 0)), f_px)) * fps
                        for r in mv])
        med, p95, peak = float(np.median(rot)), float(np.percentile(rot, 95)), float(rot.max())
        detail = f"中位 {med:.1f}°/s、p95 {p95:.1f}°/s、峰值 {peak:.1f}°/s"
        good = HANDHELD_ROT_LO <= med <= HANDHELD_ROT_HI and peak <= ROT_PEAK_MAX
        res.append(ok("C 手持感（旋转速率）",
                      f"中位 {HANDHELD_ROT_LO:.0f}–{HANDHELD_ROT_HI:.0f}°/s，峰值 ≤{ROT_PEAK_MAX:.0f}°/s", detail)
                   if good else
                   bad("C 手持感（旋转速率）",
                       f"中位 {HANDHELD_ROT_LO:.0f}–{HANDHELD_ROT_HI:.0f}°/s，峰值 ≤{ROT_PEAK_MAX:.0f}°/s", detail))
    else:
        res.append(manual("C 手持感（旋转速率）", "中位 5–25°/s，峰值 ≤90°/s", "缺 FOV 标定，无法换算角速度"))

    ptr_moving = sum(1 for f in range(n_win) if any(inputs.get(base + f, (0, 0, []))[:2]))
    res.append(ok("C 鼠标持续微扰", "右手不离鼠标，全程微动",
                  f"有鼠标位移的帧 {100*ptr_moving/n_win:.1f}%")
               if ptr_moving / n_win >= 0.5 else
               bad("C 鼠标持续微扰", "右手不离鼠标，全程微动（目标 ≥50% 帧有位移）",
                   f"有鼠标位移的帧仅 {100*ptr_moving/n_win:.1f}%"))

    # ---- D 动态物体 --------------------------------------------------------
    rf = np.array([r.get("resid_frac", 0.0) for r in mv])
    moving = float((rf > 0.02).mean())
    res.append(manual("D 动态物体占比",
                      f"≥{100*DYN_PRESENT_MIN:.0f}% 镜头有动态物体、≥{100*DYN_MOVING_MIN:.0f}% 有在动的",
                      f"补偿全局位移后残差>2% 画面的帧占 {100*moving:.1f}%（机检为代理指标，"
                      f"须配合抽帧目检定性）"))
    return res


def markdown_table(results):
    lines = ["| 检查项 | 标准/口径 | 当前数据情况 | 是否通过 |", "|---|---|---|---|"]
    for r in results:
        lines.append("| " + " | ".join(
            str(v).replace("\n", " ").replace("|", "\\|")
            for v in (r.item, r.standard, r.actual, r.status)) + " |")
    return "\n".join(lines)


def conclusion(results):
    hard = [r for r in results if r.status == "不通过"]
    if hard:
        return "最终结论：暂不通过。硬性不通过项：" + "、".join(r.item for r in hard)
    if any(r.status == "需人工确认" for r in results):
        return "最终结论：机检项全过，仍有需人工确认项。"
    return "最终结论：通过。"


def read_intrinsics(root: Path):
    """从 camera-pose-index 读逐帧内参（docs/130 起随 session 上云）。

    `n_distinct` 是判「片中有没有改 FOV」的直接证据；多于一组时取覆盖行数最多的那组
    报出来，并把 distinct 数一起交给判定，不做静默合并。
    """
    rows, counts = 0, {}
    for idx in sorted(root.glob("runs/*/camera-pose-index.jsonl")):
        for line in idx.read_text().splitlines():
            if not line.strip():
                continue
            k = json.loads(line).get("camera_intrinsics") or {}
            if k.get("fx"):
                rows += 1
                key = (round(float(k["fx"]), 4), round(float(k.get("fy", k["fx"])), 4))
                counts[key] = counts.get(key, 0) + 1
    if not counts:
        return None
    (fx, _), _ = max(counts.items(), key=lambda kv: kv[1])
    f_1280 = fx * (DW / 1920.0)
    return {"fx_1920": fx, "f_1280": f_1280,
            "hfov_deg": 2 * math.degrees(math.atan(1920 / (2 * fx))),
            "n_rows": rows, "n_distinct": len(counts)}


def main():
    ap = argparse.ArgumentParser(description="Loop-有位移 SOP v2 视频侧自检")
    ap.add_argument("session_dir", type=Path, help="从 R2 取下的原始 session 目录")
    ap.add_argument("--json", action="store_true")
    ap.add_argument("--out", type=Path, default=None, help="中间产物目录（默认 session_dir/_sopv2）")
    ap.add_argument("--window", type=int, default=1800, help="交付窗帧数（60s=1800）")
    ap.add_argument("--window-start", type=int, default=0,
                    help="交付窗起始 session_frame（录制长于交付窗时用；默认 0）")
    ap.add_argument("--proj-a", type=float, default=None,
                    help="深度线性化 proj_a（默认按 depth-index 试算后决定）")
    ap.add_argument("--proj-b", type=float, default=None)
    ap.add_argument("--metric-scale", type=float, default=None)
    args = ap.parse_args()

    work = args.out or (args.session_dir / "_sopv2")
    work.mkdir(parents=True, exist_ok=True)
    segs = load_session(args.session_dir)
    video = concat_video(segs, work)
    total = sum(s["n_frames"] for s in segs)
    start = max(0, args.window_start)
    n_win = min(args.window, total - start)
    fps = segs[0]["fps"]

    sys.stderr.write(f"segments={len(segs)} frames={total} window={start}..{start + n_win - 1}\n")
    metrics, W, H, hud = per_frame_metrics(
        video, n_win, start=start,
        progress=lambda i: sys.stderr.write(f"  metrics {i}\n"))
    (work / "video_metrics.jsonl").write_text("".join(json.dumps(r) + "\n" for r in metrics))
    zooms = per_frame_zoom(video, n_win, start=start)
    (work / "zoom.jsonl").write_text("".join(json.dumps(r) + "\n" for r in zooms))

    bank = DepthBank([s["depth"] for s in segs if s["depth"]])
    sample = bank.raw(min(bank.map)) if (bank.map and _exr) else None
    proj, proj_src = resolve_projection(args.session_dir, sample,
                                        (args.proj_a, args.proj_b, args.metric_scale))
    bank.proj = proj
    sys.stderr.write(f"depth 线性化参数：a={proj[0]:.10g} b={proj[1]:.10g} "
                     f"metric_scale={proj[2]:.10g}（{proj_src}）\n")

    # 分别在「首个缩放跳变之前 / 之后」标定，跳变即改 FOV
    jumps = zoom_steps(zooms)
    split = jumps[0]["frame"] if jumps else start
    fov_late = measure_fov(video, bank, metrics, (split, start + n_win))
    fov_early = measure_fov(video, bank, metrics, (start, split)) if split - start > 120 else None
    fov_read = read_intrinsics(args.session_dir)
    inputs = read_input_stream(segs)

    results = evaluate(segs, metrics, zooms, fov_late, fov_early, inputs, W, H, n_win, fps, hud,
                       start=start, fov_read=fov_read)
    payload = {"session_dir": str(args.session_dir),
               "window": {"start": start, "frames": n_win, "session_frames": total},
               "projection": {"proj_a": proj[0], "proj_b": proj[1], "metric_scale": proj[2],
                              "provenance": proj_src},
               "segments": [{k: (str(v) if isinstance(v, Path) else v)
                             for k, v in s.items()} for s in segs],
               "fov_read": fov_read,
               # 下游（run_acceptance.sh）要的是「内参有没有在片中变过、变在哪一帧」。
               # 早先它靠正则从判定文案里抠 `f<帧号>(`，文案一改就会抠错——落成字段。
               "fov_constant": bool(fov_read and fov_read["n_distinct"] == 1) or not jumps,
               "zoom_split_frame": int(split) if jumps else 0,
               "fov_measured": {"after_change": fov_late, "before_change": fov_early},
               "hud_vote": {k: v for k, v in hud.items() if k != "cover"},
               "results": [asdict(r) for r in results],
               "conclusion": conclusion(results)}
    (work / "sopv2_result.json").write_text(json.dumps(payload, ensure_ascii=False, indent=2))
    if args.json:
        print(json.dumps(payload, ensure_ascii=False, indent=2))
    else:
        print(markdown_table(results))
        print()
        print(conclusion(results))
    return 1 if any(r.status == "不通过" for r in results) else 0


if __name__ == "__main__":
    raise SystemExit(main())

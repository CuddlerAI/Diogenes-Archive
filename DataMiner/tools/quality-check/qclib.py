#!/usr/bin/env python3
"""quality-check 各工具的公共实现：深度读取/线性化、位姿数学、刚体拟合。

原来 `exr_array` / `linearize` 各有 4 份拷贝、`DepthBank` 3 份、FO4 深度常数 3 份。
拷贝多了就会出现「修了一处漏了三处」——本单已经在 depth 通道名、camera_speed 类型、
姿态插值上各踩过一次。这里收敛成一处。

**不收敛阈值**：验收阈值一律在 `qcthresholds.py`，那里每条都带口径出处。
"""
from __future__ import annotations

import math
import zipfile
from pathlib import Path

import numpy as np

# --- FO4 深度线性化常数 ------------------------------------------------------
# 出处：depth-index v6 `projection.{proj_a,proj_b,metric_scale}`，见
# DataMiner/tools/pr405-fallout4/external-conversion.md §3/§5。
# proj_a/proj_b 只由近/远平面决定、metric_scale 是游戏单位常数 —— 三者都与 FOV 无关，
# 所以可以跨 run 复用（本单据此为未上传 depth-index 的 session 做线性化）。
PROJ_A = 1.0000420141220188
PROJ_B = -14.850629743114256
METRIC_SCALE = 0.0142875

DEPTH_W, DEPTH_H = 1280, 720
VIDEO_W, VIDEO_H = 1920, 1080
#: depth 是 1280 宽而交付内参按 1920 标定 ⇒ 两者差 1.5 倍（docs/104 §1.1 C2）
DEPTH_TO_VIDEO_SCALE = VIDEO_W / DEPTH_W


def _exr_reader():
    """延迟导入 engine_qc.exr —— 它在 engine-data/ 下，不是本目录的包。"""
    import sys
    root = Path(__file__).resolve().parents[2].parent / "engine-data" / "qc"
    if str(root) not in sys.path:
        sys.path.insert(0, str(root))
    from engine_qc import exr
    return exr


def exr_to_array(data: bytes) -> np.ndarray:
    """单通道 HALF EXR 字节流 → float32 HxW。

    ⚠️ **不要用 cv2 读**：单通道 `Z` 的 EXR 它不报错，返回形状正确的**全 0**
    （实测 mean=0.000，external-conversion §5）。一律走 engine_qc 的最小读取器。
    """
    img = _exr_reader().read(data)
    name = img.channels[0][0]
    return (np.asarray(img.bits[name], dtype=np.uint16).view(np.float16)
              .astype(np.float32).reshape(img.height, img.width))


def linearize_depth(raw: np.ndarray) -> np.ndarray:
    """raw one_minus_d 缓冲 → 米制 view-space 线性深度；无效像素为 NaN。"""
    s = raw.astype(np.float64)
    d = 1.0 - s
    with np.errstate(divide="ignore", invalid="ignore"):
        z = PROJ_B / (d - PROJ_A) * METRIC_SCALE
    z[s == 0] = np.nan                 # 清屏值
    z[~np.isfinite(z)] = np.nan
    z[z <= 0] = np.nan
    return z


class DepthBank:
    """按 session_frame 索引一组 depth-*.zip 里的 EXR。"""

    def __init__(self, zips):
        self.map = {}
        for zp in zips:
            with zipfile.ZipFile(zp) as z:
                for n in z.namelist():
                    if n.startswith("depth_") and n.endswith(".exr"):
                        self.map[int(n[6:-4])] = (zp, n)

    def __len__(self):
        return len(self.map)

    def __contains__(self, fr):
        return fr in self.map

    def get(self, fr):
        if fr not in self.map:
            return None
        zp, n = self.map[fr]
        with zipfile.ZipFile(zp) as z:
            return linearize_depth(exr_to_array(z.read(n)))


# --- 位姿数学 ---------------------------------------------------------------
def R_to_quat(M) -> np.ndarray:
    """旋转矩阵 → 四元数 (x, y, z, w)，已归一。"""
    M = np.asarray(M, dtype=float)
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


def quat_to_R(q) -> np.ndarray:
    x, y, z, w = q
    return np.array([
        [1 - 2 * (y * y + z * z), 2 * (x * y - z * w), 2 * (x * z + y * w)],
        [2 * (x * y + z * w), 1 - 2 * (x * x + z * z), 2 * (y * z - x * w)],
        [2 * (x * z - y * w), 2 * (y * z + x * w), 1 - 2 * (x * x + y * y)]])


def slerp(Ra, Rb, a: float) -> np.ndarray:
    """姿态插值必须走 slerp。

    ⚠️ 曾经这里是「就近取整」，而里程计只在偶数帧有位姿 —— 结果**一半的相邻帧旋转差
    恰好为 0**，`handheld_feel_check` 读出「旋转中位 0.0°/s」的假象，差点把一条运镜
    已经改对的数据判成「手太稳」（docs/121 §11.4）。
    """
    qa, qb = R_to_quat(Ra), R_to_quat(Rb)
    if float(np.dot(qa, qb)) < 0.0:
        qb = -qb
    d = float(np.clip(np.dot(qa, qb), -1.0, 1.0))
    if d > 0.9995:
        q = qa + a * (qb - qa)
        return quat_to_R(q / np.linalg.norm(q))
    th = math.acos(d)
    q = (math.sin((1 - a) * th) * qa + math.sin(a * th) * qb) / math.sin(th)
    return quat_to_R(q / np.linalg.norm(q))


def rot_angle_deg(Ra, Rb) -> float:
    """两个旋转之间的夹角（度）。"""
    Rr = np.asarray(Ra).T @ np.asarray(Rb)
    return math.degrees(math.acos(max(-1.0, min(1.0, (np.trace(Rr) - 1) / 2))))


# --- 刚体拟合 ---------------------------------------------------------------
def kabsch(P, Q):
    """最小化 |R·P + t − Q| 的刚体变换。"""
    cp, cq = P.mean(0), Q.mean(0)
    U, S, Vt = np.linalg.svd((P - cp).T @ (Q - cq))
    d = np.sign(np.linalg.det(Vt.T @ U.T))
    R = Vt.T @ np.diag([1, 1, d]) @ U.T
    return R, cq - R @ cp


def robust_rigid(P, Q, iters=4, pct=70, k=2.5, floor=0.015):
    """迭代重加权的刚体拟合，返回 (R, t, 内点残差中位, 内点数)。"""
    w = np.ones(len(P), bool)
    for _ in range(iters):
        if w.sum() < 20:
            break
        R, t = kabsch(P[w], Q[w])
        e = np.linalg.norm((R @ P.T).T + t - Q, axis=1)
        w = e < max(np.percentile(e[w], pct) * k, floor)
    R, t = kabsch(P[w], Q[w])
    e = np.linalg.norm((R @ P.T).T + t - Q, axis=1)
    return R, t, (float(np.median(e[w])) if w.sum() else float("nan")), int(w.sum())


def unproject(uv, z, f, w=DEPTH_W, h=DEPTH_H):
    """像素 + 深度 → 相机系三维点（主点取画幅中心）。"""
    cx, cy = (w - 1) / 2.0, (h - 1) / 2.0
    return np.stack([(uv[:, 0] - cx) * z / f, (uv[:, 1] - cy) * z / f, z], 1)


def hfov_from_focal(f, w=DEPTH_W) -> float:
    return 2 * math.degrees(math.atan(w / (2 * f)))

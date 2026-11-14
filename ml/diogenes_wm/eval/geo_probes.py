"""E-geo / E1a-pose 几何探针:用米制深度+位姿真值替换光流代理(43 号文 R0).

两个探针共享同一套针孔重投影核(numpy,无 cv2):

- **E-geo(3D 一致性)**:用第 t 帧的真值深度与 t→t+k 的真值相对位姿,把第 t+k 帧
  反向采样到第 t 帧视角,与第 t 帧比光度残差。真实录像上的读数 = 量具噪声地板
  (量化/动态物体/去遮挡);生成 rollout 上的读数 = 世界几何稳定性。
  报告一律与「不重投影直接比」的基线并列,改善比(improvement)才是有效信号——
  单看绝对残差会把纹理复杂度读成几何误差。

- **E1a-pose(位姿遵循)**:给定参考帧的真值深度,对候选 6-DoF 位姿增量做光度
  Gauss-Newton(有限差分雅可比),从帧对里反解相机运动,与条件化的真值位姿增量做
  相关。相比 E1a 的光流代理:米制、带符号、无搜索范围上限(Lane A 实测的大位移
  失效档在此路线下不存在)。在真实录像上跑 = 估计器天花板标定;在 rollout 上跑 =
  位姿遵循分数。

坐标约定(68 号交付系):世界与相机均为左手系 x 右/y 上/z 前;像素 u 向右、v 向下,
故 u = cx + fx·X/Z,v = cy − fy·Y/Z。位姿四元数(xyzw)为**相机→世界**旋转,
相对位姿 T_rel = inv(W_{t+k})·W_t。约定错一个符号,重投影改善比会立刻塌到 1 以下,
校准 runner 以此自检,不靠背书。
"""
from __future__ import annotations

from dataclasses import dataclass

import numpy as np


def quat_to_mat(q: np.ndarray) -> np.ndarray:
    """xyzw 四元数 → 3×3 旋转矩阵(相机→世界)。"""
    x, y, z, w = float(q[0]), float(q[1]), float(q[2]), float(q[3])
    return np.array(
        [
            [1 - 2 * (y * y + z * z), 2 * (x * y - z * w), 2 * (x * z + y * w)],
            [2 * (x * y + z * w), 1 - 2 * (x * x + z * z), 2 * (y * z - x * w)],
            [2 * (x * z - y * w), 2 * (y * z + x * w), 1 - 2 * (x * x + y * y)],
        ]
    )


@dataclass(frozen=True)
class Pinhole:
    fx: float
    fy: float
    cx: float
    cy: float

    def scaled(self, sx: float, sy: float) -> "Pinhole":
        return Pinhole(self.fx * sx, self.fy * sy, self.cx * sx, self.cy * sy)


@dataclass(frozen=True)
class RelPose:
    """t → t+k 的相对位姿:X_{t+k} = R @ X_t + t(相机系)。"""

    rotation: np.ndarray  # [3,3]
    translation: np.ndarray  # [3]

    @staticmethod
    def between(
        pos_t: np.ndarray, quat_t: np.ndarray, pos_tk: np.ndarray, quat_tk: np.ndarray
    ) -> "RelPose":
        r_t = quat_to_mat(np.asarray(quat_t))
        r_tk = quat_to_mat(np.asarray(quat_tk))
        rel_r = r_tk.T @ r_t
        rel_t = r_tk.T @ (np.asarray(pos_t, dtype=np.float64) - np.asarray(pos_tk, dtype=np.float64))
        return RelPose(rel_r, rel_t)


def _bilinear(img: np.ndarray, u: np.ndarray, v: np.ndarray) -> tuple[np.ndarray, np.ndarray]:
    """双线性采样;返回 (值, 界内掩码)。img 为 [H,W] float。"""
    h, w = img.shape
    finite = np.isfinite(u) & np.isfinite(v)
    inb = finite & (u >= 0) & (u <= w - 1.001) & (v >= 0) & (v <= h - 1.001)
    # Invalid or behind-camera reprojections are masked out by ``inb``.  Replace
    # their coordinates before the integer cast nevertheless: NumPy converts
    # NaN/Inf to INT_MIN, which would turn a harmless rejected sample into an
    # out-of-bounds array access.
    uc = np.clip(np.where(finite, u, 0.0), 0, w - 1.001)
    vc = np.clip(np.where(finite, v, 0.0), 0, h - 1.001)
    u0 = uc.astype(np.int32)
    v0 = vc.astype(np.int32)
    du = uc - u0
    dv = vc - v0
    val = (
        img[v0, u0] * (1 - du) * (1 - dv)
        + img[v0, u0 + 1] * du * (1 - dv)
        + img[v0 + 1, u0] * (1 - du) * dv
        + img[v0 + 1, u0 + 1] * du * dv
    )
    return val, inb


def reproject(
    depth_t: np.ndarray, intr: Pinhole, rel: RelPose, v_up_sign: float = -1.0
) -> tuple[np.ndarray, np.ndarray, np.ndarray]:
    """把第 t 帧每个有效像素投到 t+k 视角;返回 (u', v', 有效掩码)。

    `v_up_sign`:像素 v 轴与相机 Y 轴的符号关系,交付系约定为 −1(y 上/v 下)。
    校准 runner 会对两种取值实测取优并记录——约定自检优于文档背书。
    """
    h, w = depth_t.shape
    v_grid, u_grid = np.mgrid[0:h, 0:w].astype(np.float64)
    z = depth_t.astype(np.float64)
    valid = np.isfinite(z) & (z > 0)
    # Keep invalid depth samples inert during matrix multiplication.  Their
    # validity bit remains false, but zeroing them here also avoids 0*Inf ->
    # NaN contamination and noisy runtime warnings.
    safe_z = np.where(valid, z, 0.0)
    x = (u_grid - intr.cx) / intr.fx * safe_z
    y = v_up_sign * (v_grid - intr.cy) / intr.fy * safe_z
    pts = np.stack([x, y, safe_z], axis=0).reshape(3, -1)
    moved = rel.rotation @ pts + rel.translation[:, None]
    zk = moved[2].reshape(h, w)
    front = zk > 1e-6
    with np.errstate(divide="ignore", invalid="ignore"):
        uk = intr.cx + intr.fx * moved[0].reshape(h, w) / zk
        vk = intr.cy + v_up_sign * intr.fy * moved[1].reshape(h, w) / zk
    return uk, vk, valid & front


@dataclass(frozen=True)
class EGeoResult:
    photo_l1: float
    baseline_l1: float
    valid_frac: float

    @property
    def improvement(self) -> float:
        """>1 = 重投影后比不动更像;≤1 = 几何不成立(或约定错)。"""
        return self.baseline_l1 / self.photo_l1 if self.photo_l1 > 0 else float("inf")


def e_geo(
    gray_t: np.ndarray,
    gray_tk: np.ndarray,
    depth_t: np.ndarray,
    intr: Pinhole,
    rel: RelPose,
    v_up_sign: float = -1.0,
) -> EGeoResult:
    """单帧对的 E-geo:重投影光度残差 vs 不重投影基线。"""
    uk, vk, valid = reproject(depth_t, intr, rel, v_up_sign)
    sampled, inb = _bilinear(gray_tk.astype(np.float64), uk, vk)
    mask = valid & inb
    if not mask.any():
        return EGeoResult(float("nan"), float("nan"), 0.0)
    photo = float(np.mean(np.abs(sampled[mask] - gray_t.astype(np.float64)[mask])))
    base = float(np.mean(np.abs(gray_tk.astype(np.float64)[mask] - gray_t.astype(np.float64)[mask])))
    return EGeoResult(photo, base, float(mask.mean()))


def _rel_with_delta(rel: RelPose, delta: np.ndarray) -> RelPose:
    """在 rel 上叠加小增量 [tx,ty,tz,ry,rx,rz](弧度,小角近似的一阶旋转)。"""
    tx, ty, tz, ry, rx, rz = (float(d) for d in delta)
    dr = np.array([[1, -rz, ry], [rz, 1, -rx], [-ry, rx, 1]], dtype=np.float64)
    return RelPose(dr @ rel.rotation, rel.translation + np.array([tx, ty, tz]))


def estimate_pose(
    gray_t: np.ndarray,
    gray_tk: np.ndarray,
    depth_t: np.ndarray,
    intr: Pinhole,
    init: RelPose,
    iters: int = 12,
    stride: int = 4,
    v_up_sign: float = -1.0,
) -> RelPose:
    """光度 Gauss-Newton 反解相对位姿(有限差分雅可比,像素抽样 stride)。

    刻意从 `init`(通常取单位位姿或上一对的解)出发而不是真值——从真值出发
    只能证明「真值附近是极小值」,证明不了估计器有召回。
    """
    d = depth_t[::stride, ::stride]
    gt = gray_t[::stride, ::stride]
    gk = gray_tk  # 采样端保持全分辨率
    intr_s = Pinhole(intr.fx, intr.fy, intr.cx, intr.cy)

    def residual(rel: RelPose) -> float:
        uk, vk, valid = reproject(d, Pinhole(intr_s.fx / stride, intr_s.fy / stride,
                                             intr_s.cx / stride, intr_s.cy / stride), rel, v_up_sign)
        sampled, inb = _bilinear(gk[::stride, ::stride].astype(np.float64), uk, vk)
        m = valid & inb
        if not m.any():
            return 1e9
        return float(np.mean(np.abs(sampled[m] - gt.astype(np.float64)[m])))

    cur = init
    # 步长:平移米级 1cm,旋转 0.1°——与交付数据的帧间运动同量级
    eps = np.array([0.01, 0.01, 0.01, 0.0017, 0.0017, 0.0017])
    for _ in range(iters):
        r0 = residual(cur)
        grad = np.zeros(6)
        for i in range(6):
            step = np.zeros(6)
            step[i] = eps[i]
            grad[i] = (residual(_rel_with_delta(cur, step)) - r0) / eps[i]
        norm = float(np.linalg.norm(grad))
        if norm < 1e-9:
            break
        # 带回退的线搜索
        for scale in (1.0, 0.3, 0.1):
            cand = _rel_with_delta(cur, -grad / norm * eps * scale * 4)
            if residual(cand) < r0:
                cur = cand
                break
        else:
            break
    return cur


def yaw_of(rel: RelPose) -> float:
    """相对旋转的 yaw 分量(弧度,绕 y):E1a-pose 的主轴读数。"""
    return float(np.arctan2(rel.rotation[0, 2], rel.rotation[2, 2]))

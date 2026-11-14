"""geo_probes 合成场景测试:已知几何下重投影必须闭环、位姿估计必须有召回."""
from __future__ import annotations

import numpy as np
import pytest

from diogenes_wm.eval.geo_probes import (
    EGeoResult,
    Pinhole,
    RelPose,
    _bilinear,
    e_geo,
    estimate_pose,
    quat_to_mat,
    yaw_of,
)

H, W = 120, 160
INTR = Pinhole(fx=140.0, fy=140.0, cx=W / 2, cy=H / 2)
PLANE_Z = 5.0


def _texture(x: np.ndarray, y: np.ndarray) -> np.ndarray:
    """平滑但非周期的纹理——周期纹理会给位姿估计留混叠假极小值。"""
    return (
        128
        + 60 * np.sin(1.7 * x + 0.9 * y)
        + 40 * np.sin(3.1 * x - 2.3 * y + 0.5)
        + 20 * np.sin(0.7 * x * y)
    )


def _render(cam_pos: np.ndarray, yaw: float) -> np.ndarray:
    """从位于 cam_pos、绕 y 旋转 yaw 的相机渲染世界平面 z=PLANE_Z(左手系,y 上 v 下)。"""
    v_grid, u_grid = np.mgrid[0:H, 0:W].astype(np.float64)
    dx = (u_grid - INTR.cx) / INTR.fx
    dy = -(v_grid - INTR.cy) / INTR.fy
    dirs = np.stack([dx, dy, np.ones_like(dx)], axis=0).reshape(3, -1)
    r = np.array(
        [[np.cos(yaw), 0, np.sin(yaw)], [0, 1, 0], [-np.sin(yaw), 0, np.cos(yaw)]]
    )
    world_dirs = r @ dirs
    t = (PLANE_Z - cam_pos[2]) / world_dirs[2]
    px = cam_pos[0] + t * world_dirs[0]
    py = cam_pos[1] + t * world_dirs[1]
    return _texture(px, py).reshape(H, W)


def _depth(cam_pos: np.ndarray) -> np.ndarray:
    """光轴向深度:平面 z=PLANE_Z、相机无俯仰时逐像素 Z 相同(view-space Z 非欧氏)。"""
    return np.full((H, W), PLANE_Z - cam_pos[2])


def _quat_yaw(yaw: float) -> np.ndarray:
    return np.array([0.0, np.sin(yaw / 2), 0.0, np.cos(yaw / 2)])


def test_quat_matches_yaw_matrix() -> None:
    yaw = 0.3
    r = quat_to_mat(_quat_yaw(yaw))
    expect = np.array(
        [[np.cos(yaw), 0, np.sin(yaw)], [0, 1, 0], [-np.sin(yaw), 0, np.cos(yaw)]]
    )
    assert np.abs(r - expect).max() < 1e-12


@pytest.mark.parametrize(
    "pos_tk,yaw_tk",
    [
        (np.array([0.15, 0.0, 0.0]), 0.0),   # 右移
        (np.array([0.0, 0.0, 0.4]), 0.0),    # 前进
        (np.array([0.0, 0.0, 0.0]), 0.03),   # 右转 1.7°
        (np.array([0.1, -0.05, 0.3]), -0.02) # 复合
    ],
)
def test_e_geo_improves_on_true_geometry(pos_tk: np.ndarray, yaw_tk: float) -> None:
    pos_t = np.zeros(3)
    g_t = _render(pos_t, 0.0)
    g_tk = _render(pos_tk, yaw_tk)
    rel = RelPose.between(pos_t, _quat_yaw(0.0), pos_tk, _quat_yaw(yaw_tk))
    res: EGeoResult = e_geo(g_t, g_tk, _depth(pos_t), INTR, rel)
    assert res.valid_frac > 0.5
    # 真几何下重投影残差必须远小于不动基线
    assert res.improvement > 5.0, res
    # 错误几何(逆位姿:运动方向整体反向)必须塌下去——探针要能双向报警
    bad = RelPose(rel.rotation.T, -(rel.rotation.T @ rel.translation))
    res_bad = e_geo(g_t, g_tk, _depth(pos_t), INTR, bad)
    assert res_bad.improvement < res.improvement / 2


def test_e_geo_rejects_nonfinite_reprojections_without_indexing_them() -> None:
    gray = np.arange(H * W, dtype=np.float64).reshape(H, W)
    depth = np.full((H, W), PLANE_Z)
    depth[0, 0] = np.nan
    depth[0, 1] = np.inf
    depth[0, 2] = 0.0
    rel = RelPose(np.eye(3), np.zeros(3))

    result = e_geo(gray, gray, depth, INTR, rel)

    assert np.isfinite(result.photo_l1)
    assert result.photo_l1 < 1e-9
    assert 0.98 < result.valid_frac < 1.0


def test_bilinear_masks_nonfinite_coordinates_before_integer_cast() -> None:
    image = np.arange(16, dtype=np.float64).reshape(4, 4)
    values, in_bounds = _bilinear(
        image,
        np.array([1.5, np.nan, np.inf, -np.inf]),
        np.array([1.5, 1.0, 1.0, 1.0]),
    )

    assert values[0] == 7.5
    assert in_bounds.tolist() == [True, False, False, False]


def test_estimate_pose_recovers_motion_from_identity_init() -> None:
    pos_t = np.zeros(3)
    pos_tk = np.array([0.08, 0.0, 0.25])
    yaw_tk = 0.02
    g_t = _render(pos_t, 0.0)
    g_tk = _render(pos_tk, yaw_tk)
    ident = RelPose(np.eye(3), np.zeros(3))
    est = estimate_pose(g_t, g_tk, _depth(pos_t), INTR, ident, iters=40, stride=2)
    true_rel = RelPose.between(pos_t, _quat_yaw(0.0), pos_tk, _quat_yaw(yaw_tk))
    # 召回判据:yaw 方向正确且量级同阶;平移主轴(前进)方向正确
    assert np.sign(yaw_of(est)) == np.sign(yaw_of(true_rel))
    assert abs(yaw_of(est) - yaw_of(true_rel)) < 0.015
    assert np.sign(est.translation[2]) == np.sign(true_rel.translation[2])

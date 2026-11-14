"""摆位解算：从 plan↔clip 双车道位姿闭式解出「地图局部系 → 世界系」刚体变换。

docs/88 §6 残余 #1 的处置（辩题二十四裁决乙）：世界系边界不靠采集侧诚实记录，
而是事后从两份 30fps 位姿序列解 Procrustes（水平 yaw + 平移），残差就是证据——
最坏残差过不了门槛（默认 1 mm）就拒绝产出，绝不编造边界。

约定（与 `map_meta.json`/`action_camera` 一致的左手系、y 向上、米）：

    world = R(yaw_xz) · local + t，
    R(yaw_xz) = [[cos, 0, -sin], [0, 1, 0], [sin, 0, cos]]

`yaw_xz_deg` 即该公式中的角度；真机验证：sfm-d1-20260805 解得
yaw=-157.323364° / t=(60, 31.6727, 220)，1800 帧最坏残差 8e-6 m（纯刚体摆位）。
"""

import json
import math
from pathlib import Path
from typing import Dict, List, Optional, Sequence, Tuple


class PlacementError(ValueError):
    """解算失败（形状不符 / 无水平信息 / 残差超门槛）——宁可不给，不给错的。"""


def _positions(frames: Sequence[dict], path_label: str) -> List[Tuple[float, float, float]]:
    out = []
    for f in frames:
        p = f.get("camera_position") if isinstance(f, dict) else None
        if not (isinstance(p, list) and len(p) == 3
                and all(isinstance(x, (int, float)) and not isinstance(x, bool) for x in p)):
            raise PlacementError(f"{path_label} 第 {len(out)} 项缺 camera_position")
        out.append((float(p[0]), float(p[1]), float(p[2])))
    return out


def solve_yaw_translation(plan_frames: Sequence[dict], clip_frames: Sequence[dict],
                          min_spread_m: float = 0.5,
                          ) -> Tuple[float, Tuple[float, float, float], float]:
    """解 world = R(yaw)·local + t，返回 (yaw_xz_deg, t, worst_residual_m)。

    水平面 Procrustes 闭式解 + y 取均值差。`min_spread_m`：水平轨迹散布
    （质心平均距离）低于此值时 yaw 不可辨识（如 Stay-完全静止），拒绝解算。
    """
    if len(plan_frames) != len(clip_frames) or not plan_frames:
        raise PlacementError(
            f"两车道帧数不一致或为空：plan {len(plan_frames)} vs clip {len(clip_frames)}")
    P = _positions(plan_frames, "plan")
    C = _positions(clip_frames, "clip")
    n = len(P)
    mpx = sum(p[0] for p in P) / n
    mpz = sum(p[2] for p in P) / n
    mcx = sum(c[0] for c in C) / n
    mcz = sum(c[2] for c in C) / n
    spread = sum(math.hypot(p[0] - mpx, p[2] - mpz) for p in P) / n
    if spread < min_spread_m:
        raise PlacementError(
            f"plan 水平散布仅 {spread:.3f} m（< {min_spread_m}），yaw 不可辨识"
            "（近似原地轨迹）——摆位需从采集侧记录，无法事后解算")
    num = den = 0.0
    for (px, _, pz), (cx, _, cz) in zip(P, C):
        ax, az = px - mpx, pz - mpz
        bx, bz = cx - mcx, cz - mcz
        num += ax * bz - az * bx
        den += ax * bx + az * bz
    yaw = math.atan2(num, den)
    cy, sy = math.cos(yaw), math.sin(yaw)
    tx = mcx - (cy * mpx - sy * mpz)
    tz = mcz - (sy * mpx + cy * mpz)
    ty = sum(c[1] - p[1] for p, c in zip(P, C)) / n
    worst = 0.0
    for (px, py, pz), (cx, cyv, cz) in zip(P, C):
        ex = cy * px - sy * pz + tx - cx
        ey = py + ty - cyv
        ez = sy * px + cy * pz + tz - cz
        worst = max(worst, math.sqrt(ex * ex + ey * ey + ez * ez))
    return math.degrees(yaw), (tx, ty, tz), worst


def _apply(yaw_xz_deg: float, t: Sequence[float],
           p: Sequence[float]) -> Tuple[float, float, float]:
    yaw = math.radians(yaw_xz_deg)
    cy, sy = math.cos(yaw), math.sin(yaw)
    return (cy * p[0] - sy * p[2] + t[0],
            p[1] + t[1],
            sy * p[0] + cy * p[2] + t[2])


def world_block(map_meta: dict, yaw_xz_deg: float, t: Sequence[float],
                worst_residual_m: float, frames: int) -> dict:
    """由局部 map_meta 生成 `world` 块：角点精确旋转，AABB 取旋转后外接盒。"""
    lo, hi = map_meta.get("bounds_lo"), map_meta.get("bounds_hi")
    if not (isinstance(lo, list) and isinstance(hi, list) and len(lo) == len(hi) == 3):
        raise PlacementError("map_meta 缺 bounds_lo/bounds_hi，无法生成 world 块")
    corners = [_apply(yaw_xz_deg, t, (x, y, z))
               for x in (lo[0], hi[0]) for y in (lo[1], hi[1]) for z in (lo[2], hi[2])]
    wlo = [min(c[i] for c in corners) for i in range(3)]
    whi = [max(c[i] for c in corners) for i in range(3)]
    block = {
        "derived_by": "engine_qc map-world（docs/88 辩题二十四）",
        "solved_from": "plan/action_camera_30fps.json ↔ clip/action_camera_30fps.json",
        "frames": frames,
        "yaw_xz_deg": round(yaw_xz_deg, 6),
        "translation": [round(x, 4) for x in t],
        "formula": "world = [[cos,0,-sin],[0,1,0],[sin,0,cos]](yaw_xz)·local + translation",
        "worst_residual_m": float(f"{worst_residual_m:.3g}"),
        "bounds_lo": [round(x, 3) for x in wlo],
        "bounds_hi": [round(x, 3) for x in whi],
        "note": "AABB 为局部包围盒旋转后的外接盒（over-estimate，与局部 note 同口径）",
    }
    gc = map_meta.get("ground_corners_xz")
    if isinstance(gc, list):
        rotated = [_apply(yaw_xz_deg, t, (c[0], 0.0, c[1]))
                   for c in gc if isinstance(c, list) and len(c) == 2]
        block["ground_corners_xz"] = [[round(w[0], 3), round(w[2], 3)] for w in rotated]
    yr = map_meta.get("y_range")
    if isinstance(yr, list) and len(yr) == 2:
        block["y_range"] = [round(yr[0] + t[1], 3), round(yr[1] + t[1], 3)]
    return block


def derive_run_world(run_dir, residual_tol_m: float = 1e-3) -> Tuple[dict, str]:
    """对一个跑录目录（含 plan/ 与 clip/）解算并返回（更新后的 map_meta, 摘要行）。

    只读不写；写盘由调用方（CLI）决定。残差超 `residual_tol_m` 直接抛
    PlacementError——plan↔clip 不是刚体关系时，任何边界换算都是编造。
    """
    run = Path(run_dir)
    paths = {
        "plan_ac": run / "plan" / "action_camera_30fps.json",
        "clip_ac": run / "clip" / "action_camera_30fps.json",
        "map_meta": run / "plan" / "map_meta.json",
    }
    for label, p in paths.items():
        if not p.is_file():
            raise PlacementError(f"缺 {label}：{p}")
    plan_frames = json.loads(paths["plan_ac"].read_text(encoding="utf-8"))
    clip_frames = json.loads(paths["clip_ac"].read_text(encoding="utf-8"))
    meta = json.loads(paths["map_meta"].read_text(encoding="utf-8"))
    yaw, t, worst = solve_yaw_translation(plan_frames, clip_frames)
    if worst > residual_tol_m:
        raise PlacementError(
            f"plan↔clip 最坏残差 {worst:.4g} m > 门槛 {residual_tol_m}——"
            "两车道不是刚体摆位关系（轨迹被重规划/裁剪？），拒绝产出世界系边界")
    meta["world"] = world_block(meta, yaw, t, worst, len(plan_frames))
    summary = (f"yaw_xz={yaw:.4f}° t=({t[0]:.4f}, {t[1]:.4f}, {t[2]:.4f}) "
               f"残差 {worst:.3g} m（{len(plan_frames)} 帧）")
    return meta, summary


def find_map_meta(clip_root: Path) -> Optional[Path]:
    """map_meta 发现顺序：clip 根 > 兄弟 plan/（引擎跑录布局，与 traj_class 同策略）。"""
    for p in (clip_root / "map_meta.json", clip_root.parent / "plan" / "map_meta.json"):
        if p.is_file():
            return p
    return None

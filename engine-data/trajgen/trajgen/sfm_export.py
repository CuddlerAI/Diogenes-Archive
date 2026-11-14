"""68 号交付 json 导出：`action_camera_30fps` / `action_camera_15fps` + systeminfo + plan。

键名与**键序**逐字取自 `engine-data/fixtures/sfm_action_camera_15fps.sample.json`
（PR #405 归档的需求方样例）。SFM 侧的键名怪癖与 MC 侧相反，两边不能互抄：
小写 `camera_follow_offset`、驼峰 `mouseX`/`mouseY`、15fps 独有 `depth_name`。

15fps 文件是 30fps 的**偶数帧投影**，不是另采一遍——「depth 与 rgb 帧、camera pose
严格对齐」因此是构造性的（docs/68 §6.5 把它列为单独质检项）。
"""

import json
import os
from typing import Dict, List, Optional, Sequence

from .sfm import FPS, SfmPlan

DEFAULT_T0 = "2026-08-04 10:00:00.000"

#: depth 分辨率（docs/68 §6.4）。json 里的内参描述的是**视频**相机，
#: depth 内参由消费方按分辨率比换算（docs/75 问题 10 已固化口径）。
DEPTH_W, DEPTH_H = 1280, 720
VIDEO_W, VIDEO_H = 1920, 1080


def _timestamp(base: str, frame: int, fps: float) -> str:
    from datetime import datetime, timedelta
    t0 = datetime.strptime(base, "%Y-%m-%d %H:%M:%S.%f")
    t = t0 + timedelta(seconds=frame / fps)
    return t.strftime("%Y-%m-%d %H:%M:%S.") + "%03d" % (t.microsecond // 1000)


def _r(v: Sequence[float], nd: int = 6) -> List[float]:
    return [round(float(x), nd) for x in v]


def intrinsics(vertical_fov_deg: float = 58.72) -> Dict[str, float]:
    """方形像素针孔：fx=fy=(H/2)/tan(vFOV/2)，主点在图心。

    方形像素是 docs/75 问题 6 的已固化口径——三份样例里我方能完整取得的两份
    （68 号 15fps、69 号 camN）都是 fx==fy 且主点恰在 (W/2, H/2)。
    """
    import math
    f = (VIDEO_H / 2.0) / math.tan(math.radians(vertical_fov_deg) / 2.0)
    return {"fx": round(f, 4), "fy": round(f, 4),
            "cx": VIDEO_W / 2.0, "cy": VIDEO_H / 2.0}


def frame_records(plan: SfmPlan, fps_variant: int, base_time: str = DEFAULT_T0,
                  vertical_fov_deg: float = 58.72) -> List[Dict]:
    """fps_variant ∈ {30, 15}。15 只取偶数帧并带 depth_name。"""
    if fps_variant not in (30, 15):
        raise ValueError("fps_variant 只能是 30 或 15")
    k = plan.intrinsics_cache if hasattr(plan, "intrinsics_cache") else intrinsics(vertical_fov_deg)
    out: List[Dict] = []
    step = 1 if fps_variant == 30 else 2
    for i in range(0, plan.n_frames, step):
        rec: Dict = {
            "time": _timestamp(base_time, i, FPS),
            "fps": FPS,
        }
        if fps_variant == 15:
            rec["depth_name"] = "depth_%d" % i        # 与 DepthEncoding.DepthName 同口径
        rec.update({
            "frame": i,
            "camera_position": _r(plan.positions[i]),
            "camera_rotation_quaternion": _r(plan.quats[i]),
            # 自由相机轨迹无跟随目标 → 恒零向量（docs/70 §9 第 4 条待确认，如实标注）
            "camera_follow_offset": [0.0, 0.0, 0.0],
            "camera_intrinsics": dict(k),
            "camera_speed": _r(plan.speeds[i]),
            # 无角色轨迹：player_* 取相机眼位（第一人称下二者本就重合）
            "player_position": _r(plan.positions[i]),
            "mouseX": 0.5, "mouseY": 0.5,
            "mouse_dx": 0.0, "mouse_dy": 0.0,
            "keyCode": [],
            "player_rotation_quaternion": _r(plan.quats[i]),
            "player_speed": _r(plan.speeds[i]),
            "metric_scale": 1.0,
        })
        out.append(rec)
    return out


def systeminfo(process_name: str = "DiogenesEngineScenes") -> Dict:
    """引擎产线下如实填：全屏 0,0 / 1080p / dpi 1.0。"""
    return {"gameProcessName": process_name, "x": 0, "y": 0,
            "width": VIDEO_W, "height": VIDEO_H, "recordDpi": 1.0}


def map_meta(plan: SfmPlan) -> Dict:
    """docs/68 §6.1 前置约定：地图比例 + 边界 4 坐标（缺口 D9）。

    引擎侧这两项是 authoring 数据，直接从场景包围盒出——docs/77 §8.2.1 逐通道排除后
    写明「引擎自产线不受此限」，市售游戏侧则拿不到。非矩形可行域按 AABB 高估并注明
    （docs/75 问题 9(2) 的已固化口径）。
    """
    lo, hi = plan.scene.lo, plan.scene.hi
    return {
        "scene": plan.scene.name,
        "metric_scale": 1.0,
        "units": "meters",
        "bounds_lo": _r(lo, 3),
        "bounds_hi": _r(hi, 3),
        # xz 平面四角点 + y 范围（docs/75 问题 9(1) 的已固化口径）
        "ground_corners_xz": [[round(lo[0], 3), round(lo[2], 3)],
                              [round(hi[0], 3), round(lo[2], 3)],
                              [round(hi[0], 3), round(hi[2], 3)],
                              [round(lo[0], 3), round(hi[2], 3)]],
        "y_range": [round(lo[1], 3), round(hi[1], 3)],
        "shape": "AABB",
        "note": "非矩形可行域按轴对齐包围盒高估，为外接盒（docs/75 问题 9(2)）",
    }


def write_clip(plan: SfmPlan, out_dir, base_time: str = DEFAULT_T0,
               process_name: str = "DiogenesEngineScenes",
               vertical_fov_deg: float = 58.72) -> str:
    """写出次级文件夹（不含 video/depth.zip，那两样由采集侧产出）。"""
    out_dir = str(out_dir)
    os.makedirs(out_dir, exist_ok=True)

    def dump(name, obj):
        with open(os.path.join(out_dir, name), "w", encoding="utf-8") as f:
            json.dump(obj, f, ensure_ascii=False, indent=2)
            f.write("\n")

    dump("action_camera_30fps.json", frame_records(plan, 30, base_time, vertical_fov_deg))
    dump("action_camera_15fps.json", frame_records(plan, 15, base_time, vertical_fov_deg))
    dump("systeminfo.json", systeminfo(process_name))
    dump("plan.json", plan.to_plan_dict())
    dump("map_meta.json", map_meta(plan))
    return out_dir

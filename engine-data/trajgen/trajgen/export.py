"""计划 → 次级文件夹（12 文件口径的 json 部分 + 内部元数据）。

键名**逐字照需求方样例**（`engine-data/fixtures/`），不做任何「修正」：
`camera_Follow Offset` 带空格与大写、MC 侧鼠标是 `mouse_x/mouse_y`（不是 68 的驼峰 `mouseX`）。
蓝白字段按 0803 实测判定（docs/71 §8）：蓝=每相机必含，白=仅 cam1 携带。

程序化轨迹里几个「需求方未定义/我方填法」的字段（docs/70 §4.1、docs/71 §8 待确认项），
全部集中在这里并写进 `provenance.json`，需求方回复后只需改这一处重导：
- `camera_Follow Offset`：自由相机无第三人称 rig ⇒ 零向量（docs/70 §4.1 🟨 待确认）
- `player_*`：无角色 ⇒ 取相机的地面投影（视高带下沉）；速度由该投影**逐帧后向差分**
  得到（与采集侧 `PoseRecorder` 逐字同口径，含竖直分量）（🟨 待确认）
- `mouse_x/y`：程序化无光标 ⇒ 恒 0.5；`mouse_dx/dy` 由逐帧朝向增量按满幅归一化
- `keyCode`：程序化段无输入 ⇒ 空数组（docs/71 §8 G-19「程序化段无输入」口径）
- `camera_rotation_oula` / `player_rotation_oula`：Unity `eulerAngles` 口径（顺规 🟨 docs/72 C3）
"""

import json
import math
import os
from pathlib import Path
from typing import Dict, List, Optional, Sequence

from .mathutil import Vec3, clamp, quat_from_yaw_elev_roll, wrap180
from .planner import ASPECT_RES, DURATION_S, FPS, N_FRAMES, ClipPlan
from .sampler import frame_speeds
from .scene import SCENE_KINDS

#: 相机视高（用于把相机位置下沉成 player 位置）
EYE_HEIGHT_M = 1.44
#: 鼠标 delta 归一化满幅（度/帧）——仅为把朝向增量落进 [-1,1]，非需求方规格
MOUSE_FULL_SCALE_DEG = 6.0
#: 时间戳基准（墙钟仅描述性，帧对齐一律用 frame 序号——沿用 docs/44 的墙钟纪律）
DEFAULT_T0 = "2026-08-04 00:00:00.000"


def _timestamp(base: str, frame: int, fps: float) -> str:
    from datetime import datetime, timedelta
    t0 = datetime.strptime(base, "%Y-%m-%d %H:%M:%S.%f")
    t = t0 + timedelta(seconds=frame / fps)
    return t.strftime("%Y-%m-%d %H:%M:%S.") + "%03d" % (t.microsecond // 1000)


def _r3(v: Sequence[float], nd: int = 6) -> List[float]:
    return [round(float(x), nd) for x in v]


def frame_records(plan: ClipPlan, cam_index: int, base_time: str = DEFAULT_T0) -> List[Dict]:
    """一路的逐帧记录。cam_index 从 0 起；cam_index==0 时携带白色字段。

    车辆导演模式（docs/78 §10）：Track 带 player_*/follow_offset 覆盖时，白字段取
    车体位姿、`camera_Follow Offset` 取挂载偏移（schema 语义如实）；否则沿用
    程序化口径（相机地面投影 + 零向量）。
    """
    tr = plan.tracks[cam_index]
    intr = plan.intrinsics(tr.focal_mm)
    is_main = (tr.cam_id == 1)
    follow = _r3(tr.follow_offset) if tr.follow_offset is not None else [0.0, 0.0, 0.0]
    # 程序化口径的 `player_speed` 由**导出的** `player_position` 逐帧后向差分得到，
    # 与采集侧 `PoseRecorder`（`(playerPos − prevPlayerPos)·fps`，全三维）逐字一致
    # ——车辆导演那一支（下面 `player_positions is not None`）本来就是这么写的。
    # 程序化这一支此前取相机速度并把 y 硬写 0，而 `player_position` 的 y 跟着相机走
    # （`p[1] − EYE_HEIGHT_M`），一有升降镜头两者就对不上，
    # `engine_qc` 的 `num.player_speed` 因此在每一条 clip 上都判 FAIL。
    player_pos = [(p[0], p[1] - EYE_HEIGHT_M, p[2]) for p in tr.positions]
    player_spd = frame_speeds(player_pos, FPS)
    out: List[Dict] = []
    prev_yaw = prev_elev = None
    prev_pp = None
    n = len(tr.positions)
    for i in range(n):
        rec: Dict = {
            "time": _timestamp(base_time, i, FPS),
            "fps": FPS,
            "frame": i,
            "camera_position": _r3(tr.positions[i]),
            "camera_rotation_quaternion": _r3(tr.quats[i]),
            "camera_Follow Offset": list(follow),
            "camera_intrinsics": dict(intr),
            "camera_speed": _r3(tr.speeds[i]),
        }
        if is_main:
            eul = tr.eulers[i]
            yaw, elev = eul[1], -eul[0]
            dyaw = 0.0 if prev_yaw is None else wrap180(yaw - prev_yaw)
            delev = 0.0 if prev_elev is None else (elev - prev_elev)
            prev_yaw, prev_elev = yaw, elev
            if tr.player_positions is not None:
                pp = tr.player_positions[i]
                p_speed = ((0.0, 0.0, 0.0) if prev_pp is None else
                           tuple((pp[k] - prev_pp[k]) * FPS for k in range(3)))
                prev_pp = pp
                rec.update({
                    "camera_rotation_oula": _r3(eul, 4),
                    "player_position": _r3(pp),
                    "player_rotation_oula": _r3(tr.player_eulers[i], 4),
                    "player_rotation_quaternion": _r3(tr.player_quats[i]),
                    "player_speed": _r3(p_speed),
                })
            else:
                rec.update({
                    "camera_rotation_oula": _r3(eul, 4),
                    "player_position": _r3(player_pos[i]),
                    "player_rotation_oula": [0.0, round(eul[1], 4), 0.0],
                    "player_rotation_quaternion": _r3(quat_from_yaw_elev_roll(yaw, 0.0, 0.0)),
                    "player_speed": _r3(player_spd[i]),
                })
            rec.update({
                "mouse_x": 0.5,
                "mouse_y": 0.5,
                "mouse_dx": round(clamp(dyaw / MOUSE_FULL_SCALE_DEG, -1.0, 1.0), 6),
                "mouse_dy": round(clamp(-delev / MOUSE_FULL_SCALE_DEG, -1.0, 1.0), 6),
                "keyCode": [],
                "metric_scale": 1.0,
            })
        out.append(rec)
    return out


def systeminfo(plan: ClipPlan, process_name: str = "DiogenesEngineScenes") -> Dict:
    w, h = plan.resolution()
    return {"gameProcessName": process_name, "x": 0, "y": 0,
            "width": w, "height": h, "recordDpi": 1.0}


def provenance(plan: ClipPlan) -> Dict:
    """我方返工与撤销的追溯底座（docs/70 §4.1，需求方不要求）。"""
    return {
        "generator": "engine-data-trajgen",
        "seed": plan.seed,
        # `synthetic` 曾是写死的 True——M3a 只有合成场景时是对的，但真实场景
        # 接进来之后就一直在**给交付件写假话**：Factory / Robotics-Warehouse /
        # CARLA Town 的占据图都来自真实场景导出，provenance 里却仍标着
        # 「M3a 排程仿真用合成场景」。按场景类型判，合成场景类型只有
        # `SCENE_KINDS` 里那四种。
        "scene": {"name": plan.scene.name, "kind": plan.scene.kind,
                  "synthetic": plan.scene.kind in SCENE_KINDS,
                  "note": ("trajgen 内置合成场景（M3a 排程仿真替身）"
                           if plan.scene.kind in SCENE_KINDS
                           else "真实场景导出的占据图（引擎侧 SceneOccupancyExporter / "
                                "carla_scene_export）")},
        "aspect": plan.aspect,
        "focals_mm": [t.focal_mm for t in plan.tracks],
        "cam4": plan.cam4_plan.to_dict(),
        "self_check": {
            "physics": {str(k): v.to_dict() for k, v in plan.physics.items()},
            "decouple": plan.decouple.to_dict() if plan.decouple else None,
            # 形态 b 的三条验形实测（与 QC cam4.topdown_* 同口径）；timewarp 为 None
            "cam4_topdown": (plan.cam4_topdown.to_dict()
                             if plan.cam4_topdown else None),
            "covis_ratio": round(plan.covis.ratio, 4) if plan.covis else None,
            "init_angle_deg": round(plan.init_angle, 3),
            "init_dist_m": round(plan.init_dist, 3),
            "loop_closure_m": round(plan.loop_closure_m, 4),
            "retries": plan.retries,
        },
        "field_conventions": {
            "camera_Follow Offset": "自由相机零向量（docs/70 §4.1 待确认）",
            "player_*": "相机地面投影；速度 = 该投影的逐帧后向差分（采集侧同口径，待确认）",
            "mouse_x/y": "恒 0.5；dx/dy = 朝向增量 / %.1f°（我方口径）" % MOUSE_FULL_SCALE_DEG,
            "keyCode": "程序化段无输入 ⇒ 空数组（docs/71 §8 G-19）",
            "*_rotation_oula": "Unity eulerAngles [x=pitch,y=yaw,z=roll]（顺规待确认，docs/72 C3）",
        },
    }


def write_clip(plan: ClipPlan, out_dir, base_time: str = DEFAULT_T0,
               process_name: str = "DiogenesEngineScenes") -> Path:
    """落盘一条 clip 的 json 面（video/音频由 EngineCapture SDK 在 M3b 补齐）。"""
    out = Path(out_dir)
    out.mkdir(parents=True, exist_ok=True)

    def dump(name: str, obj) -> None:
        with open(out / name, "w", encoding="utf-8") as f:
            json.dump(obj, f, ensure_ascii=False, separators=(",", ":"))

    for idx, tr in enumerate(plan.tracks):
        dump("action_camera_%d.json" % tr.cam_id, frame_records(plan, idx, base_time))
        dump("systeminfo_%d.json" % tr.cam_id, systeminfo(plan, process_name))
    dump("plan.json", plan.to_plan_dict())
    dump("covis.json", plan.covis.to_dict() if plan.covis else {"covis_ratio": 0.0})
    with open(out / "provenance.json", "w", encoding="utf-8") as f:
        json.dump(provenance(plan), f, ensure_ascii=False, indent=2)
    return out

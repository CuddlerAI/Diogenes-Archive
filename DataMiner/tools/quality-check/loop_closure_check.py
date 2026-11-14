#!/usr/bin/env python3
"""Loop-有位移 闭环自检：首尾帧 pose 差是否满足「location、rotation 尽量非常相近」。

客户脚本 quality_check_depth_dataset.py 只查格式，不查闭环——这是 Loop-有位移
类目的标的物，必须单独把关。阈值沿用 trajgen 生成侧机检闸（engine-data/trajgen/
trajgen/sfm.py::LOOP_POS_TOL_M / LOOP_ROT_TOL_DEG，docs/121 §4 L2）：
位置差 ≤ 0.60 m，朝向差 ≤ 8.0°。

只依赖标准库。用法：
    python3 loop_closure_check.py <数据目录或 action_camera_30fps.json>
退出码：0=通过，1=不通过，2=输入错误。
"""

from __future__ import annotations

import json
import math
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import qcthresholds as QT

POS_TOL_M = QT.val("loop_pos_tol_m")
ROT_TOL_DEG = QT.val("loop_rot_tol_deg")


def quat_angle_deg(q1: list[float], q2: list[float]) -> float:
    """两单位四元数的最短夹角（度）。atan2 形式，小角度不病态（acos(dot) 会）。"""
    n1 = math.sqrt(sum(a * a for a in q1))
    n2 = math.sqrt(sum(a * a for a in q2))
    if n1 == 0 or n2 == 0:
        raise ValueError("零四元数")
    a = [x / n1 for x in q1]
    b = [x / n2 for x in q2]
    dot = sum(x * y for x, y in zip(a, b))
    s = 1.0 if dot >= 0 else -1.0
    d_minus = math.sqrt(sum((x - s * y) ** 2 for x, y in zip(a, b)))
    d_plus = math.sqrt(sum((x + s * y) ** 2 for x, y in zip(a, b)))
    return math.degrees(4.0 * math.atan2(d_minus, d_plus))


def main() -> int:
    if len(sys.argv) != 2:
        print(__doc__, file=sys.stderr)
        return 2
    target = Path(sys.argv[1]).expanduser().resolve()
    json_path = target / "action_camera_30fps.json" if target.is_dir() else target
    if not json_path.exists():
        print(f"找不到 {json_path}", file=sys.stderr)
        return 2

    records = json.loads(json_path.read_text(encoding="utf-8"))
    if not isinstance(records, list) or len(records) < 2:
        print("JSON 不是含 ≥2 帧的数组", file=sys.stderr)
        return 2

    first, last = records[0], records[-1]
    p1, p2 = first["camera_position"], last["camera_position"]
    pos_diff = math.dist(p1, p2)
    rot_diff = quat_angle_deg(
        first["camera_rotation_quaternion"], last["camera_rotation_quaternion"]
    )

    # 「有位移」还要求轨迹真的离开过起点——全程最大离起点距离，防原地摇镜错分。
    max_excursion = max(math.dist(p1, r["camera_position"]) for r in records)

    pos_ok = pos_diff <= POS_TOL_M
    rot_ok = rot_diff <= ROT_TOL_DEG
    moved = max_excursion > 2.0  # 名义上要 A→B→A，起码离开起点 2 m 才算「有位移」

    rows = [
        ("首尾位置差", f"≤ {POS_TOL_M:.2f} m", f"{pos_diff:.3f} m", pos_ok),
        ("首尾朝向差", f"≤ {ROT_TOL_DEG:.1f}°", f"{rot_diff:.3f}°", rot_ok),
        ("有位移（最大离起点距离）", "> 2.0 m", f"{max_excursion:.3f} m", moved),
    ]
    print("| 检查项 | 标准 | 实测 | 是否通过 |")
    print("|---|---|---|---|")
    for item, std, actual, ok in rows:
        print(f"| {item} | {std} | {actual} | {'通过' if ok else '不通过'} |")

    all_ok = pos_ok and rot_ok and moved
    print()
    print("闭环结论：通过。" if all_ok else "闭环结论：不通过。")
    return 0 if all_ok else 1


if __name__ == "__main__":
    raise SystemExit(main())

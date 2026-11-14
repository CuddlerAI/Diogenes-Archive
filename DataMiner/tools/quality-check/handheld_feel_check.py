#!/usr/bin/env python3
"""手持感自检：量化「摄影师手太稳/太猛」（业务方 2026-08-11 video 反馈第 4 条）。

目标运动画像 =「持续微扰 + 无尖峰 + 不站桩」：
  真实手持相机的旋转是连续小幅的（中位 5–25°/s），几乎不存在完全锁定
  朝向的帧段，也不存在瞬间甩头。试产 0012 的反例：旋转中位 1.6°/s、
  45% 帧 <1°/s（死稳），转弯瞬时 432°/s（猛甩），静止帧 36%（站桩）。

阈值为 v1 建议档（介于客户反馈与 engine phys 闸之间），#717 人工采集
档位定形后以彼为准。只依赖标准库。

用法：python3 handheld_feel_check.py <数据目录或 action_camera_30fps.json>
退出码：0=通过，1=不通过，2=输入错误。
"""

from __future__ import annotations

import json
import math
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import qcthresholds as QT

IDLE_POS_MS = QT.val("idle_pos_ms")      # 低于此平移速度且低于 IDLE_ROT_DPS 记为静止帧
IDLE_ROT_DPS = QT.val("idle_rot_dps")
IDLE_FRAC_MAX = QT.val("static_frac_max")    # 客户红线：原地静止 >10% 时长打回
IDLE_RUN_MAX_S = QT.val("idle_run_max_s")    # 单段静止上限（SOP：任何停留 ≤2–3s）
ROT_MEDIAN_LO = QT.val("rot_median_lo_dps")     # 旋转速率中位下限：低于它就是「死稳」
ROT_MEDIAN_HI = QT.val("rot_median_hi_dps")    # 上限：高于它观感晃
ROT_LOCKED_FRAC_MAX = QT.val("rot_locked_frac_max")   # <1°/s（锁定朝向）帧占比上限
ROT_MAX_DPS = QT.val("rot_peak_max_dps")      # 瞬时角速度硬帽（甩头；与 engine 闸同值）


def quat_angle_deg(a: list[float], b: list[float]) -> float:
    na = math.sqrt(sum(x * x for x in a))
    nb = math.sqrt(sum(x * x for x in b))
    a = [x / na for x in a]
    b = [x / nb for x in b]
    dot = sum(x * y for x, y in zip(a, b))
    s = 1.0 if dot >= 0 else -1.0
    dm = math.sqrt(sum((x - s * y) ** 2 for x, y in zip(a, b)))
    dp = math.sqrt(sum((x + s * y) ** 2 for x, y in zip(a, b)))
    return math.degrees(4.0 * math.atan2(dm, dp))


def longest_run(flags: list[bool]) -> int:
    best = cur = 0
    for f in flags:
        cur = cur + 1 if f else 0
        best = max(best, cur)
    return best


def main() -> int:
    if len(sys.argv) != 2:
        print(__doc__, file=sys.stderr)
        return 2
    target = Path(sys.argv[1]).expanduser().resolve()
    path = target / "action_camera_30fps.json" if target.is_dir() else target
    if not path.exists():
        print(f"找不到 {path}", file=sys.stderr)
        return 2
    rows = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(rows, list) or len(rows) < 30:
        print("JSON 不是含 ≥30 帧的数组", file=sys.stderr)
        return 2

    pos = [r["camera_position"] for r in rows]
    quat = [r["camera_rotation_quaternion"] for r in rows]
    fps = 30.0
    speed = [math.dist(pos[i], pos[i + 1]) * fps for i in range(len(pos) - 1)]
    rot = [quat_angle_deg(quat[i], quat[i + 1]) * fps for i in range(len(quat) - 1)]

    idle = [s < IDLE_POS_MS and r < IDLE_ROT_DPS for s, r in zip(speed, rot)]
    idle_frac = sum(idle) / len(idle)
    idle_run_s = longest_run(idle) / fps
    rot_sorted = sorted(rot)
    rot_median = rot_sorted[len(rot_sorted) // 2]
    locked_frac = sum(1 for r in rot if r < 1.0) / len(rot)
    rot_max = max(rot)

    rows_out = [
        ("静止帧占比（客户红线 10%）", f"≤ {IDLE_FRAC_MAX:.0%}", f"{idle_frac:.1%}", idle_frac <= IDLE_FRAC_MAX),
        ("最长连续静止", f"≤ {IDLE_RUN_MAX_S:.0f} s", f"{idle_run_s:.1f} s", idle_run_s <= IDLE_RUN_MAX_S),
        ("旋转速率中位（手持感）", f"{ROT_MEDIAN_LO:.0f}–{ROT_MEDIAN_HI:.0f} °/s", f"{rot_median:.1f} °/s", ROT_MEDIAN_LO <= rot_median <= ROT_MEDIAN_HI),
        ("朝向锁定帧占比（<1°/s）", f"≤ {ROT_LOCKED_FRAC_MAX:.0%}", f"{locked_frac:.1%}", locked_frac <= ROT_LOCKED_FRAC_MAX),
        ("瞬时角速度峰值（甩头）", f"≤ {ROT_MAX_DPS:.0f} °/s", f"{rot_max:.1f} °/s", rot_max <= ROT_MAX_DPS),
    ]
    print("| 检查项 | 标准 | 实测 | 是否通过 |")
    print("|---|---|---|---|")
    for item, std, actual, ok in rows_out:
        print(f"| {item} | {std} | {actual} | {'通过' if ok else '不通过'} |")
    all_ok = all(r[3] for r in rows_out)
    print()
    print("手持感结论：通过。" if all_ok else "手持感结论：不通过。")
    return 0 if all_ok else 1


if __name__ == "__main__":
    raise SystemExit(main())

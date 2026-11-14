"""回归：交付精度（6 位小数）下的角速率链不能被数值条件性打穿。

需求方样例 json 的四元数只有 6 位小数（`0.019822` 这种），我方交付也按同精度出。
`quat_angle_deg` 若用 `2·acos(dot)`，小角处导数发散，会把这点量化噪声放大成
角 jerk 的假尖峰 —— 实测把真值 ~280°/s³ 的合规轨迹抬到 ~2600°/s³，
`phys.angjerk_p95`（≤600）必然误杀。本测试锁住良态实现。
"""

import math

from engine_qc.config import QCConfig
from engine_qc.checks_motion import check_physics
from engine_qc.mathutil import quat_angle_deg
from engine_qc.report import PASS

FPS = 30.0


def _yaw_quat(deg):
    h = math.radians(deg) / 2.0
    return [0.0, math.sin(h), 0.0, math.cos(h)]


def _round6(q):
    return [round(x, 6) for x in q]


def test_small_angle_accuracy_at_delivery_precision():
    """6 位小数量化下，1°/帧 的夹角误差要远小于 1%。"""
    worst = 0.0
    for base in (0.0, 37.0, 123.5, -88.25):
        a = _round6(_yaw_quat(base))
        b = _round6(_yaw_quat(base + 1.0))
        worst = max(worst, abs(quat_angle_deg(a, b) - 1.0))
    assert worst < 0.01, "小角夹角误差 %.4f° 过大（acos 形式的典型症状）" % worst


def test_angjerk_not_inflated_by_quantization():
    """匀速偏航（角 jerk 真值为 0）在交付精度下不该冒出角 jerk。"""
    frames = []
    for i in range(300):
        yaw = 30.0 * (i / FPS)              # 恒定 30°/s
        frames.append({
            "frame": i,
            "camera_position": [round(0.01 * i, 6), 1.6, 0.0],
            "camera_rotation_quaternion": _round6(_yaw_quat(yaw)),
            "camera_intrinsics": {"fx": 1080.0, "fy": 1080.0, "cx": 960.0, "cy": 540.0},
        })
    results = {r.rule: r for r in
               check_physics("t.json", frames, 1.0 / FPS, QCConfig(), is_cam4=False)}
    assert results["phys.angspeed_p95"].status == PASS
    assert results["phys.angaccel_p95"].status == PASS
    assert results["phys.angjerk_p95"].status == PASS, results["phys.angjerk_p95"].detail

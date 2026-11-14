"""四元数 / 有限差分 / 分位数等纯标准库数学工具。

坐标约定：Unity 左手系，y 朝上，四元数 [x, y, z, w]。
yaw/pitch 由 forward 向量导出（yaw=atan2(fx,fz)，pitch=asin(fy)），
只用于 QC 的角速率符号/幅值判定，不承诺与需求方欧拉顺规一致。
"""

import math
from typing import List, Optional, Sequence, Tuple

Vec3 = Tuple[float, float, float]
Quat = Tuple[float, float, float, float]


def vsub(a: Sequence[float], b: Sequence[float]) -> Vec3:
    return (a[0] - b[0], a[1] - b[1], a[2] - b[2])


def vnorm(a: Sequence[float]) -> float:
    return math.sqrt(a[0] * a[0] + a[1] * a[1] + a[2] * a[2])


def vdot(a: Sequence[float], b: Sequence[float]) -> float:
    return a[0] * b[0] + a[1] * b[1] + a[2] * b[2]


def quat_norm(q: Sequence[float]) -> float:
    return math.sqrt(sum(x * x for x in q))


def quat_angle_deg(q1: Sequence[float], q2: Sequence[float]) -> float:
    """两四元数间的旋转角（度）。输入应近似单位四元数。

    **必须用 atan2 形式，不能用 `2·acos(dot)`**：相邻帧夹角很小时 `dot→1`，
    `d(acos)/d(dot) = -1/√(1−dot²)` 发散，把 json 里的量化噪声放大成角速度噪声。
    交付 json 的四元数按需求方样例只有 6 位小数（`0.019822` 这种），
    30fps 下 acos 形式会把角 jerk p95 从真值 ~280°/s³ 抬到 ~2600°/s³ ——
    **任何**按需求方精度出的合规数据都会被 `phys.angjerk_p95` 误杀（实测复现）。
    atan2 形式在小角处良态，同一份数据两式差 9 倍。
    """
    ax, ay, az, aw = q1
    bx, by, bz, bw = q2
    if ax * bx + ay * by + az * bz + aw * bw < 0.0:   # 取近端半球
        bx, by, bz, bw = -bx, -by, -bz, -bw
    # q_rel = q1⁻¹ · q2（q1 近似单位四元数，共轭即逆）
    rw = aw * bw + ax * bx + ay * by + az * bz
    rx = aw * bx - ax * bw - ay * bz + az * by
    ry = aw * by + ax * bz - ay * bw - az * bx
    rz = aw * bz - ax * by + ay * bx - az * bw
    return math.degrees(2.0 * math.atan2(math.sqrt(rx * rx + ry * ry + rz * rz), abs(rw)))


def quat_forward(q: Sequence[float]) -> Vec3:
    """q 旋转 (0,0,1) 得到 forward 向量。q=[x,y,z,w]。"""
    x, y, z, w = q
    return (
        2.0 * (x * z + w * y),
        2.0 * (y * z - w * x),
        1.0 - 2.0 * (x * x + y * y),
    )


def roll_deg(q: Sequence[float]) -> float:
    """带符号 roll（bank）角（度）：asin(2(x·y + w·z))，q=[x,y,z,w]。

    纯绕光轴旋转 q=(0,0,sin(θ/2),cos(θ/2)) 时精确还原 θ，符号与 docs/69 §3.2
    的「Roll >0 = 向右歪头」一致。asin 在 ±90° 饱和——闸门阈值在 20°，量程足够；
    与 yaw/pitch 一样只用于 QC 幅值判定，不承诺与需求方欧拉顺规逐字一致。
    """
    x, y, z, w = q
    s = 2.0 * (x * y + w * z)
    return math.degrees(math.asin(max(-1.0, min(1.0, s))))


def yaw_pitch_deg(q: Sequence[float]) -> Tuple[float, float]:
    fx, fy, fz = quat_forward(q)
    yaw = math.degrees(math.atan2(fx, fz))
    pitch = math.degrees(math.asin(max(-1.0, min(1.0, fy))))
    return yaw, pitch


def angle_between_deg(a: Sequence[float], b: Sequence[float]) -> float:
    na, nb = vnorm(a), vnorm(b)
    if na == 0.0 or nb == 0.0:
        return 0.0
    c = max(-1.0, min(1.0, vdot(a, b) / (na * nb)))
    return math.degrees(math.acos(c))


def unwrap_deg(series: Sequence[float]) -> List[float]:
    """角度序列去 360° 跳变。"""
    if not series:
        return []
    out = [series[0]]
    for x in series[1:]:
        d = (x - out[-1] + 180.0) % 360.0 - 180.0
        out.append(out[-1] + d)
    return out


def percentile(values: Sequence[float], q: float) -> float:
    """线性插值分位数，q ∈ [0,100]。values 非空。"""
    s = sorted(values)
    if len(s) == 1:
        return s[0]
    k = (len(s) - 1) * q / 100.0
    lo = math.floor(k)
    hi = math.ceil(k)
    if lo == hi:
        return s[int(k)]
    return s[lo] + (s[hi] - s[lo]) * (k - lo)


def diff_series(values: Sequence, dt: float):
    """相邻差分 / dt。向量输入返回向量列表，标量输入返回标量列表。"""
    out = []
    for a, b in zip(values, values[1:]):
        if isinstance(a, (tuple, list)):
            out.append(tuple((y - x) / dt for x, y in zip(a, b)))
        else:
            out.append((b - a) / dt)
    return out


def median(values: Sequence[float]) -> Optional[float]:
    if not values:
        return None
    s = sorted(values)
    n = len(s)
    mid = n // 2
    if n % 2:
        return s[mid]
    return 0.5 * (s[mid - 1] + s[mid])

"""向量 / 四元数 / 样条基 / 平滑过渡等纯标准库数学工具。

坐标约定与 engine-data-qc 一致：Unity 左手系、y 朝上、四元数 `[x, y, z, w]`。
yaw/pitch/roll 采用 Unity 的 ZXY 内旋顺规（`Quaternion.Euler(pitch, yaw, roll)`），
与 QC 侧 `yaw=atan2(fx,fz) / pitch=asin(fy)` 的 forward 导出口径自洽。

**注意**：交付 json 里的 `camera_rotation_oula` 顺规尚未由需求方定义
（docs/71 §11 待确认 17），本模块导出的欧拉角以 Unity `eulerAngles` 口径为准并记 provenance。
"""

import math
from typing import List, Optional, Sequence, Tuple

Vec3 = Tuple[float, float, float]
Quat = Tuple[float, float, float, float]


# ---------------------------------------------------------------- 向量

def vadd(a: Sequence[float], b: Sequence[float]) -> Vec3:
    return (a[0] + b[0], a[1] + b[1], a[2] + b[2])


def vsub(a: Sequence[float], b: Sequence[float]) -> Vec3:
    return (a[0] - b[0], a[1] - b[1], a[2] - b[2])


def vscale(a: Sequence[float], k: float) -> Vec3:
    return (a[0] * k, a[1] * k, a[2] * k)


def vdot(a: Sequence[float], b: Sequence[float]) -> float:
    return a[0] * b[0] + a[1] * b[1] + a[2] * b[2]


def vcross(a: Sequence[float], b: Sequence[float]) -> Vec3:
    return (a[1] * b[2] - a[2] * b[1],
            a[2] * b[0] - a[0] * b[2],
            a[0] * b[1] - a[1] * b[0])


def vnorm(a: Sequence[float]) -> float:
    return math.sqrt(a[0] * a[0] + a[1] * a[1] + a[2] * a[2])


def vunit(a: Sequence[float]) -> Vec3:
    n = vnorm(a)
    if n < 1e-12:
        return (0.0, 0.0, 1.0)
    return (a[0] / n, a[1] / n, a[2] / n)


def vlerp(a: Sequence[float], b: Sequence[float], t: float) -> Vec3:
    return (a[0] + (b[0] - a[0]) * t,
            a[1] + (b[1] - a[1]) * t,
            a[2] + (b[2] - a[2]) * t)


def vdist(a: Sequence[float], b: Sequence[float]) -> float:
    return vnorm(vsub(a, b))


# ---------------------------------------------------------------- 角度

def clamp(x: float, lo: float, hi: float) -> float:
    return lo if x < lo else (hi if x > hi else x)


def wrap180(deg: float) -> float:
    """把角度折进 (-180, 180]。"""
    return (deg + 180.0) % 360.0 - 180.0


def unwrap_deg(series: Sequence[float]) -> List[float]:
    """角度序列去 360° 跳变（同 qc.mathutil.unwrap_deg）。"""
    if not series:
        return []
    out = [series[0]]
    for x in series[1:]:
        out.append(out[-1] + wrap180(x - out[-1]))
    return out


# ---------------------------------------------------------------- 四元数（[x,y,z,w]）

def quat_from_euler(pitch: float, yaw: float, roll: float) -> Quat:
    """Unity `Quaternion.Euler(pitch, yaw, roll)` 等价实现（ZXY 内旋，度）。"""
    cx, sx = math.cos(math.radians(pitch) / 2), math.sin(math.radians(pitch) / 2)
    cy, sy = math.cos(math.radians(yaw) / 2), math.sin(math.radians(yaw) / 2)
    cz, sz = math.cos(math.radians(roll) / 2), math.sin(math.radians(roll) / 2)
    # q = qy * qx * qz（Unity 的 ZXY 应用顺序）
    x = cy * sx * cz + sy * cx * sz
    y = sy * cx * cz - cy * sx * sz
    z = cy * cx * sz - sy * sx * cz
    w = cy * cx * cz + sy * sx * sz
    n = math.sqrt(x * x + y * y + z * z + w * w)
    return (x / n, y / n, z / n, w / n)


def quat_forward(q: Sequence[float]) -> Vec3:
    """q 旋转 (0,0,1)。与 qc.mathutil.quat_forward 同式。"""
    x, y, z, w = q
    return (2.0 * (x * z + w * y),
            2.0 * (y * z - w * x),
            1.0 - 2.0 * (x * x + y * y))


def quat_right(q: Sequence[float]) -> Vec3:
    """q 旋转 (1,0,0)。"""
    x, y, z, w = q
    return (1.0 - 2.0 * (y * y + z * z),
            2.0 * (x * y + w * z),
            2.0 * (x * z - w * y))


def quat_up(q: Sequence[float]) -> Vec3:
    """q 旋转 (0,1,0)。"""
    x, y, z, w = q
    return (2.0 * (x * y - w * z),
            1.0 - 2.0 * (x * x + z * z),
            2.0 * (y * z + w * x))


def quat_angle_deg(q1: Sequence[float], q2: Sequence[float]) -> float:
    """两四元数间的旋转角（度）——与 `engine_qc.mathutil.quat_angle_deg` 逐字同式。

    README「运动学反算规范口径」：**必须用 atan2 形式**，acos(dot) 在小角度处
    `d(acos)/d(dot)` 发散，把量化噪声放大成角速度噪声（QC 侧 PR #428 已改，
    生成侧自检若留 acos 形式，两侧算出的数就不一致）。
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


def angle_between_deg(a: Sequence[float], b: Sequence[float]) -> float:
    """两向量夹角（度）——与 `engine_qc.mathutil.angle_between_deg` 逐字同式。"""
    na, nb = vnorm(a), vnorm(b)
    if na == 0.0 or nb == 0.0:
        return 0.0
    c = clamp(vdot(a, b) / (na * nb), -1.0, 1.0)
    return math.degrees(math.acos(c))


def yaw_elev_from_forward(fwd: Sequence[float]) -> Tuple[float, float]:
    """forward 向量 → (yaw, elev)，与 qc 的 `yaw_pitch_deg` 同式。

    **elev 是仰角**（向上为正），Unity 的 euler.x（pitch）向下为正，二者互为相反数——
    本包内部一律用 (yaw, elev, roll) 表示朝向，只在生成四元数/欧拉角时才转 Unity pitch。
    """
    f = vunit(fwd)
    yaw = math.degrees(math.atan2(f[0], f[2]))
    elev = math.degrees(math.asin(clamp(f[1], -1.0, 1.0)))
    return yaw, elev


def quat_from_yaw_elev_roll(yaw: float, elev: float, roll: float) -> Quat:
    """本包朝向表示 (yaw, elev, roll) → Unity 四元数。"""
    return quat_from_euler(-elev, yaw, roll)


def euler_tuple(yaw: float, elev: float, roll: float) -> Vec3:
    """本包朝向表示 → Unity `eulerAngles` 口径 [x=pitch, y=yaw, z=roll]，折进 [0,360)。"""
    return ((-elev) % 360.0, yaw % 360.0, roll % 360.0)


# ---------------------------------------------------------------- 平滑过渡

def smoothstep5(tau: float) -> float:
    """五次 smoothstep `S₅(τ)=6τ⁵−15τ⁴+10τ³`，两端一二阶导全零（docs/71 §6）。"""
    t = clamp(tau, 0.0, 1.0)
    return t * t * t * (t * (t * 6.0 - 15.0) + 10.0)


def s5_blend(a: float, b: float, t: float, t0: float, width: float) -> float:
    """在 [t0, t0+width] 上用 S₅ 从 a 过渡到 b；区间外取端点值（C² 连续）。"""
    if width <= 0.0:
        return b if t >= t0 else a
    return a + (b - a) * smoothstep5((t - t0) / width)


# ---------------------------------------------------------------- 均匀五次 B 样条

#: 均匀五次 B 样条基函数在局部参数 u∈[0,1) 上的 6 个权重（阶乘归一 1/120）
def bspline5_basis(u: float) -> Tuple[float, ...]:
    u2 = u * u
    u3 = u2 * u
    u4 = u3 * u
    u5 = u4 * u
    b0 = (1 - 5 * u + 10 * u2 - 10 * u3 + 5 * u4 - u5) / 120.0
    b1 = (26 - 50 * u + 20 * u2 + 20 * u3 - 20 * u4 + 5 * u5) / 120.0
    b2 = (66 - 60 * u2 + 30 * u4 - 10 * u5) / 120.0
    b3 = (26 + 50 * u + 20 * u2 - 20 * u3 - 20 * u4 + 10 * u5) / 120.0
    b4 = (1 + 5 * u + 10 * u2 + 10 * u3 + 5 * u4 - 5 * u5) / 120.0
    b5 = u5 / 120.0
    return (b0, b1, b2, b3, b4, b5)


def bspline5_eval(ctrl: Sequence[float], s: float) -> float:
    """标量均匀五次 B 样条求值。

    `ctrl` 为控制值序列，`s ∈ [0, len(ctrl)-5]` 为样条参数（每加 1 跨一个控制点）。
    端点用重复控制点做 clamped 处理（由调用方 `clamp_ctrl` 预处理）。
    结果 C⁴ 连续 —— 位置/朝向的三阶导（jerk / 角 jerk）天然连续，
    这是 docs/71 §4「段间 C² 缓入缓出」的强化实现。
    """
    n = len(ctrl)
    span = max(1, n - 5)
    s = clamp(s, 0.0, float(span))
    i = int(math.floor(s))
    if i >= span:
        i = span - 1
    u = s - i
    w = bspline5_basis(u)
    return sum(ctrl[i + k] * w[k] for k in range(6))


def clamp_ctrl(values: Sequence[float]) -> List[float]:
    """首尾各复制 5 个控制点 —— 五次样条要重复「次数」个端点才**严格插值**端点值。

    重复 2 个只能贴近（端点会被 v1/v2 拉走约 22%），首帧位姿因此对不上起始位姿，
    进而把 QC 的 `init.orientation` 打成 FAIL。
    """
    if not values:
        return []
    return [values[0]] * 5 + list(values) + [values[-1]] * 5


def linear_ctrl(values: Sequence[float]) -> List[float]:
    """端点按**线性外推**补 5 个控制点（均匀 B 样条对线性函数是精确重现的）。

    用于插值**单调函数**（如弧长→时刻的反查）：`clamp_ctrl` 会把端点斜率压成 0，
    对反查函数就意味着「起点附近怎么走弧长、时刻都不动」——第四路开头会原地不动，
    QC 反算的 ρ 于是从 0 爬起来，被判成多一次变速（实测在 t≈2–7s 稳定复现）。
    """
    if len(values) < 2:
        return clamp_ctrl(values)
    d0 = values[1] - values[0]
    dn = values[-1] - values[-2]
    head = [values[0] - d0 * k for k in range(5, 0, -1)]
    tail = [values[-1] + dn * k for k in range(1, 6)]
    return head + list(values) + tail


#: `linear_ctrl` 前置的外推控制点个数带来的参数偏移。
#: 均匀五次 B 样条对线性数据有 `curve(i) = ctrl[i+2]`，而 linear_ctrl 在头部塞了 5 个，
#: 所以 `values[j]` 落在参数 `3+j` 上 —— 按 clamp 的「0 → values[0]」去映射会整体错位
#: （ArcPath 里就是弧长起点偏 3 个采样间隔）。
LINEAR_CTRL_OFFSET = 3.0


def linear_eval(ctrl: Sequence[float], n_values: int, u01: float) -> float:
    """在 `linear_ctrl(values)` 上按归一化位置 u01∈[0,1] 求值。"""
    s = LINEAR_CTRL_OFFSET + clamp(u01, 0.0, 1.0) * max(0, n_values - 1)
    return bspline5_eval(ctrl, s)


def bspline5_curve(values: Sequence[float], n_out: int) -> List[float]:
    """把控制值序列展开成 `n_out` 个等参采样（含两端）。"""
    ctrl = clamp_ctrl(values)
    span = max(1, len(ctrl) - 5)
    if n_out <= 1:
        return [bspline5_eval(ctrl, 0.0)]
    return [bspline5_eval(ctrl, span * i / (n_out - 1)) for i in range(n_out)]


# ---------------------------------------------------------------- 统计

def percentile(values: Sequence[float], q: float) -> float:
    """线性插值分位数（与 qc.mathutil.percentile 同实现，保证两侧口径一致）。"""
    s = sorted(values)
    if not s:
        return 0.0
    if len(s) == 1:
        return s[0]
    k = (len(s) - 1) * q / 100.0
    lo, hi = math.floor(k), math.ceil(k)
    if lo == hi:
        return s[int(k)]
    return s[lo] + (s[hi] - s[lo]) * (k - lo)


def median(values: Sequence[float]) -> Optional[float]:
    if not values:
        return None
    s = sorted(values)
    n = len(s)
    return s[n // 2] if n % 2 else 0.5 * (s[n // 2 - 1] + s[n // 2])


def mean(values: Sequence[float]) -> float:
    return sum(values) / len(values) if values else 0.0

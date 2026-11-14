"""轨迹平滑骨架：节点采样 → clamped 五次 B 样条 → 逐帧序列。

docs/71 §4「位置/朝向各拟合 clamped 五次 B 样条 → 套 jerk-limited 速度剖面 →
30fps 采样反算各阶导数验 p95 表」的落地方式：

- **每个运镜 move 解析地给出「局部归一化时间 → 位姿」**（`primitives.py`），
  在 `KNOT_S` 的节点网格上采样；
- 整条 60s 的节点序列（位置 xyz、朝向 yaw/elev/roll）过**一次**五次 B 样条重采样到 30fps。
  样条是 C⁴ 的，段界的 C² 缓入缓出因此是构造性的，不需要在每个段界解七段方程；
- 速度剖面用**分段目标速度 + S₅ 过渡**（与 docs/71 §6 第四路 ρ(t) 同一个器件）给出，
  move 的几何跨度按 `L = v_target × 时长` 定，段内进度按全局速度剖面的归一化积分推进 ——
  于是「段均速 = 目标速度」是构造性的，段界速度连续由 S₅ 保证。

jerk 有界性最终仍由 `metrics.py` 在 30fps 逐帧反算复核（p95 超限即降速重排），
生成侧保证 + QC 侧复核两道，与 docs/70 §4.2 一致。
"""

import math
from typing import Callable, List, Sequence, Tuple

from .mathutil import Vec3, bspline5_curve, clamp, smoothstep5, unwrap_deg

#: 节点网格间隔（s）。0.3s = 9 帧；五次 B 样条把单节点扰动摊到 ~6 个节点（1.8s）上。
#: 这个值直接定 jerk 量级：30fps 三阶差分下 `j ~ Δ³p/h³`，0.2→0.3 就是 3.4× 的余量，
#: 是把 docs/69 表 2 的 20m/s³ / 600°/s³ 两条稳住的主要杠杆。
KNOT_S = 0.3

#: 速度分段之间的 S₅ 过渡宽度（s）。docs/71 §6 同族参数，取值范围与第四路一致
SPEED_BLEND_S = 2.0


class SpeedProfile:
    """分段目标速度 + S₅ 过渡的时间函数 v(t)（C²）。"""

    def __init__(self, marks: Sequence[Tuple[float, float]], blend_s: float = SPEED_BLEND_S):
        """`marks` = [(段起始时刻 s, 目标速度 m/s)]，按时刻升序，首项时刻应为 0。"""
        self.marks = sorted(marks)
        self.blend = max(1e-3, blend_s)

    def __call__(self, t: float) -> float:
        v = self.marks[0][1]
        for (_, v_prev), (t1, v_next) in zip(self.marks, self.marks[1:]):
            if t >= t1 + self.blend:
                v = v_next
            elif t > t1:
                return v + (v_next - v) * smoothstep5((t - t1) / self.blend)
            else:
                return v
        return v

    def cumulative(self, times: Sequence[float]) -> List[float]:
        """在给定时刻网格上累积 `∫v dt`（梯形），返回与 times 等长的行程序列。"""
        out = [0.0]
        for a, b in zip(times, times[1:]):
            out.append(out[-1] + 0.5 * (self(a) + self(b)) * (b - a))
        return out

    def mean(self, t0: float, t1: float, samples: int = 32) -> float:
        if t1 - t0 <= 1e-9:
            return self(t0)
        return sum(self(t0 + (t1 - t0) * i / samples) for i in range(samples + 1)) / (samples + 1)


def progress_from_cumulative(cum: Sequence[float], i0: int, i1: int, i: int) -> float:
    """由累积行程给出 move 内归一化进度（构造性保证段均速 = 目标速度）。"""
    span = cum[i1] - cum[i0]
    if span <= 1e-9:
        return clamp((i - i0) / max(1, i1 - i0), 0.0, 1.0)
    return clamp((cum[i] - cum[i0]) / span, 0.0, 1.0)


def knot_times(duration: float, knot_s: float = KNOT_S) -> List[float]:
    n = max(2, int(round(duration / knot_s)) + 1)
    return [duration * i / (n - 1) for i in range(n)]


def smooth_positions(knots: Sequence[Vec3], n_frames: int) -> List[Vec3]:
    """位置节点 → 五次 B 样条 → 逐帧位置。"""
    xs = bspline5_curve([p[0] for p in knots], n_frames)
    ys = bspline5_curve([p[1] for p in knots], n_frames)
    zs = bspline5_curve([p[2] for p in knots], n_frames)
    return list(zip(xs, ys, zs))


def smooth_angles(knots: Sequence[float], n_frames: int) -> List[float]:
    """角度节点（度）→ unwrap → 五次 B 样条 → 逐帧角度。"""
    return bspline5_curve(unwrap_deg(list(knots)), n_frames)


def scale_speed_marks(marks: Sequence[Tuple[float, float]], k: float) -> List[Tuple[float, float]]:
    """整体降/升速（`planner` 在 p95 超限时用它重整形）。"""
    return [(t, v * k) for t, v in marks]

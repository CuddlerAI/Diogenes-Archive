"""生成侧与 QC 侧的 `quat_angle_deg` 必须算出同一个数。

README「运动学反算规范口径」把这条定成硬约束：「QC 与 trajgen 自检必须逐字一致…
**必须用 atan2 形式**」。两侧各有一份实现，靠人盯着不会一直对——这个测试是那条
约定的可执行版本。

分歧的后果不是「数字略有出入」：生成侧自检放行的片子会在 QC 侧被打回，而两边
报的是同一个指标名，查起来无从下手。联测里角 jerk 484 vs ~2600 °/s³ 就是这么来的。
"""

import math
import os
import random
import sys
import unittest

_HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(_HERE, "..", "..", "qc"))
sys.path.insert(0, os.path.join(_HERE, ".."))

from engine_qc.mathutil import quat_angle_deg as qc_quat_angle_deg  # noqa: E402
from trajgen.mathutil import quat_angle_deg as tg_quat_angle_deg  # noqa: E402


def _qmul(a, b):
    ax, ay, az, aw = a
    bx, by, bz, bw = b
    return [aw * bx + ax * bw + ay * bz - az * by,
            aw * by - ax * bz + ay * bw + az * bx,
            aw * bz + ax * by - ay * bx + az * bw,
            aw * bw - ax * bx - ay * by - az * bz]


def _axis_angle(axis, ang):
    n = math.sqrt(sum(c * c for c in axis))
    s = math.sin(ang / 2.0)
    return [axis[0] / n * s, axis[1] / n * s, axis[2] / n * s, math.cos(ang / 2.0)]


class QuatAngleParityTests(unittest.TestCase):

    def test_two_implementations_agree_bitwise(self):
        """含极小角在内，两侧逐位相同。

        小角是关键区间：30fps 下相邻帧夹角本来就小，而 acos(dot) 恰在那里病态。
        """
        rng = random.Random(11)
        worst = 0.0
        for _ in range(20000):
            q1 = [rng.gauss(0, 1) for _ in range(4)]
            n = math.sqrt(sum(x * x for x in q1))
            q1 = [x / n for x in q1]
            ang = rng.choice([rng.uniform(0, math.pi),
                              rng.uniform(0, 1e-4),
                              rng.uniform(0, 1e-7)])
            dq = _axis_angle([rng.gauss(0, 1) for _ in range(3)], ang)
            q2 = _qmul(q1, dq)
            worst = max(worst, abs(qc_quat_angle_deg(q1, q2) - tg_quat_angle_deg(q1, q2)))
        self.assertEqual(worst, 0.0, f"两侧最大偏差 {worst:.3e} 度，应为 0")

    def test_small_angle_survives_delivery_precision(self):
        """交付 json 只有 6 位小数；acos 形式在这个精度下会把 0.1° 读成 0。

        dot 舍入到 1.0 后被 `min(1.0, ...)` 夹住，acos(1)=0——角速度整段归零，
        再经两次差分喂给 jerk，虚高与归零同时发生。atan2 形式没有这个悬崖。
        """
        q1 = [round(x, 6) for x in _axis_angle((0.3, 0.9, 0.31), 0.0)]
        q2 = [round(x, 6) for x in _axis_angle((0.3, 0.9, 0.31), math.radians(0.1))]
        self.assertAlmostEqual(tg_quat_angle_deg(q1, q2), 0.1, places=3)
        legacy = math.degrees(2.0 * math.acos(
            min(1.0, abs(sum(a * b for a, b in zip(q1, q2))))))
        self.assertEqual(legacy, 0.0, "前提失效：旧式在这个样例上不再归零")

    def test_identity_and_antipodal_are_zero(self):
        q = _axis_angle((1.0, 2.0, 3.0), 1.234)
        self.assertAlmostEqual(tg_quat_angle_deg(q, q), 0.0, places=9)
        self.assertAlmostEqual(tg_quat_angle_deg(q, [-x for x in q]), 0.0, places=9)


if __name__ == "__main__":  # pragma: no cover
    unittest.main()

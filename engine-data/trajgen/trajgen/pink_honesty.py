"""粉色平移诚实性修正场（经典管线前三路；QC plan.pink_honesty 的生成侧保证）。

QC #597 的契约：每个声称的 translate_* 窗，实测净轴向位移 ≥0.8m 且 ≥ 窗内路径长
50%（基底 = 窗首帧相机 yaw）。经典管线的平移经过样条逼近、自由壳投影、共视窗口
收敛三道位置改写后契约不保（实测 144 窗只有 70 诚实）。原语侧的基底对齐与 extent
抬底把偏差压小之后，本模块在**全部位置改写完成之后**做最后一道构造性修正——与
`_force_covis_window` 同范式（后置修正场，S₅ 平滑，不碰朝向）：

- **轴向增益**：窗内沿声称轴叠加 `g·S₅(u)`，g 解自「净轴向 ≥1.0 且 ≥58% 路径」
  （对 QC 门 0.8/50% 各留余量）；
- **横向抑制**：窗内把垂直于声称轴的漂移分量按 S₅ 钟形窗压掉 85%——两端权重为零，
  端点不动 ⇒ 不产生额外偏移，只削路径污染；
- **窗后衰减**：轴向增益造成的位置偏移在下一个粉色窗开始前 S₅ 衰减回零；
  相邻窗之间没有衰减空间时**原样携带**（常量偏移不改变任何窗的位移/路径）；
- **俯冲预抬升**：translate_down 会撞视高下限（相机在视高带内、地板 ground+0.6，
  俯冲 1m 的余量经常不存在），在窗前的间隙里先把相机抬到够冲的高度。

朝向刻意不动：声称轴向在修正前从最终 quat 读出，改位置不改朝向 ⇒ 轴向自洽；
代价是修正处 look-at 视线有小角度偏差（|修正|/环带半径 量级），共视闸门在
生成侧自检里兜底复核。
"""

import math
from typing import List, Sequence, Tuple

from . import metrics as met
from .mathutil import Quat, Vec3, clamp, smoothstep5, vadd, vdot, vnorm, vscale, vsub
from .primitives import PINK

#: 修正目标（对 QC 门 0.8 / 50% 的余量档）
DISP_TARGET_M = 1.0
FRAC_TARGET = 0.58
#: 横向抑制强度与钟形窗肩宽（窗时长比例）
TRANS_SUPPRESS = 0.85
BUMP_SHOULDER = 0.22
#: 修正场自身的 jerk 预算（m/s³）。S₅ 是最小 jerk 剖面，位移 d、时长 T 的坡
#: 峰值 jerk = 60·d/T³——增益、衰减、预抬升、横向抑制统一按这条预算限幅：
#: 增益帽 g ≤ B·T³/60、衰减坡时长 ≥ ∛(60·|off|/B)、抑制场按数值 jerk 缩幅。
#: 预算取 jerk 门 20 的四成：修正叠在本就有 jerk 的轨迹上，p95 得给底轨迹
#: 留大头（12.0 时实测 cam1 p95 冲到 24）。
JERK_BUDGET = 8.0
#: 窗后衰减：衰减时长帽与留白（s）。时长下限由 JERK_BUDGET 反解，
#: 间隙不够就把偏移**携带**到下一个间隙（常量偏移不影响任何窗的判据）。
DECAY_MAX_S = 3.5
DECAY_PAD_S = 0.3
#: 俯冲预抬升的最短间隙与坡长帽（s）；视高地板与原语 `_clamp_eye` 同源
PRELIFT_MIN_GAP_S = 0.6
PRELIFT_MAX_S = 2.5
EYE_FLOOR_M = 0.6


def _jerk_safe_gain(dur_s: float) -> float:
    """时长 dur 的 S₅ 坡在 jerk 预算内允许的最大位移（m）。"""
    return JERK_BUDGET * dur_s ** 3 / 60.0


def _jerk_safe_span(disp_m: float) -> float:
    """位移 disp 的 S₅ 坡要多长才落在 jerk 预算内（s）。"""
    return (60.0 * disp_m / JERK_BUDGET) ** (1.0 / 3.0)


def _s5_ramp(corr: List[Vec3], i0: int, i1: int, delta: Vec3) -> None:
    """corr[i0..i1] 上从 0 → delta 的 S₅ 坡，i1 之后恒 delta。就地累加。"""
    n = len(corr)
    span = max(1, i1 - i0)
    for i in range(max(0, i0), n):
        w = smoothstep5((i - i0) / span) if i < i1 else 1.0
        corr[i] = vadd(corr[i], vscale(delta, w))


def enforce(positions: List[Vec3], quats: Sequence[Quat], moves,
            fps: float, ground_y: float) -> List[Vec3]:
    """返回修正后的位置序列（不改 quats；调用方负责刷新 speeds）。"""
    n = len(positions)
    dt = 1.0 / fps
    corr: List[Vec3] = [(0.0, 0.0, 0.0)] * n
    cur_off: Vec3 = (0.0, 0.0, 0.0)

    pinks = sorted((m for m in moves if m.cls in PINK), key=lambda m: m.t0)
    prev_end = 0
    for k, m in enumerate(pinks):
        i0 = max(0, int(round(m.t0 * fps)))
        i1 = min(int(round(m.t1 * fps)), n - 1)
        if i1 - i0 < 2:
            prev_end = max(prev_end, i1)
            continue
        axis, _ = met.pink_axis(m.cls, quats[i0])
        win_s = (i1 - i0) * dt
        g_cap = _jerk_safe_gain(win_s)

        # 当前有效位置 = 原始 + 已累计常量偏移（此前的坡在 i0 前都已走完，
        # 常量偏移不改变窗内位移/路径，直接用原始序列算即可）
        seg = positions[i0:i1 + 1]
        d_net = vsub(seg[-1], seg[0])
        axial = vdot(d_net, axis)
        path_raw = sum(vnorm(vsub(b, a)) for a, b in zip(seg, seg[1:]))

        # --- 先试「只加轴向增益」：多数窗（原语已对齐基底）只差一点位移，
        #     不动横向就不引入抑制场自己的 jerk ---
        g = max(0.0, DISP_TARGET_M - axial,
                (FRAC_TARGET * path_raw - axial) / (1.0 - FRAC_TARGET))
        supp: List[Vec3] = [(0.0, 0.0, 0.0)] * len(seg)
        if g > g_cap:
            # --- 横向抑制（端点权重为零 ⇒ 无偏移残留）压路径污染，再解一次 g ---
            for j, p in enumerate(seg):
                u = j / (len(seg) - 1)
                bump = min(1.0, smoothstep5(u / BUMP_SHOULDER),
                           smoothstep5((1.0 - u) / BUMP_SHOULDER))
                rel = vsub(p, seg[0])
                trans = vsub(rel, vscale(axis, vdot(rel, axis)))
                supp[j] = vscale(trans, -TRANS_SUPPRESS * bump)
            # 抑制场是「跟着横向漂移形状走」的修正，jerk 没有解析上界——
            # 数值三阶差分实测，超预算按比例缩幅（缩幅 ⇒ 抑制变弱 ⇒ g 变大，
            # 由下面的帽兜底给自检打回）。
            jmax = 0.0
            for j in range(3, len(supp)):
                jv = tuple(supp[j][a] - 3 * supp[j - 1][a] + 3 * supp[j - 2][a]
                           - supp[j - 3][a] for a in range(3))
                jmax = max(jmax, vnorm(jv) / dt ** 3)
            if jmax > JERK_BUDGET:
                supp = [vscale(s, JERK_BUDGET / jmax) for s in supp]
            seg_s = [vadd(p, s) for p, s in zip(seg, supp)]
            path_s = sum(vnorm(vsub(b, a)) for a, b in zip(seg_s, seg_s[1:]))
            g = max(0.0, DISP_TARGET_M - axial,
                    (FRAC_TARGET * path_s - axial) / (1.0 - FRAC_TARGET))
        if g > g_cap:
            # jerk 预算内救不回：声称与运动严重相反（原语对齐后应极罕见）。
            # 不硬掰，撤掉抑制、交给自检打回重排。
            g = 0.0
            supp = [(0.0, 0.0, 0.0)] * len(seg)

        # --- 俯冲撞视高地板：窗前预抬升（jerk 预算内的 S₅ 坡）---
        if axis[1] < -0.5 and g > 0.0:
            low = min(p[1] for p in seg) + cur_off[1] - g   # 增益全落下时的最低点上界
            deficit = (ground_y + EYE_FLOOR_M) - low
            gap_s = (i0 - prev_end) * dt
            if deficit > 0.0 and gap_s >= max(PRELIFT_MIN_GAP_S,
                                              _jerk_safe_span(deficit) + 0.2):
                ramp = int(min(PRELIFT_MAX_S, gap_s - 0.2) * fps)
                _s5_ramp(corr, i0 - ramp, i0, (0.0, deficit, 0.0))
                cur_off = vadd(cur_off, (0.0, deficit, 0.0))

        for j in range(len(seg)):
            u = j / (len(seg) - 1)
            add = vadd(supp[j], vscale(axis, g * smoothstep5(u)))
            corr[i0 + j] = vadd(corr[i0 + j], add)
        gain = vscale(axis, g)
        # 窗后：增益偏移恒定延续，直到衰减坡把它收回
        for i in range(i1 + 1, n):
            corr[i] = vadd(corr[i], gain)
        cur_off = vadd(cur_off, gain)

        # --- 窗后衰减：在下一个粉色窗前收回全部累计偏移。衰减坡时长由
        #     jerk 预算反解下限，间隙装不下就整段携带到下一个间隙
        #     （常量偏移不影响任何窗的位移/路径判据）---
        next_i0 = (max(0, int(round(pinks[k + 1].t0 * fps)))
                   if k + 1 < len(pinks) else n)
        room_s = (next_i0 - i1) * dt - DECAY_PAD_S
        off_n = vnorm(cur_off)
        if off_n > 1e-9 and room_s >= _jerk_safe_span(off_n):
            d0 = i1 + int(DECAY_PAD_S * 0.5 * fps)
            d1 = min(next_i0 - 2, d0 + int(min(DECAY_MAX_S, room_s) * fps))
            if d0 < n - 1 and d1 > d0:
                _s5_ramp(corr, d0, d1, vscale(cur_off, -1.0))
                cur_off = (0.0, 0.0, 0.0)
        prev_end = i1

    out = [vadd(p, c) for p, c in zip(positions, corr)]
    # 视高地板兜底（修正场只在坏声称处有非零值，钳制不会大面积改形）
    return [(p[0], max(p[1], ground_y + 0.5), p[2]) for p in out]

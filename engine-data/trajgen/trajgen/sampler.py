"""move 时间线 → 平滑轨道 → 逐帧位姿（节点采样 + 限幅 + 五次 B 样条）。

这里是「生成侧保证」的落点（docs/70 §4.2）：
- **转速限幅器**：节点间朝向增量硬夹在 `YAW_RATE_MAX/ELEV_RATE_MAX` 内 ——
  它同时实现了两件事：ω 的构造性上界，以及 docs/71 §4「绝不做朝向硬纠正（禁甩镜头）」，
  哪怕原语要求瞬时切目标（`target_switch` 的「直切」），落到轨迹上也只会是限速摇转。
- **速度限幅器**：节点间位移硬夹在 `V_MAX·KNOT_S` 内。
- 之后统一过五次 B 样条（C⁴）重采样，段界缓入缓出是其推论。

`SmoothTrack` 保留控制点并支持**任意时刻求值**——第四路的时间重参数化
（`cam4.py`，位置要在 σ(t) 处取值而不是整帧网格上）依赖这个能力。
"""

import math
from dataclasses import dataclass, field
from typing import Dict, List, Optional, Sequence, Tuple

from .curve import (KNOT_S, SpeedProfile, knot_times, progress_from_cumulative,
                    smooth_angles, smooth_positions)
from .mathutil import (Quat, Vec3, bspline5_eval, clamp, clamp_ctrl, euler_tuple,
                       quat_from_yaw_elev_roll, smoothstep5, unwrap_deg, vdist,
                       vscale, vsub, wrap180)
from .primitives import ELEV_LIMIT, STATIONARY, Ctx, Move, Pose, build

#: 限幅值（内控，给 QC 阈值留余量：ω p95 ≤90°/s、v p95 ≤6m/s）
YAW_RATE_MAX = 55.0     # deg/s
ELEV_RATE_MAX = 35.0    # deg/s
ROLL_RATE_MAX = 25.0    # deg/s
V_MAX = 4.6             # m/s
#: 位置 jerk 的节点级预算基准（m/s³）。给 docs/69 表 2 的 20 留一成余量。
JERK_MAX = 18.0
#: 段首位置残差的洗出比例（占该 move 时长）
BOUNDARY_BLEND = 0.35
#: 首段朝向并入原语策略的过渡时长（s）——保「初始帧尽量一致」
START_BLEND_S = 2.0
#: 自由壳投影余量（m）：比 scene.CLEARANCE_M 再厚一点，抵消样条切角
SHELL_MARGIN_M = 0.75
#: 投影迭代上限（每轮加厚 0.4m）
SHELL_PASSES = 4
#: 壳内校验的抽帧密度（Hz）。取交付帧率的三分之一：再密收益递减，再疏就漏切角。
PROBE_HZ = 10.0


def _slew_limit(values: Sequence[float], max_step: float) -> List[float]:
    out = [values[0]]
    for x in values[1:]:
        out.append(out[-1] + clamp(x - out[-1], -max_step, max_step))
    return out


def _d3_p95(pts: Sequence[Vec3]) -> float:
    """控制多边形三阶差分的 p95（各轴取最大）。"""
    v = [max(abs(pts[i][k] - 3.0 * pts[i - 1][k] + 3.0 * pts[i - 2][k] - pts[i - 3][k])
             for k in range(3)) for i in range(3, len(pts))]
    if not v:
        return 0.0
    v.sort()
    return v[int(0.95 * (len(v) - 1))]


def _jerk_limit(pts: Sequence[Vec3], max_d3: float, max_passes: int = 12) -> List[Vec3]:
    """三阶（jerk）限幅：节点三阶差分超预算就二项平滑，端点钉死。

    与 `_speed_limit`（一阶）、`_slew_limit`（角一阶）是同一类器件，补的是
    docs/71 §4「套 jerk-limited 速度剖面」里一直缺的那一档 —— 此前 jerk 只靠
    「超了就整体降速重排」这条**事后**通道兜，而降速对被自由壳投影搞皱的
    控制多边形无效（皱是几何的、不是速度的）：走廊类 jerk_p95 中位 15、p90 32，
    门是 20，等于把过闸交给运气。

    五次 B 样条的 `d³p/dt³` 由控制点三阶差分除以 h³ 控制，所以节点级预算
    `max_d3 = j_max·h³` 是 jerk 的构造性上界。二项平滑对节点尺度的皱衰减极快
    （周期 2–3 节点一遍即近乎抹平），对运镜本身的低频动作几乎无损。

    平滑**只落在超预算的那几个节点邻域**：整条平滑会把运镜动作一起削掉，
    实测代价是绕障碍的折角被抹圆后贴回障碍（穿模作废 3→8）、
    队形修正量变大导致共视掉线（4→9）——治好 jerk 却换来两笔新账。
    """
    out = list(pts)
    n = len(out)
    for _ in range(max_passes):
        d3 = [0.0] * n
        for i in range(3, n):
            v = max(abs(out[i][k] - 3.0 * out[i - 1][k] + 3.0 * out[i - 2][k]
                        - out[i - 3][k]) for k in range(3))
            for j in range(i - 3, i + 1):
                if v > d3[j]:
                    d3[j] = v
        if _d3_p95(out) <= max_d3:
            break
        src = list(out)
        for i in range(1, n - 1):
            if d3[i] <= max_d3:
                continue
            a, b, c = src[i - 1], src[i], src[i + 1]
            out[i] = (0.25 * a[0] + 0.5 * b[0] + 0.25 * c[0],
                      0.25 * a[1] + 0.5 * b[1] + 0.25 * c[1],
                      0.25 * a[2] + 0.5 * b[2] + 0.25 * c[2])
    return out


def _speed_limit(pts: Sequence[Vec3], max_step: float) -> List[Vec3]:
    out = [pts[0]]
    for p in pts[1:]:
        d = vsub(p, out[-1])
        n = math.sqrt(d[0] ** 2 + d[1] ** 2 + d[2] ** 2)
        if n > max_step and n > 1e-12:
            d = vscale(d, max_step / n)
        out.append((out[-1][0] + d[0], out[-1][1] + d[1], out[-1][2] + d[2]))
    return out


@dataclass
class SmoothTrack:
    """限幅后的控制点 + 五次 B 样条求值器（可在 [0, duration] 内任意时刻取位姿）。"""
    duration: float
    ctrl: Dict[str, List[float]]

    @property
    def span(self) -> float:
        return float(max(1, len(self.ctrl["x"]) - 5))

    def at(self, t: float) -> Pose:
        s = self.span * clamp(t, 0.0, self.duration) / max(1e-9, self.duration)
        return Pose(
            (bspline5_eval(self.ctrl["x"], s),
             bspline5_eval(self.ctrl["y"], s),
             bspline5_eval(self.ctrl["z"], s)),
            bspline5_eval(self.ctrl["yaw"], s),
            bspline5_eval(self.ctrl["elev"], s),
            bspline5_eval(self.ctrl["roll"], s),
        )


@dataclass
class Track:
    """一路相机的完整计划与采样产物。"""
    cam_id: int
    focal_mm: int
    aspect: str
    moves: List[Move]
    speed_marks: List[Tuple[float, float]]
    start: Pose
    smooth: Optional[SmoothTrack] = None
    positions: List[Vec3] = field(default_factory=list)
    quats: List[Quat] = field(default_factory=list)
    eulers: List[Vec3] = field(default_factory=list)
    speeds: List[Vec3] = field(default_factory=list)
    max_push_m: float = 0.0
    # ---- 车辆导演模式（docs/78 §10）可选覆盖：车载相机的白字段语义 ----
    #: player_* = 车体位姿（缺省 None ⇒ export 用程序化口径：相机地面投影）
    player_positions: Optional[List[Vec3]] = None
    player_quats: Optional[List[Quat]] = None
    player_eulers: Optional[List[Vec3]] = None
    #: camera_Follow Offset = 挂载偏移（车体局部系，恒定；缺省零向量口径）
    follow_offset: Optional[Vec3] = None

    @property
    def stationary_frac(self) -> float:
        if not self.moves:
            return 0.0
        tot = sum(m.duration for m in self.moves)
        st = sum(m.duration for m in self.moves if m.cls in STATIONARY)
        return st / tot if tot > 0 else 0.0

    def to_plan_dict(self) -> Dict:
        return {"id": self.cam_id, "focal_mm": self.focal_mm, "aspect": self.aspect,
                "segments": [m.to_dict() for m in self.moves]}


def sample_knots(track: Track, ctx: Ctx, duration: float, knot_s: float = KNOT_S
                 ) -> Tuple[List[Vec3], List[float], List[float], List[float]]:
    """按 move 时间线在节点网格上求位姿（未限幅、未平滑）。"""
    ts = knot_times(duration, knot_s)
    cum = SpeedProfile(track.speed_marks).cumulative(ts)

    # 节点区间必须**严格首尾相接且不重叠**：一旦重叠，后一个 move 的段首对齐量
    # 会算在一个并不会被发射的节点上，残差洗不掉，段界就留下位置跳变（jerk 尖峰）。
    ranges: List[Tuple[int, int]] = []
    cursor = 0
    n_left = len(track.moves)
    for m in track.moves:
        n_left -= 1
        i1 = min(range(len(ts)), key=lambda i: abs(ts[i] - m.t1))
        i1 = max(cursor + 1, min(i1, len(ts) - 1 - n_left))
        ranges.append((cursor, i1))
        cursor = i1
    if ranges:
        ranges[-1] = (ranges[-1][0], len(ts) - 1)

    pos: List[Vec3] = []
    yaw: List[float] = []
    elev: List[float] = []
    roll: List[float] = []
    cur = track.start
    for m, (i0, i1) in zip(track.moves, ranges):
        run = build(m, ctx, cur)
        # 段界位置对齐：原语按自己的几何起算（环绕的椭圆归一化、竖直轨道的相位等），
        # 残差用 S₅ 在段首 BOUNDARY_BLEND 比例内洗掉——位置与速度都连续，且不累积漂移。
        delta = vsub(cur.p, run(0.0, ts[i0]).p)

        def corrected(u: float, t: float, _run=run, _d=delta) -> Pose:
            po = _run(u, t)
            w = 1.0 - smoothstep5(u / BOUNDARY_BLEND) if u < BOUNDARY_BLEND else 0.0
            return Pose((po.p[0] + _d[0] * w, po.p[1] + _d[1] * w, po.p[2] + _d[2] * w),
                        po.yaw, po.elev, po.roll)

        for i in range(i0, i1 + 1):
            if i < len(pos):        # 段界节点归上一个 move，不重复计
                continue
            pose = corrected(progress_from_cumulative(cum, i0, i1, i), ts[i])
            pos.append(pose.p)
            yaw.append(pose.yaw)
            elev.append(pose.elev)
            roll.append(pose.roll)
        cur = corrected(1.0, ts[i1])
    while len(pos) < len(ts):
        pos.append(pos[-1] if pos else track.start.p)
        yaw.append(yaw[-1] if yaw else track.start.yaw)
        elev.append(elev[-1] if elev else track.start.elev)
        roll.append(roll[-1] if roll else track.start.roll)

    # 首段朝向对齐：69 §2「初始帧尽量一致」是硬要求（QC 有 init.orientation ≤15° 内控条），
    # 但首个 move 若是 follow/target_switch，它一上来就看自己的主体，四路首帧朝向能差 100°+。
    # 这里把首 START_BLEND_S 秒的朝向从 track.start 平滑并到原语策略上。
    n_blend = min(len(ts) - 1, max(1, int(round(START_BLEND_S / knot_s))))
    for k in range(n_blend + 1):
        w = smoothstep5(k / n_blend) if n_blend else 1.0
        yaw[k] = track.start.yaw + wrap180(yaw[k] - track.start.yaw) * w
        elev[k] = track.start.elev + (elev[k] - track.start.elev) * w
        roll[k] = track.start.roll + (roll[k] - track.start.roll) * w

    # 俯仰全局硬带：只在 pan 原语里 clamp 不够——look_at 类（环绕/跟随/螺旋/目标切换）
    # 的 elev 由几何算出，同样能把镜头顶到天花板。交付复核目检的「多段整帧只有天花板」
    # 在只改 pan 之后仍复现（9 帧联表末帧），故在采样出口对整条 elev 统一夹持。
    # 夹持发生在平滑之前，B 样条会把夹持点的折角抹平，不引入角加速度尖峰。
    lo_e, hi_e = ELEV_LIMIT
    for k in range(len(elev)):
        if elev[k] < lo_e:
            elev[k] = lo_e
        elif elev[k] > hi_e:
            elev[k] = hi_e
    return pos, yaw, elev, roll


def build_smooth(track: Track, ctx: Ctx, duration: float,
                 slew_scale: float = 1.0, knot_s: float = KNOT_S) -> SmoothTrack:
    """节点 → 限幅 → clamped 控制点，得到可任意时刻求值的平滑轨道。

    `slew_scale<1` 用于第四路的**标称**轨道：第四路实际角速度是
    `ω₄ = ω_标称(τ)·τ′`，而 `τ′ = ρ·v_g/v_标称` 会大于 1，所以标称侧要预留倍率。
    """
    pos, yaw, elev, roll = sample_knots(track, ctx, duration, knot_s)
    smooth, push = smooth_from_knots(pos, yaw, elev, roll, ctx.scene, duration,
                                     slew_scale=slew_scale, knot_s=knot_s)
    track.max_push_m = push
    return smooth


def smooth_from_knots(pos, yaw, elev, roll, scene, duration: float,
                      slew_scale: float = 1.0, knot_s: float = KNOT_S
                      ) -> Tuple[SmoothTrack, float]:
    """节点位姿 → 自由壳投影 → 限幅 → clamped 控制点。返回 (轨道, 最大推出距离)。

    与 move 计划无关，所以 68 号的整片拓扑（`sfm.py`）与 69 号的分段原语共用这一段——
    穿模防线、速度/转速限幅、样条口径因此只有一份实现。

    自由壳投影（docs/71 §7 防线 1）：节点推出去还不够，五次 B 样条是**逼近**不是插值，
    绕障碍角时曲线会切进去，帧级轨迹照样穿模。按「推 → 平滑 → 抽帧验 → 加厚再推」
    迭代到帧级间隙为正，把「穿模零容忍由构造保证」落到实处。
    """
    raw = list(pos)
    margin = SHELL_MARGIN_M
    for _ in range(SHELL_PASSES):
        # jerk 限幅要**在壳内校验之内**：平滑会把绕障碍的折角抹圆、可能贴回障碍，
        # 所以按「推 → 限幅 → 抽帧验间隙 → 加厚再推」同一条既有回路走，
        # 而不是在回路之外补一刀（那样穿模零容忍就不再是构造性的了）。
        pos = _jerk_limit(_speed_limit([scene.push_out(p, clearance=margin) for p in raw],
                                       V_MAX * knot_s), JERK_MAX * knot_s ** 3)
        # 抽帧验间隙的密度必须逼近交付帧率。此前是「每个控制点采一个、再隔一个取」，
        # 即 0.6s 一验（第四路的粗节点更是 1.2s 一验），而交付是 30fps：
        # 一次持续不到 0.6s 的切角完全看不见 —— 实测穿模作废里有一半是这么漏过来的，
        # 尤其第四路（`build_frames` 沿同一条曲线重参数化，验不到就一路带进交付）。
        probe = smooth_positions(pos, max(64, int(duration * PROBE_HZ)))
        if min(scene.clearance(p) for p in probe) > 0.0:
            break
        margin += 0.4
    push = max((vdist(a, b) for a, b in zip(raw, pos)), default=0.0)
    yaw = _slew_limit(unwrap_deg(yaw), YAW_RATE_MAX * slew_scale * knot_s)
    elev = _slew_limit(elev, ELEV_RATE_MAX * slew_scale * knot_s)
    roll = _slew_limit(roll, ROLL_RATE_MAX * slew_scale * knot_s)
    return SmoothTrack(duration, {
        "x": clamp_ctrl([p[0] for p in pos]),
        "y": clamp_ctrl([p[1] for p in pos]),
        "z": clamp_ctrl([p[2] for p in pos]),
        "yaw": clamp_ctrl(yaw),
        "elev": clamp_ctrl(elev),
        "roll": clamp_ctrl(roll),
    }), push


def frames_from_times(smooth: SmoothTrack, times: Sequence[float], fps: float
                      ) -> Tuple[List[Vec3], List[Quat], List[Vec3], List[Vec3]]:
    """在给定时刻序列上取位姿，返回 (位置, 四元数, 欧拉角, camera_speed)。"""
    poses = [smooth.at(t) for t in times]
    positions = [p.p for p in poses]
    quats = [quat_from_yaw_elev_roll(p.yaw, p.elev, p.roll) for p in poses]
    eulers = [euler_tuple(p.yaw, p.elev, p.roll) for p in poses]
    return positions, quats, eulers, frame_speeds(positions, fps)


def frame_speeds(positions: Sequence[Vec3], fps: float) -> List[Vec3]:
    """逐帧速度，口径与采集侧 `PoseRecorder.BuildLaneFrame` **逐字一致**：
    `speed[0] = 0`、`speed[k] = (pos[k] − pos[k−1])·fps`（**后向**差分）。

    此前这里写的是前向差分 `(pos[k+1] − pos[k])·fps`，与采集侧和 QC 侧都差一帧：
    `engine_qc` 的 `num.player_speed` 拿 `speed[k]` 去核 `(pos[k]−pos[k−1])·fps`，
    偏差恰好是 `|a|·dt`，于是 `gen-clip` 出的每一条都挂这一条（main 上实测 0.014 m/s）。
    采集侧那段注释还专门记着「baseline 冻在第 0 帧」的历史事故 —— 口径本来就是
    后向差分，规划侧照抄即可。
    """
    out: List[Vec3] = [(0.0, 0.0, 0.0)]
    for a, b in zip(positions, positions[1:]):
        out.append(((b[0] - a[0]) * fps, (b[1] - a[1]) * fps, (b[2] - a[2]) * fps))
    return out[:len(positions)]


def sample_track(track: Track, ctx: Ctx, duration: float, n_frames: int,
                 fps: float) -> Track:
    """常规路（1–3）：整帧网格上采样。"""
    track.smooth = build_smooth(track, ctx, duration)
    times = [i / fps for i in range(n_frames)]
    (track.positions, track.quats,
     track.eulers, track.speeds) = frames_from_times(track.smooth, times, fps)
    return track

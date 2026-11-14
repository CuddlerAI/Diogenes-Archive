"""粉色 6 类平移的「叠加窗」实现（docs/89 §5.1 的 pink_coverage 补齐）。

需求口径（需求原文已核实）：
- 6 类平移（translate_up/down/forward/back/left/right）**每一路必含**（含 cam4）；
- 粉色是**叠加成分**，不占 §3.1 的非粉色段——本模块只叠加位置运动与段标注，
  不改动 gaze/focus-lock 的任何排班与朝向逻辑；
- 平移必须是**真实运动**。与 QC 侧商定的诚实性契约（两侧逐字一致）：
  对每个声称 `translate_*` 的窗 [t0,t1]，以**窗起始帧相机 yaw** 为基底
  （forward=(sin yaw,0,cos yaw)、right=(cos yaw,0,−sin yaw)、up/down=世界 Y，
  与 `primitives._basis` 同式），**声称轴向净位移 ≥ 0.8 m 且 ≥ 窗内路径长的 50%**。
  `selfcheck()` 按该契约在生成侧逐窗复核（用**最终相机轨迹的 quat/位置**，
  与 QC 同一输入），任一窗不达标即令该 attempt 作废重排。

实现结构（吸收「前车之鉴」并修正其未解决缺陷）：

**载机 cam1-3 —— 减速叠加（brake→floor→absorb）**
- 窗前 T_B 秒把轨道推进「时钟」τ 平滑刹到地板 β=0.15（dτ = β·dt）：
  角速度、半径/高度起伏全部随 τ 减速 ⇒ 窗内基线近乎驻留，位移由轴向斜坡主导
  （残余切向 ≤~0.9 m/s，对最坏的反平行轴仍留 share 余量），且渐开斜坡
  （∫ω dτ 恒等）与起伏相位都只是**时间伸缩**而非丢失——共视/解耦的标定语义
  保持。不刹到 0 的原因：全冻结吃掉 ~33s 轨道推进，三车弧覆盖骤减 ⇒ 航迹
  方位成簇、cam4 站位失配（见 CAR_BETA_FLOOR 注释）。
- 轴向偏移用五次平滑速度斜坡（T_r 上坡 / 平台 / T_r 下坡），与 β 斜坡半程交叠
  （overlap 参数），速度过零仅 1-2 帧 ⇒ 不触发 motion.static 红线。
- **窗尾把偏移吸收进轨道参数**（前车之鉴的未解决缺陷）：由窗尾实际位置反解
  (r0, h0, az)（扣掉当刻起伏项），轨道从新参数无缝续走——轨迹在每一时刻都仍是
  「绕同一中心的轨道」，不存在「偏离基线 34-52m」的悬空偏移。
- 6 窗按 3 对成链（up/down、forward/back、left/right 同幅配对、链内相邻）：
  对内位移几何相消 ⇒ 参数漂移 ≈ 0，链间轨道回到抽样带内。
- 三路近静止「岛」去冲突重抽：任一时刻不允许三路同时落在交接岛内
  （motion.all3_static 零容忍）。

**cam4 —— 保速换向（|v₄| 逐帧不变，只转方向）**
- cam4 的 |v₄(t)| ≡ ρ(t)·|v_guide(t)| 是 QC ρ 反算「恰 1 次变速」的根基，
  窗内绝不能刹车/变速；改为把速度**方向**从轨道切向平滑摆到声称轴向再摆回，
  |Δpos| 每帧严格 = v₄·dt ⇒ ρ 反算完全不受影响。
- 转向斜坡时长按摆角/速度双预算自适应（加速度峰 ≈ v·θ/T_r·1.875、
  jerk 峰 ≈ v·θ·5.77/T_r²，各压在 p95 门下）；反平行轴（left，切向≈相机
  right 的构造性结果）经竖直枢轴走 180°，上/下坡各经 +ŷ/−ŷ ⇒ 竖直副产物相消。
- left 窗走**谷-谷排法**：两个摆坡分别踩在 v₄ 预测的相邻两个速度谷上
  （反平行摆坡近零轴向贡献，放最慢处路径开销最小），平台跨过中间鼓包出净位移
  ——「骑谷居中」的旧排法在速度回升时 share 塌到 0.30-0.47，已废弃。
- 平台段在轴向累计够 A_target 后**提前收坡**（径向/竖直类设上限，防吸收后
  半径/高度漂出取景余量；切向类只等价于方位平移，不设上限；反平行窗不提前收
  ——下坡必须落回下一个谷），段的 t1 按实际收坡时刻写进 plan ⇒ 声称窗与
  真实运动逐帧对应。
- 窗尾同样把位置吸收进 (r4, h4, az4) 续走。

朝向归属（与 gaze 的共存裁决）：**朝向永远归原 gaze/focus-lock 逻辑，粉色窗只动
位置**。两通道正交：重叠时诚实性契约以窗起帧真实相机 yaw 定基（无论该 yaw 来自
focus-lock 还是自由观察段），轴向就是「相机当刻视向系」下的真实平移；共视预算
（FREE_FRAC_PER_CAM 标定）与自由观察排班完全不被扰动。
"""

import math
import random
from dataclasses import dataclass
from typing import Callable, Dict, List, Optional, Sequence, Tuple

from .mathutil import (Vec3, clamp, smoothstep5, vadd, vdist, vnorm, vscale,
                       vsub, wrap180)
from .primitives import Move

PINK_CLASSES = ("translate_up", "translate_down", "translate_forward",
                "translate_back", "translate_left", "translate_right")

#: 类 → 轴（相机 yaw 基底下；up/down 为世界 Y）——与 primitives._translate 同表
_AXIS_OF = {"translate_up": ("y", +1.0), "translate_down": ("y", -1.0),
            "translate_forward": ("fwd", +1.0), "translate_back": ("fwd", -1.0),
            "translate_right": ("right", +1.0), "translate_left": ("right", -1.0)}


def axis_of(cls: str, yaw_deg: float) -> Vec3:
    """类 → 单位轴向（窗起帧相机 yaw 基底，与 primitives._basis 同式）。"""
    kind, sgn = _AXIS_OF[cls]
    if kind == "y":
        return (0.0, sgn, 0.0)
    ry = math.radians(yaw_deg)
    if kind == "fwd":
        return (sgn * math.sin(ry), 0.0, sgn * math.cos(ry))
    return (sgn * math.cos(ry), 0.0, -sgn * math.sin(ry))

#: 配对（链）：对内同幅、相邻 ⇒ 轨道参数漂移几何相消
_PAIRS = (("translate_up", "translate_down"),
          ("translate_forward", "translate_back"),
          ("translate_left", "translate_right"))

# ---------------------------------------------------------------- 载机参数
#: 刹车/释放斜坡（s）：把轨道时钟 τ 的推进率 β 平滑压到地板 / 拉回 1。
#: 2.0 而不是更陡的 1.4：cam4 改「同路径异速率」后，guide 的粉色斜坡 jerk 会被
#: 快档按 ρ（速度项）与 ρ³（曲率项）放大回放（实测 ρ₂=1.65 时 cam4 jerk p95
#: 28-59），guide 侧必须把自身 jerk 压到 ~7 以内（docs/71 §6 传导预算的 jerk 版）
CAR_T_BRAKE = 2.0
#: 窗内轨道时钟地板（不刹到 0）：
#: - 全冻结会把 60s 里 ~33s 的轨道推进吃掉，三车弧覆盖骤减 ⇒ 航迹在方位上成簇、
#:   traj_center 偏离轨道中心几十米，cam4 的包围球站位/瞄准双双失配
#:   （实测 seed=1 全部 attempt 在 t=0 就 cam4.frame 违规——基线 0 例）；
#: - 地板保留大部分弧覆盖，且车速恒 >0.2 ⇒ 近静止岛基本消失；
#: - 地板统一 0.15：切向残速 β·v_t 峰值 ~0.15·5.8≈0.9 m/s。曾按类分档
#:   （fwd/back 与切向正交 ⇒ 0.30），但窗轴取的是**窗起帧相机 yaw**——
#:   自由观察段（look_left/back…）叠在窗上时 fwd/back 轴一样会转到切向
#:   （实测 seed13 forward 在 look 段里 net 8.1→2.8m、share 0.49），
#:   残速对「任意轴」都必须按最坏情况（反平行）压制。
CAR_BETA_FLOOR = {c: 0.15 for c in PINK_CLASSES}
#: 轴向速度斜坡（s）——峰值 jerk ≈ v·5.77/T_r²（v=3.0 ⇒ 1.6s 时 ~6.8 m/s³）。
#: 取 1.6 同为传导预算：guide 轴向斜坡的 jerk 会被 cam4 快档放大 ρ 倍回放
CAR_T_RAMP = 1.6
#: 轴向斜坡相对窗界的交叠（s）：上坡在 t0 前 CAR_OVERLAP 起步（与刹车尾交叠）、
#: β 释放在链尾 t1 前 CAR_OVERLAP 起步（与轴向下坡交叠）——两处交叠都是为了
#: 车速不在交接处塌到 <0.2 m/s（motion.static 红线的 5% 预算是最紧的一条）
CAR_OVERLAP = 0.9
#: 平台速度带（m/s）：水平 / 竖直（竖直提到 ≥1.4 抬 share；幅度上限靠窗长压）
CAR_V_HORIZ = (2.2, 3.0)
CAR_V_VERT = (1.4, 1.7)
#: 窗长带（s）：水平 / 竖直
CAR_WIN_HORIZ = (3.6, 4.4)
CAR_WIN_VERT = (2.8, 3.2)
#: 链内两窗间隔（s）：刻意小于 CAR_OVERLAP——第二窗的上坡直接接在第一窗
#: 下坡末尾（clip 到上一窗 t1 之后），间隙近静止段收在 ~0.3s
CAR_GAP_INTRA = (0.35, 0.55)
CAR_GAP_INTER = 2.3
#: 首窗保护带（s，对齐自由观察段的片首保护带）与片尾保护（s）
CAR_GUARD_HEAD = 12.0
CAR_GUARD_TAIL = 2.8
#: 近静止「岛」的三路去冲突：重抽上限（抽不出无三重叠的排布就放行给自检兜底）
CAR_DECONFLICT_TRIES = 40

# ---------------------------------------------------------------- cam4 参数
#: 首尾保护带：cam4 不进 init 闸门、瞄准自首帧即对 traj_center 成立，
#: 保护带只为避开片头/片尾的收尾余量——压到 8/2（6 窗总预算 ~45s，49.5s
#: 可用跨度才铺得下）
CAM4_GUARD_HEAD = 8.0
CAM4_GUARD_TAIL = 2.0
CAM4_GAP = (0.5, 0.8)
#: 转向斜坡时长：T_r = clamp(K_SWEEP·θ·v, T_MIN, T_MAX)（θ 弧度、v m/s）——
#: 峰值加速度 ≈ v·(θ/T_r)·1.875，压在 ACCEL_P95_MAX=8 的 ~90% 下
CAM4_TR_K = 0.30
#: T_r 下限 0.7：低速谷（v₄~0.4 m/s）里 180° 摆的加速度/加加速度峰都 ∝ v，
#: 长斜坡反而白吃 share 的路径分母（实测 share 0.42 卡死在斜坡上）
CAM4_TR_MIN, CAM4_TR_MAX = 0.7, 3.2
#: 转向斜坡的 jerk 预算（m/s³）：峰值 jerk ≈ v·θ·5.77/T_r² ⇒
#: T_r ≥ sqrt(v·θ·5.77/J)。cam4 的 |v₄|=ρ·|v_guide| 会把 guide 粉色窗的
#: 轴向斜坡 jerk（×ρ）原样传导进来，窗口重叠时两源叠加——预算取 8 使叠加
#: 上界 ~16 仍低于 20 的门（J=12 实测叠加冲到 20.2 贴门）
CAM4_JERK_BUDGET = 6.0


def _t_ramp4(theta: float, v: float) -> float:
    """cam4 转向斜坡时长：速度比例项与 jerk 预算项取大，再夹进 [MIN, MAX]。"""
    t_speed = CAM4_TR_K * theta * max(0.8, v)
    t_jerk = math.sqrt(max(1e-6, v * theta * 5.77 / CAM4_JERK_BUDGET))
    return clamp(max(t_speed, t_jerk), CAM4_TR_MIN, CAM4_TR_MAX)


def _ramp_gain(theta: float) -> float:
    """摆坡的轴向增益 G(θ) = ∫₀¹cos(θ·(1−S₅(x)))dx：θ=90° 时 ~0.64，180° 时 0。
    share 下界由 (T_h + 2G·T_r)/(T_h + 2T_r) ≥ 0.55 给出 ⇒ 平台需
    T_h ≥ T_r·(1.1−2G)/0.45（θ 小时几乎不吃平台——摆坡自己就带轴向）。"""
    acc = 0.0
    for k in range(8):
        x = (k + 0.5) / 8.0
        acc += math.cos(theta * (1.0 - smoothstep5(x)))
    return acc / 8.0


def _hold_factor(theta: float) -> float:
    return max(0.6, (1.1 - 2.0 * _ramp_gain(theta)) / 0.45)
#: 平台段轴向累计目标 / 上限（m）：径向与竖直类（吸收后改 r4/h4，关乎取景）
CAM4_A_TARGET_RADIAL = 6.0
#: 切向类（left/right）等价于方位平移，不威胁取景，不设上限
CAM4_A_TARGET_TANG = 9.0
#: 诚实性契约（与 QC 侧逐字一致）
NET_MIN_M = 0.8
SHARE_MIN = 0.5


@dataclass
class PinkWindow:
    cls: str
    t0: float
    t1: float                     # 载机=计划即实际；cam4=计划上限，收坡后回写实际
    v_axial: float = 0.0          # 载机平台速度（cam4 不用——速度是 ρ·v_g 给定的）
    amp: float = 0.0              # 载机计划幅值（全积分净位移）
    chain_head: bool = False      # 链首（前面要接刹车）
    chain_tail: bool = False      # 链尾（后面要接释放）
    ov_in: float = 0.6            # 上坡相对 t0 的提前量（链内第二窗被 clip 小）
    t_ramp: float = 1.6           # 载机轴向斜坡时长（guide 走 gentle 档会加长）
    # ---- 运行时回填 ----
    basis_yaw: Optional[float] = None      # 窗起帧预测相机 yaw（整形用）
    t1_actual: Optional[float] = None      # cam4 提前收坡后的实际 t1
    dbg: Optional[Dict] = None             # 驱动侧诊断（入窗速度/摆角/相位时长）


# ================================================================ 排窗

def schedule_carrier_windows(rng: random.Random, duration: float,
                             gentle: bool = False) -> List[PinkWindow]:
    """单条载机路的 6 窗排程：3 条成对链，链序/对内序/时距全部随机。

    `gentle=True` 给 **guide 载机**：其相机速度剖面会被 cam4 快档按 ρ 放大回放
    （v₄≡ρ·v_g），斜坡拉长（×1.5）、平台压低（×0.78）把 guide 侧 jerk 压到
    ~4-5，快档 ×1.85 后仍 ≤10（docs/71 §6 传导预算的 jerk 版落到窗参数上）。"""
    pairs = [list(p) for p in _PAIRS]
    rng.shuffle(pairs)
    for p in pairs:
        rng.shuffle(p)
    chains: List[List[PinkWindow]] = []
    for pair in pairs:
        wins: List[PinkWindow] = []
        vert = pair[0].startswith(("translate_up", "translate_down"))
        # gentle 只软化**水平**窗：竖直窗轴向平台才 ~1.4 m/s、|v_g| 增量走正交
        # 合成（Δ|v|≈0.5-0.9），本就不是 jerk 传导源；水平窗 Δ|v|≈2.2-3.0 才是。
        # 竖直窗照常还能保住 net ≥0.8 的契约余量（gentle 压幅会把它压穿）。
        soft = gentle and not vert
        t_ramp = 2.2 if soft else CAR_T_RAMP
        v = rng.uniform(*(CAR_V_VERT if vert else CAR_V_HORIZ)) \
            * (0.8 if soft else 1.0)
        t_win = rng.uniform(*(CAR_WIN_VERT if vert else CAR_WIN_HORIZ)) \
            + (0.4 if soft else 0.0)
        amp = v * (t_win - t_ramp)
        for k, cls in enumerate(pair):
            w = PinkWindow(cls, 0.0, t_win, v_axial=v, amp=amp)
            w.t_ramp = t_ramp
            w.chain_head = (k == 0)
            w.chain_tail = (k == len(pair) - 1)
            wins.append(w)
        chains.append(wins)
    # 时间铺排：链脚印 = T_B + Σ窗长 + 链内 gap + T_B；余量随机分给 4 个间隙
    intra = [rng.uniform(*CAR_GAP_INTRA) for _ in chains]
    foot = [CAR_T_BRAKE * 2 + sum(w.t1 for w in c) + g
            for c, g in zip(chains, intra)]
    span = duration - CAR_GUARD_HEAD - CAR_GUARD_TAIL
    slack = span - sum(foot) - CAR_GAP_INTER * (len(chains) - 1)
    if slack < 0:
        raise RuntimeError("pink: 载机 6 窗排不进 %.0fs（脚印 %.1fs）"
                           % (duration, sum(foot)))
    cuts = sorted(rng.uniform(0.0, slack) for _ in range(len(chains)))
    extra = [cuts[0]] + [b - a for a, b in zip(cuts, cuts[1:])]
    out: List[PinkWindow] = []
    t = CAR_GUARD_HEAD
    for ci, chain in enumerate(chains):
        t += extra[ci] + (CAR_GAP_INTER if ci else 0.0)
        t += CAR_T_BRAKE
        for k, w in enumerate(chain):
            if k:
                t += intra[ci]
                w.ov_in = min(CAR_OVERLAP, intra[ci] - 0.05)
            else:
                w.ov_in = CAR_OVERLAP
            dur = w.t1
            w.t0, w.t1 = t, t + dur
            t += dur
            out.append(w)
        t += CAR_T_BRAKE
    return out


def _dip_islands(wins: List[PinkWindow]) -> List[Tuple[float, float]]:
    """车速可能塌到 <0.2 m/s 的时间岛（交接处）：窗起/窗尾各一小段。"""
    out = []
    for w in wins:
        out.append((w.t0 - w.ov_in - 0.1, w.t0 + 0.35))
        out.append((w.t1 - 0.35, w.t1 + 0.35))
    return out


def schedule_all_carriers(rng: random.Random, duration: float,
                          gentle_slot: Optional[int] = None
                          ) -> List[List[PinkWindow]]:
    """三路载机排窗 + 近静止岛去冲突：重抽到没有任一时刻三路同时落岛
    （motion.all3_static 是零容忍红线），并把两两同岛时长压到最低的候选。
    `gentle_slot` = 预定的 guide 载机（其窗走 gentle 档，见
    schedule_carrier_windows）。"""
    best, best_pair = None, None
    for _ in range(CAR_DECONFLICT_TRIES):
        cand = [schedule_carrier_windows(rng, duration,
                                         gentle=(k == gentle_slot))
                for k in range(3)]
        islands = [_dip_islands(ws) for ws in cand]

        def cover(k: int, t: float) -> bool:
            return any(a <= t <= b for a, b in islands[k])

        triple = 0.0
        pair = 0.0
        t = 0.0
        while t < duration:
            c = sum(1 for k in range(3) if cover(k, t))
            if c >= 3:
                triple += 0.05
            elif c == 2:
                pair += 0.05
            t += 0.05
        if triple <= 0.0:
            if best is None or pair < best_pair:
                best, best_pair = cand, pair
            if pair <= 0.6:
                break
    return best if best is not None else cand


def schedule_cam4_windows(rng: random.Random, duration: float,
                          v4_pred: Sequence[float], fps: float,
                          theta_fn: Optional[Callable[[str, float], float]]
                          = None) -> List[PinkWindow]:
    """cam4 排窗入口：谷-谷间距逐级收紧重试（left 窗最长可到 13s，把中段吃掉后
    其余 4 窗偶发排不下——短间距的谷对仍是合法谷-谷窗，share 余量吃一点）。

    `theta_fn(cls, t0) → 预计摆角(rad)`：调用方按真实基线几何（路径切向 vs
    声称轴）给摆角。缺省退回按类估计表——但同路径模式下相机 yaw 与路径切向的
    夹角随弧位漂移 ±30°+，估计表撑不住（实测窗长欠配 ⇒ 斜坡被 t_fit 压扁，
    jerk 29+/share 0.45）。"""
    last_err: Optional[RuntimeError] = None
    for t_b_cap in (11.5, 9.0, 7.0):
        try:
            return _schedule_cam4_windows(rng, duration, v4_pred, fps, t_b_cap,
                                          theta_fn)
        except RuntimeError as e:
            last_err = e
    raise last_err


def _schedule_cam4_windows(rng: random.Random, duration: float,
                           v4_pred: Sequence[float], fps: float,
                           t_b_cap: float,
                           theta_fn: Optional[Callable[[str, float], float]]
                           = None) -> List[PinkWindow]:
    """cam4 的 6 窗排程 = **谷-谷格架**：v₄ 预测的相邻局部速度谷之间各放一窗，
    摆坡踩在谷上（v 低 ⇒ jerk=v·θ·5.77/T_r² 便宜、T_r 短）、平台跨过鼓包
    （v 高 ⇒ 净位移大、share 高）。这是 left 谷-谷排法对全部 6 类的推广——
    快档下把窗排进快速区、斜坡按 jerk 预算加长再被窗长压扁的老路实测
    p95 20-62 顶穿，谷-谷是唯一同时喂饱 jerk/share/包装预算的排布。
    相邻窗共享谷（前窗 t1 与后窗 t0 隔 ~0.7s 骑在同一谷两侧）。"""
    n = len(v4_pred)
    lo_all, hi_all = CAM4_GUARD_HEAD + 0.5, duration - CAM4_GUARD_TAIL
    sm: List[float] = []
    box = max(1, int(1.0 * fps))
    acc2 = 0.0
    for i, v in enumerate(v4_pred):
        acc2 += v
        if i >= box:
            acc2 -= v4_pred[i - box]
        sm.append(acc2 / min(i + 1, box))

    # 局部速度谷（间距 ≥3.6s，含首尾哨兵）
    mins: List[Tuple[float, float]] = []
    i_lo = int((lo_all + 0.7) * fps)
    i_hi = int((hi_all - 0.7) * fps)
    for i in range(max(1, i_lo), min(n - 1, i_hi)):
        if sm[i] <= sm[i - 1] and sm[i] <= sm[i + 1]:
            t_i = i / fps
            if mins and t_i - mins[-1][0] < 3.6:
                if sm[i] < mins[-1][1]:
                    mins[-1] = (t_i, sm[i])
            else:
                mins.append((t_i, sm[i]))
    if not mins or mins[0][0] > lo_all + 5.0:
        mins.insert(0, (lo_all + 0.6, sm[int((lo_all + 0.6) * fps)]))
    if mins[-1][0] < hi_all - 5.0:
        mins.append((hi_all - 0.6, sm[int((hi_all - 0.6) * fps)]))
    # 谷间槽：跨度夹进 [3.8, 13]；过长的谷间距按 12s 切分（切点也算「肩谷」，
    # 肩上 v 高一点，由驱动的坡长自适应兜住）
    slots: List[Tuple[float, float, float]] = []   # (t0, t1, 坡速代价)
    for (a, va), (b, vb) in zip(mins, mins[1:]):
        seg_edges = [a]
        span = b - a
        n_cut = max(1, int(math.ceil(span / 12.0)))
        for kk in range(1, n_cut):
            seg_edges.append(a + span * kk / n_cut)
        seg_edges.append(b)
        for x0, x1 in zip(seg_edges, seg_edges[1:]):
            if x1 - x0 >= 3.8:
                c0 = sm[min(n - 1, int(x0 * fps))]
                c1 = sm[min(n - 1, int(x1 * fps))]
                slots.append((x0, x1, c0 + c1))
    # 槽不足 6 时把最长槽对半劈（劈点在肩上，v 稍高，由驱动坡长自适应兜住）
    while len(slots) < 6:
        slots.sort(key=lambda s: s[1] - s[0], reverse=True)
        x0, x1, _c = slots[0]
        if (x1 - x0) / 2.0 < 3.8:
            raise RuntimeError("pink: cam4 谷槽不足（%d<6；谷=%s）"
                               % (len(slots), [round(t, 1) for t, _ in mins]))
        xm = (x0 + x1) / 2.0
        cm = sm[min(n - 1, int(xm * fps))]
        c0 = sm[min(n - 1, int(x0 * fps))]
        c1 = sm[min(n - 1, int(x1 * fps))]
        slots = slots[1:] + [(x0, xm, c0 + cm), (xm, x1, cm + c1)]
    # 类 → 槽：按**实测摆角**降序配「坡速代价」升序的槽（摆角大 ⇒ 摆坡贵 ⇒
    # 要踩最慢的谷）。反平行类不必然是 left——同路径模式下相机 right 与路径
    # 切向的正反取决于 guide 轨道旋向（rng 抽），旋向反转时 right 才是 180° 摆
    # （实测 39 个 honesty 失败全是 right 在 dot=-0.99 下被按 20° 摆排窗）。
    slots.sort(key=lambda s: s[2])
    cls_rank = list(PINK_CLASSES)
    if theta_fn is not None:
        t_probe = (lo_all + hi_all) / 2.0
        cls_rank.sort(key=lambda c: -theta_fn(c, t_probe))
    else:
        cls_rank.sort(key=lambda c: 0 if c == "translate_left" else 1)
    n_take = len(cls_rank)
    take_slots = slots[:n_take]
    # 摆角最大的两类拿最便宜的两个槽；其余槽随机洗（时序多样性）
    head_slots = take_slots[:2]
    tail_slots = take_slots[2:]
    rng.shuffle(tail_slots)
    chosen = list(zip(cls_rank, head_slots + tail_slots))
    out: List[PinkWindow] = []
    for cls, (x0, x1, _c) in chosen:
        out.append(PinkWindow(cls, x0 + 0.35, x1 - 0.35))
    out.sort(key=lambda w: w.t0)
    for a4, b4 in zip(out, out[1:]):
        if b4.t0 < a4.t1 + 0.4:
            b4.t0 = a4.t1 + 0.4
            if b4.t1 - b4.t0 < 3.0:
                raise RuntimeError("pink: cam4 谷槽重叠收缩后过短 %s@%.1f"
                                   % (b4.cls, b4.t0))
    return out


# ================================================================ 载机重积分

def _chain_spans(windows: List[PinkWindow]
                 ) -> List[Tuple[float, float, float]]:
    """按 chain_head/chain_tail 把窗折成 (冻结起, 冻结止, β 地板)。
    链 = 同族配对 ⇒ 链内两窗的 CAR_BETA_FLOOR 相同。"""
    spans: List[Tuple[float, float, float]] = []
    head_t: Optional[float] = None
    floor = 0.3
    for w in sorted(windows, key=lambda x: x.t0):
        if w.chain_head:
            head_t = w.t0
            floor = CAR_BETA_FLOOR.get(w.cls, 0.3)
        if w.chain_tail:
            spans.append((head_t if head_t is not None else w.t0, w.t1, floor))
            head_t = None
    return spans


def _beta(t: float, spans: List[Tuple[float, float, float]]) -> float:
    """轨道时钟推进率 ∈[floor,1]：链首前 T_B 平滑刹到该链的 β 地板，
    整链（含链内间隙）保持地板，链尾 t1 前 CAR_OVERLAP 起平滑释放。"""
    for a, b, fl in spans:
        rel0 = b - CAR_OVERLAP
        if a - CAR_T_BRAKE <= t <= rel0 + CAR_T_BRAKE:
            if t < a:
                return 1.0 - (1.0 - fl) * smoothstep5(
                    (t - (a - CAR_T_BRAKE)) / CAR_T_BRAKE)
            if t > rel0:
                return fl + (1.0 - fl) * smoothstep5((t - rel0) / CAR_T_BRAKE)
            return fl
    return 1.0


def _axial_speed(t: float, w: PinkWindow) -> float:
    """窗的轴向速度剖面：五次平滑 上坡 w.t_ramp / 平台 / 下坡 w.t_ramp。"""
    a = w.t0 - w.ov_in
    if t < a or t > w.t1:
        return 0.0
    if t < a + w.t_ramp:
        return w.v_axial * smoothstep5((t - a) / w.t_ramp)
    if t > w.t1 - w.t_ramp:
        return w.v_axial * smoothstep5((w.t1 - t) / w.t_ramp)
    return w.v_axial


def reintegrate_carrier(orbit_args: Dict, windows: List[PinkWindow],
                        cam_yaw_fn: Callable[[float, Vec3, float], float],
                        fps: float) -> Tuple[List[Vec3], List[float],
                                             List[float], List[float], List[Dict]]:
    """带粉色窗的轨道积分（基线段与 director.integrate_orbit 逐帧同式）。

    返回 (positions, yaws, s_series, speeds, absorb_log)。
    **前缀守卫**：首窗刹车开始前的帧必须与原积分逐位一致（公式漂移即抛错），
    由调用方用原 positions 校验。
    """
    center = orbit_args["center"]
    n_frames = orbit_args["n_frames"]
    r0 = orbit_args["r0"]
    h0 = orbit_args["h0"]
    az = orbit_args["az0"]
    omega = orbit_args["omega_series"]
    r_wob = orbit_args["r_wobble"]
    r_per = orbit_args["r_period"]
    h_per = orbit_args["h_period"]
    dt = 1.0 / fps
    head_sgn = 1.0 if sum(omega) >= 0.0 else -1.0

    wins = sorted(windows, key=lambda w: w.t0)
    spans = _chain_spans(wins)
    pos: List[Vec3] = []
    yaws: List[float] = []
    s_series: List[float] = []
    absorb_log: List[Dict] = []
    tau = 0.0
    arc = 0.0
    off = (0.0, 0.0, 0.0)          # 当前窗累计偏移（窗尾吸收清零）
    active: Optional[PinkWindow] = None
    wi = 0                          # 严格顺序推进：已吸收的窗不会再激活
    axis: Vec3 = (0.0, 0.0, 0.0)
    delta = 0.0

    def _wobble(tau_: float) -> Tuple[float, float]:
        return (r_wob * math.sin(2.0 * math.pi * tau_ / r_per),
                1.6 * math.sin(2.0 * math.pi * tau_ / h_per + 0.7))

    for i in range(n_frames):
        t = i * dt
        # ---- 窗状态机（顺序指针）----
        if active is None and wi < len(wins):
            w = wins[wi]
            if w.t0 - w.ov_in <= t <= w.t1:
                active = w
                delta = 0.0
                axis = (0.0, 0.0, 0.0)   # 起帧再定（yaw 要用当帧位姿）
        # ---- 基线轨道点（时钟 τ 推进）----
        wr, wh = _wobble(tau)
        r = r0 + wr
        h = h0 + wh
        a = math.radians(az)
        base = (center[0] + r * math.cos(a), center[1] + h,
                center[2] + r * math.sin(a))
        if active is not None and axis == (0.0, 0.0, 0.0):
            # 窗起（含 overlap 提前量）帧：以**当帧**车体位姿求相机 yaw → 轴
            veh_yaw_now = math.degrees(math.atan2(
                -math.sin(a) * head_sgn, math.cos(a) * head_sgn))
            cam_yaw = cam_yaw_fn(t, vadd(base, off), veh_yaw_now)
            active.basis_yaw = cam_yaw
            kind, sgn = _AXIS_OF[active.cls]
            if kind == "y":
                axis = (0.0, sgn, 0.0)
            else:
                ry = math.radians(cam_yaw)
                fwd = (math.sin(ry), 0.0, math.cos(ry))
                right = (math.cos(ry), 0.0, -math.sin(ry))
                axis = vscale(fwd if kind == "fwd" else right, sgn)
        # ---- 轴向推进 ----
        if active is not None:
            delta += _axial_speed(t, active) * dt
            off = vscale(axis, delta)
        p = vadd(base, off)
        pos.append(p)
        # 机头：由实际水平位置的方位角给切向（无窗时与原式一致；窗内平滑演化）
        az_act = math.degrees(math.atan2(p[2] - center[2], p[0] - center[0]))
        aa = math.radians(az_act)
        yaws.append(math.degrees(math.atan2(
            -math.sin(aa) * head_sgn, math.cos(aa) * head_sgn)))
        # ---- 时钟与方位推进 ----
        b = _beta(t, spans)
        # ω 按「时钟」采样（+0.5 取整防 3·(1/30)·30=2.9999… 截断掉帧）：
        # β<1 只是时间伸缩，∫ω(τ)dτ（渐开斜坡=方位跨度）与起伏相位全部保持
        idx = min(n_frames - 1, int(tau * fps + 0.5))
        step = abs(math.radians(omega[idx])) * r * b * dt
        arc += step + (_axial_speed(t, active) * dt if active is not None else 0.0)
        s_series.append(arc)
        az += omega[idx] * b * dt
        tau += b * dt
        # ---- 窗尾吸收：把偏移折进 (r0, h0, az)，轨道语义不破 ----
        if active is not None and t >= active.t1 - 0.5 * dt:
            wr2, wh2 = _wobble(tau)
            rel = (p[0] - center[0], p[2] - center[2])
            r_tot = math.hypot(rel[0], rel[1])
            az_meas = math.degrees(math.atan2(rel[1], rel[0]))
            az_new = az + wrap180(az_meas - az)
            absorb_log.append({
                "cls": active.cls, "t": round(t, 3),
                "dr": round(r_tot - wr2 - r0, 3),
                "dh": round((p[1] - center[1]) - wh2 - h0, 3),
                "daz": round(az_new - az, 3)})
            r0 = r_tot - wr2
            h0 = (p[1] - center[1]) - wh2
            az = az_new
            off = (0.0, 0.0, 0.0)
            delta = 0.0
            active = None
            wi += 1
    speeds = [0.0]
    for i in range(1, n_frames):
        speeds.append(vnorm(vsub(pos[i], pos[i - 1])) * fps)
    if len(speeds) > 1:
        speeds[0] = speeds[1]
    return pos, yaws, s_series, speeds, absorb_log


# ================================================================ cam4 驱动

def _slerp_dir(a: Vec3, b: Vec3, s: float, pivot_sign: float) -> Vec3:
    """单位向量球面插值；近反平行时经枢轴两段走。

    枢轴默认竖直 (0, pivot_sign, 0)；当 a/b 本身近竖直（同路径模式下 guide 的
    竖直粉色窗会给出 |t̂_y|→1 的路径切向）时竖直枢轴与端点再度反平行，改取
    垂直于 a 的水平向——否则递归不收敛（实测 RecursionError）。"""
    d = clamp(a[0] * b[0] + a[1] * b[1] + a[2] * b[2], -1.0, 1.0)
    if d < -0.9:
        piv = (0.0, pivot_sign, 0.0)
        if (abs(a[0] * piv[0] + a[1] * piv[1] + a[2] * piv[2]) > 0.9
                or abs(b[0] * piv[0] + b[1] * piv[1] + b[2] * piv[2]) > 0.9):
            hx, hz = -a[2], a[0]
            hl = math.hypot(hx, hz)
            piv = (hx / hl, 0.0, hz / hl) if hl > 1e-6 else (1.0, 0.0, 0.0)
        if s < 0.5:
            return _slerp_dir(a, piv, s * 2.0, pivot_sign)
        return _slerp_dir(piv, b, s * 2.0 - 1.0, pivot_sign)
    th = math.acos(d)
    if th < 1e-6:
        return b
    w0 = math.sin((1.0 - s) * th) / math.sin(th)
    w1 = math.sin(s * th) / math.sin(th)
    v = (a[0] * w0 + b[0] * w1, a[1] * w0 + b[1] * w1, a[2] * w0 + b[2] * w1)
    n = vnorm(v) or 1.0
    return (v[0] / n, v[1] / n, v[2] / n)


class Cam4Driver:
    """cam4 逐帧窗驱动：保速换向 + 提前收坡 + 窗尾吸收信号。

    **与基线轨迹形态解耦**（需求方 0714 评论已确认 cam4 语义后续换成
    guide 路径 + ρ 重参数化——本驱动不得绑死统览圆几何）：驱动只消费
    每帧的 (t, 基线名义运动方向 tang, 速度模 v4, 相机 yaw)，产出运动方向；
    它不知道基线是圆轨道还是任意路径。调用方契约：
    - 返回 None ⇒ 本帧按基线自身的推进方式走；
    - 返回单位向量 d ⇒ 本帧位置按 pos += v4·dt·d 显式推进（|v4| 不变 ⇒
      ρ 反算无感）；
    - `absorb_now` 为 True 的帧，调用方把当前实际位置**折回自己的基线参数化**
      （统览圆=反解 r4/h4/az4；guide 路径=折进横向偏置等），保证后续名义推进
      从当前位置以连续速度方向续走。
    `caps` 为「类 → 轴向累计上限 m」的调用方策略（None=不设限）：哪些轴威胁
    取景/走廊约束由基线语义决定，不属于驱动本身。
    """

    def __init__(self, windows: List[PinkWindow], fps: float,
                 caps: Optional[Dict[str, Optional[float]]] = None):
        self.wins = sorted(windows, key=lambda w: w.t0)
        self.fps = fps
        self.caps = caps if caps is not None else {
            c: (None if c in ("translate_left", "translate_right")
                else CAM4_A_TARGET_RADIAL) for c in PINK_CLASSES}
        self.active: Optional[PinkWindow] = None
        self.phase = "idle"        # idle / ramp_in / hold / ramp_out
        self.t_phase = 0.0
        self.t_r = 1.5
        self.tang0: Vec3 = (0.0, 0.0, 1.0)
        self.axis: Vec3 = (0.0, 0.0, 1.0)
        self.pivot = 1.0
        self.a_accum = 0.0
        self.absorb_now = False
        self.hold_min = 0.0
        self.a_target = CAM4_A_TARGET_RADIAL
        self.no_early = False
        self.theta = math.pi / 2.0

    def direction(self, t: float, tang: Vec3, v4: float,
                  cam_yaw: float) -> Optional[Vec3]:
        self.absorb_now = False
        if self.active is None:
            for w in self.wins:
                if w.t0 <= t < w.t1 and w.t1_actual is None:
                    self.active = w
                    w.basis_yaw = cam_yaw
                    kind, sgn = _AXIS_OF[w.cls]
                    if kind == "y":
                        self.axis = (0.0, sgn, 0.0)
                    else:
                        ry = math.radians(cam_yaw)
                        fwd = (math.sin(ry), 0.0, math.cos(ry))
                        right = (math.cos(ry), 0.0, -math.sin(ry))
                        self.axis = vscale(fwd if kind == "fwd" else right, sgn)
                    self.tang0 = tang
                    dot = sum(self.axis[k] * tang[k] for k in range(3))
                    theta = math.acos(clamp(dot, -1.0, 1.0))
                    self.theta = theta
                    # 斜坡预算封顶在实际窗长内（平台按 θ 相关的 share 下界留足）：
                    # 排窗侧给的是谷-谷槽，入坡踩在谷上 ⇒ v4 低、T_r 天然短；
                    # 出坡时长在出坡起点按**当时速度**重算（见 hold 分支）
                    win_len = w.t1 - t
                    f_h = _hold_factor(theta)
                    c_net = max(1.4 / max(0.45, v4), 1.6)
                    t_fit = max(0.5, min((win_len - c_net) / 2.0,
                                         win_len / (2.0 + f_h) - 0.15))
                    self.t_r = min(_t_ramp4(max(theta, 0.2), v4), t_fit)
                    self.hold_min = max(f_h * self.t_r, c_net)
                    cap = self.caps.get(w.cls)
                    self.a_target = CAM4_A_TARGET_TANG if cap is None else cap
                    self.no_early = theta > 2.4
                    self.phase = "ramp_in"
                    self.t_phase = t
                    self.pivot = 1.0
                    self.a_accum = 0.0
                    w.dbg = {"v_entry": round(v4, 3),
                             "dot_entry": round(dot, 3),
                             "sweep_deg": round(math.degrees(theta), 1),
                             "t_r": round(self.t_r, 2),
                             "hold_min": round(self.hold_min, 2)}
                    break
            else:
                return None
        w = self.active
        dt = 1.0 / self.fps
        if self.phase == "ramp_in":
            s = smoothstep5((t - self.t_phase) / self.t_r)
            d = _slerp_dir(self.tang0, self.axis, s, +1.0)
            if t - self.t_phase >= self.t_r:
                self.phase = "hold"
                self.t_phase = t
        elif self.phase == "hold":
            d = self.axis
            hold_len = t - self.t_phase
            t_left = w.t1 - t
            # 出坡时长按**当下速度**重算：谷-谷槽的窗尾踩在下一个谷上，正常
            # 出坡 v 低 ⇒ T_r 短；轴向达标提前收坡发生在鼓包上（v 高），此时
            # 必须给足 T_r，否则 jerk=v·θ·5.77/T_r² 顶穿预算（实测 29-45）
            t_r_out = min(_t_ramp4(max(self.theta, 0.2), v4),
                          max(0.5, t_left))
            # 反平行窗不做轴向达标提前收坡：平台必须跨完鼓包，
            # 让下坡落回下一个速度谷（提前收坡=下坡踩在鼓包上，share 直接塌）
            done = (hold_len >= self.hold_min
                    and self.a_accum >= self.a_target
                    and not self.no_early)
            if done or t_left <= t_r_out:
                self.t_r = t_r_out
                self.phase = "ramp_out"
                self.t_phase = t
        else:                       # ramp_out
            s = smoothstep5((t - self.t_phase) / self.t_r)
            d = _slerp_dir(self.axis, tang, s, -1.0)
            if t - self.t_phase >= self.t_r:
                w.t1_actual = t + dt
                if w.dbg is not None:
                    w.dbg["a_accum"] = round(self.a_accum, 3)
                self.absorb_now = True
                self.active = None
                self.phase = "idle"
                return d
        self.a_accum += v4 * dt * sum(d[k] * self.axis[k] for k in range(3))
        return d


# ================================================================ 诚实性自检

def selfcheck(cam_positions: Sequence[Vec3], cam_quats: Sequence,
              windows: List[PinkWindow], fps: float) -> List[Dict]:
    """按两侧契约逐窗复核（输入=最终相机轨迹，与 QC 同源）：
    窗起帧相机 yaw 定基，声称轴向净位移 ≥ 0.8m 且 ≥ 窗内路径长的 50%。"""
    from .mathutil import quat_forward
    n = len(cam_positions)
    rows: List[Dict] = []
    for w in sorted(windows, key=lambda x: x.t0):
        t1 = w.t1_actual if w.t1_actual is not None else w.t1
        i0 = min(n - 1, int(round(w.t0 * fps)))
        i1 = min(n - 1, int(round(t1 * fps)))
        f = quat_forward(cam_quats[i0])
        yaw = math.degrees(math.atan2(f[0], f[2]))
        ry = math.radians(yaw)
        kind, sgn = _AXIS_OF[w.cls]
        if kind == "y":
            axis = (0.0, sgn, 0.0)
        elif kind == "fwd":
            axis = (sgn * math.sin(ry), 0.0, sgn * math.cos(ry))
        else:
            axis = (sgn * math.cos(ry), 0.0, -sgn * math.sin(ry))
        net_v = vsub(cam_positions[i1], cam_positions[i0])
        net_ax = sum(net_v[k] * axis[k] for k in range(3))
        path = 0.0
        for k in range(i0, i1):
            path += vdist(cam_positions[k + 1], cam_positions[k])
        share = net_ax / path if path > 1e-9 else 0.0
        ok = (net_ax >= NET_MIN_M) and (net_ax >= SHARE_MIN * path)
        row = {"cls": w.cls, "t0": round(w.t0, 3), "t1": round(t1, 3),
               "basis_yaw_deg": round(yaw, 2),
               "net_axial_m": round(net_ax, 3),
               "path_m": round(path, 3),
               "share": round(share, 4), "ok": ok}
        if w.basis_yaw is not None:
            row["yaw_pred_err_deg"] = round(wrap180(w.basis_yaw - yaw), 2)
        if w.dbg is not None:
            row["dbg"] = w.dbg
        rows.append(row)
    return rows


def build_moves(windows: List[PinkWindow], rows: List[Dict],
                seg_base: int) -> List[Move]:
    """把窗转成 plan segments 的 Move（family=平移，自动来自 FAMILY_OF）。"""
    out: List[Move] = []
    by_key = {(r["cls"], r["t0"]): r for r in rows}
    for k, w in enumerate(sorted(windows, key=lambda x: x.t0)):
        t1 = w.t1_actual if w.t1_actual is not None else w.t1
        r = by_key.get((w.cls, round(w.t0, 3)), {})
        out.append(Move(cls=w.cls, t0=w.t0, t1=t1, seg=seg_base + k,
                        params={"overlay": "pink",
                                "basis_yaw_deg": r.get("basis_yaw_deg"),
                                "net_axial_m": r.get("net_axial_m"),
                                "path_m": r.get("path_m"),
                                "share": r.get("share")}))
    return out

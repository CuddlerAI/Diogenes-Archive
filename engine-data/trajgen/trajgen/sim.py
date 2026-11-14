"""M3a 排程仿真（docs/71 §10）：在场景占据图上跑 N 条**虚拟计划**（不渲染）。

验收口径（docs/71 §10 表）：**闸门类指标全过的计划成功率 ≥80%**，
并产出失败原因×场景类型交叉表。三类指标一并出：

| 类 | 内容 |
|---|---|
| 闸门类 | 共视时长占比、同步窗占比、四路 v/ω/a/α/j 的 p95 与 max、ρ 禁带驻留、变速次数与 t_sw、最小障碍间隙、FOV |
| 分布类 | 两两锚点夹角直方图、overlap 时间曲线分布（防贴线）、运镜/焦距/画幅实际配额、速度直方图、第四路 DTW/瞬时距离 |
| 产线健康 | 计划成功率、各级重排次数、每条耗时、失败原因×场景类型交叉表 |

M3a 明确**不依赖美术资产**（docs/70 R1 的场景采购是 M2 硬前置，卡的是 M3b 之后），
所以这里跑的是 `scene.synthetic` 的四类合成场景；换成 SceneKit 导出的真实占据图后
同一套指标可直接复跑。
"""

import json
import math
import time
from dataclasses import dataclass, field
from typing import Dict, List, Optional, Sequence, Tuple

from . import covis as covismod
from . import metrics as met
from .cam4 import V4_CAP
from .mathutil import percentile, vdist, vnorm, vsub
from .planner import (DURATION_S, FPS, N_FRAMES, ClipPlan, QuotaTracker,
                      generate, radius_band)
from .primitives import FAMILY_OF
from .scene import SCENE_KINDS, Scene, synthetic

#: 直方图分箱
SPEED_BINS = (0.0, 0.2, 0.5, 1.0, 1.5, 2.0, 3.0, 4.0, 6.0, 1e9)
ANGLE_BINS = (0.0, 15.0, 30.0, 60.0, 90.0, 120.0, 180.0)


def _hist(values: Sequence[float], bins: Sequence[float]) -> Dict[str, int]:
    out: Dict[str, int] = {}
    for i in range(len(bins) - 1):
        lo, hi = bins[i], bins[i + 1]
        label = "%g-%s" % (lo, "inf" if hi > 1e8 else "%g" % hi)
        out[label] = sum(1 for v in values if lo <= v < hi)
    return out


def _fail_bucket(reason: str) -> str:
    """把自检失败原因归并成交叉表用的桶。"""
    if reason.startswith("phys.cam4"):
        return "phys.cam4"
    if reason.startswith("phys"):
        return "phys.cam1-3"
    for key in ("covis", "decouple", "init", "clearance", "cam4", "feasible"):
        if reason.startswith(key):
            return key
    return "other"


def _anchor_angles(plan: ClipPlan, n: int = 13) -> List[float]:
    """前三路两两的锚点夹角（度）——「不太近不太远」的中间带分布指标。"""
    out: List[float] = []
    if len(plan.tracks) < 3:
        return out
    for k in range(n):
        t = DURATION_S * k / (n - 1)
        i = min(int(t * FPS), N_FRAMES - 1)
        c = plan.anchor.at(t)
        rel = []
        for tr in plan.tracks[:3]:
            d = vsub(tr.positions[i], c)
            rel.append((d[0], d[2]))
        for a in range(3):
            for b in range(a + 1, 3):
                na = math.hypot(*rel[a])
                nb = math.hypot(*rel[b])
                if na < 1e-6 or nb < 1e-6:
                    continue
                cs = (rel[a][0] * rel[b][0] + rel[a][1] * rel[b][1]) / (na * nb)
                out.append(math.degrees(math.acos(max(-1.0, min(1.0, cs)))))
    return out


def _cam4_distances(plan: ClipPlan) -> Tuple[float, float]:
    """第四路 vs guide：DTW 近似（逐帧最近点距离均值，应小）与瞬时距离均值（应大）。"""
    if len(plan.tracks) < 4:
        return 0.0, 0.0
    g = plan.tracks[plan.cam4_plan.guide_cam - 1].positions
    p4 = plan.tracks[3].positions
    step = 15
    inst = [vdist(p4[i], g[i]) for i in range(0, len(p4), step)]
    # 路径相似度：第四路每个采样点到 guide 整条路径的最近距离（DTW 的廉价代理）
    gs = g[::step]
    near = [min(vdist(p, q) for q in gs) for p in p4[::step]]
    return (sum(near) / len(near) if near else 0.0,
            sum(inst) / len(inst) if inst else 0.0)


@dataclass
class SimReport:
    n_plans: int = 0
    n_ok: int = 0
    scenes: List[str] = field(default_factory=list)
    gate: Dict = field(default_factory=dict)
    dist: Dict = field(default_factory=dict)
    health: Dict = field(default_factory=dict)

    @property
    def success_rate(self) -> float:
        return self.n_ok / self.n_plans if self.n_plans else 0.0

    def to_dict(self) -> Dict:
        return {
            "m3a": "排程仿真（docs/71 §10）",
            "n_plans": self.n_plans, "n_ok": self.n_ok,
            "success_rate": round(self.success_rate, 4),
            "acceptance": {"target": 0.80, "pass": self.success_rate >= 0.80},
            "scenes": self.scenes,
            "gate_metrics": self.gate,
            "distribution_metrics": self.dist,
            "line_health": self.health,
        }


def run(n_plans: int = 200, scene_kinds: Sequence[str] = tuple(SCENE_KINDS),
        scene_seed: int = 1, seed0: int = 0, covis_hz: float = 2.0,
        progress: bool = False) -> SimReport:
    scenes = [synthetic(k, scene_seed) for k in scene_kinds]
    quota = QuotaTracker()
    rep = SimReport(scenes=[s.name for s in scenes])

    covis_ratios: List[float] = []
    pair_sync: List[float] = []
    all4_sync: List[float] = []
    p95: Dict[str, List[float]] = {k: [] for k in
                                   ("v", "a", "j", "w", "al", "aj")}
    v4_max: List[float] = []
    speeds: List[float] = []
    anchor_angles: List[float] = []
    overlap_curve: List[float] = []
    tsw: List[float] = []
    rho_pairs: List[Tuple[float, float]] = []
    dtw_near: List[float] = []
    dtw_inst: List[float] = []
    clearances: List[float] = []
    switch_counts: List[float] = []
    tsw_err: List[float] = []
    band_dwell: List[float] = []
    rho_valid: List[float] = []
    loop_closure: List[float] = []
    init_ang: List[float] = []
    init_dist: List[float] = []
    durations: List[float] = []
    retries = {"speed_reshape": 0, "resample": 0, "reanchor": 0}
    cross: Dict[str, Dict[str, int]] = {s.kind: {} for s in scenes}
    anchor_reuse: Dict[str, int] = {}

    for i in range(n_plans):
        scene = scenes[i % len(scenes)]
        t0 = time.time()
        plan = generate(scene, seed=seed0 + i, quota=quota, covis_hz=covis_hz)
        durations.append(time.time() - t0)
        rep.n_plans += 1
        for k in retries:
            retries[k] += plan.retries.get(k, 0)

        if not plan.ok:
            for r in plan.reasons or ["other"]:
                b = _fail_bucket(r)
                cross[scene.kind][b] = cross[scene.kind].get(b, 0) + 1
            if progress:
                print("  [%3d/%d] %-12s FAIL %s" % (i + 1, n_plans, scene.kind,
                                                    ",".join(plan.reasons[:2])))
            continue

        rep.n_ok += 1
        covis_ratios.append(plan.covis.ratio)
        overlap_curve.extend(v for _, v in plan.covis.ts)
        pair_sync.append(plan.decouple.worst_pair[1])
        all4_sync.append(plan.decouple.all4_frac)
        for cid, ph in plan.physics.items():
            p95["v"].append(ph.speed_p95)
            p95["a"].append(ph.accel_p95)
            p95["j"].append(ph.jerk_p95)
            p95["w"].append(ph.angspeed_p95)
            p95["al"].append(ph.angaccel_p95)
            p95["aj"].append(ph.angjerk_p95)
            speeds.extend(ph.speed_hist[::30])
            if cid == 4:
                v4_max.append(ph.speed_max)
        tsw.append(plan.cam4_plan.t_sw)
        rho_pairs.append((plan.cam4_plan.rho1, plan.cam4_plan.rho2))
        anchor_angles.extend(_anchor_angles(plan))
        near, inst = _cam4_distances(plan)
        dtw_near.append(near)
        dtw_inst.append(inst)
        clearances.append(plan.min_clearance_m)
        if plan.cam4_check is not None:
            switch_counts.append(float(len(plan.cam4_check.events_s)))
            band_dwell.append(plan.cam4_check.dwell_s)
            rho_valid.append(plan.cam4_check.valid_frac)
            if plan.cam4_check.events_s:
                tsw_err.append(abs(plan.cam4_check.events_s[0]
                                   - (plan.cam4_plan.t_sw + plan.cam4_plan.delta / 2)))
        loop_closure.append(plan.loop_closure_m)
        init_ang.append(plan.init_angle)
        init_dist.append(plan.init_dist)
        a0 = plan.anchor.at(0.0)
        key = "%.0f,%.0f" % (a0[0] / 5.0, a0[2] / 5.0)
        anchor_reuse[key] = anchor_reuse.get(key, 0) + 1
        if progress:
            print("  [%3d/%d] %-12s OK covis=%.2f" % (i + 1, n_plans, scene.kind,
                                                      plan.covis.ratio))

    def stat(xs: Sequence[float]) -> Dict:
        if not xs:
            return {}
        return {"min": round(min(xs), 4), "p50": round(percentile(xs, 50), 4),
                "p95": round(percentile(xs, 95), 4), "max": round(max(xs), 4)}

    rep.gate = {
        "covis_ratio": dict(stat(covis_ratios), gate=">=%.2f" % covismod.COVIS_RATIO_MIN,
                            target=covismod.COVIS_RATIO_TARGET),
        "pair_sync_worst": dict(stat(pair_sync), gate="<0.50"),
        "all4_sync": dict(stat(all4_sync), gate="<0.20"),
        "speed_p95": dict(stat(p95["v"]), gate="<=6"),
        "accel_p95": dict(stat(p95["a"]), gate="<=8"),
        "jerk_p95": dict(stat(p95["j"]), gate="<=20"),
        "angspeed_p95": dict(stat(p95["w"]), gate="<=90"),
        "angaccel_p95": dict(stat(p95["al"]), gate="<=150"),
        "angjerk_p95": dict(stat(p95["aj"]), gate="<=600"),
        "cam4_speed_max": dict(stat(v4_max), gate="<=%g（瞬时硬帽）" % V4_CAP),
        "cam4_t_sw": dict(stat(tsw), gate="[20,40]s"),
        "cam4_switch_count": dict(stat(switch_counts), gate="==1（QC 同款反算）"),
        "cam4_tsw_err_s": dict(stat(tsw_err), gate="<=2.0"),
        "cam4_band_dwell_s": dict(stat(band_dwell), gate="<=1.0（扣一次单调穿越）"),
        "cam4_rho_valid_frac": dict(stat(rho_valid), gate=">=0.5"),
        "min_clearance_m": dict(stat(clearances), gate=">0"),
        "init_angle_deg": dict(stat(init_ang), gate="<=15（内控）"),
        "init_dist_m": dict(stat(init_dist), gate="<=30（内控）"),
        "fov_const": {"by_construction": True,
                      "note": "内参逐帧由 plan 的焦距/画幅算，全程不变（8.1 红线）"},
    }
    rep.dist = {
        "anchor_angle_hist_deg": _hist(anchor_angles, ANGLE_BINS),
        "overlap_curve": dict(stat(overlap_curve),
                              near_gate_frac=round(
                                  sum(1 for x in overlap_curve
                                      if 0.25 <= x < 0.35) / max(1, len(overlap_curve)), 4)),
        "speed_hist_mps": _hist(speeds, SPEED_BINS),
        "quota": quota.report(),
        "cam4_rho_pairs": [[round(a, 3), round(b, 3)] for a, b in rho_pairs[:20]],
        "cam4_path_dist_m": {"to_guide_path_mean": stat(dtw_near),
                             "instantaneous_mean": stat(dtw_inst),
                             "expect": "前者小（路径相关）、后者大（瞬时拉开）"},
        "loop_closure_m": stat(loop_closure),
    }
    rep.health = {
        "success_rate": round(rep.success_rate, 4),
        "per_plan_seconds": stat(durations),
        "retries_total": retries,
        "retries_per_plan": {k: round(v / max(1, rep.n_plans), 3)
                             for k, v in retries.items()},
        "fail_reason_by_scene": cross,
        "anchor_reuse_top": sorted(anchor_reuse.values(), reverse=True)[:5],
    }
    return rep


def render_text(rep: SimReport) -> str:
    lines = ["M3a 排程仿真（docs/71 §10）",
             "  场景池：%s" % ", ".join(rep.scenes),
             "  计划数 %d，闸门全过 %d，成功率 %.1f%%（验收线 80%%）%s"
             % (rep.n_plans, rep.n_ok, rep.success_rate * 100,
                " ✅" if rep.success_rate >= 0.80 else " ❌"),
             "",
             "闸门类指标（p50 / p95 / max ← 闸门）："]
    for k, v in rep.gate.items():
        if "p50" in v:
            lines.append("  %-18s %8.3f /%8.3f /%8.3f   ← %s"
                         % (k, v["p50"], v["p95"], v["max"], v.get("gate", "-")))
        else:
            lines.append("  %-18s %s" % (k, v.get("note", v)))
    lines += ["", "分布类："]
    lines.append("  锚点夹角直方图 %s" % rep.dist["anchor_angle_hist_deg"])
    lines.append("  速度直方图(m/s) %s" % rep.dist["speed_hist_mps"])
    lines.append("  overlap 贴线比例(0.25~0.35) %s"
                 % rep.dist["overlap_curve"].get("near_gate_frac"))
    q = rep.dist["quota"]
    lines.append("  焦距占比 %s" % q["focal_share"])
    lines.append("  画幅占比 %s（目标 %s）" % (q["aspect_share"], q["aspect_target"]))
    lines.append("  运镜类目占比 %s" % q["cls_share"])
    lines += ["", "产线健康类："]
    lines.append("  每条耗时 %s" % rep.health["per_plan_seconds"])
    lines.append("  重排次数/条 %s" % rep.health["retries_per_plan"])
    lines.append("  失败原因×场景类型：")
    for scene, buckets in rep.health["fail_reason_by_scene"].items():
        lines.append("    %-13s %s" % (scene, buckets or "（无失败）"))
    return "\n".join(lines)

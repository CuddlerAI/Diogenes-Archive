"""几何面：用相机内外参做视锥代理，量化「动态主体可见 ≥80%」「共视 ≥50%」「相机互不可见」「相机高度」「主体占比」。

需求出处：新需求 §3.3「动态主体可见」、§3.4「共视」、§3.3「各相机模型互不可见」、§6.1「主体占比过小」；
09-22 会议「多人场景焦点取所有参与者可覆盖的中心点」。

**这是几何代理，不是画面判定**：没有遮挡、没有 NPC 位置。
- 主体 1「舞台中心」：A/C/D 程序相机（固定/环绕/横移）的注视点 = 相机 − Follow Offset，逐帧恒定；
  6 名剧本人物围绕它活动。它在视锥内 ≈ 人群区域在画面里。
- 主体 2「受控人物」：C/D 的 player_position（追背/第一人称路带，同一遍共用）。
- 主体 3「该路焦点」：自由运镜路逐帧盯的点 = 相机 − Follow Offset（交付 README：相机 − 该帧它看的焦点）。需方 2026-10-09
  要求 C/D 的自由运镜也看人群（事件）而不只围着受控人物；固定机位的注视点按受控人物的走位找、可能不在人群中心
  （beach-demo-003 D 的固定机位对着舞台前 6.6 m 散步的第 7 人），只用它判自由运镜会误判。
- 第一人称路的相机在人物头上，人物本身不可能入镜，其「动态主体」是画面里的其他人 → 只用舞台中心做代理并标 WARN。
坐标：docs/69 y-up 左手系；相机本体系 R/U/F = q 旋转 (1,0,0)/(0,1,0)/(0,0,1)，像面 x 向右 y 向上。
"""

from __future__ import annotations

from typing import Dict, List, Optional, Sequence, Tuple

from engine_qc.mathutil import percentile, vnorm, vsub

from .checks_json import LaneData
from .checks_motion import _dot, basis
from .loaders import Group
from .model import FAIL, MANUAL, PASS, PERSON_ROLES, SKIP, WARN, RuleResult

Vec3 = Tuple[float, float, float]

SUBJECT_VISIBLE_MIN = 0.80
COVIS_MIN = 0.50
PERSON_HEIGHT_M = 1.8


def in_frustum(cam_pos: Sequence[float], q: Sequence[float], intr: dict, dims: Tuple[int, int],
               point: Sequence[float], margin_m: float = 0.0) -> bool:
    """点（可带半径 margin，按角半径放宽）是否在视锥内。intr: fx/fy/cx/cy；dims: (W, H)。"""
    d = vsub(point, cam_pos)
    R, U, F = basis(q)
    zc = _dot(d, F)
    if zc <= 0.05:
        return False
    xc, yc = _dot(d, R), _dot(d, U)
    fx, fy = float(intr["fx"]), float(intr["fy"])
    cx, cy = float(intr.get("cx", dims[0] / 2)), float(intr.get("cy", dims[1] / 2))
    u = cx + fx * xc / zc
    v = cy + fy * yc / zc
    pad_u = fx * margin_m / zc
    pad_v = fy * margin_m / zc
    return (-pad_u <= u <= dims[0] + pad_u) and (-pad_v <= v <= dims[1] + pad_v)


def stage_center(g: Group, lanes: Dict[str, LaneData], worldline: str) -> Optional[Vec3]:
    """程序相机注视点（相机 − Follow Offset）在首帧的均值。"""
    pts = []
    for lf in g.lanes_of(worldline):
        ld = lanes.get(lf.info.key)
        if not ld or not ld.records or lf.info.role not in ("fixed", "orbit", "dolly", "free"):
            continue
        r = ld.records[0]
        c, fo = r.get("camera_position"), r.get("camera_follow_offset")
        if isinstance(c, list) and isinstance(fo, list) and len(c) == 3 and len(fo) == 3:
            pts.append(tuple(float(c[i]) - float(fo[i]) for i in range(3)))
    if not pts:
        return None
    return tuple(sum(p[i] for p in pts) / len(pts) for i in range(3))


def _dims_for(ld: LaneData, video_dims: Dict[str, Tuple[int, int]]) -> Tuple[int, int]:
    d = video_dims.get(ld.key)
    if d:
        return d
    intr = ld.intrinsics() or {}
    return (int(round(2 * float(intr.get("cx", 960)))), int(round(2 * float(intr.get("cy", 540)))))


def _visibility(ld: LaneData, dims: Tuple[int, int], targets: Sequence[Sequence[float]], margin_m: float) -> List[bool]:
    pos, qs = ld.positions(), ld.quats()
    out = []
    for i, (c, q, r) in enumerate(zip(pos, qs, ld.records)):
        t = targets[i] if i < len(targets) else targets[-1]
        out.append(in_frustum(c, q, r["camera_intrinsics"], dims, t, margin_m))
    return out


def check_geometry(g: Group, lanes: Dict[str, LaneData], video_dims: Dict[str, Tuple[int, int]]) -> List[RuleResult]:
    out: List[RuleResult] = []
    for wl in "ABCD":
        wl_lanes = [lanes[lf.info.key] for lf in g.lanes_of(wl) if lanes.get(lf.info.key) and lanes[lf.info.key].records
                    and lanes[lf.info.key].positions() and lanes[lf.info.key].quats()]
        tgt = f"世界线{wl}"
        if wl == "B":
            for ld in wl_lanes:
                out.append(RuleResult("geo.subject_visible", SKIP, ld.key, "B 不适用主体/共视/双路独立要求（需求§4.2）", "需求§4.2"))
                out.extend(_camera_height(ld, stage_center(g, lanes, "A")))
            out.append(RuleResult("geo.covis", SKIP, tgt, "B 只有 1 路，不适用共视", "需求§3.4/§4.2"))
            continue
        if not wl_lanes:
            out.append(RuleResult("geo.covis", SKIP, tgt, "无可用路", "需求§3.4"))
            continue
        center = stage_center(g, lanes, wl)
        person = None
        for ld in wl_lanes:
            if ld.role in PERSON_ROLES:
                person = ld.player_positions()
                if person:
                    break
        # 逐路主体可见
        vis_by_lane: Dict[str, Dict[str, List[bool]]] = {}
        for ld in wl_lanes:
            dims = _dims_for(ld, video_dims)
            n = ld.n
            vis: Dict[str, List[bool]] = {}
            if center is not None:
                vis["舞台中心"] = _visibility(ld, dims, [center] * n, margin_m=0.0)
            if person is not None and ld.role != "first_person":
                vis["受控人物"] = _visibility(ld, dims, person, margin_m=0.3)
            if ld.role in ("orbit", "dolly", "free"):
                focus = [tuple(c[i] - r["camera_follow_offset"][i] for i in range(3)) for c, r in zip(ld.positions(), ld.records)]
                vis["该路焦点"] = _visibility(ld, dims, focus, margin_m=0.0)
            vis_by_lane[ld.key] = vis
            if not vis:
                out.append(RuleResult("geo.subject_visible", SKIP, ld.key, "无可用主体代理（无注视点、无人物位置）", "需求§3.3"))
                continue
            fr = {k: sum(v) / len(v) for k, v in vis.items()}
            union = sum(1 for i in range(n) if any(v[i] for v in vis.values())) / n
            detail = "、".join(f"{k} {v * 100:.0f}%" for k, v in fr.items())
            if ld.role == "first_person":
                st = WARN
                txt = (f"第一人称路：人物本身不入镜，代理为舞台中心（其他人物）在视锥内 {union * 100:.0f}%；"
                       "动态主体口径（画面里其他人是否算主体）待需方确认，须人工看画面")
            elif ld.role == "chase":
                st = PASS if fr.get("受控人物", 0) >= SUBJECT_VISIBLE_MIN else FAIL
                txt = f"受控人物在视锥内 {fr.get('受控人物', 0) * 100:.0f}%（几何代理，无遮挡；要求 ≥80%）；{detail}"
            else:
                st = PASS if union >= SUBJECT_VISIBLE_MIN else FAIL
                txt = f"主体（舞台中心、受控人物或该路焦点）在视锥内 {union * 100:.0f}%（几何代理，无遮挡；要求 ≥80% ≈ 96 s/120 s）；{detail}"
            out.append(RuleResult("geo.subject_visible", st, ld.key, txt, "需求§3.3「动态主体可见 ≥80% 时长」",
                                  data={"visible": {k: [1 if x else 0 for x in v] for k, v in vis.items()}, "frac": fr, "union": union}))
            # 主体占比代理：1.8 m 人物的像高占画面高度
            if person is not None and ld.role in ("fixed", "orbit", "dolly", "free", "chase"):
                pos = ld.positions()
                intr = ld.intrinsics() or {}
                fy = float(intr.get("fy", 1))
                H = dims[1]
                sizes = []
                for c, p in zip(pos, person):
                    dist = vnorm(vsub(p, c))
                    if dist > 0.1:
                        sizes.append(min(1.0, fy * PERSON_HEIGHT_M / dist / H))
                if sizes:
                    p50 = percentile(sizes, 50)
                    out.append(RuleResult("geo.subject_size", MANUAL, ld.key,
                                          f"受控人物（按 1.8 m 高）像高占画面高度 p50 {p50 * 100:.0f}%、p10 {percentile(sizes, 10) * 100:.0f}%，"
                                          f"相机距人物 p50 {percentile([vnorm(vsub(p, c)) for c, p in zip(pos, person)], 50):.1f} m"
                                          "（需方 demo4 反馈「主体物占比太小」，无量化阈值，须人工判）",
                                          "需求§6.1 / demo 跟进表 demo4-#4/#7", data={"size_p50": p50}))
            out.extend(_camera_height(ld, center))
        # 共视：同一逻辑帧 ≥2 路看到同一主体
        n = min(ld.n for ld in wl_lanes)
        subjects = set()
        for v in vis_by_lane.values():
            subjects |= set(v)
        if len(wl_lanes) < 2 or not subjects:
            out.append(RuleResult("geo.covis", SKIP, tgt, "可比路数不足或无主体代理", "需求§3.4"))
        else:
            any2, allk = 0, 0
            timeline = []
            for i in range(n):
                best = 0
                for s in subjects:
                    cnt = sum(1 for v in vis_by_lane.values() if s in v and v[s][i])
                    best = max(best, cnt)
                timeline.append(best)
                if best >= 2:
                    any2 += 1
                if best >= len(wl_lanes):
                    allk += 1
            f2, fall = any2 / n, allk / n
            # 逐对
            keys = list(vis_by_lane)
            pairs = {}
            for a in range(len(keys)):
                for b in range(a + 1, len(keys)):
                    va, vb = vis_by_lane[keys[a]], vis_by_lane[keys[b]]
                    both = sum(1 for i in range(n) if any(s in va and s in vb and va[s][i] and vb[s][i] for s in subjects))
                    pairs[f"{keys[a]}↔{keys[b]}"] = both / n
            st = PASS if f2 >= COVIS_MIN else FAIL
            out.append(RuleResult("geo.covis", st, tgt,
                                  f"≥2 路同时看到同一主体 {f2 * 100:.0f}%（要求 ≥50% ≈ 60 s）；全部 {len(wl_lanes)} 路同时 {fall * 100:.0f}%；"
                                  f"逐对：{'，'.join(f'{k} {v * 100:.0f}%' for k, v in pairs.items())}"
                                  "（几何代理；参与相机范围与多人主体口径待需方确认，固定机位/第一人称未排除）",
                                  "需求§3.4「共视 ≥50%」", data={"timeline": timeline, "any2": f2, "all": fall, "pairs": pairs}))
        # 相机互见（几何）
        if len(wl_lanes) >= 2:
            seen = {}
            for ld in wl_lanes:
                dims = _dims_for(ld, video_dims)
                for other in wl_lanes:
                    if other.key == ld.key:
                        continue
                    op = other.positions()
                    v = _visibility(ld, dims, op[:ld.n], margin_m=0.2)
                    f = sum(v) / len(v)
                    if f > 0:
                        seen[f"{ld.key} 视锥内含 {other.key}"] = f
            txt = ("、".join(f"{k} {v * 100:.0f}%" for k, v in seen.items()) if seen else "任意两路相机位置都不落入对方视锥")
            out.append(RuleResult("geo.camera_mutual", MANUAL, tgt,
                                  f"相机位置落入其他路视锥的比例：{txt}。游戏相机无实体模型，几何落入不等于画面可见；须人工确认画面里无相机模型",
                                  "需求§3.3「各相机模型互不可见」", data={"mutual": seen}))
    return out


def _camera_height(ld: LaneData, center: Optional[Vec3]) -> List[RuleResult]:
    pos = ld.positions()
    if not pos or center is None:
        return []
    ys = [p[1] for p in pos]
    lo = min(ys) - center[1]
    if lo < -0.5:
        return [RuleResult("geo.camera_height", WARN, ld.key,
                           f"相机最低点比舞台中心低 {-lo:.2f} m（疑似钻地/穿模，须看画面）", "需求§3.3 不得穿模")]
    return [RuleResult("geo.camera_height", PASS, ld.key,
                       f"相机高度相对舞台中心 {lo:+.2f} … {max(ys) - center[1]:+.2f} m", "需求§3.3")]

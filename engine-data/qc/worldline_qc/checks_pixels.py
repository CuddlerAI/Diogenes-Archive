"""像素面（需 opencv + numpy；缺失时全部 UNVERIFIED，不当作通过）。

- 运动像素三口径（逐帧 / 1 s 窗并集 / 全片并集）——需方「≥50% 像素处于运动状态」的判定法未定，只报 WARN。
- 亮度异常：黑/白帧、单帧明暗突变（闪屏/闪白代理，09-22 会议与 demo 跟进表 demo4-#5 点名沙滩区域百叶窗闪屏）。
- 跳剪代理：相邻帧变化像素 >90%。
- 冻结帧：连续逐位相同帧 ≥0.5 s（掉帧/补帧代理，09-21 会议「不得掉帧、不得后处理补帧」）。
- 跨遍固定机位比对：仅在完整内外参及交付逻辑帧对应可核实时逐秒比较；不可比时三项均 UNVERIFIED。
  接管前应 ≈ 基线噪声（确认点①：分叉前必须一致），
  接管后差异应局部化且起点不早于接管（§4.3「差异须能对应具体控制行为」）。
- 每路缩略图（base64 JPEG）嵌进 HTML 供人工复核 UI/水印/指针/穿模/闪屏。

分析分辨率 320×180 灰度（与原型 proto_video.py 一致）；阈值 |Δ|>12 为「运动」、>20 为「跨遍变化」。
"""

from __future__ import annotations

import base64
import math
from typing import Dict, List, Optional, Tuple

from .checks_json import LaneData
from .loaders import Group
from .model import FAIL, MANUAL, PASS, SKIP, UNVERIFIED, WARN, RuleResult

W, H = 320, 180
MOTION_T = 12
XTAKE_T = 20
JUMP_CUT = 0.90
FROZEN_MIN_FRAMES = 15
LUM_BLACK, LUM_WHITE, LUM_JUMP = 8.0, 247.0, 40.0
XTAKE_PRE_OK, XTAKE_PRE_WARN = 0.08, 0.15
XTAKE_POST_P95_MAX = 0.35
PIX_RULES = ("pix.motion_ratio", "pix.luminance", "pix.jump_cut", "pix.frozen", "pix.thumbs")
XTAKE_RULES = ("pix.xtake_pre", "pix.xtake_post", "pix.xtake_onset")


def _xtake_unverified(target: str, reason: str) -> List[RuleResult]:
    return [RuleResult(rule, UNVERIFIED, target,
                       f"跨遍像素比对未核验：{reason}；不能用图像差判断世界线一致性或干预因果",
                       "需求§4.3 / R29–R30") for rule in XTAKE_RULES]


def _finite_vector(value, size: int) -> Optional[tuple]:
    if not isinstance(value, (list, tuple)) or len(value) != size:
        return None
    if not all(type(x) in (int, float) for x in value):
        return None
    try:
        result = tuple(float(x) for x in value)
    except OverflowError:
        return None
    return result if all(math.isfinite(x) for x in result) else None


def _xtake_comparability(ref: Optional[LaneData], other: Optional[LaneData],
                        ref_frames: int, other_frames: int,
                        summaries: Optional[Dict[str, dict]]) -> Optional[str]:
    """None means comparable under the delivery frame contract, not proven world sync.

    frame 1 is the common logical origin, followed by the 30 Hz output grid.
    Independent takes have different real wall clocks; JSON fps is game fps.
    Compare every recorded projection exactly (apart from q/-q equivalence),
    without inventing a pose tolerance or treating a shared prefix as coverage.
    Probe rates and frame counts do not independently verify every encoded PTS.
    """
    if ref is None or other is None or not ref.records or not other.records:
        return "缺少完整 action_camera 记录"
    if (type(ref_frames) is not int or ref_frames <= 0 or type(other_frames) is not int
            or ref_frames != other_frames or ref.n != ref_frames or other.n != ref_frames):
        return "两路 JSON 与完整解码帧数不一致或为空，逻辑帧无法一一对应"
    dims = []
    for ld in (ref, other):
        summary = (summaries or {}).get(ld.key)
        if not isinstance(summary, dict):
            return f"{ld.key} 缺少视频探测信息，无法确认画幅和逻辑帧速率"
        wh = (summary.get("width"), summary.get("height"))
        if not all(type(x) is int and x > 0 for x in wh):
            return f"{ld.key} 缺少有效原视频宽高"
        dims.append(wh)
        if type(summary.get("nb_frames")) is not int or summary["nb_frames"] != ref_frames:
            return f"{ld.key} 视频探测帧数与 JSON/解码不一致"
        if any(type(summary.get(k)) not in (int, float) or summary[k] != 30
               for k in ("r_frame_rate", "avg_frame_rate")):
            return f"{ld.key} 未能确认交付 30 Hz 帧网格"
    if dims[0] != dims[1]:
        return "原视频画幅不同，同一像素不能直接对应"
    for i, pair in enumerate(zip(ref.records, other.records), 1):
        projections = []
        for ld, row in zip((ref, other), pair):
            if not isinstance(row, dict) or type(row.get("frame")) is not int or row["frame"] != i:
                return f"{ld.key} 第 {i} 条缺少连续 1…N 逻辑帧号"
            intr = row.get("camera_intrinsics")
            k = _finite_vector([intr.get(x) for x in ("fx", "fy", "cx", "cy")], 4) if isinstance(intr, dict) else None
            if k is None or k[0] <= 0 or k[1] <= 0:
                return f"{ld.key} 第 {i} 帧缺少有效完整内参"
            pos = _finite_vector(row.get("camera_position"), 3)
            q = _finite_vector(row.get("camera_rotation_quaternion"), 4)
            if pos is None or q is None:
                return f"{ld.key} 第 {i} 帧缺少有效完整外参"
            norm = math.hypot(*q)
            if not math.isfinite(norm) or norm == 0:
                return f"{ld.key} 第 {i} 帧四元数无效"
            q = tuple(x / norm for x in q)
            if next(x for x in q if x != 0) < 0:
                q = tuple(-x for x in q)
            scale = _finite_vector([row.get("metric_scale")], 1)
            if scale is None or scale[0] <= 0:
                return f"{ld.key} 第 {i} 帧缺少有效坐标尺度"
            projections.append((k, pos, q, scale))
        if projections[0] != projections[1]:
            return f"第 {i} 帧内参、相机位置/朝向或坐标尺度不同，视角差不能解释为世界线分叉"
    return None


def _try_import():
    try:
        import cv2  # noqa: F401
        import numpy as np  # noqa: F401
        return cv2, np
    except (ImportError, OSError, RuntimeError):  # 未安装 / 二进制不兼容；CI 只认带 ImportError 的可选导入
        return None, None


def _decode(cv2, np, path, n_thumbs: int):
    """返回 (gray uint8 [N,H,W], mean_lum [N], thumbs [b64...])。"""
    cap = cv2.VideoCapture(str(path))
    total = int(cap.get(cv2.CAP_PROP_FRAME_COUNT) or 0)
    want = set()
    if n_thumbs > 0 and total > 0:
        want = {int(round(k * (total - 1) / max(1, n_thumbs - 1))) for k in range(n_thumbs)}
    grays, thumbs = [], []
    i = 0
    while True:
        ok, f = cap.read()
        if not ok:
            break
        small = cv2.resize(f, (W, H), interpolation=cv2.INTER_AREA)
        if i in want:
            ok2, buf = cv2.imencode(".jpg", small, [int(cv2.IMWRITE_JPEG_QUALITY), 65])
            if ok2:
                thumbs.append((i, base64.b64encode(buf.tobytes()).decode("ascii")))
        grays.append(cv2.cvtColor(small, cv2.COLOR_BGR2GRAY))
        i += 1
    cap.release()
    if not grays:
        return None, None, []
    g = np.stack(grays)
    return g, g.reshape(len(g), -1).mean(1), thumbs


def _runs(flags: List[bool]) -> List[Tuple[int, int]]:
    out, s = [], None
    for i, f in enumerate(flags):
        if f and s is None:
            s = i
        elif not f and s is not None:
            out.append((s, i - 1))
            s = None
    if s is not None:
        out.append((s, len(flags) - 1))
    return out


def _fmt_t(frames, fps=30.0, limit=5) -> str:
    return "、".join(f"{f / fps:.1f}s" for f in frames[:limit]) + ("…" if len(frames) > limit else "")


def analyze_pixels(g: Group, lanes: Dict[str, LaneData], takeover_s: Dict[str, float], preroll_s: float = 0.0,
                   n_thumbs: int = 6, enabled: bool = True,
                   video_summaries: Optional[Dict[str, dict]] = None) -> Tuple[List[RuleResult], dict]:
    """返回 (规则结果, 画图/缩略图产物)。takeover_s: {worldline: 接管视频秒}。"""
    out: List[RuleResult] = []
    art: dict = {"thumbs": {}, "motion_1s": {}, "lum_1s": {}, "xtake": {}}
    if not enabled:
        for lf in g.lanes:
            for r in PIX_RULES:
                out.append(RuleResult(r, UNVERIFIED, lf.info.key, "像素面被 --no-pixels 关闭，未核验", "需求§3.3"))
        out.extend(_xtake_unverified("group", "像素面被关闭"))
        return out, art
    cv2, np = _try_import()
    if cv2 is None:
        for lf in g.lanes:
            for r in PIX_RULES:
                out.append(RuleResult(r, UNVERIFIED, lf.info.key, "本机无 opencv/numpy，像素面未核验（不得计通过）", "需求§3.3"))
        out.extend(_xtake_unverified("group", "本机无 opencv/numpy"))
        return out, art

    fixed_gray: Dict[str, object] = {}
    pre_frames = int(round(preroll_s * 30))
    for lf in g.lanes:
        key, role = lf.info.key, lf.info.role
        if lf.video is None or not lf.video.is_file():
            for r in PIX_RULES:
                out.append(RuleResult(r, SKIP, key, "视频缺失", "需求§3.3"))
            continue
        gray, lum, thumbs = _decode(cv2, np, lf.video, n_thumbs)
        if gray is None:
            for r in PIX_RULES:
                out.append(RuleResult(r, UNVERIFIED, key, "opencv 无法解码该视频", "需求§3.3"))
            continue
        n = len(gray)
        d = np.abs(gray[1:].astype(np.int16) - gray[:-1].astype(np.int16))
        moving = d > MOTION_T                                   # [N-1,H,W]
        per_frame = moving.reshape(n - 1, -1).mean(1)           # 逐帧口径
        nwin = (n - 1) // 30
        win_union = [float(moving[k * 30:(k + 1) * 30].any(0).mean()) for k in range(nwin)]
        clip_union = float(moving.any(0).mean())
        art["motion_1s"][key] = [float(x) for x in per_frame[: nwin * 30].reshape(nwin, 30).mean(1)] if nwin else []
        art["lum_1s"][key] = [float(x) for x in lum[: (n // 30) * 30].reshape(n // 30, 30).mean(1)] if n >= 30 else []
        art["thumbs"][key] = thumbs
        mean_pf = float(per_frame.mean())
        over = float((per_frame > 0.5).mean())
        out.append(RuleResult("pix.motion_ratio", WARN, key,
                              f"运动像素占比：逐帧均值 {mean_pf * 100:.1f}%（>50% 的帧占 {over * 100:.0f}%）、"
                              f"1 s 窗并集均值 {100 * (sum(win_union) / len(win_union) if win_union else 0):.1f}%、全片并集 {clip_union * 100:.0f}%。"
                              "需求「≥50% 像素处于运动状态」的判定方法、时间口径、相机运动引起的背景运动是否计入均待需方确认——三口径都列出，暂不判定",
                              "需求§3.3「运动像素 ≥50%」", data={"per_frame_mean": mean_pf, "win_union_mean": (sum(win_union) / len(win_union) if win_union else 0),
                                                            "clip_union": clip_union, "frac_over_50": over}))
        # 亮度
        black = [i for i, x in enumerate(lum) if x < LUM_BLACK]
        white = [i for i, x in enumerate(lum) if x > LUM_WHITE]
        jumps = [i + 1 for i in range(n - 1) if abs(float(lum[i + 1]) - float(lum[i])) > LUM_JUMP]
        if black or white:
            out.append(RuleResult("pix.luminance", FAIL, key,
                                  f"黑帧 {len(black)}（{_fmt_t(black)}）、白帧 {len(white)}（{_fmt_t(white)}）", "需求§3.3/§6.3 黑白屏/闪白"))
        elif jumps:
            out.append(RuleResult("pix.luminance", WARN, key,
                                  f"单帧平均亮度突变 >{LUM_JUMP:g} 共 {len(jumps)} 处（{_fmt_t(jumps)}）：疑似闪屏/明暗跳变，须看画面", "需求§3.3/§6.3"))
        else:
            out.append(RuleResult("pix.luminance", PASS, key,
                                  f"无黑/白帧，无单帧亮度突变（均值 {float(lum.mean()):.0f}，范围 {float(lum.min()):.0f}–{float(lum.max()):.0f}）",
                                  "需求§3.3/§6.3"))
        # 跳剪
        cuts = [i + 1 for i, x in enumerate(per_frame) if x > JUMP_CUT]
        if cuts:
            st = FAIL if role == "fixed" else WARN
            out.append(RuleResult("pix.jump_cut", st, key,
                                  f"相邻帧变化像素 >{JUMP_CUT * 100:.0f}% 共 {len(cuts)} 处（{_fmt_t(cuts)}）：跳剪/场景突变代理"
                                  + ("" if st == FAIL else "（运动相机可能是快速甩动，须看画面）"), "需求§3.3 跳剪/场景突变"))
        else:
            out.append(RuleResult("pix.jump_cut", PASS, key, f"无相邻帧变化 >{JUMP_CUT * 100:.0f}% 的跳变（最大 {float(per_frame.max()) * 100:.0f}%）", "需求§3.3"))
        # 冻结帧（排除开头静止段）
        same = [bool(x == 0) for x in per_frame]
        runs = [(s, e) for s, e in _runs(same) if e - s + 1 >= FROZEN_MIN_FRAMES and e >= pre_frames]
        if runs:
            out.append(RuleResult("pix.frozen", WARN, key,
                                  f"连续逐位相同帧 ≥{FROZEN_MIN_FRAMES / 30:.1f} s 共 {len(runs)} 段（{'、'.join(f'{s / 30:.1f}–{(e + 1) / 30:.1f}s' for s, e in runs[:5])}）"
                                  "：疑似掉帧/补帧或画面冻结（固定机位且场上无人动也会触发，须看画面）", "09-21 会议「不得掉帧/补帧」/ 需求§6.2"))
        else:
            out.append(RuleResult("pix.frozen", PASS, key, "无 ≥0.5 s 的冻结帧段", "09-21 会议 / 需求§6.2"))
        out.append(RuleResult("pix.thumbs", MANUAL, key,
                              f"已抽 {len(thumbs)} 张缩略图（见报告）：请人工复核 UI/logo/水印/帧率显示/鼠标指针/穿模/闪屏/拖影/半透明/加载",
                              "需求§3.3/§6.1/§6.3"))
        if role == "fixed":
            fixed_gray[key] = gray

    # 跨遍固定机位比对
    fixed = {wl: [lf for lf in g.lanes_of(wl) if lf.info.role == "fixed"] for wl in ("A", "C", "D")}
    for wl in ("C", "D"):
        if g.lanes_of(wl):
            tgt = f"世界线{wl}"
            if len(fixed["A"]) != 1 or len(fixed[wl]) != 1:
                out.extend(_xtake_unverified(tgt, "A 或目标世界线没有唯一固定机位"))
                continue
            a_lf, other_lf = fixed["A"][0], fixed[wl][0]
            a_key, other_key = a_lf.info.key, other_lf.info.key
            if a_key not in fixed_gray or other_key not in fixed_gray:
                out.extend(_xtake_unverified(tgt, "固定机位视频缺失或未完整解码"))
                continue
            ref, other = fixed_gray[a_key], fixed_gray[other_key]
            reason = _xtake_comparability(lanes.get(a_key), lanes.get(other_key), len(ref), len(other), video_summaries)
            if reason is not None:
                out.extend(_xtake_unverified(tgt, reason))
                continue
            n = len(ref)
            diff = (np.abs(ref[:n].astype(np.int16) - other[:n].astype(np.int16)) > XTAKE_T).reshape(n, -1).mean(1)
            nsec = n // 30
            per_sec = [float(x) for x in diff[: nsec * 30].reshape(nsec, 30).mean(1)]
            label = f"{a_lf.info.take}→{other_lf.info.take}" if g.layout == "take" else f"A→{wl}"
            art["xtake"][label] = {"per_sec": per_sec, "worldline": wl, "takeover_s": takeover_s.get(wl)}
            t_take = takeover_s.get(wl)
            if t_take is None:
                out.extend(_xtake_unverified(tgt, "README 未给出接管/干预时刻，无法划分接管前后窗口"))
                continue
            pre = per_sec[: int(t_take)]
            post = per_sec[int(t_take) + 2:]
            if len(pre) >= 1:
                m = sum(pre) / len(pre)
                st = PASS if m <= XTAKE_PRE_OK else WARN if m <= XTAKE_PRE_WARN else FAIL
                out.append(RuleResult("pix.xtake_pre", st, tgt,
                                      f"固定机位 {a_key} ↔ {other_key} 接管前（0–{int(t_take)} s）逐秒变化像素占比均值 {m * 100:.1f}%、最大 {max(pre) * 100:.1f}%"
                                      f"（≤{XTAKE_PRE_OK * 100:.0f}% 视为与基线逐帧一致，海浪/植被等环境动画构成本底噪声）",
                                      "需求§4.3 / 确认点①「分叉前必须可复现、一致」", data={"pre_mean": m}))
            else:
                out.append(RuleResult("pix.xtake_pre", SKIP, tgt, f"接管从视频第 {t_take:g} 秒起，没有接管前窗口可比", "需求§4.3"))
            if post:
                p95 = sorted(post)[int(0.95 * (len(post) - 1))]
                st = PASS if p95 <= XTAKE_POST_P95_MAX else WARN
                out.append(RuleResult("pix.xtake_post", st, tgt,
                                      f"接管后（{int(t_take) + 2} s 起）逐秒变化像素占比 p95 {p95 * 100:.0f}%、均值 {100 * sum(post) / len(post):.0f}%"
                                      + ("：差异局部化（受控人物及其引发的反应），未受干预部分继续按基线" if st == PASS
                                         else "：差异大面积，需确认是否只有受干预部分在变"),
                                      "需求§4.3「差异须能对应具体控制行为」", data={"post_p95": p95}))
            # 分叉起点
            base = max(0.06, 2 * (sum(pre) / len(pre)) if pre else 0.06)
            onset = next((i for i, x in enumerate(per_sec) if x > base), None)
            if onset is None:
                out.append(RuleResult("pix.xtake_onset", WARN, tgt, "全程未检出明显分叉（与 A 几乎逐帧相同）：干预是否真的改变了世界须看画面", "需求§4.3"))
            elif onset + 1 >= t_take:
                out.append(RuleResult("pix.xtake_onset", PASS, tgt,
                                      f"画面分叉起点第 {onset} s，不早于接管时刻 {t_take:g} s（阈值 {base * 100:.0f}%）", "需求§4.3", data={"onset": onset}))
            else:
                out.append(RuleResult("pix.xtake_onset", FAIL, tgt,
                                      f"画面分叉起点第 {onset} s 早于接管时刻 {t_take:g} s：接管前世界已与基线不同（阈值 {base * 100:.0f}%）",
                                      "需求§4.3 / 确认点①", data={"onset": onset}))
    return out, art

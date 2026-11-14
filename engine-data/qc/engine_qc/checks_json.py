"""JSON 面规则：打包 / 容器 / schema / 键名 / 时长 / 帧连续性 / 数值 / systeminfo / covis / depth 对账。"""

import json
import math
import zipfile
from pathlib import Path
from typing import Dict, List, Optional, Tuple

from .config import NOMINAL_FPS, RESOLUTION_WHITELIST, WRONG_KEYS, QCConfig
from .mathutil import quat_angle_deg, quat_forward, quat_norm, vdot, vnorm, vsub
from .placement import find_map_meta
from .report import FAIL, PASS, SKIP, UNVERIFIED, RuleResult
from .schema_validator import load_schema, validate_instance


# ---------------------------------------------------------------- 打包/命名

def check_packaging(files, cfg: QCConfig) -> List[RuleResult]:
    out: List[RuleResult] = []
    missing_json = [k for k, v in files.action.items() if v is None]
    missing_sys = [k for k, v in files.sysinfo.items() if v is None]
    n_json = len(files.action) - len(missing_json) + len(files.sysinfo) - len(missing_sys)
    total_json = len(files.action) + len(files.sysinfo)
    if missing_json or missing_sys:
        miss = [f"action_camera_{k}.json" for k in missing_json]
        miss += [(f"systeminfo_{k}.json" if files.mode == "mc" else "systeminfo.json")
                 for k in missing_sys]
        out.append(RuleResult("pack.files", FAIL, "clip",
                              f"json 文件 {n_json}/{total_json}，缺：{', '.join(miss)}"))
    else:
        out.append(RuleResult("pack.files", PASS, "clip",
                              f"json 文件 {total_json}/{total_json} 齐全"))

    n_video = sum(1 for v in files.videos.values() if v is not None)
    total_video = len(files.videos)
    if n_video == total_video:
        out.append(RuleResult("pack.video", PASS, "clip",
                              f"video 文件 {n_video}/{total_video} 齐全"))
    else:
        missing = [(f"video_{k}.mp4" if files.mode == "mc" else "video.mp4")
                   for k, v in files.videos.items() if v is None]
        out.append(RuleResult("pack.video", SKIP, "clip",
                              f"video 文件 {n_video}/{total_video}，缺：{', '.join(missing)}"
                              "（video 检查按缺失项跳过）"))

    if files.mode == "sfm":
        if files.depth_zip is not None:
            out.append(RuleResult("pack.depth", PASS, "clip", "depth.zip 存在"))
        else:
            out.append(RuleResult("pack.depth", SKIP, "clip", "depth.zip 缺失（跳过 depth 对账）"))
    return out


# ---------------------------------------------------------------- 容器/schema/键名

def check_container(name: str, obj) -> Optional[RuleResult]:
    """action_camera 容器必须是 JSON 数组。返回 FAIL 或 PASS。"""
    if isinstance(obj, list):
        return RuleResult("json.container", PASS, name, f"顶层为 JSON 数组，{len(obj)} 帧")
    return RuleResult("json.container", FAIL, name,
                      f"顶层应为 JSON 数组，实际 {type(obj).__name__}")


def check_schema(name: str, frames, schema_name: str) -> RuleResult:
    schema = load_schema(schema_name)
    errors = validate_instance(frames, schema)
    if errors:
        head = "; ".join(errors[:5])
        more = f"（共 {len(errors)}+ 条）" if len(errors) > 5 else ""
        return RuleResult("json.schema", FAIL, name, f"schema({schema_name}) 不通过：{head}{more}")
    return RuleResult("json.schema", PASS, name,
                      f"{len(frames)} 帧全部通过 schema({schema_name})")


def check_wrong_keys(name: str, frames, mode: str) -> RuleResult:
    """跨口径键名黑名单：mc 里出现 68 的键 / sfm 里出现 69 的键即 FAIL。"""
    bad: Dict[str, int] = {}
    for f in frames:
        if not isinstance(f, dict):
            continue
        for k in f.keys():
            if k in WRONG_KEYS[mode]:
                bad[k] = bad.get(k, 0) + 1
    if bad:
        detail = "; ".join(f"'{k}' 出现 {c} 帧（{WRONG_KEYS[mode][k]}）" for k, c in bad.items())
        return RuleResult("json.keys", FAIL, name, detail)
    return RuleResult("json.keys", PASS, name, "未发现跨口径键名混入")


# ---------------------------------------------------------------- 时长/连续性

def check_duration(name: str, frames, mode: str, file_key: str, cfg: QCConfig) -> RuleResult:
    n = len(frames)
    if mode == "mc":
        if n == cfg.mc_frames:
            return RuleResult("dur.frames", PASS, name,
                              f"{n} 帧 = {cfg.mc_duration_s:g}s×30fps（±0）")
        return RuleResult("dur.frames", FAIL, name,
                          f"{n} 帧 ≠ {cfg.mc_frames}（{cfg.mc_duration_s:g}s×30fps ±0）")
    eff_fps = 15.0 if file_key == "15fps" else 30.0
    dur = n / eff_fps
    if any(abs(dur - d) < 1e-9 for d in cfg.sfm_durations_s):
        return RuleResult("dur.frames", PASS, name, f"{n} 帧 @ {eff_fps:g}fps = {dur:g}s ∈ {{60,180}}s")
    return RuleResult("dur.frames", FAIL, name,
                      f"{n} 帧 @ {eff_fps:g}fps = {dur:g}s ∉ {{60,180}}s")


def check_continuity(name: str, frames, step: int) -> RuleResult:
    """frame 从 0 起、恒定步进（30fps 步进 1 / 15fps 偶数步进 2），无重复/跳帧。"""
    seq = [f.get("frame") for f in frames if isinstance(f, dict)]
    if len(seq) != len(frames) or not all(isinstance(x, int) and not isinstance(x, bool) for x in seq):
        return RuleResult("frame.continuity", FAIL, name, "存在非法/缺失的 frame 字段")
    problems = []
    if seq and seq[0] != 0:
        problems.append(f"首帧 frame={seq[0]} ≠ 0")
    for i in range(1, len(seq)):
        d = seq[i] - seq[i - 1]
        if d != step:
            kind = "重复" if d == 0 else ("回退" if d < 0 else "跳帧")
            problems.append(f"第 {i} 条 frame {seq[i-1]}→{seq[i]}（{kind}，步进应为 {step}）")
            if len(problems) >= 5:
                break
    if step == 2 and any(x % 2 for x in seq[:1]):
        problems.append("15fps 文件 frame 应为偶数")
    if problems:
        return RuleResult("frame.continuity", FAIL, name, "; ".join(problems))
    return RuleResult("frame.continuity", PASS, name,
                      f"frame 0..{seq[-1] if seq else '-'} 连续步进 {step}，无重复/跳帧")


def check_depth_names(name: str, frames) -> RuleResult:
    """sfm 15fps：depth_name == depth_{frame}。"""
    bad = []
    for i, f in enumerate(frames):
        if not isinstance(f, dict):
            continue
        expect = f"depth_{f.get('frame')}"
        if f.get("depth_name") != expect:
            bad.append(f"[{i}] depth_name={f.get('depth_name')!r} 应为 {expect!r}")
            if len(bad) >= 5:
                break
    if bad:
        return RuleResult("frame.depthname", FAIL, name, "; ".join(bad))
    return RuleResult("frame.depthname", PASS, name, "depth_name 与 frame 一一对应")


def check_sfm_pair(name30: str, frames30, name15: str, frames15) -> RuleResult:
    """15fps 车道 = 30fps 车道偶数帧的逐字段拷贝（唯一新增键 `depth_name`）。

    68 号 0728 日志对「15fps json 的步进 2 写法」做了限制：15fps 文件是抽帧，
    不是重新生成。帧数对上只能证明「抽了」，证明不了「抽的是同一份」——
    speed/pose 若在抽帧时被重算，两车道会静默分叉。逐字段对账把 15fps 车道的
    数值正确性整体锚定到 30fps 车道上（后者有独立的差分/物理组核验）。
    """
    target = f"{name30}+{name15}"
    expect = (len(frames30) + 1) // 2
    if len(frames15) != expect:
        return RuleResult("sync.sfm_pair", FAIL, target,
                          f"15fps {len(frames15)} 帧 ≠ ⌈{len(frames30)}/2⌉={expect}")
    by_frame = {f.get("frame"): f for f in frames30 if isinstance(f, dict)}
    bad = []
    for f15 in frames15:
        if not isinstance(f15, dict):
            continue
        k = f15.get("frame")
        f30 = by_frame.get(k)
        if f30 is None:
            bad.append(f"[frame={k}] 30fps 车道无此帧")
        else:
            stripped = {kk: v for kk, v in f15.items() if kk != "depth_name"}
            if stripped != f30:
                diff = sorted(kk for kk in set(stripped) | set(f30)
                              if stripped.get(kk) != f30.get(kk))
                bad.append(f"[frame={k}] 字段不一致：{', '.join(diff[:4])}")
        if len(bad) >= 5:
            break
    if bad:
        return RuleResult("sync.sfm_pair", FAIL, target,
                          "15fps 不是 30fps 偶数帧的逐字段拷贝：" + "; ".join(bad))
    return RuleResult("sync.sfm_pair", PASS, target,
                      f"15fps {len(frames15)} 帧 = ⌈{len(frames30)}/2⌉，"
                      "且逐字段与 30fps 偶数帧一致（除 depth_name）")


# ---------------------------------------------------------------- 数值组

def _num3(v) -> bool:
    return (isinstance(v, list) and len(v) in (3, 4)
            and all(isinstance(x, (int, float)) and not isinstance(x, bool) for x in v))


def check_quat_norm(name: str, frames, cfg: QCConfig) -> RuleResult:
    bad = []
    worst = 0.0
    checked = 0
    for i, f in enumerate(frames):
        if not isinstance(f, dict):
            continue
        for key in ("camera_rotation_quaternion", "player_rotation_quaternion"):
            q = f.get(key)
            if q is None:
                continue
            if not _num3(q) or len(q) != 4:
                bad.append(f"[{i}].{key} 非 4 元数值数组")
                continue
            checked += 1
            dev = abs(quat_norm(q) - 1.0)
            worst = max(worst, dev)
            if dev > cfg.quat_norm_tol:
                if len(bad) < 5:
                    bad.append(f"[{i}].{key} |q|={quat_norm(q):.6f}（偏差 {dev:.2e}）")
                else:
                    bad.append("…")
                    break
    if checked == 0 and not bad:
        return RuleResult("num.quat_norm", SKIP, name, "无四元数字段可检")
    if bad:
        return RuleResult("num.quat_norm", FAIL, name,
                          f"模长偏差超容差 {cfg.quat_norm_tol:g}：{'; '.join(bad[:6])}")
    return RuleResult("num.quat_norm", PASS, name,
                      f"{checked} 个四元数模长≈1（最大偏差 {worst:.2e} ≤ {cfg.quat_norm_tol:g}）")


def _intrinsics(f):
    it = f.get("camera_intrinsics") if isinstance(f, dict) else None
    if not isinstance(it, dict):
        return None
    try:
        return tuple(float(it[k]) for k in ("fx", "fy", "cx", "cy"))
    except (KeyError, TypeError, ValueError):
        return None


def check_fx_fy(name: str, frames, cfg: QCConfig) -> RuleResult:
    bad = []
    for i, f in enumerate(frames):
        it = _intrinsics(f)
        if it is None:
            return RuleResult("num.fx_fy", SKIP, name, f"[{i}] camera_intrinsics 字段异常（见 schema）")
        fx, fy = it[0], it[1]
        if abs(fx - fy) > cfg.float_eq_tol * max(1.0, abs(fx)):
            bad.append(f"[{i}] fx={fx} ≠ fy={fy}")
            if len(bad) >= 5:
                break
    if bad:
        return RuleResult("num.fx_fy", FAIL, name, "; ".join(bad))
    return RuleResult("num.fx_fy", PASS, name, f"{len(frames)} 帧 fx=fy")


def check_intrinsics_const(name: str, frames) -> RuleResult:
    """内参逐帧恒定（FOV 恒定红线）。"""
    first = None
    for i, f in enumerate(frames):
        it = _intrinsics(f)
        if it is None:
            return RuleResult("num.intrinsics_const", SKIP, name,
                              f"[{i}] camera_intrinsics 字段异常（见 schema）")
        if first is None:
            first = it
        elif it != first:
            return RuleResult("num.intrinsics_const", FAIL, name,
                              f"[{i}] 内参漂移：{it} ≠ 首帧 {first}（FOV 恒定红线）")
    return RuleResult("num.intrinsics_const", PASS, name,
                      f"内参逐帧恒定 (fx,fy,cx,cy)={first}")


def check_metric_scale(name: str, frames, required: bool, cfg: QCConfig) -> RuleResult:
    bad = []
    seen = 0
    for i, f in enumerate(frames):
        if not isinstance(f, dict):
            continue
        ms = f.get("metric_scale")
        if ms is None:
            if required:
                bad.append(f"[{i}] 缺 metric_scale")
                if len(bad) >= 5:
                    break
            continue
        seen += 1
        if not isinstance(ms, (int, float)) or isinstance(ms, bool) or abs(ms - 1.0) > cfg.float_eq_tol:
            bad.append(f"[{i}] metric_scale={ms!r} ≠ 1.0")
            if len(bad) >= 5:
                break
    if bad:
        return RuleResult("num.metric_scale", FAIL, name, "; ".join(bad))
    if seen == 0 and not required:
        return RuleResult("num.metric_scale", SKIP, name, "该路无 metric_scale 字段（非必含）")
    return RuleResult("num.metric_scale", PASS, name, f"{seen} 帧 metric_scale=1.0")


def check_player_speed(name: str, frames, dt: float, cfg: QCConfig,
                       subsampled: bool = False) -> RuleResult:
    """`player_speed` 必须等于 `player_position` 的逐帧差分 × fps。

    69 号 §8.2 json 验收第 8 条「监控 speed 数值符合物理逻辑」。
    实测过的失败形态：差分基准错取第 0 帧（而非 k−1），于是
    `player_speed = (pos[k]−pos[0])·fps` 越跑越大——clip-003000 有 98.2%
    的帧超过表 2 的 6 m/s 上限，而 `player_position` 本身完全正确，
    两者自相矛盾。逐帧比对是唯一能抓到这种「自洽地错」的办法。

    subsampled=True（sfm 15fps 抽帧车道）：speed 是 30fps 瞬时值的拷贝，
    对 1/15s 步长做差分在加速段必然出现 |Δv|/2 量级的「偏差」（本条真机数据
    实测 0.015 m/s，恰为 a·dt/2）——那不是数据错，是口径错。该车道的数值
    正确性由 sync.sfm_pair 逐字段锚定到 30fps 车道，这里不重复差分。

    需求方侧的旁证（PR #497 查证）：#405 归档的 15fps 样例**自身也不满足**
    `player_speed = Δpos/dt`——frame 2→4 位置完全相同而 speed 非零。说明该字段
    在需方侧就是独立上报的瞬时量，不是交付位置的差商；本规则的前提对交付数据
    本就不普遍成立，抽帧车道跳过差分不是我们放宽，是回到字段的真实语义。
    """
    if subsampled:
        return RuleResult("num.player_speed", SKIP, name,
                          "抽帧车道：player_speed 为 30fps 瞬时值拷贝，"
                          "差分口径由 30fps 车道核验，逐字段一致性由 sync.sfm_pair 对账")
    idx = [i for i, f in enumerate(frames)
           if isinstance(f, dict) and "player_speed" in f and "player_position" in f]
    if len(idx) < 2:
        return RuleResult("num.player_speed", SKIP, name, "该路无 player_speed/position（非必含）")
    bad, worst = [], 0.0
    for k in range(1, len(idx)):
        a, b = frames[idx[k - 1]], frames[idx[k]]
        pa, pb, sp = a.get("player_position"), b.get("player_position"), b.get("player_speed")
        if not (isinstance(pa, list) and isinstance(pb, list) and isinstance(sp, list)
                and len(pa) == len(pb) == len(sp) == 3):
            bad.append(f"[{idx[k]}] 字段形状异常")
            break
        for j in range(3):
            d = abs(sp[j] - (pb[j] - pa[j]) / dt)
            worst = max(worst, d)
        if worst > cfg.speed_consistency_tol and len(bad) < 5:
            bad.append(f"[{idx[k]}] |player_speed−Δpos/dt| = {worst:.3f} m/s")
    if bad:
        return RuleResult("num.player_speed", FAIL, name,
                          "player_speed 与 player_position 差分不符：" + "; ".join(bad))
    return RuleResult("num.player_speed", PASS, name,
                      f"{len(idx)} 帧 player_speed = Δpos·fps（最大偏差 {worst:.2e} m/s）")


def check_input_stream(name: str, frames, cfg: QCConfig, mode: str = "mc",
                       traj_class: Optional[str] = None) -> RuleResult:
    """镜头在动，`mouse_dx/dy` 就不能恒为 0——但只在键鼠属于必交项的车道上判。

    69 号 §8.2 json 验收第 4 条要求「dx、dy 数值变化与实际画面镜头的移动精准对齐」、
    第 11 条要求「观察键鼠与画面人物对应情况」。程序化采集若把计划里的输入流丢掉、
    整列写 0，画面明明在转而鼠标零位移，人工质检一看就挂。

    两处口径（docs/88 辩题二十一）：

    * 「在转」用**全片累计转动**判（≥ `input_total_rot_min_deg`），不用逐步阈值——
      逐步阈值随采样步长漂移，同一条轨迹曾出现 30fps 判「没转」、15fps 判「在转」
      （0.36°/步卡在 0.2°/帧与 0.4°/帧之间），且累计转了 356° 的 clip 被
      30fps 车道放行，正好漏掉本规则要抓的形态。
    * sfm 模式下键鼠只在 Open 轨迹强制（docs/68 0722 变更原文「对键鼠映射的要求
      放宽，现在只在 Open 轨迹上做要求」）：非 Open 轨迹全 0 合规；轨迹类目
      不明时判 UNVERIFIED——「适用与否未知」不能读成「检查通过」。
    """
    idx = [i for i, f in enumerate(frames) if isinstance(f, dict) and "mouse_dx" in f]
    if not idx:
        return RuleResult("num.input_stream", SKIP, name, "该路无 mouse_* 字段（非必含）")
    total_deg = 0.0
    prev = None
    for i in idx:
        q = frames[i].get("camera_rotation_quaternion")
        if isinstance(q, list) and len(q) == 4:
            if prev is not None:
                total_deg += quat_angle_deg(prev, q)
            prev = q
    turned = total_deg >= cfg.input_total_rot_min_deg
    nz = sum(1 for i in idx
             if (frames[i].get("mouse_dx") or 0) != 0 or (frames[i].get("mouse_dy") or 0) != 0)
    if not turned or nz > 0:
        return RuleResult("num.input_stream", PASS, name,
                          f"mouse_dx/dy 非零帧 {nz}/{len(idx)}"
                          + ("" if turned
                             else f"；该路镜头基本无转动（累计 {total_deg:.1f}°）"))
    # 镜头在转、输入流全 0：是否成缺陷取决于该车道键鼠是否必交。
    if mode == "sfm":
        if traj_class is None:
            return RuleResult("num.input_stream", UNVERIFIED, name,
                              f"镜头累计转动 {total_deg:.0f}° 而 mouse_dx/dy 恒为 0，"
                              "但轨迹类目未知——docs/68 0722 起键鼠只在 Open 轨迹强制。"
                              "请用 --traj-class 指明，或让 clip 附 plan.json")
        if not traj_class.lower().startswith("open"):
            return RuleResult("num.input_stream", PASS, name,
                              f"轨迹 {traj_class} 非 Open：键鼠非必交（docs/68 0722），"
                              f"mouse_dx/dy 全 0 合规（镜头累计转动 {total_deg:.0f}°）")
    return RuleResult("num.input_stream", FAIL, name,
                      f"镜头累计转动 {total_deg:.0f}°，但 {len(idx)} 帧 mouse_dx/dy 恒为 0"
                      "（输入流没接上，§8.2 第 4/11 条必挂）")


# ---------------------------------------------------------------- systeminfo

def check_systeminfo(name: str, obj, cfg: QCConfig) -> List[RuleResult]:
    out: List[RuleResult] = []
    errors = validate_instance(obj, load_schema("systeminfo"))
    if errors:
        out.append(RuleResult("sysinfo.schema", FAIL, name,
                              f"6 字段 schema 不通过：{'; '.join(errors[:5])}"))
    else:
        out.append(RuleResult("sysinfo.schema", PASS, name, "6 字段 schema 通过"))
    if not isinstance(obj, dict):
        out.append(RuleResult("sysinfo.origin", SKIP, name, "非对象，无法检 (x,y)"))
        out.append(RuleResult("sysinfo.resolution", SKIP, name, "非对象，无法检分辨率"))
        return out
    x, y = obj.get("x"), obj.get("y")
    if x == 0 and y == 0:
        out.append(RuleResult("sysinfo.origin", PASS, name, "(x,y)=(0,0)"))
    else:
        out.append(RuleResult("sysinfo.origin", FAIL, name, f"(x,y)=({x},{y}) ≠ (0,0)"))
    w, h = obj.get("width"), obj.get("height")
    if (w, h) in RESOLUTION_WHITELIST:
        out.append(RuleResult("sysinfo.resolution", PASS, name,
                              f"{w}x{h} ∈ 1080P 档位白名单"))
    else:
        wl = sorted(RESOLUTION_WHITELIST)
        out.append(RuleResult("sysinfo.resolution", FAIL, name,
                              f"{w}x{h} ∉ 白名单 {wl}"))
    return out


# ---------------------------------------------------------------- covis / depth 对账

def check_covis(covis_path: Optional[Path], obj, err: Optional[str], cfg: QCConfig) -> RuleResult:
    if covis_path is None:
        return RuleResult("covis.ratio", SKIP, "covis.json", "缺失（trajgen 排程器产物未随附）")
    if err is not None:
        return RuleResult("covis.ratio", FAIL, "covis.json", f"无法解析：{err}")
    ratio = obj.get("covis_ratio") if isinstance(obj, dict) else None
    if not isinstance(ratio, (int, float)) or isinstance(ratio, bool):
        return RuleResult("covis.ratio", FAIL, "covis.json", f"covis_ratio 字段异常：{ratio!r}")
    n_ts = len(obj.get("pair_min_overlap_ts") or []) if isinstance(obj, dict) else 0
    if ratio >= cfg.covis_min:
        return RuleResult("covis.ratio", PASS, "covis.json",
                          f"covis_ratio={ratio:g} ≥ {cfg.covis_min}（曲线点 {n_ts} 个）")
    return RuleResult("covis.ratio", FAIL, "covis.json",
                      f"covis_ratio={ratio:g} < {cfg.covis_min}（曲线点 {n_ts} 个）")


#: 严读闸门要求的逐对曲线键集：前三路两两三对，第四路显式排除（69 号 §0）
_COVIS_PAIRS = ("1-2", "1-3", "2-3")


def _pair_curves(obj) -> Tuple[Optional[Dict[str, list]], Optional[str]]:
    """`pair_curves` 三态校验：(curves, None)=合法；(None, None)=无该键；(None, 错误)=形状非法。

    键集必须**恰为** {"1-2","1-3","2-3"}、三条曲线等长非空、逐点数值，且与
    `pair_min_overlap_ts`/`times` 时间轴（若存在）逐点对齐。此前的实现对
    「恰好存在的键」取 min，从不校验键集与长度——本机实测三种畸形全部静默放行，
    证据行还照打「三对全成立」：只留 1-2 一对（min 退化成单对宽读）、换成
    1-4/2-4（把被排除的第四路顶进来）、把一条曲线截成 3 个点（min(len) 只剩 3 点）。
    严读的「三对全部」首先是个键集与长度断言，才轮得到取 min。
    """
    if not isinstance(obj, dict) or "pair_curves" not in obj:
        return None, None
    curves = obj["pair_curves"]
    if not isinstance(curves, dict):
        return None, f"pair_curves 应为对象，实际 {type(curves).__name__}"
    keys, want = set(curves), set(_COVIS_PAIRS)
    if keys != want:
        miss = sorted(want - keys)
        extra = sorted(keys - want)
        parts = ([f"缺对 {'、'.join(miss)}"] if miss else []) \
            + ([f"多出 {'、'.join(extra)}"] if extra else [])
        return None, ("pair_curves 键集应恰为 {1-2,1-3,2-3}（前三路三对、第四路排除）："
                      + "；".join(parts))
    for k in _COVIS_PAIRS:
        v = curves[k]
        if not (isinstance(v, list) and v
                and all(isinstance(x, (int, float)) and not isinstance(x, bool) for x in v)):
            return None, f"曲线 {k} 非法（应为非空数值数组）"
    lens = {k: len(curves[k]) for k in _COVIS_PAIRS}
    if len(set(lens.values())) != 1:
        return None, f"三条曲线长度不一致：{lens}"
    n = lens["1-2"]
    for ts_key in ("pair_min_overlap_ts", "times"):
        ts = obj.get(ts_key)
        if isinstance(ts, list) and len(ts) != n:
            return None, f"曲线 {n} 点与 {ts_key} {len(ts)} 点不对齐"
    return curves, None


def _strict_ratio(curves: Dict[str, list], overlap_min: float) -> float:
    """从**已通过 `_pair_curves` 校验**的三对曲线按严读重算共视占比。

    这条重算是口径核验的底座：`covis_ratio` 只是个数字，它是宽读还是严读算出来的，
    从数字本身看不出来。`pair_curves` 是原始逐对曲线，两种读法都能从它推出来，
    所以拿它当裁判。
    """
    n = len(curves["1-2"])
    return sum(1 for k in range(n)
               if min(float(curves[p][k]) for p in _COVIS_PAIRS) >= overlap_min) / n


def _wide_note(obj, prefix: str = "宽读对照=") -> str:
    """宽读对照只在 `covis_ratio_any_pair` 真实存在时打印。

    此前缺键时回退打印 `covis_ratio`——「宽读对照=0.5591」实为同一个严读数
    换了个名字自己对照自己（本机实测）。没记录就写没记录。
    """
    if isinstance(obj, dict) and "covis_ratio_any_pair" in obj:
        return f"{prefix}{obj['covis_ratio_any_pair']!r}"
    return "宽读=未记录"


def _overlap_min(obj, cfg: QCConfig) -> float:
    v = obj.get("overlap_min") if isinstance(obj, dict) else None
    if isinstance(v, (int, float)) and not isinstance(v, bool) and 0.0 < float(v) <= 1.0:
        return float(v)
    return cfg.covis_overlap_min


def check_covis_reading(covis_path: Optional[Path], obj, err: Optional[str],
                        cfg: QCConfig) -> RuleResult:
    """`covis.json` 必须自证它是**严读**口径算出来的。

    docs/72 修正单 001（PR #460）把共视聚合锁死为严读——前三路**三对全部**
    overlap≥0.30 才计时长——同时 trajgen 把 `covis_ratio` 字段的语义一并改成严读，
    宽读降级为 `covis_ratio_any_pair` 只作对照（docs/84 §7.2）。

    问题在于**改口径没有改字段名**：改严读之前的 `covis.json` 里，`covis_ratio`
    装的是宽读数。同一个键、同一个量纲、同一个闸门，数字大出一截却照样过。
    R2 交付桶里那条 `2026_08_04_megacity_01` 就是这么过的闸——声明 0.686，
    按严读重算只有 0.198（差距核查见配套 PR 的核查文档；文档号待该 PR 合入时分配，此处刻意不写死）。

    所以这条规则不看数字大小，只看**这个文件是不是当前口径产出的**：
    有 `pair_curves` 就重算严读跟声明值对，没有就退回读 `gate` 自述。
    两条都拿不到证据 → FAIL：无从判断口径的 covis.json 不具备放行资格。
    """
    if covis_path is None:
        return RuleResult("covis.reading", SKIP, "covis.json", "缺失（trajgen 排程器产物未随附）")
    if err is not None or not isinstance(obj, dict):
        return RuleResult("covis.reading", FAIL, "covis.json", f"无法解析：{err}")

    ratio = obj.get("covis_ratio")
    gate = obj.get("gate") if isinstance(obj.get("gate"), str) else ""
    om = _overlap_min(obj, cfg)
    if abs(om - cfg.covis_overlap_min) > 1e-9:
        return RuleResult("covis.reading", FAIL, "covis.json",
                          f"overlap_min={om:g}，应为 {cfg.covis_overlap_min:g}（69 号 §0「≥30% 相同区域」）")

    curves, cerr = _pair_curves(obj)
    if cerr is not None:
        return RuleResult("covis.reading", FAIL, "covis.json",
                          f"pair_curves 形状非法，无法按严读复核口径：{cerr}")
    strict = None if curves is None else _strict_ratio(curves, om)
    if strict is not None and isinstance(ratio, (int, float)) and not isinstance(ratio, bool):
        if abs(float(ratio) - strict) <= cfg.covis_declare_tol:
            return RuleResult("covis.reading", PASS, "covis.json",
                              f"covis_ratio={ratio:g} 与按严读重算 {strict:.4f} 一致（严读口径）")
        return RuleResult(
            "covis.reading", FAIL, "covis.json",
            f"covis_ratio={ratio:g} 与按严读重算的 {strict:.4f} 不一致"
            f"（差 {abs(float(ratio) - strict):.4f} > {cfg.covis_declare_tol}）"
            f"，{_wide_note(obj)} —— 该文件是改严读（docs/72 修正单 001）之前的宽读口径，"
            "必须用当前 trajgen 重算 covis.json，不能只改数字")

    if "三对全部" in gate:
        return RuleResult("covis.reading", PASS, "covis.json",
                          "gate 自述为严读（三对全部成立），无 pair_curves 可复核")
    if gate:
        return RuleResult("covis.reading", FAIL, "covis.json",
                          f"gate 自述不是严读口径：{gate!r}")
    return RuleResult("covis.reading", FAIL, "covis.json",
                      "既无 pair_curves 可重算、gate 字段也未自述口径——无法判定 covis_ratio "
                      "是宽读还是严读，不予放行")


def check_covis_strict(covis_path: Optional[Path], obj, err: Optional[str],
                       cfg: QCConfig) -> RuleResult:
    """共视**严读**闸门：前三路**三对全部** overlap≥0.30 的时长占比 ≥ 50%。

    需求方原文「多个相机画面中共同存在（不包括第四相机）≥30% 相同的区域」——
    「共同存在」+ 显式排除第四路（余下恰好三路）⇒ 三路都要看到那块区域。
    docs/72 修正单 001 已按这个读法定案，本规则据此**从只报数改成硬闸门**。
    """
    if covis_path is None or err is not None or not isinstance(obj, dict):
        return RuleResult("covis.ratio_strict", SKIP, "covis.json", "无 covis 曲线可算严读")
    curves, cerr = _pair_curves(obj)
    if cerr is not None:
        return RuleResult("covis.ratio_strict", FAIL, "covis.json",
                          f"pair_curves 形状非法，禁止对残缺键集静默取 min：{cerr}")
    if curves is None:
        return RuleResult("covis.ratio_strict", SKIP, "covis.json",
                          "无 pair_curves 字段（严读闸门退回 covis.ratio）")
    strict = _strict_ratio(curves, _overlap_min(obj, cfg))
    tail = "；" + _wide_note(obj)
    if "covis_ratio_any_pair" in obj:
        tail += "（不作闸门）"
    if strict >= cfg.covis_min:
        return RuleResult("covis.ratio_strict", PASS, "covis.json",
                          f"严读（三对全成立）={strict:.4f} ≥ {cfg.covis_min}{tail}")
    return RuleResult("covis.ratio_strict", FAIL, "covis.json",
                      f"严读（三对全成立）={strict:.4f} < {cfg.covis_min}{tail}")


def check_dyn_metrics(dyn_path: Optional[Path], obj, err: Optional[str],
                      cfg: QCConfig) -> List[RuleResult]:
    """69 号 §4.3/§6：至少 80% 录制时长画面中存在动态物体，**逐路**都要满足。

    原先 QC 完全没有这条（BRINGUP-MEGACITY.md §5 自陈「QC 无此规则」），
    batch-001 因此在 20 条 lane 里有 5 条 <0.8 的情况下仍然全批 QC_PASS。
    """
    if dyn_path is None:
        return [RuleResult("dyn.active_frac", SKIP, "dyn_metrics.json", "缺失（探针产物未随附）")]
    if err is not None or not isinstance(obj, dict):
        return [RuleResult("dyn.active_frac", FAIL, "dyn_metrics.json", f"无法解析：{err}")]
    lanes = obj.get("lanes")
    if not isinstance(lanes, list) or not lanes:
        return [RuleResult("dyn.active_frac", FAIL, "dyn_metrics.json", "无 lanes 字段")]
    out: List[RuleResult] = []
    for ln in lanes:
        cam = ln.get("cam") if isinstance(ln, dict) else "?"
        frac = ln.get("active_frac") if isinstance(ln, dict) else None
        name = f"dyn_metrics.json[cam{cam}]"
        if not isinstance(frac, (int, float)) or isinstance(frac, bool):
            out.append(RuleResult("dyn.active_frac", FAIL, name, f"active_frac 异常：{frac!r}"))
        elif frac >= cfg.dyn_active_min:
            out.append(RuleResult("dyn.active_frac", PASS, name,
                                  f"active_frac={frac:.4f} ≥ {cfg.dyn_active_min}"))
        else:
            out.append(RuleResult("dyn.active_frac", FAIL, name,
                                  f"active_frac={frac:.4f} < {cfg.dyn_active_min}"))
    return out


#: 69 号 §3.2 的 6 类必含平移（粉色）。coverage 管「有没有」，honesty 管「真没真」。
_PINK_CLASSES = ("translate_up", "translate_down", "translate_forward",
                 "translate_back", "translate_left", "translate_right")


def check_pink_coverage(plan_path: Optional[Path], obj, err: Optional[str]) -> List[RuleResult]:
    """69 号 §3.2：6 类平移是「粉色」= 每条视频、**每一路**必含（100%）。

    第四路的时间重参数化会把标称轴尾部整段截掉，粉色随之丢失
    （clip-003000 的 cam4 只剩 translate_down / translate_right）。

    本规则只读 `segments[].cls` 字符串——标签造假由 plan.pink_honesty 负责拦。
    """
    pink = set(_PINK_CLASSES)
    if plan_path is None:
        return [RuleResult("plan.pink_coverage", SKIP, "plan.json", "缺失（计划未随附）")]
    if err is not None or not isinstance(obj, dict):
        return [RuleResult("plan.pink_coverage", FAIL, "plan.json", f"无法解析：{err}")]
    cams = obj.get("cameras")
    if not isinstance(cams, list) or not cams:
        return [RuleResult("plan.pink_coverage", FAIL, "plan.json", "无 cameras 字段")]
    out: List[RuleResult] = []
    for c in cams:
        cid = c.get("id") if isinstance(c, dict) else "?"
        segs = c.get("segments") if isinstance(c, dict) else None
        name = f"plan.json[cam{cid}]"
        if not isinstance(segs, list):
            out.append(RuleResult("plan.pink_coverage", FAIL, name, "无 segments"))
            continue
        have = {s.get("cls") for s in segs if isinstance(s, dict)}
        miss = sorted(pink - have)
        if miss:
            out.append(RuleResult("plan.pink_coverage", FAIL, name,
                                  f"缺必含平移 {len(miss)}/6：{', '.join(miss)}"))
        else:
            out.append(RuleResult("plan.pink_coverage", PASS, name, "6 类必含平移齐全"))
    return out


def _frame_pos(f) -> Optional[Tuple[float, float, float]]:
    p = f.get("camera_position") if isinstance(f, dict) else None
    if (isinstance(p, list) and len(p) == 3
            and all(isinstance(x, (int, float)) and not isinstance(x, bool) for x in p)):
        return (float(p[0]), float(p[1]), float(p[2]))
    return None


def _frame_quat(f) -> Optional[Tuple[float, float, float, float]]:
    q = f.get("camera_rotation_quaternion") if isinstance(f, dict) else None
    if (isinstance(q, list) and len(q) == 4
            and all(isinstance(x, (int, float)) and not isinstance(x, bool) for x in q)):
        return (float(q[0]), float(q[1]), float(q[2]), float(q[3]))
    return None


def _yaw_basis(q) -> Tuple[Tuple[float, float, float], Tuple[float, float, float]]:
    """窗起始帧四元数 → 水平 (forward, right) 基底。

    与 trajgen `primitives._basis` 同式（Unity 左手系，y 向上）：
    forward=(sin yaw, 0, cos yaw)、right=(cos yaw, 0, −sin yaw)；
    yaw 从四元数的 forward 向量取 atan2(x, z)。
    """
    fx, _, fz = quat_forward(q)
    yaw = math.atan2(fx, fz)
    fwd = (math.sin(yaw), 0.0, math.cos(yaw))
    right = (math.cos(yaw), 0.0, -math.sin(yaw))
    return fwd, right


def check_pink_honesty(plan_path: Optional[Path], obj, err: Optional[str],
                       loaded: Dict[str, Optional[list]], cfg: QCConfig) -> List[RuleResult]:
    """69 号 §3.2 的另一半：声称的平移必须**真的发生**（标签 ≠ 事实）。

    `plan.pink_coverage` 只读 `segments[].cls` 字符串——把导演段的 cls 原地改成
    6 个 translate_*、轨迹一个数字不动，coverage 即由 4×FAIL 变 4×PASS（本机实测，
    docs/89 §5.1 给接手人的修法 (a) 恰好就是这个漏洞）。本规则把每个 translate_*
    声称对回 `action_camera_N.json` 的实测轨迹（与生成侧按同一契约实现）：

    * 段窗 [t0,t1] → 30fps 名义时间轴上的帧段；
    * 基底 = **窗起始帧相机 yaw**（`_yaw_basis`，与 trajgen `primitives._basis` 同式），
      up/down 用世界 Y；
    * 判据（契约原文）：**声称轴向净位移 ≥ 0.8 m 且 ≥ 窗内路径长的 50% ⇒ PASS**；
      否则 FAIL，证据行给实测位移与占比；
    * 对应 action json 缺失 ⇒ UNVERIFIED（声称在、证据缺——不能读成通过）；
    * plan 里没有任何 translate 段 ⇒ 本规则不产生条目（「缺」由 pink_coverage 负责）。
    """
    if plan_path is None or err is not None or not isinstance(obj, dict):
        return []
    cams = obj.get("cameras")
    if not isinstance(cams, list):
        return []
    fps = obj.get("fps")
    fps = float(fps) if (isinstance(fps, (int, float)) and not isinstance(fps, bool)
                         and fps > 0) else NOMINAL_FPS

    out: List[RuleResult] = []
    for c in cams:
        if not isinstance(c, dict):
            continue
        cid = c.get("id")
        segs = c.get("segments")
        if not isinstance(segs, list):
            continue
        name = f"action_camera_{cid}.json"
        frames = loaded.get(str(cid)) if isinstance(loaded, dict) else None
        for s in segs:
            if not isinstance(s, dict):
                continue
            cls = s.get("cls")
            if not isinstance(cls, str) or not cls.startswith("translate_"):
                continue
            t0, t1 = s.get("t0"), s.get("t1")
            t0_ok = isinstance(t0, (int, float)) and not isinstance(t0, bool)
            t1_ok = isinstance(t1, (int, float)) and not isinstance(t1, bool)
            tag = (f"{cls}[{t0:g},{t1:g}]s" if t0_ok and t1_ok
                   else f"{cls}[t0={t0!r},t1={t1!r}]")
            if frames is None:
                out.append(RuleResult("plan.pink_honesty", UNVERIFIED, name,
                                      f"{tag}：对应 action json 缺失/不可用，声称无法核验"))
                continue
            if cls not in _PINK_CLASSES:
                out.append(RuleResult("plan.pink_honesty", FAIL, name,
                                      f"{tag}：未知平移类目（契约仅 6 类粉色），无法定义声称轴向"))
                continue
            if not (t0_ok and t1_ok and t1 > t0):
                out.append(RuleResult("plan.pink_honesty", FAIL, name,
                                      f"{tag}：段窗 t0/t1 非法，无法定位帧段"))
                continue
            n = len(frames)
            i0, i1 = int(round(t0 * fps)), int(round(t1 * fps))
            i0 = max(0, i0)
            i1 = min(i1, n - 1)
            if i0 >= n or i1 - i0 < 1:
                out.append(RuleResult("plan.pink_honesty", FAIL, name,
                                      f"{tag}：帧段 [{i0},{i1}] 超出 {n} 帧或不足 2 帧，"
                                      "声称的平移在轨迹里不存在"))
                continue
            q0 = _frame_quat(frames[i0])
            pos = [_frame_pos(frames[i]) for i in range(i0, i1 + 1)]
            if q0 is None or any(p is None for p in pos):
                out.append(RuleResult("plan.pink_honesty", UNVERIFIED, name,
                                      f"{tag}：窗内位姿字段异常，无法核验（见 json.schema）"))
                continue
            fwd, right = _yaw_basis(q0)
            axis = {"translate_forward": fwd,
                    "translate_back": (-fwd[0], 0.0, -fwd[2]),
                    "translate_right": right,
                    "translate_left": (-right[0], 0.0, -right[2]),
                    "translate_up": (0.0, 1.0, 0.0),
                    "translate_down": (0.0, -1.0, 0.0)}[cls]
            disp = vdot(vsub(pos[-1], pos[0]), axis)
            path = sum(vnorm(vsub(b, a)) for a, b in zip(pos, pos[1:]))
            frac = disp / path if path > 1e-9 else 0.0
            if disp >= cfg.pink_min_disp_m and disp >= cfg.pink_axis_frac_min * path:
                out.append(RuleResult(
                    "plan.pink_honesty", PASS, name,
                    f"{tag}：轴向净位移 {disp:.2f} m ≥ {cfg.pink_min_disp_m:g}，"
                    f"占窗内路径长 {frac * 100:.0f}% ≥ {cfg.pink_axis_frac_min * 100:g}%"
                    f"（路径 {path:.2f} m）"))
            else:
                out.append(RuleResult(
                    "plan.pink_honesty", FAIL, name,
                    f"{tag}：实测轴向净位移 {disp:.2f} m、窗内路径长 {path:.2f} m、"
                    f"占比 {frac * 100:.0f}%——判据须 ≥{cfg.pink_min_disp_m:g} m 且占比 "
                    f"≥{cfg.pink_axis_frac_min * 100:g}%，声称的平移未在轨迹中发生"))
    return out


def check_depth_zip(depth_zip: Optional[Path], frames15) -> RuleResult:
    if depth_zip is None:
        return RuleResult("depth.match", SKIP, "depth.zip", "depth.zip 缺失，跳过对账")
    if frames15 is None:
        return RuleResult("depth.match", SKIP, "depth.zip", "15fps json 不可用，无法对账")
    try:
        with zipfile.ZipFile(depth_zip) as zf:
            members = {Path(n).stem for n in zf.namelist() if not n.endswith("/")}
    except (zipfile.BadZipFile, OSError) as e:
        return RuleResult("depth.match", FAIL, "depth.zip", f"无法读取：{e}")
    expected = {f.get("depth_name") for f in frames15 if isinstance(f, dict)}
    expected.discard(None)
    missing = sorted(expected - members)
    extra = sorted(members - expected)
    if missing or extra:
        return RuleResult("depth.match", FAIL, "depth.zip",
                          f"缺 {len(missing)} 个（{missing[:3]}…），多 {len(extra)} 个（{extra[:3]}…）")
    return RuleResult("depth.match", PASS, "depth.zip",
                      f"{len(expected)} 个 depth 帧与 json depth_name 一一对应")


# ---------------------------------------------------------------- 地图边界坐标系

def check_map_bounds(root: Path, frames30, cfg: QCConfig) -> RuleResult:
    """clip pose 必须落在 map_meta 边界内——抓「边界局部系、pose 世界系」的口径错位。

    docs/88 §6 残余 #1：sfm-d1 的 map_meta 边界（y≤28、|z|≤25）与 clip pose
    （y=33、z≈197–219）差了整个摆位变换，原样交付会污染元信息的「地图边界 4 坐标」。
    有 `world` 块（engine_qc map-world 产出）用世界系边界核；只有局部边界时直接核
    ——恒等摆位下应在界内，出界即口径错位，指去跑 map-world。对外交付包无 map_meta，SKIP。
    """
    meta_path = find_map_meta(Path(root))
    if meta_path is None:
        return RuleResult("plan.map_bounds", SKIP, "map_meta.json",
                          "无 map_meta（对外包不含；引擎跑录布局放兄弟 plan/）")
    if not frames30:
        return RuleResult("plan.map_bounds", SKIP, "map_meta.json", "30fps json 不可用")
    try:
        meta = json.loads(meta_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError, UnicodeDecodeError) as e:
        return RuleResult("plan.map_bounds", FAIL, "map_meta.json", f"无法读取：{e}")

    world = meta.get("world") if isinstance(meta, dict) else None
    src = world if isinstance(world, dict) else meta
    frame_label = ("「world 块（map-world 解算）」" if isinstance(world, dict)
                   else "「局部边界（无 world 块）」")
    lo, hi = src.get("bounds_lo"), src.get("bounds_hi")
    if not (isinstance(lo, list) and isinstance(hi, list) and len(lo) == len(hi) == 3):
        return RuleResult("plan.map_bounds", FAIL, "map_meta.json",
                          f"{frame_label} 缺 bounds_lo/bounds_hi")

    m = cfg.map_bounds_margin_m
    out_n, worst, worst_frame = 0, 0.0, None
    for f in frames30:
        p = f.get("camera_position") if isinstance(f, dict) else None
        if not (isinstance(p, list) and len(p) == 3):
            continue
        d = max(max(lo[i] - m - p[i], p[i] - hi[i] - m, 0.0) for i in range(3))
        if d > 0.0:
            out_n += 1
            if d > worst:
                worst, worst_frame = d, f.get("frame")
    if out_n:
        hint = ("" if isinstance(world, dict)
                else "——边界疑为地图局部系而 pose 为世界系（docs/88 §6 #1），"
                     "跑 `engine_qc map-world <跑录目录>` 补世界系边界")
        return RuleResult("plan.map_bounds", FAIL, "map_meta.json",
                          f"{out_n}/{len(frames30)} 帧 pose 出界（{frame_label}，"
                          f"最远越界 {worst:.1f} m @frame {worst_frame}）{hint}")
    return RuleResult("plan.map_bounds", PASS, "map_meta.json",
                      f"{len(frames30)} 帧 pose 全部落在{frame_label}内（余量 {m} m）")

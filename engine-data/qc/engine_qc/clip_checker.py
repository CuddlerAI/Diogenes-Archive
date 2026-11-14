"""clip 级编排：把全部规则跑在一个次级文件夹上。"""

from dataclasses import replace
from pathlib import Path
from typing import Dict, Optional

from . import checks_depth as cd
from . import checks_json as cj
from . import checks_motion as cm
from . import checks_video as cv
from .config import QCConfig
from .loaders import discover, load_json
from .report import FAIL, SKIP, ClipReport, RuleResult

#: 每个文件在 30fps 时间轴上的期望 frame 步进
_STEPS = {"mc": {"1": 1, "2": 1, "3": 1, "4": 1},
          "sfm": {"30fps": 1, "15fps": 2}}


def _schema_name(mode: str, key: str) -> str:
    if mode == "mc":
        return "mc_cam1" if key == "1" else "mc_camN"
    return "sfm_30fps" if key == "30fps" else "sfm_15fps"


def _resolve_traj_class(root: Path, plan_path: Optional[Path],
                        explicit: Optional[str]) -> Optional[str]:
    """轨迹类目：显式传入 > clip 根 plan.json > 兄弟 plan/plan.json（引擎跑录布局）。

    只有 num.input_stream 的 sfm 适用范围需要它（docs/68 0722：键鼠仅 Open 强制）。
    对外交付包里没有 plan.json——那种场景靠 --traj-class 显式给。
    """
    if explicit:
        return explicit
    candidates = [plan_path, root.parent / "plan" / "plan.json"]
    for cand in candidates:
        if cand is None or not cand.is_file():
            continue
        obj, err = load_json(cand)
        if err is None and isinstance(obj, dict) and isinstance(obj.get("traj_class"), str):
            return obj["traj_class"]
    return None


def check_clip(root, mode: str, cfg: Optional[QCConfig] = None,
               include_video: bool = True,
               traj_class: Optional[str] = None) -> ClipReport:
    cfg = cfg or QCConfig()
    root = Path(root)
    report = ClipReport(clip=str(root), mode=mode)
    if not root.is_dir():
        report.add("pack.files", FAIL, "clip", f"目录不存在：{root}")
        return report

    files = discover(root, mode)
    report.extend(cj.check_packaging(files, cfg))
    if mode == "sfm":
        traj_class = _resolve_traj_class(root, files.plan, traj_class)

    # MC 的 plan 前置装载：phys.roll 跳检与初始帧豁免要先知道第四路形态声明
    # （docs/69 §8.3 形态 b）。只认显式 mode="topdown"，非法/缺省声明不触发任何豁免。
    plan_obj, plan_err = (None, None)
    if mode == "mc" and files.plan is not None:
        plan_obj, plan_err = load_json(files.plan)
    cam4_topdown = (mode == "mc" and cm.cam4_declared_mode(plan_obj) == "topdown")

    # ---- 逐 action 文件装载 + JSON 面规则 ----
    loaded: Dict[str, Optional[list]] = {}
    for key in files.action:
        path = files.action[key]
        name = f"action_camera_{key}.json"
        if path is None:
            loaded[key] = None
            report.add("json.container", SKIP, name, "文件缺失（见 pack.files）")
            continue
        obj, err = load_json(path)
        if err is not None:
            loaded[key] = None
            report.add("json.container", FAIL, name, f"JSON 解析失败：{err}")
            continue
        r = cj.check_container(name, obj)
        report.results.append(r)
        if r.status == FAIL:
            loaded[key] = None
            continue
        frames = obj
        loaded[key] = frames

        report.results.append(cj.check_schema(name, frames, _schema_name(mode, key)))
        report.results.append(cj.check_wrong_keys(name, frames, mode))
        report.results.append(cj.check_duration(name, frames, mode, key, cfg))
        step = _STEPS[mode][key]
        report.results.append(cj.check_continuity(name, frames, step))
        if mode == "sfm" and key == "15fps":
            report.results.append(cj.check_depth_names(name, frames))

        # 数值组
        report.results.append(cj.check_quat_norm(name, frames, cfg))
        report.results.append(cj.check_fx_fy(name, frames, cfg))
        report.results.append(cj.check_intrinsics_const(name, frames))
        ms_required = (mode == "sfm") or key == "1"
        report.results.append(cj.check_metric_scale(name, frames, ms_required, cfg))
        report.results.append(cj.check_player_speed(
            name, frames, _STEPS[mode][key] / 30.0, cfg,
            subsampled=(mode == "sfm" and key == "15fps")))
        report.results.append(cj.check_input_stream(name, frames, cfg, mode, traj_class))

        # 物理阈值组（有限差分；15fps 文件 dt=1/15）
        dt = step / 30.0
        report.extend(cm.check_physics(name, frames, dt, cfg,
                                       is_cam4=(mode == "mc" and key == "4")))
        # 歪头闸门只挂 MC：docs/69 §3.2 的旋转运镜口径是 MC 需求文的类目表，
        # SFM（docs/68）没有对应规格，硬套会拿别单的尺子量这单。
        if mode == "mc":
            if key == "4" and cam4_topdown:
                # 垂直俯视下欧拉分解退化：视轴 ≈ 世界 -y，绕竖轴取向 = 画面内旋转，
                # roll 读数是分解伪影而非歪头；俯视位的取向由 cam4.topdown_pitch 把关。
                report.add("phys.roll", SKIP, name,
                           "plan.cam4.mode=topdown：垂直俯视下 roll 分解退化，"
                           "闸门不适用（docs/69 §8.3 形态 b）")
            else:
                report.extend(cm.check_roll(name, frames, dt, cfg))

    # ---- systeminfo ----
    for key in files.sysinfo:
        name = f"systeminfo_{key}.json" if mode == "mc" else "systeminfo.json"
        path = files.sysinfo[key]
        if path is None:
            report.add("sysinfo.schema", SKIP, name, "文件缺失（见 pack.files）")
            continue
        obj, err = load_json(path)
        if err is not None:
            report.add("sysinfo.schema", FAIL, name, f"JSON 解析失败：{err}")
            continue
        report.extend(cj.check_systeminfo(name, obj, cfg))

    # ---- 模式专项 ----
    if mode == "mc":
        report.extend(cm.check_mc_sync(loaded))
        report.extend(cm.check_decouple(loaded, 1.0 / 30.0, cfg))
        init_cfg = cfg
        if cam4_topdown and 4 not in (cfg.init_exempt_lanes or ()):
            # 俯视位的首帧朝向（垂直向下）与首帧高差是形态定义本身，不是失形；
            # 「t=0 锚定同一地点」改由 cam4.topdown_route 的硬帽覆盖（含首帧）。
            # 前三路之间仍照常判定（豁免语义同导演模式，docs/78 §10）。
            init_cfg = replace(cfg, init_exempt_lanes=tuple(cfg.init_exempt_lanes or ()) + (4,))
        report.extend(cm.check_initial_frame(loaded, init_cfg))
        report.extend(cm.check_cam4(loaded, plan_obj, plan_err, 1.0 / 30.0, cfg))
        covis_obj, covis_err = (None, None)
        if files.covis is not None:
            covis_obj, covis_err = load_json(files.covis)
        report.results.append(cj.check_covis_reading(files.covis, covis_obj, covis_err, cfg))
        report.results.append(cj.check_covis(files.covis, covis_obj, covis_err, cfg))
        report.results.append(cj.check_covis_strict(files.covis, covis_obj, covis_err, cfg))
        report.extend(cj.check_pink_coverage(files.plan, plan_obj, plan_err))
        report.extend(cj.check_pink_honesty(files.plan, plan_obj, plan_err, loaded, cfg))
        dyn_path = files.root / "dyn_metrics.json"
        dyn_path = dyn_path if dyn_path.exists() else None
        dyn_obj, dyn_err = load_json(dyn_path) if dyn_path is not None else (None, None)
        report.extend(cj.check_dyn_metrics(dyn_path, dyn_obj, dyn_err, cfg))
    else:
        f30, f15 = loaded.get("30fps"), loaded.get("15fps")
        if f30 is not None and f15 is not None:
            report.results.append(cj.check_sfm_pair(
                "action_camera_30fps.json", f30, "action_camera_15fps.json", f15))
        else:
            report.add("sync.sfm_pair", SKIP, "action_camera_30fps+15fps",
                       "任一 json 不可用，跳过对数一致性")
        report.results.append(cj.check_map_bounds(root, f30, cfg))
        report.results.append(cj.check_depth_zip(files.depth_zip, f15))
        # depth 内容组：打开 EXR 看格式/量化/NaN/量程，并做跨模态反投影自检。
        # 文件名对账（上一行）只能证明「文件在」，证明不了「内容对」。
        report.results.append(cd.check_flat(files.depth_zip))
        sysinfo_obj = None
        if files.sysinfo.get("main") is not None:
            sysinfo_obj, _ = load_json(files.sysinfo["main"])
        report.extend(cd.check_depth_content(files.depth_zip, f15, sysinfo_obj, cfg))

    # ---- video ----
    if include_video:
        report.extend(cv.check_videos(files, mode, cfg))
    return report

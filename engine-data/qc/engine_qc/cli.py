"""CLI 入口：check-clip / check-batch。

退出码：0 = 通过；1 = 有规则 FAIL（或抽检打回）；**2 = UNVERIFIED**——数据本身没查出问题，
但有规则该跑没跑（例如目录里有 mp4 却找不到 ffprobe/ffmpeg）。2 与 1 分开是为了让
调用方能区分「数据坏了」和「检查工具没装全」——两者的处置完全不同，混成同一个码
会让人把装个 ffmpeg 就能解决的事当成数据问题去查。
"""

import argparse
import json
import sys
import zipfile
from pathlib import Path

from . import __version__, exr
from .batch import check_batch, render_batch_text
from .clip_checker import check_clip
from .config import QCConfig
from .repair import RepairError, flip_depth_zip, flip_exr_rows
from .report import FAIL, UNVERIFIED, render_clip_text


def _exit_code(verdict: str) -> int:
    return {FAIL: 1, UNVERIFIED: 2}.get(verdict, 0)


def _build_config(args) -> QCConfig:
    cfg = QCConfig()
    if args.init_angle_deg is not None:
        cfg.init_angle_max_deg = args.init_angle_deg
    if args.init_dist_m is not None:
        cfg.init_dist_max_m = args.init_dist_m
    if getattr(args, "init_exempt_lanes", ""):
        cfg.init_exempt_lanes = tuple(
            int(x) for x in str(args.init_exempt_lanes).split(",") if x.strip())
    if getattr(args, "expect_duration_s", None):
        d = float(args.expect_duration_s)
        cfg.mc_duration_s = d
        cfg.mc_frames = int(round(d * 30.0))
        cfg.tsw_lo = d / 3.0
        cfg.tsw_hi = 2.0 * d / 3.0
    return cfg


def _run_flip(args) -> int:
    """flip-depth：翻转是修复不是验收——修完必须重跑 check-clip 重新裁决。"""
    src = Path(args.path)
    if not src.is_file():
        print(f"找不到文件：{src}", file=sys.stderr)
        return 1
    if args.in_place:
        bak = src.with_name(src.name + ".bak")
        if bak.exists():
            print(f"拒绝覆盖已存在的备份：{bak}（上一次修复的原件还在，先人工处置）",
                  file=sys.stderr)
            return 1
        out = src.with_name(src.name + ".flipping")
    else:
        out = Path(args.out)

    try:
        if src.suffix.lower() == ".zip":
            n = flip_depth_zip(src, out)
        else:
            out.write_bytes(flip_exr_rows(src.read_bytes()))
            n = 1
    except (RepairError, exr.ExrError, zipfile.BadZipFile, OSError) as e:
        out.unlink(missing_ok=True)                      # 半成品不留盘
        print(f"翻转失败，未产出任何文件：{type(e).__name__}: {e}", file=sys.stderr)
        return 1

    if args.in_place:
        src.rename(bak)
        out.rename(src)
        print(f"已翻转 {n} 个 EXR：{src}（原件 → {bak}）")
    else:
        print(f"已翻转 {n} 个 EXR → {out}")
    print("提醒：修复≠验收。请重跑 check-clip，让 depth.orientation 与内容组规则重新裁决。")
    return 0


def _run_map_world(args) -> int:
    """map-world：解算是产出不是核验——写完必须重跑 check-clip 让 plan.map_bounds 裁决。"""
    from .config import QCConfig
    from .placement import PlacementError, derive_run_world

    tol = args.residual_tol_m
    if tol is None:
        tol = QCConfig().placement_residual_tol_m
    try:
        meta, summary = derive_run_world(args.path, residual_tol_m=tol)
    except (PlacementError, OSError, json.JSONDecodeError, UnicodeDecodeError) as e:
        print(f"map-world 解算失败，未写任何文件：{e}", file=sys.stderr)
        return 1
    print(f"摆位解算：{summary}")
    w = meta["world"]
    print(f"世界系边界：lo={w['bounds_lo']} hi={w['bounds_hi']}")
    if args.dry_run:
        print("--dry-run：未写盘")
        return 0
    out = Path(args.path) / "plan" / "map_meta.json"
    out.write_text(json.dumps(meta, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    print(f"已写入 world 块：{out}")
    print("提醒：解算≠验收。请重跑 check-clip，让 plan.map_bounds 用世界系边界重新裁决。")
    return 0


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(
        prog="engine_qc",
        description="engine-data-qc：引擎产线交付数据本地自动质检"
                    "（规则依据 docs/70 §4.4 / docs/71 §9，口径以 engine-data/README.md 为准）",
    )
    parser.add_argument("--version", action="version", version=f"engine_qc {__version__}")
    sub = parser.add_subparsers(dest="command", required=True)

    def _common(p):
        p.add_argument("path", help="目标目录")
        p.add_argument("--mode", choices=["mc", "sfm"], required=True,
                       help="mc=69 Multi-Camera / sfm=68 SFM-Depth")
        p.add_argument("--json", action="store_true", help="输出机器可读 JSON")
        p.add_argument("--init-angle-deg", type=float, default=None,
                       help="MC 初始帧朝向夹角上限（内控值，默认 15）")
        p.add_argument("--init-dist-m", type=float, default=None,
                       help="MC 初始帧位置距离上限（内控值，默认 30）")
        p.add_argument("--init-exempt-lanes", default="",
                       help="初始帧闸门豁免车道，逗号分隔（4=导演模式统览位，docs/78 §10）")
        p.add_argument("--expect-duration-s", type=float, default=None,
                       help="MC 期望时长覆盖（能力档，如 300）：帧数=30×d、video 时长目标=d、"
                            "cam4 t_sw 窗按 [d/3, 2d/3] 比例缩放；不传则保持 60s 交付口径")

    p_clip = sub.add_parser("check-clip", help="质检单条次级文件夹，逐规则 PASS/FAIL/SKIP")
    _common(p_clip)
    p_clip.add_argument("--traj-class", default=None,
                        help="sfm：该 clip 的轨迹类目（docs/68 §3.3 统一名称，如"
                             " Open-环境交互 / Loop-有位移）。键鼠只在 Open 轨迹强制；"
                             "不传则从 clip 根或兄弟 plan/ 的 plan.json 读，都没有时"
                             " num.input_stream 遇到空输入流按 UNVERIFIED 处理")

    p_batch = sub.add_parser("check-batch", help="批目录汇总 + 模拟需求方抽检协议")
    _common(p_batch)
    p_batch.add_argument("--seed", type=int, default=20260803,
                         help="抽检采样随机种子（默认 20260803，保证可复现）")

    p_flip = sub.add_parser(
        "flip-depth",
        help="存量修复（docs/88）：把上下翻转的 depth EXR 行序翻回，位精确重写")
    p_flip.add_argument("path", help="depth.zip 或单个 .exr")
    g = p_flip.add_mutually_exclusive_group(required=True)
    g.add_argument("--out", help="输出到新文件，不动原件")
    g.add_argument("--in-place", action="store_true",
                   help="原地替换，原件保留为 <name>.bak")

    p_mw = sub.add_parser(
        "map-world",
        help="摆位解算（docs/88 §6 #1）：从 plan↔clip 位姿解局部→世界变换，"
             "给 plan/map_meta.json 补 world 块（世界系边界）")
    p_mw.add_argument("path", help="跑录目录（含 plan/ 与 clip/）")
    p_mw.add_argument("--residual-tol-m", type=float, default=None,
                      help="plan↔clip 最坏残差门槛（默认 1e-3 m），超过即拒绝产出")
    p_mw.add_argument("--dry-run", action="store_true",
                      help="只解算打印，不写 map_meta.json")

    args = parser.parse_args(argv)
    if args.command == "flip-depth":
        return _run_flip(args)
    if args.command == "map-world":
        return _run_map_world(args)
    cfg = _build_config(args)

    if args.command == "check-clip":
        report = check_clip(args.path, args.mode, cfg,
                            traj_class=getattr(args, "traj_class", None))
        if args.json:
            print(json.dumps(report.to_dict(), ensure_ascii=False, indent=2))
        else:
            print(render_clip_text(report))
        return _exit_code(report.verdict)

    br = check_batch(args.path, args.mode, cfg, seed=args.seed)
    if args.json:
        print(json.dumps(br.to_dict(), ensure_ascii=False, indent=2))
    else:
        print(render_batch_text(br))
    return _exit_code(br.verdict)


if __name__ == "__main__":  # pragma: no cover
    sys.exit(main())

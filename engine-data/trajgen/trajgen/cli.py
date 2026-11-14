"""CLI 入口：gen-clip / simulate / list-classes。

    python -m trajgen gen-clip  out/clip-001 --scene open_plaza --seed 7
    python -m trajgen simulate  --n 200 --json out/m3a.json
    python -m trajgen list-classes

`gen-clip` 出的次级文件夹可直接喂给 engine-data-qc：

    python -m engine_qc check-clip out/clip-001 --mode mc
"""

import argparse
import json
import sys
from pathlib import Path

from . import __version__
from .export import DEFAULT_T0, write_clip
from . import sfm as sfmmod
from . import sfm_export
from .planner import QuotaTracker, generate
from .primitives import FAMILIES, QUOTA
from .scene import SCENE_KINDS, Scene, synthetic
from .sim import render_text, run


def _load_scene(args) -> Scene:
    if args.scene_file:
        return Scene.load(args.scene_file)
    return synthetic(args.scene, args.scene_seed)


def _cmd_direct_clip(args) -> int:
    from .director import plan_director_clip
    if not args.scene_file:
        print("direct-clip 需要 --scene-file（SceneProbeExporter 的真实占据快照）")
        return 2
    # 画幅配平的关键：账本跨调用持久化。不传 --ledger 则每次全新 QuotaTracker，
    # pick_aspect 见 clips=0 恒出 16:9（实测 44/44 全 16:9 的根因）。
    quota = QuotaTracker.load(args.ledger) if args.ledger else None
    try:
        rep = plan_director_clip(args.scene_file, args.seed, args.out,
                                 duration_s=args.duration,
                                 n_extras=args.extras,
                                 covis_hz=args.covis_hz,
                                 quota=quota)
    except RuntimeError as e:
        print("direct-clip 失败：%s" % e)
        return 1
    if args.ledger:
        # 只在成功后落账：抛异常的 clip 不占画幅配额（宁可不出货 ⇒ 也不记账）
        quota.note_clip(rep["aspect"])
        quota.save(args.ledger)
    print("director clip → %s" % args.out)
    print("  时长 %gs 画幅 %s 焦距 %s guide=cam%d"
          % (rep["duration_s"], rep["aspect"], rep["focals"], rep["guide_cam"]))
    c4p = rep.get("cam4_path", {})
    print("  共视(严口径) %.1f%%  初帧(四路全判) %.1f°/%.1fm  cam4 %s档 ρ实测 %s"
          % (rep["covis_ratio_strict"] * 100,
             rep["init_angle_deg"], rep["init_dist_m"],
             "快" if c4p.get("band_fast") else "慢",
             c4p.get("rho_meas")))
    print("  cam4-前三路重叠观测 %s" % rep.get("cam4_covis_observed"))
    print("  用第 %d 次尝试；相互可见 %s" % (rep["attempt_used"], rep["mutual_visibility_frac"]))
    if args.ledger:
        print("  账本 %s：clips=%d aspect_hits=%s"
              % (args.ledger, quota.clips,
                 json.dumps(quota.aspect_hits, ensure_ascii=False)))
    return 0


def _cmd_gen_clip(args) -> int:
    if getattr(args, "mode", "mc") == "sfm":
        return _cmd_gen_clip_sfm(args)
    scene = _load_scene(args)
    plan = generate(scene, seed=args.seed, quota=QuotaTracker(), covis_hz=args.covis_hz,
                    cam4_mode=getattr(args, "cam4_mode", "timewarp"))
    out = write_clip(plan, args.out, base_time=args.base_time,
                     process_name=args.process_name)
    verdict = "闸门全过" if plan.ok else "未过：" + "; ".join(plan.reasons)
    print("clip → %s" % out)
    print("  场景 %s(%s) 画幅 %s 焦距 %s"
          % (scene.name, scene.kind, plan.aspect, [t.focal_mm for t in plan.tracks]))
    print("  共视 %.1f%%  最差同步对 %.0f%%  四路同时 %.0f%%  首帧夹角 %.1f°"
          % ((plan.covis.ratio * 100) if plan.covis else -1,
             plan.decouple.worst_pair[1] * 100 if plan.decouple else -1,
             plan.decouple.all4_frac * 100 if plan.decouple else -1,
             plan.init_angle))
    print("  第四路 %s" % plan.cam4_plan.to_dict())
    print("  自检：%s" % verdict)
    return 0 if plan.ok else 1


def _cmd_gen_clip_sfm(args) -> int:
    """68 号：整片轨迹拓扑，一条 clip 一个类目。"""
    scene = _load_scene(args)
    plan, tries = sfmmod.plan_clip(scene, args.seed, cls_name=args.traj_class,
                                   duration=args.duration)
    out = sfm_export.write_clip(plan, args.out, base_time=args.base_time,
                                process_name=args.process_name)
    print("clip → %s" % out)
    print("  场景 %s(%s) 类目 %s 时长 %.0fs 帧 %d"
          % (scene.name, scene.kind, plan.cls.name, plan.duration, plan.n_frames))
    ph = plan.physics
    if ph:
        print("  运动学 p95：v=%.2f a=%.2f j=%.2f ω=%.1f α=%.1f 角j=%.0f"
              % (ph.speed_p95, ph.accel_p95, ph.jerk_p95,
                 ph.angspeed_p95, ph.angaccel_p95, ph.angjerk_p95))
    print("  拓扑：首尾位置差 %.3fm 朝向差 %.2f° 往复 %.1f 次 扰动窗转角 %.1f°%s"
          % (plan.loop_closure_m, plan.loop_closure_deg, plan.osc_cycles,
             plan.window_turn_deg, "" if plan.dither_ok else "（偏直，§3.2 非硬性）"))
    print("  自检：%s（尝试 %d 次）"
          % ("全过" if plan.ok else "未过：" + "; ".join(plan.reasons), tries))
    return 0 if plan.ok else 1


def _cmd_simulate(args) -> int:
    rep = run(n_plans=args.n, scene_kinds=args.scenes or tuple(SCENE_KINDS),
              scene_seed=args.scene_seed, seed0=args.seed, covis_hz=args.covis_hz,
              progress=args.progress)
    if args.json:
        Path(args.json).parent.mkdir(parents=True, exist_ok=True)
        with open(args.json, "w", encoding="utf-8") as f:
            json.dump(rep.to_dict(), f, ensure_ascii=False, indent=2)
        print("报告 → %s" % args.json)
    print(render_text(rep))
    return 0 if rep.success_rate >= 0.80 else 1


def _cmd_list_classes_sfm() -> int:
    print("68 号轨迹类目（%d 类；navigate 合计 %.0f%%，特殊运镜合计 %.0f%%）"
          % (len(sfmmod.ALL_CLASSES),
             100 * sum(c.quota for c in sfmmod.NAVIGATE),
             100 * sum(c.quota for c in sfmmod.SPECIAL)))
    for c in sfmmod.ALL_CLASSES:
        durs = "/".join("%.0fs×%d" % (d, w) for d, w in sorted(c.dur_weights.items()))
        print("  %-9s %-18s %5.1f%%  %s" % (c.group, c.name, 100 * c.quota, durs))
    return 0


def _cmd_list_classes(args) -> int:
    if getattr(args, "mode", "mc") == "sfm":
        return _cmd_list_classes_sfm()
    print("MC 原语库 10 族 23 类（docs/71 §4）：")
    for fam, classes in FAMILIES.items():
        for c in classes:
            op, tgt = QUOTA.get(c, ("=", 1.0))
            share = "每条必含" if c not in QUOTA else "%s%.0f%%" % (op, tgt * 100)
            print("  %-8s %-18s 语料级配额 %s" % (fam, c, share))
    return 0


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(
        prog="trajgen",
        description="engine-data-trajgen：Multi-Camera 二期计划生成器 + M3a 排程仿真"
                    "（规则依据 docs/71 §4–6/§10，键名以 engine-data/fixtures/ 为准）")
    parser.add_argument("--version", action="version", version="trajgen %s" % __version__)
    sub = parser.add_subparsers(dest="command", required=True)

    def _scene_args(p):
        p.add_argument("--scene", choices=sorted(SCENE_KINDS), default="open_plaza",
                       help="合成场景类型（M3a 无美术资产期的替身）")
        p.add_argument("--scene-file", help="真实场景占据图 json（SceneKit 导出）")
        p.add_argument("--scene-seed", type=int, default=1)
        p.add_argument("--covis-hz", type=float, default=2.0,
                       help="共视度量的时间采样率（默认 2Hz）")

    p_gen = sub.add_parser("gen-clip", help="生成一条 4 路计划并落盘次级文件夹")
    p_gen.add_argument("out", help="输出目录")
    p_gen.add_argument("--seed", type=int, default=0)
    p_gen.add_argument("--base-time", default=DEFAULT_T0, help="time 字段的墙钟基准")
    p_gen.add_argument("--process-name", default="DiogenesEngineScenes")
    p_gen.add_argument("--mode", choices=("mc", "sfm"), default="mc",
                       help="mc=69 四路多相机 / sfm=68 单路 SFM-Depth")
    p_gen.add_argument("--cam4-mode", dest="cam4_mode",
                       choices=("timewarp", "topdown"), default="timewarp",
                       help="（mc）第四路形态（docs/69 §8.3）："
                            "timewarp=同路线时间错位变速（缺省，plan 不写 mode 键）"
                            "/ topdown=同区域高空垂直俯视伴飞（形态 b）")
    p_gen.add_argument("--traj-class", dest="traj_class",
                       help="（sfm）指定轨迹类目；不给则按配额随机抽")
    p_gen.add_argument("--duration", type=float, choices=list(sfmmod.DURATIONS),
                       help="（sfm）单条时长，默认按该类目的时长比抽")
    _scene_args(p_gen)

    p_sim = sub.add_parser("simulate", help="M3a 排程仿真（docs/71 §10）")
    p_sim.add_argument("--n", type=int, default=200, help="计划条数（验收 ≥200）")
    p_sim.add_argument("--seed", type=int, default=0)
    p_sim.add_argument("--scenes", nargs="*", choices=sorted(SCENE_KINDS))
    p_sim.add_argument("--json", help="报告落盘路径")
    p_sim.add_argument("--progress", action="store_true")
    _scene_args(p_sim)

    p_lc = sub.add_parser("list-classes", help="列出运镜类目与语料级配额")
    p_lc.add_argument("--mode", choices=("mc", "sfm"), default="mc")

    p_dir = sub.add_parser("direct-clip",
                           help="车辆导演模式（docs/78 §10）：三车载机位 + 全景观察位")
    p_dir.add_argument("out", help="输出目录")
    p_dir.add_argument("--seed", type=int, default=0)
    p_dir.add_argument("--duration", type=float, default=60.0,
                       help="单条时长 s（默认 60=交付口径；300 为能力演示档）")
    p_dir.add_argument("--extras", type=int, default=2, help="氛围受控车数量")
    p_dir.add_argument("--ledger",
                       help="画幅配额账本 JSON（QuotaTracker 计数快照）。批量时"
                            "跨条传同一文件 ⇒ 画幅逐步配平 60/10/10/20；"
                            "不给则单发新账本、首条恒 16:9")
    _scene_args(p_dir)

    args = parser.parse_args(argv)

    return {"gen-clip": _cmd_gen_clip, "direct-clip": _cmd_direct_clip, "simulate": _cmd_simulate,
            "list-classes": _cmd_list_classes}[args.command](args)


if __name__ == "__main__":  # pragma: no cover
    sys.exit(main())

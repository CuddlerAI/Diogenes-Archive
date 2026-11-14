#!/usr/bin/env python3
"""为一批 clip 生成目检取证包：逐路联表 + 待人填的判定表。

**engine_qc 是数值口径，本工具覆盖的是它测不到的定性项**——穿模、贴脸遮挡、
空画面、卡墙/闪屏/漂移、画面质感。QC 全绿不构成可交付结论：这条产线已有两批
125 PASS / 0 FAIL 的数据在目检这一步被打回。

产物（写进 `<runs-root>/visual-audit/`）：

    AUDIT.md                          抽样表 + 逐项判定（**人填**，本工具不下结论）
    audit-<clip>-cam<N>.png           3×3 联表，9 个时间点

联表规格（与既有批次逐像素对齐，实测反推自 warehouse-v3）：

- 时间点 `t_i = i × 时长 / 8`（i=0..8），即含首帧与末帧；
  末帧取 `-sseof`——`-ss <时长>` 在恰好 60.000s 的片子上取不到帧。
- 3×3 网格，**tile 宽固定 520px**，高按源画幅等比（1920×1080→292、
  1440×1080→390、1080×1440→694），无留白无标注。

用法：
    python3 visual_audit.py --runs-root <dir>              # 默认抽 5 条
    python3 visual_audit.py --runs-root <dir> --count 8
    python3 visual_audit.py --runs-root <dir> --clips clip-023000 clip-023004
"""

import argparse
import json
import subprocess
import sys
from pathlib import Path

FFMPEG = "/opt/homebrew/bin/ffmpeg"
FFPROBE = "/opt/homebrew/bin/ffprobe"

LANES = 4
TILE_W = 520
GRID = 3
SHOTS = GRID * GRID

#: 判定项与它们各自的「什么算打回」，与 docs/69 §8.1 一致。
CHECKS = [
    ("穿模", "NPC 相互粘连/嵌进载具、载具互穿、agent 嵌进场景几何"),
    ("贴脸遮挡", "动态物体贴到镜头前占满画面"),
    ("空画面", "整帧只有天花板/墙/地板，无场景元素与动态物体"),
    ("卡墙/闪屏/漂移", "画面静止不动、亮度突变、位姿与画面不一致"),
    ("画面质感", "光照/材质异常、Z-fighting、明显 LOD 跳变"),
]


def probe(video: Path) -> tuple:
    """(宽, 高, 时长)。取不到就报错退出——联表几何全靠它。"""
    out = subprocess.run(
        [FFPROBE, "-v", "error", "-select_streams", "v:0",
         "-show_entries", "stream=width,height", "-show_entries", "format=duration",
         "-of", "default=nw=1:nk=1", str(video)],
        capture_output=True, text=True)
    vals = [v for v in out.stdout.split() if v]
    if len(vals) < 3:
        sys.exit("ffprobe 读不出 %s：%s" % (video, out.stderr[-200:]))
    return int(vals[0]), int(vals[1]), float(vals[2])


def aspect_label(w: int, h: int) -> str:
    from math import gcd
    g = gcd(w, h)
    return "%d:%d" % (w // g, h // g)


def tile_height(w: int, h: int) -> int:
    """tile 高 = ffmpeg `scale=520:-2` 的取值：等比后**就近取偶**。

    不用普通四舍五入——既有批次是按 `-2` 出的（292/390/694/924 全为偶数），
    3:4 那档两者差 1px（693 vs 694），联表就对不齐了。
    """
    return int(round(TILE_W * h / w / 2)) * 2


def contact_sheet(video: Path, dest: Path) -> str:
    """抽 9 帧拼 3×3 联表，返回画幅标签。"""
    w, h, dur = probe(video)
    tile_h = tile_height(w, h)
    # 每个时间点单独 seek——比 select 滤镜慢，但落点精确可复现，
    # 且不受关键帧间隔影响（联表要能和别的批次逐像素对齐）。
    inputs, filters = [], []
    for i in range(SHOTS):
        if i == SHOTS - 1:
            inputs += ["-sseof", "-0.04", "-i", str(video)]   # 末帧
        else:
            inputs += ["-ss", "%.6f" % (i * dur / (SHOTS - 1)), "-i", str(video)]
        filters.append("[%d:v]scale=%d:%d,setsar=1[t%d]" % (i, TILE_W, tile_h, i))
    filters.append("".join("[t%d]" % i for i in range(SHOTS))
                   + "xstack=inputs=%d:layout=%s[out]"
                   % (SHOTS, "|".join("%d_%d" % (c * TILE_W, r * tile_h)
                                      for r in range(GRID) for c in range(GRID))))
    cmd = [FFMPEG, "-v", "error", "-y"]
    for i in range(SHOTS):
        cmd += inputs[i * 4:(i + 1) * 4] if i < SHOTS - 1 else inputs[-4:]
    cmd += ["-filter_complex", ";".join(filters), "-map", "[out]",
            "-frames:v", "1", str(dest)]
    r = subprocess.run(cmd, capture_output=True, text=True)
    if r.returncode != 0 or not dest.exists():
        sys.exit("联表生成失败 %s：%s" % (dest.name, r.stderr[-400:]))
    return aspect_label(w, h)


def pick(runs_root: Path, count: int, explicit) -> list:
    """选样：只从 QC_PASS 里挑，按画幅去重优先，好让抽样覆盖到窄画幅。

    窄画幅是这条产线的结构性弱项（docs/71 §10.1、BRINGUP-FACTORY §4.2），
    抽样若全是 16:9 等于没查最容易出问题的那部分。
    """
    passed = [d for d in sorted(runs_root.glob("clip-*"))
              if (d / ".state-QC_PASS").exists()
              and (d / "capture" / "clip" / "video_1.mp4").exists()]
    if explicit:
        byname = {d.name: d for d in passed}
        missing = [c for c in explicit if c not in byname]
        if missing:
            sys.exit("这些 clip 不存在或非 QC_PASS：%s" % missing)
        return [byname[c] for c in explicit]
    if not passed:
        sys.exit("%s 下没有 QC_PASS 且带视频的 clip" % runs_root)
    seen, first, rest = set(), [], []
    for d in passed:
        w, h, _ = probe(d / "capture" / "clip" / "video_1.mp4")
        a = aspect_label(w, h)
        (rest if a in seen else first).append(d)
        seen.add(a)
    return (first + rest)[:count]


#: 数值线索的两条启发式阈值。**不是判据**——它们只回答「先看哪一路」。
#: 真正的判定仍然靠人看联表：occlusion 高不一定是贴脸（也可能只是画面里东西多），
#: active_frac 有空窗也不一定是空画面（场景元素可能仍在画里）。
OCCLUSION_HINT = 0.60      # 动态物体平均占画幅比
GAP_HINT = 0.95            # 有动态物体入画的帧占比


def numeric_hints(runs_root: Path) -> list:
    """扫全批 dyn_metrics，挑出值得优先看的路。engine_qc 已经判它们通过了，
    这里换一个角度重排优先级，而不是重新设一道闸。"""
    hints = []
    for clip_dir in sorted(runs_root.glob("clip-*")):
        if not (clip_dir / ".state-QC_PASS").exists():
            continue
        try:
            doc = json.loads((clip_dir / "capture" / "clip" / "dyn_metrics.json").read_text())
        except (OSError, json.JSONDecodeError):
            continue
        lanes = next((v for v in doc.values()
                      if isinstance(v, list) and v and isinstance(v[0], dict)), [])
        for i, lane in enumerate(lanes, 1):
            share = lane.get("mean_pixel_share")
            frac = lane.get("active_frac")
            if share is not None and share >= OCCLUSION_HINT:
                hints.append((clip_dir.name, i, "贴脸遮挡?",
                              "动态物体平均占画幅 %.0f%%" % (share * 100)))
            if frac is not None and frac < GAP_HINT:
                gap = lane.get("frames", 0) - lane.get("active_frames", 0)
                hints.append((clip_dir.name, i, "空画面?",
                              "%d 帧 ≈%.1fs 无动态物体入画（active_frac %.3f，"
                              "仍过 0.8 门槛）" % (gap, gap / 30.0, frac)))
    return hints


def write_audit_md(runs_root: Path, rows: list, out: Path) -> None:
    lines = [
        "# 交付目检取证", "",
        "> 批次 `%s` · 抽 %d 条 × %d 时间点 × %d 路"
        % (runs_root.name, len(rows), SHOTS, LANES), ">",
        "> **engine_qc 是数值口径，本表覆盖的是它测不到的定性项。**",
        "> QC 全绿不构成可交付结论——已有两批 125 PASS / 0 FAIL 的数据在这一步被打回。",
        "", "## 抽样", "", "| clip | 画幅 | 联表 |", "|---|---|---|",
    ]
    for name, aspect in rows:
        links = " · ".join("[cam%d](audit-%s-cam%d.png)" % (n, name, n)
                           for n in range(1, LANES + 1))
        lines.append("| %s | %s | %s |" % (name, aspect, links))
    hints = numeric_hints(runs_root)
    lines += ["", "## 数值线索（**不是判定**，只指出先看哪一路）", ""]
    if hints:
        lines += ["engine_qc 已判这些 clip 通过；下表换一个角度重排优先级。"
                  "阈值是启发式：占比高不一定是贴脸（也可能画面里东西本来就多），"
                  "有空窗也不一定是空画面（场景元素可能仍在画里）。**结论以联表为准。**",
                  "", "| clip | 路 | 疑似 | 依据 |", "|---|---|---|---|"]
        for name, lane, kind, why in hints:
            mark = "**" if name in {r[0] for r in rows} else ""
            lines.append("| %s%s%s | cam%d | %s | %s |"
                         % (mark, name, mark, lane, kind, why))
        lines.append("")
        lines.append("（**粗体** = 本次已出联表的抽样条；其余需要时用 "
                     "`--clips` 单独出图。）")
    else:
        lines.append("无——全批 `mean_pixel_share` < %.2f 且 `active_frac` ≥ %.2f。"
                     % (OCCLUSION_HINT, GAP_HINT))
    lines += ["", "## 逐项判定（人填，每项写「通过 / 打回 + 证据图 + 帧号」）", ""]
    for title, desc in CHECKS:
        lines += ["### " + title, "", "> " + desc, "", "- [ ] 判定：", ""]
    lines += ["## 结论", "",
              "- [ ] 本批**可交付** / **打回**：",
              "- 判定人：", "- 日期：", ""]
    out.write_text("\n".join(lines), encoding="utf-8")


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--runs-root", required=True)
    ap.add_argument("--count", type=int, default=5, help="抽样条数（默认 5）")
    ap.add_argument("--clips", nargs="*", help="指定 clip 目录名，给了就不自动抽样")
    args = ap.parse_args()

    runs_root = Path(args.runs_root).expanduser()
    if not runs_root.is_dir():
        sys.exit("runs-root 不存在：%s" % runs_root)
    # 作废批次不出取证包——取证包是「送去判可否交付」的东西，
    # 给一个已判死的批次做取证只会让人误以为它还在候选里。
    if (runs_root / "VOID.md").exists():
        sys.exit("%s 已作废（见 VOID.md），不生成取证包" % runs_root.name)

    out_dir = runs_root / "visual-audit"
    out_dir.mkdir(exist_ok=True)
    rows = []
    for clip_dir in pick(runs_root, args.count, args.clips):
        aspect = None
        for lane in range(1, LANES + 1):
            video = clip_dir / "capture" / "clip" / ("video_%d.mp4" % lane)
            dest = out_dir / ("audit-%s-cam%d.png" % (clip_dir.name, lane))
            a = contact_sheet(video, dest)
            aspect = aspect or a
            print("  %s" % dest.name, flush=True)
        rows.append((clip_dir.name, aspect))
    write_audit_md(runs_root, rows, out_dir / "AUDIT.md")
    print("取证包 → %s（%d 条 × %d 路，判定表待人填）"
          % (out_dir, len(rows), LANES))
    return 0


if __name__ == "__main__":
    sys.exit(main())

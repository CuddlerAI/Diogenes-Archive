"""CLI：check-group。退出码同 engine_qc：0 通过 / 1 有 FAIL / 2 UNVERIFIED（该跑没跑）。"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

from . import __version__
from .html_report import render_html
from .model import FAIL, STATUS_ZH, UNVERIFIED
from .pipeline import check_group
from .requirements import evaluate


def _exit_code(verdict: str) -> int:
    return {FAIL: 1, UNVERIFIED: 2}.get(verdict, 0)


def render_text(rep) -> str:
    lines = [f"组 {rep.group}：判定 {rep.verdict}（{STATUS_ZH[rep.verdict]}）  " + "  ".join(f"{k} {v}" for k, v in rep.counts.items() if v)]
    lines.append("")
    lines.append("需求条款：")
    for row in evaluate(rep.results):
        c = row["clause"]
        lines.append(f"  [{row['status']:<10}] {c.id} {c.section}：{c.text[:60]}")
    lines.append("")
    for st in (FAIL, UNVERIFIED):
        rs = [r for r in rep.results if r.status == st]
        if rs:
            lines.append(f"{st}（{len(rs)}）：")
            for r in rs:
                lines.append(f"  - {r.rule} @ {r.target}：{r.evidence}")
    return "\n".join(lines)


def main(argv=None) -> int:
    p = argparse.ArgumentParser(prog="worldline_qc", description="Multi-Camera 三期四世界线交付组质检")
    p.add_argument("--version", action="version", version=f"worldline_qc {__version__}")
    sub = p.add_subparsers(dest="command", required=True)
    q = sub.add_parser("check-group", help="质检一个采集组（12 路）")
    q.add_argument("path", help="采集组目录（平铺 1～12 或 AB/C/D 遍布局）")
    q.add_argument("--expect-duration-s", type=float, default=120.0, help="每路应有时长（三期 120）")
    q.add_argument("--json", dest="json_out", default=None, help="机器可读结果输出路径")
    q.add_argument("--html", dest="html_out", default=None, help="HTML 报告输出路径")
    q.add_argument("--no-pixels", action="store_true", help="跳过像素面（无 opencv 时自动 UNVERIFIED）")
    q.add_argument("--thumbs", type=int, default=6, help="每路缩略图张数")
    q.add_argument("--roles", default=None, help="角色覆盖 JSON：{\"AB/2\": \"free\", ...} 或平铺 {\"2\": \"free\"}")
    q.add_argument("--quiet", action="store_true")
    args = p.parse_args(argv)

    roles = None
    if args.roles:
        roles = json.loads(Path(args.roles).read_text(encoding="utf-8"))
    rep, art = check_group(Path(args.path), expect_s=args.expect_duration_s, roles=roles,
                           pixels=not args.no_pixels, n_thumbs=args.thumbs)
    if args.json_out:
        d = rep.to_dict()
        d["artifacts"] = {k: v for k, v in art.items() if k != "thumbs"}
        Path(args.json_out).write_text(json.dumps(d, ensure_ascii=False, indent=1), encoding="utf-8")
    if args.html_out:
        Path(args.html_out).write_text(render_html(rep, art), encoding="utf-8")
    if not args.quiet:
        print(render_text(rep))
        if args.html_out:
            print(f"\nHTML → {args.html_out}")
    return _exit_code(rep.verdict)


if __name__ == "__main__":  # pragma: no cover
    sys.exit(main())

"""CLI：pack-mc / audit-mc / audit-depth。

退出码：0 = 合格；1 = 有问题。
"""

import argparse
import csv
import json
import sys
from pathlib import Path

from . import __version__
from .audit_depth import audit_run
from .audit_depth import render as render_depth
from .audit_mc import audit_dir, audit_keys
from .audit_mc import render as render_mc
from .pack_mc import ledger_row, pack_clip, write_gameinfo


def _cmd_pack_mc(args) -> int:
    res = pack_clip(args.src, args.out, game=args.game, group=args.group,
                    prefix=args.prefix, qc_verdict=args.qc_verdict,
                    dry_run=args.dry_run)
    if args.json:
        print(json.dumps(res.to_dict(), ensure_ascii=False, indent=2))
    else:
        print(f"[{'OK' if res.ok else 'BLOCKED'}] {res.group} → {res.dest}")
        for p in res.problems:
            print(f"    ✗ {p}")
        if res.skipped_internal:
            print(f"    · 已排除的我方内部产物：{', '.join(res.skipped_internal)}")
        if res.copied:
            print(f"    · 拷入 {len(res.copied)} 个必要文件")
    if res.ok and args.ledger:
        row = ledger_row(res, args.game, args.scene or "")
        p = Path(args.ledger)
        new = not p.exists()
        with p.open("a", newline="", encoding="utf-8-sig") as fh:
            w = csv.DictWriter(fh, fieldnames=list(row))
            if new:
                w.writeheader()
            w.writerow(row)
        print(f"    · 台账追加 → {p}")
    if res.ok and args.gameinfo:
        print(f"    · 元信息骨架 → {write_gameinfo(args.gameinfo, args.game)}")
    return 0 if res.ok else 1


def _cmd_audit_mc(args) -> int:
    if args.keys:
        keys = [ln.strip() for ln in Path(args.keys).read_text(encoding="utf-8").splitlines()
                if ln.strip()]
        rep = audit_keys(args.keys, keys)
    else:
        rep = audit_dir(args.path)
    print(json.dumps(rep.to_dict(), ensure_ascii=False, indent=2) if args.json
          else render_mc(rep))
    return 0 if rep.verdict == "OK" else 1


def _cmd_audit_depth(args) -> int:
    rep = audit_run(args.path)
    print(json.dumps(rep.to_dict(), ensure_ascii=False, indent=2) if args.json
          else render_depth(rep))
    return 0 if rep.verdict == "OK" else 1


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(
        prog="engine_delivery",
        description="引擎线交付形态工具：打包 / 审计（69 号 §1、68 号 §6.4-6.5）")
    ap.add_argument("--version", action="version", version=f"engine_delivery {__version__}")
    sub = ap.add_subparsers(dest="command", required=True)

    p = sub.add_parser("pack-mc", help="采集目录 → 恰好 12 文件的 MC 交付目录")
    p.add_argument("src", help="采集出的次级文件夹")
    p.add_argument("--out", required=True, help="交付根目录")
    p.add_argument("--game", required=True, help="gameProcessName，作为路径一级")
    p.add_argument("--group", required=True,
                   help="组名 YYYY_MM_DD_<场景拼音>_<两位序号>")
    p.add_argument("--scene", default="", help="台账里的场景列")
    p.add_argument("--prefix", default="", help="2MultiCamera 之上的前缀（如 internal-validation）")
    p.add_argument("--qc-verdict", default=None,
                   help="engine_qc check-clip 的判定；非 PASS 则拒绝打包")
    p.add_argument("--ledger", default=None, help="交付台账 csv（追加一行）")
    p.add_argument("--gameinfo", default=None, help="写一份元信息字段骨架到该路径")
    p.add_argument("--dry-run", action="store_true")
    p.add_argument("--json", action="store_true")
    p.set_defaults(func=_cmd_pack_mc)

    p = sub.add_parser("audit-mc", help="审计交付树形态（本地目录或对象键清单）")
    p.add_argument("path", nargs="?", default=".", help="交付根目录")
    p.add_argument("--keys", default=None,
                   help="改为读一份对象键清单（每行一个 key），用于直接审 R2/TOS 前缀")
    p.add_argument("--json", action="store_true")
    p.set_defaults(func=_cmd_audit_mc)

    p = sub.add_parser("audit-depth", help="判一次 depth 采集能不能转成 68 号交付形态")
    p.add_argument("path", help="采集 run 目录（含 depth-index.jsonl）")
    p.add_argument("--json", action="store_true")
    p.set_defaults(func=_cmd_audit_depth)

    args = ap.parse_args(argv)
    return args.func(args)


if __name__ == "__main__":  # pragma: no cover
    sys.exit(main())

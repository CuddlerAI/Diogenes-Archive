#!/usr/bin/env python3
"""取一个还没被占的文档编号——**对着远端 main 取，不是对着你的分支取**。

`check-docs-number-collisions.py` 能拦住撞号，但只在 CI 变红之后。那时文档已经
写完、引用已经建好、PR 已经开了，改号要动文件名、标题、和所有引用它的地方。
这个脚本是同一件事的前半段：**起草之前先问一句「哪个号是空的」**。

## 为什么"看一眼目录"不够

编号是分支间的共享资源，而每条分支看到的 main 都是自己 fork 那一刻的快照。
起草时 119 空着是真的，合并时 119 已被占也是真的——两件事不矛盾，
因为中间隔着别人的合并。所以判空必须对**当前远端** main，不能对工作树。

2026-08-03 起这条已复发**十一次**。08-07 一天三次、08-11 一天三次：

    08-07：101 → 103 → 105 → 106（三次改号，后两次撞的都是几小时前刚放的号）
    08-11：#688 的 116、#700 的 119、#706 的 119（第三次撞的是当天第二次腾出来的号）

08-11 那次尤其说明问题：#700 与 #706 **各自独立地**选了 119，两边都没错——
两条分支 fork 时 119 都是空的。人工复查接不住这个，因为要复查的不是自己的分支。

## 用法

    python3 DataMiner/tools/next-docs-number.py                  # 全部目录
    python3 DataMiner/tools/next-docs-number.py DataMiner/docs   # 指定一个

默认会先 `git fetch origin main`。离线或不想动网络时加 `--no-fetch`，
此时读的是本地 `origin/main` 引用，**可能已经过期**，脚本会明说。

输出里的「已被占用的最大号」是给人看的上下文，真正要用的是「下一个可用号」。
"""

from __future__ import annotations

import argparse
import pathlib
import re
import subprocess
import sys

REPO_ROOT = pathlib.Path(__file__).resolve().parents[2]

# 与 check-docs-number-collisions.py 保持同一份清单。加目录时两处都要改。
NUMBERED_DOC_DIRS = (
    "DataMiner/docs",
    "model_research/research",
)

NUMBER_PREFIX = re.compile(r"^(\d+)-")


def git(*args: str) -> str:
    result = subprocess.run(
        ["git", *args], cwd=REPO_ROOT, capture_output=True, text=True, check=False
    )
    if result.returncode != 0:
        raise RuntimeError(f"git {' '.join(args)} 失败：{result.stderr.strip()}")
    return result.stdout


def taken_numbers(ref: str, directory: str) -> dict[int, str]:
    """`ref` 上 `directory` 里每个已占用的编号 → 占它的那份的名字。

    直接问 git 而不是看工作树：工作树是本分支的状态，而编号是跨分支共享的。
    """
    listing = git("ls-tree", "-r", "--name-only", ref, "--", directory)
    taken: dict[int, str] = {}
    for line in listing.splitlines():
        name = line[len(directory) :].lstrip("/")
        # 目录型文档 `NNN-xxx/README.md` 与单文件 `NNN-xxx.md` 同样占号。
        head = name.split("/", 1)[0]
        match = NUMBER_PREFIX.match(head)
        if match is not None:
            taken.setdefault(int(match.group(1)), head)
    return taken


def main() -> int:
    parser = argparse.ArgumentParser(
        description="报告各 docs 目录下一个可用的文档编号（对着远端 main 判）"
    )
    parser.add_argument(
        "directories",
        nargs="*",
        default=None,
        help=f"要查的目录，默认全部：{', '.join(NUMBERED_DOC_DIRS)}",
    )
    parser.add_argument(
        "--no-fetch",
        action="store_true",
        help="不 fetch，直接读本地 origin/main（可能过期）",
    )
    args = parser.parse_args()

    if args.no_fetch:
        print("⚠️  --no-fetch：读的是本地 origin/main，可能落后于远端。", file=sys.stderr)
    else:
        try:
            git("fetch", "origin", "main")
        except RuntimeError as exc:
            print(f"⚠️  fetch 失败，改用本地 origin/main（可能过期）：{exc}", file=sys.stderr)

    try:
        ref = git("rev-parse", "origin/main").strip()
    except RuntimeError as exc:
        print(f"读不到 origin/main：{exc}", file=sys.stderr)
        return 1

    wanted = args.directories or list(NUMBERED_DOC_DIRS)
    unknown = [d for d in wanted if d not in NUMBERED_DOC_DIRS]
    if unknown:
        print(
            f"未登记的目录：{', '.join(unknown)}\n"
            f"已登记的是：{', '.join(NUMBERED_DOC_DIRS)}\n"
            "新增编号目录请同时改本脚本与 check-docs-number-collisions.py 的清单。",
            file=sys.stderr,
        )
        return 1

    print(f"依据 origin/main @ {ref[:8]}\n")
    for directory in wanted:
        taken = taken_numbers(ref, directory)
        if not taken:
            print(f"{directory}/  →  下一个可用号 1（目录下还没有编号文档）")
            continue
        highest = max(taken)
        print(f"{directory}/")
        print(f"  已被占用的最大号：{highest}  （{taken[highest]}）")
        print(f"  **下一个可用号：{highest + 1}**")
        gaps = sorted(set(range(1, highest)) - taken.keys())
        if gaps:
            # 空号多半是历史上改号腾出来的。复用会让"按号排序≈按时间排序"失效，
            # 而且旧引用可能还指着那个号，所以默认不建议用——但要让人知道它存在。
            shown = ", ".join(str(g) for g in gaps[:12])
            more = f" …等 {len(gaps)} 个" if len(gaps) > 12 else ""
            print(f"  （中间的空号：{shown}{more} —— 多为改号腾出，复用前先 grep 旧引用）")
        print()

    print("取号后到合并前若隔了较长时间，合并前再跑一次——期间可能有并发 PR 占号。")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

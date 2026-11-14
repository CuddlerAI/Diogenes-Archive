#!/usr/bin/env python3
"""编号文档不得撞号：一个 `NNN-` 前缀在一个 docs 目录里只能有一份。

编号是文档之间互相引用的地址。仓内到处是 `docs/98 §5.7.1`、`35 号文 §3.3`、
`[90 号](90-bannerlord-...)` 这样的写法，还有代码注释与脚本里的硬编码路径
（`manifest="DataMiner/docs/105-r2-asset-manifest.md"`）。两份文档共用一个号，
这些引用就全部指向歧义。

**为什么需要一道机检**：并发会话各自起草文档时，取号看的是各自分支上的 main
快照——起草时 105 空着，合并时 105 已被别人占掉。2026-08-03 到 08-11 这条
撞了十一次，其中两天各撞三次：

    08-07：101 → 103（#592，撞 #589 刚合的 101）
           103 → 105（#611，撞几小时前刚给 #592 改的 103）
           105 → 106（#622，撞几十分钟前刚合的 #611）
    08-11：116 → 120（#688，撞 main 上的 116-wm-gameplay-capture-vendor-requirements）
           119 → 122（#700，撞 main 上的 119-game-depth-capture-rights-gate-request）
           119 → 121（#706，与 #700 各自独立地选了同一个 119）

08-07 那三次撞的都是同一个人几小时前刚放上去的号；08-11 那次更清楚——#700 与
#706 **各自独立地**选了 119，两边 fork 时 119 都是空的，谁都没错。人工复查接不住
这个，因为要复查的不是自己的分支。

本检查在**合并前**拦住撞号；起草时该取哪个号，问 `next-docs-number.py`。

改号的连带成本也在涨：#611 那次除了文档自身与十几处引用，还要改 R2 守卫脚本里
硬编码的 manifest 路径——漏改的话守卫会去看一个不存在的文件，于是每次写正式桶
都被拦、且无法用正常途径解除。越早在 CI 拦住，这类连带越少。

## 两条口径

**同一篇文档的多种格式不算撞号**：`08-local-collector-technical-design.md` 与
同名 `.html` 是一篇东西的两个渲染，按主干名（stem）去重后只占一个号。

**历史撞号进基线，新撞号一律拦**：本检查上线时 `DataMiner/docs` 已有 7 处历史
撞号（见 `LEGACY_COLLISIONS`），全部是 D6/D7 逐款取证那批文档同日并行产出的。
它们的引用早已按全名而非编号建立，回头改号的收益低于风险，故予以固化；但基线
**只许缩不许长**——解决掉一处就从表里删一行，检查会提示你删。

扫描范围是**每个 docs 目录各自独立**：`DataMiner/docs` 与
`model_research/research` 是两套编号，互不冲突。目录型文档（`NNN-xxx/README.md`）
与单文件同占一个号，所以两者都要计入。
"""
from __future__ import annotations

import collections
import pathlib
import re
import sys

REPO_ROOT = pathlib.Path(__file__).resolve().parents[2]

# 每个目录一套独立编号。加新目录时在这里登记。
NUMBERED_DOC_DIRS = (
    "DataMiner/docs",
    "model_research/research",
)

# `104-depth-delivery-...md` 与 `99-steam-depth-prereq-execution/` 都算占号。
NUMBER_PREFIX = re.compile(r"^(\d+)-")

# 本检查上线（2026-08-07）时既存的撞号。**只许缩不许长**：解决一处就删一行。
# 这些是 D6/D7 逐款取证那批文档同日并行产出的结果，引用早已按全名建立；
# 回头改号要动的引用面大于收益，故固化而不是先大修一轮再上闸。
LEGACY_COLLISIONS: frozenset[tuple[str, int]] = frozenset(
    {
        ("DataMiner/docs", 40),  # d6 三款取证 + d8 roster
        ("DataMiner/docs", 41),  # d6 wuchang + d7 aoe4/bannerlord
        ("DataMiner/docs", 42),  # d7 nightreign + wuchang
        ("DataMiner/docs", 55),  # executable-identity + state-jsonl-schema
        ("DataMiner/docs", 57),  # annotator-terms + catalog-simplification
        ("DataMiner/docs", 58),  # 60fps-alignment + view-scale-survey
        ("DataMiner/docs", 61),  # batch-two-handoff + nine-game-onboarding
    }
)


def numbered_entries(directory: pathlib.Path) -> dict[int, list[str]]:
    """目录下每个编号 → 占用它的**文档**名。

    同一主干名的多种格式（`x.md` / `x.html`）视为一篇，只占一个号。
    """
    by_number: dict[int, set[str]] = collections.defaultdict(set)
    if not directory.is_dir():
        return {}
    for entry in sorted(directory.iterdir()):
        match = NUMBER_PREFIX.match(entry.name)
        if match is None:
            continue
        # 前导零不改变编号身份：`07-x.md` 与 `7-y.md` 是同一个号，两者并存
        # 一样会让「7 号文」指向歧义。
        by_number[int(match.group(1))].add(entry.stem if entry.is_file() else entry.name)
    return {number: sorted(stems) for number, stems in by_number.items()}


# 文档首行的 `# NNN · 标题` / `# NNN - 标题` / `# NNN 标题`。
TITLE_NUMBER = re.compile(r"^#+\s*(\d+)\s*[·\-—:：]?\s")


def title_number_mismatches(directory: pathlib.Path, relative: str) -> list[str]:
    """文件名的号与首行标题的号对不上的文档。

    改号要动两处（文件名、标题），而只动一处照样能通过撞号检查——文件名唯一了，
    标题却还写着旧号。**旧号往往是另一份真实存在的文档**，于是「31 号文」这样的
    称呼指向两份东西：`31-phase0-gate-threshold-recalibration.md`，以及标题写着
    `# 31` 但文件名是 33 的那份。撞号检查看不见这种，因为它只看文件名。

    2026-08-11 一次扫描查出五处：#688 与 #706 当天的两次改号（`git merge --abort`
    冲掉了未暂存的标题改动，只有 `git mv` 进了提交），外加三处更早的同型遗留。
    """
    out: list[str] = []
    if not directory.is_dir():
        return out
    for entry in sorted(directory.iterdir()):
        match = NUMBER_PREFIX.match(entry.name)
        if match is None:
            continue
        # 目录型文档的标题在 README.md 里。
        doc = entry if entry.is_file() else entry / "README.md"
        if doc.suffix != ".md" or not doc.is_file():
            continue
        try:
            first_line = doc.read_text(encoding="utf-8").split("\n", 1)[0]
        except (OSError, UnicodeDecodeError):
            continue
        title = TITLE_NUMBER.match(first_line)
        # 标题不带编号是允许的——这条只管「带了但对不上」。
        if title is None:
            continue
        if int(title.group(1)) != int(match.group(1)):
            out.append(
                f"{relative}/{entry.name}：文件名是 {int(match.group(1))} 号，"
                f"首行标题写的是 {int(title.group(1))} 号"
            )
    return out


def main() -> int:
    new_collisions: list[str] = []
    resolved: list[str] = []
    mismatches: list[str] = []

    for relative in NUMBERED_DOC_DIRS:
        directory = REPO_ROOT / relative
        entries = numbered_entries(directory)
        for number, stems in sorted(entries.items()):
            key = (relative, number)
            if len(stems) > 1 and key not in LEGACY_COLLISIONS:
                new_collisions.append(
                    f"{relative}/ 的 {number} 号被 {len(stems)} 份占用：\n"
                    + "".join(f"    - {stem}\n" for stem in stems)
                )
        for relative_baseline, number in sorted(LEGACY_COLLISIONS):
            if relative_baseline != relative:
                continue
            if len(entries.get(number, [])) <= 1:
                resolved.append(f"{relative_baseline}/ 的 {number} 号")
        mismatches.extend(title_number_mismatches(directory, relative))

    if new_collisions:
        print("编号文档撞号：", file=sys.stderr)
        for text in new_collisions:
            print(f"  {text}", file=sys.stderr, end="")
        print(
            "\n编号是文档间互相引用的地址，一号两份会让所有 `docs/NNN` 引用指向歧义。\n"
            "\n下一个可用号请问这条命令（它对着远端 main 判，不是对着你的分支）：\n"
            "    python3 DataMiner/tools/next-docs-number.py\n"
            "起草时空着的号，合并时可能已被并发的 PR 占掉（本条已复发十一次），\n"
            "所以取号后到合并前若隔了较长时间，合并前再跑一次。\n"
            "改号时记得一并改：① 文档自身标题；② 其它文档与代码注释里的引用；\n"
            "③ 脚本里硬编码的路径（如 R2 守卫的 manifest 路径）。",
            file=sys.stderr,
        )
        return 1

    if mismatches:
        print("编号文档的文件名与标题对不上：", file=sys.stderr)
        for text in mismatches:
            print(f"  {text}", file=sys.stderr)
        print(
            "\n改号要动文件名与首行标题两处，只动一处照样能过撞号检查——文件名唯一了，\n"
            "标题还写着旧号。而旧号往往是另一份真实存在的文档，于是「NNN 号文」\n"
            "这个称呼同时指向两份东西。改标题即可；标题不带编号也是允许的。",
            file=sys.stderr,
        )
        return 1

    if resolved:
        print(
            "历史撞号已解决，请从 LEGACY_COLLISIONS 里删掉对应行（基线只许缩不许长）：",
            file=sys.stderr,
        )
        for text in resolved:
            print(f"  - {text}", file=sys.stderr)
        return 1

    total = sum(
        len(numbered_entries(REPO_ROOT / relative)) for relative in NUMBERED_DOC_DIRS
    )
    print(
        f"编号文档无新增撞号：{len(NUMBERED_DOC_DIRS)} 个目录、{total} 个编号"
        f"（历史撞号 {len(LEGACY_COLLISIONS)} 处在基线内）"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

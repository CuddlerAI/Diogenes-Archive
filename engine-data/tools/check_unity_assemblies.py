#!/usr/bin/env python3
"""Unity 程序集布局的静态闸：拦「写了却永远不会跑的测试」。

仓库里没有任何 CI 编译或运行 C#——engine-data 有 1.1 万行 Unity 代码，
自动化验证是零。全量方案要 Unity 许可证与 game-ci runner，是另一个决定；
本闸只做**不需要 Unity 就能判**的那部分，而它恰好覆盖了最贵的一种失效。

## 为什么是这两条

**T1 · 带 `[Test]` / `[UnityTest]` 的文件必须落在引用了 `UnityEngine.TestRunner`
的 asmdef 下。** Unity 的测试运行器只发现这类程序集里的测试；放在别处的测试
**不报错、不警告、也不运行**——它在仓库里看着像覆盖率，实际是零。

实证：`Samples~/MegacityS0/S0CaptureSmokeTest.cs` 与
`Samples~/Megacity/Tests/Runtime/S0CaptureSmokeTest.cs` **逐字节相同**
（同一个 sha256）。前者由 #436 引入，#445 把它复制进 `DiogenesCapture.PlayTests`
时没删原件，于是那份孤儿在无 asmdef 的目录里躺着，从未被编译、从未被执行。
本闸就是为了让下一次复制粘贴当场红。

**T2 · asmdef 的 `references` 必须解析得到。** 引用一个不存在的程序集名，
Unity 只在 Editor 控制台里抱怨一句，而 CI 里没人看控制台；后果是该程序集
少了一段依赖，编译错误要等到有人打开工程才出现。

## 明确不做的事

- 不检查语法、不做类型检查——那需要 Roslyn 或 Unity，属另一层。
- 不要求每个 .cs 都有 asmdef。没有 asmdef 的文件进预定义程序集
  （`Assembly-CSharp` / `Assembly-CSharp-Editor`），对编辑器工具是**合法且常见**的做法；
  已核 `DiogenesCapture.Factory` 的 `autoReferenced` 为 true，
  `Samples~/Factory/Editor/*.cs` 因此仍能引用到运行时。把它判红是风格意见，不是正确性。
"""

from __future__ import annotations

import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

#: Unity 自带或由官方包提供、不在本仓库的程序集。带 `Unity` 前缀的一律放行——
#: 逐个枚举 Unity 的包程序集会随 Unity 版本漂移，维护成本高于它挡住的错误。
BUILTIN_PREFIXES = ("Unity", "com.unity.")

TEST_ATTR = re.compile(r"\[\s*(?:UnityTest|Test|TestCase|TestCaseSource)\b")
TEST_RUNNER_REF = "UnityEngine.TestRunner"


def load_asmdefs(root: Path) -> dict[str, tuple[Path, dict]]:
    out: dict[str, tuple[Path, dict]] = {}
    for path in sorted(root.rglob("*.asmdef")):
        try:
            data = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as error:
            print(f"::error file={path}::asmdef 无法解析：{error}")
            raise SystemExit(1) from error
        name = data.get("name")
        if not name:
            print(f"::error file={path}::asmdef 缺 name 字段")
            raise SystemExit(1)
        if name in out:
            print(f"::error file={path}::asmdef 名字重复：{name}（另一处 {out[name][0]}）")
            raise SystemExit(1)
        out[name] = (path, data)
    return out


def owning_asmdef(file: Path, asmdefs: dict[str, tuple[Path, dict]]) -> str | None:
    """Unity 的归属规则：最近的**祖先**目录里的 asmdef。"""
    best: tuple[str, Path] | None = None
    for name, (path, _) in asmdefs.items():
        directory = path.parent
        try:
            file.relative_to(directory)
        except ValueError:
            continue
        if best is None or len(directory.parts) > len(best[1].parts):
            best = (name, directory)
    return best[0] if best else None


def references_of(data: dict) -> list[str]:
    # asmdef 允许 `GUID:<hex>` 形式；GUID 引用无法在不解析 .meta 的情况下核对，跳过。
    return [r for r in (data.get("references") or []) if not r.startswith("GUID:")]


def main() -> int:
    asmdefs = load_asmdefs(ROOT)
    problems = 0

    # T1：测试必须在测试运行器看得见的程序集里
    for file in sorted(ROOT.rglob("*.cs")):
        text = file.read_text(encoding="utf-8", errors="replace")
        if not TEST_ATTR.search(text):
            continue
        owner = owning_asmdef(file, asmdefs)
        rel = file.relative_to(ROOT.parent)
        if owner is None:
            print(f"::error file={rel}::带测试特性但不在任何 asmdef 下——"
                  f"Unity 测试运行器不会发现它，这个测试永远不会跑")
            problems += 1
            continue
        if TEST_RUNNER_REF not in references_of(asmdefs[owner][1]):
            print(f"::error file={rel}::所属程序集 {owner} 未引用 {TEST_RUNNER_REF}——"
                  f"测试运行器不会发现它")
            problems += 1

    # T2：引用必须解析得到
    for name, (path, data) in sorted(asmdefs.items()):
        for ref in references_of(data):
            if ref in asmdefs or ref.startswith(BUILTIN_PREFIXES):
                continue
            rel = path.relative_to(ROOT.parent)
            print(f"::error file={rel}::{name} 引用了解析不到的程序集 '{ref}'")
            problems += 1

    n_tests = sum(1 for f in ROOT.rglob("*.cs")
                  if TEST_ATTR.search(f.read_text(encoding="utf-8", errors="replace")))
    if problems:
        print(f"\n{problems} 个问题（asmdef {len(asmdefs)} 个，含测试的文件 {n_tests} 个）")
        return 1
    print(f"Unity 程序集布局 OK：asmdef {len(asmdefs)} 个，"
          f"含测试的文件 {n_tests} 个全部落在测试运行器可见的程序集里")
    return 0


if __name__ == "__main__":
    sys.exit(main())

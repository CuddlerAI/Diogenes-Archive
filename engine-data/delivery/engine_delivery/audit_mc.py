"""审计一棵 MC 交付树的**形态**：路径文法、12 文件、游戏名一致性、组名唯一。

输入可以是本地目录，也可以是一份对象键清单（每行一个 key）——后者是为了直接审
R2/TOS 上的前缀而不必把几百 GB 拉回来。两种输入走同一套判据，因为形态问题
本来就只需要看键名和几个小 json。
"""

import json
from dataclasses import dataclass, field
from pathlib import Path
from typing import Dict, List, Optional, Sequence

from .spec import (MC_DATE_RE, MC_GROUP_RE, MC_INTERNAL_FILES, MC_TOP,
                   missing_and_extra)

OK = "OK"
BAD = "BAD"


@dataclass
class GroupFinding:
    key: str                       # 次级文件夹的相对路径
    problems: List[str] = field(default_factory=list)
    notes: List[str] = field(default_factory=list)

    @property
    def status(self) -> str:
        return BAD if self.problems else OK

    def to_dict(self) -> dict:
        return {"key": self.key, "status": self.status,
                "problems": self.problems, "notes": self.notes}


@dataclass
class AuditReport:
    root: str
    groups: List[GroupFinding] = field(default_factory=list)
    orphans: List[str] = field(default_factory=list)   # 不落在任何次级文件夹下的键

    @property
    def verdict(self) -> str:
        if self.orphans or any(g.status == BAD for g in self.groups):
            return BAD
        return OK

    def to_dict(self) -> dict:
        return {
            "root": self.root,
            "verdict": self.verdict,
            "groups_total": len(self.groups),
            "groups_bad": sum(1 for g in self.groups if g.status == BAD),
            "orphan_keys": self.orphans,
            "groups": [g.to_dict() for g in self.groups],
        }


def _split(key: str) -> Optional[tuple]:
    """把一个对象键切成 (prefix, game, mmdd, group, filename)。切不出来返回 None。

    `2MultiCamera` 之前允许有任意前缀（我们用 `internal-validation/` 隔离 UCL 场景），
    但**它之后的四段是固定的**，多一层少一层都会让入库路径对不上。
    """
    parts = key.split("/")
    if MC_TOP not in parts:
        return None
    i = parts.index(MC_TOP)
    rest = parts[i + 1:]
    if len(rest) != 4:
        return None
    game, mmdd, group, filename = rest
    return "/".join(parts[:i]), game, mmdd, group, filename


def audit_keys(root: str, keys: Sequence[str],
               sysinfo: Optional[Dict[str, dict]] = None) -> AuditReport:
    """`keys` 为相对 root 的对象键；`sysinfo` 可选，键为次级文件夹相对路径。"""
    rep = AuditReport(root=root)
    buckets: Dict[str, List[str]] = {}
    for k in keys:
        s = _split(k)
        if s is None:
            rep.orphans.append(k)
            continue
        prefix, game, mmdd, group, filename = s
        gk = "/".join(x for x in (prefix, MC_TOP, game, mmdd, group) if x)
        buckets.setdefault(gk, []).append(filename)

    seen_groups: Dict[str, str] = {}
    for gk in sorted(buckets):
        f = GroupFinding(key=gk)
        parts = gk.split("/")
        game, mmdd, group = parts[-3], parts[-2], parts[-1]

        m = MC_GROUP_RE.match(group)
        if m is None:
            f.problems.append(
                f"组名 {group!r} 不合文法 YYYY_MM_DD_<场景拼音>_<两位序号>"
                "（如 2026_03_11_xingqiuchongqi_01）")
        else:
            if MC_DATE_RE.match(mmdd) is None:
                f.problems.append(f"日期目录 {mmdd!r} 不是 MMDD")
            elif mmdd != f"{m.group(2)}{m.group(3)}":
                f.problems.append(
                    f"日期目录 {mmdd} 与组名里的日期 {m.group(2)}{m.group(3)} 不一致")
            prev = seen_groups.get(group)
            if prev is not None:
                f.problems.append(f"组名与 {prev} 重复——次级文件夹须全局唯一")
            seen_groups[group] = gk

        missing, extra = missing_and_extra(buckets[gk])
        if missing:
            f.problems.append(f"缺必要文件 {len(missing)}/12：{', '.join(missing)}")
        internal = [x for x in extra if x in MC_INTERNAL_FILES]
        other = [x for x in extra if x not in MC_INTERNAL_FILES]
        if internal:
            f.problems.append(
                "混入我方内部产物（不属于 69 号 §1 的 12 必要文件，出门前须剔除）："
                + ", ".join(internal))
        if other:
            f.problems.append("混入未知文件：" + ", ".join(other))

        si = (sysinfo or {}).get(gk)
        if si is not None:
            declared = si.get("gameProcessName")
            if declared and declared != game:
                f.problems.append(
                    f"路径里的游戏名 {game!r} ≠ systeminfo.gameProcessName {declared!r}")
        else:
            f.notes.append("未提供 systeminfo，游戏名一致性未核")
        rep.groups.append(f)
    return rep


def audit_dir(root) -> AuditReport:
    root = Path(root)
    keys, sysinfo = [], {}
    for p in sorted(root.rglob("*")):
        if not p.is_file():
            continue
        rel = p.relative_to(root).as_posix()
        keys.append(rel)
        if p.name == "systeminfo_1.json":
            s = _split(rel)
            if s is not None:
                gk = "/".join(x for x in (s[0], MC_TOP, s[1], s[2], s[3]) if x)
                try:
                    sysinfo[gk] = json.loads(p.read_text(encoding="utf-8"))
                except (OSError, json.JSONDecodeError):
                    pass
    return audit_keys(str(root), keys, sysinfo)


def render(rep: AuditReport) -> str:
    lines = [f"== 交付形态审计 · {rep.root} · {len(rep.groups)} 个次级文件夹 =="]
    for g in rep.groups:
        lines.append(f"[{g.status}] {g.key}")
        for p in g.problems:
            lines.append(f"    ✗ {p}")
        for n in g.notes:
            lines.append(f"    · {n}")
    if rep.orphans:
        lines.append(f"[BAD] {len(rep.orphans)} 个键不落在 "
                     f"{MC_TOP}/<game>/<MMDD>/<group>/ 结构下：")
        for k in rep.orphans[:10]:
            lines.append(f"    ✗ {k}")
    bad = sum(1 for g in rep.groups if g.status == BAD)
    lines.append(f"-- 判定 {rep.verdict}（{bad}/{len(rep.groups)} 个次级文件夹形态不合格）")
    return "\n".join(lines)

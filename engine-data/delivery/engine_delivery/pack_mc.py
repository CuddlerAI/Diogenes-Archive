"""采集目录 → MC 交付目录：**恰好 12 个文件**，外加一行台账。

采集目录里除 12 必要文件之外还躺着 `plan.json` / `covis.json` / `dyn_metrics.json` /
`provenance.json` / `audio_mix.wav`——QC 要用它们，需求方不要，而且不该给：
`plan.json` 是我方轨迹生成参数，`provenance.json` 里带着本机绝对路径与场景授权标记。
把整个采集目录 `rclone copy` 上去就会把这些一起送出门（R2 桶里那条
`2026_08_04_megacity_01` 正是这样，16 个对象里 4 个是内部件）。

所以打包是**白名单拷贝**，不是「排除几个已知文件」——将来采集侧多产出一个中间件，
黑名单会漏，白名单不会。
"""

import json
import shutil
from dataclasses import dataclass, field
from pathlib import Path
from typing import List, Optional

from .spec import MC_REQUIRED_FILES, mc_relpath


@dataclass
class PackResult:
    group: str
    dest: str
    copied: List[str] = field(default_factory=list)
    skipped_internal: List[str] = field(default_factory=list)
    problems: List[str] = field(default_factory=list)

    @property
    def ok(self) -> bool:
        return not self.problems

    def to_dict(self) -> dict:
        return {"group": self.group, "dest": self.dest, "copied": self.copied,
                "skipped_internal": self.skipped_internal,
                "problems": self.problems, "ok": self.ok}


def pack_clip(src, out_root, game: str, group: str, prefix: str = "",
              qc_verdict: Optional[str] = None, dry_run: bool = False) -> PackResult:
    """把 `src` 里的 12 必要文件拷进 `out_root/<mc_relpath>/`。

    `qc_verdict` 是调用方跑完 `engine_qc check-clip` 的判定。不是 PASS 就不打包——
    交付目录里出现过的东西就该是能出门的东西，用「先打包再补 QC」的顺序，
    最后总会有一条没补上的留在那里。
    """
    src = Path(src)
    rel = mc_relpath(game, group, prefix)
    dest = Path(out_root) / rel
    res = PackResult(group=group, dest=str(dest))

    if qc_verdict is not None and qc_verdict != "PASS":
        res.problems.append(
            f"engine_qc 判定为 {qc_verdict}，不予打包（交付目录只放能出门的数据）")
        return res

    for name in MC_REQUIRED_FILES:
        p = src / name
        if not p.is_file():
            res.problems.append(f"缺必要文件 {name}")
    if res.problems:
        return res

    for p in sorted(src.iterdir()):
        if p.is_file() and p.name not in MC_REQUIRED_FILES:
            res.skipped_internal.append(p.name)

    if not dry_run:
        dest.mkdir(parents=True, exist_ok=True)
        for name in MC_REQUIRED_FILES:
            shutil.copy2(src / name, dest / name)
    res.copied = list(MC_REQUIRED_FILES)
    return res


def ledger_row(res: PackResult, game: str, scene: str, prefix: str = "") -> dict:
    """交付表的一行（《…正式交付数据 --对外》模板按日期分 sheet）。

    只填能从数据本身读出来的列。场景描述、轨迹类目这类需要人判断的列留空，
    由填表人补——预填一个猜出来的值比留空更糟。
    """
    return {
        "组名": res.group,
        "游戏": game,
        "场景": scene,
        "上传路径": res.dest,
        "文件数": len(res.copied),
        "状态": "OK" if res.ok else "BLOCKED",
        "备注": "；".join(res.problems),
    }


#: 元信息模板的实际列（每款游戏一份，不是每条一份）。
#: 列名按模板原件 A–S 逐列转录（docs/69 §10.3；模板 2026-08-04 起可读，此前只有
#: 68 号 §6.6 截图可见的前 10 列）。最后两列「可提供主题/滤镜变种」仅 multi-camera2
#: sheet 有，SFM 版 sheet 到「画风维度」为止。
#: 值一律留空——这份骨架的作用是保证不漏项，不是替填表人回答。
GAMEINFO_FIELDS = [
    "gameprocessName", "游戏名字", "游戏类型描述", "视角配置", "相机位置是否固定", "相机位置",
    "画面分辨率", "键鼠映射规则", "键盘对应规则+特殊配置", "鼠标对应规则+特殊配置",
    "空间属性", "世界时间设定", "player 玩家角色类型", "敌人类型", "摄像机距离",
    "玩家交互方式", "画风维度", "可提供主题变种", "可提供滤镜变种",
]


def gameinfo_skeleton(game: str, resolution: str = "") -> dict:
    d = {k: "" for k in GAMEINFO_FIELDS}
    d["gameprocessName"] = game
    d["游戏名字"] = game
    d["画面分辨率"] = resolution
    return d


def write_gameinfo(path, game: str, resolution: str = "") -> Path:
    path = Path(path)
    path.write_text(json.dumps(gameinfo_skeleton(game, resolution),
                               ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    return path

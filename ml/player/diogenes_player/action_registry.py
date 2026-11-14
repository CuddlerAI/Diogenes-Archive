"""语义动作 registry（research/07 §2）：从 DataMiner mapping.json 抽取语义词表。

DataMiner 尚无 `packages/contract` 中央 registry；本模块从各 mapping 的 bindings 抽取
语义并集，固化排序词表 + 训练类过滤，作为 BC 动作头的目标空间。`packages/contract`
落地后本模块切换为消费其生成物。

训练类过滤（research/07 §2.2）：ui.* / communication.* 前缀默认不进损失
（记分板/聊天/ping 对 BC 无意义且会学出开菜单行为）。camera.* 走独立回归头，不进 multi-hot。
"""
from __future__ import annotations

import json
from dataclasses import dataclass
from pathlib import Path

# 默认排除前缀：非操作性/会话外语义
EXCLUDE_PREFIXES = ("ui.", "communication.")
# camera 走独立分箱头，不在 multi-hot 词表里
CAMERA_SEMANTICS = ("camera.yaw", "camera.pitch")


def semantics_from_mapping(mapping_path: str | Path) -> list[str]:
    """从单个 mapping.json 抽取去重排序的语义列表（含 camera，未过滤）。"""
    m = json.loads(Path(mapping_path).read_text(encoding="utf-8"))
    sems = {b["semantic"] for b in m.get("bindings", []) if "semantic" in b}
    return sorted(sems)


def trainable_actions(semantics: list[str]) -> list[str]:
    """过滤到 BC multi-hot 目标：去掉 camera 与排除前缀，排序。"""
    return sorted(
        s for s in semantics
        if s not in CAMERA_SEMANTICS
        and not s.startswith(EXCLUDE_PREFIXES)
    )


@dataclass(frozen=True)
class ActionVocab:
    """一个固化的动作词表版本。checkpoint 记录其 name+version+actions 以保证可复现。"""
    name: str
    version: str
    actions: tuple[str, ...]          # multi-hot 目标（有序）
    all_semantics: tuple[str, ...]    # 词表全集（含 camera/excluded，供审计）

    @property
    def n_actions(self) -> int:
        return len(self.actions)

    def index_of(self, action: str) -> int:
        return self.actions.index(action)

    def to_multihot(self, present: list[str]):
        import numpy as np
        out = np.zeros(self.n_actions, dtype=np.float32)
        idx = {a: i for i, a in enumerate(self.actions)}
        for a in present:
            j = idx.get(a)
            if j is not None:
                out[j] = 1.0
        return out

    def as_dict(self) -> dict:
        return {"name": self.name, "version": self.version,
                "actions": list(self.actions), "all_semantics": list(self.all_semantics)}


def build_vocab(mapping_paths: list[str | Path], *, name: str,
                version: str) -> ActionVocab:
    """从一组 mapping.json 构建语义并集词表（跨游戏 generalist 词表用多个 mapping）。"""
    allsem: set[str] = set()
    for p in mapping_paths:
        allsem.update(semantics_from_mapping(p))
    all_sorted = sorted(allsem)
    return ActionVocab(name=name, version=version,
                       actions=tuple(trainable_actions(all_sorted)),
                       all_semantics=tuple(all_sorted))


def vocab_from_dict(d: dict) -> ActionVocab:
    return ActionVocab(name=d["name"], version=d["version"],
                       actions=tuple(d["actions"]),
                       all_semantics=tuple(d.get("all_semantics", d["actions"])))


# 仓库内 COD catalog 的相对定位（供默认词表；缺失时调用方显式传路径）
_REPO_ROOT = Path(__file__).resolve().parents[3]
COD_MAPPING = _REPO_ROOT / "DataMiner/config/catalogs/dev/cod-steam-release-test/mapping.json"


def default_cod_vocab() -> ActionVocab:
    mapping = json.loads(COD_MAPPING.read_text(encoding="utf-8"))
    return build_vocab(
        [COD_MAPPING], name="cod-steam", version=str(mapping["version"]))

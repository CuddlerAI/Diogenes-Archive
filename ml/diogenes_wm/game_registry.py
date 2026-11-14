"""DataMiner 可采集游戏注册表 —— catalog 事实 + 治理裁定的唯一汇合点.

事实(game_id / mapping / 语义词表 / taxonomy)**动态读自** DataMiner dev catalog
(`DataMiner/config/catalogs/dev/*/`), 不在这里抄一份等着漂移; 治理裁定(授权类别 /
具结车道资格)是**静态 overlay**, 因为授权不住在 catalog 里 —— 它住在文书里
(research/50 §4.1、halo/c1-c4-partner-confirmation.md、research/10)。

测试 `test_game_registry.py` 强制两边闭合: catalog 新增条目而 overlay 没登记 →
测试失败。这是有意的: 新游戏能不能取数是治理判断, 不允许默认继承。

## 授权类别(license_class)与具结车道政策(attested_policy)

| 类别 | 依据 | 具结车道 |
|---|---|---|
| halo_partner | C-1 授权按作品成立, 覆盖 Halo 系(Infinite 为主、MCC 为辅, 见 c1-c4 确认函) | partner_trainable |
| microsoft_family | 作品归微软(research/10: Activision/ZeniMax/Bethesda/XGS), 但 C-1 文本只署 Halo | blocked_pending_c1_extension |
| third_party | 无任何已落纸授权判定 | blocked_third_party |
| synthetic_first_party | 合成数据, 契约里 rights=synthetic 天然可训 | not_applicable |
| ungranted_blocked | 采集本身未授权(catalog 里根本不该有该条目) | blocked_entirely |

microsoft_family 的"pending"是真的 pending: doc 29 对齐补记曾建议 C-1 按"微软名下
多款游戏"表述, **但已发出的函以 Halo 为主体** —— 扩展要新文书, 不能由代码推定。
COD 的 owner 具结先例(cod_adapter)是 per-title 裁决 + 专用工具, 不在此扩权。
"""
from __future__ import annotations

import json
from dataclasses import dataclass
from functools import lru_cache
from pathlib import Path

__all__ = [
    "GameEntry", "load_registry", "lane_policy", "qc_channels",
    "HALO_PARTNER_GAME_IDS", "LICENSE_CLASSES",
]

#: `ungranted_blocked` 今天零条目: 唯一那条(原神本地测试)随它的 catalog bundle 一起
#: 删除(issue #855 PR 2)。类别保留, 因为它是一条**通用**裁决("采集本身未授权"), 不是
#: 为某款游戏造的特例; halo_adapter 的 fail-closed 分支也继续按它判。
LICENSE_CLASSES = ("halo_partner", "microsoft_family", "third_party",
                   "synthetic_first_party", "ungranted_blocked")

#: game_id → (license_class, 依据一句话)。catalog 每个条目必须在这里出现。
GOVERNANCE: dict[str, tuple[str, str]] = {
    "halo-infinite-steam-release-test": ("halo_partner", "C-1 主体作品(Infinite 为主)"),
    "halo-mcc-steam-release-test": ("halo_partner", "C-1 辅助作品(MCC 为辅, 闭环评估环境)"),
    "halo-reach-mcc-s0": ("halo_partner", "MCC 子条目(Reach), 随 MCC 覆盖"),
    "call-of-duty-steam-release-test": (
        "microsoft_family", "Activision→微软(research/10); COD 取数走 cod_adapter 的 per-title 具结"),
    "wolfenstein2-steam": ("microsoft_family", "ZeniMax→微软 2021(research/10)"),
    "wolfenstein2-steam-release-test": ("microsoft_family", "同 wolfenstein2-steam"),
    "fallout4-steam-release-test": ("microsoft_family", "Bethesda→微软(depth 线主力)"),
    "age-of-empires-4-steam": ("microsoft_family", "Xbox Game Studios[待核: 仓库文档未单独核过]"),
    "bioshock-infinite-steam-e2e": ("third_party", "Take-Two(research/10 已核), 需单独授权"),
    "bioshock-infinite-steam-release-test": ("third_party", "同上"),
    "a-plague-tale-innocence-steam-release-test": ("third_party", "Asobo/Focus[待核]"),
    "elden-ring-nightreign-steam-release-test": ("third_party", "FromSoftware/Bandai[待核]"),
    "hades-ii-steam-release-test": ("third_party", "Supergiant[待核]"),
    "euro-truck-simulator-2-steam": ("third_party", "SCS Software[待核]; depth 线在用"),
    "mount-and-blade-2-bannerlord-steam": ("third_party", "TaleWorlds[待核]"),
    "nioh2-steam": ("third_party", "Koei Tecmo[待核]"),
    "total-war-warhammer-iii-steam-release-test": ("third_party", "Sega/CA + GW IP[待核]"),
    "wuchang-fallen-feathers-steam": ("third_party", "Leenzee/505[待核]"),
    # --- 2026-08-15/16 catalog 扩充批（#822/#827），归属按发行方判定, [待核]=仓库未单独调研 ---
    "deathloop-pc": ("microsoft_family", "Arkane/Bethesda→微软[待核]"),
    "doom-eternal-pc": ("microsoft_family", "id Software/Bethesda→微软[待核]"),
    "prey-2017-pc": ("microsoft_family", "Arkane/Bethesda→微软[待核]"),
    "starfield-pc": ("microsoft_family", "Bethesda→微软[待核]"),
    "forza-horizon-5-pc": ("microsoft_family", "Playground/Xbox Game Studios[待核]"),
    "indiana-jones-great-circle-pc": (
        "microsoft_family", "MachineGames/Bethesda 发行→微软, 但 IP 属 Lucasfilm/Disney"
        "——即便 C-1 扩展到微软名下作品, 三方 IP 授权内容仍需单独判定[待核]"),
    "atomic-heart-pc": ("third_party", "Mundfish[待核]"),
    "assassins-creed-iii-remastered-steam": ("third_party", "Ubisoft[待核]"),
    "avatar-frontiers-of-pandora-pc": ("third_party", "Ubisoft/Massive + Disney IP[待核]"),
    "cyberpunk-2077-steam": ("third_party", "CD Projekt Red[待核]; depth 线在用"),
    "dying-light-pc": ("third_party", "Techland[待核]"),
    "far-cry-5-pc": ("third_party", "Ubisoft[待核]"),
    "gta5-legacy-pc": ("third_party", "Rockstar/Take-Two[待核]"),
    "red-dead-redemption-2-pc": ("third_party", "Rockstar/Take-Two[待核]"),
    "resident-evil-village-pc": ("third_party", "Capcom[待核]"),
    "titanfall-2-pc": ("third_party", "Respawn/EA[待核]——EA 不属微软"),
    # --- 2026-09-11 KCD2 随 #933/#934 进 catalog 时漏了这里, 09-11 已有生产 clip 落桶,
    #     周报把它归成"?"档才发现（docs/168 §7）。ml/tests 的闭合测试不在 ml-ci 里, 没拦住。
    "kingdom-come-deliverance-2-steam": ("third_party", "Warhorse/Deep Silver(Plaion)[待核]"),
    "synthetic-win32-dev": ("synthetic_first_party", "合成窗口, 一方构造"),
}

_ATTESTED_BY_CLASS = {
    "halo_partner": "partner_trainable",
    "microsoft_family": "blocked_pending_c1_extension",
    "third_party": "blocked_third_party",
    "synthetic_first_party": "not_applicable",
    "ungranted_blocked": "blocked_entirely",
}

#: 行为地板的攻击通道候选, 按各 mapping 的实际叫法排优先级
_ATTACK_CANDIDATES = (
    "combat.primary_attack", "combat.attack", "combat.light_attack",
    "combat.quick_attack", "combat.fire_left", "ui.select_or_command",
    "selection.primary",
)


@dataclass(frozen=True)
class GameEntry:
    game_id: str
    catalog_entry: str          # dev catalog 目录名
    mapping_id: str
    mapping_version: str
    taxonomy: str
    semantics: tuple[str, ...]  # mapping bindings 的语义全集
    license_class: str
    license_note: str
    attested_policy: str

    @property
    def has_combat_aim(self) -> bool:
        return "combat.aim" in self.semantics


def _repo_root() -> Path:
    return Path(__file__).resolve().parents[2]


@lru_cache(maxsize=4)
def load_registry(catalog_root: str | None = None) -> dict[str, GameEntry]:
    """读 dev catalog 全部条目并套上治理 overlay。overlay 缺条目→KeyError(fail-closed)。"""
    root = Path(catalog_root) if catalog_root else (
        _repo_root() / "DataMiner" / "config" / "catalogs" / "dev")
    if not root.is_dir():
        raise FileNotFoundError(
            f"dev catalog 不在 {root}; 仓库外运行请显式传 catalog_root")
    out: dict[str, GameEntry] = {}
    for d in sorted(p for p in root.iterdir() if p.is_dir()):
        gp = d / "game-profile.json"
        if not gp.exists():
            continue
        g = json.loads(gp.read_text(encoding="utf-8"))
        gid = g["game_id"]
        if gid not in GOVERNANCE:
            raise KeyError(
                f"catalog 条目 {d.name}(game_id={gid}) 未在 game_registry.GOVERNANCE "
                "登记 —— 新游戏能否取数是治理判断, 不允许默认继承。")
        mp = d / "mapping.json"
        m = json.loads(mp.read_text(encoding="utf-8")) if mp.exists() else {}
        sems = tuple(sorted({b["semantic"] for b in m.get("bindings", [])}))
        cls, note = GOVERNANCE[gid]
        out[gid] = GameEntry(
            game_id=gid, catalog_entry=d.name,
            mapping_id=m.get("mapping_id", g.get("mapping_id", "?")),
            mapping_version=m.get("version", "?"),
            taxonomy=m.get("taxonomy", {}).get("content", "?"),
            semantics=sems, license_class=cls, license_note=note,
            attested_policy=_ATTESTED_BY_CLASS[cls],
        )
    return out


def lane_policy(game_id: str, catalog_root: str | None = None) -> GameEntry:
    """按 game_id 取条目; 未注册的 game_id 视为最严(拒绝), 并说明去处。"""
    reg = load_registry(catalog_root)
    if game_id not in reg:
        raise KeyError(
            f"game_id {game_id!r} 不在 dev catalog/注册表内。已注册: "
            f"{sorted(reg)}。新作品先进 catalog + GOVERNANCE, 再谈取数。")
    return reg[game_id]


HALO_PARTNER_GAME_IDS: frozenset[str] = frozenset(
    gid for gid, (cls, _) in GOVERNANCE.items() if cls == "halo_partner")


def qc_channels(entry: GameEntry) -> tuple[str, ...]:
    """给 halo_qc 的 channel_completeness 推导本游戏的行为地板通道.

    推导规则(不是阈值裁决): 四向移动里 mapping 实际有的 + 一个攻击/主交互通道 +
    combat.aim(仅当该游戏词表里有)。Halo 系推导结果与 wm01 冻结六通道一致 ——
    test_game_registry 里有一致性断言, 断了说明两边有人单方面改了词表。
    """
    sems = set(entry.semantics)
    chans = [c for c in ("move.forward", "move.backward", "move.left", "move.right")
             if c in sems]
    if not chans:  # 非 WASD 游戏(RTS/驾驶): 用 mapping 里的移动/指针语义兜底
        chans = sorted(c for c in sems
                       if c.startswith(("move.", "pointer.", "grid.")))[:4]
    if "combat.aim" in sems:  # 顺序对齐 wm01 冻结六通道: aim 在 attack 之前
        chans.append("combat.aim")
    for cand in _ATTACK_CANDIDATES:
        if cand in sems:
            chans.append(cand)
            break
    return tuple(chans)

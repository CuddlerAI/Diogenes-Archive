"""校验新游戏 dev catalog（Halo MCC/Infinite、Wolfenstein II、BioShock、Elden Ring）。

catalog schema v2（docs/57 批次二）：manifest + 四本册子（game / recording /
mapping / device），目录内引用只按 id、无 catalog_mode。

- catalog.json 的 content_hash 与同级文件字节自洽（DataMiner Rust 侧靠此 fail-closed）
- mapping.json 可被 action_registry 消费；并集词表覆盖跨模态语义族
- game-profile 的 input_contract 与 mapping 键集一致（VK ↔ scan_code）
- Elden Ring taxonomy = third_person（模态断层，research/10 §6.1）
"""
import hashlib
import json
from pathlib import Path

import pytest

from diogenes_player.action_registry import (
    build_vocab,
    semantics_from_mapping,
    trainable_actions,
)

REPO = Path(__file__).resolve().parents[3]
CATALOGS = REPO / "DataMiner/config/catalogs/dev"
NEW_GAMES = [
    "halo-mcc-steam-release-test",
    "halo-infinite-steam-release-test",
    "wolfenstein2-steam-release-test",
    "bioshock-infinite-steam-release-test",
    "elden-ring-nightreign-steam-release-test",
    # S0 主条目：单作品（Reach），取代六合一的 halo-mcc（见 research/halo/s0-mcc-primary-revision.md）
    "halo-reach-mcc-s0",
]
# issue #855 PR 2 的目录生命周期分层：三份带 SUPERSEDED.md 的研究快照**冻结在 legacy
# schema、保持原字节**，其余活跃 bundle 断代到中性 schema。本文件同时覆盖两类，所以
# 下面的 schema 断言必须按类分派——把冻结快照当成"未迁移"来报错，正是 issue 明令
# 禁止的那种误判。
FROZEN_LEGACY_SNAPSHOTS = {
    "halo-mcc-steam-release-test",
    "wolfenstein2-steam-release-test",
    "bioshock-infinite-steam-release-test",
}
BUNDLE_FILES = ["catalog.json", "game-profile.json", "mapping.json",
                "recording-profile.json", "device-profile.json"]

# VK → PS/2 Set-1 make scancode（校验 game-profile↔mapping 一致性用）
VK_TO_SCAN = {
    87: 17, 65: 30, 83: 31, 68: 32, 81: 16, 69: 18, 82: 19, 84: 20, 89: 21,
    85: 22, 73: 23, 79: 24, 80: 25, 70: 33, 71: 34, 72: 35, 74: 36, 75: 37, 76: 38,
    90: 44, 88: 45, 67: 46, 86: 47, 66: 48, 78: 49, 77: 50,
    32: 57, 16: 42, 17: 29, 18: 56, 9: 15, 13: 28, 27: 1, 20: 58,
    49: 2, 50: 3, 51: 4, 52: 5, 53: 6, 54: 7, 55: 8, 56: 9, 57: 10, 48: 11,
    192: 41, 189: 12, 187: 13, 119: 66, 121: 68,
}

pytestmark = pytest.mark.skipif(not CATALOGS.exists(), reason="DataMiner catalogs 不在")


@pytest.mark.parametrize("game", NEW_GAMES)
def test_bundle_files_present(game):
    d = CATALOGS / game
    for f in BUNDLE_FILES:
        assert (d / f).exists(), f"{game} 缺 {f}"


@pytest.mark.parametrize("game", NEW_GAMES)
def test_catalog_sha_self_consistent(game):
    d = CATALOGS / game
    cat = json.loads((d / "catalog.json").read_text(encoding="utf-8"))
    frozen = game in FROZEN_LEGACY_SNAPSHOTS
    assert cat["schema_version"] == (
        "dm-catalog-bundle-2" if frozen else "dm-catalog-bundle-3")
    # v2 断代：渠道即模式，manifest 不再带 catalog_mode（docs/57 §3.3）
    assert "catalog_mode" not in cat

    def sha(fn):
        return "sha256:" + hashlib.sha256((d / fn).read_bytes()).hexdigest()

    pairs = [
        ("device-profile.json", cat["device_profiles"][0]),
        ("recording-profile.json", cat["recording_profiles"][0]),
        ("game-profile.json", cat["game_profiles"][0]),
        ("mapping.json", cat["mappings"][0]),
    ]
    for fn, entry in pairs:
        assert sha(fn) == entry["content_hash"], f"{game}/{fn} 哈希不符"
    if frozen:
        assert cat["non_trainable"] is True
    else:
        assert "non_trainable" not in cat


@pytest.mark.parametrize("game", NEW_GAMES)
def test_game_profile_training_classification_and_ids_resolve(game):
    d = CATALOGS / game
    gp = json.loads((d / "game-profile.json").read_text(encoding="utf-8"))
    mp = json.loads((d / "mapping.json").read_text(encoding="utf-8"))
    if game in FROZEN_LEGACY_SNAPSHOTS:
        assert gp["schema_version"] == "dm-game-profile-2"
        assert gp["non_trainable"] is True
    else:
        # 中性一代不声明训练分类，`non_trainable` 必须整个缺席（issue #855 §A.4）。
        assert gp["schema_version"] == "dm-game-profile-3"
        assert "non_trainable" not in gp
    assert gp["mapping_id"] == mp["mapping_id"]
    # 引用去版本化（docs/57 §3.2）：目录内只按 id，mapping_version 字段整个消失
    assert "mapping_version" not in gp
    # F8/F10 安全控制恒在 control_vks
    assert 119 in gp["input_contract"]["control_vks"]
    assert 121 in gp["input_contract"]["control_vks"]


@pytest.mark.parametrize("game", NEW_GAMES)
def test_input_contract_matches_mapping_keys(game):
    """game-profile.keyboard_vks 的每个键（除安全键）应有对应 scan_code 绑定在 mapping。"""
    d = CATALOGS / game
    gp = json.loads((d / "game-profile.json").read_text(encoding="utf-8"))
    mp = json.loads((d / "mapping.json").read_text(encoding="utf-8"))
    mapping_scans = {b["scan_code"] for b in mp["bindings"] if b.get("device") == "keyboard"}
    for vk in gp["input_contract"]["keyboard_vks"]:
        if vk in (119, 121):  # 安全键无绑定
            continue
        assert vk in VK_TO_SCAN, f"{game}: 未知 VK {vk}"
        assert VK_TO_SCAN[vk] in mapping_scans, f"{game}: VK {vk} 无 mapping 绑定"
    mouse_btns = {b["button"] for b in mp["bindings"] if "button" in b}
    assert set(gp["input_contract"]["mouse_buttons"]) == mouse_btns


@pytest.mark.parametrize("game", NEW_GAMES)
def test_mapping_builds_vocab(game):
    p = CATALOGS / game / "mapping.json"
    sems = semantics_from_mapping(p)
    assert sems, f"{game} 无语义"
    tr = trainable_actions(sems)
    assert tr, f"{game} 无可训练动作"
    # camera 走独立头，不进可训练 multi-hot
    assert all(not a.startswith("camera.") for a in tr)
    # 但 camera.yaw/pitch 应在全语义（鼠标 delta 绑定）
    assert "camera.yaw" in sems and "camera.pitch" in sems


def test_elden_ring_is_third_person_melee():
    """模态断层：ER 是唯一第三人称 action_rpg，且带近战 verb 群。"""
    mp = json.loads((CATALOGS / "elden-ring-nightreign-steam-release-test"
                     / "mapping.json").read_text(encoding="utf-8"))
    assert mp["taxonomy"]["view"] == "third_person"
    assert mp["taxonomy"]["content"] == "action_rpg"
    sems = {b["semantic"] for b in mp["bindings"]}
    for melee in ("combat.light_attack", "combat.heavy_attack", "combat.dodge_roll",
                  "combat.guard", "target.lock", "class_skill", "class_ultimate"):
        assert melee in sems, f"ER 缺近战/职业 verb {melee}"
    # 其它五款是第一人称 FPS
    for g in ("cod-steam-release-test", *[x for x in NEW_GAMES if "elden" not in x]):
        t = json.loads(
            (CATALOGS / g / "mapping.json").read_text(encoding="utf-8")
        )["taxonomy"]
        assert t["view"] == "first_person", g


def test_union_vocab_across_all_games():
    """并集词表（COD + 5 新）覆盖跨模态语义族。"""
    maps = [CATALOGS / "cod-steam-release-test" / "mapping.json"]
    maps += [CATALOGS / g / "mapping.json" for g in NEW_GAMES]
    v = build_vocab(maps, name="all-games", version="test")
    assert v.n_actions >= 40  # 六游戏并集显著大于单游戏
    # FPS 与近战两模态的代表 verb 都在
    for a in ("combat.primary_attack", "combat.aim",          # FPS
              "combat.light_attack", "combat.dodge_roll", "target.lock",  # ER 近战
              "combat.fire_left", "combat.fire_right",         # Wolfenstein 双持
              "power.cast",                                    # BioShock Vigor
              "equipment.use"):                                # Halo Infinite
        assert a in v.actions, f"并集缺 {a}"
    assert all(not a.startswith(("camera.", "ui.", "communication.")) for a in v.actions)


def test_halo_reach_s0_is_single_title_and_sprint_free():
    """Reach 没有独立冲刺（冲刺是护甲能力之一）；ability.use 必须绑 LShift。

    六合一的 halo-mcc mapping 声明了 move.sprint，那对 CE/H2/H3/ODST 是无中生有、
    对 Reach 是错误归类——S0 单作品条目必须不含它。
    """
    mp = json.loads((CATALOGS / "halo-reach-mcc-s0"
                     / "mapping.json").read_text(encoding="utf-8"))
    sems = {b["semantic"] for b in mp["bindings"]}
    assert "move.sprint" not in sems, "Reach 无独立冲刺"
    # ability.use 绑 LShift（scan 42），依 Halopedia MCC:Control_schemes
    ability = [b for b in mp["bindings"] if b.get("semantic") == "ability.use"]
    assert len(ability) == 1 and ability[0]["scan_code"] == 42
    assert mp["game"]["id"] == "halo-reach-mcc-s0"


def test_generic_halo_mcc_still_declares_sprint_and_is_flagged():
    """六合一条目仍存在（不删），但它声明的 move.sprint 正是 S0 不能用它的原因。

    这个测试是负例文档：若将来有人删掉 halo-mcc 的 sprint 绑定，
    应同步更新 README 与 s0-mcc-primary-revision.md §4 的说明。
    """
    mp = json.loads((CATALOGS / "halo-mcc-steam-release-test"
                     / "mapping.json").read_text(encoding="utf-8"))
    sems = {b["semantic"] for b in mp["bindings"]}
    assert "move.sprint" in sems

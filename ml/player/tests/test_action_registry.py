"""R0: 语义动作 registry —— COD 词表构建与过滤。"""
from pathlib import Path

import pytest

from diogenes_player.action_registry import (
    build_vocab, default_cod_vocab, trainable_actions, vocab_from_dict)

REPO = Path(__file__).resolve().parents[3]
COD = REPO / "DataMiner/config/catalogs/dev/cod-steam-release-test/mapping.json"

pytestmark = pytest.mark.skipif(not COD.exists(), reason="COD mapping 不在")


def test_cod_vocab_shape():
    v = default_cod_vocab()
    assert v.name == "cod-steam"
    assert v.n_actions == 26
    # 关键动作在
    for a in ("move.forward", "combat.primary_attack", "combat.reload", "combat.melee"):
        assert a in v.actions


def test_excludes_ui_comm_camera():
    v = default_cod_vocab()
    for a in v.actions:
        assert not a.startswith(("ui.", "communication."))
        assert not a.startswith("camera.")
    # 但全集里保留（供审计）
    assert any(a.startswith("camera.") for a in v.all_semantics)
    assert "ui.scoreboard" in v.all_semantics


def test_multihot_roundtrip():
    v = default_cod_vocab()
    hot = v.to_multihot(["move.forward", "combat.aim", "ui.map"])  # ui.map 被忽略
    assert hot.sum() == 2
    assert hot[v.index_of("move.forward")] == 1.0


def test_trainable_filter():
    assert "camera.yaw" not in trainable_actions(["camera.yaw", "move.forward"])
    assert "ui.map" not in trainable_actions(["ui.map", "combat.reload"])


def test_serialize_roundtrip():
    v = default_cod_vocab()
    v2 = vocab_from_dict(v.as_dict())
    assert v2.actions == v.actions
    assert v2.all_semantics == v.all_semantics


def test_union_across_mappings():
    """多 mapping 并集词表（generalist 路线）。synthetic + COD。"""
    syn = REPO / "DataMiner/config/catalogs/dev/synthetic/mapping.json"
    if not syn.exists():
        pytest.skip("synthetic mapping 不在")
    v = build_vocab([COD, syn], name="union", version="test")
    cod = default_cod_vocab()
    assert set(cod.actions) <= set(v.actions)  # COD 是并集子集

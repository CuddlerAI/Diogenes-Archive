"""契约黄金测试：contract.py 必须与 data_demo 数据集逐位一致。"""
import json
from pathlib import Path

import pytest

from diogenes_player import contract

REPO_ROOT = Path(__file__).resolve().parents[3]
DATASET = REPO_ROOT / "data_demo" / "dataset"


def _all_rows():
    for jl in sorted(DATASET.glob("*/*/c*/p0.jsonl")):
        with jl.open() as f:
            for line in f:
                yield jl, json.loads(line)


@pytest.mark.skipif(not DATASET.exists(), reason="data_demo dataset 不在")
def test_golden_pointer_bins_match_dataset():
    """全量 p0.jsonl: 重算 μ-law bins 必须等于存储的 pointerBins。"""
    n = 0
    for jl, row in _all_rows():
        bx, by = row["pointerBins"]
        # data_demo 是 legacy 刻度
        assert contract.encode_bin(row["pointerDx"], "legacy") == bx, (jl, row)
        assert contract.encode_bin(row["pointerDy"], "legacy") == by, (jl, row)
        n += 1
    assert n > 0


def test_mu_law_roundtrip_all_bins():
    for scheme in ("dm1-v2", "legacy"):
        for b in range(-contract.MAX_BIN, contract.MAX_BIN + 1):
            assert contract.encode_bin(contract.decode_bin(b, scheme), scheme) == b


def test_mu_law_legacy_golden():
    """legacy 刻度：data_demo 实测真值。"""
    assert contract.encode_bin(0, "legacy") == 0
    assert contract.encode_bin(60, "legacy") == 8
    assert contract.encode_bin(120, "legacy") == 9
    assert contract.encode_bin(180, "legacy") == 10
    assert contract.encode_bin(99999, "legacy") == 10   # 钳位
    assert contract.encode_bin(-99999, "legacy") == -10


def test_mu_law_dm1v2_golden():
    """dm1-v2 刻度：capture-core projection.rs 测试向量。"""
    assert contract.encode_bin(0, "dm1-v2") == 0
    assert contract.encode_bin(120, "dm1-v2") == 8      # projection.rs:548
    assert contract.encode_bin(-14, "dm1-v2") == -2     # projection.rs:551
    assert contract.encode_bin(200, "dm1-v2") == 10
    assert contract.encode_bin(-200, "dm1-v2") == -10


def test_schemes_differ():
    """两代刻度不可混用：同 delta 给不同 bin。"""
    assert contract.encode_bin(60, "legacy") != contract.encode_bin(60, "dm1-v2")


def test_unknown_scheme_raises():
    import pytest
    with pytest.raises(ValueError, match="scheme"):
        contract.encode_bin(50, "bogus")


def test_bin_class_mapping():
    assert contract.bin_to_class(-10) == 0
    assert contract.bin_to_class(0) == 10
    assert contract.bin_to_class(10) == 20
    for b in range(-10, 11):
        assert contract.class_to_bin(contract.bin_to_class(b)) == b


def test_key_vocab():
    assert contract.N_KEYS == 32
    assert list(contract.CONTRACT_VKEYS) == sorted(contract.CONTRACT_VKEYS)
    # 契约核心键必须在
    for vk in (87, 65, 83, 68, 1, 2, 4, 32, 82):  # WASD, 鼠标, 空格, R
        assert vk in contract.CONTRACT_VKEYS


def test_multihot_roundtrip():
    keys = [87, 1, 32]
    mask, unknown = contract.keys_to_mask(keys)
    assert unknown == ()
    hot = contract.mask_to_multihot(mask)
    assert hot.sum() == 3
    for vk in keys:
        assert hot[contract.CONTRACT_VKEYS.index(vk)] == 1.0


def test_unknown_vk_flagged_not_crashed():
    mask, unknown = contract.keys_to_mask([87, 255])
    assert unknown == (255,)
    assert contract.mask_to_multihot(mask).sum() == 1


def test_parse_frame_row_detects_contract_drift():
    # dm1-v2: 60→6, 存 5 即漂移
    row = {"frame": 0, "gframe": 0, "keys": [], "pointerDx": 60,
           "pointerDy": 0, "pointerBins": [5, 0], "src": "rec"}
    with pytest.raises(ValueError, match="契约漂移"):
        contract.parse_frame_row(row, scheme="dm1-v2")


def test_parse_frame_row_actions_and_scheme():
    row = {"frame": 0, "gframe": 0, "keys": [87], "pointerDx": 120, "pointerDy": 0,
           "pointerBins": [8, 0], "actions": ["move.forward"], "src": "rec"}
    fa = contract.parse_frame_row(row, scheme="dm1-v2")  # 120→8 一致, 不报警
    assert fa.actions == ("move.forward",)
    assert fa.bin_x == 8


def test_parse_frame_row_keeps_mouse_buttons_for_dynamic_vocab():
    row = {"frame": 0, "keys": [87], "buttons": [1, 5],
           "pointerDx": 0, "pointerDy": 0, "pointerBins": [0, 0]}
    fa = contract.parse_frame_row(row)
    assert fa.buttons == (1, 5)

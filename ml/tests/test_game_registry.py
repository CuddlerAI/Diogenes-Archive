"""game_registry 的闭合性与各授权类别的车道行为."""
import pytest

from diogenes_wm import game_registry as gr
from diogenes_wm.halo_adapter import compliance_stamp
from diogenes_wm.halo_qc import AIM_CHANNEL, FROZEN_CHANNELS, policy_for_game


@pytest.fixture(scope="module")
def reg():
    return gr.load_registry()


def test_catalog_and_governance_are_closed(reg):
    """catalog 每个条目都有治理裁定; 治理表没有幽灵条目(拼错的 game_id 会在这暴露)。"""
    assert set(reg) == set(gr.GOVERNANCE), (
        "catalog 与 GOVERNANCE 不闭合 —— 新游戏必须显式登记, 下线游戏必须摘除")
    for e in reg.values():
        assert e.license_class in gr.LICENSE_CLASSES
        assert e.attested_policy == gr._ATTESTED_BY_CLASS[e.license_class]
        assert e.semantics, f"{e.game_id} mapping 语义为空"


def test_halo_partner_set_is_exactly_the_halo_family(reg):
    assert gr.HALO_PARTNER_GAME_IDS == {
        "halo-infinite-steam-release-test",
        "halo-mcc-steam-release-test",
        "halo-reach-mcc-s0",
    }
    for gid in gr.HALO_PARTNER_GAME_IDS:
        assert reg[gid].attested_policy == "partner_trainable"


def test_attested_lane_accepts_all_halo_family_members():
    for gid in sorted(gr.HALO_PARTNER_GAME_IDS):
        stamp = compliance_stamp("first_party", "owner-attest-x", game_id=gid)
        assert stamp["is_trainable"] is True
        assert "halo_partner_licensed" in stamp["tags"]


def test_attested_lane_refuses_microsoft_family_with_pending_message():
    with pytest.raises(ValueError, match="microsoft_family"):
        compliance_stamp("first_party", "owner-attest-x",
                         game_id="call-of-duty-steam-release-test")
    with pytest.raises(ValueError, match="per-title"):
        compliance_stamp("first_party", "owner-attest-x",
                         game_id="wolfenstein2-steam")


def test_attested_lane_refuses_third_party():
    with pytest.raises(ValueError, match="third_party"):
        compliance_stamp("first_party", "owner-attest-x",
                         game_id="bioshock-infinite-steam-release-test")


def test_ungranted_is_blocked_on_both_lanes(monkeypatch):
    """`ungranted_blocked` 今天零条目(唯一那条随 issue #855 PR 2 的 bundle 删除),
    但闸门必须继续可证。用一条**假想**登记跑, 而不是把已删除的 game_id 留在仓库里
    当夹具 —— 那正是 issue 要求清掉的东西。"""
    fake = "an-ungranted-title"
    # halo_adapter 直接 import 了同一个 dict 对象, 所以补这一处即可。
    monkeypatch.setitem(gr.GOVERNANCE, fake, ("ungranted_blocked", "假想条目, 仅用于闸门测试"))
    for rights, consent in (("third_party", None), ("first_party", "owner-x")):
        with pytest.raises(ValueError, match="ungranted"):
            compliance_stamp(rights, consent, game_id=fake)


def test_default_lane_still_works_for_non_halo_titles():
    """默认档(不可训练体检)对 microsoft_family/third_party 照常放行, 标签降级。"""
    stamp = compliance_stamp("third_party", None,
                             game_id="wuchang-fallen-feathers-steam")
    assert stamp["is_trainable"] is False
    assert "license_unknown_non_halo_title" in stamp["tags"]


def test_qc_channels_for_halo_match_wm01_frozen_order(reg):
    """registry 推导的 Halo 通道 == wm01 冻结六通道(含顺序)。断了 = 有人单方面改词表。"""
    assert gr.qc_channels(reg["halo-infinite-steam-release-test"]) == FROZEN_CHANNELS


def test_policy_for_game_halo_strict_equals_classic():
    p = policy_for_game("halo-infinite-steam-release-test", "strict")
    assert p.channels == FROZEN_CHANNELS


def test_policy_for_game_without_aim_vocabulary():
    """nioh2 词表无 combat.aim → aim 判据整组不适用, 通道里也没有 aim。"""
    p = policy_for_game("nioh2-steam", "strict")
    assert AIM_CHANNEL not in p.channels
    assert not ({"aim_share", "toggle_zoom"} & p.enabled_checks)
    assert any(c.startswith("move.") for c in p.channels)


def test_policy_for_game_non_wasd_game_has_channels(reg):
    """RTS(aoe4)没有 WASD 四向, 兜底推导仍要给出非空行为地板通道。"""
    p = policy_for_game("age-of-empires-4-steam", "floor")
    assert p.channels, "非 WASD 游戏的行为地板通道不能为空"
    assert AIM_CHANNEL not in p.channels


def test_unregistered_game_id_is_rejected():
    with pytest.raises(KeyError, match="不在 dev catalog"):
        gr.lane_policy("some-future-game")

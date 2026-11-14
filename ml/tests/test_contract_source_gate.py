"""来源级黑名单 (docs/50 §6.1 R5 / P2) 的测试。

这道闸要防的具体事故: 明天有人给 VPT / CS:GO 新写一个适配器, 并且(手滑或图省事)
盖上 `is_trainable=true` —— 今天没有任何东西会报错, 因为 MANIFEST 里那句
`is_trainable:false` 全仓无人读取。所以本文件里最要紧的两条是
`test_deny_beats_hand_stamped_is_trainable` 与
`test_deny_not_bypassable_by_method_validation_flag`: 它们钉的是"这把锁不能被
数据自陈或调用方开关解开"。
"""
import json

import pytest

from diogenes_phase0.contract import (
    SOURCE_DENY,
    SOURCE_METHOD_VALIDATION_ONLY,
    SOURCE_RULES,
    ChunkMeta,
    ComplianceError,
    assert_source_allowed,
    gate_chunks,
    gate_sources,
    match_source_rule,
    parse_declared_source,
    parse_source_uri,
    source_stamp,
)
from diogenes_phase0.keys import DATA_DEMO_LEGACY_KEY_SCHEMA


def _meta(**kw) -> ChunkMeta:
    """默认是一份**能过四项与**的第一方 chunk —— 这样任何失败都只可能来自来源闸。"""
    base = dict(sample_key="k", game="g", rights="first_party",
                first_party=True, consent_id="c-1",
                is_trainable=True, is_cutscene=False,
                key_schema=DATA_DEMO_LEGACY_KEY_SCHEMA)
    base.update(kw)
    return ChunkMeta(**base)


# --- 名单本身 --------------------------------------------------------------

def test_every_rule_carries_reason_and_evidence():
    """没有出处的 DENY 三个月后无法复核, 会被当成迷信绕过。"""
    for r in SOURCE_RULES:
        assert r.verdict in (SOURCE_DENY, SOURCE_METHOD_VALIDATION_ONLY)
        assert len(r.reason) > 20, r
        assert r.evidence and "docs/50" in r.evidence, r


def test_required_coverage_is_present():
    """任务要求的最小覆盖面: 外部数据集整桶 + 四份镜像 + training-dev。"""
    ext = "diogenes-external-datasets"
    assert match_source_rule(ext, "").verdict == SOURCE_DENY      # 整桶兜底
    for ds in ("vpt-contractor", "csgo-deathmatch", "d2e-480p",
               "gf-minecraft", "vpt-schema-sample"):
        rule = match_source_rule(ext, f"{ds}/some/deep/key.mp4")
        assert rule is not None and rule.verdict == SOURCE_DENY, ds
        assert rule.prefix == ds, "应当命中更具体的那条(理由不同), 不是整桶兜底"
    mv = match_source_rule("diogenes-training-dev", "method-validation/v08")
    assert mv is not None and mv.verdict == SOURCE_METHOD_VALIDATION_ONLY


def test_vpt_rule_states_the_active_disclaimer_not_a_license_vacuum():
    """D12 的裁定: VPT 不是"上游没说", 是上游主动免责 —— 对外解释要引这一点。"""
    rule = match_source_rule("diogenes-external-datasets", "vpt-contractor")
    assert "MIT" in rule.reason and "Microsoft" in rule.reason


def test_attested_lane_in_the_same_bucket_is_not_blacklisted():
    """wm01-archive 与 Halo 具结车道同桶 —— 桶级误伤会把唯一的产品语料也拦掉。"""
    assert match_source_rule(
        "diogenes-model-training",
        "raw/v1/attested-trainable/dataminer-api-production/sessions/x") is None


def test_prefix_match_is_segment_aware():
    """字符串前缀会让 `wm01-archive` 误命中 `wm01-archive-clean`。"""
    assert match_source_rule("diogenes-model-training", "wm01-archive") is not None
    assert match_source_rule("diogenes-model-training", "wm01-archive/a/b") is not None
    assert match_source_rule("diogenes-model-training", "wm01-archive-clean") is None


def test_longest_prefix_wins():
    generic = match_source_rule("diogenes-external-datasets", "unknown-new-mirror")
    specific = match_source_rule("diogenes-external-datasets", "d2e-480p/x")
    assert generic.prefix == ""          # 新落桶的镜像默认被整桶兜底拦住
    assert specific.prefix == "d2e-480p"  # 但有具体规则时给具体理由


# --- 闸的行为 --------------------------------------------------------------

def test_assert_source_allowed_denies_before_download():
    with pytest.raises(ComplianceError, match="永不可训练"):
        assert_source_allowed("diogenes-external-datasets", "vpt-contractor/6.9/x.mp4")
    assert_source_allowed("diogenes-model-training", "raw/v1/attested-trainable/s")
    assert_source_allowed(None, "")       # 未知来源: 默认放行(见 contract.py 权衡)


def test_deny_not_bypassable_by_method_validation_flag():
    """`allow_method_validation_only` 放宽的是我们自己的语料, 不是别人的版权。"""
    with pytest.raises(ComplianceError, match="不可用 allow_method_validation_only"):
        assert_source_allowed("diogenes-external-datasets", "csgo-deathmatch",
                              allow_method_validation_only=True)


def test_method_validation_only_tier_needs_the_explicit_opt_in():
    src = ("diogenes-training-dev", "method-validation/v08")
    with pytest.raises(ComplianceError, match="method-validation-only"):
        assert_source_allowed(*src)
    assert_source_allowed(*src, allow_method_validation_only=True)


def test_deny_beats_hand_stamped_is_trainable():
    """R5 的事故剧本: 有人给 VPT 写了适配器并盖上 trainable 戳。"""
    m = _meta(sample_key="vpt-0001",
              source_bucket="diogenes-external-datasets",
              source_prefix="vpt-contractor/6.9/x.mp4")
    with pytest.raises(ComplianceError, match="永不可训练"):
        gate_chunks([m], allow_method_validation_only=False)
    with pytest.raises(ComplianceError, match="永不可训练"):
        gate_chunks([m], allow_method_validation_only=True)


def test_source_gate_runs_before_the_self_declared_gate():
    """报错要说"这个桶永不可训练", 而不是含混的 is_trainable 失败。"""
    m = _meta(sample_key="vpt-0002", is_trainable=False, first_party=False,
              consent_id=None, rights="third_party",
              source_bucket="diogenes-external-datasets",
              source_prefix="gf-minecraft")
    with pytest.raises(ComplianceError) as e:
        gate_chunks([m], allow_method_validation_only=False)
    assert "永不可训练" in str(e.value)


def test_clean_source_still_goes_through_the_four_way_and():
    """来源闸是**并列**的第二把锁, 不是四项与的替代。"""
    m = _meta(sample_key="halo-1", is_trainable=False,
              source_bucket="diogenes-model-training",
              source_prefix="raw/v1/attested-trainable/x")
    with pytest.raises(ComplianceError, match="is_trainable"):
        gate_chunks([m], allow_method_validation_only=False)
    ok = _meta(sample_key="halo-2",
               source_bucket="diogenes-model-training",
               source_prefix="raw/v1/attested-trainable/x")
    gate_chunks([ok], allow_method_validation_only=False)


# --- 向后兼容 & 未知来源的默认 ---------------------------------------------

def test_undeclared_source_is_allowed_by_default():
    """既有 chunk(data_demo / gaming-500 / D2E / cod_attested)都没有这两个字段。

    一道在 100% 既有语料上报错的闸会在一天内被关掉, 那时它保护的是 0。
    """
    gate_sources([_meta()], allow_method_validation_only=False)
    gate_chunks([_meta()], allow_method_validation_only=False)


def test_undeclared_source_can_be_made_fatal_per_lane():
    with pytest.raises(ComplianceError, match="未申报来源"):
        gate_sources([_meta()], allow_method_validation_only=False,
                     require_declared_source=True)
    declared = _meta(source_bucket="diogenes-model-training",
                     source_prefix="raw/v1/attested-trainable/x")
    gate_sources([declared], allow_method_validation_only=False,
                 require_declared_source=True)


def test_legacy_meta_json_without_source_still_parses():
    obj = {"sample_key": "old", "game": "cod", "fps": 30,
           "key_schema": DATA_DEMO_LEGACY_KEY_SCHEMA,
           # cod_adapter 写的 provenance.source 是自由文本, 不能被当成 bucket
           "provenance": {"source": "DiogenesLab/Diogenes_COD_sample_v01 (local build)"}}
    m = ChunkMeta.from_json(obj)
    assert m.source_bucket is None and not m.source_declared


# --- 来源坐标的解析 --------------------------------------------------------

@pytest.mark.parametrize("uri,expect", [
    ("r2://diogenes-external-datasets/vpt-contractor/x.mp4",
     ("diogenes-external-datasets", "vpt-contractor/x.mp4")),
    ("s3://b/k/e/y", ("b", "k/e/y")),
    ("diogenes-training-dev/method-validation", ("diogenes-training-dev",
                                                 "method-validation")),
    ("https://acct.r2.cloudflarestorage.com/b/k", ("b", "k")),
    ("", (None, "")),
])
def test_parse_source_uri(uri, expect):
    assert parse_source_uri(uri) == expect


def test_parse_declared_source_accepts_the_three_writings():
    assert parse_declared_source(source_stamp("b", "p")) == ("b", "p")
    assert parse_declared_source({"source_bucket": "b", "source_prefix": "p"}) \
        == ("b", "p")
    assert parse_declared_source({"source_uri": "r2://b/p"}) == ("b", "p")
    assert parse_declared_source({"provenance": {"source_uri": "r2://b/p"}}) \
        == ("b", "p")
    assert parse_declared_source({}) == (None, "")


def test_source_stamp_refuses_an_empty_bucket():
    with pytest.raises(ValueError):
        source_stamp("")


def test_meta_json_roundtrip_through_the_stamp():
    obj = {"sample_key": "vpt-x", "game": "minecraft", "fps": 20,
           "key_schema": DATA_DEMO_LEGACY_KEY_SCHEMA,
           "first_party": True, "consent_id": "c-1", "is_trainable": True,
           "rights": "first_party",
           **source_stamp("diogenes-external-datasets", "vpt-contractor/x")}
    m = ChunkMeta.from_json(json.loads(json.dumps(obj)))
    assert m.source_declared
    with pytest.raises(ComplianceError):
        gate_chunks([m], allow_method_validation_only=True)

import pytest

from diogenes_phase0.contract import (
    ChunkMeta,
    ComplianceError,
    gate_chunks,
    gate_fps,
    gate_phase0_contract,
    is_trainable_meta,
    resolve_phase0_contract_mode,
)
from diogenes_phase0.keys import DATA_DEMO_LEGACY_KEY_SCHEMA


def _meta(**kw) -> ChunkMeta:
    base = dict(sample_key="k", game="g", rights="third_party",
                first_party=False, consent_id=None,
                is_trainable=False, is_cutscene=False,
                key_schema=DATA_DEMO_LEGACY_KEY_SCHEMA)
    base.update(kw)
    return ChunkMeta(**base)


def test_third_party_not_trainable():
    assert not is_trainable_meta(_meta())


def test_first_party_needs_consent():
    m = _meta(rights="first_party", first_party=True, is_trainable=True)
    assert not is_trainable_meta(m)
    assert is_trainable_meta(_meta(rights="first_party", first_party=True,
                                   is_trainable=True, consent_id="c-1"))


def test_cutscene_blocked():
    m = _meta(rights="first_party", first_party=True, is_trainable=True,
              consent_id="c-1", is_cutscene=True)
    assert not is_trainable_meta(m)


def test_synthetic_trainable():
    assert is_trainable_meta(_meta(rights="synthetic"))


WAIVER_REF = "r2://diogenes-model-training/raw/v1/attested-trainable/WAIVER.json@2026-08-10"


def _attested(**kw) -> ChunkMeta:
    base = dict(rights="first_party_attested", first_party=True,
                is_trainable=True, attestation={"waiver_ref": WAIVER_REF})
    base.update(kw)
    return _meta(**base)


def test_attested_first_party_passes_without_consent_id():
    """具结车道：consent 由 WAIVER 承载，不要求 consent_id。"""
    m = _attested()
    assert m.consent_id is None
    assert is_trainable_meta(m)


@pytest.mark.parametrize("broken", [
    dict(attestation=None),                       # 缺 attestation 整块
    dict(attestation={}),                         # 有块但缺 waiver_ref
    dict(attestation={"waiver_ref": "   "}),      # 空白 waiver_ref
    dict(attestation={"waiver_ref": 123}),        # 非字符串
    dict(first_party=False),                      # 不是第一方
    dict(is_trainable=False),                     # 上游未标 trainable
    dict(is_cutscene=True),                       # cutscene 照拦
])
def test_attested_blocked_when_waiver_chain_broken(broken):
    assert not is_trainable_meta(_attested(**broken))


def test_attested_rights_alone_does_not_unlock_plain_first_party_gate():
    """rights=first_party_attested 但没有 attestation 时，不得回落到
    consent_id 门放行——即使它同时带着 consent_id。"""
    m = _attested(attestation=None, consent_id="c-1")
    assert not is_trainable_meta(m)


def test_gate_chunks_passes_attested_without_relaxation():
    gate_chunks([_attested()], allow_method_validation_only=False)
    with pytest.raises(ComplianceError):
        gate_chunks([_attested(attestation=None)],
                    allow_method_validation_only=False)


def test_from_json_parses_attestation():
    obj = {
        "sample_key": "k",
        "rights": "first_party_attested",
        "first_party": True,
        "is_trainable": True,
        "attestation": {"waiver_ref": WAIVER_REF},
        "video": {"fps": 60},
    }
    m = ChunkMeta.from_json(obj)
    assert m.rights == "first_party_attested"
    assert m.attestation == {"waiver_ref": WAIVER_REF}
    assert is_trainable_meta(m)


def test_from_json_rejects_non_object_attestation():
    with pytest.raises(ValueError, match="attestation"):
        ChunkMeta.from_json({"video": {"fps": 30},
                             "attestation": "r2://not-an-object"})


def test_gate_blocks_untrainable_without_flag():
    with pytest.raises(ComplianceError):
        gate_chunks([_meta()], allow_method_validation_only=False)
    gate_chunks([_meta()], allow_method_validation_only=True)  # explicit opt-in
    gate_chunks([_meta(rights="synthetic")], allow_method_validation_only=False)


def test_phase0_modes_are_explicit_and_consistent():
    assert resolve_phase0_contract_mode("legacy") == "legacy"
    assert resolve_phase0_contract_mode(
        "method-validation", allow_method_validation_only=True,
    ) == "method-validation"
    with pytest.raises(ComplianceError, match="conflicts"):
        resolve_phase0_contract_mode(
            "legacy", allow_method_validation_only=True,
        )


@pytest.mark.parametrize("mode", ["legacy", "method-validation"])
def test_phase0_rejects_dm1_per_game_mapping_in_both_lanes(mode):
    mapped = _meta(
        rights="synthetic",
        schema_version="dm-1",
        annotation_source="raw_input_recorded",
        mapping_id="game.example.pc",
        mapping_version="1",
    )
    with pytest.raises(ComplianceError, match="PhysicalInputVocab"):
        gate_phase0_contract([mapped], contract_mode=mode)


def test_phase0_method_validation_only_relaxes_governance():
    with pytest.raises(ComplianceError, match="is_trainable"):
        gate_phase0_contract([_meta()], contract_mode="legacy")
    gate_phase0_contract([_meta()], contract_mode="method-validation")


@pytest.mark.parametrize("key_schema", ["", "future-99-key-vocab"])
@pytest.mark.parametrize("mode", ["legacy", "method-validation"])
def test_phase0_rejects_missing_or_unknown_legacy_key_schema(key_schema, mode):
    with pytest.raises(ComplianceError, match="legacy key schema"):
        gate_phase0_contract(
            [_meta(rights="synthetic", key_schema=key_schema)],
            contract_mode=mode,
        )


@pytest.mark.parametrize("marker", [
    {"mapping_content_hash": "sha256:drift"},
    {"schema_version": "dm-1"},
    {"schema_version": "dm-2"},
])
def test_phase0_rejects_partial_mapping_identity(marker):
    with pytest.raises(ComplianceError, match="PhysicalInputVocab"):
        gate_phase0_contract(
            [_meta(rights="synthetic", **marker)],
            contract_mode="legacy",
        )


def test_from_json_requires_fps():
    """fps 缺失必须报错,不允许静默假定 30(混合 fps 防线,docs/50 P-4)。"""
    with pytest.raises(ValueError, match="declares no fps"):
        ChunkMeta.from_json({"sample_key": "k", "video": {"frames": 120}})
    # video.fps 或顶层 fps 任一存在即可
    assert ChunkMeta.from_json({"video": {"fps": 60}}).fps == 60.0
    assert ChunkMeta.from_json({"fps": 30}).fps == 30.0


@pytest.mark.parametrize("value, message", [
    ({"schema_version": "dm-1", "catalog": None}, "dm-1"),
    ({"schema_version": "dm-2"}, "dm-2"),
])
def test_from_json_rejects_data_model_schema_catalog_shape_mismatch(value, message):
    value["video"] = {"fps": 30}
    with pytest.raises(ValueError, match=message):
        ChunkMeta.from_json(value)


def test_from_json_rejects_unknown_data_model_schema():
    with pytest.raises(ValueError, match="unsupported DataModel schema"):
        ChunkMeta.from_json({"schema_version": "dm-99", "video": {"fps": 30}})


@pytest.mark.parametrize("catalog", [123, {}, {"provenance": "future"},
                                      {"provenance": "compiled", "extra": True}])
def test_from_json_rejects_invalid_dm_2_catalog(catalog):
    with pytest.raises(ValueError, match="catalog|provenance"):
        ChunkMeta.from_json({
            "schema_version": "dm-2",
            "catalog": catalog,
            "video": {"fps": 30},
        })


def test_gate_fps_blocks_mixed_corpus():
    metas = [_meta(sample_key="a", fps=30.0), _meta(sample_key="b", fps=60.0)]
    with pytest.raises(ComplianceError, match="mixed-fps"):
        gate_fps(metas)
    gate_fps([_meta(fps=30.0), _meta(fps=30.0)])  # 单值放行
    gate_fps([])  # 空列表放行


def test_gate_fps_expected_pin():
    metas = [_meta(sample_key="a", fps=30.0), _meta(sample_key="b", fps=60.0)]
    with pytest.raises(ComplianceError, match="expected 30"):
        gate_fps(metas, expected_fps=30.0)
    gate_fps([_meta(fps=60.0)], expected_fps=60.0)


def test_from_json_prefers_rational_fps():
    """整数 fps 是近似:59.94(60000/1001)与 60.00 取整后同为 60,
    有理数真值必须优先并被区分(docs/50 P-8)。"""
    m = ChunkMeta.from_json({"video": {"fps": 60, "fps_num": 60000, "fps_den": 1001}})
    assert m.fps == 60000 / 1001
    with pytest.raises(ComplianceError, match="mixed-fps"):
        gate_fps([m, _meta(fps=60.0)])
    # 无效有理数(0/负)退化到整数 fps
    assert ChunkMeta.from_json({"video": {"fps": 30, "fps_num": 0, "fps_den": 1}}).fps == 30.0

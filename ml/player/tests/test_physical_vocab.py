"""PhysicalInputVocab: catalog freeze, source binding and fail-closed audit."""
import json
from pathlib import Path

import pytest

from diogenes_player.ingest import audit_physical_inputs, ingest
from diogenes_player.physical_vocab import (
    CatalogInputSource,
    PhysicalInputContractError,
    PhysicalInputVocab,
    default_cod_physical_vocab,
    physical_vocab_from_checkpoint,
    validate_physical_vocab_scheme,
)


def _source_tree(root: Path, *, keys=None, buttons=None, mapping_hash=None) -> Path:
    vocab = default_cod_physical_vocab()
    source = vocab.sources[0]
    chunk = root / "game" / "session" / "c0000"
    chunk.mkdir(parents=True)
    (chunk / "meta.json").write_text(json.dumps({
        "schema_version": "dm-1",
        "session": "session",
        "chunk": 0,
        "game": "game",
        "video": {"fps": 30, "frames": 1},
        "annotation_source": "raw_input_recorded",
        "key_schema": f"VK codes + dm-1 mapping {source.mapping_id}@{source.mapping_version}",
        "provenance": {
            "mapping_id": source.mapping_id,
            "mapping_version": source.mapping_version,
            "mapping_content_hash": mapping_hash or source.mapping_sha256,
        },
    }))
    (chunk / "p0.jsonl").write_text(json.dumps({
        "frame": 0, "gframe": 0,
        "keys": keys or [], "buttons": buttons or [],
        "pointerDx": 0, "pointerDy": 0, "pointerBins": [0, 0],
    }) + "\n")
    return root


def test_catalog_vocab_is_deterministic_and_namespaces_mouse_buttons():
    a = default_cod_physical_vocab()
    b = default_cod_physical_vocab()
    assert a.content_hash == b.content_hash
    assert a.n_inputs == len(a.keyboard_vks) + len(a.mouse_buttons) == 39
    assert a.mouse_buttons == (1, 2, 4, 5, 6)
    mask = a.encode_mask([87], [1], source=a.sources[0], context="golden")
    hot = a.mask_to_multihot(mask)
    assert hot.sum() == 2
    assert hot[a.keyboard_vks.index(87)] == 1
    assert hot[len(a.keyboard_vks) + a.mouse_buttons.index(1)] == 1


def test_unknown_physical_input_is_not_silently_dropped():
    vocab = default_cod_physical_vocab()
    source = vocab.sources[0]
    with pytest.raises(PhysicalInputContractError, match="255"):
        vocab.encode_mask([87, 255], [1], source=source, context="frame=7")
    with pytest.raises(PhysicalInputContractError, match="9"):
        vocab.encode_mask([87], [9], source=source, context="frame=8")


def _catalog_source(mapping_id: str, keyboard_vks: tuple[int, ...]):
    return CatalogInputSource(
        mapping_id=mapping_id,
        mapping_version="1",
        mapping_sha256=f"sha256:{mapping_id}",
        game_profile_id=mapping_id,
        game_profile_version="1",
        game_profile_sha256=f"sha256:profile-{mapping_id}",
        keyboard_vks=keyboard_vks,
        mouse_buttons=(),
    )


def test_generalist_union_still_enforces_each_mapping_input_contract():
    source_a = _catalog_source("game.a", (10,))
    source_b = _catalog_source("game.b", (20,))
    vocab = PhysicalInputVocab(
        name="generalist",
        version="1",
        keyboard_vks=(10, 20),
        mouse_buttons=(),
        sources=(source_a, source_b),
    )
    meta_a = {
        "schema_version": "dm-1",
        "annotation_source": "raw_input_recorded",
        "key_schema": "VK codes + dm-1 mapping game.a@1",
        "provenance": {
            "mapping_id": "game.a",
            "mapping_version": "1",
            "mapping_content_hash": "sha256:game.a",
        },
    }
    pinned = vocab.validate_chunk_meta(meta_a, context="game-a")
    assert pinned == source_a
    assert vocab.encode_mask([10], [], source=pinned, context="game-a") == 1
    with pytest.raises(PhysicalInputContractError, match="20"):
        vocab.encode_mask([20], [], source=pinned, context="game-a")
    with pytest.raises(PhysicalInputContractError, match="union"):
        PhysicalInputVocab(
            name="bad-union",
            version="1",
            keyboard_vks=(10, 20, 30),
            mouse_buttons=(),
            sources=(source_a, source_b),
        )


@pytest.mark.parametrize("schema_version", ["dm-1", "dm-2"])
def test_catalog_vocab_accepts_both_versioned_data_model_schemas(schema_version):
    vocab = default_cod_physical_vocab()
    source = vocab.sources[0]
    meta = {
        "schema_version": schema_version,
        "annotation_source": "raw_input_recorded",
        "key_schema": f"VK codes + dm-1 mapping {source.mapping_id}@{source.mapping_version}",
        "provenance": {
            "mapping_id": source.mapping_id,
            "mapping_version": source.mapping_version,
            "mapping_content_hash": source.mapping_sha256,
        },
    }
    if schema_version == "dm-2":
        meta["catalog"] = None
    assert vocab.validate_chunk_meta(meta, context="versioned") == source
    if schema_version == "dm-1":
        meta["catalog"] = None
    else:
        meta.pop("catalog")
    with pytest.raises(PhysicalInputContractError, match="top-level catalog"):
        vocab.validate_chunk_meta(meta, context="wrong-shape")


def test_catalog_vocab_rejects_unknown_data_model_schema():
    vocab = default_cod_physical_vocab()
    source = vocab.sources[0]
    meta = {
        "schema_version": "dm-99",
        "annotation_source": "raw_input_recorded",
        "key_schema": f"VK codes + dm-1 mapping {source.mapping_id}@{source.mapping_version}",
        "provenance": {
            "mapping_id": source.mapping_id,
            "mapping_version": source.mapping_version,
            "mapping_content_hash": source.mapping_sha256,
        },
    }
    with pytest.raises(PhysicalInputContractError, match="schema_version"):
        vocab.validate_chunk_meta(meta, context="unknown")


@pytest.mark.parametrize(
    "meta",
    [
        {"schema_version": "dm-99"},
        {"schema_version": "dm-1", "catalog": None},
        {"schema_version": "dm-2", "catalog": 123},
        {"schema_version": "dm-2", "catalog": {}},
        {"schema_version": "dm-2", "catalog": {"provenance": "future"}},
    ],
)
def test_legacy_vocab_still_rejects_invalid_explicit_data_model_schema(meta):
    vocab = PhysicalInputVocab.legacy()
    with pytest.raises(PhysicalInputContractError, match="schema_version|catalog|provenance"):
        vocab.validate_chunk_meta(meta, context="invalid-versioned-legacy")


@pytest.mark.parametrize("catalog", [123, {}, {"provenance": "future"}])
def test_catalog_vocab_rejects_invalid_dm_2_catalog(catalog):
    vocab = default_cod_physical_vocab()
    source = vocab.sources[0]
    meta = {
        "schema_version": "dm-2",
        "catalog": catalog,
        "annotation_source": "raw_input_recorded",
        "key_schema": f"VK codes + dm-1 mapping {source.mapping_id}@{source.mapping_version}",
        "provenance": {
            "mapping_id": source.mapping_id,
            "mapping_version": source.mapping_version,
            "mapping_content_hash": source.mapping_sha256,
        },
    }
    with pytest.raises(PhysicalInputContractError, match="catalog|provenance"):
        vocab.validate_chunk_meta(meta, context="invalid-catalog")


def test_physical_vocab_generation_locks_pointer_scheme():
    validate_physical_vocab_scheme(default_cod_physical_vocab(), "dm1-v2")
    validate_physical_vocab_scheme(PhysicalInputVocab.legacy(), "legacy")
    with pytest.raises(PhysicalInputContractError, match="requires scheme='legacy'"):
        validate_physical_vocab_scheme(PhysicalInputVocab.legacy(), "dm1-v2")
    with pytest.raises(PhysicalInputContractError, match="requires scheme='dm1-v2'"):
        validate_physical_vocab_scheme(default_cod_physical_vocab(), "legacy")


def test_ingest_rejects_generation_scheme_before_writing(tmp_path):
    out = tmp_path / "plane"
    with pytest.raises(PhysicalInputContractError, match="requires scheme='legacy'"):
        ingest(
            tmp_path / "src",
            out,
            physical_vocab=PhysicalInputVocab.legacy(),
            scheme="dm1-v2",
        )
    assert not out.exists()


def test_serialized_vocab_hash_is_self_verifying(tmp_path):
    path = tmp_path / "physical_input_vocab.json"
    vocab = default_cod_physical_vocab()
    vocab.write(path)
    assert PhysicalInputVocab.read(path) == vocab
    value = json.loads(path.read_text())
    value["name"] = "tampered"
    path.write_text(json.dumps(value))
    with pytest.raises(PhysicalInputContractError, match="content_hash"):
        PhysicalInputVocab.read(path)


def test_checkpoint_binds_vocab_and_width():
    vocab = default_cod_physical_vocab()
    checkpoint = {
        "config": {"n_keys": vocab.n_inputs},
        "physical_input_vocab": vocab.as_dict(),
        "physical_input_vocab_hash": vocab.content_hash,
    }
    assert physical_vocab_from_checkpoint(checkpoint) == vocab
    checkpoint["config"]["n_keys"] += 1
    with pytest.raises(PhysicalInputContractError, match="n_keys"):
        physical_vocab_from_checkpoint(checkpoint)


def test_pre_audit_aggregates_unknown_inputs_and_ingest_writes_nothing(tmp_path):
    src = _source_tree(tmp_path / "src", keys=[87, 255, 255], buttons=[1, 9])
    vocab = default_cod_physical_vocab()
    audit = audit_physical_inputs(src, vocab)
    assert audit["unknown_keyboard_vks"] == {255: 2}
    assert audit["unknown_mouse_buttons"] == {9: 1}
    assert audit["blocking"] is True
    out = tmp_path / "plane"
    with pytest.raises(PhysicalInputContractError, match="pre-audit"):
        ingest(src, out, physical_vocab=vocab)
    assert not out.exists()


def test_pre_audit_rejects_mapping_hash_drift(tmp_path):
    src = _source_tree(
        tmp_path / "src", keys=[87], buttons=[1],
        mapping_hash="sha256:" + "0" * 64,
    )
    audit = audit_physical_inputs(src, default_cod_physical_vocab())
    assert audit["blocking"] is True
    assert any("mapping hash" in error for error in audit["identity_errors"])


def test_legacy_vocab_rejects_per_game_mapping_data(tmp_path):
    src = _source_tree(tmp_path / "src", keys=[87])
    audit = audit_physical_inputs(src, PhysicalInputVocab.legacy())
    assert audit["blocking"] is True
    assert any("legacy" in error for error in audit["identity_errors"])

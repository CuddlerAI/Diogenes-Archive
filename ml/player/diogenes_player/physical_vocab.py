"""Versioned physical-input vocabulary frozen from DataMiner catalogs.

The converter emits keyboard VKs and held mouse-button codes.  Their legal set
is per game, while the training plane stores a compact bit mask.  This module
turns one or more ``mapping.json`` + ``game-profile.json`` pairs into the
single immutable vocabulary consumed by ingest, datasets and checkpoints.

Unknown inputs, stale mapping identities and hash mismatches are contract
errors.  They are never silently dropped into an all-zero auxiliary label.
"""
from __future__ import annotations

import argparse
import hashlib
import json
from collections.abc import Iterable
from dataclasses import dataclass
from pathlib import Path

import numpy as np

SCHEMA_VERSION = "physical-input-vocab-1"
ENCODING_VERSION = "uint64-lsb-index-v1"
UNKNOWN_POLICY = "reject-v1"
CATALOG_SOURCE_KIND = "catalog-per-game-v1"
LEGACY_SOURCE_KIND = "legacy-data-demo-v1"
DATA_MODEL_SCHEMA_VERSIONS = frozenset({"dm-1", "dm-2"})
# catalog schema v2（docs/57 批次二）：目录内引用只按 id，配对一致性由
# schema_version 门 + 会话侧 id+version+hash 三元组校验兜底。
MAPPING_SCHEMA = "dm-mapping-2"
# game profile 有两代（issue #855）：legacy `dm-game-profile-2` 必填 `non_trainable`，
# 中性 `dm-game-profile-3` 把该字段整个删掉——录制配置不再兼职表达训练资格。这里两代
# 都收，因为**本模块从来不读那个字段**：它只取 `input_contract` 的 VK/按键面，再把两份
# 文件的精确字节绑进词表哈希。可训练性由 clip_trainability 派生，不由 catalog 声明。
# 三份带 SUPERSEDED.md 的研究快照冻结在 legacy 一代，所以 legacy 必须继续可读。
GAME_PROFILE_SCHEMAS = ("dm-game-profile-2", "dm-game-profile-3")
#: 兼容旧调用方的单值别名（活跃 catalog 今天全在中性一代）。
GAME_PROFILE_SCHEMA = GAME_PROFILE_SCHEMAS[-1]
SOURCE_SCHEMES = {
    CATALOG_SOURCE_KIND: "dm1-v2",
    LEGACY_SOURCE_KIND: "legacy",
}


class PhysicalInputContractError(ValueError):
    """The source and frozen physical-input contract do not agree."""


def _sha256(data: bytes) -> str:
    return f"sha256:{hashlib.sha256(data).hexdigest()}"


def _canonical_bytes(value: dict) -> bytes:
    return json.dumps(
        value, sort_keys=True, separators=(",", ":"), ensure_ascii=False,
    ).encode("utf-8")


def _sorted_unique_codes(values: Iterable[object], *, label: str) -> tuple[int, ...]:
    codes = tuple(sorted(int(v) for v in values))
    if len(codes) != len(set(codes)):
        raise PhysicalInputContractError(f"{label} contains duplicate codes")
    if any(v <= 0 or v > 0xFFFF for v in codes):
        raise PhysicalInputContractError(f"{label} must contain positive u16 codes")
    return codes


@dataclass(frozen=True)
class CatalogInputSource:
    mapping_id: str
    mapping_version: str
    mapping_sha256: str
    game_profile_id: str
    game_profile_version: str
    game_profile_sha256: str
    keyboard_vks: tuple[int, ...]
    mouse_buttons: tuple[int, ...]

    def __post_init__(self) -> None:
        keyboard = _sorted_unique_codes(
            self.keyboard_vks, label=f"{self.mapping_id}.keyboard_vks")
        buttons = _sorted_unique_codes(
            self.mouse_buttons, label=f"{self.mapping_id}.mouse_buttons")
        if keyboard != self.keyboard_vks or buttons != self.mouse_buttons:
            raise PhysicalInputContractError(
                f"{self.mapping_id}: source physical-input codes must be "
                "stored in sorted unique order"
            )

    def as_dict(self) -> dict:
        return {
            "mapping_id": self.mapping_id,
            "mapping_version": self.mapping_version,
            "mapping_sha256": self.mapping_sha256,
            "game_profile_id": self.game_profile_id,
            "game_profile_version": self.game_profile_version,
            "game_profile_sha256": self.game_profile_sha256,
            "keyboard_vks": list(self.keyboard_vks),
            "mouse_buttons": list(self.mouse_buttons),
        }

    @classmethod
    def from_dict(cls, value: dict) -> CatalogInputSource:
        return cls(
            **{k: str(value[k]) for k in (
                "mapping_id", "mapping_version", "mapping_sha256",
                "game_profile_id", "game_profile_version", "game_profile_sha256",
            )},
            keyboard_vks=tuple(int(v) for v in value["keyboard_vks"]),
            mouse_buttons=tuple(int(v) for v in value["mouse_buttons"]),
        )


@dataclass(frozen=True)
class PhysicalInputVocab:
    name: str
    version: str
    keyboard_vks: tuple[int, ...]
    mouse_buttons: tuple[int, ...]
    sources: tuple[CatalogInputSource, ...] = ()
    source_kind: str = CATALOG_SOURCE_KIND

    def __post_init__(self) -> None:
        keyboard = _sorted_unique_codes(self.keyboard_vks, label="keyboard_vks")
        buttons = _sorted_unique_codes(self.mouse_buttons, label="mouse_buttons")
        if keyboard != self.keyboard_vks or buttons != self.mouse_buttons:
            raise PhysicalInputContractError(
                "physical-input codes must be stored in sorted unique order")
        if not self.name or not self.version:
            raise PhysicalInputContractError("physical vocab name/version are required")
        if self.n_inputs == 0:
            raise PhysicalInputContractError("physical vocab cannot be empty")
        if self.n_inputs > 64:
            raise PhysicalInputContractError(
                f"physical vocab has {self.n_inputs} inputs; {ENCODING_VERSION} supports at most 64")
        ids = [source.mapping_id for source in self.sources]
        if ids != sorted(set(ids)):
            raise PhysicalInputContractError(
                "catalog sources must be sorted and unique by mapping_id")
        if self.source_kind not in SOURCE_SCHEMES:
            raise PhysicalInputContractError(
                f"unsupported physical vocab source_kind {self.source_kind!r}")
        if self.source_kind == CATALOG_SOURCE_KIND and not self.sources:
            raise PhysicalInputContractError("catalog physical vocab needs at least one source")
        if self.source_kind == LEGACY_SOURCE_KIND and self.sources:
            raise PhysicalInputContractError("legacy physical vocab cannot bind catalog sources")
        if self.source_kind == CATALOG_SOURCE_KIND:
            source_keyboard = tuple(sorted({
                vk for source in self.sources for vk in source.keyboard_vks
            }))
            source_buttons = tuple(sorted({
                button for source in self.sources for button in source.mouse_buttons
            }))
            if (source_keyboard != self.keyboard_vks
                    or source_buttons != self.mouse_buttons):
                raise PhysicalInputContractError(
                    "catalog physical vocab must equal the union of its "
                    "per-mapping input contracts"
                )

    @property
    def n_inputs(self) -> int:
        return len(self.keyboard_vks) + len(self.mouse_buttons)

    def _payload(self) -> dict:
        return {
            "schema_version": SCHEMA_VERSION,
            "encoding_version": ENCODING_VERSION,
            "unknown_policy": UNKNOWN_POLICY,
            "source_kind": self.source_kind,
            "name": self.name,
            "version": self.version,
            "keyboard_vks": list(self.keyboard_vks),
            "mouse_buttons": list(self.mouse_buttons),
            "sources": [source.as_dict() for source in self.sources],
        }

    @property
    def content_hash(self) -> str:
        return _sha256(_canonical_bytes(self._payload()))

    def as_dict(self) -> dict:
        return {**self._payload(), "content_hash": self.content_hash}

    @classmethod
    def from_dict(cls, value: dict) -> PhysicalInputVocab:
        if value.get("schema_version") != SCHEMA_VERSION:
            raise PhysicalInputContractError(
                f"unsupported physical vocab schema {value.get('schema_version')!r}")
        if value.get("encoding_version") != ENCODING_VERSION:
            raise PhysicalInputContractError(
                f"unsupported physical vocab encoding {value.get('encoding_version')!r}")
        if value.get("unknown_policy") != UNKNOWN_POLICY:
            raise PhysicalInputContractError(
                f"unsupported unknown-input policy {value.get('unknown_policy')!r}")
        vocab = cls(
            name=str(value["name"]),
            version=str(value["version"]),
            keyboard_vks=tuple(int(v) for v in value["keyboard_vks"]),
            mouse_buttons=tuple(int(v) for v in value["mouse_buttons"]),
            sources=tuple(CatalogInputSource.from_dict(v)
                          for v in value.get("sources", [])),
            source_kind=str(value["source_kind"]),
        )
        if value.get("content_hash") != vocab.content_hash:
            raise PhysicalInputContractError(
                "physical vocab content_hash does not match canonical payload")
        return vocab

    @classmethod
    def read(cls, path: str | Path) -> PhysicalInputVocab:
        return cls.from_dict(json.loads(Path(path).read_text(encoding="utf-8")))

    def write(self, path: str | Path) -> None:
        target = Path(path)
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(
            json.dumps(self.as_dict(), indent=2, ensure_ascii=False, sort_keys=True) + "\n",
            encoding="utf-8",
        )

    @classmethod
    def legacy(cls) -> PhysicalInputVocab:
        # Delayed import avoids making contract.py depend on this module.
        from .contract import CONTRACT_VKEYS
        return cls(
            name="data-demo-legacy-physical",
            version="1",
            keyboard_vks=CONTRACT_VKEYS,
            mouse_buttons=(),
            sources=(),
            source_kind=LEGACY_SOURCE_KIND,
        )

    @classmethod
    def from_catalogs(
        cls,
        mapping_paths: Iterable[str | Path],
        game_profile_paths: Iterable[str | Path],
        *,
        name: str,
        version: str,
    ) -> PhysicalInputVocab:
        """Freeze a deterministic union from matching catalog pairs.

        ``mapping.json`` owns semantic bindings in scan-code/control space;
        ``game-profile.input_contract`` owns the accepted VK/button surface
        emitted by frame rows.  The vocabulary therefore takes physical codes
        from the latter while binding both files' exact bytes into its hash.
        Pairing is by ``mapping_id`` alone — catalog schema v2 dropped
        in-bundle version references (docs/57 §3.2); the mapping's own
        version stays pinned per source for session-side id+version+hash
        validation.
        """
        mappings: dict[str, tuple[dict, bytes]] = {}
        profiles: dict[str, tuple[dict, bytes]] = {}
        for path in mapping_paths:
            raw = Path(path).read_bytes()
            value = json.loads(raw)
            mapping_id = str(value["mapping_id"])
            if mapping_id in mappings:
                raise PhysicalInputContractError(f"duplicate mapping_id {mapping_id!r}")
            mappings[mapping_id] = (value, raw)
        for path in game_profile_paths:
            raw = Path(path).read_bytes()
            value = json.loads(raw)
            mapping_id = str(value["mapping_id"])
            if mapping_id in profiles:
                raise PhysicalInputContractError(
                    f"duplicate game-profile mapping_id {mapping_id!r}")
            profiles[mapping_id] = (value, raw)
        if set(mappings) != set(profiles) or not mappings:
            raise PhysicalInputContractError(
                "mapping/game-profile sets must be non-empty and match by mapping_id")

        keyboard: set[int] = set()
        buttons: set[int] = set()
        sources: list[CatalogInputSource] = []
        for mapping_id in sorted(mappings):
            mapping, mapping_raw = mappings[mapping_id]
            profile, profile_raw = profiles[mapping_id]
            if mapping.get("schema_version") != MAPPING_SCHEMA:
                raise PhysicalInputContractError(
                    f"{mapping_id}: unsupported mapping schema "
                    f"{mapping.get('schema_version')!r} (need {MAPPING_SCHEMA!r})")
            if profile.get("schema_version") not in GAME_PROFILE_SCHEMAS:
                raise PhysicalInputContractError(
                    f"{mapping_id}: unsupported game-profile schema "
                    f"{profile.get('schema_version')!r} (need one of {GAME_PROFILE_SCHEMAS!r})")
            mapping_version = str(mapping["version"])
            contract = profile.get("input_contract") or {}
            profile_vks = _sorted_unique_codes(
                contract.get("keyboard_vks", []), label=f"{mapping_id}.keyboard_vks")
            profile_buttons = _sorted_unique_codes(
                contract.get("mouse_buttons", []), label=f"{mapping_id}.mouse_buttons")
            bound_buttons = {
                int(binding["button"])
                for binding in mapping.get("bindings", [])
                if binding.get("device") == "mouse" and "button" in binding
            }
            if not bound_buttons.issubset(profile_buttons):
                raise PhysicalInputContractError(
                    f"{mapping_id}: mapping binds mouse buttons outside game-profile input_contract")
            keyboard.update(profile_vks)
            buttons.update(profile_buttons)
            sources.append(CatalogInputSource(
                mapping_id=mapping_id,
                mapping_version=mapping_version,
                mapping_sha256=_sha256(mapping_raw),
                game_profile_id=str(profile["game_id"]),
                game_profile_version=str(profile["version"]),
                game_profile_sha256=_sha256(profile_raw),
                keyboard_vks=profile_vks,
                mouse_buttons=profile_buttons,
            ))
        return cls(
            name=name,
            version=version,
            keyboard_vks=tuple(sorted(keyboard)),
            mouse_buttons=tuple(sorted(buttons)),
            sources=tuple(sources),
        )

    def source_for(self, mapping_id: str) -> CatalogInputSource | None:
        return next((s for s in self.sources if s.mapping_id == mapping_id), None)

    def validate_chunk_meta(
        self, meta: dict, *, context: str,
    ) -> CatalogInputSource | None:
        schema_version = meta.get("schema_version")
        if schema_version not in (None, ""):
            if schema_version not in DATA_MODEL_SCHEMA_VERSIONS:
                raise PhysicalInputContractError(
                    f"{context}: unsupported schema_version {schema_version!r}")
            if schema_version == "dm-1" and "catalog" in meta:
                raise PhysicalInputContractError(
                    f"{context}: dm-1 chunk metadata must not contain top-level catalog")
            if schema_version == "dm-2" and "catalog" not in meta:
                raise PhysicalInputContractError(
                    f"{context}: dm-2 chunk metadata must contain top-level catalog")
            catalog = meta.get("catalog")
            if schema_version == "dm-2" and catalog is not None:
                if not isinstance(catalog, dict):
                    raise PhysicalInputContractError(
                        f"{context}: dm-2 catalog must be null or an object")
                if set(catalog) != {"provenance"}:
                    raise PhysicalInputContractError(
                        f"{context}: dm-2 catalog object must contain only provenance")
                if catalog.get("provenance") not in {"compiled", "delivered"}:
                    raise PhysicalInputContractError(
                        f"{context}: unsupported catalog provenance "
                        f"{catalog.get('provenance')!r}")

        provenance = meta.get("provenance") or {}
        mapping_id = provenance.get("mapping_id") or meta.get("mapping_id")
        version = provenance.get("mapping_version") or meta.get("mapping_version")
        content_hash = (provenance.get("mapping_content_hash")
                        or meta.get("mapping_content_hash"))
        has_per_game_marker = bool(
            mapping_id
            or version
            or content_hash
            or "dm-1 mapping" in str(meta.get("key_schema", "")).lower()
            or (meta.get("schema_version") in DATA_MODEL_SCHEMA_VERSIONS
                and meta.get("annotation_source") == "raw_input_recorded")
        )
        if self.source_kind == LEGACY_SOURCE_KIND:
            if has_per_game_marker:
                raise PhysicalInputContractError(
                    f"{context}: legacy physical vocab cannot consume dm-1/per-game mapping data")
            return None
        if (schema_version not in DATA_MODEL_SCHEMA_VERSIONS
                or meta.get("annotation_source") != "raw_input_recorded"):
            raise PhysicalInputContractError(
                f"{context}: catalog physical vocab requires "
                "schema_version in {'dm-1', 'dm-2'} and "
                "annotation_source='raw_input_recorded'"
            )
        if not mapping_id:
            raise PhysicalInputContractError(
                f"{context}: catalog physical vocab requires provenance.mapping_id")
        source = self.source_for(str(mapping_id))
        if source is None:
            raise PhysicalInputContractError(
                f"{context}: mapping_id {mapping_id!r} is not in physical vocab")
        if str(version) != source.mapping_version:
            raise PhysicalInputContractError(
                f"{context}: mapping version {version!r} != pinned {source.mapping_version!r}")
        if content_hash != source.mapping_sha256:
            raise PhysicalInputContractError(
                f"{context}: mapping hash {content_hash!r} != pinned {source.mapping_sha256!r}")
        expected_schema = (
            f"VK codes + dm-1 mapping {source.mapping_id}@{source.mapping_version}"
        )
        if meta.get("key_schema") != expected_schema:
            raise PhysicalInputContractError(
                f"{context}: key_schema {meta.get('key_schema')!r} != "
                f"pinned {expected_schema!r}"
            )
        return source

    def encode_mask(
        self,
        keys: Iterable[object],
        buttons: Iterable[object],
        *,
        source: CatalogInputSource | None = None,
        context: str = "frame",
    ) -> int:
        if self.source_kind == CATALOG_SOURCE_KIND:
            if source is None or source not in self.sources:
                raise PhysicalInputContractError(
                    f"{context}: catalog physical encoding requires a pinned "
                    "per-mapping source"
                )
            allowed_keys = set(source.keyboard_vks)
            allowed_buttons = set(source.mouse_buttons)
        else:
            if source is not None:
                raise PhysicalInputContractError(
                    f"{context}: legacy physical encoding cannot bind a catalog source"
                )
            allowed_keys = set(self.keyboard_vks)
            allowed_buttons = set(self.mouse_buttons)
        key_index = {vk: i for i, vk in enumerate(self.keyboard_vks)}
        button_offset = len(self.keyboard_vks)
        button_index = {
            button: button_offset + i for i, button in enumerate(self.mouse_buttons)
        }
        mask = 0
        unknown_keys: list[int] = []
        unknown_buttons: list[int] = []
        for raw in keys:
            key = int(raw)
            idx = key_index.get(key)
            if idx is None or key not in allowed_keys:
                unknown_keys.append(key)
            else:
                mask |= 1 << idx
        for raw in buttons:
            button = int(raw)
            idx = button_index.get(button)
            if idx is None or button not in allowed_buttons:
                unknown_buttons.append(button)
            else:
                mask |= 1 << idx
        if unknown_keys or unknown_buttons:
            raise PhysicalInputContractError(
                f"{context}: unknown physical inputs "
                f"keyboard_vks={sorted(set(unknown_keys))} "
                f"mouse_buttons={sorted(set(unknown_buttons))} "
                f"for vocab {self.content_hash}"
            )
        return mask

    def mask_to_multihot(self, mask: int) -> np.ndarray:
        if mask < 0 or mask >> self.n_inputs:
            raise PhysicalInputContractError(
                f"physical mask has bits outside vocab width {self.n_inputs}")
        out = np.zeros(self.n_inputs, dtype=np.float32)
        for index in range(self.n_inputs):
            if mask & (1 << index):
                out[index] = 1.0
        return out


_REPO_ROOT = Path(__file__).resolve().parents[3]
COD_CATALOG = _REPO_ROOT / "DataMiner/config/catalogs/dev/cod-steam-release-test"


def default_cod_physical_vocab() -> PhysicalInputVocab:
    return PhysicalInputVocab.from_catalogs(
        [COD_CATALOG / "mapping.json"],
        [COD_CATALOG / "game-profile.json"],
        name="cod-steam-release-test-physical",
        version="1",
    )


def physical_vocab_from_checkpoint(checkpoint: dict) -> PhysicalInputVocab:
    value = checkpoint.get("physical_input_vocab")
    if not isinstance(value, dict):
        raise PhysicalInputContractError(
            "checkpoint has no frozen physical_input_vocab; legacy checkpoints "
            "must be migrated explicitly before eval/export")
    vocab = PhysicalInputVocab.from_dict(value)
    if checkpoint.get("physical_input_vocab_hash") != vocab.content_hash:
        raise PhysicalInputContractError(
            "checkpoint physical_input_vocab_hash does not match embedded vocab")
    configured_width = (checkpoint.get("config") or {}).get("n_keys")
    if configured_width is not None and int(configured_width) != vocab.n_inputs:
        raise PhysicalInputContractError(
            f"checkpoint n_keys={configured_width} != physical vocab width {vocab.n_inputs}")
    return vocab


def validate_physical_vocab_scheme(
    vocab: PhysicalInputVocab, scheme: str,
) -> None:
    expected = SOURCE_SCHEMES[vocab.source_kind]
    if scheme != expected:
        raise PhysicalInputContractError(
            f"physical vocab source_kind {vocab.source_kind!r} requires "
            f"scheme={expected!r}, got {scheme!r}"
        )


def add_physical_vocab_arguments(parser: argparse.ArgumentParser) -> None:
    parser.add_argument("--physical-vocab", type=Path,
                        help="read a previously frozen physical_input_vocab.json")
    parser.add_argument("--legacy-physical-vocab", action="store_true",
                        help="explicitly use the data_demo legacy 32-code vocabulary")
    parser.add_argument("--mapping", type=Path, action="append", default=[],
                        help="mapping.json to freeze (repeat for a generalist union)")
    parser.add_argument("--game-profile", type=Path, action="append", default=[],
                        help="matching game-profile.json (repeat with --mapping)")
    parser.add_argument("--physical-vocab-name", default="dataset-physical")
    parser.add_argument("--physical-vocab-version", default="1")


def physical_vocab_from_args(args: argparse.Namespace) -> PhysicalInputVocab:
    modes = int(bool(args.physical_vocab)) + int(bool(args.legacy_physical_vocab)) \
        + int(bool(args.mapping or args.game_profile))
    if modes != 1:
        raise PhysicalInputContractError(
            "choose exactly one physical vocab source: --physical-vocab, "
            "--legacy-physical-vocab, or --mapping + --game-profile")
    if args.physical_vocab:
        return PhysicalInputVocab.read(args.physical_vocab)
    if args.legacy_physical_vocab:
        return PhysicalInputVocab.legacy()
    return PhysicalInputVocab.from_catalogs(
        args.mapping,
        args.game_profile,
        name=args.physical_vocab_name,
        version=args.physical_vocab_version,
    )

//! Read-only CP2077 2.31 runtime pose discovery plumbing.
//!
//! This module is intentionally an **observer**, not a production sampler:
//!
//! - it consumes the exact game's own linker-address JSON without loading RED4ext/CET/IGCS;
//! - it retains only a small allow-list of first-party symbols and never persists process
//!   pointers;
//! - it accepts bounded byte snapshots copied by a caller at an already-audited safe point and
//!   searches them for structural `WorldTransform` candidates;
//! - it records raw fixed-point/quaternion quantities for later axis/scale calibration, but never
//!   decides that a candidate is the active camera or player;
//! - every report is `production_binding_emitted=false` and `recording_ready=false`.
//!
//! # True-machine wiring contract
//!
//! 1. Call [`bootstrap_current_process`] only after the containing probe has validated the exact
//!    executable hash. The bootstrap validates the exact first-party address catalog and the
//!    section-relative player-spawn symbol against the loaded PE image.
//! 2. Bootstrap also runs the exact loaded-image ABI byte gate for the known Player routes. It
//!    still deliberately installs no hook: vtable ownership and the native callback return
//!    contract remain runtime admission facts.
//! 3. At a quiescent/safe observation point, register an object pointer with
//!    [`Cp2077PoseRuntimeSession::register_ephemeral_object`]. The pointer stays process-local and
//!    becomes an opaque ordinal. Copy at most [`MAX_OBJECT_SNAPSHOT_BYTES`] into caller-owned
//!    bytes, then pass only those bytes to [`Cp2077PoseRuntimeSession::observe_snapshot`].
//! 4. Camera must arrive through a separately audited first-party RTTI/game-system route or a
//!    separately audited render-camera route. It may not reuse the player anchor or infer player
//!    pose from the view matrix.
//! 5. Add explicit calibration markers around controlled head sweep, translation, axis movement
//!    and terminal loop return. The report carries raw deltas; E-geo and metric admission happen
//!    elsewhere.
//! 6. Write the report only through [`write_report_create_new`], which rejects the game tree.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::path::Path;

use serde::de::{IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

use crate::cp2077_pose_abi::{Cp2077PlayerAbiAudit, audit_cp2077_player_abi_loaded_image};
use crate::cp2077_pose_probe::{
    CP2077_EXE_SHA256, CP2077_PRODUCT_VERSION, Cp2077ExactBuild, DiscoveryScenario, PoseRole,
};

pub const CP2077_ADDRESS_CATALOG_SHA256: &str =
    "sha256:58da9e852fa519f851eb42fd36fd27c292c5c8585b1ec66cf34d4a2544a5689f";
pub const CP2077_PLAYER_SPAWN_SYMBOL: &str = "cp::PlayerSystem::OnPlayerMainObjectSpawned";
pub const CP2077_PLAYER_SPAWN_SYMBOL_HASH: &str = "2050111212";
pub const CP2077_PLAYER_SPAWN_SECONDARY_HASH: &str =
    "98dac7c90b04329a564b0d828419c7f30d4b65f65173691fafcbe1eae5e4863f";
pub const CP2077_PLAYER_SPAWN_SECTION_OFFSET: &str = "0001:00a1352c";
pub const CP2077_CGAME_ENGINE_SYMBOL: &str = "address_hash::CGameEngine";
pub const CP2077_CGAME_ENGINE_HASH: &str = "2549221846";
pub const CP2077_CGAME_ENGINE_SECONDARY_HASH: &str =
    "347be984cccfcc3d23c0f65e3184ff09d7f0d335563e76fa4672d23f71242dc9";
pub const CP2077_CGAME_ENGINE_SECTION_OFFSET: &str = "0003:00144c00";
pub const CP2077_CRTTI_SYSTEM_GET_SYMBOL: &str = "address_hash::CRTTISystem_Get";
pub const CP2077_CRTTI_SYSTEM_GET_HASH: &str = "1247874916";
pub const CP2077_CRTTI_SYSTEM_GET_SECONDARY_HASH: &str =
    "a15fa31a13e6606cbc5344b6b481eb7909589ea7b37e34ebe376eafcd6e21900";
pub const CP2077_CRTTI_SYSTEM_GET_SECTION_OFFSET: &str = "0001:002f0080";
pub const CP2077_PLAYER_SYSTEM_CNAME_HASH: u64 = 0xe9e2_c6cf_6b09_926e;
pub const CP2077_CAMERA_SYSTEM_CNAME_HASH: u64 = 0x73b1_5f77_7a57_3929;
pub const CP2077_PLAYER_SYSTEM_TYPE_BYTES: u32 = 0x60_500;
pub const CP2077_CAMERA_SYSTEM_TYPE_BYTES: u32 = 0x730;
pub const CP2077_POSE_RUNTIME_REPORT_SCHEMA_V1: &str = "cp2077-pose-runtime-observation/v1";
pub const MAX_OBJECT_SNAPSHOT_BYTES: usize = 64 * 1024;
pub const MAX_TRANSFORM_CANDIDATES_PER_SNAPSHOT: usize = 64;
/// One exact player pose per Present for the bounded five-minute local trial, plus discovery
/// observations. This remains a hard ceiling rather than an unbounded in-process ledger.
pub const MAX_RUNTIME_OBSERVATIONS: usize = 20_000;
pub const MAX_RENDER_PRESENT_INPUTS: usize = 4_096;
pub const MAX_SYSTEM_ENUMERATIONS: usize = 256;
pub const MAX_SYSTEM_CANDIDATES_PER_ENUMERATION: usize = 512;
pub const MAX_EPHEMERAL_OBJECTS: usize = 1_024;
/// Must match the client discovery report ceiling. Keeping the producer at or
/// below the consumer limit prevents a successfully written report from being
/// impossible for the client to seal.
pub const MAX_RUNTIME_REPORT_BYTES: usize = 16 * 1024 * 1024;
const WORLD_TRANSFORM_BYTES: usize = 0x20;
const WORLD_ORIENTATION_OFFSET: usize = 0x10;
const WORLD_POSITION_DIVISOR: f64 = 131_072.0;
const MAX_ABS_WORLD_UNITS: f64 = 10_000_000.0;
const QUATERNION_NORM_TOLERANCE: f64 = 0.02;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetainedFirstPartySymbol {
    pub symbol: String,
    pub hash: String,
    pub secondary_hash: String,
    pub section: u16,
    pub section_offset: u32,
    pub rva: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AddressCatalogObservation {
    pub catalog_sha256: String,
    pub linker_map_timestamp: String,
    pub preferred_load_address: String,
    pub code_constant_offset: String,
    pub total_address_rows: u64,
    pub named_address_rows: u64,
    pub retained_symbols: Vec<RetainedFirstPartySymbol>,
    pub camera_symbol_count: u64,
    pub player_symbol_count: u64,
    pub world_transform_symbol_count: u64,
    pub game_instance_symbol_count: u64,
    pub game_system_symbol_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AddressCatalogBody {
    linker_map_timestamp: String,
    preferred_load_address: String,
    code_constant_offset: String,
    addresses: AddressRows,
}

impl<'de> Deserialize<'de> for AddressCatalogBody {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct RootVisitor;

        impl<'de> Visitor<'de> for RootVisitor {
            type Value = AddressCatalogBody;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("the CP2077 linker-address catalog root")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut linker_map_timestamp = None;
                let mut preferred_load_address = None;
                let mut code_constant_offset = None;
                let mut addresses = None;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "Linker map timestamp" => linker_map_timestamp = Some(map.next_value()?),
                        "Preferred load address" => {
                            preferred_load_address = Some(map.next_value()?)
                        }
                        "Code constant offset" => code_constant_offset = Some(map.next_value()?),
                        "Addresses" => addresses = Some(map.next_value()?),
                        _ => {
                            map.next_value::<IgnoredAny>()?;
                        }
                    }
                }
                Ok(AddressCatalogBody {
                    linker_map_timestamp: linker_map_timestamp
                        .ok_or_else(|| serde::de::Error::missing_field("Linker map timestamp"))?,
                    preferred_load_address: preferred_load_address
                        .ok_or_else(|| serde::de::Error::missing_field("Preferred load address"))?,
                    code_constant_offset: code_constant_offset
                        .ok_or_else(|| serde::de::Error::missing_field("Code constant offset"))?,
                    addresses: addresses
                        .ok_or_else(|| serde::de::Error::missing_field("Addresses"))?,
                })
            }
        }

        deserializer.deserialize_map(RootVisitor)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct AddressRows {
    total: u64,
    named: u64,
    retained: Vec<RawAddressRow>,
    camera: u64,
    player: u64,
    world_transform: u64,
    game_instance: u64,
    game_system: u64,
}

impl<'de> Deserialize<'de> for AddressRows {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct RowsVisitor;

        impl<'de> Visitor<'de> for RowsVisitor {
            type Value = AddressRows;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("the linker-address row array")
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut rows = AddressRows::default();
                while let Some(row) = seq.next_element::<RawAddressRow>()? {
                    rows.total += 1;
                    if let Some(symbol) = row.symbol.as_deref() {
                        rows.named += 1;
                        rows.camera += u64::from(symbol.contains("Camera"));
                        rows.player += u64::from(symbol.contains("Player"));
                        rows.world_transform += u64::from(symbol.contains("WorldTransform"));
                        rows.game_instance += u64::from(symbol.contains("GameInstance"));
                        rows.game_system += u64::from(symbol.contains("GameSystem"));
                    }
                    if row.symbol.as_deref() == Some(CP2077_PLAYER_SPAWN_SYMBOL)
                        || matches!(
                            row.hash.as_str(),
                            CP2077_CGAME_ENGINE_HASH | CP2077_CRTTI_SYSTEM_GET_HASH
                        )
                    {
                        rows.retained.push(row);
                    }
                }
                Ok(rows)
            }
        }

        deserializer.deserialize_seq(RowsVisitor)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAddressRow {
    hash: String,
    #[serde(rename = "secondary hash")]
    secondary_hash: String,
    #[serde(default)]
    symbol: Option<String>,
    offset: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleImageLayout {
    pub image_size: u32,
    /// Linker address-table segments are fixed by the first-party resolver source:
    /// segment 1 = `.text`, segment 2 = `.rdata`, segment 3 = `.data`.
    pub linker_segment_rvas: [u32; 3],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SectionRelativeOffset {
    section: u16,
    offset: u32,
}

fn parse_section_relative(value: &str) -> Result<SectionRelativeOffset, String> {
    let (section, offset) = value
        .split_once(':')
        .ok_or_else(|| format!("invalid section-relative offset {value:?}"))?;
    let section = u16::from_str_radix(section, 16)
        .map_err(|_| format!("invalid section number {section:?}"))?;
    let offset = u32::from_str_radix(offset, 16)
        .map_err(|_| format!("invalid section offset {offset:?}"))?;
    if section == 0 {
        return Err("section-relative offset uses section zero".into());
    }
    Ok(SectionRelativeOffset { section, offset })
}

fn resolve_rva(relative: SectionRelativeOffset, image: &ModuleImageLayout) -> Result<u32, String> {
    let section_index = usize::from(relative.section - 1);
    let section_rva = image
        .linker_segment_rvas
        .get(section_index)
        .copied()
        .ok_or_else(|| {
            format!(
                "catalog linker segment {} is outside .text/.rdata/.data",
                relative.section
            )
        })?;
    let rva = section_rva
        .checked_add(relative.offset)
        .ok_or("catalog RVA overflow")?;
    if rva >= image.image_size {
        return Err(format!(
            "catalog RVA {rva:#x} exceeds image size {:#x}",
            image.image_size
        ));
    }
    Ok(rva)
}

pub fn parse_address_catalog<R: Read>(
    reader: R,
    catalog_sha256: &str,
    image: Option<&ModuleImageLayout>,
) -> Result<AddressCatalogObservation, String> {
    if catalog_sha256 != CP2077_ADDRESS_CATALOG_SHA256 {
        return Err(format!(
            "CP2077 first-party address catalog hash mismatch: expected {CP2077_ADDRESS_CATALOG_SHA256}, got {catalog_sha256}"
        ));
    }
    let body: AddressCatalogBody = serde_json::from_reader(reader)
        .map_err(|error| format!("parse CP2077 first-party address catalog failed: {error}"))?;
    if body.addresses.retained.len() != 3 {
        return Err(format!(
            "expected three exact CP2077 pose bootstrap addresses, found {}",
            body.addresses.retained.len()
        ));
    }
    let expected = [
        (
            CP2077_PLAYER_SPAWN_SYMBOL_HASH,
            CP2077_PLAYER_SPAWN_SECONDARY_HASH,
            CP2077_PLAYER_SPAWN_SECTION_OFFSET,
            CP2077_PLAYER_SPAWN_SYMBOL,
        ),
        (
            CP2077_CGAME_ENGINE_HASH,
            CP2077_CGAME_ENGINE_SECONDARY_HASH,
            CP2077_CGAME_ENGINE_SECTION_OFFSET,
            CP2077_CGAME_ENGINE_SYMBOL,
        ),
        (
            CP2077_CRTTI_SYSTEM_GET_HASH,
            CP2077_CRTTI_SYSTEM_GET_SECONDARY_HASH,
            CP2077_CRTTI_SYSTEM_GET_SECTION_OFFSET,
            CP2077_CRTTI_SYSTEM_GET_SYMBOL,
        ),
    ];
    let retained_symbols = expected
        .into_iter()
        .map(|(hash, secondary_hash, offset, fallback_symbol)| {
            let raw = body
                .addresses
                .retained
                .iter()
                .find(|row| row.hash == hash)
                .ok_or_else(|| format!("CP2077 exact address hash {hash} is absent"))?;
            if raw.secondary_hash != secondary_hash || raw.offset != offset {
                return Err(format!(
                    "CP2077 exact address identity drifted for {fallback_symbol}"
                ));
            }
            let relative = parse_section_relative(&raw.offset)?;
            let rva = image
                .map(|layout| resolve_rva(relative, layout))
                .transpose()?;
            Ok(RetainedFirstPartySymbol {
                symbol: raw
                    .symbol
                    .clone()
                    .unwrap_or_else(|| fallback_symbol.to_string()),
                hash: raw.hash.clone(),
                secondary_hash: raw.secondary_hash.clone(),
                section: relative.section,
                section_offset: relative.offset,
                rva,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(AddressCatalogObservation {
        catalog_sha256: catalog_sha256.into(),
        linker_map_timestamp: body.linker_map_timestamp,
        preferred_load_address: body.preferred_load_address,
        code_constant_offset: body.code_constant_offset,
        total_address_rows: body.addresses.total,
        named_address_rows: body.addresses.named,
        retained_symbols,
        camera_symbol_count: body.addresses.camera,
        player_symbol_count: body.addresses.player,
        world_transform_symbol_count: body.addresses.world_transform,
        game_instance_symbol_count: body.addresses.game_instance,
        game_system_symbol_count: body.addresses.game_system,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeResolverSource {
    FirstPartyPlayerSpawnEvent,
    FirstPartyRttiGameSystem,
    SelectedPresentRenderCamera,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum HookAbiEvidence {
    Unverified,
    Validated {
        evidence_id: String,
        original_forwarded_exactly_once: bool,
    },
    NotApplicable,
}

impl HookAbiEvidence {
    fn admitted(&self, source: RuntimeResolverSource) -> bool {
        match (source, self) {
            (
                RuntimeResolverSource::FirstPartyPlayerSpawnEvent,
                Self::Validated {
                    evidence_id,
                    original_forwarded_exactly_once: true,
                },
            ) => !evidence_id.trim().is_empty(),
            (RuntimeResolverSource::FirstPartyPlayerSpawnEvent, _) => false,
            (_, Self::NotApplicable) => true,
            _ => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolverObservation {
    pub resolver_id: String,
    pub role: PoseRole,
    pub source: RuntimeResolverSource,
    /// Required for the player-spawn route; never an absolute address.
    pub anchor_symbol: Option<String>,
    pub anchor_rva: Option<u32>,
    /// Pointer/object offsets after the anchor, in traversal order.
    pub pointer_offsets: Vec<u32>,
    pub object_type_name: Option<String>,
    pub object_type_bytes: Option<u32>,
    pub hook_abi: HookAbiEvidence,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CalibrationMarker {
    pub marker_id: String,
    pub present_seq: u64,
    pub qpc: i64,
    pub scenario: DiscoveryScenario,
    /// Operator-authored label such as `head-left-start` or `walk-forward-end`. It is not
    /// interpreted as a coordinate-system conclusion.
    pub motion_label: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransformCandidateObservation {
    pub transform_offset: u32,
    pub raw_position_bits: [i32; 3],
    /// REDengine world units, not metres.
    pub position_world_units: [f64; 3],
    pub raw_quaternion_ijkr: [f32; 4],
    pub quaternion_norm_error: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PoseSnapshotObservation {
    pub role: PoseRole,
    pub resolver_id: String,
    pub object_tag: u64,
    pub present_seq: u64,
    pub qpc: i64,
    pub scenario: DiscoveryScenario,
    pub source_snapshot_bytes: u32,
    pub candidates: Vec<TransformCandidateObservation>,
    pub candidates_truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenderPresentDiscoveryInput {
    pub present_seq: u64,
    pub qpc: i64,
    pub generation: u64,
    pub resource_epoch: u64,
    pub queue_count: u32,
    pub command_list_count: u32,
    pub depth_resource_count: u32,
    pub eligible_pre_clear_snapshot_count: u64,
    pub new_pre_clear_snapshot_since_previous_present: bool,
    pub capture_safe: bool,
    /// The current D3D12 observation backend does not expose shader constants yet.
    pub physical_view_matrix_source_observed: bool,
    pub physical_projection_matrix_source_observed: bool,
    pub camera_candidate_admitted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSystemCandidateObservation {
    pub system_index: u32,
    pub object_tag: u64,
    pub handle_instance_readable: bool,
    pub handle_refcount_readable: bool,
    pub handle_strong_ref_count: u32,
    pub handle_weak_ref_count: u32,
    pub object_vtable_readable: bool,
    pub native_type_descriptor_readable: bool,
    pub vtable_module_rva: Option<u32>,
    pub native_type_name: Option<String>,
    pub native_type_bytes: Option<u32>,
    pub player_system_type_match: bool,
    pub camera_system_type_match: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSystemEnumerationObservation {
    pub poll_ordinal: u64,
    pub qpc: i64,
    pub cgame_engine_resolved: bool,
    pub framework_resolved: bool,
    pub game_instance_resolved: bool,
    pub declared_system_count: u32,
    pub candidates_truncated: bool,
    pub candidates: Vec<RuntimeSystemCandidateObservation>,
    pub blocker: Option<String>,
}

pub struct SnapshotObservationInput<'a> {
    pub role: PoseRole,
    pub resolver_id: &'a str,
    pub object_tag: u64,
    pub present_seq: u64,
    pub qpc: i64,
    pub scenario: DiscoveryScenario,
    pub bytes: &'a [u8],
}

fn read_i32(bytes: &[u8], offset: usize) -> Option<i32> {
    Some(i32::from_le_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

fn read_f32(bytes: &[u8], offset: usize) -> Option<f32> {
    Some(f32::from_le_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

#[must_use]
pub fn scan_world_transform_candidates(
    bytes: &[u8],
    maximum_candidates: usize,
) -> (Vec<TransformCandidateObservation>, bool) {
    let limit = maximum_candidates.min(MAX_TRANSFORM_CANDIDATES_PER_SNAPSHOT);
    if limit == 0 || bytes.len() < WORLD_TRANSFORM_BYTES {
        return (Vec::new(), false);
    }
    let mut candidates = Vec::new();
    let mut truncated = false;
    // Generated WorldTransform is 0x10-aligned, but its containing class field need only be
    // observed at a four-byte offset until exact RTTI property metadata proves stricter alignment.
    for offset in (0..=bytes.len() - WORLD_TRANSFORM_BYTES).step_by(4) {
        let raw_position_bits: Option<[i32; 3]> = (0..3)
            .map(|axis| read_i32(bytes, offset + axis * 4))
            .collect::<Option<Vec<_>>>()
            .and_then(|values| values.try_into().ok());
        let Some(raw_position_bits) = raw_position_bits else {
            continue;
        };
        let position_world_units =
            raw_position_bits.map(|value| f64::from(value) / WORLD_POSITION_DIVISOR);
        if !position_world_units
            .iter()
            .all(|value| value.is_finite() && value.abs() <= MAX_ABS_WORLD_UNITS)
        {
            continue;
        }
        let raw_quaternion_ijkr: Option<[f32; 4]> = (0..4)
            .map(|axis| read_f32(bytes, offset + WORLD_ORIENTATION_OFFSET + axis * 4))
            .collect::<Option<Vec<_>>>()
            .and_then(|values| values.try_into().ok());
        let Some(raw_quaternion_ijkr) = raw_quaternion_ijkr else {
            continue;
        };
        if !raw_quaternion_ijkr.iter().all(|value| value.is_finite()) {
            continue;
        }
        let norm = raw_quaternion_ijkr
            .iter()
            .map(|value| f64::from(*value).powi(2))
            .sum::<f64>()
            .sqrt();
        let norm_error = (norm - 1.0).abs();
        if norm_error > QUATERNION_NORM_TOLERANCE {
            continue;
        }
        if candidates.len() == limit {
            truncated = true;
            break;
        }
        candidates.push(TransformCandidateObservation {
            transform_offset: offset as u32,
            raw_position_bits,
            position_world_units,
            raw_quaternion_ijkr,
            quaternion_norm_error: norm_error,
        });
    }
    (candidates, truncated)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateTrajectorySummary {
    pub role: PoseRole,
    pub resolver_id: String,
    pub object_tag: u64,
    pub transform_offset: u32,
    pub observation_count: u64,
    pub first_present_seq: u64,
    pub last_present_seq: u64,
    pub scenarios: Vec<DiscoveryScenario>,
    pub position_world_units_min: [f64; 3],
    pub position_world_units_max: [f64; 3],
    pub max_displacement_from_first_world_units: f64,
    pub position_changed_from_first: bool,
    /// Sign-invariant angular distance from the first quaternion. This is discovery evidence in
    /// degrees, not proof that the field is the active camera/player orientation.
    pub max_orientation_angle_degrees_from_first: f64,
    pub orientation_changed_from_first: bool,
    pub maximum_quaternion_norm_error: f64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimePoseBlocker {
    pub code: String,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cp2077PoseRuntimeReport {
    pub schema: String,
    pub exact_build: Cp2077ExactBuild,
    pub product_version: String,
    /// Opaque caller-supplied launch identity; never a PID.
    pub process_launch_id: String,
    pub address_catalog: AddressCatalogObservation,
    /// Present only when bootstrap verified all pinned code windows in the loaded module.
    pub player_abi_audit: Option<Cp2077PlayerAbiAudit>,
    pub resolvers: Vec<ResolverObservation>,
    pub calibration_markers: Vec<CalibrationMarker>,
    pub snapshot_observations: Vec<PoseSnapshotObservation>,
    pub render_present_inputs: Vec<RenderPresentDiscoveryInput>,
    pub system_enumerations: Vec<RuntimeSystemEnumerationObservation>,
    /// Only offsets still present in the final monotonic frontier for their role/resolver/object
    /// are summarized. Earlier structural false positives remain available in
    /// `snapshot_observations`, but must not look like surviving trajectories.
    pub trajectories: Vec<CandidateTrajectorySummary>,
    pub absolute_process_addresses_recorded: bool,
    pub production_binding_emitted: bool,
    pub recording_ready: bool,
    pub blockers: Vec<RuntimePoseBlocker>,
}

struct ObjectGenerationFrontier {
    current_object_tag: u64,
    retired_object_tags: BTreeSet<u64>,
    last_present_seq: u64,
    last_qpc: i64,
}

pub struct Cp2077PoseRuntimeSession {
    exact_build: Cp2077ExactBuild,
    process_launch_id: String,
    address_catalog: AddressCatalogObservation,
    player_abi_audit: Option<Cp2077PlayerAbiAudit>,
    next_object_tag: u64,
    pointer_tags: BTreeMap<usize, u64>,
    resolvers: BTreeMap<String, ResolverObservation>,
    calibration_markers: Vec<CalibrationMarker>,
    snapshots: Vec<PoseSnapshotObservation>,
    /// Per-object offset frontier. A structural candidate must survive every subsequent bounded
    /// snapshot of that object; offsets that disappear are never silently re-admitted later in
    /// the launch. This keeps the report bounded and makes cross-Present continuity explicit.
    candidate_offset_frontiers: BTreeMap<(PoseRole, String, u64), BTreeSet<u32>>,
    /// One resolver may legitimately replace its live game object during loading/spawn. Object
    /// generations are strictly serial: the latest tag is current and every superseded tag is
    /// retired permanently. A retired tag reappearing would be ABA/identity ambiguity and is
    /// rejected instead of being guessed into one trajectory.
    object_generations: BTreeMap<(PoseRole, String), ObjectGenerationFrontier>,
    render_present_inputs: Vec<RenderPresentDiscoveryInput>,
    system_enumerations: Vec<RuntimeSystemEnumerationObservation>,
    blockers: Vec<RuntimePoseBlocker>,
}

impl Cp2077PoseRuntimeSession {
    pub fn from_catalog_observation(
        process_launch_id: impl Into<String>,
        address_catalog: AddressCatalogObservation,
    ) -> Result<Self, String> {
        let process_launch_id = process_launch_id.into();
        if !valid_process_launch_id(&process_launch_id) {
            return Err(
                "CP2077 process_launch_id must be a safe opaque identifier or 32 lowercase hex"
                    .into(),
            );
        }
        Ok(Self {
            exact_build: Cp2077ExactBuild::pinned(),
            process_launch_id,
            address_catalog,
            player_abi_audit: None,
            next_object_tag: 1,
            pointer_tags: BTreeMap::new(),
            resolvers: BTreeMap::new(),
            calibration_markers: Vec::new(),
            snapshots: Vec::new(),
            candidate_offset_frontiers: BTreeMap::new(),
            object_generations: BTreeMap::new(),
            render_present_inputs: Vec::new(),
            system_enumerations: Vec::new(),
            blockers: Vec::new(),
        })
    }

    /// Exact loaded-image ABI evidence attached only by [`bootstrap_current_process`].
    #[must_use]
    pub fn player_abi_audit(&self) -> Option<&Cp2077PlayerAbiAudit> {
        self.player_abi_audit.as_ref()
    }

    #[must_use]
    pub fn address_catalog(&self) -> &AddressCatalogObservation {
        &self.address_catalog
    }

    pub fn record_discovery_blocker(&mut self, code: &str, detail: impl Into<String>) {
        if !self.blockers.iter().any(|blocker| blocker.code == code) {
            self.blockers.push(RuntimePoseBlocker {
                code: code.into(),
                detail: detail.into(),
            });
        }
    }

    /// Removes a provisional discovery diagnostic once later observations in
    /// the same process launch have established the exact route.  This is only
    /// for initialization-time absence; integrity, ambiguity and drift
    /// blockers remain sticky.
    pub fn clear_provisional_discovery_blocker(&mut self, code: &str) {
        self.blockers.retain(|blocker| blocker.code != code);
    }

    /// Convert an ephemeral in-process address into a report-safe ordinal. The raw address is
    /// retained only in this session's private map and is never serialized.
    pub fn register_ephemeral_object(&mut self, address: usize) -> Result<u64, String> {
        if address == 0 {
            return Err("cannot tag a null runtime object".into());
        }
        if let Some(tag) = self.pointer_tags.get(&address) {
            return Ok(*tag);
        }
        if self.pointer_tags.len() >= MAX_EPHEMERAL_OBJECTS {
            return Err(format!(
                "runtime object tag ceiling {MAX_EPHEMERAL_OBJECTS} exhausted"
            ));
        }
        let tag = self.next_object_tag;
        self.next_object_tag = self
            .next_object_tag
            .checked_add(1)
            .ok_or("runtime object tag overflow")?;
        self.pointer_tags.insert(address, tag);
        Ok(tag)
    }

    pub fn register_resolver(&mut self, resolver: ResolverObservation) -> Result<(), String> {
        if !valid_opaque_id(&resolver.resolver_id) {
            return Err("runtime resolver_id is not a safe opaque identifier".into());
        }
        if resolver.source == RuntimeResolverSource::FirstPartyPlayerSpawnEvent {
            let symbol = self
                .address_catalog
                .retained_symbols
                .iter()
                .find(|symbol| symbol.symbol == CP2077_PLAYER_SPAWN_SYMBOL)
                .ok_or("player-spawn symbol is absent from exact catalog")?;
            if resolver.role != PoseRole::Player
                || resolver.anchor_symbol.as_deref() != Some(CP2077_PLAYER_SPAWN_SYMBOL)
                || resolver.anchor_rva != symbol.rva
            {
                return Err(
                    "player-spawn resolver does not match the exact first-party symbol".into(),
                );
            }
        }
        if self.resolvers.contains_key(&resolver.resolver_id) {
            return Err(format!(
                "duplicate runtime resolver_id {}",
                resolver.resolver_id
            ));
        }
        self.resolvers
            .insert(resolver.resolver_id.clone(), resolver);
        Ok(())
    }

    #[must_use]
    pub fn has_resolver(&self, resolver_id: &str) -> bool {
        self.resolvers.contains_key(resolver_id)
    }

    #[must_use]
    pub fn resolver_has_pointer_offsets(&self, resolver_id: &str, offsets: &[u32]) -> bool {
        self.resolvers
            .get(resolver_id)
            .is_some_and(|resolver| resolver.pointer_offsets == offsets)
    }

    pub fn add_calibration_marker(&mut self, marker: CalibrationMarker) -> Result<(), String> {
        if marker.marker_id.trim().is_empty()
            || !valid_opaque_id(&marker.marker_id)
            || marker.motion_label.trim().is_empty()
            || marker.present_seq == 0
            || marker.qpc <= 0
        {
            return Err("invalid CP2077 calibration marker".into());
        }
        if self
            .calibration_markers
            .iter()
            .any(|existing| existing.marker_id == marker.marker_id)
        {
            return Err(format!("duplicate calibration marker {}", marker.marker_id));
        }
        self.calibration_markers.push(marker);
        Ok(())
    }

    pub fn observe_snapshot(
        &mut self,
        input: SnapshotObservationInput<'_>,
    ) -> Result<usize, String> {
        let SnapshotObservationInput {
            role,
            resolver_id,
            object_tag,
            present_seq,
            qpc,
            scenario,
            bytes,
        } = input;
        if self.snapshots.len() >= MAX_RUNTIME_OBSERVATIONS {
            return Err("CP2077 runtime observation budget exhausted".into());
        }
        let resolver = self
            .resolvers
            .get(resolver_id)
            .ok_or_else(|| format!("unknown runtime resolver {resolver_id}"))?;
        if resolver.role != role {
            return Err("snapshot role does not match its resolver".into());
        }
        if object_tag == 0 || !self.pointer_tags.values().any(|tag| *tag == object_tag) {
            return Err("snapshot object tag was not registered by this session".into());
        }
        if present_seq == 0 || qpc <= 0 {
            return Err("snapshot selected-Present identity is invalid".into());
        }
        if let Some(previous) = self.snapshots.iter().rev().find(|snapshot| {
            snapshot.role == role
                && snapshot.resolver_id == resolver_id
                && snapshot.object_tag == object_tag
        }) && (present_seq <= previous.present_seq || qpc <= previous.qpc)
        {
            return Err("snapshot timeline did not increase for this candidate object".into());
        }
        let generation_key = (role, resolver_id.to_string());
        if let Some(generation) = self.object_generations.get(&generation_key) {
            if present_seq <= generation.last_present_seq || qpc <= generation.last_qpc {
                return Err(
                    "snapshot timeline did not increase across runtime object generations".into(),
                );
            }
            if object_tag != generation.current_object_tag
                && generation.retired_object_tags.contains(&object_tag)
            {
                return Err(
                    "a retired runtime object reappeared after resolver object replacement".into(),
                );
            }
        }
        if bytes.len() > MAX_OBJECT_SNAPSHOT_BYTES {
            return Err(format!(
                "object snapshot exceeds {MAX_OBJECT_SNAPSHOT_BYTES} byte discovery ceiling"
            ));
        }
        let (mut candidates, candidates_truncated) =
            scan_world_transform_candidates(bytes, MAX_TRANSFORM_CANDIDATES_PER_SNAPSHOT);
        let frontier_key = (role, resolver_id.to_string(), object_tag);
        if let Some(previous_offsets) = self.candidate_offset_frontiers.get(&frontier_key) {
            candidates.retain(|candidate| previous_offsets.contains(&candidate.transform_offset));
        }
        self.candidate_offset_frontiers.insert(
            frontier_key,
            candidates
                .iter()
                .map(|candidate| candidate.transform_offset)
                .collect(),
        );
        match self.object_generations.entry(generation_key) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(ObjectGenerationFrontier {
                    current_object_tag: object_tag,
                    retired_object_tags: BTreeSet::new(),
                    last_present_seq: present_seq,
                    last_qpc: qpc,
                });
            }
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                let generation = entry.get_mut();
                if generation.current_object_tag != object_tag {
                    generation
                        .retired_object_tags
                        .insert(generation.current_object_tag);
                    generation.current_object_tag = object_tag;
                }
                generation.last_present_seq = present_seq;
                generation.last_qpc = qpc;
            }
        }
        let count = candidates.len();
        self.snapshots.push(PoseSnapshotObservation {
            role,
            resolver_id: resolver_id.into(),
            object_tag,
            present_seq,
            qpc,
            scenario,
            source_snapshot_bytes: bytes.len() as u32,
            candidates,
            candidates_truncated,
        });
        Ok(count)
    }

    pub fn observe_render_present_input(
        &mut self,
        input: RenderPresentDiscoveryInput,
    ) -> Result<(), String> {
        if self.render_present_inputs.len() >= MAX_RENDER_PRESENT_INPUTS {
            return Err("CP2077 render-Present observation budget exhausted".into());
        }
        if input.qpc <= 0 || input.generation == 0 {
            return Err("CP2077 render-Present identity is invalid".into());
        }
        if self.render_present_inputs.last().is_some_and(|previous| {
            input.present_seq <= previous.present_seq || input.qpc <= previous.qpc
        }) {
            return Err("CP2077 render-Present timeline did not strictly increase".into());
        }
        if input.camera_candidate_admitted
            && (!input.new_pre_clear_snapshot_since_previous_present
                || !input.capture_safe
                || !input.physical_view_matrix_source_observed
                || !input.physical_projection_matrix_source_observed)
        {
            return Err("CP2077 render camera candidate bypassed a required input".into());
        }
        self.render_present_inputs.push(input);
        Ok(())
    }

    pub fn observe_system_enumeration(
        &mut self,
        observation: RuntimeSystemEnumerationObservation,
    ) -> Result<(), String> {
        if self.system_enumerations.len() >= MAX_SYSTEM_ENUMERATIONS {
            return Err("CP2077 system-enumeration observation budget exhausted".into());
        }
        if observation.poll_ordinal == 0
            || observation.qpc <= 0
            || observation.candidates.len() > MAX_SYSTEM_CANDIDATES_PER_ENUMERATION
            || observation.candidates.iter().any(|candidate| {
                candidate.object_tag == 0
                    || !self
                        .pointer_tags
                        .values()
                        .any(|tag| *tag == candidate.object_tag)
                    || !candidate.handle_instance_readable
                    || !candidate.handle_refcount_readable
                    || candidate.handle_strong_ref_count == 0
                    || candidate.handle_weak_ref_count == 0
                    || !candidate.object_vtable_readable
                    || candidate.vtable_module_rva.is_none()
                    || (candidate.native_type_name.is_some()
                        != candidate.native_type_bytes.is_some())
                    || (candidate.native_type_name.is_some()
                        && !candidate.native_type_descriptor_readable)
                    || (candidate.player_system_type_match
                        && (candidate.native_type_name.as_deref() != Some("cpPlayerSystem")
                            || candidate.native_type_bytes
                                != Some(CP2077_PLAYER_SYSTEM_TYPE_BYTES)))
                    || (candidate.camera_system_type_match
                        && (candidate.native_type_name.as_deref() != Some("gameCameraSystem")
                            || candidate.native_type_bytes
                                != Some(CP2077_CAMERA_SYSTEM_TYPE_BYTES)))
            })
        {
            return Err(
                "CP2077 system-enumeration observation is invalid or overclaims RTTI".into(),
            );
        }
        if self.system_enumerations.last().is_some_and(|previous| {
            observation.poll_ordinal <= previous.poll_ordinal || observation.qpc <= previous.qpc
        }) {
            return Err("CP2077 system-enumeration timeline did not strictly increase".into());
        }
        self.system_enumerations.push(observation);
        Ok(())
    }

    #[must_use]
    pub fn finish(self) -> Cp2077PoseRuntimeReport {
        let mut blockers = self.blockers;
        let mut block = |code: &str, detail: String| {
            blockers.push(RuntimePoseBlocker {
                code: code.into(),
                detail,
            });
        };
        if self.address_catalog.camera_symbol_count == 0 {
            block(
                "first_party_camera_anchor_missing",
                "the exact game address catalog has no named Camera symbol".into(),
            );
        }
        if self.player_abi_audit.is_none() {
            block(
                "player_abi_bootstrap_missing",
                "the loaded-module exact byte audit was not attached to this session".into(),
            );
        }
        if self.address_catalog.world_transform_symbol_count == 0 {
            block(
                "first_party_world_transform_anchor_missing",
                "the exact game address catalog has no named WorldTransform symbol".into(),
            );
        }
        let resolver_values: Vec<_> = self.resolvers.into_values().collect();
        for role in [PoseRole::Camera, PoseRole::Player] {
            let matching: Vec<_> = resolver_values
                .iter()
                .filter(|resolver| resolver.role == role)
                .collect();
            if matching.is_empty() {
                block(
                    "runtime_resolver_missing",
                    format!("no {role:?} resolver was observed"),
                );
            }
            for resolver in matching {
                if !resolver.hook_abi.admitted(resolver.source) {
                    block(
                        "resolver_abi_not_admitted",
                        format!("{} has no audited source ABI", resolver.resolver_id),
                    );
                }
                if resolver.pointer_offsets.is_empty() {
                    block(
                        "resolver_chain_missing",
                        format!("{} has no audited object chain", resolver.resolver_id),
                    );
                }
                if resolver.object_type_bytes.is_none_or(|size| size == 0) {
                    block(
                        "rtti_object_extent_missing",
                        format!("{} has no exact RTTI object extent", resolver.resolver_id),
                    );
                }
            }
        }
        let camera_ids: BTreeSet<_> = resolver_values
            .iter()
            .filter(|resolver| resolver.role == PoseRole::Camera)
            .map(|resolver| resolver.resolver_id.as_str())
            .collect();
        let player_ids: BTreeSet<_> = resolver_values
            .iter()
            .filter(|resolver| resolver.role == PoseRole::Player)
            .map(|resolver| resolver.resolver_id.as_str())
            .collect();
        if !camera_ids.is_disjoint(&player_ids) {
            block(
                "camera_player_resolver_alias",
                "camera and player resolver identities overlap".into(),
            );
        }
        for camera in resolver_values
            .iter()
            .filter(|resolver| resolver.role == PoseRole::Camera)
        {
            for player in resolver_values
                .iter()
                .filter(|resolver| resolver.role == PoseRole::Player)
            {
                if camera.anchor_rva.is_some()
                    && camera.anchor_rva == player.anchor_rva
                    && camera.pointer_offsets == player.pointer_offsets
                {
                    block(
                        "camera_player_chain_alias",
                        format!(
                            "{} and {} resolve through one physical chain",
                            camera.resolver_id, player.resolver_id
                        ),
                    );
                }
            }
        }
        let camera_object_tags: BTreeSet<_> = self
            .snapshots
            .iter()
            .filter(|snapshot| snapshot.role == PoseRole::Camera)
            .map(|snapshot| snapshot.object_tag)
            .collect();
        let player_object_tags: BTreeSet<_> = self
            .snapshots
            .iter()
            .filter(|snapshot| snapshot.role == PoseRole::Player)
            .map(|snapshot| snapshot.object_tag)
            .collect();
        if !camera_object_tags.is_disjoint(&player_object_tags) {
            block(
                "camera_player_object_alias",
                "camera and player observations reused one ephemeral object".into(),
            );
        }
        let required_scenarios = [
            DiscoveryScenario::StationaryHeadSweep,
            DiscoveryScenario::OnFootTranslation,
            DiscoveryScenario::AxisDirectionCalibration,
            DiscoveryScenario::TerminalLoopReturn,
        ];
        let marker_scenarios: BTreeSet<_> = self
            .calibration_markers
            .iter()
            .map(|marker| marker.scenario)
            .collect();
        for scenario in required_scenarios {
            if !marker_scenarios.contains(&scenario) {
                block("calibration_scenario_missing", format!("{scenario:?}"));
            }
        }
        if self.snapshots.is_empty() {
            block(
                "runtime_pose_snapshots_missing",
                "no safe-point object snapshots were observed".into(),
            );
        }
        if self.render_present_inputs.is_empty() {
            block(
                "render_present_inputs_missing",
                "no D3D12 Present observation reached pose discovery".into(),
            );
        } else if !self
            .render_present_inputs
            .iter()
            .any(|input| input.physical_view_matrix_source_observed)
        {
            block(
                "render_view_matrix_source_missing",
                "D3D12 discovery observed no physical view-matrix source".into(),
            );
        }
        if !self.render_present_inputs.is_empty()
            && !self
                .render_present_inputs
                .iter()
                .any(|input| input.physical_projection_matrix_source_observed)
        {
            block(
                "render_projection_matrix_source_missing",
                "D3D12 discovery observed no physical projection-matrix source".into(),
            );
        }
        if !self.render_present_inputs.is_empty()
            && !self
                .render_present_inputs
                .iter()
                .any(|input| input.camera_candidate_admitted)
        {
            block(
                "render_camera_candidate_not_admitted",
                "no Present observation satisfied every render-camera admission input".into(),
            );
        }
        if self.system_enumerations.is_empty() {
            block(
                "first_party_system_enumeration_missing",
                "CGameEngine/GameInstance system candidates were not enumerated".into(),
            );
        } else {
            if !self.system_enumerations.iter().any(|observation| {
                observation
                    .candidates
                    .iter()
                    .any(|candidate| candidate.player_system_type_match)
            }) {
                block(
                    "player_system_rtti_match_missing",
                    "no bounded system candidate has exact cpPlayerSystem CName hash and size"
                        .into(),
                );
            }
            if !self.system_enumerations.iter().any(|observation| {
                observation
                    .candidates
                    .iter()
                    .any(|candidate| candidate.camera_system_type_match)
            }) {
                block(
                    "camera_system_rtti_match_missing",
                    "no bounded system candidate has exact gameCameraSystem CName hash and size"
                        .into(),
                );
            }
        }
        if self
            .snapshots
            .iter()
            .any(|snapshot| snapshot.candidates_truncated)
        {
            block(
                "transform_candidate_set_truncated",
                "at least one snapshot exceeded the candidate evidence ceiling".into(),
            );
        }
        if self
            .snapshots
            .iter()
            .any(|snapshot| snapshot.candidates.is_empty())
        {
            block(
                "world_transform_candidate_missing",
                "at least one snapshot had no structural WorldTransform candidate".into(),
            );
        }
        let trajectories = summarize_trajectories(&self.snapshots);
        if trajectories.is_empty() {
            block(
                "candidate_trajectories_missing",
                "no transform offset survived across runtime observations".into(),
            );
        }
        for role in [PoseRole::Camera, PoseRole::Player] {
            let role_label = match role {
                PoseRole::Camera => "camera",
                PoseRole::Player => "player",
            };
            let role_snapshot_count = self
                .snapshots
                .iter()
                .filter(|snapshot| snapshot.role == role)
                .count();
            if role_snapshot_count == 0 {
                continue;
            }
            let surviving_offsets: BTreeSet<_> = trajectories
                .iter()
                .filter(|trajectory| trajectory.role == role)
                .map(|trajectory| trajectory.transform_offset)
                .collect();
            match surviving_offsets.len() {
                0 => block(
                    &format!("{role_label}_world_transform_candidate_missing"),
                    format!(
                        "{role_snapshot_count} exact-object snapshots produced no surviving structural WorldTransform offset"
                    ),
                ),
                1 => {}
                count => block(
                    &format!("{role_label}_world_transform_candidate_not_unique"),
                    format!(
                        "{role_snapshot_count} exact-object snapshots retained {count} structural WorldTransform offsets"
                    ),
                ),
            }
        }
        // Structural candidates and raw motion are necessary but not sufficient. An audited
        // binding still requires cross-launch stability, independent camera/player semantics,
        // axis/metric calibration and E-geo in cp2077_pose_probe.
        block(
            "production_binding_not_authored",
            "runtime observations are discovery evidence only".into(),
        );
        Cp2077PoseRuntimeReport {
            schema: CP2077_POSE_RUNTIME_REPORT_SCHEMA_V1.into(),
            exact_build: self.exact_build,
            product_version: CP2077_PRODUCT_VERSION.into(),
            process_launch_id: self.process_launch_id,
            address_catalog: self.address_catalog,
            player_abi_audit: self.player_abi_audit,
            resolvers: resolver_values,
            calibration_markers: self.calibration_markers,
            snapshot_observations: self.snapshots,
            render_present_inputs: self.render_present_inputs,
            system_enumerations: self.system_enumerations,
            trajectories,
            absolute_process_addresses_recorded: false,
            production_binding_emitted: false,
            recording_ready: false,
            blockers,
        }
    }
}

fn valid_opaque_id(value: &str) -> bool {
    let value = value.trim();
    !value.is_empty()
        && value.len() <= 128
        && !value.bytes().all(|byte| byte.is_ascii_digit())
        && !value.to_ascii_lowercase().contains("0x")
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn valid_process_launch_id(value: &str) -> bool {
    valid_opaque_id(value)
        || (value.len() == 32
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)))
}

type TrajectoryKey = (PoseRole, String, u64, u32);

fn summarize_trajectories(
    snapshots: &[PoseSnapshotObservation],
) -> Vec<CandidateTrajectorySummary> {
    type FrontierKey = (PoseRole, String, u64);

    // Loading/spawn may replace the exact live object behind one resolver. `observe_snapshot`
    // proves those generations are strictly serial and rejects retired-tag ABA. Keep the last
    // object generation for each role/resolver in the trajectory surface while retaining every
    // raw snapshot in the report for audit. This prevents an initialization object from looking
    // like a concurrent survivor beside the current player.
    let mut current_object_tags: BTreeMap<(PoseRole, String), u64> = BTreeMap::new();
    for snapshot in snapshots {
        current_object_tags.insert(
            (snapshot.role, snapshot.resolver_id.clone()),
            snapshot.object_tag,
        );
    }

    // Each current object's offset frontier remains monotonic: once an offset disappears it is
    // never re-admitted. Keep only its last frontier so rejected structural false positives do
    // not reappear in the summary.
    let mut final_frontiers: BTreeMap<FrontierKey, BTreeSet<u32>> = BTreeMap::new();
    for snapshot in snapshots {
        if current_object_tags.get(&(snapshot.role, snapshot.resolver_id.clone()))
            != Some(&snapshot.object_tag)
        {
            continue;
        }
        final_frontiers.insert(
            (
                snapshot.role,
                snapshot.resolver_id.clone(),
                snapshot.object_tag,
            ),
            snapshot
                .candidates
                .iter()
                .map(|candidate| candidate.transform_offset)
                .collect(),
        );
    }

    let mut observations: BTreeMap<
        TrajectoryKey,
        Vec<(u64, DiscoveryScenario, &TransformCandidateObservation)>,
    > = BTreeMap::new();
    for snapshot in snapshots {
        if current_object_tags.get(&(snapshot.role, snapshot.resolver_id.clone()))
            != Some(&snapshot.object_tag)
        {
            continue;
        }
        for candidate in &snapshot.candidates {
            let frontier_key = (
                snapshot.role,
                snapshot.resolver_id.clone(),
                snapshot.object_tag,
            );
            if !final_frontiers
                .get(&frontier_key)
                .is_some_and(|offsets| offsets.contains(&candidate.transform_offset))
            {
                continue;
            }
            observations
                .entry((
                    snapshot.role,
                    snapshot.resolver_id.clone(),
                    snapshot.object_tag,
                    candidate.transform_offset,
                ))
                .or_default()
                .push((snapshot.present_seq, snapshot.scenario, candidate));
        }
    }
    observations
        .into_iter()
        .filter_map(
            |((role, resolver_id, object_tag, transform_offset), mut rows)| {
                rows.sort_by_key(|row| row.0);
                let first = rows.first()?.2.position_world_units;
                let mut minimum = first;
                let mut maximum = first;
                let mut max_displacement = 0.0_f64;
                let first_quaternion = rows.first()?.2.raw_quaternion_ijkr;
                let mut max_orientation_angle_degrees = 0.0_f64;
                let mut max_norm_error = 0.0_f64;
                let scenarios: BTreeSet<_> = rows.iter().map(|row| row.1).collect();
                for (_, _, row) in &rows {
                    for axis in 0..3 {
                        minimum[axis] = minimum[axis].min(row.position_world_units[axis]);
                        maximum[axis] = maximum[axis].max(row.position_world_units[axis]);
                    }
                    let displacement = (0..3)
                        .map(|axis| (row.position_world_units[axis] - first[axis]).powi(2))
                        .sum::<f64>()
                        .sqrt();
                    max_displacement = max_displacement.max(displacement);
                    let dot = first_quaternion
                        .iter()
                        .zip(row.raw_quaternion_ijkr.iter())
                        .map(|(left, right)| f64::from(*left) * f64::from(*right))
                        .sum::<f64>()
                        .abs()
                        .clamp(0.0, 1.0);
                    let angle_degrees = 2.0 * dot.acos().to_degrees();
                    max_orientation_angle_degrees =
                        max_orientation_angle_degrees.max(angle_degrees);
                    max_norm_error = max_norm_error.max(row.quaternion_norm_error);
                }
                Some(CandidateTrajectorySummary {
                    role,
                    resolver_id,
                    object_tag,
                    transform_offset,
                    observation_count: rows.len() as u64,
                    first_present_seq: rows.first()?.0,
                    last_present_seq: rows.last()?.0,
                    scenarios: scenarios.into_iter().collect(),
                    position_world_units_min: minimum,
                    position_world_units_max: maximum,
                    max_displacement_from_first_world_units: max_displacement,
                    position_changed_from_first: max_displacement > (1.0 / WORLD_POSITION_DIVISOR),
                    max_orientation_angle_degrees_from_first: max_orientation_angle_degrees,
                    orientation_changed_from_first: max_orientation_angle_degrees > 0.01,
                    maximum_quaternion_norm_error: max_norm_error,
                })
            },
        )
        .collect()
}

fn path_is_within(path: &Path, root: &Path) -> bool {
    let path = path
        .components()
        .map(|component| component.as_os_str().to_string_lossy().to_ascii_lowercase())
        .collect::<Vec<_>>();
    let root = root
        .components()
        .map(|component| component.as_os_str().to_string_lossy().to_ascii_lowercase())
        .collect::<Vec<_>>();
    path.starts_with(&root)
}

pub fn write_report_create_new(
    path: &Path,
    game_root: &Path,
    report: &Cp2077PoseRuntimeReport,
) -> Result<(), String> {
    let parent = path.parent().ok_or("runtime report path has no parent")?;
    let parent = parent
        .canonicalize()
        .map_err(|error| format!("resolve runtime report parent failed: {error}"))?;
    let game_root = game_root
        .canonicalize()
        .map_err(|error| format!("resolve CP2077 game root failed: {error}"))?;
    if path_is_within(&parent, &game_root) {
        return Err("CP2077 runtime pose evidence must not be written into the game tree".into());
    }
    // This is a machine-sealed discovery artifact. Compact encoding preserves the exact schema
    // while keeping the fixed 4 MiB budget available for bounded cross-Present trajectories.
    let bytes = serde_json::to_vec(report)
        .map_err(|error| format!("serialize runtime pose report failed: {error}"))?;
    if bytes.len() > MAX_RUNTIME_REPORT_BYTES {
        return Err(format!(
            "runtime pose report exceeds {MAX_RUNTIME_REPORT_BYTES} byte ceiling"
        ));
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| format!("create runtime pose report failed: {error}"))?;
    file.write_all(&bytes)
        .map_err(|error| format!("write runtime pose report failed: {error}"))?;
    file.sync_all()
        .map_err(|error| format!("flush runtime pose report failed: {error}"))
}

#[cfg(windows)]
pub fn bootstrap_current_process(
    actual_executable_sha256: &str,
    process_launch_id: impl Into<String>,
    address_catalog_path: &Path,
) -> Result<Cp2077PoseRuntimeSession, String> {
    use sha2::{Digest, Sha256};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::System::ProcessStatus::{K32GetModuleInformation, MODULEINFO};
    use windows::Win32::System::Threading::GetCurrentProcess;

    if actual_executable_sha256 != CP2077_EXE_SHA256 {
        return Err(format!(
            "CP2077 runtime pose exact-build mismatch: expected {CP2077_EXE_SHA256}, got {actual_executable_sha256}"
        ));
    }
    let catalog_sha256 = crate::startup_timing::phase("address_catalog_hash", || {
        let mut hash_file = std::fs::File::open(address_catalog_path)
            .map_err(|error| format!("open CP2077 address catalog failed: {error}"))?;
        let mut hasher = Sha256::new();
        std::io::copy(&mut hash_file, &mut hasher)
            .map_err(|error| format!("hash CP2077 address catalog failed: {error}"))?;
        Ok::<_, String>(format!("sha256:{:x}", hasher.finalize()))
    })?;

    let module = unsafe { GetModuleHandleW(None) }
        .map_err(|error| format!("locate CP2077 main module failed: {error}"))?;
    let mut module_info = MODULEINFO::default();
    if !unsafe {
        K32GetModuleInformation(
            GetCurrentProcess(),
            module,
            &mut module_info,
            size_of::<MODULEINFO>() as u32,
        )
    }
    .as_bool()
    {
        return Err("query CP2077 main module image failed".into());
    }
    let image =
        unsafe { pe_image_layout(module_info.lpBaseOfDll.cast(), module_info.SizeOfImage) }?;
    let loaded_image = unsafe {
        std::slice::from_raw_parts(
            module_info.lpBaseOfDll.cast::<u8>(),
            module_info.SizeOfImage as usize,
        )
    };
    let player_abi_audit =
        audit_cp2077_player_abi_loaded_image(actual_executable_sha256, loaded_image)?;
    let catalog = crate::startup_timing::phase("address_catalog_parse", || {
        let catalog_file = std::fs::File::open(address_catalog_path)
            .map_err(|error| format!("reopen CP2077 address catalog failed: {error}"))?;
        parse_address_catalog(
            std::io::BufReader::with_capacity(2 * 1024 * 1024, catalog_file),
            &catalog_sha256,
            Some(&image),
        )
    })?;
    let player_spawn_rva = catalog
        .retained_symbols
        .iter()
        .find(|symbol| symbol.symbol == CP2077_PLAYER_SPAWN_SYMBOL)
        .ok_or("exact player-spawn symbol was not retained")?
        .rva
        .ok_or("exact player-spawn RVA was not resolved")?;
    let function = (module_info.lpBaseOfDll as usize)
        .checked_add(player_spawn_rva as usize)
        .ok_or("player-spawn function address overflow")?
        as *const core::ffi::c_void;
    if !executable_address(function) {
        return Err("resolved player-spawn symbol is not in executable committed memory".into());
    }

    let mut session =
        Cp2077PoseRuntimeSession::from_catalog_observation(process_launch_id, catalog)?;
    session.player_abi_audit = Some(player_abi_audit);
    Ok(session)
}

#[cfg(windows)]
unsafe fn pe_image_layout(
    module_base: *const u8,
    process_image_size: u32,
) -> Result<ModuleImageLayout, String> {
    if module_base.is_null() || process_image_size < 4096 {
        return Err("invalid CP2077 module image".into());
    }
    let image = unsafe { std::slice::from_raw_parts(module_base, process_image_size as usize) };
    if image.get(0..2) != Some(b"MZ") {
        return Err("CP2077 main module has no DOS signature".into());
    }
    let pe_offset = u32::from_le_bytes(
        image
            .get(0x3c..0x40)
            .ok_or("CP2077 DOS header is truncated")?
            .try_into()
            .map_err(|_| "CP2077 DOS offset width is invalid")?,
    ) as usize;
    if image.get(pe_offset..pe_offset + 4) != Some(b"PE\0\0") {
        return Err("CP2077 main module has no PE signature".into());
    }
    let coff = pe_offset + 4;
    let section_count = u16::from_le_bytes(
        image
            .get(coff + 2..coff + 4)
            .ok_or("CP2077 COFF header is truncated")?
            .try_into()
            .map_err(|_| "CP2077 section count width is invalid")?,
    ) as usize;
    let optional_bytes = u16::from_le_bytes(
        image
            .get(coff + 16..coff + 18)
            .ok_or("CP2077 COFF optional-header size is truncated")?
            .try_into()
            .map_err(|_| "CP2077 optional-header size width is invalid")?,
    ) as usize;
    if section_count == 0 || section_count > 96 {
        return Err("CP2077 PE section count is outside the discovery ceiling".into());
    }
    let section_table = coff
        .checked_add(20)
        .and_then(|offset| offset.checked_add(optional_bytes))
        .ok_or("CP2077 PE section table overflow")?;
    let mut linker_segment_rvas = [None; 3];
    for index in 0..section_count {
        let header = section_table
            .checked_add(index * 40)
            .ok_or("CP2077 PE section header overflow")?;
        let name_bytes = image
            .get(header..header + 8)
            .ok_or("CP2077 PE section name is truncated")?;
        let name_end = name_bytes
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(name_bytes.len());
        let name = std::str::from_utf8(&name_bytes[..name_end])
            .map_err(|_| "CP2077 PE section name is not ASCII")?;
        let virtual_address = u32::from_le_bytes(
            image
                .get(header + 12..header + 16)
                .ok_or("CP2077 PE section header is truncated")?
                .try_into()
                .map_err(|_| "CP2077 section RVA width is invalid")?,
        );
        let slot = match name {
            ".text" => Some(0),
            ".rdata" => Some(1),
            ".data" => Some(2),
            _ => None,
        };
        if let Some(slot) = slot
            && linker_segment_rvas[slot].replace(virtual_address).is_some()
        {
            return Err(format!("CP2077 PE has duplicate {name} sections"));
        }
    }
    let linker_segment_rvas = [
        linker_segment_rvas[0].ok_or("CP2077 PE .text section is absent")?,
        linker_segment_rvas[1].ok_or("CP2077 PE .rdata section is absent")?,
        linker_segment_rvas[2].ok_or("CP2077 PE .data section is absent")?,
    ];
    Ok(ModuleImageLayout {
        image_size: process_image_size,
        linker_segment_rvas,
    })
}

#[cfg(windows)]
fn executable_address(address: *const core::ffi::c_void) -> bool {
    use windows::Win32::System::Memory::{
        MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_EXECUTE, PAGE_EXECUTE_READ,
        PAGE_EXECUTE_READWRITE, PAGE_EXECUTE_WRITECOPY, PAGE_GUARD, PAGE_NOACCESS, VirtualQuery,
    };

    let mut info = MEMORY_BASIC_INFORMATION::default();
    let queried = unsafe {
        VirtualQuery(
            Some(address),
            &mut info,
            size_of::<MEMORY_BASIC_INFORMATION>(),
        )
    };
    queried == size_of::<MEMORY_BASIC_INFORMATION>()
        && info.State == MEM_COMMIT
        && !info.Protect.contains(PAGE_GUARD)
        && !info.Protect.contains(PAGE_NOACCESS)
        && [
            PAGE_EXECUTE,
            PAGE_EXECUTE_READ,
            PAGE_EXECUTE_READWRITE,
            PAGE_EXECUTE_WRITECOPY,
        ]
        .contains(&info.Protect)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CATALOG_FIXTURE: &str = r#"{
      "Linker map timestamp":"68af45ea",
      "Preferred load address":"0000000140000000",
      "Code constant offset":"1000",
      "Addresses":[
        {"hash":"1","secondary hash":"a","symbol":"game::Noise","offset":"0001:00000010"},
        {"hash":"2","secondary hash":"b","offset":"0001:00000020"},
        {"hash":"2050111212","secondary hash":"98dac7c90b04329a564b0d828419c7f30d4b65f65173691fafcbe1eae5e4863f","symbol":"cp::PlayerSystem::OnPlayerMainObjectSpawned","offset":"0001:00a1352c"},
        {"hash":"1247874916","secondary hash":"a15fa31a13e6606cbc5344b6b481eb7909589ea7b37e34ebe376eafcd6e21900","offset":"0001:002f0080"},
        {"hash":"2549221846","secondary hash":"347be984cccfcc3d23c0f65e3184ff09d7f0d335563e76fa4672d23f71242dc9","offset":"0003:00144c00"}
      ]
    }"#;

    fn catalog() -> AddressCatalogObservation {
        parse_address_catalog(
            CATALOG_FIXTURE.as_bytes(),
            CP2077_ADDRESS_CATALOG_SHA256,
            Some(&ModuleImageLayout {
                image_size: 0x0400_0000,
                linker_segment_rvas: [0x1000, 0x0200_0000, 0x0300_0000],
            }),
        )
        .unwrap()
    }

    fn fnv1a64(value: &str) -> u64 {
        value.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x100_0000_01b3)
        })
    }

    fn write_transform(bytes: &mut [u8], offset: usize, position: [i32; 3], quat: [f32; 4]) {
        for (axis, value) in position.into_iter().enumerate() {
            bytes[offset + axis * 4..offset + axis * 4 + 4].copy_from_slice(&value.to_le_bytes());
        }
        for (axis, value) in quat.into_iter().enumerate() {
            let start = offset + WORLD_ORIENTATION_OFFSET + axis * 4;
            bytes[start..start + 4].copy_from_slice(&value.to_le_bytes());
        }
    }

    fn player_resolver(rva: u32) -> ResolverObservation {
        ResolverObservation {
            resolver_id: "player-spawn-rtti-candidate-v1".into(),
            role: PoseRole::Player,
            source: RuntimeResolverSource::FirstPartyPlayerSpawnEvent,
            anchor_symbol: Some(CP2077_PLAYER_SPAWN_SYMBOL.into()),
            anchor_rva: Some(rva),
            pointer_offsets: vec![0x18, 0x40],
            object_type_name: Some("PlayerPuppetCandidate".into()),
            object_type_bytes: Some(0x200),
            hook_abi: HookAbiEvidence::Validated {
                evidence_id: "synthetic-abi-v1".into(),
                original_forwarded_exactly_once: true,
            },
        }
    }

    #[test]
    fn streaming_catalog_keeps_only_the_three_exact_first_party_anchors() {
        let catalog = catalog();
        assert_eq!(catalog.total_address_rows, 5);
        assert_eq!(catalog.named_address_rows, 2);
        assert_eq!(catalog.camera_symbol_count, 0);
        assert_eq!(catalog.player_symbol_count, 1);
        assert_eq!(catalog.retained_symbols.len(), 3);
        assert_eq!(catalog.retained_symbols[0].rva, Some(0x00a1_452c));
        assert_eq!(catalog.retained_symbols[1].rva, Some(0x0314_4c00));
        assert_eq!(catalog.retained_symbols[2].rva, Some(0x002f_1080));
    }

    #[test]
    fn system_cname_hashes_are_reproducible_from_exact_type_names() {
        assert_eq!(fnv1a64("cpPlayerSystem"), CP2077_PLAYER_SYSTEM_CNAME_HASH);
        assert_eq!(fnv1a64("gameCameraSystem"), CP2077_CAMERA_SYSTEM_CNAME_HASH);
    }

    #[test]
    fn catalog_hash_and_exact_symbol_identity_are_pinned() {
        assert!(parse_address_catalog(CATALOG_FIXTURE.as_bytes(), "sha256:wrong", None).is_err());
        let changed = CATALOG_FIXTURE.replace("00a1352c", "00a1352d");
        assert!(
            parse_address_catalog(changed.as_bytes(), CP2077_ADDRESS_CATALOG_SHA256, None).is_err()
        );
    }

    #[test]
    fn transform_scan_reports_raw_units_without_claiming_metres() {
        let mut bytes = vec![0x55; 128];
        write_transform(
            &mut bytes,
            32,
            [131_072, -262_144, 65_536],
            [0.0, 0.0, 0.0, 1.0],
        );
        let (candidates, truncated) = scan_world_transform_candidates(&bytes, 8);
        assert!(!truncated);
        let candidate = candidates
            .iter()
            .find(|candidate| candidate.transform_offset == 32)
            .unwrap();
        assert_eq!(candidate.raw_position_bits, [131_072, -262_144, 65_536]);
        assert_eq!(candidate.position_world_units, [1.0, -2.0, 0.5]);
        assert_eq!(candidate.quaternion_norm_error, 0.0);
    }

    #[test]
    fn bad_quaternion_is_not_a_world_transform_candidate() {
        let mut bytes = vec![0x55; 64];
        write_transform(&mut bytes, 0, [1, 2, 3], [4.0, 0.0, 0.0, 0.0]);
        let (candidates, _) = scan_world_transform_candidates(&bytes, 8);
        assert!(
            candidates
                .iter()
                .all(|candidate| candidate.transform_offset != 0)
        );
    }

    #[test]
    fn pointers_are_replaced_by_stable_opaque_tags() {
        let mut session =
            Cp2077PoseRuntimeSession::from_catalog_observation("launch-a", catalog()).unwrap();
        assert_eq!(session.register_ephemeral_object(0x7fff_1234).unwrap(), 1);
        assert_eq!(session.register_ephemeral_object(0x7fff_1234).unwrap(), 1);
        assert_eq!(session.register_ephemeral_object(0x7fff_5678).unwrap(), 2);
        assert!(session.register_ephemeral_object(0).is_err());
        assert!(Cp2077PoseRuntimeSession::from_catalog_observation("12345", catalog()).is_err());
        assert!(
            Cp2077PoseRuntimeSession::from_catalog_observation(
                "01234567890123456789012345678901",
                catalog()
            )
            .is_ok()
        );
    }

    #[test]
    fn player_symbol_does_not_admit_unverified_hook_abi() {
        let catalog_observation = catalog();
        let rva = catalog_observation.retained_symbols[0].rva.unwrap();
        let mut session =
            Cp2077PoseRuntimeSession::from_catalog_observation("launch-a", catalog_observation)
                .unwrap();
        let mut resolver = player_resolver(rva);
        resolver.hook_abi = HookAbiEvidence::Unverified;
        session.register_resolver(resolver).unwrap();
        let report = session.finish();
        assert!(
            report
                .blockers
                .iter()
                .any(|blocker| blocker.code == "resolver_abi_not_admitted")
        );
        assert!(!report.production_binding_emitted);
        assert!(!report.recording_ready);
    }

    #[test]
    fn observation_is_bounded_and_keeps_selected_present_identity() {
        let catalog_observation = catalog();
        let rva = catalog_observation.retained_symbols[0].rva.unwrap();
        let mut session =
            Cp2077PoseRuntimeSession::from_catalog_observation("launch-a", catalog_observation)
                .unwrap();
        session.register_resolver(player_resolver(rva)).unwrap();
        let object_tag = session.register_ephemeral_object(0x7fff_1234).unwrap();
        let mut bytes = vec![0x55; 128];
        write_transform(&mut bytes, 32, [1, 2, 3], [0.0, 0.0, 0.0, 1.0]);
        let count = session
            .observe_snapshot(SnapshotObservationInput {
                role: PoseRole::Player,
                resolver_id: "player-spawn-rtti-candidate-v1",
                object_tag,
                present_seq: 100,
                qpc: 1_000,
                scenario: DiscoveryScenario::OnFootTranslation,
                bytes: &bytes,
            })
            .unwrap();
        assert!(count >= 1);
        let report = session.finish();
        assert_eq!(report.snapshot_observations[0].present_seq, 100);
        assert_eq!(report.snapshot_observations[0].qpc, 1_000);
        assert!(!report.absolute_process_addresses_recorded);
        let json = serde_json::to_string(&report).unwrap();
        assert!(!json.contains("7fff1234"));
    }

    #[test]
    fn a_candidate_object_timeline_must_be_strictly_increasing() {
        let catalog = catalog();
        let rva = catalog.retained_symbols[0].rva.unwrap();
        let mut session =
            Cp2077PoseRuntimeSession::from_catalog_observation("launch-a", catalog).unwrap();
        session.register_resolver(player_resolver(rva)).unwrap();
        let object_tag = session.register_ephemeral_object(0x7fff_1234).unwrap();
        let mut bytes = vec![0x55; 128];
        write_transform(&mut bytes, 32, [1, 2, 3], [0.0, 0.0, 0.0, 1.0]);
        let input = |present_seq, qpc| SnapshotObservationInput {
            role: PoseRole::Player,
            resolver_id: "player-spawn-rtti-candidate-v1",
            object_tag,
            present_seq,
            qpc,
            scenario: DiscoveryScenario::OnFootTranslation,
            bytes: &bytes,
        };
        session.observe_snapshot(input(100, 1_000)).unwrap();
        assert!(session.observe_snapshot(input(100, 1_001)).is_err());
        assert!(session.observe_snapshot(input(101, 999)).is_err());
    }

    #[test]
    fn serial_player_object_replacement_summarizes_only_the_current_generation() {
        let catalog = catalog();
        let rva = catalog.retained_symbols[0].rva.unwrap();
        let mut session =
            Cp2077PoseRuntimeSession::from_catalog_observation("launch-a", catalog).unwrap();
        session.register_resolver(player_resolver(rva)).unwrap();
        let loading_object = session.register_ephemeral_object(0x7fff_1234).unwrap();
        let spawned_object = session.register_ephemeral_object(0x7fff_5678).unwrap();
        let mut loading = vec![0x55; 128];
        let mut spawned_first = vec![0x55; 128];
        let mut spawned_second = vec![0x55; 128];
        write_transform(&mut loading, 32, [1, 2, 3], [0.0, 0.0, 0.0, 1.0]);
        write_transform(
            &mut spawned_first,
            32,
            [100, 200, 300],
            [0.0, 0.0, 0.0, 1.0],
        );
        write_transform(
            &mut spawned_second,
            32,
            [200, 200, 300],
            [0.0, 0.0, 0.0, 1.0],
        );
        for (object_tag, present_seq, qpc, bytes) in [
            (loading_object, 100, 1_000, &loading),
            (loading_object, 101, 1_001, &loading),
            (spawned_object, 200, 2_000, &spawned_first),
            (spawned_object, 201, 2_001, &spawned_second),
        ] {
            session
                .observe_snapshot(SnapshotObservationInput {
                    role: PoseRole::Player,
                    resolver_id: "player-spawn-rtti-candidate-v1",
                    object_tag,
                    present_seq,
                    qpc,
                    scenario: DiscoveryScenario::OnFootTranslation,
                    bytes,
                })
                .unwrap();
        }

        let report = session.finish();
        assert_eq!(report.snapshot_observations.len(), 4);
        let player_trajectories = report
            .trajectories
            .iter()
            .filter(|trajectory| trajectory.role == PoseRole::Player)
            .collect::<Vec<_>>();
        assert_eq!(player_trajectories.len(), 1);
        assert_eq!(player_trajectories[0].object_tag, spawned_object);
        assert_eq!(player_trajectories[0].first_present_seq, 200);
        assert!(player_trajectories[0].position_changed_from_first);
    }

    #[test]
    fn retired_player_object_cannot_reappear_after_replacement() {
        let catalog = catalog();
        let rva = catalog.retained_symbols[0].rva.unwrap();
        let mut session =
            Cp2077PoseRuntimeSession::from_catalog_observation("launch-a", catalog).unwrap();
        session.register_resolver(player_resolver(rva)).unwrap();
        let first_object = session.register_ephemeral_object(0x7fff_1234).unwrap();
        let second_object = session.register_ephemeral_object(0x7fff_5678).unwrap();
        let mut bytes = vec![0x55; 128];
        write_transform(&mut bytes, 32, [1, 2, 3], [0.0, 0.0, 0.0, 1.0]);
        let input = |object_tag, present_seq, qpc| SnapshotObservationInput {
            role: PoseRole::Player,
            resolver_id: "player-spawn-rtti-candidate-v1",
            object_tag,
            present_seq,
            qpc,
            scenario: DiscoveryScenario::OnFootTranslation,
            bytes: &bytes,
        };
        session
            .observe_snapshot(input(first_object, 100, 1_000))
            .unwrap();
        session
            .observe_snapshot(input(second_object, 200, 2_000))
            .unwrap();
        assert!(
            session
                .observe_snapshot(input(first_object, 300, 3_000))
                .unwrap_err()
                .contains("retired runtime object reappeared")
        );
    }

    #[test]
    fn vanished_transform_offset_is_not_re_admitted_within_a_launch() {
        let catalog = catalog();
        let rva = catalog.retained_symbols[0].rva.unwrap();
        let mut session =
            Cp2077PoseRuntimeSession::from_catalog_observation("launch-a", catalog).unwrap();
        session.register_resolver(player_resolver(rva)).unwrap();
        let object_tag = session.register_ephemeral_object(0x7fff_1234).unwrap();
        let mut first = vec![0x55; 128];
        write_transform(&mut first, 32, [1, 2, 3], [0.0, 0.0, 0.0, 1.0]);
        write_transform(&mut first, 64, [4, 5, 6], [0.0, 0.0, 0.0, 1.0]);
        let mut second = first.clone();
        write_transform(&mut second, 64, [4, 5, 6], [4.0, 0.0, 0.0, 0.0]);
        let third = first.clone();
        session
            .observe_snapshot(SnapshotObservationInput {
                role: PoseRole::Player,
                resolver_id: "player-spawn-rtti-candidate-v1",
                object_tag,
                present_seq: 100,
                qpc: 1_000,
                scenario: DiscoveryScenario::OnFootTranslation,
                bytes: &first,
            })
            .unwrap();
        session
            .observe_snapshot(SnapshotObservationInput {
                role: PoseRole::Player,
                resolver_id: "player-spawn-rtti-candidate-v1",
                object_tag,
                present_seq: 101,
                qpc: 1_001,
                scenario: DiscoveryScenario::OnFootTranslation,
                bytes: &second,
            })
            .unwrap();
        session
            .observe_snapshot(SnapshotObservationInput {
                role: PoseRole::Player,
                resolver_id: "player-spawn-rtti-candidate-v1",
                object_tag,
                present_seq: 102,
                qpc: 1_002,
                scenario: DiscoveryScenario::OnFootTranslation,
                bytes: &third,
            })
            .unwrap();
        let report = session.finish();
        assert!(
            report.snapshot_observations[0]
                .candidates
                .iter()
                .any(|candidate| candidate.transform_offset == 64)
        );
        assert!(report.snapshot_observations[1..].iter().all(|snapshot| {
            snapshot
                .candidates
                .iter()
                .all(|candidate| candidate.transform_offset != 64)
        }));
        assert!(report.snapshot_observations.iter().all(|snapshot| {
            snapshot
                .candidates
                .iter()
                .any(|candidate| candidate.transform_offset == 32)
        }));
        assert!(
            report
                .trajectories
                .iter()
                .any(|trajectory| trajectory.transform_offset == 32)
        );
        assert!(
            report
                .trajectories
                .iter()
                .all(|trajectory| trajectory.transform_offset != 64)
        );
    }

    #[test]
    fn trajectory_reports_sign_invariant_orientation_motion() {
        let catalog = catalog();
        let rva = catalog.retained_symbols[0].rva.unwrap();
        let mut session =
            Cp2077PoseRuntimeSession::from_catalog_observation("launch-a", catalog).unwrap();
        session.register_resolver(player_resolver(rva)).unwrap();
        let object_tag = session.register_ephemeral_object(0x7fff_1234).unwrap();
        let mut first = vec![0x55; 128];
        let mut second = vec![0x55; 128];
        write_transform(&mut first, 32, [1, 2, 3], [0.0, 0.0, 0.0, 1.0]);
        let half_sqrt = std::f32::consts::FRAC_1_SQRT_2;
        write_transform(
            &mut second,
            32,
            [1, 2, 3],
            [0.0, 0.0, -half_sqrt, -half_sqrt],
        );
        for (present_seq, qpc, bytes) in [(100, 1_000, &first), (101, 1_001, &second)] {
            session
                .observe_snapshot(SnapshotObservationInput {
                    role: PoseRole::Player,
                    resolver_id: "player-spawn-rtti-candidate-v1",
                    object_tag,
                    present_seq,
                    qpc,
                    scenario: DiscoveryScenario::StationaryHeadSweep,
                    bytes,
                })
                .unwrap();
        }
        let report = session.finish();
        let trajectory = report
            .trajectories
            .iter()
            .find(|trajectory| trajectory.transform_offset == 32)
            .unwrap();
        assert!((trajectory.max_orientation_angle_degrees_from_first - 90.0).abs() < 0.01);
        assert!(trajectory.orientation_changed_from_first);
        assert!(!trajectory.position_changed_from_first);
    }

    #[test]
    fn finish_reports_role_local_missing_and_non_unique_frontiers() {
        let catalog_observation = catalog();
        let rva = catalog_observation.retained_symbols[0].rva.unwrap();
        let mut session =
            Cp2077PoseRuntimeSession::from_catalog_observation("launch-a", catalog_observation)
                .unwrap();
        session.register_resolver(player_resolver(rva)).unwrap();
        let object_tag = session.register_ephemeral_object(0x7fff_1234).unwrap();
        let mut bytes = vec![0x55; 128];
        write_transform(&mut bytes, 32, [1, 2, 3], [0.0, 0.0, 0.0, 1.0]);
        write_transform(&mut bytes, 64, [4, 5, 6], [0.0, 0.0, 0.0, 1.0]);
        session
            .observe_snapshot(SnapshotObservationInput {
                role: PoseRole::Player,
                resolver_id: "player-spawn-rtti-candidate-v1",
                object_tag,
                present_seq: 100,
                qpc: 1_000,
                scenario: DiscoveryScenario::OnFootTranslation,
                bytes: &bytes,
            })
            .unwrap();
        let report = session.finish();
        assert!(report.blockers.iter().any(|blocker| {
            blocker.code == "player_world_transform_candidate_not_unique"
                && blocker.detail.contains("structural WorldTransform offsets")
        }));
        assert!(
            !report
                .blockers
                .iter()
                .any(|blocker| blocker.code == "player_world_transform_candidate_missing")
        );

        let catalog_observation = catalog();
        let rva = catalog_observation.retained_symbols[0].rva.unwrap();
        let mut session =
            Cp2077PoseRuntimeSession::from_catalog_observation("launch-b", catalog_observation)
                .unwrap();
        session.register_resolver(player_resolver(rva)).unwrap();
        let object_tag = session.register_ephemeral_object(0x7fff_5678).unwrap();
        session
            .observe_snapshot(SnapshotObservationInput {
                role: PoseRole::Player,
                resolver_id: "player-spawn-rtti-candidate-v1",
                object_tag,
                present_seq: 100,
                qpc: 1_000,
                scenario: DiscoveryScenario::OnFootTranslation,
                bytes: &[0; 128],
            })
            .unwrap();
        let report = session.finish();
        assert!(report.blockers.iter().any(|blocker| {
            blocker.code == "player_world_transform_candidate_missing"
                && blocker.detail.contains("1 exact-object snapshots")
        }));
    }

    #[test]
    fn render_camera_admission_requires_every_physical_present_input() {
        let mut session =
            Cp2077PoseRuntimeSession::from_catalog_observation("launch-a", catalog()).unwrap();
        let candidate = RenderPresentDiscoveryInput {
            present_seq: 1,
            qpc: 100,
            generation: 1,
            resource_epoch: 3,
            queue_count: 1,
            command_list_count: 1,
            depth_resource_count: 1,
            eligible_pre_clear_snapshot_count: 1,
            new_pre_clear_snapshot_since_previous_present: true,
            capture_safe: true,
            physical_view_matrix_source_observed: true,
            physical_projection_matrix_source_observed: false,
            camera_candidate_admitted: true,
        };
        assert!(
            session
                .observe_render_present_input(candidate.clone())
                .is_err()
        );
        session
            .observe_render_present_input(RenderPresentDiscoveryInput {
                physical_projection_matrix_source_observed: true,
                ..candidate
            })
            .unwrap();
        assert!(
            session
                .observe_render_present_input(RenderPresentDiscoveryInput {
                    present_seq: 1,
                    qpc: 101,
                    physical_projection_matrix_source_observed: true,
                    ..candidate
                })
                .is_err()
        );
    }

    #[test]
    fn system_rtti_matches_require_registered_tags_exact_name_and_size() {
        let mut session =
            Cp2077PoseRuntimeSession::from_catalog_observation("launch-a", catalog()).unwrap();
        let unregistered = RuntimeSystemEnumerationObservation {
            poll_ordinal: 1,
            qpc: 100,
            cgame_engine_resolved: true,
            framework_resolved: true,
            game_instance_resolved: true,
            declared_system_count: 1,
            candidates_truncated: false,
            candidates: vec![RuntimeSystemCandidateObservation {
                system_index: 0,
                object_tag: 99,
                handle_instance_readable: true,
                handle_refcount_readable: true,
                handle_strong_ref_count: 1,
                handle_weak_ref_count: 1,
                object_vtable_readable: true,
                native_type_descriptor_readable: true,
                vtable_module_rva: Some(0x1234),
                native_type_name: Some("cpPlayerSystem".into()),
                native_type_bytes: Some(CP2077_PLAYER_SYSTEM_TYPE_BYTES),
                player_system_type_match: true,
                camera_system_type_match: false,
            }],
            blocker: None,
        };
        assert!(session.observe_system_enumeration(unregistered).is_err());

        let object_tag = session.register_ephemeral_object(0x7fff_1234).unwrap();
        let wrong_size = RuntimeSystemEnumerationObservation {
            poll_ordinal: 1,
            qpc: 100,
            cgame_engine_resolved: true,
            framework_resolved: true,
            game_instance_resolved: true,
            declared_system_count: 1,
            candidates_truncated: false,
            candidates: vec![RuntimeSystemCandidateObservation {
                system_index: 0,
                object_tag,
                handle_instance_readable: true,
                handle_refcount_readable: true,
                handle_strong_ref_count: 1,
                handle_weak_ref_count: 1,
                object_vtable_readable: true,
                native_type_descriptor_readable: true,
                vtable_module_rva: Some(0x1234),
                native_type_name: Some("cpPlayerSystem".into()),
                native_type_bytes: Some(0x60_4f0),
                player_system_type_match: true,
                camera_system_type_match: false,
            }],
            blocker: None,
        };
        assert!(session.observe_system_enumeration(wrong_size).is_err());
    }

    #[test]
    fn reports_never_claim_production_readiness() {
        let report = Cp2077PoseRuntimeSession::from_catalog_observation("launch-a", catalog())
            .unwrap()
            .finish();
        assert!(!report.production_binding_emitted);
        assert!(!report.recording_ready);
        assert!(report.player_abi_audit.is_none());
        assert!(
            report
                .blockers
                .iter()
                .any(|blocker| blocker.code == "player_abi_bootstrap_missing")
        );
        assert!(
            report
                .blockers
                .iter()
                .any(|blocker| blocker.code == "first_party_camera_anchor_missing")
        );
        assert!(
            report
                .blockers
                .iter()
                .any(|blocker| blocker.code == "production_binding_not_authored")
        );
    }

    #[test]
    fn provisional_route_absence_is_cleared_but_unrelated_blockers_remain() {
        let mut session =
            Cp2077PoseRuntimeSession::from_catalog_observation("launch-a", catalog()).unwrap();
        session.record_discovery_blocker(
            "player_world_transform_read_only_chain_not_unique",
            "not initialized",
        );
        session.record_discovery_blocker("resolver_chain_drift", "sticky integrity failure");
        session.clear_provisional_discovery_blocker(
            "player_world_transform_read_only_chain_not_unique",
        );
        let report = session.finish();
        assert!(!report.blockers.iter().any(|blocker| {
            blocker.code == "player_world_transform_read_only_chain_not_unique"
        }));
        assert!(
            report
                .blockers
                .iter()
                .any(|blocker| blocker.code == "resolver_chain_drift")
        );
    }

    #[test]
    fn output_writer_rejects_the_game_tree() {
        let temp =
            std::env::temp_dir().join(format!("dm-cp-runtime-writer-{}", std::process::id()));
        let game_root = temp.join("game");
        let outside = temp.join("evidence");
        std::fs::create_dir_all(&game_root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let report = Cp2077PoseRuntimeSession::from_catalog_observation("launch-a", catalog())
            .unwrap()
            .finish();
        assert!(
            write_report_create_new(&game_root.join("report.json"), &game_root, &report).is_err()
        );
        write_report_create_new(&outside.join("report.json"), &game_root, &report).unwrap();
        let written: Cp2077PoseRuntimeReport =
            serde_json::from_slice(&std::fs::read(outside.join("report.json")).unwrap()).unwrap();
        assert!(!written.recording_ready);
        let _ = std::fs::remove_dir_all(temp);
    }

    #[test]
    #[ignore = "requires an explicitly supplied exact-build first-party address catalog"]
    fn explicit_local_catalog_is_streamed_without_materializing_one_million_rows() {
        let path = std::env::var_os("DM_CP2077_ADDRESS_CATALOG")
            .expect("set DM_CP2077_ADDRESS_CATALOG to the exact local first-party catalog");
        let file = std::fs::File::open(path).unwrap();
        let observed = parse_address_catalog(
            std::io::BufReader::with_capacity(2 * 1024 * 1024, file),
            CP2077_ADDRESS_CATALOG_SHA256,
            Some(&ModuleImageLayout {
                image_size: 0x04ef_c000,
                linker_segment_rvas: [0x1000, 0x02a8_d000, 0x032e_6000],
            }),
        )
        .unwrap();
        assert_eq!(observed.total_address_rows, 987_479);
        assert_eq!(observed.named_address_rows, 84);
        assert_eq!(observed.camera_symbol_count, 0);
        assert_eq!(observed.player_symbol_count, 1);
        assert_eq!(observed.world_transform_symbol_count, 0);
        assert_eq!(observed.game_instance_symbol_count, 0);
        assert_eq!(observed.game_system_symbol_count, 0);
        assert_eq!(observed.retained_symbols.len(), 3);
        assert_eq!(observed.retained_symbols[0].rva, Some(0x00a1_452c));
        assert_eq!(observed.retained_symbols[1].rva, Some(0x0342_ac00));
        assert_eq!(observed.retained_symbols[2].rva, Some(0x002f_1080));
    }
}

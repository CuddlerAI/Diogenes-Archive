//! Exact-build CP2077 2.31 pose ABI evidence and observation adapters.
//!
//! The local first-party linker catalog provides useful entry RVAs, but a symbol name is not an
//! ABI. This module pins the relevant instruction windows and records what the code actually
//! proves. It deliberately installs no inline detour or vtable hook.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;

use depth_lab::camera::{Mat4, MatrixKind, classify};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::cp2077_pose_probe::CP2077_EXE_SHA256;

pub const CP2077_PLAYER_SPAWN_WRAPPER_RVA: u32 = 0x00a1_4514;
pub const CP2077_PLAYER_SPAWN_ENTRY_RVA: u32 = 0x00a1_452c;
pub const CP2077_PLAYER_VCALL_BLOCK_RVA: u32 = 0x00a1_4650;
pub const CP2077_PLAYER_VCALL_SLOT_BYTES: u32 = 0x01f8;
pub const CP2077_PLAYER_VCALL_SLOT_INDEX: u32 = CP2077_PLAYER_VCALL_SLOT_BYTES / 8;
pub const CP2077_RUNTIME_ENTITY_REGISTER_RVA: u32 = 0x008b_57f4;
pub const CP2077_SPAWN_SET_SPAWN_OBJECT_RVA: u32 = 0x0065_eea0;
pub const CP2077_PLAYER_ABI_EVIDENCE_ID: &str = "cp2077-steam-20383525-player-spawn-abi-v1";

pub const PLAYER_WRAPPER_SHA256: &str =
    "sha256:a982b5f659c049d44693252592d82a55bcf09e7e9433b70db89148c750686414";
pub const PLAYER_ENTRY_SHA256: &str =
    "sha256:2dc527cd4219c162bd6815492fa99dbc9ae9459b94f8ea6b4020c777246a709b";
pub const PLAYER_VCALL_BLOCK_SHA256: &str =
    "sha256:1688009f6c467548c3f9b46723074b201a9af7e50e6d5f534912282792a57f66";
pub const RUNTIME_ENTITY_REGISTER_ENTRY_SHA256: &str =
    "sha256:680513dbb7e3eee34937324f0fecd00573241f80ff279d416c9ec09a13e75649";
pub const SPAWN_SET_SPAWN_OBJECT_ENTRY_SHA256: &str =
    "sha256:1df87718224fa6caceeec52a11964b87227b4d5a24d7cf30665765dc977a287e";

const PLAYER_WRAPPER_BYTES: [u8; 24] = [
    0x48, 0x8b, 0x51, 0x18, 0x4c, 0x8d, 0x89, 0xb0, 0x00, 0x00, 0x00, 0x4c, 0x8d, 0x41, 0x20, 0x48,
    0x8b, 0x49, 0x10, 0xe9, 0x00, 0x00, 0x00, 0x00,
];
const PLAYER_ENTRY_BYTES: [u8; 32] = [
    0x48, 0x89, 0x5c, 0x24, 0x18, 0x48, 0x89, 0x74, 0x24, 0x20, 0x55, 0x57, 0x41, 0x54, 0x41, 0x56,
    0x41, 0x57, 0x48, 0x8b, 0xec, 0x48, 0x83, 0xec, 0x50, 0x48, 0x8b, 0xda, 0x48, 0x8b, 0xf9, 0x48,
];
const PLAYER_VCALL_BLOCK_BYTES: [u8; 24] = [
    0x48, 0x8b, 0x07, 0x48, 0x8b, 0xcf, 0x4c, 0x8b, 0x06, 0x41, 0x8a, 0x56, 0x52, 0xff, 0x90, 0xf8,
    0x01, 0x00, 0x00, 0x41, 0x80, 0x7e, 0x50, 0x00,
];
const RUNTIME_ENTITY_REGISTER_ENTRY_BYTES: [u8; 32] = [
    0x48, 0x89, 0x5c, 0x24, 0x10, 0x48, 0x89, 0x6c, 0x24, 0x18, 0x48, 0x89, 0x74, 0x24, 0x20, 0x57,
    0x48, 0x83, 0xec, 0x60, 0x48, 0x8b, 0xf1, 0x48, 0x8b, 0xfa, 0x48, 0x83, 0xc1, 0x48, 0xe8, 0x51,
];
const SPAWN_SET_SPAWN_OBJECT_ENTRY_BYTES: [u8; 32] = [
    0x48, 0x89, 0x5c, 0x24, 0x10, 0x48, 0x89, 0x74, 0x24, 0x18, 0x55, 0x57, 0x41, 0x56, 0x48, 0x8d,
    0x6c, 0x24, 0xf0, 0x48, 0x81, 0xec, 0x10, 0x01, 0x00, 0x00, 0x48, 0x83, 0x79, 0x48, 0x00, 0x49,
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutableWindow<'a> {
    pub rva: u32,
    pub bytes: &'a [u8],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlayerArgumentRegister {
    RcxPlayerSystemThis,
    RdxUnknownSpawnState,
    R8UnknownDescriptor,
    R9UnknownOwnerReference,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WrapperArgumentEvidence {
    pub target_register: PlayerArgumentRegister,
    pub wrapper_object_field_offset: u32,
    pub field_is_address_not_dereference: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlayerObservationRoute {
    PlayerSpawnEntryInline,
    PlayerSystemVirtualEvent,
    RuntimeEntityRegistryRegister,
    SpawnSetSpawnObject,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlayerRouteAssessment {
    pub route: PlayerObservationRoute,
    pub exact_entry_rva: Option<u32>,
    pub argument_shape_observed: bool,
    pub local_player_identity_available: bool,
    pub original_exact_once_adapter_available: bool,
    pub automatic_installation_admitted: bool,
    pub blocker: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cp2077PlayerAbiAudit {
    pub schema: String,
    pub evidence_id: String,
    pub executable_sha256: String,
    pub wrapper_rva: u32,
    pub entry_rva: u32,
    pub vcall_instruction_rva: u32,
    pub vcall_slot_bytes: u32,
    pub vcall_slot_index: u32,
    pub wrapper_arguments: Vec<WrapperArgumentEvidence>,
    pub rdx_plus_0x10_forwarded_to_vcall_r8: bool,
    pub byte_from_r8_plus_0x52_forwarded_to_vcall_dl: bool,
    pub route_assessments: Vec<PlayerRouteAssessment>,
    pub inline_hook_installed: bool,
    pub production_player_binding_emitted: bool,
    pub recording_ready: bool,
}

fn exact_window<'a>(
    windows: &'a [ExecutableWindow<'a>],
    rva: u32,
    expected: &[u8],
) -> Result<(), String> {
    let matches: Vec<_> = windows.iter().filter(|window| window.rva == rva).collect();
    if matches.len() != 1 {
        return Err(format!(
            "exact ABI audit expected one window at RVA {rva:#x}, found {}",
            matches.len()
        ));
    }
    if matches[0].bytes != expected {
        return Err(format!("exact ABI byte drift at RVA {rva:#x}"));
    }
    Ok(())
}

fn direct_image_window(image: &[u8], rva: u32, bytes: usize) -> Result<&[u8], String> {
    let start = rva as usize;
    let end = start
        .checked_add(bytes)
        .ok_or("CP2077 ABI image-window overflow")?;
    image
        .get(start..end)
        .ok_or_else(|| format!("CP2077 ABI image is truncated at RVA {rva:#x}"))
}

/// Audit a loaded PE image, where each RVA is directly indexable from the module base.
pub fn audit_cp2077_player_abi_loaded_image(
    executable_sha256: &str,
    image: &[u8],
) -> Result<Cp2077PlayerAbiAudit, String> {
    let windows = [
        ExecutableWindow {
            rva: CP2077_PLAYER_SPAWN_WRAPPER_RVA,
            bytes: direct_image_window(
                image,
                CP2077_PLAYER_SPAWN_WRAPPER_RVA,
                PLAYER_WRAPPER_BYTES.len(),
            )?,
        },
        ExecutableWindow {
            rva: CP2077_PLAYER_SPAWN_ENTRY_RVA,
            bytes: direct_image_window(
                image,
                CP2077_PLAYER_SPAWN_ENTRY_RVA,
                PLAYER_ENTRY_BYTES.len(),
            )?,
        },
        ExecutableWindow {
            rva: CP2077_PLAYER_VCALL_BLOCK_RVA,
            bytes: direct_image_window(
                image,
                CP2077_PLAYER_VCALL_BLOCK_RVA,
                PLAYER_VCALL_BLOCK_BYTES.len(),
            )?,
        },
        ExecutableWindow {
            rva: CP2077_RUNTIME_ENTITY_REGISTER_RVA,
            bytes: direct_image_window(
                image,
                CP2077_RUNTIME_ENTITY_REGISTER_RVA,
                RUNTIME_ENTITY_REGISTER_ENTRY_BYTES.len(),
            )?,
        },
        ExecutableWindow {
            rva: CP2077_SPAWN_SET_SPAWN_OBJECT_RVA,
            bytes: direct_image_window(
                image,
                CP2077_SPAWN_SET_SPAWN_OBJECT_RVA,
                SPAWN_SET_SPAWN_OBJECT_ENTRY_BYTES.len(),
            )?,
        },
    ];
    audit_cp2077_player_abi(executable_sha256, &windows)
}

fn pe_file_window(file: &[u8], rva: u32, bytes: usize) -> Result<&[u8], String> {
    if file.get(0..2) != Some(b"MZ") {
        return Err("CP2077 ABI file has no DOS signature".into());
    }
    let pe_offset = u32::from_le_bytes(
        file.get(0x3c..0x40)
            .ok_or("CP2077 ABI DOS header is truncated")?
            .try_into()
            .map_err(|_| "CP2077 ABI DOS offset width is invalid")?,
    ) as usize;
    if file.get(pe_offset..pe_offset + 4) != Some(b"PE\0\0") {
        return Err("CP2077 ABI file has no PE signature".into());
    }
    let coff = pe_offset + 4;
    let machine = u16::from_le_bytes(
        file.get(coff..coff + 2)
            .ok_or("CP2077 ABI COFF header is truncated")?
            .try_into()
            .map_err(|_| "CP2077 ABI machine width is invalid")?,
    );
    if machine != 0x8664 {
        return Err(format!("CP2077 ABI expected AMD64 PE, got {machine:#06x}"));
    }
    let section_count = u16::from_le_bytes(
        file.get(coff + 2..coff + 4)
            .ok_or("CP2077 ABI section count is truncated")?
            .try_into()
            .map_err(|_| "CP2077 ABI section count width is invalid")?,
    ) as usize;
    let optional_bytes = u16::from_le_bytes(
        file.get(coff + 16..coff + 18)
            .ok_or("CP2077 ABI optional-header size is truncated")?
            .try_into()
            .map_err(|_| "CP2077 ABI optional-header size width is invalid")?,
    ) as usize;
    if section_count == 0 || section_count > 96 {
        return Err("CP2077 ABI section count is outside the audit ceiling".into());
    }
    let section_table = coff
        .checked_add(20)
        .and_then(|offset| offset.checked_add(optional_bytes))
        .ok_or("CP2077 ABI section-table overflow")?;
    for index in 0..section_count {
        let header = section_table
            .checked_add(index * 40)
            .ok_or("CP2077 ABI section-header overflow")?;
        let read_u32 = |offset: usize| -> Result<u32, String> {
            Ok(u32::from_le_bytes(
                file.get(header + offset..header + offset + 4)
                    .ok_or("CP2077 ABI section header is truncated")?
                    .try_into()
                    .map_err(|_| "CP2077 ABI section field width is invalid")?,
            ))
        };
        let virtual_address = read_u32(12)?;
        let raw_size = read_u32(16)?;
        let raw_offset = read_u32(20)?;
        let Some(section_offset) = rva.checked_sub(virtual_address) else {
            continue;
        };
        let bytes_u32 = u32::try_from(bytes).map_err(|_| "CP2077 ABI window is too wide")?;
        if section_offset
            .checked_add(bytes_u32)
            .is_none_or(|end| end > raw_size)
        {
            continue;
        }
        let start = (raw_offset as usize)
            .checked_add(section_offset as usize)
            .ok_or("CP2077 ABI raw window overflow")?;
        let end = start
            .checked_add(bytes)
            .ok_or("CP2077 ABI raw window overflow")?;
        return file
            .get(start..end)
            .ok_or_else(|| format!("CP2077 ABI file is truncated at RVA {rva:#x}"));
    }
    Err(format!(
        "CP2077 ABI RVA {rva:#x} is absent from raw PE sections"
    ))
}

/// Read-only offline exact-build audit. The returned evidence contains no filesystem path.
pub fn audit_cp2077_player_abi_pe_file(
    executable_path: &Path,
) -> Result<Cp2077PlayerAbiAudit, String> {
    let file = std::fs::read(executable_path)
        .map_err(|error| format!("read CP2077 executable for ABI audit failed: {error}"))?;
    let executable_sha256 = format!("sha256:{:x}", Sha256::digest(&file));
    let windows = [
        ExecutableWindow {
            rva: CP2077_PLAYER_SPAWN_WRAPPER_RVA,
            bytes: pe_file_window(
                &file,
                CP2077_PLAYER_SPAWN_WRAPPER_RVA,
                PLAYER_WRAPPER_BYTES.len(),
            )?,
        },
        ExecutableWindow {
            rva: CP2077_PLAYER_SPAWN_ENTRY_RVA,
            bytes: pe_file_window(
                &file,
                CP2077_PLAYER_SPAWN_ENTRY_RVA,
                PLAYER_ENTRY_BYTES.len(),
            )?,
        },
        ExecutableWindow {
            rva: CP2077_PLAYER_VCALL_BLOCK_RVA,
            bytes: pe_file_window(
                &file,
                CP2077_PLAYER_VCALL_BLOCK_RVA,
                PLAYER_VCALL_BLOCK_BYTES.len(),
            )?,
        },
        ExecutableWindow {
            rva: CP2077_RUNTIME_ENTITY_REGISTER_RVA,
            bytes: pe_file_window(
                &file,
                CP2077_RUNTIME_ENTITY_REGISTER_RVA,
                RUNTIME_ENTITY_REGISTER_ENTRY_BYTES.len(),
            )?,
        },
        ExecutableWindow {
            rva: CP2077_SPAWN_SET_SPAWN_OBJECT_RVA,
            bytes: pe_file_window(
                &file,
                CP2077_SPAWN_SET_SPAWN_OBJECT_RVA,
                SPAWN_SET_SPAWN_OBJECT_ENTRY_BYTES.len(),
            )?,
        },
    ];
    audit_cp2077_player_abi(&executable_sha256, &windows)
}

pub fn audit_cp2077_player_abi(
    executable_sha256: &str,
    windows: &[ExecutableWindow<'_>],
) -> Result<Cp2077PlayerAbiAudit, String> {
    if executable_sha256 != CP2077_EXE_SHA256 {
        return Err(format!(
            "CP2077 Player ABI exact-build mismatch: expected {CP2077_EXE_SHA256}, got {executable_sha256}"
        ));
    }
    exact_window(
        windows,
        CP2077_PLAYER_SPAWN_WRAPPER_RVA,
        &PLAYER_WRAPPER_BYTES,
    )?;
    exact_window(windows, CP2077_PLAYER_SPAWN_ENTRY_RVA, &PLAYER_ENTRY_BYTES)?;
    exact_window(
        windows,
        CP2077_PLAYER_VCALL_BLOCK_RVA,
        &PLAYER_VCALL_BLOCK_BYTES,
    )?;
    exact_window(
        windows,
        CP2077_RUNTIME_ENTITY_REGISTER_RVA,
        &RUNTIME_ENTITY_REGISTER_ENTRY_BYTES,
    )?;
    exact_window(
        windows,
        CP2077_SPAWN_SET_SPAWN_OBJECT_RVA,
        &SPAWN_SET_SPAWN_OBJECT_ENTRY_BYTES,
    )?;

    Ok(Cp2077PlayerAbiAudit {
        schema: "cp2077-player-abi-audit/v1".into(),
        evidence_id: CP2077_PLAYER_ABI_EVIDENCE_ID.into(),
        executable_sha256: executable_sha256.into(),
        wrapper_rva: CP2077_PLAYER_SPAWN_WRAPPER_RVA,
        entry_rva: CP2077_PLAYER_SPAWN_ENTRY_RVA,
        // Entry + 0x131. The vcall setup block starts at entry + 0x124.
        vcall_instruction_rva: 0x00a1_465d,
        vcall_slot_bytes: CP2077_PLAYER_VCALL_SLOT_BYTES,
        vcall_slot_index: CP2077_PLAYER_VCALL_SLOT_INDEX,
        wrapper_arguments: vec![
            WrapperArgumentEvidence {
                target_register: PlayerArgumentRegister::RcxPlayerSystemThis,
                wrapper_object_field_offset: 0x10,
                field_is_address_not_dereference: false,
            },
            WrapperArgumentEvidence {
                target_register: PlayerArgumentRegister::RdxUnknownSpawnState,
                wrapper_object_field_offset: 0x18,
                field_is_address_not_dereference: false,
            },
            WrapperArgumentEvidence {
                target_register: PlayerArgumentRegister::R8UnknownDescriptor,
                wrapper_object_field_offset: 0x20,
                field_is_address_not_dereference: true,
            },
            WrapperArgumentEvidence {
                target_register: PlayerArgumentRegister::R9UnknownOwnerReference,
                wrapper_object_field_offset: 0xb0,
                field_is_address_not_dereference: true,
            },
        ],
        rdx_plus_0x10_forwarded_to_vcall_r8: true,
        byte_from_r8_plus_0x52_forwarded_to_vcall_dl: true,
        route_assessments: vec![
            PlayerRouteAssessment {
                route: PlayerObservationRoute::PlayerSpawnEntryInline,
                exact_entry_rva: Some(CP2077_PLAYER_SPAWN_ENTRY_RVA),
                argument_shape_observed: true,
                local_player_identity_available: true,
                original_exact_once_adapter_available: true,
                automatic_installation_admitted: false,
                blocker: "No audited unwind/trampoline/return contract; a hot function-entry patch is not installed automatically".into(),
            },
            PlayerRouteAssessment {
                route: PlayerObservationRoute::PlayerSystemVirtualEvent,
                exact_entry_rva: None,
                argument_shape_observed: true,
                local_player_identity_available: true,
                original_exact_once_adapter_available: false,
                automatic_installation_admitted: false,
                blocker: "Preferred narrow route, but the exact PlayerSystem instance and vtable owner/slot identity need a runtime audit before a shadow-vtable hook".into(),
            },
            PlayerRouteAssessment {
                route: PlayerObservationRoute::RuntimeEntityRegistryRegister,
                exact_entry_rva: Some(CP2077_RUNTIME_ENTITY_REGISTER_RVA),
                argument_shape_observed: true,
                local_player_identity_available: false,
                original_exact_once_adapter_available: false,
                automatic_installation_admitted: false,
                blocker: "The event exposes a broad entity candidate in RDX; RTTI plus correlation to the first-party player event is still required".into(),
            },
            PlayerRouteAssessment {
                route: PlayerObservationRoute::SpawnSetSpawnObject,
                exact_entry_rva: Some(CP2077_SPAWN_SET_SPAWN_OBJECT_RVA),
                argument_shape_observed: true,
                local_player_identity_available: false,
                original_exact_once_adapter_available: false,
                automatic_installation_admitted: false,
                blocker: "The four-argument spawn route is too broad and does not identify the local player".into(),
            },
        ],
        inline_hook_installed: false,
        production_player_binding_emitted: false,
        recording_ready: false,
    })
}

/// Raw Windows-x64 argument registers at the audited Player spawn entry.
///
/// The three non-`this` pointers intentionally keep non-semantic names. Disassembly establishes
/// how they are used, not their complete C++ types.
#[derive(Clone, Copy, Debug)]
pub struct PlayerSpawnArguments {
    pub player_system_this: *mut core::ffi::c_void,
    pub arg_rdx: *mut core::ffi::c_void,
    pub arg_r8: *mut core::ffi::c_void,
    pub arg_r9: *mut core::ffi::c_void,
}

/// Observer used by a future already-admitted detour/vtable owner. It may copy bounded evidence
/// before the original call and publish it after the original returns.
pub trait PlayerSpawnObserver {
    type Pending;

    fn capture_before_original(
        &mut self,
        arguments: PlayerSpawnArguments,
    ) -> Result<Self::Pending, String>;

    fn publish_after_original(&mut self, pending: Self::Pending) -> Result<(), String>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExactOnceForwardOutcome<R> {
    pub original_return: R,
    pub observation_error: Option<String>,
}

/// Forward through an already-admitted hook owner while preserving the original call exactly
/// once. Observer errors and panics never suppress or repeat the original.
///
/// This function does not install a hook and is generic over the still-unproven native return
/// contract. A future FFI bridge must select the exact return type only after its caller contract
/// is audited.
pub fn forward_player_spawn_exact_once<R, F, O>(
    arguments: PlayerSpawnArguments,
    original: F,
    observer: &mut O,
) -> ExactOnceForwardOutcome<R>
where
    F: FnOnce(PlayerSpawnArguments) -> R,
    O: PlayerSpawnObserver,
{
    let pending = catch_unwind(AssertUnwindSafe(|| {
        observer.capture_before_original(arguments)
    }));
    let original_return = original(arguments);
    let observation_error = match pending {
        Ok(Ok(pending)) => match catch_unwind(AssertUnwindSafe(|| {
            observer.publish_after_original(pending)
        })) {
            Ok(Ok(())) => None,
            Ok(Err(error)) => Some(error),
            Err(_) => Some("Player spawn observer panicked after the original call".into()),
        },
        Ok(Err(error)) => Some(error),
        Err(_) => Some("Player spawn observer panicked before the original call".into()),
    };
    ExactOnceForwardOutcome {
        original_return,
        observation_error,
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenderMatrixObservation {
    /// Opaque process-local resource ordinal, never a GPU virtual address or COM pointer.
    pub source_tag: u64,
    pub root_parameter_index: u32,
    pub buffer_bytes: u32,
    pub matrix_offset: u32,
    pub matrix: Mat4,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectedPresentRenderCameraObservation {
    pub present_seq: u64,
    pub present_qpc: i64,
    pub generation: u64,
    pub command_queue_epoch: u64,
    pub command_list_tag: u64,
    pub selected_depth_resource_tag: u64,
    pub selected_depth_resource_epoch: u64,
    pub pass_ordinal: u32,
    pub draw_ordinal: u32,
    pub copy_before_clear_submitted: bool,
    pub selected_present_identity_passed: bool,
    pub view: RenderMatrixObservation,
    pub projection: RenderMatrixObservation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenderCameraRouteVerdict {
    pub schema: String,
    pub candidate_admitted_for_cross_launch_discovery: bool,
    pub player_pose_available: bool,
    pub production_camera_binding_emitted: bool,
    pub recording_ready: bool,
    pub blockers: Vec<String>,
}

#[must_use]
pub fn validate_selected_present_render_camera(
    observation: &SelectedPresentRenderCameraObservation,
) -> RenderCameraRouteVerdict {
    let mut blockers = Vec::new();
    if observation.present_seq == 0 || observation.present_qpc <= 0 || observation.generation == 0 {
        blockers.push("selected Present identity is invalid".into());
    }
    if observation.command_queue_epoch == 0
        || observation.command_list_tag == 0
        || observation.selected_depth_resource_tag == 0
        || observation.selected_depth_resource_epoch == 0
    {
        blockers.push("D3D12 queue/list/depth resource identity is incomplete".into());
    }
    if !observation.copy_before_clear_submitted || !observation.selected_present_identity_passed {
        blockers
            .push("camera candidate is not joined to an admitted selected-depth Present".into());
    }
    for (role, source) in [
        ("view", &observation.view),
        ("projection", &observation.projection),
    ] {
        if source.source_tag == 0
            || source.buffer_bytes < 64
            || !source.matrix_offset.is_multiple_of(16)
            || source
                .matrix_offset
                .checked_add(64)
                .is_none_or(|end| end > source.buffer_bytes)
            || !source
                .matrix
                .iter()
                .flatten()
                .all(|value| value.is_finite())
        {
            blockers.push(format!("{role} matrix source is structurally invalid"));
        }
    }
    if !matches!(classify(&observation.view.matrix), MatrixKind::View { .. }) {
        blockers.push("view matrix failed rigid-view classification".into());
    }
    if classify(&observation.projection.matrix) != MatrixKind::Projection {
        blockers.push("projection matrix failed perspective classification".into());
    }
    if observation.view.source_tag == observation.projection.source_tag
        && observation.view.matrix_offset == observation.projection.matrix_offset
    {
        blockers.push("view and projection alias one physical matrix".into());
    }
    RenderCameraRouteVerdict {
        schema: "cp2077-selected-present-render-camera-verdict/v1".into(),
        candidate_admitted_for_cross_launch_discovery: blockers.is_empty(),
        // Render state cannot establish game-logic player pose.
        player_pose_available: false,
        // Cross-launch stability, metric calibration and E-geo are separate gates.
        production_camera_binding_emitted: false,
        recording_ready: false,
        blockers,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn windows() -> Vec<ExecutableWindow<'static>> {
        vec![
            ExecutableWindow {
                rva: CP2077_PLAYER_SPAWN_WRAPPER_RVA,
                bytes: &PLAYER_WRAPPER_BYTES,
            },
            ExecutableWindow {
                rva: CP2077_PLAYER_SPAWN_ENTRY_RVA,
                bytes: &PLAYER_ENTRY_BYTES,
            },
            ExecutableWindow {
                rva: CP2077_PLAYER_VCALL_BLOCK_RVA,
                bytes: &PLAYER_VCALL_BLOCK_BYTES,
            },
            ExecutableWindow {
                rva: CP2077_RUNTIME_ENTITY_REGISTER_RVA,
                bytes: &RUNTIME_ENTITY_REGISTER_ENTRY_BYTES,
            },
            ExecutableWindow {
                rva: CP2077_SPAWN_SET_SPAWN_OBJECT_RVA,
                bytes: &SPAWN_SET_SPAWN_OBJECT_ENTRY_BYTES,
            },
        ]
    }

    #[test]
    fn exact_windows_admit_abi_evidence_but_not_hook_installation() {
        let audit = audit_cp2077_player_abi(CP2077_EXE_SHA256, &windows()).unwrap();
        assert_eq!(audit.vcall_instruction_rva, 0x00a1_465d);
        assert_eq!(audit.vcall_slot_index, 63);
        assert!(audit.rdx_plus_0x10_forwarded_to_vcall_r8);
        assert!(audit.byte_from_r8_plus_0x52_forwarded_to_vcall_dl);
        assert!(
            audit
                .route_assessments
                .iter()
                .all(|route| !route.automatic_installation_admitted)
        );
        assert!(!audit.inline_hook_installed);
        assert!(!audit.production_player_binding_emitted);
        assert!(!audit.recording_ready);
    }

    #[test]
    fn published_fingerprints_match_the_audited_windows() {
        let fingerprint = |bytes: &[u8]| format!("sha256:{:x}", Sha256::digest(bytes));
        assert_eq!(fingerprint(&PLAYER_WRAPPER_BYTES), PLAYER_WRAPPER_SHA256);
        assert_eq!(fingerprint(&PLAYER_ENTRY_BYTES), PLAYER_ENTRY_SHA256);
        assert_eq!(
            fingerprint(&PLAYER_VCALL_BLOCK_BYTES),
            PLAYER_VCALL_BLOCK_SHA256
        );
        assert_eq!(
            fingerprint(&RUNTIME_ENTITY_REGISTER_ENTRY_BYTES),
            RUNTIME_ENTITY_REGISTER_ENTRY_SHA256
        );
        assert_eq!(
            fingerprint(&SPAWN_SET_SPAWN_OBJECT_ENTRY_BYTES),
            SPAWN_SET_SPAWN_OBJECT_ENTRY_SHA256
        );
    }

    #[test]
    fn one_byte_drift_or_hash_drift_fails_closed() {
        let mut wrapper = PLAYER_WRAPPER_BYTES;
        wrapper[0] ^= 1;
        let mut windows = windows();
        windows[0].bytes = &wrapper;
        assert!(audit_cp2077_player_abi(CP2077_EXE_SHA256, &windows).is_err());
        assert!(audit_cp2077_player_abi("sha256:wrong", &windows).is_err());
    }

    #[test]
    fn loaded_image_audit_rejects_a_truncated_module() {
        assert!(audit_cp2077_player_abi_loaded_image(CP2077_EXE_SHA256, &[0_u8; 4096]).is_err());
    }

    #[test]
    #[ignore = "requires an explicitly supplied exact-build CP2077 executable"]
    fn explicit_local_pe_file_passes_all_exact_abi_windows() {
        let path = std::env::var_os("DM_CP2077_EXE")
            .expect("set DM_CP2077_EXE to the exact local Cyberpunk2077.exe");
        let audit = audit_cp2077_player_abi_pe_file(Path::new(&path)).unwrap();
        assert_eq!(audit.executable_sha256, CP2077_EXE_SHA256);
        assert_eq!(audit.vcall_instruction_rva, 0x00a1_465d);
        assert!(!audit.inline_hook_installed);
    }

    struct TestObserver {
        events: Vec<&'static str>,
        fail_before: bool,
        panic_before: bool,
        panic_after: bool,
    }

    impl PlayerSpawnObserver for TestObserver {
        type Pending = ();

        fn capture_before_original(
            &mut self,
            _arguments: PlayerSpawnArguments,
        ) -> Result<Self::Pending, String> {
            self.events.push("before");
            assert!(!self.panic_before, "synthetic before panic");
            if self.fail_before {
                return Err("synthetic observation failure".into());
            }
            Ok(())
        }

        fn publish_after_original(&mut self, _pending: Self::Pending) -> Result<(), String> {
            self.events.push("after");
            assert!(!self.panic_after, "synthetic after panic");
            Ok(())
        }
    }

    fn arguments() -> PlayerSpawnArguments {
        PlayerSpawnArguments {
            player_system_this: std::ptr::null_mut(),
            arg_rdx: std::ptr::null_mut(),
            arg_r8: std::ptr::null_mut(),
            arg_r9: std::ptr::null_mut(),
        }
    }

    #[test]
    fn forwarding_orders_observation_around_exactly_one_original_call() {
        let mut observer = TestObserver {
            events: Vec::new(),
            fail_before: false,
            panic_before: false,
            panic_after: false,
        };
        let mut calls = 0;
        let outcome = forward_player_spawn_exact_once(
            arguments(),
            |_| {
                calls += 1;
                17
            },
            &mut observer,
        );
        assert_eq!(calls, 1);
        assert_eq!(outcome.original_return, 17);
        assert_eq!(outcome.observation_error, None);
        assert_eq!(observer.events, ["before", "after"]);
    }

    #[test]
    fn observer_failure_or_panic_never_suppresses_the_original() {
        for (fail_before, panic_before, panic_after) in [
            (true, false, false),
            (false, true, false),
            (false, false, true),
        ] {
            let mut observer = TestObserver {
                events: Vec::new(),
                fail_before,
                panic_before,
                panic_after,
            };
            let mut calls = 0;
            let outcome = forward_player_spawn_exact_once(
                arguments(),
                |_| {
                    calls += 1;
                },
                &mut observer,
            );
            assert_eq!(calls, 1);
            assert!(outcome.observation_error.is_some());
        }
    }

    fn render_observation() -> SelectedPresentRenderCameraObservation {
        SelectedPresentRenderCameraObservation {
            present_seq: 100,
            present_qpc: 1_000,
            generation: 1,
            command_queue_epoch: 2,
            command_list_tag: 3,
            selected_depth_resource_tag: 4,
            selected_depth_resource_epoch: 5,
            pass_ordinal: 1,
            draw_ordinal: 7,
            copy_before_clear_submitted: true,
            selected_present_identity_passed: true,
            view: RenderMatrixObservation {
                source_tag: 10,
                root_parameter_index: 2,
                buffer_bytes: 256,
                matrix_offset: 0,
                matrix: [
                    [1.0, 0.0, 0.0, 0.0],
                    [0.0, 1.0, 0.0, 0.0],
                    [0.0, 0.0, 1.0, 0.0],
                    [1.0, 2.0, 3.0, 1.0],
                ],
            },
            projection: RenderMatrixObservation {
                source_tag: 10,
                root_parameter_index: 2,
                buffer_bytes: 256,
                matrix_offset: 64,
                matrix: [
                    [1.0, 0.0, 0.0, 0.0],
                    [0.0, 1.0, 0.0, 0.0],
                    [0.0, 0.0, 1.0, 1.0],
                    [0.0, 0.0, -0.1, 0.0],
                ],
            },
        }
    }

    #[test]
    fn selected_present_render_route_admits_only_a_camera_candidate() {
        let verdict = validate_selected_present_render_camera(&render_observation());
        assert!(
            verdict.candidate_admitted_for_cross_launch_discovery,
            "{:?}",
            verdict.blockers
        );
        assert!(!verdict.player_pose_available);
        assert!(!verdict.production_camera_binding_emitted);
        assert!(!verdict.recording_ready);
    }

    #[test]
    fn render_route_rejects_depth_identity_loss_and_matrix_alias() {
        let mut observation = render_observation();
        observation.copy_before_clear_submitted = false;
        observation.projection.matrix_offset = 0;
        let verdict = validate_selected_present_render_camera(&observation);
        assert!(!verdict.candidate_admitted_for_cross_launch_discovery);
        assert!(verdict.blockers.len() >= 2);
    }
}

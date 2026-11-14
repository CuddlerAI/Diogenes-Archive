//! Offline, fail-closed reduction of two independent generic-D3D11 discovery runs.
//!
//! The output is deliberately capped at a bridge-only DSV selection attestation. It binds the
//! module-load receipt, probe-authored activation ACK, final complete report, and an operator's
//! external visual review for both runs; it never claims recording, profile, camera binding,
//! publication, or delivery readiness.

use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::{Cursor, Read};
#[cfg(feature = "evidence-authoring")]
use std::io::{Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::dsv_discovery::{
    DSV_DISCOVERY_REPORT_FILE_NAME, DSV_DISCOVERY_REPORT_SCHEMA_V2, DSV_DISCOVERY_REPORT_SCHEMA_V3,
    DsvDiscoveryDescriptor, DsvDiscoveryLimits, DsvDiscoveryReport, MAX_DSV_DISCOVERY_REPORT_BYTES,
};

pub const DSV_SELECTION_ATTESTATION_SCHEMA_V1: &str = "d3d11-dsv-selection-attestation/v1";
pub const DSV_SELECTION_ATTESTATION_FILE_NAME: &str = "d3d11-dsv-selection-attestation.json";
pub const DSV_CANDIDATE_VISUAL_REVIEW_SCHEMA_V1: &str = "d3d11-dsv-candidate-visual-review/v1";
pub const DSV_CANDIDATE_VISUAL_REVIEW_FILE_NAME: &str = "d3d11-dsv-candidate-visual-review.json";
pub const GENERIC_D3D11_INJECTION_RECEIPT_SCHEMA_V1: &str =
    "dm-generic-d3d11-dsv-injection-receipt/v1";
pub const GENERIC_D3D11_INJECTION_RECEIPT_FILE_NAME: &str = "receipt.json";
pub const MAX_DSV_SELECTION_RECEIPT_BYTES: u64 = 64 * 1024;
pub const MAX_DSV_SELECTION_VISUAL_REVIEW_BYTES: u64 = 64 * 1024;
pub const MAX_DSV_SELECTION_VISUAL_EVIDENCE_BYTES: u64 = 32 * 1024 * 1024;
pub const MAX_DSV_SELECTION_BOUND_SNAPSHOT_BYTES: u64 = 4 * 1024 * 1024;
pub const MAX_DSV_SELECTION_ATTESTATION_BYTES: u64 = 256 * 1024;
pub const DSV_DISCOVERY_ACTIVATION_ACK_SCHEMA_V1: &str = "d3d11-dsv-discovery-activation-ack/v1";
pub const DSV_DISCOVERY_ACTIVATION_ACK_FILE_NAME: &str = "d3d11-dsv-discovery-activation-ack.json";
pub const MAX_DSV_DISCOVERY_ACTIVATION_ACK_BYTES: u64 = 16 * 1024;
const MAX_LOCATOR_COMPONENTS: usize = 16;
const MAX_LOCATOR_COMPONENT_BYTES: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DsvSelectionReportInput<'a> {
    pub report_path: &'a Path,
    pub selected_candidate_id: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedDsvSelectionReport {
    pub locator: String,
    pub exact_bytes_sha256: String,
    pub discovery_id: String,
    pub executable_sha256: String,
    pub selected_candidate_id: String,
    pub selected_descriptor: DsvDiscoveryDescriptor,
    pub limits: DsvDiscoveryLimits,
}

/// Report-side proof that exactly two complete v2/v3 observations agree on one globally unique
/// descriptor. This is an intermediate value only; it is not serializable and is not an
/// attestation or readiness claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedDsvSelectionReportPair {
    pub reports: [ValidatedDsvSelectionReport; 2],
    pub selected_descriptor: DsvDiscoveryDescriptor,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DsvDiscoveryActivationAck {
    schema: String,
    status: String,
    discovery_id: String,
    session_id: String,
    process_id: u32,
    process_start_time_filetime: String,
    executable_sha256: String,
    probe_config_sha256: String,
    output_leaf: String,
    limits: DsvDiscoveryLimits,
    runtime: DsvDiscoveryActivationRuntime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DsvDiscoveryActivationRuntime {
    writer_started: bool,
    present_hook_installed: bool,
    config_watcher_started: bool,
    discovery_runtime_enabled: bool,
    discovery_complete: bool,
    recording_ready: bool,
    formal_capture_armed: bool,
    selection_ready: bool,
    binding_ready: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedDsvSelectionActivationAck {
    pub locator: String,
    pub exact_bytes_sha256: String,
    pub discovery_id: String,
    pub executable_sha256: String,
    pub process_id: u32,
    pub process_start_time_filetime: String,
    pub probe_config_sha256: String,
    pub session_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DsvSelectionRunInput<'a> {
    pub run_directory: &'a Path,
    pub selected_candidate_id: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DsvSelectionTargetIdentity {
    pub game_id: String,
    pub steam_app_id: String,
    pub steam_build_id: String,
    pub executable: DsvSelectionExecutableIdentity,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DsvSelectionExecutableIdentity {
    pub basename: String,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ValidatedInjectionReceipt {
    locator: String,
    exact_bytes_sha256: String,
    target: DsvSelectionTargetIdentity,
    process_start_time_utc: String,
    representative_scene: DsvSelectionEvidenceBinding,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ValidatedVisualReview {
    locator: String,
    exact_bytes_sha256: String,
    reviewed_at_utc: String,
    reviewed_by: String,
    visual_evidence: DsvSelectionEvidenceBinding,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DsvSelectionEvidenceBinding {
    pub locator: String,
    pub sha256: String,
    pub length_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DsvSelectionSourceBinding {
    pub locator: String,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DsvSelectionAttestationRun {
    pub discovery_id: String,
    pub selected_candidate_id: String,
    pub report: DsvSelectionSourceBinding,
    pub activation_ack: DsvSelectionSourceBinding,
    pub injection_receipt: DsvSelectionSourceBinding,
    pub visual_review: DsvSelectionSourceBinding,
    pub visual_evidence: DsvSelectionEvidenceBinding,
    pub representative_scene_evidence: DsvSelectionEvidenceBinding,
    pub reviewed_at_utc: String,
    pub reviewed_by: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DsvSelectionAttestation {
    pub schema: String,
    pub status: String,
    pub renderer_backend: String,
    pub capability_ceiling: String,
    pub publication_allowed: bool,
    pub deliverable: bool,
    pub recording_ready: bool,
    pub depth_profile_generated: bool,
    pub camera_binding_generated: bool,
    pub target: DsvSelectionTargetIdentity,
    pub selected_descriptor: DsvDiscoveryDescriptor,
    pub runs: [DsvSelectionAttestationRun; 2],
}

/// A selection attestation whose exact bytes and every transitive source have been replayed.
///
/// This value is intentionally smaller than [`DsvSelectionAttestation`]: downstream consumers
/// receive only the identity needed to bind a later discovery phase, not a parsed self-claim that
/// could accidentally be treated as proof without replaying its sources.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedDsvSelectionAttestation {
    pub locator: String,
    pub exact_bytes_sha256: String,
    pub target: DsvSelectionTargetIdentity,
    pub selected_descriptor: DsvDiscoveryDescriptor,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct GenericD3d11InjectionReceipt {
    schema: String,
    mode: String,
    status: String,
    publication_allowed: bool,
    probe_initialization_confirmed: bool,
    discovery_started: bool,
    recording_ready: bool,
    deliverable_ready: bool,
    profile_selected: bool,
    profile_generated: bool,
    selection_binding_ready: bool,
    binding_generated: bool,
    receipt: InjectionReceiptSelfBinding,
    target: DsvSelectionTargetIdentity,
    run: InjectionReceiptRun,
    artifacts: InjectionReceiptArtifacts,
    injector_outcome: InjectionReceiptOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct InjectionReceiptSelfBinding {
    locator: String,
    created_at_utc: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct InjectionReceiptRun {
    discovery_id: String,
    output_leaf: String,
    requested_output_locator: String,
    process_id: u32,
    process_start_time_utc: String,
    process_start_time_filetime: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct InjectionReceiptArtifacts {
    representative_scene_attestation_snapshot: InjectionReceiptSnapshot,
    arm_runtime_gate_snapshot: InjectionReceiptSnapshot,
    pre_injection_runtime_gate_snapshot: InjectionReceiptSnapshot,
    armed_config_snapshot: InjectionReceiptSnapshot,
    injector: InjectionReceiptHash,
    probe: InjectionReceiptHash,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct InjectionReceiptSnapshot {
    locator: String,
    locator_scope: String,
    sha256: String,
    length_bytes: u64,
    publication_allowed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct InjectionReceiptHash {
    sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct InjectionReceiptOutcome {
    exit_code: i32,
    loaded_probe_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
struct ArmedDiscoveryConfig {
    enabled: bool,
    output_dir: String,
    unbind_from_end: usize,
    max_frames: Option<u64>,
    present_only: bool,
    watermark_only: bool,
    writer_bypass: bool,
    target_depth_hz: Option<f64>,
    lane_fps: Option<u32>,
    camera_probe: bool,
    camera_discovery: Option<serde_json::Value>,
    representative_scene_attestation_sha256: String,
    dsv_discovery: Option<crate::dsv_discovery::DsvDiscoveryConfig>,
    camera_production: Option<serde_json::Value>,
    depth_profile_path: Option<String>,
    depth_profile_sha256: Option<String>,
    session_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RepresentativeSceneSnapshot {
    schema: String,
    attestation_kind: String,
    game_id: String,
    steam_app_id: String,
    steam_build_id: String,
    discovery_id: String,
    output_leaf: String,
    executable: DsvSelectionExecutableIdentity,
    runtime_evidence: RepresentativeSceneRuntimeBinding,
    window: RepresentativeSceneWindow,
    scene_kind: String,
    observed_at_utc: String,
    nonce: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RepresentativeSceneRuntimeBinding {
    sha256: String,
    process_id: u32,
    process_start_time_utc: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RepresentativeSceneWindow {
    class_name: String,
    client_width: u32,
    client_height: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct FrozenRuntimeGateSnapshot {
    schema: String,
    captured_utc: String,
    host: String,
    collector_sha256: String,
    process: FrozenRuntimeProcess,
    module_enumeration: FrozenModuleEnumeration,
    graphics: FrozenGraphicsEvidence,
    anti_cheat: FrozenAntiCheatEvidence,
    status: String,
    blocking: Vec<String>,
    limitations: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct FrozenRuntimeProcess {
    process_id: u32,
    process_name: String,
    executable_path: String,
    executable_sha256: String,
    start_time_utc: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct FrozenModuleEnumeration {
    complete: bool,
    error: Option<String>,
    collector_process_architecture: String,
    target_pe_machine: String,
    count: u64,
    names: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct FrozenGraphicsEvidence {
    expected_api: String,
    hits: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct FrozenAntiCheatEvidence {
    require_none_loaded: bool,
    module_hits: Vec<String>,
    service_enumeration_complete: bool,
    service_enumeration_error: Option<String>,
    service_hits: Vec<FrozenProtectionHit>,
    driver_enumeration_complete: bool,
    driver_enumeration_error: Option<String>,
    driver_hits: Vec<FrozenProtectionHit>,
    running_service_hits: u64,
    running_driver_hits: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct FrozenProtectionHit {
    name: String,
    display_name: String,
    state: String,
    start_mode: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DsvCandidateVisualReview {
    schema: String,
    report: DsvSelectionSourceBinding,
    candidate_id: String,
    descriptor: DsvDiscoveryDescriptor,
    visual_evidence: DsvSelectionEvidenceBinding,
    reviewed_at_utc: String,
    reviewed_by: String,
    confirms_representative_scene_depth: bool,
    publication_allowed: bool,
    recording_ready: bool,
}

pub fn validate_dsv_selection_report_pair(
    evidence_root: &Path,
    reports: [DsvSelectionReportInput<'_>; 2],
) -> Result<ValidatedDsvSelectionReportPair, String> {
    let first = validate_one_report(evidence_root, &reports[0])?;
    let second = validate_one_report(evidence_root, &reports[1])?;
    if first.locator == second.locator {
        return Err("D3D11 DSV selection requires two distinct run report locators".into());
    }
    let first_parent = locator_parent(&first.locator)?;
    let second_parent = locator_parent(&second.locator)?;
    if first_parent == second_parent {
        return Err("D3D11 DSV selection reports must come from distinct run directories".into());
    }
    if first.discovery_id == second.discovery_id {
        return Err("D3D11 DSV selection reports repeat one discovery id".into());
    }
    if first.executable_sha256 != second.executable_sha256 {
        return Err("D3D11 DSV selection reports disagree on executable bytes".into());
    }
    if first.selected_descriptor != second.selected_descriptor {
        return Err("D3D11 DSV selection reports disagree on the full DSV descriptor".into());
    }
    let selected_descriptor = first.selected_descriptor;
    Ok(ValidatedDsvSelectionReportPair {
        reports: [first, second],
        selected_descriptor,
    })
}

pub fn validate_dsv_selection_activation_ack_pair(
    evidence_root: &Path,
    reports: &ValidatedDsvSelectionReportPair,
    activation_ack_paths: [&Path; 2],
) -> Result<[ValidatedDsvSelectionActivationAck; 2], String> {
    let first =
        validate_one_activation_ack(evidence_root, activation_ack_paths[0], &reports.reports[0])?;
    let second =
        validate_one_activation_ack(evidence_root, activation_ack_paths[1], &reports.reports[1])?;
    if first.process_id == second.process_id {
        return Err("D3D11 DSV selection activation ACKs repeat one process id".into());
    }
    if first.process_start_time_filetime == second.process_start_time_filetime {
        return Err("D3D11 DSV selection activation ACKs repeat one process start time".into());
    }
    Ok([first, second])
}

fn validate_one_activation_ack(
    evidence_root: &Path,
    path: &Path,
    report: &ValidatedDsvSelectionReport,
) -> Result<ValidatedDsvSelectionActivationAck, String> {
    let (bytes, locator) = read_bounded_regular_file(
        evidence_root,
        path,
        MAX_DSV_DISCOVERY_ACTIVATION_ACK_BYTES,
        "D3D11 DSV discovery activation ACK",
    )?;
    let components = locator.split('/').collect::<Vec<_>>();
    if components.len() != 2 || components[1] != DSV_DISCOVERY_ACTIVATION_ACK_FILE_NAME {
        return Err(
            "D3D11 DSV activation ACK must use the fixed filename in a direct-child run directory"
                .into(),
        );
    }
    if locator_parent(&locator)? != locator_parent(&report.locator)? {
        return Err("D3D11 DSV activation ACK is not in its report run directory".into());
    }
    let ack: DsvDiscoveryActivationAck = serde_json::from_slice(&bytes)
        .map_err(|error| format!("cannot parse D3D11 DSV activation ACK: {error}"))?;
    let start_filetime = ack
        .process_start_time_filetime
        .parse::<u64>()
        .map_err(|_| "D3D11 DSV activation ACK process start FILETIME is not decimal")?;
    if ack.schema != DSV_DISCOVERY_ACTIVATION_ACK_SCHEMA_V1
        || ack.status != "probe_discovery_runtime_activated"
        || ack.discovery_id != report.discovery_id
        || ack.session_id != format!("dsv-discovery-{}", ack.discovery_id)
        || ack.process_id == 0
        || start_filetime == 0
        || ack.process_start_time_filetime != start_filetime.to_string()
        || ack.executable_sha256 != report.executable_sha256
        || !crate::camera_binding::is_sha256_digest(&ack.probe_config_sha256)
        || ack.output_leaf != locator_parent(&report.locator)?
        || ack.limits != report.limits
    {
        return Err("D3D11 DSV activation ACK identity does not match its final report".into());
    }
    if !ack.runtime.writer_started
        || !ack.runtime.present_hook_installed
        || !ack.runtime.config_watcher_started
        || !ack.runtime.discovery_runtime_enabled
        || ack.runtime.discovery_complete
        || ack.runtime.recording_ready
        || ack.runtime.formal_capture_armed
        || ack.runtime.selection_ready
        || ack.runtime.binding_ready
    {
        return Err("D3D11 DSV activation ACK runtime ceiling is invalid".into());
    }
    Ok(ValidatedDsvSelectionActivationAck {
        locator,
        exact_bytes_sha256: format!("sha256:{:x}", Sha256::digest(&bytes)),
        discovery_id: ack.discovery_id,
        executable_sha256: ack.executable_sha256,
        process_id: ack.process_id,
        process_start_time_filetime: ack.process_start_time_filetime,
        probe_config_sha256: ack.probe_config_sha256,
        session_id: ack.session_id,
    })
}

fn validate_one_report(
    evidence_root: &Path,
    input: &DsvSelectionReportInput<'_>,
) -> Result<ValidatedDsvSelectionReport, String> {
    let (bytes, locator) = read_bounded_regular_file(
        evidence_root,
        input.report_path,
        MAX_DSV_DISCOVERY_REPORT_BYTES,
        "D3D11 DSV discovery report",
    )?;
    let components = locator.split('/').collect::<Vec<_>>();
    if components.len() != 2 || components[1] != DSV_DISCOVERY_REPORT_FILE_NAME {
        return Err(
            "D3D11 DSV selection report must use the fixed filename in a direct-child run directory"
                .into(),
        );
    }
    let report: DsvDiscoveryReport = serde_json::from_slice(&bytes)
        .map_err(|error| format!("cannot parse D3D11 DSV selection report: {error}"))?;
    report.validate()?;
    if !matches!(
        report.schema.as_str(),
        DSV_DISCOVERY_REPORT_SCHEMA_V2 | DSV_DISCOVERY_REPORT_SCHEMA_V3
    ) || !report.complete
        || !report.blockers.is_empty()
    {
        return Err(
            "D3D11 DSV selection requires a complete, blocker-free v2/v3 discovery report".into(),
        );
    }
    let selected_descriptor = unique_selected_descriptor(&report, input.selected_candidate_id)?;
    Ok(ValidatedDsvSelectionReport {
        locator,
        exact_bytes_sha256: format!("sha256:{:x}", Sha256::digest(&bytes)),
        discovery_id: report.discovery_id,
        executable_sha256: report.executable_sha256,
        selected_candidate_id: input.selected_candidate_id.into(),
        selected_descriptor,
        limits: report.limits,
    })
}

fn unique_selected_descriptor(
    report: &DsvDiscoveryReport,
    selected_candidate_id: &str,
) -> Result<DsvDiscoveryDescriptor, String> {
    let mut selected = None;
    for candidate in report
        .frames
        .iter()
        .flat_map(|frame| frame.candidates.iter())
        .filter(|candidate| candidate.candidate_id == selected_candidate_id)
    {
        if selected
            .replace(candidate.descriptor)
            .is_some_and(|previous| previous != candidate.descriptor)
        {
            return Err("selected D3D11 DSV candidate descriptor drifted".into());
        }
    }
    let selected = selected.ok_or("selected D3D11 DSV candidate is absent from the report")?;
    let owners = report
        .frames
        .iter()
        .flat_map(|frame| frame.candidates.iter())
        .filter(|candidate| candidate.descriptor == selected)
        .map(|candidate| candidate.candidate_id.as_str())
        .collect::<BTreeSet<_>>();
    if owners.len() != 1 || !owners.contains(selected_candidate_id) {
        return Err(
            "selected D3D11 DSV descriptor is not globally unique within its report".into(),
        );
    }
    Ok(selected)
}

/// Replays an existing bridge-only selection attestation from its exact bytes and all transitive
/// evidence sources.
///
/// The attestation is used only to locate its two run directories and name the two candidate IDs.
/// Reports, activation ACKs, receipt bundles (including scene/runtime/config snapshots), visual
/// reviews, and PNG bytes are then revalidated by the same reduction kernel used by the authoring
/// path. The rebuilt canonical value must equal every field in the supplied attestation.
pub fn validate_dsv_selection_attestation_with_sources(
    evidence_root: &Path,
    attestation_path: &Path,
    expected_attestation_sha256: &str,
) -> Result<ValidatedDsvSelectionAttestation, String> {
    if !crate::camera_binding::is_sha256_digest(expected_attestation_sha256) {
        return Err("D3D11 DSV selection attestation expected hash is not canonical".into());
    }
    let (bytes, locator) = read_bounded_regular_file(
        evidence_root,
        attestation_path,
        MAX_DSV_SELECTION_ATTESTATION_BYTES,
        "D3D11 DSV selection attestation",
    )?;
    if locator.contains('/') || !locator.ends_with(".json") {
        return Err(
            "D3D11 DSV selection attestation must be a JSON direct child of evidence_root".into(),
        );
    }
    let exact_bytes_sha256 = format!("sha256:{:x}", Sha256::digest(&bytes));
    if exact_bytes_sha256 != expected_attestation_sha256 {
        return Err("D3D11 DSV selection attestation exact-byte hash mismatch".into());
    }
    let attestation: DsvSelectionAttestation = serde_json::from_slice(&bytes)
        .map_err(|error| format!("cannot parse D3D11 DSV selection attestation: {error}"))?;
    attestation.validate()?;

    let run_leaves = [
        locator_parent(&attestation.runs[0].report.locator)?.to_owned(),
        locator_parent(&attestation.runs[1].report.locator)?.to_owned(),
    ];
    let run_directories = [
        evidence_root.join(&run_leaves[0]),
        evidence_root.join(&run_leaves[1]),
    ];
    let rebuilt = build_dsv_selection_attestation(
        evidence_root,
        [
            DsvSelectionRunInput {
                run_directory: &run_directories[0],
                selected_candidate_id: &attestation.runs[0].selected_candidate_id,
            },
            DsvSelectionRunInput {
                run_directory: &run_directories[1],
                selected_candidate_id: &attestation.runs[1].selected_candidate_id,
            },
        ],
    )?;
    if rebuilt != attestation {
        return Err(
            "D3D11 DSV selection attestation does not equal its fully replayed sources".into(),
        );
    }

    // Detect replacement of the attestation path while the slower transitive replay ran. Source
    // identities remain protected by the hashes in the rebuilt value and are replayed on every
    // consumer invocation.
    let (sealed_bytes, sealed_locator) = read_bounded_regular_file(
        evidence_root,
        attestation_path,
        MAX_DSV_SELECTION_ATTESTATION_BYTES,
        "D3D11 DSV selection attestation",
    )?;
    if sealed_locator != locator || sealed_bytes != bytes {
        return Err("D3D11 DSV selection attestation changed during source replay".into());
    }

    Ok(ValidatedDsvSelectionAttestation {
        locator,
        exact_bytes_sha256,
        target: rebuilt.target,
        selected_descriptor: rebuilt.selected_descriptor,
    })
}

#[cfg(feature = "evidence-authoring")]
pub fn materialize_dsv_selection_attestation(
    evidence_root: &Path,
    runs: [DsvSelectionRunInput<'_>; 2],
    output_path: &Path,
) -> Result<PathBuf, String> {
    let attestation = build_dsv_selection_attestation(evidence_root, runs)?;
    write_attestation_create_new(evidence_root, output_path, &attestation)
}

fn build_dsv_selection_attestation(
    evidence_root: &Path,
    runs: [DsvSelectionRunInput<'_>; 2],
) -> Result<DsvSelectionAttestation, String> {
    let run_directories = [
        validate_run_directory(evidence_root, runs[0].run_directory)?,
        validate_run_directory(evidence_root, runs[1].run_directory)?,
    ];
    if run_directories[0].1 == run_directories[1].1 {
        return Err("D3D11 DSV selection requires two distinct run directories".into());
    }
    let report_paths = [
        run_directories[0].0.join(DSV_DISCOVERY_REPORT_FILE_NAME),
        run_directories[1].0.join(DSV_DISCOVERY_REPORT_FILE_NAME),
    ];
    let reports = validate_dsv_selection_report_pair(
        evidence_root,
        [
            DsvSelectionReportInput {
                report_path: &report_paths[0],
                selected_candidate_id: runs[0].selected_candidate_id,
            },
            DsvSelectionReportInput {
                report_path: &report_paths[1],
                selected_candidate_id: runs[1].selected_candidate_id,
            },
        ],
    )?;
    let activation_paths = [
        run_directories[0]
            .0
            .join(DSV_DISCOVERY_ACTIVATION_ACK_FILE_NAME),
        run_directories[1]
            .0
            .join(DSV_DISCOVERY_ACTIVATION_ACK_FILE_NAME),
    ];
    let activations = validate_dsv_selection_activation_ack_pair(
        evidence_root,
        &reports,
        [&activation_paths[0], &activation_paths[1]],
    )?;
    let receipt_paths = [
        evidence_root
            .join(format!("{}-injection-receipt", run_directories[0].1))
            .join(GENERIC_D3D11_INJECTION_RECEIPT_FILE_NAME),
        evidence_root
            .join(format!("{}-injection-receipt", run_directories[1].1))
            .join(GENERIC_D3D11_INJECTION_RECEIPT_FILE_NAME),
    ];
    let receipts = [
        validate_one_injection_receipt(
            evidence_root,
            &receipt_paths[0],
            &run_directories[0].1,
            &reports.reports[0],
            &activations[0],
        )?,
        validate_one_injection_receipt(
            evidence_root,
            &receipt_paths[1],
            &run_directories[1].1,
            &reports.reports[1],
            &activations[1],
        )?,
    ];
    if receipts[0].locator == receipts[1].locator {
        return Err("D3D11 DSV selection repeats one injection receipt bundle".into());
    }
    if receipts[0].target != receipts[1].target {
        return Err(
            "D3D11 DSV selection receipts disagree on game/build/executable identity".into(),
        );
    }
    if receipts[0].process_start_time_utc == receipts[1].process_start_time_utc {
        return Err("D3D11 DSV selection receipts repeat one UTC process start time".into());
    }
    let review_paths = [
        run_directories[0]
            .0
            .join(DSV_CANDIDATE_VISUAL_REVIEW_FILE_NAME),
        run_directories[1]
            .0
            .join(DSV_CANDIDATE_VISUAL_REVIEW_FILE_NAME),
    ];
    let reviews = [
        validate_one_visual_review(
            evidence_root,
            &review_paths[0],
            &run_directories[0].1,
            &reports.reports[0],
        )?,
        validate_one_visual_review(
            evidence_root,
            &review_paths[1],
            &run_directories[1].1,
            &reports.reports[1],
        )?,
    ];
    for index in 0..2 {
        if parse_utc_timestamp(&reviews[index].reviewed_at_utc)?
            < parse_utc_timestamp(&receipts[index].process_start_time_utc)?
        {
            return Err("D3D11 DSV visual review predates its discovery process".into());
        }
    }
    let attestation = DsvSelectionAttestation {
        schema: DSV_SELECTION_ATTESTATION_SCHEMA_V1.into(),
        status: "bridge_ready_only".into(),
        renderer_backend: "d3d11".into(),
        capability_ceiling: "dsv_selection_bridge_only".into(),
        publication_allowed: false,
        deliverable: false,
        recording_ready: false,
        depth_profile_generated: false,
        camera_binding_generated: false,
        target: receipts[0].target.clone(),
        selected_descriptor: reports.selected_descriptor,
        runs: std::array::from_fn(|index| DsvSelectionAttestationRun {
            discovery_id: reports.reports[index].discovery_id.clone(),
            selected_candidate_id: reports.reports[index].selected_candidate_id.clone(),
            report: source_binding(
                &reports.reports[index].locator,
                &reports.reports[index].exact_bytes_sha256,
            ),
            activation_ack: source_binding(
                &activations[index].locator,
                &activations[index].exact_bytes_sha256,
            ),
            injection_receipt: source_binding(
                &receipts[index].locator,
                &receipts[index].exact_bytes_sha256,
            ),
            visual_review: source_binding(
                &reviews[index].locator,
                &reviews[index].exact_bytes_sha256,
            ),
            visual_evidence: reviews[index].visual_evidence.clone(),
            representative_scene_evidence: receipts[index].representative_scene.clone(),
            reviewed_at_utc: reviews[index].reviewed_at_utc.clone(),
            reviewed_by: reviews[index].reviewed_by.clone(),
        }),
    };
    attestation.validate()?;
    Ok(attestation)
}

fn source_binding(locator: &str, sha256: &str) -> DsvSelectionSourceBinding {
    DsvSelectionSourceBinding {
        locator: locator.into(),
        sha256: sha256.into(),
    }
}

fn validate_run_directory(
    evidence_root: &Path,
    run_directory: &Path,
) -> Result<(PathBuf, String), String> {
    if !evidence_root.is_absolute() || !run_directory.is_absolute() {
        return Err("D3D11 DSV selection run paths must be absolute".into());
    }
    reject_reparse_chain(evidence_root, "selection evidence root")?;
    reject_reparse_chain(run_directory, "selection run directory")?;
    let root = evidence_root
        .canonicalize()
        .map_err(|error| format!("cannot resolve DSV selection evidence root: {error}"))?;
    let run = run_directory
        .canonicalize()
        .map_err(|error| format!("cannot resolve DSV selection run directory: {error}"))?;
    if !run.is_dir() || run.parent() != Some(root.as_path()) {
        return Err("D3D11 DSV selection run must be a direct child of evidence_root".into());
    }
    let leaf = run
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("D3D11 DSV selection run leaf is not UTF-8")?;
    validate_safe_component(leaf, "D3D11 DSV selection run leaf")?;
    let leaf = leaf.to_owned();
    Ok((run, leaf))
}

fn validate_one_injection_receipt(
    evidence_root: &Path,
    receipt_path: &Path,
    run_leaf: &str,
    report: &ValidatedDsvSelectionReport,
    activation: &ValidatedDsvSelectionActivationAck,
) -> Result<ValidatedInjectionReceipt, String> {
    let bundle_directory = receipt_path
        .parent()
        .ok_or("generic D3D11 injection receipt has no bundle directory")?;
    validate_receipt_bundle_shape(bundle_directory)?;
    let (bytes, locator) = read_bounded_regular_file(
        evidence_root,
        receipt_path,
        MAX_DSV_SELECTION_RECEIPT_BYTES,
        "generic D3D11 injection receipt",
    )?;
    let expected_bundle_leaf = format!("{run_leaf}-injection-receipt");
    let expected_locator =
        format!("{expected_bundle_leaf}/{GENERIC_D3D11_INJECTION_RECEIPT_FILE_NAME}");
    if locator != expected_locator {
        return Err("generic D3D11 injection receipt bundle locator is invalid".into());
    }
    let receipt: GenericD3d11InjectionReceipt = serde_json::from_slice(&bytes)
        .map_err(|error| format!("cannot parse generic D3D11 injection receipt: {error}"))?;
    if receipt.schema != GENERIC_D3D11_INJECTION_RECEIPT_SCHEMA_V1
        || receipt.mode != "discovery_only"
        || receipt.status != "guarded_module_load_succeeded"
        || receipt.publication_allowed
        || receipt.probe_initialization_confirmed
        || receipt.discovery_started
        || receipt.recording_ready
        || receipt.deliverable_ready
        || receipt.profile_selected
        || receipt.profile_generated
        || receipt.selection_binding_ready
        || receipt.binding_generated
    {
        return Err("generic D3D11 injection receipt weakened its module-load-only ceiling".into());
    }
    if receipt.receipt.locator != locator {
        return Err("generic D3D11 injection receipt self locator is inconsistent".into());
    }
    validate_utc_timestamp(
        &receipt.receipt.created_at_utc,
        "receipt creation timestamp",
    )?;
    validate_target_identity(&receipt.target)?;
    validate_utc_timestamp(
        &receipt.run.process_start_time_utc,
        "process start timestamp",
    )?;
    if receipt.run.discovery_id != report.discovery_id
        || receipt.run.discovery_id != activation.discovery_id
        || receipt.run.output_leaf != run_leaf
        || receipt.run.requested_output_locator != run_leaf
        || receipt.run.process_id != activation.process_id
        || receipt.run.process_start_time_filetime != activation.process_start_time_filetime
        || receipt.target.executable.sha256 != report.executable_sha256
        || receipt.target.executable.sha256 != activation.executable_sha256
    {
        return Err("generic D3D11 injection receipt identity does not match ACK/report".into());
    }
    let receipt_created = parse_utc_timestamp(&receipt.receipt.created_at_utc)?;
    let process_started = parse_utc_timestamp(&receipt.run.process_start_time_utc)?;
    if receipt_created < process_started {
        return Err("generic D3D11 injection receipt predates its process".into());
    }
    if utc_to_filetime(process_started)? != receipt.run.process_start_time_filetime {
        return Err("generic D3D11 injection receipt process UTC/FILETIME identity drifted".into());
    }
    if !crate::camera_binding::is_sha256_digest(&receipt.artifacts.injector.sha256)
        || !crate::camera_binding::is_sha256_digest(&receipt.artifacts.probe.sha256)
        || receipt.injector_outcome.exit_code != 0
        || receipt.injector_outcome.loaded_probe_sha256 != receipt.artifacts.probe.sha256
    {
        return Err("generic D3D11 injector/probe outcome identity is invalid".into());
    }
    let (representative_scene, representative_scene_bytes) = validate_receipt_snapshot(
        evidence_root,
        bundle_directory,
        &receipt.artifacts.representative_scene_attestation_snapshot,
        "representative-scene-attestation.json",
    )?;
    let (arm_runtime, arm_runtime_bytes) = validate_receipt_snapshot(
        evidence_root,
        bundle_directory,
        &receipt.artifacts.arm_runtime_gate_snapshot,
        "arm-runtime-gate.json",
    )?;
    let (_, fresh_runtime_bytes) = validate_receipt_snapshot(
        evidence_root,
        bundle_directory,
        &receipt.artifacts.pre_injection_runtime_gate_snapshot,
        "pre-injection-runtime-gate.json",
    )?;
    let (armed_config_binding, armed_config_bytes) = validate_receipt_snapshot(
        evidence_root,
        bundle_directory,
        &receipt.artifacts.armed_config_snapshot,
        "armed-config.json",
    )?;
    if armed_config_binding.sha256 != activation.probe_config_sha256 {
        return Err("armed config snapshot hash disagrees with the activation ACK".into());
    }
    validate_armed_discovery_config(
        evidence_root,
        run_leaf,
        &armed_config_bytes,
        report,
        activation,
        &representative_scene.sha256,
    )?;
    let arm_captured = validate_frozen_runtime_gate(
        &arm_runtime_bytes,
        &receipt.target,
        receipt.run.process_id,
        &receipt.run.process_start_time_utc,
        "arm runtime gate snapshot",
    )?;
    let fresh_captured = validate_frozen_runtime_gate(
        &fresh_runtime_bytes,
        &receipt.target,
        receipt.run.process_id,
        &receipt.run.process_start_time_utc,
        "pre-injection runtime gate snapshot",
    )?;
    if fresh_captured < arm_captured || receipt_created < fresh_captured {
        return Err("generic D3D11 runtime snapshot chronology is invalid".into());
    }
    validate_representative_scene_snapshot(
        &representative_scene_bytes,
        &receipt.target,
        &receipt.run,
        &arm_runtime.sha256,
        arm_captured,
    )?;
    validate_receipt_bundle_shape(bundle_directory)?;
    Ok(ValidatedInjectionReceipt {
        locator,
        exact_bytes_sha256: format!("sha256:{:x}", Sha256::digest(&bytes)),
        target: receipt.target,
        process_start_time_utc: receipt.run.process_start_time_utc,
        representative_scene,
    })
}

fn validate_receipt_snapshot(
    evidence_root: &Path,
    bundle_directory: &Path,
    snapshot: &InjectionReceiptSnapshot,
    expected_leaf: &str,
) -> Result<(DsvSelectionEvidenceBinding, Vec<u8>), String> {
    if snapshot.locator != expected_leaf
        || snapshot.locator_scope != "receipt_bundle"
        || snapshot.publication_allowed
        || snapshot.length_bytes == 0
        || snapshot.length_bytes > MAX_DSV_SELECTION_BOUND_SNAPSHOT_BYTES
        || !crate::camera_binding::is_sha256_digest(&snapshot.sha256)
    {
        return Err("generic D3D11 receipt snapshot metadata is invalid".into());
    }
    validate_safe_component(&snapshot.locator, "receipt snapshot locator")?;
    let path = bundle_directory.join(&snapshot.locator);
    let (bytes, root_locator) = read_bounded_regular_file(
        evidence_root,
        &path,
        MAX_DSV_SELECTION_BOUND_SNAPSHOT_BYTES,
        "generic D3D11 receipt snapshot",
    )?;
    let actual_hash = format!("sha256:{:x}", Sha256::digest(&bytes));
    if bytes.len() as u64 != snapshot.length_bytes || actual_hash != snapshot.sha256 {
        return Err("generic D3D11 receipt snapshot length/hash mismatch".into());
    }
    serde_json::from_slice::<serde_json::Value>(&bytes)
        .map_err(|error| format!("generic D3D11 receipt snapshot is not strict JSON: {error}"))?;
    Ok((
        DsvSelectionEvidenceBinding {
            locator: root_locator,
            sha256: actual_hash,
            length_bytes: bytes.len() as u64,
        },
        bytes,
    ))
}

fn validate_receipt_bundle_shape(bundle_directory: &Path) -> Result<(), String> {
    const EXPECTED: [&str; 5] = [
        GENERIC_D3D11_INJECTION_RECEIPT_FILE_NAME,
        "representative-scene-attestation.json",
        "arm-runtime-gate.json",
        "pre-injection-runtime-gate.json",
        "armed-config.json",
    ];
    reject_reparse_chain(bundle_directory, "generic D3D11 injection receipt bundle")?;
    let mut leaves = BTreeSet::new();
    for (index, entry) in fs::read_dir(bundle_directory)
        .map_err(|error| format!("cannot enumerate generic D3D11 receipt bundle: {error}"))?
        .enumerate()
    {
        if index >= EXPECTED.len() {
            return Err("generic D3D11 injection receipt bundle has unexpected entries".into());
        }
        let entry = entry.map_err(|error| {
            format!("cannot inspect generic D3D11 receipt bundle entry: {error}")
        })?;
        let leaf = entry
            .file_name()
            .into_string()
            .map_err(|_| "generic D3D11 receipt bundle entry is not UTF-8")?;
        let metadata = fs::symlink_metadata(entry.path()).map_err(|error| {
            format!("cannot inspect generic D3D11 receipt bundle entry: {error}")
        })?;
        if is_reparse_point(&metadata)
            || !metadata.is_file()
            || !EXPECTED.contains(&leaf.as_str())
            || !leaves.insert(leaf)
        {
            return Err("generic D3D11 injection receipt bundle shape is invalid".into());
        }
    }
    if leaves.len() != EXPECTED.len() {
        return Err("generic D3D11 injection receipt bundle is incomplete".into());
    }
    Ok(())
}

fn validate_armed_discovery_config(
    evidence_root: &Path,
    run_leaf: &str,
    bytes: &[u8],
    report: &ValidatedDsvSelectionReport,
    activation: &ValidatedDsvSelectionActivationAck,
    representative_scene_sha256: &str,
) -> Result<(), String> {
    let config: ArmedDiscoveryConfig = serde_json::from_slice(bytes)
        .map_err(|error| format!("cannot parse armed D3D11 discovery config: {error}"))?;
    let discovery = config
        .dsv_discovery
        .as_ref()
        .ok_or("armed config has no D3D11 discovery config")?;
    discovery.validate()?;
    if !config.enabled
        || config.camera_probe
        || config.camera_discovery.is_some()
        || config.camera_production.is_some()
        || config.depth_profile_path.is_some()
        || config.depth_profile_sha256.is_some()
        || config.max_frames.is_some()
        || config.present_only
        || config.watermark_only
        || config.writer_bypass
        || config.target_depth_hz.is_some()
        || config.lane_fps.is_some()
        || config.session_id != activation.session_id
        || config.representative_scene_attestation_sha256 != representative_scene_sha256
        || discovery.discovery_id != report.discovery_id
        || discovery.executable_sha256 != report.executable_sha256
        || discovery.limits != report.limits
    {
        return Err("armed D3D11 discovery config does not match receipt/ACK/report".into());
    }
    let _ = config.unbind_from_end;
    let root = evidence_root
        .canonicalize()
        .map_err(|error| format!("cannot resolve config evidence root: {error}"))?;
    let configured_root = Path::new(&discovery.evidence_root)
        .canonicalize()
        .map_err(|error| format!("cannot resolve armed config evidence root: {error}"))?;
    let configured_output = Path::new(&config.output_dir)
        .canonicalize()
        .map_err(|error| format!("cannot resolve armed config output: {error}"))?;
    reject_reparse_chain(
        Path::new(&discovery.evidence_root),
        "armed config evidence root",
    )?;
    reject_reparse_chain(Path::new(&config.output_dir), "armed config output")?;
    if configured_root != root || configured_output != root.join(run_leaf) {
        return Err("armed D3D11 discovery config escaped its evidence/run identity".into());
    }
    Ok(())
}

fn validate_frozen_runtime_gate(
    bytes: &[u8],
    target: &DsvSelectionTargetIdentity,
    process_id: u32,
    process_start_time_utc: &str,
    label: &str,
) -> Result<time::OffsetDateTime, String> {
    const MAX_RUNTIME_NAMES: usize = 4_096;
    let gate: FrozenRuntimeGateSnapshot =
        serde_json::from_slice(bytes).map_err(|error| format!("cannot parse {label}: {error}"))?;
    let captured = parse_utc_timestamp(&gate.captured_utc)?;
    let process_started = parse_utc_timestamp(&gate.process.start_time_utc)?;
    let executable_basename = Path::new(&gate.process.executable_path)
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| format!("{label} executable path has no UTF-8 basename"))?;
    if gate.schema != "game-runtime-gate-evidence/v1"
        || gate.status != "pass"
        || !gate.blocking.is_empty()
        || gate.host.trim().is_empty()
        || gate.host.len() > 256
        || !crate::camera_binding::is_sha256_digest(&gate.collector_sha256)
        || gate.process.process_id != process_id
        || gate.process.process_name.trim().is_empty()
        || gate.process.process_name.len() > 260
        || !Path::new(&gate.process.executable_path).is_absolute()
        || gate.process.executable_path.len() > 32_768
        || executable_basename != target.executable.basename
        || gate.process.executable_sha256 != target.executable.sha256
        || gate.process.start_time_utc != process_start_time_utc
        || captured < process_started
        || !gate.module_enumeration.complete
        || gate.module_enumeration.error.is_some()
        || gate.module_enumeration.count != gate.module_enumeration.names.len() as u64
        || gate.module_enumeration.names.len() > MAX_RUNTIME_NAMES
        || gate
            .module_enumeration
            .collector_process_architecture
            .trim()
            .is_empty()
        || gate.module_enumeration.target_pe_machine.trim().is_empty()
        || gate.graphics.expected_api != "d3d11"
        || !gate.graphics.hits.iter().any(|name| name == "d3d11.dll")
        || gate.graphics.hits.len() > MAX_RUNTIME_NAMES
        || !gate.anti_cheat.require_none_loaded
        || !gate.anti_cheat.module_hits.is_empty()
        || !gate.anti_cheat.service_enumeration_complete
        || gate.anti_cheat.service_enumeration_error.is_some()
        || !gate.anti_cheat.driver_enumeration_complete
        || gate.anti_cheat.driver_enumeration_error.is_some()
        || gate.anti_cheat.running_service_hits != 0
        || gate.anti_cheat.running_driver_hits != 0
        || gate.anti_cheat.service_hits.len() > MAX_RUNTIME_NAMES
        || gate.anti_cheat.driver_hits.len() > MAX_RUNTIME_NAMES
        || gate.limitations.len() > MAX_RUNTIME_NAMES
    {
        return Err(format!(
            "{label} is not a complete passing bound runtime gate"
        ));
    }
    validate_bounded_strings(&gate.module_enumeration.names, label)?;
    validate_bounded_strings(&gate.graphics.hits, label)?;
    validate_bounded_strings(&gate.anti_cheat.module_hits, label)?;
    validate_bounded_strings(&gate.blocking, label)?;
    validate_bounded_strings(&gate.limitations, label)?;
    for hit in gate
        .anti_cheat
        .service_hits
        .iter()
        .chain(gate.anti_cheat.driver_hits.iter())
    {
        if hit.name.trim().is_empty()
            || hit.name.len() > 512
            || hit.display_name.len() > 512
            || hit.state.len() > 128
            || hit.start_mode.len() > 128
        {
            return Err(format!("{label} protection hit is not bounded/canonical"));
        }
        if hit.state == "Running" {
            return Err(format!(
                "{label} contains an unaccounted running protection hit"
            ));
        }
    }
    Ok(captured)
}

fn validate_bounded_strings(values: &[String], label: &str) -> Result<(), String> {
    if values
        .iter()
        .any(|value| value.len() > 512 || value.chars().any(char::is_control))
    {
        return Err(format!("{label} contains an unbounded/control string"));
    }
    Ok(())
}

fn validate_representative_scene_snapshot(
    bytes: &[u8],
    target: &DsvSelectionTargetIdentity,
    run: &InjectionReceiptRun,
    arm_runtime_sha256: &str,
    arm_runtime_captured: time::OffsetDateTime,
) -> Result<(), String> {
    let scene: RepresentativeSceneSnapshot = serde_json::from_slice(bytes)
        .map_err(|error| format!("cannot parse representative-scene snapshot: {error}"))?;
    let observed = parse_utc_timestamp(&scene.observed_at_utc)?;
    validate_utc_timestamp(
        &scene.runtime_evidence.process_start_time_utc,
        "representative-scene process start",
    )?;
    if scene.schema != "generic-d3d11-representative-scene-attestation/v1"
        || scene.attestation_kind != "local_operator"
        || scene.scene_kind != "representative_offline_single_player"
        || scene.game_id != target.game_id
        || scene.steam_app_id != target.steam_app_id
        || scene.steam_build_id != target.steam_build_id
        || scene.discovery_id != run.discovery_id
        || scene.output_leaf != run.output_leaf
        || scene.executable != target.executable
        || scene.runtime_evidence.sha256 != arm_runtime_sha256
        || scene.runtime_evidence.process_id != run.process_id
        || scene.runtime_evidence.process_start_time_utc != run.process_start_time_utc
        || scene.window.class_name.trim().is_empty()
        || scene.window.class_name.len() > 256
        || !(1..=16_384).contains(&scene.window.client_width)
        || !(1..=16_384).contains(&scene.window.client_height)
        || scene.nonce.len() != 64
        || !scene
            .nonce
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        || observed < arm_runtime_captured
    {
        return Err("representative-scene snapshot does not bind the receipt/runtime gate".into());
    }
    Ok(())
}

fn validate_one_visual_review(
    evidence_root: &Path,
    review_path: &Path,
    run_leaf: &str,
    report: &ValidatedDsvSelectionReport,
) -> Result<ValidatedVisualReview, String> {
    let (bytes, locator) = read_bounded_regular_file(
        evidence_root,
        review_path,
        MAX_DSV_SELECTION_VISUAL_REVIEW_BYTES,
        "D3D11 DSV candidate visual review",
    )?;
    if locator != format!("{run_leaf}/{DSV_CANDIDATE_VISUAL_REVIEW_FILE_NAME}") {
        return Err("D3D11 DSV visual review is not the fixed file in its run directory".into());
    }
    let review: DsvCandidateVisualReview = serde_json::from_slice(&bytes)
        .map_err(|error| format!("cannot parse D3D11 DSV visual review: {error}"))?;
    if review.schema != DSV_CANDIDATE_VISUAL_REVIEW_SCHEMA_V1
        || review.report.locator != report.locator
        || review.report.sha256 != report.exact_bytes_sha256
        || review.candidate_id != report.selected_candidate_id
        || review.descriptor != report.selected_descriptor
        || !review.confirms_representative_scene_depth
        || review.publication_allowed
        || review.recording_ready
    {
        return Err("D3D11 DSV visual review does not bind the selected report/candidate".into());
    }
    validate_utc_timestamp(&review.reviewed_at_utc, "visual review timestamp")?;
    validate_reviewer(&review.reviewed_by)?;
    validate_evidence_binding(&review.visual_evidence, "visual evidence")?;
    if locator_parent(&review.visual_evidence.locator)? != run_leaf {
        return Err("D3D11 DSV visual evidence is not in its run directory".into());
    }
    let visual_leaf = review
        .visual_evidence
        .locator
        .split_once('/')
        .map(|(_, leaf)| leaf)
        .ok_or("D3D11 DSV visual evidence locator has no file")?;
    if visual_leaf.contains('/')
        || matches!(
            visual_leaf,
            DSV_DISCOVERY_REPORT_FILE_NAME
                | DSV_DISCOVERY_ACTIVATION_ACK_FILE_NAME
                | DSV_CANDIDATE_VISUAL_REVIEW_FILE_NAME
        )
    {
        return Err("D3D11 DSV visual evidence locator aliases a control artifact".into());
    }
    let visual_path = evidence_root.join(Path::new(&review.visual_evidence.locator));
    let (visual_bytes, actual_locator) = read_bounded_regular_file(
        evidence_root,
        &visual_path,
        MAX_DSV_SELECTION_VISUAL_EVIDENCE_BYTES,
        "D3D11 DSV visual evidence",
    )?;
    let actual_hash = format!("sha256:{:x}", Sha256::digest(&visual_bytes));
    if actual_locator != review.visual_evidence.locator
        || visual_bytes.len() as u64 != review.visual_evidence.length_bytes
        || actual_hash != review.visual_evidence.sha256
    {
        return Err("D3D11 DSV visual evidence length/hash/locator mismatch".into());
    }
    validate_visual_png(&visual_bytes, visual_leaf)?;
    Ok(ValidatedVisualReview {
        locator,
        exact_bytes_sha256: format!("sha256:{:x}", Sha256::digest(&bytes)),
        reviewed_at_utc: review.reviewed_at_utc,
        reviewed_by: review.reviewed_by,
        visual_evidence: review.visual_evidence,
    })
}

fn validate_visual_png(bytes: &[u8], leaf: &str) -> Result<(), String> {
    const MAX_VISUAL_PIXELS: u64 = 33_554_432;
    const MAX_DECODED_VISUAL_BYTES: usize = 128 * 1024 * 1024;
    if Path::new(leaf).extension().and_then(|value| value.to_str()) != Some("png")
        || !bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a])
    {
        return Err("D3D11 DSV visual evidence must be a lowercase .png image".into());
    }
    let decoder = png::Decoder::new(Cursor::new(bytes));
    let mut reader = decoder
        .read_info()
        .map_err(|error| format!("cannot parse D3D11 DSV visual PNG: {error}"))?;
    let info = reader.info();
    let pixels = u64::from(info.width)
        .checked_mul(u64::from(info.height))
        .ok_or("D3D11 DSV visual PNG dimensions overflow")?;
    if info.width == 0
        || info.height == 0
        || info.width > 16_384
        || info.height > 16_384
        || pixels > MAX_VISUAL_PIXELS
    {
        return Err("D3D11 DSV visual PNG dimensions are outside the admitted bound".into());
    }
    let output_size = reader
        .output_buffer_size()
        .ok_or("D3D11 DSV visual PNG decoded size overflowed")?;
    if output_size == 0 || output_size > MAX_DECODED_VISUAL_BYTES {
        return Err("D3D11 DSV visual PNG decoded bytes exceed the admitted bound".into());
    }
    let mut decoded = vec![0; output_size];
    let frame = reader
        .next_frame(&mut decoded)
        .map_err(|error| format!("cannot decode D3D11 DSV visual PNG: {error}"))?;
    if frame.width == 0 || frame.height == 0 || frame.buffer_size() == 0 {
        return Err("D3D11 DSV visual PNG decoded to an empty image".into());
    }
    Ok(())
}

impl DsvSelectionAttestation {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema != DSV_SELECTION_ATTESTATION_SCHEMA_V1
            || self.status != "bridge_ready_only"
            || self.renderer_backend != "d3d11"
            || self.capability_ceiling != "dsv_selection_bridge_only"
            || self.publication_allowed
            || self.deliverable
            || self.recording_ready
            || self.depth_profile_generated
            || self.camera_binding_generated
        {
            return Err("D3D11 DSV selection attestation exceeds its bridge-only ceiling".into());
        }
        validate_target_identity(&self.target)?;
        self.selected_descriptor.validate()?;
        if self.runs[0].discovery_id == self.runs[1].discovery_id {
            return Err("D3D11 DSV selection attestation repeats one discovery id".into());
        }
        let mut source_locators = BTreeSet::new();
        for run in &self.runs {
            validate_safe_component(&run.discovery_id, "attestation discovery id")?;
            validate_safe_component(&run.selected_candidate_id, "attestation candidate id")?;
            for (label, binding) in [
                ("report", &run.report),
                ("activation ACK", &run.activation_ack),
                ("injection receipt", &run.injection_receipt),
                ("visual review", &run.visual_review),
            ] {
                validate_source_binding(binding, label)?;
                if !source_locators.insert(binding.locator.as_str()) {
                    return Err("D3D11 DSV selection attestation repeats a source locator".into());
                }
            }
            validate_evidence_binding(&run.visual_evidence, "visual evidence")?;
            validate_evidence_binding(
                &run.representative_scene_evidence,
                "representative-scene evidence",
            )?;
            validate_utc_timestamp(&run.reviewed_at_utc, "attestation review timestamp")?;
            validate_reviewer(&run.reviewed_by)?;
            let report_parent = locator_parent(&run.report.locator)?;
            if locator_parent(&run.activation_ack.locator)? != report_parent
                || locator_parent(&run.visual_review.locator)? != report_parent
                || locator_parent(&run.visual_evidence.locator)? != report_parent
                || run.report.locator != format!("{report_parent}/{DSV_DISCOVERY_REPORT_FILE_NAME}")
                || run.activation_ack.locator
                    != format!("{report_parent}/{DSV_DISCOVERY_ACTIVATION_ACK_FILE_NAME}")
                || run.visual_review.locator
                    != format!("{report_parent}/{DSV_CANDIDATE_VISUAL_REVIEW_FILE_NAME}")
            {
                return Err("D3D11 DSV attestation run source topology is invalid".into());
            }
            let expected_bundle = format!("{report_parent}-injection-receipt");
            if run.injection_receipt.locator
                != format!("{expected_bundle}/{GENERIC_D3D11_INJECTION_RECEIPT_FILE_NAME}")
                || locator_parent(&run.representative_scene_evidence.locator)? != expected_bundle
            {
                return Err("D3D11 DSV attestation receipt topology is invalid".into());
            }
        }
        if locator_parent(&self.runs[0].report.locator)?
            == locator_parent(&self.runs[1].report.locator)?
        {
            return Err("D3D11 DSV selection attestation repeats one run directory".into());
        }
        Ok(())
    }
}

fn validate_target_identity(target: &DsvSelectionTargetIdentity) -> Result<(), String> {
    if target.game_id.is_empty()
        || target.game_id.len() > 128
        || !target
            .game_id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        || !target.game_id.as_bytes()[0].is_ascii_lowercase()
            && !target.game_id.as_bytes()[0].is_ascii_digit()
    {
        return Err("target game id is not canonical".into());
    }
    validate_decimal_id(&target.steam_app_id, "Steam AppID")?;
    validate_decimal_id(&target.steam_build_id, "Steam build id")?;
    validate_safe_component(&target.executable.basename, "target executable basename")?;
    if !target
        .executable
        .basename
        .to_ascii_lowercase()
        .ends_with(".exe")
        || !crate::camera_binding::is_sha256_digest(&target.executable.sha256)
    {
        return Err("D3D11 DSV selection target executable identity is invalid".into());
    }
    Ok(())
}

fn validate_decimal_id(value: &str, label: &str) -> Result<(), String> {
    let parsed = value
        .parse::<u64>()
        .map_err(|_| format!("{label} is not a positive canonical decimal integer"))?;
    if parsed == 0 || value != parsed.to_string() {
        return Err(format!(
            "{label} is not a positive canonical decimal integer"
        ));
    }
    Ok(())
}

fn validate_safe_component(value: &str, label: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > MAX_LOCATOR_COMPONENT_BYTES
        || matches!(value, "." | "..")
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
        || !value.as_bytes()[0].is_ascii_alphanumeric()
    {
        return Err(format!("{label} is not a safe bounded identifier"));
    }
    Ok(())
}

fn validate_source_binding(binding: &DsvSelectionSourceBinding, label: &str) -> Result<(), String> {
    validate_locator_text(&binding.locator, label)?;
    if !crate::camera_binding::is_sha256_digest(&binding.sha256) {
        return Err(format!("{label} hash is not canonical"));
    }
    Ok(())
}

fn validate_evidence_binding(
    binding: &DsvSelectionEvidenceBinding,
    label: &str,
) -> Result<(), String> {
    validate_locator_text(&binding.locator, label)?;
    if binding.length_bytes == 0
        || binding.length_bytes > MAX_DSV_SELECTION_VISUAL_EVIDENCE_BYTES
        || !crate::camera_binding::is_sha256_digest(&binding.sha256)
    {
        return Err(format!("{label} binding is not bounded/canonical"));
    }
    Ok(())
}

fn validate_locator_text(locator: &str, label: &str) -> Result<(), String> {
    if locator.contains('\\') || safe_relative_locator(Path::new(locator), label)? != locator {
        return Err(format!("{label} locator is not canonical"));
    }
    Ok(())
}

fn validate_reviewer(value: &str) -> Result<(), String> {
    if value.trim() != value
        || value.is_empty()
        || value.len() > 128
        || value.chars().any(char::is_control)
    {
        return Err("D3D11 DSV visual review has an invalid reviewer".into());
    }
    Ok(())
}

fn validate_utc_timestamp(value: &str, label: &str) -> Result<(), String> {
    parse_utc_timestamp(value)
        .map(|_| ())
        .map_err(|error| format!("{label}: {error}"))
}

fn parse_utc_timestamp(value: &str) -> Result<time::OffsetDateTime, String> {
    use time::format_description::well_known::Rfc3339;
    if value.len() != 28
        || !value.ends_with('Z')
        || value.as_bytes().get(19) != Some(&b'.')
        || !value.as_bytes()[20..27].iter().all(u8::is_ascii_digit)
    {
        return Err("timestamp is not bounded canonical UTC RFC3339".into());
    }
    let parsed = time::OffsetDateTime::parse(value, &Rfc3339)
        .map_err(|error| format!("timestamp is not RFC3339: {error}"))?;
    if parsed.offset() != time::UtcOffset::UTC {
        return Err("timestamp is not UTC".into());
    }
    Ok(parsed)
}

fn utc_to_filetime(value: time::OffsetDateTime) -> Result<String, String> {
    const WINDOWS_EPOCH_OFFSET_100NS: i128 = 116_444_736_000_000_000;
    let ticks = value
        .unix_timestamp_nanos()
        .checked_div(100)
        .and_then(|ticks| ticks.checked_add(WINDOWS_EPOCH_OFFSET_100NS))
        .ok_or("process timestamp is outside FILETIME range")?;
    let ticks = u64::try_from(ticks).map_err(|_| "process timestamp is outside FILETIME range")?;
    Ok(ticks.to_string())
}

#[cfg(feature = "evidence-authoring")]
fn write_attestation_create_new(
    evidence_root: &Path,
    output_path: &Path,
    attestation: &DsvSelectionAttestation,
) -> Result<PathBuf, String> {
    attestation.validate()?;
    if !evidence_root.is_absolute() || !output_path.is_absolute() {
        return Err("D3D11 DSV selection output paths must be absolute".into());
    }
    reject_reparse_chain(evidence_root, "selection output evidence root")?;
    let root = evidence_root
        .canonicalize()
        .map_err(|error| format!("cannot resolve selection output evidence root: {error}"))?;
    let parent = output_path
        .parent()
        .ok_or("D3D11 DSV selection output has no parent")?;
    reject_reparse_chain(parent, "selection output parent")?;
    let canonical_parent = parent
        .canonicalize()
        .map_err(|error| format!("cannot resolve selection output parent: {error}"))?;
    let leaf = output_path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or("D3D11 DSV selection output leaf is not UTF-8")?;
    validate_safe_component(leaf, "D3D11 DSV selection output leaf")?;
    if canonical_parent != root || !leaf.ends_with(".json") || output_path.exists() {
        return Err(
            "D3D11 DSV selection output must be a new JSON direct child of evidence_root".into(),
        );
    }
    let mut bytes = serde_json::to_vec_pretty(attestation)
        .map_err(|error| format!("cannot serialize D3D11 DSV selection attestation: {error}"))?;
    bytes.push(b'\n');
    if bytes.len() as u64 > MAX_DSV_SELECTION_ATTESTATION_BYTES {
        return Err("D3D11 DSV selection attestation exceeds its byte bound".into());
    }
    publish_prepared_attestation(&root, output_path, leaf, &bytes)
}

#[cfg(all(feature = "evidence-authoring", windows))]
fn publish_prepared_attestation(
    root: &Path,
    output: &Path,
    final_leaf: &str,
    bytes: &[u8],
) -> Result<PathBuf, String> {
    use std::os::windows::ffi::OsStrExt as _;
    use std::os::windows::fs::OpenOptionsExt as _;
    use std::os::windows::io::AsRawHandle as _;
    use windows::Wdk::Storage::FileSystem::{
        FILE_RENAME_INFORMATION, FileRenameInformation, NtSetInformationFile,
    };
    use windows::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, HANDLE};
    use windows::Win32::Storage::FileSystem::{
        DELETE, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TAG_INFO,
        FILE_DISPOSITION_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_SHARE_READ, FILE_SHARE_WRITE, FileAttributeTagInfo, FileDispositionInfo,
        GetFileInformationByHandleEx, SetFileInformationByHandle,
    };
    use windows::Win32::System::IO::IO_STATUS_BLOCK;

    let directory = OpenOptions::new()
        .access_mode(GENERIC_READ.0 | GENERIC_WRITE.0)
        .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0 | FILE_FLAG_OPEN_REPARSE_POINT.0)
        .open(root)
        .map_err(|error| format!("cannot pin D3D11 DSV selection output directory: {error}"))?;
    let mut attributes = FILE_ATTRIBUTE_TAG_INFO::default();
    unsafe {
        GetFileInformationByHandleEx(
            HANDLE(directory.as_raw_handle()),
            FileAttributeTagInfo,
            (&raw mut attributes).cast(),
            u32::try_from(std::mem::size_of::<FILE_ATTRIBUTE_TAG_INFO>())
                .map_err(|_| "D3D11 DSV selection directory attribute buffer is too large")?,
        )
    }
    .map_err(|error| format!("cannot inspect D3D11 DSV selection output directory: {error}"))?;
    if attributes.FileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 == 0
        || attributes.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
    {
        return Err("D3D11 DSV selection output handle is not a plain directory".into());
    }
    let pending = root.join(format!(
        ".dsv-selection-pending-{}.tmp",
        uuid::Uuid::new_v4().simple()
    ));
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .access_mode(GENERIC_READ.0 | GENERIC_WRITE.0 | DELETE.0)
        .share_mode(0)
        .create_new(true);
    let mut file = options
        .open(&pending)
        .map_err(|error| format!("cannot create pending D3D11 DSV attestation: {error}"))?;
    let prepare_result = (|| -> Result<(), String> {
        let opened_path = pending
            .canonicalize()
            .map_err(|error| format!("cannot resolve pending D3D11 DSV attestation: {error}"))?;
        let opened_metadata = file
            .metadata()
            .map_err(|error| format!("cannot inspect pending D3D11 DSV attestation: {error}"))?;
        if opened_path.parent() != Some(root)
            || is_reparse_point(&opened_metadata)
            || !opened_metadata.is_file()
        {
            return Err("pending D3D11 DSV attestation escaped its checked root".into());
        }
        file.write_all(bytes)
            .and_then(|_| file.sync_all())
            .and_then(|_| file.seek(SeekFrom::Start(0)).map(|_| ()))
            .map_err(|error| format!("cannot seal pending D3D11 DSV attestation: {error}"))?;
        let mut readback = Vec::with_capacity(bytes.len());
        (&mut file)
            .take(MAX_DSV_SELECTION_ATTESTATION_BYTES + 1)
            .read_to_end(&mut readback)
            .map_err(|error| format!("cannot read back pending D3D11 DSV attestation: {error}"))?;
        let sealed_metadata = file
            .metadata()
            .map_err(|error| format!("cannot re-inspect pending D3D11 DSV attestation: {error}"))?;
        let sealed_path = pending
            .canonicalize()
            .map_err(|error| format!("cannot re-resolve pending D3D11 DSV attestation: {error}"))?;
        if readback != bytes
            || sealed_metadata.len() != bytes.len() as u64
            || sealed_path != opened_path
        {
            return Err("pending D3D11 DSV attestation changed before commit".into());
        }
        #[cfg(test)]
        let fail_before_commit = {
            let mut leaf = TEST_ATTESTATION_FAIL_BEFORE_COMMIT_LEAF
                .lock()
                .map_err(|_| "D3D11 DSV attestation failure hook lock poisoned")?;
            if leaf.as_deref() == Some(final_leaf) {
                leaf.take();
                true
            } else {
                false
            }
        };
        #[cfg(test)]
        if fail_before_commit {
            return Err("injected D3D11 DSV attestation pre-commit failure".into());
        }

        let name = std::ffi::OsStr::new(final_leaf)
            .encode_wide()
            .collect::<Vec<_>>();
        let name_bytes = name
            .len()
            .checked_mul(std::mem::size_of::<u16>())
            .and_then(|length| u32::try_from(length).ok())
            .ok_or("D3D11 DSV attestation target leaf is too long")?;
        let header_bytes = std::mem::offset_of!(FILE_RENAME_INFORMATION, FileName);
        let buffer_bytes = header_bytes
            .checked_add(name_bytes as usize)
            .ok_or("D3D11 DSV attestation rename buffer overflow")?;
        let words = buffer_bytes
            .checked_add(std::mem::size_of::<usize>() - 1)
            .ok_or("D3D11 DSV attestation rename buffer overflow")?
            / std::mem::size_of::<usize>();
        let mut buffer = vec![0usize; words];
        let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFORMATION>();
        unsafe {
            (*info).Anonymous.ReplaceIfExists = false;
            (*info).RootDirectory = HANDLE(directory.as_raw_handle());
            (*info).FileNameLength = name_bytes;
            std::ptr::copy_nonoverlapping(name.as_ptr(), (*info).FileName.as_mut_ptr(), name.len());
            let mut io_status = IO_STATUS_BLOCK::default();
            NtSetInformationFile(
                HANDLE(file.as_raw_handle()),
                &mut io_status,
                info.cast(),
                u32::try_from(buffer_bytes)
                    .map_err(|_| "D3D11 DSV attestation rename buffer is too large")?,
                FileRenameInformation,
            )
            .ok()
            .map_err(|error| {
                format!("cannot atomically publish D3D11 DSV selection attestation: {error}")
            })?;
        }
        Ok(())
    })();
    if let Err(error) = prepare_result {
        // Dispose the still-owned share-none file object. Never drop the handle and then delete a
        // path that an attacker could replace.
        let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
        let _ = unsafe {
            SetFileInformationByHandle(
                HANDLE(file.as_raw_handle()),
                FileDispositionInfo,
                (&raw const disposition).cast(),
                u32::try_from(std::mem::size_of::<FILE_DISPOSITION_INFO>()).unwrap_or(u32::MAX),
            )
        };
        drop(file);
        return Err(error);
    }
    drop(file);
    drop(directory);
    // Handle rename is the sole commit point. No fallible operation follows it.
    Ok(output.to_path_buf())
}

#[cfg(all(feature = "evidence-authoring", not(windows)))]
fn publish_prepared_attestation(
    root: &Path,
    output: &Path,
    _final_leaf: &str,
    bytes: &[u8],
) -> Result<PathBuf, String> {
    let pending = root.join(format!(
        ".dsv-selection-pending-{}.tmp",
        uuid::Uuid::new_v4().simple()
    ));
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&pending)
        .map_err(|error| format!("cannot create pending D3D11 DSV attestation: {error}"))?;
    let prepared = (|| -> Result<(), String> {
        file.write_all(bytes)
            .and_then(|_| file.sync_all())
            .and_then(|_| file.seek(SeekFrom::Start(0)).map(|_| ()))
            .map_err(|error| format!("cannot seal pending D3D11 DSV attestation: {error}"))?;
        let mut readback = Vec::with_capacity(bytes.len());
        file.read_to_end(&mut readback)
            .map_err(|error| format!("cannot read back pending D3D11 DSV attestation: {error}"))?;
        if readback != bytes {
            return Err("pending D3D11 DSV attestation changed before commit".into());
        }
        Ok(())
    })();
    if let Err(error) = prepared {
        drop(file);
        let _ = fs::remove_file(&pending);
        return Err(error);
    }
    drop(file);
    fs::hard_link(&pending, output)
        .map_err(|error| format!("cannot commit D3D11 DSV selection attestation: {error}"))?;
    // The hard link is the no-replace commit point. Cleanup is best-effort and cannot turn a
    // visible successful final artifact into Err.
    let _ = fs::remove_file(pending);
    Ok(output.to_path_buf())
}

#[cfg(all(test, feature = "evidence-authoring", windows))]
static TEST_ATTESTATION_FAIL_BEFORE_COMMIT_LEAF: std::sync::Mutex<Option<String>> =
    std::sync::Mutex::new(None);

fn locator_parent(locator: &str) -> Result<&str, String> {
    locator
        .split_once('/')
        .map(|(parent, _)| parent)
        .ok_or_else(|| "D3D11 DSV selection locator has no run directory".into())
}

fn read_bounded_regular_file(
    evidence_root: &Path,
    input: &Path,
    maximum_bytes: u64,
    label: &str,
) -> Result<(Vec<u8>, String), String> {
    if !evidence_root.is_absolute() || !input.is_absolute() {
        return Err(format!("{label} paths must be absolute"));
    }
    reject_reparse_chain(evidence_root, label)?;
    reject_reparse_chain(input, label)?;
    let root = evidence_root
        .canonicalize()
        .map_err(|error| format!("cannot resolve {label} evidence root: {error}"))?;
    if !root.is_dir() {
        return Err(format!("{label} evidence root is not a directory"));
    }
    let metadata =
        fs::symlink_metadata(input).map_err(|error| format!("cannot inspect {label}: {error}"))?;
    if is_reparse_point(&metadata)
        || !metadata.is_file()
        || metadata.len() == 0
        || metadata.len() > maximum_bytes
    {
        return Err(format!(
            "{label} must be a nonempty bounded non-reparse regular file"
        ));
    }
    let canonical_input = input
        .canonicalize()
        .map_err(|error| format!("cannot resolve {label}: {error}"))?;
    let relative = canonical_input
        .strip_prefix(&root)
        .map_err(|_| format!("{label} escaped evidence_root"))?;
    let locator = safe_relative_locator(relative, label)?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        options.share_mode(1); // FILE_SHARE_READ: reject new write/delete handles while hashing.
    }
    let file = options
        .open(&canonical_input)
        .map_err(|error| format!("cannot open {label}: {error}"))?;
    let opened_length = file
        .metadata()
        .map_err(|error| format!("cannot inspect opened {label}: {error}"))?
        .len();
    if opened_length == 0 || opened_length > maximum_bytes || opened_length != metadata.len() {
        return Err(format!("{label} changed before its bounded read"));
    }
    let capacity = usize::try_from(opened_length)
        .map_err(|_| format!("{label} length does not fit in memory"))?;
    let mut bytes = Vec::with_capacity(capacity);
    file.take(maximum_bytes + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("cannot read {label}: {error}"))?;
    if bytes.len() as u64 != opened_length || bytes.len() as u64 > maximum_bytes {
        return Err(format!("{label} changed during its bounded read"));
    }
    if bytes.starts_with(&[0xef, 0xbb, 0xbf]) {
        return Err(format!("{label} must not contain a UTF-8 BOM"));
    }
    let sealed_input = input
        .canonicalize()
        .map_err(|error| format!("cannot re-resolve {label}: {error}"))?;
    if sealed_input != canonical_input {
        return Err(format!("{label} path changed during its bounded read"));
    }
    reject_reparse_chain(input, label)?;
    Ok((bytes, locator))
}

fn safe_relative_locator(relative: &Path, label: &str) -> Result<String, String> {
    let components = relative.components().collect::<Vec<_>>();
    if components.is_empty() || components.len() > MAX_LOCATOR_COMPONENTS {
        return Err(format!(
            "{label} locator depth is outside the admitted bound"
        ));
    }
    let mut names = Vec::with_capacity(components.len());
    for component in components {
        let Component::Normal(name) = component else {
            return Err(format!("{label} locator is not a safe relative path"));
        };
        let name = name
            .to_str()
            .ok_or_else(|| format!("{label} locator is not UTF-8"))?;
        if name.is_empty()
            || name.len() > MAX_LOCATOR_COMPONENT_BYTES
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
        {
            return Err(format!("{label} locator contains an unsafe component"));
        }
        names.push(name);
    }
    Ok(names.join("/"))
}

fn is_reparse_point(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        false
    }
}

fn reject_reparse_chain(path: &Path, label: &str) -> Result<(), String> {
    for ancestor in path.ancestors() {
        let metadata = fs::symlink_metadata(ancestor)
            .map_err(|error| format!("cannot inspect {label} ancestor: {error}"))?;
        if is_reparse_point(&metadata) {
            return Err(format!("{label} ancestor is a reparse point"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsv_discovery::{
        DSV_DISCOVERY_CONFIG_SCHEMA_V2, DsvDiscoveryBuilder, DsvDiscoveryCandidateObservation,
        DsvDiscoveryCompletedPassObservation, DsvDiscoveryConfig, DsvDiscoveryFrameObservation,
        DsvDiscoveryLimits, DsvDiscoveryRuntimeCounters, write_report_create_new_under_root,
    };
    use std::fs::File;
    #[cfg(not(feature = "evidence-authoring"))]
    use std::io::Write as _;
    use std::path::PathBuf;

    fn root() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn descriptor(width: u32) -> DsvDiscoveryDescriptor {
        DsvDiscoveryDescriptor {
            view_format: 45,
            view_dimension: 3,
            view_flags: 0,
            mip_slice: 0,
            first_array_slice: 0,
            view_array_size: 1,
            resource_format: 44,
            width,
            height: 1080,
            resource_mip_levels: 1,
            resource_array_size: 1,
            sample_count: 1,
            sample_quality: 0,
            usage: 0,
            bind_flags: 0x40,
            cpu_access_flags: 0,
            misc_flags: 0,
        }
    }

    fn candidate(id: &str, descriptor: DsvDiscoveryDescriptor) -> DsvDiscoveryCandidateObservation {
        DsvDiscoveryCandidateObservation {
            candidate_id: id.into(),
            descriptor,
            bind_count: 1,
            unbind_count: 1,
            pass_count: 1,
            draw_calls: 10,
            clear_count: 1,
            completed_passes: vec![DsvDiscoveryCompletedPassObservation(10, 1, false)],
            dropped_completed_passes: 0,
            first_bound_order: Some(1),
            last_bound_order: Some(1),
            carried_from_previous_present: false,
        }
    }

    fn report(
        root: &Path,
        discovery_id: &str,
        frames: Vec<DsvDiscoveryFrameObservation>,
        runtime: DsvDiscoveryRuntimeCounters,
    ) -> DsvDiscoveryReport {
        let config = DsvDiscoveryConfig {
            schema: DSV_DISCOVERY_CONFIG_SCHEMA_V2.into(),
            discovery_id: discovery_id.into(),
            executable_sha256: format!("sha256:{}", "a".repeat(64)),
            evidence_root: root.display().to_string(),
            limits: DsvDiscoveryLimits {
                max_frames: 4,
                max_candidates: 4,
                max_candidates_per_frame: 4,
            },
        };
        let mut builder = DsvDiscoveryBuilder::new(config).unwrap();
        for frame in frames {
            builder.observe_frame(frame);
        }
        builder.finish(runtime).unwrap()
    }

    fn write_run_report(root: &Path, run: &str, report: &DsvDiscoveryReport) -> PathBuf {
        let directory = root.join(run);
        fs::create_dir(&directory).unwrap();
        write_report_create_new_under_root(root, &directory, report).unwrap()
    }

    fn write_activation_ack(
        root: &Path,
        run: &str,
        report: &DsvDiscoveryReport,
        process_id: u32,
        process_start_time_filetime: u64,
    ) -> PathBuf {
        write_activation_ack_with_config_hash(
            root,
            run,
            report,
            process_id,
            process_start_time_filetime,
            &format!("sha256:{}", "c".repeat(64)),
        )
    }

    fn write_activation_ack_with_config_hash(
        root: &Path,
        run: &str,
        report: &DsvDiscoveryReport,
        process_id: u32,
        process_start_time_filetime: u64,
        probe_config_sha256: &str,
    ) -> PathBuf {
        let ack = DsvDiscoveryActivationAck {
            schema: DSV_DISCOVERY_ACTIVATION_ACK_SCHEMA_V1.into(),
            status: "probe_discovery_runtime_activated".into(),
            discovery_id: report.discovery_id.clone(),
            session_id: format!("dsv-discovery-{}", report.discovery_id),
            process_id,
            process_start_time_filetime: process_start_time_filetime.to_string(),
            executable_sha256: report.executable_sha256.clone(),
            probe_config_sha256: probe_config_sha256.into(),
            output_leaf: run.into(),
            limits: report.limits,
            runtime: DsvDiscoveryActivationRuntime {
                writer_started: true,
                present_hook_installed: true,
                config_watcher_started: true,
                discovery_runtime_enabled: true,
                discovery_complete: false,
                recording_ready: false,
                formal_capture_armed: false,
                selection_ready: false,
                binding_ready: false,
            },
        };
        let path = root.join(run).join(DSV_DISCOVERY_ACTIVATION_ACK_FILE_NAME);
        fs::write(path.clone(), serde_json::to_vec_pretty(&ack).unwrap()).unwrap();
        path
    }

    #[derive(Debug)]
    struct CompleteRunFixture {
        run_directory: PathBuf,
        report_path: PathBuf,
        receipt_path: PathBuf,
        review_path: PathBuf,
        visual_path: PathBuf,
        candidate_id: String,
    }

    fn hash(bytes: &[u8]) -> String {
        format!("sha256:{:x}", Sha256::digest(bytes))
    }

    fn visual_png_bytes(seed: u8) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, 1, 1);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&[seed, 0x44, 0x88, 0xff]).unwrap();
        }
        bytes
    }

    fn snapshot(path: &Path, bytes: &[u8]) -> serde_json::Value {
        fs::write(path, bytes).unwrap();
        serde_json::json!({
            "locator": path.file_name().unwrap().to_str().unwrap(),
            "locatorScope": "receipt_bundle",
            "sha256": hash(bytes),
            "lengthBytes": bytes.len(),
            "publicationAllowed": false
        })
    }

    fn write_complete_run(
        root: &Path,
        run: &str,
        discovery_id: &str,
        candidate_id: &str,
        process_id: u32,
        process_start_time_utc: &str,
    ) -> CompleteRunFixture {
        let report = report(
            root,
            discovery_id,
            vec![DsvDiscoveryFrameObservation {
                present_seq: 1,
                candidates: vec![candidate(candidate_id, descriptor(1920))],
                ambiguous_descriptor_groups: vec![],
            }],
            DsvDiscoveryRuntimeCounters::default(),
        );
        let report_path = write_run_report(root, run, &report);
        let run_directory = root.join(run);
        let bundle_leaf = format!("{run}-injection-receipt");
        let bundle = root.join(&bundle_leaf);
        fs::create_dir(&bundle).unwrap();

        let runtime_value = serde_json::json!({
            "schema": "game-runtime-gate-evidence/v1",
            "captured_utc": "2026-09-03T03:30:00.0000000Z",
            "host": "fixture-host",
            "collector_sha256": format!("sha256:{}", "9".repeat(64)),
            "process": {
                "process_id": process_id,
                "process_name": "FixtureGame",
                "executable_path": root.join("FixtureGame.exe").display().to_string(),
                "executable_sha256": report.executable_sha256.clone(),
                "start_time_utc": process_start_time_utc
            },
            "module_enumeration": {
                "complete": true,
                "error": null,
                "collector_process_architecture": "x64",
                "target_pe_machine": "AMD64",
                "count": 2,
                "names": ["d3d11.dll", "dxgi.dll"]
            },
            "graphics": {
                "expected_api": "d3d11",
                "hits": ["d3d11.dll", "dxgi.dll"]
            },
            "anti_cheat": {
                "require_none_loaded": true,
                "module_hits": [],
                "service_enumeration_complete": true,
                "service_enumeration_error": null,
                "service_hits": [],
                "driver_enumeration_complete": true,
                "driver_enumeration_error": null,
                "driver_hits": [],
                "running_service_hits": 0,
                "running_driver_hits": 0
            },
            "status": "pass",
            "blocking": [],
            "limitations": ["offline fixture"]
        });
        let arm_bytes = serde_json::to_vec(&runtime_value).unwrap();
        let pre_bytes = serde_json::to_vec(&runtime_value).unwrap();
        let scene_bytes = serde_json::to_vec(&serde_json::json!({
            "schema": "generic-d3d11-representative-scene-attestation/v1",
            "attestationKind": "local_operator",
            "gameId": "fixture-game",
            "steamAppId": "123456",
            "steamBuildId": "987654",
            "discoveryId": discovery_id,
            "outputLeaf": run,
            "executable": {
                "basename": "FixtureGame.exe",
                "sha256": report.executable_sha256.clone()
            },
            "runtimeEvidence": {
                "sha256": hash(&arm_bytes),
                "processId": process_id,
                "processStartTimeUtc": process_start_time_utc
            },
            "window": {
                "className": "FixtureWindow",
                "clientWidth": 1920,
                "clientHeight": 1080
            },
            "sceneKind": "representative_offline_single_player",
            "observedAtUtc": "2026-09-03T03:45:00.0000000Z",
            "nonce": "7".repeat(64)
        }))
        .unwrap();
        let scene_sha256 = hash(&scene_bytes);
        let config_value = serde_json::json!({
            "enabled": true,
            "output_dir": run_directory.display().to_string(),
            "unbind_from_end": 0,
            "max_frames": null,
            "present_only": false,
            "watermark_only": false,
            "writer_bypass": false,
            "target_depth_hz": null,
            "lane_fps": null,
            "camera_probe": false,
            "camera_discovery": null,
            "representative_scene_attestation_sha256": scene_sha256,
            "dsv_discovery": {
                "schema": DSV_DISCOVERY_CONFIG_SCHEMA_V2,
                "discovery_id": discovery_id,
                "executable_sha256": report.executable_sha256.clone(),
                "evidence_root": root.display().to_string(),
                "limits": report.limits
            },
            "camera_production": null,
            "depth_profile_path": null,
            "depth_profile_sha256": null,
            "session_id": format!("dsv-discovery-{discovery_id}")
        });
        let config_bytes = serde_json::to_vec(&config_value).unwrap();
        let scene_binding = snapshot(
            &bundle.join("representative-scene-attestation.json"),
            &scene_bytes,
        );
        let arm_binding = snapshot(&bundle.join("arm-runtime-gate.json"), &arm_bytes);
        let pre_binding = snapshot(&bundle.join("pre-injection-runtime-gate.json"), &pre_bytes);
        let config_binding = snapshot(&bundle.join("armed-config.json"), &config_bytes);
        let config_sha256 = hash(&config_bytes);
        let filetime = utc_to_filetime(parse_utc_timestamp(process_start_time_utc).unwrap())
            .unwrap()
            .parse::<u64>()
            .unwrap();
        write_activation_ack_with_config_hash(
            root,
            run,
            &report,
            process_id,
            filetime,
            &config_sha256,
        );
        let receipt_value = serde_json::json!({
            "schema": GENERIC_D3D11_INJECTION_RECEIPT_SCHEMA_V1,
            "mode": "discovery_only",
            "status": "guarded_module_load_succeeded",
            "publicationAllowed": false,
            "probeInitializationConfirmed": false,
            "discoveryStarted": false,
            "recordingReady": false,
            "deliverableReady": false,
            "profileSelected": false,
            "profileGenerated": false,
            "selectionBindingReady": false,
            "bindingGenerated": false,
            "receipt": {
                "locator": format!("{bundle_leaf}/receipt.json"),
                "createdAtUtc": "2026-09-03T04:00:00.0000000Z"
            },
            "target": {
                "gameId": "fixture-game",
                "steamAppId": "123456",
                "steamBuildId": "987654",
                "executable": {
                    "basename": "FixtureGame.exe",
                    "sha256": report.executable_sha256.clone()
                }
            },
            "run": {
                "discoveryId": discovery_id,
                "outputLeaf": run,
                "requestedOutputLocator": run,
                "processId": process_id,
                "processStartTimeUtc": process_start_time_utc,
                "processStartTimeFiletime": filetime.to_string()
            },
            "artifacts": {
                "representativeSceneAttestationSnapshot": scene_binding,
                "armRuntimeGateSnapshot": arm_binding,
                "preInjectionRuntimeGateSnapshot": pre_binding,
                "armedConfigSnapshot": config_binding,
                "injector": { "sha256": format!("sha256:{}", "d".repeat(64)) },
                "probe": { "sha256": format!("sha256:{}", "e".repeat(64)) }
            },
            "injectorOutcome": {
                "exitCode": 0,
                "loadedProbeSha256": format!("sha256:{}", "e".repeat(64))
            }
        });
        let receipt_path = bundle.join(GENERIC_D3D11_INJECTION_RECEIPT_FILE_NAME);
        fs::write(
            &receipt_path,
            serde_json::to_vec_pretty(&receipt_value).unwrap(),
        )
        .unwrap();

        let report_bytes = fs::read(&report_path).unwrap();
        let visual_path = run_directory.join(format!("{candidate_id}-visual-evidence.png"));
        let visual_bytes = visual_png_bytes(process_id as u8);
        fs::write(&visual_path, &visual_bytes).unwrap();
        let review_value = serde_json::json!({
            "schema": DSV_CANDIDATE_VISUAL_REVIEW_SCHEMA_V1,
            "report": {
                "locator": format!("{run}/{DSV_DISCOVERY_REPORT_FILE_NAME}"),
                "sha256": hash(&report_bytes)
            },
            "candidateId": candidate_id,
            "descriptor": descriptor(1920),
            "visualEvidence": {
                "locator": format!("{run}/{candidate_id}-visual-evidence.png"),
                "sha256": hash(&visual_bytes),
                "lengthBytes": visual_bytes.len()
            },
            "reviewedAtUtc": "2026-09-03T05:00:00.0000000Z",
            "reviewedBy": "fixture-operator",
            "confirmsRepresentativeSceneDepth": true,
            "publicationAllowed": false,
            "recordingReady": false
        });
        let review_path = run_directory.join(DSV_CANDIDATE_VISUAL_REVIEW_FILE_NAME);
        fs::write(
            &review_path,
            serde_json::to_vec_pretty(&review_value).unwrap(),
        )
        .unwrap();
        CompleteRunFixture {
            run_directory,
            report_path,
            receipt_path,
            review_path,
            visual_path,
            candidate_id: candidate_id.into(),
        }
    }

    fn write_complete_pair(root: &Path) -> [CompleteRunFixture; 2] {
        [
            write_complete_run(
                root,
                "run-a",
                "discovery-a",
                "dsv-0001",
                101,
                "2026-09-03T02:00:00.0000000Z",
            ),
            write_complete_run(
                root,
                "run-b",
                "discovery-b",
                "dsv-0002",
                202,
                "2026-09-03T03:00:00.0000000Z",
            ),
        ]
    }

    #[cfg(feature = "evidence-authoring")]
    fn materialize_fixture(
        root: &Path,
        pair: &[CompleteRunFixture; 2],
        output: &Path,
    ) -> Result<PathBuf, String> {
        materialize_dsv_selection_attestation(
            root,
            [
                DsvSelectionRunInput {
                    run_directory: &pair[0].run_directory,
                    selected_candidate_id: &pair[0].candidate_id,
                },
                DsvSelectionRunInput {
                    run_directory: &pair[1].run_directory,
                    selected_candidate_id: &pair[1].candidate_id,
                },
            ],
            output,
        )
    }

    fn write_existing_attestation_fixture(
        root: &Path,
        pair: &[CompleteRunFixture; 2],
        output: &Path,
    ) {
        let attestation = build_dsv_selection_attestation(
            root,
            [
                DsvSelectionRunInput {
                    run_directory: &pair[0].run_directory,
                    selected_candidate_id: &pair[0].candidate_id,
                },
                DsvSelectionRunInput {
                    run_directory: &pair[1].run_directory,
                    selected_candidate_id: &pair[1].candidate_id,
                },
            ],
        )
        .unwrap();
        let mut bytes = serde_json::to_vec_pretty(&attestation).unwrap();
        bytes.push(b'\n');
        fs::write(output, bytes).unwrap();
    }

    #[cfg(feature = "evidence-authoring")]
    fn pending_attestation_leaves(root: &Path) -> Vec<String> {
        fs::read_dir(root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .filter(|leaf| leaf.starts_with(".dsv-selection-pending-"))
            .collect()
    }

    fn pair<'a>(
        first: &'a Path,
        first_id: &'a str,
        second: &'a Path,
        second_id: &'a str,
    ) -> [DsvSelectionReportInput<'a>; 2] {
        [
            DsvSelectionReportInput {
                report_path: first,
                selected_candidate_id: first_id,
            },
            DsvSelectionReportInput {
                report_path: second,
                selected_candidate_id: second_id,
            },
        ]
    }

    #[test]
    fn two_complete_reports_accept_different_candidate_ids_for_one_exact_descriptor() {
        let fixture = root();
        let root = fixture.path();
        let mut first_report = report(
            root,
            "discovery-a",
            vec![DsvDiscoveryFrameObservation {
                present_seq: 1,
                candidates: vec![candidate("dsv-0001", descriptor(1920))],
                ambiguous_descriptor_groups: vec![],
            }],
            DsvDiscoveryRuntimeCounters::default(),
        );
        first_report.schema = DSV_DISCOVERY_REPORT_SCHEMA_V2.into();
        first_report.summary.runtime.hook_integrity_failure_reasons = None;
        let second_report = report(
            root,
            "discovery-b",
            vec![DsvDiscoveryFrameObservation {
                present_seq: 2,
                candidates: vec![candidate("dsv-0002", descriptor(1920))],
                ambiguous_descriptor_groups: vec![],
            }],
            DsvDiscoveryRuntimeCounters::default(),
        );
        let first = write_run_report(root, "run-a", &first_report);
        let second = write_run_report(root, "run-b", &second_report);
        let validated =
            validate_dsv_selection_report_pair(root, pair(&first, "dsv-0001", &second, "dsv-0002"))
                .unwrap();
        assert_eq!(validated.selected_descriptor, descriptor(1920));
        assert_eq!(
            validated.reports[0].locator,
            "run-a/d3d11-dsv-discovery-report.json"
        );
        assert_ne!(
            validated.reports[0].exact_bytes_sha256,
            validated.reports[1].exact_bytes_sha256
        );
        let first_ack = write_activation_ack(root, "run-a", &first_report, 101, 1001);
        let second_ack = write_activation_ack(root, "run-b", &second_report, 202, 2002);
        let activations =
            validate_dsv_selection_activation_ack_pair(root, &validated, [&first_ack, &second_ack])
                .unwrap();
        assert_eq!(activations[0].process_id, 101);
        assert_eq!(
            activations[1].locator,
            "run-b/d3d11-dsv-discovery-activation-ack.json"
        );
    }

    #[test]
    fn activation_ack_rejects_duplicate_process_identity_unknown_fields_and_runtime_overclaim() {
        let fixture = root();
        let root = fixture.path();
        let make_report = |id: &str, candidate_id: &str| {
            report(
                root,
                id,
                vec![DsvDiscoveryFrameObservation {
                    present_seq: 1,
                    candidates: vec![candidate(candidate_id, descriptor(1920))],
                    ambiguous_descriptor_groups: vec![],
                }],
                DsvDiscoveryRuntimeCounters::default(),
            )
        };
        let first_report = make_report("discovery-a", "dsv-0001");
        let second_report = make_report("discovery-b", "dsv-0002");
        let first = write_run_report(root, "run-a", &first_report);
        let second = write_run_report(root, "run-b", &second_report);
        let reports =
            validate_dsv_selection_report_pair(root, pair(&first, "dsv-0001", &second, "dsv-0002"))
                .unwrap();
        let first_ack = write_activation_ack(root, "run-a", &first_report, 101, 1001);
        let second_ack = write_activation_ack(root, "run-b", &second_report, 101, 2002);
        assert!(
            validate_dsv_selection_activation_ack_pair(root, &reports, [&first_ack, &second_ack])
                .unwrap_err()
                .contains("process id")
        );

        let second_value: serde_json::Value =
            serde_json::from_slice(&fs::read(&second_ack).unwrap()).unwrap();
        let mut unknown = second_value.clone();
        unknown["unexpected"] = serde_json::json!(true);
        fs::write(&second_ack, serde_json::to_vec(&unknown).unwrap()).unwrap();
        assert!(validate_one_activation_ack(root, &second_ack, &reports.reports[1]).is_err());

        let mut runtime_overclaim = second_value;
        runtime_overclaim["runtime"]["selectionReady"] = serde_json::json!(true);
        fs::write(&second_ack, serde_json::to_vec(&runtime_overclaim).unwrap()).unwrap();
        assert!(
            validate_one_activation_ack(root, &second_ack, &reports.reports[1])
                .unwrap_err()
                .contains("runtime ceiling")
        );
    }

    #[test]
    fn activation_ack_requires_distinct_process_start_and_exact_report_binding() {
        let fixture = root();
        let root = fixture.path();
        let make_report = |id: &str, candidate_id: &str| {
            report(
                root,
                id,
                vec![DsvDiscoveryFrameObservation {
                    present_seq: 1,
                    candidates: vec![candidate(candidate_id, descriptor(1920))],
                    ambiguous_descriptor_groups: vec![],
                }],
                DsvDiscoveryRuntimeCounters::default(),
            )
        };
        let first_report = make_report("discovery-a", "dsv-0001");
        let second_report = make_report("discovery-b", "dsv-0002");
        let first = write_run_report(root, "run-a", &first_report);
        let second = write_run_report(root, "run-b", &second_report);
        let reports =
            validate_dsv_selection_report_pair(root, pair(&first, "dsv-0001", &second, "dsv-0002"))
                .unwrap();
        let first_ack = write_activation_ack(root, "run-a", &first_report, 101, 1001);
        let second_ack = write_activation_ack(root, "run-b", &second_report, 202, 1001);
        assert!(
            validate_dsv_selection_activation_ack_pair(root, &reports, [&first_ack, &second_ack])
                .unwrap_err()
                .contains("process start")
        );

        let mut drifted: serde_json::Value =
            serde_json::from_slice(&fs::read(&second_ack).unwrap()).unwrap();
        drifted["discoveryId"] = serde_json::json!("other-discovery");
        fs::write(&second_ack, serde_json::to_vec(&drifted).unwrap()).unwrap();
        assert!(
            validate_one_activation_ack(root, &second_ack, &reports.reports[1])
                .unwrap_err()
                .contains("identity")
        );
    }

    #[test]
    fn pair_rejects_repeated_run_executable_drift_and_descriptor_drift() {
        let fixture = root();
        let root = fixture.path();
        let base = |id: &str, width: u32| {
            report(
                root,
                id,
                vec![DsvDiscoveryFrameObservation {
                    present_seq: 1,
                    candidates: vec![candidate("dsv-0001", descriptor(width))],
                    ambiguous_descriptor_groups: vec![],
                }],
                DsvDiscoveryRuntimeCounters::default(),
            )
        };
        let first = write_run_report(root, "run-a", &base("discovery-a", 1920));
        let repeated = write_run_report(root, "run-b", &base("discovery-a", 1920));
        assert!(
            validate_dsv_selection_report_pair(
                root,
                pair(&first, "dsv-0001", &repeated, "dsv-0001"),
            )
            .unwrap_err()
            .contains("discovery id")
        );

        let mut executable_drift = base("discovery-c", 1920);
        executable_drift.executable_sha256 = format!("sha256:{}", "b".repeat(64));
        let executable_path = write_run_report(root, "run-c", &executable_drift);
        assert!(
            validate_dsv_selection_report_pair(
                root,
                pair(&first, "dsv-0001", &executable_path, "dsv-0001"),
            )
            .unwrap_err()
            .contains("executable bytes")
        );

        let descriptor_path = write_run_report(root, "run-d", &base("discovery-d", 1280));
        assert!(
            validate_dsv_selection_report_pair(
                root,
                pair(&first, "dsv-0001", &descriptor_path, "dsv-0001"),
            )
            .unwrap_err()
            .contains("full DSV descriptor")
        );
    }

    #[test]
    fn pair_rejects_incomplete_loss_and_global_descriptor_ambiguity() {
        let fixture = root();
        let root = fixture.path();
        let good_report = report(
            root,
            "discovery-a",
            vec![DsvDiscoveryFrameObservation {
                present_seq: 1,
                candidates: vec![candidate("dsv-0001", descriptor(1920))],
                ambiguous_descriptor_groups: vec![],
            }],
            DsvDiscoveryRuntimeCounters::default(),
        );
        let good = write_run_report(root, "run-a", &good_report);
        let incomplete_report = report(
            root,
            "discovery-b",
            vec![DsvDiscoveryFrameObservation {
                present_seq: 1,
                candidates: vec![candidate("dsv-0002", descriptor(1920))],
                ambiguous_descriptor_groups: vec![],
            }],
            DsvDiscoveryRuntimeCounters {
                hook_integrity_failures: 1,
                hook_integrity_failure_reasons: Some(
                    crate::dsv_discovery::DsvDiscoveryHookIntegrityFailureReasons {
                        tracker_failure: 1,
                        ..Default::default()
                    },
                ),
                ..Default::default()
            },
        );
        let incomplete = write_run_report(root, "run-b", &incomplete_report);
        assert!(
            validate_dsv_selection_report_pair(
                root,
                pair(&good, "dsv-0001", &incomplete, "dsv-0002"),
            )
            .is_err()
        );

        let ambiguous_report = report(
            root,
            "discovery-c",
            vec![
                DsvDiscoveryFrameObservation {
                    present_seq: 1,
                    candidates: vec![candidate("dsv-0002", descriptor(1920))],
                    ambiguous_descriptor_groups: vec![],
                },
                DsvDiscoveryFrameObservation {
                    present_seq: 2,
                    candidates: vec![candidate("dsv-0003", descriptor(1920))],
                    ambiguous_descriptor_groups: vec![],
                },
            ],
            DsvDiscoveryRuntimeCounters::default(),
        );
        let ambiguous = write_run_report(root, "run-c", &ambiguous_report);
        assert!(
            validate_dsv_selection_report_pair(
                root,
                pair(&good, "dsv-0001", &ambiguous, "dsv-0002"),
            )
            .unwrap_err()
            .contains("globally unique")
        );
    }

    #[test]
    fn every_runtime_drop_integrity_and_limit_class_blocks_report_selection() {
        let fixture = root();
        let root = fixture.path();
        let cases = [
            DsvDiscoveryRuntimeCounters {
                queue_dropped_frames: 1,
                ..Default::default()
            },
            DsvDiscoveryRuntimeCounters {
                hook_dropped_events: 1,
                ..Default::default()
            },
            DsvDiscoveryRuntimeCounters {
                hook_integrity_failures: 1,
                hook_integrity_failure_reasons: Some(
                    crate::dsv_discovery::DsvDiscoveryHookIntegrityFailureReasons {
                        tracker_failure: 1,
                        ..Default::default()
                    },
                ),
                ..Default::default()
            },
            DsvDiscoveryRuntimeCounters {
                candidate_limit_events: 1,
                ..Default::default()
            },
            DsvDiscoveryRuntimeCounters {
                pass_ledger_dropped_rows: 1,
                ..Default::default()
            },
        ];
        for (index, runtime) in cases.into_iter().enumerate() {
            let run = format!("run-{index}");
            let blocked_report = report(
                root,
                &format!("discovery-{index}"),
                vec![DsvDiscoveryFrameObservation {
                    present_seq: 1,
                    candidates: vec![candidate("dsv-0001", descriptor(1920))],
                    ambiguous_descriptor_groups: vec![],
                }],
                runtime,
            );
            assert!(!blocked_report.complete);
            let path = write_run_report(root, &run, &blocked_report);
            assert!(
                validate_one_report(
                    root,
                    &DsvSelectionReportInput {
                        report_path: &path,
                        selected_candidate_id: "dsv-0001",
                    }
                )
                .is_err(),
                "runtime blocker case {index} was accepted"
            );
        }
    }

    #[cfg(windows)]
    fn create_directory_link(target: &Path, link: &Path) {
        let status = std::process::Command::new("cmd.exe")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[cfg(unix)]
    fn create_directory_link(target: &Path, link: &Path) {
        std::os::unix::fs::symlink(target, link).unwrap();
    }

    #[cfg(windows)]
    fn remove_directory_link(link: &Path) {
        fs::remove_dir(link).unwrap();
    }

    #[cfg(unix)]
    fn remove_directory_link(link: &Path) {
        fs::remove_file(link).unwrap();
    }

    #[test]
    fn report_reader_rejects_a_reparse_run_directory() {
        let fixture = root();
        let root = fixture.path();
        let report = report(
            root,
            "discovery-a",
            vec![DsvDiscoveryFrameObservation {
                present_seq: 1,
                candidates: vec![candidate("dsv-0001", descriptor(1920))],
                ambiguous_descriptor_groups: vec![],
            }],
            DsvDiscoveryRuntimeCounters::default(),
        );
        let actual = write_run_report(root, "actual-run", &report);
        let linked_run = root.join("linked-run");
        create_directory_link(actual.parent().unwrap(), &linked_run);
        let linked_report = linked_run.join(DSV_DISCOVERY_REPORT_FILE_NAME);
        assert!(
            validate_one_report(
                root,
                &DsvSelectionReportInput {
                    report_path: &linked_report,
                    selected_candidate_id: "dsv-0001",
                }
            )
            .is_err()
        );
        remove_directory_link(&linked_run);
    }

    #[test]
    fn report_reader_rejects_escape_wrong_shape_and_oversize() {
        let fixture = root();
        let root = fixture.path();
        let outside = tempfile::NamedTempFile::new().unwrap();
        assert!(
            read_bounded_regular_file(
                root,
                outside.path(),
                MAX_DSV_DISCOVERY_REPORT_BYTES,
                "report"
            )
            .is_err()
        );

        let nested = root.join("run-a").join("nested");
        fs::create_dir_all(&nested).unwrap();
        let nested_path = nested.join(DSV_DISCOVERY_REPORT_FILE_NAME);
        fs::write(&nested_path, b"{}").unwrap();
        assert!(
            validate_one_report(
                root,
                &DsvSelectionReportInput {
                    report_path: &nested_path,
                    selected_candidate_id: "dsv-0001",
                }
            )
            .unwrap_err()
            .contains("direct-child")
        );

        let oversize_dir = root.join("run-b");
        fs::create_dir(&oversize_dir).unwrap();
        let oversize = oversize_dir.join(DSV_DISCOVERY_REPORT_FILE_NAME);
        let file = File::create(&oversize).unwrap();
        file.set_len(MAX_DSV_DISCOVERY_REPORT_BYTES + 1).unwrap();
        assert!(
            validate_one_report(
                root,
                &DsvSelectionReportInput {
                    report_path: &oversize,
                    selected_candidate_id: "dsv-0001",
                }
            )
            .is_err()
        );
    }

    #[test]
    fn exact_report_byte_hash_changes() {
        let fixture = root();
        let root = fixture.path();
        let report = report(
            root,
            "discovery-a",
            vec![DsvDiscoveryFrameObservation {
                present_seq: 1,
                candidates: vec![candidate("dsv-0001", descriptor(1920))],
                ambiguous_descriptor_groups: vec![],
            }],
            DsvDiscoveryRuntimeCounters::default(),
        );
        let path = write_run_report(root, "run-a", &report);
        let before = validate_one_report(
            root,
            &DsvSelectionReportInput {
                report_path: &path,
                selected_candidate_id: "dsv-0001",
            },
        )
        .unwrap();
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b" \n")
            .unwrap();
        let after = validate_one_report(
            root,
            &DsvSelectionReportInput {
                report_path: &path,
                selected_candidate_id: "dsv-0001",
            },
        )
        .unwrap();
        assert_ne!(before.exact_bytes_sha256, after.exact_bytes_sha256);
    }

    #[cfg(feature = "evidence-authoring")]
    #[test]
    fn materializer_binds_two_complete_runs_and_preserves_bridge_only_ceiling() {
        let fixture = root();
        let root = fixture.path();
        let pair = write_complete_pair(root);
        let output = root.join(DSV_SELECTION_ATTESTATION_FILE_NAME);
        let sealed = materialize_fixture(root, &pair, &output).unwrap();
        assert_eq!(sealed, output);
        let attestation: DsvSelectionAttestation =
            serde_json::from_slice(&fs::read(&output).unwrap()).unwrap();
        attestation.validate().unwrap();
        assert_eq!(attestation.status, "bridge_ready_only");
        assert_eq!(attestation.capability_ceiling, "dsv_selection_bridge_only");
        assert!(!attestation.recording_ready);
        assert!(!attestation.depth_profile_generated);
        assert!(!attestation.camera_binding_generated);
        assert_eq!(attestation.selected_descriptor, descriptor(1920));
        assert_eq!(attestation.runs[0].selected_candidate_id, "dsv-0001");
        assert_eq!(attestation.runs[1].selected_candidate_id, "dsv-0002");
        assert_eq!(
            attestation.runs[0].report.sha256,
            hash(&fs::read(&pair[0].report_path).unwrap())
        );
        assert_eq!(
            attestation.runs[0].injection_receipt.sha256,
            hash(&fs::read(&pair[0].receipt_path).unwrap())
        );
        assert!(materialize_fixture(root, &pair, &output).is_err());
        assert!(pending_attestation_leaves(root).is_empty());
    }

    #[test]
    fn existing_attestation_replay_returns_only_revalidated_identity() {
        let fixture = root();
        let root = fixture.path();
        let pair = write_complete_pair(root);
        let output = root.join(DSV_SELECTION_ATTESTATION_FILE_NAME);
        write_existing_attestation_fixture(root, &pair, &output);
        let bytes = fs::read(&output).unwrap();
        let exact_hash = hash(&bytes);

        let validated =
            validate_dsv_selection_attestation_with_sources(root, &output, &exact_hash).unwrap();
        assert_eq!(validated.locator, DSV_SELECTION_ATTESTATION_FILE_NAME);
        assert_eq!(validated.exact_bytes_sha256, exact_hash);
        assert_eq!(validated.target.game_id, "fixture-game");
        assert_eq!(
            validated.target.executable.sha256,
            format!("sha256:{}", "a".repeat(64))
        );
        assert_eq!(validated.selected_descriptor, descriptor(1920));

        assert!(
            validate_dsv_selection_attestation_with_sources(
                root,
                &output,
                &format!("sha256:{}", "f".repeat(64)),
            )
            .unwrap_err()
            .contains("exact-byte hash mismatch")
        );
    }

    #[test]
    fn existing_attestation_replay_rejects_overclaim_and_self_tamper() {
        for mutation in [
            "overclaim",
            "descriptor",
            "source-binding",
            "unknown",
            "bom",
        ] {
            let fixture = root();
            let root = fixture.path();
            let pair = write_complete_pair(root);
            let output = root.join(DSV_SELECTION_ATTESTATION_FILE_NAME);
            write_existing_attestation_fixture(root, &pair, &output);

            if mutation == "bom" {
                let mut bytes = vec![0xef, 0xbb, 0xbf];
                bytes.extend(fs::read(&output).unwrap());
                fs::write(&output, bytes).unwrap();
            } else {
                let mut value: serde_json::Value =
                    serde_json::from_slice(&fs::read(&output).unwrap()).unwrap();
                match mutation {
                    "overclaim" => value["recordingReady"] = serde_json::json!(true),
                    "descriptor" => value["selectedDescriptor"]["width"] = serde_json::json!(1280),
                    "source-binding" => {
                        value["runs"][0]["report"]["sha256"] =
                            serde_json::json!(format!("sha256:{}", "f".repeat(64)))
                    }
                    "unknown" => value["untrustedClaim"] = serde_json::json!(false),
                    _ => unreachable!(),
                }
                fs::write(&output, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
            }
            let mutated = fs::read(&output).unwrap();
            assert!(
                validate_dsv_selection_attestation_with_sources(root, &output, &hash(&mutated))
                    .is_err(),
                "{mutation}"
            );
        }
    }

    #[test]
    fn existing_attestation_replay_rejects_drift_in_every_transitive_source_class() {
        for source in [
            "report",
            "activation",
            "receipt",
            "review",
            "visual",
            "scene",
            "arm-runtime",
            "pre-runtime",
            "config",
        ] {
            let fixture = root();
            let root = fixture.path();
            let pair = write_complete_pair(root);
            let output = root.join(DSV_SELECTION_ATTESTATION_FILE_NAME);
            write_existing_attestation_fixture(root, &pair, &output);
            let attestation_hash = hash(&fs::read(&output).unwrap());
            let bundle = pair[0].receipt_path.parent().unwrap();
            let source_path = match source {
                "report" => pair[0].report_path.clone(),
                "activation" => pair[0]
                    .run_directory
                    .join(DSV_DISCOVERY_ACTIVATION_ACK_FILE_NAME),
                "receipt" => pair[0].receipt_path.clone(),
                "review" => pair[0].review_path.clone(),
                "visual" => pair[0].visual_path.clone(),
                "scene" => bundle.join("representative-scene-attestation.json"),
                "arm-runtime" => bundle.join("arm-runtime-gate.json"),
                "pre-runtime" => bundle.join("pre-injection-runtime-gate.json"),
                "config" => bundle.join("armed-config.json"),
                _ => unreachable!(),
            };
            fs::OpenOptions::new()
                .append(true)
                .open(source_path)
                .unwrap()
                .write_all(b" ")
                .unwrap();

            assert!(
                validate_dsv_selection_attestation_with_sources(root, &output, &attestation_hash,)
                    .is_err(),
                "{source} source drift was accepted"
            );
        }
    }

    #[cfg(all(feature = "evidence-authoring", windows))]
    #[test]
    fn attestation_publish_failure_and_existing_final_leave_no_pending_or_false_success() {
        let fixture = root();
        let root = fixture.path();
        let pair = write_complete_pair(root);
        let seed = root.join("seed-attestation.json");
        materialize_fixture(root, &pair, &seed).unwrap();
        let attestation: DsvSelectionAttestation =
            serde_json::from_slice(&fs::read(&seed).unwrap()).unwrap();
        fs::remove_file(&seed).unwrap();

        let fail_leaf = "fail-before-commit.json";
        *TEST_ATTESTATION_FAIL_BEFORE_COMMIT_LEAF.lock().unwrap() = Some(fail_leaf.into());
        let fail_output = root.join(fail_leaf);
        assert!(write_attestation_create_new(root, &fail_output, &attestation).is_err());
        assert!(!fail_output.exists());
        assert!(pending_attestation_leaves(root).is_empty());

        let existing = root.join("existing-attestation.json");
        fs::write(&existing, b"operator-owned-existing-final").unwrap();
        assert!(write_attestation_create_new(root, &existing, &attestation).is_err());
        assert_eq!(
            fs::read(&existing).unwrap(),
            b"operator-owned-existing-final"
        );
        assert!(pending_attestation_leaves(root).is_empty());

        let mut invalid = attestation;
        invalid.recording_ready = true;
        let invalid_output = root.join("invalid-before-pending.json");
        assert!(write_attestation_create_new(root, &invalid_output, &invalid).is_err());
        assert!(!invalid_output.exists());
        assert!(pending_attestation_leaves(root).is_empty());
    }

    #[cfg(feature = "evidence-authoring")]
    #[test]
    fn materializer_rejects_receipt_overclaim_identity_drift_and_snapshot_byte_drift() {
        for mutation in ["overclaim", "identity", "snapshot", "extra", "bom"] {
            let fixture = root();
            let root = fixture.path();
            let pair = write_complete_pair(root);
            if mutation == "snapshot" {
                fs::OpenOptions::new()
                    .append(true)
                    .open(
                        pair[0]
                            .receipt_path
                            .parent()
                            .unwrap()
                            .join("armed-config.json"),
                    )
                    .unwrap()
                    .write_all(b" ")
                    .unwrap();
            } else if mutation == "extra" {
                fs::write(
                    pair[0]
                        .receipt_path
                        .parent()
                        .unwrap()
                        .join("unexpected.json"),
                    b"{}",
                )
                .unwrap();
            } else if mutation == "bom" {
                let mut bytes = vec![0xef, 0xbb, 0xbf];
                bytes.extend(fs::read(&pair[0].receipt_path).unwrap());
                fs::write(&pair[0].receipt_path, bytes).unwrap();
            } else {
                let mut receipt: serde_json::Value =
                    serde_json::from_slice(&fs::read(&pair[0].receipt_path).unwrap()).unwrap();
                if mutation == "overclaim" {
                    receipt["recordingReady"] = serde_json::json!(true);
                } else {
                    receipt["run"]["processId"] = serde_json::json!(303);
                }
                fs::write(&pair[0].receipt_path, serde_json::to_vec(&receipt).unwrap()).unwrap();
            }
            let output = root.join(DSV_SELECTION_ATTESTATION_FILE_NAME);
            assert!(
                materialize_fixture(root, &pair, &output).is_err(),
                "{mutation}"
            );
            assert!(!output.exists());
        }
    }

    #[cfg(feature = "evidence-authoring")]
    #[test]
    fn materializer_rejects_unknown_receipt_field_and_cross_run_target_drift() {
        for mutation in ["unknown", "target"] {
            let fixture = root();
            let root = fixture.path();
            let pair = write_complete_pair(root);
            let receipt_path = if mutation == "unknown" {
                &pair[0].receipt_path
            } else {
                &pair[1].receipt_path
            };
            let mut receipt: serde_json::Value =
                serde_json::from_slice(&fs::read(receipt_path).unwrap()).unwrap();
            if mutation == "unknown" {
                receipt["unexpected"] = serde_json::json!(false);
            } else {
                receipt["target"]["steamBuildId"] = serde_json::json!("987655");
            }
            fs::write(receipt_path, serde_json::to_vec(&receipt).unwrap()).unwrap();
            assert!(
                materialize_fixture(root, &pair, &root.join(DSV_SELECTION_ATTESTATION_FILE_NAME))
                    .is_err(),
                "{mutation}"
            );
        }
    }

    #[test]
    fn frozen_scene_and_runtime_snapshots_replay_strict_semantics() {
        let fixture = root();
        let root = fixture.path();
        let pair = write_complete_pair(root);
        let receipt: GenericD3d11InjectionReceipt =
            serde_json::from_slice(&fs::read(&pair[0].receipt_path).unwrap()).unwrap();
        let bundle = pair[0].receipt_path.parent().unwrap();
        let arm_bytes = fs::read(bundle.join("arm-runtime-gate.json")).unwrap();
        let arm_captured = validate_frozen_runtime_gate(
            &arm_bytes,
            &receipt.target,
            receipt.run.process_id,
            &receipt.run.process_start_time_utc,
            "arm fixture",
        )
        .unwrap();
        let scene_bytes = fs::read(bundle.join("representative-scene-attestation.json")).unwrap();
        validate_representative_scene_snapshot(
            &scene_bytes,
            &receipt.target,
            &receipt.run,
            &hash(&arm_bytes),
            arm_captured,
        )
        .unwrap();

        let mut drifted_runtime: serde_json::Value = serde_json::from_slice(&arm_bytes).unwrap();
        drifted_runtime["status"] = serde_json::json!("pass-looking");
        assert!(
            validate_frozen_runtime_gate(
                &serde_json::to_vec(&drifted_runtime).unwrap(),
                &receipt.target,
                receipt.run.process_id,
                &receipt.run.process_start_time_utc,
                "drifted runtime",
            )
            .is_err()
        );

        let mut drifted_scene: serde_json::Value = serde_json::from_slice(&scene_bytes).unwrap();
        drifted_scene["sceneKind"] = serde_json::json!("menu");
        assert!(
            validate_representative_scene_snapshot(
                &serde_json::to_vec(&drifted_scene).unwrap(),
                &receipt.target,
                &receipt.run,
                &hash(&arm_bytes),
                arm_captured,
            )
            .is_err()
        );
    }

    #[cfg(feature = "evidence-authoring")]
    #[test]
    fn materializer_requires_exact_human_visual_reviews_and_visual_bytes() {
        for mutation in [
            "missing",
            "claim",
            "candidate",
            "descriptor",
            "report",
            "bytes",
            "fake-png",
        ] {
            let fixture = root();
            let root = fixture.path();
            let pair = write_complete_pair(root);
            if mutation == "missing" {
                fs::remove_file(&pair[0].review_path).unwrap();
            } else if mutation == "bytes" {
                fs::OpenOptions::new()
                    .append(true)
                    .open(&pair[0].visual_path)
                    .unwrap()
                    .write_all(b"changed")
                    .unwrap();
            } else if mutation == "fake-png" {
                let fake = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 1, 2];
                fs::write(&pair[0].visual_path, fake).unwrap();
                let mut review: serde_json::Value =
                    serde_json::from_slice(&fs::read(&pair[0].review_path).unwrap()).unwrap();
                review["visualEvidence"]["sha256"] = serde_json::json!(hash(&fake));
                review["visualEvidence"]["lengthBytes"] = serde_json::json!(fake.len());
                fs::write(&pair[0].review_path, serde_json::to_vec(&review).unwrap()).unwrap();
            } else {
                let mut review: serde_json::Value =
                    serde_json::from_slice(&fs::read(&pair[0].review_path).unwrap()).unwrap();
                match mutation {
                    "claim" => {
                        review["confirmsRepresentativeSceneDepth"] = serde_json::json!(false)
                    }
                    "candidate" => review["candidateId"] = serde_json::json!("dsv-9999"),
                    "descriptor" => review["descriptor"]["width"] = serde_json::json!(1280),
                    "report" => {
                        review["report"]["sha256"] =
                            serde_json::json!(format!("sha256:{}", "f".repeat(64)))
                    }
                    _ => unreachable!(),
                }
                fs::write(&pair[0].review_path, serde_json::to_vec(&review).unwrap()).unwrap();
            }
            let output = root.join(DSV_SELECTION_ATTESTATION_FILE_NAME);
            assert!(
                materialize_fixture(root, &pair, &output).is_err(),
                "{mutation}"
            );
            assert!(!output.exists());
        }
    }

    #[cfg(feature = "evidence-authoring")]
    #[test]
    fn materializer_rejects_oversized_visual_and_output_escape() {
        let fixture = root();
        let root = fixture.path();
        let pair = write_complete_pair(root);
        File::options()
            .write(true)
            .open(&pair[0].visual_path)
            .unwrap()
            .set_len(MAX_DSV_SELECTION_VISUAL_EVIDENCE_BYTES + 1)
            .unwrap();
        assert!(
            materialize_fixture(root, &pair, &root.join(DSV_SELECTION_ATTESTATION_FILE_NAME))
                .is_err()
        );

        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path();
        let pair = write_complete_pair(root);
        File::options()
            .write(true)
            .open(&pair[0].receipt_path)
            .unwrap()
            .set_len(MAX_DSV_SELECTION_RECEIPT_BYTES + 1)
            .unwrap();
        assert!(
            materialize_fixture(root, &pair, &root.join(DSV_SELECTION_ATTESTATION_FILE_NAME))
                .is_err()
        );

        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path();
        let pair = write_complete_pair(root);
        let outside = tempfile::tempdir().unwrap();
        let output = outside.path().join(DSV_SELECTION_ATTESTATION_FILE_NAME);
        assert!(materialize_fixture(root, &pair, &output).is_err());
        assert!(!output.exists());
    }
}

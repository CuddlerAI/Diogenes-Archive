//! Fail-closed authoring contract for a local raw-depth/RGB smoke.
//!
//! This is deliberately **not** [`crate::DepthProfile`]. Discovery can prove an exact D3D12
//! resource identity before it proves projection, metric scale, render-camera pose, E-geo, or
//! performance. Turning that partial evidence into a production profile would fabricate the
//! missing fields. This module preserves the useful part instead: a sealed discovery report may
//! author one exact selector for a permanently non-trainable, non-deliverable local smoke.

use serde::{Deserialize, Serialize};

pub const RAW_DEPTH_DEMO_PROFILE_SCHEMA_V1: &str = "dm-local-raw-depth-demo-profile/v1";
pub const D3D12_DISCOVERY_REPORT_SCHEMA_V1: &str = "dm-d3d12-discovery-observation/v1";
pub const DISCOVERY_SEALED_SCHEMA_V1: &str = "dm-cp2077-discovery-report-sealed/v1";
pub const CLIENT_MANAGED_DISCOVERY_PROVENANCE: &str = "client_managed_discovery";
pub const D3D12_DISCOVERY_REPORT_FILE: &str = "d3d12-observation.json";
pub const CP2077_GAME_ID: &str = "cyberpunk-2077-steam";
pub const CP2077_BUILD_20383525_EXE_SHA256: &str =
    "sha256:a7de82945c03e041fc7339fcf9066224d98db2f5d80fea50f7947bb350a60991";
pub const ISSUE735_LOCAL_CONTRACT: &str = "issue735_local";
pub const PR405_LOCAL_CONTRACT: &str = "pr405_local";
pub const ISSUE735_LOCAL_DURATION_MS: u64 = 64_000;
pub const PR405_LOCAL_DURATION_MS: u64 = 30_000;

const DXGI_FORMAT_R32_TYPELESS: i32 = 39;
const DXGI_FORMAT_D32_FLOAT: i32 = 40;
const DXGI_FORMAT_R32G8X24_TYPELESS: i32 = 19;
const DXGI_FORMAT_D32_FLOAT_S8X24_UINT: i32 = 20;
const D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES: u32 = u32::MAX;
const D3D12_RESOURCE_STATE_DEPTH_WRITE: i32 = 0x10;
const CP2077_COMPOSED_DEPTH_READ: i32 = 0xe0;
const CP2077_DEPTH_READ: i32 = 0x40;
const MIN_PROVEN_INTERVALS: u64 = 2;
const D32S8_MIN_BINDS_PER_CLEAR: u64 = 16;
// Two independent exact-build 1920x1080 discovery launches observed the selected scene target
// across 20 and 36 command lists respectively.  Keep a large, measured breadth floor without
// encoding the first launch's incidental pool size as a semantic identity requirement.
// Fresh exact-build CP2077 launches repeatedly use a twelve-list pool for the
// native scene target. Pool width is not itself a semantic safety boundary;
// the stronger one-queue, every-clear/every-execute/every-Present and >=16x
// bind-breadth proofs below remain mandatory.
// CP2077's native 1080p scene depth pool is allowed to reuse a bounded set of
// command lists across a long run.  Eight independent list identities, in
// addition to the exact per-Present clear/copy/queue proof below, separates it
// from the one-list auxiliary targets without baking in a scheduler-dependent
// pool size (the exact 2.31 build currently presents eleven).
// CP2077 2.31 reuses a stable three-list render pool for the native 1920x1080
// D32S8 scene target. List breadth is only a scene-disambiguation signal; the
// safety proof remains the exact per-Present clear/copy/execute transition
// chain, unique DIRECT queue, and the bind-per-clear ratio below.
// CP2077 2.31 reuses a stable two-list scene pool on the final 1920x1080
// D32S8 generation.  Keep the breadth gate above a single incidental list;
// the exact per-Present clear/execute, bind-ratio, transition and single-queue
// proofs below remain mandatory.
const D32S8_MIN_DISTINCT_COMMAND_LISTS: usize = 2;
const D32S8_MAX_PREARM_DOUBLE_CLEAR_INTERVALS: u64 = 1;
const D32S8_MAX_PREARM_UNPAIRED_CLEARS: u64 = 1;
const DEFAULT_READBACK_SLOTS: usize = 4;

/// Hash callback kept injectable so the platform-neutral default probe build does not gain a
/// SHA-256 implementation solely for an offline authoring operation.
pub type Sha256Hex = fn(&[u8]) -> String;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawDepthDemoSelector {
    pub candidate_id: String,
    pub width: u32,
    pub height: u32,
    pub sample_count: u32,
    pub resource_format: i32,
    pub dsv_format: i32,
    pub readback_slots: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RawDepthFormatFamily {
    R32D32,
    R32G8X24D32S8,
}

/// An exact D3D12 selector that is honest about everything it does not establish.
///
/// The fixed false/true fields are validated again by [`RawDepthDemoProfile::validate`]. A caller
/// must never use this as catalog `depth_profile` material and the production parser cannot parse
/// it as one because both its schema and shape are different.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawDepthDemoProfile {
    pub schema: String,
    pub intent: String,
    pub game_id: String,
    pub executable_sha256: String,
    pub process_launch_id: String,
    pub resident_probe_sha256: String,
    pub source_d3d12_report_sha256: String,
    pub source_sealed_marker_sha256: String,
    pub discovery_provenance_mode: String,
    #[serde(default = "default_capture_contract")]
    pub capture_contract: String,
    #[serde(default = "default_duration_ms")]
    pub duration_ms: u64,
    pub selector: RawDepthDemoSelector,
    pub non_trainable: bool,
    pub production_certified: bool,
    pub upload_allowed: bool,
    pub deliverable: bool,
    pub recording_ready: bool,
    pub projection_resolved: bool,
    pub metric_scale_resolved: bool,
    pub camera_pose_resolved: bool,
    pub rgb_depth_egeo_passed: bool,
    pub performance_certified: bool,
    pub blockers: Vec<String>,
}

impl RawDepthDemoProfile {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema != RAW_DEPTH_DEMO_PROFILE_SCHEMA_V1 {
            return Err("unsupported raw depth demo profile schema".into());
        }
        if self.intent != "local_raw_depth_rgb_alignment_smoke"
            || self.game_id != CP2077_GAME_ID
            || self.executable_sha256 != CP2077_BUILD_20383525_EXE_SHA256
        {
            return Err("raw depth demo identity is not the pinned CP2077 local D7 build".into());
        }
        if !is_launch_id(&self.process_launch_id)
            || !is_sha256(&self.resident_probe_sha256)
            || !is_sha256(&self.source_d3d12_report_sha256)
            || !is_sha256(&self.source_sealed_marker_sha256)
            || !matches!(
                self.discovery_provenance_mode.as_str(),
                CLIENT_MANAGED_DISCOVERY_PROVENANCE
                    | crate::local_discovery_seal::MANUAL_LOCAL_SMOKE_PROVENANCE
            )
        {
            return Err("raw depth demo provenance is malformed".into());
        }
        validate_capture_contract(&self.capture_contract, self.duration_ms)?;
        let format_family =
            raw_depth_format_family(self.selector.resource_format, self.selector.dsv_format);
        if self.selector.candidate_id.trim().is_empty()
            || self.selector.width == 0
            || self.selector.height == 0
            || self.selector.sample_count != 1
            || format_family.is_none()
            || format_family == Some(RawDepthFormatFamily::R32G8X24D32S8)
                && (self.selector.width != 1920 || self.selector.height != 1080)
            || self.selector.readback_slots < DEFAULT_READBACK_SLOTS
        {
            return Err(
                "raw depth demo selector is not an admitted exact single-sample CP2077 depth resource"
                    .into(),
            );
        }
        if !self.non_trainable
            || self.production_certified
            || self.upload_allowed
            || self.deliverable
            || self.recording_ready
            || self.projection_resolved
            || self.metric_scale_resolved
            || self.camera_pose_resolved
            || self.rgb_depth_egeo_passed
            || self.performance_certified
            || self.blockers.is_empty()
        {
            return Err("raw depth demo safety/readiness ceiling was weakened".into());
        }
        Ok(())
    }
}

fn default_capture_contract() -> String {
    ISSUE735_LOCAL_CONTRACT.into()
}

const fn default_duration_ms() -> u64 {
    ISSUE735_LOCAL_DURATION_MS
}

pub fn validate_capture_contract(contract: &str, duration_ms: u64) -> Result<(), String> {
    if matches!(
        (contract, duration_ms),
        (ISSUE735_LOCAL_CONTRACT, ISSUE735_LOCAL_DURATION_MS)
            | (PR405_LOCAL_CONTRACT, PR405_LOCAL_DURATION_MS)
    ) {
        Ok(())
    } else {
        Err("raw depth demo capture contract/duration is unsupported".into())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RawDepthDemoAuthoringError {
    MalformedSealedMarker(String),
    MalformedD3d12Report(String),
    UnsealedEvidence(&'static str),
    WrongExecutable(String),
    ReportHashMismatch {
        expected: String,
        actual: String,
    },
    LaunchIdentityMismatch,
    CandidateNotUnique {
        candidate_id: String,
        matches: usize,
    },
    CandidateNotCurrent,
    UnsupportedCandidateFormat {
        resource_format: i32,
        dsv_format: i32,
    },
    UnsupportedCandidateGeometry,
    CandidateNotCaptureProven {
        eligible_clears: u64,
        same_list_eligible_clears: u64,
        present_intervals: u64,
        same_list_present_intervals: u64,
        direct_queues: usize,
    },
    CandidateSceneProofMissing {
        clear_calls: u64,
        bind_calls: u64,
        distinct_command_lists: usize,
        reason: &'static str,
    },
}

impl core::fmt::Display for RawDepthDemoAuthoringError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "raw depth demo profile refused: {self:?}")
    }
}

impl std::error::Error for RawDepthDemoAuthoringError {}

#[derive(Deserialize)]
struct SealedMarker {
    schema: String,
    #[serde(default)]
    provenance_mode: Option<String>,
    #[serde(default)]
    client_managed: Option<bool>,
    discovery_only: bool,
    recording_ready: bool,
    process_launch_id: String,
    executable_sha256: String,
    resident_probe_sha256: String,
    #[serde(default)]
    launcher_stdout_sha256: Option<String>,
    #[serde(default)]
    discovery_config_sha256: Option<String>,
    reports: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    address_audit: Option<SealedAddressAudit>,
}

#[derive(Deserialize)]
struct SealedAddressAudit {
    absolute_process_addresses_recorded: bool,
    d3d12_candidate_ids_opaque: bool,
    module_relative_rvas_allowed: bool,
}

#[derive(Deserialize)]
struct D3d12Report {
    schema: String,
    process_launch_id: String,
    recording_ready: bool,
    observation: D3d12Observation,
}

#[derive(Deserialize)]
struct D3d12Observation {
    present_calls: u64,
    depth_candidates: Vec<D3d12Candidate>,
}

#[derive(Deserialize)]
struct D3d12Candidate {
    candidate_id: String,
    width: u64,
    height: u32,
    sample_count: u32,
    resource_format: i32,
    dsv_format: i32,
    clear_calls: u64,
    eligible_clear_calls: u64,
    #[serde(default)]
    same_list_capture_eligible_clear_calls: u64,
    present_intervals_with_clear: u64,
    present_intervals_with_one_clear: u64,
    present_intervals_with_multiple_clears: u64,
    max_clears_per_present_interval: u64,
    #[serde(default)]
    present_intervals_with_same_list_capture_eligible_clear: u64,
    execute_calls: u64,
    bind_calls: u64,
    clear_while_bound_calls: u64,
    clear_parameters: Vec<D3d12ClearParameter>,
    raw_states_before_clear: Vec<D3d12RawState>,
    transition_pairs: Vec<D3d12TransitionPair>,
    distinct_command_lists: usize,
    distinct_direct_queues: usize,
    current: bool,
}

#[derive(Deserialize, PartialEq, Eq)]
struct D3d12ClearParameter {
    clear_flags: u32,
    depth_value_bits: u32,
    stencil: u8,
    rect_count: u32,
    calls: u64,
}

#[derive(Deserialize, PartialEq, Eq)]
struct D3d12RawState {
    state: Option<i32>,
    calls: u64,
}

#[derive(Deserialize)]
struct D3d12TransitionPair {
    state_before: i32,
    state_after: i32,
    subresource: u32,
    split: bool,
    calls: u64,
}

/// Authors the narrow local-attempt selector from a client-sealed discovery run.
///
/// Absence of projection/camera evidence is not papered over. The result is permanently capped at
/// raw-depth/RGB alignment smoke status. The current D3D12 readback implementation is additionally
/// pinned to single-sample `R32_TYPELESS` + `D32_FLOAT` or the exact CP2077 native 1920x1080
/// `R32G8X24_TYPELESS` + `D32_FLOAT_S8X24_UINT` scene candidate. Other formats are refused.
pub fn author_cp2077_raw_depth_demo(
    d3d12_report_bytes: &[u8],
    sealed_marker_bytes: &[u8],
    candidate_id: &str,
    sha256_hex: Sha256Hex,
) -> Result<RawDepthDemoProfile, RawDepthDemoAuthoringError> {
    let marker: SealedMarker = serde_json::from_slice(sealed_marker_bytes)
        .map_err(|error| RawDepthDemoAuthoringError::MalformedSealedMarker(error.to_string()))?;
    let discovery_provenance_mode =
        if marker.schema == DISCOVERY_SEALED_SCHEMA_V1 {
            if marker.provenance_mode.is_some()
                || marker.client_managed.is_some()
                || marker.launcher_stdout_sha256.is_some()
                || marker.discovery_config_sha256.is_some()
                || marker.address_audit.is_some()
            {
                return Err(RawDepthDemoAuthoringError::UnsealedEvidence(
                    "client-managed marker contains manual-launch provenance fields",
                ));
            }
            CLIENT_MANAGED_DISCOVERY_PROVENANCE
        } else if marker.schema == crate::local_discovery_seal::LOCAL_DISCOVERY_SEALED_SCHEMA_V1 {
            let local: crate::local_discovery_seal::LocalDiscoverySealedMarker =
                serde_json::from_slice(sealed_marker_bytes).map_err(|error| {
                    RawDepthDemoAuthoringError::MalformedSealedMarker(error.to_string())
                })?;
            local
                .validate()
                .map_err(RawDepthDemoAuthoringError::MalformedSealedMarker)?;
            let audit = marker.address_audit.as_ref().ok_or(
                RawDepthDemoAuthoringError::UnsealedEvidence(
                    "manual local marker has no address audit",
                ),
            )?;
            if marker.provenance_mode.as_deref()
                != Some(crate::local_discovery_seal::MANUAL_LOCAL_SMOKE_PROVENANCE)
                || marker.client_managed != Some(false)
                || !marker
                    .launcher_stdout_sha256
                    .as_deref()
                    .is_some_and(is_sha256)
                || !marker
                    .discovery_config_sha256
                    .as_deref()
                    .is_some_and(is_sha256)
                || audit.absolute_process_addresses_recorded
                || !audit.d3d12_candidate_ids_opaque
                || !audit.module_relative_rvas_allowed
            {
                return Err(RawDepthDemoAuthoringError::UnsealedEvidence(
                    "manual local marker provenance/address audit is malformed",
                ));
            }
            crate::local_discovery_seal::MANUAL_LOCAL_SMOKE_PROVENANCE
        } else {
            return Err(RawDepthDemoAuthoringError::UnsealedEvidence(
                "marker is not a supported sealed discovery contract",
            ));
        };
    if !marker.discovery_only || marker.recording_ready {
        return Err(RawDepthDemoAuthoringError::UnsealedEvidence(
            "marker is not discovery-only and recording_ready=false",
        ));
    }
    if marker.executable_sha256 != CP2077_BUILD_20383525_EXE_SHA256 {
        return Err(RawDepthDemoAuthoringError::WrongExecutable(
            marker.executable_sha256,
        ));
    }
    if !is_launch_id(&marker.process_launch_id) || !is_sha256(&marker.resident_probe_sha256) {
        return Err(RawDepthDemoAuthoringError::UnsealedEvidence(
            "marker provenance is malformed",
        ));
    }
    let expected_report_hash = marker.reports.get(D3D12_DISCOVERY_REPORT_FILE).ok_or(
        RawDepthDemoAuthoringError::UnsealedEvidence("marker does not bind d3d12-observation.json"),
    )?;
    let actual_report_hash = sha256_hex(d3d12_report_bytes);
    let sealed_marker_hash = sha256_hex(sealed_marker_bytes);
    if expected_report_hash != &actual_report_hash {
        return Err(RawDepthDemoAuthoringError::ReportHashMismatch {
            expected: expected_report_hash.clone(),
            actual: actual_report_hash,
        });
    }

    let report: D3d12Report = serde_json::from_slice(d3d12_report_bytes)
        .map_err(|error| RawDepthDemoAuthoringError::MalformedD3d12Report(error.to_string()))?;
    if report.schema != D3D12_DISCOVERY_REPORT_SCHEMA_V1 || report.recording_ready {
        return Err(RawDepthDemoAuthoringError::UnsealedEvidence(
            "D3D12 report is not discovery-only v1",
        ));
    }
    if report.process_launch_id != marker.process_launch_id {
        return Err(RawDepthDemoAuthoringError::LaunchIdentityMismatch);
    }
    if report.observation.present_calls < MIN_PROVEN_INTERVALS {
        return Err(RawDepthDemoAuthoringError::UnsealedEvidence(
            "D3D12 report has too few Presents",
        ));
    }
    let matches: Vec<_> = report
        .observation
        .depth_candidates
        .iter()
        .filter(|candidate| candidate.candidate_id == candidate_id)
        .collect();
    if matches.len() != 1 {
        return Err(RawDepthDemoAuthoringError::CandidateNotUnique {
            candidate_id: candidate_id.to_owned(),
            matches: matches.len(),
        });
    }
    let candidate = matches[0];
    if !candidate.current {
        return Err(RawDepthDemoAuthoringError::CandidateNotCurrent);
    }
    let Some(format_family) =
        raw_depth_format_family(candidate.resource_format, candidate.dsv_format)
    else {
        return Err(RawDepthDemoAuthoringError::UnsupportedCandidateFormat {
            resource_format: candidate.resource_format,
            dsv_format: candidate.dsv_format,
        });
    };
    let width = u32::try_from(candidate.width)
        .map_err(|_| RawDepthDemoAuthoringError::UnsupportedCandidateGeometry)?;
    if width == 0
        || candidate.height == 0
        || candidate.sample_count != 1
        || format_family == RawDepthFormatFamily::R32G8X24D32S8
            && (width != 1920 || candidate.height != 1080)
    {
        return Err(RawDepthDemoAuthoringError::UnsupportedCandidateGeometry);
    }
    if !candidate_has_capture_proof(candidate) {
        return Err(RawDepthDemoAuthoringError::CandidateNotCaptureProven {
            eligible_clears: candidate.eligible_clear_calls,
            same_list_eligible_clears: candidate.same_list_capture_eligible_clear_calls,
            present_intervals: candidate.present_intervals_with_clear,
            same_list_present_intervals: candidate
                .present_intervals_with_same_list_capture_eligible_clear,
            direct_queues: candidate.distinct_direct_queues,
        });
    }
    validate_candidate_scene_proof(candidate, format_family)?;
    validate_d32s8_scene_breadth(
        candidate,
        &report.observation.depth_candidates,
        format_family,
    )?;

    let profile = RawDepthDemoProfile {
        schema: RAW_DEPTH_DEMO_PROFILE_SCHEMA_V1.into(),
        intent: "local_raw_depth_rgb_alignment_smoke".into(),
        game_id: CP2077_GAME_ID.into(),
        executable_sha256: marker.executable_sha256,
        process_launch_id: marker.process_launch_id,
        resident_probe_sha256: marker.resident_probe_sha256,
        source_d3d12_report_sha256: actual_report_hash,
        source_sealed_marker_sha256: sealed_marker_hash,
        discovery_provenance_mode: discovery_provenance_mode.into(),
        capture_contract: ISSUE735_LOCAL_CONTRACT.into(),
        duration_ms: ISSUE735_LOCAL_DURATION_MS,
        selector: RawDepthDemoSelector {
            candidate_id: candidate.candidate_id.clone(),
            width,
            height: candidate.height,
            sample_count: candidate.sample_count,
            resource_format: candidate.resource_format,
            dsv_format: candidate.dsv_format,
            readback_slots: DEFAULT_READBACK_SLOTS,
        },
        non_trainable: true,
        production_certified: false,
        upload_allowed: false,
        deliverable: false,
        recording_ready: false,
        projection_resolved: false,
        metric_scale_resolved: false,
        camera_pose_resolved: false,
        rgb_depth_egeo_passed: false,
        performance_certified: false,
        blockers: vec![
            "projection coefficients are unresolved".into(),
            "metric scale is unresolved".into(),
            "render-camera/player pose is unresolved".into(),
            "RGB/depth E-geo is not validated".into(),
            "performance certification is missing".into(),
        ],
    };
    profile
        .validate()
        .map_err(RawDepthDemoAuthoringError::MalformedSealedMarker)?;
    Ok(profile)
}

fn raw_depth_format_family(resource_format: i32, dsv_format: i32) -> Option<RawDepthFormatFamily> {
    match (resource_format, dsv_format) {
        (DXGI_FORMAT_R32_TYPELESS, DXGI_FORMAT_D32_FLOAT) => Some(RawDepthFormatFamily::R32D32),
        (DXGI_FORMAT_R32G8X24_TYPELESS, DXGI_FORMAT_D32_FLOAT_S8X24_UINT) => {
            Some(RawDepthFormatFamily::R32G8X24D32S8)
        }
        _ => None,
    }
}

fn candidate_has_capture_proof(candidate: &D3d12Candidate) -> bool {
    candidate.eligible_clear_calls >= MIN_PROVEN_INTERVALS
        && candidate.same_list_capture_eligible_clear_calls >= MIN_PROVEN_INTERVALS
        && candidate.present_intervals_with_clear >= MIN_PROVEN_INTERVALS
        && candidate.present_intervals_with_same_list_capture_eligible_clear >= MIN_PROVEN_INTERVALS
        && candidate.distinct_direct_queues == 1
}

fn d32s8_has_bind_breadth(candidate: &D3d12Candidate) -> bool {
    candidate.bind_calls
        >= candidate
            .clear_calls
            .saturating_mul(D32S8_MIN_BINDS_PER_CLEAR)
        && candidate.distinct_command_lists >= D32S8_MIN_DISTINCT_COMMAND_LISTS
}

fn validate_d32s8_scene_breadth(
    candidate: &D3d12Candidate,
    all_candidates: &[D3d12Candidate],
    family: RawDepthFormatFamily,
) -> Result<(), RawDepthDemoAuthoringError> {
    if family != RawDepthFormatFamily::R32G8X24D32S8 {
        return Ok(());
    }
    if candidate.bind_calls
        < candidate
            .clear_calls
            .saturating_mul(D32S8_MIN_BINDS_PER_CLEAR)
    {
        return Err(scene_breadth_error(
            candidate,
            "D32S8 candidate lacks main-scene bind breadth",
        ));
    }
    if candidate.distinct_command_lists >= D32S8_MIN_DISTINCT_COMMAND_LISTS {
        return Ok(());
    }

    let strict_predecessors = all_candidates
        .iter()
        .filter(|other| {
            other.candidate_id != candidate.candidate_id
                && !other.current
                && other.width == candidate.width
                && other.height == candidate.height
                && other.sample_count == candidate.sample_count
                && other.resource_format == candidate.resource_format
                && other.dsv_format == candidate.dsv_format
                && candidate_has_capture_proof(other)
                && validate_candidate_scene_proof(other, family).is_ok()
                && d32s8_has_bind_breadth(other)
        })
        .count();
    if strict_predecessors != 1 {
        return Err(scene_breadth_error(
            candidate,
            "D32S8 current generation lacks one unique strict broad predecessor",
        ));
    }
    Ok(())
}

fn scene_breadth_error(
    candidate: &D3d12Candidate,
    reason: &'static str,
) -> RawDepthDemoAuthoringError {
    RawDepthDemoAuthoringError::CandidateSceneProofMissing {
        clear_calls: candidate.clear_calls,
        bind_calls: candidate.bind_calls,
        distinct_command_lists: candidate.distinct_command_lists,
        reason,
    }
}

fn validate_candidate_scene_proof(
    candidate: &D3d12Candidate,
    family: RawDepthFormatFamily,
) -> Result<(), RawDepthDemoAuthoringError> {
    let expected_clear_flags = match family {
        RawDepthFormatFamily::R32D32 => 1,
        RawDepthFormatFamily::R32G8X24D32S8 => 3,
    };
    let strict_clear = candidate.clear_parameters.as_slice()
        == [D3d12ClearParameter {
            clear_flags: expected_clear_flags,
            depth_value_bits: 0,
            stencil: 0,
            rect_count: 0,
            calls: candidate.clear_calls,
        }];
    let strict_raw_state = candidate.raw_states_before_clear.as_slice()
        == [D3d12RawState {
            state: Some(D3D12_RESOURCE_STATE_DEPTH_WRITE),
            calls: candidate.clear_calls,
        }];
    let transitions_are_strict = candidate
        .transition_pairs
        .iter()
        .all(|pair| pair.subresource == D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES && !pair.split);
    let has_transition = |before, after| {
        candidate.transition_pairs.iter().any(|pair| {
            pair.state_before == before
                && pair.state_after == after
                && pair.subresource == D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES
                && !pair.split
                && pair.calls >= MIN_PROVEN_INTERVALS
        })
    };
    let exact_per_clear_proof = candidate.clear_calls >= MIN_PROVEN_INTERVALS
        && candidate.clear_calls == candidate.eligible_clear_calls
        && candidate.clear_calls == candidate.same_list_capture_eligible_clear_calls
        && candidate.clear_calls == candidate.execute_calls
        && candidate.clear_calls
            >= candidate.present_intervals_with_same_list_capture_eligible_clear
        && candidate.clear_while_bound_calls == 0;
    let exact_clear_cadence = match family {
        RawDepthFormatFamily::R32D32 => {
            candidate.clear_calls == candidate.present_intervals_with_clear
                && candidate.clear_calls == candidate.present_intervals_with_one_clear
                && candidate.clear_calls
                    == candidate.present_intervals_with_same_list_capture_eligible_clear
                && candidate.present_intervals_with_multiple_clears == 0
                && candidate.max_clears_per_present_interval == 1
        }
        RawDepthFormatFamily::R32G8X24D32S8 => {
            let double_clear_intervals = candidate.present_intervals_with_multiple_clears;
            let paired_clears = candidate
                .present_intervals_with_one_clear
                .saturating_add(double_clear_intervals.saturating_mul(2));
            double_clear_intervals <= D32S8_MAX_PREARM_DOUBLE_CLEAR_INTERVALS
                && candidate.max_clears_per_present_interval
                    == if double_clear_intervals == 0 { 1 } else { 2 }
                && candidate.present_intervals_with_clear
                    == candidate
                        .present_intervals_with_one_clear
                        .saturating_add(double_clear_intervals)
                // The discovery report may begin or end between the ResourceBarrier/Clear and
                // its Present callback. Permit exactly one such boundary clear before arming;
                // runtime capture freezes this cadence and still faults on any later drift.
                && candidate.clear_calls >= paired_clears
                && candidate.clear_calls.saturating_sub(paired_clears)
                    <= D32S8_MAX_PREARM_UNPAIRED_CLEARS
                && candidate.present_intervals_with_same_list_capture_eligible_clear
                    == candidate.present_intervals_with_clear
        }
    };
    if !strict_clear
        || !strict_raw_state
        || !transitions_are_strict
        || !has_transition(D3D12_RESOURCE_STATE_DEPTH_WRITE, CP2077_COMPOSED_DEPTH_READ)
        || !has_transition(CP2077_COMPOSED_DEPTH_READ, CP2077_DEPTH_READ)
        || !has_transition(CP2077_DEPTH_READ, D3D12_RESOURCE_STATE_DEPTH_WRITE)
        || !exact_per_clear_proof
        || !exact_clear_cadence
    {
        return Err(RawDepthDemoAuthoringError::CandidateSceneProofMissing {
            clear_calls: candidate.clear_calls,
            bind_calls: candidate.bind_calls,
            distinct_command_lists: candidate.distinct_command_lists,
            reason: "strict full-clear/transition/cadence proof is incomplete",
        });
    }
    Ok(())
}

fn is_sha256(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    })
}

fn is_launch_id(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_contracts_are_exact_and_legacy_profiles_default_to_64_seconds() {
        assert!(validate_capture_contract(ISSUE735_LOCAL_CONTRACT, 64_000).is_ok());
        assert!(validate_capture_contract(PR405_LOCAL_CONTRACT, 30_000).is_ok());
        assert!(validate_capture_contract(PR405_LOCAL_CONTRACT, 64_000).is_err());
        assert!(validate_capture_contract(ISSUE735_LOCAL_CONTRACT, 30_000).is_err());
    }

    const LAUNCH: &str = "0123456789abcdef0123456789abcdef";
    const PROBE: &str = "sha256:1111111111111111111111111111111111111111111111111111111111111111";

    fn fake_sha(bytes: &[u8]) -> String {
        format!("sha256:{:064x}", bytes.len())
    }

    fn report(candidate: serde_json::Value) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "schema": D3D12_DISCOVERY_REPORT_SCHEMA_V1,
            "process_launch_id": LAUNCH,
            "recording_ready": false,
            "observation": {
                "present_calls": 300,
                "depth_candidates": [candidate],
                "capture_safe": false,
                "blockers": ["an unrelated DSV used a combined state"]
            }
        }))
        .unwrap()
    }

    fn candidate() -> serde_json::Value {
        serde_json::json!({
            "candidate_id": "dsv-r31-g1-e1",
            "width": 1280,
            "height": 720,
            "sample_count": 1,
            "resource_format": DXGI_FORMAT_R32_TYPELESS,
            "dsv_format": DXGI_FORMAT_D32_FLOAT,
            "clear_calls": 250,
            "eligible_clear_calls": 250,
            "same_list_capture_eligible_clear_calls": 250,
            "execute_calls": 250,
            "present_intervals_with_clear": 250,
            "present_intervals_with_one_clear": 250,
            "present_intervals_with_multiple_clears": 0,
            "max_clears_per_present_interval": 1,
            "present_intervals_with_same_list_capture_eligible_clear": 250,
            "bind_calls": 250,
            "clear_while_bound_calls": 0,
            "clear_parameters": [{
                "clear_flags": 1,
                "depth_value_bits": 0,
                "stencil": 0,
                "rect_count": 0,
                "calls": 250
            }],
            "raw_states_before_clear": [{"state": 16, "calls": 250}],
            "transition_pairs": [
                {"state_before": 16, "state_after": 224, "subresource": 4294967295u32, "split": false, "calls": 250},
                {"state_before": 224, "state_after": 64, "subresource": 4294967295u32, "split": false, "calls": 250},
                {"state_before": 64, "state_after": 16, "subresource": 4294967295u32, "split": false, "calls": 249}
            ],
            "distinct_command_lists": 24,
            "distinct_direct_queues": 1,
            "current": true
        })
    }

    fn d32s8_candidate() -> serde_json::Value {
        let mut value = candidate();
        value["candidate_id"] = "dsv-r19-g1-e1".into();
        value["width"] = 1920.into();
        value["height"] = 1080.into();
        value["resource_format"] = DXGI_FORMAT_R32G8X24_TYPELESS.into();
        value["dsv_format"] = DXGI_FORMAT_D32_FLOAT_S8X24_UINT.into();
        value["bind_calls"] = 7_500.into();
        value["distinct_command_lists"] = 57.into();
        value["clear_parameters"][0]["clear_flags"] = 3.into();
        value
    }

    fn marker(report_bytes: &[u8]) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "schema": DISCOVERY_SEALED_SCHEMA_V1,
            "discovery_only": true,
            "recording_ready": false,
            "process_launch_id": LAUNCH,
            "process_id": 42,
            "executable_sha256": CP2077_BUILD_20383525_EXE_SHA256,
            "resident_probe_sha256": PROBE,
            "reports": {
                D3D12_DISCOVERY_REPORT_FILE: fake_sha(report_bytes),
                "cp2077-pose-runtime-observation.json": format!("sha256:{}", "2".repeat(64))
            }
        }))
        .unwrap()
    }

    fn local_marker(report_bytes: &[u8]) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "schema": crate::local_discovery_seal::LOCAL_DISCOVERY_SEALED_SCHEMA_V1,
            "provenance_mode": crate::local_discovery_seal::MANUAL_LOCAL_SMOKE_PROVENANCE,
            "client_managed": false,
            "discovery_only": true,
            "recording_ready": false,
            "process_launch_id": LAUNCH,
            "launched_process_id": 42,
            "executable_sha256": CP2077_BUILD_20383525_EXE_SHA256,
            "resident_probe_sha256": PROBE,
            "launcher_stdout_sha256": format!("sha256:{}", "3".repeat(64)),
            "discovery_config_sha256": format!("sha256:{}", "4".repeat(64)),
            "reports": {
                D3D12_DISCOVERY_REPORT_FILE: fake_sha(report_bytes),
                "cp2077-pose-runtime-observation.json": format!("sha256:{}", "2".repeat(64))
            },
            "address_audit": {
                "absolute_process_addresses_recorded": false,
                "d3d12_candidate_ids_opaque": true,
                "module_relative_rvas_allowed": true
            }
        }))
        .unwrap()
    }

    #[test]
    fn exact_d32_candidate_authors_only_a_non_ready_raw_demo() {
        let report = report(candidate());
        let profile =
            author_cp2077_raw_depth_demo(&report, &marker(&report), "dsv-r31-g1-e1", fake_sha)
                .unwrap();
        profile.validate().unwrap();
        assert!(profile.non_trainable);
        assert!(!profile.recording_ready);
        assert!(!profile.deliverable);
        assert!(!profile.camera_pose_resolved);
        assert_eq!(profile.selector.dsv_format, DXGI_FORMAT_D32_FLOAT);
        assert!(
            serde_json::from_slice::<crate::DepthProfile>(&serde_json::to_vec(&profile).unwrap())
                .is_err(),
            "attempt profile must never parse as a production DepthProfile"
        );
    }

    #[test]
    fn exact_cp2077_d32s8_scene_candidate_authors_only_a_non_ready_raw_demo() {
        let report = report(d32s8_candidate());
        let profile =
            author_cp2077_raw_depth_demo(&report, &marker(&report), "dsv-r19-g1-e1", fake_sha)
                .unwrap();
        profile.validate().unwrap();
        assert_eq!(profile.selector.width, 1920);
        assert_eq!(profile.selector.height, 1080);
        assert_eq!(
            profile.selector.resource_format,
            DXGI_FORMAT_R32G8X24_TYPELESS
        );
        assert_eq!(
            profile.selector.dsv_format,
            DXGI_FORMAT_D32_FLOAT_S8X24_UINT
        );
        assert!(profile.non_trainable);
        assert!(!profile.production_certified);
        assert!(!profile.recording_ready);
    }

    #[test]
    fn d32s8_requires_exact_geometry_and_main_scene_bind_breadth() {
        let mut wrong_geometry = d32s8_candidate();
        wrong_geometry["width"] = 1280.into();
        let report_bytes = report(wrong_geometry);
        assert!(matches!(
            author_cp2077_raw_depth_demo(
                &report_bytes,
                &marker(&report_bytes),
                "dsv-r19-g1-e1",
                fake_sha
            ),
            Err(RawDepthDemoAuthoringError::UnsupportedCandidateGeometry)
        ));

        let mut weak_bind = d32s8_candidate();
        weak_bind["bind_calls"] = 3_999.into();
        let report_bytes = report(weak_bind);
        assert!(matches!(
            author_cp2077_raw_depth_demo(
                &report_bytes,
                &marker(&report_bytes),
                "dsv-r19-g1-e1",
                fake_sha
            ),
            Err(RawDepthDemoAuthoringError::CandidateSceneProofMissing {
                reason: "D32S8 candidate lacks main-scene bind breadth",
                ..
            })
        ));

        let mut too_narrow = d32s8_candidate();
        too_narrow["distinct_command_lists"] = (D32S8_MIN_DISTINCT_COMMAND_LISTS - 1).into();
        let report_bytes = report(too_narrow);
        assert!(matches!(
            author_cp2077_raw_depth_demo(
                &report_bytes,
                &marker(&report_bytes),
                "dsv-r19-g1-e1",
                fake_sha
            ),
            Err(RawDepthDemoAuthoringError::CandidateSceneProofMissing {
                reason: "D32S8 current generation lacks one unique strict broad predecessor",
                ..
            })
        ));
    }

    #[test]
    fn d32s8_current_generation_can_use_one_unique_strict_broad_predecessor() {
        let mut current = d32s8_candidate();
        current["candidate_id"] = "dsv-r48-g1-e1".into();
        current["distinct_command_lists"] = 1.into();
        let mut predecessor = d32s8_candidate();
        predecessor["current"] = false.into();
        let report_bytes = serde_json::to_vec(&serde_json::json!({
            "schema": D3D12_DISCOVERY_REPORT_SCHEMA_V1,
            "process_launch_id": LAUNCH,
            "recording_ready": false,
            "observation": {
                "present_calls": 300,
                "depth_candidates": [predecessor, current]
            }
        }))
        .unwrap();
        let profile = author_cp2077_raw_depth_demo(
            &report_bytes,
            &marker(&report_bytes),
            "dsv-r48-g1-e1",
            fake_sha,
        )
        .unwrap();
        assert_eq!(profile.selector.candidate_id, "dsv-r48-g1-e1");
    }

    #[test]
    fn d32s8_generation_continuity_rejects_zero_or_multiple_strict_predecessors() {
        let mut current = d32s8_candidate();
        current["candidate_id"] = "dsv-r48-g1-e1".into();
        current["distinct_command_lists"] = 1.into();
        let no_predecessor = report(current.clone());
        assert!(matches!(
            author_cp2077_raw_depth_demo(
                &no_predecessor,
                &marker(&no_predecessor),
                "dsv-r48-g1-e1",
                fake_sha,
            ),
            Err(RawDepthDemoAuthoringError::CandidateSceneProofMissing {
                reason: "D32S8 current generation lacks one unique strict broad predecessor",
                ..
            })
        ));

        let mut first = d32s8_candidate();
        first["current"] = false.into();
        let mut second = d32s8_candidate();
        second["candidate_id"] = "dsv-r41-g1-e1".into();
        second["current"] = false.into();
        let ambiguous = serde_json::to_vec(&serde_json::json!({
            "schema": D3D12_DISCOVERY_REPORT_SCHEMA_V1,
            "process_launch_id": LAUNCH,
            "recording_ready": false,
            "observation": {
                "present_calls": 300,
                "depth_candidates": [first, second, current]
            }
        }))
        .unwrap();
        assert!(matches!(
            author_cp2077_raw_depth_demo(
                &ambiguous,
                &marker(&ambiguous),
                "dsv-r48-g1-e1",
                fake_sha,
            ),
            Err(RawDepthDemoAuthoringError::CandidateSceneProofMissing {
                reason: "D32S8 current generation lacks one unique strict broad predecessor",
                ..
            })
        ));
    }

    #[test]
    fn d32s8_allows_one_audited_prearm_double_clear_but_not_two() {
        let mut one_loading_boundary = d32s8_candidate();
        one_loading_boundary["clear_calls"] = 251.into();
        one_loading_boundary["eligible_clear_calls"] = 251.into();
        one_loading_boundary["same_list_capture_eligible_clear_calls"] = 251.into();
        one_loading_boundary["execute_calls"] = 251.into();
        one_loading_boundary["present_intervals_with_one_clear"] = 249.into();
        one_loading_boundary["present_intervals_with_multiple_clears"] = 1.into();
        one_loading_boundary["max_clears_per_present_interval"] = 2.into();
        one_loading_boundary["clear_parameters"][0]["calls"] = 251.into();
        one_loading_boundary["raw_states_before_clear"][0]["calls"] = 251.into();
        one_loading_boundary["bind_calls"] = 7_530.into();
        let report_bytes = report(one_loading_boundary.clone());
        assert!(
            author_cp2077_raw_depth_demo(
                &report_bytes,
                &marker(&report_bytes),
                "dsv-r19-g1-e1",
                fake_sha,
            )
            .is_ok()
        );

        one_loading_boundary["clear_calls"] = 252.into();
        one_loading_boundary["eligible_clear_calls"] = 252.into();
        one_loading_boundary["same_list_capture_eligible_clear_calls"] = 252.into();
        one_loading_boundary["execute_calls"] = 252.into();
        one_loading_boundary["present_intervals_with_one_clear"] = 248.into();
        one_loading_boundary["present_intervals_with_multiple_clears"] = 2.into();
        one_loading_boundary["clear_parameters"][0]["calls"] = 252.into();
        one_loading_boundary["raw_states_before_clear"][0]["calls"] = 252.into();
        let report_bytes = report(one_loading_boundary);
        assert!(matches!(
            author_cp2077_raw_depth_demo(
                &report_bytes,
                &marker(&report_bytes),
                "dsv-r19-g1-e1",
                fake_sha,
            ),
            Err(RawDepthDemoAuthoringError::CandidateSceneProofMissing { .. })
        ));
    }

    #[test]
    fn both_formats_require_strict_clear_transition_and_single_queue_proof() {
        for (id, mut value) in [
            ("dsv-r31-g1-e1", candidate()),
            ("dsv-r19-g1-e1", d32s8_candidate()),
        ] {
            value["transition_pairs"][0]["split"] = true.into();
            let report = report(value);
            assert!(matches!(
                author_cp2077_raw_depth_demo(&report, &marker(&report), id, fake_sha),
                Err(RawDepthDemoAuthoringError::CandidateSceneProofMissing { .. })
            ));
        }
    }

    #[test]
    fn manually_launched_seal_preserves_its_non_client_provenance() {
        let report = report(candidate());
        let sealed = local_marker(&report);
        let profile =
            author_cp2077_raw_depth_demo(&report, &sealed, "dsv-r31-g1-e1", fake_sha).unwrap();
        assert_eq!(
            profile.discovery_provenance_mode,
            crate::local_discovery_seal::MANUAL_LOCAL_SMOKE_PROVENANCE
        );
        assert_eq!(profile.source_sealed_marker_sha256, fake_sha(&sealed));
        assert!(!profile.recording_ready);
    }

    #[test]
    fn a_report_without_same_list_capture_proof_is_refused() {
        let mut value = candidate();
        value
            .as_object_mut()
            .unwrap()
            .remove("same_list_capture_eligible_clear_calls");
        let report = report(value);
        let error =
            author_cp2077_raw_depth_demo(&report, &marker(&report), "dsv-r31-g1-e1", fake_sha)
                .unwrap_err();
        assert!(matches!(
            error,
            RawDepthDemoAuthoringError::CandidateNotCaptureProven {
                same_list_eligible_clears: 0,
                ..
            }
        ));
    }

    #[test]
    fn d16_is_not_promoted_even_when_it_has_many_eligible_clears() {
        let mut value = candidate();
        value["resource_format"] = 53.into();
        value["dsv_format"] = 55.into();
        let report = report(value);
        let error =
            author_cp2077_raw_depth_demo(&report, &marker(&report), "dsv-r31-g1-e1", fake_sha)
                .unwrap_err();
        assert!(matches!(
            error,
            RawDepthDemoAuthoringError::UnsupportedCandidateFormat { .. }
        ));
    }

    #[test]
    fn report_bytes_must_match_the_client_sealed_hash() {
        let report = report(candidate());
        let mut marker: serde_json::Value = serde_json::from_slice(&marker(&report)).unwrap();
        marker["reports"][D3D12_DISCOVERY_REPORT_FILE] =
            format!("sha256:{}", "f".repeat(64)).into();
        let error = author_cp2077_raw_depth_demo(
            &report,
            &serde_json::to_vec(&marker).unwrap(),
            "dsv-r31-g1-e1",
            fake_sha,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            RawDepthDemoAuthoringError::ReportHashMismatch { .. }
        ));
    }

    #[test]
    fn wrong_executable_is_refused_before_candidate_selection() {
        let report = report(candidate());
        let mut marker: serde_json::Value = serde_json::from_slice(&marker(&report)).unwrap();
        marker["executable_sha256"] = format!("sha256:{}", "a".repeat(64)).into();
        let error = author_cp2077_raw_depth_demo(
            &report,
            &serde_json::to_vec(&marker).unwrap(),
            "dsv-r31-g1-e1",
            fake_sha,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            RawDepthDemoAuthoringError::WrongExecutable(_)
        ));
    }
}

//! Bounded, discovery-only D3D11 DSV evidence.
//!
//! This module deliberately stops before selection. It records what the injected probe observed,
//! validates that the observation is bounded and internally consistent, and materializes a local
//! review copy. It never authors a [`crate::DepthProfile`] or [`crate::CameraBinding`].

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt as _;
#[cfg(windows)]
use std::os::windows::io::AsRawHandle as _;

use serde::{Deserialize, Serialize};
#[cfg(feature = "evidence-authoring")]
use sha2::{Digest as _, Sha256};

pub const DSV_DISCOVERY_CONFIG_SCHEMA_V1: &str = "d3d11-dsv-discovery-config/v1";
pub const DSV_DISCOVERY_CONFIG_SCHEMA_V2: &str = "d3d11-dsv-discovery-config/v2";
pub const DSV_DISCOVERY_REPORT_SCHEMA_V1: &str = "d3d11-dsv-discovery-report/v1";
pub const DSV_DISCOVERY_REPORT_SCHEMA_V2: &str = "d3d11-dsv-discovery-report/v2";
pub const DSV_DISCOVERY_REPORT_SCHEMA_V3: &str = "d3d11-dsv-discovery-report/v3";
pub const DSV_DISCOVERY_REPORT_FILE_NAME: &str = "d3d11-dsv-discovery-report.json";
pub const DSV_DISCOVERY_CANDIDATE_PASS_SUMMARY_SCHEMA_V1: &str =
    "d3d11-dsv-candidate-pass-summary/v1";
pub const DSV_DISCOVERY_CANDIDATE_PASS_SUMMARY_FILE_NAME: &str =
    "d3d11-dsv-candidate-pass-summary.json";
// v2/v3 admit 1,200 frames x 64 candidate rows x 12 completed-pass tuples. Pretty-print indentation
// makes that legal maximum materially larger than the old inventory-only estimate (~152 MiB).
// Keep a conservative 256 MiB writer/reader ceiling; the maximum-shape regression below serializes
// one real max-width v3 frame and pessimistically repeats the whole document envelope 1,200 times.
pub const MAX_DSV_DISCOVERY_REPORT_BYTES: u64 = 256 * 1024 * 1024;
pub const MAX_DSV_DISCOVERY_FRAMES: u32 = 1_200;
pub const MAX_DSV_DISCOVERY_CANDIDATES: u32 = 256;
pub const MAX_DSV_DISCOVERY_CANDIDATES_PER_FRAME: u32 = 64;
/// The legacy D3D11 snapshot ring can address only the final twelve completed passes. Discovery
/// keeps the same bounded envelope so authoring can explain every admissible `unbind_from_end`
/// value without growing an unbounded render-thread ledger.
pub const MAX_DSV_DISCOVERY_COMPLETED_PASSES_PER_CANDIDATE_PER_FRAME: usize = 12;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DsvDiscoveryLimits {
    pub max_frames: u32,
    pub max_candidates: u32,
    pub max_candidates_per_frame: u32,
}

impl Default for DsvDiscoveryLimits {
    fn default() -> Self {
        Self {
            max_frames: 600,
            max_candidates: 128,
            max_candidates_per_frame: 32,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DsvDiscoveryConfig {
    pub schema: String,
    pub discovery_id: String,
    pub executable_sha256: String,
    /// Existing operator-owned root. `ProbeConfig.output_dir` must be one new direct child.
    pub evidence_root: String,
    #[serde(default)]
    pub limits: DsvDiscoveryLimits,
}

impl DsvDiscoveryConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema != DSV_DISCOVERY_CONFIG_SCHEMA_V2 {
            return Err("D3D11 DSV discovery config schema is unsupported".into());
        }
        validate_identity_and_limits(&self.discovery_id, &self.executable_sha256, self.limits)?;
        if !Path::new(&self.evidence_root).is_absolute() {
            return Err("D3D11 DSV discovery evidence_root must be absolute".into());
        }
        Ok(())
    }
}

fn validate_identity_and_limits(
    discovery_id: &str,
    executable_sha256: &str,
    limits: DsvDiscoveryLimits,
) -> Result<(), String> {
    if discovery_id.is_empty()
        || discovery_id.len() > 128
        || !discovery_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(
            "D3D11 DSV discovery id must contain only ASCII letters, digits, - or _".into(),
        );
    }
    if !crate::camera_binding::is_sha256_digest(executable_sha256) {
        return Err("D3D11 DSV discovery executable hash is not canonical SHA-256".into());
    }
    if !(1..=MAX_DSV_DISCOVERY_FRAMES).contains(&limits.max_frames) {
        return Err("D3D11 DSV discovery max_frames is outside the admitted bound".into());
    }
    if !(1..=MAX_DSV_DISCOVERY_CANDIDATES).contains(&limits.max_candidates) {
        return Err("D3D11 DSV discovery max_candidates is outside the admitted bound".into());
    }
    if !(1..=MAX_DSV_DISCOVERY_CANDIDATES_PER_FRAME).contains(&limits.max_candidates_per_frame) {
        return Err(
            "D3D11 DSV discovery max_candidates_per_frame is outside the admitted bound".into(),
        );
    }
    Ok(())
}

/// Exact D3D11 view/resource identity. Numeric DXGI values are used so an unknown future label is
/// never guessed or silently normalized by an older offline binary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DsvDiscoveryDescriptor {
    pub view_format: i32,
    pub view_dimension: i32,
    pub view_flags: u32,
    pub mip_slice: u32,
    pub first_array_slice: u32,
    pub view_array_size: u32,
    pub resource_format: i32,
    pub width: u32,
    pub height: u32,
    pub resource_mip_levels: u32,
    pub resource_array_size: u32,
    pub sample_count: u32,
    pub sample_quality: u32,
    pub usage: i32,
    pub bind_flags: u32,
    pub cpu_access_flags: u32,
    pub misc_flags: u32,
}

impl DsvDiscoveryDescriptor {
    pub(crate) fn validate(self) -> Result<(), String> {
        if self.view_format < 0 || self.resource_format < 0 {
            return Err("D3D11 DSV discovery contains a negative DXGI format".into());
        }
        if self.width == 0
            || self.height == 0
            || self.resource_mip_levels == 0
            || self.resource_array_size == 0
            || self.sample_count == 0
            || self.view_array_size == 0
        {
            return Err("D3D11 DSV discovery contains an empty resource geometry".into());
        }
        // A Texture2D DSV can only use the four 2D view dimensions. Keep the numeric values in the
        // report, but validate their D3D11 relationship instead of guessing labels offline.
        if !(3..=6).contains(&self.view_dimension)
            || self.view_flags & !0x3 != 0
            || self.bind_flags & 0x40 == 0
            || self
                .first_array_slice
                .checked_add(self.view_array_size)
                .is_none_or(|end| end > self.resource_array_size)
        {
            return Err("D3D11 DSV discovery contains an invalid Texture2D DSV descriptor".into());
        }
        let multisampled_view = matches!(self.view_dimension, 5 | 6);
        if (multisampled_view && (self.sample_count == 1 || self.mip_slice != 0))
            || (!multisampled_view
                && (self.sample_count != 1 || self.mip_slice >= self.resource_mip_levels))
        {
            return Err(
                "D3D11 DSV discovery view/resource sample or mip identity is inconsistent".into(),
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DsvDiscoveryCandidateObservation {
    /// Process-local pointer identities are replaced by first-observation ordinals in the probe.
    pub candidate_id: String,
    pub descriptor: DsvDiscoveryDescriptor,
    pub bind_count: u32,
    pub unbind_count: u32,
    pub pass_count: u32,
    pub draw_calls: u64,
    pub clear_count: u32,
    /// Completed candidate passes in unbind order. Each compact tuple is
    /// `[draw_calls_since_bind, clear_count_since_bind, carried_from_previous_present]`; its
    /// one-based array position is the candidate's `unbind_ordinal_from_start` for this Present.
    /// v1 reports did not contain this ledger and deserialize to an empty vector.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub completed_passes: Vec<DsvDiscoveryCompletedPassObservation>,
    /// Completed passes beyond the twelve-entry formal snapshot envelope. Any non-zero value is
    /// a durable blocker; the report never pretends its pass ledger is complete.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub dropped_completed_passes: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_bound_order: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_bound_order: Option<u64>,
    pub carried_from_previous_present: bool,
}

/// Compact wire shape for one completed DSV pass. Field names would multiply the worst-case JSON
/// report size by hundreds of thousands of rows, so the tuple layout is schema-versioned instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DsvDiscoveryCompletedPassObservation(pub u64, pub u32, pub bool);

fn is_zero_u32(value: &u32) -> bool {
    *value == 0
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DsvDiscoveryFrameObservation {
    pub present_seq: u64,
    pub candidates: Vec<DsvDiscoveryCandidateObservation>,
    /// Groups of distinct DSV views with the same exact descriptor in this Present.
    pub ambiguous_descriptor_groups: Vec<Vec<String>>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DsvDiscoveryHookIntegrityFailureReasons {
    pub unsupported_execute_command_list: u64,
    pub vtable_or_slot_drift: u64,
    pub context_drift: u64,
    pub tracker_failure: u64,
    pub lifecycle_or_queue_failure: u64,
}

impl DsvDiscoveryHookIntegrityFailureReasons {
    fn checked_total(self) -> Result<u64, String> {
        checked_sum(&[
            self.unsupported_execute_command_list,
            self.vtable_or_slot_drift,
            self.context_drift,
            self.tracker_failure,
            self.lifecycle_or_queue_failure,
        ])
        .map_err(|_| "D3D11 DSV hook-integrity reason total overflowed".into())
    }

    fn checked_add(self, other: Self) -> Result<Self, String> {
        Ok(Self {
            unsupported_execute_command_list: self
                .unsupported_execute_command_list
                .checked_add(other.unsupported_execute_command_list)
                .ok_or("D3D11 DSV unsupported-command-list counter overflowed")?,
            vtable_or_slot_drift: self
                .vtable_or_slot_drift
                .checked_add(other.vtable_or_slot_drift)
                .ok_or("D3D11 DSV vtable/slot-drift counter overflowed")?,
            context_drift: self
                .context_drift
                .checked_add(other.context_drift)
                .ok_or("D3D11 DSV context-drift counter overflowed")?,
            tracker_failure: self
                .tracker_failure
                .checked_add(other.tracker_failure)
                .ok_or("D3D11 DSV tracker-failure counter overflowed")?,
            lifecycle_or_queue_failure: self
                .lifecycle_or_queue_failure
                .checked_add(other.lifecycle_or_queue_failure)
                .ok_or("D3D11 DSV lifecycle/queue-failure counter overflowed")?,
        })
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DsvDiscoveryRuntimeCounters {
    pub queue_dropped_frames: u64,
    pub queue_dropped_candidate_rows: u64,
    pub hook_dropped_events: u64,
    pub hook_integrity_failures: u64,
    /// Exhaustive v3 classification of `hook_integrity_failures`. Older v1/v2 reports omit this
    /// ledger; a v3 validator requires its checked sum to equal the aggregate exactly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hook_integrity_failure_reasons: Option<DsvDiscoveryHookIntegrityFailureReasons>,
    pub descriptor_identity_conflicts: u64,
    pub candidate_limit_events: u64,
    /// Completed pass rows discarded after the per-candidate twelve-entry bound.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub pass_ledger_dropped_rows: u64,
}

fn is_zero_u64(value: &u64) -> bool {
    *value == 0
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DsvDiscoveryLossSummary {
    pub non_monotonic_frames: u64,
    pub non_monotonic_candidate_rows: u64,
    pub frame_limit_frames: u64,
    pub frame_limit_candidate_rows: u64,
    pub per_frame_limit_candidate_rows: u64,
    pub candidate_limit_candidate_rows: u64,
    pub invalid_candidate_rows: u64,
    pub descriptor_conflict_candidate_rows: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DsvDiscoverySummary {
    pub observed_frames: u64,
    pub retained_frames: u64,
    pub dropped_frames: u64,
    pub observed_candidate_rows: u64,
    pub retained_candidate_rows: u64,
    pub dropped_candidate_rows: u64,
    pub distinct_candidates: u64,
    pub ambiguous_frames: u64,
    pub loss: DsvDiscoveryLossSummary,
    #[serde(flatten)]
    pub runtime: DsvDiscoveryRuntimeCounters,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DsvDiscoveryBlocker {
    NoFramesObserved,
    NoCandidatesObserved,
    NonMonotonicPresent,
    FrameLimitReached,
    CandidateLimitReached,
    PerFrameCandidateLimitReached,
    AmbiguousDescriptorIdentity,
    DescriptorIdentityConflict,
    QueueSaturated,
    HookObservationDropped,
    HookIntegrityFailure,
    PassLedgerLimitReached,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DsvDiscoveryReport {
    pub schema: String,
    pub mode: String,
    pub discovery_id: String,
    pub executable_sha256: String,
    pub renderer_backend: String,
    pub recording_ready: bool,
    pub deliverable: bool,
    pub profile_generated: bool,
    pub binding_generated: bool,
    pub limits: DsvDiscoveryLimits,
    pub frames: Vec<DsvDiscoveryFrameObservation>,
    pub summary: DsvDiscoverySummary,
    pub blockers: Vec<DsvDiscoveryBlocker>,
    pub complete: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DsvDiscoveryCandidatePassSummaryBlocker {
    OperatorVisualCandidateReviewMissing,
    OperatorUnbindOrdinalSelectionMissing,
    ProjectionEvidenceMissing,
    MetricScaleEvidenceMissing,
    CameraPlayerBindingMissing,
    PerformanceCertificationMissing,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DsvDiscoveryPassOrdinalStats {
    pub unbind_ordinal_from_end: u32,
    pub observed_passes: u64,
    pub total_draw_calls: u64,
    pub min_draw_calls: u64,
    pub max_draw_calls: u64,
    pub passes_with_clear: u64,
    pub carried_from_previous_present: u64,
}

/// Exact-build offline reduction of one run-local DSV candidate. This is deliberately a review
/// surface, not a profile author: it binds the source report bytes and exposes every admissible
/// `unbind_from_end` histogram while keeping all promotion gates closed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DsvDiscoveryCandidatePassSummary {
    pub schema: String,
    pub source_report_schema: String,
    pub source_report_sha256: String,
    pub discovery_id: String,
    pub executable_sha256: String,
    pub renderer_backend: String,
    pub inspected_candidate_id: String,
    pub descriptor: DsvDiscoveryDescriptor,
    pub observed_frames: u64,
    pub completed_passes: u64,
    pub ordinal_stats: Vec<DsvDiscoveryPassOrdinalStats>,
    pub candidate_selected: bool,
    pub unbind_ordinal_selected: bool,
    pub recording_ready: bool,
    pub deliverable: bool,
    pub profile_generated: bool,
    pub binding_generated: bool,
    pub blockers: Vec<DsvDiscoveryCandidatePassSummaryBlocker>,
}

impl DsvDiscoveryCandidatePassSummary {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema != DSV_DISCOVERY_CANDIDATE_PASS_SUMMARY_SCHEMA_V1
            || !matches!(
                self.source_report_schema.as_str(),
                DSV_DISCOVERY_REPORT_SCHEMA_V2 | DSV_DISCOVERY_REPORT_SCHEMA_V3
            )
            || self.renderer_backend != "d3d11"
            || !crate::camera_binding::is_sha256_digest(&self.source_report_sha256)
            || !crate::camera_binding::is_sha256_digest(&self.executable_sha256)
        {
            return Err("D3D11 DSV candidate pass summary identity is invalid".into());
        }
        if self.inspected_candidate_id.len() != 8
            || !self.inspected_candidate_id.starts_with("dsv-")
            || !self.inspected_candidate_id[4..]
                .bytes()
                .all(|byte| byte.is_ascii_digit())
        {
            return Err("D3D11 DSV candidate pass summary id is not a probe ordinal".into());
        }
        if self.candidate_selected
            || self.unbind_ordinal_selected
            || self.recording_ready
            || self.deliverable
            || self.profile_generated
            || self.binding_generated
        {
            return Err("D3D11 DSV candidate pass summary weakened its review-only ceiling".into());
        }
        if self.observed_frames == 0 || self.completed_passes == 0 || self.ordinal_stats.is_empty()
        {
            return Err(
                "D3D11 DSV candidate pass summary contains no completed-pass evidence".into(),
            );
        }
        let expected_blockers = vec![
            DsvDiscoveryCandidatePassSummaryBlocker::OperatorVisualCandidateReviewMissing,
            DsvDiscoveryCandidatePassSummaryBlocker::OperatorUnbindOrdinalSelectionMissing,
            DsvDiscoveryCandidatePassSummaryBlocker::ProjectionEvidenceMissing,
            DsvDiscoveryCandidatePassSummaryBlocker::MetricScaleEvidenceMissing,
            DsvDiscoveryCandidatePassSummaryBlocker::CameraPlayerBindingMissing,
            DsvDiscoveryCandidatePassSummaryBlocker::PerformanceCertificationMissing,
        ];
        if self.blockers != expected_blockers {
            return Err("D3D11 DSV candidate pass summary blocker ledger is inconsistent".into());
        }
        let mut previous = 0;
        let mut total_passes = 0u64;
        for stats in &self.ordinal_stats {
            if stats.unbind_ordinal_from_end == 0
                || stats.unbind_ordinal_from_end <= previous
                || stats.observed_passes == 0
                || stats.min_draw_calls > stats.max_draw_calls
                || stats.passes_with_clear > stats.observed_passes
                || stats.carried_from_previous_present > stats.observed_passes
            {
                return Err("D3D11 DSV candidate pass summary ordinal ledger is invalid".into());
            }
            previous = stats.unbind_ordinal_from_end;
            total_passes = total_passes
                .checked_add(stats.observed_passes)
                .ok_or("D3D11 DSV candidate pass summary count overflowed")?;
            let minimum_total = stats
                .min_draw_calls
                .checked_mul(stats.observed_passes)
                .ok_or("D3D11 DSV candidate pass summary draw minimum overflowed")?;
            let maximum_total = stats
                .max_draw_calls
                .checked_mul(stats.observed_passes)
                .ok_or("D3D11 DSV candidate pass summary draw maximum overflowed")?;
            if stats.total_draw_calls < minimum_total || stats.total_draw_calls > maximum_total {
                return Err("D3D11 DSV candidate pass summary draw totals are inconsistent".into());
            }
        }
        if total_passes != self.completed_passes {
            return Err("D3D11 DSV candidate pass summary total is inconsistent".into());
        }
        self.descriptor.validate()
    }
}

pub fn summarize_dsv_candidate_passes(
    report: &DsvDiscoveryReport,
    source_report_sha256: String,
    candidate_id: &str,
) -> Result<DsvDiscoveryCandidatePassSummary, String> {
    report.validate()?;
    if !matches!(
        report.schema.as_str(),
        DSV_DISCOVERY_REPORT_SCHEMA_V2 | DSV_DISCOVERY_REPORT_SCHEMA_V3
    ) || !report.complete
    {
        return Err(
            "D3D11 DSV candidate pass summary requires one complete v2/v3 discovery report".into(),
        );
    }
    if !crate::camera_binding::is_sha256_digest(&source_report_sha256) {
        return Err("D3D11 DSV source report hash is not canonical SHA-256".into());
    }
    let mut descriptor = None;
    let mut observed_frames = 0u64;
    let mut completed_passes = 0u64;
    let mut ordinals = BTreeMap::<u32, DsvDiscoveryPassOrdinalStats>::new();
    for frame in &report.frames {
        let Some(candidate) = frame
            .candidates
            .iter()
            .find(|candidate| candidate.candidate_id == candidate_id)
        else {
            continue;
        };
        observed_frames = observed_frames
            .checked_add(1)
            .ok_or("D3D11 DSV candidate observed-frame count overflowed")?;
        if descriptor
            .replace(candidate.descriptor)
            .is_some_and(|previous| previous != candidate.descriptor)
        {
            return Err("D3D11 DSV candidate descriptor drifted while summarizing".into());
        }
        let pass_count = u32::try_from(candidate.completed_passes.len())
            .map_err(|_| "D3D11 DSV candidate pass count overflowed")?;
        for (index, pass) in candidate.completed_passes.iter().enumerate() {
            let ordinal_from_start = u32::try_from(index + 1)
                .map_err(|_| "D3D11 DSV candidate pass ordinal overflowed")?;
            let ordinal_from_end = pass_count - ordinal_from_start + 1;
            let stats = ordinals
                .entry(ordinal_from_end)
                .or_insert(DsvDiscoveryPassOrdinalStats {
                    unbind_ordinal_from_end: ordinal_from_end,
                    observed_passes: 0,
                    total_draw_calls: 0,
                    min_draw_calls: pass.0,
                    max_draw_calls: pass.0,
                    passes_with_clear: 0,
                    carried_from_previous_present: 0,
                });
            stats.observed_passes = stats
                .observed_passes
                .checked_add(1)
                .ok_or("D3D11 DSV candidate pass count overflowed")?;
            stats.total_draw_calls = stats
                .total_draw_calls
                .checked_add(pass.0)
                .ok_or("D3D11 DSV candidate draw count overflowed")?;
            stats.min_draw_calls = stats.min_draw_calls.min(pass.0);
            stats.max_draw_calls = stats.max_draw_calls.max(pass.0);
            stats.passes_with_clear = stats
                .passes_with_clear
                .checked_add(u64::from(pass.1 > 0))
                .ok_or("D3D11 DSV candidate clear count overflowed")?;
            stats.carried_from_previous_present = stats
                .carried_from_previous_present
                .checked_add(u64::from(pass.2))
                .ok_or("D3D11 DSV candidate carried-pass count overflowed")?;
            completed_passes = completed_passes
                .checked_add(1)
                .ok_or("D3D11 DSV candidate completed-pass count overflowed")?;
        }
    }
    let summary = DsvDiscoveryCandidatePassSummary {
        schema: DSV_DISCOVERY_CANDIDATE_PASS_SUMMARY_SCHEMA_V1.into(),
        source_report_schema: report.schema.clone(),
        source_report_sha256,
        discovery_id: report.discovery_id.clone(),
        executable_sha256: report.executable_sha256.clone(),
        renderer_backend: report.renderer_backend.clone(),
        inspected_candidate_id: candidate_id.into(),
        descriptor: descriptor.ok_or("D3D11 DSV candidate does not exist in the report")?,
        observed_frames,
        completed_passes,
        ordinal_stats: ordinals.into_values().collect(),
        candidate_selected: false,
        unbind_ordinal_selected: false,
        recording_ready: false,
        deliverable: false,
        profile_generated: false,
        binding_generated: false,
        blockers: vec![
            DsvDiscoveryCandidatePassSummaryBlocker::OperatorVisualCandidateReviewMissing,
            DsvDiscoveryCandidatePassSummaryBlocker::OperatorUnbindOrdinalSelectionMissing,
            DsvDiscoveryCandidatePassSummaryBlocker::ProjectionEvidenceMissing,
            DsvDiscoveryCandidatePassSummaryBlocker::MetricScaleEvidenceMissing,
            DsvDiscoveryCandidatePassSummaryBlocker::CameraPlayerBindingMissing,
            DsvDiscoveryCandidatePassSummaryBlocker::PerformanceCertificationMissing,
        ],
    };
    summary.validate()?;
    Ok(summary)
}

impl DsvDiscoveryReport {
    pub fn validate(&self) -> Result<(), String> {
        if !matches!(
            self.schema.as_str(),
            DSV_DISCOVERY_REPORT_SCHEMA_V1
                | DSV_DISCOVERY_REPORT_SCHEMA_V2
                | DSV_DISCOVERY_REPORT_SCHEMA_V3
        ) || self.mode != "discovery_only"
            || self.renderer_backend != "d3d11"
        {
            return Err("D3D11 DSV discovery report identity is invalid".into());
        }
        if self.recording_ready
            || self.deliverable
            || self.profile_generated
            || self.binding_generated
        {
            return Err("D3D11 DSV discovery report weakened its discovery-only ceiling".into());
        }
        validate_identity_and_limits(&self.discovery_id, &self.executable_sha256, self.limits)?;
        match self.schema.as_str() {
            DSV_DISCOVERY_REPORT_SCHEMA_V3 => {
                let reasons = self
                    .summary
                    .runtime
                    .hook_integrity_failure_reasons
                    .ok_or("D3D11 DSV v3 report is missing its hook-integrity reason ledger")?;
                if reasons.checked_total()? != self.summary.runtime.hook_integrity_failures {
                    return Err(
                        "D3D11 DSV hook-integrity aggregate does not match its reason ledger"
                            .into(),
                    );
                }
            }
            DSV_DISCOVERY_REPORT_SCHEMA_V1 | DSV_DISCOVERY_REPORT_SCHEMA_V2 => {
                if self
                    .summary
                    .runtime
                    .hook_integrity_failure_reasons
                    .is_some()
                {
                    return Err(
                        "historical D3D11 DSV discovery report contains a v3 hook-integrity reason ledger"
                            .into(),
                    );
                }
            }
            _ => unreachable!("report schema identity checked above"),
        }
        if self.frames.len() > self.limits.max_frames as usize
            || self.summary.retained_frames != self.frames.len() as u64
        {
            return Err("D3D11 DSV discovery retained frame count is inconsistent".into());
        }
        let mut last_present = None;
        let mut descriptors = BTreeMap::<String, DsvDiscoveryDescriptor>::new();
        let mut candidate_rows = 0u64;
        let mut dropped_completed_pass_rows = 0u64;
        let mut ambiguous_frames = 0u64;
        for frame in &self.frames {
            if last_present.is_some_and(|previous| frame.present_seq <= previous) {
                return Err("D3D11 DSV discovery Presents are not strictly monotonic".into());
            }
            last_present = Some(frame.present_seq);
            if frame.candidates.len() > self.limits.max_candidates_per_frame as usize {
                return Err("D3D11 DSV discovery frame exceeds its candidate bound".into());
            }
            let mut frame_ids = BTreeSet::new();
            let mut bind_order_owners = BTreeMap::<u64, String>::new();
            let mut descriptor_groups = BTreeMap::<DsvDiscoveryDescriptor, Vec<String>>::new();
            let mut frame_bind_count = 0u64;
            let mut frame_last_bound_order = 0u64;
            for candidate in &frame.candidates {
                validate_candidate(candidate, &self.schema)?;
                if !frame_ids.insert(candidate.candidate_id.clone()) {
                    return Err("D3D11 DSV discovery frame repeats a candidate id".into());
                }
                if let Some(previous) =
                    descriptors.insert(candidate.candidate_id.clone(), candidate.descriptor)
                    && previous != candidate.descriptor
                {
                    return Err("D3D11 DSV discovery candidate descriptor drifted".into());
                }
                descriptor_groups
                    .entry(candidate.descriptor)
                    .or_default()
                    .push(candidate.candidate_id.clone());
                candidate_rows = candidate_rows
                    .checked_add(1)
                    .ok_or("D3D11 DSV discovery candidate row count overflowed")?;
                dropped_completed_pass_rows = dropped_completed_pass_rows
                    .checked_add(u64::from(candidate.dropped_completed_passes))
                    .ok_or("D3D11 DSV discovery completed-pass loss count overflowed")?;
                frame_bind_count = frame_bind_count
                    .checked_add(u64::from(candidate.bind_count))
                    .ok_or("D3D11 DSV discovery bind count overflowed")?;
                if let (Some(first), Some(last)) =
                    (candidate.first_bound_order, candidate.last_bound_order)
                {
                    frame_last_bound_order = frame_last_bound_order.max(last);
                    for order in [first, last] {
                        if let Some(owner) =
                            bind_order_owners.insert(order, candidate.candidate_id.clone())
                            && owner != candidate.candidate_id
                        {
                            return Err(
                                "D3D11 DSV discovery bind order belongs to multiple candidates"
                                    .into(),
                            );
                        }
                    }
                }
            }
            if frame_bind_count != frame_last_bound_order {
                return Err("D3D11 DSV discovery frame bind-order range is inconsistent".into());
            }
            if descriptors.len() > self.limits.max_candidates as usize {
                return Err("D3D11 DSV discovery exceeds its distinct candidate bound".into());
            }
            let mut expected_groups = descriptor_groups
                .into_values()
                .filter(|group| group.len() > 1)
                .collect::<Vec<_>>();
            for group in &mut expected_groups {
                group.sort();
            }
            expected_groups.sort();
            if frame.ambiguous_descriptor_groups != expected_groups {
                return Err("D3D11 DSV discovery ambiguity ledger is inconsistent".into());
            }
            ambiguous_frames = ambiguous_frames
                .checked_add(u64::from(!expected_groups.is_empty()))
                .ok_or("D3D11 DSV discovery ambiguous frame count overflowed")?;
        }
        let expected_dropped_frames = checked_sum(&[
            self.summary.loss.non_monotonic_frames,
            self.summary.loss.frame_limit_frames,
            self.summary.runtime.queue_dropped_frames,
        ])?;
        let expected_dropped_candidate_rows = checked_sum(&[
            self.summary.loss.non_monotonic_candidate_rows,
            self.summary.loss.frame_limit_candidate_rows,
            self.summary.loss.per_frame_limit_candidate_rows,
            self.summary.loss.candidate_limit_candidate_rows,
            self.summary.loss.invalid_candidate_rows,
            self.summary.loss.descriptor_conflict_candidate_rows,
            self.summary.runtime.queue_dropped_candidate_rows,
        ])?;
        let expected_observed_frames = self
            .summary
            .retained_frames
            .checked_add(self.summary.dropped_frames)
            .ok_or("D3D11 DSV discovery frame summary overflowed")?;
        let expected_observed_candidate_rows = self
            .summary
            .retained_candidate_rows
            .checked_add(self.summary.dropped_candidate_rows)
            .ok_or("D3D11 DSV discovery candidate summary overflowed")?;
        if (self.summary.loss.non_monotonic_candidate_rows > 0
            && self.summary.loss.non_monotonic_frames == 0)
            || (self.summary.loss.frame_limit_candidate_rows > 0
                && self.summary.loss.frame_limit_frames == 0)
            || (self.summary.runtime.queue_dropped_candidate_rows > 0
                && self.summary.runtime.queue_dropped_frames == 0)
        {
            return Err("D3D11 DSV discovery candidate loss has no owning frame".into());
        }
        if self.summary.retained_candidate_rows != candidate_rows
            || self.summary.distinct_candidates != descriptors.len() as u64
            || self.summary.ambiguous_frames != ambiguous_frames
            || self.summary.dropped_frames != expected_dropped_frames
            || self.summary.dropped_candidate_rows != expected_dropped_candidate_rows
            || self.summary.observed_frames != expected_observed_frames
            || self.summary.observed_candidate_rows != expected_observed_candidate_rows
            || self.summary.runtime.pass_ledger_dropped_rows < dropped_completed_pass_rows
        {
            return Err("D3D11 DSV discovery summary is inconsistent".into());
        }
        let expected_blockers = blockers_for_summary(&self.summary);
        if self.blockers != expected_blockers || self.complete != self.blockers.is_empty() {
            return Err("D3D11 DSV discovery blocker ledger is inconsistent".into());
        }
        Ok(())
    }
}

fn checked_sum(values: &[u64]) -> Result<u64, String> {
    values.iter().try_fold(0u64, |sum, value| {
        sum.checked_add(*value)
            .ok_or_else(|| "D3D11 DSV discovery loss summary overflowed".to_string())
    })
}

fn validate_candidate(
    candidate: &DsvDiscoveryCandidateObservation,
    report_schema: &str,
) -> Result<(), String> {
    if candidate.candidate_id.len() != 8
        || !candidate.candidate_id.starts_with("dsv-")
        || !candidate.candidate_id[4..]
            .bytes()
            .all(|byte| byte.is_ascii_digit())
    {
        return Err("D3D11 DSV discovery candidate id is not a probe ordinal".into());
    }
    candidate.descriptor.validate()?;
    if report_schema == DSV_DISCOVERY_REPORT_SCHEMA_V1 {
        if !candidate.completed_passes.is_empty() || candidate.dropped_completed_passes != 0 {
            return Err("D3D11 DSV discovery v1 report contains a v2 completed-pass ledger".into());
        }
    } else {
        if candidate.completed_passes.len()
            > MAX_DSV_DISCOVERY_COMPLETED_PASSES_PER_CANDIDATE_PER_FRAME
        {
            return Err("D3D11 DSV discovery candidate exceeds its completed-pass bound".into());
        }
        let retained_and_dropped = u64::try_from(candidate.completed_passes.len())
            .map_err(|_| "D3D11 DSV discovery completed-pass count overflowed")?
            .checked_add(u64::from(candidate.dropped_completed_passes))
            .ok_or("D3D11 DSV discovery completed-pass count overflowed")?;
        if retained_and_dropped != u64::from(candidate.unbind_count) {
            return Err(
                "D3D11 DSV discovery completed-pass ledger does not match unbind_count".into(),
            );
        }
        if candidate.completed_passes.iter().any(|pass| pass.2)
            && !candidate.carried_from_previous_present
        {
            return Err(
                "D3D11 DSV discovery completed-pass carry is missing from candidate ledger".into(),
            );
        }
    }
    match (candidate.first_bound_order, candidate.last_bound_order) {
        (Some(first), Some(last))
            if candidate.bind_count > 0
                && first > 0
                && last >= first
                && u64::from(candidate.bind_count) <= last - first + 1 => {}
        (None, None) if candidate.bind_count == 0 => {}
        _ => return Err("D3D11 DSV discovery bind-order ledger is inconsistent".into()),
    }
    if candidate.pass_count
        > candidate
            .bind_count
            .saturating_add(u32::from(candidate.carried_from_previous_present))
        || candidate.unbind_count > candidate.pass_count
        || candidate.unbind_count
            > candidate
                .bind_count
                .saturating_add(u32::from(candidate.carried_from_previous_present))
        || (candidate.carried_from_previous_present && candidate.pass_count == 0)
        || (candidate.bind_count > 0 && candidate.pass_count == 0)
        || (candidate.draw_calls > 0 && candidate.pass_count == 0)
    {
        return Err("D3D11 DSV discovery pass/unbind ledger is inconsistent".into());
    }
    Ok(())
}

#[derive(Debug)]
pub struct DsvDiscoveryBuilder {
    config: DsvDiscoveryConfig,
    frames: Vec<DsvDiscoveryFrameObservation>,
    descriptors: BTreeMap<String, DsvDiscoveryDescriptor>,
    summary: DsvDiscoverySummary,
    last_present: Option<u64>,
}

impl DsvDiscoveryBuilder {
    pub fn new(config: DsvDiscoveryConfig) -> Result<Self, String> {
        config.validate()?;
        Ok(Self {
            frames: Vec::with_capacity(config.limits.max_frames as usize),
            descriptors: BTreeMap::new(),
            summary: DsvDiscoverySummary::default(),
            last_present: None,
            config,
        })
    }

    pub fn observe_frame(&mut self, mut frame: DsvDiscoveryFrameObservation) {
        self.summary.observed_frames = self.summary.observed_frames.saturating_add(1);
        self.summary.observed_candidate_rows = self
            .summary
            .observed_candidate_rows
            .saturating_add(frame.candidates.len() as u64);
        if self
            .last_present
            .is_some_and(|previous| frame.present_seq <= previous)
        {
            self.summary.dropped_frames = self.summary.dropped_frames.saturating_add(1);
            self.summary.dropped_candidate_rows = self
                .summary
                .dropped_candidate_rows
                .saturating_add(frame.candidates.len() as u64);
            self.summary.loss.non_monotonic_frames =
                self.summary.loss.non_monotonic_frames.saturating_add(1);
            self.summary.loss.non_monotonic_candidate_rows = self
                .summary
                .loss
                .non_monotonic_candidate_rows
                .saturating_add(frame.candidates.len() as u64);
            return;
        }
        self.last_present = Some(frame.present_seq);
        if self.frames.len() >= self.config.limits.max_frames as usize {
            self.summary.dropped_frames = self.summary.dropped_frames.saturating_add(1);
            self.summary.dropped_candidate_rows = self
                .summary
                .dropped_candidate_rows
                .saturating_add(frame.candidates.len() as u64);
            self.summary.loss.frame_limit_frames =
                self.summary.loss.frame_limit_frames.saturating_add(1);
            self.summary.loss.frame_limit_candidate_rows = self
                .summary
                .loss
                .frame_limit_candidate_rows
                .saturating_add(frame.candidates.len() as u64);
            return;
        }
        frame
            .candidates
            .sort_by(|left, right| left.candidate_id.cmp(&right.candidate_id));
        if frame.candidates.len() > self.config.limits.max_candidates_per_frame as usize {
            let dropped =
                frame.candidates.len() - self.config.limits.max_candidates_per_frame as usize;
            frame
                .candidates
                .truncate(self.config.limits.max_candidates_per_frame as usize);
            self.summary.dropped_candidate_rows = self
                .summary
                .dropped_candidate_rows
                .saturating_add(dropped as u64);
            self.summary.loss.per_frame_limit_candidate_rows = self
                .summary
                .loss
                .per_frame_limit_candidate_rows
                .saturating_add(dropped as u64);
        }
        frame.candidates.retain(|candidate| {
            if validate_candidate(candidate, DSV_DISCOVERY_REPORT_SCHEMA_V3).is_err() {
                self.summary.dropped_candidate_rows =
                    self.summary.dropped_candidate_rows.saturating_add(1);
                self.summary.runtime.hook_integrity_failures = self
                    .summary
                    .runtime
                    .hook_integrity_failures
                    .saturating_add(1);
                let tracker_failures = &mut self
                    .summary
                    .runtime
                    .hook_integrity_failure_reasons
                    .get_or_insert_default()
                    .tracker_failure;
                *tracker_failures = tracker_failures.saturating_add(1);
                self.summary.loss.invalid_candidate_rows =
                    self.summary.loss.invalid_candidate_rows.saturating_add(1);
                return false;
            }
            if let Some(previous) = self.descriptors.get(&candidate.candidate_id) {
                if previous != &candidate.descriptor {
                    self.summary.dropped_candidate_rows =
                        self.summary.dropped_candidate_rows.saturating_add(1);
                    self.summary.runtime.descriptor_identity_conflicts = self
                        .summary
                        .runtime
                        .descriptor_identity_conflicts
                        .saturating_add(1);
                    self.summary.loss.descriptor_conflict_candidate_rows = self
                        .summary
                        .loss
                        .descriptor_conflict_candidate_rows
                        .saturating_add(1);
                    return false;
                }
                return true;
            }
            if self.descriptors.len() >= self.config.limits.max_candidates as usize {
                self.summary.dropped_candidate_rows =
                    self.summary.dropped_candidate_rows.saturating_add(1);
                self.summary.loss.candidate_limit_candidate_rows = self
                    .summary
                    .loss
                    .candidate_limit_candidate_rows
                    .saturating_add(1);
                return false;
            }
            self.descriptors
                .insert(candidate.candidate_id.clone(), candidate.descriptor);
            true
        });
        let mut groups = BTreeMap::<DsvDiscoveryDescriptor, Vec<String>>::new();
        for candidate in &frame.candidates {
            groups
                .entry(candidate.descriptor)
                .or_default()
                .push(candidate.candidate_id.clone());
        }
        frame.ambiguous_descriptor_groups = groups
            .into_values()
            .filter(|group| group.len() > 1)
            .map(|mut group| {
                group.sort();
                group
            })
            .collect();
        frame.ambiguous_descriptor_groups.sort();
        if !frame.ambiguous_descriptor_groups.is_empty() {
            self.summary.ambiguous_frames = self.summary.ambiguous_frames.saturating_add(1);
        }
        self.summary.retained_frames = self.summary.retained_frames.saturating_add(1);
        self.summary.retained_candidate_rows = self
            .summary
            .retained_candidate_rows
            .saturating_add(frame.candidates.len() as u64);
        self.frames.push(frame);
    }

    pub fn finish(
        mut self,
        runtime: DsvDiscoveryRuntimeCounters,
    ) -> Result<DsvDiscoveryReport, String> {
        self.summary.runtime.queue_dropped_frames = self
            .summary
            .runtime
            .queue_dropped_frames
            .checked_add(runtime.queue_dropped_frames)
            .ok_or("D3D11 DSV queue-dropped frame counter overflowed")?;
        self.summary.runtime.queue_dropped_candidate_rows = self
            .summary
            .runtime
            .queue_dropped_candidate_rows
            .checked_add(runtime.queue_dropped_candidate_rows)
            .ok_or("D3D11 DSV queue-dropped candidate counter overflowed")?;
        self.summary.runtime.hook_dropped_events = self
            .summary
            .runtime
            .hook_dropped_events
            .checked_add(runtime.hook_dropped_events)
            .ok_or("D3D11 DSV hook-dropped event counter overflowed")?;
        self.summary.runtime.hook_integrity_failures = self
            .summary
            .runtime
            .hook_integrity_failures
            .checked_add(runtime.hook_integrity_failures)
            .ok_or("D3D11 DSV hook-integrity aggregate overflowed")?;
        self.summary.runtime.hook_integrity_failure_reasons = Some(
            self.summary
                .runtime
                .hook_integrity_failure_reasons
                .unwrap_or_default()
                .checked_add(runtime.hook_integrity_failure_reasons.unwrap_or_default())?,
        );
        self.summary.runtime.descriptor_identity_conflicts = self
            .summary
            .runtime
            .descriptor_identity_conflicts
            .checked_add(runtime.descriptor_identity_conflicts)
            .ok_or("D3D11 DSV descriptor-identity counter overflowed")?;
        self.summary.runtime.candidate_limit_events = self
            .summary
            .runtime
            .candidate_limit_events
            .checked_add(runtime.candidate_limit_events)
            .ok_or("D3D11 DSV candidate-limit counter overflowed")?;
        self.summary.runtime.pass_ledger_dropped_rows = self
            .summary
            .runtime
            .pass_ledger_dropped_rows
            .checked_add(runtime.pass_ledger_dropped_rows)
            .ok_or("D3D11 DSV pass-ledger dropped-row counter overflowed")?;
        self.summary.dropped_frames = self
            .summary
            .dropped_frames
            .checked_add(runtime.queue_dropped_frames)
            .ok_or("D3D11 DSV dropped-frame summary overflowed")?;
        self.summary.observed_frames = self
            .summary
            .observed_frames
            .checked_add(runtime.queue_dropped_frames)
            .ok_or("D3D11 DSV observed-frame summary overflowed")?;
        self.summary.dropped_candidate_rows = self
            .summary
            .dropped_candidate_rows
            .checked_add(runtime.queue_dropped_candidate_rows)
            .ok_or("D3D11 DSV dropped-candidate summary overflowed")?;
        self.summary.observed_candidate_rows = self
            .summary
            .observed_candidate_rows
            .checked_add(runtime.queue_dropped_candidate_rows)
            .ok_or("D3D11 DSV observed-candidate summary overflowed")?;
        self.summary.distinct_candidates = self.descriptors.len() as u64;
        let blockers = blockers_for_summary(&self.summary);
        Ok(DsvDiscoveryReport {
            schema: DSV_DISCOVERY_REPORT_SCHEMA_V3.into(),
            mode: "discovery_only".into(),
            discovery_id: self.config.discovery_id,
            executable_sha256: self.config.executable_sha256,
            renderer_backend: "d3d11".into(),
            recording_ready: false,
            deliverable: false,
            profile_generated: false,
            binding_generated: false,
            limits: self.config.limits,
            frames: self.frames,
            summary: self.summary,
            complete: blockers.is_empty(),
            blockers,
        })
    }
}

fn blockers_for_summary(summary: &DsvDiscoverySummary) -> Vec<DsvDiscoveryBlocker> {
    let mut blockers = BTreeSet::new();
    if summary.retained_frames == 0 {
        blockers.insert(DsvDiscoveryBlocker::NoFramesObserved);
    }
    if summary.retained_candidate_rows == 0 {
        blockers.insert(DsvDiscoveryBlocker::NoCandidatesObserved);
    }
    if summary.ambiguous_frames > 0 {
        blockers.insert(DsvDiscoveryBlocker::AmbiguousDescriptorIdentity);
    }
    if summary.loss.non_monotonic_frames > 0 {
        blockers.insert(DsvDiscoveryBlocker::NonMonotonicPresent);
    }
    if summary.loss.frame_limit_frames > 0 {
        blockers.insert(DsvDiscoveryBlocker::FrameLimitReached);
    }
    if summary.loss.per_frame_limit_candidate_rows > 0 {
        blockers.insert(DsvDiscoveryBlocker::PerFrameCandidateLimitReached);
    }
    if summary.loss.candidate_limit_candidate_rows > 0 || summary.runtime.candidate_limit_events > 0
    {
        blockers.insert(DsvDiscoveryBlocker::CandidateLimitReached);
    }
    if summary.runtime.queue_dropped_frames > 0 {
        blockers.insert(DsvDiscoveryBlocker::QueueSaturated);
    }
    if summary.runtime.hook_dropped_events > 0 {
        blockers.insert(DsvDiscoveryBlocker::HookObservationDropped);
    }
    if summary.runtime.hook_integrity_failures > 0 || summary.loss.invalid_candidate_rows > 0 {
        blockers.insert(DsvDiscoveryBlocker::HookIntegrityFailure);
    }
    if summary.runtime.pass_ledger_dropped_rows > 0 {
        blockers.insert(DsvDiscoveryBlocker::PassLedgerLimitReached);
    }
    if summary.runtime.descriptor_identity_conflicts > 0
        || summary.loss.descriptor_conflict_candidate_rows > 0
    {
        blockers.insert(DsvDiscoveryBlocker::DescriptorIdentityConflict);
    }
    blockers.into_iter().collect()
}

/// Atomically claims one new direct child of an existing evidence root. A canonicalized parent
/// comparison rejects `..`, junction and symlink escapes before any file is created.
pub fn create_new_evidence_directory(
    evidence_root: &Path,
    output_dir: &Path,
) -> Result<PathBuf, String> {
    if !evidence_root.is_absolute() || !output_dir.is_absolute() {
        return Err("D3D11 DSV discovery evidence paths must be absolute".into());
    }
    reject_reparse_chain(evidence_root, "evidence root")?;
    let canonical_root = evidence_root
        .canonicalize()
        .map_err(|error| format!("cannot resolve DSV evidence root: {error}"))?;
    if !canonical_root.is_dir() {
        return Err("D3D11 DSV discovery evidence root is not a directory".into());
    }
    let parent = output_dir
        .parent()
        .ok_or("D3D11 DSV discovery output has no parent")?
        .canonicalize()
        .map_err(|error| format!("cannot resolve DSV discovery output parent: {error}"))?;
    reject_reparse_chain(
        output_dir
            .parent()
            .ok_or("D3D11 DSV discovery output has no parent")?,
        "output parent",
    )?;
    if parent != canonical_root {
        return Err("D3D11 DSV discovery output must be a direct child of evidence_root".into());
    }
    let name = output_dir
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("D3D11 DSV discovery output name is not UTF-8")?;
    if name.is_empty()
        || name.len() > 128
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err("D3D11 DSV discovery output name is unsafe".into());
    }
    fs::create_dir(output_dir).map_err(|error| {
        format!(
            "D3D11 DSV discovery output must be new ({}): {error}",
            output_dir.display()
        )
    })?;
    reject_reparse_chain(output_dir, "new output directory")?;
    let canonical_output = output_dir
        .canonicalize()
        .map_err(|error| format!("cannot resolve new DSV discovery output: {error}"))?;
    let metadata = fs::symlink_metadata(output_dir)
        .map_err(|error| format!("cannot inspect new DSV discovery output: {error}"))?;
    if is_reparse_point(&metadata)
        || !metadata.is_dir()
        || canonical_output.parent() != Some(canonical_root.as_path())
    {
        return Err("new D3D11 DSV discovery output escaped evidence_root".into());
    }
    Ok(canonical_output)
}

fn is_reparse_point(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
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
        let metadata = fs::symlink_metadata(ancestor).map_err(|error| {
            format!(
                "cannot inspect D3D11 DSV discovery {label} ancestor {}: {error}",
                ancestor.display()
            )
        })?;
        if is_reparse_point(&metadata) {
            return Err(format!(
                "D3D11 DSV discovery {label} ancestor is a reparse point: {}",
                ancestor.display()
            ));
        }
    }
    Ok(())
}

fn validate_existing_evidence_directory(
    evidence_root: &Path,
    output_dir: &Path,
) -> Result<PathBuf, String> {
    reject_reparse_chain(evidence_root, "evidence root")?;
    reject_reparse_chain(output_dir, "output directory")?;
    let canonical_root = evidence_root
        .canonicalize()
        .map_err(|error| format!("cannot resolve DSV evidence root: {error}"))?;
    let metadata = fs::symlink_metadata(output_dir)
        .map_err(|error| format!("cannot inspect DSV discovery output: {error}"))?;
    if is_reparse_point(&metadata) || !metadata.is_dir() {
        return Err("D3D11 DSV discovery output is not a plain directory".into());
    }
    let canonical_output = output_dir
        .canonicalize()
        .map_err(|error| format!("cannot resolve DSV discovery output: {error}"))?;
    if canonical_output.parent() != Some(canonical_root.as_path()) {
        return Err("D3D11 DSV discovery output escaped evidence_root before write".into());
    }
    Ok(canonical_output)
}

pub fn write_report_create_new(
    output_dir: &Path,
    report: &DsvDiscoveryReport,
) -> Result<PathBuf, String> {
    write_report_create_new_inner(output_dir, report, false)
}

fn write_report_create_new_inner(
    output_dir: &Path,
    report: &DsvDiscoveryReport,
    fail_before_commit: bool,
) -> Result<PathBuf, String> {
    report.validate()?;
    reject_reparse_chain(output_dir, "report output directory")?;
    let canonical_output = output_dir
        .canonicalize()
        .map_err(|error| format!("cannot resolve DSV report output directory: {error}"))?;
    let output_metadata = fs::symlink_metadata(&canonical_output)
        .map_err(|error| format!("cannot inspect DSV report output directory: {error}"))?;
    if is_reparse_point(&output_metadata) || !output_metadata.is_dir() {
        return Err("D3D11 DSV report output is not a plain directory".into());
    }
    let mut bytes = serde_json::to_vec_pretty(report)
        .map_err(|error| format!("cannot serialize DSV discovery report: {error}"))?;
    bytes.push(b'\n');
    if bytes.len() as u64 > MAX_DSV_DISCOVERY_REPORT_BYTES {
        return Err("serialized DSV discovery report exceeds its byte bound".into());
    }

    // Pin the exact directory object before the random staging file is created. On Windows the
    // handle excludes delete sharing, so the directory cannot be renamed out from underneath the
    // handle-relative commit. Unix keeps a directory fd for its relative no-replace operation.
    let output_handle = open_pinned_report_directory(&canonical_output)?;
    let final_path = canonical_output.join(DSV_DISCOVERY_REPORT_FILE_NAME);
    let pending_leaf = random_report_pending_leaf()?;
    let pending_path = canonical_output.join(&pending_leaf);
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(windows)]
    {
        // `create_new` plus a share-none handle carrying READ/WRITE/DELETE is the ownership
        // boundary. Cleanup and commit both operate on this file object, never on an untrusted
        // path that merely happens to have the same random name.
        const GENERIC_READ: u32 = 0x8000_0000;
        const GENERIC_WRITE: u32 = 0x4000_0000;
        const DELETE: u32 = 0x0001_0000;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options
            .access_mode(GENERIC_READ | GENERIC_WRITE | DELETE)
            .share_mode(0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let mut pending_file = options.open(&pending_path).map_err(|error| {
        format!(
            "cannot create pending DSV discovery report {}: {error}",
            pending_path.display()
        )
    })?;

    let prepared = (|| -> Result<(), String> {
        let metadata = pending_file
            .metadata()
            .map_err(|error| format!("cannot inspect pending DSV discovery report: {error}"))?;
        if is_reparse_point(&metadata) || !metadata.is_file() {
            return Err("pending D3D11 DSV discovery report is not a plain file".into());
        }
        pending_file
            .write_all(&bytes)
            .and_then(|()| pending_file.sync_all())
            .and_then(|()| pending_file.seek(SeekFrom::Start(0)).map(|_| ()))
            .map_err(|error| format!("cannot seal pending DSV discovery report: {error}"))?;
        let mut verified = 0usize;
        let mut readback = [0u8; 64 * 1024];
        loop {
            let read = pending_file.read(&mut readback).map_err(|error| {
                format!("cannot read back pending DSV discovery report: {error}")
            })?;
            if read == 0 {
                break;
            }
            let end = verified
                .checked_add(read)
                .ok_or("pending DSV discovery report readback length overflow")?;
            if end > bytes.len() || readback[..read] != bytes[verified..end] {
                return Err("pending D3D11 DSV discovery report changed before commit".into());
            }
            verified = end;
        }
        let sealed_metadata = pending_file
            .metadata()
            .map_err(|error| format!("cannot re-inspect pending DSV discovery report: {error}"))?;
        if verified != bytes.len() || sealed_metadata.len() != bytes.len() as u64 {
            return Err("pending D3D11 DSV discovery report changed before commit".into());
        }
        if fail_before_commit {
            return Err("injected D3D11 DSV report pre-commit failure".into());
        }
        commit_open_report_no_replace(
            &pending_file,
            &output_handle,
            &pending_leaf,
            DSV_DISCOVERY_REPORT_FILE_NAME,
            &pending_path,
            &final_path,
        )
    })();

    if let Err(error) = prepared {
        // Failure cleanup is tied to the still-open owned object. Windows sets disposition on the
        // handle itself; Unix uses the pinned parent fd/random leaf after checking the open file's
        // identity. A cleanup failure is surfaced and the success-shaped final was never created.
        return match discard_owned_pending_report(
            &pending_file,
            &output_handle,
            &pending_leaf,
            &pending_path,
        ) {
            Ok(()) => Err(error),
            Err(cleanup) => Err(format!(
                "{error}; cannot discard owned pending report: {cleanup}"
            )),
        };
    }

    // The handle-relative no-replace operation above is the commit point. There are deliberately
    // no fallible operations after it: callers cannot observe Err with a success-shaped final.
    Ok(final_path)
}

fn random_report_pending_leaf() -> Result<String, String> {
    let mut random = [0u8; 16];
    fill_report_random(&mut random)?;
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut leaf = String::with_capacity(32 + random.len() * 2);
    leaf.push_str(".dm-dsv-report-pending-");
    for byte in random {
        leaf.push(HEX[(byte >> 4) as usize] as char);
        leaf.push(HEX[(byte & 0x0f) as usize] as char);
    }
    Ok(leaf)
}

#[cfg(windows)]
fn fill_report_random(output: &mut [u8]) -> Result<(), String> {
    use std::ffi::c_void;

    #[link(name = "bcrypt")]
    unsafe extern "system" {
        fn BCryptGenRandom(algorithm: *mut c_void, buffer: *mut u8, length: u32, flags: u32)
        -> i32;
    }
    const BCRYPT_USE_SYSTEM_PREFERRED_RNG: u32 = 0x0000_0002;
    let length = u32::try_from(output.len())
        .map_err(|_| "DSV report random pending token is too long".to_string())?;
    // SAFETY: the system-preferred RNG accepts a null algorithm handle and the mutable slice is
    // valid for exactly `length` bytes for the duration of the synchronous call.
    let status = unsafe {
        BCryptGenRandom(
            std::ptr::null_mut(),
            output.as_mut_ptr(),
            length,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if status < 0 {
        return Err(format!(
            "cannot generate DSV report pending token (NTSTATUS=0x{:08x})",
            status as u32
        ));
    }
    Ok(())
}

#[cfg(not(windows))]
fn fill_report_random(output: &mut [u8]) -> Result<(), String> {
    fs::File::open("/dev/urandom")
        .and_then(|mut random| random.read_exact(output))
        .map_err(|error| format!("cannot generate DSV report pending token: {error}"))
}

#[cfg(windows)]
fn open_pinned_report_directory(directory: &Path) -> Result<fs::File, String> {
    use std::ffi::c_void;

    #[repr(C)]
    struct FileAttributeTagInformation {
        file_attributes: u32,
        reparse_tag: u32,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetFileInformationByHandleEx(
            file: *mut c_void,
            information_class: u32,
            information: *mut c_void,
            size: u32,
        ) -> i32;
    }
    const GENERIC_READ: u32 = 0x8000_0000;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    const FILE_ATTRIBUTE_TAG_INFO_CLASS: u32 = 9;
    let file = OpenOptions::new()
        .access_mode(GENERIC_READ | GENERIC_WRITE)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(directory)
        .map_err(|error| {
            format!(
                "cannot pin DSV report output directory {}: {error}",
                directory.display()
            )
        })?;
    let mut tag = FileAttributeTagInformation {
        file_attributes: 0,
        reparse_tag: 0,
    };
    // SAFETY: `file` is a live directory handle opened with OPEN_REPARSE_POINT, and `tag` is an
    // exactly sized writable FILE_ATTRIBUTE_TAG_INFO buffer. This handle query, rather than a
    // second path lookup, proves that the pinned object is a directory and not a junction/symlink.
    let queried = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FILE_ATTRIBUTE_TAG_INFO_CLASS,
            (&mut tag as *mut FileAttributeTagInformation).cast(),
            u32::try_from(std::mem::size_of::<FileAttributeTagInformation>())
                .map_err(|_| "DSV report directory tag buffer is too large")?,
        )
    };
    if queried == 0 {
        return Err(format!(
            "cannot query pinned DSV report directory FileAttributeTagInfo: {}",
            std::io::Error::last_os_error()
        ));
    }
    if tag.file_attributes & FILE_ATTRIBUTE_DIRECTORY == 0
        || tag.file_attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || tag.reparse_tag != 0
    {
        return Err("pinned DSV report output is not a plain directory".into());
    }
    Ok(file)
}

#[cfg(not(windows))]
fn open_pinned_report_directory(directory: &Path) -> Result<fs::File, String> {
    fs::File::open(directory)
        .map_err(|error| format!("cannot pin DSV report output directory: {error}"))
}

#[cfg(windows)]
fn commit_open_report_no_replace(
    pending_file: &fs::File,
    output_directory: &fs::File,
    _pending_leaf: &str,
    final_leaf: &str,
    _pending_path: &Path,
    _final_path: &Path,
) -> Result<(), String> {
    use std::ffi::{OsStr, c_void};
    use std::os::windows::ffi::OsStrExt as _;

    #[repr(C)]
    union RenameMode {
        replace_if_exists: u8,
        flags: u32,
    }
    #[repr(C)]
    struct RenameInformation {
        mode: RenameMode,
        root_directory: *mut c_void,
        file_name_length: u32,
        file_name: [u16; 1],
    }
    #[repr(C)]
    struct IoStatusBlock {
        status_or_pointer: usize,
        information: usize,
    }
    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn NtSetInformationFile(
            file: *mut c_void,
            io_status: *mut IoStatusBlock,
            information: *mut c_void,
            length: u32,
            information_class: u32,
        ) -> i32;
    }
    const FILE_RENAME_INFORMATION_CLASS: u32 = 10;

    let name = OsStr::new(final_leaf).encode_wide().collect::<Vec<_>>();
    let name_bytes = name
        .len()
        .checked_mul(std::mem::size_of::<u16>())
        .and_then(|length| u32::try_from(length).ok())
        .ok_or("DSV report target leaf is too long")?;
    let header_bytes = std::mem::offset_of!(RenameInformation, file_name);
    let buffer_bytes = header_bytes
        .checked_add(name_bytes as usize)
        .ok_or("DSV report rename buffer overflow")?;
    let words = buffer_bytes
        .checked_add(std::mem::size_of::<usize>() - 1)
        .ok_or("DSV report rename buffer overflow")?
        / std::mem::size_of::<usize>();
    let mut buffer = vec![0usize; words];
    let information = buffer.as_mut_ptr().cast::<RenameInformation>();
    let mut io_status = IoStatusBlock {
        status_or_pointer: 0,
        information: 0,
    };
    // SAFETY: the pending file was opened with DELETE access and stays share-none/open through
    // this call. `buffer` is pointer-aligned and sized for the flexible UTF-16 leaf payload; the
    // pinned directory handle remains valid and excludes delete sharing.
    let status = unsafe {
        (*information).mode.flags = 0;
        (*information).root_directory = output_directory.as_raw_handle();
        (*information).file_name_length = name_bytes;
        std::ptr::copy_nonoverlapping(
            name.as_ptr(),
            (*information).file_name.as_mut_ptr(),
            name.len(),
        );
        NtSetInformationFile(
            pending_file.as_raw_handle(),
            &mut io_status,
            information.cast(),
            u32::try_from(buffer_bytes).map_err(|_| "DSV report rename buffer is too large")?,
            FILE_RENAME_INFORMATION_CLASS,
        )
    };
    if status < 0 {
        return Err(format!(
            "cannot atomically publish DSV discovery report (NTSTATUS=0x{:08x})",
            status as u32
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn discard_owned_pending_report(
    pending_file: &fs::File,
    _output_directory: &fs::File,
    _pending_leaf: &str,
    _pending_path: &Path,
) -> Result<(), String> {
    use std::ffi::c_void;

    #[repr(C)]
    struct DispositionInformation {
        delete_file: u8,
    }
    #[repr(C)]
    struct IoStatusBlock {
        status_or_pointer: usize,
        information: usize,
    }
    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn NtSetInformationFile(
            file: *mut c_void,
            io_status: *mut IoStatusBlock,
            information: *mut c_void,
            length: u32,
            information_class: u32,
        ) -> i32;
    }
    const FILE_DISPOSITION_INFORMATION_CLASS: u32 = 13;
    let mut disposition = DispositionInformation { delete_file: 1 };
    let mut io_status = IoStatusBlock {
        status_or_pointer: 0,
        information: 0,
    };
    // SAFETY: DELETE access belongs to this still-open share-none staging handle. Setting its
    // disposition cannot delete a different path-replacement object.
    let status = unsafe {
        NtSetInformationFile(
            pending_file.as_raw_handle(),
            &mut io_status,
            (&mut disposition as *mut DispositionInformation).cast(),
            u32::try_from(std::mem::size_of::<DispositionInformation>())
                .map_err(|_| "DSV report disposition buffer is too large")?,
            FILE_DISPOSITION_INFORMATION_CLASS,
        )
    };
    if status < 0 {
        return Err(format!(
            "cannot mark owned pending report for deletion (NTSTATUS=0x{:08x})",
            status as u32
        ));
    }
    Ok(())
}

#[cfg(not(windows))]
fn commit_open_report_no_replace(
    _pending_file: &fs::File,
    _output_directory: &fs::File,
    _pending_leaf: &str,
    _final_leaf: &str,
    pending_path: &Path,
    final_path: &Path,
) -> Result<(), String> {
    fs::hard_link(pending_path, final_path)
        .map_err(|error| format!("cannot atomically claim DSV discovery report: {error}"))?;
    // The hard link is the no-replace commit point. Cleanup is best-effort and cannot turn a
    // visible success-shaped final report into Err.
    let _ = fs::remove_file(pending_path);
    Ok(())
}

#[cfg(not(windows))]
fn discard_owned_pending_report(
    pending_file: &fs::File,
    _output_directory: &fs::File,
    _pending_leaf: &str,
    pending_path: &Path,
) -> Result<(), String> {
    let opened = pending_file
        .metadata()
        .map_err(|error| format!("cannot inspect owned pending report: {error}"))?;
    let named = fs::symlink_metadata(pending_path)
        .map_err(|error| format!("cannot inspect named pending report: {error}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if opened.dev() != named.dev() || opened.ino() != named.ino() {
            return Err("pending report name no longer identifies the owned file".into());
        }
    }
    fs::remove_file(pending_path)
        .map_err(|error| format!("cannot remove identity-checked pending report: {error}"))
}

/// Revalidate the root boundary immediately before the create-new report open. Runtime and
/// offline paths use this entry instead of trusting the directory path retained at activation.
pub fn write_report_create_new_under_root(
    evidence_root: &Path,
    output_dir: &Path,
    report: &DsvDiscoveryReport,
) -> Result<PathBuf, String> {
    let output_dir = validate_existing_evidence_directory(evidence_root, output_dir)?;
    write_report_create_new(&output_dir, report)
}

pub fn read_report_bounded(
    input: &Path,
    evidence_root: &Path,
) -> Result<DsvDiscoveryReport, String> {
    read_report_with_bytes_bounded(input, evidence_root).map(|(report, _)| report)
}

fn read_report_with_bytes_bounded(
    input: &Path,
    evidence_root: &Path,
) -> Result<(DsvDiscoveryReport, Vec<u8>), String> {
    reject_reparse_chain(evidence_root, "evidence root")?;
    reject_reparse_chain(input, "input report")?;
    let root = evidence_root
        .canonicalize()
        .map_err(|error| format!("cannot resolve DSV evidence root: {error}"))?;
    let metadata = fs::symlink_metadata(input)
        .map_err(|error| format!("cannot inspect DSV discovery report: {error}"))?;
    if is_reparse_point(&metadata)
        || !metadata.is_file()
        || metadata.len() > MAX_DSV_DISCOVERY_REPORT_BYTES
    {
        return Err("DSV discovery input must be a bounded, non-symlink regular file".into());
    }
    let input = input
        .canonicalize()
        .map_err(|error| format!("cannot resolve DSV discovery input: {error}"))?;
    if !input.starts_with(&root) {
        return Err("DSV discovery input escaped evidence_root".into());
    }
    let file = fs::File::open(&input)
        .map_err(|error| format!("cannot open DSV discovery report: {error}"))?;
    let opened_metadata = file
        .metadata()
        .map_err(|error| format!("cannot inspect opened DSV discovery report: {error}"))?;
    if !opened_metadata.is_file() || opened_metadata.len() > MAX_DSV_DISCOVERY_REPORT_BYTES {
        return Err("opened DSV discovery input exceeded its regular-file bound".into());
    }
    let mut bytes = Vec::with_capacity(opened_metadata.len() as usize);
    file.take(MAX_DSV_DISCOVERY_REPORT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("cannot read DSV discovery report: {error}"))?;
    if bytes.len() as u64 > MAX_DSV_DISCOVERY_REPORT_BYTES {
        return Err("DSV discovery input grew beyond its byte bound while reading".into());
    }
    let report: DsvDiscoveryReport = serde_json::from_slice(&bytes)
        .map_err(|error| format!("cannot parse DSV discovery report: {error}"))?;
    report.validate()?;
    Ok((report, bytes))
}

/// Hash one validated v2/v3 report and materialize the review-only pass histogram for a single
/// run-local candidate. Both files use the same in-memory bytes, so a concurrent path replacement
/// cannot make the summary claim a digest for different parsed content.
#[cfg(feature = "evidence-authoring")]
pub fn materialize_dsv_candidate_pass_summary(
    input: &Path,
    evidence_root: &Path,
    output_dir: &Path,
    candidate_id: &str,
) -> Result<PathBuf, String> {
    let (report, bytes) = read_report_with_bytes_bounded(input, evidence_root)?;
    let digest = format!("sha256:{:x}", Sha256::digest(&bytes));
    let summary = summarize_dsv_candidate_passes(&report, digest, candidate_id)?;
    let output_dir = create_new_evidence_directory(evidence_root, output_dir)?;
    let canonical_output = validate_existing_evidence_directory(evidence_root, &output_dir)?;
    let path = output_dir.join(DSV_DISCOVERY_CANDIDATE_PASS_SUMMARY_FILE_NAME);
    let mut serialized = serde_json::to_vec_pretty(&summary)
        .map_err(|error| format!("cannot serialize DSV candidate pass summary: {error}"))?;
    serialized.push(b'\n');
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|error| format!("cannot create DSV candidate pass summary: {error}"))?;
    let opened_path = path
        .canonicalize()
        .map_err(|error| format!("cannot resolve new DSV candidate pass summary: {error}"))?;
    if opened_path.parent() != Some(canonical_output.as_path()) {
        return Err("new D3D11 DSV candidate pass summary escaped its checked directory".into());
    }
    let mut writer = std::io::BufWriter::new(file);
    writer
        .write_all(&serialized)
        .and_then(|()| writer.flush())
        .and_then(|()| writer.get_ref().sync_all())
        .map_err(|error| format!("cannot seal DSV candidate pass summary: {error}"))?;
    let sealed_path = path
        .canonicalize()
        .map_err(|error| format!("cannot re-resolve sealed DSV candidate pass summary: {error}"))?;
    if sealed_path != opened_path {
        return Err("D3D11 DSV candidate pass summary path changed while sealing".into());
    }
    Ok(opened_path)
}

/// Validate a probe report and write a normalized review copy into a new directory. This function
/// has no authoring path and cannot create profiles or bindings.
pub fn materialize_validated_report(
    input: &Path,
    evidence_root: &Path,
    output_dir: &Path,
) -> Result<PathBuf, String> {
    let report = read_report_bounded(input, evidence_root)?;
    let output_dir = create_new_evidence_directory(evidence_root, output_dir)?;
    write_report_create_new_under_root(evidence_root, &output_dir, &report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> PathBuf {
        static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "dm-dsv-discovery-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn config(root: &Path) -> DsvDiscoveryConfig {
        DsvDiscoveryConfig {
            schema: DSV_DISCOVERY_CONFIG_SCHEMA_V2.into(),
            discovery_id: "local-run-1".into(),
            executable_sha256: format!("sha256:{}", "a".repeat(64)),
            evidence_root: root.display().to_string(),
            limits: DsvDiscoveryLimits {
                max_frames: 2,
                max_candidates: 2,
                max_candidates_per_frame: 2,
            },
        }
    }

    fn candidate(id: &str, width: u32) -> DsvDiscoveryCandidateObservation {
        DsvDiscoveryCandidateObservation {
            candidate_id: id.into(),
            descriptor: DsvDiscoveryDescriptor {
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
            },
            bind_count: 1,
            unbind_count: 1,
            pass_count: 1,
            draw_calls: 50,
            clear_count: 1,
            completed_passes: vec![DsvDiscoveryCompletedPassObservation(50, 1, false)],
            dropped_completed_passes: 0,
            first_bound_order: Some(1),
            last_bound_order: Some(1),
            carried_from_previous_present: false,
        }
    }

    fn candidate_at(id: &str, width: u32, order: u64) -> DsvDiscoveryCandidateObservation {
        let mut candidate = candidate(id, width);
        candidate.first_bound_order = Some(order);
        candidate.last_bound_order = Some(order);
        candidate
    }

    fn one_frame_report(root: &Path) -> DsvDiscoveryReport {
        let mut builder = DsvDiscoveryBuilder::new(config(root)).unwrap();
        builder.observe_frame(DsvDiscoveryFrameObservation {
            present_seq: 1,
            candidates: vec![candidate("dsv-0001", 1920)],
            ambiguous_descriptor_groups: Vec::new(),
        });
        builder
            .finish(DsvDiscoveryRuntimeCounters::default())
            .unwrap()
    }

    fn report_pending_entries(output: &Path) -> Vec<PathBuf> {
        fs::read_dir(output)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(".dm-dsv-report-pending-"))
            })
            .collect()
    }

    #[test]
    fn discovery_report_can_never_claim_ready_or_authoring() {
        let root = root();
        let mut builder = DsvDiscoveryBuilder::new(config(&root)).unwrap();
        builder.observe_frame(DsvDiscoveryFrameObservation {
            present_seq: 1,
            candidates: vec![candidate("dsv-0001", 1920)],
            ambiguous_descriptor_groups: Vec::new(),
        });
        let report = builder
            .finish(DsvDiscoveryRuntimeCounters::default())
            .unwrap();
        report.validate().unwrap();
        assert!(report.complete);
        assert!(!report.recording_ready);
        assert!(!report.deliverable);
        assert!(!report.profile_generated);
        assert!(!report.binding_generated);
        let mut weakened = report;
        weakened.recording_ready = true;
        assert!(weakened.validate().is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn retired_config_v1_cannot_start_a_pass_incomplete_run() {
        let root = root();
        let mut legacy = config(&root);
        legacy.schema = DSV_DISCOVERY_CONFIG_SCHEMA_V1.into();
        assert!(DsvDiscoveryBuilder::new(legacy).is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn v3_report_byte_bound_covers_the_maximum_pretty_json_shape() {
        let root = root();
        let mut maximum_config = config(&root);
        maximum_config.limits = DsvDiscoveryLimits {
            max_frames: MAX_DSV_DISCOVERY_FRAMES,
            max_candidates: MAX_DSV_DISCOVERY_CANDIDATES,
            max_candidates_per_frame: MAX_DSV_DISCOVERY_CANDIDATES_PER_FRAME,
        };
        let mut builder = DsvDiscoveryBuilder::new(maximum_config).unwrap();
        let candidates = (1..=MAX_DSV_DISCOVERY_CANDIDATES_PER_FRAME)
            .map(|ordinal| {
                let mut row = candidate(&format!("dsv-{ordinal:04}"), u32::MAX);
                row.descriptor = DsvDiscoveryDescriptor {
                    view_format: i32::MAX,
                    view_dimension: 3,
                    view_flags: 3,
                    mip_slice: u32::MAX - 1,
                    first_array_slice: 0,
                    view_array_size: u32::MAX,
                    resource_format: i32::MAX,
                    width: u32::MAX,
                    height: u32::MAX,
                    resource_mip_levels: u32::MAX,
                    resource_array_size: u32::MAX,
                    sample_count: 1,
                    sample_quality: u32::MAX,
                    usage: i32::MAX,
                    bind_flags: u32::MAX,
                    cpu_access_flags: u32::MAX,
                    misc_flags: u32::MAX,
                };
                row.bind_count = 12;
                row.unbind_count = 12;
                row.pass_count = 12;
                row.draw_calls = u64::MAX;
                row.clear_count = u32::MAX;
                row.completed_passes = vec![
                        DsvDiscoveryCompletedPassObservation(u64::MAX, u32::MAX, false,);
                        MAX_DSV_DISCOVERY_COMPLETED_PASSES_PER_CANDIDATE_PER_FRAME
                    ];
                row.first_bound_order = Some(u64::from((ordinal - 1) * 12 + 1));
                row.last_bound_order = Some(u64::from(ordinal * 12));
                row
            })
            .collect();
        builder.observe_frame(DsvDiscoveryFrameObservation {
            present_seq: u64::MAX,
            candidates,
            ambiguous_descriptor_groups: Vec::new(),
        });
        let report = builder
            .finish(DsvDiscoveryRuntimeCounters::default())
            .unwrap();
        report.validate().unwrap();

        // A one-frame report has the same nested indentation as every frame in a full report.
        // Repeating the entire document (including its top-level envelope) is deliberately more
        // expensive than serializing one envelope around 1,200 frames.
        let one_frame_bytes =
            u64::try_from(serde_json::to_vec_pretty(&report).unwrap().len()).unwrap() + 1;
        let conservative_full_report_bytes = one_frame_bytes * u64::from(MAX_DSV_DISCOVERY_FRAMES);
        assert!(conservative_full_report_bytes > 128 * 1024 * 1024);
        assert!(conservative_full_report_bytes <= MAX_DSV_DISCOVERY_REPORT_BYTES);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn completed_unbinds_cannot_exceed_logical_passes_in_any_report_schema() {
        let mut forged = candidate("dsv-0001", 1920);
        forged.bind_count = 2;
        forged.unbind_count = 2;
        forged.pass_count = 1;
        forged.completed_passes = vec![
            DsvDiscoveryCompletedPassObservation(10, 0, false),
            DsvDiscoveryCompletedPassObservation(20, 0, false),
        ];
        forged.last_bound_order = Some(2);
        assert!(validate_candidate(&forged, DSV_DISCOVERY_REPORT_SCHEMA_V2).is_err());

        forged.completed_passes.clear();
        assert!(validate_candidate(&forged, DSV_DISCOVERY_REPORT_SCHEMA_V1).is_err());
    }

    #[test]
    fn report_cannot_hide_builder_loss_by_removing_limit_blockers() {
        let root = root();
        let mut row_loss_builder = DsvDiscoveryBuilder::new(config(&root)).unwrap();
        row_loss_builder.observe_frame(DsvDiscoveryFrameObservation {
            present_seq: 1,
            candidates: vec![
                candidate_at("dsv-0001", 1920, 1),
                candidate_at("dsv-0002", 1280, 2),
                candidate_at("dsv-0003", 640, 3),
            ],
            ambiguous_descriptor_groups: Vec::new(),
        });
        let mut report = row_loss_builder
            .finish(DsvDiscoveryRuntimeCounters::default())
            .unwrap();
        assert!(
            report
                .blockers
                .contains(&DsvDiscoveryBlocker::PerFrameCandidateLimitReached)
        );
        report.blockers.clear();
        report.complete = true;
        assert!(report.validate().is_err());

        let mut frame_loss_builder = DsvDiscoveryBuilder::new(config(&root)).unwrap();
        for present_seq in 1..=3 {
            frame_loss_builder.observe_frame(DsvDiscoveryFrameObservation {
                present_seq,
                candidates: vec![candidate("dsv-0001", 1920)],
                ambiguous_descriptor_groups: Vec::new(),
            });
        }
        let mut frame_loss = frame_loss_builder
            .finish(DsvDiscoveryRuntimeCounters::default())
            .unwrap();
        assert_eq!(frame_loss.summary.runtime.queue_dropped_frames, 0);
        assert_eq!(frame_loss.summary.dropped_frames, 1);
        assert!(
            frame_loss
                .blockers
                .contains(&DsvDiscoveryBlocker::FrameLimitReached)
        );
        frame_loss.blockers.clear();
        frame_loss.complete = true;
        assert!(frame_loss.validate().is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn report_rejects_forged_bind_and_pass_ledgers() {
        let root = root();
        let mut builder = DsvDiscoveryBuilder::new(config(&root)).unwrap();
        builder.observe_frame(DsvDiscoveryFrameObservation {
            present_seq: 1,
            candidates: vec![candidate("dsv-0001", 1920)],
            ambiguous_descriptor_groups: Vec::new(),
        });
        let report = builder
            .finish(DsvDiscoveryRuntimeCounters::default())
            .unwrap();

        let mut zero_bind_with_order = report.clone();
        zero_bind_with_order.frames[0].candidates[0].bind_count = 0;
        assert!(zero_bind_with_order.validate().is_err());

        let mut draw_without_pass = report;
        draw_without_pass.frames[0].candidates[0].pass_count = 0;
        assert!(draw_without_pass.validate().is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn v2_pass_summary_exposes_every_ordinal_without_selecting_one() {
        let root = root();
        let mut builder = DsvDiscoveryBuilder::new(config(&root)).unwrap();
        let mut first = candidate("dsv-0001", 1920);
        first.bind_count = 2;
        first.unbind_count = 2;
        first.pass_count = 2;
        first.draw_calls = 30;
        first.last_bound_order = Some(2);
        first.completed_passes = vec![
            DsvDiscoveryCompletedPassObservation(10, 1, false),
            DsvDiscoveryCompletedPassObservation(20, 0, false),
        ];
        builder.observe_frame(DsvDiscoveryFrameObservation {
            present_seq: 1,
            candidates: vec![first],
            ambiguous_descriptor_groups: Vec::new(),
        });
        let mut second = candidate("dsv-0001", 1920);
        second.draw_calls = 30;
        second.carried_from_previous_present = true;
        second.completed_passes = vec![DsvDiscoveryCompletedPassObservation(30, 1, true)];
        builder.observe_frame(DsvDiscoveryFrameObservation {
            present_seq: 2,
            candidates: vec![second],
            ambiguous_descriptor_groups: Vec::new(),
        });
        let report = builder
            .finish(DsvDiscoveryRuntimeCounters::default())
            .unwrap();
        let summary = summarize_dsv_candidate_passes(
            &report,
            format!("sha256:{}", "b".repeat(64)),
            "dsv-0001",
        )
        .unwrap();
        assert_eq!(summary.observed_frames, 2);
        assert_eq!(summary.completed_passes, 3);
        assert_eq!(summary.ordinal_stats.len(), 2);
        assert_eq!(summary.ordinal_stats[0].unbind_ordinal_from_end, 1);
        assert_eq!(summary.ordinal_stats[0].observed_passes, 2);
        assert_eq!(summary.ordinal_stats[0].total_draw_calls, 50);
        assert_eq!(summary.ordinal_stats[0].passes_with_clear, 1);
        assert_eq!(summary.ordinal_stats[0].carried_from_previous_present, 1);
        assert_eq!(summary.ordinal_stats[1].unbind_ordinal_from_end, 2);
        assert_eq!(summary.ordinal_stats[1].total_draw_calls, 10);
        assert!(!summary.candidate_selected);
        assert!(!summary.unbind_ordinal_selected);
        assert!(!summary.recording_ready);
        assert!(!summary.profile_generated);
        summary.validate().unwrap();

        let mut forged = summary;
        forged.unbind_ordinal_selected = true;
        assert!(forged.validate().is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn completed_pass_overflow_is_a_durable_blocker() {
        let root = root();
        let mut builder = DsvDiscoveryBuilder::new(config(&root)).unwrap();
        let mut row = candidate("dsv-0001", 1920);
        row.bind_count = 13;
        row.unbind_count = 13;
        row.pass_count = 13;
        row.first_bound_order = Some(1);
        row.last_bound_order = Some(13);
        row.completed_passes = (0..12)
            .map(|draws| DsvDiscoveryCompletedPassObservation(draws, 1, false))
            .collect();
        row.dropped_completed_passes = 1;
        builder.observe_frame(DsvDiscoveryFrameObservation {
            present_seq: 1,
            candidates: vec![row],
            ambiguous_descriptor_groups: Vec::new(),
        });
        let report = builder
            .finish(DsvDiscoveryRuntimeCounters {
                pass_ledger_dropped_rows: 1,
                ..DsvDiscoveryRuntimeCounters::default()
            })
            .unwrap();
        assert!(!report.complete);
        assert!(
            report
                .blockers
                .contains(&DsvDiscoveryBlocker::PassLedgerLimitReached)
        );
        report.validate().unwrap();
        assert!(
            summarize_dsv_candidate_passes(
                &report,
                format!("sha256:{}", "b".repeat(64)),
                "dsv-0001"
            )
            .is_err()
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn historical_v1_report_cannot_masquerade_as_pass_authoring_evidence() {
        let root = root();
        let mut builder = DsvDiscoveryBuilder::new(config(&root)).unwrap();
        builder.observe_frame(DsvDiscoveryFrameObservation {
            present_seq: 1,
            candidates: vec![candidate("dsv-0001", 1920)],
            ambiguous_descriptor_groups: Vec::new(),
        });
        let mut report = builder
            .finish(DsvDiscoveryRuntimeCounters::default())
            .unwrap();
        report.schema = DSV_DISCOVERY_REPORT_SCHEMA_V1.into();
        report.summary.runtime.hook_integrity_failure_reasons = None;
        report.frames[0].candidates[0].completed_passes.clear();
        report.validate().unwrap();
        assert!(
            summarize_dsv_candidate_passes(
                &report,
                format!("sha256:{}", "b".repeat(64)),
                "dsv-0001"
            )
            .is_err()
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn an_empty_window_is_explicitly_incomplete() {
        let root = root();
        let report = DsvDiscoveryBuilder::new(config(&root))
            .unwrap()
            .finish(DsvDiscoveryRuntimeCounters::default())
            .unwrap();
        assert!(!report.complete);
        assert!(
            report
                .blockers
                .contains(&DsvDiscoveryBlocker::NoFramesObserved)
        );
        assert!(
            report
                .blockers
                .contains(&DsvDiscoveryBlocker::NoCandidatesObserved)
        );
        report.validate().unwrap();
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn ambiguity_and_all_loss_counters_are_durable_blockers() {
        let root = root();
        let mut builder = DsvDiscoveryBuilder::new(config(&root)).unwrap();
        builder.observe_frame(DsvDiscoveryFrameObservation {
            present_seq: 1,
            candidates: vec![
                candidate_at("dsv-0001", 1920, 1),
                candidate_at("dsv-0002", 1920, 2),
            ],
            ambiguous_descriptor_groups: Vec::new(),
        });
        let report = builder
            .finish(DsvDiscoveryRuntimeCounters {
                queue_dropped_frames: 1,
                hook_dropped_events: 2,
                hook_integrity_failures: 3,
                hook_integrity_failure_reasons: Some(DsvDiscoveryHookIntegrityFailureReasons {
                    tracker_failure: 3,
                    ..DsvDiscoveryHookIntegrityFailureReasons::default()
                }),
                descriptor_identity_conflicts: 4,
                candidate_limit_events: 5,
                ..DsvDiscoveryRuntimeCounters::default()
            })
            .unwrap();
        assert!(!report.complete);
        assert!(
            report
                .blockers
                .contains(&DsvDiscoveryBlocker::AmbiguousDescriptorIdentity)
        );
        assert!(
            report
                .blockers
                .contains(&DsvDiscoveryBlocker::QueueSaturated)
        );
        assert!(
            report
                .blockers
                .contains(&DsvDiscoveryBlocker::HookObservationDropped)
        );
        assert!(
            report
                .blockers
                .contains(&DsvDiscoveryBlocker::HookIntegrityFailure)
        );
        assert!(
            report
                .blockers
                .contains(&DsvDiscoveryBlocker::DescriptorIdentityConflict)
        );
        assert!(
            report
                .blockers
                .contains(&DsvDiscoveryBlocker::CandidateLimitReached)
        );
        report.validate().unwrap();
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn v3_hook_integrity_reason_ledger_is_exhaustive_and_exact() {
        let root = root();
        let reasons = DsvDiscoveryHookIntegrityFailureReasons {
            unsupported_execute_command_list: 1,
            vtable_or_slot_drift: 2,
            context_drift: 3,
            tracker_failure: 4,
            lifecycle_or_queue_failure: 5,
        };
        let report = DsvDiscoveryBuilder::new(config(&root))
            .unwrap()
            .finish(DsvDiscoveryRuntimeCounters {
                hook_integrity_failures: 15,
                hook_integrity_failure_reasons: Some(reasons),
                ..DsvDiscoveryRuntimeCounters::default()
            })
            .unwrap();
        assert_eq!(report.schema, DSV_DISCOVERY_REPORT_SCHEMA_V3);
        assert_eq!(
            report.summary.runtime.hook_integrity_failure_reasons,
            Some(reasons)
        );
        report.validate().unwrap();

        let mut aggregate_mismatch = report.clone();
        aggregate_mismatch.summary.runtime.hook_integrity_failures = 14;
        assert!(aggregate_mismatch.validate().is_err());

        let mut reason_mismatch = report.clone();
        reason_mismatch
            .summary
            .runtime
            .hook_integrity_failure_reasons
            .as_mut()
            .unwrap()
            .tracker_failure = 5;
        assert!(reason_mismatch.validate().is_err());

        let mut overflowed_reason_sum = report.clone();
        overflowed_reason_sum
            .summary
            .runtime
            .hook_integrity_failures = u64::MAX;
        overflowed_reason_sum
            .summary
            .runtime
            .hook_integrity_failure_reasons = Some(DsvDiscoveryHookIntegrityFailureReasons {
            unsupported_execute_command_list: u64::MAX,
            tracker_failure: 1,
            ..DsvDiscoveryHookIntegrityFailureReasons::default()
        });
        assert!(overflowed_reason_sum.validate().is_err());

        let mut missing_ledger = serde_json::to_value(&report).unwrap();
        missing_ledger["summary"]
            .as_object_mut()
            .unwrap()
            .remove("hook_integrity_failure_reasons");
        let missing_ledger: DsvDiscoveryReport = serde_json::from_value(missing_ledger).unwrap();
        assert!(missing_ledger.validate().is_err());

        for missing_reason in [
            "unsupported_execute_command_list",
            "vtable_or_slot_drift",
            "context_drift",
            "tracker_failure",
            "lifecycle_or_queue_failure",
        ] {
            let mut missing_reason_value = serde_json::to_value(&report).unwrap();
            missing_reason_value["summary"]["hook_integrity_failure_reasons"]
                .as_object_mut()
                .unwrap()
                .remove(missing_reason);
            assert!(
                serde_json::from_value::<DsvDiscoveryReport>(missing_reason_value).is_err(),
                "v3 reason ledger accepted missing field {missing_reason}"
            );
        }

        let mut unknown_reason = serde_json::to_value(&report).unwrap();
        unknown_reason["summary"]["hook_integrity_failure_reasons"]["unknown_failure"] =
            serde_json::json!(1);
        assert!(serde_json::from_value::<DsvDiscoveryReport>(unknown_reason).is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn historical_reports_cannot_claim_a_v3_reason_ledger() {
        let root = root();
        let mut report = DsvDiscoveryBuilder::new(config(&root))
            .unwrap()
            .finish(DsvDiscoveryRuntimeCounters {
                hook_integrity_failures: 1,
                hook_integrity_failure_reasons: Some(DsvDiscoveryHookIntegrityFailureReasons {
                    tracker_failure: 1,
                    ..DsvDiscoveryHookIntegrityFailureReasons::default()
                }),
                ..DsvDiscoveryRuntimeCounters::default()
            })
            .unwrap();
        report.schema = DSV_DISCOVERY_REPORT_SCHEMA_V2.into();
        assert!(report.validate().is_err());

        for schema in [
            DSV_DISCOVERY_REPORT_SCHEMA_V1,
            DSV_DISCOVERY_REPORT_SCHEMA_V2,
        ] {
            let mut historical = report.clone();
            historical.schema = schema.into();
            historical.summary.runtime.hook_integrity_failure_reasons = None;
            historical.validate().unwrap();

            let mut historical_wire = serde_json::to_value(&historical).unwrap();
            assert!(
                historical_wire["summary"]
                    .get("hook_integrity_failure_reasons")
                    .is_none()
            );
            historical_wire["summary"]["hook_integrity_failure_reasons"] =
                serde_json::to_value(DsvDiscoveryHookIntegrityFailureReasons::default()).unwrap();
            let historical_with_v3_ledger: DsvDiscoveryReport =
                serde_json::from_value(historical_wire).unwrap();
            assert!(historical_with_v3_ledger.validate().is_err());
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn builder_refuses_runtime_counter_merge_overflow() {
        let root = root();
        let mut aggregate_builder = DsvDiscoveryBuilder::new(config(&root)).unwrap();
        aggregate_builder.summary.runtime.hook_integrity_failures = 1;
        assert!(
            aggregate_builder
                .finish(DsvDiscoveryRuntimeCounters {
                    hook_integrity_failures: u64::MAX,
                    ..DsvDiscoveryRuntimeCounters::default()
                })
                .unwrap_err()
                .contains("aggregate overflowed")
        );

        let mut reason_builder = DsvDiscoveryBuilder::new(config(&root)).unwrap();
        reason_builder
            .summary
            .runtime
            .hook_integrity_failure_reasons = Some(DsvDiscoveryHookIntegrityFailureReasons {
            tracker_failure: u64::MAX,
            ..DsvDiscoveryHookIntegrityFailureReasons::default()
        });
        assert!(
            reason_builder
                .finish(DsvDiscoveryRuntimeCounters {
                    hook_integrity_failure_reasons: Some(DsvDiscoveryHookIntegrityFailureReasons {
                        tracker_failure: 1,
                        ..DsvDiscoveryHookIntegrityFailureReasons::default()
                    },),
                    ..DsvDiscoveryRuntimeCounters::default()
                })
                .unwrap_err()
                .contains("tracker-failure counter overflowed")
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn builder_bounds_frames_candidates_and_descriptor_drift() {
        let root = root();
        let mut builder = DsvDiscoveryBuilder::new(config(&root)).unwrap();
        builder.observe_frame(DsvDiscoveryFrameObservation {
            present_seq: 1,
            candidates: vec![
                candidate_at("dsv-0001", 1920, 1),
                candidate_at("dsv-0002", 1280, 2),
                candidate_at("dsv-0003", 640, 3),
            ],
            ambiguous_descriptor_groups: Vec::new(),
        });
        builder.observe_frame(DsvDiscoveryFrameObservation {
            present_seq: 2,
            candidates: vec![candidate("dsv-0001", 2560)],
            ambiguous_descriptor_groups: Vec::new(),
        });
        builder.observe_frame(DsvDiscoveryFrameObservation {
            present_seq: 3,
            candidates: vec![],
            ambiguous_descriptor_groups: Vec::new(),
        });
        let report = builder
            .finish(DsvDiscoveryRuntimeCounters::default())
            .unwrap();
        assert!(!report.complete);
        assert_eq!(report.frames.len(), 2);
        assert!(
            report
                .blockers
                .contains(&DsvDiscoveryBlocker::PerFrameCandidateLimitReached)
        );
        assert!(
            report
                .blockers
                .contains(&DsvDiscoveryBlocker::DescriptorIdentityConflict)
        );
        assert!(
            report
                .blockers
                .contains(&DsvDiscoveryBlocker::FrameLimitReached)
        );
        report.validate().unwrap();
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn filesystem_boundary_rejects_existing_sibling_and_escape() {
        let root = root();
        let out = root.join("run_1");
        assert!(create_new_evidence_directory(&root, &out).is_ok());
        assert!(create_new_evidence_directory(&root, &out).is_err());
        let sibling = root.parent().unwrap().join("escaped_run");
        assert!(create_new_evidence_directory(&root, &sibling).is_err());
        let _ = fs::remove_dir_all(root);
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
    fn filesystem_boundary_rejects_reparse_root_and_ancestor() {
        let base = root();
        let actual_root = base.join("actual_root");
        fs::create_dir(&actual_root).unwrap();
        let linked_root = base.join("linked_root");
        create_directory_link(&actual_root, &linked_root);
        assert!(create_new_evidence_directory(&linked_root, &linked_root.join("run_1")).is_err());

        let actual_parent = base.join("actual_parent");
        let nested_root = actual_parent.join("nested_root");
        fs::create_dir_all(&nested_root).unwrap();
        let linked_parent = base.join("linked_parent");
        create_directory_link(&actual_parent, &linked_parent);
        let root_through_link = linked_parent.join("nested_root");
        assert!(
            create_new_evidence_directory(&root_through_link, &root_through_link.join("run_2"))
                .is_err()
        );

        // Remove links as links, never recursively follow their targets.
        remove_directory_link(&linked_root);
        remove_directory_link(&linked_parent);
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn report_publication_commits_verified_bytes_and_leaves_no_pending_file() {
        let root = root();
        let output = root.join("success");
        fs::create_dir(&output).unwrap();
        let report = one_frame_report(&root);
        let mut expected = serde_json::to_vec_pretty(&report).unwrap();
        expected.push(b'\n');

        let final_path = write_report_create_new(&output, &report).unwrap();
        assert_eq!(
            final_path,
            output
                .canonicalize()
                .unwrap()
                .join(DSV_DISCOVERY_REPORT_FILE_NAME)
        );
        assert_eq!(fs::read(&final_path).unwrap(), expected);
        assert!(report_pending_entries(&output).is_empty());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn report_precommit_fault_returns_err_without_final_or_pending_file() {
        let root = root();
        let output = root.join("fault");
        fs::create_dir(&output).unwrap();
        let report = one_frame_report(&root);

        assert!(write_report_create_new_inner(&output, &report, true).is_err());
        assert!(!output.join(DSV_DISCOVERY_REPORT_FILE_NAME).exists());
        assert!(report_pending_entries(&output).is_empty());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn existing_final_is_never_replaced_and_failed_attempt_leaves_no_pending_file() {
        let root = root();
        let output = root.join("existing");
        fs::create_dir(&output).unwrap();
        let final_path = output.join(DSV_DISCOVERY_REPORT_FILE_NAME);
        let sentinel = b"operator-owned-existing-report\n";
        fs::write(&final_path, sentinel).unwrap();
        let report = one_frame_report(&root);

        assert!(write_report_create_new(&output, &report).is_err());
        assert_eq!(fs::read(&final_path).unwrap(), sentinel);
        assert!(report_pending_entries(&output).is_empty());
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(windows)]
    #[test]
    fn pinned_directory_handle_rejects_a_reparse_object_without_following_it() {
        let root = root();
        let target = root.join("actual-output");
        fs::create_dir(&target).unwrap();
        let junction = root.join("output-junction");
        create_directory_link(&target, &junction);

        let error = open_pinned_report_directory(&junction).unwrap_err();
        assert!(error.contains("not a plain directory"), "{error}");

        remove_directory_link(&junction);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn offline_validator_rejects_unknown_fields_and_never_overwrites() {
        let root = root();
        let source_dir = root.join("source");
        fs::create_dir(&source_dir).unwrap();
        let mut builder = DsvDiscoveryBuilder::new(config(&root)).unwrap();
        builder.observe_frame(DsvDiscoveryFrameObservation {
            present_seq: 1,
            candidates: vec![candidate("dsv-0001", 1920)],
            ambiguous_descriptor_groups: Vec::new(),
        });
        let report = builder
            .finish(DsvDiscoveryRuntimeCounters::default())
            .unwrap();
        let input = write_report_create_new_under_root(&root, &source_dir, &report).unwrap();
        let sibling_dir = root
            .parent()
            .unwrap()
            .join(format!("dm-dsv-discovery-sibling-{}", std::process::id()));
        let _ = fs::remove_dir_all(&sibling_dir);
        fs::create_dir(&sibling_dir).unwrap();
        assert!(write_report_create_new_under_root(&root, &sibling_dir, &report).is_err());
        let _ = fs::remove_dir_all(&sibling_dir);
        let out = root.join("review_1");
        assert!(materialize_validated_report(&input, &root, &out).is_ok());
        assert!(materialize_validated_report(&input, &root, &out).is_err());
        let mut value = serde_json::to_value(report).unwrap();
        value["selected_candidate_id"] = serde_json::Value::String("dsv-0001".into());
        let unknown = root.join("unknown.json");
        fs::write(&unknown, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(read_report_bounded(&unknown, &root).is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(feature = "evidence-authoring")]
    #[test]
    fn pass_summary_materialization_hashes_the_validated_bytes_and_never_overwrites() {
        let root = root();
        let source_dir = root.join("source");
        fs::create_dir(&source_dir).unwrap();
        let mut builder = DsvDiscoveryBuilder::new(config(&root)).unwrap();
        builder.observe_frame(DsvDiscoveryFrameObservation {
            present_seq: 1,
            candidates: vec![candidate("dsv-0001", 1920)],
            ambiguous_descriptor_groups: Vec::new(),
        });
        let report = builder
            .finish(DsvDiscoveryRuntimeCounters::default())
            .unwrap();
        let input = write_report_create_new_under_root(&root, &source_dir, &report).unwrap();
        let input_bytes = fs::read(&input).unwrap();
        let expected_hash = format!("sha256:{:x}", Sha256::digest(&input_bytes));
        let out = root.join("pass_summary");
        let summary_path =
            materialize_dsv_candidate_pass_summary(&input, &root, &out, "dsv-0001").unwrap();
        let summary: DsvDiscoveryCandidatePassSummary =
            serde_json::from_slice(&fs::read(summary_path).unwrap()).unwrap();
        assert_eq!(summary.source_report_sha256, expected_hash);
        summary.validate().unwrap();
        assert!(materialize_dsv_candidate_pass_summary(&input, &root, &out, "dsv-0001").is_err());
        let _ = fs::remove_dir_all(root);
    }
}

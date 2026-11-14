//! #636 render-camera authoring evidence.
//!
//! This module deliberately stops before production binding authoring. Three reliable discovery
//! phases can identify a GPU view source whose motion has the expected static/walk/turn response,
//! and can show how the old singleton agrees or disagrees on the same Presents. Only an E-geo run
//! against RGB + metric depth can promote that source to the render-camera authority.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path};

use serde::{Deserialize, Serialize};

use crate::camera::{CameraPose, correlation, pose_from_view};
use crate::camera_binding::{
    CAMERA_BINDING_SCHEMA_V2, D3d12AxisMapping, D3d12CameraBinding, D3d12MatrixSourceBinding,
    D3d12ViewSourceBinding, GameCameraBinding, ViewSourceBinding,
    fallout_render_rotation_from_basis, is_sha256_digest,
};
use crate::camera_discovery::{
    CAMERA_DISCOVERY_REPORT_SCHEMA_V2, CAMERA_DISCOVERY_REPORT_SCHEMA_V3,
    CAMERA_DISCOVERY_REPORT_SCHEMA_V4, CameraDiscoveryReport, DiscoveryReferenceCameraObservation,
    DiscoveryViewCandidate, MAX_DISCOVERY_CANDIDATES,
};
use crate::probe_config::{
    CAMERA_DISCOVERY_SCHEMA_V3, CameraDiscoveryConfig, CameraDiscoveryDsvSelection,
    CameraDiscoveryRenderIdentity, MAX_DISCOVERY_DRAWS_PER_SAMPLED_PRESENT,
};
use crate::raw_depth_demo::Sha256Hex;

/// Offline evidence review for the explicit CP2077 holstered v3 binding. The old v2
/// authoring contract below is intentionally unchanged.
pub mod holstered;

pub const CAMERA_EVIDENCE_REPORT_SCHEMA_V1: &str = "camera-evidence-report/v1";
pub const CAMERA_EVIDENCE_REPORT_SCHEMA_V2: &str = "camera-evidence-report/v2";
pub const CAMERA_EVIDENCE_SWEEP_REPORT_SCHEMA_V1: &str = "camera-evidence-sweep-report/v1";
pub const FALLOUT_HYBRID_EVIDENCE_SCHEMA_V2: &str = "camera-evidence-fallout-render-hybrid/v2";
pub const FALLOUT_HYBRID_SWEEP_SCHEMA_V2: &str = "camera-evidence-sweep-fallout-render-hybrid/v2";

/// Explicit offline interpretation, never inferred from a matrix's shape or a NoMatch result.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum CameraEvidenceAnalysisMode {
    #[default]
    RigidView,
    FalloutRenderHybrid,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FalloutHybridEvidence {
    pub reference_orientation_max_error_rad: [f64; 3],
    pub walk_turn_rotation_response_ratio: [Option<f64>; 2],
    pub reference_orientation_matches: bool,
    pub rotation_response_matches: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RejectedCameraSource {
    pub source: ViewSourceBinding,
    pub reason: String,
}
pub const CAMERA_REFERENCE_FREE_CANDIDATE_REVIEW_SCHEMA_V1: &str =
    "camera-reference-free-candidate-review/v1";
pub const CAMERA_REFERENCE_FREE_CANDIDATE_REVIEW_SWEEP_SCHEMA_V1: &str =
    "camera-reference-free-candidate-review-sweep/v1";
pub const D3D12_CAMERA_AUTHORING_LAUNCH_SCHEMA_V1: &str = "d3d12-camera-authoring-launch/v1";
pub const D3D12_CAMERA_AUTHORING_PAIR_SCHEMA_V1: &str = "d3d12-camera-authoring-pair/v1";
pub const D3D12_CAMERA_EGEO_ATTESTATION_SCHEMA_V1: &str = "d3d12-camera-egeo-attestation/v1";
pub const D3D12_CAMERA_EGEO_ATTESTATION_PAYLOAD_SCHEMA_V1: &str =
    "d3d12-camera-egeo-attestation-payload/v1";
pub const D3D12_CAMERA_EGEO_CANONICALIZATION_V1: &str = "serde-json-struct-order-utf8-compact/v1";
pub const POSE_STATIC_PHASE: &str = "POSE_STATIC";
pub const POSE_WALK_PHASE: &str = "POSE_WALK";
pub const POSE_TURN_PHASE: &str = "POSE_TURN";
const MIN_PHASE_SAMPLES: usize = 8;
const MIN_TRANSLATION_RESPONSE: f64 = 1.0e-5;
const MIN_ROTATION_RESPONSE_RAD: f64 = 1.0e-5;
const STATIC_RESPONSE_RATIO: f64 = 0.25;
const MAX_STATIC_TRANSLATION_P95_METRES: f64 = 0.25;
const MAX_STATIC_ROTATION_P95_RAD: f64 = 0.02;
const FOV_SETTING_TOLERANCE_DEGREES: f64 = 1.0;
const FOV_RETURN_RELATIVE_TOLERANCE: f64 = 0.01;
const FOV_RESPONSE_MIN_RELATIVE_CHANGE: f64 = 0.10;
const AXIS_INDEPENDENCE_RESIDUAL: f64 = 0.25;
const MIN_AXIS_MOTION_METRES: f64 = 0.05;
const MAX_RETURN_POSITION_ERROR_METRES: f64 = 0.50;
const MAX_RETURN_ROTATION_ERROR_RAD: f64 = 0.05;
const AXIS_MAPPING_RELATIVE_TOLERANCE: f64 = 0.01;
const AXIS_MAPPING_ABSOLUTE_TOLERANCE_METRES: f64 = 1.0e-4;
const AXIS_DIRECTION_MIN_COSINE: f64 = 0.85;
const CROSS_LAUNCH_AXIS_MIN_COSINE: f64 = 0.95;
const CP2077_EGEO_REPORT_SCHEMA_V1: &str = "dm-cp2077-camera-egeo-evidence/v1";
const CAMERA_EGEO_VALIDATION_SCHEMA_V3: &str = "camera-egeo-validation/v3";

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MotionStats {
    pub samples: u64,
    pub steps: u64,
    pub translation_median: f64,
    pub translation_p95: f64,
    pub rotation_median_rad: f64,
    pub rotation_p95_rad: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PhaseCoupling {
    pub joined_samples: u64,
    pub translation_magnitude_correlation: Option<f32>,
    pub rotation_magnitude_correlation: Option<f32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateEvidence {
    pub source: ViewSourceBinding,
    pub static_motion: MotionStats,
    pub walk_motion: MotionStats,
    pub turn_motion: MotionStats,
    pub static_coupling_to_legacy_reference: PhaseCoupling,
    pub walk_coupling_to_legacy_reference: PhaseCoupling,
    pub turn_coupling_to_legacy_reference: PhaseCoupling,
    pub enough_samples: bool,
    pub static_translation_p95_metres: f64,
    pub static_absolute_stable: bool,
    pub static_stable: bool,
    pub walk_translation_response: bool,
    pub turn_rotation_response: bool,
    pub controlled_phase_pattern_matches: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallout_hybrid: Option<FalloutHybridEvidence>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CameraEvidenceSelection {
    Selected {
        source: ViewSourceBinding,
        /// The same immutable source bytes were observed at these draw ordinals with exactly
        /// identical evidence. Draw provenance remains visible without turning one binding into
        /// a false ambiguity.
        #[serde(default)]
        equivalent_draw_ordinals: Vec<u32>,
    },
    Ambiguous {
        matching_sources: Vec<ViewSourceBinding>,
    },
    NoMatch,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CameraEvidenceReport {
    pub schema: String,
    pub phase_ids: [String; 3],
    pub executable_sha256: String,
    pub candidates: Vec<CandidateEvidence>,
    pub legacy_reference_static_motion: MotionStats,
    pub legacy_reference_walk_motion: MotionStats,
    pub legacy_reference_turn_motion: MotionStats,
    pub selection: CameraEvidenceSelection,
    /// Always true at this stage. The selected source is an authoring candidate, not a production
    /// camera binding, until RGB/depth E-geo beats the no-motion baseline.
    pub requires_e_geo_validation: bool,
    pub production_binding_authorized: bool,
    /// Hybrid translation comes from this exact-build reference, not from the GPU basis.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallout_position_reference: Option<GameCameraBinding>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rejected_sources: Vec<RejectedCameraSource>,
}

#[derive(Debug, Clone)]
pub struct CameraEvidenceSweepInput {
    pub static_report: CameraDiscoveryReport,
    pub walk_report: CameraDiscoveryReport,
    pub turn_report: CameraDiscoveryReport,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CameraEvidenceSweepWindow {
    pub draw_window_start: u32,
    pub draw_window_count: u32,
    pub evidence: CameraEvidenceReport,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CameraEvidenceSweepReport {
    pub schema: String,
    pub executable_sha256: String,
    pub windows: Vec<CameraEvidenceSweepWindow>,
    pub selection: CameraEvidenceSelection,
    pub requires_e_geo_validation: bool,
    pub production_binding_authorized: bool,
}

/// Raw, scale-free motion for one source observed in every controlled phase.
///
/// This deliberately contains no score, metres, comparison coupling, stability verdict, or
/// selected status. Without an exact-build comparison camera and metric scale those would be
/// authority claims that the evidence cannot support.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReferenceFreeCameraCandidate {
    pub source: ViewSourceBinding,
    pub static_motion: MotionStats,
    pub walk_motion: MotionStats,
    pub turn_motion: MotionStats,
}

/// Candidates-only bridge for a v3 discovery run that has no legacy comparison camera.
///
/// Human review may use this artifact to choose what to investigate next. It cannot select a
/// source, establish metric scale, generate a camera binding, authorize recording/publication, or
/// enter the production authoring path.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReferenceFreeCameraCandidateReview {
    pub schema: String,
    pub status: String,
    pub phase_ids: [String; 3],
    pub executable_sha256: String,
    pub dsv_selection: CameraDiscoveryDsvSelection,
    pub selected_dsv_descriptor: crate::DsvDiscoveryDescriptor,
    pub candidates: Vec<ReferenceFreeCameraCandidate>,
    pub human_review_required: bool,
    pub metric_scale_available: bool,
    pub automatic_selection_authorized: bool,
    pub camera_binding_generated: bool,
    pub recording_ready: bool,
    pub deliverable: bool,
    pub publication_allowed: bool,
    pub requires_metric_scale_validation: bool,
    pub requires_e_geo_validation: bool,
    pub production_binding_authorized: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReferenceFreeCameraCandidateReviewSweepWindow {
    pub draw_window_start: u32,
    pub draw_window_count: u32,
    pub review: ReferenceFreeCameraCandidateReview,
}

/// Cross-window candidates-only bridge. `common_sources` is an intersection with draw ordinals
/// normalized to zero, never a selection; every window retains the exact draw provenance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReferenceFreeCameraCandidateReviewSweep {
    pub schema: String,
    pub status: String,
    pub executable_sha256: String,
    pub dsv_selection: CameraDiscoveryDsvSelection,
    pub selected_dsv_descriptor: crate::DsvDiscoveryDescriptor,
    pub windows: Vec<ReferenceFreeCameraCandidateReviewSweepWindow>,
    pub common_sources: Vec<ViewSourceBinding>,
    pub human_review_required: bool,
    pub metric_scale_available: bool,
    pub automatic_selection_authorized: bool,
    pub camera_binding_generated: bool,
    pub recording_ready: bool,
    pub deliverable: bool,
    pub publication_allowed: bool,
    pub requires_metric_scale_validation: bool,
    pub requires_e_geo_validation: bool,
    pub production_binding_authorized: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum D3d12AuthoringLaunchRole {
    A,
    B,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct D3d12FovPhaseObservation {
    pub configured_horizontal_fov_degrees: f64,
    pub observed_horizontal_fov_degrees: f64,
    pub fx_pixels: f64,
    pub fy_pixels: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum D3d12ControlledMotion {
    Forward,
    StrafeRight,
    VerticalUp,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct D3d12AxisMotionObservation {
    pub motion: D3d12ControlledMotion,
    /// Raw displacement before axis permutation or metric scaling.
    pub source_displacement_world_units: [f64; 3],
    /// The same two endpoints after applying the declared mapping and metric scale.
    pub delivery_displacement_metres: [f64; 3],
}

/// Cross-launch claims that bind one exact E-geo report to one build/profile and stable D3D12
/// camera source. The raw Python report stays external and is verified by content hash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct D3d12EgeoAttestationClaims {
    pub process_launch_id: String,
    pub executable_sha256: String,
    pub depth_profile_sha256: String,
    pub view_metres_per_unit: String,
    pub axis_mapping: D3d12AxisMapping,
    pub view_source: D3d12ViewSourceBinding,
    pub projection_source: D3d12MatrixSourceBinding,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct D3d12EgeoAttestationPayload {
    pub schema: String,
    pub egeo_report_sha256: String,
    pub claims: D3d12EgeoAttestationClaims,
}

/// Canonical SHA covers the typed payload after the raw E-geo report hash is inserted. Compact
/// `serde_json` serialization of this struct (field order as declared, UTF-8, no trailing LF) is
/// the versioned canonical byte representation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct D3d12EgeoAttestation {
    pub schema: String,
    pub canonicalization: String,
    pub payload: D3d12EgeoAttestationPayload,
    pub canonical_payload_sha256: String,
}

#[derive(Debug, Deserialize)]
struct Cp2077TypedEgeoReport {
    schema: String,
    non_trainable: bool,
    local_only: bool,
    upload_allowed: bool,
    e_geo: Cp2077TypedEgeoVerdict,
}

#[derive(Debug, Deserialize)]
struct Cp2077TypedEgeoVerdict {
    schema: String,
    passed: bool,
    ready_for_independent_repeat: bool,
    production_binding_authorized: bool,
    failures: Vec<String>,
    raw_depth_profile_sha256: String,
}

/// Sealed output contract for one hardware authoring launch.  It is evidence, not a production
/// binding: even a valid A/B pair remains non-trainable and requires reviewed catalog promotion.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct D3d12CameraAuthoringLaunchEvidence {
    pub schema: String,
    pub launch_role: D3d12AuthoringLaunchRole,
    pub process_launch_id: String,
    pub executable_sha256: String,
    pub depth_profile_sha256: String,
    pub non_trainable: bool,
    pub upload_allowed: bool,
    pub production_binding_authorized: bool,
    pub authoring_blockers: Vec<String>,
    pub render_width: u32,
    pub render_height: u32,
    pub view_metres_per_unit: String,
    pub axis_mapping: D3d12AxisMapping,
    pub view_source: D3d12ViewSourceBinding,
    pub projection_source: D3d12MatrixSourceBinding,
    /// Exactly 75 -> 90 -> 75, measured from the physical projection source.
    pub fov_75_90_75: [D3d12FovPhaseObservation; 3],
    /// Exactly forward, strafe-right and vertical-up, in that order.
    pub axis_motion: [D3d12AxisMotionObservation; 3],
    pub egeo_attestation: D3d12EgeoAttestation,
    pub route_start_position_metres: [f64; 3],
    pub route_end_position_metres: [f64; 3],
    pub route_start_rotation_quaternion: [f64; 4],
    pub route_end_rotation_quaternion: [f64; 4],
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct D3d12CameraAuthoringPairVerdict {
    pub schema: String,
    pub launch_a_id: String,
    pub launch_b_id: String,
    pub executable_sha256: String,
    pub depth_profile_sha256: String,
    pub render_width: u32,
    pub render_height: u32,
    pub view_source: D3d12ViewSourceBinding,
    pub projection_source: D3d12MatrixSourceBinding,
    pub view_metres_per_unit: String,
    pub axis_mapping: D3d12AxisMapping,
    pub ready_for_human_review: bool,
    pub requires_signed_catalog_promotion: bool,
    pub non_trainable: bool,
    pub upload_allowed: bool,
    pub production_binding_authorized: bool,
}

#[derive(Debug)]
pub struct CameraEvidenceSweepBuilder {
    analysis_mode: CameraEvidenceAnalysisMode,
    baseline_contract: Option<(
        CameraDiscoveryRenderIdentity,
        CameraDiscoveryConfig,
        Option<crate::DsvDiscoveryDescriptor>,
    )>,
    previous_end: Option<u32>,
    windows: Vec<CameraEvidenceSweepWindow>,
}

impl Default for CameraEvidenceSweepBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl CameraEvidenceSweepBuilder {
    pub fn new() -> Self {
        Self::with_analysis_mode(CameraEvidenceAnalysisMode::RigidView)
    }

    pub fn with_analysis_mode(analysis_mode: CameraEvidenceAnalysisMode) -> Self {
        Self {
            analysis_mode,
            baseline_contract: None,
            previous_end: None,
            windows: Vec::new(),
        }
    }

    pub fn push(&mut self, input: CameraEvidenceSweepInput) -> Result<(), String> {
        let start = input.static_report.config.draw_window_start;
        let count = input.static_report.config.draw_window_count;
        let end = start
            .checked_add(count)
            .ok_or("camera evidence draw window overflow")?;
        if count == 0 || end > 512 {
            return Err(format!(
                "camera evidence draw window must be non-empty and within 0..512: start={start} count={count}"
            ));
        }
        if let Some(previous_end) = self.previous_end
            && start < previous_end
        {
            return Err(format!(
                "camera evidence draw windows overlap or are unsorted at start={start}"
            ));
        }
        if let Some((render, config, selected_dsv_descriptor)) = &self.baseline_contract {
            validate_sweep_contract(
                render,
                config,
                *selected_dsv_descriptor,
                &input.static_report,
            )?;
        } else {
            self.baseline_contract = Some((
                input.static_report.render.clone(),
                input.static_report.config.clone(),
                input.static_report.selected_dsv_descriptor,
            ));
        }
        let suffix = format!("W{start:03}");
        let phase_ids = [
            format!("{POSE_STATIC_PHASE}_{suffix}"),
            format!("{POSE_WALK_PHASE}_{suffix}"),
            format!("{POSE_TURN_PHASE}_{suffix}"),
        ];
        let evidence = analyze_camera_evidence_with_mode(
            &input.static_report,
            &input.walk_report,
            &input.turn_report,
            [&phase_ids[0], &phase_ids[1], &phase_ids[2]],
            self.analysis_mode,
        )?;
        self.windows.push(CameraEvidenceSweepWindow {
            draw_window_start: start,
            draw_window_count: count,
            evidence,
        });
        self.previous_end = Some(end);
        Ok(())
    }

    pub fn finish(self) -> Result<CameraEvidenceSweepReport, String> {
        let Some((_, baseline_config, _)) = self.baseline_contract else {
            return Err("camera evidence sweep contains no draw windows".into());
        };
        let selection = selection_from_window_reports(&self.windows);
        Ok(CameraEvidenceSweepReport {
            schema: match self.analysis_mode {
                CameraEvidenceAnalysisMode::RigidView => CAMERA_EVIDENCE_SWEEP_REPORT_SCHEMA_V1,
                CameraEvidenceAnalysisMode::FalloutRenderHybrid => FALLOUT_HYBRID_SWEEP_SCHEMA_V2,
            }
            .into(),
            executable_sha256: baseline_config.executable_sha256,
            windows: self.windows,
            selection,
            requires_e_geo_validation: true,
            production_binding_authorized: false,
        })
    }
}

impl ReferenceFreeCameraCandidateReview {
    /// Validate the candidates-only ceiling without upgrading raw motion into authority.
    pub fn validate(&self) -> Result<(), String> {
        if self.schema != CAMERA_REFERENCE_FREE_CANDIDATE_REVIEW_SCHEMA_V1
            || self.status != "human_review_required"
            || !self.human_review_required
            || self.metric_scale_available
            || self.automatic_selection_authorized
            || self.camera_binding_generated
            || self.recording_ready
            || self.deliverable
            || self.publication_allowed
            || !self.requires_metric_scale_validation
            || !self.requires_e_geo_validation
            || self.production_binding_authorized
        {
            return Err("reference-free camera review exceeded its candidates-only ceiling".into());
        }
        validate_reference_free_phase_ids(&self.phase_ids, None)?;
        if !is_sha256_digest(&self.executable_sha256) {
            return Err("reference-free camera review executable hash is not canonical".into());
        }
        validate_reference_free_selection_binding(&self.dsv_selection)?;
        self.selected_dsv_descriptor.validate()?;
        if self.candidates.len() > MAX_DISCOVERY_CANDIDATES {
            return Err("reference-free camera review has too many candidates".into());
        }
        let sources = self
            .candidates
            .iter()
            .map(|candidate| candidate.source)
            .collect::<Vec<_>>();
        let mut sorted = sources.clone();
        sorted.sort_unstable();
        sorted.dedup();
        if sources != sorted {
            return Err("reference-free camera review candidates are not unique and sorted".into());
        }
        for candidate in &self.candidates {
            for motion in [
                candidate.static_motion,
                candidate.walk_motion,
                candidate.turn_motion,
            ] {
                validate_raw_motion_stats(motion)?;
            }
        }
        Ok(())
    }
}

impl ReferenceFreeCameraCandidateReviewSweep {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema != CAMERA_REFERENCE_FREE_CANDIDATE_REVIEW_SWEEP_SCHEMA_V1
            || self.status != "human_review_required"
            || !self.human_review_required
            || self.metric_scale_available
            || self.automatic_selection_authorized
            || self.camera_binding_generated
            || self.recording_ready
            || self.deliverable
            || self.publication_allowed
            || !self.requires_metric_scale_validation
            || !self.requires_e_geo_validation
            || self.production_binding_authorized
        {
            return Err(
                "reference-free camera review sweep exceeded its candidates-only ceiling".into(),
            );
        }
        if self.windows.is_empty() || self.windows.len() > 512 {
            return Err("reference-free camera review sweep window count is invalid".into());
        }
        if !is_sha256_digest(&self.executable_sha256) {
            return Err(
                "reference-free camera review sweep executable hash is not canonical".into(),
            );
        }
        validate_reference_free_selection_binding(&self.dsv_selection)?;
        self.selected_dsv_descriptor.validate()?;
        let mut previous_end = None;
        for window in &self.windows {
            let end = window
                .draw_window_start
                .checked_add(window.draw_window_count)
                .ok_or("reference-free camera review sweep window overflow")?;
            if window.draw_window_count == 0
                || end > 512
                || previous_end.is_some_and(|previous| window.draw_window_start != previous)
            {
                return Err(
                    "reference-free camera review sweep windows are not contiguous or are invalid"
                        .into(),
                );
            }
            window.review.validate()?;
            validate_reference_free_phase_ids(
                &window.review.phase_ids,
                Some(window.draw_window_start),
            )?;
            if window.review.executable_sha256 != self.executable_sha256
                || window.review.dsv_selection != self.dsv_selection
                || window.review.selected_dsv_descriptor != self.selected_dsv_descriptor
            {
                return Err("reference-free camera review sweep window identity drifted".into());
            }
            previous_end = Some(end);
        }
        let expected_common_sources = common_reference_free_window_sources(&self.windows);
        if self.common_sources != expected_common_sources {
            return Err(
                "reference-free camera review sweep common sources are not the exact intersection"
                    .into(),
            );
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct ReferenceFreeCameraCandidateReviewSweepBuilder {
    baseline_contract: Option<(
        CameraDiscoveryRenderIdentity,
        CameraDiscoveryConfig,
        crate::DsvDiscoveryDescriptor,
    )>,
    previous_end: Option<u32>,
    windows: Vec<ReferenceFreeCameraCandidateReviewSweepWindow>,
}

impl Default for ReferenceFreeCameraCandidateReviewSweepBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl ReferenceFreeCameraCandidateReviewSweepBuilder {
    pub fn new() -> Self {
        Self {
            baseline_contract: None,
            previous_end: None,
            windows: Vec::new(),
        }
    }

    pub fn push(&mut self, input: CameraEvidenceSweepInput) -> Result<(), String> {
        let start = input.static_report.config.draw_window_start;
        let count = input.static_report.config.draw_window_count;
        let end = start
            .checked_add(count)
            .ok_or("reference-free camera review draw window overflow")?;
        if count == 0 || end > 512 {
            return Err(format!(
                "reference-free camera review draw window must be non-empty and within 0..512: start={start} count={count}"
            ));
        }
        if self.previous_end.is_some_and(|previous| start != previous) {
            return Err(format!(
                "reference-free camera review draw windows are not contiguous at start={start}"
            ));
        }
        let suffix = format!("W{start:03}");
        let phase_ids = [
            format!("{POSE_STATIC_PHASE}_{suffix}"),
            format!("{POSE_WALK_PHASE}_{suffix}"),
            format!("{POSE_TURN_PHASE}_{suffix}"),
        ];
        let review = analyze_reference_free_camera_candidate_review_for_phases(
            &input.static_report,
            &input.walk_report,
            &input.turn_report,
            [&phase_ids[0], &phase_ids[1], &phase_ids[2]],
        )?;
        let descriptor = review.selected_dsv_descriptor;
        if let Some((render, config, selected_descriptor)) = &self.baseline_contract {
            validate_sweep_contract(
                render,
                config,
                Some(*selected_descriptor),
                &input.static_report,
            )?;
        } else {
            self.baseline_contract = Some((
                input.static_report.render.clone(),
                input.static_report.config.clone(),
                descriptor,
            ));
        }
        self.windows
            .push(ReferenceFreeCameraCandidateReviewSweepWindow {
                draw_window_start: start,
                draw_window_count: count,
                review,
            });
        self.previous_end = Some(end);
        Ok(())
    }

    pub fn finish(self) -> Result<ReferenceFreeCameraCandidateReviewSweep, String> {
        let Some((_, baseline_config, selected_dsv_descriptor)) = self.baseline_contract else {
            return Err("reference-free camera review sweep contains no draw windows".into());
        };
        let dsv_selection = baseline_config
            .dsv_selection
            .ok_or("reference-free camera review sweep lost its DSV selection binding")?;
        let common_sources = common_reference_free_window_sources(&self.windows);
        let report = ReferenceFreeCameraCandidateReviewSweep {
            schema: CAMERA_REFERENCE_FREE_CANDIDATE_REVIEW_SWEEP_SCHEMA_V1.into(),
            status: "human_review_required".into(),
            executable_sha256: baseline_config.executable_sha256,
            dsv_selection,
            selected_dsv_descriptor,
            windows: self.windows,
            common_sources,
            human_review_required: true,
            metric_scale_available: false,
            automatic_selection_authorized: false,
            camera_binding_generated: false,
            recording_ready: false,
            deliverable: false,
            publication_allowed: false,
            requires_metric_scale_validation: true,
            requires_e_geo_validation: true,
            production_binding_authorized: false,
        };
        report.validate()?;
        Ok(report)
    }
}

/// Seal the typed launch/source claims to the exact raw E-geo report bytes. The hash function is
/// injected so the platform-neutral depth-lab library does not pull SHA-256 into the probe DLL.
pub fn seal_d3d12_egeo_attestation(
    claims: D3d12EgeoAttestationClaims,
    egeo_report_bytes: &[u8],
    sha256_hex: Sha256Hex,
) -> Result<D3d12EgeoAttestation, String> {
    let egeo_report_sha256 = sha256_hex(egeo_report_bytes);
    if !is_sha256_digest(&egeo_report_sha256) {
        return Err("D3D12 E-geo report hash is not canonical SHA-256".into());
    }
    let payload = D3d12EgeoAttestationPayload {
        schema: D3D12_CAMERA_EGEO_ATTESTATION_PAYLOAD_SCHEMA_V1.into(),
        egeo_report_sha256,
        claims,
    };
    let canonical = canonical_egeo_payload(&payload)?;
    let canonical_payload_sha256 = sha256_hex(&canonical);
    if !is_sha256_digest(&canonical_payload_sha256) {
        return Err("D3D12 E-geo canonical payload hash is not canonical SHA-256".into());
    }
    Ok(D3d12EgeoAttestation {
        schema: D3D12_CAMERA_EGEO_ATTESTATION_SCHEMA_V1.into(),
        canonicalization: D3D12_CAMERA_EGEO_CANONICALIZATION_V1.into(),
        payload,
        canonical_payload_sha256,
    })
}

/// Validate two independently restarted D3D12 authoring launches. Success means the stable source
/// is ready for human review; it never turns on production authorization or upload.
pub fn validate_d3d12_camera_authoring_pair(
    launch_a: &D3d12CameraAuthoringLaunchEvidence,
    egeo_report_a_bytes: &[u8],
    launch_b: &D3d12CameraAuthoringLaunchEvidence,
    egeo_report_b_bytes: &[u8],
    sha256_hex: Sha256Hex,
) -> Result<D3d12CameraAuthoringPairVerdict, String> {
    validate_d3d12_camera_authoring_launch(
        launch_a,
        D3d12AuthoringLaunchRole::A,
        egeo_report_a_bytes,
        sha256_hex,
    )?;
    validate_d3d12_camera_authoring_launch(
        launch_b,
        D3d12AuthoringLaunchRole::B,
        egeo_report_b_bytes,
        sha256_hex,
    )?;
    if launch_a.process_launch_id == launch_b.process_launch_id {
        return Err(
            "D3D12 camera authoring A/B must come from independent process launches".into(),
        );
    }
    if launch_a.executable_sha256 != launch_b.executable_sha256
        || launch_a.depth_profile_sha256 != launch_b.depth_profile_sha256
        || launch_a.render_width != launch_b.render_width
        || launch_a.render_height != launch_b.render_height
    {
        return Err(
            "D3D12 camera authoring A/B build, profile hash or render geometry differs".into(),
        );
    }
    if launch_a.view_source != launch_b.view_source
        || launch_a.projection_source != launch_b.projection_source
    {
        return Err("D3D12 camera authoring stable matrix source changed across launches".into());
    }
    if launch_a.view_metres_per_unit != launch_b.view_metres_per_unit
        || launch_a.axis_mapping != launch_b.axis_mapping
    {
        return Err(
            "D3D12 camera authoring metric scale or axis mapping changed across launches".into(),
        );
    }
    validate_cross_launch_axis_correspondence(&launch_a.axis_motion, &launch_b.axis_motion)?;
    let a_baseline = launch_a.fov_75_90_75[0];
    let b_baseline = launch_b.fov_75_90_75[0];
    if relative_difference(a_baseline.fx_pixels, b_baseline.fx_pixels)
        > FOV_RETURN_RELATIVE_TOLERANCE
        || relative_difference(a_baseline.fy_pixels, b_baseline.fy_pixels)
            > FOV_RETURN_RELATIVE_TOLERANCE
    {
        return Err(
            "D3D12 camera FOV-75 intrinsics did not regress across independent launches".into(),
        );
    }
    Ok(D3d12CameraAuthoringPairVerdict {
        schema: D3D12_CAMERA_AUTHORING_PAIR_SCHEMA_V1.into(),
        launch_a_id: launch_a.process_launch_id.clone(),
        launch_b_id: launch_b.process_launch_id.clone(),
        executable_sha256: launch_a.executable_sha256.clone(),
        depth_profile_sha256: launch_a.depth_profile_sha256.clone(),
        render_width: launch_a.render_width,
        render_height: launch_a.render_height,
        view_source: launch_a.view_source.clone(),
        projection_source: launch_a.projection_source.clone(),
        view_metres_per_unit: launch_a.view_metres_per_unit.clone(),
        axis_mapping: launch_a.axis_mapping,
        ready_for_human_review: true,
        requires_signed_catalog_promotion: true,
        non_trainable: true,
        upload_allowed: false,
        production_binding_authorized: false,
    })
}

fn validate_d3d12_camera_authoring_launch(
    launch: &D3d12CameraAuthoringLaunchEvidence,
    expected_role: D3d12AuthoringLaunchRole,
    egeo_report_bytes: &[u8],
    sha256_hex: Sha256Hex,
) -> Result<(), String> {
    if launch.schema != D3D12_CAMERA_AUTHORING_LAUNCH_SCHEMA_V1
        || launch.launch_role != expected_role
    {
        return Err("unsupported D3D12 camera authoring launch schema or A/B role".into());
    }
    if !valid_process_launch_id(&launch.process_launch_id) {
        return Err("D3D12 camera authoring process_launch_id is invalid".into());
    }
    if !is_sha256_digest(&launch.executable_sha256)
        || !is_sha256_digest(&launch.depth_profile_sha256)
    {
        return Err("D3D12 camera authoring build/profile hash is invalid".into());
    }
    if !launch.non_trainable
        || launch.upload_allowed
        || launch.production_binding_authorized
        || !launch.authoring_blockers.is_empty()
    {
        return Err("D3D12 camera authoring launch violates DEV fail-closed state".into());
    }
    D3d12CameraBinding {
        holstered_camera: None,
        schema: CAMERA_BINDING_SCHEMA_V2.into(),
        binding_id: "authoring-validation".into(),
        binding_version: "authoring-validation".into(),
        profile_id: "authoring-validation".into(),
        profile_version: "authoring-validation".into(),
        executable_sha256: launch.executable_sha256.clone(),
        render_width: launch.render_width,
        render_height: launch.render_height,
        view_metres_per_unit: launch.view_metres_per_unit.clone(),
        axis_mapping: launch.axis_mapping,
        view_sources: vec![launch.view_source.clone()],
        projection_sources: vec![launch.projection_source.clone()],
    }
    .validate_for_executable(&launch.executable_sha256)
    .map_err(|error| format!("D3D12 camera authoring stable source is invalid: {error}"))?;
    validate_d3d12_egeo_attestation(launch, egeo_report_bytes, sha256_hex)?;
    validate_fov_75_90_75(&launch.fov_75_90_75)?;
    validate_axis_motion(launch)?;
    let delivery_vectors = launch
        .axis_motion
        .map(|observation| observation.delivery_displacement_metres);
    if independent_axis_rank(&delivery_vectors) != 3 {
        return Err(
            "D3D12 camera authoring motion vectors do not span three independent axes".into(),
        );
    }
    if !launch
        .route_start_position_metres
        .iter()
        .chain(launch.route_end_position_metres.iter())
        .all(|value| value.is_finite())
    {
        return Err("D3D12 camera authoring route position is non-finite".into());
    }
    let return_distance = distance(
        launch.route_start_position_metres,
        launch.route_end_position_metres,
    );
    if return_distance > MAX_RETURN_POSITION_ERROR_METRES {
        return Err(format!(
            "D3D12 camera authoring route did not return: position_error={return_distance}"
        ));
    }
    for (label, quaternion) in [
        ("start", launch.route_start_rotation_quaternion),
        ("end", launch.route_end_rotation_quaternion),
    ] {
        let norm = quaternion.iter().map(|value| value * value).sum::<f64>();
        if !norm.is_finite() || (norm - 1.0).abs() > 0.002 {
            return Err(format!(
                "D3D12 camera authoring route {label} quaternion is invalid"
            ));
        }
    }
    let return_rotation = quaternion_angle(
        launch.route_start_rotation_quaternion,
        launch.route_end_rotation_quaternion,
    );
    if return_rotation > MAX_RETURN_ROTATION_ERROR_RAD {
        return Err(format!(
            "D3D12 camera authoring route did not return: rotation_error={return_rotation}"
        ));
    }
    Ok(())
}

fn canonical_egeo_payload(payload: &D3d12EgeoAttestationPayload) -> Result<Vec<u8>, String> {
    serde_json::to_vec(payload)
        .map_err(|error| format!("serialize canonical D3D12 E-geo payload failed: {error}"))
}

fn validate_d3d12_egeo_attestation(
    launch: &D3d12CameraAuthoringLaunchEvidence,
    egeo_report_bytes: &[u8],
    sha256_hex: Sha256Hex,
) -> Result<(), String> {
    let attestation = &launch.egeo_attestation;
    if attestation.schema != D3D12_CAMERA_EGEO_ATTESTATION_SCHEMA_V1
        || attestation.canonicalization != D3D12_CAMERA_EGEO_CANONICALIZATION_V1
        || attestation.payload.schema != D3D12_CAMERA_EGEO_ATTESTATION_PAYLOAD_SCHEMA_V1
    {
        return Err("unsupported D3D12 E-geo attestation contract".into());
    }
    let canonical = canonical_egeo_payload(&attestation.payload)?;
    let actual_canonical_hash = sha256_hex(&canonical);
    if !is_sha256_digest(&actual_canonical_hash)
        || actual_canonical_hash != attestation.canonical_payload_sha256
    {
        return Err("D3D12 E-geo canonical payload hash mismatch".into());
    }
    let actual_report_hash = sha256_hex(egeo_report_bytes);
    if !is_sha256_digest(&actual_report_hash)
        || actual_report_hash != attestation.payload.egeo_report_sha256
    {
        return Err("D3D12 E-geo report content hash mismatch".into());
    }
    let claims = &attestation.payload.claims;
    if claims.process_launch_id != launch.process_launch_id
        || claims.executable_sha256 != launch.executable_sha256
        || claims.depth_profile_sha256 != launch.depth_profile_sha256
        || claims.view_metres_per_unit != launch.view_metres_per_unit
        || claims.axis_mapping != launch.axis_mapping
        || claims.view_source != launch.view_source
        || claims.projection_source != launch.projection_source
    {
        return Err(
            "D3D12 E-geo attestation does not bind this launch/build/profile/metric/axis/stable source"
                .into(),
        );
    }
    let report: Cp2077TypedEgeoReport = serde_json::from_slice(egeo_report_bytes)
        .map_err(|error| format!("parse typed CP2077 E-geo report failed: {error}"))?;
    if report.schema != CP2077_EGEO_REPORT_SCHEMA_V1
        || !report.non_trainable
        || !report.local_only
        || report.upload_allowed
        || report.e_geo.schema != CAMERA_EGEO_VALIDATION_SCHEMA_V3
        || !report.e_geo.passed
        || !report.e_geo.ready_for_independent_repeat
        || report.e_geo.production_binding_authorized
        || !report.e_geo.failures.is_empty()
    {
        return Err("typed CP2077 E-geo report did not pass its DEV-only repeat gate".into());
    }
    if report.e_geo.raw_depth_profile_sha256 != launch.depth_profile_sha256 {
        return Err("typed CP2077 E-geo report is bound to another depth profile".into());
    }
    Ok(())
}

fn validate_axis_motion(launch: &D3d12CameraAuthoringLaunchEvidence) -> Result<(), String> {
    let scale = launch
        .view_metres_per_unit
        .parse::<f64>()
        .map_err(|_| "D3D12 camera metric scale is not numeric")?;
    let expected = [
        (D3d12ControlledMotion::Forward, 2_usize),
        (D3d12ControlledMotion::StrafeRight, 0_usize),
        (D3d12ControlledMotion::VerticalUp, 1_usize),
    ];
    for (observation, (expected_motion, expected_axis)) in launch.axis_motion.iter().zip(expected) {
        if observation.motion != expected_motion {
            return Err(
                "D3D12 camera axis evidence must be forward/strafe-right/vertical-up in order"
                    .into(),
            );
        }
        if !observation
            .source_displacement_world_units
            .iter()
            .chain(observation.delivery_displacement_metres.iter())
            .all(|value| value.is_finite())
        {
            return Err("D3D12 camera axis evidence contains a non-finite displacement".into());
        }
        let expected_delivery = launch
            .axis_mapping
            .source_to_delivery(observation.source_displacement_world_units)
            .map(|value| value * scale);
        for (actual, expected) in observation
            .delivery_displacement_metres
            .iter()
            .zip(expected_delivery)
        {
            let tolerance = AXIS_MAPPING_ABSOLUTE_TOLERANCE_METRES
                + expected.abs() * AXIS_MAPPING_RELATIVE_TOLERANCE;
            if (*actual - expected).abs() > tolerance {
                return Err(
                    "D3D12 camera axis evidence contradicts its signed mapping or metric scale"
                        .into(),
                );
            }
        }
        let magnitude = vector_magnitude(observation.delivery_displacement_metres);
        if magnitude < MIN_AXIS_MOTION_METRES
            || observation.delivery_displacement_metres[expected_axis] / magnitude
                < AXIS_DIRECTION_MIN_COSINE
        {
            return Err(
                "D3D12 camera controlled motion has the wrong delivery direction or sign".into(),
            );
        }
    }
    Ok(())
}

fn validate_cross_launch_axis_correspondence(
    launch_a: &[D3d12AxisMotionObservation; 3],
    launch_b: &[D3d12AxisMotionObservation; 3],
) -> Result<(), String> {
    for (left, right) in launch_a.iter().zip(launch_b) {
        if left.motion != right.motion {
            return Err("D3D12 camera A/B controlled-motion labels differ".into());
        }
        let left_magnitude = vector_magnitude(left.delivery_displacement_metres);
        let right_magnitude = vector_magnitude(right.delivery_displacement_metres);
        let cosine = (0..3)
            .map(|axis| {
                left.delivery_displacement_metres[axis] * right.delivery_displacement_metres[axis]
            })
            .sum::<f64>()
            / (left_magnitude * right_magnitude);
        if !cosine.is_finite() || cosine < CROSS_LAUNCH_AXIS_MIN_COSINE {
            return Err("D3D12 camera A/B controlled-motion directions do not correspond".into());
        }
    }
    Ok(())
}

fn vector_magnitude(vector: [f64; 3]) -> f64 {
    vector.iter().map(|value| value * value).sum::<f64>().sqrt()
}

fn validate_fov_75_90_75(phases: &[D3d12FovPhaseObservation; 3]) -> Result<(), String> {
    for (phase, expected) in phases.iter().zip([75.0, 90.0, 75.0]) {
        if ![
            phase.configured_horizontal_fov_degrees,
            phase.observed_horizontal_fov_degrees,
            phase.fx_pixels,
            phase.fy_pixels,
        ]
        .iter()
        .all(|value| value.is_finite())
            || phase.fx_pixels <= 0.0
            || phase.fy_pixels <= 0.0
            || (phase.configured_horizontal_fov_degrees - expected).abs() > f64::EPSILON
            || (phase.observed_horizontal_fov_degrees - expected).abs()
                > FOV_SETTING_TOLERANCE_DEGREES
        {
            return Err("D3D12 camera FOV evidence is not a physical 75->90->75 sequence".into());
        }
    }
    let [first, middle, returned] = phases;
    if middle.fx_pixels >= first.fx_pixels * (1.0 - FOV_RESPONSE_MIN_RELATIVE_CHANGE)
        || middle.fy_pixels >= first.fy_pixels * (1.0 - FOV_RESPONSE_MIN_RELATIVE_CHANGE)
    {
        return Err("D3D12 camera projection did not causally respond to FOV 75->90".into());
    }
    if relative_difference(first.fx_pixels, returned.fx_pixels) > FOV_RETURN_RELATIVE_TOLERANCE
        || relative_difference(first.fy_pixels, returned.fy_pixels) > FOV_RETURN_RELATIVE_TOLERANCE
    {
        return Err("D3D12 camera projection did not return to the FOV-75 baseline".into());
    }
    Ok(())
}

fn independent_axis_rank(vectors: &[[f64; 3]]) -> usize {
    let mut basis = Vec::<[f64; 3]>::new();
    for vector in vectors {
        if !vector.iter().all(|value| value.is_finite()) {
            continue;
        }
        let magnitude = vector.iter().map(|value| value * value).sum::<f64>().sqrt();
        if magnitude < MIN_AXIS_MOTION_METRES {
            continue;
        }
        let mut residual = vector.map(|value| value / magnitude);
        for axis in &basis {
            let dot = (0..3)
                .map(|index| residual[index] * axis[index])
                .sum::<f64>();
            for index in 0..3 {
                residual[index] -= dot * axis[index];
            }
        }
        let residual_norm = residual
            .iter()
            .map(|value| value * value)
            .sum::<f64>()
            .sqrt();
        if residual_norm >= AXIS_INDEPENDENCE_RESIDUAL {
            basis.push(residual.map(|value| value / residual_norm));
            if basis.len() == 3 {
                break;
            }
        }
    }
    basis.len()
}

fn relative_difference(left: f64, right: f64) -> f64 {
    (left - right).abs() / left.abs().max(right.abs()).max(f64::MIN_POSITIVE)
}

fn valid_process_launch_id(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

pub fn analyze_camera_evidence(
    static_report: &CameraDiscoveryReport,
    walk_report: &CameraDiscoveryReport,
    turn_report: &CameraDiscoveryReport,
) -> Result<CameraEvidenceReport, String> {
    analyze_camera_evidence_for_phases(
        static_report,
        walk_report,
        turn_report,
        [POSE_STATIC_PHASE, POSE_WALK_PHASE, POSE_TURN_PHASE],
    )
}

pub fn analyze_reference_free_camera_candidate_review(
    static_report: &CameraDiscoveryReport,
    walk_report: &CameraDiscoveryReport,
    turn_report: &CameraDiscoveryReport,
) -> Result<ReferenceFreeCameraCandidateReview, String> {
    analyze_reference_free_camera_candidate_review_for_phases(
        static_report,
        walk_report,
        turn_report,
        [POSE_STATIC_PHASE, POSE_WALK_PHASE, POSE_TURN_PHASE],
    )
}

fn analyze_reference_free_camera_candidate_review_for_phases(
    static_report: &CameraDiscoveryReport,
    walk_report: &CameraDiscoveryReport,
    turn_report: &CameraDiscoveryReport,
    expected_phases: [&str; 3],
) -> Result<ReferenceFreeCameraCandidateReview, String> {
    let reports = [static_report, walk_report, turn_report];
    for (report, expected_phase) in reports.iter().zip(expected_phases) {
        validate_reference_free_phase(report, expected_phase)?;
    }
    validate_shared_contract(static_report, walk_report)?;
    validate_shared_contract(static_report, turn_report)?;
    let dsv_selection = static_report
        .dsv_selection
        .clone()
        .ok_or("reference-free camera review is missing its DSV selection binding")?;
    let selected_dsv_descriptor = static_report
        .selected_dsv_descriptor
        .ok_or("reference-free camera review is missing its selected DSV descriptor")?;
    let candidates = common_view_sources(&reports)
        .into_iter()
        .map(|source| {
            let phase_candidates = reports.map(|report| {
                report
                    .view_candidates
                    .iter()
                    .find(|candidate| candidate.source == source)
                    .expect("source came from the intersection")
            });
            ReferenceFreeCameraCandidate {
                source,
                static_motion: candidate_motion(phase_candidates[0]),
                walk_motion: candidate_motion(phase_candidates[1]),
                turn_motion: candidate_motion(phase_candidates[2]),
            }
        })
        .collect();
    let report = ReferenceFreeCameraCandidateReview {
        schema: CAMERA_REFERENCE_FREE_CANDIDATE_REVIEW_SCHEMA_V1.into(),
        status: "human_review_required".into(),
        phase_ids: reports.map(|report| report.phase_id.clone()),
        executable_sha256: static_report.config.executable_sha256.clone(),
        dsv_selection,
        selected_dsv_descriptor,
        candidates,
        human_review_required: true,
        metric_scale_available: false,
        automatic_selection_authorized: false,
        camera_binding_generated: false,
        recording_ready: false,
        deliverable: false,
        publication_allowed: false,
        requires_metric_scale_validation: true,
        requires_e_geo_validation: true,
        production_binding_authorized: false,
    };
    report.validate()?;
    Ok(report)
}

pub fn analyze_camera_evidence_for_phases(
    static_report: &CameraDiscoveryReport,
    walk_report: &CameraDiscoveryReport,
    turn_report: &CameraDiscoveryReport,
    expected_phases: [&str; 3],
) -> Result<CameraEvidenceReport, String> {
    analyze_camera_evidence_with_mode(
        static_report,
        walk_report,
        turn_report,
        expected_phases,
        CameraEvidenceAnalysisMode::RigidView,
    )
}

pub fn analyze_camera_evidence_with_mode(
    static_report: &CameraDiscoveryReport,
    walk_report: &CameraDiscoveryReport,
    turn_report: &CameraDiscoveryReport,
    expected_phases: [&str; 3],
    mode: CameraEvidenceAnalysisMode,
) -> Result<CameraEvidenceReport, String> {
    let reports = [static_report, walk_report, turn_report];
    for (report, expected_phase) in reports.iter().zip(expected_phases) {
        validate_phase(report, expected_phase)?;
    }
    validate_shared_contract(static_report, walk_report)?;
    validate_shared_contract(static_report, turn_report)?;
    if mode == CameraEvidenceAnalysisMode::FalloutRenderHybrid {
        for report in reports {
            validate_fallout_hybrid_reference(report)?;
        }
    }
    let metres_per_game_unit = static_report
        .config
        .comparison_camera_reference
        .as_ref()
        .ok_or("camera evidence is missing its exact-build comparison reference")?
        .metres_per_game_unit
        .parse::<f64>()
        .map_err(|error| format!("invalid camera metres_per_game_unit: {error}"))?;
    if !metres_per_game_unit.is_finite() || metres_per_game_unit <= 0.0 {
        return Err("camera metres_per_game_unit must be positive and finite".into());
    }

    let common_sources = common_view_sources(&reports);
    let mut candidates = Vec::new();
    let mut rejected_sources = Vec::new();
    for source in common_sources {
        let phase_candidates = reports.map(|report| {
            report
                .view_candidates
                .iter()
                .find(|candidate| candidate.source == source)
                .expect("source came from the intersection")
        });
        let (phase_motion, coupling, fallout_hybrid) = if mode
            == CameraEvidenceAnalysisMode::FalloutRenderHybrid
        {
            let decoded = reports
                .iter()
                .zip(phase_candidates)
                .map(|(report, candidate)| {
                    fallout_hybrid_phase(report, candidate, metres_per_game_unit)
                })
                .collect::<Result<Vec<_>, _>>();
            let decoded = match decoded {
                Ok(decoded) => decoded,
                Err(reason) => {
                    rejected_sources.push(RejectedCameraSource { source, reason });
                    continue;
                }
            };
            let ratios = [decoded[1].response_ratio, decoded[2].response_ratio];
            let evidence = FalloutHybridEvidence {
                reference_orientation_max_error_rad: std::array::from_fn(|i| decoded[i].max_error),
                walk_turn_rotation_response_ratio: ratios,
                reference_orientation_matches: decoded.iter().all(|phase| phase.max_error <= 0.02),
                rotation_response_matches: ratios
                    .iter()
                    .all(|ratio| ratio.is_some_and(|value| (0.5..=2.0).contains(&value))),
            };
            (
                std::array::from_fn(|i| decoded[i].motion),
                std::array::from_fn(|i| decoded[i].coupling),
                Some(evidence),
            )
        } else {
            (
                phase_candidates.map(candidate_motion),
                [
                    coupling_to_reference(
                        phase_candidates[0],
                        &static_report.comparison_camera_samples,
                    ),
                    coupling_to_reference(
                        phase_candidates[1],
                        &walk_report.comparison_camera_samples,
                    ),
                    coupling_to_reference(
                        phase_candidates[2],
                        &turn_report.comparison_camera_samples,
                    ),
                ],
                None,
            )
        };
        let enough_samples = phase_motion
            .iter()
            .all(|metrics| metrics.samples as usize >= MIN_PHASE_SAMPLES);
        let static_translation_p95_metres = phase_motion[0].translation_p95 * metres_per_game_unit;
        let static_absolute_stable = static_translation_p95_metres
            <= MAX_STATIC_TRANSLATION_P95_METRES
            && phase_motion[0].rotation_p95_rad <= MAX_STATIC_ROTATION_P95_RAD;
        let static_stable = static_absolute_stable
            && phase_motion[0].translation_p95
                <= (phase_motion[1].translation_p95 * STATIC_RESPONSE_RATIO)
            && phase_motion[0].rotation_p95_rad
                <= (phase_motion[2].rotation_p95_rad * STATIC_RESPONSE_RATIO);
        // Phase switches are operator-controlled, so a valid phase can contain idle boundary
        // samples while the operator returns to the game. p95 requires a sustained response in
        // more than a single outlier, while the static p95 remains the fail-closed noise floor.
        let walk_translation_response = phase_motion[1].translation_p95
            > phase_motion[0]
                .translation_p95
                .mul_add(4.0, MIN_TRANSLATION_RESPONSE);
        let turn_rotation_response = phase_motion[2].rotation_p95_rad
            > phase_motion[0]
                .rotation_p95_rad
                .mul_add(4.0, MIN_ROTATION_RESPONSE_RAD);
        let controlled_phase_pattern_matches = enough_samples
            && static_stable
            && walk_translation_response
            && turn_rotation_response
            && fallout_hybrid.as_ref().is_none_or(|evidence| {
                evidence.reference_orientation_matches && evidence.rotation_response_matches
            });
        candidates.push(CandidateEvidence {
            source,
            static_motion: phase_motion[0],
            walk_motion: phase_motion[1],
            turn_motion: phase_motion[2],
            static_coupling_to_legacy_reference: coupling[0],
            walk_coupling_to_legacy_reference: coupling[1],
            turn_coupling_to_legacy_reference: coupling[2],
            enough_samples,
            static_translation_p95_metres,
            static_absolute_stable,
            static_stable,
            walk_translation_response,
            turn_rotation_response,
            controlled_phase_pattern_matches,
            fallout_hybrid,
        });
    }
    candidates.sort_by_key(|candidate| candidate.source);
    let selection = selection_from_candidate_evidence(&candidates);
    Ok(CameraEvidenceReport {
        schema: match mode {
            CameraEvidenceAnalysisMode::RigidView => CAMERA_EVIDENCE_REPORT_SCHEMA_V2,
            CameraEvidenceAnalysisMode::FalloutRenderHybrid => FALLOUT_HYBRID_EVIDENCE_SCHEMA_V2,
        }
        .into(),
        phase_ids: reports.map(|report| report.phase_id.clone()),
        executable_sha256: static_report.config.executable_sha256.clone(),
        candidates,
        legacy_reference_static_motion: reference_motion(&static_report.comparison_camera_samples),
        legacy_reference_walk_motion: reference_motion(&walk_report.comparison_camera_samples),
        legacy_reference_turn_motion: reference_motion(&turn_report.comparison_camera_samples),
        selection,
        requires_e_geo_validation: true,
        production_binding_authorized: false,
        fallout_position_reference: (mode == CameraEvidenceAnalysisMode::FalloutRenderHybrid).then(
            || {
                static_report
                    .config
                    .comparison_camera_reference
                    .clone()
                    .unwrap()
            },
        ),
        rejected_sources,
    })
}

pub fn analyze_camera_evidence_sweep(
    inputs: impl IntoIterator<Item = CameraEvidenceSweepInput>,
) -> Result<CameraEvidenceSweepReport, String> {
    let mut builder = CameraEvidenceSweepBuilder::new();
    for input in inputs {
        builder.push(input)?;
    }
    builder.finish()
}

pub fn analyze_reference_free_camera_candidate_review_sweep(
    inputs: impl IntoIterator<Item = CameraEvidenceSweepInput>,
) -> Result<ReferenceFreeCameraCandidateReviewSweep, String> {
    let mut builder = ReferenceFreeCameraCandidateReviewSweepBuilder::new();
    for input in inputs {
        builder.push(input)?;
    }
    builder.finish()
}

fn validate_sweep_contract(
    baseline_render: &CameraDiscoveryRenderIdentity,
    baseline_config: &CameraDiscoveryConfig,
    baseline_selected_dsv_descriptor: Option<crate::DsvDiscoveryDescriptor>,
    other: &CameraDiscoveryReport,
) -> Result<(), String> {
    if *baseline_render != other.render
        || baseline_config.schema != other.config.schema
        || baseline_config.dsv_selection != other.config.dsv_selection
        || baseline_selected_dsv_descriptor != other.selected_dsv_descriptor
        || baseline_config.executable_sha256 != other.config.executable_sha256
        || baseline_config.sample_every_n_presents != other.config.sample_every_n_presents
        || baseline_config.max_main_passes_per_present != other.config.max_main_passes_per_present
        || baseline_config.draw_window_count != other.config.draw_window_count
        || baseline_config.comparison_camera_reference != other.config.comparison_camera_reference
    {
        return Err(format!(
            "draw window {} does not share the sweep executable/render/sampling/reference contract",
            other.config.draw_window_start
        ));
    }
    Ok(())
}

#[derive(Debug)]
struct EquivalentSourceGroup {
    source: ViewSourceBinding,
    draw_ordinals: BTreeSet<u32>,
}

fn normalized_draw_source(mut source: ViewSourceBinding) -> ViewSourceBinding {
    source.source.draw_ordinal = 0;
    source
}

fn candidate_equal_ignoring_draw(a: &CandidateEvidence, b: &CandidateEvidence) -> bool {
    let mut a = a.clone();
    let mut b = b.clone();
    a.source = normalized_draw_source(a.source);
    b.source = normalized_draw_source(b.source);
    a == b
}

fn selection_from_candidate_evidence(candidates: &[CandidateEvidence]) -> CameraEvidenceSelection {
    let mut groups: Vec<(CandidateEvidence, EquivalentSourceGroup)> = Vec::new();
    for candidate in candidates
        .iter()
        .filter(|candidate| candidate.controlled_phase_pattern_matches)
    {
        if let Some((_, group)) = groups
            .iter_mut()
            .find(|(representative, _)| candidate_equal_ignoring_draw(representative, candidate))
        {
            group
                .draw_ordinals
                .insert(candidate.source.source.draw_ordinal);
            if candidate.source < group.source {
                group.source = candidate.source;
            }
        } else {
            groups.push((
                candidate.clone(),
                EquivalentSourceGroup {
                    source: candidate.source,
                    draw_ordinals: BTreeSet::from([candidate.source.source.draw_ordinal]),
                },
            ));
        }
    }
    selection_from_groups(groups.into_iter().map(|(_, group)| group).collect())
}

pub(crate) fn selection_from_window_reports(
    windows: &[CameraEvidenceSweepWindow],
) -> CameraEvidenceSelection {
    let mut groups = BTreeMap::<ViewSourceBinding, EquivalentSourceGroup>::new();
    for window in windows {
        let sources = match &window.evidence.selection {
            CameraEvidenceSelection::Selected {
                source,
                equivalent_draw_ordinals,
            } => vec![(*source, equivalent_draw_ordinals.as_slice())],
            CameraEvidenceSelection::Ambiguous { matching_sources } => matching_sources
                .iter()
                .map(|source| (*source, std::slice::from_ref(&source.source.draw_ordinal)))
                .collect(),
            CameraEvidenceSelection::NoMatch => Vec::new(),
        };
        for (source, draws) in sources {
            let key = normalized_draw_source(source);
            let group = groups.entry(key).or_insert_with(|| EquivalentSourceGroup {
                source,
                draw_ordinals: BTreeSet::new(),
            });
            group.draw_ordinals.extend(draws.iter().copied());
            if source < group.source {
                group.source = source;
            }
        }
    }
    selection_from_groups(groups.into_values().collect())
}

fn selection_from_groups(mut groups: Vec<EquivalentSourceGroup>) -> CameraEvidenceSelection {
    groups.sort_by_key(|group| group.source);
    match groups.as_slice() {
        [group] => CameraEvidenceSelection::Selected {
            source: group.source,
            equivalent_draw_ordinals: group.draw_ordinals.iter().copied().collect(),
        },
        [] => CameraEvidenceSelection::NoMatch,
        _ => CameraEvidenceSelection::Ambiguous {
            matching_sources: groups.iter().map(|group| group.source).collect(),
        },
    }
}

fn validate_phase(report: &CameraDiscoveryReport, expected_phase: &str) -> Result<(), String> {
    validate_phase_common(report, expected_phase)?;
    if report.config.comparison_camera_reference.is_none() {
        return Err("camera evidence is missing its exact-build comparison reference".into());
    }
    if report.comparison_camera_samples.len() < MIN_PHASE_SAMPLES {
        return Err(format!(
            "phase {expected_phase} has only {} comparison-camera samples; need at least {MIN_PHASE_SAMPLES}",
            report.comparison_camera_samples.len()
        ));
    }
    Ok(())
}

fn validate_phase_common(
    report: &CameraDiscoveryReport,
    expected_phase: &str,
) -> Result<(), String> {
    report.validate_dsv_selection_binding()?;
    if report.schema != CAMERA_DISCOVERY_REPORT_SCHEMA_V2
        && report.schema != CAMERA_DISCOVERY_REPORT_SCHEMA_V3
        && report.schema != CAMERA_DISCOVERY_REPORT_SCHEMA_V4
    {
        return Err(format!(
            "phase {expected_phase} report schema must be {CAMERA_DISCOVERY_REPORT_SCHEMA_V2} or {CAMERA_DISCOVERY_REPORT_SCHEMA_V3}, got {}",
            report.schema
        ));
    }
    if report.phase_id != expected_phase || report.config.phase_id != expected_phase {
        return Err(format!(
            "expected phase {expected_phase}, got report={} config={}",
            report.phase_id, report.config.phase_id
        ));
    }
    if !report.reliable || !report.blockers.is_empty() {
        return Err(format!(
            "phase {expected_phase} is not reliable: {:?}",
            report.blockers
        ));
    }
    Ok(())
}

fn validate_reference_free_phase(
    report: &CameraDiscoveryReport,
    expected_phase: &str,
) -> Result<(), String> {
    validate_phase_common(report, expected_phase)?;
    if report.config.schema != CAMERA_DISCOVERY_SCHEMA_V3
        || report.config.comparison_camera_reference.is_some()
    {
        return Err(
            "reference-free camera review requires config/v3 without a comparison reference".into(),
        );
    }
    if !report.comparison_camera_samples.is_empty() || !report.comparison_camera_errors.is_empty() {
        return Err(
            "reference-free camera review rejects comparison samples or errors without a reference"
                .into(),
        );
    }
    let selection = report
        .dsv_selection
        .as_ref()
        .ok_or("reference-free camera review is missing its DSV selection binding")?;
    validate_reference_free_selection_binding(selection)?;
    if !is_sha256_digest(&report.config.executable_sha256) {
        return Err("reference-free camera review executable hash is not canonical".into());
    }
    report
        .selected_dsv_descriptor
        .ok_or("reference-free camera review is missing its selected DSV descriptor")?
        .validate()?;
    let config = &report.config;
    if config.sample_every_n_presents == 0
        || !(1..=16).contains(&config.max_main_passes_per_present)
        || !(1..=256).contains(&config.draw_window_count)
        || config
            .draw_window_start
            .checked_add(config.draw_window_count)
            .is_none_or(|end| end > 512)
        || config
            .max_main_passes_per_present
            .checked_mul(config.draw_window_count)
            .is_none_or(|draws| draws > MAX_DISCOVERY_DRAWS_PER_SAMPLED_PRESENT)
    {
        return Err(
            "reference-free camera review config window/sampling bounds are invalid".into(),
        );
    }
    let sources = report
        .view_candidates
        .iter()
        .map(|candidate| candidate.source)
        .collect::<BTreeSet<_>>();
    if sources.len() != report.view_candidates.len() {
        return Err("reference-free camera review phase repeats a view source".into());
    }
    Ok(())
}

fn validate_reference_free_selection_binding(
    selection: &CameraDiscoveryDsvSelection,
) -> Result<(), String> {
    if selection.evidence_root.is_empty()
        || selection.evidence_root.len() > 32_768
        || !Path::new(&selection.evidence_root).is_absolute()
    {
        return Err(
            "reference-free camera review evidence root is not a bounded absolute path".into(),
        );
    }
    if !is_sha256_digest(&selection.attestation_sha256) {
        return Err("reference-free camera review attestation hash is not canonical".into());
    }
    let locator = Path::new(&selection.attestation_locator);
    let components = locator.components().collect::<Vec<_>>();
    let safe_leaf = match components.as_slice() {
        [Component::Normal(leaf)] => leaf.to_str().is_some_and(|leaf| {
            !leaf.is_empty()
                && leaf.len() <= 128
                && leaf.ends_with(".json")
                && leaf.as_bytes()[0].is_ascii_alphanumeric()
                && leaf
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
        }),
        _ => false,
    };
    if selection.attestation_locator.contains('\\') || !safe_leaf {
        return Err(
            "reference-free camera review attestation locator is not one safe JSON leaf".into(),
        );
    }
    Ok(())
}

fn validate_reference_free_phase_ids(
    phase_ids: &[String; 3],
    expected_window_start: Option<u32>,
) -> Result<(), String> {
    let prefixes = [POSE_STATIC_PHASE, POSE_WALK_PHASE, POSE_TURN_PHASE];
    let suffixes = phase_ids
        .iter()
        .zip(prefixes)
        .map(|(phase_id, prefix)| {
            phase_id
                .strip_prefix(prefix)
                .ok_or("reference-free camera review phase role is invalid")
        })
        .collect::<Result<Vec<_>, _>>()?;
    if suffixes[0] != suffixes[1] || suffixes[0] != suffixes[2] {
        return Err("reference-free camera review phase suffixes differ".into());
    }
    let suffix = suffixes[0];
    match expected_window_start {
        Some(start) if suffix == format!("_W{start:03}") => Ok(()),
        Some(_) => Err("reference-free camera review phase/window identity differs".into()),
        None if suffix.is_empty() => Ok(()),
        None => {
            let digits = suffix
                .strip_prefix("_W")
                .ok_or("reference-free camera review phase suffix is invalid")?;
            if digits.len() != 3 || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err("reference-free camera review phase suffix is invalid".into());
            }
            let start = digits
                .parse::<u32>()
                .map_err(|_| "reference-free camera review phase suffix is invalid")?;
            if start > 511 || digits != format!("{start:03}") {
                return Err("reference-free camera review phase suffix is invalid".into());
            }
            Ok(())
        }
    }
}

fn validate_raw_motion_stats(motion: MotionStats) -> Result<(), String> {
    if motion.steps > motion.samples.saturating_sub(1)
        || [
            motion.translation_median,
            motion.translation_p95,
            motion.rotation_median_rad,
            motion.rotation_p95_rad,
        ]
        .iter()
        .any(|value| !value.is_finite() || *value < 0.0)
    {
        return Err("reference-free camera review contains invalid raw motion statistics".into());
    }
    Ok(())
}

fn common_reference_free_window_sources(
    windows: &[ReferenceFreeCameraCandidateReviewSweepWindow],
) -> Vec<ViewSourceBinding> {
    let Some(first) = windows.first() else {
        return Vec::new();
    };
    let mut common = first
        .review
        .candidates
        .iter()
        .map(|candidate| normalized_draw_source(candidate.source))
        .collect::<BTreeSet<_>>();
    for window in &windows[1..] {
        let sources = window
            .review
            .candidates
            .iter()
            .map(|candidate| normalized_draw_source(candidate.source))
            .collect::<BTreeSet<_>>();
        common.retain(|source| sources.contains(source));
    }
    common.into_iter().collect()
}

fn validate_shared_contract(
    baseline: &CameraDiscoveryReport,
    other: &CameraDiscoveryReport,
) -> Result<(), String> {
    if baseline.schema != other.schema
        || baseline.render != other.render
        || baseline.dsv_selection != other.dsv_selection
        || baseline.selected_dsv_descriptor != other.selected_dsv_descriptor
        || baseline.config.executable_sha256 != other.config.executable_sha256
        || baseline.config.sample_every_n_presents != other.config.sample_every_n_presents
        || baseline.config.max_main_passes_per_present != other.config.max_main_passes_per_present
        || baseline.config.draw_window_start != other.config.draw_window_start
        || baseline.config.draw_window_count != other.config.draw_window_count
        || baseline.config.comparison_camera_reference != other.config.comparison_camera_reference
    {
        return Err(format!(
            "phase {} does not share the baseline executable/render/window/reference contract",
            other.phase_id
        ));
    }
    Ok(())
}

fn common_view_sources(reports: &[&CameraDiscoveryReport; 3]) -> BTreeSet<ViewSourceBinding> {
    let mut common = reports[0]
        .view_candidates
        .iter()
        .map(|candidate| candidate.source)
        .collect::<BTreeSet<_>>();
    for report in &reports[1..] {
        let sources = report
            .view_candidates
            .iter()
            .map(|candidate| candidate.source)
            .collect::<BTreeSet<_>>();
        common.retain(|source| sources.contains(source));
    }
    common
}

fn validate_fallout_hybrid_reference(report: &CameraDiscoveryReport) -> Result<(), String> {
    if report.schema != CAMERA_DISCOVERY_REPORT_SCHEMA_V4
        || !is_sha256_digest(&report.config.executable_sha256)
    {
        return Err(
            "Fallout hybrid analysis requires report/v4 exact-draw viewport evidence and an exact executable hash; resample old reports".into(),
        );
    }
    let reference = report
        .config
        .comparison_camera_reference
        .as_ref()
        .ok_or("Fallout hybrid analysis requires an exact-build position reference")?;
    reference
        .validate()
        .map_err(|error| format!("invalid Fallout reference: {error}"))?;
    if reference.provider != "fallout4-player-camera-root-exact-build/v1"
        || reference.axis_mapping != "x_z_y"
    {
        return Err("Fallout hybrid reference provider or axis convention is unsupported".into());
    }
    let sampling = report.sampling;
    if report.candidate_overflows != 0
        || report.view_observation_overflows != 0
        || !report.comparison_camera_errors.is_empty()
        || sampling.queue_dropped_draws != 0
        || sampling.runtime_dropped_callbacks != 0
        || sampling.tracked_buffer_overflows != 0
        || sampling.dsv_ambiguities != 0
        || report.processed_draws == 0
        || report.processed_draws != sampling.submitted_draws
        || report.draw_contexts.len() as u64 != report.processed_draws
        || sampling.phase_boundary_discarded_presents > 1
        || sampling.phase_boundary_discarded_draws
            > u64::from(report.config.draw_window_count)
                * u64::from(report.config.max_main_passes_per_present)
    {
        return Err("Fallout hybrid report has invalid capture accounting or errors".into());
    }
    if report.comparison_camera_samples.windows(2).any(|rows| {
        rows[1].present_seq <= rows[0].present_seq || rows[1].present_qpc <= rows[0].present_qpc
    }) {
        return Err("Fallout reference Presents and QPC must be strictly increasing".into());
    }
    for sample in &report.comparison_camera_samples {
        if sample
            .camera_position
            .iter()
            .any(|value| !value.is_finite())
        {
            return Err("Fallout reference position is not finite".into());
        }
        normalized_camera_quaternion(sample.camera_rotation_quaternion)?;
    }
    let sources = report
        .view_candidates
        .iter()
        .map(|candidate| candidate.source)
        .collect::<BTreeSet<_>>();
    if sources.len() != report.view_candidates.len() {
        return Err("Fallout hybrid report repeats a candidate source".into());
    }
    Ok(())
}

/// Only normalize an already valid proper-rotation quaternion. This must never repair a
/// reflected matrix, a corrupt reference, or a zero quaternion into plausible evidence.
pub(crate) fn normalized_camera_quaternion(rotation: [f64; 4]) -> Result<[f64; 4], String> {
    let norm_squared = rotation.iter().map(|value| value * value).sum::<f64>();
    if !norm_squared.is_finite() || (norm_squared - 1.0).abs() > 0.002 {
        return Err("camera quaternion is not finite and unit length".into());
    }
    Ok(rotation.map(|value| value / norm_squared.sqrt()))
}

struct FalloutHybridPhase {
    motion: MotionStats,
    coupling: PhaseCoupling,
    max_error: f64,
    response_ratio: Option<f64>,
}

fn fallout_hybrid_phase(
    report: &CameraDiscoveryReport,
    candidate: &DiscoveryViewCandidate,
    metres_per_game_unit: f64,
) -> Result<FalloutHybridPhase, String> {
    report.validate_main_scene_draw(candidate)?;
    if candidate.observations.len() < MIN_PHASE_SAMPLES
        || candidate.frames as usize != candidate.observations.len()
        || candidate
            .observations
            .windows(2)
            .any(|rows| rows[1].present_seq <= rows[0].present_seq)
    {
        return Err(format!(
            "{}: insufficient, unordered or inconsistent observations",
            report.phase_id
        ));
    }
    let reference = report
        .comparison_camera_samples
        .iter()
        .map(|row| (row.present_seq, row))
        .collect::<BTreeMap<_, _>>();
    let mut poses = Vec::new();
    let mut reference_poses = Vec::new();
    let mut max_error = 0.0_f64;
    for observation in &candidate.observations {
        let rotation =
            fallout_render_rotation_from_basis(&observation.matrix, candidate.source.translation)
                .ok_or_else(|| {
                format!(
                    "{}: invalid left-handed Fallout basis at Present {}",
                    report.phase_id, observation.present_seq
                )
            })?;
        let rotation = normalized_camera_quaternion(rotation.map(f64::from))?;
        let row = reference.get(&observation.present_seq).ok_or_else(|| {
            format!(
                "{}: no exact reference for Present {}",
                report.phase_id, observation.present_seq
            )
        })?;
        let reference_rotation = normalized_camera_quaternion(row.camera_rotation_quaternion)?;
        max_error = max_error.max(quaternion_angle(rotation, reference_rotation));
        // Preserve the existing motion-stat units (game units); the explicit reference field
        // declares that this translation is inherited, not independent GPU position evidence.
        let position = row
            .camera_position
            .map(|value| value / metres_per_game_unit);
        if position.iter().any(|value| !value.is_finite()) {
            return Err("Fallout hybrid position conversion is not finite".into());
        }
        poses.push((observation.present_seq, position, rotation));
        reference_poses.push((observation.present_seq, position, reference_rotation));
    }
    let motion = motion_stats(poses.iter().copied());
    let reference_motion = motion_stats(reference_poses.iter().copied());
    let (_, rotation_steps) = motion_steps(&poses);
    let (_, reference_steps) = motion_steps(&reference_poses);
    let response_ratio = (reference_motion.rotation_p95_rad > 0.0)
        .then(|| motion.rotation_p95_rad / reference_motion.rotation_p95_rad)
        .filter(|ratio| ratio.is_finite());
    Ok(FalloutHybridPhase {
        motion,
        coupling: PhaseCoupling {
            joined_samples: poses.len() as u64,
            // The position source is shared: correlation would be tautological, not evidence.
            translation_magnitude_correlation: None,
            rotation_magnitude_correlation: correlation(
                &rotation_steps
                    .iter()
                    .map(|value| *value as f32)
                    .collect::<Vec<_>>(),
                &reference_steps
                    .iter()
                    .map(|value| *value as f32)
                    .collect::<Vec<_>>(),
            ),
        },
        max_error,
        response_ratio,
    })
}

fn candidate_motion(candidate: &DiscoveryViewCandidate) -> MotionStats {
    let poses = candidate
        .observations
        .iter()
        .map(|observation| {
            (
                observation.present_seq,
                pose_from_view(&observation.matrix, candidate.source.translation),
            )
        })
        .collect::<Vec<_>>();
    motion_stats(poses.iter().map(|(seq, pose)| {
        (
            *seq,
            pose.position.map(f64::from),
            pose.rotation.map(f64::from),
        )
    }))
}

fn reference_motion(samples: &[DiscoveryReferenceCameraObservation]) -> MotionStats {
    motion_stats(samples.iter().map(|sample| {
        (
            sample.present_seq,
            sample.camera_position,
            sample.camera_rotation_quaternion,
        )
    }))
}

fn motion_stats<I>(samples: I) -> MotionStats
where
    I: IntoIterator<Item = (u64, [f64; 3], [f64; 4])>,
{
    let rows = samples.into_iter().collect::<Vec<_>>();
    let (mut translation, mut rotation) = motion_steps(&rows);
    MotionStats {
        samples: rows.len() as u64,
        steps: translation.len() as u64,
        translation_median: quantile(&mut translation, 0.5),
        translation_p95: quantile(&mut translation, 0.95),
        rotation_median_rad: quantile(&mut rotation, 0.5),
        rotation_p95_rad: quantile(&mut rotation, 0.95),
    }
}

fn coupling_to_reference(
    candidate: &DiscoveryViewCandidate,
    reference: &[DiscoveryReferenceCameraObservation],
) -> PhaseCoupling {
    let candidate_by_seq = candidate
        .observations
        .iter()
        .map(|observation| {
            let pose: CameraPose =
                pose_from_view(&observation.matrix, candidate.source.translation);
            (
                observation.present_seq,
                (pose.position.map(f64::from), pose.rotation.map(f64::from)),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let reference_by_seq = reference
        .iter()
        .map(|sample| {
            (
                sample.present_seq,
                (sample.camera_position, sample.camera_rotation_quaternion),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let joined = candidate_by_seq
        .iter()
        .filter_map(|(&seq, &(position, rotation))| {
            reference_by_seq
                .get(&seq)
                .map(|&(reference_position, reference_rotation)| {
                    (
                        seq,
                        position,
                        rotation,
                        reference_position,
                        reference_rotation,
                    )
                })
        })
        .collect::<Vec<_>>();
    let mut candidate_translation = Vec::new();
    let mut candidate_rotation = Vec::new();
    let mut reference_translation = Vec::new();
    let mut reference_rotation = Vec::new();
    for pair in joined.windows(2) {
        candidate_translation.push(distance(pair[0].1, pair[1].1) as f32);
        candidate_rotation.push(quaternion_angle(pair[0].2, pair[1].2) as f32);
        reference_translation.push(distance(pair[0].3, pair[1].3) as f32);
        reference_rotation.push(quaternion_angle(pair[0].4, pair[1].4) as f32);
    }
    PhaseCoupling {
        joined_samples: joined.len() as u64,
        translation_magnitude_correlation: correlation(
            &candidate_translation,
            &reference_translation,
        ),
        rotation_magnitude_correlation: correlation(&candidate_rotation, &reference_rotation),
    }
}

fn motion_steps(rows: &[(u64, [f64; 3], [f64; 4])]) -> (Vec<f64>, Vec<f64>) {
    let mut translation = Vec::new();
    let mut rotation = Vec::new();
    for pair in rows.windows(2) {
        if pair[1].0 <= pair[0].0 {
            continue;
        }
        translation.push(distance(pair[0].1, pair[1].1));
        rotation.push(quaternion_angle(pair[0].2, pair[1].2));
    }
    (translation, rotation)
}

fn distance(a: [f64; 3], b: [f64; 3]) -> f64 {
    (0..3)
        .map(|axis| (b[axis] - a[axis]).powi(2))
        .sum::<f64>()
        .sqrt()
}

fn quaternion_angle(a: [f64; 4], b: [f64; 4]) -> f64 {
    let dot = (0..4)
        .map(|axis| a[axis] * b[axis])
        .sum::<f64>()
        .abs()
        .clamp(0.0, 1.0);
    2.0 * dot.acos()
}

fn quantile(values: &mut [f64], q: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(|a, b| a.total_cmp(b));
    let index = ((values.len() - 1) as f64 * q).round() as usize;
    values[index]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DsvDiscoveryDescriptor;
    use crate::camera::{Mat4, TranslationSlot};
    use crate::camera_binding::{
        D3d12AxisMapping, D3d12DescriptorTableBinding, D3d12DrawRelation, D3d12MatrixSourceBinding,
        D3d12SignedAxis, D3d12ViewSourceBinding, GameCameraBinding, MatrixEncoding, MatrixLayout,
        MatrixRole, MatrixSourceBinding, ShaderStage,
    };
    use crate::camera_discovery::{
        DiscoveryProjectionCandidate, DiscoverySamplingStats, DiscoveryViewObservation,
    };
    use crate::probe_config::{
        CAMERA_DISCOVERY_SCHEMA_V2, CAMERA_DISCOVERY_SCHEMA_V3, CameraDiscoveryConfig,
        CameraDiscoveryDsvSelection, CameraDiscoveryRenderIdentity, DiscoveryDepthFormat,
        render_identity_from_dsv_descriptor,
    };

    fn source(draw: u32) -> ViewSourceBinding {
        ViewSourceBinding {
            source: MatrixSourceBinding {
                shader_stage: ShaderStage::Vertex,
                bind_slot: 5,
                pass_ordinal: 0,
                draw_ordinal: draw,
                buffer_bytes: 256,
                offset: 64,
                encoding: MatrixEncoding::F32LittleEndian,
                layout: MatrixLayout::RowMajor,
                transpose: false,
                role: MatrixRole::View,
            },
            translation: TranslationSlot::Row3,
        }
    }

    fn view(position_x: f32, yaw: f32) -> Mat4 {
        let (sin, cos) = yaw.sin_cos();
        let r = [[cos, 0.0, sin], [0.0, 1.0, 0.0], [-sin, 0.0, cos]];
        let rv = [
            [r[0][0], r[1][0], r[2][0]],
            [r[0][1], r[1][1], r[2][1]],
            [r[0][2], r[1][2], r[2][2]],
        ];
        let p = [position_x, 0.0, 0.0];
        let tv = [
            -(rv[0][0] * p[0] + rv[0][1] * p[1] + rv[0][2] * p[2]),
            -(rv[1][0] * p[0] + rv[1][1] * p[1] + rv[1][2] * p[2]),
            -(rv[2][0] * p[0] + rv[2][1] * p[1] + rv[2][2] * p[2]),
        ];
        [
            [rv[0][0], rv[0][1], rv[0][2], 0.0],
            [rv[1][0], rv[1][1], rv[1][2], 0.0],
            [rv[2][0], rv[2][1], rv[2][2], 0.0],
            [tv[0], tv[1], tv[2], 1.0],
        ]
    }

    fn report(phase: &str, motion: impl Fn(usize) -> (f32, f32)) -> CameraDiscoveryReport {
        let source = source(7);
        let observations = (0..12)
            .map(|index| {
                let (x, yaw) = motion(index);
                DiscoveryViewObservation {
                    present_seq: 100 + index as u64 * 30,
                    matrix: view(x, yaw),
                }
            })
            .collect::<Vec<_>>();
        let reference = observations
            .iter()
            .map(|observation| {
                let pose = pose_from_view(&observation.matrix, source.translation);
                DiscoveryReferenceCameraObservation {
                    present_seq: observation.present_seq,
                    present_qpc: observation.present_seq as i64 * 10,
                    camera_position: pose.position.map(f64::from),
                    camera_rotation_quaternion: pose.rotation.map(f64::from),
                }
            })
            .collect::<Vec<_>>();
        let config = CameraDiscoveryConfig {
            schema: CAMERA_DISCOVERY_SCHEMA_V2.into(),
            phase_id: phase.into(),
            executable_sha256: format!("sha256:{}", "0".repeat(64)),
            render: Some(CameraDiscoveryRenderIdentity {
                width: 1920,
                height: 1080,
                sample_count: 1,
                formats: vec![DiscoveryDepthFormat::R24G8Typeless],
            }),
            dsv_selection: None,
            sample_every_n_presents: 30,
            max_main_passes_per_present: 4,
            draw_window_start: 0,
            draw_window_count: 128,
            comparison_camera_reference: Some(GameCameraBinding {
                provider: "fallout4-player-camera-root-exact-build/v1".into(),
                camera_singleton_relocation: 1,
                camera_root_offset: 2,
                world_transform_offset: 3,
                metres_per_game_unit: "1".into(),
                axis_mapping: "x_z_y".into(),
            }),
        };
        CameraDiscoveryReport {
            schema: CAMERA_DISCOVERY_REPORT_SCHEMA_V2.into(),
            phase_id: phase.into(),
            render: config.render.clone().unwrap(),
            config,
            dsv_selection: None,
            selected_dsv_descriptor: None,
            sampling: DiscoverySamplingStats {
                submitted_draws: 12,
                scheduled_draws: 12,
                ..DiscoverySamplingStats::default()
            },
            processed_draws: 12,
            candidate_overflows: 0,
            view_observation_overflows: 0,
            reliable: true,
            blockers: Vec::new(),
            comparison_camera_samples: reference,
            comparison_camera_errors: Vec::new(),
            view_candidates: vec![DiscoveryViewCandidate {
                source,
                frames: 12,
                matrix: observations.last().unwrap().matrix,
                distinct_values: 12,
                observations,
            }],
            projection_candidates: Vec::<DiscoveryProjectionCandidate>::new(),
            draw_contexts: Vec::new(),
        }
    }

    fn hybrid_reports() -> [CameraDiscoveryReport; 3] {
        let mut reports = [
            report(POSE_STATIC_PHASE, |_| (0.0, 0.6)),
            report(POSE_WALK_PHASE, |i| (i as f32, 0.6 + i as f32 * 0.002)),
            report(POSE_TURN_PHASE, |i| (0.0, 0.6 + i as f32 * 0.08)),
        ];
        for report in &mut reports {
            report.schema = CAMERA_DISCOVERY_REPORT_SCHEMA_V4.into();
            for (i, reference) in report.comparison_camera_samples.iter_mut().enumerate() {
                reference.camera_position = if report.phase_id == POSE_WALK_PHASE {
                    [i as f64, 0.0, 0.0]
                } else {
                    [0.0; 3]
                };
            }
            for candidate in &mut report.view_candidates {
                candidate.source.source.bind_slot = 12;
                candidate.source.source.buffer_bytes = 752;
                candidate.source.source.offset = 0;
                for observation in &mut candidate.observations {
                    // Synthetic fixture only: encode a known proper rotation as Fallout's
                    // camera-relative reflected basis. Position lives only in the reference.
                    for row in &mut observation.matrix[..3] {
                        row.swap(1, 2);
                    }
                    observation.matrix[3] = [0.0, 0.0, 0.0, 1.0];
                }
                candidate.matrix = candidate.observations.last().unwrap().matrix;
                report
                    .draw_contexts
                    .extend(candidate.observations.iter().map(|row| {
                        crate::camera_discovery::DiscoveryDrawContext {
                            present_seq: row.present_seq,
                            pass_ordinal: candidate.source.source.pass_ordinal,
                            draw_ordinal: candidate.source.source.draw_ordinal,
                            viewport: Some(crate::camera_discovery::DiscoveryViewport {
                                count: 1,
                                x: 0.0,
                                y: 0.0,
                                width: report.render.width as f32,
                                height: report.render.height as f32,
                                min_depth: 0.0098,
                                max_depth: 1.0,
                            }),
                        }
                    }));
            }
        }
        reports
    }

    fn hybrid_analysis(
        reports: &[CameraDiscoveryReport; 3],
    ) -> Result<CameraEvidenceReport, String> {
        analyze_camera_evidence_with_mode(
            &reports[0],
            &reports[1],
            &reports[2],
            [POSE_STATIC_PHASE, POSE_WALK_PHASE, POSE_TURN_PHASE],
            CameraEvidenceAnalysisMode::FalloutRenderHybrid,
        )
    }

    #[test]
    fn fallout_hybrid_decodes_reflected_basis_without_changing_legacy_analysis() {
        let reports = hybrid_reports();
        let legacy = analyze_camera_evidence(&reports[0], &reports[1], &reports[2]).unwrap();
        assert_eq!(legacy.selection, CameraEvidenceSelection::NoMatch);
        let legacy_json = serde_json::to_value(&legacy).unwrap();
        assert!(legacy_json.get("fallout_position_reference").is_none());
        assert!(legacy_json["candidates"][0].get("fallout_hybrid").is_none());
        let evidence = hybrid_analysis(&reports).unwrap();
        assert!(matches!(
            evidence.selection,
            CameraEvidenceSelection::Selected { .. }
        ));
        assert_eq!(evidence.schema, FALLOUT_HYBRID_EVIDENCE_SCHEMA_V2);
        assert_eq!(
            evidence.fallout_position_reference,
            reports[0].config.comparison_camera_reference
        );
        assert!(evidence.requires_e_geo_validation && !evidence.production_binding_authorized);
        let candidate = &evidence.candidates[0];
        assert!(candidate.static_motion.rotation_p95_rad < 1e-7);
        assert!((candidate.walk_motion.translation_p95 - 1.0).abs() < 1e-5);
        assert!(
            candidate
                .walk_coupling_to_legacy_reference
                .translation_magnitude_correlation
                .is_none()
        );
        assert!(
            candidate
                .fallout_hybrid
                .as_ref()
                .unwrap()
                .reference_orientation_max_error_rad
                .iter()
                .all(|value| *value < 1e-6)
        );
    }

    #[test]
    fn fallout_hybrid_rejects_inverse_orientation_and_keeps_true_ambiguity() {
        let mut reports = hybrid_reports();
        for report in &mut reports {
            let mut duplicate = report.view_candidates[0].clone();
            duplicate.source.source.offset = 192;
            report.view_candidates.push(duplicate);
            // The sweep collapses equivalent draws within a pass; it is not identical to a
            // window's uncollapsed selection. Replay must preserve that distinction.
            let mut equivalent = report.view_candidates[0].clone();
            equivalent.source.source.draw_ordinal = 5;
            report.view_candidates.push(equivalent);
        }
        assert!(matches!(
            hybrid_analysis(&reports).unwrap().selection,
            CameraEvidenceSelection::Ambiguous { .. }
        ));
        for report in &mut reports {
            for observation in &mut report.view_candidates[1].observations {
                let original = observation.matrix;
                // Inverse basis with the same relative motion, but the wrong absolute direction.
                observation.matrix =
                    std::array::from_fn(|i| std::array::from_fn(|j| original[j][i]));
            }
            report.view_candidates[1].matrix = report.view_candidates[1]
                .observations
                .last()
                .unwrap()
                .matrix;
        }
        let evidence = hybrid_analysis(&reports).unwrap();
        let CameraEvidenceSelection::Selected { source, .. } = evidence.selection else {
            panic!("not unique");
        };
        assert_eq!(source.source.offset, 0);
        assert!(
            !evidence.candidates[1]
                .fallout_hybrid
                .as_ref()
                .unwrap()
                .reference_orientation_matches
        );
    }

    #[test]
    fn fallout_hybrid_selects_world_draw_not_identical_weapon_basis_in_same_pass() {
        let mut reports = hybrid_reports();
        for report in &mut reports {
            let mut world = report.view_candidates[0].clone();
            world.source.source.draw_ordinal = 11;
            let world_contexts = report
                .draw_contexts
                .iter()
                .cloned()
                .map(|mut row| {
                    row.draw_ordinal = 11;
                    row
                })
                .collect::<Vec<_>>();
            for row in &mut report.draw_contexts {
                let viewport = row.viewport.as_mut().unwrap();
                viewport.min_depth = 0.0;
                viewport.max_depth = 0.01;
            }
            report.draw_contexts.extend(world_contexts);
            report.view_candidates.push(world);
            report.processed_draws *= 2;
            report.sampling.submitted_draws *= 2;
            report.sampling.scheduled_draws *= 2;
        }
        let evidence = hybrid_analysis(&reports).unwrap();
        let CameraEvidenceSelection::Selected { source, .. } = evidence.selection else {
            panic!(
                "world draw must be the sole candidate: {:?}",
                evidence.selection
            );
        };
        assert_eq!(
            source.source.draw_ordinal, 11,
            "retain original draw ordinal, never renumber after filtering"
        );
        assert!(!evidence.production_binding_authorized);
    }

    #[test]
    fn fallout_hybrid_rejects_missing_mixed_or_weapon_viewport_even_with_correct_pose() {
        for corruption in [
            "weapon",
            "one_weapon_frame",
            "missing",
            "wrong_present",
            "multiple",
            "cropped",
            "duplicate",
        ] {
            let mut reports = hybrid_reports();
            let report = &mut reports[1];
            match corruption {
                "weapon" => {
                    for row in &mut report.draw_contexts {
                        row.viewport.as_mut().unwrap().min_depth = 0.0;
                        row.viewport.as_mut().unwrap().max_depth = 0.01;
                    }
                }
                "one_weapon_frame" => {
                    report.draw_contexts[5].viewport.as_mut().unwrap().max_depth = 0.01
                }
                "missing" => report.draw_contexts[5].viewport = None,
                "wrong_present" => report.draw_contexts[5].present_seq += 1,
                "multiple" => report.draw_contexts[5].viewport.as_mut().unwrap().count = 2,
                "cropped" => report.draw_contexts[5].viewport.as_mut().unwrap().width = 960.0,
                "duplicate" => report.draw_contexts[5] = report.draw_contexts[4].clone(),
                _ => unreachable!(),
            }
            assert_eq!(
                hybrid_analysis(&reports).unwrap().selection,
                CameraEvidenceSelection::NoMatch,
                "{corruption}"
            );
        }
        let mut old = hybrid_reports();
        for report in &mut old {
            report.schema = CAMERA_DISCOVERY_REPORT_SCHEMA_V3.into();
            report.draw_contexts.clear();
        }
        assert!(hybrid_analysis(&old).unwrap_err().contains("report/v4"));
    }

    #[test]
    fn fallout_hybrid_rejects_missing_or_unordered_joins_and_invalid_basis() {
        for corruption in [
            "missing",
            "duplicate",
            "reflection",
            "nonfinite",
            "short",
            "frames",
        ] {
            let mut reports = hybrid_reports();
            let candidate = &mut reports[1].view_candidates[0];
            match corruption {
                "missing" => candidate.observations[0].present_seq -= 1,
                "duplicate" => {
                    candidate.observations[1].present_seq = candidate.observations[0].present_seq
                }
                "reflection" => candidate.observations[0].matrix = view(0.0, 0.0),
                "nonfinite" => candidate.observations[0].matrix[0][0] = f32::NAN,
                "short" => candidate.observations.truncate(7),
                "frames" => candidate.frames += 1,
                _ => unreachable!(),
            }
            let evidence = hybrid_analysis(&reports).unwrap();
            assert_eq!(
                evidence.selection,
                CameraEvidenceSelection::NoMatch,
                "{corruption}"
            );
            assert_eq!(evidence.rejected_sources.len(), 1, "{corruption}");
        }
    }

    #[test]
    fn fallout_hybrid_rejects_reference_contract_and_accounting_corruption() {
        for corruption in [
            "v2",
            "hash",
            "axes",
            "provider",
            "reference",
            "scale",
            "quaternion",
            "position",
            "qpc",
            "overflow",
            "errors",
        ] {
            let mut reports = hybrid_reports();
            match corruption {
                "v2" => reports[1].schema = CAMERA_DISCOVERY_REPORT_SCHEMA_V2.into(),
                "hash" => {
                    reports[1].config.executable_sha256 = format!("sha256:{}", "1".repeat(64))
                }
                "axes" | "provider" | "scale" => {
                    for report in &mut reports {
                        let reference = report.config.comparison_camera_reference.as_mut().unwrap();
                        match corruption {
                            "axes" => reference.axis_mapping = "x_y_z".into(),
                            "provider" => reference.provider = "unknown/v1".into(),
                            _ => reference.metres_per_game_unit = "NaN".into(),
                        }
                    }
                }
                "reference" => reports[1].config.comparison_camera_reference = None,
                "quaternion" => {
                    reports[1].comparison_camera_samples[0].camera_rotation_quaternion = [0.0; 4]
                }
                "position" => reports[1].comparison_camera_samples[0].camera_position[0] = f64::NAN,
                "qpc" => reports[1].comparison_camera_samples[1].present_qpc = 0,
                "overflow" => reports[1].candidate_overflows = 1,
                "errors" => reports[1]
                    .comparison_camera_errors
                    .push("read failed".into()),
                _ => unreachable!(),
            }
            assert!(hybrid_analysis(&reports).is_err(), "{corruption}");
        }
    }

    #[test]
    fn fallout_hybrid_preserves_static_and_motion_response_gates() {
        for corruption in ["static_walk", "no_walk", "no_turn"] {
            let mut reports = hybrid_reports();
            match corruption {
                "static_walk" => {
                    for (i, row) in reports[0].comparison_camera_samples.iter_mut().enumerate() {
                        row.camera_position[0] = i as f64;
                    }
                }
                "no_walk" => {
                    for row in &mut reports[1].comparison_camera_samples {
                        row.camera_position = [0.0; 3];
                    }
                }
                "no_turn" => {
                    let first = reports[2].view_candidates[0].observations[0].matrix;
                    for row in &mut reports[2].view_candidates[0].observations {
                        row.matrix = first;
                    }
                }
                _ => unreachable!(),
            }
            assert_eq!(
                hybrid_analysis(&reports).unwrap().selection,
                CameraEvidenceSelection::NoMatch,
                "{corruption}"
            );
        }
    }

    #[test]
    fn fallout_hybrid_authoring_recomputes_evidence_and_rejects_tampering() {
        let mut reports = hybrid_reports();
        let mut projection_source = reports[0].view_candidates[0].source.source;
        projection_source.role = MatrixRole::Projection;
        projection_source.offset = 64;
        for report in &mut reports {
            report.phase_id.push_str("_W000");
            report.config.phase_id = report.phase_id.clone();
            report
                .projection_candidates
                .push(DiscoveryProjectionCandidate {
                    source: projection_source,
                    frames: 12,
                    distinct_values: 1,
                    matrix: [
                        [1.2, 0.0, 0.0, 0.0],
                        [0.0, 2.1, 0.0, 0.0],
                        [0.0, 0.0, 1.0001, 1.0],
                        [0.0, 0.0, -1.0001, 0.0],
                    ],
                });
        }
        let mut builder = CameraEvidenceSweepBuilder::with_analysis_mode(
            CameraEvidenceAnalysisMode::FalloutRenderHybrid,
        );
        builder
            .push(CameraEvidenceSweepInput {
                static_report: reports[0].clone(),
                walk_report: reports[1].clone(),
                turn_report: reports[2].clone(),
            })
            .unwrap();
        let sweep = builder.finish().unwrap();
        let template = crate::CameraBinding {
            schema: crate::CAMERA_BINDING_SCHEMA_V1.into(),
            binding_id: "synthetic-hybrid".into(),
            binding_version: "authoring-template".into(),
            profile_id: "synthetic-profile".into(),
            profile_version: "1".into(),
            executable_sha256: reports[0].config.executable_sha256.clone(),
            render_width: 1920,
            render_height: 1080,
            view_metres_per_unit: None,
            game_camera: reports[0].config.comparison_camera_reference.clone(),
            view_sources: Vec::new(),
            projection_sources: vec![projection_source],
        };
        let author = |sweep: &CameraEvidenceSweepReport,
                      reports: &[CameraDiscoveryReport; 3],
                      template: &crate::CameraBinding| {
            crate::author_render_camera_binding(
                sweep,
                [&reports[0], &reports[1], &reports[2]],
                template,
            )
        };
        let candidate = author(&sweep, &reports, &template).unwrap();
        assert_eq!(
            candidate.view_sources[0],
            reports[0].view_candidates[0].source
        );
        assert!(candidate.binding_version.starts_with("authoring-"));
        for corruption in [
            "metric",
            "selection",
            "schema",
            "promoted",
            "missing_window",
            "raw_report",
            "template_reference",
            "template_projection",
            "weapon_viewport",
        ] {
            let mut bad_sweep = sweep.clone();
            let mut bad_reports = reports.clone();
            let mut bad_template = template.clone();
            match corruption {
                "metric" => {
                    bad_sweep.windows[0].evidence.candidates[0]
                        .walk_motion
                        .translation_p95 += 1.0
                }
                "selection" => bad_sweep.selection = CameraEvidenceSelection::NoMatch,
                "schema" => bad_sweep.schema = CAMERA_EVIDENCE_SWEEP_REPORT_SCHEMA_V1.into(),
                "promoted" => bad_sweep.production_binding_authorized = true,
                "missing_window" => bad_sweep.windows.clear(),
                "raw_report" => {
                    bad_reports[1].comparison_camera_samples[0].camera_position[0] += 10.0
                }
                "template_reference" => {
                    bad_template
                        .game_camera
                        .as_mut()
                        .unwrap()
                        .camera_singleton_relocation += 8
                }
                "template_projection" => bad_template.projection_sources[0].offset = 128,
                "weapon_viewport" => {
                    bad_reports[1].draw_contexts[0]
                        .viewport
                        .as_mut()
                        .unwrap()
                        .max_depth = 0.01
                }
                _ => unreachable!(),
            }
            assert!(
                author(&bad_sweep, &bad_reports, &bad_template).is_err(),
                "{corruption}"
            );
        }
    }

    #[test]
    fn explicit_trial_preserves_ambiguity_and_revalidates_the_exact_requested_draw() {
        let mut reports = hybrid_reports();
        for report in &mut reports {
            report.phase_id.push_str("_W000");
            report.config.phase_id = report.phase_id.clone();
            report.view_candidates[0].source.source.pass_ordinal = 3;
            for row in &mut report.draw_contexts {
                row.pass_ordinal = 3;
            }
            let mut duplicate = report.view_candidates[0].clone();
            duplicate.source.source.pass_ordinal = 2;
            report.view_candidates.push(duplicate);
            let duplicate_contexts = report
                .draw_contexts
                .iter()
                .cloned()
                .map(|mut row| {
                    row.pass_ordinal = 2;
                    row
                })
                .collect::<Vec<_>>();
            report.draw_contexts.extend(duplicate_contexts);
            let equivalent_contexts = report
                .draw_contexts
                .iter()
                .filter(|row| row.pass_ordinal == 3)
                .cloned()
                .map(|mut row| {
                    row.draw_ordinal = 5;
                    row
                })
                .collect::<Vec<_>>();
            report.draw_contexts.extend(equivalent_contexts);
            report.processed_draws *= 3;
            report.sampling.submitted_draws *= 3;
            report.sampling.scheduled_draws *= 3;
            for view in &report.view_candidates {
                let mut source = view.source.source;
                source.role = MatrixRole::Projection;
                source.offset = 64;
                source.transpose = true;
                report
                    .projection_candidates
                    .push(DiscoveryProjectionCandidate {
                        source,
                        frames: 12,
                        distinct_values: 1,
                        matrix: [
                            [1.2, 0.0, 0.0, 0.0],
                            [0.0, 2.1, 0.0, 0.0],
                            [0.0, 0.0, 1.0001, 1.0],
                            [0.0, 0.0, -15.0015, 0.0],
                        ],
                    });
            }
        }
        let mut builder = CameraEvidenceSweepBuilder::with_analysis_mode(
            CameraEvidenceAnalysisMode::FalloutRenderHybrid,
        );
        builder
            .push(CameraEvidenceSweepInput {
                static_report: reports[0].clone(),
                walk_report: reports[1].clone(),
                turn_report: reports[2].clone(),
            })
            .unwrap();
        let sweep = builder.finish().unwrap();
        let before = sweep.clone();
        assert!(matches!(
            sweep.selection,
            CameraEvidenceSelection::Ambiguous { .. }
        ));
        let source = reports[0].view_candidates[0].source;
        let template = crate::CameraBinding {
            schema: crate::CAMERA_BINDING_SCHEMA_V1.into(),
            binding_id: "synthetic-trial".into(),
            binding_version: "template".into(),
            profile_id: "synthetic-profile".into(),
            profile_version: "1".into(),
            executable_sha256: reports[0].config.executable_sha256.clone(),
            render_width: 1920,
            render_height: 1080,
            view_metres_per_unit: None,
            game_camera: reports[0].config.comparison_camera_reference.clone(),
            view_sources: Vec::new(),
            projection_sources: vec![reports[0].projection_candidates[0].source],
        };
        let phases = [&reports[0], &reports[1], &reports[2]];
        assert!(crate::author_render_camera_binding(&sweep, phases, &template).is_err());
        let candidate =
            crate::author_render_camera_trial(&sweep, phases, &template, source).unwrap();
        assert_eq!(candidate.view_sources, vec![source]);
        assert_eq!(candidate.binding_version, "authoring-trial-1");
        assert_eq!(sweep, before, "trial must not manufacture a unique sweep");
        let mut absent = source;
        absent.source.pass_ordinal = 99;
        assert!(crate::author_render_camera_trial(&sweep, phases, &template, absent).is_err());
        let mut tampered = sweep.clone();
        tampered.windows[0].evidence.candidates[0]
            .walk_motion
            .translation_p95 += 1.0;
        assert!(crate::author_render_camera_trial(&tampered, phases, &template, source).is_err());
        tampered = sweep.clone();
        tampered.selection = CameraEvidenceSelection::NoMatch;
        assert!(crate::author_render_camera_trial(&tampered, phases, &template, source).is_err());
    }

    fn v3_descriptor() -> DsvDiscoveryDescriptor {
        DsvDiscoveryDescriptor {
            view_format: 45,
            view_dimension: 3,
            view_flags: 0,
            mip_slice: 0,
            first_array_slice: 0,
            view_array_size: 1,
            resource_format: 44,
            width: 1920,
            height: 1080,
            resource_mip_levels: 1,
            resource_array_size: 1,
            sample_count: 1,
            sample_quality: 0,
            usage: 0,
            bind_flags: 0x48,
            cpu_access_flags: 0,
            misc_flags: 0,
        }
    }

    fn into_v3(mut report: CameraDiscoveryReport) -> CameraDiscoveryReport {
        let descriptor = v3_descriptor();
        let selection = CameraDiscoveryDsvSelection {
            evidence_root: "C:/evidence".into(),
            attestation_locator: "selection.json".into(),
            attestation_sha256: format!("sha256:{}", "a".repeat(64)),
        };
        report.schema = CAMERA_DISCOVERY_REPORT_SCHEMA_V3.into();
        report.render = render_identity_from_dsv_descriptor(descriptor).unwrap();
        report.config.schema = CAMERA_DISCOVERY_SCHEMA_V3.into();
        report.config.render = None;
        report.config.dsv_selection = Some(selection.clone());
        report.dsv_selection = Some(selection);
        report.selected_dsv_descriptor = Some(descriptor);
        report
    }

    fn into_reference_free_v3(mut report: CameraDiscoveryReport) -> CameraDiscoveryReport {
        report = into_v3(report);
        let selection = report.config.dsv_selection.as_mut().unwrap();
        selection.evidence_root = std::env::temp_dir()
            .join("dm-reference-free-camera-evidence")
            .to_string_lossy()
            .into_owned();
        report.dsv_selection = report.config.dsv_selection.clone();
        report.config.comparison_camera_reference = None;
        report.comparison_camera_samples.clear();
        report.comparison_camera_errors.clear();
        report
    }

    fn d3d12_source(matrix_offset: u32, role: MatrixRole) -> D3d12MatrixSourceBinding {
        D3d12MatrixSourceBinding {
            descriptor_table: D3d12DescriptorTableBinding {
                root_signature_sha256: format!("sha256:{}", "1".repeat(64)),
                root_parameter_index: 18,
                descriptor_range_index: 0,
                descriptor_offset_in_range: 0,
                descriptor_count: 1,
                base_shader_register: 7,
                register_space: 0,
            },
            cbv_resource_offset: 0,
            declared_cbv_bytes: 256,
            matrix_offset,
            encoding: MatrixEncoding::F32LittleEndian,
            layout: MatrixLayout::RowMajor,
            transpose: true,
            role,
            draw_relation: D3d12DrawRelation::ExactSelectedDepthDsvDraw,
        }
    }

    fn fake_sha(bytes: &[u8]) -> String {
        let digest = bytes
            .iter()
            .fold(0xcbf2_9ce4_8422_2325_u64, |digest, byte| {
                digest.wrapping_mul(0x100_0000_01b3) ^ u64::from(*byte)
            });
        format!("sha256:{digest:064x}")
    }

    fn typed_egeo_report(depth_profile_sha256: &str, passed: bool) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "schema": CP2077_EGEO_REPORT_SCHEMA_V1,
            "non_trainable": true,
            "local_only": true,
            "upload_allowed": false,
            "e_geo": {
                "schema": CAMERA_EGEO_VALIDATION_SCHEMA_V3,
                "passed": passed,
                "ready_for_independent_repeat": passed,
                "production_binding_authorized": false,
                "failures": if passed {
                    Vec::<String>::new()
                } else {
                    vec!["synthetic failure".to_owned()]
                },
                "raw_depth_profile_sha256": depth_profile_sha256
            }
        }))
        .unwrap()
    }

    fn identity_axis_mapping() -> D3d12AxisMapping {
        D3d12AxisMapping {
            delivery_x_from: D3d12SignedAxis::PositiveX,
            delivery_y_from: D3d12SignedAxis::PositiveY,
            delivery_z_from: D3d12SignedAxis::PositiveZ,
        }
    }

    fn d3d12_launch(
        launch_role: D3d12AuthoringLaunchRole,
        process_launch_id: &str,
        egeo_passed: bool,
    ) -> (D3d12CameraAuthoringLaunchEvidence, Vec<u8>) {
        let executable_sha256 = format!("sha256:{}", "2".repeat(64));
        let depth_profile_sha256 = format!("sha256:{}", "3".repeat(64));
        let view_source = D3d12ViewSourceBinding {
            source: d3d12_source(0, MatrixRole::View),
            translation: TranslationSlot::Row3,
        };
        let projection_source = d3d12_source(128, MatrixRole::Projection);
        let egeo_report = typed_egeo_report(&depth_profile_sha256, egeo_passed);
        let egeo_attestation = seal_d3d12_egeo_attestation(
            D3d12EgeoAttestationClaims {
                process_launch_id: process_launch_id.into(),
                executable_sha256: executable_sha256.clone(),
                depth_profile_sha256: depth_profile_sha256.clone(),
                view_metres_per_unit: "1.0".into(),
                axis_mapping: identity_axis_mapping(),
                view_source: view_source.clone(),
                projection_source: projection_source.clone(),
            },
            &egeo_report,
            fake_sha,
        )
        .unwrap();
        let launch = D3d12CameraAuthoringLaunchEvidence {
            schema: D3D12_CAMERA_AUTHORING_LAUNCH_SCHEMA_V1.into(),
            launch_role,
            process_launch_id: process_launch_id.into(),
            executable_sha256,
            depth_profile_sha256,
            non_trainable: true,
            upload_allowed: false,
            production_binding_authorized: false,
            authoring_blockers: Vec::new(),
            render_width: 1920,
            render_height: 1080,
            view_metres_per_unit: "1.0".into(),
            axis_mapping: identity_axis_mapping(),
            view_source,
            projection_source,
            fov_75_90_75: [
                D3d12FovPhaseObservation {
                    configured_horizontal_fov_degrees: 75.0,
                    observed_horizontal_fov_degrees: 75.1,
                    fx_pixels: 1_000.0,
                    fy_pixels: 1_000.0,
                },
                D3d12FovPhaseObservation {
                    configured_horizontal_fov_degrees: 90.0,
                    observed_horizontal_fov_degrees: 90.2,
                    fx_pixels: 800.0,
                    fy_pixels: 800.0,
                },
                D3d12FovPhaseObservation {
                    configured_horizontal_fov_degrees: 75.0,
                    observed_horizontal_fov_degrees: 75.0,
                    fx_pixels: 1_002.0,
                    fy_pixels: 998.0,
                },
            ],
            axis_motion: [
                D3d12AxisMotionObservation {
                    motion: D3d12ControlledMotion::Forward,
                    source_displacement_world_units: [0.0, 0.0, 2.0],
                    delivery_displacement_metres: [0.0, 0.0, 2.0],
                },
                D3d12AxisMotionObservation {
                    motion: D3d12ControlledMotion::StrafeRight,
                    source_displacement_world_units: [2.0, 0.0, 0.0],
                    delivery_displacement_metres: [2.0, 0.0, 0.0],
                },
                D3d12AxisMotionObservation {
                    motion: D3d12ControlledMotion::VerticalUp,
                    source_displacement_world_units: [0.0, 1.0, 0.0],
                    delivery_displacement_metres: [0.0, 1.0, 0.0],
                },
            ],
            egeo_attestation,
            route_start_position_metres: [0.0, 0.0, 0.0],
            route_end_position_metres: [0.1, 0.0, 0.0],
            route_start_rotation_quaternion: [0.0, 0.0, 0.0, 1.0],
            route_end_rotation_quaternion: [0.0, 0.0, 0.0, 1.0],
        };
        (launch, egeo_report)
    }

    #[test]
    fn independent_d3d12_launches_reach_review_but_never_authorize_production() {
        let (launch_a, report_a) = d3d12_launch(
            D3d12AuthoringLaunchRole::A,
            "0123456789abcdef0123456789abcdef",
            true,
        );
        let (launch_b, report_b) = d3d12_launch(
            D3d12AuthoringLaunchRole::B,
            "fedcba9876543210fedcba9876543210",
            true,
        );
        let verdict = validate_d3d12_camera_authoring_pair(
            &launch_a, &report_a, &launch_b, &report_b, fake_sha,
        )
        .unwrap();
        assert!(verdict.ready_for_human_review);
        assert!(verdict.requires_signed_catalog_promotion);
        assert!(verdict.non_trainable);
        assert!(!verdict.upload_allowed);
        assert!(!verdict.production_binding_authorized);
        assert_eq!((verdict.render_width, verdict.render_height), (1920, 1080));
        assert_eq!(verdict.view_metres_per_unit, "1.0");
    }

    #[test]
    fn d3d12_authoring_rejects_same_launch_fov_drift_axis_and_route_failures() {
        let (launch_a, report_a) = d3d12_launch(
            D3d12AuthoringLaunchRole::A,
            "0123456789abcdef0123456789abcdef",
            true,
        );
        let (same_launch_b, same_report_b) = d3d12_launch(
            D3d12AuthoringLaunchRole::B,
            "0123456789abcdef0123456789abcdef",
            true,
        );
        assert!(
            validate_d3d12_camera_authoring_pair(
                &launch_a,
                &report_a,
                &same_launch_b,
                &same_report_b,
                fake_sha,
            )
            .unwrap_err()
            .contains("independent process launches")
        );

        let (mut launch_b, report_b) = d3d12_launch(
            D3d12AuthoringLaunchRole::B,
            "fedcba9876543210fedcba9876543210",
            true,
        );
        launch_b.fov_75_90_75[2].fx_pixels = 900.0;
        assert!(
            validate_d3d12_camera_authoring_pair(
                &launch_a, &report_a, &launch_b, &report_b, fake_sha,
            )
            .unwrap_err()
            .contains("return to the FOV-75 baseline")
        );

        launch_b.fov_75_90_75[2].fx_pixels = 1_002.0;
        launch_b.axis_motion[0].delivery_displacement_metres = [0.0, 0.0, 1.0];
        assert!(
            validate_d3d12_camera_authoring_pair(
                &launch_a, &report_a, &launch_b, &report_b, fake_sha,
            )
            .unwrap_err()
            .contains("contradicts its signed mapping or metric scale")
        );

        launch_b.axis_motion[0].source_displacement_world_units = [0.0, 0.0, -1.0];
        launch_b.axis_motion[0].delivery_displacement_metres = [0.0, 0.0, -1.0];
        assert!(
            validate_d3d12_camera_authoring_pair(
                &launch_a, &report_a, &launch_b, &report_b, fake_sha,
            )
            .unwrap_err()
            .contains("wrong delivery direction or sign")
        );

        launch_b.axis_motion[0].source_displacement_world_units = [0.0, 0.0, 2.0];
        launch_b.axis_motion[0].delivery_displacement_metres = [0.0, 0.0, 2.0];
        launch_b.route_end_position_metres = [f64::NAN, 0.0, 0.0];
        assert!(
            validate_d3d12_camera_authoring_pair(
                &launch_a, &report_a, &launch_b, &report_b, fake_sha,
            )
            .unwrap_err()
            .contains("position is non-finite")
        );

        launch_b.route_end_position_metres = [1.0, 0.0, 0.0];
        assert!(
            validate_d3d12_camera_authoring_pair(
                &launch_a, &report_a, &launch_b, &report_b, fake_sha,
            )
            .unwrap_err()
            .contains("route did not return")
        );
    }

    #[test]
    fn d3d12_egeo_attestation_rejects_fake_bool_wrong_hash_profile_and_source() {
        let (launch_a, report_a) = d3d12_launch(
            D3d12AuthoringLaunchRole::A,
            "0123456789abcdef0123456789abcdef",
            true,
        );
        let (launch_b, report_b) = d3d12_launch(
            D3d12AuthoringLaunchRole::B,
            "fedcba9876543210fedcba9876543210",
            true,
        );

        let (failed_b, failed_report_b) = d3d12_launch(
            D3d12AuthoringLaunchRole::B,
            "fedcba9876543210fedcba9876543210",
            false,
        );
        assert!(
            validate_d3d12_camera_authoring_pair(
                &launch_a,
                &report_a,
                &failed_b,
                &failed_report_b,
                fake_sha,
            )
            .unwrap_err()
            .contains("did not pass")
        );

        let mut tampered_report: serde_json::Value = serde_json::from_slice(&report_b).unwrap();
        tampered_report["e_geo"]["passed"] = serde_json::json!(false);
        let tampered_report = serde_json::to_vec(&tampered_report).unwrap();
        assert!(
            validate_d3d12_camera_authoring_pair(
                &launch_a,
                &report_a,
                &launch_b,
                &tampered_report,
                fake_sha,
            )
            .unwrap_err()
            .contains("report content hash mismatch")
        );

        let mut wrong_hash = launch_b.clone();
        wrong_hash.egeo_attestation.canonical_payload_sha256 = format!("sha256:{}", "4".repeat(64));
        assert!(
            validate_d3d12_camera_authoring_pair(
                &launch_a,
                &report_a,
                &wrong_hash,
                &report_b,
                fake_sha,
            )
            .unwrap_err()
            .contains("canonical payload hash mismatch")
        );

        let mut wrong_source = launch_b.clone();
        wrong_source
            .egeo_attestation
            .payload
            .claims
            .view_source
            .source
            .matrix_offset = 16;
        wrong_source.egeo_attestation.canonical_payload_sha256 =
            fake_sha(&canonical_egeo_payload(&wrong_source.egeo_attestation.payload).unwrap());
        assert!(
            validate_d3d12_camera_authoring_pair(
                &launch_a,
                &report_a,
                &wrong_source,
                &report_b,
                fake_sha,
            )
            .unwrap_err()
            .contains("stable source")
        );

        let mut wrong_axis = launch_b.clone();
        wrong_axis.egeo_attestation.payload.claims.axis_mapping = D3d12AxisMapping {
            delivery_x_from: D3d12SignedAxis::PositiveZ,
            delivery_y_from: D3d12SignedAxis::PositiveY,
            delivery_z_from: D3d12SignedAxis::PositiveX,
        };
        wrong_axis.egeo_attestation.canonical_payload_sha256 =
            fake_sha(&canonical_egeo_payload(&wrong_axis.egeo_attestation.payload).unwrap());
        assert!(
            validate_d3d12_camera_authoring_pair(
                &launch_a,
                &report_a,
                &wrong_axis,
                &report_b,
                fake_sha,
            )
            .unwrap_err()
            .contains("metric/axis/stable source")
        );

        let mut wrong_profile_report: serde_json::Value =
            serde_json::from_slice(&report_b).unwrap();
        wrong_profile_report["e_geo"]["raw_depth_profile_sha256"] =
            serde_json::json!(format!("sha256:{}", "5".repeat(64)));
        let wrong_profile_report = serde_json::to_vec(&wrong_profile_report).unwrap();
        let mut wrong_profile = launch_b.clone();
        wrong_profile.egeo_attestation = seal_d3d12_egeo_attestation(
            wrong_profile.egeo_attestation.payload.claims.clone(),
            &wrong_profile_report,
            fake_sha,
        )
        .unwrap();
        assert!(
            validate_d3d12_camera_authoring_pair(
                &launch_a,
                &report_a,
                &wrong_profile,
                &wrong_profile_report,
                fake_sha,
            )
            .unwrap_err()
            .contains("another depth profile")
        );
    }

    #[test]
    fn d3d12_axis_mapping_rejects_non_permutation_and_ab_direction_drift() {
        let (launch_a, report_a) = d3d12_launch(
            D3d12AuthoringLaunchRole::A,
            "0123456789abcdef0123456789abcdef",
            true,
        );
        let (mut launch_b, report_b) = d3d12_launch(
            D3d12AuthoringLaunchRole::B,
            "fedcba9876543210fedcba9876543210",
            true,
        );
        launch_b.axis_mapping.delivery_z_from = D3d12SignedAxis::PositiveY;
        assert!(
            validate_d3d12_camera_authoring_pair(
                &launch_a, &report_a, &launch_b, &report_b, fake_sha,
            )
            .unwrap_err()
            .contains("stable source is invalid")
        );

        launch_b.axis_mapping = identity_axis_mapping();
        launch_b.axis_motion[0].source_displacement_world_units = [0.341, 0.0, 0.940];
        launch_b.axis_motion[0].delivery_displacement_metres = [0.341, 0.0, 0.940];
        assert!(
            validate_d3d12_camera_authoring_pair(
                &launch_a, &report_a, &launch_b, &report_b, fake_sha,
            )
            .unwrap_err()
            .contains("directions do not correspond")
        );

        assert_eq!(
            independent_axis_rank(&[[1.0, 0.0, 0.0], [2.0, 0.0, 0.0], [3.0, 0.0, 0.0]]),
            1
        );
    }

    #[test]
    fn controlled_phases_select_one_source_but_do_not_authorize_production() {
        let static_report = report(POSE_STATIC_PHASE, |_| (0.0, 0.0));
        let walk_report = report(POSE_WALK_PHASE, |index| (index as f32, 0.0));
        let turn_report = report(POSE_TURN_PHASE, |index| (0.0, index as f32 * 0.05));
        let evidence = analyze_camera_evidence(&static_report, &walk_report, &turn_report).unwrap();
        assert!(matches!(
            evidence.selection,
            CameraEvidenceSelection::Selected { .. }
        ));
        assert!(evidence.requires_e_geo_validation);
        assert!(!evidence.production_binding_authorized);
    }

    #[test]
    fn v3_controlled_phases_reject_source_or_exact_descriptor_drift() {
        let static_report = into_v3(report(POSE_STATIC_PHASE, |_| (0.0, 0.0)));
        let mut walk_report = into_v3(report(POSE_WALK_PHASE, |index| (index as f32, 0.0)));
        let turn_report = into_v3(report(POSE_TURN_PHASE, |index| (0.0, index as f32 * 0.05)));

        walk_report
            .config
            .dsv_selection
            .as_mut()
            .unwrap()
            .attestation_sha256 = format!("sha256:{}", "b".repeat(64));
        walk_report.dsv_selection = walk_report.config.dsv_selection.clone();
        assert!(walk_report.validate_dsv_selection_binding().is_ok());
        assert!(
            analyze_camera_evidence(&static_report, &walk_report, &turn_report)
                .unwrap_err()
                .contains("does not share")
        );

        let mut walk_report = into_v3(report(POSE_WALK_PHASE, |index| (index as f32, 0.0)));
        walk_report
            .selected_dsv_descriptor
            .as_mut()
            .unwrap()
            .misc_flags = 1;
        assert!(walk_report.validate_dsv_selection_binding().is_ok());
        assert!(
            analyze_camera_evidence(&static_report, &walk_report, &turn_report)
                .unwrap_err()
                .contains("does not share")
        );
    }

    #[test]
    fn reference_free_v3_emits_only_raw_candidates_under_a_human_review_ceiling() {
        let static_report = into_reference_free_v3(report(POSE_STATIC_PHASE, |_| (0.0, 0.0)));
        let walk_report =
            into_reference_free_v3(report(POSE_WALK_PHASE, |index| (index as f32, 0.0)));
        let turn_report =
            into_reference_free_v3(report(POSE_TURN_PHASE, |index| (0.0, index as f32 * 0.05)));
        let review = analyze_reference_free_camera_candidate_review(
            &static_report,
            &walk_report,
            &turn_report,
        )
        .unwrap();
        review.validate().unwrap();
        assert_eq!(
            review.schema,
            CAMERA_REFERENCE_FREE_CANDIDATE_REVIEW_SCHEMA_V1
        );
        assert_eq!(review.status, "human_review_required");
        assert_eq!(review.candidates.len(), 1);
        assert!(review.human_review_required);
        assert!(!review.metric_scale_available);
        assert!(!review.automatic_selection_authorized);
        assert!(!review.camera_binding_generated);
        assert!(!review.recording_ready);
        assert!(!review.deliverable);
        assert!(!review.publication_allowed);
        assert!(review.requires_metric_scale_validation);
        assert!(review.requires_e_geo_validation);
        assert!(!review.production_binding_authorized);

        let json = serde_json::to_value(&review).unwrap();
        assert!(json.get("selection").is_none());
        let candidate = &json["candidates"][0];
        for forbidden in [
            "static_coupling_to_legacy_reference",
            "walk_coupling_to_legacy_reference",
            "turn_coupling_to_legacy_reference",
            "static_translation_p95_metres",
            "static_absolute_stable",
            "static_stable",
            "walk_translation_response",
            "turn_rotation_response",
            "controlled_phase_pattern_matches",
        ] {
            assert!(candidate.get(forbidden).is_none(), "{forbidden}");
        }
        assert!(serde_json::from_value::<CameraEvidenceReport>(json).is_err());
    }

    #[test]
    fn reference_free_review_rejects_ceiling_overclaim_and_unbound_comparison_data() {
        let static_report = into_reference_free_v3(report(POSE_STATIC_PHASE, |_| (0.0, 0.0)));
        let walk_report =
            into_reference_free_v3(report(POSE_WALK_PHASE, |index| (index as f32, 0.0)));
        let turn_report =
            into_reference_free_v3(report(POSE_TURN_PHASE, |index| (0.0, index as f32 * 0.05)));
        let review = analyze_reference_free_camera_candidate_review(
            &static_report,
            &walk_report,
            &turn_report,
        )
        .unwrap();
        for mutation in ["automatic", "metric", "production", "status"] {
            let mut forged = review.clone();
            match mutation {
                "automatic" => forged.automatic_selection_authorized = true,
                "metric" => forged.metric_scale_available = true,
                "production" => forged.production_binding_authorized = true,
                "status" => forged.status = "selected".into(),
                _ => unreachable!(),
            }
            assert!(forged.validate().is_err(), "{mutation}");
        }

        let mut unexpected_samples = static_report.clone();
        unexpected_samples
            .comparison_camera_errors
            .push("unbound comparison channel".into());
        assert!(
            analyze_reference_free_camera_candidate_review(
                &unexpected_samples,
                &walk_report,
                &turn_report,
            )
            .unwrap_err()
            .contains("comparison samples or errors")
        );
    }

    #[test]
    fn reference_free_review_rejects_mixed_legacy_and_selection_identity_drift() {
        let static_report = into_reference_free_v3(report(POSE_STATIC_PHASE, |_| (0.0, 0.0)));
        let mut walk_report =
            into_reference_free_v3(report(POSE_WALK_PHASE, |index| (index as f32, 0.0)));
        let turn_report =
            into_reference_free_v3(report(POSE_TURN_PHASE, |index| (0.0, index as f32 * 0.05)));

        walk_report.config.comparison_camera_reference = Some(GameCameraBinding {
            provider: "unexpected-reference/v1".into(),
            camera_singleton_relocation: 1,
            camera_root_offset: 2,
            world_transform_offset: 3,
            metres_per_game_unit: "1".into(),
            axis_mapping: "x_z_y".into(),
        });
        assert!(
            analyze_reference_free_camera_candidate_review(
                &static_report,
                &walk_report,
                &turn_report,
            )
            .is_err()
        );

        let mut walk_report =
            into_reference_free_v3(report(POSE_WALK_PHASE, |index| (index as f32, 0.0)));
        walk_report
            .config
            .dsv_selection
            .as_mut()
            .unwrap()
            .evidence_root = std::env::temp_dir()
            .join("drifted-reference-free-root")
            .to_string_lossy()
            .into_owned();
        walk_report.dsv_selection = walk_report.config.dsv_selection.clone();
        assert!(
            analyze_reference_free_camera_candidate_review(
                &static_report,
                &walk_report,
                &turn_report,
            )
            .unwrap_err()
            .contains("does not share")
        );

        let mut legacy_static = report(POSE_STATIC_PHASE, |_| (0.0, 0.0));
        legacy_static.config.comparison_camera_reference = None;
        legacy_static.comparison_camera_samples.clear();
        assert!(
            analyze_reference_free_camera_candidate_review(
                &legacy_static,
                &walk_report,
                &turn_report,
            )
            .is_err()
        );
    }

    #[test]
    fn reference_free_zero_or_multiple_candidates_never_become_a_selection() {
        let make = || {
            [
                into_reference_free_v3(report(POSE_STATIC_PHASE, |_| (0.0, 0.0))),
                into_reference_free_v3(report(POSE_WALK_PHASE, |index| (index as f32, 0.0))),
                into_reference_free_v3(report(POSE_TURN_PHASE, |index| (0.0, index as f32 * 0.05))),
            ]
        };
        let mut empty = make();
        for report in &mut empty {
            report.view_candidates.clear();
        }
        let review =
            analyze_reference_free_camera_candidate_review(&empty[0], &empty[1], &empty[2])
                .unwrap();
        assert!(review.candidates.is_empty());
        assert!(
            serde_json::to_value(&review)
                .unwrap()
                .get("selection")
                .is_none()
        );

        let mut multiple = make();
        for report in &mut multiple {
            let mut second = report.view_candidates[0].clone();
            second.source.source.draw_ordinal = 9;
            report.view_candidates.push(second);
        }
        let review = analyze_reference_free_camera_candidate_review(
            &multiple[0],
            &multiple[1],
            &multiple[2],
        )
        .unwrap();
        assert_eq!(review.candidates.len(), 2);
        assert!(
            serde_json::to_value(&review)
                .unwrap()
                .get("selection")
                .is_none()
        );
    }

    #[test]
    fn v3_sweep_rejects_exact_descriptor_drift_between_draw_windows() {
        let make_window = |start: u32| {
            let suffix = format!("W{start:03}");
            let mut static_report =
                into_v3(report(&format!("{POSE_STATIC_PHASE}_{suffix}"), |_| {
                    (0.0, 0.0)
                }));
            let mut walk_report =
                into_v3(report(&format!("{POSE_WALK_PHASE}_{suffix}"), |index| {
                    (index as f32, 0.0)
                }));
            let mut turn_report =
                into_v3(report(&format!("{POSE_TURN_PHASE}_{suffix}"), |index| {
                    (0.0, index as f32 * 0.05)
                }));
            for report in [&mut static_report, &mut walk_report, &mut turn_report] {
                report.config.draw_window_start = start;
                report.config.draw_window_count = 8;
            }
            CameraEvidenceSweepInput {
                static_report,
                walk_report,
                turn_report,
            }
        };

        let first = make_window(0);
        let mut second = make_window(8);
        for report in [
            &mut second.static_report,
            &mut second.walk_report,
            &mut second.turn_report,
        ] {
            report.selected_dsv_descriptor.as_mut().unwrap().misc_flags = 1;
        }
        assert!(
            analyze_camera_evidence_sweep([first, second])
                .unwrap_err()
                .contains("does not share")
        );
    }

    #[test]
    fn reference_free_sweep_is_candidates_only_and_rejects_cross_window_identity_drift() {
        let make_window = |start: u32| {
            let suffix = format!("W{start:03}");
            let mut static_report =
                into_reference_free_v3(report(&format!("{POSE_STATIC_PHASE}_{suffix}"), |_| {
                    (0.0, 0.0)
                }));
            let mut walk_report =
                into_reference_free_v3(report(&format!("{POSE_WALK_PHASE}_{suffix}"), |index| {
                    (index as f32, 0.0)
                }));
            let mut turn_report =
                into_reference_free_v3(report(&format!("{POSE_TURN_PHASE}_{suffix}"), |index| {
                    (0.0, index as f32 * 0.05)
                }));
            for report in [&mut static_report, &mut walk_report, &mut turn_report] {
                report.config.draw_window_start = start;
                report.config.draw_window_count = 8;
                report.view_candidates[0].source.source.draw_ordinal = start + 7;
            }
            CameraEvidenceSweepInput {
                static_report,
                walk_report,
                turn_report,
            }
        };

        let sweep =
            analyze_reference_free_camera_candidate_review_sweep([make_window(0), make_window(8)])
                .unwrap();
        sweep.validate().unwrap();
        assert_eq!(
            sweep.schema,
            CAMERA_REFERENCE_FREE_CANDIDATE_REVIEW_SWEEP_SCHEMA_V1
        );
        assert_eq!(sweep.windows.len(), 2);
        assert_eq!(sweep.common_sources.len(), 1);
        assert_eq!(sweep.common_sources[0].source.draw_ordinal, 0);
        let json = serde_json::to_value(&sweep).unwrap();
        assert!(json.get("selection").is_none());
        assert!(
            serde_json::from_value::<CameraEvidenceSweepReport>(json).is_err(),
            "reference-free sweep must not enter legacy camera authoring"
        );

        assert!(
            analyze_reference_free_camera_candidate_review_sweep(
                [make_window(0), make_window(16)],
            )
            .unwrap_err()
            .contains("not contiguous")
        );

        let first = make_window(0);
        let mut source_drift = make_window(8);
        for report in [
            &mut source_drift.static_report,
            &mut source_drift.walk_report,
            &mut source_drift.turn_report,
        ] {
            report
                .config
                .dsv_selection
                .as_mut()
                .unwrap()
                .attestation_sha256 = format!("sha256:{}", "b".repeat(64));
            report.dsv_selection = report.config.dsv_selection.clone();
        }
        assert!(
            analyze_reference_free_camera_candidate_review_sweep([first, source_drift])
                .unwrap_err()
                .contains("does not share")
        );

        let first = make_window(0);
        let mut descriptor_drift = make_window(8);
        for report in [
            &mut descriptor_drift.static_report,
            &mut descriptor_drift.walk_report,
            &mut descriptor_drift.turn_report,
        ] {
            report.selected_dsv_descriptor.as_mut().unwrap().misc_flags = 1;
        }
        assert!(
            analyze_reference_free_camera_candidate_review_sweep([first, descriptor_drift])
                .unwrap_err()
                .contains("does not share")
        );
    }

    #[test]
    fn referenced_v2_report_shape_and_selection_behavior_are_unchanged() {
        let static_report = report(POSE_STATIC_PHASE, |_| (0.0, 0.0));
        let walk_report = report(POSE_WALK_PHASE, |index| (index as f32, 0.0));
        let turn_report = report(POSE_TURN_PHASE, |index| (0.0, index as f32 * 0.05));
        let evidence = analyze_camera_evidence(&static_report, &walk_report, &turn_report).unwrap();
        assert_eq!(evidence.schema, CAMERA_EVIDENCE_REPORT_SCHEMA_V2);
        assert!(matches!(
            evidence.selection,
            CameraEvidenceSelection::Selected { .. }
        ));
        let json = serde_json::to_value(&evidence).unwrap();
        assert!(json.get("status").is_none());
        assert!(json.get("human_review_required").is_none());
        assert!(json.get("metric_scale_available").is_none());
        assert!(
            json["candidates"][0]
                .get("static_translation_p95_metres")
                .is_some()
        );
        assert!(
            json["candidates"][0]
                .get("static_coupling_to_legacy_reference")
                .is_some()
        );
    }

    #[test]
    fn identical_evidence_across_draws_is_one_source_with_visible_provenance() {
        let mut static_report = report(POSE_STATIC_PHASE, |_| (0.0, 0.0));
        let mut walk_report = report(POSE_WALK_PHASE, |index| (index as f32, 0.0));
        let mut turn_report = report(POSE_TURN_PHASE, |index| (0.0, index as f32 * 0.05));
        for report in [&mut static_report, &mut walk_report, &mut turn_report] {
            let mut duplicate = report.view_candidates[0].clone();
            duplicate.source.source.draw_ordinal = 9;
            report.view_candidates.push(duplicate);
        }
        let evidence = analyze_camera_evidence(&static_report, &walk_report, &turn_report).unwrap();
        assert!(matches!(
            evidence.selection,
            CameraEvidenceSelection::Selected {
                equivalent_draw_ordinals,
                ..
            } if equivalent_draw_ordinals == vec![7, 9]
        ));
        assert_eq!(evidence.schema, CAMERA_EVIDENCE_REPORT_SCHEMA_V2);
        assert_eq!(evidence.candidates.len(), 2);
    }

    #[test]
    fn draw_aliases_with_different_evidence_remain_ambiguous() {
        let mut static_report = report(POSE_STATIC_PHASE, |_| (0.0, 0.0));
        let mut walk_report = report(POSE_WALK_PHASE, |index| (index as f32, 0.0));
        let mut turn_report = report(POSE_TURN_PHASE, |index| (0.0, index as f32 * 0.05));
        for report in [&mut static_report, &mut walk_report, &mut turn_report] {
            let mut duplicate = report.view_candidates[0].clone();
            duplicate.source.source.draw_ordinal = 9;
            report.view_candidates.push(duplicate);
        }
        let translation = walk_report.view_candidates[1].source.translation;
        for observation in &mut walk_report.view_candidates[1].observations {
            let pose = pose_from_view(&observation.matrix, translation);
            observation.matrix = view(pose.position[0] * 2.0, 0.0);
        }
        let evidence = analyze_camera_evidence(&static_report, &walk_report, &turn_report).unwrap();
        assert!(matches!(
            evidence.selection,
            CameraEvidenceSelection::Ambiguous { matching_sources }
                if matching_sources.len() == 2
        ));
    }

    #[test]
    fn sweep_combines_non_overlapping_windows_without_authorizing_production() {
        let make_window = |start: u32| {
            let suffix = format!("W{start:03}");
            let mut static_report =
                report(&format!("{POSE_STATIC_PHASE}_{suffix}"), |_| (0.0, 0.0));
            let mut walk_report = report(&format!("{POSE_WALK_PHASE}_{suffix}"), |index| {
                (index as f32, 0.0)
            });
            let mut turn_report = report(&format!("{POSE_TURN_PHASE}_{suffix}"), |index| {
                (0.0, index as f32 * 0.05)
            });
            for report in [&mut static_report, &mut walk_report, &mut turn_report] {
                report.config.draw_window_start = start;
                report.config.draw_window_count = 8;
                report.view_candidates[0].source.source.draw_ordinal = start + 7;
            }
            CameraEvidenceSweepInput {
                static_report,
                walk_report,
                turn_report,
            }
        };
        let report = analyze_camera_evidence_sweep([make_window(0), make_window(8)]).unwrap();
        assert_eq!(report.schema, CAMERA_EVIDENCE_SWEEP_REPORT_SCHEMA_V1);
        assert_eq!(report.windows.len(), 2);
        assert!(matches!(
            report.selection,
            CameraEvidenceSelection::Selected {
                equivalent_draw_ordinals,
                ..
            } if equivalent_draw_ordinals == vec![7, 15]
        ));
        assert!(report.requires_e_geo_validation);
        assert!(!report.production_binding_authorized);
    }

    #[test]
    fn sweep_rejects_empty_and_overlapping_window_sets() {
        assert!(analyze_camera_evidence_sweep(Vec::<CameraEvidenceSweepInput>::new()).is_err());
        let make_window = |start: u32| {
            let suffix = format!("W{start:03}");
            let mut static_report =
                report(&format!("{POSE_STATIC_PHASE}_{suffix}"), |_| (0.0, 0.0));
            let mut walk_report = report(&format!("{POSE_WALK_PHASE}_{suffix}"), |index| {
                (index as f32, 0.0)
            });
            let mut turn_report = report(&format!("{POSE_TURN_PHASE}_{suffix}"), |index| {
                (0.0, index as f32 * 0.05)
            });
            for report in [&mut static_report, &mut walk_report, &mut turn_report] {
                report.config.draw_window_start = start;
                report.config.draw_window_count = 8;
            }
            CameraEvidenceSweepInput {
                static_report,
                walk_report,
                turn_report,
            }
        };
        assert!(analyze_camera_evidence_sweep([make_window(0), make_window(4)]).is_err());
    }

    #[test]
    fn sustained_motion_tail_survives_operator_idle_without_accepting_one_spike() {
        let static_report = report(POSE_STATIC_PHASE, |_| (0.0, 0.0));
        let walk_report = report(POSE_WALK_PHASE, |index| {
            if index < 10 {
                (0.0, 0.0)
            } else {
                ((index - 9) as f32, 0.0)
            }
        });
        let turn_report = report(POSE_TURN_PHASE, |index| (0.0, index as f32 * 0.05));
        let evidence = analyze_camera_evidence(&static_report, &walk_report, &turn_report).unwrap();
        assert!(matches!(
            evidence.selection,
            CameraEvidenceSelection::Selected { .. }
        ));
        assert_eq!(evidence.candidates[0].walk_motion.translation_median, 0.0);
        assert!(evidence.candidates[0].walk_motion.translation_p95 > 0.0);
    }

    #[test]
    fn relative_response_cannot_hide_an_absolutely_unstable_static_candidate() {
        let static_report = report(POSE_STATIC_PHASE, |index| {
            (if index % 2 == 0 { 0.0 } else { 1.0 }, 0.0)
        });
        let walk_report = report(POSE_WALK_PHASE, |index| (index as f32 * 10.0, 0.0));
        let turn_report = report(POSE_TURN_PHASE, |index| (0.0, index as f32 * 0.1));
        let evidence = analyze_camera_evidence(&static_report, &walk_report, &turn_report).unwrap();
        assert!(matches!(
            evidence.selection,
            CameraEvidenceSelection::NoMatch
        ));
        assert!(!evidence.candidates[0].static_absolute_stable);
        assert!(!evidence.candidates[0].static_stable);
    }

    #[test]
    fn unreliable_phase_is_rejected_before_scoring() {
        let static_report = report(POSE_STATIC_PHASE, |_| (0.0, 0.0));
        let mut walk_report = report(POSE_WALK_PHASE, |index| (index as f32, 0.0));
        let turn_report = report(POSE_TURN_PHASE, |index| (0.0, index as f32 * 0.05));
        walk_report.reliable = false;
        assert!(analyze_camera_evidence(&static_report, &walk_report, &turn_report).is_err());
    }
}

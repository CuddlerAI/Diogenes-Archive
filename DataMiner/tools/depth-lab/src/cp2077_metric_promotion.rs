//! Git-safe promotion record for a private CP2077 metric calibration.
//!
//! The record deliberately contains only exact-build identities, public Source A provenance,
//! aggregate Source B measurements and anonymous content digests. Raw screenshots, positions,
//! Present/QPC identities and operator/session/device identifiers remain in private storage.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::Read as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest as _, Sha256};

use crate::cp2077_metric_evidence::{
    CP2077_HUD_METRIC_OBSERVATIONS_SCHEMA_V2, CP2077_METRIC_EVIDENCE_SCHEMA_V1,
    CP2077_METRIC_FRAME_SCHEMA_V1, CP2077_METRIC_POSE_SCHEMA_V1, Cp2077MetricEvidence,
    MetricCrossCheckEvidence, MetricSourceA, UpstreamMetricSource, validate_cp2077_metric_evidence,
};

pub const CP2077_METRIC_PROMOTION_SCHEMA_V1: &str = "dm-cp2077-metric-promotion-evidence/v1";

const GAME_ID: &str = "cyberpunk-2077-steam";
const STEAM_APP_ID: u64 = 1_091_500;
const STEAM_BUILD_ID: u64 = 20_383_525;
const PRODUCT_VERSION: &str = "2.31";
const EXPECTED_EXE_SHA256: &str =
    "sha256:a7de82945c03e041fc7339fcf9066224d98db2f5d80fea50f7947bb350a60991";
const EXPECTED_PROBE_SHA256: &str =
    "sha256:1130b1c0d4bd03a7bbb09e22d35bce238ad27a39377062b9c0c27a6e468fa4aa";
const SETTINGS_RUNTIME_SCHEMA_V1: &str =
    "dm-cp2077-metric-calibration-settings-runtime-evidence/v1";
const RENDER_CAMERA_SCHEMA_V1: &str = "dm-cp2077-render-camera-observation/v1";
const RENDER_CAMERA_SCHEMA_V2: &str = "dm-cp2077-render-camera-observation/v2";
const POSE_RUNTIME_SCHEMA_V1: &str = "cp2077-pose-runtime-observation/v1";
const PROMOTED_LEGACY_RENDER_CAMERA_SHA256: &str =
    "sha256:f169241430913b1606fafc1731b11eeef8c229cec506a4cbdd83482b4b1d3670";
const PROMOTION_MAX_BYTES: u64 = 1024 * 1024;
const PRIVATE_INPUT_MAX_BYTES: u64 = 32 * 1024 * 1024;
const MAX_PATH_TO_CHORD_RATIO: f64 = 1.02;
const MIN_HUD_SPAN_METRES: f64 = 20.0;
const MIN_SAMPLES: usize = 3;
const MAX_SAMPLES: usize = 32;
const MIN_RENDER_CAMERA_PRESENTS: usize = 120;
const RENDER_FOV_TOLERANCE_DEGREES: f64 = 1.0;
const MAX_PROJECTION_MATRIX_DELTA: f64 = 0.001;

const METRIC_REPORT_FILE: &str = "cp2077-metric-evidence.json";
const SETTINGS_RUNTIME_FILE: &str = "metric-calibration-settings-runtime-evidence.json";
const RAW_PROFILE_FILE: &str = "raw-depth-demo-profile.json";
const RENDER_CAMERA_FILE: &str = "render-camera-observation.json";
const POSE_RUNTIME_FILE: &str = "cp2077-pose-runtime-observation.json";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cp2077MetricPromotionEvidence {
    pub schema: String,
    pub game: PromotionGameIdentity,
    pub contracts: PromotionContracts,
    pub controlled_settings: PromotionSettings,
    pub source_a: MetricSourceA,
    pub source_b_summary: PromotionSourceBSummary,
    pub cross_check: MetricCrossCheckEvidence,
    pub adopted_metres_per_world_unit: f64,
    pub private_evidence: PrivateEvidenceManifest,
    pub review: PromotionReview,
    pub decision: PromotionDecision,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromotionGameIdentity {
    pub game_id: String,
    pub steam_app_id: u64,
    pub steam_build_id: u64,
    pub product_version: String,
    pub executable_sha256: String,
    pub probe_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromotionContracts {
    pub metric_evidence: String,
    pub metric_pose: String,
    pub metric_frame_identity: String,
    pub hud_observations: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromotionSettings {
    pub width: u32,
    pub height: u32,
    pub fov_degrees: f64,
    pub allowed_window_modes: Vec<String>,
    pub settings_runtime_evidence_sha256: String,
    pub runtime_fov_evidence_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromotionSourceBSummary {
    pub sample_count: usize,
    pub hud_span_metres: f64,
    pub probe_chord_world_units: f64,
    pub probe_sample_path_world_units: f64,
    pub path_to_chord_ratio: f64,
    pub measured_metres_per_world_unit: f64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrivateEvidenceManifest {
    pub immutable_private_storage_required: bool,
    pub raw_material_in_git: bool,
    pub inputs: Vec<AnonymousEvidenceDigest>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnonymousEvidenceDigest {
    pub role: String,
    pub bytes: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromotionReview {
    pub method: String,
    pub operator_reviewer_distinct: bool,
    pub every_screenshot_reviewed: bool,
    pub same_objective_confirmed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromotionDecision {
    pub metric_gate: String,
    pub local_only: bool,
    pub non_trainable: bool,
    pub production_certified: bool,
    pub upload_allowed: bool,
}

#[derive(Debug, Deserialize)]
struct SettingsRuntimeIdentity {
    schema: String,
    purpose: String,
    process_launch_id: String,
    runtime_fov_memory_evidence: String,
    settings_ready: bool,
    recording_ready: bool,
    non_trainable: bool,
}

#[derive(Debug, Deserialize)]
struct PoseRuntimeIdentity {
    schema: String,
    exact_build: PoseExactBuildIdentity,
    process_launch_id: String,
    absolute_process_addresses_recorded: bool,
    production_binding_emitted: bool,
    recording_ready: bool,
}

#[derive(Debug, Deserialize)]
struct PoseExactBuildIdentity {
    app_id: u64,
    build_id: u64,
    product_version: String,
    executable_sha256: String,
}

#[derive(Debug, Deserialize)]
struct RenderCameraSchemaIdentity {
    schema: String,
}

#[derive(Debug, Deserialize)]
struct RenderCameraReport<T> {
    schema: String,
    process_launch_id: String,
    non_trainable: bool,
    production_certified: bool,
    deliverable: bool,
    recording_ready: bool,
    observation: T,
}

#[derive(Debug, Deserialize)]
struct LegacyRenderCameraObservation {
    selected_depth_armed: bool,
    blockers: Vec<String>,
    candidate_admitted_for_cross_launch_discovery: bool,
    production_camera_binding_emitted: bool,
    recording_ready: bool,
}

#[derive(Debug, Deserialize)]
struct CurrentRenderCameraObservation {
    selected_depth_armed: bool,
    observations: Vec<CurrentRenderDrawObservation>,
    blockers: Vec<String>,
    authoring_blockers: Vec<String>,
    candidate_admitted_for_cross_launch_discovery: bool,
    production_camera_binding_emitted: bool,
    recording_ready: bool,
}

#[derive(Debug, Deserialize)]
struct CurrentRenderDrawObservation {
    present_seq: u64,
    copy_before_clear_submitted: bool,
    selected_present_identity_passed: bool,
    physical_source: String,
    matrices: Vec<CurrentRenderMatrixObservation>,
}

#[derive(Debug, Deserialize)]
struct CurrentRenderMatrixObservation {
    root_parameter_index: u32,
    cbv_resource_offset: u64,
    matrix_offset: u32,
    transpose: bool,
    matrix_kind: String,
    matrix: [[f64; 4]; 4],
    horizontal_fov_degrees: Option<f64>,
    configured_vertical_fov_matches: Option<bool>,
    physical_source: String,
    stable_source: Option<StableD3d12MatrixSource>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(deny_unknown_fields)]
struct StableD3d12MatrixSource {
    descriptor_table: StableD3d12DescriptorTable,
    cbv_resource_offset: u64,
    declared_cbv_bytes: u32,
    matrix_offset: u32,
    encoding: String,
    layout: String,
    transpose: bool,
    role: String,
    draw_relation: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(deny_unknown_fields)]
struct StableD3d12DescriptorTable {
    root_signature_sha256: String,
    root_parameter_index: u32,
    descriptor_range_index: u32,
    descriptor_offset_in_range: u32,
    descriptor_count: u32,
    base_shader_register: u32,
    register_space: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MetricPromotionValidation {
    pub steam_build_id: u64,
    pub adopted_metres_per_world_unit: f64,
    pub private_inputs_verified: bool,
}

pub fn verify_metric_promotion(path: &Path) -> Result<MetricPromotionValidation, String> {
    let path = canonical_regular_file(path, PROMOTION_MAX_BYTES, "promotion evidence")?;
    let promotion: Cp2077MetricPromotionEvidence =
        serde_json::from_slice(&read_bounded(&path, PROMOTION_MAX_BYTES)?)
            .map_err(|error| format!("parse CP2077 metric promotion failed: {error}"))?;
    validate_promotion(&promotion)?;
    Ok(MetricPromotionValidation {
        steam_build_id: promotion.game.steam_build_id,
        adopted_metres_per_world_unit: promotion.adopted_metres_per_world_unit,
        private_inputs_verified: false,
    })
}

pub fn verify_private_metric_promotion(
    promotion_path: &Path,
    private_session: &Path,
) -> Result<MetricPromotionValidation, String> {
    let promotion_path =
        canonical_regular_file(promotion_path, PROMOTION_MAX_BYTES, "promotion evidence")?;
    let promotion: Cp2077MetricPromotionEvidence =
        serde_json::from_slice(&read_bounded(&promotion_path, PROMOTION_MAX_BYTES)?)
            .map_err(|error| format!("parse CP2077 metric promotion failed: {error}"))?;
    validate_promotion(&promotion)?;

    let session = private_session
        .canonicalize()
        .map_err(|error| format!("canonicalize private metric session failed: {error}"))?;
    if !session.is_dir() {
        return Err("private metric session is not a directory".into());
    }
    let roles = digest_roles(&promotion)?;
    let report_path = checked_session_leaf(&session, METRIC_REPORT_FILE)?;
    check_digest(&report_path, roles["metric_evidence_report"])?;
    validate_cp2077_metric_evidence(&session, &report_path)?;
    let report: Cp2077MetricEvidence =
        serde_json::from_slice(&read_bounded(&report_path, PRIVATE_INPUT_MAX_BYTES)?)
            .map_err(|error| format!("parse private metric evidence failed: {error}"))?;
    compare_report(&promotion, &report)?;

    check_report_file(
        &session,
        &report.pose,
        roles["metric_pose_rows"],
        "metric pose",
    )?;
    check_report_file(
        &session,
        &report.frame_evidence,
        roles["metric_frame_identity_rows"],
        "metric frame identity",
    )?;
    check_report_file(
        &session,
        &report.hud_observations,
        roles["hud_observations"],
        "HUD observations",
    )?;
    for (index, sample) in report.source_b.samples.iter().enumerate() {
        check_report_file(
            &session,
            &sample.screenshot,
            roles[&format!("hud_screenshot_{index}")],
            "HUD screenshot",
        )?;
    }

    let settings_path = checked_session_leaf(&session, SETTINGS_RUNTIME_FILE)?;
    let raw_profile_path = checked_session_leaf(&session, RAW_PROFILE_FILE)?;
    let render_camera_path = checked_session_leaf(&session, RENDER_CAMERA_FILE)?;
    let pose_runtime_path = checked_session_leaf(&session, POSE_RUNTIME_FILE)?;
    check_digest(&settings_path, roles["settings_runtime_evidence"])?;
    check_digest(&raw_profile_path, roles["raw_depth_profile_identity"])?;
    check_digest(&render_camera_path, roles["render_camera_observation"])?;
    check_digest(&pose_runtime_path, roles["pose_runtime_observation"])?;
    validate_private_identities(
        &promotion,
        &settings_path,
        &raw_profile_path,
        &render_camera_path,
        &pose_runtime_path,
    )?;

    Ok(MetricPromotionValidation {
        steam_build_id: promotion.game.steam_build_id,
        adopted_metres_per_world_unit: promotion.adopted_metres_per_world_unit,
        private_inputs_verified: true,
    })
}

fn validate_promotion(promotion: &Cp2077MetricPromotionEvidence) -> Result<(), String> {
    if promotion.schema != CP2077_METRIC_PROMOTION_SCHEMA_V1 {
        return Err("unsupported CP2077 metric promotion schema".into());
    }
    let game = &promotion.game;
    if game.game_id != GAME_ID
        || game.steam_app_id != STEAM_APP_ID
        || game.steam_build_id != STEAM_BUILD_ID
        || game.product_version != PRODUCT_VERSION
        || game.executable_sha256 != EXPECTED_EXE_SHA256
        || game.probe_sha256 != EXPECTED_PROBE_SHA256
    {
        return Err("CP2077 promotion exact-build identity is invalid".into());
    }
    let contracts = &promotion.contracts;
    if contracts.metric_evidence != CP2077_METRIC_EVIDENCE_SCHEMA_V1
        || contracts.metric_pose != CP2077_METRIC_POSE_SCHEMA_V1
        || contracts.metric_frame_identity != CP2077_METRIC_FRAME_SCHEMA_V1
        || contracts.hud_observations != CP2077_HUD_METRIC_OBSERVATIONS_SCHEMA_V2
    {
        return Err("CP2077 promotion contract identity drifted".into());
    }
    let settings = &promotion.controlled_settings;
    if settings.width != 1920
        || settings.height != 1080
        || settings.fov_degrees != 75.0
        || settings.allowed_window_modes != ["fullscreen", "borderless"]
        || !valid_sha256(&settings.settings_runtime_evidence_sha256)
        || !valid_sha256(&settings.runtime_fov_evidence_sha256)
    {
        return Err("CP2077 promotion controlled settings are invalid".into());
    }
    if promotion.source_a != fixed_source_a() {
        return Err("CP2077 promotion Source A provenance drifted".into());
    }
    let source_b = &promotion.source_b_summary;
    if !(MIN_SAMPLES..=MAX_SAMPLES).contains(&source_b.sample_count)
        || !positive_finite(source_b.hud_span_metres)
        || source_b.hud_span_metres < MIN_HUD_SPAN_METRES
        || !positive_finite(source_b.probe_chord_world_units)
        || !positive_finite(source_b.probe_sample_path_world_units)
        || !positive_finite(source_b.path_to_chord_ratio)
        || source_b.path_to_chord_ratio > MAX_PATH_TO_CHORD_RATIO
        || !positive_finite(source_b.measured_metres_per_world_unit)
        || !same_aggregate(
            source_b.path_to_chord_ratio,
            source_b.probe_sample_path_world_units / source_b.probe_chord_world_units,
        )
        || !same_aggregate(
            source_b.measured_metres_per_world_unit,
            source_b.hud_span_metres / source_b.probe_chord_world_units,
        )
    {
        return Err("CP2077 promotion Source B summary is invalid".into());
    }
    let cross = &promotion.cross_check;
    if cross.source_a != promotion.source_a.inferred_metres_per_converted_world_unit
        || cross.source_b != source_b.measured_metres_per_world_unit
        || cross.ratio != cross.source_b / cross.source_a
        || cross.tolerance != 0.05
        || !cross.passed
        || (cross.ratio - 1.0).abs() > cross.tolerance
        || promotion.adopted_metres_per_world_unit != cross.source_a
    {
        return Err("CP2077 promotion metric cross-check is invalid".into());
    }
    if !promotion
        .private_evidence
        .immutable_private_storage_required
        || promotion.private_evidence.raw_material_in_git
    {
        return Err("CP2077 promotion weakens private evidence storage boundaries".into());
    }
    let roles = digest_roles(promotion)?;
    if roles["settings_runtime_evidence"].sha256
        != promotion
            .controlled_settings
            .settings_runtime_evidence_sha256
        || roles["render_camera_observation"].sha256
            != promotion.controlled_settings.runtime_fov_evidence_sha256
    {
        return Err(
            "controlled-settings evidence digests do not match the private manifest".into(),
        );
    }
    if promotion.review.method != "independent_human_transcription"
        || !promotion.review.operator_reviewer_distinct
        || !promotion.review.every_screenshot_reviewed
        || !promotion.review.same_objective_confirmed
    {
        return Err("CP2077 promotion independent review is incomplete".into());
    }
    if promotion.decision.metric_gate != "passed"
        || !promotion.decision.local_only
        || !promotion.decision.non_trainable
        || promotion.decision.production_certified
        || promotion.decision.upload_allowed
    {
        return Err("CP2077 promotion decision crosses the DEV/local-only boundary".into());
    }
    Ok(())
}

fn digest_roles(
    promotion: &Cp2077MetricPromotionEvidence,
) -> Result<BTreeMap<String, &AnonymousEvidenceDigest>, String> {
    let mut expected = BTreeSet::from([
        "metric_evidence_report".to_string(),
        "metric_pose_rows".to_string(),
        "metric_frame_identity_rows".to_string(),
        "hud_observations".to_string(),
        "settings_runtime_evidence".to_string(),
        "raw_depth_profile_identity".to_string(),
        "render_camera_observation".to_string(),
        "pose_runtime_observation".to_string(),
    ]);
    for index in 0..promotion.source_b_summary.sample_count {
        expected.insert(format!("hud_screenshot_{index}"));
    }
    let mut observed = BTreeMap::new();
    for digest in &promotion.private_evidence.inputs {
        if digest.bytes == 0 || !valid_sha256(&digest.sha256) {
            return Err(format!(
                "invalid anonymous evidence digest for {}",
                digest.role
            ));
        }
        if observed.insert(digest.role.clone(), digest).is_some() {
            return Err(format!("duplicate anonymous evidence role {}", digest.role));
        }
    }
    if observed.keys().cloned().collect::<BTreeSet<_>>() != expected {
        return Err("anonymous private evidence roles are incomplete or unexpected".into());
    }
    Ok(observed)
}

fn compare_report(
    promotion: &Cp2077MetricPromotionEvidence,
    report: &Cp2077MetricEvidence,
) -> Result<(), String> {
    let source_b = &promotion.source_b_summary;
    if report.steam_build_id != promotion.game.steam_build_id
        || report.executable_sha256 != promotion.game.executable_sha256
        || report.source_a != promotion.source_a
        || report.source_b.samples.len() != source_b.sample_count
        || report.source_b.hud_span_metres != source_b.hud_span_metres
        || report.source_b.probe_chord_world_units != source_b.probe_chord_world_units
        || report.source_b.probe_sample_path_world_units != source_b.probe_sample_path_world_units
        || report.source_b.path_to_chord_ratio != source_b.path_to_chord_ratio
        || report.source_b.measured_metres_per_world_unit != source_b.measured_metres_per_world_unit
        || report.cross_check != promotion.cross_check
        || report.adopted_metres_per_world_unit != promotion.adopted_metres_per_world_unit
    {
        return Err("promotion summary does not exactly match the sealed metric report".into());
    }
    Ok(())
}

fn validate_private_identities(
    promotion: &Cp2077MetricPromotionEvidence,
    settings_path: &Path,
    raw_profile_path: &Path,
    render_camera_path: &Path,
    pose_runtime_path: &Path,
) -> Result<(), String> {
    let settings: SettingsRuntimeIdentity = read_json_document(settings_path, "settings")?;
    validate_settings_identity(&settings)?;

    let raw_profile = read_json_value(raw_profile_path)?;
    if raw_profile.pointer("/schema").and_then(|v| v.as_str())
        != Some("dm-local-raw-depth-demo-profile/v1")
        || raw_profile.pointer("/game_id").and_then(|v| v.as_str()) != Some(GAME_ID)
        || raw_profile
            .pointer("/executable_sha256")
            .and_then(|v| v.as_str())
            != Some(promotion.game.executable_sha256.as_str())
        || raw_profile
            .pointer("/resident_probe_sha256")
            .and_then(|v| v.as_str())
            != Some(promotion.game.probe_sha256.as_str())
        || raw_profile
            .pointer("/selector/width")
            .and_then(|v| v.as_u64())
            != Some(u64::from(promotion.controlled_settings.width))
        || raw_profile
            .pointer("/selector/height")
            .and_then(|v| v.as_u64())
            != Some(u64::from(promotion.controlled_settings.height))
        || raw_profile
            .pointer("/non_trainable")
            .and_then(|v| v.as_bool())
            != Some(true)
        || raw_profile
            .pointer("/production_certified")
            .and_then(|v| v.as_bool())
            != Some(false)
        || raw_profile
            .pointer("/upload_allowed")
            .and_then(|v| v.as_bool())
            != Some(false)
    {
        return Err("private raw-depth profile identity is invalid".into());
    }

    let pose_runtime: PoseRuntimeIdentity = read_json_document(pose_runtime_path, "pose runtime")?;
    validate_pose_identity(promotion, &pose_runtime)?;
    let (_, render_sha256) = file_digest(render_camera_path)?;
    let render_launch = validate_render_camera_document(
        promotion,
        &read_bounded(render_camera_path, PRIVATE_INPUT_MAX_BYTES)?,
        &render_sha256,
    )?;
    validate_launch_coherence(
        &settings.process_launch_id,
        &render_launch,
        &pose_runtime.process_launch_id,
    )?;
    Ok(())
}

fn validate_settings_identity(settings: &SettingsRuntimeIdentity) -> Result<(), String> {
    if settings.schema != SETTINGS_RUNTIME_SCHEMA_V1
        || settings.purpose != "metric_calibration"
        || !valid_process_launch_id(&settings.process_launch_id)
        || !settings.settings_ready
        || settings.recording_ready
        || !settings.non_trainable
        || settings.runtime_fov_memory_evidence != RENDER_CAMERA_FILE
    {
        return Err("private settings identity is invalid".into());
    }
    Ok(())
}

fn validate_pose_identity(
    promotion: &Cp2077MetricPromotionEvidence,
    pose: &PoseRuntimeIdentity,
) -> Result<(), String> {
    if pose.schema != POSE_RUNTIME_SCHEMA_V1
        || !valid_process_launch_id(&pose.process_launch_id)
        || pose.exact_build.app_id != promotion.game.steam_app_id
        || pose.exact_build.build_id != promotion.game.steam_build_id
        || pose.exact_build.product_version != promotion.game.product_version
        || pose.exact_build.executable_sha256 != promotion.game.executable_sha256
        || pose.absolute_process_addresses_recorded
        || pose.production_binding_emitted
        || pose.recording_ready
    {
        return Err("private pose-runtime exact-build identity is invalid".into());
    }
    Ok(())
}

fn validate_render_camera_document(
    promotion: &Cp2077MetricPromotionEvidence,
    bytes: &[u8],
    sha256: &str,
) -> Result<String, String> {
    let identity: RenderCameraSchemaIdentity = serde_json::from_slice(bytes)
        .map_err(|error| format!("parse render-camera schema identity failed: {error}"))?;
    match identity.schema.as_str() {
        RENDER_CAMERA_SCHEMA_V1 => {
            if sha256 != PROMOTED_LEGACY_RENDER_CAMERA_SHA256 {
                return Err(
                    "render-camera v1 is allowed only for the exact already-promoted artifact"
                        .into(),
                );
            }
            let report: RenderCameraReport<LegacyRenderCameraObservation> =
                serde_json::from_slice(bytes).map_err(|error| {
                    format!("parse legacy render-camera evidence failed: {error}")
                })?;
            validate_render_envelope(&report, RENDER_CAMERA_SCHEMA_V1)?;
            let observation = &report.observation;
            if !observation.selected_depth_armed
                || !observation.candidate_admitted_for_cross_launch_discovery
                || !observation.blockers.is_empty()
                || observation.production_camera_binding_emitted
                || observation.recording_ready
            {
                return Err("legacy render-camera evidence is not safely admitted".into());
            }
            Ok(report.process_launch_id)
        }
        RENDER_CAMERA_SCHEMA_V2 => {
            let report: RenderCameraReport<CurrentRenderCameraObservation> =
                serde_json::from_slice(bytes).map_err(|error| {
                    format!("parse current render-camera evidence failed: {error}")
                })?;
            validate_render_envelope(&report, RENDER_CAMERA_SCHEMA_V2)?;
            validate_current_render_camera(promotion, &report.observation)?;
            Ok(report.process_launch_id)
        }
        _ => Err("unsupported private render-camera evidence schema".into()),
    }
}

fn validate_render_envelope<T>(
    report: &RenderCameraReport<T>,
    expected_schema: &str,
) -> Result<(), String> {
    if report.schema != expected_schema
        || !valid_process_launch_id(&report.process_launch_id)
        || !report.non_trainable
        || report.production_certified
        || report.deliverable
        || report.recording_ready
    {
        return Err("private render-camera safety envelope is invalid".into());
    }
    Ok(())
}

fn validate_current_render_camera(
    promotion: &Cp2077MetricPromotionEvidence,
    observation: &CurrentRenderCameraObservation,
) -> Result<(), String> {
    if !observation.selected_depth_armed
        || !observation.candidate_admitted_for_cross_launch_discovery
        || !observation.blockers.is_empty()
        || !observation.authoring_blockers.is_empty()
        || observation.production_camera_binding_emitted
        || observation.recording_ready
    {
        return Err("current render-camera evidence is not safely admitted".into());
    }

    let mut draw_sources = BTreeSet::new();
    let mut matrix_sources = BTreeSet::new();
    let mut view_sources = BTreeMap::<StableD3d12MatrixSource, BTreeSet<u64>>::new();
    let mut projection_sources = BTreeMap::<StableD3d12MatrixSource, BTreeSet<u64>>::new();
    let mut first_projection = None;
    for draw in observation
        .observations
        .iter()
        .filter(|draw| draw.copy_before_clear_submitted && draw.selected_present_identity_passed)
    {
        draw_sources.insert(draw.physical_source.as_str());
        for matrix in &draw.matrices {
            if matrix.matrix_kind != "view" && matrix.matrix_kind != "projection" {
                continue;
            }
            if !matrix
                .matrix
                .iter()
                .flatten()
                .all(|entry| entry.is_finite())
                || !matrix_source_matches_draw(&draw.physical_source, &matrix.physical_source)
            {
                return Err("render-camera matrix physical source is invalid".into());
            }
            matrix_sources.insert(matrix.physical_source.as_str());
            let stable = matrix
                .stable_source
                .as_ref()
                .ok_or_else(|| "render-camera matrix has no stable source".to_string())?;
            validate_stable_source(stable, &matrix.matrix_kind)?;
            if matrix.root_parameter_index != stable.descriptor_table.root_parameter_index
                || matrix.cbv_resource_offset != stable.cbv_resource_offset
                || matrix.matrix_offset != stable.matrix_offset
                || matrix.transpose != stable.transpose
            {
                return Err(
                    "render-camera diagnostic identity contradicts its stable source".into(),
                );
            }
            if matrix.matrix_kind == "view" {
                view_sources
                    .entry(stable.clone())
                    .or_default()
                    .insert(draw.present_seq);
            } else if matrix.configured_vertical_fov_matches == Some(true) {
                let fov = matrix
                    .horizontal_fov_degrees
                    .filter(|value| value.is_finite())
                    .ok_or_else(|| "render-camera projection has no actual FOV".to_string())?;
                if (fov - promotion.controlled_settings.fov_degrees).abs()
                    > RENDER_FOV_TOLERANCE_DEGREES
                {
                    return Err("render-camera projection does not match configured FOV 75".into());
                }
                if let Some(first) = first_projection {
                    if matrix_delta(first, matrix.matrix) > MAX_PROJECTION_MATRIX_DELTA {
                        return Err("render-camera projection is not stable".into());
                    }
                } else {
                    first_projection = Some(matrix.matrix);
                }
                projection_sources
                    .entry(stable.clone())
                    .or_default()
                    .insert(draw.present_seq);
            }
        }
    }
    if draw_sources.len() != 1 || matrix_sources.len() != 1 {
        return Err("render-camera physical source is not stable".into());
    }
    let admitted_views = view_sources
        .iter()
        .filter(|(_, presents)| presents.len() >= MIN_RENDER_CAMERA_PRESENTS)
        .map(|(source, _)| source)
        .collect::<Vec<_>>();
    let admitted_projections = projection_sources
        .iter()
        .filter(|(_, presents)| presents.len() >= MIN_RENDER_CAMERA_PRESENTS)
        .map(|(source, _)| source)
        .collect::<Vec<_>>();
    let ([view], [projection]) = (admitted_views.as_slice(), admitted_projections.as_slice())
    else {
        return Err("render-camera evidence lacks one stable view/projection source".into());
    };
    if view.descriptor_table != projection.descriptor_table
        || view.cbv_resource_offset != projection.cbv_resource_offset
        || view.declared_cbv_bytes != projection.declared_cbv_bytes
        || view.draw_relation != projection.draw_relation
        || view.matrix_offset == projection.matrix_offset
    {
        return Err("render-camera view/projection source identity mismatched".into());
    }
    Ok(())
}

fn validate_stable_source(
    source: &StableD3d12MatrixSource,
    expected_role: &str,
) -> Result<(), String> {
    let descriptor = &source.descriptor_table;
    if source.role != expected_role
        || source.encoding != "f32_little_endian"
        || source.layout != "row_major"
        || source.draw_relation != "exact_selected_depth_dsv_draw"
        || !valid_sha256(&descriptor.root_signature_sha256)
        || descriptor.descriptor_count == 0
        || descriptor.descriptor_offset_in_range >= descriptor.descriptor_count
        || source.declared_cbv_bytes < 64
        || !source.declared_cbv_bytes.is_multiple_of(256)
        || !source.cbv_resource_offset.is_multiple_of(256)
        || !source.matrix_offset.is_multiple_of(16)
        || source
            .matrix_offset
            .checked_add(64)
            .is_none_or(|end| end > source.declared_cbv_bytes)
    {
        return Err("render-camera stable matrix source is invalid".into());
    }
    Ok(())
}

fn matrix_source_matches_draw(draw: &str, matrix: &str) -> bool {
    matches!(
        (draw, matrix),
        (
            "direct_game_mapped_root_cbv_at_selected_dsv_draw",
            "direct_root_cbv"
        ) | (
            "game_mapped_root_signature_declared_cbv_at_selected_dsv_draw",
            "root_signature_declared_cbv"
        ) | (
            "upload_unmap_snapshot_root_signature_declared_cbv_at_selected_dsv_draw",
            "upload_unmap_snapshot_root_signature_declared_cbv"
        ) | (
            "copy_buffer_region_root_signature_declared_cbv_at_selected_dsv_draw",
            "copy_buffer_region_root_signature_declared_cbv"
        )
    )
}

fn matrix_delta(left: [[f64; 4]; 4], right: [[f64; 4]; 4]) -> f64 {
    left.iter()
        .flatten()
        .zip(right.iter().flatten())
        .map(|(left, right)| (left - right).abs())
        .fold(0.0, f64::max)
}

fn validate_launch_coherence(settings: &str, render: &str, pose: &str) -> Result<(), String> {
    if settings != render || settings != pose {
        return Err("settings/render/pose process_launch_id mismatch".into());
    }
    Ok(())
}

fn check_report_file(
    session: &Path,
    report_file: &crate::cp2077_metric_evidence::EvidenceFile,
    anonymous: &AnonymousEvidenceDigest,
    label: &str,
) -> Result<(), String> {
    if report_file.bytes != anonymous.bytes || report_file.sha256 != anonymous.sha256 {
        return Err(format!(
            "{label} digest differs between report and promotion"
        ));
    }
    let path = checked_session_leaf(session, &report_file.file)?;
    check_digest(&path, anonymous)
}

fn check_digest(path: &Path, expected: &AnonymousEvidenceDigest) -> Result<(), String> {
    let (bytes, sha256) = file_digest(path)?;
    if bytes != expected.bytes || sha256 != expected.sha256 {
        return Err(format!(
            "private evidence hash mismatch for role {}",
            expected.role
        ));
    }
    Ok(())
}

fn checked_session_leaf(session: &Path, leaf: &str) -> Result<PathBuf, String> {
    let leaf_path = Path::new(leaf);
    if leaf.is_empty()
        || leaf_path.is_absolute()
        || leaf_path.file_name().and_then(|name| name.to_str()) != Some(leaf)
    {
        return Err("private evidence path is not a plain leaf name".into());
    }
    let path = canonical_regular_file(
        &session.join(leaf),
        PRIVATE_INPUT_MAX_BYTES,
        "private evidence input",
    )?;
    if path.parent() != Some(session) {
        return Err("private evidence input escapes the session".into());
    }
    Ok(path)
}

fn canonical_regular_file(path: &Path, max: u64, label: &str) -> Result<PathBuf, String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("read {label} metadata failed: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > max {
        return Err(format!("{label} is not a bounded regular non-symlink file"));
    }
    path.canonicalize()
        .map_err(|error| format!("canonicalize {label} failed: {error}"))
}

fn read_bounded(path: &Path, max: u64) -> Result<Vec<u8>, String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("read evidence metadata failed: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > max {
        return Err("evidence input is not a bounded regular non-symlink file".into());
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    File::open(path)
        .and_then(|mut file| file.read_to_end(&mut bytes))
        .map_err(|error| format!("read evidence input failed: {error}"))?;
    Ok(bytes)
}

fn read_json_value(path: &Path) -> Result<serde_json::Value, String> {
    serde_json::from_slice(&read_bounded(path, PRIVATE_INPUT_MAX_BYTES)?)
        .map_err(|error| format!("parse private identity evidence failed: {error}"))
}

fn read_json_document<T: DeserializeOwned>(path: &Path, label: &str) -> Result<T, String> {
    serde_json::from_slice(&read_bounded(path, PRIVATE_INPUT_MAX_BYTES)?)
        .map_err(|error| format!("parse private {label} evidence failed: {error}"))
}

fn file_digest(path: &Path) -> Result<(u64, String), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("read evidence hash metadata failed: {error}"))?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() > PRIVATE_INPUT_MAX_BYTES
    {
        return Err("hash input is not a bounded regular non-symlink file".into());
    }
    let mut file = File::open(path).map_err(|error| format!("open hash input failed: {error}"))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|error| format!("read hash input failed: {error}"))?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok((metadata.len(), format!("sha256:{:x}", hasher.finalize())))
}

fn fixed_source_a() -> MetricSourceA {
    MetricSourceA {
        kind: "red4ext_worldposition_plus_cet_distance_meter".into(),
        red4ext_world_position: UpstreamMetricSource {
            repository: "https://github.com/WopsS/RED4ext.SDK".into(),
            commit: "ad7277714ad30d6885d7050c5ba24fa0102f6920".into(),
            path: "include/RED4ext/Scripting/Natives/WorldPosition.hpp".into(),
            git_blob_sha1: "ddc2eb166a9b30b9f76f56354cf56c6d5d93320e".into(),
        },
        fixed_point_divisor: 131_072,
        world_position_bytes: 12,
        cet_distance_meter: UpstreamMetricSource {
            repository: "https://github.com/martine-dee/cp2077-traveling-companion-distance-meter"
                .into(),
            commit: "ed6d40fb2facf2f529bb30a1ba707ccb1ed08e04".into(),
            path:
                "bin/x64/plugins/cyber_engine_tweaks/mods/TravelingCompanionDistanceMeter/init.lua"
                    .into(),
            git_blob_sha1: "e1703719bf5e37917063435eff6b2e6dce1cb119".into(),
        },
        cet_algorithm: "euclidean_delta_of_consecutive_Game.GetPlayer_GetWorldPosition".into(),
        cet_output_unit: "metres".into(),
        inferred_metres_per_converted_world_unit: 1.0,
    }
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 71
        && value.starts_with("sha256:")
        && value[7..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn valid_process_launch_id(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn positive_finite(value: f64) -> bool {
    value.is_finite() && value > 0.0
}

fn same_aggregate(observed: f64, recomputed: f64) -> bool {
    observed.is_finite()
        && recomputed.is_finite()
        && (observed - recomputed).abs()
            <= f64::EPSILON * 4.0 * observed.abs().max(recomputed.abs()).max(1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest(role: &str) -> AnonymousEvidenceDigest {
        AnonymousEvidenceDigest {
            role: role.into(),
            bytes: 1,
            sha256: format!("sha256:{}", "a".repeat(64)),
        }
    }

    fn valid() -> Cp2077MetricPromotionEvidence {
        let mut inputs = vec![
            digest("metric_evidence_report"),
            digest("metric_pose_rows"),
            digest("metric_frame_identity_rows"),
            digest("hud_observations"),
            digest("settings_runtime_evidence"),
            digest("raw_depth_profile_identity"),
            digest("render_camera_observation"),
            digest("pose_runtime_observation"),
        ];
        for index in 0..3 {
            inputs.push(digest(&format!("hud_screenshot_{index}")));
        }
        Cp2077MetricPromotionEvidence {
            schema: CP2077_METRIC_PROMOTION_SCHEMA_V1.into(),
            game: PromotionGameIdentity {
                game_id: GAME_ID.into(),
                steam_app_id: STEAM_APP_ID,
                steam_build_id: STEAM_BUILD_ID,
                product_version: PRODUCT_VERSION.into(),
                executable_sha256: EXPECTED_EXE_SHA256.into(),
                probe_sha256: EXPECTED_PROBE_SHA256.into(),
            },
            contracts: PromotionContracts {
                metric_evidence: CP2077_METRIC_EVIDENCE_SCHEMA_V1.into(),
                metric_pose: CP2077_METRIC_POSE_SCHEMA_V1.into(),
                metric_frame_identity: CP2077_METRIC_FRAME_SCHEMA_V1.into(),
                hud_observations: CP2077_HUD_METRIC_OBSERVATIONS_SCHEMA_V2.into(),
            },
            controlled_settings: PromotionSettings {
                width: 1920,
                height: 1080,
                fov_degrees: 75.0,
                allowed_window_modes: vec!["fullscreen".into(), "borderless".into()],
                settings_runtime_evidence_sha256: format!("sha256:{}", "a".repeat(64)),
                runtime_fov_evidence_sha256: format!("sha256:{}", "a".repeat(64)),
            },
            source_a: fixed_source_a(),
            source_b_summary: PromotionSourceBSummary {
                sample_count: 3,
                hud_span_metres: 30.0,
                probe_chord_world_units: 30.0,
                probe_sample_path_world_units: 30.0,
                path_to_chord_ratio: 1.0,
                measured_metres_per_world_unit: 1.0,
            },
            cross_check: MetricCrossCheckEvidence {
                source_a: 1.0,
                source_b: 1.0,
                ratio: 1.0,
                tolerance: 0.05,
                passed: true,
            },
            adopted_metres_per_world_unit: 1.0,
            private_evidence: PrivateEvidenceManifest {
                immutable_private_storage_required: true,
                raw_material_in_git: false,
                inputs,
            },
            review: PromotionReview {
                method: "independent_human_transcription".into(),
                operator_reviewer_distinct: true,
                every_screenshot_reviewed: true,
                same_objective_confirmed: true,
            },
            decision: PromotionDecision {
                metric_gate: "passed".into(),
                local_only: true,
                non_trainable: true,
                production_certified: false,
                upload_allowed: false,
            },
        }
    }

    fn stable_source(role: &str, matrix_offset: u32) -> serde_json::Value {
        serde_json::json!({
            "descriptor_table": {
                "root_signature_sha256": format!("sha256:{}", "1".repeat(64)),
                "root_parameter_index": 7,
                "descriptor_range_index": 0,
                "descriptor_offset_in_range": 0,
                "descriptor_count": 1,
                "base_shader_register": 3,
                "register_space": 0
            },
            "cbv_resource_offset": 0,
            "declared_cbv_bytes": 256,
            "matrix_offset": matrix_offset,
            "encoding": "f32_little_endian",
            "layout": "row_major",
            "transpose": false,
            "role": role,
            "draw_relation": "exact_selected_depth_dsv_draw"
        })
    }

    fn current_render_camera(launch: &str) -> serde_json::Value {
        let observations = (0..MIN_RENDER_CAMERA_PRESENTS)
            .map(|present_seq| {
                serde_json::json!({
                    "present_seq": present_seq,
                    "copy_before_clear_submitted": true,
                    "selected_present_identity_passed": true,
                    "physical_source": "copy_buffer_region_root_signature_declared_cbv_at_selected_dsv_draw",
                    "matrices": [
                        {
                            "root_parameter_index": 7,
                            "cbv_resource_offset": 0,
                            "matrix_offset": 0,
                            "transpose": false,
                            "matrix_kind": "view",
                            "matrix": [
                                [1.0, 0.0, 0.0, 0.0],
                                [0.0, 1.0, 0.0, 0.0],
                                [0.0, 0.0, 1.0, 0.0],
                                [0.0, 0.0, 0.0, 1.0]
                            ],
                            "horizontal_fov_degrees": null,
                            "configured_vertical_fov_matches": null,
                            "physical_source": "copy_buffer_region_root_signature_declared_cbv",
                            "stable_source": stable_source("view", 0)
                        },
                        {
                            "root_parameter_index": 7,
                            "cbv_resource_offset": 0,
                            "matrix_offset": 64,
                            "transpose": false,
                            "matrix_kind": "projection",
                            "matrix": [
                                [1.0, 0.0, 0.0, 0.0],
                                [0.0, 1.0, 0.0, 0.0],
                                [0.0, 0.0, 0.0, -1.0],
                                [0.0, 0.0, 0.1, 0.0]
                            ],
                            "horizontal_fov_degrees": 75.6,
                            "configured_vertical_fov_matches": true,
                            "physical_source": "copy_buffer_region_root_signature_declared_cbv",
                            "stable_source": stable_source("projection", 64)
                        }
                    ]
                })
            })
            .collect::<Vec<_>>();
        serde_json::json!({
            "schema": RENDER_CAMERA_SCHEMA_V2,
            "process_launch_id": launch,
            "non_trainable": true,
            "production_certified": false,
            "deliverable": false,
            "recording_ready": false,
            "observation": {
                "selected_depth_armed": true,
                "observations": observations,
                "blockers": [],
                "authoring_blockers": [],
                "candidate_admitted_for_cross_launch_discovery": true,
                "production_camera_binding_emitted": false,
                "recording_ready": false
            }
        })
    }

    fn legacy_render_camera(launch: &str) -> serde_json::Value {
        serde_json::json!({
            "schema": RENDER_CAMERA_SCHEMA_V1,
            "process_launch_id": launch,
            "non_trainable": true,
            "production_certified": false,
            "deliverable": false,
            "recording_ready": false,
            "observation": {
                "selected_depth_armed": true,
                "blockers": [],
                "candidate_admitted_for_cross_launch_discovery": true,
                "production_camera_binding_emitted": false,
                "recording_ready": false
            }
        })
    }

    fn validate_render_json(
        promotion: &Cp2077MetricPromotionEvidence,
        value: &serde_json::Value,
        digest: &str,
    ) -> Result<String, String> {
        validate_render_camera_document(promotion, &serde_json::to_vec(value).unwrap(), digest)
    }

    #[test]
    fn sanitized_promotion_contract_passes() {
        assert!(validate_promotion(&valid()).is_ok());
    }

    #[test]
    fn committed_promotion_record_passes_the_same_validator_as_the_cli() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../docs/evidence/issue-814/cp2077-metric-scale-promotion-20383525.json");
        let validation = verify_metric_promotion(&path).unwrap();
        assert_eq!(validation.steam_build_id, STEAM_BUILD_ID);
        assert_eq!(validation.adopted_metres_per_world_unit, 1.0);
        assert!(!validation.private_inputs_verified);
    }

    #[test]
    fn unknown_or_sensitive_fields_are_not_deserializable() {
        let mut value = serde_json::to_value(valid()).unwrap();
        value["session_id"] = serde_json::json!("private-session");
        assert!(serde_json::from_value::<Cp2077MetricPromotionEvidence>(value).is_err());
    }

    #[test]
    fn identity_source_and_settings_drift_fail_closed() {
        let mut promotion = valid();
        promotion.game.executable_sha256 = format!("sha256:{}", "c".repeat(64));
        assert!(validate_promotion(&promotion).is_err());

        let mut promotion = valid();
        promotion.game.probe_sha256 = format!("sha256:{}", "c".repeat(64));
        assert!(validate_promotion(&promotion).is_err());

        let mut promotion = valid();
        promotion.source_a.red4ext_world_position.commit = "changed".into();
        assert!(validate_promotion(&promotion).is_err());

        let mut promotion = valid();
        promotion.controlled_settings.fov_degrees = 80.0;
        assert!(validate_promotion(&promotion).is_err());
    }

    #[test]
    fn weak_metric_or_rights_claims_fail_closed() {
        let mut promotion = valid();
        promotion.cross_check.passed = false;
        assert!(validate_promotion(&promotion).is_err());

        let mut promotion = valid();
        promotion.source_b_summary.path_to_chord_ratio = 1.03;
        assert!(validate_promotion(&promotion).is_err());

        let mut promotion = valid();
        promotion.source_b_summary.probe_chord_world_units = 29.0;
        assert!(validate_promotion(&promotion).is_err());

        let mut promotion = valid();
        promotion.decision.upload_allowed = true;
        assert!(validate_promotion(&promotion).is_err());
    }

    #[test]
    fn anonymous_manifest_is_exact_and_content_addressed() {
        let mut promotion = valid();
        promotion.private_evidence.inputs.pop();
        assert!(validate_promotion(&promotion).is_err());

        let mut promotion = valid();
        promotion.private_evidence.inputs[0].sha256 = "sha256:not-a-digest".into();
        assert!(validate_promotion(&promotion).is_err());

        let mut promotion = valid();
        promotion
            .private_evidence
            .inputs
            .push(digest("metric_pose_rows"));
        assert!(validate_promotion(&promotion).is_err());
    }

    #[test]
    fn both_approved_window_modes_are_required_without_device_binding() {
        let mut promotion = valid();
        promotion.controlled_settings.allowed_window_modes = vec!["fullscreen".into()];
        assert!(validate_promotion(&promotion).is_err());
        assert!(
            serde_json::to_string(&valid())
                .unwrap()
                .find("luid")
                .is_none()
        );
    }

    #[test]
    fn exact_promoted_render_camera_v1_remains_compatible_without_downgrade_bypass() {
        let launch = "0123456789abcdef0123456789abcdef";
        let document = legacy_render_camera(launch);
        assert_eq!(
            validate_render_json(&valid(), &document, PROMOTED_LEGACY_RENDER_CAMERA_SHA256)
                .unwrap(),
            launch
        );
        assert!(
            validate_render_json(&valid(), &document, &format!("sha256:{}", "f".repeat(64)))
                .is_err()
        );
    }

    #[test]
    fn current_render_camera_v2_proves_stable_source_and_actual_fov() {
        let launch = "0123456789abcdef0123456789abcdef";
        assert_eq!(
            validate_render_json(&valid(), &current_render_camera(launch), "sha256:v2").unwrap(),
            launch
        );
    }

    #[test]
    fn render_camera_schema_and_launch_mismatch_fail_closed() {
        let launch = "0123456789abcdef0123456789abcdef";
        let mut document = current_render_camera(launch);
        document["schema"] = serde_json::json!("dm-cp2077-render-camera-observation/v3");
        assert!(validate_render_json(&valid(), &document, "sha256:v3").is_err());
        assert!(
            validate_launch_coherence(launch, "fedcba9876543210fedcba9876543210", launch).is_err()
        );
    }

    #[test]
    fn current_render_camera_rejects_fov_or_admission_blockers() {
        let launch = "0123456789abcdef0123456789abcdef";
        let mut fov = current_render_camera(launch);
        for draw in fov["observation"]["observations"].as_array_mut().unwrap() {
            draw["matrices"][1]["horizontal_fov_degrees"] = serde_json::json!(70.0);
        }
        assert!(validate_render_json(&valid(), &fov, "sha256:v2").is_err());

        let mut blocked = current_render_camera(launch);
        blocked["observation"]["authoring_blockers"] = serde_json::json!(["unstable projection"]);
        assert!(validate_render_json(&valid(), &blocked, "sha256:v2").is_err());

        let mut unarmed = current_render_camera(launch);
        unarmed["observation"]["selected_depth_armed"] = serde_json::json!(false);
        assert!(validate_render_json(&valid(), &unarmed, "sha256:v2").is_err());

        let mut unadmitted = current_render_camera(launch);
        unadmitted["observation"]["candidate_admitted_for_cross_launch_discovery"] =
            serde_json::json!(false);
        assert!(validate_render_json(&valid(), &unadmitted, "sha256:v2").is_err());
    }

    #[test]
    fn current_render_camera_rejects_projection_source_mismatch() {
        let launch = "0123456789abcdef0123456789abcdef";
        let mut document = current_render_camera(launch);
        for draw in document["observation"]["observations"]
            .as_array_mut()
            .unwrap()
        {
            draw["matrices"][1]["stable_source"]["descriptor_table"]["root_signature_sha256"] =
                format!("sha256:{}", "2".repeat(64)).into();
        }
        assert!(validate_render_json(&valid(), &document, "sha256:v2").is_err());
    }
}

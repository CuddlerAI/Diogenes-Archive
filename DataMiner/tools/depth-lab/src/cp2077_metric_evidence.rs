//! CP2077 metric-scale evidence authoring and verification.
//!
//! This is deliberately an offline, local-only gate.  Source A pins the exact RED4ext
//! `WorldPosition` conversion and a published CET distance meter which labels direct
//! `GetWorldPosition` deltas as metres.  Source B is a measured, straight radial movement where
//! the in-game objective HUD distance and the exact same probe player poses agree.  A naked
//! `metric_scale = 1` or a screenshot without an exact Present/QPC join is never sufficient.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

pub const CP2077_HUD_METRIC_OBSERVATIONS_SCHEMA_V2: &str =
    "dm-cp2077-hud-objective-metric-observations/v2";
pub const CP2077_METRIC_EVIDENCE_SCHEMA_V1: &str = "dm-cp2077-metric-evidence/v1";
pub const CP2077_METRIC_POSE_SCHEMA_V1: &str = "dm-cp2077-metric-pose/v1";
pub const CP2077_METRIC_FRAME_SCHEMA_V1: &str = "dm-cp2077-metric-frame-identity/v1";

const MIN_SAMPLES: usize = 3;
const MAX_SAMPLES: usize = 32;
const MIN_HUD_SPAN_METRES: f64 = 20.0;
const MAX_PATH_TO_CHORD_RATIO: f64 = 1.02;
const CROSS_CHECK_TOLERANCE: f64 = 0.05;
const MAX_SMALL_FILE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_SCREENSHOT_BYTES: u64 = 32 * 1024 * 1024;

const RED4EXT_REPOSITORY: &str = "https://github.com/WopsS/RED4ext.SDK";
const RED4EXT_COMMIT: &str = "ad7277714ad30d6885d7050c5ba24fa0102f6920";
const RED4EXT_PATH: &str = "include/RED4ext/Scripting/Natives/WorldPosition.hpp";
const RED4EXT_GIT_BLOB_SHA1: &str = "ddc2eb166a9b30b9f76f56354cf56c6d5d93320e";
const CET_REPOSITORY: &str =
    "https://github.com/martine-dee/cp2077-traveling-companion-distance-meter";
const CET_COMMIT: &str = "ed6d40fb2facf2f529bb30a1ba707ccb1ed08e04";
const CET_PATH: &str =
    "bin/x64/plugins/cyber_engine_tweaks/mods/TravelingCompanionDistanceMeter/init.lua";
const CET_GIT_BLOB_SHA1: &str = "e1703719bf5e37917063435eff6b2e6dce1cb119";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HudMetricObservations {
    pub schema: String,
    pub non_trainable: bool,
    pub local_only: bool,
    pub steam_build_id: u64,
    pub executable_sha256: String,
    pub calibration_session_id: String,
    pub calibration_run_id: String,
    pub capture_operator_id: String,
    pub hud_visible_for_calibration_only: bool,
    pub objective_label: String,
    pub motion_protocol: String,
    pub samples: Vec<HudMetricObservation>,
    pub review: HudMetricReview,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HudMetricReview {
    pub method: String,
    pub reviewer_id: String,
    pub reviewed_utc: String,
    pub every_screenshot_reviewed: bool,
    pub same_objective_confirmed: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HudMetricObservation {
    pub session_frame: u64,
    pub present_seq: u64,
    pub qpc: i64,
    pub screenshot_file: String,
    pub ocr_text: String,
    pub objective_distance_metres: u64,
}

/// Minimal, pre-scale player fact produced by the dedicated calibration run.
/// It intentionally is not a formal action-camera row: the metric scale is the
/// thing this run is measuring, so claiming metric camera coordinates here
/// would be circular.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricPoseRow {
    pub schema: String,
    pub session_frame: u64,
    pub present_seq: u64,
    pub qpc: i64,
    pub player_position_world_units: [f64; 3],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricPresentIdentity {
    pub present_seq: u64,
    pub qpc: i64,
}

/// Exact RGB/player join for one F9 sample. Camera/depth/package claims do not
/// belong in the metric-calibration input and therefore are absent by design.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricFrameRow {
    pub schema: String,
    pub session_frame: u64,
    pub rgb: MetricPresentIdentity,
    pub player: MetricPresentIdentity,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceFile {
    pub file: String,
    pub bytes: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpstreamMetricSource {
    pub repository: String,
    pub commit: String,
    pub path: String,
    pub git_blob_sha1: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricSourceA {
    pub kind: String,
    pub red4ext_world_position: UpstreamMetricSource,
    pub fixed_point_divisor: u64,
    pub world_position_bytes: u64,
    pub cet_distance_meter: UpstreamMetricSource,
    pub cet_algorithm: String,
    pub cet_output_unit: String,
    pub inferred_metres_per_converted_world_unit: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricSampleEvidence {
    pub session_frame: u64,
    pub present_seq: u64,
    pub qpc: i64,
    pub player_position: [f64; 3],
    pub screenshot: EvidenceFile,
    pub ocr_text: String,
    pub objective_distance_metres: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricSourceB {
    pub kind: String,
    pub objective_label: String,
    pub motion_protocol: String,
    pub samples: Vec<MetricSampleEvidence>,
    pub probe_chord_world_units: f64,
    pub probe_sample_path_world_units: f64,
    pub path_to_chord_ratio: f64,
    pub hud_span_metres: f64,
    pub measured_metres_per_world_unit: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricCrossCheckEvidence {
    pub source_a: f64,
    pub source_b: f64,
    pub ratio: f64,
    pub tolerance: f64,
    pub passed: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cp2077MetricEvidence {
    pub schema: String,
    pub non_trainable: bool,
    pub local_only: bool,
    pub upload_allowed: bool,
    pub steam_build_id: u64,
    pub executable_sha256: String,
    pub calibration_session_id: String,
    pub calibration_run_id: String,
    pub hud_visible_for_calibration_only: bool,
    pub pose: EvidenceFile,
    pub frame_evidence: EvidenceFile,
    pub hud_observations: EvidenceFile,
    pub source_a: MetricSourceA,
    pub source_b: MetricSourceB,
    pub cross_check: MetricCrossCheckEvidence,
    pub adopted_metres_per_world_unit: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MetricEvidenceValidation {
    pub adopted_scale: f64,
    pub source_a_scale: f64,
    pub source_b_scale: f64,
    pub tolerance: f64,
    pub referenced_sha256: BTreeMap<String, String>,
}

/// Generate an evidence report from immutable local inputs, then revalidate it before publishing.
pub fn generate_cp2077_metric_evidence(
    session_dir: &Path,
    pose_path: &Path,
    frame_evidence_path: &Path,
    observations_path: &Path,
    output_path: &Path,
) -> Result<Cp2077MetricEvidence, String> {
    let session = canonical_session(session_dir)?;
    if !output_path.is_absolute() {
        return Err("metric evidence output must be an absolute path".into());
    }
    let output_name = output_path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or("metric evidence output has no UTF-8 file name")?;
    validate_leaf_name(output_name)?;
    let output_parent = output_path
        .parent()
        .ok_or("metric evidence output has no parent")?
        .canonicalize()
        .map_err(|error| format!("canonicalize metric evidence output parent failed: {error}"))?;
    if output_parent != session {
        return Err("metric evidence output must be a direct child of the session".into());
    }
    let output_path = session.join(output_name);
    if output_path.exists() {
        return Err("metric evidence output must be a new file".into());
    }
    let pose = require_exact_input(&session, pose_path, MAX_SMALL_FILE_BYTES)?;
    let frames = require_exact_input(&session, frame_evidence_path, MAX_SMALL_FILE_BYTES)?;
    let observations = require_exact_input(&session, observations_path, MAX_SMALL_FILE_BYTES)?;
    let report = derive_evidence(&session, &pose, &frames, &observations)?;
    atomic_write_json(&output_path, &report)?;
    if let Err(error) = validate_cp2077_metric_evidence(&session, &output_path) {
        let _ = fs::remove_file(&output_path);
        return Err(format!(
            "generated metric evidence did not revalidate: {error}"
        ));
    }
    Ok(report)
}

/// Validate the report against its referenced pose, frame, observation and screenshot bytes.
pub fn validate_cp2077_metric_evidence(
    session_dir: &Path,
    evidence_path: &Path,
) -> Result<MetricEvidenceValidation, String> {
    let session = canonical_session(session_dir)?;
    let evidence_path = require_exact_input(&session, evidence_path, MAX_SMALL_FILE_BYTES)?;
    let observed: Cp2077MetricEvidence =
        serde_json::from_slice(&read_bounded(&evidence_path, MAX_SMALL_FILE_BYTES)?)
            .map_err(|error| format!("parse CP2077 metric evidence failed: {error}"))?;
    validate_report_envelope(&observed)?;
    let pose = resolve_leaf(&session, &observed.pose.file, MAX_SMALL_FILE_BYTES)?;
    let frames = resolve_leaf(
        &session,
        &observed.frame_evidence.file,
        MAX_SMALL_FILE_BYTES,
    )?;
    let observations = resolve_leaf(
        &session,
        &observed.hud_observations.file,
        MAX_SMALL_FILE_BYTES,
    )?;
    check_file_digest(&pose, &observed.pose)?;
    check_file_digest(&frames, &observed.frame_evidence)?;
    check_file_digest(&observations, &observed.hud_observations)?;
    for sample in &observed.source_b.samples {
        let screenshot = resolve_leaf(
            session.as_path(),
            &sample.screenshot.file,
            MAX_SCREENSHOT_BYTES,
        )?;
        check_file_digest(&screenshot, &sample.screenshot)?;
    }
    let expected = derive_evidence(&session, &pose, &frames, &observations)?;
    // JSON is the immutable evidence representation. Normalize the recomputed report through the
    // same serde_json float encoding before exact comparison so non-trivial real-world positions
    // are compared as persisted bytes would be read, rather than against pre-serialization f64s.
    let expected: Cp2077MetricEvidence = serde_json::from_slice(
        &serde_json::to_vec(&expected)
            .map_err(|error| format!("serialize recomputed metric evidence failed: {error}"))?,
    )
    .map_err(|error| format!("normalize recomputed metric evidence failed: {error}"))?;
    if observed != expected {
        let observed_value = serde_json::to_value(&observed)
            .map_err(|error| format!("serialize observed metric evidence failed: {error}"))?;
        let expected_value = serde_json::to_value(&expected)
            .map_err(|error| format!("serialize expected metric evidence failed: {error}"))?;
        let differing_fields = observed_value
            .as_object()
            .into_iter()
            .flatten()
            .filter_map(|(key, value)| {
                (expected_value.get(key) != Some(value)).then_some(key.as_str())
            })
            .collect::<Vec<_>>();
        return Err(format!(
            "metric evidence content does not equal recomputation from bound inputs; differing top-level fields: {}",
            differing_fields.join(", ")
        ));
    }
    let mut referenced_sha256 = BTreeMap::from([
        ("pose".into(), observed.pose.sha256.clone()),
        (
            "frame_evidence".into(),
            observed.frame_evidence.sha256.clone(),
        ),
        (
            "hud_observations".into(),
            observed.hud_observations.sha256.clone(),
        ),
    ]);
    for (index, sample) in observed.source_b.samples.iter().enumerate() {
        referenced_sha256.insert(
            format!("hud_screenshot_{index}"),
            sample.screenshot.sha256.clone(),
        );
    }
    Ok(MetricEvidenceValidation {
        adopted_scale: observed.adopted_metres_per_world_unit,
        source_a_scale: observed.cross_check.source_a,
        source_b_scale: observed.cross_check.source_b,
        tolerance: observed.cross_check.tolerance,
        referenced_sha256,
    })
}

fn derive_evidence(
    session: &Path,
    pose_path: &Path,
    frame_evidence_path: &Path,
    observations_path: &Path,
) -> Result<Cp2077MetricEvidence, String> {
    let observations: HudMetricObservations =
        serde_json::from_slice(&read_bounded(observations_path, MAX_SMALL_FILE_BYTES)?)
            .map_err(|error| format!("parse HUD metric observations failed: {error}"))?;
    validate_observations_envelope(&observations)?;
    let poses: Vec<MetricPoseRow> = read_jsonl(pose_path, 4_096)?;
    let frames: Vec<MetricFrameRow> = read_jsonl(frame_evidence_path, 4_096)?;
    let mut poses_by_frame = BTreeMap::new();
    for pose in poses {
        if pose.schema != CP2077_METRIC_POSE_SCHEMA_V1
            || pose.present_seq == 0
            || pose.qpc <= 0
            || !pose
                .player_position_world_units
                .iter()
                .all(|value| value.is_finite())
        {
            return Err("metric pose input contains an invalid pre-scale player row".into());
        }
        if poses_by_frame.insert(pose.session_frame, pose).is_some() {
            return Err("metric pose input repeats session_frame".into());
        }
    }
    let mut frames_by_identity = BTreeMap::new();
    for frame in frames {
        if frame.schema != CP2077_METRIC_FRAME_SCHEMA_V1 || frame.rgb != frame.player {
            return Err("metric frame evidence is not an exact RGB/player identity row".into());
        }
        if frames_by_identity
            .insert((frame.rgb.present_seq, frame.rgb.qpc), frame)
            .is_some()
        {
            return Err("metric frame evidence repeats Present/QPC identity".into());
        }
    }

    let mut samples = Vec::with_capacity(observations.samples.len());
    let mut screenshots = BTreeSet::new();
    let mut previous_identity = None;
    for observation in &observations.samples {
        if observation.present_seq == 0 || observation.qpc <= 0 {
            return Err("HUD observation has an invalid Present/QPC identity".into());
        }
        if previous_identity
            .is_some_and(|(seq, qpc)| observation.present_seq <= seq || observation.qpc <= qpc)
        {
            return Err("HUD observations must be strictly increasing by Present and QPC".into());
        }
        previous_identity = Some((observation.present_seq, observation.qpc));
        validate_leaf_name(&observation.screenshot_file)?;
        if !screenshots.insert(observation.screenshot_file.clone()) {
            return Err("HUD observations reuse a screenshot".into());
        }
        validate_ocr(observation)?;
        let frame = frames_by_identity
            .get(&(observation.present_seq, observation.qpc))
            .ok_or("HUD observation has no exact frame-evidence Present/QPC row")?;
        if frame.session_frame != observation.session_frame {
            return Err(
                "HUD observation session_frame differs from its exact Present/QPC row".into(),
            );
        }
        let pose = poses_by_frame
            .get(&frame.session_frame)
            .ok_or("HUD observation has no exact probe player pose")?;
        if pose.session_frame != frame.session_frame
            || pose.present_seq != observation.present_seq
            || pose.qpc != observation.qpc
        {
            return Err("HUD observation pose does not match the exact session frame".into());
        }
        let screenshot_path =
            resolve_leaf(session, &observation.screenshot_file, MAX_SCREENSHOT_BYTES)?;
        let magic = read_bounded(&screenshot_path, MAX_SCREENSHOT_BYTES)?;
        if magic.len() < 24
            || !magic.starts_with(b"\x89PNG\r\n\x1a\n")
            || u32::from_be_bytes(magic[16..20].try_into().unwrap()) != 1920
            || u32::from_be_bytes(magic[20..24].try_into().unwrap()) != 1080
        {
            return Err("HUD metric screenshot must be a full-frame 1920x1080 PNG".into());
        }
        samples.push(MetricSampleEvidence {
            session_frame: frame.session_frame,
            present_seq: observation.present_seq,
            qpc: observation.qpc,
            player_position: pose.player_position_world_units,
            screenshot: digest_file(&screenshot_path)?,
            ocr_text: observation.ocr_text.clone(),
            objective_distance_metres: observation.objective_distance_metres,
        });
    }

    let positions = samples
        .iter()
        .map(|sample| sample.player_position)
        .collect::<Vec<_>>();
    let chord = distance(positions.first().unwrap(), positions.last().unwrap());
    let path = positions
        .windows(2)
        .map(|pair| distance(&pair[0], &pair[1]))
        .sum::<f64>();
    if !chord.is_finite() || chord < MIN_HUD_SPAN_METRES {
        return Err(format!(
            "probe calibration chord must be at least {MIN_HUD_SPAN_METRES} converted world units"
        ));
    }
    let path_to_chord = path / chord;
    if !path_to_chord.is_finite() || path_to_chord > MAX_PATH_TO_CHORD_RATIO {
        return Err("HUD calibration motion is not a sufficiently straight radial segment".into());
    }
    let hud_direction = monotonic_hud_direction(&samples)?;
    let first_hud = samples.first().unwrap().objective_distance_metres as f64;
    let last_hud = samples.last().unwrap().objective_distance_metres as f64;
    let hud_span = (last_hud - first_hud).abs();
    if hud_span < MIN_HUD_SPAN_METRES {
        return Err(format!(
            "objective HUD span must be at least {MIN_HUD_SPAN_METRES} metres"
        ));
    }
    if hud_direction == 0 {
        return Err("objective HUD samples are not strictly monotonic".into());
    }
    let measured = hud_span / chord;
    let source_a = 1.0;
    let ratio = measured / source_a;
    let passed = measured.is_finite() && (ratio - 1.0_f64).abs() <= CROSS_CHECK_TOLERANCE;
    if !passed {
        return Err(format!(
            "HUD/probe metric cross-check ratio {ratio:.6} exceeds ±{CROSS_CHECK_TOLERANCE:.3}"
        ));
    }

    Ok(Cp2077MetricEvidence {
        schema: CP2077_METRIC_EVIDENCE_SCHEMA_V1.into(),
        non_trainable: true,
        local_only: true,
        upload_allowed: false,
        steam_build_id: observations.steam_build_id,
        executable_sha256: observations.executable_sha256,
        calibration_session_id: observations.calibration_session_id,
        calibration_run_id: observations.calibration_run_id,
        hud_visible_for_calibration_only: observations.hud_visible_for_calibration_only,
        pose: digest_file(pose_path)?,
        frame_evidence: digest_file(frame_evidence_path)?,
        hud_observations: digest_file(observations_path)?,
        source_a: fixed_source_a(),
        source_b: MetricSourceB {
            kind: "in_game_hud_objective_radial_cross_check".into(),
            objective_label: observations.objective_label,
            motion_protocol: observations.motion_protocol,
            samples,
            probe_chord_world_units: chord,
            probe_sample_path_world_units: path,
            path_to_chord_ratio: path_to_chord,
            hud_span_metres: hud_span,
            measured_metres_per_world_unit: measured,
        },
        cross_check: MetricCrossCheckEvidence {
            source_a,
            source_b: measured,
            ratio,
            tolerance: CROSS_CHECK_TOLERANCE,
            passed,
        },
        adopted_metres_per_world_unit: source_a,
    })
}

fn fixed_source_a() -> MetricSourceA {
    MetricSourceA {
        kind: "red4ext_worldposition_plus_cet_distance_meter".into(),
        red4ext_world_position: UpstreamMetricSource {
            repository: RED4EXT_REPOSITORY.into(),
            commit: RED4EXT_COMMIT.into(),
            path: RED4EXT_PATH.into(),
            git_blob_sha1: RED4EXT_GIT_BLOB_SHA1.into(),
        },
        fixed_point_divisor: 131_072,
        world_position_bytes: 12,
        cet_distance_meter: UpstreamMetricSource {
            repository: CET_REPOSITORY.into(),
            commit: CET_COMMIT.into(),
            path: CET_PATH.into(),
            git_blob_sha1: CET_GIT_BLOB_SHA1.into(),
        },
        cet_algorithm: "euclidean_delta_of_consecutive_Game.GetPlayer_GetWorldPosition".into(),
        cet_output_unit: "metres".into(),
        inferred_metres_per_converted_world_unit: 1.0,
    }
}

fn validate_report_envelope(report: &Cp2077MetricEvidence) -> Result<(), String> {
    if report.schema != CP2077_METRIC_EVIDENCE_SCHEMA_V1
        || !report.non_trainable
        || !report.local_only
        || report.upload_allowed
        || report.source_a != fixed_source_a()
        || !report.cross_check.passed
        || report.adopted_metres_per_world_unit != 1.0
        || report.calibration_session_id.trim().is_empty()
        || report.calibration_run_id.trim().is_empty()
        || !report.hud_visible_for_calibration_only
    {
        return Err("CP2077 metric evidence envelope or pinned source A is invalid".into());
    }
    Ok(())
}

fn validate_observations_envelope(observations: &HudMetricObservations) -> Result<(), String> {
    if observations.schema != CP2077_HUD_METRIC_OBSERVATIONS_SCHEMA_V2
        || !observations.non_trainable
        || !observations.local_only
        || observations.steam_build_id != 20_383_525
        || observations.executable_sha256 != crate::raw_depth_demo::CP2077_BUILD_20383525_EXE_SHA256
        || observations.calibration_session_id.trim().is_empty()
        || observations.calibration_run_id.trim().is_empty()
        || !valid_reviewer_identity(&observations.capture_operator_id)
        || observations.review.method != "independent_human_transcription"
        || !valid_reviewer_identity(&observations.review.reviewer_id)
        || observations.review.reviewer_id == observations.capture_operator_id
        || observations.review.reviewed_utc.trim().is_empty()
        || !observations.review.every_screenshot_reviewed
        || !observations.review.same_objective_confirmed
        || !observations.hud_visible_for_calibration_only
        || observations.objective_label.trim().is_empty()
        || observations.motion_protocol != "straight_radial_same_objective"
        || !(MIN_SAMPLES..=MAX_SAMPLES).contains(&observations.samples.len())
    {
        return Err("HUD observations do not satisfy the local straight-radial protocol".into());
    }
    Ok(())
}

fn valid_reviewer_identity(value: &str) -> bool {
    let normalized = value.trim().to_ascii_lowercase();
    !normalized.is_empty()
        && !normalized.contains("replace")
        && normalized != "unknown"
        && normalized.len() <= 128
}

fn validate_ocr(observation: &HudMetricObservation) -> Result<(), String> {
    if observation.objective_distance_metres == 0 {
        return Err("HUD objective distance must be a positive whole-metre reading".into());
    }
    let normalized = observation
        .ocr_text
        .chars()
        .filter(|character| !character.is_whitespace())
        .flat_map(char::to_uppercase)
        .collect::<String>();
    let expected = format!("{}M", observation.objective_distance_metres);
    if normalized.contains("KM") || !normalized.contains(&expected) {
        return Err("OCR transcript does not contain the declared whole-metre HUD reading".into());
    }
    Ok(())
}

fn monotonic_hud_direction(samples: &[MetricSampleEvidence]) -> Result<i8, String> {
    let mut direction = 0;
    for pair in samples.windows(2) {
        let left = pair[0].objective_distance_metres;
        let right = pair[1].objective_distance_metres;
        let current = right.cmp(&left);
        let signed = match current {
            std::cmp::Ordering::Less => -1,
            std::cmp::Ordering::Equal => 0,
            std::cmp::Ordering::Greater => 1,
        };
        if signed == 0 || (direction != 0 && direction != signed) {
            return Err("objective HUD readings must be strictly monotonic".into());
        }
        direction = signed;
    }
    Ok(direction)
}

fn distance(left: &[f64; 3], right: &[f64; 3]) -> f64 {
    left.iter()
        .zip(right)
        .map(|(a, b)| (b - a).powi(2))
        .sum::<f64>()
        .sqrt()
}

fn canonical_session(path: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err("metric session path must be absolute".into());
    }
    let path = path
        .canonicalize()
        .map_err(|error| format!("canonicalize metric session failed: {error}"))?;
    if !path.is_dir() {
        return Err("metric session is not a directory".into());
    }
    Ok(path)
}

fn require_exact_input(session: &Path, path: &Path, max: u64) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err("metric input paths must be absolute".into());
    }
    let canonical = path
        .canonicalize()
        .map_err(|error| format!("canonicalize metric input failed: {error}"))?;
    if canonical.parent() != Some(session) {
        return Err("metric input must be a direct child of the session".into());
    }
    require_regular_file(&canonical, max)?;
    Ok(canonical)
}

fn resolve_leaf(session: &Path, name: &str, max: u64) -> Result<PathBuf, String> {
    validate_leaf_name(name)?;
    let path = session.join(name);
    require_regular_file(&path, max)?;
    let canonical = path
        .canonicalize()
        .map_err(|error| format!("canonicalize metric evidence input failed: {error}"))?;
    if canonical.parent() != Some(session) {
        return Err("metric evidence input escapes the session".into());
    }
    Ok(canonical)
}

fn validate_leaf_name(name: &str) -> Result<(), String> {
    let path = Path::new(name);
    if name.is_empty()
        || path.is_absolute()
        || path.file_name().and_then(|value| value.to_str()) != Some(name)
    {
        return Err("metric evidence paths must be plain relative leaf names".into());
    }
    Ok(())
}

fn require_regular_file(path: &Path, max: u64) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("read metric input metadata failed: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > max {
        return Err("metric evidence input is not a bounded regular non-symlink file".into());
    }
    Ok(())
}

fn read_bounded(path: &Path, max: u64) -> Result<Vec<u8>, String> {
    require_regular_file(path, max)?;
    let mut bytes = Vec::new();
    File::open(path)
        .and_then(|mut file| file.read_to_end(&mut bytes))
        .map_err(|error| format!("read metric input failed: {error}"))?;
    Ok(bytes)
}

fn read_jsonl<T: for<'de> Deserialize<'de>>(
    path: &Path,
    max_rows: usize,
) -> Result<Vec<T>, String> {
    let bytes = read_bounded(path, MAX_SMALL_FILE_BYTES)?;
    let text = std::str::from_utf8(&bytes).map_err(|_| "metric JSONL is not UTF-8")?;
    let mut rows = Vec::new();
    for (line_no, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        if rows.len() == max_rows {
            return Err("metric JSONL exceeds the row bound".into());
        }
        rows.push(
            serde_json::from_str(line).map_err(|error| {
                format!("parse metric JSONL line {} failed: {error}", line_no + 1)
            })?,
        );
    }
    if rows.is_empty() {
        return Err("metric JSONL is empty".into());
    }
    Ok(rows)
}

fn digest_file(path: &Path) -> Result<EvidenceFile, String> {
    let mut file =
        File::open(path).map_err(|error| format!("open metric hash input failed: {error}"))?;
    let bytes = file
        .metadata()
        .map_err(|error| format!("read metric hash metadata failed: {error}"))?
        .len();
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("read metric hash input failed: {error}"))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(EvidenceFile {
        file: path
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or("metric evidence input has no UTF-8 leaf name")?
            .into(),
        bytes,
        sha256: format!("sha256:{:x}", hasher.finalize()),
    })
}

fn check_file_digest(path: &Path, expected: &EvidenceFile) -> Result<(), String> {
    if digest_file(path)? != *expected {
        return Err(format!(
            "metric evidence hash mismatch for {}",
            expected.file
        ));
    }
    Ok(())
}

fn atomic_write_json(path: &Path, value: &Cp2077MetricEvidence) -> Result<(), String> {
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or("metric evidence output has no UTF-8 filename")?;
    let staging = path.with_file_name(format!(".{name}.staging-{}", std::process::id()));
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|error| format!("serialize metric evidence failed: {error}"))?;
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staging)
            .map_err(|error| format!("create metric staging file failed: {error}"))?;
        file.write_all(&bytes)
            .and_then(|()| file.write_all(b"\n"))
            .and_then(|()| file.sync_all())
            .map_err(|error| format!("seal metric staging file failed: {error}"))?;
        if path.exists() {
            return Err("metric evidence output appeared before publish".into());
        }
        fs::rename(&staging, path)
            .map_err(|error| format!("publish metric evidence failed: {error}"))
    })();
    if result.is_err() {
        let _ = fs::remove_file(staging);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_fixture(
        root: &Path,
        hud: [u64; 3],
        positions: [[f64; 3]; 3],
    ) -> (PathBuf, PathBuf, PathBuf) {
        let pose_path = root.join("metric-pose.jsonl");
        let frame_path = root.join("metric-frames.jsonl");
        let observation_path = root.join("hud-observations.json");
        let mut pose_file = File::create(&pose_path).unwrap();
        let mut frame_file = File::create(&frame_path).unwrap();
        let mut samples = Vec::new();
        for index in 0..3_u64 {
            let identity = MetricPresentIdentity {
                present_seq: 100 + index,
                qpc: 1_000 + index as i64,
            };
            let pose = MetricPoseRow {
                schema: CP2077_METRIC_POSE_SCHEMA_V1.into(),
                session_frame: index,
                present_seq: identity.present_seq,
                qpc: identity.qpc,
                player_position_world_units: positions[index as usize],
            };
            serde_json::to_writer(&mut pose_file, &pose).unwrap();
            writeln!(pose_file).unwrap();
            let frame = serde_json::json!({
                "schema": CP2077_METRIC_FRAME_SCHEMA_V1,
                "session_frame": index,
                "rgb": {"present_seq": identity.present_seq, "qpc": identity.qpc},
                "player": {"present_seq": identity.present_seq, "qpc": identity.qpc}
            });
            writeln!(frame_file, "{}", frame).unwrap();
            let screenshot = format!("hud-{index}.png");
            let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
            png.extend_from_slice(&1920_u32.to_be_bytes());
            png.extend_from_slice(&1080_u32.to_be_bytes());
            fs::write(root.join(&screenshot), png).unwrap();
            samples.push(serde_json::json!({
                "session_frame": index,
                "present_seq": identity.present_seq,
                "qpc": identity.qpc,
                "screenshot_file": screenshot,
                "ocr_text": format!("OBJECTIVE {} M", hud[index as usize]),
                "objective_distance_metres": hud[index as usize]
            }));
        }
        fs::write(
            &observation_path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "schema": CP2077_HUD_METRIC_OBSERVATIONS_SCHEMA_V2,
                "non_trainable": true,
                "local_only": true,
                "steam_build_id": 20383525,
                "executable_sha256": crate::raw_depth_demo::CP2077_BUILD_20383525_EXE_SHA256,
                "calibration_session_id": "metric-calibration-session",
                "calibration_run_id": "hud-visible-straight-radial-run",
                "capture_operator_id": "operator-a",
                "hud_visible_for_calibration_only": true,
                "objective_label": "calibration objective",
                "motion_protocol": "straight_radial_same_objective",
                "samples": samples,
                "review": {
                    "method": "independent_human_transcription",
                    "reviewer_id": "reviewer-b",
                    "reviewed_utc": "2026-08-18T00:00:00Z",
                    "every_screenshot_reviewed": true,
                    "same_objective_confirmed": true
                }
            }))
            .unwrap(),
        )
        .unwrap();
        (pose_path, frame_path, observation_path)
    }

    #[test]
    fn synthetic_hud_and_probe_segment_generates_and_revalidates() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let (pose, frames, observations) = write_fixture(
            &root,
            [150, 135, 120],
            [[0.0, 0.0, 0.0], [15.0, 0.0, 0.0], [30.0, 0.0, 0.0]],
        );
        let out = root.join("metric-evidence.json");
        let report =
            generate_cp2077_metric_evidence(&root, &pose, &frames, &observations, &out).unwrap();
        assert_eq!(report.adopted_metres_per_world_unit, 1.0);
        assert_eq!(report.source_b.measured_metres_per_world_unit, 1.0);
        assert!(validate_cp2077_metric_evidence(&root, &out).is_ok());
    }

    #[test]
    fn stale_screenshot_hash_is_rejected() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let (pose, frames, observations) = write_fixture(
            &root,
            [150, 135, 120],
            [[0.0, 0.0, 0.0], [15.0, 0.0, 0.0], [30.0, 0.0, 0.0]],
        );
        let out = root.join("metric-evidence.json");
        generate_cp2077_metric_evidence(&root, &pose, &frames, &observations, &out).unwrap();
        let mut changed = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        changed.extend_from_slice(&1920_u32.to_be_bytes());
        changed.extend_from_slice(&1080_u32.to_be_bytes());
        changed.push(1);
        fs::write(root.join("hud-1.png"), changed).unwrap();
        assert!(
            validate_cp2077_metric_evidence(&root, &out)
                .unwrap_err()
                .contains("hash mismatch")
        );
    }

    #[test]
    fn capture_operator_cannot_self_review_hud_readings() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let (pose, frames, observations) = write_fixture(
            &root,
            [150, 135, 120],
            [[0.0, 0.0, 0.0], [15.0, 0.0, 0.0], [30.0, 0.0, 0.0]],
        );
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&observations).unwrap()).unwrap();
        value["review"]["reviewer_id"] = value["capture_operator_id"].clone();
        fs::write(&observations, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
        assert!(derive_evidence(&root, &pose, &frames, &observations).is_err());
    }

    #[test]
    fn qpc_alias_curved_motion_and_scale_disagreement_fail_closed() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let (pose, frames, observations) = write_fixture(
            &root,
            [150, 135, 120],
            [[0.0, 0.0, 0.0], [15.0, 10.0, 0.0], [30.0, 0.0, 0.0]],
        );
        assert!(
            derive_evidence(&root, &pose, &frames, &observations)
                .unwrap_err()
                .contains("straight radial")
        );

        let temp = TempDir::new().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let (pose, frames, observations) = write_fixture(
            &root,
            [150, 140, 130],
            [[0.0, 0.0, 0.0], [15.0, 0.0, 0.0], [30.0, 0.0, 0.0]],
        );
        assert!(
            derive_evidence(&root, &pose, &frames, &observations)
                .unwrap_err()
                .contains("cross-check")
        );

        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&observations).unwrap()).unwrap();
        value["samples"][1]["qpc"] = serde_json::json!(9_999);
        fs::write(&observations, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
        assert!(
            derive_evidence(&root, &pose, &frames, &observations)
                .unwrap_err()
                .contains("no exact frame-evidence")
        );
    }
}

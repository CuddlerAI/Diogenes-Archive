//! Fail-closed, local-only CP2077 raw capture to PR #405 package conversion.
//!
//! The adapter does not infer projection, metric scale, pose, or frame pairing. Every output RGB
//! frame must carry one exact selected-Present/QPC identity shared by camera and player pose; every
//! even frame additionally carries the exact raw D32 row and physical projection observed on that
//! same Present. Missing evidence is rejected before an output directory is created.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read as _, Write as _};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use mp4::{MediaType, Mp4Reader};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};

use crate::calibration::MetricScaleCalibration;
use crate::camera_action::{CameraPoseSource, FinalCameraIdentity, FinalPoseRow};
use crate::cp2077_metric_evidence::validate_cp2077_metric_evidence;
use crate::deliver::{FlatZip, build_action_camera_30fps, probe_video};
use crate::depth::{Projection, Variant};
use crate::pairing::depth_name_for;
use crate::raw_depth_demo::RawDepthDemoProfile;

pub const CP2077_PR405_INPUT_SCHEMA_V1: &str = "dm-cp2077-pr405-local-package-input/v1";
pub const CP2077_PR405_FRAME_SCHEMA_V1: &str = "dm-cp2077-pr405-frame-evidence/v1";
pub const CP2077_PR405_METADATA_SCHEMA_V1: &str = "dm-cp2077-pr405-local-metadata/v1";
pub const CP2077_PR405_QC_SCHEMA_V1: &str = "dm-cp2077-pr405-local-qc/v1";

const RAW_INDEX_SCHEMA_V1: &str = "dm-cp2077-raw-depth-frame-index/v1";
const RGB_FRAMES: usize = 900;
const DEPTH_FRAMES: usize = 450;
const VIDEO_WIDTH: u32 = 1920;
const VIDEO_HEIGHT: u32 = 1080;
const SOURCE_DEPTH_WIDTH: usize = 1920;
const SOURCE_DEPTH_HEIGHT: usize = 1080;
const DEPTH_WIDTH: usize = 1280;
const DEPTH_HEIGHT: usize = 720;
const MAX_SMALL_FILE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_VIDEO_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const CP2077_EGEO_TOOL: &[u8] =
    include_bytes!("../../../../ml/diogenes_wm/eval/cp2077_egeo_gate.py");
const GEO_PROBES_TOOL: &[u8] = include_bytes!("../../../../ml/diogenes_wm/eval/geo_probes.py");

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageInput {
    pub schema: String,
    pub clip_id: String,
    pub non_trainable: bool,
    pub local_only: bool,
    pub upload_allowed: bool,
    pub duration_seconds: u32,
    pub qpc_frequency_hz: u64,
    pub video_file: String,
    pub raw_depth_index_file: String,
    pub raw_depth_profile_file: String,
    pub frame_evidence_file: String,
    pub pose_file: String,
    pub systeminfo_file: String,
    pub egeo_report_file: String,
    pub metric_calibration: MetricScaleCalibration,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PresentIdentity {
    pub present_seq: u64,
    pub qpc: i64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DepthBinding {
    pub ordinal: u64,
    pub file: String,
    pub identity: PresentIdentity,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectionEvidence {
    pub proj_a: f64,
    pub proj_b: f64,
    pub reverse_z: bool,
    pub observed_this_frame: bool,
    pub physical_source: String,
    pub candidate_admitted: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrameEvidence {
    pub schema: String,
    pub session_frame: u64,
    pub duplicate: bool,
    pub rgb: PresentIdentity,
    pub camera: PresentIdentity,
    pub player: PresentIdentity,
    pub depth: Option<DepthBinding>,
    pub projection: Option<ProjectionEvidence>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDepthRow {
    schema: String,
    ordinal: u64,
    file: String,
    present_seq: u64,
    present_qpc: i64,
    current_present_seq: u64,
    readback_lag_presents: u64,
    capture_id: u64,
    source_candidate_id: String,
    runtime_candidate_id: String,
    arm_request_observed_qpc: i64,
    arm_start_present_seq: u64,
    width: usize,
    height: usize,
    row_pitch: usize,
    pixel_format: String,
    byte_len: usize,
    rgb_pairing: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SystemInfo {
    #[serde(rename = "gameProcessName")]
    game_process_name: String,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    #[serde(rename = "recordDpi")]
    record_dpi: f64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileDigest {
    pub bytes: u64,
    pub sha256: String,
}

#[derive(Debug, Serialize)]
struct PackageMetadata {
    schema: &'static str,
    clip_id: String,
    spec: &'static str,
    duration_seconds: u32,
    rgb: VideoMetadata,
    depth: DepthMetadata,
    alignment: AlignmentMetadata,
    metric_calibration: MetricScaleCalibration,
    source_hashes: BTreeMap<String, FileDigest>,
    non_trainable: bool,
    local_only: bool,
    upload_allowed: bool,
}

#[derive(Debug, Serialize)]
struct VideoMetadata {
    width: u32,
    height: u32,
    fps: f64,
    frames: usize,
    codec: &'static str,
}

#[derive(Debug, Serialize)]
struct DepthMetadata {
    source_width: usize,
    source_height: usize,
    source_resource_format: i32,
    source_dsv_format: i32,
    width: usize,
    height: usize,
    fps: u32,
    frames: usize,
    channel: &'static str,
    pixel_type: &'static str,
    domain: &'static str,
    units: &'static str,
    invalid: &'static str,
    quantization: &'static str,
    reverse_z_source: &'static str,
    resampling: &'static str,
    intrinsics_scaling: &'static str,
}

#[derive(Debug, Serialize)]
struct AlignmentMetadata {
    qpc_frequency_hz: u64,
    identity: &'static str,
    rgb_pose_rows: usize,
    even_rgb_depth_rows: usize,
    first_present_seq: u64,
    last_present_seq: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct PackageQcReport {
    pub schema: &'static str,
    pub passed: bool,
    pub rgb_frames: usize,
    pub depth_frames: usize,
    pub checks: Vec<String>,
    pub output_hashes: BTreeMap<String, FileDigest>,
    pub non_trainable: bool,
    pub local_only: bool,
    pub upload_allowed: bool,
}

struct Prepared {
    input: PackageInput,
    session: PathBuf,
    video: PathBuf,
    systeminfo: PathBuf,
    frames: Vec<FrameEvidence>,
    poses: Vec<FinalPoseRow>,
    raw_by_identity: BTreeMap<(u64, i64), RawDepthRow>,
    source_hashes: BTreeMap<String, FileDigest>,
}

pub fn package_cp2077_pr405_local(
    session_dir: &Path,
    manifest_path: &Path,
    output_dir: &Path,
) -> Result<PackageQcReport, String> {
    let prepared = prepare(session_dir, manifest_path, output_dir)?;
    let parent = output_dir
        .parent()
        .ok_or("output directory has no parent")?
        .canonicalize()
        .map_err(|error| format!("canonicalize output parent failed: {error}"))?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "system clock is before Unix epoch")?
        .as_nanos();
    let staging = parent.join(format!(
        ".cp2077-pr405-staging-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir(&staging)
        .map_err(|error| format!("create staging directory failed: {error}"))?;
    let result = materialize(&prepared, &staging);
    match result {
        Ok(report) => {
            if let Err(error) = publish_staging(&staging, output_dir) {
                let _ = fs::remove_dir_all(&staging);
                return Err(error);
            }
            Ok(report)
        }
        Err(error) => {
            let _ = fs::remove_dir_all(&staging);
            Err(error)
        }
    }
}

fn publish_staging(staging: &Path, output: &Path) -> Result<(), String> {
    if output.exists() {
        return Err("output directory appeared during materialization".into());
    }
    fs::rename(staging, output)
        .map_err(|error| format!("atomically publish local package failed: {error}"))
}

fn prepare(
    session_dir: &Path,
    manifest_path: &Path,
    output_dir: &Path,
) -> Result<Prepared, String> {
    if !session_dir.is_absolute() || !manifest_path.is_absolute() || !output_dir.is_absolute() {
        return Err("session, manifest, and output paths must all be absolute".into());
    }
    if output_dir.exists() {
        return Err("output directory must not already exist".into());
    }
    let session = session_dir
        .canonicalize()
        .map_err(|error| format!("canonicalize session failed: {error}"))?;
    require_regular_file(manifest_path, MAX_SMALL_FILE_BYTES)?;
    if manifest_path
        .canonicalize()
        .map_err(|error| format!("canonicalize manifest failed: {error}"))?
        .parent()
        != Some(session.as_path())
    {
        return Err("manifest must be a direct child of the session directory".into());
    }
    let input: PackageInput =
        serde_json::from_slice(&read_bounded(manifest_path, MAX_SMALL_FILE_BYTES)?)
            .map_err(|error| format!("parse package manifest failed: {error}"))?;
    validate_input(&input)?;
    let video = resolve_leaf(&session, &input.video_file, MAX_VIDEO_BYTES)?;
    let raw_index = resolve_leaf(&session, &input.raw_depth_index_file, MAX_SMALL_FILE_BYTES)?;
    let raw_profile = resolve_leaf(
        &session,
        &input.raw_depth_profile_file,
        MAX_SMALL_FILE_BYTES,
    )?;
    let frame_evidence = resolve_leaf(&session, &input.frame_evidence_file, MAX_SMALL_FILE_BYTES)?;
    let pose = resolve_leaf(&session, &input.pose_file, MAX_SMALL_FILE_BYTES)?;
    let systeminfo = resolve_leaf(&session, &input.systeminfo_file, MAX_SMALL_FILE_BYTES)?;
    let egeo = resolve_leaf(&session, &input.egeo_report_file, MAX_SMALL_FILE_BYTES)?;
    let metric_evidence = resolve_leaf(
        &session,
        &input.metric_calibration.evidence_path,
        MAX_SMALL_FILE_BYTES,
    )?;
    validate_metric_calibration(&input.metric_calibration, &session, &metric_evidence)?;

    validate_video(&video)?;
    validate_systeminfo(&systeminfo)?;
    let frames: Vec<FrameEvidence> = read_jsonl(&frame_evidence, RGB_FRAMES)?;
    let poses = crate::deliver::read_pose_rows(&pose)?;
    let raw_rows: Vec<RawDepthRow> = read_jsonl(&raw_index, 1_024)?;
    let profile = validate_raw_profile(&raw_profile)?;
    let raw_by_identity = validate_alignment(&input, &frames, &poses, raw_rows, &profile)?;

    let mut source_hashes = BTreeMap::new();
    for (name, path) in [
        ("manifest", manifest_path),
        ("video", video.as_path()),
        ("raw_depth_index", raw_index.as_path()),
        ("raw_depth_profile", raw_profile.as_path()),
        ("frame_evidence", frame_evidence.as_path()),
        ("pose", pose.as_path()),
        ("systeminfo", systeminfo.as_path()),
        ("egeo", egeo.as_path()),
        ("metric_evidence", metric_evidence.as_path()),
    ] {
        source_hashes.insert(name.into(), digest_file(path)?);
    }
    validate_egeo(&egeo, &source_hashes, &session, &raw_by_identity)?;
    Ok(Prepared {
        input,
        session,
        video,
        systeminfo,
        frames,
        poses,
        raw_by_identity,
        source_hashes,
    })
}

fn validate_metric_calibration(
    calibration: &MetricScaleCalibration,
    session: &Path,
    evidence_path: &Path,
) -> Result<(), String> {
    calibration
        .validate()
        .map_err(|error| format!("metric calibration rejected: {error}"))?;
    let cross = calibration
        .cross_check
        .ok_or("CP2077 metric scale requires two independent, agreeing sources")?;
    let evidence = validate_cp2077_metric_evidence(session, evidence_path)?;
    let equal = |left: f64, right: f64| {
        left.is_finite()
            && right.is_finite()
            && (left - right).abs() <= f64::EPSILON * 16.0 * left.abs().max(right.abs()).max(1.0)
    };
    if !equal(calibration.value, evidence.adopted_scale)
        || !equal(cross.convention, evidence.source_a_scale)
        || !equal(cross.measured, evidence.source_b_scale)
        || !equal(
            cross.ratio,
            evidence.source_b_scale / evidence.source_a_scale,
        )
        || !equal(cross.tolerance, evidence.tolerance)
    {
        return Err(
            "metric calibration fields do not match recomputed CP2077 dual-source evidence".into(),
        );
    }
    Ok(())
}

fn validate_input(input: &PackageInput) -> Result<(), String> {
    if input.schema != CP2077_PR405_INPUT_SCHEMA_V1 {
        return Err("unsupported CP2077 PR405 package manifest schema".into());
    }
    if input.clip_id.is_empty()
        || input.clip_id.len() > 64
        || !input
            .clip_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err("clip_id is not a bounded opaque identifier".into());
    }
    if !input.non_trainable || !input.local_only || input.upload_allowed {
        return Err(
            "CP2077 PR405 converter is local-only, non-trainable, and never uploadable".into(),
        );
    }
    if input.duration_seconds != 30 || input.qpc_frequency_hz == 0 {
        return Err(
            "this converter accepts exactly 30 seconds and a non-zero QPC frequency".into(),
        );
    }
    for path in [
        &input.video_file,
        &input.raw_depth_index_file,
        &input.raw_depth_profile_file,
        &input.frame_evidence_file,
        &input.pose_file,
        &input.systeminfo_file,
        &input.egeo_report_file,
        &input.metric_calibration.evidence_path,
    ] {
        validate_relative_leaf(path)?;
    }
    Ok(())
}

fn validate_video(path: &Path) -> Result<(), String> {
    let info = probe_video(path)?;
    if info.width != VIDEO_WIDTH
        || info.height != VIDEO_HEIGHT
        || (info.fps - 30.0).abs() > 0.001
        || info.frames != RGB_FRAMES as u64
    {
        return Err(format!(
            "video must be exactly 1920x1080, 30fps, 900 frames; observed {}x{} {:.6}fps {} frames",
            info.width, info.height, info.fps, info.frames
        ));
    }
    let file = File::open(path).map_err(|error| format!("open video failed: {error}"))?;
    let bytes = file
        .metadata()
        .map_err(|error| format!("read video metadata failed: {error}"))?
        .len();
    let reader = Mp4Reader::read_header(std::io::BufReader::new(file), bytes)
        .map_err(|error| format!("parse video codec failed: {error}"))?;
    let track = reader
        .tracks()
        .values()
        .next()
        .ok_or("video has no track")?;
    if track
        .media_type()
        .map_err(|error| format!("read video codec failed: {error}"))?
        != MediaType::H264
    {
        return Err("video track must be H.264".into());
    }
    Ok(())
}

fn validate_systeminfo(path: &Path) -> Result<(), String> {
    let info: SystemInfo = serde_json::from_slice(&read_bounded(path, MAX_SMALL_FILE_BYTES)?)
        .map_err(|error| format!("parse systeminfo failed: {error}"))?;
    if !info
        .game_process_name
        .to_ascii_lowercase()
        .contains("cyberpunk2077")
        || info.x != 0
        || info.y != 0
        || info.width != VIDEO_WIDTH
        || info.height != VIDEO_HEIGHT
        || !info.record_dpi.is_finite()
        || info.record_dpi <= 0.0
    {
        return Err(
            "systeminfo does not prove fullscreen CP2077 at 1920x1080 with valid DPI".into(),
        );
    }
    Ok(())
}

fn validate_raw_profile(path: &Path) -> Result<RawDepthDemoProfile, String> {
    let profile: RawDepthDemoProfile =
        serde_json::from_slice(&read_bounded(path, MAX_SMALL_FILE_BYTES)?)
            .map_err(|error| format!("parse raw depth profile failed: {error}"))?;
    profile
        .validate()
        .map_err(|error| format!("raw depth profile rejected: {error}"))?;
    if profile.selector.width != SOURCE_DEPTH_WIDTH as u32
        || profile.selector.height != SOURCE_DEPTH_HEIGHT as u32
        || profile.selector.sample_count != 1
        || profile.selector.resource_format != 19
        || profile.selector.dsv_format != 20
        || !is_canonical_dsv_candidate_id(&profile.selector.candidate_id)
    {
        return Err(
            "PR405 CP2077 requires the exact native 1920x1080 R32G8X24/D32S8 selector".into(),
        );
    }
    Ok(profile)
}

fn is_canonical_dsv_candidate_id(candidate_id: &str) -> bool {
    let Some((r_component, suffix)) = candidate_id
        .strip_prefix("dsv-r")
        .and_then(|suffix| suffix.split_once("-g"))
    else {
        return false;
    };
    let Some((generation, epoch)) = suffix.split_once("-e") else {
        return false;
    };
    [r_component, generation, epoch]
        .into_iter()
        .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EgeoConsumedDepth {
    ordinal: u64,
    file: String,
    present_seq: u64,
    qpc: i64,
    sha256: String,
}

fn validate_egeo(
    path: &Path,
    source_hashes: &BTreeMap<String, FileDigest>,
    session: &Path,
    raw_by_identity: &BTreeMap<(u64, i64), RawDepthRow>,
) -> Result<(), String> {
    let value: Value = serde_json::from_slice(&read_bounded(path, MAX_SMALL_FILE_BYTES)?)
        .map_err(|error| format!("parse E-geo report failed: {error}"))?;
    // 这是 local/non-trainable 证据包，只能证明 E-geo 已达到可独立复跑状态，绝不能
    // 让同一份报告自授 production 权限。正式晋升必须由包外的独立签名 promotion 完成。
    if value.pointer("/schema").and_then(Value::as_str) != Some("dm-cp2077-camera-egeo-evidence/v1")
        || value.pointer("/non_trainable").and_then(Value::as_bool) != Some(true)
        || value.pointer("/local_only").and_then(Value::as_bool) != Some(true)
        || value.pointer("/upload_allowed").and_then(Value::as_bool) != Some(false)
        || value.pointer("/e_geo/schema").and_then(Value::as_str)
            != Some("camera-egeo-validation/v3")
        || value.pointer("/e_geo/passed").and_then(Value::as_bool) != Some(true)
        || value
            .pointer("/e_geo/ready_for_independent_repeat")
            .and_then(Value::as_bool)
            != Some(true)
        || value
            .pointer("/e_geo/production_binding_authorized")
            .and_then(Value::as_bool)
            != Some(false)
    {
        return Err(
            "E-geo v3 local evidence must pass, be ready for independent repeat, and must not self-authorize a production camera binding"
                .into(),
        );
    }
    for (pointer, source) in [
        ("/e_geo/video_sha256", "video"),
        ("/e_geo/raw_depth_index_sha256", "raw_depth_index"),
        ("/e_geo/raw_depth_profile_sha256", "raw_depth_profile"),
        ("/e_geo/frame_evidence_sha256", "frame_evidence"),
        ("/e_geo/pose_sha256", "pose"),
        ("/e_geo/metric_evidence_sha256", "metric_evidence"),
    ] {
        let expected = source_hashes
            .get(source)
            .expect("all E-geo source hashes were computed");
        if value.pointer(pointer).and_then(Value::as_str) != Some(expected.sha256.as_str()) {
            return Err(format!(
                "E-geo v3 is not bound to the current {source} bytes"
            ));
        }
    }
    let failures = value
        .pointer("/e_geo/failures")
        .and_then(Value::as_array)
        .ok_or("E-geo v3 report has no explicit failure list")?;
    if !failures.is_empty()
        || json_u64(&value, "/e_geo/sampled_pairs/static")? < 5
        || json_u64(&value, "/e_geo/sampled_pairs/translation")? < 5
        || json_u64(&value, "/e_geo/sampled_pairs/rotation")? < 5
        || json_u64(&value, "/e_geo/usable_motion_pairs")? < 30
        || json_u64(&value, "/e_geo/invalid_metric_pairs")? != 0
    {
        return Err("E-geo v3 static/translation/rotation coverage is insufficient".into());
    }
    let improvement = json_f64(&value, "/e_geo/metrics/improvement_p50")?;
    let inverse = json_f64(&value, "/e_geo/metrics/inverse_improvement_p50")?;
    let improved_share = json_f64(&value, "/e_geo/metrics/improved_share")?;
    let valid_frac = json_f64(&value, "/e_geo/metrics/valid_frac_p10")?;
    let static_l1 = json_f64(&value, "/e_geo/metrics/static_baseline_l1_p50")?;
    let moving_l1 = json_f64(&value, "/e_geo/metrics/moving_baseline_l1_p50")?;
    if improvement < 1.05
        || improvement < inverse * 1.05
        || improved_share < 0.60
        || valid_frac < 0.50
        || static_l1 >= moving_l1
    {
        return Err("E-geo v3 metrics do not satisfy the FO4-equivalent hard thresholds".into());
    }
    if json_u64(&value, "/e_geo/source_depth_geometry/width")? != 1920
        || json_u64(&value, "/e_geo/source_depth_geometry/height")? != 1080
        || json_u64(&value, "/e_geo/source_depth_geometry/resource_format")? != 19
        || json_u64(&value, "/e_geo/source_depth_geometry/dsv_format")? != 20
        || json_u64(&value, "/e_geo/source_depth_geometry/sample_count")? != 1
        || value
            .pointer("/e_geo/source_depth_geometry/extracted_pixel_format")
            .and_then(Value::as_str)
            != Some("DXGI_FORMAT_D32_FLOAT_LE")
        || json_u64(&value, "/e_geo/output_depth_geometry/width")? != 1280
        || json_u64(&value, "/e_geo/output_depth_geometry/height")? != 720
        || value
            .pointer("/e_geo/resampling/method")
            .and_then(Value::as_str)
            != Some("pixel_center_nearest_2_over_3")
        || value
            .pointer("/e_geo/resampling/integer_mapping")
            .and_then(Value::as_str)
            != Some("src=floor((3*dst+1)/2)")
        || value
            .pointer("/e_geo/resampling/intrinsics_scaling")
            .and_then(Value::as_str)
            != Some("fx_fy_cx_cy_times_2_over_3")
    {
        return Err(
            "E-geo v3 is not bound to the exact native-D32S8 2/3 resampling contract".into(),
        );
    }
    for (pointer, bytes, label) in [
        ("/e_geo/tool_sha256", CP2077_EGEO_TOOL, "CP2077 E-geo tool"),
        (
            "/e_geo/geo_probes_sha256",
            GEO_PROBES_TOOL,
            "shared geometry kernel",
        ),
    ] {
        let expected = sha256_bytes(bytes);
        if value.pointer(pointer).and_then(Value::as_str) != Some(expected.as_str()) {
            return Err(format!("E-geo v3 is not bound to the current {label}"));
        }
    }
    if value
        .pointer("/e_geo/depth_byte_binding")
        .and_then(Value::as_str)
        != Some("all_450_indexed_depth_files")
    {
        return Err("E-geo v3 does not bind every indexed D32 file".into());
    }
    let bound: Vec<EgeoConsumedDepth> = serde_json::from_value(
        value
            .pointer("/e_geo/bound_depth_files")
            .cloned()
            .ok_or("E-geo v3 has no complete D32 byte manifest")?,
    )
    .map_err(|error| format!("parse E-geo bound D32 manifest failed: {error}"))?;
    if bound.len() != raw_by_identity.len() {
        return Err("E-geo v3 bound D32 manifest does not cover the full raw index".into());
    }
    let mut bound_identities = BTreeSet::new();
    for depth in &bound {
        if !bound_identities.insert((depth.present_seq, depth.qpc)) {
            return Err("E-geo v3 repeats a bound D32 identity".into());
        }
        let raw = raw_by_identity
            .get(&(depth.present_seq, depth.qpc))
            .ok_or("E-geo v3 bound D32 is not in the current raw index")?;
        if raw.ordinal != depth.ordinal || raw.file != depth.file {
            return Err("E-geo v3 bound D32 identity/ordinal/file mismatch".into());
        }
        validate_relative_leaf(&depth.file)?;
        if digest_file(&session.join("raw-depth").join(&depth.file))?.sha256 != depth.sha256 {
            return Err("E-geo v3 bound D32 bytes changed after validation".into());
        }
    }
    let expected_identities = raw_by_identity.keys().copied().collect::<BTreeSet<_>>();
    if bound_identities != expected_identities {
        return Err("E-geo v3 bound D32 identities differ from the full raw index".into());
    }
    let consumed: Vec<EgeoConsumedDepth> = serde_json::from_value(
        value
            .pointer("/e_geo/consumed_depth_files")
            .cloned()
            .ok_or("E-geo v3 does not identify consumed D32 files")?,
    )
    .map_err(|error| format!("parse E-geo consumed D32 manifest failed: {error}"))?;
    if consumed.len() < 30 || consumed.len() > 120 {
        return Err("E-geo v3 consumed D32 sample count is outside [30,120]".into());
    }
    let mut identities = BTreeSet::new();
    for depth in consumed {
        if !identities.insert((depth.present_seq, depth.qpc)) {
            return Err("E-geo v3 repeats a consumed D32 identity".into());
        }
        let raw = raw_by_identity
            .get(&(depth.present_seq, depth.qpc))
            .ok_or("E-geo v3 consumed D32 is not in the current raw index")?;
        if raw.ordinal != depth.ordinal || raw.file != depth.file {
            return Err("E-geo v3 consumed D32 identity/ordinal/file mismatch".into());
        }
        validate_relative_leaf(&depth.file)?;
        if digest_file(&session.join("raw-depth").join(&depth.file))?.sha256 != depth.sha256 {
            return Err("E-geo v3 consumed D32 bytes changed after validation".into());
        }
    }
    Ok(())
}

fn json_f64(value: &Value, pointer: &str) -> Result<f64, String> {
    let number = value
        .pointer(pointer)
        .and_then(Value::as_f64)
        .ok_or_else(|| format!("E-geo v3 missing numeric {pointer}"))?;
    if !number.is_finite() {
        return Err(format!("E-geo v3 has non-finite {pointer}"));
    }
    Ok(number)
}

fn json_u64(value: &Value, pointer: &str) -> Result<u64, String> {
    value
        .pointer(pointer)
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("E-geo v3 missing integer {pointer}"))
}

fn sha256_bytes(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn validate_alignment(
    input: &PackageInput,
    frames: &[FrameEvidence],
    poses: &[FinalPoseRow],
    raw_rows: Vec<RawDepthRow>,
    profile: &RawDepthDemoProfile,
) -> Result<BTreeMap<(u64, i64), RawDepthRow>, String> {
    if frames.len() != RGB_FRAMES || poses.len() != RGB_FRAMES {
        return Err(format!(
            "30s package requires 900 frame-evidence and pose rows; observed {} and {}",
            frames.len(),
            poses.len()
        ));
    }
    let mut raw_by_identity = BTreeMap::new();
    let mut raw_ordinals = BTreeSet::new();
    let mut previous_raw = None;
    for raw in raw_rows {
        validate_raw_row(&raw, profile)?;
        let identity = (raw.present_seq, raw.present_qpc);
        if previous_raw.is_some_and(|previous: (u64, i64)| {
            identity.0 <= previous.0 || identity.1 <= previous.1
        }) {
            return Err("raw depth index Present/QPC timeline is not strictly increasing".into());
        }
        previous_raw = Some(identity);
        if !raw_ordinals.insert(raw.ordinal) || raw_by_identity.insert(identity, raw).is_some() {
            return Err("raw depth index has duplicate ordinal or Present/QPC identity".into());
        }
    }

    let mut previous_rgb = None;
    let mut depth_count = 0;
    let mut used_raw_identities = BTreeSet::new();
    let mut camera_identity = None;
    let mut camera_source = None;
    for (index, (frame, pose)) in frames.iter().zip(poses).enumerate() {
        if frame.schema != CP2077_PR405_FRAME_SCHEMA_V1
            || frame.session_frame != index as u64
            || pose.session_frame != index as u64
        {
            return Err(format!(
                "frame evidence or pose row {index} is not contiguous"
            ));
        }
        validate_present(frame.rgb, "RGB", index)?;
        if frame.camera != frame.rgb || frame.player != frame.rgb {
            return Err(format!(
                "frame {index} camera/player identity is not the exact RGB Present/QPC"
            ));
        }
        if pose.present_seq != frame.rgb.present_seq {
            return Err(format!("frame {index} pose Present does not match RGB"));
        }
        validate_pose(pose, index)?;
        if let Some(identity) = camera_identity.as_ref() {
            if identity != &pose.camera_identity {
                return Err("camera stream identity changed within the clip".into());
            }
        } else {
            camera_identity = Some(pose.camera_identity.clone());
        }
        if let Some(source) = camera_source {
            if source != pose.camera_source {
                return Err("camera source changed within the clip".into());
            }
        } else {
            camera_source = Some(pose.camera_source);
        }
        if let Some(previous) = previous_rgb {
            if frame.duplicate {
                if frame.rgb != previous {
                    return Err(format!(
                        "frame {index} is duplicate but does not retain the immediately previous RGB identity"
                    ));
                }
            } else if frame.rgb.present_seq <= previous.present_seq || frame.rgb.qpc <= previous.qpc
            {
                return Err(format!("frame {index} RGB identity did not advance"));
            }
        } else if frame.duplicate {
            return Err("frame 0 cannot be a duplicate".into());
        }
        previous_rgb = Some(frame.rgb);

        if index.is_multiple_of(2) {
            let depth = frame
                .depth
                .as_ref()
                .ok_or_else(|| format!("even frame {index} has no depth binding"))?;
            let projection = frame
                .projection
                .as_ref()
                .ok_or_else(|| format!("even frame {index} has no physical projection"))?;
            if depth.identity != frame.rgb {
                return Err(format!(
                    "even frame {index} depth identity does not match RGB"
                ));
            }
            validate_projection(projection, index, input.metric_calibration.value)?;
            let raw = raw_by_identity
                .get(&(depth.identity.present_seq, depth.identity.qpc))
                .ok_or_else(|| format!("even frame {index} has no exact raw D32 row"))?;
            if raw.ordinal != depth.ordinal || raw.file != depth.file {
                return Err(format!("even frame {index} raw depth binding drifted"));
            }
            used_raw_identities.insert((depth.identity.present_seq, depth.identity.qpc));
            depth_count += 1;
        } else if frame.depth.is_some() || frame.projection.is_some() {
            return Err(format!(
                "odd frame {index} must have neither depth nor projection"
            ));
        }
    }
    if depth_count != DEPTH_FRAMES {
        return Err(format!(
            "expected 450 even depth frames, observed {depth_count}"
        ));
    }
    if used_raw_identities.len() != raw_by_identity.len() {
        return Err("raw depth index contains rows not selected by exact RGB alignment".into());
    }
    Ok(raw_by_identity)
}

fn validate_present(identity: PresentIdentity, label: &str, index: usize) -> Result<(), String> {
    if identity.present_seq == 0 || identity.qpc <= 0 {
        return Err(format!("frame {index} {label} identity is invalid"));
    }
    Ok(())
}

fn validate_projection(
    projection: &ProjectionEvidence,
    index: usize,
    metric_scale: f64,
) -> Result<(), String> {
    if !projection.observed_this_frame
        || !projection.candidate_admitted
        || projection.physical_source.trim().is_empty()
        || !projection.proj_a.is_finite()
        || !projection.proj_b.is_finite()
        || projection.proj_b <= 0.0
        || !projection.reverse_z
        || !metric_scale.is_finite()
        || metric_scale <= 0.0
    {
        return Err(format!(
            "frame {index} lacks an admitted physical reversed-Z projection or metric scale"
        ));
    }
    let derived = Projection {
        a: projection.proj_a,
        b: projection.proj_b,
        metric_scale,
    };
    if !derived.is_reversed_z() {
        return Err(format!(
            "frame {index} projection convention is contradictory"
        ));
    }
    Ok(())
}

fn validate_pose(pose: &FinalPoseRow, index: usize) -> Result<(), String> {
    if !matches!(
        pose.camera_identity,
        FinalCameraIdentity::ProbeStream { .. }
    ) || !matches!(
        pose.camera_source,
        CameraPoseSource::ResolvedMainCameraHook | CameraPoseSource::ResolvedRenderCameraHybrid
    ) || !pose.fps.is_finite()
        || pose.fps < 30.0
    {
        return Err(format!(
            "frame {index} pose is not one resolved exact-build camera stream"
        ));
    }
    if let FinalCameraIdentity::ProbeStream {
        session_id,
        run_id,
        generation,
    } = &pose.camera_identity
        && (session_id.trim().is_empty() || run_id.trim().is_empty() || *generation == 0)
    {
        return Err(format!(
            "frame {index} camera stream identity is incomplete"
        ));
    }
    for (label, values) in [
        ("camera_position", pose.camera_position.as_slice()),
        (
            "camera_rotation_quaternion",
            pose.camera_rotation_quaternion.as_slice(),
        ),
        ("camera_follow_offset", pose.camera_follow_offset.as_slice()),
        ("camera_speed", pose.camera_speed.as_slice()),
        ("player_position", pose.player_position.as_slice()),
        (
            "player_rotation_quaternion",
            pose.player_rotation_quaternion.as_slice(),
        ),
        ("player_speed", pose.player_speed.as_slice()),
    ] {
        if !values.iter().all(|value| value.is_finite()) {
            return Err(format!("frame {index} {label} contains a non-finite value"));
        }
    }
    for (label, quaternion) in [
        ("camera", pose.camera_rotation_quaternion),
        ("player", pose.player_rotation_quaternion),
    ] {
        let norm = quaternion.iter().map(|value| value * value).sum::<f64>();
        if (norm - 1.0).abs() > 0.002 {
            return Err(format!(
                "frame {index} {label} quaternion is not normalized"
            ));
        }
    }
    let intrinsics = pose.camera_intrinsics;
    if ![intrinsics.fx, intrinsics.fy, intrinsics.cx, intrinsics.cy]
        .into_iter()
        .all(|value| value.is_finite())
        || intrinsics.fx <= 0.0
        || intrinsics.fy <= 0.0
        || (intrinsics.cx - VIDEO_WIDTH as f32 / 2.0).abs() > 0.01
        || (intrinsics.cy - VIDEO_HEIGHT as f32 / 2.0).abs() > 0.01
    {
        return Err(format!(
            "frame {index} camera intrinsics are not for 1920x1080"
        ));
    }
    intrinsics
        .check_square_pixels()
        .map_err(|error| format!("frame {index}: {error}"))?;
    if !pose.mouse_x.is_finite()
        || !pose.mouse_y.is_finite()
        || !pose.mouse_dx.is_finite()
        || !pose.mouse_dy.is_finite()
        || !(0.0..=1.0).contains(&pose.mouse_x)
        || !(0.0..=1.0).contains(&pose.mouse_y)
        || !(-1.0..=1.0).contains(&pose.mouse_dx)
        || !(-1.0..=1.0).contains(&pose.mouse_dy)
        || pose.time.trim().is_empty()
    {
        return Err(format!("frame {index} action fields are invalid"));
    }
    Ok(())
}

fn validate_raw_row(row: &RawDepthRow, profile: &RawDepthDemoProfile) -> Result<(), String> {
    if row.schema != RAW_INDEX_SCHEMA_V1
        || row.file != format!("{:012}.d32", row.ordinal)
        || row.present_seq == 0
        || row.present_qpc <= 0
        || row.current_present_seq < row.present_seq
        || row.current_present_seq - row.present_seq != row.readback_lag_presents
        || row.capture_id == 0
        || row.source_candidate_id != profile.selector.candidate_id
        || row.runtime_candidate_id.trim().is_empty()
        || row.arm_request_observed_qpc <= 0
        || row.arm_start_present_seq == 0
        || row.present_seq < row.arm_start_present_seq
        || row.width != SOURCE_DEPTH_WIDTH
        || row.height != SOURCE_DEPTH_HEIGHT
        || row.row_pitch != SOURCE_DEPTH_WIDTH * 4
        || row.pixel_format != "DXGI_FORMAT_D32_FLOAT_LE"
        || row.byte_len != SOURCE_DEPTH_WIDTH * SOURCE_DEPTH_HEIGHT * 4
        || row.rgb_pairing != "same_present_seq_gpu_watermark_submitted"
    {
        return Err(format!(
            "raw depth row {} violates the exact D32 contract",
            row.ordinal
        ));
    }
    Ok(())
}

fn materialize(prepared: &Prepared, staging: &Path) -> Result<PackageQcReport, String> {
    let video_out = staging.join("video.mp4");
    fs::copy(&prepared.video, &video_out).map_err(|error| format!("copy video failed: {error}"))?;
    let systeminfo_out = staging.join("systeminfo.json");
    fs::copy(&prepared.systeminfo, &systeminfo_out)
        .map_err(|error| format!("copy systeminfo failed: {error}"))?;

    let full_action = build_action_camera_30fps(&prepared.poses)?;
    let full_action_path = staging.join("action_camera_30fps.json");
    write_new(&full_action_path, full_action.as_bytes())?;
    let depth_action = build_depth_action_camera(&full_action)?;
    let depth_action_path = staging.join("action_camera.json");
    write_new(&depth_action_path, depth_action.as_bytes())?;

    let mut zip = FlatZip::new();
    let temporary_exr = staging.join(".depth-frame.exr");
    let mut projection_sources = BTreeSet::new();
    for frame in prepared
        .frames
        .iter()
        .filter(|frame| frame.session_frame.is_multiple_of(2))
    {
        let depth = frame.depth.as_ref().expect("alignment validated");
        let projection = frame.projection.as_ref().expect("alignment validated");
        projection_sources.insert(projection.physical_source.clone());
        let raw = prepared
            .raw_by_identity
            .get(&(depth.identity.present_seq, depth.identity.qpc))
            .expect("alignment validated");
        let buffer = read_raw_frame(&prepared.session, raw)?;
        let metric_projection = Projection {
            a: projection.proj_a,
            b: projection.proj_b,
            metric_scale: prepared.input.metric_calibration.value,
        };
        validate_metric_pixels(&buffer, metric_projection, frame.session_frame)?;
        crate::exr_out::write_frame(
            &temporary_exr,
            &buffer,
            DEPTH_WIDTH,
            DEPTH_HEIGHT,
            metric_projection,
            Variant::Spec,
        )?;
        let exr = fs::read(&temporary_exr)
            .map_err(|error| format!("read generated EXR failed: {error}"))?;
        audit_exr_header(&exr)?;
        let (readback, width, height) = crate::exr_out::read_frame(&temporary_exr)?;
        if width != DEPTH_WIDTH
            || height != DEPTH_HEIGHT
            || readback
                .iter()
                .any(|value| value.is_finite() && *value <= 0.0)
        {
            return Err(format!(
                "generated EXR for frame {} failed metric readback QC",
                frame.session_frame
            ));
        }
        zip.add(
            &format!("{}.exr", depth_name_for(frame.session_frame)),
            &exr,
        );
        fs::remove_file(&temporary_exr)
            .map_err(|error| format!("remove temporary EXR failed: {error}"))?;
    }
    if projection_sources.len() != 1 {
        return Err(format!(
            "physical projection source changed within the clip: {} sources",
            projection_sources.len()
        ));
    }
    let depth_zip_path = staging.join("depth.zip");
    write_new(&depth_zip_path, &zip.finish())?;

    let video_info = probe_video(&video_out)?;
    let metadata = PackageMetadata {
        schema: CP2077_PR405_METADATA_SCHEMA_V1,
        clip_id: prepared.input.clip_id.clone(),
        spec: "PR #405 / docs/68 §5-§7; docs/69 not applicable (single camera)",
        duration_seconds: 30,
        rgb: VideoMetadata {
            width: video_info.width,
            height: video_info.height,
            fps: video_info.fps,
            frames: RGB_FRAMES,
            codec: "h264",
        },
        depth: DepthMetadata {
            source_width: SOURCE_DEPTH_WIDTH,
            source_height: SOURCE_DEPTH_HEIGHT,
            source_resource_format: 19,
            source_dsv_format: 20,
            width: DEPTH_WIDTH,
            height: DEPTH_HEIGHT,
            fps: 15,
            frames: DEPTH_FRAMES,
            channel: crate::EXR_CHANNEL_NAME,
            pixel_type: "float16",
            domain: "view_space_optical_axis_z",
            units: "meters",
            invalid: "NaN",
            quantization: "trunc(z*100)/100 then IEEE-754 binary16",
            reverse_z_source: "per-frame admitted physical projection",
            resampling: "pixel_center_nearest_2_over_3; src=floor((3*dst+1)/2)",
            intrinsics_scaling: "fx,fy,cx,cy multiplied by 2/3 for 1280x720 geometry checks",
        },
        alignment: AlignmentMetadata {
            qpc_frequency_hz: prepared.input.qpc_frequency_hz,
            identity: "exact selected Present sequence and QPC; no nearest-neighbor matching",
            rgb_pose_rows: RGB_FRAMES,
            even_rgb_depth_rows: DEPTH_FRAMES,
            first_present_seq: prepared.frames[0].rgb.present_seq,
            last_present_seq: prepared.frames[RGB_FRAMES - 1].rgb.present_seq,
        },
        metric_calibration: prepared.input.metric_calibration.clone(),
        source_hashes: prepared.source_hashes.clone(),
        non_trainable: true,
        local_only: true,
        upload_allowed: false,
    };
    let metadata_path = staging.join("metadata.json");
    write_json_new(&metadata_path, &metadata)?;

    let mut output_hashes = BTreeMap::new();
    for name in [
        "video.mp4",
        "depth.zip",
        "action_camera.json",
        "action_camera_30fps.json",
        "systeminfo.json",
        "metadata.json",
    ] {
        output_hashes.insert(name.into(), digest_file(&staging.join(name))?);
    }
    let report = PackageQcReport {
        schema: CP2077_PR405_QC_SCHEMA_V1,
        passed: true,
        rgb_frames: RGB_FRAMES,
        depth_frames: DEPTH_FRAMES,
        checks: vec![
            "video_h264_1920x1080_30fps_900_frames".into(),
            "depth_1280x720_15fps_half_zip_single_z_channel".into(),
            "per_frame_reverse_z_physical_projection_linearized_to_metric_optical_axis_z".into(),
            "invalid_nan_and_truncate_toward_zero_0_01m".into(),
            "rgb_depth_camera_player_exact_present_qpc_alignment".into(),
            "unique_current_camera_and_player_pose_for_every_rgb_frame".into(),
            "dual_source_metric_calibration".into(),
            "egeo_v3_ready_for_independent_repeat_without_production_authorization".into(),
            "non_trainable_local_only_no_upload".into(),
        ],
        output_hashes,
        non_trainable: true,
        local_only: true,
        upload_allowed: false,
    };
    let qc_path = staging.join("qc.json");
    write_json_new(&qc_path, &report)?;
    let mut hashes = report.output_hashes.clone();
    hashes.insert("qc.json".into(), digest_file(&qc_path)?);
    write_json_new(&staging.join("sha256.json"), &hashes)?;
    verify_output_hashes(staging, &hashes)?;
    Ok(report)
}

fn build_depth_action_camera(full_action: &str) -> Result<String, String> {
    let rows: Vec<Value> = serde_json::from_str(full_action)
        .map_err(|error| format!("parse generated full action-camera failed: {error}"))?;
    if rows.len() != RGB_FRAMES {
        return Err("generated full action-camera does not have 900 rows".into());
    }
    let mut depth_rows = Vec::with_capacity(DEPTH_FRAMES);
    for (index, mut row) in rows.into_iter().enumerate() {
        if !index.is_multiple_of(2) {
            continue;
        }
        let object = row
            .as_object_mut()
            .ok_or("generated action-camera row is not an object")?;
        object.insert(
            "depth_name".into(),
            Value::String(depth_name_for(index as u64)),
        );
        depth_rows.push(row);
    }
    serde_json::to_string_pretty(&depth_rows)
        .map(|mut text| {
            text.push('\n');
            text
        })
        .map_err(|error| format!("serialize depth action-camera failed: {error}"))
}

fn read_raw_frame(session: &Path, row: &RawDepthRow) -> Result<Vec<f32>, String> {
    let root = session.join("raw-depth");
    let canonical_root = root
        .canonicalize()
        .map_err(|error| format!("canonicalize raw-depth directory failed: {error}"))?;
    validate_relative_leaf(&row.file)?;
    let path = root.join(&row.file);
    require_regular_file(&path, row.byte_len as u64)?;
    let canonical = path
        .canonicalize()
        .map_err(|error| format!("canonicalize raw frame failed: {error}"))?;
    if canonical.parent() != Some(canonical_root.as_path()) {
        return Err("raw frame escaped the raw-depth directory".into());
    }
    let bytes = fs::read(&canonical).map_err(|error| format!("read raw D32 failed: {error}"))?;
    if bytes.len() != row.byte_len {
        return Err(format!(
            "raw D32 {} has an unexpected byte length",
            row.ordinal
        ));
    }
    let mut source = Vec::with_capacity(SOURCE_DEPTH_WIDTH * SOURCE_DEPTH_HEIGHT);
    for chunk in bytes.as_chunks::<4>().0 {
        let value = f32::from_le_bytes(*chunk);
        if !value.is_finite() || !(0.0..=1.0).contains(&value) {
            return Err(format!(
                "raw D32 {} contains a non-finite or out-of-range pixel",
                row.ordinal
            ));
        }
        source.push(value);
    }
    if source.len() != SOURCE_DEPTH_WIDTH * SOURCE_DEPTH_HEIGHT {
        return Err("raw native D32 pixel count is not exactly 1920x1080".into());
    }
    Ok(resample_native_depth_nearest(&source))
}

/// Map each 1280x720 destination pixel centre to the nearest 1920x1080 source pixel centre.
///
/// For the exact 2/3 scale, `src_x = floor(3*x/2 + 1/2)` and likewise for y. This is equivalent
/// to nearest-neighbour sampling of `(dst + 0.5) * 3/2 - 0.5`; it is integer-only and therefore
/// deterministic across toolchains.
fn resample_native_depth_nearest(source: &[f32]) -> Vec<f32> {
    debug_assert_eq!(source.len(), SOURCE_DEPTH_WIDTH * SOURCE_DEPTH_HEIGHT);
    let mut output = Vec::with_capacity(DEPTH_WIDTH * DEPTH_HEIGHT);
    for y in 0..DEPTH_HEIGHT {
        let source_y = (3 * y).div_ceil(2);
        for x in 0..DEPTH_WIDTH {
            let source_x = (3 * x).div_ceil(2);
            output.push(source[source_y * SOURCE_DEPTH_WIDTH + source_x]);
        }
    }
    output
}

fn validate_metric_pixels(
    buffer: &[f32],
    projection: Projection,
    session_frame: u64,
) -> Result<(), String> {
    let mut valid = 0usize;
    let mut min = f64::INFINITY;
    let mut max = f64::NEG_INFINITY;
    for value in buffer {
        if let Some(z) = projection.linearize(f64::from(*value)) {
            valid += 1;
            min = min.min(z);
            max = max.max(z);
        }
    }
    if valid == 0 || !min.is_finite() || !max.is_finite() || max <= min {
        return Err(format!(
            "frame {session_frame} has no non-constant positive metric depth"
        ));
    }
    Ok(())
}

fn audit_exr_header(bytes: &[u8]) -> Result<(), String> {
    if bytes.len() < 16 || bytes[0..4] != [0x76, 0x2f, 0x31, 0x01] {
        return Err("generated depth is not OpenEXR".into());
    }
    let mut cursor = 8usize;
    let mut channels = None;
    let mut compression = None;
    let mut data_window = None;
    while cursor < bytes.len() {
        let name = read_cstring(bytes, &mut cursor)?;
        if name.is_empty() {
            break;
        }
        let kind = read_cstring(bytes, &mut cursor)?;
        let size = read_u32(bytes, &mut cursor)? as usize;
        let end = cursor
            .checked_add(size)
            .filter(|end| *end <= bytes.len())
            .ok_or("EXR attribute extends beyond file")?;
        let payload = &bytes[cursor..end];
        match (name.as_str(), kind.as_str()) {
            ("channels", "chlist") => channels = Some(audit_channel_list(payload)?),
            ("compression", "compression") if payload.len() == 1 => compression = Some(payload[0]),
            ("dataWindow", "box2i") if payload.len() == 16 => {
                let x0 = i32::from_le_bytes(payload[0..4].try_into().unwrap());
                let y0 = i32::from_le_bytes(payload[4..8].try_into().unwrap());
                let x1 = i32::from_le_bytes(payload[8..12].try_into().unwrap());
                let y1 = i32::from_le_bytes(payload[12..16].try_into().unwrap());
                data_window = Some((x0, y0, x1, y1));
            }
            _ => {}
        }
        cursor = end;
    }
    if channels != Some((crate::EXR_CHANNEL_NAME.to_string(), 1))
        || compression != Some(3)
        || data_window != Some((0, 0, DEPTH_WIDTH as i32 - 1, DEPTH_HEIGHT as i32 - 1))
    {
        return Err("EXR is not one Z/HALF/ZIP/1280x720 layer".into());
    }
    Ok(())
}

fn audit_channel_list(payload: &[u8]) -> Result<(String, i32), String> {
    let mut cursor = 0usize;
    let name = read_cstring(payload, &mut cursor)?;
    if name.is_empty() || cursor + 16 > payload.len() {
        return Err("EXR channel list is truncated".into());
    }
    let pixel_type = i32::from_le_bytes(payload[cursor..cursor + 4].try_into().unwrap());
    cursor += 16;
    if !read_cstring(payload, &mut cursor)?.is_empty() || cursor != payload.len() {
        return Err("EXR must have exactly one channel".into());
    }
    Ok((name, pixel_type))
}

fn read_cstring(bytes: &[u8], cursor: &mut usize) -> Result<String, String> {
    let tail = bytes.get(*cursor..).ok_or("EXR string cursor is invalid")?;
    let length = tail
        .iter()
        .position(|byte| *byte == 0)
        .ok_or("EXR string is not terminated")?;
    let value = std::str::from_utf8(&tail[..length])
        .map_err(|_| "EXR string is not UTF-8")?
        .to_string();
    *cursor += length + 1;
    Ok(value)
}

fn read_u32(bytes: &[u8], cursor: &mut usize) -> Result<u32, String> {
    let end = cursor.checked_add(4).ok_or("EXR cursor overflow")?;
    let value = bytes.get(*cursor..end).ok_or("EXR u32 is truncated")?;
    *cursor = end;
    Ok(u32::from_le_bytes(value.try_into().unwrap()))
}

fn validate_relative_leaf(value: &str) -> Result<(), String> {
    let path = Path::new(value);
    if value.trim().is_empty()
        || path.is_absolute()
        || path.components().count() != 1
        || !matches!(path.components().next(), Some(Component::Normal(_)))
    {
        return Err(format!(
            "evidence path must be one relative file name: {value}"
        ));
    }
    Ok(())
}

fn resolve_leaf(root: &Path, value: &str, max_bytes: u64) -> Result<PathBuf, String> {
    validate_relative_leaf(value)?;
    let path = root.join(value);
    require_regular_file(&path, max_bytes)?;
    let canonical = path
        .canonicalize()
        .map_err(|error| format!("canonicalize {value} failed: {error}"))?;
    if canonical.parent() != Some(root) {
        return Err(format!(
            "evidence file {value} escaped the session directory"
        ));
    }
    Ok(canonical)
}

fn require_regular_file(path: &Path, max_bytes: u64) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("read {} metadata failed: {error}", path.display()))?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() == 0
        || metadata.len() > max_bytes
    {
        return Err(format!("{} is not a bounded regular file", path.display()));
    }
    Ok(())
}

fn read_bounded(path: &Path, max_bytes: u64) -> Result<Vec<u8>, String> {
    require_regular_file(path, max_bytes)?;
    fs::read(path).map_err(|error| format!("read {} failed: {error}", path.display()))
}

fn read_jsonl<T: for<'de> Deserialize<'de>>(
    path: &Path,
    max_rows: usize,
) -> Result<Vec<T>, String> {
    let text = String::from_utf8(read_bounded(path, MAX_SMALL_FILE_BYTES)?)
        .map_err(|_| format!("{} is not UTF-8", path.display()))?;
    let mut rows = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if line.len() > 256 * 1024 {
            return Err(format!("{}:{} is too large", path.display(), index + 1));
        }
        if line.trim().is_empty() {
            continue;
        }
        rows.push(
            serde_json::from_str(line).map_err(|error| {
                format!("parse {}:{} failed: {error}", path.display(), index + 1)
            })?,
        );
        if rows.len() > max_rows {
            return Err(format!("{} has too many rows", path.display()));
        }
    }
    if rows.is_empty() {
        return Err(format!("{} is empty", path.display()));
    }
    Ok(rows)
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| format!("create {} failed: {error}", path.display()))?;
    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(|error| format!("write {} failed: {error}", path.display()))
}

fn write_json_new(path: &Path, value: &impl Serialize) -> Result<(), String> {
    let mut bytes = serde_json::to_vec_pretty(value)
        .map_err(|error| format!("serialize {} failed: {error}", path.display()))?;
    bytes.push(b'\n');
    write_new(path, &bytes)
}

fn digest_file(path: &Path) -> Result<FileDigest, String> {
    let mut file = File::open(path).map_err(|error| format!("open hash input failed: {error}"))?;
    let mut hasher = Sha256::new();
    let mut bytes = 0u64;
    let mut buffer = [0u8; 1024 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("hash read failed: {error}"))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        bytes = bytes.saturating_add(read as u64);
    }
    Ok(FileDigest {
        bytes,
        sha256: format!("sha256:{:x}", hasher.finalize()),
    })
}

fn verify_output_hashes(
    root: &Path,
    expected: &BTreeMap<String, FileDigest>,
) -> Result<(), String> {
    for (name, digest) in expected {
        validate_relative_leaf(name)?;
        if digest_file(&root.join(name))? != *digest {
            return Err(format!("output hash changed after sealing: {name}"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calibration::{CrossCheck, MetricScaleSource};
    use crate::camera::Intrinsics;

    fn metric(cross_checked: bool) -> MetricScaleCalibration {
        MetricScaleCalibration {
            value: 1.0,
            source: MetricScaleSource::KnownSizeAnchors {
                anchors: 4,
                residual_rms: 0.001,
            },
            cross_check: cross_checked.then(|| CrossCheck::new(1.0, 1.0, 0.01)),
            evidence_path: "metric-evidence.json".into(),
            evidence_sha256: None,
        }
    }

    fn input() -> PackageInput {
        PackageInput {
            schema: CP2077_PR405_INPUT_SCHEMA_V1.into(),
            clip_id: "cp2077-local-30s-test".into(),
            non_trainable: true,
            local_only: true,
            upload_allowed: false,
            duration_seconds: 30,
            qpc_frequency_hz: 10_000_000,
            video_file: "video.mp4".into(),
            raw_depth_index_file: "raw-depth-index.jsonl".into(),
            raw_depth_profile_file: "raw-depth-demo-profile.json".into(),
            frame_evidence_file: "frame-evidence.jsonl".into(),
            pose_file: "pose.jsonl".into(),
            systeminfo_file: "systeminfo.json".into(),
            egeo_report_file: "egeo.json".into(),
            metric_calibration: metric(true),
        }
    }

    fn pose(frame: u64, present_seq: u64) -> FinalPoseRow {
        FinalPoseRow {
            present_seq,
            session_frame: frame,
            camera_identity: FinalCameraIdentity::ProbeStream {
                session_id: "session".into(),
                run_id: "run".into(),
                generation: 1,
            },
            camera_source: CameraPoseSource::ResolvedRenderCameraHybrid,
            time: format!("2026-08-13 00:00:{:02}.000", frame / 30),
            fps: 60.0,
            camera_position: [frame as f64 / 30.0, 1.6, 0.0],
            camera_rotation_quaternion: [0.0, 0.0, 0.0, 1.0],
            camera_follow_offset: [0.0; 3],
            camera_intrinsics: Intrinsics {
                fx: 1_251.0,
                fy: 1_251.0,
                cx: 960.0,
                cy: 540.0,
            },
            camera_speed: [1.0, 0.0, 0.0],
            player_position: [frame as f64 / 30.0, 0.0, 0.0],
            player_rotation_quaternion: [0.0, 0.0, 0.0, 1.0],
            player_speed: [1.0, 0.0, 0.0],
            mouse_x: 0.5,
            mouse_y: 0.5,
            mouse_dx: 0.0,
            mouse_dy: 0.0,
            key_code: vec![87],
        }
    }

    fn raw(ordinal: u64, identity: PresentIdentity) -> RawDepthRow {
        RawDepthRow {
            schema: RAW_INDEX_SCHEMA_V1.into(),
            ordinal,
            file: format!("{ordinal:012}.d32"),
            present_seq: identity.present_seq,
            present_qpc: identity.qpc,
            current_present_seq: identity.present_seq,
            readback_lag_presents: 0,
            capture_id: ordinal + 1,
            source_candidate_id: "dsv-r19-g1-e1".into(),
            runtime_candidate_id: "dsv-runtime".into(),
            arm_request_observed_qpc: 1,
            arm_start_present_seq: 1,
            width: SOURCE_DEPTH_WIDTH,
            height: SOURCE_DEPTH_HEIGHT,
            row_pitch: SOURCE_DEPTH_WIDTH * 4,
            pixel_format: "DXGI_FORMAT_D32_FLOAT_LE".into(),
            byte_len: SOURCE_DEPTH_WIDTH * SOURCE_DEPTH_HEIGHT * 4,
            rgb_pairing: "same_present_seq_gpu_watermark_submitted".into(),
        }
    }

    fn raw_profile() -> RawDepthDemoProfile {
        RawDepthDemoProfile {
            schema: crate::raw_depth_demo::RAW_DEPTH_DEMO_PROFILE_SCHEMA_V1.into(),
            intent: "local_raw_depth_rgb_alignment_smoke".into(),
            game_id: crate::raw_depth_demo::CP2077_GAME_ID.into(),
            executable_sha256: crate::raw_depth_demo::CP2077_BUILD_20383525_EXE_SHA256.into(),
            process_launch_id: "0123456789abcdef0123456789abcdef".into(),
            resident_probe_sha256: format!("sha256:{}", "a".repeat(64)),
            source_d3d12_report_sha256: format!("sha256:{}", "b".repeat(64)),
            source_sealed_marker_sha256: format!("sha256:{}", "c".repeat(64)),
            discovery_provenance_mode: crate::raw_depth_demo::CLIENT_MANAGED_DISCOVERY_PROVENANCE
                .into(),
            capture_contract: crate::raw_depth_demo::PR405_LOCAL_CONTRACT.into(),
            duration_ms: crate::raw_depth_demo::PR405_LOCAL_DURATION_MS,
            selector: crate::raw_depth_demo::RawDepthDemoSelector {
                candidate_id: "dsv-r19-g1-e1".into(),
                width: SOURCE_DEPTH_WIDTH as u32,
                height: SOURCE_DEPTH_HEIGHT as u32,
                sample_count: 1,
                resource_format: 19,
                dsv_format: 20,
                readback_slots: 4,
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
            blockers: vec!["test blocker".into()],
        }
    }

    fn aligned() -> (Vec<FrameEvidence>, Vec<FinalPoseRow>, Vec<RawDepthRow>) {
        let mut frames = Vec::with_capacity(RGB_FRAMES);
        let mut poses = Vec::with_capacity(RGB_FRAMES);
        let mut raw_rows = Vec::with_capacity(DEPTH_FRAMES);
        for frame in 0..RGB_FRAMES as u64 {
            let identity = PresentIdentity {
                present_seq: frame + 1,
                qpc: 10_000 + frame as i64 * 10,
            };
            let even = frame.is_multiple_of(2);
            frames.push(FrameEvidence {
                schema: CP2077_PR405_FRAME_SCHEMA_V1.into(),
                session_frame: frame,
                duplicate: false,
                rgb: identity,
                camera: identity,
                player: identity,
                depth: even.then(|| DepthBinding {
                    ordinal: frame / 2,
                    file: format!("{:012}.d32", frame / 2),
                    identity,
                }),
                projection: even.then(|| ProjectionEvidence {
                    proj_a: 0.0,
                    proj_b: 0.1,
                    reverse_z: true,
                    observed_this_frame: true,
                    physical_source: "same-present-mapped-root-cbv".into(),
                    candidate_admitted: true,
                }),
            });
            poses.push(pose(frame, identity.present_seq));
            if even {
                raw_rows.push(raw(frame / 2, identity));
            }
        }
        (frames, poses, raw_rows)
    }

    #[test]
    fn exact_30s_alignment_accepts_900_rgb_and_450_depth_rows() {
        let (frames, poses, raw_rows) = aligned();
        let selected =
            validate_alignment(&input(), &frames, &poses, raw_rows, &raw_profile()).unwrap();
        assert_eq!(selected.len(), DEPTH_FRAMES);
    }

    #[test]
    fn missing_projection_or_qpc_alias_is_fail_closed() {
        let (mut frames, poses, raw_rows) = aligned();
        frames[100].projection = None;
        let error =
            validate_alignment(&input(), &frames, &poses, raw_rows, &raw_profile()).unwrap_err();
        assert!(error.contains("physical projection"), "{error}");

        let (mut frames, poses, raw_rows) = aligned();
        frames[101].camera.qpc += 1;
        let error =
            validate_alignment(&input(), &frames, &poses, raw_rows, &raw_profile()).unwrap_err();
        assert!(error.contains("exact RGB Present/QPC"), "{error}");
    }

    #[test]
    fn camera_candidate_or_telemetry_pose_cannot_masquerade_as_cp_render_pose() {
        let (mut frames, mut poses, raw_rows) = aligned();
        frames[0].projection.as_mut().unwrap().candidate_admitted = false;
        assert!(validate_alignment(&input(), &frames, &poses, raw_rows, &raw_profile()).is_err());

        let (frames, _, raw_rows) = aligned();
        poses[0].camera_source = CameraPoseSource::GameTelemetry;
        assert!(validate_alignment(&input(), &frames, &poses, raw_rows, &raw_profile()).is_err());
    }

    #[test]
    fn metric_manifest_still_requires_two_agreeing_sources() {
        assert!(metric(false).validate().is_ok());
        let mut disagreeing = metric(true);
        disagreeing.cross_check = Some(CrossCheck::new(1.0, 1.2, 0.01));
        assert!(disagreeing.validate().is_err());
        assert!(metric(true).validate().is_ok());
    }

    #[test]
    fn generated_depth_action_has_only_even_frames_and_exact_names() {
        let (_, poses, _) = aligned();
        let full = build_action_camera_30fps(&poses).unwrap();
        let depth = build_depth_action_camera(&full).unwrap();
        let rows: Vec<Value> = serde_json::from_str(&depth).unwrap();
        assert_eq!(rows.len(), DEPTH_FRAMES);
        assert_eq!(rows[0]["frame"], 0);
        assert_eq!(rows[0]["depth_name"], "depth_0");
        assert_eq!(rows[449]["frame"], 898);
        assert_eq!(rows[449]["depth_name"], "depth_898");
    }

    #[test]
    fn emitted_exr_header_is_single_z_half_zip_720p() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("depth.exr");
        let mut buffer = vec![0.5_f32; DEPTH_WIDTH * DEPTH_HEIGHT];
        buffer[0] = 0.25;
        crate::exr_out::write_frame(
            &path,
            &buffer,
            DEPTH_WIDTH,
            DEPTH_HEIGHT,
            Projection::reversed_z_infinite(0.1, 1.0),
            Variant::Spec,
        )
        .unwrap();
        audit_exr_header(&fs::read(path).unwrap()).unwrap();
    }

    #[test]
    fn native_d32s8_is_exact_and_two_thirds_pixel_center_mapping_is_locked() {
        let mut source = vec![0.0_f32; SOURCE_DEPTH_WIDTH * SOURCE_DEPTH_HEIGHT];
        for y in 0..SOURCE_DEPTH_HEIGHT {
            for x in 0..SOURCE_DEPTH_WIDTH {
                source[y * SOURCE_DEPTH_WIDTH + x] = (y * SOURCE_DEPTH_WIDTH + x) as f32;
            }
        }
        let output = resample_native_depth_nearest(&source);
        assert_eq!(output.len(), DEPTH_WIDTH * DEPTH_HEIGHT);
        for (x, expected_x) in [(0, 0), (1, 2), (2, 3), (3, 5), (1279, 1919)] {
            assert_eq!(output[x], expected_x as f32);
        }
        assert_eq!(
            output[719 * DEPTH_WIDTH + 1279],
            (1079 * SOURCE_DEPTH_WIDTH + 1919) as f32
        );

        let mut legacy = raw_profile();
        legacy.selector.width = 1280;
        legacy.selector.height = 720;
        assert!(
            legacy.validate().is_err(),
            "legacy r26 geometry must stay closed"
        );

        let temp = tempfile::tempdir().unwrap();
        let profile_path = temp.path().join("raw-depth-demo-profile.json");
        let mut opaque_identity = raw_profile();
        opaque_identity.selector.candidate_id = "dsv-r26-g1-e1".into();
        fs::write(&profile_path, serde_json::to_vec(&opaque_identity).unwrap()).unwrap();
        let opaque_result = validate_raw_profile(&profile_path);
        assert!(
            opaque_result.is_ok(),
            "candidate ids are opaque identities; format 19/20 is pinned by its own fields: {opaque_result:?}"
        );

        for candidate_id in [
            "",
            "dsv-r19-g1",
            "dsv-r-g1-e1",
            "dsv-r19-g-e1",
            "dsv-r19-g1-e",
            "dsv-r19-g1-e1-extra",
            "DSV-r19-g1-e1",
        ] {
            let mut invalid = raw_profile();
            invalid.selector.candidate_id = candidate_id.into();
            fs::write(&profile_path, serde_json::to_vec(&invalid).unwrap()).unwrap();
            assert!(
                validate_raw_profile(&profile_path).is_err(),
                "non-canonical candidate id {candidate_id:?} must fail closed"
            );
        }

        let mut wrong_format = raw_profile();
        wrong_format.selector.resource_format = 18;
        fs::write(&profile_path, serde_json::to_vec(&wrong_format).unwrap()).unwrap();
        assert!(
            validate_raw_profile(&profile_path).is_err(),
            "resource/dsv format remains independently pinned to 19/20"
        );
    }

    #[test]
    fn paths_and_local_only_policy_are_not_relaxable() {
        let mut value = input();
        assert!(validate_input(&value).is_ok());
        value.upload_allowed = true;
        assert!(validate_input(&value).is_err());
        value.upload_allowed = false;
        value.video_file = "nested/video.mp4".into();
        assert!(validate_input(&value).is_err());
    }

    #[test]
    fn staging_is_published_by_one_rename_and_never_overwrites() {
        let temp = tempfile::tempdir().unwrap();
        let staging = temp.path().join("staging-a");
        let output = temp.path().join("package-a");
        fs::create_dir(&staging).unwrap();
        fs::write(staging.join("sealed"), b"ok").unwrap();
        publish_staging(&staging, &output).unwrap();
        assert!(!staging.exists());
        assert_eq!(fs::read(output.join("sealed")).unwrap(), b"ok");

        let staging = temp.path().join("staging-b");
        fs::create_dir(&staging).unwrap();
        let error = publish_staging(&staging, &output).unwrap_err();
        assert!(error.contains("appeared"), "{error}");
        assert!(staging.exists());
        assert_eq!(fs::read(output.join("sealed")).unwrap(), b"ok");
    }

    #[test]
    fn egeo_repeatability_is_bound_to_current_source_hashes_without_self_authorization() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("egeo.json");
        let digest = FileDigest {
            bytes: 1,
            sha256: format!("sha256:{}", "a".repeat(64)),
        };
        let sources = BTreeMap::from([
            ("video".into(), digest.clone()),
            ("raw_depth_index".into(), digest.clone()),
            ("raw_depth_profile".into(), digest.clone()),
            ("frame_evidence".into(), digest.clone()),
            ("pose".into(), digest.clone()),
            ("metric_evidence".into(), digest.clone()),
        ]);
        let mut raw_by_identity = BTreeMap::new();
        let mut bound = Vec::new();
        let mut consumed = Vec::new();
        fs::create_dir(temp.path().join("raw-depth")).unwrap();
        for ordinal in 0..31_u64 {
            let identity = PresentIdentity {
                present_seq: ordinal + 1,
                qpc: 1_000 + ordinal as i64,
            };
            let row = raw(ordinal, identity);
            fs::write(
                temp.path().join("raw-depth").join(&row.file),
                [ordinal as u8],
            )
            .unwrap();
            let manifest_row = serde_json::json!({
                "ordinal": ordinal,
                "file": row.file.clone(),
                "present_seq": identity.present_seq,
                "qpc": identity.qpc,
                "sha256": digest_file(&temp.path().join("raw-depth").join(format!("{ordinal:012}.d32"))).unwrap().sha256
            });
            bound.push(manifest_row.clone());
            if ordinal < 30 {
                consumed.push(manifest_row);
            }
            raw_by_identity.insert((identity.present_seq, identity.qpc), row);
        }
        let report = serde_json::json!({
            "schema": "dm-cp2077-camera-egeo-evidence/v1",
            "non_trainable": true,
            "local_only": true,
            "upload_allowed": false,
            "e_geo": {
                "schema": "camera-egeo-validation/v3",
                "passed": true,
                "ready_for_independent_repeat": true,
                "production_binding_authorized": false,
                "failures": [],
                "video_sha256": digest.sha256,
                "raw_depth_index_sha256": digest.sha256,
                "raw_depth_profile_sha256": digest.sha256,
                "frame_evidence_sha256": digest.sha256,
                "pose_sha256": digest.sha256,
                "metric_evidence_sha256": digest.sha256,
                "tool_sha256": sha256_bytes(CP2077_EGEO_TOOL),
                "geo_probes_sha256": sha256_bytes(GEO_PROBES_TOOL),
                "source_depth_geometry": {
                    "width": 1920,
                    "height": 1080,
                    "resource_format": 19,
                    "dsv_format": 20,
                    "sample_count": 1,
                    "extracted_pixel_format": "DXGI_FORMAT_D32_FLOAT_LE"
                },
                "output_depth_geometry": {"width": 1280, "height": 720},
                "resampling": {
                    "method": "pixel_center_nearest_2_over_3",
                    "integer_mapping": "src=floor((3*dst+1)/2)",
                    "intrinsics_scaling": "fx_fy_cx_cy_times_2_over_3"
                },
                "sampled_pairs": {"static": 5, "translation": 5, "rotation": 30},
                "depth_byte_binding": "all_450_indexed_depth_files",
                "bound_depth_files": bound,
                "usable_motion_pairs": 30,
                "invalid_metric_pairs": 0,
                "consumed_depth_files": consumed,
                "metrics": {
                    "improvement_p50": 2.0,
                    "inverse_improvement_p50": 1.0,
                    "improved_share": 0.9,
                    "valid_frac_p10": 0.8,
                    "static_baseline_l1_p50": 0.2,
                    "moving_baseline_l1_p50": 2.0
                }
            }
        });
        fs::write(&path, serde_json::to_vec(&report).unwrap()).unwrap();
        assert!(validate_egeo(&path, &sources, temp.path(), &raw_by_identity).is_ok());

        let mut self_authorizing = report.clone();
        self_authorizing["e_geo"]["production_binding_authorized"] = serde_json::json!(true);
        fs::write(&path, serde_json::to_vec(&self_authorizing).unwrap()).unwrap();
        assert!(
            validate_egeo(&path, &sources, temp.path(), &raw_by_identity)
                .unwrap_err()
                .contains("must not self-authorize")
        );

        let mut not_repeatable = report.clone();
        not_repeatable["e_geo"]["ready_for_independent_repeat"] = serde_json::json!(false);
        fs::write(&path, serde_json::to_vec(&not_repeatable).unwrap()).unwrap();
        assert!(validate_egeo(&path, &sources, temp.path(), &raw_by_identity).is_err());

        fs::write(&path, serde_json::to_vec(&report).unwrap()).unwrap();
        let mut stale = sources.clone();
        stale.get_mut("pose").unwrap().sha256 = format!("sha256:{}", "b".repeat(64));
        assert!(validate_egeo(&path, &stale, temp.path(), &raw_by_identity).is_err());
        fs::write(
            temp.path().join("raw-depth").join("000000000030.d32"),
            b"changed outside the E-geo sample",
        )
        .unwrap();
        assert!(
            validate_egeo(&path, &sources, temp.path(), &raw_by_identity)
                .unwrap_err()
                .contains("changed after validation")
        );
    }
}

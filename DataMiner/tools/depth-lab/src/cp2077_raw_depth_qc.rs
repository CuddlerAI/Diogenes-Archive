//! Offline, memory-bounded QC for the local-only CP2077 raw D32 demo.
//!
//! This validates bytes that actually reached disk. It cannot promote the lane to production:
//! projection, metric scale, camera/player pose, E-geo and Loop remain independent gates.

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{BufRead as _, BufReader, Read as _};
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

pub const CP2077_RAW_DEPTH_QC_SCHEMA_V1: &str = "dm-cp2077-local-raw-depth-qc/v1";
const INDEX_SCHEMA_V1: &str = "dm-cp2077-raw-depth-frame-index/v1";
const INDEX_FILE: &str = "raw-depth-index.jsonl";
const FRAME_DIR: &str = "raw-depth";
const MAX_INDEX_BYTES: u64 = 4 * 1024 * 1024;
const MAX_INDEX_LINE_BYTES: usize = 16 * 1024;
const HARD_MAX_FRAMES: usize = 1_024;
const MANUAL_STOP_MAX_FRAMES: usize = 4_096;
const PIXEL_EPSILON: f32 = 1.0e-6;
const MIN_FRAME_RANGE: f32 = 1.0e-5;
const SAMPLE_STEP_X: usize = 16;
const SAMPLE_STEP_Y: usize = 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RawDepthQcContract {
    ShortSmoke,
    Pr405Local30Seconds,
    CoordinatedDemo,
    ManualStopDemo,
}

impl RawDepthQcContract {
    const fn label(self) -> &'static str {
        match self {
            Self::ShortSmoke => "short_smoke",
            Self::Pr405Local30Seconds => "pr405_local_29_31s_demo",
            Self::CoordinatedDemo => "coordinated_63_65s_demo",
            Self::ManualStopDemo => "manual_stop_60_240s_demo",
        }
    }

    const fn frame_bounds(self) -> (usize, usize) {
        match self {
            Self::ShortSmoke => (2, HARD_MAX_FRAMES),
            Self::Pr405Local30Seconds => (450, 450),
            Self::CoordinatedDemo => (900, HARD_MAX_FRAMES),
            Self::ManualStopDemo => (900, MANUAL_STOP_MAX_FRAMES),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct IndexRow {
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

#[derive(Debug, Serialize)]
pub struct RawDepthQcReport {
    pub schema: &'static str,
    pub contract: &'static str,
    pub raw_depth_index_sha256: String,
    pub passed: bool,
    pub frame_count: usize,
    pub width: usize,
    pub height: usize,
    pub row_pitch: usize,
    pub total_pixels: u64,
    pub finite_pixels: u64,
    pub non_finite_pixels: u64,
    pub below_zero_pixels: u64,
    pub above_one_pixels: u64,
    pub global_min: Option<f32>,
    pub global_max: Option<f32>,
    pub non_constant_frames: usize,
    pub unique_frame_signatures: usize,
    pub adjacent_pairs_with_sample_change: usize,
    pub adjacent_pair_count: usize,
    pub changed_sample_ratio: f64,
    pub first_present_seq: Option<u64>,
    pub last_present_seq: Option<u64>,
    pub first_present_qpc: Option<i64>,
    pub last_present_qpc: Option<i64>,
    pub max_readback_lag_presents: u64,
    pub source_candidate_id: Option<String>,
    pub runtime_candidate_id: Option<String>,
    pub blockers: Vec<String>,
    pub non_trainable: bool,
    pub production_certified: bool,
    pub upload_allowed: bool,
    pub deliverable: bool,
    pub recording_ready: bool,
}

#[derive(Default)]
struct PixelAudit {
    pixels: u64,
    finite: u64,
    non_finite: u64,
    below_zero: u64,
    above_one: u64,
    min: Option<f32>,
    max: Option<f32>,
    signature: u64,
    samples: Vec<f32>,
}

pub fn audit_cp2077_raw_depth_session(
    session_dir: &Path,
    contract: RawDepthQcContract,
) -> Result<RawDepthQcReport, String> {
    if !session_dir.is_absolute() {
        return Err("raw depth QC session path must be absolute".into());
    }
    let session = session_dir
        .canonicalize()
        .map_err(|error| format!("canonicalize raw depth QC session failed: {error}"))?;
    let index_path = session.join(INDEX_FILE);
    let metadata = index_path
        .metadata()
        .map_err(|error| format!("read raw depth index metadata failed: {error}"))?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_INDEX_BYTES {
        return Err("raw depth index is not a bounded regular file".into());
    }
    let (min_frames, max_frames) = contract.frame_bounds();
    let rows = read_index(&index_path, max_frames)?;
    let raw_depth_index_sha256 = sha256_file(&index_path)?;
    let mut blockers = Vec::new();
    if !(min_frames..=max_frames).contains(&rows.len()) {
        blockers.push(format!(
            "frame_count_out_of_contract:{} not in {min_frames}..={max_frames}",
            rows.len()
        ));
    }

    let frame_root = session.join(FRAME_DIR);
    let canonical_frame_root = frame_root
        .canonicalize()
        .map_err(|error| format!("canonicalize raw depth frame directory failed: {error}"))?;
    let mut expected_geometry = None;
    let mut previous_seq = None;
    let mut previous_qpc = None;
    let mut previous_samples: Option<Vec<f32>> = None;
    let mut signatures = BTreeSet::new();
    let mut total_pixels = 0u64;
    let mut finite_pixels = 0u64;
    let mut non_finite_pixels = 0u64;
    let mut below_zero_pixels = 0u64;
    let mut above_one_pixels = 0u64;
    let mut global_min: Option<f32> = None;
    let mut global_max: Option<f32> = None;
    let mut non_constant_frames = 0usize;
    let mut adjacent_pairs_with_change = 0usize;
    let mut changed_samples = 0u64;
    let mut compared_samples = 0u64;
    let mut max_lag = 0u64;
    let mut source_candidate: Option<String> = None;
    let mut runtime_candidate: Option<String> = None;

    for (position, row) in rows.iter().enumerate() {
        validate_row(row, position, &mut blockers);
        let geometry = (row.width, row.height, row.row_pitch);
        if expected_geometry.is_none() {
            expected_geometry = Some(geometry);
        } else if expected_geometry != Some(geometry) {
            blockers.push(format!("frame_geometry_drift_at_ordinal:{}", row.ordinal));
        }
        if previous_seq.is_some_and(|value| row.present_seq <= value) {
            blockers.push(format!(
                "present_seq_not_strictly_increasing_at:{}",
                row.ordinal
            ));
        }
        if previous_qpc.is_some_and(|value| row.present_qpc <= value) {
            blockers.push(format!(
                "present_qpc_not_strictly_increasing_at:{}",
                row.ordinal
            ));
        }
        previous_seq = Some(row.present_seq);
        previous_qpc = Some(row.present_qpc);
        max_lag = max_lag.max(row.readback_lag_presents);
        bind_identity(
            &mut source_candidate,
            &row.source_candidate_id,
            "source_candidate_id",
            row.ordinal,
            &mut blockers,
        );
        bind_identity(
            &mut runtime_candidate,
            &row.runtime_candidate_id,
            "runtime_candidate_id",
            row.ordinal,
            &mut blockers,
        );

        let expected_name = format!("{:012}.d32", row.ordinal);
        if row.file != expected_name {
            blockers.push(format!(
                "noncanonical_frame_name_at_ordinal:{}",
                row.ordinal
            ));
            continue;
        }
        let path = frame_root.join(&row.file);
        let canonical = match path.canonicalize() {
            Ok(value) => value,
            Err(error) => {
                blockers.push(format!("frame_missing_at_ordinal:{}:{error}", row.ordinal));
                continue;
            }
        };
        if canonical.parent() != Some(canonical_frame_root.as_path()) {
            blockers.push(format!("frame_escaped_session_at_ordinal:{}", row.ordinal));
            continue;
        }
        let audit = match audit_frame(&canonical, row) {
            Ok(value) => value,
            Err(error) => {
                blockers.push(format!("frame_invalid_at_ordinal:{}:{error}", row.ordinal));
                continue;
            }
        };
        total_pixels = total_pixels.saturating_add(audit.pixels);
        finite_pixels = finite_pixels.saturating_add(audit.finite);
        non_finite_pixels = non_finite_pixels.saturating_add(audit.non_finite);
        below_zero_pixels = below_zero_pixels.saturating_add(audit.below_zero);
        above_one_pixels = above_one_pixels.saturating_add(audit.above_one);
        merge_min(&mut global_min, audit.min);
        merge_max(&mut global_max, audit.max);
        if audit
            .min
            .zip(audit.max)
            .is_some_and(|(min, max)| max - min >= MIN_FRAME_RANGE)
        {
            non_constant_frames += 1;
        }
        signatures.insert(audit.signature);
        if let Some(previous) = &previous_samples {
            let mut pair_changed = false;
            for (left, right) in previous.iter().zip(&audit.samples) {
                compared_samples = compared_samples.saturating_add(1);
                if left.is_finite() && right.is_finite() && (left - right).abs() > PIXEL_EPSILON {
                    changed_samples = changed_samples.saturating_add(1);
                    pair_changed = true;
                }
            }
            if pair_changed {
                adjacent_pairs_with_change += 1;
            }
        }
        previous_samples = Some(audit.samples);
    }

    let pair_count = rows.len().saturating_sub(1);
    let changed_ratio = if compared_samples == 0 {
        0.0
    } else {
        changed_samples as f64 / compared_samples as f64
    };
    if non_finite_pixels != 0 {
        blockers.push(format!("non_finite_depth_pixels:{non_finite_pixels}"));
    }
    if below_zero_pixels != 0 || above_one_pixels != 0 {
        blockers.push(format!(
            "depth_pixels_outside_normalized_range:below={below_zero_pixels},above={above_one_pixels}"
        ));
    }
    if global_min
        .zip(global_max)
        .is_none_or(|(min, max)| max - min < MIN_FRAME_RANGE)
    {
        blockers.push("depth_global_range_is_constant".into());
    }
    if rows.len() >= 2 && signatures.len() < 2 {
        blockers.push("all_depth_frame_signatures_are_identical".into());
    }
    if pair_count > 0 && adjacent_pairs_with_change * 4 < pair_count {
        blockers.push(format!(
            "too_few_adjacent_depth_changes:{adjacent_pairs_with_change}/{pair_count}"
        ));
    }
    if pair_count > 0 && changed_ratio < 0.001 {
        blockers.push(format!(
            "depth_sample_change_ratio_too_low:{changed_ratio:.8}"
        ));
    }
    if !rows.is_empty() && non_constant_frames * 10 < rows.len() * 9 {
        blockers.push(format!(
            "too_many_spatially_constant_depth_frames:{non_constant_frames}/{}",
            rows.len()
        ));
    }

    let (width, height, row_pitch) = expected_geometry.unwrap_or_default();
    Ok(RawDepthQcReport {
        schema: CP2077_RAW_DEPTH_QC_SCHEMA_V1,
        contract: contract.label(),
        raw_depth_index_sha256,
        passed: blockers.is_empty(),
        frame_count: rows.len(),
        width,
        height,
        row_pitch,
        total_pixels,
        finite_pixels,
        non_finite_pixels,
        below_zero_pixels,
        above_one_pixels,
        global_min,
        global_max,
        non_constant_frames,
        unique_frame_signatures: signatures.len(),
        adjacent_pairs_with_sample_change: adjacent_pairs_with_change,
        adjacent_pair_count: pair_count,
        changed_sample_ratio: changed_ratio,
        first_present_seq: rows.first().map(|row| row.present_seq),
        last_present_seq: rows.last().map(|row| row.present_seq),
        first_present_qpc: rows.first().map(|row| row.present_qpc),
        last_present_qpc: rows.last().map(|row| row.present_qpc),
        max_readback_lag_presents: max_lag,
        source_candidate_id: source_candidate,
        runtime_candidate_id: runtime_candidate,
        blockers,
        non_trainable: true,
        production_certified: false,
        upload_allowed: false,
        deliverable: false,
        recording_ready: false,
    })
}

fn read_index(path: &Path, max_frames: usize) -> Result<Vec<IndexRow>, String> {
    let reader = BufReader::new(
        File::open(path).map_err(|error| format!("open raw depth index failed: {error}"))?,
    );
    let mut rows = Vec::new();
    for line in reader.lines() {
        let line = line.map_err(|error| format!("read raw depth index failed: {error}"))?;
        if line.len() > MAX_INDEX_LINE_BYTES {
            return Err("raw depth index line exceeds bound".into());
        }
        if line.trim().is_empty() {
            return Err("raw depth index contains an empty line".into());
        }
        rows.push(
            serde_json::from_str(&line)
                .map_err(|error| format!("parse raw depth index failed: {error}"))?,
        );
        if rows.len() > max_frames {
            return Err("raw depth index exceeds frame bound".into());
        }
    }
    Ok(rows)
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file = File::open(path)
        .map_err(|error| format!("open raw depth index hash input failed: {error}"))?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 1024 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("read raw depth index hash input failed: {error}"))?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(format!("sha256:{:x}", digest.finalize()))
}

fn validate_row(row: &IndexRow, position: usize, blockers: &mut Vec<String>) {
    if row.schema != INDEX_SCHEMA_V1 {
        blockers.push(format!("index_schema_mismatch_at_ordinal:{}", row.ordinal));
    }
    if row.ordinal != position as u64 {
        blockers.push(format!("index_ordinal_gap_at_position:{position}"));
    }
    if row.width == 0
        || row.height == 0
        || row.row_pitch < row.width.saturating_mul(4)
        || row.byte_len != row.row_pitch.saturating_mul(row.height)
    {
        blockers.push(format!("index_geometry_invalid_at_ordinal:{}", row.ordinal));
    }
    if row.pixel_format != "DXGI_FORMAT_D32_FLOAT_LE" {
        blockers.push(format!("pixel_format_mismatch_at_ordinal:{}", row.ordinal));
    }
    if row.rgb_pairing != "same_present_seq_gpu_watermark_submitted" {
        blockers.push(format!(
            "rgb_pairing_contract_mismatch_at_ordinal:{}",
            row.ordinal
        ));
    }
    if row.current_present_seq < row.present_seq
        || row.current_present_seq - row.present_seq != row.readback_lag_presents
        || row.present_seq < row.arm_start_present_seq
        || row.present_qpc < row.arm_request_observed_qpc
        || row.capture_id == 0
    {
        blockers.push(format!("frame_identity_invalid_at_ordinal:{}", row.ordinal));
    }
}

fn bind_identity(
    bound: &mut Option<String>,
    value: &str,
    label: &str,
    ordinal: u64,
    blockers: &mut Vec<String>,
) {
    if value.is_empty() {
        blockers.push(format!("{label}_missing_at_ordinal:{ordinal}"));
    } else if let Some(existing) = bound {
        if existing != value {
            blockers.push(format!("{label}_drift_at_ordinal:{ordinal}"));
        }
    } else {
        *bound = Some(value.to_owned());
    }
}

fn audit_frame(path: &Path, row: &IndexRow) -> Result<PixelAudit, String> {
    let metadata = path
        .metadata()
        .map_err(|error| format!("read frame metadata failed: {error}"))?;
    if !metadata.is_file() || metadata.len() != row.byte_len as u64 {
        return Err("frame byte length does not match index".into());
    }
    let mut reader =
        BufReader::new(File::open(path).map_err(|error| format!("open frame failed: {error}"))?);
    let mut bytes = vec![0u8; row.row_pitch];
    let mut audit = PixelAudit {
        signature: 0xcbf2_9ce4_8422_2325,
        samples: Vec::with_capacity(
            row.width.div_ceil(SAMPLE_STEP_X) * row.height.div_ceil(SAMPLE_STEP_Y),
        ),
        ..PixelAudit::default()
    };
    for y in 0..row.height {
        reader
            .read_exact(&mut bytes)
            .map_err(|error| format!("read frame row failed: {error}"))?;
        for x in 0..row.width {
            let offset = x * 4;
            let bits = u32::from_le_bytes(
                bytes[offset..offset + 4]
                    .try_into()
                    .expect("four bytes were bounds checked"),
            );
            let value = f32::from_bits(bits);
            audit.pixels = audit.pixels.saturating_add(1);
            audit.signature ^= u64::from(bits);
            audit.signature = audit.signature.wrapping_mul(0x100_0000_01b3);
            if value.is_finite() {
                audit.finite = audit.finite.saturating_add(1);
                if value < 0.0 {
                    audit.below_zero = audit.below_zero.saturating_add(1);
                } else if value > 1.0 {
                    audit.above_one = audit.above_one.saturating_add(1);
                }
                merge_min(&mut audit.min, Some(value));
                merge_max(&mut audit.max, Some(value));
            } else {
                audit.non_finite = audit.non_finite.saturating_add(1);
            }
            if x % SAMPLE_STEP_X == 0 && y % SAMPLE_STEP_Y == 0 {
                audit.samples.push(value);
            }
        }
    }
    Ok(audit)
}

fn merge_min(target: &mut Option<f32>, value: Option<f32>) {
    if let Some(value) = value {
        *target = Some(target.map_or(value, |current| current.min(value)));
    }
}

fn merge_max(target: &mut Option<f32>, value: Option<f32>) {
    if let Some(value) = value {
        *target = Some(target.map_or(value, |current| current.max(value)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    fn write_session(frames: usize, changing: bool) -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        let frame_dir = root.path().join(FRAME_DIR);
        std::fs::create_dir(&frame_dir).unwrap();
        let mut index = File::create(root.path().join(INDEX_FILE)).unwrap();
        for ordinal in 0..frames {
            let name = format!("{ordinal:012}.d32");
            let values = if changing {
                [ordinal as f32 * 0.01, 0.25, 0.5, 1.0]
            } else {
                [0.0f32, 0.25, 0.5, 1.0]
            };
            let mut frame = File::create(frame_dir.join(&name)).unwrap();
            for value in values {
                frame.write_all(&value.to_le_bytes()).unwrap();
            }
            serde_json::to_writer(
                &mut index,
                &serde_json::json!({
                    "schema": INDEX_SCHEMA_V1,
                    "ordinal": ordinal,
                    "file": name,
                    "present_seq": 100 + ordinal,
                    "present_qpc": 1000 + ordinal * 10,
                    "current_present_seq": 101 + ordinal,
                    "readback_lag_presents": 1,
                    "capture_id": 1 + ordinal,
                    "source_candidate_id": "dsv-r31-g1-e1",
                    "runtime_candidate_id": "dsv-r42-g1-e1",
                    "arm_request_observed_qpc": 900,
                    "arm_start_present_seq": 100,
                    "width": 4,
                    "height": 1,
                    "row_pitch": 16,
                    "pixel_format": "DXGI_FORMAT_D32_FLOAT_LE",
                    "byte_len": 16,
                    "rgb_pairing": "same_present_seq_gpu_watermark_submitted"
                }),
            )
            .unwrap();
            writeln!(index).unwrap();
        }
        root
    }

    #[test]
    fn changing_normalized_d32_short_smoke_passes_without_upgrading_readiness() {
        let root = write_session(4, true);
        let report =
            audit_cp2077_raw_depth_session(root.path(), RawDepthQcContract::ShortSmoke).unwrap();
        assert!(report.passed, "{:?}", report.blockers);
        assert_eq!(report.unique_frame_signatures, 4);
        assert!(!report.recording_ready && report.non_trainable);
    }

    #[test]
    fn identical_or_nonfinite_frames_fail_closed() {
        let root = write_session(3, false);
        let report =
            audit_cp2077_raw_depth_session(root.path(), RawDepthQcContract::ShortSmoke).unwrap();
        assert!(!report.passed);
        assert!(
            report
                .blockers
                .iter()
                .any(|value| value == "all_depth_frame_signatures_are_identical")
        );

        let path = root.path().join(FRAME_DIR).join("000000000001.d32");
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[..4].copy_from_slice(&f32::NAN.to_le_bytes());
        std::fs::write(path, bytes).unwrap();
        let report =
            audit_cp2077_raw_depth_session(root.path(), RawDepthQcContract::ShortSmoke).unwrap();
        assert!(report.non_finite_pixels > 0);
        assert!(!report.passed);
    }

    #[test]
    fn manual_stop_qc_accepts_more_than_the_coordinated_frame_bound() {
        let root = write_session(HARD_MAX_FRAMES + 1, true);
        let report =
            audit_cp2077_raw_depth_session(root.path(), RawDepthQcContract::ManualStopDemo)
                .unwrap();
        assert_eq!(report.frame_count, HARD_MAX_FRAMES + 1);
        assert!(report.raw_depth_index_sha256.starts_with("sha256:"));
    }
}

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::spec::RationalRate;

pub const REQUEST_SCHEMA: &str = "dm-normalization-request-1";
pub const ARTIFACT_SCHEMA: &str = "dm-normalized-dataset-1";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NormalizationRequest {
    pub schema_version: String,
    pub session_id: String,
    pub source_fps: RationalRate,
    pub source_width: u32,
    pub source_height: u32,
    pub mapping_path: PathBuf,
    pub runs: Vec<RequestRun>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RequestRun {
    pub run_id: String,
    pub qpc_anchor: i64,
    pub qpc_frequency: i64,
    pub segments: Vec<RequestSegment>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RequestSegment {
    pub timing_path: PathBuf,
    pub events_path: PathBuf,
    pub video_path: PathBuf,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SourceFrameTiming {
    pub frame: u64,
    pub session_frame: u64,
    pub output_qpc_start: i64,
    pub output_qpc_end: i64,
    pub source_qpc: i64,
    pub source_seq: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub present_seq: Option<u64>,
    pub duplicate: bool,
    pub source_superseded: u32,
    pub queue_drop: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FramePlanRow {
    pub out_frame: u64,
    pub run_id: String,
    pub run_frame: u64,
    pub qpc_start: i64,
    pub qpc_end: i64,
    pub source_frame: u64,
    pub source_segment: u32,
    pub source_segment_frame: u64,
    pub source_qpc: i64,
    pub source_seq: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub present_seq: Option<u64>,
    pub duplicate: bool,
    pub source_superseded: u32,
    pub trim: bool,
    pub trimmed_tail_source_frames: u32,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NormalizedEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vk: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scan_code: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flags: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dx: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dy: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delta: Option<i16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub horizontal: Option<bool>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NormalizedLabelRow {
    pub out_frame: u64,
    pub run_id: String,
    pub run_frame: u64,
    pub video_t_us: u64,
    pub qpc_start: i64,
    pub qpc_end: i64,
    pub source_frame: u64,
    pub source_qpc: i64,
    pub source_seq: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub present_seq: Option<u64>,
    pub duplicate: bool,
    pub keys: Vec<u16>,
    pub buttons: Vec<u16>,
    pub actions: Vec<String>,
    #[serde(rename = "pointerDx")]
    pub pointer_dx: i32,
    #[serde(rename = "pointerDy")]
    pub pointer_dy: i32,
    #[serde(rename = "pointerRawDx")]
    pub pointer_raw_dx: i64,
    #[serde(rename = "pointerRawDy")]
    pub pointer_raw_dy: i64,
    #[serde(rename = "pointerBins")]
    pub pointer_bins: [i32; 2],
    pub events: Vec<NormalizedEvent>,
    pub src: String,
    pub normalization_spec_sha256: String,
    pub frame_plan_sha256: String,
}

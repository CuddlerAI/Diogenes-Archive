use std::collections::HashMap;
use std::fs;
use std::path::{Component, Path};

use anyhow::{Context, ensure};
use capture_core::audio_contract::{
    AUDIO_ARTIFACT_NAME, SegmentAudioMetadata, expected_manifest_payloads,
};
#[cfg(test)]
use capture_core::ingest_contract::RECORDING_SCHEMA_V2;
use capture_core::ingest_contract::SessionContract;
use capture_core::mapping::MappingSnapshot;
use capture_core::profile::CaptureProfile;
use capture_core::qpc::{frame_index_at_qpc, frame_pts_100ns};
use capture_core::recording::{PublishedSegment, RecordingOutcome, RecordingStopReason};
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
#[cfg(test)]
use sha2::{Digest, Sha256};

use crate::domain::collector::{HeartbeatAudit, MarkerAudit, MarkerKind, PreflightStatus};
use crate::platform::windows::catalog::CatalogSelection;
use crate::services::preflight;

pub(crate) const JOURNAL_SCHEMA_VERSION: i64 = 34;
const MANUALLY_RECOVERABLE_UPLOAD_ERROR_CODE: &str = "UPLOAD_REQUEST_REJECTED";
const LEGACY_PROFILE_MISMATCH_SERVER_CODE: &str = "device_profile_mismatch";
const MISSING_CATALOG_VERSION_SERVER_CODE: &str = "catalog_version_not_found";
pub(crate) const DATA_MODEL_SOURCE_CORRUPT_CODE: &str = "DATAMODEL_SOURCE_CORRUPT";
const BUSY_TIMEOUT_MS: u64 = 5_000;

pub struct SessionJournal {
    connection: Mutex<Connection>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DataModelJobState {
    Waiting,
    Pending,
    Running,
    Finalizing,
    Completed,
    Failed,
    Cancelled,
}

impl DataModelJobState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Waiting => "waiting",
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Finalizing => "finalizing",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    fn parse(value: &str) -> anyhow::Result<Self> {
        match value {
            "waiting" => Ok(Self::Waiting),
            "pending" => Ok(Self::Pending),
            "running" => Ok(Self::Running),
            "finalizing" => Ok(Self::Finalizing),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            _ => anyhow::bail!("unsupported DataModel job state {value}"),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DataModelJobStatus {
    pub session_id: String,
    pub state: DataModelJobState,
    pub ready_segments: u32,
    pub processed_segments: u32,
    pub published_chunks: u32,
    pub published_frames: u64,
    pub pending_frames: u64,
    pub finalize_requested: bool,
    pub output_relative_path: Option<String>,
    pub error_code: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DataModelReadySegmentRef {
    pub session_id: String,
    pub run_name: String,
    pub run_seq: u32,
    pub segment_index: u32,
    pub session_frame_start: u64,
    pub frame_count: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DataModelWallClockContext {
    pub anchor_at_utc: Option<String>,
    pub anchor_qpc: i64,
    pub qpc_frequency: i64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DataModelProgress {
    pub processed_segments: u32,
    pub published_chunks: u32,
    pub published_frames: u64,
    pub pending_frames: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UploadJobState {
    Pending,
    Running,
    RetryWait,
    Stored,
    Verified,
    Rejected,
}

impl UploadJobState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::RetryWait => "retry_wait",
            Self::Stored => "stored",
            Self::Verified => "verified",
            Self::Rejected => "rejected",
        }
    }

    fn parse(value: &str) -> anyhow::Result<Self> {
        match value {
            "pending" => Ok(Self::Pending),
            "running" => Ok(Self::Running),
            "retry_wait" => Ok(Self::RetryWait),
            "stored" => Ok(Self::Stored),
            "verified" => Ok(Self::Verified),
            "rejected" => Ok(Self::Rejected),
            _ => anyhow::bail!("unsupported upload job state {value}"),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UploadJob {
    pub segment_id: String,
    pub session_id: String,
    pub run_name: String,
    pub segment_index: u32,
    pub state: UploadJobState,
    pub attempt_count: u32,
    pub status_attempt_count: u32,
    pub remote_clip_id: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UploadFailureDetails<'a> {
    pub error_code: &'a str,
    pub http_status: Option<u16>,
    pub server_code: Option<&'a str>,
    pub server_message: Option<&'a str>,
    pub request_id: Option<&'a str>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionSealJob {
    pub local_session_id: String,
    pub remote_session_id: String,
    pub ended_at_utc: String,
    pub local_final_state: String,
    pub stop_reason: Option<String>,
    pub attempt_count: u32,
}

/// 上传两条队列（segment 与 session artifact）的聚合读数，供
/// `CollectorSnapshot.upload` 展示（docs/65 J-29）。只读聚合，不改任何状态。
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct UploadQueueStatus {
    /// `pending` + `retry_wait` 的分段任务数。
    pub pending_segments: u32,
    /// `running` 的分段任务数。
    pub inflight_segments: u32,
    /// `stored`（字节已入 R2、等服务端复验）的分段任务数。
    pub awaiting_verification_segments: u32,
    /// `rejected` 终态分段任务数。
    pub rejected_segments: u32,
    /// 未到终态（verified/rejected）的会话级 artifact 任务数。
    pub pending_session_artifacts: u32,
    /// 两张表里最近一次记下的失败码（按 `updated_at_utc` 取最新）。
    pub last_error_code: Option<String>,
    /// 退避中的任务里最早的下一次尝试时刻（Unix ms）。
    pub next_retry_unix_ms: Option<i64>,
}

#[derive(Clone, Debug)]
pub struct UploadArtifactSource {
    pub kind: &'static str,
    pub relative_path: String,
    pub expected_sha256: Option<String>,
}

#[derive(Clone, Debug)]
pub struct UploadCandidate {
    pub segment_id: String,
    pub local_session_id: String,
    pub session_registration_state: String,
    pub session_state: String,
    pub session_schema_version: String,
    pub session_started_at_utc: String,
    pub session_ended_at_utc: Option<String>,
    pub session_stop_reason: Option<String>,
    /// Always `true` since the certified lane was removed (docs/100); the v27
    /// schema CHECK pins it. Trainability is only ever a server-side verdict.
    pub non_trainable: bool,
    /// The consent id frozen into the session at recording time. Nothing
    /// populates it today (docs/100 §3 推论 3) — the channel is kept as the
    /// carrier for the next cut. Declared to the server as `declaredConsentId`.
    pub consent_id: Option<String>,
    pub session_contract: String,
    pub catalog_id: String,
    pub catalog_version: String,
    pub catalog_hash: String,
    pub device_profile_id: String,
    pub device_profile_version: String,
    pub device_profile_hash: String,
    pub recording_profile_id: String,
    pub recording_profile_version: String,
    pub recording_profile_hash: String,
    pub game_id: String,
    pub game_profile_version: String,
    pub game_profile_hash: String,
    pub game_executable_sha256: Option<String>,
    pub game_executable_sha256_missing_reason: Option<String>,
    pub mapping_id: String,
    pub mapping_version: String,
    pub mapping_hash: String,
    pub remote_session_id: Option<String>,
    pub run_journal_id: String,
    pub local_run_id: String,
    pub run_registration_state: String,
    pub run_sequence: u32,
    pub run_started_at_utc: String,
    pub run_ended_at_utc: String,
    pub qpc_frequency: i64,
    pub session_frame_start: u64,
    pub preflight_id: String,
    pub preflight_result: String,
    pub preflight_evidence_hash: String,
    pub capture_engine: String,
    pub encoder_backend: String,
    pub width: u32,
    pub height: u32,
    pub fps_num: u32,
    pub fps_den: u32,
    pub remote_run_id: Option<String>,
    /// Run outcome facts (docs/67 §3.2). The journal has had them since v1;
    /// they simply never left the machine, so the server-side columns stayed
    /// NULL and the console rendered "—" for every run ever recorded.
    pub run_frame_count: Option<u64>,
    pub run_stop_reason: Option<String>,
    pub run_error_code: Option<String>,
    pub segment_index: u32,
    pub segment_session_frame_start: u64,
    pub frame_count: u64,
    pub qpc_start: i64,
    pub qpc_end: i64,
    pub event_count: u64,
    pub duplicate_frames: u64,
    pub source_superseded: u64,
    pub source_drops: u64,
    pub remote_clip_id: Option<String>,
    pub clip_revision: Option<u32>,
    pub remote_upload_intent_id: Option<String>,
    pub audio: Option<SegmentAudioMetadata>,
    pub artifacts: Vec<UploadArtifactSource>,
}

/// journal 中已经 ready 的客户端段边界。depth finalizer 只可使用这里的精确区间，
/// 不得由文件行号、固定段长或时间戳反推。
#[cfg(any(feature = "depth-lab", test))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DepthSegmentBoundary {
    pub segment_index: u32,
    pub session_frame_start: u64,
    pub frame_count: u64,
}

/// staging 目录原子提交成功后，写入 journal 的本地完成事实。
#[cfg(any(feature = "depth-lab", test))]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DepthSegmentCommit {
    pub segment_index: u32,
    pub generation: u64,
    /// 相对 run directory 的独立 depth segment manifest 路径。
    pub manifest_relative_path: String,
    pub manifest_sha256: String,
    /// 相对 session root 的真实 ZIP。只有合格段才有，不能拿 manifest 冒充像素产物。
    pub upload_relative_path: Option<String>,
    pub upload_sha256: Option<String>,
    pub candidate_eligible: bool,
    pub invalid_frame_count: u32,
}

/// CP2077 DEV raw-depth is deliberately not a production depth upload. This
/// fact joins its local five-stream seal to the formal main collector run
/// without creating, rejecting, or otherwise mutating any R2 upload job.
#[cfg(any(all(feature = "depth-lab", feature = "dev-catalog"), test))]
pub struct Cp2077MainRunCommit<'a> {
    pub session_id: &'a str,
    pub run_id: &'a str,
    pub generation: u64,
    pub process_id: u32,
    pub process_launch_id: &'a str,
    pub result_relative_path: &'a str,
    pub result_sha256: &'a str,
    pub sidecar_locator: &'a str,
    pub delivery_locator: &'a str,
    pub rgb_frames: u64,
    pub source_depth_frames: u64,
    pub paired_depth_frames: u64,
    pub duration_ms: u64,
    pub capture_queue_drops: u64,
    pub input_queue_overflows: u64,
    pub exact_present_matches: u64,
    pub exact_present_pairing_rate: f64,
    pub raw_input_events: u64,
    pub keyboard_input_events: u64,
    pub mouse_input_events: u64,
    pub player_pose_samples: u64,
    pub render_camera_samples: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DepthLabJobState {
    Pending,
    Running,
}

impl DepthLabJobState {
    fn parse(value: &str) -> anyhow::Result<Self> {
        match value {
            "pending" => Ok(Self::Pending),
            "running" => Ok(Self::Running),
            _ => anyhow::bail!("unsupported depth-lab job state {value}"),
        }
    }
}

/// 旧独立 depth-lab archive 队列的 durable candidate，保留本地兼容/实验事实。
/// production uploader 不读这张队列；它从 segment 的 v28 ZIP path/hash 加入
/// [`UploadCandidate`]。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DepthLabCandidate {
    pub segment_id: String,
    pub session_id: String,
    pub run_id: String,
    pub generation: u64,
    pub segment_index: u32,
    pub state: DepthLabJobState,
    pub attempt_count: u32,
    pub manifest_relative_path: String,
    pub manifest_sha256: String,
}

#[cfg(any(feature = "depth-lab", test))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DepthArchiveJobState {
    Pending,
    Materialized,
    Registered,
    Uploading,
    ManifestAuthorized,
    RetryWait,
    Finalized,
}

#[cfg(any(feature = "depth-lab", test))]
impl DepthArchiveJobState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Materialized => "materialized",
            Self::Registered => "registered",
            Self::Uploading => "uploading",
            Self::ManifestAuthorized => "manifest_authorized",
            Self::RetryWait => "retry_wait",
            Self::Finalized => "finalized",
        }
    }

    fn parse(value: &str) -> anyhow::Result<Self> {
        match value {
            "pending" => Ok(Self::Pending),
            "materialized" => Ok(Self::Materialized),
            "registered" => Ok(Self::Registered),
            "uploading" => Ok(Self::Uploading),
            "manifest_authorized" => Ok(Self::ManifestAuthorized),
            "retry_wait" => Ok(Self::RetryWait),
            "finalized" => Ok(Self::Finalized),
            _ => anyhow::bail!("unsupported depth archive job state {value}"),
        }
    }
}

#[cfg(any(feature = "depth-lab", test))]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DepthArchiveJob {
    pub job_id: String,
    pub session_id: String,
    pub run_id: String,
    pub generation: u64,
    pub state: DepthArchiveJobState,
    pub attempt_count: u32,
    pub next_attempt_at_ms: u64,
    pub lease_owner: Option<String>,
    pub lease_expires_at_ms: Option<u64>,
    pub materialized_json: Option<String>,
    pub remote_scope_json: Option<String>,
    pub progress_json: Option<String>,
    pub last_error_code: Option<String>,
    pub last_error_message: Option<String>,
}

#[cfg(any(feature = "depth-lab", test))]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DepthArchiveLease {
    pub job: DepthArchiveJob,
    pub candidates: Vec<DepthLabCandidate>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UploadedPart {
    pub artifact_kind: String,
    pub provider_attempt_no: u32,
    pub part_number: u32,
    pub byte_offset: u64,
    pub byte_size: u64,
    pub content_md5: String,
    pub etag: String,
}

/// 视角标定种类（journal v13，docs/46 §2.3；v15 加 `Free`）。
///
/// `Free` = 自由测量段（docs/60 §6 Q4）：测绘工具，不是标定。它没有验收
/// 裁决，也**永远不进有效标定池**——`latest_valid_view_calibration` 按
/// `kind = 'full'` 过滤，preflight 走同一查询，两处天然隔离。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ViewCalibrationKind {
    Full,
    SpotCheck,
    Free,
}

impl ViewCalibrationKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::SpotCheck => "spot_check",
            Self::Free => "free",
        }
    }
}

/// 一次视角标定的落库请求。`calibration_id` 即幂等键（服务生成 UUID）。
pub struct ViewCalibrationWrite<'a> {
    pub calibration_id: &'a str,
    pub kind: ViewCalibrationKind,
    pub parent_calibration_id: Option<&'a str>,
    pub game_id: &'a str,
    pub recording_profile_hash: &'a str,
    pub device_profile_hash: &'a str,
    pub created_qpc: i64,
    /// 验收结果；`None` = 自由测量段（无裁决）。与 `kind` 必须成对，
    /// 由 `record_view_calibration` 与表上的 CHECK 两头锁死。
    pub verdict_pass: Option<bool>,
    pub counts_per_360_measured: Option<f64>,
    pub deviation_pct: Option<f64>,
    pub polling_hz_p50: Option<f64>,
    /// 证据文件相对 app-data 根的路径。
    pub file_path: &'a str,
    pub file_sha256: &'a str,
}

/// 视角标定的审计行（UI 历史与 preflight 门都读它）。
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ViewCalibrationAudit {
    pub calibration_id: String,
    pub kind: String,
    pub parent_calibration_id: Option<String>,
    pub game_id: String,
    pub recording_profile_hash: String,
    pub device_profile_hash: String,
    pub created_utc: String,
    pub created_qpc: i64,
    /// `None` = 自由测量段（`kind = "free"`），没有验收裁决。
    pub verdict: Option<String>,
    pub counts_per_360_measured: Option<f64>,
    pub deviation_pct: Option<f64>,
    pub polling_hz_p50: Option<f64>,
    pub file_path: String,
    pub file_sha256: String,
    pub invalidated_utc: Option<String>,
    pub invalidation_reason: Option<String>,
}

/// 会话绑定的标定证据摘要（`session_view_calibrations` × `view_calibrations`）。
#[derive(Clone, Debug)]
pub struct SessionViewCalibrationEvidence {
    /// 相对 app-data 根。
    pub file_path: String,
    pub file_sha256: String,
    /// 刻意**不是** `Option`：能绑到会话的只有 full/spot_check，自由测量段
    /// 进不了 `session_view_calibrations`。若真读到 NULL，说明有自由段被
    /// 当成标定绑上了会话——那要当场报错，不能静默降级成 None。
    pub verdict: String,
    pub deviation_pct: Option<f64>,
    pub polling_hz_p50: Option<f64>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionArtifactKind {
    Session,
    Catalog,
    DeviceProfile,
    RecordingProfile,
    GameProfile,
    Mapping,
    Markers,
    /// 视角尺度标定证据 `input-calibration.json`（docs/46 §2.2）。可选：
    /// 仅当 journal 引用了会话标定时随 bundle 冻结，与 `Markers` 同级。
    InputCalibration,
}

impl SessionArtifactKind {
    pub const REQUIRED: [Self; 6] = [
        Self::Session,
        Self::Catalog,
        Self::DeviceProfile,
        Self::RecordingProfile,
        Self::GameProfile,
        Self::Mapping,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::Catalog => "catalog",
            Self::DeviceProfile => "device_profile",
            Self::RecordingProfile => "recording_profile",
            Self::GameProfile => "game_profile",
            Self::Mapping => "mapping",
            Self::Markers => "markers",
            Self::InputCalibration => "input_calibration",
        }
    }

    pub const fn file_name(self) -> &'static str {
        match self {
            Self::Session => "session.json",
            Self::Catalog => "catalog.json",
            Self::DeviceProfile => "device-profile.json",
            Self::RecordingProfile => "recording-profile.json",
            Self::GameProfile => "game-profile.json",
            Self::Mapping => "mapping.json",
            Self::Markers => "markers.jsonl",
            Self::InputCalibration => "input-calibration.json",
        }
    }

    fn parse(value: &str) -> anyhow::Result<Self> {
        match value {
            "session" => Ok(Self::Session),
            "catalog" => Ok(Self::Catalog),
            "device_profile" => Ok(Self::DeviceProfile),
            "recording_profile" => Ok(Self::RecordingProfile),
            "game_profile" => Ok(Self::GameProfile),
            "mapping" => Ok(Self::Mapping),
            "markers" => Ok(Self::Markers),
            "input_calibration" => Ok(Self::InputCalibration),
            _ => anyhow::bail!("unsupported session artifact kind {value}"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionArtifactUploadState {
    Pending,
    Declared,
    Signed,
    Put,
    Completing,
    Verified,
    Rejected,
}

impl SessionArtifactUploadState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Declared => "declared",
            Self::Signed => "signed",
            Self::Put => "put",
            Self::Completing => "completing",
            Self::Verified => "verified",
            Self::Rejected => "rejected",
        }
    }

    fn parse(value: &str) -> anyhow::Result<Self> {
        match value {
            "pending" => Ok(Self::Pending),
            "declared" => Ok(Self::Declared),
            "signed" => Ok(Self::Signed),
            "put" => Ok(Self::Put),
            "completing" => Ok(Self::Completing),
            "verified" => Ok(Self::Verified),
            "rejected" => Ok(Self::Rejected),
            _ => anyhow::bail!("unsupported session artifact upload state {value}"),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionArtifactSource {
    pub kind: SessionArtifactKind,
    pub relative_path: String,
    pub expected_sha256: String,
    pub expected_size: u64,
    pub content_md5: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionArtifactUploadJob {
    pub session_id: String,
    pub kind: SessionArtifactKind,
    pub local_path: String,
    pub object_key: Option<String>,
    pub expected_sha256: String,
    pub expected_size: u64,
    pub content_md5: String,
    pub state: SessionArtifactUploadState,
    pub attempts: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionArtifactRemoteStatus {
    pub kind: SessionArtifactKind,
    pub object_key: String,
    pub expected_sha256: String,
    pub expected_size: u64,
    pub verification_state: String,
}

#[derive(Clone, Debug)]
pub struct SessionRegistrationCandidate {
    pub local_session_id: String,
    pub registration_state: String,
    #[allow(dead_code)]
    pub session_state: String,
    pub session_schema_version: String,
    pub started_at_utc: String,
    #[allow(dead_code)]
    pub ended_at_utc: Option<String>,
    #[allow(dead_code)]
    pub stop_reason: Option<String>,
    pub non_trainable: bool,
    pub consent_id: Option<String>,
    pub session_contract: String,
    pub catalog_id: String,
    pub catalog_version: String,
    pub catalog_hash: String,
    pub device_profile_id: String,
    pub device_profile_version: String,
    pub device_profile_hash: String,
    pub recording_profile_id: String,
    pub recording_profile_version: String,
    pub recording_profile_hash: String,
    pub game_id: String,
    pub game_profile_version: String,
    pub game_profile_hash: String,
    pub game_executable_sha256: Option<String>,
    pub game_executable_sha256_missing_reason: Option<String>,
    pub mapping_id: String,
    pub mapping_version: String,
    pub mapping_hash: String,
    pub width: u32,
    pub height: u32,
    pub fps_num: u32,
    pub fps_den: u32,
    pub capture_engine: String,
    pub remote_session_id: Option<String>,
    pub remote_session_sealed: bool,
}

#[derive(Clone, Debug)]
pub struct RecoveryInventory {
    pub sessions: Vec<RecoverySession>,
    pub runs: Vec<RecoveryRun>,
    pub segments: Vec<RecoverySegment>,
    pub markers: Vec<MarkerAudit>,
    pub heartbeats: Vec<HeartbeatAudit>,
}

#[derive(Clone, Debug)]
pub struct RecoverySession {
    pub id: String,
    pub state: String,
    pub schema_version: String,
    pub session_contract: String,
    pub catalog_id: String,
    pub catalog_version: String,
    pub catalog_hash: String,
    pub device_profile_id: String,
    pub device_profile_version: String,
    pub device_profile_hash: String,
    pub recording_profile_id: String,
    pub recording_profile_version: String,
    pub recording_profile_hash: String,
    pub recording_profile_hash_kind: String,
    pub game_id: String,
    pub game_executable_sha256: Option<String>,
    pub game_profile_version: String,
    pub game_profile_hash: String,
    pub mapping_id: String,
    pub mapping_version: String,
    pub mapping_hash: String,
}

#[derive(Clone, Debug)]
pub struct RecoveryRun {
    pub id: String,
    pub session_id: String,
    pub run_name: String,
    pub state: String,
    pub fps_num: u32,
    pub fps_den: u32,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Debug)]
pub struct RecoverySegment {
    pub id: String,
    pub session_id: String,
    pub run_id: String,
    pub run_name: String,
    pub seq: u32,
    pub state: String,
    pub session_frame_start: u64,
    pub n_frames: Option<u64>,
    pub qpc_start: Option<i64>,
    pub qpc_end: Option<i64>,
    pub video_path: Option<String>,
    pub frames_path: Option<String>,
    pub events_path: Option<String>,
    pub meta_path: Option<String>,
    pub manifest_path: Option<String>,
    pub video_sha256: Option<String>,
    pub frames_sha256: Option<String>,
    pub events_sha256: Option<String>,
    pub meta_sha256: Option<String>,
    pub bytes: Option<u64>,
    pub event_count: Option<u64>,
    pub duplicate_frames: Option<u64>,
    pub source_superseded: Option<u64>,
    pub source_drops: Option<u64>,
    pub audio: Option<SegmentAudioMetadata>,
}

#[derive(Clone, Debug, Serialize)]
pub struct RecoveryFinding {
    pub severity: &'static str,
    pub code: &'static str,
    pub action: &'static str,
    pub session_id: Option<String>,
    pub run_name: Option<String>,
    pub segment_seq: Option<u32>,
    pub relative_path: Option<String>,
    pub message: String,
}

#[derive(Clone, Debug)]
pub struct RecoveryCommit<'a> {
    pub recovery_id: &'a str,
    pub report_path: &'a str,
    pub scanned_sessions: u32,
    pub verified_ready_segments: u32,
    pub quarantined_items: u32,
    pub corrupt_segments: u32,
    pub orphan_items: u32,
    pub unfinished_run_ids: &'a [String],
    pub quarantined_segment_ids: &'a [String],
    pub corrupt_segment_ids: &'a [String],
    pub corrupt_session_ids: &'a [String],
    pub findings: &'a [RecoveryFinding],
}

#[derive(Clone, Debug, Serialize)]
pub struct JournalDiagnosticSnapshot {
    pub scoped_session_id: Option<String>,
    pub sessions: Vec<JournalDiagnosticSession>,
    pub runs: Vec<JournalDiagnosticRun>,
    pub segments: Vec<JournalDiagnosticSegment>,
    pub markers: Vec<JournalDiagnosticMarker>,
    pub preflights: Vec<PreflightStatus>,
    pub data_model_jobs: Vec<DataModelJobStatus>,
    pub latest_recovery: Option<JournalDiagnosticRecovery>,
    pub recovery_findings: Vec<JournalDiagnosticRecoveryFinding>,
}

#[derive(Clone, Debug, Serialize)]
pub struct JournalDiagnosticSession {
    pub id: String,
    pub state: String,
    pub started_at_utc: String,
    pub ended_at_utc: Option<String>,
    pub session_contract: String,
    pub catalog_id: String,
    pub catalog_version: String,
    pub catalog_hash: String,
    pub device_profile_id: String,
    pub device_profile_version: String,
    pub device_profile_hash: String,
    pub recording_profile_id: String,
    pub recording_profile_version: String,
    pub recording_profile_hash: String,
    pub recording_profile_hash_kind: String,
    pub game_id: String,
    pub game_profile_version: String,
    pub game_profile_hash: String,
    pub mapping_id: String,
    pub mapping_version: String,
    pub mapping_hash: String,
    pub schema_version: String,
    pub stop_reason: Option<String>,
    pub last_error_code: Option<String>,
    pub upload_seal_state: Option<String>,
    pub upload_seal_http_status: Option<u16>,
    pub upload_seal_server_code: Option<String>,
    pub upload_seal_server_message: Option<String>,
    pub upload_seal_request_id: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct JournalDiagnosticRun {
    pub session_id: String,
    pub run_name: String,
    pub seq: u32,
    pub state: String,
    pub backend: String,
    pub encoder_backend: String,
    pub fps_num: u32,
    pub fps_den: u32,
    pub width: u32,
    pub height: u32,
    pub frame_count: Option<u64>,
    pub ready_segments: Option<u32>,
    pub duplicate_frames: Option<u64>,
    pub source_superseded: Option<u64>,
    pub input_event_count: Option<u64>,
    pub input_queue_overflows: Option<u64>,
    pub stop_reason: Option<String>,
    pub error_code: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct JournalDiagnosticSegment {
    pub session_id: String,
    pub run_name: String,
    pub seq: u32,
    pub state: String,
    pub session_frame_start: u64,
    pub n_frames: Option<u64>,
    pub qpc_start: Option<i64>,
    pub qpc_end: Option<i64>,
    pub video_sha256: Option<String>,
    pub frames_sha256: Option<String>,
    pub events_sha256: Option<String>,
    pub meta_sha256: Option<String>,
    pub bytes: Option<u64>,
    pub event_count: Option<u64>,
    pub duplicate_frames: Option<u64>,
    pub source_superseded: Option<u64>,
    pub source_drops: Option<u64>,
    pub audio_state: Option<String>,
    pub audio_failure_code: Option<String>,
    pub audio_bytes: Option<u64>,
    pub error_code: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct JournalDiagnosticMarker {
    pub marker_id: String,
    pub session_id: String,
    pub run_id: String,
    pub seq: u32,
    pub kind: String,
    pub qpc: i64,
    pub qpc_frequency: i64,
    pub run_frame: u64,
    pub session_frame: u64,
    pub video_time_us: u64,
    pub alignment_state: String,
    pub segment_seq: Option<u32>,
}

#[derive(Clone, Debug, Serialize)]
pub struct JournalDiagnosticRecovery {
    pub id: String,
    pub state: String,
    pub scanned_sessions: u32,
    pub verified_ready_segments: u32,
    pub quarantined_items: u32,
    pub corrupt_segments: u32,
    pub orphan_items: u32,
    pub error_code: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct JournalDiagnosticRecoveryFinding {
    pub severity: String,
    pub code: String,
    pub action: String,
    pub session_id: Option<String>,
    pub run_name: Option<String>,
    pub segment_seq: Option<u32>,
    pub message: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionHistorySummary {
    pub session_id: String,
    pub state: String,
    pub game_id: String,
    pub recording_profile_id: String,
    pub mapping_id: String,
    pub mapping_version: String,
    pub session_contract: String,
    pub catalog_id: String,
    pub catalog_version: String,
    pub catalog_hash: String,
    pub device_profile_id: String,
    pub device_profile_version: String,
    pub device_profile_hash: String,
    pub recording_profile_version: String,
    pub recording_profile_hash: String,
    pub recording_profile_hash_kind: String,
    pub game_profile_version: String,
    pub game_profile_hash: String,
    pub mapping_hash: String,
    pub started_at_utc: String,
    pub ended_at_utc: Option<String>,
    pub stop_reason: Option<String>,
    pub error_code: Option<String>,
    pub run_count: u32,
    pub segment_count: u32,
    pub ready_segments: u32,
    pub corrupt_segments: u32,
    pub quarantined_segments: u32,
    pub frame_count: u64,
    pub input_event_count: u64,
    pub marker_count: u32,
    pub video_bytes: u64,
    /// True only after the annotator explicitly starts this session's upload
    /// from local history. Queue creation alone never opens the network gate.
    pub upload_requested: bool,
    pub upload_requested_at_utc: Option<String>,
    pub upload_job_segments: u32,
    /// Any performance-validation run permanently makes the whole session
    /// non-deliverable, even after recording ends or the client restarts.
    pub performance_validation_only: bool,
    /// Whether history should offer the manual upload action right now.
    pub upload_requestable: bool,
    pub upload_pending_segments: u32,
    pub upload_running_segments: u32,
    pub upload_retry_wait_segments: u32,
    pub upload_stored_segments: u32,
    /// Upload jobs that reached remote storage (`stored` or `verified`).
    pub uploaded_segments: u32,
    /// Upload jobs whose five objects passed the server's streamed SHA-256
    /// re-verification (`verified`) — the byte-identity fact local reclaim
    /// decisions key on.
    pub upload_verified_segments: u32,
    /// Terminal rejections (`rejected`): the remote copy must not be trusted.
    pub upload_rejected_segments: u32,
    /// Most recent stable upload error code. Request bodies, URLs, credentials,
    /// object keys and local absolute paths are intentionally never exposed.
    pub upload_last_error_code: Option<String>,
    pub upload_last_http_status: Option<u16>,
    pub upload_last_server_code: Option<String>,
    pub upload_last_server_message: Option<String>,
    pub upload_last_request_id: Option<String>,
    /// True when the session is `completed`, has at least one ready segment,
    /// and every ready segment's upload is server-`verified`. The UI offers the
    /// existing staged full deletion for these — nothing is deleted
    /// automatically, and RAW facts never change because of upload state.
    pub reclaimable: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionHistoryDetail {
    pub session: SessionHistorySummary,
    pub runs: Vec<SessionHistoryRun>,
    pub segments: Vec<SessionHistorySegment>,
    pub markers: Vec<SessionHistoryMarker>,
    pub preflights: Vec<PreflightStatus>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionHistoryRun {
    pub run_id: String,
    pub run_name: String,
    pub seq: u32,
    pub state: String,
    pub frame_count: Option<u64>,
    pub ready_segments: Option<u32>,
    pub input_event_count: Option<u64>,
    pub input_queue_overflows: Option<u64>,
    pub stop_reason: Option<String>,
    pub error_code: Option<String>,
    pub started_at_utc: Option<String>,
    pub ended_at_utc: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionHistorySegment {
    pub run_id: String,
    pub run_name: String,
    pub seq: u32,
    pub state: String,
    pub session_frame_start: u64,
    pub frame_count: Option<u64>,
    pub qpc_start: Option<i64>,
    pub qpc_end: Option<i64>,
    pub video_bytes: Option<u64>,
    pub event_count: Option<u64>,
    pub hashes_complete: bool,
    pub audio_state: Option<String>,
    pub audio_failure_code: Option<String>,
    pub audio_bytes: Option<u64>,
    pub error_code: Option<String>,
    pub ready_at_utc: Option<String>,
    /// 该分段 upload job 的状态（`upload_jobs.state`）；尚未入队时为 None。
    pub upload_state: Option<String>,
    /// 该分段最近一次上传失败/拒收的稳定错误码（docs/65 J-29：rejected 的
    /// `last_error_code` 此前不在任何前端契约里）。
    pub upload_error_code: Option<String>,
    pub upload_http_status: Option<u16>,
    pub upload_server_code: Option<String>,
    pub upload_server_message: Option<String>,
    pub upload_request_id: Option<String>,
    pub upload_attempt_count: Option<u32>,
    pub upload_next_attempt_at_ms: Option<u64>,
    pub uploaded_part_count: u32,
    pub uploaded_bytes: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionHistoryMarker {
    pub marker_id: String,
    pub run_id: String,
    pub run_name: String,
    pub seq: u32,
    pub kind: String,
    pub qpc: i64,
    pub qpc_frequency: i64,
    pub run_frame: u64,
    pub session_frame: u64,
    pub video_time_us: u64,
    pub alignment_state: String,
    pub segment_seq: Option<u32>,
    pub created_at_utc: String,
}

#[derive(Clone, Debug)]
pub struct PendingSessionDeletion {
    pub deletion_id: String,
    pub session_id: String,
    pub state: String,
    pub staged_relative_path: String,
}

pub struct RunStart<'a> {
    pub request_id: &'a str,
    pub session_id: &'a str,
    pub run_id: &'a str,
    pub run_seq: u32,
    pub session_frame_start: u64,
    /// 本 run 已通过 preflight 启动 depth probe。一旦为 true，每个
    /// ready segment 在 finalizer 落下 manifest/ZIP 之前都不得降级成纯视频上传。
    pub depth_expected: bool,
    /// 本 run 只服务于性能认证候选证据采集，RAW 与 depth 产物均永久不可交付。
    pub performance_validation_only: bool,
    pub process_id: u32,
    pub target_window_id: &'a str,
    pub executable_sha256: Option<&'a str>,
    /// 指纹缺席时的可读原因说明，与 `executable_sha256` 严格互斥（docs/55 §3.1）。
    pub executable_sha256_missing_reason: Option<&'a str>,
    pub profile: &'a CaptureProfile,
    pub mapping: &'a MappingSnapshot,
    pub catalog: &'a CatalogSelection,
    pub preflight: &'a PreflightStatus,
    /// 视角标定引用 (calibration_id, file_sha256)：preflight 门通过时由
    /// 编排器传入，与 preflight pass 同一事务写进 session_view_calibrations。
    pub view_calibration: Option<(&'a str, &'a str)>,
    /// consent 载体，随会话冻结、上传时作为 `declaredConsentId` 声明给服务端。
    /// 今天没有来源会填它（docs/100 §3 推论 3），恒 `None`；通路保留是为了下一刀
    /// （可训练性开通）接上账号/设备登记级的新来源时不必重开一条列。
    pub consent_id: Option<&'a str>,
}

#[cfg(test)]
pub struct TestRunStart<'a> {
    pub request_id: &'a str,
    pub session_id: &'a str,
    pub run_id: &'a str,
    pub run_seq: u32,
    pub session_frame_start: u64,
    pub process_id: u32,
    pub target_window_id: &'a str,
    pub executable_sha256: Option<&'a str>,
    /// 指纹缺席时的可读原因说明，与 `executable_sha256` 严格互斥（docs/55 §3.1）。
    pub executable_sha256_missing_reason: Option<&'a str>,
    pub profile: &'a CaptureProfile,
    pub mapping: &'a MappingSnapshot,
    pub view_calibration: Option<(&'a str, &'a str)>,
}

pub struct PreflightStart<'a> {
    pub evidence: &'a PreflightStatus,
    pub target_window_id: &'a str,
    pub game_id: &'a str,
    pub catalog: &'a CatalogSelection,
}

pub struct MarkerWrite<'a> {
    pub request_id: &'a str,
    pub session_id: &'a str,
    pub run_id: &'a str,
    pub kind: MarkerKind,
    pub qpc: i64,
}

pub struct HeartbeatWrite<'a> {
    pub session_id: &'a str,
    pub run_id: &'a str,
    pub qpc: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunTerminalState {
    Paused,
    Completed,
    Faulted,
}

impl RunTerminalState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Paused => "paused",
            Self::Completed => "completed",
            Self::Faulted => "faulted",
        }
    }
}

#[derive(Debug, Deserialize)]
struct ReadyMeta {
    schema_version: String,
    session_id: String,
    run_id: String,
    segment_index: u32,
    session_frame_start: u64,
    n_frames: u64,
    state: String,
    /// legacy 一代的两个训练声明；中性一代整体缺席（issue #855 §B）。
    /// `Option` 让"缺席"可表达，代际闸在 `validate_ready_artifact` 里两个方向都拦。
    #[serde(default)]
    non_trainable: Option<bool>,
    #[serde(default)]
    production_certified: Option<bool>,
    qpc_start: i64,
    qpc_end: i64,
    fps_num: u32,
    fps_den: u32,
    width: u32,
    height: u32,
    video: ArtifactFile,
    frames: ArtifactRows,
    events: ArtifactRows,
    #[serde(default)]
    audio: Option<SegmentAudioMetadata>,
    mapping: ArtifactMapping,
    #[serde(default)]
    catalog: Option<ArtifactCatalog>,
    health: ArtifactHealth,
}

#[derive(Debug, Deserialize)]
struct ArtifactCatalog {
    /// legacy 目录身份的 `mode`；中性一代不再自称车道，字段缺席（issue #855 §A.4）。
    #[serde(default)]
    mode: Option<String>,
    catalog_id: String,
    catalog_version: String,
    catalog_content_hash: String,
    device: ArtifactCatalogEntry,
    /// capture + settings 两本册子并成一本（docs/57 §3.1）。这是 session.json
    /// 里 catalog 段的读侧镜像，字段名必须与写侧一致。
    recording: ArtifactCatalogEntry,
    game: ArtifactCatalogEntry,
    mapping: ArtifactCatalogEntry,
}

#[derive(Debug, Deserialize)]
struct ArtifactCatalogEntry {
    id: String,
    version: String,
    content_hash: String,
}

#[derive(Debug, Deserialize)]
struct ArtifactFile {
    path: String,
    bytes: u64,
    sha256: String,
}

#[derive(Debug, Deserialize)]
struct ArtifactRows {
    path: String,
    lines: u64,
    sha256: String,
}

#[derive(Debug, Deserialize)]
struct ArtifactMapping {
    id: String,
    version: String,
    content_hash: String,
}

#[derive(Debug, Deserialize)]
struct ArtifactHealth {
    duplicate_frames: u64,
    source_superseded: u64,
    queue_drops: u64,
    input_overflow: u64,
}

struct ReadyArtifact {
    session_frame_start: u64,
    n_frames: u64,
    qpc_start: i64,
    qpc_end: i64,
    video_path: String,
    frames_path: String,
    events_path: String,
    meta_path: String,
    manifest_path: String,
    video_sha256: String,
    frames_sha256: String,
    events_sha256: String,
    meta_sha256: String,
    audio: Option<SegmentAudioMetadata>,
    bytes: u64,
    event_count: u64,
    duplicate_frames: u64,
    source_superseded: u64,
    source_drops: u64,
    mapping_id: String,
    mapping_version: String,
    mapping_hash: String,
    fps_num: u32,
    fps_den: u32,
    width: u32,
    height: u32,
    catalog: Option<ArtifactCatalog>,
    /// 这份产物自称属于哪一代摄取契约，由它的 `schema_version` 反解而来。
    /// `mark_segment_ready` 会拿它与会话在 journal 里那一代对账。
    contract: SessionContract,
}

struct JournalCatalogIdentity {
    schema_version: String,
    session_contract: String,
    catalog_id: String,
    catalog_version: String,
    catalog_hash: String,
    device_id: String,
    device_version: String,
    device_hash: String,
    recording_id: String,
    recording_version: String,
    recording_hash: String,
    game_id: String,
    game_version: String,
    game_hash: String,
    mapping_id: String,
    mapping_version: String,
    mapping_hash: String,
    fps_num: i64,
    fps_den: i64,
    width: i64,
    height: i64,
}

struct PreflightCatalogIdentity {
    target_id: String,
    session_contract: String,
    catalog_id: String,
    catalog_version: String,
    catalog_hash: String,
    device_id: String,
    device_version: String,
    device_hash: String,
    recording_id: String,
    recording_version: String,
    recording_hash: String,
    game_id: String,
    game_version: String,
    game_hash: String,
    mapping_id: String,
    mapping_version: String,
    mapping_hash: String,
}

impl PreflightCatalogIdentity {
    fn matches(&self, target_id: &str, catalog: &CatalogSelection) -> bool {
        self.target_id == target_id
            && self.session_contract == catalog.session_contract.source_family()
            && self.catalog_id == catalog.catalog.catalog_id
            && self.catalog_version == catalog.catalog.catalog_version
            && self.catalog_hash == catalog.catalog.content_hash
            && self.device_id == catalog.device.identity.id
            && self.device_version == catalog.device.identity.version
            && self.device_hash == catalog.device.identity.content_hash
            && self.recording_id == catalog.recording.identity.id
            && self.recording_version == catalog.recording.identity.version
            && self.recording_hash == catalog.recording.identity.content_hash
            && self.game_id == catalog.game.identity.id
            && self.game_version == catalog.game.identity.version
            && self.game_hash == catalog.game.identity.content_hash
            && self.mapping_id == catalog.mapping.identity.id
            && self.mapping_version == catalog.mapping.identity.version
            && self.mapping_hash == catalog.mapping.identity.content_hash
    }
}

struct PersistedSessionIdentity {
    game_id: String,
    game_exe_hash: Option<String>,
    recording_id: String,
    recording_version: String,
    recording_hash: String,
    recording_hash_kind: String,
    mapping_id: String,
    mapping_version: String,
    mapping_hash: String,
    session_contract: String,
    catalog_id: String,
    catalog_version: String,
    catalog_hash: String,
    device_id: String,
    device_version: String,
    device_hash: String,
    game_version: String,
    game_hash: String,
    non_trainable: i64,
    production_certified: i64,
    state: String,
    schema_version: String,
}

impl SessionJournal {
    pub fn open(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create journal directory {}", parent.display())
            })?;
        }
        let mut connection = Connection::open(path)
            .with_context(|| format!("failed to open SQLite journal {}", path.display()))?;
        connection
            .busy_timeout(std::time::Duration::from_millis(BUSY_TIMEOUT_MS))
            .context("failed to configure SQLite busy timeout")?;
        connection
            .pragma_update(None, "foreign_keys", "ON")
            .context("failed to enable SQLite foreign keys")?;
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .context("failed to enable SQLite WAL")?;
        connection
            .pragma_update(None, "synchronous", "FULL")
            .context("failed to configure SQLite durability")?;
        migrate(&connection)?;
        recover_interrupted_data_model_jobs(&connection)?;
        recover_interrupted_upload_jobs(&connection)?;
        terminalize_interrupted_preflights(&mut connection)?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    /// 借出连接给同 crate 的 outbox 模块（client_events）。锁的持有范围就是闭包
    /// 的执行范围，调用方拿不到连接的所有权，也就没法把锁泄漏出去。
    pub(crate) fn with_connection<T>(
        &self,
        run: impl FnOnce(&Connection) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let connection = self.connection.lock();
        run(&connection)
    }

    pub fn begin_preflight(&self, start: &PreflightStart<'_>) -> anyhow::Result<PreflightStatus> {
        preflight::verify(start.evidence)?;
        ensure!(
            start.evidence.result == "running"
                && start.evidence.session_id.is_none()
                && start.evidence.run_id.is_none()
                && start.evidence.session_contract == start.catalog.session_contract
                && start.game_id == start.catalog.game.identity.id,
            "initial preflight evidence identity is invalid"
        );
        let evidence_json = serde_json::to_string(start.evidence)?;
        let mut connection = self.connection.lock();
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(existing) = load_preflight_by_request_id(&tx, &start.evidence.request_id)? {
            let identity = load_preflight_catalog_identity(&tx, &existing.preflight_id)?
                .context("persisted preflight catalog identity is missing")?;
            ensure!(
                existing.preflight_id == start.evidence.preflight_id
                    && existing.session_contract == start.evidence.session_contract
                    && identity.matches(start.target_window_id, start.catalog),
                "preflight request id is already bound to another target/catalog identity"
            );
            tx.commit()?;
            return Ok(existing);
        }
        tx.execute(
            "INSERT INTO preflight_attempts (
                id, request_id, state, phase, session_contract, target_window_id, game_id,
                catalog_id, catalog_version, catalog_hash,
                device_profile_id, device_profile_version, device_profile_hash,
                recording_profile_id, recording_profile_version, recording_profile_hash,
                game_profile_version, game_profile_hash,
                mapping_id, mapping_version, mapping_hash,
                evidence_json, evidence_hash, created_at_utc, updated_at_utc
             ) VALUES (
                ?1, ?2, 'running', ?3, ?4, ?5, ?6,
                ?7, ?8, ?9,
                ?10, ?11, ?12,
                ?13, ?14, ?15,
                ?16, ?17,
                ?18, ?19, ?20,
                ?21, ?22,
                strftime('%Y-%m-%dT%H:%M:%fZ','now'), strftime('%Y-%m-%dT%H:%M:%fZ','now')
             )",
            params![
                start.evidence.preflight_id,
                start.evidence.request_id,
                start.evidence.phase,
                start.evidence.session_contract.source_family(),
                start.target_window_id,
                start.game_id,
                start.catalog.catalog.catalog_id,
                start.catalog.catalog.catalog_version,
                start.catalog.catalog.content_hash,
                start.catalog.device.identity.id,
                start.catalog.device.identity.version,
                start.catalog.device.identity.content_hash,
                start.catalog.recording.identity.id,
                start.catalog.recording.identity.version,
                start.catalog.recording.identity.content_hash,
                start.catalog.game.identity.version,
                start.catalog.game.identity.content_hash,
                start.catalog.mapping.identity.id,
                start.catalog.mapping.identity.version,
                start.catalog.mapping.identity.content_hash,
                evidence_json,
                start.evidence.evidence_hash,
            ],
        )?;
        tx.commit()?;
        Ok(start.evidence.clone())
    }

    pub fn update_preflight(&self, evidence: &PreflightStatus) -> anyhow::Result<()> {
        preflight::verify(evidence)?;
        let mut connection = self.connection.lock();
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        update_preflight_in_transaction(&tx, evidence)?;
        tx.commit()?;
        Ok(())
    }

    #[cfg(test)]
    pub fn preflight_by_request_id(
        &self,
        request_id: &str,
    ) -> anyhow::Result<Option<PreflightStatus>> {
        let connection = self.connection.lock();
        load_preflight_by_request_id(&connection, request_id)
    }

    #[cfg(test)]
    pub fn preflight_by_id(&self, preflight_id: &str) -> anyhow::Result<Option<PreflightStatus>> {
        let connection = self.connection.lock();
        load_preflight_by_id(&connection, preflight_id)
    }

    #[cfg(test)]
    pub fn list_preflights(&self, limit: u32) -> anyhow::Result<Vec<PreflightStatus>> {
        ensure!(
            (1..=200).contains(&limit),
            "preflight history limit is invalid"
        );
        let connection = self.connection.lock();
        let mut statement = connection.prepare(
            "SELECT evidence_json FROM preflight_attempts
             ORDER BY created_at_utc DESC, id DESC LIMIT ?1",
        )?;
        statement
            .query_map(params![i64::from(limit)], |row| row.get::<_, String>(0))?
            .map(|row| parse_preflight_json(&row?))
            .collect()
    }

    pub fn begin_run(&self, start: &RunStart<'_>) -> anyhow::Result<()> {
        ensure!(
            !start.request_id.trim().is_empty(),
            "journal request id is empty"
        );
        ensure!(
            start.request_id == start.preflight.request_id,
            "capture run request id does not match its preflight attempt"
        );
        ensure!(!start.session_id.is_empty(), "journal session id is empty");
        ensure!(!start.run_id.is_empty(), "journal run id is empty");
        preflight::verify(start.preflight)?;
        // 通过词必须是这条车道自己的那个（`preflight::pass_result_for`）：带着别的
        // 词的证据说明它不是走完这条门禁来的，拒收。
        ensure!(
            start.preflight.result == preflight::pass_result_for(start.preflight.session_contract)
                && start.preflight.session_id.as_deref() == Some(start.session_id)
                && start.preflight.run_id.as_deref() == Some(start.run_id)
                && start.preflight.session_contract == start.catalog.session_contract,
            "capture run preflight candidate is not linked or did not pass"
        );

        let profile_hash = start.catalog.recording.identity.content_hash.clone();
        let mapping = start.mapping.mapping();
        let run_journal_id = run_journal_id(start.session_id, start.run_id);
        let mut connection = self.connection.lock();
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let persisted_preflight = load_preflight_by_id(&tx, &start.preflight.preflight_id)?
            .context("capture run references no preflight evidence")?;
        let preflight_identity =
            load_preflight_catalog_identity(&tx, &start.preflight.preflight_id)?
                .context("capture run preflight catalog identity is missing")?;
        ensure!(
            persisted_preflight.request_id == start.preflight.request_id
                && persisted_preflight.session_contract == start.preflight.session_contract
                && (persisted_preflight.result == "running"
                    || persisted_preflight.result
                        == preflight::pass_result_for(persisted_preflight.session_contract)),
            "capture run preflight candidate conflicts with persisted evidence"
        );
        ensure!(
            preflight_identity.matches(start.target_window_id, start.catalog),
            "capture run catalog identity does not match its preflight evidence"
        );
        update_preflight_in_transaction(&tx, start.preflight)?;

        // certified 车道废除后（docs/100 §3 推论 2）这个派生退回恒 1：本地永远不
        // 主张可训练。**不要**把它改成「按 consent 派生」——那需要 admission、
        // 前缀与发布闸配套，是下一刀的事。v27 的表级 CHECK 与上传信封
        // （uploader `classification_for`）两头钉着同一个恒值。
        //
        // consent 与车道的强制配对随之删除：带 consent 的会话仍然 non_trainable=1，
        // 那正是想要的失败方向——「录了但标签保守」可逆，「该录的没录」不可逆。
        let non_trainable_flag: i64 = 1;
        let session_consent_id: Option<&str> = start.consent_id;

        tx.execute(
            "INSERT INTO sessions (
                id, state, non_trainable, production_certified,
                annotator_id, device_id, consent_id,
                game_id, game_exe_hash,
                recording_profile_id, recording_profile_version,
                recording_profile_hash, recording_profile_hash_kind,
                mapping_id, mapping_version, mapping_hash,
                session_contract, catalog_id, catalog_version, catalog_hash,
                device_profile_id, device_profile_version, device_profile_hash,
                game_profile_version, game_profile_hash,
                game_exe_hash_missing_reason,
                schema_version, started_at_utc, created_at_utc, updated_at_utc
             ) VALUES (
                ?1, 'active', ?19, 0,
                NULL, NULL, ?20,
                ?2, ?3,
                ?4, ?5,
                ?6, 'exact_source_bytes_sha256',
                ?7, ?8, ?9,
                ?10, ?11, ?12, ?13,
                ?14, ?15, ?16,
                ?17, ?18,
                ?21,
                ?22, strftime('%Y-%m-%dT%H:%M:%fZ','now'), strftime('%Y-%m-%dT%H:%M:%fZ','now'), strftime('%Y-%m-%dT%H:%M:%fZ','now')
             ) ON CONFLICT(id) DO NOTHING",
            params![
                start.session_id,
                mapping.game.id,
                start.executable_sha256,
                start.catalog.recording.identity.id,
                start.catalog.recording.identity.version,
                profile_hash,
                mapping.mapping_id,
                mapping.version,
                start.mapping.content_hash(),
                start.catalog.session_contract.source_family(),
                start.catalog.catalog.catalog_id,
                start.catalog.catalog.catalog_version,
                start.catalog.catalog.content_hash,
                start.catalog.device.identity.id,
                start.catalog.device.identity.version,
                start.catalog.device.identity.content_hash,
                start.catalog.game.identity.version,
                start.catalog.game.identity.content_hash,
                non_trainable_flag,
                session_consent_id,
                start.executable_sha256_missing_reason,
                start
                    .catalog
                    .session_contract
                    .session_schema_version(start.profile.audio.is_some()),
            ],
        )?;
        let session_state = validate_session_identity(&tx, start, &profile_hash)?;
        ensure_data_model_job(&tx, start.session_id)?;
        if let Some((calibration_id, calibration_sha256)) = start.view_calibration {
            // resume/rebind 以同 session 重放 begin_run；PK=session_id，
            // 首次引用生效、后续重放幂等，与 preflight pass 原子同生死。
            tx.execute(
                "INSERT OR IGNORE INTO session_view_calibrations(
                    session_id, calibration_id, calibration_sha256, created_at_utc
                 ) VALUES (?1, ?2, ?3, strftime('%Y-%m-%dT%H:%M:%fZ','now'))",
                params![start.session_id, calibration_id, calibration_sha256],
            )?;
        }

        if let Some((existing_id, existing_session, existing_run)) = tx
            .query_row(
                "SELECT id, session_id, run_name FROM capture_runs WHERE start_request_id = ?1",
                params![start.request_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?
        {
            ensure!(
                existing_id == run_journal_id
                    && existing_session == start.session_id
                    && existing_run == start.run_id,
                "request_id is already bound to a different capture run"
            );
            validate_run_identity(&tx, start, &run_journal_id)?;
            tx.commit()?;
            return Ok(());
        }

        ensure!(
            matches!(session_state.as_str(), "active" | "paused"),
            "session cannot accept a new run from state {session_state}"
        );

        tx.execute(
            "INSERT INTO capture_runs (
                id, session_id, run_name, seq, state, start_request_id,
                backend, target_pid, target_window_id, encoder_backend,
                fps_num, fps_den, width, height, session_frame_start, preflight_id, depth_expected,
                performance_validation_only,
                created_at_utc, updated_at_utc
             ) VALUES (
                ?1, ?2, ?3, ?4, 'starting', ?5,
                'native_wgc_window_v1', ?6, ?7, 'mf_nvidia_hardware_v1',
                ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15,
                strftime('%Y-%m-%dT%H:%M:%fZ','now'), strftime('%Y-%m-%dT%H:%M:%fZ','now')
             ) ON CONFLICT(id) DO NOTHING",
            params![
                run_journal_id,
                start.session_id,
                start.run_id,
                i64::from(start.run_seq),
                start.request_id,
                i64::from(start.process_id),
                start.target_window_id,
                i64::from(start.profile.fps_num),
                i64::from(start.profile.fps_den),
                i64::from(start.profile.width),
                i64::from(start.profile.height),
                u64_to_i64(start.session_frame_start, "session frame start")?,
                start.preflight.preflight_id,
                start.depth_expected,
                start.performance_validation_only,
            ],
        )?;
        let run_state = validate_run_identity(&tx, start, &run_journal_id)?;
        ensure!(
            run_state == "starting",
            "new capture run did not enter starting state"
        );
        transition_session(&tx, start.session_id, &["active", "paused"], "active", None)?;
        tx.commit()?;
        Ok(())
    }

    #[cfg(test)]
    pub fn begin_test_run(&self, start: &TestRunStart<'_>) -> anyhow::Result<()> {
        let catalog = CatalogSelection::for_test(start.profile, start.mapping);
        let preflight_request_id = start.request_id.to_owned();
        let mut evidence = preflight::start(
            &preflight_request_id,
            start.target_window_id,
            &catalog.catalog.content_hash,
            catalog.session_contract,
        )?;
        evidence = self.begin_preflight(&PreflightStart {
            evidence: &evidence,
            target_window_id: start.target_window_id,
            game_id: &start.mapping.mapping().game.id,
            catalog: &catalog,
        })?;
        if evidence.result == "running" {
            // 通过词由这次预检的契约给出（issue #855 §B）。先取出来再借可变引用。
            let pass_result = preflight::pass_result_for(evidence.session_contract);
            preflight::append_check(
                &mut evidence,
                "test_fixture",
                pass_result,
                serde_json::json!({ "fixture": true }),
            )?;
            preflight::link_run(&mut evidence, start.session_id, start.run_id)?;
            preflight::pass(&mut evidence)?;
        }
        self.begin_run(&RunStart {
            request_id: start.request_id,
            session_id: start.session_id,
            run_id: start.run_id,
            run_seq: start.run_seq,
            session_frame_start: start.session_frame_start,
            depth_expected: false,
            performance_validation_only: false,
            process_id: start.process_id,
            target_window_id: start.target_window_id,
            executable_sha256: start.executable_sha256,
            executable_sha256_missing_reason: start.executable_sha256_missing_reason,
            profile: start.profile,
            mapping: start.mapping,
            catalog: &catalog,
            preflight: &evidence,
            view_calibration: start.view_calibration,
            // 测试夹具恒 dev 车道（上面的注释），dev 会话不得带 consent。
            consent_id: None,
        })
    }

    pub fn mark_run_started_with_wall_clock(
        &self,
        session_id: &str,
        run_id: &str,
        anchor_qpc: i64,
        anchor_at_utc: Option<&str>,
        qpc_frequency: i64,
    ) -> anyhow::Result<()> {
        ensure!(qpc_frequency > 0, "QPC frequency must be positive");
        let id = run_journal_id(session_id, run_id);
        let mut connection = self.connection.lock();
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let state = run_state(&tx, &id)?;
        match state.as_str() {
            "starting" => {
                tx.execute(
                    "UPDATE capture_runs
                     SET state = 'recording', anchor_qpc = ?2, anchor_at_utc = ?3,
                         qpc_frequency = ?4,
                         started_at_utc = COALESCE(started_at_utc, strftime('%Y-%m-%dT%H:%M:%fZ','now')),
                         updated_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now')
                     WHERE id = ?1",
                    params![id, anchor_qpc, anchor_at_utc, qpc_frequency],
                )?;
            }
            "recording" => {
                let existing = tx.query_row(
                    "SELECT anchor_qpc, anchor_at_utc, qpc_frequency
                     FROM capture_runs WHERE id = ?1",
                    params![id],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, Option<String>>(1)?,
                            row.get::<_, i64>(2)?,
                        ))
                    },
                )?;
                ensure!(
                    existing == (anchor_qpc, anchor_at_utc.map(str::to_owned), qpc_frequency),
                    "run start replay changed its wall-clock/QPC identity"
                );
            }
            _ => anyhow::bail!("illegal capture run transition {state} -> recording"),
        }
        tx.commit()?;
        Ok(())
    }

    #[cfg(test)]
    pub fn mark_run_started(
        &self,
        session_id: &str,
        run_id: &str,
        anchor_qpc: i64,
        qpc_frequency: i64,
    ) -> anyhow::Result<()> {
        self.mark_run_started_with_wall_clock(session_id, run_id, anchor_qpc, None, qpc_frequency)
    }

    pub fn record_heartbeat(
        &self,
        heartbeat: &HeartbeatWrite<'_>,
    ) -> anyhow::Result<HeartbeatAudit> {
        ensure!(heartbeat.qpc >= 0, "heartbeat QPC must not be negative");
        let run_journal_id = run_journal_id(heartbeat.session_id, heartbeat.run_id);
        let mut connection = self.connection.lock();
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (state, anchor_qpc, qpc_frequency) = tx
            .query_row(
                "SELECT state, anchor_qpc, qpc_frequency
                 FROM capture_runs WHERE id = ?1 AND session_id = ?2 AND run_name = ?3",
                params![run_journal_id, heartbeat.session_id, heartbeat.run_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<i64>>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                    ))
                },
            )
            .with_context(|| {
                format!(
                    "heartbeat run {}/{} is missing from the SQLite journal",
                    heartbeat.session_id, heartbeat.run_id
                )
            })?;
        ensure!(
            state == "recording",
            "heartbeat requires an active recording run"
        );
        let anchor_qpc = anchor_qpc.context("recording run has no QPC anchor")?;
        let qpc_frequency = qpc_frequency.context("recording run has no QPC frequency")?;
        ensure!(
            heartbeat.qpc >= anchor_qpc,
            "heartbeat QPC precedes the recording anchor"
        );
        if let Some(existing) = load_heartbeat_by_run_qpc(&tx, &run_journal_id, heartbeat.qpc)? {
            tx.commit()?;
            return Ok(existing);
        }

        let next_seq: u32 = tx.query_row(
            "SELECT COALESCE(MAX(seq) + 1, 0) FROM heartbeats WHERE session_id = ?1",
            params![heartbeat.session_id],
            |row| row.get(0),
        )?;
        let heartbeat_id = format!("{}:h{next_seq:06}", heartbeat.session_id);
        tx.execute(
            "INSERT INTO heartbeats (
                id, session_id, run_id, run_name, seq, qpc, qpc_frequency,
                collector, created_at_utc
             ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7,
                'ok', strftime('%Y-%m-%dT%H:%M:%fZ','now')
             )",
            params![
                heartbeat_id,
                heartbeat.session_id,
                run_journal_id,
                heartbeat.run_id,
                i64::from(next_seq),
                heartbeat.qpc,
                qpc_frequency,
            ],
        )?;
        let persisted = load_heartbeat_by_run_qpc(&tx, &run_journal_id, heartbeat.qpc)?
            .context("inserted heartbeat disappeared before commit")?;
        tx.commit()?;
        Ok(persisted)
    }

    pub fn heartbeats_for_session(&self, session_id: &str) -> anyhow::Result<Vec<HeartbeatAudit>> {
        let connection = self.connection.lock();
        load_heartbeats_for_session(&connection, session_id)
    }

    pub fn marker_by_request_id(&self, request_id: &str) -> anyhow::Result<Option<MarkerAudit>> {
        let connection = self.connection.lock();
        load_marker_by_request_id(&connection, request_id)
    }

    /// 落库一次视角标定（幂等：`calibration_id` 重放时校验关键字段一致
    /// 后返回既有行，不覆盖）。证据文件本身由服务原子写盘，这里只登记
    /// 路径与指纹。
    pub fn record_view_calibration(
        &self,
        write: &ViewCalibrationWrite<'_>,
    ) -> anyhow::Result<ViewCalibrationAudit> {
        let id_ok = write.calibration_id.len() == 36
            && write
                .calibration_id
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase() || byte == b'-');
        ensure!(id_ok, "view calibration id must be a lowercase UUID");
        ensure!(
            valid_sha256_reference(write.recording_profile_hash)
                && valid_sha256_reference(write.device_profile_hash)
                && valid_sha256_reference(write.file_sha256),
            "view calibration hashes must be sha256:<64 lowercase hex>"
        );
        ensure!(
            !write.game_id.trim().is_empty(),
            "view calibration game id must not be empty"
        );
        ensure!(
            write.created_qpc >= 0,
            "view calibration QPC must not be negative"
        );
        let path = Path::new(write.file_path);
        ensure!(
            !write.file_path.trim().is_empty()
                && !path.is_absolute()
                && path
                    .components()
                    .all(|component| matches!(component, Component::Normal(_))),
            "view calibration file path must stay inside the app-data root"
        );
        match write.kind {
            ViewCalibrationKind::Full => ensure!(
                write.parent_calibration_id.is_none(),
                "full calibration must not reference a parent"
            ),
            ViewCalibrationKind::SpotCheck => ensure!(
                write.parent_calibration_id.is_some(),
                "spot check must reference its full calibration"
            ),
            ViewCalibrationKind::Free => ensure!(
                write.parent_calibration_id.is_none(),
                "free measurement must not reference a parent"
            ),
        }
        // kind 与裁决必须成对：自由测量段无裁决，其余必须有。表上的
        // CHECK((kind = 'free') = (verdict IS NULL)) 是同一条不变量的第二道锁，
        // 这里先拦是为了给出可读错误而不是 SQLite 约束码。
        ensure!(
            (write.kind == ViewCalibrationKind::Free) == write.verdict_pass.is_none(),
            "view calibration kind {} and verdict presence disagree (free measurements have no \
             verdict; every other kind must have one)",
            write.kind.as_str()
        );

        let mut connection = self.connection.lock();
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(existing) = load_view_calibration(&tx, write.calibration_id)? {
            ensure!(
                existing.kind == write.kind.as_str()
                    && existing.game_id == write.game_id
                    && existing.recording_profile_hash == write.recording_profile_hash
                    && existing.device_profile_hash == write.device_profile_hash
                    && existing.file_sha256 == write.file_sha256,
                "view calibration id is already bound to a different calibration"
            );
            tx.commit()?;
            return Ok(existing);
        }
        tx.execute(
            "INSERT INTO view_calibrations(
                calibration_id, kind, parent_calibration_id, game_id,
                recording_profile_hash, device_profile_hash, created_utc,
                created_qpc, verdict, counts_per_360_measured, deviation_pct,
                polling_hz_p50, file_path, file_sha256
             ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6,
                strftime('%Y-%m-%dT%H:%M:%fZ','now'),
                ?7, ?8, ?9, ?10, ?11, ?12, ?13
             )",
            params![
                write.calibration_id,
                write.kind.as_str(),
                write.parent_calibration_id,
                write.game_id,
                write.recording_profile_hash,
                write.device_profile_hash,
                write.created_qpc,
                write
                    .verdict_pass
                    .map(|pass| if pass { "pass" } else { "fail" }),
                write.counts_per_360_measured,
                write.deviation_pct,
                write.polling_hz_p50,
                write.file_path,
                write.file_sha256,
            ],
        )?;
        let persisted = load_view_calibration(&tx, write.calibration_id)?
            .context("inserted view calibration disappeared before commit")?;
        tx.commit()?;
        Ok(persisted)
    }

    /// 指定全量标定下最近一次未失效、pass 的抽检（preflight 门用）。
    pub fn latest_valid_spot_check(
        &self,
        parent_calibration_id: &str,
    ) -> anyhow::Result<Option<ViewCalibrationAudit>> {
        let connection = self.connection.lock();
        connection
            .query_row(
                &format!(
                    "SELECT {VIEW_CALIBRATION_COLUMNS} FROM view_calibrations
                      WHERE parent_calibration_id = ?1 AND kind = 'spot_check'
                        AND verdict = 'pass' AND invalidated_utc IS NULL
                      ORDER BY created_qpc DESC LIMIT 1"
                ),
                params![parent_calibration_id],
                view_calibration_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    /// 会话引用的标定证据摘要（uploader 声明与 artifact 冻结共用）。
    /// sha 取 session_view_calibrations 里 begin_run 时钉下的会话绑定值。
    pub fn session_view_calibration_evidence(
        &self,
        session_id: &str,
    ) -> anyhow::Result<Option<SessionViewCalibrationEvidence>> {
        let connection = self.connection.lock();
        connection
            .query_row(
                "SELECT vc.file_path, svc.calibration_sha256,
                        vc.verdict, vc.deviation_pct, vc.polling_hz_p50
                   FROM session_view_calibrations svc
                   JOIN view_calibrations vc ON vc.calibration_id = svc.calibration_id
                  WHERE svc.session_id = ?1",
                params![session_id],
                |row| {
                    Ok(SessionViewCalibrationEvidence {
                        file_path: row.get(0)?,
                        file_sha256: row.get(1)?,
                        verdict: row.get(2)?,
                        deviation_pct: row.get(3)?,
                        polling_hz_p50: row.get(4)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    /// 该目录三元组下最近一次未失效、verdict=pass 的全量标定。
    pub fn latest_valid_view_calibration(
        &self,
        game_id: &str,
        recording_profile_hash: &str,
        device_profile_hash: &str,
    ) -> anyhow::Result<Option<ViewCalibrationAudit>> {
        let connection = self.connection.lock();
        connection
            .query_row(
                &format!(
                    "SELECT {VIEW_CALIBRATION_COLUMNS} FROM view_calibrations
                      WHERE game_id = ?1 AND recording_profile_hash = ?2
                        AND device_profile_hash = ?3 AND kind = 'full'
                        AND verdict = 'pass' AND invalidated_utc IS NULL
                      ORDER BY created_qpc DESC LIMIT 1"
                ),
                params![game_id, recording_profile_hash, device_profile_hash],
                view_calibration_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    /// UI 历史列表（新→旧）。
    pub fn list_view_calibrations(&self, limit: u32) -> anyhow::Result<Vec<ViewCalibrationAudit>> {
        let connection = self.connection.lock();
        let mut statement = connection.prepare(&format!(
            "SELECT {VIEW_CALIBRATION_COLUMNS} FROM view_calibrations
              ORDER BY created_utc DESC LIMIT ?1"
        ))?;
        statement
            .query_map(params![limit], view_calibration_from_row)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// 标记一次标定失效（配置变更/设备变更/过期）。调用点：settings_watch
    /// 的 config_watch 路径；device_changed/expired 由后续生命周期接入。
    pub fn invalidate_view_calibration(
        &self,
        calibration_id: &str,
        reason: &str,
    ) -> anyhow::Result<()> {
        ensure!(
            matches!(
                reason,
                "settings_changed" | "config_watch" | "device_changed" | "expired"
            ),
            "unsupported view calibration invalidation reason {reason}"
        );
        let connection = self.connection.lock();
        let updated = connection.execute(
            "UPDATE view_calibrations
                SET invalidated_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now'),
                    invalidation_reason = ?2
              WHERE calibration_id = ?1 AND invalidated_utc IS NULL",
            params![calibration_id, reason],
        )?;
        ensure!(
            updated == 1,
            "view calibration {calibration_id} is missing or already invalidated"
        );
        Ok(())
    }

    pub fn record_marker(&self, marker: &MarkerWrite<'_>) -> anyhow::Result<MarkerAudit> {
        ensure!(
            !marker.request_id.trim().is_empty() && marker.request_id.len() <= 128,
            "marker request id must contain 1..=128 bytes"
        );
        ensure!(
            marker.request_id.is_ascii()
                && marker
                    .request_id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric()
                        || matches!(byte, b'-' | b'_' | b'.' | b':')),
            "marker request id contains unsupported characters"
        );
        ensure!(marker.qpc >= 0, "marker QPC must not be negative");

        let run_journal_id = run_journal_id(marker.session_id, marker.run_id);
        let mut connection = self.connection.lock();
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(existing) = load_marker_by_request_id(&tx, marker.request_id)? {
            ensure!(
                existing.session_id == marker.session_id
                    && existing.run_id == marker.run_id
                    && existing.kind == marker.kind.as_str(),
                "marker request id is already bound to a different marker"
            );
            align_marker_to_existing_ready_segment(
                &tx,
                &existing.marker_id,
                &run_journal_id,
                existing.qpc,
                existing.session_frame,
            )?;
            tx.commit()?;
            return Ok(existing);
        }

        let (state, anchor_qpc, qpc_frequency, fps_num, fps_den, session_frame_start) = tx
            .query_row(
                "SELECT state, anchor_qpc, qpc_frequency, fps_num, fps_den, session_frame_start
                 FROM capture_runs WHERE id = ?1 AND session_id = ?2 AND run_name = ?3",
                params![run_journal_id, marker.session_id, marker.run_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<i64>>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                        row.get::<_, u32>(3)?,
                        row.get::<_, u32>(4)?,
                        row.get::<_, u64>(5)?,
                    ))
                },
            )
            .with_context(|| {
                format!(
                    "marker run {}/{} is missing from the SQLite journal",
                    marker.session_id, marker.run_id
                )
            })?;
        ensure!(
            state == "recording",
            "marker requires an active recording run"
        );
        let anchor_qpc = anchor_qpc.context("recording run has no QPC anchor")?;
        let qpc_frequency = qpc_frequency.context("recording run has no QPC frequency")?;
        let run_frame =
            frame_index_at_qpc(anchor_qpc, marker.qpc, qpc_frequency, fps_num, fps_den)?;
        let session_frame = session_frame_start
            .checked_add(run_frame)
            .context("marker session frame overflowed")?;
        let video_time_100ns = frame_pts_100ns(run_frame, fps_num, fps_den);
        let video_time_us =
            u64::try_from(video_time_100ns / 10).context("marker video time is outside u64")?;
        let next_seq: u32 = tx.query_row(
            "SELECT COALESCE(MAX(seq) + 1, 0) FROM markers WHERE session_id = ?1",
            params![marker.session_id],
            |row| row.get(0),
        )?;
        let marker_id = format!("{}:m{next_seq:06}", marker.session_id);
        tx.execute(
            "INSERT INTO markers (
                id, session_id, run_id, run_name, seq, request_id, kind,
                qpc, qpc_frequency, run_frame, session_frame, video_time_us,
                alignment_state, non_trainable, production_certified, created_at_utc
             ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7,
                ?8, ?9, ?10, ?11, ?12,
                'pending',
                (SELECT non_trainable FROM sessions WHERE id = ?2),
                0, strftime('%Y-%m-%dT%H:%M:%fZ','now')
             )",
            params![
                marker_id,
                marker.session_id,
                run_journal_id,
                marker.run_id,
                i64::from(next_seq),
                marker.request_id,
                marker.kind.as_str(),
                marker.qpc,
                qpc_frequency,
                u64_to_i64(run_frame, "marker run frame")?,
                u64_to_i64(session_frame, "marker session frame")?,
                u64_to_i64(video_time_us, "marker video time")?,
            ],
        )?;
        align_marker_to_existing_ready_segment(
            &tx,
            &marker_id,
            &run_journal_id,
            marker.qpc,
            session_frame,
        )?;
        let persisted = load_marker_by_request_id(&tx, marker.request_id)?
            .context("inserted marker disappeared before commit")?;
        tx.commit()?;
        Ok(persisted)
    }

    pub fn markers_for_session(&self, session_id: &str) -> anyhow::Result<Vec<MarkerAudit>> {
        let connection = self.connection.lock();
        load_markers_for_session(&connection, session_id)
    }

    pub fn mark_segment_started(
        &self,
        session_id: &str,
        run_id: &str,
        segment_index: u32,
        session_frame_start: u64,
    ) -> anyhow::Result<()> {
        let run_journal_id = run_journal_id(session_id, run_id);
        let segment_id = segment_journal_id(session_id, run_id, segment_index);
        let mut connection = self.connection.lock();
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure!(
            run_state(&tx, &run_journal_id)? == "recording",
            "segment cannot start outside a recording run"
        );
        tx.execute(
            "INSERT INTO segments (
                id, session_id, run_id, run_name, seq, state, session_frame_start,
                created_at_utc, updated_at_utc
             ) VALUES (?1, ?2, ?3, ?4, ?5, 'active', ?6,
                       strftime('%Y-%m-%dT%H:%M:%fZ','now'), strftime('%Y-%m-%dT%H:%M:%fZ','now'))
             ON CONFLICT(id) DO NOTHING",
            params![
                segment_id,
                session_id,
                run_journal_id,
                run_id,
                i64::from(segment_index),
                u64_to_i64(session_frame_start, "segment session frame start")?,
            ],
        )?;
        let existing = tx.query_row(
            "SELECT state, session_frame_start, seq FROM segments WHERE id = ?1",
            params![segment_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )?;
        ensure!(
            existing
                == (
                    "active".to_owned(),
                    u64_to_i64(session_frame_start, "segment session frame start")?,
                    i64::from(segment_index)
                ),
            "segment start replay changed persisted identity"
        );
        tx.commit()?;
        Ok(())
    }

    pub fn mark_segment_finalizing(
        &self,
        session_id: &str,
        run_id: &str,
        segment_index: u32,
        session_frame_start: u64,
        frame_count: u64,
    ) -> anyhow::Result<()> {
        ensure!(frame_count > 0, "cannot finalize an empty segment");
        let id = segment_journal_id(session_id, run_id, segment_index);
        let mut connection = self.connection.lock();
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (state, existing_start, existing_frames) = tx.query_row(
            "SELECT state, session_frame_start, n_frames FROM segments WHERE id = ?1",
            params![id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                ))
            },
        )?;
        let expected_start = u64_to_i64(session_frame_start, "segment session frame start")?;
        let expected_frames = u64_to_i64(frame_count, "segment frame count")?;
        ensure!(
            existing_start == expected_start,
            "segment frame start changed before finalize"
        );
        match state.as_str() {
            "active" => {
                tx.execute(
                    "UPDATE segments SET state = 'finalizing', n_frames = ?2,
                            updated_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE id = ?1",
                    params![id, expected_frames],
                )?;
            }
            "finalizing" => ensure!(
                existing_frames == Some(expected_frames),
                "segment finalize replay changed frame count"
            ),
            _ => anyhow::bail!("illegal segment transition {state} -> finalizing"),
        }
        tx.commit()?;
        Ok(())
    }

    pub fn mark_segment_ready(
        &self,
        session_id: &str,
        run_id: &str,
        segment: &PublishedSegment,
        run_directory: &Path,
    ) -> anyhow::Result<()> {
        let artifact = validate_ready_artifact(session_id, run_id, segment, run_directory)?;
        let id = segment_journal_id(session_id, run_id, segment.index);
        let mut connection = self.connection.lock();
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let state = segment_state(&tx, &id)?;
        let journal_identity = tx.query_row(
            "SELECT s.schema_version,
                    s.session_contract, s.catalog_id, s.catalog_version, s.catalog_hash,
                    s.device_profile_id, s.device_profile_version, s.device_profile_hash,
                    s.recording_profile_id, s.recording_profile_version, s.recording_profile_hash,
                    s.game_id, s.game_profile_version, s.game_profile_hash,
                    s.mapping_id, s.mapping_version, s.mapping_hash,
                    r.fps_num, r.fps_den, r.width, r.height
             FROM sessions s
             JOIN segments g ON g.session_id = s.id
             JOIN capture_runs r ON r.id = g.run_id
             WHERE g.id = ?1",
            params![id],
            |row| {
                Ok(JournalCatalogIdentity {
                    schema_version: row.get(0)?,
                    session_contract: row.get(1)?,
                    catalog_id: row.get(2)?,
                    catalog_version: row.get(3)?,
                    catalog_hash: row.get(4)?,
                    device_id: row.get(5)?,
                    device_version: row.get(6)?,
                    device_hash: row.get(7)?,
                    recording_id: row.get(8)?,
                    recording_version: row.get(9)?,
                    recording_hash: row.get(10)?,
                    game_id: row.get(11)?,
                    game_version: row.get(12)?,
                    game_hash: row.get(13)?,
                    mapping_id: row.get(14)?,
                    mapping_version: row.get(15)?,
                    mapping_hash: row.get(16)?,
                    fps_num: row.get(17)?,
                    fps_den: row.get(18)?,
                    width: row.get(19)?,
                    height: row.get(20)?,
                })
            },
        )?;
        ensure!(
            journal_identity.mapping_id == artifact.mapping_id
                && journal_identity.mapping_version == artifact.mapping_version
                && journal_identity.mapping_hash == artifact.mapping_hash
                && journal_identity.fps_num == i64::from(artifact.fps_num)
                && journal_identity.fps_den == i64::from(artifact.fps_den)
                && journal_identity.width == i64::from(artifact.width)
                && journal_identity.height == i64::from(artifact.height),
            "ready artifact profile/mapping identity does not match the session journal"
        );
        // 这一场属于哪一代，是它**创建时**写进 `sessions.schema_version` 的历史事实
        // （issue #855 §B：唯一的路由键）。产物必须回到它自己那一代去核对，不能按
        // 今天的目录重新推断。未知版本 fail-closed，不落任何默认车道。
        let journal_contract =
            SessionContract::for_session_schema_version(&journal_identity.schema_version)
                .context("session journal carries an unsupported ingest contract")?;
        // 产物那一代与会话那一代必须相同。会话的契约在创建时写死，段是后来落的：
        // 两者不同只可能是一次半迁移，而半迁移正是本 issue 反复要拦的形状。
        ensure!(
            artifact.contract == journal_contract,
            "ready artifact ingest contract does not match the session journal"
        );
        ensure!(
            journal_identity.schema_version
                == journal_contract.session_schema_version(artifact.audio.is_some()),
            "ready artifact audio generation does not match the session journal schema"
        );
        {
            let catalog = artifact
                .catalog
                .as_ref()
                .context("versioned ready artifact is missing catalog identity")?;
            ensure!(
                artifact_catalog_matches_journal(catalog, &journal_identity, journal_contract),
                "ready artifact catalog identity does not match the session journal"
            );
        }
        // ready 是幂等重放的：重放不该再投一条镜像事件。outbox 的 seq 单调，
        // 重复投递会在云端占用两个不同 seq 而不是被去重掉。
        let audio_json = artifact
            .audio
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        let transitioned = match state.as_str() {
            "finalizing" => {
                let changed = tx.execute(
                    "UPDATE segments SET
                        state = 'ready', session_frame_start = ?2, n_frames = ?3,
                        qpc_start = ?4, qpc_end = ?5,
                        video_path = ?6, frames_path = ?7, events_path = ?8,
                        meta_path = ?9, manifest_path = ?10,
                        video_sha256 = ?11, frames_sha256 = ?12,
                        events_sha256 = ?13, meta_sha256 = ?14,
                        bytes = ?15, event_count = ?16,
                        duplicate_frames = ?17, source_superseded = ?18,
                        source_drops = ?19,
                        audio_json = ?20,
                        ready_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now'),
                        updated_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now')
                     WHERE id = ?1 AND state = 'finalizing'",
                    params![
                        id,
                        u64_to_i64(artifact.session_frame_start, "ready session frame start")?,
                        u64_to_i64(artifact.n_frames, "ready frame count")?,
                        artifact.qpc_start,
                        artifact.qpc_end,
                        artifact.video_path,
                        artifact.frames_path,
                        artifact.events_path,
                        artifact.meta_path,
                        artifact.manifest_path,
                        artifact.video_sha256,
                        artifact.frames_sha256,
                        artifact.events_sha256,
                        artifact.meta_sha256,
                        u64_to_i64(artifact.bytes, "ready video bytes")?,
                        u64_to_i64(artifact.event_count, "ready event count")?,
                        u64_to_i64(artifact.duplicate_frames, "ready duplicate frames")?,
                        u64_to_i64(artifact.source_superseded, "ready superseded frames")?,
                        u64_to_i64(artifact.source_drops, "ready source drops")?,
                        audio_json,
                    ],
                )?;
                ensure!(
                    changed == 1,
                    "ready transaction did not update exactly one segment"
                );
                true
            }
            "ready" => {
                validate_ready_replay(&tx, &id, &artifact)?;
                false
            }
            _ => anyhow::bail!("illegal segment transition {state} -> ready"),
        };
        let session_frame_end = artifact
            .session_frame_start
            .checked_add(artifact.n_frames)
            .context("ready segment frame interval overflowed")?;
        tx.execute(
            "UPDATE markers SET alignment_state = 'aligned', segment_seq = ?2
             WHERE run_id = ?1 AND alignment_state = 'pending'
               AND qpc >= ?3 AND qpc < ?4
               AND session_frame >= ?5 AND session_frame < ?6",
            params![
                run_journal_id(session_id, run_id),
                i64::from(segment.index),
                artifact.qpc_start,
                artifact.qpc_end,
                u64_to_i64(artifact.session_frame_start, "ready session frame start")?,
                u64_to_i64(session_frame_end, "ready session frame end")?,
            ],
        )?;
        queue_data_model_segment(&tx, session_id)?;
        // 与 segment ready 同事务：投递意图和事实同生同死。通知丢失、进程退出、
        // worker 中断都不会让一个已 ready 的段没有对应的镜像事件。
        if transitioned {
            enqueue_mirror_segment(&tx, session_id, run_id, segment.index)?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn finish_run(
        &self,
        outcome: &RecordingOutcome,
        terminal: RunTerminalState,
    ) -> anyhow::Result<()> {
        let id = run_journal_id(&outcome.session_id, &outcome.run_id);
        let stop_reason = stop_reason(outcome.stop_reason);
        let mut connection = self.connection.lock();
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = run_state(&tx, &id)?;
        let target = terminal.as_str();
        let clean_zero_frame_start = current == "starting"
            && outcome.frame_count == 0
            && outcome.segment_count == 0
            && outcome.anchor_qpc.is_none();
        ensure!(
            current == "recording" || current == target || clean_zero_frame_start,
            "illegal capture run transition {current} -> {target}"
        );
        if !clean_zero_frame_start {
            let persisted = tx.query_row(
                "SELECT anchor_qpc, anchor_at_utc, qpc_frequency
                 FROM capture_runs WHERE id = ?1",
                params![id],
                |row| {
                    Ok((
                        row.get::<_, Option<i64>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                    ))
                },
            )?;
            ensure!(
                persisted.0 == outcome.anchor_qpc
                    && persisted.2 == Some(outcome.qpc_frequency)
                    && (persisted.1.is_none() || persisted.1 == outcome.anchor_at_utc),
                "terminal run wall-clock/QPC identity differs from its Started event"
            );
        }
        tx.execute(
            "UPDATE capture_runs SET
                state = ?2, session_frame_end = ?3, frame_count = ?4,
                ready_segments = ?5, duplicate_frames = ?6, source_superseded = ?7,
                input_event_count = ?8, input_queue_overflows = ?9,
                qpc_start = ?10, qpc_end = ?11, stop_reason = ?12,
                ended_at_utc = COALESCE(ended_at_utc, strftime('%Y-%m-%dT%H:%M:%fZ','now')),
                updated_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now')
             WHERE id = ?1",
            params![
                id,
                target,
                u64_to_i64(outcome.session_frame_end, "run session frame end")?,
                u64_to_i64(outcome.frame_count, "run frame count")?,
                i64::from(outcome.segment_count),
                u64_to_i64(outcome.duplicate_frames, "run duplicate frames")?,
                u64_to_i64(outcome.source_superseded, "run superseded frames")?,
                u64_to_i64(outcome.input_event_count, "run input events")?,
                u64_to_i64(outcome.input_queue_overflows, "run input overflows")?,
                outcome.qpc_start,
                outcome.qpc_end,
                stop_reason,
            ],
        )?;
        tx.execute(
            "UPDATE segments SET state = 'aborted',
                    updated_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now')
             WHERE run_id = ?1 AND state = 'active'",
            params![id],
        )?;
        tx.execute(
            "UPDATE markers SET alignment_state = 'outside_video'
             WHERE run_id = ?1 AND alignment_state = 'pending'",
            params![id],
        )?;
        transition_session(
            &tx,
            &outcome.session_id,
            &["active", target],
            target,
            Some(stop_reason),
        )?;
        if terminal == RunTerminalState::Completed {
            request_data_model_finalize(&tx, &outcome.session_id)?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn fail_run(&self, session_id: &str, run_id: &str, error_code: &str) -> anyhow::Result<()> {
        let id = run_journal_id(session_id, run_id);
        let mut connection = self.connection.lock();
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = run_state(&tx, &id)?;
        if current != "faulted" {
            ensure!(
                matches!(current.as_str(), "starting" | "recording"),
                "illegal capture run transition {current} -> faulted"
            );
        }
        tx.execute(
            "UPDATE capture_runs SET state = 'faulted', error_code = ?2,
                    ended_at_utc = COALESCE(ended_at_utc, strftime('%Y-%m-%dT%H:%M:%fZ','now')),
                    updated_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now')
             WHERE id = ?1",
            params![id, error_code],
        )?;
        tx.execute(
            "UPDATE segments SET state = 'failed', error_code = ?2,
                    updated_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now')
             WHERE run_id = ?1 AND state IN ('active', 'finalizing')",
            params![id, error_code],
        )?;
        transition_session(
            &tx,
            session_id,
            &["active", "faulted"],
            "faulted",
            Some(error_code),
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn complete_session(&self, session_id: &str) -> anyhow::Result<()> {
        let mut connection = self.connection.lock();
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transition_session(
            &tx,
            session_id,
            &["paused", "completed"],
            "completed",
            Some("manual"),
        )?;
        request_data_model_finalize(&tx, session_id)?;
        tx.commit()?;
        Ok(())
    }

    #[cfg(not(feature = "local-catalog-only"))]
    pub fn fault_paused_session(&self, session_id: &str, error_code: &str) -> anyhow::Result<()> {
        let mut connection = self.connection.lock();
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transition_session(
            &tx,
            session_id,
            &["paused", "faulted"],
            "faulted",
            Some(error_code),
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn data_model_job(&self, session_id: &str) -> anyhow::Result<Option<DataModelJobStatus>> {
        let connection = self.connection.lock();
        load_data_model_job(&connection, session_id)
    }

    pub fn invalidated_data_model_sessions(&self) -> anyhow::Result<Vec<String>> {
        let connection = self.connection.lock();
        let mut statement = connection.prepare(
            "SELECT session_id FROM data_model_jobs
             WHERE state = 'failed' AND error_code = ?1
             ORDER BY session_id",
        )?;
        statement
            .query_map(params![DATA_MODEL_SOURCE_CORRUPT_CODE], |row| row.get(0))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub fn mark_data_model_empty(&self, session_id: &str) -> anyhow::Result<DataModelJobStatus> {
        let mut connection = self.connection.lock();
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = load_data_model_job(&tx, session_id)?
            .with_context(|| format!("DataModel job for session {session_id} is missing"))?;
        if current.state == DataModelJobState::Cancelled {
            ensure!(
                current.finalize_requested
                    && current.ready_segments == 0
                    && current.processed_segments == 0
                    && current.output_relative_path.is_none(),
                "cancelled DataModel job is not an empty completed session"
            );
            tx.commit()?;
            return Ok(current);
        }
        ensure!(
            current.state == DataModelJobState::Finalizing,
            "empty DataModel job cannot terminate from state {}",
            current.state.as_str()
        );
        ensure!(
            current.finalize_requested
                && current.ready_segments == 0
                && current.processed_segments == 0,
            "empty DataModel termination requires a finalized zero-segment session"
        );
        let session_state: String = tx.query_row(
            "SELECT state FROM sessions WHERE id = ?1",
            params![session_id],
            |row| row.get(0),
        )?;
        ensure!(
            session_state == "completed",
            "empty DataModel output cannot terminate for session state {session_state}"
        );
        tx.execute(
            "UPDATE data_model_jobs
             SET state = 'cancelled', published_chunks = 0, published_frames = 0,
                 pending_frames = 0, output_relative_path = NULL, error_code = NULL,
                 updated_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now')
             WHERE session_id = ?1",
            params![session_id],
        )?;
        let updated = load_data_model_job(&tx, session_id)?
            .context("empty DataModel job disappeared while terminating")?;
        tx.commit()?;
        Ok(updated)
    }

    #[cfg(test)]
    pub fn list_processable_data_model_jobs(
        &self,
        limit: u32,
    ) -> anyhow::Result<Vec<DataModelJobStatus>> {
        ensure!(
            (1..=1_000).contains(&limit),
            "DataModel job limit must be in 1..=1000"
        );
        let connection = self.connection.lock();
        let mut statement = connection.prepare(
            "SELECT session_id, state, ready_segments, processed_segments,
                    published_chunks, published_frames, pending_frames,
                    finalize_requested, output_relative_path, error_code
             FROM data_model_jobs
             WHERE state = 'pending'
             ORDER BY finalize_requested DESC, updated_at_utc, session_id
             LIMIT ?1",
        )?;
        let rows = statement
            .query_map(params![i64::from(limit)], data_model_job_from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter().map(data_model_job_from_sql).collect()
    }

    pub fn claim_next_data_model_job(&self) -> anyhow::Result<Option<DataModelJobStatus>> {
        let mut connection = self.connection.lock();
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let session_id = tx
            .query_row(
                "SELECT session_id FROM data_model_jobs
                 WHERE state = 'pending'
                 ORDER BY finalize_requested DESC, updated_at_utc, session_id
                 LIMIT 1",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        let Some(session_id) = session_id else {
            tx.commit()?;
            return Ok(None);
        };
        let changed = tx.execute(
            "UPDATE data_model_jobs
             SET state = CASE WHEN finalize_requested = 1
                              THEN 'finalizing' ELSE 'running' END,
                 error_code = NULL,
                 updated_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now')
             WHERE session_id = ?1 AND state = 'pending'",
            params![session_id],
        )?;
        ensure!(changed == 1, "DataModel job claim lost its pending row");
        let job = load_data_model_job(&tx, &session_id)?
            .context("claimed DataModel job disappeared before commit")?;
        tx.commit()?;
        Ok(Some(job))
    }

    pub fn ready_segments_for_data_model(
        &self,
        session_id: &str,
    ) -> anyhow::Result<Vec<DataModelReadySegmentRef>> {
        let connection = self.connection.lock();
        let mut statement = connection.prepare(
            "SELECT g.session_id, r.run_name, r.seq, g.seq,
                    g.session_frame_start, g.n_frames
             FROM segments g
             JOIN capture_runs r ON r.id = g.run_id
             WHERE g.session_id = ?1 AND g.state = 'ready'
             ORDER BY r.seq, g.seq",
        )?;
        statement
            .query_map(params![session_id], |row| {
                Ok(DataModelReadySegmentRef {
                    session_id: row.get(0)?,
                    run_name: row.get(1)?,
                    run_seq: row.get(2)?,
                    segment_index: row.get(3)?,
                    session_frame_start: row.get(4)?,
                    frame_count: row.get(5)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub fn data_model_wall_clock_context(
        &self,
        session_id: &str,
    ) -> anyhow::Result<Option<DataModelWallClockContext>> {
        let connection = self.connection.lock();
        let stored = connection
            .query_row(
                "SELECT anchor_at_utc, anchor_qpc, qpc_frequency
                 FROM capture_runs
                 WHERE session_id = ?1
                   AND anchor_qpc IS NOT NULL
                   AND qpc_frequency IS NOT NULL
                 ORDER BY seq
                 LIMIT 1",
                params![session_id],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<i64>>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                    ))
                },
            )
            .optional()?;
        let Some((anchor_at_utc, anchor_qpc, qpc_frequency)) = stored else {
            return Ok(None);
        };
        let (Some(anchor_qpc), Some(qpc_frequency)) = (anchor_qpc, qpc_frequency) else {
            ensure!(
                anchor_qpc.is_none() && qpc_frequency.is_none() && anchor_at_utc.is_none(),
                "first capture run contains an incomplete DataModel wall-clock context"
            );
            return Ok(None);
        };
        ensure!(
            qpc_frequency > 0,
            "first capture run has a non-positive QPC frequency"
        );
        ensure!(
            anchor_at_utc
                .as_deref()
                .is_none_or(|value| !value.trim().is_empty()),
            "first capture run has an empty UTC anchor"
        );
        Ok(Some(DataModelWallClockContext {
            anchor_at_utc,
            anchor_qpc,
            qpc_frequency,
        }))
    }

    pub fn save_data_model_progress(
        &self,
        session_id: &str,
        progress: &DataModelProgress,
    ) -> anyhow::Result<DataModelJobStatus> {
        let mut connection = self.connection.lock();
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = load_data_model_job(&tx, session_id)?
            .with_context(|| format!("DataModel job for session {session_id} is missing"))?;
        ensure!(
            matches!(
                current.state,
                DataModelJobState::Running | DataModelJobState::Finalizing
            ),
            "DataModel progress cannot be saved from state {}",
            current.state.as_str()
        );
        validate_data_model_progress(&current, progress)?;
        let next_state =
            if progress.processed_segments < current.ready_segments || current.finalize_requested {
                DataModelJobState::Pending
            } else {
                DataModelJobState::Waiting
            };
        update_data_model_progress(&tx, session_id, progress, next_state, None, None)?;
        let updated = load_data_model_job(&tx, session_id)?
            .context("DataModel job disappeared while saving progress")?;
        tx.commit()?;
        Ok(updated)
    }

    pub fn mark_data_model_completed(
        &self,
        session_id: &str,
        progress: &DataModelProgress,
        output_relative_path: &str,
    ) -> anyhow::Result<DataModelJobStatus> {
        validate_data_model_output_path(output_relative_path)?;
        let mut connection = self.connection.lock();
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = load_data_model_job(&tx, session_id)?
            .with_context(|| format!("DataModel job for session {session_id} is missing"))?;
        if current.state == DataModelJobState::Completed {
            ensure!(
                current.processed_segments == progress.processed_segments
                    && current.published_chunks == progress.published_chunks
                    && current.published_frames == progress.published_frames
                    && current.pending_frames == progress.pending_frames
                    && current.output_relative_path.as_deref() == Some(output_relative_path),
                "completed DataModel job replay changed its output identity"
            );
            tx.commit()?;
            return Ok(current);
        }
        ensure!(
            !matches!(
                current.state,
                DataModelJobState::Cancelled | DataModelJobState::Failed
            ),
            "DataModel job cannot complete from state {}",
            current.state.as_str()
        );
        ensure!(
            current.finalize_requested,
            "DataModel job cannot complete before session finalization"
        );
        let session_state: String = tx.query_row(
            "SELECT state FROM sessions WHERE id = ?1",
            params![session_id],
            |row| row.get(0),
        )?;
        ensure!(
            session_state == "completed",
            "DataModel output cannot complete for session state {session_state}"
        );
        validate_data_model_progress(&current, progress)?;
        ensure!(
            progress.processed_segments == current.ready_segments,
            "DataModel output cannot complete with unprocessed ready segments"
        );
        update_data_model_progress(
            &tx,
            session_id,
            progress,
            DataModelJobState::Completed,
            Some(output_relative_path),
            None,
        )?;
        let updated = load_data_model_job(&tx, session_id)?
            .context("DataModel job disappeared while completing")?;
        tx.commit()?;
        Ok(updated)
    }

    pub fn next_session_artifact_freeze_candidate(&self) -> anyhow::Result<Option<String>> {
        let connection = self.connection.lock();
        connection
            .query_row(
                "SELECT s.id
                   FROM sessions s
                   JOIN data_model_jobs d ON d.session_id = s.id
                  WHERE s.state = 'completed'
                    AND s.non_trainable = 1
                    AND s.production_certified = 0
                    -- 'cancelled' here is the empty session's durable terminal:
                    -- its only writer is mark_data_model_empty, which requires a
                    -- finalized zero-segment job on a completed session. An
                    -- empty session still has a session root worth auditing, so
                    -- it must not be invisible to artifact freezing.
                    AND d.state IN ('completed', 'cancelled')
                    AND NOT EXISTS (
                        SELECT 1 FROM session_artifact_uploads a WHERE a.session_id = s.id
                    )
                  ORDER BY s.ended_at_utc, s.id
                  LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn enqueue_session_artifacts(
        &self,
        session_id: &str,
        sources: &[SessionArtifactSource],
    ) -> anyhow::Result<()> {
        validate_session_artifact_sources(session_id, sources)?;
        let mut connection = self.connection.lock();
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (session_state, non_trainable, production_certified, data_model_state): (
            String,
            i64,
            i64,
            String,
        ) = tx.query_row(
            "SELECT s.state, s.non_trainable, s.production_certified, d.state
               FROM sessions s
               JOIN data_model_jobs d ON d.session_id = s.id
              WHERE s.id = ?1",
            params![session_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
        ensure!(
            session_state == "completed"
                && non_trainable == 1
                && production_certified == 0
                // 'cancelled' is the empty session's durable terminal (sole
                // writer: mark_data_model_empty, finalized zero-segment only).
                && matches!(data_model_state.as_str(), "completed" | "cancelled"),
            "session artifacts require a durable completed non-trainable session"
        );
        tx.execute(
            "INSERT INTO upload_sessions(
                session_id, registration_state, created_at_utc, updated_at_utc
             ) VALUES (
                ?1, 'completed',
                strftime('%Y-%m-%dT%H:%M:%fZ','now'),
                strftime('%Y-%m-%dT%H:%M:%fZ','now')
             ) ON CONFLICT(session_id) DO NOTHING",
            params![session_id],
        )?;

        let existing: i64 = tx.query_row(
            "SELECT COUNT(*) FROM session_artifact_uploads WHERE session_id = ?1",
            params![session_id],
            |row| row.get(0),
        )?;
        if existing == 0 {
            for source in sources {
                tx.execute(
                    "INSERT INTO session_artifact_uploads(
                        session_id, kind, local_path, object_key,
                        expected_sha256, expected_size, content_md5,
                        state, attempts, next_attempt_at, verified_at,
                        last_error_code, created_at_utc, updated_at_utc
                     ) VALUES (
                        ?1, ?2, ?3, NULL, ?4, ?5, ?6,
                        'pending', 0, 0, NULL, NULL,
                        strftime('%Y-%m-%dT%H:%M:%fZ','now'),
                        strftime('%Y-%m-%dT%H:%M:%fZ','now')
                     )",
                    params![
                        session_id,
                        source.kind.as_str(),
                        source.relative_path,
                        source.expected_sha256,
                        u64_to_i64(source.expected_size, "session artifact size")?,
                        source.content_md5,
                    ],
                )?;
            }
        } else {
            ensure!(
                existing == i64::try_from(sources.len())?,
                "session artifact enqueue replay changed the artifact set"
            );
            for source in sources {
                let persisted: (String, String, i64, String) = tx.query_row(
                    "SELECT local_path, expected_sha256, expected_size, content_md5
                       FROM session_artifact_uploads
                      WHERE session_id = ?1 AND kind = ?2",
                    params![session_id, source.kind.as_str()],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )?;
                ensure!(
                    persisted
                        == (
                            source.relative_path.clone(),
                            source.expected_sha256.clone(),
                            u64_to_i64(source.expected_size, "session artifact size")?,
                            source.content_md5.clone(),
                        ),
                    "session artifact enqueue replay changed {}",
                    source.kind.as_str()
                );
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn session_registration_candidate(
        &self,
        session_id: &str,
    ) -> anyhow::Result<SessionRegistrationCandidate> {
        let connection = self.connection.lock();
        connection
            .query_row(
                "SELECT s.id, us.registration_state, s.state, s.schema_version,
                        s.started_at_utc, s.ended_at_utc, s.stop_reason,
                        s.non_trainable, s.consent_id,
                        s.session_contract, s.catalog_id, s.catalog_version, s.catalog_hash,
                        s.device_profile_id, s.device_profile_version, s.device_profile_hash,
                        s.recording_profile_id, s.recording_profile_version, s.recording_profile_hash,
                        s.game_id, s.game_profile_version, s.game_profile_hash, s.game_exe_hash,
                        s.mapping_id, s.mapping_version, s.mapping_hash,
                        r.width, r.height, r.fps_num, r.fps_den, r.backend,
                        s.game_exe_hash_missing_reason,
                        us.remote_session_id, us.sealed
                   FROM sessions s
                   JOIN upload_sessions us ON us.session_id = s.id
                   JOIN capture_runs r ON r.session_id = s.id
                  WHERE s.id = ?1
                  ORDER BY r.seq
                  LIMIT 1",
                params![session_id],
                |row| {
                    Ok(SessionRegistrationCandidate {
                        local_session_id: row.get(0)?,
                        registration_state: row.get(1)?,
                        session_state: row.get(2)?,
                        session_schema_version: row.get(3)?,
                        started_at_utc: row.get(4)?,
                        ended_at_utc: row.get(5)?,
                        stop_reason: row.get(6)?,
                        non_trainable: row.get(7)?,
                        consent_id: row.get(8)?,
                        session_contract: row.get(9)?,
                        catalog_id: row.get(10)?,
                        catalog_version: row.get(11)?,
                        catalog_hash: row.get(12)?,
                        device_profile_id: row.get(13)?,
                        device_profile_version: row.get(14)?,
                        device_profile_hash: row.get(15)?,
                        recording_profile_id: row.get(16)?,
                        recording_profile_version: row.get(17)?,
                        recording_profile_hash: row.get(18)?,
                        game_id: row.get(19)?,
                        game_profile_version: row.get(20)?,
                        game_profile_hash: row.get(21)?,
                        game_executable_sha256: row.get(22)?,
                        mapping_id: row.get(23)?,
                        mapping_version: row.get(24)?,
                        mapping_hash: row.get(25)?,
                        width: row.get(26)?,
                        height: row.get(27)?,
                        fps_num: row.get(28)?,
                        fps_den: row.get(29)?,
                        capture_engine: row.get(30)?,
                        game_executable_sha256_missing_reason: row.get(31)?,
                        remote_session_id: row.get(32)?,
                        remote_session_sealed: row.get(33)?,
                    })
                },
            )
            .with_context(|| format!("session {session_id} has no upload registration candidate"))
    }

    pub fn session_artifact_jobs(
        &self,
        session_id: &str,
    ) -> anyhow::Result<Vec<SessionArtifactUploadJob>> {
        let connection = self.connection.lock();
        load_session_artifact_jobs(&connection, session_id)
    }

    pub fn claim_next_session_artifact_job(
        &self,
    ) -> anyhow::Result<Option<SessionArtifactUploadJob>> {
        let mut connection = self.connection.lock();
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let selected = tx
            .query_row(
                "SELECT a.session_id, a.kind
                   FROM session_artifact_uploads a
                   JOIN upload_sessions us ON us.session_id = a.session_id
                   JOIN data_model_jobs d ON d.session_id = a.session_id
                   WHERE a.state NOT IN ('verified', 'rejected')
                    AND us.upload_requested = 1
                    AND a.next_attempt_at <= CAST(unixepoch('subsec') * 1000 AS INTEGER)
                    AND (
                        us.sealed = 1
                        OR (
                            -- 'cancelled' = the empty session's durable terminal
                            -- (sole writer mark_data_model_empty, zero segments),
                            -- so the clip-drain conditions below hold trivially.
                            d.state IN ('completed', 'cancelled')
                            AND NOT EXISTS (
                                SELECT 1 FROM upload_jobs j
                                 WHERE j.session_id = a.session_id
                                   AND j.state NOT IN ('stored', 'verified')
                            )
                            AND (
                                SELECT COUNT(*) FROM upload_jobs j
                                 WHERE j.session_id = a.session_id
                            ) = d.processed_segments
                        )
                    )
                  ORDER BY a.next_attempt_at, a.updated_at_utc, a.session_id, a.kind
                  LIMIT 1",
                [],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?;
        let Some((session_id, kind)) = selected else {
            tx.commit()?;
            return Ok(None);
        };
        let changed = tx.execute(
            "UPDATE session_artifact_uploads
                SET attempts = attempts + 1, last_error_code = NULL,
                    updated_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now')
              WHERE session_id = ?1 AND kind = ?2
                AND state NOT IN ('verified', 'rejected')",
            params![session_id, kind],
        )?;
        ensure!(changed == 1, "session artifact claim lost its durable row");
        let job = load_session_artifact_job(&tx, &session_id, &kind)?
            .context("claimed session artifact disappeared")?;
        tx.commit()?;
        Ok(Some(job))
    }

    pub fn set_session_artifacts_declared(
        &self,
        session_id: &str,
        statuses: &[SessionArtifactRemoteStatus],
    ) -> anyhow::Result<()> {
        let mut connection = self.connection.lock();
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let local = load_session_artifact_jobs(&tx, session_id)?;
        ensure!(
            local.len() == statuses.len(),
            "remote session artifact declaration changed the artifact set"
        );
        for job in local {
            let status = statuses
                .iter()
                .find(|status| status.kind == job.kind)
                .with_context(|| {
                    format!(
                        "remote declaration omitted session artifact {}",
                        job.kind.as_str()
                    )
                })?;
            ensure!(
                status.expected_sha256 == job.expected_sha256
                    && status.expected_size == job.expected_size
                    && !status.object_key.trim().is_empty()
                    && job
                        .object_key
                        .as_deref()
                        .is_none_or(|persisted| persisted == status.object_key),
                "remote declaration conflicts with local session artifact {}",
                job.kind.as_str()
            );
            let target = match status.verification_state.as_str() {
                "verified" => SessionArtifactUploadState::Verified,
                "rejected" => SessionArtifactUploadState::Rejected,
                "unverified" | "queued" | "verifying" => SessionArtifactUploadState::Declared,
                other => anyhow::bail!("unsupported remote session artifact state {other}"),
            };
            let verified_at = (target == SessionArtifactUploadState::Verified)
                .then_some("strftime('%Y-%m-%dT%H:%M:%fZ','now')");
            let sql = if verified_at.is_some() {
                "UPDATE session_artifact_uploads
                    SET object_key = ?3, state = ?4,
                        verified_at = strftime('%Y-%m-%dT%H:%M:%fZ','now'),
                        last_error_code = NULL,
                        updated_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now')
                  WHERE session_id = ?1 AND kind = ?2"
            } else {
                "UPDATE session_artifact_uploads
                    SET object_key = ?3, state = ?4, verified_at = NULL,
                        last_error_code = CASE WHEN ?4 = 'rejected'
                                              THEN 'UPLOAD_REMOTE_REJECTED' ELSE NULL END,
                        updated_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now')
                  WHERE session_id = ?1 AND kind = ?2"
            };
            tx.execute(
                sql,
                params![
                    session_id,
                    job.kind.as_str(),
                    status.object_key,
                    target.as_str()
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn mark_session_artifact_signed(
        &self,
        session_id: &str,
        kind: SessionArtifactKind,
    ) -> anyhow::Result<()> {
        transition_session_artifact_upload(
            &self.connection,
            session_id,
            kind,
            SessionArtifactUploadState::Signed,
            None,
        )
    }

    pub fn mark_session_artifact_put(
        &self,
        session_id: &str,
        kind: SessionArtifactKind,
    ) -> anyhow::Result<()> {
        transition_session_artifact_upload(
            &self.connection,
            session_id,
            kind,
            SessionArtifactUploadState::Put,
            None,
        )
    }

    pub fn mark_session_artifact_completing(
        &self,
        session_id: &str,
        kind: SessionArtifactKind,
    ) -> anyhow::Result<()> {
        transition_session_artifact_upload(
            &self.connection,
            session_id,
            kind,
            SessionArtifactUploadState::Completing,
            None,
        )
    }

    pub fn finish_session_artifact(
        &self,
        session_id: &str,
        kind: SessionArtifactKind,
        verified: bool,
    ) -> anyhow::Result<()> {
        transition_session_artifact_upload(
            &self.connection,
            session_id,
            kind,
            if verified {
                SessionArtifactUploadState::Verified
            } else {
                SessionArtifactUploadState::Rejected
            },
            (!verified).then_some("UPLOAD_REMOTE_REJECTED"),
        )
    }

    pub fn retry_session_artifact(
        &self,
        session_id: &str,
        kind: SessionArtifactKind,
        error_code: &str,
        delay_ms: u64,
    ) -> anyhow::Result<()> {
        ensure_upload_error_code(error_code)?;
        let connection = self.connection.lock();
        let changed = connection.execute(
            "UPDATE session_artifact_uploads
                SET last_error_code = ?3,
                    next_attempt_at = CAST(unixepoch('subsec') * 1000 AS INTEGER) + ?4,
                    updated_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now')
              WHERE session_id = ?1 AND kind = ?2
                AND state NOT IN ('verified', 'rejected')",
            params![
                session_id,
                kind.as_str(),
                error_code,
                u64_to_i64(delay_ms, "session artifact retry delay")?
            ],
        )?;
        ensure!(changed == 1, "session artifact retry lost its durable row");
        Ok(())
    }

    pub fn reject_session_artifact(
        &self,
        session_id: &str,
        kind: SessionArtifactKind,
        error_code: &str,
    ) -> anyhow::Result<()> {
        ensure_upload_error_code(error_code)?;
        transition_session_artifact_upload(
            &self.connection,
            session_id,
            kind,
            SessionArtifactUploadState::Rejected,
            Some(error_code),
        )
    }

    pub fn claim_next_upload_job(&self) -> anyhow::Result<Option<UploadJob>> {
        let mut connection = self.connection.lock();
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let segment_id = tx
            .query_row(
                "SELECT j.segment_id
                   FROM upload_jobs j
                   JOIN upload_sessions us ON us.session_id = j.session_id
                   JOIN sessions s ON s.id = j.session_id
                   JOIN capture_runs r ON r.id = j.run_id
                  WHERE j.state IN ('pending', 'retry_wait')
                    AND us.upload_requested = 1
                    AND j.next_attempt_at_ms <= unixepoch('subsec') * 1000
                    AND s.state IN ('paused', 'completed', 'faulted', 'corrupt')
                    AND r.state IN ('paused', 'completed', 'faulted')
                    AND (
                        r.depth_expected = 0
                        OR EXISTS (
                            SELECT 1 FROM segments depth_segment
                             WHERE depth_segment.id = j.segment_id
                               AND depth_segment.depth_path IS NOT NULL
                               AND depth_segment.depth_sha256 IS NOT NULL
                               AND depth_segment.depth_upload_path IS NOT NULL
                               AND depth_segment.depth_upload_sha256 IS NOT NULL
                        )
                    )
                    AND NOT EXISTS (
                        SELECT 1 FROM session_deletions d
                         WHERE d.session_id = j.session_id
                           AND d.state IN ('prepared', 'staged', 'cleanup_pending')
                    )
                  ORDER BY j.created_at_utc, j.segment_id
                  LIMIT 1",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        let Some(segment_id) = segment_id else {
            tx.commit()?;
            return Ok(None);
        };
        freeze_upload_registration_states(&tx, &segment_id)?;
        let changed = tx.execute(
            "UPDATE upload_jobs
                SET state = 'running', attempt_count = attempt_count + 1,
                    last_error_code = NULL,
                    http_status = NULL, server_code = NULL,
                    server_message = NULL, request_id = NULL,
                    updated_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now')
              WHERE segment_id = ?1 AND state IN ('pending', 'retry_wait')",
            params![segment_id],
        )?;
        ensure!(changed == 1, "upload job claim lost its processable row");
        let job = load_upload_job(&tx, &segment_id)?.context("claimed upload job disappeared")?;
        tx.commit()?;
        Ok(Some(job))
    }

    pub fn requeue_interrupted_upload_jobs(&self) -> anyhow::Result<()> {
        recover_interrupted_upload_jobs(&self.connection.lock())
    }

    /// 上传队列的聚合读数（docs/65 J-29）。uploader worker 每轮调用一次并推进
    /// 快照；这里只做四条小聚合查询，绝不碰任何任务状态。
    pub fn upload_queue_status(&self) -> anyhow::Result<UploadQueueStatus> {
        let connection = self.connection.lock();
        let (pending, inflight, stored, rejected) = connection.query_row(
            "SELECT COALESCE(SUM(state IN ('pending', 'retry_wait')), 0),
                    COALESCE(SUM(state = 'running'), 0),
                    COALESCE(SUM(state = 'stored'), 0),
                    COALESCE(SUM(state = 'rejected'), 0)
               FROM upload_jobs",
            [],
            |row| {
                Ok((
                    row.get::<_, u32>(0)?,
                    row.get::<_, u32>(1)?,
                    row.get::<_, u32>(2)?,
                    row.get::<_, u32>(3)?,
                ))
            },
        )?;
        let pending_session_artifacts: u32 = connection.query_row(
            "SELECT COALESCE(SUM(state NOT IN ('verified', 'rejected')), 0)
               FROM session_artifact_uploads",
            [],
            |row| row.get(0),
        )?;
        let last_error_code: Option<String> = connection
            .query_row(
                "SELECT last_error_code FROM (
                    SELECT last_error_code, updated_at_utc FROM upload_jobs
                     WHERE last_error_code IS NOT NULL
                    UNION ALL
                    SELECT last_error_code, updated_at_utc FROM session_artifact_uploads
                     WHERE last_error_code IS NOT NULL
                    UNION ALL
                    SELECT seal_last_error_code, updated_at_utc FROM upload_sessions
                     WHERE seal_last_error_code IS NOT NULL
                 ) ORDER BY updated_at_utc DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()?;
        let next_retry_unix_ms: Option<i64> = connection.query_row(
            "SELECT MIN(next_attempt) FROM (
                SELECT MIN(next_attempt_at_ms) AS next_attempt FROM upload_jobs
                 WHERE state = 'retry_wait'
                UNION ALL
                SELECT MIN(next_attempt_at) FROM session_artifact_uploads
                  WHERE state NOT IN ('verified', 'rejected') AND last_error_code IS NOT NULL
                UNION ALL
                SELECT MIN(seal_next_attempt_at_ms) FROM upload_sessions
                 WHERE seal_state = 'retry_wait'
             )",
            [],
            |row| row.get(0),
        )?;
        Ok(UploadQueueStatus {
            pending_segments: pending,
            inflight_segments: inflight,
            awaiting_verification_segments: stored,
            rejected_segments: rejected,
            pending_session_artifacts,
            last_error_code,
            next_retry_unix_ms,
        })
    }

    pub fn upload_candidate(&self, segment_id: &str) -> anyhow::Result<UploadCandidate> {
        let connection = self.connection.lock();
        connection
            .query_row(
                "SELECT g.id, s.id, us.registration_state, s.state, s.schema_version,
                        s.started_at_utc, s.ended_at_utc, s.stop_reason,
                        s.session_contract, s.catalog_id, s.catalog_version, s.catalog_hash,
                        s.device_profile_id, s.device_profile_version, s.device_profile_hash,
                        s.recording_profile_id, s.recording_profile_version, s.recording_profile_hash,
                        s.game_id, s.game_profile_version, s.game_profile_hash, s.game_exe_hash,
                        s.mapping_id, s.mapping_version, s.mapping_hash, us.remote_session_id,
                        r.run_name, ur.registration_state, r.seq, r.started_at_utc, r.ended_at_utc,
                        r.qpc_frequency, r.session_frame_start, r.preflight_id,
                        p.state, p.evidence_hash, r.backend, r.encoder_backend,
                        r.width, r.height, r.fps_num, r.fps_den, ur.remote_run_id,
                        g.seq, g.session_frame_start, g.n_frames, g.qpc_start, g.qpc_end,
                        g.event_count, g.duplicate_frames, g.source_superseded, g.source_drops,
                        j.remote_clip_id, j.clip_revision, j.remote_upload_intent_id,
                        g.video_path, g.frames_path, g.events_path, g.meta_path, g.manifest_path,
                        g.video_sha256, g.frames_sha256, g.events_sha256, g.meta_sha256,
                        r.id, s.non_trainable, s.consent_id,
                        s.game_exe_hash_missing_reason, g.audio_json,
                        r.frame_count, r.stop_reason, r.error_code,
                        g.depth_path, g.depth_sha256,
                        g.depth_upload_path, g.depth_upload_sha256,
                        r.depth_expected
                   FROM upload_jobs j
                   JOIN segments g ON g.id = j.segment_id
                   JOIN sessions s ON s.id = g.session_id
                   JOIN upload_sessions us ON us.session_id = s.id
                   JOIN capture_runs r ON r.id = g.run_id
                   JOIN upload_runs ur ON ur.run_id = r.id
                   JOIN preflight_attempts p ON p.id = r.preflight_id
                  WHERE j.segment_id = ?1 AND g.state = 'ready'",
                params![segment_id],
                |row| {
                    let manifest_relative_path = row.get::<_, String>(59)?;
                    let audio = parse_audio_json_row(row.get(68)?, 68)?;
                    // v28 把收口 manifest 与真实 ZIP 分开。manifest 在而 ZIP 不在表示
                    // 本轮确实尝试了 depth、但该段不合格或尚未完整物化；此时必须拒绝
                    // 整个 clip upload，不能把它悄悄降级成纯视频。
                    let depth_manifest = parse_depth_artifact_row(
                        row.get::<_, Option<String>>(72)?,
                        row.get::<_, Option<String>>(73)?,
                    )?;
                    let depth_upload = parse_depth_upload_artifact_row(
                        row.get::<_, bool>(76)?,
                        depth_manifest.as_ref(),
                        row.get::<_, Option<String>>(74)?,
                        row.get::<_, Option<String>>(75)?,
                    )?;
                    let mut artifacts = vec![
                        UploadArtifactSource {
                            kind: "video",
                            relative_path: row.get(55)?,
                            expected_sha256: row.get(60)?,
                        },
                        UploadArtifactSource {
                            kind: "frames",
                            relative_path: row.get(56)?,
                            expected_sha256: row.get(61)?,
                        },
                        UploadArtifactSource {
                            kind: "events",
                            relative_path: row.get(57)?,
                            expected_sha256: row.get(62)?,
                        },
                        UploadArtifactSource {
                            kind: "meta",
                            relative_path: row.get(58)?,
                            expected_sha256: row.get(63)?,
                        },
                        UploadArtifactSource {
                            kind: "manifest",
                            relative_path: manifest_relative_path.clone(),
                            expected_sha256: None,
                        },
                    ];
                    if let Some(audio) = audio.as_ref().filter(|audio| audio.has_artifact()) {
                        let prefix = manifest_relative_path
                            .strip_suffix("sha256.json")
                            .ok_or_else(|| {
                                rusqlite::Error::InvalidColumnType(
                                    59,
                                    "manifest_path".into(),
                                    rusqlite::types::Type::Text,
                                )
                            })?;
                        artifacts.push(UploadArtifactSource {
                            kind: "audio",
                            relative_path: format!("{prefix}{AUDIO_ARTIFACT_NAME}"),
                            expected_sha256: audio.sha256.clone(),
                        });
                    }
                    if let Some((relative_path, sha256)) = depth_upload {
                        artifacts.push(UploadArtifactSource {
                            kind: "depth",
                            relative_path,
                            expected_sha256: Some(sha256),
                        });
                    }
                    Ok(UploadCandidate {
                        segment_id: row.get(0)?,
                        local_session_id: row.get(1)?,
                        session_registration_state: row.get(2)?,
                        session_state: row.get(3)?,
                        session_schema_version: row.get(4)?,
                        session_started_at_utc: row.get(5)?,
                        session_ended_at_utc: row.get(6)?,
                        session_stop_reason: row.get(7)?,
                        non_trainable: row.get::<_, i64>(65)? != 0,
                        consent_id: row.get(66)?,
                        game_executable_sha256_missing_reason: row.get(67)?,
                        session_contract: row.get(8)?,
                        catalog_id: row.get(9)?,
                        catalog_version: row.get(10)?,
                        catalog_hash: row.get(11)?,
                        device_profile_id: row.get(12)?,
                        device_profile_version: row.get(13)?,
                        device_profile_hash: row.get(14)?,
                        recording_profile_id: row.get(15)?,
                        recording_profile_version: row.get(16)?,
                        recording_profile_hash: row.get(17)?,
                        game_id: row.get(18)?,
                        game_profile_version: row.get(19)?,
                        game_profile_hash: row.get(20)?,
                        game_executable_sha256: row.get(21)?,
                        mapping_id: row.get(22)?,
                        mapping_version: row.get(23)?,
                        mapping_hash: row.get(24)?,
                        remote_session_id: row.get(25)?,
                        run_journal_id: row.get(64)?,
                        local_run_id: row.get(26)?,
                        run_registration_state: row.get(27)?,
                        run_sequence: row.get(28)?,
                        run_started_at_utc: row.get::<_, Option<String>>(29)?.ok_or_else(|| {
                            rusqlite::Error::InvalidColumnType(
                                29,
                                "started_at_utc".into(),
                                rusqlite::types::Type::Null,
                            )
                        })?,
                        run_ended_at_utc: row.get::<_, Option<String>>(30)?.ok_or_else(|| {
                            rusqlite::Error::InvalidColumnType(
                                30,
                                "ended_at_utc".into(),
                                rusqlite::types::Type::Null,
                            )
                        })?,
                        qpc_frequency: row.get::<_, Option<i64>>(31)?.ok_or_else(|| {
                            rusqlite::Error::InvalidColumnType(
                                31,
                                "qpc_frequency".into(),
                                rusqlite::types::Type::Null,
                            )
                        })?,
                        session_frame_start: row.get(32)?,
                        preflight_id: row.get::<_, Option<String>>(33)?.ok_or_else(|| {
                            rusqlite::Error::InvalidColumnType(
                                33,
                                "preflight_id".into(),
                                rusqlite::types::Type::Null,
                            )
                        })?,
                        preflight_result: row.get(34)?,
                        preflight_evidence_hash: row.get(35)?,
                        capture_engine: row.get(36)?,
                        encoder_backend: row.get(37)?,
                        width: row.get(38)?,
                        height: row.get(39)?,
                        fps_num: row.get(40)?,
                        fps_den: row.get(41)?,
                        remote_run_id: row.get(42)?,
                        run_frame_count: row.get::<_, Option<u64>>(69)?,
                        run_stop_reason: row.get(70)?,
                        run_error_code: row.get(71)?,
                        segment_index: row.get(43)?,
                        segment_session_frame_start: row.get(44)?,
                        frame_count: row.get::<_, Option<u64>>(45)?.ok_or_else(|| {
                            rusqlite::Error::InvalidColumnType(
                                45,
                                "n_frames".into(),
                                rusqlite::types::Type::Null,
                            )
                        })?,
                        qpc_start: row.get::<_, Option<i64>>(46)?.ok_or_else(|| {
                            rusqlite::Error::InvalidColumnType(
                                46,
                                "qpc_start".into(),
                                rusqlite::types::Type::Null,
                            )
                        })?,
                        qpc_end: row.get::<_, Option<i64>>(47)?.ok_or_else(|| {
                            rusqlite::Error::InvalidColumnType(
                                47,
                                "qpc_end".into(),
                                rusqlite::types::Type::Null,
                            )
                        })?,
                        event_count: row.get::<_, Option<u64>>(48)?.unwrap_or(0),
                        duplicate_frames: row.get::<_, Option<u64>>(49)?.unwrap_or(0),
                        source_superseded: row.get::<_, Option<u64>>(50)?.unwrap_or(0),
                        source_drops: row.get::<_, Option<u64>>(51)?.unwrap_or(0),
                        remote_clip_id: row.get(52)?,
                        clip_revision: row.get(53)?,
                        remote_upload_intent_id: row.get(54)?,
                        audio,
                        artifacts,
                    })
                },
            )
            .with_context(|| format!("upload candidate {segment_id} is incomplete"))
            .and_then(|candidate| {
                // session schema 与"有没有音频"必须配对。legacy 用 `-2` / `-3` 表达
                // 音频世代，所以配对是硬约束；中性只有一个值，音频改由 clip 的
                // artifact 事实回答（服务端 `audioDeclaration: "per_clip"`），
                // 于是那一代**没有**可配错的东西——`session_schema_declares_audio`
                // 返回 `None` 正是"这一代不声明音频"。
                let contract = SessionContract::for_session_schema_version(
                    &candidate.session_schema_version,
                )
                .context("upload candidate carries an unsupported session schema")?;
                if let Some(declares_audio) =
                    SessionContract::session_schema_declares_audio(&candidate.session_schema_version)
                {
                    ensure!(
                        declares_audio == candidate.audio.is_some(),
                        "upload candidate session/audio schema pairing is invalid"
                    );
                }
                let _ = contract;
                Ok(candidate)
            })
    }

    #[cfg(any(feature = "depth-lab", test))]
    pub fn depth_segment_boundaries(
        &self,
        session_id: &str,
        run_id: &str,
    ) -> anyhow::Result<Vec<DepthSegmentBoundary>> {
        let connection = self.connection.lock();
        let mut statement = connection.prepare(
            "SELECT g.seq, g.session_frame_start, g.n_frames
               FROM segments g
               JOIN capture_runs r ON r.id = g.run_id
              WHERE g.session_id = ?1 AND r.run_name = ?2 AND g.state = 'ready'
              ORDER BY g.seq",
        )?;
        let boundaries = statement
            .query_map(params![session_id, run_id], |row| {
                Ok(DepthSegmentBoundary {
                    segment_index: row.get(0)?,
                    session_frame_start: row.get(1)?,
                    frame_count: row.get::<_, Option<u64>>(2)?.ok_or_else(|| {
                        rusqlite::Error::InvalidColumnType(
                            2,
                            "n_frames".into(),
                            rusqlite::types::Type::Null,
                        )
                    })?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        ensure!(
            !boundaries.is_empty(),
            "depth finalization requires at least one ready segment"
        );
        Ok(boundaries)
    }

    #[cfg(any(feature = "depth-lab", test))]
    pub fn depth_run_fps(&self, session_id: &str, run_id: &str) -> anyhow::Result<(u32, u32)> {
        self.connection
            .lock()
            .query_row(
                "SELECT fps_num, fps_den FROM capture_runs
                  WHERE session_id = ?1 AND run_name = ?2 AND depth_expected = 1",
                params![session_id, run_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .with_context(|| {
                format!("depth run {session_id}/{run_id} is missing or was not declared at start")
            })
    }

    /// The one-shot repair tool is intentionally narrower than the recorder's
    /// normal lifecycle: it may attach missing Depth artifacts only to an
    /// already-completed historical run. A faulted run has no committed
    /// `RecordingOutcome`, so this tool must not manufacture terminal facts.
    #[cfg(any(feature = "depth-lab", test))]
    pub fn ensure_completed_depth_recovery_source(
        &self,
        session_id: &str,
        run_id: &str,
    ) -> anyhow::Result<()> {
        let (session_state, run_state): (String, String) = self
            .connection
            .lock()
            .query_row(
                "SELECT s.state, r.state
                   FROM capture_runs r
                   JOIN sessions s ON s.id = r.session_id
                  WHERE r.session_id = ?1 AND r.run_name = ?2 AND r.depth_expected = 1",
                params![session_id, run_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .with_context(|| {
                format!("completed Depth recovery source {session_id}/{run_id} is missing")
            })?;
        ensure!(
            session_state == "completed" && run_state == "completed",
            "Depth recovery supports only completed historical runs (session={session_state}, run={run_state})"
        );
        Ok(())
    }

    #[cfg(any(all(feature = "depth-lab", feature = "dev-catalog"), test))]
    pub fn commit_cp2077_main_run(&self, commit: &Cp2077MainRunCommit<'_>) -> anyhow::Result<()> {
        ensure!(
            !commit.session_id.trim().is_empty() && !commit.run_id.trim().is_empty(),
            "CP2077 main run identity is empty"
        );
        ensure!(commit.process_id != 0, "CP2077 main run PID is zero");
        ensure!(
            commit.process_launch_id.len() == 32
                && commit
                    .process_launch_id
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
            "CP2077 process launch id is invalid"
        );
        ensure!(
            commit.result_relative_path == "cp2077-main-result.json",
            "CP2077 main result must live at the immutable run root"
        );
        ensure!(
            valid_sha256_reference(commit.result_sha256),
            "CP2077 main result hash is invalid"
        );
        ensure!(
            !commit.sidecar_locator.trim().is_empty() && !commit.delivery_locator.trim().is_empty(),
            "CP2077 local artifact locator is empty"
        );
        ensure!(
            commit.rgb_frames > 0
                && commit.source_depth_frames > 0
                && commit.paired_depth_frames > 0
                && commit.duration_ms > 0
                && commit.raw_input_events > 0
                && commit.keyboard_input_events > 0
                && commit.mouse_input_events > 0
                && commit.player_pose_samples > 0
                && commit.render_camera_samples > 0,
            "CP2077 main run is missing a critical stream"
        );
        ensure!(
            commit.capture_queue_drops == 0 && commit.input_queue_overflows == 0,
            "CP2077 main run reported RGB/input drops"
        );
        ensure!(
            commit.exact_present_pairing_rate.is_finite()
                && (0.98..=1.0).contains(&commit.exact_present_pairing_rate),
            "CP2077 exact-Present pairing rate is below the fail-closed gate"
        );

        let run_journal_id = run_journal_id(commit.session_id, commit.run_id);
        let mut connection = self.connection.lock();
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (state, target_pid, depth_expected, performance_validation_only): (
            String,
            u32,
            bool,
            bool,
        ) = tx
            .query_row(
                "SELECT state, target_pid, depth_expected, performance_validation_only
                   FROM capture_runs
                  WHERE id = ?1 AND session_id = ?2 AND run_name = ?3",
                params![run_journal_id, commit.session_id, commit.run_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .context("CP2077 main collector run is missing from the journal")?;
        ensure!(
            matches!(state.as_str(), "recording" | "stopping")
                && target_pid == commit.process_id
                && depth_expected
                && performance_validation_only,
            "CP2077 main run journal identity/safety ceiling changed before finalization"
        );
        let ready_segments: u32 = tx.query_row(
            "SELECT COUNT(*) FROM segments WHERE run_id = ?1 AND state = 'ready'",
            params![run_journal_id],
            |row| row.get(0),
        )?;
        ensure!(
            ready_segments == 1,
            "CP2077 main run requires exactly one ready RGB segment"
        );
        tx.execute(
            "INSERT INTO cp2077_main_run_facts(
                run_id, session_id, generation, process_id, process_launch_id,
                result_path, result_sha256, sidecar_locator, delivery_locator,
                rgb_frames, source_depth_frames, paired_depth_frames, duration_ms,
                capture_queue_drops, input_queue_overflows, exact_present_matches,
                exact_present_pairing_rate, raw_input_events, keyboard_input_events,
                mouse_input_events, player_pose_samples,
                render_camera_samples, confirmed_flush, non_trainable, upload_allowed,
                created_at_utc
             ) VALUES (
                ?1, ?2, ?3, ?4, ?5,
                ?6, ?7, ?8, ?9,
                ?10, ?11, ?12, ?13,
                ?14, ?15, ?16,
                ?17, ?18, ?19, ?20, ?21,
                ?22, 1, 1, 0,
                strftime('%Y-%m-%dT%H:%M:%fZ','now')
             ) ON CONFLICT(run_id) DO NOTHING",
            params![
                run_journal_id,
                commit.session_id,
                u64_to_i64(commit.generation, "CP2077 depth generation")?,
                i64::from(commit.process_id),
                commit.process_launch_id,
                commit.result_relative_path,
                commit.result_sha256,
                commit.sidecar_locator,
                commit.delivery_locator,
                u64_to_i64(commit.rgb_frames, "CP2077 RGB frames")?,
                u64_to_i64(commit.source_depth_frames, "CP2077 source depth frames")?,
                u64_to_i64(commit.paired_depth_frames, "CP2077 paired depth frames")?,
                u64_to_i64(commit.duration_ms, "CP2077 duration")?,
                u64_to_i64(commit.capture_queue_drops, "CP2077 capture drops")?,
                u64_to_i64(commit.input_queue_overflows, "CP2077 input overflows")?,
                u64_to_i64(commit.exact_present_matches, "CP2077 Present matches")?,
                commit.exact_present_pairing_rate,
                u64_to_i64(commit.raw_input_events, "CP2077 raw input events")?,
                u64_to_i64(commit.keyboard_input_events, "CP2077 keyboard input events")?,
                u64_to_i64(commit.mouse_input_events, "CP2077 mouse input events")?,
                u64_to_i64(commit.player_pose_samples, "CP2077 pose samples")?,
                u64_to_i64(commit.render_camera_samples, "CP2077 camera samples")?,
            ],
        )?;
        type PersistedCp2077MainRun = (
            String,
            u64,
            u32,
            String,
            String,
            String,
            u64,
            u64,
            u64,
            u64,
            u64,
            u64,
            u64,
            f64,
            u64,
            u64,
            u64,
            u64,
            u64,
            bool,
            bool,
            bool,
        );
        let persisted: PersistedCp2077MainRun = tx.query_row(
            "SELECT session_id, generation, process_id, process_launch_id,
                    result_path, result_sha256, rgb_frames, source_depth_frames,
                    paired_depth_frames, duration_ms, capture_queue_drops,
                    input_queue_overflows, exact_present_matches,
                    exact_present_pairing_rate, raw_input_events, keyboard_input_events,
                    mouse_input_events, player_pose_samples, render_camera_samples,
                    confirmed_flush, non_trainable, upload_allowed
               FROM cp2077_main_run_facts WHERE run_id = ?1",
            params![run_journal_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                    row.get(9)?,
                    row.get(10)?,
                    row.get(11)?,
                    row.get(12)?,
                    row.get(13)?,
                    row.get(14)?,
                    row.get(15)?,
                    row.get(16)?,
                    row.get(17)?,
                    row.get(18)?,
                    row.get(19)?,
                    row.get(20)?,
                    row.get(21)?,
                ))
            },
        )?;
        ensure!(
            persisted.0 == commit.session_id
                && persisted.1 == commit.generation
                && persisted.2 == commit.process_id
                && persisted.3 == commit.process_launch_id
                && persisted.4 == commit.result_relative_path
                && persisted.5 == commit.result_sha256
                && persisted.6 == commit.rgb_frames
                && persisted.7 == commit.source_depth_frames
                && persisted.8 == commit.paired_depth_frames
                && persisted.9 == commit.duration_ms
                && persisted.10 == commit.capture_queue_drops
                && persisted.11 == commit.input_queue_overflows
                && persisted.12 == commit.exact_present_matches
                && persisted.13 == commit.exact_present_pairing_rate
                && persisted.14 == commit.raw_input_events
                && persisted.15 == commit.keyboard_input_events
                && persisted.16 == commit.mouse_input_events
                && persisted.17 == commit.player_pose_samples
                && persisted.18 == commit.render_camera_samples
                && persisted.19
                && persisted.20
                && !persisted.21,
            "CP2077 main run fact replay changed immutable evidence"
        );
        tx.commit()?;
        Ok(())
    }

    /// 将 durable manifest 与真实 ZIP 的两组 path/hash 同事务提交。
    ///
    /// 文件系统 staging/rename 必须先成功；本函数不接收半成品。既有 production
    /// `upload_jobs` 在领取时从 segment 行读取 ZIP，因此重启与先入队后物化都复用同一
    /// durable source。整批失败会回滚，任何段都不会被当成普通 no-depth。
    #[cfg(any(feature = "depth-lab", test))]
    pub fn commit_depth_segments(
        &self,
        session_id: &str,
        run_id: &str,
        commits: &[DepthSegmentCommit],
    ) -> anyhow::Result<()> {
        ensure!(
            !session_id.trim().is_empty() && !run_id.trim().is_empty(),
            "depth segment identity must not be empty"
        );
        ensure!(!commits.is_empty(), "depth segment commit set is empty");
        let mut seen = std::collections::BTreeSet::new();
        for commit in commits {
            ensure!(
                seen.insert(commit.segment_index),
                "duplicate depth segment commit {}",
                commit.segment_index
            );
            ensure!(
                valid_sha256_reference(&commit.manifest_sha256),
                "depth segment manifest hash must be sha256:<64 lowercase hex>"
            );
            ensure!(
                commit.candidate_eligible == (commit.invalid_frame_count == 0),
                "depth candidate eligibility must match its per-frame invalid count"
            );
            let path = Path::new(&commit.manifest_relative_path);
            ensure!(
                path.is_relative()
                    && path
                        .components()
                        .all(|component| matches!(component, Component::Normal(_))),
                "depth segment manifest path contains unsafe components"
            );
            let expected = Path::new("depth-segments")
                .join(format!("{:05}", commit.segment_index))
                .join("depth-segment.json");
            ensure!(
                path == expected,
                "depth segment manifest path is outside its immutable segment directory"
            );
            match (
                commit.upload_relative_path.as_deref(),
                commit.upload_sha256.as_deref(),
            ) {
                (Some(upload_path), Some(upload_sha256)) => {
                    ensure!(
                        commit.candidate_eligible,
                        "non-deliverable depth segment must not name an upload ZIP"
                    );
                    ensure!(
                        valid_sha256_reference(upload_sha256),
                        "depth upload ZIP hash must be sha256:<64 lowercase hex>"
                    );
                    let upload_path = Path::new(upload_path);
                    ensure!(
                        upload_path.is_relative()
                            && upload_path
                                .components()
                                .all(|component| matches!(component, Component::Normal(_))),
                        "depth upload ZIP path contains unsafe components"
                    );
                    let expected_upload = Path::new("runs")
                        .join(run_id)
                        .join("depth-uploads")
                        .join(format!("depth_{:05}.zip", commit.segment_index));
                    ensure!(
                        upload_path == expected_upload,
                        "depth upload ZIP path is outside its immutable run directory"
                    );
                }
                (None, None) => ensure!(
                    !commit.candidate_eligible,
                    "eligible depth segment is missing its upload ZIP"
                ),
                _ => anyhow::bail!("depth upload ZIP has one-sided path/hash metadata"),
            }
        }

        let mut connection = self.connection.lock();
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let expected_run_journal_id = run_journal_id(session_id, run_id);
        for commit in commits {
            let segment_id = segment_journal_id(session_id, run_id, commit.segment_index);
            let (
                state,
                persisted_run_id,
                persisted_seq,
                depth_path,
                depth_sha256,
                depth_upload_path,
                depth_upload_sha256,
            ) = tx
                .query_row(
                    "SELECT state, run_id, seq, depth_path, depth_sha256,
                            depth_upload_path, depth_upload_sha256
                       FROM segments WHERE id = ?1 AND session_id = ?2",
                    params![segment_id, session_id],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, u32>(2)?,
                            row.get::<_, Option<String>>(3)?,
                            row.get::<_, Option<String>>(4)?,
                            row.get::<_, Option<String>>(5)?,
                            row.get::<_, Option<String>>(6)?,
                        ))
                    },
                )
                .with_context(|| format!("depth segment {} is missing", commit.segment_index))?;
            ensure!(
                state == "ready",
                "depth may attach only to a ready RGB segment"
            );
            ensure!(
                persisted_run_id == expected_run_journal_id
                    && persisted_seq == commit.segment_index,
                "depth segment journal identity mismatch"
            );
            match (
                depth_path.as_deref(),
                depth_sha256.as_deref(),
                depth_upload_path.as_deref(),
                depth_upload_sha256.as_deref(),
            ) {
                (None, None, None, None) => {
                    let changed = tx.execute(
                        "UPDATE segments
                            SET depth_path = ?2, depth_sha256 = ?3,
                                depth_upload_path = ?4, depth_upload_sha256 = ?5,
                                updated_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now')
                          WHERE id = ?1
                            AND depth_path IS NULL AND depth_sha256 IS NULL
                            AND depth_upload_path IS NULL AND depth_upload_sha256 IS NULL",
                        params![
                            segment_id,
                            commit.manifest_relative_path,
                            commit.manifest_sha256,
                            commit.upload_relative_path,
                            commit.upload_sha256,
                        ],
                    )?;
                    ensure!(changed == 1, "depth segment completion lost its empty row");
                }
                (Some(path), Some(sha256), None, None) => {
                    ensure!(
                        path == commit.manifest_relative_path && sha256 == commit.manifest_sha256,
                        "depth segment manifest completion is immutable"
                    );
                    if commit.candidate_eligible {
                        let changed = tx.execute(
                            "UPDATE segments
                                SET depth_upload_path = ?2, depth_upload_sha256 = ?3,
                                    updated_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now')
                              WHERE id = ?1
                                AND depth_upload_path IS NULL AND depth_upload_sha256 IS NULL",
                            params![
                                segment_id,
                                commit.upload_relative_path,
                                commit.upload_sha256,
                            ],
                        )?;
                        ensure!(changed == 1, "depth ZIP completion lost its empty row");
                    }
                }
                (Some(path), Some(sha256), Some(upload_path), Some(upload_sha256)) => ensure!(
                    path == commit.manifest_relative_path
                        && sha256 == commit.manifest_sha256
                        && Some(upload_path) == commit.upload_relative_path.as_deref()
                        && Some(upload_sha256) == commit.upload_sha256.as_deref(),
                    "depth segment completion is immutable"
                ),
                _ => anyhow::bail!(
                    "depth segment has incomplete manifest/ZIP metadata; refusing to enqueue"
                ),
            }
            tx.execute(
                "INSERT INTO depth_lab_segment_facts(
                    segment_id, session_id, run_id, generation, segment_seq,
                    manifest_path, manifest_sha256, candidate_eligible,
                    invalid_frame_count, created_at_utc
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9,
                           strftime('%Y-%m-%dT%H:%M:%fZ','now'))
                 ON CONFLICT(segment_id) DO NOTHING",
                params![
                    segment_id,
                    session_id,
                    expected_run_journal_id,
                    u64_to_i64(commit.generation, "depth generation")?,
                    i64::from(commit.segment_index),
                    commit.manifest_relative_path,
                    commit.manifest_sha256,
                    i64::from(commit.candidate_eligible),
                    i64::from(commit.invalid_frame_count),
                ],
            )?;
            let persisted_fact = tx.query_row(
                "SELECT s.id, f.session_id, r.run_name, f.generation, f.segment_seq,
                        f.manifest_path, f.manifest_sha256, f.candidate_eligible,
                        f.invalid_frame_count
                   FROM depth_lab_segment_facts f
                   JOIN segments s ON s.id = f.segment_id
                   JOIN capture_runs r ON r.id = f.run_id
                  WHERE f.segment_id = ?1",
                params![segment_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, u64>(3)?,
                        row.get::<_, u32>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, bool>(7)?,
                        row.get::<_, u32>(8)?,
                    ))
                },
            )?;
            ensure!(
                persisted_fact.0 == segment_id
                    && persisted_fact.1 == session_id
                    && persisted_fact.2 == run_id
                    && persisted_fact.3 == commit.generation
                    && persisted_fact.4 == commit.segment_index
                    && persisted_fact.5 == commit.manifest_relative_path
                    && persisted_fact.6 == commit.manifest_sha256
                    && persisted_fact.7 == commit.candidate_eligible
                    && persisted_fact.8 == commit.invalid_frame_count,
                "depth segment fact replay changed persisted identity or validity"
            );
            if !commit.candidate_eligible {
                tx.execute(
                    "UPDATE upload_jobs
                        SET state = 'rejected',
                            last_error_code = 'DEPTH_SEGMENT_INELIGIBLE',
                            next_attempt_at_ms = 0,
                            updated_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now')
                      WHERE segment_id = ?1
                        AND state IN ('pending', 'retry_wait')",
                    params![segment_id],
                )?;
                let queued: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM depth_lab_jobs WHERE segment_id = ?1)",
                    params![segment_id],
                    |row| row.get(0),
                )?;
                ensure!(
                    !queued,
                    "non-deliverable depth segment must never become a queue candidate"
                );
                continue;
            }
            tx.execute(
                "INSERT INTO depth_lab_jobs(
                    segment_id, session_id, run_id, generation, segment_seq, state,
                    attempt_count, manifest_path, manifest_sha256, created_at_utc, updated_at_utc
                 ) VALUES (?1, ?2, ?3, ?4, ?5, 'pending', 0, ?6, ?7,
                           strftime('%Y-%m-%dT%H:%M:%fZ','now'),
                           strftime('%Y-%m-%dT%H:%M:%fZ','now'))
                 ON CONFLICT(segment_id) DO NOTHING",
                params![
                    segment_id,
                    session_id,
                    expected_run_journal_id,
                    u64_to_i64(commit.generation, "depth generation")?,
                    i64::from(commit.segment_index),
                    commit.manifest_relative_path,
                    commit.manifest_sha256,
                ],
            )?;
            let candidate = load_depth_lab_candidate(&tx, &segment_id)?
                .context("depth-lab queue row disappeared before commit")?;
            ensure!(
                candidate.session_id == session_id
                    && candidate.run_id == run_id
                    && candidate.generation == commit.generation
                    && candidate.segment_index == commit.segment_index
                    && candidate.manifest_relative_path == commit.manifest_relative_path
                    && candidate.manifest_sha256 == commit.manifest_sha256,
                "depth-lab queue replay changed persisted identity"
            );
            tx.execute(
                "UPDATE upload_jobs
                    SET state = 'pending', last_error_code = NULL,
                        next_attempt_at_ms = 0,
                        updated_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now')
                  WHERE segment_id = ?1
                    AND state = 'rejected'
                    AND last_error_code = 'DEPTH_FINALIZE_FAILED'",
                params![segment_id],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// A Depth-enabled run must not masquerade as an R2 backlog when its local
    /// manifest/ZIP materialization failed. Recovery may reopen these exact
    /// rows after the immutable segment facts have been committed successfully.
    #[cfg(any(feature = "depth-lab", test))]
    pub fn reject_depth_finalization(&self, session_id: &str, run_id: &str) -> anyhow::Result<()> {
        ensure!(
            !session_id.trim().is_empty() && !run_id.trim().is_empty(),
            "depth finalization identity must not be empty"
        );
        let expected_run_journal_id = run_journal_id(session_id, run_id);
        self.connection.lock().execute(
            "UPDATE upload_jobs
                SET state = 'rejected',
                    last_error_code = 'DEPTH_FINALIZE_FAILED',
                    next_attempt_at_ms = 0,
                    updated_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now')
              WHERE session_id = ?1 AND run_id = ?2
                AND state IN ('pending', 'retry_wait')",
            params![session_id, expected_run_journal_id],
        )?;
        Ok(())
    }

    #[allow(dead_code)] // 专用 uploader 尚未接线；T-A 先落 durable consumer boundary。
    pub fn depth_lab_candidate(
        &self,
        segment_id: &str,
    ) -> anyhow::Result<Option<DepthLabCandidate>> {
        load_depth_lab_candidate(&self.connection.lock(), segment_id)
    }

    /// 只推进独立 depth-lab 队列；不读取 production upload gate/credentials。
    #[allow(dead_code)] // 专用 uploader 尚未接线；T-A 先落 durable consumer boundary。
    pub fn claim_next_depth_lab_candidate(&self) -> anyhow::Result<Option<DepthLabCandidate>> {
        let mut connection = self.connection.lock();
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let segment_id = tx
            .query_row(
                "SELECT segment_id FROM depth_lab_jobs
                  WHERE state = 'pending'
                  ORDER BY created_at_utc, segment_id LIMIT 1",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        let Some(segment_id) = segment_id else {
            tx.commit()?;
            return Ok(None);
        };
        let changed = tx.execute(
            "UPDATE depth_lab_jobs
                SET state = 'running', attempt_count = attempt_count + 1,
                    updated_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now')
              WHERE segment_id = ?1 AND state = 'pending'",
            params![segment_id],
        )?;
        ensure!(
            changed == 1,
            "depth-lab candidate claim lost its pending row"
        );
        let candidate = load_depth_lab_candidate(&tx, &segment_id)?
            .context("claimed depth-lab candidate disappeared")?;
        tx.commit()?;
        Ok(Some(candidate))
    }

    #[allow(dead_code)] // 专用 uploader 尚未接线；T-A 先落 durable recovery boundary。
    pub fn requeue_interrupted_depth_lab_jobs(&self) -> anyhow::Result<()> {
        self.connection.lock().execute(
            "UPDATE depth_lab_jobs
                SET state = 'pending',
                    updated_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now')
              WHERE state = 'running' AND archive_job_id IS NULL",
            [],
        )?;
        Ok(())
    }

    /// 原子领取一个 run 级 depth archive job。已有 job 优先按过期 lease 恢复；
    /// 没有时才把同 identity 的 #529 segment candidates 一次性绑定成新 job。
    #[cfg(any(feature = "depth-lab", test))]
    pub fn claim_next_depth_archive_job(
        &self,
        now_ms: u64,
        lease_owner: &str,
        lease_duration_ms: u64,
    ) -> anyhow::Result<Option<DepthArchiveLease>> {
        ensure_depth_lease_owner(lease_owner)?;
        ensure!(
            lease_duration_ms > 0,
            "depth archive lease duration is zero"
        );
        let now = u64_to_i64(now_ms, "depth archive current time")?;
        let expires = u64_to_i64(
            now_ms
                .checked_add(lease_duration_ms)
                .context("depth archive lease expiry overflow")?,
            "depth archive lease expiry",
        )?;
        let mut connection = self.connection.lock();
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut job_id = tx
            .query_row(
                "SELECT job_id FROM depth_lab_archive_jobs
                  WHERE state <> 'finalized'
                    AND next_attempt_at_ms <= ?1
                    AND (lease_owner IS NULL OR lease_expires_at_ms <= ?1)
                  ORDER BY created_at_utc, job_id LIMIT 1",
                params![now],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if job_id.is_none() {
            let seed = tx
                .query_row(
                    "SELECT d.session_id, d.run_id, r.run_name, d.generation
                       FROM depth_lab_jobs d
                       JOIN capture_runs r ON r.id = d.run_id
                      WHERE d.state = 'pending' AND d.archive_job_id IS NULL
                      ORDER BY d.created_at_utc, d.segment_id LIMIT 1",
                    [],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, u64>(3)?,
                        ))
                    },
                )
                .optional()?;
            if let Some((session_id, run_journal_id, run_name, generation)) = seed {
                let created_job_id = format!("depth-archive:{session_id}:{run_name}:g{generation}");
                tx.execute(
                    "INSERT INTO depth_lab_archive_jobs(
                        job_id, session_id, run_id, generation, state,
                        created_at_utc, updated_at_utc
                     ) VALUES (?1, ?2, ?3, ?4, 'pending',
                               strftime('%Y-%m-%dT%H:%M:%fZ','now'),
                               strftime('%Y-%m-%dT%H:%M:%fZ','now'))
                     ON CONFLICT(session_id, run_id, generation) DO NOTHING",
                    params![
                        created_job_id,
                        session_id,
                        run_journal_id,
                        u64_to_i64(generation, "depth archive generation")?,
                    ],
                )?;
                let persisted_job_id: String = tx.query_row(
                    "SELECT job_id FROM depth_lab_archive_jobs
                      WHERE session_id = ?1 AND run_id = ?2 AND generation = ?3",
                    params![
                        session_id,
                        run_journal_id,
                        u64_to_i64(generation, "depth archive generation")?,
                    ],
                    |row| row.get(0),
                )?;
                let bound = tx.execute(
                    "UPDATE depth_lab_jobs
                        SET state = 'running', archive_job_id = ?4,
                            updated_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now')
                      WHERE session_id = ?1 AND run_id = ?2 AND generation = ?3
                        AND state = 'pending' AND archive_job_id IS NULL",
                    params![
                        session_id,
                        run_journal_id,
                        u64_to_i64(generation, "depth archive generation")?,
                        persisted_job_id,
                    ],
                )?;
                ensure!(bound > 0, "depth archive job bound no segment candidates");
                job_id = Some(persisted_job_id);
            }
        }
        let Some(job_id) = job_id else {
            tx.commit()?;
            return Ok(None);
        };
        let changed = tx.execute(
            "UPDATE depth_lab_archive_jobs
                SET lease_owner = ?2, lease_expires_at_ms = ?3,
                    attempt_count = attempt_count + 1,
                    state = CASE WHEN state = 'retry_wait' THEN 'pending' ELSE state END,
                    last_error_code = NULL, last_error_message = NULL,
                    updated_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now')
              WHERE job_id = ?1 AND state <> 'finalized'
                AND next_attempt_at_ms <= ?4
                AND (lease_owner IS NULL OR lease_expires_at_ms <= ?4)",
            params![job_id, lease_owner, expires, now],
        )?;
        ensure!(changed == 1, "depth archive lease was claimed concurrently");
        let job = load_depth_archive_job(&tx, &job_id)?
            .context("claimed depth archive job disappeared")?;
        let candidates = load_depth_archive_candidates(&tx, &job_id)?;
        ensure!(
            !candidates.is_empty(),
            "depth archive job has no durable segment candidates"
        );
        ensure!(
            candidates.iter().all(|candidate| {
                candidate.session_id == job.session_id
                    && candidate.run_id == job.run_id
                    && candidate.generation == job.generation
                    && candidate.state == DepthLabJobState::Running
            }),
            "depth archive candidates do not match the leased identity"
        );
        tx.commit()?;
        Ok(Some(DepthArchiveLease { job, candidates }))
    }

    #[cfg(feature = "depth-lab")]
    pub fn renew_depth_archive_lease(
        &self,
        job_id: &str,
        lease_owner: &str,
        now_ms: u64,
        lease_duration_ms: u64,
    ) -> anyhow::Result<()> {
        ensure_depth_lease_owner(lease_owner)?;
        ensure!(
            lease_duration_ms > 0,
            "depth archive lease duration is zero"
        );
        let now = u64_to_i64(now_ms, "depth archive current time")?;
        let expires = u64_to_i64(
            now_ms
                .checked_add(lease_duration_ms)
                .context("depth archive lease expiry overflow")?,
            "depth archive lease expiry",
        )?;
        let changed = self.connection.lock().execute(
            "UPDATE depth_lab_archive_jobs
                SET lease_expires_at_ms = ?4,
                    updated_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now')
              WHERE job_id = ?1 AND lease_owner = ?2
                AND lease_expires_at_ms > ?3 AND state <> 'finalized'",
            params![job_id, lease_owner, now, expires],
        )?;
        ensure!(changed == 1, "depth archive lease is missing or expired");
        Ok(())
    }

    #[cfg(any(feature = "depth-lab", test))]
    pub fn save_depth_archive_materialization(
        &self,
        job_id: &str,
        lease_owner: &str,
        now_ms: u64,
        materialized_json: &str,
        progress_json: &str,
    ) -> anyhow::Result<()> {
        validate_depth_json("materialized archive", materialized_json)?;
        validate_depth_json("archive progress", progress_json)?;
        update_depth_archive_job_json(
            &self.connection,
            &DepthArchiveJsonUpdate {
                job_id,
                lease_owner,
                now_ms,
                state: DepthArchiveJobState::Materialized,
                field: "materialized_json",
                value: materialized_json,
                extra: Some(("progress_json", progress_json)),
            },
        )
    }

    #[cfg(any(feature = "depth-lab", test))]
    pub fn save_depth_archive_remote_scope(
        &self,
        job_id: &str,
        lease_owner: &str,
        now_ms: u64,
        remote_scope_json: &str,
    ) -> anyhow::Result<()> {
        validate_depth_json("depth remote scope", remote_scope_json)?;
        update_depth_archive_job_json(
            &self.connection,
            &DepthArchiveJsonUpdate {
                job_id,
                lease_owner,
                now_ms,
                state: DepthArchiveJobState::Registered,
                field: "remote_scope_json",
                value: remote_scope_json,
                extra: None,
            },
        )
    }

    #[cfg(any(feature = "depth-lab", test))]
    pub fn save_depth_archive_progress(
        &self,
        job_id: &str,
        lease_owner: &str,
        now_ms: u64,
        state: DepthArchiveJobState,
        progress_json: &str,
    ) -> anyhow::Result<()> {
        ensure!(
            matches!(
                state,
                DepthArchiveJobState::Uploading | DepthArchiveJobState::ManifestAuthorized
            ),
            "invalid depth archive progress state"
        );
        validate_depth_json("depth archive progress", progress_json)?;
        update_depth_archive_job_json(
            &self.connection,
            &DepthArchiveJsonUpdate {
                job_id,
                lease_owner,
                now_ms,
                state,
                field: "progress_json",
                value: progress_json,
                extra: None,
            },
        )
    }

    #[cfg(any(feature = "depth-lab", test))]
    pub fn retry_depth_archive_job(
        &self,
        job_id: &str,
        lease_owner: &str,
        now_ms: u64,
        delay_ms: u64,
        error_code: &str,
        error_message: &str,
    ) -> anyhow::Result<()> {
        ensure!(
            !error_code.trim().is_empty(),
            "depth retry error code is empty"
        );
        ensure!(
            error_message.len() <= 2048,
            "depth retry error message is too long"
        );
        let next = now_ms
            .checked_add(delay_ms)
            .context("depth retry timestamp overflow")?;
        let changed = self.connection.lock().execute(
            "UPDATE depth_lab_archive_jobs
                SET state = 'retry_wait', next_attempt_at_ms = ?4,
                    lease_owner = NULL, lease_expires_at_ms = NULL,
                    last_error_code = ?5, last_error_message = ?6,
                    updated_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now')
              WHERE job_id = ?1 AND lease_owner = ?2
                AND lease_expires_at_ms > ?3 AND state <> 'finalized'",
            params![
                job_id,
                lease_owner,
                u64_to_i64(now_ms, "depth retry current time")?,
                u64_to_i64(next, "depth retry next attempt")?,
                error_code,
                error_message,
            ],
        )?;
        ensure!(changed == 1, "depth archive retry lost its live lease");
        Ok(())
    }

    #[cfg(any(feature = "depth-lab", test))]
    pub fn complete_depth_archive_job(
        &self,
        job_id: &str,
        lease_owner: &str,
        now_ms: u64,
        remote_phase: &str,
    ) -> anyhow::Result<()> {
        ensure!(
            remote_phase == "finalized",
            "depth archive queue may complete only after remote Finalized"
        );
        let mut connection = self.connection.lock();
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "UPDATE depth_lab_archive_jobs
                SET state = 'finalized', lease_owner = NULL, lease_expires_at_ms = NULL,
                    last_error_code = NULL, last_error_message = NULL,
                    updated_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now')
              WHERE job_id = ?1 AND lease_owner = ?2
                AND lease_expires_at_ms > ?3
                AND materialized_json IS NOT NULL
                AND remote_scope_json IS NOT NULL
                AND progress_json IS NOT NULL
                AND state IN ('uploading', 'manifest_authorized')",
            params![
                job_id,
                lease_owner,
                u64_to_i64(now_ms, "depth completion current time")?,
            ],
        )?;
        ensure!(changed == 1, "depth archive completion lost state or lease");
        let removed = tx.execute(
            "DELETE FROM depth_lab_jobs WHERE archive_job_id = ?1",
            params![job_id],
        )?;
        ensure!(
            removed > 0,
            "finalized depth archive removed no queue candidates"
        );
        tx.commit()?;
        Ok(())
    }

    #[cfg(any(feature = "depth-lab", test))]
    pub fn depth_archive_job(&self, job_id: &str) -> anyhow::Result<Option<DepthArchiveJob>> {
        load_depth_archive_job(&self.connection.lock(), job_id)
    }

    pub fn set_remote_session_id(&self, session_id: &str, remote_id: &str) -> anyhow::Result<()> {
        set_once(
            &self.connection,
            "upload_sessions",
            "session_id",
            session_id,
            "remote_session_id",
            remote_id,
        )
    }

    pub fn set_remote_run_id(&self, run_id: &str, remote_id: &str) -> anyhow::Result<()> {
        set_once(
            &self.connection,
            "upload_runs",
            "run_id",
            run_id,
            "remote_run_id",
            remote_id,
        )
    }

    pub fn set_remote_clip(
        &self,
        segment_id: &str,
        remote_id: &str,
        revision: u32,
    ) -> anyhow::Result<()> {
        let connection = self.connection.lock();
        let changed = connection.execute(
            "UPDATE upload_jobs
                SET remote_clip_id = COALESCE(remote_clip_id, ?2),
                    clip_revision = COALESCE(clip_revision, ?3),
                    updated_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now')
              WHERE segment_id = ?1
                AND (remote_clip_id IS NULL OR remote_clip_id = ?2)
                AND (clip_revision IS NULL OR clip_revision = ?3)",
            params![segment_id, remote_id, i64::from(revision)],
        )?;
        ensure!(
            changed == 1,
            "remote clip identity conflicts with the durable upload job"
        );
        Ok(())
    }

    pub fn set_remote_upload_intent(
        &self,
        segment_id: &str,
        remote_id: &str,
    ) -> anyhow::Result<()> {
        set_once(
            &self.connection,
            "upload_jobs",
            "segment_id",
            segment_id,
            "remote_upload_intent_id",
            remote_id,
        )
    }

    pub fn uploaded_parts(&self, segment_id: &str) -> anyhow::Result<Vec<UploadedPart>> {
        let connection = self.connection.lock();
        let mut statement = connection.prepare(
            "SELECT artifact_kind, provider_attempt_no, part_number, byte_offset, byte_size, content_md5, etag
               FROM upload_parts WHERE segment_id = ?1
              ORDER BY artifact_kind, part_number",
        )?;
        statement
            .query_map(params![segment_id], |row| {
                Ok(UploadedPart {
                    artifact_kind: row.get(0)?,
                    provider_attempt_no: row.get(1)?,
                    part_number: row.get(2)?,
                    byte_offset: row.get(3)?,
                    byte_size: row.get(4)?,
                    content_md5: row.get(5)?,
                    etag: row.get(6)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub fn save_uploaded_part(&self, segment_id: &str, part: &UploadedPart) -> anyhow::Result<()> {
        let connection = self.connection.lock();
        connection.execute(
            "INSERT INTO upload_parts(
                segment_id, artifact_kind, provider_attempt_no, part_number, byte_offset, byte_size,
                content_md5, etag, uploaded_at_utc
             ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,strftime('%Y-%m-%dT%H:%M:%fZ','now'))
             ON CONFLICT(segment_id, artifact_kind, part_number) DO UPDATE SET
                provider_attempt_no=excluded.provider_attempt_no,
                byte_offset=excluded.byte_offset, byte_size=excluded.byte_size,
                content_md5=excluded.content_md5, etag=excluded.etag,
                uploaded_at_utc=excluded.uploaded_at_utc",
            params![
                segment_id,
                part.artifact_kind,
                i64::from(part.provider_attempt_no),
                i64::from(part.part_number),
                u64_to_i64(part.byte_offset, "upload part offset")?,
                u64_to_i64(part.byte_size, "upload part size")?,
                part.content_md5,
                part.etag,
            ],
        )?;
        Ok(())
    }

    pub fn retry_upload_job(
        &self,
        segment_id: &str,
        delay_ms: u64,
        failure: UploadFailureDetails<'_>,
    ) -> anyhow::Result<()> {
        ensure_upload_error_code(failure.error_code)?;
        let connection = self.connection.lock();
        let changed = connection.execute(
            "UPDATE upload_jobs
                SET state = 'retry_wait', last_error_code = ?2,
                    next_attempt_at_ms = CAST(unixepoch('subsec') * 1000 AS INTEGER) + ?3,
                    http_status = ?4, server_code = ?5,
                    server_message = ?6, request_id = ?7,
                    updated_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now')
              WHERE segment_id = ?1 AND state = 'running'",
            params![
                segment_id,
                failure.error_code,
                u64_to_i64(delay_ms, "upload retry delay")?,
                failure.http_status.map(i64::from),
                failure.server_code,
                failure.server_message,
                failure.request_id,
            ],
        )?;
        ensure!(changed == 1, "upload retry lost its running job");
        Ok(())
    }

    pub fn reject_running_upload_job(
        &self,
        segment_id: &str,
        error_code: &str,
        http_status: Option<u16>,
        server_code: Option<&str>,
        server_message: Option<&str>,
        request_id: Option<&str>,
    ) -> anyhow::Result<()> {
        ensure_upload_error_code(error_code)?;
        let connection = self.connection.lock();
        let changed = connection.execute(
            "UPDATE upload_jobs
                SET state = 'rejected', last_error_code = ?2,
                    http_status = ?3, server_code = ?4,
                    server_message = ?5, request_id = ?6,
                    updated_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now')
              WHERE segment_id = ?1 AND state = 'running'",
            params![
                segment_id,
                error_code,
                http_status.map(i64::from),
                server_code,
                server_message,
                request_id,
            ],
        )?;
        ensure!(changed == 1, "upload rejection lost its running job");
        Ok(())
    }

    pub fn mark_upload_stored(&self, segment_id: &str) -> anyhow::Result<()> {
        update_upload_job_state(
            &self.connection,
            segment_id,
            UploadJobState::Stored,
            30_000,
            None,
        )
    }

    pub fn next_stored_upload(&self) -> anyhow::Result<Option<UploadJob>> {
        let connection = self.connection.lock();
        connection
            .query_row(
                "SELECT j.segment_id, j.state, j.attempt_count, j.remote_clip_id,
                        j.session_id, g.run_name, g.seq, j.status_attempt_count
                   FROM upload_jobs j
                   JOIN upload_sessions us ON us.session_id = j.session_id
                   JOIN segments g ON g.id = j.segment_id
                  WHERE j.state = 'stored'
                    AND us.upload_requested = 1
                    AND j.next_attempt_at_ms <= unixepoch('subsec') * 1000
                  ORDER BY j.next_attempt_at_ms, j.segment_id LIMIT 1",
                [],
                upload_job_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn reschedule_upload_status(&self, segment_id: &str, delay_ms: u64) -> anyhow::Result<()> {
        update_upload_job_state(
            &self.connection,
            segment_id,
            UploadJobState::Stored,
            delay_ms,
            None,
        )?;
        self.connection.lock().execute(
            "UPDATE upload_jobs SET status_attempt_count = 0 WHERE segment_id = ?1",
            params![segment_id],
        )?;
        Ok(())
    }

    pub fn retry_upload_status(
        &self,
        segment_id: &str,
        error_code: &str,
        delay_ms: u64,
    ) -> anyhow::Result<()> {
        ensure_upload_error_code(error_code)?;
        let connection = self.connection.lock();
        let changed = connection.execute(
            "UPDATE upload_jobs
                SET status_attempt_count = status_attempt_count + 1,
                    last_error_code = ?2,
                    next_attempt_at_ms = CAST(unixepoch('subsec') * 1000 AS INTEGER) + ?3,
                    updated_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now')
              WHERE segment_id = ?1 AND state = 'stored'",
            params![
                segment_id,
                error_code,
                u64_to_i64(delay_ms, "upload status retry delay")?,
            ],
        )?;
        ensure!(changed == 1, "upload status retry lost its stored job");
        Ok(())
    }

    pub fn finish_upload_status(
        &self,
        segment_id: &str,
        verified: bool,
        error_code: Option<&str>,
    ) -> anyhow::Result<()> {
        if let Some(code) = error_code {
            ensure_upload_error_code(code)?;
        }
        update_upload_job_state(
            &self.connection,
            segment_id,
            if verified {
                UploadJobState::Verified
            } else {
                UploadJobState::Rejected
            },
            0,
            error_code,
        )
    }

    pub fn finish_upload_status_with_retention(
        &self,
        segment_id: &str,
        verified: bool,
        error_code: Option<&str>,
        remote_retention_state: Option<&str>,
    ) -> anyhow::Result<()> {
        if let Some(code) = error_code {
            ensure_upload_error_code(code)?;
        }
        if let Some(state) = remote_retention_state {
            ensure!(
                matches!(
                    state,
                    "retained" | "purge_requested" | "purging" | "purged" | "purge_failed"
                ),
                "unsupported remote retention state"
            );
        }
        let connection = self.connection.lock();
        let current = load_upload_job(&connection, segment_id)?.context("upload job is missing")?;
        ensure!(
            current.state == UploadJobState::Stored,
            "upload status terminal transition requires a stored job"
        );
        let changed = connection.execute(
            "UPDATE upload_jobs
                SET state = ?2, last_error_code = ?3, next_attempt_at_ms = 0,
                    remote_retention_state = ?4, status_attempt_count = 0,
                    updated_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now')
              WHERE segment_id = ?1 AND state = 'stored'",
            params![
                segment_id,
                if verified { "verified" } else { "rejected" },
                error_code,
                remote_retention_state,
            ],
        )?;
        ensure!(
            changed == 1,
            "upload status terminal transition lost its stored job"
        );
        Ok(())
    }

    pub fn claim_upload_session_seal(&self) -> anyhow::Result<Option<SessionSealJob>> {
        let mut connection = self.connection.lock();
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let candidate: Option<(String, String, String, String, Option<String>)> = tx
            .query_row(
                "SELECT s.id, us.remote_session_id, s.ended_at_utc, s.state, s.stop_reason
                   FROM sessions s
                   JOIN upload_sessions us ON us.session_id = s.id
                  WHERE s.state = 'completed'
                    AND us.upload_requested = 1
                    AND us.sealed = 0
                    AND us.seal_state IN ('pending', 'retry_wait')
                    AND us.seal_next_attempt_at_ms <= unixepoch('subsec') * 1000
                    AND us.remote_session_id IS NOT NULL
                    AND s.ended_at_utc IS NOT NULL
                    AND (SELECT COUNT(*) FROM upload_jobs j WHERE j.session_id = s.id) > 0
                    AND NOT EXISTS (
                        SELECT 1 FROM upload_jobs j
                         WHERE j.session_id = s.id AND j.state NOT IN ('stored', 'verified')
                    )
                    AND (SELECT COUNT(*) FROM upload_jobs j WHERE j.session_id = s.id) =
                        (SELECT COUNT(*) FROM segments g
                          WHERE g.session_id = s.id AND g.state = 'ready')
                  ORDER BY us.seal_next_attempt_at_ms, s.id
                  LIMIT 1",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .optional()?;
        let Some((
            local_session_id,
            remote_session_id,
            ended_at_utc,
            local_final_state,
            stop_reason,
        )) = candidate
        else {
            tx.commit()?;
            return Ok(None);
        };
        let changed = tx.execute(
            "UPDATE upload_sessions
                SET seal_state = 'running', seal_attempt_count = seal_attempt_count + 1,
                    seal_last_error_code = NULL, seal_http_status = NULL,
                    seal_server_code = NULL, seal_server_message = NULL,
                    seal_request_id = NULL,
                    updated_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now')
              WHERE session_id = ?1 AND seal_state IN ('pending', 'retry_wait')",
            params![local_session_id],
        )?;
        ensure!(changed == 1, "session seal claim lost its processable row");
        let attempt_count: u32 = tx.query_row(
            "SELECT seal_attempt_count FROM upload_sessions WHERE session_id = ?1",
            params![local_session_id],
            |row| row.get(0),
        )?;
        tx.commit()?;
        Ok(Some(SessionSealJob {
            local_session_id,
            remote_session_id,
            ended_at_utc,
            local_final_state,
            stop_reason,
            attempt_count,
        }))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn retry_upload_session_seal(
        &self,
        session_id: &str,
        error_code: &str,
        delay_ms: u64,
        http_status: Option<u16>,
        server_code: Option<&str>,
        server_message: Option<&str>,
        request_id: Option<&str>,
    ) -> anyhow::Result<()> {
        self.finish_upload_session_seal_attempt(
            session_id,
            "retry_wait",
            error_code,
            delay_ms,
            http_status,
            server_code,
            server_message,
            request_id,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn reject_upload_session_seal(
        &self,
        session_id: &str,
        error_code: &str,
        http_status: Option<u16>,
        server_code: Option<&str>,
        server_message: Option<&str>,
        request_id: Option<&str>,
    ) -> anyhow::Result<()> {
        self.finish_upload_session_seal_attempt(
            session_id,
            "rejected",
            error_code,
            0,
            http_status,
            server_code,
            server_message,
            request_id,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn finish_upload_session_seal_attempt(
        &self,
        session_id: &str,
        state: &str,
        error_code: &str,
        delay_ms: u64,
        http_status: Option<u16>,
        server_code: Option<&str>,
        server_message: Option<&str>,
        request_id: Option<&str>,
    ) -> anyhow::Result<()> {
        ensure!(
            matches!(state, "retry_wait" | "rejected"),
            "invalid seal terminal state"
        );
        ensure_upload_error_code(error_code)?;
        let connection = self.connection.lock();
        let changed = connection.execute(
            "UPDATE upload_sessions
                SET seal_state = ?2, seal_last_error_code = ?3,
                    seal_next_attempt_at_ms = CAST(unixepoch('subsec') * 1000 AS INTEGER) + ?4,
                    seal_http_status = ?5, seal_server_code = ?6,
                    seal_server_message = ?7, seal_request_id = ?8,
                    updated_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now')
              WHERE session_id = ?1 AND seal_state = 'running'",
            params![
                session_id,
                state,
                error_code,
                u64_to_i64(delay_ms, "session seal delay")?,
                http_status.map(i64::from),
                server_code,
                server_message,
                request_id,
            ],
        )?;
        ensure!(changed == 1, "session seal attempt lost its running task");
        Ok(())
    }

    pub fn upload_session_seal_state(
        &self,
        session_id: &str,
    ) -> anyhow::Result<(String, Option<String>)> {
        self.connection
            .lock()
            .query_row(
                "SELECT seal_state, seal_last_error_code
                   FROM upload_sessions WHERE session_id = ?1",
                params![session_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(Into::into)
    }

    pub fn session_can_be_sealed(&self, session_id: &str) -> anyhow::Result<bool> {
        let connection = self.connection.lock();
        connection
            .query_row(
                "SELECT s.state = 'completed'
                    AND us.upload_requested = 1
                    AND us.sealed = 0
                    AND (SELECT COUNT(*) FROM upload_jobs j WHERE j.session_id = s.id) > 0
                    AND NOT EXISTS (
                        SELECT 1 FROM upload_jobs j
                         WHERE j.session_id = s.id AND j.state NOT IN ('stored', 'verified')
                    )
                    AND (SELECT COUNT(*) FROM upload_jobs j WHERE j.session_id = s.id) =
                        (SELECT COUNT(*) FROM segments g
                          WHERE g.session_id = s.id AND g.state = 'ready')
               FROM sessions s
               JOIN upload_sessions us ON us.session_id = s.id
              WHERE s.id = ?1",
                params![session_id],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    pub fn mark_upload_session_sealed(&self, session_id: &str) -> anyhow::Result<()> {
        let connection = self.connection.lock();
        let changed = connection.execute(
            "UPDATE upload_sessions SET sealed = 1, seal_state = 'sealed',
                    seal_last_error_code = NULL, seal_next_attempt_at_ms = 0,
                    updated_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now')
              WHERE session_id = ?1",
            params![session_id],
        )?;
        ensure!(changed == 1, "upload session disappeared while sealing");
        Ok(())
    }

    pub fn release_data_model_job(
        &self,
        session_id: &str,
        error_code: Option<&str>,
    ) -> anyhow::Result<DataModelJobStatus> {
        if let Some(error_code) = error_code {
            validate_data_model_error_code(error_code)?;
        }
        let mut connection = self.connection.lock();
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = load_data_model_job(&tx, session_id)?
            .with_context(|| format!("DataModel job for session {session_id} is missing"))?;
        ensure!(
            matches!(
                current.state,
                DataModelJobState::Running
                    | DataModelJobState::Finalizing
                    | DataModelJobState::Pending
            ),
            "DataModel job cannot be released from state {}",
            current.state.as_str()
        );
        tx.execute(
            "UPDATE data_model_jobs
             SET state = 'pending', error_code = ?2,
                 updated_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now')
             WHERE session_id = ?1",
            params![session_id, error_code],
        )?;
        let updated = load_data_model_job(&tx, session_id)?
            .context("DataModel job disappeared while releasing")?;
        tx.commit()?;
        Ok(updated)
    }

    pub fn mark_data_model_failed(
        &self,
        session_id: &str,
        error_code: &str,
    ) -> anyhow::Result<DataModelJobStatus> {
        validate_data_model_error_code(error_code)?;
        let mut connection = self.connection.lock();
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = load_data_model_job(&tx, session_id)?
            .with_context(|| format!("DataModel job for session {session_id} is missing"))?;
        ensure!(
            !matches!(
                current.state,
                DataModelJobState::Completed | DataModelJobState::Cancelled
            ),
            "DataModel job cannot fail from state {}",
            current.state.as_str()
        );
        tx.execute(
            "UPDATE data_model_jobs
             SET state = 'failed', error_code = ?2,
                 updated_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now')
             WHERE session_id = ?1",
            params![session_id, error_code],
        )?;
        let updated = load_data_model_job(&tx, session_id)?
            .context("DataModel job disappeared while failing")?;
        tx.commit()?;
        Ok(updated)
    }

    pub fn list_sessions(&self) -> anyhow::Result<Vec<SessionHistorySummary>> {
        let connection = self.connection.lock();
        let mut statement =
            connection.prepare("SELECT id FROM sessions ORDER BY created_at_utc DESC, id DESC")?;
        let ids = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        ids.iter()
            .map(|session_id| load_session_summary(&connection, session_id))
            .collect()
    }

    /// Opens the durable upload gate for one completed local session.
    ///
    /// Upload preparation is intentionally derived from immutable ready RAW in
    /// the same transaction as the user's request. DataModel is a rebuildable
    /// local projection; its progress or failure must not gate RAW transport.
    pub fn request_session_upload(
        &self,
        session_id: &str,
    ) -> anyhow::Result<SessionHistorySummary> {
        let mut connection = self.connection.lock();
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (state, segment_count, ready_segments, deletion_pending, validation_runs): (
            String,
            u32,
            u32,
            bool,
            u32,
        ) = tx
            .query_row(
                "SELECT s.state,
                        (SELECT COUNT(*) FROM segments g
                          WHERE g.session_id = s.id),
                        (SELECT COUNT(*) FROM segments g
                          WHERE g.session_id = s.id AND g.state = 'ready'),
                        EXISTS(
                            SELECT 1 FROM session_deletions d
                             WHERE d.session_id = s.id
                               AND d.state IN ('prepared', 'staged', 'cleanup_pending')
                        ),
                        (SELECT COUNT(*) FROM capture_runs r
                          WHERE r.session_id = s.id
                            AND r.performance_validation_only = 1)
                   FROM sessions s
                  WHERE s.id = ?1",
                params![session_id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .with_context(|| format!("session {session_id} is missing from the SQLite journal"))?;
        ensure!(
            state == "completed",
            "session upload can only be requested after recording completes"
        );
        ensure!(
            segment_count > 0,
            "session upload requires at least one ready segment"
        );
        ensure!(
            ready_segments == segment_count,
            "session upload requires every RAW segment to be ready ({ready_segments}/{segment_count})"
        );
        ensure!(
            !deletion_pending,
            "session upload cannot start while local deletion is pending"
        );
        ensure!(
            validation_runs == 0,
            "performance-validation-only session is permanently non-deliverable"
        );
        enqueue_ready_segments_for_upload(&tx, session_id)?;
        recover_manually_recoverable_upload_request_rejections(&tx, session_id)?;
        let upload_job_segments: u32 = tx.query_row(
            "SELECT COUNT(*) FROM upload_jobs WHERE session_id = ?1",
            params![session_id],
            |row| row.get(0),
        )?;
        ensure!(
            upload_job_segments == ready_segments,
            "session upload preparation is incomplete ({upload_job_segments}/{ready_segments} ready segments queued)"
        );
        tx.execute(
            "INSERT INTO upload_sessions(
                session_id, registration_state, upload_requested,
                upload_requested_at_utc, created_at_utc, updated_at_utc
             ) VALUES (
                ?1, 'pending', 1, strftime('%Y-%m-%dT%H:%M:%fZ','now'),
                strftime('%Y-%m-%dT%H:%M:%fZ','now'),
                strftime('%Y-%m-%dT%H:%M:%fZ','now')
             )
             ON CONFLICT(session_id) DO UPDATE SET
                upload_requested = 1,
                upload_requested_at_utc = COALESCE(
                    upload_sessions.upload_requested_at_utc,
                    strftime('%Y-%m-%dT%H:%M:%fZ','now')
                ),
                updated_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now')
              WHERE upload_sessions.upload_requested = 0",
            params![session_id],
        )?;
        tx.commit()?;
        load_session_summary(&connection, session_id)
    }

    pub fn session_detail(&self, session_id: &str) -> anyhow::Result<SessionHistoryDetail> {
        let connection = self.connection.lock();
        let session = load_session_summary(&connection, session_id)?;
        let mut run_statement = connection.prepare(
            "SELECT id, run_name, seq, state, frame_count, ready_segments,
                    input_event_count, input_queue_overflows, stop_reason, error_code,
                    started_at_utc, ended_at_utc
             FROM capture_runs WHERE session_id = ?1 ORDER BY seq",
        )?;
        let runs = run_statement
            .query_map(params![session_id], |row| {
                Ok(SessionHistoryRun {
                    run_id: row.get(0)?,
                    run_name: row.get(1)?,
                    seq: row.get(2)?,
                    state: row.get(3)?,
                    frame_count: row.get(4)?,
                    ready_segments: row.get(5)?,
                    input_event_count: row.get(6)?,
                    input_queue_overflows: row.get(7)?,
                    stop_reason: row.get(8)?,
                    error_code: row.get(9)?,
                    started_at_utc: row.get(10)?,
                    ended_at_utc: row.get(11)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let mut segment_statement = connection.prepare(
            "SELECT g.run_id, g.run_name, g.seq, g.state, g.session_frame_start, g.n_frames,
                    g.qpc_start, g.qpc_end, g.bytes, g.event_count,
                    CASE WHEN g.video_sha256 IS NOT NULL AND g.frames_sha256 IS NOT NULL
                                   AND g.events_sha256 IS NOT NULL AND g.meta_sha256 IS NOT NULL
                         THEN 1 ELSE 0 END,
                    json_extract(g.audio_json, '$.state'),
                    json_extract(g.audio_json, '$.failure_code'),
                    json_extract(g.audio_json, '$.bytes'),
                    g.error_code, g.ready_at_utc,
                    CASE WHEN j.remote_retention_state = 'purged' THEN 'purged' ELSE j.state END,
                    j.attempt_count, j.next_attempt_at_ms, j.last_error_code,
                    j.http_status, j.server_code, j.server_message, j.request_id,
                    COALESCE((SELECT COUNT(*) FROM upload_parts p WHERE p.segment_id = g.id), 0),
                    COALESCE((SELECT SUM(p.byte_size) FROM upload_parts p WHERE p.segment_id = g.id), 0)
               FROM segments g
               LEFT JOIN upload_jobs j ON j.segment_id = g.id
              WHERE g.session_id = ?1 ORDER BY g.run_id, g.seq",
        )?;
        let segments = segment_statement
            .query_map(params![session_id], |row| {
                Ok(SessionHistorySegment {
                    run_id: row.get(0)?,
                    run_name: row.get(1)?,
                    seq: row.get(2)?,
                    state: row.get(3)?,
                    session_frame_start: row.get(4)?,
                    frame_count: row.get(5)?,
                    qpc_start: row.get(6)?,
                    qpc_end: row.get(7)?,
                    video_bytes: row.get(8)?,
                    event_count: row.get(9)?,
                    hashes_complete: row.get::<_, i64>(10)? == 1,
                    audio_state: row.get(11)?,
                    audio_failure_code: row.get(12)?,
                    audio_bytes: row.get(13)?,
                    error_code: row.get(14)?,
                    ready_at_utc: row.get(15)?,
                    upload_state: row.get(16)?,
                    upload_attempt_count: row.get(17)?,
                    upload_next_attempt_at_ms: row.get(18)?,
                    upload_error_code: row.get(19)?,
                    upload_http_status: row.get(20)?,
                    upload_server_code: row.get(21)?,
                    upload_server_message: row.get(22)?,
                    upload_request_id: row.get(23)?,
                    uploaded_part_count: row.get(24)?,
                    uploaded_bytes: row.get(25)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let mut marker_statement = connection.prepare(
            "SELECT id, run_id, run_name, seq, kind, qpc, qpc_frequency,
                    run_frame, session_frame, video_time_us, alignment_state,
                    segment_seq, created_at_utc
             FROM markers WHERE session_id = ?1 ORDER BY seq",
        )?;
        let markers = marker_statement
            .query_map(params![session_id], |row| {
                Ok(SessionHistoryMarker {
                    marker_id: row.get(0)?,
                    run_id: row.get(1)?,
                    run_name: row.get(2)?,
                    seq: row.get(3)?,
                    kind: row.get(4)?,
                    qpc: row.get(5)?,
                    qpc_frequency: row.get(6)?,
                    run_frame: row.get(7)?,
                    session_frame: row.get(8)?,
                    video_time_us: row.get(9)?,
                    alignment_state: row.get(10)?,
                    segment_seq: row.get(11)?,
                    created_at_utc: row.get(12)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let preflights = load_preflights_for_session(&connection, session_id)?;
        Ok(SessionHistoryDetail {
            session,
            runs,
            segments,
            markers,
            preflights,
        })
    }

    pub fn prepare_session_deletion(
        &self,
        deletion_id: &str,
        session_id: &str,
        staged_relative_path: &str,
    ) -> anyhow::Result<()> {
        let mut connection = self.connection.lock();
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let state: String = tx
            .query_row(
                "SELECT state FROM sessions WHERE id = ?1",
                params![session_id],
                |row| row.get(0),
            )
            .with_context(|| format!("session {session_id} is missing from the SQLite journal"))?;
        ensure!(
            matches!(state.as_str(), "completed" | "faulted" | "corrupt"),
            "session {session_id} cannot be deleted from state {state}"
        );
        let unfinished_uploads: i64 = tx.query_row(
            "SELECT COUNT(*) FROM upload_jobs
              WHERE session_id = ?1 AND state NOT IN ('verified', 'rejected')",
            params![session_id],
            |row| row.get(0),
        )?;
        ensure!(
            unfinished_uploads == 0,
            "session {session_id} still has unfinished R2 uploads"
        );
        let unfinished_session_artifacts: i64 = tx.query_row(
            "SELECT COUNT(*) FROM session_artifact_uploads
              WHERE session_id = ?1 AND state NOT IN ('verified', 'rejected')",
            params![session_id],
            |row| row.get(0),
        )?;
        ensure!(
            unfinished_session_artifacts == 0,
            "session {session_id} still has unfinished session-artifact uploads"
        );
        tx.execute(
            "INSERT INTO session_deletions (
                id, session_id, state, staged_relative_path, requested_at_utc, updated_at_utc
             ) VALUES (?1, ?2, 'prepared', ?3,
                       strftime('%Y-%m-%dT%H:%M:%fZ','now'),
                       strftime('%Y-%m-%dT%H:%M:%fZ','now'))",
            params![deletion_id, session_id, staged_relative_path],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn mark_session_deletion_staged(&self, deletion_id: &str) -> anyhow::Result<()> {
        let connection = self.connection.lock();
        let changed = connection.execute(
            "UPDATE session_deletions SET state = 'staged',
                    updated_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now')
             WHERE id = ?1 AND state IN ('prepared', 'staged')",
            params![deletion_id],
        )?;
        ensure!(changed == 1, "session deletion is not prepared");
        Ok(())
    }

    pub fn commit_session_deletion(
        &self,
        deletion_id: &str,
        session_id: &str,
    ) -> anyhow::Result<()> {
        let mut connection = self.connection.lock();
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let deletion: (String, String) = tx.query_row(
            "SELECT session_id, state FROM session_deletions WHERE id = ?1",
            params![deletion_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        ensure!(
            deletion.0 == session_id && matches!(deletion.1.as_str(), "prepared" | "staged"),
            "session deletion identity/state mismatch"
        );
        tx.execute(
            "DELETE FROM recovery_findings WHERE session_id = ?1",
            params![session_id],
        )?;
        // CP2077 facts are local evidence owned by the session. Remove them in
        // the same transaction before the RESTRICT foreign keys on the v32
        // table can block the ordinary terminal-session deletion path.
        tx.execute(
            "DELETE FROM cp2077_main_run_facts WHERE session_id = ?1",
            params![session_id],
        )?;
        let changed = tx.execute("DELETE FROM sessions WHERE id = ?1", params![session_id])?;
        ensure!(changed == 1, "session disappeared before deletion commit");
        tx.execute(
            "UPDATE session_deletions SET state = 'cleanup_pending',
                    updated_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now')
             WHERE id = ?1",
            params![deletion_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn finish_session_deletion(&self, deletion_id: &str) -> anyhow::Result<()> {
        let connection = self.connection.lock();
        let changed = connection.execute(
            "UPDATE session_deletions SET state = 'completed', session_id = 'deleted:' || id,
                    error_code = NULL,
                    completed_at_utc = COALESCE(completed_at_utc, strftime('%Y-%m-%dT%H:%M:%fZ','now')),
                    updated_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now')
             WHERE id = ?1 AND state IN ('cleanup_pending', 'completed')",
            params![deletion_id],
        )?;
        ensure!(changed == 1, "session deletion is not awaiting cleanup");
        Ok(())
    }

    pub fn cancel_session_deletion(
        &self,
        deletion_id: &str,
        error_code: &str,
    ) -> anyhow::Result<()> {
        let connection = self.connection.lock();
        connection.execute(
            "UPDATE session_deletions SET state = 'cancelled', error_code = ?2,
                    completed_at_utc = COALESCE(completed_at_utc, strftime('%Y-%m-%dT%H:%M:%fZ','now')),
                    updated_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now')
             WHERE id = ?1 AND state IN ('prepared', 'staged')",
            params![deletion_id, error_code],
        )?;
        Ok(())
    }

    pub fn pending_session_deletions(&self) -> anyhow::Result<Vec<PendingSessionDeletion>> {
        let connection = self.connection.lock();
        let mut statement = connection.prepare(
            "SELECT id, session_id, state, staged_relative_path
             FROM session_deletions
             WHERE state IN ('prepared', 'staged', 'cleanup_pending')
             ORDER BY requested_at_utc, id",
        )?;
        Ok(statement
            .query_map([], |row| {
                Ok(PendingSessionDeletion {
                    deletion_id: row.get(0)?,
                    session_id: row.get(1)?,
                    state: row.get(2)?,
                    staged_relative_path: row.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?)
    }

    pub fn session_exists(&self, session_id: &str) -> anyhow::Result<bool> {
        let connection = self.connection.lock();
        Ok(connection
            .query_row(
                "SELECT 1 FROM sessions WHERE id = ?1",
                params![session_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    pub fn recovery_inventory(&self) -> anyhow::Result<RecoveryInventory> {
        let connection = self.connection.lock();

        let mut session_statement = connection.prepare(
            "SELECT id, state, schema_version,
                    session_contract, catalog_id, catalog_version, catalog_hash,
                    device_profile_id, device_profile_version, device_profile_hash,
                    recording_profile_id, recording_profile_version, recording_profile_hash,
                    recording_profile_hash_kind,
                    game_id, game_exe_hash, game_profile_version, game_profile_hash,
                    mapping_id, mapping_version, mapping_hash
             FROM sessions ORDER BY id",
        )?;
        let sessions = session_statement
            .query_map([], |row| {
                Ok(RecoverySession {
                    id: row.get(0)?,
                    state: row.get(1)?,
                    schema_version: row.get(2)?,
                    session_contract: row.get(3)?,
                    catalog_id: row.get(4)?,
                    catalog_version: row.get(5)?,
                    catalog_hash: row.get(6)?,
                    device_profile_id: row.get(7)?,
                    device_profile_version: row.get(8)?,
                    device_profile_hash: row.get(9)?,
                    recording_profile_id: row.get(10)?,
                    recording_profile_version: row.get(11)?,
                    recording_profile_hash: row.get(12)?,
                    recording_profile_hash_kind: row.get(13)?,
                    game_id: row.get(14)?,
                    game_executable_sha256: row.get(15)?,
                    game_profile_version: row.get(16)?,
                    game_profile_hash: row.get(17)?,
                    mapping_id: row.get(18)?,
                    mapping_version: row.get(19)?,
                    mapping_hash: row.get(20)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        let mut run_statement = connection.prepare(
            "SELECT id, session_id, run_name, state, fps_num, fps_den, width, height
             FROM capture_runs ORDER BY session_id, seq",
        )?;
        let runs = run_statement
            .query_map([], |row| {
                Ok(RecoveryRun {
                    id: row.get(0)?,
                    session_id: row.get(1)?,
                    run_name: row.get(2)?,
                    state: row.get(3)?,
                    fps_num: row.get(4)?,
                    fps_den: row.get(5)?,
                    width: row.get(6)?,
                    height: row.get(7)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        let mut segment_statement = connection.prepare(
            "SELECT id, session_id, run_id, run_name, seq, state,
                    session_frame_start, n_frames, qpc_start, qpc_end,
                    video_path, frames_path, events_path, meta_path, manifest_path,
                    video_sha256, frames_sha256, events_sha256, meta_sha256,
                    bytes, event_count, duplicate_frames, source_superseded, source_drops,
                    audio_json
             FROM segments ORDER BY session_id, run_id, seq",
        )?;
        let segments = segment_statement
            .query_map([], |row| {
                Ok(RecoverySegment {
                    id: row.get(0)?,
                    session_id: row.get(1)?,
                    run_id: row.get(2)?,
                    run_name: row.get(3)?,
                    seq: row.get(4)?,
                    state: row.get(5)?,
                    session_frame_start: row.get(6)?,
                    n_frames: row.get(7)?,
                    qpc_start: row.get(8)?,
                    qpc_end: row.get(9)?,
                    video_path: row.get(10)?,
                    frames_path: row.get(11)?,
                    events_path: row.get(12)?,
                    meta_path: row.get(13)?,
                    manifest_path: row.get(14)?,
                    video_sha256: row.get(15)?,
                    frames_sha256: row.get(16)?,
                    events_sha256: row.get(17)?,
                    meta_sha256: row.get(18)?,
                    bytes: row.get(19)?,
                    event_count: row.get(20)?,
                    duplicate_frames: row.get(21)?,
                    source_superseded: row.get(22)?,
                    source_drops: row.get(23)?,
                    audio: parse_audio_json_row(row.get(24)?, 24)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let markers = load_all_markers(&connection)?;
        let heartbeats = load_all_heartbeats(&connection)?;

        Ok(RecoveryInventory {
            sessions,
            runs,
            segments,
            markers,
            heartbeats,
        })
    }

    pub fn diagnostic_snapshot(
        &self,
        session_id: Option<&str>,
    ) -> anyhow::Result<JournalDiagnosticSnapshot> {
        let connection = self.connection.lock();
        let mut session_statement = connection.prepare(
            "SELECT s.id, s.state, s.started_at_utc, s.ended_at_utc,
                    session_contract, catalog_id, catalog_version, catalog_hash,
                    device_profile_id, device_profile_version, device_profile_hash,
                    recording_profile_id, recording_profile_version, recording_profile_hash,
                    recording_profile_hash_kind,
                    game_id, game_profile_version, game_profile_hash,
                    mapping_id, mapping_version, mapping_hash,
                    schema_version, stop_reason, s.last_error_code,
                    us.seal_state, us.seal_http_status, us.seal_server_code,
                    us.seal_server_message, us.seal_request_id
               FROM sessions s
               LEFT JOIN upload_sessions us ON us.session_id = s.id
              WHERE ?1 IS NOT NULL AND s.id = ?1 ORDER BY s.id",
        )?;
        let sessions = session_statement
            .query_map(params![session_id], |row| {
                Ok(JournalDiagnosticSession {
                    id: row.get(0)?,
                    state: row.get(1)?,
                    started_at_utc: row.get(2)?,
                    ended_at_utc: row.get(3)?,
                    session_contract: row.get(4)?,
                    catalog_id: row.get(5)?,
                    catalog_version: row.get(6)?,
                    catalog_hash: row.get(7)?,
                    device_profile_id: row.get(8)?,
                    device_profile_version: row.get(9)?,
                    device_profile_hash: row.get(10)?,
                    recording_profile_id: row.get(11)?,
                    recording_profile_version: row.get(12)?,
                    recording_profile_hash: row.get(13)?,
                    recording_profile_hash_kind: row.get(14)?,
                    game_id: row.get(15)?,
                    game_profile_version: row.get(16)?,
                    game_profile_hash: row.get(17)?,
                    mapping_id: row.get(18)?,
                    mapping_version: row.get(19)?,
                    mapping_hash: row.get(20)?,
                    schema_version: row.get(21)?,
                    stop_reason: row.get(22)?,
                    last_error_code: row.get(23)?,
                    upload_seal_state: row.get(24)?,
                    upload_seal_http_status: row.get(25)?,
                    upload_seal_server_code: row.get(26)?,
                    upload_seal_server_message: row.get(27)?,
                    upload_seal_request_id: row.get(28)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        let mut run_statement = connection.prepare(
            "SELECT session_id, run_name, seq, state, backend, encoder_backend,
                    fps_num, fps_den, width, height, frame_count, ready_segments,
                    duplicate_frames, source_superseded, input_event_count,
                    input_queue_overflows, stop_reason, error_code
             FROM capture_runs WHERE ?1 IS NOT NULL AND session_id = ?1
             ORDER BY seq",
        )?;
        let runs = run_statement
            .query_map(params![session_id], |row| {
                Ok(JournalDiagnosticRun {
                    session_id: row.get(0)?,
                    run_name: row.get(1)?,
                    seq: row.get(2)?,
                    state: row.get(3)?,
                    backend: row.get(4)?,
                    encoder_backend: row.get(5)?,
                    fps_num: row.get(6)?,
                    fps_den: row.get(7)?,
                    width: row.get(8)?,
                    height: row.get(9)?,
                    frame_count: row.get(10)?,
                    ready_segments: row.get(11)?,
                    duplicate_frames: row.get(12)?,
                    source_superseded: row.get(13)?,
                    input_event_count: row.get(14)?,
                    input_queue_overflows: row.get(15)?,
                    stop_reason: row.get(16)?,
                    error_code: row.get(17)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        let mut segment_statement = connection.prepare(
            "SELECT session_id, run_name, seq, state, session_frame_start, n_frames,
                    qpc_start, qpc_end, video_sha256, frames_sha256, events_sha256,
                    meta_sha256, bytes, event_count, duplicate_frames,
                    source_superseded, source_drops,
                    json_extract(audio_json, '$.state'),
                    json_extract(audio_json, '$.failure_code'),
                    json_extract(audio_json, '$.bytes'), error_code
             FROM segments WHERE ?1 IS NOT NULL AND session_id = ?1
             ORDER BY run_id, seq",
        )?;
        let segments = segment_statement
            .query_map(params![session_id], |row| {
                Ok(JournalDiagnosticSegment {
                    session_id: row.get(0)?,
                    run_name: row.get(1)?,
                    seq: row.get(2)?,
                    state: row.get(3)?,
                    session_frame_start: row.get(4)?,
                    n_frames: row.get(5)?,
                    qpc_start: row.get(6)?,
                    qpc_end: row.get(7)?,
                    video_sha256: row.get(8)?,
                    frames_sha256: row.get(9)?,
                    events_sha256: row.get(10)?,
                    meta_sha256: row.get(11)?,
                    bytes: row.get(12)?,
                    event_count: row.get(13)?,
                    duplicate_frames: row.get(14)?,
                    source_superseded: row.get(15)?,
                    source_drops: row.get(16)?,
                    audio_state: row.get(17)?,
                    audio_failure_code: row.get(18)?,
                    audio_bytes: row.get(19)?,
                    error_code: row.get(20)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        let mut marker_statement = connection.prepare(
            "SELECT id, session_id, run_name, seq, kind, qpc, qpc_frequency,
                    run_frame, session_frame, video_time_us, alignment_state, segment_seq
             FROM markers WHERE ?1 IS NOT NULL AND session_id = ?1 ORDER BY seq",
        )?;
        let markers = marker_statement
            .query_map(params![session_id], |row| {
                Ok(JournalDiagnosticMarker {
                    marker_id: row.get(0)?,
                    session_id: row.get(1)?,
                    run_id: row.get(2)?,
                    seq: row.get(3)?,
                    kind: row.get(4)?,
                    qpc: row.get(5)?,
                    qpc_frequency: row.get(6)?,
                    run_frame: row.get(7)?,
                    session_frame: row.get(8)?,
                    video_time_us: row.get(9)?,
                    alignment_state: row.get(10)?,
                    segment_seq: row.get(11)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let preflights = if let Some(session_id) = session_id {
            load_preflights_for_session(&connection, session_id)?
        } else {
            load_recent_preflights(&connection, 200)?
        };
        let mut data_model_statement = connection.prepare(
            "SELECT session_id, state, ready_segments, processed_segments,
                    published_chunks, published_frames, pending_frames,
                    finalize_requested, output_relative_path, error_code
             FROM data_model_jobs
             WHERE ?1 IS NOT NULL AND session_id = ?1",
        )?;
        let data_model_rows = data_model_statement
            .query_map(params![session_id], data_model_job_from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        let data_model_jobs = data_model_rows
            .into_iter()
            .map(data_model_job_from_sql)
            .collect::<anyhow::Result<Vec<_>>>()?;

        let latest_recovery = connection
            .query_row(
                "SELECT id, state, scanned_sessions, verified_ready_segments,
                        quarantined_items, corrupt_segments, orphan_items, error_code
                 FROM recovery_runs ORDER BY started_at_utc DESC, id DESC LIMIT 1",
                [],
                |row| {
                    Ok(JournalDiagnosticRecovery {
                        id: row.get(0)?,
                        state: row.get(1)?,
                        scanned_sessions: row.get(2)?,
                        verified_ready_segments: row.get(3)?,
                        quarantined_items: row.get(4)?,
                        corrupt_segments: row.get(5)?,
                        orphan_items: row.get(6)?,
                        error_code: row.get(7)?,
                    })
                },
            )
            .optional()?;
        let mut recovery_findings = Vec::new();
        if let Some(recovery) = &latest_recovery {
            let mut finding_statement = connection.prepare(
                "SELECT severity, code, action, session_id, run_name, segment_seq, message
                 FROM recovery_findings
                 WHERE recovery_id = ?1
                   AND (?2 IS NULL OR session_id IS NULL OR session_id = ?2)
                 ORDER BY id",
            )?;
            recovery_findings = finding_statement
                .query_map(params![recovery.id, session_id], |row| {
                    Ok(JournalDiagnosticRecoveryFinding {
                        severity: row.get(0)?,
                        code: row.get(1)?,
                        action: row.get(2)?,
                        session_id: row.get(3)?,
                        run_name: row.get(4)?,
                        segment_seq: row.get(5)?,
                        message: row.get(6)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
        }

        Ok(JournalDiagnosticSnapshot {
            scoped_session_id: session_id.map(str::to_owned),
            sessions,
            runs,
            segments,
            markers,
            preflights,
            data_model_jobs,
            latest_recovery,
            recovery_findings,
        })
    }

    pub fn commit_recovery(&self, recovery: &RecoveryCommit<'_>) -> anyhow::Result<()> {
        ensure!(
            !recovery.recovery_id.trim().is_empty(),
            "recovery id is empty"
        );
        ensure!(
            recovery.report_path.starts_with("recovery/")
                && !recovery.report_path.contains("..")
                && !Path::new(recovery.report_path).is_absolute(),
            "recovery report path must be app-local and relative"
        );
        let mut connection = self.connection.lock();
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;

        for run_id in recovery.unfinished_run_ids {
            let state = run_state(&tx, run_id)?;
            if matches!(state.as_str(), "starting" | "recording") {
                tx.execute(
                    "UPDATE capture_runs SET state = 'faulted',
                            error_code = 'RECOVERY_UNFINISHED_RUN',
                            stop_reason = 'recovery_unfinished',
                            ended_at_utc = COALESCE(ended_at_utc, strftime('%Y-%m-%dT%H:%M:%fZ','now')),
                            updated_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now')
                     WHERE id = ?1 AND state IN ('starting', 'recording')",
                    params![run_id],
                )?;
                tx.execute(
                    "UPDATE sessions SET state = 'faulted',
                            stop_reason = 'recovery_unfinished',
                            last_error_code = 'RECOVERY_UNFINISHED_RUN',
                            ended_at_utc = COALESCE(ended_at_utc, strftime('%Y-%m-%dT%H:%M:%fZ','now')),
                            updated_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now')
                     WHERE id = (SELECT session_id FROM capture_runs WHERE id = ?1)
                       AND state != 'corrupt'",
                    params![run_id],
                )?;
                tx.execute(
                    "UPDATE markers SET alignment_state = 'outside_video'
                     WHERE run_id = ?1 AND alignment_state = 'pending'",
                    params![run_id],
                )?;
            } else {
                ensure!(
                    state == "faulted",
                    "unfinished recovery replay found run {run_id} in state {state}"
                );
            }
        }

        for segment_id in recovery.quarantined_segment_ids {
            let state = segment_state(&tx, segment_id)?;
            if matches!(state.as_str(), "active" | "finalizing") {
                tx.execute(
                    "UPDATE segments SET state = 'quarantined',
                            error_code = 'RECOVERY_UNFINISHED_SEGMENT',
                            updated_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now')
                     WHERE id = ?1 AND state IN ('active', 'finalizing')",
                    params![segment_id],
                )?;
            } else {
                ensure!(
                    state == "quarantined",
                    "segment quarantine replay found {segment_id} in state {state}"
                );
            }
        }

        for segment_id in recovery.corrupt_segment_ids {
            let state = segment_state(&tx, segment_id)?;
            let session_id: String = tx.query_row(
                "SELECT session_id FROM segments WHERE id = ?1",
                params![segment_id],
                |row| row.get(0),
            )?;
            if state == "ready" {
                tx.execute(
                    "UPDATE segments SET state = 'corrupt',
                            error_code = 'RECOVERY_READY_ARTIFACT_CORRUPT',
                            updated_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now')
                     WHERE id = ?1 AND state = 'ready'",
                    params![segment_id],
                )?;
                tx.execute(
                    "UPDATE capture_runs SET state = 'faulted',
                            error_code = 'RECOVERY_READY_ARTIFACT_CORRUPT',
                            stop_reason = COALESCE(stop_reason, 'recovery_corrupt'),
                            ended_at_utc = COALESCE(ended_at_utc, strftime('%Y-%m-%dT%H:%M:%fZ','now')),
                            updated_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now')
                     WHERE id = (SELECT run_id FROM segments WHERE id = ?1)",
                    params![segment_id],
                )?;
                tx.execute(
                    "UPDATE sessions SET state = 'corrupt',
                            stop_reason = 'recovery_corrupt',
                            last_error_code = 'RECOVERY_READY_ARTIFACT_CORRUPT',
                            ended_at_utc = COALESCE(ended_at_utc, strftime('%Y-%m-%dT%H:%M:%fZ','now')),
                            updated_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now')
                     WHERE id = (SELECT session_id FROM segments WHERE id = ?1)",
                    params![segment_id],
                )?;
            } else {
                ensure!(
                    state == "corrupt",
                    "corrupt recovery replay found {segment_id} in state {state}"
                );
            }
            invalidate_data_model_for_corrupt_source(&tx, &session_id)?;
        }

        for session_id in recovery.corrupt_session_ids {
            let changed = tx.execute(
                "UPDATE sessions SET state = 'corrupt',
                        stop_reason = 'recovery_corrupt',
                        last_error_code = 'RECOVERY_SESSION_CATALOG_CORRUPT',
                        ended_at_utc = COALESCE(ended_at_utc, strftime('%Y-%m-%dT%H:%M:%fZ','now')),
                        updated_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now')
                 WHERE id = ?1 AND state != 'corrupt'",
                params![session_id],
            )?;
            if changed == 0 {
                let state: String = tx.query_row(
                    "SELECT state FROM sessions WHERE id = ?1",
                    params![session_id],
                    |row| row.get(0),
                )?;
                ensure!(
                    state == "corrupt",
                    "corrupt session recovery replay found {session_id} in state {state}"
                );
            }
            invalidate_data_model_for_corrupt_source(&tx, session_id)?;
        }

        tx.execute(
            "INSERT INTO recovery_runs (
                id, state, scanned_sessions, verified_ready_segments,
                quarantined_items, corrupt_segments, orphan_items, report_path,
                started_at_utc, completed_at_utc
             ) VALUES (?1, 'completed', ?2, ?3, ?4, ?5, ?6, ?7,
                       strftime('%Y-%m-%dT%H:%M:%fZ','now'), strftime('%Y-%m-%dT%H:%M:%fZ','now'))",
            params![
                recovery.recovery_id,
                i64::from(recovery.scanned_sessions),
                i64::from(recovery.verified_ready_segments),
                i64::from(recovery.quarantined_items),
                i64::from(recovery.corrupt_segments),
                i64::from(recovery.orphan_items),
                recovery.report_path,
            ],
        )?;
        for finding in recovery.findings {
            ensure!(
                matches!(finding.severity, "info" | "warning" | "error"),
                "invalid recovery finding severity"
            );
            tx.execute(
                "INSERT INTO recovery_findings (
                    recovery_id, severity, code, action, session_id, run_name,
                    segment_seq, relative_path, message, created_at_utc
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9,
                           strftime('%Y-%m-%dT%H:%M:%fZ','now'))",
                params![
                    recovery.recovery_id,
                    finding.severity,
                    finding.code,
                    finding.action,
                    finding.session_id,
                    finding.run_name,
                    finding.segment_seq.map(i64::from),
                    finding.relative_path,
                    finding.message,
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }
}

const HEARTBEAT_AUDIT_COLUMNS: &str =
    "id, session_id, run_name, seq, qpc, qpc_frequency, collector";
const MARKER_AUDIT_COLUMNS: &str = "id, session_id, run_name, seq, kind, qpc, qpc_frequency, run_frame, session_frame, video_time_us, created_at_utc";

fn update_preflight_in_transaction(
    tx: &Transaction<'_>,
    evidence: &PreflightStatus,
) -> anyhow::Result<()> {
    preflight::verify(evidence)?;
    let existing = load_preflight_by_id(tx, &evidence.preflight_id)?
        .context("preflight evidence row is missing")?;
    ensure!(
        existing.request_id == evidence.request_id
            && existing.session_contract == evidence.session_contract,
        "preflight evidence identity changed"
    );
    if existing == *evidence {
        return Ok(());
    }
    ensure!(
        existing.result == "running",
        "terminal preflight evidence cannot be rewritten"
    );
    let evidence_json = serde_json::to_string(evidence)?;
    tx.execute(
        "UPDATE preflight_attempts
         SET state = ?2, phase = ?3, session_id = ?4, run_name = ?5,
             failure_code = ?6, details_id = ?7,
             evidence_json = ?8, evidence_hash = ?9,
             completed_at_utc = CASE WHEN ?2 = 'running' THEN NULL
                                     ELSE strftime('%Y-%m-%dT%H:%M:%fZ','now') END,
             updated_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now')
         WHERE id = ?1",
        params![
            evidence.preflight_id,
            evidence.result,
            evidence.phase,
            evidence.session_id,
            evidence.run_id,
            evidence.failure_code,
            evidence.details_id,
            evidence_json,
            evidence.evidence_hash,
        ],
    )?;
    Ok(())
}

fn load_preflight_by_request_id(
    connection: &Connection,
    request_id: &str,
) -> anyhow::Result<Option<PreflightStatus>> {
    let json = connection
        .query_row(
            "SELECT evidence_json FROM preflight_attempts WHERE request_id = ?1",
            params![request_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    json.map(|json| parse_preflight_json(&json)).transpose()
}

fn load_preflight_by_id(
    connection: &Connection,
    preflight_id: &str,
) -> anyhow::Result<Option<PreflightStatus>> {
    let json = connection
        .query_row(
            "SELECT evidence_json FROM preflight_attempts WHERE id = ?1",
            params![preflight_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    json.map(|json| parse_preflight_json(&json)).transpose()
}

fn load_preflight_catalog_identity(
    connection: &Connection,
    preflight_id: &str,
) -> anyhow::Result<Option<PreflightCatalogIdentity>> {
    connection
        .query_row(
            "SELECT target_window_id, session_contract, catalog_id, catalog_version, catalog_hash,
                    device_profile_id, device_profile_version, device_profile_hash,
                    recording_profile_id, recording_profile_version, recording_profile_hash,
                    game_id, game_profile_version, game_profile_hash,
                    mapping_id, mapping_version, mapping_hash
             FROM preflight_attempts WHERE id = ?1",
            params![preflight_id],
            |row| {
                Ok(PreflightCatalogIdentity {
                    target_id: row.get(0)?,
                    session_contract: row.get(1)?,
                    catalog_id: row.get(2)?,
                    catalog_version: row.get(3)?,
                    catalog_hash: row.get(4)?,
                    device_id: row.get(5)?,
                    device_version: row.get(6)?,
                    device_hash: row.get(7)?,
                    recording_id: row.get(8)?,
                    recording_version: row.get(9)?,
                    recording_hash: row.get(10)?,
                    game_id: row.get(11)?,
                    game_version: row.get(12)?,
                    game_hash: row.get(13)?,
                    mapping_id: row.get(14)?,
                    mapping_version: row.get(15)?,
                    mapping_hash: row.get(16)?,
                })
            },
        )
        .optional()
        .context("failed to load preflight catalog identity")
}

fn parse_preflight_json(json: &str) -> anyhow::Result<PreflightStatus> {
    let evidence: PreflightStatus =
        serde_json::from_str(json).context("invalid preflight evidence JSON in journal")?;
    preflight::verify(&evidence)?;
    Ok(evidence)
}

fn load_preflights_for_session(
    connection: &Connection,
    session_id: &str,
) -> anyhow::Result<Vec<PreflightStatus>> {
    let mut statement = connection.prepare(
        "SELECT evidence_json FROM preflight_attempts
         WHERE session_id = ?1 ORDER BY run_name, created_at_utc, id",
    )?;
    statement
        .query_map(params![session_id], |row| row.get::<_, String>(0))?
        .map(|row| parse_preflight_json(&row?))
        .collect()
}

fn load_recent_preflights(
    connection: &Connection,
    limit: u32,
) -> anyhow::Result<Vec<PreflightStatus>> {
    let mut statement = connection.prepare(
        "SELECT evidence_json FROM preflight_attempts
         ORDER BY created_at_utc DESC, id DESC LIMIT ?1",
    )?;
    statement
        .query_map(params![i64::from(limit)], |row| row.get::<_, String>(0))?
        .map(|row| parse_preflight_json(&row?))
        .collect()
}

fn terminalize_interrupted_preflights(connection: &mut Connection) -> anyhow::Result<()> {
    let rows = {
        let mut statement = connection.prepare(
            "SELECT evidence_json FROM preflight_attempts WHERE state = 'running' ORDER BY id",
        )?;
        statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?
    };
    if rows.is_empty() {
        return Ok(());
    }
    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    for json in rows {
        let mut evidence = parse_preflight_json(&json)?;
        preflight::append_check(
            &mut evidence,
            "startup_recovery",
            "failed",
            serde_json::json!({
                "reason": "process_ended_before_preflight_terminal_state",
                "recordingStarted": false,
            }),
        )?;
        let details_id =
            preflight::details_id(&evidence, "startup_recovery", "PREFLIGHT_INTERRUPTED");
        preflight::fail(
            &mut evidence,
            "startup_recovery",
            "failed",
            "PREFLIGHT_INTERRUPTED",
            &details_id,
        )?;
        let evidence_json = serde_json::to_string(&evidence)?;
        let changed = tx.execute(
            "UPDATE preflight_attempts
             SET state = 'failed', phase = 'startup_recovery',
                 failure_code = 'PREFLIGHT_INTERRUPTED', details_id = ?2,
                 evidence_json = ?3, evidence_hash = ?4,
                 completed_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now'),
                 updated_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now')
             WHERE id = ?1 AND state = 'running'",
            params![
                evidence.preflight_id,
                details_id,
                evidence_json,
                evidence.evidence_hash,
            ],
        )?;
        ensure!(
            changed == 1,
            "interrupted preflight disappeared during recovery"
        );
    }
    tx.commit()?;
    Ok(())
}

fn marker_audit_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<MarkerAudit> {
    Ok(MarkerAudit {
        marker_id: row.get(0)?,
        session_id: row.get(1)?,
        run_id: row.get(2)?,
        seq: row.get(3)?,
        kind: row.get(4)?,
        qpc: row.get(5)?,
        qpc_frequency: row.get(6)?,
        run_frame: row.get(7)?,
        session_frame: row.get(8)?,
        video_time_us: row.get(9)?,
        created_at_utc: row.get(10)?,
    })
}

fn heartbeat_audit_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<HeartbeatAudit> {
    Ok(HeartbeatAudit {
        heartbeat_id: row.get(0)?,
        session_id: row.get(1)?,
        run_id: row.get(2)?,
        seq: row.get(3)?,
        qpc: row.get(4)?,
        qpc_frequency: row.get(5)?,
        collector: row.get(6)?,
    })
}

fn load_heartbeat_by_run_qpc(
    connection: &Connection,
    run_id: &str,
    qpc: i64,
) -> anyhow::Result<Option<HeartbeatAudit>> {
    let sql =
        format!("SELECT {HEARTBEAT_AUDIT_COLUMNS} FROM heartbeats WHERE run_id = ?1 AND qpc = ?2");
    Ok(connection
        .query_row(&sql, params![run_id, qpc], heartbeat_audit_from_row)
        .optional()?)
}

fn load_heartbeats_for_session(
    connection: &Connection,
    session_id: &str,
) -> anyhow::Result<Vec<HeartbeatAudit>> {
    let sql = format!(
        "SELECT {HEARTBEAT_AUDIT_COLUMNS} FROM heartbeats WHERE session_id = ?1 ORDER BY seq"
    );
    let mut statement = connection.prepare(&sql)?;
    Ok(statement
        .query_map(params![session_id], heartbeat_audit_from_row)?
        .collect::<Result<Vec<_>, _>>()?)
}

fn load_all_heartbeats(connection: &Connection) -> anyhow::Result<Vec<HeartbeatAudit>> {
    let sql = format!("SELECT {HEARTBEAT_AUDIT_COLUMNS} FROM heartbeats ORDER BY session_id, seq");
    let mut statement = connection.prepare(&sql)?;
    Ok(statement
        .query_map([], heartbeat_audit_from_row)?
        .collect::<Result<Vec<_>, _>>()?)
}

fn load_marker_by_request_id(
    connection: &Connection,
    request_id: &str,
) -> anyhow::Result<Option<MarkerAudit>> {
    let sql = format!("SELECT {MARKER_AUDIT_COLUMNS} FROM markers WHERE request_id = ?1");
    Ok(connection
        .query_row(&sql, params![request_id], marker_audit_from_row)
        .optional()?)
}

fn load_markers_for_session(
    connection: &Connection,
    session_id: &str,
) -> anyhow::Result<Vec<MarkerAudit>> {
    let sql =
        format!("SELECT {MARKER_AUDIT_COLUMNS} FROM markers WHERE session_id = ?1 ORDER BY seq");
    let mut statement = connection.prepare(&sql)?;
    Ok(statement
        .query_map(params![session_id], marker_audit_from_row)?
        .collect::<Result<Vec<_>, _>>()?)
}

fn load_all_markers(connection: &Connection) -> anyhow::Result<Vec<MarkerAudit>> {
    let sql = format!("SELECT {MARKER_AUDIT_COLUMNS} FROM markers ORDER BY session_id, seq");
    let mut statement = connection.prepare(&sql)?;
    Ok(statement
        .query_map([], marker_audit_from_row)?
        .collect::<Result<Vec<_>, _>>()?)
}

fn align_marker_to_existing_ready_segment(
    tx: &Transaction<'_>,
    marker_id: &str,
    run_id: &str,
    qpc: i64,
    session_frame: u64,
) -> anyhow::Result<()> {
    let session_frame = u64_to_i64(session_frame, "marker session frame")?;
    let mut statement = tx.prepare(
        "SELECT seq FROM segments
         WHERE run_id = ?1 AND state = 'ready'
           AND qpc_start <= ?2 AND ?2 < qpc_end
           AND session_frame_start <= ?3
           AND ?3 < session_frame_start + n_frames
         ORDER BY seq LIMIT 2",
    )?;
    let matches = statement
        .query_map(params![run_id, qpc, session_frame], |row| {
            row.get::<_, u32>(0)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    ensure!(
        matches.len() <= 1,
        "marker overlaps multiple ready segments"
    );
    if let Some(segment_seq) = matches.first() {
        let changed = tx.execute(
            "UPDATE markers SET alignment_state = 'aligned', segment_seq = ?2
             WHERE id = ?1 AND alignment_state = 'pending'",
            params![marker_id, i64::from(*segment_seq)],
        )?;
        ensure!(changed <= 1, "marker alignment updated multiple rows");
    }
    Ok(())
}

#[derive(Default)]
struct PersistedUploadFailure {
    error_code: Option<String>,
    http_status: Option<u16>,
    server_code: Option<String>,
    server_message: Option<String>,
    request_id: Option<String>,
}

struct PersistedUploadSession {
    requested: bool,
    requested_at_utc: Option<String>,
    sealed: bool,
    failure: PersistedUploadFailure,
}

struct PersistedUploadTotals {
    pending: u32,
    running: u32,
    retry_wait: u32,
    stored: u32,
    verified: u32,
    rejected: u32,
    failure: PersistedUploadFailure,
}

fn load_session_summary(
    connection: &Connection,
    session_id: &str,
) -> anyhow::Result<SessionHistorySummary> {
    let mut summary = connection
        .query_row(
            "SELECT id, state, game_id, recording_profile_id, mapping_id, mapping_version,
                    session_contract, catalog_id, catalog_version, catalog_hash,
                    device_profile_id, device_profile_version, device_profile_hash,
                    recording_profile_version, recording_profile_hash, recording_profile_hash_kind,
                    game_profile_version, game_profile_hash,
                    mapping_hash,
                    started_at_utc, ended_at_utc, stop_reason, last_error_code
             FROM sessions WHERE id = ?1",
            params![session_id],
            |row| {
                Ok(SessionHistorySummary {
                    session_id: row.get(0)?,
                    state: row.get(1)?,
                    game_id: row.get(2)?,
                    recording_profile_id: row.get(3)?,
                    mapping_id: row.get(4)?,
                    mapping_version: row.get(5)?,
                    session_contract: row.get(6)?,
                    catalog_id: row.get(7)?,
                    catalog_version: row.get(8)?,
                    catalog_hash: row.get(9)?,
                    device_profile_id: row.get(10)?,
                    device_profile_version: row.get(11)?,
                    device_profile_hash: row.get(12)?,
                    recording_profile_version: row.get(13)?,
                    recording_profile_hash: row.get(14)?,
                    recording_profile_hash_kind: row.get(15)?,
                    game_profile_version: row.get(16)?,
                    game_profile_hash: row.get(17)?,
                    mapping_hash: row.get(18)?,
                    started_at_utc: row.get(19)?,
                    ended_at_utc: row.get(20)?,
                    stop_reason: row.get(21)?,
                    error_code: row.get(22)?,
                    run_count: 0,
                    segment_count: 0,
                    ready_segments: 0,
                    corrupt_segments: 0,
                    quarantined_segments: 0,
                    frame_count: 0,
                    input_event_count: 0,
                    marker_count: 0,
                    video_bytes: 0,
                    upload_requested: false,
                    upload_requested_at_utc: None,
                    upload_job_segments: 0,
                    performance_validation_only: false,
                    upload_requestable: false,
                    upload_pending_segments: 0,
                    upload_running_segments: 0,
                    upload_retry_wait_segments: 0,
                    upload_stored_segments: 0,
                    uploaded_segments: 0,
                    upload_verified_segments: 0,
                    upload_rejected_segments: 0,
                    upload_last_error_code: None,
                    upload_last_http_status: None,
                    upload_last_server_code: None,
                    upload_last_server_message: None,
                    upload_last_request_id: None,
                    reclaimable: false,
                })
            },
        )
        .with_context(|| format!("session {session_id} is missing from the SQLite journal"))?;
    let run_totals: (u32, u64, u64) = connection.query_row(
        "SELECT COUNT(*), COALESCE(SUM(frame_count), 0), COALESCE(SUM(input_event_count), 0)
         FROM capture_runs WHERE session_id = ?1",
        params![session_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    let segment_totals: (u32, u32, u32, u32, u64) = connection.query_row(
        "SELECT COUNT(*),
                COALESCE(SUM(CASE WHEN state = 'ready' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN state = 'corrupt' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN state = 'quarantined' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(bytes), 0)
         FROM segments WHERE session_id = ?1",
        params![session_id],
        |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        },
    )?;
    summary.run_count = run_totals.0;
    summary.frame_count = run_totals.1;
    summary.input_event_count = run_totals.2;
    summary.marker_count = connection.query_row(
        "SELECT COUNT(*) FROM markers WHERE session_id = ?1",
        params![session_id],
        |row| row.get(0),
    )?;
    summary.segment_count = segment_totals.0;
    summary.ready_segments = segment_totals.1;
    summary.corrupt_segments = segment_totals.2;
    summary.quarantined_segments = segment_totals.3;
    summary.video_bytes = segment_totals.4;
    let upload_request: Option<PersistedUploadSession> = connection
        .query_row(
            "SELECT upload_requested = 1, upload_requested_at_utc,
                    sealed = 1, seal_last_error_code, seal_http_status,
                    seal_server_code, seal_server_message, seal_request_id
               FROM upload_sessions WHERE session_id = ?1",
            params![session_id],
            |row| {
                Ok(PersistedUploadSession {
                    requested: row.get(0)?,
                    requested_at_utc: row.get(1)?,
                    sealed: row.get(2)?,
                    failure: PersistedUploadFailure {
                        error_code: row.get(3)?,
                        http_status: row.get(4)?,
                        server_code: row.get(5)?,
                        server_message: row.get(6)?,
                        request_id: row.get(7)?,
                    },
                })
            },
        )
        .optional()?;
    let mut upload_session_sealed = false;
    let mut upload_seal_failure = PersistedUploadFailure::default();
    if let Some(upload_request) = upload_request {
        summary.upload_requested = upload_request.requested;
        summary.upload_requested_at_utc = upload_request.requested_at_utc;
        upload_session_sealed = upload_request.sealed;
        upload_seal_failure = upload_request.failure;
    }
    let upload_totals: PersistedUploadTotals = connection.query_row(
        "SELECT COALESCE(SUM(CASE WHEN state = 'pending' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN state = 'running' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN state = 'retry_wait' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN state = 'stored' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN state = 'verified' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN state = 'rejected' THEN 1 ELSE 0 END), 0),
                (SELECT last_error_code FROM upload_jobs recent
                  WHERE recent.session_id = ?1 AND recent.last_error_code IS NOT NULL
                  ORDER BY recent.updated_at_utc DESC, recent.segment_id DESC LIMIT 1),
                (SELECT http_status FROM upload_jobs recent
                  WHERE recent.session_id = ?1 AND recent.last_error_code IS NOT NULL
                  ORDER BY recent.updated_at_utc DESC, recent.segment_id DESC LIMIT 1),
                (SELECT server_code FROM upload_jobs recent
                  WHERE recent.session_id = ?1 AND recent.last_error_code IS NOT NULL
                  ORDER BY recent.updated_at_utc DESC, recent.segment_id DESC LIMIT 1),
                (SELECT server_message FROM upload_jobs recent
                  WHERE recent.session_id = ?1 AND recent.last_error_code IS NOT NULL
                  ORDER BY recent.updated_at_utc DESC, recent.segment_id DESC LIMIT 1),
                (SELECT request_id FROM upload_jobs recent
                  WHERE recent.session_id = ?1 AND recent.last_error_code IS NOT NULL
                  ORDER BY recent.updated_at_utc DESC, recent.segment_id DESC LIMIT 1)
         FROM upload_jobs WHERE session_id = ?1",
        params![session_id],
        |row| {
            Ok(PersistedUploadTotals {
                pending: row.get(0)?,
                running: row.get(1)?,
                retry_wait: row.get(2)?,
                stored: row.get(3)?,
                verified: row.get(4)?,
                rejected: row.get(5)?,
                failure: PersistedUploadFailure {
                    error_code: row.get(6)?,
                    http_status: row.get(7)?,
                    server_code: row.get(8)?,
                    server_message: row.get(9)?,
                    request_id: row.get(10)?,
                },
            })
        },
    )?;
    summary.upload_pending_segments = upload_totals.pending;
    summary.upload_running_segments = upload_totals.running;
    summary.upload_retry_wait_segments = upload_totals.retry_wait;
    summary.upload_stored_segments = upload_totals.stored;
    summary.uploaded_segments = upload_totals.stored + upload_totals.verified;
    summary.upload_verified_segments = upload_totals.verified;
    summary.upload_rejected_segments = upload_totals.rejected;
    let latest_failure = if upload_seal_failure.error_code.is_some() {
        upload_seal_failure
    } else {
        upload_totals.failure
    };
    summary.upload_last_error_code = latest_failure.error_code;
    summary.upload_last_http_status = latest_failure.http_status;
    summary.upload_last_server_code = latest_failure.server_code;
    summary.upload_last_server_message = latest_failure.server_message;
    summary.upload_last_request_id = latest_failure.request_id;
    summary.upload_job_segments = upload_totals.pending
        + upload_totals.running
        + upload_totals.retry_wait
        + upload_totals.stored
        + upload_totals.verified
        + upload_totals.rejected;
    // Reclaimable = the cloud provably holds byte-identical copies of every
    // ready segment and the session can no longer grow. Deletion stays the
    // user's explicit act through the existing staged path.
    summary.reclaimable = summary.state == "completed"
        && summary.ready_segments > 0
        && summary.upload_verified_segments == summary.ready_segments
        && upload_session_sealed;
    let deletion_pending: bool = connection.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM session_deletions
             WHERE session_id = ?1
               AND state IN ('prepared', 'staged', 'cleanup_pending')
         )",
        params![session_id],
        |row| row.get(0),
    )?;
    let upload_request_rejection_recoverable =
        has_manually_recoverable_upload_request_rejection(connection, session_id)?;
    let performance_validation_only: bool = connection.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM capture_runs
             WHERE session_id = ?1 AND performance_validation_only = 1
         )",
        params![session_id],
        |row| row.get(0),
    )?;
    summary.performance_validation_only = performance_validation_only;
    summary.upload_requestable = summary.state == "completed"
        && summary.ready_segments > 0
        && summary.segment_count == summary.ready_segments
        && !summary.reclaimable
        && !deletion_pending
        && !performance_validation_only
        && (!summary.upload_requested || upload_request_rejection_recoverable);
    Ok(summary)
}

fn has_manually_recoverable_upload_request_rejection(
    connection: &Connection,
    session_id: &str,
) -> anyhow::Result<bool> {
    connection
        .query_row(
            "SELECT EXISTS(
                SELECT 1 FROM upload_jobs
                 WHERE session_id = ?1
                   AND state = 'rejected'
                   AND last_error_code = ?2
                   AND remote_clip_id IS NULL
                   AND clip_revision IS NULL
                   AND remote_upload_intent_id IS NULL
                   AND (
                       (http_status = 409 AND server_code = ?3)
                       OR (http_status = 404 AND server_code = ?4)
                   )
             )",
            params![
                session_id,
                MANUALLY_RECOVERABLE_UPLOAD_ERROR_CODE,
                LEGACY_PROFILE_MISMATCH_SERVER_CODE,
                MISSING_CATALOG_VERSION_SERVER_CODE,
            ],
            |row| row.get(0),
        )
        .context("failed to inspect manually recoverable upload request rejections")
}

fn recover_manually_recoverable_upload_request_rejections(
    tx: &Transaction<'_>,
    session_id: &str,
) -> anyhow::Result<usize> {
    tx.execute(
        "UPDATE upload_jobs
            SET state = 'pending', next_attempt_at_ms = 0,
                last_error_code = NULL, http_status = NULL, server_code = NULL,
                server_message = NULL, request_id = NULL,
                updated_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now')
          WHERE session_id = ?1
            AND state = 'rejected'
            AND last_error_code = ?2
            AND remote_clip_id IS NULL
            AND clip_revision IS NULL
            AND remote_upload_intent_id IS NULL
            AND (
                (http_status = 409 AND server_code = ?3)
                OR (http_status = 404 AND server_code = ?4)
            )",
        params![
            session_id,
            MANUALLY_RECOVERABLE_UPLOAD_ERROR_CODE,
            LEGACY_PROFILE_MISMATCH_SERVER_CODE,
            MISSING_CATALOG_VERSION_SERVER_CODE,
        ],
    )
    .context("failed to recover manually recoverable upload request rejections")
}

fn enqueue_ready_segments_for_upload(tx: &Transaction<'_>, session_id: &str) -> anyhow::Result<()> {
    // The uploader builds its classification envelope from the session's own
    // facts (`non_trainable`/`consent_id` on UploadCandidate) rather than a
    // compile-time constant (docs/49 E7), so the wire label always matches the
    // local label. The v10-era skip existed because the envelope was hardcoded
    // `nonTrainable: true`; that hardcoding is gone. The offline teaser
    // path (manual double-backup) still works — enqueueing does not delete or
    // move local RAW, and the uploader only runs with a device credential.
    tx.execute(
        "INSERT INTO upload_sessions(
            session_id, registration_state, created_at_utc, updated_at_utc
         ) VALUES (?1, 'pending', strftime('%Y-%m-%dT%H:%M:%fZ','now'),
                   strftime('%Y-%m-%dT%H:%M:%fZ','now'))
         ON CONFLICT(session_id) DO NOTHING",
        params![session_id],
    )?;
    tx.execute(
        "INSERT INTO upload_runs(
            run_id, session_id, registration_state, created_at_utc, updated_at_utc
         )
         SELECT DISTINCT g.run_id, g.session_id, 'pending',
                strftime('%Y-%m-%dT%H:%M:%fZ','now'),
                strftime('%Y-%m-%dT%H:%M:%fZ','now')
           FROM segments g
          WHERE g.session_id = ?1 AND g.state = 'ready'
         ON CONFLICT(run_id) DO NOTHING",
        params![session_id],
    )?;
    tx.execute(
        "INSERT INTO upload_jobs(
            segment_id, session_id, run_id, state, next_attempt_at_ms,
            last_error_code, created_at_utc, updated_at_utc
         )
         SELECT g.id, g.session_id, g.run_id,
                CASE WHEN f.candidate_eligible = 0 THEN 'rejected' ELSE 'pending' END,
                0,
                CASE WHEN f.candidate_eligible = 0
                     THEN 'DEPTH_SEGMENT_INELIGIBLE' ELSE NULL END,
                strftime('%Y-%m-%dT%H:%M:%fZ','now'),
                strftime('%Y-%m-%dT%H:%M:%fZ','now')
           FROM segments g
           LEFT JOIN depth_lab_segment_facts f ON f.segment_id = g.id
          WHERE g.session_id = ?1 AND g.state = 'ready'
         ON CONFLICT(segment_id) DO NOTHING",
        params![session_id],
    )?;
    Ok(())
}

fn freeze_upload_registration_states(tx: &Transaction<'_>, segment_id: &str) -> anyhow::Result<()> {
    let (session_id, session_state, run_id, run_state): (String, String, String, String) = tx
        .query_row(
            "SELECT j.session_id, s.state, j.run_id, r.state
               FROM upload_jobs j
               JOIN sessions s ON s.id = j.session_id
               JOIN capture_runs r ON r.id = j.run_id
              WHERE j.segment_id = ?1",
            params![segment_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
    ensure!(
        matches!(
            session_state.as_str(),
            "paused" | "completed" | "faulted" | "corrupt"
        ),
        "upload session is not at a registrable local state"
    );
    ensure!(
        matches!(run_state.as_str(), "paused" | "completed" | "faulted"),
        "upload run is not at a registrable local state"
    );
    tx.execute(
        "UPDATE upload_sessions SET registration_state = ?2,
                updated_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now')
          WHERE session_id = ?1 AND registration_state = 'pending'",
        params![session_id, session_state],
    )?;
    tx.execute(
        "UPDATE upload_runs SET registration_state = ?2,
                updated_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now')
          WHERE run_id = ?1 AND registration_state = 'pending'",
        params![run_id, run_state],
    )?;
    Ok(())
}

fn upload_job_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<UploadJob> {
    let state = row.get::<_, String>(1)?;
    let parsed = UploadJobState::parse(&state).map_err(|_| {
        rusqlite::Error::InvalidColumnType(1, "state".into(), rusqlite::types::Type::Text)
    })?;
    Ok(UploadJob {
        segment_id: row.get(0)?,
        session_id: row.get(4)?,
        run_name: row.get(5)?,
        segment_index: row.get(6)?,
        state: parsed,
        attempt_count: row.get(2)?,
        status_attempt_count: row.get(7)?,
        remote_clip_id: row.get(3)?,
    })
}

fn session_artifact_job_from_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<SessionArtifactUploadJob> {
    use rusqlite::types::Type;

    let kind_text = row.get::<_, String>(1)?;
    let state_text = row.get::<_, String>(7)?;
    let kind = SessionArtifactKind::parse(&kind_text)
        .map_err(|error| rusqlite::Error::FromSqlConversionFailure(1, Type::Text, error.into()))?;
    let state = SessionArtifactUploadState::parse(&state_text)
        .map_err(|error| rusqlite::Error::FromSqlConversionFailure(7, Type::Text, error.into()))?;
    Ok(SessionArtifactUploadJob {
        session_id: row.get(0)?,
        kind,
        local_path: row.get(2)?,
        object_key: row.get(3)?,
        expected_sha256: row.get(4)?,
        expected_size: row.get(5)?,
        content_md5: row.get(6)?,
        state,
        attempts: row.get(8)?,
    })
}

fn load_session_artifact_jobs(
    connection: &Connection,
    session_id: &str,
) -> anyhow::Result<Vec<SessionArtifactUploadJob>> {
    let mut statement = connection.prepare(
        "SELECT session_id, kind, local_path, object_key,
                expected_sha256, expected_size, content_md5, state, attempts
           FROM session_artifact_uploads
          WHERE session_id = ?1
          ORDER BY kind",
    )?;
    statement
        .query_map(params![session_id], session_artifact_job_from_row)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(Into::into)
}

fn load_session_artifact_job(
    connection: &Connection,
    session_id: &str,
    kind: &str,
) -> anyhow::Result<Option<SessionArtifactUploadJob>> {
    connection
        .query_row(
            "SELECT session_id, kind, local_path, object_key,
                    expected_sha256, expected_size, content_md5, state, attempts
               FROM session_artifact_uploads
              WHERE session_id = ?1 AND kind = ?2",
            params![session_id, kind],
            session_artifact_job_from_row,
        )
        .optional()
        .map_err(Into::into)
}

const VIEW_CALIBRATION_COLUMNS: &str = "calibration_id, kind, parent_calibration_id, game_id, \
     recording_profile_hash, device_profile_hash, created_utc, created_qpc, verdict, \
     counts_per_360_measured, deviation_pct, polling_hz_p50, file_path, file_sha256, \
     invalidated_utc, invalidation_reason";

fn view_calibration_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ViewCalibrationAudit> {
    Ok(ViewCalibrationAudit {
        calibration_id: row.get(0)?,
        kind: row.get(1)?,
        parent_calibration_id: row.get(2)?,
        game_id: row.get(3)?,
        recording_profile_hash: row.get(4)?,
        device_profile_hash: row.get(5)?,
        created_utc: row.get(6)?,
        created_qpc: row.get(7)?,
        verdict: row.get(8)?,
        counts_per_360_measured: row.get(9)?,
        deviation_pct: row.get(10)?,
        polling_hz_p50: row.get(11)?,
        file_path: row.get(12)?,
        file_sha256: row.get(13)?,
        invalidated_utc: row.get(14)?,
        invalidation_reason: row.get(15)?,
    })
}

fn load_view_calibration(
    connection: &Connection,
    calibration_id: &str,
) -> anyhow::Result<Option<ViewCalibrationAudit>> {
    connection
        .query_row(
            &format!(
                "SELECT {VIEW_CALIBRATION_COLUMNS} FROM view_calibrations
                  WHERE calibration_id = ?1"
            ),
            params![calibration_id],
            view_calibration_from_row,
        )
        .optional()
        .map_err(Into::into)
}

fn valid_sha256_reference(value: &str) -> bool {
    value.len() == 71
        && value.starts_with("sha256:")
        && value[7..]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn parse_audio_json(value: Option<&str>) -> anyhow::Result<Option<SegmentAudioMetadata>> {
    value
        .map(|json| {
            let audio: SegmentAudioMetadata =
                serde_json::from_str(json).context("invalid persisted segment audio metadata")?;
            audio.validate()?;
            Ok(audio)
        })
        .transpose()
}

fn parse_audio_json_row(
    value: Option<String>,
    column: usize,
) -> rusqlite::Result<Option<SegmentAudioMetadata>> {
    parse_audio_json(value.as_deref()).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(column, rusqlite::types::Type::Text, error.into())
    })
}

fn parse_depth_artifact_row(
    path: Option<String>,
    sha256: Option<String>,
) -> rusqlite::Result<Option<(String, String)>> {
    match (path, sha256) {
        (None, None) => Ok(None),
        (Some(path), Some(sha256)) => Ok(Some((path, sha256))),
        (Some(_), None) => Err(rusqlite::Error::FromSqlConversionFailure(
            73,
            rusqlite::types::Type::Null,
            anyhow::anyhow!("depth artifact path is present but sha256 is missing").into(),
        )),
        (None, Some(_)) => Err(rusqlite::Error::FromSqlConversionFailure(
            72,
            rusqlite::types::Type::Null,
            anyhow::anyhow!("depth artifact sha256 is present but path is missing").into(),
        )),
    }
}

fn parse_depth_upload_artifact_row(
    depth_expected: bool,
    manifest: Option<&(String, String)>,
    path: Option<String>,
    sha256: Option<String>,
) -> rusqlite::Result<Option<(String, String)>> {
    match (manifest, path, sha256) {
        (None, None, None) if !depth_expected => Ok(None),
        (None, None, None) => Err(rusqlite::Error::FromSqlConversionFailure(
            72,
            rusqlite::types::Type::Null,
            anyhow::anyhow!(
                "depth was declared at run start but has no finalized manifest/ZIP; refusing video-only downgrade"
            )
            .into(),
        )),
        (Some(_), Some(path), Some(sha256)) if depth_expected => Ok(Some((path, sha256))),
        (Some(_), Some(_), Some(_)) => Err(rusqlite::Error::FromSqlConversionFailure(
            76,
            rusqlite::types::Type::Integer,
            anyhow::anyhow!("depth ZIP exists for a run that did not declare depth at start").into(),
        )),
        (Some(_), None, None) => Err(rusqlite::Error::FromSqlConversionFailure(
            74,
            rusqlite::types::Type::Null,
            anyhow::anyhow!(
                "depth manifest exists without an eligible ZIP; refusing video-only downgrade"
            )
            .into(),
        )),
        (None, Some(_), Some(_)) => Err(rusqlite::Error::FromSqlConversionFailure(
            72,
            rusqlite::types::Type::Null,
            anyhow::anyhow!("depth ZIP exists without its durable segment manifest").into(),
        )),
        (_, Some(_), None) => Err(rusqlite::Error::FromSqlConversionFailure(
            75,
            rusqlite::types::Type::Null,
            anyhow::anyhow!("depth ZIP path is present but sha256 is missing").into(),
        )),
        (_, None, Some(_)) => Err(rusqlite::Error::FromSqlConversionFailure(
            74,
            rusqlite::types::Type::Null,
            anyhow::anyhow!("depth ZIP sha256 is present but path is missing").into(),
        )),
    }
}

fn load_depth_lab_candidate(
    connection: &Connection,
    segment_id: &str,
) -> anyhow::Result<Option<DepthLabCandidate>> {
    let row = connection
        .query_row(
            "SELECT d.segment_id, d.session_id, r.run_name, d.generation, d.segment_seq,
                    d.state, d.attempt_count, d.manifest_path, d.manifest_sha256,
                    g.depth_path, g.depth_sha256,
                    f.candidate_eligible, f.invalid_frame_count,
                    f.manifest_path, f.manifest_sha256
               FROM depth_lab_jobs d
               JOIN segments g ON g.id = d.segment_id
               JOIN capture_runs r ON r.id = d.run_id
               JOIN depth_lab_segment_facts f ON f.segment_id = d.segment_id
              WHERE d.segment_id = ?1",
            params![segment_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, u64>(3)?,
                    row.get::<_, u32>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, u32>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, String>(8)?,
                    row.get::<_, Option<String>>(9)?,
                    row.get::<_, Option<String>>(10)?,
                    row.get::<_, bool>(11)?,
                    row.get::<_, u32>(12)?,
                    row.get::<_, String>(13)?,
                    row.get::<_, String>(14)?,
                ))
            },
        )
        .optional()?;
    let Some((
        segment_id,
        session_id,
        run_id,
        generation,
        segment_index,
        state,
        attempt_count,
        manifest_relative_path,
        manifest_sha256,
        depth_path,
        depth_sha256,
        candidate_eligible,
        invalid_frame_count,
        fact_manifest_path,
        fact_manifest_sha256,
    )) = row
    else {
        return Ok(None);
    };
    let local_fact = parse_depth_artifact_row(depth_path, depth_sha256)
        .map_err(anyhow::Error::from)?
        .context("depth-lab queue row has no complete local depth fact")?;
    ensure!(
        local_fact.0 == manifest_relative_path && local_fact.1 == manifest_sha256,
        "depth-lab queue/local completion fact mismatch"
    );
    ensure!(
        candidate_eligible
            && invalid_frame_count == 0
            && fact_manifest_path == manifest_relative_path
            && fact_manifest_sha256 == manifest_sha256,
        "depth-lab queue references a non-deliverable or mismatched segment fact"
    );
    ensure!(
        valid_sha256_reference(&manifest_sha256),
        "depth-lab candidate has an invalid manifest SHA-256"
    );
    Ok(Some(DepthLabCandidate {
        segment_id,
        session_id,
        run_id,
        generation,
        segment_index,
        state: DepthLabJobState::parse(&state)?,
        attempt_count,
        manifest_relative_path,
        manifest_sha256,
    }))
}

#[cfg(any(feature = "depth-lab", test))]
fn load_depth_archive_job(
    connection: &Connection,
    job_id: &str,
) -> anyhow::Result<Option<DepthArchiveJob>> {
    let row = connection
        .query_row(
            "SELECT j.job_id, j.session_id, r.run_name, j.generation, j.state,
                    j.attempt_count, j.next_attempt_at_ms,
                    j.lease_owner, j.lease_expires_at_ms,
                    j.materialized_json, j.remote_scope_json, j.progress_json,
                    j.last_error_code, j.last_error_message
               FROM depth_lab_archive_jobs j
               JOIN capture_runs r ON r.id = j.run_id
              WHERE j.job_id = ?1",
            params![job_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, u64>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, u32>(5)?,
                    row.get::<_, u64>(6)?,
                    row.get::<_, Option<String>>(7)?,
                    row.get::<_, Option<u64>>(8)?,
                    row.get::<_, Option<String>>(9)?,
                    row.get::<_, Option<String>>(10)?,
                    row.get::<_, Option<String>>(11)?,
                    row.get::<_, Option<String>>(12)?,
                    row.get::<_, Option<String>>(13)?,
                ))
            },
        )
        .optional()?;
    let Some((
        job_id,
        session_id,
        run_id,
        generation,
        state,
        attempt_count,
        next_attempt_at_ms,
        lease_owner,
        lease_expires_at_ms,
        materialized_json,
        remote_scope_json,
        progress_json,
        last_error_code,
        last_error_message,
    )) = row
    else {
        return Ok(None);
    };
    ensure!(
        lease_owner.is_some() == lease_expires_at_ms.is_some(),
        "depth archive lease columns are one-sided"
    );
    for (label, value) in [
        ("materialized archive", materialized_json.as_deref()),
        ("remote scope", remote_scope_json.as_deref()),
        ("upload progress", progress_json.as_deref()),
    ] {
        if let Some(value) = value {
            validate_depth_json(label, value)?;
        }
    }
    Ok(Some(DepthArchiveJob {
        job_id,
        session_id,
        run_id,
        generation,
        state: DepthArchiveJobState::parse(&state)?,
        attempt_count,
        next_attempt_at_ms,
        lease_owner,
        lease_expires_at_ms,
        materialized_json,
        remote_scope_json,
        progress_json,
        last_error_code,
        last_error_message,
    }))
}

#[cfg(any(feature = "depth-lab", test))]
fn load_depth_archive_candidates(
    connection: &Connection,
    job_id: &str,
) -> anyhow::Result<Vec<DepthLabCandidate>> {
    let mut statement = connection.prepare(
        "SELECT segment_id FROM depth_lab_jobs
          WHERE archive_job_id = ?1 ORDER BY segment_seq, segment_id",
    )?;
    let ids = statement
        .query_map(params![job_id], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    ids.iter()
        .map(|segment_id| {
            load_depth_lab_candidate(connection, segment_id)?
                .with_context(|| format!("depth archive candidate {segment_id} disappeared"))
        })
        .collect()
}

#[cfg(any(feature = "depth-lab", test))]
fn ensure_depth_lease_owner(value: &str) -> anyhow::Result<()> {
    ensure!(
        !value.trim().is_empty()
            && value.len() <= 160
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric()
                    || matches!(byte, b'.' | b'_' | b':' | b'-')),
        "invalid depth archive lease owner"
    );
    Ok(())
}

#[cfg(any(feature = "depth-lab", test))]
fn validate_depth_json(label: &str, value: &str) -> anyhow::Result<()> {
    serde_json::from_str::<serde_json::Value>(value)
        .with_context(|| format!("{label} is not valid JSON"))?;
    Ok(())
}

#[cfg(any(feature = "depth-lab", test))]
struct DepthArchiveJsonUpdate<'a> {
    job_id: &'a str,
    lease_owner: &'a str,
    now_ms: u64,
    state: DepthArchiveJobState,
    field: &'a str,
    value: &'a str,
    extra: Option<(&'a str, &'a str)>,
}

#[cfg(any(feature = "depth-lab", test))]
fn update_depth_archive_job_json(
    connection: &Mutex<Connection>,
    update: &DepthArchiveJsonUpdate<'_>,
) -> anyhow::Result<()> {
    ensure_depth_lease_owner(update.lease_owner)?;
    let sql = match (update.field, update.extra.map(|(field, _)| field)) {
        ("materialized_json", Some("progress_json")) => {
            "UPDATE depth_lab_archive_jobs
                SET state = ?4,
                    materialized_json = COALESCE(materialized_json, ?5),
                    progress_json = COALESCE(progress_json, ?6),
                    updated_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now')
              WHERE job_id = ?1 AND lease_owner = ?2
                AND lease_expires_at_ms > ?3 AND state <> 'finalized'
                AND (materialized_json IS NULL OR materialized_json = ?5)
                AND (progress_json IS NULL OR progress_json = ?6)"
        }
        ("remote_scope_json", None) => {
            "UPDATE depth_lab_archive_jobs
                SET state = ?4,
                    remote_scope_json = COALESCE(remote_scope_json, ?5),
                    updated_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now')
              WHERE job_id = ?1 AND lease_owner = ?2
                AND lease_expires_at_ms > ?3 AND state <> 'finalized'
                AND materialized_json IS NOT NULL
                AND (remote_scope_json IS NULL OR remote_scope_json = ?5)"
        }
        ("progress_json", None) => {
            "UPDATE depth_lab_archive_jobs
                SET state = ?4, progress_json = ?5,
                    updated_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now')
              WHERE job_id = ?1 AND lease_owner = ?2
                AND lease_expires_at_ms > ?3 AND state <> 'finalized'
                AND materialized_json IS NOT NULL
                AND remote_scope_json IS NOT NULL"
        }
        _ => anyhow::bail!("unsupported depth archive JSON field transition"),
    };
    let now = u64_to_i64(update.now_ms, "depth archive update current time")?;
    let changed = if let Some((_, extra_value)) = update.extra {
        connection.lock().execute(
            sql,
            params![
                update.job_id,
                update.lease_owner,
                now,
                update.state.as_str(),
                update.value,
                extra_value
            ],
        )?
    } else {
        connection.lock().execute(
            sql,
            params![
                update.job_id,
                update.lease_owner,
                now,
                update.state.as_str(),
                update.value
            ],
        )?
    };
    ensure!(
        changed == 1,
        "depth archive JSON update lost its lease or immutable state"
    );
    Ok(())
}

fn validate_session_artifact_sources(
    session_id: &str,
    sources: &[SessionArtifactSource],
) -> anyhow::Result<()> {
    ensure!(
        sources.len() >= SessionArtifactKind::REQUIRED.len()
            && sources.len() <= SessionArtifactKind::REQUIRED.len() + 2,
        "session artifact set must contain seven required files plus optional \
         markers and input calibration"
    );
    let mut seen = HashMap::new();
    for source in sources {
        ensure!(
            seen.insert(source.kind, ()).is_none(),
            "duplicate session artifact {}",
            source.kind.as_str()
        );
        ensure!(
            source.expected_size > 0,
            "session artifact {} is empty",
            source.kind.as_str()
        );
        ensure!(
            source.expected_sha256.len() == 71
                && source.expected_sha256.starts_with("sha256:")
                && source.expected_sha256[7..]
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
            "session artifact {} has an invalid SHA-256",
            source.kind.as_str()
        );
        ensure!(
            source.content_md5.len() == 24,
            "session artifact {} has an invalid Content-MD5",
            source.kind.as_str()
        );
        let path = Path::new(&source.relative_path);
        ensure!(
            !path.is_absolute()
                && path
                    .components()
                    .all(|component| matches!(component, Component::Normal(_))),
            "session artifact {} has an unsafe local path",
            source.kind.as_str()
        );
        let expected = Path::new(session_id)
            .join("session")
            .join(source.kind.file_name());
        ensure!(
            path == expected,
            "session artifact {} is outside its immutable session bundle",
            source.kind.as_str()
        );
    }
    for required in SessionArtifactKind::REQUIRED {
        ensure!(
            seen.contains_key(&required),
            "session artifact set is missing {}",
            required.as_str()
        );
    }
    ensure!(
        seen.keys()
            .all(|kind| SessionArtifactKind::REQUIRED.contains(kind)
                || matches!(
                    kind,
                    SessionArtifactKind::Markers | SessionArtifactKind::InputCalibration
                )),
        "session artifact set contains an unsupported kind"
    );
    Ok(())
}

fn load_upload_job(connection: &Connection, segment_id: &str) -> anyhow::Result<Option<UploadJob>> {
    connection
        .query_row(
            "SELECT j.segment_id, j.state, j.attempt_count, j.remote_clip_id,
                    j.session_id, g.run_name, g.seq, j.status_attempt_count
               FROM upload_jobs j
               JOIN segments g ON g.id = j.segment_id
              WHERE j.segment_id = ?1",
            params![segment_id],
            upload_job_from_row,
        )
        .optional()
        .map_err(Into::into)
}

fn set_once(
    mutex: &Mutex<Connection>,
    table: &str,
    key_column: &str,
    key: &str,
    value_column: &str,
    value: &str,
) -> anyhow::Result<()> {
    ensure!(!value.trim().is_empty(), "remote identity is empty");
    let sql = format!(
        "UPDATE {table}
            SET {value_column} = COALESCE({value_column}, ?2),
                updated_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now')
          WHERE {key_column} = ?1
            AND ({value_column} IS NULL OR {value_column} = ?2)"
    );
    let connection = mutex.lock();
    let changed = connection.execute(&sql, params![key, value])?;
    ensure!(
        changed == 1,
        "remote identity conflicts with the durable upload journal"
    );
    Ok(())
}

fn update_upload_job_state(
    mutex: &Mutex<Connection>,
    segment_id: &str,
    target: UploadJobState,
    delay_ms: u64,
    error_code: Option<&str>,
) -> anyhow::Result<()> {
    let connection = mutex.lock();
    let current = load_upload_job(&connection, segment_id)?.context("upload job is missing")?;
    let allowed = match target {
        UploadJobState::Stored => matches!(
            current.state,
            UploadJobState::Running | UploadJobState::Stored
        ),
        UploadJobState::Verified | UploadJobState::Rejected => {
            current.state == UploadJobState::Stored
        }
        _ => false,
    };
    ensure!(
        allowed,
        "illegal upload job transition {} -> {}",
        current.state.as_str(),
        target.as_str()
    );
    connection.execute(
        "UPDATE upload_jobs
            SET state = ?2, last_error_code = ?3,
                http_status = NULL, server_code = NULL,
                server_message = NULL, request_id = NULL,
                next_attempt_at_ms = CAST(unixepoch('subsec') * 1000 AS INTEGER) + ?4,
                updated_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now')
          WHERE segment_id = ?1",
        params![
            segment_id,
            target.as_str(),
            error_code,
            u64_to_i64(delay_ms, "upload status delay")?,
        ],
    )?;
    Ok(())
}

fn transition_session_artifact_upload(
    connection: &Mutex<Connection>,
    session_id: &str,
    kind: SessionArtifactKind,
    target: SessionArtifactUploadState,
    error_code: Option<&str>,
) -> anyhow::Result<()> {
    if let Some(error_code) = error_code {
        ensure_upload_error_code(error_code)?;
    }
    let connection = connection.lock();
    let current = load_session_artifact_job(&connection, session_id, kind.as_str())?
        .with_context(|| format!("session artifact {session_id}/{} is missing", kind.as_str()))?;
    let allowed = match target {
        SessionArtifactUploadState::Signed => matches!(
            current.state,
            SessionArtifactUploadState::Declared | SessionArtifactUploadState::Signed
        ),
        SessionArtifactUploadState::Put => matches!(
            current.state,
            SessionArtifactUploadState::Signed | SessionArtifactUploadState::Put
        ),
        SessionArtifactUploadState::Completing => matches!(
            current.state,
            SessionArtifactUploadState::Put | SessionArtifactUploadState::Completing
        ),
        SessionArtifactUploadState::Verified => {
            current.state == SessionArtifactUploadState::Completing
        }
        SessionArtifactUploadState::Rejected => !matches!(
            current.state,
            SessionArtifactUploadState::Verified | SessionArtifactUploadState::Rejected
        ),
        SessionArtifactUploadState::Pending | SessionArtifactUploadState::Declared => false,
    };
    ensure!(
        allowed,
        "illegal session artifact transition {} -> {}",
        current.state.as_str(),
        target.as_str()
    );
    let changed = if target == SessionArtifactUploadState::Verified {
        connection.execute(
            "UPDATE session_artifact_uploads
                SET state = ?3, next_attempt_at = 0, last_error_code = NULL,
                    verified_at = strftime('%Y-%m-%dT%H:%M:%fZ','now'),
                    updated_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now')
              WHERE session_id = ?1 AND kind = ?2",
            params![session_id, kind.as_str(), target.as_str()],
        )?
    } else {
        connection.execute(
            "UPDATE session_artifact_uploads
                SET state = ?3, next_attempt_at = 0, last_error_code = ?4,
                    verified_at = NULL,
                    updated_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now')
              WHERE session_id = ?1 AND kind = ?2",
            params![session_id, kind.as_str(), target.as_str(), error_code],
        )?
    };
    ensure!(
        changed == 1,
        "session artifact transition lost its durable row"
    );
    Ok(())
}

fn ensure_upload_error_code(error_code: &str) -> anyhow::Result<()> {
    ensure!(
        (1..=80).contains(&error_code.len())
            && error_code
                .bytes()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_'),
        "upload error code must be stable uppercase ASCII"
    );
    Ok(())
}

fn migrate(connection: &Connection) -> anyhow::Result<()> {
    migrate_to(connection, JOURNAL_SCHEMA_VERSION)
}

/// 把库迁到第 `target` 版**为止**，中途停下。
///
/// 生产只有 [`migrate`] 一个调用方，它传 [`JOURNAL_SCHEMA_VERSION`]，于是下面每一个
/// `target > N` / `target >= N` 都恒真——整条链与没有上限时**逐字等价**。上限只服务
/// 于测试：造一个**真的**第 N 版库，比「拿今天的库把 `journal_meta.schema_version`
/// 回拨到 N」可靠得多。回拨这一招在「迁移只加列」的年代成立，v16 删列改名之后不
/// 成立：重放时每一张被重建过的表都会拿今天的形状去满足当年的列清单，而现场根本
/// 不存在这种状态（现场的版本号只会往前走）。见
/// `version_five_journal_migrates_data_model_job_schema_idempotently`。
///
/// `target` 必须是这条链真的会停下的版本号。链上每一版都在（1..=17），但要注意
/// `version < N` 那几步是**跨版本兜底**的：一个停在 12 的库传 target = 13 会真的
/// 停在 13，传 14 则由 `version < 14` 那一步一次推到 14。传一个链上不产生的号会以
/// 「migration stopped at version N (target M)」失败，而不是悄悄停在别处。
fn migrate_to(connection: &Connection, target: i64) -> anyhow::Result<()> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS journal_meta (
            singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
            schema_version INTEGER NOT NULL
         );

         CREATE TABLE IF NOT EXISTS sessions (
            id TEXT PRIMARY KEY,
            state TEXT NOT NULL CHECK(state IN ('active', 'paused', 'completed', 'faulted', 'corrupt')),
            non_trainable INTEGER NOT NULL CHECK(non_trainable = 1),
            production_certified INTEGER NOT NULL CHECK(production_certified = 0),
            annotator_id TEXT,
            device_id TEXT,
            consent_id TEXT,
            game_id TEXT NOT NULL,
            -- 这张 CREATE TABLE 是 **v1 历史形状**，不是当前形状：新库先按它建表、
            -- 把 schema_version 置 1，再顺序跑全部迁移。因此这里**不能**加新列
            -- ——中途的 v10 重建用的是 `INSERT INTO sessions_v10 SELECT *`，多一列
            -- 就会以 35 columns but 36 values 失败。指纹放宽为可空、以及
            -- game_exe_hash_missing_reason，都由 v13→v14 迁移完成。
            game_exe_hash TEXT NOT NULL,
            capture_profile_id TEXT NOT NULL,
            capture_profile_hash TEXT NOT NULL,
            settings_profile_id TEXT NOT NULL,
            mapping_id TEXT NOT NULL,
            mapping_version TEXT NOT NULL,
            mapping_hash TEXT NOT NULL,
            schema_version TEXT NOT NULL,
            started_at_utc TEXT NOT NULL,
            ended_at_utc TEXT,
            stop_reason TEXT,
            last_error_code TEXT,
            created_at_utc TEXT NOT NULL,
            updated_at_utc TEXT NOT NULL
         );

         CREATE TABLE IF NOT EXISTS capture_runs (
            id TEXT PRIMARY KEY,
            session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
            run_name TEXT NOT NULL,
            seq INTEGER NOT NULL CHECK(seq >= 0),
            state TEXT NOT NULL CHECK(state IN ('starting', 'recording', 'paused', 'completed', 'faulted')),
            start_request_id TEXT NOT NULL UNIQUE,
            backend TEXT NOT NULL CHECK(backend = 'native_wgc_window_v1'),
            target_pid INTEGER NOT NULL CHECK(target_pid > 0),
            target_window_id TEXT NOT NULL,
            encoder_backend TEXT NOT NULL CHECK(encoder_backend = 'mf_nvidia_hardware_v1'),
            fps_num INTEGER NOT NULL CHECK(fps_num > 0),
            fps_den INTEGER NOT NULL CHECK(fps_den > 0),
            width INTEGER NOT NULL CHECK(width >= 1920),
            height INTEGER NOT NULL CHECK(height >= 1080),
            qpc_frequency INTEGER,
            anchor_qpc INTEGER,
            session_frame_start INTEGER NOT NULL CHECK(session_frame_start >= 0),
            session_frame_end INTEGER,
            frame_count INTEGER,
            ready_segments INTEGER,
            duplicate_frames INTEGER,
            source_superseded INTEGER,
            input_event_count INTEGER,
            input_queue_overflows INTEGER,
            qpc_start INTEGER,
            qpc_end INTEGER,
            stop_reason TEXT,
            error_code TEXT,
            started_at_utc TEXT,
            ended_at_utc TEXT,
            created_at_utc TEXT NOT NULL,
            updated_at_utc TEXT NOT NULL,
            CHECK(fps_num >= 30 * fps_den),
            UNIQUE(session_id, seq),
            UNIQUE(session_id, run_name)
         );

         CREATE TABLE IF NOT EXISTS segments (
            id TEXT PRIMARY KEY,
            session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
            run_id TEXT NOT NULL REFERENCES capture_runs(id) ON DELETE CASCADE,
            run_name TEXT NOT NULL,
            seq INTEGER NOT NULL CHECK(seq >= 0),
            state TEXT NOT NULL CHECK(state IN ('active', 'finalizing', 'ready', 'aborted', 'failed', 'corrupt', 'quarantined')),
            session_frame_start INTEGER NOT NULL CHECK(session_frame_start >= 0),
            n_frames INTEGER,
            qpc_start INTEGER,
            qpc_end INTEGER,
            video_path TEXT,
            frames_path TEXT,
            events_path TEXT,
            meta_path TEXT,
            manifest_path TEXT,
            video_sha256 TEXT,
            frames_sha256 TEXT,
            events_sha256 TEXT,
            meta_sha256 TEXT,
            bytes INTEGER,
            event_count INTEGER,
            duplicate_frames INTEGER,
            source_superseded INTEGER,
            source_drops INTEGER,
            error_code TEXT,
            ready_at_utc TEXT,
            created_at_utc TEXT NOT NULL,
            updated_at_utc TEXT NOT NULL,
            UNIQUE(run_id, seq)
         );

         CREATE INDEX IF NOT EXISTS idx_capture_runs_session ON capture_runs(session_id, seq);
         CREATE INDEX IF NOT EXISTS idx_segments_session ON segments(session_id, run_id, seq);
         CREATE INDEX IF NOT EXISTS idx_segments_state ON segments(state);

         INSERT INTO journal_meta(singleton, schema_version) VALUES (1, 1)
         ON CONFLICT(singleton) DO NOTHING;",
    )?;
    let mut version: i64 = connection.query_row(
        "SELECT schema_version FROM journal_meta WHERE singleton = 1",
        [],
        |row| row.get(0),
    )?;
    ensure!(
        (1..=JOURNAL_SCHEMA_VERSION).contains(&version),
        "unsupported SQLite journal schema version {version}"
    );
    ensure!(
        (1..=JOURNAL_SCHEMA_VERSION).contains(&target),
        "unsupported SQLite journal migration target {target}"
    );
    if version == 1 && target > 1 {
        connection.execute_batch(
            "BEGIN IMMEDIATE;
             CREATE TABLE IF NOT EXISTS recovery_runs (
                id TEXT PRIMARY KEY,
                state TEXT NOT NULL CHECK(state IN ('completed', 'faulted')),
                scanned_sessions INTEGER NOT NULL CHECK(scanned_sessions >= 0),
                verified_ready_segments INTEGER NOT NULL CHECK(verified_ready_segments >= 0),
                quarantined_items INTEGER NOT NULL CHECK(quarantined_items >= 0),
                corrupt_segments INTEGER NOT NULL CHECK(corrupt_segments >= 0),
                orphan_items INTEGER NOT NULL CHECK(orphan_items >= 0),
                report_path TEXT NOT NULL,
                error_code TEXT,
                started_at_utc TEXT NOT NULL,
                completed_at_utc TEXT
             );
             CREATE TABLE IF NOT EXISTS recovery_findings (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                recovery_id TEXT NOT NULL REFERENCES recovery_runs(id) ON DELETE CASCADE,
                severity TEXT NOT NULL CHECK(severity IN ('info', 'warning', 'error')),
                code TEXT NOT NULL,
                action TEXT NOT NULL,
                session_id TEXT,
                run_name TEXT,
                segment_seq INTEGER,
                relative_path TEXT,
                message TEXT NOT NULL,
                created_at_utc TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_recovery_findings_run ON recovery_findings(recovery_id, id);
             UPDATE journal_meta SET schema_version = 2 WHERE singleton = 1;
             COMMIT;",
        )?;
        version = 2;
    }
    if version == 2 && target > 2 {
        connection.execute_batch(
            "BEGIN IMMEDIATE;
             CREATE TABLE IF NOT EXISTS session_deletions (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                state TEXT NOT NULL CHECK(state IN (
                    'prepared', 'staged', 'cleanup_pending', 'completed', 'cancelled'
                )),
                staged_relative_path TEXT NOT NULL,
                error_code TEXT,
                requested_at_utc TEXT NOT NULL,
                completed_at_utc TEXT,
                updated_at_utc TEXT NOT NULL
             );
             CREATE UNIQUE INDEX IF NOT EXISTS idx_session_deletions_pending
                 ON session_deletions(session_id)
                 WHERE state IN ('prepared', 'staged', 'cleanup_pending');
             UPDATE journal_meta SET schema_version = 3 WHERE singleton = 1;
             COMMIT;",
        )?;
        version = 3;
    }
    if version == 3 && target > 3 {
        connection.execute_batch(
            "BEGIN IMMEDIATE;
             CREATE TABLE IF NOT EXISTS markers (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                run_id TEXT NOT NULL REFERENCES capture_runs(id) ON DELETE CASCADE,
                run_name TEXT NOT NULL,
                seq INTEGER NOT NULL CHECK(seq >= 0),
                request_id TEXT NOT NULL UNIQUE,
                kind TEXT NOT NULL CHECK(kind IN ('manual')),
                qpc INTEGER NOT NULL CHECK(qpc >= 0),
                qpc_frequency INTEGER NOT NULL CHECK(qpc_frequency > 0),
                run_frame INTEGER NOT NULL CHECK(run_frame >= 0),
                session_frame INTEGER NOT NULL CHECK(session_frame >= 0),
                video_time_us INTEGER NOT NULL CHECK(video_time_us >= 0),
                alignment_state TEXT NOT NULL CHECK(alignment_state IN ('pending', 'aligned', 'outside_video')),
                segment_seq INTEGER,
                non_trainable INTEGER NOT NULL CHECK(non_trainable = 1),
                production_certified INTEGER NOT NULL CHECK(production_certified = 0),
                created_at_utc TEXT NOT NULL,
                UNIQUE(session_id, seq),
                CHECK((alignment_state = 'aligned' AND segment_seq IS NOT NULL)
                   OR (alignment_state != 'aligned' AND segment_seq IS NULL))
             );
             CREATE INDEX IF NOT EXISTS idx_markers_session ON markers(session_id, seq);
             CREATE INDEX IF NOT EXISTS idx_markers_run_qpc ON markers(run_id, qpc);
             UPDATE journal_meta SET schema_version = 4 WHERE singleton = 1;
             COMMIT;",
        )?;
        version = 4;
    }
    if version == 4 && target > 4 {
        connection.execute_batch(
            "BEGIN IMMEDIATE;
             CREATE TABLE IF NOT EXISTS preflight_attempts (
                id TEXT PRIMARY KEY,
                request_id TEXT NOT NULL UNIQUE,
                state TEXT NOT NULL CHECK(state IN ('running', 'dev_non_trainable_pass', 'blocked', 'failed')),
                phase TEXT NOT NULL,
                catalog_mode TEXT NOT NULL CHECK(catalog_mode IN ('certified', 'dev_non_trainable')),
                target_window_id TEXT NOT NULL,
                game_id TEXT NOT NULL,
                catalog_id TEXT NOT NULL,
                catalog_version TEXT NOT NULL,
                catalog_hash TEXT NOT NULL,
                device_profile_id TEXT NOT NULL,
                device_profile_version TEXT NOT NULL,
                device_profile_hash TEXT NOT NULL,
                capture_profile_id TEXT NOT NULL,
                capture_profile_version TEXT NOT NULL,
                capture_profile_hash TEXT NOT NULL,
                game_profile_version TEXT NOT NULL,
                game_profile_hash TEXT NOT NULL,
                settings_profile_id TEXT NOT NULL,
                settings_profile_version TEXT NOT NULL,
                settings_profile_hash TEXT NOT NULL,
                mapping_id TEXT NOT NULL,
                mapping_version TEXT NOT NULL,
                mapping_hash TEXT NOT NULL,
                session_id TEXT,
                run_name TEXT,
                failure_code TEXT,
                details_id TEXT,
                evidence_json TEXT NOT NULL,
                evidence_hash TEXT NOT NULL,
                created_at_utc TEXT NOT NULL,
                completed_at_utc TEXT,
                updated_at_utc TEXT NOT NULL,
                CHECK((session_id IS NULL AND run_name IS NULL)
                   OR (session_id IS NOT NULL AND run_name IS NOT NULL)),
                CHECK((state IN ('blocked', 'failed') AND failure_code IS NOT NULL AND details_id IS NOT NULL)
                   OR (state NOT IN ('blocked', 'failed') AND failure_code IS NULL AND details_id IS NULL))
             );
             CREATE INDEX IF NOT EXISTS idx_preflight_created
                 ON preflight_attempts(created_at_utc DESC, id DESC);
             CREATE INDEX IF NOT EXISTS idx_preflight_session
                 ON preflight_attempts(session_id, run_name);
             ALTER TABLE sessions ADD COLUMN catalog_mode TEXT NOT NULL DEFAULT 'legacy_dev_non_trainable';
             ALTER TABLE sessions ADD COLUMN catalog_id TEXT NOT NULL DEFAULT 'legacy-unversioned';
             ALTER TABLE sessions ADD COLUMN catalog_version TEXT NOT NULL DEFAULT 'legacy';
             ALTER TABLE sessions ADD COLUMN catalog_hash TEXT NOT NULL DEFAULT 'legacy-unavailable';
             ALTER TABLE sessions ADD COLUMN device_profile_id TEXT NOT NULL DEFAULT 'legacy-unavailable';
             ALTER TABLE sessions ADD COLUMN device_profile_version TEXT NOT NULL DEFAULT 'legacy';
             ALTER TABLE sessions ADD COLUMN device_profile_hash TEXT NOT NULL DEFAULT 'legacy-unavailable';
             ALTER TABLE sessions ADD COLUMN capture_profile_version TEXT NOT NULL DEFAULT 'legacy';
             ALTER TABLE sessions ADD COLUMN game_profile_version TEXT NOT NULL DEFAULT 'legacy';
             ALTER TABLE sessions ADD COLUMN game_profile_hash TEXT NOT NULL DEFAULT 'legacy-unavailable';
              ALTER TABLE sessions ADD COLUMN settings_profile_version TEXT NOT NULL DEFAULT 'legacy';
              ALTER TABLE sessions ADD COLUMN settings_profile_hash TEXT NOT NULL DEFAULT 'legacy-unavailable';
              ALTER TABLE sessions ADD COLUMN capture_profile_hash_kind TEXT NOT NULL DEFAULT 'legacy_reencoded_json_sha256';
              ALTER TABLE capture_runs ADD COLUMN preflight_id TEXT;
             UPDATE journal_meta SET schema_version = 5 WHERE singleton = 1;
             COMMIT;",
        )?;
        version = 5;
    }
    if version == 5 && target > 5 {
        connection.execute_batch(
            "BEGIN IMMEDIATE;
             CREATE TABLE IF NOT EXISTS data_model_jobs (
                session_id TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
                state TEXT NOT NULL CHECK(state IN (
                    'waiting', 'pending', 'running', 'finalizing',
                    'completed', 'failed', 'cancelled'
                )),
                ready_segments INTEGER NOT NULL CHECK(ready_segments >= 0),
                processed_segments INTEGER NOT NULL CHECK(processed_segments >= 0),
                published_chunks INTEGER NOT NULL CHECK(published_chunks >= 0),
                published_frames INTEGER NOT NULL CHECK(published_frames >= 0),
                pending_frames INTEGER NOT NULL CHECK(pending_frames >= 0),
                finalize_requested INTEGER NOT NULL CHECK(finalize_requested IN (0, 1)),
                output_relative_path TEXT,
                error_code TEXT,
                created_at_utc TEXT NOT NULL,
                updated_at_utc TEXT NOT NULL,
                CHECK(processed_segments <= ready_segments),
                CHECK((state = 'completed' AND finalize_requested = 1
                       AND output_relative_path IS NOT NULL)
                   OR state != 'completed')
             );
             CREATE INDEX IF NOT EXISTS idx_data_model_jobs_processable
                 ON data_model_jobs(state, finalize_requested, updated_at_utc);
             INSERT INTO data_model_jobs (
                session_id, state, ready_segments, processed_segments,
                published_chunks, published_frames, pending_frames,
                finalize_requested, created_at_utc, updated_at_utc
             )
             SELECT s.id,
                    CASE
                        WHEN s.state = 'completed'
                          OR EXISTS (
                            SELECT 1 FROM segments g
                            WHERE g.session_id = s.id AND g.state = 'ready'
                          ) THEN 'pending'
                        ELSE 'waiting'
                    END,
                    (SELECT COUNT(*) FROM segments g
                     WHERE g.session_id = s.id AND g.state = 'ready'),
                    0, 0, 0, 0,
                    CASE WHEN s.state = 'completed' THEN 1 ELSE 0 END,
                    strftime('%Y-%m-%dT%H:%M:%fZ','now'),
                    strftime('%Y-%m-%dT%H:%M:%fZ','now')
             FROM sessions s
             WHERE 1
             ON CONFLICT(session_id) DO NOTHING;
             UPDATE journal_meta SET schema_version = 6 WHERE singleton = 1;
             COMMIT;",
        )?;
        version = 6;
    }
    if version == 6 && target > 6 {
        // 单向观测镜像的本地出口（docs/13 §5.1）。两张表现在是空的：
        // 采集链路还没有往里写，投递 task 也还不存在。先落 schema，
        // 是为了让后续接入只改调用点，而不是在接入的同一个 PR 里既动迁移
        // 又动 ready 提交事务。
        //
        // seq 用 AUTOINCREMENT：删除已确认的行之后也绝不复用序号。
        // 复用会让云端把两条不同事件当成同一条幂等重放，静默丢掉后一条。
        connection.execute_batch(
            "BEGIN IMMEDIATE;
             CREATE TABLE IF NOT EXISTS mirror_outbox (
                seq INTEGER PRIMARY KEY AUTOINCREMENT,
                entity TEXT NOT NULL CHECK(entity IN (
                    'session', 'run', 'segment', 'marker', 'preflight', 'fault', 'heartbeat'
                )),
                entity_id TEXT NOT NULL CHECK(length(entity_id) BETWEEN 1 AND 128),
                payload_json TEXT NOT NULL CHECK(json_valid(payload_json)),
                observed_at_ms INTEGER NOT NULL CHECK(observed_at_ms >= 0),
                created_at_utc TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_mirror_outbox_entity
                 ON mirror_outbox(entity, entity_id);
             CREATE TABLE IF NOT EXISTS mirror_cursor (
                singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
                acked_seq INTEGER NOT NULL DEFAULT 0 CHECK(acked_seq >= 0),
                attempt_count INTEGER NOT NULL DEFAULT 0 CHECK(attempt_count >= 0),
                last_error TEXT,
                last_attempt_at_utc TEXT,
                updated_at_utc TEXT NOT NULL
             );
             INSERT INTO mirror_cursor(singleton, acked_seq, updated_at_utc)
             VALUES (1, 0, strftime('%Y-%m-%dT%H:%M:%fZ','now'))
             ON CONFLICT(singleton) DO NOTHING;
             UPDATE journal_meta SET schema_version = 7 WHERE singleton = 1;
             COMMIT;",
        )?;
        version = 7;
    }
    if version == 7 && target > 7 {
        connection.execute_batch(
            "BEGIN IMMEDIATE;
             CREATE TABLE IF NOT EXISTS heartbeats (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                run_id TEXT NOT NULL REFERENCES capture_runs(id) ON DELETE CASCADE,
                run_name TEXT NOT NULL,
                seq INTEGER NOT NULL CHECK(seq >= 0),
                qpc INTEGER NOT NULL CHECK(qpc >= 0),
                qpc_frequency INTEGER NOT NULL CHECK(qpc_frequency > 0),
                collector TEXT NOT NULL CHECK(collector = 'ok'),
                created_at_utc TEXT NOT NULL,
                UNIQUE(session_id, seq),
                UNIQUE(run_id, qpc)
             );
             CREATE INDEX IF NOT EXISTS idx_heartbeats_session
                 ON heartbeats(session_id, seq);
             CREATE INDEX IF NOT EXISTS idx_heartbeats_run_qpc
                 ON heartbeats(run_id, qpc);
             UPDATE journal_meta SET schema_version = 8 WHERE singleton = 1;
             COMMIT;",
        )?;
        version = 8;
    }
    if version == 8 && target > 8 {
        connection.execute_batch(
            "BEGIN IMMEDIATE;
             CREATE TABLE IF NOT EXISTS upload_sessions (
                session_id TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
                registration_state TEXT NOT NULL CHECK(registration_state IN (
                    'pending', 'paused', 'completed', 'faulted', 'corrupt'
                )),
                remote_session_id TEXT,
                sealed INTEGER NOT NULL DEFAULT 0 CHECK(sealed IN (0, 1)),
                created_at_utc TEXT NOT NULL,
                updated_at_utc TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS upload_runs (
                run_id TEXT PRIMARY KEY REFERENCES capture_runs(id) ON DELETE CASCADE,
                session_id TEXT NOT NULL REFERENCES upload_sessions(session_id) ON DELETE CASCADE,
                registration_state TEXT NOT NULL CHECK(registration_state IN (
                    'pending', 'paused', 'completed', 'faulted'
                )),
                remote_run_id TEXT,
                created_at_utc TEXT NOT NULL,
                updated_at_utc TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS upload_jobs (
                segment_id TEXT PRIMARY KEY REFERENCES segments(id) ON DELETE CASCADE,
                session_id TEXT NOT NULL REFERENCES upload_sessions(session_id) ON DELETE CASCADE,
                run_id TEXT NOT NULL REFERENCES upload_runs(run_id) ON DELETE CASCADE,
                state TEXT NOT NULL CHECK(state IN (
                    'pending', 'running', 'retry_wait', 'stored', 'verified', 'rejected'
                )),
                attempt_count INTEGER NOT NULL DEFAULT 0 CHECK(attempt_count >= 0),
                next_attempt_at_ms INTEGER NOT NULL DEFAULT 0 CHECK(next_attempt_at_ms >= 0),
                last_error_code TEXT,
                remote_clip_id TEXT,
                clip_revision INTEGER CHECK(clip_revision IS NULL OR clip_revision > 0),
                remote_upload_intent_id TEXT,
                created_at_utc TEXT NOT NULL,
                updated_at_utc TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_upload_jobs_processable
                 ON upload_jobs(state, next_attempt_at_ms, created_at_utc);
             CREATE TABLE IF NOT EXISTS upload_parts (
                segment_id TEXT NOT NULL REFERENCES upload_jobs(segment_id) ON DELETE CASCADE,
                artifact_kind TEXT NOT NULL CHECK(artifact_kind IN (
                    'video', 'frames', 'events', 'meta', 'manifest'
                )),
                part_number INTEGER NOT NULL CHECK(part_number BETWEEN 1 AND 10000),
                byte_offset INTEGER NOT NULL CHECK(byte_offset >= 0),
                byte_size INTEGER NOT NULL CHECK(byte_size > 0),
                content_md5 TEXT NOT NULL,
                etag TEXT NOT NULL,
                uploaded_at_utc TEXT NOT NULL,
                PRIMARY KEY(segment_id, artifact_kind, part_number)
             );
             INSERT INTO upload_sessions(
                session_id, registration_state, created_at_utc, updated_at_utc
             )
             SELECT DISTINCT ranked.session_id, 'pending',
                    strftime('%Y-%m-%dT%H:%M:%fZ','now'),
                    strftime('%Y-%m-%dT%H:%M:%fZ','now')
             FROM (
                SELECT g.id AS segment_id, g.session_id, g.run_id,
                       ROW_NUMBER() OVER (PARTITION BY g.session_id ORDER BY r.seq, g.seq) AS ordinal
                FROM segments g
                JOIN capture_runs r ON r.id = g.run_id
                WHERE g.state = 'ready'
             ) ranked
             JOIN data_model_jobs d ON d.session_id = ranked.session_id
             WHERE ranked.ordinal <= d.processed_segments
             ON CONFLICT(session_id) DO NOTHING;
             INSERT INTO upload_runs(
                run_id, session_id, registration_state, created_at_utc, updated_at_utc
             )
             SELECT DISTINCT ranked.run_id, ranked.session_id, 'pending',
                    strftime('%Y-%m-%dT%H:%M:%fZ','now'),
                    strftime('%Y-%m-%dT%H:%M:%fZ','now')
             FROM (
                SELECT g.id AS segment_id, g.session_id, g.run_id,
                       ROW_NUMBER() OVER (PARTITION BY g.session_id ORDER BY r.seq, g.seq) AS ordinal
                FROM segments g
                JOIN capture_runs r ON r.id = g.run_id
                WHERE g.state = 'ready'
             ) ranked
             JOIN data_model_jobs d ON d.session_id = ranked.session_id
             WHERE ranked.ordinal <= d.processed_segments
             ON CONFLICT(run_id) DO NOTHING;
             INSERT INTO upload_jobs(
                segment_id, session_id, run_id, state, created_at_utc, updated_at_utc
             )
             SELECT ranked.segment_id, ranked.session_id, ranked.run_id, 'pending',
                    strftime('%Y-%m-%dT%H:%M:%fZ','now'),
                    strftime('%Y-%m-%dT%H:%M:%fZ','now')
             FROM (
                SELECT g.id AS segment_id, g.session_id, g.run_id,
                       ROW_NUMBER() OVER (PARTITION BY g.session_id ORDER BY r.seq, g.seq) AS ordinal
                FROM segments g
                JOIN capture_runs r ON r.id = g.run_id
                WHERE g.state = 'ready'
             ) ranked
             JOIN data_model_jobs d ON d.session_id = ranked.session_id
             WHERE ranked.ordinal <= d.processed_segments
             ON CONFLICT(segment_id) DO NOTHING;
             UPDATE journal_meta SET schema_version = 9 WHERE singleton = 1;
             COMMIT;",
        )?;
        version = 9;
    }
    if version == 9 && target > 9 {
        // First-party-release capture (docs/18): a consented session is
        // trainable, so `non_trainable = 0`. The historical fail-closed
        // `CHECK(non_trainable = 1)` on `sessions`/`markers` and the
        // `catalog_mode IN ('certified','dev_non_trainable')` guard on
        // `preflight_attempts` must be relaxed — SQLite cannot ALTER a CHECK,
        // so each table is rebuilt. `production_certified` stays pinned to 0
        // (the converter certifies delivery, never the recorder), and a new
        // paired CHECK ties `non_trainable = 0` to `catalog_mode =
        // 'first_party_release'` so a trainable row can never claim a dev mode.
        //
        // Only `sessions` has inbound foreign keys, so the rebuild runs with
        // foreign_keys OFF (a pragma that no-ops inside a transaction, hence the
        // separate statements); every id is preserved, so `foreign_key_check`
        // must come back empty. markers/preflight_attempts have no inbound refs.
        // The prior enforcement state is captured and restored so this migration
        // never silently changes the connection's foreign-key mode.
        let foreign_keys_were_on: bool = connection
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
            .unwrap_or(false);
        connection.execute_batch("PRAGMA foreign_keys=OFF;")?;
        connection.execute_batch(
            "BEGIN IMMEDIATE;
             CREATE TABLE sessions_v10 (
                id TEXT PRIMARY KEY,
                state TEXT NOT NULL CHECK(state IN ('active', 'paused', 'completed', 'faulted', 'corrupt')),
                non_trainable INTEGER NOT NULL CHECK(non_trainable IN (0, 1)),
                production_certified INTEGER NOT NULL CHECK(production_certified = 0),
                annotator_id TEXT,
                device_id TEXT,
                consent_id TEXT,
                game_id TEXT NOT NULL,
                game_exe_hash TEXT NOT NULL,
                capture_profile_id TEXT NOT NULL,
                capture_profile_hash TEXT NOT NULL,
                settings_profile_id TEXT NOT NULL,
                mapping_id TEXT NOT NULL,
                mapping_version TEXT NOT NULL,
                mapping_hash TEXT NOT NULL,
                schema_version TEXT NOT NULL,
                started_at_utc TEXT NOT NULL,
                ended_at_utc TEXT,
                stop_reason TEXT,
                last_error_code TEXT,
                created_at_utc TEXT NOT NULL,
                updated_at_utc TEXT NOT NULL,
                catalog_mode TEXT NOT NULL DEFAULT 'legacy_dev_non_trainable',
                catalog_id TEXT NOT NULL DEFAULT 'legacy-unversioned',
                catalog_version TEXT NOT NULL DEFAULT 'legacy',
                catalog_hash TEXT NOT NULL DEFAULT 'legacy-unavailable',
                device_profile_id TEXT NOT NULL DEFAULT 'legacy-unavailable',
                device_profile_version TEXT NOT NULL DEFAULT 'legacy',
                device_profile_hash TEXT NOT NULL DEFAULT 'legacy-unavailable',
                capture_profile_version TEXT NOT NULL DEFAULT 'legacy',
                game_profile_version TEXT NOT NULL DEFAULT 'legacy',
                game_profile_hash TEXT NOT NULL DEFAULT 'legacy-unavailable',
                settings_profile_version TEXT NOT NULL DEFAULT 'legacy',
                settings_profile_hash TEXT NOT NULL DEFAULT 'legacy-unavailable',
                capture_profile_hash_kind TEXT NOT NULL DEFAULT 'legacy_reencoded_json_sha256',
                CHECK((non_trainable = 0) = (catalog_mode = 'first_party_release'))
             );
             -- 显式列名拷贝而不是 SELECT *：SELECT * 拷的是**源表此刻的形状**，
             -- 一旦源表比这一版认识的列多，就以 35 columns but 36 values 失败。
             -- （历史上这条约定是为了让「把 schema_version 回拨后重放」的测试能过；
             -- 那种回拨已经退役，测试改用 migrate_to 造真库——见 migrate_to 的注释。
             -- 但显式列名本身与回拨无关，它是这一步该有的写法，别改回 SELECT *。）
             INSERT INTO sessions_v10(
                id, state, non_trainable, production_certified, annotator_id, device_id,
                consent_id, game_id, game_exe_hash, capture_profile_id, capture_profile_hash,
                settings_profile_id, mapping_id, mapping_version, mapping_hash, schema_version,
                started_at_utc, ended_at_utc, stop_reason, last_error_code, created_at_utc,
                updated_at_utc, catalog_mode, catalog_id, catalog_version, catalog_hash,
                device_profile_id, device_profile_version, device_profile_hash,
                capture_profile_version, game_profile_version, game_profile_hash,
                settings_profile_version, settings_profile_hash, capture_profile_hash_kind)
             SELECT
                id, state, non_trainable, production_certified, annotator_id, device_id,
                consent_id, game_id, game_exe_hash, capture_profile_id, capture_profile_hash,
                settings_profile_id, mapping_id, mapping_version, mapping_hash, schema_version,
                started_at_utc, ended_at_utc, stop_reason, last_error_code, created_at_utc,
                updated_at_utc, catalog_mode, catalog_id, catalog_version, catalog_hash,
                device_profile_id, device_profile_version, device_profile_hash,
                capture_profile_version, game_profile_version, game_profile_hash,
                settings_profile_version, settings_profile_hash, capture_profile_hash_kind
               FROM sessions;
             DROP TABLE sessions;
             ALTER TABLE sessions_v10 RENAME TO sessions;

             CREATE TABLE markers_v10 (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                run_id TEXT NOT NULL REFERENCES capture_runs(id) ON DELETE CASCADE,
                run_name TEXT NOT NULL,
                seq INTEGER NOT NULL CHECK(seq >= 0),
                request_id TEXT NOT NULL UNIQUE,
                kind TEXT NOT NULL CHECK(kind IN ('manual')),
                qpc INTEGER NOT NULL CHECK(qpc >= 0),
                qpc_frequency INTEGER NOT NULL CHECK(qpc_frequency > 0),
                run_frame INTEGER NOT NULL CHECK(run_frame >= 0),
                session_frame INTEGER NOT NULL CHECK(session_frame >= 0),
                video_time_us INTEGER NOT NULL CHECK(video_time_us >= 0),
                alignment_state TEXT NOT NULL CHECK(alignment_state IN ('pending', 'aligned', 'outside_video')),
                segment_seq INTEGER,
                non_trainable INTEGER NOT NULL CHECK(non_trainable IN (0, 1)),
                production_certified INTEGER NOT NULL CHECK(production_certified = 0),
                created_at_utc TEXT NOT NULL,
                UNIQUE(session_id, seq),
                CHECK((alignment_state = 'aligned' AND segment_seq IS NOT NULL)
                   OR (alignment_state != 'aligned' AND segment_seq IS NULL))
             );
             INSERT INTO markers_v10 SELECT * FROM markers;
             DROP TABLE markers;
             ALTER TABLE markers_v10 RENAME TO markers;
             CREATE INDEX IF NOT EXISTS idx_markers_session ON markers(session_id, seq);
             CREATE INDEX IF NOT EXISTS idx_markers_run_qpc ON markers(run_id, qpc);

             CREATE TABLE preflight_attempts_v10 (
                id TEXT PRIMARY KEY,
                request_id TEXT NOT NULL UNIQUE,
                state TEXT NOT NULL CHECK(state IN ('running', 'dev_non_trainable_pass', 'first_party_release_pass', 'blocked', 'failed')),
                phase TEXT NOT NULL,
                catalog_mode TEXT NOT NULL CHECK(catalog_mode IN ('certified', 'dev_non_trainable', 'first_party_release')),
                target_window_id TEXT NOT NULL,
                game_id TEXT NOT NULL,
                catalog_id TEXT NOT NULL,
                catalog_version TEXT NOT NULL,
                catalog_hash TEXT NOT NULL,
                device_profile_id TEXT NOT NULL,
                device_profile_version TEXT NOT NULL,
                device_profile_hash TEXT NOT NULL,
                capture_profile_id TEXT NOT NULL,
                capture_profile_version TEXT NOT NULL,
                capture_profile_hash TEXT NOT NULL,
                game_profile_version TEXT NOT NULL,
                game_profile_hash TEXT NOT NULL,
                settings_profile_id TEXT NOT NULL,
                settings_profile_version TEXT NOT NULL,
                settings_profile_hash TEXT NOT NULL,
                mapping_id TEXT NOT NULL,
                mapping_version TEXT NOT NULL,
                mapping_hash TEXT NOT NULL,
                session_id TEXT,
                run_name TEXT,
                failure_code TEXT,
                details_id TEXT,
                evidence_json TEXT NOT NULL,
                evidence_hash TEXT NOT NULL,
                created_at_utc TEXT NOT NULL,
                completed_at_utc TEXT,
                updated_at_utc TEXT NOT NULL,
                CHECK((session_id IS NULL AND run_name IS NULL)
                   OR (session_id IS NOT NULL AND run_name IS NOT NULL)),
                CHECK((state IN ('blocked', 'failed') AND failure_code IS NOT NULL AND details_id IS NOT NULL)
                   OR (state NOT IN ('blocked', 'failed') AND failure_code IS NULL AND details_id IS NULL))
             );
             INSERT INTO preflight_attempts_v10 SELECT * FROM preflight_attempts;
             DROP TABLE preflight_attempts;
             ALTER TABLE preflight_attempts_v10 RENAME TO preflight_attempts;
             CREATE INDEX IF NOT EXISTS idx_preflight_created
                 ON preflight_attempts(created_at_utc DESC, id DESC);
             CREATE INDEX IF NOT EXISTS idx_preflight_session
                 ON preflight_attempts(session_id, run_name);

             UPDATE journal_meta SET schema_version = 10 WHERE singleton = 1;
             COMMIT;",
        )?;
        let fk_violations: i64 = connection
            .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
                row.get(0)
            })
            .context("foreign_key_check after schema v10 rebuild failed")?;
        if foreign_keys_were_on {
            connection.execute_batch("PRAGMA foreign_keys=ON;")?;
        }
        ensure!(
            fk_violations == 0,
            "schema v10 rebuild left {fk_violations} dangling foreign-key reference(s)"
        );
        version = 10;
    }
    if version == 10 && target > 10 {
        // 客户端行为遥测的持久 outbox（docs/23 §4.1）。表结构与语义同构于 v7 的
        // mirror_outbox/mirror_cursor：seq AUTOINCREMENT 不复用、游标推进即删行、
        // 失败退避记录在游标上。差异：
        //   - event_id 是客户端生成的 uuid v4，贯穿 客户端→Postgres→GCS→BQ 的
        //     全链幂等键（服务端 ON CONFLICT (event_id) DO NOTHING）；
        //   - dropped_events 计数环形缓冲的丢弃（上限见 client_events::MAX_OUTBOX_ROWS），
        //     由下一条 app_started 事件带走并清零——丢弃本身必须可观测。
        connection.execute_batch(
            "BEGIN IMMEDIATE;
             CREATE TABLE IF NOT EXISTS client_event_outbox (
                seq INTEGER PRIMARY KEY AUTOINCREMENT,
                event_id TEXT NOT NULL UNIQUE CHECK(length(event_id) = 36),
                occurred_at_utc TEXT NOT NULL,
                kind TEXT NOT NULL CHECK(length(kind) BETWEEN 1 AND 64),
                severity TEXT NOT NULL CHECK(severity IN ('debug','info','warn','error')),
                session_id TEXT,
                payload_json TEXT NOT NULL CHECK(json_valid(payload_json)),
                created_at_utc TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS client_event_cursor (
                singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
                acked_seq INTEGER NOT NULL DEFAULT 0 CHECK(acked_seq >= 0),
                attempt_count INTEGER NOT NULL DEFAULT 0 CHECK(attempt_count >= 0),
                dropped_events INTEGER NOT NULL DEFAULT 0 CHECK(dropped_events >= 0),
                last_error TEXT,
                last_attempt_at_utc TEXT,
                updated_at_utc TEXT NOT NULL
             );
             INSERT INTO client_event_cursor(singleton, acked_seq, updated_at_utc)
             VALUES (1, 0, strftime('%Y-%m-%dT%H:%M:%fZ','now'))
             ON CONFLICT(singleton) DO NOTHING;
             UPDATE journal_meta SET schema_version = 11 WHERE singleton = 1;
             COMMIT;",
        )?;
        version = 11;
    }
    if version == 11 && target > 11 {
        // Session-root artifacts use a separate durable single-PUT state machine.
        // v11 is already the behavior telemetry outbox, so this feature advances
        // the journal independently instead of silently reusing that schema number.
        connection.execute_batch(
            "BEGIN IMMEDIATE;
             CREATE TABLE IF NOT EXISTS session_artifact_uploads (
                session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                kind TEXT NOT NULL CHECK(kind IN (
                    'session', 'catalog', 'device_profile', 'capture_profile',
                    'game_profile', 'settings_profile', 'mapping', 'markers'
                )),
                local_path TEXT NOT NULL,
                object_key TEXT,
                expected_sha256 TEXT NOT NULL,
                expected_size INTEGER NOT NULL CHECK(expected_size > 0),
                content_md5 TEXT NOT NULL,
                state TEXT NOT NULL CHECK(state IN (
                    'pending', 'declared', 'signed', 'put', 'completing',
                    'verified', 'rejected'
                )),
                attempts INTEGER NOT NULL DEFAULT 0 CHECK(attempts >= 0),
                next_attempt_at INTEGER NOT NULL DEFAULT 0 CHECK(next_attempt_at >= 0),
                verified_at TEXT,
                last_error_code TEXT,
                created_at_utc TEXT NOT NULL,
                updated_at_utc TEXT NOT NULL,
                PRIMARY KEY(session_id, kind),
                CHECK(state <> 'pending' OR object_key IS NULL),
                CHECK(state IN ('pending', 'rejected') OR object_key IS NOT NULL),
                CHECK((state = 'verified' AND verified_at IS NOT NULL)
                   OR (state <> 'verified' AND verified_at IS NULL))
             );
             CREATE INDEX IF NOT EXISTS idx_session_artifact_uploads_processable
                 ON session_artifact_uploads(state, next_attempt_at, updated_at_utc);
             UPDATE journal_meta SET schema_version = 12 WHERE singleton = 1;
             COMMIT;",
        )?;
        version = 12;
    }
    if version == 12 && target > 12 {
        // 视角尺度标定（docs/45/46）：标定是跨 session 的实体（属于
        // 游戏 × settings hash × 设备 profile hash × 本机）。session 与
        // 标定的引用放在独立关联表 session_view_calibrations 而不是给
        // sessions 加列。（原因当时写的是"v9→v10 的 sessions 重建用 SELECT *、
        // 迁移必须支持版本号回拨后重放"——那两条都已不成立：v9→v10 改成了显式
        // 列名，回拨式重放也随 migrate_to 退役。但结论仍然对：关联表让
        // sessions 的列集不必为一个跨 session 的实体而增长。）
        // 证据文件作为第 9 种可选 artifact 进入既有上传状态机；SQLite
        // 无法修改 CHECK 约束，session_artifact_uploads 按 sessions_v10
        // 先例整表重建（显式列名拷贝）。
        connection.execute_batch(
            "BEGIN IMMEDIATE;
             CREATE TABLE IF NOT EXISTS view_calibrations (
                calibration_id TEXT PRIMARY KEY,
                kind TEXT NOT NULL CHECK(kind IN ('full', 'spot_check')),
                parent_calibration_id TEXT REFERENCES view_calibrations(calibration_id),
                game_id TEXT NOT NULL,
                settings_profile_hash TEXT NOT NULL,
                device_profile_hash TEXT NOT NULL,
                created_utc TEXT NOT NULL,
                created_qpc INTEGER NOT NULL,
                verdict TEXT NOT NULL CHECK(verdict IN ('pass', 'fail')),
                counts_per_360_measured REAL,
                deviation_pct REAL,
                polling_hz_p50 REAL,
                file_path TEXT NOT NULL,
                file_sha256 TEXT NOT NULL,
                invalidated_utc TEXT,
                invalidation_reason TEXT CHECK(
                    invalidation_reason IN
                        ('settings_changed', 'config_watch', 'device_changed', 'expired')
                    OR invalidation_reason IS NULL),
                CHECK((invalidated_utc IS NULL) = (invalidation_reason IS NULL)),
                CHECK((kind = 'spot_check') = (parent_calibration_id IS NOT NULL))
             );
             CREATE INDEX IF NOT EXISTS idx_view_calibrations_scope
                 ON view_calibrations(
                    game_id, settings_profile_hash, device_profile_hash, created_qpc);
             CREATE TABLE IF NOT EXISTS session_view_calibrations (
                session_id TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
                calibration_id TEXT NOT NULL REFERENCES view_calibrations(calibration_id),
                calibration_sha256 TEXT NOT NULL,
                created_at_utc TEXT NOT NULL
             );
             CREATE TABLE session_artifact_uploads_v13 (
                session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                kind TEXT NOT NULL CHECK(kind IN (
                    'session', 'catalog', 'device_profile', 'capture_profile',
                    'game_profile', 'settings_profile', 'mapping', 'markers',
                    'input_calibration'
                )),
                local_path TEXT NOT NULL,
                object_key TEXT,
                expected_sha256 TEXT NOT NULL,
                expected_size INTEGER NOT NULL CHECK(expected_size > 0),
                content_md5 TEXT NOT NULL,
                state TEXT NOT NULL CHECK(state IN (
                    'pending', 'declared', 'signed', 'put', 'completing',
                    'verified', 'rejected'
                )),
                attempts INTEGER NOT NULL DEFAULT 0 CHECK(attempts >= 0),
                next_attempt_at INTEGER NOT NULL DEFAULT 0 CHECK(next_attempt_at >= 0),
                verified_at TEXT,
                last_error_code TEXT,
                created_at_utc TEXT NOT NULL,
                updated_at_utc TEXT NOT NULL,
                PRIMARY KEY(session_id, kind),
                CHECK(state <> 'pending' OR object_key IS NULL),
                CHECK(state IN ('pending', 'rejected') OR object_key IS NOT NULL),
                CHECK((state = 'verified' AND verified_at IS NOT NULL)
                   OR (state <> 'verified' AND verified_at IS NULL))
             );
             INSERT INTO session_artifact_uploads_v13(
                session_id, kind, local_path, object_key, expected_sha256,
                expected_size, content_md5, state, attempts, next_attempt_at,
                verified_at, last_error_code, created_at_utc, updated_at_utc)
             SELECT session_id, kind, local_path, object_key, expected_sha256,
                expected_size, content_md5, state, attempts, next_attempt_at,
                verified_at, last_error_code, created_at_utc, updated_at_utc
               FROM session_artifact_uploads;
             DROP TABLE session_artifact_uploads;
             ALTER TABLE session_artifact_uploads_v13 RENAME TO session_artifact_uploads;
             CREATE INDEX IF NOT EXISTS idx_session_artifact_uploads_processable
                 ON session_artifact_uploads(state, next_attempt_at, updated_at_utc);
             UPDATE journal_meta SET schema_version = 13 WHERE singleton = 1;
             COMMIT;",
        )?;
    }
    if version < 14 && target >= 14 {
        // docs/55：身份采集失败改为降级留痕，`game_exe_hash` 因此可空。SQLite
        // 不支持 ALTER COLUMN 放宽 NOT NULL，按本文件既有先例整表重建（显式列名
        // 拷贝）。FK 先关后恢复——pragma 在事务内是 no-op，故放在事务外。
        let foreign_keys_were_on: bool =
            connection.query_row("PRAGMA foreign_keys", [], |row| row.get(0))?;
        connection.execute_batch("PRAGMA foreign_keys=OFF;")?;
        let rebuilt = connection.execute_batch(
            "BEGIN IMMEDIATE;
             -- v14 表 = v10 表的**逐字拷贝**，只改两处：game_exe_hash 放宽为可空、
             -- 新增 game_exe_hash_missing_reason 与二者互斥的 CHECK。
             -- 全部 DEFAULT 与既有 CHECK 必须原样保留——上一版我照 v1 形状手写，
             -- 丢了 legacy DEFAULT（依赖默认值的插入全部 NOT NULL 失败）、丢了
             -- first_party_release 配对约束，还把 non_trainable IN (0,1) 写成 = 1，
             -- 直接禁掉了可训练会话。
             CREATE TABLE sessions_v14 (
                id TEXT PRIMARY KEY,
                state TEXT NOT NULL CHECK(state IN ('active', 'paused', 'completed', 'faulted', 'corrupt')),
                non_trainable INTEGER NOT NULL CHECK(non_trainable IN (0, 1)),
                production_certified INTEGER NOT NULL CHECK(production_certified = 0),
                annotator_id TEXT,
                device_id TEXT,
                consent_id TEXT,
                game_id TEXT NOT NULL,
                game_exe_hash TEXT,
                capture_profile_id TEXT NOT NULL,
                capture_profile_hash TEXT NOT NULL,
                settings_profile_id TEXT NOT NULL,
                mapping_id TEXT NOT NULL,
                mapping_version TEXT NOT NULL,
                mapping_hash TEXT NOT NULL,
                schema_version TEXT NOT NULL,
                started_at_utc TEXT NOT NULL,
                ended_at_utc TEXT,
                stop_reason TEXT,
                last_error_code TEXT,
                created_at_utc TEXT NOT NULL,
                updated_at_utc TEXT NOT NULL,
                catalog_mode TEXT NOT NULL DEFAULT 'legacy_dev_non_trainable',
                catalog_id TEXT NOT NULL DEFAULT 'legacy-unversioned',
                catalog_version TEXT NOT NULL DEFAULT 'legacy',
                catalog_hash TEXT NOT NULL DEFAULT 'legacy-unavailable',
                device_profile_id TEXT NOT NULL DEFAULT 'legacy-unavailable',
                device_profile_version TEXT NOT NULL DEFAULT 'legacy',
                device_profile_hash TEXT NOT NULL DEFAULT 'legacy-unavailable',
                capture_profile_version TEXT NOT NULL DEFAULT 'legacy',
                game_profile_version TEXT NOT NULL DEFAULT 'legacy',
                game_profile_hash TEXT NOT NULL DEFAULT 'legacy-unavailable',
                settings_profile_version TEXT NOT NULL DEFAULT 'legacy',
                settings_profile_hash TEXT NOT NULL DEFAULT 'legacy-unavailable',
                capture_profile_hash_kind TEXT NOT NULL DEFAULT 'legacy_reencoded_json_sha256',
                game_exe_hash_missing_reason TEXT,
                CHECK((non_trainable = 0) = (catalog_mode = 'first_party_release')),
                CHECK((game_exe_hash IS NULL) = (game_exe_hash_missing_reason IS NOT NULL))
             );
             INSERT INTO sessions_v14(
                id, state, non_trainable, production_certified, annotator_id, device_id,
                consent_id, game_id, game_exe_hash, game_exe_hash_missing_reason,
                capture_profile_id, capture_profile_hash, capture_profile_hash_kind,
                settings_profile_id, mapping_id, mapping_version, mapping_hash,
                catalog_mode, catalog_id, catalog_version, catalog_hash,
                device_profile_id, device_profile_version, device_profile_hash,
                capture_profile_version, game_profile_version, game_profile_hash,
                settings_profile_version, settings_profile_hash,
                schema_version, started_at_utc, ended_at_utc, stop_reason, last_error_code,
                created_at_utc, updated_at_utc)
             SELECT
                id, state, non_trainable, production_certified, annotator_id, device_id,
                consent_id, game_id, game_exe_hash, NULL,
                capture_profile_id, capture_profile_hash, capture_profile_hash_kind,
                settings_profile_id, mapping_id, mapping_version, mapping_hash,
                catalog_mode, catalog_id, catalog_version, catalog_hash,
                device_profile_id, device_profile_version, device_profile_hash,
                capture_profile_version, game_profile_version, game_profile_hash,
                settings_profile_version, settings_profile_hash,
                schema_version, started_at_utc, ended_at_utc, stop_reason, last_error_code,
                created_at_utc, updated_at_utc
               FROM sessions;
             DROP TABLE sessions;
             ALTER TABLE sessions_v14 RENAME TO sessions;
             UPDATE journal_meta SET schema_version = 14 WHERE singleton = 1;
             COMMIT;",
        );
        if foreign_keys_were_on {
            connection.execute_batch("PRAGMA foreign_keys=ON;")?;
        }
        rebuilt?;
    }
    if version < 15 && target >= 15 {
        // docs/60 §6 Q4 的自由测量段：kind 词表加 'free'，且这种记录**没有
        // 验收裁决**。SQLite 改不了 CHECK，按 v13/v14 先例整表重建（显式列名
        // 拷贝）。FK 先关后恢复——view_calibrations 既被自己的
        // parent_calibration_id 引用，也被 session_view_calibrations 引用，
        // 不关 FK 则 DROP 那一步会被拒；pragma 在事务内是 no-op，故放事务外。
        let foreign_keys_were_on: bool =
            connection.query_row("PRAGMA foreign_keys", [], |row| row.get(0))?;
        connection.execute_batch("PRAGMA foreign_keys=OFF;")?;
        let rebuilt = connection.execute_batch(
            "BEGIN IMMEDIATE;
             -- v15 表 = v13 表的**逐字拷贝**，只改三处：kind 词表加 'free'、
             -- verdict 放宽为可空、加 kind/verdict 的对偶 CHECK。其余列、
             -- DEFAULT 与两条既有 CHECK 必须原样保留（v14 的教训：照记忆手写
             -- 会丢 DEFAULT 和配对约束）。
             CREATE TABLE view_calibrations_v15 (
                calibration_id TEXT PRIMARY KEY,
                kind TEXT NOT NULL CHECK(kind IN ('full', 'spot_check', 'free')),
                parent_calibration_id TEXT REFERENCES view_calibrations_v15(calibration_id),
                game_id TEXT NOT NULL,
                settings_profile_hash TEXT NOT NULL,
                device_profile_hash TEXT NOT NULL,
                created_utc TEXT NOT NULL,
                created_qpc INTEGER NOT NULL,
                verdict TEXT CHECK(verdict IN ('pass', 'fail') OR verdict IS NULL),
                counts_per_360_measured REAL,
                deviation_pct REAL,
                polling_hz_p50 REAL,
                file_path TEXT NOT NULL,
                file_sha256 TEXT NOT NULL,
                invalidated_utc TEXT,
                invalidation_reason TEXT CHECK(
                    invalidation_reason IN
                        ('settings_changed', 'config_watch', 'device_changed', 'expired')
                    OR invalidation_reason IS NULL),
                CHECK((invalidated_utc IS NULL) = (invalidation_reason IS NULL)),
                CHECK((kind = 'spot_check') = (parent_calibration_id IS NOT NULL)),
                -- 自由段必无裁决、正式标定必有：两头锁死，不给「拿 fail 当
                -- 占位」这种说谎留门。
                CHECK((kind = 'free') = (verdict IS NULL))
             );
             INSERT INTO view_calibrations_v15(
                calibration_id, kind, parent_calibration_id, game_id,
                settings_profile_hash, device_profile_hash, created_utc, created_qpc,
                verdict, counts_per_360_measured, deviation_pct, polling_hz_p50,
                file_path, file_sha256, invalidated_utc, invalidation_reason)
             SELECT
                calibration_id, kind, parent_calibration_id, game_id,
                settings_profile_hash, device_profile_hash, created_utc, created_qpc,
                verdict, counts_per_360_measured, deviation_pct, polling_hz_p50,
                file_path, file_sha256, invalidated_utc, invalidation_reason
               FROM view_calibrations;
             DROP TABLE view_calibrations;
             ALTER TABLE view_calibrations_v15 RENAME TO view_calibrations;
             CREATE INDEX IF NOT EXISTS idx_view_calibrations_scope
                 ON view_calibrations(
                    game_id, settings_profile_hash, device_profile_hash, created_qpc);
             UPDATE journal_meta SET schema_version = 15 WHERE singleton = 1;
             COMMIT;",
        );
        if foreign_keys_were_on {
            connection.execute_batch("PRAGMA foreign_keys=ON;")?;
        }
        rebuilt?;
    }
    if version < 16 && target >= 16 {
        // docs/57 批次二：五本册子收成四本。capture/settings 两对列并成一对
        // recording_*；`first_party_release` 随通道退役，可训练车道改成 certified。
        // 老行的 capture/settings 并列不丢事实——两本册子的字节都还在
        // session_artifact_uploads 里，行上留下的是"这次录制用了哪一份"。
        // 同 v14：SQLite 改不了列名与 CHECK，整表重建；pragma 在事务内 no-op，放事务外。
        let foreign_keys_were_on: bool =
            connection.query_row("PRAGMA foreign_keys", [], |row| row.get(0))?;
        connection.execute_batch("PRAGMA foreign_keys=OFF;")?;
        let rebuilt = connection.execute_batch(
            "BEGIN IMMEDIATE;
             CREATE TABLE sessions_v16 (
                id TEXT PRIMARY KEY,
                state TEXT NOT NULL CHECK(state IN ('active', 'paused', 'completed', 'faulted', 'corrupt')),
                non_trainable INTEGER NOT NULL CHECK(non_trainable IN (0, 1)),
                production_certified INTEGER NOT NULL CHECK(production_certified = 0),
                annotator_id TEXT,
                device_id TEXT,
                consent_id TEXT,
                game_id TEXT NOT NULL,
                game_exe_hash TEXT,
                recording_profile_id TEXT NOT NULL,
                recording_profile_hash TEXT NOT NULL,
                mapping_id TEXT NOT NULL,
                mapping_version TEXT NOT NULL,
                mapping_hash TEXT NOT NULL,
                schema_version TEXT NOT NULL,
                started_at_utc TEXT NOT NULL,
                ended_at_utc TEXT,
                stop_reason TEXT,
                last_error_code TEXT,
                created_at_utc TEXT NOT NULL,
                updated_at_utc TEXT NOT NULL,
                catalog_mode TEXT NOT NULL DEFAULT \'legacy_dev_non_trainable\',
                catalog_id TEXT NOT NULL DEFAULT \'legacy-unversioned\',
                catalog_version TEXT NOT NULL DEFAULT \'legacy\',
                catalog_hash TEXT NOT NULL DEFAULT \'legacy-unavailable\',
                device_profile_id TEXT NOT NULL DEFAULT \'legacy-unavailable\',
                device_profile_version TEXT NOT NULL DEFAULT \'legacy\',
                device_profile_hash TEXT NOT NULL DEFAULT \'legacy-unavailable\',
                recording_profile_version TEXT NOT NULL DEFAULT \'legacy\',
                game_profile_version TEXT NOT NULL DEFAULT \'legacy\',
                game_profile_hash TEXT NOT NULL DEFAULT \'legacy-unavailable\',
                recording_profile_hash_kind TEXT NOT NULL DEFAULT \'legacy_reencoded_json_sha256\',
                game_exe_hash_missing_reason TEXT,
                -- first_party 是唯一声明过 non_trainable=0 的车道，随 docs/57 §3.3
                -- 退役；certified 的上传车道（dm-certified family）在客户端尚未接线，
                -- 所以这个版本的客户端不会声明任何可训练会话。接线时连同这条 CHECK
                -- 一起放宽，而不是现在留一个没有生产者的 0 分支。
                CHECK(non_trainable = 1),
                CHECK((game_exe_hash IS NULL) = (game_exe_hash_missing_reason IS NOT NULL))
             );
             INSERT INTO sessions_v16(
                id, state, non_trainable, production_certified, annotator_id, device_id,
                consent_id, game_id, game_exe_hash, game_exe_hash_missing_reason,
                recording_profile_id, recording_profile_hash, recording_profile_hash_kind,
                recording_profile_version, mapping_id, mapping_version, mapping_hash,
                catalog_mode, catalog_id, catalog_version, catalog_hash,
                device_profile_id, device_profile_version, device_profile_hash,
                game_profile_version, game_profile_hash,
                schema_version, started_at_utc, ended_at_utc, stop_reason, last_error_code,
                created_at_utc, updated_at_utc)
             SELECT
                id, state, non_trainable, production_certified, annotator_id, device_id,
                consent_id, game_id, game_exe_hash, game_exe_hash_missing_reason,
                capture_profile_id, capture_profile_hash, capture_profile_hash_kind,
                capture_profile_version, mapping_id, mapping_version, mapping_hash,
                catalog_mode, catalog_id, catalog_version, catalog_hash,
                device_profile_id, device_profile_version, device_profile_hash,
                game_profile_version, game_profile_hash,
                schema_version, started_at_utc, ended_at_utc, stop_reason, last_error_code,
                created_at_utc, updated_at_utc
               FROM sessions
              WHERE catalog_mode <> \'first_party_release\';
             DROP TABLE sessions;
             ALTER TABLE sessions_v16 RENAME TO sessions;

             CREATE TABLE preflight_attempts_v16 (
                id TEXT PRIMARY KEY,
                request_id TEXT NOT NULL UNIQUE,
                state TEXT NOT NULL CHECK(state IN (\'running\', \'dev_non_trainable_pass\', \'blocked\', \'failed\')),
                phase TEXT NOT NULL,
                catalog_mode TEXT NOT NULL CHECK(catalog_mode IN (\'certified\', \'dev_non_trainable\')),
                target_window_id TEXT NOT NULL,
                game_id TEXT NOT NULL,
                catalog_id TEXT NOT NULL,
                catalog_version TEXT NOT NULL,
                catalog_hash TEXT NOT NULL,
                device_profile_id TEXT NOT NULL,
                device_profile_version TEXT NOT NULL,
                device_profile_hash TEXT NOT NULL,
                recording_profile_id TEXT NOT NULL,
                recording_profile_version TEXT NOT NULL,
                recording_profile_hash TEXT NOT NULL,
                game_profile_version TEXT NOT NULL,
                game_profile_hash TEXT NOT NULL,
                mapping_id TEXT NOT NULL,
                mapping_version TEXT NOT NULL,
                mapping_hash TEXT NOT NULL,
                session_id TEXT,
                run_name TEXT,
                failure_code TEXT,
                details_id TEXT,
                evidence_json TEXT NOT NULL,
                evidence_hash TEXT NOT NULL,
                created_at_utc TEXT NOT NULL,
                completed_at_utc TEXT,
                updated_at_utc TEXT NOT NULL,
                CHECK((session_id IS NULL AND run_name IS NULL)
                   OR (session_id IS NOT NULL AND run_name IS NOT NULL)),
                CHECK((state IN (\'blocked\', \'failed\') AND failure_code IS NOT NULL AND details_id IS NOT NULL)
                   OR (state NOT IN (\'blocked\', \'failed\') AND failure_code IS NULL AND details_id IS NULL))
             );
             INSERT INTO preflight_attempts_v16(
                id, request_id, state, phase, catalog_mode, target_window_id, game_id,
                catalog_id, catalog_version, catalog_hash,
                device_profile_id, device_profile_version, device_profile_hash,
                recording_profile_id, recording_profile_version, recording_profile_hash,
                game_profile_version, game_profile_hash,
                mapping_id, mapping_version, mapping_hash,
                session_id, run_name, failure_code, details_id,
                evidence_json, evidence_hash, created_at_utc, completed_at_utc, updated_at_utc)
             SELECT
                id, request_id, state, phase, catalog_mode, target_window_id, game_id,
                catalog_id, catalog_version, catalog_hash,
                device_profile_id, device_profile_version, device_profile_hash,
                capture_profile_id, capture_profile_version, capture_profile_hash,
                game_profile_version, game_profile_hash,
                mapping_id, mapping_version, mapping_hash,
                session_id, run_name, failure_code, details_id,
                evidence_json, evidence_hash, created_at_utc, completed_at_utc, updated_at_utc
               FROM preflight_attempts
              WHERE catalog_mode <> \'first_party_release\'
                AND state <> \'first_party_release_pass\';
             DROP TABLE preflight_attempts;
             ALTER TABLE preflight_attempts_v16 RENAME TO preflight_attempts;
             CREATE INDEX IF NOT EXISTS idx_preflight_created
                 ON preflight_attempts(created_at_utc DESC, id DESC);
             CREATE INDEX IF NOT EXISTS idx_preflight_session
                 ON preflight_attempts(session_id, run_name);

             -- kind 词表里的 \'free\'、可空 verdict、以及 kind/verdict 的对偶 CHECK
             -- 来自迁移 15（自由测量段，docs/60 §6 Q4 / PR #347）。这里**必须原样
             -- 带上**：本迁移重建了整张表，漏掉任何一条就等于把 15 的改动静默回退。
             --
             -- 两条线一度各自占了 15。让 15 归自由测量段、本批次挪到 16 是对的：
             -- #347 已经先合进 main（2026-07-31 合了回来），15 就在上面那一步，
             -- 一个从 14 上来的旧库会老老实实走 14 -> 15 -> 16。
             -- 当初这么排是因为撤掉那一段会让 #347 在 main 上直接坏着（那时 main
             -- 的这张表还建于迁移 13，kind 两值且 verdict NOT NULL，写一行 free 会
             -- 被两个约束各挡一次），而批次二的合并时机取决于真机验收、没有日期。
             --
             -- 本批次自己的改动只有一处：settings_profile_hash -> recording_profile_hash
             -- （docs/57 §3.1）。从 15 的形状出来时源列仍叫 settings_profile_hash，
             -- 所以下面 INSERT 的列映射不因改号而变。
             CREATE TABLE view_calibrations_v16 (
                calibration_id TEXT PRIMARY KEY,
                kind TEXT NOT NULL CHECK(kind IN (\'full\', \'spot_check\', \'free\')),
                parent_calibration_id TEXT REFERENCES view_calibrations_v16(calibration_id),
                game_id TEXT NOT NULL,
                recording_profile_hash TEXT NOT NULL,
                device_profile_hash TEXT NOT NULL,
                created_utc TEXT NOT NULL,
                created_qpc INTEGER NOT NULL,
                verdict TEXT CHECK(verdict IN (\'pass\', \'fail\') OR verdict IS NULL),
                counts_per_360_measured REAL,
                deviation_pct REAL,
                polling_hz_p50 REAL,
                file_path TEXT NOT NULL,
                file_sha256 TEXT NOT NULL,
                invalidated_utc TEXT,
                invalidation_reason TEXT CHECK(
                    invalidation_reason IN
                        (\'settings_changed\', \'config_watch\', \'device_changed\', \'expired\')
                    OR invalidation_reason IS NULL),
                CHECK((invalidated_utc IS NULL) = (invalidation_reason IS NULL)),
                CHECK((kind = \'spot_check\') = (parent_calibration_id IS NOT NULL)),
                -- 自由段必无裁决、正式标定必有：两头锁死，不给「拿 fail 当占位」
                -- 这种说谎留门。
                CHECK((kind = \'free\') = (verdict IS NULL))
             );
             -- 显式列名，不用 SELECT *。这一步正在**改列名**（settings -> recording），
             -- SELECT * 靠的是列的位置恰好没变——那是巧合不是契约，v9->v10 的
             -- sessions 重建已经在这个仓库栽过一次。
             INSERT INTO view_calibrations_v16(
                calibration_id, kind, parent_calibration_id, game_id,
                recording_profile_hash, device_profile_hash, created_utc, created_qpc,
                verdict, counts_per_360_measured, deviation_pct, polling_hz_p50,
                file_path, file_sha256, invalidated_utc, invalidation_reason)
             SELECT
                calibration_id, kind, parent_calibration_id, game_id,
                settings_profile_hash, device_profile_hash, created_utc, created_qpc,
                verdict, counts_per_360_measured, deviation_pct, polling_hz_p50,
                file_path, file_sha256, invalidated_utc, invalidation_reason
               FROM view_calibrations;
             DROP TABLE view_calibrations;
             ALTER TABLE view_calibrations_v16 RENAME TO view_calibrations;
             CREATE INDEX IF NOT EXISTS idx_view_calibrations_scope
                 ON view_calibrations(
                    game_id, recording_profile_hash, device_profile_hash, created_qpc);

             CREATE TABLE session_artifact_uploads_v16 (
                session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                kind TEXT NOT NULL CHECK(kind IN (
                    \'session\', \'catalog\', \'device_profile\', \'recording_profile\',
                    \'game_profile\', \'mapping\', \'markers\', \'input_calibration\'
                )),
                local_path TEXT NOT NULL,
                object_key TEXT,
                expected_sha256 TEXT NOT NULL,
                expected_size INTEGER NOT NULL CHECK(expected_size > 0),
                content_md5 TEXT NOT NULL,
                state TEXT NOT NULL CHECK(state IN (
                    \'pending\', \'declared\', \'signed\', \'put\', \'completing\',
                    \'verified\', \'rejected\'
                )),
                attempts INTEGER NOT NULL DEFAULT 0 CHECK(attempts >= 0),
                next_attempt_at INTEGER NOT NULL DEFAULT 0 CHECK(next_attempt_at >= 0),
                verified_at TEXT,
                last_error_code TEXT,
                created_at_utc TEXT NOT NULL,
                updated_at_utc TEXT NOT NULL,
                PRIMARY KEY(session_id, kind),
                CHECK(state <> \'pending\' OR object_key IS NULL),
                CHECK(state IN (\'pending\', \'rejected\') OR object_key IS NOT NULL),
                CHECK((state = \'verified\' AND verified_at IS NOT NULL)
                   OR (state <> \'verified\' AND verified_at IS NULL))
             );
             -- 老行的两条 capture/settings 记录合并不了（它们是两份不同的字节），
             -- 但它们属于的会话也已经在上面按渠道被清掉了，直接丢弃残留行。
             INSERT INTO session_artifact_uploads_v16(
                session_id, kind, local_path, object_key, expected_sha256,
                expected_size, content_md5, state, attempts, next_attempt_at,
                verified_at, last_error_code, created_at_utc, updated_at_utc)
             SELECT session_id, kind, local_path, object_key, expected_sha256,
                expected_size, content_md5, state, attempts, next_attempt_at,
                verified_at, last_error_code, created_at_utc, updated_at_utc
               FROM session_artifact_uploads
              WHERE kind NOT IN (\'capture_profile\', \'settings_profile\')
                AND session_id IN (SELECT id FROM sessions);
             DROP TABLE session_artifact_uploads;
             ALTER TABLE session_artifact_uploads_v16 RENAME TO session_artifact_uploads;
             CREATE INDEX IF NOT EXISTS idx_session_artifact_uploads_processable
                 ON session_artifact_uploads(state, next_attempt_at, updated_at_utc);
             UPDATE journal_meta SET schema_version = 16 WHERE singleton = 1;
             COMMIT;",
        );
        if foreign_keys_were_on {
            connection.execute_batch("PRAGMA foreign_keys=ON;")?;
        }
        rebuilt?;
    }
    if version < 17 && target >= 17 {
        // Optional process-loopback audio is a clip-level sidecar. Persist the
        // exact metadata block on the segment so recovery, UI and uploader all
        // read one durable fact. The multipart journal needs the matching
        // optional sixth kind; SQLite cannot widen its CHECK in place.
        let has_audio_column: bool = connection.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM pragma_table_info('segments') WHERE name = 'audio_json'
             )",
            [],
            |row| row.get(0),
        )?;
        let add_audio_column = if has_audio_column {
            ""
        } else {
            "ALTER TABLE segments ADD COLUMN audio_json TEXT
                 CHECK(audio_json IS NULL OR json_valid(audio_json));"
        };
        connection.execute_batch(&format!(
            "BEGIN IMMEDIATE;
             {add_audio_column}
             CREATE TABLE upload_parts_v17 (
                segment_id TEXT NOT NULL REFERENCES upload_jobs(segment_id) ON DELETE CASCADE,
                artifact_kind TEXT NOT NULL CHECK(artifact_kind IN (
                    'video', 'frames', 'events', 'meta', 'manifest', 'audio'
                )),
                part_number INTEGER NOT NULL CHECK(part_number BETWEEN 1 AND 10000),
                byte_offset INTEGER NOT NULL CHECK(byte_offset >= 0),
                byte_size INTEGER NOT NULL CHECK(byte_size > 0),
                content_md5 TEXT NOT NULL,
                etag TEXT NOT NULL,
                uploaded_at_utc TEXT NOT NULL,
                PRIMARY KEY(segment_id, artifact_kind, part_number)
             );
             INSERT INTO upload_parts_v17(
                segment_id, artifact_kind, part_number, byte_offset, byte_size,
                content_md5, etag, uploaded_at_utc)
             SELECT segment_id, artifact_kind, part_number, byte_offset, byte_size,
                    content_md5, etag, uploaded_at_utc
               FROM upload_parts;
             DROP TABLE upload_parts;
             ALTER TABLE upload_parts_v17 RENAME TO upload_parts;
             UPDATE journal_meta SET schema_version = 17 WHERE singleton = 1;
             COMMIT;"
        ))?;
    }
    if version < 18 && target >= 18 {
        // certified 预检终态（docs/26 §7）。`preflight_attempts.state` 的词表从
        // 迁移 4 起就只认 dev 那一个通过词，于是即使 grant 到位、预检走到了终点，
        // 落盘也会被 CHECK 挡下——一个「验过 grant 却写不进 journal」的死角。
        //
        // ⚠ 加一个词**不等于**打开 certified 录制：一行要带上这个值，前提是本机
        // 持有一份验签通过的 capture grant，而 grant 由服务端按治理条件签发
        // （docs/26 §5）。词表放开的是记录能力，不是授权。
        //
        // SQLite 改不了 CHECK，按 v13/v14/v16 先例整表重建：**逐字拷贝 v16 的形状**，
        // 只在 state 词表里多一个 'certified_pass'。列、两条配对 CHECK 与两个索引
        // 必须原样带上（v14 的教训：照记忆手写会丢约束）。
        //
        // 不做 FK 的 pragma 开关：preflight_attempts 既不引用别人，也没有别人引用它
        // （`capture_runs.preflight_id` 是裸 TEXT，没有 REFERENCES），DROP 不会被 FK
        // 拒绝。v16 要关 FK 是因为它同时重建了被引用的 `sessions`。
        connection.execute_batch(
            "BEGIN IMMEDIATE;
             CREATE TABLE preflight_attempts_v18 (
                id TEXT PRIMARY KEY,
                request_id TEXT NOT NULL UNIQUE,
                state TEXT NOT NULL CHECK(state IN (
                    'running', 'dev_non_trainable_pass', 'certified_pass', 'blocked', 'failed')),
                phase TEXT NOT NULL,
                catalog_mode TEXT NOT NULL CHECK(catalog_mode IN ('certified', 'dev_non_trainable')),
                target_window_id TEXT NOT NULL,
                game_id TEXT NOT NULL,
                catalog_id TEXT NOT NULL,
                catalog_version TEXT NOT NULL,
                catalog_hash TEXT NOT NULL,
                device_profile_id TEXT NOT NULL,
                device_profile_version TEXT NOT NULL,
                device_profile_hash TEXT NOT NULL,
                recording_profile_id TEXT NOT NULL,
                recording_profile_version TEXT NOT NULL,
                recording_profile_hash TEXT NOT NULL,
                game_profile_version TEXT NOT NULL,
                game_profile_hash TEXT NOT NULL,
                mapping_id TEXT NOT NULL,
                mapping_version TEXT NOT NULL,
                mapping_hash TEXT NOT NULL,
                session_id TEXT,
                run_name TEXT,
                failure_code TEXT,
                details_id TEXT,
                evidence_json TEXT NOT NULL,
                evidence_hash TEXT NOT NULL,
                created_at_utc TEXT NOT NULL,
                completed_at_utc TEXT,
                updated_at_utc TEXT NOT NULL,
                CHECK((session_id IS NULL AND run_name IS NULL)
                   OR (session_id IS NOT NULL AND run_name IS NOT NULL)),
                CHECK((state IN ('blocked', 'failed') AND failure_code IS NOT NULL AND details_id IS NOT NULL)
                   OR (state NOT IN ('blocked', 'failed') AND failure_code IS NULL AND details_id IS NULL))
             );
             -- 显式列名而不是 SELECT *：列序相同是巧合不是契约（v9->v10 栽过一次）。
             INSERT INTO preflight_attempts_v18(
                id, request_id, state, phase, catalog_mode, target_window_id, game_id,
                catalog_id, catalog_version, catalog_hash,
                device_profile_id, device_profile_version, device_profile_hash,
                recording_profile_id, recording_profile_version, recording_profile_hash,
                game_profile_version, game_profile_hash,
                mapping_id, mapping_version, mapping_hash,
                session_id, run_name, failure_code, details_id,
                evidence_json, evidence_hash, created_at_utc, completed_at_utc, updated_at_utc)
             SELECT
                id, request_id, state, phase, catalog_mode, target_window_id, game_id,
                catalog_id, catalog_version, catalog_hash,
                device_profile_id, device_profile_version, device_profile_hash,
                recording_profile_id, recording_profile_version, recording_profile_hash,
                game_profile_version, game_profile_hash,
                mapping_id, mapping_version, mapping_hash,
                session_id, run_name, failure_code, details_id,
                evidence_json, evidence_hash, created_at_utc, completed_at_utc, updated_at_utc
               FROM preflight_attempts;
             DROP TABLE preflight_attempts;
             ALTER TABLE preflight_attempts_v18 RENAME TO preflight_attempts;
             CREATE INDEX IF NOT EXISTS idx_preflight_created
                 ON preflight_attempts(created_at_utc DESC, id DESC);
             CREATE INDEX IF NOT EXISTS idx_preflight_session
                 ON preflight_attempts(session_id, run_name);
             UPDATE journal_meta SET schema_version = 18 WHERE singleton = 1;
             COMMIT;",
        )?;
    }
    if version < 19 && target >= 19 {
        // certified 会话准入（docs/63 §5.2，渠道裁决 A 之后）。v16 建表时留过话：
        // 「接线时连同这条 CHECK 一起放宽」——现在接线了：grant 链与 certified
        // 预检终态（v18）都已落地，certified 会话要能以 non_trainable = 0 落盘。
        //
        // 「放宽」换成三条更精确的耦合，而不是删检查：
        //   1) non_trainable = 0 当且仅当 catalog_mode = 'certified'——车道由
        //      已验签目录的 mode 给出，dev 会话永远抬不了自己的天花板；
        //   2) certified 行必须带 consent_id（grant 的 consent；上传时作为
        //      declaredConsentId 交服务端与 grant 对账，docs/26 §5）——
        //      「已授权」必须能指认是哪一份授权，匿名的授权不存在；
        //   3) certified 词表 ⇔ certified mode——与服务端迁移 0024 的真配对同一
        //      条不变量，词表交叉的行在两端都写不进去。
        // production_certified 仍恒 0：那是 first_party 时代的会话声明；certified
        // 家族的标记走上传信封的 productionCertified 字面量，不落 RAW/journal。
        //
        // 0022 过渡态的存量 certified 行（上一版 begin_run 恒写 non_trainable=1、
        // consent NULL）满足不了下面的配对 CHECK；不清直接重建会让迁移中途炸掉、
        // journal 从此打不开。这些行录于 consent 载体存在之前，作为 certified
        // 永远传不上去（服务端要求 declaredConsentId 与 grant 对账），照 v16 清
        // first_party 存量的先例在拷贝前删掉——趁 FK 还开着删，runs/segments 等
        // 子行随 ON DELETE CASCADE 一起走。
        connection.execute(
            "DELETE FROM sessions WHERE catalog_mode = 'certified' AND non_trainable = 1",
            [],
        )?;
        // SQLite 改不了 CHECK，按 v13/v14/v16 先例整表重建，逐字拷贝 v16 的形状、
        // 只动上述约束。sessions 被多张表 FK 引用，照 v16 关 FK 重建（pragma 在
        // 事务内是 no-op，必须放事务外）。
        let foreign_keys_were_on: bool =
            connection.query_row("PRAGMA foreign_keys", [], |row| row.get(0))?;
        connection.execute_batch("PRAGMA foreign_keys=OFF;")?;
        let rebuilt = connection.execute_batch(
            "BEGIN IMMEDIATE;
             CREATE TABLE sessions_v19 (
                id TEXT PRIMARY KEY,
                state TEXT NOT NULL CHECK(state IN ('active', 'paused', 'completed', 'faulted', 'corrupt')),
                non_trainable INTEGER NOT NULL CHECK(non_trainable IN (0, 1)),
                production_certified INTEGER NOT NULL CHECK(production_certified = 0),
                annotator_id TEXT,
                device_id TEXT,
                consent_id TEXT,
                game_id TEXT NOT NULL,
                game_exe_hash TEXT,
                recording_profile_id TEXT NOT NULL,
                recording_profile_hash TEXT NOT NULL,
                mapping_id TEXT NOT NULL,
                mapping_version TEXT NOT NULL,
                mapping_hash TEXT NOT NULL,
                schema_version TEXT NOT NULL,
                started_at_utc TEXT NOT NULL,
                ended_at_utc TEXT,
                stop_reason TEXT,
                last_error_code TEXT,
                created_at_utc TEXT NOT NULL,
                updated_at_utc TEXT NOT NULL,
                catalog_mode TEXT NOT NULL DEFAULT 'legacy_dev_non_trainable',
                catalog_id TEXT NOT NULL DEFAULT 'legacy-unversioned',
                catalog_version TEXT NOT NULL DEFAULT 'legacy',
                catalog_hash TEXT NOT NULL DEFAULT 'legacy-unavailable',
                device_profile_id TEXT NOT NULL DEFAULT 'legacy-unavailable',
                device_profile_version TEXT NOT NULL DEFAULT 'legacy',
                device_profile_hash TEXT NOT NULL DEFAULT 'legacy-unavailable',
                recording_profile_version TEXT NOT NULL DEFAULT 'legacy',
                game_profile_version TEXT NOT NULL DEFAULT 'legacy',
                game_profile_hash TEXT NOT NULL DEFAULT 'legacy-unavailable',
                recording_profile_hash_kind TEXT NOT NULL DEFAULT 'legacy_reencoded_json_sha256',
                game_exe_hash_missing_reason TEXT,
                CHECK((non_trainable = 0) = (catalog_mode = 'certified')),
                CHECK(non_trainable = 1 OR (consent_id IS NOT NULL AND trim(consent_id) <> '')),
                CHECK((schema_version LIKE 'dm-certified-%') = (catalog_mode = 'certified')),
                CHECK((game_exe_hash IS NULL) = (game_exe_hash_missing_reason IS NOT NULL))
             );
             -- 显式列名而不是 SELECT *：列序相同是巧合不是契约（v9->v10 栽过一次）。
             INSERT INTO sessions_v19(
                id, state, non_trainable, production_certified, annotator_id, device_id,
                consent_id, game_id, game_exe_hash, game_exe_hash_missing_reason,
                recording_profile_id, recording_profile_hash, recording_profile_hash_kind,
                recording_profile_version, mapping_id, mapping_version, mapping_hash,
                catalog_mode, catalog_id, catalog_version, catalog_hash,
                device_profile_id, device_profile_version, device_profile_hash,
                game_profile_version, game_profile_hash,
                schema_version, started_at_utc, ended_at_utc, stop_reason, last_error_code,
                created_at_utc, updated_at_utc)
             SELECT
                id, state, non_trainable, production_certified, annotator_id, device_id,
                consent_id, game_id, game_exe_hash, game_exe_hash_missing_reason,
                recording_profile_id, recording_profile_hash, recording_profile_hash_kind,
                recording_profile_version, mapping_id, mapping_version, mapping_hash,
                catalog_mode, catalog_id, catalog_version, catalog_hash,
                device_profile_id, device_profile_version, device_profile_hash,
                game_profile_version, game_profile_hash,
                schema_version, started_at_utc, ended_at_utc, stop_reason, last_error_code,
                created_at_utc, updated_at_utc
               FROM sessions;
             DROP TABLE sessions;
             ALTER TABLE sessions_v19 RENAME TO sessions;
             UPDATE journal_meta SET schema_version = 19 WHERE singleton = 1;
             COMMIT;",
        );
        if foreign_keys_were_on {
            connection.execute_batch("PRAGMA foreign_keys=ON;")?;
        }
        rebuilt?;
    }
    if version < 20 && target >= 20 {
        // v19 clients resolved segment-relative paths directly below the RAW root,
        // omitting the session directory. Those attempts failed before creating
        // any remote identity and were permanently rejected as SOURCE_MISSING.
        // Requeue only that exact local-only shape; remote rejections and changed
        // source bytes remain terminal.
        connection.execute_batch(
            "BEGIN IMMEDIATE;
             UPDATE upload_jobs
                SET state = 'retry_wait',
                    next_attempt_at_ms = 0,
                    updated_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now')
              WHERE state = 'rejected'
                AND last_error_code = 'UPLOAD_SOURCE_MISSING'
                AND remote_clip_id IS NULL
                AND clip_revision IS NULL
                AND remote_upload_intent_id IS NULL;
             UPDATE journal_meta SET schema_version = 20 WHERE singleton = 1;
             COMMIT;",
        )?;
    }
    if version < 21 && target >= 21 {
        // Manual upload policy: queue rows remain useful local preparation, but
        // no network-facing worker may claim them until the annotator explicitly
        // requests this session. DEFAULT 0 is intentional for both new rows and
        // every pre-v21 queue, including interrupted/retry/stored work.
        connection.execute_batch(
            "BEGIN IMMEDIATE;
             ALTER TABLE upload_sessions
                 ADD COLUMN upload_requested INTEGER NOT NULL DEFAULT 0
                 CHECK(upload_requested IN (0, 1));
             ALTER TABLE upload_sessions
                 ADD COLUMN upload_requested_at_utc TEXT;
             UPDATE journal_meta SET schema_version = 21 WHERE singleton = 1;
             COMMIT;",
        )?;
    }
    if version < 22 && target >= 22 {
        // Session seal is a session-scoped durable task. Keeping its retry
        // ledger on upload_sessions prevents N stored segments from each
        // issuing the same failing seal request. Remote retention is recorded
        // separately so a purged clip is not flattened into generic rejected.
        connection.execute_batch(
            "BEGIN IMMEDIATE;
             ALTER TABLE upload_sessions
                 ADD COLUMN seal_state TEXT NOT NULL DEFAULT 'pending'
                 CHECK(seal_state IN ('pending', 'running', 'retry_wait', 'sealed', 'rejected'));
             ALTER TABLE upload_sessions
                 ADD COLUMN seal_attempt_count INTEGER NOT NULL DEFAULT 0
                 CHECK(seal_attempt_count >= 0);
             ALTER TABLE upload_sessions
                 ADD COLUMN seal_next_attempt_at_ms INTEGER NOT NULL DEFAULT 0
                 CHECK(seal_next_attempt_at_ms >= 0);
             ALTER TABLE upload_sessions ADD COLUMN seal_last_error_code TEXT;
             ALTER TABLE upload_sessions ADD COLUMN seal_http_status INTEGER;
             ALTER TABLE upload_sessions ADD COLUMN seal_server_code TEXT;
             ALTER TABLE upload_sessions ADD COLUMN seal_server_message TEXT;
             ALTER TABLE upload_sessions ADD COLUMN seal_request_id TEXT;
             ALTER TABLE upload_jobs
                 ADD COLUMN status_attempt_count INTEGER NOT NULL DEFAULT 0
                 CHECK(status_attempt_count >= 0);
             ALTER TABLE upload_jobs ADD COLUMN remote_retention_state TEXT
                 CHECK(remote_retention_state IS NULL OR remote_retention_state IN (
                     'retained', 'purge_requested', 'purging', 'purged', 'purge_failed'
                 ));
             UPDATE upload_sessions
                SET seal_state = CASE WHEN sealed = 1 THEN 'sealed' ELSE 'pending' END;
             UPDATE journal_meta SET schema_version = 22 WHERE singleton = 1;
             COMMIT;",
        )?;
    }
    if version < 23 && target >= 23 {
        // Preserve the structured control-plane refusal that HttpTransport has
        // already decoded. Without these columns a durable rejected job kept
        // only UPLOAD_REQUEST_REJECTED and lost the HTTP status, server code,
        // safe bounded message and request correlation id needed for support.
        connection.execute_batch(
            "BEGIN IMMEDIATE;
             ALTER TABLE upload_jobs ADD COLUMN http_status INTEGER
                 CHECK(http_status IS NULL OR http_status BETWEEN 100 AND 599);
             ALTER TABLE upload_jobs ADD COLUMN server_code TEXT;
             ALTER TABLE upload_jobs ADD COLUMN server_message TEXT;
             ALTER TABLE upload_jobs ADD COLUMN request_id TEXT;
             UPDATE journal_meta SET schema_version = 23 WHERE singleton = 1;
             COMMIT;",
        )?;
    }
    if version < 24 && target >= 24 {
        // 兼容列：两列同时 NULL 是普通 no-depth 段；同时非空是本地 depth segment
        // manifest 的完成事实；单边永远损坏。v24 最初把它直接枚举进 production
        // uploader，v25 按 docs/77 §7 改为独立 durable queue，列本身继续保留。
        connection.execute_batch(
            "BEGIN IMMEDIATE;
             ALTER TABLE segments ADD COLUMN depth_path TEXT;
             ALTER TABLE segments ADD COLUMN depth_sha256 TEXT;
             UPDATE journal_meta SET schema_version = 24 WHERE singleton = 1;
             COMMIT;",
        )?;
    }
    if version < 25 && target >= 25 {
        // 历史裁决（docs/77）要求 depth-lab 与 production `upload_jobs` / API /
        // credentials 物理分流，因此这里建立了独立 durable queue。docs/107
        // 后正式 clip 改走 production uploader；v25 表仍留作迁移兼容/实验事实。
        connection.execute_batch(
            "BEGIN IMMEDIATE;
             CREATE TABLE depth_lab_segment_facts (
                segment_id TEXT PRIMARY KEY REFERENCES segments(id) ON DELETE CASCADE,
                session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                run_id TEXT NOT NULL REFERENCES capture_runs(id) ON DELETE CASCADE,
                generation INTEGER NOT NULL CHECK(generation >= 0),
                segment_seq INTEGER NOT NULL CHECK(segment_seq >= 0),
                manifest_path TEXT NOT NULL,
                manifest_sha256 TEXT NOT NULL,
                candidate_eligible INTEGER NOT NULL CHECK(candidate_eligible IN (0, 1)),
                invalid_frame_count INTEGER NOT NULL CHECK(invalid_frame_count >= 0),
                created_at_utc TEXT NOT NULL,
                CHECK(candidate_eligible = (invalid_frame_count = 0)),
                UNIQUE(session_id, run_id, generation, segment_seq)
             );
             CREATE TABLE depth_lab_jobs (
                segment_id TEXT PRIMARY KEY REFERENCES depth_lab_segment_facts(segment_id) ON DELETE CASCADE,
                session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                run_id TEXT NOT NULL REFERENCES capture_runs(id) ON DELETE CASCADE,
                generation INTEGER NOT NULL CHECK(generation >= 0),
                segment_seq INTEGER NOT NULL CHECK(segment_seq >= 0),
                state TEXT NOT NULL CHECK(state IN ('pending', 'running')),
                attempt_count INTEGER NOT NULL DEFAULT 0 CHECK(attempt_count >= 0),
                manifest_path TEXT NOT NULL,
                manifest_sha256 TEXT NOT NULL,
                created_at_utc TEXT NOT NULL,
                updated_at_utc TEXT NOT NULL,
                UNIQUE(session_id, run_id, generation, segment_seq)
             );
             CREATE INDEX idx_depth_lab_jobs_state
                 ON depth_lab_jobs(state, created_at_utc, segment_id);
             UPDATE journal_meta SET schema_version = 25 WHERE singleton = 1;
             COMMIT;",
        )?;
    }
    if version < 26 && target >= 26 {
        // U2 在 v25 的 segment candidate 之上增加 run 级可恢复编排。远端 scope、
        // multipart/manifest 进度都是独立 depth principal 的本地事实；不复用
        // production upload_jobs、artifact_objects 或它们的 credential/gate。
        connection.execute_batch(
            "BEGIN IMMEDIATE;
             ALTER TABLE depth_lab_jobs ADD COLUMN archive_job_id TEXT;
             CREATE TABLE depth_lab_archive_jobs (
                job_id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                run_id TEXT NOT NULL REFERENCES capture_runs(id) ON DELETE CASCADE,
                generation INTEGER NOT NULL CHECK(generation >= 0),
                state TEXT NOT NULL CHECK(state IN (
                    'pending', 'materialized', 'registered', 'uploading',
                    'manifest_authorized', 'retry_wait', 'finalized'
                )),
                attempt_count INTEGER NOT NULL DEFAULT 0 CHECK(attempt_count >= 0),
                next_attempt_at_ms INTEGER NOT NULL DEFAULT 0 CHECK(next_attempt_at_ms >= 0),
                lease_owner TEXT,
                lease_expires_at_ms INTEGER CHECK(lease_expires_at_ms IS NULL OR lease_expires_at_ms >= 0),
                materialized_json TEXT CHECK(materialized_json IS NULL OR json_valid(materialized_json)),
                remote_scope_json TEXT CHECK(remote_scope_json IS NULL OR json_valid(remote_scope_json)),
                progress_json TEXT CHECK(progress_json IS NULL OR json_valid(progress_json)),
                last_error_code TEXT,
                last_error_message TEXT,
                created_at_utc TEXT NOT NULL,
                updated_at_utc TEXT NOT NULL,
                CHECK((lease_owner IS NULL) = (lease_expires_at_ms IS NULL)),
                UNIQUE(session_id, run_id, generation)
             );
             CREATE INDEX idx_depth_lab_archive_jobs_processable
                 ON depth_lab_archive_jobs(state, next_attempt_at_ms, lease_expires_at_ms, created_at_utc);
             CREATE INDEX idx_depth_lab_jobs_archive
                 ON depth_lab_jobs(archive_job_id, segment_seq);
             UPDATE journal_meta SET schema_version = 26 WHERE singleton = 1;
             COMMIT;",
        )?;
    }
    if version < 27 && target >= 27 {
        // certified 车道废除（docs/100）。v19 把「non_trainable = 0 ⇔ catalog_mode =
        // 'certified' ⇔ consent 在场 ⇔ certified 词表」钉成三重死对；车道一走，这三条
        // 里每一条都在描述一个不可能达成的状态。本迁移把它们放宽成剩下那条车道**真正**
        // 成立的形状，并把 v18 放开的 'certified_pass' 从预检词表收回。
        //
        // 三条 CHECK 的处置，逐条：
        //   1) `(non_trainable = 0) = (catalog_mode = 'certified')` → 直接钉死
        //      `non_trainable = 1`。这不是放宽而是收紧：本地永远不主张可训练，
        //      可训练性只能是服务端 clip_trainability 的裁决（docs/100 §3）。
        //   2) `non_trainable = 1 OR consent 非空` → **删**。consent 通路保留、值恒
        //      NULL（§3 推论 3），下一刀接账号/设备登记级的新来源；那时一行
        //      non_trainable=1 且带 consent 是完全正常的形状，这条 CHECK 会挡住它。
        //   3) `schema_version LIKE 'dm-certified-%' ⇔ certified mode` → 换成
        //      **禁止** certified 词表：那套世代随 audio_contract 的四值退两值一起
        //      退役，任何 dm-certified-* 行都是不该存在的字节。
        //
        // ⚠ 存量 certified 行：**明确失败，不静默改写**。本地实验机可能留着真机测试
        // 的残留，把它们的 non_trainable 从 0 改成 1 是在替人做一个数据决定——那些
        // 段录制时的标签是什么，只有人知道该怎么处置。照 v16/v19 的先例本可以删，
        // 但删除和改写在这里都不对：v19 删的是「永远传不上去的过渡态行」，而这些行
        // 曾是一条完整接通的车道的产物。
        let stale_certified: i64 = connection.query_row(
            "SELECT count(*) FROM sessions
              WHERE catalog_mode = 'certified'
                 OR non_trainable = 0
                 OR schema_version LIKE 'dm-certified-%'",
            [],
            |row| row.get(0),
        )?;
        anyhow::ensure!(
            stale_certified == 0,
            "journal migration 27 refuses to rewrite {stale_certified} certified session row(s). \
             The certified lane was removed (docs/100); these rows predate it and their \
             classification is a data decision, not a migration's. Archive or delete this \
             journal (and the RAW artifacts it indexes) explicitly, then retry."
        );
        let stale_preflight: i64 = connection.query_row(
            "SELECT count(*) FROM preflight_attempts
              WHERE state = 'certified_pass' OR catalog_mode = 'certified'",
            [],
            |row| row.get(0),
        )?;
        anyhow::ensure!(
            stale_preflight == 0,
            "journal migration 27 refuses to rewrite {stale_preflight} certified preflight \
             attempt(s). See the sessions message above; the same decision applies."
        );

        // SQLite 改不了 CHECK，按 v13/v14/v16/v18/v19 先例整表重建：**逐字拷贝上一版
        // 的形状**，只动上述约束（v14 的教训：照记忆手写会丢约束）。sessions 被多张表
        // FK 引用，照 v16/v19 关 FK 重建（pragma 在事务内是 no-op，必须放事务外）；
        // preflight_attempts 既不引用别人也没人引用它，不需要。
        let foreign_keys_were_on: bool =
            connection.query_row("PRAGMA foreign_keys", [], |row| row.get(0))?;
        connection.execute_batch("PRAGMA foreign_keys=OFF;")?;
        let rebuilt = connection.execute_batch(
            "BEGIN IMMEDIATE;
             CREATE TABLE sessions_v27 (
                id TEXT PRIMARY KEY,
                state TEXT NOT NULL CHECK(state IN ('active', 'paused', 'completed', 'faulted', 'corrupt')),
                non_trainable INTEGER NOT NULL CHECK(non_trainable = 1),
                production_certified INTEGER NOT NULL CHECK(production_certified = 0),
                annotator_id TEXT,
                device_id TEXT,
                consent_id TEXT,
                game_id TEXT NOT NULL,
                game_exe_hash TEXT,
                recording_profile_id TEXT NOT NULL,
                recording_profile_hash TEXT NOT NULL,
                mapping_id TEXT NOT NULL,
                mapping_version TEXT NOT NULL,
                mapping_hash TEXT NOT NULL,
                schema_version TEXT NOT NULL CHECK(schema_version NOT LIKE 'dm-certified-%'),
                started_at_utc TEXT NOT NULL,
                ended_at_utc TEXT,
                stop_reason TEXT,
                last_error_code TEXT,
                created_at_utc TEXT NOT NULL,
                updated_at_utc TEXT NOT NULL,
                catalog_mode TEXT NOT NULL DEFAULT 'legacy_dev_non_trainable',
                catalog_id TEXT NOT NULL DEFAULT 'legacy-unversioned',
                catalog_version TEXT NOT NULL DEFAULT 'legacy',
                catalog_hash TEXT NOT NULL DEFAULT 'legacy-unavailable',
                device_profile_id TEXT NOT NULL DEFAULT 'legacy-unavailable',
                device_profile_version TEXT NOT NULL DEFAULT 'legacy',
                device_profile_hash TEXT NOT NULL DEFAULT 'legacy-unavailable',
                recording_profile_version TEXT NOT NULL DEFAULT 'legacy',
                game_profile_version TEXT NOT NULL DEFAULT 'legacy',
                game_profile_hash TEXT NOT NULL DEFAULT 'legacy-unavailable',
                recording_profile_hash_kind TEXT NOT NULL DEFAULT 'legacy_reencoded_json_sha256',
                game_exe_hash_missing_reason TEXT,
                CHECK(catalog_mode <> 'certified'),
                CHECK((game_exe_hash IS NULL) = (game_exe_hash_missing_reason IS NOT NULL))
             );
             -- 显式列名而不是 SELECT *：列序相同是巧合不是契约（v9->v10 栽过一次）。
             INSERT INTO sessions_v27(
                id, state, non_trainable, production_certified, annotator_id, device_id,
                consent_id, game_id, game_exe_hash, game_exe_hash_missing_reason,
                recording_profile_id, recording_profile_hash, recording_profile_hash_kind,
                recording_profile_version, mapping_id, mapping_version, mapping_hash,
                catalog_mode, catalog_id, catalog_version, catalog_hash,
                device_profile_id, device_profile_version, device_profile_hash,
                game_profile_version, game_profile_hash,
                schema_version, started_at_utc, ended_at_utc, stop_reason, last_error_code,
                created_at_utc, updated_at_utc)
             SELECT
                id, state, non_trainable, production_certified, annotator_id, device_id,
                consent_id, game_id, game_exe_hash, game_exe_hash_missing_reason,
                recording_profile_id, recording_profile_hash, recording_profile_hash_kind,
                recording_profile_version, mapping_id, mapping_version, mapping_hash,
                catalog_mode, catalog_id, catalog_version, catalog_hash,
                device_profile_id, device_profile_version, device_profile_hash,
                game_profile_version, game_profile_hash,
                schema_version, started_at_utc, ended_at_utc, stop_reason, last_error_code,
                created_at_utc, updated_at_utc
               FROM sessions;
             DROP TABLE sessions;
             ALTER TABLE sessions_v27 RENAME TO sessions;

             CREATE TABLE preflight_attempts_v27 (
                id TEXT PRIMARY KEY,
                request_id TEXT NOT NULL UNIQUE,
                state TEXT NOT NULL CHECK(state IN (
                    'running', 'dev_non_trainable_pass', 'blocked', 'failed')),
                phase TEXT NOT NULL,
                catalog_mode TEXT NOT NULL CHECK(catalog_mode = 'dev_non_trainable'),
                target_window_id TEXT NOT NULL,
                game_id TEXT NOT NULL,
                catalog_id TEXT NOT NULL,
                catalog_version TEXT NOT NULL,
                catalog_hash TEXT NOT NULL,
                device_profile_id TEXT NOT NULL,
                device_profile_version TEXT NOT NULL,
                device_profile_hash TEXT NOT NULL,
                recording_profile_id TEXT NOT NULL,
                recording_profile_version TEXT NOT NULL,
                recording_profile_hash TEXT NOT NULL,
                game_profile_version TEXT NOT NULL,
                game_profile_hash TEXT NOT NULL,
                mapping_id TEXT NOT NULL,
                mapping_version TEXT NOT NULL,
                mapping_hash TEXT NOT NULL,
                session_id TEXT,
                run_name TEXT,
                failure_code TEXT,
                details_id TEXT,
                evidence_json TEXT NOT NULL,
                evidence_hash TEXT NOT NULL,
                created_at_utc TEXT NOT NULL,
                completed_at_utc TEXT,
                updated_at_utc TEXT NOT NULL,
                CHECK((session_id IS NULL AND run_name IS NULL)
                   OR (session_id IS NOT NULL AND run_name IS NOT NULL)),
                CHECK((state IN ('blocked', 'failed') AND failure_code IS NOT NULL AND details_id IS NOT NULL)
                   OR (state NOT IN ('blocked', 'failed') AND failure_code IS NULL AND details_id IS NULL))
             );
             INSERT INTO preflight_attempts_v27(
                id, request_id, state, phase, catalog_mode, target_window_id, game_id,
                catalog_id, catalog_version, catalog_hash,
                device_profile_id, device_profile_version, device_profile_hash,
                recording_profile_id, recording_profile_version, recording_profile_hash,
                game_profile_version, game_profile_hash,
                mapping_id, mapping_version, mapping_hash,
                session_id, run_name, failure_code, details_id,
                evidence_json, evidence_hash, created_at_utc, completed_at_utc, updated_at_utc)
             SELECT
                id, request_id, state, phase, catalog_mode, target_window_id, game_id,
                catalog_id, catalog_version, catalog_hash,
                device_profile_id, device_profile_version, device_profile_hash,
                recording_profile_id, recording_profile_version, recording_profile_hash,
                game_profile_version, game_profile_hash,
                mapping_id, mapping_version, mapping_hash,
                session_id, run_name, failure_code, details_id,
                evidence_json, evidence_hash, created_at_utc, completed_at_utc, updated_at_utc
               FROM preflight_attempts;
             DROP TABLE preflight_attempts;
             ALTER TABLE preflight_attempts_v27 RENAME TO preflight_attempts;
             CREATE INDEX IF NOT EXISTS idx_preflight_created
                 ON preflight_attempts(created_at_utc DESC, id DESC);
             CREATE INDEX IF NOT EXISTS idx_preflight_session
                 ON preflight_attempts(session_id, run_name);
             UPDATE journal_meta SET schema_version = 27 WHERE singleton = 1;
             COMMIT;",
        );
        if foreign_keys_were_on {
            connection.execute_batch("PRAGMA foreign_keys=ON;")?;
        }
        rebuilt?;
    }
    if version < 28 && target >= 28 {
        // docs/107 推翻独立 depth bucket 后，`depth_path/depth_sha256` 仍只表示
        // #529 的 durable segment manifest，不能拿它冒充含像素的上传对象。新增一对
        // 列单独钉住确定性 ZIP；老库升级后保持 NULL，必须重新通过 finalizer 复核并
        // 物化 ZIP 才能进入 production uploader，绝不从 manifest 猜字节或补零。
        // `upload_parts` 的种类 CHECK 也要同步扩到第七件；否则候选和
        // upload intent 都已带 depth，第一片 ZIP 的 ETag 却会在落 journal 时被
        // v17 的六件词表拒绝，重启也无从恢复。
        connection.execute_batch(
            "BEGIN IMMEDIATE;
             ALTER TABLE segments ADD COLUMN depth_upload_path TEXT;
             ALTER TABLE segments ADD COLUMN depth_upload_sha256 TEXT;
             ALTER TABLE capture_runs ADD COLUMN depth_expected INTEGER NOT NULL DEFAULT 0
                 CHECK(depth_expected IN (0, 1));
             UPDATE capture_runs
                SET depth_expected = 1
              WHERE EXISTS (
                    SELECT 1 FROM segments g
                     WHERE g.run_id = capture_runs.id AND g.depth_path IS NOT NULL
              );
             CREATE TABLE upload_parts_v28 (
                segment_id TEXT NOT NULL REFERENCES upload_jobs(segment_id) ON DELETE CASCADE,
                artifact_kind TEXT NOT NULL CHECK(artifact_kind IN (
                    'video', 'frames', 'events', 'meta', 'manifest', 'audio', 'depth'
                )),
                part_number INTEGER NOT NULL CHECK(part_number BETWEEN 1 AND 10000),
                byte_offset INTEGER NOT NULL CHECK(byte_offset >= 0),
                byte_size INTEGER NOT NULL CHECK(byte_size > 0),
                content_md5 TEXT NOT NULL,
                etag TEXT NOT NULL,
                uploaded_at_utc TEXT NOT NULL,
                PRIMARY KEY(segment_id, artifact_kind, part_number)
             );
             INSERT INTO upload_parts_v28(
                segment_id, artifact_kind, part_number, byte_offset, byte_size,
                content_md5, etag, uploaded_at_utc)
             SELECT segment_id, artifact_kind, part_number, byte_offset, byte_size,
                    content_md5, etag, uploaded_at_utc
               FROM upload_parts;
             DROP TABLE upload_parts;
             ALTER TABLE upload_parts_v28 RENAME TO upload_parts;
             UPDATE journal_meta SET schema_version = 28 WHERE singleton = 1;
             COMMIT;",
        )?;
    }
    if version < 29 && target >= 29 {
        // #451：Started 事件中的墙钟锚点必须在首个 segment ready 前
        // 持久化，让增量 DataModel 与终态 run.json 回放使用同一个锚点。
        // 旧库没有该事实，因此列必须可空，不伪造补值。
        connection.execute_batch(
            "BEGIN IMMEDIATE;
             ALTER TABLE capture_runs ADD COLUMN anchor_at_utc TEXT;
             UPDATE journal_meta SET schema_version = 29 WHERE singleton = 1;
             COMMIT;",
        )?;
    }
    if version < 30 && target >= 30 {
        // 性能认证三档共用正常录制会话，但只能在本机生成性能证据。
        // 把不可交付事实钉进 run identity，避免仅靠目录标记或 UI 状态。
        connection.execute_batch(
            "BEGIN IMMEDIATE;
             ALTER TABLE capture_runs
                 ADD COLUMN performance_validation_only INTEGER NOT NULL DEFAULT 0
                 CHECK(performance_validation_only IN (0, 1));
             UPDATE journal_meta SET schema_version = 30 WHERE singleton = 1;
             COMMIT;",
        )?;
    }
    if version < 31 && target >= 31 {
        // #758：行为遥测与账号环境共享凭据边界。v11 的单一 outbox/cursor
        // 没有环境事实，切换 staging/production 后会把旧环境积压交给新环境的
        // device token。迁移不能猜历史行属于 production：全部标成 unknown，
        // 保留且永不被任一已选环境的 drain 查询命中。
        connection.execute_batch(
            "BEGIN IMMEDIATE;
             ALTER TABLE client_event_outbox ADD COLUMN account_environment TEXT NOT NULL
                 DEFAULT 'unknown'
                 CHECK(account_environment IN ('unknown', 'staging', 'production'));
             CREATE INDEX idx_client_event_outbox_environment_seq
                 ON client_event_outbox(account_environment, seq);

             CREATE TABLE client_event_cursor_v30 (
                account_environment TEXT PRIMARY KEY
                    CHECK(account_environment IN ('unknown', 'staging', 'production')),
                acked_seq INTEGER NOT NULL DEFAULT 0 CHECK(acked_seq >= 0),
                attempt_count INTEGER NOT NULL DEFAULT 0 CHECK(attempt_count >= 0),
                dropped_events INTEGER NOT NULL DEFAULT 0 CHECK(dropped_events >= 0),
                last_error TEXT,
                last_attempt_at_utc TEXT,
                updated_at_utc TEXT NOT NULL
             );
             INSERT INTO client_event_cursor_v30(
                account_environment, acked_seq, attempt_count, dropped_events,
                last_error, last_attempt_at_utc, updated_at_utc)
             SELECT 'unknown', acked_seq, attempt_count, dropped_events,
                    'CLIENT_EVENTS_ENVIRONMENT_UNKNOWN', last_attempt_at_utc, updated_at_utc
               FROM client_event_cursor WHERE singleton = 1;
             INSERT INTO client_event_cursor_v30(account_environment, updated_at_utc)
             VALUES
                ('staging', strftime('%Y-%m-%dT%H:%M:%fZ','now')),
                ('production', strftime('%Y-%m-%dT%H:%M:%fZ','now'));
             DROP TABLE client_event_cursor;
             ALTER TABLE client_event_cursor_v30 RENAME TO client_event_cursor;
             UPDATE journal_meta SET schema_version = 31 WHERE singleton = 1;
             COMMIT;",
        )?;
    }
    if version < 32 && target >= 32 {
        // CP2077 DEV raw D32 remains local/non-trainable, but its five streams
        // still need one durable identity with the formal collector run. Keep
        // this fact in its own table: no trigger or foreign key touches the R2
        // upload tables, so local sealing cannot alter another task's queue.
        connection.execute_batch(
            "BEGIN IMMEDIATE;
             CREATE TABLE cp2077_main_run_facts (
                run_id TEXT PRIMARY KEY REFERENCES capture_runs(id) ON DELETE RESTRICT,
                session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE RESTRICT,
                generation INTEGER NOT NULL CHECK(generation > 0),
                process_id INTEGER NOT NULL CHECK(process_id > 0),
                process_launch_id TEXT NOT NULL CHECK(length(process_launch_id) = 32),
                result_path TEXT NOT NULL CHECK(result_path = 'cp2077-main-result.json'),
                result_sha256 TEXT NOT NULL,
                sidecar_locator TEXT NOT NULL,
                delivery_locator TEXT NOT NULL,
                rgb_frames INTEGER NOT NULL CHECK(rgb_frames > 0),
                source_depth_frames INTEGER NOT NULL CHECK(source_depth_frames > 0),
                paired_depth_frames INTEGER NOT NULL CHECK(paired_depth_frames > 0),
                duration_ms INTEGER NOT NULL CHECK(duration_ms > 0),
                capture_queue_drops INTEGER NOT NULL CHECK(capture_queue_drops = 0),
                input_queue_overflows INTEGER NOT NULL CHECK(input_queue_overflows = 0),
                exact_present_matches INTEGER NOT NULL CHECK(exact_present_matches > 0),
                exact_present_pairing_rate REAL NOT NULL
                    CHECK(exact_present_pairing_rate >= 0.98 AND exact_present_pairing_rate <= 1.0),
                raw_input_events INTEGER NOT NULL CHECK(raw_input_events > 0),
                keyboard_input_events INTEGER NOT NULL CHECK(keyboard_input_events >= 0),
                mouse_input_events INTEGER NOT NULL CHECK(mouse_input_events >= 0),
                player_pose_samples INTEGER NOT NULL CHECK(player_pose_samples > 0),
                render_camera_samples INTEGER NOT NULL CHECK(render_camera_samples > 0),
                confirmed_flush INTEGER NOT NULL CHECK(confirmed_flush = 1),
                non_trainable INTEGER NOT NULL CHECK(non_trainable = 1),
                upload_allowed INTEGER NOT NULL CHECK(upload_allowed = 0),
                created_at_utc TEXT NOT NULL,
                UNIQUE(session_id, process_launch_id)
             );
             CREATE INDEX cp2077_main_run_facts_session_idx
                 ON cp2077_main_run_facts(session_id, run_id);
             UPDATE journal_meta SET schema_version = 32 WHERE singleton = 1;
             COMMIT;",
        )?;
    }
    if version < 33 && target >= 33 {
        // issue #855 §A.4 / §B：本地 journal 退掉 `catalog_mode` 这个词，换成
        // **摄取契约**（`session_contract`），并让 preflight 终态词表容得下中性
        // 那一代的 `preflight_pass`。
        //
        // 为什么是换列而不是加列：`catalog_mode` 和 `session_contract` 描述的是同
        // 一件事的两种说法——"这一场按哪一套词表落盘与上传"。两列并存意味着每个
        // 读者都要决定信哪一个，而这正是本 issue 反复在拆的形状。
        //
        // ── 旧行怎么处置 ────────────────────────────────────────────────────
        //
        // 逐值映射，**不重新分类任何一场采集**：
        //
        //   `dev_non_trainable`        → `dm-local-dev`
        //   其余（`legacy_dev_non_trainable`）→ `legacy`
        //
        // 第一条不是改写语义：那些行的上传信封本来就一直申报
        // `sourceFamily: "dm-local-dev"`（uploader 的 `classification_for`），
        // 新值逐字就是它。它们继续走 `raw/v1/non-trainable/...`、继续永久
        // non-trainable，一行都不会变成 candidate。
        //
        // 第二条是 v19 之前建的行——那时这一列还不存在，是迁移补的默认值，没有
        // 目录证据。它们保留为 `legacy` 且**故意解析不出契约**
        // （`SessionContract::from_source_family` 不认这个词），于是 recovery 对
        // 它们的判定与断代前逐字相同：`verify_session_catalog_snapshots` 从前要求
        // `catalog_mode == 'dev_non_trainable'`，这些行本来就过不了。
        //
        // ── 落盘产物一个字节都没动 ──────────────────────────────────────────
        //
        // 已经躺在磁盘上的 legacy `session.json` / `meta.json` 里仍然写着
        // `catalog.mode = "dev_non_trainable"`。那是**产物的**词，不是这一列的词，
        // 读侧改成拿契约的 `legacy_catalog_mode()` 去比（`artifact_catalog_matches_journal`、
        // recovery 的 `catalog_matches_session`），所以存量段的校验结果不变。
        //
        // preflight 证据的 `evidence_hash` 同理：它按 `preflight::hash` 那张 map 算，
        // 而那张 map 的 legacy 那一格仍然逐字写 `dev_non_trainable`。换了列不换哈希。
        //
        // SQLite 改不了列名带 CHECK 的表，照 v13/v14/v16/v19/v27 先例整表重建；
        // sessions 被多张表 FK 引用，同样关 FK 重建（pragma 在事务内是 no-op）。
        let foreign_keys_were_on: bool =
            connection.query_row("PRAGMA foreign_keys", [], |row| row.get(0))?;
        connection.execute_batch("PRAGMA foreign_keys=OFF;")?;
        let rebuilt = connection.execute_batch(
            "BEGIN IMMEDIATE;
             CREATE TABLE sessions_v33 (
                id TEXT PRIMARY KEY,
                state TEXT NOT NULL CHECK(state IN ('active', 'paused', 'completed', 'faulted', 'corrupt')),
                non_trainable INTEGER NOT NULL CHECK(non_trainable = 1),
                production_certified INTEGER NOT NULL CHECK(production_certified = 0),
                annotator_id TEXT,
                device_id TEXT,
                consent_id TEXT,
                game_id TEXT NOT NULL,
                game_exe_hash TEXT,
                recording_profile_id TEXT NOT NULL,
                recording_profile_hash TEXT NOT NULL,
                mapping_id TEXT NOT NULL,
                mapping_version TEXT NOT NULL,
                mapping_hash TEXT NOT NULL,
                schema_version TEXT NOT NULL CHECK(schema_version NOT LIKE 'dm-certified-%'),
                started_at_utc TEXT NOT NULL,
                ended_at_utc TEXT,
                stop_reason TEXT,
                last_error_code TEXT,
                created_at_utc TEXT NOT NULL,
                updated_at_utc TEXT NOT NULL,
                -- 默认值给的是历史行的那一格。新写入永远显式带值（begin_run 传
                -- `SessionContract::source_family()`），默认只服务于迁移本身。
                session_contract TEXT NOT NULL DEFAULT 'legacy'
                    CHECK(session_contract IN ('legacy', 'dm-local-dev', 'dm-session-ingest')),
                catalog_id TEXT NOT NULL DEFAULT 'legacy-unversioned',
                catalog_version TEXT NOT NULL DEFAULT 'legacy',
                catalog_hash TEXT NOT NULL DEFAULT 'legacy-unavailable',
                device_profile_id TEXT NOT NULL DEFAULT 'legacy-unavailable',
                device_profile_version TEXT NOT NULL DEFAULT 'legacy',
                device_profile_hash TEXT NOT NULL DEFAULT 'legacy-unavailable',
                recording_profile_version TEXT NOT NULL DEFAULT 'legacy',
                game_profile_version TEXT NOT NULL DEFAULT 'legacy',
                game_profile_hash TEXT NOT NULL DEFAULT 'legacy-unavailable',
                recording_profile_hash_kind TEXT NOT NULL DEFAULT 'legacy_reencoded_json_sha256',
                game_exe_hash_missing_reason TEXT,
                -- 契约与 session schema 必须成对。这是 issue #855 §B 的路由键
                -- 不变量在本地这一侧的落点，与服务端迁移 0036 的 CHECK 同源：
                -- 一条自称 v2 的会话不能带 legacy 的 schema，反之亦然。
                CHECK(
                    (session_contract = 'dm-session-ingest')
                        = (schema_version = 'dm-session-ingest-2')
                ),
                CHECK((game_exe_hash IS NULL) = (game_exe_hash_missing_reason IS NOT NULL))
             );
             -- 显式列名而不是 SELECT *：列序相同是巧合不是契约（v9->v10 栽过一次）。
             INSERT INTO sessions_v33(
                id, state, non_trainable, production_certified, annotator_id, device_id,
                consent_id, game_id, game_exe_hash, game_exe_hash_missing_reason,
                recording_profile_id, recording_profile_hash, recording_profile_hash_kind,
                recording_profile_version, mapping_id, mapping_version, mapping_hash,
                session_contract, catalog_id, catalog_version, catalog_hash,
                device_profile_id, device_profile_version, device_profile_hash,
                game_profile_version, game_profile_hash,
                schema_version, started_at_utc, ended_at_utc, stop_reason, last_error_code,
                created_at_utc, updated_at_utc)
             SELECT
                id, state, non_trainable, production_certified, annotator_id, device_id,
                consent_id, game_id, game_exe_hash, game_exe_hash_missing_reason,
                recording_profile_id, recording_profile_hash, recording_profile_hash_kind,
                recording_profile_version, mapping_id, mapping_version, mapping_hash,
                CASE catalog_mode WHEN 'dev_non_trainable' THEN 'dm-local-dev' ELSE 'legacy' END,
                catalog_id, catalog_version, catalog_hash,
                device_profile_id, device_profile_version, device_profile_hash,
                game_profile_version, game_profile_hash,
                schema_version, started_at_utc, ended_at_utc, stop_reason, last_error_code,
                created_at_utc, updated_at_utc
               FROM sessions;
             DROP TABLE sessions;
             ALTER TABLE sessions_v33 RENAME TO sessions;

             CREATE TABLE preflight_attempts_v33 (
                id TEXT PRIMARY KEY,
                request_id TEXT NOT NULL UNIQUE,
                -- 中性那一代的终态是 `preflight_pass`：旧值把车道名写进了结果里，
                -- 而它只回答\"preflight 过了没有\"。两个值同时合法，**配对**由
                -- `preflight::pass_result_for` 与服务端迁移 0036 的触发器强制。
                state TEXT NOT NULL CHECK(state IN (
                    'running', 'dev_non_trainable_pass', 'preflight_pass', 'blocked', 'failed')),
                phase TEXT NOT NULL,
                session_contract TEXT NOT NULL
                    CHECK(session_contract IN ('legacy', 'dm-local-dev', 'dm-session-ingest')),
                target_window_id TEXT NOT NULL,
                game_id TEXT NOT NULL,
                catalog_id TEXT NOT NULL,
                catalog_version TEXT NOT NULL,
                catalog_hash TEXT NOT NULL,
                device_profile_id TEXT NOT NULL,
                device_profile_version TEXT NOT NULL,
                device_profile_hash TEXT NOT NULL,
                recording_profile_id TEXT NOT NULL,
                recording_profile_version TEXT NOT NULL,
                recording_profile_hash TEXT NOT NULL,
                game_profile_version TEXT NOT NULL,
                game_profile_hash TEXT NOT NULL,
                mapping_id TEXT NOT NULL,
                mapping_version TEXT NOT NULL,
                mapping_hash TEXT NOT NULL,
                session_id TEXT,
                run_name TEXT,
                failure_code TEXT,
                details_id TEXT,
                evidence_json TEXT NOT NULL,
                evidence_hash TEXT NOT NULL,
                created_at_utc TEXT NOT NULL,
                completed_at_utc TEXT,
                updated_at_utc TEXT NOT NULL,
                CHECK((session_id IS NULL AND run_name IS NULL)
                   OR (session_id IS NOT NULL AND run_name IS NOT NULL)),
                CHECK((state IN ('blocked', 'failed') AND failure_code IS NOT NULL AND details_id IS NOT NULL)
                   OR (state NOT IN ('blocked', 'failed') AND failure_code IS NULL AND details_id IS NULL))
             );
             INSERT INTO preflight_attempts_v33(
                id, request_id, state, phase, session_contract, target_window_id, game_id,
                catalog_id, catalog_version, catalog_hash,
                device_profile_id, device_profile_version, device_profile_hash,
                recording_profile_id, recording_profile_version, recording_profile_hash,
                game_profile_version, game_profile_hash,
                mapping_id, mapping_version, mapping_hash,
                session_id, run_name, failure_code, details_id,
                evidence_json, evidence_hash, created_at_utc, completed_at_utc, updated_at_utc)
             SELECT
                id, request_id, state, phase,
                CASE catalog_mode WHEN 'dev_non_trainable' THEN 'dm-local-dev' ELSE 'legacy' END,
                target_window_id, game_id,
                catalog_id, catalog_version, catalog_hash,
                device_profile_id, device_profile_version, device_profile_hash,
                recording_profile_id, recording_profile_version, recording_profile_hash,
                game_profile_version, game_profile_hash,
                mapping_id, mapping_version, mapping_hash,
                session_id, run_name, failure_code, details_id,
                evidence_json, evidence_hash, created_at_utc, completed_at_utc, updated_at_utc
               FROM preflight_attempts;
             DROP TABLE preflight_attempts;
             ALTER TABLE preflight_attempts_v33 RENAME TO preflight_attempts;
             CREATE INDEX IF NOT EXISTS idx_preflight_created
                 ON preflight_attempts(created_at_utc DESC, id DESC);
             CREATE INDEX IF NOT EXISTS idx_preflight_session
                 ON preflight_attempts(session_id, run_name);
             UPDATE journal_meta SET schema_version = 33 WHERE singleton = 1;
             COMMIT;",
        );
        if foreign_keys_were_on {
            connection.execute_batch("PRAGMA foreign_keys=ON;")?;
        }
        rebuilt?;
    }
    if version < 34 && target >= 34 {
        // A provider multipart can disappear after CompleteMultipartUpload fails
        // without producing the final object. The API then advances only that
        // artifact's opaque attempt number. Persist the generation beside each
        // ETag so a retry re-uploads the affected artifact instead of replaying
        // stale ETags into the replacement provider upload.
        //
        // v0.7.18 permanently rejected the follow-up 409 because the old API
        // marked `multipart_completion_in_progress` non-retryable. Revive only
        // that exact structured failure; all other rejected jobs stay terminal.
        connection.execute_batch(
            "BEGIN IMMEDIATE;
             ALTER TABLE upload_parts
                 ADD COLUMN provider_attempt_no INTEGER NOT NULL DEFAULT 1
                 CHECK(provider_attempt_no > 0);
             UPDATE upload_jobs
                SET state = 'retry_wait', next_attempt_at_ms = 0,
                    updated_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now')
              WHERE state = 'rejected'
                AND last_error_code = 'UPLOAD_REQUEST_REJECTED'
                AND http_status = 409
                AND server_code = 'multipart_completion_in_progress';
             UPDATE journal_meta SET schema_version = 34 WHERE singleton = 1;
             COMMIT;",
        )?;
    }
    let migrated_version: i64 = connection.query_row(
        "SELECT schema_version FROM journal_meta WHERE singleton = 1",
        [],
        |row| row.get(0),
    )?;
    ensure!(
        migrated_version == target,
        "SQLite journal migration stopped at version {migrated_version} (target {target})"
    );
    Ok(())
}

struct DataModelJobRow {
    session_id: String,
    state: String,
    ready_segments: u32,
    processed_segments: u32,
    published_chunks: u32,
    published_frames: u64,
    pending_frames: u64,
    finalize_requested: i64,
    output_relative_path: Option<String>,
    error_code: Option<String>,
}

fn data_model_job_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<DataModelJobRow> {
    Ok(DataModelJobRow {
        session_id: row.get(0)?,
        state: row.get(1)?,
        ready_segments: row.get(2)?,
        processed_segments: row.get(3)?,
        published_chunks: row.get(4)?,
        published_frames: row.get(5)?,
        pending_frames: row.get(6)?,
        finalize_requested: row.get(7)?,
        output_relative_path: row.get(8)?,
        error_code: row.get(9)?,
    })
}

fn data_model_job_from_sql(row: DataModelJobRow) -> anyhow::Result<DataModelJobStatus> {
    ensure!(
        matches!(row.finalize_requested, 0 | 1),
        "invalid DataModel finalize flag {}",
        row.finalize_requested
    );
    Ok(DataModelJobStatus {
        session_id: row.session_id,
        state: DataModelJobState::parse(&row.state)?,
        ready_segments: row.ready_segments,
        processed_segments: row.processed_segments,
        published_chunks: row.published_chunks,
        published_frames: row.published_frames,
        pending_frames: row.pending_frames,
        finalize_requested: row.finalize_requested == 1,
        output_relative_path: row.output_relative_path,
        error_code: row.error_code,
    })
}

fn load_data_model_job(
    connection: &Connection,
    session_id: &str,
) -> anyhow::Result<Option<DataModelJobStatus>> {
    connection
        .query_row(
            "SELECT session_id, state, ready_segments, processed_segments,
                    published_chunks, published_frames, pending_frames,
                    finalize_requested, output_relative_path, error_code
             FROM data_model_jobs WHERE session_id = ?1",
            params![session_id],
            data_model_job_from_row,
        )
        .optional()?
        .map(data_model_job_from_sql)
        .transpose()
}

fn ensure_data_model_job(tx: &Transaction<'_>, session_id: &str) -> anyhow::Result<()> {
    tx.execute(
        "INSERT INTO data_model_jobs (
            session_id, state, ready_segments, processed_segments,
            published_chunks, published_frames, pending_frames,
            finalize_requested, created_at_utc, updated_at_utc
         ) VALUES (
            ?1, 'waiting', 0, 0, 0, 0, 0, 0,
            strftime('%Y-%m-%dT%H:%M:%fZ','now'),
            strftime('%Y-%m-%dT%H:%M:%fZ','now')
         ) ON CONFLICT(session_id) DO NOTHING",
        params![session_id],
    )?;
    Ok(())
}

/// 把一个刚提交为 ready 的 segment 投影进镜像 outbox。
///
/// payload **从刚写入的行里 SELECT 出来**，而不是从调用方的结构体里拼。
/// 这条 SELECT 的列清单本身就是脱敏白名单：路径、窗口标题、PID、原始输入
/// 都不在里面，因此它们进不来——不需要再写一个「记得别放 X」的过滤器，
/// 也就不存在那个过滤器漏掉新字段的可能。
fn enqueue_mirror_segment(
    tx: &Transaction<'_>,
    session_id: &str,
    run_id: &str,
    segment_index: u32,
) -> anyhow::Result<()> {
    let id = segment_journal_id(session_id, run_id, segment_index);
    let entity_id = format!("{session_id}:{run_id}:{segment_index}");
    let changed = tx.execute(
        "INSERT INTO mirror_outbox(entity, entity_id, payload_json, observed_at_ms, created_at_utc)
         SELECT 'segment',
                ?2,
                json_object(
                    'localSessionId', ?3,
                    'localRunId', ?4,
                    'segmentIndex', g.seq,
                    'localState', g.state,
                    'nFrames', g.n_frames,
                    'totalBytes', g.bytes,
                     'clientUploadState', 'unknown'
                ),
                CAST(strftime('%s','now') AS INTEGER) * 1000,
                strftime('%Y-%m-%dT%H:%M:%fZ','now')
         FROM segments g
         WHERE g.id = ?1",
        params![id, entity_id, session_id, run_id],
    )?;
    ensure!(
        changed == 1,
        "mirror enqueue did not observe exactly one ready segment"
    );
    Ok(())
}

fn queue_data_model_segment(tx: &Transaction<'_>, session_id: &str) -> anyhow::Result<()> {
    ensure_data_model_job(tx, session_id)?;
    let changed = tx.execute(
        "UPDATE data_model_jobs
         SET ready_segments = (
                 SELECT COUNT(*) FROM segments
                 WHERE session_id = ?1 AND state = 'ready'
             ),
             state = CASE
                 WHEN state IN ('completed', 'cancelled', 'running', 'finalizing') THEN state
                 WHEN (SELECT COUNT(*) FROM segments
                       WHERE session_id = ?1 AND state = 'ready') > processed_segments
                     THEN 'pending'
                 ELSE state
             END,
             error_code = CASE
                 WHEN (SELECT COUNT(*) FROM segments
                       WHERE session_id = ?1 AND state = 'ready') > ready_segments
                     THEN NULL
                 ELSE error_code
             END,
             updated_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now')
         WHERE session_id = ?1",
        params![session_id],
    )?;
    ensure!(
        changed == 1,
        "ready segment did not queue one DataModel job"
    );
    Ok(())
}

fn request_data_model_finalize(tx: &Transaction<'_>, session_id: &str) -> anyhow::Result<()> {
    ensure_data_model_job(tx, session_id)?;
    let changed = tx.execute(
        "UPDATE data_model_jobs
         SET finalize_requested = 1,
             state = CASE
                 WHEN state IN ('completed', 'cancelled', 'running', 'finalizing') THEN state
                 ELSE 'pending'
             END,
             error_code = CASE WHEN state = 'failed' THEN NULL ELSE error_code END,
             updated_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now')
         WHERE session_id = ?1",
        params![session_id],
    )?;
    ensure!(
        changed == 1,
        "session finalization did not queue one DataModel job"
    );
    Ok(())
}

fn recover_interrupted_data_model_jobs(connection: &Connection) -> anyhow::Result<()> {
    connection.execute(
        "UPDATE data_model_jobs
         SET state = 'pending', error_code = 'DATAMODEL_WORKER_INTERRUPTED',
             updated_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now')
         WHERE state IN ('running', 'finalizing')",
        [],
    )?;
    Ok(())
}

fn recover_interrupted_upload_jobs(connection: &Connection) -> anyhow::Result<()> {
    connection.execute(
        "UPDATE upload_jobs
            SET state = 'retry_wait', next_attempt_at_ms = 0,
                last_error_code = 'UPLOAD_WORKER_INTERRUPTED',
                updated_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now')
          WHERE state = 'running'",
        [],
    )?;
    connection.execute(
        "UPDATE upload_sessions
            SET seal_state = 'retry_wait', seal_next_attempt_at_ms = 0,
                seal_last_error_code = 'UPLOAD_WORKER_INTERRUPTED',
                updated_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now')
          WHERE seal_state = 'running'",
        [],
    )?;
    Ok(())
}

fn invalidate_data_model_for_corrupt_source(
    tx: &Transaction<'_>,
    session_id: &str,
) -> anyhow::Result<()> {
    let changed = tx.execute(
        "UPDATE data_model_jobs
         SET state = 'failed',
             ready_segments = (
                 SELECT COUNT(*) FROM segments
                 WHERE session_id = ?1 AND state = 'ready'
             ),
             processed_segments = 0,
             published_chunks = 0,
             published_frames = 0,
             pending_frames = 0,
             output_relative_path = NULL,
             error_code = ?2,
             updated_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now')
         WHERE session_id = ?1",
        params![session_id, DATA_MODEL_SOURCE_CORRUPT_CODE],
    )?;
    ensure!(
        changed == 1,
        "corrupt source did not invalidate exactly one DataModel job"
    );
    Ok(())
}

fn validate_data_model_progress(
    current: &DataModelJobStatus,
    progress: &DataModelProgress,
) -> anyhow::Result<()> {
    ensure!(
        progress.processed_segments >= current.processed_segments
            && progress.published_chunks >= current.published_chunks
            && progress.published_frames >= current.published_frames,
        "DataModel progress cannot move backwards"
    );
    ensure!(
        progress.processed_segments <= current.ready_segments,
        "DataModel progress exceeds ready segment count"
    );
    Ok(())
}

fn update_data_model_progress(
    tx: &Transaction<'_>,
    session_id: &str,
    progress: &DataModelProgress,
    state: DataModelJobState,
    output_relative_path: Option<&str>,
    error_code: Option<&str>,
) -> anyhow::Result<()> {
    let changed = tx.execute(
        "UPDATE data_model_jobs
         SET state = ?2, processed_segments = ?3, published_chunks = ?4,
             published_frames = ?5, pending_frames = ?6,
             output_relative_path = ?7, error_code = ?8,
             updated_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now')
         WHERE session_id = ?1",
        params![
            session_id,
            state.as_str(),
            i64::from(progress.processed_segments),
            i64::from(progress.published_chunks),
            u64_to_i64(progress.published_frames, "DataModel published frame count")?,
            u64_to_i64(progress.pending_frames, "DataModel pending frame count")?,
            output_relative_path,
            error_code,
        ],
    )?;
    ensure!(changed == 1, "DataModel progress did not update one job");
    Ok(())
}

fn validate_data_model_error_code(error_code: &str) -> anyhow::Result<()> {
    ensure!(
        !error_code.trim().is_empty() && error_code.len() <= 128,
        "DataModel error code must contain 1..=128 bytes"
    );
    ensure!(
        error_code.is_ascii()
            && error_code
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.')),
        "DataModel error code contains unsupported characters"
    );
    Ok(())
}

fn validate_data_model_output_path(path: &str) -> anyhow::Result<()> {
    use std::path::Component;

    ensure!(!path.trim().is_empty(), "DataModel output path is empty");
    let path = Path::new(path);
    ensure!(path.is_relative(), "DataModel output path must be relative");
    ensure!(
        path.components()
            .all(|component| matches!(component, Component::Normal(_))),
        "DataModel output path contains unsafe components"
    );
    Ok(())
}

fn validate_session_identity(
    tx: &Transaction<'_>,
    start: &RunStart<'_>,
    profile_hash: &str,
) -> anyhow::Result<String> {
    let mapping = start.mapping.mapping();
    let identity = tx.query_row(
        "SELECT game_id, game_exe_hash,
                recording_profile_id, recording_profile_version, recording_profile_hash,
                recording_profile_hash_kind,
                mapping_id, mapping_version, mapping_hash,
                session_contract, catalog_id, catalog_version, catalog_hash,
                device_profile_id, device_profile_version, device_profile_hash,
                game_profile_version, game_profile_hash,
                non_trainable, production_certified, state, schema_version
         FROM sessions WHERE id = ?1",
        params![start.session_id],
        |row| {
            Ok(PersistedSessionIdentity {
                game_id: row.get(0)?,
                game_exe_hash: row.get(1)?,
                recording_id: row.get(2)?,
                recording_version: row.get(3)?,
                recording_hash: row.get(4)?,
                recording_hash_kind: row.get(5)?,
                mapping_id: row.get(6)?,
                mapping_version: row.get(7)?,
                mapping_hash: row.get(8)?,
                session_contract: row.get(9)?,
                catalog_id: row.get(10)?,
                catalog_version: row.get(11)?,
                catalog_hash: row.get(12)?,
                device_id: row.get(13)?,
                device_version: row.get(14)?,
                device_hash: row.get(15)?,
                game_version: row.get(16)?,
                game_hash: row.get(17)?,
                non_trainable: row.get(18)?,
                production_certified: row.get(19)?,
                state: row.get(20)?,
                schema_version: row.get(21)?,
            })
        },
    )?;
    ensure!(
        identity.game_id == mapping.game.id
            && identity.game_exe_hash.as_deref() == start.executable_sha256
            && identity.recording_id == start.profile.profile_id
            && identity.recording_id == mapping.recording_profile_id
            && identity.recording_version == start.catalog.recording.identity.version
            && identity.recording_hash == profile_hash
            && identity.recording_hash_kind == "exact_source_bytes_sha256"
            && identity.mapping_id == mapping.mapping_id
            && identity.mapping_version == mapping.version
            && identity.mapping_hash == start.mapping.content_hash()
            && identity.session_contract == start.catalog.session_contract.source_family()
            && identity.catalog_id == start.catalog.catalog.catalog_id
            && identity.catalog_version == start.catalog.catalog.catalog_version
            && identity.catalog_hash == start.catalog.catalog.content_hash
            && identity.device_id == start.catalog.device.identity.id
            && identity.device_version == start.catalog.device.identity.version
            && identity.device_hash == start.catalog.device.identity.content_hash
            && identity.game_version == start.catalog.game.identity.version
            && identity.game_hash == start.catalog.game.identity.content_hash
            && identity.non_trainable == 1
            && identity.production_certified == 0
            && identity.schema_version
                == start
                    .catalog
                    .session_contract
                    .session_schema_version(start.profile.audio.is_some()),
        "session journal identity does not match the bound versioned catalog selection"
    );
    Ok(identity.state)
}

fn validate_run_identity(
    tx: &Transaction<'_>,
    start: &RunStart<'_>,
    journal_id: &str,
) -> anyhow::Result<String> {
    let identity = tx.query_row(
        "SELECT session_id, run_name, seq, start_request_id, session_frame_start,
                preflight_id, state, target_pid, target_window_id, backend, encoder_backend,
                fps_num, fps_den, width, height, depth_expected, performance_validation_only
         FROM capture_runs WHERE id = ?1",
        params![journal_id],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, i64>(7)?,
                row.get::<_, String>(8)?,
                row.get::<_, String>(9)?,
                row.get::<_, String>(10)?,
                row.get::<_, i64>(11)?,
                row.get::<_, i64>(12)?,
                row.get::<_, i64>(13)?,
                row.get::<_, i64>(14)?,
                row.get::<_, bool>(15)?,
                row.get::<_, bool>(16)?,
            ))
        },
    )?;
    ensure!(
        identity.0 == start.session_id
            && identity.1 == start.run_id
            && identity.2 == i64::from(start.run_seq)
            && identity.3 == start.request_id
            && identity.4 == u64_to_i64(start.session_frame_start, "session frame start")?
            && identity.5.as_deref() == Some(start.preflight.preflight_id.as_str())
            && identity.7 == i64::from(start.process_id)
            && identity.8 == start.target_window_id
            && identity.9 == "native_wgc_window_v1"
            && identity.10 == "mf_nvidia_hardware_v1"
            && identity.11 == i64::from(start.profile.fps_num)
            && identity.12 == i64::from(start.profile.fps_den)
            && identity.13 == i64::from(start.profile.width)
            && identity.14 == i64::from(start.profile.height)
            && identity.15 == start.depth_expected
            && identity.16 == start.performance_validation_only,
        "capture run replay changed persisted identity"
    );
    Ok(identity.6)
}

fn transition_session(
    tx: &Transaction<'_>,
    session_id: &str,
    allowed: &[&str],
    target: &str,
    reason: Option<&str>,
) -> anyhow::Result<()> {
    let current: String = tx.query_row(
        "SELECT state FROM sessions WHERE id = ?1",
        params![session_id],
        |row| row.get(0),
    )?;
    ensure!(
        allowed.contains(&current.as_str()),
        "illegal session transition {current} -> {target}"
    );
    let terminal = matches!(target, "completed" | "faulted" | "corrupt");
    tx.execute(
        "UPDATE sessions SET state = ?2, stop_reason = COALESCE(?3, stop_reason),
                last_error_code = CASE WHEN ?2 = 'faulted' THEN COALESCE(?3, last_error_code) ELSE last_error_code END,
                ended_at_utc = CASE WHEN ?4 THEN COALESCE(ended_at_utc, strftime('%Y-%m-%dT%H:%M:%fZ','now')) ELSE NULL END,
                updated_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now')
         WHERE id = ?1",
        params![session_id, target, reason, terminal],
    )?;
    Ok(())
}

fn run_state(tx: &Transaction<'_>, id: &str) -> anyhow::Result<String> {
    tx.query_row(
        "SELECT state FROM capture_runs WHERE id = ?1",
        params![id],
        |row| row.get(0),
    )
    .with_context(|| format!("capture run {id} is missing from the SQLite journal"))
}

fn segment_state(tx: &Transaction<'_>, id: &str) -> anyhow::Result<String> {
    tx.query_row(
        "SELECT state FROM segments WHERE id = ?1",
        params![id],
        |row| row.get(0),
    )
    .with_context(|| format!("segment {id} is missing from the SQLite journal"))
}

fn validate_ready_artifact(
    session_id: &str,
    run_id: &str,
    published: &PublishedSegment,
    run_directory: &Path,
) -> anyhow::Result<ReadyArtifact> {
    let segment_dir = run_directory
        .join("segments")
        .join(format!("{:05}", published.index));
    ensure!(
        segment_dir.is_dir(),
        "ready segment directory is missing: {}",
        segment_dir.display()
    );
    let manifest_path = segment_dir.join("sha256.json");
    let manifest: HashMap<String, String> = serde_json::from_slice(
        &fs::read(&manifest_path)
            .with_context(|| format!("failed to read {}", manifest_path.display()))?,
    )
    .context("invalid ready segment SHA-256 manifest")?;
    for name in [
        "video.mp4",
        "frames.jsonl.zst",
        "events.jsonl.zst",
        "meta.json",
    ] {
        ensure!(
            segment_dir.join(name).is_file(),
            "ready artifact is missing {name}"
        );
        ensure!(manifest.contains_key(name), "ready manifest has no {name}");
    }

    let meta_path = segment_dir.join("meta.json");
    let meta: ReadyMeta = serde_json::from_slice(&fs::read(&meta_path)?)
        .context("invalid ready segment meta.json")?;
    // 这一段属于哪一代，由它自己的 `schema_version` 反解；未知版本 fail-closed。
    // 是否与**会话**那一代一致，由 `mark_segment_ready` 在拿到 journal 行之后对账
    // ——这里没有会话上下文，自证不了。
    let contract = SessionContract::for_recording_schema_version(&meta.schema_version)
        .context("ready segment metadata carries an unsupported ingest contract")?;
    ensure!(
        meta.schema_version == contract.recording_schema_version(published.audio.is_some())
            && meta.session_id == session_id
            && meta.run_id == run_id
            && meta.segment_index == published.index
            && meta.state == contract.segment_ready_state()
            // legacy 必须带、且带的是 true/false 那一对；中性必须一个都不带。
            // 两个方向都拦：只拦前半条的话，一份自称中性却仍在申报训练分类的段
            // 会一路走到上传信封，再被服务端 403。
            && meta.non_trainable == contract.declares_training_classification().then_some(true)
            && meta.production_certified
                == contract.declares_training_classification().then_some(false),
        "ready segment metadata identity/state mismatch"
    );
    ensure!(
        meta.audio.as_ref() == published.audio.as_deref(),
        "ready audio metadata differs between finalizer output and meta.json"
    );
    let expected_payloads = expected_manifest_payloads(meta.audio.as_ref());
    ensure!(
        manifest.len() == expected_payloads.len()
            && manifest
                .keys()
                .all(|name| expected_payloads.contains(name.as_str())),
        "ready manifest contains an unexpected payload set"
    );
    ensure!(
        meta.n_frames > 0 && meta.qpc_end > meta.qpc_start,
        "ready segment timing is invalid"
    );
    ensure!(
        meta.frames.lines == meta.n_frames,
        "ready frames row count does not match n_frames"
    );
    ensure!(
        meta.health.queue_drops == 0 && meta.health.input_overflow == 0,
        "unhealthy segment cannot enter ready journal state"
    );
    ensure!(
        meta.video.path == "video.mp4"
            && meta.frames.path == "frames.jsonl.zst"
            && meta.events.path == "events.jsonl.zst",
        "ready artifact contains unexpected relative paths"
    );
    ensure!(
        meta.video.bytes == fs::metadata(segment_dir.join("video.mp4"))?.len(),
        "ready video byte count mismatch"
    );
    ensure!(
        meta.n_frames == published.frame_count
            && meta.events.lines == published.event_count
            && meta.qpc_start == published.qpc_start
            && meta.qpc_end == published.qpc_end
            && meta.video.bytes == published.video_bytes,
        "ready artifact summary does not match finalizer output"
    );
    ensure!(
        manifest.get("video.mp4") == Some(&meta.video.sha256)
            && manifest.get("frames.jsonl.zst") == Some(&meta.frames.sha256)
            && manifest.get("events.jsonl.zst") == Some(&meta.events.sha256)
            && meta.video.sha256 == published.video_sha256
            && meta.frames.sha256 == published.frames_sha256
            && meta.events.sha256 == published.events_sha256,
        "ready artifact hashes differ between finalizer, meta and manifest"
    );
    let meta_sha256 = manifest
        .get("meta.json")
        .context("ready manifest has no meta.json")?
        .clone();
    ensure!(
        meta_sha256 == published.meta_sha256,
        "ready meta hash differs between finalizer and manifest"
    );

    if let Some(audio) = &meta.audio {
        audio
            .validate()
            .context("ready audio metadata is invalid")?;
        if audio.has_artifact() {
            let audio_path = segment_dir.join(AUDIO_ARTIFACT_NAME);
            ensure!(audio_path.is_file(), "ready audio artifact is missing");
            ensure!(
                audio.bytes == Some(fs::metadata(&audio_path)?.len())
                    && manifest.get(AUDIO_ARTIFACT_NAME) == audio.sha256.as_ref(),
                "ready audio byte/hash identity differs from its file or manifest"
            );
        } else {
            ensure!(
                !segment_dir.join(AUDIO_ARTIFACT_NAME).exists()
                    && !manifest.contains_key(AUDIO_ARTIFACT_NAME),
                "absent audio unexpectedly retained an artifact"
            );
        }
    } else {
        ensure!(
            !segment_dir.join(AUDIO_ARTIFACT_NAME).exists(),
            "legacy ready segment unexpectedly contains audio"
        );
    }

    let prefix = format!("runs/{run_id}/segments/{:05}", published.index);
    Ok(ReadyArtifact {
        session_frame_start: meta.session_frame_start,
        n_frames: meta.n_frames,
        qpc_start: meta.qpc_start,
        qpc_end: meta.qpc_end,
        video_path: format!("{prefix}/video.mp4"),
        frames_path: format!("{prefix}/frames.jsonl.zst"),
        events_path: format!("{prefix}/events.jsonl.zst"),
        meta_path: format!("{prefix}/meta.json"),
        manifest_path: format!("{prefix}/sha256.json"),
        video_sha256: meta.video.sha256,
        frames_sha256: meta.frames.sha256,
        events_sha256: meta.events.sha256,
        meta_sha256,
        audio: meta.audio,
        bytes: meta.video.bytes,
        event_count: meta.events.lines,
        duplicate_frames: meta.health.duplicate_frames,
        source_superseded: meta.health.source_superseded,
        source_drops: meta.health.queue_drops,
        mapping_id: meta.mapping.id,
        mapping_version: meta.mapping.version,
        mapping_hash: meta.mapping.content_hash,
        fps_num: meta.fps_num,
        fps_den: meta.fps_den,
        width: meta.width,
        height: meta.height,
        catalog: meta.catalog,
        contract,
    })
}

fn artifact_catalog_matches_journal(
    catalog: &ArtifactCatalog,
    journal: &JournalCatalogIdentity,
    contract: SessionContract,
) -> bool {
    // 落盘产物里的 `mode` 是 **legacy 的**词；journal 存的是契约的 source family。
    // 两者不能直接比——它们在断代前恰好是同一个字符串，现在不是了。各自与契约比。
    journal.session_contract == contract.source_family()
        && catalog.mode.as_deref() == contract.legacy_catalog_mode()
        && catalog.catalog_id == journal.catalog_id
        && catalog.catalog_version == journal.catalog_version
        && catalog.catalog_content_hash == journal.catalog_hash
        && artifact_catalog_entry_matches(
            &catalog.device,
            &journal.device_id,
            &journal.device_version,
            &journal.device_hash,
        )
        && artifact_catalog_entry_matches(
            &catalog.recording,
            &journal.recording_id,
            &journal.recording_version,
            &journal.recording_hash,
        )
        && artifact_catalog_entry_matches(
            &catalog.game,
            &journal.game_id,
            &journal.game_version,
            &journal.game_hash,
        )
        && artifact_catalog_entry_matches(
            &catalog.mapping,
            &journal.mapping_id,
            &journal.mapping_version,
            &journal.mapping_hash,
        )
}

fn artifact_catalog_entry_matches(
    entry: &ArtifactCatalogEntry,
    id: &str,
    version: &str,
    hash: &str,
) -> bool {
    entry.id == id && entry.version == version && entry.content_hash == hash
}

fn validate_ready_replay(
    tx: &Transaction<'_>,
    id: &str,
    artifact: &ReadyArtifact,
) -> anyhow::Result<()> {
    let persisted = tx.query_row(
        "SELECT session_frame_start, n_frames, video_sha256, frames_sha256,
                events_sha256, meta_sha256, bytes, duplicate_frames, source_drops,
                audio_json
         FROM segments WHERE id = ?1",
        params![id],
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, i64>(7)?,
                row.get::<_, i64>(8)?,
                row.get::<_, Option<String>>(9)?,
            ))
        },
    )?;
    ensure!(
        persisted.0 == u64_to_i64(artifact.session_frame_start, "ready session frame start")?
            && persisted.1 == u64_to_i64(artifact.n_frames, "ready frame count")?
            && persisted.2 == artifact.video_sha256
            && persisted.3 == artifact.frames_sha256
            && persisted.4 == artifact.events_sha256
            && persisted.5 == artifact.meta_sha256
            && persisted.6 == u64_to_i64(artifact.bytes, "ready video bytes")?
            && persisted.7 == u64_to_i64(artifact.duplicate_frames, "ready duplicate frames")?
            && persisted.8 == u64_to_i64(artifact.source_drops, "ready source drops")?
            && parse_audio_json(persisted.9.as_deref())? == artifact.audio,
        "ready transaction replay does not match persisted artifact identity"
    );
    Ok(())
}

fn run_journal_id(session_id: &str, run_id: &str) -> String {
    format!("{session_id}:{run_id}")
}

fn segment_journal_id(session_id: &str, run_id: &str, segment_index: u32) -> String {
    format!("{session_id}:{run_id}:{segment_index:05}")
}

fn stop_reason(reason: RecordingStopReason) -> &'static str {
    match reason {
        RecordingStopReason::Manual => "manual",
        RecordingStopReason::Pause => "pause",
        RecordingStopReason::ForegroundLost => "foreground_lost",
        RecordingStopReason::WindowMinimized => "window_minimized",
        RecordingStopReason::WindowDestroyed => "window_destroyed",
        RecordingStopReason::GeometryChanged => "geometry_changed",
        RecordingStopReason::DiskLow => "disk_low",
        RecordingStopReason::DiskStatusUnavailable => "disk_status_unavailable",
        RecordingStopReason::SourceUnavailable => "source_unavailable",
        RecordingStopReason::SettingsChanged => "settings_changed",
        RecordingStopReason::InputFault => "input_fault",
        RecordingStopReason::CatalogRetired => "catalog_retired",
        // 130 号 §6.5：自动停录**不能是静默中止**，理由必须留在 session 里。
        // 这个串会随 `finish_run` 进 capture_runs.stop_reason，再进 session.json。
        RecordingStopReason::DepthFrameHealth => "depth_frame_health",
    }
}

#[cfg(test)]
fn bytes_sha256(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("sha256:{:x}", hasher.finalize())
}

fn u64_to_i64(value: u64, label: &str) -> anyhow::Result<i64> {
    i64::try_from(value).with_context(|| format!("{label} exceeds SQLite INTEGER range"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const MAPPING: &[u8] = br#"{
      "schema_version":"dm-mapping-2",
      "mapping_id":"game.test.pc",
      "version":"1",
      "game":{"id":"test","name":"Test","platform":"pc"},
      "taxonomy":{"content":"act","view":"first_person","material":"3d"},
      "recording_profile_id":"test-profile",
      "bindings":[{"device":"keyboard","scan_code":17,"extended":false,"semantic":"move.forward"}]
    }"#;

    fn profile() -> CaptureProfile {
        CaptureProfile {
            profile_id: "test-profile".into(),
            non_trainable: true,
            capture_backend: "native_wgc_window_v1".into(),
            encoder_backend: "mf_nvidia_hardware_v1".into(),
            width: 1920,
            height: 1080,
            fps_num: 30,
            fps_den: 1,
            bitrate_bps: 8_000_000,
            max_bitrate_bps: 12_000_000,
            gop_frames: 120,
            segment_seconds: 10,
            test_segments: 1,
            accepted_encoder_relaxations: None,
            audio: None,
        }
    }

    fn begin<'a>(
        journal: &SessionJournal,
        profile: &'a CaptureProfile,
        mapping: &'a MappingSnapshot,
    ) {
        journal
            .begin_test_run(&TestRunStart {
                request_id: "request-1",
                session_id: "session-1",
                run_id: "r0000",
                run_seq: 0,
                session_frame_start: 0,
                process_id: 42,
                target_window_id: "window-digest",
                executable_sha256: Some("sha256:exe"),
                executable_sha256_missing_reason: None,
                profile,
                mapping,
                view_calibration: None,
            })
            .unwrap();
    }

    fn write_ready_segment(
        run_directory: &Path,
        mapping: &MappingSnapshot,
        segment_index: u32,
    ) -> (PathBuf, PublishedSegment) {
        let segment_dir = run_directory
            .join("segments")
            .join(format!("{segment_index:05}"));
        fs::create_dir_all(&segment_dir).unwrap();
        let video = b"video";
        let frames = b"frames";
        let events = b"events";
        fs::write(segment_dir.join("video.mp4"), video).unwrap();
        fs::write(segment_dir.join("frames.jsonl.zst"), frames).unwrap();
        fs::write(segment_dir.join("events.jsonl.zst"), events).unwrap();
        let video_hash = bytes_sha256(video);
        let frames_hash = bytes_sha256(frames);
        let events_hash = bytes_sha256(events);
        let session_frame_start = u64::from(segment_index) * 300;
        let qpc_start = 100 + i64::from(segment_index) * 200;
        let qpc_end = qpc_start + 100;
        let catalog = CatalogSelection::for_test(&profile(), mapping);
        let meta = serde_json::json!({
            "schema_version": "dm-local-dev-recording-1",
            "session_id": "session-1",
            "run_id": "r0000",
            "segment_index": segment_index,
            "session_frame_start": session_frame_start,
            "n_frames": 300,
            "state": "non_trainable_ready",
            "non_trainable": true,
            "production_certified": false,
            "qpc_start": qpc_start,
            "qpc_end": qpc_end,
            "fps_num": 30,
            "fps_den": 1,
            "width": 1920,
            "height": 1080,
            "video": {"path": "video.mp4", "bytes": 5, "sha256": video_hash},
            "frames": {"path": "frames.jsonl.zst", "lines": 300, "sha256": frames_hash},
            "events": {"path": "events.jsonl.zst", "lines": 2, "sha256": events_hash},
            "mapping": {
                "id": mapping.mapping().mapping_id,
                "version": mapping.mapping().version,
                "content_hash": mapping.content_hash()
            },
            "catalog": catalog.recording_context(),
            "health": {
                "duplicate_frames": 0,
                "source_superseded": 10,
                "queue_drops": 0,
                "input_overflow": 0
            }
        });
        let meta_bytes = serde_json::to_vec_pretty(&meta).unwrap();
        fs::write(segment_dir.join("meta.json"), &meta_bytes).unwrap();
        let meta_hash = bytes_sha256(&meta_bytes);
        let manifest = serde_json::json!({
            "video.mp4": video_hash,
            "frames.jsonl.zst": frames_hash,
            "events.jsonl.zst": events_hash,
            "meta.json": meta_hash
        });
        fs::write(
            segment_dir.join("sha256.json"),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();
        let published = PublishedSegment {
            index: segment_index,
            frame_count: 300,
            event_count: 2,
            qpc_start,
            qpc_end,
            video_bytes: video.len() as u64,
            video_sha256: video_hash,
            frames_sha256: frames_hash,
            events_sha256: events_hash,
            meta_sha256: meta_hash,
            audio: None,
        };
        (segment_dir, published)
    }

    fn write_audio_ready_segment(
        run_directory: &Path,
        profile: &CaptureProfile,
        mapping: &MappingSnapshot,
        segment_index: u32,
    ) -> (PathBuf, PublishedSegment) {
        let (segment_dir, mut published) =
            write_ready_segment(run_directory, mapping, segment_index);
        let audio_bytes = b"zstd-compressed-pcm";
        fs::write(segment_dir.join(AUDIO_ARTIFACT_NAME), audio_bytes).unwrap();
        let audio_hash = bytes_sha256(audio_bytes);
        let audio: SegmentAudioMetadata = serde_json::from_value(serde_json::json!({
            "state": "ready",
            "source": "process_loopback",
            "path": AUDIO_ARTIFACT_NAME,
            "sha256": audio_hash,
            "bytes": audio_bytes.len(),
            "format": {
                "sample_rate": 48_000,
                "channels": 2,
                "bits": 16,
                "sample_format": "pcm_s16le",
                "channel_mask": 3,
                "requested_format": "pcm_s16le/48000/2",
                "system_src": true
            },
            "qpc_frequency": 10_000_000,
            "first_sample_qpc": 100,
            "frames": 480_000,
            "health": {
                "discontinuities": [],
                "silence_filled_spans": [],
                "queue_overflows": 0,
                "timestamp_errors": 0,
                "max_qpc_drift_us": 0
            }
        }))
        .unwrap();
        audio.validate().unwrap();

        let meta_path = segment_dir.join("meta.json");
        let mut meta: serde_json::Value =
            serde_json::from_slice(&fs::read(&meta_path).unwrap()).unwrap();
        meta["schema_version"] = RECORDING_SCHEMA_V2.into();
        meta["catalog"] =
            serde_json::to_value(CatalogSelection::for_test(profile, mapping).recording_context())
                .unwrap();
        meta["audio"] = serde_json::to_value(&audio).unwrap();
        let meta_bytes = serde_json::to_vec_pretty(&meta).unwrap();
        fs::write(&meta_path, &meta_bytes).unwrap();
        let meta_hash = bytes_sha256(&meta_bytes);

        let manifest_path = segment_dir.join("sha256.json");
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        manifest["meta.json"] = meta_hash.clone().into();
        manifest[AUDIO_ARTIFACT_NAME] = audio_hash.into();
        fs::write(manifest_path, serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();

        published.meta_sha256 = meta_hash;
        published.audio = Some(Box::new(audio));
        (segment_dir, published)
    }

    fn zero_outcome(
        root: &Path,
        mapping: &MappingSnapshot,
        reason: RecordingStopReason,
    ) -> RecordingOutcome {
        RecordingOutcome {
            schema_version: "dm-local-dev-recording-1",
            state: "non_trainable_complete",
            non_trainable: Some(true),
            consent_id: None,
            production_certified: Some(false),
            input_status: "raw_input_v1",
            session_id: "session-1".into(),
            run_id: "r0000".into(),
            mapping_id: mapping.mapping().mapping_id.clone(),
            mapping_version: mapping.mapping().version.clone(),
            mapping_content_hash: mapping.content_hash().into(),
            session_frame_start: 0,
            session_frame_end: 0,
            run_directory: root.join("session-1/runs/r0000"),
            frame_count: 0,
            segment_count: 0,
            duplicate_frames: 0,
            source_superseded: 0,
            wgc_callback_count: 0,
            watermark_decoded_observations: 0,
            watermark_rejected_observations: 0,
            watermark_integrity_rejections: 0,
            watermark_ambiguous_rejections: 0,
            max_consecutive_watermark_rejections: 0,
            watermark_stale_observations: 0,
            max_watermark_stale_lag_presents: 0,
            input_event_count: 0,
            input_queue_overflows: 0,
            anchor_qpc: None,
            anchor_at_utc: None,
            qpc_frequency: 10_000_000,
            qpc_start: None,
            qpc_end: None,
            ended_at_utc: None,
            stop_reason: reason,
        }
    }

    fn insert_ready_segment_row(
        journal: &SessionJournal,
        session_id: &str,
        run_name: &str,
        segment_index: u32,
        session_frame_start: u64,
        frame_count: u64,
    ) {
        let connection = journal.connection.lock();
        connection
            .execute(
                "INSERT INTO segments (
                    id, session_id, run_id, run_name, seq, state,
                    session_frame_start, n_frames, created_at_utc, updated_at_utc
                 ) VALUES (
                    ?1, ?2, ?3, ?4, ?5, 'ready', ?6, ?7,
                    strftime('%Y-%m-%dT%H:%M:%fZ','now'),
                    strftime('%Y-%m-%dT%H:%M:%fZ','now')
                 )",
                params![
                    segment_journal_id(session_id, run_name, segment_index),
                    session_id,
                    run_journal_id(session_id, run_name),
                    run_name,
                    i64::from(segment_index),
                    u64_to_i64(session_frame_start, "test segment start").unwrap(),
                    u64_to_i64(frame_count, "test segment frames").unwrap(),
                ],
            )
            .unwrap();
    }

    fn session_artifact_sources() -> Vec<SessionArtifactSource> {
        SessionArtifactKind::REQUIRED
            .into_iter()
            .map(|kind| SessionArtifactSource {
                kind,
                relative_path: format!("session-1/session/{}", kind.file_name()),
                expected_sha256: format!("sha256:{}", "0".repeat(64)),
                expected_size: 10,
                content_md5: "1B2M2Y8AsgTpgAmY7PhCfg==".into(),
            })
            .collect()
    }

    fn complete_empty_data_model_session(
        journal: &SessionJournal,
        root: &Path,
        mapping: &MappingSnapshot,
    ) {
        journal
            .mark_run_started("session-1", "r0000", 100, 10_000_000)
            .unwrap();
        let mut outcome = zero_outcome(root, mapping, RecordingStopReason::Manual);
        outcome.anchor_qpc = Some(100);
        journal
            .finish_run(&outcome, RunTerminalState::Completed)
            .unwrap();
        assert_eq!(
            journal.claim_next_data_model_job().unwrap().unwrap().state,
            DataModelJobState::Finalizing
        );
        journal.mark_data_model_empty("session-1").unwrap();
    }

    #[test]
    fn schema_v7_adds_an_empty_mirror_outbox_without_touching_capture_facts() {
        let connection = Connection::open_in_memory().unwrap();
        migrate(&connection).unwrap();
        connection
            .execute_batch(
                "INSERT INTO sessions(
                    id, state, non_trainable, production_certified, game_id, game_exe_hash,
                    recording_profile_id, recording_profile_hash,
                    mapping_id, mapping_version, mapping_hash, schema_version,
                    started_at_utc, created_at_utc, updated_at_utc
                 ) VALUES (
                    'mirror-session', 'completed', 1, 0, 'game', 'hash',
                    'rp', 'rph', 'map', '1', 'mh', 'dm-local-dev-session-2',
                    't', 't', 't'
                 );",
            )
            .unwrap();

        // 迁移到此为止只创建结构：采集链路还没有往 outbox 写。
        // 如果哪天这条断言开始失败，说明有人接了写入却没更新这里的说明。
        let queued: i64 = connection
            .query_row("SELECT COUNT(*) FROM mirror_outbox", [], |row| row.get(0))
            .unwrap();
        let cursor: (i64, i64) = connection
            .query_row(
                "SELECT acked_seq, attempt_count FROM mirror_cursor WHERE singleton = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(queued, 0);
        assert_eq!(cursor, (0, 0));

        // 二次迁移必须幂等：不重建表、不把游标插成两行。
        migrate(&connection).unwrap();
        let cursor_rows: i64 = connection
            .query_row("SELECT COUNT(*) FROM mirror_cursor", [], |row| row.get(0))
            .unwrap();
        let sessions: i64 = connection
            .query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))
            .unwrap();
        assert_eq!(cursor_rows, 1);
        assert_eq!(sessions, 1);
    }

    #[test]
    fn version_sixteen_journal_migrates_audio_metadata_and_upload_kind_to_v17() {
        let connection = Connection::open_in_memory().unwrap();
        migrate_to(&connection, 16).unwrap();
        let before: (i64, i64) = connection
            .query_row(
                "SELECT
                    (SELECT schema_version FROM journal_meta WHERE singleton = 1),
                    EXISTS(SELECT 1 FROM pragma_table_info('segments') WHERE name = 'audio_json')",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(before, (16, 0));

        migrate(&connection).unwrap();
        let after: (i64, i64, String) = connection
            .query_row(
                "SELECT
                    (SELECT schema_version FROM journal_meta WHERE singleton = 1),
                    EXISTS(SELECT 1 FROM pragma_table_info('segments') WHERE name = 'audio_json'),
                    (SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'upload_parts')",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        // 断言 head 而不是字面量 17：`migrate` 走到当前最高版本，而 v17 之后又接了
        // v18（certified 预检终态）。写死 17 会让「本迁移之后再加任何一版」都误报成
        // 音频迁移坏了。音频的真正判据是下面两条：列在、且 upload_parts 收 'audio'。
        // 用全量 `migrate` 而非 `migrate_to(17)` 是有意的——它同时证明后续迁移的整表
        // 重建没有把音频列或 artifact_kind 词表冲掉。
        assert_eq!((after.0, after.1), (JOURNAL_SCHEMA_VERSION, 1));
        assert!(after.2.contains("'audio'"));

        migrate(&connection).unwrap();
        let replayed_version: i64 = connection
            .query_row(
                "SELECT schema_version FROM journal_meta WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(replayed_version, JOURNAL_SCHEMA_VERSION);
    }

    #[test]
    fn v34_tracks_provider_attempts_and_revives_only_the_legacy_completion_lease_rejection() {
        let connection = Connection::open_in_memory().unwrap();
        migrate_to(&connection, 33).unwrap();
        connection
            .execute_batch(
                "PRAGMA foreign_keys=OFF;
                 INSERT INTO upload_jobs(
                    segment_id, session_id, run_id, state, last_error_code,
                    remote_clip_id, remote_upload_intent_id,
                    http_status, server_code, created_at_utc, updated_at_utc
                 ) VALUES
                    ('events-stuck', 'session', 'run', 'rejected', 'UPLOAD_REQUEST_REJECTED',
                     'clip', 'intent', 409, 'multipart_completion_in_progress', 't', 't'),
                    ('other-rejection', 'session', 'run', 'rejected', 'UPLOAD_REQUEST_REJECTED',
                     'clip-2', 'intent-2', 409, 'device_profile_mismatch', 't', 't');
                 INSERT INTO upload_parts(
                    segment_id, artifact_kind, part_number, byte_offset, byte_size,
                    content_md5, etag, uploaded_at_utc
                 ) VALUES ('events-stuck', 'events', 1, 0, 7, 'md5', 'etag', 't');",
            )
            .unwrap();

        migrate(&connection).unwrap();
        let migrated: (i64, String, i64) = connection
            .query_row(
                "SELECT provider_attempt_no,
                        (SELECT state FROM upload_jobs WHERE segment_id = 'events-stuck'),
                        (SELECT next_attempt_at_ms FROM upload_jobs WHERE segment_id = 'events-stuck')
                   FROM upload_parts WHERE segment_id = 'events-stuck'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        let unrelated: String = connection
            .query_row(
                "SELECT state FROM upload_jobs WHERE segment_id = 'other-rejection'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(migrated, (1, "retry_wait".into(), 0));
        assert_eq!(unrelated, "rejected");
    }

    #[test]
    fn v24_adds_optional_depth_columns_to_a_v23_journal() {
        let connection = Connection::open_in_memory().unwrap();
        migrate_to(&connection, 23).unwrap();

        let before: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('segments')
                  WHERE name IN ('depth_path', 'depth_sha256')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(before, 0);

        migrate(&connection).unwrap();
        let after: (i64, i64) = connection
            .query_row(
                "SELECT COUNT(*), COALESCE(SUM([notnull]), 0)
                   FROM pragma_table_info('segments')
                  WHERE name IN ('depth_path', 'depth_sha256')",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let version: i64 = connection
            .query_row("SELECT schema_version FROM journal_meta", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(
            after,
            (2, 0),
            "legacy no-depth segments require nullable columns"
        );
        assert_eq!(version, JOURNAL_SCHEMA_VERSION);

        migrate(&connection).unwrap();
    }

    #[test]
    fn v25_adds_an_independent_depth_lab_queue_without_touching_upload_jobs() {
        let connection = Connection::open_in_memory().unwrap();
        migrate_to(&connection, 24).unwrap();
        let production_shape_before: String = connection
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'upload_jobs'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let before: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                  WHERE type = 'table'
                    AND name IN ('depth_lab_segment_facts', 'depth_lab_jobs')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(before, 0);

        migrate(&connection).unwrap();
        let after: (i64, String) = connection
            .query_row(
                "SELECT COUNT(*), MIN(sql) FROM sqlite_master
                  WHERE type = 'table' AND name = 'depth_lab_jobs'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(after.0, 1);
        assert!(after.1.contains("manifest_sha256 TEXT NOT NULL"));
        assert!(after.1.contains("state IN ('pending', 'running')"));
        let fact_shape: String = connection
            .query_row(
                "SELECT sql FROM sqlite_master
                  WHERE type = 'table' AND name = 'depth_lab_segment_facts'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(fact_shape.contains("candidate_eligible INTEGER NOT NULL"));
        assert!(fact_shape.contains("invalid_frame_count INTEGER NOT NULL"));
        let production_shape_after: String = connection
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'upload_jobs'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(production_shape_after, production_shape_before);
    }

    #[test]
    fn v30_persists_the_performance_validation_upload_denial() {
        let connection = Connection::open_in_memory().unwrap();
        migrate_to(&connection, 29).unwrap();
        let before: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('capture_runs')
                  WHERE name = 'performance_validation_only'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(before, 0);

        migrate_to(&connection, 30).unwrap();
        let shape: (i64, i64) = connection
            .query_row(
                "SELECT [notnull], dflt_value = '0'
                   FROM pragma_table_info('capture_runs')
                  WHERE name = 'performance_validation_only'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(shape, (1, 1));
        let version: i64 = connection
            .query_row("SELECT schema_version FROM journal_meta", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(version, 30);
        let account_partition_columns: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('client_event_outbox')
                  WHERE name = 'account_environment'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(account_partition_columns, 0);
        migrate(&connection).unwrap();
    }

    #[test]
    fn v28_separates_the_durable_depth_manifest_from_the_real_upload_zip() {
        let connection = Connection::open_in_memory().unwrap();
        migrate_to(&connection, 27).unwrap();
        let before: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('segments')
                  WHERE name IN ('depth_upload_path', 'depth_upload_sha256')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(before, 0);

        migrate(&connection).unwrap();
        let after: (i64, i64) = connection
            .query_row(
                "SELECT COUNT(*), COALESCE(SUM([notnull]), 0)
                   FROM pragma_table_info('segments')
                  WHERE name IN ('depth_upload_path', 'depth_upload_sha256')",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(after, (2, 0));
        let depth_expected_shape: (i64, i64) = connection
            .query_row(
                "SELECT [notnull], dflt_value = '0'
                   FROM pragma_table_info('capture_runs')
                  WHERE name = 'depth_expected'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(depth_expected_shape, (1, 1));
        let upload_parts_sql: String = connection
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'upload_parts'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(upload_parts_sql.contains("'depth'"));
        assert_eq!(
            connection
                .query_row("SELECT schema_version FROM journal_meta", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            JOURNAL_SCHEMA_VERSION
        );
        migrate(&connection).unwrap();
    }

    #[test]
    fn v29_adds_a_nullable_run_wall_clock_anchor_without_changing_legacy_rows() {
        let connection = Connection::open_in_memory().unwrap();
        migrate_to(&connection, 28).unwrap();
        let before: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('capture_runs')
                  WHERE name = 'anchor_at_utc'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(before, 0);

        migrate(&connection).unwrap();
        let after: (i64, i64) = connection
            .query_row(
                "SELECT COUNT(*), COALESCE(SUM([notnull]), 0)
                   FROM pragma_table_info('capture_runs')
                  WHERE name = 'anchor_at_utc'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(after, (1, 0));
        assert_eq!(
            connection
                .query_row("SELECT schema_version FROM journal_meta", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            JOURNAL_SCHEMA_VERSION
        );
        migrate(&connection).unwrap();
    }

    #[test]
    fn v31_quarantines_legacy_client_events_in_an_unknown_environment() {
        let connection = Connection::open_in_memory().unwrap();
        migrate_to(&connection, 30).unwrap();
        let performance_column: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('capture_runs')
                  WHERE name = 'performance_validation_only'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(performance_column, 1);
        connection
            .execute(
                "INSERT INTO client_event_outbox(
                    event_id, occurred_at_utc, kind, severity, payload_json, created_at_utc
                 ) VALUES (
                    '00000000-0000-4000-8000-000000000001', 't', 'fault', 'error', '{}', 't'
                 )",
                [],
            )
            .unwrap();
        connection
            .execute(
                "UPDATE client_event_cursor
                 SET attempt_count = 2, dropped_events = 3, last_error = 'OLD_ERROR'",
                [],
            )
            .unwrap();

        migrate(&connection).unwrap();

        let legacy_environment: String = connection
            .query_row(
                "SELECT account_environment FROM client_event_outbox",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(legacy_environment, "unknown");
        let unknown_cursor: (i64, i64, String) = connection
            .query_row(
                "SELECT attempt_count, dropped_events, last_error
                   FROM client_event_cursor
                  WHERE account_environment = 'unknown'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            unknown_cursor,
            (2, 3, "CLIENT_EVENTS_ENVIRONMENT_UNKNOWN".into())
        );
        let configured_cursor_count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM client_event_cursor
                  WHERE account_environment IN ('staging', 'production')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(configured_cursor_count, 2);
        assert_eq!(
            connection
                .query_row("SELECT schema_version FROM journal_meta", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            JOURNAL_SCHEMA_VERSION
        );
        migrate(&connection).unwrap();
    }

    /// v27：certified 车道废除（docs/100）。v19 钉下的三重死对——可训练声明 ⟺
    /// certified mode ⟺ consent 在场 ⟺ certified 词表——被换成「只剩一条车道」的
    /// 形状：`non_trainable` 恒 1、certified mode 与 certified 词表都写不进去，
    /// 而 consent 从此**可以**与 non_trainable = 1 共存（下一刀要用的载体）。
    ///
    /// v33 之后这条测试跑在断代后的列上：`catalog_mode` 换成了 `session_contract`，
    /// 取值也从车道词换成契约的 source family。命题一条没少——v27 想拦的三件事
    /// （自称可训练、certified 车道、certified 词表）现在由 v33 的新 CHECK 继续拦。
    #[test]
    fn schema_v27_retires_the_certified_lane_and_frees_the_consent_carrier() {
        let connection = Connection::open_in_memory().unwrap();
        migrate(&connection).unwrap();
        let version: i64 = connection
            .query_row("SELECT schema_version FROM journal_meta", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(version, JOURNAL_SCHEMA_VERSION);

        let forge =
            |id: &str, non_trainable: i64, mode: &str, schema: &str, consent: Option<&str>| {
                connection.execute(
                    "INSERT INTO sessions(
                    id, state, non_trainable, production_certified, consent_id,
                    game_id, game_exe_hash, recording_profile_id, recording_profile_hash,
                    mapping_id, mapping_version, mapping_hash, session_contract,
                    schema_version, started_at_utc, created_at_utc, updated_at_utc
                 ) VALUES (
                    ?1, 'active', ?2, 0, ?3, 'game', 'hash', 'rp', 'rph',
                    'map', '1', 'mh', ?4, ?5, 't', 't', 't'
                 )",
                    params![id, non_trainable, consent, mode, schema],
                )
            };

        // 普通 legacy 行照旧。
        forge(
            "dev-session",
            1,
            "dm-local-dev",
            "dm-local-dev-session-2",
            None,
        )
        .unwrap();

        // 中性契约的行：v33 之后它是合法形状，且仍然 non_trainable = 1
        // ——本地永远不主张可训练，与走哪一代无关（issue #855 §B）。
        forge(
            "ingest-v2-session",
            1,
            "dm-session-ingest",
            "dm-session-ingest-2",
            None,
        )
        .unwrap();

        // consent 在场不再蕴含可训练：这是 v19 三重死对里被解开的那一条，
        // 也是下一刀（可训练性开通）要用的载体形状。
        forge(
            "consented-but-non-trainable",
            1,
            "dm-local-dev",
            "dm-local-dev-session-3",
            Some("CNS-20260805-001"),
        )
        .unwrap();

        // 可训练声明：本地永远不主张，任何 mode 下都写不进去。
        assert!(
            forge(
                "t-trainable",
                0,
                "dm-local-dev",
                "dm-local-dev-session-2",
                Some("CNS-20260805-002"),
            )
            .is_err()
        );
        // 退役的 certified 车道词：v33 的 CHECK 只认两代契约与历史 `legacy`。
        assert!(forge("t-mode", 1, "certified", "dm-local-dev-session-2", None).is_err());
        // 退役的车道词 `dev_non_trainable` 本身也写不进去了——v33 把它迁成了
        // `dm-local-dev`，再写回来就是让那个词借尸还魂（issue #855 §A.4）。
        assert!(
            forge(
                "t-retired-word",
                1,
                "dev_non_trainable",
                "dm-local-dev-session-2",
                None
            )
            .is_err()
        );
        // 退役的 certified 词表。
        assert!(forge("t-vocab", 1, "dm-local-dev", "dm-certified-session-2", None).is_err());

        // production_certified 仍然钉死在 0（first_party 尸体，不在本轮范围）。
        assert!(
            connection
                .execute(
                    "UPDATE sessions SET production_certified = 1 WHERE id = 'dev-session'",
                    [],
                )
                .is_err()
        );

        // 预检词表同样收回 certified_pass。
        assert!(
            connection
                .execute("UPDATE preflight_attempts SET state = 'certified_pass'", [],)
                .is_ok(),
            "an empty table update is vacuously fine; the CHECK is exercised on insert"
        );

        // 重跑迁移幂等，三行合法行都还在（legacy 两行 + 中性一行）。
        migrate(&connection).unwrap();
        let sessions: i64 = connection
            .query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))
            .unwrap();
        assert_eq!(sessions, 3);
    }

    /// v27 遇到存量 certified 行时**明确失败**（docs/100 §4.1）：那些行的分类是一个
    /// 数据决定，不该由一条迁移替谁做主。对照 v19——它删的是「永远传不上去的过渡
    /// 态行」，而这些行曾是一条完整接通的车道的产物。
    #[test]
    fn v27_refuses_to_rewrite_surviving_certified_rows() {
        let connection = Connection::open_in_memory().unwrap();
        migrate_to(&connection, 26).unwrap();
        connection
            .execute(
                "INSERT INTO sessions(
                    id, state, non_trainable, production_certified, consent_id,
                    game_id, game_exe_hash, recording_profile_id, recording_profile_hash,
                    mapping_id, mapping_version, mapping_hash, catalog_mode,
                    schema_version, started_at_utc, created_at_utc, updated_at_utc
                 ) VALUES (
                    'survivor', 'completed', 0, 0, 'CNS-20260731-001', 'game', 'hash',
                    'rp', 'rph', 'map', '1', 'mh', 'certified',
                    'dm-certified-session-2', 't', 't', 't'
                 )",
                [],
            )
            .unwrap();

        let error = migrate(&connection).expect_err("v27 must refuse, not rewrite");
        let message = format!("{error:#}");
        assert!(
            message.contains("refuses to rewrite") && message.contains("docs/100"),
            "the refusal must name what it refuses and why, got: {message}"
        );

        // 迁移失败后 journal 仍停在 26 且那一行分毫未动——「明确失败」的意义就在
        // 于此：人还能在原样的库上做决定。
        let (version, survivors): (i64, i64) = connection
            .query_row(
                "SELECT
                    (SELECT schema_version FROM journal_meta WHERE singleton = 1),
                    (SELECT count(*) FROM sessions WHERE catalog_mode = 'certified')",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!((version, survivors), (26, 1));
    }

    /// v19 对 0022 过渡态存量（certified mode + non_trainable = 1、consent NULL，
    /// 上一版 begin_run 的固定写法）的处置：照 v16 清 first_party 的先例在重建前
    /// 删掉——不清会让整次迁移在 CHECK 上炸掉、journal 从此打不开。
    #[test]
    fn v19_clears_transitional_certified_rows_instead_of_bricking_the_journal() {
        let connection = Connection::open_in_memory().unwrap();
        migrate_to(&connection, 18).unwrap();
        // v18 形状下铸一行过渡态 certified（当时的 CHECK 只有 non_trainable = 1）。
        connection
            .execute_batch(
                "INSERT INTO sessions(
                    id, state, non_trainable, production_certified, game_id, game_exe_hash,
                    recording_profile_id, recording_profile_hash,
                    mapping_id, mapping_version, mapping_hash, catalog_mode, schema_version,
                    started_at_utc, created_at_utc, updated_at_utc
                 ) VALUES (
                    'transitional', 'completed', 1, 0, 'game', 'hash',
                    'rp', 'rph', 'map', '1', 'mh', 'certified', 'dm-local-dev-session-2',
                    't', 't', 't'
                 );",
            )
            .unwrap();
        // v19 必须能走完，且过渡态行被清而不是把迁移炸死。
        migrate(&connection).unwrap();
        let survivors: i64 = connection
            .query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))
            .unwrap();
        assert_eq!(survivors, 0);
    }

    #[test]
    fn v20_requeues_only_local_source_missing_uploads_without_remote_identity() {
        let connection = Connection::open_in_memory().unwrap();
        migrate_to(&connection, 19).unwrap();
        // The migration predicate only depends on upload_jobs. Keep this fixture
        // deliberately small instead of recreating the full capture graph.
        connection
            .pragma_update(None, "foreign_keys", "OFF")
            .unwrap();
        connection
            .execute_batch(
                "INSERT INTO upload_jobs(
                    segment_id, session_id, run_id, state, attempt_count,
                    next_attempt_at_ms, last_error_code, remote_clip_id,
                    clip_revision, remote_upload_intent_id,
                    created_at_utc, updated_at_utc
                 ) VALUES
                    ('local-missing', 'session-1', 'run-1', 'rejected', 3, 123,
                     'UPLOAD_SOURCE_MISSING', NULL, NULL, NULL, 't', 't'),
                    ('remote-started', 'session-1', 'run-1', 'rejected', 2, 456,
                     'UPLOAD_SOURCE_MISSING', NULL, NULL, 'intent-1', 't', 't'),
                    ('source-changed', 'session-1', 'run-1', 'rejected', 1, 789,
                     'UPLOAD_SOURCE_CHANGED', NULL, NULL, NULL, 't', 't');",
            )
            .unwrap();
        connection
            .pragma_update(None, "foreign_keys", "ON")
            .unwrap();

        migrate(&connection).unwrap();

        let recovered: (String, i64, i64, String) = connection
            .query_row(
                "SELECT state, attempt_count, next_attempt_at_ms, last_error_code
                   FROM upload_jobs WHERE segment_id = 'local-missing'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            recovered,
            ("retry_wait".into(), 3, 0, "UPLOAD_SOURCE_MISSING".into())
        );

        for segment_id in ["remote-started", "source-changed"] {
            let state: String = connection
                .query_row(
                    "SELECT state FROM upload_jobs WHERE segment_id = ?1",
                    params![segment_id],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(state, "rejected", "{segment_id} must stay terminal");
        }
        let version: i64 = connection
            .query_row("SELECT schema_version FROM journal_meta", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(version, JOURNAL_SCHEMA_VERSION);

        migrate(&connection).unwrap();
        let replayed_state: String = connection
            .query_row(
                "SELECT state FROM upload_jobs WHERE segment_id = 'local-missing'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(replayed_state, "retry_wait");
    }

    #[test]
    fn v21_closes_every_existing_upload_queue_until_a_manual_request() {
        let connection = Connection::open_in_memory().unwrap();
        migrate_to(&connection, 20).unwrap();
        connection
            .pragma_update(None, "foreign_keys", "OFF")
            .unwrap();
        connection
            .execute(
                "INSERT INTO upload_sessions(
                    session_id, registration_state, created_at_utc, updated_at_utc
                 ) VALUES ('legacy-session', 'completed', 't', 't')",
                [],
            )
            .unwrap();
        connection
            .pragma_update(None, "foreign_keys", "ON")
            .unwrap();

        migrate(&connection).unwrap();

        let gate: (i64, Option<String>) = connection
            .query_row(
                "SELECT upload_requested, upload_requested_at_utc
                   FROM upload_sessions WHERE session_id = 'legacy-session'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(gate, (0, None));
        migrate(&connection).unwrap();
        let replayed_gate: i64 = connection
            .query_row(
                "SELECT upload_requested FROM upload_sessions
                  WHERE session_id = 'legacy-session'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(replayed_gate, 0);
    }

    #[test]
    fn v23_adds_structured_upload_refusal_columns() {
        let connection = Connection::open_in_memory().unwrap();
        migrate_to(&connection, 22).unwrap();

        let before: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('upload_jobs')
                  WHERE name IN ('http_status', 'server_code', 'server_message', 'request_id')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(before, 0);

        migrate(&connection).unwrap();
        let after: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('upload_jobs')
                  WHERE name IN ('http_status', 'server_code', 'server_message', 'request_id')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let version: i64 = connection
            .query_row("SELECT schema_version FROM journal_meta", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(after, 4);
        assert_eq!(version, JOURNAL_SCHEMA_VERSION);
    }

    /// v18：`preflight_attempts` 的状态词表收下 certified 终态（docs/26 §7）。
    ///
    /// ⚠ **这条词表已被 v27 收回**（docs/100：certified 车道整条废除）。本测试因此
    /// 钉在 **v18 本身**而不是 head——v18 仍在迁移链上，一个停在 v17 的旧 journal
    /// 升级时必须还能正确走过它。head 的行为由
    /// `schema_v27_retires_the_certified_lane_and_frees_the_consent_carrier` 覆盖，
    /// 两条合起来才说清「当时能、现在不能」。
    ///
    /// 编号说明：本迁移原为 v17，与上游先落地的音频 sidecar 迁移撞号，rebase 时
    /// 顺延为 v18。两者互不相干——v17 动 `segments`/`upload_parts`，v18 动
    /// `preflight_attempts`。
    #[test]
    fn schema_v18_admitted_the_certified_preflight_terminal_before_v27_took_it_back() {
        let connection = Connection::open_in_memory().unwrap();
        migrate_to(&connection, 18).unwrap();
        let version: i64 = connection
            .query_row("SELECT schema_version FROM journal_meta", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(version, 18);

        let insert = |id: &str, state: &str| {
            connection.execute(
                "INSERT INTO preflight_attempts(
                    id, request_id, state, phase, catalog_mode, target_window_id, game_id,
                    catalog_id, catalog_version, catalog_hash,
                    device_profile_id, device_profile_version, device_profile_hash,
                    recording_profile_id, recording_profile_version, recording_profile_hash,
                    game_profile_version, game_profile_hash,
                    mapping_id, mapping_version, mapping_hash,
                    evidence_json, evidence_hash, created_at_utc, updated_at_utc
                 ) VALUES (
                    ?1, ?1, ?2, 'complete', 'certified', 'win', 'game',
                    'cat', '2026-07-31.1', 'sha256:cat',
                    'dp', '1', 'sha256:dp', 'rp', '1', 'sha256:rp', '1', 'sha256:gp',
                    'map', '1', 'sha256:map', '{}', 'sha256:evidence', 't', 't'
                 )",
                params![id, state],
            )
        };
        insert("certified-terminal", "certified_pass").unwrap();
        insert("dev-terminal", "dev_non_trainable_pass").unwrap();
        // 词表仍然是白名单：编出来的第三种通过态照样进不来。
        assert!(insert("invented", "first_party_release_pass").is_err());
        assert!(insert("also-invented", "passed").is_err());

        // 通过态不许带失败码（v16 的配对 CHECK 必须原样带过来）。
        assert!(
            connection
                .execute(
                    "UPDATE preflight_attempts SET failure_code = 'X'
                       WHERE id = 'certified-terminal'",
                    [],
                )
                .is_err()
        );

        // v18 内重跑幂等，两行都在。
        migrate_to(&connection, 18).unwrap();
        let rows: i64 = connection
            .query_row("SELECT COUNT(*) FROM preflight_attempts", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(rows, 2);

        // 升到 head 必须**明确拒绝**：这份 journal 里有一行真的 certified 预检，
        // 它的处置是数据决定（docs/100 §4.1）。这条断言的价值在于夹具是真的——由
        // v18 自己的写入路径产生，不是手铸出来的。
        let error = migrate(&connection).expect_err("v27 must refuse a real certified row");
        assert!(
            format!("{error:#}").contains("refuses to rewrite"),
            "got: {error:#}"
        );
        // 版本停在 26 而不是 18：v19–v26 各自 COMMIT 过，v27 是链上第九个，它在任何
        // DDL 之前就拒绝了。这正是「明确失败」想要的形状——库停在一个自洽的版本上，
        // 那两行分毫未动，人可以在原样的库上做决定。
        let (version, survivors): (i64, i64) = connection
            .query_row(
                "SELECT
                    (SELECT schema_version FROM journal_meta WHERE singleton = 1),
                    (SELECT count(*) FROM preflight_attempts
                      WHERE state = 'certified_pass' OR catalog_mode = 'certified')",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!((version, survivors), (26, 2));
    }

    #[test]
    fn session_artifacts_follow_the_durable_single_put_state_machine_and_block_deletion() {
        let dir = tempfile::tempdir().unwrap();
        let journal = SessionJournal::open(dir.path().join("journal.sqlite3")).unwrap();
        let profile = profile();
        let mapping = MappingSnapshot::from_slice(MAPPING).unwrap();
        begin(&journal, &profile, &mapping);
        complete_empty_data_model_session(&journal, dir.path(), &mapping);

        assert_eq!(
            journal.next_session_artifact_freeze_candidate().unwrap(),
            Some("session-1".into())
        );
        let sources = session_artifact_sources();
        journal
            .enqueue_session_artifacts("session-1", &sources)
            .unwrap();
        journal
            .enqueue_session_artifacts("session-1", &sources)
            .unwrap();
        assert!(
            journal
                .next_session_artifact_freeze_candidate()
                .unwrap()
                .is_none()
        );

        let registration = journal.session_registration_candidate("session-1").unwrap();
        assert_eq!(registration.registration_state, "completed");
        assert_eq!(registration.remote_session_id, None);
        assert_eq!(
            (registration.width, registration.height),
            (profile.width, profile.height)
        );

        assert!(
            journal.claim_next_session_artifact_job().unwrap().is_none(),
            "session artifacts stay local until the manual gate is opened"
        );
        journal
            .connection
            .lock()
            .execute(
                "UPDATE upload_sessions SET upload_requested = 1,
                        upload_requested_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now')
                  WHERE session_id = 'session-1'",
                [],
            )
            .unwrap();
        let claimed = journal.claim_next_session_artifact_job().unwrap().unwrap();
        assert_eq!(claimed.state, SessionArtifactUploadState::Pending);
        assert_eq!(claimed.attempts, 1);
        let statuses = sources
            .iter()
            .map(|source| SessionArtifactRemoteStatus {
                kind: source.kind,
                object_key: format!("raw/session-1/session/{}", source.kind.file_name()),
                expected_sha256: source.expected_sha256.clone(),
                expected_size: source.expected_size,
                verification_state: "unverified".into(),
            })
            .collect::<Vec<_>>();
        journal
            .set_session_artifacts_declared("session-1", &statuses)
            .unwrap();

        let deletion_error = journal
            .prepare_session_deletion("delete-1", "session-1", "delete/session-1")
            .unwrap_err();
        assert!(
            format!("{deletion_error:#}").contains("unfinished session-artifact uploads"),
            "unexpected deletion interlock error: {deletion_error:#}"
        );

        journal
            .mark_session_artifact_signed("session-1", claimed.kind)
            .unwrap();
        journal
            .mark_session_artifact_signed("session-1", claimed.kind)
            .unwrap();
        journal
            .retry_session_artifact("session-1", claimed.kind, "UPLOAD_API_UNAVAILABLE", 1)
            .unwrap();
        assert_eq!(
            journal
                .session_artifact_jobs("session-1")
                .unwrap()
                .into_iter()
                .find(|job| job.kind == claimed.kind)
                .unwrap()
                .state,
            SessionArtifactUploadState::Signed
        );
        journal
            .mark_session_artifact_put("session-1", claimed.kind)
            .unwrap();
        journal
            .mark_session_artifact_completing("session-1", claimed.kind)
            .unwrap();
        journal
            .finish_session_artifact("session-1", claimed.kind, true)
            .unwrap();

        for source in sources.iter().filter(|source| source.kind != claimed.kind) {
            journal
                .reject_session_artifact("session-1", source.kind, "UPLOAD_REMOTE_REJECTED")
                .unwrap();
        }
        let connection = journal.connection.lock();
        let terminal: (i64, i64) = connection
            .query_row(
                "SELECT SUM(state = 'verified'), SUM(state = 'rejected')
                   FROM session_artifact_uploads
                  WHERE session_id = 'session-1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        // 一份被采纳 + 其余全被拒。册子合并后 `sources` 少了一条（capture 与 settings
        // 并成 recording，docs/57 §3.1），所以被拒的从 6 变 5。
        assert_eq!(terminal, (1, 5));
        drop(connection);
        journal
            .prepare_session_deletion("delete-1", "session-1", "delete/session-1")
            .unwrap();
    }

    #[test]
    fn session_artifact_declaration_rejects_remote_identity_changes() {
        let dir = tempfile::tempdir().unwrap();
        let journal = SessionJournal::open(dir.path().join("journal.sqlite3")).unwrap();
        let profile = profile();
        let mapping = MappingSnapshot::from_slice(MAPPING).unwrap();
        begin(&journal, &profile, &mapping);
        complete_empty_data_model_session(&journal, dir.path(), &mapping);
        let sources = session_artifact_sources();
        journal
            .enqueue_session_artifacts("session-1", &sources)
            .unwrap();
        let mut statuses = sources
            .iter()
            .map(|source| SessionArtifactRemoteStatus {
                kind: source.kind,
                object_key: format!("raw/session-1/session/{}", source.kind.file_name()),
                expected_sha256: source.expected_sha256.clone(),
                expected_size: source.expected_size,
                verification_state: "unverified".into(),
            })
            .collect::<Vec<_>>();
        statuses[0].expected_size += 1;
        let error = journal
            .set_session_artifacts_declared("session-1", &statuses)
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("conflicts with local"),
            "unexpected declaration conflict: {error:#}"
        );
        assert!(
            journal
                .session_artifact_jobs("session-1")
                .unwrap()
                .iter()
                .all(|job| job.state == SessionArtifactUploadState::Pending)
        );
        journal
            .reject_session_artifact("session-1", sources[0].kind, "UPLOAD_SOURCE_CHANGED")
            .unwrap();
        let rejected = journal
            .session_artifact_jobs("session-1")
            .unwrap()
            .into_iter()
            .find(|job| job.kind == sources[0].kind)
            .unwrap();
        assert_eq!(rejected.state, SessionArtifactUploadState::Rejected);
        assert_eq!(rejected.object_key, None);
    }

    #[test]
    fn manual_upload_uses_ready_raw_even_when_data_model_failed() {
        let connection = Connection::open_in_memory().unwrap();
        migrate(&connection).unwrap();
        connection
            .execute_batch(
                "INSERT INTO sessions(
                    id, state, non_trainable, production_certified, consent_id,
                    game_id, game_exe_hash, recording_profile_id, recording_profile_hash,
                    mapping_id, mapping_version, mapping_hash,
                    schema_version, started_at_utc, created_at_utc, updated_at_utc
                 ) VALUES (
                    'manual', 'completed', 1, 0, 'CNS-20260725-001', 'game', 'hash', 'rp', 'rph',
                    'map', '1', 'mh', 'dm-local-dev-session-2', 't', 't', 't'
                 );
                 INSERT INTO capture_runs(
                    id, session_id, run_name, seq, state, start_request_id, backend,
                    target_pid, target_window_id, encoder_backend, fps_num, fps_den,
                    width, height, session_frame_start, created_at_utc, updated_at_utc
                 ) VALUES (
                    'manual:r0000', 'manual', 'r0000', 0, 'completed', 'req-manual',
                    'native_wgc_window_v1', 1, 'win', 'mf_nvidia_hardware_v1',
                    30, 1, 1920, 1080, 0, 't', 't'
                 );
                 INSERT INTO segments(
                    id, session_id, run_id, run_name, seq, state, session_frame_start,
                    created_at_utc, updated_at_utc
                 ) VALUES
                    ('manual:r0000:g0', 'manual', 'manual:r0000', 'r0000', 0, 'ready', 0, 't', 't'),
                    ('manual:r0000:g1', 'manual', 'manual:r0000', 'r0000', 1, 'finalizing', 600, 't', 't');
                 INSERT INTO data_model_jobs(
                    session_id, state, ready_segments, processed_segments,
                    published_chunks, published_frames, pending_frames,
                    finalize_requested, error_code, created_at_utc, updated_at_utc
                 ) VALUES (
                    'manual', 'failed', 1, 0, 0, 0, 0, 1,
                    'DATA_MODEL_VALIDATION_FAILED', 't', 't'
                 );",
            )
            .unwrap();
        let journal = SessionJournal {
            connection: Mutex::new(connection),
        };

        let partial = load_session_summary(&journal.connection.lock(), "manual").unwrap();
        assert_eq!(partial.segment_count, 2);
        assert_eq!(partial.ready_segments, 1);
        assert_eq!(partial.upload_job_segments, 0);
        assert!(!partial.upload_requestable);
        let error = journal.request_session_upload("manual").unwrap_err();
        assert!(format!("{error:#}").contains("every RAW segment to be ready (1/2)"));
        let jobs: i64 = journal
            .connection
            .lock()
            .query_row("SELECT COUNT(*) FROM upload_jobs", [], |row| row.get(0))
            .unwrap();
        assert_eq!(jobs, 0, "incomplete RAW must not create upload work");

        journal
            .connection
            .lock()
            .execute(
                "UPDATE segments SET state = 'ready' WHERE id = 'manual:r0000:g1'",
                [],
            )
            .unwrap();
        let complete = load_session_summary(&journal.connection.lock(), "manual").unwrap();
        assert_eq!(complete.ready_segments, 2);
        assert_eq!(complete.upload_job_segments, 0);
        assert!(complete.upload_requestable);
        assert!(!complete.performance_validation_only);
        journal
            .connection
            .lock()
            .execute(
                "UPDATE capture_runs SET performance_validation_only = 1
                  WHERE id = 'manual:r0000'",
                [],
            )
            .unwrap();
        let validation = load_session_summary(&journal.connection.lock(), "manual").unwrap();
        assert!(validation.performance_validation_only);
        assert!(!validation.upload_requestable);
        assert_eq!(validation.upload_job_segments, 0);
        let serialized = serde_json::to_value(&validation).unwrap();
        assert_eq!(serialized["performanceValidationOnly"], true);
        let validation_error = journal.request_session_upload("manual").unwrap_err();
        assert!(
            format!("{validation_error:#}").contains("permanently non-deliverable"),
            "unexpected performance validation upload error: {validation_error:#}"
        );
        let jobs: i64 = journal
            .connection
            .lock()
            .query_row("SELECT COUNT(*) FROM upload_jobs", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            jobs, 0,
            "validation refusal must happen before queue mutation"
        );
        journal
            .connection
            .lock()
            .execute(
                "UPDATE capture_runs SET performance_validation_only = 0
                  WHERE id = 'manual:r0000'",
                [],
            )
            .unwrap();
        let requested = journal.request_session_upload("manual").unwrap();
        assert!(requested.upload_requested);
        assert_eq!(requested.upload_job_segments, 2);
        let data_model: (String, String) = journal
            .connection
            .lock()
            .query_row(
                "SELECT state, error_code FROM data_model_jobs WHERE session_id = 'manual'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            data_model,
            ("failed".into(), "DATA_MODEL_VALIDATION_FAILED".into())
        );
        assert_eq!(
            journal.claim_next_upload_job().unwrap().unwrap().segment_id,
            "manual:r0000:g0"
        );
        journal
            .connection
            .lock()
            .execute(
                "UPDATE upload_jobs SET state = 'stored' WHERE session_id = 'manual'",
                [],
            )
            .unwrap();
        assert!(
            journal.session_can_be_sealed("manual").unwrap(),
            "failed DataModel must not block sealing a complete stored RAW queue"
        );
    }

    #[test]
    fn manual_request_enqueues_ready_segments_with_their_classification_facts() {
        let connection = Connection::open_in_memory().unwrap();
        migrate(&connection).unwrap();
        // Minimal ready segment plus the run row its foreign key requires.
        connection
            .execute_batch(
                "INSERT INTO sessions(
                    id, state, non_trainable, production_certified, consent_id,
                    game_id, game_exe_hash, recording_profile_id, recording_profile_hash,
                    mapping_id, mapping_version, mapping_hash,
                    schema_version, started_at_utc, created_at_utc, updated_at_utc
                 ) VALUES (
                    'rel', 'completed', 1, 0, 'CNS-20260725-001', 'game', 'hash', 'rp', 'rph',
                    'map', '1', 'mh', 'dm-local-dev-session-2', 't', 't', 't'
                 );
                 INSERT INTO capture_runs(
                    id, session_id, run_name, seq, state, start_request_id, backend,
                    target_pid, target_window_id, encoder_backend, fps_num, fps_den,
                    width, height, session_frame_start, created_at_utc, updated_at_utc
                 ) VALUES (
                    'rel:r0000', 'rel', 'r0000', 0, 'completed', 'req-1', 'native_wgc_window_v1',
                    1, 'win', 'mf_nvidia_hardware_v1', 30, 1, 1920, 1080, 0, 't', 't'
                 );
                 INSERT INTO segments(
                    id, session_id, run_id, run_name, seq, state, session_frame_start,
                    created_at_utc, updated_at_utc
                 ) VALUES (
                    'rel:r0000:g0', 'rel', 'rel:r0000', 'r0000', 0, 'ready', 0, 't', 't'
                 );",
            )
            .unwrap();

        let jobs: i64 = connection
            .query_row("SELECT COUNT(*) FROM upload_jobs", [], |row| row.get(0))
            .unwrap();
        let upload_sessions: i64 = connection
            .query_row("SELECT COUNT(*) FROM upload_sessions", [], |row| row.get(0))
            .unwrap();
        assert_eq!(jobs, 0, "ready RAW waits for an explicit upload request");
        assert_eq!(upload_sessions, 0);
        let pending = load_session_summary(&connection, "rel").unwrap();
        assert_eq!(pending.upload_pending_segments, 0);
        assert!(!pending.upload_requested);
        assert!(pending.upload_requestable);

        let journal = SessionJournal {
            connection: Mutex::new(connection),
        };
        assert!(journal.claim_next_upload_job().unwrap().is_none());
        let requested = journal.request_session_upload("rel").unwrap();
        assert!(requested.upload_requested);
        assert_eq!(requested.upload_pending_segments, 1);
        assert!(!requested.upload_requestable);
        assert!(requested.upload_requested_at_utc.is_some());

        // The facts the uploader's classification envelope derives from.
        let (non_trainable, consent_id): (i64, Option<String>) = journal
            .connection
            .lock()
            .query_row(
                "SELECT s.non_trainable, s.consent_id
                   FROM upload_jobs j JOIN sessions s ON s.id = j.session_id",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(non_trainable, 1);
        assert_eq!(consent_id.as_deref(), Some("CNS-20260725-001"));

        journal
            .connection
            .lock()
            .execute_batch(
                "UPDATE upload_jobs
                    SET state = 'retry_wait', attempt_count = 2,
                        next_attempt_at_ms = 123456789,
                        last_error_code = 'UPLOAD_R2_UNAVAILABLE';
                 INSERT INTO upload_parts(
                    segment_id, artifact_kind, part_number, byte_offset, byte_size,
                    content_md5, etag, uploaded_at_utc
                 ) VALUES (
                    'rel:r0000:g0', 'video', 1, 0, 123,
                    'md5', 'etag', 't'
                 );",
            )
            .unwrap();
        let detail = journal.session_detail("rel").unwrap();
        assert_eq!(detail.session.upload_pending_segments, 0);
        assert_eq!(detail.session.upload_retry_wait_segments, 1);
        assert_eq!(
            detail.session.upload_last_error_code.as_deref(),
            Some("UPLOAD_R2_UNAVAILABLE")
        );
        assert_eq!(
            detail.segments[0].upload_state.as_deref(),
            Some("retry_wait")
        );
        assert_eq!(detail.segments[0].upload_attempt_count, Some(2));
        assert_eq!(
            detail.segments[0].upload_error_code.as_deref(),
            Some("UPLOAD_R2_UNAVAILABLE")
        );
        assert_eq!(detail.segments[0].uploaded_part_count, 1);
        assert_eq!(detail.segments[0].uploaded_bytes, 123);

        journal
            .connection
            .lock()
            .execute(
                "UPDATE upload_jobs SET next_attempt_at_ms = 0 WHERE session_id = 'rel'",
                [],
            )
            .unwrap();
        assert!(
            journal.claim_next_upload_job().unwrap().is_some(),
            "the explicit request must open the durable network gate"
        );
    }

    #[test]
    fn manual_request_recovers_only_legacy_device_profile_mismatch_jobs() {
        let dir = tempfile::tempdir().unwrap();
        let journal = SessionJournal::open(dir.path().join("journal.sqlite3")).unwrap();
        let profile = profile();
        let mapping = MappingSnapshot::from_slice(MAPPING).unwrap();
        begin(&journal, &profile, &mapping);
        for index in 0..6 {
            insert_ready_segment_row(&journal, "session-1", "r0000", index, u64::from(index), 1);
        }
        journal
            .connection
            .lock()
            .execute_batch(
                "UPDATE sessions SET state = 'completed';
                 UPDATE capture_runs SET state = 'completed';",
            )
            .unwrap();
        journal.request_session_upload("session-1").unwrap();

        let segment_id = |index| segment_journal_id("session-1", "r0000", index);
        {
            let connection = journal.connection.lock();
            connection
                .execute_batch(
                    "UPDATE upload_sessions
                        SET remote_session_id = 'remote-session-existing';
                     UPDATE upload_jobs
                        SET state = 'rejected', attempt_count = 7, next_attempt_at_ms = 12345,
                            last_error_code = 'UPLOAD_REQUEST_REJECTED', http_status = 409,
                            server_code = 'device_profile_mismatch', server_message = 'legacy rejection',
                            request_id = 'legacy-request';",
                )
                .unwrap();
            connection
                .execute(
                    "UPDATE upload_jobs SET server_code = 'other_conflict' WHERE segment_id = ?1",
                    params![segment_id(1)],
                )
                .unwrap();
            connection
                .execute(
                    "UPDATE upload_jobs SET last_error_code = 'UPLOAD_HASH_MISMATCH' WHERE segment_id = ?1",
                    params![segment_id(2)],
                )
                .unwrap();
            connection
                .execute(
                    "UPDATE upload_jobs SET http_status = 400 WHERE segment_id = ?1",
                    params![segment_id(3)],
                )
                .unwrap();
            connection
                .execute(
                    "UPDATE upload_jobs SET remote_clip_id = 'remote-clip' WHERE segment_id = ?1",
                    params![segment_id(4)],
                )
                .unwrap();
            connection
                .execute(
                    "UPDATE upload_jobs SET remote_upload_intent_id = 'remote-intent' WHERE segment_id = ?1",
                    params![segment_id(5)],
                )
                .unwrap();
        }

        let before = journal.session_detail("session-1").unwrap().session;
        assert!(before.upload_requested);
        assert!(
            before.upload_requestable,
            "the precise legacy rejection must offer manual recovery"
        );

        let recovered = journal.request_session_upload("session-1").unwrap();
        assert_eq!(recovered.upload_pending_segments, 1);
        assert_eq!(recovered.upload_rejected_segments, 5);
        assert!(!recovered.upload_requestable);

        struct RecoveredJob {
            state: String,
            attempt_count: i64,
            next_attempt_at_ms: i64,
            last_error_code: Option<String>,
            http_status: Option<i64>,
            server_code: Option<String>,
            server_message: Option<String>,
            request_id: Option<String>,
            remote_clip_id: Option<String>,
            remote_upload_intent_id: Option<String>,
        }
        let exact = journal
            .connection
            .lock()
            .query_row(
                "SELECT state, attempt_count, next_attempt_at_ms, last_error_code,
                        http_status, server_code, server_message, request_id,
                        remote_clip_id, remote_upload_intent_id
                   FROM upload_jobs WHERE segment_id = ?1",
                params![segment_id(0)],
                |row| {
                    Ok(RecoveredJob {
                        state: row.get(0)?,
                        attempt_count: row.get(1)?,
                        next_attempt_at_ms: row.get(2)?,
                        last_error_code: row.get(3)?,
                        http_status: row.get(4)?,
                        server_code: row.get(5)?,
                        server_message: row.get(6)?,
                        request_id: row.get(7)?,
                        remote_clip_id: row.get(8)?,
                        remote_upload_intent_id: row.get(9)?,
                    })
                },
            )
            .unwrap();
        assert_eq!(exact.state, "pending");
        assert_eq!(
            exact.attempt_count, 7,
            "recovery must preserve attempt history"
        );
        assert_eq!(exact.next_attempt_at_ms, 0);
        assert!(exact.last_error_code.is_none());
        assert!(exact.http_status.is_none());
        assert!(exact.server_code.is_none());
        assert!(exact.server_message.is_none());
        assert!(exact.request_id.is_none());
        assert!(exact.remote_clip_id.is_none());
        assert!(exact.remote_upload_intent_id.is_none());

        let connection = journal.connection.lock();
        let remote_session_id: Option<String> = connection
            .query_row(
                "SELECT remote_session_id FROM upload_sessions WHERE session_id = 'session-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            remote_session_id.as_deref(),
            Some("remote-session-existing")
        );
        for index in 1..6 {
            let state: String = connection
                .query_row(
                    "SELECT state FROM upload_jobs WHERE segment_id = ?1",
                    params![segment_id(index)],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                state, "rejected",
                "non-matching job {index} must stay terminal"
            );
        }
        let raw_ready: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM segments WHERE state = 'ready'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(raw_ready, 6, "recovery must not rewrite RAW facts");
        drop(connection);

        let replay = journal.request_session_upload("session-1").unwrap();
        assert_eq!(replay.upload_pending_segments, 1);
        let attempts: i64 = journal
            .connection
            .lock()
            .query_row(
                "SELECT attempt_count FROM upload_jobs WHERE segment_id = ?1",
                params![segment_id(0)],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(attempts, 7, "repeated clicks must be idempotent");
    }

    #[test]
    fn manual_request_recovers_only_missing_catalog_version_jobs() {
        let dir = tempfile::tempdir().unwrap();
        let journal = SessionJournal::open(dir.path().join("journal.sqlite3")).unwrap();
        let profile = profile();
        let mapping = MappingSnapshot::from_slice(MAPPING).unwrap();
        begin(&journal, &profile, &mapping);
        for index in 0..7 {
            insert_ready_segment_row(&journal, "session-1", "r0000", index, u64::from(index), 1);
        }
        journal
            .connection
            .lock()
            .execute_batch(
                "UPDATE sessions SET state = 'completed';
                 UPDATE capture_runs SET state = 'completed';",
            )
            .unwrap();
        journal.request_session_upload("session-1").unwrap();

        let segment_id = |index| segment_journal_id("session-1", "r0000", index);
        {
            let connection = journal.connection.lock();
            connection
                .execute_batch(
                    "UPDATE upload_sessions
                        SET remote_session_id = 'remote-session-existing';
                     UPDATE upload_jobs
                        SET state = 'rejected', attempt_count = 9, next_attempt_at_ms = 67890,
                            last_error_code = 'UPLOAD_REQUEST_REJECTED', http_status = 404,
                            server_code = 'catalog_version_not_found',
                            server_message = 'catalog identity was not found',
                            request_id = 'missing-catalog-request';",
                )
                .unwrap();
            connection
                .execute(
                    "UPDATE upload_jobs SET server_code = 'other_not_found' WHERE segment_id = ?1",
                    params![segment_id(1)],
                )
                .unwrap();
            connection
                .execute(
                    "UPDATE upload_jobs SET last_error_code = 'UPLOAD_HASH_MISMATCH' WHERE segment_id = ?1",
                    params![segment_id(2)],
                )
                .unwrap();
            connection
                .execute(
                    "UPDATE upload_jobs SET http_status = 409 WHERE segment_id = ?1",
                    params![segment_id(3)],
                )
                .unwrap();
            connection
                .execute(
                    "UPDATE upload_jobs SET remote_clip_id = 'remote-clip' WHERE segment_id = ?1",
                    params![segment_id(4)],
                )
                .unwrap();
            connection
                .execute(
                    "UPDATE upload_jobs SET remote_upload_intent_id = 'remote-intent' WHERE segment_id = ?1",
                    params![segment_id(5)],
                )
                .unwrap();
            connection
                .execute(
                    "UPDATE upload_jobs SET clip_revision = 1 WHERE segment_id = ?1",
                    params![segment_id(6)],
                )
                .unwrap();
        }

        let before = journal.session_detail("session-1").unwrap().session;
        assert!(before.upload_requested);
        assert!(
            before.upload_requestable,
            "the precise missing-catalog rejection must offer manual recovery"
        );

        let recovered = journal.request_session_upload("session-1").unwrap();
        assert_eq!(recovered.upload_pending_segments, 1);
        assert_eq!(recovered.upload_rejected_segments, 6);
        assert!(!recovered.upload_requestable);

        struct RecoveredCatalogJob {
            state: String,
            attempt_count: i64,
            next_attempt_at_ms: i64,
            last_error_code: Option<String>,
            http_status: Option<i64>,
            server_code: Option<String>,
            server_message: Option<String>,
            request_id: Option<String>,
        }
        let exact = journal
            .connection
            .lock()
            .query_row(
                "SELECT state, attempt_count, next_attempt_at_ms, last_error_code,
                        http_status, server_code, server_message, request_id
                   FROM upload_jobs WHERE segment_id = ?1",
                params![segment_id(0)],
                |row| {
                    Ok(RecoveredCatalogJob {
                        state: row.get(0)?,
                        attempt_count: row.get(1)?,
                        next_attempt_at_ms: row.get(2)?,
                        last_error_code: row.get(3)?,
                        http_status: row.get(4)?,
                        server_code: row.get(5)?,
                        server_message: row.get(6)?,
                        request_id: row.get(7)?,
                    })
                },
            )
            .unwrap();
        assert_eq!(exact.state, "pending");
        assert_eq!(
            exact.attempt_count, 9,
            "recovery must preserve attempt history"
        );
        assert_eq!(exact.next_attempt_at_ms, 0);
        assert!(exact.last_error_code.is_none());
        assert!(exact.http_status.is_none());
        assert!(exact.server_code.is_none());
        assert!(exact.server_message.is_none());
        assert!(exact.request_id.is_none());

        let connection = journal.connection.lock();
        let remote_session_id: Option<String> = connection
            .query_row(
                "SELECT remote_session_id FROM upload_sessions WHERE session_id = 'session-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            remote_session_id.as_deref(),
            Some("remote-session-existing")
        );
        for index in 1..7 {
            let state: String = connection
                .query_row(
                    "SELECT state FROM upload_jobs WHERE segment_id = ?1",
                    params![segment_id(index)],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                state, "rejected",
                "non-matching job {index} must stay terminal"
            );
        }
        let raw_ready: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM segments WHERE state = 'ready'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(raw_ready, 7, "recovery must not rewrite RAW facts");
        drop(connection);

        let replay = journal.request_session_upload("session-1").unwrap();
        assert_eq!(replay.upload_pending_segments, 1);
        let attempts: i64 = journal
            .connection
            .lock()
            .query_row(
                "SELECT attempt_count FROM upload_jobs WHERE segment_id = ?1",
                params![segment_id(0)],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(attempts, 9, "repeated clicks must be idempotent");
    }

    #[test]
    fn mirror_outbox_refuses_unredacted_shapes_and_never_reuses_a_seq() {
        let connection = Connection::open_in_memory().unwrap();
        migrate(&connection).unwrap();
        let insert = "INSERT INTO mirror_outbox(
                entity, entity_id, payload_json, observed_at_ms, created_at_utc
             ) VALUES (?1, ?2, ?3, ?4, 't')";

        // entity 是固定枚举：任何新事件类型都要显式加进 CHECK，
        // 而不是让调用方自己发明一个名字就能写进来。
        assert!(
            connection
                .execute(insert, params!["window_title", "a", "{}", 1i64])
                .is_err()
        );
        // payload 必须是 JSON：这里存的是脱敏投影，不是自由文本。
        assert!(
            connection
                .execute(insert, params!["segment", "a", "not json", 1i64])
                .is_err()
        );

        connection
            .execute(insert, params!["segment", "a", "{\"n\":1}", 1i64])
            .unwrap();
        connection
            .execute(insert, params!["segment", "b", "{\"n\":2}", 2i64])
            .unwrap();
        connection.execute("DELETE FROM mirror_outbox", []).unwrap();
        connection
            .execute(insert, params!["segment", "c", "{\"n\":3}", 3i64])
            .unwrap();

        // 清掉已确认的行之后 seq 仍然前进。复用序号会让云端把两条不同事件
        // 当成同一条幂等重放，静默丢掉后一条。
        let seq: i64 = connection
            .query_row("SELECT seq FROM mirror_outbox", [], |row| row.get(0))
            .unwrap();
        assert_eq!(seq, 3);
    }

    #[test]
    fn migrations_and_run_start_are_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let journal = SessionJournal::open(dir.path().join("journal.sqlite3")).unwrap();
        let profile = profile();
        let mapping = MappingSnapshot::from_slice(MAPPING).unwrap();
        begin(&journal, &profile, &mapping);
        begin(&journal, &profile, &mapping);

        let connection = journal.connection.lock();
        let sessions: i64 = connection
            .query_row("SELECT count(*) FROM sessions", [], |row| row.get(0))
            .unwrap();
        let runs: i64 = connection
            .query_row("SELECT count(*) FROM capture_runs", [], |row| row.get(0))
            .unwrap();
        let jobs: i64 = connection
            .query_row("SELECT count(*) FROM data_model_jobs", [], |row| row.get(0))
            .unwrap();
        let job_state: String = connection
            .query_row("SELECT state FROM data_model_jobs", [], |row| row.get(0))
            .unwrap();
        assert_eq!((sessions, runs), (1, 1));
        assert_eq!((jobs, job_state.as_str()), (1, "waiting"));
    }

    #[test]
    fn device_profile_mismatch_preflight_creates_no_recording_rows() {
        let dir = tempfile::tempdir().unwrap();
        let journal = SessionJournal::open(dir.path().join("journal.sqlite3")).unwrap();
        let profile = profile();
        let mapping = MappingSnapshot::from_slice(MAPPING).unwrap();
        let catalog = CatalogSelection::for_test(&profile, &mapping);
        let mut evidence = preflight::start(
            "request-device-runtime-mismatch",
            "window-digest",
            &catalog.catalog.content_hash,
            catalog.session_contract,
        )
        .unwrap();
        evidence = journal
            .begin_preflight(&PreflightStart {
                evidence: &evidence,
                target_window_id: "window-digest",
                game_id: &mapping.mapping().game.id,
                catalog: &catalog,
            })
            .unwrap();
        preflight::append_check(
            &mut evidence,
            "device_runtime",
            "blocked",
            serde_json::json!({
                "windowsChecked": true,
                "gpuChecked": true,
                "driverChecked": true,
                "hardwareEncoderChecked": true,
            }),
        )
        .unwrap();
        preflight::fail(
            &mut evidence,
            "device_runtime",
            "blocked",
            "DEVICE_PROFILE_RUNTIME_MISMATCH",
            "details-device-runtime-mismatch",
        )
        .unwrap();
        journal.update_preflight(&evidence).unwrap();

        let connection = journal.connection.lock();
        let rows: (i64, i64, i64) = connection
            .query_row(
                "SELECT (SELECT COUNT(*) FROM sessions),
                        (SELECT COUNT(*) FROM capture_runs),
                        (SELECT COUNT(*) FROM segments)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(rows, (0, 0, 0));
        let persisted = load_preflight_by_id(&connection, &evidence.preflight_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            persisted.failure_code.as_deref(),
            Some("DEVICE_PROFILE_RUNTIME_MISMATCH")
        );
    }

    #[test]
    fn begin_run_rejects_any_preflight_catalog_identity_drift() {
        let dir = tempfile::tempdir().unwrap();
        let journal = SessionJournal::open(dir.path().join("journal.sqlite3")).unwrap();
        let profile = profile();
        let mapping = MappingSnapshot::from_slice(MAPPING).unwrap();
        let catalog = CatalogSelection::for_test(&profile, &mapping);
        let mut evidence = preflight::start(
            "preflight-catalog-drift",
            "window-digest",
            &catalog.catalog.content_hash,
            catalog.session_contract,
        )
        .unwrap();
        evidence = journal
            .begin_preflight(&PreflightStart {
                evidence: &evidence,
                target_window_id: "window-digest",
                game_id: &mapping.mapping().game.id,
                catalog: &catalog,
            })
            .unwrap();
        preflight::append_check(
            &mut evidence,
            "test_fixture",
            "dev_non_trainable_pass",
            serde_json::json!({ "fixture": true }),
        )
        .unwrap();
        preflight::link_run(&mut evidence, "session-drift", "r0000").unwrap();
        preflight::pass(&mut evidence).unwrap();

        let persisted = {
            let connection = journal.connection.lock();
            load_preflight_catalog_identity(&connection, &evidence.preflight_id)
                .unwrap()
                .unwrap()
        };
        assert!(persisted.matches("window-digest", &catalog));

        let request_error = journal
            .begin_run(&RunStart {
                request_id: "different-run-request",
                session_id: "session-drift",
                run_id: "r0000",
                run_seq: 0,
                session_frame_start: 0,
                depth_expected: false,
                performance_validation_only: false,
                process_id: 42,
                target_window_id: "window-digest",
                executable_sha256: Some("sha256:exe"),
                executable_sha256_missing_reason: None,
                profile: &profile,
                mapping: &mapping,
                catalog: &catalog,
                preflight: &evidence,
                view_calibration: None,
                consent_id: None,
            })
            .unwrap_err();
        assert!(format!("{request_error:#}").contains("does not match its preflight attempt"));
        assert!(!journal.session_exists("session-drift").unwrap());
        assert_eq!(
            journal
                .preflight_by_id(&evidence.preflight_id)
                .unwrap()
                .unwrap()
                .result,
            "running"
        );

        let mut drifted = catalog.clone();
        drifted.device.identity.version = "unexpected-version".into();
        let error = journal
            .begin_run(&RunStart {
                request_id: &evidence.request_id,
                session_id: "session-drift",
                run_id: "r0000",
                run_seq: 0,
                session_frame_start: 0,
                depth_expected: false,
                performance_validation_only: false,
                process_id: 42,
                target_window_id: "window-digest",
                executable_sha256: Some("sha256:exe"),
                executable_sha256_missing_reason: None,
                profile: &profile,
                mapping: &mapping,
                catalog: &drifted,
                preflight: &evidence,
                view_calibration: None,
                consent_id: None,
            })
            .unwrap_err();
        assert!(format!("{error:#}").contains("does not match its preflight evidence"));
        assert!(!journal.session_exists("session-drift").unwrap());
        assert_eq!(
            journal
                .preflight_by_id(&evidence.preflight_id)
                .unwrap()
                .unwrap()
                .result,
            "running"
        );
    }

    #[test]
    fn begin_run_rolls_back_preflight_pass_when_session_identity_rejects_commit() {
        let dir = tempfile::tempdir().unwrap();
        let journal = SessionJournal::open(dir.path().join("journal.sqlite3")).unwrap();
        let original_profile = profile();
        let mapping = MappingSnapshot::from_slice(MAPPING).unwrap();
        begin(&journal, &original_profile, &mapping);

        let mut changed_profile = profile();
        changed_profile.profile_id = "different-profile".into();
        let catalog = CatalogSelection::for_test(&changed_profile, &mapping);
        let mut evidence = preflight::start(
            "request-conflicting-session",
            "window-digest",
            &catalog.catalog.content_hash,
            catalog.session_contract,
        )
        .unwrap();
        evidence = journal
            .begin_preflight(&PreflightStart {
                evidence: &evidence,
                target_window_id: "window-digest",
                game_id: &mapping.mapping().game.id,
                catalog: &catalog,
            })
            .unwrap();
        preflight::append_check(
            &mut evidence,
            "test_fixture",
            "dev_non_trainable_pass",
            serde_json::json!({ "fixture": true }),
        )
        .unwrap();
        preflight::link_run(&mut evidence, "session-1", "r0001").unwrap();
        preflight::pass(&mut evidence).unwrap();

        let error = journal
            .begin_run(&RunStart {
                request_id: &evidence.request_id,
                session_id: "session-1",
                run_id: "r0001",
                run_seq: 1,
                session_frame_start: 0,
                depth_expected: false,
                performance_validation_only: false,
                process_id: 43,
                target_window_id: "window-digest",
                executable_sha256: Some("sha256:exe"),
                executable_sha256_missing_reason: None,
                profile: &changed_profile,
                mapping: &mapping,
                catalog: &catalog,
                preflight: &evidence,
                view_calibration: None,
                consent_id: None,
            })
            .unwrap_err();

        assert!(format!("{error:#}").contains("session journal identity"));
        assert_eq!(
            journal
                .preflight_by_id(&evidence.preflight_id)
                .unwrap()
                .unwrap()
                .result,
            "running"
        );
        let connection = journal.connection.lock();
        let conflicting_runs: i64 = connection
            .query_row(
                "SELECT count(*) FROM capture_runs WHERE run_name = 'r0001'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(conflicting_runs, 0);
    }

    #[test]
    fn startup_terminalizes_interrupted_preflight_once_with_stable_details() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.sqlite3");
        let profile = profile();
        let mapping = MappingSnapshot::from_slice(MAPPING).unwrap();
        let catalog = CatalogSelection::for_test(&profile, &mapping);
        let journal = SessionJournal::open(&path).unwrap();
        let evidence = preflight::start(
            "preflight-interrupted",
            "window-digest",
            &catalog.catalog.content_hash,
            catalog.session_contract,
        )
        .unwrap();
        journal
            .begin_preflight(&PreflightStart {
                evidence: &evidence,
                target_window_id: "window-digest",
                game_id: &mapping.mapping().game.id,
                catalog: &catalog,
            })
            .unwrap();
        drop(journal);

        let reopened = SessionJournal::open(&path).unwrap();
        let recovered = reopened
            .preflight_by_request_id("preflight-interrupted")
            .unwrap()
            .unwrap();
        assert_eq!(recovered.result, "failed");
        assert_eq!(
            recovered.failure_code.as_deref(),
            Some("PREFLIGHT_INTERRUPTED")
        );
        assert_eq!(recovered.phase, "startup_recovery");
        assert!(
            recovered
                .details_id
                .as_deref()
                .is_some_and(|id| { id.starts_with("details-preflight-") })
        );
        assert_eq!(
            reopened.list_preflights(10).unwrap(),
            vec![recovered.clone()]
        );
        let recovered_by_id = reopened
            .preflight_by_id(&recovered.preflight_id)
            .unwrap()
            .unwrap();
        assert_eq!(recovered_by_id, recovered);
        drop(reopened);

        let replay = SessionJournal::open(&path).unwrap();
        assert_eq!(
            replay
                .preflight_by_request_id("preflight-interrupted")
                .unwrap()
                .unwrap(),
            recovered
        );
    }

    #[test]
    fn version_one_journal_migrates_through_marker_schema() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.sqlite3");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE journal_meta (
                    singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
                    schema_version INTEGER NOT NULL
                 );
                 INSERT INTO journal_meta(singleton, schema_version) VALUES (1, 1);
                 CREATE TABLE sessions (
                    id TEXT PRIMARY KEY,
                    state TEXT NOT NULL,
                    non_trainable INTEGER NOT NULL,
                    production_certified INTEGER NOT NULL,
                    annotator_id TEXT,
                    device_id TEXT,
                    consent_id TEXT,
                    game_id TEXT NOT NULL,
                    game_exe_hash TEXT NOT NULL,
                    capture_profile_id TEXT NOT NULL,
                    capture_profile_hash TEXT NOT NULL,
                    settings_profile_id TEXT NOT NULL,
                    mapping_id TEXT NOT NULL,
                    mapping_version TEXT NOT NULL,
                    mapping_hash TEXT NOT NULL,
                    schema_version TEXT NOT NULL,
                    started_at_utc TEXT NOT NULL,
                    ended_at_utc TEXT,
                    stop_reason TEXT,
                    last_error_code TEXT,
                    created_at_utc TEXT NOT NULL,
                    updated_at_utc TEXT NOT NULL
                 );
                 INSERT INTO sessions (
                    id, state, non_trainable, production_certified,
                    game_id, game_exe_hash, capture_profile_id, capture_profile_hash,
                    settings_profile_id, mapping_id, mapping_version, mapping_hash,
                    schema_version, started_at_utc, created_at_utc, updated_at_utc
                 ) VALUES (
                    'legacy-session', 'completed', 1, 0,
                    'test', 'sha256:exe', 'test-profile', 'sha256:legacy-reencoded',
                    'test-settings-v1', 'game.test.pc', '1', 'sha256:mapping',
                    'dm-local-dev-session-1', '2026-01-01T00:00:00Z',
                    '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z'
                 );",
            )
            .unwrap();
        drop(connection);

        let journal = SessionJournal::open(&path).unwrap();
        let connection = journal.connection.lock();
        let version: i64 = connection
            .query_row("SELECT schema_version FROM journal_meta", [], |row| {
                row.get(0)
            })
            .unwrap();
        let recovery_tables: i64 = connection
            .query_row(
                "SELECT count(*) FROM sqlite_master
                 WHERE type = 'table' AND name IN ('recovery_runs', 'recovery_findings')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let durable_tables: i64 = connection
            .query_row(
                "SELECT count(*) FROM sqlite_master
                 WHERE type = 'table' AND name IN (
                    'session_deletions', 'markers', 'heartbeats', 'data_model_jobs'
                 )",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let legacy_hash_semantics: (String, String, String) = connection
            .query_row(
                "SELECT recording_profile_hash, recording_profile_hash_kind, catalog_hash
                 FROM sessions WHERE id = 'legacy-session'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        let legacy_job: (String, i64, i64) = connection
            .query_row(
                "SELECT state, ready_segments, finalize_requested
                 FROM data_model_jobs WHERE session_id = 'legacy-session'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(version, JOURNAL_SCHEMA_VERSION);
        assert_eq!(recovery_tables, 2);
        assert_eq!(durable_tables, 4);
        assert_eq!(legacy_job, ("pending".into(), 0, 1));
        assert_eq!(
            legacy_hash_semantics,
            (
                "sha256:legacy-reencoded".into(),
                "legacy_reencoded_json_sha256".into(),
                "legacy-unavailable".into(),
            )
        );
    }

    #[test]
    fn heartbeat_sequence_is_durable_contiguous_and_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let journal = SessionJournal::open(dir.path().join("journal.sqlite3")).unwrap();
        let profile = profile();
        let mapping = MappingSnapshot::from_slice(MAPPING).unwrap();
        begin(&journal, &profile, &mapping);
        journal
            .mark_run_started("session-1", "r0000", 100, 10_000_000)
            .unwrap();

        let first = journal
            .record_heartbeat(&HeartbeatWrite {
                session_id: "session-1",
                run_id: "r0000",
                qpc: 100,
            })
            .unwrap();
        let replay = journal
            .record_heartbeat(&HeartbeatWrite {
                session_id: "session-1",
                run_id: "r0000",
                qpc: 100,
            })
            .unwrap();
        let second = journal
            .record_heartbeat(&HeartbeatWrite {
                session_id: "session-1",
                run_id: "r0000",
                qpc: 10_000_100,
            })
            .unwrap();

        assert_eq!(first, replay);
        assert_eq!(first.seq, 0);
        assert_eq!(second.seq, 1);
        drop(journal);

        let reopened = SessionJournal::open(dir.path().join("journal.sqlite3")).unwrap();
        assert_eq!(
            reopened.heartbeats_for_session("session-1").unwrap(),
            vec![first, second]
        );
    }

    /// 一个 v5 期的库迁到今天：`data_model_jobs` 被建出来，并按现有 ready segment 回填。
    ///
    /// **为什么用 `migrate_to` 造真库，而不是把 `schema_version` 回拨到 5。**
    /// 回拨这一招在「迁移只加列」的年代成立；v16 删列改名之后不成立——重放时
    /// v10 / v14 / v16 三次 `sessions` 重建各自按**自己那一版的列名**去读源表，而表
    /// 此刻已经是 v16 的形状，v10 第一个就撞 `no such column: capture_profile_id`。
    /// 一张张往回还原（`sessions` 的 v5 形状、`preflight_attempts` 连列序、
    /// `view_calibrations` 的 v15 改名……）是在给一个**现场根本不存在的状态**做适配：
    /// 现场的版本号只会往前走，现场的 v5 库就是这条迁移链停在 5 时的样子。所以直接
    /// 让它停在 5。见 docs/62 §三。
    #[test]
    fn version_five_journal_migrates_data_model_job_schema_idempotently() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.sqlite3");
        let connection = Connection::open(&path).unwrap();
        migrate_to(&connection, 5).unwrap();
        let staged_version: i64 = connection
            .query_row("SELECT schema_version FROM journal_meta", [], |row| {
                row.get(0)
            })
            .unwrap();
        let staged_job_table: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type = 'table' AND name = 'data_model_jobs'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(staged_version, 5);
        // data_model_jobs 由 v5→v6 建出来。它此刻不存在，正是本用例要让迁移去做的事
        // ——否则这个用例会在一个已经有表的库上「验证」建表。
        assert_eq!(staged_job_table, 0);

        // v5 期的三行数据，只写当时**必填**的列（v4→v5 ALTER 上去的 13 列都有
        // DEFAULT）。capture_runs 那一行不是摆设：v9→v10 收尾有一次
        // `pragma_foreign_key_check`，segments.run_id 悬空会让整条链停在那里。
        connection
            .execute_batch(
                "INSERT INTO sessions (
                    id, state, non_trainable, production_certified,
                    game_id, game_exe_hash, capture_profile_id, capture_profile_hash,
                    settings_profile_id, mapping_id, mapping_version, mapping_hash,
                    schema_version, started_at_utc, created_at_utc, updated_at_utc
                 ) VALUES (
                    'session-1', 'active', 1, 0,
                    'test', 'sha256:exe', 'test-profile', 'sha256:legacy-reencoded',
                    'test-settings-v1', 'game.test.pc', '1', 'sha256:mapping',
                    'dm-local-dev-session-1', '2026-01-01T00:00:00Z',
                    '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z'
                 );
                 INSERT INTO capture_runs (
                    id, session_id, run_name, seq, state, start_request_id, backend,
                    target_pid, target_window_id, encoder_backend, fps_num, fps_den,
                    width, height, session_frame_start, created_at_utc, updated_at_utc
                 ) VALUES (
                    'run-1', 'session-1', 'r0000', 0, 'completed', 'start-1',
                    'native_wgc_window_v1', 4242, 'window-1', 'mf_nvidia_hardware_v1',
                    60, 1, 1920, 1080, 0,
                    '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z'
                 );
                 INSERT INTO segments (
                    id, session_id, run_id, run_name, seq, state,
                    session_frame_start, n_frames, created_at_utc, updated_at_utc
                 ) VALUES (
                    'segment-1', 'session-1', 'run-1', 'r0000', 0, 'ready',
                    0, 300, '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z'
                 );",
            )
            .unwrap();
        drop(connection);

        let journal = SessionJournal::open(&path).unwrap();
        let connection = journal.connection.lock();
        let version: i64 = connection
            .query_row("SELECT schema_version FROM journal_meta", [], |row| {
                row.get(0)
            })
            .unwrap();
        let tables: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type = 'table' AND name = 'data_model_jobs'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, JOURNAL_SCHEMA_VERSION);
        assert_eq!(tables, 1);
        drop(connection);
        let backfilled = journal.data_model_job("session-1").unwrap().unwrap();
        assert_eq!(backfilled.state, DataModelJobState::Pending);
        assert_eq!(backfilled.ready_segments, 1);
        assert!(!backfilled.finalize_requested);

        drop(journal);
        let replay = SessionJournal::open(&path).unwrap();
        assert_eq!(
            replay.list_processable_data_model_jobs(10).unwrap(),
            vec![backfilled]
        );
    }

    #[test]
    fn startup_requeues_interrupted_data_model_claims() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.sqlite3");
        let profile = profile();
        let mapping = MappingSnapshot::from_slice(MAPPING).unwrap();
        {
            let journal = SessionJournal::open(&path).unwrap();
            begin(&journal, &profile, &mapping);
            let connection = journal.connection.lock();
            connection
                .execute(
                    "UPDATE data_model_jobs SET state = 'running' WHERE session_id = 'session-1'",
                    [],
                )
                .unwrap();
        }

        {
            let reopened = SessionJournal::open(&path).unwrap();
            let recovered = reopened.data_model_job("session-1").unwrap().unwrap();
            assert_eq!(recovered.state, DataModelJobState::Pending);
            assert_eq!(
                recovered.error_code.as_deref(),
                Some("DATAMODEL_WORKER_INTERRUPTED")
            );
            let claimed = reopened.claim_next_data_model_job().unwrap().unwrap();
            assert_eq!(claimed.state, DataModelJobState::Running);
            let released = reopened
                .release_data_model_job("session-1", Some("DATAMODEL_SOURCE_NOT_READY"))
                .unwrap();
            assert_eq!(released.state, DataModelJobState::Pending);
            assert_eq!(
                released.error_code.as_deref(),
                Some("DATAMODEL_SOURCE_NOT_READY")
            );
            let connection = reopened.connection.lock();
            connection
                .execute(
                    "UPDATE data_model_jobs
                     SET state = 'finalizing', finalize_requested = 1
                     WHERE session_id = 'session-1'",
                    [],
                )
                .unwrap();
        }

        let reopened = SessionJournal::open(&path).unwrap();
        let recovered = reopened.data_model_job("session-1").unwrap().unwrap();
        assert_eq!(recovered.state, DataModelJobState::Pending);
        assert!(recovered.finalize_requested);
        assert_eq!(
            reopened.claim_next_data_model_job().unwrap().unwrap().state,
            DataModelJobState::Finalizing
        );
    }

    #[test]
    fn startup_requeues_interrupted_upload_claims_without_touching_raw() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.sqlite3");
        let profile = profile();
        let mapping = MappingSnapshot::from_slice(MAPPING).unwrap();
        {
            let journal = SessionJournal::open(&path).unwrap();
            begin(&journal, &profile, &mapping);
            journal
                .mark_run_started("session-1", "r0000", 100, 10_000_000)
                .unwrap();
            journal
                .mark_segment_started("session-1", "r0000", 0, 0)
                .unwrap();
            journal
                .mark_segment_finalizing("session-1", "r0000", 0, 0, 300)
                .unwrap();
            let run_directory = dir.path().join("session-1/runs/r0000");
            let (_, published) = write_ready_segment(&run_directory, &mapping, 0);
            journal
                .mark_segment_ready("session-1", "r0000", &published, &run_directory)
                .unwrap();
            journal.claim_next_data_model_job().unwrap().unwrap();
            journal
                .save_data_model_progress(
                    "session-1",
                    &DataModelProgress {
                        processed_segments: 1,
                        published_chunks: 2,
                        published_frames: 240,
                        pending_frames: 60,
                    },
                )
                .unwrap();
            let mut outcome = zero_outcome(dir.path(), &mapping, RecordingStopReason::Manual);
            outcome.anchor_qpc = Some(100);
            outcome.qpc_start = Some(100);
            outcome.qpc_end = Some(200);
            outcome.frame_count = 300;
            outcome.session_frame_end = 300;
            outcome.segment_count = 1;
            journal
                .finish_run(&outcome, RunTerminalState::Completed)
                .unwrap();
            journal.request_session_upload("session-1").unwrap();
            journal
                .connection
                .lock()
                .execute("UPDATE upload_jobs SET state = 'running'", [])
                .unwrap();
        }

        let reopened = SessionJournal::open(&path).unwrap();
        let recovered: (String, String) = reopened
            .connection
            .lock()
            .query_row(
                "SELECT state, last_error_code FROM upload_jobs",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            recovered,
            ("retry_wait".into(), "UPLOAD_WORKER_INTERRUPTED".into())
        );
        assert!(
            dir.path()
                .join("session-1/runs/r0000/segments/00000/video.mp4")
                .is_file()
        );
    }

    // docs/65 J-29/J-30：上传队列聚合与历史页逐段错误码。走真实的
    // enqueue → claim → retry → reject 状态机，不手捏聚合输入。
    #[test]
    fn upload_queue_status_and_history_expose_the_facts_the_ui_reports() {
        let dir = tempfile::tempdir().unwrap();
        let journal = SessionJournal::open(dir.path().join("journal.sqlite3")).unwrap();
        let profile = profile();
        let mapping = MappingSnapshot::from_slice(MAPPING).unwrap();
        begin(&journal, &profile, &mapping);
        journal
            .mark_run_started("session-1", "r0000", 100, 10_000_000)
            .unwrap();
        journal
            .mark_segment_started("session-1", "r0000", 0, 0)
            .unwrap();
        journal
            .mark_segment_finalizing("session-1", "r0000", 0, 0, 300)
            .unwrap();
        let run_directory = dir.path().join("session-1/runs/r0000");
        let (_, published) = write_ready_segment(&run_directory, &mapping, 0);
        journal
            .mark_segment_ready("session-1", "r0000", &published, &run_directory)
            .unwrap();
        // DataModel 转换只维护自己的进度，不再隐式创建 R2 上传任务。
        journal.claim_next_data_model_job().unwrap().unwrap();
        journal
            .save_data_model_progress(
                "session-1",
                &DataModelProgress {
                    processed_segments: 1,
                    published_chunks: 2,
                    published_frames: 240,
                    pending_frames: 60,
                },
            )
            .unwrap();

        // 上传只由已完成会话上的显式用户请求触发。
        journal
            .connection
            .lock()
            .execute_batch(
                "UPDATE sessions SET state = 'completed';
                 UPDATE capture_runs SET state = 'completed';",
            )
            .unwrap();
        journal.request_session_upload("session-1").unwrap();

        // 手动发起后 ready 分段已入队：pending 可见，还没有任何错误或退避。
        let status = journal.upload_queue_status().unwrap();
        assert_eq!(status.pending_segments, 1);
        assert_eq!(status.inflight_segments, 0);
        assert_eq!(status.rejected_segments, 0);
        assert_eq!(status.last_error_code, None);
        assert_eq!(status.next_retry_unix_ms, None);

        // uploader 只处理已收尾且已手动发起的会话，走一次 claim → retry。
        let job = journal.claim_next_upload_job().unwrap().unwrap();
        assert_eq!(
            journal.upload_queue_status().unwrap().inflight_segments,
            1,
            "a claimed job is in flight"
        );
        journal
            .retry_upload_job(
                &job.segment_id,
                60_000,
                UploadFailureDetails {
                    error_code: "UPLOAD_NETWORK_UNAVAILABLE",
                    http_status: None,
                    server_code: None,
                    server_message: None,
                    request_id: None,
                },
            )
            .unwrap();
        let status = journal.upload_queue_status().unwrap();
        assert_eq!(status.pending_segments, 1, "retry_wait counts as pending");
        assert_eq!(
            status.last_error_code.as_deref(),
            Some("UPLOAD_NETWORK_UNAVAILABLE")
        );
        assert!(
            status.next_retry_unix_ms.is_some(),
            "a backoff deadline is visible"
        );

        // 到点重试 → 终态拒收：计数、错误码与历史页逐段展开都要能看见。
        journal
            .connection
            .lock()
            .execute("UPDATE upload_jobs SET next_attempt_at_ms = 0", [])
            .unwrap();
        let job = journal.claim_next_upload_job().unwrap().unwrap();
        journal
            .reject_running_upload_job(
                &job.segment_id,
                "UPLOAD_CLASSIFICATION_MISMATCH",
                Some(409),
                Some("device_profile_mismatch"),
                Some("device profile does not match the registered device"),
                Some("request-device-profile"),
            )
            .unwrap();
        let status = journal.upload_queue_status().unwrap();
        assert_eq!(status.pending_segments, 0);
        assert_eq!(status.rejected_segments, 1);
        assert_eq!(
            status.last_error_code.as_deref(),
            Some("UPLOAD_CLASSIFICATION_MISMATCH")
        );
        assert_eq!(status.next_retry_unix_ms, None);

        let detail = journal.session_detail("session-1").unwrap();
        assert_eq!(detail.segments.len(), 1);
        assert_eq!(detail.segments[0].upload_state.as_deref(), Some("rejected"));
        assert_eq!(
            detail.segments[0].upload_error_code.as_deref(),
            Some("UPLOAD_CLASSIFICATION_MISMATCH"),
            "the per-segment rejection reason reaches the history contract (docs/65 J-29)"
        );
        assert_eq!(detail.segments[0].upload_http_status, Some(409));
        assert_eq!(
            detail.segments[0].upload_server_code.as_deref(),
            Some("device_profile_mismatch")
        );
        assert_eq!(
            detail.segments[0].upload_server_message.as_deref(),
            Some("device profile does not match the registered device")
        );
        assert_eq!(
            detail.segments[0].upload_request_id.as_deref(),
            Some("request-device-profile")
        );
        assert_eq!(detail.session.upload_last_http_status, Some(409));
        assert_eq!(
            detail.session.upload_last_server_code.as_deref(),
            Some("device_profile_mismatch")
        );
        assert_eq!(
            detail.session.upload_last_request_id.as_deref(),
            Some("request-device-profile")
        );
    }

    #[test]
    fn seven_stored_segments_share_one_bounded_session_seal_task() {
        let dir = tempfile::tempdir().unwrap();
        let journal = SessionJournal::open(dir.path().join("journal.sqlite3")).unwrap();
        let profile = profile();
        let mapping = MappingSnapshot::from_slice(MAPPING).unwrap();
        begin(&journal, &profile, &mapping);
        journal
            .mark_run_started("session-1", "r0000", 100, 10_000_000)
            .unwrap();
        let run_directory = dir.path().join("session-1/runs/r0000");
        for seq in 0..7 {
            let frame_start = u64::from(seq) * 300;
            journal
                .mark_segment_started("session-1", "r0000", seq, frame_start)
                .unwrap();
            journal
                .mark_segment_finalizing("session-1", "r0000", seq, frame_start, 300)
                .unwrap();
            let (_, published) = write_ready_segment(&run_directory, &mapping, seq);
            journal
                .mark_segment_ready("session-1", "r0000", &published, &run_directory)
                .unwrap();
        }
        journal
            .connection
            .lock()
            .execute_batch(
                "UPDATE sessions
                    SET state = 'completed', ended_at_utc = '2026-08-03T12:20:08.000Z',
                        stop_reason = 'manual';
                 UPDATE capture_runs SET state = 'completed';",
            )
            .unwrap();
        journal.request_session_upload("session-1").unwrap();
        journal
            .connection
            .lock()
            .execute_batch(
                "UPDATE upload_sessions
                    SET remote_session_id = 'e4bfc734-f1c5-4987-9aa3-bb06ebf00b69';
                 UPDATE upload_jobs SET state = 'stored';",
            )
            .unwrap();

        let first = journal.claim_upload_session_seal().unwrap().unwrap();
        assert_eq!(first.attempt_count, 1);
        assert_eq!(first.local_final_state, "completed");
        assert_eq!(first.stop_reason.as_deref(), Some("manual"));
        assert!(
            journal.claim_upload_session_seal().unwrap().is_none(),
            "all seven stored segments must share the one running session task"
        );

        journal
            .retry_upload_session_seal(
                "session-1",
                "UPLOAD_SESSION_SEAL_RETRYABLE",
                60_000,
                Some(503),
                Some("dependency_unavailable"),
                Some("dependency temporarily unavailable"),
                Some("request-1"),
            )
            .unwrap();
        assert!(journal.claim_upload_session_seal().unwrap().is_none());
        journal
            .connection
            .lock()
            .execute(
                "UPDATE upload_sessions SET seal_next_attempt_at_ms = 0 WHERE session_id = 'session-1'",
                [],
            )
            .unwrap();
        let second = journal.claim_upload_session_seal().unwrap().unwrap();
        assert_eq!(second.attempt_count, 2);
        journal
            .reject_upload_session_seal(
                "session-1",
                "UPLOAD_SESSION_SEAL_REJECTED",
                Some(400),
                Some("invalid_request"),
                Some("endedAt violates the session contract"),
                Some("request-2"),
            )
            .unwrap();
        assert!(
            journal.claim_upload_session_seal().unwrap().is_none(),
            "a non-retryable 400 is a durable terminal, not an infinite retry"
        );
        assert_eq!(
            journal
                .upload_queue_status()
                .unwrap()
                .last_error_code
                .as_deref(),
            Some("UPLOAD_SESSION_SEAL_REJECTED")
        );
        let snapshot = journal.diagnostic_snapshot(Some("session-1")).unwrap();
        let session = &snapshot.sessions[0];
        assert_eq!(
            session.ended_at_utc.as_deref(),
            Some("2026-08-03T12:20:08.000Z")
        );
        assert_eq!(session.upload_seal_http_status, Some(400));
        assert_eq!(
            session.upload_seal_server_code.as_deref(),
            Some("invalid_request")
        );
        assert_eq!(session.upload_seal_request_id.as_deref(), Some("request-2"));
    }

    #[test]
    fn duplicate_request_cannot_target_another_run() {
        let dir = tempfile::tempdir().unwrap();
        let journal = SessionJournal::open(dir.path().join("journal.sqlite3")).unwrap();
        let profile = profile();
        let mapping = MappingSnapshot::from_slice(MAPPING).unwrap();
        begin(&journal, &profile, &mapping);

        let identity_error = journal
            .begin_test_run(&TestRunStart {
                request_id: "request-1",
                session_id: "session-1",
                run_id: "r0000",
                run_seq: 99,
                session_frame_start: 42,
                process_id: 42,
                target_window_id: "window-digest",
                executable_sha256: Some("sha256:exe"),
                executable_sha256_missing_reason: None,
                profile: &profile,
                mapping: &mapping,
                view_calibration: None,
            })
            .unwrap_err();
        assert!(
            format!("{identity_error:#}").contains("replay changed persisted identity"),
            "unexpected replay identity error: {identity_error:#}"
        );

        let error = journal
            .begin_test_run(&TestRunStart {
                request_id: "request-1",
                session_id: "session-1",
                run_id: "r0001",
                run_seq: 1,
                session_frame_start: 0,
                process_id: 42,
                target_window_id: "window-digest",
                executable_sha256: Some("sha256:exe"),
                executable_sha256_missing_reason: None,
                profile: &profile,
                mapping: &mapping,
                view_calibration: None,
            })
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("preflight candidate is not linked"),
            "unexpected duplicate request error: {error:#}"
        );
        let connection = journal.connection.lock();
        let run_count: i64 = connection
            .query_row("SELECT count(*) FROM capture_runs", [], |row| row.get(0))
            .unwrap();
        assert_eq!(run_count, 1);
    }

    #[test]
    fn marker_is_idempotent_aligned_and_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.sqlite3");
        let profile = profile();
        let mapping = MappingSnapshot::from_slice(MAPPING).unwrap();
        {
            let journal = SessionJournal::open(&path).unwrap();
            begin(&journal, &profile, &mapping);
            journal
                .mark_run_started("session-1", "r0000", 100, 10_000_000)
                .unwrap();
            let first = journal
                .record_marker(&MarkerWrite {
                    request_id: "marker-request-1",
                    session_id: "session-1",
                    run_id: "r0000",
                    kind: MarkerKind::Manual,
                    qpc: 150,
                })
                .unwrap();
            let replay = journal
                .record_marker(&MarkerWrite {
                    request_id: "marker-request-1",
                    session_id: "session-1",
                    run_id: "r0000",
                    kind: MarkerKind::Manual,
                    qpc: 199,
                })
                .unwrap();
            assert_eq!(first, replay);
            assert_eq!(first.run_frame, 0);
            assert_eq!(first.video_time_us, 0);

            journal
                .mark_segment_started("session-1", "r0000", 0, 0)
                .unwrap();
            journal
                .mark_segment_finalizing("session-1", "r0000", 0, 0, 300)
                .unwrap();
            let run_directory = dir.path().join("session-1/runs/r0000");
            let (_, published) = write_ready_segment(&run_directory, &mapping, 0);
            journal
                .mark_segment_ready("session-1", "r0000", &published, &run_directory)
                .unwrap();
            let mut outcome = zero_outcome(dir.path(), &mapping, RecordingStopReason::Manual);
            outcome.anchor_qpc = Some(100);
            outcome.qpc_start = Some(100);
            outcome.qpc_end = Some(200);
            outcome.frame_count = 300;
            outcome.session_frame_end = 300;
            outcome.segment_count = 1;
            journal
                .finish_run(&outcome, RunTerminalState::Completed)
                .unwrap();
        }

        let reopened = SessionJournal::open(&path).unwrap();
        let detail = reopened.session_detail("session-1").unwrap();
        assert_eq!(detail.session.marker_count, 1);
        assert_eq!(detail.markers.len(), 1);
        assert_eq!(detail.markers[0].run_name, "r0000");
        assert_eq!(detail.markers[0].segment_seq, Some(0));
        assert_eq!(detail.markers[0].alignment_state, "aligned");
    }

    #[test]
    fn marker_added_after_ready_segment_is_aligned_with_half_open_boundaries() {
        let dir = tempfile::tempdir().unwrap();
        let journal = SessionJournal::open(dir.path().join("journal.sqlite3")).unwrap();
        let profile = profile();
        let mapping = MappingSnapshot::from_slice(MAPPING).unwrap();
        begin(&journal, &profile, &mapping);
        journal
            .mark_run_started("session-1", "r0000", 100, 10_000_000)
            .unwrap();
        journal
            .mark_segment_started("session-1", "r0000", 0, 0)
            .unwrap();
        journal
            .mark_segment_finalizing("session-1", "r0000", 0, 0, 300)
            .unwrap();
        let run_directory = dir.path().join("session-1/runs/r0000");
        let (_, published) = write_ready_segment(&run_directory, &mapping, 0);
        journal
            .mark_segment_ready("session-1", "r0000", &published, &run_directory)
            .unwrap();

        journal
            .record_marker(&MarkerWrite {
                request_id: "marker-inside-ready",
                session_id: "session-1",
                run_id: "r0000",
                kind: MarkerKind::Manual,
                qpc: 199,
            })
            .unwrap();
        journal
            .record_marker(&MarkerWrite {
                request_id: "marker-at-ready-end",
                session_id: "session-1",
                run_id: "r0000",
                kind: MarkerKind::Manual,
                qpc: 200,
            })
            .unwrap();

        let connection = journal.connection.lock();
        let mut statement = connection
            .prepare(
                "SELECT request_id, alignment_state, segment_seq
                 FROM markers ORDER BY seq",
            )
            .unwrap();
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<u32>>(2)?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            rows,
            vec![
                ("marker-inside-ready".into(), "aligned".into(), Some(0)),
                ("marker-at-ready-end".into(), "pending".into(), None),
            ]
        );
    }

    #[test]
    fn cp2077_main_fact_is_durable_without_upload_and_is_deleted_with_session() {
        let dir = tempfile::tempdir().unwrap();
        let journal = SessionJournal::open(dir.path().join("journal.sqlite3")).unwrap();
        let profile = profile();
        let mapping = MappingSnapshot::from_slice(MAPPING).unwrap();
        begin(&journal, &profile, &mapping);
        journal
            .mark_run_started("session-1", "r0000", 100, 10_000_000)
            .unwrap();
        journal
            .connection
            .lock()
            .execute(
                "UPDATE capture_runs
                    SET depth_expected = 1, performance_validation_only = 1
                  WHERE session_id = 'session-1' AND run_name = 'r0000'",
                [],
            )
            .unwrap();
        journal
            .mark_segment_started("session-1", "r0000", 0, 0)
            .unwrap();
        journal
            .mark_segment_finalizing("session-1", "r0000", 0, 0, 300)
            .unwrap();
        let run_directory = dir.path().join("session-1/runs/r0000");
        let (_, published) = write_ready_segment(&run_directory, &mapping, 0);
        journal
            .mark_segment_ready("session-1", "r0000", &published, &run_directory)
            .unwrap();
        let before: (u32, u32, u32) = journal
            .connection
            .lock()
            .query_row(
                "SELECT COUNT(*),
                        COALESCE(SUM(CASE WHEN state = 'pending' THEN 1 ELSE 0 END), 0),
                        COALESCE(SUM(CASE WHEN state = 'rejected' THEN 1 ELSE 0 END), 0)
                   FROM upload_jobs",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        journal
            .commit_cp2077_main_run(&Cp2077MainRunCommit {
                session_id: "session-1",
                run_id: "r0000",
                generation: 7,
                process_id: 42,
                process_launch_id: "0123456789abcdef0123456789abcdef",
                result_relative_path: "cp2077-main-result.json",
                result_sha256: &format!("sha256:{}", "a".repeat(64)),
                sidecar_locator: "DataMiner/depth-raw-demo/cp2077/test",
                delivery_locator: "DataMiner/depth-delivery/cp2077/test",
                rgb_frames: 1_900,
                source_depth_frames: 1_000,
                paired_depth_frames: 990,
                duration_ms: 64_000,
                capture_queue_drops: 0,
                input_queue_overflows: 0,
                exact_present_matches: 990,
                exact_present_pairing_rate: 0.99,
                raw_input_events: 100,
                keyboard_input_events: 12,
                mouse_input_events: 88,
                player_pose_samples: 500,
                render_camera_samples: 500,
            })
            .unwrap();
        let connection = journal.connection.lock();
        let after: (u32, u32, u32) = connection
            .query_row(
                "SELECT COUNT(*),
                        COALESCE(SUM(CASE WHEN state = 'pending' THEN 1 ELSE 0 END), 0),
                        COALESCE(SUM(CASE WHEN state = 'rejected' THEN 1 ELSE 0 END), 0)
                   FROM upload_jobs",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(after, before, "CP2077 local sealing changed the R2 queue");
        let persisted: (String, String, u64, u64, u64, bool, bool) = connection
            .query_row(
                "SELECT session_id, process_launch_id, paired_depth_frames,
                        keyboard_input_events, mouse_input_events,
                        confirmed_flush, upload_allowed
                   FROM cp2077_main_run_facts",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(persisted.0, "session-1");
        assert_eq!(persisted.1, "0123456789abcdef0123456789abcdef");
        assert_eq!(persisted.2, 990);
        assert_eq!(persisted.3, 12);
        assert_eq!(persisted.4, 88);
        assert!(persisted.5);
        assert!(!persisted.6);
        drop(connection);
        journal
            .connection
            .lock()
            .execute(
                "UPDATE sessions SET state = 'completed' WHERE id = 'session-1'",
                [],
            )
            .unwrap();

        journal
            .prepare_session_deletion("delete-cp2077", "session-1", "delete/session-1")
            .unwrap();
        journal
            .commit_session_deletion("delete-cp2077", "session-1")
            .unwrap();

        let connection = journal.connection.lock();
        let remaining: (u32, u32) = connection
            .query_row(
                "SELECT
                    (SELECT COUNT(*) FROM sessions WHERE id = 'session-1'),
                    (SELECT COUNT(*) FROM cp2077_main_run_facts
                      WHERE session_id = 'session-1')",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(remaining, (0, 0));
    }

    #[test]
    fn marker_qpc_boundary_maps_to_the_next_cfr_pts() {
        let dir = tempfile::tempdir().unwrap();
        let journal = SessionJournal::open(dir.path().join("journal.sqlite3")).unwrap();
        let profile = profile();
        let mapping = MappingSnapshot::from_slice(MAPPING).unwrap();
        begin(&journal, &profile, &mapping);
        let anchor = 1_000;
        let frequency = 10_000_000;
        journal
            .mark_run_started("session-1", "r0000", anchor, frequency)
            .unwrap();
        let boundary = capture_core::qpc::frame_qpc(anchor, 1, frequency, 30, 1);

        let before = journal
            .record_marker(&MarkerWrite {
                request_id: "marker-before-boundary",
                session_id: "session-1",
                run_id: "r0000",
                kind: MarkerKind::Manual,
                qpc: boundary - 1,
            })
            .unwrap();
        let at = journal
            .record_marker(&MarkerWrite {
                request_id: "marker-at-boundary",
                session_id: "session-1",
                run_id: "r0000",
                kind: MarkerKind::Manual,
                qpc: boundary,
            })
            .unwrap();

        assert_eq!((before.run_frame, before.video_time_us), (0, 0));
        assert_eq!((at.run_frame, at.video_time_us), (1, 33_333));
        assert_eq!((before.seq, at.seq), (0, 1));
    }

    #[test]
    fn ready_artifact_and_journal_commit_together_idempotently() {
        let dir = tempfile::tempdir().unwrap();
        let journal = SessionJournal::open(dir.path().join("journal.sqlite3")).unwrap();
        let profile = profile();
        let mapping = MappingSnapshot::from_slice(MAPPING).unwrap();
        begin(&journal, &profile, &mapping);
        journal
            .mark_run_started("session-1", "r0000", 100, 10_000_000)
            .unwrap();
        journal
            .mark_segment_started("session-1", "r0000", 0, 0)
            .unwrap();
        journal
            .mark_segment_finalizing("session-1", "r0000", 0, 0, 300)
            .unwrap();

        let run_directory = dir.path().join("session-1/runs/r0000");
        let (_, published) = write_ready_segment(&run_directory, &mapping, 0);
        journal
            .mark_segment_ready("session-1", "r0000", &published, &run_directory)
            .unwrap();
        journal
            .mark_segment_ready("session-1", "r0000", &published, &run_directory)
            .unwrap();

        let queued = journal.data_model_job("session-1").unwrap().unwrap();
        assert_eq!(queued.state, DataModelJobState::Pending);
        assert_eq!((queued.ready_segments, queued.processed_segments), (1, 0));
        assert_eq!(
            journal.list_processable_data_model_jobs(10).unwrap(),
            vec![queued.clone()]
        );
        assert_eq!(
            journal.ready_segments_for_data_model("session-1").unwrap(),
            vec![DataModelReadySegmentRef {
                session_id: "session-1".into(),
                run_name: "r0000".into(),
                run_seq: 0,
                segment_index: 0,
                session_frame_start: 0,
                frame_count: 300,
            }]
        );

        let connection = journal.connection.lock();
        let persisted = connection
            .query_row(
                "SELECT state, n_frames, video_sha256, source_drops FROM segments",
                [],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(persisted.0, "ready");
        assert_eq!(persisted.1, 300);
        assert!(persisted.2.starts_with("sha256:"));
        assert_eq!(persisted.3, 0);
        drop(connection);

        let detail = journal.session_detail("session-1").unwrap();
        assert_eq!(detail.session.ready_segments, 1);
        assert_eq!(detail.segments.len(), 1);
        assert_eq!(detail.segments[0].state, "ready");
        assert!(detail.segments[0].hashes_complete);
        let upload_jobs_before_request: i64 = journal
            .connection
            .lock()
            .query_row("SELECT COUNT(*) FROM upload_jobs", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            upload_jobs_before_request, 0,
            "journal-ready RAW must wait for an explicit upload request"
        );

        let claimed = journal.claim_next_data_model_job().unwrap().unwrap();
        assert_eq!(claimed.state, DataModelJobState::Running);
        let saved = journal
            .save_data_model_progress(
                "session-1",
                &DataModelProgress {
                    processed_segments: 1,
                    published_chunks: 2,
                    published_frames: 240,
                    pending_frames: 60,
                },
            )
            .unwrap();
        assert_eq!(saved.state, DataModelJobState::Waiting);
        assert!(journal.claim_next_data_model_job().unwrap().is_none());
        let upload_jobs_after_conversion: i64 = journal
            .connection
            .lock()
            .query_row("SELECT COUNT(*) FROM upload_jobs", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            upload_jobs_after_conversion, 0,
            "DataModel progress must not create RAW upload work"
        );
        assert!(
            journal.claim_next_upload_job().unwrap().is_none(),
            "DataModel progress must not open the manual upload gate"
        );
    }

    #[test]
    fn qualified_depth_zip_is_durable_and_enters_the_same_production_candidate() {
        let dir = tempfile::tempdir().unwrap();
        let journal = SessionJournal::open(dir.path().join("journal.sqlite3")).unwrap();
        let mut profile = profile();
        profile.audio = Some(capture_core::profile::CaptureAudioProfile::process_loopback());
        let mapping = MappingSnapshot::from_slice(MAPPING).unwrap();
        begin(&journal, &profile, &mapping);
        journal
            .connection
            .lock()
            .execute(
                "UPDATE capture_runs SET depth_expected = 1
                  WHERE session_id = 'session-1' AND run_name = 'r0000'",
                [],
            )
            .unwrap();
        journal
            .mark_run_started("session-1", "r0000", 100, 10_000_000)
            .unwrap();
        journal
            .mark_segment_started("session-1", "r0000", 0, 0)
            .unwrap();
        journal
            .mark_segment_finalizing("session-1", "r0000", 0, 0, 300)
            .unwrap();

        let run_directory = dir.path().join("session-1/runs/r0000");
        let (_, published) = write_audio_ready_segment(&run_directory, &profile, &mapping, 0);
        journal
            .mark_segment_ready("session-1", "r0000", &published, &run_directory)
            .unwrap();
        assert_eq!(
            journal
                .depth_segment_boundaries("session-1", "r0000")
                .unwrap(),
            vec![DepthSegmentBoundary {
                segment_index: 0,
                session_frame_start: 0,
                frame_count: 300,
            }]
        );
        assert_eq!(
            journal.depth_run_fps("session-1", "r0000").unwrap(),
            (profile.fps_num, profile.fps_den)
        );

        let inventory = journal.recovery_inventory().unwrap();
        assert_eq!(
            inventory.sessions[0].schema_version,
            "dm-local-dev-session-3"
        );
        assert_eq!(
            inventory.segments[0]
                .audio
                .as_ref()
                .and_then(|audio| audio.bytes),
            Some(19)
        );

        let mut outcome = zero_outcome(dir.path(), &mapping, RecordingStopReason::Manual);
        outcome.schema_version = RECORDING_SCHEMA_V2;
        outcome.anchor_qpc = Some(100);
        outcome.qpc_start = Some(100);
        outcome.qpc_end = Some(200);
        outcome.frame_count = 300;
        outcome.session_frame_end = 300;
        outcome.segment_count = 1;
        journal
            .finish_run(&outcome, RunTerminalState::Completed)
            .unwrap();
        journal.claim_next_data_model_job().unwrap().unwrap();
        journal
            .mark_data_model_completed(
                "session-1",
                &DataModelProgress {
                    processed_segments: 1,
                    published_chunks: 3,
                    published_frames: 300,
                    pending_frames: 0,
                },
                "ready/session-1",
            )
            .unwrap();
        journal.request_session_upload("session-1").unwrap();

        let segment_id = segment_journal_id("session-1", "r0000", 0);
        let pending_depth = journal.upload_candidate(&segment_id).unwrap_err();
        assert!(
            format!("{pending_depth:#}").contains("declared at run start"),
            "depth-enabled run must not upload video before finalization: {pending_depth:#}"
        );
        assert!(
            journal.claim_next_upload_job().unwrap().is_none(),
            "depth-enabled upload job must stay pending until manifest and ZIP are durable"
        );
        journal
            .reject_depth_finalization("session-1", "r0000")
            .unwrap();
        let rejected: (String, Option<String>) = journal
            .connection
            .lock()
            .query_row(
                "SELECT state, last_error_code FROM upload_jobs WHERE segment_id = ?1",
                params![segment_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            rejected,
            ("rejected".into(), Some("DEPTH_FINALIZE_FAILED".into()))
        );
        assert!(journal.depth_lab_candidate(&segment_id).unwrap().is_none());
        journal
            .commit_depth_segments(
                "session-1",
                "r0000",
                &[DepthSegmentCommit {
                    segment_index: 0,
                    generation: 7,
                    manifest_relative_path: "depth-segments/00000/depth-segment.json".into(),
                    manifest_sha256:
                        "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                            .into(),
                    upload_relative_path: Some("runs/r0000/depth-uploads/depth_00000.zip".into()),
                    upload_sha256: Some(
                        "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
                            .into(),
                    ),
                    candidate_eligible: true,
                    invalid_frame_count: 0,
                }],
            )
            .unwrap();
        let claimed_upload = journal
            .claim_next_upload_job()
            .unwrap()
            .expect("durable depth ZIP must reopen and release the existing clip upload job");
        assert_eq!(claimed_upload.segment_id, segment_id);
        let production_after_depth_completion = journal.upload_candidate(&segment_id).unwrap();
        assert_eq!(
            production_after_depth_completion.session_schema_version,
            "dm-local-dev-session-3"
        );
        assert!(matches!(
            production_after_depth_completion
                .audio
                .as_ref()
                .map(|audio| audio.state),
            Some(capture_core::audio_contract::SegmentAudioState::Ready)
        ));
        assert_eq!(
            production_after_depth_completion
                .artifacts
                .iter()
                .map(|artifact| artifact.kind)
                .collect::<Vec<_>>(),
            vec![
                "video", "frames", "events", "meta", "manifest", "audio", "depth"
            ]
        );
        let production_depth = production_after_depth_completion
            .artifacts
            .iter()
            .find(|artifact| artifact.kind == "depth")
            .expect("qualified depth must share the clip upload candidate");
        assert_eq!(
            production_depth.relative_path,
            "runs/r0000/depth-uploads/depth_00000.zip"
        );
        assert_eq!(
            production_depth.expected_sha256.as_deref(),
            Some("sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc")
        );
        journal
            .save_uploaded_part(
                &segment_id,
                &UploadedPart {
                    artifact_kind: "depth".into(),
                    provider_attempt_no: 1,
                    part_number: 1,
                    byte_offset: 0,
                    byte_size: 4_096,
                    content_md5: "depth-part-md5".into(),
                    etag: "depth-part-etag".into(),
                },
            )
            .unwrap();
        assert_eq!(
            journal
                .uploaded_parts(&segment_id)
                .unwrap()
                .into_iter()
                .find(|part| part.artifact_kind == "depth")
                .expect("depth multipart progress must survive in the shared journal")
                .etag,
            "depth-part-etag"
        );
        journal
            .connection
            .lock()
            .execute(
                "UPDATE segments
                    SET depth_upload_path = NULL, depth_upload_sha256 = NULL
                  WHERE id = ?1",
                params![segment_id],
            )
            .unwrap();
        let ineligible = journal.upload_candidate(&segment_id).unwrap_err();
        assert!(
            format!("{ineligible:#}").contains("refusing video-only downgrade"),
            "manifest-only depth must block the whole clip upload: {ineligible:#}"
        );
        journal
            .connection
            .lock()
            .execute(
                "UPDATE segments
                    SET depth_upload_path = ?2, depth_upload_sha256 = ?3
                  WHERE id = ?1",
                params![
                    segment_id,
                    "runs/r0000/depth-uploads/depth_00000.zip",
                    "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
                ],
            )
            .unwrap();
        let depth = journal.depth_lab_candidate(&segment_id).unwrap().unwrap();
        assert_eq!(depth.state, DepthLabJobState::Pending);
        assert_eq!(depth.generation, 7);
        assert_eq!(
            depth.manifest_relative_path,
            "depth-segments/00000/depth-segment.json"
        );
        let claimed = journal.claim_next_depth_lab_candidate().unwrap().unwrap();
        assert_eq!(claimed.state, DepthLabJobState::Running);
        assert_eq!(claimed.attempt_count, 1);
        journal.requeue_interrupted_depth_lab_jobs().unwrap();
        assert_eq!(
            journal
                .depth_lab_candidate(&segment_id)
                .unwrap()
                .unwrap()
                .state,
            DepthLabJobState::Pending
        );

        let lease = journal
            .claim_next_depth_archive_job(1_000, "worker-a", 100)
            .unwrap()
            .unwrap();
        assert_eq!(lease.job.run_id, "r0000");
        assert_eq!(lease.job.generation, 7);
        assert_eq!(lease.job.attempt_count, 1);
        assert_eq!(lease.candidates.len(), 1);
        journal
            .retry_depth_archive_job(
                &lease.job.job_id,
                "worker-a",
                1_000,
                50,
                "REMOTE_FAILED",
                "temporary depth-ingest failure",
            )
            .unwrap();
        assert!(
            journal
                .claim_next_depth_archive_job(1_049, "worker-b", 100)
                .unwrap()
                .is_none(),
            "retry backoff must survive the worker boundary"
        );

        let journal_path = dir.path().join("journal.sqlite3");
        drop(journal);
        let journal = SessionJournal::open(&journal_path).unwrap();
        let restarted_upload = journal.upload_candidate(&segment_id).unwrap();
        assert_eq!(
            restarted_upload
                .artifacts
                .iter()
                .map(|artifact| artifact.kind)
                .collect::<Vec<_>>(),
            vec![
                "video", "frames", "events", "meta", "manifest", "audio", "depth"
            ],
            "restart must recover the same video+depth clip source set"
        );
        assert!(
            journal
                .uploaded_parts(&segment_id)
                .unwrap()
                .iter()
                .any(|part| part.artifact_kind == "depth" && part.etag == "depth-part-etag"),
            "restart must recover depth multipart progress for idempotent retry"
        );
        let recovered = journal
            .claim_next_depth_archive_job(1_050, "worker-b", 100)
            .unwrap()
            .unwrap();
        assert_eq!(recovered.job.job_id, lease.job.job_id);
        assert_eq!(recovered.job.attempt_count, 2);
        assert_eq!(recovered.job.last_error_code, None);
        journal
            .save_depth_archive_materialization(
                &recovered.job.job_id,
                "worker-b",
                1_050,
                r#"{"archive":"local"}"#,
                r#"{"parts":[]}"#,
            )
            .unwrap();
        journal
            .save_depth_archive_remote_scope(
                &recovered.job.job_id,
                "worker-b",
                1_050,
                r#"{"scope":"depth-only"}"#,
            )
            .unwrap();
        journal
            .save_depth_archive_progress(
                &recovered.job.job_id,
                "worker-b",
                1_050,
                DepthArchiveJobState::Uploading,
                r#"{"parts":[1]}"#,
            )
            .unwrap();
        let not_final = journal
            .complete_depth_archive_job(
                &recovered.job.job_id,
                "worker-b",
                1_050,
                "payload_verified",
            )
            .unwrap_err();
        assert!(format!("{not_final:#}").contains("only after remote Finalized"));
        assert!(journal.depth_lab_candidate(&segment_id).unwrap().is_some());
        journal
            .complete_depth_archive_job(&recovered.job.job_id, "worker-b", 1_050, "finalized")
            .unwrap();
        assert!(journal.depth_lab_candidate(&segment_id).unwrap().is_none());
        assert_eq!(
            journal
                .depth_archive_job(&recovered.job.job_id)
                .unwrap()
                .unwrap()
                .state,
            DepthArchiveJobState::Finalized
        );
        assert!(
            journal
                .claim_next_depth_archive_job(2_000, "worker-c", 100)
                .unwrap()
                .is_none(),
            "finalized archive job must be idempotently invisible"
        );
        let retained_facts: i64 = journal
            .connection
            .lock()
            .query_row("SELECT COUNT(*) FROM depth_lab_segment_facts", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(
            retained_facts, 1,
            "remote finalization must not delete local facts"
        );

        for (path, sha256, expected_error) in [
            (
                Some("depth-segments/00000/depth-segment.json"),
                None,
                "depth artifact path is present but sha256 is missing",
            ),
            (
                None,
                Some("sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
                "depth artifact sha256 is present but path is missing",
            ),
        ] {
            journal
                .connection
                .lock()
                .execute(
                    "UPDATE segments SET depth_path = ?1, depth_sha256 = ?2 WHERE id = ?3",
                    params![path, sha256, segment_id],
                )
                .unwrap();
            let error = journal.upload_candidate(&segment_id).unwrap_err();
            assert!(
                format!("{error:#}").contains(expected_error),
                "unexpected partial-depth error: {error:#}"
            );
        }
    }

    #[test]
    fn invalid_depth_segment_keeps_local_fact_but_never_becomes_a_candidate() {
        let dir = tempfile::tempdir().unwrap();
        let journal = SessionJournal::open(dir.path().join("journal.sqlite3")).unwrap();
        let profile = profile();
        let mapping = MappingSnapshot::from_slice(MAPPING).unwrap();
        begin(&journal, &profile, &mapping);
        journal
            .connection
            .lock()
            .execute(
                "UPDATE capture_runs SET depth_expected = 1
                  WHERE session_id = 'session-1' AND run_name = 'r0000'",
                [],
            )
            .unwrap();
        let active_recovery = journal
            .ensure_completed_depth_recovery_source("session-1", "r0000")
            .unwrap_err();
        assert!(format!("{active_recovery:#}").contains("only completed historical runs"));
        journal
            .mark_run_started("session-1", "r0000", 100, 10_000_000)
            .unwrap();
        journal
            .mark_segment_started("session-1", "r0000", 0, 0)
            .unwrap();
        journal
            .mark_segment_finalizing("session-1", "r0000", 0, 0, 300)
            .unwrap();
        let run_directory = dir.path().join("session-1/runs/r0000");
        let (_, published) = write_ready_segment(&run_directory, &mapping, 0);
        journal
            .mark_segment_ready("session-1", "r0000", &published, &run_directory)
            .unwrap();

        let segment_id = segment_journal_id("session-1", "r0000", 0);
        journal
            .commit_depth_segments(
                "session-1",
                "r0000",
                &[DepthSegmentCommit {
                    segment_index: 0,
                    generation: 3,
                    manifest_relative_path: "depth-segments/00000/depth-segment.json".into(),
                    manifest_sha256:
                        "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                            .into(),
                    upload_relative_path: None,
                    upload_sha256: None,
                    candidate_eligible: false,
                    invalid_frame_count: 1,
                }],
            )
            .unwrap();

        assert!(journal.depth_lab_candidate(&segment_id).unwrap().is_none());
        assert!(journal.claim_next_depth_lab_candidate().unwrap().is_none());
        {
            let connection = journal.connection.lock();
            let (facts, jobs): (i64, i64) = connection
                .query_row(
                    "SELECT
                        (SELECT COUNT(*) FROM depth_lab_segment_facts),
                        (SELECT COUNT(*) FROM depth_lab_jobs)",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            assert_eq!((facts, jobs), (1, 0));
        }

        let mut outcome = zero_outcome(dir.path(), &mapping, RecordingStopReason::Manual);
        outcome.anchor_qpc = Some(100);
        outcome.qpc_start = Some(100);
        outcome.qpc_end = Some(200);
        outcome.frame_count = 300;
        outcome.session_frame_end = 300;
        outcome.segment_count = 1;
        journal
            .finish_run(&outcome, RunTerminalState::Completed)
            .unwrap();
        journal
            .ensure_completed_depth_recovery_source("session-1", "r0000")
            .unwrap();
        journal.request_session_upload("session-1").unwrap();

        let queued: (String, Option<String>, u32) = journal
            .connection
            .lock()
            .query_row(
                "SELECT state, last_error_code, attempt_count
                   FROM upload_jobs WHERE segment_id = ?1",
                params![segment_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            queued,
            (
                "rejected".into(),
                Some("DEPTH_SEGMENT_INELIGIBLE".into()),
                0
            )
        );
        assert!(journal.claim_next_upload_job().unwrap().is_none());
    }

    #[test]
    fn depth_batch_commit_rolls_back_every_segment_if_one_identity_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let journal = SessionJournal::open(dir.path().join("journal.sqlite3")).unwrap();
        let profile = profile();
        let mapping = MappingSnapshot::from_slice(MAPPING).unwrap();
        begin(&journal, &profile, &mapping);
        journal
            .mark_run_started("session-1", "r0000", 100, 10_000_000)
            .unwrap();
        journal
            .mark_segment_started("session-1", "r0000", 0, 0)
            .unwrap();
        journal
            .mark_segment_finalizing("session-1", "r0000", 0, 0, 300)
            .unwrap();
        let run_directory = dir.path().join("session-1/runs/r0000");
        let (_, published) = write_ready_segment(&run_directory, &mapping, 0);
        journal
            .mark_segment_ready("session-1", "r0000", &published, &run_directory)
            .unwrap();

        let hash = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let error = journal
            .commit_depth_segments(
                "session-1",
                "r0000",
                &[
                    DepthSegmentCommit {
                        segment_index: 0,
                        generation: 1,
                        manifest_relative_path: "depth-segments/00000/depth-segment.json".into(),
                        manifest_sha256: hash.into(),
                        upload_relative_path: Some(
                            "runs/r0000/depth-uploads/depth_00000.zip".into(),
                        ),
                        upload_sha256: Some(hash.into()),
                        candidate_eligible: true,
                        invalid_frame_count: 0,
                    },
                    // 目录名/哈希形状合法，但 journal 没有这个 segment；失败发生在
                    // 第一段 UPDATE/INSERT 之后，必须由事务把第一段也回滚。
                    DepthSegmentCommit {
                        segment_index: 1,
                        generation: 1,
                        manifest_relative_path: "depth-segments/00001/depth-segment.json".into(),
                        manifest_sha256: hash.into(),
                        upload_relative_path: Some(
                            "runs/r0000/depth-uploads/depth_00001.zip".into(),
                        ),
                        upload_sha256: Some(hash.into()),
                        candidate_eligible: true,
                        invalid_frame_count: 0,
                    },
                ],
            )
            .unwrap_err();
        assert!(format!("{error:#}").contains("depth segment 1 is missing"));
        let connection = journal.connection.lock();
        let local_fact: (Option<String>, Option<String>) = connection
            .query_row(
                "SELECT depth_path, depth_sha256 FROM segments WHERE id = ?1",
                params![segment_journal_id("session-1", "r0000", 0)],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let queued: i64 = connection
            .query_row("SELECT COUNT(*) FROM depth_lab_jobs", [], |row| row.get(0))
            .unwrap();
        let facts: i64 = connection
            .query_row("SELECT COUNT(*) FROM depth_lab_segment_facts", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(local_fact, (None, None));
        assert_eq!(queued, 0);
        assert_eq!(facts, 0);
    }

    #[test]
    fn ready_enqueues_one_redacted_mirror_event_and_the_replay_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let journal = SessionJournal::open(dir.path().join("journal.sqlite3")).unwrap();
        let profile = profile();
        let mapping = MappingSnapshot::from_slice(MAPPING).unwrap();
        begin(&journal, &profile, &mapping);
        journal
            .mark_run_started("session-1", "r0000", 100, 10_000_000)
            .unwrap();
        journal
            .mark_segment_started("session-1", "r0000", 0, 0)
            .unwrap();
        journal
            .mark_segment_finalizing("session-1", "r0000", 0, 0, 300)
            .unwrap();

        let run_directory = dir.path().join("session-1/runs/r0000");
        let (_, published) = write_ready_segment(&run_directory, &mapping, 0);
        journal
            .mark_segment_ready("session-1", "r0000", &published, &run_directory)
            .unwrap();
        // ready 是幂等重放的：第二次不能再投一条事件，否则云端会用两个不同 seq
        // 各存一份，而不是把它当同一条去重。
        journal
            .mark_segment_ready("session-1", "r0000", &published, &run_directory)
            .unwrap();

        let connection = journal.connection.lock();
        let (count, entity, entity_id, payload): (i64, String, String, String) = connection
            .query_row(
                "SELECT COUNT(*), MIN(entity), MIN(entity_id), MIN(payload_json) FROM mirror_outbox",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(count, 1, "replay must not enqueue a second mirror event");
        assert_eq!(entity, "segment");
        assert_eq!(entity_id, "session-1:r0000:0");

        // payload 只应包含脱敏白名单字段。这些是采集事实里最不该外流的东西——
        // 本地路径、原始 PID、窗口标题——SELECT 的列清单本身就把它们挡在外面。
        for forbidden in [
            "video_path",
            "\\\\",
            "sha256",
            "target_pid",
            "window",
            run_directory.to_string_lossy().as_ref(),
        ] {
            assert!(
                !payload.contains(forbidden),
                "mirror payload leaked {forbidden:?}: {payload}"
            );
        }
        assert!(payload.contains("\"clientUploadState\":\"unknown\""));
        assert!(payload.contains("\"localState\":\"ready\""));
        assert!(payload.contains("\"nFrames\":300"));

        // 采集事实的权威表不受影响：镜像是旁路投影，不是它们的镜子。
        let segments: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM segments WHERE state = 'ready'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(segments, 1);
    }

    #[test]
    fn recovery_corruption_invalidates_completed_data_model_output() {
        let dir = tempfile::tempdir().unwrap();
        let journal = SessionJournal::open(dir.path().join("journal.sqlite3")).unwrap();
        let profile = profile();
        let mapping = MappingSnapshot::from_slice(MAPPING).unwrap();
        begin(&journal, &profile, &mapping);
        journal
            .mark_run_started("session-1", "r0000", 100, 10_000_000)
            .unwrap();
        journal
            .mark_segment_started("session-1", "r0000", 0, 0)
            .unwrap();
        journal
            .mark_segment_finalizing("session-1", "r0000", 0, 0, 300)
            .unwrap();
        let run_directory = dir.path().join("session-1/runs/r0000");
        let (_, published) = write_ready_segment(&run_directory, &mapping, 0);
        journal
            .mark_segment_ready("session-1", "r0000", &published, &run_directory)
            .unwrap();
        let mut outcome = zero_outcome(dir.path(), &mapping, RecordingStopReason::Manual);
        outcome.anchor_qpc = Some(100);
        outcome.qpc_start = Some(100);
        outcome.qpc_end = Some(200);
        outcome.frame_count = 300;
        outcome.session_frame_end = 300;
        outcome.segment_count = 1;
        journal
            .finish_run(&outcome, RunTerminalState::Completed)
            .unwrap();
        assert_eq!(
            journal.claim_next_data_model_job().unwrap().unwrap().state,
            DataModelJobState::Finalizing
        );
        journal
            .mark_data_model_completed(
                "session-1",
                &DataModelProgress {
                    processed_segments: 1,
                    published_chunks: 3,
                    published_frames: 300,
                    pending_frames: 0,
                },
                "ready/session-1",
            )
            .unwrap();

        let corrupt_segment_ids = vec![segment_journal_id("session-1", "r0000", 0)];
        journal
            .commit_recovery(&RecoveryCommit {
                recovery_id: "recovery-data-model-corrupt",
                report_path: "recovery/data-model-corrupt.json",
                scanned_sessions: 1,
                verified_ready_segments: 0,
                quarantined_items: 0,
                corrupt_segments: 1,
                orphan_items: 0,
                unfinished_run_ids: &[],
                quarantined_segment_ids: &[],
                corrupt_segment_ids: &corrupt_segment_ids,
                corrupt_session_ids: &[],
                findings: &[],
            })
            .unwrap();

        let invalidated = journal.data_model_job("session-1").unwrap().unwrap();
        assert_eq!(invalidated.state, DataModelJobState::Failed);
        assert_eq!(invalidated.ready_segments, 0);
        assert_eq!(invalidated.processed_segments, 0);
        assert_eq!(invalidated.published_chunks, 0);
        assert_eq!(invalidated.published_frames, 0);
        assert_eq!(invalidated.output_relative_path, None);
        assert_eq!(
            invalidated.error_code.as_deref(),
            Some(DATA_MODEL_SOURCE_CORRUPT_CODE)
        );
        assert_eq!(
            journal.invalidated_data_model_sessions().unwrap(),
            vec!["session-1".to_owned()]
        );
        assert!(
            journal
                .ready_segments_for_data_model("session-1")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn inconsistent_manifest_is_not_committed_ready() {
        let dir = tempfile::tempdir().unwrap();
        let journal = SessionJournal::open(dir.path().join("journal.sqlite3")).unwrap();
        let profile = profile();
        let mapping = MappingSnapshot::from_slice(MAPPING).unwrap();
        begin(&journal, &profile, &mapping);
        journal
            .mark_run_started("session-1", "r0000", 100, 10_000_000)
            .unwrap();
        journal
            .mark_segment_started("session-1", "r0000", 0, 0)
            .unwrap();
        journal
            .mark_segment_finalizing("session-1", "r0000", 0, 0, 300)
            .unwrap();

        let run_directory = dir.path().join("session-1/runs/r0000");
        let (segment_dir, published) = write_ready_segment(&run_directory, &mapping, 0);
        let manifest_path = segment_dir.join("sha256.json");
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        manifest["video.mp4"] = serde_json::Value::String("sha256:tampered".into());
        fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();
        let error = journal
            .mark_segment_ready("session-1", "r0000", &published, &run_directory)
            .unwrap_err();
        assert!(error.to_string().contains("hashes differ"));

        let connection = journal.connection.lock();
        let state: String = connection
            .query_row("SELECT state FROM segments", [], |row| row.get(0))
            .unwrap();
        assert_eq!(state, "finalizing");
        drop(connection);
        let job = journal.data_model_job("session-1").unwrap().unwrap();
        assert_eq!(job.state, DataModelJobState::Waiting);
        assert_eq!(job.ready_segments, 0);
    }

    #[test]
    fn data_model_wall_clock_context_round_trips_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let database = dir.path().join("journal.sqlite3");
        let journal = SessionJournal::open(&database).unwrap();
        let profile = profile();
        let mapping = MappingSnapshot::from_slice(MAPPING).unwrap();
        begin(&journal, &profile, &mapping);
        let expected = DataModelWallClockContext {
            anchor_at_utc: Some("2026-08-12T13:14:15.678Z".into()),
            anchor_qpc: 12_345,
            qpc_frequency: 10_000_000,
        };
        journal
            .mark_run_started_with_wall_clock(
                "session-1",
                "r0000",
                expected.anchor_qpc,
                expected.anchor_at_utc.as_deref(),
                expected.qpc_frequency,
            )
            .unwrap();
        assert_eq!(
            journal.data_model_wall_clock_context("session-1").unwrap(),
            Some(expected.clone())
        );
        drop(journal);

        let reopened = SessionJournal::open(database).unwrap();
        assert_eq!(
            reopened.data_model_wall_clock_context("session-1").unwrap(),
            Some(expected)
        );
    }

    #[test]
    fn data_model_wall_clock_context_skips_a_prior_zero_frame_run() {
        let dir = tempfile::tempdir().unwrap();
        let journal = SessionJournal::open(dir.path().join("journal.sqlite3")).unwrap();
        let profile = profile();
        let mapping = MappingSnapshot::from_slice(MAPPING).unwrap();
        begin(&journal, &profile, &mapping);
        let first = zero_outcome(dir.path(), &mapping, RecordingStopReason::Pause);
        journal
            .finish_run(&first, RunTerminalState::Paused)
            .unwrap();
        journal
            .begin_test_run(&TestRunStart {
                request_id: "request-2",
                session_id: "session-1",
                run_id: "r0001",
                run_seq: 1,
                session_frame_start: 0,
                process_id: 84,
                target_window_id: "replacement-window-digest",
                executable_sha256: Some("sha256:exe"),
                executable_sha256_missing_reason: None,
                profile: &profile,
                mapping: &mapping,
                view_calibration: None,
            })
            .unwrap();
        journal
            .mark_run_started_with_wall_clock(
                "session-1",
                "r0001",
                12_345,
                Some("2026-08-12T13:14:15.678Z"),
                10_000_000,
            )
            .unwrap();

        assert_eq!(
            journal.data_model_wall_clock_context("session-1").unwrap(),
            Some(DataModelWallClockContext {
                anchor_at_utc: Some("2026-08-12T13:14:15.678Z".into()),
                anchor_qpc: 12_345,
                qpc_frequency: 10_000_000,
            })
        );
    }

    #[test]
    fn run_start_replay_rejects_a_changed_wall_clock_anchor() {
        let dir = tempfile::tempdir().unwrap();
        let journal = SessionJournal::open(dir.path().join("journal.sqlite3")).unwrap();
        let profile = profile();
        let mapping = MappingSnapshot::from_slice(MAPPING).unwrap();
        begin(&journal, &profile, &mapping);
        journal
            .mark_run_started_with_wall_clock(
                "session-1",
                "r0000",
                12_345,
                Some("2026-08-12T13:14:15.678Z"),
                10_000_000,
            )
            .unwrap();
        journal
            .mark_run_started_with_wall_clock(
                "session-1",
                "r0000",
                12_345,
                Some("2026-08-12T13:14:15.678Z"),
                10_000_000,
            )
            .unwrap();

        let error = journal
            .mark_run_started_with_wall_clock(
                "session-1",
                "r0000",
                12_345,
                Some("2026-08-12T13:14:15.679Z"),
                10_000_000,
            )
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("changed its wall-clock/QPC identity")
        );
    }

    #[test]
    fn finish_run_rejects_a_wall_clock_anchor_that_differs_from_started() {
        let dir = tempfile::tempdir().unwrap();
        let journal = SessionJournal::open(dir.path().join("journal.sqlite3")).unwrap();
        let profile = profile();
        let mapping = MappingSnapshot::from_slice(MAPPING).unwrap();
        begin(&journal, &profile, &mapping);
        journal
            .mark_run_started_with_wall_clock(
                "session-1",
                "r0000",
                12_345,
                Some("2026-08-12T13:14:15.678Z"),
                10_000_000,
            )
            .unwrap();
        let mut outcome = zero_outcome(dir.path(), &mapping, RecordingStopReason::Pause);
        outcome.anchor_qpc = Some(12_345);
        outcome.anchor_at_utc = Some("2026-08-12T13:14:15.679Z".into());

        let error = journal
            .finish_run(&outcome, RunTerminalState::Paused)
            .unwrap_err();
        assert!(error.to_string().contains("differs from its Started event"));
    }

    #[test]
    fn finish_run_rejects_a_missing_anchor_after_started() {
        let dir = tempfile::tempdir().unwrap();
        let journal = SessionJournal::open(dir.path().join("journal.sqlite3")).unwrap();
        let profile = profile();
        let mapping = MappingSnapshot::from_slice(MAPPING).unwrap();
        begin(&journal, &profile, &mapping);
        journal
            .mark_run_started_with_wall_clock(
                "session-1",
                "r0000",
                12_345,
                Some("2026-08-12T13:14:15.678Z"),
                10_000_000,
            )
            .unwrap();
        let outcome = zero_outcome(dir.path(), &mapping, RecordingStopReason::Pause);

        let error = journal
            .finish_run(&outcome, RunTerminalState::Paused)
            .unwrap_err();
        assert!(error.to_string().contains("differs from its Started event"));
    }

    #[test]
    fn terminal_states_and_stop_reason_persist_idempotently() {
        let dir = tempfile::tempdir().unwrap();
        let journal = SessionJournal::open(dir.path().join("journal.sqlite3")).unwrap();
        let profile = profile();
        let mapping = MappingSnapshot::from_slice(MAPPING).unwrap();
        begin(&journal, &profile, &mapping);
        journal
            .mark_run_started("session-1", "r0000", 100, 10_000_000)
            .unwrap();
        let mut outcome = zero_outcome(dir.path(), &mapping, RecordingStopReason::Pause);
        outcome.anchor_qpc = Some(100);
        journal
            .finish_run(&outcome, RunTerminalState::Paused)
            .unwrap();
        journal
            .finish_run(&outcome, RunTerminalState::Paused)
            .unwrap();

        let connection = journal.connection.lock();
        let run = connection
            .query_row("SELECT state, stop_reason FROM capture_runs", [], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .unwrap();
        let session: String = connection
            .query_row("SELECT state FROM sessions", [], |row| row.get(0))
            .unwrap();
        assert_eq!(run, ("paused".into(), "pause".into()));
        assert_eq!(session, "paused");
        drop(connection);
        let paused_job = journal.data_model_job("session-1").unwrap().unwrap();
        assert_eq!(paused_job.state, DataModelJobState::Waiting);
        assert!(!paused_job.finalize_requested);
        assert!(journal.claim_next_data_model_job().unwrap().is_none());

        journal.complete_session("session-1").unwrap();
        journal.complete_session("session-1").unwrap();
        let connection = journal.connection.lock();
        let session: String = connection
            .query_row("SELECT state FROM sessions", [], |row| row.get(0))
            .unwrap();
        assert_eq!(session, "completed");
        drop(connection);

        let claimed = journal.claim_next_data_model_job().unwrap().unwrap();
        assert_eq!(claimed.state, DataModelJobState::Finalizing);
        assert!(claimed.finalize_requested);
        let completed = journal
            .mark_data_model_completed(
                "session-1",
                &DataModelProgress::default(),
                "ready/session-1",
            )
            .unwrap();
        assert_eq!(completed.state, DataModelJobState::Completed);
        assert_eq!(
            journal
                .mark_data_model_completed(
                    "session-1",
                    &DataModelProgress::default(),
                    "ready/session-1",
                )
                .unwrap(),
            completed
        );
    }

    #[test]
    fn faulted_run_does_not_request_data_model_finalization() {
        let dir = tempfile::tempdir().unwrap();
        let journal = SessionJournal::open(dir.path().join("journal.sqlite3")).unwrap();
        let profile = profile();
        let mapping = MappingSnapshot::from_slice(MAPPING).unwrap();
        begin(&journal, &profile, &mapping);
        let outcome = zero_outcome(dir.path(), &mapping, RecordingStopReason::InputFault);

        journal
            .finish_run(&outcome, RunTerminalState::Faulted)
            .unwrap();

        let job = journal.data_model_job("session-1").unwrap().unwrap();
        assert_eq!(job.state, DataModelJobState::Waiting);
        assert!(!job.finalize_requested);
        let failed = journal
            .mark_data_model_failed("session-1", "DATAMODEL_SOURCE_FAULTED")
            .unwrap();
        assert_eq!(failed.state, DataModelJobState::Failed);
        assert!(!failed.finalize_requested);
        assert_eq!(
            failed.error_code.as_deref(),
            Some("DATAMODEL_SOURCE_FAULTED")
        );
    }

    #[test]
    fn replacement_window_creates_a_new_run_in_the_same_session() {
        let dir = tempfile::tempdir().unwrap();
        let journal = SessionJournal::open(dir.path().join("journal.sqlite3")).unwrap();
        let profile = profile();
        let mapping = MappingSnapshot::from_slice(MAPPING).unwrap();
        begin(&journal, &profile, &mapping);
        journal
            .mark_run_started("session-1", "r0000", 100, 10_000_000)
            .unwrap();
        let mut outcome = zero_outcome(dir.path(), &mapping, RecordingStopReason::WindowDestroyed);
        outcome.frame_count = 300;
        outcome.session_frame_end = 300;
        outcome.anchor_qpc = Some(100);
        outcome.qpc_start = Some(100);
        outcome.qpc_end = Some(200);
        journal
            .finish_run(&outcome, RunTerminalState::Paused)
            .unwrap();

        journal
            .begin_test_run(&TestRunStart {
                request_id: "request-2",
                session_id: "session-1",
                run_id: "r0001",
                run_seq: 1,
                session_frame_start: 300,
                process_id: 84,
                target_window_id: "replacement-window-digest",
                executable_sha256: Some("sha256:exe"),
                executable_sha256_missing_reason: None,
                profile: &profile,
                mapping: &mapping,
                view_calibration: None,
            })
            .unwrap();

        let connection = journal.connection.lock();
        let session_count: i64 = connection
            .query_row("SELECT count(*) FROM sessions", [], |row| row.get(0))
            .unwrap();
        let runs = connection
            .prepare(
                "SELECT run_name, seq, state, session_frame_start, target_window_id
                 FROM capture_runs ORDER BY seq",
            )
            .unwrap()
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(session_count, 1);
        assert_eq!(runs.len(), 2);
        assert_eq!(
            runs[0],
            (
                "r0000".into(),
                0,
                "paused".into(),
                0,
                "window-digest".into()
            )
        );
        assert_eq!(
            runs[1],
            (
                "r0001".into(),
                1,
                "starting".into(),
                300,
                "replacement-window-digest".into()
            )
        );
    }

    #[test]
    fn data_model_ready_refs_are_sorted_by_run_then_segment_sequence() {
        let dir = tempfile::tempdir().unwrap();
        let journal = SessionJournal::open(dir.path().join("journal.sqlite3")).unwrap();
        let profile = profile();
        let mapping = MappingSnapshot::from_slice(MAPPING).unwrap();
        begin(&journal, &profile, &mapping);
        journal
            .mark_run_started("session-1", "r0000", 100, 10_000_000)
            .unwrap();
        insert_ready_segment_row(&journal, "session-1", "r0000", 1, 300, 300);
        insert_ready_segment_row(&journal, "session-1", "r0000", 0, 0, 300);
        let mut paused = zero_outcome(dir.path(), &mapping, RecordingStopReason::Pause);
        paused.anchor_qpc = Some(100);
        paused.frame_count = 600;
        paused.session_frame_end = 600;
        paused.segment_count = 2;
        journal
            .finish_run(&paused, RunTerminalState::Paused)
            .unwrap();
        journal
            .begin_test_run(&TestRunStart {
                request_id: "request-2",
                session_id: "session-1",
                run_id: "r0001",
                run_seq: 1,
                session_frame_start: 600,
                process_id: 84,
                target_window_id: "replacement-window-digest",
                executable_sha256: Some("sha256:exe"),
                executable_sha256_missing_reason: None,
                profile: &profile,
                mapping: &mapping,
                view_calibration: None,
            })
            .unwrap();
        insert_ready_segment_row(&journal, "session-1", "r0001", 1, 900, 300);
        insert_ready_segment_row(&journal, "session-1", "r0001", 0, 600, 300);

        let refs = journal.ready_segments_for_data_model("session-1").unwrap();
        assert_eq!(
            refs.iter()
                .map(|segment| (
                    segment.run_seq,
                    segment.run_name.as_str(),
                    segment.segment_index,
                    segment.session_frame_start,
                ))
                .collect::<Vec<_>>(),
            vec![
                (0, "r0000", 0, 0),
                (0, "r0000", 1, 300),
                (1, "r0001", 0, 600),
                (1, "r0001", 1, 900),
            ]
        );
    }

    #[test]
    fn history_survives_reopen_and_keeps_failure_reason() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.sqlite3");
        let profile = profile();
        let mapping = MappingSnapshot::from_slice(MAPPING).unwrap();
        {
            let journal = SessionJournal::open(&path).unwrap();
            begin(&journal, &profile, &mapping);
            journal
                .fail_run("session-1", "r0000", "CAPTURE_TEST_FAILED")
                .unwrap();
        }

        let reopened = SessionJournal::open(&path).unwrap();
        let sessions = reopened.list_sessions().unwrap();
        let detail = reopened.session_detail("session-1").unwrap();

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].state, "faulted");
        assert_eq!(
            sessions[0].error_code.as_deref(),
            Some("CAPTURE_TEST_FAILED")
        );
        assert_eq!(detail.runs[0].state, "faulted");
        assert_eq!(
            detail.runs[0].error_code.as_deref(),
            Some("CAPTURE_TEST_FAILED")
        );
    }

    #[test]
    fn stop_before_first_frame_is_a_clean_terminal_transition() {
        let dir = tempfile::tempdir().unwrap();
        let journal = SessionJournal::open(dir.path().join("journal.sqlite3")).unwrap();
        let profile = profile();
        let mapping = MappingSnapshot::from_slice(MAPPING).unwrap();
        begin(&journal, &profile, &mapping);
        let outcome = zero_outcome(dir.path(), &mapping, RecordingStopReason::Manual);

        journal
            .finish_run(&outcome, RunTerminalState::Completed)
            .unwrap();

        let connection = journal.connection.lock();
        let run_state: String = connection
            .query_row("SELECT state FROM capture_runs", [], |row| row.get(0))
            .unwrap();
        let session_state: String = connection
            .query_row("SELECT state FROM sessions", [], |row| row.get(0))
            .unwrap();
        assert_eq!(run_state, "completed");
        assert_eq!(session_state, "completed");
        drop(connection);

        let claimed = journal.claim_next_data_model_job().unwrap().unwrap();
        assert_eq!(claimed.state, DataModelJobState::Finalizing);
        let empty = journal.mark_data_model_empty("session-1").unwrap();
        assert_eq!(empty.state, DataModelJobState::Cancelled);
        assert!(empty.finalize_requested);
        assert_eq!(empty.ready_segments, 0);
        assert_eq!(empty.processed_segments, 0);
        assert!(empty.output_relative_path.is_none());
        assert!(empty.error_code.is_none());
        assert!(journal.claim_next_data_model_job().unwrap().is_none());
    }

    #[test]
    fn disk_watermark_stop_is_persisted_as_a_clean_completion() {
        let dir = tempfile::tempdir().unwrap();
        let journal = SessionJournal::open(dir.path().join("journal.sqlite3")).unwrap();
        let profile = profile();
        let mapping = MappingSnapshot::from_slice(MAPPING).unwrap();
        begin(&journal, &profile, &mapping);
        let outcome = zero_outcome(dir.path(), &mapping, RecordingStopReason::DiskLow);

        journal
            .finish_run(&outcome, RunTerminalState::Completed)
            .unwrap();

        let connection = journal.connection.lock();
        let persisted = connection
            .query_row(
                "SELECT r.state, r.stop_reason, s.state
                 FROM capture_runs r JOIN sessions s ON s.id = r.session_id",
                [],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(
            persisted,
            ("completed".into(), "disk_low".into(), "completed".into())
        );
    }

    #[test]
    fn diagnostic_snapshot_is_scoped_and_omits_window_process_and_paths() {
        let dir = tempfile::tempdir().unwrap();
        let journal = SessionJournal::open(dir.path().join("journal.sqlite3")).unwrap();
        let profile = profile();
        let mapping = MappingSnapshot::from_slice(MAPPING).unwrap();
        begin(&journal, &profile, &mapping);

        let snapshot = journal.diagnostic_snapshot(Some("session-1")).unwrap();
        let json = serde_json::to_string(&snapshot).unwrap();

        assert_eq!(snapshot.sessions.len(), 1);
        assert_eq!(snapshot.runs.len(), 1);
        assert_eq!(snapshot.data_model_jobs.len(), 1);
        assert_eq!(
            snapshot.data_model_jobs[0].state,
            DataModelJobState::Waiting
        );
        assert!(json.contains("test-profile"));
        assert!(!json.contains("window-digest"));
        assert!(!json.contains("sha256:exe"));
        assert!(!json.contains("process_id"));
        assert!(!json.contains("target_window_id"));
        assert!(!json.contains("video_path"));
    }

    const FREE_RECORDING_HASH: &str =
        "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const FREE_DEVICE_HASH: &str =
        "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const FREE_FILE_HASH: &str =
        "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

    fn free_write(calibration_id: &str) -> ViewCalibrationWrite<'_> {
        ViewCalibrationWrite {
            calibration_id,
            kind: ViewCalibrationKind::Free,
            parent_calibration_id: None,
            game_id: "cod-steam-release-test",
            recording_profile_hash: FREE_RECORDING_HASH,
            device_profile_hash: FREE_DEVICE_HASH,
            created_qpc: 1,
            // 自由段没有裁决——圈数未声明，无从判定偏差。
            verdict_pass: None,
            counts_per_360_measured: None,
            deviation_pct: None,
            polling_hz_p50: Some(1_000.0),
            file_path: "calibrations/free/input-calibration.json",
            file_sha256: FREE_FILE_HASH,
        }
    }

    #[test]
    fn free_measurement_records_without_a_verdict_and_stays_out_of_the_valid_pool() {
        let dir = tempfile::tempdir().unwrap();
        let journal = SessionJournal::open(dir.path().join("journal.sqlite3")).unwrap();

        let recorded = journal
            .record_view_calibration(&free_write("11111111-2222-3333-4444-555555555555"))
            .expect("free measurement is recordable");
        assert_eq!(recorded.kind, "free");
        assert_eq!(recorded.verdict, None);

        // 隔离的要害：自由段永远不是"有效标定"，所以抽检拿不到它当父项、
        // preflight 也看不见它（两者走同一查询）。
        assert_eq!(
            journal
                .latest_valid_view_calibration(
                    "cod-steam-release-test",
                    FREE_RECORDING_HASH,
                    FREE_DEVICE_HASH
                )
                .unwrap(),
            None
        );
        // 但历史列表要看得见——读数就是自由段的全部产出。
        assert_eq!(journal.list_view_calibrations(10).unwrap().len(), 1);
    }

    #[test]
    fn journal_rejects_kind_and_verdict_disagreement() {
        let dir = tempfile::tempdir().unwrap();
        let journal = SessionJournal::open(dir.path().join("journal.sqlite3")).unwrap();

        // 自由段谎报裁决 → 拒。
        let mut lying_free = free_write("11111111-2222-3333-4444-555555555555");
        lying_free.verdict_pass = Some(true);
        assert!(
            journal
                .record_view_calibration(&lying_free)
                .unwrap_err()
                .to_string()
                .contains("verdict presence disagree")
        );

        // 正式标定缺裁决 → 也拒，否则"用 free 的形状混进有效标定池"就有路。
        let mut verdictless_full = free_write("22222222-3333-4444-5555-666666666666");
        verdictless_full.kind = ViewCalibrationKind::Full;
        assert!(
            journal
                .record_view_calibration(&verdictless_full)
                .unwrap_err()
                .to_string()
                .contains("verdict presence disagree")
        );
    }
}

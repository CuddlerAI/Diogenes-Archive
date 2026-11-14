//! probe run 级输出到客户端 segment 的安全收口核心。
//!
//! 这一层只消费已经存在的事实：
//!
//! - [`E2eReport`] 通过的 `present_seq ↔ session_frame` 水印配对；
//! - 客户端 journal 给出的精确 `[session_frame_start, frame_count)` 段边界；
//! - 未来控制协议给出的显式 [`ProbeFlushedEvidence`]。
//!
//! 它不会按 JSONL 行号、绝对奇偶或 QPC 猜边界，也不会把「配置已经关闭」冒充
//! writer 已清空。产物先写同盘 staging，逐文件复核 size/SHA-256 后再 rename；本模块
//! 不写客户端 journal，也不实现上传。journal 的两列与独立 durable queue 由客户端胶水
//! 在目录提交成功后同一 SQLite 事务写入。
//!
//! EXR 会按 #514 的通用 `classify_frame`/Z 推断读回；全同值、未知状态、非 gameplay、
//! Camera/Action 覆盖不完整都保留逐帧 invalid reason，只形成 local-only 审计段，不分配
//! #524 object key，更不会因为外部诊断报告 `passed=true` 就进入 durable candidate。

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Component, Path, PathBuf};

use exr::prelude::{Blocks, Compression, FlatSamples, LineOrder, read_first_flat_layer_from_file};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::archive_contract::{
    ARCHIVE_BUCKET, ARCHIVE_RETENTION_DAYS, ARCHIVE_SCHEMA_VERSION, ArchiveLane,
    manifest_object_key, object_key, validate_target,
};
use crate::capture_timing::{
    DepthIndexEntry, FrameContent, INDEX_SCHEMA_V3, INDEX_SCHEMA_V4, INDEX_SCHEMA_V5,
    INDEX_SCHEMA_V6, StorageTransform, classify_frame, infer_z_convention,
};
use crate::frame_health::FrameDefect;
use crate::pairing::{AbsentReason, FramePair};
use crate::pipeline::E2eReport;

pub const FLUSH_EVIDENCE_SCHEMA: &str = "depth-probe-flushed/v1";
pub const FRAME_STATE_EVIDENCE_SCHEMA: &str = "depth-frame-state/v1";
pub const SEGMENT_MANIFEST_SCHEMA: &str = "depth-lab-segment/v1";
const SEGMENT_INDEX_NAME: &str = "depth-index.jsonl";
const SEGMENT_MANIFEST_NAME: &str = "depth-segment.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamIdentity {
    pub session_id: String,
    pub run_id: String,
    pub generation: u64,
}

impl StreamIdentity {
    pub fn new(session_id: &str, run_id: &str, generation: u64) -> Result<Self, FinalizeError> {
        if session_id.trim().is_empty() || run_id.trim().is_empty() {
            return Err(FinalizeError::Contract(
                "session_id/run_id must not be empty".into(),
            ));
        }
        Ok(Self {
            session_id: session_id.to_owned(),
            run_id: run_id.to_owned(),
            generation,
        })
    }
}

/// CA-3 将来要生产的证明。本 PR 只定义并消费它。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeFlushedEvidence {
    pub schema: String,
    pub identity: StreamIdentity,
    /// 证明生成时 writer channel 中仍待写的帧数；必须严格为 0。
    pub writer_queue_depth: u64,
    /// probe 已经观察并完成处理的最后一个展开后 present 序号。
    pub last_present_seq: u64,
    /// 本次收口允许拥有的最后一个 present 序号（含）。
    pub cutoff_present_seq: u64,
}

impl ProbeFlushedEvidence {
    pub fn validate_for(&self, expected: &StreamIdentity) -> Result<(), FinalizeError> {
        if self.schema != FLUSH_EVIDENCE_SCHEMA {
            return Err(FinalizeError::Contract(format!(
                "unsupported flushed evidence schema `{}`",
                self.schema
            )));
        }
        if &self.identity != expected {
            return Err(FinalizeError::IdentityMismatch {
                expected: expected.clone(),
                actual: self.identity.clone(),
            });
        }
        if self.writer_queue_depth != 0 {
            return Err(FinalizeError::NotFlushed {
                writer_queue_depth: self.writer_queue_depth,
            });
        }
        if self.last_present_seq != self.cutoff_present_seq {
            return Err(FinalizeError::Contract(format!(
                "flushed proof stops at present_seq {} but cutoff is {}",
                self.last_present_seq, self.cutoff_present_seq
            )));
        }
        Ok(())
    }
}

/// 内容状态证明的通用词表。它描述的是 RGB 帧所处状态，不从 depth clear 值反推；
/// 特别是不能把 FO4 的某个 clear 常量写死成菜单或 gameplay。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrameCaptureState {
    Gameplay,
    Menu,
    Loading,
    PipBoy,
    Cutscene,
    FastTravel,
    OtherNonGameplay,
    Unknown,
}

impl FrameCaptureState {
    fn is_gameplay(self) -> bool {
        self == Self::Gameplay
    }
}

/// 上游状态分类器交给 finalizer 的半开区间。当前 PR 只定义/消费边界，
/// 不接 UI、Camera/Action 或生命周期 IPC。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameStateBoundary {
    pub session_frame_start: u64,
    pub frame_count: u64,
    pub state: FrameCaptureState,
    /// T-A 只消费证明，不生成 Camera/Action 数据；两者缺一都不能入队。
    pub camera_coverage_proven: bool,
    pub action_coverage_proven: bool,
}

impl FrameStateBoundary {
    fn end(self) -> Result<u64, FinalizeError> {
        if self.frame_count == 0 {
            return Err(FinalizeError::Contract(
                "frame state boundary must not be empty".into(),
            ));
        }
        self.session_frame_start
            .checked_add(self.frame_count)
            .ok_or_else(|| FinalizeError::Contract("frame state boundary overflows".into()))
    }
}

/// 与 Flushed 证明一样，状态证据必须绑定同一 session/run/generation。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameStateEvidence {
    pub schema: String,
    pub identity: StreamIdentity,
    pub boundaries: Vec<FrameStateBoundary>,
}

impl FrameStateEvidence {
    pub fn validate_for(&self, expected: &StreamIdentity) -> Result<(), FinalizeError> {
        if self.schema != FRAME_STATE_EVIDENCE_SCHEMA {
            return Err(FinalizeError::Contract(format!(
                "unsupported frame state evidence schema `{}`",
                self.schema
            )));
        }
        if &self.identity != expected {
            return Err(FinalizeError::IdentityMismatch {
                expected: expected.clone(),
                actual: self.identity.clone(),
            });
        }
        let mut previous_end = None;
        for boundary in &self.boundaries {
            let end = boundary.end()?;
            if previous_end.is_some_and(|previous| boundary.session_frame_start < previous) {
                return Err(FinalizeError::Contract(
                    "frame state boundaries overlap or are out of order".into(),
                ));
            }
            previous_end = Some(end);
        }
        Ok(())
    }

    fn validity_at(&self, session_frame: u64) -> FrameValidityProof {
        self.boundaries
            .iter()
            .find_map(|boundary| {
                let end = boundary.end().ok()?;
                (boundary.session_frame_start..end)
                    .contains(&session_frame)
                    .then_some(FrameValidityProof {
                        state: boundary.state,
                        camera_coverage_proven: boundary.camera_coverage_proven,
                        action_coverage_proven: boundary.action_coverage_proven,
                    })
            })
            .unwrap_or(FrameValidityProof {
                state: FrameCaptureState::Unknown,
                camera_coverage_proven: false,
                action_coverage_proven: false,
            })
    }
}

#[derive(Debug, Clone, Copy)]
struct FrameValidityProof {
    state: FrameCaptureState,
    camera_coverage_proven: bool,
    action_coverage_proven: bool,
}

/// 每帧不可交付的稳定原因。原因进入本地 manifest/index，不通过删行制造伪连续段。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrameInvalidReason {
    AllSameDepth,
    AllDepthNonFinite,
    StorageQuantizationCollapse,
    StateUnverified,
    NonGameplayState,
    PairingProbeDropped,
    PairingWatermarkUndecodable,
    PairingOutsideProbeWindow,
    PairingDsvMissing,
    PairingUnexplainedAbsence,
    CameraModeNotPlayer,
    CameraCoverageMissing,
    ActionCoverageMissing,
    ZConventionMismatch,
}

/// 只接受 journal 已经落定的 ready segment 边界。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegmentBoundary {
    pub segment_index: u32,
    pub session_frame_start: u64,
    pub frame_count: u64,
}

impl SegmentBoundary {
    fn end(self) -> Result<u64, FinalizeError> {
        if self.frame_count == 0 {
            return Err(FinalizeError::Contract(format!(
                "segment {} has an empty frame interval",
                self.segment_index
            )));
        }
        self.session_frame_start
            .checked_add(self.frame_count)
            .ok_or_else(|| {
                FinalizeError::Contract(format!(
                    "segment {} frame interval overflows",
                    self.segment_index
                ))
            })
    }
}

/// 经过现有水印 + pairing 两道门的映射证据。
///
/// `FramePair.segment_index/segment_frame` 不能作为本模块的边界事实：当前 recorder 的
/// `frames.jsonl.zst` 原始行只有 `frame` 与 `session_frame`，没有 `segment_index`，旧解析器
/// 会把缺字段默认成 0。这里仅信 `session_frame ↔ present_seq`，再与 journal 的区间做精确
/// 连接。
#[derive(Debug, Clone)]
pub struct VerifiedPairingEvidence {
    identity: StreamIdentity,
    pairs: Vec<FramePair>,
    report: E2eReport,
}

impl VerifiedPairingEvidence {
    pub fn new(
        identity: StreamIdentity,
        pairs: Vec<FramePair>,
        report: E2eReport,
    ) -> Result<Self, FinalizeError> {
        if !report.passed || !report.pairing.passes_coverage_gate() {
            return Err(FinalizeError::PairingNotVerified(
                report.failures.join("; "),
            ));
        }
        if report.pairing.total_frames != pairs.len() as u64 {
            return Err(FinalizeError::Contract(format!(
                "pairing report covers {} frames but table has {} rows",
                report.pairing.total_frames,
                pairs.len()
            )));
        }
        let eligible = pairs
            .iter()
            .filter(|pair| {
                !pair.duplicate && pair.depth_absent_reason != Some(AbsentReason::OddFrameBySpec)
            })
            .count() as u64;
        let paired = pairs
            .iter()
            .filter(|pair| pair.depth.as_ref().is_some_and(|depth| !depth.inherited))
            .count() as u64;
        let inherited = pairs
            .iter()
            .filter(|pair| pair.depth.as_ref().is_some_and(|depth| depth.inherited))
            .count() as u64;
        let coverage = if eligible == 0 {
            1.0
        } else {
            paired as f64 / eligible as f64
        };
        if report.pairing.eligible_even_frames != eligible
            || report.pairing.paired_even_frames != paired
            || report.pairing.inherited_frames != inherited
            || (report.pairing.coverage - coverage).abs() > f64::EPSILON
        {
            return Err(FinalizeError::Contract(
                "pairing report counters do not match frame-pair rows".into(),
            ));
        }
        let mut previous: Option<u64> = None;
        let mut previous_qpc: Option<i64> = None;
        let mut previous_present_seq: Option<u64> = None;
        for pair in &pairs {
            if pair.depth.is_some() == pair.depth_absent_reason.is_some() {
                return Err(FinalizeError::Contract(format!(
                    "frame-pair {} must contain exactly one depth or absent reason",
                    pair.session_frame
                )));
            }
            if let Some(depth) = &pair.depth {
                if pair.present_seq != Some(depth.present_seq) {
                    return Err(FinalizeError::Contract(format!(
                        "frame-pair {} RGB/depth identity mismatch: {:?}/{}",
                        pair.session_frame, pair.present_seq, depth.present_seq
                    )));
                }
                if depth.inherited && !pair.duplicate {
                    return Err(FinalizeError::Contract(format!(
                        "frame-pair {} inherits depth without an RGB duplicate",
                        pair.session_frame
                    )));
                }
                if pair.duplicate && !depth.inherited {
                    return Err(FinalizeError::Contract(format!(
                        "frame-pair {} attaches a new depth to an RGB duplicate",
                        pair.session_frame
                    )));
                }
            }
            if pair.duplicate
                && (previous.and_then(|frame| frame.checked_add(1)) != Some(pair.session_frame)
                    || previous_present_seq != pair.present_seq)
            {
                return Err(FinalizeError::Contract(format!(
                    "frame-pair {} duplicate does not inherit the immediately preceding source identity",
                    pair.session_frame
                )));
            }
            if let (Some(previous_seq), Some(current_seq)) =
                (previous_present_seq, pair.present_seq)
                && (current_seq < previous_seq
                    || (current_seq == previous_seq
                        && previous.and_then(|frame| frame.checked_add(1))
                            != Some(pair.session_frame)))
            {
                return Err(FinalizeError::Contract(format!(
                    "frame-pair present identity regresses or reuses a non-contiguous source at session_frame {}: {} -> {}",
                    pair.session_frame, previous_seq, current_seq
                )));
            }
            if previous.is_some_and(|value| pair.session_frame <= value) {
                return Err(FinalizeError::PairingOrder {
                    previous: previous.expect("checked above"),
                    current: pair.session_frame,
                });
            }
            if previous_qpc.is_some_and(|value| pair.source_qpc < value) {
                return Err(FinalizeError::Contract(format!(
                    "frame-pair QPC regresses at session_frame {}",
                    pair.session_frame
                )));
            }
            previous = Some(pair.session_frame);
            previous_qpc = Some(pair.source_qpc);
            previous_present_seq = pair.present_seq;
        }
        Ok(Self {
            identity,
            pairs,
            report,
        })
    }

    pub fn identity(&self) -> &StreamIdentity {
        &self.identity
    }

    pub fn report(&self) -> &E2eReport {
        &self.report
    }
}

pub struct FinalizeRequest<'a> {
    pub identity: &'a StreamIdentity,
    pub archive_lane: ArchiveLane,
    pub flushed: &'a ProbeFlushedEvidence,
    pub pairing: &'a VerifiedPairingEvidence,
    pub frame_states: &'a FrameStateEvidence,
    pub boundaries: &'a [SegmentBoundary],
    pub probe_root: &'a Path,
    /// 独立 depth segment 根，不是 production `segments/` 五件套目录。
    pub output_root: &'a Path,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedDepthSegment {
    pub segment_index: u32,
    pub manifest_path: PathBuf,
    pub manifest_sha256: String,
    pub object_count: usize,
    /// 只有完整配对、逐帧状态均为 gameplay、且内容校验通过时才为 true。
    pub candidate_eligible: bool,
    pub invalid_frame_count: usize,
    pub archive_manifest_object_key: Option<String>,
    pub archive_run_index_object_key: Option<String>,
}

/// U2 archive helper 复核后的单个真实 depth 帧。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedSegmentFrame {
    pub session_frame: u64,
    pub present_seq: u64,
    pub source_path: PathBuf,
    pub size_bytes: u64,
    pub sha256: String,
}

/// U2 只接受 #529 已发布且仍完整的 candidate segment。这个类型由
/// [`verify_candidate_segment`] 构造，调用方不能靠 queue 行跳过文件复核。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedCandidateSegment {
    pub identity: StreamIdentity,
    pub lane: ArchiveLane,
    pub segment_index: u32,
    pub session_frame_start: u64,
    pub frame_count: u64,
    pub index_path: PathBuf,
    pub frames: Vec<VerifiedSegmentFrame>,
}

#[derive(Debug)]
pub enum FinalizeError {
    Contract(String),
    IdentityMismatch {
        expected: StreamIdentity,
        actual: StreamIdentity,
    },
    NotFlushed {
        writer_queue_depth: u64,
    },
    PairingNotVerified(String),
    PairingOrder {
        previous: u64,
        current: u64,
    },
    DuplicatePresentSeq(u64),
    IndexOrder {
        previous: u64,
        current: u64,
    },
    MissingPair(u64),
    MissingIndex(u64),
    OutOfBounds(String),
    FileMissing(PathBuf),
    Integrity(String),
    Io(String),
}

impl std::fmt::Display for FinalizeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Contract(message) => write!(f, "contract violation: {message}"),
            Self::IdentityMismatch { expected, actual } => write!(
                f,
                "stream identity mismatch: expected {expected:?}, got {actual:?}"
            ),
            Self::NotFlushed { writer_queue_depth } => write!(
                f,
                "probe is not Flushed: writer queue depth is {writer_queue_depth}"
            ),
            Self::PairingNotVerified(message) => {
                write!(f, "pairing evidence did not pass: {message}")
            }
            Self::PairingOrder { previous, current } => write!(
                f,
                "frame-pairs are not strictly ordered: {previous} then {current}"
            ),
            Self::DuplicatePresentSeq(seq) => write!(f, "duplicate present_seq {seq}"),
            Self::IndexOrder { previous, current } => write!(
                f,
                "depth-index is not strictly ordered: {previous} then {current}"
            ),
            Self::MissingPair(seq) => write!(f, "depth present_seq {seq} has no verified pair"),
            Self::MissingIndex(seq) => {
                write!(f, "verified present_seq {seq} is missing from depth-index")
            }
            Self::OutOfBounds(message) => write!(f, "out of bounds: {message}"),
            Self::FileMissing(path) => {
                write!(f, "required depth file is missing: {}", path.display())
            }
            Self::Integrity(message) => write!(f, "depth integrity check failed: {message}"),
            Self::Io(message) => write!(f, "depth finalization I/O failed: {message}"),
        }
    }
}

impl std::error::Error for FinalizeError {}

#[derive(Debug, Clone)]
struct MappedEntry {
    entry: DepthIndexEntry,
    source_path: PathBuf,
    session_frame: u64,
    segment_index: u32,
    segment_frame: u64,
    capture_state: FrameCaptureState,
    invalid_reasons: Vec<FrameInvalidReason>,
    source_size: u64,
    source_sha256: String,
}

#[derive(Debug, Clone)]
struct IndexedEntry {
    entry: DepthIndexEntry,
    source_path: PathBuf,
    content: FrameContent,
    observed_z: Option<crate::decode::ZConvention>,
    health_defects: Vec<FrameDefect>,
    source_size: u64,
    source_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct FrameValidityRecord {
    session_frame: u64,
    /// 只保存已验证 pair 给出的 QPC；T-A 不按数组相邻或固定帧数猜空窗。
    source_qpc: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    present_seq: Option<u64>,
    capture_state: FrameCaptureState,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    invalid_reasons: Vec<FrameInvalidReason>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SegmentObject {
    kind: String,
    relative_path: String,
    size: u64,
    sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SegmentManifest {
    schema: String,
    identity: StreamIdentity,
    segment_index: u32,
    session_frame_start: u64,
    frame_count: u64,
    first_present_seq: u64,
    last_present_seq: u64,
    pairing_coverage: String,
    candidate_eligible: bool,
    invalid_frame_count: usize,
    frame_validity: Vec<FrameValidityRecord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    archive_adapter: Option<ArchiveAdapter>,
    objects: Vec<SegmentObject>,
}

/// 本地 segment 不是 #524 的最终 session manifest。只有可入队 segment 才钉住正式
/// 转换目标；local-only invalid segment 连 object key 都没有。未来 adapter 只能聚合/压缩，
/// 不能改桶、lane、retention 或对象键。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ArchiveAdapter {
    schema_version: String,
    lane: ArchiveLane,
    bucket: String,
    retention_days: u16,
    manifest_object_key: String,
    run_index_object_key: String,
}

#[derive(Serialize)]
struct SegmentIndexRow<'a> {
    #[serde(flatten)]
    entry: &'a DepthIndexEntry,
    session_frame: u64,
    segment_index: u32,
    segment_frame: u64,
    capture_state: FrameCaptureState,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    invalid_reasons: &'a Vec<FrameInvalidReason>,
}

pub fn finalize_segments(
    request: &FinalizeRequest<'_>,
) -> Result<Vec<PublishedDepthSegment>, FinalizeError> {
    finalize_segments_inner(request, FaultPoint::None)
}

/// 重新读取、哈希并校验 durable queue 指向的 segment manifest 及其全部对象。
/// queue 是调度事实，不是完整性证明；上传前必须再次经过这里。
pub fn verify_candidate_segment(
    manifest_path: &Path,
    expected_identity: &StreamIdentity,
    expected_segment_index: u32,
    expected_manifest_sha256: &str,
) -> Result<VerifiedCandidateSegment, FinalizeError> {
    if manifest_path.file_name().and_then(|name| name.to_str()) != Some(SEGMENT_MANIFEST_NAME) {
        return Err(FinalizeError::OutOfBounds(format!(
            "candidate manifest path is not {SEGMENT_MANIFEST_NAME}: {}",
            manifest_path.display()
        )));
    }
    let directory = manifest_path.parent().ok_or_else(|| {
        FinalizeError::OutOfBounds(format!(
            "manifest has no parent: {}",
            manifest_path.display()
        ))
    })?;
    ensure_real_directory(directory)?;
    let bytes = fs::read(manifest_path).map_err(io_error)?;
    let actual_manifest_sha256 = sha256_bytes(&bytes);
    if actual_manifest_sha256 != expected_manifest_sha256 {
        return Err(FinalizeError::Integrity(format!(
            "candidate manifest SHA-256 mismatch for {}: expected {}, got {}",
            manifest_path.display(),
            expected_manifest_sha256,
            actual_manifest_sha256
        )));
    }
    let manifest: SegmentManifest = serde_json::from_slice(&bytes).map_err(|error| {
        FinalizeError::Integrity(format!("parse {}: {error}", manifest_path.display()))
    })?;
    if &manifest.identity != expected_identity || manifest.segment_index != expected_segment_index {
        return Err(FinalizeError::IdentityMismatch {
            expected: expected_identity.clone(),
            actual: manifest.identity,
        });
    }
    if !manifest.candidate_eligible || manifest.invalid_frame_count != 0 {
        return Err(FinalizeError::Contract(format!(
            "segment {} is local-only and cannot enter archive upload",
            expected_segment_index
        )));
    }
    let adapter = manifest.archive_adapter.as_ref().ok_or_else(|| {
        FinalizeError::Contract(format!(
            "candidate segment {} has no #524 archive adapter",
            expected_segment_index
        ))
    })?;
    let boundary = SegmentBoundary {
        segment_index: manifest.segment_index,
        session_frame_start: manifest.session_frame_start,
        frame_count: manifest.frame_count,
    };
    validate_published(directory, expected_identity, adapter.lane, boundary)?;

    let index_path = directory.join(SEGMENT_INDEX_NAME);
    let index_text = fs::read_to_string(&index_path).map_err(io_error)?;
    let by_path: BTreeMap<&str, &SegmentObject> = manifest
        .objects
        .iter()
        .filter(|object| object.kind == "depth_exr")
        .map(|object| (object.relative_path.as_str(), object))
        .collect();
    let mut frames = Vec::new();
    let mut seen_session_frames = BTreeSet::new();
    let mut referenced_paths = BTreeSet::new();
    for (line_index, line) in index_text.lines().enumerate() {
        if line.trim().is_empty() {
            return Err(FinalizeError::Integrity(format!(
                "empty segment index row {} in {}",
                line_index + 1,
                index_path.display()
            )));
        }
        let row: serde_json::Value = serde_json::from_str(line).map_err(|error| {
            FinalizeError::Integrity(format!(
                "parse segment index row {} in {}: {error}",
                line_index + 1,
                index_path.display()
            ))
        })?;
        let entry: DepthIndexEntry = serde_json::from_value(row.clone()).map_err(|error| {
            FinalizeError::Integrity(format!(
                "parse durable depth entry row {} in {}: {error}",
                line_index + 1,
                index_path.display()
            ))
        })?;
        // #542 v4 的 transform 是交付语义，不是可忽略元数据。durable queue 重放时
        // 重新走与首次 finalize 相同的 schema/storage/transform 门，不能只信旧 manifest。
        validate_index_entry(&entry, directory, u64::MAX)?;
        let session_frame = row
            .get("session_frame")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| FinalizeError::Integrity("segment index lacks session_frame".into()))?;
        let present_seq = entry.present_seq;
        let relative_path = entry.path;
        if !seen_session_frames.insert(session_frame) {
            return Err(FinalizeError::Integrity(format!(
                "duplicate session_frame {session_frame} in candidate segment"
            )));
        }
        let object = by_path.get(relative_path.as_str()).ok_or_else(|| {
            FinalizeError::Integrity(format!(
                "segment index path {relative_path} is not declared by manifest"
            ))
        })?;
        referenced_paths.insert(relative_path.clone());
        frames.push(VerifiedSegmentFrame {
            session_frame,
            present_seq,
            source_path: directory.join(&relative_path),
            size_bytes: object.size,
            sha256: object.sha256.clone(),
        });
    }
    if frames.is_empty() || referenced_paths.len() != by_path.len() {
        return Err(FinalizeError::Integrity(format!(
            "segment {} index/object coverage mismatch: {} rows reference {} paths, {} EXRs",
            expected_segment_index,
            frames.len(),
            referenced_paths.len(),
            by_path.len()
        )));
    }
    Ok(VerifiedCandidateSegment {
        identity: expected_identity.clone(),
        lane: adapter.lane,
        segment_index: expected_segment_index,
        session_frame_start: boundary.session_frame_start,
        frame_count: boundary.frame_count,
        index_path,
        frames,
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FaultPoint {
    None,
    #[cfg(test)]
    AfterFirstStage,
    #[cfg(test)]
    AfterFirstPublish,
}

fn finalize_segments_inner(
    request: &FinalizeRequest<'_>,
    _fault: FaultPoint,
) -> Result<Vec<PublishedDepthSegment>, FinalizeError> {
    request.flushed.validate_for(request.identity)?;
    request.frame_states.validate_for(request.identity)?;
    if request.pairing.identity() != request.identity {
        return Err(FinalizeError::IdentityMismatch {
            expected: request.identity.clone(),
            actual: request.pairing.identity().clone(),
        });
    }
    let boundaries = validate_boundaries(request.boundaries)?;
    let PairingMapping {
        by_present_seq: mapping,
        mut frame_validity,
    } = build_mapping(request.pairing, request.frame_states, &boundaries)?;
    let indexed = read_and_validate_index(request.probe_root, request.flushed.cutoff_present_seq)?;
    let grouped = join_index_to_mapping(indexed, &mapping, &boundaries, &mut frame_validity)?;

    fs::create_dir_all(request.output_root).map_err(|error| {
        FinalizeError::Io(format!(
            "create output root {}: {error}",
            request.output_root.display()
        ))
    })?;
    ensure_real_directory(request.output_root)?;
    let staging_root = request.output_root.join(".staging");
    fs::create_dir_all(&staging_root).map_err(|error| {
        FinalizeError::Io(format!("create {}: {error}", staging_root.display()))
    })?;
    ensure_real_directory(&staging_root)?;

    let mut published = Vec::with_capacity(boundaries.len());
    #[cfg(test)]
    let first_segment_index = boundaries.keys().next().copied();
    for boundary in boundaries.values().copied() {
        let entries = grouped.get(&boundary.segment_index).ok_or_else(|| {
            FinalizeError::Contract(format!(
                "segment {} has no fully paired depth frame",
                boundary.segment_index
            ))
        })?;
        let final_dir = request
            .output_root
            .join(format!("{:05}", boundary.segment_index));
        match fs::symlink_metadata(&final_dir) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    return Err(FinalizeError::Integrity(format!(
                        "published segment path is not a real directory: {}",
                        final_dir.display()
                    )));
                }
                published.push(validate_published(
                    &final_dir,
                    request.identity,
                    request.archive_lane,
                    boundary,
                )?);
                continue;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(FinalizeError::Io(format!(
                    "stat published segment {}: {error}",
                    final_dir.display()
                )));
            }
        }

        let staging_dir = staging_root.join(format!(
            "{:05}-g{}",
            boundary.segment_index, request.identity.generation
        ));
        if staging_dir.exists() {
            let metadata = fs::symlink_metadata(&staging_dir).map_err(io_error)?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(FinalizeError::Integrity(format!(
                    "unsafe stale staging path {}",
                    staging_dir.display()
                )));
            }
            fs::remove_dir_all(&staging_dir).map_err(|error| {
                FinalizeError::Io(format!(
                    "remove stale staging {}: {error}",
                    staging_dir.display()
                ))
            })?;
        }
        stage_segment(
            &staging_dir,
            request.identity,
            request.archive_lane,
            boundary,
            entries,
            frame_validity
                .values()
                .filter(|record| {
                    (boundary.session_frame_start..boundary.end().expect("validated boundary"))
                        .contains(&record.session_frame)
                })
                .cloned()
                .collect(),
            request.pairing.report().pairing.coverage,
        )?;

        #[cfg(test)]
        if _fault == FaultPoint::AfterFirstStage
            && first_segment_index == Some(boundary.segment_index)
        {
            return Err(FinalizeError::Io(
                "injected interruption after staging".into(),
            ));
        }

        fs::rename(&staging_dir, &final_dir).map_err(|error| {
            FinalizeError::Io(format!(
                "atomically publish {} -> {}: {error}",
                staging_dir.display(),
                final_dir.display()
            ))
        })?;
        #[cfg(test)]
        if _fault == FaultPoint::AfterFirstPublish
            && first_segment_index == Some(boundary.segment_index)
        {
            return Err(FinalizeError::Io(
                "injected interruption after filesystem publish".into(),
            ));
        }
        published.push(validate_published(
            &final_dir,
            request.identity,
            request.archive_lane,
            boundary,
        )?);
    }
    Ok(published)
}

fn validate_boundaries(
    input: &[SegmentBoundary],
) -> Result<BTreeMap<u32, SegmentBoundary>, FinalizeError> {
    if input.is_empty() {
        return Err(FinalizeError::Contract(
            "at least one ready segment boundary is required".into(),
        ));
    }
    let mut by_index = BTreeMap::new();
    let mut ranges = Vec::with_capacity(input.len());
    for boundary in input {
        let end = boundary.end()?;
        if by_index.insert(boundary.segment_index, *boundary).is_some() {
            return Err(FinalizeError::Contract(format!(
                "duplicate segment boundary {}",
                boundary.segment_index
            )));
        }
        ranges.push((boundary.session_frame_start, end, boundary.segment_index));
    }
    ranges.sort_unstable();
    for window in ranges.windows(2) {
        if window[0].1 > window[1].0 {
            return Err(FinalizeError::Contract(format!(
                "segment {} overlaps segment {}",
                window[0].2, window[1].2
            )));
        }
    }
    Ok(by_index)
}

#[derive(Debug, Clone)]
struct PairTarget {
    session_frame: u64,
    segment_index: u32,
    segment_frame: u64,
    dsv_id: String,
}

struct PairingMapping {
    by_present_seq: BTreeMap<u64, Vec<PairTarget>>,
    frame_validity: BTreeMap<u64, FrameValidityRecord>,
}

fn build_mapping(
    pairing: &VerifiedPairingEvidence,
    frame_states: &FrameStateEvidence,
    boundaries: &BTreeMap<u32, SegmentBoundary>,
) -> Result<PairingMapping, FinalizeError> {
    let mut mapping = BTreeMap::new();
    let mut frame_validity = BTreeMap::new();
    for pair in &pairing.pairs {
        let mut matched = None;
        for boundary in boundaries.values().copied() {
            let end = boundary.end()?;
            if (boundary.session_frame_start..end).contains(&pair.session_frame) {
                if matched.is_some() {
                    return Err(FinalizeError::OutOfBounds(format!(
                        "session_frame {} belongs to multiple segments",
                        pair.session_frame
                    )));
                }
                matched = Some(boundary);
            }
        }
        let boundary = matched.ok_or_else(|| {
            FinalizeError::OutOfBounds(format!(
                "session_frame {} belongs to no ready segment",
                pair.session_frame
            ))
        })?;
        let validity = frame_states.validity_at(pair.session_frame);
        let capture_state = validity.state;
        let mut invalid_reasons = Vec::new();
        match capture_state {
            FrameCaptureState::Gameplay => {}
            FrameCaptureState::Unknown => invalid_reasons.push(FrameInvalidReason::StateUnverified),
            _ => invalid_reasons.push(FrameInvalidReason::NonGameplayState),
        }
        if pair.camera_mode != crate::pairing::CameraMode::Player {
            invalid_reasons.push(FrameInvalidReason::CameraModeNotPlayer);
        }
        if !validity.camera_coverage_proven {
            invalid_reasons.push(FrameInvalidReason::CameraCoverageMissing);
        }
        if !validity.action_coverage_proven {
            invalid_reasons.push(FrameInvalidReason::ActionCoverageMissing);
        }
        if pair.depth.is_none() {
            match pair.depth_absent_reason {
                Some(AbsentReason::OddFrameBySpec) => {}
                Some(AbsentReason::ProbeDropped) => {
                    invalid_reasons.push(FrameInvalidReason::PairingProbeDropped)
                }
                Some(AbsentReason::WatermarkUndecodable) => {
                    invalid_reasons.push(FrameInvalidReason::PairingWatermarkUndecodable)
                }
                Some(AbsentReason::OutsideProbeWindow) => {
                    invalid_reasons.push(FrameInvalidReason::PairingOutsideProbeWindow)
                }
                Some(AbsentReason::DsvMissing) => {
                    invalid_reasons.push(FrameInvalidReason::PairingDsvMissing)
                }
                None => invalid_reasons.push(FrameInvalidReason::PairingUnexplainedAbsence),
            }
        }
        if frame_validity
            .insert(
                pair.session_frame,
                FrameValidityRecord {
                    session_frame: pair.session_frame,
                    source_qpc: pair.source_qpc,
                    present_seq: pair.present_seq,
                    capture_state,
                    invalid_reasons,
                },
            )
            .is_some()
        {
            return Err(FinalizeError::Contract(format!(
                "duplicate frame-pair session_frame {}",
                pair.session_frame
            )));
        }
        let Some(depth) = pair.depth.as_ref() else {
            continue;
        };
        let target = PairTarget {
            session_frame: pair.session_frame,
            segment_index: boundary.segment_index,
            segment_frame: pair.session_frame - boundary.session_frame_start,
            dsv_id: depth.dsv_id.clone(),
        };
        mapping
            .entry(depth.present_seq)
            .or_insert_with(Vec::new)
            .push(target);
    }
    if mapping.is_empty() {
        return Err(FinalizeError::Contract(
            "verified pairing contains no real depth pair".into(),
        ));
    }
    for boundary in boundaries.values().copied() {
        let end = boundary.end()?;
        let frames = frame_validity
            .range(boundary.session_frame_start..end)
            .map(|(session_frame, _)| *session_frame)
            .collect::<Vec<_>>();
        if frames.len() as u64 != boundary.frame_count
            || frames
                .iter()
                .enumerate()
                .any(|(offset, frame)| *frame != boundary.session_frame_start + offset as u64)
        {
            return Err(FinalizeError::Contract(format!(
                "segment {} does not have one frame-pair row for every journal frame",
                boundary.segment_index
            )));
        }
    }
    Ok(PairingMapping {
        by_present_seq: mapping,
        frame_validity,
    })
}

fn read_and_validate_index(
    probe_root: &Path,
    cutoff_present_seq: u64,
) -> Result<Vec<IndexedEntry>, FinalizeError> {
    ensure_real_directory(probe_root)?;
    ensure_real_directory(&probe_root.join("raw"))?;
    let index_path = probe_root.join(SEGMENT_INDEX_NAME);
    let file = File::open(&index_path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            FinalizeError::FileMissing(index_path.clone())
        } else {
            FinalizeError::Io(format!("open {}: {error}", index_path.display()))
        }
    })?;
    let mut rows = Vec::new();
    let mut seen = BTreeSet::new();
    let mut previous = None;
    for (line_number, line) in BufReader::new(file).lines().enumerate() {
        let line = line.map_err(io_error)?;
        if line.trim().is_empty() {
            return Err(FinalizeError::Contract(format!(
                "depth-index line {} is empty",
                line_number + 1
            )));
        }
        let entry: DepthIndexEntry = serde_json::from_str(&line).map_err(|error| {
            FinalizeError::Contract(format!(
                "depth-index line {} is invalid: {error}",
                line_number + 1
            ))
        })?;
        validate_index_entry(&entry, probe_root, cutoff_present_seq)?;
        if !seen.insert(entry.present_seq) {
            return Err(FinalizeError::DuplicatePresentSeq(entry.present_seq));
        }
        if previous.is_some_and(|value| entry.present_seq <= value) {
            return Err(FinalizeError::IndexOrder {
                previous: previous.expect("checked above"),
                current: entry.present_seq,
            });
        }
        previous = Some(entry.present_seq);
        let source_path = probe_root.join(Path::new(&entry.path));
        let metadata = fs::symlink_metadata(&source_path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                FinalizeError::FileMissing(source_path.clone())
            } else {
                FinalizeError::Io(format!("stat {}: {error}", source_path.display()))
            }
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(FinalizeError::Integrity(format!(
                "{} is not a regular EXR file",
                source_path.display()
            )));
        }
        let source_sha256_before = sha256_file(&source_path)?;
        let (values, width, height) = read_strict_probe_exr(&source_path)?;
        let source_sha256 = sha256_file(&source_path)?;
        let source_size = fs::metadata(&source_path).map_err(io_error)?.len();
        if source_size != metadata.len() || source_sha256 != source_sha256_before {
            return Err(FinalizeError::Integrity(format!(
                "source EXR changed while classifying {}",
                source_path.display()
            )));
        }
        if width != entry.width as usize || height != entry.height as usize {
            return Err(FinalizeError::Integrity(format!(
                "EXR geometry mismatch for present_seq {}: index {}x{}, file {}x{}",
                entry.present_seq, entry.width, entry.height, width, height
            )));
        }
        // 盘上的字节可能是 `1−d`（94 号）。**Z 约定必须在还原回缓冲值之后再判**：
        // 传统 Z 的清除值 1.0 存成 `1−d` 就是 0.0，直接判会得出「reversed」——
        // 于是每一帧都被标成 Z 约定不符，而数据其实完全正确。
        //
        // 变换取值走 `resolve_transform`：v4 缺这个字段时它会拒，
        // 因为那时盘上做没做过变换根本无从判断。
        let transform = entry.resolve_transform().map_err(FinalizeError::Contract)?;
        let health_defects = match entry.resolve_health().map_err(FinalizeError::Contract)? {
            Some(health) => {
                health
                    .validate_stored_values(&values, width)
                    .map_err(FinalizeError::Integrity)?;
                health.defects.clone()
            }
            None => Vec::new(),
        };
        let buffer_values: Vec<f32> = values.iter().map(|&v| transform.invert(v)).collect();
        // 空帧判据对可逆变换不敏感（常量映射成常量），但仍用缓冲值判，
        // 免得将来加一个非单调的变换时这里悄悄变错。
        let content = classify_frame(&buffer_values);
        let observed_z = infer_z_convention(&buffer_values);
        rows.push(IndexedEntry {
            entry,
            source_path,
            content,
            observed_z,
            health_defects,
            source_size,
            source_sha256,
        });
    }
    if rows.is_empty() {
        return Err(FinalizeError::Contract("depth-index is empty".into()));
    }
    Ok(rows)
}

fn read_strict_probe_exr(path: &Path) -> Result<(Vec<f32>, usize, usize), FinalizeError> {
    let image = read_first_flat_layer_from_file(path)
        .map_err(|error| FinalizeError::Integrity(format!("read {}: {error}", path.display())))?;
    let layer = &image.layer_data;
    if layer.encoding.compression != Compression::ZIP16
        || layer.encoding.blocks != Blocks::ScanLines
        || layer.encoding.line_order != LineOrder::Increasing
        || layer.channel_data.list.len() != 1
    {
        return Err(FinalizeError::Integrity(format!(
            "{} is not a single-channel ZIP16 scan-line EXR",
            path.display()
        )));
    }
    let channel = &layer.channel_data.list[0];
    if channel.name.to_string() != crate::EXR_CHANNEL_NAME {
        return Err(FinalizeError::Integrity(format!(
            "{} does not contain the sole `{}` channel",
            path.display(),
            crate::EXR_CHANNEL_NAME
        )));
    }
    let FlatSamples::F16(samples) = &channel.sample_data else {
        return Err(FinalizeError::Integrity(format!(
            "{} channel `{}` is not fp16",
            path.display(),
            crate::EXR_CHANNEL_NAME
        )));
    };
    let values = samples
        .iter()
        .map(|value| value.to_f32())
        .collect::<Vec<_>>();
    let width = layer.size.width();
    let height = layer.size.height();
    if values.len() != width.saturating_mul(height) {
        return Err(FinalizeError::Integrity(format!(
            "{} decoded pixel count does not match its geometry",
            path.display()
        )));
    }
    Ok((values, width, height))
}

fn validate_index_entry(
    entry: &DepthIndexEntry,
    probe_root: &Path,
    cutoff_present_seq: u64,
) -> Result<(), FinalizeError> {
    // v3-v6 都收：v4 加 `storage.transform`，v5 加逐帧 health，v6 再加投影 scope 与
    // DSV pass 生产事实。
    // v3 的行缺 transform 证明是 identity
    // （那一版的 writer 只写原始 `d`）。收 v3 是为了让已采的数据还能取证，
    // 但它的精度问题另有一道门挡着——见下面对 traditional-Z + identity 的拒绝。
    if entry.schema != INDEX_SCHEMA_V3
        && entry.schema != INDEX_SCHEMA_V4
        && entry.schema != INDEX_SCHEMA_V5
        && entry.schema != INDEX_SCHEMA_V6
    {
        return Err(FinalizeError::Contract(format!(
            "present_seq {} uses unsupported index schema `{}`",
            entry.present_seq, entry.schema
        )));
    }
    if entry.present_seq > cutoff_present_seq {
        return Err(FinalizeError::OutOfBounds(format!(
            "present_seq {} exceeds flushed cutoff {}",
            entry.present_seq, cutoff_present_seq
        )));
    }
    if entry.source_frame != entry.present_seq {
        return Err(FinalizeError::Contract(format!(
            "probe present_seq {} has mismatched source_frame {}",
            entry.present_seq, entry.source_frame
        )));
    }
    if entry.depth_name.is_some() {
        return Err(FinalizeError::Contract(format!(
            "probe index present_seq {} must not invent delivery depth_name",
            entry.present_seq
        )));
    }
    if entry.width == 0 || entry.height == 0 || entry.dsv_id.trim().is_empty() {
        return Err(FinalizeError::Contract(format!(
            "present_seq {} has incomplete geometry/DSV identity",
            entry.present_seq
        )));
    }
    if entry.storage.encoding != "exr-half-zip"
        || entry.storage.domain != "raw_buffer"
        || entry.storage.channel != crate::EXR_CHANNEL_NAME
    {
        return Err(FinalizeError::Contract(format!(
            "present_seq {} has unexpected probe storage {:?}",
            entry.present_seq, entry.storage
        )));
    }
    // v4 缺 transform 在这里就要拒，不能等到读像素——那时已经读了几千个文件。
    let transform = entry.resolve_transform().map_err(FinalizeError::Contract)?;
    // 同理，v5 缺失或自相矛盾的 health 证据先拒；实际 stored 统计在读 EXR 后再复核。
    entry.resolve_health().map_err(FinalizeError::Contract)?;
    entry.resolve_pass().map_err(FinalizeError::Contract)?;
    // 传统 Z 存缓冲原值 = 精度已经被 fp16 毁掉，收口没有意义。
    //
    // **只在约定已知时挡**（`reverse_z == Some(false)`）：未知时我们分不出
    // 「传统 Z 存错了」和「reversed-Z 存对了」，而拒绝一份其实没问题的数据
    // 与放行一份废数据一样糟。约定未知的那批另有 proj_source=unknown 挡着。
    if entry.projection.reverse_z == Some(false) && transform == StorageTransform::Identity {
        return Err(FinalizeError::Contract(format!(
            "present_seq {} 是传统 Z 却按 identity 落盘：fp16 存 `d` 在 20 m 处只剩 \
             0.95 m 分辨率（94 号）。这批数据无法交付，需按逆深度重采",
            entry.present_seq
        )));
    }
    let expected = PathBuf::from("raw").join(format!("{:012}.exr", entry.present_seq));
    let actual = Path::new(&entry.path);
    if actual != expected
        || actual.is_absolute()
        || !actual
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
    {
        return Err(FinalizeError::OutOfBounds(format!(
            "present_seq {} has unsafe/noncanonical path `{}` under {}",
            entry.present_seq,
            entry.path,
            probe_root.display()
        )));
    }
    Ok(())
}

fn join_index_to_mapping(
    indexed: Vec<IndexedEntry>,
    mapping: &BTreeMap<u64, Vec<PairTarget>>,
    boundaries: &BTreeMap<u32, SegmentBoundary>,
    frame_validity: &mut BTreeMap<u64, FrameValidityRecord>,
) -> Result<BTreeMap<u32, Vec<MappedEntry>>, FinalizeError> {
    let index_sequences: BTreeSet<_> = indexed.iter().map(|row| row.entry.present_seq).collect();
    for seq in mapping.keys() {
        if !index_sequences.contains(seq) {
            return Err(FinalizeError::MissingIndex(*seq));
        }
    }
    let mut grouped: BTreeMap<u32, Vec<MappedEntry>> = BTreeMap::new();
    for indexed in indexed {
        let entry = indexed.entry;
        let Some(targets) = mapping.get(&entry.present_seq) else {
            // selected-present 会为所有 distinct RGB source 只回读一次；只落在奇数
            // session_frame 的 source 不属于 15 FPS 交付集，保留在 probe evidence 中即可。
            continue;
        };
        for target in targets {
            if entry.dsv_id != target.dsv_id {
                return Err(FinalizeError::Integrity(format!(
                    "present_seq {} DSV identity differs: index `{}`, pair `{}`",
                    entry.present_seq, entry.dsv_id, target.dsv_id
                )));
            }
            if !boundaries.contains_key(&target.segment_index) {
                return Err(FinalizeError::OutOfBounds(format!(
                    "present_seq {} resolved to unknown segment {}",
                    entry.present_seq, target.segment_index
                )));
            }
            let record = frame_validity
                .get_mut(&target.session_frame)
                .ok_or_else(|| {
                    FinalizeError::Contract(format!(
                        "present_seq {} resolved to missing frame validity row",
                        entry.present_seq
                    ))
                })?;
            if matches!(indexed.content, FrameContent::Blank { .. }) {
                record
                    .invalid_reasons
                    .push(FrameInvalidReason::AllSameDepth);
            }
            for defect in &indexed.health_defects {
                let reason = match defect {
                    FrameDefect::RawAllNonFinite | FrameDefect::StoredAllNonFinite => {
                        FrameInvalidReason::AllDepthNonFinite
                    }
                    FrameDefect::RawUniform | FrameDefect::StoredUniform => {
                        FrameInvalidReason::AllSameDepth
                    }
                    FrameDefect::StoredQuantizationCollapse { .. } => {
                        FrameInvalidReason::StorageQuantizationCollapse
                    }
                };
                record.invalid_reasons.push(reason);
            }
            if let (Some(declared_reverse), Some(observed)) =
                (entry.projection.reverse_z, indexed.observed_z)
                && declared_reverse != (observed == crate::decode::ZConvention::Reversed)
            {
                record
                    .invalid_reasons
                    .push(FrameInvalidReason::ZConventionMismatch);
            }
            record.invalid_reasons.sort_unstable();
            record.invalid_reasons.dedup();
            grouped
                .entry(target.segment_index)
                .or_default()
                .push(MappedEntry {
                    entry: entry.clone(),
                    source_path: indexed.source_path.clone(),
                    session_frame: target.session_frame,
                    segment_index: target.segment_index,
                    segment_frame: target.segment_frame,
                    capture_state: record.capture_state,
                    invalid_reasons: record.invalid_reasons.clone(),
                    source_size: indexed.source_size,
                    source_sha256: indexed.source_sha256.clone(),
                });
        }
    }
    for entries in grouped.values_mut() {
        entries.sort_by_key(|entry| entry.session_frame);
    }
    for segment_index in boundaries.keys() {
        if !grouped.contains_key(segment_index) {
            return Err(FinalizeError::Contract(format!(
                "segment {segment_index} has no complete depth pair"
            )));
        }
    }
    Ok(grouped)
}

fn stage_segment(
    staging_dir: &Path,
    identity: &StreamIdentity,
    archive_lane: ArchiveLane,
    boundary: SegmentBoundary,
    entries: &[MappedEntry],
    frame_validity: Vec<FrameValidityRecord>,
    pairing_coverage: f64,
) -> Result<(), FinalizeError> {
    let raw_dir = staging_dir.join("raw");
    fs::create_dir_all(&raw_dir).map_err(io_error)?;
    let mut objects = Vec::with_capacity(entries.len() + 1);
    let mut staged_sources = BTreeSet::new();
    let mut index_bytes = Vec::new();
    for mapped in entries {
        let name = format!("{:012}.exr", mapped.entry.present_seq);
        if staged_sources.insert(mapped.entry.present_seq) {
            let destination = raw_dir.join(&name);
            let (size, sha256) = copy_file_verified(
                &mapped.source_path,
                &destination,
                mapped.source_size,
                &mapped.source_sha256,
            )?;
            objects.push(SegmentObject {
                kind: "depth_exr".into(),
                relative_path: format!("raw/{name}"),
                size,
                sha256: sha256.clone(),
            });
        }
        let mut entry = mapped.entry.clone();
        entry.path = format!("raw/{name}");
        let row = SegmentIndexRow {
            entry: &entry,
            session_frame: mapped.session_frame,
            segment_index: mapped.segment_index,
            segment_frame: mapped.segment_frame,
            capture_state: mapped.capture_state,
            invalid_reasons: &mapped.invalid_reasons,
        };
        serde_json::to_writer(&mut index_bytes, &row).map_err(|error| {
            FinalizeError::Contract(format!("serialize segment index: {error}"))
        })?;
        index_bytes.push(b'\n');
    }
    let index_path = staging_dir.join(SEGMENT_INDEX_NAME);
    write_new_synced(&index_path, &index_bytes)?;
    let index_size = fs::metadata(&index_path).map_err(io_error)?.len();
    let index_sha256 = sha256_file(&index_path)?;
    objects.insert(
        0,
        SegmentObject {
            kind: "depth_index".into(),
            relative_path: SEGMENT_INDEX_NAME.into(),
            size: index_size,
            sha256: index_sha256,
        },
    );

    let invalid_frame_count = frame_validity
        .iter()
        .filter(|record| !record.invalid_reasons.is_empty())
        .count();
    let candidate_eligible = invalid_frame_count == 0
        && frame_validity
            .iter()
            .all(|record| record.capture_state.is_gameplay());
    let manifest = SegmentManifest {
        schema: SEGMENT_MANIFEST_SCHEMA.into(),
        identity: identity.clone(),
        segment_index: boundary.segment_index,
        session_frame_start: boundary.session_frame_start,
        frame_count: boundary.frame_count,
        first_present_seq: entries.first().expect("non-empty group").entry.present_seq,
        last_present_seq: entries.last().expect("non-empty group").entry.present_seq,
        // JSON 浮点的跨实现规范化边界留给 #524 适配器；本地 manifest 用固定 9 位小数。
        pairing_coverage: format!("{pairing_coverage:.9}"),
        candidate_eligible,
        invalid_frame_count,
        frame_validity,
        archive_adapter: candidate_eligible
            .then(|| archive_adapter(identity, archive_lane))
            .transpose()?,
        objects,
    };
    let manifest_bytes = serde_json::to_vec_pretty(&manifest)
        .map_err(|error| FinalizeError::Contract(format!("serialize segment manifest: {error}")))?;
    write_new_synced(&staging_dir.join(SEGMENT_MANIFEST_NAME), &manifest_bytes)?;
    validate_published(staging_dir, identity, archive_lane, boundary)?;
    Ok(())
}

fn validate_published(
    directory: &Path,
    identity: &StreamIdentity,
    archive_lane: ArchiveLane,
    boundary: SegmentBoundary,
) -> Result<PublishedDepthSegment, FinalizeError> {
    let manifest_path = directory.join(SEGMENT_MANIFEST_NAME);
    let bytes = fs::read(&manifest_path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            FinalizeError::FileMissing(manifest_path.clone())
        } else {
            FinalizeError::Io(format!("read {}: {error}", manifest_path.display()))
        }
    })?;
    let manifest: SegmentManifest = serde_json::from_slice(&bytes).map_err(|error| {
        FinalizeError::Integrity(format!("parse {}: {error}", manifest_path.display()))
    })?;
    if manifest.schema != SEGMENT_MANIFEST_SCHEMA
        || &manifest.identity != identity
        || manifest.segment_index != boundary.segment_index
        || manifest.session_frame_start != boundary.session_frame_start
        || manifest.frame_count != boundary.frame_count
        || manifest.frame_validity.len() as u64 != boundary.frame_count
        || manifest
            .frame_validity
            .iter()
            .enumerate()
            .any(|(offset, record)| {
                record.session_frame != boundary.session_frame_start + offset as u64
            })
        || manifest.invalid_frame_count
            != manifest
                .frame_validity
                .iter()
                .filter(|record| !record.invalid_reasons.is_empty())
                .count()
        || manifest.candidate_eligible
            != (manifest.invalid_frame_count == 0
                && manifest
                    .frame_validity
                    .iter()
                    .all(|record| record.capture_state.is_gameplay()))
        || manifest.objects.is_empty()
        || manifest.objects[0].kind != "depth_index"
        || manifest.archive_adapter
            != manifest
                .candidate_eligible
                .then(|| archive_adapter(identity, archive_lane))
                .transpose()?
    {
        return Err(FinalizeError::Integrity(format!(
            "manifest identity/boundary mismatch at {}",
            manifest_path.display()
        )));
    }
    let mut paths = BTreeSet::new();
    for object in &manifest.objects {
        let relative = Path::new(&object.relative_path);
        if relative.is_absolute()
            || !relative
                .components()
                .all(|part| matches!(part, Component::Normal(_)))
            || !paths.insert(object.relative_path.as_str())
        {
            return Err(FinalizeError::Integrity(format!(
                "manifest has unsafe/duplicate object path `{}`",
                object.relative_path
            )));
        }
        let path = directory.join(relative);
        let metadata = fs::symlink_metadata(&path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                FinalizeError::FileMissing(path.clone())
            } else {
                FinalizeError::Io(format!("stat {}: {error}", path.display()))
            }
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(FinalizeError::Integrity(format!(
                "manifest object {} is not a regular file",
                path.display()
            )));
        }
        if metadata.len() != object.size {
            return Err(FinalizeError::Integrity(format!(
                "size mismatch for {}: expected {}, got {}",
                path.display(),
                object.size,
                metadata.len()
            )));
        }
        let actual = sha256_file(&path)?;
        if actual != object.sha256 {
            return Err(FinalizeError::Integrity(format!(
                "SHA-256 mismatch for {}: expected {}, got {}",
                path.display(),
                object.sha256,
                actual
            )));
        }
    }
    Ok(PublishedDepthSegment {
        segment_index: boundary.segment_index,
        manifest_sha256: sha256_bytes(&bytes),
        manifest_path,
        object_count: manifest.objects.len(),
        candidate_eligible: manifest.candidate_eligible,
        invalid_frame_count: manifest.invalid_frame_count,
        archive_manifest_object_key: manifest
            .archive_adapter
            .as_ref()
            .map(|adapter| adapter.manifest_object_key.clone()),
        archive_run_index_object_key: manifest
            .archive_adapter
            .as_ref()
            .map(|adapter| adapter.run_index_object_key.clone()),
    })
}

fn archive_adapter(
    identity: &StreamIdentity,
    lane: ArchiveLane,
) -> Result<ArchiveAdapter, FinalizeError> {
    validate_target(ARCHIVE_BUCKET, ARCHIVE_RETENTION_DAYS).map_err(|violations| {
        FinalizeError::Contract(format!("formal archive target is invalid: {violations:?}"))
    })?;
    let manifest_object_key = manifest_object_key(lane, &identity.session_id).map_err(|error| {
        FinalizeError::Contract(format!("formal archive session key is invalid: {error:?}"))
    })?;
    let run_index_relative = format!("runs/{}/depth-index.jsonl.zst", identity.run_id);
    let run_index_object_key = object_key(lane, &identity.session_id, &run_index_relative)
        .map_err(|error| {
            FinalizeError::Contract(format!(
                "formal archive run index key is invalid: {error:?}"
            ))
        })?;
    Ok(ArchiveAdapter {
        schema_version: ARCHIVE_SCHEMA_VERSION.into(),
        lane,
        bucket: ARCHIVE_BUCKET.into(),
        retention_days: ARCHIVE_RETENTION_DAYS,
        manifest_object_key,
        run_index_object_key,
    })
}

fn copy_file_verified(
    source: &Path,
    destination: &Path,
    expected_size: u64,
    expected_sha256: &str,
) -> Result<(u64, String), FinalizeError> {
    let before = fs::metadata(source).map_err(io_error)?;
    if before.len() != expected_size {
        return Err(FinalizeError::Integrity(format!(
            "source size changed before staging {}",
            source.display()
        )));
    }
    let mut input = File::open(source).map_err(io_error)?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .map_err(io_error)?;
    let mut hasher = Sha256::new();
    let mut total = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = input.read(&mut buffer).map_err(io_error)?;
        if read == 0 {
            break;
        }
        output.write_all(&buffer[..read]).map_err(io_error)?;
        hasher.update(&buffer[..read]);
        total = total
            .checked_add(read as u64)
            .ok_or_else(|| FinalizeError::Integrity("EXR size overflow".into()))?;
    }
    output.sync_all().map_err(io_error)?;
    let after = fs::metadata(source).map_err(io_error)?;
    if before.len() != after.len() || total != before.len() {
        return Err(FinalizeError::Integrity(format!(
            "source size changed while copying {}",
            source.display()
        )));
    }
    let expected = format_digest(hasher.finalize());
    if expected != expected_sha256 {
        return Err(FinalizeError::Integrity(format!(
            "source hash changed after content classification {}",
            source.display()
        )));
    }
    let destination_size = fs::metadata(destination).map_err(io_error)?.len();
    let destination_hash = sha256_file(destination)?;
    if destination_size != total || destination_hash != expected {
        return Err(FinalizeError::Integrity(format!(
            "staged copy size/hash mismatch for {}",
            destination.display()
        )));
    }
    Ok((total, expected))
}

fn write_new_synced(path: &Path, bytes: &[u8]) -> Result<(), FinalizeError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(io_error)?;
    file.write_all(bytes).map_err(io_error)?;
    file.sync_all().map_err(io_error)
}

fn ensure_real_directory(path: &Path) -> Result<(), FinalizeError> {
    let metadata = fs::symlink_metadata(path).map_err(io_error)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(FinalizeError::Integrity(format!(
            "{} is not a real directory",
            path.display()
        )));
    }
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String, FinalizeError> {
    let mut file = File::open(path).map_err(io_error)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(io_error)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format_digest(hasher.finalize()))
}

fn sha256_bytes(bytes: &[u8]) -> String {
    format_digest(Sha256::digest(bytes))
}

fn format_digest(bytes: impl AsRef<[u8]>) -> String {
    let mut output = String::with_capacity(71);
    output.push_str("sha256:");
    for byte in bytes.as_ref() {
        use std::fmt::Write as _;
        write!(&mut output, "{byte:02x}").expect("writing to String cannot fail");
    }
    output
}

fn io_error(error: std::io::Error) -> FinalizeError {
    FinalizeError::Io(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture_timing::{DepthStorage, IndexProjection, StorageTransform};
    use crate::decode::DepthFormat;
    use crate::frame_health::{FRAME_HEALTH_SCHEMA_V1, FrameHealthEvidence};
    use crate::pairing::{CameraMode, PairedDepth, PairingReport};
    use crate::pipeline::{E2eReport, WatermarkStats};

    fn identity(generation: u64) -> StreamIdentity {
        StreamIdentity::new("session-a", "r0000", generation).unwrap()
    }

    fn boundary(index: u32, start: u64) -> SegmentBoundary {
        boundary_with_count(index, start, 4)
    }

    fn boundary_with_count(index: u32, start: u64, frame_count: u64) -> SegmentBoundary {
        SegmentBoundary {
            segment_index: index,
            session_frame_start: start,
            frame_count,
        }
    }

    fn pair(session_frame: u64, present_seq: u64) -> FramePair {
        FramePair {
            session_frame,
            // 这两个字段故意全填 0，复现当前实际 parser 的默认值；收口必须用 journal 边界。
            segment_index: 0,
            segment_frame: 0,
            source_qpc: session_frame as i64 * 1_000,
            duplicate: false,
            camera_mode: CameraMode::Player,
            present_seq: Some(present_seq),
            depth: Some(PairedDepth {
                present_seq,
                depth_name: format!("depth_{session_frame}"),
                dsv_id: "main-scene-v1".into(),
                inherited: false,
            }),
            depth_absent_reason: None,
        }
    }

    fn absent_pair(session_frame: u64, reason: AbsentReason) -> FramePair {
        FramePair {
            session_frame,
            segment_index: 0,
            segment_frame: 0,
            source_qpc: session_frame as i64 * 1_000,
            duplicate: false,
            camera_mode: CameraMode::Player,
            present_seq: None,
            depth: None,
            depth_absent_reason: Some(reason),
        }
    }

    fn evidence(identity: &StreamIdentity, pairs: Vec<FramePair>) -> VerifiedPairingEvidence {
        let total = pairs.len() as u64;
        let eligible = pairs
            .iter()
            .filter(|pair| {
                !pair.duplicate && pair.depth_absent_reason != Some(AbsentReason::OddFrameBySpec)
            })
            .count() as u64;
        let paired = pairs
            .iter()
            .filter(|pair| pair.depth.as_ref().is_some_and(|depth| !depth.inherited))
            .count() as u64;
        let inherited = pairs
            .iter()
            .filter(|pair| pair.depth.as_ref().is_some_and(|depth| depth.inherited))
            .count() as u64;
        VerifiedPairingEvidence::new(
            identity.clone(),
            pairs,
            E2eReport {
                pairing: PairingReport {
                    total_frames: total,
                    eligible_even_frames: eligible,
                    paired_even_frames: paired,
                    inherited_frames: inherited,
                    unreferenced_depth_frames: 0,
                    coverage: if eligible == 0 {
                        1.0
                    } else {
                        paired as f64 / eligible as f64
                    },
                },
                watermark: WatermarkStats {
                    decoded: total,
                    ..WatermarkStats::default()
                },
                passed: true,
                failures: vec![],
            },
        )
        .unwrap()
    }

    fn flush(identity: &StreamIdentity, cutoff: u64) -> ProbeFlushedEvidence {
        ProbeFlushedEvidence {
            schema: FLUSH_EVIDENCE_SCHEMA.into(),
            identity: identity.clone(),
            writer_queue_depth: 0,
            last_present_seq: cutoff,
            cutoff_present_seq: cutoff,
        }
    }

    fn frame_states(
        identity: &StreamIdentity,
        boundaries: &[SegmentBoundary],
        state: FrameCaptureState,
    ) -> FrameStateEvidence {
        FrameStateEvidence {
            schema: FRAME_STATE_EVIDENCE_SCHEMA.into(),
            identity: identity.clone(),
            boundaries: boundaries
                .iter()
                .map(|boundary| FrameStateBoundary {
                    session_frame_start: boundary.session_frame_start,
                    frame_count: boundary.frame_count,
                    state,
                    camera_coverage_proven: true,
                    action_coverage_proven: true,
                })
                .collect(),
        }
    }

    fn write_probe(root: &Path, sequences: &[u64]) {
        write_probe_with_values(root, sequences, &[0.0, 0.25, 0.5, 1.0]);
    }

    fn write_probe_with_values(root: &Path, sequences: &[u64], values: &[f32]) {
        fs::create_dir_all(root.join("raw")).unwrap();
        let mut index = String::new();
        for seq in sequences {
            let path = format!("raw/{seq:012}.exr");
            crate::exr_out::write_buffer_frame(&root.join(&path), values, 2, 2).unwrap();
            let row = DepthIndexEntry {
                schema: INDEX_SCHEMA_V3.into(),
                present_seq: *seq,
                source_frame: *seq,
                depth_name: None,
                width: 2,
                height: 2,
                format: DepthFormat::D24S8,
                dsv_id: "main-scene-v1".into(),
                storage: DepthStorage::raw_buffer_exr(StorageTransform::Identity),
                path,
                readback_latency_frames: 2,
                projection: IndexProjection::unknown(),
                health: None,
                pass: None,
            };
            index.push_str(&serde_json::to_string(&row).unwrap());
            index.push('\n');
        }
        fs::write(root.join(SEGMENT_INDEX_NAME), index).unwrap();
    }

    fn write_v5_probe(
        root: &Path,
        present_seq: u64,
        width: usize,
        height: usize,
        raw: &[f32],
        transform: StorageTransform,
    ) {
        fs::create_dir_all(root.join("raw")).unwrap();
        let relative_path = format!("raw/{present_seq:012}.exr");
        let stored: Vec<f32> = raw.iter().map(|&value| transform.apply(value)).collect();
        crate::exr_out::write_buffer_frame(&root.join(&relative_path), &stored, width, height)
            .unwrap();
        let entry = DepthIndexEntry {
            schema: INDEX_SCHEMA_V5.into(),
            present_seq,
            source_frame: present_seq,
            depth_name: None,
            width: width as u32,
            height: height as u32,
            format: DepthFormat::D24S8,
            dsv_id: "main-scene-v1".into(),
            storage: DepthStorage::raw_buffer_exr(transform),
            path: relative_path,
            readback_latency_frames: 2,
            projection: IndexProjection::unknown(),
            health: Some(FrameHealthEvidence::inspect(raw, width, transform)),
            pass: None,
        };
        fs::write(
            root.join(SEGMENT_INDEX_NAME),
            format!("{}\n", serde_json::to_string(&entry).unwrap()),
        )
        .unwrap();
    }

    fn request<'a>(
        identity: &'a StreamIdentity,
        flushed: &'a ProbeFlushedEvidence,
        pairing: &'a VerifiedPairingEvidence,
        frame_states: &'a FrameStateEvidence,
        boundaries: &'a [SegmentBoundary],
        probe_root: &'a Path,
        output_root: &'a Path,
    ) -> FinalizeRequest<'a> {
        FinalizeRequest {
            identity,
            archive_lane: ArchiveLane::Lane30,
            flushed,
            pairing,
            frame_states,
            boundaries,
            probe_root,
            output_root,
        }
    }

    #[test]
    fn finalizes_multiple_segments_from_verified_session_frame_boundaries() {
        let temp = tempfile::tempdir().unwrap();
        let probe = temp.path().join("probe");
        let output = temp.path().join("depth-segments");
        write_probe(&probe, &[10, 12, 65_534, 65_536]);
        let id = identity(7);
        let pairing = evidence(
            &id,
            vec![
                pair(0, 10),
                absent_pair(1, AbsentReason::OddFrameBySpec),
                pair(2, 12),
                absent_pair(3, AbsentReason::OddFrameBySpec),
                pair(4, 65_534),
                absent_pair(5, AbsentReason::OddFrameBySpec),
                pair(6, 65_536),
                absent_pair(7, AbsentReason::OddFrameBySpec),
            ],
        );
        let flushed = flush(&id, 65_536);
        let boundaries = [boundary(0, 0), boundary(1, 4)];
        let states = frame_states(&id, &boundaries, FrameCaptureState::Gameplay);
        let published = finalize_segments(&request(
            &id,
            &flushed,
            &pairing,
            &states,
            &boundaries,
            &probe,
            &output,
        ))
        .unwrap();

        assert_eq!(published.len(), 2);
        assert_eq!(
            published[0].archive_manifest_object_key.as_deref(),
            Some("depth-lab/v1/lane-30/sessions/session-a/session/depth-manifest.json")
        );
        assert_eq!(
            published[0].archive_run_index_object_key.as_deref(),
            Some("depth-lab/v1/lane-30/sessions/session-a/runs/r0000/depth-index.jsonl.zst")
        );
        assert!(output.join("00000/depth-segment.json").is_file());
        assert!(output.join("00001/raw/000000065536.exr").is_file());
        assert!(published.iter().all(|segment| segment.candidate_eligible));
        assert!(
            published
                .iter()
                .all(|segment| segment.invalid_frame_count == 0)
        );
        for (expected_index, segment) in published.iter().enumerate() {
            let verified = verify_candidate_segment(
                &segment.manifest_path,
                &id,
                expected_index as u32,
                &segment.manifest_sha256,
            )
            .unwrap();
            assert_eq!(verified.segment_index, expected_index as u32);
            assert_eq!(verified.frames.len(), 2);
            assert!(
                verified
                    .frames
                    .iter()
                    .all(|frame| frame.source_path.is_file())
            );
        }
        let second = fs::read_to_string(output.join("00001/depth-index.jsonl")).unwrap();
        let rows: Vec<serde_json::Value> = second
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(rows[0]["session_frame"], 4);
        assert_eq!(rows[0]["segment_index"], 1);
        assert_eq!(rows[1]["segment_frame"], 2);
        assert_eq!(rows[1]["present_seq"], 65_536);
    }

    #[test]
    fn blank_non_gameplay_and_unknown_frames_are_preserved_but_never_candidates() {
        let temp = tempfile::tempdir().unwrap();
        let probe = temp.path().join("probe");
        let output = temp.path().join("out");
        write_probe(&probe, &[10, 12, 14]);
        // 通用 classify_frame 判「全同值」，不假设 clear 一定是 0.0 或 1.0。
        crate::exr_out::write_buffer_frame(&probe.join("raw/000000000010.exr"), &[0.375; 4], 2, 2)
            .unwrap();
        let id = identity(11);
        let pairing = evidence(&id, vec![pair(0, 10), pair(1, 12), pair(2, 14)]);
        let flushed = flush(&id, 14);
        let boundaries = [
            boundary_with_count(0, 0, 1),
            boundary_with_count(1, 1, 1),
            boundary_with_count(2, 2, 1),
        ];
        let states = FrameStateEvidence {
            schema: FRAME_STATE_EVIDENCE_SCHEMA.into(),
            identity: id.clone(),
            boundaries: vec![
                FrameStateBoundary {
                    session_frame_start: 0,
                    frame_count: 1,
                    state: FrameCaptureState::Gameplay,
                    camera_coverage_proven: true,
                    action_coverage_proven: true,
                },
                FrameStateBoundary {
                    session_frame_start: 1,
                    frame_count: 1,
                    state: FrameCaptureState::Loading,
                    camera_coverage_proven: true,
                    action_coverage_proven: true,
                },
                // session_frame=2 故意不提供状态证明：孤立未知帧必须不可交付。
            ],
        };
        let published = finalize_segments(&request(
            &id,
            &flushed,
            &pairing,
            &states,
            &boundaries,
            &probe,
            &output,
        ))
        .unwrap();

        assert_eq!(
            published.len(),
            3,
            "invalid frame must not be silently deleted"
        );
        assert!(published.iter().all(|segment| !segment.candidate_eligible));
        assert!(
            published
                .iter()
                .all(|segment| segment.archive_manifest_object_key.is_none())
        );
        for (segment, reason) in [
            (0, "all_same_depth"),
            (1, "non_gameplay_state"),
            (2, "state_unverified"),
        ] {
            assert!(
                output
                    .join(format!("{segment:05}/raw/{:012}.exr", 10 + segment * 2))
                    .is_file()
            );
            let index =
                fs::read_to_string(output.join(format!("{segment:05}/{SEGMENT_INDEX_NAME}")))
                    .unwrap();
            let row: serde_json::Value = serde_json::from_str(index.trim()).unwrap();
            assert_eq!(row["invalid_reasons"][0], reason);
        }
        let unknown_row: serde_json::Value = serde_json::from_str(
            fs::read_to_string(output.join("00002/depth-index.jsonl"))
                .unwrap()
                .trim(),
        )
        .unwrap();
        let unknown_reasons = unknown_row["invalid_reasons"].as_array().unwrap();
        assert!(unknown_reasons.contains(&serde_json::json!("state_unverified")));
        assert!(unknown_reasons.contains(&serde_json::json!("camera_coverage_missing")));
        assert!(unknown_reasons.contains(&serde_json::json!("action_coverage_missing")));
    }

    #[test]
    fn camera_and_action_coverage_are_required_independently() {
        let temp = tempfile::tempdir().unwrap();
        let probe = temp.path().join("probe");
        let output = temp.path().join("out");
        write_probe(&probe, &[0, 2, 4]);
        let id = identity(12);
        let mut freecam_pair = pair(2, 4);
        freecam_pair.camera_mode = CameraMode::Freecam;
        let pairing = evidence(&id, vec![pair(0, 0), pair(1, 2), freecam_pair]);
        let flushed = flush(&id, 4);
        let boundaries = [
            boundary_with_count(0, 0, 1),
            boundary_with_count(1, 1, 1),
            boundary_with_count(2, 2, 1),
        ];
        let states = FrameStateEvidence {
            schema: FRAME_STATE_EVIDENCE_SCHEMA.into(),
            identity: id.clone(),
            boundaries: vec![
                FrameStateBoundary {
                    session_frame_start: 0,
                    frame_count: 1,
                    state: FrameCaptureState::Gameplay,
                    camera_coverage_proven: false,
                    action_coverage_proven: true,
                },
                FrameStateBoundary {
                    session_frame_start: 1,
                    frame_count: 1,
                    state: FrameCaptureState::Gameplay,
                    camera_coverage_proven: true,
                    action_coverage_proven: false,
                },
                FrameStateBoundary {
                    session_frame_start: 2,
                    frame_count: 1,
                    state: FrameCaptureState::Gameplay,
                    camera_coverage_proven: true,
                    action_coverage_proven: true,
                },
            ],
        };
        let published = finalize_segments(&request(
            &id,
            &flushed,
            &pairing,
            &states,
            &boundaries,
            &probe,
            &output,
        ))
        .unwrap();
        assert!(published.iter().all(|segment| !segment.candidate_eligible));
        for (segment, reason) in [
            (0, "camera_coverage_missing"),
            (1, "action_coverage_missing"),
            (2, "camera_mode_not_player"),
        ] {
            let index =
                fs::read_to_string(output.join(format!("{segment:05}/depth-index.jsonl"))).unwrap();
            let row: serde_json::Value = serde_json::from_str(index.trim()).unwrap();
            assert!(
                row["invalid_reasons"]
                    .as_array()
                    .unwrap()
                    .contains(&serde_json::json!(reason))
            );
        }
    }

    #[test]
    fn diagnostic_pass_and_long_blank_run_stay_unknown_without_explicit_state_proof() {
        let temp = tempfile::tempdir().unwrap();
        let probe = temp.path().join("probe");
        let output = temp.path().join("out");
        let sequences = (0..16).map(|index| index * 2).collect::<Vec<_>>();
        write_probe_with_values(&probe, &sequences, &[0.625; 4]);
        let id = identity(13);
        let pairs = sequences
            .iter()
            .enumerate()
            .map(|(session_frame, present_seq)| pair(session_frame as u64, *present_seq))
            .collect();
        let pairing = evidence(&id, pairs);
        assert!(pairing.report().passed);
        let flushed = flush(&id, *sequences.last().unwrap());
        let boundaries = [boundary_with_count(0, 0, sequences.len() as u64)];
        // 不采用诊断层的「数组连续 >=15 行」推定；没有显式状态证明就是 Unknown。
        let states = FrameStateEvidence {
            schema: FRAME_STATE_EVIDENCE_SCHEMA.into(),
            identity: id.clone(),
            boundaries: vec![],
        };
        let published = finalize_segments(&request(
            &id,
            &flushed,
            &pairing,
            &states,
            &boundaries,
            &probe,
            &output,
        ))
        .unwrap();
        assert_eq!(published.len(), 1);
        assert!(!published[0].candidate_eligible);
        assert_eq!(published[0].invalid_frame_count, sequences.len());
        assert_eq!(
            fs::read_to_string(output.join("00000/depth-index.jsonl"))
                .unwrap()
                .lines()
                .count(),
            sequences.len(),
            "long invalid runs are audited in place, not deleted or stitched"
        );
    }

    #[test]
    fn observed_z_mismatch_marks_gameplay_frame_non_deliverable() {
        let temp = tempfile::tempdir().unwrap();
        let probe = temp.path().join("probe");
        let output = temp.path().join("out");
        // 盘上是 `1−d`，所以缓冲值是 [0.0, 0.0, 0.25, 0.5]——清除值 0.0 ⇒ reversed，
        // 而下面声明的 profile 说 traditional。**先还原再判**才看得出这个矛盾；
        // 直接拿盘上的字节判会得出 traditional，与声明一致，静默放过。
        write_probe_with_values(&probe, &[0], &[1.0, 1.0, 0.75, 0.5]);
        let index_path = probe.join(SEGMENT_INDEX_NAME);
        let mut entry: DepthIndexEntry =
            serde_json::from_str(fs::read_to_string(&index_path).unwrap().trim()).unwrap();
        entry.projection = IndexProjection::known(
            crate::depth::Projection::traditional_z(0.1, 100.0, 1.0),
            Some(1.0),
            crate::capture_timing::IndexProjectionSource::EngineDefault,
        );
        entry.storage.transform = Some(StorageTransform::OneMinusD);
        fs::write(
            &index_path,
            format!("{}\n", serde_json::to_string(&entry).unwrap()),
        )
        .unwrap();
        let id = identity(14);
        let pairing = evidence(&id, vec![pair(0, 0)]);
        let flushed = flush(&id, 0);
        let boundaries = [boundary_with_count(0, 0, 1)];
        let states = frame_states(&id, &boundaries, FrameCaptureState::Gameplay);
        let published = finalize_segments(&request(
            &id,
            &flushed,
            &pairing,
            &states,
            &boundaries,
            &probe,
            &output,
        ))
        .unwrap();
        assert!(!published[0].candidate_eligible);
        let index = fs::read_to_string(output.join("00000/depth-index.jsonl")).unwrap();
        let row: serde_json::Value = serde_json::from_str(index.trim()).unwrap();
        assert!(
            row["invalid_reasons"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!("z_convention_mismatch"))
        );
    }

    /// 传统 Z 存缓冲原值就是 94 号判死的那种数据：fp16 之后 20 m 处只剩
    /// 0.95 m 分辨率。收口必须拒，而且要在读像素**之前**拒——
    /// 那时可能已经要读几千个文件。
    #[test]
    fn a_traditional_z_capture_stored_as_raw_d_is_refused_at_finalize() {
        let temp = tempfile::tempdir().unwrap();
        let probe = temp.path().join("probe");
        let output = temp.path().join("out");
        write_probe_with_values(&probe, &[0], &[1.0, 1.0, 0.9, 0.8]);
        let index_path = probe.join(SEGMENT_INDEX_NAME);
        let mut entry: DepthIndexEntry =
            serde_json::from_str(fs::read_to_string(&index_path).unwrap().trim()).unwrap();
        entry.projection = IndexProjection::known(
            crate::depth::Projection::traditional_z(0.1, 100.0, 1.0),
            Some(1.0),
            crate::capture_timing::IndexProjectionSource::EngineDefault,
        );
        entry.storage.transform = Some(StorageTransform::Identity);
        fs::write(
            &index_path,
            format!("{}\n", serde_json::to_string(&entry).unwrap()),
        )
        .unwrap();
        let id = identity(15);
        let boundaries = [boundary_with_count(0, 0, 1)];
        let err = finalize_segments(&request(
            &id,
            &flush(&id, 0),
            &evidence(&id, vec![pair(0, 0)]),
            &frame_states(&id, &boundaries, FrameCaptureState::Gameplay),
            &boundaries,
            &probe,
            &output,
        ))
        .unwrap_err();
        assert!(
            matches!(&err, FinalizeError::Contract(m) if m.contains("传统 Z") && m.contains("94")),
            "错误要说清楚是哪条规矩：{err:?}"
        );
    }

    /// 但约定未知时不拒：那时分不出「传统 Z 存错了」和「reversed-Z 存对了」，
    /// 而拒绝一份其实没问题的数据与放行一份废数据一样糟。
    #[test]
    fn identity_storage_is_allowed_when_the_convention_is_unknown() {
        let temp = tempfile::tempdir().unwrap();
        let probe = temp.path().join("probe");
        let output = temp.path().join("out");
        // 默认夹具就是 projection=unknown + identity
        write_probe(&probe, &[0]);
        let id = identity(16);
        let boundaries = [boundary_with_count(0, 0, 1)];
        assert!(
            finalize_segments(&request(
                &id,
                &flush(&id, 0),
                &evidence(&id, vec![pair(0, 0)]),
                &frame_states(&id, &boundaries, FrameCaptureState::Gameplay),
                &boundaries,
                &probe,
                &output,
            ))
            .is_ok()
        );
    }

    /// v4 却缺 transform：盘上做没做过变换无从判断，必须拒而不是当 identity。
    #[test]
    fn a_v4_row_missing_its_transform_is_refused_at_finalize() {
        let temp = tempfile::tempdir().unwrap();
        let probe = temp.path().join("probe");
        let output = temp.path().join("out");
        write_probe(&probe, &[0]);
        let index_path = probe.join(SEGMENT_INDEX_NAME);
        let mut entry: DepthIndexEntry =
            serde_json::from_str(fs::read_to_string(&index_path).unwrap().trim()).unwrap();
        entry.schema = INDEX_SCHEMA_V4.into();
        entry.storage.transform = None;
        fs::write(
            &index_path,
            format!("{}\n", serde_json::to_string(&entry).unwrap()),
        )
        .unwrap();
        let id = identity(17);
        let boundaries = [boundary_with_count(0, 0, 1)];
        let err = finalize_segments(&request(
            &id,
            &flush(&id, 0),
            &evidence(&id, vec![pair(0, 0)]),
            &frame_states(&id, &boundaries, FrameCaptureState::Gameplay),
            &boundaries,
            &probe,
            &output,
        ))
        .unwrap_err();
        assert!(
            matches!(&err, FinalizeError::Contract(m) if m.contains("transform")),
            "{err:?}"
        );
    }

    #[test]
    fn v5_quantization_collapse_is_durable_and_blocks_the_candidate() {
        let temp = tempfile::tempdir().unwrap();
        let probe = temp.path().join("probe");
        let output = temp.path().join("out");
        let (width, height) = (1280usize, 720usize);
        let pixels = width * height;
        let near = 0.2143f32;
        let raw: Vec<f32> = (0..pixels)
            .map(|index| {
                let t = index as f32 / (pixels - 1) as f32;
                let z = 2.0 + 198.0 * t;
                1.0 - near / z
            })
            .collect();
        // Deliberately reproduce the obsolete #543 storage path: traditional-Z raw d on fp16.
        // Projection stays unknown so the old convention gate cannot hide the new health gate.
        write_v5_probe(&probe, 0, width, height, &raw, StorageTransform::Identity);

        let id = identity(18);
        let boundaries = [boundary_with_count(0, 0, 1)];
        let published = finalize_segments(&request(
            &id,
            &flush(&id, 0),
            &evidence(&id, vec![pair(0, 0)]),
            &frame_states(&id, &boundaries, FrameCaptureState::Gameplay),
            &boundaries,
            &probe,
            &output,
        ))
        .unwrap();
        assert!(!published[0].candidate_eligible);
        let index = fs::read_to_string(output.join("00000/depth-index.jsonl")).unwrap();
        let row: serde_json::Value = serde_json::from_str(index.trim()).unwrap();
        assert!(
            row["invalid_reasons"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!("storage_quantization_collapse"))
        );
        assert_eq!(row["health"]["schema"], FRAME_HEALTH_SCHEMA_V1);
    }

    #[test]
    fn v5_health_is_checked_against_the_actual_exr() {
        let temp = tempfile::tempdir().unwrap();
        let probe = temp.path().join("probe");
        let output = temp.path().join("out");
        write_v5_probe(
            &probe,
            0,
            2,
            2,
            &[0.1, 0.2, 0.3, 0.4],
            StorageTransform::Identity,
        );
        // Corrupt the payload after the index health evidence was written.
        crate::exr_out::write_buffer_frame(&probe.join("raw/000000000000.exr"), &[0.0; 4], 2, 2)
            .unwrap();
        let id = identity(19);
        let boundaries = [boundary_with_count(0, 0, 1)];
        let error = finalize_segments(&request(
            &id,
            &flush(&id, 0),
            &evidence(&id, vec![pair(0, 0)]),
            &frame_states(&id, &boundaries, FrameCaptureState::Gameplay),
            &boundaries,
            &probe,
            &output,
        ))
        .unwrap_err();
        assert!(
            matches!(&error, FinalizeError::Integrity(message) if message.contains("stored frame health differs")),
            "{error:?}"
        );
    }

    #[test]
    fn a_v5_row_missing_health_is_refused_before_publication() {
        let temp = tempfile::tempdir().unwrap();
        let probe = temp.path().join("probe");
        let output = temp.path().join("out");
        write_probe(&probe, &[0]);
        let index_path = probe.join(SEGMENT_INDEX_NAME);
        let mut entry: DepthIndexEntry =
            serde_json::from_str(fs::read_to_string(&index_path).unwrap().trim()).unwrap();
        entry.schema = INDEX_SCHEMA_V5.into();
        entry.storage.transform = Some(StorageTransform::Identity);
        entry.health = None;
        fs::write(
            &index_path,
            format!("{}\n", serde_json::to_string(&entry).unwrap()),
        )
        .unwrap();
        let id = identity(20);
        let boundaries = [boundary_with_count(0, 0, 1)];
        let error = finalize_segments(&request(
            &id,
            &flush(&id, 0),
            &evidence(&id, vec![pair(0, 0)]),
            &frame_states(&id, &boundaries, FrameCaptureState::Gameplay),
            &boundaries,
            &probe,
            &output,
        ))
        .unwrap_err();
        assert!(
            matches!(&error, FinalizeError::Contract(message) if message.contains("health")),
            "{error:?}"
        );
    }

    #[test]
    fn an_odd_present_seq_is_not_a_health_failure_when_pairing_proves_it() {
        let temp = tempfile::tempdir().unwrap();
        let probe = temp.path().join("probe");
        let output = temp.path().join("out");
        write_v5_probe(
            &probe,
            101,
            2,
            2,
            &[0.1, 0.2, 0.3, 0.4],
            StorageTransform::Identity,
        );
        let id = identity(21);
        let boundaries = [boundary_with_count(0, 0, 1)];
        let published = finalize_segments(&request(
            &id,
            &flush(&id, 101),
            &evidence(&id, vec![pair(0, 101)]),
            &frame_states(&id, &boundaries, FrameCaptureState::Gameplay),
            &boundaries,
            &probe,
            &output,
        ))
        .unwrap();
        assert!(
            published[0].candidate_eligible,
            "absolute parity is not a delivery fact; verified frame-pair/cadence evidence is"
        );
    }

    #[test]
    fn segment_boundary_present_sequences_do_not_leak_across_segments() {
        let temp = tempfile::tempdir().unwrap();
        let probe = temp.path().join("probe");
        let output = temp.path().join("out");
        write_probe(&probe, &[100, 102]);
        let id = identity(1);
        let pairing = evidence(
            &id,
            vec![
                absent_pair(0, AbsentReason::OddFrameBySpec),
                absent_pair(1, AbsentReason::OddFrameBySpec),
                absent_pair(2, AbsentReason::OddFrameBySpec),
                pair(3, 100),
                pair(4, 102),
                absent_pair(5, AbsentReason::OddFrameBySpec),
                absent_pair(6, AbsentReason::OddFrameBySpec),
                absent_pair(7, AbsentReason::OddFrameBySpec),
            ],
        );
        let flushed = flush(&id, 102);
        let boundaries = [boundary(0, 0), boundary(1, 4)];
        let states = frame_states(&id, &boundaries, FrameCaptureState::Gameplay);
        finalize_segments(&request(
            &id,
            &flushed,
            &pairing,
            &states,
            &boundaries,
            &probe,
            &output,
        ))
        .unwrap();
        assert!(output.join("00000/raw/000000000100.exr").exists());
        assert!(!output.join("00000/raw/000000000102.exr").exists());
        assert!(output.join("00001/raw/000000000102.exr").exists());
    }

    #[test]
    fn refuses_duplicate_disorder_and_out_of_cutoff() {
        for (name, sequences, pair_sequences, expected) in [
            ("duplicate", vec![0, 0], vec![0], "duplicate present_seq"),
            ("disorder", vec![2, 0], vec![0, 2], "not strictly ordered"),
            ("cutoff", vec![0, 4], vec![0, 4], "exceeds flushed cutoff"),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let probe = temp.path().join(name);
            write_probe(&probe, &sequences);
            let id = identity(2);
            let pairs = pair_sequences
                .iter()
                .enumerate()
                .map(|(frame, seq)| pair(frame as u64, *seq))
                .collect();
            let pairing = evidence(&id, pairs);
            let cutoff = if name == "cutoff" { 2 } else { 10 };
            let flushed = flush(&id, cutoff);
            let boundaries = [boundary_with_count(0, 0, pair_sequences.len() as u64)];
            let states = frame_states(&id, &boundaries, FrameCaptureState::Gameplay);
            let error = finalize_segments(&request(
                &id,
                &flushed,
                &pairing,
                &states,
                &boundaries,
                &probe,
                &temp.path().join("out"),
            ))
            .unwrap_err();
            assert!(
                error.to_string().contains(expected),
                "{name}: unexpected error {error}"
            );
        }
    }

    #[test]
    fn unreferenced_selected_depth_is_preserved_as_probe_evidence_but_not_delivered() {
        let temp = tempfile::tempdir().unwrap();
        let probe = temp.path().join("probe");
        let output = temp.path().join("out");
        write_probe(&probe, &[0, 2]);
        let id = identity(2);
        let pairing = evidence(&id, vec![pair(0, 0)]);
        let flushed = flush(&id, 10);
        let boundaries = [boundary_with_count(0, 0, 1)];
        let states = frame_states(&id, &boundaries, FrameCaptureState::Gameplay);
        let published = finalize_segments(&request(
            &id,
            &flushed,
            &pairing,
            &states,
            &boundaries,
            &probe,
            &output,
        ))
        .unwrap();
        assert_eq!(published.len(), 1);
        assert!(output.join("00000/raw/000000000000.exr").is_file());
        assert!(!output.join("00000/raw/000000000002.exr").exists());
    }

    #[test]
    fn exact_duplicate_alias_reuses_one_exr_for_two_even_frames_in_one_segment() {
        let temp = tempfile::tempdir().unwrap();
        let probe = temp.path().join("probe");
        let output = temp.path().join("out");
        write_probe(&probe, &[100]);
        let id = identity(7);
        let mut odd_alias = pair(1, 100);
        odd_alias.depth = None;
        odd_alias.depth_absent_reason = Some(AbsentReason::OddFrameBySpec);
        let mut duplicate_even = pair(2, 100);
        duplicate_even.duplicate = true;
        duplicate_even.depth.as_mut().unwrap().inherited = true;
        let pairing = evidence(&id, vec![pair(0, 100), odd_alias, duplicate_even]);
        let flushed = flush(&id, 100);
        let boundaries = [boundary_with_count(0, 0, 3)];
        let states = frame_states(&id, &boundaries, FrameCaptureState::Gameplay);
        let published = finalize_segments(&request(
            &id,
            &flushed,
            &pairing,
            &states,
            &boundaries,
            &probe,
            &output,
        ))
        .unwrap();
        assert_eq!(
            published[0].object_count, 2,
            "one index + one exact source EXR"
        );
        let verified = verify_candidate_segment(
            &published[0].manifest_path,
            &id,
            0,
            &published[0].manifest_sha256,
        )
        .unwrap();
        assert_eq!(verified.frames.len(), 2);
        assert_eq!(verified.frames[0].present_seq, 100);
        assert_eq!(verified.frames[1].present_seq, 100);
        assert_eq!(
            verified.frames[0].source_path,
            verified.frames[1].source_path
        );
        let rows = fs::read_to_string(output.join("00000/depth-index.jsonl")).unwrap();
        let rows = rows
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["session_frame"], 0);
        assert_eq!(rows[1]["session_frame"], 2);
        assert_eq!(rows[0]["path"], rows[1]["path"]);
    }

    #[test]
    fn exact_source_alias_can_cross_segment_boundaries_without_a_second_readback() {
        let temp = tempfile::tempdir().unwrap();
        let probe = temp.path().join("probe");
        let output = temp.path().join("out");
        write_probe(&probe, &[100]);
        let id = identity(8);
        let mut odd_alias = pair(1, 100);
        odd_alias.depth = None;
        odd_alias.depth_absent_reason = Some(AbsentReason::OddFrameBySpec);
        let mut duplicate_even = pair(2, 100);
        duplicate_even.duplicate = true;
        duplicate_even.depth.as_mut().unwrap().inherited = true;
        let pairing = evidence(&id, vec![pair(0, 100), odd_alias, duplicate_even]);
        let flushed = flush(&id, 100);
        let boundaries = [boundary_with_count(0, 0, 2), boundary_with_count(1, 2, 1)];
        let states = frame_states(&id, &boundaries, FrameCaptureState::Gameplay);
        let published = finalize_segments(&request(
            &id,
            &flushed,
            &pairing,
            &states,
            &boundaries,
            &probe,
            &output,
        ))
        .unwrap();
        assert_eq!(published.len(), 2);
        assert!(output.join("00000/raw/000000000100.exr").is_file());
        assert!(output.join("00001/raw/000000000100.exr").is_file());
    }

    #[test]
    fn refuses_stale_generation_wrong_session_and_non_flushed_writer() {
        let temp = tempfile::tempdir().unwrap();
        let probe = temp.path().join("probe");
        write_probe(&probe, &[0]);
        let id = identity(4);
        let pairing = evidence(&id, vec![pair(0, 0)]);
        let boundaries = [boundary_with_count(0, 0, 1)];
        let states = frame_states(&id, &boundaries, FrameCaptureState::Gameplay);
        for bad in [
            identity(3),
            StreamIdentity::new("other", "r0000", 4).unwrap(),
        ] {
            let flushed = flush(&bad, 0);
            let error = finalize_segments(&request(
                &id,
                &flushed,
                &pairing,
                &states,
                &boundaries,
                &probe,
                &temp.path().join("out"),
            ))
            .unwrap_err();
            assert!(matches!(error, FinalizeError::IdentityMismatch { .. }));
        }
        let mut unflushed = flush(&id, 0);
        unflushed.writer_queue_depth = 1;
        let error = finalize_segments(&request(
            &id,
            &unflushed,
            &pairing,
            &states,
            &boundaries,
            &probe,
            &temp.path().join("out"),
        ))
        .unwrap_err();
        assert!(matches!(error, FinalizeError::NotFlushed { .. }));
    }

    #[test]
    fn refuses_missing_files_and_detects_committed_size_or_hash_damage() {
        let temp = tempfile::tempdir().unwrap();
        let probe = temp.path().join("probe");
        let output = temp.path().join("out");
        write_probe(&probe, &[0]);
        let id = identity(1);
        let pairing = evidence(&id, vec![pair(0, 0)]);
        let flushed = flush(&id, 0);
        let boundaries = [boundary_with_count(0, 0, 1)];
        let states = frame_states(&id, &boundaries, FrameCaptureState::Gameplay);
        let req = request(
            &id,
            &flushed,
            &pairing,
            &states,
            &boundaries,
            &probe,
            &output,
        );
        fs::remove_file(probe.join("raw/000000000000.exr")).unwrap();
        assert!(matches!(
            finalize_segments(&req).unwrap_err(),
            FinalizeError::FileMissing(_)
        ));

        write_probe(&probe, &[0]);
        crate::exr_out::write_frame(
            &probe.join("raw/000000000000.exr"),
            &[0.1, 0.2, 0.3, 0.4],
            2,
            2,
            crate::depth::Projection::reversed_z_infinite(0.1, 1.0),
            crate::depth::Variant::Reference,
        )
        .unwrap();
        let format_error = finalize_segments(&req).unwrap_err();
        assert!(
            format_error.to_string().contains("not fp16"),
            "unexpected EXR storage error: {format_error}"
        );

        write_probe(&probe, &[0]);
        finalize_segments(&req).unwrap();
        let published_exr = output.join("00000/raw/000000000000.exr");
        let mut same_size_tamper = fs::read(&published_exr).unwrap();
        same_size_tamper[0] ^= 0xff;
        fs::write(&published_exr, b"wide").unwrap();
        let size_error = finalize_segments(&req).unwrap_err();
        assert!(
            size_error.to_string().contains("size mismatch"),
            "unexpected size integrity error: {size_error}"
        );

        // 保持 EXR 字节长度不变，单独证明 hash 损坏也会 fail-closed。
        fs::write(&published_exr, same_size_tamper).unwrap();
        let hash_error = finalize_segments(&req).unwrap_err();
        assert!(
            hash_error.to_string().contains("SHA-256 mismatch"),
            "unexpected hash integrity error: {hash_error}"
        );
    }

    #[test]
    fn interruption_leaves_only_staging_and_retry_recovers_atomically() {
        let temp = tempfile::tempdir().unwrap();
        let probe = temp.path().join("probe");
        let output = temp.path().join("out");
        write_probe(&probe, &[0]);
        let id = identity(9);
        let pairing = evidence(&id, vec![pair(0, 0)]);
        let flushed = flush(&id, 0);
        let boundaries = [boundary_with_count(0, 0, 1)];
        let states = frame_states(&id, &boundaries, FrameCaptureState::Gameplay);
        let req = request(
            &id,
            &flushed,
            &pairing,
            &states,
            &boundaries,
            &probe,
            &output,
        );

        let error = finalize_segments_inner(&req, FaultPoint::AfterFirstStage).unwrap_err();
        assert!(error.to_string().contains("injected interruption"));
        assert!(!output.join("00000").exists());
        assert!(output.join(".staging/00000-g9").exists());

        let published = finalize_segments(&req).unwrap();
        assert_eq!(published.len(), 1);
        assert!(output.join("00000/depth-segment.json").exists());
        assert!(!output.join(".staging/00000-g9").exists());
    }

    #[test]
    fn retry_recovers_when_filesystem_publish_precedes_journal_commit() {
        let temp = tempfile::tempdir().unwrap();
        let probe = temp.path().join("probe");
        let output = temp.path().join("out");
        write_probe(&probe, &[0, 2]);
        let id = identity(10);
        let pairing = evidence(&id, vec![pair(0, 0), pair(1, 2)]);
        let flushed = flush(&id, 2);
        let boundaries = [boundary_with_count(0, 0, 1), boundary_with_count(1, 1, 1)];
        let states = frame_states(&id, &boundaries, FrameCaptureState::Gameplay);
        let req = request(
            &id,
            &flushed,
            &pairing,
            &states,
            &boundaries,
            &probe,
            &output,
        );

        let error = finalize_segments_inner(&req, FaultPoint::AfterFirstPublish).unwrap_err();
        assert!(error.to_string().contains("after filesystem publish"));
        assert!(output.join("00000/depth-segment.json").is_file());
        assert!(!output.join("00001").exists());

        let recovered = finalize_segments(&req).unwrap();
        assert_eq!(recovered.len(), 2);
        assert!(recovered.iter().all(|segment| segment.candidate_eligible));
        assert!(output.join("00001/depth-segment.json").is_file());
    }

    #[test]
    fn pairing_with_a_frame_outside_journal_boundaries_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let probe = temp.path().join("probe");
        write_probe(&probe, &[0]);
        let id = identity(1);
        let pairing = evidence(&id, vec![pair(9, 0)]);
        let flushed = flush(&id, 0);
        let boundaries = [boundary(0, 0)];
        let states = frame_states(&id, &boundaries, FrameCaptureState::Gameplay);
        let error = finalize_segments(&request(
            &id,
            &flushed,
            &pairing,
            &states,
            &boundaries,
            &probe,
            &temp.path().join("out"),
        ))
        .unwrap_err();
        assert!(error.to_string().contains("belongs to no ready segment"));
    }
}

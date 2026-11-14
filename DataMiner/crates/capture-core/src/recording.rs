use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, ensure};
use crossbeam_channel::{Receiver, Sender, bounded, select, unbounded};
use serde::Serialize;
use sha2::{Digest, Sha256};
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_TEXTURE2D_DESC, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Storage::FileSystem::{MOVEFILE_WRITE_THROUGH, MoveFileExW};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::core::PCWSTR;
use windows_capture_wgc::{CaptureControl, WgcSettings, Window};

use crate::audio_contract::{
    AUDIO_ARTIFACT_NAME, SegmentAudioHealth, SegmentAudioMetadata, SegmentAudioState,
};
use crate::audio_segment::{AudioAck, AudioSegmentSeal, AudioSegmentWriter};
use crate::cadence_trace::{
    CadenceTrace, Event as CadenceEvent, SealReason, Slot as CadenceSlot, SourceStage,
};
#[cfg(all(feature = "cadence-trace", debug_assertions))]
pub use crate::cadence_trace::{CadenceTraceConfig, RawWatermarkTraceConfig};
use crate::capture::{
    CAPTURE_QUEUE_CAPACITY, CaptureIngress, CapturedFrame, CfrIngressPoll, CfrObservation,
    CfrSlotSelector, DEFAULT_WATERMARK_DECODE_MARGIN, ENCODER_DRAIN_CFR_FRAMES, GpuCopyHandler,
    PresentIdentitySession, SelectedSourceIdentity, WGC_SOURCE_OBSERVATIONS_PER_CFR_FRAME,
    WatermarkCaptureConfig, copy_queries_after_start, decode_watermark_texture,
    gpu_copy_wait_limit_qpc, poll_cfr_ingress, textures_after_start, wait_for_gpu_copy_completion,
    watermark_textures_after_start,
};
use crate::crop::{capture_surface_size, client_area_crop};
use crate::d3d::D3dContext;
use crate::gpu::{BgraToNv12, VideoConversionSpec, create_bgra_copy_texture, create_nv12_texture};
use crate::ingest_contract::SessionContract;
use crate::input::{
    EventSegmentSeal, RawEventWriter, RawInputControl, RawInputEvent, RawInputRecord,
    RawInputStatus, open_sealed_event_file, wait_input_ack,
};
use crate::mapping::MappingSnapshot;
use crate::mf::{
    EffectiveEncoderConfig, EncodedH264Sample, H264Mp4Writer, HardwareH264Encoder, MfPlatform,
};
use crate::platform::WindowsVersion;
use crate::probe::{Mp4Probe, probe_h264_mp4};
use crate::profile::CaptureProfile;
use crate::projection::{FrameProjectionRow, FrameProjector, FrameTiming};
use crate::qpc::{QpcClock, frame_duration_100ns, frame_pts_100ns, frame_qpc};
use crate::wall_clock::WallClockSample;

const NV12_POOL_SIZE: usize = 6;
const NV12_CANONICAL_SLOT: usize = 0;
/// Wall-clock liveness watchdog for Media Foundation encoder progress.
///
/// Source loss is detected independently by the bounded WGC queue. A short
/// NVIDIA/Windows scheduling stall which the queue absorbs without a drop is
/// recoverable and must not be mislabeled as encoder failure. Two seconds is
/// only the final bound for a writer which stops returning both output samples
/// and tracked NV12 surfaces permanently.
const ENCODER_PROGRESS_LIVENESS_WATCHDOG_MS: i64 = 2_000;
const SEAL_DELAY_MS: i64 = 4;
const AUDIO_SEAL_TIMEOUT: Duration = Duration::from_secs(3);
const SELECTED_FEEDBACK_SEAL_TIMEOUT: Duration = Duration::from_secs(3);
const WGC_FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(3);
// One requested 30-FPS CFR window contains at most eight 240-Hz Present
// identities. A valid-CRC observation that regresses by no more than this can
// only be recycled before selection; it never advances the decoder or becomes
// a canonical RGB source. This is a bounded v1 acceptance budget, not a claim
// about every WGC/compositor implementation.
const WGC_STALE_PRESENT_BUDGET: u64 = WGC_SOURCE_OBSERVATIONS_PER_CFR_FRAME as u64;
static PREFLIGHT_SCRATCH_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct RecorderFeedback {
    transport: present_identity::transport::ClientTransport,
    stream: present_identity::feedback::StreamEpoch,
    next_decision_ordinal: u64,
    next_ack_ordinal: u64,
    pending: VecDeque<(u64, present_identity::feedback::Decision)>,
    max_pending: usize,
    select_count: u64,
    distinct_depth_count: u64,
    last_selected_present_seq: Option<u64>,
    last_depth_present_seq: Option<u64>,
    cadence: present_identity::feedback::DepthSelectionCadence,
    first_candidate_present_seq: Option<u64>,
    arm_sent: bool,
    sealed: bool,
}

struct SelectedFrameDecision {
    session_frame: u64,
    present_seq: u64,
    duplicate: bool,
    source_superseded: u32,
    source_qpc: i64,
    callback_start_qpc: i64,
    callback_ready_qpc: i64,
    decision_qpc: i64,
}

fn observation_is_at_or_after_arm_boundary(
    present_seq: Option<u64>,
    arm_boundary: Option<u64>,
) -> anyhow::Result<bool> {
    let Some(boundary) = arm_boundary else {
        return Ok(true);
    };
    let present_seq = present_seq
        .context("selected-Present WGC observation has no full identity at the Arm boundary")?;
    Ok(present_seq >= boundary)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StartupObservationDisposition {
    ProveWgcLiveThenArm,
    Recycle,
    Eligible,
}

fn scaled_watermark_extent(
    logical_extent: u32,
    client_extent: u32,
    source_extent: u32,
) -> anyhow::Result<u32> {
    ensure!(source_extent > 0, "watermark source extent is zero");
    let numerator = u64::from(logical_extent)
        .checked_mul(u64::from(client_extent))
        .context("scaled watermark extent overflow")?;
    let scaled = numerator
        .checked_add(u64::from(source_extent) - 1)
        .context("scaled watermark rounding overflow")?
        / u64::from(source_extent);
    u32::try_from(scaled).context("scaled watermark extent exceeds u32")
}

fn scaled_watermark_geometry(
    client_width: u32,
    client_height: u32,
    source_width: u32,
    source_height: u32,
) -> anyhow::Result<(u32, u32)> {
    ensure!(
        client_width > 0 && client_height > 0 && source_width > 0 && source_height > 0,
        "watermark source/client geometry contains a zero extent"
    );
    ensure!(
        u64::from(client_width) * u64::from(source_height)
            == u64::from(client_height) * u64::from(source_width),
        "watermark source geometry {source_width}x{source_height} and WGC client geometry {client_width}x{client_height} have different aspect ratios"
    );
    Ok((
        scaled_watermark_extent(
            present_identity::watermark::RECT_W,
            client_width,
            source_width,
        )?,
        scaled_watermark_extent(
            present_identity::watermark::RECT_H,
            client_height,
            source_height,
        )?,
    ))
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct WatermarkObservationStats {
    decoded: u64,
    rejected: u64,
    integrity_mismatch: u64,
    ambiguous: u64,
    consecutive: u64,
    max_consecutive: u64,
    stale: u64,
    max_stale_lag: u64,
}

impl WatermarkObservationStats {
    fn classify<T>(
        &mut self,
        source_seq: u64,
        decoded: anyhow::Result<T>,
    ) -> anyhow::Result<Option<T>> {
        match decoded {
            Ok(value) => {
                self.decoded = self.decoded.saturating_add(1);
                self.consecutive = 0;
                Ok(Some(value))
            }
            Err(error)
                if matches!(
                    error.downcast_ref::<present_identity::watermark::DecodeError>(),
                    Some(
                        present_identity::watermark::DecodeError::IntegrityMismatch
                            | present_identity::watermark::DecodeError::AmbiguousBlock
                    )
                ) =>
            {
                match error.downcast_ref::<present_identity::watermark::DecodeError>() {
                    Some(present_identity::watermark::DecodeError::IntegrityMismatch) => {
                        self.integrity_mismatch = self.integrity_mismatch.saturating_add(1);
                    }
                    Some(present_identity::watermark::DecodeError::AmbiguousBlock) => {
                        self.ambiguous = self.ambiguous.saturating_add(1);
                    }
                    _ => unreachable!("rejectable DecodeError was matched above"),
                }
                self.rejected = self.rejected.saturating_add(1);
                self.consecutive = self.consecutive.saturating_add(1);
                self.max_consecutive = self.max_consecutive.max(self.consecutive);
                tracing::warn!(
                    source_seq,
                    rejected = self.rejected,
                    integrity_rejections = self.integrity_mismatch,
                    ambiguous_rejections = self.ambiguous,
                    consecutive = self.consecutive,
                    error = %error,
                    "discarding WGC observation with an invalid Present watermark"
                );
                Ok(None)
            }
            Err(error) => Err(error).with_context(|| {
                format!("WGC watermark decode failed for source_seq {source_seq}")
            }),
        }
    }

    fn expand(
        &mut self,
        source_seq: u64,
        decoder: &mut present_identity::FullSequenceDecoder,
        generation: u64,
        raw: u16,
    ) -> anyhow::Result<Option<u64>> {
        match decoder
            .observe_with_stale_budget(generation, raw, WGC_STALE_PRESENT_BUDGET)
            .with_context(|| {
                format!("WGC watermark full-sequence expansion failed for source_seq {source_seq}")
            })? {
            present_identity::SequenceObservation::Accepted(present_seq) => Ok(Some(present_seq)),
            present_identity::SequenceObservation::Stale { present_seq, lag } => {
                self.stale = self.stale.saturating_add(1);
                self.max_stale_lag = self.max_stale_lag.max(lag);
                if self.stale.is_power_of_two() {
                    tracing::warn!(
                        source_seq,
                        present_seq,
                        lag,
                        stale_observations = self.stale,
                        "recycling bounded stale WGC Present observations before CFR selection"
                    );
                }
                Ok(None)
            }
        }
    }
}

fn classify_startup_observation(
    present_seq: Option<u64>,
    arm_required: bool,
    arm_started: bool,
    arm_boundary: Option<u64>,
) -> anyhow::Result<StartupObservationDisposition> {
    if arm_required {
        present_seq.context(
            "selected-Present pre-Arm WGC observation has no decoded full Present identity",
        )?;
    }
    if arm_required && !arm_started {
        return Ok(StartupObservationDisposition::ProveWgcLiveThenArm);
    }
    if arm_required && arm_boundary.is_none() {
        return Ok(StartupObservationDisposition::Recycle);
    }
    if observation_is_at_or_after_arm_boundary(present_seq, arm_boundary)? {
        Ok(StartupObservationDisposition::Eligible)
    } else {
        Ok(StartupObservationDisposition::Recycle)
    }
}

impl RecorderFeedback {
    fn new(transport: present_identity::transport::ClientTransport) -> Self {
        // Probe consumes one decision and publishes one ACK. Allow one full queue in each
        // direction, then fail closed instead of letting a non-acking peer grow memory forever.
        let max_pending = transport.queue_capacity().saturating_mul(2);
        Self {
            stream: transport.stream().clone(),
            transport,
            next_decision_ordinal: 1,
            next_ack_ordinal: 1,
            pending: VecDeque::new(),
            max_pending,
            select_count: 0,
            distinct_depth_count: 0,
            last_selected_present_seq: None,
            last_depth_present_seq: None,
            cadence: present_identity::feedback::DepthSelectionCadence::EveryRgbFrame,
            first_candidate_present_seq: None,
            arm_sent: false,
            sealed: false,
        }
    }

    fn begin_arm(&mut self) -> anyhow::Result<()> {
        ensure!(
            !self.arm_sent,
            "selected-Present generation was armed twice"
        );
        self.send(present_identity::feedback::Decision::ArmGeneration)?;
        self.arm_sent = true;
        Ok(())
    }

    fn poll_arm_boundary(&mut self) -> anyhow::Result<Option<u64>> {
        self.drain_acks()?;
        Ok(self.first_candidate_present_seq)
    }

    fn select(&mut self, decision: SelectedFrameDecision) -> anyhow::Result<()> {
        let SelectedFrameDecision {
            session_frame,
            present_seq,
            duplicate,
            source_superseded,
            source_qpc,
            callback_start_qpc,
            callback_ready_qpc,
            decision_qpc,
        } = decision;
        if let Some(previous) = self.last_selected_present_seq {
            ensure!(
                present_seq >= previous,
                "selected-Present identity regressed from {previous} to {present_seq}"
            );
            ensure!(
                !duplicate || present_seq == previous,
                "selected-Present duplicate changed source identity: previous={previous} current={present_seq}"
            );
        } else {
            ensure!(
                !duplicate,
                "first selected-Present frame cannot be duplicate"
            );
        }
        self.send(present_identity::feedback::Decision::Select {
            session_frame,
            present_seq,
            depth_required: self.cadence.requires_depth(session_frame),
            duplicate,
            source_superseded,
            source_qpc,
            callback_start_qpc,
            callback_ready_qpc,
            decision_qpc,
        })?;
        self.select_count = self.select_count.saturating_add(1);
        if self.cadence.requires_depth(session_frame)
            && self.last_depth_present_seq != Some(present_seq)
        {
            self.distinct_depth_count = self.distinct_depth_count.saturating_add(1);
            self.last_depth_present_seq = Some(present_seq);
        }
        self.last_selected_present_seq = Some(present_seq);
        Ok(())
    }

    fn retire_through(&mut self, present_seq: u64) -> anyhow::Result<()> {
        self.send(present_identity::feedback::Decision::RetireThrough { present_seq })
    }

    fn seal(&mut self, final_session_frame: Option<u64>) -> anyhow::Result<()> {
        ensure!(!self.sealed, "selected-Present generation was sealed twice");
        self.send(present_identity::feedback::Decision::SealGeneration {
            final_session_frame,
            expected_select_count: self.select_count,
            expected_distinct_depth_count: self.distinct_depth_count,
        })?;
        let deadline = Instant::now() + SELECTED_FEEDBACK_SEAL_TIMEOUT;
        while !self.sealed {
            self.drain_acks()?;
            ensure!(
                Instant::now() < deadline,
                "selected-Present GenerationSealed ACK timed out"
            );
            thread::sleep(Duration::from_millis(1));
        }
        ensure!(
            self.pending.is_empty(),
            "selected-Present generation sealed with {} unresolved decision ACK(s)",
            self.pending.len()
        );
        Ok(())
    }

    fn send(&mut self, payload: present_identity::feedback::Decision) -> anyhow::Result<()> {
        // A probe can fault while draining an earlier readback, outside the
        // decision handler. Observe its terminal ACK/header before appending
        // another decision, otherwise a full queue can mask the original cause.
        self.drain_acks()?;
        ensure!(!self.sealed, "selected-Present decision sent after Seal");
        ensure!(
            self.pending.len() < self.max_pending,
            "selected-Present peer exceeded the bounded pending-ACK budget ({})",
            self.max_pending
        );
        let ordinal = self.next_decision_ordinal;
        let message = present_identity::feedback::Envelope {
            schema: self.cadence.schema(),
            stream: self.stream.clone(),
            ordinal,
            payload: payload.clone(),
        };
        self.transport
            .decisions
            .try_push(&message)
            .with_context(|| format!("selected-Present decision {ordinal} enqueue failed"))?;
        self.pending.push_back((ordinal, payload));
        self.next_decision_ordinal = self.next_decision_ordinal.saturating_add(1);
        self.drain_acks()
    }

    fn drain_acks(&mut self) -> anyhow::Result<()> {
        while let Some(message) = self
            .transport
            .acks
            .try_pop()
            .context("selected-Present ACK dequeue failed")?
        {
            ensure!(
                message.schema == self.cadence.schema(),
                "selected-Present ACK schema mismatch"
            );
            ensure!(
                message.ordinal == self.next_ack_ordinal,
                "selected-Present ACK ordinal gap: expected {}, got {}",
                self.next_ack_ordinal,
                message.ordinal
            );
            let decision_ordinal = match &message.payload {
                present_identity::feedback::Ack::GenerationArmed {
                    decision_ordinal, ..
                }
                | present_identity::feedback::Ack::SelectAccepted {
                    decision_ordinal, ..
                }
                | present_identity::feedback::Ack::Retired {
                    decision_ordinal, ..
                }
                | present_identity::feedback::Ack::GenerationSealed {
                    decision_ordinal, ..
                }
                | present_identity::feedback::Ack::Fault {
                    decision_ordinal, ..
                } => *decision_ordinal,
            };
            let (expected_ordinal, expected) = self.pending.front().with_context(|| {
                format!("selected-Present ACK references unknown decision {decision_ordinal}")
            })?;
            ensure!(
                *expected_ordinal == decision_ordinal,
                "selected-Present ACK did not resolve the oldest decision: expected {expected_ordinal}, got {decision_ordinal}"
            );
            match (&message.payload, expected) {
                (
                    present_identity::feedback::Ack::GenerationArmed {
                        first_candidate_present_seq,
                        ..
                    },
                    present_identity::feedback::Decision::ArmGeneration,
                ) => {
                    self.first_candidate_present_seq = Some(*first_candidate_present_seq);
                }
                (
                    present_identity::feedback::Ack::SelectAccepted {
                        session_frame,
                        present_seq,
                        depth_required,
                        ..
                    },
                    present_identity::feedback::Decision::Select {
                        session_frame: expected_frame,
                        present_seq: expected_present,
                        depth_required: expected_required,
                        ..
                    },
                ) if session_frame == expected_frame
                    && present_seq == expected_present
                    && depth_required == expected_required => {}
                (
                    present_identity::feedback::Ack::Retired { present_seq, .. },
                    present_identity::feedback::Decision::RetireThrough {
                        present_seq: expected_present,
                    },
                ) if present_seq == expected_present => {}
                (
                    present_identity::feedback::Ack::GenerationSealed {
                        select_count,
                        distinct_depth_count,
                        ..
                    },
                    present_identity::feedback::Decision::SealGeneration { .. },
                ) if *select_count == self.select_count
                    && *distinct_depth_count == self.distinct_depth_count =>
                {
                    self.sealed = true;
                }
                (present_identity::feedback::Ack::Fault { reason, .. }, _) => {
                    anyhow::bail!("selected-Present probe fault: {}", reason.code());
                }
                _ => anyhow::bail!(
                    "selected-Present ACK payload conflicts with decision {decision_ordinal}"
                ),
            }
            self.pending.pop_front();
            self.next_ack_ordinal = self.next_ack_ordinal.saturating_add(1);
        }
        self.transport
            .check_fault()
            .context("selected-Present peer transport fault")
    }
}

pub struct RecordingRequest {
    pub session_id: String,
    pub run_id: String,
    pub session_frame_start: u64,
    pub output_root: PathBuf,
    pub target: RecordingTarget,
    pub profile: CaptureProfile,
    /// An explicit, local-only segmentation override. The signed catalog bytes
    /// remain untouched; this records why the effective recorder profile uses
    /// a different segment layout for the same bounded capture horizon.
    pub segment_policy_override: Option<SegmentPolicyOverride>,
    pub mapping: MappingSnapshot,
    pub catalog: RecordingCatalogContext,
    /// Application control keys from the session's input contract.
    ///
    /// Supplied so the projector arbitrates control input from the contract
    /// rather than from the flag persisted on each event — see
    /// [`crate::projection::FrameProjector::with_control_vks`]. Empty means
    /// "no control keys declared", which is different from "not supplied":
    /// the recorder always knows its contract.
    pub control_vks: Vec<u16>,
    /// A declared consent id, recorded verbatim into the session manifest when
    /// present. It no longer changes any classification: with the certified lane
    /// gone (docs/100) RAW output is unconditionally non-trainable and the
    /// `consented_*` states are retired. Nothing populates this today — the
    /// channel is kept so the next cut (trainability enablement) has a carrier.
    pub consent_id: Option<String>,
    /// Enables exact online WGC identity. `None` preserves the ordinary video
    /// path without staging or mapping the watermark ROI.
    pub present_identity: Option<PresentIdentitySession>,
    pub selected_present_feedback: Option<present_identity::transport::ClientTransport>,
    /// Explicit local CP2077 raw diagnostic opt-in; ordinary recordings keep V3.
    pub selected_depth_cadence: present_identity::feedback::DepthSelectionCadence,
    pub cutoff_qpc: Arc<AtomicI64>,
    pub input_records: Receiver<RawInputRecord>,
    pub input_control: RawInputControl,
    pub input_status: RawInputStatus,
}

#[derive(Clone, Debug, Serialize)]
pub struct SegmentPolicyOverride {
    pub reason: String,
    pub catalog_segment_seconds: u32,
    pub catalog_test_segments: u32,
    pub effective_segment_seconds: u32,
    pub effective_test_segments: u32,
}

impl SegmentPolicyOverride {
    fn validate_for(&self, profile: &CaptureProfile) -> anyhow::Result<()> {
        ensure!(
            !self.reason.trim().is_empty(),
            "segment policy override reason is empty"
        );
        ensure!(
            self.catalog_segment_seconds > 0
                && self.catalog_test_segments > 0
                && self.effective_segment_seconds > 0
                && self.effective_test_segments > 0,
            "segment policy override values must be positive"
        );
        ensure!(
            profile.segment_seconds == self.effective_segment_seconds
                && profile.test_segments == self.effective_test_segments,
            "effective recorder profile does not match its segment policy override"
        );
        ensure!(
            u64::from(self.catalog_segment_seconds) * u64::from(self.catalog_test_segments)
                == u64::from(self.effective_segment_seconds)
                    * u64::from(self.effective_test_segments),
            "segment policy override changed the bounded capture horizon"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct RecordingCatalogEntry {
    pub id: String,
    pub version: String,
    pub content_hash: String,
    #[serde(skip)]
    pub source_bytes: Arc<Vec<u8>>,
}

/// Where the active catalog's bytes came from.
///
/// `mode` says what the catalog claims to be; this says how it reached the
/// client. The two are independent: a `dev_non_trainable` catalog can arrive
/// either compiled into the binary or over signed delivery, and the recorded
/// session cannot be reconciled against a source of truth without knowing
/// which. docs/54 found a collection box whose game list held six profiles
/// present in no git ref; nothing in a session's artifacts said so.
///
/// This is a recorded fact, not a gate — `Compiled` is normal for dev builds
/// and is never refused here.
///
/// Not to be confused with the converter's `SessionProvenance`/`ChunkProvenance`
/// (data-demo-converter), which is *data lineage* — which RAW session and mapping
/// a derived chunk came from. Both are "provenance" and both are qualified by
/// their parent: this one is only ever read as `catalog.provenance`, that one as
/// `metadata.provenance`. DataModel output carries this fact separately as
/// `catalog.provenance` without conflating the two meanings.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogProvenance {
    /// `include_bytes!` payloads from the repository config directory.
    Compiled,
    /// A signed bundle fetched from the catalog delivery API.
    Delivered,
}

impl CatalogProvenance {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Compiled => "compiled",
            Self::Delivered => "delivered",
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct RecordingCatalogContext {
    /// 这场采集属于哪一代摄取契约（issue #855 §B）。下面两个字段由它派生，
    /// 只为把 legacy 产物的字节原样留住；引擎自己读的是这一个。
    ///
    /// `skip`：它不是一条独立的落盘事实。legacy 产物里表达契约的正是 `mode`，
    /// 中性产物由 `schema_version` 表达——多写一个字段只会给读者两份可能打架的说法。
    #[serde(skip)]
    pub contract: SessionContract,
    /// legacy 目录身份里的 `mode` 字面量（`dev_non_trainable`）。
    ///
    /// 由 `contract` 派生，唯一构造点是 `CatalogSelection::recording_context()`；
    /// `record_until_stopped` 开头会核对两者自洽。中性契约为 `None` 且**不落盘**：
    /// 目录不再自称车道，"从哪条通道来"由验签的钉死公钥回答（issue #855 §A.4）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// legacy 一代的渠道派生分类（docs/57 §3.4 的第一个布尔在 manifest 上的落点），
    /// 恒 true。中性契约不再携带训练声明（§B），为 `None` 且不落盘。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub non_trainable: Option<bool>,
    pub provenance: CatalogProvenance,
    pub catalog_id: String,
    pub catalog_version: String,
    pub catalog_content_hash: String,
    #[serde(skip)]
    pub catalog_source_bytes: Arc<Vec<u8>>,
    pub device: RecordingCatalogEntry,
    /// 合并后的 recording profile（引擎参数 + 游戏侧配置，docs/57 §3.1）。
    /// 引擎只拿它的投影：`RecordingRequest::profile` 那个 `CaptureProfile`。
    pub recording: RecordingCatalogEntry,
    pub game: RecordingCatalogEntry,
    pub mapping: RecordingCatalogEntry,
}

#[derive(Clone, Debug, Serialize)]
pub struct HardwarePreflightEvidence {
    pub schema_version: &'static str,
    pub scope: &'static str,
    pub wgc_frame_observed: bool,
    pub windows: WindowsVersion,
    /// Exact DXGI adapter description observed by the capture device. Depth
    /// performance certification keys off this value, never a configured or
    /// guessed model name.
    pub adapter_description: String,
    pub adapter_vendor_id: u32,
    pub adapter_device_id: u32,
    /// Exact DXGI adapter identity. Unlike the display name, the LUID is
    /// unambiguous on hybrid/virtual-adapter systems.
    pub adapter_luid_high: i32,
    pub adapter_luid_low: u32,
    pub adapter_driver_version: Option<String>,
    pub d3d_feature_level: u32,
    pub encoder: HardwareEncoderPreflightEvidence,
    pub profile: HardwareCaptureProfileEvidence,
    pub media_probe: Mp4Probe,
    pub gpu_conversion: &'static str,
    pub scratch_artifact_retained: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct HardwareEncoderPreflightEvidence {
    pub transform_clsid: Option<String>,
    pub friendly_name: Option<String>,
    pub control_policy: &'static str,
    pub relaxed_controls: Vec<String>,
    pub rate_control_mode: u32,
    pub mean_bitrate_bps: u32,
    pub max_bitrate_bps: u32,
    pub gop_frames: u32,
    pub b_frames: u32,
    pub low_latency: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct HardwareCaptureProfileEvidence {
    pub profile_id: String,
    pub width: u32,
    pub height: u32,
    pub fps_num: u32,
    pub fps_den: u32,
}

#[derive(Clone, Debug, Serialize)]
pub struct RecordingTarget {
    #[serde(skip)]
    pub hwnd_value: usize,
    #[serde(skip)]
    pub process_id: u32,
    pub target_id: String,
    pub game_id: String,
    /// 实测 build 指纹，受保护进程拒绝观察时缺席（docs/55 §3.1）。
    /// session.json 里如实写 null，而不是编一个占位串。
    pub executable_sha256: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordingStopReason {
    Manual,
    Pause,
    ForegroundLost,
    WindowMinimized,
    WindowDestroyed,
    GeometryChanged,
    DiskLow,
    DiskStatusUnavailable,
    SourceUnavailable,
    InputFault,
    /// 录制期检测到游戏配置文件变更（视角标定政策项可能已失效，
    /// docs/45 §6.1）：当前 run 立即封口，重新对表后才能继续。
    SettingsChanged,
    CatalogRetired,
    /// 深度连续异常到线，探针已自行中止并留存现场（docs/130 §6.5）。
    ///
    /// 与 `DiskLow` 不同：磁盘满是可以清一清再录的，而连续 300 帧没有有效深度
    /// 说明有系统性问题（DSV 选错 / 快照环失效 / 场景切换未适配）。
    /// **所以它封口进 Faulted 而不是 Completed**——再录一次只会再废一次。
    DepthFrameHealth,
}

#[derive(Clone, Copy, Debug)]
pub struct RecordingStopSignal {
    pub reason: RecordingStopReason,
    pub cutoff_qpc: i64,
}

#[derive(Clone, Debug)]
pub enum RecorderEvent {
    Started {
        anchor_qpc: i64,
        anchor_at_utc: Option<String>,
        qpc_frequency: i64,
    },
    SegmentStarted {
        segment_index: u32,
        session_frame_start: u64,
    },
    SegmentFinalizing {
        segment_index: u32,
        session_frame_start: u64,
        frame_count: u64,
    },
    Progress(RecordingProgress),
    SegmentReady {
        segment: PublishedSegment,
        ready_segments: u32,
    },
}

#[derive(Clone, Debug)]
pub struct PublishedSegment {
    pub index: u32,
    pub frame_count: u64,
    pub event_count: u64,
    pub qpc_start: i64,
    pub qpc_end: i64,
    pub video_bytes: u64,
    pub video_sha256: String,
    pub frames_sha256: String,
    pub events_sha256: String,
    pub meta_sha256: String,
    pub audio: Option<Box<SegmentAudioMetadata>>,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordingProgress {
    pub observed_qpc: i64,
    pub frame_count: u64,
    /// Every WGC FrameArrived callback observed by the capture ingress.
    ///
    /// Performance certification must use this source fact directly. Rebuilding
    /// it from selected/duplicate/superseded CFR decisions drops watermark-stale
    /// observations and creates an artificial FPS regression in injected stages.
    pub wgc_callback_count: u64,
    pub active_segment: u32,
    pub ready_segments: u32,
    pub duplicate_frames: u64,
    pub source_superseded: u64,
    pub capture_queue_depth: usize,
    pub capture_queue_drops: u64,
    pub input_event_count: u64,
    pub input_queue_depth: usize,
    pub input_queue_overflows: u64,
    pub pressed_contract_keys: Vec<u16>,
    pub mouse_buttons: Vec<u8>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordingOutcome {
    pub schema_version: &'static str,
    pub state: &'static str,
    /// legacy 的训练声明，恒 `Some(true)`；中性契约为 `None` 且不落盘（§B）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub non_trainable: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub consent_id: Option<String>,
    /// 同上：first_party 时代的会话声明，legacy 恒 `Some(false)`、中性缺席。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub production_certified: Option<bool>,
    pub input_status: &'static str,
    pub session_id: String,
    pub run_id: String,
    pub mapping_id: String,
    pub mapping_version: String,
    pub mapping_content_hash: String,
    pub session_frame_start: u64,
    pub session_frame_end: u64,
    #[serde(skip)]
    pub run_directory: PathBuf,
    pub frame_count: u64,
    pub segment_count: u32,
    pub duplicate_frames: u64,
    pub source_superseded: u64,
    pub wgc_callback_count: u64,
    pub watermark_decoded_observations: u64,
    pub watermark_rejected_observations: u64,
    pub watermark_integrity_rejections: u64,
    pub watermark_ambiguous_rejections: u64,
    pub max_consecutive_watermark_rejections: u64,
    pub watermark_stale_observations: u64,
    pub max_watermark_stale_lag_presents: u64,
    pub input_event_count: u64,
    pub input_queue_overflows: u64,
    pub anchor_qpc: Option<i64>,
    // 描述性墙钟锚点：`anchor_at_utc` 是 `anchor_qpc`、`ended_at_utc` 是 `qpc_end`
    // 对应的 UTC 时刻，各由 run 两端一次独立的 (QPC, UTC) 配对读数投影而来。
    // 每帧每事件都带 QPC 且 `qpc_frequency` 已知，这两对锚点足以还原全部记录的
    // 绝对时刻。二者都不参与排序或帧对齐——那仍然只用 QPC。
    pub anchor_at_utc: Option<String>,
    pub qpc_frequency: i64,
    pub qpc_start: Option<i64>,
    pub qpc_end: Option<i64>,
    pub ended_at_utc: Option<String>,
    pub stop_reason: RecordingStopReason,
}

#[derive(Clone, Debug, Serialize)]
struct SegmentSummary {
    index: u32,
    frame_count: u64,
    event_count: u64,
    qpc_start: i64,
    qpc_end: i64,
    video_bytes: u64,
    video_sha256: String,
    frames_sha256: String,
    events_sha256: String,
    meta_sha256: String,
    audio: Option<SegmentAudioMetadata>,
    probe: Mp4Probe,
}

impl From<&SegmentSummary> for PublishedSegment {
    fn from(summary: &SegmentSummary) -> Self {
        Self {
            index: summary.index,
            frame_count: summary.frame_count,
            event_count: summary.event_count,
            qpc_start: summary.qpc_start,
            qpc_end: summary.qpc_end,
            video_bytes: summary.video_bytes,
            video_sha256: summary.video_sha256.clone(),
            frames_sha256: summary.frames_sha256.clone(),
            events_sha256: summary.events_sha256.clone(),
            meta_sha256: summary.meta_sha256.clone(),
            audio: summary.audio.clone().map(Box::new),
        }
    }
}

struct ComGuard;

impl ComGuard {
    fn mta() -> anyhow::Result<Self> {
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
            .ok()
            .context("CoInitializeEx(MTA) failed")?;
        Ok(Self)
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

struct FinalizeJob {
    index: u32,
    writer: H264Mp4Writer,
    timing: Vec<FrameTiming>,
    staging_dir: PathBuf,
    final_dir: PathBuf,
    prepare: Option<(u32, PathBuf)>,
    effective_encoder: EffectiveEncoderConfig,
    events: Receiver<Result<EventSegmentSeal, String>>,
    audio: Option<Receiver<AudioAck<AudioSegmentSeal>>>,
}

// Media Foundation COM wrappers are created for MTA use. The transfer is to
// one owned, joinable MTA finalizer thread and never aliases a writer.
unsafe impl Send for FinalizeJob {}

struct FinalizeResult {
    summary: SegmentSummary,
    prepared: Option<(u32, H264Mp4Writer)>,
}

unsafe impl Send for FinalizeResult {}

struct Finalizer {
    jobs: Sender<FinalizeJob>,
    results: Receiver<anyhow::Result<FinalizeResult>>,
    join: Option<JoinHandle<()>>,
}

struct FinalizeContext {
    profile: CaptureProfile,
    target: RecordingTarget,
    session_id: String,
    run_id: String,
    mapping: MappingSnapshot,
    catalog: RecordingCatalogContext,
    consent_id: Option<String>,
    control_vks: Vec<u16>,
    qpc_frequency: i64,
}

impl Finalizer {
    #[allow(clippy::too_many_arguments)]
    fn start(
        profile: CaptureProfile,
        target: RecordingTarget,
        session_id: String,
        run_id: String,
        mapping: MappingSnapshot,
        catalog: RecordingCatalogContext,
        consent_id: Option<String>,
        control_vks: Vec<u16>,
        qpc_frequency: i64,
    ) -> anyhow::Result<Self> {
        let (jobs, job_rx) = bounded::<FinalizeJob>(1);
        // Jobs stay strictly bounded to one in flight. Results are tiny control
        // objects; keeping this side non-blocking prevents Drop from deadlocking
        // if the pipeline exits while a diagnostic result is waiting.
        let (result_tx, results) = unbounded::<anyhow::Result<FinalizeResult>>();
        let context = FinalizeContext {
            profile,
            target,
            session_id,
            run_id,
            mapping,
            catalog,
            consent_id,
            control_vks,
            qpc_frequency,
        };
        let join = thread::Builder::new()
            .name("dm-finalize".into())
            .spawn(move || {
                let _com = match ComGuard::mta() {
                    Ok(com) => com,
                    Err(error) => {
                        let _ = result_tx.send(Err(error));
                        return;
                    }
                };
                let mut projector = FrameProjector::with_control_vks(context.control_vks.clone());
                while let Ok(job) = job_rx.recv() {
                    if result_tx
                        .send(finalize_segment(job, &context, &mut projector))
                        .is_err()
                    {
                        return;
                    }
                }
            })
            .context("failed to spawn segment finalizer")?;
        Ok(Self {
            jobs,
            results,
            join: Some(join),
        })
    }

    fn submit(&self, job: FinalizeJob) -> anyhow::Result<()> {
        self.jobs.try_send(job).map_err(|error| match error {
            crossbeam_channel::TrySendError::Full(_) => {
                anyhow::anyhow!("finalize queue is full; recording stopped")
            }
            crossbeam_channel::TrySendError::Disconnected(_) => {
                anyhow::anyhow!("finalize worker stopped before accepting segment")
            }
        })
    }

    fn try_result(&self) -> anyhow::Result<Option<FinalizeResult>> {
        match self.results.try_recv() {
            Ok(result) => result.map(Some),
            Err(crossbeam_channel::TryRecvError::Empty) => Ok(None),
            Err(crossbeam_channel::TryRecvError::Disconnected) => {
                anyhow::bail!("finalize worker exited unexpectedly")
            }
        }
    }

    fn recv_result(&self) -> anyhow::Result<FinalizeResult> {
        self.results
            .recv()
            .context("finalize worker exited unexpectedly")?
    }

    fn close_jobs(&mut self) {
        let (replacement, receiver) = bounded(0);
        drop(receiver);
        drop(std::mem::replace(&mut self.jobs, replacement));
    }

    fn stop(mut self) -> anyhow::Result<()> {
        self.close_jobs();
        if let Some(join) = self.join.take() {
            join.join()
                .map_err(|_| anyhow::anyhow!("finalize worker panicked"))?;
        }
        Ok(())
    }
}

impl Drop for Finalizer {
    fn drop(&mut self) {
        self.close_jobs();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CfrSealTiming {
    normal_delay_ms: i64,
    empty_slot_delay_ms: i64,
}

impl CfrSealTiming {
    const NORMAL: Self = Self {
        normal_delay_ms: SEAL_DELAY_MS,
        empty_slot_delay_ms: SEAL_DELAY_MS,
    };

    fn for_fps(fps_num: u32, fps_den: u32) -> Self {
        Self {
            empty_slot_delay_ms: if is_exactly_30_fps(fps_num, fps_den) {
                16
            } else {
                SEAL_DELAY_MS
            },
            ..Self::NORMAL
        }
    }

    fn deadline(self, slot_end: i64, frequency: i64, has_candidate: bool) -> i64 {
        let delay_ms = if has_candidate {
            self.normal_delay_ms
        } else {
            self.empty_slot_delay_ms
        };
        slot_end + frequency * delay_ms / 1_000
    }
}

fn is_exactly_30_fps(fps_num: u32, fps_den: u32) -> bool {
    fps_den != 0 && u64::from(fps_num) == 30 * u64::from(fps_den)
}

pub fn record_until_stopped(
    request: RecordingRequest,
    controls: &Receiver<RecordingStopSignal>,
    on_event: impl Fn(RecorderEvent) -> anyhow::Result<()>,
) -> anyhow::Result<RecordingOutcome> {
    let seal_timing = CfrSealTiming::for_fps(request.profile.fps_num, request.profile.fps_den);
    record_with_trace(
        request,
        controls,
        on_event,
        CadenceTrace::default(),
        seal_timing,
    )
}

#[cfg(any(test, all(feature = "cadence-trace", debug_assertions)))]
fn cadence_seal_delay_ms(requested: Option<u8>, fps_num: u32, fps_den: u32) -> anyhow::Result<i64> {
    match requested {
        None | Some(4) => Ok(SEAL_DELAY_MS),
        Some(16) if is_exactly_30_fps(fps_num, fps_den) => Ok(16),
        _ => anyhow::bail!("cadence trial supports only 4ms, or 16ms at 30 FPS"),
    }
}

#[cfg(any(test, all(feature = "cadence-trace", debug_assertions)))]
fn cadence_seal_timing(
    requested: Option<u8>,
    empty_slot_grace: bool,
    fps_num: u32,
    fps_den: u32,
) -> anyhow::Result<CfrSealTiming> {
    // No override means exactly the same policy as the ordinary entry point.
    // Some(4) explicitly selects the legacy baseline, independently of tracing.
    if requested.is_none() && !empty_slot_grace {
        return Ok(CfrSealTiming::for_fps(fps_num, fps_den));
    }
    let normal_delay_ms = cadence_seal_delay_ms(requested, fps_num, fps_den)?;
    if empty_slot_grace {
        ensure!(
            normal_delay_ms == SEAL_DELAY_MS && is_exactly_30_fps(fps_num, fps_den),
            "empty-slot grace requires the normal 4ms budget at 30 FPS"
        );
    }
    Ok(CfrSealTiming {
        normal_delay_ms,
        empty_slot_delay_ms: if empty_slot_grace {
            16
        } else {
            normal_delay_ms
        },
    })
}

/// Explicit engineering entry point into the same recorder. No environment or
/// UI switch; unavailable in release builds even if the feature is requested.
#[cfg(all(feature = "cadence-trace", debug_assertions))]
pub fn record_until_stopped_with_cadence_trace(
    request: RecordingRequest,
    controls: &Receiver<RecordingStopSignal>,
    config: CadenceTraceConfig,
    on_event: impl Fn(RecorderEvent) -> anyhow::Result<()>,
) -> anyhow::Result<RecordingOutcome> {
    request.profile.validate()?;
    let seal_timing = cadence_seal_timing(
        config.experimental_seal_delay_ms,
        config.experimental_empty_slot_grace,
        request.profile.fps_num,
        request.profile.fps_den,
    )?;
    ensure!(
        config.raw_watermark.is_none()
            || (config.output_path.is_some()
                && request.present_identity.is_none()
                && request.selected_present_feedback.is_none()
                && config.experimental_seal_delay_ms.is_none()
                && !config.experimental_empty_slot_grace),
        "raw watermark diagnostics require a trace, no formal identity and unchanged CFR timing"
    );
    let Some(output_path) = config.output_path else {
        return record_with_trace(
            request,
            controls,
            on_event,
            CadenceTrace::default(),
            seal_timing,
        );
    };
    // Resolve parents before capture and keep diagnostics out of ready artifacts.
    ensure!(output_path.is_absolute(), "trace path must be absolute");
    let output_parent =
        fs::canonicalize(output_path.parent().context("trace path has no parent")?)?;
    let artifact_root = fs::canonicalize(&request.output_root)?;
    let output_key = output_parent.to_string_lossy().to_lowercase();
    let artifact_key = artifact_root.to_string_lossy().to_lowercase();
    ensure!(
        output_key != artifact_key && !output_key.starts_with(&format!("{artifact_key}\\")),
        "cadence trace must be outside the session artifact root"
    );
    let clock = QpcClock::new()?;
    let sink = crate::cadence_trace::TraceFile::create(
        &output_path,
        config.max_events,
        serde_json::json!({"session_id":request.session_id, "run_id":request.run_id,
            "session_frame_start":request.session_frame_start, "qpc_frequency":clock.frequency(),
            "fps_num":request.profile.fps_num, "fps_den":request.profile.fps_den,
            "seal_delay_ms":seal_timing.normal_delay_ms,
            "empty_slot_seal_delay_ms":seal_timing.empty_slot_delay_ms,
            "experimental_empty_slot_grace":config.experimental_empty_slot_grace,
            "experimental_seal_delay_override":seal_timing != CfrSealTiming::for_fps(request.profile.fps_num, request.profile.fps_den)}),
    )?;
    let sink = sink.with_raw_watermark(config.raw_watermark);
    let result = record_with_trace(request, controls, on_event, sink.trace(), seal_timing);
    // record_inner's CaptureControl is stopped/joined (also on errors) before
    // returning here. finish additionally awaits the handler lifetime: WinRT may
    // still be retiring a callback delegate after the control thread has joined.
    if let Err(error) = sink.finish(result.is_ok()) {
        tracing::error!(error = %error, "cadence trace flush failed; inspect missing/incomplete footer; recording result unchanged");
    }
    result
}

fn record_with_trace(
    mut request: RecordingRequest,
    controls: &Receiver<RecordingStopSignal>,
    on_event: impl Fn(RecorderEvent) -> anyhow::Result<()>,
    trace: CadenceTrace,
    seal_timing: CfrSealTiming,
) -> anyhow::Result<RecordingOutcome> {
    let _com = ComGuard::mta()?;
    request.profile.validate()?;
    if let Some(segment_policy_override) = &request.segment_policy_override {
        segment_policy_override.validate_for(&request.profile)?;
    }
    ensure!(
        !request.session_id.is_empty(),
        "session_id must not be empty"
    );
    ensure!(!request.run_id.is_empty(), "run_id must not be empty");
    validate_catalog_contract_pair(&request)?;
    // consent 通路保留、今天没有来源会填它（docs/100 §3 推论 3）。与 certified 目录
    // 的强制配对随车道删除；带上一个 consent id 不再改变任何分类——产物恒
    // non-trainable，可训练性只能由发布侧人工放行开启。
    if let Some(consent_id) = &request.consent_id {
        ensure!(
            !consent_id.trim().is_empty(),
            "a declared consent id must not be blank"
        );
    }
    ensure!(
        request.mapping.mapping().game.id == request.target.game_id,
        "recording target does not match the selected mapping"
    );
    ensure!(
        request.catalog.game.id == request.target.game_id
            && request.catalog.recording.id == request.profile.profile_id
            && request.catalog.mapping.id == request.mapping.mapping().mapping_id
            && request.catalog.mapping.version == request.mapping.mapping().version
            && request.catalog.mapping.content_hash == request.mapping.content_hash(),
        "recording request catalog identity is inconsistent"
    );
    let clock = QpcClock::new()?;
    let session_dir = request.output_root.join(&request.session_id);
    ensure_session_contract(&session_dir, &request)?;
    let run_dir = session_dir.join("runs").join(&request.run_id);
    ensure_run_directory_available(&run_dir)?;
    let segments_dir = run_dir.join("segments");
    fs::create_dir_all(&segments_dir)
        .with_context(|| format!("failed to create {}", segments_dir.display()))?;

    match record_inner(
        &mut request,
        controls,
        &on_event,
        &run_dir,
        clock,
        &trace,
        seal_timing,
    ) {
        Ok(outcome) => {
            write_json_atomic(&run_dir.join("run.json"), &outcome)?;
            Ok(outcome)
        }
        Err(error) => {
            let failure = serde_json::json!({
                "schema_version": request
                    .catalog
                    .contract
                    .recording_schema_version(request.profile.audio.is_some()),
                "session_id": request.session_id,
                "run_id": request.run_id,
                "mapping": {
                    "id": request.mapping.mapping().mapping_id,
                    "version": request.mapping.mapping().version,
                    "content_hash": request.mapping.content_hash(),
                },
                "catalog": &request.catalog,
                "consent_id": &request.consent_id,
                "state": "failed",
                "error": sanitize_diagnostic(&format!("{error:#}"), &request.output_root, &run_dir),
            });
            let mut failure = failure;
            // legacy 的两个训练声明字节保持原样；中性契约一个都不写（§B）。
            if request.catalog.contract.declares_training_classification() {
                failure["non_trainable"] = serde_json::json!(true);
                failure["production_certified"] = serde_json::json!(false);
            }
            write_json_atomic(&run_dir.join("failure.json"), &failure)?;
            Err(error.context("recording failed; local diagnostics were retained"))
        }
    }
}

/// 目录上下文必须与它自称的摄取契约自洽。
///
/// 断代前这里核对的是"mode 是不是 `dev_non_trainable`"——那正是把渠道、运行环境与
/// 训练资格粘在一起的那个词（issue #855 §2）。现在核对的是同一件事的正确形状：
/// legacy 契约**必须**带着它那两个 legacy 字节，中性契约**必须**一个都不带。
///
/// 两个方向都拦是有意的。只拦前半条的话，"忘了清空"会静默产出一份自称中性、却仍在
/// 申报训练分类的会话；服务端的 `retiredClassificationKeys` 会在上传时把它 403，
/// 而那时段已经录完了。
fn validate_catalog_contract_pair(request: &RecordingRequest) -> anyhow::Result<()> {
    ensure!(
        request.catalog.mode.as_deref() == request.catalog.contract.legacy_catalog_mode()
            && request.catalog.non_trainable
                == request
                    .catalog
                    .contract
                    .declares_training_classification()
                    .then_some(true),
        "catalog context does not match its ingest contract: contract={:?} mode={:?} non_trainable={:?}",
        request.catalog.contract,
        request.catalog.mode,
        request.catalog.non_trainable
    );
    // 采集档案**永远** non-trainable，与契约无关：可训练性只能由发布侧的人工放行
    // 开启，客户端与录制链路的任何位置都不得主张它（AGENTS.md、docs/100）。
    // 这个布尔还不只是标签——它选的是 source geometry 规则（profile.rs
    // `source_geometry_rule`），翻成 false 会静默把几何政策从 AtLeast 换成 Exactly。
    ensure!(
        request.profile.non_trainable,
        "a recording profile must always be non-trainable; the client never claims trainability"
    );
    Ok(())
}

fn ensure_run_directory_available(run_dir: &Path) -> anyhow::Result<()> {
    if !run_dir.exists() {
        return Ok(());
    }

    // The optional depth probe must prove Ready before the RGB recorder starts,
    // so it can legitimately create the run's live `depth-lab` sidecar first.
    // Nothing else may pre-exist: accepting a recording artifact here would
    // weaken the immutable-run contract and could overwrite evidence from an
    // earlier attempt.
    let mut entries = fs::read_dir(run_dir)
        .with_context(|| format!("failed to inspect prepared run {}", run_dir.display()))?;
    let Some(entry) = entries.next().transpose()? else {
        anyhow::bail!("refusing to overwrite immutable run {}", run_dir.display());
    };
    let metadata = fs::symlink_metadata(entry.path())?;
    let prepared_depth_sidecar = entry.file_name() == "depth-lab"
        && metadata.file_type().is_dir()
        && !metadata.file_type().is_symlink()
        && entries.next().transpose()?.is_none();
    ensure!(
        prepared_depth_sidecar,
        "refusing to overwrite immutable run {}",
        run_dir.display()
    );
    Ok(())
}

/// Proves the production GPU conversion + hardware-only Media Foundation path
/// without creating a session or run directory. The caller persists the
/// returned facts as preflight evidence before starting capture.
pub fn probe_hardware_path(
    profile: &CaptureProfile,
    scratch_root: impl AsRef<Path>,
) -> anyhow::Result<HardwarePreflightEvidence> {
    let profile = profile.clone();
    let scratch_root = scratch_root.as_ref().to_path_buf();
    thread::Builder::new()
        .name("dm-hardware-preflight".into())
        .spawn(move || probe_hardware_path_mta(&profile, &scratch_root))
        .context("failed to spawn hardware preflight worker")?
        .join()
        .map_err(|_| anyhow::anyhow!("hardware preflight worker panicked"))?
}

fn probe_hardware_path_mta(
    profile: &CaptureProfile,
    scratch_root: &Path,
) -> anyhow::Result<HardwarePreflightEvidence> {
    let _com = ComGuard::mta()?;
    profile.validate()?;
    let windows = WindowsVersion::current()?;
    windows.require_hardware_only_media_foundation()?;
    ensure_plain_scratch_root(scratch_root)?;
    let sequence = PREFLIGHT_SCRATCH_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let scratch_path = scratch_root.join(format!(
        "hardware-preflight-{}-{sequence:016x}.mp4",
        std::process::id()
    ));
    let scratch = ScratchArtifact::new(scratch_path);
    let d3d = D3dContext::create_nvidia_hardware_only()?;
    let crop_right = i32::try_from(profile.width).context("preflight width exceeds i32")?;
    let crop_bottom = i32::try_from(profile.height).context("preflight height exceeds i32")?;
    let converter = BgraToNv12::new(
        &d3d.device,
        &d3d.immediate_context,
        VideoConversionSpec {
            surface_width: profile.width,
            surface_height: profile.height,
            crop: crate::crop::CropRect {
                left: 0,
                top: 0,
                right: crop_right,
                bottom: crop_bottom,
            },
            output_width: profile.width,
            output_height: profile.height,
            fps_num: profile.fps_num,
            fps_den: profile.fps_den,
        },
    )?;
    let nv12_textures = (0..2)
        .map(|_| create_nv12_texture(&d3d.device, profile.width, profile.height))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let _mf = MfPlatform::startup()?;
    let (effective_encoder, media_probe) = probe_hardware_path_with_context(
        &converter,
        &d3d.device,
        &d3d.immediate_context,
        &nv12_textures,
        profile.width,
        profile.height,
        profile,
        scratch.path(),
    )?;
    scratch.remove()?;
    Ok(HardwarePreflightEvidence {
        schema_version: "dm-hardware-preflight-1",
        scope: "gpu_encoder_path_no_window_capture",
        wgc_frame_observed: false,
        windows,
        adapter_description: d3d.adapter.description.clone(),
        adapter_vendor_id: d3d.adapter.vendor_id,
        adapter_device_id: d3d.adapter.device_id,
        adapter_luid_high: d3d.adapter.luid_high,
        adapter_luid_low: d3d.adapter.luid_low,
        adapter_driver_version: Some(d3d.adapter.driver_version.clone()),
        d3d_feature_level: d3d.adapter.feature_level,
        encoder: HardwareEncoderPreflightEvidence {
            transform_clsid: effective_encoder.transform_clsid,
            friendly_name: effective_encoder.friendly_name,
            control_policy: effective_encoder.control_policy,
            relaxed_controls: effective_encoder.relaxed_controls,
            rate_control_mode: effective_encoder.rate_control_mode,
            mean_bitrate_bps: effective_encoder.mean_bitrate_bps,
            max_bitrate_bps: effective_encoder.max_bitrate_bps,
            gop_frames: effective_encoder.gop_frames,
            b_frames: effective_encoder.b_frames,
            low_latency: effective_encoder.low_latency,
        },
        profile: HardwareCaptureProfileEvidence {
            profile_id: profile.profile_id.clone(),
            width: profile.width,
            height: profile.height,
            fps_num: profile.fps_num,
            fps_den: profile.fps_den,
        },
        media_probe,
        gpu_conversion: "d3d11_video_processor_bgra_to_nv12",
        scratch_artifact_retained: false,
    })
}

struct ScratchArtifact {
    path: PathBuf,
}

impl ScratchArtifact {
    fn new(path: PathBuf) -> Self {
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn remove(self) -> anyhow::Result<()> {
        if self.path.exists() {
            fs::remove_file(&self.path).with_context(|| {
                format!(
                    "failed to remove hardware preflight scratch artifact {}",
                    self.path.display()
                )
            })?;
        }
        remove_empty_scratch_parent(&self.path)?;
        Ok(())
    }
}

impl Drop for ScratchArtifact {
    fn drop(&mut self) {
        if self.path.exists()
            && let Err(error) = fs::remove_file(&self.path)
        {
            tracing::warn!(
                path = %self.path.display(),
                error = %error,
                "failed to clean hardware preflight scratch artifact"
            );
        }
        if let Err(error) = remove_empty_scratch_parent(&self.path) {
            tracing::warn!(error = %error, "failed to clean empty hardware preflight scratch directory");
        }
    }
}

fn remove_empty_scratch_parent(path: &Path) -> anyhow::Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    if parent.file_name().and_then(|name| name.to_str()) != Some(".preflight-scratch") {
        return Ok(());
    }
    match fs::remove_dir(parent) {
        Ok(()) => Ok(()),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty
            ) =>
        {
            Ok(())
        }
        Err(error) => Err(error).with_context(|| {
            format!(
                "failed to remove empty hardware preflight scratch directory {}",
                parent.display()
            )
        }),
    }
}

fn ensure_plain_scratch_root(scratch_root: &Path) -> anyhow::Result<()> {
    ensure!(
        scratch_root.file_name().and_then(|name| name.to_str()) == Some(".preflight-scratch"),
        "hardware preflight scratch must use the fixed .preflight-scratch directory"
    );
    if let Ok(metadata) = fs::symlink_metadata(scratch_root) {
        ensure!(
            metadata.is_dir() && metadata.file_attributes() & 0x400 == 0,
            "hardware preflight scratch path is not a plain directory"
        );
    }
    fs::create_dir_all(scratch_root).with_context(|| {
        format!(
            "failed to create hardware preflight scratch directory {}",
            scratch_root.display()
        )
    })?;
    let metadata = fs::symlink_metadata(scratch_root)?;
    ensure!(
        metadata.is_dir() && metadata.file_attributes() & 0x400 == 0,
        "hardware preflight scratch path became a reparse point"
    );
    Ok(())
}

fn should_force_keyframe(
    session_frame_start: u64,
    session_frame: u64,
    local_frame: u64,
    gop_frames: u32,
) -> anyhow::Result<bool> {
    ensure!(gop_frames > 0, "GOP frame count must be greater than zero");
    let global_session_frame = session_frame_start
        .checked_add(session_frame)
        .context("session frame index overflow while scheduling keyframe")?;
    Ok(local_frame == 0 || global_session_frame % u64::from(gop_frames) == 0)
}

fn keyframe_requires_encoder_barrier(local_frame: u64, force_keyframe: bool) -> bool {
    // Segment transitions already drain every previously submitted frame before
    // the muxer is replaced. A GOP boundary inside the active segment needs the
    // same guarantee: AVEncVideoForceKeyFrame targets the next frame received by
    // the encoder MFT, while Sink Writer may still have an earlier app sample in
    // its asynchronous queue.
    force_keyframe && local_frame != 0
}

/// An empty poll/timeout is not a durable statement about the queue. A callback
/// can publish while the consumer resumes or checks source health. Recheck once
/// at the seal boundary; never wait again or change source timestamp membership.
fn finish_empty_cfr_wake(
    frames: &Receiver<CapturedFrame>,
    check_health: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<Option<CapturedFrame>> {
    check_health()?;
    match frames.try_recv() {
        Ok(frame) => Ok(Some(frame)),
        Err(crossbeam_channel::TryRecvError::Empty) => Ok(None),
        Err(crossbeam_channel::TryRecvError::Disconnected) => {
            anyhow::bail!("WGC callback channel disconnected before CFR seal")
        }
    }
}

fn record_inner(
    request: &mut RecordingRequest,
    controls: &Receiver<RecordingStopSignal>,
    on_event: &impl Fn(RecorderEvent) -> anyhow::Result<()>,
    run_dir: &Path,
    clock: QpcClock,
    trace: &CadenceTrace,
    seal_timing: CfrSealTiming,
) -> anyhow::Result<RecordingOutcome> {
    if request.selected_depth_cadence
        == present_identity::feedback::DepthSelectionCadence::EvenCfrFrames
    {
        ensure!(
            cfg!(all(feature = "cadence-trace", debug_assertions))
                && request.selected_present_feedback.is_some()
                && request.session_frame_start == 0
                && request.session_id.starts_with("raw-rgb-")
                && request.target.game_id == "cyberpunk-2077-steam"
                && request.profile.width == 1920
                && request.profile.height == 1080
                && is_exactly_30_fps(request.profile.fps_num, request.profile.fps_den),
            "even-CFR depth requires the explicit 30 FPS CP2077 raw diagnostic entry"
        );
    }
    let mut selected_feedback = request.selected_present_feedback.take().map(|transport| {
        let mut feedback = RecorderFeedback::new(transport);
        feedback.cadence = request.selected_depth_cadence;
        feedback
    });
    // 这一代由 session 创建时定死的目录上下文给出，run 中途不会变（`ensure_session_contract`
    // 的整体相等比较让换代在构造上不可能发生）。
    let contract = request.catalog.contract;
    let profile = &request.profile;
    let input_writer = RawEventWriter::start(
        request.input_records.clone(),
        request.input_control.clone(),
        request.input_status.clone(),
        run_dir.join("spool"),
        request.run_id.clone(),
        clock.frequency(),
    )?;
    let mut audio_writer = profile.audio.as_ref().map(|_| {
        AudioSegmentWriter::start(request.target.process_id, clock, run_dir.join("spool"))
    });
    WindowsVersion::current()?.require_hardware_only_media_foundation()?;
    let hwnd = HWND(request.target.hwnd_value as *mut _);
    let d3d = D3dContext::create_nvidia_hardware_only()?;
    let crop = client_area_crop(hwnd)?;
    let (minimum_source_width, minimum_source_height) = profile.source_geometry_rule().dimensions();
    ensure!(
        profile.accepts_source_dimensions(crop.width(), crop.height()),
        "target client area {}x{} does not satisfy the source geometry policy (minimum {}x{}, output {}x{})",
        crop.width(),
        crop.height(),
        minimum_source_width,
        minimum_source_height,
        profile.width,
        profile.height
    );
    let (surface_width, surface_height) = capture_surface_size(hwnd)?;
    crop.validate_for_surface(surface_width, surface_height)?;
    let converter = BgraToNv12::new(
        &d3d.device,
        &d3d.immediate_context,
        VideoConversionSpec {
            surface_width,
            surface_height,
            crop,
            output_width: profile.width,
            output_height: profile.height,
            fps_num: profile.fps_num,
            fps_den: profile.fps_den,
        },
    )?;
    let nv12_textures = (0..NV12_POOL_SIZE)
        .map(|_| create_nv12_texture(&d3d.device, profile.width, profile.height))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let mut free_nv12 = (1..NV12_POOL_SIZE).collect::<VecDeque<_>>();

    let _mf = MfPlatform::startup()?;

    let final_segment_dir = |index: u32| run_dir.join("segments").join(format!("{index:05}"));
    let staging_segment_dir =
        |index: u32| run_dir.join("segments").join(format!(".{index:05}.tmp"));
    fs::create_dir_all(staging_segment_dir(0))?;
    fs::create_dir_all(staging_segment_dir(1))?;
    let mut encoder = HardwareH264Encoder::create(&d3d.device, profile)?;
    let effective_encoder = encoder.effective_encoder().clone();
    let mut current = H264Mp4Writer::create(staging_segment_dir(0).join("video.mp4"), profile)?;
    let mut next = Some((
        1,
        H264Mp4Writer::create(staging_segment_dir(1).join("video.mp4"), profile)?,
    ));
    let finalizer = Finalizer::start(
        profile.clone(),
        request.target.clone(),
        request.session_id.clone(),
        request.run_id.clone(),
        request.mapping.clone(),
        request.catalog.clone(),
        request.consent_id.clone(),
        request.control_vks.clone(),
        clock.frequency(),
    )?;

    let watermark_config = request
        .present_identity
        .map(|session| {
            let (roi_width, roi_height) = scaled_watermark_geometry(
                crop.width(),
                crop.height(),
                session.watermark_source_width,
                session.watermark_source_height,
            )?;
            Ok::<_, anyhow::Error>(WatermarkCaptureConfig {
                roi_left: u32::try_from(crop.left).expect("validated non-negative crop left"),
                roi_top: u32::try_from(crop.top).expect("validated non-negative crop top"),
                roi_width,
                roi_height,
                decode_margin: DEFAULT_WATERMARK_DECODE_MARGIN,
            })
        })
        .transpose()?;
    let watermark_config = if let Some(raw) = trace.raw_watermark() {
        let (roi_width, roi_height) = scaled_watermark_geometry(
            crop.width(),
            crop.height(),
            raw.source_width,
            raw.source_height,
        )?;
        // A bounded diagnostic ROI only; never allocate a frame-sized readback.
        ensure!(
            roi_width <= 1024 && roi_height <= 64,
            "diagnostic watermark ROI exceeds bound"
        );
        Some(WatermarkCaptureConfig {
            roi_left: u32::try_from(crop.left)?,
            roi_top: u32::try_from(crop.top)?,
            roi_width,
            roi_height,
            decode_margin: DEFAULT_WATERMARK_DECODE_MARGIN,
        })
    } else {
        watermark_config
    };
    let mut ingress = CaptureIngress::with_cadence_trace(clock, watermark_config, trace.clone());
    let flags = ingress.take_flags()?;
    let settings = WgcSettings {
        window: Window::from_raw_hwnd(hwnd.0),
        device: d3d.device.clone(),
        device_context: d3d.immediate_context.clone(),
        // The OS cursor is composed after the game's backbuffer and can cover
        // the fixed watermark ROI without being part of the source Present.
        // Exact identity runs therefore exclude it; ordinary RGB capture keeps
        // the historical cursor behavior.
        cursor_capture: request.present_identity.is_none(),
        draw_border: false,
        include_secondary_windows: false,
        minimum_update_interval: Duration::from_nanos(
            (1_000_000_000_u64 * u64::from(profile.fps_den))
                / (u64::from(profile.fps_num)
                    * u64::try_from(WGC_SOURCE_OBSERVATIONS_PER_CFR_FRAME)?),
        ),
        flags,
    };
    let mut capture = Some(
        CaptureControl::<GpuCopyHandler>::start(settings)
            .map_err(|error| anyhow::anyhow!("WGC HWND capture failed: {error}"))?,
    );
    let bgra_textures = textures_after_start(&ingress)?.to_vec();
    let copy_queries = copy_queries_after_start(&ingress)?.to_vec();
    let watermark_textures = watermark_config
        .map(|_| watermark_textures_after_start(&ingress).map(<[_]>::to_vec))
        .transpose()?;
    ensure!(
        bgra_textures.len() == copy_queries.len(),
        "GPU-copy texture/query ring length mismatch"
    );
    if let Some(textures) = &watermark_textures {
        ensure!(
            textures.len() == bgra_textures.len(),
            "watermark ROI/BGRA ring length mismatch"
        );
    }
    let mut present_decoder = request
        .present_identity
        .map(|session| {
            present_identity::FullSequenceDecoder::new(
                session.generation,
                session.first_present_seq,
                session.unwrap_accept_budget,
            )
        })
        .transpose()
        .context("invalid online Present identity baseline/budget")?;
    let mut watermark_observation_stats = WatermarkObservationStats::default();
    let arm_required = selected_feedback.is_some();
    let mut arm_started = !arm_required;
    let mut arm_boundary = None;
    let video_surfaces = converter.prepare_surfaces(&bgra_textures, &nv12_textures)?;
    let mut source_desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { bgra_textures[0].GetDesc(&mut source_desc) };
    ensure!(
        source_desc.Width == surface_width && source_desc.Height == surface_height,
        "WGC surface geometry changed between preflight and capture start"
    );

    let mut stop = None;
    let first_timeout = WGC_FIRST_FRAME_TIMEOUT;
    let first_deadline = Instant::now() + first_timeout;
    let mut startup_wgc_frames_seen = 0_u64;
    let first = loop {
        if arm_started && arm_required && arm_boundary.is_none() {
            arm_boundary = selected_feedback
                .as_mut()
                .context("selected-Present feedback disappeared while arming")?
                .poll_arm_boundary()?;
        }
        let remaining = first_deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            let phase = if startup_wgc_frames_seen == 0 {
                "first_wgc_frame"
            } else if arm_required && !arm_started {
                "first_decodable_wgc_present"
            } else if arm_required && arm_boundary.is_none() {
                "generation_armed_ack"
            } else {
                "first_post_arm_wgc_frame"
            };
            anyhow::bail!(
                "timed out waiting for WGC/selected-Present startup phase={phase} after {} ms: wgc_frames_seen={startup_wgc_frames_seen} watermark_decoded={} watermark_rejected={} watermark_integrity_mismatch={} watermark_ambiguous={} watermark_stale={} watermark_geometry={:?} arm_sent={} generation_armed={} arm_boundary={arm_boundary:?}",
                first_timeout.as_millis(),
                watermark_observation_stats.decoded,
                watermark_observation_stats.rejected,
                watermark_observation_stats.integrity_mismatch,
                watermark_observation_stats.ambiguous,
                watermark_observation_stats.stale,
                watermark_config.map(|config| (
                    request.present_identity.map(|s| s.watermark_source_width),
                    request.present_identity.map(|s| s.watermark_source_height),
                    crop.width(),
                    crop.height(),
                    config.roi_width,
                    config.roi_height,
                )),
                arm_required && arm_started,
                arm_boundary.is_some(),
            );
        }
        let poll_wait = remaining.min(Duration::from_millis(2));
        let (next, stopped) = select! {
            recv(controls) -> signal => {
                if let Ok(signal) = signal {
                    observe_stop(&request.cutoff_qpc, &mut stop, signal);
                }
                (None, true)
            }
            recv(ingress.frames) -> frame => {
                (Some(frame.context("WGC callback channel disconnected before first frame")?), false)
            }
            default(poll_wait) => {
                (None, false)
            }
        };
        if stopped {
            break None;
        }
        let Some(mut frame) = next else {
            continue;
        };
        trace.emit(clock, || {
            CadenceEvent::source(
                SourceStage::Dequeued,
                (&frame).into(),
                None,
                ingress.frames.len(),
            )
        });
        startup_wgc_frames_seen = startup_wgc_frames_seen.saturating_add(1);
        wait_for_gpu_copy_completion(
            &d3d.immediate_context,
            &copy_queries[frame.slot],
            &clock,
            &ingress.metrics,
            frame.callback_start_qpc,
            gpu_copy_wait_limit_qpc(profile.fps_num, profile.fps_den, clock.frequency()),
        )?;
        trace.emit(clock, || {
            CadenceEvent::source(
                SourceStage::GpuReady,
                (&frame).into(),
                None,
                ingress.frames.len(),
            )
        });
        if let (Some(textures), Some(config)) = (watermark_textures.as_ref(), watermark_config) {
            trace.observe_raw_watermark(clock, (&frame).into(), || {
                decode_watermark_texture(
                    &d3d.immediate_context,
                    &textures[frame.slot],
                    config.decode_margin,
                )
            });
        }
        if let (Some(decoder), Some(textures), Some(config)) = (
            present_decoder.as_mut(),
            watermark_textures.as_ref(),
            watermark_config,
        ) {
            let Some(raw) = watermark_observation_stats.classify(
                frame.source_seq,
                decode_watermark_texture(
                    &d3d.immediate_context,
                    &textures[frame.slot],
                    config.decode_margin,
                ),
            )?
            else {
                trace.emit(clock, || {
                    CadenceEvent::source(
                        SourceStage::WatermarkRejected,
                        (&frame).into(),
                        None,
                        ingress.frames.len(),
                    )
                });
                ingress.recycle.send(frame.slot)?;
                continue;
            };
            let Some(present_seq) = watermark_observation_stats.expand(
                frame.source_seq,
                decoder,
                request
                    .present_identity
                    .expect("formal decoder requires identity session")
                    .generation,
                raw,
            )?
            else {
                trace.emit(clock, || {
                    CadenceEvent::source(
                        SourceStage::WatermarkStale,
                        (&frame).into(),
                        None,
                        ingress.frames.len(),
                    )
                });
                ingress.recycle.send(frame.slot)?;
                continue;
            };
            frame.present_seq = Some(present_seq);
            trace.emit(clock, || {
                CadenceEvent::source(
                    SourceStage::WatermarkReady,
                    (&frame).into(),
                    None,
                    ingress.frames.len(),
                )
            });
        }
        match classify_startup_observation(
            frame.present_seq,
            arm_required,
            arm_started,
            arm_boundary,
        )? {
            StartupObservationDisposition::ProveWgcLiveThenArm => {
                trace.emit(clock, || {
                    CadenceEvent::source(
                        SourceStage::StartupArm,
                        (&frame).into(),
                        None,
                        ingress.frames.len(),
                    )
                });
                // This decoded observation proves WGC is live, but remains
                // pre-arm and therefore cannot become a CFR source.
                ingress.recycle.send(frame.slot)?;
                selected_feedback
                    .as_mut()
                    .context("selected-Present feedback disappeared before Arm")?
                    .begin_arm()?;
                arm_started = true;
            }
            StartupObservationDisposition::Recycle => {
                trace.emit(clock, || {
                    CadenceEvent::source(
                        SourceStage::BeforeArm,
                        (&frame).into(),
                        None,
                        ingress.frames.len(),
                    )
                });
                // Keep the bounded WGC callback queue draining while the Arm
                // ACK is pending or old observations trail the boundary.
                ingress.recycle.send(frame.slot)?;
            }
            StartupObservationDisposition::Eligible => {
                trace.emit(clock, || {
                    CadenceEvent::source(
                        SourceStage::StartupEligible,
                        (&frame).into(),
                        None,
                        ingress.frames.len(),
                    )
                });
                break Some(frame);
            }
        }
    };

    let Some(first) = first else {
        capture
            .take()
            .context("capture control disappeared")?
            .stop()
            .map_err(|error| anyhow::anyhow!("WGC stop failed: {error}"))?;
        drop(current);
        let initial_staging = staging_segment_dir(0);
        if initial_staging.exists() {
            fs::remove_dir_all(&initial_staging).with_context(|| {
                format!(
                    "failed to remove unused staging muxer {}",
                    initial_staging.display()
                )
            })?;
        }
        cleanup_unused_muxer(&mut next, &staging_segment_dir)?;
        finalizer.stop()?;
        let abort = request.input_control.request_abort()?;
        wait_input_ack(&abort, "abort")?;
        input_writer.stop()?;
        if let Some(audio_writer) = audio_writer.take() {
            audio_writer.abort();
            audio_writer.stop();
        }
        drain_stop_signals(controls, &request.cutoff_qpc, &mut stop);
        if let Some(feedback) = selected_feedback.as_mut() {
            feedback.seal(None)?;
        }
        let stop = stop.unwrap_or(RecordingStopSignal {
            reason: RecordingStopReason::Manual,
            cutoff_qpc: clock.now()?,
        });
        return Ok(RecordingOutcome {
            schema_version: contract.recording_schema_version(profile.audio.is_some()),
            state: contract.run_complete_state(),
            non_trainable: contract.declares_training_classification().then_some(true),
            consent_id: request.consent_id.clone(),
            production_certified: contract.declares_training_classification().then_some(false),
            input_status: "raw_input_v1",
            session_id: request.session_id.clone(),
            run_id: request.run_id.clone(),
            mapping_id: request.mapping.mapping().mapping_id.clone(),
            mapping_version: request.mapping.mapping().version.clone(),
            mapping_content_hash: request.mapping.content_hash().to_owned(),
            session_frame_start: request.session_frame_start,
            session_frame_end: request.session_frame_start,
            run_directory: run_dir.to_path_buf(),
            frame_count: 0,
            segment_count: 0,
            duplicate_frames: 0,
            source_superseded: 0,
            wgc_callback_count: ingress.metrics.callbacks.load(Ordering::Relaxed),
            watermark_decoded_observations: watermark_observation_stats.decoded,
            watermark_rejected_observations: watermark_observation_stats.rejected,
            watermark_integrity_rejections: watermark_observation_stats.integrity_mismatch,
            watermark_ambiguous_rejections: watermark_observation_stats.ambiguous,
            max_consecutive_watermark_rejections: watermark_observation_stats.max_consecutive,
            watermark_stale_observations: watermark_observation_stats.stale,
            max_watermark_stale_lag_presents: watermark_observation_stats.max_stale_lag,
            input_event_count: 0,
            input_queue_overflows: 0,
            anchor_qpc: None,
            anchor_at_utc: None,
            qpc_frequency: clock.frequency(),
            qpc_start: None,
            qpc_end: None,
            ended_at_utc: None,
            stop_reason: stop.reason,
        });
    };

    let anchor_qpc = first.source_qpc;
    // 紧挨着 anchor 取一次配对读数，好让投影距离保持在毫秒量级。
    let wall_clock_anchor = WallClockSample::take(clock)?;
    // Started 事件与最终 run.json 必须复用同一次配对读数的同一个投影结果。
    // 若在两处重新读墙钟，增量 DataModel 与最终回放就会拿到不同的字节。
    let anchor_at_utc = wall_clock_anchor.iso8601_at(anchor_qpc, clock.frequency());
    let expected_per_segment = profile.expected_frames_per_segment();
    let first_segment_end_qpc =
        slot_qpc(anchor_qpc, expected_per_segment, clock.frequency(), profile);
    let armed = request
        .input_control
        .request_arm(anchor_qpc, first_segment_end_qpc)?;
    wait_input_ack(&armed, "arming")?;
    input_writer.check()?;
    on_event(RecorderEvent::Started {
        anchor_qpc,
        anchor_at_utc: anchor_at_utc.clone(),
        qpc_frequency: clock.frequency(),
    })?;
    on_event(RecorderEvent::SegmentStarted {
        segment_index: 0,
        session_frame_start: request.session_frame_start,
    })?;
    let mut future = Some(first);
    let mut last_source_qpc = anchor_qpc;
    let mut last_callback_start_qpc = first.callback_start_qpc;
    let mut last_callback_ready_qpc = first.callback_ready_qpc;
    let mut last_source_seq = 0_u64;
    let mut last_present_seq = None;
    let mut have_canonical = false;
    let mut timing: Vec<FrameTiming> = Vec::with_capacity(expected_per_segment as usize);
    let mut summaries = Vec::new();
    let mut active_segment = 0_u32;
    let mut session_frame = 0_u64;
    let mut encoded_timeline = EncodedTimeline::default();
    let mut duplicates = 0_u64;
    let mut superseded_total = 0_u64;
    let mut next_progress_qpc = anchor_qpc
        .checked_add(clock.frequency())
        .context("first progress QPC deadline overflowed")?;

    'recording: loop {
        input_writer.check()?;
        drain_stop_signals(controls, &request.cutoff_qpc, &mut stop);
        if request.cutoff_qpc.load(Ordering::Acquire)
            <= slot_qpc(anchor_qpc, session_frame, clock.frequency(), profile)
        {
            break;
        }
        while let Some(result) = finalizer.try_result()? {
            let segment = PublishedSegment::from(&result.summary);
            accept_finalize_result(result, &mut summaries, &mut next)?;
            on_event(RecorderEvent::SegmentReady {
                segment,
                ready_segments: summaries.len() as u32,
            })?;
        }
        drain_encoded_samples(&encoder, &mut current, &mut encoded_timeline, profile)?;

        if timing.len() as u64 == expected_per_segment {
            wait_for_encoder_barrier(
                EncoderOutput {
                    encoder: &encoder,
                    muxer: &mut current,
                    timeline: &mut encoded_timeline,
                },
                session_frame,
                profile,
                clock,
                &ingress,
                "segment tail",
            )?;
            let (prepared_index, prepared) = next
                .take()
                .context("next MP4 muxer was not prepared before segment boundary")?;
            ensure!(
                prepared_index == active_segment + 1,
                "prepared MP4 muxer index mismatch"
            );
            let old = std::mem::replace(&mut current, prepared);
            let completed_index = active_segment;
            let completed_qpc_end = timing
                .last()
                .context("completed segment has no timing rows")?
                .output_qpc_end;
            let next_qpc_end = slot_qpc(
                anchor_qpc,
                session_frame + expected_per_segment,
                clock.frequency(),
                profile,
            );
            let event_seal = request.input_control.request_seal(
                completed_index,
                completed_qpc_end,
                next_qpc_end,
            )?;
            let completed_qpc_start = timing
                .first()
                .context("completed segment has no timing rows")?
                .output_qpc_start;
            let audio_seal = audio_writer.as_ref().map(|writer| {
                writer.request_seal(completed_index, completed_qpc_start, completed_qpc_end)
            });
            let completed_frame_count = timing.len() as u64;
            let completed_session_frame_start = request
                .session_frame_start
                .checked_add(session_frame.saturating_sub(completed_frame_count))
                .context("completed segment session frame start overflow")?;
            on_event(RecorderEvent::SegmentFinalizing {
                segment_index: completed_index,
                session_frame_start: completed_session_frame_start,
                frame_count: completed_frame_count,
            })?;
            active_segment += 1;
            on_event(RecorderEvent::SegmentStarted {
                segment_index: active_segment,
                session_frame_start: request
                    .session_frame_start
                    .checked_add(session_frame)
                    .context("active segment session frame start overflow")?,
            })?;
            let prepare_index = active_segment + 1;
            // H264Mp4Writer::create owns creation of the standby muxer's parent
            // directory on the finalizer thread. Doing the same filesystem call
            // here needlessly stalls the WGC consumer at every segment boundary.
            finalizer.submit(FinalizeJob {
                index: completed_index,
                writer: old,
                timing: std::mem::take(&mut timing),
                staging_dir: staging_segment_dir(completed_index),
                final_dir: final_segment_dir(completed_index),
                prepare: Some((
                    prepare_index,
                    staging_segment_dir(prepare_index).join("video.mp4"),
                )),
                effective_encoder: effective_encoder.clone(),
                events: event_seal,
                audio: audio_seal,
            })?;
        }

        let local_frame = timing.len() as u64;
        let slot_start = slot_qpc(anchor_qpc, session_frame, clock.frequency(), profile);
        let slot_end = slot_qpc(anchor_qpc, session_frame + 1, clock.frequency(), profile);
        let mut selector = CfrSlotSelector::default();
        let mut trace_slot = CadenceSlot {
            run_frame: session_frame,
            start_qpc: slot_start,
            end_qpc: slot_end,
            seal_qpc: seal_timing.deadline(slot_end, clock.frequency(), false),
        };
        trace.emit(clock, || CadenceEvent::SlotOpen {
            slot: trace_slot,
            queue_depth: ingress.frames.len(),
        });
        let mut seal_reason = SealReason::FutureSource;

        loop {
            // Only a GPU/watermark/Arm-validated candidate ends the empty-slot
            // grace. Once it exists, never wait longer than the normal budget;
            // the ingress poll still drains already queued observations first.
            let seal_qpc =
                seal_timing.deadline(slot_end, clock.frequency(), selector.has_candidate());
            trace_slot.seal_qpc = seal_qpc;
            let (mut frame, mut newly_received) = if let Some(frame) = future.take() {
                (Some(frame), false)
            } else {
                let now = clock.now()?;
                let polled = poll_cfr_ingress(&ingress.frames, now, seal_qpc)
                    .context("WGC callback channel disconnected")?;
                trace.emit(clock, || CadenceEvent::Poll {
                    slot: trace_slot,
                    before_qpc: now,
                    result: match &polled {
                        CfrIngressPoll::Frame(_) => "frame",
                        CfrIngressPoll::DeadlineReached => "deadline",
                        CfrIngressPoll::Wait => "wait",
                    },
                    queue_depth: ingress.frames.len(),
                });
                match polled {
                    CfrIngressPoll::Frame(frame) => (Some(frame), true),
                    CfrIngressPoll::DeadlineReached => {
                        seal_reason = SealReason::QueueEmptyAtDeadline;
                        (None, false)
                    }
                    CfrIngressPoll::Wait => {
                        let wait = Duration::from_secs_f64(
                            (seal_qpc - now) as f64 / clock.frequency() as f64,
                        );
                        select! {
                            recv(controls) -> signal => {
                                seal_reason = SealReason::Control;
                                if let Ok(signal) = signal {
                                    observe_stop(&request.cutoff_qpc, &mut stop, signal);
                                }
                                (None, false)
                            }
                            recv(ingress.frames) -> frame => {
                                (Some(frame.context("WGC callback channel disconnected")?), true)
                            }
                            default(wait) => {
                                seal_reason = SealReason::WaitTimeout;
                                (None, false)
                            }
                        }
                    }
                }
            };
            if let Some(frame) = &frame {
                trace.emit(clock, || {
                    CadenceEvent::source(
                        if newly_received {
                            SourceStage::Dequeued
                        } else {
                            SourceStage::ReusedFuture
                        },
                        frame.into(),
                        Some(session_frame),
                        ingress.frames.len(),
                    )
                });
            }
            if frame.is_none() && request.cutoff_qpc.load(Ordering::Acquire) > slot_end {
                let mut recheck_before_qpc = None;
                frame = finish_empty_cfr_wake(&ingress.frames, || {
                    let drops = ingress.metrics.drops.load(Ordering::Relaxed);
                    ensure!(
                        drops == 0,
                        "WGC callback queue dropped {drops} frame(s) with capacity {CAPTURE_QUEUE_CAPACITY} and max observed depth {}",
                        ingress.metrics.max_queue_depth.load(Ordering::Relaxed)
                    );
                    if capture.as_ref().is_some_and(CaptureControl::is_finished) {
                        let result = capture
                            .take()
                            .context("capture control disappeared")?
                            .wait();
                        match result {
                            Ok(()) => anyhow::bail!("WGC control thread stopped unexpectedly"),
                            Err(error) => anyhow::bail!("WGC stopped with a fatal error: {error}"),
                        }
                    }
                    recheck_before_qpc = trace.now(clock);
                    Ok(())
                })?;
                trace.emit(clock, || CadenceEvent::SealRecheck {
                    slot: trace_slot,
                    before_qpc: recheck_before_qpc,
                    recovered_source_seq: frame.as_ref().map(|source| source.source_seq),
                    queue_depth: ingress.frames.len(),
                });
                if let Some(recovered) = &frame {
                    newly_received = true;
                    trace.emit(clock, || {
                        CadenceEvent::source(
                            SourceStage::Dequeued,
                            recovered.into(),
                            Some(session_frame),
                            ingress.frames.len(),
                        )
                    });
                }
            }
            if request.cutoff_qpc.load(Ordering::Acquire) <= slot_end {
                if let Some(frame) = frame {
                    trace.emit(clock, || {
                        CadenceEvent::source(
                            SourceStage::CutoffRecycle,
                            (&frame).into(),
                            Some(session_frame),
                            ingress.frames.len(),
                        )
                    });
                    ingress.recycle.send(frame.slot)?;
                }
                if let Some(frame) = selector.take_selected() {
                    trace.emit(clock, || {
                        CadenceEvent::source(
                            SourceStage::CutoffRecycle,
                            (&frame).into(),
                            Some(session_frame),
                            ingress.frames.len(),
                        )
                    });
                    ingress.recycle.send(frame.slot)?;
                }
                break 'recording;
            }
            let Some(mut frame) = frame else {
                break;
            };
            if frame.present_seq.is_none() && (newly_received || frame.source_seq == 1) {
                wait_for_gpu_copy_completion(
                    &d3d.immediate_context,
                    &copy_queries[frame.slot],
                    &clock,
                    &ingress.metrics,
                    frame.callback_start_qpc,
                    gpu_copy_wait_limit_qpc(profile.fps_num, profile.fps_den, clock.frequency()),
                )?;
                trace.emit(clock, || {
                    CadenceEvent::source(
                        SourceStage::GpuReady,
                        (&frame).into(),
                        Some(session_frame),
                        ingress.frames.len(),
                    )
                });
                if let (Some(textures), Some(config)) =
                    (watermark_textures.as_ref(), watermark_config)
                {
                    trace.observe_raw_watermark(clock, (&frame).into(), || {
                        decode_watermark_texture(
                            &d3d.immediate_context,
                            &textures[frame.slot],
                            config.decode_margin,
                        )
                    });
                }
                if let (Some(decoder), Some(textures), Some(config)) = (
                    present_decoder.as_mut(),
                    watermark_textures.as_ref(),
                    watermark_config,
                ) {
                    let Some(raw) = watermark_observation_stats.classify(
                        frame.source_seq,
                        decode_watermark_texture(
                            &d3d.immediate_context,
                            &textures[frame.slot],
                            config.decode_margin,
                        ),
                    )?
                    else {
                        trace.emit(clock, || {
                            CadenceEvent::source(
                                SourceStage::WatermarkRejected,
                                (&frame).into(),
                                Some(session_frame),
                                ingress.frames.len(),
                            )
                        });
                        ingress.recycle.send(frame.slot)?;
                        continue;
                    };
                    let Some(present_seq) = watermark_observation_stats.expand(
                        frame.source_seq,
                        decoder,
                        request
                            .present_identity
                            .expect("formal decoder requires identity session")
                            .generation,
                        raw,
                    )?
                    else {
                        trace.emit(clock, || {
                            CadenceEvent::source(
                                SourceStage::WatermarkStale,
                                (&frame).into(),
                                Some(session_frame),
                                ingress.frames.len(),
                            )
                        });
                        ingress.recycle.send(frame.slot)?;
                        continue;
                    };
                    frame.present_seq = Some(present_seq);
                    trace.emit(clock, || {
                        CadenceEvent::source(
                            SourceStage::WatermarkReady,
                            (&frame).into(),
                            Some(session_frame),
                            ingress.frames.len(),
                        )
                    });
                }
            }
            if !observation_is_at_or_after_arm_boundary(frame.present_seq, arm_boundary)? {
                trace.emit(clock, || {
                    CadenceEvent::source(
                        SourceStage::BeforeArm,
                        (&frame).into(),
                        Some(session_frame),
                        ingress.frames.len(),
                    )
                });
                ingress.recycle.send(frame.slot)?;
                continue;
            }
            match selector.observe(frame, slot_end) {
                CfrObservation::Future(frame) => {
                    seal_reason = SealReason::FutureSource;
                    trace.emit(clock, || CadenceEvent::Observe {
                        slot: trace_slot,
                        source_seq: frame.source_seq,
                        source_qpc: frame.source_qpc,
                        present_seq: frame.present_seq,
                        future: true,
                        replaced_source_seq: None,
                    });
                    future = Some(frame);
                    break;
                }
                CfrObservation::Candidate { replaced } => {
                    trace.emit(clock, || CadenceEvent::Observe {
                        slot: trace_slot,
                        source_seq: frame.source_seq,
                        source_qpc: frame.source_qpc,
                        present_seq: frame.present_seq,
                        future: false,
                        replaced_source_seq: replaced.map(|previous| previous.source_seq),
                    });
                    if let Some(previous) = replaced {
                        ingress.recycle.send(previous.slot)?;
                    }
                }
            }
        }

        let previous = have_canonical.then_some(SelectedSourceIdentity {
            source_seq: last_source_seq,
            source_qpc: last_source_qpc,
            present_seq: last_present_seq,
        });
        let decision = selector.seal(previous)?;
        let selected_decision_qpc = clock.now()?;
        trace.at(Some(selected_decision_qpc), || CadenceEvent::Seal {
            slot: trace_slot,
            reason: seal_reason,
            source_seq: decision.identity.source_seq,
            source_qpc: decision.identity.source_qpc,
            present_seq: decision.identity.present_seq,
            duplicate: decision.duplicate,
            superseded: decision.source_superseded,
            queue_depth: ingress.frames.len(),
        });
        let duplicate = decision.duplicate;
        let superseded = decision.source_superseded;
        if request.present_identity.is_some() {
            ensure!(
                decision.identity.present_seq.is_some(),
                "CFR decision lost its required full Present identity"
            );
        }
        if let Some(source) = decision.selected {
            converter.convert(
                &video_surfaces,
                source.slot,
                NV12_CANONICAL_SLOT,
                session_frame as u32,
            )?;
            ingress.recycle.send(source.slot)?;
            last_source_qpc = source.source_qpc;
            last_callback_start_qpc = source.callback_start_qpc;
            last_callback_ready_qpc = source.callback_ready_qpc;
            last_source_seq = source.source_seq;
            last_present_seq = source.present_seq;
            have_canonical = true;
        } else {
            ensure!(have_canonical, "first CFR slot had no source frame");
            duplicates += 1;
        }
        superseded_total += u64::from(superseded);

        for released in encoder.take_released()? {
            free_nv12.push_back(released as usize);
        }
        drain_encoded_samples(&encoder, &mut current, &mut encoded_timeline, profile)?;
        if request.cutoff_qpc.load(Ordering::Acquire) <= slot_end {
            break 'recording;
        }
        let surface_wait_start = clock.now()?;
        let output_slot = loop {
            for released in encoder.take_released()? {
                free_nv12.push_back(released as usize);
            }
            drain_encoded_samples(&encoder, &mut current, &mut encoded_timeline, profile)?;
            if let Some(slot) = free_nv12.pop_front() {
                break slot;
            }
            ensure!(
                ingress.metrics.drops.load(Ordering::Relaxed) == 0,
                "source queue dropped while waiting for an encoder surface (capacity {CAPTURE_QUEUE_CAPACITY}, max observed depth {})",
                ingress.metrics.max_queue_depth.load(Ordering::Relaxed)
            );
            // The bounded source queue is the authority for data loss. The
            // wall-clock limit below is only a liveness backstop for a genuinely
            // wedged Media Foundation encoder (issues #266 and #797).
            let waited = clock.now()? - surface_wait_start;
            let limit = encoder_progress_limit_qpc(profile, clock.frequency());
            ensure!(
                waited < limit,
                "NV12 encoder backpressure exceeded the bounded liveness watchdog \
                 ({}ms of a {}ms budget, {} of {} reusable encoder surfaces free, {} in flight, source queue depth {}/{CAPTURE_QUEUE_CAPACITY}, max observed depth {}); recording stopped",
                qpc_to_millis(waited, clock.frequency()),
                qpc_to_millis(limit, clock.frequency()),
                free_nv12.len(),
                NV12_POOL_SIZE - 1,
                encoder.in_flight(),
                ingress.frames.len(),
                ingress.metrics.max_queue_depth.load(Ordering::Relaxed),
            );
            thread::sleep(Duration::from_micros(200));
        };
        unsafe {
            d3d.immediate_context.CopyResource(
                &nv12_textures[output_slot],
                &nv12_textures[NV12_CANONICAL_SLOT],
            );
        }
        if request.cutoff_qpc.load(Ordering::Acquire) <= slot_end {
            free_nv12.push_back(output_slot);
            break 'recording;
        }
        // The CFR source identity is final and its canonical RGB copy has been
        // submitted. Publish Select/Retire before any encoder barrier: the GPU
        // candidate ring protects WGC decision latency, not encoder stalls.
        // From this point the frame is committed; a later encoder failure makes
        // the whole run fail and cannot create a successful orphan depth row.
        if let Some(feedback) = selected_feedback.as_mut() {
            let selected_present_seq = last_present_seq
                .context("selected-Present feedback lost the final CFR source identity")?;
            let global_session_frame = request
                .session_frame_start
                .checked_add(session_frame)
                .context("selected-Present session frame overflow")?;
            feedback.select(SelectedFrameDecision {
                session_frame: global_session_frame,
                present_seq: selected_present_seq,
                duplicate,
                source_superseded: superseded,
                source_qpc: last_source_qpc,
                callback_start_qpc: last_callback_start_qpc,
                callback_ready_qpc: last_callback_ready_qpc,
                decision_qpc: selected_decision_qpc,
            })?;
            // An odd slot's new canonical RGB may be inherited by the next even
            // slot. Retain that exact GPU candidate until its fate is known.
            let retire = if feedback.last_depth_present_seq == Some(selected_present_seq) {
                selected_present_seq
            } else {
                selected_present_seq
                    .checked_sub(1)
                    .context("invalid pending Present zero")?
            };
            feedback.retire_through(retire)?;
        }
        let force_keyframe = should_force_keyframe(
            request.session_frame_start,
            session_frame,
            local_frame,
            profile.gop_frames,
        )?;
        if keyframe_requires_encoder_barrier(local_frame, force_keyframe) {
            wait_for_encoder_barrier(
                EncoderOutput {
                    encoder: &encoder,
                    muxer: &mut current,
                    timeline: &mut encoded_timeline,
                },
                session_frame,
                profile,
                clock,
                &ingress,
                "GOP keyframe boundary",
            )?;
        }
        encoder.write_texture(
            &nv12_textures[output_slot],
            output_slot as u32,
            session_frame,
            force_keyframe,
        )?;
        drain_encoded_samples(&encoder, &mut current, &mut encoded_timeline, profile)?;
        timing.push(FrameTiming {
            frame: local_frame,
            session_frame: request
                .session_frame_start
                .checked_add(session_frame)
                .context("session frame index overflow")?,
            output_qpc_start: slot_start,
            output_qpc_end: slot_end,
            source_qpc: last_source_qpc,
            source_seq: last_source_seq,
            present_seq: last_present_seq,
            duplicate,
            source_superseded: superseded,
            queue_drop: ingress.metrics.drops.load(Ordering::Relaxed),
        });
        trace.emit(clock, || CadenceEvent::OutputSubmitted {
            run_frame: session_frame,
            source_seq: last_source_seq,
        });
        session_frame += 1;

        let progress_qpc = clock.now()?;
        if progress_qpc >= next_progress_qpc {
            next_progress_qpc =
                next_progress_deadline(anchor_qpc, progress_qpc, clock.frequency())?;
            let input = request.input_status.snapshot();
            on_event(RecorderEvent::Progress(RecordingProgress {
                observed_qpc: progress_qpc,
                frame_count: session_frame,
                wgc_callback_count: ingress.metrics.callbacks.load(Ordering::Relaxed),
                active_segment,
                ready_segments: summaries.len() as u32,
                duplicate_frames: duplicates,
                source_superseded: superseded_total,
                capture_queue_depth: ingress.frames.len(),
                capture_queue_drops: ingress.metrics.drops.load(Ordering::Relaxed),
                input_event_count: input.persisted_events,
                input_queue_depth: input.queue_depth,
                input_queue_overflows: input.queue_overflows,
                pressed_contract_keys: input.pressed_contract_keys,
                mouse_buttons: input.mouse_buttons,
            }))?;
        }
    }

    drain_stop_signals(controls, &request.cutoff_qpc, &mut stop);
    if let Some(feedback) = selected_feedback.as_mut() {
        let final_session_frame = session_frame
            .checked_sub(1)
            .map(|frame| request.session_frame_start.saturating_add(frame));
        feedback.seal(final_session_frame)?;
    }
    if let Some(future) = future.take() {
        trace.emit(clock, || {
            CadenceEvent::source(
                SourceStage::ShutdownRecycle,
                (&future).into(),
                Some(session_frame),
                ingress.frames.len(),
            )
        });
        ingress.recycle.send(future.slot)?;
    }
    capture
        .take()
        .context("capture control disappeared before shutdown")?
        .stop()
        .map_err(|error| anyhow::anyhow!("WGC stopped with a fatal error: {error}"))?;
    let finished_encoder = encoder.finish()?;
    ensure!(
        finished_encoder.retained_slots.is_empty(),
        "hardware encoder retained {} NV12 surface(s) after finalize",
        finished_encoder.retained_slots.len()
    );
    for released in &finished_encoder.released_slots {
        free_nv12.push_back(*released as usize);
    }
    write_encoded_samples(
        finished_encoder.encoded_samples,
        &mut current,
        &mut encoded_timeline,
        profile,
    )?;
    ensure!(
        encoded_timeline.frames_written == session_frame,
        "hardware encoder emitted {} frames for {session_frame} inputs",
        encoded_timeline.frames_written
    );
    ensure!(
        free_nv12.len() == NV12_POOL_SIZE - 1,
        "NV12 pool lost {} surface(s) across the run",
        (NV12_POOL_SIZE - 1).saturating_sub(free_nv12.len())
    );
    ensure!(
        current.frames_written() == timing.len() as u64,
        "active segment muxed {} frames for {} timing rows",
        current.frames_written(),
        timing.len()
    );
    input_writer.check()?;
    let terminal_input = request.input_status.snapshot();
    ensure!(
        !terminal_input.faulted,
        "Raw Input pipeline reported a fatal input fault: {}",
        terminal_input
            .fault_reason
            .as_deref()
            .unwrap_or("unspecified Raw Input failure")
    );

    while let Some(result) = finalizer.try_result()? {
        let segment = PublishedSegment::from(&result.summary);
        accept_finalize_result(result, &mut summaries, &mut next)?;
        on_event(RecorderEvent::SegmentReady {
            segment,
            ready_segments: summaries.len() as u32,
        })?;
    }
    if !timing.is_empty() {
        let effective_end_qpc = timing
            .last()
            .context("active segment has no final timing row")?
            .output_qpc_end;
        let event_seal = request
            .input_control
            .request_finish(active_segment, effective_end_qpc)?;
        let effective_start_qpc = timing
            .first()
            .context("active segment has no first timing row")?
            .output_qpc_start;
        let audio_seal = audio_writer.as_ref().map(|writer| {
            writer.request_seal(active_segment, effective_start_qpc, effective_end_qpc)
        });
        let final_frame_count = timing.len() as u64;
        let final_session_frame_start = request
            .session_frame_start
            .checked_add(session_frame.saturating_sub(final_frame_count))
            .context("final segment session frame start overflow")?;
        on_event(RecorderEvent::SegmentFinalizing {
            segment_index: active_segment,
            session_frame_start: final_session_frame_start,
            frame_count: final_frame_count,
        })?;
        finalizer.submit(FinalizeJob {
            index: active_segment,
            writer: current,
            timing,
            staging_dir: staging_segment_dir(active_segment),
            final_dir: final_segment_dir(active_segment),
            prepare: None,
            effective_encoder,
            events: event_seal,
            audio: audio_seal,
        })?;
        let expected_segments = active_segment as usize + 1;
        while summaries.len() < expected_segments {
            let result = finalizer.recv_result()?;
            let segment = PublishedSegment::from(&result.summary);
            accept_finalize_result(result, &mut summaries, &mut next)?;
            on_event(RecorderEvent::SegmentReady {
                segment,
                ready_segments: summaries.len() as u32,
            })?;
        }
    } else {
        drop(current);
        let abort = request.input_control.request_abort()?;
        wait_input_ack(&abort, "abort")?;
        if let Some(audio_writer) = &audio_writer {
            audio_writer.abort();
        }
    }
    cleanup_unused_muxer(&mut next, &staging_segment_dir)?;
    finalizer.stop()?;
    input_writer.stop()?;
    if let Some(audio_writer) = audio_writer.take() {
        audio_writer.stop();
    }
    // A Win32 close commonly reports foreground loss just before the target
    // HWND destroy event. Finalization gives the monitor time to deliver that
    // terminal lifecycle signal; fold it in without moving the earlier data
    // cutoff so the run is classified as window_destroyed, not resumable on a
    // stale HWND.
    drain_stop_signals(controls, &request.cutoff_qpc, &mut stop);
    summaries.sort_by_key(|summary| summary.index);
    ensure!(
        ingress.metrics.drops.load(Ordering::Relaxed) == 0,
        "capture queue dropped a source frame"
    );
    let stop = stop.unwrap_or(RecordingStopSignal {
        reason: RecordingStopReason::Manual,
        cutoff_qpc: clock.now()?,
    });
    let qpc_start = summaries.first().map(|summary| summary.qpc_start);
    let qpc_end = summaries.last().map(|summary| summary.qpc_end);
    // 收尾锚点走一次全新的配对读数，而不是从 anchor 样本外推——否则它只会复述
    // anchor 的墙钟观点，无法界定这一整段里 QPC 与墙钟的漂移。
    let ended_at_utc = match qpc_end {
        Some(qpc_end) => WallClockSample::take(clock)?.iso8601_at(qpc_end, clock.frequency()),
        None => None,
    };
    let input = request.input_status.snapshot();
    let session_frame_end = request
        .session_frame_start
        .checked_add(session_frame)
        .context("session frame range overflow")?;
    Ok(RecordingOutcome {
        schema_version: contract.recording_schema_version(profile.audio.is_some()),
        state: contract.run_complete_state(),
        non_trainable: contract.declares_training_classification().then_some(true),
        consent_id: request.consent_id.clone(),
        production_certified: contract.declares_training_classification().then_some(false),
        input_status: "raw_input_v1",
        session_id: request.session_id.clone(),
        run_id: request.run_id.clone(),
        mapping_id: request.mapping.mapping().mapping_id.clone(),
        mapping_version: request.mapping.mapping().version.clone(),
        mapping_content_hash: request.mapping.content_hash().to_owned(),
        session_frame_start: request.session_frame_start,
        session_frame_end,
        run_directory: run_dir.to_path_buf(),
        frame_count: session_frame,
        segment_count: summaries.len() as u32,
        duplicate_frames: duplicates,
        source_superseded: superseded_total,
        wgc_callback_count: ingress.metrics.callbacks.load(Ordering::Relaxed),
        watermark_decoded_observations: watermark_observation_stats.decoded,
        watermark_rejected_observations: watermark_observation_stats.rejected,
        watermark_integrity_rejections: watermark_observation_stats.integrity_mismatch,
        watermark_ambiguous_rejections: watermark_observation_stats.ambiguous,
        max_consecutive_watermark_rejections: watermark_observation_stats.max_consecutive,
        watermark_stale_observations: watermark_observation_stats.stale,
        max_watermark_stale_lag_presents: watermark_observation_stats.max_stale_lag,
        input_event_count: summaries.iter().map(|summary| summary.event_count).sum(),
        input_queue_overflows: input.queue_overflows,
        anchor_qpc: Some(anchor_qpc),
        anchor_at_utc,
        qpc_frequency: clock.frequency(),
        qpc_start,
        qpc_end,
        ended_at_utc,
        stop_reason: stop.reason,
    })
}

fn observe_stop(
    cutoff: &AtomicI64,
    current: &mut Option<RecordingStopSignal>,
    signal: RecordingStopSignal,
) {
    cutoff.fetch_min(signal.cutoff_qpc, Ordering::AcqRel);
    match current {
        None => *current = Some(signal),
        Some(existing) => {
            let window_close_race = matches!(
                (existing.reason, signal.reason),
                (
                    RecordingStopReason::ForegroundLost,
                    RecordingStopReason::WindowDestroyed
                ) | (
                    RecordingStopReason::WindowDestroyed,
                    RecordingStopReason::ForegroundLost
                )
            );
            if window_close_race {
                existing.reason = RecordingStopReason::WindowDestroyed;
                existing.cutoff_qpc = existing.cutoff_qpc.min(signal.cutoff_qpc);
            } else if signal.cutoff_qpc < existing.cutoff_qpc {
                *existing = signal;
            }
        }
    }
}

fn drain_stop_signals(
    controls: &Receiver<RecordingStopSignal>,
    cutoff: &AtomicI64,
    current: &mut Option<RecordingStopSignal>,
) {
    while let Ok(signal) = controls.try_recv() {
        observe_stop(cutoff, current, signal);
    }
}

fn cleanup_unused_muxer(
    next: &mut Option<(u32, H264Mp4Writer)>,
    staging_segment_dir: &impl Fn(u32) -> PathBuf,
) -> anyhow::Result<()> {
    let Some((index, writer)) = next.take() else {
        return Ok(());
    };
    drop(writer);
    let path = staging_segment_dir(index);
    if path.exists() {
        fs::remove_dir_all(&path)
            .with_context(|| format!("failed to remove unused staging muxer {}", path.display()))?;
    }
    Ok(())
}

fn accept_finalize_result(
    result: FinalizeResult,
    summaries: &mut Vec<SegmentSummary>,
    next: &mut Option<(u32, H264Mp4Writer)>,
) -> anyhow::Result<()> {
    if let Some(prepared) = result.prepared {
        ensure!(
            next.replace(prepared).is_none(),
            "finalizer prepared two standby MP4 muxers"
        );
    }
    summaries.push(result.summary);
    Ok(())
}

/// Wait until every submitted frame has emerged from the asynchronous encoder.
///
/// The budget is `ENCODER_DRAIN_CFR_FRAMES` output periods, not one: this drains
/// the whole pipeline, and the driver keeps several frames in flight (issue
/// #266). The capture queue is sized to cover exactly that wait, and the drop
/// check below is the authoritative limit — it fires on real data loss, whereas
/// the deadline is only a backstop against a wedged encoder.
/// 编码输出侧的三件套：编码器、正在写的 muxer、已写帧的时间线。
///
/// 它们在每次 drain 里同进同出——drain 从 encoder 取样本、写进 muxer、推进 timeline，
/// 缺一不可。合成一个参数既是给 clippy 的 7 参上限让路，也让「barrier 要操作的是这一
/// 组状态」这件事在签名上看得见。
struct EncoderOutput<'a> {
    encoder: &'a HardwareH264Encoder,
    muxer: &'a mut H264Mp4Writer,
    timeline: &'a mut EncodedTimeline,
}

fn wait_for_encoder_barrier(
    output: EncoderOutput<'_>,
    expected: u64,
    profile: &CaptureProfile,
    clock: QpcClock,
    ingress: &CaptureIngress,
    reason: &'static str,
) -> anyhow::Result<()> {
    let EncoderOutput {
        encoder,
        muxer,
        timeline: encoded_timeline,
    } = output;
    let start = clock.now()?;
    let limit = encoder_progress_limit_qpc(profile, clock.frequency());
    while encoded_timeline.frames_written < expected {
        drain_encoded_samples(encoder, muxer, encoded_timeline, profile)?;
        // Blocking here lets source observations pile up. The queue is sized for
        // this wait, so a drop means the real constraint was hit — say so
        // instead of blaming the encoder for a deadline that had not expired.
        ensure!(
            ingress.metrics.drops.load(Ordering::Relaxed) == 0,
            "source queue dropped while waiting for the {reason} encoder barrier (capacity {CAPTURE_QUEUE_CAPACITY}, max observed depth {}, {} frame(s) still outstanding)",
            ingress.metrics.max_queue_depth.load(Ordering::Relaxed),
            expected - encoded_timeline.frames_written
        );
        let waited = clock.now()? - start;
        ensure!(
            waited < limit,
            "hardware encoder did not reach the {reason} barrier before the bounded liveness watchdog \
             ({}ms of a {}ms budget, {} of {expected} frame(s) still outstanding, source queue depth {}/{CAPTURE_QUEUE_CAPACITY}, max observed depth {})",
            qpc_to_millis(waited, clock.frequency()),
            qpc_to_millis(limit, clock.frequency()),
            expected - encoded_timeline.frames_written,
            ingress.frames.len(),
            ingress.metrics.max_queue_depth.load(Ordering::Relaxed)
        );
        thread::sleep(Duration::from_micros(200));
    }
    Ok(())
}

/// QPC ticks to whole milliseconds, for diagnostics only.
fn qpc_to_millis(ticks: i64, frequency: i64) -> i64 {
    if frequency <= 0 {
        return 0;
    }
    (i128::from(ticks) * 1_000 / i128::from(frequency)) as i64
}

fn drain_encoded_samples(
    encoder: &HardwareH264Encoder,
    muxer: &mut H264Mp4Writer,
    encoded_timeline: &mut EncodedTimeline,
    profile: &CaptureProfile,
) -> anyhow::Result<()> {
    write_encoded_samples(encoder.take_encoded()?, muxer, encoded_timeline, profile)
}

#[derive(Default)]
struct EncodedTimeline {
    frames_written: u64,
    pts_offset_100ns: Option<i64>,
}

impl EncodedTimeline {
    fn validate(
        &mut self,
        sample: &EncodedH264Sample,
        profile: &CaptureProfile,
    ) -> anyhow::Result<()> {
        let frame = self.frames_written;
        validate_encoded_sample_timing(sample, frame, &mut self.pts_offset_100ns, profile)
    }
}

pub fn validate_encoded_sample_timing(
    sample: &EncodedH264Sample,
    frame: u64,
    pts_offset_100ns: &mut Option<i64>,
    profile: &CaptureProfile,
) -> anyhow::Result<()> {
    let expected_pts = frame_pts_100ns(frame, profile.fps_num, profile.fps_den);
    let expected_duration = frame_duration_100ns(frame, profile.fps_num, profile.fps_den);
    let offset = match *pts_offset_100ns {
        Some(offset) => offset,
        None => {
            let observed = sample
                .pts_100ns
                .checked_sub(expected_pts)
                .context("hardware encoder PTS offset overflowed")?;
            ensure!(
                observed == 0 || observed == expected_duration,
                "hardware encoder changed frame {frame} PTS: expected {expected_pts} or a \
                 one-frame startup offset, received {}",
                sample.pts_100ns
            );
            *pts_offset_100ns = Some(observed);
            observed
        }
    };
    let exact_pts = expected_pts
        .checked_add(offset)
        .context("expected hardware encoder PTS overflowed")?;
    let nominal_duration = frame_duration_100ns(0, profile.fps_num, profile.fps_den);
    let constant_cfr_pts = i64::try_from(frame)
        .ok()
        .and_then(|frame| frame.checked_mul(nominal_duration))
        .and_then(|elapsed| offset.checked_add(elapsed))
        .context("constant-CFR hardware encoder PTS overflowed")?;
    ensure!(
        sample.pts_100ns == exact_pts || sample.pts_100ns == constant_cfr_pts,
        "hardware encoder changed frame {frame} PTS: expected rational CFR {exact_pts} or constant \
         CFR {constant_cfr_pts} with the established startup offset, received {}",
        sample.pts_100ns
    );
    ensure!(
        sample.duration_100ns == expected_duration || sample.duration_100ns == nominal_duration,
        "hardware encoder changed frame {frame} duration: expected {expected_duration} or constant \
         CFR duration {nominal_duration}, received {}",
        sample.duration_100ns
    );
    Ok(())
}

fn write_encoded_samples(
    samples: impl IntoIterator<Item = EncodedH264Sample>,
    muxer: &mut H264Mp4Writer,
    encoded_timeline: &mut EncodedTimeline,
    profile: &CaptureProfile,
) -> anyhow::Result<()> {
    for sample in samples {
        encoded_timeline.validate(&sample, profile)?;
        muxer.write_sample(&sample)?;
        encoded_timeline.frames_written += 1;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn probe_hardware_path_with_context(
    converter: &BgraToNv12,
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    nv12_textures: &[ID3D11Texture2D],
    surface_width: u32,
    surface_height: u32,
    profile: &CaptureProfile,
    path: &Path,
) -> anyhow::Result<(EffectiveEncoderConfig, Mp4Probe)> {
    ensure!(
        nv12_textures.len() >= 2,
        "hardware preflight requires two NV12 textures"
    );
    let scratch_bgra = create_bgra_copy_texture(device, surface_width, surface_height)?;
    let scratch_surfaces = converter.prepare_surfaces(
        std::slice::from_ref(&scratch_bgra),
        std::slice::from_ref(&nv12_textures[NV12_CANONICAL_SLOT]),
    )?;
    converter.convert(&scratch_surfaces, 0, 0, 0)?;
    unsafe {
        context.CopyResource(&nv12_textures[1], &nv12_textures[NV12_CANONICAL_SLOT]);
        context.Flush();
    }
    let mut encoder = HardwareH264Encoder::create(device, profile)?;
    encoder.write_texture(&nv12_textures[1], 1, 0, true)?;
    let finished = encoder.finish()?;
    ensure!(
        finished.retained_slots.is_empty(),
        "hardware preflight retained {} NV12 surface(s) after encoder shutdown",
        finished.retained_slots.len()
    );
    ensure!(
        finished.released_slots == [1],
        "hardware preflight released unexpected NV12 pool slots"
    );
    ensure!(
        finished.encoded_samples.len() == 1,
        "hardware preflight emitted an unexpected sample count"
    );
    let mut encoded_timeline = EncodedTimeline::default();
    encoded_timeline.validate(&finished.encoded_samples[0], profile)?;
    let mut muxer = H264Mp4Writer::create(path, profile)?;
    muxer.write_sample(&finished.encoded_samples[0])?;
    muxer.finish()?;
    let probe = probe_h264_mp4(path, 1, profile)?;
    ensure!(
        probe.first_sample_clean_point && probe.pts_monotonic,
        "hardware preflight did not produce a valid first IDR sample"
    );
    Ok((finished.effective_encoder, probe))
}

fn finalize_segment(
    job: FinalizeJob,
    context: &FinalizeContext,
    projector: &mut FrameProjector,
) -> anyhow::Result<FinalizeResult> {
    let FinalizeContext {
        profile,
        target,
        session_id,
        run_id,
        mapping,
        catalog,
        consent_id,
        // Consumed when the projector is built, not per segment.
        control_vks: _,
        qpc_frequency,
    } = context;
    let finalize_start = Instant::now();
    job.writer.finish()?;
    let video_path = job.staging_dir.join("video.mp4");
    let probe = probe_h264_mp4(&video_path, job.timing.len() as u64, profile)?;
    let qpc_start = job
        .timing
        .first()
        .context("segment timing is empty")?
        .output_qpc_start;
    let qpc_end = job
        .timing
        .last()
        .context("segment timing is empty")?
        .output_qpc_end;
    let event_seal = wait_input_ack(&job.events, "segment seal")?;
    ensure!(
        event_seal.segment_index == job.index
            && event_seal.qpc_start == qpc_start
            && event_seal.qpc_end == qpc_end,
        "Raw Input segment boundaries do not match frame timing"
    );
    // A published segment claims complete per-event input. The journal's ready gate
    // runs only after `durable_rename` has already moved the directory into its
    // immutable final location, so refusing here is what keeps an artifact with a
    // known input gap from reaching disk at all — otherwise it lands as a
    // ready-looking directory the journal disowns and recovery reports as an orphan.
    ensure!(
        !event_seal.input_faulted,
        "Raw Input pipeline faulted before segment {} seal acknowledgement: {}",
        job.index,
        event_seal
            .input_fault_reason
            .as_deref()
            .unwrap_or("unspecified Raw Input failure")
    );
    ensure!(
        event_seal.input_queue_overflows == 0,
        "Raw Input queue overflowed {} time(s) before segment {} seal acknowledgement; refusing to publish",
        event_seal.input_queue_overflows,
        job.index
    );
    let events_path = job.staging_dir.join("events.jsonl.zst");
    let events = write_events_zstd(&event_seal, &events_path)?;
    let event_count = events.len() as u64;
    let frame_rows =
        projector.project_segment(&job.timing, &events, run_id, profile, mapping.mapping())?;
    ensure!(
        frame_rows.len() as u64 == probe.sample_count,
        "projected frame rows do not match media samples"
    );
    let frames_path = job.staging_dir.join("frames.jsonl.zst");
    write_frame_projection_zstd(&frames_path, &frame_rows)?;
    let video_sha256 = sha256_file(&video_path)?;
    let frames_sha256 = sha256_file(&frames_path)?;
    let events_sha256 = sha256_file(&events_path)?;
    let video_bytes = fs::metadata(&video_path)?.len();
    let audio = job.audio.as_ref().map(|receiver| {
        let seal = wait_audio_seal(
            receiver,
            job.index,
            qpc_start,
            qpc_end,
            *qpc_frequency,
        );
        match publish_audio_seal(seal, &job.staging_dir) {
            Ok(metadata) => metadata,
            Err(error) => {
                tracing::warn!(
                    segment_index = job.index,
                    error = %error,
                    "audio finalization failed; publishing the video/input segment with audio absent"
                );
                failed_audio_metadata(
                    *qpc_frequency,
                    "AUDIO_FINALIZE_FAILED".into(),
                    SegmentAudioHealth::default(),
                )
            }
        }
    });
    let meta_path = job.staging_dir.join("meta.json");
    let meta = serde_json::json!({
        "schema_version": catalog.contract.recording_schema_version(audio.is_some()),
        "session_id": session_id,
        "run_id": run_id,
        "segment_index": job.index,
        "session_frame_start": job.timing.first().map(|row| row.session_frame),
        "n_frames": job.timing.len(),
        // consented_ready 段状态随 certified 车道删除（docs/100 §4.2）：它是
        // first_party 时代的遗留，被 certified 复活使用，车道一走就再次变死。
        // 剩下两值由契约给出：legacy 把车道名写进了状态词，中性只说"封好了"。
        "state": catalog.contract.segment_ready_state(),
        "input_status": "raw_input_v1",
        "target": target,
        "qpc_start": qpc_start,
        "qpc_end": qpc_end,
        "fps_num": profile.fps_num,
        "fps_den": profile.fps_den,
        "width": profile.width,
        "height": profile.height,
        "mapping": {
            "id": mapping.mapping().mapping_id,
            "version": mapping.mapping().version,
            "content_hash": mapping.content_hash(),
        },
        "catalog": catalog,
        "contract": {
            "version": "dm-1",
            "pointer_clamp": 200,
            "pointer_mu": 8,
            "pointer_ref": 200,
            "pointer_max_bin": 10,
        },
        "video": {"path": "video.mp4", "bytes": video_bytes, "sha256": video_sha256},
        "frames": {"path": "frames.jsonl.zst", "lines": job.timing.len(), "sha256": frames_sha256},
        "events": {"path": "events.jsonl.zst", "lines": event_count, "sha256": events_sha256},
        "effective_encoder": &job.effective_encoder,
        "encoder_health": {
            "controls_relaxed": job.effective_encoder.relaxed_controls.len(),
            "bitrate_invariant_violated": job.effective_encoder.max_bitrate_bps < job.effective_encoder.mean_bitrate_bps
                && !profile.accepts_effective_max_bitrate(job.effective_encoder.max_bitrate_bps),
            "gop_normalized_unaccepted": !profile.accepts_effective_gop(job.effective_encoder.gop_frames),
        },
        "probe": &probe,
        "health": {
            "duplicate_frames": job.timing.iter().filter(|row| row.duplicate).count(),
            "source_superseded": job.timing.iter().map(|row| u64::from(row.source_superseded)).sum::<u64>(),
            "queue_drops": job.timing.iter().map(|row| row.queue_drop).max().unwrap_or_default(),
            "input_overflow": event_seal.input_queue_overflows,
            "input_boundary_reorder_drops": event_seal.input_boundary_reorder_drops,
            "input_raw_reports": event_seal.input_raw_reports,
            "input_buffered_raw_reports": event_seal.input_buffered_raw_reports,
            "input_max_raw_read_batch_size": event_seal.input_max_raw_read_batch_size,
            "input_polling_hz_p50": event_seal.input_polling_hz_p50,
            "input_polling_hz_p05": event_seal.input_polling_hz_p05,
            "input_polling_delivery_groups": event_seal.input_polling_delivery_groups,
            "input_polling_max_delivery_batch_size": event_seal.input_polling_max_delivery_batch_size,
            "input_polling_estimator": event_seal.input_polling_estimator,
            // Session-cumulative, keyed by VK. Control input is dropped before
            // projection by design, but the drop used to leave no trace: a game
            // profile that wrongly lists a real binding in `control_vks` silently
            // deletes that action from every derived plane. A counter that is
            // normally all-zeros (F8/F10 are rarely pressed) makes that visible
            // the first time it happens instead of an audit later.
            "dropped_control_events": projector
                .dropped_control_events()
                .iter()
                .map(|(vk, count)| (vk.to_string(), serde_json::Value::from(*count)))
                .collect::<serde_json::Map<String, serde_json::Value>>(),
        },
    });
    let mut meta = meta;
    // legacy 的两个训练声明字节原样保留——存量段已经这么落在磁盘和 R2 上，
    // recovery 与 data-demo-converter 都按它们校验。中性契约一个都不写（§B）。
    if catalog.contract.declares_training_classification() {
        meta["non_trainable"] = serde_json::json!(true);
        meta["production_certified"] = serde_json::json!(false);
    }
    if let Some(audio) = &audio {
        meta["audio"] = serde_json::to_value(audio)?;
    }
    if let Some(consent_id) = consent_id {
        meta["consent_id"] = serde_json::json!(consent_id);
    }
    write_json(&meta_path, &meta)?;
    let meta_sha256 = sha256_file(&meta_path)?;
    let mut manifest = serde_json::Map::from_iter([
        ("video.mp4".into(), serde_json::json!(video_sha256)),
        ("frames.jsonl.zst".into(), serde_json::json!(frames_sha256)),
        ("events.jsonl.zst".into(), serde_json::json!(events_sha256)),
        ("meta.json".into(), serde_json::json!(meta_sha256)),
    ]);
    if let Some(audio) = &audio
        && audio.has_artifact()
    {
        manifest.insert(
            AUDIO_ARTIFACT_NAME.into(),
            serde_json::json!(
                audio
                    .sha256
                    .as_deref()
                    .context("audio artifact hash disappeared")?
            ),
        );
    }
    let manifest_path = job.staging_dir.join("sha256.json");
    write_json(&manifest_path, &manifest)?;
    let mut durable_paths = vec![
        video_path.as_path(),
        frames_path.as_path(),
        events_path.as_path(),
        meta_path.as_path(),
        manifest_path.as_path(),
    ];
    let audio_path = job.staging_dir.join(AUDIO_ARTIFACT_NAME);
    if audio
        .as_ref()
        .is_some_and(SegmentAudioMetadata::has_artifact)
    {
        durable_paths.push(audio_path.as_path());
    }
    for path in durable_paths {
        sync_file(path)?;
    }
    ensure!(
        !job.final_dir.exists(),
        "refusing to overwrite immutable segment {}",
        job.final_dir.display()
    );
    durable_rename(&job.staging_dir, &job.final_dir)?;
    if let Err(error) = fs::remove_file(&event_seal.path) {
        tracing::warn!(
            segment_index = job.index,
            error = %error,
            "ready segment published but its sealed Raw Input spool file could not be removed"
        );
    }
    tracing::info!(
        segment_index = job.index,
        elapsed_ms = finalize_start.elapsed().as_secs_f64() * 1_000.0,
        "non-trainable local segment published"
    );
    let prepared = match job.prepare {
        Some((index, path)) => Some((index, H264Mp4Writer::create(path, profile)?)),
        None => None,
    };
    Ok(FinalizeResult {
        summary: SegmentSummary {
            index: job.index,
            frame_count: job.timing.len() as u64,
            event_count,
            qpc_start,
            qpc_end,
            video_bytes,
            video_sha256,
            frames_sha256,
            events_sha256,
            meta_sha256,
            audio,
            probe,
        },
        prepared,
    })
}

fn wait_audio_seal(
    receiver: &Receiver<AudioAck<AudioSegmentSeal>>,
    segment_index: u32,
    qpc_start: i64,
    qpc_end: i64,
    qpc_frequency: i64,
) -> AudioSegmentSeal {
    match receiver.recv_timeout(AUDIO_SEAL_TIMEOUT) {
        Ok(Ok(seal))
            if seal.segment_index == segment_index
                && seal.qpc_start == qpc_start
                && seal.qpc_end == qpc_end =>
        {
            seal
        }
        Ok(Ok(seal)) => {
            if let Some(path) = &seal.path {
                let _ = fs::remove_file(path);
            }
            AudioSegmentSeal {
                segment_index,
                qpc_start,
                qpc_end,
                path: None,
                metadata: failed_audio_metadata(
                    qpc_frequency,
                    "AUDIO_BOUNDARY_MISMATCH".into(),
                    seal.metadata.health,
                ),
            }
        }
        Ok(Err(error)) => {
            tracing::warn!(segment_index, error = %error, "audio segment seal failed");
            AudioSegmentSeal {
                segment_index,
                qpc_start,
                qpc_end,
                path: None,
                metadata: failed_audio_metadata(
                    qpc_frequency,
                    "AUDIO_SEAL_FAILED".into(),
                    SegmentAudioHealth::default(),
                ),
            }
        }
        Err(error) => {
            tracing::warn!(segment_index, error = %error, "audio segment seal timed out");
            AudioSegmentSeal {
                segment_index,
                qpc_start,
                qpc_end,
                path: None,
                metadata: failed_audio_metadata(
                    qpc_frequency,
                    "AUDIO_SEAL_TIMEOUT".into(),
                    SegmentAudioHealth::default(),
                ),
            }
        }
    }
}

fn publish_audio_seal(
    mut seal: AudioSegmentSeal,
    staging_dir: &Path,
) -> anyhow::Result<SegmentAudioMetadata> {
    ensure!(
        seal.metadata.qpc_frequency > 0,
        "audio seal QPC frequency is invalid"
    );
    ensure!(
        seal.qpc_end > seal.qpc_start,
        "audio seal QPC range is empty"
    );
    if let Err(error) = seal.metadata.validate() {
        if let Some(path) = &seal.path {
            let _ = fs::remove_file(path);
        }
        return Err(error);
    }
    if !seal.metadata.has_artifact() {
        ensure!(
            seal.path.is_none(),
            "absent audio seal retained a spool path"
        );
        return Ok(seal.metadata);
    }

    let source = seal
        .path
        .take()
        .context("audio metadata claims an artifact but the seal has no spool path")?;
    let destination = staging_dir.join(AUDIO_ARTIFACT_NAME);
    ensure!(
        !destination.exists(),
        "audio staging artifact already exists"
    );
    if let Err(error) = fs::rename(&source, &destination).with_context(|| {
        format!(
            "failed to move sealed audio {} -> {}",
            source.display(),
            destination.display()
        )
    }) {
        let _ = fs::remove_file(&source);
        return Err(error);
    }

    let verify = (|| {
        let actual_bytes = fs::metadata(&destination)?.len();
        let actual_hash = sha256_file(&destination)?;
        ensure!(
            seal.metadata.bytes == Some(actual_bytes),
            "audio compressed byte count changed during finalization"
        );
        ensure!(
            seal.metadata.sha256.as_deref() == Some(actual_hash.as_str()),
            "audio SHA-256 changed during finalization"
        );
        Ok::<(), anyhow::Error>(())
    })();
    if let Err(error) = verify {
        let _ = fs::remove_file(&destination);
        return Err(error);
    }
    Ok(seal.metadata)
}

fn failed_audio_metadata(
    qpc_frequency: i64,
    failure_code: String,
    health: SegmentAudioHealth,
) -> SegmentAudioMetadata {
    SegmentAudioMetadata {
        state: SegmentAudioState::Absent,
        source: crate::audio_contract::AudioSource::ProcessLoopback,
        path: None,
        sha256: None,
        bytes: None,
        format: crate::audio_contract::AudioPcmFormat::default(),
        qpc_frequency,
        first_sample_qpc: None,
        frames: None,
        failure_code: Some(failure_code),
        health,
    }
}

fn write_events_zstd(seal: &EventSegmentSeal, output: &Path) -> anyhow::Result<Vec<RawInputEvent>> {
    let input = open_sealed_event_file(&seal.path)?;
    let file =
        File::create(output).with_context(|| format!("failed to create {}", output.display()))?;
    let mut encoder = zstd::Encoder::new(BufWriter::new(file), 3)?;
    let mut previous_seq = None;
    let mut events = Vec::with_capacity(seal.event_count as usize);
    for line in BufReader::new(input).lines() {
        let line = line?;
        ensure!(
            !line.is_empty(),
            "Raw Input spool contains an empty JSONL row"
        );
        let event: RawInputEvent = serde_json::from_str(&line)?;
        ensure!(
            event.qpc >= seal.qpc_start && event.qpc < seal.qpc_end,
            "Raw Input event falls outside the sealed segment"
        );
        if let Some(previous) = previous_seq {
            ensure!(
                event.seq > previous,
                "Raw Input event sequence is not monotonic"
            );
        }
        previous_seq = Some(event.seq);
        encoder.write_all(line.as_bytes())?;
        encoder.write_all(b"\n")?;
        events.push(event);
    }
    encoder.finish()?.flush()?;
    ensure!(
        events.len() as u64 == seal.event_count,
        "Raw Input sealed count {count} does not match {expected}",
        count = events.len(),
        expected = seal.event_count
    );
    Ok(events)
}

fn write_frame_projection_zstd(path: &Path, rows: &[FrameProjectionRow]) -> anyhow::Result<()> {
    let file =
        File::create(path).with_context(|| format!("failed to create {}", path.display()))?;
    let mut encoder = zstd::Encoder::new(BufWriter::new(file), 3)?;
    for row in rows {
        serde_json::to_writer(&mut encoder, row)?;
        encoder.write_all(b"\n")?;
    }
    encoder.finish()?.flush()?;
    Ok(())
}

fn ensure_session_contract(session_dir: &Path, request: &RecordingRequest) -> anyhow::Result<()> {
    fs::create_dir_all(session_dir)
        .with_context(|| format!("failed to create {}", session_dir.display()))?;
    for (name, bytes, expected_hash) in [
        (
            "catalog.json",
            request.catalog.catalog_source_bytes.as_slice(),
            request.catalog.catalog_content_hash.as_str(),
        ),
        (
            "device-profile.json",
            request.catalog.device.source_bytes.as_slice(),
            request.catalog.device.content_hash.as_str(),
        ),
        (
            "recording-profile.json",
            request.catalog.recording.source_bytes.as_slice(),
            request.catalog.recording.content_hash.as_str(),
        ),
        (
            "game-profile.json",
            request.catalog.game.source_bytes.as_slice(),
            request.catalog.game.content_hash.as_str(),
        ),
        (
            "mapping.json",
            request.catalog.mapping.source_bytes.as_slice(),
            request.catalog.mapping.content_hash.as_str(),
        ),
    ] {
        ensure!(
            sha256_bytes(bytes) == expected_hash,
            "catalog artifact {name} does not match its declared content hash"
        );
        let path = session_dir.join(name);
        if path.exists() {
            ensure!(
                fs::read(&path)? == bytes,
                "session catalog artifact {name} does not match the selected catalog"
            );
        } else {
            write_bytes_atomic(&path, bytes)?;
        }
    }

    let session_path = session_dir.join("session.json");
    let mapping = request.mapping.mapping();
    let audio_enabled = request.profile.audio.is_some();
    let audio_policy = request
        .profile
        .audio
        .as_ref()
        .map(serde_json::to_value)
        .transpose()?
        .unwrap_or(serde_json::Value::Bool(false));
    let session = serde_json::json!({
        "schema_version": request.catalog.contract.session_schema_version(audio_enabled),
        "session_id": request.session_id,
        "game": {
            "id": request.target.game_id,
            "exe_hash": request.target.executable_sha256,
        },
        "mapping": {
            "id": mapping.mapping_id,
            "version": mapping.version,
            "content_hash": request.mapping.content_hash(),
            "recording_profile_id": mapping.recording_profile_id,
        },
        "catalog": &request.catalog,
        "capture_policy": {
            "source": "game_window_only",
            "width": request.profile.width,
            "height": request.profile.height,
            "scale": request.profile.allows_source_scaling(),
            // 键名沿用 `minimum_*` 不改（RAW manifest schema 稳定性）。读的时候要
            // 配 `scale` 一起看：`scale=false` 时这对数不是下限，是**精确要求**。
            // 规则种类见 `SourceGeometryRule`。
            "minimum_source_width": request.profile.source_geometry_rule().dimensions().0,
            "minimum_source_height": request.profile.source_geometry_rule().dimensions().1,
            "fps_num": request.profile.fps_num,
            "fps_den": request.profile.fps_den,
            "avg_bitrate_bps": request.profile.bitrate_bps,
            "max_bitrate_bps": request.profile.max_bitrate_bps,
            "segment_seconds": request.profile.segment_seconds,
            "gop_frames": request.profile.gop_frames,
            "audio": audio_policy,
            "segment_policy_override": &request.segment_policy_override,
        },
        "contract": {
            "version": "dm-1",
            "pointer_clamp": 200,
            "pointer_mu": 8,
            "pointer_ref": 200,
            "pointer_max_bin": 10,
        },
        "todo": ["annotator_device_consent_grant", "crash_recovery"],
    });
    let mut session = session;
    // legacy 的两个训练声明字节原样保留（理由同段级 meta.json）。中性契约不写：
    // "可训练"只能是服务端 clip_trainability 的裁决，客户端连一句自称都不留。
    if request.catalog.contract.declares_training_classification() {
        session["non_trainable"] = serde_json::json!(true);
        session["production_certified"] = serde_json::json!(false);
    }
    // consent 如实记录，但**不改分类**（docs/100 §3 推论 2）。从前这里把
    // non_trainable 翻成 false——那是 certified 车道唯一让本地产物自称可训练的
    // 地方。车道废除后它必须走：journal v27 的 CHECK 钉死 non_trainable = 1，
    // 上传信封也只认 non_trainable=1，留着这一行会让 session.json 与两者当场矛盾。
    if let Some(consent_id) = &request.consent_id {
        session["consent_id"] = serde_json::json!(consent_id);
    }
    if session_path.exists() {
        let existing_bytes = fs::read(&session_path)
            .with_context(|| format!("failed to read {}", session_path.display()))?;
        let existing: serde_json::Value = serde_json::from_slice(&existing_bytes)
            .with_context(|| format!("invalid session manifest {}", session_path.display()))?;
        ensure!(
            existing == session,
            "session manifest does not match the selected catalog, target, mapping, or capture policy"
        );
    } else {
        write_json_atomic(&session_path, &session)?;
    }
    Ok(())
}

fn write_json(path: &Path, value: &impl Serialize) -> anyhow::Result<()> {
    let file =
        File::create(path).with_context(|| format!("failed to create {}", path.display()))?;
    let mut writer = BufWriter::new(file);
    serde_json::to_writer_pretty(&mut writer, value)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

fn write_bytes_atomic(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    ensure!(!path.exists(), "refusing to overwrite {}", path.display());
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .context("atomic byte path has no UTF-8 file name")?;
    let staging = path.with_file_name(format!(".{file_name}.tmp"));
    ensure!(
        !staging.exists(),
        "refusing to overwrite staging file {}",
        staging.display()
    );
    fs::write(&staging, bytes)?;
    sync_file(&staging)?;
    durable_rename(&staging, path)
}

fn write_json_atomic(path: &Path, value: &impl Serialize) -> anyhow::Result<()> {
    ensure!(!path.exists(), "refusing to overwrite {}", path.display());
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .context("atomic JSON path has no UTF-8 file name")?;
    let staging = path.with_file_name(format!(".{file_name}.tmp"));
    ensure!(
        !staging.exists(),
        "refusing to overwrite staging file {}",
        staging.display()
    );
    write_json(&staging, value)?;
    sync_file(&staging)?;
    durable_rename(&staging, path)
}

fn sync_file(path: &Path) -> anyhow::Result<()> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("failed to reopen {} for durable sync", path.display()))?
        .sync_all()
        .with_context(|| format!("failed to durable-sync {}", path.display()))
}

fn durable_rename(source: &Path, destination: &Path) -> anyhow::Result<()> {
    let source_wide = source
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let destination_wide = destination
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    unsafe {
        MoveFileExW(
            PCWSTR::from_raw(source_wide.as_ptr()),
            PCWSTR::from_raw(destination_wide.as_ptr()),
            MOVEFILE_WRITE_THROUGH,
        )
    }
    .context("MoveFileExW(MOVEFILE_WRITE_THROUGH) failed")
}

fn sha256_file(path: &Path) -> anyhow::Result<String> {
    let mut reader = BufReader::new(
        File::open(path).with_context(|| format!("failed to open {}", path.display()))?,
    );
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("sha256:{:x}", hasher.finalize()))
}

fn sha256_bytes(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn sanitize_diagnostic(message: &str, output_root: &Path, run_dir: &Path) -> String {
    message
        .replace(&run_dir.to_string_lossy().to_string(), "<session-dir>")
        .replace(
            &output_root.to_string_lossy().to_string(),
            "<app-local-data>",
        )
}

fn slot_qpc(anchor: i64, index: u64, frequency: i64, profile: &CaptureProfile) -> i64 {
    frame_qpc(anchor, index, frequency, profile.fps_num, profile.fps_den)
}

fn next_progress_deadline(
    anchor_qpc: i64,
    observed_qpc: i64,
    qpc_frequency: i64,
) -> anyhow::Result<i64> {
    ensure!(qpc_frequency > 0, "QPC frequency must be positive");
    let elapsed_qpc = observed_qpc
        .checked_sub(anchor_qpc)
        .context("heartbeat QPC elapsed range overflowed")?;
    ensure!(elapsed_qpc >= 0, "heartbeat QPC precedes its anchor");
    let completed_seconds = elapsed_qpc / qpc_frequency;
    completed_seconds
        .checked_add(1)
        .and_then(|seconds| seconds.checked_mul(qpc_frequency))
        .and_then(|offset| anchor_qpc.checked_add(offset))
        .context("next progress QPC deadline overflowed")
}

/// One output frame period used to scale encoder-pipeline progress deadlines.
fn frame_period_qpc(profile: &CaptureProfile, frequency: i64) -> i64 {
    ((i128::from(frequency) * i128::from(profile.fps_den)) / i128::from(profile.fps_num)).max(1)
        as i64
}

/// The budget for waiting on the *encoder pipeline* to make progress, whether
/// that is flushing a whole segment tail or returning one NV12 surface.
///
/// This is not one frame of work: the driver keeps several frames in flight (it
/// normalises `MF_LOW_LATENCY` away — see `ENCODER_DRAIN_CFR_FRAMES`), so the
/// cost scales with pipeline depth, not with a single frame. Using the one-frame
/// surface budget here was the bug in issue #266 — it made the *time* limit
/// stricter than the resource it exists to protect, so recording stopped while
/// the capture queue still had room and had dropped nothing.
///
/// `ENCODER_DRAIN_CFR_FRAMES` remains the queue-coupled minimum, but it is not a
/// valid wall-clock failure threshold: a real source cadence below the requested
/// 8x ceiling leaves more queue headroom. The queue-drop check at each call site
/// remains the authority for real data loss; the longer wall-clock deadline only
/// prevents a permanently wedged encoder from hanging the recorder forever.
fn encoder_progress_limit_qpc(profile: &CaptureProfile, frequency: i64) -> i64 {
    let frequency = frequency.max(1);
    let queue_coupled_minimum = frame_period_qpc(profile, frequency)
        .saturating_mul(i64::try_from(ENCODER_DRAIN_CFR_FRAMES).unwrap_or(1))
        .max(1);
    let liveness_watchdog =
        (i128::from(frequency) * i128::from(ENCODER_PROGRESS_LIVENESS_WATCHDOG_MS) / 1_000)
            .clamp(1, i128::from(i64::MAX)) as i64;
    queue_coupled_minimum.max(liveness_watchdog)
}

#[allow(dead_code)]
fn _keep_types(_: CapturedFrame) {}

#[cfg(test)]
#[path = "recording_cadence_tests.rs"]
mod cadence_wake_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn recorder_feedback_fixture(
        capacity: usize,
    ) -> (
        RecorderFeedback,
        present_identity::transport::ProbeTransport,
    ) {
        use present_identity::feedback::{StreamEpoch, WireIdentity};
        use present_identity::transport::{ClientTransport, ProbeTransport, TransportNames};
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let names = TransportNames::new(
            std::process::id(),
            std::process::id(),
            1,
            &format!("recorder-fault-{}", NEXT.fetch_add(1, Ordering::Relaxed)),
        )
        .unwrap();
        let stream = StreamEpoch {
            identity: WireIdentity::new("recorder-fault", "r0000").unwrap(),
            generation: 1,
            resource_epoch: 1,
        };
        let client = ClientTransport::create(&names, &stream, capacity).unwrap();
        let probe = ProbeTransport::open(&names, &stream).unwrap();
        (RecorderFeedback::new(client), probe)
    }

    #[test]
    fn recorder_reads_original_fault_ack_before_attempting_a_full_queue() {
        use present_identity::feedback::{
            Ack, Decision, Envelope, FEEDBACK_SCHEMA_V3, FaultReason,
        };
        let (mut recorder, mut probe) = recorder_feedback_fixture(2);
        recorder
            .send(Decision::RetireThrough { present_seq: 10 })
            .unwrap();
        // The probe accepted decision 1; decisions 2/3 then fill the ingress
        // while its original fault ACK is already available to the recorder.
        assert_eq!(probe.decisions.try_pop().unwrap().unwrap().ordinal, 1);
        recorder
            .send(Decision::RetireThrough { present_seq: 20 })
            .unwrap();
        recorder
            .send(Decision::RetireThrough { present_seq: 30 })
            .unwrap();
        probe
            .acks
            .try_push(&Envelope {
                schema: FEEDBACK_SCHEMA_V3,
                stream: recorder.stream.clone(),
                ordinal: 1,
                payload: Ack::Fault {
                    decision_ordinal: 1,
                    reason: FaultReason::ReadbackMapFailed,
                },
            })
            .unwrap();
        let error = recorder
            .send(Decision::RetireThrough { present_seq: 40 })
            .unwrap_err();
        let diagnostic = format!("{error:#}");
        assert!(diagnostic.contains("readback_map_failed"), "{diagnostic}");
        assert!(!diagnostic.contains("FeedbackQueueFull"), "{diagnostic}");
        assert_eq!(probe.decisions.stats().unwrap().depth, 2);
        assert_eq!(probe.decisions.stats().unwrap().full_count, 0);
        assert_eq!(recorder.next_decision_ordinal, 4);
    }

    #[test]
    fn recorder_stops_on_peer_fault_without_waiting_for_queue_exhaustion() {
        use present_identity::feedback::{Decision, FaultReason, ProtocolError};
        use present_identity::transport::TransportError;
        let (mut recorder, probe) = recorder_feedback_fixture(256);
        // Readback/pose failure can happen with no decision-specific ACK. It
        // must be visible on the very next send, not after 256 more decisions.
        probe.publish_fault(FaultReason::ReadbackMapFailed).unwrap();
        let error = recorder
            .send(Decision::RetireThrough { present_seq: 10 })
            .unwrap_err();
        assert!(matches!(
            error.downcast_ref::<TransportError>(),
            Some(TransportError::Protocol(ProtocolError::Fault(
                FaultReason::ReadbackMapFailed
            )))
        ));
        assert_eq!(probe.decisions.stats().unwrap().depth, 0);
        assert_eq!(probe.decisions.stats().unwrap().full_count, 0);
        assert!(recorder.pending.is_empty());
        assert_eq!(recorder.next_decision_ordinal, 1);
    }
    use windows::Win32::Media::MediaFoundation::{
        MFNominalRange_16_235, MFVideoPrimaries_BT709, MFVideoTransFunc_709,
        MFVideoTransferMatrix_BT709, eAVEncH264VProfile_High,
    };

    #[test]
    fn watermark_geometry_tracks_uniform_dpi_scaling_and_rejects_letterboxing() {
        assert_eq!(
            scaled_watermark_geometry(1_920, 1_080, 1_920, 1_080).unwrap(),
            (192, 8)
        );
        assert_eq!(
            scaled_watermark_geometry(2_880, 1_620, 1_920, 1_080).unwrap(),
            (288, 12)
        );
        assert!(scaled_watermark_geometry(2_560, 1_440, 1_920, 1_200).is_err());
    }

    #[test]
    fn wgc_observations_before_the_actual_arm_boundary_are_filtered() {
        assert!(!observation_is_at_or_after_arm_boundary(Some(4_999), Some(5_000)).unwrap());
        assert!(observation_is_at_or_after_arm_boundary(Some(5_000), Some(5_000)).unwrap());
        assert!(observation_is_at_or_after_arm_boundary(Some(5_001), Some(5_000)).unwrap());
        assert!(observation_is_at_or_after_arm_boundary(None, None).unwrap());
        assert!(observation_is_at_or_after_arm_boundary(None, Some(5_000)).is_err());
        assert_eq!(
            classify_startup_observation(Some(4_990), true, false, None).unwrap(),
            StartupObservationDisposition::ProveWgcLiveThenArm
        );
        assert_eq!(
            classify_startup_observation(Some(4_999), true, true, None).unwrap(),
            StartupObservationDisposition::Recycle
        );
        assert_eq!(
            classify_startup_observation(Some(4_999), true, true, Some(5_000)).unwrap(),
            StartupObservationDisposition::Recycle
        );
        assert_eq!(
            classify_startup_observation(Some(5_000), true, true, Some(5_000)).unwrap(),
            StartupObservationDisposition::Eligible
        );
        assert!(classify_startup_observation(None, true, false, None).is_err());
        assert!(classify_startup_observation(None, true, true, None).is_err());
    }

    #[test]
    fn corrupt_watermark_observations_are_rejected_before_selection_and_counted() {
        use present_identity::watermark::DecodeError;

        let mut stats = WatermarkObservationStats::default();
        assert_eq!(
            stats
                .classify::<u16>(1, Err(anyhow::Error::new(DecodeError::IntegrityMismatch)))
                .unwrap(),
            None
        );
        assert_eq!(
            stats
                .classify::<u16>(2, Err(anyhow::Error::new(DecodeError::AmbiguousBlock)))
                .unwrap(),
            None
        );
        assert_eq!(stats.rejected, 2);
        assert_eq!(stats.decoded, 0);
        assert_eq!(stats.integrity_mismatch, 1);
        assert_eq!(stats.ambiguous, 1);
        assert_eq!(stats.consecutive, 2);
        assert_eq!(stats.max_consecutive, 2);

        assert_eq!(stats.classify(3, Ok(42_u16)).unwrap(), Some(42));
        assert_eq!(stats.decoded, 1);
        assert_eq!(stats.consecutive, 0);
        assert_eq!(stats.max_consecutive, 2);
    }

    #[test]
    fn bounded_stale_wgc_observation_is_recycled_before_selection_and_counted() {
        let mut stats = WatermarkObservationStats::default();
        let mut decoder = present_identity::FullSequenceDecoder::new(7, 44_260, 88).unwrap();

        assert_eq!(
            stats.expand(10, &mut decoder, 7, 44_264).unwrap(),
            Some(44_264)
        );
        assert_eq!(stats.expand(11, &mut decoder, 7, 44_263).unwrap(), None);
        assert_eq!(decoder.previous(), Some(44_264));
        assert_eq!(stats.stale, 1);
        assert_eq!(stats.max_stale_lag, 1);
        assert_eq!(
            stats.expand(12, &mut decoder, 7, 44_265).unwrap(),
            Some(44_265)
        );
    }

    #[test]
    fn stale_wgc_observation_beyond_one_cfr_window_remains_fatal() {
        let mut stats = WatermarkObservationStats::default();
        let mut decoder = present_identity::FullSequenceDecoder::new(7, 44_260, 88).unwrap();
        assert_eq!(
            stats.expand(10, &mut decoder, 7, 44_264).unwrap(),
            Some(44_264)
        );

        let error = stats.expand(11, &mut decoder, 7, 44_255).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("WGC watermark full-sequence expansion failed for source_seq 11")
        );
        assert_eq!(decoder.previous(), Some(44_264));
        assert_eq!(stats.stale, 0);
    }

    #[test]
    fn watermark_readback_plane_errors_remain_fatal() {
        use present_identity::watermark::DecodeError;

        let mut stats = WatermarkObservationStats::default();
        let error = stats
            .classify::<u16>(9, Err(anyhow::anyhow!("watermark ROI Map failed")))
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "WGC watermark decode failed for source_seq 9"
        );
        assert_eq!(
            error
                .source()
                .expect("fatal decode detail should remain attached")
                .to_string(),
            "watermark ROI Map failed"
        );
        let wrong_shape = stats
            .classify::<u16>(10, Err(anyhow::Error::new(DecodeError::WrongSampleCount)))
            .unwrap_err();
        assert_eq!(
            wrong_shape.to_string(),
            "WGC watermark decode failed for source_seq 10"
        );
        assert_eq!(stats, WatermarkObservationStats::default());
    }

    // The serialized spelling is the audit surface: session artifacts, the
    // diagnostics export, and the reconciliation in docs/54 §6.2 all read these
    // exact strings. A rename here silently reclassifies every recorded session,
    // so pin both the serde encoding and the Display-ish helper together.
    #[test]
    fn catalog_provenance_serializes_to_its_stable_audit_spelling() {
        for (value, expected) in [
            (CatalogProvenance::Compiled, "compiled"),
            (CatalogProvenance::Delivered, "delivered"),
        ] {
            assert_eq!(value.as_str(), expected);
            assert_eq!(
                serde_json::to_string(&value).expect("provenance serializes"),
                format!("\"{expected}\""),
                "serde encoding drifted from as_str for {value:?}"
            );
        }
    }

    #[test]
    fn progress_deadline_skips_missed_ticks_without_backfilling() {
        assert_eq!(
            next_progress_deadline(100, 10_000_100, 10_000_000).unwrap(),
            20_000_100
        );
        assert_eq!(
            next_progress_deadline(100, 35_000_100, 10_000_000).unwrap(),
            40_000_100
        );
        assert!(next_progress_deadline(100, 99, 10_000_000).is_err());
    }

    #[test]
    fn prepared_depth_sidecar_is_the_only_allowed_existing_run_content() {
        let temp = tempfile::tempdir().unwrap();
        let run = temp.path().join("r0000");

        assert!(ensure_run_directory_available(&run).is_ok());
        fs::create_dir_all(&run).unwrap();
        assert!(ensure_run_directory_available(&run).is_err());

        let depth = run.join("depth-lab");
        fs::create_dir(&depth).unwrap();
        fs::write(depth.join("depth-index.jsonl"), b"ready\n").unwrap();
        assert!(ensure_run_directory_available(&run).is_ok());

        fs::write(run.join("failure.json"), b"{}").unwrap();
        assert!(ensure_run_directory_available(&run).is_err());
    }

    #[test]
    fn encoder_timeline_accepts_one_constant_startup_frame_offset() {
        let profile = test_profile();
        let offset = frame_duration_100ns(0, profile.fps_num, profile.fps_den);
        let mut timeline = EncodedTimeline::default();
        for frame in 0..3 {
            let sample = EncodedH264Sample {
                data: Vec::new(),
                pts_100ns: offset + i64::from(frame) * offset,
                duration_100ns: offset,
                sample_flags: 0,
                clean_point: frame == 0,
            };
            timeline.validate(&sample, &profile).unwrap();
            timeline.frames_written += 1;
        }
        assert_eq!(timeline.pts_offset_100ns, Some(offset));
    }

    #[test]
    fn encoder_timeline_rejects_an_offset_change_after_startup() {
        let profile = test_profile();
        let offset = frame_duration_100ns(0, profile.fps_num, profile.fps_den);
        let mut timeline = EncodedTimeline::default();
        let first = EncodedH264Sample {
            data: Vec::new(),
            pts_100ns: offset,
            duration_100ns: offset,
            sample_flags: 0,
            clean_point: true,
        };
        timeline.validate(&first, &profile).unwrap();
        timeline.frames_written = 1;
        let shifted = EncodedH264Sample {
            data: Vec::new(),
            pts_100ns: frame_pts_100ns(1, profile.fps_num, profile.fps_den),
            duration_100ns: frame_duration_100ns(1, profile.fps_num, profile.fps_den),
            sample_flags: 0,
            clean_point: false,
        };

        assert!(timeline.validate(&shifted, &profile).is_err());
    }

    const TEST_MAPPING: &[u8] = br#"{
      "schema_version":"dm-mapping-2",
      "mapping_id":"game.test.pc",
      "version":"1",
      "game":{"id":"test","name":"Test","platform":"pc"},
      "taxonomy":{"content":"act","view":"first_person","material":"3d"},
      "recording_profile_id":"test-recording-v1",
      "bindings":[{"device":"keyboard","scan_code":17,"extended":false,"semantic":"move.forward"}]
    }"#;

    fn test_profile() -> CaptureProfile {
        CaptureProfile {
            profile_id: "test-capture-v1".into(),
            non_trainable: true,
            capture_backend: "native_wgc_window_v1".into(),
            encoder_backend: "mf_nvidia_hardware_v1".into(),
            width: 1920,
            height: 1080,
            fps_num: 30,
            fps_den: 1,
            bitrate_bps: 12_000_000,
            max_bitrate_bps: 18_000_000,
            gop_frames: 120,
            segment_seconds: 10,
            test_segments: 30,
            accepted_encoder_relaxations: None,
            audio: None,
        }
    }

    /// Issues #266/#797: source loss is decided by queue overflow, not by a
    /// shorter wall-clock guess. The encoder deadline is only a liveness guard.
    #[test]
    fn encoder_progress_uses_liveness_watchdog_above_queue_headroom() {
        let profile = test_profile();
        let frequency = 10_000_000; // QPC ticks per second

        let one_frame = frame_period_qpc(&profile, frequency);
        assert_eq!(
            one_frame,
            frequency / 30,
            "30fps profile is one 1/30s period"
        );

        let pipeline = encoder_progress_limit_qpc(&profile, frequency);
        assert_eq!(
            pipeline,
            frequency * ENCODER_PROGRESS_LIVENESS_WATCHDOG_MS / 1_000
        );
        assert!(
            pipeline > one_frame * ENCODER_DRAIN_CFR_FRAMES as i64,
            "the queue-drop signal must win before the liveness watchdog"
        );
    }

    #[test]
    fn encoder_progress_accepts_reported_30fps_133ms_transient_without_source_loss() {
        let profile = test_profile();
        let frequency = 10_000_000;
        let limit = encoder_progress_limit_qpc(&profile, frequency);

        assert!(
            limit > 1_333_330,
            "the reported 133.333ms surface stall must fit when the source queue has not dropped"
        );
    }

    #[test]
    fn encoder_progress_uses_same_wall_clock_watchdog_at_60fps() {
        let mut profile = test_profile();
        profile.fps_num = 60;
        profile.fps_den = 1;
        let frequency = 10_000_000;

        let limit = encoder_progress_limit_qpc(&profile, frequency);
        assert_eq!(
            limit,
            frequency * ENCODER_PROGRESS_LIVENESS_WATCHDOG_MS / 1_000
        );
    }

    #[test]
    fn encoder_progress_never_shrinks_below_queue_coupled_headroom() {
        let mut profile = test_profile();
        profile.fps_num = 1;
        profile.fps_den = 1;
        let frequency = 10_000_000;

        assert_eq!(
            encoder_progress_limit_qpc(&profile, frequency),
            frequency * ENCODER_DRAIN_CFR_FRAMES as i64
        );
    }

    /// The queue still covers the documented four-period data-preservation
    /// window. A longer liveness watchdog does not weaken this fail-closed bound:
    /// actual overflow is checked on every encoder-wait poll and stops first.
    #[test]
    fn capture_queue_retains_encoder_drain_headroom() {
        let arrivals_during_drain =
            WGC_SOURCE_OBSERVATIONS_PER_CFR_FRAME * ENCODER_DRAIN_CFR_FRAMES;
        assert!(
            arrivals_during_drain + WGC_SOURCE_OBSERVATIONS_PER_CFR_FRAME
                <= crate::capture::CAPTURE_QUEUE_STEADY_STATE_CAPACITY,
            "a {ENCODER_DRAIN_CFR_FRAMES}-frame drain admits {arrivals_during_drain} observations; \
             the steady-state queue holds {} and must also keep the selector's window free",
            crate::capture::CAPTURE_QUEUE_STEADY_STATE_CAPACITY,
        );
    }

    #[test]
    fn qpc_millis_are_reported_for_diagnostics_and_never_divide_by_zero() {
        assert_eq!(qpc_to_millis(10_000_000, 10_000_000), 1_000);
        assert_eq!(qpc_to_millis(333_333, 10_000_000), 33);
        assert_eq!(qpc_to_millis(1_000, 0), 0);
    }

    #[test]
    fn hardware_preflight_evidence_serializes_selected_adapter_driver_version() {
        let evidence = HardwarePreflightEvidence {
            schema_version: "dm-hardware-preflight-1",
            scope: "gpu_encoder_path_no_window_capture",
            wgc_frame_observed: false,
            windows: WindowsVersion {
                major: 10,
                minor: 0,
                build: 26_200,
            },
            adapter_description: "NVIDIA GeForce GTX 1060".into(),
            adapter_vendor_id: 0x10de,
            adapter_device_id: 0x1c03,
            adapter_luid_high: 7,
            adapter_luid_low: 42,
            adapter_driver_version: Some("32.0.15.8253".into()),
            d3d_feature_level: 0xb000,
            encoder: HardwareEncoderPreflightEvidence {
                transform_clsid: None,
                friendly_name: None,
                control_policy: "hardware_capability_validated_best_effort_controls",
                relaxed_controls: Vec::new(),
                rate_control_mode: 0,
                mean_bitrate_bps: 12_000_000,
                max_bitrate_bps: 18_000_000,
                gop_frames: 120,
                b_frames: 0,
                low_latency: true,
            },
            profile: HardwareCaptureProfileEvidence {
                profile_id: "test-capture-v1".into(),
                width: 1920,
                height: 1080,
                fps_num: 30,
                fps_den: 1,
            },
            media_probe: Mp4Probe {
                codec: "H.264".into(),
                h264_profile: eAVEncH264VProfile_High.0 as u32,
                coded_width: 1920,
                coded_height: 1080,
                width: 1920,
                height: 1080,
                fps_num: 30,
                fps_den: 1,
                video_primaries: MFVideoPrimaries_BT709.0 as u32,
                transfer_function: MFVideoTransFunc_709.0 as u32,
                yuv_matrix: MFVideoTransferMatrix_BT709.0 as u32,
                nominal_range: MFNominalRange_16_235.0 as u32,
                sample_count: 1,
                first_pts_100ns: 0,
                last_pts_100ns: 0,
                first_sample_clean_point: true,
                pts_monotonic: true,
            },
            gpu_conversion: "d3d11_video_processor_bgra_to_nv12",
            scratch_artifact_retained: false,
        };

        let json = serde_json::to_value(evidence).unwrap();
        assert_eq!(json["adapter_description"], "NVIDIA GeForce GTX 1060");
        assert_eq!(json["adapter_luid_high"], 7);
        assert_eq!(json["adapter_luid_low"], 42);
        assert_eq!(json["adapter_driver_version"], "32.0.15.8253");
    }

    fn catalog_entry(id: &str, version: &str, bytes: Vec<u8>) -> RecordingCatalogEntry {
        RecordingCatalogEntry {
            id: id.into(),
            version: version.into(),
            content_hash: sha256_bytes(&bytes),
            source_bytes: Arc::new(bytes),
        }
    }

    /// consent 被如实记录，但**不再改变分类**（docs/100 §3 推论 3）：带着 consent
    /// 的会话仍然 `non_trainable: true`。这正是删除后想要的失败方向——「录了但标签
    /// 保守」可以一条 UPDATE 修回来，「该录的没录」不可逆。
    #[test]
    fn a_declared_consent_is_recorded_without_making_the_session_trainable() {
        let temp = tempfile::tempdir().unwrap();
        let session_dir = temp.path().join("session-1");
        let mut request = recording_request(temp.path().to_owned());
        request.consent_id = Some("CNS-20260724-001".into());
        ensure_session_contract(&session_dir, &request).unwrap();
        let manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(session_dir.join("session.json")).unwrap()).unwrap();
        assert_eq!(manifest["non_trainable"], true);
        assert_eq!(manifest["consent_id"], "CNS-20260724-001");

        let debug_dir = temp.path().join("session-debug");
        let mut debug_request = recording_request(temp.path().to_owned());
        debug_request.session_id = "session-debug".into();
        ensure_session_contract(&debug_dir, &debug_request).unwrap();
        let manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(debug_dir.join("session.json")).unwrap()).unwrap();
        assert_eq!(manifest["schema_version"], "dm-local-dev-session-2");
        assert_eq!(manifest["capture_policy"]["audio"], false);
        assert_eq!(manifest["non_trainable"], true);
        assert!(manifest.get("consent_id").is_none());
    }

    /// 中性契约的 session.json：schema 换代、两个训练声明字节整体消失、目录身份
    /// 不再自称车道（issue #855 §A.4 / §B）。legacy 的那一份由上面那条测试钉住，
    /// 两条一起构成"断代只影响新一代"的双向证据。
    #[test]
    fn the_neutral_contract_writes_a_session_manifest_without_any_lane_or_training_word() {
        let temp = tempfile::tempdir().unwrap();
        let session_dir = temp.path().join("session-neutral");
        let mut request = recording_request(temp.path().to_owned());
        request.session_id = "session-neutral".into();
        request.catalog.contract = SessionContract::IngestV2;
        request.catalog.mode = None;
        request.catalog.non_trainable = None;
        ensure_session_contract(&session_dir, &request).unwrap();
        let manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(session_dir.join("session.json")).unwrap()).unwrap();
        assert_eq!(manifest["schema_version"], "dm-session-ingest-2");
        assert!(manifest.get("non_trainable").is_none());
        assert!(manifest.get("production_certified").is_none());
        assert!(manifest["catalog"].get("mode").is_none());
        assert!(manifest["catalog"].get("non_trainable").is_none());
        // provenance 一个字节没少：去掉的是车道词，不是身份与来源（§A.4）。
        assert_eq!(manifest["catalog"]["provenance"], "compiled");
        assert_eq!(manifest["catalog"]["catalog_id"], "test-catalog");
    }

    /// 目录上下文与它自称的契约必须自洽，两个方向都拦：legacy 少带一个 legacy
    /// 字节要拒，中性多带一个也要拒。少了后半条，"忘了清空"就会静默产出一份
    /// 自称中性、却仍然申报训练分类的会话。
    #[test]
    fn a_catalog_context_that_disagrees_with_its_contract_is_refused_in_both_directions() {
        let temp = tempfile::tempdir().unwrap();
        let mut half_migrated = recording_request(temp.path().to_owned());
        half_migrated.catalog.contract = SessionContract::IngestV2;
        let error = validate_catalog_contract_pair(&half_migrated).unwrap_err();
        assert!(
            format!("{error:#}").contains("does not match its ingest contract"),
            "unexpected error: {error:#}"
        );

        let mut stripped_legacy = recording_request(temp.path().to_owned());
        stripped_legacy.catalog.mode = None;
        stripped_legacy.catalog.non_trainable = None;
        let error = validate_catalog_contract_pair(&stripped_legacy).unwrap_err();
        assert!(
            format!("{error:#}").contains("does not match its ingest contract"),
            "unexpected error: {error:#}"
        );

        validate_catalog_contract_pair(&recording_request(temp.path().to_owned())).unwrap();
    }

    #[test]
    fn process_loopback_session_uses_v3_and_an_explicit_audio_policy() {
        let temp = tempfile::tempdir().unwrap();
        let session_dir = temp.path().join("session-audio");
        let mut request = recording_request(temp.path().to_owned());
        request.session_id = "session-audio".into();
        request.profile.audio = Some(crate::profile::CaptureAudioProfile::process_loopback());
        let capture_bytes = serde_json::to_vec_pretty(&request.profile).unwrap();
        request.catalog.recording.source_bytes = Arc::new(capture_bytes.clone());
        request.catalog.recording.content_hash = sha256_bytes(&capture_bytes);

        ensure_session_contract(&session_dir, &request).unwrap();
        let manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(session_dir.join("session.json")).unwrap()).unwrap();
        assert_eq!(manifest["schema_version"], "dm-local-dev-session-3");
        assert_eq!(
            manifest["capture_policy"]["audio"]["mode"],
            "process_loopback"
        );
        assert_eq!(
            manifest["capture_policy"]["audio"]["sample_rate_hz"],
            48_000
        );
    }

    #[test]
    fn audio_seal_moves_a_verified_sidecar_into_segment_staging() {
        let temp = tempfile::tempdir().unwrap();
        let spool = temp.path().join("00000.audio.pcm.zst.sealed");
        let staging = temp.path().join("segment.tmp");
        fs::create_dir_all(&staging).unwrap();
        let pcm = [1_u8, 2, 3, 4].repeat(48_000);
        let compressed = zstd::stream::encode_all(pcm.as_slice(), 3).unwrap();
        fs::write(&spool, &compressed).unwrap();
        let sha256 = sha256_file(&spool).unwrap();
        let metadata = SegmentAudioMetadata {
            state: SegmentAudioState::Ready,
            source: crate::audio_contract::AudioSource::ProcessLoopback,
            path: Some(AUDIO_ARTIFACT_NAME.into()),
            sha256: Some(sha256.clone()),
            bytes: Some(compressed.len() as u64),
            format: crate::audio_contract::AudioPcmFormat::default(),
            qpc_frequency: 10_000_000,
            first_sample_qpc: Some(1_000_000),
            frames: Some(48_000),
            failure_code: None,
            health: SegmentAudioHealth::default(),
        };
        let published = publish_audio_seal(
            AudioSegmentSeal {
                segment_index: 0,
                qpc_start: 1_000_000,
                qpc_end: 11_000_000,
                path: Some(spool.clone()),
                metadata,
            },
            &staging,
        )
        .unwrap();

        assert!(!spool.exists());
        let destination = staging.join(AUDIO_ARTIFACT_NAME);
        assert!(destination.is_file());
        assert_eq!(published.sha256.as_deref(), Some(sha256.as_str()));
        assert_eq!(
            zstd::stream::decode_all(File::open(destination).unwrap()).unwrap(),
            pcm
        );
    }

    fn recording_request(output_root: PathBuf) -> RecordingRequest {
        let profile = test_profile();
        let mapping = MappingSnapshot::from_slice(TEST_MAPPING).unwrap();
        let catalog_bytes = br#"{"schema_version":"dm-test-catalog-1"}"#.to_vec();
        let recording_bytes = serde_json::to_vec_pretty(&profile).unwrap();
        let catalog = RecordingCatalogContext {
            contract: SessionContract::LocalDevV1,
            mode: Some("dev_non_trainable".into()),
            non_trainable: Some(true),
            provenance: CatalogProvenance::Compiled,
            catalog_id: "test-catalog".into(),
            catalog_version: "1".into(),
            catalog_content_hash: sha256_bytes(&catalog_bytes),
            catalog_source_bytes: Arc::new(catalog_bytes),
            device: catalog_entry("test-device", "1", b"{\"device\":1}".to_vec()),
            recording: catalog_entry(&profile.profile_id, "1", recording_bytes),
            game: catalog_entry("test", "1", b"{\"game\":1}".to_vec()),
            mapping: catalog_entry("game.test.pc", "1", TEST_MAPPING.to_vec()),
        };
        let input_status = RawInputStatus::default();
        let (_input_records_tx, input_records) = bounded(1);
        let (commands, _command_rx) = bounded::<crate::input::RawInputThreadCommand>(1);
        let (emergency_records, _emergency_rx) = bounded(1);
        RecordingRequest {
            session_id: "session-1".into(),
            run_id: "r0000".into(),
            session_frame_start: 0,
            output_root,
            target: RecordingTarget {
                hwnd_value: 1,
                process_id: 42,
                target_id: "target-old".into(),
                game_id: "test".into(),
                executable_sha256: Some("sha256:exe".into()),
            },
            profile,
            segment_policy_override: None,
            mapping,
            catalog,
            consent_id: None,
            present_identity: None,
            selected_present_feedback: None,
            selected_depth_cadence: Default::default(),
            control_vks: Vec::new(),
            cutoff_qpc: Arc::new(AtomicI64::new(i64::MAX)),
            input_records,
            input_control: RawInputControl::new(
                1,
                commands,
                emergency_records,
                input_status.clone(),
            ),
            input_status,
        }
    }

    #[test]
    fn keyframe_schedule_uses_nonzero_run_session_frame_start() {
        assert!(should_force_keyframe(189, 51, 51, 120).unwrap());
    }

    #[test]
    fn keyframe_schedule_forces_each_segment_first_frame() {
        assert!(should_force_keyframe(189, 111, 0, 120).unwrap());
    }

    #[test]
    fn keyframe_schedule_forces_global_session_gop_boundary() {
        assert!(should_force_keyframe(0, 240, 240, 120).unwrap());
    }

    #[test]
    fn keyframe_schedule_does_not_force_an_ordinary_frame() {
        assert!(!should_force_keyframe(189, 50, 50, 120).unwrap());
    }

    #[test]
    fn keyframe_schedule_rejects_zero_gop_and_frame_overflow() {
        assert!(should_force_keyframe(0, 0, 0, 0).is_err());
        assert!(should_force_keyframe(u64::MAX, 1, 1, 120).is_err());
    }

    #[test]
    fn internal_gop_keyframe_waits_for_prior_encoder_inputs() {
        assert!(keyframe_requires_encoder_barrier(120, true));
        assert!(keyframe_requires_encoder_barrier(360, true));
        assert!(!keyframe_requires_encoder_barrier(0, true));
        assert!(!keyframe_requires_encoder_barrier(119, false));
    }

    #[test]
    fn window_destroy_upgrades_foreground_loss_without_delaying_cutoff() {
        let cutoff = AtomicI64::new(i64::MAX);
        let mut observed = None;
        observe_stop(
            &cutoff,
            &mut observed,
            RecordingStopSignal {
                reason: RecordingStopReason::ForegroundLost,
                cutoff_qpc: 100,
            },
        );
        observe_stop(
            &cutoff,
            &mut observed,
            RecordingStopSignal {
                reason: RecordingStopReason::WindowDestroyed,
                cutoff_qpc: 110,
            },
        );

        let observed = observed.unwrap();
        assert_eq!(observed.reason, RecordingStopReason::WindowDestroyed);
        assert_eq!(observed.cutoff_qpc, 100);
        assert_eq!(cutoff.load(Ordering::Acquire), 100);
    }

    #[test]
    fn unrelated_later_stop_does_not_replace_the_first_cause() {
        let cutoff = AtomicI64::new(i64::MAX);
        let mut observed = None;
        observe_stop(
            &cutoff,
            &mut observed,
            RecordingStopSignal {
                reason: RecordingStopReason::Manual,
                cutoff_qpc: 100,
            },
        );
        observe_stop(
            &cutoff,
            &mut observed,
            RecordingStopSignal {
                reason: RecordingStopReason::WindowDestroyed,
                cutoff_qpc: 110,
            },
        );

        let observed = observed.unwrap();
        assert_eq!(observed.reason, RecordingStopReason::Manual);
        assert_eq!(observed.cutoff_qpc, 100);
    }

    #[test]
    fn session_v2_manifest_allows_rebind_but_rejects_stable_identity_tampering() {
        let temp = tempfile::tempdir().unwrap();
        let session_dir = temp.path().join("session-1");
        let first = recording_request(temp.path().to_owned());
        ensure_session_contract(&session_dir, &first).unwrap();
        let original = fs::read(session_dir.join("session.json")).unwrap();
        let manifest: serde_json::Value = serde_json::from_slice(&original).unwrap();
        assert_eq!(manifest["capture_policy"]["scale"], true);
        assert_eq!(
            manifest["capture_policy"]["minimum_source_width"],
            first.profile.source_geometry_rule().dimensions().0
        );
        assert_eq!(
            manifest["capture_policy"]["minimum_source_height"],
            first.profile.source_geometry_rule().dimensions().1
        );

        let mut rebound = recording_request(temp.path().to_owned());
        rebound.run_id = "r0001".into();
        rebound.target.hwnd_value = 2;
        rebound.target.target_id = "target-new".into();
        ensure_session_contract(&session_dir, &rebound).unwrap();
        assert_eq!(
            fs::read(session_dir.join("session.json")).unwrap(),
            original
        );

        let mut tampered: serde_json::Value = serde_json::from_slice(&original).unwrap();
        tampered["mapping"]["version"] = serde_json::json!("tampered");
        let tampered_bytes = serde_json::to_vec_pretty(&tampered).unwrap();
        fs::write(session_dir.join("session.json"), &tampered_bytes).unwrap();
        assert!(ensure_session_contract(&session_dir, &rebound).is_err());
        assert_eq!(
            fs::read(session_dir.join("session.json")).unwrap(),
            tampered_bytes
        );
    }

    #[test]
    fn scratch_artifact_cleanup_removes_only_an_empty_fixed_parent() {
        let temp = tempfile::tempdir().unwrap();
        let scratch_root = temp.path().join(".preflight-scratch");
        fs::create_dir_all(&scratch_root).unwrap();
        let first_path = scratch_root.join("hardware-preflight-1.mp4");
        fs::write(&first_path, b"probe").unwrap();
        ScratchArtifact::new(first_path).remove().unwrap();
        assert!(!scratch_root.exists());

        fs::create_dir_all(&scratch_root).unwrap();
        let second_path = scratch_root.join("hardware-preflight-2.mp4");
        fs::write(&second_path, b"probe").unwrap();
        fs::write(scratch_root.join("crash-residue.mp4"), b"retain").unwrap();
        ScratchArtifact::new(second_path).remove().unwrap();
        assert!(scratch_root.join("crash-residue.mp4").is_file());
    }
}

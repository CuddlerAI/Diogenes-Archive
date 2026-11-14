//! Pure selected-Present feedback protocol and validation state machine.
//!
//! The hot transport is two independent SPSC queues. This module deliberately
//! contains no file polling, OS handles, locks, or multi-producer arbitration.

#[cfg(test)]
mod even_cfr_tests;

use std::collections::VecDeque;

/// V3 carries both WGC callback-entry and callback-ready timestamps. WGC source
/// time and process QPC are not a strict causal pair; consumers preserve both
/// and report source-ahead inversions separately instead of rejecting them.
pub const FEEDBACK_SCHEMA_V3: u16 = 3;
/// Explicit CP2077 raw-only opt-in. V3 remains every RGB frame and wire-compatible.
pub const FEEDBACK_SCHEMA_V4: u16 = 4;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DepthSelectionCadence {
    #[default]
    EveryRgbFrame,
    EvenCfrFrames,
}

impl DepthSelectionCadence {
    pub const fn schema(self) -> u16 {
        match self {
            Self::EveryRgbFrame => FEEDBACK_SCHEMA_V3,
            Self::EvenCfrFrames => FEEDBACK_SCHEMA_V4,
        }
    }

    pub const fn requires_depth(self, session_frame: u64) -> bool {
        matches!(self, Self::EveryRgbFrame) || session_frame.is_multiple_of(2)
    }
}
pub const SESSION_ID_CAPACITY: usize = 64;
pub const RUN_ID_CAPACITY: usize = 32;
pub const DEFAULT_QUEUE_CAPACITY: usize = 256;
pub const DEFAULT_RING_CAPACITY: u32 = 96;
pub const DEFAULT_MAX_FEEDBACK_LAG_PRESENTS: u32 = 88;
pub const DEFAULT_RING_GUARD_SLOTS: u32 = 8;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct WireIdentity {
    session_len: u8,
    session_id: [u8; SESSION_ID_CAPACITY],
    run_len: u8,
    run_id: [u8; RUN_ID_CAPACITY],
}

impl WireIdentity {
    pub fn new(session_id: &str, run_id: &str) -> Result<Self, ProtocolError> {
        if session_id.is_empty() || session_id.len() > SESSION_ID_CAPACITY {
            return Err(ProtocolError::InvalidIdentityLength);
        }
        if run_id.is_empty() || run_id.len() > RUN_ID_CAPACITY {
            return Err(ProtocolError::InvalidIdentityLength);
        }
        let mut value = Self {
            session_len: session_id.len() as u8,
            session_id: [0; SESSION_ID_CAPACITY],
            run_len: run_id.len() as u8,
            run_id: [0; RUN_ID_CAPACITY],
        };
        value.session_id[..session_id.len()].copy_from_slice(session_id.as_bytes());
        value.run_id[..run_id.len()].copy_from_slice(run_id.as_bytes());
        Ok(value)
    }

    #[must_use]
    pub fn session_id(&self) -> &str {
        std::str::from_utf8(&self.session_id[..usize::from(self.session_len)])
            .expect("WireIdentity is only constructed from UTF-8")
    }

    #[must_use]
    pub fn run_id(&self) -> &str {
        std::str::from_utf8(&self.run_id[..usize::from(self.run_len)])
            .expect("WireIdentity is only constructed from UTF-8")
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct StreamEpoch {
    pub identity: WireIdentity,
    pub generation: u64,
    /// Version of the probe-owned GPU candidate resources. Version 1 keeps this
    /// immutable from Ready to Seal; a resize faults the generation.
    pub resource_epoch: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Envelope<T> {
    pub schema: u16,
    pub stream: StreamEpoch,
    pub ordinal: u64,
    pub payload: T,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Decision {
    /// Starts the run-time candidate budget after WGC has established its
    /// capture session. The probe does not ACK this decision until the first
    /// GPU candidate has actually been materialized.
    ArmGeneration,
    Select {
        session_frame: u64,
        present_seq: u64,
        /// False still commits the RGB identity, but must not submit depth.
        depth_required: bool,
        duplicate: bool,
        source_superseded: u32,
        /// WGC observation timestamp in the system QPC domain.
        source_qpc: i64,
        /// QPC captured at the beginning of the WGC FrameArrived callback.
        callback_start_qpc: i64,
        /// QPC captured after the WGC frame and GPU-copy commands are ready.
        callback_ready_qpc: i64,
        /// Recorder timestamp immediately before enqueueing this decision.
        decision_qpc: i64,
    },
    RetireThrough {
        present_seq: u64,
    },
    SealGeneration {
        final_session_frame: Option<u64>,
        expected_select_count: u64,
        expected_distinct_depth_count: u64,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Ack {
    GenerationArmed {
        decision_ordinal: u64,
        /// The first candidate that was actually materialized, not a predicted
        /// next Present identity.
        first_candidate_present_seq: u64,
    },
    SelectAccepted {
        decision_ordinal: u64,
        session_frame: u64,
        present_seq: u64,
        depth_required: bool,
        depth_already_selected: bool,
    },
    Retired {
        decision_ordinal: u64,
        present_seq: u64,
    },
    GenerationSealed {
        decision_ordinal: u64,
        select_count: u64,
        distinct_depth_count: u64,
    },
    Fault {
        decision_ordinal: u64,
        reason: FaultReason,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u16)]
pub enum FaultReason {
    FeedbackQueueFull = 1,
    AckQueueFull = 2,
    IdentityMismatch = 3,
    GenerationMismatch = 4,
    ResourceEpochMismatch = 5,
    DecisionOrdinalGap = 6,
    SessionFrameConflict = 7,
    DuplicateSourceMismatch = 8,
    RetireFrontierRegressed = 9,
    SelectAfterRetire = 10,
    SelectAfterSeal = 11,
    SealCountMismatch = 12,
    StopBeforeGenerationSealed = 13,
    SelectedFeedbackLate = 14,
    RingOverwriteUnresolved = 15,
    RingIdentityMismatch = 16,
    ResizeAfterReady = 17,
    ReadbackPoolFull = 18,
    ReadbackTimeout = 19,
    ReadbackMapFailed = 20,
    WriterQueueFull = 21,
    WatermarkWriteFailed = 22,
    VramBudgetInsufficient = 23,
    CandidateAllocationFailed = 24,
    FlushCountMismatch = 25,
    PeerExited = 26,
    TransportCorrupt = 27,
    PresentSeqRegressed = 28,
    FeedbackTimestampInvalid = 29,
    FeedbackTelemetryOverflow = 30,
    SwapchainAdapterMismatch = 31,
    ArmRepeated = 32,
    ArmAfterSeal = 33,
    CandidateBeforeArm = 34,
    FeedbackBeforeArm = 35,
    ArmBoundaryMissing = 36,
}

impl FaultReason {
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::FeedbackQueueFull => "feedback_queue_full",
            Self::AckQueueFull => "ack_queue_full",
            Self::IdentityMismatch => "identity_mismatch",
            Self::GenerationMismatch => "generation_mismatch",
            Self::ResourceEpochMismatch => "resource_epoch_mismatch",
            Self::DecisionOrdinalGap => "decision_ordinal_gap",
            Self::SessionFrameConflict => "session_frame_conflict",
            Self::DuplicateSourceMismatch => "duplicate_source_mismatch",
            Self::RetireFrontierRegressed => "retire_frontier_regressed",
            Self::SelectAfterRetire => "select_after_retire",
            Self::SelectAfterSeal => "select_after_seal",
            Self::SealCountMismatch => "seal_count_mismatch",
            Self::StopBeforeGenerationSealed => "stop_before_generation_sealed",
            Self::SelectedFeedbackLate => "selected_feedback_late",
            Self::RingOverwriteUnresolved => "ring_overwrite_unresolved",
            Self::RingIdentityMismatch => "ring_identity_mismatch",
            Self::ResizeAfterReady => "resize_after_ready",
            Self::ReadbackPoolFull => "readback_pool_full",
            Self::ReadbackTimeout => "readback_timeout",
            Self::ReadbackMapFailed => "readback_map_failed",
            Self::WriterQueueFull => "writer_queue_full",
            Self::WatermarkWriteFailed => "watermark_write_failed",
            Self::VramBudgetInsufficient => "vram_budget_insufficient",
            Self::CandidateAllocationFailed => "candidate_allocation_failed",
            Self::FlushCountMismatch => "flush_count_mismatch",
            Self::PeerExited => "peer_exited",
            Self::TransportCorrupt => "transport_corrupt",
            Self::PresentSeqRegressed => "present_seq_regressed",
            Self::FeedbackTimestampInvalid => "feedback_timestamp_invalid",
            Self::FeedbackTelemetryOverflow => "feedback_telemetry_overflow",
            Self::SwapchainAdapterMismatch => "swapchain_adapter_mismatch",
            Self::ArmRepeated => "arm_repeated",
            Self::ArmAfterSeal => "arm_after_seal",
            Self::CandidateBeforeArm => "candidate_before_arm",
            Self::FeedbackBeforeArm => "feedback_before_arm",
            Self::ArmBoundaryMissing => "arm_boundary_missing",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ProtocolError {
    #[error("session_id/run_id is empty or exceeds the fixed wire capacity")]
    InvalidIdentityLength,
    #[error("feedback schema mismatch")]
    SchemaMismatch,
    #[error("feedback stream identity mismatch: {0}")]
    StreamMismatch(&'static str),
    #[error("feedback decision ordinal gap: expected {expected}, got {actual}")]
    OrdinalGap { expected: u64, actual: u64 },
    #[error("feedback protocol fault: {0:?}")]
    Fault(FaultReason),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RingBudget {
    pub capacity: u32,
    pub max_feedback_lag_presents: u32,
    pub guard_slots: u32,
    pub width: u32,
    pub height: u32,
    pub bytes_per_pixel: u32,
    pub vram_limit_bytes: u64,
}

impl RingBudget {
    pub fn validate(self) -> Result<u64, ProtocolError> {
        if self.capacity == 0
            || self.max_feedback_lag_presents == 0
            || self.max_feedback_lag_presents.checked_add(self.guard_slots) != Some(self.capacity)
        {
            return Err(ProtocolError::Fault(FaultReason::VramBudgetInsufficient));
        }
        let bytes = u64::from(self.capacity)
            .checked_mul(u64::from(self.width))
            .and_then(|value| value.checked_mul(u64::from(self.height)))
            .and_then(|value| value.checked_mul(u64::from(self.bytes_per_pixel)))
            .ok_or(ProtocolError::Fault(FaultReason::VramBudgetInsufficient))?;
        if bytes > self.vram_limit_bytes {
            return Err(ProtocolError::Fault(FaultReason::VramBudgetInsufficient));
        }
        Ok(bytes)
    }
}

/// Largest WGC clock lead that compositor scheduling still explains.
///
/// One 30 fps CFR frame. The lead exists because the compositor stamps a frame
/// with when it *schedules* the present, so its physical ceiling is roughly one
/// refresh interval; the slowest CFR lane is the most permissive interval this
/// pipeline ever runs at. Fallout 4 on an RTX 5060 Ti leads `callback_start` by
/// ~10.7 ms, so this leaves about 3x headroom over measured hardware.
///
/// This is a ceiling, not a licence: a real identity mix-up — wrong session,
/// run, generation, resource epoch or Present epoch — puts `source` orders of
/// magnitude past this, and every one of those has its own fail-closed check
/// that this bound does not touch.
pub const MAX_WGC_SOURCE_LEAD_US: i64 = 33_333;

/// The five QPC readings that accompany one CFR `Select`.
///
/// Four of them this codebase takes itself, in this order: the WGC callback
/// opens (`callback_start`), the frame and its GPU copy commands are ready
/// (`callback_ready`), the recorder commits the CFR slot (`decision`), and the
/// probe reads that decision off the queue (`process`).
///
/// `source` is different in kind. It is WGC `SystemRelativeTime` — the
/// compositor's own time for the frame, not an observation this process made —
/// and it legitimately runs *ahead* of all four. Ordering it against `decision`
/// or `process` therefore does not test the clock: it tests how long our own
/// CFR window happened to take, because `decision` waits for the next CFR slot
/// (up to a full frame) and `process` adds the queue hop on top. A lead that
/// merely outran that wait used to stop the recording.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SelectTimestamps {
    pub source: i64,
    pub callback_start: i64,
    pub callback_ready: i64,
    pub decision: i64,
    pub process: i64,
}

/// One `Select`'s latency sample, pre-split by direction.
///
/// Every field is an unsigned tick count that has already been proven
/// non-negative, so no consumer subtracts two QPC readings and casts the result
/// hoping it was positive. The `Option`s are the two places the compositor
/// clock can invert against ours; `None` means "this sample belongs to the
/// other distribution", never "zero".
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SelectLatency {
    /// `callback_ready - source`, only while the compositor clock is behind us.
    pub wgc_delivery_ticks: Option<u64>,
    /// `source - callback_ready`, only while it leads.
    pub source_lead_callback_ready_ticks: Option<u64>,
    pub callback_to_decision_ticks: u64,
    /// `process - source`, only while the probe read is not behind the
    /// compositor clock.
    pub source_to_probe_ticks: Option<u64>,
    /// `source - process`, only while it leads even the probe read.
    pub source_lead_probe_ticks: Option<u64>,
    pub decision_to_probe_ticks: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error("{detail}")]
pub struct TimestampFault {
    pub reason: FaultReason,
    pub detail: String,
}

impl SelectTimestamps {
    /// Splits one `Select` into latency samples, or faults fail-closed.
    ///
    /// Strict on our own four readings, because this process takes all of them
    /// in a fixed order and any inversion there is a real defect:
    /// `callback_start <= callback_ready <= decision <= process`.
    ///
    /// The compositor clock only has to be positive and stay within
    /// [`MAX_WGC_SOURCE_LEAD_US`] of the moment we first saw the frame.
    /// `callback_start` is the reference because it is the first reading we
    /// take, which makes it both the earliest and the strictest one available;
    /// `decision` and `process` carry our own scheduling and are deliberately
    /// not used as bounds.
    ///
    /// Cross-frame monotonicity of `source` is *not* re-checked here. WGC
    /// ingress already drops non-monotonic source frames before they can reach
    /// a CFR decision, and a duplicate alias legitimately repeats the previous
    /// `source`, so a monotonic check at this layer would reject valid input
    /// while adding no coverage.
    pub fn classify(self, qpc_frequency: u64) -> Result<SelectLatency, TimestampFault> {
        let Self {
            source,
            callback_start,
            callback_ready,
            decision,
            process,
        } = self;
        let fault = |reason, detail: String| TimestampFault { reason, detail };
        if qpc_frequency == 0 {
            return Err(fault(
                FaultReason::FeedbackTimestampInvalid,
                "QPC frequency is zero; QPC deltas cannot be converted to time".into(),
            ));
        }
        if source <= 0 || callback_start <= 0 {
            return Err(fault(
                FaultReason::FeedbackTimestampInvalid,
                format!(
                    "non-positive feedback QPC source={source} callback_start={callback_start}"
                ),
            ));
        }
        if callback_start > callback_ready || callback_ready > decision || decision > process {
            return Err(fault(
                FaultReason::FeedbackTimestampInvalid,
                format!(
                    "feedback QPC readings taken by this process are out of order \
                     callback_start={callback_start} callback_ready={callback_ready} \
                     decision={decision} process={process}"
                ),
            ));
        }
        let lead_ticks = i128::from(source) - i128::from(callback_start);
        let max_lead_ticks =
            i128::from(MAX_WGC_SOURCE_LEAD_US) * i128::from(qpc_frequency) / 1_000_000;
        if lead_ticks > max_lead_ticks {
            let lead_us = lead_ticks * 1_000_000 / i128::from(qpc_frequency);
            return Err(fault(
                FaultReason::FeedbackTimestampInvalid,
                format!(
                    "WGC source clock leads the capture callback by {lead_us}us, over the \
                     {MAX_WGC_SOURCE_LEAD_US}us ceiling (source={source} \
                     callback_start={callback_start} qpc_frequency={qpc_frequency})"
                ),
            ));
        }
        // Exactly one side of each pair is populated. Splitting on `>` rather
        // than converting both differences is what puts an equal pair in one
        // distribution instead of both: a sample counted twice would satisfy
        // the summary's behind+ahead reconciliation while hiding a dropped one.
        let (wgc_delivery_ticks, source_lead_callback_ready_ticks) = if source > callback_ready {
            (None, Some((source - callback_ready) as u64))
        } else {
            (Some((callback_ready - source) as u64), None)
        };
        let (source_to_probe_ticks, source_lead_probe_ticks) = if source > process {
            (None, Some((source - process) as u64))
        } else {
            (Some((process - source) as u64), None)
        };
        Ok(SelectLatency {
            wgc_delivery_ticks,
            source_lead_callback_ready_ticks,
            callback_to_decision_ticks: (decision - callback_ready) as u64,
            source_to_probe_ticks,
            source_lead_probe_ticks,
            decision_to_probe_ticks: (process - decision) as u64,
        })
    }
}

/// A deterministic model of one SPSC direction. Production gives one instance
/// to each direction; there is intentionally no bidirectional queue type.
#[derive(Clone, Debug)]
pub struct SpscModel<T> {
    capacity: usize,
    values: VecDeque<T>,
    pub max_depth: usize,
    pub full_count: u64,
}

impl<T> SpscModel<T> {
    pub fn new(capacity: usize) -> Result<Self, ProtocolError> {
        if capacity == 0 {
            return Err(ProtocolError::Fault(FaultReason::TransportCorrupt));
        }
        Ok(Self {
            capacity,
            values: VecDeque::with_capacity(capacity),
            max_depth: 0,
            full_count: 0,
        })
    }

    pub fn push(&mut self, value: T, full: FaultReason) -> Result<(), ProtocolError> {
        if self.values.len() == self.capacity {
            self.full_count = self.full_count.saturating_add(1);
            return Err(ProtocolError::Fault(full));
        }
        self.values.push_back(value);
        self.max_depth = self.max_depth.max(self.values.len());
        Ok(())
    }

    pub fn pop(&mut self) -> Option<T> {
        self.values.pop_front()
    }
}

#[derive(Clone, Debug)]
pub struct FeedbackState {
    stream: StreamEpoch,
    cadence: DepthSelectionCadence,
    next_ordinal: u64,
    last_session_frame: Option<u64>,
    last_present_seq: Option<u64>,
    last_depth_present_seq: Option<u64>,
    retired_through: Option<u64>,
    select_count: u64,
    distinct_depth_count: u64,
    armed: bool,
    sealed: bool,
}

impl FeedbackState {
    #[must_use]
    pub fn new(stream: StreamEpoch) -> Self {
        Self::with_cadence(stream, DepthSelectionCadence::EveryRgbFrame)
    }

    pub fn with_cadence(stream: StreamEpoch, cadence: DepthSelectionCadence) -> Self {
        Self {
            stream,
            cadence,
            next_ordinal: 1,
            last_session_frame: None,
            last_present_seq: None,
            last_depth_present_seq: None,
            retired_through: None,
            select_count: 0,
            distinct_depth_count: 0,
            armed: false,
            sealed: false,
        }
    }

    pub fn apply(&mut self, message: Envelope<Decision>) -> Result<Ack, ProtocolError> {
        self.apply_with_arm_boundary(message, None)
    }

    /// All committed RGB slots, and distinct depth readbacks (not duplicate references).
    pub fn counts(&self) -> (u64, u64) {
        (self.select_count, self.distinct_depth_count)
    }

    /// Commits a previously staged Arm decision at the exact Present where
    /// the probe completed its first candidate materialization.
    pub fn apply_arm(
        &mut self,
        message: Envelope<Decision>,
        first_candidate_present_seq: u64,
    ) -> Result<Ack, ProtocolError> {
        self.apply_with_arm_boundary(message, Some(first_candidate_present_seq))
    }

    /// Validates an Arm envelope without advancing the decision ordinal. This
    /// lets the Present-thread consumer open the GPU gate, retain the sole
    /// queue item, and ACK only after real materialization on a later Present.
    pub fn stage_arm(&self, message: &Envelope<Decision>) -> Result<(), ProtocolError> {
        self.validate_envelope(message)?;
        if !matches!(message.payload, Decision::ArmGeneration) {
            return Err(ProtocolError::Fault(FaultReason::ArmBoundaryMissing));
        }
        if self.sealed {
            return Err(ProtocolError::Fault(FaultReason::ArmAfterSeal));
        }
        if self.armed {
            return Err(ProtocolError::Fault(FaultReason::ArmRepeated));
        }
        Ok(())
    }

    fn apply_with_arm_boundary(
        &mut self,
        message: Envelope<Decision>,
        arm_boundary: Option<u64>,
    ) -> Result<Ack, ProtocolError> {
        self.validate_envelope(&message)?;
        let ordinal = message.ordinal;
        let ack = match message.payload {
            Decision::ArmGeneration => {
                if self.sealed {
                    return Err(ProtocolError::Fault(FaultReason::ArmAfterSeal));
                }
                if self.armed {
                    return Err(ProtocolError::Fault(FaultReason::ArmRepeated));
                }
                let first_candidate_present_seq =
                    arm_boundary.ok_or(ProtocolError::Fault(FaultReason::ArmBoundaryMissing))?;
                self.armed = true;
                Ack::GenerationArmed {
                    decision_ordinal: ordinal,
                    first_candidate_present_seq,
                }
            }
            Decision::Select {
                session_frame,
                present_seq,
                depth_required,
                duplicate,
                source_superseded: _,
                source_qpc: _,
                callback_start_qpc: _,
                callback_ready_qpc: _,
                decision_qpc: _,
            } => {
                if !self.armed {
                    return Err(ProtocolError::Fault(FaultReason::FeedbackBeforeArm));
                }
                if self.sealed {
                    return Err(ProtocolError::Fault(FaultReason::SelectAfterSeal));
                }
                if depth_required != self.cadence.requires_depth(session_frame)
                    || (self.cadence == DepthSelectionCadence::EvenCfrFrames
                        && self.last_session_frame.is_none()
                        && session_frame != 0)
                {
                    return Err(ProtocolError::Fault(FaultReason::SessionFrameConflict));
                }
                if self
                    .last_present_seq
                    .is_some_and(|previous| present_seq < previous)
                {
                    return Err(ProtocolError::Fault(FaultReason::PresentSeqRegressed));
                }
                if self
                    .retired_through
                    .is_some_and(|retired| present_seq <= retired)
                    && self.last_depth_present_seq != Some(present_seq)
                {
                    return Err(ProtocolError::Fault(FaultReason::SelectAfterRetire));
                }
                if let Some(previous_frame) = self.last_session_frame
                    && session_frame != previous_frame.saturating_add(1)
                {
                    return Err(ProtocolError::Fault(FaultReason::SessionFrameConflict));
                }
                if duplicate && self.last_present_seq != Some(present_seq) {
                    return Err(ProtocolError::Fault(FaultReason::DuplicateSourceMismatch));
                }
                let depth_already_selected = self.last_depth_present_seq == Some(present_seq);
                self.select_count = self.select_count.saturating_add(1);
                if depth_required && !depth_already_selected {
                    self.distinct_depth_count = self.distinct_depth_count.saturating_add(1);
                    self.last_depth_present_seq = Some(present_seq);
                }
                self.last_session_frame = Some(session_frame);
                self.last_present_seq = Some(present_seq);
                Ack::SelectAccepted {
                    decision_ordinal: ordinal,
                    session_frame,
                    present_seq,
                    depth_required,
                    depth_already_selected,
                }
            }
            Decision::RetireThrough { present_seq } => {
                if !self.armed {
                    return Err(ProtocolError::Fault(FaultReason::FeedbackBeforeArm));
                }
                if self.sealed {
                    return Err(ProtocolError::Fault(FaultReason::SelectAfterSeal));
                }
                // An odd CFR slot may be inherited by the next even slot. Do not
                // retire its unselected candidate until the RGB source advances.
                if self.cadence == DepthSelectionCadence::EvenCfrFrames
                    && self.last_present_seq.is_some_and(|seq| present_seq >= seq)
                    && self.last_depth_present_seq != self.last_present_seq
                {
                    return Err(ProtocolError::Fault(FaultReason::SelectAfterRetire));
                }
                if self
                    .retired_through
                    .is_some_and(|retired| present_seq < retired)
                {
                    return Err(ProtocolError::Fault(FaultReason::RetireFrontierRegressed));
                }
                self.retired_through = Some(present_seq);
                Ack::Retired {
                    decision_ordinal: ordinal,
                    present_seq,
                }
            }
            Decision::SealGeneration {
                final_session_frame,
                expected_select_count,
                expected_distinct_depth_count,
            } => {
                if !self.armed
                    && (final_session_frame.is_some()
                        || expected_select_count != 0
                        || expected_distinct_depth_count != 0)
                {
                    return Err(ProtocolError::Fault(FaultReason::FeedbackBeforeArm));
                }
                if self.sealed
                    || final_session_frame != self.last_session_frame
                    || expected_select_count != self.select_count
                    || expected_distinct_depth_count != self.distinct_depth_count
                {
                    return Err(ProtocolError::Fault(FaultReason::SealCountMismatch));
                }
                self.sealed = true;
                Ack::GenerationSealed {
                    decision_ordinal: ordinal,
                    select_count: self.select_count,
                    distinct_depth_count: self.distinct_depth_count,
                }
            }
        };
        self.next_ordinal = self.next_ordinal.saturating_add(1);
        Ok(ack)
    }

    fn validate_envelope(&self, message: &Envelope<Decision>) -> Result<(), ProtocolError> {
        if message.schema != self.cadence.schema() {
            return Err(ProtocolError::SchemaMismatch);
        }
        if message.stream.identity != self.stream.identity {
            return Err(ProtocolError::StreamMismatch("identity"));
        }
        if message.stream.generation != self.stream.generation {
            return Err(ProtocolError::StreamMismatch("generation"));
        }
        if message.stream.resource_epoch != self.stream.resource_epoch {
            return Err(ProtocolError::StreamMismatch("resource_epoch"));
        }
        if message.ordinal != self.next_ordinal {
            return Err(ProtocolError::OrdinalGap {
                expected: self.next_ordinal,
                actual: message.ordinal,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod timestamp_tests {
    use super::*;

    /// Windows 10/11 reports 10 MHz, so one tick is 100ns.
    const QPC_HZ: u64 = 10_000_000;

    /// Verbatim from the Fallout 4 / RTX 5060 Ti run that stopped with
    /// `feedback_timestamp_invalid` on 2026-08-12. The compositor clock leads
    /// `callback_start` by 10.674 ms and still leads `process` by 105 us after
    /// the CFR window (8.88 ms) and the queue hop (1.68 ms) have elapsed.
    const FIELD_SAMPLE: SelectTimestamps = SelectTimestamps {
        source: 2_405_971_127_436,
        callback_start: 2_405_971_020_694,
        callback_ready: 2_405_971_020_766,
        decision: 2_405_971_109_578,
        process: 2_405_971_126_386,
    };

    #[test]
    fn field_sample_that_stopped_recording_is_now_classified_not_rejected() {
        let latency = FIELD_SAMPLE.classify(QPC_HZ).unwrap();
        assert_eq!(latency.wgc_delivery_ticks, None);
        assert_eq!(latency.source_lead_callback_ready_ticks, Some(106_670));
        assert_eq!(latency.callback_to_decision_ticks, 88_812);
        // The lead survives all the way past the probe read, which is exactly
        // the case the old `source <= decision` rule stopped the run over.
        assert_eq!(latency.source_to_probe_ticks, None);
        assert_eq!(latency.source_lead_probe_ticks, Some(1_050));
        assert_eq!(latency.decision_to_probe_ticks, 16_808);
    }

    #[test]
    fn a_lead_that_outruns_only_our_own_scheduling_is_not_a_fault() {
        // Same clock lead, but the CFR window happened to be long enough that
        // `source` lands before `process`. Old rule: still rejected, because
        // `source` was past `decision`. It is the same clock either way.
        let slower_cfr = SelectTimestamps {
            process: FIELD_SAMPLE.process + 50_000,
            ..FIELD_SAMPLE
        };
        let latency = slower_cfr.classify(QPC_HZ).unwrap();
        assert_eq!(latency.source_lead_probe_ticks, None);
        assert_eq!(latency.source_to_probe_ticks, Some(48_950));
    }

    #[test]
    fn a_lead_past_the_ceiling_still_stops_the_run() {
        let ceiling_ticks = MAX_WGC_SOURCE_LEAD_US * QPC_HZ as i64 / 1_000_000;
        let at_ceiling = SelectTimestamps {
            source: FIELD_SAMPLE.callback_start + ceiling_ticks,
            process: FIELD_SAMPLE.callback_start + ceiling_ticks,
            ..FIELD_SAMPLE
        };
        assert!(at_ceiling.classify(QPC_HZ).is_ok());
        let over_ceiling = SelectTimestamps {
            source: at_ceiling.source + 1,
            ..at_ceiling
        };
        let fault = over_ceiling.classify(QPC_HZ).unwrap_err();
        assert_eq!(fault.reason, FaultReason::FeedbackTimestampInvalid);
        assert!(fault.detail.contains("over the"), "{}", fault.detail);
    }

    #[test]
    fn our_own_readings_stay_strictly_ordered() {
        let ordered = SelectTimestamps {
            source: 1_000,
            callback_start: 1_000,
            callback_ready: 1_010,
            decision: 1_020,
            process: 1_030,
        };
        assert!(ordered.classify(QPC_HZ).is_ok());
        for inverted in [
            SelectTimestamps {
                callback_ready: ordered.callback_start - 1,
                ..ordered
            },
            SelectTimestamps {
                decision: ordered.callback_ready - 1,
                ..ordered
            },
            SelectTimestamps {
                process: ordered.decision - 1,
                ..ordered
            },
        ] {
            let fault = inverted.classify(QPC_HZ).unwrap_err();
            assert_eq!(fault.reason, FaultReason::FeedbackTimestampInvalid);
            assert!(fault.detail.contains("out of order"), "{inverted:?}");
        }
        for non_positive in [
            SelectTimestamps {
                source: 0,
                ..ordered
            },
            SelectTimestamps {
                source: -1,
                ..ordered
            },
            SelectTimestamps {
                callback_start: 0,
                ..ordered
            },
        ] {
            assert_eq!(
                non_positive.classify(QPC_HZ).unwrap_err().reason,
                FaultReason::FeedbackTimestampInvalid
            );
        }
    }

    #[test]
    fn every_sample_lands_in_exactly_one_side_of_each_pair() {
        // An equal pair is the case a `try_into().ok()` split would put in both
        // distributions at once.
        for offset in -2..=2 {
            let sample = SelectTimestamps {
                source: (1_000 + offset).max(1),
                callback_start: 1_000,
                callback_ready: 1_000,
                decision: 1_000,
                process: 1_000,
            };
            let latency = sample.classify(QPC_HZ).unwrap();
            assert_eq!(
                latency.wgc_delivery_ticks.is_some(),
                latency.source_lead_callback_ready_ticks.is_none(),
                "offset {offset}"
            );
            assert_eq!(
                latency.source_to_probe_ticks.is_some(),
                latency.source_lead_probe_ticks.is_none(),
                "offset {offset}"
            );
        }
    }

    #[test]
    fn a_leading_source_never_underflows_into_a_huge_delay() {
        let latency = FIELD_SAMPLE.classify(QPC_HZ).unwrap();
        // `(process - source) as u64` wrapped to ~1.8e19 ticks here.
        assert_eq!(latency.source_to_probe_ticks, None);
        for value in [
            latency.wgc_delivery_ticks,
            latency.source_lead_callback_ready_ticks,
            latency.source_to_probe_ticks,
            latency.source_lead_probe_ticks,
        ]
        .into_iter()
        .flatten()
        .chain([
            latency.callback_to_decision_ticks,
            latency.decision_to_probe_ticks,
        ]) {
            assert!(value < u64::from(u32::MAX), "wrapped tick count {value}");
        }
    }

    #[test]
    fn a_zero_qpc_frequency_faults_instead_of_dividing_by_zero() {
        assert_eq!(
            FIELD_SAMPLE.classify(0).unwrap_err().reason,
            FaultReason::FeedbackTimestampInvalid
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream(generation: u64) -> StreamEpoch {
        StreamEpoch {
            identity: WireIdentity::new("s0001", "r0001").unwrap(),
            generation,
            resource_epoch: 1,
        }
    }

    fn message(stream: &StreamEpoch, ordinal: u64, payload: Decision) -> Envelope<Decision> {
        Envelope {
            schema: FEEDBACK_SCHEMA_V3,
            stream: stream.clone(),
            ordinal,
            payload,
        }
    }

    fn armed_state(stream: &StreamEpoch, first_candidate_present_seq: u64) -> FeedbackState {
        let mut state = FeedbackState::new(stream.clone());
        let arm = message(stream, 1, Decision::ArmGeneration);
        state.stage_arm(&arm).unwrap();
        assert_eq!(
            state.apply_arm(arm, first_candidate_present_seq).unwrap(),
            Ack::GenerationArmed {
                decision_ordinal: 1,
                first_candidate_present_seq,
            }
        );
        state
    }

    #[test]
    fn arm_ack_requires_actual_boundary_and_is_single_shot() {
        let stream = stream(8);
        let mut state = FeedbackState::new(stream.clone());
        let arm = message(&stream, 1, Decision::ArmGeneration);
        state.stage_arm(&arm).unwrap();
        assert_eq!(
            state.apply(arm.clone()),
            Err(ProtocolError::Fault(FaultReason::ArmBoundaryMissing))
        );
        state.apply_arm(arm, 4_975).unwrap();
        assert_eq!(
            state.apply_arm(message(&stream, 2, Decision::ArmGeneration), 4_976),
            Err(ProtocolError::Fault(FaultReason::ArmRepeated))
        );
    }

    #[test]
    fn pre_arm_feedback_fails_closed_but_zero_select_seal_is_legal() {
        let stream = stream(8);
        let mut select = FeedbackState::new(stream.clone());
        assert_eq!(
            select.apply(message(
                &stream,
                1,
                Decision::RetireThrough { present_seq: 1 },
            )),
            Err(ProtocolError::Fault(FaultReason::FeedbackBeforeArm))
        );

        let mut empty = FeedbackState::new(stream.clone());
        empty
            .apply(message(
                &stream,
                1,
                Decision::SealGeneration {
                    final_session_frame: None,
                    expected_select_count: 0,
                    expected_distinct_depth_count: 0,
                },
            ))
            .unwrap();
        assert_eq!(
            empty.stage_arm(&message(&stream, 2, Decision::ArmGeneration)),
            Err(ProtocolError::Fault(FaultReason::ArmAfterSeal))
        );
    }

    #[test]
    fn default_ring_budget_is_exactly_337_point_5_mib() {
        let budget = RingBudget {
            capacity: DEFAULT_RING_CAPACITY,
            max_feedback_lag_presents: DEFAULT_MAX_FEEDBACK_LAG_PRESENTS,
            guard_slots: DEFAULT_RING_GUARD_SLOTS,
            width: 1280,
            height: 720,
            bytes_per_pixel: 4,
            vram_limit_bytes: 400 * 1024 * 1024,
        };
        assert_eq!(budget.validate().unwrap(), 337 * 1024 * 1024 + 512 * 1024);
    }

    #[test]
    fn vram_and_capacity_arithmetic_fail_closed() {
        let mut budget = RingBudget {
            capacity: 96,
            max_feedback_lag_presents: 88,
            guard_slots: 7,
            width: 1280,
            height: 720,
            bytes_per_pixel: 4,
            vram_limit_bytes: u64::MAX,
        };
        assert!(budget.validate().is_err());
        budget.guard_slots = 8;
        budget.vram_limit_bytes = 1;
        assert!(budget.validate().is_err());
    }

    #[test]
    fn each_spsc_direction_fails_closed_when_full_without_overwrite() {
        let mut decisions = SpscModel::new(1).unwrap();
        decisions.push(1, FaultReason::FeedbackQueueFull).unwrap();
        assert_eq!(
            decisions.push(2, FaultReason::FeedbackQueueFull),
            Err(ProtocolError::Fault(FaultReason::FeedbackQueueFull))
        );
        assert_eq!(decisions.pop(), Some(1));

        let mut acks = SpscModel::new(1).unwrap();
        acks.push(3, FaultReason::AckQueueFull).unwrap();
        assert_eq!(
            acks.push(4, FaultReason::AckQueueFull),
            Err(ProtocolError::Fault(FaultReason::AckQueueFull))
        );
        assert_eq!(acks.pop(), Some(3));
    }

    #[test]
    fn duplicate_aliases_the_previous_identity_and_does_not_resubmit_depth() {
        let stream = stream(9);
        let mut state = armed_state(&stream, 500);
        let first = state
            .apply(message(
                &stream,
                2,
                Decision::Select {
                    depth_required: true,
                    session_frame: 100,
                    present_seq: 500,
                    duplicate: false,
                    source_superseded: 2,
                    source_qpc: 1_000,
                    callback_start_qpc: 1_010,
                    callback_ready_qpc: 1_015,
                    decision_qpc: 1_020,
                },
            ))
            .unwrap();
        assert!(matches!(
            first,
            Ack::SelectAccepted {
                depth_already_selected: false,
                ..
            }
        ));
        let duplicate = state
            .apply(message(
                &stream,
                3,
                Decision::Select {
                    depth_required: true,
                    session_frame: 101,
                    present_seq: 500,
                    duplicate: true,
                    source_superseded: 0,
                    source_qpc: 1_000,
                    callback_start_qpc: 1_010,
                    callback_ready_qpc: 1_015,
                    decision_qpc: 1_030,
                },
            ))
            .unwrap();
        assert!(matches!(
            duplicate,
            Ack::SelectAccepted {
                depth_already_selected: true,
                ..
            }
        ));
        assert!(
            state
                .apply(message(
                    &stream,
                    4,
                    Decision::SealGeneration {
                        final_session_frame: Some(101),
                        expected_select_count: 2,
                        expected_distinct_depth_count: 1,
                    },
                ))
                .is_ok()
        );
    }

    #[test]
    fn a_new_wgc_observation_may_repeat_the_same_present_identity() {
        let stream = stream(12);
        let mut state = armed_state(&stream, 700);
        for (ordinal, session_frame) in [(2, 200), (3, 201)] {
            let ack = state
                .apply(message(
                    &stream,
                    ordinal,
                    Decision::Select {
                        depth_required: true,
                        session_frame,
                        present_seq: 700,
                        duplicate: false,
                        source_superseded: 0,
                        source_qpc: 2_000 + ordinal as i64,
                        callback_start_qpc: 2_010 + ordinal as i64,
                        callback_ready_qpc: 2_015 + ordinal as i64,
                        decision_qpc: 2_020 + ordinal as i64,
                    },
                ))
                .unwrap();
            assert_eq!(
                matches!(
                    ack,
                    Ack::SelectAccepted {
                        depth_already_selected: true,
                        ..
                    }
                ),
                ordinal == 3
            );
        }
        state
            .apply(message(
                &stream,
                4,
                Decision::SealGeneration {
                    final_session_frame: Some(201),
                    expected_select_count: 2,
                    expected_distinct_depth_count: 1,
                },
            ))
            .unwrap();
    }

    #[test]
    fn conflicts_late_selects_and_cross_generation_messages_are_rejected() {
        let stream = stream(10);
        let mut state = armed_state(&stream, 99);
        state
            .apply(message(
                &stream,
                2,
                Decision::RetireThrough { present_seq: 99 },
            ))
            .unwrap();
        assert_eq!(
            state.apply(message(
                &stream,
                3,
                Decision::Select {
                    depth_required: true,
                    session_frame: 0,
                    present_seq: 99,
                    duplicate: false,
                    source_superseded: 0,
                    source_qpc: 1_000,
                    callback_start_qpc: 1_010,
                    callback_ready_qpc: 1_015,
                    decision_qpc: 1_020,
                },
            )),
            Err(ProtocolError::Fault(FaultReason::SelectAfterRetire))
        );

        let mut wrong_generation = stream.clone();
        wrong_generation.generation += 1;
        assert_eq!(
            state.apply(message(
                &wrong_generation,
                3,
                Decision::RetireThrough { present_seq: 100 },
            )),
            Err(ProtocolError::StreamMismatch("generation"))
        );
    }

    #[test]
    fn ordinal_gap_and_seal_count_mismatch_do_not_advance_state() {
        let stream = stream(11);
        let mut state = FeedbackState::new(stream.clone());
        assert_eq!(
            state.apply(message(
                &stream,
                2,
                Decision::RetireThrough { present_seq: 1 },
            )),
            Err(ProtocolError::OrdinalGap {
                expected: 1,
                actual: 2
            })
        );
        assert_eq!(
            state.apply(message(
                &stream,
                1,
                Decision::SealGeneration {
                    final_session_frame: None,
                    expected_select_count: 1,
                    expected_distinct_depth_count: 0,
                },
            )),
            Err(ProtocolError::Fault(FaultReason::FeedbackBeforeArm))
        );
        assert!(
            state
                .apply(message(
                    &stream,
                    1,
                    Decision::SealGeneration {
                        final_session_frame: None,
                        expected_select_count: 0,
                        expected_distinct_depth_count: 0,
                    },
                ))
                .is_ok()
        );
    }

    #[test]
    fn previously_selected_identity_cannot_reappear_after_progress() {
        let stream = stream(12);
        let mut state = armed_state(&stream, 100);
        for (ordinal, session_frame, present_seq) in [(2, 0, 100), (3, 1, 200)] {
            state
                .apply(message(
                    &stream,
                    ordinal,
                    Decision::Select {
                        depth_required: true,
                        session_frame,
                        present_seq,
                        duplicate: false,
                        source_superseded: 0,
                        source_qpc: 1_000 + present_seq as i64,
                        callback_start_qpc: 1_010 + present_seq as i64,
                        callback_ready_qpc: 1_015 + present_seq as i64,
                        decision_qpc: 1_020 + present_seq as i64,
                    },
                ))
                .unwrap();
        }
        assert_eq!(
            state.apply(message(
                &stream,
                4,
                Decision::Select {
                    depth_required: true,
                    session_frame: 2,
                    present_seq: 100,
                    duplicate: false,
                    source_superseded: 0,
                    source_qpc: 1_100,
                    callback_start_qpc: 1_110,
                    callback_ready_qpc: 1_115,
                    decision_qpc: 1_120,
                },
            )),
            Err(ProtocolError::Fault(FaultReason::PresentSeqRegressed))
        );
    }
}

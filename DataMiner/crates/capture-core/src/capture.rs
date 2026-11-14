use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread;

use anyhow::{Context, ensure};
use crossbeam_channel::{Receiver, Sender, TryRecvError, TrySendError, bounded, unbounded};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BOX, D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_QUERY_DESC,
    D3D11_QUERY_EVENT, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING, ID3D11Device,
    ID3D11DeviceContext, ID3D11Query, ID3D11Resource, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::core::BOOL;
use windows_capture_wgc::{
    CaptureContext, Frame, FrameControl, GraphicsCaptureHandler, WGC_FRAME_POOL_BUFFER_COUNT,
};

use crate::gpu::create_bgra_copy_texture;
use crate::qpc::QpcClock;

/// WGC is requested at up to eight source observations per CFR output frame so
/// compositor and output-frame phases do not create avoidable duplicates.
pub const WGC_SOURCE_OBSERVATIONS_PER_CFR_FRAME: usize = 8;
/// How many output-frame periods the pipeline may spend flushing the hardware
/// encoder at a segment boundary.
///
/// One period was not enough. A segment boundary has to drain *every* frame the
/// encoder still holds, and the driver keeps more than one in flight: DataMiner
/// asks for `MF_LOW_LATENCY`, and the NVIDIA MFT is documented (docs/rca —
/// encoder control normalization, capture profile `accepted_encoder_relaxations`)
/// to normalise that request away. With a multi-frame pipeline the drain costs
/// roughly `depth × per-frame encode time`, which exceeds a single 33 ms period
/// as soon as frames get expensive — observed as
/// `hardware encoder did not emit the segment tail before the next boundary` on
/// Age of Empires IV while capture itself stayed clean (0 queue drops), and as
/// NV12 backpressure on WUCHANG, while the lighter Wolfenstein II frames passed
/// on the same machine (issue #266).
///
/// The first 60fps real-game runs then showed that two periods shrink the
/// existing 30fps allowance from roughly 66ms to 33ms: one GPU copy needed
/// 38.090ms, and a later segment boundary still had one encoded frame pending
/// at 33ms while the capture queue had dropped nothing. Four 60fps periods
/// restore the proven absolute allowance without hiding real data loss.
///
/// This is the budget the *queue* is sized to cover, so it may not be raised on
/// its own — see `CAPTURE_QUEUE_CFR_WINDOWS`.
pub const ENCODER_DRAIN_CFR_FRAMES: usize = 4;
/// Keep one complete source-observation window available while the capture
/// pipeline is allowed to spend up to `ENCODER_DRAIN_CFR_FRAMES` output frames
/// draining an encoder tail at a segment boundary. The remaining windows absorb
/// observations arriving during that bounded wait without weakening the existing
/// fail-closed overflow path.
///
/// The `1 +` is the invariant: at the requested eight observations per output
/// frame, a drain of N frame periods enqueues N windows of observations, so the
/// queue must hold those plus the window the selector has not consumed yet.
/// Raising the drain budget without this coupling would silently trade a
/// fail-closed encoder timeout for a fail-closed *queue overflow* — the same
/// stop, blamed on the wrong subsystem.
pub const CAPTURE_QUEUE_CFR_WINDOWS: usize = 1 + ENCODER_DRAIN_CFR_FRAMES;
/// Steady-state observations retained for the active CFR window plus the full
/// bounded encoder wait. This arithmetic intentionally excludes delivery
/// burst slack so the encoder budget and WinRT scheduling allowance remain two
/// independently reviewable decisions.
pub const CAPTURE_QUEUE_STEADY_STATE_CAPACITY: usize =
    WGC_SOURCE_OBSERVATIONS_PER_CFR_FRAME * CAPTURE_QUEUE_CFR_WINDOWS;
/// `MinUpdateInterval` controls average WGC cadence, not callback burst size.
/// A free-threaded frame pool may make every retained frame observable after a
/// scheduling delay. Once those retained frames are queued, one more callback
/// can already be executing outside the channel: `Sender::len()` does not count
/// that in-flight observation. Reserve the complete pool plus that callback.
///
/// This is capacity only: it does not extend any encoder, GPU-copy, or selector
/// deadline and the next observation still fails closed.
pub const WGC_CALLBACK_IN_FLIGHT_GUARD: usize = 1;
pub const CAPTURE_QUEUE_BURST_GUARD: usize =
    WGC_FRAME_POOL_BUFFER_COUNT as usize + WGC_CALLBACK_IN_FLIGHT_GUARD;
pub const CAPTURE_QUEUE_CAPACITY: usize =
    CAPTURE_QUEUE_STEADY_STATE_CAPACITY + CAPTURE_QUEUE_BURST_GUARD;
/// Wall-clock liveness watchdog for a D3D11 copy-completion query.
///
/// Source loss is detected independently by the bounded capture queue. A short
/// GPU scheduling stall which still fits that queue is therefore recoverable
/// and must not be mislabeled as data loss. Two seconds is only the final
/// liveness bound for a query which never completes; the first queue overflow
/// still stops the run through the existing fail-closed path.
pub const GPU_COPY_LIVENESS_WATCHDOG_MS: i64 = 2_000;
/// Bounded wait for one WGC GPU-copy completion query.
///
/// `ENCODER_DRAIN_CFR_FRAMES` remains the minimum because the source queue was
/// dimensioned to cover that much pipeline backpressure. It is not a valid
/// wall-clock failure threshold, however: the real source cadence may be below
/// the requested 8x observation ceiling, leaving substantially more queue
/// headroom. The queue's overflow signal remains the authority for source loss;
/// this deadline only prevents a permanently incomplete D3D query from hanging
/// the recorder forever.
#[must_use]
pub fn gpu_copy_wait_limit_qpc(fps_num: u32, fps_den: u32, frequency: i64) -> i64 {
    let frequency = frequency.max(1);
    let frame_period = ((i128::from(frequency) * i128::from(fps_den.max(1)))
        / i128::from(fps_num.max(1)))
    .max(1) as i64;
    let queue_coupled_minimum = frame_period
        .saturating_mul(i64::try_from(ENCODER_DRAIN_CFR_FRAMES).unwrap_or(1))
        .max(frame_period);
    let liveness_watchdog = (i128::from(frequency) * i128::from(GPU_COPY_LIVENESS_WATCHDOG_MS)
        / 1_000)
        .clamp(1, i128::from(i64::MAX)) as i64;
    queue_coupled_minimum.max(liveness_watchdog)
}

/// Wait for one WGC GPU-copy event query without losing `S_FALSE` semantics.
///
/// The generated `windows` binding converts both `S_OK` and `S_FALSE` to
/// `Ok(())`, so the HRESULT alone cannot distinguish a completed query from an
/// incomplete one. `D3D11_QUERY_EVENT` also returns a `BOOL`; use that value as
/// the completion signal and reserve the `Result` for actual API failures.
pub fn wait_for_gpu_copy_completion(
    context: &ID3D11DeviceContext,
    query: &ID3D11Query,
    clock: &QpcClock,
    metrics: &CaptureMetrics,
    callback_start_qpc: i64,
    limit_qpc: i64,
) -> anyhow::Result<f64> {
    let completed_qpc = wait_for_gpu_copy_completion_with(
        || {
            let mut completed = BOOL(0);
            unsafe {
                context.GetData(
                    query,
                    Some((&raw mut completed).cast::<c_void>()),
                    std::mem::size_of::<BOOL>() as u32,
                    0,
                )
            }
            .context("D3D11 GPU-copy completion query failed")?;
            let drops = metrics.drops.load(Ordering::Relaxed);
            ensure!(
                drops == 0,
                "WGC callback queue dropped {drops} frame(s) while waiting for GPU-copy completion \
                 (capacity {CAPTURE_QUEUE_CAPACITY}, max observed depth {})",
                metrics.max_queue_depth.load(Ordering::Relaxed)
            );
            Ok(completed.as_bool())
        },
        || clock.now(),
        callback_start_qpc,
        limit_qpc,
        clock.frequency(),
    )?;

    Ok(clock.elapsed_micros(callback_start_qpc, completed_qpc))
}

fn wait_for_gpu_copy_completion_with<P, N>(
    mut poll_complete: P,
    mut now_qpc: N,
    callback_start_qpc: i64,
    limit_qpc: i64,
    frequency: i64,
) -> anyhow::Result<i64>
where
    P: FnMut() -> anyhow::Result<bool>,
    N: FnMut() -> anyhow::Result<i64>,
{
    ensure!(limit_qpc > 0, "GPU-copy wait budget must be positive");
    ensure!(frequency > 0, "QPC frequency must be positive");

    loop {
        let completed = poll_complete()?;
        let now = now_qpc()?;
        if completed {
            return Ok(now);
        }
        let waited = now - callback_start_qpc;
        ensure!(
            waited < limit_qpc,
            "GPU-copy completion exceeded its bounded wait budget ({:.3}ms of {:.3}ms); recording stopped",
            waited as f64 * 1_000.0 / frequency as f64,
            limit_qpc as f64 * 1_000.0 / frequency as f64,
        );
        thread::yield_now();
    }
}
/// One transient compositor timestamp regression can be discarded safely; a
/// full CFR window of them means the source clock is no longer trustworthy.
const MAX_CONSECUTIVE_NON_MONOTONIC_SOURCE_FRAMES: usize = WGC_SOURCE_OBSERVATIONS_PER_CFR_FRAME;
/// In addition to queued textures, the pacer may hold a selected source and the
/// first source for the next CFR window while the callback owns another slot.
pub const BGRA_RING_SIZE: usize = CAPTURE_QUEUE_CAPACITY + 3;

/// Mathematical uniqueness window for online low-16 expansion. This is kept
/// separate from the 88-Present selected-feedback/ring admission budget: WGC
/// startup may legitimately take longer than ring feedback while still being
/// unambiguous. A gap of half a low-16 period is never accepted.
pub const DEFAULT_PRESENT_IDENTITY_UNWRAP_BUDGET: u64 = present_identity::MAX_FORWARD_ACCEPT_BUDGET;
pub const DEFAULT_WATERMARK_DECODE_MARGIN: u8 = 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PresentIdentitySession {
    pub generation: u64,
    pub resource_epoch: u64,
    pub first_present_seq: u64,
    pub unwrap_accept_budget: u64,
    /// Backbuffer geometry in which the probe writes the watermark. WGC may
    /// expose a differently sized client surface after borderless/compositor
    /// scaling, so this cannot be inferred from the encoder output profile.
    pub watermark_source_width: u32,
    pub watermark_source_height: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WatermarkCaptureConfig {
    pub roi_left: u32,
    pub roi_top: u32,
    pub roi_width: u32,
    pub roi_height: u32,
    pub decode_margin: u8,
}

#[derive(Clone, Copy, Debug)]
pub struct CapturedFrame {
    pub slot: usize,
    pub source_seq: u64,
    pub source_qpc: i64,
    pub callback_start_qpc: i64,
    pub callback_ready_qpc: i64,
    pub queue_depth: usize,
    /// Full Present identity decoded after the asynchronous ROI copy completes.
    /// It is absent only when the run did not enable the depth identity plane.
    pub present_seq: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SelectedSourceIdentity {
    pub source_seq: u64,
    pub source_qpc: i64,
    pub present_seq: Option<u64>,
}

impl From<&CapturedFrame> for SelectedSourceIdentity {
    fn from(frame: &CapturedFrame) -> Self {
        Self {
            source_seq: frame.source_seq,
            source_qpc: frame.source_qpc,
            present_seq: frame.present_seq,
        }
    }
}

#[derive(Debug)]
pub enum CfrObservation {
    Candidate { replaced: Option<CapturedFrame> },
    Future(CapturedFrame),
}

pub(crate) enum CfrIngressPoll {
    Frame(CapturedFrame),
    DeadlineReached,
    Wait,
}

/// The seal deadline limits waiting for new arrivals, not consuming observations
/// already retained by the bounded ingress queue. Otherwise a delayed consumer
/// synthesizes duplicates while the next slot supersedes those same queued frames.
/// Source timestamps still decide slot membership in `CfrSlotSelector`.
pub(crate) fn poll_cfr_ingress(
    frames: &Receiver<CapturedFrame>,
    now_qpc: i64,
    seal_qpc: i64,
) -> Result<CfrIngressPoll, TryRecvError> {
    match frames.try_recv() {
        Ok(frame) => Ok(CfrIngressPoll::Frame(frame)),
        Err(TryRecvError::Disconnected) => Err(TryRecvError::Disconnected),
        Err(TryRecvError::Empty) if now_qpc >= seal_qpc => Ok(CfrIngressPoll::DeadlineReached),
        Err(TryRecvError::Empty) => Ok(CfrIngressPoll::Wait),
    }
}

#[derive(Debug)]
pub struct CfrSlotDecision {
    pub selected: Option<CapturedFrame>,
    pub identity: SelectedSourceIdentity,
    pub duplicate: bool,
    pub source_superseded: u32,
}

#[derive(Default, Debug)]
pub struct CfrSlotSelector {
    selected: Option<CapturedFrame>,
    source_superseded: u32,
}

impl CfrSlotSelector {
    pub(crate) fn has_candidate(&self) -> bool {
        self.selected.is_some()
    }

    pub fn take_selected(&mut self) -> Option<CapturedFrame> {
        self.selected.take()
    }

    #[must_use]
    pub fn observe(&mut self, frame: CapturedFrame, slot_end_qpc: i64) -> CfrObservation {
        if frame.source_qpc >= slot_end_qpc {
            return CfrObservation::Future(frame);
        }
        let replaced = self.selected.replace(frame);
        if replaced.is_some() {
            self.source_superseded = self.source_superseded.saturating_add(1);
        }
        CfrObservation::Candidate { replaced }
    }

    pub fn seal(
        self,
        previous: Option<SelectedSourceIdentity>,
    ) -> Result<CfrSlotDecision, CfrSelectionError> {
        let identity = self
            .selected
            .as_ref()
            .map(SelectedSourceIdentity::from)
            .or(previous)
            .ok_or(CfrSelectionError::FirstSlotHasNoSource)?;
        Ok(CfrSlotDecision {
            duplicate: self.selected.is_none(),
            selected: self.selected,
            identity,
            source_superseded: self.source_superseded,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum CfrSelectionError {
    #[error("first CFR slot had no source frame")]
    FirstSlotHasNoSource,
}

#[derive(Default)]
pub struct CaptureMetrics {
    pub callbacks: AtomicU64,
    pub drops: AtomicU64,
    pub max_queue_depth: AtomicUsize,
}

pub struct CaptureIngress {
    pub frames: Receiver<CapturedFrame>,
    pub recycle: Sender<usize>,
    pub textures: Arc<OnceLock<Vec<ID3D11Texture2D>>>,
    pub copy_queries: Arc<OnceLock<Vec<ID3D11Query>>>,
    pub watermark_textures: Arc<OnceLock<Vec<ID3D11Texture2D>>>,
    pub metrics: Arc<CaptureMetrics>,
    flags: Option<IngressFlags>,
}

impl CaptureIngress {
    #[must_use]
    pub fn new(clock: QpcClock, watermark: Option<WatermarkCaptureConfig>) -> Self {
        Self::with_cadence_trace(clock, watermark, Default::default())
    }

    pub(crate) fn with_cadence_trace(
        clock: QpcClock,
        watermark: Option<WatermarkCaptureConfig>,
        trace: crate::cadence_trace::CadenceTrace,
    ) -> Self {
        let (frame_tx, frames) = bounded(CAPTURE_QUEUE_CAPACITY);
        let (recycle, recycle_rx) = unbounded();
        let textures = Arc::new(OnceLock::new());
        let copy_queries = Arc::new(OnceLock::new());
        let watermark_textures = Arc::new(OnceLock::new());
        let metrics = Arc::new(CaptureMetrics::default());
        let flags = IngressFlags {
            clock,
            frame_tx,
            recycle_rx,
            textures: Arc::clone(&textures),
            copy_queries: Arc::clone(&copy_queries),
            watermark_textures: Arc::clone(&watermark_textures),
            watermark,
            metrics: Arc::clone(&metrics),
            trace,
        };
        Self {
            frames,
            recycle,
            textures,
            copy_queries,
            watermark_textures,
            metrics,
            flags: Some(flags),
        }
    }

    pub fn take_flags(&mut self) -> anyhow::Result<IngressFlags> {
        self.flags
            .take()
            .ok_or_else(|| anyhow::anyhow!("capture ingress flags were already consumed"))
    }
}

pub struct IngressFlags {
    clock: QpcClock,
    frame_tx: Sender<CapturedFrame>,
    recycle_rx: Receiver<usize>,
    textures: Arc<OnceLock<Vec<ID3D11Texture2D>>>,
    copy_queries: Arc<OnceLock<Vec<ID3D11Query>>>,
    watermark_textures: Arc<OnceLock<Vec<ID3D11Texture2D>>>,
    watermark: Option<WatermarkCaptureConfig>,
    metrics: Arc<CaptureMetrics>,
    trace: crate::cadence_trace::CadenceTrace,
}

unsafe impl Send for IngressFlags {}

#[derive(Debug, thiserror::Error)]
pub enum IngressError {
    #[error("QPC read failed: {0}")]
    Qpc(#[source] anyhow::Error),
    #[error("WGC timestamp failed: {0}")]
    Timestamp(#[from] windows::core::Error),
    #[error(
        "capture queue exhausted at depth {queue_depth}/{queue_capacity} with {ring_size} BGRA slots; recording was stopped rather than degraded"
    )]
    QueueExhausted {
        queue_depth: usize,
        queue_capacity: usize,
        ring_size: usize,
    },
    #[error("capture texture ring was initialized more than once")]
    RingAlreadyInitialized,
    #[error("failed to create GPU-copy ring: {0}")]
    Ring(#[source] anyhow::Error),
    #[error("watermark ROI is outside the WGC surface: {0}")]
    WatermarkRoi(String),
    #[error(
        "WGC source timestamp stayed non-monotonic for {streak} callbacks (previous QPC {previous_source_qpc}, current QPC {source_qpc})"
    )]
    NonMonotonicTimestamp {
        previous_source_qpc: i64,
        source_qpc: i64,
        streak: usize,
    },
}

pub struct GpuCopyHandler {
    clock: QpcClock,
    context: ID3D11DeviceContext,
    textures: Vec<ID3D11Texture2D>,
    copy_queries: Vec<ID3D11Query>,
    watermark_textures: Option<Vec<ID3D11Texture2D>>,
    watermark_box: Option<D3D11_BOX>,
    free: VecDeque<usize>,
    frame_tx: Sender<CapturedFrame>,
    recycle_rx: Receiver<usize>,
    metrics: Arc<CaptureMetrics>,
    trace: crate::cadence_trace::CadenceTrace,
    _trace_producer: Option<crate::cadence_trace::TraceProducer>,
    last_source_qpc: Option<i64>,
    non_monotonic_source_streak: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SourceTimestampDecision {
    Accept,
    Ignore { streak: usize },
    Stop { streak: usize },
}

fn classify_source_timestamp(
    previous_source_qpc: Option<i64>,
    source_qpc: i64,
    non_monotonic_streak: usize,
) -> SourceTimestampDecision {
    if previous_source_qpc.is_none_or(|previous| source_qpc > previous) {
        return SourceTimestampDecision::Accept;
    }
    let streak = non_monotonic_streak.saturating_add(1);
    if streak > MAX_CONSECUTIVE_NON_MONOTONIC_SOURCE_FRAMES {
        SourceTimestampDecision::Stop { streak }
    } else {
        SourceTimestampDecision::Ignore { streak }
    }
}

impl GraphicsCaptureHandler for GpuCopyHandler {
    type Flags = IngressFlags;
    type Error = IngressError;

    fn new(context: CaptureContext<Self::Flags>) -> Result<Self, Self::Error> {
        let watermark_box = context
            .flags
            .watermark
            .map(|config| {
                let right = config
                    .roi_left
                    .checked_add(config.roi_width)
                    .ok_or_else(|| IngressError::WatermarkRoi("ROI right overflowed".into()))?;
                let bottom = config
                    .roi_top
                    .checked_add(config.roi_height)
                    .ok_or_else(|| IngressError::WatermarkRoi("ROI bottom overflowed".into()))?;
                if config.roi_width < present_identity::watermark::TOTAL_BLOCKS
                    || config.roi_height == 0
                    || right > context.surface_width
                    || bottom > context.surface_height
                {
                    return Err(IngressError::WatermarkRoi(format!(
                        "({}, {})..({}, {}) with {}x{} ROI is invalid for {}x{}",
                        config.roi_left,
                        config.roi_top,
                        right,
                        bottom,
                        config.roi_width,
                        config.roi_height,
                        context.surface_width,
                        context.surface_height
                    )));
                }
                Ok(D3D11_BOX {
                    left: config.roi_left,
                    top: config.roi_top,
                    front: 0,
                    right,
                    bottom,
                    back: 1,
                })
            })
            .transpose()?;
        let mut textures = Vec::with_capacity(BGRA_RING_SIZE);
        let mut copy_queries = Vec::with_capacity(BGRA_RING_SIZE);
        let mut watermark_textures = context
            .flags
            .watermark
            .map(|_| Vec::with_capacity(BGRA_RING_SIZE));
        for _ in 0..BGRA_RING_SIZE {
            textures.push(
                create_bgra_copy_texture(
                    &context.device,
                    context.surface_width,
                    context.surface_height,
                )
                .map_err(IngressError::Ring)?,
            );
            let mut query = None;
            let query_desc = D3D11_QUERY_DESC {
                Query: D3D11_QUERY_EVENT,
                MiscFlags: 0,
            };
            unsafe {
                context
                    .device
                    .CreateQuery(&raw const query_desc, Some(&mut query))
            }
            .map_err(|error| IngressError::Ring(error.into()))?;
            copy_queries.push(query.ok_or_else(|| {
                IngressError::Ring(anyhow::anyhow!(
                    "D3D11 returned no GPU-copy completion query"
                ))
            })?);
            if let Some(textures) = watermark_textures.as_mut() {
                let config = context
                    .flags
                    .watermark
                    .expect("watermark texture ring requires watermark config");
                textures.push(
                    create_watermark_staging_texture(
                        &context.device,
                        config.roi_width,
                        config.roi_height,
                    )
                    .map_err(IngressError::Ring)?,
                );
            }
        }
        context
            .flags
            .textures
            .set(textures.clone())
            .map_err(|_| IngressError::RingAlreadyInitialized)?;
        if let Some(textures) = &watermark_textures {
            context
                .flags
                .watermark_textures
                .set(textures.clone())
                .map_err(|_| IngressError::RingAlreadyInitialized)?;
        }

        context
            .flags
            .copy_queries
            .set(copy_queries.clone())
            .map_err(|_| IngressError::RingAlreadyInitialized)?;

        let trace_producer = context.flags.trace.producer();
        Ok(Self {
            clock: context.flags.clock,
            context: context.device_context,
            textures,
            copy_queries,
            watermark_textures,
            watermark_box,
            free: (0..BGRA_RING_SIZE).collect(),
            frame_tx: context.flags.frame_tx,
            recycle_rx: context.flags.recycle_rx,
            metrics: context.flags.metrics,
            trace: context.flags.trace,
            _trace_producer: trace_producer,
            last_source_qpc: None,
            non_monotonic_source_streak: 0,
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame<'_>,
        control: FrameControl,
    ) -> Result<(), Self::Error> {
        let callback_start_qpc = self.clock.now().map_err(IngressError::Qpc)?;
        let source_qpc = self
            .clock
            .relative_100ns_to_qpc(frame.timestamp()?.Duration);
        let source_seq = self.metrics.callbacks.fetch_add(1, Ordering::Relaxed) + 1;
        use crate::cadence_trace::{Event, SourceStage};
        let identity = SelectedSourceIdentity {
            source_seq,
            source_qpc,
            present_seq: None,
        };
        self.trace.at(Some(callback_start_qpc), || {
            Event::source(
                SourceStage::CallbackStart,
                identity,
                None,
                self.frame_tx.len(),
            )
        });
        let previous_source_qpc = self.last_source_qpc;
        // SystemRelativeTime identifies when the compositor rendered a frame,
        // but WGC does not promise that delivery order is strictly increasing.
        // Reject a late observation before it can replace the canonical source
        // frame; keep the persistent-regression path fail-closed.
        match classify_source_timestamp(
            previous_source_qpc,
            source_qpc,
            self.non_monotonic_source_streak,
        ) {
            SourceTimestampDecision::Accept => {
                self.last_source_qpc = Some(source_qpc);
                self.non_monotonic_source_streak = 0;
            }
            SourceTimestampDecision::Ignore { streak } => {
                self.trace.emit(self.clock, || Event::TimestampRejected {
                    source_seq,
                    source_qpc,
                    previous_source_qpc,
                    streak: streak as u64,
                    fatal: false,
                });
                self.trace.emit(self.clock, || {
                    Event::source(
                        SourceStage::TimestampIgnored,
                        identity,
                        None,
                        self.frame_tx.len(),
                    )
                });
                self.non_monotonic_source_streak = streak;
                tracing::warn!(
                    source_seq,
                    source_qpc,
                    previous_source_qpc = previous_source_qpc.unwrap_or_default(),
                    streak,
                    "discarding a non-monotonic WGC source frame"
                );
                return Ok(());
            }
            SourceTimestampDecision::Stop { streak } => {
                self.trace.emit(self.clock, || Event::TimestampRejected {
                    source_seq,
                    source_qpc,
                    previous_source_qpc,
                    streak: streak as u64,
                    fatal: true,
                });
                self.trace.emit(self.clock, || {
                    Event::source(
                        SourceStage::TimestampFatal,
                        identity,
                        None,
                        self.frame_tx.len(),
                    )
                });
                control.request_stop();
                return Err(IngressError::NonMonotonicTimestamp {
                    previous_source_qpc: previous_source_qpc.unwrap_or_default(),
                    source_qpc,
                    streak,
                });
            }
        }
        while let Ok(slot) = self.recycle_rx.try_recv() {
            self.free.push_back(slot);
        }
        let Some(slot) = self.free.pop_front() else {
            self.trace.emit(self.clock, || {
                Event::source(
                    SourceStage::RingExhausted,
                    identity,
                    None,
                    self.frame_tx.len(),
                )
            });
            self.metrics.drops.fetch_add(1, Ordering::Relaxed);
            control.request_stop();
            return Err(IngressError::QueueExhausted {
                queue_depth: self.frame_tx.len(),
                queue_capacity: CAPTURE_QUEUE_CAPACITY,
                ring_size: BGRA_RING_SIZE,
            });
        };

        unsafe {
            self.context
                .CopyResource(&self.textures[slot], frame.as_raw_texture());
            if let (Some(textures), Some(copy_box)) = (&self.watermark_textures, self.watermark_box)
            {
                self.context.CopySubresourceRegion(
                    &textures[slot],
                    0,
                    0,
                    0,
                    0,
                    frame.as_raw_texture(),
                    0,
                    Some(&raw const copy_box),
                );
            }
            self.context.End(&self.copy_queries[slot]);
        }
        let callback_ready_qpc = self.clock.now().map_err(IngressError::Qpc)?;
        let queue_depth = self.frame_tx.len() + 1;
        self.metrics
            .max_queue_depth
            .fetch_max(queue_depth, Ordering::Relaxed);
        let message = CapturedFrame {
            slot,
            source_seq,
            source_qpc,
            callback_start_qpc,
            callback_ready_qpc,
            queue_depth,
            present_seq: None,
        };
        let send_before_qpc = self.trace.now(self.clock);
        let sent = self.frame_tx.try_send(message);
        let send_after_qpc = self.trace.now(self.clock);
        self.trace.at(send_after_qpc, || Event::Enqueue {
            source_seq,
            source_qpc,
            callback_start_qpc,
            copy_submitted_qpc: callback_ready_qpc,
            send_before_qpc,
            send_after_qpc,
            queued: sent.is_ok(),
            queue_depth_before: queue_depth - 1,
            queue_depth_after: self.frame_tx.len(),
        });
        match sent {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                self.free.push_front(slot);
                self.metrics.drops.fetch_add(1, Ordering::Relaxed);
                control.request_stop();
                Err(IngressError::QueueExhausted {
                    queue_depth: self.frame_tx.len(),
                    queue_capacity: CAPTURE_QUEUE_CAPACITY,
                    ring_size: BGRA_RING_SIZE,
                })
            }
        }
    }
}

pub fn textures_after_start(ingress: &CaptureIngress) -> anyhow::Result<&[ID3D11Texture2D]> {
    ingress
        .textures
        .get()
        .map(Vec::as_slice)
        .ok_or_else(|| anyhow::anyhow!("WGC handler did not initialize the GPU-copy ring"))
}

pub fn copy_queries_after_start(ingress: &CaptureIngress) -> anyhow::Result<&[ID3D11Query]> {
    ingress
        .copy_queries
        .get()
        .map(Vec::as_slice)
        .ok_or_else(|| {
            anyhow::anyhow!("WGC handler did not initialize GPU-copy completion queries")
        })
}

pub fn watermark_textures_after_start(
    ingress: &CaptureIngress,
) -> anyhow::Result<&[ID3D11Texture2D]> {
    ingress
        .watermark_textures
        .get()
        .map(Vec::as_slice)
        .ok_or_else(|| anyhow::anyhow!("WGC handler did not initialize the watermark ROI ring"))
}

fn create_watermark_staging_texture(
    device: &ID3D11Device,
    width: u32,
    height: u32,
) -> anyhow::Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };
    let mut texture = None;
    unsafe { device.CreateTexture2D(&raw const desc, None, Some(&mut texture)) }
        .context("failed to create watermark ROI staging texture")?;
    texture.context("D3D11 returned no watermark ROI staging texture")
}

pub fn decode_watermark_texture(
    context: &ID3D11DeviceContext,
    texture: &ID3D11Texture2D,
    margin: u8,
) -> anyhow::Result<u16> {
    let resource: ID3D11Resource = texture.clone().into();
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    unsafe { context.Map(&resource, 0, D3D11_MAP_READ, 0, Some(&mut mapped)) }
        .context("failed to map completed watermark ROI")?;
    let result = (|| {
        ensure!(
            !mapped.pData.is_null(),
            "watermark ROI Map returned null data"
        );
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&mut desc) };
        let row_pitch = mapped.RowPitch as usize;
        let byte_len = row_pitch
            .checked_mul(desc.Height as usize)
            .context("watermark ROI mapped length overflow")?;
        let bytes = unsafe { std::slice::from_raw_parts(mapped.pData.cast::<u8>(), byte_len) };
        let means = present_identity::watermark::scaled_block_means_from_bgra(
            bytes,
            row_pitch,
            desc.Width as usize,
            desc.Height as usize,
        )
        .map_err(anyhow::Error::new)?;
        // Preserve DecodeError as the anyhow source. The recorder may safely
        // reject a corrupt observation before it enters the CFR selector;
        // Map/layout failures still identify a fatal readback-plane failure.
        present_identity::watermark::decode(&means, margin).map_err(anyhow::Error::new)
    })();
    unsafe { context.Unmap(&resource, 0) };
    result
}

#[allow(dead_code)]
fn _assert_device_is_kept_in_data_plane(_: &ID3D11Device) {}

#[cfg(test)]
#[path = "cadence_replay_tests.rs"]
mod cadence_replay_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(source_seq: u64, source_qpc: i64, present_seq: u64) -> CapturedFrame {
        CapturedFrame {
            slot: source_seq as usize,
            source_seq,
            source_qpc,
            callback_start_qpc: source_qpc,
            callback_ready_qpc: source_qpc,
            queue_depth: 1,
            present_seq: Some(present_seq),
        }
    }

    #[test]
    fn cfr_selector_keeps_the_last_candidate_and_counts_superseded_sources() {
        let mut selector = CfrSlotSelector::default();
        assert!(matches!(
            selector.observe(frame(1, 10, 100), 20),
            CfrObservation::Candidate { replaced: None }
        ));
        let CfrObservation::Candidate {
            replaced: Some(replaced),
        } = selector.observe(frame(2, 15, 101), 20)
        else {
            panic!("second candidate did not supersede the first");
        };
        assert_eq!(replaced.present_seq, Some(100));
        let decision = selector.seal(None).unwrap();
        assert!(!decision.duplicate);
        assert_eq!(decision.identity.present_seq, Some(101));
        assert_eq!(decision.source_superseded, 1);
    }

    #[test]
    fn cfr_selector_seals_at_the_first_future_observation() {
        let mut selector = CfrSlotSelector::default();
        let _ = selector.observe(frame(1, 10, 100), 20);
        let CfrObservation::Future(future) = selector.observe(frame(2, 20, 101), 20) else {
            panic!("slot-end observation was not held for the next slot");
        };
        assert_eq!(future.present_seq, Some(101));
        assert_eq!(selector.seal(None).unwrap().identity.present_seq, Some(100));
    }

    #[test]
    fn duplicate_slot_inherits_the_previous_present_identity() {
        let previous = SelectedSourceIdentity {
            source_seq: 4,
            source_qpc: 40,
            present_seq: Some(70000),
        };
        let decision = CfrSlotSelector::default().seal(Some(previous)).unwrap();
        assert!(decision.duplicate);
        assert_eq!(decision.identity, previous);
        assert!(decision.selected.is_none());
    }

    #[test]
    fn first_duplicate_slot_fails_closed() {
        assert_eq!(
            CfrSlotSelector::default().seal(None).unwrap_err(),
            CfrSelectionError::FirstSlotHasNoSource
        );
    }

    #[test]
    fn late_cfr_consumer_uses_an_already_queued_source_before_sealing() {
        let (send, receive) = bounded(2);
        send.send(frame(2, 15, 101)).unwrap();
        // Consumer wakes after end=20 + seal delay=4. The frame is already queued.
        let CfrIngressPoll::Frame(queued) = poll_cfr_ingress(&receive, 30, 24).unwrap() else {
            panic!("a seal deadline must not hide an already queued source");
        };
        let mut selector = CfrSlotSelector::default();
        assert!(matches!(
            selector.observe(queued, 20),
            CfrObservation::Candidate { replaced: None }
        ));
        let previous = SelectedSourceIdentity::from(&frame(1, 5, 100));
        let decision = selector.seal(Some(previous)).unwrap();
        assert!(!decision.duplicate);
        assert_eq!(decision.identity.present_seq, Some(101));
        assert_eq!(decision.identity.source_qpc, 15);
    }

    #[test]
    fn late_cfr_drain_preserves_latest_source_and_future_slot_boundary() {
        let (send, receive) = bounded(3);
        for sample in [frame(1, 10, 100), frame(2, 15, 101), frame(3, 20, 102)] {
            send.send(sample).unwrap();
        }
        let mut selector = CfrSlotSelector::default();
        let future = loop {
            let CfrIngressPoll::Frame(sample) = poll_cfr_ingress(&receive, 30, 24).unwrap() else {
                panic!("queued frame unexpectedly ignored");
            };
            if let CfrObservation::Future(sample) = selector.observe(sample, 20) {
                break sample;
            }
        };
        let decision = selector.seal(None).unwrap();
        assert!(!decision.duplicate);
        assert_eq!(decision.identity.present_seq, Some(101));
        assert_eq!(decision.source_superseded, 1);
        assert_eq!(future.present_seq, Some(102));
        assert_eq!(future.source_qpc, 20);
        let mut next = CfrSlotSelector::default();
        assert!(matches!(
            next.observe(future, 40),
            CfrObservation::Candidate { replaced: None }
        ));
        assert_eq!(next.seal(None).unwrap().identity.present_seq, Some(102));
    }

    #[test]
    fn empty_cfr_ingress_waits_only_before_the_existing_deadline() {
        let (_send, receive) = bounded(1);
        assert!(matches!(
            poll_cfr_ingress(&receive, 23, 24).unwrap(),
            CfrIngressPoll::Wait
        ));
        for now in [24, 30] {
            assert!(matches!(
                poll_cfr_ingress(&receive, now, 24).unwrap(),
                CfrIngressPoll::DeadlineReached
            ));
        }
    }

    #[test]
    fn disconnected_cfr_ingress_cannot_be_treated_as_a_duplicate_slot() {
        let (send, receive) = bounded(1);
        drop(send);
        for now in [23, 24, 30] {
            assert!(matches!(
                poll_cfr_ingress(&receive, now, 24),
                Err(TryRecvError::Disconnected)
            ));
        }
    }

    #[test]
    fn source_timestamp_filter_ignores_a_transient_regression_and_recovers() {
        assert_eq!(
            classify_source_timestamp(None, 100, 0),
            SourceTimestampDecision::Accept
        );
        assert_eq!(
            classify_source_timestamp(Some(100), 100, 0),
            SourceTimestampDecision::Ignore { streak: 1 }
        );
        assert_eq!(
            classify_source_timestamp(Some(100), 99, 1),
            SourceTimestampDecision::Ignore { streak: 2 }
        );
        assert_eq!(
            classify_source_timestamp(Some(100), 101, 2),
            SourceTimestampDecision::Accept
        );
    }

    #[test]
    fn source_timestamp_filter_stops_a_persistent_regression() {
        assert_eq!(
            classify_source_timestamp(Some(100), 99, MAX_CONSECUTIVE_NON_MONOTONIC_SOURCE_FRAMES,),
            SourceTimestampDecision::Stop {
                streak: MAX_CONSECUTIVE_NON_MONOTONIC_SOURCE_FRAMES + 1,
            }
        );
    }

    /// The queue exists to cover the encoder drain plus a separately bounded
    /// WinRT delivery burst. This test keeps both couplings explicit: raising
    /// `ENCODER_DRAIN_CFR_FRAMES` without the steady-state capacity following
    /// turns an encoder timeout into a queue overflow (issue #266), while
    /// removing the delivery guard reproduces issue #636's 40 -> 41 and
    /// 43 -> 44 transients.
    #[test]
    fn capture_queue_covers_encoder_wait_source_window_and_frame_pool_burst() {
        assert_eq!(
            CAPTURE_QUEUE_CFR_WINDOWS,
            1 + ENCODER_DRAIN_CFR_FRAMES,
            "the queue must hold the unconsumed observation window plus every window the drain lets in"
        );
        assert_eq!(
            CAPTURE_QUEUE_STEADY_STATE_CAPACITY,
            WGC_SOURCE_OBSERVATIONS_PER_CFR_FRAME * CAPTURE_QUEUE_CFR_WINDOWS
        );
        assert_eq!(
            CAPTURE_QUEUE_BURST_GUARD,
            WGC_FRAME_POOL_BUFFER_COUNT as usize + WGC_CALLBACK_IN_FLIGHT_GUARD
        );
        assert_eq!(
            CAPTURE_QUEUE_CAPACITY,
            CAPTURE_QUEUE_STEADY_STATE_CAPACITY + CAPTURE_QUEUE_BURST_GUARD
        );
        assert_eq!(BGRA_RING_SIZE, CAPTURE_QUEUE_CAPACITY + 3);

        // Worst case is the requested observation rate: a drain of
        // ENCODER_DRAIN_CFR_FRAMES periods enqueues that many windows, and the
        // queue must still have the selector's own window free.
        let worst_case_arrivals_during_drain =
            WGC_SOURCE_OBSERVATIONS_PER_CFR_FRAME * ENCODER_DRAIN_CFR_FRAMES;
        assert!(
            worst_case_arrivals_during_drain + WGC_SOURCE_OBSERVATIONS_PER_CFR_FRAME
                <= CAPTURE_QUEUE_STEADY_STATE_CAPACITY
        );
        assert_eq!(CAPTURE_QUEUE_STEADY_STATE_CAPACITY, 40);
        assert_eq!(
            CAPTURE_QUEUE_CAPACITY, 44,
            "the complete three-frame WinRT pool and its in-flight callback must fit"
        );

        // Reproduce the hardware failure shape directly: the steady-state
        // encoder window and all retained frame-pool observations are already
        // queued, then the currently executing callback tries to publish its
        // observation. Capacity 43 rejected exactly this final send.
        let (sender, _receiver) = bounded::<()>(CAPTURE_QUEUE_CAPACITY);
        for _ in 0..(CAPTURE_QUEUE_STEADY_STATE_CAPACITY + WGC_FRAME_POOL_BUFFER_COUNT as usize) {
            sender
                .try_send(())
                .expect("steady-state and retained frame-pool observations must fit");
        }
        sender
            .try_send(())
            .expect("the in-flight WGC callback must have one publish slot");
        assert!(matches!(sender.try_send(()), Err(TrySendError::Full(()))));

        let diagnostic = IngressError::QueueExhausted {
            queue_depth: CAPTURE_QUEUE_CAPACITY,
            queue_capacity: CAPTURE_QUEUE_CAPACITY,
            ring_size: BGRA_RING_SIZE,
        }
        .to_string();
        assert!(diagnostic.contains(&format!(
            "depth {CAPTURE_QUEUE_CAPACITY}/{CAPTURE_QUEUE_CAPACITY}"
        )));
        assert!(diagnostic.contains(&format!("{BGRA_RING_SIZE} BGRA slots")));
    }

    #[test]
    fn gpu_copy_wait_uses_liveness_watchdog_above_queue_headroom() {
        let frequency = 10_000_000;

        for fps in [30, 60, 120, 240] {
            let frame_period = frequency / fps;
            assert_eq!(
                gpu_copy_wait_limit_qpc(fps as u32, 1, frequency),
                frequency * GPU_COPY_LIVENESS_WATCHDOG_MS / 1_000,
                "ordinary capture rates must use the wall-clock liveness watchdog"
            );
            assert!(
                gpu_copy_wait_limit_qpc(fps as u32, 1, frequency)
                    > frame_period * ENCODER_DRAIN_CFR_FRAMES as i64,
                "source loss is guarded by queue overflow, not a shorter wall-clock timeout"
            );
        }
    }

    #[test]
    fn gpu_copy_wait_never_shrinks_below_queue_coupled_headroom() {
        let frequency = 10_000_000;

        assert_eq!(
            gpu_copy_wait_limit_qpc(1, 1, frequency),
            frequency * ENCODER_DRAIN_CFR_FRAMES as i64
        );
    }

    #[test]
    fn gpu_copy_wait_retries_an_incomplete_query_until_completion() {
        let mut completions = VecDeque::from([false, true]);
        let mut times = VecDeque::from([110, 120]);

        let completed_qpc = wait_for_gpu_copy_completion_with(
            || Ok(completions.pop_front().unwrap()),
            || Ok(times.pop_front().unwrap()),
            100,
            50,
            1_000,
        )
        .unwrap();

        assert_eq!(completed_qpc, 120);
        assert!(completions.is_empty());
        assert!(times.is_empty());
    }

    #[test]
    fn gpu_copy_wait_accepts_the_observed_60fps_scheduling_jitter() {
        let frequency = 10_000_000;
        let limit = gpu_copy_wait_limit_qpc(60, 1, frequency);
        let mut completions = VecDeque::from([false, true]);
        let mut times = VecDeque::from([380_900, 400_000]);

        let completed_qpc = wait_for_gpu_copy_completion_with(
            || Ok(completions.pop_front().unwrap()),
            || Ok(times.pop_front().unwrap()),
            0,
            limit,
            frequency,
        )
        .unwrap();

        assert_eq!(completed_qpc, 400_000);
        assert!(limit > 380_900, "the real 38.090ms wait must fit");
    }

    #[test]
    fn gpu_copy_wait_accepts_observed_30fps_transient_within_queue_headroom() {
        let frequency = 10_000_000;
        let limit = gpu_copy_wait_limit_qpc(30, 1, frequency);
        let mut completions = VecDeque::from([false, true]);
        let mut times = VecDeque::from([821_750, 830_000]);

        let completed_qpc = wait_for_gpu_copy_completion_with(
            || Ok(completions.pop_front().unwrap()),
            || Ok(times.pop_front().unwrap()),
            0,
            limit,
            frequency,
        )
        .unwrap();

        assert_eq!(completed_qpc, 830_000);
        assert!(limit > 821_750, "the observed 82.175ms wait must fit");
    }

    #[test]
    fn gpu_copy_wait_accepts_reported_30fps_133ms_transient_without_source_loss() {
        let frequency = 10_000_000;
        let limit = gpu_copy_wait_limit_qpc(30, 1, frequency);
        let mut completions = VecDeque::from([false, true]);
        let mut times = VecDeque::from([1_333_330, 1_340_000]);

        let completed_qpc = wait_for_gpu_copy_completion_with(
            || Ok(completions.pop_front().unwrap()),
            || Ok(times.pop_front().unwrap()),
            0,
            limit,
            frequency,
        )
        .unwrap();

        assert_eq!(completed_qpc, 1_340_000);
        assert!(limit > 1_333_330, "the reported 133.333ms stall must fit");
    }

    #[test]
    fn gpu_copy_wait_stops_when_an_incomplete_query_exceeds_the_budget() {
        let error = wait_for_gpu_copy_completion_with(|| Ok(false), || Ok(151), 100, 50, 1_000)
            .unwrap_err();

        assert!(format!("{error:#}").contains("51.000ms of 50.000ms"));
    }

    #[test]
    fn gpu_copy_wait_propagates_query_errors_without_relabeling_them_as_timeouts() {
        let error = wait_for_gpu_copy_completion_with(
            || anyhow::bail!("query failed"),
            || Ok(100),
            100,
            50,
            1_000,
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("query failed"));
    }
}

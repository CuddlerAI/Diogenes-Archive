//! P4 Windows 侧：截获常量缓冲写入，扫出相机矩阵候选。
//!
//! 判定全部在 [`depth_lab::camera`] / [`depth_lab::camera_track`] 里（跨平台可测），
//! 本模块只做三件 Windows 才能做的事：
//!
//! 1. hook `Map`/`Unmap` 与 `UpdateSubresource`，拿到常量缓冲的字节；
//! 2. discovery 保存最近上传字节，但只在受审的主场景 pass/draw 窗口送后台扫描；
//!    production 同样只在钉死的窗口取绑定快照；
//! 3. discovery 落 `camera-candidates.json`，production 落 `camera-pose-index.jsonl`。
//!
//! # 为什么「只在主场景 pass 里取」不是性能优化
//!
//! [77 号 §5.11.2 第一步](../../../docs/77-depth-ground-truth-hook-experiment-plan.md)
//! 把它写成「缩小搜索面」，读起来像可选的加速。**它不是。**
//!
//! `camera_track` 的测试证明：视模型（手/武器）的投影矩阵与主相机同宽高比、
//! 只差近平面，三条离线判据**一条都分不开**，选择器只能报 `Ambiguous`。
//! 唯一的消歧信息就是「这个常量缓冲是在主场景 pass 里上传的」。
//! 省掉这一步，后面再多的打分也救不回来。
//!
//! # 为什么扫描必须离开渲染线程
//!
//! `scan_candidates` 按 4 字节步进，一个 4 KB 的常量缓冲就是约 1000 次
//! 4×4 矩阵构造 + 分类。常量缓冲每帧被 `Map` 几十上百次。
//! 在 `Unmap` hook 里同步扫 = 把 C5 刚压下去的开销重新顶上来
//! （#443 的教训：decode 在 Present hook 里跑，每次回读 11 ms）。
//!
//! 所以渲染线程只缓存最新常量缓冲，并把每个 sampled Present 聚合成一个有界 batch 送进
//! 后台线程。队列满时不阻塞游戏，但当前 phase 会明确标为 `queue_saturated`，不得拿来
//! 裁决 production binding；控制 ACK 走独立通道，不与这些 batch 抢槽位。

use std::collections::{BTreeMap, HashMap};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{
    Receiver, RecvTimeoutError, Sender, SyncSender, TryRecvError, TrySendError, channel,
    sync_channel,
};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use depth_lab::{
    BoundBufferAtDraw, CAMERA_POSE_DIAGNOSTIC_SCHEMA_V1, CameraBinding, CameraDiscoveryConfig,
    CameraDiscoveryTracker, CameraDrawSample, CameraPoseAssembler, CameraPoseAssemblyError,
    CameraPoseDiagnosticReason, CameraPoseDiagnosticRow, CameraPoseStreamContext,
    DiscoveryBindingSnapshot, DiscoveryPresentBatch, DiscoveryReferenceCameraObservation,
    DiscoverySamplingStats, MatrixSourceBinding, Projection, ShaderStage, StreamIdentity,
    ViewSourceBinding, fallout_render_rotation_from_bound_buffer, projection_from_bound_buffer,
    view_pose_from_bound_buffer,
};
use windows::Win32::System::Performance::QueryPerformanceCounter;

use crate::bannerlord_player_probe::BannerlordPlayerSampler;
use crate::camera_writer::CameraPoseIndexWriter;
use crate::cp2077_player_probe::Cp2077PlayerSampler;
use crate::fallout_build::{FALLOUT4_1_11_240, FalloutCapturePurpose, prepare_fallout_capture};
use crate::fallout_camera_probe::{FalloutCameraSample, FalloutCameraSampler};
use crate::fallout_player_probe::FalloutPlayerSampler;
use crate::player_writer::{PlayerPoseIndexWriter, PlayerRootSample};
use crate::probe::log;

/// discovery 按 sampled Present 入队；容量按 batch 而非 Draw 计，不能靠扩队列掩盖背压。
const DISCOVERY_QUEUE_CAPACITY: usize = 8;
const PRODUCTION_QUEUE_CAPACITY: usize = 1024;
const MAX_TRACKED_PRODUCTION_BUFFERS: usize = 1024;
/// More than one second at Fallout 4's observed 240 Hz. GPU readback normally completes on the
/// next Present; exceeding this window is a real timing failure, not a reason for unbounded state.
const MAX_OBSERVED_PRODUCTION_PROJECTIONS: usize = 512;
const MAX_TRACKED_DISCOVERY_BUFFERS: usize = 1024;
const MAX_DISCOVERY_BINDINGS_PER_BATCH: usize = 8_192;

/// 单个常量缓冲的最大采样字节数。
///
/// 常量缓冲通常几 KB。设上限是为了防一个异常大的缓冲把队列内存吃光——
/// 相机矩阵不会出现在一个 1 MB 的缓冲深处，真出现了也该先怀疑认错了资源。
const MAX_BUFFER_BYTES: usize = 64 * 1024;

const DISCOVERY_WORKER_POLL_INTERVAL: Duration = Duration::from_millis(25);
const DISCOVERY_CHECKPOINT_INTERVAL: Duration = Duration::from_secs(2);
const DISCOVERY_REPORT_BUFFER_BYTES: usize = 256 * 1024;
const DISCOVERY_SLOW_BATCH_THRESHOLD: Duration = Duration::from_millis(250);
const DISCOVERY_CALLBACK_QUIESCE_TIMEOUT: Duration = Duration::from_secs(2);
const DISCOVERY_DRAIN_TIMEOUT: Duration = Duration::from_secs(8);
const DISCOVERY_ACK_TIMEOUT: Duration = Duration::from_secs(10);

static DISCOVERY_ENABLED: AtomicBool = AtomicBool::new(false);
static PRODUCTION_ENABLED: AtomicBool = AtomicBool::new(false);
/// Production remains enabled during lifecycle warmup so hooks and resource caches can settle,
/// but timeline events must not reach the formal writers until the probe arms the generation.
static PRODUCTION_ARMED: AtomicBool = AtomicBool::new(false);
static PRODUCTION_FAILED: AtomicBool = AtomicBool::new(false);
static PRODUCTION_VIEW_IS_HYBRID: AtomicBool = AtomicBool::new(false);
static PRODUCTION_TARGET_DRAW_LOGGED: AtomicBool = AtomicBool::new(false);
static DISCOVERY_WORKER: OnceLock<DiscoveryWorkerHandle> = OnceLock::new();
static DISCOVERY_WORKER_INIT: Mutex<()> = Mutex::new(());
static PRODUCTION_TX: Mutex<Option<SyncSender<ProductionEvent>>> = Mutex::new(None);
static PRODUCTION_JOIN: Mutex<Option<JoinHandle<()>>> = Mutex::new(None);
static PRODUCTION_SEND_LOCK: Mutex<()> = Mutex::new(());
#[derive(Clone, Copy)]
enum PlayerPoseOutput {
    Disabled,
    Enabled,
    // Existing D3D11 ACKs advertise a player count even without a sampler.
    // Preserve that coverage contract; the CP2077 fix must not relax it.
    LegacyCount,
}

struct ProductionOutput {
    root: PathBuf,
    // Preserve the initialized stream contract through begin_stop_production,
    // which clears the samplers before the closed files are counted.
    player_pose: PlayerPoseOutput,
}

static PRODUCTION_ROOT: Mutex<Option<ProductionOutput>> = Mutex::new(None);
static PRODUCTION_ERROR: Mutex<Option<String>> = Mutex::new(None);
static CAMERA_SAMPLER: Mutex<Option<FalloutCameraSampler>> = Mutex::new(None);
static PLAYER_SAMPLER: Mutex<Option<PlayerSampler>> = Mutex::new(None);
/// Only D3D12 production reads this flag. Camera-only bindings must not touch
/// the player sampler mutex or manufacture an unexpected player result.
static D3D12_PLAYER_SAMPLING_ENABLED: AtomicBool = AtomicBool::new(false);
/// 控制线程周期审计用的任务（只含钉死的槽地址）。渲染线程从不触碰这把锁。
static POINTER_AUDIT_TASKS: Mutex<(
    Option<crate::fallout_camera_probe::CameraAuditTask>,
    Option<crate::fallout_player_probe::PlayerAuditTask>,
    Option<crate::cp2077_player_probe::PlayerAuditTask>,
)> = Mutex::new((None, None, None));

/// 控制线程入口：走一遍两条指针链并发布带 generation/QPC 时戳的结论。
///
/// #504 遥测两轮实证渲染线程上的 VirtualQuery 会被游戏的内存活动拖到毫秒级，
/// 这里是 production 期**唯一**执行 VirtualQuery 的地方。与 StartRun/StopRun
/// 同线程串行执行，Stop 清空结论后不存在迟到写入的窗口。
pub fn run_background_pointer_audit(generation: u64, now_qpc: i64) {
    let Ok(tasks) = POINTER_AUDIT_TASKS.lock() else {
        return;
    };
    if let Some(task) = tasks.0.as_ref() {
        crate::fallout_camera_probe::publish_camera_verdict(task.audit(generation, now_qpc));
    }
    if let Some(task) = tasks.1.as_ref() {
        crate::fallout_player_probe::publish_player_verdict(task.audit(generation, now_qpc));
    }
    if let Some(task) = tasks.2.as_ref() {
        let _perf = crate::perf::aggregate_scope(crate::perf::Section::PoseAudit);
        crate::cp2077_player_probe::publish_player_verdict(task.audit(generation));
    }
}

enum PlayerSampler {
    Fallout(FalloutPlayerSampler),
    Bannerlord(BannerlordPlayerSampler),
    Cp2077(Box<Cp2077PlayerSampler>),
}

impl PlayerSampler {
    /// D3D11 Present path, where no rendered camera exists yet.
    fn sample(
        &mut self,
        present_seq: u64,
        present_qpc: i64,
        generation: u64,
    ) -> Result<PlayerRootSample, String> {
        match self {
            Self::Fallout(sampler) => sampler.sample(present_seq, present_qpc, generation),
            Self::Bannerlord(sampler) => sampler.sample(present_seq, present_qpc, generation),
            Self::Cp2077(_) => {
                Err("CP2077 player pose needs the camera its Present rendered".into())
            }
        }
    }

    /// D3D12 pose path. CP2077 associates its actor root with a Present by the camera that
    /// Present rendered, not by the moment memory was read.
    fn sample_with_camera(
        &mut self,
        camera: &depth_lab::CameraPoseRow,
        generation: u64,
    ) -> Result<PlayerRootSample, String> {
        match self {
            Self::Cp2077(sampler) => sampler.sample(
                camera.present_seq,
                camera.present_qpc,
                generation,
                camera.camera_position,
                camera.camera_rotation_quaternion,
            ),
            _ => self.sample(camera.present_seq, camera.present_qpc, generation),
        }
    }

    fn source(&self) -> &'static str {
        match self {
            Self::Fallout(_) => "fallout4-player-root-exact-build/v1",
            Self::Bannerlord(_) => crate::bannerlord_player_probe::PROVIDER,
            Self::Cp2077(_) => crate::cp2077_player_probe::PROVIDER,
        }
    }
}
static PRODUCTION_GENERATION: AtomicU64 = AtomicU64::new(0);
static STALE_PRODUCTION_CALLBACK_LOGGED: AtomicBool = AtomicBool::new(false);
static PRODUCTION_LAST_SUBMITTED_PRESENT: AtomicU64 = AtomicU64::new(u64::MAX);
static PRODUCTION_LAST_WRITTEN_PRESENT: AtomicU64 = AtomicU64::new(u64::MAX);
static PRODUCTION_WARMUP_PROJECTION_PRESENT: AtomicU64 = AtomicU64::new(u64::MAX);
static PRODUCTION_WARMUP_VIEW_PRESENT: AtomicU64 = AtomicU64::new(u64::MAX);
/// CP2077 only: `(present_seq, join error)` of the latest warmup Present. Its player pose is
/// joined to the camera a Present rendered, which the readiness query below does not have, so
/// `on_d3d12_pose` runs the real join on every warmup camera row and leaves the verdict here.
/// Ready therefore also proves that the sampler ring already serves the live camera.
static PRODUCTION_WARMUP_CAMERA_JOIN: Mutex<Option<(u64, Option<String>)>> = Mutex::new(None);

fn reset_warmup_camera_join() {
    if let Ok(mut verdict) = PRODUCTION_WARMUP_CAMERA_JOIN.lock() {
        *verdict = None;
    }
}
static PRODUCTION_BIND_SLOT: AtomicU32 = AtomicU32::new(u32::MAX);
static PRODUCTION_VIEW_BIND_SLOT: AtomicU32 = AtomicU32::new(u32::MAX);
static PRODUCTION_PASS_ORDINAL: AtomicU32 = AtomicU32::new(u32::MAX);
static PRODUCTION_DRAW_ORDINAL: AtomicU32 = AtomicU32::new(u32::MAX);
static PRODUCTION_BUFFER_BYTES: AtomicU32 = AtomicU32::new(0);
static PRODUCTION_VIEW_BUFFER_BYTES: AtomicU32 = AtomicU32::new(0);
/// 只有 exact-build 的 game-camera sampler 能在非 depth 证据帧继续独立产 pose；
/// 纯 GPU view binding 仍必须逐 Present 走完整 draw 路径。
static PRODUCTION_CAN_HOLD_PROJECTION: AtomicBool = AtomicBool::new(false);
const MAX_PRODUCTION_SOURCE_TAGS: usize = 16;
static PRODUCTION_SOURCE_TAGS: [AtomicU64; MAX_PRODUCTION_SOURCE_TAGS] =
    [const { AtomicU64::new(0) }; MAX_PRODUCTION_SOURCE_TAGS];
static PRODUCTION_SOURCE_TAG_BLOOM: AtomicU64 = AtomicU64::new(0);
static PRODUCTION_DIRTY_SOURCE_MASK: AtomicU64 = AtomicU64::new(0);
const MAX_PENDING_PRODUCTION_BUFFERS: usize = 64;
static PENDING_PRODUCTION_BUFFERS: Mutex<Option<HashMap<u64, Vec<u8>>>> = Mutex::new(None);
static PENDING_PRODUCTION_PRESENT: AtomicU64 = AtomicU64::new(u64::MAX);
static KNOWN_PRODUCTION_BUFFERS: Mutex<Option<HashMap<u64, Vec<u8>>>> = Mutex::new(None);
static OBSERVED_PRODUCTION_PROJECTIONS: Mutex<Option<ObservedProductionProjections>> =
    Mutex::new(None);
static PRODUCTION_VIEW_SOURCE: Mutex<Option<ViewSourceBinding>> = Mutex::new(None);
static DISCOVERY_EPOCH: AtomicU64 = AtomicU64::new(0);
static DISCOVERY_CALLBACKS_IN_FLIGHT: AtomicU64 = AtomicU64::new(0);
static DISCOVERY_OBSERVED_PRESENTS: AtomicU64 = AtomicU64::new(0);
static DISCOVERY_SCHEDULED_PRESENTS: AtomicU64 = AtomicU64::new(0);
static DISCOVERY_OBSERVED_DRAWS: AtomicU64 = AtomicU64::new(0);
static DISCOVERY_SCHEDULED_DRAWS: AtomicU64 = AtomicU64::new(0);
static DISCOVERY_SUBMITTED_DRAWS: AtomicU64 = AtomicU64::new(0);
static DISCOVERY_QUEUE_DROPPED: AtomicU64 = AtomicU64::new(0);
static DISCOVERY_RUNTIME_DROPPED: AtomicU64 = AtomicU64::new(0);
static DISCOVERY_BUFFER_OVERFLOWS: AtomicU64 = AtomicU64::new(0);
static DISCOVERY_DSV_AMBIGUITIES: AtomicU64 = AtomicU64::new(0);
static DISCOVERY_BOUNDARY_DISCARDED_PRESENTS: AtomicU64 = AtomicU64::new(0);
static DISCOVERY_BOUNDARY_DISCARDED_DRAWS: AtomicU64 = AtomicU64::new(0);
static QUEUE_FULL_LOGGED: AtomicBool = AtomicBool::new(false);
static TIMELINE_PRESENT: AtomicU64 = AtomicU64::new(u64::MAX);
static NEXT_PASS_ORDINAL: AtomicU32 = AtomicU32::new(0);
static CURRENT_PASS_ORDINAL: AtomicU32 = AtomicU32::new(0);
static CURRENT_DRAW_ORDINAL: AtomicU32 = AtomicU32::new(0);
static PRODUCTION_EXACT_DRAW_PRESENT: AtomicU64 = AtomicU64::new(u64::MAX);
static PRODUCTION_EXACT_DRAW_PASS: AtomicU32 = AtomicU32::new(u32::MAX);
static PRODUCTION_EXACT_DRAW_ORDINAL: AtomicU32 = AtomicU32::new(u32::MAX);
static PRODUCTION_PROJECTION_OBSERVED_PRESENT: AtomicU64 = AtomicU64::new(u64::MAX);
// Fast-path gate for the overwhelmingly common unscheduled draw. Without this,
// every draw in a busy Fallout frame contends on DISCOVERY_RUNTIME and turns
// harmless lock races into fail-closed `runtime_dropped_callbacks` evidence.
static DISCOVERY_SCHEDULED_PRESENT_SEQ: AtomicU64 = AtomicU64::new(u64::MAX);

struct ObservedProductionProjections {
    generation: u64,
    source: Option<MatrixSourceBinding>,
    render_width: u32,
    render_height: u32,
    frames: BTreeMap<u64, Projection>,
}

pub(crate) fn begin_external_projection_observations(
    generation: u64,
    width: u32,
    height: u32,
) -> Result<(), String> {
    if generation == 0 || width == 0 || height == 0 {
        return Err("external projection identity/geometry is invalid".into());
    }
    *OBSERVED_PRODUCTION_PROJECTIONS
        .lock()
        .map_err(|_| "external projection cache poisoned")? = Some(ObservedProductionProjections {
        generation,
        source: None,
        render_width: width,
        render_height: height,
        frames: BTreeMap::new(),
    });
    Ok(())
}

pub(crate) fn record_external_projection_observation(
    generation: u64,
    present_seq: u64,
    projection: Projection,
) -> Result<(), String> {
    if !projection.a.is_finite() || !projection.b.is_finite() || projection.metric_scale != 1.0 {
        return Err("external projection must be a finite unscaled engine observation".into());
    }
    let mut guard = OBSERVED_PRODUCTION_PROJECTIONS
        .lock()
        .map_err(|_| "external projection cache poisoned")?;
    let observed = guard
        .as_mut()
        .ok_or("external projection cache is not initialized")?;
    if observed.generation != generation || observed.source.is_some() {
        return Err("external projection generation/source mismatch".into());
    }
    if observed.frames.contains_key(&present_seq) {
        return Err("duplicate external same-Present projection".into());
    }
    if observed.frames.len() >= MAX_OBSERVED_PRODUCTION_PROJECTIONS {
        return Err("external projection cache full before depth drain".into());
    }
    observed.frames.insert(present_seq, projection);
    Ok(())
}

/// Frame-local identity of the selected DSV pass currently observed by Camera production.
///
/// The identity deliberately contains no COM pointer. A selected depth resource can be rebound
/// several times in one Present, so `(present_seq, pass_ordinal)` is the pass instance; a resource
/// address alone is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MainPassIdentity {
    pub pass_ordinal: u32,
    pub exact_draw_ordinal: Option<u32>,
    pub projection_observed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExactProductionDrawIdentity {
    pub pass_ordinal: u32,
    pub draw_ordinal: u32,
    pub projection_observed: bool,
}

struct DiscoveryWorkerHandle {
    data_tx: SyncSender<DiscoveryDataEvent>,
    control_tx: Sender<DiscoveryControlEvent>,
}

struct DiscoveryDataEvent {
    epoch: u64,
    batch_seq: u64,
    batch: DiscoveryPresentBatch,
    sampling: DiscoverySamplingStats,
}

enum DiscoveryControlEvent {
    Start {
        epoch: u64,
        output_dir: PathBuf,
        config: Box<CameraDiscoveryConfig>,
        selected_dsv_descriptor: Option<depth_lab::DsvDiscoveryDescriptor>,
        reply: SyncSender<Result<(), String>>,
    },
    Flush {
        epoch: u64,
        cutoff_batch_seq: u64,
        sampling: DiscoverySamplingStats,
        reply: SyncSender<Result<(), String>>,
    },
}

#[derive(Debug, Clone)]
struct VersionedDiscoveryBuffer {
    version: u64,
    cold_seed: bool,
    bytes: Arc<[u8]>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct DiscoveryBindingKey {
    pass_ordinal: u32,
    bind_slot: u32,
    buffer_tag: u64,
    buffer_version: u64,
}

#[derive(Debug)]
struct PendingDiscoveryBinding {
    bytes: Arc<[u8]>,
    draw_ordinals: Vec<u32>,
}

#[derive(Debug)]
struct PendingDiscoveryBatch {
    present_seq: u64,
    logical_draws: u64,
    bindings: BTreeMap<DiscoveryBindingKey, PendingDiscoveryBinding>,
    comparison_camera_reference: Option<DiscoveryReferenceCameraObservation>,
    comparison_camera_error: Option<String>,
    draw_contexts: Vec<depth_lab::camera_discovery::DiscoveryDrawContext>,
}

impl PendingDiscoveryBatch {
    fn new(present_seq: u64) -> Self {
        Self {
            present_seq,
            logical_draws: 0,
            bindings: BTreeMap::new(),
            comparison_camera_reference: None,
            comparison_camera_error: None,
            draw_contexts: Vec::new(),
        }
    }

    fn into_batch(self) -> DiscoveryPresentBatch {
        DiscoveryPresentBatch {
            present_seq: self.present_seq,
            logical_draws: self.logical_draws,
            bindings: self
                .bindings
                .into_iter()
                .map(|(key, binding)| DiscoveryBindingSnapshot {
                    pass_ordinal: key.pass_ordinal,
                    bind_slot: key.bind_slot,
                    draw_ordinals: binding.draw_ordinals,
                    bytes: binding.bytes,
                })
                .collect(),
            comparison_camera_reference: self.comparison_camera_reference,
            comparison_camera_error: self.comparison_camera_error,
            draw_contexts: self.draw_contexts,
        }
    }

    fn record_draw(
        &mut self,
        pass_ordinal: u32,
        draw_ordinal: u32,
        vs_slots: &BTreeMap<u32, u64>,
        latest_buffers: &HashMap<u64, VersionedDiscoveryBuffer>,
    ) -> u64 {
        self.logical_draws = self.logical_draws.saturating_add(1);
        let mut overflows = 0u64;
        for (&bind_slot, &buffer_tag) in vs_slots {
            let Some(buffer) = latest_buffers.get(&buffer_tag) else {
                continue;
            };
            let key = DiscoveryBindingKey {
                pass_ordinal,
                bind_slot,
                buffer_tag,
                buffer_version: buffer.version,
            };
            if let Some(binding) = self.bindings.get_mut(&key) {
                binding.draw_ordinals.push(draw_ordinal);
            } else if self.bindings.len() >= MAX_DISCOVERY_BINDINGS_PER_BATCH {
                overflows = overflows.saturating_add(1);
            } else {
                self.bindings.insert(
                    key,
                    PendingDiscoveryBinding {
                        bytes: buffer.bytes.clone(),
                        draw_ordinals: vec![draw_ordinal],
                    },
                );
            }
        }
        overflows
    }
}

struct DiscoveryRuntime {
    epoch: u64,
    config: CameraDiscoveryConfig,
    first_present_seq: Option<u64>,
    scheduled_present_seq: Option<u64>,
    latest_buffers: HashMap<u64, VersionedDiscoveryBuffer>,
    vs_slots: BTreeMap<u32, u64>,
    next_buffer_version: u64,
    pending_batch: Option<PendingDiscoveryBatch>,
    last_submitted_batch_seq: u64,
    cold_seed_pending: bool,
}

static DISCOVERY_RUNTIME: Mutex<Option<DiscoveryRuntime>> = Mutex::new(None);

enum ProductionEvent {
    D3d12Pose {
        row: Box<depth_lab::CameraPoseRow>,
        player: Option<Result<PlayerRootSample, String>>,
    },
    BufferUpdate {
        buffer_tag: u64,
        bytes: Vec<u8>,
    },
    VsBind {
        bind_slot: u32,
        buffer: Option<u64>,
    },
    MainScenePass {
        present_seq: u64,
    },
    Draw {
        present_seq: u64,
        sample_qpc: i64,
        pass_ordinal: u32,
        draw_ordinal: u32,
    },
    Present {
        present_seq: u64,
        projection_evidence_frame: bool,
        camera: Option<Result<FalloutCameraSample, String>>,
        player: Option<Result<PlayerRootSample, String>>,
    },
}

/// `Map` 之后、`Unmap` 之前记下来的映射信息。
///
/// 必须在 `Map` 里记：`Unmap` 只给资源指针，拿不到映射地址。
struct Mapping {
    ptr: *mut u8,
    len: usize,
    present_seq: u64,
    generation: u64,
    discovery_epoch: Option<u64>,
}
// SAFETY: 只在下面的 Mutex 之下访问，且指针只在 Map/Unmap 之间有效。
unsafe impl Send for Mapping {}

static MAPPINGS: Mutex<Option<HashMap<u64, Mapping>>> = Mutex::new(None);

#[must_use]
pub fn enabled() -> bool {
    discovery_enabled() || production_enabled()
}

#[must_use]
pub fn discovery_enabled() -> bool {
    DISCOVERY_ENABLED.load(Ordering::Relaxed)
}

/// Claim the one-time cold-start snapshot for the active discovery phase.
///
/// Discovery normally learns constant-buffer bytes from Map/Unmap or UpdateSubresource. A buffer
/// that was populated and bound before the probe started has neither callback, so the first draw
/// inside the configured window must seed the currently bound VS buffers once. Returning the
/// epoch lets the caller reject a snapshot that races with a phase switch.
#[must_use]
pub fn take_discovery_cold_seed_request(present_seq: u64) -> Option<u64> {
    let guard = enter_discovery_callback()?;
    if DISCOVERY_SCHEDULED_PRESENT_SEQ.load(Ordering::Acquire) != present_seq {
        return None;
    }
    let mut runtime = DISCOVERY_RUNTIME.try_lock().ok()?;
    let runtime = runtime.as_mut()?;
    if runtime.epoch != guard.epoch || runtime.scheduled_present_seq != Some(present_seq) {
        return None;
    }
    let pass_ordinal = CURRENT_PASS_ORDINAL.load(Ordering::Acquire);
    let draw_ordinal = CURRENT_DRAW_ORDINAL.load(Ordering::Acquire);
    let in_window = pass_ordinal < runtime.config.max_main_passes_per_present
        && draw_ordinal >= runtime.config.draw_window_start
        && draw_ordinal
            < runtime
                .config
                .draw_window_start
                .saturating_add(runtime.config.draw_window_count);
    if !runtime.cold_seed_pending || !in_window {
        return None;
    }
    runtime.cold_seed_pending = false;
    Some(guard.epoch)
}

/// Return the active discovery epoch only while a D3D11.1 range bind can affect a sampled draw.
/// The caller uses this cheap gate before doing the synchronous, range-only GPU readback.
#[must_use]
pub fn discovery_bound_range_snapshot_epoch(present_seq: u64) -> Option<u64> {
    let guard = enter_discovery_callback()?;
    if DISCOVERY_SCHEDULED_PRESENT_SEQ.load(Ordering::Acquire) != present_seq {
        return None;
    }
    let runtime = DISCOVERY_RUNTIME.try_lock().ok()?;
    let runtime = runtime.as_ref()?;
    if runtime.epoch != guard.epoch || runtime.scheduled_present_seq != Some(present_seq) {
        return None;
    }
    let pass_ordinal = CURRENT_PASS_ORDINAL.load(Ordering::Acquire);
    let draw_ordinal = CURRENT_DRAW_ORDINAL.load(Ordering::Acquire);
    let in_window = pass_ordinal < runtime.config.max_main_passes_per_present
        && draw_ordinal >= runtime.config.draw_window_start
        && draw_ordinal
            < runtime
                .config
                .draw_window_start
                .saturating_add(runtime.config.draw_window_count);
    in_window.then_some(guard.epoch)
}

/// Install bytes read from the exact VS bindings observed by the phase's cold-start draw.
pub fn on_discovery_cold_seed(epoch: u64, buffers: &[(u32, u64, Vec<u8>)]) -> Result<(), String> {
    let guard = enter_discovery_callback_for_epoch(epoch)
        .ok_or_else(|| "camera discovery phase changed during cold-start snapshot".to_string())?;
    let mut runtime = DISCOVERY_RUNTIME
        .try_lock()
        .map_err(|_| "camera discovery runtime is busy during cold-start snapshot".to_string())?;
    let runtime = runtime
        .as_mut()
        .ok_or_else(|| "camera discovery stopped during cold-start snapshot".to_string())?;
    if runtime.epoch != guard.epoch {
        return Err("camera discovery epoch changed during cold-start snapshot".into());
    }
    for (bind_slot, buffer_tag, bytes) in buffers {
        runtime.vs_slots.insert(*bind_slot, *buffer_tag);
        if bytes.len() < 64 {
            continue;
        }
        if !runtime.latest_buffers.contains_key(buffer_tag)
            && runtime.latest_buffers.len() >= MAX_TRACKED_DISCOVERY_BUFFERS
        {
            DISCOVERY_BUFFER_OVERFLOWS.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        let Some(version) = runtime.next_buffer_version.checked_add(1) else {
            DISCOVERY_BUFFER_OVERFLOWS.fetch_add(1, Ordering::Relaxed);
            continue;
        };
        runtime.next_buffer_version = version;
        runtime.latest_buffers.insert(
            *buffer_tag,
            VersionedDiscoveryBuffer {
                version,
                cold_seed: true,
                bytes: Arc::from(bytes.as_slice()),
            },
        );
    }
    Ok(())
}

pub(crate) struct DiscoveryCallbackGuard {
    epoch: u64,
}

impl Drop for DiscoveryCallbackGuard {
    fn drop(&mut self) {
        DISCOVERY_CALLBACKS_IN_FLIGHT.fetch_sub(1, Ordering::AcqRel);
    }
}

pub(crate) fn enter_discovery_callback() -> Option<DiscoveryCallbackGuard> {
    let epoch = DISCOVERY_EPOCH.load(Ordering::Acquire);
    enter_discovery_callback_for_epoch(epoch)
}

fn enter_discovery_callback_for_epoch(epoch: u64) -> Option<DiscoveryCallbackGuard> {
    if !discovery_enabled() {
        return None;
    }
    DISCOVERY_CALLBACKS_IN_FLIGHT.fetch_add(1, Ordering::AcqRel);
    if !DISCOVERY_ENABLED.load(Ordering::Acquire)
        || DISCOVERY_EPOCH.load(Ordering::Acquire) != epoch
    {
        DISCOVERY_CALLBACKS_IN_FLIGHT.fetch_sub(1, Ordering::AcqRel);
        return None;
    }
    Some(DiscoveryCallbackGuard { epoch })
}

fn wait_for_discovery_callbacks() -> Result<(), String> {
    let deadline = Instant::now() + DISCOVERY_CALLBACK_QUIESCE_TIMEOUT;
    loop {
        let in_flight = DISCOVERY_CALLBACKS_IN_FLIGHT.load(Ordering::Acquire);
        if in_flight == 0 {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "P4 discovery callback quiesce 超时: in_flight={in_flight} timeout_ms={}",
                DISCOVERY_CALLBACK_QUIESCE_TIMEOUT.as_millis()
            ));
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[must_use]
pub fn production_enabled() -> bool {
    PRODUCTION_ENABLED.load(Ordering::Relaxed) && !PRODUCTION_FAILED.load(Ordering::Acquire)
}

#[must_use]
pub fn production_failure() -> Option<String> {
    if !PRODUCTION_FAILED.load(Ordering::Acquire) {
        return None;
    }
    PRODUCTION_ERROR
        .lock()
        .ok()
        .and_then(|error| error.clone())
        .or_else(|| Some("Camera production failed without a retained diagnostic".into()))
}

#[must_use]
pub fn production_armed() -> bool {
    production_enabled() && PRODUCTION_ARMED.load(Ordering::Acquire)
}

/// Fallout 的 Camera pose 来自逐 Present 的 direct-memory sampler，投影可只在 depth
/// 证据帧复核；其他 binding 依赖 GPU view matrix，不能套用这条快路径。
#[must_use]
pub fn production_can_hold_projection() -> bool {
    production_enabled() && PRODUCTION_CAN_HOLD_PROJECTION.load(Ordering::Acquire)
}

/// 启动或切换 discovery phase。`output_dir` 与深度产物同目录。
pub fn init_discovery(
    output_dir: PathBuf,
    config: CameraDiscoveryConfig,
    selected_dsv_descriptor: Option<depth_lab::DsvDiscoveryDescriptor>,
    generation: u64,
) -> Result<(), String> {
    let worker = discovery_worker()?;
    let mut runtime = DISCOVERY_RUNTIME
        .lock()
        .map_err(|_| "P4 discovery runtime lock poisoned".to_string())?;
    if let Some(active) = runtime.as_ref() {
        return Err(format!(
            "P4 discovery phase={} epoch={} 尚未 Flush ACK，拒绝启动 phase={}",
            active.config.phase_id, active.epoch, config.phase_id
        ));
    }
    DISCOVERY_ENABLED.store(false, Ordering::Release);
    wait_for_discovery_callbacks()?;
    if let Err(error) = install_discovery_camera_reference(&config, generation) {
        clear_discovery_camera_reference();
        return Err(error);
    }
    let epoch = DISCOVERY_EPOCH.fetch_add(1, Ordering::AcqRel) + 1;
    let (reply_tx, reply_rx) = sync_channel(1);
    if worker
        .control_tx
        .send(DiscoveryControlEvent::Start {
            epoch,
            output_dir,
            config: Box::new(config.clone()),
            selected_dsv_descriptor,
            reply: reply_tx,
        })
        .is_err()
    {
        clear_discovery_camera_reference();
        return Err("P4 discovery worker 控制面已退出".into());
    }
    if let Err(error) = wait_for_discovery_ack(
        reply_rx,
        "Start",
        &config.phase_id,
        epoch,
        DISCOVERY_ACK_TIMEOUT,
    ) {
        clear_discovery_camera_reference();
        return Err(error);
    }

    reset_discovery_stats();
    TIMELINE_PRESENT.store(u64::MAX, Ordering::Release);
    CURRENT_PASS_ORDINAL.store(0, Ordering::Release);
    NEXT_PASS_ORDINAL.store(0, Ordering::Release);
    CURRENT_DRAW_ORDINAL.store(0, Ordering::Release);
    DISCOVERY_SCHEDULED_PRESENT_SEQ.store(u64::MAX, Ordering::Release);
    *runtime = Some(DiscoveryRuntime {
        epoch,
        config: config.clone(),
        first_present_seq: None,
        scheduled_present_seq: None,
        latest_buffers: HashMap::new(),
        vs_slots: BTreeMap::new(),
        next_buffer_version: 0,
        pending_batch: None,
        last_submitted_batch_seq: 0,
        cold_seed_pending: true,
    });
    drop(runtime);
    if let Ok(mut g) = MAPPINGS.lock() {
        *g = Some(HashMap::new());
    }
    DISCOVERY_ENABLED.store(true, Ordering::Release);
    log(&format!(
        "P4 相机 discovery phase={} epoch={} Start ACK 后已启用（此 session 不得用于交付）",
        config.phase_id, epoch
    ));
    Ok(())
}

pub fn stop_discovery() -> Result<(), String> {
    let result = stop_discovery_inner();
    clear_discovery_camera_reference();
    result
}

fn stop_discovery_inner() -> Result<(), String> {
    DISCOVERY_ENABLED.store(false, Ordering::Release);
    wait_for_discovery_callbacks()?;
    DISCOVERY_SCHEDULED_PRESENT_SEQ.store(u64::MAX, Ordering::Release);
    let worker = match DISCOVERY_WORKER.get() {
        Some(worker) => worker,
        None => return Ok(()),
    };
    let mut runtime = DISCOVERY_RUNTIME
        .lock()
        .map_err(|_| "P4 discovery runtime lock poisoned during Flush".to_string())?;
    let Some(active) = runtime.as_mut() else {
        return Ok(());
    };
    discard_pending_discovery_boundary(active);
    let epoch = active.epoch;
    let phase_id = active.config.phase_id.clone();
    let cutoff_batch_seq = active.last_submitted_batch_seq;
    let sampling = discovery_sampling_stats();
    drop(runtime);

    let (reply_tx, reply_rx) = sync_channel(1);
    worker
        .control_tx
        .send(DiscoveryControlEvent::Flush {
            epoch,
            cutoff_batch_seq,
            sampling,
            reply: reply_tx,
        })
        .map_err(|_| "P4 discovery worker 控制面已退出".to_string())?;
    wait_for_discovery_ack(reply_rx, "Flush", &phase_id, epoch, DISCOVERY_ACK_TIMEOUT)?;

    let mut runtime = DISCOVERY_RUNTIME
        .lock()
        .map_err(|_| "P4 discovery runtime lock poisoned after Flush ACK".to_string())?;
    if runtime.as_ref().is_some_and(|active| active.epoch == epoch) {
        runtime.take();
    }
    log(&format!(
        "P4 相机 discovery phase={phase_id} epoch={epoch} Flush ACK 完成，固定报告已封口"
    ));
    Ok(())
}

fn install_discovery_camera_reference(
    config: &CameraDiscoveryConfig,
    generation: u64,
) -> Result<(), String> {
    clear_discovery_camera_reference();
    let Some(binding) = config.comparison_camera_reference.as_ref() else {
        return Ok(());
    };
    if generation == 0 {
        return Err("camera discovery comparison generation must be non-zero".into());
    }
    let sampler = FalloutCameraSampler::new(
        &config.executable_sha256,
        binding,
        FalloutCapturePurpose::ValidationOnly,
    )?;
    let task = sampler.audit_task();
    let now_qpc = present_qpc()?;
    let verdict = task.audit(generation, now_qpc).map_err(|reason| {
        format!("camera discovery comparison pointer audit failed before Start ACK: {reason}")
    })?;
    crate::fallout_camera_probe::publish_camera_verdict(Ok(verdict));
    POINTER_AUDIT_TASKS
        .lock()
        .map_err(|_| "camera discovery comparison audit task lock poisoned".to_string())?
        .0 = Some(task);
    *CAMERA_SAMPLER
        .lock()
        .map_err(|_| "camera discovery comparison sampler lock poisoned".to_string())? =
        Some(sampler);
    Ok(())
}

fn clear_discovery_camera_reference() {
    if let Ok(mut tasks) = POINTER_AUDIT_TASKS.lock() {
        tasks.0 = None;
    }
    if let Ok(mut sampler) = CAMERA_SAMPLER.lock() {
        sampler.take();
    }
    crate::fallout_camera_probe::clear_camera_verdict();
}

fn discovery_worker() -> Result<&'static DiscoveryWorkerHandle, String> {
    if let Some(worker) = DISCOVERY_WORKER.get() {
        return Ok(worker);
    }
    let _guard = DISCOVERY_WORKER_INIT
        .lock()
        .map_err(|_| "P4 discovery worker init lock poisoned".to_string())?;
    if let Some(worker) = DISCOVERY_WORKER.get() {
        return Ok(worker);
    }
    let (data_tx, data_rx) = sync_channel::<DiscoveryDataEvent>(DISCOVERY_QUEUE_CAPACITY);
    let (control_tx, control_rx) = channel::<DiscoveryControlEvent>();
    std::thread::Builder::new()
        .name("dm-depth-camera".into())
        .spawn(move || scan_loop(control_rx, data_rx))
        .map_err(|error| format!("启动 P4 扫描线程失败: {error}"))?;
    DISCOVERY_WORKER
        .set(DiscoveryWorkerHandle {
            data_tx,
            control_tx,
        })
        .map_err(|_| "P4 discovery worker 初始化竞争".to_string())?;
    DISCOVERY_WORKER
        .get()
        .ok_or_else(|| "P4 discovery worker 未初始化".to_string())
}

fn wait_for_discovery_ack(
    reply: Receiver<Result<(), String>>,
    operation: &str,
    phase_id: &str,
    epoch: u64,
    timeout: Duration,
) -> Result<(), String> {
    match reply.recv_timeout(timeout) {
        Ok(result) => result,
        Err(RecvTimeoutError::Timeout) => Err(format!(
            "P4 discovery {operation} ACK 超时: phase={phase_id} epoch={epoch} timeout_ms={}",
            timeout.as_millis()
        )),
        Err(RecvTimeoutError::Disconnected) => Err(format!(
            "P4 discovery {operation} ACK 通道断开: phase={phase_id} epoch={epoch}"
        )),
    }
}

pub(crate) fn mark_discovery_dsv_ambiguous(_guard: &DiscoveryCallbackGuard) {
    DISCOVERY_DSV_AMBIGUITIES.fetch_add(1, Ordering::Relaxed);
}

/// render callback 入口在运行中丢失或身份无法确认时，由外层 hook 完整性审计记账。
/// 报告会因此带上 `runtime_callback_dropped` blocker，不能拿后续自愈后的样本冒充
/// 这个 phase 全程完整。
pub fn mark_discovery_runtime_callback_dropped() {
    let Some(_guard) = enter_discovery_callback() else {
        return;
    };
    DISCOVERY_RUNTIME_DROPPED.fetch_add(1, Ordering::Relaxed);
}

/// A protected camera hook became unavailable, or an integrity audit found a
/// callback interval whose completeness can no longer be proved. Discovery
/// records the gap in its report; production is stricter and stops the camera
/// stream immediately so it cannot certify partial per-Present evidence.
pub fn mark_runtime_callback_dropped(reason: &str) {
    mark_discovery_runtime_callback_dropped();
    if production_enabled() {
        fail_production(reason);
    }
}

fn reset_discovery_stats() {
    for counter in [
        &DISCOVERY_OBSERVED_PRESENTS,
        &DISCOVERY_SCHEDULED_PRESENTS,
        &DISCOVERY_OBSERVED_DRAWS,
        &DISCOVERY_SCHEDULED_DRAWS,
        &DISCOVERY_SUBMITTED_DRAWS,
        &DISCOVERY_QUEUE_DROPPED,
        &DISCOVERY_RUNTIME_DROPPED,
        &DISCOVERY_BUFFER_OVERFLOWS,
        &DISCOVERY_DSV_AMBIGUITIES,
        &DISCOVERY_BOUNDARY_DISCARDED_PRESENTS,
        &DISCOVERY_BOUNDARY_DISCARDED_DRAWS,
    ] {
        counter.store(0, Ordering::Release);
    }
    QUEUE_FULL_LOGGED.store(false, Ordering::Release);
}

fn discovery_sampling_stats() -> DiscoverySamplingStats {
    DiscoverySamplingStats {
        observed_presents: DISCOVERY_OBSERVED_PRESENTS.load(Ordering::Acquire),
        scheduled_presents: DISCOVERY_SCHEDULED_PRESENTS.load(Ordering::Acquire),
        observed_draws: DISCOVERY_OBSERVED_DRAWS.load(Ordering::Acquire),
        scheduled_draws: DISCOVERY_SCHEDULED_DRAWS.load(Ordering::Acquire),
        submitted_draws: DISCOVERY_SUBMITTED_DRAWS.load(Ordering::Acquire),
        queue_dropped_draws: DISCOVERY_QUEUE_DROPPED.load(Ordering::Acquire),
        runtime_dropped_callbacks: DISCOVERY_RUNTIME_DROPPED.load(Ordering::Acquire),
        tracked_buffer_overflows: DISCOVERY_BUFFER_OVERFLOWS.load(Ordering::Acquire),
        dsv_ambiguities: DISCOVERY_DSV_AMBIGUITIES.load(Ordering::Acquire),
        phase_boundary_discarded_presents: DISCOVERY_BOUNDARY_DISCARDED_PRESENTS
            .load(Ordering::Acquire),
        phase_boundary_discarded_draws: DISCOVERY_BOUNDARY_DISCARDED_DRAWS.load(Ordering::Acquire),
    }
}

/// 启动逐 Present 的 D3D12 production pose 合成。Exact-build CP2077 同时启用正式
/// PlayerPuppet writer；其他 D3D12 binding 保持 camera-only。
pub fn init_d3d12_production(
    output_dir: PathBuf,
    binding: depth_lab::D3d12CameraBinding,
    context: CameraPoseStreamContext,
    actual_executable_sha256: &str,
) -> Result<(), String> {
    init_d3d12_production_inner(output_dir, binding, context, actual_executable_sha256, true)
}

/// Performance baseline and watermark-only stages prove the camera source during warmup, but
/// deliberately stop deep per-Present observation afterwards. Do not create a Player writer for
/// those stages: an enabled writer would correctly reject its intentionally empty stream at Stop.
pub(crate) fn init_d3d12_production_camera_only(
    output_dir: PathBuf,
    binding: depth_lab::D3d12CameraBinding,
    context: CameraPoseStreamContext,
    actual_executable_sha256: &str,
) -> Result<(), String> {
    init_d3d12_production_inner(
        output_dir,
        binding,
        context,
        actual_executable_sha256,
        false,
    )
}

fn init_d3d12_production_inner(
    output_dir: PathBuf,
    binding: depth_lab::D3d12CameraBinding,
    context: CameraPoseStreamContext,
    actual_executable_sha256: &str,
    enable_exact_build_player: bool,
) -> Result<(), String> {
    binding
        .validate_for_executable(actual_executable_sha256)
        .map_err(|e| e.to_string())?;
    let _guard = PRODUCTION_SEND_LOCK
        .lock()
        .map_err(|_| "camera send lock poisoned")?;
    let mut sender = PRODUCTION_TX
        .lock()
        .map_err(|_| "camera sender lock poisoned")?;
    let mut worker = PRODUCTION_JOIN
        .lock()
        .map_err(|_| "camera worker lock poisoned")?;
    let mut root = PRODUCTION_ROOT
        .lock()
        .map_err(|_| "camera root lock poisoned")?;
    let mut error = PRODUCTION_ERROR
        .lock()
        .map_err(|_| "camera error lock poisoned")?;
    let mut observed = OBSERVED_PRODUCTION_PROJECTIONS
        .lock()
        .map_err(|_| "projection cache poisoned")?;
    let mut player_sampler_slot = PLAYER_SAMPLER
        .lock()
        .map_err(|_| "player sampler lock poisoned")?;
    let mut camera_sampler_slot = CAMERA_SAMPLER
        .lock()
        .map_err(|_| "camera sampler lock poisoned")?;
    let mut pointer_audit_tasks = POINTER_AUDIT_TASKS
        .lock()
        .map_err(|_| "pointer audit task lock poisoned")?;
    if sender.is_some() || worker.is_some() || root.is_some() {
        return Err("Camera production previous generation is still active".into());
    }
    let generation = context.identity.generation;
    let identity = context.identity.clone();
    let player_sampler = if enable_exact_build_player
        && actual_executable_sha256 == depth_lab::CP2077_BUILD_20383525_EXE_SHA256
    {
        Some(PlayerSampler::Cp2077(Box::new(Cp2077PlayerSampler::new(
            actual_executable_sha256,
            &binding,
            generation,
            &output_dir,
        )?)))
    } else {
        None
    };
    let player_writer = if let Some(sampler) = player_sampler.as_ref() {
        let mut frequency = 0i64;
        unsafe { windows::Win32::System::Performance::QueryPerformanceFrequency(&mut frequency) }
            .map_err(|error| format!("query player pose QPC frequency failed: {error}"))?;
        if frequency <= 0 {
            return Err("player pose QPC frequency is invalid".into());
        }
        Some(PlayerPoseIndexWriter::new(
            &output_dir,
            context.identity.clone(),
            actual_executable_sha256.to_owned(),
            frequency as u64,
            sampler.source().to_owned(),
        )?)
    } else {
        None
    };
    let player_pose = if player_writer.is_some() {
        PlayerPoseOutput::Enabled
    } else {
        PlayerPoseOutput::Disabled
    };
    let player_audit_task = player_sampler.as_ref().and_then(|sampler| match sampler {
        PlayerSampler::Cp2077(sampler) => Some(sampler.audit_task()),
        _ => None,
    });
    let writer = CameraPoseIndexWriter::new(&output_dir)?;
    let (tx, rx) = sync_channel(PRODUCTION_QUEUE_CAPACITY);
    let join = std::thread::Builder::new()
        .name("dm-camera-production".into())
        .spawn(move || production_loop(rx, None, writer, player_writer, identity))
        .map_err(|e| format!("start camera writer: {e}"))?;
    #[cfg(all(feature = "cp2077-diagnostics", debug_assertions))]
    crate::cp2077_camera_diagnostics::begin(
        &output_dir,
        context.identity.clone(),
        &binding,
        player_sampler.as_ref().and_then(|sampler| match sampler {
            PlayerSampler::Cp2077(sampler) => Some(sampler.diagnostic_addresses()),
            _ => None,
        }),
    );
    *sender = Some(tx);
    *worker = Some(join);
    *root = Some(ProductionOutput {
        root: output_dir,
        player_pose,
    });
    *pointer_audit_tasks = (None, None, player_audit_task);
    crate::fallout_camera_probe::clear_camera_verdict();
    crate::fallout_player_probe::clear_player_verdict();
    crate::cp2077_player_probe::clear_player_verdict();
    *player_sampler_slot = player_sampler;
    *camera_sampler_slot = None;
    *error = None;
    *observed = Some(ObservedProductionProjections {
        generation,
        source: None,
        render_width: binding.render_width,
        render_height: binding.render_height,
        frames: BTreeMap::new(),
    });
    PRODUCTION_BIND_SLOT.store(u32::MAX, Ordering::Release);
    PRODUCTION_VIEW_BIND_SLOT.store(u32::MAX, Ordering::Release);
    PRODUCTION_CAN_HOLD_PROJECTION.store(false, Ordering::Release);
    PRODUCTION_VIEW_IS_HYBRID.store(false, Ordering::Release);
    PRODUCTION_LAST_SUBMITTED_PRESENT.store(u64::MAX, Ordering::Release);
    PRODUCTION_LAST_WRITTEN_PRESENT.store(u64::MAX, Ordering::Release);
    PRODUCTION_WARMUP_PROJECTION_PRESENT.store(u64::MAX, Ordering::Release);
    PRODUCTION_WARMUP_VIEW_PRESENT.store(u64::MAX, Ordering::Release);
    reset_warmup_camera_join();
    PRODUCTION_GENERATION.store(generation, Ordering::Release);
    PRODUCTION_FAILED.store(false, Ordering::Release);
    STALE_PRODUCTION_CALLBACK_LOGGED.store(false, Ordering::Release);
    D3D12_PLAYER_SAMPLING_ENABLED.store(
        matches!(player_pose, PlayerPoseOutput::Enabled),
        Ordering::Release,
    );
    PRODUCTION_ARMED.store(true, Ordering::Release);
    PRODUCTION_ENABLED.store(true, Ordering::Release);
    Ok(())
}

/// The D3D12 backend has already resolved the exact bound draw and checked its source identity.
/// Use the same generation, warmup, bounded writer queue and flush owner as the D3D11 path.
/// Retain a projection only when a depth consumer will retrieve it. A bound
/// camera's real GPU proof copy before Arm or after Seal has no such consumer.
pub fn on_d3d12_pose(
    row: depth_lab::CameraPoseRow,
    retain_depth_projection: bool,
) -> Result<bool, String> {
    let generation = row.identity.generation;
    if !production_enabled() || !production_generation_matches(generation) {
        return Err("stale D3D12 camera generation".into());
    }
    depth_lab::validate_camera_pose_row(&row).map_err(str::to_owned)?;
    let depth_lab::CameraPoseProvenance::ResolvedD3d12CameraHook {
        projection_matrix, ..
    } = &row.provenance
    else {
        return Err("D3D12 camera row has no exact v2 provenance".into());
    };
    let projection = depth_lab::camera_track::projection_from_matrix(projection_matrix)
        .ok_or("D3D12 camera projection is unusable")?;
    let mut projection_guard = OBSERVED_PRODUCTION_PROJECTIONS
        .try_lock()
        .map_err(|_| "D3D12 projection cache is contended")?;
    let observed = projection_guard
        .as_mut()
        .ok_or("D3D12 projection cache is missing")?;
    if observed.generation != generation || observed.source.is_some() {
        return Err("D3D12 projection cache identity mismatch".into());
    }
    if production_armed() {
        if retain_depth_projection && observed.frames.len() >= MAX_OBSERVED_PRODUCTION_PROJECTIONS {
            return Err("D3D12 projection cache full before depth drain".into());
        }
        if retain_depth_projection
            && observed
                .frames
                .insert(row.present_seq, projection)
                .is_some()
        {
            return Err("duplicate D3D12 camera Present".into());
        }
        drop(projection_guard);
        let player = if D3D12_PLAYER_SAMPLING_ENABLED.load(Ordering::Acquire) {
            let _perf = crate::perf::scope(crate::perf::Section::Pose);
            match PLAYER_SAMPLER.try_lock() {
                Ok(mut sampler) => Some(
                    sampler
                        .as_mut()
                        .ok_or_else(|| "D3D12 player sampler is missing".to_string())
                        .and_then(|sampler| {
                            #[cfg(all(feature = "cp2077-diagnostics", debug_assertions))]
                            if matches!(sampler, PlayerSampler::Cp2077(_)) {
                                return crate::cp2077_camera_diagnostics::trace_player_sample(
                                    row.present_seq,
                                    row.present_qpc,
                                    generation,
                                    || sampler.sample_with_camera(&row, generation),
                                );
                            }
                            sampler.sample_with_camera(&row, generation)
                        }),
                ),
                Err(_) => Some(Err("player sampler lock is contended".into())),
            }
        } else {
            None
        };
        if !submit_production(
            generation,
            ProductionEvent::D3d12Pose {
                row: Box::new(row),
                player,
            },
        ) {
            return Err("D3D12 camera writer rejected the frame".into());
        }
        Ok(true)
    } else {
        if D3D12_PLAYER_SAMPLING_ENABLED.load(Ordering::Acquire) {
            let join_error = match PLAYER_SAMPLER.try_lock() {
                Ok(mut sampler) => match sampler.as_mut() {
                    Some(sampler @ PlayerSampler::Cp2077(_)) => {
                        Some(sampler.sample_with_camera(&row, generation).err())
                    }
                    _ => None,
                },
                Err(_) => Some(Some("player sampler lock is contended".into())),
            };
            if let Some(join_error) = join_error
                && let Ok(mut verdict) = PRODUCTION_WARMUP_CAMERA_JOIN.try_lock()
            {
                *verdict = Some((row.present_seq, join_error));
            }
        }
        PRODUCTION_WARMUP_PROJECTION_PRESENT.store(row.present_seq, Ordering::Release);
        PRODUCTION_WARMUP_VIEW_PRESENT.store(row.present_seq, Ordering::Release);
        Ok(false)
    }
}

pub fn init_production(
    output_dir: PathBuf,
    binding: CameraBinding,
    context: CameraPoseStreamContext,
    actual_executable_sha256: &str,
    performance_validation_only: bool,
) -> Result<(), String> {
    let purpose = if performance_validation_only {
        FalloutCapturePurpose::ValidationOnly
    } else {
        FalloutCapturePurpose::Production
    };
    if actual_executable_sha256 == FALLOUT4_1_11_240.executable_sha256
        || binding.game_camera.is_some()
        || binding.profile_id.to_ascii_lowercase().contains("fallout4")
    {
        prepare_fallout_capture(&output_dir, actual_executable_sha256, purpose)?;
    }
    let _send_guard = PRODUCTION_SEND_LOCK
        .lock()
        .map_err(|_| "Camera production send lock poisoned".to_string())?;
    let mut sender = PRODUCTION_TX
        .lock()
        .map_err(|_| "Camera production sender lock poisoned".to_string())?;
    let mut worker = PRODUCTION_JOIN
        .lock()
        .map_err(|_| "Camera production worker lock poisoned".to_string())?;
    let mut production_root = PRODUCTION_ROOT
        .lock()
        .map_err(|_| "Camera production root lock poisoned".to_string())?;
    let mut production_error = PRODUCTION_ERROR
        .lock()
        .map_err(|_| "Camera production error lock poisoned".to_string())?;
    let mut mappings = MAPPINGS
        .lock()
        .map_err(|_| "Camera production mappings lock poisoned".to_string())?;
    if sender.is_some() || worker.is_some() || production_root.is_some() {
        return Err("Camera production previous generation is still active".into());
    }
    let projection = binding
        .projection_sources
        .first()
        .copied()
        .ok_or_else(|| "Camera production projection source is missing".to_string())?;
    let view = binding.view_sources.first().copied();
    let view_source = view.map(|view| view.source);
    if let Some(view_source) = view_source
        && (view_source.bind_slot != projection.bind_slot
            || view_source.buffer_bytes != projection.buffer_bytes)
    {
        return Err(format!(
            "Camera production v1 requires view/projection to share one exact logical VS buffer: view_slot={} projection_slot={} view_bytes={} projection_bytes={}",
            view_source.bind_slot,
            projection.bind_slot,
            view_source.buffer_bytes,
            projection.buffer_bytes
        ));
    }
    *PRODUCTION_VIEW_SOURCE
        .lock()
        .map_err(|_| "Camera production view source lock poisoned".to_string())? = view;
    let generation = context.identity.generation;
    let diagnostic_identity = context.identity.clone();
    *OBSERVED_PRODUCTION_PROJECTIONS
        .lock()
        .map_err(|_| "Camera observed projection cache lock poisoned".to_string())? =
        Some(ObservedProductionProjections {
            generation,
            source: Some(projection),
            render_width: binding.render_width,
            render_height: binding.render_height,
            frames: BTreeMap::new(),
        });
    let camera_sampler = binding
        .game_camera
        .as_ref()
        .map(|game_camera| {
            FalloutCameraSampler::new(actual_executable_sha256, game_camera, purpose)
        })
        .transpose()?;
    // 审计任务与结论槽同步就位：任务给控制线程，旧结论必须清零，
    // 首个有效结论只能来自本 generation 的后台审计。
    if let Ok(mut tasks) = POINTER_AUDIT_TASKS.lock() {
        tasks.0 = camera_sampler
            .as_ref()
            .map(FalloutCameraSampler::audit_task);
        tasks.2 = None;
    }
    crate::fallout_camera_probe::clear_camera_verdict();
    crate::cp2077_player_probe::clear_player_verdict();
    let view_is_hybrid = binding.game_camera.is_some() && view.is_some();
    PRODUCTION_VIEW_IS_HYBRID.store(view_is_hybrid, Ordering::Release);
    let can_hold_projection = binding.game_camera.is_some() && view.is_none();
    let profile_id = binding.profile_id.to_ascii_lowercase();
    let player_sampler = if profile_id.contains("fallout4") {
        let sampler = FalloutPlayerSampler::new(actual_executable_sha256, purpose)?;
        if let Ok(mut tasks) = POINTER_AUDIT_TASKS.lock() {
            tasks.1 = Some(sampler.audit_task());
        }
        Some(PlayerSampler::Fallout(sampler))
    } else if profile_id.contains("bannerlord") {
        if let Ok(mut tasks) = POINTER_AUDIT_TASKS.lock() {
            tasks.1 = None;
        }
        Some(PlayerSampler::Bannerlord(BannerlordPlayerSampler::new(
            actual_executable_sha256,
        )?))
    } else {
        if let Ok(mut tasks) = POINTER_AUDIT_TASKS.lock() {
            tasks.1 = None;
        }
        None
    };
    let player_writer = if let Some(sampler) = player_sampler {
        crate::fallout_player_probe::clear_player_verdict();
        let mut frequency = 0i64;
        unsafe { windows::Win32::System::Performance::QueryPerformanceFrequency(&mut frequency) }
            .map_err(|error| format!("query player pose QPC frequency failed: {error}"))?;
        if frequency <= 0 {
            return Err("player pose QPC frequency is invalid".into());
        }
        *PLAYER_SAMPLER
            .lock()
            .map_err(|_| "player sampler lock poisoned".to_string())? = Some(sampler);
        let source = PLAYER_SAMPLER
            .lock()
            .map_err(|_| "player sampler source lock poisoned".to_string())?
            .as_ref()
            .ok_or("player sampler disappeared during initialization")?
            .source()
            .to_owned();
        Some(PlayerPoseIndexWriter::new(
            &output_dir,
            context.identity.clone(),
            actual_executable_sha256.to_owned(),
            frequency as u64,
            source,
        )?)
    } else {
        if let Ok(mut tasks) = POINTER_AUDIT_TASKS.lock() {
            tasks.1 = None;
        }
        crate::fallout_player_probe::clear_player_verdict();
        None
    };
    let player_pose = if player_writer.is_some() {
        PlayerPoseOutput::Enabled
    } else {
        PlayerPoseOutput::LegacyCount
    };
    let assembler = CameraPoseAssembler::new(&binding, context, actual_executable_sha256)
        .map_err(|error| error.to_string())?;
    let writer = CameraPoseIndexWriter::new(&output_dir)?;
    let (tx, rx) = sync_channel::<ProductionEvent>(PRODUCTION_QUEUE_CAPACITY);
    let join = std::thread::Builder::new()
        .name("dm-camera-pose-writer".into())
        .spawn(move || {
            production_loop(
                rx,
                Some(assembler),
                writer,
                player_writer,
                diagnostic_identity,
            )
        })
        .map_err(|error| format!("启动 Camera pose writer 线程失败: {error}"))?;
    *sender = Some(tx);
    drop(sender);
    *worker = Some(join);
    *production_root = Some(ProductionOutput {
        root: output_dir,
        player_pose,
    });
    *production_error = None;
    *mappings = Some(HashMap::new());
    *CAMERA_SAMPLER
        .lock()
        .map_err(|_| "camera sampler lock poisoned".to_string())? = camera_sampler;
    TIMELINE_PRESENT.store(u64::MAX, Ordering::Release);
    CURRENT_PASS_ORDINAL.store(0, Ordering::Release);
    NEXT_PASS_ORDINAL.store(0, Ordering::Release);
    CURRENT_DRAW_ORDINAL.store(0, Ordering::Release);
    reset_production_draw_identity();
    let source_changed = PRODUCTION_BIND_SLOT.load(Ordering::Acquire) != projection.bind_slot
        || PRODUCTION_VIEW_BIND_SLOT.load(Ordering::Acquire)
            != view_source.map_or(u32::MAX, |source| source.bind_slot)
        || PRODUCTION_PASS_ORDINAL.load(Ordering::Acquire) != projection.pass_ordinal
        || PRODUCTION_DRAW_ORDINAL.load(Ordering::Acquire) != projection.draw_ordinal
        || PRODUCTION_BUFFER_BYTES.load(Ordering::Acquire) != projection.buffer_bytes
        || PRODUCTION_VIEW_BUFFER_BYTES.load(Ordering::Acquire)
            != view_source.map_or(0, |source| source.buffer_bytes);
    let cached_source_buffers = if let Ok(mut known) = KNOWN_PRODUCTION_BUFFERS.lock() {
        if source_changed {
            *known = Some(HashMap::new());
        }
        known
            .as_ref()
            .into_iter()
            .flat_map(|buffers| buffers.iter())
            .map(|(buffer_tag, bytes)| (*buffer_tag, bytes.clone()))
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    if source_changed {
        for tag in &PRODUCTION_SOURCE_TAGS {
            tag.store(0, Ordering::Release);
        }
        PRODUCTION_SOURCE_TAG_BLOOM.store(0, Ordering::Release);
        PRODUCTION_DIRTY_SOURCE_MASK.store(0, Ordering::Release);
    }
    if let Ok(mut pending) = PENDING_PRODUCTION_BUFFERS.lock() {
        *pending = Some(HashMap::new());
    }
    PENDING_PRODUCTION_PRESENT.store(u64::MAX, Ordering::Release);
    PRODUCTION_BIND_SLOT.store(projection.bind_slot, Ordering::Release);
    PRODUCTION_VIEW_BIND_SLOT.store(
        view_source.map_or(u32::MAX, |source| source.bind_slot),
        Ordering::Release,
    );
    PRODUCTION_PASS_ORDINAL.store(projection.pass_ordinal, Ordering::Release);
    PRODUCTION_DRAW_ORDINAL.store(projection.draw_ordinal, Ordering::Release);
    PRODUCTION_BUFFER_BYTES.store(projection.buffer_bytes, Ordering::Release);
    PRODUCTION_VIEW_BUFFER_BYTES.store(
        view_source.map_or(0, |source| source.buffer_bytes),
        Ordering::Release,
    );
    PRODUCTION_CAN_HOLD_PROJECTION.store(can_hold_projection, Ordering::Release);
    PRODUCTION_LAST_SUBMITTED_PRESENT.store(u64::MAX, Ordering::Release);
    PRODUCTION_LAST_WRITTEN_PRESENT.store(u64::MAX, Ordering::Release);
    PRODUCTION_WARMUP_PROJECTION_PRESENT.store(u64::MAX, Ordering::Release);
    PRODUCTION_WARMUP_VIEW_PRESENT.store(u64::MAX, Ordering::Release);
    reset_warmup_camera_join();
    STALE_PRODUCTION_CALLBACK_LOGGED.store(false, Ordering::Release);
    PRODUCTION_GENERATION.store(generation, Ordering::Release);
    PRODUCTION_FAILED.store(false, Ordering::Release);
    PRODUCTION_TARGET_DRAW_LOGGED.store(false, Ordering::Release);
    D3D12_PLAYER_SAMPLING_ENABLED.store(false, Ordering::Release);
    for (buffer_tag, bytes) in cached_source_buffers {
        if !submit_production_locked(
            generation,
            ProductionEvent::BufferUpdate { buffer_tag, bytes },
        ) {
            return Err("failed to seed Camera production from the previous generation".into());
        }
    }
    PRODUCTION_ARMED.store(true, Ordering::Release);
    PRODUCTION_ENABLED.store(true, Ordering::Release);
    log("Camera production 已启用：逐 Present 写 camera-pose-index/v1");
    Ok(())
}

#[derive(Debug, Clone, Default)]
pub struct CameraProductionSummary {
    pub pose_row_count: u64,
    pub player_pose_row_count: Option<u64>,
    pub diagnostic_row_count: u64,
    pub last_present_seq: Option<u64>,
    pub errors: Vec<String>,
}

#[must_use]
pub fn last_submitted_present_seq() -> Option<u64> {
    let value = PRODUCTION_LAST_SUBMITTED_PRESENT.load(Ordering::Acquire);
    (value != u64::MAX).then_some(value)
}

/// Keep production hooks/caches live while preventing formal Camera rows from being emitted.
/// The lifecycle owner calls this before exposing the generation to render callbacks.
pub fn begin_production_warmup(generation: u64) -> Result<(), String> {
    let _guard = PRODUCTION_SEND_LOCK
        .lock()
        .map_err(|_| "Camera production warmup lock poisoned".to_string())?;
    if !production_generation_matches(generation) || !production_enabled() {
        return Err(format!(
            "Camera production warmup generation mismatch: requested={generation} active={}",
            PRODUCTION_GENERATION.load(Ordering::Acquire)
        ));
    }
    if PRODUCTION_LAST_SUBMITTED_PRESENT.load(Ordering::Acquire) != u64::MAX {
        return Err("Camera production cannot enter warmup after formal Present output".into());
    }
    PRODUCTION_ARMED.store(false, Ordering::Release);
    PRODUCTION_WARMUP_PROJECTION_PRESENT.store(u64::MAX, Ordering::Release);
    PRODUCTION_WARMUP_VIEW_PRESENT.store(u64::MAX, Ordering::Release);
    reset_warmup_camera_join();
    TIMELINE_PRESENT.store(u64::MAX, Ordering::Release);
    CURRENT_PASS_ORDINAL.store(0, Ordering::Release);
    NEXT_PASS_ORDINAL.store(0, Ordering::Release);
    CURRENT_DRAW_ORDINAL.store(0, Ordering::Release);
    reset_production_draw_identity();
    Ok(())
}

/// Atomically turn the warmed generation into the formal Camera timeline.
pub fn arm_production(generation: u64) -> Result<(), String> {
    let _guard = PRODUCTION_SEND_LOCK
        .lock()
        .map_err(|_| "Camera production arm lock poisoned".to_string())?;
    if !production_generation_matches(generation) || !production_enabled() {
        return Err(format!(
            "Camera production arm generation mismatch: requested={generation} active={}",
            PRODUCTION_GENERATION.load(Ordering::Acquire)
        ));
    }
    let mut observed = OBSERVED_PRODUCTION_PROJECTIONS
        .lock()
        .map_err(|_| "Camera observed projection cache lock poisoned during arm".to_string())?;
    let observed = observed
        .as_mut()
        .ok_or_else(|| "Camera observed projection cache is not initialized".to_string())?;
    if observed.generation != generation {
        return Err(format!(
            "Camera observed projection generation mismatch during arm: requested={generation} active={}",
            observed.generation
        ));
    }
    // Warmup observations prove readiness only. Formal depth rows may consume only projections
    // observed after this boundary.
    observed.frames.clear();
    TIMELINE_PRESENT.store(u64::MAX, Ordering::Release);
    CURRENT_PASS_ORDINAL.store(0, Ordering::Release);
    NEXT_PASS_ORDINAL.store(0, Ordering::Release);
    CURRENT_DRAW_ORDINAL.store(0, Ordering::Release);
    reset_production_draw_identity();
    PRODUCTION_WARMUP_PROJECTION_PRESENT.store(u64::MAX, Ordering::Release);
    PRODUCTION_WARMUP_VIEW_PRESENT.store(u64::MAX, Ordering::Release);
    reset_warmup_camera_join();
    PRODUCTION_ARMED.store(true, Ordering::Release);
    log("Camera production warmup 完成：正式逐 Present 输出已开启");
    Ok(())
}

/// Stop accepting render callbacks before the probe publishes `Stopped`.
pub fn begin_stop_production() {
    let Ok(_guard) = PRODUCTION_SEND_LOCK.lock() else {
        fail_production("Camera production stop lock poisoned");
        return;
    };
    PRODUCTION_ARMED.store(false, Ordering::Release);
    PRODUCTION_ENABLED.store(false, Ordering::Release);
    PRODUCTION_GENERATION.store(0, Ordering::Release);
    PRODUCTION_CAN_HOLD_PROJECTION.store(false, Ordering::Release);
    PRODUCTION_VIEW_IS_HYBRID.store(false, Ordering::Release);
    D3D12_PLAYER_SAMPLING_ENABLED.store(false, Ordering::Release);
    if let Ok(mut sender) = PRODUCTION_TX.lock() {
        sender.take();
    } else {
        fail_production("Camera production sender lock poisoned during stop");
    }
    if let Ok(mut sampler) = PLAYER_SAMPLER.lock() {
        sampler.take();
    } else {
        fail_production("player sampler lock poisoned during stop");
    }
    if let Ok(mut sampler) = CAMERA_SAMPLER.lock() {
        sampler.take();
    } else {
        fail_production("camera sampler lock poisoned during stop");
    }
    // Stop 边界：审计任务与结论一并清空。审计与本函数同在控制线程串行执行，
    // 因此不存在「Stop 之后迟到写入」的窗口；渲染线程侧还有 generation 校验兜底。
    if let Ok(mut tasks) = POINTER_AUDIT_TASKS.lock() {
        *tasks = (None, None, None);
    }
    crate::fallout_camera_probe::clear_camera_verdict();
    crate::fallout_player_probe::clear_player_verdict();
    crate::cp2077_player_probe::clear_player_verdict();
}

/// Join the writer and count closed files. Counts returned here are stable:
/// no later callback retains a sender capable of modifying this generation.
pub fn finish_stop_production() -> CameraProductionSummary {
    begin_stop_production();
    #[cfg(all(feature = "cp2077-diagnostics", debug_assertions))]
    crate::cp2077_camera_diagnostics::finish();
    let mut summary = CameraProductionSummary::default();
    match PRODUCTION_JOIN.lock() {
        Ok(mut worker) => {
            if let Some(join) = worker.take()
                && join.join().is_err()
            {
                summary
                    .errors
                    .push("Camera production writer panicked".into());
            }
        }
        Err(_) => summary
            .errors
            .push("Camera production worker lock poisoned".into()),
    }
    if let Ok(mut error) = PRODUCTION_ERROR.lock()
        && let Some(error) = error.take()
    {
        summary.errors.push(error);
    }
    if let Ok(mut root) = PRODUCTION_ROOT.lock()
        && let Some(output) = root.take()
    {
        let root = output.root;
        match count_lines(&root.join(crate::camera_writer::CAMERA_POSE_INDEX_FILE)) {
            Ok(count) => summary.pose_row_count = count,
            Err(error) => summary.errors.push(error),
        }
        match count_lines(&root.join(crate::camera_writer::CAMERA_POSE_DIAGNOSTIC_FILE)) {
            Ok(count) => summary.diagnostic_row_count = count,
            Err(error) => summary.errors.push(error),
        }
        let player_path = root.join(crate::player_writer::PLAYER_POSE_INDEX_FILE);
        if !matches!(output.player_pose, PlayerPoseOutput::Disabled) {
            // Some(0) is an enabled but empty stream and must still fail coverage.
            // D3D12 camera-only generations explicitly omit this stream.
            summary.player_pose_row_count = Some(0);
            if matches!(output.player_pose, PlayerPoseOutput::Enabled) || player_path.is_file() {
                match count_lines(&player_path) {
                    Ok(count) => summary.player_pose_row_count = Some(count),
                    Err(error) => summary.errors.push(error),
                }
            }
        }
    }
    let last_present_seq = PRODUCTION_LAST_WRITTEN_PRESENT.load(Ordering::Acquire);
    summary.last_present_seq = (last_present_seq != u64::MAX).then_some(last_present_seq);
    match OBSERVED_PRODUCTION_PROJECTIONS.lock() {
        Ok(mut observed) => observed.take(),
        Err(_) => {
            summary
                .errors
                .push("Camera observed projection cache lock poisoned during stop".into());
            None
        }
    };
    match PRODUCTION_VIEW_SOURCE.lock() {
        Ok(mut view) => view.take(),
        Err(_) => {
            summary
                .errors
                .push("Camera production view source lock poisoned during stop".into());
            None
        }
    };
    summary
}

fn count_lines(path: &std::path::Path) -> Result<u64, String> {
    use std::io::BufRead;
    let file = std::fs::File::open(path)
        .map_err(|error| format!("open {} for row count failed: {error}", path.display()))?;
    let mut count = 0u64;
    for line in std::io::BufReader::new(file).lines() {
        line.map_err(|error| format!("read {} for row count failed: {error}", path.display()))?;
        count = count
            .checked_add(1)
            .ok_or_else(|| format!("row count overflow for {}", path.display()))?;
    }
    Ok(count)
}

fn production_loop(
    rx: Receiver<ProductionEvent>,
    mut assembler: Option<CameraPoseAssembler>,
    mut writer: CameraPoseIndexWriter,
    mut player_writer: Option<PlayerPoseIndexWriter>,
    diagnostic_identity: StreamIdentity,
) {
    crate::probe::set_current_thread_lossless_writer_priority();
    let mut latest_buffers: HashMap<u64, Vec<u8>> = HashMap::new();
    let mut vs_slots: BTreeMap<u32, u64> = BTreeMap::new();
    let mut main_scene_present: Option<u64> = None;
    let mut frame_diagnostic: Option<(u64, CameraPoseDiagnosticReason)> = None;

    'production: while let Ok(event) = rx.recv() {
        if PRODUCTION_FAILED.load(Ordering::Acquire) {
            break;
        }
        if let ProductionEvent::D3d12Pose { row, player } = event {
            if assembler.is_some() || row.identity != diagnostic_identity {
                fail_production("D3D12 pose arrived in a different backend/run");
                break;
            }
            if let Some(player_writer) = player_writer.as_mut() {
                match player {
                    Some(Ok(sample))
                        if sample.present_seq == row.present_seq
                            && sample.present_qpc == row.present_qpc =>
                    {
                        if let Err(error) = player_writer.push(sample) {
                            fail_production(&error);
                            break;
                        }
                    }
                    Some(Ok(_)) => {
                        fail_production("D3D12 player pose identity disagrees with camera Present");
                        break;
                    }
                    Some(Err(error)) => {
                        fail_production(&format!(
                            "player pose sample {} failed: {error}",
                            row.present_seq
                        ));
                        break;
                    }
                    None => {
                        fail_production("D3D12 production Present omitted the player sample");
                        break;
                    }
                }
            } else if player.is_some() {
                fail_production("D3D12 camera-only run received an unexpected player sample");
                break;
            }
            if let Err(error) = writer.write_row(&row) {
                fail_production(&error);
                break;
            }
            PRODUCTION_LAST_WRITTEN_PRESENT.store(row.present_seq, Ordering::Release);
            continue;
        }
        let Some(assembler) = assembler.as_mut() else {
            fail_production("D3D11 event arrived in a D3D12 camera run");
            break;
        };
        let result = match event {
            ProductionEvent::D3d12Pose { .. } => unreachable!("handled above"),
            ProductionEvent::BufferUpdate { buffer_tag, bytes } => {
                if !latest_buffers.contains_key(&buffer_tag)
                    && latest_buffers.len() >= MAX_TRACKED_PRODUCTION_BUFFERS
                {
                    Err("production 常量缓冲身份超过硬上限，拒绝无界缓存".into())
                } else {
                    latest_buffers.insert(buffer_tag, bytes);
                    Ok(())
                }
            }
            ProductionEvent::VsBind { bind_slot, buffer } => {
                if let Some(buffer_tag) = buffer {
                    vs_slots.insert(bind_slot, buffer_tag);
                } else {
                    vs_slots.remove(&bind_slot);
                }
                Ok(())
            }
            ProductionEvent::MainScenePass { present_seq } => {
                if main_scene_present.is_some_and(|active| active != present_seq) {
                    Err("主场景 DSV pass 跨过 Present 边界".into())
                } else {
                    main_scene_present = Some(present_seq);
                    Ok(())
                }
            }
            ProductionEvent::Draw {
                present_seq,
                sample_qpc,
                pass_ordinal,
                draw_ordinal,
            } => {
                let bound_buffers: Vec<_> = vs_slots
                    .iter()
                    .filter_map(|(&bind_slot, &buffer_tag)| {
                        latest_buffers
                            .get(&buffer_tag)
                            .map(|bytes| BoundBufferAtDraw {
                                shader_stage: ShaderStage::Vertex,
                                bind_slot,
                                buffer_tag,
                                bytes,
                            })
                    })
                    .collect();
                let outcome = assembler.observe_draw(CameraDrawSample {
                    present_seq,
                    sample_qpc,
                    pass_ordinal,
                    draw_ordinal,
                    bound_buffers: &bound_buffers,
                });
                match outcome {
                    Ok(()) => Ok(()),
                    Err(error) => match diagnostic_reason(&error) {
                        Some(reason) => {
                            frame_diagnostic = Some((present_seq, reason));
                            Ok(())
                        }
                        None => Err(error.to_string()),
                    },
                }
            }
            ProductionEvent::Present {
                present_seq,
                projection_evidence_frame,
                camera,
                player,
            } => {
                if let Some(player_writer) = player_writer.as_mut() {
                    match player {
                        Some(Ok(sample)) => {
                            if let Err(error) = player_writer.push(sample) {
                                fail_production(&error);
                                break 'production;
                            }
                        }
                        Some(Err(error)) => {
                            fail_production(&format!(
                                "player pose sample {present_seq} failed: {error}"
                            ));
                            break 'production;
                        }
                        None => {
                            fail_production("production Present omitted the player sample");
                            break 'production;
                        }
                    }
                }
                let observed_main_scene = match main_scene_present.take() {
                    Some(observed) if observed == present_seq => true,
                    Some(observed) => {
                        fail_production(&format!(
                            "主场景 DSV seq {observed} 与 Present {present_seq} 不一致"
                        ));
                        break 'production;
                    }
                    None => false,
                };
                let recorded_diagnostic = frame_diagnostic.take();
                let result = if !projection_evidence_frame {
                    if observed_main_scene || recorded_diagnostic.is_some() {
                        fail_production(
                            "non-evidence Present received DSV/draw production bookkeeping",
                        );
                        break 'production;
                    }
                    match camera {
                        Some(Ok(sample)) => {
                            if sample.present_seq != present_seq {
                                fail_production("Fallout camera sample seq disagrees with Present");
                                break 'production;
                            }
                            match assembler.finish_present_game_telemetry_with_held_projection(
                                present_seq,
                                sample.present_qpc,
                                sample.camera_position,
                                sample.camera_rotation_quaternion,
                            ) {
                                Ok(Some(row)) => writer.write_row(&row),
                                Ok(None) => write_absence(
                                    assembler,
                                    &mut writer,
                                    &diagnostic_identity,
                                    present_seq,
                                    CameraPoseDiagnosticReason::BindingSampleMissing,
                                ),
                                Err(error) => match diagnostic_reason(&error) {
                                    Some(reason) => write_absence(
                                        assembler,
                                        &mut writer,
                                        &diagnostic_identity,
                                        present_seq,
                                        reason,
                                    ),
                                    None => Err(error.to_string()),
                                },
                            }
                        }
                        Some(Err(error)) => {
                            fail_production(&format!(
                                "Fallout camera pose sample {present_seq} failed: {error}"
                            ));
                            break 'production;
                        }
                        None => Err(
                            "non-evidence production Present requires direct game camera sampling"
                                .into(),
                        ),
                    }
                } else if !observed_main_scene {
                    write_absence(
                        assembler,
                        &mut writer,
                        &diagnostic_identity,
                        present_seq,
                        CameraPoseDiagnosticReason::NoMainSceneDepth,
                    )
                } else if let Some((diagnostic_seq, reason)) = recorded_diagnostic {
                    if diagnostic_seq != present_seq {
                        Err(format!(
                            "Camera 诊断 seq {diagnostic_seq} 与 Present {present_seq} 不一致"
                        ))
                    } else {
                        write_absence(
                            assembler,
                            &mut writer,
                            &diagnostic_identity,
                            present_seq,
                            reason,
                        )
                    }
                } else {
                    let assembled = match camera {
                        Some(Ok(sample)) => {
                            if sample.present_seq != present_seq {
                                fail_production("Fallout camera sample seq disagrees with Present");
                                break 'production;
                            }
                            assembler.finish_present_game_telemetry(
                                present_seq,
                                sample.present_qpc,
                                sample.camera_position,
                                sample.camera_rotation_quaternion,
                            )
                        }
                        Some(Err(error)) => {
                            fail_production(&format!(
                                "Fallout camera pose sample {present_seq} failed: {error}"
                            ));
                            break 'production;
                        }
                        None => assembler.finish_present(present_seq),
                    };
                    match assembled {
                        Ok(Some(row)) => writer.write_row(&row),
                        Ok(None) => write_absence(
                            assembler,
                            &mut writer,
                            &diagnostic_identity,
                            present_seq,
                            CameraPoseDiagnosticReason::BindingSampleMissing,
                        ),
                        Err(error) => match diagnostic_reason(&error) {
                            Some(reason) => write_absence(
                                assembler,
                                &mut writer,
                                &diagnostic_identity,
                                present_seq,
                                reason,
                            ),
                            None => Err(error.to_string()),
                        },
                    }
                };
                if result.is_ok() {
                    PRODUCTION_LAST_WRITTEN_PRESENT.store(present_seq, Ordering::Release);
                }
                result
            }
        };
        if let Err(error) = result {
            fail_production(&error);
            break;
        }
    }
    if let Err(error) = writer.finish() {
        fail_production(&error);
    }
    if let Some(player_writer) = player_writer
        && let Err(error) = player_writer.finish()
    {
        fail_production(&error);
    }
}

fn diagnostic_reason(error: &CameraPoseAssemblyError) -> Option<CameraPoseDiagnosticReason> {
    match error {
        CameraPoseAssemblyError::MissingView { .. }
        | CameraPoseAssemblyError::MissingProjection { .. }
        | CameraPoseAssemblyError::InvalidSampleQpc { .. } => {
            Some(CameraPoseDiagnosticReason::BindingSampleMissing)
        }
        CameraPoseAssemblyError::AmbiguousMatrixSource { .. }
        | CameraPoseAssemblyError::MatrixChangedWithinPresent { .. } => {
            Some(CameraPoseDiagnosticReason::BindingAmbiguity)
        }
        CameraPoseAssemblyError::InvalidViewMatrix { .. }
        | CameraPoseAssemblyError::InvalidProjectionMatrix { .. } => {
            Some(CameraPoseDiagnosticReason::MatrixInvalid)
        }
        CameraPoseAssemblyError::Binding(_)
        | CameraPoseAssemblyError::InvalidStreamIdentity { .. }
        | CameraPoseAssemblyError::DuplicatePresentSeq { .. }
        | CameraPoseAssemblyError::PresentSeqOutOfOrder { .. }
        | CameraPoseAssemblyError::PresentSeqGap { .. }
        | CameraPoseAssemblyError::MissingPresentBoundary { .. }
        | CameraPoseAssemblyError::BoundaryMismatch { .. } => None,
    }
}

fn write_absence(
    assembler: &mut CameraPoseAssembler,
    writer: &mut CameraPoseIndexWriter,
    identity: &StreamIdentity,
    present_seq: u64,
    reason: CameraPoseDiagnosticReason,
) -> Result<(), String> {
    assembler
        .skip_present(present_seq)
        .map_err(|error| error.to_string())?;
    writer.write_diagnostic(&CameraPoseDiagnosticRow {
        schema: CAMERA_POSE_DIAGNOSTIC_SCHEMA_V1.into(),
        identity: identity.clone(),
        present_seq,
        reason,
    })
}

fn fail_production(reason: &str) {
    // Failure is exceptional and the stream is already being stopped, so taking this tiny lock is
    // preferable to losing the root cause. Holding it while publishing PRODUCTION_FAILED also
    // guarantees the control thread cannot observe `failed=true` before the reason is retained.
    let mut retained_error = PRODUCTION_ERROR.lock().ok();
    if !PRODUCTION_FAILED.swap(true, Ordering::AcqRel) {
        PRODUCTION_ARMED.store(false, Ordering::Release);
        PRODUCTION_ENABLED.store(false, Ordering::Release);
        if let Some(error) = retained_error.as_mut() {
            **error = Some(reason.to_string());
        }
        log(&format!(
            "Camera production 已 fail-closed，停止 pose 输出: {reason}"
        ));
    }
}

pub fn mark_production_hook_integrity_failure(reason: &str) {
    if production_enabled() {
        fail_production(reason);
    }
}

struct DiscoveryPhase {
    epoch: u64,
    output_dir: PathBuf,
    config: CameraDiscoveryConfig,
    tracker: CameraDiscoveryTracker,
    last_processed_batch_seq: u64,
    last_sampling: Option<DiscoverySamplingStats>,
    last_checkpoint: Instant,
    failure: Option<String>,
}

/// 后台线程：每个 phase 独立扫描、统计并落盘。
fn scan_loop(control_rx: Receiver<DiscoveryControlEvent>, data_rx: Receiver<DiscoveryDataEvent>) {
    crate::probe::set_current_thread_background();
    let mut phase: Option<DiscoveryPhase> = None;
    loop {
        match control_rx.try_recv() {
            Ok(DiscoveryControlEvent::Start {
                epoch,
                output_dir,
                config,
                selected_dsv_descriptor,
                reply,
            }) => {
                let result = start_discovery_phase(
                    &mut phase,
                    epoch,
                    output_dir,
                    *config,
                    selected_dsv_descriptor,
                );
                let _ = reply.send(result);
                continue;
            }
            Ok(DiscoveryControlEvent::Flush {
                epoch,
                cutoff_batch_seq,
                sampling,
                reply,
            }) => {
                let result =
                    flush_discovery_phase(&mut phase, &data_rx, epoch, cutoff_batch_seq, sampling);
                let _ = reply.send(result);
                continue;
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => break,
        }

        match data_rx.recv_timeout(DISCOVERY_WORKER_POLL_INTERVAL) {
            Ok(event) => process_discovery_batch(&mut phase, event),
            Err(RecvTimeoutError::Timeout) => maybe_dump_discovery_checkpoint(&mut phase),
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    if let Some(active) = phase.as_ref() {
        log(&format!(
            "P4 discovery worker 在 phase={} epoch={} 未 Flush 时退出；固定报告未封口",
            active.config.phase_id, active.epoch
        ));
    }
}

fn start_discovery_phase(
    phase: &mut Option<DiscoveryPhase>,
    epoch: u64,
    output_dir: PathBuf,
    config: CameraDiscoveryConfig,
    selected_dsv_descriptor: Option<depth_lab::DsvDiscoveryDescriptor>,
) -> Result<(), String> {
    if let Some(active) = phase.as_ref() {
        return Err(format!(
            "P4 discovery worker 仍持有 phase={} epoch={}，拒绝 Start phase={} epoch={epoch}",
            active.config.phase_id, active.epoch, config.phase_id
        ));
    }
    let sealed = output_dir.join(format!("camera-candidates-{}.json", config.phase_id));
    if sealed.exists() {
        return Err(format!(
            "P4 discovery 固定 phase 报告已存在，拒绝覆盖: {}",
            sealed.display()
        ));
    }
    let tracker = match (config.dsv_selection.as_ref(), selected_dsv_descriptor) {
        (Some(_), Some(descriptor)) => {
            CameraDiscoveryTracker::for_validated_dsv_selection(&config, descriptor)
        }
        (None, None) => CameraDiscoveryTracker::for_config(&config),
        _ => {
            return Err(
                "camera discovery selection binding and replayed descriptor must be present together"
                    .into(),
            );
        }
    };
    *phase = Some(DiscoveryPhase {
        epoch,
        output_dir,
        config,
        tracker,
        last_processed_batch_seq: 0,
        last_sampling: None,
        last_checkpoint: Instant::now(),
        failure: None,
    });
    Ok(())
}

fn process_discovery_batch(phase: &mut Option<DiscoveryPhase>, event: DiscoveryDataEvent) {
    let Some(active) = phase.as_mut() else {
        return;
    };
    if active.epoch != event.epoch {
        active.failure.get_or_insert_with(|| {
            format!(
                "stale batch epoch entered active phase: active={} actual={} batch_seq={}",
                active.epoch, event.epoch, event.batch_seq
            )
        });
        return;
    }
    let expected = active.last_processed_batch_seq.saturating_add(1);
    if event.batch_seq != expected {
        active.failure.get_or_insert_with(|| {
            format!(
                "batch sequence mismatch: expected={expected} actual={}",
                event.batch_seq
            )
        });
        return;
    }
    let logical_draws = event.batch.logical_draws;
    let binding_snapshots = event.batch.bindings.len();
    let started = Instant::now();
    active.tracker.observe_batch(&event.batch);
    let elapsed = started.elapsed();
    active.last_processed_batch_seq = event.batch_seq;
    active.last_sampling = Some(event.sampling);
    if elapsed >= DISCOVERY_SLOW_BATCH_THRESHOLD {
        log(&format!(
            "P4 discovery slow Present batch: phase={} epoch={} batch_seq={} logical_draws={} binding_snapshots={} elapsed_ms={}",
            active.config.phase_id,
            active.epoch,
            event.batch_seq,
            logical_draws,
            binding_snapshots,
            elapsed.as_millis()
        ));
    }
}

fn maybe_dump_discovery_checkpoint(phase: &mut Option<DiscoveryPhase>) {
    let Some(active) = phase.as_mut() else {
        return;
    };
    if active.last_checkpoint.elapsed() < DISCOVERY_CHECKPOINT_INTERVAL {
        return;
    }
    let Some(sampling) = active.last_sampling else {
        return;
    };
    let started = Instant::now();
    match dump_discovery_checkpoint(active, sampling) {
        Ok(report_bytes) => log(&format!(
            "P4 phase mutable checkpoint 已在数据队列空闲时落盘: phase={} epoch={} processed_batch_seq={} report_bytes={} elapsed_ms={}",
            active.config.phase_id,
            active.epoch,
            active.last_processed_batch_seq,
            report_bytes,
            started.elapsed().as_millis()
        )),
        Err(error) => log(&format!("P4 phase mutable checkpoint 落盘失败: {error}")),
    }
    active.last_checkpoint = Instant::now();
}

fn flush_discovery_phase(
    phase: &mut Option<DiscoveryPhase>,
    data_rx: &Receiver<DiscoveryDataEvent>,
    epoch: u64,
    cutoff_batch_seq: u64,
    sampling: DiscoverySamplingStats,
) -> Result<(), String> {
    let Some(active) = phase.as_ref() else {
        return Err(format!(
            "P4 discovery Flush 找不到活动 phase: epoch={epoch} cutoff_batch_seq={cutoff_batch_seq}"
        ));
    };
    if active.epoch != epoch {
        return Err(format!(
            "P4 discovery Flush epoch 不匹配: active={} request={epoch}",
            active.epoch
        ));
    }

    let deadline = Instant::now() + DISCOVERY_DRAIN_TIMEOUT;
    while phase
        .as_ref()
        .is_some_and(|active| active.last_processed_batch_seq < cutoff_batch_seq)
    {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            let processed = phase
                .as_ref()
                .map_or(0, |active| active.last_processed_batch_seq);
            return Err(format!(
                "P4 discovery Flush drain 超时: epoch={epoch} processed_batch_seq={processed} cutoff_batch_seq={cutoff_batch_seq} timeout_ms={}",
                DISCOVERY_DRAIN_TIMEOUT.as_millis()
            ));
        };
        match data_rx.recv_timeout(remaining.min(DISCOVERY_WORKER_POLL_INTERVAL)) {
            Ok(event) => process_discovery_batch(phase, event),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                return Err(format!(
                    "P4 discovery Flush drain 数据面断开: epoch={epoch} cutoff_batch_seq={cutoff_batch_seq}"
                ));
            }
        }
    }

    let active = phase.as_ref().expect("phase checked above");
    if let Some(error) = active.failure.as_deref() {
        return Err(format!(
            "P4 discovery worker phase={} epoch={epoch} 已 fail-closed: {error}",
            active.config.phase_id
        ));
    }
    dump_discovery_final(active, sampling)?;
    phase.take();
    Ok(())
}

fn dump_discovery_checkpoint(
    phase: &DiscoveryPhase,
    sampling: DiscoverySamplingStats,
) -> Result<usize, String> {
    std::fs::create_dir_all(&phase.output_dir)
        .map_err(|e| format!("建 {} 失败: {e}", phase.output_dir.display()))?;
    let report = phase.tracker.report(&phase.config, sampling);
    report.validate_dsv_selection_binding()?;
    write_discovery_progress(&phase.output_dir, &report)?;
    let serialized = serialize_discovery_report(&report)?;
    let report_bytes = serialized.len();
    write_mutable_discovery_report(&phase.output_dir, &serialized)?;
    Ok(report_bytes)
}

fn dump_discovery_final(
    phase: &DiscoveryPhase,
    sampling: DiscoverySamplingStats,
) -> Result<(), String> {
    std::fs::create_dir_all(&phase.output_dir)
        .map_err(|e| format!("建 {} 失败: {e}", phase.output_dir.display()))?;
    let report = phase.tracker.report(&phase.config, sampling);
    report.validate_dsv_selection_binding()?;
    write_discovery_progress(&phase.output_dir, &report)?;
    let serialized = serialize_discovery_report(&report)?;
    write_mutable_discovery_report(&phase.output_dir, &serialized)?;
    let name = format!("camera-candidates-{}.json", phase.config.phase_id);
    let path = phase.output_dir.join(&name);
    if path.exists() {
        return Err(format!(
            "固定 phase 报告已存在，拒绝覆盖: {}",
            path.display()
        ));
    }
    let tmp = phase
        .output_dir
        .join(format!(".{name}.epoch-{}.tmp", phase.epoch));
    write_discovery_report_file(&tmp, &serialized)?;
    if path.exists() {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!(
            "固定 phase 报告已存在，拒绝覆盖: {}",
            path.display()
        ));
    }
    std::fs::rename(&tmp, &path)
        .map_err(|error| format!("封口 {} 失败: {error}", path.display()))?;
    write_sealed_discovery_progress(&phase.output_dir, &phase.config.phase_id, &report)?;
    Ok(())
}

fn write_mutable_discovery_report(
    output_dir: &std::path::Path,
    serialized: &[u8],
) -> Result<(), String> {
    let path = output_dir.join("camera-candidates.json");
    let tmp = output_dir.join("camera-candidates.json.tmp");
    write_discovery_report_file(&tmp, serialized)?;
    if path.exists() {
        std::fs::remove_file(&path).map_err(|error| {
            format!("替换 {} 前删除旧 checkpoint 失败: {error}", path.display())
        })?;
    }
    std::fs::rename(&tmp, &path).map_err(|error| format!("更新 {} 失败: {error}", path.display()))
}

fn write_discovery_progress(
    output_dir: &std::path::Path,
    report: &depth_lab::CameraDiscoveryReport,
) -> Result<(), String> {
    let progress = depth_lab::CameraDiscoveryProgress::from(report);
    let serialized = serde_json::to_vec_pretty(&progress)
        .map_err(|error| format!("序列化 Camera discovery progress 失败: {error}"))?;
    let path = output_dir.join("camera-discovery-progress.json");
    let tmp = output_dir.join("camera-discovery-progress.json.tmp");
    let mut file = std::fs::File::create(&tmp)
        .map_err(|error| format!("建 {} 失败: {error}", tmp.display()))?;
    file.write_all(&serialized)
        .map_err(|error| format!("写 {} 失败: {error}", tmp.display()))?;
    file.sync_all()
        .map_err(|error| format!("sync {} 失败: {error}", tmp.display()))?;
    if path.exists() {
        std::fs::remove_file(&path)
            .map_err(|error| format!("替换 {} 前删除旧 progress 失败: {error}", path.display()))?;
    }
    std::fs::rename(&tmp, &path).map_err(|error| format!("更新 {} 失败: {error}", path.display()))
}

fn write_sealed_discovery_progress(
    output_dir: &std::path::Path,
    phase_id: &str,
    report: &depth_lab::CameraDiscoveryReport,
) -> Result<(), String> {
    let progress = depth_lab::CameraDiscoveryProgress::from(report);
    let serialized = serde_json::to_vec_pretty(&progress)
        .map_err(|error| format!("序列化 sealed Camera discovery progress 失败: {error}"))?;
    let name = format!("camera-discovery-progress-{phase_id}.json");
    let path = output_dir.join(&name);
    if path.exists() {
        return Err(format!(
            "固定 phase progress 已存在，拒绝覆盖: {}",
            path.display()
        ));
    }
    let tmp = output_dir.join(format!(".{name}.tmp"));
    let mut file = std::fs::File::create(&tmp)
        .map_err(|error| format!("建 {} 失败: {error}", tmp.display()))?;
    file.write_all(&serialized)
        .map_err(|error| format!("写 {} 失败: {error}", tmp.display()))?;
    file.sync_all()
        .map_err(|error| format!("sync {} 失败: {error}", tmp.display()))?;
    if path.exists() {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!(
            "固定 phase progress 已存在，拒绝覆盖: {}",
            path.display()
        ));
    }
    std::fs::rename(&tmp, &path).map_err(|error| format!("封口 {} 失败: {error}", path.display()))
}

fn serialize_discovery_report(
    report: &depth_lab::CameraDiscoveryReport,
) -> Result<Vec<u8>, String> {
    let mut serialized = Vec::with_capacity(DISCOVERY_REPORT_BUFFER_BYTES);
    serde_json::to_writer_pretty(&mut serialized, report)
        .map_err(|error| format!("序列化 Camera discovery 报告失败: {error}"))?;
    Ok(serialized)
}

fn write_discovery_report_file(path: &std::path::Path, serialized: &[u8]) -> Result<(), String> {
    let file = std::fs::File::create(path)
        .map_err(|error| format!("建 {} 失败: {error}", path.display()))?;
    let mut writer = BufWriter::with_capacity(DISCOVERY_REPORT_BUFFER_BYTES, file);
    writer
        .write_all(serialized)
        .map_err(|error| format!("写 {} 失败: {error}", path.display()))?;
    writer
        .flush()
        .map_err(|error| format!("flush {} 失败: {error}", path.display()))?;
    writer
        .get_ref()
        .sync_all()
        .map_err(|error| format!("sync {} 失败: {error}", path.display()))
}

/// `Map` hook 调用：记下映射地址，等 `Unmap` 时读。
///
/// `should_sample` 由调用方判断（是否常量缓冲、主场景 pass 是否活跃）——
/// 那些判断要用 D3D 接口，放在 hook 那边更直接。
///
/// # Safety
/// `ptr` 必须是刚才 `Map` 成功返回的地址，且在配对的 `Unmap` 之前保持有效。
pub unsafe fn on_map(
    generation: u64,
    resource: u64,
    ptr: *mut u8,
    len: usize,
    present_seq: u64,
    should_sample: bool,
) {
    let discovery_guard = enter_discovery_callback();
    if !should_sample || ptr.is_null() || len == 0 {
        return;
    }
    let _generation_guard = if production_enabled() {
        let Ok(guard) = PRODUCTION_SEND_LOCK.try_lock() else {
            if production_generation_matches(generation) {
                fail_production("Camera Map callback raced with StopRun");
            } else {
                log_stale_production_callback(generation);
            }
            return;
        };
        if !production_generation_matches(generation) {
            log_stale_production_callback(generation);
            return;
        }
        Some(guard)
    } else {
        None
    };
    if let Ok(mut g) = MAPPINGS.try_lock()
        && let Some(map) = g.as_mut()
    {
        map.insert(
            resource,
            Mapping {
                ptr,
                len: len.min(MAX_BUFFER_BYTES),
                present_seq,
                generation,
                discovery_epoch: discovery_guard.as_ref().map(|guard| guard.epoch),
            },
        );
    }
    // try_lock 失败就放弃这一次采样。**绝不在渲染线程上等锁**——
    // 后台线程持锁时阻塞渲染线程正是 #461 修掉的那类优先级反转。
}

/// `Unmap` hook 调用：把写入的字节抄一份丢给后台线程。
///
/// 必须在**调用原始 `Unmap` 之前**读——`Unmap` 之后那块内存就不属于我们了。
///
/// # Safety
/// 必须在原始 `Unmap` **之前**调用，且 `resource` 是配对的那次 `Map` 的资源。
pub unsafe fn on_unmap(generation: u64, resource: u64) {
    let discovery_guard = enter_discovery_callback();
    let Some(mapping) = ({
        match MAPPINGS.try_lock() {
            Ok(mut g) => g.as_mut().and_then(|m| {
                m.get(&resource)
                    .is_some_and(|mapping| mapping.generation == generation)
                    .then(|| m.remove(&resource))
                    .flatten()
            }),
            Err(_) => None,
        }
    }) else {
        return;
    };
    let discovery_guard = discovery_guard.filter(|guard| {
        mapping
            .discovery_epoch
            .is_some_and(|epoch| epoch == guard.epoch)
    });
    // SAFETY: 指针来自本进程刚才成功返回的 `Map`，且尚未 `Unmap`。
    let bytes = unsafe { std::slice::from_raw_parts(mapping.ptr, mapping.len) };
    submit(
        generation,
        resource,
        mapping.present_seq,
        discovery_guard.as_ref(),
        bytes,
    );
}

/// `UpdateSubresource` hook 调用：数据指针直接给了，立刻抄走。
///
/// # Safety
/// `data` 必须至少 `len` 字节可读——这是 D3D11 对调用方的契约。
pub unsafe fn on_update_subresource(
    generation: u64,
    resource: u64,
    data: *const u8,
    len: usize,
    present_seq: u64,
    should_sample: bool,
) {
    let discovery_guard = enter_discovery_callback();
    if !should_sample || data.is_null() || len == 0 {
        return;
    }
    // SAFETY: 调用方保证 `data` 至少 `len` 字节可读（D3D11 的契约要求如此）。
    let bytes = unsafe { std::slice::from_raw_parts(data, len.min(MAX_BUFFER_BYTES)) };
    submit(
        generation,
        resource,
        present_seq,
        discovery_guard.as_ref(),
        bytes,
    );
}

fn submit(
    generation: u64,
    buffer_tag: u64,
    present_seq: u64,
    discovery_guard: Option<&DiscoveryCallbackGuard>,
    bytes: &[u8],
) {
    // 装不下一个 4×4 矩阵的缓冲连扫都不用扫
    if bytes.len() < 64 {
        return;
    }
    if let Some(discovery_guard) = discovery_guard {
        match DISCOVERY_RUNTIME.try_lock() {
            Ok(mut runtime) => {
                if let Some(runtime) = runtime.as_mut() {
                    if runtime.epoch != discovery_guard.epoch {
                        DISCOVERY_RUNTIME_DROPPED.fetch_add(1, Ordering::Relaxed);
                        return;
                    }
                    // 未绑定资源只保留第一次写入，既覆盖常见的“先写后绑 + 静态 buffer”，
                    // 又避免同一个无关 buffer 的数百万次更新重新变成 memcpy 洪水。资源一旦
                    // 进入 VS slot，后续每次更新都会替换为最新字节。
                    let is_bound = runtime
                        .vs_slots
                        .values()
                        .any(|&bound_tag| bound_tag == buffer_tag);
                    // The phase's exact-draw D3D11.1 range snapshot is stronger evidence than a
                    // later generic Map/Update callback. Large ring buffers are intentionally
                    // truncated on that generic path, so replacing the range would lose both its
                    // absolute offset and the bytes actually visible to the shader.
                    if runtime
                        .latest_buffers
                        .get(&buffer_tag)
                        .is_some_and(|buffer| buffer.cold_seed)
                    {
                        return;
                    }
                    if !is_bound && runtime.latest_buffers.contains_key(&buffer_tag) {
                        return;
                    }
                    if !runtime.latest_buffers.contains_key(&buffer_tag)
                        && runtime.latest_buffers.len() >= MAX_TRACKED_DISCOVERY_BUFFERS
                    {
                        DISCOVERY_BUFFER_OVERFLOWS.fetch_add(1, Ordering::Relaxed);
                    } else {
                        let Some(version) = runtime.next_buffer_version.checked_add(1) else {
                            DISCOVERY_BUFFER_OVERFLOWS.fetch_add(1, Ordering::Relaxed);
                            return;
                        };
                        runtime.next_buffer_version = version;
                        runtime.latest_buffers.insert(
                            buffer_tag,
                            VersionedDiscoveryBuffer {
                                version,
                                cold_seed: false,
                                bytes: Arc::from(bytes),
                            },
                        );
                    }
                }
            }
            Err(_) => {
                DISCOVERY_RUNTIME_DROPPED.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    let production_buffer_length = [
        PRODUCTION_BUFFER_BYTES.load(Ordering::Acquire),
        PRODUCTION_VIEW_BUFFER_BYTES.load(Ordering::Acquire),
    ]
    .into_iter()
    .any(|expected| expected != 0 && bytes.len() == expected as usize);
    if production_enabled() && production_buffer_length {
        if let Ok(mut pending) = PENDING_PRODUCTION_BUFFERS.try_lock()
            && let Some(pending) = pending.as_mut()
        {
            if PENDING_PRODUCTION_PRESENT.load(Ordering::Acquire) != present_seq {
                pending.clear();
                PENDING_PRODUCTION_PRESENT.store(present_seq, Ordering::Release);
            }
            if pending.len() >= MAX_PENDING_PRODUCTION_BUFFERS
                && !pending.contains_key(&buffer_tag)
                && let Some(evicted) = pending.keys().next().copied()
            {
                pending.remove(&evicted);
            }
            pending.insert(buffer_tag, bytes.to_vec());
        }
        if production_source_tag_known(buffer_tag) {
            if let Ok(mut known) = KNOWN_PRODUCTION_BUFFERS.try_lock()
                && let Some(known) = known.as_mut()
            {
                known.insert(buffer_tag, bytes.to_vec());
                clear_production_source_tag_dirty(buffer_tag);
            }
            let _ = submit_production(
                generation,
                ProductionEvent::BufferUpdate {
                    buffer_tag,
                    bytes: bytes.to_vec(),
                },
            );
        }
    }
}

fn cached_production_buffer(buffer_tag: u64, present_seq: u64) -> Option<Vec<u8>> {
    if PENDING_PRODUCTION_PRESENT.load(Ordering::Acquire) != present_seq {
        return None;
    }
    PENDING_PRODUCTION_BUFFERS
        .try_lock()
        .ok()
        .and_then(|pending| pending.as_ref()?.get(&buffer_tag).cloned())
}

/// Return CPU-upload bytes proven to belong to this exact Present and resource.
///
/// The production draw must never fall back to `KNOWN_PRODUCTION_BUFFERS`: that cache is useful
/// for discovery and writer bookkeeping, but it may describe an earlier Present. Fallout 4 updates
/// the render-camera constant buffer through Map/Unmap (or UpdateSubresource) before the audited
/// draw, so the upload hook gives stronger evidence without forcing a synchronous GPU Map on every
/// game Present.
pub fn exact_production_buffer_for_present(
    buffer_tag: u64,
    present_seq: u64,
    expected_bytes: usize,
) -> Result<Vec<u8>, String> {
    if !production_enabled() {
        return Err("Camera production is not enabled".into());
    }
    let observed_present = PENDING_PRODUCTION_PRESENT.load(Ordering::Acquire);
    if observed_present != present_seq {
        return Err(format!(
            "exact production CPU upload is missing for Present {present_seq}: latest_upload_present={observed_present}"
        ));
    }
    let pending = PENDING_PRODUCTION_BUFFERS
        .try_lock()
        .map_err(|_| "exact production CPU upload cache is contended".to_string())?;
    let pending = pending
        .as_ref()
        .ok_or_else(|| "exact production CPU upload cache is not initialized".to_string())?;
    let bytes = pending.get(&buffer_tag).ok_or_else(|| {
        format!(
            "exact production CPU upload is missing for Present {present_seq} resource={buffer_tag:#x}"
        )
    })?;
    if bytes.len() != expected_bytes {
        return Err(format!(
            "exact production CPU upload byte length mismatch: expected={expected_bytes} observed={}",
            bytes.len()
        ));
    }
    Ok(bytes.clone())
}

fn known_production_buffer(buffer_tag: u64) -> Option<Vec<u8>> {
    if production_source_tag_dirty(buffer_tag) {
        return None;
    }
    KNOWN_PRODUCTION_BUFFERS
        .try_lock()
        .ok()
        .and_then(|known| known.as_ref()?.get(&buffer_tag).cloned())
}

/// Return bytes that were actually captured from this exact D3D11 resource.
///
/// A constant-buffer binding may persist across StartRun generations, so the tag-keyed `known`
/// cache is intentionally retained. Bytes from a different tag are never interchangeable: doing
/// so would turn a missing observation into false `observed_this_frame` evidence when a ring buffer
/// advances to a resource whose update callback was not seen.
#[cfg(test)]
fn observed_production_buffer(buffer_tag: u64, present_seq: u64) -> Option<Vec<u8>> {
    known_production_buffer(buffer_tag)
        .or_else(|| cached_production_buffer(buffer_tag, present_seq))
}

fn record_observed_production_projection(
    generation: u64,
    present_seq: u64,
    bytes: &[u8],
) -> Result<bool, String> {
    let mut observed = OBSERVED_PRODUCTION_PROJECTIONS
        .try_lock()
        .map_err(|_| "Camera observed projection cache is contended".to_string())?;
    let observed = observed
        .as_mut()
        .ok_or_else(|| "Camera observed projection cache is not initialized".to_string())?;
    if observed.generation != generation {
        return Err(format!(
            "Camera observed projection generation mismatch: callback={generation} active={}",
            observed.generation
        ));
    }
    let Some(projection) = projection_from_bound_buffer(
        observed
            .source
            .ok_or("v1 projection bytes sent to a D3D12 camera")?,
        bytes,
        observed.render_width,
        observed.render_height,
    ) else {
        // CameraPoseAssembler will write the frame-level matrix diagnostic. A sampled depth frame
        // that needs this projection fails closed later when it cannot claim observed evidence.
        return Ok(false);
    };
    if let Some(previous) = observed.frames.get(&present_seq) {
        if previous != &projection {
            return Err(format!(
                "Camera projection changed within Present {present_seq}: previous={previous:?} incoming={projection:?}"
            ));
        }
        return Ok(true);
    }
    while observed.frames.len() >= MAX_OBSERVED_PRODUCTION_PROJECTIONS {
        let Some(oldest) = observed.frames.keys().next().copied() else {
            break;
        };
        observed.frames.remove(&oldest);
    }
    observed.frames.insert(present_seq, projection);
    Ok(true)
}

/// Consume the exact constant-buffer projection observed for one depth frame.
///
/// The cache remains generation-addressable after `begin_stop_production`: GPU readbacks issued
/// before StopRun may complete while the camera sender is already closed.
pub fn take_observed_projection(
    generation: u64,
    present_seq: u64,
) -> Result<Option<Projection>, String> {
    let mut observed = OBSERVED_PRODUCTION_PROJECTIONS.try_lock().map_err(|_| {
        "Camera observed projection cache is contended during depth drain".to_string()
    })?;
    let observed = observed
        .as_mut()
        .ok_or_else(|| "Camera observed projection cache is not initialized".to_string())?;
    if observed.generation != generation {
        return Err(format!(
            "Camera observed projection generation mismatch during depth drain: requested={generation} active={}",
            observed.generation
        ));
    }
    Ok(observed.frames.remove(&present_seq))
}

/// Make a missing/drifting per-frame projection visible in the durable Flush summary.
pub fn fail_depth_projection(message: &str) {
    fail_production(&format!("depth projection evidence failed: {message}"));
}

/// Return the one bound constant buffer that still needs a cold-start byte snapshot.
///
/// Production normally learns bytes from Map/Unmap or UpdateSubresource, and
/// [`on_vs_constant_buffers`] can seed from [`cached_production_buffer`] when those bytes
/// were seen at least once. Neither path covers an **immutable** buffer whose last upload
/// predates the hook: nothing ever writes it again, so the cache stays empty and the
/// projection never gets bytes. This is the only case that needs a GPU read-back.
///
/// Returns `(offset, buffer_tag, expected_bytes)`. The caller may snapshot only this
/// already-pinned source; arbitrary buffers are never read back or guessed.
///
/// **Takes `buffer_at` rather than a slice.** #665 removed the per-call `Vec` from the
/// `VSSetConstantBuffers` hook; rebuilding one here just to ask this question would put
/// the allocation straight back into the hot path. The closure reads a single slot.
pub fn production_buffer_snapshot_request<F>(
    start_slot: u32,
    num_buffers: u32,
    buffer_at: F,
) -> Option<(u32, u64, usize)>
where
    F: Fn(u32) -> Option<u64>,
{
    if !production_enabled() {
        return None;
    }
    let projection_slot = PRODUCTION_BIND_SLOT.load(Ordering::Acquire);
    let targets = [
        (
            projection_slot,
            PRODUCTION_BUFFER_BYTES.load(Ordering::Acquire),
        ),
        (
            PRODUCTION_VIEW_BIND_SLOT.load(Ordering::Acquire),
            PRODUCTION_VIEW_BUFFER_BYTES.load(Ordering::Acquire),
        ),
    ];
    for (index, (target_slot, expected_bytes)) in targets.into_iter().enumerate() {
        if target_slot == u32::MAX || (index == 1 && target_slot == projection_slot) {
            continue;
        }
        let Some(offset) = target_slot.checked_sub(start_slot) else {
            continue;
        };
        if offset >= num_buffers {
            continue;
        }
        let Some(buffer_tag) = buffer_at(offset) else {
            continue;
        };
        if !production_source_tag_known(buffer_tag) || known_production_buffer(buffer_tag).is_some()
        {
            continue;
        }
        let expected_bytes = usize::try_from(expected_bytes).ok()?;
        if expected_bytes >= 64 {
            return Some((offset, buffer_tag, expected_bytes));
        }
    }
    None
}

/// Return the exact byte size pinned to one active production VS slot.
///
/// D3D11.1 may bind a small logical range from a much larger ring buffer. The render hook uses
/// this size to read back that exact range at the configured draw instead of treating the backing
/// resource size or address as the shader-visible buffer identity. Production v1 intentionally
/// permits only one shared view/projection slot, so a different slot is never guessed.
#[must_use]
pub fn production_buffer_bytes_for_slot(bind_slot: u32) -> Option<usize> {
    if !production_enabled() {
        return None;
    }
    if bind_slot != PRODUCTION_BIND_SLOT.load(Ordering::Acquire) {
        return None;
    }
    usize::try_from(PRODUCTION_BUFFER_BYTES.load(Ordering::Acquire))
        .ok()
        .filter(|bytes| *bytes >= 64)
}

/// Feed a one-time, read-only snapshot of the pinned production constant buffer into the same
/// bounded cache and writer queue used by ordinary CPU uploads.
pub fn on_production_buffer_snapshot(generation: u64, buffer_tag: u64, bytes: &[u8]) {
    let valid_length = [
        PRODUCTION_BUFFER_BYTES.load(Ordering::Acquire),
        PRODUCTION_VIEW_BUFFER_BYTES.load(Ordering::Acquire),
    ]
    .into_iter()
    .any(|expected| expected != 0 && bytes.len() == expected as usize);
    if !valid_length || !production_source_tag_known(buffer_tag) {
        return;
    }
    submit(generation, buffer_tag, u64::MAX, None, bytes);
}

fn production_source_tag_index(buffer_tag: u64) -> Option<usize> {
    if buffer_tag == 0
        || PRODUCTION_SOURCE_TAG_BLOOM.load(Ordering::Acquire)
            & production_source_tag_bloom_bit(buffer_tag)
            == 0
    {
        return None;
    }
    PRODUCTION_SOURCE_TAGS
        .iter()
        .position(|tag| tag.load(Ordering::Acquire) == buffer_tag)
}

fn production_source_tag_bloom_bit(buffer_tag: u64) -> u64 {
    let mixed = buffer_tag ^ buffer_tag.rotate_right(17) ^ (buffer_tag >> 32);
    1u64 << (mixed & 63)
}

fn production_source_tag_known(buffer_tag: u64) -> bool {
    production_source_tag_index(buffer_tag).is_some()
}

fn production_source_tag_dirty(buffer_tag: u64) -> bool {
    production_source_tag_index(buffer_tag).is_some_and(|index| {
        PRODUCTION_DIRTY_SOURCE_MASK.load(Ordering::Acquire) & (1u64 << index) != 0
    })
}

fn clear_production_source_tag_dirty(buffer_tag: u64) {
    if let Some(index) = production_source_tag_index(buffer_tag) {
        PRODUCTION_DIRTY_SOURCE_MASK.fetch_and(!(1u64 << index), Ordering::AcqRel);
    }
}

/// Map/Update hook 的非证据帧先做一次原子 bloom 检查；只有可能命中时才核对最多 16
/// 个 source tag。若恰好写到已验证 source，就让旧字节失效。这里不查询 COM、不复制
/// buffer，也不排队事件。
pub fn mark_production_buffer_write(buffer_tag: u64) {
    if let Some(index) = production_source_tag_index(buffer_tag) {
        PRODUCTION_DIRTY_SOURCE_MASK.fetch_or(1u64 << index, Ordering::AcqRel);
    }
}

fn remember_production_source_tag(buffer_tag: u64) -> bool {
    if buffer_tag == 0 {
        return false;
    }
    if production_source_tag_known(buffer_tag) {
        return true;
    }
    for (index, tag) in PRODUCTION_SOURCE_TAGS.iter().enumerate() {
        if tag
            .compare_exchange(0, buffer_tag, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            PRODUCTION_SOURCE_TAG_BLOOM.fetch_or(
                production_source_tag_bloom_bit(buffer_tag),
                Ordering::AcqRel,
            );
            PRODUCTION_DIRTY_SOURCE_MASK.fetch_or(1u64 << index, Ordering::AcqRel);
            return true;
        }
        if tag.load(Ordering::Acquire) == buffer_tag {
            return true;
        }
    }
    fail_production("production projection source resource identities exceeded the hard limit");
    false
}

fn update_discovery_vs_slots<F>(
    discovery_guard: &DiscoveryCallbackGuard,
    start_slot: u32,
    num_buffers: u32,
    buffer_at: &F,
) where
    F: Fn(u32) -> Option<u64>,
{
    let Ok(mut runtime) = DISCOVERY_RUNTIME.try_lock() else {
        DISCOVERY_RUNTIME_DROPPED.fetch_add(1, Ordering::Relaxed);
        return;
    };
    let Some(runtime) = runtime.as_mut() else {
        return;
    };
    if runtime.epoch != discovery_guard.epoch {
        DISCOVERY_RUNTIME_DROPPED.fetch_add(1, Ordering::Relaxed);
        return;
    }
    for offset in 0..num_buffers {
        let Some(slot) = start_slot.checked_add(offset) else {
            DISCOVERY_RUNTIME_DROPPED.fetch_add(1, Ordering::Relaxed);
            return;
        };
        if let Some(buffer_tag) = buffer_at(offset) {
            runtime.vs_slots.insert(slot, buffer_tag);
        } else {
            runtime.vs_slots.remove(&slot);
        }
    }
}

fn record_discovery_draw(
    discovery_guard: &DiscoveryCallbackGuard,
    present_seq: u64,
    pass_ordinal: u32,
    draw_ordinal: u32,
    sample_viewport: impl FnOnce() -> Option<depth_lab::camera_discovery::DiscoveryViewport>,
) {
    DISCOVERY_OBSERVED_DRAWS.fetch_add(1, Ordering::Relaxed);
    if DISCOVERY_SCHEDULED_PRESENT_SEQ.load(Ordering::Acquire) != present_seq {
        return;
    }
    let Ok(mut runtime) = DISCOVERY_RUNTIME.try_lock() else {
        DISCOVERY_RUNTIME_DROPPED.fetch_add(1, Ordering::Relaxed);
        return;
    };
    let Some(runtime) = runtime.as_mut() else {
        return;
    };
    if runtime.epoch != discovery_guard.epoch {
        DISCOVERY_RUNTIME_DROPPED.fetch_add(1, Ordering::Relaxed);
        return;
    }
    if runtime.scheduled_present_seq != Some(present_seq)
        || pass_ordinal >= runtime.config.max_main_passes_per_present
        || draw_ordinal < runtime.config.draw_window_start
        || draw_ordinal
            >= runtime
                .config
                .draw_window_start
                .saturating_add(runtime.config.draw_window_count)
    {
        return;
    }
    if runtime
        .pending_batch
        .as_ref()
        .is_some_and(|batch| batch.present_seq != present_seq)
    {
        submit_pending_discovery_batch(runtime);
    }
    DISCOVERY_SCHEDULED_DRAWS.fetch_add(1, Ordering::Relaxed);
    let batch = runtime
        .pending_batch
        .get_or_insert_with(|| PendingDiscoveryBatch::new(present_seq));
    batch
        .draw_contexts
        .push(depth_lab::camera_discovery::DiscoveryDrawContext {
            present_seq,
            pass_ordinal,
            draw_ordinal,
            viewport: sample_viewport(),
        });
    let overflows = batch.record_draw(
        pass_ordinal,
        draw_ordinal,
        &runtime.vs_slots,
        &runtime.latest_buffers,
    );
    if overflows != 0 {
        DISCOVERY_BUFFER_OVERFLOWS.fetch_add(overflows, Ordering::Relaxed);
    }
}

fn submit_discovery_present(
    discovery_guard: &DiscoveryCallbackGuard,
    generation: u64,
    present_seq: u64,
) {
    let Ok(mut runtime) = DISCOVERY_RUNTIME.try_lock() else {
        DISCOVERY_RUNTIME_DROPPED.fetch_add(1, Ordering::Relaxed);
        return;
    };
    let Some(runtime) = runtime.as_mut() else {
        return;
    };
    if runtime.epoch != discovery_guard.epoch {
        DISCOVERY_RUNTIME_DROPPED.fetch_add(1, Ordering::Relaxed);
        return;
    }
    if runtime
        .pending_batch
        .as_ref()
        .is_some_and(|batch| batch.present_seq == present_seq)
        && runtime.config.comparison_camera_reference.is_some()
    {
        let sample = match present_qpc() {
            Ok(present_qpc) => match CAMERA_SAMPLER.try_lock() {
                Ok(mut sampler) => match sampler.as_mut() {
                    Some(sampler) => sampler.sample(present_seq, present_qpc, generation),
                    None => Err("camera discovery comparison sampler is missing".into()),
                },
                Err(_) => Err("camera discovery comparison sampler lock is contended".into()),
            },
            Err(error) => Err(error),
        };
        if let Some(batch) = runtime.pending_batch.as_mut() {
            match sample {
                Ok(sample) => {
                    batch.comparison_camera_reference = Some(DiscoveryReferenceCameraObservation {
                        present_seq: sample.present_seq,
                        present_qpc: sample.present_qpc,
                        camera_position: sample.camera_position,
                        camera_rotation_quaternion: sample.camera_rotation_quaternion,
                    });
                }
                Err(error) => batch.comparison_camera_error = Some(error),
            }
        }
    }
    if runtime
        .pending_batch
        .as_ref()
        .is_some_and(|batch| batch.present_seq == present_seq)
    {
        submit_pending_discovery_batch(runtime);
    }
}

fn submit_pending_discovery_batch(runtime: &mut DiscoveryRuntime) {
    let Some(worker) = DISCOVERY_WORKER.get() else {
        if let Some(pending) = runtime.pending_batch.take() {
            DISCOVERY_RUNTIME_DROPPED.fetch_add(pending.logical_draws, Ordering::Relaxed);
        }
        return;
    };
    submit_pending_discovery_batch_to(runtime, &worker.data_tx);
}

/// Exclude a sampled Present that was interrupted by a config-driven phase boundary.
///
/// A phase switch can quiesce after its Draw callbacks completed but before the same Present
/// reached [`submit_discovery_present`]. Submitting that half batch would attach no comparison
/// camera sample and, more importantly, would claim evidence for a Present that never crossed the
/// phase boundary. Keep the scheduled accounting visible, but classify the one possible pending
/// batch separately instead of silently dropping it or poisoning the sealed phase.
fn discard_pending_discovery_boundary(runtime: &mut DiscoveryRuntime) {
    let Some(pending) = runtime.pending_batch.take() else {
        return;
    };
    if pending.logical_draws == 0 {
        return;
    }
    DISCOVERY_BOUNDARY_DISCARDED_PRESENTS.fetch_add(1, Ordering::Relaxed);
    DISCOVERY_BOUNDARY_DISCARDED_DRAWS.fetch_add(pending.logical_draws, Ordering::Relaxed);
    log(&format!(
        "P4 discovery phase={} excluded incomplete boundary Present {} ({} draws)",
        runtime.config.phase_id, pending.present_seq, pending.logical_draws
    ));
}

fn submit_pending_discovery_batch_to(
    runtime: &mut DiscoveryRuntime,
    data_tx: &SyncSender<DiscoveryDataEvent>,
) {
    let Some(pending) = runtime.pending_batch.take() else {
        return;
    };
    if pending.logical_draws == 0 {
        return;
    }
    let logical_draws = pending.logical_draws;
    let batch_seq = runtime.last_submitted_batch_seq.saturating_add(1);
    let mut sampling = discovery_sampling_stats();
    sampling.submitted_draws = sampling.submitted_draws.saturating_add(logical_draws);
    let event = DiscoveryDataEvent {
        epoch: runtime.epoch,
        batch_seq,
        batch: pending.into_batch(),
        sampling,
    };
    match data_tx.try_send(event) {
        Ok(()) => {
            DISCOVERY_SUBMITTED_DRAWS.fetch_add(logical_draws, Ordering::Relaxed);
            runtime.last_submitted_batch_seq = batch_seq;
        }
        Err(TrySendError::Full(_)) => {
            DISCOVERY_QUEUE_DROPPED.fetch_add(logical_draws, Ordering::Relaxed);
            if !QUEUE_FULL_LOGGED.swap(true, Ordering::Relaxed) {
                log(
                    "P4 discovery Present batch 队列已满；当前 phase 已 fail-closed，不得裁决 binding",
                );
            }
        }
        Err(TrySendError::Disconnected(_)) => {
            DISCOVERY_RUNTIME_DROPPED.fetch_add(logical_draws, Ordering::Relaxed);
            DISCOVERY_ENABLED.store(false, Ordering::Release);
            log("P4 discovery worker 数据面已退出，停止采样");
        }
    }
}

fn submit_production(generation: u64, event: ProductionEvent) -> bool {
    let Ok(_send_guard) = PRODUCTION_SEND_LOCK.try_lock() else {
        if production_generation_matches(generation) {
            fail_production("late Camera callback raced with StopRun");
        } else {
            log_stale_production_callback(generation);
        }
        return false;
    };
    submit_production_locked(generation, event)
}

fn submit_production_locked(generation: u64, event: ProductionEvent) -> bool {
    if !production_generation_matches(generation) {
        log_stale_production_callback(generation);
        return false;
    }
    let submitted_present_seq = match &event {
        ProductionEvent::Present { present_seq, .. } => Some(*present_seq),
        ProductionEvent::D3d12Pose { row, .. } => Some(row.present_seq),
        _ => None,
    };
    let Ok(sender) = PRODUCTION_TX.try_lock() else {
        fail_production("production sender lock is busy");
        return false;
    };
    let Some(tx) = sender.as_ref() else {
        fail_production("production event channel 尚未初始化");
        return false;
    };
    match tx.try_send(event) {
        Ok(()) => {
            if let Some(present_seq) = submitted_present_seq {
                PRODUCTION_LAST_SUBMITTED_PRESENT.store(present_seq, Ordering::Release);
            }
            true
        }
        Err(TrySendError::Full(_)) => {
            fail_production("production event queue 已满，严格模式禁止丢样本");
            false
        }
        Err(TrySendError::Disconnected(_)) => {
            fail_production("production writer 线程已退出");
            false
        }
    }
}

fn production_generation_matches(generation: u64) -> bool {
    generation != 0 && PRODUCTION_GENERATION.load(Ordering::Acquire) == generation
}

fn log_stale_production_callback(generation: u64) {
    if !STALE_PRODUCTION_CALLBACK_LOGGED.swap(true, Ordering::AcqRel) {
        log(&format!(
            "discarded stale Camera callback generation={generation} active={}",
            PRODUCTION_GENERATION.load(Ordering::Acquire)
        ));
    }
}

fn reset_production_draw_identity() {
    PRODUCTION_EXACT_DRAW_PRESENT.store(u64::MAX, Ordering::Release);
    PRODUCTION_EXACT_DRAW_PASS.store(u32::MAX, Ordering::Release);
    PRODUCTION_EXACT_DRAW_ORDINAL.store(u32::MAX, Ordering::Release);
    PRODUCTION_PROJECTION_OBSERVED_PRESENT.store(u64::MAX, Ordering::Release);
}

fn record_exact_production_draw(present_seq: u64, pass_ordinal: u32, draw_ordinal: u32) {
    PRODUCTION_EXACT_DRAW_PASS.store(pass_ordinal, Ordering::Relaxed);
    PRODUCTION_EXACT_DRAW_ORDINAL.store(draw_ordinal, Ordering::Relaxed);
    PRODUCTION_EXACT_DRAW_PRESENT.store(present_seq, Ordering::Release);
}

fn mark_production_projection_observed(present_seq: u64) {
    PRODUCTION_PROJECTION_OBSERVED_PRESENT.store(present_seq, Ordering::Release);
}

/// Whether the configured exact draw has already supplied this Present's projection bytes.
///
/// Render hooks use this only to close the remaining Camera deep-accounting window. DSV ranking
/// and snapshot identity continue through the end of the frame, while later writes to a known
/// source still mark its cached bytes dirty in the cheap hook prefix.
#[must_use]
pub fn production_projection_observed_this_frame(present_seq: u64) -> bool {
    production_enabled()
        && PRODUCTION_PROJECTION_OBSERVED_PRESENT.load(Ordering::Acquire) == present_seq
}

/// Return the pass instance that would be snapshotted if the selected DSV were unbound now.
///
/// Snapshot hooks call this only on an evidence frame. The double read of `TIMELINE_PRESENT`
/// prevents a future Present reset from being paired with ordinals from the previous one.
#[must_use]
pub fn current_main_pass_identity(present_seq: u64) -> Option<MainPassIdentity> {
    if TIMELINE_PRESENT.load(Ordering::Acquire) != present_seq {
        return None;
    }
    let pass_ordinal = CURRENT_PASS_ORDINAL.load(Ordering::Acquire);
    let exact_draw_ordinal = (PRODUCTION_EXACT_DRAW_PRESENT.load(Ordering::Acquire) == present_seq
        && PRODUCTION_EXACT_DRAW_PASS.load(Ordering::Acquire) == pass_ordinal)
        .then(|| PRODUCTION_EXACT_DRAW_ORDINAL.load(Ordering::Acquire));
    let projection_observed = exact_draw_ordinal.is_some()
        && PRODUCTION_PROJECTION_OBSERVED_PRESENT.load(Ordering::Acquire) == present_seq;
    (TIMELINE_PRESENT.load(Ordering::Acquire) == present_seq).then_some(MainPassIdentity {
        pass_ordinal,
        exact_draw_ordinal,
        projection_observed,
    })
}

#[must_use]
pub fn exact_production_draw_identity(present_seq: u64) -> Option<ExactProductionDrawIdentity> {
    if PRODUCTION_EXACT_DRAW_PRESENT.load(Ordering::Acquire) != present_seq {
        return None;
    }
    let pass_ordinal = PRODUCTION_EXACT_DRAW_PASS.load(Ordering::Acquire);
    let draw_ordinal = PRODUCTION_EXACT_DRAW_ORDINAL.load(Ordering::Acquire);
    (PRODUCTION_EXACT_DRAW_PRESENT.load(Ordering::Acquire) == present_seq).then_some(
        ExactProductionDrawIdentity {
            pass_ordinal,
            draw_ordinal,
            projection_observed: PRODUCTION_PROJECTION_OBSERVED_PRESENT.load(Ordering::Acquire)
                == present_seq,
        },
    )
}

/// 主场景 DSV 从未绑定切为绑定时建立一个新的 pass 窗口。
pub fn on_main_pass_bound(generation: u64, present_seq: u64) {
    let discovery_guard = enter_discovery_callback();
    if discovery_guard.is_none() && !production_enabled() {
        return;
    }
    on_main_pass_bound_inner(generation, present_seq, discovery_guard.as_ref());
}

fn on_main_pass_bound_inner(
    generation: u64,
    present_seq: u64,
    discovery_guard: Option<&DiscoveryCallbackGuard>,
) {
    let previous_present = TIMELINE_PRESENT.swap(present_seq, Ordering::AcqRel);
    let pass_ordinal = if previous_present == present_seq {
        NEXT_PASS_ORDINAL.fetch_add(1, Ordering::AcqRel)
    } else {
        NEXT_PASS_ORDINAL.store(1, Ordering::Release);
        0
    };
    CURRENT_PASS_ORDINAL.store(pass_ordinal, Ordering::Release);
    CURRENT_DRAW_ORDINAL.store(0, Ordering::Release);

    if let Some(discovery_guard) = discovery_guard {
        let Ok(mut runtime) = DISCOVERY_RUNTIME.try_lock() else {
            DISCOVERY_RUNTIME_DROPPED.fetch_add(1, Ordering::Relaxed);
            return;
        };
        if let Some(runtime) = runtime.as_mut() {
            if runtime.epoch != discovery_guard.epoch {
                DISCOVERY_RUNTIME_DROPPED.fetch_add(1, Ordering::Relaxed);
                return;
            }
            let first = *runtime.first_present_seq.get_or_insert(present_seq);
            let scheduled = present_seq.checked_sub(first).is_some_and(|delta| {
                delta.is_multiple_of(u64::from(runtime.config.sample_every_n_presents))
            });
            if pass_ordinal == 0 {
                runtime.scheduled_present_seq = scheduled.then_some(present_seq);
                DISCOVERY_SCHEDULED_PRESENT_SEQ.store(
                    if scheduled { present_seq } else { u64::MAX },
                    Ordering::Release,
                );
                if scheduled {
                    DISCOVERY_SCHEDULED_PRESENTS.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }

    if production_armed() && pass_ordinal == PRODUCTION_PASS_ORDINAL.load(Ordering::Acquire) {
        let Ok(_generation_guard) = PRODUCTION_SEND_LOCK.try_lock() else {
            if production_generation_matches(generation) {
                fail_production("Camera main-pass callback raced with StopRun");
            } else {
                log_stale_production_callback(generation);
            }
            return;
        };
        if !production_generation_matches(generation) {
            log_stale_production_callback(generation);
            return;
        }
        let _ =
            submit_production_locked(generation, ProductionEvent::MainScenePass { present_seq });
    }
}

/// 记录 VS constant-buffer 绑定状态。这里保留 tag 只为本进程内把 draw 绑定映射到最近字节。
pub fn on_vs_constant_buffers<F>(
    generation: u64,
    present_seq: u64,
    start_slot: u32,
    num_buffers: u32,
    buffer_at: F,
) where
    F: Fn(u32) -> Option<u64>,
{
    let discovery_guard = enter_discovery_callback();
    if let Some(guard) = discovery_guard.as_ref() {
        update_discovery_vs_slots(guard, start_slot, num_buffers, &buffer_at);
    }
    if production_enabled() {
        let projection_slot = PRODUCTION_BIND_SLOT.load(Ordering::Acquire);
        let target_slots = [
            projection_slot,
            PRODUCTION_VIEW_BIND_SLOT.load(Ordering::Acquire),
        ];
        let target_pass = PRODUCTION_PASS_ORDINAL.load(Ordering::Acquire);
        let target_draw = PRODUCTION_DRAW_ORDINAL.load(Ordering::Acquire);
        let current_pass = CURRENT_PASS_ORDINAL.load(Ordering::Acquire);
        let current_draw = CURRENT_DRAW_ORDINAL.load(Ordering::Acquire);
        if current_pass == target_pass && current_draw <= target_draw {
            for (index, target_slot) in target_slots.into_iter().enumerate() {
                if target_slot == u32::MAX
                    || (index == 1 && target_slot == projection_slot)
                    || !target_slot
                        .checked_sub(start_slot)
                        .is_some_and(|offset| offset < num_buffers)
                {
                    continue;
                }
                let offset = target_slot - start_slot;
                let buffer = buffer_at(offset);
                if let Some(buffer_tag) = buffer {
                    let needs_seed = known_production_buffer(buffer_tag).is_none();
                    if !remember_production_source_tag(buffer_tag) {
                        return;
                    }
                    if needs_seed
                        && let Some(bytes) = cached_production_buffer(buffer_tag, present_seq)
                    {
                        if let Ok(mut known) = KNOWN_PRODUCTION_BUFFERS.try_lock()
                            && let Some(known) = known.as_mut()
                        {
                            known.insert(buffer_tag, bytes.clone());
                            clear_production_source_tag_dirty(buffer_tag);
                        }
                        let _ = submit_production(
                            generation,
                            ProductionEvent::BufferUpdate { buffer_tag, bytes },
                        );
                    }
                }
                if production_armed() {
                    let _ = submit_production(
                        generation,
                        ProductionEvent::VsBind {
                            bind_slot: target_slot,
                            buffer,
                        },
                    );
                }
            }
        }
    }
}

/// 主场景 pass 的一次 draw。pass/draw ordinal 把歧义限定在同一 Present 采样窗内。
///
/// `resolve_bound_vs_buffer` 只会在命中 production 精确 draw 时调用。D3D11 的绑定可以跨
/// Present 和 StartRun generation 持续有效，不能只依赖新的 `VSSetConstantBuffers` 回调；
/// 在唯一 draw 上读取一次当前 slot，既补齐持久绑定身份，也避免给每个 Draw 增加查询开销。
///
/// The resolver returns same-Present CPU-upload bytes for the exact resource visible to this draw.
/// A backing ring buffer can keep the same COM identity while its bound range rotates every frame,
/// so production v1 rejects logical subranges and a missing exact upload is terminal; it may never
/// fall back to tag-keyed bytes from another Present.
#[cfg(test)]
pub fn on_draw<Q, F>(generation: u64, present_seq: u64, sample_qpc: Q, resolve_bound_vs_buffer: F)
where
    Q: FnOnce() -> i64,
    F: FnOnce(u32) -> Result<Option<(u64, Vec<u8>)>, String>,
{
    on_draw_with_viewport(
        generation,
        present_seq,
        sample_qpc,
        resolve_bound_vs_buffer,
        || None,
    );
}

pub fn on_draw_with_viewport<Q, F, V>(
    generation: u64,
    present_seq: u64,
    sample_qpc: Q,
    resolve_bound_vs_buffer: F,
    sample_viewport: V,
) where
    Q: FnOnce() -> i64,
    F: FnOnce(u32) -> Result<Option<(u64, Vec<u8>)>, String>,
    V: FnOnce() -> Option<depth_lab::camera_discovery::DiscoveryViewport>,
{
    let discovery_guard = enter_discovery_callback();
    let discovery = discovery_guard.as_ref();
    if discovery.is_none() && !production_enabled() {
        return;
    }
    if TIMELINE_PRESENT.load(Ordering::Acquire) != present_seq {
        // 调用方只会在 probe 仍持有已唯一验证的主 DSV 时进入这里。D3D11 的 DSV 绑定可
        // 跨 Present 持续有效，因此新 Present 可能没有新的 OMSetRenderTargets 回调；此时
        // 首个 Draw 与当前受审 DSV 共同构成 pass 仍有效的权威证据。Discovery 与 production
        // 都必须在这里重建各自 Present 内的 pass/draw ordinal，不能把跨帧累计 ordinal 当成
        // production binding 漂移。
        on_main_pass_bound_inner(generation, present_seq, discovery);
    }
    let pass_ordinal = CURRENT_PASS_ORDINAL.load(Ordering::Acquire);
    let draw_ordinal = CURRENT_DRAW_ORDINAL.fetch_add(1, Ordering::AcqRel);
    if let Some(discovery_guard) = discovery {
        record_discovery_draw(
            discovery_guard,
            present_seq,
            pass_ordinal,
            draw_ordinal,
            sample_viewport,
        );
    }
    if production_enabled()
        && pass_ordinal == PRODUCTION_PASS_ORDINAL.load(Ordering::Acquire)
        && draw_ordinal == PRODUCTION_DRAW_ORDINAL.load(Ordering::Acquire)
    {
        let Ok(_generation_guard) = PRODUCTION_SEND_LOCK.try_lock() else {
            if production_generation_matches(generation) {
                fail_production("Camera draw callback raced with StopRun");
            } else {
                log_stale_production_callback(generation);
            }
            return;
        };
        if !production_generation_matches(generation) {
            log_stale_production_callback(generation);
            return;
        }
        // This branch is already the configured exact production draw and only runs on an
        // evidence frame. Retain its lock-free identity for exact DSV snapshot selection even when
        // diagnostic JSONL tracing is disabled.
        record_exact_production_draw(present_seq, pass_ordinal, draw_ordinal);
        let projection_slot = PRODUCTION_BIND_SLOT.load(Ordering::Acquire);
        let view_slot = PRODUCTION_VIEW_BIND_SLOT.load(Ordering::Acquire);
        let Some((projection_buffer, projection_bytes)) = seed_resolved_production_buffer_locked(
            generation,
            projection_slot,
            resolve_bound_vs_buffer(projection_slot),
            PRODUCTION_BUFFER_BYTES.load(Ordering::Acquire),
        ) else {
            return;
        };
        let view_buffer = if view_slot == u32::MAX {
            None
        } else if view_slot == projection_slot {
            Some((projection_buffer, projection_bytes.as_slice()))
        } else {
            fail_production("Camera production v1 encountered distinct view/projection VS slots");
            return;
        };
        match record_observed_production_projection(generation, present_seq, &projection_bytes) {
            Ok(true) => {
                mark_production_projection_observed(present_seq);
                if !production_armed() {
                    PRODUCTION_WARMUP_PROJECTION_PRESENT.store(present_seq, Ordering::Release);
                }
            }
            Ok(false) => {
                fail_production(&format!(
                    "Camera production projection matrix is invalid at Present {present_seq}"
                ));
                return;
            }
            Err(error) => {
                fail_production(&error);
                return;
            }
        }
        let view_source = match PRODUCTION_VIEW_SOURCE.try_lock() {
            Ok(view) => *view,
            Err(_) => {
                fail_production("Camera production view source lock is contended");
                return;
            }
        };
        if let Some(view_source) = view_source
            && let Some((_buffer_tag, bytes)) = view_buffer
        {
            let view_valid = if PRODUCTION_VIEW_IS_HYBRID.load(Ordering::Acquire) {
                fallout_render_rotation_from_bound_buffer(view_source, bytes).is_some()
            } else {
                view_pose_from_bound_buffer(view_source, bytes).is_some()
            };
            if !view_valid {
                fail_production(&format!(
                    "Camera production view matrix is invalid at Present {present_seq}"
                ));
                return;
            }
            if !production_armed() {
                PRODUCTION_WARMUP_VIEW_PRESENT.store(present_seq, Ordering::Release);
            }
        }
        if !production_armed() {
            return;
        }
        if !submit_production_locked(
            generation,
            ProductionEvent::VsBind {
                bind_slot: projection_slot,
                buffer: Some(projection_buffer),
            },
        ) {
            return;
        }
        let _ = submit_production_locked(
            generation,
            ProductionEvent::Draw {
                present_seq,
                sample_qpc: sample_qpc(),
                pass_ordinal,
                draw_ordinal,
            },
        );
    }
}

fn seed_resolved_production_buffer_locked(
    generation: u64,
    bind_slot: u32,
    resolved: Result<Option<(u64, Vec<u8>)>, String>,
    expected_bytes: u32,
) -> Option<(u64, Vec<u8>)> {
    let (buffer_tag, bytes) = match resolved {
        Ok(Some(resolved)) => resolved,
        Ok(None) => {
            fail_production(&format!(
                "exact production VS buffer slot {bind_slot} is unbound"
            ));
            return None;
        }
        Err(error) => {
            fail_production(&format!(
                "exact production VS buffer snapshot failed at slot {bind_slot}: {error}"
            ));
            return None;
        }
    };
    if !remember_production_source_tag(buffer_tag) {
        return None;
    }
    if bytes.len() != expected_bytes as usize {
        fail_production(&format!(
            "exact production VS buffer byte length mismatch: expected={expected_bytes} observed={}",
            bytes.len()
        ));
        return None;
    }
    if let Ok(mut known) = KNOWN_PRODUCTION_BUFFERS.try_lock()
        && let Some(known) = known.as_mut()
    {
        known.insert(buffer_tag, bytes.clone());
        clear_production_source_tag_dirty(buffer_tag);
    }
    submit_production_locked(
        generation,
        ProductionEvent::BufferUpdate {
            buffer_tag,
            bytes: bytes.clone(),
        },
    )
    .then_some((buffer_tag, bytes))
}

/// Present 前收口当前 seq；生产线程只会在这个边界写一行。
pub fn on_present(generation: u64, present_seq: u64, projection_evidence_frame: bool) -> bool {
    let discovery_guard = enter_discovery_callback();
    if let Some(guard) = discovery_guard.as_ref() {
        DISCOVERY_OBSERVED_PRESENTS.fetch_add(1, Ordering::Relaxed);
        submit_discovery_present(guard, generation, present_seq);
    }
    let production = if production_armed() {
        let present_qpc = present_qpc();
        let camera = match CAMERA_SAMPLER.try_lock() {
            Ok(mut sampler) => sampler.as_mut().map(|sampler| match &present_qpc {
                Ok(present_qpc) => sampler.sample(present_seq, *present_qpc, generation),
                Err(error) => Err(error.clone()),
            }),
            Err(_) => Some(Err("camera sampler lock is contended".into())),
        };
        let player = match PLAYER_SAMPLER.try_lock() {
            Ok(mut sampler) => sampler.as_mut().map(|sampler| match &present_qpc {
                Ok(present_qpc) => sampler.sample(present_seq, *present_qpc, generation),
                Err(error) => Err(error.clone()),
            }),
            Err(_) => Some(Err("player sampler lock is contended".into())),
        };
        submit_production(
            generation,
            ProductionEvent::Present {
                present_seq,
                projection_evidence_frame,
                camera,
                player,
            },
        )
    } else {
        false
    };
    discovery_guard.is_some() || production
}

fn present_qpc() -> Result<i64, String> {
    let mut qpc = 0i64;
    unsafe { QueryPerformanceCounter(&mut qpc) }
        .map_err(|error| format!("sample Fallout Present QPC failed: {error}"))?;
    (qpc > 0)
        .then_some(qpc)
        .ok_or_else(|| "sample Fallout Present QPC returned a non-positive value".into())
}

/// Readiness evidence for one lifecycle-warmup Present.
///
/// Keep the individual blockers: a generic timeout followed by an empty writer is not enough to
/// distinguish a missing exact draw from a bad in-process pointer chain on the real game.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WarmupCameraReadiness {
    pub production_ready: bool,
    pub view_ready: bool,
    pub projection_ready: bool,
    pub camera_sample_error: Option<String>,
    pub player_sample_error: Option<String>,
}

impl WarmupCameraReadiness {
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.production_ready
            && self.view_ready
            && self.projection_ready
            && self.camera_sample_error.is_none()
            && self.player_sample_error.is_none()
    }
}

/// Check that the exact production draw produced a valid projection and that every configured
/// direct-memory sampler can read the same Present. No timeline event is emitted here.
#[must_use]
pub fn warmup_present_readiness(generation: u64, present_seq: u64) -> WarmupCameraReadiness {
    let production_ready =
        production_enabled() && !production_armed() && production_generation_matches(generation);
    let projection_ready = production_ready
        && PRODUCTION_WARMUP_PROJECTION_PRESENT.load(Ordering::Acquire) == present_seq;
    let view_ready = production_ready
        && (PRODUCTION_VIEW_BIND_SLOT.load(Ordering::Acquire) == u32::MAX
            || PRODUCTION_WARMUP_VIEW_PRESENT.load(Ordering::Acquire) == present_seq);
    let present_qpc = production_ready.then(present_qpc);
    let camera_sample_error = if production_ready {
        match CAMERA_SAMPLER.try_lock() {
            Ok(mut sampler) => sampler.as_mut().and_then(|sampler| match &present_qpc {
                Some(Ok(present_qpc)) => {
                    sampler.sample(present_seq, *present_qpc, generation).err()
                }
                Some(Err(error)) => Some(error.clone()),
                None => None,
            }),
            Err(_) => Some("camera sampler lock is contended".into()),
        }
    } else {
        None
    };
    let player_sample_error = if production_ready {
        match PLAYER_SAMPLER.try_lock() {
            Ok(mut sampler) => sampler.as_mut().and_then(|sampler| match &present_qpc {
                _ if matches!(sampler, PlayerSampler::Cp2077(_)) => {
                    warmup_camera_join_error(present_seq)
                }
                Some(Ok(present_qpc)) => {
                    sampler.sample(present_seq, *present_qpc, generation).err()
                }
                Some(Err(error)) => Some(error.clone()),
                None => None,
            }),
            Err(_) => Some("player sampler lock is contended".into()),
        }
    } else {
        None
    };
    WarmupCameraReadiness {
        production_ready,
        view_ready,
        projection_ready,
        camera_sample_error,
        player_sample_error,
    }
}

fn warmup_camera_join_error(present_seq: u64) -> Option<String> {
    match PRODUCTION_WARMUP_CAMERA_JOIN.try_lock().as_deref() {
        Ok(Some((joined_seq, error))) if *joined_seq == present_seq => error.clone(),
        Ok(_) => Some("CP2077 camera join has not run for this warmup Present".into()),
        Err(_) => Some("CP2077 warmup camera join verdict is contended".into()),
    }
}

#[must_use]
pub fn warmup_present_ready(generation: u64, present_seq: u64) -> bool {
    warmup_present_readiness(generation, present_seq).is_ready()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn cp2077_warmup_readiness_is_the_join_verdict_of_that_same_present() {
        let _guard = CAMERA_TEST_LOCK.lock().unwrap();
        reset_warmup_camera_join();
        // No camera row has been joined yet: not ready, and never a silent pass.
        assert!(warmup_camera_join_error(7).unwrap().contains("has not run"));
        *PRODUCTION_WARMUP_CAMERA_JOIN.lock().unwrap() = Some((7, None));
        assert_eq!(warmup_camera_join_error(7), None);
        // A verdict for another Present proves nothing about this one.
        assert!(warmup_camera_join_error(8).is_some());
        *PRODUCTION_WARMUP_CAMERA_JOIN.lock().unwrap() =
            Some((8, Some("no sampled CP2077 simulation state".into())));
        assert_eq!(
            warmup_camera_join_error(8).as_deref(),
            Some("no sampled CP2077 simulation state")
        );
        reset_warmup_camera_join();
        assert!(warmup_camera_join_error(8).unwrap().contains("has not run"));
    }

    #[test]
    fn candidate_cannot_enter_production_by_renaming_profile_or_removing_game_camera() {
        let mut binding: CameraBinding = serde_json::from_str(include_str!(
            "../../../config/catalogs/dev/fallout4-steam-release-test/camera-binding.json"
        ))
        .unwrap();
        binding.profile_id = "renamed-authoring".into();
        binding.game_camera = None;
        binding.executable_sha256 = FALLOUT4_1_11_240.executable_sha256.into();
        let context = CameraPoseStreamContext {
            identity: StreamIdentity {
                session_id: "candidate-rejection".into(),
                run_id: "r0".into(),
                generation: 1,
            },
            dsv_id: "main-scene-v1".into(),
        };
        let error = init_production(
            PathBuf::new(),
            binding,
            context,
            FALLOUT4_1_11_240.executable_sha256,
            false,
        )
        .unwrap_err();
        assert!(error.contains("authoring-only"), "{error}");
    }
    use depth_lab::camera::TranslationSlot;
    use depth_lab::{
        CAMERA_DISCOVERY_SCHEMA_V1, CameraDiscoveryRenderIdentity, CameraDiscoveryReport,
        CameraPoseStreamContext, DiscoveryDepthFormat, GameCameraBinding, MatrixEncoding,
        MatrixLayout, MatrixRole, MatrixSourceBinding, ShaderStage, StreamIdentity,
        ViewSourceBinding,
    };
    use sha2::{Digest, Sha256};

    use crate::probe::{CameraDrawMethod, dispatch_camera_draw_after_gate};

    #[test]
    fn stop_preserves_disabled_empty_and_missing_player_streams() {
        let _guard = CAMERA_TEST_LOCK.lock().unwrap();
        let _ = finish_stop_production();
        for (mode, file_rows, expected_count, expect_error) in [
            (PlayerPoseOutput::Disabled, None, None, false),
            (PlayerPoseOutput::Enabled, Some(0), Some(0), false),
            (PlayerPoseOutput::Enabled, Some(2), Some(2), false),
            (PlayerPoseOutput::Enabled, None, Some(0), true),
            (PlayerPoseOutput::LegacyCount, None, Some(0), false),
            (PlayerPoseOutput::LegacyCount, Some(2), Some(2), false),
        ] {
            let root =
                std::env::temp_dir().join(format!("dm-player-flush-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&root).unwrap();
            std::fs::write(root.join(crate::camera_writer::CAMERA_POSE_INDEX_FILE), "").unwrap();
            std::fs::write(
                root.join(crate::camera_writer::CAMERA_POSE_DIAGNOSTIC_FILE),
                "",
            )
            .unwrap();
            if let Some(rows) = file_rows {
                std::fs::write(
                    root.join(crate::player_writer::PLAYER_POSE_INDEX_FILE),
                    "{}\n".repeat(rows),
                )
                .unwrap();
            }
            *PRODUCTION_ROOT.lock().unwrap() = Some(ProductionOutput {
                root,
                player_pose: mode,
            });
            begin_stop_production();
            let summary = finish_stop_production();
            assert_eq!(summary.player_pose_row_count, expected_count);
            assert_eq!(summary.errors.is_empty(), !expect_error);
            assert_eq!(finish_stop_production().player_pose_row_count, None);
        }
    }

    pub(crate) static CAMERA_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn production_v2_warmup_flush_and_next_generation_share_the_camera_writer() {
        let _guard = CAMERA_TEST_LOCK.lock().unwrap();
        let _ = finish_stop_production();
        let binding: depth_lab::D3d12CameraBinding = serde_json::from_str(include_str!(
            "../../depth-lab/tests/fixtures/camera-binding-v2.json"
        ))
        .unwrap();
        let view = [
            [1., 0., 0., 0.],
            [0., 1., 0., 0.],
            [0., 0., 1., 0.],
            [-1., -2., -3., 1.],
        ];
        let projection = [
            [1., 0., 0., 0.],
            [0., 16. / 9., 0., 0.],
            [0., 0., 1.001, 1.],
            [0., 0., -0.1, 0.],
        ];
        for generation in [81, 82] {
            let root = std::env::temp_dir().join(format!("dm-v2-camera-{}", uuid::Uuid::new_v4()));
            let context = CameraPoseStreamContext {
                identity: StreamIdentity {
                    session_id: "v2-test".into(),
                    run_id: format!("run-{generation}"),
                    generation,
                },
                dsv_id: "scene".into(),
            };
            init_d3d12_production(
                root,
                binding.clone(),
                context.clone(),
                &binding.executable_sha256,
            )
            .unwrap();
            let row = |seq| {
                depth_lab::d3d12_camera::assemble_pose(
                    &binding,
                    &context,
                    seq,
                    seq as i64 * 100,
                    view,
                    projection,
                )
                .unwrap()
            };
            begin_production_warmup(generation).unwrap();
            assert!(!on_d3d12_pose(row(10), true).unwrap());
            assert!(warmup_present_ready(generation, 10));
            assert_eq!(last_submitted_present_seq(), None);
            arm_production(generation).unwrap();
            // The client can wait for Ready before it sends Arm. More than
            // the projection-cache capacity of genuine proof-copy camera rows
            // must not manufacture depth entries with no consumer.
            let first_selected = 11 + MAX_OBSERVED_PRODUCTION_PROJECTIONS as u64 + 8;
            assert!(!D3D12_PLAYER_SAMPLING_ENABLED.load(Ordering::Acquire));
            let player_sampler_guard = PLAYER_SAMPLER.lock().unwrap();
            assert!(on_d3d12_pose(row(11), false).unwrap());
            drop(player_sampler_guard);
            for seq in 12..first_selected {
                let retain = crate::probe::d3d12_pose_needs_depth_projection(true, true, false);
                assert!(on_d3d12_pose(row(seq), retain).unwrap());
            }
            assert!(
                OBSERVED_PRODUCTION_PROJECTIONS
                    .lock()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .frames
                    .is_empty()
            );
            assert!(
                on_d3d12_pose(
                    row(first_selected),
                    crate::probe::d3d12_pose_needs_depth_projection(true, true, true),
                )
                .unwrap()
            );
            assert!(
                on_d3d12_pose(
                    row(first_selected + 1),
                    crate::probe::d3d12_pose_needs_depth_projection(true, true, false),
                )
                .unwrap()
            );
            let mut stale = row(first_selected + 2);
            stale.identity.generation -= 1;
            assert!(on_d3d12_pose(stale, true).is_err());
            begin_stop_production();
            // GPU work accepted before StopRun can still retrieve its exact projection.
            assert!(
                take_observed_projection(generation, first_selected)
                    .unwrap()
                    .is_some()
            );
            assert!(
                take_observed_projection(generation, first_selected + 1)
                    .unwrap()
                    .is_none()
            );
            assert!(on_d3d12_pose(row(first_selected + 2), true).is_err());
            let summary = finish_stop_production();
            assert!(summary.errors.is_empty(), "{:?}", summary.errors);
            assert_eq!(summary.pose_row_count, first_selected - 11 + 2);
            assert_eq!(summary.diagnostic_row_count, 0);
            assert_eq!(summary.last_present_seq, Some(first_selected + 1));
        }
    }

    #[test]
    fn d3d12_writer_keeps_cp2077_camera_and_player_on_one_present_identity() {
        let _guard = CAMERA_TEST_LOCK.lock().unwrap();
        let _ = finish_stop_production();
        let root =
            std::env::temp_dir().join(format!("dm-cp2077-d3d12-player-{}", uuid::Uuid::new_v4()));
        let binding: depth_lab::D3d12CameraBinding = serde_json::from_str(include_str!(
            "../../depth-lab/tests/fixtures/camera-binding-v2.json"
        ))
        .unwrap();
        let context = CameraPoseStreamContext {
            identity: StreamIdentity {
                session_id: "cp2077-player-test".into(),
                run_id: "run-1".into(),
                generation: 91,
            },
            dsv_id: "scene".into(),
        };
        let view = [
            [1., 0., 0., 0.],
            [0., 1., 0., 0.],
            [0., 0., 1., 0.],
            [-1., -2., -3., 1.],
        ];
        let projection = [
            [1., 0., 0., 0.],
            [0., 16. / 9., 0., 0.],
            [0., 0., 1.001, 1.],
            [0., 0., -0.1, 0.],
        ];
        let camera_writer = CameraPoseIndexWriter::new(&root).unwrap();
        let player_writer = PlayerPoseIndexWriter::new(
            &root,
            context.identity.clone(),
            depth_lab::CP2077_BUILD_20383525_EXE_SHA256.into(),
            1_000,
            crate::cp2077_player_probe::PROVIDER.into(),
        )
        .unwrap();
        PRODUCTION_FAILED.store(false, Ordering::Release);
        *PRODUCTION_ERROR.lock().unwrap() = None;
        let (tx, rx) = channel();
        let identity = context.identity.clone();
        let join = std::thread::spawn(move || {
            production_loop(rx, None, camera_writer, Some(player_writer), identity)
        });
        for (seq, qpc, x) in [(10, 1_000, 0.0), (11, 1_100, 1.0)] {
            let row = depth_lab::d3d12_camera::assemble_pose(
                &binding, &context, seq, qpc, view, projection,
            )
            .unwrap();
            tx.send(ProductionEvent::D3d12Pose {
                row: Box::new(row),
                player: Some(Ok(PlayerRootSample {
                    present_seq: seq,
                    present_qpc: qpc,
                    kcd2_root_source: None,
                    // The state this Present rendered was in memory about one frame earlier.
                    cp2077_camera_join: Some(depth_lab::Cp2077CameraJoinReceipt {
                        state_first_qpc: qpc - 40,
                        state_last_qpc: qpc - 9,
                        state_reads: 16,
                        position_residual_metres: 0.0002,
                        rotation_residual_degrees: 3e-6,
                        candidates: 2,
                        torn_candidates: 1,
                    }),
                    player_position: [x, 0.0, 0.0],
                    player_rotation_quaternion: [0.0, 0.0, 0.0, 1.0],
                })),
            })
            .unwrap();
        }
        drop(tx);
        join.join().unwrap();
        assert!(!PRODUCTION_FAILED.load(Ordering::Acquire));
        assert!(PRODUCTION_ERROR.lock().unwrap().is_none());
        let camera_rows =
            std::fs::read_to_string(root.join(crate::camera_writer::CAMERA_POSE_INDEX_FILE))
                .unwrap();
        let player_rows =
            std::fs::read_to_string(root.join(crate::player_writer::PLAYER_POSE_INDEX_FILE))
                .unwrap();
        assert_eq!(camera_rows.lines().count(), 2);
        let player_rows = player_rows
            .lines()
            .map(|line| serde_json::from_str::<depth_lab::PlayerPoseRow>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(player_rows.len(), 2);
        assert_eq!(player_rows[0].present_seq, 10);
        assert_eq!(player_rows[1].present_seq, 11);
        assert_eq!(player_rows[0].present_qpc, 1_000);
        assert_eq!(player_rows[1].present_qpc, 1_100);
        assert_eq!(player_rows[0].source, crate::cp2077_player_probe::PROVIDER);
        assert_eq!(player_rows[0].player_speed, Some([10.0, 0.0, 0.0]));
        let join = player_rows[1].cp2077_camera_join.as_ref().unwrap();
        assert_eq!((join.state_first_qpc, join.torn_candidates), (1_060, 1));
        std::fs::remove_dir_all(root).unwrap();

        let failure_root = std::env::temp_dir().join(format!(
            "dm-cp2077-d3d12-player-missing-{}",
            uuid::Uuid::new_v4()
        ));
        let camera_writer = CameraPoseIndexWriter::new(&failure_root).unwrap();
        let player_writer = PlayerPoseIndexWriter::new(
            &failure_root,
            context.identity.clone(),
            depth_lab::CP2077_BUILD_20383525_EXE_SHA256.into(),
            1_000,
            crate::cp2077_player_probe::PROVIDER.into(),
        )
        .unwrap();
        PRODUCTION_FAILED.store(false, Ordering::Release);
        *PRODUCTION_ERROR.lock().unwrap() = None;
        let (tx, rx) = channel();
        let identity = context.identity.clone();
        let join = std::thread::spawn(move || {
            production_loop(rx, None, camera_writer, Some(player_writer), identity)
        });
        tx.send(ProductionEvent::D3d12Pose {
            row: Box::new(
                depth_lab::d3d12_camera::assemble_pose(
                    &binding, &context, 12, 1_200, view, projection,
                )
                .unwrap(),
            ),
            player: None,
        })
        .unwrap();
        drop(tx);
        join.join().unwrap();
        assert!(PRODUCTION_FAILED.load(Ordering::Acquire));
        assert!(
            PRODUCTION_ERROR
                .lock()
                .unwrap()
                .as_deref()
                .is_some_and(|error| error.contains("omitted the player sample"))
        );
        PRODUCTION_FAILED.store(false, Ordering::Release);
        *PRODUCTION_ERROR.lock().unwrap() = None;
        std::fs::remove_dir_all(failure_root).unwrap();
    }

    fn discovery_config(phase_id: &str) -> CameraDiscoveryConfig {
        CameraDiscoveryConfig {
            schema: CAMERA_DISCOVERY_SCHEMA_V1.into(),
            phase_id: phase_id.into(),
            executable_sha256: format!("sha256:{}", "0".repeat(64)),
            render: Some(CameraDiscoveryRenderIdentity {
                width: 1920,
                height: 1080,
                sample_count: 1,
                formats: vec![DiscoveryDepthFormat::R24G8Typeless],
            }),
            dsv_selection: None,
            sample_every_n_presents: 1,
            max_main_passes_per_present: 1,
            draw_window_start: 0,
            draw_window_count: 1,
            comparison_camera_reference: None,
        }
    }

    fn discovery_runtime(phase_id: &str) -> DiscoveryRuntime {
        DiscoveryRuntime {
            epoch: 1,
            config: discovery_config(phase_id),
            first_present_seq: None,
            scheduled_present_seq: None,
            latest_buffers: HashMap::new(),
            vs_slots: BTreeMap::new(),
            next_buffer_version: 0,
            pending_batch: None,
            last_submitted_batch_seq: 0,
            cold_seed_pending: true,
        }
    }

    fn empty_batch_event(
        epoch: u64,
        batch_seq: u64,
        present_seq: u64,
        logical_draws: u64,
    ) -> DiscoveryDataEvent {
        DiscoveryDataEvent {
            epoch,
            batch_seq,
            batch: DiscoveryPresentBatch {
                present_seq,
                logical_draws,
                draw_contexts: (0..logical_draws)
                    .map(|draw| depth_lab::camera_discovery::DiscoveryDrawContext {
                        present_seq,
                        pass_ordinal: 0,
                        draw_ordinal: draw as u32,
                        viewport: None,
                    })
                    .collect(),
                bindings: Vec::new(),
                comparison_camera_reference: None,
                comparison_camera_error: None,
            },
            sampling: DiscoverySamplingStats {
                observed_presents: present_seq.saturating_add(1),
                scheduled_presents: batch_seq,
                observed_draws: logical_draws,
                scheduled_draws: logical_draws,
                submitted_draws: logical_draws,
                ..DiscoverySamplingStats::default()
            },
        }
    }

    fn wait_for_discovery_report(path: &std::path::Path) -> CameraDiscoveryReport {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if let Ok(bytes) = std::fs::read(path)
                && let Ok(report) = serde_json::from_slice(&bytes)
            {
                return report;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for {}",
                path.display()
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    fn source(role: MatrixRole, bind_slot: u32) -> MatrixSourceBinding {
        MatrixSourceBinding {
            shader_stage: ShaderStage::Vertex,
            bind_slot,
            pass_ordinal: 0,
            draw_ordinal: 0,
            buffer_bytes: 64,
            offset: 0,
            encoding: MatrixEncoding::F32LittleEndian,
            layout: MatrixLayout::RowMajor,
            transpose: false,
            role,
        }
    }

    #[test]
    fn production_diagnostics_do_not_collapse_sampling_gaps_into_no_scene_depth() {
        assert_eq!(
            diagnostic_reason(&CameraPoseAssemblyError::MissingProjection { present_seq: 7 }),
            Some(CameraPoseDiagnosticReason::BindingSampleMissing)
        );
        assert_eq!(
            diagnostic_reason(&CameraPoseAssemblyError::AmbiguousMatrixSource {
                present_seq: 7,
                kind: "view",
                first_buffer_tag: 1,
                second_buffer_tag: 2,
            }),
            Some(CameraPoseDiagnosticReason::BindingAmbiguity)
        );
        assert_eq!(
            diagnostic_reason(&CameraPoseAssemblyError::InvalidViewMatrix { present_seq: 7 }),
            Some(CameraPoseDiagnosticReason::MatrixInvalid)
        );
        assert_ne!(
            diagnostic_reason(&CameraPoseAssemblyError::MissingView { present_seq: 7 }),
            Some(CameraPoseDiagnosticReason::NoMainSceneDepth)
        );
    }

    #[test]
    fn observed_projection_bytes_never_cross_d3d_resource_tags() {
        let _test_guard = CAMERA_TEST_LOCK.lock().unwrap();
        let known_bytes = vec![1; 64];
        let pending_bytes = vec![2; 64];
        let previous_known = KNOWN_PRODUCTION_BUFFERS
            .lock()
            .unwrap()
            .replace(HashMap::from([(0x100, known_bytes.clone())]));
        let previous_pending = PENDING_PRODUCTION_BUFFERS
            .lock()
            .unwrap()
            .replace(HashMap::from([(0x200, pending_bytes.clone())]));
        let previous_pending_present = PENDING_PRODUCTION_PRESENT.swap(7, Ordering::AcqRel);
        let previous_dirty = PRODUCTION_DIRTY_SOURCE_MASK.swap(0, Ordering::AcqRel);

        assert_eq!(observed_production_buffer(0x100, 7), Some(known_bytes));
        assert_eq!(observed_production_buffer(0x200, 7), Some(pending_bytes));
        assert_eq!(
            observed_production_buffer(0x300, 7),
            None,
            "a newly bound resource must not borrow another tag's projection bytes"
        );
        assert_eq!(
            observed_production_buffer(0x200, 8),
            None,
            "pending bytes from another Present must never become observed_this_frame"
        );

        *KNOWN_PRODUCTION_BUFFERS.lock().unwrap() = previous_known;
        *PENDING_PRODUCTION_BUFFERS.lock().unwrap() = previous_pending;
        PENDING_PRODUCTION_PRESENT.store(previous_pending_present, Ordering::Release);
        PRODUCTION_DIRTY_SOURCE_MASK.store(previous_dirty, Ordering::Release);
    }

    #[test]
    fn exact_draw_uses_only_same_present_cpu_upload_bytes() {
        let _test_guard = CAMERA_TEST_LOCK.lock().unwrap();
        let previous_enabled = PRODUCTION_ENABLED.swap(true, Ordering::AcqRel);
        let previous_failed = PRODUCTION_FAILED.swap(false, Ordering::AcqRel);
        let previous_known = KNOWN_PRODUCTION_BUFFERS
            .lock()
            .unwrap()
            .replace(HashMap::from([(0x100, vec![9; 64])]));
        let previous_pending = PENDING_PRODUCTION_BUFFERS
            .lock()
            .unwrap()
            .replace(HashMap::from([(0x100, vec![7; 64])]));
        let previous_pending_present = PENDING_PRODUCTION_PRESENT.swap(42, Ordering::AcqRel);

        assert_eq!(
            exact_production_buffer_for_present(0x100, 42, 64).unwrap(),
            vec![7; 64]
        );
        assert!(
            exact_production_buffer_for_present(0x100, 41, 64)
                .unwrap_err()
                .contains("missing for Present 41"),
            "an older known buffer must never satisfy exact-draw evidence"
        );
        assert!(
            exact_production_buffer_for_present(0x200, 42, 64)
                .unwrap_err()
                .contains("resource=0x200")
        );
        assert!(
            exact_production_buffer_for_present(0x100, 42, 752)
                .unwrap_err()
                .contains("byte length mismatch")
        );

        PRODUCTION_ENABLED.store(previous_enabled, Ordering::Release);
        PRODUCTION_FAILED.store(previous_failed, Ordering::Release);
        *KNOWN_PRODUCTION_BUFFERS.lock().unwrap() = previous_known;
        *PENDING_PRODUCTION_BUFFERS.lock().unwrap() = previous_pending;
        PENDING_PRODUCTION_PRESENT.store(previous_pending_present, Ordering::Release);
    }

    #[test]
    fn skipped_source_write_invalidates_cached_projection_without_copying_bytes() {
        let _test_guard = CAMERA_TEST_LOCK.lock().unwrap();
        let previous_tags = PRODUCTION_SOURCE_TAGS
            .iter()
            .map(|tag| tag.load(Ordering::Acquire))
            .collect::<Vec<_>>();
        let previous_bloom = PRODUCTION_SOURCE_TAG_BLOOM.load(Ordering::Acquire);
        let previous_dirty = PRODUCTION_DIRTY_SOURCE_MASK.load(Ordering::Acquire);
        let previous_known = KNOWN_PRODUCTION_BUFFERS
            .lock()
            .unwrap()
            .replace(HashMap::from([(0x100, vec![1; 64])]));

        for tag in &PRODUCTION_SOURCE_TAGS {
            tag.store(0, Ordering::Release);
        }
        PRODUCTION_SOURCE_TAGS[0].store(0x100, Ordering::Release);
        PRODUCTION_SOURCE_TAG_BLOOM
            .store(production_source_tag_bloom_bit(0x100), Ordering::Release);
        PRODUCTION_DIRTY_SOURCE_MASK.store(0, Ordering::Release);
        assert!(known_production_buffer(0x100).is_some());

        mark_production_buffer_write(0x100);
        assert!(
            known_production_buffer(0x100).is_none(),
            "a skipped Map/Update must make old projection bytes ineligible"
        );

        for (tag, previous) in PRODUCTION_SOURCE_TAGS.iter().zip(previous_tags) {
            tag.store(previous, Ordering::Release);
        }
        PRODUCTION_SOURCE_TAG_BLOOM.store(previous_bloom, Ordering::Release);
        PRODUCTION_DIRTY_SOURCE_MASK.store(previous_dirty, Ordering::Release);
        *KNOWN_PRODUCTION_BUFFERS.lock().unwrap() = previous_known;
    }

    #[test]
    fn a_high_draw_burst_stays_in_one_present_batch() {
        let _test_guard = CAMERA_TEST_LOCK.lock().unwrap();
        let bytes: Arc<[u8]> = vec![0; 128].into();
        let latest_buffers = HashMap::from([(
            0x100,
            VersionedDiscoveryBuffer {
                version: 7,
                cold_seed: false,
                bytes,
            },
        )]);
        let vs_slots = BTreeMap::from([(5, 0x100)]);
        let mut pending = PendingDiscoveryBatch::new(42);
        for draw_ordinal in 0..512 {
            assert_eq!(
                pending.record_draw(0, draw_ordinal, &vs_slots, &latest_buffers),
                0
            );
        }

        let batch = pending.into_batch();
        assert_eq!(batch.present_seq, 42);
        assert_eq!(batch.logical_draws, 512);
        assert_eq!(batch.bindings.len(), 1);
        assert_eq!(batch.bindings[0].draw_ordinals.len(), 512);
    }

    #[test]
    fn buffer_version_changes_split_snapshots_without_losing_draws() {
        let _test_guard = CAMERA_TEST_LOCK.lock().unwrap();
        let mut latest_buffers = HashMap::from([(
            0x100,
            VersionedDiscoveryBuffer {
                version: 1,
                cold_seed: false,
                bytes: vec![1; 128].into(),
            },
        )]);
        let vs_slots = BTreeMap::from([(5, 0x100)]);
        let mut pending = PendingDiscoveryBatch::new(42);
        assert_eq!(pending.record_draw(0, 7, &vs_slots, &latest_buffers), 0);
        assert_eq!(pending.record_draw(0, 8, &vs_slots, &latest_buffers), 0);
        latest_buffers.insert(
            0x100,
            VersionedDiscoveryBuffer {
                version: 2,
                cold_seed: false,
                bytes: vec![2; 128].into(),
            },
        );
        assert_eq!(pending.record_draw(0, 9, &vs_slots, &latest_buffers), 0);

        let batch = pending.into_batch();
        assert_eq!(batch.logical_draws, 3);
        assert_eq!(batch.bindings.len(), 2);
        assert_eq!(batch.bindings[0].draw_ordinals, vec![7, 8]);
        assert_eq!(batch.bindings[1].draw_ordinals, vec![9]);
        assert_ne!(batch.bindings[0].bytes, batch.bindings[1].bytes);
    }

    #[test]
    fn full_data_queue_accounts_the_whole_logical_batch() {
        let _test_guard = CAMERA_TEST_LOCK.lock().unwrap();
        reset_discovery_stats();
        DISCOVERY_SCHEDULED_DRAWS.store(7, Ordering::Release);
        let (data_tx, _data_rx) = sync_channel(1);
        data_tx.try_send(empty_batch_event(99, 1, 0, 1)).unwrap();
        let mut runtime = discovery_runtime("queue-full");
        let mut pending = PendingDiscoveryBatch::new(10);
        pending.logical_draws = 7;
        runtime.pending_batch = Some(pending);

        submit_pending_discovery_batch_to(&mut runtime, &data_tx);

        let sampling = discovery_sampling_stats();
        assert_eq!(sampling.scheduled_draws, 7);
        assert_eq!(sampling.submitted_draws, 0);
        assert_eq!(sampling.queue_dropped_draws, 7);
        assert_eq!(runtime.last_submitted_batch_seq, 0);
    }

    #[test]
    fn control_ack_drains_full_data_queue_and_seals_each_epoch_once() {
        let _test_guard = CAMERA_TEST_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join(format!(
            "dm-camera-control-data-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let (data_tx, data_rx) = sync_channel(1);
        let (control_tx, control_rx) = channel();

        let (a1_start_tx, a1_start_rx) = sync_channel(1);
        control_tx
            .send(DiscoveryControlEvent::Start {
                epoch: 1,
                output_dir: root.clone(),
                config: Box::new(discovery_config("A1-worker")),
                selected_dsv_descriptor: None,
                reply: a1_start_tx,
            })
            .unwrap();
        data_tx.try_send(empty_batch_event(1, 1, 0, 2)).unwrap();

        let (early_a2_tx, early_a2_rx) = sync_channel(1);
        control_tx
            .send(DiscoveryControlEvent::Start {
                epoch: 2,
                output_dir: root.clone(),
                config: Box::new(discovery_config("A2-worker")),
                selected_dsv_descriptor: None,
                reply: early_a2_tx,
            })
            .unwrap();
        let (a1_flush_tx, a1_flush_rx) = sync_channel(1);
        // 数据槽此刻确定为满；独立控制通道仍能立即接收 Flush。
        control_tx
            .send(DiscoveryControlEvent::Flush {
                epoch: 1,
                cutoff_batch_seq: 1,
                sampling: empty_batch_event(1, 1, 0, 2).sampling,
                reply: a1_flush_tx,
            })
            .unwrap();

        let worker = std::thread::spawn(move || scan_loop(control_rx, data_rx));
        assert_eq!(
            a1_start_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            Ok(())
        );
        assert!(
            early_a2_rx
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .unwrap_err()
                .contains("仍持有 phase=A1-worker")
        );
        assert_eq!(
            a1_flush_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            Ok(())
        );

        let a1_path = root.join("camera-candidates-A1-worker.json");
        let a1_sealed = std::fs::read(&a1_path).unwrap();
        let a1: CameraDiscoveryReport = serde_json::from_slice(&a1_sealed).unwrap();
        let a1_progress: depth_lab::CameraDiscoveryProgress = serde_json::from_slice(
            &std::fs::read(root.join("camera-discovery-progress-A1-worker.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(a1.phase_id, "A1-worker");
        assert_eq!(a1_progress.phase_id, a1.phase_id);
        assert_eq!(a1_progress.reliable, a1.reliable);
        assert_eq!(a1.processed_draws, 2);
        assert_eq!(a1.sampling.submitted_draws, 2);
        assert_eq!(
            std::fs::read(root.join("camera-candidates.json")).unwrap(),
            a1_sealed,
            "Flush 的 mutable 与 sealed 报告必须复用同一份序列化字节"
        );

        // Flush ACK 后的迟到数据没有活动 phase，不能再改 A1 固定文件。
        data_tx.send(empty_batch_event(1, 2, 1, 100)).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(std::fs::read(&a1_path).unwrap(), a1_sealed);

        let (a2_start_tx, a2_start_rx) = sync_channel(1);
        control_tx
            .send(DiscoveryControlEvent::Start {
                epoch: 2,
                output_dir: root.clone(),
                config: Box::new(discovery_config("A2-worker")),
                selected_dsv_descriptor: None,
                reply: a2_start_tx,
            })
            .unwrap();
        assert_eq!(
            a2_start_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            Ok(())
        );
        data_tx.send(empty_batch_event(2, 1, 2, 1)).unwrap();
        let (a2_flush_tx, a2_flush_rx) = sync_channel(1);
        control_tx
            .send(DiscoveryControlEvent::Flush {
                epoch: 2,
                cutoff_batch_seq: 1,
                sampling: empty_batch_event(2, 1, 2, 1).sampling,
                reply: a2_flush_tx,
            })
            .unwrap();
        assert_eq!(
            a2_flush_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            Ok(())
        );

        let a2: CameraDiscoveryReport = serde_json::from_slice(
            &std::fs::read(root.join("camera-candidates-A2-worker.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(a2.phase_id, "A2-worker");
        assert_eq!(a2.processed_draws, 1);
        assert_eq!(a2.sampling.submitted_draws, 1);
        assert_eq!(std::fs::read(&a1_path).unwrap(), a1_sealed);

        drop(control_tx);
        drop(data_tx);
        worker.join().unwrap();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn stop_quiesces_admitted_callbacks_and_old_epoch_cannot_aba_into_a2() {
        let _test_guard = CAMERA_TEST_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join(format!(
            "dm-camera-callback-quiesce-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);

        init_discovery(root.clone(), discovery_config("race-A1"), None, 1).unwrap();
        on_main_pass_bound(0, 90);
        on_draw(0, 90, || 1, |_| Ok(None));
        assert!(on_present(0, 90, true));
        on_main_pass_bound(0, 100);
        let admitted = enter_discovery_callback().expect("A1 callback should be admitted");
        let a1_epoch = admitted.epoch;
        let (stop_tx, stop_rx) = sync_channel(1);
        let stop_thread = std::thread::spawn(move || {
            stop_tx.send(stop_discovery()).unwrap();
        });
        let deadline = Instant::now() + Duration::from_secs(1);
        while discovery_enabled() && Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert!(
            !discovery_enabled(),
            "stop did not close callback admission"
        );
        assert!(matches!(stop_rx.try_recv(), Err(TryRecvError::Empty)));

        // 模拟已经 admission、随后挂起的旧 Draw：stop 必须等待回调退出，并把这个尚未
        // 到达 Present hook 的半 batch 显式记为 boundary discard；不能冒充完整证据。
        record_discovery_draw(&admitted, 100, 0, 0, || None);
        drop(admitted);
        assert_eq!(
            stop_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            Ok(())
        );
        stop_thread.join().unwrap();

        let a1_path = root.join("camera-candidates-race-A1.json");
        let a1_sealed = std::fs::read(&a1_path).unwrap();
        let a1: CameraDiscoveryReport = serde_json::from_slice(&a1_sealed).unwrap();
        assert_eq!(a1.sampling.scheduled_draws, 2);
        assert_eq!(a1.sampling.submitted_draws, 1);
        assert_eq!(a1.processed_draws, 1);
        assert_eq!(a1.sampling.phase_boundary_discarded_presents, 1);
        assert_eq!(a1.sampling.phase_boundary_discarded_draws, 1);
        assert!(a1.reliable, "{:?}", a1.blockers);

        init_discovery(root.clone(), discovery_config("race-A2"), None, 1).unwrap();
        let a2_epoch = DISCOVERY_EPOCH.load(Ordering::Acquire);
        assert_ne!(a1_epoch, a2_epoch);
        // 模拟旧 callback 在 A1 捕获 epoch 后、in_flight++ 前挂起，直到 A2 才恢复。
        // 二次复查必须拒绝它，不能把 A1 callback 当成 A2 callback 接纳。
        assert!(enter_discovery_callback_for_epoch(a1_epoch).is_none());
        assert_eq!(DISCOVERY_CALLBACKS_IN_FLIGHT.load(Ordering::Acquire), 0);

        on_main_pass_bound(0, 200);
        on_draw(0, 200, || 1, |_| Ok(None));
        assert!(on_present(0, 200, true));
        stop_discovery().unwrap();
        let a2: CameraDiscoveryReport = serde_json::from_slice(
            &std::fs::read(root.join("camera-candidates-race-A2.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(a2.sampling.observed_draws, 1);
        assert_eq!(a2.sampling.scheduled_draws, 1);
        assert_eq!(a2.sampling.submitted_draws, 1);
        assert_eq!(a2.processed_draws, 1);
        assert_eq!(std::fs::read(&a1_path).unwrap(), a1_sealed);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn discovery_viewport_is_sampled_only_inside_exact_scheduled_draw_window() {
        let _test_guard = CAMERA_TEST_LOCK.lock().unwrap();
        let root =
            std::env::temp_dir().join(format!("dm-camera-viewport-{}", uuid::Uuid::new_v4()));
        let mut config = discovery_config("viewport-window");
        config.schema = depth_lab::probe_config::CAMERA_DISCOVERY_SCHEMA_V2.into();
        config.sample_every_n_presents = 2;
        config.draw_window_start = 1;
        let viewport = depth_lab::camera_discovery::DiscoveryViewport {
            count: 1,
            x: 0.0,
            y: 0.0,
            width: 1920.0,
            height: 1080.0,
            min_depth: 0.0098,
            max_depth: 1.0,
        };
        init_discovery(root.clone(), config, None, 1).unwrap();
        let queries = std::cell::Cell::new(0);
        for present_seq in 10..=12 {
            on_main_pass_bound(0, present_seq);
            for draw in 0..3 {
                on_draw_with_viewport(
                    0,
                    present_seq,
                    || 1,
                    |_| Ok(None),
                    || {
                        assert_eq!(draw, 1);
                        assert_ne!(present_seq, 11);
                        queries.set(queries.get() + 1);
                        Some(viewport)
                    },
                );
            }
            assert!(on_present(0, present_seq, true));
        }
        stop_discovery().unwrap();
        let report: CameraDiscoveryReport = serde_json::from_slice(
            &std::fs::read(root.join("camera-candidates-viewport-window.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(queries.get(), 2);
        assert_eq!(
            report
                .draw_contexts
                .iter()
                .map(|row| (
                    row.present_seq,
                    row.pass_ordinal,
                    row.draw_ordinal,
                    row.viewport
                ))
                .collect::<Vec<_>>(),
            vec![(10, 0, 1, Some(viewport)), (12, 0, 1, Some(viewport))]
        );
        assert_eq!(report.processed_draws, 2);
        assert!(report.reliable, "{:?}", report.blockers);
        on_draw_with_viewport(
            0,
            13,
            || 1,
            |_| Ok(None),
            || panic!("disabled discovery must not query viewport"),
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn admitted_ambiguity_is_counted_before_stop_can_seal_the_report() {
        let _test_guard = CAMERA_TEST_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join(format!(
            "dm-camera-ambiguity-quiesce-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);

        init_discovery(root.clone(), discovery_config("ambiguity-race"), None, 1).unwrap();
        let admitted = enter_discovery_callback().expect("callback should be admitted");
        let (stop_tx, stop_rx) = sync_channel(1);
        let stop_thread = std::thread::spawn(move || {
            stop_tx.send(stop_discovery()).unwrap();
        });
        let deadline = Instant::now() + Duration::from_secs(1);
        while discovery_enabled() && Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert!(matches!(stop_rx.try_recv(), Err(TryRecvError::Empty)));

        mark_discovery_dsv_ambiguous(&admitted);
        drop(admitted);
        assert_eq!(
            stop_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            Ok(())
        );
        stop_thread.join().unwrap();

        let report: CameraDiscoveryReport = serde_json::from_slice(
            &std::fs::read(root.join("camera-candidates-ambiguity-race.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(report.sampling.dsv_ambiguities, 1);
        assert!(!report.reliable);
        assert!(
            report
                .blockers
                .contains(&depth_lab::DiscoveryBlocker::AmbiguousDepthTarget)
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn stale_epoch_data_fail_closes_an_active_phase() {
        let _test_guard = CAMERA_TEST_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join(format!(
            "dm-camera-stale-epoch-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let mut phase = None;
        start_discovery_phase(
            &mut phase,
            2,
            root.clone(),
            discovery_config("stale-A2"),
            None,
        )
        .unwrap();
        process_discovery_batch(&mut phase, empty_batch_event(1, 1, 0, 1));
        let (_data_tx, data_rx) = sync_channel(1);
        let error = flush_discovery_phase(
            &mut phase,
            &data_rx,
            2,
            0,
            DiscoverySamplingStats::default(),
        )
        .unwrap_err();
        assert!(error.contains("stale batch epoch entered active phase"));
        assert!(!root.join("camera-candidates-stale-A2.json").exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn checkpoint_waits_for_an_idle_worker_poll() {
        let _test_guard = CAMERA_TEST_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join(format!(
            "dm-camera-idle-checkpoint-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let mut phase = None;
        start_discovery_phase(&mut phase, 1, root.clone(), discovery_config("idle"), None).unwrap();
        phase.as_mut().unwrap().last_checkpoint = Instant::now() - DISCOVERY_CHECKPOINT_INTERVAL;

        process_discovery_batch(&mut phase, empty_batch_event(1, 1, 10, 3));
        assert!(
            !root.join("camera-candidates.json").exists(),
            "处理 batch 时不得内联生成大 checkpoint"
        );

        maybe_dump_discovery_checkpoint(&mut phase);
        let checkpoint: CameraDiscoveryReport =
            serde_json::from_slice(&std::fs::read(root.join("camera-candidates.json")).unwrap())
                .unwrap();
        let progress: depth_lab::CameraDiscoveryProgress = serde_json::from_slice(
            &std::fs::read(root.join("camera-discovery-progress.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(checkpoint.phase_id, "idle");
        assert_eq!(checkpoint.processed_draws, 3);
        assert_eq!(checkpoint.sampling.submitted_draws, 3);
        assert_eq!(progress.phase_id, checkpoint.phase_id);
        assert_eq!(progress.processed_draws, checkpoint.processed_draws);
        assert_eq!(
            progress.schema,
            depth_lab::CAMERA_DISCOVERY_PROGRESS_SCHEMA_V1
        );
        assert!(!root.join("camera-candidates-idle.json").exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn discovery_cold_seed_recovers_a_prebound_projection_once_per_phase() {
        let _test_guard = CAMERA_TEST_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join(format!(
            "dm-camera-cold-seed-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);

        init_discovery(root.clone(), discovery_config("cold-seed"), None, 1).unwrap();
        on_main_pass_bound(0, 800);
        let epoch = take_discovery_cold_seed_request(800)
            .expect("first draw in the phase window must claim the cold seed");
        assert!(
            take_discovery_cold_seed_request(800).is_none(),
            "the synchronous GPU readback must not become a per-draw cost"
        );

        let projection = [
            [1.19_f32, 0.0, 0.0, 0.0],
            [0.0, 2.11, 0.0, 0.0],
            [0.0, 0.0, 1.0, -1.0],
            [0.0, 0.0, 1.0, 0.0],
        ];
        let bytes = projection
            .into_iter()
            .flatten()
            .flat_map(f32::to_le_bytes)
            .collect::<Vec<_>>();
        on_discovery_cold_seed(epoch, &[(12, 0xabc, bytes)]).unwrap();

        on_draw(0, 800, || 1, |_| Ok(None));
        assert!(on_present(0, 800, true));
        stop_discovery().unwrap();

        let report = wait_for_discovery_report(&root.join("camera-candidates-cold-seed.json"));
        assert!(report.reliable, "{:?}", report.blockers);
        assert_eq!(report.processed_draws, 1);
        assert_eq!(report.projection_candidates.len(), 1);
        assert_eq!(report.projection_candidates[0].source.bind_slot, 12);
        assert_eq!(report.projection_candidates[0].source.buffer_bytes, 64);
        assert_eq!(report.projection_candidates[0].source.offset, 0);
        assert_eq!(report.projection_candidates[0].matrix, projection);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn missing_control_ack_returns_an_explicit_timeout() {
        let _test_guard = CAMERA_TEST_LOCK.lock().unwrap();
        for operation in ["Start", "Flush"] {
            let (_reply_tx, reply_rx) = sync_channel(1);
            let error = wait_for_discovery_ack(
                reply_rx,
                operation,
                "timeout-test",
                77,
                Duration::from_millis(5),
            )
            .unwrap_err();
            assert!(error.contains(&format!("{operation} ACK 超时")));
            assert!(error.contains("phase=timeout-test"));
            assert!(error.contains("epoch=77"));
        }
    }

    #[test]
    fn production_warmup_observes_binding_without_emitting_formal_rows() {
        let _test_guard = CAMERA_TEST_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join(format!(
            "dm-camera-production-warmup-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let executable = std::env::current_exe().unwrap();
        let executable_hash = format!(
            "sha256:{:x}",
            Sha256::digest(std::fs::read(executable).unwrap())
        );
        let mut view_source = source(MatrixRole::View, 12);
        view_source.buffer_bytes = 752;
        let mut projection_source = source(MatrixRole::Projection, 12);
        projection_source.buffer_bytes = 752;
        projection_source.offset = 64;
        let binding = CameraBinding {
            schema: depth_lab::CAMERA_BINDING_SCHEMA_V1.into(),
            binding_id: "synthetic-warmup".into(),
            binding_version: "1".into(),
            profile_id: "synthetic".into(),
            profile_version: "dm-depth-profile-1".into(),
            executable_sha256: executable_hash.clone(),
            render_width: 1920,
            render_height: 1080,
            view_metres_per_unit: Some("1".into()),
            game_camera: None,
            view_sources: vec![ViewSourceBinding {
                source: view_source,
                translation: TranslationSlot::Row3,
            }],
            projection_sources: vec![projection_source],
        };
        let view_matrix = [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [0.0, 0.0, 0.0, 1.0],
        ];
        let projection_matrix = [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 16.0 / 9.0, 0.0, 0.0],
            [0.0, 0.0, 1.001, 1.0],
            [0.0, 0.0, -0.1, 0.0],
        ];
        let view_bytes = view_matrix
            .into_iter()
            .flatten()
            .flat_map(f32::to_le_bytes)
            .collect::<Vec<_>>();
        let mut exact_bytes = vec![0; 752];
        exact_bytes[..64].copy_from_slice(&view_bytes);
        for (target, byte) in exact_bytes[64..128].iter_mut().zip(
            projection_matrix
                .into_iter()
                .flatten()
                .flat_map(f32::to_le_bytes),
        ) {
            *target = byte;
        }
        let generation = 41;
        let buffer_tag = 0x4200;
        let mut unsupported = binding.clone();
        unsupported.view_sources[0].source.bind_slot = 2;
        let error = init_production(
            root.join("unsupported-distinct-slot"),
            unsupported,
            CameraPoseStreamContext {
                identity: StreamIdentity {
                    session_id: "warmup".into(),
                    run_id: "rejected".into(),
                    generation: generation - 1,
                },
                dsv_id: "main-scene-v1".into(),
            },
            &executable_hash,
            false,
        )
        .unwrap_err();
        assert!(
            error.contains("share one exact logical VS buffer"),
            "{error}"
        );
        init_production(
            root.clone(),
            binding.clone(),
            CameraPoseStreamContext {
                identity: StreamIdentity {
                    session_id: "warmup".into(),
                    run_id: "r1".into(),
                    generation,
                },
                dsv_id: "main-scene-v1".into(),
            },
            &executable_hash,
            false,
        )
        .unwrap();
        begin_production_warmup(generation).unwrap();
        on_main_pass_bound(generation, 100);
        on_draw_with_viewport(
            generation,
            100,
            || 123_456,
            |slot| Ok((slot == 12).then(|| (buffer_tag, exact_bytes.clone()))),
            || panic!("production warmup must not execute discovery viewport queries"),
        );
        let readiness = warmup_present_readiness(generation, 100);
        assert!(readiness.is_ready(), "{readiness:?}");
        assert!(!on_present(generation, 100, true));
        assert_eq!(last_submitted_present_seq(), None);

        arm_production(generation).unwrap();
        on_main_pass_bound(generation, 101);
        on_draw(
            generation,
            101,
            || 123_457,
            |slot| Ok((slot == 12).then(|| (buffer_tag, exact_bytes.clone()))),
        );
        assert!(on_present(generation, 101, true));
        assert!(take_observed_projection(generation, 101).unwrap().is_some());
        let summary = finish_stop_production();
        assert_eq!(summary.pose_row_count, 1);
        assert_eq!(summary.diagnostic_row_count, 0);
        assert_eq!(summary.last_present_seq, Some(101));
        assert!(summary.errors.is_empty(), "{:?}", summary.errors);

        let failure_generation = generation + 1;
        init_production(
            root.join("snapshot-failure"),
            binding,
            CameraPoseStreamContext {
                identity: StreamIdentity {
                    session_id: "warmup".into(),
                    run_id: "snapshot-failure".into(),
                    generation: failure_generation,
                },
                dsv_id: "main-scene-v1".into(),
            },
            &executable_hash,
            false,
        )
        .unwrap();
        on_main_pass_bound(failure_generation, 200);
        on_draw(
            failure_generation,
            200,
            || 123_458,
            |slot| Ok((slot == 12).then(|| (buffer_tag, exact_bytes.clone()))),
        );
        assert!(
            take_observed_projection(failure_generation, 200)
                .unwrap()
                .is_some()
        );
        // Present 200 必须先落地：否则下一次 on_main_pass_bound(201) 会让 production
        // 线程先以「主场景 DSV pass 跨过 Present 边界」fail-closed，与本线程随后的
        // 合成 readback 失败竞争 fail_production 的首错位——谁先到谁被保留，另一条被丢，
        // 本用例就在 CI 上随线程调度红绿不定。这里只考「失败的 exact snapshot 不能复用
        // 上一 Present 的 projection」，边界违规不是它要验的东西。
        assert!(on_present(failure_generation, 200, true));
        on_main_pass_bound(failure_generation, 201);
        on_draw(
            failure_generation,
            201,
            || 123_459,
            |_| Err("synthetic D3D11.1 logical-range readback failure".into()),
        );
        assert!(
            take_observed_projection(failure_generation, 201)
                .unwrap()
                .is_none(),
            "failed exact snapshot must not reuse the previous Present projection"
        );
        let summary = finish_stop_production();
        assert!(
            summary
                .errors
                .iter()
                .any(|error| error.contains("synthetic D3D11.1 logical-range readback failure")),
            "{:?}",
            summary.errors
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn non_evidence_present_writes_direct_pose_without_d3d_events() {
        let _test_guard = CAMERA_TEST_LOCK.lock().unwrap();
        let root =
            std::env::temp_dir().join(format!("dm-camera-held-projection-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let projection_source = source(MatrixRole::Projection, 7);
        let binding = CameraBinding {
            schema: depth_lab::CAMERA_BINDING_SCHEMA_V1.into(),
            binding_id: "synthetic-held-projection".into(),
            binding_version: "1".into(),
            profile_id: "synthetic".into(),
            profile_version: "dm-depth-profile-1".into(),
            executable_sha256: format!("sha256:{}", "1".repeat(64)),
            render_width: 1920,
            render_height: 1080,
            view_metres_per_unit: None,
            game_camera: Some(GameCameraBinding {
                provider: "synthetic-exact-build-camera/v1".into(),
                camera_singleton_relocation: 1,
                camera_root_offset: 2,
                world_transform_offset: 3,
                metres_per_game_unit: "0.0142875".into(),
                axis_mapping: "x_z_y".into(),
            }),
            view_sources: Vec::new(),
            projection_sources: vec![projection_source],
        };
        let identity = StreamIdentity {
            session_id: "held-projection".into(),
            run_id: "r1".into(),
            generation: 1,
        };
        let context = CameraPoseStreamContext {
            identity: identity.clone(),
            dsv_id: "main-scene-v1".into(),
        };
        let assembler =
            CameraPoseAssembler::new(&binding, context, &binding.executable_sha256).unwrap();
        let writer = CameraPoseIndexWriter::new(&root).unwrap();
        let (tx, rx) = sync_channel(8);
        PRODUCTION_FAILED.store(false, Ordering::Release);
        PRODUCTION_LAST_WRITTEN_PRESENT.store(u64::MAX, Ordering::Release);
        let join = std::thread::spawn(move || {
            production_loop(rx, Some(assembler), writer, None, identity)
        });

        let projection: Vec<u8> = [
            [1.0f32, 0.0, 0.0, 0.0],
            [0.0, 16.0 / 9.0, 0.0, 0.0],
            [0.0, 0.0, 1.001, 1.0],
            [0.0, 0.0, -0.1, 0.0],
        ]
        .into_iter()
        .flatten()
        .flat_map(f32::to_le_bytes)
        .collect();
        tx.send(ProductionEvent::BufferUpdate {
            buffer_tag: 0x700,
            bytes: projection,
        })
        .unwrap();
        tx.send(ProductionEvent::VsBind {
            bind_slot: 7,
            buffer: Some(0x700),
        })
        .unwrap();
        tx.send(ProductionEvent::MainScenePass { present_seq: 10 })
            .unwrap();
        tx.send(ProductionEvent::Draw {
            present_seq: 10,
            sample_qpc: 100,
            pass_ordinal: 0,
            draw_ordinal: 0,
        })
        .unwrap();
        tx.send(ProductionEvent::Present {
            present_seq: 10,
            projection_evidence_frame: true,
            camera: Some(Ok(FalloutCameraSample {
                present_seq: 10,
                present_qpc: 101,
                camera_position: [1.0, 2.0, 3.0],
                camera_rotation_quaternion: [0.0, 0.0, 0.0, 1.0],
            })),
            player: None,
        })
        .unwrap();
        tx.send(ProductionEvent::Present {
            present_seq: 11,
            projection_evidence_frame: false,
            camera: Some(Ok(FalloutCameraSample {
                present_seq: 11,
                present_qpc: 102,
                camera_position: [4.0, 5.0, 6.0],
                camera_rotation_quaternion: [0.0, 0.0, 0.0, 1.0],
            })),
            player: None,
        })
        .unwrap();
        drop(tx);
        join.join().unwrap();

        assert_eq!(
            count_lines(&root.join(crate::camera_writer::CAMERA_POSE_INDEX_FILE)).unwrap(),
            2
        );
        assert!(
            std::fs::read_to_string(root.join(crate::camera_writer::CAMERA_POSE_DIAGNOSTIC_FILE))
                .unwrap_or_default()
                .is_empty()
        );
        assert_eq!(PRODUCTION_LAST_WRITTEN_PRESENT.load(Ordering::Acquire), 11);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn production_stop_closes_writer_and_late_present_cannot_change_files() {
        let _test_guard = CAMERA_TEST_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join(format!("dm-camera-lifecycle-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let executable = std::env::current_exe().unwrap();
        let executable_hash = format!(
            "sha256:{:x}",
            Sha256::digest(std::fs::read(executable).unwrap())
        );
        let mut lifecycle_view = source(MatrixRole::View, 1);
        lifecycle_view.buffer_bytes = 128;
        let mut lifecycle_projection = source(MatrixRole::Projection, 1);
        lifecycle_projection.buffer_bytes = 128;
        lifecycle_projection.offset = 64;
        let binding = CameraBinding {
            schema: depth_lab::CAMERA_BINDING_SCHEMA_V1.into(),
            binding_id: "synthetic-lifecycle".into(),
            binding_version: "1".into(),
            profile_id: "synthetic".into(),
            profile_version: "dm-depth-profile-1".into(),
            executable_sha256: executable_hash.clone(),
            render_width: 1920,
            render_height: 1080,
            view_metres_per_unit: Some("1".into()),
            game_camera: None,
            view_sources: vec![ViewSourceBinding {
                source: lifecycle_view,
                translation: TranslationSlot::Row3,
            }],
            projection_sources: vec![lifecycle_projection],
        };
        init_production(
            root.clone(),
            binding.clone(),
            CameraPoseStreamContext {
                identity: StreamIdentity {
                    session_id: "s1".into(),
                    run_id: "r1".into(),
                    generation: 1,
                },
                dsv_id: "main-scene-v1".into(),
            },
            &executable_hash,
            false,
        )
        .unwrap();
        assert!(on_present(1, 0, true));
        let summary = finish_stop_production();
        assert_eq!(summary.pose_row_count, 0);
        assert_eq!(summary.diagnostic_row_count, 1);
        assert!(summary.errors.is_empty(), "{:?}", summary.errors);

        let pose_path = root.join(crate::camera_writer::CAMERA_POSE_INDEX_FILE);
        let diagnostic_path = root.join(crate::camera_writer::CAMERA_POSE_DIAGNOSTIC_FILE);
        let pose_after_stop = std::fs::read(&pose_path).unwrap();
        let diagnostic_after_stop = std::fs::read(&diagnostic_path).unwrap();
        assert!(!on_present(1, 1, true));
        assert_eq!(std::fs::read(&pose_path).unwrap(), pose_after_stop);
        assert_eq!(
            std::fs::read(&diagnostic_path).unwrap(),
            diagnostic_after_stop
        );

        let resumed_root = root.join("resume");
        init_production(
            resumed_root.clone(),
            binding,
            CameraPoseStreamContext {
                identity: StreamIdentity {
                    session_id: "s1".into(),
                    run_id: "r2".into(),
                    generation: 2,
                },
                dsv_id: "main-scene-v1".into(),
            },
            &executable_hash,
            false,
        )
        .unwrap();
        assert!(
            !on_present(1, 1, true),
            "old generation callback must be dropped"
        );
        assert!(on_present(2, 1, true));
        let resumed = finish_stop_production();
        assert_eq!(resumed.pose_row_count, 0);
        assert_eq!(resumed.diagnostic_row_count, 1);
        assert_eq!(resumed.last_present_seq, Some(1));
        assert!(resumed.errors.is_empty(), "{:?}", resumed.errors);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn production_rehydrates_a_persistent_vs_binding_without_a_new_setter_callback() {
        let _test_guard = CAMERA_TEST_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join(format!(
            "dm-camera-persistent-vs-binding-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let executable = std::env::current_exe().unwrap();
        let executable_hash = format!(
            "sha256:{:x}",
            Sha256::digest(std::fs::read(executable).unwrap())
        );
        let mut view_source = source(MatrixRole::View, 7);
        view_source.buffer_bytes = 128;
        let mut projection_source = source(MatrixRole::Projection, 7);
        projection_source.buffer_bytes = 128;
        projection_source.offset = 64;
        let binding = CameraBinding {
            schema: depth_lab::CAMERA_BINDING_SCHEMA_V1.into(),
            binding_id: "synthetic-persistent-vs-binding".into(),
            binding_version: "1".into(),
            profile_id: "synthetic".into(),
            profile_version: "dm-depth-profile-1".into(),
            executable_sha256: executable_hash.clone(),
            render_width: 1920,
            render_height: 1080,
            view_metres_per_unit: Some("1".into()),
            game_camera: None,
            view_sources: vec![ViewSourceBinding {
                source: view_source,
                translation: TranslationSlot::Row3,
            }],
            projection_sources: vec![projection_source],
        };
        let matrices: Vec<u8> = [
            [
                [1.0, 0.0, 0.0, 0.0],
                [0.0, 1.0, 0.0, 0.0],
                [0.0, 0.0, 1.0, 0.0],
                [0.0, 0.0, 0.0, 1.0],
            ],
            [
                [1.0, 0.0, 0.0, 0.0],
                [0.0, 16.0 / 9.0, 0.0, 0.0],
                [0.0, 0.0, 1.001, 1.0],
                [0.0, 0.0, -0.1, 0.0],
            ],
        ]
        .into_iter()
        .flatten()
        .flatten()
        .flat_map(f32::to_le_bytes)
        .collect();
        let buffer_tag = 0xabc_u64;

        for generation in [1, 2] {
            let output = root.join(format!("generation-{generation}"));
            init_production(
                output,
                binding.clone(),
                CameraPoseStreamContext {
                    identity: StreamIdentity {
                        session_id: "persistent-vs-binding".into(),
                        run_id: format!("r{generation}"),
                        generation,
                    },
                    dsv_id: "main-scene-v1".into(),
                },
                &executable_hash,
                false,
            )
            .unwrap();
            if generation == 1 {
                // 第一代先观察到字节；第二代故意既没有更新，也没有 VSSetConstantBuffers，
                // 模拟 D3D11 状态原样跨过新的 StartRun。
                submit(generation, buffer_tag, 0, None, &matrices);
            }
            for present_seq in 0..2 {
                if present_seq == 0 {
                    on_main_pass_bound(generation, present_seq);
                }
                if present_seq == 0 {
                    let vs_slot_reads = std::cell::Cell::new(0);
                    on_vs_constant_buffers(generation, present_seq, 0, 16, |offset| {
                        vs_slot_reads.set(vs_slot_reads.get() + 1);
                        (offset == 7).then_some(buffer_tag)
                    });
                    assert_eq!(
                        vs_slot_reads.get(),
                        1,
                        "production VSSet bookkeeping must extract only the configured slot"
                    );
                }
                let mut queried_slot = None;
                let qpc_calls = std::cell::Cell::new(0);
                on_draw(
                    generation,
                    present_seq,
                    || {
                        qpc_calls.set(qpc_calls.get() + 1);
                        123_456 + present_seq as i64
                    },
                    |slot| {
                        queried_slot = Some(slot);
                        Ok(Some((buffer_tag, matrices.clone())))
                    },
                );
                on_draw(
                    generation,
                    present_seq,
                    || {
                        qpc_calls.set(qpc_calls.get() + 1);
                        i64::MAX
                    },
                    |_| panic!("non-target draw must not query the VS binding"),
                );
                assert_eq!(queried_slot, Some(7));
                assert_eq!(qpc_calls.get(), 1, "only the exact draw may read QPC");
                assert_eq!(
                    current_main_pass_identity(present_seq),
                    Some(MainPassIdentity {
                        pass_ordinal: 0,
                        exact_draw_ordinal: Some(0),
                        projection_observed: true,
                    }),
                    "the outgoing DSV pass must retain the exact projection-draw identity"
                );
                assert!(on_present(generation, present_seq, true));
                let observed = take_observed_projection(generation, present_seq)
                    .unwrap()
                    .expect("exact production draw must retain its projection for depth");
                assert_eq!(observed.a, f64::from(1.001f32));
                assert_eq!(observed.b, f64::from(-0.1f32));
            }
            let summary = finish_stop_production();
            assert_eq!(summary.pose_row_count, 2, "generation {generation}");
            assert_eq!(summary.diagnostic_row_count, 0, "generation {generation}");
            assert!(summary.errors.is_empty(), "{:?}", summary.errors);
        }

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn discovery_phase_reset_rearms_draws_when_the_same_dsv_stays_bound() {
        let _test_guard = CAMERA_TEST_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join(format!(
            "dm-camera-discovery-phase-reset-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);

        init_discovery(root.clone(), discovery_config("A1"), None, 1).unwrap();
        on_main_pass_bound(0, 100);
        on_draw(0, 100, || 1, |_| Ok(None));
        assert!(on_present(0, 100, true));
        stop_discovery().unwrap();

        // 模拟 probe 保留了同一个已验证 DSV：新 phase 与下一 Present 都没有新的
        // OMSetRenderTargets，只有 Draw。Discovery 必须从 Draw 重新建立 pass 时间线。
        init_discovery(root.clone(), discovery_config("A2"), None, 1).unwrap();
        on_draw(0, 101, || 2, |_| Ok(None));
        assert!(on_present(0, 101, true));
        on_draw(0, 102, || 3, |_| Ok(None));
        assert!(on_present(0, 102, true));
        stop_discovery().unwrap();

        let a1 = wait_for_discovery_report(&root.join("camera-candidates-A1.json"));
        assert_eq!(a1.processed_draws, 1);
        assert_eq!(a1.sampling.submitted_draws, 1);
        assert!(a1.reliable, "{:?}", a1.blockers);

        let a2 = wait_for_discovery_report(&root.join("camera-candidates-A2.json"));
        assert_eq!(a2.sampling.observed_presents, 2);
        assert_eq!(a2.sampling.scheduled_presents, 2);
        assert_eq!(a2.sampling.observed_draws, 2);
        assert_eq!(a2.sampling.scheduled_draws, 2);
        assert_eq!(a2.sampling.submitted_draws, 2);
        assert_eq!(a2.processed_draws, 2);
        assert_eq!(a2.sampling.queue_dropped_draws, 0);
        assert_eq!(a2.sampling.runtime_dropped_callbacks, 0);
        assert!(a2.reliable, "{:?}", a2.blockers);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn every_raster_draw_entry_shares_one_ordinal_and_discovery_ledger() {
        let _guard = CAMERA_TEST_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join(format!(
            "dm-camera-all-draw-entrypoints-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let mut config = discovery_config("all-draw-entrypoints");
        config.draw_window_count = CameraDrawMethod::ALL.len() as u32;
        init_discovery(root.clone(), config, None, 1).unwrap();
        on_main_pass_bound(0, 700);

        for (offset, method) in CameraDrawMethod::ALL.into_iter().enumerate() {
            assert!(dispatch_camera_draw_after_gate(
                method,
                0,
                700,
                || offset as i64,
                true,
            ));
        }
        assert_eq!(
            CURRENT_DRAW_ORDINAL.load(Ordering::Acquire),
            CameraDrawMethod::ALL.len() as u32
        );
        assert!(on_present(0, 700, true));

        stop_discovery().unwrap();
        let report =
            wait_for_discovery_report(&root.join("camera-candidates-all-draw-entrypoints.json"));
        assert_eq!(report.sampling.observed_draws, 7);
        assert_eq!(report.sampling.scheduled_draws, 7);
        assert_eq!(report.sampling.submitted_draws, 7);
        assert_eq!(report.processed_draws, 7);
        assert!(report.sampling.queue_dropped_draws == 0);
        assert!(report.sampling.runtime_dropped_callbacks == 0);
        let _ = std::fs::remove_dir_all(root);
    }
}

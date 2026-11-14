//! 深度采集在客户端一侧的接线。
//!
//! 整个模块只在 `depth-lab` cargo feature 下编译；线上口味显式开启，默认
//! Cargo 构建仍不携带。能否实际采集继续由签名 catalog 与运行时门禁决定。
//!
//! # 这里刻意只有胶水
//!
//! 门控、性能认证、控制协议与 ACK 状态机都放在可独立测试的 `depth-lab` crate；
//! 客户端这里只组装真实运行事实并驱动协议，避免把第二套判断塞进 orchestrator。
//!
//! 所以这里只做四件事：
//!
//! 1. 记住采集员这次运行有没有打开开关；
//! 2. 把散落各处的事实收集成一个 [`GateFacts`]，交给 `evaluate_gates`；
//! 3. 发 `StartRun` 并等待真实 `Started -> Ready` 后才放行正式录制；
//! 4. 所有终止路径发 `StopRun`，只接受排空 writer 后的真实 `Flushed`。

use std::fs::OpenOptions;
use std::io::Write;
use std::os::windows::fs::MetadataExt;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use depth_lab::archive_contract::ArchiveLane;
use depth_lab::certification::{
    DEPTH_PERF_CERTIFICATION_CANDIDATE_SCHEMA_V1, DepthProbeRuntimeManifest, DeviceProfileContext,
    PerfCertificationCandidate, PerfCertificationDenial, PerfCertificationKey,
    PerfHardwareIdentity, evaluate_device_profile_certification, normalize_gpu_model,
};
use depth_lab::client_archive::{SegmentUploadRequest, materialize_segment_upload};
use depth_lab::integration::{
    DepthToggle, GateDenial, GateFacts, MAX_AVG_FPS_DROP, MAX_LOW1_FPS_DROP, evaluate_gates,
};
use depth_lab::pairing::FramePair;
use depth_lab::perf_run::{PerfRun, PerfRunOutcome, Stage, StageMeasurement};
use depth_lab::pipeline::E2eReport;
use depth_lab::probe_control::{
    AckKind, CONTROL_SCHEMA_V3, CameraBindingIdentity, ClientLifecycle, ClientLifecycleState,
    ControlCommand, ControlEnvelope, DepthProfileIdentity, GpuAdapterIdentity, StartRun, StopRun,
};
use depth_lab::segment_finalize::{
    FinalizeRequest, FrameStateEvidence, ProbeFlushedEvidence, PublishedDepthSegment,
    SegmentBoundary, StreamIdentity, VerifiedPairingEvidence, finalize_segments,
};
use sha2::{Digest as _, Sha256};
use windows::Win32::System::Threading::CREATE_NO_WINDOW;

use crate::platform::windows::catalog::CatalogSelection;
use crate::services::depth_certification_delivery::{
    certification_candidate_directory, load_active_manifest,
};
use crate::services::early_launch::{
    EarlyLaunchTarget, EarlyLauncherMode, EarlyLauncherRequest, parse_early_launcher_output,
};
use crate::services::journal::{DepthSegmentCommit, SessionJournal};

/// 采集员开关。**进程内状态，不落盘**——这就是「每次启动重置为关」的实现方式
/// （77 号 §6.3）。落盘会带来「上次开着这次自动开着」，而这个功能不该有那种记忆。
static TOGGLE: Mutex<DepthToggle> = Mutex::new(DepthToggle {
    enabled_this_run: false,
    risk_acknowledged: false,
});

pub const PROBE_VERSION: &str = env!("CARGO_PKG_VERSION");
const MIN_START_TIMEOUT_MS: u64 = 15_000;
const PROBE_RUNTIME_MANIFEST_FILE: &str = "dm-depth-probe-runtime.json";
const MAX_PROBE_RUNTIME_MANIFEST_BYTES: u64 = 64 * 1024;

const fn certification_stage_requires_selected_present_feedback(
    certification_stage: Option<Stage>,
) -> bool {
    !matches!(
        certification_stage,
        Some(Stage::Baseline | Stage::WatermarkOnly)
    )
}

const fn certification_stage_requires_present_identity(certification_stage: Option<Stage>) -> bool {
    !matches!(certification_stage, Some(Stage::Baseline))
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DepthProbeSettings {
    pub enabled: bool,
    pub risk_acknowledged: bool,
    pub unbind_from_end: usize,
    pub planned_seconds: u64,
    pub start_timeout_ms: u64,
    pub stop_timeout_ms: u64,
    /// 仅为当前 backend generation + 配置 key 采集候选证据；run 永久不可交付，
    /// candidate 也不能在本机直接变成有效认证。
    #[serde(default)]
    pub performance_validation_only: bool,
    /// Enables the A3 per-Present timing rings for this run. This is carried
    /// through StartRun and must be echoed by Ready and Flushed.
    #[serde(default)]
    pub performance_metrics_enabled: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DepthProbeRuntimeStatus {
    pub configured: bool,
    pub enabled: bool,
    pub risk_acknowledged: bool,
    pub lifecycle: ClientLifecycleState,
    pub active_identity: Option<depth_lab::StreamIdentity>,
    pub last_error: Option<String>,
    pub performance_validation_only: bool,
    pub performance_metrics_enabled: bool,
    pub early_launch_required: bool,
    /// Only the explicit diagnostic build can offer the CP2077 local short run.
    pub cp2077_local_validation_available: bool,
    pub early_launch_game_id: Option<String>,
    pub early_launch_state: EarlyLaunchState,
    pub early_launch_pid: Option<u32>,
    pub discovery_available: bool,
    pub discovery_state: DiscoverySmokeState,
    pub discovery_pid: Option<u32>,
    pub discovery_output_locator: Option<String>,
    pub discovery_last_error: Option<String>,
    /// Discovery evidence is deliberately incapable of turning this true.
    pub discovery_recording_ready: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EarlyLaunchState {
    #[default]
    NotRequired,
    AwaitingLaunch,
    Launching,
    WaitingForWindow,
    Bound,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiscoverySmokeState {
    #[default]
    Idle,
    Preparing,
    HooksArmed,
    WaitingObservations,
    Sealing,
    ReportSealed,
    Faulted,
}

#[derive(Debug, Clone)]
struct EarlyLaunchRecord {
    target: EarlyLaunchTarget,
    process_id: u32,
    executable_sha256: String,
    depth_profile_sha256: String,
    resident_probe_sha256: String,
    launch_id: String,
}

#[derive(Debug, Clone)]
struct ConfiguredDepthProfile {
    game_id: String,
    renderer_backend: depth_lab::RendererBackend,
    depth_profile_sha256: String,
    early_launch_target: Option<EarlyLaunchTarget>,
}

#[derive(Debug, Clone)]
struct DiscoverySmokeRecord {
    process_id: u32,
    executable_sha256: String,
    resident_probe_sha256: String,
    launch_id: String,
    output_dir: PathBuf,
    output_locator: String,
}

#[derive(Default)]
struct Runtime {
    settings: Option<DepthProbeSettings>,
    lifecycle: ClientLifecycle,
    last_error: Option<String>,
    configured_game_id: Option<String>,
    configured_renderer_backend: Option<depth_lab::RendererBackend>,
    configured_depth_profile_sha256: Option<String>,
    configured_early_launch_target: Option<EarlyLaunchTarget>,
    early_launch_state: EarlyLaunchState,
    early_launch: Option<EarlyLaunchRecord>,
    discovery_state: DiscoverySmokeState,
    discovery: Option<DiscoverySmokeRecord>,
    discovery_last_error: Option<String>,
    perf_certification: Option<ClientPerfCertification>,
}

#[derive(Debug)]
struct ClientPerfCertification {
    id: String,
    run: PerfRun,
    key: Option<PerfCertificationKey>,
    runtime_manifest: Option<DepthProbeRuntimeManifest>,
    hardware: Option<PerfHardwareIdentity>,
    outcome: Option<PerfRunOutcome>,
    candidate_path: Option<PathBuf>,
    completion_error: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LatencyQuantilesIpc {
    pub p50: u64,
    pub p95: u64,
    pub p99: u64,
    pub max: u64,
}

impl From<depth_lab::LatencyQuantiles> for LatencyQuantilesIpc {
    fn from(value: depth_lab::LatencyQuantiles) -> Self {
        Self {
            p50: value.p50,
            p95: value.p95,
            p99: value.p99,
            max: value.max,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WriterTimingSummaryIpc {
    pub sample_count: u64,
    pub total_committed: u64,
    pub wrapped: bool,
    pub over_budget_count: u64,
    pub duration_us: LatencyQuantilesIpc,
}

impl From<depth_lab::WriterTimingSummary> for WriterTimingSummaryIpc {
    fn from(value: depth_lab::WriterTimingSummary) -> Self {
        Self {
            sample_count: value.sample_count,
            total_committed: value.total_committed,
            wrapped: value.wrapped,
            over_budget_count: value.over_budget_count,
            duration_us: value.duration_us.into(),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PerfRunOutcomeIpc {
    pub avg_fps_drop: f64,
    pub low1_fps_drop: f64,
    pub watermark_avg_drop: f64,
    pub readback_avg_drop: f64,
    pub writer_avg_drop: f64,
    pub watermark_low1_drop: f64,
    pub readback_low1_drop: f64,
    pub writer_low1_drop: f64,
    pub readback_only_bypassed_frames: u64,
    pub full_writer_timing: WriterTimingSummaryIpc,
    pub max_avg_fps_drop: f64,
    pub max_low1_fps_drop: f64,
    pub passes: bool,
}

impl From<&PerfRunOutcome> for PerfRunOutcomeIpc {
    fn from(outcome: &PerfRunOutcome) -> Self {
        Self {
            avg_fps_drop: outcome.avg_fps_drop,
            low1_fps_drop: outcome.low1_fps_drop,
            watermark_avg_drop: outcome.watermark_avg_drop,
            readback_avg_drop: outcome.readback_avg_drop,
            writer_avg_drop: outcome.writer_avg_drop,
            watermark_low1_drop: outcome.watermark_low1_drop,
            readback_low1_drop: outcome.readback_low1_drop,
            writer_low1_drop: outcome.writer_low1_drop,
            readback_only_bypassed_frames: outcome.readback_only_bypassed_frames,
            full_writer_timing: outcome.full_writer_timing.into(),
            max_avg_fps_drop: MAX_AVG_FPS_DROP,
            max_low1_fps_drop: MAX_LOW1_FPS_DROP,
            passes: outcome.passes,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PerfCertificationStatus {
    pub active: bool,
    pub certification_id: Option<String>,
    pub next_stage: Option<Stage>,
    pub outcome: Option<PerfRunOutcomeIpc>,
    pub candidate_path: Option<String>,
    pub completion_error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PerfCertificationGateState {
    Passed,
    BaselineMatched,
    Blocked,
}

/// 当前绑定目标能否复用线上签名性能认证。
///
/// 这是只读预检结果：不创建 session/run，也不把本机候选证据晋升成认证。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PerfCertificationGateStatus {
    pub state: PerfCertificationGateState,
    pub certification_id: Option<String>,
    pub gpu_model: String,
    pub runtime_version: String,
    pub renderer_backend: depth_lab::RendererBackend,
    pub backend_generation: u64,
    pub detail: String,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PerfStageMeasurementRequest {
    pub certification_id: String,
    pub stage: Stage,
    pub target_fps: f64,
}

static RUNTIME: OnceLock<Mutex<Runtime>> = OnceLock::new();
static NEXT_GENERATION: OnceLock<AtomicU64> = OnceLock::new();

fn runtime() -> &'static Mutex<Runtime> {
    RUNTIME.get_or_init(|| Mutex::new(Runtime::default()))
}

pub fn begin_perf_certification() -> Result<PerfCertificationStatus, String> {
    let mut guard = runtime().lock().map_err(|_| "depth runtime 锁已中毒")?;
    if !matches!(
        guard.lifecycle.state(),
        ClientLifecycleState::Idle | ClientLifecycleState::Flushed | ClientLifecycleState::Faulted
    ) {
        return Err("depth run 活跃时不能开始性能认证；请先等待 Flushed".into());
    }
    let id = uuid::Uuid::new_v4().simple().to_string();
    guard.perf_certification = Some(ClientPerfCertification {
        run: PerfRun::begin(id.clone()),
        id,
        key: None,
        runtime_manifest: None,
        hardware: None,
        outcome: None,
        candidate_path: None,
        completion_error: None,
    });
    drop(guard);
    perf_certification_status()
}

pub fn perf_certification_status() -> Result<PerfCertificationStatus, String> {
    let guard = runtime().lock().map_err(|_| "depth runtime 锁已中毒")?;
    let current = guard.perf_certification.as_ref();
    let candidate_path = current
        .and_then(|run| run.candidate_path.clone())
        .map(|path| path.to_string_lossy().into_owned());
    Ok(PerfCertificationStatus {
        active: current.is_some_and(|run| run.run.next_stage().is_some()),
        certification_id: current.map(|run| run.id.clone()),
        next_stage: current.and_then(|run| run.run.next_stage()),
        outcome: current.and_then(|run| run.outcome.as_ref().map(PerfRunOutcomeIpc::from)),
        candidate_path,
        completion_error: current.and_then(|run| run.completion_error.clone()),
    })
}

pub fn perf_certification_blocks_generic_start(collector_paused: bool) -> Result<bool, String> {
    let guard = runtime().lock().map_err(|_| "depth runtime 锁已中毒")?;
    Ok(guard.perf_certification.as_ref().is_some_and(|run| {
        (run.outcome.is_none() && run.run.next_stage().is_some())
            || (collector_paused && run.run.next_stage().is_none())
    }))
}

/// Validate the backend-owned certification stage immediately before capture starts.
///
/// Generic start/resume controls are deliberately kept outside this path: otherwise a
/// paused certification can create a new run without advancing the stage timer or binding
/// the measurement to the new run identity.
pub fn prepare_perf_certification_stage_start() -> Result<Stage, String> {
    let guard = runtime().lock().map_err(|_| "depth runtime 锁已中毒")?;
    let certification = guard
        .perf_certification
        .as_ref()
        .filter(|run| run.outcome.is_none())
        .ok_or_else(|| "没有进行中的性能认证".to_string())?;
    let stage = certification
        .run
        .next_stage()
        .ok_or_else(|| "性能认证四档已经完成".to_string())?;
    let settings = guard
        .settings
        .as_ref()
        .ok_or_else(|| "性能认证档尚未配置".to_string())?;
    if !settings.performance_validation_only {
        return Err("性能认证档必须使用 performance-validation-only 配置".into());
    }
    if !settings.enabled || !settings.risk_acknowledged || !settings.performance_metrics_enabled {
        return Err("性能认证档配置与当前阶段不一致，请从认证卡片重新启动本档".into());
    }
    Ok(stage)
}

pub fn configured_performance_validation_only() -> Result<bool, String> {
    let guard = runtime().lock().map_err(|_| "depth runtime 锁已中毒")?;
    Ok(guard
        .settings
        .as_ref()
        .is_some_and(|settings| settings.performance_validation_only))
}

pub fn cancel_perf_certification() -> Result<PerfCertificationStatus, String> {
    let mut guard = runtime().lock().map_err(|_| "depth runtime 锁已中毒")?;
    if matches!(
        guard.lifecycle.state(),
        ClientLifecycleState::Starting
            | ClientLifecycleState::Started
            | ClientLifecycleState::Ready
            | ClientLifecycleState::Stopping
            | ClientLifecycleState::Stopped
    ) {
        return Err("性能认证档仍在运行；请先等待录制完全暂停和 probe Flushed".into());
    }
    guard.perf_certification = None;
    clear_performance_validation_settings(&mut guard);
    drop(guard);
    reset();
    perf_certification_status()
}

fn measurement_from_probe_flush(
    request: &PerfStageMeasurementRequest,
    summary: &depth_lab::ProbeFlushSummary,
) -> Result<StageMeasurement, String> {
    summary.validate().map_err(|error| error.to_string())?;
    if !request.target_fps.is_finite() || request.target_fps <= 0.0 {
        return Err("性能认证目标 FPS 必须是正有限数".into());
    }
    let timing = summary
        .present_timing
        .as_ref()
        .ok_or_else(|| format!("{:?} 档缺少 probe Present QPC 证据", request.stage))?;
    if timing.wrapped {
        return Err(format!(
            "{:?} 档的 Present 计时环发生覆盖，五分钟窗口不完整，整轮不可采信",
            request.stage
        ));
    }
    let effective_measurement_cutoff = timing.measurement_cutoff_qpc.min(timing.stop_cutoff_qpc);
    let measured_ticks = effective_measurement_cutoff
        .saturating_sub(timing.measurement_window_start_qpc)
        .max(0);
    let seconds = measured_ticks as f64 / timing.qpc_frequency as f64;
    let avg_fps = timing.avg_millifps as f64 / 1_000.0;
    let low1_fps = timing.low1_millifps as f64 / 1_000.0;
    let frame_capped = request.stage != Stage::Baseline || avg_fps >= request.target_fps * 0.9;
    let (writer_bypassed_frames, writer_timing) = match request.stage {
        Stage::Baseline | Stage::WatermarkOnly => {
            if summary.writer_bypass {
                return Err(format!(
                    "{:?} 档错误地回显 writer_bypass，整轮不可采信",
                    request.stage
                ));
            }
            (0, None)
        }
        Stage::ReadbackOnly => {
            if !summary.writer_bypass {
                return Err("readback-only 档没有回显 writer_bypass，整轮不可采信".into());
            }
            (summary.writer_bypassed_frames, summary.writer_timing)
        }
        Stage::Full => {
            if summary.writer_bypass {
                return Err("完整档错误地回显 writer_bypass，整轮不可采信".into());
            }
            (0, summary.writer_timing)
        }
    };
    Ok(StageMeasurement {
        stage: request.stage,
        avg_fps,
        low1_fps,
        seconds,
        frame_capped,
        writer_bypassed_frames,
        writer_timing,
    })
}

pub fn record_perf_stage(
    request: PerfStageMeasurementRequest,
) -> Result<PerfCertificationStatus, String> {
    let mut guard = runtime().lock().map_err(|_| "depth runtime 锁已中毒")?;
    let certification = guard
        .perf_certification
        .as_ref()
        .ok_or_else(|| "没有进行中的性能认证".to_string())?;
    if certification.id != request.certification_id {
        return Err("性能认证身份已变化，请刷新当前状态；未修改已有结果".into());
    }
    if certification.run.next_stage().is_some() {
        let summary = guard
            .lifecycle
            .confirmed_flush()
            .ok_or_else(|| format!("{:?} 档缺少 probe Flushed 证据", request.stage))?;
        let measurement = measurement_from_probe_flush(&request, summary)?;
        guard
            .perf_certification
            .as_mut()
            .expect("certification checked under the same lock")
            .run
            .record(&request.certification_id, measurement)
            .map_err(|error| format!("性能认证本档被拒绝: {error:?}"))?;
    } else if request.stage != Stage::Full {
        return Err("四档已经测完，只能重试结果导出，不能重复登记旧档".into());
    }
    let certification = guard.perf_certification.as_mut().expect("checked above");
    if certification.run.next_stage().is_none() {
        // Keep the fourth measurement on failure. A finalization retry must never
        // start another capture or append a fifth measurement.
        complete_perf_certification(certification, write_perf_certification_candidate)?;
        clear_performance_validation_settings(&mut guard);
        reset();
    }
    drop(guard);
    perf_certification_status()
}

fn complete_perf_certification(
    certification: &mut ClientPerfCertification,
    write_candidate: impl FnOnce(PerfCertificationCandidate) -> Result<PathBuf, String>,
) -> Result<(), String> {
    if certification.candidate_path.is_some() {
        return Ok(());
    }
    let result = (|| {
        let outcome = certification
            .run
            .finish()
            .map_err(|error| format!("性能认证结果无效，需重新完成四档: {error:?}"))?;
        // Preserve the measured result even when exporting it fails.
        certification.outcome = Some(outcome.clone());
        let candidate_path = write_candidate(PerfCertificationCandidate {
            schema: DEPTH_PERF_CERTIFICATION_CANDIDATE_SCHEMA_V1.to_string(),
            candidate_id: certification.id.clone(),
            runtime: certification.runtime_manifest.clone().ok_or_else(|| {
                "性能认证没有取得 probe runtime manifest，整轮不可导出".to_string()
            })?,
            key: certification
                .key
                .clone()
                .ok_or_else(|| "性能认证没有取得完整 generation 身份，整轮不可导出".to_string())?,
            hardware: certification
                .hardware
                .clone()
                .ok_or_else(|| "性能认证没有取得硬件审计身份，整轮不可导出".to_string())?,
            avg_fps_drop: outcome.avg_fps_drop,
            low1_fps_drop: outcome.low1_fps_drop,
            measured_on: time::OffsetDateTime::now_utc().date().to_string(),
        })?;
        certification.candidate_path = Some(candidate_path);
        Ok(())
    })();
    certification.completion_error = result.as_ref().err().cloned();
    result
}

fn write_perf_certification_candidate(
    candidate: PerfCertificationCandidate,
) -> Result<PathBuf, String> {
    candidate.validate()?;
    let directory = certification_candidate_directory()?;
    std::fs::create_dir_all(&directory).map_err(|error| {
        format!(
            "创建性能认证候选证据目录 {} 失败: {error}",
            directory.display()
        )
    })?;
    let path = directory.join(format!("{}.json", candidate.candidate_id));
    let bytes = serde_json::to_vec_pretty(&candidate)
        .map_err(|error| format!("序列化性能认证候选证据失败: {error}"))?;
    write_create_new(&path, &bytes, "performance certification candidate")?;
    Ok(path)
}

fn clear_performance_validation_settings(runtime: &mut Runtime) {
    if let Some(settings) = runtime.settings.as_mut() {
        settings.enabled = false;
        settings.risk_acknowledged = false;
        settings.performance_validation_only = false;
        settings.performance_metrics_enabled = false;
    }
}

fn preserves_verified_early_launch_for_performance_reconfigure(
    runtime: &Runtime,
    settings: &DepthProbeSettings,
    configured_profile: Option<&ConfiguredDepthProfile>,
) -> bool {
    let Some(profile) = configured_profile else {
        return false;
    };
    let Some(record) = runtime.early_launch.as_ref() else {
        return false;
    };
    settings.enabled
        && settings.performance_validation_only
        && runtime
            .perf_certification
            .as_ref()
            .is_some_and(|certification| certification.outcome.is_none())
        && matches!(
            runtime.early_launch_state,
            EarlyLaunchState::WaitingForWindow | EarlyLaunchState::Bound
        )
        && runtime.configured_game_id.as_deref() == Some(profile.game_id.as_str())
        && runtime.configured_renderer_backend == Some(profile.renderer_backend)
        && runtime
            .configured_depth_profile_sha256
            .as_deref()
            .is_some_and(|hash| hash.eq_ignore_ascii_case(&profile.depth_profile_sha256))
        && runtime.configured_early_launch_target == profile.early_launch_target
        && profile.early_launch_target == Some(record.target)
        && record.target.game_id() == profile.game_id
        && record
            .depth_profile_sha256
            .eq_ignore_ascii_case(&profile.depth_profile_sha256)
}

pub fn next_generation() -> Result<u64, String> {
    let counter = if let Some(counter) = NEXT_GENERATION.get() {
        counter
    } else {
        let seed = u64::try_from(current_qpc()?).map_err(|_| {
            "QueryPerformanceCounter returned a negative depth generation".to_string()
        })?;
        NEXT_GENERATION.get_or_init(|| AtomicU64::new(seed))
    };
    counter
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
            value.checked_add(1)
        })
        .map_err(|_| "depth generation 已耗尽；必须重启客户端".to_string())
}

pub fn configure(
    settings: DepthProbeSettings,
    selection: Option<&CatalogSelection>,
) -> Result<DepthProbeRuntimeStatus, String> {
    if settings.enabled && !settings.risk_acknowledged {
        return Err("开启 depth 必须显式确认注入与账号风险".into());
    }
    if settings.performance_metrics_enabled && !settings.performance_validation_only {
        return Err("A3 分项计时只能在性能校准模式中启用；该 run 永久不可交付".into());
    }
    if settings.enabled {
        if settings.planned_seconds == 0 {
            return Err("plannedSeconds 必须大于 0".into());
        }
        if !(MIN_START_TIMEOUT_MS..=60_000).contains(&settings.start_timeout_ms) {
            return Err(format!(
                "start timeout 必须在 {MIN_START_TIMEOUT_MS}..=60000 ms；需要覆盖 probe 自动热身与 Faulted 收口"
            ));
        }
        if !(500..=60_000).contains(&settings.stop_timeout_ms) {
            return Err("stop timeout 必须在 500..=60000 ms".into());
        }
    }
    let configured_profile = if settings.enabled {
        let selection =
            selection.ok_or_else(|| "depth 已开启但没有已选游戏 catalog".to_string())?;
        // Window discovery may deliberately omit an executable pin for a
        // non-trainable DEV lane. Depth is stricter: use the exact identity in
        // the depth profile itself to validate the catalog pair here, then use
        // the bound process's independently observed hash again at start_run.
        let configured_depth = selection
            .depth_profile
            .as_ref()
            .ok_or_else(|| "已选游戏没有 depth profile，不能启用 depth".to_string())?;
        let pinned_profile: depth_lab::DepthProfile =
            serde_json::from_slice(configured_depth.source_bytes())
                .map_err(|error| format!("parse depth profile failed: {error}"))?;
        let (profile_bytes, _) = resolve_catalog_depth(
            selection,
            &pinned_profile.game_id,
            &pinned_profile.executable_sha256,
        )?;
        let profile: depth_lab::DepthProfile = serde_json::from_slice(&profile_bytes)
            .map_err(|error| format!("parse depth profile failed: {error}"))?;
        let depth_profile_sha256 = sha256_bytes(&profile_bytes);
        let early_launch_target = resolve_formal_early_launch_target(
            profile.renderer_backend,
            &profile.game_id,
            &profile.executable_sha256,
        )?;
        Some(ConfiguredDepthProfile {
            game_id: profile.game_id,
            renderer_backend: profile.renderer_backend,
            depth_profile_sha256,
            early_launch_target,
        })
    } else {
        None
    };
    {
        let guard = runtime().lock().map_err(|_| "depth runtime 锁已中毒")?;
        if matches!(
            guard.lifecycle.state(),
            ClientLifecycleState::Starting
                | ClientLifecycleState::Started
                | ClientLifecycleState::Ready
                | ClientLifecycleState::Stopping
                | ClientLifecycleState::Stopped
        ) {
            return Err("depth run 活跃时不能改配置；请先等待 Flushed".into());
        }
        if settings.enabled
            && matches!(
                guard.discovery_state,
                DiscoverySmokeState::Preparing
                    | DiscoverySmokeState::HooksArmed
                    | DiscoverySmokeState::WaitingObservations
                    | DiscoverySmokeState::Sealing
            )
        {
            return Err(
                "CP2077 discovery-only smoke is active; it cannot be promoted to formal Depth"
                    .into(),
            );
        }
    }
    set_enabled(settings.enabled, settings.risk_acknowledged);
    let mut guard = runtime().lock().map_err(|_| "depth runtime 锁已中毒")?;
    let preserve_early_launch = preserves_verified_early_launch_for_performance_reconfigure(
        &guard,
        &settings,
        configured_profile.as_ref(),
    );
    guard.settings = Some(settings);
    guard.last_error = None;
    guard.configured_game_id = configured_profile
        .as_ref()
        .map(|value| value.game_id.clone());
    guard.configured_renderer_backend = configured_profile
        .as_ref()
        .map(|value| value.renderer_backend);
    guard.configured_depth_profile_sha256 = configured_profile
        .as_ref()
        .map(|value| value.depth_profile_sha256.clone());
    guard.configured_early_launch_target = configured_profile
        .as_ref()
        .and_then(|value| value.early_launch_target);
    if !preserve_early_launch {
        guard.early_launch = None;
        guard.early_launch_state = if guard.configured_early_launch_target.is_some() {
            EarlyLaunchState::AwaitingLaunch
        } else {
            EarlyLaunchState::NotRequired
        };
    }
    drop(guard);
    Ok(runtime_status())
}

/// 探针在录制中自行中止的通知（130 号 §6.5）。
///
/// 两路文案都带上：`annotator` 进 UI 通知，`diagnostic` 进诊断包与日志。
/// 分开的理由见 `depth_lab::health_abort` 的模块文档。
#[derive(Debug)]
pub struct HealthAbortNotice {
    pub annotator: String,
    pub diagnostic: String,
}

/// 录制期轮询探针有没有因为连续异常自行中止。
///
/// # 为什么是只读的
///
/// 这条轮询与启动/停止那两条**共用同一份 ACK 目录**，但**不推进**
/// [`ClientLifecycle`]：推进就要占 ordinal，而停止路径的 `wait_for_state`
/// 正靠 ordinal 逐条消费。轮询只是「看一眼有没有那张 Faulted」，
/// 真正的状态推进仍然只发生在停止路径上——那条路已经有测试盯着。
///
/// # 为什么只认这一个 code
///
/// 探针的其它 Fault（peer 退出、warmup 超时）要么发生在录制开始之前，要么
/// 本来就会在收口时被 `stop_and_flush` 抓住。**这里只负责把「烧到采集结束」
/// 这一种失败提前掐掉**，不顺手改别的失败的处置——那些没有对应的真机验证。
pub fn poll_health_abort(output_dir: &Path) -> Option<HealthAbortNotice> {
    let acks = depth_lab::probe_control::read_acks(output_dir).ok()?;
    let faulted = acks.iter().find(|ack| {
        matches!(
            &ack.ack,
            AckKind::Faulted { code, .. } if code == depth_lab::health_abort::HEALTH_ABORT_FAULT_CODE
        )
    })?;
    let AckKind::Faulted { message, .. } = &faulted.ack else {
        unreachable!("matched Faulted above")
    };
    // 采集员文案优先取探针落盘的那份记录（带真实连续帧数）。记录读不到时
    // **不编一个数字**，退回一句不带数字的话，诊断仍然走 ACK 上的原文。
    let record = std::fs::read_to_string(
        output_dir
            .join(depth_lab::health_abort::SCENE_DIR)
            .join(depth_lab::health_abort::SCENE_RECORD),
    )
    .ok()
    .and_then(|text| serde_json::from_str::<depth_lab::health_abort::HealthAbort>(&text).ok());
    Some(HealthAbortNotice {
        annotator: record.as_ref().map_or_else(
            || {
                "深度采集已停止——连续多帧没有采到有效深度。\
                 请截图本页并联系工程，先不要继续录制。\
                 本次录制到停止为止的画面与操作已经保存。"
                    .to_string()
            },
            depth_lab::health_abort::HealthAbort::annotator_message,
        ),
        diagnostic: message.clone(),
    })
}

#[must_use]
pub fn runtime_status() -> DepthProbeRuntimeStatus {
    let toggle = *TOGGLE.lock().expect("depth toggle lock poisoned");
    let guard = runtime().lock().expect("depth runtime lock poisoned");
    DepthProbeRuntimeStatus {
        configured: guard.settings.is_some(),
        enabled: toggle.enabled_this_run,
        risk_acknowledged: toggle.risk_acknowledged,
        lifecycle: guard.lifecycle.state(),
        active_identity: guard.lifecycle.identity().cloned(),
        last_error: guard.last_error.clone(),
        performance_validation_only: guard
            .settings
            .as_ref()
            .is_some_and(|settings| settings.performance_validation_only),
        performance_metrics_enabled: guard
            .settings
            .as_ref()
            .is_some_and(|settings| settings.performance_metrics_enabled),
        early_launch_required: guard.configured_early_launch_target.is_some(),
        // The ~30 s non-deliverable short test is an engineering tool; collectors only see the
        // formal controls.
        cp2077_local_validation_available: cfg!(all(
            feature = "cp2077-diagnostics",
            debug_assertions
        )),
        early_launch_game_id: guard
            .configured_early_launch_target
            .map(|target| target.game_id().to_owned()),
        early_launch_state: guard.early_launch_state,
        early_launch_pid: guard.early_launch.as_ref().map(|launch| launch.process_id),
        discovery_available: cp2077_discovery_available(),
        discovery_state: guard.discovery_state,
        discovery_pid: guard.discovery.as_ref().map(|run| run.process_id),
        discovery_output_locator: guard
            .discovery
            .as_ref()
            .map(|run| run.output_locator.clone()),
        discovery_last_error: guard.discovery_last_error.clone(),
        discovery_recording_ready: false,
    }
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EarlyLaunchResult {
    pub process_id: u32,
    pub state: EarlyLaunchState,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoverySmokeResult {
    pub state: DiscoverySmokeState,
    pub process_id: Option<u32>,
    pub output_locator: Option<String>,
    pub recording_ready: bool,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DiscoverySmokeStartRequest {
    pub risk_acknowledged: bool,
    pub game_id: String,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EarlyLaunchPreflight {
    pub eligible: bool,
    pub ready: bool,
    pub executable_hash_verified: bool,
    pub launcher_present: bool,
    pub launcher_sha256: Option<String>,
    pub probe_present: bool,
    pub probe_sha256: Option<String>,
    pub blockers: Vec<String>,
}

const MAX_STEAM_VDF_BYTES: u64 = 2 * 1024 * 1024;
const MAX_APP_MANIFEST_BYTES: u64 = 256 * 1024;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
const DISCOVERY_CONFIG_SCHEMA: &str = "dm-cp2077-discovery-smoke-config/v1";
const D3D12_DISCOVERY_REPORT_SCHEMA: &str = "dm-d3d12-discovery-observation/v1";
const POSE_DISCOVERY_REPORT_SCHEMA: &str = "cp2077-pose-runtime-observation/v1";
const DISCOVERY_SEALED_SCHEMA: &str = "dm-cp2077-discovery-report-sealed/v1";
const DISCOVERY_STOP_SCHEMA: &str = "dm-cp2077-discovery-stop/v1";
const DISCOVERY_CONFIG_FILE: &str = "discovery-config.json";
const DISCOVERY_STOP_FILE: &str = "discovery-stop-request.json";
const D3D12_DISCOVERY_REPORT_FILE: &str = "d3d12-observation.json";
const POSE_DISCOVERY_REPORT_FILE: &str = "cp2077-pose-runtime-observation.json";
const DISCOVERY_SEALED_FILE: &str = "discovery-report-sealed.json";
const DISCOVERY_REPORT_MAX_BYTES: u64 = 4 * 1024 * 1024;

#[must_use]
const fn cp2077_discovery_available() -> bool {
    cfg!(feature = "depth-lab") && !crate::build_flavor::uses_delivered_catalog()
}

pub fn preflight_cp2077_discovery() -> EarlyLaunchPreflight {
    if !cp2077_discovery_available() {
        return EarlyLaunchPreflight {
            eligible: false,
            ready: false,
            executable_hash_verified: false,
            launcher_present: false,
            launcher_sha256: None,
            probe_present: false,
            probe_sha256: None,
            blockers: vec!["CP2077 local discovery smoke is not available in this build".into()],
        };
    }
    let tools_dir = default_tools_dir();
    build_early_launch_preflight(
        resolve_cp2077_catalog_executable(),
        &tools_dir.join("dm-depth-launch.exe"),
        &tools_dir.join("dm_depth_probe.dll"),
    )
}

pub fn preflight_configured_cp2077() -> Result<EarlyLaunchPreflight, String> {
    let target = {
        let guard = runtime()
            .lock()
            .map_err(|_| "depth runtime lock is poisoned")?;
        guard.configured_early_launch_target
    };
    let target =
        target.ok_or("early-launch preflight requires a configured promoted D3D12 Depth target")?;
    let tools_dir = default_tools_dir();
    Ok(build_early_launch_preflight(
        resolve_steam_early_launch_target(target),
        &tools_dir.join("dm-depth-launch.exe"),
        &tools_dir.join("dm_depth_probe.dll"),
    ))
}

fn build_early_launch_preflight(
    executable: Result<PathBuf, String>,
    launcher: &Path,
    probe: &Path,
) -> EarlyLaunchPreflight {
    let mut blockers = Vec::new();
    let executable_hash_verified = match executable {
        Ok(_) => true,
        Err(error) => {
            blockers.push(error);
            false
        }
    };
    let launcher_present = launcher.is_absolute() && launcher.is_file();
    let launcher_sha256 = if launcher_present {
        match sha256_file(launcher) {
            Ok(hash) => Some(hash),
            Err(error) => {
                blockers.push(format!(
                    "cannot hash packaged D3D12 early launcher: {error}"
                ));
                None
            }
        }
    } else {
        blockers.push("D3D12 early launcher is missing from packaged depth tools".into());
        None
    };
    let probe_present = probe.is_absolute() && probe.is_file();
    let probe_sha256 = if probe_present {
        match sha256_file(probe) {
            Ok(hash) => Some(hash),
            Err(error) => {
                blockers.push(format!("cannot hash packaged D3D12 depth probe: {error}"));
                None
            }
        }
    } else {
        blockers.push("D3D12 depth probe is missing from packaged depth tools".into());
        None
    };
    let ready = executable_hash_verified
        && launcher_sha256.is_some()
        && probe_sha256.is_some()
        && blockers.is_empty();
    EarlyLaunchPreflight {
        eligible: true,
        ready,
        executable_hash_verified,
        launcher_present,
        launcher_sha256,
        probe_present,
        probe_sha256,
        blockers,
    }
}

pub fn launch_configured_cp2077() -> Result<EarlyLaunchResult, String> {
    let toggle = *TOGGLE.lock().map_err(|_| "depth toggle lock is poisoned")?;
    let (target, depth_profile_sha256) = {
        let mut guard = runtime()
            .lock()
            .map_err(|_| "depth runtime lock is poisoned")?;
        if !toggle.enabled_this_run || !toggle.risk_acknowledged {
            return Err(
                "enable Depth and acknowledge the injection risk before launching the configured D3D12 game".into(),
            );
        }
        if guard.early_launch.is_some() {
            return Err(
                "the configured D3D12 game was already launched by the verified early-launch path"
                    .into(),
            );
        }
        if matches!(
            guard.discovery_state,
            DiscoverySmokeState::Preparing
                | DiscoverySmokeState::HooksArmed
                | DiscoverySmokeState::WaitingObservations
                | DiscoverySmokeState::Sealing
        ) {
            return Err(
                "CP2077 discovery-only smoke is active; it cannot be promoted to recording".into(),
            );
        }
        let target = guard
            .configured_early_launch_target
            .ok_or("no promoted D3D12 early-launch target is configured")?;
        let depth_profile_sha256 = guard
            .configured_depth_profile_sha256
            .clone()
            .ok_or("D3D12 early launch is missing its exact DepthProfile hash")?;
        guard.early_launch_state = EarlyLaunchState::Launching;
        (target, depth_profile_sha256)
    };

    let launch_result = (|| {
        let executable = resolve_steam_early_launch_target(target)?;
        let tools_dir = default_tools_dir();
        let launcher = tools_dir.join("dm-depth-launch.exe");
        let probe = tools_dir.join("dm_depth_probe.dll");
        validate_early_launch_tools(&launcher, &probe)?;
        let probe_sha256 = sha256_file(&probe)?;
        let args = EarlyLauncherRequest {
            target,
            executable: &executable,
            probe: &probe,
            probe_sha256: &probe_sha256,
            mode: EarlyLauncherMode::FormalDepth,
        }
        .args()?;
        let output = std::process::Command::new(&launcher)
            .creation_flags(CREATE_NO_WINDOW.0)
            .envs(target.steam_environment())
            .args(&args)
            .output()
            .map_err(|error| format!("start D3D12 early launcher failed: {error}"))?;
        if !output.status.success() {
            return Err(format!(
                "D3D12 early launcher failed ({:?}): {}",
                output.status.code(),
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        parse_early_launcher_output(&output.stdout, &probe_sha256).map(|launched| {
            EarlyLaunchRecord {
                target,
                process_id: launched.process_id,
                executable_sha256: target.executable_sha256().into(),
                depth_profile_sha256,
                resident_probe_sha256: launched.resident_probe_sha256,
                launch_id: launched.launch_id,
            }
        })
    })();

    let mut guard = runtime()
        .lock()
        .map_err(|_| "depth runtime lock is poisoned")?;
    match launch_result {
        Ok(record) => {
            let result = EarlyLaunchResult {
                process_id: record.process_id,
                state: EarlyLaunchState::WaitingForWindow,
            };
            guard.early_launch = Some(record);
            guard.early_launch_state = EarlyLaunchState::WaitingForWindow;
            guard.last_error = None;
            Ok(result)
        }
        Err(error) => {
            guard.early_launch_state = EarlyLaunchState::AwaitingLaunch;
            guard.last_error = Some(error.clone());
            Err(error)
        }
    }
}

fn validate_early_launch_tools(launcher: &Path, probe: &Path) -> Result<(), String> {
    if !launcher.is_absolute() || !launcher.is_file() {
        return Err("D3D12 early launcher is missing from the packaged depth tools".into());
    }
    if !probe.is_absolute() || !probe.is_file() {
        return Err("D3D12 depth probe is missing from the packaged depth tools".into());
    }
    Ok(())
}

fn valid_launch_id(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[derive(Debug)]
struct PreparedDiscoverySession {
    output_dir: PathBuf,
    output_locator: String,
    config_path: PathBuf,
}

fn prepare_discovery_session() -> Result<PreparedDiscoverySession, String> {
    let local_app_data = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .ok_or("LOCALAPPDATA is unavailable for the discovery-only evidence root")?;
    if !local_app_data.is_absolute() {
        return Err("LOCALAPPDATA is not an absolute trusted discovery root".into());
    }
    let session_id = uuid::Uuid::new_v4().simple().to_string();
    prepare_discovery_session_at(&local_app_data, &session_id)
}

fn prepare_discovery_session_at(
    local_app_data: &Path,
    session_id: &str,
) -> Result<PreparedDiscoverySession, String> {
    if !local_app_data.is_absolute() || !valid_launch_id(session_id) {
        return Err("discovery root/session identity is invalid".into());
    }
    let relative = PathBuf::from("DataMiner")
        .join("depth-discovery")
        .join("cp2077")
        .join(session_id);
    let output_dir = local_app_data.join(&relative);
    std::fs::create_dir_all(&output_dir)
        .map_err(|error| format!("create CP2077 discovery evidence directory failed: {error}"))?;
    let output_dir = std::fs::canonicalize(&output_dir)
        .map_err(|error| format!("resolve CP2077 discovery evidence directory failed: {error}"))?;
    let config_path = output_dir.join(DISCOVERY_CONFIG_FILE);
    let config = serde_json::to_vec_pretty(&serde_json::json!({
        "schema": DISCOVERY_CONFIG_SCHEMA,
        "recording_ready": false,
        // Relative to this config's parent: no Windows account path is persisted.
        "output_dir": ".",
    }))
    .map_err(|error| format!("serialize CP2077 discovery config failed: {error}"))?;
    write_create_new(&config_path, &config, "CP2077 discovery config")?;
    Ok(PreparedDiscoverySession {
        output_dir,
        output_locator: relative.to_string_lossy().replace('\\', "/"),
        config_path,
    })
}

fn write_create_new(path: &Path, bytes: &[u8], label: &str) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| format!("create {label} failed: {error}"))?;
    file.write_all(bytes)
        .map_err(|error| format!("write {label} failed: {error}"))?;
    file.sync_all()
        .map_err(|error| format!("flush {label} failed: {error}"))
}

fn discovery_process_has_exited(process_id: u32) -> Result<bool, String> {
    use windows::Win32::Foundation::{
        CloseHandle, ERROR_INVALID_PARAMETER, WAIT_OBJECT_0, WAIT_TIMEOUT,
    };
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };

    if process_id == 0 {
        return Err("discovery process id is zero".into());
    }
    let process = match unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, process_id) } {
        Ok(process) => process,
        Err(error) if error.code() == ERROR_INVALID_PARAMETER.to_hresult() => return Ok(true),
        Err(error) => {
            return Err(format!(
                "cannot prove whether discovery process {process_id} exited: {error}"
            ));
        }
    };
    let wait = unsafe { WaitForSingleObject(process, 0) };
    let _ = unsafe { CloseHandle(process) };
    if wait == WAIT_OBJECT_0 {
        Ok(true)
    } else if wait == WAIT_TIMEOUT {
        Ok(false)
    } else {
        Err(format!(
            "cannot prove whether discovery process {process_id} exited: wait={wait:?}"
        ))
    }
}

/// Drops only the in-memory active provenance after Windows proves that the
/// failed launch has exited. The evidence directory is intentionally untouched
/// and a subsequent start creates a fresh session and launch identity.
fn recover_faulted_discovery(
    runtime: &mut Runtime,
    process_exited: impl FnOnce(u32) -> Result<bool, String>,
) -> Result<(), String> {
    if runtime.discovery_state != DiscoverySmokeState::Faulted {
        return Ok(());
    }
    let Some(record) = runtime.discovery.as_ref() else {
        runtime.discovery_state = DiscoverySmokeState::Idle;
        runtime.discovery_last_error = None;
        return Ok(());
    };
    if !process_exited(record.process_id)? {
        return Err(format!(
            "the previous discovery process {} is still alive; refusing to launch another",
            record.process_id
        ));
    }
    runtime.discovery = None;
    runtime.discovery_state = DiscoverySmokeState::Idle;
    runtime.discovery_last_error = None;
    Ok(())
}

pub fn start_cp2077_discovery(
    request: DiscoverySmokeStartRequest,
) -> Result<DiscoverySmokeResult, String> {
    if !cp2077_discovery_available() {
        return Err("CP2077 discovery smoke is not available in this client build".into());
    }
    if !request.risk_acknowledged {
        return Err("CP2077 discovery requires explicit injection-risk acknowledgement".into());
    }
    let target = EarlyLaunchTarget::from_local_trial_game_id(&request.game_id)
        .filter(|target| *target == EarlyLaunchTarget::cp2077())
        .ok_or("discovery-only launch is restricted to the CP2077 D3D12 catalog lane")?;
    if request.game_id != target.game_id() {
        return Err("discovery-only launch is restricted to the CP2077 D3D12 catalog lane".into());
    }
    {
        let mut guard = runtime()
            .lock()
            .map_err(|_| "depth runtime lock is poisoned")?;
        if guard.early_launch.is_some()
            || matches!(
                guard.lifecycle.state(),
                ClientLifecycleState::Starting
                    | ClientLifecycleState::Started
                    | ClientLifecycleState::Ready
                    | ClientLifecycleState::Stopping
                    | ClientLifecycleState::Stopped
            )
        {
            return Err(
                "formal Depth activity is present; discovery-only launch was refused".into(),
            );
        }
        if matches!(
            guard.discovery_state,
            DiscoverySmokeState::Preparing
                | DiscoverySmokeState::HooksArmed
                | DiscoverySmokeState::WaitingObservations
                | DiscoverySmokeState::Sealing
        ) {
            return Err("a CP2077 discovery-only smoke is already active".into());
        }
        recover_faulted_discovery(&mut guard, discovery_process_has_exited)?;
        guard.discovery_state = DiscoverySmokeState::Preparing;
        guard.discovery = None;
        guard.discovery_last_error = None;
    }

    let launched = (|| {
        let preflight = preflight_cp2077_discovery();
        if !preflight.ready {
            return Err(format!(
                "CP2077 discovery preflight failed: {}",
                preflight.blockers.join("; ")
            ));
        }
        let prepared = prepare_discovery_session()?;
        let executable = resolve_cp2077_catalog_executable()?;
        let tools_dir = default_tools_dir();
        let launcher = tools_dir.join("dm-depth-launch.exe");
        let probe = tools_dir.join("dm_depth_probe.dll");
        validate_early_launch_tools(&launcher, &probe)?;
        let probe_sha256 = sha256_file(&probe)?;
        let args = EarlyLauncherRequest {
            target,
            executable: &executable,
            probe: &probe,
            probe_sha256: &probe_sha256,
            mode: EarlyLauncherMode::Discovery {
                config: &prepared.config_path,
            },
        }
        .args()?;
        let output = std::process::Command::new(&launcher)
            .creation_flags(CREATE_NO_WINDOW.0)
            .envs(target.steam_environment())
            .args(&args)
            .output()
            .map_err(|error| format!("start CP2077 discovery launcher failed: {error}"))?;
        if !output.status.success() {
            return Err(format!(
                "CP2077 discovery launcher failed ({:?}): {}",
                output.status.code(),
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        let parsed = parse_early_launcher_output(&output.stdout, &probe_sha256)?;
        Ok(DiscoverySmokeRecord {
            process_id: parsed.process_id,
            executable_sha256: target.executable_sha256().into(),
            resident_probe_sha256: parsed.resident_probe_sha256,
            launch_id: parsed.launch_id,
            output_dir: prepared.output_dir,
            output_locator: prepared.output_locator,
        })
    })();

    let mut guard = runtime()
        .lock()
        .map_err(|_| "depth runtime lock is poisoned")?;
    match launched {
        Ok(record) => {
            let result = DiscoverySmokeResult {
                state: DiscoverySmokeState::HooksArmed,
                process_id: Some(record.process_id),
                output_locator: Some(record.output_locator.clone()),
                recording_ready: false,
            };
            guard.discovery = Some(record);
            guard.discovery_state = DiscoverySmokeState::HooksArmed;
            guard.discovery_last_error = None;
            Ok(result)
        }
        Err(error) => {
            guard.discovery_state = DiscoverySmokeState::Faulted;
            guard.discovery_last_error = Some(error.clone());
            Err(error)
        }
    }
}

/// Connects the discovery-only launch provenance to the normal catalog/window
/// verifier. This does not create a formal early-launch record and therefore
/// cannot satisfy `start_for_run`.
pub fn observe_cp2077_discovery_binding(
    game_id: &str,
    process_id: u32,
    executable_sha256: Option<&str>,
) -> Result<(), String> {
    let mut guard = runtime()
        .lock()
        .map_err(|_| "depth runtime lock is poisoned")?;
    if guard.discovery_state != DiscoverySmokeState::HooksArmed {
        return Ok(());
    }
    let record = guard
        .discovery
        .as_ref()
        .ok_or("CP2077 discovery state has no launch provenance")?;
    let matches = discovery_binding_matches(record, game_id, process_id, executable_sha256);
    if !matches {
        let error =
            "bound window PID/hash does not match the discovery-only CP2077 launch".to_string();
        guard.discovery_state = DiscoverySmokeState::Faulted;
        guard.discovery_last_error = Some(error.clone());
        return Err(error);
    }
    guard.discovery_state = DiscoverySmokeState::WaitingObservations;
    guard.discovery_last_error = None;
    Ok(())
}

fn discovery_binding_matches(
    record: &DiscoverySmokeRecord,
    game_id: &str,
    process_id: u32,
    executable_sha256: Option<&str>,
) -> bool {
    EarlyLaunchTarget::from_local_trial_game_id(game_id) == Some(EarlyLaunchTarget::cp2077())
        && record.process_id == process_id
        && executable_sha256.is_some_and(|hash| {
            hash.eq_ignore_ascii_case(&record.executable_sha256)
                && hash.eq_ignore_ascii_case(EarlyLaunchTarget::cp2077().executable_sha256())
        })
        && valid_launch_id(&record.launch_id)
        && !record.resident_probe_sha256.is_empty()
}

fn discovery_process_blocks_recording(
    discovery: Option<&DiscoverySmokeRecord>,
    game_pid: u32,
) -> bool {
    discovery.is_some_and(|run| run.process_id == game_pid)
}

fn validate_discovery_report(
    bytes: &[u8],
    expected_schema: &str,
    expected_launch_id: &str,
) -> Result<(), String> {
    let report: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|error| format!("parse discovery report failed: {error}"))?;
    if report.get("schema").and_then(serde_json::Value::as_str) != Some(expected_schema) {
        return Err(format!("discovery report schema is not {expected_schema}"));
    }
    if report
        .get("process_launch_id")
        .and_then(serde_json::Value::as_str)
        != Some(expected_launch_id)
    {
        return Err("discovery report launch identity does not match this process".into());
    }
    if report
        .get("recording_ready")
        .and_then(serde_json::Value::as_bool)
        != Some(false)
    {
        return Err("discovery report must explicitly keep recording_ready=false".into());
    }
    Ok(())
}

fn read_validated_discovery_report(
    path: &Path,
    schema: &str,
    launch_id: &str,
) -> Result<(Vec<u8>, String), String> {
    let metadata = path
        .metadata()
        .map_err(|error| format!("discovery report {} is not sealed: {error}", path.display()))?;
    if !metadata.is_file() || metadata.len() > DISCOVERY_REPORT_MAX_BYTES {
        return Err("discovery report is not a bounded regular file".into());
    }
    let bytes =
        std::fs::read(path).map_err(|error| format!("read discovery report failed: {error}"))?;
    validate_discovery_report(&bytes, schema, launch_id)?;
    let hash = sha256_bytes(&bytes);
    Ok((bytes, hash))
}

pub fn stop_cp2077_discovery() -> Result<DiscoverySmokeResult, String> {
    let record = {
        let mut guard = runtime()
            .lock()
            .map_err(|_| "depth runtime lock is poisoned")?;
        if !matches!(
            guard.discovery_state,
            DiscoverySmokeState::HooksArmed
                | DiscoverySmokeState::WaitingObservations
                | DiscoverySmokeState::Faulted
        ) {
            return Err("CP2077 discovery-only smoke is not collecting observations".into());
        }
        let record = guard
            .discovery
            .clone()
            .ok_or("CP2077 discovery state has no launch provenance")?;
        guard.discovery_state = DiscoverySmokeState::Sealing;
        guard.discovery_last_error = None;
        record
    };

    let sealed = (|| {
        let stop = serde_json::to_vec_pretty(&serde_json::json!({
            "schema": DISCOVERY_STOP_SCHEMA,
            "discovery_only": true,
            "recording_ready": false,
            "process_launch_id": record.launch_id,
        }))
        .map_err(|error| format!("serialize discovery stop request failed: {error}"))?;
        let stop_path = record.output_dir.join(DISCOVERY_STOP_FILE);
        if stop_path.exists() {
            let existing = std::fs::read(&stop_path)
                .map_err(|error| format!("read existing discovery stop request failed: {error}"))?;
            validate_discovery_report(&existing, DISCOVERY_STOP_SCHEMA, &record.launch_id)?;
        } else {
            write_create_new(&stop_path, &stop, "discovery stop request")?;
        }

        let d3d12_path = record.output_dir.join(D3D12_DISCOVERY_REPORT_FILE);
        let pose_path = record.output_dir.join(POSE_DISCOVERY_REPORT_FILE);
        let deadline = Instant::now() + Duration::from_secs(20);
        let (d3d12_hash, pose_hash) = loop {
            let d3d12 = read_validated_discovery_report(
                &d3d12_path,
                D3D12_DISCOVERY_REPORT_SCHEMA,
                &record.launch_id,
            );
            let pose = read_validated_discovery_report(
                &pose_path,
                POSE_DISCOVERY_REPORT_SCHEMA,
                &record.launch_id,
            );
            if let (Ok((_, d3d12_hash)), Ok((_, pose_hash))) = (d3d12, pose) {
                break (d3d12_hash, pose_hash);
            }
            if Instant::now() >= deadline {
                return Err(
                    "timed out waiting for both D3D12 and pose discovery reports to seal"
                        .to_string(),
                );
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        let marker = serde_json::to_vec_pretty(&serde_json::json!({
            "schema": DISCOVERY_SEALED_SCHEMA,
            "discovery_only": true,
            "recording_ready": false,
            "process_launch_id": record.launch_id,
            "process_id": record.process_id,
            "executable_sha256": record.executable_sha256,
            "resident_probe_sha256": record.resident_probe_sha256,
            "reports": {
                D3D12_DISCOVERY_REPORT_FILE: d3d12_hash,
                POSE_DISCOVERY_REPORT_FILE: pose_hash,
            },
        }))
        .map_err(|error| format!("serialize sealed discovery marker failed: {error}"))?;
        write_create_new(
            &record.output_dir.join(DISCOVERY_SEALED_FILE),
            &marker,
            "sealed discovery marker",
        )
    })();

    let mut guard = runtime()
        .lock()
        .map_err(|_| "depth runtime lock is poisoned")?;
    match sealed {
        Ok(()) => {
            guard.discovery_state = DiscoverySmokeState::ReportSealed;
            guard.discovery_last_error = None;
            Ok(DiscoverySmokeResult {
                state: DiscoverySmokeState::ReportSealed,
                process_id: Some(record.process_id),
                output_locator: Some(record.output_locator),
                recording_ready: false,
            })
        }
        Err(error) => {
            guard.discovery_state = DiscoverySmokeState::Faulted;
            guard.discovery_last_error = Some(error.clone());
            Err(error)
        }
    }
}

pub(crate) fn resolve_cp2077_catalog_executable() -> Result<PathBuf, String> {
    resolve_steam_early_launch_target(EarlyLaunchTarget::cp2077())
}

fn resolve_steam_early_launch_target(target: EarlyLaunchTarget) -> Result<PathBuf, String> {
    resolve_steam_early_launch_target_from_roots(
        &trusted_steam_roots(),
        target,
        target.executable_sha256(),
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum VdfToken {
    Text(String),
    Open,
    Close,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum VdfValue {
    Text(String),
    Object(Vec<(String, VdfValue)>),
}

fn tokenize_vdf(input: &str) -> Result<Vec<VdfToken>, String> {
    let bytes = input.as_bytes();
    let mut tokens = Vec::new();
    let mut index = 0usize;
    while index < bytes.len() {
        match bytes[index] {
            b' ' | b'\t' | b'\r' | b'\n' => index += 1,
            b'/' if bytes.get(index + 1) == Some(&b'/') => {
                index += 2;
                while index < bytes.len() && bytes[index] != b'\n' {
                    index += 1;
                }
            }
            b'{' => {
                tokens.push(VdfToken::Open);
                index += 1;
            }
            b'}' => {
                tokens.push(VdfToken::Close);
                index += 1;
            }
            b'"' => {
                index += 1;
                let mut value = Vec::new();
                let mut closed = false;
                while index < bytes.len() {
                    match bytes[index] {
                        b'"' => {
                            index += 1;
                            closed = true;
                            break;
                        }
                        b'\\' => {
                            index += 1;
                            let escaped =
                                *bytes.get(index).ok_or("unterminated escape in Steam VDF")?;
                            value.push(match escaped {
                                b'n' => b'\n',
                                b't' => b'\t',
                                b'"' => b'"',
                                b'\\' => b'\\',
                                other => other,
                            });
                            index += 1;
                        }
                        byte => {
                            value.push(byte);
                            index += 1;
                        }
                    }
                }
                if !closed {
                    return Err("unterminated quoted value in Steam VDF".into());
                }
                tokens.push(VdfToken::Text(
                    String::from_utf8(value).map_err(|_| "Steam VDF is not valid UTF-8")?,
                ));
            }
            _ => {
                let start = index;
                while index < bytes.len()
                    && !bytes[index].is_ascii_whitespace()
                    && !matches!(bytes[index], b'{' | b'}')
                {
                    index += 1;
                }
                tokens.push(VdfToken::Text(
                    std::str::from_utf8(&bytes[start..index])
                        .map_err(|_| "Steam VDF is not valid UTF-8")?
                        .to_owned(),
                ));
            }
        }
        if tokens.len() > 100_000 {
            return Err("Steam VDF token count exceeds safety limit".into());
        }
    }
    Ok(tokens)
}

fn parse_vdf(input: &str) -> Result<Vec<(String, VdfValue)>, String> {
    fn object(
        tokens: &[VdfToken],
        cursor: &mut usize,
        expects_close: bool,
    ) -> Result<Vec<(String, VdfValue)>, String> {
        let mut entries = Vec::new();
        while *cursor < tokens.len() {
            if tokens[*cursor] == VdfToken::Close {
                if !expects_close {
                    return Err("unexpected closing brace in Steam VDF".into());
                }
                *cursor += 1;
                return Ok(entries);
            }
            let VdfToken::Text(key) = &tokens[*cursor] else {
                return Err("Steam VDF key is not text".into());
            };
            *cursor += 1;
            let value = match tokens.get(*cursor) {
                Some(VdfToken::Text(value)) => {
                    *cursor += 1;
                    VdfValue::Text(value.clone())
                }
                Some(VdfToken::Open) => {
                    *cursor += 1;
                    VdfValue::Object(object(tokens, cursor, true)?)
                }
                _ => return Err("Steam VDF key has no value".into()),
            };
            entries.push((key.clone(), value));
        }
        if expects_close {
            return Err("unterminated object in Steam VDF".into());
        }
        Ok(entries)
    }

    let tokens = tokenize_vdf(input)?;
    let mut cursor = 0;
    let root = object(&tokens, &mut cursor, false)?;
    if cursor != tokens.len() {
        return Err("trailing tokens in Steam VDF".into());
    }
    Ok(root)
}

fn vdf_object<'a>(
    entries: &'a [(String, VdfValue)],
    key: &str,
) -> Option<&'a [(String, VdfValue)]> {
    entries.iter().find_map(|(name, value)| {
        (name.eq_ignore_ascii_case(key))
            .then_some(value)
            .and_then(|value| match value {
                VdfValue::Object(object) => Some(object.as_slice()),
                VdfValue::Text(_) => None,
            })
    })
}

fn vdf_text<'a>(entries: &'a [(String, VdfValue)], key: &str) -> Option<&'a str> {
    entries.iter().find_map(|(name, value)| {
        (name.eq_ignore_ascii_case(key))
            .then_some(value)
            .and_then(|value| match value {
                VdfValue::Text(text) => Some(text.as_str()),
                VdfValue::Object(_) => None,
            })
    })
}

fn read_bounded_utf8(path: &Path, max_bytes: u64, label: &str) -> Result<String, String> {
    let metadata = path
        .metadata()
        .map_err(|error| format!("cannot inspect {label}: {error}"))?;
    if !metadata.is_file() || metadata.len() > max_bytes {
        return Err(format!("{label} is not a bounded regular file"));
    }
    std::fs::read_to_string(path).map_err(|error| format!("cannot read {label}: {error}"))
}

fn steam_libraries_from_root(steam_root: &Path) -> Result<Vec<PathBuf>, String> {
    let canonical_root = std::fs::canonicalize(steam_root)
        .map_err(|error| format!("cannot resolve trusted Steam root: {error}"))?;
    let mut libraries = vec![canonical_root];
    let vdf_path = steam_root.join("steamapps").join("libraryfolders.vdf");
    if !vdf_path.is_file() {
        return Ok(libraries);
    }
    let text = read_bounded_utf8(&vdf_path, MAX_STEAM_VDF_BYTES, "Steam libraryfolders.vdf")?;
    let root = parse_vdf(&text)?;
    let folders = vdf_object(&root, "libraryfolders")
        .ok_or("Steam libraryfolders.vdf has no libraryfolders object")?;
    for (index, value) in folders {
        if !index.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }
        let VdfValue::Object(folder) = value else {
            return Err("Steam library entry is not an object".into());
        };
        let Some(path) = vdf_text(folder, "path") else {
            return Err("Steam library entry has no path".into());
        };
        let path = PathBuf::from(path);
        if !path.is_absolute() {
            return Err("Steam libraryfolders.vdf contains a relative library path".into());
        }
        if path.is_dir() {
            let canonical = std::fs::canonicalize(path)
                .map_err(|error| format!("cannot resolve Steam library path: {error}"))?;
            if !libraries
                .iter()
                .any(|known| same_windows_path(known, &canonical))
            {
                libraries.push(canonical);
            }
        }
    }
    Ok(libraries)
}

fn same_windows_path(left: &Path, right: &Path) -> bool {
    left.to_string_lossy()
        .replace('/', r"\")
        .trim_end_matches('\\')
        .eq_ignore_ascii_case(
            right
                .to_string_lossy()
                .replace('/', r"\")
                .trim_end_matches('\\'),
        )
}

fn trusted_steam_roots() -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    for candidate in [
        steam_registry_path(true),
        steam_registry_path(false),
        Some(PathBuf::from(r"C:\Program Files (x86)\Steam")),
        Some(PathBuf::from(r"C:\Program Files\Steam")),
    ]
    .into_iter()
    .flatten()
    {
        if candidate.is_dir()
            && let Ok(canonical) = std::fs::canonicalize(candidate)
            && !roots
                .iter()
                .any(|known| same_windows_path(known, &canonical))
        {
            roots.push(canonical);
        }
    }
    roots
}

fn steam_registry_path(current_user: bool) -> Option<PathBuf> {
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{
        HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, REG_ROUTINE_FLAGS, RRF_RT_REG_SZ,
        RRF_SUBKEY_WOW6432KEY, RegGetValueW,
    };
    use windows::core::w;

    let (root, name, flags) = if current_user {
        (HKEY_CURRENT_USER, w!("SteamPath"), RRF_RT_REG_SZ)
    } else {
        (
            HKEY_LOCAL_MACHINE,
            w!("InstallPath"),
            REG_ROUTINE_FLAGS(RRF_RT_REG_SZ.0 | RRF_SUBKEY_WOW6432KEY.0),
        )
    };
    let mut byte_count = 0u32;
    let first = unsafe {
        RegGetValueW(
            root,
            w!("Software\\Valve\\Steam"),
            name,
            flags,
            None,
            None,
            Some(&mut byte_count),
        )
    };
    if first != ERROR_SUCCESS || !(2..=32_768 * 2).contains(&byte_count) {
        return None;
    }
    let mut buffer = vec![0u16; byte_count.div_ceil(2) as usize];
    let second = unsafe {
        RegGetValueW(
            root,
            w!("Software\\Valve\\Steam"),
            name,
            flags,
            None,
            Some(buffer.as_mut_ptr().cast()),
            Some(&mut byte_count),
        )
    };
    if second != ERROR_SUCCESS {
        return None;
    }
    let end = buffer
        .iter()
        .position(|value| *value == 0)
        .unwrap_or(buffer.len());
    let path = PathBuf::from(String::from_utf16_lossy(&buffer[..end]));
    path.is_absolute().then_some(path)
}

fn resolve_steam_early_launch_target_from_roots(
    steam_roots: &[PathBuf],
    target: EarlyLaunchTarget,
    expected_sha256: &str,
) -> Result<PathBuf, String> {
    let mut libraries: Vec<PathBuf> = Vec::new();
    for steam_root in steam_roots {
        for library in steam_libraries_from_root(steam_root)? {
            if !libraries
                .iter()
                .any(|known| same_windows_path(known, &library))
            {
                libraries.push(library);
            }
        }
    }
    let mut manifests = Vec::new();
    for library in libraries {
        let manifest = library
            .join("steamapps")
            .join(format!("appmanifest_{}.acf", target.steam_app_id()));
        if manifest.is_file() {
            manifests.push((library, manifest));
        }
    }
    let [(library, manifest)] = manifests.as_slice() else {
        return match manifests.len() {
            0 => Err(format!(
                "{} exact app manifest was not found in trusted Steam libraries",
                target.game_id()
            )),
            count => Err(format!(
                "{} has {count} app manifests in trusted Steam libraries; early launch requires one unambiguous complete install",
                target.game_id()
            )),
        };
    };
    ensure_no_reparse_beneath(library, manifest, "Steam app manifest")?;
    let manifest_text = read_bounded_utf8(
        manifest,
        MAX_APP_MANIFEST_BYTES,
        &format!("{} app manifest", target.game_id()),
    )?;
    let root = parse_vdf(&manifest_text)?;
    let app = vdf_object(&root, "AppState")
        .ok_or_else(|| format!("{} app manifest has no AppState", target.game_id()))?;
    if vdf_text(app, "appid") != Some(target.steam_app_id()) {
        return Err(format!(
            "{} app manifest identity mismatch",
            target.game_id()
        ));
    }
    let state_flags = vdf_text(app, "StateFlags")
        .ok_or_else(|| format!("{} app manifest has no StateFlags", target.game_id()))?
        .parse::<u64>()
        .map_err(|_| format!("{} app manifest has invalid StateFlags", target.game_id()))?;
    let size_on_disk = vdf_text(app, "SizeOnDisk")
        .ok_or_else(|| format!("{} app manifest has no SizeOnDisk", target.game_id()))?
        .parse::<u64>()
        .map_err(|_| format!("{} app manifest has invalid SizeOnDisk", target.game_id()))?;
    let build_id = vdf_text(app, "buildid")
        .ok_or_else(|| format!("{} app manifest has no buildid", target.game_id()))?;
    if state_flags != 4 || size_on_disk == 0 || build_id != target.steam_build_id() {
        return Err(format!(
            "{} Steam install is incomplete or not the reviewed build: StateFlags={state_flags} SizeOnDisk={size_on_disk} buildid={build_id} expectedBuildId={}",
            target.game_id(),
            target.steam_build_id()
        ));
    }
    let install_dir = vdf_text(app, "installdir")
        .ok_or_else(|| format!("{} app manifest has no installdir", target.game_id()))?;
    let install_component = Path::new(install_dir);
    if install_component.is_absolute()
        || install_component.components().count() != 1
        || !matches!(
            install_component.components().next(),
            Some(std::path::Component::Normal(_))
        )
    {
        return Err(format!(
            "{} app manifest contains an unsafe installdir",
            target.game_id()
        ));
    }
    let candidate = library
        .join("steamapps")
        .join("common")
        .join(install_component)
        .join(target.relative_executable());
    if !candidate.is_file() {
        return Err(format!(
            "{} reviewed executable is missing from the complete Steam install",
            target.game_id()
        ));
    }
    ensure_no_reparse_beneath(library, &candidate, "Steam install executable path")?;
    let canonical_library = std::fs::canonicalize(library)
        .map_err(|error| format!("cannot resolve Steam library: {error}"))?;
    let canonical_common =
        std::fs::canonicalize(canonical_library.join("steamapps").join("common"))
            .map_err(|error| format!("cannot resolve Steam common directory: {error}"))?;
    let canonical_candidate = std::fs::canonicalize(&candidate)
        .map_err(|error| format!("cannot resolve {} executable: {error}", target.game_id()))?;
    ensure_candidate_within_common(
        target,
        &canonical_library,
        &canonical_common,
        &canonical_candidate,
    )?;
    let actual = sha256_file(&canonical_candidate)?;
    if !actual.eq_ignore_ascii_case(expected_sha256) {
        return Err(format!(
            "{} exact-build hash mismatch: expected={expected_sha256} actual={actual}",
            target.game_id()
        ));
    }
    // Steam may finish a download or begin an update while this resolver is
    // hashing a large executable. Re-observe every trust input before returning
    // so a mixed manifest/path/byte snapshot cannot authorize the launcher.
    ensure_no_reparse_beneath(library, manifest, "Steam app manifest")?;
    let manifest_after = read_bounded_utf8(
        manifest,
        MAX_APP_MANIFEST_BYTES,
        &format!("{} app manifest", target.game_id()),
    )?;
    ensure_no_reparse_beneath(library, &candidate, "Steam install executable path")?;
    let canonical_candidate_after = std::fs::canonicalize(&candidate)
        .map_err(|error| format!("cannot re-resolve {} executable: {error}", target.game_id()))?;
    let actual_after = sha256_file(&canonical_candidate_after)?;
    ensure_steam_resolution_stable(
        target,
        &manifest_text,
        &manifest_after,
        &canonical_candidate,
        &canonical_candidate_after,
        &actual,
        &actual_after,
    )?;
    Ok(canonical_candidate_after)
}

fn ensure_no_reparse_beneath(base: &Path, target: &Path, label: &str) -> Result<(), String> {
    let relative = target
        .strip_prefix(base)
        .map_err(|_| format!("{label} is outside its trusted Steam library"))?;
    let mut current = base.to_path_buf();
    for component in relative.components() {
        let std::path::Component::Normal(component) = component else {
            return Err(format!("{label} contains an unsafe path component"));
        };
        current.push(component);
        let metadata = std::fs::symlink_metadata(&current)
            .map_err(|error| format!("cannot inspect {label} {}: {error}", current.display()))?;
        ensure_not_reparse_attributes(metadata.file_attributes(), label)?;
    }
    Ok(())
}

fn ensure_not_reparse_attributes(attributes: u32, label: &str) -> Result<(), String> {
    if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(format!("{label} contains a filesystem reparse point"));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn ensure_steam_resolution_stable(
    target: EarlyLaunchTarget,
    manifest_before: &str,
    manifest_after: &str,
    executable_before: &Path,
    executable_after: &Path,
    hash_before: &str,
    hash_after: &str,
) -> Result<(), String> {
    if manifest_before != manifest_after
        || !same_windows_path(executable_before, executable_after)
        || !hash_before.eq_ignore_ascii_case(hash_after)
    {
        return Err(format!(
            "{} Steam manifest or executable changed during exact-build resolution",
            target.game_id()
        ));
    }
    Ok(())
}

fn ensure_candidate_within_common(
    target: EarlyLaunchTarget,
    canonical_library: &Path,
    canonical_common: &Path,
    canonical_candidate: &Path,
) -> Result<(), String> {
    if !canonical_common.starts_with(canonical_library)
        || !canonical_candidate.starts_with(canonical_common)
    {
        return Err(format!(
            "{} manifest resolved through a link outside its trusted Steam common directory",
            target.game_id()
        ));
    }
    Ok(())
}

pub struct DepthStartContext<'a> {
    pub catalog_selection: &'a CatalogSelection,
    pub hardware_evidence: Option<&'a capture_core::recording::HardwarePreflightEvidence>,
    pub session_id: &'a str,
    pub run_id: &'a str,
    pub generation: u64,
    pub game_id: &'a str,
    pub game_pid: u32,
    pub executable_sha256: Option<&'a str>,
    pub output_dir: &'a Path,
    pub capture_fps_num: u32,
    pub capture_fps_den: u32,
    pub free_disk_bytes: u64,
    pub gpu_adapter_description: &'a str,
    pub gpu_driver_version: Option<&'a str>,
    pub gpu_adapter_luid_high: i32,
    pub gpu_adapter_luid_low: u32,
}

pub struct DepthPerformanceCertificationContext<'a> {
    pub catalog_selection: &'a CatalogSelection,
    pub hardware_evidence: Option<&'a capture_core::recording::HardwarePreflightEvidence>,
    pub game_id: &'a str,
    pub executable_sha256: Option<&'a str>,
    pub capture_fps_num: u32,
    pub capture_fps_den: u32,
    pub gpu_adapter_description: &'a str,
    pub gpu_driver_version: Option<&'a str>,
    pub gpu_adapter_luid_high: i32,
    pub gpu_adapter_luid_low: u32,
}

struct PreparedPerformanceGate {
    profile_bytes: Vec<u8>,
    binding_bytes: Vec<u8>,
    profile: depth_lab::DepthProfile,
    binding: depth_lab::CameraBindingDocument,
    game_whitelisted: bool,
    executable_sha256_matches: bool,
    lane_matches: bool,
    tools_dir: PathBuf,
    probe_dll_bytes: Vec<u8>,
    probe_dll_sha256: String,
    probe_runtime_manifest: DepthProbeRuntimeManifest,
    probe_runtime_manifest_bytes: Vec<u8>,
    perf_hardware: PerfHardwareIdentity,
    perf_key: PerfCertificationKey,
    device_profile_sha256: String,
    recording_profile_sha256: String,
}

pub struct DepthProbeRunGuard {
    identity: depth_lab::StreamIdentity,
    first_present_seq: u64,
    watermark_source_width: u32,
    watermark_source_height: u32,
    output_dir: PathBuf,
    stop_timeout: Duration,
    active: bool,
    confirmed: Option<ConfirmedProbeFlush>,
    feedback: Option<depth_lab::selected_present_transport::ClientTransport>,
    selected_present_feedback_required: bool,
    present_identity_required: bool,
    performance_validation_only: bool,
    local_cp2077_audit: bool,
}

fn requires_local_cp2077_audit(game_id: &str, validation_only: bool, stage: Option<Stage>) -> bool {
    validation_only && stage.is_none() && game_id == EarlyLaunchTarget::cp2077().game_id()
}

impl DepthProbeRunGuard {
    pub fn local_cp2077_audit_required(&self) -> bool {
        self.local_cp2077_audit
    }
    #[must_use]
    pub fn identity(&self) -> &depth_lab::StreamIdentity {
        &self.identity
    }

    #[must_use]
    pub fn first_present_seq(&self) -> u64 {
        self.first_present_seq
    }

    #[must_use]
    pub fn watermark_source_dimensions(&self) -> (u32, u32) {
        (self.watermark_source_width, self.watermark_source_height)
    }

    #[must_use]
    pub fn performance_validation_only(&self) -> bool {
        self.performance_validation_only
    }

    pub fn take_feedback(
        &mut self,
    ) -> Option<depth_lab::selected_present_transport::ClientTransport> {
        self.feedback.take()
    }

    #[must_use]
    pub fn selected_present_feedback_required(&self) -> bool {
        self.selected_present_feedback_required
    }

    #[must_use]
    pub fn present_identity_required(&self) -> bool {
        self.present_identity_required
    }

    #[must_use]
    pub fn confirmed_flush(&self) -> Option<&ConfirmedProbeFlush> {
        self.confirmed.as_ref()
    }

    #[must_use]
    pub fn output_dir(&self) -> &Path {
        &self.output_dir
    }

    pub fn stop_and_flush(
        &mut self,
        cutoff_qpc: i64,
        reason: &str,
    ) -> Result<&ConfirmedProbeFlush, String> {
        if !self.active {
            return self
                .confirmed
                .as_ref()
                .ok_or_else(|| "depth run 已停止但没有确认过的 Flushed".into());
        }
        let command = ControlEnvelope {
            schema: CONTROL_SCHEMA_V3.into(),
            command_id: uuid::Uuid::new_v4().to_string(),
            identity: self.identity.clone(),
            command: ControlCommand::StopRun(StopRun {
                cutoff_qpc,
                reason: reason.to_owned(),
            }),
        };
        {
            let mut runtime = runtime().lock().map_err(|_| "depth runtime 锁已中毒")?;
            let mut next = runtime.lifecycle.clone();
            next.begin_stop(&command)
                .map_err(|error| format!("depth StopRun 状态不合法: {error}"))?;
            if let Err(error) = depth_lab::probe_control::write_command(
                &depth_lab::probe_control::control_path(),
                &command,
            ) {
                let error = format!("写 depth StopRun 失败: {error}");
                runtime.last_error = Some(error.clone());
                return Err(error);
            }
            runtime.lifecycle = next;
        }
        if let Err(error) = wait_for_state(
            &self.output_dir,
            ClientLifecycleState::Flushed,
            self.stop_timeout,
            None,
        ) {
            mark_local_fault(&self.identity, error.clone());
            self.active = false;
            return Err(error);
        }
        let summary = {
            let runtime = runtime().lock().map_err(|_| "depth runtime 锁已中毒")?;
            runtime
                .lifecycle
                .confirmed_flush()
                .cloned()
                .ok_or_else(|| "状态到达 Flushed 但缺少 flush summary".to_string())?
        };
        self.confirmed = match ConfirmedProbeFlush::new(
            &self.identity,
            summary,
            self.performance_validation_only,
        ) {
            Ok(confirmed) => Some(confirmed),
            Err(error) => {
                mark_local_fault(&self.identity, error.clone());
                self.active = false;
                return Err(error);
            }
        };
        if let Ok(mut runtime) = runtime().lock() {
            runtime.last_error = None;
        }
        self.active = false;
        Ok(self.confirmed.as_ref().expect("just inserted"))
    }
}

impl Drop for DepthProbeRunGuard {
    fn drop(&mut self) {
        if self.active {
            let cutoff = current_qpc().unwrap_or(1);
            if let Err(error) = self.stop_and_flush(cutoff, "client_cleanup") {
                record_runtime_error(format!("depth cleanup 未确认 Flushed: {error}"));
            }
        }
    }
}

fn prepare_perf_certification_key(
    key: &PerfCertificationKey,
    runtime_manifest: &DepthProbeRuntimeManifest,
    hardware: &PerfHardwareIdentity,
) -> Result<Option<Stage>, String> {
    let mut guard = runtime().lock().map_err(|_| "depth runtime 锁已中毒")?;
    let Some(certification) = guard.perf_certification.as_mut() else {
        return Ok(None);
    };
    let stage = certification
        .run
        .next_stage()
        .ok_or_else(|| "性能认证三档已经完成，不能再启动验证 run".to_string())?;
    if certification
        .key
        .as_ref()
        .is_some_and(|expected| expected != key)
        || certification
            .runtime_manifest
            .as_ref()
            .is_some_and(|expected| expected != runtime_manifest)
        || certification
            .hardware
            .as_ref()
            .is_some_and(|expected| expected != hardware)
    {
        guard.perf_certification = None;
        return Err(
            "性能认证的硬件、runtime generation 或 catalog 身份中途变化，整轮已作废".into(),
        );
    }
    certification.key = Some(key.clone());
    certification.runtime_manifest = Some(runtime_manifest.clone());
    certification.hardware = Some(hardware.clone());
    Ok(Some(stage))
}

fn load_probe_runtime_manifest(
    tools_dir: &Path,
    actual_dll_sha256: &str,
) -> Result<(DepthProbeRuntimeManifest, Vec<u8>), String> {
    let path = tools_dir.join(PROBE_RUNTIME_MANIFEST_FILE);
    let metadata = std::fs::metadata(&path).map_err(|error| {
        format!(
            "读取 probe runtime manifest {} 失败: {error}",
            path.display()
        )
    })?;
    if !metadata.is_file() || metadata.len() > MAX_PROBE_RUNTIME_MANIFEST_BYTES {
        return Err(format!(
            "probe runtime manifest {} 必须是小于等于 {} 字节的普通文件",
            path.display(),
            MAX_PROBE_RUNTIME_MANIFEST_BYTES
        ));
    }
    let bytes = std::fs::read(&path).map_err(|error| {
        format!(
            "读取 probe runtime manifest {} 失败: {error}",
            path.display()
        )
    })?;
    let manifest: DepthProbeRuntimeManifest = serde_json::from_slice(&bytes).map_err(|error| {
        format!(
            "解析 probe runtime manifest {} 失败: {error}",
            path.display()
        )
    })?;
    manifest.validate()?;
    if !manifest
        .dll_sha256
        .trim()
        .eq_ignore_ascii_case(actual_dll_sha256.trim())
    {
        return Err(format!(
            "打包的 probe DLL SHA-256 与 runtime manifest 不一致: manifest={} actual={actual_dll_sha256}",
            manifest.dll_sha256
        ));
    }
    Ok((manifest, bytes))
}

fn prepare_performance_gate(
    context: DepthPerformanceCertificationContext<'_>,
    unbind_from_end: usize,
) -> Result<PreparedPerformanceGate, String> {
    let actual_hash = context.executable_sha256.ok_or_else(|| {
        "depth 必须拿到真实 executable_sha256，不能用空值核验性能认证".to_string()
    })?;
    let (profile_bytes, binding_bytes) =
        resolve_catalog_depth(context.catalog_selection, context.game_id, actual_hash)?;
    let profile: depth_lab::DepthProfile = serde_json::from_slice(&profile_bytes)
        .map_err(|error| format!("解析 depth profile 失败: {error}"))?;
    injection_transport(
        profile.renderer_backend,
        &profile.game_id,
        &profile.executable_sha256,
    )?;
    let binding: depth_lab::CameraBindingDocument = serde_json::from_slice(&binding_bytes)
        .map_err(|error| format!("解析 CameraBinding 失败: {error}"))?;
    binding.validate_for_profile(&profile, actual_hash)?;
    context
        .catalog_selection
        .device_profile()
        .map_err(|e| format!("DEPTH_DEVICE_REQUIREMENTS_FAILED: {e:#}"))?
        .verify_hardware_evidence(
            context
                .hardware_evidence
                .ok_or("DEPTH_DEVICE_REQUIREMENTS_FAILED: 未取得本机硬件预检证据")?,
        )
        .map_err(|e| format!("DEPTH_DEVICE_REQUIREMENTS_FAILED: {e:#}"))?;
    let game_whitelisted = profile.game_id == context.game_id;
    let executable_sha256_matches = profile.executable_sha256 == actual_hash
        && binding.identity().executable_sha256 == actual_hash;
    let lane_matches = context.capture_fps_den != 0
        && u64::from(context.capture_fps_num)
            == u64::from(profile.lane.video_fps()) * u64::from(context.capture_fps_den);
    let tools_dir = default_tools_dir();
    let probe_dll_path = tools_dir.join("dm_depth_probe.dll");
    let probe_dll_bytes =
        std::fs::read(&probe_dll_path).map_err(|error| format!("读 probe DLL 失败: {error}"))?;
    let probe_dll_sha256 = sha256_bytes(&probe_dll_bytes);
    let (probe_runtime_manifest, probe_runtime_manifest_bytes) =
        load_probe_runtime_manifest(&tools_dir, &probe_dll_sha256)?;
    let gpu_driver_version = context
        .gpu_driver_version
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| "depth 性能认证必须拿到实际 GPU driver version".to_string())?;
    let perf_hardware = PerfHardwareIdentity {
        gpu_model: context.gpu_adapter_description.to_owned(),
        gpu_driver_version: gpu_driver_version.to_owned(),
        gpu_adapter_luid_high: context.gpu_adapter_luid_high,
        gpu_adapter_luid_low: context.gpu_adapter_luid_low,
    };
    perf_hardware.validate()?;
    let perf_key = PerfCertificationKey {
        game_id: profile.game_id.clone(),
        renderer_backend: profile.renderer_backend,
        backend_generation: probe_runtime_manifest.generation_for(profile.renderer_backend),
        depth_profile_sha256: sha256_bytes(&profile_bytes),
        camera_binding_sha256: sha256_bytes(&binding_bytes),
        selected_present_mode: "wgc_selected_present_gpu_ring_v1".into(),
        unbind_from_end: u32::try_from(unbind_from_end)
            .map_err(|_| "unbind_from_end exceeds certification key width".to_string())?,
        feedback_schema: depth_lab::selected_present_feedback::FEEDBACK_SCHEMA_V3,
        queue_capacity: depth_lab::selected_present_feedback::DEFAULT_QUEUE_CAPACITY as u32,
        ring_capacity: depth_lab::selected_present_feedback::DEFAULT_RING_CAPACITY,
        max_feedback_lag_presents:
            depth_lab::selected_present_feedback::DEFAULT_MAX_FEEDBACK_LAG_PRESENTS,
        guard_slots: depth_lab::selected_present_feedback::DEFAULT_RING_GUARD_SLOTS,
        width: 1280,
        height: 720,
        bytes_per_pixel: 4,
        vram_limit_bytes: 512 * 1024 * 1024,
        vram_reserve_bytes: 256 * 1024 * 1024,
    };
    perf_key.validate()?;
    Ok(PreparedPerformanceGate {
        profile_bytes,
        binding_bytes,
        profile,
        binding,
        game_whitelisted,
        executable_sha256_matches,
        lane_matches,
        tools_dir,
        probe_dll_bytes,
        probe_dll_sha256,
        probe_runtime_manifest,
        probe_runtime_manifest_bytes,
        perf_hardware,
        perf_key,
        device_profile_sha256: sha256_bytes(context.catalog_selection.device.source_bytes()),
        recording_profile_sha256: sha256_bytes(context.catalog_selection.recording.source_bytes()),
    })
}

pub fn present_start_failure(error: &str) -> (&'static str, &'static str, &'static str) {
    if error.contains("DEPTH_PRESENT_RATE_MISMATCH") {
        (
            "DEPTH_PRESENT_RATE_MISMATCH",
            "实际游戏帧率不满足基准工况，录制未开始。",
            "检查游戏限帧、帧生成和当前负载；实际帧率达标后重试，无需重跑四档。",
        )
    } else if error.contains("DEPTH_CERTIFICATION_BLOCKED") {
        (
            "DEPTH_CERTIFICATION_BLOCKED",
            "当前运行配置未匹配可用的基准认证。",
            "查看诊断中的缺失项；本地重跑四档不会自动发布线上凭证。",
        )
    } else if error.contains("DEPTH_DEVICE_REQUIREMENTS_FAILED") {
        (
            "DEPTH_DEVICE_REQUIREMENTS_FAILED",
            "本机硬件或驱动未通过游戏设备要求。",
            "查看所需版本与实测值，修正后重新预检。",
        )
    } else if error.contains("DEPTH_LOCAL_VALIDATION_FAILED")
        || error.contains("local_compatibility")
    {
        (
            "DEPTH_LOCAL_VALIDATION_FAILED",
            "本机采集条件校验未完成，录制未开始。",
            "检查游戏帧率及探针版本，保留诊断后重试。",
        )
    } else {
        (
            "DEPTH_PROBE_NOT_READY",
            "深度探针没有确认 Ready，录制未开始。",
            "检查启动状态、同帧来源和探针诊断；从 Depth 入口启动并重新绑定游戏。",
        )
    }
}

fn perf_certification_denial_message(denial: &PerfCertificationDenial) -> String {
    match denial {
        PerfCertificationDenial::HardwareNotCovered => {
            "当前 GPU 型号不在这份认证的明确覆盖名单里。".into()
        }
        PerfCertificationDenial::CertificationNotFound => {
            "当前游戏、profile/binding、帧率车道或 backend generation 没有匹配的线上认证。".into()
        }
        PerfCertificationDenial::RuntimeNotApproved => {
            "当前安装包携带的 Depth runtime 尚未进入线上批准名单。".into()
        }
        PerfCertificationDenial::BackendGenerationMismatch => {
            "当前 backend generation 与认证记录不一致。".into()
        }
        PerfCertificationDenial::DllHashMismatch => {
            "当前 Depth DLL 与随包 runtime manifest 的身份不一致。".into()
        }
        PerfCertificationDenial::InvalidRuntime(error) => {
            format!("当前 Depth runtime manifest 无效：{error}")
        }
        PerfCertificationDenial::InvalidCentralManifest(error) => {
            format!("线上性能认证清单无效：{error}")
        }
    }
}

pub fn check_performance_certification(
    context: DepthPerformanceCertificationContext<'_>,
) -> Result<PerfCertificationGateStatus, String> {
    let toggle = *TOGGLE.lock().map_err(|_| "depth toggle 锁已中毒")?;
    if !toggle.enabled_this_run {
        return Err("请先开启 Depth，再检查当前机器的性能认证".into());
    }
    let settings = runtime()
        .lock()
        .map_err(|_| "depth runtime 锁已中毒")?
        .settings
        .clone()
        .ok_or_else(|| "depth 已开启但没有配置 profile/binding/性能认证".to_string())?;
    if settings.performance_validation_only {
        return Err("性能校准模式只生成候选证据，不复用线上认证".into());
    }
    let prepared = prepare_performance_gate(context, settings.unbind_from_end)?;
    let base = |state, certification_id, detail| PerfCertificationGateStatus {
        state,
        certification_id,
        gpu_model: normalize_gpu_model(&prepared.perf_hardware.gpu_model),
        runtime_version: prepared
            .probe_runtime_manifest
            .runtime_version
            .trim()
            .to_string(),
        renderer_backend: prepared.profile.renderer_backend,
        backend_generation: prepared.perf_key.backend_generation,
        detail,
    };
    if !prepared.game_whitelisted {
        return Ok(base(
            PerfCertificationGateState::Blocked,
            None,
            "当前绑定游戏不在所选 Depth profile 的白名单中。".into(),
        ));
    }
    if !prepared.executable_sha256_matches {
        return Ok(base(
            PerfCertificationGateState::Blocked,
            None,
            "当前游戏可执行文件与 Depth profile/binding 的精确指纹不一致。".into(),
        ));
    }
    if !prepared.lane_matches {
        return Ok(base(
            PerfCertificationGateState::Blocked,
            None,
            "当前录制帧率车道与 Depth profile 不一致。".into(),
        ));
    }
    let central_manifest = load_active_manifest()?;
    match evaluate_device_profile_certification(
        &central_manifest,
        &prepared.probe_runtime_manifest,
        &prepared.probe_dll_sha256,
        &prepared.perf_key,
        &prepared.perf_hardware,
        DeviceProfileContext {
            device_profile_sha256: &prepared.device_profile_sha256,
            recording_profile_sha256: &prepared.recording_profile_sha256,
        },
    ) {
        Ok(certification) => Ok(base(
            if certification.device_profile_grant.is_some() {
                PerfCertificationGateState::BaselineMatched
            } else {
                PerfCertificationGateState::Passed
            },
            Some(certification.certification.certification_id.clone()),
            if let Some(grant) = certification.device_profile_grant {
                format!(
                    "已匹配可复用基准认证；要求游戏 {} FPS、录制视频 {} FPS，本机开录校验通过后即可录制，无需重跑四档。",
                    grant.required_present_fps,
                    prepared.profile.lane.video_fps()
                )
            } else {
                "已匹配线上签名性能认证；正式录制仍会再次执行同一套门禁。".into()
            },
        )),
        Err(denial) => Ok(base(
            PerfCertificationGateState::Blocked,
            None,
            perf_certification_denial_message(&denial),
        )),
    }
}

pub fn start_for_run(context: DepthStartContext<'_>) -> Result<Option<DepthProbeRunGuard>, String> {
    {
        let guard = runtime()
            .lock()
            .map_err(|_| "depth runtime lock is poisoned")?;
        if discovery_process_blocks_recording(guard.discovery.as_ref(), context.game_pid) {
            return Err(
                "this CP2077 process was launched discovery-only; Recording Ready is false and formal recording was not started"
                    .into(),
            );
        }
    }
    let toggle = *TOGGLE.lock().map_err(|_| "depth toggle 锁已中毒")?;
    if !toggle.enabled_this_run {
        return Ok(None);
    }
    let settings = runtime()
        .lock()
        .map_err(|_| "depth runtime 锁已中毒")?
        .settings
        .clone()
        .ok_or_else(|| "depth 已开启但没有配置 profile/binding/性能认证".to_string())?;
    let actual_hash = context
        .executable_sha256
        .ok_or_else(|| "depth 必须拿到真实 executable_sha256，不能用空值启动".to_string())?;
    let PreparedPerformanceGate {
        profile_bytes,
        binding_bytes,
        profile,
        binding,
        game_whitelisted,
        executable_sha256_matches,
        lane_matches,
        tools_dir,
        probe_dll_bytes,
        probe_dll_sha256,
        probe_runtime_manifest,
        probe_runtime_manifest_bytes,
        perf_hardware,
        perf_key,
        device_profile_sha256,
        recording_profile_sha256,
    } = prepare_performance_gate(
        DepthPerformanceCertificationContext {
            catalog_selection: context.catalog_selection,
            hardware_evidence: context.hardware_evidence,
            game_id: context.game_id,
            executable_sha256: context.executable_sha256,
            capture_fps_num: context.capture_fps_num,
            capture_fps_den: context.capture_fps_den,
            gpu_adapter_description: context.gpu_adapter_description,
            gpu_driver_version: context.gpu_driver_version,
            gpu_adapter_luid_high: context.gpu_adapter_luid_high,
            gpu_adapter_luid_low: context.gpu_adapter_luid_low,
        },
        settings.unbind_from_end,
    )?;
    let depth_profile_sha256 = perf_key.depth_profile_sha256.clone();
    let camera_binding_sha256 = perf_key.camera_binding_sha256.clone();
    let injection_transport = injection_transport(
        profile.renderer_backend,
        &profile.game_id,
        &profile.executable_sha256,
    )?;
    let certification_stage = if settings.performance_validation_only {
        prepare_perf_certification_key(&perf_key, &probe_runtime_manifest, &perf_hardware)?
    } else {
        None
    };
    let expected_swapchain_adapter = GpuAdapterIdentity {
        description: context.gpu_adapter_description.to_owned(),
        driver_version: perf_hardware.gpu_driver_version.clone(),
        luid_high: context.gpu_adapter_luid_high,
        luid_low: context.gpu_adapter_luid_low,
    };
    let mut required_present_fps = None;
    let mut inherited_evidence = None;
    let fps_gate_certified = if settings.performance_validation_only {
        false
    } else {
        let central_manifest = load_active_manifest()?;
        let matched = evaluate_device_profile_certification(
            &central_manifest,
            &probe_runtime_manifest,
            &probe_dll_sha256,
            &perf_key,
            &perf_hardware,
            DeviceProfileContext {
                device_profile_sha256: &device_profile_sha256,
                recording_profile_sha256: &recording_profile_sha256,
            },
        )
        .map_err(|denial| {
            format!(
                "DEPTH_CERTIFICATION_BLOCKED: {} ({denial:?})",
                perf_certification_denial_message(&denial)
            )
        })?;
        required_present_fps = matched.device_profile_grant.map(|g| g.required_present_fps);
        if required_present_fps.is_some() {
            inherited_evidence = Some(serde_json::json!({
                "schema": "depth-inherited-certification/v1",
                "baselineCertificationId": matched.certification.certification_id,
                "baselineGpuModel": matched.certification.baseline_hardware.gpu_model,
                "coverage": "device_profile",
                "deviceProfileSha256": device_profile_sha256,
                "recordingProfileSha256": recording_profile_sha256,
                "requiredPresentFps": required_present_fps,
                "recordingFps": profile.lane.video_fps(),
                "actualHardware": perf_hardware,
                "hardwarePreflightPassed": true,
                "locallyMeasuredPerformanceCertificate": false
            }));
        }
        true
    };
    let facts = collect_facts(
        game_whitelisted,
        executable_sha256_matches,
        probe_bitness_matches(context.game_pid)?,
        lane_matches,
        context.free_disk_bytes,
        settings.planned_seconds,
        depth_lab::integration::depth_bytes_per_hour(u64::from(profile.depth_hz())),
        fps_gate_certified,
        &perf_hardware,
        &probe_runtime_manifest,
        settings.performance_validation_only,
    );
    evaluate_gates(toggle, &facts).map_err(|denial| {
        let diagnostic = denial.diagnostic().unwrap_or_default();
        format!("{} {diagnostic}", denial.annotator_message())
    })?;
    if let InjectionTransport::EarlyD3d12(target) = injection_transport {
        let guard = runtime()
            .lock()
            .map_err(|_| "depth runtime lock is poisoned")?;
        verify_early_launch_record(
            guard.early_launch.as_ref(),
            context.game_id,
            context.game_pid,
            actual_hash,
            &depth_profile_sha256,
            &probe_dll_sha256,
            target,
        )?;
    }

    std::fs::create_dir_all(context.output_dir)
        .map_err(|error| format!("创建 depth run 目录失败: {error}"))?;
    if settings.performance_validation_only {
        let marker = serde_json::to_vec_pretty(&serde_json::json!({
            "schema": "dm-depth-performance-validation-only-v1",
            "deliverable": false,
            "certificationCandidateWritten": false,
            "runtime": &probe_runtime_manifest,
            "hardware": &perf_hardware,
            "key": &perf_key,
        }))
        .map_err(|error| format!("序列化 performance validation 标记失败: {error}"))?;
        write_run_snapshot(
            context.output_dir,
            "performance-validation-only",
            "json",
            &marker,
        )?;
    }
    if let Some(evidence) = &inherited_evidence {
        write_run_snapshot(
            context.output_dir,
            "inherited-certification",
            "json",
            &serde_json::to_vec_pretty(evidence).map_err(|e| e.to_string())?,
        )?;
    }
    let run_profile_path =
        write_run_snapshot(context.output_dir, "depth-profile", "json", &profile_bytes)?;
    let run_binding_path =
        write_run_snapshot(context.output_dir, "camera-binding", "json", &binding_bytes)?;
    write_run_snapshot(
        context.output_dir,
        "depth-probe-runtime",
        "json",
        &probe_runtime_manifest_bytes,
    )?;
    let run_probe_path = write_run_snapshot(
        context.output_dir,
        "dm_depth_probe",
        "dll",
        &probe_dll_bytes,
    )?;
    let identity = depth_lab::StreamIdentity {
        session_id: context.session_id.to_owned(),
        run_id: context.run_id.to_owned(),
        generation: context.generation,
    };
    let expected_profile = DepthProfileIdentity {
        schema_version: profile.schema_version.clone(),
        game_id: profile.game_id.clone(),
        lane: match profile.lane {
            depth_lab::Lane::Lane30 => "lane30",
            depth_lab::Lane::Lane60 => "lane60",
        }
        .into(),
        executable_sha256: profile.executable_sha256.clone(),
    };
    let expected_binding = binding.identity();
    let command_id = uuid::Uuid::new_v4().to_string();
    let selected_present_feedback_required =
        certification_stage_requires_selected_present_feedback(certification_stage);
    let present_identity_required =
        certification_stage_requires_present_identity(certification_stage);
    let (selected_present_feedback, feedback, expected_resource_epoch) =
        if selected_present_feedback_required {
            let feedback_nonce = uuid::Uuid::new_v4().to_string();
            let feedback_bootstrap = depth_lab::SelectedPresentFeedbackBootstrap {
                client_process_id: std::process::id(),
                nonce: feedback_nonce.clone(),
                resource_epoch: 1,
                queue_capacity: perf_key.queue_capacity,
                ring_capacity: perf_key.ring_capacity,
                max_feedback_lag_presents: perf_key.max_feedback_lag_presents,
                guard_slots: perf_key.guard_slots,
                width: perf_key.width,
                height: perf_key.height,
                bytes_per_pixel: perf_key.bytes_per_pixel,
                vram_limit_bytes: perf_key.vram_limit_bytes,
                vram_reserve_bytes: perf_key.vram_reserve_bytes,
            };
            feedback_bootstrap
                .validate()
                .map_err(|error| format!("selected-Present feedback config invalid: {error}"))?;
            let feedback_stream = depth_lab::selected_present_feedback::StreamEpoch {
                identity: depth_lab::selected_present_feedback::WireIdentity::new(
                    context.session_id,
                    context.run_id,
                )
                .map_err(|error| error.to_string())?,
                generation: context.generation,
                resource_epoch: feedback_bootstrap.resource_epoch,
            };
            let feedback_names = depth_lab::selected_present_transport::TransportNames::new(
                feedback_bootstrap.client_process_id,
                context.game_pid,
                context.generation,
                &feedback_nonce,
            )
            .map_err(|error| error.to_string())?;
            let feedback = depth_lab::selected_present_transport::ClientTransport::create(
                &feedback_names,
                &feedback_stream,
                feedback_bootstrap.queue_capacity as usize,
            )
            .map_err(|error| {
                format!("create selected-Present feedback transport failed: {error}")
            })?;
            let resource_epoch = feedback_bootstrap.resource_epoch;
            (
                Some(feedback_bootstrap),
                Some(feedback),
                Some(resource_epoch),
            )
        } else {
            (None, None, None)
        };
    let command = ControlEnvelope {
        schema: CONTROL_SCHEMA_V3.into(),
        command_id,
        identity: identity.clone(),
        command: ControlCommand::StartRun(StartRun {
            target_process_id: context.game_pid,
            output_dir: context.output_dir.to_string_lossy().into_owned(),
            unbind_from_end: settings.unbind_from_end,
            max_frames: None,
            target_depth_hz: f64::from(profile.depth_hz()),
            // 车道帧率单独传：present_rate 闸要拿它判「游戏锁没锁帧」，
            // 不能让 probe 从 target_depth_hz 反推（#801）。
            lane_fps: Some(profile.lane.video_fps()),
            required_present_fps,
            depth_profile_path: run_profile_path.to_string_lossy().into_owned(),
            depth_profile_sha256: depth_profile_sha256.clone(),
            camera_binding_path: run_binding_path.to_string_lossy().into_owned(),
            camera_binding_sha256: camera_binding_sha256.clone(),
            pass_identity_trace: false,
            performance_metrics_enabled: settings.performance_metrics_enabled,
            performance_validation_only: settings.performance_validation_only,
            present_only: certification_stage == Some(Stage::Baseline),
            watermark_only: matches!(
                certification_stage,
                Some(Stage::Baseline | Stage::WatermarkOnly)
            ),
            writer_bypass: certification_stage == Some(Stage::ReadbackOnly),
            selected_present_feedback,
        }),
    };
    {
        let mut runtime = runtime().lock().map_err(|_| "depth runtime 锁已中毒")?;
        let mut next = runtime.lifecycle.clone();
        next.begin_start(&command)
            .map_err(|error| format!("depth StartRun 状态不合法: {error}"))?;
        if let Err(error) = depth_lab::probe_control::write_command(
            &depth_lab::probe_control::control_path(),
            &command,
        ) {
            let error = format!("写 depth StartRun 失败: {error}");
            runtime.last_error = Some(error.clone());
            return Err(error);
        }
        runtime.lifecycle = next;
        runtime.last_error = None;
    }
    if injection_transport == InjectionTransport::LateD3d11 {
        if let Err(error) = launch_injector(
            &tools_dir,
            context.game_pid,
            &run_probe_path,
            &probe_dll_sha256,
        ) {
            record_runtime_error(error.clone());
            best_effort_stop_startup(&identity, context.output_dir, settings.stop_timeout_ms);
            return Err(error);
        }
    } else if let Ok(mut runtime) = runtime().lock() {
        runtime.early_launch_state = EarlyLaunchState::Bound;
    }
    let start_timeout = Duration::from_millis(depth_lab::probe_control::client_ready_timeout_ms(
        settings.start_timeout_ms,
        &expected_binding.schema,
    ));
    if let Err(error) = wait_for_state(
        context.output_dir,
        ClientLifecycleState::Ready,
        start_timeout,
        Some((
            &expected_profile,
            &expected_binding,
            &depth_profile_sha256,
            &camera_binding_sha256,
            &probe_runtime_manifest.runtime_version,
            &expected_swapchain_adapter,
            settings.performance_metrics_enabled,
        )),
    ) {
        record_runtime_error(error.clone());
        best_effort_stop_startup(&identity, context.output_dir, settings.stop_timeout_ms);
        return Err(error);
    }
    let (first_present_seq, ready_resource_epoch) = {
        let runtime = runtime()
            .lock()
            .map_err(|_| "depth runtime lock is poisoned".to_string())?;
        (
            runtime
                .lifecycle
                .ready_first_present_seq()
                .ok_or_else(|| "depth Ready did not preserve first_present_seq".to_string())?,
            runtime
                .lifecycle
                .ready_resource_epoch()
                .ok_or_else(|| "depth Ready did not preserve resource_epoch".to_string())?,
        )
    };
    if let Some(expected_resource_epoch) = expected_resource_epoch
        && ready_resource_epoch != expected_resource_epoch
    {
        let error = format!(
            "depth Ready resource_epoch {ready_resource_epoch} does not match selected-Present mapping epoch {expected_resource_epoch}"
        );
        record_runtime_error(error.clone());
        best_effort_stop_startup(&identity, context.output_dir, settings.stop_timeout_ms);
        return Err(error);
    }
    if let Some(fps) = required_present_fps {
        let validation = (|| {
            let path = context
                .output_dir
                .join(depth_lab::compatibility::RECEIPT_FILE);
            let bytes = std::fs::read(&path)
                .map_err(|e| format!("DEPTH_LOCAL_VALIDATION_FAILED: 缺少本机帧率校验回执: {e}"))?;
            let receipt: depth_lab::compatibility::LocalCompatibilityReceipt =
                serde_json::from_slice(&bytes)
                    .map_err(|e| format!("DEPTH_LOCAL_VALIDATION_FAILED: 无效回执: {e}"))?;
            receipt.validate(&identity, fps)
        })();
        if let Err(error) = validation {
            record_runtime_error(error.clone());
            best_effort_stop_startup(&identity, context.output_dir, settings.stop_timeout_ms);
            return Err(error);
        }
    }
    Ok(Some(DepthProbeRunGuard {
        identity,
        first_present_seq,
        watermark_source_width: binding.render_size().0,
        watermark_source_height: binding.render_size().1,
        output_dir: context.output_dir.to_path_buf(),
        stop_timeout: Duration::from_millis(settings.stop_timeout_ms),
        active: true,
        confirmed: None,
        feedback,
        selected_present_feedback_required,
        present_identity_required,
        performance_validation_only: settings.performance_validation_only,
        local_cp2077_audit: requires_local_cp2077_audit(
            &profile.game_id,
            settings.performance_validation_only,
            certification_stage,
        ),
    }))
}

fn resolve_formal_early_launch_target(
    renderer_backend: depth_lab::RendererBackend,
    game_id: &str,
    executable_sha256: &str,
) -> Result<Option<EarlyLaunchTarget>, String> {
    if renderer_backend == depth_lab::RendererBackend::D3d11 {
        return Ok(None);
    }
    if renderer_backend == depth_lab::RendererBackend::D3d12 {
        let target = EarlyLaunchTarget::from_formal_game_id(game_id).ok_or_else(|| {
            format!(
                "depth profile requires unsupported renderer/game combination {renderer_backend:?}/{game_id}; recording was not started"
            )
        })?;
        target.verify_executable_identity(game_id, executable_sha256)?;
        return Ok(Some(target));
    }
    Err(format!(
        "depth profile requires unsupported renderer/game combination {renderer_backend:?}/{game_id}; recording was not started"
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InjectionTransport {
    LateD3d11,
    EarlyD3d12(EarlyLaunchTarget),
}

fn verify_early_launch_record(
    record: Option<&EarlyLaunchRecord>,
    game_id: &str,
    game_pid: u32,
    executable_sha256: &str,
    depth_profile_sha256: &str,
    probe_sha256: &str,
    target: EarlyLaunchTarget,
) -> Result<(), String> {
    let record = record.ok_or(
        "D3D12 depth requires an allowlisted early launcher; late injection is forbidden and recording was not started",
    )?;
    target.verify_executable_identity(game_id, executable_sha256)?;
    if record.target != target
        || record.process_id != game_pid
        || !valid_launch_id(&record.launch_id)
        || !record
            .executable_sha256
            .eq_ignore_ascii_case(executable_sha256)
        || !record
            .depth_profile_sha256
            .eq_ignore_ascii_case(depth_profile_sha256)
        || !record
            .resident_probe_sha256
            .eq_ignore_ascii_case(probe_sha256)
    {
        return Err(
            "bound game PID/executable/profile/probe identity does not match the verified allowlisted early launch; recording was not started"
                .into(),
        );
    }
    Ok(())
}

fn injection_transport(
    renderer_backend: depth_lab::RendererBackend,
    game_id: &str,
    executable_sha256: &str,
) -> Result<InjectionTransport, String> {
    match resolve_formal_early_launch_target(renderer_backend, game_id, executable_sha256)? {
        None => Ok(InjectionTransport::LateD3d11),
        Some(target) => Ok(InjectionTransport::EarlyD3d12(target)),
    }
}

fn wait_for_state(
    output_dir: &Path,
    target: ClientLifecycleState,
    timeout: Duration,
    expected_ready: Option<(
        &DepthProfileIdentity,
        &CameraBindingIdentity,
        &str,
        &str,
        &str,
        &GpuAdapterIdentity,
        bool,
    )>,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    loop {
        let acks = depth_lab::probe_control::read_acks(output_dir)
            .map_err(|error| format!("读 depth ACK 失败: {error}"))?;
        for ack in acks {
            let mut runtime = runtime().lock().map_err(|_| "depth runtime 锁已中毒")?;
            if ack.ordinal <= runtime.lifecycle.last_ordinal() {
                if runtime.lifecycle.identity() != Some(&ack.identity) {
                    tracing::warn!(
                        ack_session_id = %ack.identity.session_id,
                        ack_run_id = %ack.identity.run_id,
                        ack_generation = ack.identity.generation,
                        ack_ordinal = ack.ordinal,
                        "discarded stale depth ACK that was already behind the accepted ordinal"
                    );
                }
                continue;
            }
            if matches!(
                runtime.lifecycle.state(),
                ClientLifecycleState::Flushed | ClientLifecycleState::Faulted
            ) {
                tracing::warn!(
                    ack_session_id = %ack.identity.session_id,
                    ack_run_id = %ack.identity.run_id,
                    ack_generation = ack.identity.generation,
                    ack_ordinal = ack.ordinal,
                    terminal_state = ?runtime.lifecycle.state(),
                    "discarded late depth ACK after a terminal lifecycle state"
                );
                continue;
            }
            if let (
                Some((
                    expected_profile,
                    expected_binding,
                    expected_profile_hash,
                    expected_binding_hash,
                    expected_probe_version,
                    expected_swapchain_adapter,
                    expected_performance_metrics_enabled,
                )),
                AckKind::Ready {
                    actual_profile,
                    actual_binding,
                    depth_profile_sha256,
                    camera_binding_sha256,
                    probe_version,
                    actual_swapchain_adapter,
                    performance_metrics_enabled,
                    ..
                },
            ) = (expected_ready, &ack.ack)
                && !ready_matches_expected(
                    actual_profile,
                    actual_binding,
                    depth_profile_sha256,
                    camera_binding_sha256,
                    probe_version,
                    expected_profile,
                    expected_binding,
                    expected_profile_hash,
                    expected_binding_hash,
                    expected_probe_version,
                    actual_swapchain_adapter,
                    expected_swapchain_adapter,
                    *performance_metrics_enabled,
                    expected_performance_metrics_enabled,
                )
            {
                runtime.lifecycle.consume_rejected_ready(&ack).map_err(|error| {
                    format!(
                        "Ready provenance mismatch could not be consumed before startup cleanup: {error}"
                    )
                })?;
                runtime.last_error = Some(
                    "Ready 回显的 profile/CameraBinding/hash/probe version/实际 swapchain adapter 与启动请求不一致".into(),
                );
                return Err(runtime.last_error.clone().expect("just inserted"));
            }
            runtime
                .lifecycle
                .apply_ack(&ack)
                .map_err(|error| format!("depth ACK 身份/顺序/状态校验失败（已拒绝）: {error}"))?;
            if let AckKind::Faulted { code, message, .. } = &ack.ack {
                let error = format!("probe fault {code}: {message}");
                runtime.last_error = Some(error.clone());
                return Err(error);
            }
        }
        let state = runtime()
            .lock()
            .map_err(|_| "depth runtime 锁已中毒")?
            .lifecycle
            .state();
        if state == target {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "等待 depth {target:?} 超时（当前 {state:?}）；配置写入不算 ACK"
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[allow(clippy::too_many_arguments)]
fn ready_matches_expected(
    actual_profile: &DepthProfileIdentity,
    actual_binding: &CameraBindingIdentity,
    actual_profile_hash: &str,
    actual_binding_hash: &str,
    actual_probe_version: &str,
    expected_profile: &DepthProfileIdentity,
    expected_binding: &CameraBindingIdentity,
    expected_profile_hash: &str,
    expected_binding_hash: &str,
    expected_probe_version: &str,
    actual_swapchain_adapter: &GpuAdapterIdentity,
    expected_swapchain_adapter: &GpuAdapterIdentity,
    actual_performance_metrics_enabled: bool,
    expected_performance_metrics_enabled: bool,
) -> bool {
    actual_profile == expected_profile
        && actual_binding == expected_binding
        && actual_profile_hash == expected_profile_hash
        && actual_binding_hash == expected_binding_hash
        && actual_probe_version == expected_probe_version
        && actual_swapchain_adapter == expected_swapchain_adapter
        && actual_performance_metrics_enabled == expected_performance_metrics_enabled
}

fn best_effort_stop_startup(
    identity: &depth_lab::StreamIdentity,
    output_dir: &Path,
    timeout_ms: u64,
) {
    // Preserve the error that triggered cleanup. Consuming a later probe Fault
    // ACK may update runtime.last_error while we wait, but that cleanup result
    // must remain secondary in both the UI and exported diagnostics.
    let primary_error = runtime()
        .lock()
        .ok()
        .and_then(|runtime| runtime.last_error.clone());
    if runtime()
        .lock()
        .ok()
        .is_some_and(|runtime| runtime.lifecycle.state() == ClientLifecycleState::Faulted)
    {
        return;
    }
    let command = ControlEnvelope {
        schema: CONTROL_SCHEMA_V3.into(),
        command_id: uuid::Uuid::new_v4().to_string(),
        identity: identity.clone(),
        command: ControlCommand::StopRun(StopRun {
            cutoff_qpc: current_qpc().unwrap_or(1),
            reason: "start_failed".into(),
        }),
    };
    let began = runtime().lock().ok().is_some_and(|mut runtime| {
        let mut next = runtime.lifecycle.clone();
        if next.begin_stop(&command).is_err()
            || depth_lab::probe_control::write_command(
                &depth_lab::probe_control::control_path(),
                &command,
            )
            .is_err()
        {
            return false;
        }
        runtime.lifecycle = next;
        true
    });
    if !began {
        mark_startup_cleanup_fault(
            identity,
            primary_error,
            "depth startup cleanup command failed".into(),
        );
        return;
    }
    if let Err(error) = wait_for_state(
        output_dir,
        ClientLifecycleState::Flushed,
        Duration::from_millis(timeout_ms),
        None,
    ) {
        mark_startup_cleanup_fault(
            identity,
            primary_error,
            format!("depth startup cleanup 未确认 Flushed: {error}"),
        );
    }
}

fn launch_injector(
    tools_dir: &Path,
    game_pid: u32,
    dll: &Path,
    expected_probe_sha256: &str,
) -> Result<(), String> {
    let injector = tools_dir.join("dm-depth-inject.exe");
    if !injector.is_file() || !dll.is_file() {
        return Err(format!(
            "depth 工具不完整: injector={}, probe={}",
            injector.display(),
            dll.display()
        ));
    }
    let actual_probe_sha256 = sha256_file(dll)?;
    if !actual_probe_sha256.eq_ignore_ascii_case(expected_probe_sha256) {
        return Err(format!(
            "probe DLL changed after performance certification gate: expected={expected_probe_sha256} actual={actual_probe_sha256}"
        ));
    }
    let output = std::process::Command::new(&injector)
        .creation_flags(CREATE_NO_WINDOW.0)
        .arg("--pid")
        .arg(game_pid.to_string())
        .arg("--dll")
        .arg(dll)
        .arg("--expected-sha256")
        .arg(expected_probe_sha256)
        .arg("--allow-existing")
        .output()
        .map_err(|error| format!("启动 depth injector 失败: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "depth injector 失败（{:?}）: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let loaded_hash = stdout
        .lines()
        .find_map(|line| line.strip_prefix("loaded_probe_sha256="))
        .ok_or_else(|| "depth injector 未回显实际加载 DLL hash".to_string())?;
    if !loaded_hash
        .trim()
        .eq_ignore_ascii_case(expected_probe_sha256)
    {
        return Err(format!(
            "depth injector loaded DLL hash mismatch: expected={expected_probe_sha256} actual={loaded_hash}"
        ));
    }
    Ok(())
}

fn probe_bitness_matches(game_pid: u32) -> Result<bool, String> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        IsWow64Process, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, game_pid) }
        .map_err(|error| format!("打开目标进程检查位数失败: {error}"))?;
    let mut wow64 = windows::core::BOOL(0);
    let result = unsafe { IsWow64Process(process, &mut wow64) }
        .map_err(|error| format!("检查目标进程位数失败: {error}"))
        .map(|()| wow64.as_bool() == cfg!(target_pointer_width = "32"));
    unsafe {
        let _ = CloseHandle(process);
    }
    result
}

fn current_qpc() -> Result<i64, String> {
    use windows::Win32::System::Performance::QueryPerformanceCounter;
    let mut value = 0i64;
    unsafe { QueryPerformanceCounter(&mut value) }
        .map_err(|error| format!("QueryPerformanceCounter 失败: {error}"))?;
    if value <= 0 {
        return Err("QueryPerformanceCounter 返回非正值".into());
    }
    Ok(value)
}

pub fn current_qpc_for_cleanup() -> i64 {
    current_qpc().unwrap_or(1)
}

fn record_runtime_error(error: String) {
    if let Ok(mut runtime) = runtime().lock() {
        runtime.last_error = Some(error);
    }
}

fn mark_local_fault(identity: &depth_lab::StreamIdentity, error: String) {
    if let Ok(mut runtime) = runtime().lock() {
        let _ = runtime.lifecycle.fail_local(identity);
        runtime.last_error = Some(error);
    }
}

fn compose_startup_cleanup_error(primary_error: Option<String>, cleanup_error: String) -> String {
    match primary_error.filter(|error| !error.is_empty()) {
        Some(primary) => format!("{primary}; cleanup also failed: {cleanup_error}"),
        None => cleanup_error,
    }
}

fn mark_startup_cleanup_fault(
    identity: &depth_lab::StreamIdentity,
    primary_error: Option<String>,
    cleanup_error: String,
) {
    if let Ok(mut runtime) = runtime().lock() {
        let _ = runtime.lifecycle.fail_local(identity);
        let primary_error = primary_error.or_else(|| runtime.last_error.take());
        runtime.last_error = Some(compose_startup_cleanup_error(primary_error, cleanup_error));
    }
}

/// 给前端的状态。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DepthLabStatus {
    /// 开关当前是否打开。
    pub enabled: bool,
    /// 是否已确认风险清单。
    pub risk_acknowledged: bool,
    /// 现在能不能真的开始采集。
    pub can_capture: bool,
    /// 不能的话，给采集员看的一句话。
    pub blocked_reason: Option<String>,
    /// 预计每小时占用字节，前端用来显示磁盘提示。
    pub bytes_per_hour: u64,
}

/// 从各处收集事实。
///
/// 参数由调用方（orchestrator）提供——这个函数刻意不自己去各个服务里拿，
/// 那样会让它变得不可测且与 orchestrator 的生命周期耦合。
#[allow(clippy::too_many_arguments)]
pub fn collect_facts(
    game_whitelisted: bool,
    executable_sha256_matches: bool,
    probe_bitness_matches: bool,
    lane_matches: bool,
    free_disk_bytes: u64,
    planned_seconds: u64,
    bytes_per_hour: u64,
    fps_gate_certified: bool,
    perf_hardware: &PerfHardwareIdentity,
    probe_runtime_manifest: &DepthProbeRuntimeManifest,
    performance_validation_only: bool,
) -> GateFacts {
    GateFacts {
        // 能编到这里就说明 feature 开着——这个字段不从外部传，
        // 免得被一个配置项覆盖掉「正式包永不含此能力」的保证。
        feature_compiled_in: true,
        game_whitelisted,
        executable_sha256_matches,
        probe_bitness_matches,
        lane_matches,
        free_disk_bytes,
        planned_seconds,
        bytes_per_hour,
        fps_gate_certified,
        performance_validation_only,
        // 诊断用。**归一化后存**，这样日志里印出来的就是实际参与匹配的那个串——
        // 而不是一个带着尾随空白、肉眼看不出差别的原始 DXGI Description。
        gpu_model: normalize_gpu_model(&perf_hardware.gpu_model),
        probe_version: probe_runtime_manifest.runtime_version.trim().to_string(),
    }
}

fn resolve_catalog_depth(
    selection: &CatalogSelection,
    game_id: &str,
    running_executable_sha256: &str,
) -> Result<(Vec<u8>, Vec<u8>), String> {
    let profile_ref =
        selection
            .depth_profile
            .as_ref()
            .map(|entry| depth_lab::catalog_depth::CatalogPayloadRef {
                id: entry.identity.id.clone(),
                version: entry.identity.version.clone(),
                content_hash: entry.identity.content_hash.clone(),
            });
    let binding_ref = selection.camera_binding.as_ref().map(|entry| {
        depth_lab::catalog_depth::CatalogPayloadRef {
            id: entry.identity.id.clone(),
            version: entry.identity.version.clone(),
            content_hash: entry.identity.content_hash.clone(),
        }
    });
    let input = depth_lab::catalog_depth::DepthCatalogInput {
        game_id,
        profile_ref: profile_ref.as_ref(),
        binding_ref: binding_ref.as_ref(),
        profile_bytes: selection
            .depth_profile
            .as_ref()
            .map(|entry| entry.source_bytes()),
        binding_bytes: selection
            .camera_binding
            .as_ref()
            .map(|entry| entry.source_bytes()),
        running_executable_sha256,
    };
    match depth_lab::catalog_depth::resolve(&input, sha256_bytes) {
        Ok(_) => Ok((
            input
                .profile_bytes
                .expect("resolved catalog has a depth profile")
                .to_vec(),
            input
                .binding_bytes
                .expect("resolved catalog has a camera binding")
                .to_vec(),
        )),
        Err(refusal) => {
            let mut message = refusal.annotator_message();
            if let Some(diagnostic) = refusal.diagnostic() {
                message.push(' ');
                message.push_str(&diagnostic);
            }
            Err(message)
        }
    }
}

fn sha256_bytes(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(path)
        .map_err(|error| format!("读取 probe DLL {} 失败: {error}", path.display()))?;
    Ok(sha256_bytes(&bytes))
}

fn write_run_snapshot(
    output_dir: &Path,
    stem: &str,
    extension: &str,
    bytes: &[u8],
) -> Result<PathBuf, String> {
    let digest = sha256_bytes(bytes);
    let digest_hex = digest
        .strip_prefix("sha256:")
        .ok_or_else(|| "internal SHA-256 format is invalid".to_string())?;
    let directory = output_dir.join("provenance");
    std::fs::create_dir_all(&directory)
        .map_err(|error| format!("创建 run provenance 目录失败: {error}"))?;
    let path = directory.join(format!("{stem}-{digest_hex}.{extension}"));
    match OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(mut file) => {
            file.write_all(bytes)
                .map_err(|error| format!("写 run snapshot {} 失败: {error}", path.display()))?;
            file.sync_all()
                .map_err(|error| format!("同步 run snapshot {} 失败: {error}", path.display()))?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let existing = std::fs::read(&path)
                .map_err(|read| format!("读取已有 run snapshot {} 失败: {read}", path.display()))?;
            if existing != bytes {
                return Err(format!(
                    "已有 run snapshot 内容与 hash 命名冲突: {}",
                    path.display()
                ));
            }
        }
        Err(error) => {
            return Err(format!(
                "创建 run snapshot {} 失败: {error}",
                path.display()
            ));
        }
    }
    let mut permissions = std::fs::metadata(&path)
        .map_err(|error| format!("读取 run snapshot 权限失败: {error}"))?
        .permissions();
    permissions.set_readonly(true);
    std::fs::set_permissions(&path, permissions)
        .map_err(|error| format!("锁定 run snapshot 只读失败: {error}"))?;
    if sha256_file(&path)?.eq_ignore_ascii_case(&digest) {
        Ok(path)
    } else {
        Err(format!(
            "run snapshot 写入后 hash 不一致: {}",
            path.display()
        ))
    }
}

/// 当前状态。前端每次进设置页调一次。
pub fn status(facts: &GateFacts) -> DepthLabStatus {
    let toggle = *TOGGLE.lock().expect("depth toggle 锁");
    let verdict = evaluate_gates(toggle, facts);
    DepthLabStatus {
        enabled: toggle.enabled_this_run,
        risk_acknowledged: toggle.risk_acknowledged,
        can_capture: verdict.is_ok(),
        blocked_reason: verdict.err().map(|d: GateDenial| d.annotator_message()),
        bytes_per_hour: facts.bytes_per_hour,
    }
}

/// 采集员点开关。
///
/// `acknowledged` 来自那个逐条勾选的风险确认框。**打开必须带确认**——
/// 关闭则不需要，任何时候都应该能立刻关掉（§5.3 让采集员遇到卡顿立刻关）。
pub fn set_enabled(enabled: bool, acknowledged: bool) {
    let mut t = TOGGLE.lock().expect("depth toggle 锁");
    if enabled {
        t.enabled_this_run = acknowledged;
        t.risk_acknowledged = acknowledged;
    } else {
        t.enabled_this_run = false;
    }
}

/// 进程重启即回到关闭。由 `lifecycle` 在启动时调用，也可在 session 结束后调用。
pub fn reset() {
    *TOGGLE.lock().expect("depth toggle 锁") = DepthToggle::default();
}

/// 起注入器。
///
/// **不自己做注入**——那段 `unsafe` 在 `dm-depth-inject.exe` 里，
/// 随 probe crate 一起被交叉编译验证过。这里只负责把配置和参数摆对。
///
/// # 配置走文件，不走环境变量
///
/// probe 被注入进**已经在跑的游戏进程**，它读到的 `env` 是**游戏进程**的环境——
/// 注入器上设的变量到不了它。所以注入前先把配置写到双方约定的路径
/// （`depth_lab::probe_config::config_path()`），probe 启动时去读。
///
/// 这个坑很隐蔽：env 版本在手工测试里一路都对（operator 先 set 再启动游戏），
/// 接进客户端后 probe 会安静地什么都拿不到，而注入本身是成功的。
pub fn spawn_injector(
    tools_dir: &Path,
    game_pid: u32,
    output_dir: &Path,
    unbind_from_end: usize,
    // 本次 capture profile 的车道标称帧率（30 / 60）。深度目标频率由它算出。
    lane_fps: u32,
    // 逐游戏 depth profile 的路径（投影参数 + metric_scale + Z 约定）。
    depth_profile_path: Option<&Path>,
    session_id: &str,
) -> Result<(), String> {
    let injector = tools_dir.join("dm-depth-inject.exe");
    let dll = tools_dir.join("dm_depth_probe.dll");
    if !injector.exists() {
        return Err(format!("找不到注入器：{}", injector.display()));
    }
    if !dll.exists() {
        return Err(format!("找不到 probe：{}", dll.display()));
    }
    std::fs::create_dir_all(output_dir)
        .map_err(|e| format!("建深度输出目录 {} 失败: {e}", output_dir.display()))?;

    depth_lab::probe_config::write(
        &depth_lab::probe_config::config_path(),
        &depth_lab::probe_config::ProbeConfig {
            enabled: true,
            output_dir: output_dir.to_string_lossy().into_owned(),
            unbind_from_end,
            max_frames: None,
            // **显式写入，不留给 probe 兜底。** probe 不知道车道是什么——
            // 它只看得到一个输出目录。车道信息在客户端这边，就该在这边落成数字。
            target_depth_hz: Some(depth_lab::cadence::default_target_hz(lane_fps)),
            lane_fps: Some(lane_fps),
            // P4 候选采集默认关。开着它跑的 session 不得用于交付，
            // 只用于识别矩阵槽位——所以不该由正常采集流程打开。
            camera_probe: false,
            camera_discovery: None,
            representative_scene_attestation_sha256: None,
            dsv_discovery: None,
            camera_production: None,
            present_only: false,
            watermark_only: false,
            writer_bypass: false,
            // 逐游戏 profile 的路径。给不出来时探针仍会采，但索引全是
            // proj_source=unknown，事后无法线性化成米（#490）。
            depth_profile_path: depth_profile_path.map(|p| p.to_string_lossy().into_owned()),
            depth_profile_sha256: None,
            session_id: session_id.to_string(),
        },
    )?;

    let out = std::process::Command::new(&injector)
        .arg("--pid")
        .arg(game_pid.to_string())
        .arg("--dll")
        .arg(&dll)
        .output()
        .map_err(|e| format!("启动注入器失败: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "注入失败（退出码 {:?}）: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

/// session 结束时把配置里的 `enabled` 翻掉。
///
/// **翻字段而不是删文件**：下一次注入前客户端会整份重写，
/// 而留着文件让排障时还能看到上一次用的是什么参数。
pub fn disable_probe_config() -> Result<(), String> {
    let path = depth_lab::probe_config::config_path();
    if !path.exists() {
        return Ok(());
    }
    let mut cfg =
        depth_lab::probe_config::load_from(&path).unwrap_or(depth_lab::probe_config::ProbeConfig {
            enabled: false,
            output_dir: String::new(),
            unbind_from_end: 4,
            max_frames: None,
            target_depth_hz: None,
            lane_fps: None,
            camera_probe: false,
            camera_discovery: None,
            representative_scene_attestation_sha256: None,
            dsv_discovery: None,
            camera_production: None,
            present_only: false,
            watermark_only: false,
            writer_bypass: false,
            depth_profile_path: None,
            depth_profile_sha256: None,
            session_id: String::new(),
        });
    cfg.enabled = false;
    depth_lab::probe_config::write(&path, &cfg)
}

/// 只能由客户端消费并验证真实 `Flushed` ACK 后构造的收口证明。
/// 配置关闭、队列猜成 0、手写 JSON 都拿不到这个类型。
#[derive(Debug, Clone)]
pub struct ConfirmedProbeFlush {
    evidence: ProbeFlushedEvidence,
    performance_validation_only: bool,
    summary: depth_lab::ProbeFlushSummary,
}

impl ConfirmedProbeFlush {
    pub(crate) fn new(
        identity: &depth_lab::StreamIdentity,
        summary: depth_lab::ProbeFlushSummary,
        performance_validation_only: bool,
    ) -> Result<Self, String> {
        summary.validate().map_err(|error| error.to_string())?;
        Ok(Self {
            evidence: ProbeFlushedEvidence {
                schema: depth_lab::segment_finalize::FLUSH_EVIDENCE_SCHEMA.into(),
                identity: StreamIdentity::new(
                    &identity.session_id,
                    &identity.run_id,
                    identity.generation,
                )
                .map_err(|error| error.to_string())?,
                writer_queue_depth: summary.writer_queue_depth,
                last_present_seq: summary.last_present_seq,
                cutoff_present_seq: summary.cutoff_present_seq,
            },
            performance_validation_only,
            summary,
        })
    }

    pub(crate) fn ensure_deliverable(&self) -> Result<(), String> {
        if self.performance_validation_only {
            Err("performance-validation-only run 永久不可生成或上传 depth archive".into())
        } else {
            Ok(())
        }
    }

    pub(crate) fn local_diagnostic_summary(&self) -> Result<&depth_lab::ProbeFlushSummary, String> {
        if !self.performance_validation_only {
            return Err("local diagnostic audit requires a validation-only run".into());
        }
        Ok(&self.summary)
    }
}

pub struct FinalizeDepthRunRequest<'a> {
    pub session_id: &'a str,
    pub run_id: &'a str,
    pub generation: u64,
    pub probe_root: &'a Path,
    pub run_directory: &'a Path,
    pub flushed: &'a ConfirmedProbeFlush,
    /// 来自未来通用状态分类器的显式边界；本模块不从 clear 值猜菜单/gameplay。
    pub frame_states: &'a FrameStateEvidence,
    pub pairs: &'a [FramePair],
    pub report: &'a E2eReport,
    pub raw_evidence: Option<&'a depth_lab::client_archive::RawRunEvidence>,
}

/// 按 journal 的精确 segment 区间收口，再把本地完成事实与独立 depth-lab queue
/// 同事务提交。production uploader 不读取这张队列表。
pub fn finalize_run_segments(
    journal: &SessionJournal,
    request: &FinalizeDepthRunRequest<'_>,
) -> Result<Vec<PublishedDepthSegment>, String> {
    request.flushed.ensure_deliverable()?;
    let identity = StreamIdentity::new(request.session_id, request.run_id, request.generation)
        .map_err(|error| error.to_string())?;
    let pairing = VerifiedPairingEvidence::new(
        identity.clone(),
        request.pairs.to_vec(),
        request.report.clone(),
    )
    .map_err(|error| error.to_string())?;
    let (fps_num, fps_den) = journal
        .depth_run_fps(request.session_id, request.run_id)
        .map_err(|error| format!("读取 depth run 车道失败: {error:#}"))?;
    let archive_lane = if fps_den.checked_mul(30) == Some(fps_num) {
        ArchiveLane::Lane30
    } else if fps_den.checked_mul(60) == Some(fps_num) {
        ArchiveLane::Lane60
    } else {
        return Err(format!(
            "depth archive 只接受 lane-30/lane-60，journal 记录为 {fps_num}/{fps_den} fps"
        ));
    };
    let boundaries = journal
        .depth_segment_boundaries(request.session_id, request.run_id)
        .map_err(|error| format!("读取 ready segment 边界失败: {error:#}"))?
        .into_iter()
        .map(|boundary| SegmentBoundary {
            segment_index: boundary.segment_index,
            session_frame_start: boundary.session_frame_start,
            frame_count: boundary.frame_count,
        })
        .collect::<Vec<_>>();
    let output_root = request.run_directory.join("depth-segments");
    let published = finalize_segments(&FinalizeRequest {
        identity: &identity,
        archive_lane,
        flushed: &request.flushed.evidence,
        pairing: &pairing,
        frame_states: request.frame_states,
        boundaries: &boundaries,
        probe_root: request.probe_root,
        output_root: &output_root,
    })
    .map_err(|error| error.to_string())?;

    let commits = published
        .iter()
        .map(|segment| {
            let relative = segment
                .manifest_path
                .strip_prefix(request.run_directory)
                .map_err(|_| {
                    format!(
                        "depth manifest {} 不在 run 目录 {} 内",
                        segment.manifest_path.display(),
                        request.run_directory.display()
                    )
                })?;
            let relative = relative
                .components()
                .map(|component| component.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            let (upload_relative_path, upload_sha256) = if segment.candidate_eligible {
                let upload_path = request
                    .run_directory
                    .join("depth-uploads")
                    .join(format!("depth_{:05}.zip", segment.segment_index));
                let upload = materialize_segment_upload(&SegmentUploadRequest {
                    identity: &identity,
                    segment_index: segment.segment_index,
                    manifest_path: &segment.manifest_path,
                    manifest_sha256: &segment.manifest_sha256,
                    output_path: &upload_path,
                    raw_evidence: request.raw_evidence,
                })
                .map_err(|error| error.to_string())?;
                if upload.frame_count == 0 || upload.size_bytes == 0 {
                    return Err("合格 depth segment 不得物化为空 ZIP".to_string());
                }
                (
                    Some(format!(
                        "runs/{}/depth-uploads/depth_{:05}.zip",
                        request.run_id, segment.segment_index
                    )),
                    Some(upload.sha256),
                )
            } else {
                (None, None)
            };
            Ok(DepthSegmentCommit {
                segment_index: segment.segment_index,
                generation: request.generation,
                manifest_relative_path: relative,
                manifest_sha256: segment.manifest_sha256.clone(),
                upload_relative_path,
                upload_sha256,
                candidate_eligible: segment.candidate_eligible,
                invalid_frame_count: u32::try_from(segment.invalid_frame_count)
                    .map_err(|_| "depth invalid frame count 超出 journal 范围".to_string())?,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    journal
        .commit_depth_segments(request.session_id, request.run_id, &commits)
        .map_err(|error| format!("提交 depth-lab durable queue 失败: {error:#}"))?;
    Ok(published)
}

/// 本次构建随包附带的工具目录。
#[must_use]
pub fn default_tools_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("depth-lab")
}

#[cfg(test)]
mod tests {
    use super::*;

    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn serial_test_guard() -> std::sync::MutexGuard<'static, ()> {
        TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn reset_runtime_for_test() {
        *runtime().lock().expect("depth runtime test lock") = Runtime::default();
    }

    /// 录制期轮询必须认出探针的自动停录，并且**只认这一个 code**。
    ///
    /// 顺带钉住「轮询是只读的」：它不能推进 `ClientLifecycle`，否则停止路径的
    /// `wait_for_state` 会因为 ordinal 被占掉而错过终态 ACK。
    #[test]
    fn a_probe_health_abort_is_seen_during_recording_and_only_that_code_is() {
        let _guard = serial_test_guard();
        reset_runtime_for_test();
        let root = std::env::temp_dir().join(format!(
            "dm-depth-health-abort-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let identity = begin_synthetic_start(&root);
        assert!(poll_health_abort(&root).is_none(), "还没出事就不该报");

        let fault = |ordinal: u32, code: &str| depth_lab::ProbeAck {
            schema: depth_lab::probe_control::ACK_SCHEMA_V3.into(),
            command_id: "start-timeout".into(),
            identity: identity.clone(),
            ordinal,
            ack: AckKind::Faulted {
                phase: "lifecycle".into(),
                code: code.into(),
                message: format!("{code} diagnostic body"),
                summary: None,
            },
        };
        // 别的 Fault 不归这条轮询管：它们要么发生在录制之前，要么由收口抓住。
        depth_lab::probe_control::write_ack(&root, &fault(1, "peer_exited")).unwrap();
        assert!(poll_health_abort(&root).is_none(), "不该被别的 Fault 触发");

        depth_lab::probe_control::write_ack(
            &root,
            &fault(2, depth_lab::health_abort::HEALTH_ABORT_FAULT_CODE),
        )
        .unwrap();
        let notice = poll_health_abort(&root).expect("自动停录必须被看见");
        assert!(notice.diagnostic.contains("diagnostic body"), "{notice:?}");
        // 探针的记录还没落盘时，退回一句不带数字的话，而不是编一个连续帧数
        assert!(notice.annotator.contains("连续多帧"), "{notice:?}");

        let mut abort = depth_lab::health_abort::HealthAbort::new(
            9_625,
            10_223,
            300,
            vec![depth_lab::frame_health::FrameDefect::RawUniform],
            300,
            9_800,
            vec![9_625],
        );
        abort.scene_errors.clear();
        depth_lab::health_abort::write_record(&root, &abort).unwrap();
        let notice = poll_health_abort(&root).expect("自动停录必须被看见");
        assert!(notice.annotator.contains("连续 300 帧"), "{notice:?}");

        // 只读：轮询过后状态机必须原地不动，停止路径才能照常消费这些 ACK
        assert_eq!(
            runtime().lock().unwrap().lifecycle.state(),
            ClientLifecycleState::Starting
        );
        assert_eq!(runtime().lock().unwrap().lifecycle.last_ordinal(), 0);
        reset_runtime_for_test();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn performance_outcome_ipc_is_camel_case_without_changing_the_core_contract() {
        let outcome = PerfRunOutcome {
            avg_fps_drop: 0.01602,
            low1_fps_drop: 0.08143,
            watermark_avg_drop: 0.005,
            readback_avg_drop: 0.004,
            writer_avg_drop: 0.00702,
            watermark_low1_drop: 0.010,
            readback_low1_drop: 0.020,
            writer_low1_drop: 0.05143,
            readback_only_bypassed_frames: 4_200,
            full_writer_timing: depth_lab::WriterTimingSummary {
                sample_count: 4_096,
                total_committed: 4_200,
                wrapped: true,
                over_budget_count: 17,
                duration_us: depth_lab::LatencyQuantiles {
                    p50: 10_000,
                    p95: 25_000,
                    p99: 60_000,
                    max: 119_300,
                },
            },
            passes: false,
        };
        let ipc = serde_json::to_value(PerfRunOutcomeIpc::from(&outcome)).unwrap();

        assert_eq!(ipc["avgFpsDrop"], 0.01602);
        assert_eq!(ipc["low1FpsDrop"], 0.08143);
        assert_eq!(ipc["maxAvgFpsDrop"], MAX_AVG_FPS_DROP);
        assert_eq!(ipc["maxLow1FpsDrop"], MAX_LOW1_FPS_DROP);
        assert_eq!(ipc["readbackOnlyBypassedFrames"], 4_200);
        assert_eq!(ipc["fullWriterTiming"]["durationUs"]["max"], 119_300);
        assert!(ipc.get("avg_fps_drop").is_none());

        // depth-lab 的可落盘/CLI 类型仍保持既有 snake_case 契约。
        let core = serde_json::to_value(outcome).unwrap();
        assert_eq!(core["avg_fps_drop"], 0.01602);
        assert!(core.get("avgFpsDrop").is_none());
    }

    fn performance_flush(avg_millifps: u64, low1_millifps: u64) -> depth_lab::ProbeFlushSummary {
        depth_lab::ProbeFlushSummary {
            writer_queue_depth: 0,
            last_present_seq: 20_000,
            cutoff_present_seq: 20_000,
            depth_row_count: 0,
            depth_file_count: 0,
            camera_pose_row_count: 20_000,
            camera_diagnostic_row_count: 0,
            player_pose_row_count: Some(20_000),
            selected_present: None,
            writer_bypass: false,
            writer_bypassed_frames: 0,
            writer_timing: None,
            performance_metrics_enabled: true,
            present_timing: Some(depth_lab::PresentTimingSummary {
                sample_count: 8_986,
                qpc_frequency: 10_000_000,
                qpc_start: 10_000_000,
                qpc_end: 3_010_000_000,
                measurement_window_start_qpc: 10_000_000,
                measurement_cutoff_qpc: 3_010_000_000,
                stop_cutoff_qpc: 3_060_000_000,
                avg_millifps,
                low1_millifps,
                slowest_one_percent: vec![
                    depth_lab::PresentIntervalEvidence {
                        start_present_seq: 1,
                        end_present_seq: 2,
                        start_qpc: 10_000_000,
                        end_qpc: 10_363_636,
                        duration_qpc: 363_636,
                    };
                    90
                ],
                wrapped: false,
            }),
            errors: Vec::new(),
        }
    }

    #[test]
    fn performance_measurement_comes_only_from_probe_present_evidence() {
        let request = PerfStageMeasurementRequest {
            certification_id: "cert".into(),
            stage: Stage::Baseline,
            target_fps: 30.0,
        };
        let measurement =
            measurement_from_probe_flush(&request, &performance_flush(29_950, 27_500)).unwrap();
        assert_eq!(measurement.avg_fps, 29.95);
        assert_eq!(measurement.low1_fps, 27.5);
        assert_eq!(measurement.seconds, 300.0);
        assert!(measurement.frame_capped);
    }

    #[test]
    fn performance_measurement_rejects_a_wrapped_present_ring() {
        let request = PerfStageMeasurementRequest {
            certification_id: "cert".into(),
            stage: Stage::Full,
            target_fps: 30.0,
        };
        let mut summary = performance_flush(29_950, 27_500);
        summary.present_timing.as_mut().unwrap().wrapped = true;
        let error = measurement_from_probe_flush(&request, &summary).unwrap_err();
        assert!(error.contains("计时环发生覆盖"), "{error}");
    }

    #[test]
    fn startup_cleanup_error_keeps_the_triggering_failure_first() {
        assert_eq!(
            compose_startup_cleanup_error(
                Some("watermark decode timed out".into()),
                "StopRun arrived before GenerationSealed".into(),
            ),
            "watermark decode timed out; cleanup also failed: StopRun arrived before GenerationSealed"
        );
    }

    #[test]
    fn baseline_and_watermark_only_do_not_require_selected_present_feedback() {
        assert!(certification_stage_requires_selected_present_feedback(None));
        assert!(!certification_stage_requires_selected_present_feedback(
            Some(Stage::Baseline)
        ));
        assert!(!certification_stage_requires_selected_present_feedback(
            Some(Stage::WatermarkOnly)
        ));
        assert!(certification_stage_requires_selected_present_feedback(
            Some(Stage::ReadbackOnly)
        ));
        assert!(certification_stage_requires_selected_present_feedback(
            Some(Stage::Full)
        ));
    }

    #[test]
    fn present_only_baseline_is_the_only_stage_without_present_identity() {
        assert!(certification_stage_requires_present_identity(None));
        assert!(!certification_stage_requires_present_identity(Some(
            Stage::Baseline
        )));
        assert!(certification_stage_requires_present_identity(Some(
            Stage::WatermarkOnly
        )));
        assert!(certification_stage_requires_present_identity(Some(
            Stage::ReadbackOnly
        )));
        assert!(certification_stage_requires_present_identity(Some(
            Stage::Full
        )));
    }

    fn begin_synthetic_start(root: &Path) -> depth_lab::StreamIdentity {
        let identity = depth_lab::StreamIdentity {
            session_id: "s-timeout".into(),
            run_id: "r0001".into(),
            generation: 7,
        };
        let command = ControlEnvelope {
            schema: CONTROL_SCHEMA_V3.into(),
            command_id: "start-timeout".into(),
            identity: identity.clone(),
            command: ControlCommand::StartRun(StartRun {
                target_process_id: 42,
                output_dir: root.to_string_lossy().into_owned(),
                unbind_from_end: 4,
                max_frames: None,
                target_depth_hz: 15.0,
                lane_fps: Some(30),
                required_present_fps: None,
                depth_profile_path: root.join("profile.json").to_string_lossy().into_owned(),
                depth_profile_sha256: format!("sha256:{}", "a".repeat(64)),
                camera_binding_path: root.join("binding.json").to_string_lossy().into_owned(),
                camera_binding_sha256: format!("sha256:{}", "b".repeat(64)),
                pass_identity_trace: false,
                performance_metrics_enabled: false,
                performance_validation_only: false,
                present_only: false,
                watermark_only: false,
                writer_bypass: false,
                selected_present_feedback: None,
            }),
        };
        runtime()
            .lock()
            .expect("depth runtime test lock")
            .lifecycle
            .begin_start(&command)
            .unwrap();
        identity
    }

    fn facts(ok: bool) -> GateFacts {
        GateFacts {
            feature_compiled_in: true,
            game_whitelisted: ok,
            executable_sha256_matches: true,
            probe_bitness_matches: true,
            lane_matches: true,
            free_disk_bytes: u64::MAX,
            planned_seconds: 60,
            bytes_per_hour: depth_lab::integration::depth_bytes_per_hour(30),
            fps_gate_certified: true,
            performance_validation_only: false,
            // 纯诊断字段——判定已在 fps_gate_certified 里做完。这里给一对
            // 具体值而不是空串：这两个字段存在的理由正是「记录里的串与本机
            // 报出来的对不上」，空串会让夹具看起来像"没有这回事"。
            gpu_model: "nvidia geforce rtx 4070".to_string(),
            probe_version: "0.7.1".to_string(),
        }
    }

    #[test]
    fn start_failure_reports_the_actual_gate() {
        assert_eq!(
            present_start_failure("DEPTH_CERTIFICATION_BLOCKED: HardwareNotCovered").0,
            "DEPTH_CERTIFICATION_BLOCKED"
        );
        assert_eq!(
            present_start_failure(
                "probe fault present_rate_mismatch: DEPTH_PRESENT_RATE_MISMATCH: 120"
            )
            .0,
            "DEPTH_PRESENT_RATE_MISMATCH"
        );
        assert_eq!(
            present_start_failure("DEPTH_LOCAL_VALIDATION_FAILED: missing receipt").0,
            "DEPTH_LOCAL_VALIDATION_FAILED"
        );
        assert_eq!(
            present_start_failure("DEPTH_DEVICE_REQUIREMENTS_FAILED: driver too old").0,
            "DEPTH_DEVICE_REQUIREMENTS_FAILED"
        );
        assert_eq!(
            present_start_failure("probe warmup_timeout").0,
            "DEPTH_PROBE_NOT_READY"
        );
    }

    #[test]
    fn packaged_runtime_manifest_must_match_the_exact_dll_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let dll = b"independent depth probe artifact";
        let dll_hash = sha256_bytes(dll);
        std::fs::write(temp.path().join("dm_depth_probe.dll"), dll).unwrap();
        std::fs::write(
            temp.path().join(PROBE_RUNTIME_MANIFEST_FILE),
            serde_json::to_vec(&DepthProbeRuntimeManifest {
                schema: depth_lab::certification::DEPTH_PROBE_RUNTIME_MANIFEST_SCHEMA_V1.into(),
                runtime_version: "depth-2026.08.1".into(),
                dll_sha256: dll_hash.clone(),
                d3d11_generation: 4,
                d3d12_generation: 9,
            })
            .unwrap(),
        )
        .unwrap();

        assert_eq!(
            load_probe_runtime_manifest(temp.path(), &dll_hash)
                .unwrap()
                .0
                .d3d12_generation,
            9
        );
        assert!(
            load_probe_runtime_manifest(temp.path(), &format!("sha256:{}", "f".repeat(64)),)
                .is_err()
        );
    }

    #[test]
    fn performance_reconfigure_preserves_only_the_matching_bound_early_launch() {
        let target = EarlyLaunchTarget::KingdomCome2SteamBuild23914554;
        let profile_hash = format!("sha256:{}", "a".repeat(64));
        let certification_id = "kcd2-preserve-early-launch".to_string();
        let mut runtime = Runtime {
            configured_game_id: Some(target.game_id().into()),
            configured_renderer_backend: Some(depth_lab::RendererBackend::D3d12),
            configured_depth_profile_sha256: Some(profile_hash.clone()),
            configured_early_launch_target: Some(target),
            early_launch_state: EarlyLaunchState::WaitingForWindow,
            early_launch: Some(EarlyLaunchRecord {
                target,
                process_id: 7,
                executable_sha256: target.executable_sha256().into(),
                depth_profile_sha256: profile_hash.clone(),
                resident_probe_sha256: format!("sha256:{}", "b".repeat(64)),
                launch_id: "c".repeat(32),
            }),
            perf_certification: Some(ClientPerfCertification {
                run: PerfRun::begin(certification_id.clone()),
                id: certification_id,
                key: None,
                runtime_manifest: None,
                hardware: None,
                outcome: None,
                candidate_path: None,
                completion_error: None,
            }),
            ..Runtime::default()
        };
        let settings = DepthProbeSettings {
            enabled: true,
            risk_acknowledged: true,
            unbind_from_end: 4,
            planned_seconds: 315,
            start_timeout_ms: MIN_START_TIMEOUT_MS,
            stop_timeout_ms: 30_000,
            performance_validation_only: true,
            performance_metrics_enabled: true,
        };
        let profile = ConfiguredDepthProfile {
            game_id: target.game_id().into(),
            renderer_backend: depth_lab::RendererBackend::D3d12,
            depth_profile_sha256: profile_hash.clone(),
            early_launch_target: Some(target),
        };

        assert!(preserves_verified_early_launch_for_performance_reconfigure(
            &runtime,
            &settings,
            Some(&profile),
        ));
        runtime.early_launch_state = EarlyLaunchState::Bound;
        assert!(preserves_verified_early_launch_for_performance_reconfigure(
            &runtime,
            &settings,
            Some(&profile),
        ));

        let mut formal_settings = settings.clone();
        formal_settings.performance_validation_only = false;
        assert!(
            !preserves_verified_early_launch_for_performance_reconfigure(
                &runtime,
                &formal_settings,
                Some(&profile),
            )
        );

        let changed_profile = ConfiguredDepthProfile {
            depth_profile_sha256: format!("sha256:{}", "d".repeat(64)),
            ..profile
        };
        assert!(
            !preserves_verified_early_launch_for_performance_reconfigure(
                &runtime,
                &settings,
                Some(&changed_profile),
            )
        );
    }

    #[test]
    fn certification_stage_start_requires_matching_validation_configuration() {
        let _serial = serial_test_guard();
        reset_runtime_for_test();
        let certification_id = "cert-stage-start".to_string();
        {
            let mut guard = runtime().lock().unwrap();
            guard.perf_certification = Some(ClientPerfCertification {
                run: PerfRun::begin(certification_id.clone()),
                id: certification_id.clone(),
                key: None,
                runtime_manifest: None,
                hardware: None,
                outcome: None,
                candidate_path: None,
                completion_error: None,
            });
            guard.settings = Some(DepthProbeSettings {
                enabled: true,
                risk_acknowledged: true,
                unbind_from_end: 4,
                planned_seconds: 300,
                start_timeout_ms: MIN_START_TIMEOUT_MS,
                stop_timeout_ms: 30_000,
                performance_validation_only: true,
                performance_metrics_enabled: true,
            });
        }
        assert!(perf_certification_blocks_generic_start(false).unwrap());
        assert_eq!(
            prepare_perf_certification_stage_start().unwrap(),
            Stage::Baseline
        );

        runtime().lock().unwrap().settings.as_mut().unwrap().enabled = false;
        let error = prepare_perf_certification_stage_start().unwrap_err();
        assert!(error.contains("当前阶段"), "{error}");

        {
            let mut guard = runtime().lock().unwrap();
            let certification = guard.perf_certification.as_mut().unwrap();
            for stage in Stage::ORDER {
                certification
                    .run
                    .record(
                        &certification_id,
                        StageMeasurement {
                            stage,
                            avg_fps: 60.0,
                            low1_fps: 45.0,
                            seconds: 300.0,
                            frame_capped: true,
                            writer_bypassed_frames: u64::from(stage == Stage::ReadbackOnly),
                            writer_timing: (stage == Stage::Full).then_some(
                                depth_lab::WriterTimingSummary {
                                    sample_count: 1,
                                    total_committed: 1,
                                    ..depth_lab::WriterTimingSummary::default()
                                },
                            ),
                        },
                    )
                    .unwrap();
            }
            certification.outcome = Some(certification.run.finish().unwrap());
        }
        assert!(!perf_certification_blocks_generic_start(false).unwrap());
        assert!(perf_certification_blocks_generic_start(true).unwrap());
        reset_runtime_for_test();
    }

    fn completed_certification(full_fps: f64) -> ClientPerfCertification {
        let id = "completion-test".to_string();
        let mut run = PerfRun::begin(id.clone());
        for stage in Stage::ORDER {
            run.record(
                &id,
                StageMeasurement {
                    stage,
                    avg_fps: if stage == Stage::Full { full_fps } else { 60.0 },
                    low1_fps: 45.0,
                    seconds: 300.0,
                    frame_capped: true,
                    writer_bypassed_frames: u64::from(stage == Stage::ReadbackOnly),
                    writer_timing: (stage == Stage::Full).then_some(
                        depth_lab::WriterTimingSummary {
                            sample_count: 1,
                            total_committed: 1,
                            ..depth_lab::WriterTimingSummary::default()
                        },
                    ),
                },
            )
            .unwrap();
        }
        ClientPerfCertification {
            id,
            run,
            key: Some(
                serde_json::from_value(serde_json::json!({
                    "gameId": "test-game", "rendererBackend": "d3d12", "backendGeneration": 7,
                    "depthProfileSha256": format!("sha256:{}", "a".repeat(64)),
                    "cameraBindingSha256": format!("sha256:{}", "b".repeat(64)),
                    "selectedPresentMode": "wgc_selected_present_gpu_ring_v1", "unbindFromEnd": 4,
                    "feedbackSchema": 1, "queueCapacity": 4, "ringCapacity": 8,
                    "maxFeedbackLagPresents": 2, "guardSlots": 1, "width": 1920, "height": 1080,
                    "bytesPerPixel": 4, "vramLimitBytes": 1000000000, "vramReserveBytes": 1000000
                }))
                .unwrap(),
            ),
            runtime_manifest: Some(DepthProbeRuntimeManifest {
                schema: depth_lab::certification::DEPTH_PROBE_RUNTIME_MANIFEST_SCHEMA_V1.into(),
                runtime_version: "depth-test".into(),
                dll_sha256: format!("sha256:{}", "c".repeat(64)),
                d3d11_generation: 3,
                d3d12_generation: 7,
            }),
            hardware: Some(PerfHardwareIdentity {
                gpu_model: "test-gpu".into(),
                gpu_driver_version: "test-driver".into(),
                gpu_adapter_luid_high: 0,
                gpu_adapter_luid_low: 1,
            }),
            outcome: None,
            candidate_path: None,
            completion_error: None,
        }
    }

    #[test]
    fn performance_completion_invalid_comparison_is_terminal_and_preserves_cause() {
        let _serial = serial_test_guard();
        reset_runtime_for_test();
        let mut certification = completed_certification(90.0);
        let error = complete_perf_certification(&mut certification, |_| panic!("must not export"))
            .unwrap_err();
        assert!(error.contains("UnstableComparison"));
        assert_eq!(
            certification.completion_error.as_deref(),
            Some(error.as_str())
        );
        assert!(certification.outcome.is_none());
        runtime().lock().unwrap().perf_certification = Some(certification);
        let status = perf_certification_status().unwrap();
        assert!(!status.active);
        assert!(status.next_stage.is_none());
        assert!(status.candidate_path.is_none());
        assert_eq!(status.completion_error.as_deref(), Some(error.as_str()));
        assert!(perf_certification_blocks_generic_start(true).unwrap());
        reset_runtime_for_test();
    }

    #[test]
    fn performance_completion_export_retry_keeps_four_measurements_and_result() {
        let mut certification = completed_certification(59.5);
        let error =
            complete_perf_certification(&mut certification, |_| Err("disk unavailable".into()))
                .unwrap_err();
        assert_eq!(error, "disk unavailable");
        let original = certification.outcome.clone().unwrap();
        assert!(certification.run.next_stage().is_none());
        assert!(certification.candidate_path.is_none());
        complete_perf_certification(&mut certification, |candidate| {
            candidate.validate().unwrap();
            assert_eq!(candidate.avg_fps_drop, original.avg_fps_drop);
            Ok(PathBuf::from("candidate.json"))
        })
        .unwrap();
        assert_eq!(certification.outcome.as_ref(), Some(&original));
        assert!(certification.completion_error.is_none());
        complete_perf_certification(&mut certification, |_| panic!("must not export twice"))
            .unwrap();
        assert_eq!(certification.run.finish().unwrap(), original);
    }

    #[test]
    fn performance_completion_retry_does_not_require_another_flush_or_accept_stale_identity() {
        let _serial = serial_test_guard();
        reset_runtime_for_test();
        let mut certification = completed_certification(59.5);
        certification.outcome = Some(certification.run.finish().unwrap());
        certification.candidate_path = Some(PathBuf::from("candidate.json"));
        runtime().lock().unwrap().perf_certification = Some(certification);
        let request = |id: &str, stage| PerfStageMeasurementRequest {
            certification_id: id.into(),
            stage,
            target_fps: 30.0,
        };
        assert!(record_perf_stage(request("stale-id", Stage::Full)).is_err());
        assert!(record_perf_stage(request("completion-test", Stage::Baseline)).is_err());
        let status = record_perf_stage(request("completion-test", Stage::Full)).unwrap();
        assert!(!status.active);
        assert!(status.outcome.is_some());
        assert_eq!(status.candidate_path.as_deref(), Some("candidate.json"));
        reset_runtime_for_test();
    }

    #[test]
    fn generations_are_monotonic_and_seeded_beyond_process_local_ordinals() {
        let first = next_generation().unwrap();
        let second = next_generation().unwrap();
        assert!(first > 1);
        assert_eq!(second, first + 1);
    }

    #[test]
    fn toggle_starts_off_and_needs_acknowledgement() {
        let _serial = serial_test_guard();
        reset();
        let s = status(&facts(true));
        assert!(!s.enabled);
        assert!(!s.can_capture);

        // 没勾确认就打开 → 打不开
        set_enabled(true, false);
        assert!(!status(&facts(true)).enabled);

        set_enabled(true, true);
        assert!(status(&facts(true)).enabled);
        reset();
    }

    #[test]
    fn turning_off_never_requires_acknowledgement() {
        let _serial = serial_test_guard();
        // §5.3：采集员遇到卡顿要能立刻关掉，不该再弹一次确认
        reset();
        set_enabled(true, true);
        set_enabled(false, false);
        assert!(!status(&facts(true)).enabled);
        reset();
    }

    #[test]
    fn configure_reserves_time_for_probe_warmup_fault_cleanup() {
        let _serial = serial_test_guard();
        reset_runtime_for_test();
        let root = std::env::temp_dir().join(format!(
            "dm-depth-start-timeout-config-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let settings = DepthProbeSettings {
            enabled: true,
            risk_acknowledged: true,
            unbind_from_end: 4,
            planned_seconds: 60,
            start_timeout_ms: MIN_START_TIMEOUT_MS - 1,
            stop_timeout_ms: 500,
            performance_validation_only: false,
            performance_metrics_enabled: false,
        };
        let error = configure(settings, None).unwrap_err();
        assert!(error.contains("start timeout"), "{error}");
        assert!(error.contains(&MIN_START_TIMEOUT_MS.to_string()), "{error}");
        reset_runtime_for_test();
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(feature = "dev-catalog")]
    #[test]
    fn fallout4_version_gate_runs_before_probe_files_or_performance_certification() {
        let store = crate::platform::windows::catalog::CatalogStore::load().unwrap();
        let game_id = "fallout4-steam-release-test";
        let selection = store.selection(game_id).unwrap();
        let profile: depth_lab::DepthProfile =
            serde_json::from_slice(selection.depth_profile.as_ref().unwrap().source_bytes())
                .unwrap();
        resolve_catalog_depth(&selection, game_id, &profile.executable_sha256).unwrap();

        let unverified_hash = format!("sha256:{}", "f".repeat(64));
        let error = prepare_performance_gate(
            DepthPerformanceCertificationContext {
                catalog_selection: &selection,
                hardware_evidence: None,
                game_id,
                executable_sha256: Some(&unverified_hash),
                capture_fps_num: 30,
                capture_fps_den: 1,
                gpu_adapter_description: "test-only GPU",
                gpu_driver_version: None,
                gpu_adapter_luid_high: 0,
                gpu_adapter_luid_low: 0,
            },
            4,
        )
        .err()
        .expect("unverified Fallout 4 must not reach probe files or performance lookup");
        assert!(error.contains("尚未完成深度采集适配"), "{error}");
        assert!(error.contains("重跑性能认证不能解除"), "{error}");
    }

    #[cfg(feature = "dev-catalog")]
    #[test]
    fn configure_uses_the_depth_pin_when_game_discovery_is_name_only() {
        let _serial = serial_test_guard();
        reset();
        reset_runtime_for_test();
        let store = crate::platform::windows::catalog::CatalogStore::load().unwrap();
        let game = store.game("fallout4-steam-release-test").unwrap();
        assert!(game.executable_sha256.is_none());
        let selection = store.selection("fallout4-steam-release-test").unwrap();

        let status = configure(
            DepthProbeSettings {
                enabled: true,
                risk_acknowledged: true,
                unbind_from_end: 4,
                planned_seconds: 60,
                start_timeout_ms: MIN_START_TIMEOUT_MS,
                stop_timeout_ms: 500,
                performance_validation_only: false,
                performance_metrics_enabled: false,
            },
            Some(&selection),
        )
        .unwrap();
        assert!(status.configured);
        assert_eq!(
            runtime().lock().unwrap().configured_game_id.as_deref(),
            Some("fallout4-steam-release-test")
        );
        reset();
        reset_runtime_for_test();
    }

    #[test]
    fn d3d12_formal_lane_opens_only_for_allowlisted_targets() {
        assert_eq!(
            injection_transport(
                depth_lab::RendererBackend::D3d11,
                "fallout4",
                &format!("sha256:{}", "f".repeat(64)),
            ),
            Ok(InjectionTransport::LateD3d11)
        );
        let cp2077 = EarlyLaunchTarget::cp2077();
        assert_eq!(
            injection_transport(
                depth_lab::RendererBackend::D3d12,
                cp2077.game_id(),
                cp2077.executable_sha256(),
            ),
            Ok(InjectionTransport::EarlyD3d12(cp2077))
        );
        let error = injection_transport(
            depth_lab::RendererBackend::D3d12,
            "unsupported-d3d12-game",
            cp2077.executable_sha256(),
        )
        .unwrap_err();
        assert!(error.contains("D3d12"), "{error}");
        assert!(error.contains("recording was not started"), "{error}");
    }

    #[test]
    fn cp2077_formal_lane_still_pins_the_game_and_executable() {
        let target = EarlyLaunchTarget::cp2077();
        for (game, hash) in [
            (
                "unsupported-d3d12-game",
                target.executable_sha256().to_owned(),
            ),
            (target.game_id(), format!("sha256:{}", "f".repeat(64))),
        ] {
            assert!(injection_transport(depth_lab::RendererBackend::D3d12, game, &hash).is_err());
        }
    }

    #[cfg(feature = "dev-catalog")]
    #[test]
    fn cp2077_real_catalog_pair_configures_formal_and_validation_lanes() {
        let _serial = serial_test_guard();
        reset();
        reset_runtime_for_test();
        assert_eq!(
            runtime_status().cp2077_local_validation_available,
            cfg!(all(feature = "cp2077-diagnostics", debug_assertions))
        );
        let store = crate::platform::windows::catalog::CatalogStore::load().unwrap();
        let game_id = EarlyLaunchTarget::cp2077().game_id();
        let selection = store.selection(game_id).unwrap();
        let (profile_bytes, binding_bytes) = resolve_catalog_depth(
            &selection,
            game_id,
            EarlyLaunchTarget::cp2077().executable_sha256(),
        )
        .unwrap();
        let profile: depth_lab::DepthProfile = serde_json::from_slice(&profile_bytes).unwrap();
        let binding: depth_lab::CameraBindingDocument =
            serde_json::from_slice(&binding_bytes).unwrap();
        binding
            .validate_for_profile(&profile, &profile.executable_sha256)
            .unwrap();
        assert_eq!(binding.schema(), depth_lab::CAMERA_BINDING_SCHEMA_V3);
        assert_eq!(profile.lane.video_fps(), 30);
        assert_eq!(profile.metric_scale.value, 1.0);
        let evidence = std::fs::read(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../../config/cp2077-local-depth/local-depth-evidence.json"),
        )
        .unwrap();
        assert_eq!(
            profile.metric_scale.evidence_sha256,
            Some(sha256_bytes(&evidence))
        );
        for performance_validation_only in [false, true] {
            let settings = DepthProbeSettings {
                enabled: true,
                risk_acknowledged: true,
                unbind_from_end: 4,
                planned_seconds: 32,
                start_timeout_ms: MIN_START_TIMEOUT_MS,
                stop_timeout_ms: 10_000,
                performance_validation_only,
                performance_metrics_enabled: false,
            };
            assert!(
                configure(settings, Some(&selection)).unwrap().configured,
                "performance_validation_only={performance_validation_only}"
            );
            {
                let guard = runtime().lock().unwrap();
                assert_eq!(
                    guard.configured_early_launch_target,
                    Some(EarlyLaunchTarget::cp2077())
                );
                assert_eq!(
                    guard.settings.as_ref().unwrap().performance_validation_only,
                    performance_validation_only
                );
                assert_eq!(
                    guard.configured_depth_profile_sha256,
                    Some(sha256_bytes(&profile_bytes))
                );
                assert!(guard.early_launch.is_none()); // Configuration does not launch or inject.
            }
            reset();
            reset_runtime_for_test();
        }
    }

    #[test]
    fn local_cp2077_audit_does_not_enter_certification_or_other_game_runs() {
        let game = EarlyLaunchTarget::cp2077().game_id();
        assert!(requires_local_cp2077_audit(game, true, None));
        assert!(!requires_local_cp2077_audit(game, false, None));
        assert!(!requires_local_cp2077_audit(
            "fallout4-steam-release-test",
            true,
            None
        ));
        for stage in [
            Stage::Baseline,
            Stage::WatermarkOnly,
            Stage::ReadbackOnly,
            Stage::Full,
        ] {
            assert!(!requires_local_cp2077_audit(game, true, Some(stage)));
        }
    }

    #[test]
    fn d3d12_record_rechecks_target_pid_executable_profile_and_probe() {
        let target = EarlyLaunchTarget::cp2077();
        let profile = format!("sha256:{}", "a".repeat(64));
        let probe = format!("sha256:{}", "b".repeat(64));
        let error = verify_early_launch_record(
            None,
            target.game_id(),
            7,
            target.executable_sha256(),
            &profile,
            &probe,
            target,
        )
        .unwrap_err();
        assert!(error.contains("late injection is forbidden"), "{error}");
        let record = EarlyLaunchRecord {
            target,
            process_id: 7,
            executable_sha256: target.executable_sha256().into(),
            depth_profile_sha256: profile.clone(),
            resident_probe_sha256: probe.clone(),
            launch_id: "a".repeat(32),
        };
        let verify = |pid, profile_hash: &str| {
            verify_early_launch_record(
                Some(&record),
                target.game_id(),
                pid,
                target.executable_sha256(),
                profile_hash,
                &probe,
                target,
            )
        };
        assert!(verify(7, &profile).is_ok());
        assert!(verify(8, &profile).is_err());
        assert!(verify(7, &format!("sha256:{}", "c".repeat(64))).is_err());
    }

    #[test]
    fn discovery_report_cannot_claim_recording_ready_or_cross_launches() {
        let launch_id = "a".repeat(32);
        let report = serde_json::to_vec(&serde_json::json!({
            "schema": D3D12_DISCOVERY_REPORT_SCHEMA,
            "process_launch_id": launch_id,
            "recording_ready": false,
            "snapshot": { "create_dsv_calls": 1 },
        }))
        .unwrap();
        assert!(
            validate_discovery_report(&report, D3D12_DISCOVERY_REPORT_SCHEMA, &launch_id).is_ok()
        );
        let unsafe_report = serde_json::to_vec(&serde_json::json!({
            "schema": D3D12_DISCOVERY_REPORT_SCHEMA,
            "process_launch_id": launch_id,
            "recording_ready": true,
        }))
        .unwrap();
        assert!(
            validate_discovery_report(&unsafe_report, D3D12_DISCOVERY_REPORT_SCHEMA, &launch_id)
                .is_err()
        );
        assert!(
            validate_discovery_report(&report, D3D12_DISCOVERY_REPORT_SCHEMA, &"b".repeat(32))
                .is_err()
        );
    }

    #[cfg(feature = "depth-lab")]
    #[test]
    fn discovery_start_rejects_every_non_cp2077_catalog_lane_before_preflight() {
        let error = start_cp2077_discovery(DiscoverySmokeStartRequest {
            risk_acknowledged: true,
            game_id: "fallout4-steam-release-test".into(),
        })
        .unwrap_err();
        assert!(error.contains("restricted to the CP2077 D3D12 catalog lane"));
    }

    #[cfg(feature = "depth-lab")]
    #[test]
    fn discovery_binding_requires_exact_game_pid_hash_and_never_authorizes_recording() {
        let temp = tempfile::tempdir().unwrap();
        let target = EarlyLaunchTarget::cp2077();
        let record = DiscoverySmokeRecord {
            process_id: 735,
            executable_sha256: target.executable_sha256().into(),
            resident_probe_sha256: format!("sha256:{}", "e".repeat(64)),
            launch_id: "f".repeat(32),
            output_dir: temp.path().to_path_buf(),
            output_locator: "DataMiner/depth-discovery/cp2077/test".into(),
        };
        assert!(discovery_binding_matches(
            &record,
            "cyberpunk-2077-steam",
            735,
            Some(target.executable_sha256())
        ));
        assert!(!discovery_binding_matches(
            &record,
            "cyberpunk-2077-steam",
            736,
            Some(target.executable_sha256())
        ));
        assert!(!discovery_binding_matches(
            &record,
            "fallout4-steam-release-test",
            735,
            Some(target.executable_sha256())
        ));
        assert!(discovery_process_blocks_recording(Some(&record), 735));
        assert!(!discovery_process_blocks_recording(Some(&record), 736));
    }

    fn faulted_discovery_record(output_dir: &Path) -> DiscoverySmokeRecord {
        let target = EarlyLaunchTarget::cp2077();
        DiscoverySmokeRecord {
            process_id: 735,
            executable_sha256: target.executable_sha256().into(),
            resident_probe_sha256: format!("sha256:{}", "e".repeat(64)),
            launch_id: "f".repeat(32),
            output_dir: output_dir.to_path_buf(),
            output_locator: "DataMiner/depth-discovery/cp2077/old-session".into(),
        }
    }

    #[test]
    fn exited_faulted_discovery_can_reset_without_deleting_old_evidence() {
        let temp = tempfile::tempdir().unwrap();
        let evidence_dir = temp.path().join("old-session");
        std::fs::create_dir_all(&evidence_dir).unwrap();
        let evidence = evidence_dir.join("partial-evidence.json");
        std::fs::write(&evidence, br#"{"process_launch_id":"old-launch"}"#).unwrap();
        let mut runtime = Runtime {
            discovery_state: DiscoverySmokeState::Faulted,
            discovery: Some(faulted_discovery_record(&evidence_dir)),
            discovery_last_error: Some("seal failed".into()),
            ..Runtime::default()
        };

        recover_faulted_discovery(&mut runtime, |pid| {
            assert_eq!(pid, 735);
            Ok(true)
        })
        .unwrap();

        assert_eq!(runtime.discovery_state, DiscoverySmokeState::Idle);
        assert!(runtime.discovery.is_none());
        assert!(runtime.discovery_last_error.is_none());
        assert!(
            evidence.is_file(),
            "old discovery evidence must be retained"
        );
        assert_eq!(
            std::fs::read_to_string(evidence).unwrap(),
            r#"{"process_launch_id":"old-launch"}"#
        );
    }

    #[test]
    fn alive_faulted_discovery_refuses_reset_and_keeps_provenance() {
        let temp = tempfile::tempdir().unwrap();
        let old_dir = temp.path().join("old-session");
        std::fs::create_dir_all(&old_dir).unwrap();
        let mut runtime = Runtime {
            discovery_state: DiscoverySmokeState::Faulted,
            discovery: Some(faulted_discovery_record(&old_dir)),
            discovery_last_error: Some("seal failed".into()),
            ..Runtime::default()
        };

        let error = recover_faulted_discovery(&mut runtime, |_| Ok(false)).unwrap_err();

        assert!(error.contains("still alive"), "{error}");
        assert_eq!(runtime.discovery_state, DiscoverySmokeState::Faulted);
        let retained = runtime.discovery.as_ref().expect("provenance retained");
        assert_eq!(retained.process_id, 735);
        assert_eq!(retained.launch_id, "f".repeat(32));
        assert_eq!(retained.output_dir, old_dir);
        assert_eq!(runtime.discovery_last_error.as_deref(), Some("seal failed"));
    }

    #[test]
    fn local_discovery_config_persists_no_account_path_and_is_never_ready() {
        let temp = tempfile::tempdir().unwrap();
        let session_id = "c".repeat(32);
        let prepared = prepare_discovery_session_at(temp.path(), &session_id).unwrap();
        let config = std::fs::read_to_string(prepared.config_path).unwrap();
        assert!(config.contains(DISCOVERY_CONFIG_SCHEMA));
        assert!(config.contains(r#""output_dir": ".""#));
        assert!(config.contains(r#""recording_ready": false"#));
        assert!(!config.contains(&temp.path().to_string_lossy().to_string()));
        assert_eq!(
            prepared.output_locator,
            format!("DataMiner/depth-discovery/cp2077/{session_id}")
        );
    }

    #[test]
    fn missing_launcher_and_probe_fail_closed() {
        let root = std::env::temp_dir().join(format!(
            "dm-depth-missing-early-tools-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        assert!(
            validate_early_launch_tools(
                &root.join("dm-depth-launch.exe"),
                &root.join("dm_depth_probe.dll")
            )
            .unwrap_err()
            .contains("missing")
        );
    }

    fn escape_vdf_path(path: &Path) -> String {
        path.to_string_lossy().replace('\\', r"\\")
    }

    fn write_cp2077_test_install(library: &Path, install_dir: &str, executable: &[u8]) -> PathBuf {
        let target = EarlyLaunchTarget::cp2077();
        std::fs::create_dir_all(library.join("steamapps")).unwrap();
        std::fs::write(
            library
                .join("steamapps")
                .join(format!("appmanifest_{}.acf", target.steam_app_id())),
            format!(
                "\"AppState\" {{ \"appid\" \"{}\" \"installdir\" \"{install_dir}\" \"StateFlags\" \"4\" \"buildid\" \"{}\" \"SizeOnDisk\" \"1\" }}",
                target.steam_app_id(),
                target.steam_build_id(),
            ),
        )
        .unwrap();
        let exe = library
            .join("steamapps")
            .join("common")
            .join(install_dir)
            .join(target.relative_executable());
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(&exe, executable).unwrap();
        exe
    }

    #[test]
    fn default_steam_library_is_resolved_from_its_manifest_and_exact_hash() {
        let temp = tempfile::tempdir().unwrap();
        let steam = temp.path().join("Steam");
        let exe = write_cp2077_test_install(&steam, "Cyberpunk 2077", b"default-library");
        let expected = sha256_file(&exe).unwrap();
        let resolved = resolve_steam_early_launch_target_from_roots(
            &[steam],
            EarlyLaunchTarget::cp2077(),
            &expected,
        )
        .unwrap();
        assert_eq!(resolved, std::fs::canonicalize(exe).unwrap());
    }

    #[test]
    fn nondefault_libraryfolders_entry_is_resolved_without_ui_path_input() {
        let temp = tempfile::tempdir().unwrap();
        let steam = temp.path().join("Steam");
        let library = temp.path().join("GamesLibrary");
        std::fs::create_dir_all(steam.join("steamapps")).unwrap();
        std::fs::create_dir_all(&library).unwrap();
        std::fs::write(
            steam.join("steamapps").join("libraryfolders.vdf"),
            format!(
                "\"libraryfolders\" {{ \"0\" {{ \"path\" \"{}\" }} \"1\" {{ \"path\" \"{}\" }} }}",
                escape_vdf_path(&steam),
                escape_vdf_path(&library)
            ),
        )
        .unwrap();
        let exe = write_cp2077_test_install(&library, "Cyberpunk 2077", b"other-library");
        let expected = sha256_file(&exe).unwrap();
        let resolved = resolve_steam_early_launch_target_from_roots(
            &[steam],
            EarlyLaunchTarget::cp2077(),
            &expected,
        )
        .unwrap();
        assert_eq!(resolved, std::fs::canonicalize(exe).unwrap());
    }

    #[test]
    fn downloading_or_wrong_build_manifest_cannot_reuse_a_stale_exact_executable() {
        let temp = tempfile::tempdir().unwrap();
        let steam = temp.path().join("Steam");
        let target = EarlyLaunchTarget::cp2077();
        let exe = write_cp2077_test_install(&steam, "Cyberpunk 2077", b"stale-exact-exe");
        let expected = sha256_file(&exe).unwrap();
        std::fs::write(
            steam
                .join("steamapps")
                .join(format!("appmanifest_{}.acf", target.steam_app_id())),
            format!(
                "\"AppState\" {{ \"appid\" \"{}\" \"installdir\" \"Cyberpunk 2077\" \"StateFlags\" \"1026\" \"buildid\" \"0\" \"SizeOnDisk\" \"0\" }}",
                target.steam_app_id(),
            ),
        )
        .unwrap();

        let error =
            resolve_steam_early_launch_target_from_roots(&[steam], target, &expected).unwrap_err();
        assert!(
            error.contains("incomplete or not the reviewed build"),
            "{error}"
        );
    }

    #[test]
    fn duplicate_app_manifests_are_ambiguous_even_when_one_executable_matches() {
        let temp = tempfile::tempdir().unwrap();
        let steam = temp.path().join("Steam");
        let library = temp.path().join("GamesLibrary");
        std::fs::create_dir_all(steam.join("steamapps")).unwrap();
        std::fs::create_dir_all(&library).unwrap();
        std::fs::write(
            steam.join("steamapps").join("libraryfolders.vdf"),
            format!(
                "\"libraryfolders\" {{ \"0\" {{ \"path\" \"{}\" }} \"1\" {{ \"path\" \"{}\" }} }}",
                escape_vdf_path(&steam),
                escape_vdf_path(&library)
            ),
        )
        .unwrap();
        let matching = write_cp2077_test_install(&steam, "Cyberpunk 2077", b"matching");
        write_cp2077_test_install(&library, "Cyberpunk 2077", b"other");
        let expected = sha256_file(&matching).unwrap();

        let error = resolve_steam_early_launch_target_from_roots(
            &[steam],
            EarlyLaunchTarget::cp2077(),
            &expected,
        )
        .unwrap_err();
        assert!(error.contains("2 app manifests"), "{error}");
    }

    #[test]
    fn canonicalized_staging_target_is_rejected_even_if_a_common_link_named_it() {
        let library = Path::new(r"C:\SteamLibrary");
        let common = library.join("steamapps").join("common");
        let staging_exe = library
            .join("steamapps")
            .join("downloading")
            .join("1091500")
            .join("Cyberpunk2077.exe");

        let error = ensure_candidate_within_common(
            EarlyLaunchTarget::cp2077(),
            library,
            &common,
            &staging_exe,
        )
        .unwrap_err();
        assert!(error.contains("trusted Steam common directory"), "{error}");
    }

    #[test]
    fn reparse_attributes_are_rejected_for_manifest_and_common_chains() {
        assert!(ensure_not_reparse_attributes(0, "fixture").is_ok());
        let error =
            ensure_not_reparse_attributes(FILE_ATTRIBUTE_REPARSE_POINT, "fixture").unwrap_err();
        assert!(error.contains("filesystem reparse point"), "{error}");
    }

    #[test]
    fn manifest_path_or_executable_drift_is_rejected() {
        let target = EarlyLaunchTarget::cp2077();
        let executable = Path::new(r"C:\Steam\steamapps\common\Cyberpunk 2077\Cyberpunk2077.exe");
        assert!(
            ensure_steam_resolution_stable(
                target,
                "manifest-a",
                "manifest-a",
                executable,
                executable,
                "sha256:a",
                "sha256:a",
            )
            .is_ok()
        );
        assert!(
            ensure_steam_resolution_stable(
                target,
                "manifest-a",
                "manifest-b",
                executable,
                executable,
                "sha256:a",
                "sha256:a",
            )
            .is_err()
        );
        assert!(
            ensure_steam_resolution_stable(
                target,
                "manifest-a",
                "manifest-a",
                executable,
                Path::new(r"C:\Steam\steamapps\common\Other\Cyberpunk2077.exe"),
                "sha256:a",
                "sha256:b",
            )
            .is_err()
        );
    }

    #[test]
    fn steam_manifest_cannot_escape_the_trusted_library() {
        let temp = tempfile::tempdir().unwrap();
        let steam = temp.path().join("Steam");
        let target = EarlyLaunchTarget::cp2077();
        std::fs::create_dir_all(steam.join("steamapps")).unwrap();
        std::fs::write(
            steam
                .join("steamapps")
                .join(format!("appmanifest_{}.acf", target.steam_app_id())),
            format!(
                "\"AppState\" {{ \"appid\" \"{}\" \"installdir\" \"..\\\\escape\" \"StateFlags\" \"4\" \"buildid\" \"{}\" \"SizeOnDisk\" \"1\" }}",
                target.steam_app_id(),
                target.steam_build_id(),
            ),
        )
        .unwrap();
        let error = resolve_steam_early_launch_target_from_roots(
            &[steam],
            target,
            &format!("sha256:{}", "a".repeat(64)),
        )
        .unwrap_err();
        assert!(error.contains("unsafe installdir"), "{error}");
    }

    #[test]
    fn dry_run_preflight_hashes_the_same_packaged_tools_used_by_launch() {
        let temp = tempfile::tempdir().unwrap();
        let launcher = temp.path().join("dm-depth-launch.exe");
        let probe = temp.path().join("dm_depth_probe.dll");
        std::fs::write(&launcher, b"launcher-release").unwrap();
        std::fs::write(&probe, b"probe-release").unwrap();
        let report =
            build_early_launch_preflight(Ok(temp.path().join("game.exe")), &launcher, &probe);
        assert!(report.ready);
        assert_eq!(
            report.launcher_sha256,
            Some(sha256_file(&launcher).unwrap())
        );
        assert_eq!(report.probe_sha256, Some(sha256_file(&probe).unwrap()));
        assert!(report.blockers.is_empty());
    }

    #[test]
    fn all_games_package_contract_names_both_early_launch_artifacts() {
        let manifest = std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tauri.all-games-dev.conf.json"),
        )
        .unwrap();
        let build_script = std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../../tools/build-windows-installer.ps1"),
        )
        .unwrap();
        for artifact in ["dm-depth-launch.exe", "dm_depth_probe.dll"] {
            assert!(
                manifest.contains(artifact),
                "missing {artifact} from bundle resources"
            );
            assert!(
                build_script.contains(artifact),
                "missing {artifact} from build validation"
            );
        }
    }

    #[test]
    fn a_blocked_gate_surfaces_an_annotator_readable_reason() {
        let _serial = serial_test_guard();
        reset();
        set_enabled(true, true);
        let s = status(&facts(false));
        assert!(!s.can_capture);
        let reason = s.blocked_reason.unwrap();
        assert!(reason.contains("许可名单"), "实际文案: {reason}");
        reset();
    }

    #[test]
    fn reset_clears_everything() {
        let _serial = serial_test_guard();
        set_enabled(true, true);
        reset();
        let s = status(&facts(true));
        assert!(!s.enabled && !s.risk_acknowledged);
    }

    #[test]
    fn finalize_proof_can_only_be_built_from_a_valid_flush_summary() {
        let identity = depth_lab::StreamIdentity {
            session_id: "s1".into(),
            run_id: "r1".into(),
            generation: 1,
        };
        let mut summary = depth_lab::ProbeFlushSummary {
            writer_queue_depth: 1,
            last_present_seq: 12,
            cutoff_present_seq: 12,
            depth_row_count: 4,
            depth_file_count: 4,
            camera_pose_row_count: 3,
            camera_diagnostic_row_count: 9,
            player_pose_row_count: None,
            selected_present: None,
            writer_bypass: false,
            writer_bypassed_frames: 0,
            writer_timing: None,
            performance_metrics_enabled: false,
            present_timing: None,
            errors: Vec::new(),
        };
        assert!(ConfirmedProbeFlush::new(&identity, summary.clone(), false).is_err());
        summary.writer_queue_depth = 0;
        summary.errors.push("writer failed".into());
        assert!(ConfirmedProbeFlush::new(&identity, summary.clone(), false).is_err());
        summary.errors.clear();
        assert!(ConfirmedProbeFlush::new(&identity, summary, false).is_ok());
    }

    #[test]
    fn performance_validation_flush_is_permanently_non_deliverable() {
        let identity = depth_lab::StreamIdentity {
            session_id: "s1".into(),
            run_id: "validation".into(),
            generation: 2,
        };
        let summary = depth_lab::ProbeFlushSummary {
            writer_queue_depth: 0,
            last_present_seq: 12,
            cutoff_present_seq: 12,
            depth_row_count: 4,
            depth_file_count: 4,
            camera_pose_row_count: 3,
            camera_diagnostic_row_count: 9,
            player_pose_row_count: None,
            selected_present: None,
            writer_bypass: false,
            writer_bypassed_frames: 0,
            writer_timing: None,
            performance_metrics_enabled: false,
            present_timing: None,
            errors: Vec::new(),
        };
        let confirmed = ConfirmedProbeFlush::new(&identity, summary, true).unwrap();
        assert!(confirmed.ensure_deliverable().is_err());
    }

    #[test]
    fn missing_ack_times_out_without_becoming_ready() {
        let _serial = serial_test_guard();
        reset_runtime_for_test();
        let root = std::env::temp_dir().join(format!("dm-depth-timeout-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let identity = begin_synthetic_start(&root);
        let error = wait_for_state(
            &root,
            ClientLifecycleState::Ready,
            Duration::from_millis(1),
            None,
        )
        .unwrap_err();
        assert!(error.contains("超时"), "{error}");
        assert_eq!(
            runtime().lock().unwrap().lifecycle.state(),
            ClientLifecycleState::Starting
        );
        mark_local_fault(&identity, error);
        reset_runtime_for_test();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn ready_adapter_mismatch_is_consumed_before_stop_cleanup() {
        let _serial = serial_test_guard();
        reset_runtime_for_test();
        let root = std::env::temp_dir().join(format!("dm-depth-version-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let identity = begin_synthetic_start(&root);
        let expected_profile = DepthProfileIdentity {
            schema_version: "dm-depth-profile-1".into(),
            game_id: "synthetic".into(),
            lane: "lane30".into(),
            executable_sha256: "sha256:abc".into(),
        };
        let expected_binding = CameraBindingIdentity {
            schema: "camera-binding/v1".into(),
            binding_id: "b1".into(),
            binding_version: "1".into(),
            profile_id: "synthetic".into(),
            profile_version: "dm-depth-profile-1".into(),
            executable_sha256: "sha256:abc".into(),
        };
        let expected_adapter = GpuAdapterIdentity {
            description: "NVIDIA GeForce RTX 5070 Ti Laptop GPU".into(),
            driver_version: "32.0.15.9227".into(),
            luid_high: 7,
            luid_low: 42,
        };
        let mut wrong_adapter = expected_adapter.clone();
        wrong_adapter.luid_low += 1;
        for ack in [
            depth_lab::ProbeAck {
                schema: depth_lab::probe_control::ACK_SCHEMA_V3.into(),
                command_id: "start-timeout".into(),
                identity: identity.clone(),
                ordinal: 1,
                ack: AckKind::Started,
            },
            depth_lab::ProbeAck {
                schema: depth_lab::probe_control::ACK_SCHEMA_V3.into(),
                command_id: "start-timeout".into(),
                identity: identity.clone(),
                ordinal: 2,
                ack: AckKind::Ready {
                    actual_profile: expected_profile.clone(),
                    actual_binding: expected_binding.clone(),
                    depth_profile_sha256: format!("sha256:{}", "a".repeat(64)),
                    camera_binding_sha256: format!("sha256:{}", "b".repeat(64)),
                    probe_version: PROBE_VERSION.into(),
                    actual_swapchain_adapter: wrong_adapter,
                    first_present_seq: 10,
                    resource_epoch: 1,
                    performance_metrics_enabled: false,
                },
            },
        ] {
            depth_lab::probe_control::write_ack(&root, &ack).unwrap();
        }
        let error = wait_for_state(
            &root,
            ClientLifecycleState::Ready,
            Duration::from_millis(100),
            Some((
                &expected_profile,
                &expected_binding,
                &format!("sha256:{}", "a".repeat(64)),
                &format!("sha256:{}", "b".repeat(64)),
                PROBE_VERSION,
                &expected_adapter,
                false,
            )),
        )
        .unwrap_err();
        assert!(error.contains("swapchain adapter"), "{error}");
        assert_eq!(
            runtime().lock().unwrap().lifecycle.state(),
            ClientLifecycleState::Started
        );
        assert_eq!(runtime().lock().unwrap().lifecycle.last_ordinal(), 2);
        let stop = ControlEnvelope {
            schema: CONTROL_SCHEMA_V3.into(),
            command_id: "cleanup-after-rejected-ready".into(),
            identity: identity.clone(),
            command: ControlCommand::StopRun(StopRun {
                cutoff_qpc: 99,
                reason: "ready_identity_mismatch".into(),
            }),
        };
        runtime()
            .lock()
            .unwrap()
            .lifecycle
            .begin_stop(&stop)
            .unwrap();
        depth_lab::probe_control::write_ack(
            &root,
            &depth_lab::ProbeAck {
                schema: depth_lab::probe_control::ACK_SCHEMA_V3.into(),
                command_id: stop.command_id.clone(),
                identity: identity.clone(),
                ordinal: 3,
                ack: AckKind::Faulted {
                    phase: "lifecycle".into(),
                    code: "stop_before_generation_sealed".into(),
                    message: "startup cleanup complete".into(),
                    summary: None,
                },
            },
        )
        .unwrap();
        let cleanup_error = wait_for_state(
            &root,
            ClientLifecycleState::Flushed,
            Duration::from_millis(100),
            None,
        )
        .unwrap_err();
        assert!(cleanup_error.contains("probe fault"), "{cleanup_error}");
        assert_eq!(
            runtime().lock().unwrap().lifecycle.state(),
            ClientLifecycleState::Faulted
        );
        reset_runtime_for_test();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn ready_provenance_requires_both_content_hashes() {
        let profile = DepthProfileIdentity {
            schema_version: "dm-depth-profile-1".into(),
            game_id: "synthetic".into(),
            lane: "lane30".into(),
            executable_sha256: "sha256:abc".into(),
        };
        let binding = CameraBindingIdentity {
            schema: "camera-binding/v1".into(),
            binding_id: "b1".into(),
            binding_version: "1".into(),
            profile_id: "synthetic".into(),
            profile_version: "dm-depth-profile-1".into(),
            executable_sha256: "sha256:abc".into(),
        };
        let profile_hash = format!("sha256:{}", "a".repeat(64));
        let binding_hash = format!("sha256:{}", "b".repeat(64));
        let adapter = GpuAdapterIdentity {
            description: "NVIDIA GeForce RTX 5070 Ti Laptop GPU".into(),
            driver_version: "32.0.15.9227".into(),
            luid_high: 7,
            luid_low: 42,
        };
        assert!(ready_matches_expected(
            &profile,
            &binding,
            &profile_hash,
            &binding_hash,
            PROBE_VERSION,
            &profile,
            &binding,
            &profile_hash,
            &binding_hash,
            PROBE_VERSION,
            &adapter,
            &adapter,
            true,
            true,
        ));
        assert!(!ready_matches_expected(
            &profile,
            &binding,
            "sha256:stale-profile",
            &binding_hash,
            PROBE_VERSION,
            &profile,
            &binding,
            &profile_hash,
            &binding_hash,
            PROBE_VERSION,
            &adapter,
            &adapter,
            true,
            true,
        ));
        assert!(!ready_matches_expected(
            &profile,
            &binding,
            &profile_hash,
            "sha256:stale-binding",
            PROBE_VERSION,
            &profile,
            &binding,
            &profile_hash,
            &binding_hash,
            PROBE_VERSION,
            &adapter,
            &adapter,
            true,
            true,
        ));
        let mut wrong_description = adapter.clone();
        wrong_description.description.push(' ');
        assert!(!ready_matches_expected(
            &profile,
            &binding,
            &profile_hash,
            &binding_hash,
            PROBE_VERSION,
            &profile,
            &binding,
            &profile_hash,
            &binding_hash,
            PROBE_VERSION,
            &wrong_description,
            &adapter,
            true,
            true,
        ));
        let mut wrong_driver = adapter.clone();
        wrong_driver.driver_version = "32.0.15.9999".into();
        assert!(!ready_matches_expected(
            &profile,
            &binding,
            &profile_hash,
            &binding_hash,
            PROBE_VERSION,
            &profile,
            &binding,
            &profile_hash,
            &binding_hash,
            PROBE_VERSION,
            &wrong_driver,
            &adapter,
            true,
            true,
        ));
        let mut wrong_luid = adapter.clone();
        wrong_luid.luid_low += 1;
        assert!(!ready_matches_expected(
            &profile,
            &binding,
            &profile_hash,
            &binding_hash,
            PROBE_VERSION,
            &profile,
            &binding,
            &profile_hash,
            &binding_hash,
            PROBE_VERSION,
            &wrong_luid,
            &adapter,
            true,
            true,
        ));
    }
}

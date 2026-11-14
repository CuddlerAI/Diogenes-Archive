//! Explicit local-only CP2077 raw D32 demo controller.
//!
//! This is intentionally separate from `probe_control::StartRun`. It consumes an operator-authored
//! profile derived from sealed discovery evidence, writes only bounded raw diagnostic files, and
//! can never emit Ready/Flushed or a formal depth segment.

use std::collections::VecDeque;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

use crate::d3d12_backend::{
    D3d12DepthCandidateObservation, D3d12DepthSelector, D3d12ObservationBackend,
};
use crate::d3d12_capture::D3d12RawDepthFrame;
use crate::d3d12_render_camera::D3d12RenderCameraSnapshot;
use crate::raw_depth_writer_io::{WriterIoMetrics, write_durable_file};

mod authoring_windows;
mod camera_archive;
mod feedback;

pub const RAW_DEMO_CONFIG_ENV: &str = "DM_DEPTH_RAW_DEMO_CONFIG";
pub const RAW_DEMO_CONFIG_SCHEMA_V1: &str = "dm-cp2077-raw-depth-demo-config/v1";
pub const RAW_DEMO_TELEMETRY_SCHEMA_V1: &str = "dm-cp2077-raw-depth-demo-telemetry/v1";
pub const RAW_DEMO_INDEX_SCHEMA_V1: &str = "dm-cp2077-raw-depth-frame-index/v1";
const PROFILE_FILE: &str = "raw-depth-demo-profile.json";
const ARM_FILE: &str = "raw-depth-demo-arm-request.json";
pub(crate) const STOP_FILE: &str = "raw-depth-demo-stop-request.json";
const INDEX_FILE: &str = "raw-depth-index.jsonl";
const TELEMETRY_FILE: &str = "raw-depth-telemetry.json";
const RENDER_CAMERA_FILE: &str = "render-camera-observation.json";
const FRAME_DIR: &str = "raw-depth";
const MAX_PROFILE_BYTES: u64 = 128 * 1024;
const MAX_CONTROL_REQUEST_BYTES: u64 = 4 * 1024;
const MIN_MAX_FRAMES: u64 = 2;
const AUTOMATIC_MAX_FRAMES: u64 = 1_024;
const MANUAL_STOP_MAX_FRAMES: u64 = 4_096;
const MANUAL_STOP_SAFETY_MS: i64 = 240_000;
const MAX_PRESENT_STRIDE: u64 = 240;
const PR405_TARGET_DEPTH_HZ: u64 = 15;
const ISSUE735_TARGET_DEPTH_HZ: u64 = 16;
// Both local contracts arm the probe before WGC starts so RGB slot zero cannot
// precede the first depth interval. The depth duration clock then runs from the
// arm stamp while RGB's runs from the compositor's Started event, so the depth
// window closes that whole readiness lead *earlier* than the RGB window unless
// the lead is handed back. Retain a bounded tail after the nominal action
// window which covers the lead and lets the final rows reach the private
// fence/readback writer.
//
// On the real CP2077 path the PR405 lead is about ten 30 Hz Presents; 750 ms
// keeps telemetry inside the coordinator's strict <=31 s contract.
const PR405_READBACK_DRAIN_GRACE_MS: i64 = 750;
// issue735 had no grace at all, so its depth window ended before RGB's every
// time. The 2026-09-10 joint attempt measured a 942 ms lead - session directory,
// WGC capture item, device and encoder startup - and lost its last 0.9 s of RGB
// to it. 1,500 ms covers that with margin. This cannot recover the tail once the
// game actually reaches the 16 Hz target rate: `max_frames` is capped at the
// contract's own 1,024, which is exactly 64 s at 16 Hz, so the frame cap stops
// the run first. That ceiling is a property of the contract, not of this grace.
const ISSUE735_READINESS_LEAD_GRACE_MS: i64 = 1_500;

/// Nominal action window plus the contract's bounded readiness/drain tail.
fn effective_duration_ms(duration_ms: u64, capture_contract: &str) -> i64 {
    duration_ms as i64
        + match capture_contract {
            depth_lab::PR405_LOCAL_CONTRACT => PR405_READBACK_DRAIN_GRACE_MS,
            depth_lab::ISSUE735_LOCAL_CONTRACT => ISSUE735_READINESS_LEAD_GRACE_MS,
            _ => 0,
        }
}
const PRESENT_STAMP_CAPACITY: usize = 2_048;
const WRITER_QUEUE_CAPACITY: usize = 32;
const D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES: u32 = u32::MAX;
const D3D12_RESOURCE_STATE_DEPTH_WRITE: i32 = 0x10;
const CP2077_COMPOSED_DEPTH_READ: i32 = 0xe0;
const CP2077_DEPTH_READ: i32 = 0x40;
const DXGI_FORMAT_R32G8X24_TYPELESS: i32 = 19;
const DXGI_FORMAT_D32_FLOAT_S8X24_UINT: i32 = 20;
const DXGI_FORMAT_R32_TYPELESS: i32 = 39;
const DXGI_FORMAT_D32_FLOAT: i32 = 40;
const D32S8_MIN_BINDS_PER_CLEAR: u64 = 16;
// The >=32 command-list breadth is a discovery-authoring proof accumulated over a full launch.
// It is deliberately not part of cross-launch runtime rebinding: CP2077 may reuse a smaller list
// pool during a short already-loaded scene. Runtime identity remains exact on format/geometry,
// full-resource state transitions, clear semantics, bind/clear ratio, and the unique DIRECT queue.
const D32S8_MIN_RUNTIME_DISTINCT_COMMAND_LISTS: usize = 1;
const D32S8_MAX_PREARM_DOUBLE_CLEAR_INTERVALS: u64 = 1;
const D32S8_MAX_PREARM_UNPAIRED_CLEARS: u64 = 1;

fn default_capture_contract() -> String {
    depth_lab::ISSUE735_LOCAL_CONTRACT.into()
}

const fn default_duration_ms() -> u64 {
    depth_lab::ISSUE735_LOCAL_DURATION_MS
}

pub const RAW_DEMO_ARM_REQUEST_SCHEMA_V1: &str = "dm-cp2077-raw-depth-demo-arm-request/v1";
pub const RAW_DEMO_STOP_REQUEST_SCHEMA_V1: &str = "dm-cp2077-raw-depth-demo-stop-request/v1";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDemoConfig {
    schema: String,
    output_dir: String,
    profile_file: String,
    profile_sha256: String,
    #[serde(default = "default_capture_contract")]
    capture_contract: String,
    #[serde(default = "default_duration_ms")]
    duration_ms: u64,
    #[serde(default)]
    manual_stop: bool,
    #[serde(default)]
    camera_authoring_windows: bool,
    max_frames: u64,
    present_stride: u64,
    recording_ready: bool,
    non_trainable: bool,
    upload_allowed: bool,
    formal_start_run: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDemoArmRequest {
    schema: String,
    process_launch_id: String,
    #[serde(default = "default_capture_contract")]
    capture_contract: String,
    duration_ms: u64,
    target_depth_hz: u64,
    #[serde(default)]
    manual_stop: bool,
    #[serde(default)]
    selected_present: Option<depth_lab::cp2077_raw_feedback::RawSelectedRequest>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDemoStopRequest {
    schema: String,
    process_launch_id: String,
}

#[derive(Clone, Copy, Debug)]
struct CandidateFingerprint {
    width: u32,
    height: u32,
    sample_count: u32,
    resource_format: i32,
    dsv_format: i32,
    format_family: RawDepthFormatFamily,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RawDepthFormatFamily {
    R32D32,
    R32G8X24D32S8,
}

#[derive(Debug, Eq, PartialEq)]
enum SemanticBindingAction {
    Wait {
        prebound_candidate_id: Option<String>,
    },
    Arm {
        candidate_id: String,
    },
    VerifyArmed,
    Fault(String),
}

#[derive(Clone, Copy, Debug)]
struct PresentStamp {
    present_seq: u64,
    qpc: i64,
}

struct RawFrameForWriter {
    enqueued_at: Instant,
    ordinal: u64,
    current_present_seq: u64,
    source_present_qpc: i64,
    runtime_candidate_id: String,
    authoring_phase: Option<authoring_windows::FramePhase>,
    arm_request_observed_qpc: i64,
    arm_start_present_seq: u64,
    frame: RawPayload,
}

struct RawPayload {
    source: Option<crate::d3d12_abi::DsvIdentity>,
    capture_id: u64,
    present_seq: u64,
    src_width: usize,
    src_height: usize,
    row_pitch: usize,
    bytes: Vec<u8>,
}

// This bounded 32-slot queue carries frames inline. Keep the enqueue timestamp
// inline too, without adding a separate heap allocation on the render callback.
#[allow(clippy::large_enum_variant)]
enum WriterCommand {
    Frame(RawFrameForWriter),
    CameraArchive(Vec<crate::d3d12_render_camera::D3d12RenderDrawObservation>),
    CameraReport(Box<D3d12RenderCameraSnapshot>),
    AuthoringBarrier(u64),
    Stop,
}

struct Runtime {
    camera_authoring_windows: bool,
    authoring_interval: authoring_windows::IntervalGate,
    authoring_control: Mutex<authoring_windows::Control>,
    authoring_writer_barrier: AtomicU64,
    feedback: Mutex<Option<feedback::Feedback>>,
    feedback_enabled: AtomicBool,
    feedback_drained: AtomicBool,
    feedback_capture_start_qpc: AtomicI64,
    launch_id: String,
    source_discovery_launch_id: String,
    profile_sha256: String,
    source_report_sha256: String,
    source_candidate_id: String,
    capture_contract: String,
    duration_ms: u64,
    manual_stop: bool,
    fingerprint: CandidateFingerprint,
    readback_slots: usize,
    runtime_candidate_id: Mutex<Option<String>>,
    armed_double_clear_interval_baseline: AtomicU64,
    armed_unpaired_clear_baseline: AtomicU64,
    width: u32,
    height: u32,
    row_pitch_expected_min: usize,
    max_frames: u64,
    present_stride: u64,
    qpc_frequency: i64,
    output_dir: PathBuf,
    pose_worker: crate::cp2077_pose_wiring::RawDemoPoseWorkerHandle,
    sender: SyncSender<WriterCommand>,
    present_stamps: Mutex<VecDeque<PresentStamp>>,
    arm_authorized: AtomicBool,
    arm_request_observed_qpc: AtomicI64,
    arm_start_present_seq: AtomicU64,
    stop_observed_qpc: AtomicI64,
    last_cadence_request_qpc: AtomicI64,
    cadence_not_due_presents: AtomicU64,
    semantic_match_count: AtomicU64,
    semantic_candidates_observed: Mutex<Vec<SemanticCandidateTelemetry>>,
    requested_intervals: AtomicU64,
    submitted_frames: AtomicU64,
    writer_queue_dropped: AtomicU64,
    present_stamp_dropped: AtomicU64,
    capture_pool_dropped: AtomicU64,
    stopping: AtomicBool,
    fatal: Mutex<Option<String>>,
}

static RUNTIME: OnceLock<Arc<Runtime>> = OnceLock::new();

pub(crate) fn selected_feedback_enabled() -> bool {
    RUNTIME
        .get()
        .is_some_and(|r| r.feedback_enabled.load(Ordering::Acquire))
}

pub(crate) fn camera_archive_enabled() -> bool {
    selected_feedback_enabled() || authoring_windows_enabled()
}

pub(crate) fn authoring_windows_enabled() -> bool {
    RUNTIME.get().is_some_and(|r| r.camera_authoring_windows)
}

pub(crate) fn authoring_window_allows_sampling() -> bool {
    RUNTIME.get().is_none_or(|r| {
        !r.camera_authoring_windows
            || r.authoring_interval
                .allows_new(current_qpc(), r.stopping.load(Ordering::Acquire))
    })
}

/// The decision belongs to the whole render interval, including its tail after the deadline.
/// A wall-clock check in each draw would discard the last already-authorized depth's camera.
pub(crate) fn authoring_camera_interval_active() -> bool {
    RUNTIME
        .get()
        .is_none_or(|r| !r.camera_authoring_windows || r.authoring_interval.observing())
}

pub(crate) fn authoring_source_prepared(
    source: crate::d3d12_abi::DsvIdentity,
    present: u64,
) -> Result<(), String> {
    let Some(runtime) = RUNTIME.get().filter(|r| r.camera_authoring_windows) else {
        return Ok(());
    };
    authoring_windows::source_prepared(runtime, source, present)
}

pub(crate) fn selected_preflight(
    backend: &D3d12ObservationBackend,
    swapchain: *mut std::ffi::c_void,
) -> Result<(), String> {
    let r = RUNTIME.get().ok_or("raw demo runtime missing")?;
    let owner = r
        .feedback
        .lock()
        .map_err(|_| "raw feedback lock poisoned")?;
    if let Some(owner) = owner.as_ref() {
        owner.preflight(r, backend, swapchain)?;
    }
    Ok(())
}

pub(crate) fn selected_on_present(
    backend: &D3d12ObservationBackend,
    swapchain: *mut std::ffi::c_void,
    present: u64,
) -> Result<(), String> {
    let r = RUNTIME.get().ok_or("raw demo runtime missing")?;
    if r.stopping.load(Ordering::Acquire) {
        return Ok(());
    }
    let mut owner = r
        .feedback
        .lock()
        .map_err(|_| "raw feedback lock poisoned")?;
    if r.stopping.load(Ordering::Acquire) {
        return Ok(());
    }
    backend.archive_local_camera_before(present, |rows| {
        match r.sender.try_send(WriterCommand::CameraArchive(rows)) {
            Ok(()) => Ok(()),
            Err(
                std::sync::mpsc::TrySendError::Full(WriterCommand::CameraArchive(rows))
                | std::sync::mpsc::TrySendError::Disconnected(WriterCommand::CameraArchive(rows)),
            ) => Err(rows),
            Err(_) => unreachable!("try_send returns its original command"),
        }
    })?;
    if let Some(owner) = owner.as_mut() {
        owner.on_present(r, backend, swapchain, present)
    } else if r.camera_authoring_windows {
        Ok(())
    } else {
        Err("raw feedback owner missing".into())
    }
}

pub(crate) fn selected_after_present(present: u64, succeeded: bool) {
    if let Some(r) = RUNTIME.get()
        && r.feedback_enabled.load(Ordering::Acquire)
        && let Ok(mut owner) = r.feedback.lock()
        && let Some(owner) = owner.as_mut()
        && let Err(error) = owner.after_present(r, present, succeeded)
    {
        set_fatal(r, error);
    }
}

#[derive(Debug)]
pub struct RawDemoActivation {
    pub readback_slots: usize,
}

#[derive(Serialize)]
struct FrameIndex<'a> {
    schema: &'static str,
    ordinal: u64,
    file: &'a str,
    present_seq: u64,
    present_qpc: i64,
    current_present_seq: u64,
    readback_lag_presents: u64,
    capture_id: u64,
    source_candidate_id: &'a str,
    runtime_candidate_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    authoring_phase: Option<&'a authoring_windows::FramePhase>,
    arm_request_observed_qpc: i64,
    arm_start_present_seq: u64,
    width: usize,
    height: usize,
    row_pitch: usize,
    pixel_format: &'static str,
    byte_len: usize,
    rgb_pairing: &'static str,
}

#[derive(Serialize)]
struct Telemetry<'a> {
    schema: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    sampling_mode: Option<&'static str>,
    process_launch_id: &'a str,
    source_discovery_launch_id: &'a str,
    profile_sha256: &'a str,
    source_d3d12_report_sha256: &'a str,
    source_candidate_id: &'a str,
    capture_contract: &'a str,
    runtime_candidate_id: Option<&'a str>,
    semantic_match_count: u64,
    semantic_candidates_observed: Vec<SemanticCandidateTelemetry>,
    arm_request_observed_qpc: Option<i64>,
    arm_start_present_seq: Option<u64>,
    stop_observed_qpc: Option<i64>,
    elapsed_ms: Option<u64>,
    duration_ms: u64,
    manual_stop: bool,
    target_depth_hz: u64,
    cadence_not_due_presents: u64,
    max_frames: u64,
    present_stride: u64,
    qpc_frequency: i64,
    requested_intervals: u64,
    submitted_frames: u64,
    written_frames: u64,
    writer_queue_capacity: usize,
    writer_io: &'a WriterIoMetrics,
    writer_queue_dropped: u64,
    present_stamp_dropped: u64,
    capture_pool_dropped: u64,
    fatal_error: Option<&'a str>,
    non_trainable: bool,
    production_certified: bool,
    upload_allowed: bool,
    deliverable: bool,
    recording_ready: bool,
    formal_start_run_used: bool,
}

#[derive(Clone, Debug, Serialize)]
struct SemanticCandidateTelemetry {
    candidate_id: String,
    resource_epoch: u64,
    descriptor_generation: u64,
    observed_after_present_calls: u64,
    first_present_call_with_clear: Option<u64>,
    last_present_call_with_clear: Option<u64>,
    pending_clear_calls: u64,
    clear_calls: u64,
    eligible_clear_calls: u64,
    same_list_capture_eligible_clear_calls: u64,
    execute_calls: u64,
    present_intervals_with_clear: u64,
    present_intervals_with_one_clear: u64,
    present_intervals_with_multiple_clears: u64,
    max_clears_per_present_interval: u64,
    same_list_present_intervals: u64,
    bind_calls: u64,
    distinct_command_lists: usize,
    distinct_direct_queues: usize,
    full_semantic_match: bool,
}

#[derive(Serialize)]
struct RenderCameraReport<'a> {
    schema: &'static str,
    process_launch_id: &'a str,
    non_trainable: bool,
    production_certified: bool,
    deliverable: bool,
    recording_ready: bool,
    observation: D3d12RenderCameraSnapshot,
}

#[must_use]
pub fn requested() -> bool {
    std::env::var_os(RAW_DEMO_CONFIG_ENV).is_some()
}

#[must_use]
pub fn active() -> bool {
    RUNTIME.get().is_some()
}

/// Parse and activate a raw demo before the suspended game is resumed.
pub fn try_start_from_environment(
    executable_sha256: &str,
    backend: Arc<D3d12ObservationBackend>,
) -> Result<Option<RawDemoActivation>, String> {
    let Some(config_path) = std::env::var_os(RAW_DEMO_CONFIG_ENV) else {
        return Ok(None);
    };
    let launch_id = std::env::var("DM_DEPTH_LAUNCH_ID")
        .map_err(|_| "raw depth demo requires DM_DEPTH_LAUNCH_ID".to_string())?;
    if !valid_launch_id(&launch_id) {
        return Err("raw depth demo launch id is not 32 lowercase hex digits".into());
    }
    let expected_probe_sha256 = std::env::var("DM_DEPTH_EXPECTED_PROBE_SHA256")
        .map_err(|_| "raw depth demo requires the launcher's exact probe hash".to_string())?;
    require_sha256(&expected_probe_sha256, "expected probe SHA-256")?;

    let config_path = PathBuf::from(config_path);
    if !config_path.is_absolute() {
        return Err("raw depth demo config path must be absolute".into());
    }
    let config_path = config_path
        .canonicalize()
        .map_err(|error| format!("resolve raw depth demo config failed: {error}"))?;
    let config: RawDemoConfig = serde_json::from_slice(
        &std::fs::read(&config_path)
            .map_err(|error| format!("read raw depth demo config failed: {error}"))?,
    )
    .map_err(|error| format!("parse raw depth demo config failed: {error}"))?;
    validate_config(&config)?;
    let output_dir = config_path
        .parent()
        .ok_or("raw depth demo config has no parent")?
        .canonicalize()
        .map_err(|error| format!("resolve raw depth demo output directory failed: {error}"))?;
    require_private_output_root(&output_dir)?;
    for stale in [
        ARM_FILE,
        STOP_FILE,
        INDEX_FILE,
        TELEMETRY_FILE,
        RENDER_CAMERA_FILE,
        crate::cp2077_pose_wiring::DISCOVERY_REPORT_FILE,
        FRAME_DIR,
    ] {
        if output_dir.join(stale).exists() {
            return Err(format!("raw depth demo output contains stale {stale}"));
        }
    }
    let profile_path = output_dir.join(&config.profile_file);
    let profile_metadata = profile_path
        .metadata()
        .map_err(|error| format!("inspect raw depth demo profile failed: {error}"))?;
    if !profile_metadata.is_file() || profile_metadata.len() > MAX_PROFILE_BYTES {
        return Err("raw depth demo profile is not a bounded regular file".into());
    }
    let profile_bytes = std::fs::read(&profile_path)
        .map_err(|error| format!("read raw depth demo profile failed: {error}"))?;
    let actual_profile_sha256 = sha256_bytes(&profile_bytes);
    if actual_profile_sha256 != config.profile_sha256 {
        return Err(format!(
            "raw depth demo profile hash mismatch: expected={} actual={actual_profile_sha256}",
            config.profile_sha256
        ));
    }
    let profile: depth_lab::RawDepthDemoProfile = serde_json::from_slice(&profile_bytes)
        .map_err(|error| format!("parse raw depth demo profile failed: {error}"))?;
    profile.validate()?;
    if profile.capture_contract != config.capture_contract
        || profile.duration_ms != config.duration_ms
    {
        return Err("raw depth demo config does not match the profile capture contract".into());
    }
    if profile.executable_sha256 != executable_sha256 {
        return Err("raw depth demo profile does not match the resident executable".into());
    }
    if profile.resident_probe_sha256 != expected_probe_sha256 {
        return Err("raw depth demo profile does not match the launcher's exact probe".into());
    }
    if profile.process_launch_id == launch_id {
        return Err("raw depth demo must cite a prior sealed discovery launch".into());
    }

    let format_family = raw_depth_format_family(
        profile.selector.resource_format,
        profile.selector.dsv_format,
    )
    .ok_or("raw depth demo profile selected an unsupported format family")?;
    let fingerprint = CandidateFingerprint {
        width: profile.selector.width,
        height: profile.selector.height,
        sample_count: profile.selector.sample_count,
        resource_format: profile.selector.resource_format,
        dsv_format: profile.selector.dsv_format,
        format_family,
    };
    let frame_dir = output_dir.join(FRAME_DIR);
    std::fs::create_dir(&frame_dir)
        .map_err(|error| format!("create raw depth frame directory failed: {error}"))?;
    let index_path = output_dir.join(INDEX_FILE);
    let index = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&index_path)
        .map_err(|error| format!("create raw depth index failed: {error}"))?;
    let (sender, receiver) = sync_channel(WRITER_QUEUE_CAPACITY);
    let qpc_frequency = qpc_frequency();
    if qpc_frequency <= 0 {
        return Err("QueryPerformanceFrequency failed for raw depth demo".into());
    }
    let pose_worker = crate::cp2077_pose_wiring::start_for_raw_demo(
        executable_sha256,
        &launch_id,
        &config_path,
        &output_dir,
        &output_dir.join(STOP_FILE),
    )?;
    let runtime = Arc::new(Runtime {
        camera_authoring_windows: config.camera_authoring_windows,
        authoring_interval: authoring_windows::IntervalGate::default(),
        authoring_control: Mutex::new(authoring_windows::Control::default()),
        authoring_writer_barrier: AtomicU64::new(0),
        feedback: Mutex::new(None),
        feedback_enabled: AtomicBool::new(false),
        feedback_drained: AtomicBool::new(false),
        feedback_capture_start_qpc: AtomicI64::new(0),
        launch_id,
        source_discovery_launch_id: profile.process_launch_id,
        profile_sha256: actual_profile_sha256,
        source_report_sha256: profile.source_d3d12_report_sha256,
        source_candidate_id: profile.selector.candidate_id,
        capture_contract: config.capture_contract,
        duration_ms: config.duration_ms,
        manual_stop: config.manual_stop,
        fingerprint,
        readback_slots: profile.selector.readback_slots,
        runtime_candidate_id: Mutex::new(None),
        armed_double_clear_interval_baseline: AtomicU64::new(u64::MAX),
        armed_unpaired_clear_baseline: AtomicU64::new(u64::MAX),
        width: fingerprint.width,
        height: fingerprint.height,
        row_pitch_expected_min: usize::try_from(fingerprint.width)
            .map_err(|_| "raw demo width exceeds usize".to_string())?
            .checked_mul(4)
            .ok_or("raw demo row byte count overflow")?,
        max_frames: config.max_frames,
        present_stride: config.present_stride,
        qpc_frequency,
        output_dir,
        pose_worker,
        sender,
        present_stamps: Mutex::new(VecDeque::with_capacity(PRESENT_STAMP_CAPACITY)),
        arm_authorized: AtomicBool::new(false),
        arm_request_observed_qpc: AtomicI64::new(0),
        arm_start_present_seq: AtomicU64::new(u64::MAX),
        stop_observed_qpc: AtomicI64::new(0),
        last_cadence_request_qpc: AtomicI64::new(0),
        cadence_not_due_presents: AtomicU64::new(0),
        semantic_match_count: AtomicU64::new(0),
        semantic_candidates_observed: Mutex::new(Vec::new()),
        requested_intervals: AtomicU64::new(0),
        submitted_frames: AtomicU64::new(0),
        writer_queue_dropped: AtomicU64::new(0),
        present_stamp_dropped: AtomicU64::new(0),
        capture_pool_dropped: AtomicU64::new(0),
        stopping: AtomicBool::new(false),
        fatal: Mutex::new(None),
    });
    let writer_runtime = Arc::clone(&runtime);
    if let Err(error) = std::thread::Builder::new()
        .name("dm-cp2077-raw-depth-writer".into())
        .spawn(move || writer_loop(receiver, index, writer_runtime))
    {
        let message = format!("start raw depth writer failed: {error}");
        abort_start_after_pose_worker(&runtime, &message);
        return Err(message);
    }
    let stop_runtime = Arc::clone(&runtime);
    if let Err(error) = std::thread::Builder::new()
        .name("dm-cp2077-raw-depth-stop".into())
        .spawn(move || stop_watcher(stop_runtime, backend))
    {
        let message = format!("start raw depth stop watcher failed: {error}");
        abort_start_after_pose_worker(&runtime, &message);
        return Err(message);
    }
    if let Err(runtime) = RUNTIME.set(runtime) {
        let message = "raw depth demo runtime was already initialized".to_string();
        abort_start_after_pose_worker(&runtime, &message);
        return Err(message);
    }
    Ok(Some(RawDemoActivation {
        readback_slots: profile.selector.readback_slots,
    }))
}

fn abort_start_after_pose_worker(runtime: &Runtime, error: &str) {
    set_fatal(runtime, error);
    runtime.stopping.store(true, Ordering::Release);
    runtime
        .stop_observed_qpc
        .compare_exchange(0, current_qpc(), Ordering::AcqRel, Ordering::Acquire)
        .ok();
    if let Err(pose_error) = runtime
        .pose_worker
        .stop_and_seal(&runtime.launch_id, &runtime.output_dir)
    {
        crate::probe::log(&format!(
            "seal raw-demo pose report during startup abort failed: {pose_error}"
        ));
    }
    let _ = runtime.sender.send(WriterCommand::Stop);
}

fn validate_config(config: &RawDemoConfig) -> Result<(), String> {
    if config.camera_authoring_windows
        && (!cfg!(all(feature = "cp2077-diagnostics", debug_assertions))
            || !config.manual_stop
            || config.capture_contract != depth_lab::ISSUE735_LOCAL_CONTRACT
            || config.max_frames > AUTOMATIC_MAX_FRAMES)
    {
        return Err("camera authoring windows require diagnostic manual-stop issue735 with at most 1024 original frames".into());
    }
    if config.schema != RAW_DEMO_CONFIG_SCHEMA_V1
        || config.output_dir != "."
        || config.profile_file != PROFILE_FILE
        || config.recording_ready
        || !config.non_trainable
        || config.upload_allowed
        || config.formal_start_run
    {
        return Err("raw depth demo config weakens the local non-recording contract".into());
    }
    require_sha256(&config.profile_sha256, "profile SHA-256")?;
    depth_lab::validate_capture_contract(&config.capture_contract, config.duration_ms)?;
    let admitted_max_frames = if config.manual_stop {
        MANUAL_STOP_MAX_FRAMES
    } else {
        AUTOMATIC_MAX_FRAMES
    };
    if !(MIN_MAX_FRAMES..=admitted_max_frames).contains(&config.max_frames)
        || !(1..=MAX_PRESENT_STRIDE).contains(&config.present_stride)
    {
        return Err("raw depth demo bounds are outside the admitted range".into());
    }
    Ok(())
}

fn require_private_output_root(path: &Path) -> Result<(), String> {
    let local = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .ok_or("LOCALAPPDATA is unavailable for raw depth demo output")?
        .canonicalize()
        .map_err(|error| format!("resolve LOCALAPPDATA failed: {error}"))?;
    let trusted = local
        .join("DataMiner")
        .join("depth-raw-demo")
        .join("cp2077");
    if !path_is_within_case_insensitive(path, &trusted) || path == trusted {
        return Err("raw depth demo output is not a private CP2077 session directory".into());
    }
    Ok(())
}

/// Called once at the beginning of each D3D12 Present callback. The returned QPC is the
/// canonical raw-depth Present timestamp and may be reused by other same-Present observers.
pub fn note_present(present_seq: u64) -> Option<i64> {
    let runtime = RUNTIME.get()?;
    let qpc = current_qpc();
    if qpc <= 0 {
        set_fatal(
            runtime,
            "QueryPerformanceCounter failed for a raw demo Present",
        );
        return None;
    }
    observe_arm_request(runtime, present_seq, qpc);
    let Ok(mut stamps) = runtime.present_stamps.try_lock() else {
        runtime
            .present_stamp_dropped
            .fetch_add(1, Ordering::Relaxed);
        return Some(qpc);
    };
    if stamps.len() == PRESENT_STAMP_CAPACITY {
        stamps.pop_front();
    }
    stamps.push_back(PresentStamp { present_seq, qpc });
    Some(qpc)
}

fn observe_arm_request(runtime: &Runtime, present_seq: u64, qpc: i64) {
    if runtime.arm_authorized.load(Ordering::Acquire)
        || runtime.stopping.load(Ordering::Acquire)
        || runtime.fatal.lock().is_ok_and(|fatal| fatal.is_some())
    {
        return;
    }
    let path = runtime.output_dir.join(ARM_FILE);
    let Ok(metadata) = path.metadata() else {
        return;
    };
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_CONTROL_REQUEST_BYTES {
        set_fatal(
            runtime,
            "raw depth demo arm request is not a bounded regular file",
        );
        return;
    }
    let request = std::fs::read(&path)
        .map_err(|error| format!("read raw depth demo arm request failed: {error}"))
        .and_then(|bytes| {
            serde_json::from_slice::<RawDemoArmRequest>(&bytes)
                .map_err(|error| format!("parse raw depth demo arm request failed: {error}"))
        })
        .and_then(|request| {
            validate_arm_request(
                &request,
                &runtime.launch_id,
                &runtime.capture_contract,
                runtime.duration_ms,
                runtime.manual_stop,
            )?;
            if let Some(selected) = request.selected_present {
                if runtime.camera_authoring_windows {
                    return Err(
                        "camera authoring windows cannot run selected-Present recording".into(),
                    );
                }
                for name in [
                    depth_lab::cp2077_raw_feedback::READY_FILE,
                    depth_lab::cp2077_raw_feedback::SUMMARY_FILE,
                    depth_lab::cp2077_raw_feedback::RGB_STOP_FILE,
                ] {
                    if runtime.output_dir.join(name).exists() {
                        return Err("refusing stale raw selected-Present session".into());
                    }
                }
                let owner = feedback::Feedback::new(runtime, selected)?;
                *runtime
                    .feedback
                    .lock()
                    .map_err(|_| "raw feedback lock poisoned")? = Some(owner);
                runtime.feedback_enabled.store(true, Ordering::Release);
            }
            Ok(())
        });
    if let Err(error) = request {
        set_fatal(runtime, error);
        return;
    }
    runtime
        .arm_request_observed_qpc
        .store(qpc, Ordering::Release);
    runtime.last_cadence_request_qpc.store(0, Ordering::Release);
    runtime
        .arm_start_present_seq
        .store(present_seq, Ordering::Release);
    runtime.arm_authorized.store(true, Ordering::Release);
}

fn validate_arm_request(
    request: &RawDemoArmRequest,
    launch_id: &str,
    capture_contract: &str,
    duration_ms: u64,
    manual_stop: bool,
) -> Result<(), String> {
    let expected_schema = if request.selected_present.is_some() {
        "dm-cp2077-raw-depth-demo-arm-request/v2"
    } else {
        RAW_DEMO_ARM_REQUEST_SCHEMA_V1
    };
    if request.schema != expected_schema {
        return Err("raw depth demo arm request schema is unsupported".into());
    }
    if request.process_launch_id != launch_id {
        return Err("raw depth demo arm request targets a different process launch".into());
    }
    depth_lab::validate_capture_contract(&request.capture_contract, request.duration_ms)?;
    let expected_target_hz = if request.selected_present.is_some() {
        depth_lab::cp2077_raw_feedback::TARGET_DEPTH_HZ
    } else {
        target_depth_hz(capture_contract)?
    };
    if request.capture_contract != capture_contract
        || request.duration_ms != duration_ms
        || request.target_depth_hz != expected_target_hz
        || request.manual_stop != manual_stop
    {
        return Err(
            "raw depth demo arm request does not match the configured capture contract".into(),
        );
    }
    Ok(())
}

fn target_depth_hz(capture_contract: &str) -> Result<u64, String> {
    match capture_contract {
        depth_lab::ISSUE735_LOCAL_CONTRACT => Ok(ISSUE735_TARGET_DEPTH_HZ),
        depth_lab::PR405_LOCAL_CONTRACT => Ok(PR405_TARGET_DEPTH_HZ),
        _ => Err("raw depth demo capture contract has no depth cadence".into()),
    }
}

/// Resolve the source profile's audited semantic fingerprint to exactly one current-launch DSV.
/// Opaque candidate IDs are never assumed to survive a process restart. Zero or multiple matches
/// remain unarmed. A pre-arm match is only diagnostic and may disappear or change while menus and
/// loading screens rebuild resources. The candidate becomes immutable only after the backend has
/// successfully armed capture.
pub fn selector_for_arm(
    backend: &D3d12ObservationBackend,
    present: u64,
) -> Option<(D3d12DepthSelector, usize)> {
    let runtime = RUNTIME.get()?;
    if runtime.stopping.load(Ordering::Acquire) || runtime.fatal.lock().is_ok_and(|f| f.is_some()) {
        return None;
    }
    let snapshot = backend.snapshot();
    if let Ok(mut observed) = runtime.semantic_candidates_observed.try_lock() {
        *observed = snapshot
            .depth_candidates
            .iter()
            .filter(|candidate| {
                candidate.current
                    && candidate.width == u64::from(runtime.fingerprint.width)
                    && candidate.height == runtime.fingerprint.height
                    && candidate.sample_count == runtime.fingerprint.sample_count
                    && candidate.resource_format == runtime.fingerprint.resource_format
                    && candidate.dsv_format == runtime.fingerprint.dsv_format
            })
            .take(4)
            .map(|candidate| SemanticCandidateTelemetry {
                candidate_id: candidate.candidate_id.clone(),
                resource_epoch: candidate.resource_epoch,
                descriptor_generation: candidate.descriptor_generation,
                observed_after_present_calls: snapshot.present_calls,
                first_present_call_with_clear: candidate.first_present_call_with_clear,
                last_present_call_with_clear: candidate.last_present_call_with_clear,
                pending_clear_calls: candidate.pending_clear_calls,
                clear_calls: candidate.clear_calls,
                eligible_clear_calls: candidate.eligible_clear_calls,
                same_list_capture_eligible_clear_calls: candidate
                    .same_list_capture_eligible_clear_calls,
                execute_calls: candidate.execute_calls,
                present_intervals_with_clear: candidate.present_intervals_with_clear,
                present_intervals_with_one_clear: candidate.present_intervals_with_one_clear,
                present_intervals_with_multiple_clears: candidate
                    .present_intervals_with_multiple_clears,
                max_clears_per_present_interval: candidate.max_clears_per_present_interval,
                same_list_present_intervals: candidate
                    .present_intervals_with_same_list_capture_eligible_clear,
                bind_calls: candidate.bind_calls,
                distinct_command_lists: candidate.distinct_command_lists,
                distinct_direct_queues: candidate.distinct_direct_queues,
                full_semantic_match: semantic_candidate_matches(candidate, runtime.fingerprint),
            })
            .collect();
    }
    if runtime.camera_authoring_windows {
        match authoring_windows::prepare_selection(runtime, backend, &snapshot, present) {
            Ok(Some(selector)) => return Some((selector, runtime.readback_slots)),
            Ok(None) => {}
            Err(error) => {
                set_fatal(runtime, error);
                return None;
            }
        }
    }
    // Authoring deliberately stops sampling while the operator visits the FOV menu. Live
    // resources can retain a valid *historical* fingerprint there. Do not arm/rebind/verify a
    // paused window. Once sampling resumes, require one executed-clear target in this interval
    // and still run every ordinary identity/cadence guard on that original frozen target.
    // Continuous raw, selected-Present and production paths retain their existing policy.
    let matching = sampling_semantic_candidates(
        &snapshot.depth_candidates,
        runtime.fingerprint,
        runtime.camera_authoring_windows,
        authoring_window_allows_sampling()
            || (runtime.camera_authoring_windows && authoring_camera_interval_active()),
    )?;
    runtime
        .semantic_match_count
        .store(matching.len() as u64, Ordering::Release);

    let already_bound = runtime
        .runtime_candidate_id
        .lock()
        .ok()
        .and_then(|candidate| candidate.clone());
    let action = semantic_binding_action(
        &matching,
        runtime.arm_authorized.load(Ordering::Acquire),
        snapshot.capture_armed,
        already_bound.as_deref(),
        (runtime
            .armed_double_clear_interval_baseline
            .load(Ordering::Acquire)
            != u64::MAX)
            .then(|| {
                runtime
                    .armed_double_clear_interval_baseline
                    .load(Ordering::Acquire)
            }),
        (runtime
            .armed_unpaired_clear_baseline
            .load(Ordering::Acquire)
            != u64::MAX)
            .then(|| {
                runtime
                    .armed_unpaired_clear_baseline
                    .load(Ordering::Acquire)
            }),
    );
    let candidate_id = match action {
        SemanticBindingAction::Wait {
            prebound_candidate_id,
        } => {
            if let Ok(mut bound) = runtime.runtime_candidate_id.lock() {
                *bound = prebound_candidate_id;
            } else {
                set_fatal(runtime, "raw D32 runtime candidate lock poisoned");
            }
            return None;
        }
        SemanticBindingAction::Arm { candidate_id } => candidate_id,
        SemanticBindingAction::VerifyArmed => return None,
        SemanticBindingAction::Fault(error) => {
            set_fatal(runtime, error);
            return None;
        }
    };
    let Some(arm_candidate) = matching
        .iter()
        .find(|candidate| candidate.candidate_id == candidate_id)
    else {
        set_fatal(
            runtime,
            "raw D32 arm candidate disappeared before baseline freeze",
        );
        return None;
    };
    runtime.armed_double_clear_interval_baseline.store(
        arm_candidate.present_intervals_with_multiple_clears,
        Ordering::Release,
    );
    runtime
        .armed_unpaired_clear_baseline
        .store(unpaired_clear_count(arm_candidate), Ordering::Release);
    if let Ok(mut bound) = runtime.runtime_candidate_id.lock() {
        *bound = Some(candidate_id.clone());
    } else {
        set_fatal(runtime, "raw D32 runtime candidate lock poisoned");
        return None;
    }
    Some((
        D3d12DepthSelector {
            candidate_id,
            width: runtime.fingerprint.width,
            height: runtime.fingerprint.height,
            sample_count: runtime.fingerprint.sample_count,
            accepted_formats: vec![
                runtime.fingerprint.resource_format,
                runtime.fingerprint.dsv_format,
            ],
            stable_selection: None,
        },
        runtime.readback_slots,
    ))
}

fn semantic_binding_action(
    matching: &[&D3d12DepthCandidateObservation],
    arm_authorized: bool,
    backend_capture_armed: bool,
    bound_candidate_id: Option<&str>,
    armed_double_clear_interval_baseline: Option<u64>,
    armed_unpaired_clear_baseline: Option<u64>,
) -> SemanticBindingAction {
    if backend_capture_armed {
        if matching.len() != 1 {
            return SemanticBindingAction::Fault(format!(
                "raw D32 semantic binding lost uniqueness after backend arm: matches={}",
                matching.len()
            ));
        }
        let Some(bound) = bound_candidate_id else {
            return SemanticBindingAction::Fault(
                "raw D32 backend capture is armed without a runtime semantic binding".into(),
            );
        };
        if bound != matching[0].candidate_id {
            return SemanticBindingAction::Fault(
                "raw D32 semantic binding changed candidate identity after backend arm".into(),
            );
        }
        let Some(baseline) = armed_double_clear_interval_baseline else {
            return SemanticBindingAction::Fault(
                "raw D32 backend capture is armed without a frozen multi-clear baseline".into(),
            );
        };
        if matching[0].present_intervals_with_multiple_clears != baseline {
            return SemanticBindingAction::Fault(
                "raw D32 multi-clear cadence changed after backend arm".into(),
            );
        }
        let Some(unpaired_baseline) = armed_unpaired_clear_baseline else {
            return SemanticBindingAction::Fault(
                "raw D32 backend capture is armed without a frozen unpaired-clear baseline".into(),
            );
        };
        // `unpaired_clear_count` is a point-in-time remainder.  The Present
        // callback can observe the one current clear after Execute and before
        // the matching Present interval is accounted, so a healthy 1:1
        // cadence legitimately alternates between zero and one.  Keep the
        // frozen value as an audited pre-arm upper bound, but do not require
        // equality with that transient remainder after arming.
        if unpaired_clear_count(matching[0])
            > unpaired_baseline.max(D32S8_MAX_PREARM_UNPAIRED_CLEARS)
        {
            return SemanticBindingAction::Fault(
                "raw D32 unpaired-clear cadence changed after backend arm".into(),
            );
        }
        return SemanticBindingAction::VerifyArmed;
    }

    let unique = (matching.len() == 1).then(|| matching[0].candidate_id.clone());
    if !arm_authorized {
        return SemanticBindingAction::Wait {
            prebound_candidate_id: unique,
        };
    }
    unique.map_or(
        SemanticBindingAction::Wait {
            prebound_candidate_id: None,
        },
        |candidate_id| SemanticBindingAction::Arm { candidate_id },
    )
}

fn semantic_candidates(
    candidates: &[D3d12DepthCandidateObservation],
    fingerprint: CandidateFingerprint,
) -> Vec<&D3d12DepthCandidateObservation> {
    candidates
        .iter()
        .filter(|candidate| semantic_candidate_matches(candidate, fingerprint))
        .collect()
}

fn sampling_semantic_candidates(
    candidates: &[D3d12DepthCandidateObservation],
    fingerprint: CandidateFingerprint,
    authoring_windows: bool,
    window_active: bool,
) -> Option<Vec<&D3d12DepthCandidateObservation>> {
    if !authoring_windows {
        return Some(semantic_candidates(candidates, fingerprint));
    }
    if !window_active {
        return None;
    }
    Some(
        semantic_candidates(candidates, fingerprint)
            .into_iter()
            // selector_for_arm precedes note_present. Never use last-Present recency or
            // lifetime bind counts as a substitute for an executed clear in this interval.
            // More than one clear stays visible to the ordinary cadence checks; it is not
            // discarded here to make another candidate appear unique.
            .filter(|candidate| candidate.pending_clear_calls > 0)
            .collect(),
    )
}

fn unpaired_clear_count(candidate: &D3d12DepthCandidateObservation) -> u64 {
    candidate.clear_calls.saturating_sub(
        candidate.present_intervals_with_one_clear.saturating_add(
            candidate
                .present_intervals_with_multiple_clears
                .saturating_mul(2),
        ),
    )
}

fn semantic_candidate_matches(
    candidate: &D3d12DepthCandidateObservation,
    fingerprint: CandidateFingerprint,
) -> bool {
    let expected_clear_flags = match fingerprint.format_family {
        RawDepthFormatFamily::R32D32 => 1,
        RawDepthFormatFamily::R32G8X24D32S8 => 3,
    };
    let strict_clear = candidate.clear_parameters.as_slice()
        == [crate::d3d12_backend::D3d12ClearParameterObservation {
            clear_flags: expected_clear_flags,
            depth_value_bits: 0,
            stencil: 0,
            rect_count: 0,
            calls: candidate.clear_calls,
        }];
    let strict_raw_state = candidate.raw_states_before_clear.as_slice()
        == [crate::d3d12_backend::D3d12RawStateObservation {
            state: Some(D3D12_RESOURCE_STATE_DEPTH_WRITE),
            calls: candidate.clear_calls,
        }];
    let transitions_are_all_resource_non_split = candidate
        .transition_pairs
        .iter()
        .all(|pair| pair.subresource == D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES && !pair.split);
    let has_transition = |before, after| {
        candidate.transition_pairs.iter().any(|pair| {
            pair.state_before == before
                && pair.state_after == after
                && pair.subresource == D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES
                && !pair.split
                && pair.calls >= 2
        })
    };
    let exact_per_clear_proof = candidate.clear_calls >= 2
        && candidate.clear_calls == candidate.eligible_clear_calls
        && candidate.clear_calls == candidate.same_list_capture_eligible_clear_calls
        && candidate.clear_calls == candidate.execute_calls
        && candidate.clear_while_bound_calls == 0;
    let exact_clear_cadence = match fingerprint.format_family {
        RawDepthFormatFamily::R32D32 => {
            // selector_for_arm precedes backend.before_present: Execute has
            // already recorded this frame's clear, while its Present interval
            // is still pending. Admit only that single fully proven clear;
            // closed intervals must retain exact 1:1 same-list coverage.
            let pending_clear = unpaired_clear_count(candidate);
            candidate.present_intervals_with_clear >= 2
                && pending_clear <= 1
                && candidate.clear_calls
                    == candidate
                        .present_intervals_with_clear
                        .saturating_add(pending_clear)
                && candidate.present_intervals_with_clear
                    == candidate.present_intervals_with_one_clear
                && candidate.present_intervals_with_clear
                    == candidate.present_intervals_with_same_list_capture_eligible_clear
                && candidate.present_intervals_with_multiple_clears == 0
                && candidate.max_clears_per_present_interval == 1
        }
        RawDepthFormatFamily::R32G8X24D32S8 => {
            let double_clear_intervals = candidate.present_intervals_with_multiple_clears;
            let unpaired_clears = unpaired_clear_count(candidate);
            double_clear_intervals <= D32S8_MAX_PREARM_DOUBLE_CLEAR_INTERVALS
                && unpaired_clears <= D32S8_MAX_PREARM_UNPAIRED_CLEARS
                && candidate.max_clears_per_present_interval
                    == if double_clear_intervals == 0 { 1 } else { 2 }
                && candidate.present_intervals_with_clear
                    == candidate
                        .present_intervals_with_one_clear
                        .saturating_add(double_clear_intervals)
                && candidate.clear_calls
                    == candidate
                        .present_intervals_with_one_clear
                        .saturating_add(double_clear_intervals.saturating_mul(2))
                        .saturating_add(unpaired_clears)
                && candidate.present_intervals_with_same_list_capture_eligible_clear
                    == candidate.present_intervals_with_clear
        }
    };
    let scene_bind_breadth = fingerprint.format_family == RawDepthFormatFamily::R32D32
        || candidate.bind_calls
            >= candidate
                .clear_calls
                .saturating_mul(D32S8_MIN_BINDS_PER_CLEAR)
            && candidate.distinct_command_lists >= D32S8_MIN_RUNTIME_DISTINCT_COMMAND_LISTS;

    candidate.current
        && candidate.width == u64::from(fingerprint.width)
        && candidate.height == fingerprint.height
        && candidate.sample_count == fingerprint.sample_count
        && candidate.sample_quality == 0
        && candidate.resource_format == fingerprint.resource_format
        && candidate.dsv_format == fingerprint.dsv_format
        && exact_clear_cadence
        && exact_per_clear_proof
        && candidate.distinct_direct_queues == 1
        && scene_bind_breadth
        && strict_clear
        && strict_raw_state
        && transitions_are_all_resource_non_split
        && has_transition(D3D12_RESOURCE_STATE_DEPTH_WRITE, CP2077_COMPOSED_DEPTH_READ)
        && has_transition(CP2077_COMPOSED_DEPTH_READ, CP2077_DEPTH_READ)
        && has_transition(CP2077_DEPTH_READ, D3D12_RESOURCE_STATE_DEPTH_WRITE)
}

fn raw_depth_format_family(resource_format: i32, dsv_format: i32) -> Option<RawDepthFormatFamily> {
    match (resource_format, dsv_format) {
        (DXGI_FORMAT_R32_TYPELESS, DXGI_FORMAT_D32_FLOAT) => Some(RawDepthFormatFamily::R32D32),
        (DXGI_FORMAT_R32G8X24_TYPELESS, DXGI_FORMAT_D32_FLOAT_S8X24_UINT) => {
            Some(RawDepthFormatFamily::R32G8X24D32S8)
        }
        _ => None,
    }
}

#[must_use]
pub fn should_capture_interval(present_seq: u64) -> bool {
    let Some(runtime) = RUNTIME.get() else {
        return false;
    };
    let window_active = !runtime.camera_authoring_windows
        || runtime
            .authoring_interval
            .begin(current_qpc(), runtime.stopping.load(Ordering::Acquire));
    if !window_active {
        return false;
    }
    let candidate_bound = runtime
        .runtime_candidate_id
        .try_lock()
        .is_ok_and(|candidate| candidate.is_some());
    if runtime.stopping.load(Ordering::Acquire)
        || !raw_capture_is_armed(
            runtime.arm_authorized.load(Ordering::Acquire),
            candidate_bound,
        )
        || (!selected_feedback_enabled()
            && runtime.submitted_frames.load(Ordering::Acquire) >= runtime.max_frames)
        || runtime.fatal.lock().is_ok_and(|f| f.is_some())
    {
        return false;
    }
    if selected_feedback_enabled() {
        return runtime
            .feedback
            .lock()
            .is_ok_and(|owner| owner.as_ref().is_some_and(|f| f.capture_next()));
    }
    let now_qpc = current_qpc();
    if now_qpc <= 0 {
        set_fatal(
            runtime,
            "QueryPerformanceCounter failed during raw demo cadence",
        );
        return false;
    }
    let arm_qpc = runtime.arm_request_observed_qpc.load(Ordering::Acquire);
    let duration_ms = effective_duration_ms(runtime.duration_ms, &runtime.capture_contract);
    let duration_ticks = runtime.qpc_frequency.saturating_mul(duration_ms) / 1_000;
    if !runtime.manual_stop && now_qpc.saturating_sub(arm_qpc) >= duration_ticks {
        runtime.stopping.store(true, Ordering::Release);
        return false;
    }
    // PR405's 30 fps RGB contract must select depth by the exact Present
    // identity chosen by WGC.  The local-only CP lane therefore retains one
    // raw candidate for every game Present and compacts the set to the 450
    // even CFR identities after recording.  This is deliberately bounded by
    // max_frames and remains non-production; it avoids guessing a 15 Hz phase
    // independently of the compositor.
    if runtime.capture_contract == depth_lab::PR405_LOCAL_CONTRACT {
        let prior = runtime.requested_intervals.fetch_add(1, Ordering::AcqRel);
        return prior < runtime.max_frames;
    }
    let target_hz = target_depth_hz(&runtime.capture_contract).unwrap_or(PR405_TARGET_DEPTH_HZ);
    let interval_ticks = runtime.qpc_frequency / target_hz as i64;
    loop {
        let previous = runtime.last_cadence_request_qpc.load(Ordering::Acquire);
        if previous != 0 && now_qpc.saturating_sub(previous) < interval_ticks {
            runtime
                .cadence_not_due_presents
                .fetch_add(1, Ordering::Relaxed);
            return false;
        }
        // Advance the ideal 15 Hz clock rather than resetting it to the
        // arrival time of this Present.  Resetting to `now_qpc` discards the
        // fractional remainder at every sample and turns a stable 29-30 Hz
        // game into only ~12.6 Hz.  The bounded catch-up keeps every output
        // tied to a distinct real Present while preserving the requested
        // long-run cadence.
        let next = if previous == 0 {
            now_qpc
        } else {
            previous.saturating_add(interval_ticks).min(now_qpc)
        };
        if runtime
            .last_cadence_request_qpc
            .compare_exchange(previous, next, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            break;
        }
    }
    let prior = runtime.requested_intervals.fetch_add(1, Ordering::AcqRel);
    if prior >= runtime.max_frames {
        return false;
    }
    if runtime.camera_authoring_windows
        && let Err(error) =
            authoring_windows::authorize_interval(runtime, present_seq.saturating_add(1))
    {
        set_fatal(runtime, error);
        return false;
    }
    true
}

fn raw_capture_is_armed(arm_authorized: bool, candidate_bound: bool) -> bool {
    arm_authorized && candidate_bound
}

pub fn submit_frame(frame: D3d12RawDepthFrame, current_present_seq: u64) -> Result<bool, String> {
    submit_payload(
        RawPayload {
            source: Some(frame.source),
            capture_id: frame.capture_id,
            present_seq: frame.present_seq,
            src_width: frame.src_width,
            src_height: frame.src_height,
            row_pitch: frame.row_pitch,
            bytes: frame.bytes,
        },
        current_present_seq,
    )
}

fn submit_selected_frame(
    mut frame: crate::readback::RawDepthFrame,
    capture_id: u64,
    present: u64,
) -> Result<(), String> {
    if !frame.pooled || frame.src_width != frame.dst_width || frame.src_height != frame.dst_height {
        return Err("raw selected readback changed native depth geometry".into());
    }
    let accepted = submit_payload(
        RawPayload {
            source: None,
            capture_id,
            present_seq: frame.present_seq,
            src_width: frame.src_width,
            src_height: frame.src_height,
            row_pitch: frame.row_pitch,
            bytes: std::mem::take(&mut frame.bytes),
        },
        present,
    )?;
    if !accepted {
        return Err("raw selected original-depth frame cap exceeded".into());
    }
    Ok(())
}

fn submit_payload(frame: RawPayload, current_present_seq: u64) -> Result<bool, String> {
    let runtime = RUNTIME.get().ok_or("raw depth demo is not active")?;
    if !runtime.arm_authorized.load(Ordering::Acquire) {
        return Err("raw depth frame arrived before the explicit arm request".into());
    }
    let runtime_candidate_id = runtime
        .runtime_candidate_id
        .try_lock()
        .ok()
        .and_then(|candidate| candidate.clone())
        .ok_or("raw depth frame arrived before semantic candidate binding")?;
    let authoring_phase = if runtime.camera_authoring_windows {
        Some(authoring_windows::claim_frame(
            runtime,
            frame.present_seq,
            frame.source,
        )?)
    } else {
        if let Some(source) = frame.source
            && authoring_windows::candidate_id(source) != runtime_candidate_id
        {
            return Err("raw depth frame source differs from frozen binding".into());
        }
        None
    };
    let arm_request_observed_qpc = runtime.arm_request_observed_qpc.load(Ordering::Acquire);
    let runtime_candidate_id = authoring_phase
        .as_ref()
        .map_or(runtime_candidate_id, |phase| {
            phase.runtime_candidate_id.clone()
        });
    let arm_start_present_seq = runtime.arm_start_present_seq.load(Ordering::Acquire);
    if arm_request_observed_qpc <= 0 || arm_start_present_seq == u64::MAX {
        return Err("raw depth frame has no audited arm observation anchor".into());
    }
    let ordinal = runtime.submitted_frames.load(Ordering::Acquire);
    // Stop prevents new intervals, but a copy requested immediately before
    // the cutoff may complete one Present later.  Preserve that already
    // authorized in-flight frame so a 30 s × 15 Hz contract seals all 450
    // samples instead of deterministically dropping the last one.
    if ordinal >= runtime.max_frames {
        return Ok(false);
    }
    if frame.src_width != runtime.width as usize
        || frame.src_height != runtime.height as usize
        || frame.row_pitch < runtime.row_pitch_expected_min
        || frame.bytes.len() != frame.row_pitch.saturating_mul(frame.src_height)
    {
        let error = "raw D32 frame geometry/footprint drifted from the pinned profile";
        set_fatal(runtime, error);
        return Err(error.into());
    }
    let source_present_qpc = runtime
        .present_stamps
        .try_lock()
        .ok()
        .and_then(|stamps| {
            stamps
                .iter()
                .find(|stamp| stamp.present_seq == frame.present_seq)
                .map(|stamp| stamp.qpc)
        })
        .ok_or_else(|| {
            runtime
                .present_stamp_dropped
                .fetch_add(1, Ordering::Relaxed);
            "raw depth frame has no exact source Present QPC".to_string()
        })?;
    match runtime
        .sender
        .try_send(WriterCommand::Frame(RawFrameForWriter {
            enqueued_at: Instant::now(),
            ordinal,
            current_present_seq,
            source_present_qpc,
            runtime_candidate_id,
            authoring_phase,
            arm_request_observed_qpc,
            arm_start_present_seq,
            frame,
        })) {
        Ok(()) => {
            runtime.submitted_frames.fetch_add(1, Ordering::AcqRel);
            Ok(true)
        }
        Err(TrySendError::Full(_)) => {
            runtime.writer_queue_dropped.fetch_add(1, Ordering::Relaxed);
            set_fatal(runtime, "raw depth writer queue filled");
            Err("raw depth writer queue filled".into())
        }
        Err(TrySendError::Disconnected(_)) => {
            set_fatal(runtime, "raw depth writer disconnected");
            Err("raw depth writer disconnected".into())
        }
    }
}

pub fn record_fault(error: impl Into<String>) {
    if let Some(runtime) = RUNTIME.get() {
        set_fatal(runtime, error.into());
    }
}

fn writer_loop(
    receiver: std::sync::mpsc::Receiver<WriterCommand>,
    index: std::fs::File,
    runtime: Arc<Runtime>,
) {
    let mut index = BufWriter::new(index);
    let mut written = 0u64;
    let mut writer_io = WriterIoMetrics::default();
    let mut camera_archive = camera_archive::ArchiveWriter::new(&runtime.output_dir);
    while let Ok(command) = receiver.recv() {
        match command {
            WriterCommand::Frame(item) => {
                if let Err(error) = write_frame(&runtime, &mut index, item, &mut writer_io) {
                    set_fatal(&runtime, error);
                    break;
                }
                written = written.saturating_add(1);
            }
            WriterCommand::CameraArchive(rows) => {
                if let Err(error) = camera_archive.append(&rows) {
                    set_fatal(&runtime, error);
                }
            }
            WriterCommand::CameraReport(snapshot) => {
                let archive = snapshot
                    .local_archive
                    .is_some()
                    .then_some(&mut camera_archive);
                if let Err(error) = write_render_camera_report(&runtime, *snapshot, archive) {
                    set_fatal(&runtime, error);
                }
            }
            WriterCommand::AuthoringBarrier(ordinal) => {
                if let Err(error) = index.flush().and_then(|_| index.get_ref().sync_all()) {
                    set_fatal(&runtime, format!("sync authoring boundary: {error}"));
                    break;
                }
                runtime
                    .authoring_writer_barrier
                    .store(ordinal, Ordering::Release);
            }
            WriterCommand::Stop => break,
        }
    }
    if let Err(error) = index.flush().and_then(|_| index.get_ref().sync_all()) {
        set_fatal(&runtime, format!("sync raw depth index failed: {error}"));
    }
    if runtime.feedback_enabled.load(Ordering::Acquire)
        && let Ok(mut owner) = runtime.feedback.lock()
        && let Some(owner) = owner.as_mut()
        && let Err(error) = owner.seal_report(&runtime, written)
    {
        set_fatal(&runtime, error);
    }
    write_telemetry(&runtime, written, &writer_io);
}

fn write_frame(
    runtime: &Runtime,
    index: &mut BufWriter<std::fs::File>,
    item: RawFrameForWriter,
    writer_io: &mut WriterIoMetrics,
) -> Result<(), String> {
    let name = format!("{:012}.d32", item.ordinal);
    let path = runtime.output_dir.join(FRAME_DIR).join(&name);
    write_durable_file(
        &path,
        &item.frame.bytes,
        item.ordinal,
        item.enqueued_at,
        writer_io,
    )?;
    let row = FrameIndex {
        schema: if item.authoring_phase.is_some() {
            "dm-cp2077-raw-depth-frame-index/v2"
        } else {
            RAW_DEMO_INDEX_SCHEMA_V1
        },
        ordinal: item.ordinal,
        file: &name,
        present_seq: item.frame.present_seq,
        present_qpc: item.source_present_qpc,
        current_present_seq: item.current_present_seq,
        readback_lag_presents: item
            .current_present_seq
            .saturating_sub(item.frame.present_seq),
        capture_id: item.frame.capture_id,
        source_candidate_id: &runtime.source_candidate_id,
        runtime_candidate_id: &item.runtime_candidate_id,
        authoring_phase: item.authoring_phase.as_ref(),
        arm_request_observed_qpc: item.arm_request_observed_qpc,
        arm_start_present_seq: item.arm_start_present_seq,
        width: item.frame.src_width,
        height: item.frame.src_height,
        row_pitch: item.frame.row_pitch,
        pixel_format: "DXGI_FORMAT_D32_FLOAT_LE",
        byte_len: item.frame.bytes.len(),
        rgb_pairing: "same_present_seq_gpu_watermark_submitted",
    };
    serde_json::to_writer(&mut *index, &row)
        .map_err(|error| format!("serialize raw depth index row failed: {error}"))?;
    index
        .write_all(b"\n")
        .map_err(|error| format!("write raw depth index row failed: {error}"))?;
    Ok(())
}

fn stop_watcher(runtime: Arc<Runtime>, backend: Arc<D3d12ObservationBackend>) {
    let stop_path = runtime.output_dir.join(STOP_FILE);
    let mut authoring = authoring_windows::Windows::default();
    loop {
        let explicit_stop = match validate_stop_request_file(&stop_path, &runtime.launch_id) {
            Ok(value) => value,
            Err(error) => {
                set_fatal(&runtime, error);
                true
            }
        };
        let arm_qpc = runtime.arm_request_observed_qpc.load(Ordering::Acquire);
        if runtime.camera_authoring_windows
            && arm_qpc > 0
            && !explicit_stop
            && runtime.fatal.lock().is_ok_and(|f| f.is_none())
            && let Err(error) = authoring.poll(&runtime)
        {
            set_fatal(&runtime, error);
        }
        let duration_ms = effective_duration_ms(runtime.duration_ms, &runtime.capture_contract);
        let selected = runtime.feedback_enabled.load(Ordering::Acquire);
        let duration_complete = !selected
            && !runtime.manual_stop
            && arm_qpc > 0
            && current_qpc().saturating_sub(arm_qpc)
                >= runtime.qpc_frequency.saturating_mul(duration_ms) / 1_000;
        let manual_safety_expired = (selected || runtime.manual_stop)
            && arm_qpc > 0
            && current_qpc().saturating_sub(arm_qpc)
                >= runtime.qpc_frequency.saturating_mul(MANUAL_STOP_SAFETY_MS) / 1_000;
        if manual_safety_expired {
            set_fatal(
                &runtime,
                "manual-stop raw demo exceeded the 240 second safety ceiling",
            );
        }
        if explicit_stop
            || (!selected && runtime.submitted_frames.load(Ordering::Acquire) >= runtime.max_frames)
            || duration_complete
            || manual_safety_expired
            || runtime.fatal.lock().is_ok_and(|fatal| fatal.is_some())
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    if runtime.feedback_enabled.load(Ordering::Acquire) {
        // Only Present drains the SPSC and GPU owner. Stop must not race Seal ACK/readback.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !runtime.feedback_drained.load(Ordering::Acquire)
            && runtime.fatal.lock().is_ok_and(|f| f.is_none())
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        if !runtime.feedback_drained.load(Ordering::Acquire) {
            set_fatal(
                &runtime,
                "raw selected-Present stopped without sealed, drained feedback",
            );
        }
    }
    runtime.stopping.store(true, Ordering::Release);
    if runtime.camera_authoring_windows && runtime.fatal.lock().is_ok_and(|f| f.is_none()) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let drained = !runtime.authoring_interval.active.load(Ordering::Acquire)
                && runtime
                    .authoring_control
                    .lock()
                    .is_ok_and(|c| !c.has_authorized_frames())
                && backend.authoring_boundary_is_drained().unwrap_or(false);
            if drained {
                break;
            }
            if Instant::now() >= deadline {
                set_fatal(
                    &runtime,
                    "authoring stop could not complete authorized tail work",
                );
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    // Join the serialized Present producer before freezing/queuing the final report.
    match runtime.feedback.lock() {
        Ok(_owner) => {
            if let Err(error) = backend.freeze_local_camera_archive() {
                set_fatal(&runtime, error);
            }
        }
        Err(_) => set_fatal(
            &runtime,
            "raw feedback poisoned while freezing camera archive",
        ),
    }
    runtime
        .stop_observed_qpc
        .compare_exchange(0, current_qpc(), Ordering::AcqRel, Ordering::Acquire)
        .ok();
    if let Err(error) = runtime
        .pose_worker
        .stop_and_seal(&runtime.launch_id, &runtime.output_dir)
    {
        set_fatal(
            &runtime,
            format!("seal raw-demo CP2077 pose report failed: {error}"),
        );
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while backend.snapshot().capture_in_flight != 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    let render_camera_observation = backend.render_camera_snapshot();
    if backend.snapshot().capture_in_flight == 0 {
        if let Err(error) = backend.disarm_depth_capture() {
            set_fatal(&runtime, format!("disarm raw D32 capture failed: {error}"));
        }
    } else {
        set_fatal(
            &runtime,
            "raw D32 capture did not drain within stop deadline",
        );
    }
    runtime.capture_pool_dropped.store(
        backend.snapshot().capture_dropped_pool_full,
        Ordering::Release,
    );
    if runtime.camera_authoring_windows
        && let Err(error) = authoring_windows::write_timeline(&runtime)
    {
        set_fatal(&runtime, error);
    }
    if runtime.feedback_enabled.load(Ordering::Acquire) || runtime.camera_authoring_windows {
        // Same writer and FIFO as every archived batch; no final JSON before durable evidence.
        if runtime
            .sender
            .send(WriterCommand::CameraReport(Box::new(
                render_camera_observation,
            )))
            .is_err()
        {
            set_fatal(&runtime, "camera report writer disconnected");
        }
    } else if let Err(error) = write_render_camera_report(&runtime, render_camera_observation, None)
    {
        set_fatal(&runtime, error);
    }
    if runtime.sender.send(WriterCommand::Stop).is_err() {
        set_fatal(&runtime, "raw depth writer disconnected before stop");
    }
}

fn write_render_camera_report(
    runtime: &Runtime,
    observation: D3d12RenderCameraSnapshot,
    archive: Option<&mut camera_archive::ArchiveWriter>,
) -> Result<(), String> {
    let report = RenderCameraReport {
        schema: "dm-cp2077-render-camera-observation/v2",
        process_launch_id: &runtime.launch_id,
        non_trainable: true,
        production_certified: false,
        deliverable: false,
        recording_ready: false,
        observation,
    };
    let path = runtime.output_dir.join(RENDER_CAMERA_FILE);
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|error| format!("create render-camera observation failed: {error}"))?;
    let mut file = BufWriter::new(file);
    if let Some(archive) = archive {
        archive.write_report(&mut file, &report)?;
    } else {
        serde_json::to_writer_pretty(&mut file, &report)
            .map_err(|error| format!("serialize render-camera observation failed: {error}"))?;
    }
    file.write_all(b"\n")
        .and_then(|_| file.flush())
        .and_then(|_| file.get_ref().sync_all())
        .map_err(|error| format!("sync render-camera observation failed: {error}"))
}

fn validate_stop_request_file(path: &Path, launch_id: &str) -> Result<bool, String> {
    let Ok(metadata) = path.metadata() else {
        return Ok(false);
    };
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_CONTROL_REQUEST_BYTES {
        return Err("raw depth demo stop request is not a bounded regular file".into());
    }
    let bytes = std::fs::read(path)
        .map_err(|error| format!("read raw depth demo stop request failed: {error}"))?;
    let request: RawDemoStopRequest = serde_json::from_slice(&bytes)
        .map_err(|error| format!("parse raw depth demo stop request failed: {error}"))?;
    validate_stop_request(&request, launch_id)?;
    Ok(true)
}

fn validate_stop_request(request: &RawDemoStopRequest, launch_id: &str) -> Result<(), String> {
    if request.schema != RAW_DEMO_STOP_REQUEST_SCHEMA_V1 {
        return Err("raw depth demo stop request schema is unsupported".into());
    }
    if request.process_launch_id != launch_id {
        return Err("raw depth demo stop request targets a different process launch".into());
    }
    Ok(())
}

fn write_telemetry(runtime: &Runtime, written: u64, writer_io: &WriterIoMetrics) {
    let fatal = runtime.fatal.lock().ok().and_then(|value| value.clone());
    let runtime_candidate_id = runtime
        .runtime_candidate_id
        .lock()
        .ok()
        .and_then(|candidate| candidate.clone());
    let semantic_candidates_observed = runtime
        .semantic_candidates_observed
        .lock()
        .map(|candidates| candidates.clone())
        .unwrap_or_default();
    let arm_request_observed_qpc = runtime.arm_request_observed_qpc.load(Ordering::Acquire);
    let arm_start_present_seq = runtime.arm_start_present_seq.load(Ordering::Acquire);
    let stop_observed_qpc = runtime.stop_observed_qpc.load(Ordering::Acquire);
    let elapsed_ms = (arm_request_observed_qpc > 0
        && stop_observed_qpc >= arm_request_observed_qpc)
        .then(|| {
            u64::try_from(stop_observed_qpc.saturating_sub(arm_request_observed_qpc))
                .unwrap_or(u64::MAX)
                .saturating_mul(1_000)
                / u64::try_from(runtime.qpc_frequency).unwrap_or(1)
        });
    let telemetry = Telemetry {
        schema: RAW_DEMO_TELEMETRY_SCHEMA_V1,
        sampling_mode: if runtime.camera_authoring_windows {
            Some("discontinuous_camera_authoring_windows/v2")
        } else {
            runtime
                .feedback_enabled
                .load(Ordering::Acquire)
                .then_some("selected_present_even_cfr_gpu_candidates/v2")
        },
        process_launch_id: &runtime.launch_id,
        source_discovery_launch_id: &runtime.source_discovery_launch_id,
        profile_sha256: &runtime.profile_sha256,
        source_d3d12_report_sha256: &runtime.source_report_sha256,
        source_candidate_id: &runtime.source_candidate_id,
        runtime_candidate_id: runtime_candidate_id.as_deref(),
        semantic_match_count: runtime.semantic_match_count.load(Ordering::Acquire),
        semantic_candidates_observed,
        arm_request_observed_qpc: (arm_request_observed_qpc > 0)
            .then_some(arm_request_observed_qpc),
        arm_start_present_seq: (arm_start_present_seq != u64::MAX).then_some(arm_start_present_seq),
        stop_observed_qpc: (stop_observed_qpc > 0).then_some(stop_observed_qpc),
        elapsed_ms,
        capture_contract: &runtime.capture_contract,
        duration_ms: runtime.duration_ms,
        manual_stop: runtime.manual_stop,
        target_depth_hz: if runtime.feedback_enabled.load(Ordering::Acquire) {
            depth_lab::cp2077_raw_feedback::TARGET_DEPTH_HZ
        } else {
            target_depth_hz(&runtime.capture_contract).unwrap_or(PR405_TARGET_DEPTH_HZ)
        },
        cadence_not_due_presents: runtime.cadence_not_due_presents.load(Ordering::Acquire),
        max_frames: runtime.max_frames,
        present_stride: runtime.present_stride,
        qpc_frequency: runtime.qpc_frequency,
        requested_intervals: runtime.requested_intervals.load(Ordering::Acquire),
        submitted_frames: runtime.submitted_frames.load(Ordering::Acquire),
        written_frames: written,
        writer_queue_capacity: WRITER_QUEUE_CAPACITY,
        writer_io,
        writer_queue_dropped: runtime.writer_queue_dropped.load(Ordering::Acquire),
        present_stamp_dropped: runtime.present_stamp_dropped.load(Ordering::Acquire),
        capture_pool_dropped: runtime.capture_pool_dropped.load(Ordering::Acquire),
        fatal_error: fatal.as_deref(),
        non_trainable: true,
        production_certified: false,
        upload_allowed: false,
        deliverable: false,
        recording_ready: false,
        formal_start_run_used: false,
    };
    let path = runtime.output_dir.join(TELEMETRY_FILE);
    let result = (|| -> Result<(), String> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|error| format!("create raw depth telemetry failed: {error}"))?;
        serde_json::to_writer_pretty(&mut file, &telemetry)
            .map_err(|error| format!("serialize raw depth telemetry failed: {error}"))?;
        file.write_all(b"\n")
            .and_then(|_| file.sync_all())
            .map_err(|error| format!("sync raw depth telemetry failed: {error}"))
    })();
    if let Err(error) = result {
        crate::probe::log(&error);
    }
}

fn set_fatal(runtime: &Runtime, error: impl Into<String>) {
    set_first_fatal(&runtime.fatal, error.into());
}

fn set_first_fatal(fatal: &Mutex<Option<String>>, error: String) {
    let mut fatal = fatal
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if fatal.is_none() {
        *fatal = Some(error);
    }
}

fn current_qpc() -> i64 {
    let mut qpc = 0i64;
    if unsafe { QueryPerformanceCounter(&mut qpc) }.is_ok() {
        qpc
    } else {
        0
    }
}

fn qpc_frequency() -> i64 {
    let mut frequency = 0i64;
    if unsafe { QueryPerformanceFrequency(&mut frequency) }.is_ok() {
        frequency
    } else {
        0
    }
}

fn require_sha256(value: &str, label: &str) -> Result<(), String> {
    let valid = value.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    });
    valid
        .then_some(())
        .ok_or_else(|| format!("{label} is not lowercase sha256:<64 hex>"))
}

fn valid_launch_id(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn sha256_bytes(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn path_is_within_case_insensitive(path: &Path, root: &Path) -> bool {
    let components = |value: &Path| {
        value
            .components()
            .map(|component| component.as_os_str().to_string_lossy().to_ascii_lowercase())
            .collect::<Vec<_>>()
    };
    components(path).starts_with(&components(root))
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::d3d12_backend::{
        D3d12ClearParameterObservation, D3d12RawStateObservation, D3d12TransitionPairObservation,
    };

    /// Both coordinated contracts arm before WGC, so both must hand the
    /// readiness lead back or their depth window closes before the RGB window
    /// does. issue735 had no grace at all: the 2026-09-10 attempt armed 942 ms
    /// early and its last depth Present landed at RGB t=63.097 s of a 64.001 s
    /// recording.
    #[test]
    fn coordinated_contracts_hand_back_their_arm_before_wgc_lead() {
        let nominal = depth_lab::ISSUE735_LOCAL_DURATION_MS;
        let issue735 = effective_duration_ms(nominal, depth_lab::ISSUE735_LOCAL_CONTRACT);
        assert_eq!(issue735, nominal as i64 + ISSUE735_READINESS_LEAD_GRACE_MS);
        assert!(
            issue735 - nominal as i64 >= 942,
            "grace must cover the measured arm-to-Started lead"
        );

        let pr405 = effective_duration_ms(
            depth_lab::PR405_LOCAL_DURATION_MS,
            depth_lab::PR405_LOCAL_CONTRACT,
        );
        assert_eq!(
            pr405,
            depth_lab::PR405_LOCAL_DURATION_MS as i64 + PR405_READBACK_DRAIN_GRACE_MS,
            "PR405's validated tail is unchanged"
        );

        // An unrecognised contract keeps the bare nominal window.
        assert_eq!(effective_duration_ms(nominal, "other"), nominal as i64);
    }

    #[test]
    fn fatal_waits_for_contention_and_preserves_the_first_error() {
        let fatal = Arc::new(Mutex::new(None));
        let guard = fatal.lock().unwrap();
        let worker_fatal = Arc::clone(&fatal);
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            set_first_fatal(&worker_fatal, "writer failed".into());
            done_tx.send(()).unwrap();
        });
        assert!(
            done_rx
                .recv_timeout(std::time::Duration::from_millis(25))
                .is_err()
        );
        drop(guard);
        done_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        worker.join().unwrap();
        set_first_fatal(&fatal, "later failure".into());
        assert_eq!(fatal.lock().unwrap().as_deref(), Some("writer failed"));
    }

    pub(super) fn config() -> RawDemoConfig {
        RawDemoConfig {
            schema: RAW_DEMO_CONFIG_SCHEMA_V1.into(),
            output_dir: ".".into(),
            profile_file: PROFILE_FILE.into(),
            profile_sha256: format!("sha256:{}", "a".repeat(64)),
            capture_contract: depth_lab::ISSUE735_LOCAL_CONTRACT.into(),
            duration_ms: depth_lab::ISSUE735_LOCAL_DURATION_MS,
            manual_stop: false,
            camera_authoring_windows: false,
            max_frames: 960,
            present_stride: 4,
            recording_ready: false,
            non_trainable: true,
            upload_allowed: false,
            formal_start_run: false,
        }
    }

    fn fingerprint() -> CandidateFingerprint {
        CandidateFingerprint {
            width: 1280,
            height: 720,
            sample_count: 1,
            resource_format: 39,
            dsv_format: 40,
            format_family: RawDepthFormatFamily::R32D32,
        }
    }

    fn d32s8_fingerprint() -> CandidateFingerprint {
        CandidateFingerprint {
            resource_format: DXGI_FORMAT_R32G8X24_TYPELESS,
            dsv_format: DXGI_FORMAT_D32_FLOAT_S8X24_UINT,
            format_family: RawDepthFormatFamily::R32G8X24D32S8,
            ..fingerprint()
        }
    }

    fn candidate(candidate_id: &str) -> D3d12DepthCandidateObservation {
        D3d12DepthCandidateObservation {
            candidate_id: candidate_id.into(),
            descriptor_generation: 7,
            resource_epoch: 3,
            width: 1280,
            height: 720,
            sample_count: 1,
            sample_quality: 0,
            resource_format: 39,
            dsv_format: 40,
            clear_calls: 8,
            eligible_clear_calls: 8,
            same_list_capture_eligible_clear_calls: 8,
            execute_calls: 8,
            present_intervals_with_clear: 8,
            present_intervals_with_one_clear: 8,
            present_intervals_with_multiple_clears: 0,
            max_clears_per_present_interval: 1,
            first_present_call_with_clear: Some(2),
            last_present_call_with_clear: Some(9),
            pending_clear_calls: 0,
            present_intervals_with_same_list_capture_eligible_clear: 8,
            bind_calls: 0,
            clear_while_bound_calls: 0,
            clear_parameters: vec![D3d12ClearParameterObservation {
                clear_flags: 1,
                depth_value_bits: 0,
                stencil: 0,
                rect_count: 0,
                calls: 8,
            }],
            raw_states_before_clear: vec![D3d12RawStateObservation {
                state: Some(D3D12_RESOURCE_STATE_DEPTH_WRITE),
                calls: 8,
            }],
            transition_pairs: vec![
                D3d12TransitionPairObservation {
                    state_before: D3D12_RESOURCE_STATE_DEPTH_WRITE,
                    state_after: CP2077_COMPOSED_DEPTH_READ,
                    subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                    split: false,
                    calls: 8,
                },
                D3d12TransitionPairObservation {
                    state_before: CP2077_COMPOSED_DEPTH_READ,
                    state_after: CP2077_DEPTH_READ,
                    subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                    split: false,
                    calls: 8,
                },
                D3d12TransitionPairObservation {
                    state_before: CP2077_DEPTH_READ,
                    state_after: D3D12_RESOURCE_STATE_DEPTH_WRITE,
                    subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                    split: false,
                    calls: 8,
                },
            ],
            distinct_command_lists: 2,
            distinct_direct_queues: 1,
            current: true,
        }
    }

    fn d32s8_candidate(candidate_id: &str) -> D3d12DepthCandidateObservation {
        let mut candidate = candidate(candidate_id);
        candidate.resource_format = DXGI_FORMAT_R32G8X24_TYPELESS;
        candidate.dsv_format = DXGI_FORMAT_D32_FLOAT_S8X24_UINT;
        candidate.bind_calls = 256;
        candidate.distinct_command_lists = D32S8_MIN_RUNTIME_DISTINCT_COMMAND_LISTS;
        candidate.clear_parameters[0].clear_flags = 3;
        candidate
    }

    #[test]
    fn config_is_bounded_and_cannot_claim_formal_readiness() {
        assert!(validate_config(&config()).is_ok());
        let mut pr405 = config();
        pr405.capture_contract = depth_lab::PR405_LOCAL_CONTRACT.into();
        pr405.duration_ms = depth_lab::PR405_LOCAL_DURATION_MS;
        assert!(validate_config(&pr405).is_ok());
        pr405.duration_ms = depth_lab::ISSUE735_LOCAL_DURATION_MS;
        assert!(validate_config(&pr405).is_err());
        let mut ready = config();
        ready.recording_ready = true;
        assert!(validate_config(&ready).is_err());
        let mut formal = config();
        formal.formal_start_run = true;
        assert!(validate_config(&formal).is_err());
        let mut too_many = config();
        too_many.max_frames = AUTOMATIC_MAX_FRAMES + 1;
        assert!(validate_config(&too_many).is_err());
        let mut manual = config();
        manual.manual_stop = true;
        manual.max_frames = MANUAL_STOP_MAX_FRAMES;
        assert!(validate_config(&manual).is_ok());
        manual.max_frames += 1;
        assert!(validate_config(&manual).is_err());
    }

    #[test]
    fn semantic_binding_accepts_a_new_opaque_id_with_the_full_profile_fingerprint() {
        let current = candidate("dsv-r77-g7-e3");
        let matches = semantic_candidates(std::slice::from_ref(&current), fingerprint());
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].candidate_id, "dsv-r77-g7-e3");
        assert_ne!(matches[0].candidate_id, "dsv-r32-g1-e1");
    }

    #[test]
    fn semantic_binding_rejects_two_equally_proven_current_candidates() {
        let candidates = [candidate("dsv-r77-g7-e3"), candidate("dsv-r91-g2-e3")];
        assert_eq!(semantic_candidates(&candidates, fingerprint()).len(), 2);
    }

    #[test]
    fn d32_pre_present_clear_can_arm_then_complete_its_interval() {
        // CP2077 build 20383525, 2026-09-09: selector_for_arm runs before
        // before_present accounts for the current interval. Every clear was
        // proven, but the snapshot had 4295 clears and 4294 closed intervals.
        let mut current = candidate("dsv-current");
        let expected = CandidateFingerprint {
            width: 1920,
            height: 1080,
            ..fingerprint()
        };
        current.width = 1920;
        current.height = 1080;
        current.clear_calls = 4295;
        current.eligible_clear_calls = 4295;
        current.same_list_capture_eligible_clear_calls = 4295;
        current.execute_calls = 4295;
        current.clear_parameters[0].calls = 4295;
        current.raw_states_before_clear[0].calls = 4295;
        current.present_intervals_with_clear = 4294;
        current.present_intervals_with_one_clear = 4294;
        current.present_intervals_with_same_list_capture_eligible_clear = 4294;

        let matches = semantic_candidates(std::slice::from_ref(&current), expected);
        assert_eq!(matches.len(), 1);
        assert_eq!(unpaired_clear_count(&current), 1);
        assert_eq!(
            semantic_binding_action(&matches, true, false, None, None, None),
            SemanticBindingAction::Arm {
                candidate_id: "dsv-current".into(),
            }
        );
        current.present_intervals_with_clear += 1;
        current.present_intervals_with_one_clear += 1;
        current.present_intervals_with_same_list_capture_eligible_clear += 1;
        let settled = semantic_candidates(std::slice::from_ref(&current), expected);
        assert_eq!(settled.len(), 1);
        assert_eq!(unpaired_clear_count(&current), 0);
        assert_eq!(
            semantic_binding_action(&settled, true, true, Some("dsv-current"), Some(0), Some(1)),
            SemanticBindingAction::VerifyArmed
        );
    }

    #[test]
    fn d32_pending_clear_never_admits_inconsistent_or_unproven_intervals() {
        let mut pending = candidate("dsv-current");
        pending.present_intervals_with_clear = 7;
        pending.present_intervals_with_one_clear = 7;
        pending.present_intervals_with_same_list_capture_eligible_clear = 7;

        let mut invalid = pending.clone();
        invalid.present_intervals_with_clear = 6;
        invalid.present_intervals_with_one_clear = 6;
        invalid.present_intervals_with_same_list_capture_eligible_clear = 6;
        assert!(!semantic_candidate_matches(&invalid, fingerprint()));

        let mut invalid = pending.clone();
        invalid.same_list_capture_eligible_clear_calls -= 1;
        assert!(!semantic_candidate_matches(&invalid, fingerprint()));

        let mut invalid = pending.clone();
        invalid.present_intervals_with_same_list_capture_eligible_clear -= 1;
        assert!(!semantic_candidate_matches(&invalid, fingerprint()));

        let mut invalid = pending.clone();
        invalid.present_intervals_with_multiple_clears = 1;
        invalid.max_clears_per_present_interval = 2;
        assert!(!semantic_candidate_matches(&invalid, fingerprint()));

        let mut invalid = pending;
        invalid.present_intervals_with_clear = 9;
        invalid.present_intervals_with_one_clear = 9;
        invalid.present_intervals_with_same_list_capture_eligible_clear = 9;
        assert!(!semantic_candidate_matches(&invalid, fingerprint()));
    }

    #[test]
    fn semantic_binding_rejects_weak_or_non_exact_evidence() {
        let mut weak = candidate("dsv-r77-g7-e3");
        weak.same_list_capture_eligible_clear_calls = 1;
        assert!(semantic_candidates(&[weak], fingerprint()).is_empty());

        let mut mixed_clear = candidate("dsv-r77-g7-e3");
        mixed_clear
            .clear_parameters
            .push(D3d12ClearParameterObservation {
                clear_flags: 1,
                depth_value_bits: 1.0f32.to_bits(),
                stencil: 0,
                rect_count: 0,
                calls: 1,
            });
        assert!(semantic_candidates(&[mixed_clear], fingerprint()).is_empty());

        let mut split = candidate("dsv-r77-g7-e3");
        split.transition_pairs[0].split = true;
        assert!(semantic_candidates(&[split], fingerprint()).is_empty());
    }

    #[test]
    fn d32s8_semantic_binding_requires_exact_cp_scene_bind_breadth() {
        let current = d32s8_candidate("dsv-r26-g9-e4");
        let matches = semantic_candidates(std::slice::from_ref(&current), d32s8_fingerprint());
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].candidate_id, "dsv-r26-g9-e4");

        let mut weak_binds = current.clone();
        weak_binds.bind_calls = weak_binds.clear_calls * D32S8_MIN_BINDS_PER_CLEAR - 1;
        assert!(semantic_candidates(&[weak_binds], d32s8_fingerprint()).is_empty());

        let mut narrow_lists = current.clone();
        narrow_lists.distinct_command_lists = D32S8_MIN_RUNTIME_DISTINCT_COMMAND_LISTS - 1;
        assert!(semantic_candidates(&[narrow_lists], d32s8_fingerprint()).is_empty());

        let mut depth_only_clear = current.clone();
        depth_only_clear.clear_parameters[0].clear_flags = 1;
        assert!(semantic_candidates(&[depth_only_clear], d32s8_fingerprint()).is_empty());
    }

    #[test]
    fn d32s8_freezes_one_prearm_double_clear_and_faults_on_post_arm_change() {
        let mut current = d32s8_candidate("dsv-r26-g9-e4");
        current.clear_calls = 9;
        current.eligible_clear_calls = 9;
        current.same_list_capture_eligible_clear_calls = 9;
        current.execute_calls = 9;
        current.present_intervals_with_clear = 8;
        current.present_intervals_with_one_clear = 7;
        current.present_intervals_with_multiple_clears = 1;
        current.max_clears_per_present_interval = 2;
        current.present_intervals_with_same_list_capture_eligible_clear = 8;
        current.clear_parameters[0].calls = 9;
        current.raw_states_before_clear[0].calls = 9;
        current.bind_calls = 9 * D32S8_MIN_BINDS_PER_CLEAR;

        let matches = semantic_candidates(std::slice::from_ref(&current), d32s8_fingerprint());
        assert_eq!(matches.len(), 1);
        assert_eq!(
            semantic_binding_action(
                &matches,
                true,
                true,
                Some("dsv-r26-g9-e4"),
                Some(1),
                Some(0),
            ),
            SemanticBindingAction::VerifyArmed
        );

        current.clear_calls = 10;
        current.eligible_clear_calls = 10;
        current.same_list_capture_eligible_clear_calls = 10;
        current.execute_calls = 10;
        current.present_intervals_with_clear = 8;
        current.present_intervals_with_one_clear = 6;
        current.present_intervals_with_multiple_clears = 2;
        current.clear_parameters[0].calls = 10;
        current.raw_states_before_clear[0].calls = 10;
        assert!(semantic_candidates(&[current], d32s8_fingerprint()).is_empty());
    }

    #[test]
    fn d32s8_freezes_one_present_callback_unpaired_clear() {
        let mut current = d32s8_candidate("dsv-r26-g9-e4");
        current.clear_calls = 9;
        current.eligible_clear_calls = 9;
        current.same_list_capture_eligible_clear_calls = 9;
        current.execute_calls = 9;
        current.present_intervals_with_clear = 8;
        current.present_intervals_with_one_clear = 8;
        current.present_intervals_with_multiple_clears = 0;
        current.present_intervals_with_same_list_capture_eligible_clear = 8;
        current.clear_parameters[0].calls = 9;
        current.raw_states_before_clear[0].calls = 9;
        current.bind_calls = 9 * D32S8_MIN_BINDS_PER_CLEAR;

        let matches = semantic_candidates(std::slice::from_ref(&current), d32s8_fingerprint());
        assert_eq!(matches.len(), 1);
        assert_eq!(unpaired_clear_count(&current), 1);
        assert_eq!(
            semantic_binding_action(
                &matches,
                true,
                true,
                Some("dsv-r26-g9-e4"),
                Some(0),
                Some(1),
            ),
            SemanticBindingAction::VerifyArmed
        );

        current.clear_calls = 8;
        current.eligible_clear_calls = 8;
        current.same_list_capture_eligible_clear_calls = 8;
        current.execute_calls = 8;
        current.clear_parameters[0].calls = 8;
        current.raw_states_before_clear[0].calls = 8;
        current.bind_calls = 8 * D32S8_MIN_BINDS_PER_CLEAR;
        let settled = semantic_candidates(std::slice::from_ref(&current), d32s8_fingerprint());
        assert_eq!(settled.len(), 1);
        assert_eq!(
            semantic_binding_action(
                &settled,
                true,
                true,
                Some("dsv-r26-g9-e4"),
                Some(0),
                Some(1),
            ),
            SemanticBindingAction::VerifyArmed
        );
    }

    #[test]
    fn d32_and_d32s8_fingerprints_do_not_cross_match() {
        assert!(semantic_candidates(&[candidate("dsv-r31-g1-e1")], d32s8_fingerprint()).is_empty());
        assert!(semantic_candidates(&[d32s8_candidate("dsv-r26-g1-e1")], fingerprint()).is_empty());
    }

    #[test]
    fn prearm_candidate_churn_waits_and_only_backend_armed_drift_faults() {
        let r17 = candidate("dsv-r17-g1-e1");
        let r23 = candidate("dsv-r23-g1-e1");
        let r31 = candidate("dsv-r31-g1-e1");
        let one_r17 = semantic_candidates(std::slice::from_ref(&r17), fingerprint());
        assert_eq!(
            semantic_binding_action(&one_r17, false, false, None, None, None),
            SemanticBindingAction::Wait {
                prebound_candidate_id: Some("dsv-r17-g1-e1".into())
            }
        );
        assert_eq!(
            semantic_binding_action(&[], false, false, Some("dsv-r17-g1-e1"), None, None),
            SemanticBindingAction::Wait {
                prebound_candidate_id: None
            }
        );
        let ambiguous_candidates = [r17.clone(), r23];
        let ambiguous = semantic_candidates(&ambiguous_candidates, fingerprint());
        assert_eq!(
            semantic_binding_action(&ambiguous, false, false, None, None, None),
            SemanticBindingAction::Wait {
                prebound_candidate_id: None
            }
        );

        let one_r31 = semantic_candidates(std::slice::from_ref(&r31), fingerprint());
        assert_eq!(
            semantic_binding_action(&one_r31, true, false, Some("dsv-r17-g1-e1"), None, None,),
            SemanticBindingAction::Arm {
                candidate_id: "dsv-r31-g1-e1".into()
            }
        );
        assert_eq!(
            semantic_binding_action(
                &one_r31,
                true,
                true,
                Some("dsv-r31-g1-e1"),
                Some(0),
                Some(0),
            ),
            SemanticBindingAction::VerifyArmed
        );
        assert!(matches!(
            semantic_binding_action(&[], true, true, Some("dsv-r31-g1-e1"), Some(0), Some(0),),
            SemanticBindingAction::Fault(_)
        ));
        assert!(matches!(
            semantic_binding_action(
                &one_r17,
                true,
                true,
                Some("dsv-r31-g1-e1"),
                Some(0),
                Some(0),
            ),
            SemanticBindingAction::Fault(_)
        ));
    }

    #[test]
    fn explicit_phase_boundary_revalidates_replacement_but_active_window_stays_immutable() {
        let old = d32s8_candidate("dsv-r6-g1-e1");
        let mut replacement = d32s8_candidate("dsv-r29-g1-e1");
        replacement.pending_clear_calls = 1;
        replacement.clear_calls += 1;
        replacement.eligible_clear_calls += 1;
        replacement.same_list_capture_eligible_clear_calls += 1;
        replacement.execute_calls += 1;
        replacement.clear_parameters[0].calls += 1;
        replacement.raw_states_before_clear[0].calls += 1;
        let pair = [old.clone(), replacement.clone()];
        let matching =
            sampling_semantic_candidates(&pair, d32s8_fingerprint(), true, true).unwrap();
        let chosen = authoring_windows::select_boundary_candidate(&matching)
            .unwrap()
            .unwrap();
        assert_eq!(chosen.candidate_id, "dsv-r29-g1-e1");
        assert!(matches!(
            semantic_binding_action(
                &[chosen],
                true,
                true,
                Some(&old.candidate_id),
                Some(0),
                Some(0)
            ),
            SemanticBindingAction::Fault(_)
        ));
        let choose = |items: &[D3d12DepthCandidateObservation]| {
            let matching =
                sampling_semantic_candidates(items, d32s8_fingerprint(), true, true).unwrap();
            authoring_windows::select_boundary_candidate(&matching)
                .map(|c| c.map(|c| c.candidate_id.clone()))
        };
        // Replay the failed live boundary: both targets have valid historical proof,
        // neither has an executed clear now. Wait without selecting the old resource.
        let mut idle_replacement = old.clone();
        idle_replacement.candidate_id = replacement.candidate_id.clone();
        assert_eq!(choose(&[old.clone(), idle_replacement]), Ok(None));
        assert_eq!(choose(std::slice::from_ref(&old)), Ok(None));
        assert_eq!(
            choose(&[old, replacement.clone()]),
            Ok(Some(replacement.candidate_id.clone()))
        );
        let mut duplicate = replacement.clone();
        duplicate.candidate_id = "dsv-r30-g1-e1".into();
        assert!(choose(&[replacement.clone(), duplicate]).is_err());
        replacement.width += 1;
        assert_eq!(choose(&[replacement]), Ok(None));
    }

    #[test]
    fn authoring_menu_pause_keeps_binding_and_requires_unique_live_target_on_resume() {
        let mut scene = d32s8_candidate("scene");
        // Eight closed intervals plus the clear executed before this Present.
        scene.clear_calls += 1;
        scene.eligible_clear_calls += 1;
        scene.same_list_capture_eligible_clear_calls += 1;
        scene.execute_calls += 1;
        scene.clear_parameters[0].calls += 1;
        scene.raw_states_before_clear[0].calls += 1;
        scene.pending_clear_calls = 1;
        let mut menu = d32s8_candidate("menu");
        let decide = |candidates: &[D3d12DepthCandidateObservation], authoring, active| {
            sampling_semantic_candidates(candidates, d32s8_fingerprint(), authoring, active).map(
                |matching| {
                    semantic_binding_action(&matching, true, true, Some("scene"), Some(0), Some(0))
                },
            )
        };
        let targets = [scene.clone(), menu.clone()];
        assert_eq!(decide(&targets, true, false), None);
        // The change is scoped to discontinuous authoring, including when paused.
        assert!(matches!(
            decide(&targets, false, false),
            Some(SemanticBindingAction::Fault(_))
        ));
        assert!(matches!(
            decide(&targets, false, true),
            Some(SemanticBindingAction::Fault(_))
        ));
        assert_eq!(
            decide(&targets, true, true),
            Some(SemanticBindingAction::VerifyArmed)
        );

        menu = scene.clone();
        menu.candidate_id = "menu".into();
        assert!(matches!(
            decide(&[scene.clone(), menu.clone()], true, true),
            Some(SemanticBindingAction::Fault(_))
        ));
        // A menu/replacement target cannot take over the immutable armed identity.
        let stale_scene = d32s8_candidate("scene");
        assert!(matches!(
            decide(&[stale_scene.clone(), menu], true, true),
            Some(SemanticBindingAction::Fault(_))
        ));
        assert!(matches!(
            decide(&[stale_scene], true, true),
            Some(SemanticBindingAction::Fault(_))
        ));
        scene.current = false;
        assert!(matches!(
            decide(&[scene.clone()], true, true),
            Some(SemanticBindingAction::Fault(_))
        ));
        scene.current = true;
        scene.present_intervals_with_one_clear -= 1;
        scene.present_intervals_with_multiple_clears = 1;
        scene.max_clears_per_present_interval = 2;
        scene.clear_calls += 1;
        scene.eligible_clear_calls += 1;
        scene.same_list_capture_eligible_clear_calls += 1;
        scene.execute_calls += 1;
        scene.clear_parameters[0].calls += 1;
        scene.raw_states_before_clear[0].calls += 1;
        assert!(semantic_candidate_matches(&scene, d32s8_fingerprint()));
        assert!(matches!(
            decide(&[scene], true, true),
            Some(SemanticBindingAction::Fault(_))
        ));
    }

    #[test]
    fn arm_request_is_exact_launch_bound_and_capture_stays_off_without_it() {
        let launch_id = "0123456789abcdef0123456789abcdef";
        let request = RawDemoArmRequest {
            schema: RAW_DEMO_ARM_REQUEST_SCHEMA_V1.into(),
            process_launch_id: launch_id.into(),
            capture_contract: depth_lab::ISSUE735_LOCAL_CONTRACT.into(),
            duration_ms: depth_lab::ISSUE735_LOCAL_DURATION_MS,
            target_depth_hz: ISSUE735_TARGET_DEPTH_HZ,
            manual_stop: false,
            selected_present: None,
        };
        assert!(
            validate_arm_request(
                &request,
                launch_id,
                depth_lab::ISSUE735_LOCAL_CONTRACT,
                depth_lab::ISSUE735_LOCAL_DURATION_MS,
                false
            )
            .is_ok()
        );
        assert!(
            validate_arm_request(
                &request,
                "fedcba9876543210fedcba9876543210",
                depth_lab::ISSUE735_LOCAL_CONTRACT,
                depth_lab::ISSUE735_LOCAL_DURATION_MS,
                false
            )
            .is_err()
        );
        let wrong_cadence = RawDemoArmRequest {
            target_depth_hz: ISSUE735_TARGET_DEPTH_HZ + 1,
            ..request.clone()
        };
        assert!(
            validate_arm_request(
                &wrong_cadence,
                launch_id,
                depth_lab::ISSUE735_LOCAL_CONTRACT,
                depth_lab::ISSUE735_LOCAL_DURATION_MS,
                false
            )
            .is_err()
        );
        let pr405 = RawDemoArmRequest {
            capture_contract: depth_lab::PR405_LOCAL_CONTRACT.into(),
            duration_ms: depth_lab::PR405_LOCAL_DURATION_MS,
            target_depth_hz: PR405_TARGET_DEPTH_HZ,
            ..request
        };
        assert!(
            validate_arm_request(
                &pr405,
                launch_id,
                depth_lab::PR405_LOCAL_CONTRACT,
                depth_lab::PR405_LOCAL_DURATION_MS,
                false
            )
            .is_ok()
        );
        assert!(
            validate_arm_request(
                &pr405,
                launch_id,
                depth_lab::ISSUE735_LOCAL_CONTRACT,
                depth_lab::ISSUE735_LOCAL_DURATION_MS,
                false
            )
            .is_err()
        );
        assert!(!raw_capture_is_armed(false, true));
        assert!(!raw_capture_is_armed(true, false));
        assert!(raw_capture_is_armed(true, true));

        let stop = RawDemoStopRequest {
            schema: RAW_DEMO_STOP_REQUEST_SCHEMA_V1.into(),
            process_launch_id: launch_id.into(),
        };
        assert!(validate_stop_request(&stop, launch_id).is_ok());
        assert!(validate_stop_request(&stop, "fedcba9876543210fedcba9876543210").is_err());
    }

    #[test]
    fn trusted_output_requires_a_child_of_the_private_cp2077_root() {
        let child =
            Path::new(r"C:\Users\operator\AppData\Local\DataMiner\depth-raw-demo\cp2077\run1");
        let root = Path::new(r"c:\users\operator\appdata\local\DataMiner\depth-raw-demo\cp2077");
        assert!(path_is_within_case_insensitive(child, root));
        assert!(!path_is_within_case_insensitive(
            Path::new(r"C:\Games\Cyberpunk 2077\run1"),
            root
        ));
    }
}

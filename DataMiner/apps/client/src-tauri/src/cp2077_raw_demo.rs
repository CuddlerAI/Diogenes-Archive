//! Explicit local-only CP2077 raw-depth + RGB coordination.
//!
//! This module deliberately does not call the collector orchestrator. CP2077's
//! ordinary collector lane requires formal depth `StartRun -> Ready`, while this
//! experiment is permanently non-trainable and never makes that claim. The
//! coordinator reuses only the HWND-bound WGC recorder and its QPC frame index.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead as _, Read as _, Write as _};
use std::os::windows::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context as _, ensure};
use capture_core::input::{RAW_INPUT_QUEUE_CAPACITY, RawInputEvent, RawInputRecord};
use capture_core::profile::CaptureProfile;
use capture_core::qpc::QpcClock;
use capture_core::recording::{
    RecorderEvent, RecordingOutcome, RecordingRequest, RecordingStopReason, RecordingStopSignal,
    RecordingTarget, SegmentPolicyOverride, record_until_stopped,
};
use crossbeam_channel::bounded;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use windows::Win32::Foundation::{CloseHandle, FILETIME, HANDLE, HWND};
use windows::Win32::System::Diagnostics::Debug::MessageBeep;
use windows::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput, VIRTUAL_KEY, VK_S, VK_W,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GA_ROOTOWNER, GetAncestor, GetForegroundWindow, GetWindowThreadProcessId, MB_ICONASTERISK,
};

use crate::domain::collector::MarkerAudit;
use crate::platform::windows::window_tracker::{BoundTarget, TargetWindowMonitor, WindowTracker};

mod authoring_input;
pub use authoring_input::observe_authoring_input;
mod camera_alias;
mod camera_report;
mod offline_audit;
mod projection_validation;
mod selected_feedback;
mod window_diagnostic;
pub use window_diagnostic::observe_window;

const GAME_ID: &str = "cyberpunk-2077-steam";
const CP2077_EXE_SHA256: &str =
    "sha256:a7de82945c03e041fc7339fcf9066224d98db2f5d80fea50f7947bb350a60991";
const SETTINGS_VERSION: i64 = 140;
const HUD_COMPONENTS: [&str; 16] = [
    "minimap",
    "healthbar",
    "stamina_oxygen",
    "npc_healthbar",
    "ammo_counter",
    "input_hints",
    "action_buttons",
    "activity_log",
    "quest_tracker",
    "johnny_hud",
    "object_markers",
    "npc_names",
    "wanted_level",
    "npc_nameplates",
    "crouch_indicator",
    "vehicle_hud",
];
const CATALOG_SEGMENT_SECONDS: u32 = 10;
const CATALOG_TEST_SEGMENTS: u32 = 30;
const LOCAL_DEPTH_SEGMENT_SECONDS: u32 = 300;
const LOCAL_DEPTH_TEST_SEGMENTS: u32 = 1;
const PR405_TARGET_DEPTH_HZ: u64 = 15;
const ISSUE735_TARGET_DEPTH_HZ: u64 = 16;
/// Largest tolerated compositor lag, in multiples of the run's own measured
/// Present interval. See [`RgbDepthPairingWindow`].
const RGB_COMPOSITE_LAG_PRESENT_INTERVALS: i64 = 3;
const ARM_FILE: &str = "raw-depth-demo-arm-request.json";
const STOP_FILE: &str = "raw-depth-demo-stop-request.json";
const TELEMETRY_FILE: &str = "raw-depth-telemetry.json";
const ARM_SCHEMA: &str = "dm-cp2077-raw-depth-demo-arm-request/v1";
const STOP_SCHEMA: &str = "dm-cp2077-raw-depth-demo-stop-request/v1";
const TELEMETRY_SCHEMA: &str = "dm-cp2077-raw-depth-demo-telemetry/v1";
const INDEX_SCHEMA: &str = "dm-cp2077-raw-rgb-index/v1";
const RESULT_SCHEMA: &str = "dm-cp2077-local-raw-demo-result/v3";
// tools/pr735-cp2077/package-local-fivepiece.py recomputes this report field for
// field and refuses any drift; change both together.
const PAIRING_SCHEMA: &str = "dm-cp2077-local-raw-rgb-depth-pairing/v2";
const DEPTH_QC_FILE: &str = "raw-depth-qc.json";
const PAIRING_FILE: &str = "raw-rgb-depth-pairing.json";
const WATERMARK_SAMPLES_FILE: &str = "watermark-samples.jsonl";
const POSE_REPORT_FILE: &str = "cp2077-pose-runtime-observation.json";
const RENDER_CAMERA_REPORT_FILE: &str = "render-camera-observation.json";
const ACTION_PLAN_FILE: &str = "operator-action-plan.json";
const ACTION_MARKERS_FILE: &str = "operator-action-markers.jsonl";
const POSE_CAMERA_LOOP_QC_FILE: &str = "pose-camera-loop-qc.json";
const PR727_SETTINGS_EVIDENCE_FILE: &str = "pr727-settings-runtime-evidence.json";
const METRIC_SETTINGS_EVIDENCE_FILE: &str = "metric-calibration-settings-runtime-evidence.json";
const METRIC_POSE_FILE: &str = "metric-pose.jsonl";
const METRIC_FRAME_FILE: &str = "metric-frame-evidence.jsonl";
const METRIC_OBSERVATIONS_TEMPLATE_FILE: &str = "hud-metric-observations.review-template.json";
const METRIC_CAPTURE_REPORT_FILE: &str = "metric-calibration-capture.json";
const METRIC_PRESENT_IDENTITY_MAX_QPC_DELTA: u64 = 1_000_000;
const MAIN_COLLECTOR_RUN_FILE: &str = "main-collector-run.json";
const MAIN_COLLECTOR_FLUSH_FILE: &str = "main-collector-confirmed-flush.json";
const AUTONOMOUS_INPUT_LEDGER_FILE: &str = "autonomous-input-ledger.jsonl";
const AUTONOMOUS_INPUT_LEDGER_SCHEMA: &str = "dm-cp2077-authorized-autonomous-input/v1";
const AUTONOMOUS_INPUT_PROVENANCE: &str = "authorized_autonomous_sendinput";
const MANUAL_INPUT_PROVENANCE: &str = "hardware_raw_input";
const PR405_RESOLVED_AXIS_MAPPING: &str = "motion_calibrated_redengine_to_delivery_v1";
const SETTINGS_ATTESTATION_PREFIX: &str = "UserSettings.issue735-attestation-";
const SETTINGS_SNAPSHOT_PREFIX: &str = "UserSettings.issue735-applied-";
const SETTINGS_ATTESTATION_SCHEMA: &str = "dm-cp2077-capture-settings-attestation/v1";
const SETTINGS_EVIDENCE_SCHEMA: &str = "dm-cp2077-pr727-settings-runtime-evidence/v1";
const METRIC_SETTINGS_ATTESTATION_PREFIX: &str = "UserSettings.metric-calibration-attestation-";
const METRIC_SETTINGS_SNAPSHOT_PREFIX: &str = "UserSettings.metric-calibration-applied-";
const METRIC_SETTINGS_ATTESTATION_SCHEMA: &str =
    "dm-cp2077-metric-calibration-settings-attestation/v1";
const METRIC_SETTINGS_EVIDENCE_SCHEMA: &str =
    "dm-cp2077-metric-calibration-settings-runtime-evidence/v1";
const MAX_PRELAUNCH_SETTINGS_LEAD_100NS: u64 = 5 * 60 * 10_000_000;
const ACTION_PLAN_SCHEMA: &str = "dm-cp2077-raw-demo-action-plan/v2";
const ACTION_MARKER_SCHEMA: &str = "dm-cp2077-raw-demo-action-marker/v1";
const POSE_CAMERA_LOOP_QC_SCHEMA: &str = "dm-cp2077-raw-pose-camera-loop-qc/v4";
const POSE_REPORT_SCHEMA: &str = "cp2077-pose-runtime-observation/v1";
const RENDER_CAMERA_REPORT_SCHEMA: &str = "dm-cp2077-render-camera-observation/v2";
const MIN_EXACT_DEPTH_RGB_COVERAGE: f64 = 0.98;
const MIN_PLAYER_POSE_SAMPLES: usize = 120;
const MIN_RENDER_CAMERA_PRESENTS: usize = 120;
const MIN_RENDER_SOURCE_COVERAGE: f64 = 0.90;
const MIN_PLAYER_DISPLACEMENT_WORLD_UNITS: f64 = 2.0;
const MAX_TERMINAL_CLOSURE_WORLD_UNITS: f64 = 0.60;
const MAX_RENDER_VIEW_TERMINAL_DELTA: f64 = 1.0;
const ACTION_CUE_TOLERANCE_MS: u64 = 3_000;
// Human-counted fixed-view 64-second walks allow a few seconds of reaction drift.
const FIXED_VIEW_CUE_TOLERANCE_MS: u64 = 5_000;
const FIXED_VIEW_INPUT_POLICY: &str = "fixed_view_ws/v3";
const MAX_ACTION_CUE_DELAY_MS: u64 = 250;
const MAX_JSONL_BYTES: u64 = 64 * 1024 * 1024;
const MAX_JSONL_ROWS: usize = 100_000;
const CLIENT_CLICK_SETTLE_DELAY: Duration = Duration::from_millis(750);
const FOREGROUND_STABILITY_WINDOW: Duration = Duration::from_millis(500);
const FOREGROUND_STABILITY_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MainCollectorPurpose {
    #[default]
    FiveStream,
    MetricCalibration,
}

impl MainCollectorPurpose {
    const fn as_str(self) -> &'static str {
        match self {
            Self::FiveStream => "five_stream",
            Self::MetricCalibration => "metric_calibration",
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct OperatorActionStep {
    pub marker_id: &'static str,
    pub offset_ms: u64,
    pub phase: &'static str,
    pub instruction: &'static str,
    pub expected_input: &'static str,
}

pub const ISSUE735_OPERATOR_ACTION_STEPS: [OperatorActionStep; 9] = [
    OperatorActionStep {
        marker_id: "settle-start",
        offset_ms: 0,
        phase: "stationary",
        instruction: "Do not move; keep the view fixed.",
        expected_input: "none",
    },
    OperatorActionStep {
        marker_id: "outbound-start",
        offset_ms: 5_000,
        phase: "outbound",
        instruction: "Hold W and walk straight forward; do not sprint or turn.",
        expected_input: "key_w_down",
    },
    OperatorActionStep {
        marker_id: "outbound-stop",
        offset_ms: 23_000,
        phase: "outbound-settle",
        instruction: "Release W and remain still.",
        expected_input: "key_w_up",
    },
    OperatorActionStep {
        marker_id: "return-start",
        offset_ms: 29_000,
        phase: "return",
        instruction: "Hold S and walk straight backward along the same path.",
        expected_input: "key_s_down",
    },
    OperatorActionStep {
        marker_id: "return-stop",
        offset_ms: 47_000,
        phase: "return-settle",
        instruction: "Release S and remain still.",
        expected_input: "key_s_up",
    },
    OperatorActionStep {
        marker_id: "correction-start",
        offset_ms: 49_000,
        phase: "closure-correction",
        instruction: "Use brief W or S taps only to return to the visible start point.",
        expected_input: "optional_key_w_or_s",
    },
    OperatorActionStep {
        marker_id: "correction-stop",
        offset_ms: 57_000,
        phase: "closure-correction-stop",
        instruction: "Release all movement keys.",
        expected_input: "movement_keys_up",
    },
    OperatorActionStep {
        marker_id: "terminal-settle",
        offset_ms: 58_000,
        phase: "terminal-loop-return",
        instruction: "Remain still at the start point until capture ends.",
        expected_input: "none",
    },
    OperatorActionStep {
        marker_id: "capture-end",
        offset_ms: 64_000,
        phase: "complete",
        instruction: "Capture stops automatically.",
        expected_input: "none",
    },
];

pub const PR405_OPERATOR_ACTION_STEPS: [OperatorActionStep; 9] = [
    OperatorActionStep {
        marker_id: "settle-start",
        offset_ms: 0,
        phase: "stationary",
        instruction: "Do not move; keep the view fixed.",
        expected_input: "none",
    },
    OperatorActionStep {
        marker_id: "outbound-start",
        offset_ms: 2_000,
        phase: "outbound",
        instruction: "Hold W and walk straight forward; do not sprint or turn.",
        expected_input: "key_w_down",
    },
    OperatorActionStep {
        marker_id: "outbound-stop",
        offset_ms: 11_000,
        phase: "outbound-settle",
        instruction: "Release W and remain still.",
        expected_input: "key_w_up",
    },
    OperatorActionStep {
        marker_id: "return-start",
        offset_ms: 14_000,
        phase: "return",
        instruction: "Hold S and walk straight backward along the same path.",
        expected_input: "key_s_down",
    },
    OperatorActionStep {
        marker_id: "return-stop",
        offset_ms: 23_000,
        phase: "return-settle",
        instruction: "Release S and remain still.",
        expected_input: "key_s_up",
    },
    OperatorActionStep {
        marker_id: "correction-start",
        offset_ms: 26_250,
        phase: "closure-correction",
        instruction: "Use a brief W correction to return to the measured start point.",
        expected_input: "key_w_down",
    },
    OperatorActionStep {
        marker_id: "correction-stop",
        offset_ms: 27_000,
        phase: "closure-correction-stop",
        instruction: "Release all movement keys.",
        expected_input: "movement_keys_up",
    },
    OperatorActionStep {
        marker_id: "terminal-settle",
        offset_ms: 28_000,
        phase: "terminal-loop-return",
        instruction: "Remain still at the start point until capture ends.",
        expected_input: "none",
    },
    OperatorActionStep {
        marker_id: "capture-end",
        offset_ms: 30_000,
        phase: "complete",
        instruction: "Capture stops automatically.",
        expected_input: "none",
    },
];

const MANUAL_STOP_OPERATOR_ACTION_STEPS: [OperatorActionStep; 0] = [];

#[derive(Clone, Copy, Debug)]
struct CaptureContract {
    name: &'static str,
    duration_ms: u64,
    min_duration_ms: u64,
    max_duration_ms: u64,
    min_depth_frames: u64,
    max_depth_frames: u64,
    min_rgb_frames: u64,
    max_rgb_frames: u64,
    target_depth_hz: u64,
    min_input_hold_ms: u64,
    max_input_hold_ms: u64,
    steps: &'static [OperatorActionStep],
}

impl CaptureContract {
    fn fixed_view_walk(self) -> bool {
        self.name == depth_lab::ISSUE735_LOCAL_CONTRACT && !self.steps.is_empty()
    }
}

fn standalone_requires_mouse_movement(contract: CaptureContract, manual_stop: bool) -> bool {
    // Only the explicitly timed 64-second fixed-view diagnostic excludes mouse
    // movement. PR405 and main-collector five-stream requirements remain strict.
    manual_stop || !contract.fixed_view_walk()
}

fn input_policy_name(require_mouse_movement: bool, manual_stop: bool) -> &'static str {
    if require_mouse_movement {
        "keyboard_and_mouse/v1"
    } else if manual_stop {
        "keyboard_activity/v1"
    } else {
        FIXED_VIEW_INPUT_POLICY
    }
}

fn input_cue_tolerance_ms(require_mouse_movement: bool, manual_stop: bool) -> u64 {
    if !require_mouse_movement && !manual_stop {
        FIXED_VIEW_CUE_TOLERANCE_MS
    } else {
        ACTION_CUE_TOLERANCE_MS
    }
}

fn chinese_action_instruction(marker_id: &str) -> &'static str {
    match marker_id {
        "settle-start" => "采集开始，保持不动，别动鼠标。",
        "outbound-start" => "按住 W 不松手，直走；不要按 Shift。",
        "outbound-stop" => "松开 W，站住。",
        "return-start" => "按住 S 不松手，原方向倒退，不转身。",
        "return-stop" => "松开 S，站住。",
        "correction-start" => "仅用 W 或 S 短按微调回起点；已到起点就不动。",
        "correction-stop" => "松开所有键。",
        "terminal-settle" => "保持不动，别动鼠标。",
        "capture-end" => "采集结束，松开所有键，等待检查。",
        _ => "未知动作，请停止并检查动作计划。",
    }
}

/// Preview only: no game binding, capture, input or session writes.
fn chinese_action_cue(marker_id: &str) -> &'static str {
    match marker_id {
        "settle-start" => "开始。",
        "outbound-start" => "前进。",
        "outbound-stop" | "return-stop" | "correction-stop" => "松手。",
        "return-start" => "后退。",
        "correction-start" => "微调。",
        "terminal-settle" => "别动。",
        "capture-end" => "结束。",
        _ => "停止。",
    }
}

pub fn fixed_view_action_plan_preview() -> Value {
    json!({
        "schema": "dm-cp2077-guided-preview/v2",
        "speech_protocol": "short_cue_sapi_completion/v1",
        "action_plan_schema": ACTION_PLAN_SCHEMA,
        "input_policy": FIXED_VIEW_INPUT_POLICY,
        "action_cue_tolerance_ms": FIXED_VIEW_CUE_TOLERANCE_MS,
        "contract": depth_lab::ISSUE735_LOCAL_CONTRACT,
        "duration_ms": depth_lab::ISSUE735_LOCAL_DURATION_MS,
        "steps": ISSUE735_OPERATOR_ACTION_STEPS.iter().map(|step| json!({
            "marker_id": step.marker_id, "offset_ms": step.offset_ms,
            "instruction": step.instruction,
            "instruction_zh": chinese_action_instruction(step.marker_id),
            "speech_zh": chinese_action_cue(step.marker_id),
            "expected_input": step.expected_input,
        })).collect::<Vec<_>>(),
        "non_trainable": true, "recording_ready": false
    })
}

pub use offline_audit::audit_existing_session;

fn capture_contract(name: &str, duration_ms: u64) -> anyhow::Result<CaptureContract> {
    match (name, duration_ms) {
        (depth_lab::ISSUE735_LOCAL_CONTRACT, depth_lab::ISSUE735_LOCAL_DURATION_MS) => {
            Ok(CaptureContract {
                name: depth_lab::ISSUE735_LOCAL_CONTRACT,
                duration_ms,
                min_duration_ms: 63_000,
                max_duration_ms: 65_000,
                min_depth_frames: 900,
                max_depth_frames: 1_024,
                min_rgb_frames: 1_890,
                max_rgb_frames: 1_950,
                target_depth_hz: ISSUE735_TARGET_DEPTH_HZ,
                min_input_hold_ms: 12_000,
                max_input_hold_ms: 24_000,
                steps: &ISSUE735_OPERATOR_ACTION_STEPS,
            })
        }
        (depth_lab::PR405_LOCAL_CONTRACT, depth_lab::PR405_LOCAL_DURATION_MS) => {
            Ok(CaptureContract {
                name: depth_lab::PR405_LOCAL_CONTRACT,
                duration_ms,
                min_duration_ms: 29_000,
                max_duration_ms: 31_000,
                // The probe retains every game Present.  After RGB closes the
                // coordinator atomically publishes the exact 450 even-CFR
                // subset under the canonical raw-depth index/directory.
                min_depth_frames: 750,
                max_depth_frames: 1_024,
                min_rgb_frames: 870,
                max_rgb_frames: 930,
                target_depth_hz: PR405_TARGET_DEPTH_HZ,
                min_input_hold_ms: 6_000,
                max_input_hold_ms: 12_000,
                steps: &PR405_OPERATOR_ACTION_STEPS,
            })
        }
        _ => anyhow::bail!("unsupported raw demo capture contract/duration"),
    }
}

#[derive(Clone, Copy, Debug)]
struct ActualActionCue {
    step_index: usize,
    scheduled_qpc: i64,
    actual_qpc: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AutonomousInputLedgerRow {
    schema: String,
    process_launch_id: String,
    input_provenance: String,
    marker_id: String,
    scheduled_qpc: i64,
    attempt_qpc: i64,
    actual_qpc: i64,
    qpc_frequency: i64,
    target_process_id: u32,
    bound_hwnd_process_matches: bool,
    foreground_root_matches: bool,
    foreground_process_matches: bool,
    vk: u16,
    action: String,
    send_input_count: u32,
    non_trainable: bool,
    recording_ready: bool,
}

#[derive(Clone, Copy)]
struct AutonomousInputTarget {
    hwnd_value: usize,
    root_owner_value: usize,
    process_id: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AutonomousKeyAction {
    Down,
    Up,
}

#[derive(Default)]
struct HeldAutonomousKeys(std::collections::BTreeSet<u16>);

impl HeldAutonomousKeys {
    fn contains(&self, vk: u16) -> bool {
        self.0.contains(&vk)
    }

    fn insert(&mut self, vk: u16) {
        self.0.insert(vk);
    }

    fn remove(&mut self, vk: u16) {
        self.0.remove(&vk);
    }

    fn release_all(&mut self) {
        for vk in std::mem::take(&mut self.0) {
            send_autonomous_key_up_unchecked(vk);
        }
    }
}

impl Drop for HeldAutonomousKeys {
    fn drop(&mut self) {
        self.release_all();
    }
}

impl AutonomousKeyAction {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Down => "key_down",
            Self::Up => "key_up",
        }
    }
}

pub fn operator_action_plan_text(contract: &str, duration_ms: u64) -> anyhow::Result<String> {
    let contract = capture_contract(contract, duration_ms)?;
    let mut text = String::from(
        "Audited out-and-back route (choose a clear, flat, straight path; no sprint/ADS/phone/scanner/vehicle):\n",
    );
    if contract.fixed_view_walk() {
        text.push_str("固定视角徒步：只按 W/S；按住直到松开提示，禁止 Shift、转头和切出游戏。\n先试听语音、读完时间表；聊天转述可能延迟，不作为秒级操作时钟。\n");
    }
    for step in contract.steps {
        use std::fmt::Write as _;
        let _ = writeln!(
            text,
            "  {:>2}.{:03}s  {}",
            step.offset_ms / 1_000,
            step.offset_ms % 1_000,
            step.instruction
        );
        if contract.fixed_view_walk() {
            let _ = writeln!(
                text,
                "           {}",
                chinese_action_instruction(step.marker_id)
            );
        }
    }
    Ok(text)
}

pub struct RawDemoCoordinatorRequest {
    pub pid: u32,
    pub process_launch_id: String,
    pub session_dir: PathBuf,
    pub capture_contract: String,
    pub duration_ms: u64,
    pub manual_stop: bool,
    pub autonomous_input: bool,
    /// Test seam. Production CLI always resolves the live PR #727 settings.
    pub settings_path: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
pub struct RawDemoCoordinatorResult {
    pub schema: &'static str,
    pub process_launch_id: String,
    pub process_id: u32,
    pub capture_contract: String,
    pub duration_ms: u64,
    pub video_frame_count: u64,
    pub depth_frame_count: u64,
    pub video_sha256: String,
    pub video: String,
    pub rgb_index: String,
    pub depth_index: String,
    pub depth_telemetry: String,
    pub depth_qc: String,
    pub rgb_depth_pairing: String,
    pub pose_report: String,
    pub render_camera_report: String,
    pub operator_action_plan: String,
    pub operator_action_markers: String,
    pub raw_input_events: String,
    pub raw_input_meta: String,
    pub raw_input_manifest: String,
    pub pose_camera_loop_qc: String,
    pub pr727_settings_runtime_evidence: String,
    pub input_provenance: &'static str,
    pub autonomous_input_ledger: Option<String>,
    pub pr405_frame_evidence: Option<String>,
    pub pr405_final_pose: Option<String>,
    pub pr405_frame_evidence_rows: Option<usize>,
    pub pr405_final_pose_rows: Option<usize>,
    pub non_trainable: bool,
    pub production_certified: bool,
    pub upload_allowed: bool,
    pub deliverable: bool,
    pub recording_ready: bool,
    pub formal_start_run_used: bool,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct MainCollectorDepthSummary {
    pub rgb_frames: u64,
    pub source_depth_frames: u64,
    pub paired_depth_frames: u64,
    pub rgb_duration_ms: u64,
    pub depth_duration_ms: u64,
    pub exact_present_matches: usize,
    pub exact_present_pairing_rate: f64,
    pub raw_input_events: usize,
    pub keyboard_input_events: usize,
    pub mouse_input_events: usize,
    pub player_pose_samples: usize,
    pub render_camera_samples: usize,
    pub capture_queue_drops: u64,
    pub input_queue_overflows: u64,
    pub metric_calibration_samples: Option<usize>,
    pub rgb_segment_directory: PathBuf,
}

fn manual_main_collector_contract() -> anyhow::Result<CaptureContract> {
    let mut contract = capture_contract(
        depth_lab::ISSUE735_LOCAL_CONTRACT,
        depth_lab::ISSUE735_LOCAL_DURATION_MS,
    )?;
    contract.min_duration_ms = 60_000;
    contract.max_duration_ms = 240_000;
    contract.min_depth_frames = 900;
    contract.max_depth_frames = 4_096;
    contract.min_rgb_frames = 1_800;
    contract.max_rgb_frames = 7_200;
    contract.min_input_hold_ms = 0;
    contract.max_input_hold_ms = 240_000;
    contract.steps = &MANUAL_STOP_OPERATOR_ACTION_STEPS;
    Ok(contract)
}

/// Prepare the evidence owned by the CP2077 sidecar without starting a second
/// WGC recorder or Raw Input monitor. The main collector remains the sole
/// owner of RGB/input and of the session/run journal.
pub(crate) fn prepare_main_collector_sidecar(
    session_dir: &Path,
    pid: u32,
    process_launch_id: &str,
    purpose: MainCollectorPurpose,
) -> anyhow::Result<()> {
    validate_launch_id(process_launch_id)?;
    ensure!(pid != 0, "CP2077 PID must be non-zero");
    let session_dir = validate_session_dir(session_dir)?;
    let contract = manual_main_collector_contract()?;
    validate_raw_config(&session_dir, contract)?;
    let (settings, settings_file) = match purpose {
        MainCollectorPurpose::FiveStream => (
            audit_pr727_settings_for_process(&default_settings_path(), pid, process_launch_id)?,
            PR727_SETTINGS_EVIDENCE_FILE,
        ),
        MainCollectorPurpose::MetricCalibration => (
            audit_metric_calibration_settings_for_process(
                &default_settings_path(),
                pid,
                process_launch_id,
            )?,
            METRIC_SETTINGS_EVIDENCE_FILE,
        ),
    };
    write_json_create_new(&session_dir.join(settings_file), &settings)?;
    if purpose == MainCollectorPurpose::MetricCalibration {
        write_json_create_new(
            &session_dir.join(ACTION_PLAN_FILE),
            &json!({
                "schema": ACTION_PLAN_SCHEMA,
                "process_launch_id": process_launch_id,
                "duration_ms": contract.duration_ms,
                "route": "straight_radial_same_objective",
                "route_constraints": [
                    "first-person on-foot only",
                    "walk one straight radial line toward or away from one unchanged objective",
                    "press Ctrl+Alt+F9 at 3 to 8 clearly readable whole-metre HUD distances",
                    "cover at least 20 HUD metres and do not sprint, scan, enter a vehicle, dialogue, or menu"
                ],
                "sample_hotkey": "Ctrl+Alt+F9",
                "hud_visible_for_calibration_only": true,
                "input_provenance": MANUAL_INPUT_PROVENANCE,
                "non_trainable": true,
                "recording_ready": false
            }),
        )?;
    } else {
        write_action_plan(&session_dir, process_launch_id, contract, false, true)?;
    }
    write_json_create_new(
        &session_dir.join("operator-launch-local.json"),
        &serde_json::json!({
            "schema": "dm-cp2077-main-collector-launch/v1",
            "launch_id": process_launch_id,
            "process_id": pid,
            "capture_owner": "dataminer_main_collector",
            "purpose": purpose.as_str(),
            "formal_start_run": false,
            "non_trainable": true,
            "recording_ready": false
        }),
    )?;
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(session_dir.join(ACTION_MARKERS_FILE))?
        .sync_all()?;
    Ok(())
}

/// Arm raw depth only after the main collector has durably committed its
/// session/run start. This request never starts WGC or an input monitor.
pub(crate) fn arm_main_collector_sidecar(
    session_dir: &Path,
    process_launch_id: &str,
    process_id: u32,
    session_id: &str,
    run_id: &str,
    generation: u64,
    purpose: MainCollectorPurpose,
) -> anyhow::Result<()> {
    ensure!(process_id != 0, "CP2077 PID must be non-zero");
    ensure!(
        !session_id.trim().is_empty() && !run_id.trim().is_empty(),
        "main collector session/run identity is empty"
    );
    let contract = manual_main_collector_contract()?;
    write_arm_request(session_dir, process_launch_id, contract, true)?;
    write_json_create_new(
        &session_dir.join(MAIN_COLLECTOR_RUN_FILE),
        &json!({
            "schema": "dm-cp2077-main-collector-run/v1",
            "session_id": session_id,
            "run_id": run_id,
            "generation": generation,
            "process_id": process_id,
            "process_launch_id": process_launch_id,
            "collector_start_session": true,
            "journal_committed_before_arm": true,
            "main_collector_started": true,
            "main_collector_recording_ready": false,
            "sidecar_probe_recording_ready": false,
            "main_collector_formal_start_run": true,
            "sidecar_probe_formal_start_run": false,
            "capture_owner": "dataminer_main_collector",
            "purpose": purpose.as_str(),
            "non_trainable": true,
            "upload_allowed": false
        }),
    )
}

pub(crate) fn local_depth_trial_profile(
    catalog_profile: &CaptureProfile,
) -> anyhow::Result<(CaptureProfile, SegmentPolicyOverride)> {
    ensure!(
        catalog_profile.profile_id == "cyberpunk-2077-steam-recording-v1"
            && catalog_profile.segment_seconds == CATALOG_SEGMENT_SECONDS
            && catalog_profile.test_segments == CATALOG_TEST_SEGMENTS,
        "CP2077 local Depth trial requires the canonical 10x30 RGB profile baseline"
    );
    let mut effective = catalog_profile.clone();
    effective.segment_seconds = LOCAL_DEPTH_SEGMENT_SECONDS;
    effective.test_segments = LOCAL_DEPTH_TEST_SEGMENTS;
    effective.validate()?;
    Ok((
        effective,
        SegmentPolicyOverride {
            reason: "cp2077_local_depth_trial_single_video".into(),
            catalog_segment_seconds: CATALOG_SEGMENT_SECONDS,
            catalog_test_segments: CATALOG_TEST_SEGMENTS,
            effective_segment_seconds: LOCAL_DEPTH_SEGMENT_SECONDS,
            effective_test_segments: LOCAL_DEPTH_TEST_SEGMENTS,
        },
    ))
}

pub(crate) fn stop_main_collector_sidecar(
    session_dir: &Path,
    process_launch_id: &str,
) -> anyhow::Result<()> {
    write_stop_request_if_absent(session_dir, process_launch_id)
}

pub(crate) fn play_main_collector_started_beep() {
    unsafe {
        let _ = MessageBeep(MB_ICONASTERISK);
    }
}

/// Seal the probe-owned streams against the exact RGB segment produced by the
/// main collector. Files are hard-linked on the same private volume: no second
/// capture and no lossy transcode is introduced.
#[allow(clippy::too_many_arguments)]
pub(crate) fn finalize_main_collector_sidecar(
    session_dir: &Path,
    process_launch_id: &str,
    process_id: u32,
    session_id: &str,
    run_id: &str,
    generation: u64,
    outcome: &RecordingOutcome,
    purpose: MainCollectorPurpose,
    markers: &[MarkerAudit],
) -> anyhow::Result<MainCollectorDepthSummary> {
    validate_launch_id(process_launch_id)?;
    ensure!(process_id != 0, "CP2077 PID must be non-zero");
    ensure!(
        outcome.session_id == session_id && outcome.run_id == run_id,
        "main collector outcome belongs to another session/run"
    );
    ensure!(
        outcome.non_trainable == Some(true) && outcome.production_certified == Some(false),
        "CP2077 DEV outcome weakened its non-trainable ceiling"
    );
    ensure!(
        outcome.segment_count == 1,
        "CP2077 main collector requires exactly one RGB segment"
    );
    let session_dir = validate_session_dir(session_dir)?;
    let contract = manual_main_collector_contract()?;
    let qpc_start = outcome.qpc_start.context("RGB outcome has no start QPC")?;
    let qpc_end = outcome.qpc_end.context("RGB outcome has no end QPC")?;
    let rgb_duration_ms = duration_ms(qpc_start, qpc_end, outcome.qpc_frequency)?;
    ensure_duration("RGB QPC", rgb_duration_ms, contract)?;
    ensure!(
        (contract.min_rgb_frames..=contract.max_rgb_frames).contains(&outcome.frame_count),
        "RGB frame count is outside the manual main-collector contract"
    );

    let rgb_segment_directory = link_main_rgb_segment(&session_dir, outcome)?;
    let source_video = rgb_segment_directory.join("video.mp4");
    let source_frames = rgb_segment_directory.join("frames.jsonl.zst");
    let source_events = rgb_segment_directory.join("events.jsonl.zst");
    let source_meta = rgb_segment_directory.join("meta.json");
    let source_manifest = rgb_segment_directory.join("sha256.json");
    let published_video = session_dir.join("video.mp4");
    fs::hard_link(&source_video, &published_video)
        .context("failed to publish the main collector video into the CP2077 sidecar")?;
    extract_watermark_samples(&session_dir, &published_video)?;

    let telemetry_path = wait_for_file(&session_dir.join(TELEMETRY_FILE), Duration::from_secs(15))?;
    let telemetry =
        validate_depth_telemetry(&fs::read(&telemetry_path)?, process_launch_id, contract)?;
    let source_depth_frames = required_u64(&telemetry, "written_frames")?;
    let depth_duration_ms = required_u64(&telemetry, "elapsed_ms")?;
    ensure_duration("raw depth QPC", depth_duration_ms, contract)?;
    ensure!(
        depth_duration_ms.abs_diff(rgb_duration_ms) <= 2_000,
        "RGB/depth capture windows differ by more than two seconds"
    );
    ensure!(
        (900..=4_096).contains(&source_depth_frames),
        "raw depth frame count is outside the manual main-collector contract"
    );

    let paired_depth_frames = materialize_issue735_exact_depth(
        &session_dir,
        &source_frames,
        outcome.qpc_frequency,
        contract,
    )?;
    let qc = depth_lab::audit_cp2077_raw_depth_session(
        &session_dir,
        depth_lab::RawDepthQcContract::ManualStopDemo,
    )
    .map_err(anyhow::Error::msg)?;
    ensure!(
        qc.passed,
        "raw D32 pixel/QPC QC failed: {}",
        qc.blockers.join(", ")
    );
    write_json_create_new(&session_dir.join(DEPTH_QC_FILE), &qc)?;

    let pairing = validate_rgb_depth_pairing(&session_dir, &source_frames, outcome.qpc_frequency)?;
    ensure!(
        pairing.passed,
        "raw RGB/depth exact-Present pairing failed: {}",
        pairing.blockers.join(", ")
    );
    write_json_create_new(&session_dir.join(PAIRING_FILE), &pairing)?;

    let pose_report_path =
        wait_for_file(&session_dir.join(POSE_REPORT_FILE), Duration::from_secs(15))?;
    let (pose_camera_loop_qc, input_metrics) = if purpose == MainCollectorPurpose::FiveStream {
        let render_camera_report_path = wait_for_file(
            &session_dir.join(RENDER_CAMERA_REPORT_FILE),
            Duration::from_secs(15),
        )?;
        let qc = audit_pose_camera_loop(
            &session_dir,
            process_launch_id,
            qpc_start,
            qpc_end,
            outcome.qpc_frequency,
            outcome.frame_count,
            outcome.input_event_count,
            outcome.input_queue_overflows,
            false,
            true,
            true,
            process_id,
            &[],
            contract,
            &pose_report_path,
            &render_camera_report_path,
            &source_events,
            &source_meta,
            &source_manifest,
        )?;
        ensure!(
            qc.passed,
            "raw input/player pose/render camera evidence did not pass"
        );
        write_json_create_new(&session_dir.join(POSE_CAMERA_LOOP_QC_FILE), &qc)?;
        let input = RawInputMetrics {
            evidence_event_count: qc.input_event_count,
            raw_event_count: qc.raw_input_event_count,
            outbound_hold_ms: qc.outbound_hold_ms,
            return_hold_ms: qc.return_hold_ms,
            mouse_move_events: qc.mouse_move_events,
            keyboard_events: qc.keyboard_events,
        };
        (Some(qc), input)
    } else {
        // Metric authoring needs healthy main-collector input and an exact player pose for each
        // selected Present. It intentionally does not require the five-stream render-camera loop.
        let input = audit_raw_input(
            &source_events,
            &source_meta,
            &source_manifest,
            qpc_start,
            qpc_end,
            outcome.qpc_frequency,
            outcome.frame_count,
            outcome.input_event_count,
            outcome.input_queue_overflows,
            &[],
            contract,
            true,
            false,
        )?;
        (None, input)
    };

    let video_sha256 = sha256_file(&published_video)?;
    write_json_create_new(
        &session_dir.join("rgb-index.json"),
        &json!({
            "schema": INDEX_SCHEMA,
            "session_id": session_id,
            "run_id": run_id,
            "generation": generation,
            "process_launch_id": process_launch_id,
            "process_id": process_id,
            "video": "video.mp4",
            "video_sha256": video_sha256,
            "frames_index": relative_path(&session_dir, &source_frames)?,
            "frame_count": outcome.frame_count,
            "qpc_frequency": outcome.qpc_frequency,
            "qpc_start": qpc_start,
            "qpc_end": qpc_end,
            "duration_ms": rgb_duration_ms,
            "capture_owner": "dataminer_main_collector",
            "main_collector_formal_start_run": true,
            "sidecar_probe_formal_start_run": false,
            "non_trainable": true,
            "production_certified": false,
            "upload_allowed": false
        }),
    )?;

    let meta: Value = serde_json::from_slice(&read_bounded_file(&source_meta, 2 * 1024 * 1024)?)?;
    let capture_queue_drops = meta
        .pointer("/health/queue_drops")
        .and_then(Value::as_u64)
        .unwrap_or(u64::MAX);
    ensure!(
        capture_queue_drops == 0 && outcome.input_queue_overflows == 0,
        "main collector reported capture/input drops"
    );
    ensure!(
        session_dir.join(STOP_FILE).is_file(),
        "CP2077 sidecar stop request is missing"
    );
    if purpose == MainCollectorPurpose::MetricCalibration {
        append_metric_action_markers(&session_dir, process_launch_id, markers)?;
    }
    let metric_calibration_samples = if purpose == MainCollectorPurpose::MetricCalibration {
        Some(author_metric_calibration_capture(
            &session_dir,
            &source_frames,
            &published_video,
            &pose_report_path,
            process_launch_id,
            session_id,
            run_id,
            outcome.qpc_frequency,
            markers,
        )?)
    } else {
        None
    };
    let player_pose_samples = pose_camera_loop_qc.as_ref().map_or_else(
        || metric_calibration_samples.unwrap_or(0),
        |qc| qc.player_pose_samples,
    );
    let render_camera_samples = pose_camera_loop_qc
        .as_ref()
        .map_or(0, |qc| qc.render_matched_depth_presents);

    write_json_create_new(
        &session_dir.join(MAIN_COLLECTOR_FLUSH_FILE),
        &json!({
            "schema": "dm-cp2077-main-collector-confirmed-flush/v1",
            "session_id": session_id,
            "run_id": run_id,
            "generation": generation,
            "process_id": process_id,
            "process_launch_id": process_launch_id,
            "qpc_start": qpc_start,
            "qpc_end": qpc_end,
            "qpc_frequency": outcome.qpc_frequency,
            "rgb_frames": outcome.frame_count,
            "source_depth_frames": source_depth_frames,
            "paired_depth_frames": paired_depth_frames,
            "depth_submitted_frames": required_u64(&telemetry, "submitted_frames")?,
            "depth_written_frames": source_depth_frames,
            "depth_drops": {
                "writer_queue": required_u64(&telemetry, "writer_queue_dropped")?,
                "present_stamp": required_u64(&telemetry, "present_stamp_dropped")?,
                "capture_pool": required_u64(&telemetry, "capture_pool_dropped")?
            },
            "capture_queue_drops": capture_queue_drops,
            "input_queue_overflows": outcome.input_queue_overflows,
            "raw_input_events": input_metrics.raw_event_count,
            "keyboard_input_events": input_metrics.keyboard_events,
            "mouse_input_events": input_metrics.mouse_move_events,
            "exact_present_pairing_rate": pairing.exact_depth_rgb_coverage,
            "player_pose_samples": player_pose_samples,
            "render_camera_samples": render_camera_samples,
            "confirmed_flush": true,
            "main_collector_formal_start_run": true,
            "sidecar_probe_formal_start_run": false,
            "non_trainable": true,
            "upload_allowed": false
        }),
    )?;
    write_json_create_new(
        &session_dir.join("raw-demo-result.json"),
        &json!({
            "schema": "dm-cp2077-main-collector-sidecar-result/v1",
            "session_id": session_id,
            "run_id": run_id,
            "generation": generation,
            "process_id": process_id,
            "process_launch_id": process_launch_id,
            "capture_owner": "dataminer_main_collector",
            "purpose": purpose.as_str(),
            "duration_ms": rgb_duration_ms,
            "video_frame_count": outcome.frame_count,
            "source_depth_frame_count": source_depth_frames,
            "paired_depth_frame_count": paired_depth_frames,
            "raw_input_event_count": input_metrics.raw_event_count,
            "keyboard_input_event_count": input_metrics.keyboard_events,
            "mouse_input_event_count": input_metrics.mouse_move_events,
            "player_pose_samples": player_pose_samples,
            "render_camera_samples": render_camera_samples,
            "exact_present_pairing_rate": pairing.exact_depth_rgb_coverage,
            "confirmed_flush": true,
            "main_collector_started": true,
            "main_collector_recording_ready": false,
            "sidecar_probe_recording_ready": false,
            "main_collector_formal_start_run": true,
            "sidecar_probe_formal_start_run": false,
            "non_trainable": true,
            "production_certified": false,
            "upload_allowed": false,
            "deliverable": false
        }),
    )?;
    Ok(MainCollectorDepthSummary {
        rgb_frames: outcome.frame_count,
        source_depth_frames,
        paired_depth_frames,
        rgb_duration_ms,
        depth_duration_ms,
        exact_present_matches: pairing.exact_present_matches,
        exact_present_pairing_rate: pairing.exact_depth_rgb_coverage,
        raw_input_events: input_metrics.raw_event_count,
        keyboard_input_events: input_metrics.keyboard_events,
        mouse_input_events: input_metrics.mouse_move_events,
        player_pose_samples,
        render_camera_samples,
        capture_queue_drops,
        input_queue_overflows: outcome.input_queue_overflows,
        metric_calibration_samples,
        rgb_segment_directory,
    })
}

fn link_main_rgb_segment(
    session_dir: &Path,
    outcome: &RecordingOutcome,
) -> anyhow::Result<PathBuf> {
    ensure!(
        outcome.run_directory.is_absolute(),
        "main run directory is not absolute"
    );
    let source = outcome.run_directory.join("segments").join("00000");
    ensure!(
        source.is_dir(),
        "main collector did not finalize RGB segment 00000"
    );
    ensure!(
        !outcome
            .run_directory
            .join("segments")
            .join("00001")
            .exists(),
        "CP2077 main-collector sidecar currently requires exactly one RGB segment"
    );
    let destination = session_dir
        .join("rgb-recorder")
        .join(&outcome.session_id)
        .join("runs")
        .join(&outcome.run_id)
        .join("segments")
        .join("00000");
    fs::create_dir_all(&destination)?;
    for entry in fs::read_dir(&source)? {
        let entry = entry?;
        let metadata = entry.metadata()?;
        ensure!(
            metadata.is_file(),
            "RGB segment contains an unexpected directory"
        );
        link_or_copy_main_rgb_file(&entry.path(), &destination.join(entry.file_name()))?;
    }
    for required in [
        "video.mp4",
        "frames.jsonl.zst",
        "events.jsonl.zst",
        "meta.json",
        "sha256.json",
    ] {
        ensure!(
            destination.join(required).is_file(),
            "RGB segment is missing {required}"
        );
    }
    Ok(destination)
}

const ERROR_NOT_SAME_DEVICE: i32 = 17;

fn link_or_copy_main_rgb_file(source: &Path, destination: &Path) -> anyhow::Result<()> {
    link_or_copy_main_rgb_file_with(source, destination, |source, destination| {
        fs::hard_link(source, destination)
    })
}

fn link_or_copy_main_rgb_file_with<F>(
    source: &Path,
    destination: &Path,
    hard_link: F,
) -> anyhow::Result<()>
where
    F: FnOnce(&Path, &Path) -> io::Result<()>,
{
    match hard_link(source, destination) {
        Ok(()) => Ok(()),
        Err(error) if error.raw_os_error() == Some(ERROR_NOT_SAME_DEVICE) => {
            let expected_bytes = fs::metadata(source)?.len();
            let copied_bytes = fs::copy(source, destination)?;
            ensure!(
                copied_bytes == expected_bytes,
                "cross-volume RGB segment copy was truncated"
            );
            Ok(())
        }
        Err(error) => Err(error).with_context(|| {
            format!(
                "hard-link RGB segment file {} to {}",
                source.display(),
                destination.display()
            )
        }),
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Pr727SettingsAttestation {
    schema: String,
    #[serde(default)]
    purpose: Option<String>,
    settings_version: i64,
    applied_utc: String,
    settings_snapshot_file_name: String,
    settings_sha256: String,
    live_settings_last_write_100ns: u64,
    settings_passed: bool,
    settings_ready: bool,
    recording_ready: bool,
}

#[derive(Debug, Serialize)]
struct Pr727SettingsRuntimeEvidence {
    schema: &'static str,
    purpose: &'static str,
    process_launch_id: String,
    process_id: u32,
    source: &'static str,
    settings_sha256: String,
    current_live_settings_sha256: String,
    process_creation_time_100ns: u64,
    applied_live_settings_last_write_100ns: u64,
    current_live_settings_last_write_100ns: u64,
    prelaunch_lead_ms: u64,
    runtime_fov_memory_evidence: &'static str,
    runtime_hud_subtitle_visual_evidence: &'static str,
    runtime_hud_subtitle_visual_classification: &'static str,
    settings_ready: bool,
    recording_ready: bool,
    non_trainable: bool,
}

#[derive(Debug, Deserialize)]
struct RgbTimingRow {
    session_frame: u64,
    source_qpc: i64,
}

#[derive(Debug, Clone, Copy)]
struct DecodedRgbFrame {
    present_low16: u64,
    source_qpc: i64,
}

#[derive(Debug, Deserialize)]
struct WatermarkSampleRow {
    session_frame: u64,
    block_means: Vec<u8>,
}

#[derive(Debug, Deserialize)]
struct RawDepthPairingRow {
    present_seq: u64,
    present_qpc: i64,
}

#[derive(Debug, Deserialize)]
struct Pr405RgbAuthorRow {
    session_frame: u64,
    source_qpc: i64,
    duplicate: bool,
    present_seq: Option<u64>,
    #[serde(default)]
    present_qpc: Option<i64>,
    #[serde(default)]
    keys: Vec<u16>,
    #[serde(default)]
    events: Vec<Pr405ActionEvent>,
}

#[derive(Debug, Deserialize)]
struct Pr405ActionEvent {
    #[serde(rename = "type")]
    event_type: String,
    dx: Option<i32>,
    dy: Option<i32>,
}

#[derive(Debug, Deserialize)]
struct Pr405RawAuthorRow {
    ordinal: u64,
    file: String,
    present_seq: u64,
    present_qpc: i64,
}

#[derive(Debug, Deserialize, Serialize)]
struct RawDepthSelectionRow {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
struct Pr405PresentIdentity {
    present_seq: u64,
    qpc: i64,
}

#[derive(Debug, Serialize)]
struct Pr405DepthBinding {
    ordinal: u64,
    file: String,
    identity: Pr405PresentIdentity,
}

#[derive(Debug, Serialize)]
struct Pr405ProjectionEvidence {
    proj_a: f64,
    proj_b: f64,
    reverse_z: bool,
    observed_this_frame: bool,
    physical_source: &'static str,
    candidate_admitted: bool,
}

#[derive(Debug, Serialize)]
struct Pr405FrameEvidence {
    schema: &'static str,
    session_frame: u64,
    duplicate: bool,
    rgb: Pr405PresentIdentity,
    camera: Pr405PresentIdentity,
    player: Pr405PresentIdentity,
    depth: Option<Pr405DepthBinding>,
    projection: Option<Pr405ProjectionEvidence>,
}

#[derive(Clone, Copy)]
struct Pr405RenderPose {
    identity: Pr405PresentIdentity,
    view: depth_lab::camera::Mat4,
    translation: depth_lab::camera::TranslationSlot,
    projection: depth_lab::camera::Mat4,
    proj_a: f64,
    proj_b: f64,
}

#[derive(Clone, Copy)]
struct Pr405PlayerPose {
    identity: Pr405PresentIdentity,
    position: [f64; 3],
    rotation: [f64; 4],
}

#[derive(Debug, Serialize)]
struct Pr405AuthoredEvidence {
    frame_evidence: String,
    final_pose: String,
    frame_rows: usize,
    pose_rows: usize,
}

#[derive(Debug, Serialize)]
struct RawRgbDepthPairingReport {
    schema: &'static str,
    passed: bool,
    rgb_frame_count: usize,
    decoded_rgb_watermarks: usize,
    decoded_rgb_watermark_rate: f64,
    depth_frame_count: usize,
    exact_present_matches: usize,
    exact_depth_rgb_coverage: f64,
    /// Measured cadence and the signed window derived from it. See
    /// [`RgbDepthPairingWindow`]; `lead` bounds the causally impossible
    /// direction and `lag` the compositor's own latency.
    measured_present_interval_ticks: i64,
    pairing_window_lead_ticks: i64,
    pairing_window_lag_ticks: i64,
    /// Signed `rgb_source_qpc - depth_present_qpc` across accepted pairs. A
    /// healthy run is entirely positive and well inside `pairing_window_lag`.
    min_compositor_lag_ticks: i64,
    median_compositor_lag_ticks: i64,
    max_compositor_lag_ticks: i64,
    /// Accepted pairs whose RGB stamp preceded the game's Present stamp. Not a
    /// blocker on its own, but a nonzero count means the two timestamp
    /// semantics need re-checking before the lag distribution is trusted.
    rgb_before_depth_pairs: usize,
    unmatched_depth_present_count: usize,
    low16_unambiguous_within_session: bool,
    blockers: Vec<String>,
    non_trainable: bool,
    recording_ready: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    even_cfr: Option<Value>,
}

#[derive(Debug, Serialize)]
struct OperatorActionPlan<'a> {
    schema: &'static str,
    process_launch_id: &'a str,
    duration_ms: u64,
    route: &'static str,
    route_constraints: [&'static str; 6],
    steps: &'static [OperatorActionStep],
    input_provenance: &'static str,
    input_validation_policy: &'static str,
    non_trainable: bool,
    recording_ready: bool,
}

#[derive(Debug, Serialize)]
struct OperatorActionMarker<'a> {
    schema: &'static str,
    process_launch_id: &'a str,
    marker_id: &'static str,
    phase: &'static str,
    instruction: &'static str,
    expected_input: &'static str,
    offset_ms: u64,
    scheduled_qpc: i64,
    actual_cue_qpc: i64,
    cue_delay_ms: u64,
    qpc_frequency: i64,
    input_provenance: &'static str,
    non_trainable: bool,
    recording_ready: bool,
}

#[derive(Debug, Serialize)]
struct PlayerPoseMetrics {
    sample_count: usize,
    resolver_id: String,
    object_tag: u64,
    transform_offset: u64,
    first_qpc: i64,
    last_qpc: i64,
    maximum_displacement_world_units: f64,
    outbound_displacement_world_units: f64,
    return_stop_closure_world_units: f64,
    terminal_closure_world_units: f64,
}

#[derive(Debug, Serialize)]
struct RenderCameraMetrics {
    matched_depth_presents: usize,
    depth_present_coverage: f64,
    physical_source: String,
    view_source: String,
    projection_source: String,
    view_source_aliases: Vec<depth_lab::D3d12MatrixSourceBinding>,
    projection_source_aliases: Vec<depth_lab::D3d12MatrixSourceBinding>,
    copy_validation_policy: &'static str,
    retained_view_copy_observations: usize,
    retained_projection_copy_observations: usize,
    view_sample_count: usize,
    projection_sample_count: usize,
    maximum_view_matrix_delta: f64,
    terminal_view_matrix_delta: f64,
    maximum_projection_matrix_delta: f64,
    projection_validation: projection_validation::ProjectionValidation,
}

type RenderMatrix = [[f64; 4]; 4];
type RenderMatrixSamples = std::collections::BTreeMap<u64, RenderMatrix>;
type RenderMatrixSources = std::collections::BTreeMap<String, RenderMatrixSamples>;

#[derive(Debug, Serialize)]
struct RawInputMetrics {
    evidence_event_count: usize,
    raw_event_count: usize,
    outbound_hold_ms: u64,
    return_hold_ms: u64,
    mouse_move_events: usize,
    keyboard_events: usize,
}

#[derive(Debug, Serialize)]
struct PoseCameraLoopQcReport {
    schema: &'static str,
    passed: bool,
    process_launch_id: String,
    qpc_start: i64,
    qpc_end: i64,
    qpc_frequency: i64,
    pose_report: &'static str,
    pose_report_sha256: String,
    render_camera_report: &'static str,
    render_camera_report_sha256: String,
    operator_action_plan: &'static str,
    operator_action_plan_sha256: String,
    operator_action_markers: &'static str,
    operator_action_markers_sha256: String,
    input_provenance: &'static str,
    autonomous_input_ledger: Option<&'static str>,
    autonomous_input_ledger_sha256: Option<String>,
    raw_input_events: String,
    raw_input_events_sha256: String,
    raw_input_meta: String,
    raw_input_meta_sha256: String,
    raw_input_manifest: String,
    raw_input_manifest_sha256: String,
    input_event_count: usize,
    raw_input_event_count: usize,
    outbound_hold_ms: u64,
    return_hold_ms: u64,
    mouse_move_events: usize,
    keyboard_events: usize,
    player_pose_samples: usize,
    player_resolver_id: String,
    player_object_tag: u64,
    player_transform_offset: u64,
    player_first_qpc: i64,
    player_last_qpc: i64,
    maximum_player_displacement_world_units: f64,
    outbound_player_displacement_world_units: f64,
    return_stop_closure_world_units: f64,
    terminal_player_closure_world_units: f64,
    render_matched_depth_presents: usize,
    render_depth_present_coverage: f64,
    render_physical_source: String,
    render_view_source: String,
    render_projection_source: String,
    render_view_source_aliases: Vec<depth_lab::D3d12MatrixSourceBinding>,
    render_projection_source_aliases: Vec<depth_lab::D3d12MatrixSourceBinding>,
    render_copy_validation_policy: &'static str,
    render_retained_view_copy_observations: usize,
    render_retained_projection_copy_observations: usize,
    render_view_samples: usize,
    render_projection_samples: usize,
    maximum_view_matrix_delta: f64,
    terminal_view_matrix_delta: f64,
    maximum_projection_matrix_delta: f64,
    projection_validation: projection_validation::ProjectionValidation,
    input_validation_policy: &'static str,
    blockers: Vec<String>,
    non_trainable: bool,
    production_certified: bool,
    deliverable: bool,
    recording_ready: bool,
}

fn validate_cadence_trace_path(path: &Path, session: &Path) -> anyhow::Result<()> {
    ensure!(
        cfg!(all(feature = "cp2077-diagnostics", debug_assertions)),
        "cadence trace requires cp2077-diagnostics and debug assertions"
    );
    ensure!(
        path.is_absolute() && !path.exists(),
        "trace must be a new absolute path"
    );
    let parent = fs::canonicalize(path.parent().context("trace has no parent")?)?;
    let session = fs::canonicalize(session)?;
    let parent = parent.to_string_lossy().to_lowercase();
    let session = session.to_string_lossy().to_lowercase();
    ensure!(
        parent != session && !parent.starts_with(&format!("{session}\\")),
        "cadence trace must be outside the entire raw session"
    );
    Ok(())
}

pub fn run(request: RawDemoCoordinatorRequest) -> anyhow::Result<RawDemoCoordinatorResult> {
    run_internal(request, None, false)
}

/// Engineering-only: preserve the existing raw coordinator and normal CFR policy.
#[cfg(all(feature = "cp2077-diagnostics", debug_assertions))]
pub fn run_with_cadence_trace(
    request: RawDemoCoordinatorRequest,
    trace_path: PathBuf,
) -> anyhow::Result<RawDemoCoordinatorResult> {
    run_internal(request, Some(trace_path), false)
}

/// Explicit holstered, non-trainable diagnostic using the existing GPU feedback protocol.
#[cfg(all(feature = "cp2077-diagnostics", debug_assertions))]
pub fn run_with_selected_present(
    request: RawDemoCoordinatorRequest,
) -> anyhow::Result<RawDemoCoordinatorResult> {
    run_internal(request, None, true)
}

fn run_internal(
    request: RawDemoCoordinatorRequest,
    cadence_trace_path: Option<PathBuf>,
    selected_present: bool,
) -> anyhow::Result<RawDemoCoordinatorResult> {
    eprintln!("cp2077 raw demo: validate request");
    validate_launch_id(&request.process_launch_id)?;
    ensure!(request.pid != 0, "CP2077 PID must be non-zero");
    let mut contract = capture_contract(&request.capture_contract, request.duration_ms)?;
    if request.manual_stop {
        ensure!(
            contract.name == depth_lab::ISSUE735_LOCAL_CONTRACT,
            "manual stop is only admitted for the local 64-second CP2077 contract"
        );
        contract.min_duration_ms = 60_000;
        contract.max_duration_ms = 240_000;
        contract.min_depth_frames = 900;
        contract.max_depth_frames = 4_096;
        contract.min_rgb_frames = 1_800;
        contract.max_rgb_frames = 7_200;
        contract.min_input_hold_ms = 0;
        contract.max_input_hold_ms = 240_000;
        contract.steps = &MANUAL_STOP_OPERATOR_ACTION_STEPS;
    }
    validate_autonomous_contract(request.autonomous_input, contract)?;
    if selected_present {
        ensure!(
            cfg!(all(feature = "cp2077-diagnostics", debug_assertions))
                && !request.autonomous_input
                && cadence_trace_path.is_none()
                && contract.name == depth_lab::ISSUE735_LOCAL_CONTRACT,
            "selected-Present raw mode requires a holstered diagnostic without autonomous input or raw cadence trace"
        );
        // V2 selects exact final RGB identities on even CFR slots. Preserve the
        // independent 900-original minimum and 1,024-original hard maximum.
        contract.max_depth_frames = 1024;
        contract.target_depth_hz = depth_lab::cp2077_raw_feedback::TARGET_DEPTH_HZ;
    }
    let session_dir = validate_session_dir(&request.session_dir)?;
    if let Some(path) = &cadence_trace_path {
        validate_cadence_trace_path(path, &session_dir)?;
    }
    validate_raw_config(&session_dir, contract)?;
    eprintln!("cp2077 raw demo: bind exact window");
    let settings_path = request.settings_path.unwrap_or_else(default_settings_path);
    let target = bind_exact_launched_window(request.pid)?;
    validate_recording_target(&target, request.pid)?;
    eprintln!("cp2077 raw demo: audit PR727 settings");
    let autonomous_target = AutonomousInputTarget {
        hwnd_value: target.hwnd_value,
        root_owner_value: target.root_owner_value,
        process_id: target.process_id,
    };
    let settings_evidence =
        audit_pr727_settings_for_process(&settings_path, request.pid, &request.process_launch_id)?;
    eprintln!("cp2077 raw demo: settings accepted, initialize monitor");
    write_json_create_new(
        &session_dir.join(PR727_SETTINGS_EVIDENCE_FILE),
        &settings_evidence,
    )?;
    write_action_plan(
        &session_dir,
        &request.process_launch_id,
        contract,
        request.autonomous_input,
        request.manual_stop,
    )?;

    let clock = QpcClock::new()?;
    let cutoff_qpc = Arc::new(AtomicI64::new(i64::MAX));
    let (stop_tx, stop_rx) = bounded::<RecordingStopSignal>(8);
    let (input_tx, input_rx) = bounded::<RawInputRecord>(RAW_INPUT_QUEUE_CAPACITY);
    // The Tauri command that starts this worker returns immediately.  On Windows the
    // originating WebView click can restore the client after SetForegroundWindow succeeds,
    // which used to seal otherwise healthy captures as `foreground_lost` after ~0.7 s.  Let
    // that click finish, then require the exact CP2077 root owner to stay foreground before
    // installing the fail-fast monitor or establishing the WGC/QPC anchor.
    std::thread::sleep(CLIENT_CLICK_SETTLE_DELAY);
    if !selected_present {
        WindowTracker::new()?.focus_revalidated(&target).context(
            "failed to restore the exact CP2077 window before starting its input monitor",
        )?;
    }
    wait_for_stable_target_foreground(&target)?;
    let monitor = TargetWindowMonitor::start(
        &target,
        format!("raw-demo:{}", request.process_launch_id),
        Arc::clone(&cutoff_qpc),
        stop_tx.clone(),
        input_tx,
        Arc::new(AtomicBool::new(false)),
    )?;
    eprintln!("cp2077 raw demo: monitor initialized");
    let input_control = monitor.input_control();
    let input_status = monitor.input_status();
    ensure!(
        !input_status.is_accepting() && !input_status.faulted() && input_rx.is_empty(),
        "raw input monitor was not quiescent before RGB start"
    );
    let commit = monitor.prepare_commit()?;
    ensure!(
        commit.activate_and_open(),
        "raw input gate faulted before RGB start"
    );
    // Arm the exact semantic depth target before WGC starts.  The probe does
    // not write anything until a game Present arrives, but this closes the
    // otherwise unavoidable startup hole where RGB slots 0/1 can precede the
    // first arm request and therefore have no same-Present raw depth.  The
    // Started event below still owns the 30-second QPC action/cutoff window;
    // this only establishes readiness for its first RGB source Present.
    let feedback = if selected_present {
        Some(selected_feedback::prepare(
            request.pid,
            &request.process_launch_id,
        )?)
    } else {
        None
    };
    write_arm_request_with_feedback(
        &session_dir,
        &request.process_launch_id,
        contract,
        request.manual_stop,
        feedback.as_ref().map(|f| &f.request),
    )?;
    // Raw depth is already armed before WGC produces its first frame. Every
    // subsequent return must stop it, including failures before Started.
    let mut raw_depth_stop = RawDepthStopGuard::new(&session_dir, &request.process_launch_id);
    let present_identity = if selected_present {
        let profile_sha = sha256_file(&session_dir.join("raw-depth-demo-profile.json"))?;
        Some(selected_feedback::wait_ready(
            &session_dir,
            &request.process_launch_id,
            &profile_sha,
        )?)
    } else {
        None
    };
    eprintln!("cp2077 raw demo: raw input gate open, start RGB recorder");

    let rgb_root = session_dir.join("rgb-recorder");
    fs::create_dir(&rgb_root)
        .with_context(|| format!("refusing to reuse RGB output {}", rgb_root.display()))?;
    let session_id = format!("raw-rgb-{}", request.process_launch_id);
    let run_id = "r0000".to_owned();
    let (profile, segment_policy_override) = local_depth_trial_profile(&target.capture_profile)?;
    let recording = RecordingRequest {
        session_id: session_id.clone(),
        run_id: run_id.clone(),
        session_frame_start: 0,
        output_root: rgb_root.clone(),
        target: RecordingTarget {
            hwnd_value: target.hwnd_value,
            process_id: target.process_id,
            target_id: target.summary.target_id.clone(),
            game_id: target.summary.game_id.clone(),
            executable_sha256: target.executable_sha256.clone(),
        },
        profile,
        segment_policy_override: Some(segment_policy_override),
        mapping: target.mapping.clone(),
        catalog: target.catalog_selection.recording_context(),
        control_vks: target.control_vks(),
        consent_id: None,
        // Raw demo pairing reads the probe's Present watermark from the encoded
        // pixels offline. It never joins the formal selected-Present transport.
        present_identity,
        selected_present_feedback: feedback.map(|f| f.transport),
        selected_depth_cadence: if selected_present {
            depth_lab::selected_present_feedback::DepthSelectionCadence::EvenCfrFrames
        } else {
            Default::default()
        },
        cutoff_qpc: Arc::clone(&cutoff_qpc),
        input_records: input_rx,
        input_control,
        input_status,
    };

    let timer = Arc::new(Mutex::new(None::<JoinHandle<()>>));
    let action_cues = Arc::new(Mutex::new(Vec::<ActualActionCue>::new()));
    let autonomous_ledger = Arc::new(Mutex::new(Vec::<AutonomousInputLedgerRow>::new()));
    let autonomous_failure = Arc::new(Mutex::new(None::<String>));
    let cancel_timer = Arc::new(AtomicBool::new(false));
    let started = Arc::new(AtomicBool::new(false));
    let arm_session_dir = session_dir.clone();
    let arm_launch_id = request.process_launch_id.clone();
    let event_timer = Arc::clone(&timer);
    let event_action_cues = Arc::clone(&action_cues);
    let event_autonomous_ledger = Arc::clone(&autonomous_ledger);
    let event_autonomous_failure = Arc::clone(&autonomous_failure);
    let autonomous_input = request.autonomous_input;
    let manual_stop = request.manual_stop;
    let event_cancel = Arc::clone(&cancel_timer);
    let event_started = Arc::clone(&started);
    let event_cutoff = Arc::clone(&cutoff_qpc);
    let timer_stop = stop_tx.clone();
    let timer_clock = clock;

    let on_event = move |event| {
        if let RecorderEvent::Started {
            anchor_qpc,
            qpc_frequency,
            ..
        } = event
        {
            eprintln!("cp2077 raw demo: RGB started, arm depth");
            ensure!(
                !event_started.swap(true, Ordering::AcqRel),
                "RGB recorder emitted Started twice"
            );
            ensure!(
                qpc_frequency == timer_clock.frequency(),
                "RGB recorder changed QPC frequency"
            );
            // This OS sound is emitted synchronously from the exact recorder
            // Started event.  It is the operator's sole zero-time cue; chat or
            // UI status messages are deliberately not part of capture timing.
            unsafe {
                let _ = MessageBeep(MB_ICONASTERISK);
            }
            let duration_ticks = qpc_frequency
                .checked_mul(i64::try_from(contract.duration_ms)?)
                .context("raw demo duration QPC overflow")?
                / 1_000;
            let scheduled_depth_cutoff = anchor_qpc
                .checked_add(duration_ticks)
                .context("raw demo cutoff QPC overflow")?;
            // The CFR recorder emits slots [0, 30s) and needs the boundary at
            // 30s to finalize slot 899. Keep RGB alive for one 30 fps tick
            // after the exact 30s depth stop; this does not extend the raw
            // capture or the audited action window.
            let rgb_tail_ticks = qpc_frequency / 30;
            let scheduled_rgb_cutoff = scheduled_depth_cutoff
                .checked_add(rgb_tail_ticks)
                .context("raw demo RGB tail cutoff QPC overflow")?;
            event_cutoff.store(scheduled_rgb_cutoff, Ordering::Release);
            let worker_dir = arm_session_dir.clone();
            let worker_launch_id = arm_launch_id.clone();
            let worker_cancel = Arc::clone(&event_cancel);
            let worker_action_cues = Arc::clone(&event_action_cues);
            let worker_autonomous_ledger = Arc::clone(&event_autonomous_ledger);
            let worker_autonomous_failure = Arc::clone(&event_autonomous_failure);
            let worker_stop = timer_stop.clone();
            let worker = std::thread::Builder::new()
                .name("dm-cp2077-raw-demo-cutoff".into())
                .spawn(move || {
                    let mut next_action_cue = 0usize;
                    let mut held_autonomous_keys = HeldAutonomousKeys::default();
                    let mut last_qpc = anchor_qpc;
                    let mut depth_stop_written = false;
                    while !worker_cancel.load(Ordering::Acquire) {
                        let now = match timer_clock.now() {
                            Ok(now) => {
                                last_qpc = now;
                                now
                            }
                            Err(error) if autonomous_input => {
                                release_autonomous_keys(&mut held_autonomous_keys);
                                *worker_autonomous_failure
                                    .lock()
                                    .expect("autonomous input failure lock poisoned") =
                                    Some(format!("autonomous input QPC failed: {error}"));
                                let _ = write_stop_request(&worker_dir, &worker_launch_id);
                                let _ = worker_stop.try_send(RecordingStopSignal {
                                    reason: RecordingStopReason::Manual,
                                    cutoff_qpc: last_qpc,
                                });

                                return;
                            }
                            Err(_) => {
                                std::thread::sleep(Duration::from_millis(2));
                                continue;
                            }
                        };
                        let manual_requested = if selected_present {
                            match selected_feedback::rgb_stop_requested(
                                &worker_dir,
                                &worker_launch_id,
                            ) {
                                Ok(stop) => stop,
                                Err(error) => {
                                    *worker_autonomous_failure
                                        .lock()
                                        .expect("raw stop failure lock poisoned") =
                                        Some(error.to_string());
                                    true
                                }
                            }
                        } else {
                            worker_dir.join(STOP_FILE).is_file()
                        };
                        if manual_stop && manual_requested {
                            let _ = worker_stop.try_send(RecordingStopSignal {
                                reason: RecordingStopReason::Manual,
                                cutoff_qpc: now,
                            });
                            return;
                        }
                        if autonomous_input
                            && let Err(error) = validate_autonomous_input_target(autonomous_target)
                        {
                            release_autonomous_keys(&mut held_autonomous_keys);
                            *worker_autonomous_failure
                                .lock()
                                .expect("autonomous input failure lock poisoned") =
                                Some(error.to_string());
                            let _ = write_stop_request(&worker_dir, &worker_launch_id);
                            let _ = worker_stop.try_send(RecordingStopSignal {
                                reason: RecordingStopReason::Manual,
                                cutoff_qpc: now,
                            });
                            return;
                        }
                        while let Some(step) = contract.steps.get(next_action_cue)
                            && scheduled_action_qpc(anchor_qpc, qpc_frequency, step.offset_ms)
                                .is_ok_and(|deadline| now >= deadline)
                        {
                            let scheduled_qpc =
                                scheduled_action_qpc(anchor_qpc, qpc_frequency, step.offset_ms)
                                    .expect("validated action-plan QPC");
                            let mut actual_qpc = now;
                            if autonomous_input
                                && let Some((vk, action)) = autonomous_transition(step.marker_id)
                            {
                                let state_transition_is_valid = match action {
                                    AutonomousKeyAction::Down => !held_autonomous_keys.contains(vk),
                                    AutonomousKeyAction::Up => held_autonomous_keys.contains(vk),
                                };
                                if !state_transition_is_valid {
                                    release_autonomous_keys(&mut held_autonomous_keys);
                                    *worker_autonomous_failure
                                        .lock()
                                        .expect("autonomous input failure lock poisoned") =
                                        Some(format!(
                                            "autonomous input marker {} contradicts held-key state",
                                            step.marker_id
                                        ));
                                    let _ = write_stop_request(&worker_dir, &worker_launch_id);
                                    let _ = worker_stop.try_send(RecordingStopSignal {
                                        reason: RecordingStopReason::Manual,
                                        cutoff_qpc: now,
                                    });
                                    return;
                                }
                                match send_autonomous_key(
                                    autonomous_target,
                                    &worker_launch_id,
                                    step.marker_id,
                                    scheduled_qpc,
                                    now,
                                    qpc_frequency,
                                    vk,
                                    action,
                                    timer_clock,
                                ) {
                                    Ok(row) => {
                                        actual_qpc = row.actual_qpc;
                                        match action {
                                            AutonomousKeyAction::Down => {
                                                held_autonomous_keys.insert(vk);
                                            }
                                            AutonomousKeyAction::Up => {
                                                held_autonomous_keys.remove(vk);
                                            }
                                        }
                                        worker_autonomous_ledger
                                            .lock()
                                            .expect("autonomous input ledger lock poisoned")
                                            .push(row);
                                    }
                                    Err(error) => {
                                        release_autonomous_keys(&mut held_autonomous_keys);
                                        *worker_autonomous_failure
                                            .lock()
                                            .expect("autonomous input failure lock poisoned") =
                                            Some(error.to_string());
                                        let _ = write_stop_request(&worker_dir, &worker_launch_id);
                                        let _ = worker_stop.try_send(RecordingStopSignal {
                                            reason: RecordingStopReason::Manual,
                                            cutoff_qpc: now,
                                        });
                                        return;
                                    }
                                }
                            }
                            worker_action_cues
                                .lock()
                                .expect("raw demo action cue lock poisoned")
                                .push(ActualActionCue {
                                    step_index: next_action_cue,
                                    scheduled_qpc,
                                    actual_qpc,
                                });
                            eprintln!(
                                "\u{7}[CP2077 action {:>2}.{:03}s] {}",
                                step.offset_ms / 1_000,
                                step.offset_ms % 1_000,
                                step.instruction
                            );
                            next_action_cue += 1;
                        }
                        if !manual_stop
                            && !selected_present
                            && contract.name != depth_lab::PR405_LOCAL_CONTRACT
                            && now >= scheduled_depth_cutoff
                            && !depth_stop_written
                        {
                            release_autonomous_keys(&mut held_autonomous_keys);
                            let _ = write_stop_request(&worker_dir, &worker_launch_id);
                            depth_stop_written = true;
                        }
                        if !manual_stop && now >= scheduled_rgb_cutoff {
                            let _ = worker_stop.try_send(RecordingStopSignal {
                                reason: RecordingStopReason::Manual,
                                cutoff_qpc: scheduled_rgb_cutoff,
                            });
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    release_autonomous_keys(&mut held_autonomous_keys);
                })?;
            *event_timer.lock().expect("raw demo timer lock poisoned") = Some(worker);
        }
        Ok(())
    };
    let capture_result = if let Some(path) = cadence_trace_path {
        #[cfg(all(feature = "cp2077-diagnostics", debug_assertions))]
        {
            use capture_core::recording::{
                CadenceTraceConfig, RawWatermarkTraceConfig,
                record_until_stopped_with_cadence_trace,
            };
            record_until_stopped_with_cadence_trace(
                recording,
                &stop_rx,
                CadenceTraceConfig {
                    output_path: Some(path),
                    max_events: 65_536,
                    raw_watermark: Some(RawWatermarkTraceConfig {
                        source_width: 1920,
                        source_height: 1080,
                    }),
                    experimental_seal_delay_ms: None,
                    experimental_empty_slot_grace: false,
                },
                on_event,
            )
        }
        #[cfg(not(all(feature = "cp2077-diagnostics", debug_assertions)))]
        {
            let _ = path;
            unreachable!("diagnostics rejected before arm")
        }
    } else {
        record_until_stopped(recording, &stop_rx, on_event)
    };

    // PR405 publishes exactly 900 CFR RGB rows, but the probe's depth copy is
    // completed asynchronously one Present later.  Keep only the probe alive
    // for a short bounded drain tail after WGC has finalized row 899.  The tail
    // remains inside the strict 29..=31 second raw-depth duration gate and is
    // never represented as additional RGB/action time.
    if capture_result.is_ok()
        && contract.name == depth_lab::PR405_LOCAL_CONTRACT
        && !session_dir.join(STOP_FILE).exists()
    {
        // Real CP2077/WGC observations show that the CFR encoder can finalize
        // row 899 roughly eleven Presents ahead of the probe writer.  Give the
        // private fence/readback ring a bounded 800 ms tail: this is long
        // enough for that measured backlog at 30 Hz while keeping the probe's
        // elapsed duration below the contract's strict 31 second ceiling.
        std::thread::sleep(Duration::from_millis(800));
    }
    cancel_timer.store(true, Ordering::Release);
    raw_depth_stop.stop()?;
    drop(raw_depth_stop);
    if let Some(worker) = timer.lock().expect("raw demo timer lock poisoned").take() {
        worker
            .join()
            .map_err(|_| anyhow::anyhow!("raw demo cutoff worker panicked"))?;
    }
    let actual_action_cues = action_cues
        .lock()
        .expect("raw demo action cue lock poisoned")
        .clone();
    let autonomous_ledger = autonomous_ledger
        .lock()
        .expect("autonomous input ledger lock poisoned")
        .clone();
    if request.autonomous_input && !autonomous_ledger.is_empty() {
        write_jsonl_create_new(
            &session_dir.join(AUTONOMOUS_INPUT_LEDGER_FILE),
            &autonomous_ledger,
        )?;
    }
    if let Some(failure) = autonomous_failure
        .lock()
        .expect("autonomous input failure lock poisoned")
        .clone()
    {
        anyhow::bail!("authorized autonomous input failed closed: {failure}");
    }
    drop(monitor);
    let outcome = capture_result.context("local raw demo RGB recorder failed")?;

    let qpc_start = outcome.qpc_start.context("RGB outcome has no start QPC")?;
    let qpc_end = outcome.qpc_end.context("RGB outcome has no end QPC")?;
    let action_anchor_qpc = outcome
        .anchor_qpc
        .context("RGB outcome has no action-plan anchor QPC")?;
    let rgb_duration_ms = duration_ms(qpc_start, qpc_end, outcome.qpc_frequency)?;
    let video_duration_ms = outcome
        .frame_count
        .checked_mul(1_000)
        .and_then(|value| value.checked_mul(u64::from(target.capture_profile.fps_den)))
        .context("RGB frame duration overflow")?
        / u64::from(target.capture_profile.fps_num);
    ensure_duration("RGB QPC", rgb_duration_ms, contract)?;
    ensure_duration("RGB MP4 timeline", video_duration_ms, contract)?;
    ensure!(
        (contract.min_rgb_frames..=contract.max_rgb_frames).contains(&outcome.frame_count),
        "RGB frame count is outside the selected capture contract"
    );
    ensure!(
        outcome.segment_count == 1,
        "raw demo RGB produced multiple segments"
    );
    ensure!(outcome.non_trainable == Some(true) && outcome.production_certified == Some(false));

    // Persist the operator/QPC timeline before any offline post-processing.
    // Watermark extraction and pixel QC are intentionally fail-closed, but a
    // missing packaged helper must not discard the already completed input
    // evidence or make an otherwise recoverable 64-second session opaque.
    write_action_markers(
        &session_dir,
        &request.process_launch_id,
        action_anchor_qpc,
        outcome.qpc_frequency,
        &actual_action_cues,
        contract,
        request.autonomous_input,
    )?;

    let segment_dir = outcome.run_directory.join("segments").join("00000");
    let source_video = segment_dir.join("video.mp4");
    let source_frames = segment_dir.join("frames.jsonl.zst");
    ensure!(source_video.is_file(), "RGB video.mp4 was not finalized");
    ensure!(
        source_frames.is_file(),
        "RGB QPC frame index was not finalized"
    );
    let published_video = session_dir.join("video.mp4");
    fs::hard_link(&source_video, &published_video).with_context(|| {
        format!(
            "failed to publish local video hard link {}",
            published_video.display()
        )
    })?;
    let video_sha256 = sha256_file(&published_video)?;
    extract_watermark_samples(&session_dir, &published_video)?;

    let telemetry_path = wait_for_file(&session_dir.join(TELEMETRY_FILE), Duration::from_secs(10))?;
    let depth_telemetry = validate_depth_telemetry(
        &fs::read(&telemetry_path)?,
        &request.process_launch_id,
        contract,
    )?;
    let source_depth_frame_count = required_u64(&depth_telemetry, "written_frames")?;
    let depth_duration_ms = required_u64(&depth_telemetry, "elapsed_ms")?;
    // Selected feedback includes camera warmup and a bounded GPU/writer drain
    // in this lifecycle clock. The unchanged RGB timeline defines duration;
    // required-slot mapping below proves that depth covers that timeline.
    if !selected_present {
        ensure_duration("raw depth QPC", depth_duration_ms, contract)?;
    }
    ensure!(
        (contract.min_depth_frames..=contract.max_depth_frames).contains(&source_depth_frame_count),
        "raw depth wrote {source_depth_frame_count} source-Present frames outside the capture contract"
    );

    let depth_frame_count = if selected_present {
        // Every raw row was explicitly selected by an even CFR slot. Do not
        // prune/reindex this evidence after recording or hide orphan rows.
        source_depth_frame_count
    } else if contract.name == depth_lab::PR405_LOCAL_CONTRACT {
        materialize_pr405_selected_depth(&session_dir, &source_frames)?
    } else {
        materialize_issue735_exact_depth(
            &session_dir,
            &source_frames,
            outcome.qpc_frequency,
            contract,
        )?
    };

    let depth_qc_contract = if contract.name == depth_lab::PR405_LOCAL_CONTRACT {
        depth_lab::RawDepthQcContract::Pr405Local30Seconds
    } else if request.manual_stop {
        depth_lab::RawDepthQcContract::ManualStopDemo
    } else {
        depth_lab::RawDepthQcContract::CoordinatedDemo
    };
    let depth_qc = depth_lab::audit_cp2077_raw_depth_session(&session_dir, depth_qc_contract)
        .map_err(anyhow::Error::msg)?;
    ensure!(
        depth_qc.passed,
        "raw D32 pixel/QPC QC failed: {}",
        depth_qc.blockers.join(", ")
    );
    write_json_create_new(&session_dir.join(DEPTH_QC_FILE), &depth_qc)?;

    let pairing = validate_rgb_depth_pairing(&session_dir, &source_frames, outcome.qpc_frequency)?;
    write_json_create_new(&session_dir.join(PAIRING_FILE), &pairing)?;
    if selected_present {
        ensure!(
            pairing.schema == depth_lab::cp2077_raw_feedback::PAIRING_SCHEMA
                && pairing.even_cfr.is_some(),
            "even-CFR recording requires its complete v3 required-slot pairing report"
        );
    }
    if selected_present || contract.name == depth_lab::PR405_LOCAL_CONTRACT {
        ensure!(
            pairing.passed,
            "raw RGB/depth exact-Present pairing failed: {}",
            pairing.blockers.join(", ")
        );
    }

    let pose_report_path =
        wait_for_file(&session_dir.join(POSE_REPORT_FILE), Duration::from_secs(10))?;
    let render_camera_report_path = wait_for_file(
        &session_dir.join(RENDER_CAMERA_REPORT_FILE),
        Duration::from_secs(10),
    )?;
    let source_events = segment_dir.join("events.jsonl.zst");
    let source_meta = segment_dir.join("meta.json");
    let source_manifest = segment_dir.join("sha256.json");
    let pose_camera_loop_qc = audit_pose_camera_loop(
        &session_dir,
        &request.process_launch_id,
        qpc_start,
        qpc_end,
        outcome.qpc_frequency,
        outcome.frame_count,
        outcome.input_event_count,
        outcome.input_queue_overflows,
        request.autonomous_input,
        request.manual_stop,
        standalone_requires_mouse_movement(contract, request.manual_stop),
        request.pid,
        &actual_action_cues,
        contract,
        &pose_report_path,
        &render_camera_report_path,
        &source_events,
        &source_meta,
        &source_manifest,
    )?;
    ensure!(
        pose_camera_loop_qc.passed,
        "raw pose/camera/Loop evidence did not pass"
    );
    if selected_present {
        ensure!(
            pose_camera_loop_qc.render_depth_present_coverage == 1.0,
            "even-CFR required depths do not all have exact render-camera evidence"
        );
    }
    write_json_create_new(
        &session_dir.join(POSE_CAMERA_LOOP_QC_FILE),
        &pose_camera_loop_qc,
    )?;

    let pr405_evidence = if contract.name == depth_lab::PR405_LOCAL_CONTRACT {
        Some(author_pr405_frame_and_pose_evidence(
            &session_dir,
            &source_frames,
            &request.process_launch_id,
            outcome.qpc_frequency,
            request.autonomous_input,
        )?)
    } else {
        None
    };

    let relative_frames = relative_path(&session_dir, &source_frames)?;
    let rgb_index_path = session_dir.join("rgb-index.json");
    write_json_create_new(
        &rgb_index_path,
        &json!({
            "schema": INDEX_SCHEMA,
            "process_launch_id": request.process_launch_id,
            "process_id": request.pid,
            "video": "video.mp4",
            "video_sha256": video_sha256,
            "frames_index": relative_frames,
            "frame_count": outcome.frame_count,
            "qpc_frequency": outcome.qpc_frequency,
            "qpc_start": qpc_start,
            "qpc_end": qpc_end,
            "duration_ms": rgb_duration_ms,
            "present_identity": {
                "carrier": "gpu_watermark_embedded_in_video",
                "decoder": "dm-depth-extract",
                "depth_join_key": "present_seq",
                "rgb_time_join_key": "source_qpc"
            },
            "non_trainable": true,
            "production_certified": false,
            "upload_allowed": false,
            "recording_ready": false,
            "formal_start_run_used": false
        }),
    )?;

    let result = RawDemoCoordinatorResult {
        schema: RESULT_SCHEMA,
        process_launch_id: request.process_launch_id,
        process_id: request.pid,
        capture_contract: contract.name.into(),
        duration_ms: rgb_duration_ms,
        video_frame_count: outcome.frame_count,
        depth_frame_count,
        video_sha256,
        video: "video.mp4".into(),
        rgb_index: "rgb-index.json".into(),
        depth_index: "raw-depth-index.jsonl".into(),
        depth_telemetry: TELEMETRY_FILE.into(),
        depth_qc: DEPTH_QC_FILE.into(),
        rgb_depth_pairing: PAIRING_FILE.into(),
        pose_report: POSE_REPORT_FILE.into(),
        render_camera_report: RENDER_CAMERA_REPORT_FILE.into(),
        operator_action_plan: ACTION_PLAN_FILE.into(),
        operator_action_markers: ACTION_MARKERS_FILE.into(),
        raw_input_events: relative_path(&session_dir, &source_events)?,
        raw_input_meta: relative_path(&session_dir, &source_meta)?,
        raw_input_manifest: relative_path(&session_dir, &source_manifest)?,
        pose_camera_loop_qc: POSE_CAMERA_LOOP_QC_FILE.into(),
        pr727_settings_runtime_evidence: PR727_SETTINGS_EVIDENCE_FILE.into(),
        input_provenance: if request.autonomous_input {
            AUTONOMOUS_INPUT_PROVENANCE
        } else {
            MANUAL_INPUT_PROVENANCE
        },
        autonomous_input_ledger: request
            .autonomous_input
            .then(|| AUTONOMOUS_INPUT_LEDGER_FILE.into()),
        pr405_frame_evidence: pr405_evidence
            .as_ref()
            .map(|evidence| evidence.frame_evidence.clone()),
        pr405_final_pose: pr405_evidence
            .as_ref()
            .map(|evidence| evidence.final_pose.clone()),
        pr405_frame_evidence_rows: pr405_evidence.as_ref().map(|evidence| evidence.frame_rows),
        pr405_final_pose_rows: pr405_evidence.as_ref().map(|evidence| evidence.pose_rows),
        non_trainable: true,
        production_certified: false,
        upload_allowed: false,
        deliverable: false,
        recording_ready: false,
        formal_start_run_used: false,
    };
    write_json_create_new(&session_dir.join("raw-demo-result.json"), &result)?;
    Ok(result)
}

fn wait_for_stable_target_foreground(target: &BoundTarget) -> anyhow::Result<()> {
    let deadline = Instant::now() + FOREGROUND_STABILITY_TIMEOUT;
    let mut stable_since = None;
    loop {
        let foreground_root = unsafe { GetAncestor(GetForegroundWindow(), GA_ROOTOWNER) };
        let target_is_foreground = foreground_root.0 as usize == target.root_owner_value;
        if target_is_foreground {
            let since = stable_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= FOREGROUND_STABILITY_WINDOW {
                return Ok(());
            }
        } else {
            stable_since = None;
        }
        ensure!(
            Instant::now() < deadline,
            "exact CP2077 window did not remain foreground for 500 ms within 10 seconds"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn validate_autonomous_contract(enabled: bool, contract: CaptureContract) -> anyhow::Result<()> {
    ensure!(
        !enabled
            || matches!(
                (contract.name, contract.duration_ms),
                (
                    depth_lab::PR405_LOCAL_CONTRACT,
                    depth_lab::PR405_LOCAL_DURATION_MS
                ) | (
                    depth_lab::ISSUE735_LOCAL_CONTRACT,
                    depth_lab::ISSUE735_LOCAL_DURATION_MS
                )
            ),
        "--autonomous-input requires an exact supported contract/duration pair"
    );
    Ok(())
}

fn validate_rgb_depth_pairing(
    session_dir: &Path,
    frames_path: &Path,
    qpc_frequency: i64,
) -> anyhow::Result<RawRgbDepthPairingReport> {
    ensure!(
        qpc_frequency > 0,
        "RGB/depth pairing has invalid QPC frequency"
    );
    let samples_path = session_dir.join(WATERMARK_SAMPLES_FILE);
    ensure!(
        samples_path.is_file(),
        "watermark samples were not extracted"
    );

    let rgb: Vec<RgbTimingRow> = read_bounded_jsonl(frames_path, true)?;
    let samples: Vec<WatermarkSampleRow> = read_bounded_jsonl(&samples_path, false)?;
    let depth: Vec<RawDepthPairingRow> =
        read_bounded_jsonl(&session_dir.join("raw-depth-index.jsonl"), false)?;
    ensure!(
        !rgb.is_empty() && !depth.is_empty(),
        "RGB/depth pairing inputs are empty"
    );

    let rgb_frame_ids = rgb
        .iter()
        .map(|row| row.session_frame)
        .collect::<std::collections::BTreeSet<_>>();
    let mut sample_by_frame = std::collections::HashMap::new();
    for sample in samples {
        ensure!(
            rgb_frame_ids.contains(&sample.session_frame),
            "watermark sample is outside the RGB timeline"
        );
        ensure!(
            sample_by_frame
                .insert(sample.session_frame, sample.block_means)
                .is_none(),
            "duplicate watermark evidence for one RGB frame"
        );
    }
    let mut decoded = Vec::new();
    for frame in &rgb {
        if let Some(means) = sample_by_frame.get(&frame.session_frame)
            && let Ok(low16) = depth_lab::watermark::decode(means, 32)
        {
            decoded.push(DecodedRgbFrame {
                present_low16: u64::from(low16),
                source_qpc: frame.source_qpc,
            });
        }
    }
    let mut report = pair_rgb_depth(rgb.len(), &decoded, &depth, qpc_frequency);
    let summary = session_dir.join(depth_lab::cp2077_raw_feedback::SUMMARY_FILE);
    if summary.is_file() {
        let value: Value = serde_json::from_slice(&read_bounded_file(&summary, 128 * 1024)?)?;
        if value["schema"] == depth_lab::cp2077_raw_feedback::SUMMARY_SCHEMA {
            selected_feedback::audit_even_cfr(
                session_dir,
                frames_path,
                &sample_by_frame,
                &value,
                &mut report,
            )?;
        } else {
            ensure!(
                value["schema"] == "dm-cp2077-raw-selected-summary/v1",
                "unknown raw selected summary schema"
            );
        }
    }
    Ok(report)
}

fn extract_watermark_samples(session_dir: &Path, video: &Path) -> anyhow::Result<()> {
    let samples_path = session_dir.join(WATERMARK_SAMPLES_FILE);
    ensure!(
        !samples_path.exists(),
        "raw demo contains stale watermark samples"
    );
    let extractor = std::env::current_exe()?
        .parent()
        .context("raw demo recorder has no executable directory")?
        .join("dm-depth-extract.exe");
    ensure!(
        extractor.is_file(),
        "dm-depth-extract.exe is not installed beside the recorder"
    );
    // Media Foundation treats Rust's canonical Windows `\\?\` spelling as a
    // network path. The session has already been canonically contained above;
    // hand the extractor a normal drive-letter spelling.
    let extractor_video = strip_windows_verbatim_prefix(video);
    let extractor_samples = strip_windows_verbatim_prefix(&samples_path);
    let status = std::process::Command::new(&extractor)
        .arg("--video")
        .arg(&extractor_video)
        .arg("--out")
        .arg(&extractor_samples)
        .status()
        .context("launching dm-depth-extract failed")?;
    ensure!(status.success(), "dm-depth-extract failed with {status}");
    Ok(())
}

fn extract_metric_screenshots(
    session_dir: &Path,
    video: &Path,
    session_frames: &[u64],
) -> anyhow::Result<Vec<String>> {
    ensure!(
        (3..=32).contains(&session_frames.len()),
        "metric calibration requires 3..=32 F9 samples"
    );
    ensure!(
        session_frames.windows(2).all(|pair| pair[1] > pair[0]),
        "metric calibration sample frames must be strictly increasing"
    );
    let extractor = std::env::current_exe()?
        .parent()
        .context("raw demo recorder has no executable directory")?
        .join("dm-depth-extract.exe");
    ensure!(
        extractor.is_file(),
        "dm-depth-extract.exe is not installed beside the recorder"
    );
    let frames_arg = session_frames
        .iter()
        .map(u64::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let samples_path = session_dir.join("metric-watermark-samples.jsonl");
    let status = std::process::Command::new(&extractor)
        .arg("--video")
        .arg(strip_windows_verbatim_prefix(video))
        .arg("--out")
        .arg(strip_windows_verbatim_prefix(&samples_path))
        .arg("--png-frames")
        .arg(frames_arg)
        .arg("--png-dir")
        .arg(strip_windows_verbatim_prefix(session_dir))
        .status()
        .context("launching dm-depth-extract for metric screenshots failed")?;
    ensure!(
        status.success(),
        "dm-depth-extract metric screenshot export failed with {status}"
    );
    let names = session_frames
        .iter()
        .enumerate()
        .map(|(index, frame)| format!("metric-hud-{index:02}-frame-{frame:010}.png"))
        .collect::<Vec<_>>();
    for name in &names {
        ensure!(
            session_dir.join(name).is_file(),
            "metric screenshot export omitted {name}"
        );
    }
    Ok(names)
}

fn append_metric_action_markers(
    session_dir: &Path,
    process_launch_id: &str,
    markers: &[MarkerAudit],
) -> anyhow::Result<()> {
    ensure!(
        markers.len() == 5,
        "metric calibration requires exactly five Ctrl+Alt+F9 samples; observed {}",
        markers.len()
    );
    let mut file = OpenOptions::new()
        .append(true)
        .open(session_dir.join(ACTION_MARKERS_FILE))?;
    for marker in markers {
        serde_json::to_writer(
            &mut file,
            &json!({
                "schema": "dm-cp2077-metric-action-marker/v1",
                "process_launch_id": process_launch_id,
                "marker": marker,
                "non_trainable": true,
                "recording_ready": false
            }),
        )?;
        file.write_all(b"\n")?;
    }
    file.sync_all()?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn author_metric_calibration_capture(
    session_dir: &Path,
    frames_path: &Path,
    video: &Path,
    pose_report_path: &Path,
    process_launch_id: &str,
    session_id: &str,
    run_id: &str,
    qpc_frequency: i64,
    markers: &[MarkerAudit],
) -> anyhow::Result<usize> {
    ensure!(
        qpc_frequency > 0,
        "metric calibration QPC frequency is invalid"
    );
    ensure!(
        markers.len() == 5,
        "metric calibration requires exactly five Ctrl+Alt+F9 samples; observed {}",
        markers.len()
    );
    ensure!(
        markers.iter().all(|marker| {
            marker.session_id == session_id
                && marker.run_id == run_id
                && marker.qpc_frequency == qpc_frequency
                && marker.qpc > 0
        }),
        "metric calibration marker identity/QPC domain does not match the main run"
    );
    ensure!(
        markers.windows(2).all(|pair| {
            pair[1].qpc > pair[0].qpc && pair[1].session_frame > pair[0].session_frame
        }),
        "metric calibration markers must resolve to strictly increasing unique RGB frames"
    );

    let mut rgb: Vec<Pr405RgbAuthorRow> = read_bounded_jsonl(frames_path, true)?;
    let selected_session_frames = markers
        .iter()
        .map(|marker| marker.session_frame)
        .collect::<Vec<_>>();
    resolve_metric_rgb_present_identities(
        session_dir,
        pose_report_path,
        &mut rgb,
        &selected_session_frames,
    )?;
    let rgb_by_frame = rgb
        .iter()
        .map(|row| (row.session_frame, row))
        .collect::<std::collections::BTreeMap<_, _>>();
    let selected = markers
        .iter()
        .map(|marker| {
            let frame = rgb_by_frame
                .get(&marker.session_frame)
                .context("F9 marker resolved outside the finalized RGB frame range")?;
            let present_seq = frame
                .present_seq
                .context("F9 RGB frame has no decoded full Present identity")?;
            let present_qpc = frame
                .present_qpc
                .context("F9 RGB frame has no exact Present QPC")?;
            Ok((marker.session_frame, present_seq, present_qpc))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let wanted = selected
        .iter()
        .map(|(_, present_seq, _)| *present_seq)
        .collect::<std::collections::BTreeSet<_>>();

    let pose: Value =
        serde_json::from_slice(&read_bounded_file(pose_report_path, MAX_JSONL_BYTES)?)?;
    ensure!(
        pose.get("schema").and_then(Value::as_str) == Some(POSE_REPORT_SCHEMA),
        "metric calibration player report schema is invalid"
    );
    ensure!(
        pose.get("process_launch_id").and_then(Value::as_str) == Some(process_launch_id),
        "metric calibration player report belongs to another process launch"
    );
    ensure!(
        pose.pointer("/player_abi_audit/evidence_id")
            .and_then(Value::as_str)
            == Some("cp2077-steam-20383525-player-spawn-abi-v1"),
        "metric calibration player report has no exact loaded-image ABI evidence"
    );
    let admitted_player_resolvers = pose
        .get("resolvers")
        .and_then(Value::as_array)
        .context("metric calibration player report has no resolvers")?
        .iter()
        .filter(|resolver| {
            resolver.get("role").and_then(Value::as_str) == Some("player")
                && resolver.get("source").and_then(Value::as_str)
                    == Some("first_party_player_spawn_event")
                && resolver.pointer("/hook_abi/status").and_then(Value::as_str) == Some("validated")
                && resolver
                    .pointer("/hook_abi/original_forwarded_exactly_once")
                    .and_then(Value::as_bool)
                    == Some(true)
        })
        .filter_map(|resolver| resolver.get("resolver_id").and_then(Value::as_str))
        .collect::<std::collections::BTreeSet<_>>();
    ensure!(
        admitted_player_resolvers.len() == 1,
        "metric calibration requires one audited first-party player-event resolver"
    );
    let critical_blockers = pose
        .get("blockers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|blocker| blocker.get("code").and_then(Value::as_str))
        .filter(|code| {
            matches!(
                *code,
                "player_snapshot_asynchronous_to_selected_present"
                    | "render_present_inputs_dropped"
                    | "same_present_player_snapshot_rejected"
                    | "player_virtual_event_hook_install_failed"
            )
        })
        .collect::<Vec<_>>();
    ensure!(
        critical_blockers.is_empty(),
        "metric calibration player report carries critical blockers: {}",
        critical_blockers.join(", ")
    );
    let mut player_key = None::<(String, u64, u64)>;
    let mut players = std::collections::BTreeMap::<u64, (i64, [f64; 3])>::new();
    for snapshot in pose
        .get("snapshot_observations")
        .and_then(Value::as_array)
        .context("metric calibration player report has no snapshots")?
    {
        if snapshot.get("role").and_then(Value::as_str) != Some("player") {
            continue;
        }
        let present_seq = snapshot
            .get("present_seq")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if !wanted.contains(&present_seq) {
            continue;
        }
        ensure!(snapshot.get("candidates_truncated") == Some(&Value::Bool(false)));
        let candidates = snapshot
            .get("candidates")
            .and_then(Value::as_array)
            .context("metric calibration player snapshot has no candidates")?;
        ensure!(
            candidates.len() == 1,
            "metric calibration player pose is not unique for a sampled Present"
        );
        let candidate = &candidates[0];
        let key = (
            snapshot
                .get("resolver_id")
                .and_then(Value::as_str)
                .context("metric calibration player resolver is missing")?
                .to_owned(),
            snapshot
                .get("object_tag")
                .and_then(Value::as_u64)
                .context("metric calibration player object tag is missing")?,
            candidate
                .get("transform_offset")
                .and_then(Value::as_u64)
                .context("metric calibration player transform offset is missing")?,
        );
        ensure!(
            admitted_player_resolvers.contains(key.0.as_str()),
            "metric calibration player snapshot did not use the audited player-event resolver"
        );
        if let Some(expected) = &player_key {
            ensure!(
                expected == &key,
                "metric calibration player resolver/object/offset changed"
            );
        } else {
            player_key = Some(key);
        }
        let qpc = snapshot
            .get("qpc")
            .and_then(Value::as_i64)
            .context("metric calibration player QPC is missing")?;
        let position: [f64; 3] = serde_json::from_value(
            candidate
                .get("position_world_units")
                .cloned()
                .context("metric calibration player position is missing")?,
        )?;
        ensure!(qpc > 0 && position.iter().all(|value| value.is_finite()));
        if let Some(previous) = players.insert(present_seq, (qpc, position)) {
            ensure!(
                previous == (qpc, position),
                "metric calibration player pose changed within one Present"
            );
        }
    }
    ensure!(
        player_key.is_some(),
        "metric calibration selected no player identity"
    );

    let screenshots = extract_metric_screenshots(
        session_dir,
        video,
        &selected.iter().map(|row| row.0).collect::<Vec<_>>(),
    )?;
    let mut pose_rows = Vec::with_capacity(selected.len());
    let mut frame_rows = Vec::with_capacity(selected.len());
    let mut observation_samples = Vec::with_capacity(selected.len());
    for ((session_frame, present_seq, present_qpc), screenshot) in selected.iter().zip(&screenshots)
    {
        let (player_qpc, player_position) = players
            .get(present_seq)
            .context("F9 sample has no exact player pose Present")?;
        ensure!(
            player_qpc == present_qpc,
            "F9 sample RGB/player Present QPC does not match exactly"
        );
        let identity = depth_lab::cp2077_metric_evidence::MetricPresentIdentity {
            present_seq: *present_seq,
            qpc: *present_qpc,
        };
        pose_rows.push(depth_lab::cp2077_metric_evidence::MetricPoseRow {
            schema: depth_lab::cp2077_metric_evidence::CP2077_METRIC_POSE_SCHEMA_V1.into(),
            session_frame: *session_frame,
            present_seq: *present_seq,
            qpc: *present_qpc,
            player_position_world_units: *player_position,
        });
        frame_rows.push(depth_lab::cp2077_metric_evidence::MetricFrameRow {
            schema: depth_lab::cp2077_metric_evidence::CP2077_METRIC_FRAME_SCHEMA_V1.into(),
            session_frame: *session_frame,
            rgb: identity,
            player: identity,
        });
        observation_samples.push(json!({
            "session_frame": session_frame,
            "present_seq": present_seq,
            "qpc": present_qpc,
            "screenshot_file": screenshot,
            "ocr_text": "",
            "objective_distance_metres": 0
        }));
    }
    let pose_path = session_dir.join(METRIC_POSE_FILE);
    let frame_path = session_dir.join(METRIC_FRAME_FILE);
    write_two_jsonl_atomically(&pose_path, &pose_rows, &frame_path, &frame_rows)?;
    let template_path = session_dir.join(METRIC_OBSERVATIONS_TEMPLATE_FILE);
    write_json_create_new(
        &template_path,
        &json!({
            "schema": depth_lab::cp2077_metric_evidence::CP2077_HUD_METRIC_OBSERVATIONS_SCHEMA_V2,
            "non_trainable": true,
            "local_only": true,
            "steam_build_id": 20_383_525,
            "executable_sha256": CP2077_EXE_SHA256,
            "calibration_session_id": session_id,
            "calibration_run_id": run_id,
            "capture_operator_id": "REPLACE_WITH_CAPTURE_OPERATOR_ID",
            "hud_visible_for_calibration_only": true,
            "objective_label": "REVIEWER_REPLACE_WITH_ONE_UNCHANGED_OBJECTIVE_LABEL",
            "motion_protocol": "straight_radial_same_objective",
            "samples": observation_samples,
            "review": {
                "method": "independent_human_transcription",
                "reviewer_id": "REPLACE_WITH_DIFFERENT_REVIEWER_ID",
                "reviewed_utc": "REPLACE_WITH_UTC_TIMESTAMP",
                "every_screenshot_reviewed": false,
                "same_objective_confirmed": false
            }
        }),
    )?;
    write_json_create_new(
        &session_dir.join(METRIC_CAPTURE_REPORT_FILE),
        &json!({
            "schema": "dm-cp2077-metric-calibration-capture/v1",
            "status": "awaiting_independent_hud_review",
            "session_id": session_id,
            "run_id": run_id,
            "sample_count": selected.len(),
            "sample_hotkey": "Ctrl+Alt+F9",
            "pose": { "file": METRIC_POSE_FILE, "sha256": sha256_file(&pose_path)? },
            "frame_evidence": { "file": METRIC_FRAME_FILE, "sha256": sha256_file(&frame_path)? },
            "review_template": {
                "file": METRIC_OBSERVATIONS_TEMPLATE_FILE,
                "sha256": sha256_file(&template_path)?,
                "instruction": "A second reviewer must copy this template to hud-metric-observations.json, transcribe each PNG whole-metre HUD reading and enter one unchanged objective label. Zero/blank placeholders are intentionally invalid."
            },
            "screenshots": screenshots.iter().map(|file| json!({
                "file": file,
                "sha256": sha256_file(&session_dir.join(file)).expect("exported screenshot remains readable")
            })).collect::<Vec<_>>(),
            "metric_evidence_sealed": false,
            "non_trainable": true,
            "local_only": true,
            "upload_allowed": false,
            "recording_ready": false
        }),
    )?;
    Ok(selected.len())
}

fn strip_windows_verbatim_prefix(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let text = path.as_os_str().to_string_lossy();
        if let Some(rest) = text.strip_prefix(r"\\?\") {
            return PathBuf::from(rest);
        }
    }
    path.to_path_buf()
}

/// Signed bound on `rgb_source_qpc - depth_present_qpc` for one exact pair.
///
/// `capture-core` stamps every RGB frame with the WGC `SystemRelativeTime` that
/// "identifies when the compositor rendered a frame", converted into the QPC
/// domain, while the probe stamps depth inside the game's own Present hook. The
/// compositor is downstream of the game, so a correct pair always carries the
/// RGB stamp *later* by the present-queue, flip, composition and delivery
/// latency. That lag is one-sided and positive; it is not jitter around zero.
///
/// Pair identity does not depend on this window and never did: the RGB
/// watermark decodes to the depth row's `present_seq` low16, and
/// `low16_unambiguous_within_session` refuses any run whose span could alias
/// that identity. The window only has to keep a watermark misdecode from
/// landing on a valid-looking Present, so it is bounded in units of the run's
/// own measured Present interval instead of a fixed target rate.
///
/// Deriving the bound from `ISSUE735_TARGET_DEPTH_HZ` assumed the game presents
/// at 16 Hz. A slower run makes that symmetric 62.5 ms window narrower than a
/// single Present interval *and* centres it on a lag that is never zero, so it
/// rejects correctly identified pairs: the 2026-09-10 attempt measured a
/// +57.3 ms median lag at 13.38 Hz and lost 323 of 843 pairs to it. Frame-count
/// and coverage contracts are unchanged; only this predicate is corrected.
#[derive(Clone, Copy, Debug)]
struct RgbDepthPairingWindow {
    /// Largest tolerated RGB-before-depth delta. Causally impossible for a
    /// correct pair, so this is slack for timestamp semantics rather than an
    /// expected range; it stays at the historical bound instead of being
    /// tightened on evidence a single run cannot supply.
    lead_ticks: i64,
    /// Largest tolerated RGB-after-depth delta.
    lag_ticks: i64,
}

impl RgbDepthPairingWindow {
    fn accepts(self, rgb_minus_depth: i64) -> bool {
        rgb_minus_depth >= -self.lead_ticks && rgb_minus_depth <= self.lag_ticks
    }
}

/// Median interval between consecutive Present stamps, or `None` when the run
/// is too short to measure one. The median keeps a stall or a reordered stamp
/// from widening the window.
fn median_present_interval_ticks(present_qpcs: &[i64]) -> Option<i64> {
    let mut intervals: Vec<i64> = present_qpcs
        .windows(2)
        .map(|pair| pair[1].saturating_sub(pair[0]))
        .filter(|interval| *interval > 0)
        .collect();
    if intervals.is_empty() {
        return None;
    }
    intervals.sort_unstable();
    Some(intervals[intervals.len() / 2])
}

fn rgb_depth_pairing_window(qpc_frequency: i64, present_qpcs: &[i64]) -> RgbDepthPairingWindow {
    // Without a frequency the stamps have no scale, so admit nothing rather
    // than letting a measured cadence widen the window on its own. Callers
    // report this as a blocker; the gate refuses it outright.
    if qpc_frequency <= 0 {
        return RgbDepthPairingWindow {
            lead_ticks: 0,
            lag_ticks: 0,
        };
    }
    let target_rate_ticks = qpc_frequency / i64::try_from(ISSUE735_TARGET_DEPTH_HZ).unwrap_or(1);
    // Three intervals cover a deep present queue plus DWM composition and WGC
    // delivery. A run that presents faster than the target rate keeps the
    // historical bound, so this can never be narrower than the previous window.
    let cadence_ticks = median_present_interval_ticks(present_qpcs)
        .unwrap_or(0)
        .saturating_mul(RGB_COMPOSITE_LAG_PRESENT_INTERVALS);
    RgbDepthPairingWindow {
        lead_ticks: target_rate_ticks,
        lag_ticks: target_rate_ticks.max(cadence_ticks),
    }
}

fn pair_rgb_depth(
    rgb_frame_count: usize,
    decoded: &[DecodedRgbFrame],
    depth: &[RawDepthPairingRow],
    qpc_frequency: i64,
) -> RawRgbDepthPairingReport {
    let decoded_rate = if rgb_frame_count == 0 {
        0.0
    } else {
        decoded.len() as f64 / rgb_frame_count as f64
    };
    let span = depth
        .last()
        .and_then(|last| {
            depth
                .first()
                .map(|first| last.present_seq.saturating_sub(first.present_seq))
        })
        .unwrap_or(0);
    let unambiguous = span < u64::from(u16::MAX);
    let present_qpcs: Vec<i64> = depth.iter().map(|row| row.present_qpc).collect();
    let present_interval = median_present_interval_ticks(&present_qpcs).unwrap_or(0);
    let window = rgb_depth_pairing_window(qpc_frequency, &present_qpcs);
    // Retain the signed compositor lag of every accepted pair. The 2026-09-10
    // failure was only diagnosable because an offline pass recomputed this
    // distribution by hand; recording it here keeps the assumption observable
    // in the run's own report instead of hiding it behind a pass/fail count.
    //
    // A duplicated source frame decodes the same watermark into several RGB
    // slots, so one depth row can have several candidates. This accepts the row
    // when *any* candidate lands in the window, which is what
    // `materialize_issue735_exact_depth` has always done; the diagnostic used
    // to require the nearest candidate instead. The two agreed as long as the
    // window was symmetric and the lag one-sided, and they must agree now that
    // it is not.
    let mut lags = Vec::new();
    for row in depth {
        let low16 = row.present_seq & u64::from(u16::MAX);
        let best = decoded
            .iter()
            .filter(|frame| frame.present_low16 == low16)
            .map(|frame| frame.source_qpc.saturating_sub(row.present_qpc))
            .filter(|delta| window.accepts(*delta))
            .min_by_key(|delta| delta.abs());
        if let Some(delta) = best {
            lags.push(delta);
        }
    }
    let exact_matches = lags.len();
    let rgb_before_depth_pairs = lags.iter().filter(|delta| **delta < 0).count();
    lags.sort_unstable();
    let min_lag = lags.first().copied().unwrap_or(0);
    let max_lag = lags.last().copied().unwrap_or(0);
    let median_lag = lags.get(lags.len() / 2).copied().unwrap_or(0);
    let coverage = if depth.is_empty() {
        0.0
    } else {
        exact_matches as f64 / depth.len() as f64
    };
    let mut blockers = Vec::new();
    if rgb_frame_count == 0 || depth.is_empty() || qpc_frequency <= 0 {
        blockers.push("pairing_inputs_invalid_or_empty".to_owned());
    }
    if decoded_rate < depth_lab::pipeline::MIN_WATERMARK_RATE {
        blockers.push(format!(
            "rgb_watermark_decode_rate_too_low:{decoded_rate:.6}"
        ));
    }
    if !unambiguous {
        blockers.push(format!("present_low16_wrap_ambiguous:span={span}"));
    }
    if coverage < MIN_EXACT_DEPTH_RGB_COVERAGE {
        blockers.push(format!("exact_depth_rgb_coverage_too_low:{coverage:.6}"));
    }
    RawRgbDepthPairingReport {
        schema: PAIRING_SCHEMA,
        passed: blockers.is_empty(),
        rgb_frame_count,
        decoded_rgb_watermarks: decoded.len(),
        decoded_rgb_watermark_rate: decoded_rate,
        depth_frame_count: depth.len(),
        exact_present_matches: exact_matches,
        exact_depth_rgb_coverage: coverage,
        measured_present_interval_ticks: present_interval,
        pairing_window_lead_ticks: window.lead_ticks,
        pairing_window_lag_ticks: window.lag_ticks,
        min_compositor_lag_ticks: min_lag,
        median_compositor_lag_ticks: median_lag,
        max_compositor_lag_ticks: max_lag,
        rgb_before_depth_pairs,
        unmatched_depth_present_count: depth.len().saturating_sub(exact_matches),
        low16_unambiguous_within_session: unambiguous,
        blockers,
        non_trainable: true,
        recording_ready: false,
        even_cfr: None,
    }
}

fn materialize_pr405_selected_depth(session_dir: &Path, frames_path: &Path) -> anyhow::Result<u64> {
    const RGB_ROWS: usize = 900;
    const DEPTH_ROWS: usize = 450;
    let rgb: Vec<Pr405RgbAuthorRow> = read_bounded_jsonl(frames_path, true)?;
    let source: Vec<RawDepthSelectionRow> =
        read_bounded_jsonl(&session_dir.join("raw-depth-index.jsonl"), false)?;
    let samples: Vec<WatermarkSampleRow> =
        read_bounded_jsonl(&session_dir.join(WATERMARK_SAMPLES_FILE), false)?;
    ensure!(
        rgb.len() == RGB_ROWS,
        "PR405 selected-depth compaction requires exactly 900 RGB rows"
    );
    ensure!(
        !source.is_empty(),
        "PR405 selected-depth source index is empty"
    );
    ensure!(
        source
            .iter()
            .map(|row| row.present_seq)
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            == source.len(),
        "PR405 source depth Present identities are not unique"
    );
    let sample_by_frame = samples
        .into_iter()
        .map(|sample| (sample.session_frame, sample.block_means))
        .collect::<std::collections::BTreeMap<_, _>>();
    let by_low16 = source.iter().fold(
        std::collections::BTreeMap::<u16, Vec<&RawDepthSelectionRow>>::new(),
        |mut map, row| {
            map.entry(row.present_seq as u16).or_default().push(row);
            map
        },
    );

    let source_dir = session_dir.join("raw-depth");
    let selected_dir = session_dir.join("raw-depth-selected");
    fs::create_dir(&selected_dir)?;
    let selected_index = session_dir.join("raw-depth-index-selected.jsonl");
    let mut index = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&selected_index)?;
    for (selected_ordinal, frame) in rgb
        .iter()
        .enumerate()
        .filter(|(index, _)| index.is_multiple_of(2))
    {
        let present_low16 = decode_pr405_watermark_with_duplicate_fallback(
            &rgb,
            selected_ordinal,
            &sample_by_frame,
        )?;
        let candidates = by_low16
            .get(&present_low16)
            .context("PR405 even RGB watermark has no retained raw depth")?;
        let plausible = candidates
            .iter()
            .copied()
            .filter(|row| frame.source_qpc.abs_diff(row.present_qpc) <= 1_000_000)
            .collect::<Vec<_>>();
        ensure!(
            plausible.len() == 1,
            "PR405 even RGB watermark does not resolve to one QPC-plausible raw depth"
        );
        let source_row = plausible[0];
        ensure!(
            frame.source_qpc.abs_diff(source_row.present_qpc) <= 1_000_000,
            "PR405 selected RGB/depth QPC anchors are implausibly far apart"
        );
        let name = format!("{:012}.d32", selected_ordinal / 2);
        fs::hard_link(source_dir.join(&source_row.file), selected_dir.join(&name))?;
        let selected = RawDepthSelectionRow {
            schema: source_row.schema.clone(),
            ordinal: (selected_ordinal / 2) as u64,
            file: name,
            present_seq: source_row.present_seq,
            present_qpc: source_row.present_qpc,
            current_present_seq: source_row.current_present_seq,
            readback_lag_presents: source_row.readback_lag_presents,
            capture_id: source_row.capture_id,
            source_candidate_id: source_row.source_candidate_id.clone(),
            runtime_candidate_id: source_row.runtime_candidate_id.clone(),
            arm_request_observed_qpc: source_row.arm_request_observed_qpc,
            arm_start_present_seq: source_row.arm_start_present_seq,
            width: source_row.width,
            height: source_row.height,
            row_pitch: source_row.row_pitch,
            pixel_format: source_row.pixel_format.clone(),
            byte_len: source_row.byte_len,
            rgb_pairing: source_row.rgb_pairing.clone(),
        };
        serde_json::to_writer(&mut index, &selected)?;
        index.write_all(b"\n")?;
    }
    index.sync_all()?;
    ensure!(
        selected_ordinal_count(&rgb) == DEPTH_ROWS,
        "PR405 selected-depth set is not exactly 450 rows"
    );

    // Preserve the every-Present source set under explicit audit names, then
    // atomically publish the exact 450-row set at the canonical paths consumed
    // by QC and the PR405 packager.
    fs::rename(
        session_dir.join("raw-depth-index.jsonl"),
        session_dir.join("raw-depth-index-all-presents.jsonl"),
    )?;
    fs::rename(&source_dir, session_dir.join("raw-depth-all-presents"))?;
    fs::rename(&selected_index, session_dir.join("raw-depth-index.jsonl"))?;
    fs::rename(&selected_dir, session_dir.join("raw-depth"))?;
    Ok(DEPTH_ROWS as u64)
}

fn materialize_issue735_exact_depth(
    session_dir: &Path,
    frames_path: &Path,
    qpc_frequency: i64,
    contract: CaptureContract,
) -> anyhow::Result<u64> {
    ensure!(
        qpc_frequency > 0,
        "issue735 depth selection has no QPC frequency"
    );
    let rgb: Vec<Pr405RgbAuthorRow> = read_bounded_jsonl(frames_path, true)?;
    let source: Vec<RawDepthSelectionRow> =
        read_bounded_jsonl(&session_dir.join("raw-depth-index.jsonl"), false)?;
    let samples: Vec<WatermarkSampleRow> =
        read_bounded_jsonl(&session_dir.join(WATERMARK_SAMPLES_FILE), false)?;
    ensure!(!rgb.is_empty() && !source.is_empty());
    let source_span = source
        .last()
        .expect("source is nonempty")
        .present_seq
        .saturating_sub(source[0].present_seq);
    ensure!(
        source_span < u64::from(u16::MAX),
        "issue735 source Present low16 wraps ambiguously"
    );
    let rgb_qpc = rgb
        .iter()
        .map(|row| (row.session_frame, row.source_qpc))
        .collect::<std::collections::BTreeMap<_, _>>();
    let mut decoded = std::collections::BTreeMap::<u16, Vec<i64>>::new();
    for sample in samples {
        let Some(qpc) = rgb_qpc.get(&sample.session_frame).copied() else {
            continue;
        };
        if let Ok(low16) = depth_lab::watermark::decode(&sample.block_means, 32) {
            decoded.entry(low16).or_default().push(qpc);
        }
    }
    // Identity is already exact here: `decoded` is keyed by the watermark that
    // decoded out of the encoded RGB pixels, and the `source_span` check above
    // refuses a run whose low16 could alias. The window below only rejects a
    // misdecode that happens to land on a valid-looking Present, so it is
    // measured against this run's own cadence rather than a fixed target rate.
    let window = rgb_depth_pairing_window(
        qpc_frequency,
        &source.iter().map(|row| row.present_qpc).collect::<Vec<_>>(),
    );
    let selected = source
        .iter()
        .filter(|row| {
            decoded.get(&(row.present_seq as u16)).is_some_and(|qpcs| {
                qpcs.iter()
                    .any(|qpc| window.accepts(qpc.saturating_sub(row.present_qpc)))
            })
        })
        .collect::<Vec<_>>();
    ensure!(
        (contract.min_depth_frames..=contract.max_depth_frames).contains(&(selected.len() as u64)),
        "issue735 exact-Present selected depth count {} is outside {}..={}",
        selected.len(),
        contract.min_depth_frames,
        contract.max_depth_frames
    );
    let selected_span_ms = selected
        .last()
        .expect("selected depth is nonempty")
        .present_qpc
        .saturating_sub(selected[0].present_qpc)
        .saturating_mul(1_000)
        / qpc_frequency;
    ensure!(
        (contract.min_duration_ms.saturating_sub(1_000)..=contract.max_duration_ms)
            .contains(&(selected_span_ms as u64)),
        "issue735 exact-Present selected depth span {selected_span_ms} ms is outside the selected contract"
    );

    let source_dir = session_dir.join("raw-depth");
    let selected_dir = session_dir.join("raw-depth-selected");
    fs::create_dir(&selected_dir)?;
    let selected_index = session_dir.join("raw-depth-index-selected.jsonl");
    let mut index = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&selected_index)?;
    for (ordinal, source_row) in selected.iter().enumerate() {
        let name = format!("{ordinal:012}.d32");
        fs::hard_link(source_dir.join(&source_row.file), selected_dir.join(&name))?;
        let selected_row = RawDepthSelectionRow {
            schema: source_row.schema.clone(),
            ordinal: ordinal as u64,
            file: name,
            present_seq: source_row.present_seq,
            present_qpc: source_row.present_qpc,
            current_present_seq: source_row.current_present_seq,
            readback_lag_presents: source_row.readback_lag_presents,
            capture_id: source_row.capture_id,
            source_candidate_id: source_row.source_candidate_id.clone(),
            runtime_candidate_id: source_row.runtime_candidate_id.clone(),
            arm_request_observed_qpc: source_row.arm_request_observed_qpc,
            arm_start_present_seq: source_row.arm_start_present_seq,
            width: source_row.width,
            height: source_row.height,
            row_pitch: source_row.row_pitch,
            pixel_format: source_row.pixel_format.clone(),
            byte_len: source_row.byte_len,
            rgb_pairing: source_row.rgb_pairing.clone(),
        };
        serde_json::to_writer(&mut index, &selected_row)?;
        index.write_all(b"\n")?;
    }
    index.sync_all()?;
    fs::rename(
        session_dir.join("raw-depth-index.jsonl"),
        session_dir.join("raw-depth-index-all-presents.jsonl"),
    )?;
    fs::rename(&source_dir, session_dir.join("raw-depth-all-presents"))?;
    fs::rename(&selected_index, session_dir.join("raw-depth-index.jsonl"))?;
    fs::rename(&selected_dir, session_dir.join("raw-depth"))?;
    Ok(selected.len() as u64)
}

fn selected_ordinal_count(rgb: &[Pr405RgbAuthorRow]) -> usize {
    rgb.iter()
        .enumerate()
        .filter(|(index, _)| index.is_multiple_of(2))
        .count()
}

fn resolve_pr405_rgb_present_identities(
    session_dir: &Path,
    rgb: &mut [Pr405RgbAuthorRow],
) -> anyhow::Result<()> {
    let samples: Vec<WatermarkSampleRow> =
        read_bounded_jsonl(&session_dir.join(WATERMARK_SAMPLES_FILE), false)?;
    let all_depth: Vec<RawDepthSelectionRow> = read_bounded_jsonl(
        &session_dir.join("raw-depth-index-all-presents.jsonl"),
        false,
    )?;
    let sample_by_frame = samples
        .into_iter()
        .map(|sample| (sample.session_frame, sample.block_means))
        .collect::<std::collections::BTreeMap<_, _>>();
    let by_low16 = all_depth.iter().fold(
        std::collections::BTreeMap::<u16, Vec<&RawDepthSelectionRow>>::new(),
        |mut map, row| {
            map.entry(row.present_seq as u16).or_default().push(row);
            map
        },
    );
    for index in 0..rgb.len() {
        let low16 = decode_pr405_watermark_with_duplicate_fallback(rgb, index, &sample_by_frame)?;
        let frame = &mut rgb[index];
        let plausible = by_low16
            .get(&low16)
            .into_iter()
            .flatten()
            .copied()
            .filter(|row| frame.source_qpc.abs_diff(row.present_qpc) <= 1_000_000)
            .collect::<Vec<_>>();
        ensure!(
            plausible.len() == 1,
            "PR405 RGB watermark does not resolve to one QPC-plausible Present"
        );
        frame.present_seq = Some(plausible[0].present_seq);
        frame.present_qpc = Some(plausible[0].present_qpc);
    }
    Ok(())
}

fn resolve_metric_rgb_present_identities(
    session_dir: &Path,
    pose_report_path: &Path,
    rgb: &mut [Pr405RgbAuthorRow],
    selected_session_frames: &[u64],
) -> anyhow::Result<()> {
    let samples: Vec<WatermarkSampleRow> =
        read_bounded_jsonl(&session_dir.join(WATERMARK_SAMPLES_FILE), false)?;
    let sample_by_frame = samples
        .into_iter()
        .map(|sample| (sample.session_frame, sample.block_means))
        .collect::<std::collections::BTreeMap<_, _>>();
    let pose: Value =
        serde_json::from_slice(&read_bounded_file(pose_report_path, MAX_JSONL_BYTES)?)?;
    let mut player_present_by_low16 =
        std::collections::BTreeMap::<u16, std::collections::BTreeSet<(u64, i64)>>::new();
    for snapshot in pose
        .get("snapshot_observations")
        .and_then(Value::as_array)
        .context("metric calibration player report has no snapshots")?
    {
        if snapshot.get("role").and_then(Value::as_str) != Some("player") {
            continue;
        }
        let Some(present_seq) = snapshot.get("present_seq").and_then(Value::as_u64) else {
            continue;
        };
        let Some(qpc) = snapshot.get("qpc").and_then(Value::as_i64) else {
            continue;
        };
        if present_seq == 0 || qpc <= 0 {
            continue;
        }
        player_present_by_low16
            .entry(present_seq as u16)
            .or_default()
            .insert((present_seq, qpc));
    }
    resolve_metric_selected_rgb_present_identities(
        rgb,
        selected_session_frames,
        &sample_by_frame,
        &player_present_by_low16,
    )
}

fn resolve_metric_selected_rgb_present_identities(
    rgb: &mut [Pr405RgbAuthorRow],
    selected_session_frames: &[u64],
    sample_by_frame: &std::collections::BTreeMap<u64, Vec<u8>>,
    player_present_by_low16: &std::collections::BTreeMap<
        u16,
        std::collections::BTreeSet<(u64, i64)>,
    >,
) -> anyhow::Result<()> {
    for session_frame in selected_session_frames {
        let index = rgb
            .iter()
            .position(|row| row.session_frame == *session_frame)
            .context("F9 marker resolved outside the finalized RGB frame range")?;
        let low16 = decode_pr405_watermark_with_duplicate_fallback(rgb, index, sample_by_frame)?;
        let source_qpc = rgb[index].source_qpc;
        let plausible = player_present_by_low16
            .get(&low16)
            .into_iter()
            .flatten()
            .copied()
            .filter(|(_, qpc)| source_qpc.abs_diff(*qpc) <= METRIC_PRESENT_IDENTITY_MAX_QPC_DELTA)
            .collect::<Vec<_>>();
        ensure!(
            plausible.len() == 1,
            "metric calibration RGB watermark does not resolve to one QPC-plausible player Present"
        );
        rgb[index].present_seq = Some(plausible[0].0);
        rgb[index].present_qpc = Some(plausible[0].1);
    }
    Ok(())
}

fn decode_pr405_watermark_with_duplicate_fallback(
    rgb: &[Pr405RgbAuthorRow],
    index: usize,
    samples: &std::collections::BTreeMap<u64, Vec<u8>>,
) -> anyhow::Result<u16> {
    let decode_at = |candidate: usize| -> Option<u16> {
        samples
            .get(&rgb[candidate].session_frame)
            .and_then(|means| depth_lab::watermark::decode(means, 32).ok())
    };
    if let Some(value) = decode_at(index) {
        return Ok(value);
    }
    // WGC can emit one pre-watermark startup frame. A duplicate run has one
    // source Present by definition, so only borrow from an adjacent row that
    // the recorder itself marked duplicate. Never interpolate across motion.
    if rgb[index].duplicate
        && index > 0
        && let Some(value) = decode_at(index - 1)
    {
        return Ok(value);
    }
    if index + 1 < rgb.len()
        && rgb[index + 1].duplicate
        && let Some(value) = decode_at(index + 1)
    {
        return Ok(value);
    }
    // The recorder starts on its own QPC cadence. Its first output can precede
    // the first completed GPU watermark submission. Only that startup row may
    // bind to the next decoded Present, and only inside one 30 Hz interval.
    if index == 0
        && rgb.len() > 1
        && rgb[0].source_qpc.abs_diff(rgb[1].source_qpc) <= 400_000
        && let Some(value) = decode_at(1)
    {
        return Ok(value);
    }
    anyhow::bail!("PR405 RGB watermark did not decode and has no duplicate-frame fallback")
}

fn author_pr405_frame_and_pose_evidence(
    session_dir: &Path,
    frames_path: &Path,
    process_launch_id: &str,
    qpc_frequency: i64,
    autonomous_input: bool,
) -> anyhow::Result<Pr405AuthoredEvidence> {
    const FRAME_FILE: &str = "cp2077-pr405-frame-evidence.jsonl";
    const POSE_FILE: &str = "cp2077-final-pose.jsonl";
    const PACKAGE_INPUT_FILE: &str = "cp2077-pr405-package-input.json";
    const FRAME_SCHEMA: &str = "dm-cp2077-pr405-frame-evidence/v1";
    const DRAW_SOURCE: &str = "copy_buffer_region_root_signature_declared_cbv_at_selected_dsv_draw";
    const MATRIX_SOURCE: &str = "copy_buffer_region_root_signature_declared_cbv";
    const RGB_ROWS: usize = 900;
    const DEPTH_ROWS: usize = 450;

    #[derive(Deserialize)]
    struct MetricInput {
        qpc_frequency_hz: u64,
        metric_calibration: depth_lab::calibration::MetricScaleCalibration,
    }

    ensure!(
        qpc_frequency > 0,
        "PR405 authoring QPC frequency is invalid"
    );
    let metric_input: MetricInput = serde_json::from_slice(&read_bounded_file(
        &session_dir.join(PACKAGE_INPUT_FILE),
        1024 * 1024,
    )?)?;
    metric_input
        .metric_calibration
        .validate()
        .map_err(|error| anyhow::anyhow!("PR405 metric calibration is not valid: {error}"))?;
    ensure!(
        metric_input.qpc_frequency_hz == u64::try_from(qpc_frequency)?,
        "PR405 package input QPC frequency does not match the recorder"
    );
    let metres_per_world_unit = metric_input.metric_calibration.value;

    let mut rgb: Vec<Pr405RgbAuthorRow> = read_bounded_jsonl(frames_path, true)?;
    resolve_pr405_rgb_present_identities(session_dir, &mut rgb)?;
    let autonomous_rows = if autonomous_input {
        Some(read_bounded_jsonl::<AutonomousInputLedgerRow>(
            &session_dir.join(AUTONOMOUS_INPUT_LEDGER_FILE),
            false,
        )?)
    } else {
        None
    };
    let raw: Vec<Pr405RawAuthorRow> =
        read_bounded_jsonl(&session_dir.join("raw-depth-index.jsonl"), false)?;
    ensure!(
        rgb.len() == RGB_ROWS && raw.len() == DEPTH_ROWS,
        "PR405 authoring requires exactly 900 RGB and 450 raw depth rows"
    );
    for (index, frame) in rgb.iter().enumerate() {
        ensure!(
            frame.session_frame == index as u64,
            "PR405 RGB rows are not contiguous"
        );
        ensure!(
            frame.present_seq.is_some_and(|value| value > 0) && frame.source_qpc > 0,
            "PR405 RGB row lacks an online full Present identity"
        );
        ensure!(
            frame.keys.iter().all(|key| matches!(*key, 0x53 | 0x57)),
            "PR405 action stream contains a key outside the audited W/S route"
        );
        ensure!(
            frame.events.iter().all(|event| {
                event.event_type != "mouse_move"
                    || (event.dx.unwrap_or(0) == 0 && event.dy.unwrap_or(0) == 0)
            }),
            "PR405 local pose authoring refuses non-zero mouse motion without a normalization calibration"
        );
        if frame.duplicate {
            ensure!(
                index > 0 && frame.present_seq == rgb[index - 1].present_seq,
                "PR405 duplicate RGB row changed its Present identity"
            );
        } else if index > 0 {
            ensure!(
                frame.present_seq > rgb[index - 1].present_seq,
                "PR405 non-duplicate RGB Present timeline did not advance"
            );
        }
    }

    let wanted_presents = rgb
        .iter()
        .map(|frame| frame.present_seq.expect("validated Present identity"))
        .collect::<std::collections::BTreeSet<_>>();
    let render = camera_report::read(&session_dir.join(RENDER_CAMERA_REPORT_FILE))?.value;
    ensure!(
        camera_report::supported_schema(&render)
            && render.get("process_launch_id").and_then(Value::as_str) == Some(process_launch_id),
        "PR405 render-camera report identity mismatch"
    );
    let observation = render
        .get("observation")
        .context("PR405 render-camera report has no observation")?;
    ensure!(
        observation.get("candidate_admitted_for_cross_launch_discovery")
            == Some(&Value::Bool(true)),
        "PR405 render-camera candidate was not physically admitted"
    );
    ensure!(
        observation.get("production_camera_binding_emitted") == Some(&Value::Bool(false))
            && observation.get("recording_ready") == Some(&Value::Bool(false))
            && observation
                .get("blockers")
                .and_then(Value::as_array)
                .is_some_and(Vec::is_empty),
        "PR405 render-camera observation weakens its discovery-only safety ceiling"
    );
    ensure!(
        observation
            .get("authoring_blockers")
            .and_then(Value::as_array)
            .is_some_and(Vec::is_empty),
        "PR405 render-camera observation contains authoring blockers"
    );
    type ViewSample = (
        Pr405PresentIdentity,
        depth_lab::camera::Mat4,
        depth_lab::camera::TranslationSlot,
    );
    type ProjectionSample = (Pr405PresentIdentity, depth_lab::camera::Mat4, f64, f64);
    let mut views =
        std::collections::BTreeMap::<String, std::collections::BTreeMap<u64, ViewSample>>::new();
    let mut projections = std::collections::BTreeMap::<
        String,
        std::collections::BTreeMap<u64, ProjectionSample>,
    >::new();
    for draw in observation
        .get("observations")
        .and_then(Value::as_array)
        .context("PR405 render-camera report has no draw observations")?
    {
        let present_seq = draw.get("present_seq").and_then(Value::as_u64).unwrap_or(0);
        // Retain the full physically admitted capture-window timeline. WGC is
        // 30 Hz while the game Present cadence can differ; exact QPC-nearest
        // selection below binds each RGB output without inventing a matrix.
        ensure!(
            draw.get("selected_present_identity_passed") == Some(&Value::Bool(true))
                && draw.get("copy_before_clear_submitted") == Some(&Value::Bool(true))
                && draw.get("physical_source").and_then(Value::as_str) == Some(DRAW_SOURCE),
            "PR405 render draw lacks exact selected-Present CopyBufferRegion provenance"
        );
        let qpc = draw
            .get("present_qpc")
            .and_then(Value::as_i64)
            .context("PR405 render draw has no Present QPC")?;
        ensure!(qpc > 0, "PR405 render draw QPC is invalid");
        let identity = Pr405PresentIdentity { present_seq, qpc };
        for matrix in draw
            .get("matrices")
            .and_then(Value::as_array)
            .context("PR405 render draw has no matrices")?
        {
            ensure!(
                matrix.get("physical_source").and_then(Value::as_str) == Some(MATRIX_SOURCE),
                "PR405 render matrix lacks exact CopyBufferRegion provenance"
            );
            let value: depth_lab::camera::Mat4 = serde_json::from_value(
                matrix
                    .get("matrix")
                    .cloned()
                    .context("PR405 render matrix has no values")?,
            )?;
            ensure!(value.iter().flatten().all(|entry| entry.is_finite()));
            match matrix.get("matrix_kind").and_then(Value::as_str) {
                Some("view") => {
                    let key = render_matrix_source_key(matrix, depth_lab::MatrixRole::View)?;
                    ensure_resolved_pr405_axis_mapping(matrix)?;
                    let translation = match matrix.get("translation_slot").and_then(Value::as_str) {
                        Some("row3") => depth_lab::camera::TranslationSlot::Row3,
                        Some("col3") => depth_lab::camera::TranslationSlot::Col3,
                        _ => anyhow::bail!("PR405 view matrix lacks a translation convention"),
                    };
                    let entry = views.entry(key).or_default();
                    if let Some(previous) =
                        entry.insert(present_seq, (identity, value, translation))
                    {
                        ensure!(
                            previous == (identity, value, translation),
                            "PR405 view source changed within one Present"
                        );
                    }
                }
                Some("projection") => {
                    let key = render_matrix_source_key(matrix, depth_lab::MatrixRole::Projection)?;
                    ensure!(
                        matrix.get("configured_vertical_fov_matches") == Some(&Value::Bool(true)),
                        "PR405 projection does not attest FOV 75"
                    );
                    let a = matrix
                        .get("projection_a")
                        .and_then(Value::as_f64)
                        .context("PR405 projection A missing")?;
                    let b = matrix
                        .get("projection_b")
                        .and_then(Value::as_f64)
                        .context("PR405 projection B missing")?;
                    let derived = depth_lab::camera_track::projection_from_matrix(&value)
                        .context("PR405 projection matrix is not physically usable")?;
                    ensure!(
                        a.is_finite()
                            && b.is_finite()
                            && b > 0.0
                            && derived.is_reversed_z()
                            && (derived.a - a).abs() <= 1e-6
                            && (derived.b - b).abs() <= 1e-6,
                        "PR405 projection report contradicts its matrix"
                    );
                    let entry = projections.entry(key).or_default();
                    if let Some(previous) = entry.insert(present_seq, (identity, value, a, b)) {
                        ensure!(
                            previous == (identity, value, a, b),
                            "PR405 projection source changed within one Present"
                        );
                    }
                }
                _ => {}
            }
        }
    }
    let view_sources = views
        .iter()
        .filter(|(_, samples)| {
            wanted_presents
                .iter()
                .all(|present_seq| samples.contains_key(present_seq))
        })
        .collect::<Vec<_>>();
    let projection_sources = projections
        .iter()
        .filter(|(_, samples)| {
            wanted_presents
                .iter()
                .all(|present_seq| samples.contains_key(present_seq))
        })
        .collect::<Vec<_>>();
    ensure!(
        view_sources.len() == 1 && projection_sources.len() == 1,
        "PR405 requires one unique render view and projection source on every RGB Present"
    );
    let stable_view: depth_lab::D3d12MatrixSourceBinding = serde_json::from_str(view_sources[0].0)?;
    let stable_projection: depth_lab::D3d12MatrixSourceBinding =
        serde_json::from_str(projection_sources[0].0)?;
    ensure!(
        stable_view.descriptor_table == stable_projection.descriptor_table
            && stable_view.cbv_resource_offset == stable_projection.cbv_resource_offset
            && stable_view.declared_cbv_bytes == stable_projection.declared_cbv_bytes
            && stable_view.draw_relation == stable_projection.draw_relation
            && stable_view.matrix_offset != stable_projection.matrix_offset,
        "PR405 render-camera view/projection are not distinct matrices in one stable selected-DSV CBV"
    );
    let view_samples = view_sources[0].1;
    let projection_samples = projection_sources[0].1;

    let pose_report: Value = serde_json::from_slice(&read_bounded_file(
        &session_dir.join(POSE_REPORT_FILE),
        MAX_JSONL_BYTES,
    )?)?;
    ensure!(
        pose_report.get("schema").and_then(Value::as_str) == Some(POSE_REPORT_SCHEMA)
            && pose_report.get("process_launch_id").and_then(Value::as_str)
                == Some(process_launch_id),
        "PR405 player pose report identity mismatch"
    );
    let mut player_key = None::<(String, u64, u64)>;
    let mut players = std::collections::BTreeMap::<u64, Pr405PlayerPose>::new();
    for snapshot in pose_report
        .get("snapshot_observations")
        .and_then(Value::as_array)
        .context("PR405 player pose report has no snapshots")?
    {
        if snapshot.get("role").and_then(Value::as_str) != Some("player") {
            continue;
        }
        let present_seq = snapshot
            .get("present_seq")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if !wanted_presents.contains(&present_seq) {
            continue;
        }
        ensure!(snapshot.get("candidates_truncated") == Some(&Value::Bool(false)));
        let candidates = snapshot
            .get("candidates")
            .and_then(Value::as_array)
            .context("PR405 player snapshot has no candidates")?;
        ensure!(
            candidates.len() == 1,
            "PR405 player pose is not unique for an RGB Present"
        );
        let candidate = &candidates[0];
        let key = (
            snapshot
                .get("resolver_id")
                .and_then(Value::as_str)
                .context("PR405 player resolver missing")?
                .to_owned(),
            snapshot
                .get("object_tag")
                .and_then(Value::as_u64)
                .context("PR405 player object tag missing")?,
            candidate
                .get("transform_offset")
                .and_then(Value::as_u64)
                .context("PR405 player transform offset missing")?,
        );
        if let Some(expected) = &player_key {
            ensure!(
                expected == &key,
                "PR405 player resolver/object/offset changed"
            );
        } else {
            player_key = Some(key);
        }
        let sample = Pr405PlayerPose {
            identity: Pr405PresentIdentity {
                present_seq,
                qpc: snapshot
                    .get("qpc")
                    .and_then(Value::as_i64)
                    .context("PR405 player QPC missing")?,
            },
            position: serde_json::from_value(
                candidate
                    .get("position_world_units")
                    .cloned()
                    .context("PR405 player position missing")?,
            )?,
            rotation: serde_json::from_value(
                candidate
                    .get("raw_quaternion_ijkr")
                    .cloned()
                    .context("PR405 player rotation missing")?,
            )?,
        };
        ensure!(
            sample.identity.qpc > 0
                && sample
                    .position
                    .iter()
                    .chain(&sample.rotation)
                    .all(|value| value.is_finite())
        );
        ensure!(
            (sample
                .rotation
                .iter()
                .map(|value| value * value)
                .sum::<f64>()
                - 1.0)
                .abs()
                <= 0.002
        );
        if let Some(previous) = players.insert(present_seq, sample) {
            ensure!(
                previous.identity == sample.identity
                    && previous.position == sample.position
                    && previous.rotation == sample.rotation,
                "PR405 player pose changed within one Present"
            );
        }
    }
    let mut render_poses = Vec::with_capacity(rgb.len());
    for frame in &rgb {
        let present_seq = frame
            .present_seq
            .context("PR405 RGB frame has no Present identity")?;
        let (view_identity, view, translation) = view_samples
            .get(&present_seq)
            .context("PR405 view missing for exact RGB Present")?;
        let (projection_identity, projection, proj_a, proj_b) = projection_samples
            .get(&present_seq)
            .context("PR405 projection missing for selected view Present")?;
        ensure!(
            view_identity == projection_identity,
            "PR405 view/projection Present identities differ"
        );
        ensure!(
            frame.source_qpc.abs_diff(view_identity.qpc) <= u64::try_from(qpc_frequency / 30)?,
            "PR405 WGC source QPC is farther than one RGB interval from its Present"
        );
        render_poses.push(Pr405RenderPose {
            identity: *view_identity,
            view: *view,
            translation: *translation,
            projection: *projection,
            proj_a: *proj_a,
            proj_b: *proj_b,
        });
    }

    let mut frame_evidence = Vec::<Pr405FrameEvidence>::with_capacity(RGB_ROWS);
    let mut final_pose = Vec::<depth_lab::FinalPoseRow>::with_capacity(RGB_ROWS);
    for (index, (frame, render_pose)) in rgb.iter().zip(&render_poses).enumerate() {
        let present_seq = render_pose.identity.present_seq;
        let player = players
            .get(&present_seq)
            .context("PR405 player pose is missing for the exact RGB Present")?;
        ensure_exact_pr405_identity(player.identity, render_pose.identity, "player pose")?;
        let camera = depth_lab::camera::pose_from_view(&render_pose.view, render_pose.translation);
        let camera_position = camera
            .position
            .map(|value| f64::from(value) * metres_per_world_unit);
        let player_position = player.position.map(|value| value * metres_per_world_unit);
        let camera_rotation_quaternion = camera.rotation.map(f64::from);
        let intrinsics =
            depth_lab::camera::intrinsics_from_projection(&render_pose.projection, 1920.0, 1080.0);
        let camera_speed = pr405_position_speed(&render_poses, index, qpc_frequency, |sample| {
            depth_lab::camera::pose_from_view(&sample.view, sample.translation)
                .position
                .map(|value| f64::from(value) * metres_per_world_unit)
        })?;
        let player_speed = pr405_position_speed(&render_poses, index, qpc_frequency, |sample| {
            players
                .get(&sample.identity.present_seq)
                .expect("exact player pose exists")
                .position
                .map(|value| value * metres_per_world_unit)
        })?;
        let fps = pr405_dynamic_fps(&render_poses, index, qpc_frequency)?;
        let selected_raw = index.is_multiple_of(2).then(|| &raw[index / 2]);
        if let Some(raw) = selected_raw {
            let raw_identity = Pr405PresentIdentity {
                present_seq: raw.present_seq,
                qpc: raw.present_qpc,
            };
            ensure_exact_pr405_identity(raw_identity, render_pose.identity, "raw depth")?;
        }
        let canonical_identity = render_pose.identity;
        let (frame_identity, depth, projection) = if index.is_multiple_of(2) {
            let raw = selected_raw.expect("even frame has selected raw row");
            let raw_identity = Pr405PresentIdentity {
                present_seq: raw.present_seq,
                qpc: raw.present_qpc,
            };
            (
                canonical_identity,
                Some(Pr405DepthBinding {
                    ordinal: raw.ordinal,
                    file: raw.file.clone(),
                    identity: raw_identity,
                }),
                Some(Pr405ProjectionEvidence {
                    proj_a: render_pose.proj_a,
                    proj_b: render_pose.proj_b,
                    reverse_z: true,
                    observed_this_frame: true,
                    physical_source: MATRIX_SOURCE,
                    candidate_admitted: true,
                }),
            )
        } else {
            (canonical_identity, None, None)
        };
        ensure!(
            frame.source_qpc.abs_diff(frame_identity.qpc) <= u64::try_from(qpc_frequency / 30)?,
            "PR405 WGC source QPC is farther than one RGB interval from its authored identity"
        );
        if let Some(previous) = frame_evidence.last() {
            if frame.duplicate {
                ensure!(frame_identity == previous.rgb);
            } else {
                ensure!(
                    frame_identity.present_seq > previous.rgb.present_seq
                        && frame_identity.qpc > previous.rgb.qpc,
                    "PR405 authored RGB identity timeline did not advance"
                );
            }
        }
        frame_evidence.push(Pr405FrameEvidence {
            schema: FRAME_SCHEMA,
            session_frame: index as u64,
            duplicate: frame.duplicate,
            rgb: frame_identity,
            camera: frame_identity,
            player: player.identity,
            depth,
            projection,
        });
        final_pose.push(depth_lab::FinalPoseRow {
            present_seq,
            session_frame: index as u64,
            camera_identity: depth_lab::FinalCameraIdentity::ProbeStream {
                session_id: format!("raw-rgb-{process_launch_id}"),
                run_id: "r0000".into(),
                generation: 1,
            },
            camera_source: depth_lab::CameraPoseSource::ResolvedRenderCameraHybrid,
            time: format!("qpc:{}", render_pose.identity.qpc),
            fps,
            camera_position,
            camera_rotation_quaternion,
            camera_follow_offset: std::array::from_fn(|axis| {
                camera_position[axis] - player_position[axis]
            }),
            camera_intrinsics: intrinsics,
            camera_speed,
            player_position,
            player_rotation_quaternion: player.rotation,
            player_speed,
            mouse_x: 0.5,
            mouse_y: 0.5,
            mouse_dx: 0.0,
            mouse_dy: 0.0,
            key_code: if let Some(rows) = autonomous_rows.as_deref() {
                autonomous_keys_at(frame_identity.qpc, rows)
                    .into_iter()
                    .map(u32::from)
                    .collect()
            } else {
                frame.keys.iter().copied().map(u32::from).collect()
            },
        });
    }
    ensure!(
        raw.len() == DEPTH_ROWS,
        "PR405 raw depth rows do not map one-to-one to even RGB frames"
    );

    let frame_path = session_dir.join(FRAME_FILE);
    let pose_path = session_dir.join(POSE_FILE);
    write_two_jsonl_atomically(&frame_path, &frame_evidence, &pose_path, &final_pose)?;
    Ok(Pr405AuthoredEvidence {
        frame_evidence: FRAME_FILE.into(),
        final_pose: POSE_FILE.into(),
        frame_rows: frame_evidence.len(),
        pose_rows: final_pose.len(),
    })
}

fn ensure_exact_pr405_identity(
    actual: Pr405PresentIdentity,
    expected: Pr405PresentIdentity,
    source: &str,
) -> anyhow::Result<()> {
    ensure!(
        actual == expected,
        "PR405 {source} is not the exact RGB/render Present/QPC"
    );
    Ok(())
}

fn ensure_resolved_pr405_axis_mapping(matrix: &Value) -> anyhow::Result<()> {
    let axis_mapping = matrix
        .get("axis_mapping")
        .and_then(Value::as_str)
        .context("PR405 view matrix has no world-axis mapping")?;
    ensure!(
        axis_mapping == PR405_RESOLVED_AXIS_MAPPING,
        "PR405 view matrix world-axis mapping is not the audited motion calibration"
    );
    Ok(())
}

fn pr405_neighbour(rows: &[Pr405RenderPose], index: usize) -> anyhow::Result<(usize, usize)> {
    let current = rows
        .get(index)
        .context("PR405 pose row index is outside the timeline")?;
    if let Some(next) = rows[index + 1..]
        .iter()
        .position(|row| row.identity.present_seq != current.identity.present_seq)
        .map(|offset| index + 1 + offset)
    {
        Ok((index, next))
    } else if let Some(previous) = rows[..index]
        .iter()
        .rposition(|row| row.identity.present_seq != current.identity.present_seq)
    {
        Ok((previous, index))
    } else {
        anyhow::bail!("PR405 pose timeline has fewer than two distinct Presents")
    }
}

fn pr405_position_speed(
    rows: &[Pr405RenderPose],
    index: usize,
    qpc_frequency: i64,
    position: impl Fn(&Pr405RenderPose) -> [f64; 3],
) -> anyhow::Result<[f64; 3]> {
    let (from, to) = pr405_neighbour(rows, index)?;
    let ticks = rows[to].identity.qpc - rows[from].identity.qpc;
    ensure!(ticks > 0, "PR405 pose QPC timeline did not advance");
    let dt = ticks as f64 / qpc_frequency as f64;
    let from_position = position(&rows[from]);
    let to_position = position(&rows[to]);
    let speed = std::array::from_fn(|axis| (to_position[axis] - from_position[axis]) / dt);
    ensure!(
        speed.iter().all(|value| value.is_finite()),
        "PR405 pose speed is non-finite"
    );
    Ok(speed)
}

fn pr405_dynamic_fps(
    rows: &[Pr405RenderPose],
    index: usize,
    qpc_frequency: i64,
) -> anyhow::Result<f64> {
    let (from, to) = pr405_neighbour(rows, index)?;
    let seq = rows[to].identity.present_seq - rows[from].identity.present_seq;
    let ticks = rows[to].identity.qpc - rows[from].identity.qpc;
    ensure!(seq > 0 && ticks > 0);
    let fps = qpc_frequency as f64 * seq as f64 / ticks as f64;
    ensure!(
        fps.is_finite() && fps >= 30.0,
        "PR405 render camera rate is below 30fps"
    );
    Ok(fps)
}

fn write_two_jsonl_atomically<A: Serialize, B: Serialize>(
    first: &Path,
    first_rows: &[A],
    second: &Path,
    second_rows: &[B],
) -> anyhow::Result<()> {
    ensure!(
        !first.exists() && !second.exists(),
        "PR405 authored evidence already exists"
    );
    let first_temp = first.with_extension(format!("jsonl.{}.tmp", uuid::Uuid::new_v4().simple()));
    let second_temp = second.with_extension(format!("jsonl.{}.tmp", uuid::Uuid::new_v4().simple()));
    let result = (|| {
        write_jsonl_temp(&first_temp, first_rows)?;
        write_jsonl_temp(&second_temp, second_rows)?;
        fs::rename(&first_temp, first)?;
        if let Err(error) = fs::rename(&second_temp, second) {
            let _ = fs::remove_file(first);
            return Err(error.into());
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&first_temp);
        let _ = fs::remove_file(&second_temp);
    }
    result
}

fn write_jsonl_temp<T: Serialize>(path: &Path, rows: &[T]) -> anyhow::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    for row in rows {
        serde_json::to_writer(&mut file, row)?;
        file.write_all(b"\n")?;
    }
    file.sync_all()?;
    Ok(())
}

fn write_action_plan(
    session_dir: &Path,
    launch_id: &str,
    contract: CaptureContract,
    autonomous_input: bool,
    manual_stop: bool,
) -> anyhow::Result<()> {
    write_json_create_new(
        &session_dir.join(ACTION_PLAN_FILE),
        &OperatorActionPlan {
            schema: ACTION_PLAN_SCHEMA,
            process_launch_id: launch_id,
            duration_ms: contract.duration_ms,
            route: if manual_stop {
                "operator_controlled_until_explicit_stop"
            } else {
                "straight_out_and_back_with_terminal_correction"
            },
            route_constraints: [
                "start on a clear flat straight path",
                "first-person on-foot only",
                "no sprint, jump, crouch, ADS, scanner, phone, vehicle, dialogue, or menu",
                "keep camera direction fixed",
                "outbound uses W; return uses S on the same line",
                "terminal correction may use brief W/S taps only",
            ],
            steps: contract.steps,
            input_provenance: if autonomous_input {
                AUTONOMOUS_INPUT_PROVENANCE
            } else {
                MANUAL_INPUT_PROVENANCE
            },
            input_validation_policy: if autonomous_input {
                "autonomous_ledger/v1"
            } else {
                input_policy_name(
                    standalone_requires_mouse_movement(contract, manual_stop),
                    manual_stop,
                )
            },
            non_trainable: true,
            recording_ready: false,
        },
    )
}

fn scheduled_action_qpc(anchor_qpc: i64, frequency: i64, offset_ms: u64) -> anyhow::Result<i64> {
    ensure!(
        anchor_qpc > 0 && frequency > 0,
        "invalid action-plan QPC anchor"
    );
    let ticks = i128::from(frequency)
        .checked_mul(i128::from(offset_ms))
        .context("action marker QPC overflow")?
        / 1_000;
    i64::try_from(i128::from(anchor_qpc) + ticks).context("action marker QPC overflow")
}

fn autonomous_transition(marker_id: &str) -> Option<(u16, AutonomousKeyAction)> {
    match marker_id {
        "outbound-start" => Some((VK_W.0, AutonomousKeyAction::Down)),
        "outbound-stop" => Some((VK_W.0, AutonomousKeyAction::Up)),
        "return-start" => Some((VK_S.0, AutonomousKeyAction::Down)),
        "return-stop" => Some((VK_S.0, AutonomousKeyAction::Up)),
        "correction-start" => Some((VK_W.0, AutonomousKeyAction::Down)),
        "correction-stop" => Some((VK_W.0, AutonomousKeyAction::Up)),
        _ => None,
    }
}

fn validate_autonomous_input_target(target: AutonomousInputTarget) -> anyhow::Result<()> {
    let hwnd = HWND(target.hwnd_value as *mut _);
    let mut bound_pid = 0_u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut bound_pid)) };
    ensure!(
        bound_pid == target.process_id,
        "bound autonomous-input HWND changed PID"
    );
    let foreground = unsafe { GetForegroundWindow() };
    ensure!(
        !foreground.0.is_null(),
        "autonomous input has no foreground HWND"
    );
    let foreground_root = unsafe { GetAncestor(foreground, GA_ROOTOWNER) };
    ensure!(
        foreground_root.0 as usize == target.root_owner_value,
        "bound autonomous-input root is not foreground"
    );
    let mut foreground_pid = 0_u32;
    unsafe { GetWindowThreadProcessId(foreground_root, Some(&mut foreground_pid)) };
    ensure!(
        foreground_pid == target.process_id,
        "foreground root belongs to another PID"
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn send_autonomous_key(
    target: AutonomousInputTarget,
    launch_id: &str,
    marker_id: &str,
    scheduled_qpc: i64,
    attempt_qpc: i64,
    qpc_frequency: i64,
    vk: u16,
    action: AutonomousKeyAction,
    clock: QpcClock,
) -> anyhow::Result<AutonomousInputLedgerRow> {
    validate_autonomous_input_target(target)?;
    ensure!(matches!(vk, 0x53 | 0x57), "autonomous input key is not W/S");
    let flags = match action {
        AutonomousKeyAction::Down => Default::default(),
        AutonomousKeyAction::Up => KEYEVENTF_KEYUP,
    };
    let input = INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk),
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    let sent = unsafe { SendInput(&[input], i32::try_from(std::mem::size_of::<INPUT>())?) };
    ensure!(
        sent == 1,
        "SendInput did not inject exactly one W/S transition"
    );
    let actual_qpc = match clock.now() {
        Ok(qpc) => qpc,
        Err(error) => {
            if action == AutonomousKeyAction::Down {
                send_autonomous_key_up_unchecked(vk);
            }
            return Err(error).context("QPC failed after SendInput transition");
        }
    };
    ensure!(
        actual_qpc >= attempt_qpc && attempt_qpc >= scheduled_qpc,
        "autonomous input QPC timeline is inconsistent"
    );
    // A second identity/focus check closes the race between validation and SendInput.
    if let Err(error) = validate_autonomous_input_target(target) {
        if action == AutonomousKeyAction::Down {
            send_autonomous_key_up_unchecked(vk);
        }
        return Err(error).context("focus/PID changed during SendInput transition");
    }
    Ok(AutonomousInputLedgerRow {
        schema: AUTONOMOUS_INPUT_LEDGER_SCHEMA.into(),
        process_launch_id: launch_id.into(),
        input_provenance: AUTONOMOUS_INPUT_PROVENANCE.into(),
        marker_id: marker_id.into(),
        scheduled_qpc,
        attempt_qpc,
        actual_qpc,
        qpc_frequency,
        target_process_id: target.process_id,
        bound_hwnd_process_matches: true,
        foreground_root_matches: true,
        foreground_process_matches: true,
        vk,
        action: action.as_str().into(),
        send_input_count: sent,
        non_trainable: true,
        recording_ready: false,
    })
}

fn release_autonomous_keys(held: &mut HeldAutonomousKeys) {
    held.release_all();
}

fn send_autonomous_key_up_unchecked(vk: u16) {
    let input = INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk),
                wScan: 0,
                dwFlags: KEYEVENTF_KEYUP,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    let _ = unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32) };
}

fn write_action_markers(
    session_dir: &Path,
    launch_id: &str,
    anchor_qpc: i64,
    qpc_frequency: i64,
    actual_cues: &[ActualActionCue],
    contract: CaptureContract,
    autonomous_input: bool,
) -> anyhow::Result<()> {
    ensure!(
        actual_cues.len() == contract.steps.len(),
        "not every operator action cue was emitted"
    );
    let path = session_dir.join(ACTION_MARKERS_FILE);
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    for (step_index, step) in contract.steps.iter().enumerate() {
        let cue = actual_cues
            .get(step_index)
            .context("operator action cue sequence is incomplete")?;
        let scheduled_qpc = scheduled_action_qpc(anchor_qpc, qpc_frequency, step.offset_ms)?;
        ensure!(
            cue.step_index == step_index
                && cue.scheduled_qpc == scheduled_qpc
                && cue.actual_qpc >= cue.scheduled_qpc,
            "operator action cue identity/QPC is inconsistent"
        );
        let cue_delay_ms = duration_ms(cue.scheduled_qpc, cue.actual_qpc, qpc_frequency)?;
        ensure!(
            cue_delay_ms <= MAX_ACTION_CUE_DELAY_MS,
            "operator action cue {} was delayed by {} ms",
            step.marker_id,
            cue_delay_ms
        );
        serde_json::to_writer(
            &mut file,
            &OperatorActionMarker {
                schema: ACTION_MARKER_SCHEMA,
                process_launch_id: launch_id,
                marker_id: step.marker_id,
                phase: step.phase,
                instruction: step.instruction,
                expected_input: step.expected_input,
                offset_ms: step.offset_ms,
                scheduled_qpc,
                actual_cue_qpc: cue.actual_qpc,
                cue_delay_ms,
                qpc_frequency,
                input_provenance: if autonomous_input {
                    AUTONOMOUS_INPUT_PROVENANCE
                } else {
                    MANUAL_INPUT_PROVENANCE
                },
                non_trainable: true,
                recording_ready: false,
            },
        )?;
        file.write_all(b"\n")?;
    }
    file.sync_all()?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn audit_pose_camera_loop(
    session_dir: &Path,
    launch_id: &str,
    qpc_start: i64,
    qpc_end: i64,
    qpc_frequency: i64,
    video_frame_count: u64,
    input_event_count: u64,
    input_queue_overflows: u64,
    autonomous_input: bool,
    manual_stop: bool,
    require_mouse_movement: bool,
    expected_pid: u32,
    actual_cues: &[ActualActionCue],
    contract: CaptureContract,
    pose_report_path: &Path,
    render_camera_report_path: &Path,
    events_path: &Path,
    meta_path: &Path,
    manifest_path: &Path,
) -> anyhow::Result<PoseCameraLoopQcReport> {
    ensure!(qpc_frequency > 0 && qpc_start > 0 && qpc_end > qpc_start);
    let pose_bytes = read_bounded_file(pose_report_path, 16 * 1024 * 1024)?;
    let render_report = camera_report::read(render_camera_report_path)?;
    let pose: Value = serde_json::from_slice(&pose_bytes).context("invalid raw pose report")?;
    let player = audit_player_pose(
        &pose,
        launch_id,
        qpc_start,
        qpc_end,
        qpc_frequency,
        actual_cues,
        contract,
        manual_stop,
    )?;
    let depth: Vec<RawDepthPairingRow> =
        read_bounded_jsonl(&session_dir.join("raw-depth-index.jsonl"), false)?;
    let camera = audit_render_camera(
        &render_report,
        launch_id,
        qpc_start,
        qpc_end,
        &depth,
        contract,
    )?;
    let input = if autonomous_input {
        audit_autonomous_input(
            &session_dir.join(AUTONOMOUS_INPUT_LEDGER_FILE),
            events_path,
            meta_path,
            manifest_path,
            launch_id,
            expected_pid,
            qpc_start,
            qpc_end,
            qpc_frequency,
            video_frame_count,
            input_event_count,
            input_queue_overflows,
            contract,
        )?
    } else {
        audit_raw_input(
            events_path,
            meta_path,
            manifest_path,
            qpc_start,
            qpc_end,
            qpc_frequency,
            video_frame_count,
            input_event_count,
            input_queue_overflows,
            actual_cues,
            contract,
            manual_stop,
            require_mouse_movement,
        )?
    };
    let action_plan = session_dir.join(ACTION_PLAN_FILE);
    let action_markers = session_dir.join(ACTION_MARKERS_FILE);
    ensure!(action_plan.is_file() && action_markers.is_file());
    Ok(PoseCameraLoopQcReport {
        schema: POSE_CAMERA_LOOP_QC_SCHEMA,
        passed: true,
        process_launch_id: launch_id.to_owned(),
        qpc_start,
        qpc_end,
        qpc_frequency,
        pose_report: POSE_REPORT_FILE,
        pose_report_sha256: sha256_bytes(&pose_bytes),
        render_camera_report: RENDER_CAMERA_REPORT_FILE,
        render_camera_report_sha256: render_report.sha256,
        operator_action_plan: ACTION_PLAN_FILE,
        operator_action_plan_sha256: sha256_file(&action_plan)?,
        operator_action_markers: ACTION_MARKERS_FILE,
        operator_action_markers_sha256: sha256_file(&action_markers)?,
        input_provenance: if autonomous_input {
            AUTONOMOUS_INPUT_PROVENANCE
        } else {
            MANUAL_INPUT_PROVENANCE
        },
        autonomous_input_ledger: autonomous_input.then_some(AUTONOMOUS_INPUT_LEDGER_FILE),
        autonomous_input_ledger_sha256: autonomous_input
            .then(|| sha256_file(&session_dir.join(AUTONOMOUS_INPUT_LEDGER_FILE)))
            .transpose()?,
        raw_input_events: relative_path(session_dir, events_path)?,
        raw_input_events_sha256: sha256_file(events_path)?,
        raw_input_meta: relative_path(session_dir, meta_path)?,
        raw_input_meta_sha256: sha256_file(meta_path)?,
        raw_input_manifest: relative_path(session_dir, manifest_path)?,
        raw_input_manifest_sha256: sha256_file(manifest_path)?,
        input_event_count: input.evidence_event_count,
        raw_input_event_count: input.raw_event_count,
        outbound_hold_ms: input.outbound_hold_ms,
        return_hold_ms: input.return_hold_ms,
        mouse_move_events: input.mouse_move_events,
        keyboard_events: input.keyboard_events,
        player_pose_samples: player.sample_count,
        player_resolver_id: player.resolver_id,
        player_object_tag: player.object_tag,
        player_transform_offset: player.transform_offset,
        player_first_qpc: player.first_qpc,
        player_last_qpc: player.last_qpc,
        maximum_player_displacement_world_units: player.maximum_displacement_world_units,
        outbound_player_displacement_world_units: player.outbound_displacement_world_units,
        return_stop_closure_world_units: player.return_stop_closure_world_units,
        terminal_player_closure_world_units: player.terminal_closure_world_units,
        render_matched_depth_presents: camera.matched_depth_presents,
        render_depth_present_coverage: camera.depth_present_coverage,
        render_physical_source: camera.physical_source,
        render_view_source: camera.view_source,
        render_projection_source: camera.projection_source,
        render_view_source_aliases: camera.view_source_aliases,
        render_projection_source_aliases: camera.projection_source_aliases,
        render_copy_validation_policy: camera.copy_validation_policy,
        render_retained_view_copy_observations: camera.retained_view_copy_observations,
        render_retained_projection_copy_observations: camera.retained_projection_copy_observations,
        render_view_samples: camera.view_sample_count,
        render_projection_samples: camera.projection_sample_count,
        maximum_view_matrix_delta: camera.maximum_view_matrix_delta,
        terminal_view_matrix_delta: camera.terminal_view_matrix_delta,
        maximum_projection_matrix_delta: camera.maximum_projection_matrix_delta,
        projection_validation: camera.projection_validation,
        input_validation_policy: if autonomous_input {
            "autonomous_ledger/v1"
        } else {
            input_policy_name(require_mouse_movement, manual_stop)
        },
        blockers: Vec::new(),
        non_trainable: true,
        production_certified: false,
        deliverable: false,
        recording_ready: false,
    })
}

#[allow(clippy::too_many_arguments)]
fn audit_player_pose(
    report: &Value,
    launch_id: &str,
    qpc_start: i64,
    qpc_end: i64,
    qpc_frequency: i64,
    actual_cues: &[ActualActionCue],
    contract: CaptureContract,
    manual_stop: bool,
) -> anyhow::Result<PlayerPoseMetrics> {
    ensure!(report.get("schema").and_then(Value::as_str) == Some(POSE_REPORT_SCHEMA));
    ensure!(
        report.get("process_launch_id").and_then(Value::as_str) == Some(launch_id),
        "raw pose report belongs to another launch"
    );
    ensure!(
        report
            .pointer("/exact_build/executable_sha256")
            .and_then(Value::as_str)
            == Some(CP2077_EXE_SHA256)
            && report.get("absolute_process_addresses_recorded") == Some(&Value::Bool(false))
            && report.get("production_binding_emitted") == Some(&Value::Bool(false))
            && report.get("recording_ready") == Some(&Value::Bool(false)),
        "raw pose report weakens identity/safety constraints"
    );
    if let Some(blockers) = report.get("blockers").and_then(Value::as_array) {
        for blocker in blockers {
            let code = blocker.get("code").and_then(Value::as_str).unwrap_or("");
            ensure!(
                !code.contains("player")
                    && !matches!(
                        code,
                        "camera_player_resolver_alias"
                            | "camera_player_chain_alias"
                            | "camera_player_object_alias"
                            | "runtime_pose_snapshots_missing"
                            | "transform_candidate_set_truncated"
                            | "world_transform_candidate_missing"
                            | "candidate_trajectories_missing"
                    ),
                "raw pose report carries critical blocker {code}"
            );
        }
    }
    let snapshots = report
        .get("snapshot_observations")
        .and_then(Value::as_array)
        .context("raw pose report has no snapshot observations")?;
    let mut samples = Vec::<(i64, u64, [f64; 3])>::new();
    let mut selected_key: Option<(String, u64, u64)> = None;
    let mut previous_present = 0u64;
    let mut previous_qpc = 0i64;
    for snapshot in snapshots {
        if snapshot.get("role").and_then(Value::as_str) != Some("player") {
            continue;
        }
        let qpc = snapshot
            .get("qpc")
            .and_then(Value::as_i64)
            .context("player pose snapshot has no QPC")?;
        if qpc < qpc_start || qpc > qpc_end {
            continue;
        }
        ensure!(snapshot.get("candidates_truncated") == Some(&Value::Bool(false)));
        let candidates = snapshot
            .get("candidates")
            .and_then(Value::as_array)
            .context("player pose snapshot has no candidates")?;
        ensure!(
            candidates.len() == 1,
            "player pose is not unique within a capture-window snapshot"
        );
        let resolver_id = snapshot
            .get("resolver_id")
            .and_then(Value::as_str)
            .context("player pose has no resolver id")?;
        let object_tag = snapshot
            .get("object_tag")
            .and_then(Value::as_u64)
            .context("player pose has no object tag")?;
        let present_seq = snapshot
            .get("present_seq")
            .and_then(Value::as_u64)
            .context("player pose has no Present identity")?;
        let candidate = &candidates[0];
        let transform_offset = candidate
            .get("transform_offset")
            .and_then(Value::as_u64)
            .context("player pose has no transform offset")?;
        let key = (resolver_id.to_owned(), object_tag, transform_offset);
        if let Some(expected) = &selected_key {
            ensure!(
                expected == &key,
                "player pose resolver/object/offset changed inside the recording window"
            );
        } else {
            selected_key = Some(key);
        }
        ensure!(
            present_seq > previous_present && qpc > previous_qpc,
            "player pose Present/QPC timeline is not strictly increasing"
        );
        previous_present = present_seq;
        previous_qpc = qpc;
        let position: [f64; 3] = serde_json::from_value(
            candidate
                .get("position_world_units")
                .cloned()
                .context("player pose has no position")?,
        )?;
        ensure!(position.iter().all(|value| value.is_finite()));
        let quaternion: [f64; 4] = serde_json::from_value(
            candidate
                .get("raw_quaternion_ijkr")
                .cloned()
                .context("player pose has no quaternion")?,
        )?;
        ensure!(quaternion.iter().all(|value| value.is_finite()));
        ensure!(
            candidate
                .get("quaternion_norm_error")
                .and_then(Value::as_f64)
                .is_some_and(|error| error <= 0.001)
        );
        samples.push((qpc, present_seq, position));
    }
    ensure!(
        samples.len() >= MIN_PLAYER_POSE_SAMPLES,
        "too few unique player pose samples in recording window"
    );
    let coverage_slack = qpc_frequency.saturating_mul(2);
    ensure!(
        samples[0].0.saturating_sub(qpc_start) <= coverage_slack
            && qpc_end.saturating_sub(samples.last().expect("samples nonempty").0)
                <= coverage_slack,
        "player pose does not cover both ends of the recording window"
    );
    let first = samples[0].2;
    let last = samples.last().expect("samples nonempty").2;
    let maximum_displacement = samples
        .iter()
        .map(|sample| distance3(first, sample.2))
        .fold(0.0, f64::max);
    let (outbound_displacement, return_stop_closure) = if manual_stop {
        (maximum_displacement, distance3(first, last))
    } else {
        let outbound = nearest_pose_position(
            &samples,
            actual_cue_qpc(actual_cues, contract.steps, "outbound-stop")?,
        );
        let return_stop = nearest_pose_position(
            &samples,
            actual_cue_qpc(actual_cues, contract.steps, "return-stop")?,
        );
        (distance3(first, outbound), distance3(first, return_stop))
    };
    let terminal_closure = distance3(first, last);
    ensure!(
        maximum_displacement >= MIN_PLAYER_DISPLACEMENT_WORLD_UNITS
            && outbound_displacement >= MIN_PLAYER_DISPLACEMENT_WORLD_UNITS,
        "player pose did not prove required outbound displacement"
    );
    if contract.name == depth_lab::PR405_LOCAL_CONTRACT {
        ensure!(
            terminal_closure <= MAX_TERMINAL_CLOSURE_WORLD_UNITS,
            "player pose did not close the terminal loop in raw world units"
        );
    }
    let trajectories = report
        .get("trajectories")
        .and_then(Value::as_array)
        .context("raw pose report has no trajectory summaries")?
        .iter()
        .filter(|trajectory| trajectory.get("role").and_then(Value::as_str) == Some("player"))
        .collect::<Vec<_>>();
    ensure!(
        trajectories.len() == 1,
        "raw pose report does not contain exactly one surviving player trajectory"
    );
    let key = selected_key.context("player pose key was not selected")?;
    ensure!(
        trajectories[0].get("resolver_id").and_then(Value::as_str) == Some(key.0.as_str())
            && trajectories[0].get("object_tag").and_then(Value::as_u64) == Some(key.1)
            && trajectories[0]
                .get("transform_offset")
                .and_then(Value::as_u64)
                == Some(key.2)
            && trajectories[0].get("position_changed_from_first") == Some(&Value::Bool(true)),
        "player trajectory summary conflicts with capture-window samples"
    );
    Ok(PlayerPoseMetrics {
        sample_count: samples.len(),
        resolver_id: key.0,
        object_tag: key.1,
        transform_offset: key.2,
        first_qpc: samples[0].0,
        last_qpc: samples.last().expect("samples nonempty").0,
        maximum_displacement_world_units: maximum_displacement,
        outbound_displacement_world_units: outbound_displacement,
        return_stop_closure_world_units: return_stop_closure,
        terminal_closure_world_units: terminal_closure,
    })
}

fn audit_render_camera(
    checked: &camera_report::CameraReport,
    launch_id: &str,
    qpc_start: i64,
    qpc_end: i64,
    depth: &[RawDepthPairingRow],
    contract: CaptureContract,
) -> anyhow::Result<RenderCameraMetrics> {
    let report = &checked.value;
    ensure!(
        camera_report::supported_schema(report)
            && report.get("process_launch_id").and_then(Value::as_str) == Some(launch_id),
        "render-camera report schema/launch mismatch"
    );
    ensure!(
        report.get("non_trainable") == Some(&Value::Bool(true))
            && report.get("production_certified") == Some(&Value::Bool(false))
            && report.get("deliverable") == Some(&Value::Bool(false))
            && report.get("recording_ready") == Some(&Value::Bool(false)),
        "render-camera report weakens safety ceiling"
    );
    let observation = report
        .get("observation")
        .context("render-camera report has no observation")?;
    ensure!(observation.get("selected_depth_armed") == Some(&Value::Bool(true)));
    ensure!(
        contract.fixed_view_walk()
            || observation.get("candidate_admitted_for_cross_launch_discovery")
                == Some(&Value::Bool(true)),
        "render-camera candidate was not physically admitted"
    );
    ensure!(
        observation.get("production_camera_binding_emitted") == Some(&Value::Bool(false))
            && observation.get("recording_ready") == Some(&Value::Bool(false))
    );
    ensure!(
        observation
            .get("blockers")
            .and_then(Value::as_array)
            .is_some_and(Vec::is_empty),
        "render-camera observation contains blockers"
    );
    ensure!(
        contract.fixed_view_walk()
            || observation
                .get("authoring_blockers")
                .and_then(Value::as_array)
                .is_some_and(Vec::is_empty),
        "render-camera observation contains authoring blockers"
    );
    if contract.fixed_view_walk() {
        camera_alias::validate_local_observation(observation, checked.archive_verified())?;
    }
    let depth_presents = depth
        .iter()
        .filter(|row| (qpc_start..=qpc_end).contains(&row.present_qpc))
        .map(|row| row.present_seq)
        .collect::<std::collections::BTreeSet<_>>();
    ensure!(!depth_presents.is_empty());
    let mut matched_presents = std::collections::BTreeSet::new();
    let mut physical_sources = std::collections::BTreeSet::new();
    let mut views = RenderMatrixSources::new();
    let mut projections = RenderMatrixSources::new();
    let mut local_views = Vec::new();
    let mut local_projections = Vec::new();
    let mut archived_camera = (contract.fixed_view_walk() && checked.archive_verified())
        .then(camera_alias::ArchiveCamera::default);
    for draw in observation
        .get("observations")
        .and_then(Value::as_array)
        .context("render-camera report has no draw observations")?
    {
        let qpc = draw.get("present_qpc").and_then(Value::as_i64).unwrap_or(0);
        let present = draw.get("present_seq").and_then(Value::as_u64).unwrap_or(0);
        if !(qpc_start..=qpc_end).contains(&qpc)
            || !depth_presents.contains(&present)
            || draw.get("selected_present_identity_passed") != Some(&Value::Bool(true))
        {
            continue;
        }
        ensure!(draw.get("copy_before_clear_submitted") == Some(&Value::Bool(true)));
        let physical_source = draw
            .get("physical_source")
            .and_then(Value::as_str)
            .context("render-camera draw has no physical source")?;
        ensure!(
            audited_render_physical_source(physical_source),
            "render-camera draw uses an unaudited physical source"
        );
        physical_sources.insert(physical_source.to_owned());
        matched_presents.insert(present);
        if let Some(camera) = &mut archived_camera {
            camera.push_draw(draw)?;
            continue;
        }
        for matrix in draw
            .get("matrices")
            .and_then(Value::as_array)
            .context("render-camera draw has no matrices")?
        {
            if physical_source
                == "copy_buffer_region_root_signature_declared_cbv_at_selected_dsv_draw"
            {
                ensure!(
                    audited_render_matrix_physical_source(
                        physical_source,
                        matrix.get("physical_source").and_then(Value::as_str)
                    ),
                    "render-camera matrix does not carry the audited exact CopyBufferRegion provenance"
                );
            }
            let kind = matrix
                .get("matrix_kind")
                .and_then(Value::as_str)
                .unwrap_or("");
            let value: RenderMatrix = serde_json::from_value(
                matrix
                    .get("matrix")
                    .cloned()
                    .context("render-camera matrix has no values")?,
            )?;
            ensure!(value.iter().flatten().all(|entry| entry.is_finite()));
            if kind == "view" {
                if contract.fixed_view_walk() {
                    let sample = camera_alias::sample(matrix, draw, depth_lab::MatrixRole::View)?;
                    local_views.push(sample);
                }
                let key = render_matrix_source_key(matrix, depth_lab::MatrixRole::View)?;
                views.entry(key).or_default().insert(present, value);
            } else if kind == "projection"
                && matrix.get("configured_vertical_fov_matches") == Some(&Value::Bool(true))
                && matrix
                    .get("projection_a")
                    .and_then(Value::as_f64)
                    .is_some_and(f64::is_finite)
                && matrix
                    .get("projection_b")
                    .and_then(Value::as_f64)
                    .is_some_and(f64::is_finite)
            {
                let key = render_matrix_source_key(matrix, depth_lab::MatrixRole::Projection)?;
                if contract.fixed_view_walk() {
                    let sample =
                        camera_alias::sample(matrix, draw, depth_lab::MatrixRole::Projection)?;
                    local_projections.push(sample);
                }
                projections.entry(key).or_default().insert(present, value);
            }
        }
    }
    let depth_coverage = matched_presents.len() as f64 / depth_presents.len() as f64;
    ensure!(
        matched_presents.len() >= MIN_RENDER_CAMERA_PRESENTS,
        "render-camera report has too few exact depth Presents"
    );
    if contract.name == depth_lab::PR405_LOCAL_CONTRACT {
        ensure!(
            depth_coverage >= MIN_RENDER_SOURCE_COVERAGE,
            "render-camera report does not cover enough exact depth Presents"
        );
    }
    let physical_source = unique_render_physical_source(&physical_sources)?;
    let mut retained_view_copy_observations = 0;
    let mut retained_projection_copy_observations = 0;
    let (view_source_aliases, projection_source_aliases) = if let Some(camera) = archived_camera {
        let camera = camera.finish(matched_presents.len())?;
        retained_view_copy_observations = camera.retained_views;
        retained_projection_copy_observations = camera.retained_projections;
        views = [(
            serde_json::to_string(&camera.view_sources[0])?,
            camera.views,
        )]
        .into();
        projections = [(
            serde_json::to_string(&camera.projection_sources[0])?,
            camera.projections,
        )]
        .into();
        (camera.view_sources, camera.projection_sources)
    } else if contract.fixed_view_walk() {
        let view = camera_alias::unique(local_views, matched_presents.len())?;
        let projection = camera_alias::unique(local_projections, matched_presents.len())?;
        depth_lab::d3d12_camera_copy_series::validate_pair(&view, &projection)?;
        retained_view_copy_observations = view.observations.len();
        retained_projection_copy_observations = projection.observations.len();
        views = [(
            serde_json::to_string(&view.sources[0])?,
            camera_alias::matrices(&view),
        )]
        .into();
        projections = [(
            serde_json::to_string(&projection.sources[0])?,
            camera_alias::matrices(&projection),
        )]
        .into();
        (view.sources, projection.sources)
    } else {
        (Vec::new(), Vec::new())
    };
    let (view_source, view_samples) = unique_stable_matrix_source(&views, matched_presents.len())?;
    let (projection_source, projection_samples) =
        unique_stable_matrix_source(&projections, matched_presents.len())?;
    let stable_view: depth_lab::D3d12MatrixSourceBinding = serde_json::from_str(view_source)?;
    let stable_projection: depth_lab::D3d12MatrixSourceBinding =
        serde_json::from_str(projection_source)?;
    ensure!(
        stable_view.descriptor_table == stable_projection.descriptor_table
            && stable_view.cbv_resource_offset == stable_projection.cbv_resource_offset
            && stable_view.declared_cbv_bytes == stable_projection.declared_cbv_bytes
            && stable_view.draw_relation == stable_projection.draw_relation
            && stable_view.matrix_offset != stable_projection.matrix_offset,
        "render-camera view/projection are not distinct matrices in one stable selected-DSV CBV"
    );
    let view_values = view_samples.values().copied().collect::<Vec<_>>();
    let projection_values = projection_samples.values().copied().collect::<Vec<_>>();
    let maximum_view_delta = view_values
        .iter()
        .map(|matrix| matrix_delta(view_values[0], *matrix))
        .fold(0.0, f64::max);
    let terminal_view_delta = matrix_delta(
        view_values[0],
        *view_values.last().expect("stable view source is nonempty"),
    );
    // The timed native-1080p local walk permits bounded framewise principal
    // offsets, not changes in focal/depth/homogeneous terms. Other contracts
    // retain the old full-matrix rule. Original matrices remain in the report.
    let projection_validation = projection_validation::validate(
        &projection_values,
        1920,
        1080,
        contract.fixed_view_walk(),
    )?;
    ensure!(
        maximum_view_delta > 0.0001,
        "render-camera view matrix did not respond to player translation"
    );
    if contract.name == depth_lab::PR405_LOCAL_CONTRACT {
        ensure!(
            terminal_view_delta <= MAX_RENDER_VIEW_TERMINAL_DELTA,
            "render-camera view matrix did not return near its initial state"
        );
    }
    Ok(RenderCameraMetrics {
        matched_depth_presents: matched_presents.len(),
        depth_present_coverage: depth_coverage,
        physical_source: physical_source.to_owned(),
        view_source: view_source.clone(),
        projection_source: projection_source.clone(),
        view_source_aliases,
        projection_source_aliases,
        copy_validation_policy: if contract.fixed_view_walk() {
            depth_lab::d3d12_camera_copy_series::POLICY
        } else {
            "exact_stable_source/v1"
        },
        retained_view_copy_observations,
        retained_projection_copy_observations,
        view_sample_count: view_samples.len(),
        projection_sample_count: projection_samples.len(),
        maximum_view_matrix_delta: maximum_view_delta,
        terminal_view_matrix_delta: terminal_view_delta,
        maximum_projection_matrix_delta: projection_validation.maximum_matrix_delta,
        projection_validation,
    })
}

fn audited_render_physical_source(value: &str) -> bool {
    matches!(
        value,
        "direct_game_mapped_root_cbv_at_selected_dsv_draw"
            | "game_mapped_descriptor_table_base_cbv_at_selected_dsv_draw"
            | "copy_buffer_region_root_signature_declared_cbv_at_selected_dsv_draw"
    )
}

fn audited_render_matrix_physical_source(draw_source: &str, matrix_source: Option<&str>) -> bool {
    draw_source != "copy_buffer_region_root_signature_declared_cbv_at_selected_dsv_draw"
        || matrix_source == Some("copy_buffer_region_root_signature_declared_cbv")
}

fn unique_render_physical_source(
    sources: &std::collections::BTreeSet<String>,
) -> anyhow::Result<&str> {
    ensure!(
        sources.len() == 1,
        "render-camera physical source is missing or changed inside the recording window"
    );
    Ok(sources
        .first()
        .expect("one render physical source was checked")
        .as_str())
}

#[allow(clippy::too_many_arguments)]
fn audit_autonomous_input(
    ledger_path: &Path,
    events_path: &Path,
    meta_path: &Path,
    manifest_path: &Path,
    launch_id: &str,
    expected_pid: u32,
    qpc_start: i64,
    qpc_end: i64,
    qpc_frequency: i64,
    video_frame_count: u64,
    expected_raw_event_count: u64,
    input_queue_overflows: u64,
    contract: CaptureContract,
) -> anyhow::Result<RawInputMetrics> {
    validate_autonomous_contract(true, contract)?;
    ensure!(
        input_queue_overflows == 0,
        "autonomous input monitor overflowed"
    );
    let raw_events: Vec<RawInputEvent> = read_bounded_jsonl(events_path, true)?;
    ensure!(
        raw_events.len() as u64 == expected_raw_event_count,
        "autonomous Raw Input count conflicts with recorder outcome"
    );

    let rows: Vec<AutonomousInputLedgerRow> = read_bounded_jsonl(ledger_path, false)?;
    let mut expected = vec![
        ("outbound-start", VK_W.0, AutonomousKeyAction::Down),
        ("outbound-stop", VK_W.0, AutonomousKeyAction::Up),
        ("return-start", VK_S.0, AutonomousKeyAction::Down),
        ("return-stop", VK_S.0, AutonomousKeyAction::Up),
    ];
    if contract.name == depth_lab::ISSUE735_LOCAL_CONTRACT {
        expected.extend([
            ("correction-start", VK_W.0, AutonomousKeyAction::Down),
            ("correction-stop", VK_W.0, AutonomousKeyAction::Up),
        ]);
    }
    ensure!(
        rows.len() == expected.len(),
        "autonomous input ledger does not match the exact contract transitions"
    );
    let first_step = contract
        .steps
        .iter()
        .find(|step| step.marker_id == expected[0].0)
        .context("autonomous action plan lacks outbound-start")?;
    let first_offset =
        i64::try_from(i128::from(qpc_frequency) * i128::from(first_step.offset_ms) / 1_000)?;
    let anchor_qpc = rows[0]
        .scheduled_qpc
        .checked_sub(first_offset)
        .context("autonomous input anchor underflow")?;
    let mut previous_actual = 0_i64;
    for (row, &(marker_id, vk, action)) in rows.iter().zip(&expected) {
        let step = contract
            .steps
            .iter()
            .find(|step| step.marker_id == marker_id)
            .context("autonomous ledger marker is absent from action plan")?;
        ensure!(
            row.schema == AUTONOMOUS_INPUT_LEDGER_SCHEMA
                && row.process_launch_id == launch_id
                && row.input_provenance == AUTONOMOUS_INPUT_PROVENANCE
                && row.marker_id == marker_id
                && row.qpc_frequency == qpc_frequency
                && row.target_process_id == expected_pid
                && row.bound_hwnd_process_matches
                && row.foreground_root_matches
                && row.foreground_process_matches
                && row.vk == vk
                && row.action == action.as_str()
                && row.send_input_count == 1
                && row.non_trainable
                && !row.recording_ready,
            "autonomous input ledger identity/provenance fact drifted"
        );
        ensure!(
            row.scheduled_qpc == scheduled_action_qpc(anchor_qpc, qpc_frequency, step.offset_ms)?,
            "autonomous input ledger does not follow the exact contract QPC plan"
        );
        ensure!(
            row.attempt_qpc >= row.scheduled_qpc
                && row.actual_qpc >= row.attempt_qpc
                && (qpc_start..qpc_end).contains(&row.actual_qpc)
                && row.actual_qpc > previous_actual,
            "autonomous input ledger QPC timeline is invalid"
        );
        ensure!(
            duration_ms(row.scheduled_qpc, row.actual_qpc, qpc_frequency)?
                <= MAX_ACTION_CUE_DELAY_MS,
            "autonomous input transition exceeded the cue-delay budget"
        );
        previous_actual = row.actual_qpc;
    }
    validate_autonomous_raw_input_mirror(
        &raw_events,
        &rows,
        events_path,
        meta_path,
        manifest_path,
        qpc_start,
        qpc_end,
        video_frame_count,
        qpc_frequency,
    )?;
    let outbound_hold_ms = autonomous_hold_ms(&rows[0], &rows[1], qpc_frequency, contract)?;
    let return_hold_ms = autonomous_hold_ms(&rows[2], &rows[3], qpc_frequency, contract)?;
    Ok(RawInputMetrics {
        evidence_event_count: rows.len(),
        raw_event_count: raw_events.len(),
        outbound_hold_ms,
        return_hold_ms,
        mouse_move_events: 0,
        keyboard_events: rows.len(),
    })
}

#[allow(clippy::too_many_arguments)]
fn validate_autonomous_raw_input_mirror(
    raw_events: &[RawInputEvent],
    rows: &[AutonomousInputLedgerRow],
    events_path: &Path,
    meta_path: &Path,
    manifest_path: &Path,
    qpc_start: i64,
    qpc_end: i64,
    video_frame_count: u64,
    qpc_frequency: i64,
) -> anyhow::Result<()> {
    ensure!(
        raw_events.is_empty() || raw_events.len() == rows.len(),
        "autonomous run contains Raw Input not explained by its SendInput ledger"
    );
    for (index, event) in raw_events.iter().enumerate() {
        let row = &rows[index];
        ensure!(
            event.seq == index as u64
                && event.run_id == "r0000"
                && (qpc_start..qpc_end).contains(&event.qpc)
                && !event.device.trim().is_empty()
                && !event.control
                && event.vk == Some(row.vk)
                && event.event_type == row.action
                && event.qpc.abs_diff(row.actual_qpc)
                    <= u64::try_from(qpc_frequency)? * MAX_ACTION_CUE_DELAY_MS / 1_000,
            "Raw Input event is not an exact timestamped mirror of authorized SendInput"
        );
    }
    validate_raw_input_artifact_envelope(
        events_path,
        meta_path,
        manifest_path,
        qpc_start,
        qpc_end,
        video_frame_count,
        raw_events.len() as u64,
    )
}

fn autonomous_hold_ms(
    down: &AutonomousInputLedgerRow,
    up: &AutonomousInputLedgerRow,
    qpc_frequency: i64,
    contract: CaptureContract,
) -> anyhow::Result<u64> {
    ensure!(down.action == "key_down" && up.action == "key_up" && down.vk == up.vk);
    let held_ms = duration_ms(down.actual_qpc, up.actual_qpc, qpc_frequency)?;
    ensure!(
        (contract.min_input_hold_ms..=contract.max_input_hold_ms).contains(&held_ms),
        "autonomous movement hold duration is outside the audited contract bound"
    );
    Ok(held_ms)
}

fn autonomous_keys_at(qpc: i64, rows: &[AutonomousInputLedgerRow]) -> Vec<u16> {
    let mut held = std::collections::BTreeSet::new();
    for row in rows.iter().filter(|row| row.actual_qpc <= qpc) {
        if row.action == "key_down" {
            held.insert(row.vk);
        } else if row.action == "key_up" {
            held.remove(&row.vk);
        }
    }
    held.into_iter().collect()
}

#[allow(clippy::too_many_arguments)]
fn validate_raw_input_artifact_envelope(
    events_path: &Path,
    meta_path: &Path,
    manifest_path: &Path,
    qpc_start: i64,
    qpc_end: i64,
    video_frame_count: u64,
    expected_lines: u64,
) -> anyhow::Result<()> {
    let meta: Value = serde_json::from_slice(&read_bounded_file(meta_path, 2 * 1024 * 1024)?)?;
    let manifest: Value =
        serde_json::from_slice(&read_bounded_file(manifest_path, 2 * 1024 * 1024)?)?;
    let events_hash = sha256_file(events_path)?;
    ensure!(
        meta.get("state").and_then(Value::as_str) == Some("non_trainable_ready")
            && meta.get("non_trainable") == Some(&Value::Bool(true))
            && meta.get("input_status").and_then(Value::as_str) == Some("raw_input_v1")
            && meta.get("qpc_start").and_then(Value::as_i64) == Some(qpc_start)
            && meta.get("qpc_end").and_then(Value::as_i64) == Some(qpc_end)
            && meta.get("n_frames").and_then(Value::as_u64) == Some(video_frame_count)
            && meta.pointer("/events/path").and_then(Value::as_str) == Some("events.jsonl.zst")
            && meta.pointer("/events/lines").and_then(Value::as_u64) == Some(expected_lines)
            && meta.pointer("/events/sha256").and_then(Value::as_str) == Some(events_hash.as_str())
            && meta
                .pointer("/health/input_overflow")
                .and_then(Value::as_u64)
                == Some(0)
            && meta
                .pointer("/health/input_boundary_reorder_drops")
                .and_then(Value::as_u64)
                == Some(0)
            && manifest.get("events.jsonl.zst").and_then(Value::as_str)
                == Some(events_hash.as_str()),
        "Raw Input meta/manifest contract is incomplete or inconsistent"
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn audit_raw_input(
    events_path: &Path,
    meta_path: &Path,
    manifest_path: &Path,
    qpc_start: i64,
    qpc_end: i64,
    qpc_frequency: i64,
    video_frame_count: u64,
    expected_event_count: u64,
    input_queue_overflows: u64,
    actual_cues: &[ActualActionCue],
    contract: CaptureContract,
    manual_stop: bool,
    require_mouse_movement: bool,
) -> anyhow::Result<RawInputMetrics> {
    ensure!(
        input_queue_overflows == 0,
        "Raw Input outcome reported overflow"
    );
    let events: Vec<RawInputEvent> = read_bounded_jsonl(events_path, true)?;
    ensure!(
        !events.is_empty() && events.len() as u64 == expected_event_count,
        "Raw Input artifact count conflicts with recorder outcome"
    );
    for window in events.windows(2) {
        ensure!(
            window[1].seq > window[0].seq && window[1].qpc >= window[0].qpc,
            "Raw Input sequence/QPC is not monotonic"
        );
    }
    ensure!(events.iter().all(|event| {
        event.run_id == "r0000"
            && (qpc_start..qpc_end).contains(&event.qpc)
            && !event.device.trim().is_empty()
            && !event.control
    }));
    let meta_bytes = read_bounded_file(meta_path, 2 * 1024 * 1024)?;
    let manifest_bytes = read_bounded_file(manifest_path, 2 * 1024 * 1024)?;
    let meta: Value = serde_json::from_slice(&meta_bytes)?;
    let manifest: Value = serde_json::from_slice(&manifest_bytes)?;
    let events_hash = sha256_file(events_path)?;
    ensure!(
        meta.get("state").and_then(Value::as_str) == Some("non_trainable_ready")
            && meta.get("non_trainable") == Some(&Value::Bool(true))
            && meta.get("input_status").and_then(Value::as_str) == Some("raw_input_v1")
            && meta.get("qpc_start").and_then(Value::as_i64) == Some(qpc_start)
            && meta.get("qpc_end").and_then(Value::as_i64) == Some(qpc_end)
            && meta.get("n_frames").and_then(Value::as_u64) == Some(video_frame_count)
            && meta.pointer("/events/path").and_then(Value::as_str) == Some("events.jsonl.zst")
            && meta.pointer("/events/lines").and_then(Value::as_u64) == Some(events.len() as u64)
            && meta.pointer("/events/sha256").and_then(Value::as_str) == Some(events_hash.as_str())
            && meta
                .pointer("/health/input_raw_reports")
                .and_then(Value::as_u64)
                .is_some_and(|count| count >= events.len() as u64)
            && meta
                .pointer("/health/input_overflow")
                .and_then(Value::as_u64)
                == Some(0)
            && meta
                .pointer("/health/input_boundary_reorder_drops")
                .and_then(Value::as_u64)
                == Some(0)
            && manifest.get("events.jsonl.zst").and_then(Value::as_str)
                == Some(events_hash.as_str()),
        "Raw Input meta/manifest contract is incomplete or inconsistent"
    );
    let keyboard_events = events
        .iter()
        .filter(|event| matches!(event.event_type.as_str(), "key_down" | "key_up"))
        .count();
    let mouse_move_events = events
        .iter()
        .filter(|event| event.event_type == "mouse_move")
        .count();
    ensure!(
        keyboard_events > 0
            && events.iter().any(|event| event.event_type == "key_down")
            && events.iter().any(|event| event.event_type == "key_up"),
        "Raw Input artifact contains no complete keyboard activity"
    );
    if require_mouse_movement {
        ensure!(
            mouse_move_events > 0,
            "Raw Input artifact contains no mouse movement"
        );
    }
    if !require_mouse_movement && !manual_stop && contract.fixed_view_walk() {
        audit_fixed_view_controls(&events)?;
    }
    if manual_stop {
        return Ok(RawInputMetrics {
            evidence_event_count: events.len(),
            raw_event_count: events.len(),
            outbound_hold_ms: 0,
            return_hold_ms: 0,
            mouse_move_events,
            keyboard_events,
        });
    }
    let tolerance_ms = input_cue_tolerance_ms(require_mouse_movement, manual_stop);
    let outbound_hold_ms = action_hold_ms(
        &events,
        87,
        actual_cue_qpc(actual_cues, contract.steps, "outbound-start")?,
        actual_cue_qpc(actual_cues, contract.steps, "outbound-stop")?,
        qpc_frequency,
        contract,
        tolerance_ms,
    )?;
    let return_hold_ms = action_hold_ms(
        &events,
        83,
        actual_cue_qpc(actual_cues, contract.steps, "return-start")?,
        actual_cue_qpc(actual_cues, contract.steps, "return-stop")?,
        qpc_frequency,
        contract,
        tolerance_ms,
    )?;
    Ok(RawInputMetrics {
        evidence_event_count: events.len(),
        raw_event_count: events.len(),
        outbound_hold_ms,
        return_hold_ms,
        mouse_move_events,
        keyboard_events,
    })
}

fn actual_cue_qpc(
    actual_cues: &[ActualActionCue],
    steps: &[OperatorActionStep],
    marker_id: &str,
) -> anyhow::Result<i64> {
    let step_index = steps
        .iter()
        .position(|step| step.marker_id == marker_id)
        .context("action-plan marker is unknown")?;
    let cue = actual_cues
        .iter()
        .find(|cue| cue.step_index == step_index)
        .context("required operator cue was not emitted")?;
    ensure!(cue.actual_qpc >= cue.scheduled_qpc);
    Ok(cue.actual_qpc)
}

fn action_hold_ms(
    events: &[RawInputEvent],
    vk: u16,
    expected_down_qpc: i64,
    expected_up_qpc: i64,
    qpc_frequency: i64,
    contract: CaptureContract,
    tolerance_ms: u64,
) -> anyhow::Result<u64> {
    let tolerance = i64::try_from(i128::from(qpc_frequency) * i128::from(tolerance_ms) / 1_000)?;
    let down = events
        .iter()
        .find(|event| {
            event.event_type == "key_down"
                && event.vk == Some(vk)
                && event.repeat != Some(true)
                && event.qpc.abs_diff(expected_down_qpc) <= tolerance as u64
        })
        .context("required movement key-down was not aligned to its action marker")?;
    let up = events
        .iter()
        .find(|event| event.event_type == "key_up" && event.vk == Some(vk) && event.qpc > down.qpc)
        .with_context(|| format!("movement key {vk} was never released"))?;
    ensure!(
        up.qpc.abs_diff(expected_up_qpc) <= tolerance as u64,
        "movement key {vk} was released outside the +/-{tolerance_ms} ms cue window; hold continuously within the permitted window"
    );
    let held_ms = duration_ms(down.qpc, up.qpc, qpc_frequency)?;
    ensure!(
        (contract.min_input_hold_ms..=contract.max_input_hold_ms).contains(&held_ms),
        "movement hold duration is outside the audited action-plan bound"
    );
    Ok(held_ms)
}

fn audit_fixed_view_controls(events: &[RawInputEvent]) -> anyhow::Result<()> {
    let mut held = std::collections::BTreeSet::new();
    for event in events {
        ensure!(
            matches!(event.event_type.as_str(), "key_down" | "key_up"),
            "fixed-view W/S walk received {}; do not use the mouse in this phase",
            event.event_type
        );
        let vk = event
            .vk
            .context("fixed-view keyboard event is missing its key")?;
        ensure!(
            matches!(vk, 87 | 83),
            "fixed-view walk only permits W/S; received key {vk} (Shift=16, sprint is forbidden)"
        );
        if event.event_type == "key_down" {
            ensure!(
                held.is_empty() || held.contains(&vk),
                "W and S must not be held together"
            );
            held.insert(vk);
        } else {
            ensure!(
                held.remove(&vk),
                "fixed-view key-up has no recorded key-down"
            );
        }
    }
    ensure!(held.is_empty(), "release all W/S keys before capture ends");
    Ok(())
}

fn unique_stable_matrix_source(
    sources: &RenderMatrixSources,
    present_count: usize,
) -> anyhow::Result<(&String, &RenderMatrixSamples)> {
    let stable = sources
        .iter()
        .filter(|(_, samples)| {
            samples.len() as f64 / present_count as f64 >= MIN_RENDER_SOURCE_COVERAGE
        })
        .collect::<Vec<_>>();
    ensure!(
        stable.len() == 1,
        "render-camera matrix source is missing or ambiguous"
    );
    Ok((stable[0].0, stable[0].1))
}

fn render_matrix_source_key(
    matrix: &Value,
    expected_role: depth_lab::MatrixRole,
) -> anyhow::Result<String> {
    let stable: depth_lab::D3d12MatrixSourceBinding = serde_json::from_value(
        matrix
            .get("stable_source")
            .cloned()
            .context("render matrix has no bounded cross-launch stable source")?,
    )?;
    stable
        .validate_for_role(expected_role)
        .map_err(anyhow::Error::msg)?;
    ensure!(
        matrix.get("root_parameter_index").and_then(Value::as_u64)
            == Some(u64::from(stable.descriptor_table.root_parameter_index))
            && matrix.get("cbv_resource_offset").and_then(Value::as_u64)
                == Some(stable.cbv_resource_offset)
            && matrix.get("matrix_offset").and_then(Value::as_u64)
                == Some(u64::from(stable.matrix_offset))
            && matrix.get("transpose").and_then(Value::as_bool) == Some(stable.transpose),
        "render matrix legacy diagnostics contradict its stable source"
    );
    serde_json::to_string(&stable).context("serialize render matrix stable source")
}

fn nearest_pose_position(samples: &[(i64, u64, [f64; 3])], target: i64) -> [f64; 3] {
    samples
        .iter()
        .min_by_key(|sample| sample.0.abs_diff(target))
        .expect("pose samples were checked nonempty")
        .2
}

fn distance3(left: [f64; 3], right: [f64; 3]) -> f64 {
    left.into_iter()
        .zip(right)
        .map(|(left, right)| (left - right).powi(2))
        .sum::<f64>()
        .sqrt()
}

fn matrix_delta(left: RenderMatrix, right: RenderMatrix) -> f64 {
    left.into_iter()
        .flatten()
        .zip(right.into_iter().flatten())
        .map(|(left, right)| (left - right).abs())
        .fold(0.0, f64::max)
}

fn read_bounded_file(path: &Path, maximum_bytes: u64) -> anyhow::Result<Vec<u8>> {
    let metadata = path.metadata()?;
    ensure!(
        metadata.is_file() && metadata.len() > 0 && metadata.len() <= maximum_bytes,
        "evidence file is empty or exceeds its bound: {}",
        path.display()
    );
    Ok(fs::read(path)?)
}

fn sha256_bytes(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn read_bounded_jsonl<T: serde::de::DeserializeOwned>(
    path: &Path,
    compressed: bool,
) -> anyhow::Result<Vec<T>> {
    let metadata = path.metadata()?;
    ensure!(
        metadata.is_file() && metadata.len() <= MAX_JSONL_BYTES,
        "JSONL input exceeds bound"
    );
    let file = File::open(path)?;
    let reader: Box<dyn std::io::BufRead> = if compressed {
        Box::new(std::io::BufReader::new(zstd::stream::read::Decoder::new(
            file,
        )?))
    } else {
        Box::new(std::io::BufReader::new(file))
    };
    let mut rows = Vec::new();
    for line in reader.lines() {
        let line = line?;
        ensure!(!line.trim().is_empty(), "JSONL input contains an empty row");
        rows.push(serde_json::from_str(&line)?);
        ensure!(
            rows.len() <= MAX_JSONL_ROWS,
            "JSONL input exceeds row bound"
        );
    }
    Ok(rows)
}

fn bind_exact_launched_window(pid: u32) -> anyhow::Result<BoundTarget> {
    let tracker = WindowTracker::new()?;
    let candidates = tracker.discover(GAME_ID)?;
    let mut matching = Vec::new();
    for candidate in candidates {
        let target = tracker.bind(&candidate.candidate_id)?;
        if target.process_id == pid {
            matching.push(target);
        }
    }
    ensure!(
        matching.len() == 1,
        "expected exactly one admitted CP2077 HWND owned by early-launch PID {pid}, found {}",
        matching.len()
    );
    Ok(matching.pop().expect("one matching target was checked"))
}

fn validate_recording_target(target: &BoundTarget, pid: u32) -> anyhow::Result<()> {
    ensure!(target.process_id == pid && target.summary.process_id == pid);
    ensure!(target.summary.game_id == GAME_ID);
    ensure!(
        target.executable_sha256.as_deref() == Some(CP2077_EXE_SHA256),
        "bound CP2077 executable does not match the pinned Steam build"
    );
    ensure!(
        target.capture_profile.non_trainable
            // CP2077 raw demo 是本地 debug 路径，永远 legacy v1、永远 non-trainable
            // （issue #855 §D）。目录侧那一位不再由 bundle 声明，改由契约表达。
            && target.catalog_selection.session_contract
                == capture_core::ingest_contract::SessionContract::LocalDevV1
            && target.summary.non_trainable,
        "raw demo target is not consistently non-trainable"
    );
    ensure!(
        target.capture_profile.width == 1_920
            && target.capture_profile.height == 1_080
            && target.capture_profile.fps_num == 30
            && target.capture_profile.fps_den == 1
            && target.client_width == 1_920
            && target.client_height == 1_080,
        "raw demo requires exact 1920x1080 @ 30 fps HWND capture; bound HWND={}x{} (DPI {}), profile={}x{} @ {}/{} fps",
        target.client_width,
        target.client_height,
        target.dpi,
        target.capture_profile.width,
        target.capture_profile.height,
        target.capture_profile.fps_num,
        target.capture_profile.fps_den,
    );
    let authored = target.catalog_selection.recording_profile()?;
    ensure!(
        authored.field_of_view_degrees == Some(75.0)
            && authored.window_mode == "fullscreen_or_borderless"
            && !authored.hdr,
        "CP2077 recording catalog does not carry the canonical capture contract"
    );
    Ok(())
}

fn validate_pr727_settings(bytes: &[u8]) -> anyhow::Result<()> {
    validate_capture_settings(bytes, MainCollectorPurpose::FiveStream)
}

fn validate_metric_calibration_settings(bytes: &[u8]) -> anyhow::Result<()> {
    validate_capture_settings(bytes, MainCollectorPurpose::MetricCalibration)
}

fn validate_capture_settings(bytes: &[u8], purpose: MainCollectorPurpose) -> anyhow::Result<()> {
    let document: Value =
        serde_json::from_slice(bytes).context("invalid CP2077 UserSettings JSON")?;
    ensure!(document.get("version").and_then(Value::as_i64) == Some(SETTINGS_VERSION));
    let (hud_preset, hud_markers) = match purpose {
        MainCollectorPurpose::FiveStream => ("WholeHUDHidden", false),
        MainCollectorPurpose::MetricCalibration => ("WholeHUDVisible", true),
    };
    // Editing the preset token alone left the persisted components visible on
    // the real game. Require both the preset and each independently stored switch.
    for name in HUD_COMPONENTS {
        ensure!(
            unique_setting(&document, "/interface/hud", name)? == &json!(hud_markers),
            "CP2077 HUD component {name} does not match the selected capture purpose"
        );
    }
    for (group, name, expected) in [
        ("/video/display", "Resolution", json!("1920x1080")),
        ("/video/display", "MaximumFPS_OnOff", json!(true)),
        ("/video/display", "MaximumFPS_Value", json!(32)),
        ("/graphics/presets", "ResolutionScaling", json!("Off")),
        (
            "/graphics/presets",
            "DynamicResolutionScaling",
            json!(false),
        ),
        ("/graphics/presets", "FrameGeneration", json!("Off")),
        ("/graphics/presets", "DLSSFrameGen", json!(false)),
        ("/graphics/presets", "FSR3_FrameGeneration", json!(false)),
        ("/graphics/presets", "XESS_FrameGeneration", json!(false)),
        ("/graphics/raytracing", "RayTracing", json!(false)),
        ("/graphics/advanced", "ShadowMeshQuality", json!("Low")),
        ("/graphics/advanced", "LocalShadowsQuality", json!("Low")),
        ("/graphics/advanced", "CascadedShadowsRange", json!("Low")),
        (
            "/graphics/advanced",
            "CascadedShadowsResolution",
            json!("Low"),
        ),
        (
            "/graphics/advanced",
            "DistantShadowsResolution",
            json!("Low"),
        ),
        (
            "/graphics/advanced",
            "VolumetricFogResolution",
            json!("Low"),
        ),
        (
            "/graphics/advanced",
            "VolumetricCloudsQuality",
            json!("Off"),
        ),
        (
            "/graphics/advanced",
            "ScreenSpaceReflectionsQuality",
            json!("Off"),
        ),
        ("/graphics/advanced", "AmbientOcclusion", json!("Off")),
        ("/graphics/advanced", "MirrorQuality", json!("Low")),
        ("/graphics/advanced", "LODPreset", json!("Low")),
        ("/graphics/basic", "FieldOfView", json!(75.0)),
        ("/interface/hud", "hud_presets", json!(hud_preset)),
        ("/interface/hud", "crosshairs", json!(false)),
        ("/interface/hud", "hud_markers", json!(hud_markers)),
        ("/interface", "HitIndicator", json!(false)),
        ("/gameplay/misc", "Tutorials", json!(false)),
        ("/accessibility/subtitles", "Cinematic", json!(false)),
        ("/accessibility/subtitles", "Radio", json!(false)),
        ("/accessibility/subtitles", "Overheads", json!(false)),
        (
            "/accessibility/subtitles",
            "CinematicForceKiroshiTexts",
            json!(false),
        ),
        (
            "/accessibility/subtitles",
            "OverheadsForceKiroshiTexts",
            json!(false),
        ),
        ("/accessibility/subtitles", "BackgroundOpacity", json!(0.0)),
        ("/gameplay/muteholocalls", "ncpd_dispatcher", json!(true)),
        (
            "/gameplay/muteholocalls",
            "fixer_briefs_and_debriefs",
            json!(true),
        ),
        ("/graphics/basic", "FilmGrain", json!(false)),
        ("/graphics/basic", "ChromaticAberration", json!(false)),
        ("/graphics/basic", "DepthOfField", json!(false)),
        ("/graphics/basic", "LensFlares", json!(false)),
        ("/graphics/basic", "MotionBlur", json!("Off")),
        ("/graphics/basic", "Vignette", json!(false)),
    ] {
        let actual = unique_setting(&document, group, name)?;
        ensure!(
            actual == &expected,
            "CP2077 setting {group}/{name} drifted: expected {expected}, got {actual}"
        );
    }
    Ok(())
}

fn audit_pr727_settings_for_process(
    settings_path: &Path,
    pid: u32,
    process_launch_id: &str,
) -> anyhow::Result<Pr727SettingsRuntimeEvidence> {
    audit_settings_for_process(
        settings_path,
        pid,
        process_launch_id,
        MainCollectorPurpose::FiveStream,
    )
}

fn audit_metric_calibration_settings_for_process(
    settings_path: &Path,
    pid: u32,
    process_launch_id: &str,
) -> anyhow::Result<Pr727SettingsRuntimeEvidence> {
    audit_settings_for_process(
        settings_path,
        pid,
        process_launch_id,
        MainCollectorPurpose::MetricCalibration,
    )
}

fn audit_settings_for_process(
    settings_path: &Path,
    pid: u32,
    process_launch_id: &str,
    purpose: MainCollectorPurpose,
) -> anyhow::Result<Pr727SettingsRuntimeEvidence> {
    let process_creation_time_100ns = process_creation_time_100ns(pid)?;
    let settings_directory = settings_path
        .parent()
        .context("CP2077 settings path has no parent directory")?;
    let (attestation_prefix, snapshot_prefix, attestation_schema, evidence_schema) = match purpose {
        MainCollectorPurpose::FiveStream => (
            SETTINGS_ATTESTATION_PREFIX,
            SETTINGS_SNAPSHOT_PREFIX,
            SETTINGS_ATTESTATION_SCHEMA,
            SETTINGS_EVIDENCE_SCHEMA,
        ),
        MainCollectorPurpose::MetricCalibration => (
            METRIC_SETTINGS_ATTESTATION_PREFIX,
            METRIC_SETTINGS_SNAPSHOT_PREFIX,
            METRIC_SETTINGS_ATTESTATION_SCHEMA,
            METRIC_SETTINGS_EVIDENCE_SCHEMA,
        ),
    };
    let attestation_path = newest_settings_attestation(settings_directory, attestation_prefix)?;
    let (attestation_bytes, attestation_last_write_100ns) =
        read_stable_file_with_last_write(&attestation_path)?;
    let attestation: Pr727SettingsAttestation = serde_json::from_slice(&attestation_bytes)
        .context("invalid CP2077 issue735 settings attestation JSON")?;
    ensure!(
        attestation.schema == attestation_schema
            && attestation.settings_version == SETTINGS_VERSION
            && !attestation.applied_utc.trim().is_empty()
            && attestation.settings_passed
            && attestation.settings_ready
            && !attestation.recording_ready,
        "CP2077 issue735 settings attestation changes the fail-closed contract"
    );
    ensure!(
        attestation.purpose.as_deref().unwrap_or("formal")
            == match purpose {
                MainCollectorPurpose::FiveStream => "formal",
                MainCollectorPurpose::MetricCalibration => "metric_calibration",
            },
        "CP2077 settings attestation purpose does not match this run"
    );
    ensure!(
        is_timestamped_settings_name(&attestation.settings_snapshot_file_name, snapshot_prefix),
        "CP2077 issue735 settings attestation names an invalid snapshot"
    );
    let attestation_name = attestation_path
        .file_name()
        .and_then(|value| value.to_str())
        .context("CP2077 issue735 settings attestation has a non-UTF8 file name")?;
    let timestamp = attestation_name
        .strip_prefix(attestation_prefix)
        .and_then(|value| value.strip_suffix(".json"))
        .context("CP2077 issue735 settings attestation file name is invalid")?;
    ensure!(
        attestation.settings_snapshot_file_name == format!("{snapshot_prefix}{timestamp}.json"),
        "CP2077 issue735 settings attestation does not name its paired snapshot"
    );

    let snapshot_path = settings_directory.join(&attestation.settings_snapshot_file_name);
    ensure!(
        snapshot_path.parent() == Some(settings_directory),
        "CP2077 issue735 settings snapshot escaped the settings directory"
    );
    let (snapshot_bytes, snapshot_last_write_100ns) =
        read_stable_file_with_last_write(&snapshot_path)?;
    let (live_bytes, current_live_settings_last_write_100ns) =
        read_stable_file_with_last_write(settings_path)?;
    let source = classify_settings_state(
        &snapshot_bytes,
        &attestation.settings_sha256,
        &live_bytes,
        attestation.live_settings_last_write_100ns,
        snapshot_last_write_100ns,
        attestation_last_write_100ns,
        current_live_settings_last_write_100ns,
        process_creation_time_100ns,
        purpose,
    )?;

    Ok(Pr727SettingsRuntimeEvidence {
        schema: evidence_schema,
        purpose: purpose.as_str(),
        process_launch_id: process_launch_id.to_owned(),
        process_id: pid,
        source,
        settings_sha256: attestation.settings_sha256,
        current_live_settings_sha256: sha256_bytes(&live_bytes),
        process_creation_time_100ns,
        applied_live_settings_last_write_100ns: attestation.live_settings_last_write_100ns,
        current_live_settings_last_write_100ns,
        prelaunch_lead_ms: process_creation_time_100ns
            .saturating_sub(attestation.live_settings_last_write_100ns)
            / 10_000,
        runtime_fov_memory_evidence: RENDER_CAMERA_REPORT_FILE,
        runtime_hud_subtitle_visual_evidence: "video.mp4",
        runtime_hud_subtitle_visual_classification: match purpose {
            MainCollectorPurpose::FiveStream => {
                "auditable_bound_hwnd_video_not_automatically_classified"
            }
            MainCollectorPurpose::MetricCalibration => {
                "visible_objective_hud_allowed_for_metric_calibration_only"
            }
        },
        settings_ready: true,
        recording_ready: false,
        non_trainable: true,
    })
}

#[allow(clippy::too_many_arguments)]
fn classify_settings_state(
    snapshot_bytes: &[u8],
    attested_snapshot_sha256: &str,
    live_bytes: &[u8],
    applied_live_last_write_100ns: u64,
    snapshot_last_write_100ns: u64,
    attestation_last_write_100ns: u64,
    current_live_last_write_100ns: u64,
    process_creation_time_100ns: u64,
    purpose: MainCollectorPurpose,
) -> anyhow::Result<&'static str> {
    let validate = |bytes: &[u8]| match purpose {
        MainCollectorPurpose::FiveStream => validate_pr727_settings(bytes),
        MainCollectorPurpose::MetricCalibration => validate_metric_calibration_settings(bytes),
    };
    validate(snapshot_bytes).context(
        "the prelaunch CP2077 settings snapshot does not satisfy the selected capture purpose",
    )?;
    ensure!(
        sha256_bytes(snapshot_bytes) == attested_snapshot_sha256,
        "the prelaunch CP2077 settings snapshot hash does not match its attestation"
    );
    ensure!(
        applied_live_last_write_100ns <= snapshot_last_write_100ns
            && snapshot_last_write_100ns <= attestation_last_write_100ns
            && attestation_last_write_100ns <= process_creation_time_100ns,
        "the CP2077 settings attestation was not completed before this process started"
    );
    ensure!(
        process_creation_time_100ns.saturating_sub(applied_live_last_write_100ns)
            <= MAX_PRELAUNCH_SETTINGS_LEAD_100NS,
        "the CP2077 settings recipe was not applied within five minutes before this process started"
    );

    if validate(live_bytes).is_ok() {
        return Ok("live_file_exact");
    }
    ensure!(
        current_live_last_write_100ns > process_creation_time_100ns,
        "the live CP2077 settings drift predates this process; reapply the recipe after exiting and launch again"
    );
    Ok("prelaunch_snapshot_plus_postlaunch_live_rewrite")
}

fn newest_settings_attestation(
    settings_directory: &Path,
    attestation_prefix: &str,
) -> anyhow::Result<PathBuf> {
    let mut candidates = fs::read_dir(settings_directory)?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            is_timestamped_settings_name(&name, attestation_prefix).then_some((name, entry.path()))
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| left.0.cmp(&right.0));
    candidates.pop().map(|(_, path)| path).context(
        "no CP2077 issue735 settings attestation exists; exit the game and rerun the setter",
    )
}

fn is_timestamped_settings_name(name: &str, prefix: &str) -> bool {
    let Some(timestamp) = name
        .strip_prefix(prefix)
        .and_then(|value| value.strip_suffix(".json"))
    else {
        return false;
    };
    timestamp.len() == 15
        && timestamp.as_bytes().get(8) == Some(&b'-')
        && timestamp
            .bytes()
            .enumerate()
            .all(|(index, byte)| index == 8 || byte.is_ascii_digit())
}

fn read_stable_file_with_last_write(path: &Path) -> anyhow::Result<(Vec<u8>, u64)> {
    let before = fs::metadata(path)?;
    ensure!(
        before.is_file(),
        "expected a regular settings evidence file"
    );
    let bytes = fs::read(path)?;
    let after = fs::metadata(path)?;
    ensure!(
        before.len() == after.len()
            && before.last_write_time() == after.last_write_time()
            && after.len() == u64::try_from(bytes.len())?,
        "settings evidence file changed while it was being audited"
    );
    Ok((bytes, after.last_write_time()))
}

fn process_creation_time_100ns(pid: u32) -> anyhow::Result<u64> {
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }
        .with_context(|| format!("OpenProcess({pid}) failed for PR #727 settings audit"))?;
    let result = query_process_creation_time(process);
    let _ = unsafe { CloseHandle(process) };
    result
}

fn query_process_creation_time(process: HANDLE) -> anyhow::Result<u64> {
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    unsafe {
        GetProcessTimes(
            process,
            &raw mut creation,
            &raw mut exit,
            &raw mut kernel,
            &raw mut user,
        )
    }
    .context("GetProcessTimes failed for PR #727 settings audit")?;
    Ok((u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime))
}

fn unique_setting<'a>(document: &'a Value, group: &str, name: &str) -> anyhow::Result<&'a Value> {
    let groups = document
        .get("data")
        .and_then(Value::as_array)
        .context("CP2077 settings has no data array")?
        .iter()
        .filter(|entry| entry.get("group_name").and_then(Value::as_str) == Some(group))
        .collect::<Vec<_>>();
    ensure!(
        groups.len() == 1,
        "expected exactly one CP2077 group {group}"
    );
    let options = groups[0]
        .get("options")
        .and_then(Value::as_array)
        .context("CP2077 settings group has no options")?
        .iter()
        .filter(|entry| entry.get("name").and_then(Value::as_str) == Some(name))
        .collect::<Vec<_>>();
    ensure!(
        options.len() == 1,
        "expected exactly one CP2077 setting {group}/{name}"
    );
    options[0]
        .get("value")
        .context("CP2077 setting has no value")
}

fn validate_raw_config(session_dir: &Path, contract: CaptureContract) -> anyhow::Result<()> {
    for name in ["raw-depth-demo-config.json", "raw-depth-demo-profile.json"] {
        ensure!(
            session_dir.join(name).is_file(),
            "raw demo session has no {name}"
        );
    }
    for name in [
        ARM_FILE,
        STOP_FILE,
        TELEMETRY_FILE,
        "video.mp4",
        "rgb-index.json",
        ACTION_PLAN_FILE,
        ACTION_MARKERS_FILE,
        POSE_CAMERA_LOOP_QC_FILE,
        PR727_SETTINGS_EVIDENCE_FILE,
        METRIC_SETTINGS_EVIDENCE_FILE,
        METRIC_POSE_FILE,
        METRIC_FRAME_FILE,
        METRIC_OBSERVATIONS_TEMPLATE_FILE,
        METRIC_CAPTURE_REPORT_FILE,
        AUTONOMOUS_INPUT_LEDGER_FILE,
        "cp2077-pr405-frame-evidence.jsonl",
        "cp2077-final-pose.jsonl",
        "raw-demo-result.json",
    ] {
        ensure!(
            !session_dir.join(name).exists(),
            "raw demo session contains stale {name}"
        );
    }
    let config: Value =
        serde_json::from_slice(&fs::read(session_dir.join("raw-depth-demo-config.json"))?)?;
    let configured_contract = config
        .get("capture_contract")
        .and_then(Value::as_str)
        .unwrap_or(depth_lab::ISSUE735_LOCAL_CONTRACT);
    let configured_duration = config
        .get("duration_ms")
        .and_then(Value::as_u64)
        .unwrap_or(depth_lab::ISSUE735_LOCAL_DURATION_MS);
    ensure!(
        config.get("schema").and_then(Value::as_str) == Some("dm-cp2077-raw-depth-demo-config/v1")
            && config.get("output_dir").and_then(Value::as_str) == Some(".")
            && config.get("profile_file").and_then(Value::as_str)
                == Some("raw-depth-demo-profile.json")
            && config
                .get("max_frames")
                .and_then(Value::as_u64)
                .is_some_and(
                    |frames| (contract.min_depth_frames..=contract.max_depth_frames)
                        .contains(&frames)
                )
            && config.get("recording_ready") == Some(&Value::Bool(false))
            && config.get("non_trainable") == Some(&Value::Bool(true))
            && config.get("upload_allowed") == Some(&Value::Bool(false))
            && config.get("formal_start_run") == Some(&Value::Bool(false)),
        "raw depth config weakens the local-only capture contract"
    );
    ensure!(
        configured_contract == contract.name && configured_duration == contract.duration_ms,
        "raw depth config does not match the selected capture contract"
    );
    ensure!(
        config
            .get("manual_stop")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            == (contract.max_duration_ms == 240_000),
        "raw depth config manual-stop mode does not match the coordinator"
    );
    let profile: depth_lab::RawDepthDemoProfile =
        serde_json::from_slice(&fs::read(session_dir.join("raw-depth-demo-profile.json"))?)?;
    profile.validate().map_err(anyhow::Error::msg)?;
    ensure!(
        profile.capture_contract == contract.name && profile.duration_ms == contract.duration_ms,
        "raw depth profile does not match the selected capture contract"
    );
    ensure!(
        session_dir.join("raw-depth-index.jsonl").is_file()
            && session_dir.join("raw-depth").is_dir(),
        "raw depth probe has not initialized this session"
    );
    if contract.name == depth_lab::PR405_LOCAL_CONTRACT {
        ensure!(
            session_dir
                .join("cp2077-pr405-package-input.json")
                .is_file(),
            "PR405 requires a package input with independently measured metric calibration before capture"
        );
    }
    Ok(())
}

fn validate_session_dir(path: &Path) -> anyhow::Result<PathBuf> {
    ensure!(path.is_absolute(), "raw demo session path must be absolute");
    let session = path.canonicalize()?;
    let local =
        PathBuf::from(std::env::var_os("LOCALAPPDATA").context("LOCALAPPDATA is unavailable")?)
            .canonicalize()?;
    let trusted = local.join("DataMiner/depth-raw-demo/cp2077");
    ensure!(
        path_is_within_case_insensitive(&session, &trusted) && session != trusted,
        "raw demo session is outside the private CP2077 raw-demo root"
    );
    Ok(session)
}

fn path_is_within_case_insensitive(path: &Path, parent: &Path) -> bool {
    let normalize = |value: &Path| {
        value
            .to_string_lossy()
            .replace('/', "\\")
            .trim_end_matches('\\')
            .to_ascii_lowercase()
    };
    let path = normalize(path);
    let parent = normalize(parent);
    path == parent || path.starts_with(&format!("{parent}\\"))
}

fn default_settings_path() -> PathBuf {
    PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap_or_default())
        .join("CD Projekt Red/Cyberpunk 2077/UserSettings.json")
}

fn write_arm_request(
    session_dir: &Path,
    launch_id: &str,
    contract: CaptureContract,
    manual_stop: bool,
) -> anyhow::Result<()> {
    write_arm_request_with_feedback(session_dir, launch_id, contract, manual_stop, None)
}

fn write_arm_request_with_feedback(
    session_dir: &Path,
    launch_id: &str,
    contract: CaptureContract,
    manual_stop: bool,
    feedback: Option<&depth_lab::cp2077_raw_feedback::RawSelectedRequest>,
) -> anyhow::Result<()> {
    let mut request = json!({
        "schema": if feedback.is_some() {"dm-cp2077-raw-depth-demo-arm-request/v2"} else {ARM_SCHEMA},
        "process_launch_id":launch_id,"capture_contract":contract.name,"duration_ms":contract.duration_ms,
        "target_depth_hz":contract.target_depth_hz,"manual_stop":manual_stop });
    if let Some(feedback) = feedback {
        request["selected_present"] = serde_json::to_value(feedback)?;
    }
    write_request_atomic(session_dir, ARM_FILE, &request)
}

fn write_stop_request(session_dir: &Path, launch_id: &str) -> anyhow::Result<()> {
    write_request_atomic(
        session_dir,
        STOP_FILE,
        &json!({"schema": STOP_SCHEMA, "process_launch_id": launch_id}),
    )
}

fn write_stop_request_if_absent(session_dir: &Path, launch_id: &str) -> anyhow::Result<()> {
    if session_dir.join(STOP_FILE).exists() {
        return Ok(());
    }
    write_stop_request(session_dir, launch_id)
}

struct RawDepthStopGuard<'a> {
    session_dir: &'a Path,
    launch_id: &'a str,
    stopped: bool,
}

impl<'a> RawDepthStopGuard<'a> {
    fn new(session_dir: &'a Path, launch_id: &'a str) -> Self {
        Self {
            session_dir,
            launch_id,
            stopped: false,
        }
    }

    fn stop(&mut self) -> anyhow::Result<()> {
        if !self.stopped {
            write_stop_request_if_absent(self.session_dir, self.launch_id)?;
            self.stopped = true;
        }
        Ok(())
    }
}

impl Drop for RawDepthStopGuard<'_> {
    fn drop(&mut self) {
        if let Err(error) = self.stop() {
            eprintln!("[CP2077] failed to stop already armed raw depth: {error:#}");
        }
    }
}

fn write_request_atomic(session_dir: &Path, name: &str, value: &Value) -> anyhow::Result<()> {
    let destination = session_dir.join(name);
    ensure!(!destination.exists(), "refusing to overwrite {name}");
    let temporary = session_dir.join(format!(".{name}.{}.tmp", uuid::Uuid::new_v4().simple()));
    let result = (|| -> anyhow::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        serde_json::to_writer(&mut file, value)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&temporary, &destination)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn validate_depth_telemetry(
    bytes: &[u8],
    launch_id: &str,
    contract: CaptureContract,
) -> anyhow::Result<Value> {
    let telemetry: Value = serde_json::from_slice(bytes)?;
    ensure!(telemetry.get("schema").and_then(Value::as_str) == Some(TELEMETRY_SCHEMA));
    ensure!(
        telemetry.get("process_launch_id").and_then(Value::as_str) == Some(launch_id),
        "raw depth telemetry belongs to another launch"
    );
    ensure!(
        telemetry.get("capture_contract").and_then(Value::as_str) == Some(contract.name)
            && telemetry.get("duration_ms").and_then(Value::as_u64) == Some(contract.duration_ms)
            && telemetry
                .get("manual_stop")
                .and_then(Value::as_bool)
                .unwrap_or(false)
                == (contract.max_duration_ms == 240_000)
            && telemetry.get("target_depth_hz").and_then(Value::as_u64)
                == Some(contract.target_depth_hz)
            && telemetry.get("non_trainable") == Some(&Value::Bool(true))
            && telemetry.get("production_certified") == Some(&Value::Bool(false))
            && telemetry.get("upload_allowed") == Some(&Value::Bool(false))
            && telemetry.get("deliverable") == Some(&Value::Bool(false))
            && telemetry.get("recording_ready") == Some(&Value::Bool(false))
            && telemetry.get("formal_start_run_used") == Some(&Value::Bool(false)),
        "raw depth telemetry weakens or changes the coordinated demo contract"
    );
    ensure!(
        telemetry.get("fatal_error").is_some_and(Value::is_null),
        "raw depth telemetry reports a fatal error"
    );
    for field in [
        "writer_queue_dropped",
        "present_stamp_dropped",
        "capture_pool_dropped",
    ] {
        ensure!(
            required_u64(&telemetry, field)? == 0,
            "raw depth reported {field}"
        );
    }
    ensure!(
        required_u64(&telemetry, "submitted_frames")?
            == required_u64(&telemetry, "written_frames")?,
        "raw depth writer did not publish every submitted frame"
    );
    Ok(telemetry)
}

fn required_u64(value: &Value, field: &str) -> anyhow::Result<u64> {
    value
        .get(field)
        .and_then(Value::as_u64)
        .with_context(|| format!("raw depth telemetry has no {field}"))
}

fn wait_for_file(path: &Path, timeout: Duration) -> anyhow::Result<PathBuf> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.is_file() {
            return Ok(path.to_path_buf());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    anyhow::bail!("timed out waiting for {}", path.display())
}

fn duration_ms(start: i64, end: i64, frequency: i64) -> anyhow::Result<u64> {
    ensure!(frequency > 0 && end >= start, "invalid RGB QPC duration");
    u64::try_from(end - start)?
        .checked_mul(1_000)
        .context("RGB QPC duration overflow")
        .map(|ticks| ticks / u64::try_from(frequency).expect("positive QPC frequency"))
}

fn ensure_duration(
    label: &str,
    milliseconds: u64,
    contract: CaptureContract,
) -> anyhow::Result<()> {
    ensure!(
        (contract.min_duration_ms..=contract.max_duration_ms).contains(&milliseconds),
        "{label} duration {milliseconds} ms is outside the selected capture contract"
    );
    Ok(())
}

fn validate_launch_id(value: &str) -> anyhow::Result<()> {
    ensure!(
        value.len() == 32
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "process launch id must be 32 lowercase hex digits"
    );
    Ok(())
}

fn sha256_file(path: &Path) -> anyhow::Result<String> {
    let mut file = File::open(path)?;
    let mut buffer = [0_u8; 1024 * 1024];
    let mut hasher = Sha256::new();
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("sha256:{:x}", hasher.finalize()))
}

fn relative_path(root: &Path, path: &Path) -> anyhow::Result<String> {
    Ok(path
        .strip_prefix(root)
        .context("RGB artifact escaped raw demo session")?
        .to_string_lossy()
        .replace('\\', "/"))
}

fn write_json_create_new(path: &Path, value: &impl Serialize) -> anyhow::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    serde_json::to_writer_pretty(&mut file, value)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}

fn write_jsonl_create_new(path: &Path, rows: &[impl Serialize]) -> anyhow::Result<()> {
    ensure!(
        !rows.is_empty(),
        "refusing to publish an empty JSONL ledger"
    );
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    for row in rows {
        serde_json::to_writer(&mut file, row)?;
        file.write_all(b"\n")?;
    }
    file.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canonical_capture_profile() -> CaptureProfile {
        CaptureProfile {
            profile_id: "cyberpunk-2077-steam-recording-v1".into(),
            non_trainable: true,
            capture_backend: "native_wgc_window_v1".into(),
            encoder_backend: "mf_nvidia_hardware_v1".into(),
            width: 1920,
            height: 1080,
            fps_num: 30,
            fps_den: 1,
            bitrate_bps: 16_000_000,
            max_bitrate_bps: 24_000_000,
            gop_frames: 120,
            segment_seconds: CATALOG_SEGMENT_SECONDS,
            test_segments: CATALOG_TEST_SEGMENTS,
            accepted_encoder_relaxations: None,
            audio: None,
        }
    }

    #[test]
    fn local_depth_trial_uses_one_segment_without_changing_the_capture_horizon() {
        let catalog = canonical_capture_profile();
        let (effective, policy) = local_depth_trial_profile(&catalog).unwrap();

        assert_eq!(
            (catalog.segment_seconds, catalog.test_segments),
            (CATALOG_SEGMENT_SECONDS, CATALOG_TEST_SEGMENTS)
        );
        assert_eq!(
            (effective.segment_seconds, effective.test_segments),
            (LOCAL_DEPTH_SEGMENT_SECONDS, LOCAL_DEPTH_TEST_SEGMENTS)
        );
        assert_eq!(
            catalog.segment_seconds * catalog.test_segments,
            effective.segment_seconds * effective.test_segments
        );
        assert_eq!(
            (
                policy.catalog_segment_seconds,
                policy.catalog_test_segments,
                policy.effective_segment_seconds,
                policy.effective_test_segments,
            ),
            (
                CATALOG_SEGMENT_SECONDS,
                CATALOG_TEST_SEGMENTS,
                LOCAL_DEPTH_SEGMENT_SECONDS,
                LOCAL_DEPTH_TEST_SEGMENTS,
            )
        );

        let mut drifted = catalog;
        drifted.segment_seconds = 60;
        assert!(local_depth_trial_profile(&drifted).is_err());
    }

    #[test]
    fn main_rgb_materialization_copies_only_for_cross_volume_links() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source.bin");
        let copied = root.path().join("copied.bin");
        fs::write(&source, b"rgb-segment-bytes").unwrap();

        link_or_copy_main_rgb_file_with(&source, &copied, |_, _| {
            Err(io::Error::from_raw_os_error(ERROR_NOT_SAME_DEVICE))
        })
        .unwrap();
        assert_eq!(fs::read(&copied).unwrap(), b"rgb-segment-bytes");

        let refused = root.path().join("refused.bin");
        assert!(
            link_or_copy_main_rgb_file_with(&source, &refused, |_, _| {
                Err(io::Error::from_raw_os_error(5))
            })
            .is_err()
        );
        assert!(!refused.exists());
    }

    #[test]
    fn pr405_identity_and_axis_gates_reject_near_or_unresolved_evidence() {
        let expected = Pr405PresentIdentity {
            present_seq: 42,
            qpc: 1_000,
        };
        assert!(ensure_exact_pr405_identity(expected, expected, "test").is_ok());
        assert!(
            ensure_exact_pr405_identity(
                Pr405PresentIdentity {
                    present_seq: 42,
                    qpc: 1_001,
                },
                expected,
                "test",
            )
            .is_err()
        );
        assert!(
            ensure_exact_pr405_identity(
                Pr405PresentIdentity {
                    present_seq: 43,
                    qpc: 1_000,
                },
                expected,
                "test",
            )
            .is_err()
        );
        assert!(
            ensure_resolved_pr405_axis_mapping(&json!({
                "axis_mapping": "unresolved_requires_motion_calibration"
            }))
            .is_err()
        );
        assert!(
            ensure_resolved_pr405_axis_mapping(&json!({
                "axis_mapping": PR405_RESOLVED_AXIS_MAPPING
            }))
            .is_ok()
        );
    }

    #[test]
    fn render_matrix_key_requires_stable_d3d12_identity_not_process_tag() {
        let matrix = json!({
            "source_tag": 987,
            "root_parameter_index": 18,
            "cbv_resource_offset": 0,
            "matrix_offset": 0,
            "transpose": true,
            "stable_source": {
                "descriptor_table": {
                    "root_signature_sha256": format!("sha256:{}", "1".repeat(64)),
                    "root_parameter_index": 18,
                    "descriptor_range_index": 0,
                    "descriptor_offset_in_range": 0,
                    "descriptor_count": 1,
                    "base_shader_register": 7,
                    "register_space": 0
                },
                "cbv_resource_offset": 0,
                "declared_cbv_bytes": 256,
                "matrix_offset": 0,
                "encoding": "f32_little_endian",
                "layout": "row_major",
                "transpose": true,
                "role": "view",
                "draw_relation": "exact_selected_depth_dsv_draw"
            }
        });
        let key = render_matrix_source_key(&matrix, depth_lab::MatrixRole::View).unwrap();
        assert!(!key.contains("987"));
        let mut other_process = matrix.clone();
        other_process["source_tag"] = json!(3);
        assert_eq!(
            key,
            render_matrix_source_key(&other_process, depth_lab::MatrixRole::View).unwrap()
        );

        let mut missing = matrix;
        missing.as_object_mut().unwrap().remove("stable_source");
        assert!(render_matrix_source_key(&missing, depth_lab::MatrixRole::View).is_err());

        let mut wrong_role = other_process;
        wrong_role["stable_source"]["role"] = json!("projection");
        assert!(render_matrix_source_key(&wrong_role, depth_lab::MatrixRole::View).is_err());
    }

    fn watermark_means(seq: u16) -> Vec<u8> {
        let encoded = depth_lab::watermark::encode(seq, 0, 0);
        (0..depth_lab::watermark::TOTAL_BLOCKS)
            .map(|index| {
                let left = index * depth_lab::watermark::BLOCK_PX;
                u8::from(encoded.white.iter().any(|rect| rect.left == left)) * 255
            })
            .collect()
    }

    #[test]
    fn metric_marker_identity_uses_player_present_without_a_depth_row() {
        let mut rgb = vec![
            Pr405RgbAuthorRow {
                session_frame: 10,
                source_qpc: 900_000,
                duplicate: false,
                present_seq: None,
                present_qpc: None,
                keys: Vec::new(),
                events: Vec::new(),
            },
            Pr405RgbAuthorRow {
                session_frame: 11,
                source_qpc: 1_270_000,
                duplicate: false,
                present_seq: None,
                present_qpc: None,
                keys: Vec::new(),
                events: Vec::new(),
            },
        ];
        let samples = std::collections::BTreeMap::from([
            (10, watermark_means(40)),
            (11, watermark_means(41)),
        ]);
        let player_presents = std::collections::BTreeMap::from([(
            41,
            std::collections::BTreeSet::from([(65_577, 1_000_000)]),
        )]);

        resolve_metric_selected_rgb_present_identities(&mut rgb, &[11], &samples, &player_presents)
            .unwrap();

        assert_eq!(rgb[0].present_seq, None);
        assert_eq!(rgb[1].present_seq, Some(65_577));
        assert_eq!(rgb[1].present_qpc, Some(1_000_000));
    }

    fn issue_contract() -> CaptureContract {
        capture_contract(
            depth_lab::ISSUE735_LOCAL_CONTRACT,
            depth_lab::ISSUE735_LOCAL_DURATION_MS,
        )
        .unwrap()
    }

    fn pr405_contract() -> CaptureContract {
        capture_contract(
            depth_lab::PR405_LOCAL_CONTRACT,
            depth_lab::PR405_LOCAL_DURATION_MS,
        )
        .unwrap()
    }

    fn settings() -> Value {
        let mut groups = std::collections::BTreeMap::<String, Vec<Value>>::new();
        for (group, name, value) in [
            ("/video/display", "Resolution", json!("1920x1080")),
            ("/video/display", "MaximumFPS_OnOff", json!(true)),
            ("/video/display", "MaximumFPS_Value", json!(32)),
            ("/graphics/presets", "ResolutionScaling", json!("Off")),
            (
                "/graphics/presets",
                "DynamicResolutionScaling",
                json!(false),
            ),
            ("/graphics/presets", "FrameGeneration", json!("Off")),
            ("/graphics/presets", "DLSSFrameGen", json!(false)),
            ("/graphics/presets", "FSR3_FrameGeneration", json!(false)),
            ("/graphics/presets", "XESS_FrameGeneration", json!(false)),
            ("/graphics/raytracing", "RayTracing", json!(false)),
            ("/graphics/advanced", "ShadowMeshQuality", json!("Low")),
            ("/graphics/advanced", "LocalShadowsQuality", json!("Low")),
            ("/graphics/advanced", "CascadedShadowsRange", json!("Low")),
            (
                "/graphics/advanced",
                "CascadedShadowsResolution",
                json!("Low"),
            ),
            (
                "/graphics/advanced",
                "DistantShadowsResolution",
                json!("Low"),
            ),
            (
                "/graphics/advanced",
                "VolumetricFogResolution",
                json!("Low"),
            ),
            (
                "/graphics/advanced",
                "VolumetricCloudsQuality",
                json!("Off"),
            ),
            (
                "/graphics/advanced",
                "ScreenSpaceReflectionsQuality",
                json!("Off"),
            ),
            ("/graphics/advanced", "AmbientOcclusion", json!("Off")),
            ("/graphics/advanced", "MirrorQuality", json!("Low")),
            ("/graphics/advanced", "LODPreset", json!("Low")),
            ("/graphics/basic", "FieldOfView", json!(75.0)),
            ("/interface/hud", "hud_presets", json!("WholeHUDHidden")),
            ("/interface/hud", "crosshairs", json!(false)),
            ("/interface/hud", "hud_markers", json!(false)),
            ("/interface", "HitIndicator", json!(false)),
            ("/gameplay/misc", "Tutorials", json!(false)),
            ("/accessibility/subtitles", "Cinematic", json!(false)),
            ("/accessibility/subtitles", "Radio", json!(false)),
            ("/accessibility/subtitles", "Overheads", json!(false)),
            (
                "/accessibility/subtitles",
                "CinematicForceKiroshiTexts",
                json!(false),
            ),
            (
                "/accessibility/subtitles",
                "OverheadsForceKiroshiTexts",
                json!(false),
            ),
            ("/accessibility/subtitles", "BackgroundOpacity", json!(0.0)),
            ("/gameplay/muteholocalls", "ncpd_dispatcher", json!(true)),
            (
                "/gameplay/muteholocalls",
                "fixer_briefs_and_debriefs",
                json!(true),
            ),
            ("/graphics/basic", "FilmGrain", json!(false)),
            ("/graphics/basic", "ChromaticAberration", json!(false)),
            ("/graphics/basic", "DepthOfField", json!(false)),
            ("/graphics/basic", "LensFlares", json!(false)),
            ("/graphics/basic", "MotionBlur", json!("Off")),
            ("/graphics/basic", "Vignette", json!(false)),
        ] {
            groups
                .entry(group.into())
                .or_default()
                .push(json!({"name": name, "value": value}));
        }
        for name in HUD_COMPONENTS {
            groups
                .entry("/interface/hud".into())
                .or_default()
                .push(json!({"name": name, "value": false}));
        }
        json!({
            "version": SETTINGS_VERSION,
            "data": groups.into_iter().map(|(group_name, options)| {
                json!({"group_name": group_name, "options": options})
            }).collect::<Vec<_>>()
        })
    }

    #[test]
    fn pr727_settings_are_required_exactly() {
        let authored = settings();
        validate_pr727_settings(&serde_json::to_vec(&authored).unwrap()).unwrap();
        let mut drifted = authored;
        let fov = drifted["data"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .flat_map(|group| group["options"].as_array_mut().unwrap())
            .find(|option| option["name"] == "FieldOfView")
            .unwrap();
        fov["value"] = json!(90.0);
        assert!(validate_pr727_settings(&serde_json::to_vec(&drifted).unwrap()).is_err());
    }

    #[test]
    fn hidden_preset_does_not_allow_visible_individual_hud_components() {
        for name in HUD_COMPONENTS {
            let mut drifted = settings();
            let option = drifted["data"]
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .flat_map(|group| group["options"].as_array_mut().unwrap())
                .find(|option| option["name"] == name)
                .unwrap();
            option["value"] = json!(true);
            let error =
                validate_pr727_settings(&serde_json::to_vec(&drifted).unwrap()).unwrap_err();
            assert!(error.to_string().contains(name));
        }
    }

    #[test]
    fn metric_settings_require_visible_objective_hud_but_formal_rejects_it() {
        let mut metric = settings();
        for option in metric["data"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .flat_map(|group| group["options"].as_array_mut().unwrap())
        {
            match option["name"].as_str() {
                Some("hud_presets") => option["value"] = json!("WholeHUDVisible"),
                Some(name) if name == "hud_markers" || HUD_COMPONENTS.contains(&name) => {
                    option["value"] = json!(true)
                }
                _ => {}
            }
        }
        let bytes = serde_json::to_vec(&metric).unwrap();
        validate_metric_calibration_settings(&bytes).unwrap();
        assert!(validate_pr727_settings(&bytes).is_err());
    }

    #[test]
    fn pr727_postlaunch_writeback_is_bound_to_one_process_creation() {
        let snapshot = serde_json::to_vec(&settings()).unwrap();
        let snapshot_hash = sha256_bytes(&snapshot);
        let mut drifted = settings();
        let fov = drifted["data"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .flat_map(|group| group["options"].as_array_mut().unwrap())
            .find(|option| option["name"] == "FieldOfView")
            .unwrap();
        fov["value"] = json!(80.0);
        let live = serde_json::to_vec(&drifted).unwrap();

        assert_eq!(
            classify_settings_state(
                &snapshot,
                &snapshot_hash,
                &live,
                1_000,
                1_100,
                1_200,
                2_000,
                1_500,
                MainCollectorPurpose::FiveStream,
            )
            .unwrap(),
            "prelaunch_snapshot_plus_postlaunch_live_rewrite"
        );
        assert!(
            classify_settings_state(
                &snapshot,
                &snapshot_hash,
                &live,
                1_000,
                1_100,
                1_200,
                2_000,
                2_500,
                MainCollectorPurpose::FiveStream,
            )
            .is_err(),
            "a later process must not reuse the previous launch's snapshot"
        );
    }

    #[test]
    fn pr727_attestation_rejects_bad_hash_and_after_launch_creation() {
        let snapshot = serde_json::to_vec(&settings()).unwrap();
        assert!(
            classify_settings_state(
                &snapshot,
                "sha256:bad",
                &snapshot,
                1_000,
                1_100,
                1_200,
                1_300,
                1_500,
                MainCollectorPurpose::FiveStream,
            )
            .is_err()
        );
        assert!(
            classify_settings_state(
                &snapshot,
                &sha256_bytes(&snapshot),
                &snapshot,
                1_000,
                1_100,
                1_600,
                1_300,
                1_500,
                MainCollectorPurpose::FiveStream,
            )
            .is_err()
        );
    }

    #[test]
    fn pr727_subtitle_and_holocall_surfaces_fail_closed_on_any_drift() {
        for (group_name, option_name, drifted_value) in [
            ("/accessibility/subtitles", "Cinematic", json!(true)),
            ("/accessibility/subtitles", "Radio", json!(true)),
            ("/accessibility/subtitles", "Overheads", json!(true)),
            (
                "/accessibility/subtitles",
                "CinematicForceKiroshiTexts",
                json!(true),
            ),
            (
                "/accessibility/subtitles",
                "OverheadsForceKiroshiTexts",
                json!(true),
            ),
            ("/accessibility/subtitles", "BackgroundOpacity", json!(30.0)),
            ("/gameplay/muteholocalls", "ncpd_dispatcher", json!(false)),
            (
                "/gameplay/muteholocalls",
                "fixer_briefs_and_debriefs",
                json!(false),
            ),
        ] {
            let mut drifted = settings();
            let group = drifted["data"]
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .find(|group| group["group_name"] == group_name)
                .unwrap();
            let option = group["options"]
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .find(|option| option["name"] == option_name)
                .unwrap();
            option["value"] = drifted_value;
            assert!(
                validate_pr727_settings(&serde_json::to_vec(&drifted).unwrap()).is_err(),
                "{group_name}/{option_name} drift unexpectedly passed"
            );
        }
    }

    #[test]
    fn armed_depth_stops_on_failure_before_first_rgb_frame() {
        let root = tempfile::tempdir().unwrap();
        let launch_id = "0123456789abcdef0123456789abcdef";
        let fail_before_started = || -> anyhow::Result<()> {
            write_arm_request(root.path(), launch_id, issue_contract(), false)?;
            let _stop = RawDepthStopGuard::new(root.path(), launch_id);
            anyhow::bail!("RGB failed before Started");
        };
        assert!(fail_before_started().is_err());
        let stop: Value =
            serde_json::from_slice(&fs::read(root.path().join(STOP_FILE)).unwrap()).unwrap();
        assert_eq!(stop["schema"], STOP_SCHEMA);
        assert_eq!(stop["process_launch_id"], launch_id);
    }

    #[test]
    fn armed_depth_cleanup_preserves_an_existing_scheduled_stop() {
        let root = tempfile::tempdir().unwrap();
        let launch_id = "0123456789abcdef0123456789abcdef";
        write_arm_request(root.path(), launch_id, issue_contract(), false).unwrap();
        let mut guard = RawDepthStopGuard::new(root.path(), launch_id);
        write_stop_request(root.path(), launch_id).unwrap();
        let original = fs::read(root.path().join(STOP_FILE)).unwrap();
        guard.stop().unwrap();
        drop(guard);
        assert_eq!(fs::read(root.path().join(STOP_FILE)).unwrap(), original);
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 2);
    }

    #[test]
    fn request_files_are_atomic_strict_and_non_overwriting() {
        let root = tempfile::tempdir().unwrap();
        let launch_id = "0123456789abcdef0123456789abcdef";
        write_arm_request(root.path(), launch_id, issue_contract(), false).unwrap();
        let arm: Value =
            serde_json::from_slice(&fs::read(root.path().join(ARM_FILE)).unwrap()).unwrap();
        assert_eq!(arm["schema"], ARM_SCHEMA);
        assert_eq!(arm["capture_contract"], depth_lab::ISSUE735_LOCAL_CONTRACT);
        assert_eq!(arm["duration_ms"], depth_lab::ISSUE735_LOCAL_DURATION_MS);
        assert_eq!(arm["target_depth_hz"], ISSUE735_TARGET_DEPTH_HZ);
        assert_eq!(arm["manual_stop"], false);
        assert!(write_arm_request(root.path(), launch_id, issue_contract(), false).is_err());
        assert_eq!(
            fs::read_dir(root.path())
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
                .count(),
            0
        );

        let pr405_root = tempfile::tempdir().unwrap();
        write_arm_request(pr405_root.path(), launch_id, pr405_contract(), false).unwrap();
        let pr405: Value =
            serde_json::from_slice(&fs::read(pr405_root.path().join(ARM_FILE)).unwrap()).unwrap();
        assert_eq!(pr405["capture_contract"], depth_lab::PR405_LOCAL_CONTRACT);
        assert_eq!(pr405["duration_ms"], depth_lab::PR405_LOCAL_DURATION_MS);
        assert_eq!(pr405["target_depth_hz"], PR405_TARGET_DEPTH_HZ);

        let manual_root = tempfile::tempdir().unwrap();
        write_arm_request(manual_root.path(), launch_id, issue_contract(), true).unwrap();
        let manual: Value =
            serde_json::from_slice(&fs::read(manual_root.path().join(ARM_FILE)).unwrap()).unwrap();
        assert_eq!(manual["manual_stop"], true);
    }

    #[test]
    fn issue735_selection_publishes_only_exact_rgb_presents_and_keeps_source() {
        let root = tempfile::tempdir().unwrap();
        let raw = root.path().join("raw-depth");
        fs::create_dir(&raw).unwrap();
        let mut index = File::create(root.path().join("raw-depth-index.jsonl")).unwrap();
        let frames_path = root.path().join("frames.jsonl.zst");
        let mut frames =
            zstd::stream::write::Encoder::new(File::create(&frames_path).unwrap(), 1).unwrap();
        let mut samples = File::create(root.path().join(WATERMARK_SAMPLES_FILE)).unwrap();
        let qpc_frequency = 10_000_000_i64;
        let first_qpc = 1_000_000_000_i64;
        let mut expected = 0_usize;
        for ordinal in 0..1_024_u64 {
            let present_seq = 20_000 + ordinal;
            let present_qpc = first_qpc + ordinal as i64 * (qpc_frequency / 16);
            let name = format!("{ordinal:012}.d32");
            File::create(raw.join(&name)).unwrap();
            serde_json::to_writer(
                &mut index,
                &RawDepthSelectionRow {
                    schema: "dm-cp2077-raw-depth-frame-index/v1".into(),
                    ordinal,
                    file: name,
                    present_seq,
                    present_qpc,
                    current_present_seq: present_seq + 1,
                    readback_lag_presents: 1,
                    capture_id: ordinal + 1,
                    source_candidate_id: "dsv-r15-g1-e1".into(),
                    runtime_candidate_id: "dsv-r15-g1-e1".into(),
                    arm_request_observed_qpc: first_qpc - 1,
                    arm_start_present_seq: 19_999,
                    width: 1_920,
                    height: 1_080,
                    row_pitch: 7_680,
                    pixel_format: "DXGI_FORMAT_D32_FLOAT_LE".into(),
                    byte_len: 8_294_400,
                    rgb_pairing: "same_present_seq_gpu_watermark_submitted".into(),
                },
            )
            .unwrap();
            index.write_all(b"\n").unwrap();
            // Model WGC superseding roughly 7% of game Presents while keeping
            // both endpoints so the selected set still proves the full span.
            if ordinal != 0 && ordinal != 1_023 && ordinal.is_multiple_of(14) {
                continue;
            }
            let session_frame = ordinal;
            serde_json::to_writer(
                &mut frames,
                &json!({
                    "session_frame": session_frame,
                    "source_qpc": present_qpc,
                    "duplicate": false,
                    "present_seq": null,
                    "keys": [],
                    "events": []
                }),
            )
            .unwrap();
            frames.write_all(b"\n").unwrap();
            serde_json::to_writer(
                &mut samples,
                &json!({
                    "session_frame": session_frame,
                    "block_means": watermark_means(present_seq as u16)
                }),
            )
            .unwrap();
            samples.write_all(b"\n").unwrap();
            expected += 1;
        }
        drop(index);
        frames.finish().unwrap();
        drop(samples);
        let selected = materialize_issue735_exact_depth(
            root.path(),
            &frames_path,
            qpc_frequency,
            issue_contract(),
        )
        .unwrap();
        assert_eq!(selected as usize, expected);
        assert!((900..=1_024).contains(&expected));
        assert!(root.path().join("raw-depth-all-presents").is_dir());
        assert!(
            root.path()
                .join("raw-depth-index-all-presents.jsonl")
                .is_file()
        );
        let published: Vec<RawDepthSelectionRow> =
            read_bounded_jsonl(&root.path().join("raw-depth-index.jsonl"), false).unwrap();
        assert_eq!(published.len(), expected);
        assert_eq!(published[0].ordinal, 0);
        assert_eq!(published.last().unwrap().ordinal, expected as u64 - 1);
        assert_eq!(published[0].present_seq, 20_000);
        assert_eq!(published.last().unwrap().present_seq, 21_023);
    }

    #[test]
    fn duration_contract_is_exactly_63_through_65_seconds() {
        ensure_duration("test", 63_000, issue_contract()).unwrap();
        ensure_duration("test", 65_000, issue_contract()).unwrap();
        assert!(ensure_duration("test", 62_999, issue_contract()).is_err());
        assert!(ensure_duration("test", 65_001, issue_contract()).is_err());
        ensure_duration("test", 29_000, pr405_contract()).unwrap();
        ensure_duration("test", 31_000, pr405_contract()).unwrap();
    }

    #[test]
    fn telemetry_can_never_upgrade_the_raw_attempt() {
        let launch_id = "0123456789abcdef0123456789abcdef";
        let mut telemetry = json!({
            "schema": TELEMETRY_SCHEMA,
            "process_launch_id": launch_id,
            "capture_contract": depth_lab::ISSUE735_LOCAL_CONTRACT,
            "duration_ms": depth_lab::ISSUE735_LOCAL_DURATION_MS,
            "target_depth_hz": ISSUE735_TARGET_DEPTH_HZ,
            "submitted_frames": 960,
            "written_frames": 960,
            "writer_queue_dropped": 0,
            "present_stamp_dropped": 0,
            "capture_pool_dropped": 0,
            "fatal_error": null,
            "non_trainable": true,
            "production_certified": false,
            "upload_allowed": false,
            "deliverable": false,
            "recording_ready": false,
            "formal_start_run_used": false
        });
        validate_depth_telemetry(
            &serde_json::to_vec(&telemetry).unwrap(),
            launch_id,
            issue_contract(),
        )
        .unwrap();
        assert!(
            validate_depth_telemetry(
                &serde_json::to_vec(&telemetry).unwrap(),
                launch_id,
                pr405_contract()
            )
            .is_err()
        );
        telemetry["recording_ready"] = json!(true);
        assert!(
            validate_depth_telemetry(
                &serde_json::to_vec(&telemetry).unwrap(),
                launch_id,
                issue_contract()
            )
            .is_err()
        );
    }

    #[test]
    fn launch_identity_is_narrow() {
        validate_launch_id("0123456789abcdef0123456789abcdef").unwrap();
        assert!(validate_launch_id("0123456789ABCDEF0123456789ABCDEF").is_err());
        assert!(validate_launch_id("0123").is_err());
    }

    #[test]
    fn rgb_depth_pairing_requires_exact_present_and_nearby_qpc() {
        let frequency = 10_000_000;
        let depth = vec![
            RawDepthPairingRow {
                present_seq: 100,
                present_qpc: 20_000_000,
            },
            RawDepthPairingRow {
                present_seq: 101,
                present_qpc: 20_666_666,
            },
        ];
        let decoded = vec![
            DecodedRgbFrame {
                present_low16: 100,
                source_qpc: 20_000_100,
            },
            DecodedRgbFrame {
                present_low16: 101,
                source_qpc: 20_666_766,
            },
        ];
        let report = pair_rgb_depth(2, &decoded, &depth, frequency);
        assert!(report.passed);
        assert_eq!(report.exact_present_matches, 2);
        assert_eq!(report.median_compositor_lag_ticks, 100);
        assert_eq!(report.rgb_before_depth_pairs, 0);

        let wrong_present = [DecodedRgbFrame {
            present_low16: 102,
            source_qpc: 20_666_766,
        }];
        let report = pair_rgb_depth(2, &wrong_present, &depth, frequency);
        assert!(!report.passed);
        assert_eq!(report.exact_present_matches, 0);

        let stale_qpc = [DecodedRgbFrame {
            present_low16: 100,
            source_qpc: 21_000_000,
        }];
        let report = pair_rgb_depth(1, &stale_qpc, &depth[..1], frequency);
        assert!(!report.passed);
        assert_eq!(report.exact_present_matches, 0);
    }

    /// Regression for the 2026-09-10 joint attempt: the game presented at
    /// 13.38 Hz and the WGC compositor stamps trailed the game's Present stamps
    /// by 25..119 ms. A symmetric 62.5 ms window derived from
    /// `ISSUE735_TARGET_DEPTH_HZ` dropped every pair past its right edge -
    /// 323 of 843 - even though each one had already been identified exactly by
    /// its watermark.
    #[test]
    fn rgb_depth_pairing_accepts_measured_compositor_lag_below_target_rate() {
        let frequency = 10_000_000;
        // 13.38 Hz: one Present every 74.73 ms.
        let interval = 747_300i64;
        let base_qpc = 50_000_000i64;
        // Observed lag percentiles, in ticks: p0, p25, p50, p75, p95, p100.
        let lags = [249_802i64, 433_345, 572_720, 678_446, 824_455, 1_185_343];
        let depth: Vec<RawDepthPairingRow> = (0..lags.len())
            .map(|index| RawDepthPairingRow {
                present_seq: 400 + index as u64,
                present_qpc: base_qpc + interval * index as i64,
            })
            .collect();
        let decoded: Vec<DecodedRgbFrame> = depth
            .iter()
            .zip(lags)
            .map(|(row, lag)| DecodedRgbFrame {
                present_low16: row.present_seq,
                source_qpc: row.present_qpc + lag,
            })
            .collect();

        let historical_window = frequency / i64::try_from(ISSUE735_TARGET_DEPTH_HZ).unwrap();
        assert!(
            lags.iter().any(|lag| *lag > historical_window),
            "fixture must contain the pairs the old symmetric window rejected"
        );

        let report = pair_rgb_depth(depth.len(), &decoded, &depth, frequency);
        assert_eq!(report.exact_present_matches, depth.len());
        assert_eq!(report.exact_depth_rgb_coverage, 1.0);
        assert_eq!(report.measured_present_interval_ticks, interval);
        assert_eq!(report.min_compositor_lag_ticks, 249_802);
        assert_eq!(report.max_compositor_lag_ticks, 1_185_343);
        assert_eq!(report.rgb_before_depth_pairs, 0);
        assert!(report.passed);
    }

    /// The corrected window is asymmetric on purpose. The compositor cannot
    /// render a frame the game has not presented, so the causally impossible
    /// direction keeps the historical bound while the lag side follows the
    /// run's measured cadence. Neither side may swallow an arbitrary neighbour.
    #[test]
    fn rgb_depth_pairing_window_is_causal_and_still_bounded() {
        let frequency = 10_000_000;
        let interval = 747_300i64;
        let present_qpcs: Vec<i64> = (0..8).map(|index| 1_000_000 + interval * index).collect();
        let window = rgb_depth_pairing_window(frequency, &present_qpcs);

        assert_eq!(window.lead_ticks, frequency / 16);
        assert_eq!(
            window.lag_ticks,
            interval * RGB_COMPOSITE_LAG_PRESENT_INTERVALS
        );
        assert!(window.accepts(0));
        assert!(window.accepts(window.lag_ticks));
        assert!(!window.accepts(window.lag_ticks + 1));
        assert!(window.accepts(-window.lead_ticks));
        assert!(!window.accepts(-window.lead_ticks - 1));

        // A run that presents faster than the target rate keeps the historical
        // bound, so the corrected window is never narrower than the old one.
        let fast: Vec<i64> = (0..8).map(|index| 1_000_000 + 166_666 * index).collect();
        let fast_window = rgb_depth_pairing_window(frequency, &fast);
        assert_eq!(fast_window.lag_ticks, frequency / 16);
        assert_eq!(fast_window.lead_ticks, frequency / 16);

        // Too short to measure a cadence: fall back to the historical bound.
        let single = rgb_depth_pairing_window(frequency, &[1_000_000]);
        assert_eq!(single.lag_ticks, frequency / 16);
        assert_eq!(median_present_interval_ticks(&[1_000_000]), None);
    }

    #[test]
    fn rgb_depth_pairing_rejects_low16_wrap_and_empty_inputs() {
        let frequency = 10_000_000;
        let wrapped = vec![
            RawDepthPairingRow {
                present_seq: 1,
                present_qpc: 1,
            },
            RawDepthPairingRow {
                present_seq: 65_536,
                present_qpc: 2,
            },
        ];
        let decoded = vec![
            DecodedRgbFrame {
                present_low16: 1,
                source_qpc: 1,
            },
            DecodedRgbFrame {
                present_low16: 0,
                source_qpc: 2,
            },
        ];
        let report = pair_rgb_depth(2, &decoded, &wrapped, frequency);
        assert!(!report.passed);
        assert!(!report.low16_unambiguous_within_session);

        let report = pair_rgb_depth(0, &[], &[], 0);
        assert!(!report.passed);
        assert_eq!(report.exact_depth_rgb_coverage, 0.0);
        assert!(
            report
                .blockers
                .iter()
                .any(|blocker| blocker == "pairing_inputs_invalid_or_empty")
        );
    }

    fn player_pose_report(duplicate_candidate: bool, terminal_x: f64) -> Value {
        let qpc_start = 10_000i64;
        let mut snapshots = Vec::new();
        for index in 0..=128u64 {
            let offset_ms = index * 500;
            let x = if offset_ms <= 5_000 {
                0.0
            } else if offset_ms <= 23_000 {
                3.0 * (offset_ms - 5_000) as f64 / 18_000.0
            } else if offset_ms <= 29_000 {
                3.0
            } else if offset_ms <= 47_000 {
                3.0 * (47_000 - offset_ms) as f64 / 18_000.0
            } else {
                terminal_x * (offset_ms - 47_000) as f64 / 17_000.0
            };
            let candidate = json!({
                "transform_offset": 224,
                "raw_position_bits": [0, 0, 0],
                "position_world_units": [x, 0.0, 0.0],
                "raw_quaternion_ijkr": [0.0, 0.0, 0.0, 1.0],
                "quaternion_norm_error": 0.0
            });
            let mut candidates = vec![candidate.clone()];
            if duplicate_candidate && index == 10 {
                candidates.push(candidate);
            }
            snapshots.push(json!({
                "role": "player",
                "resolver_id": "player-system-rtti-world-transform",
                "object_tag": 7,
                "present_seq": 1_000 + index,
                "qpc": qpc_start + offset_ms as i64,
                "scenario": "on_foot_translation",
                "source_snapshot_bytes": 32,
                "candidates": candidates,
                "candidates_truncated": false
            }));
        }
        json!({
            "schema": POSE_REPORT_SCHEMA,
            "exact_build": {"executable_sha256": CP2077_EXE_SHA256},
            "process_launch_id": "0123456789abcdef0123456789abcdef",
            "snapshot_observations": snapshots,
            "trajectories": [{
                "role": "player",
                "resolver_id": "player-system-rtti-world-transform",
                "object_tag": 7,
                "transform_offset": 224,
                "position_changed_from_first": true
            }],
            "absolute_process_addresses_recorded": false,
            "production_binding_emitted": false,
            "recording_ready": false,
            "blockers": [{
                "code": "production_binding_not_authored",
                "detail": "runtime observations are discovery evidence only"
            }]
        })
    }

    fn action_cues(
        anchor_qpc: i64,
        frequency: i64,
        contract: CaptureContract,
    ) -> Vec<ActualActionCue> {
        contract
            .steps
            .iter()
            .enumerate()
            .map(|(step_index, step)| {
                let scheduled_qpc =
                    scheduled_action_qpc(anchor_qpc, frequency, step.offset_ms).unwrap();
                ActualActionCue {
                    step_index,
                    scheduled_qpc,
                    actual_qpc: scheduled_qpc,
                }
            })
            .collect()
    }

    #[test]
    fn player_pose_gate_keeps_pr405_loop_strict_and_issue735_observational() {
        let launch = "0123456789abcdef0123456789abcdef";
        let cues = action_cues(10_000, 1_000, issue_contract());
        let accepted = audit_player_pose(
            &player_pose_report(false, 0.1),
            launch,
            10_000,
            74_000,
            1_000,
            &cues,
            issue_contract(),
            false,
        )
        .unwrap();
        assert_eq!(accepted.sample_count, 129);
        assert!(accepted.maximum_displacement_world_units >= 3.0);
        assert!(accepted.terminal_closure_world_units <= 0.1 + f64::EPSILON);

        assert!(
            audit_player_pose(
                &player_pose_report(true, 0.1),
                launch,
                10_000,
                74_000,
                1_000,
                &cues,
                issue_contract(),
                false
            )
            .is_err()
        );
        let observational = audit_player_pose(
            &player_pose_report(false, 1.0),
            launch,
            10_000,
            74_000,
            1_000,
            &cues,
            issue_contract(),
            false,
        )
        .unwrap();
        assert!(observational.terminal_closure_world_units > MAX_TERMINAL_CLOSURE_WORLD_UNITS);
        let pr405_cues = action_cues(10_000, 1_000, pr405_contract());
        assert!(
            audit_player_pose(
                &player_pose_report(false, 1.0),
                launch,
                10_000,
                40_000,
                1_000,
                &pr405_cues,
                pr405_contract(),
                false
            )
            .is_err()
        );
        assert!(
            audit_player_pose(
                &player_pose_report(false, 0.1),
                "ffffffffffffffffffffffffffffffff",
                10_000,
                74_000,
                1_000,
                &cues,
                issue_contract(),
                false
            )
            .is_err()
        );
    }

    fn matrix_source_map(
        duplicate: bool,
    ) -> std::collections::BTreeMap<String, std::collections::BTreeMap<u64, [[f64; 4]; 4]>> {
        let samples = (1..=120)
            .map(|present| {
                (
                    present,
                    [
                        [1.0, 0.0, 0.0, 0.0],
                        [0.0, 1.0, 0.0, 0.0],
                        [0.0, 0.0, 1.0, 0.0],
                        [present as f64 / 120.0, 0.0, 0.0, 1.0],
                    ],
                )
            })
            .collect::<std::collections::BTreeMap<_, _>>();
        let mut sources = std::collections::BTreeMap::from([("source-a".into(), samples.clone())]);
        if duplicate {
            sources.insert("source-b".into(), samples);
        }
        sources
    }

    #[test]
    fn render_camera_source_must_be_unique_and_cover_the_selected_presents() {
        let one = matrix_source_map(false);
        let (source, samples) = unique_stable_matrix_source(&one, 120).unwrap();
        assert_eq!(source, "source-a");
        assert_eq!(samples.len(), 120);
        assert!(unique_stable_matrix_source(&matrix_source_map(true), 120).is_err());
        assert!(unique_stable_matrix_source(&one, 200).is_err());
        assert!(audited_render_physical_source(
            "direct_game_mapped_root_cbv_at_selected_dsv_draw"
        ));
        assert!(audited_render_physical_source(
            "game_mapped_descriptor_table_base_cbv_at_selected_dsv_draw"
        ));
        assert!(audited_render_physical_source(
            "copy_buffer_region_root_signature_declared_cbv_at_selected_dsv_draw"
        ));
        assert!(audited_render_matrix_physical_source(
            "copy_buffer_region_root_signature_declared_cbv_at_selected_dsv_draw",
            Some("copy_buffer_region_root_signature_declared_cbv")
        ));
        assert!(!audited_render_matrix_physical_source(
            "copy_buffer_region_root_signature_declared_cbv_at_selected_dsv_draw",
            Some("game_mapped_descriptor_table_base_cbv")
        ));
        assert!(!audited_render_matrix_physical_source(
            "copy_buffer_region_root_signature_declared_cbv_at_selected_dsv_draw",
            None
        ));
        assert!(!audited_render_physical_source("cpu_guessed_constant"));
        let one_physical = std::collections::BTreeSet::from([
            "game_mapped_descriptor_table_base_cbv_at_selected_dsv_draw".to_owned(),
        ]);
        assert_eq!(
            unique_render_physical_source(&one_physical).unwrap(),
            "game_mapped_descriptor_table_base_cbv_at_selected_dsv_draw"
        );
        let changed_physical = std::collections::BTreeSet::from([
            "direct_game_mapped_root_cbv_at_selected_dsv_draw".to_owned(),
            "game_mapped_descriptor_table_base_cbv_at_selected_dsv_draw".to_owned(),
        ]);
        assert!(unique_render_physical_source(&changed_physical).is_err());
        assert!(unique_render_physical_source(&std::collections::BTreeSet::new()).is_err());
    }

    #[test]
    fn pr405_pose_math_rejects_missing_distinct_present_and_low_render_rate() {
        let pose = |present_seq, qpc| Pr405RenderPose {
            identity: Pr405PresentIdentity { present_seq, qpc },
            view: [
                [1.0, 0.0, 0.0, 0.0],
                [0.0, 1.0, 0.0, 0.0],
                [0.0, 0.0, 1.0, 0.0],
                [0.0, 0.0, 0.0, 1.0],
            ],
            translation: depth_lab::camera::TranslationSlot::Row3,
            projection: [
                [1.0, 0.0, 0.0, 0.0],
                [0.0, 1.0, 0.0, 0.0],
                [0.0, 0.0, 0.0, 1.0],
                [0.0, 0.0, 0.1, 0.0],
            ],
            proj_a: 0.0,
            proj_b: 0.1,
        };
        let duplicate_only = [pose(10, 1_000), pose(10, 1_000)];
        assert!(pr405_neighbour(&duplicate_only, 0).is_err());

        let sixty_hz = [pose(10, 1_000), pose(11, 1_100)];
        assert!((pr405_dynamic_fps(&sixty_hz, 0, 6_000).unwrap() - 60.0).abs() < f64::EPSILON);
        let twenty_hz = [pose(10, 1_000), pose(11, 1_300)];
        assert!(pr405_dynamic_fps(&twenty_hz, 0, 6_000).is_err());
    }

    fn autonomous_row(
        marker_id: &str,
        scheduled_qpc: i64,
        vk: u16,
        action: AutonomousKeyAction,
    ) -> AutonomousInputLedgerRow {
        AutonomousInputLedgerRow {
            schema: AUTONOMOUS_INPUT_LEDGER_SCHEMA.into(),
            process_launch_id: "0123456789abcdef0123456789abcdef".into(),
            input_provenance: AUTONOMOUS_INPUT_PROVENANCE.into(),
            marker_id: marker_id.into(),
            scheduled_qpc,
            attempt_qpc: scheduled_qpc,
            actual_qpc: scheduled_qpc,
            qpc_frequency: 1_000,
            target_process_id: 77,
            bound_hwnd_process_matches: true,
            foreground_root_matches: true,
            foreground_process_matches: true,
            vk,
            action: action.as_str().into(),
            send_input_count: 1,
            non_trainable: true,
            recording_ready: false,
        }
    }

    #[test]
    fn autonomous_input_supports_both_exact_local_contracts_and_maps_transitions() {
        assert!(validate_autonomous_contract(true, issue_contract()).is_ok());
        assert!(validate_autonomous_contract(true, pr405_contract()).is_ok());
        assert_eq!(
            autonomous_transition("outbound-start"),
            Some((VK_W.0, AutonomousKeyAction::Down))
        );
        assert_eq!(
            autonomous_transition("outbound-stop"),
            Some((VK_W.0, AutonomousKeyAction::Up))
        );
        assert_eq!(
            autonomous_transition("return-start"),
            Some((VK_S.0, AutonomousKeyAction::Down))
        );
        assert_eq!(
            autonomous_transition("return-stop"),
            Some((VK_S.0, AutonomousKeyAction::Up))
        );
        assert_eq!(
            autonomous_transition("correction-start"),
            Some((VK_W.0, AutonomousKeyAction::Down))
        );
        assert_eq!(
            autonomous_transition("correction-stop"),
            Some((VK_W.0, AutonomousKeyAction::Up))
        );
    }

    #[test]
    fn autonomous_ledger_is_distinct_from_raw_input_and_fails_on_focus_drift() {
        let root = tempfile::tempdir().unwrap();
        let events = root.path().join("events.jsonl.zst");
        let encoder = zstd::stream::write::Encoder::new(File::create(&events).unwrap(), 1).unwrap();
        encoder.finish().unwrap();
        let events_hash = sha256_file(&events).unwrap();
        let meta = root.path().join("meta.json");
        fs::write(
            &meta,
            serde_json::to_vec(&json!({
                "state": "non_trainable_ready",
                "non_trainable": true,
                "input_status": "raw_input_v1",
                "qpc_start": 1_000,
                "qpc_end": 31_000,
                "n_frames": 900,
                "events": {"path": "events.jsonl.zst", "lines": 0, "sha256": events_hash},
                "health": {"input_overflow": 0, "input_boundary_reorder_drops": 0}
            }))
            .unwrap(),
        )
        .unwrap();
        let manifest = root.path().join("sha256.json");
        fs::write(
            &manifest,
            serde_json::to_vec(&json!({"events.jsonl.zst": events_hash})).unwrap(),
        )
        .unwrap();
        let ledger = root.path().join(AUTONOMOUS_INPUT_LEDGER_FILE);
        let rows = vec![
            autonomous_row("outbound-start", 3_000, VK_W.0, AutonomousKeyAction::Down),
            autonomous_row("outbound-stop", 12_000, VK_W.0, AutonomousKeyAction::Up),
            autonomous_row("return-start", 15_000, VK_S.0, AutonomousKeyAction::Down),
            autonomous_row("return-stop", 24_000, VK_S.0, AutonomousKeyAction::Up),
        ];
        write_jsonl_create_new(&ledger, &rows).unwrap();
        let metrics = audit_autonomous_input(
            &ledger,
            &events,
            &meta,
            &manifest,
            "0123456789abcdef0123456789abcdef",
            77,
            1_000,
            31_000,
            1_000,
            900,
            0,
            0,
            pr405_contract(),
        )
        .unwrap();
        assert_eq!(metrics.evidence_event_count, 4);
        assert_eq!(metrics.raw_event_count, 0);
        assert_eq!(metrics.outbound_hold_ms, 9_000);
        assert_eq!(metrics.return_hold_ms, 9_000);
        assert_eq!(autonomous_keys_at(5_000, &rows), vec![VK_W.0]);
        assert_eq!(autonomous_keys_at(13_000, &rows), Vec::<u16>::new());
        assert_eq!(autonomous_keys_at(16_000, &rows), vec![VK_S.0]);

        let mut drifted = rows;
        drifted[2].foreground_root_matches = false;
        let mut file = File::create(&ledger).unwrap();
        for row in &drifted {
            serde_json::to_writer(&mut file, row).unwrap();
            file.write_all(b"\n").unwrap();
        }
        assert!(
            audit_autonomous_input(
                &ledger,
                &events,
                &meta,
                &manifest,
                "0123456789abcdef0123456789abcdef",
                77,
                1_000,
                31_000,
                1_000,
                900,
                0,
                0,
                pr405_contract(),
            )
            .is_err()
        );
    }

    #[test]
    fn pr405_atomic_evidence_writer_refuses_partial_or_existing_outputs() {
        let root = tempfile::tempdir().unwrap();
        let frames = root.path().join("frames.jsonl");
        let poses = root.path().join("poses.jsonl");
        let frame_rows = [Pr405FrameEvidence {
            schema: "dm-cp2077-pr405-frame-evidence/v1",
            session_frame: 0,
            duplicate: false,
            rgb: Pr405PresentIdentity {
                present_seq: 10,
                qpc: 100,
            },
            camera: Pr405PresentIdentity {
                present_seq: 10,
                qpc: 100,
            },
            player: Pr405PresentIdentity {
                present_seq: 10,
                qpc: 100,
            },
            depth: None,
            projection: None,
        }];
        let pose_rows = [serde_json::json!({"present_seq": 10})];
        write_two_jsonl_atomically(&frames, &frame_rows, &poses, &pose_rows).unwrap();
        assert!(frames.is_file() && poses.is_file());
        assert!(write_two_jsonl_atomically(&frames, &frame_rows, &poses, &pose_rows).is_err());
    }

    #[test]
    fn pr405_authoring_missing_real_render_observations_leaves_no_outputs() {
        let root = tempfile::tempdir().unwrap();
        fs::write(
            root.path().join("cp2077-pr405-package-input.json"),
            serde_json::to_vec(&json!({
                "qpc_frequency_hz": 6000,
                "metric_calibration": {
                    "value": 1.0,
                    "source": {"path": "known_size_anchors", "anchors": 4, "residual_rms": 0.0},
                    "cross_check": null,
                    "evidence_path": "metric-scale-evidence.json"
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let frames_path = root.path().join("frames.jsonl.zst");
        let file = File::create(&frames_path).unwrap();
        let mut encoder = zstd::stream::write::Encoder::new(file, 1).unwrap();
        for index in 0..900u64 {
            serde_json::to_writer(
                &mut encoder,
                &json!({
                    "session_frame": index,
                    "source_qpc": 10_000 + index as i64 * 200,
                    "duplicate": false,
                    "present_seq": 1_000 + index,
                    "keys": [],
                    "events": []
                }),
            )
            .unwrap();
            encoder.write_all(b"\n").unwrap();
        }
        encoder.finish().unwrap();
        let mut raw = File::create(root.path().join("raw-depth-index.jsonl")).unwrap();
        for ordinal in 0..450u64 {
            let index = ordinal * 2;
            serde_json::to_writer(
                &mut raw,
                &json!({
                    "ordinal": ordinal,
                    "file": format!("{ordinal:012}.d32"),
                    "present_seq": 1_000 + index,
                    "present_qpc": 10_000 + index as i64 * 200
                }),
            )
            .unwrap();
            raw.write_all(b"\n").unwrap();
        }
        assert!(
            author_pr405_frame_and_pose_evidence(
                root.path(),
                &frames_path,
                "0123456789abcdef0123456789abcdef",
                6_000,
                false
            )
            .is_err()
        );
        assert!(
            !root
                .path()
                .join("cp2077-pr405-frame-evidence.jsonl")
                .exists()
        );
        assert!(!root.path().join("cp2077-final-pose.jsonl").exists());
    }

    #[test]
    fn action_marker_seals_actual_cue_qpc_and_rejects_late_delivery() {
        let root = tempfile::tempdir().unwrap();
        let mut cues = action_cues(10_000, 1_000, issue_contract());
        for cue in &mut cues {
            cue.actual_qpc += 25;
        }
        write_action_markers(
            root.path(),
            "0123456789abcdef0123456789abcdef",
            10_000,
            1_000,
            &cues,
            issue_contract(),
            false,
        )
        .unwrap();
        let markers: Vec<Value> =
            read_bounded_jsonl(&root.path().join(ACTION_MARKERS_FILE), false).unwrap();
        assert_eq!(markers.len(), ISSUE735_OPERATOR_ACTION_STEPS.len());
        assert_eq!(markers[0]["scheduled_qpc"], json!(10_000));
        assert_eq!(markers[0]["actual_cue_qpc"], json!(10_025));
        assert_eq!(markers[0]["cue_delay_ms"], json!(25));
        assert_eq!(
            actual_cue_qpc(&cues, issue_contract().steps, "outbound-start").unwrap(),
            15_025
        );

        let late_root = tempfile::tempdir().unwrap();
        cues[3].actual_qpc = cues[3].scheduled_qpc + 251;
        assert!(
            write_action_markers(
                late_root.path(),
                "0123456789abcdef0123456789abcdef",
                10_000,
                1_000,
                &cues,
                issue_contract(),
                false,
            )
            .is_err()
        );
    }

    fn key_event(seq: u64, qpc: i64, event_type: &str, vk: u16) -> RawInputEvent {
        RawInputEvent {
            seq,
            run_id: "r0000".into(),
            qpc,
            event_type: event_type.into(),
            device: "kbd:test".into(),
            vk: Some(vk),
            scan_code: Some(17),
            flags: Some(0),
            repeat: None,
            dx: None,
            dy: None,
            absolute: None,
            delta: None,
            horizontal: None,
            control: false,
        }
    }

    fn mouse_event(seq: u64, qpc: i64) -> RawInputEvent {
        RawInputEvent {
            seq,
            run_id: "r0000".into(),
            qpc,
            event_type: "mouse_move".into(),
            device: "mouse:test".into(),
            vk: None,
            scan_code: None,
            flags: None,
            repeat: None,
            dx: Some(3),
            dy: Some(-2),
            absolute: Some(false),
            delta: None,
            horizontal: None,
            control: false,
        }
    }

    fn raw_input_fixture(root: &Path, events: &[RawInputEvent]) -> (PathBuf, PathBuf, PathBuf) {
        let events_path = root.join("events.jsonl.zst");
        let mut encoder =
            zstd::stream::write::Encoder::new(File::create(&events_path).unwrap(), 1).unwrap();
        for event in events {
            serde_json::to_writer(&mut encoder, event).unwrap();
            encoder.write_all(b"\n").unwrap();
        }
        encoder.finish().unwrap();
        let events_hash = sha256_file(&events_path).unwrap();
        let meta_path = root.join("meta.json");
        fs::write(
            &meta_path,
            serde_json::to_vec(&json!({
                "state": "non_trainable_ready",
                "non_trainable": true,
                "input_status": "raw_input_v1",
                "qpc_start": 1_000,
                "qpc_end": 2_000,
                "n_frames": 30,
                "events": {
                    "path": "events.jsonl.zst",
                    "lines": events.len(),
                    "sha256": events_hash
                },
                "health": {
                    "input_raw_reports": events.len(),
                    "input_overflow": 0,
                    "input_boundary_reorder_drops": 0
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let manifest_path = root.join("sha256.json");
        fs::write(
            &manifest_path,
            serde_json::to_vec(&json!({"events.jsonl.zst": events_hash})).unwrap(),
        )
        .unwrap();
        (events_path, meta_path, manifest_path)
    }

    #[test]
    fn formal_raw_input_requires_both_keyboard_and_mouse_streams() {
        let root = tempfile::tempdir().unwrap();
        let complete = vec![
            key_event(1, 1_100, "key_down", 87),
            mouse_event(2, 1_200),
            key_event(3, 1_300, "key_up", 87),
        ];
        let (events, meta, manifest) = raw_input_fixture(root.path(), &complete);
        let metrics = audit_raw_input(
            &events,
            &meta,
            &manifest,
            1_000,
            2_000,
            1_000,
            30,
            3,
            0,
            &[],
            issue_contract(),
            true,
            true,
        )
        .unwrap();
        assert_eq!(metrics.keyboard_events, 2);
        assert_eq!(metrics.mouse_move_events, 1);

        let no_mouse_root = tempfile::tempdir().unwrap();
        let (events, meta, manifest) = raw_input_fixture(no_mouse_root.path(), &complete[..1]);
        assert!(
            audit_raw_input(
                &events,
                &meta,
                &manifest,
                1_000,
                2_000,
                1_000,
                30,
                1,
                0,
                &[],
                issue_contract(),
                true,
                true,
            )
            .is_err()
        );

        let calibration_root = tempfile::tempdir().unwrap();
        let keyboard_only = vec![
            key_event(1, 1_100, "key_down", 87),
            key_event(2, 1_300, "key_up", 87),
        ];
        let (events, meta, manifest) = raw_input_fixture(calibration_root.path(), &keyboard_only);
        let metrics = audit_raw_input(
            &events,
            &meta,
            &manifest,
            1_000,
            2_000,
            1_000,
            30,
            2,
            0,
            &[],
            issue_contract(),
            true,
            false,
        )
        .unwrap();
        assert_eq!(metrics.keyboard_events, 2);
        assert_eq!(metrics.mouse_move_events, 0);
    }

    #[test]
    fn raw_input_hold_must_align_to_the_qpc_action_markers() {
        let events = vec![
            key_event(1, 15_100, "key_down", 87),
            key_event(2, 32_900, "key_up", 87),
        ];
        assert_eq!(
            action_hold_ms(
                &events,
                87,
                15_000,
                33_000,
                1_000,
                issue_contract(),
                ACTION_CUE_TOLERANCE_MS
            )
            .unwrap(),
            17_800
        );
        let late = vec![
            key_event(1, 20_000, "key_down", 87),
            key_event(2, 38_000, "key_up", 87),
        ];
        assert!(
            action_hold_ms(
                &late,
                87,
                15_000,
                33_000,
                1_000,
                issue_contract(),
                ACTION_CUE_TOLERANCE_MS
            )
            .is_err()
        );
    }

    #[test]
    fn manual_fixed_view_timing_allows_counting_error_but_keeps_hold_limits() {
        // The observed S release was 3.733 seconds early; keep its real duration.
        let check = |down, up, tolerance| {
            action_hold_ms(
                &[
                    key_event(0, down, "key_down", 83),
                    key_event(1, up, "key_up", 83),
                ],
                83,
                29_000,
                47_000,
                1_000,
                issue_contract(),
                tolerance,
            )
        };
        assert!(check(27_414, 43_267, ACTION_CUE_TOLERANCE_MS).is_err());
        assert_eq!(
            check(27_414, 43_267, FIXED_VIEW_CUE_TOLERANCE_MS).unwrap(),
            15_853
        );
        for delta in [-5_000, 5_000] {
            assert_eq!(
                check(29_000 + delta, 47_000 + delta, FIXED_VIEW_CUE_TOLERANCE_MS).unwrap(),
                18_000
            );
        }
        for (down, up) in [
            (23_999, 42_000),
            (34_001, 52_000),
            (29_000, 41_999),
            (29_000, 52_001),
            (34_000, 42_000),
            (24_000, 52_000),
        ] {
            assert!(
                check(down, up, FIXED_VIEW_CUE_TOLERANCE_MS).is_err(),
                "{down}..{up}"
            );
        }
        assert_eq!(input_cue_tolerance_ms(false, false), 5_000);
        for (mouse, manual) in [(true, false), (true, true), (false, true)] {
            assert_eq!(input_cue_tolerance_ms(mouse, manual), 3_000);
        }
    }

    #[test]
    fn action_plans_cover_both_explicit_contracts() {
        for contract in [issue_contract(), pr405_contract()] {
            assert_eq!(contract.steps.first().unwrap().offset_ms, 0);
            assert_eq!(
                contract.steps.last().unwrap().offset_ms,
                contract.duration_ms
            );
            assert_eq!(
                contract
                    .steps
                    .iter()
                    .map(|step| step.marker_id)
                    .collect::<std::collections::BTreeSet<_>>()
                    .len(),
                contract.steps.len()
            );
            let text = operator_action_plan_text(contract.name, contract.duration_ms).unwrap();
            assert!(text.contains("Hold W"));
            assert!(text.contains("Hold S"));
        }
    }

    #[test]
    fn fixed_view_walk_matches_keyboard_only_guidance_without_weakening_other_phases() {
        assert!(!standalone_requires_mouse_movement(issue_contract(), false));
        assert!(standalone_requires_mouse_movement(issue_contract(), true));
        assert!(standalone_requires_mouse_movement(pr405_contract(), false));
        let contract = issue_contract();
        let cues = contract
            .steps
            .iter()
            .enumerate()
            .map(|(index, step)| {
                let qpc = scheduled_action_qpc(1_000, 10, step.offset_ms).unwrap();
                ActualActionCue {
                    step_index: index,
                    scheduled_qpc: qpc,
                    actual_qpc: qpc,
                }
            })
            .collect::<Vec<_>>();
        let events = vec![
            key_event(0, 1_050, "key_down", 87),
            key_event(1, 1_230, "key_up", 87),
            key_event(2, 1_274, "key_down", 83),
            key_event(3, 1_433, "key_up", 83),
        ];
        let root = tempfile::tempdir().unwrap();
        let (path, meta, manifest) = raw_input_fixture(root.path(), &events);
        let audit = audit_raw_input(
            &path,
            &meta,
            &manifest,
            1_000,
            2_000,
            10,
            30,
            4,
            0,
            &cues,
            contract,
            false,
            standalone_requires_mouse_movement(contract, false),
        )
        .unwrap();
        assert_eq!(audit.outbound_hold_ms, 18_000);
        assert_eq!(audit.return_hold_ms, 15_900);
        assert_eq!(audit.mouse_move_events, 0);
        assert!(
            audit_raw_input(
                &path, &meta, &manifest, 1_000, 2_000, 10, 30, 4, 0, &cues, contract, false, true
            )
            .unwrap_err()
            .to_string()
            .contains("no mouse movement")
        );
        let preview = fixed_view_action_plan_preview();
        assert_eq!(preview["schema"], "dm-cp2077-guided-preview/v2");
        assert_eq!(preview["speech_protocol"], "short_cue_sapi_completion/v1");
        assert_eq!(preview["input_policy"], "fixed_view_ws/v3");
        assert_eq!(preview["action_cue_tolerance_ms"], 5_000);
        assert_eq!(
            preview["steps"].as_array().unwrap().len(),
            contract.steps.len()
        );
        for (row, step) in preview["steps"]
            .as_array()
            .unwrap()
            .iter()
            .zip(contract.steps)
        {
            assert_eq!(row["offset_ms"], step.offset_ms);
            assert_eq!(
                row["instruction_zh"],
                chinese_action_instruction(step.marker_id)
            );
            assert_eq!(row["speech_zh"], chinese_action_cue(step.marker_id));
            assert!(row["speech_zh"].as_str().unwrap().chars().count() <= 6);
        }
    }

    #[test]
    fn movement_hold_cannot_bridge_a_release_and_restart() {
        // Old implementation skipped the first release to find a later key-up
        // near the stop cue, falsely counting the gap as continuous movement.
        let interrupted = vec![
            key_event(0, 15_100, "key_down", 87),
            key_event(1, 16_000, "key_up", 87),
            key_event(2, 20_000, "key_down", 87),
            key_event(3, 32_900, "key_up", 87),
        ];
        assert!(
            action_hold_ms(
                &interrupted,
                87,
                15_000,
                33_000,
                1_000,
                issue_contract(),
                FIXED_VIEW_CUE_TOLERANCE_MS
            )
            .unwrap_err()
            .to_string()
            .contains("continuously")
        );
        let mut repeat = key_event(1, 20_000, "key_down", 87);
        repeat.repeat = Some(true);
        let continuous = vec![
            key_event(0, 15_100, "key_down", 87),
            repeat,
            key_event(2, 32_900, "key_up", 87),
        ];
        assert_eq!(
            action_hold_ms(
                &continuous,
                87,
                15_000,
                33_000,
                1_000,
                issue_contract(),
                FIXED_VIEW_CUE_TOLERANCE_MS
            )
            .unwrap(),
            17_800
        );
    }

    #[test]
    fn fixed_view_controls_reject_sprint_mouse_conflicts_and_unreleased_keys() {
        let good = vec![
            key_event(0, 1_100, "key_down", 87),
            key_event(1, 1_300, "key_up", 87),
        ];
        assert!(audit_fixed_view_controls(&good).is_ok());
        for bad in [key_event(2, 1_400, "key_down", 16), mouse_event(2, 1_400)] {
            let mut events = good.clone();
            events.push(bad);
            assert!(audit_fixed_view_controls(&events).is_err());
        }
        assert!(audit_fixed_view_controls(&good[..1]).is_err());
        assert!(
            audit_fixed_view_controls(&[
                key_event(0, 1_100, "key_down", 87),
                key_event(1, 1_200, "key_down", 83)
            ])
            .is_err()
        );
        assert!(audit_fixed_view_controls(&[key_event(0, 1_100, "key_up", 87)]).is_err());
    }
}

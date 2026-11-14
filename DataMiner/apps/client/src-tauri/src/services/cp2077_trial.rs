//! Client-owned CP2077 local raw-depth launch and sidecar lifecycle.
//!
//! The early launcher and raw D32 probe remain local/non-trainable. RGB, Raw
//! Input, start/stop, session/run identity, and final sealing belong exclusively
//! to the formal main collector.

use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::os::windows::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use windows::Win32::Foundation::{CloseHandle, FILETIME, HANDLE};
use windows::Win32::System::Threading::{
    CREATE_NO_WINDOW, GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
};

use crate::cp2077_raw_demo::{
    MainCollectorDepthSummary, MainCollectorPurpose, arm_main_collector_sidecar,
    finalize_main_collector_sidecar, prepare_main_collector_sidecar, stop_main_collector_sidecar,
};
use crate::domain::collector::CollectorState;
use crate::platform::windows::shell;
use crate::services::depth_lab::{default_tools_dir, resolve_cp2077_catalog_executable};
use crate::services::early_launch::{
    EarlyLaunchTarget, EarlyLauncherMode, EarlyLauncherRequest, parse_early_launcher_output,
};
use crate::services::journal::{Cp2077MainRunCommit, SessionJournal};
use capture_core::recording::RecordingOutcome;

const EARLY_LAUNCH_TARGET: EarlyLaunchTarget = EarlyLaunchTarget::cp2077();
const GAME_EXE_SHA256: &str = EARLY_LAUNCH_TARGET.executable_sha256();
const CONTRACT: &str = depth_lab::ISSUE735_LOCAL_CONTRACT;
const DURATION_MS: u64 = depth_lab::ISSUE735_LOCAL_DURATION_MS;
const CONFIG_SCHEMA: &str = "dm-cp2077-raw-depth-demo-config/v1";
const PROFILE_FILE: &str = "raw-depth-demo-profile.json";
const CONFIG_FILE: &str = "raw-depth-demo-config.json";
const ARM_FILE: &str = "raw-depth-demo-arm-request.json";
const PACKAGER_FILE: &str = "package-local-fivepiece.py";
const METRIC_EVIDENCE_FILE: &str = "cp2077-metric-evidence.json";
const MAX_PROFILE_BYTES: u64 = 4 * 1024 * 1024;
const CP2077_APP_ID: &str = EARLY_LAUNCH_TARGET.steam_app_id();
const CP2077_BUILD_ID: &str = EARLY_LAUNCH_TARGET.steam_build_id();
const RIGHTS_MANIFEST_BYTES: &[u8] =
    include_bytes!("../../../../../config/game-depth-rights-gates.json");
const RUNTIME_EVIDENCE_BYTES: &[u8] =
    include_bytes!("../../../../../tools/pr735-cp2077/evidence/cp2077-runtime-d6-20260818.json");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TrialPhase {
    Idle,
    Preparing,
    Prepared,
    Recording,
    Processing,
    Completed,
    Faulted,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrialStatus {
    pub available: bool,
    pub phase: TrialPhase,
    pub purpose: Option<MainCollectorPurpose>,
    pub process_id: Option<u32>,
    pub session_locator: Option<String>,
    pub profile_candidate: Option<String>,
    pub elapsed_target_seconds: u64,
    pub video_frames: Option<u64>,
    pub depth_frames: Option<u64>,
    pub keyboard_input_events: Option<u64>,
    pub mouse_input_events: Option<u64>,
    pub metric_marker_count: usize,
    pub metric_calibration_samples: Option<usize>,
    pub metric_evidence_sealed: bool,
    pub metric_evidence_metres_per_world_unit: Option<f64>,
    pub metric_evidence_session_locator: Option<String>,
    pub delivery_locator: Option<String>,
    pub recording_elapsed_seconds: u64,
    pub can_stop: bool,
    pub last_error: Option<String>,
    pub non_trainable: bool,
    pub recording_ready: bool,
}

#[derive(Debug, Clone)]
struct PreparedTrial {
    process_id: u32,
    process_creation_time_100ns: u64,
    launch_id: String,
    session_dir: PathBuf,
    candidate_id: String,
    purpose: MainCollectorPurpose,
    delivery_dir: Option<PathBuf>,
}

#[derive(Debug, Clone)]
struct SealedMetricEvidence {
    session_dir: PathBuf,
    session_locator: String,
    metres_per_world_unit: f64,
}

pub struct MainCollectorSidecarGuard {
    prepared: PreparedTrial,
    session_id: String,
    run_id: String,
    generation: u64,
    armed: bool,
    stopped: bool,
}

#[derive(Debug, Clone)]
struct TrialRuntime {
    phase: TrialPhase,
    requested_purpose: Option<MainCollectorPurpose>,
    prepared: Option<PreparedTrial>,
    video_frames: Option<u64>,
    depth_frames: Option<u64>,
    keyboard_input_events: Option<u64>,
    mouse_input_events: Option<u64>,
    metric_marker_count: usize,
    metric_calibration_samples: Option<usize>,
    metric_evidence: Option<SealedMetricEvidence>,
    recording_started: Option<Instant>,
    last_error: Option<String>,
}

impl Default for TrialRuntime {
    fn default() -> Self {
        Self {
            phase: TrialPhase::Idle,
            requested_purpose: None,
            prepared: None,
            video_frames: None,
            depth_frames: None,
            keyboard_input_events: None,
            mouse_input_events: None,
            metric_marker_count: 0,
            metric_calibration_samples: None,
            metric_evidence: None,
            recording_started: None,
            last_error: None,
        }
    }
}

static RUNTIME: OnceLock<Mutex<TrialRuntime>> = OnceLock::new();

const RIGHTS_MANIFEST_SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Deserialize)]
struct RightsManifest {
    schema_version: u32,
    /// Issue #855 §E.5 retired the `project_research_only` lane. The field is kept only so
    /// the runtime can reject a manifest that still carries the contradictory policy instead
    /// of silently ignoring it.
    #[serde(default)]
    project_research_only: Option<serde_json::Value>,
    unified_rights_decision: UnifiedRightsDecision,
    records: Vec<RightsRecord>,
}

#[derive(Debug, Deserialize)]
struct UnifiedRightsDecision {
    declared_by: String,
    declared_at: String,
    reference_url: String,
    note: String,
}

#[derive(Debug, Deserialize)]
struct RightsRecord {
    app_id: String,
    game: String,
    rights_owner: String,
    decision: String,
    scopes: RightsScopes,
    mode_restrictions: String,
    effective_date: String,
    expiry_or_review_date: String,
    #[serde(default)]
    evidence_url: Option<String>,
    reviewer: String,
}

#[derive(Debug, Deserialize)]
struct RightsScopes {
    instrumentation: RightsScope,
    capture: RightsScope,
    commercial_ml: RightsScope,
    delivery: RightsScope,
}

#[derive(Debug, Deserialize)]
struct RightsScope {
    approved: bool,
    boundaries: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RuntimeEvidence {
    schema_version: String,
    status: String,
    non_trainable: bool,
    identity: RuntimeIdentity,
    launch: RuntimeLaunch,
    graphics: RuntimeGraphics,
    protection_and_network: RuntimeProtection,
}

#[derive(Debug, Deserialize)]
struct RuntimeIdentity {
    app_id: String,
    build_id: String,
    executable_sha256: String,
}

#[derive(Debug, Deserialize)]
struct RuntimeLaunch {
    method: String,
}

#[derive(Debug, Deserialize)]
struct RuntimeGraphics {
    direct3d12_confirmed: bool,
}

#[derive(Debug, Deserialize)]
struct RuntimeProtection {
    known_anticheat_modules_observed: Vec<String>,
    known_anticheat_processes_observed: Vec<String>,
    established_external_tcp_connections: u64,
    runtime_protection_gate_completed: bool,
    game_runtime_network_policy: String,
}

fn runtime() -> &'static Mutex<TrialRuntime> {
    RUNTIME.get_or_init(|| {
        let runtime = TrialRuntime {
            metric_evidence: latest_valid_metric_evidence().ok().flatten(),
            ..TrialRuntime::default()
        };
        Mutex::new(runtime)
    })
}

pub(crate) fn validate_prepare_request(
    risk_acknowledged: bool,
    collector_state: CollectorState,
) -> Result<(), String> {
    if !risk_acknowledged {
        return Err("CP2077 trial requires explicit injection-risk acknowledgement".into());
    }
    if !matches!(
        collector_state,
        CollectorState::Idle | CollectorState::Completed | CollectorState::Faulted
    ) {
        return Err(format!(
            "CP2077 trial preparation is blocked while the main collector is {collector_state:?}"
        ));
    }
    Ok(())
}

fn ensure_cp2077_launch_gate() -> Result<(), String> {
    validate_cp2077_rights_manifest(RIGHTS_MANIFEST_BYTES)?;
    validate_runtime_launch_evidence(RUNTIME_EVIDENCE_BYTES)
}

/// Requires a live publisher/counsel approval for CP2077 in the embedded rights ledger.
///
/// Issue #855 §E replaced the old project research-only lane with one unified decision that
/// covers every real game, so this gate now checks the same facts the repository rights gate
/// checks in CI: an approval decision, four explicitly bounded scopes, an accountable
/// reviewer, and dates that are in force today. It is deliberately *not* the whole gate:
/// `validate_runtime_launch_evidence` still enforces the observed D3D12/anti-cheat/network
/// facts, and no rights decision can satisfy that.
fn validate_cp2077_rights_manifest(bytes: &[u8]) -> Result<(), String> {
    let manifest: RightsManifest = serde_json::from_slice(bytes)
        .map_err(|error| format!("parse embedded game Depth rights manifest failed: {error}"))?;
    if manifest.schema_version != RIGHTS_MANIFEST_SCHEMA_VERSION {
        return Err("embedded game Depth rights manifest schema is unsupported".into());
    }
    if manifest.project_research_only.is_some() {
        return Err(
            "embedded rights manifest still carries the retired project_research_only lane".into(),
        );
    }
    let today = time::OffsetDateTime::now_utc().date().to_string();
    let unified = &manifest.unified_rights_decision;
    if !non_blank(&unified.declared_by)
        || !non_blank(&unified.note)
        || !http_url(&unified.reference_url)
        || !iso_date(&unified.declared_at)
        || unified.declared_at.as_str() > today.as_str()
    {
        return Err("embedded unified rights decision is invalid or not effective".into());
    }

    let mut matching = manifest
        .records
        .iter()
        .filter(|record| record.app_id == CP2077_APP_ID);
    let record = matching
        .next()
        .ok_or("embedded rights manifest has no CP2077 record")?;
    if matching.next().is_some() {
        return Err("embedded rights manifest has duplicate CP2077 records".into());
    }
    if !non_blank(&record.game)
        || !non_blank(&record.rights_owner)
        || !non_blank(&record.mode_restrictions)
        || !non_blank(&record.reviewer)
    {
        return Err("embedded CP2077 rights record is incomplete".into());
    }
    if !matches!(
        record.decision.as_str(),
        "publisher_approved" | "counsel_approved"
    ) {
        return Err(format!(
            "CP2077 rights decision '{}' is not a publisher or counsel approval",
            record.decision
        ));
    }
    for (name, scope) in [
        ("instrumentation", &record.scopes.instrumentation),
        ("capture", &record.scopes.capture),
        ("commercial_ml", &record.scopes.commercial_ml),
        ("delivery", &record.scopes.delivery),
    ] {
        if !scope.approved || !scope.boundaries.as_deref().is_some_and(non_blank) {
            return Err(format!(
                "embedded CP2077 rights scope '{name}' is not approved with explicit boundaries"
            ));
        }
    }
    // Issue #855 §E.1: the written counsel document has not been issued yet, so an absent or
    // blank link no longer blocks. A link that is present must still be a real absolute
    // http(s) URL so a broken reference cannot pass as evidence.
    if let Some(url) = record.evidence_url.as_deref()
        && non_blank(url)
        && !http_url(url)
    {
        return Err(
            "embedded CP2077 evidence_url is present but is not an absolute http(s) link".into(),
        );
    }
    if !iso_date(&record.effective_date) || record.effective_date.as_str() > today.as_str() {
        return Err("embedded CP2077 rights approval is not effective yet".into());
    }
    if !iso_date(&record.expiry_or_review_date)
        || record.expiry_or_review_date.as_str() < today.as_str()
    {
        return Err("embedded CP2077 rights approval is expired or past its review date".into());
    }
    // Widening the rights never widens the runtime. The anti-cheat ceiling must stay written
    // into the record itself, independently of the approved scopes.
    if !record
        .mode_restrictions
        .to_ascii_lowercase()
        .contains("anti-cheat")
    {
        return Err("embedded CP2077 mode restrictions omit the anti-cheat ceiling".into());
    }
    Ok(())
}

fn validate_runtime_launch_evidence(bytes: &[u8]) -> Result<(), String> {
    let evidence: RuntimeEvidence = serde_json::from_slice(bytes)
        .map_err(|error| format!("parse embedded CP2077 runtime evidence failed: {error}"))?;
    if evidence.schema_version != "dm-cp2077-runtime-d6-observation-2"
        || evidence.identity.app_id != CP2077_APP_ID
        || evidence.identity.build_id != CP2077_BUILD_ID
        || !evidence.identity.executable_sha256.eq_ignore_ascii_case(
            GAME_EXE_SHA256
                .strip_prefix("sha256:")
                .expect("early-launch target SHA-256 is normalized"),
        )
    {
        return Err("embedded CP2077 runtime evidence does not match the pinned build".into());
    }
    if !evidence.non_trainable || !evidence.graphics.direct3d12_confirmed {
        return Err(
            "embedded CP2077 runtime evidence violates the local non-trainable D3D12 lane".into(),
        );
    }
    let protection = &evidence.protection_and_network;
    if evidence.status != "pass"
        || !non_blank(&evidence.launch.method)
        || !protection.runtime_protection_gate_completed
        || !protection.known_anticheat_modules_observed.is_empty()
        || !protection.known_anticheat_processes_observed.is_empty()
        || protection.game_runtime_network_policy != "observe_only_non_blocking"
    {
        return Err(format!(
            "CP2077 runtime protection gate is blocked (status={}, external_tcp_connections_observed={}); rerun the exact-build D3D12/anti-cheat observation before instrumentation",
            evidence.status, protection.established_external_tcp_connections
        ));
    }
    Ok(())
}

fn non_blank(value: &str) -> bool {
    !value.trim().is_empty()
}

fn http_url(value: &str) -> bool {
    value.starts_with("https://") || value.starts_with("http://")
}

fn iso_date(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| matches!(index, 4 | 7) || byte.is_ascii_digit())
}

fn reserve_prepare(
    runtime: &mut TrialRuntime,
    purpose: MainCollectorPurpose,
) -> Result<(), String> {
    if matches!(
        runtime.phase,
        TrialPhase::Preparing
            | TrialPhase::Prepared
            | TrialPhase::Recording
            | TrialPhase::Processing
    ) {
        return Err("a CP2077 trial is already preparing, prepared, or recording".into());
    }
    runtime.phase = TrialPhase::Preparing;
    runtime.requested_purpose = Some(purpose);
    runtime.prepared = None;
    runtime.video_frames = None;
    runtime.depth_frames = None;
    runtime.keyboard_input_events = None;
    runtime.mouse_input_events = None;
    runtime.metric_marker_count = 0;
    runtime.metric_calibration_samples = None;
    runtime.recording_started = None;
    runtime.last_error = None;
    Ok(())
}

pub fn status() -> TrialStatus {
    let mut guard = runtime().lock().expect("CP2077 trial runtime poisoned");
    refresh_prepared_process(&mut guard);
    observe_recording_start(&mut guard);
    status_from(&guard)
}

fn refresh_prepared_process(runtime: &mut TrialRuntime) {
    if runtime.phase != TrialPhase::Prepared {
        return;
    }
    let Some(prepared) = runtime.prepared.as_ref() else {
        runtime.phase = TrialPhase::Faulted;
        runtime.last_error = Some("prepared CP2077 trial has no launch provenance".into());
        return;
    };
    if let Err(error) = validate_prepared_process_identity(prepared) {
        runtime.phase = TrialPhase::Faulted;
        runtime.prepared = None;
        runtime.last_error = Some(error);
    }
}

fn validate_prepared_process_identity(prepared: &PreparedTrial) -> Result<(), String> {
    let observed = process_creation_time_100ns(prepared.process_id)?;
    if observed != prepared.process_creation_time_100ns {
        return Err(
            "the prepared CP2077 process exited or its PID was reused; prepare a new trial".into(),
        );
    }
    Ok(())
}

fn process_creation_time_100ns(process_id: u32) -> Result<u64, String> {
    if process_id == 0 {
        return Err("CP2077 process id is zero".into());
    }
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id) }
        .map_err(|error| {
            format!("the prepared CP2077 process is no longer queryable ({process_id}): {error}")
        })?;
    let result = query_process_creation_time(process);
    let _ = unsafe { CloseHandle(process) };
    result
}

fn query_process_creation_time(process: HANDLE) -> Result<u64, String> {
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
    .map_err(|error| format!("query prepared CP2077 process creation time failed: {error}"))?;
    Ok((u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime))
}

fn observe_recording_start(runtime: &mut TrialRuntime) {
    if runtime.phase != TrialPhase::Recording || runtime.recording_started.is_some() {
        return;
    }
    let Some(prepared) = runtime.prepared.as_ref() else {
        return;
    };
    let Ok(modified) =
        fs::metadata(prepared.session_dir.join(ARM_FILE)).and_then(|value| value.modified())
    else {
        return;
    };
    let age = modified.elapsed().unwrap_or_default();
    let now = Instant::now();
    runtime.recording_started = Some(now.checked_sub(age).unwrap_or(now));
}

fn status_from(runtime: &TrialRuntime) -> TrialStatus {
    TrialStatus {
        available: !crate::build_flavor::uses_delivered_catalog(),
        phase: runtime.phase,
        purpose: runtime
            .prepared
            .as_ref()
            .map(|value| value.purpose)
            .or(runtime.requested_purpose),
        process_id: runtime.prepared.as_ref().map(|value| value.process_id),
        session_locator: runtime
            .prepared
            .as_ref()
            .and_then(|value| private_locator(&value.session_dir).ok()),
        profile_candidate: runtime
            .prepared
            .as_ref()
            .map(|value| value.candidate_id.clone()),
        elapsed_target_seconds: DURATION_MS / 1_000,
        video_frames: runtime.video_frames,
        depth_frames: runtime.depth_frames,
        keyboard_input_events: runtime.keyboard_input_events,
        mouse_input_events: runtime.mouse_input_events,
        metric_marker_count: runtime.metric_marker_count,
        metric_calibration_samples: runtime.metric_calibration_samples,
        metric_evidence_sealed: runtime.metric_evidence.is_some(),
        metric_evidence_metres_per_world_unit: runtime
            .metric_evidence
            .as_ref()
            .map(|value| value.metres_per_world_unit),
        metric_evidence_session_locator: runtime
            .metric_evidence
            .as_ref()
            .map(|value| value.session_locator.clone()),
        delivery_locator: runtime
            .prepared
            .as_ref()
            .and_then(|value| value.delivery_dir.as_ref())
            .and_then(|value| private_delivery_locator(value).ok()),
        recording_elapsed_seconds: runtime
            .recording_started
            .map(|value| value.elapsed().as_secs())
            .unwrap_or(0),
        can_stop: runtime.phase == TrialPhase::Recording
            && runtime
                .recording_started
                .is_some_and(|value| value.elapsed().as_millis() >= u128::from(DURATION_MS)),
        last_error: runtime.last_error.clone(),
        non_trainable: true,
        recording_ready: false,
    }
}

/// Called only after the main collector has durably appended a hotkey marker to its journal.
pub fn note_metric_marker() {
    if let Ok(mut guard) = runtime().lock()
        && guard.phase == TrialPhase::Recording
        && guard
            .prepared
            .as_ref()
            .is_some_and(|prepared| prepared.purpose == MainCollectorPurpose::MetricCalibration)
    {
        guard.metric_marker_count = guard.metric_marker_count.saturating_add(1);
    }
}

pub fn prepare_and_launch(
    risk_acknowledged: bool,
    collector_state: CollectorState,
    purpose: MainCollectorPurpose,
) -> Result<TrialStatus, String> {
    if crate::build_flavor::uses_delivered_catalog() {
        return Err(
            "CP2077 local Depth trial is disabled in server-delivered catalog builds".into(),
        );
    }
    validate_prepare_request(risk_acknowledged, collector_state)?;
    ensure_cp2077_launch_gate()?;
    {
        let mut guard = runtime()
            .lock()
            .map_err(|_| "CP2077 trial runtime lock is poisoned")?;
        reserve_prepare(&mut guard, purpose)?;
    }

    let result = prepare_and_launch_inner(purpose);
    let mut guard = runtime()
        .lock()
        .map_err(|_| "CP2077 trial runtime lock is poisoned")?;
    match result {
        Ok(prepared) => {
            guard.phase = TrialPhase::Prepared;
            guard.prepared = Some(prepared);
            guard.last_error = None;
            Ok(status_from(&guard))
        }
        Err(error) => {
            guard.phase = TrialPhase::Faulted;
            guard.prepared = None;
            guard.last_error = Some(error.clone());
            Err(error)
        }
    }
}

fn prepare_and_launch_inner(purpose: MainCollectorPurpose) -> Result<PreparedTrial, String> {
    let tools = default_tools_dir();
    let launcher = tools.join("dm-depth-launch.exe");
    let probe = tools.join("dm_depth_probe.dll");
    let setter = tools.join("Set-Cp2077CaptureSettings.ps1");
    for (path, label) in [
        (&launcher, "early launcher"),
        (&probe, "depth probe"),
        (&setter, "PR727 settings recipe"),
    ] {
        if !path.is_absolute() || !path.is_file() {
            return Err(format!("packaged CP2077 {label} is missing"));
        }
    }
    if purpose == MainCollectorPurpose::FiveStream {
        verify_packager_runtime(&tools.join(PACKAGER_FILE))?;
    }

    apply_settings(&setter, purpose)?;
    let probe_sha256 = sha256_file(&probe)?;
    let source_profile = find_matching_profile(&probe_sha256)?;
    let profile_bytes = fs::read(&source_profile)
        .map_err(|error| format!("read matching raw-depth profile failed: {error}"))?;
    let profile: depth_lab::RawDepthDemoProfile = serde_json::from_slice(&profile_bytes)
        .map_err(|error| format!("parse matching raw-depth profile failed: {error}"))?;
    profile.validate()?;

    let private_root = private_root()?;
    fs::create_dir_all(&private_root)
        .map_err(|error| format!("create private CP2077 trial root failed: {error}"))?;
    let session_dir = private_root.join(format!("client-trial-{}", uuid::Uuid::new_v4().simple()));
    fs::create_dir(&session_dir)
        .map_err(|error| format!("create private CP2077 trial directory failed: {error}"))?;
    write_create_new(
        &session_dir.join(PROFILE_FILE),
        &profile_bytes,
        "trial profile",
    )?;
    let profile_sha256 = sha256_bytes(&profile_bytes);
    let config = trial_config(&profile_sha256);
    let mut config_bytes = serde_json::to_vec_pretty(&config)
        .map_err(|error| format!("serialize CP2077 trial config failed: {error}"))?;
    config_bytes.push(b'\n');
    let config_path = session_dir.join(CONFIG_FILE);
    write_create_new(&config_path, &config_bytes, "trial config")?;

    let executable = resolve_cp2077_catalog_executable()?;
    EARLY_LAUNCH_TARGET
        .verify_executable_identity(EARLY_LAUNCH_TARGET.game_id(), GAME_EXE_SHA256)?;
    let args = EarlyLauncherRequest {
        target: EARLY_LAUNCH_TARGET,
        executable: &executable,
        probe: &probe,
        probe_sha256: &probe_sha256,
        mode: EarlyLauncherMode::RawDepthDemo {
            config: &config_path,
        },
    }
    .args()?;
    let output = std::process::Command::new(&launcher)
        .creation_flags(CREATE_NO_WINDOW.0)
        .args(args)
        .output()
        .map_err(|error| format!("start packaged CP2077 early launcher failed: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "CP2077 early launcher rejected the trial ({:?}): {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let launched = parse_early_launcher_output(&output.stdout, &probe_sha256)?;
    let process_id = launched.process_id;
    let launch_id = launched.launch_id;
    let process_creation_time_100ns = process_creation_time_100ns(process_id)?;

    Ok(PreparedTrial {
        process_id,
        process_creation_time_100ns,
        launch_id,
        session_dir,
        candidate_id: profile.selector.candidate_id,
        purpose,
        delivery_dir: None,
    })
}

fn trial_config(profile_sha256: &str) -> serde_json::Value {
    serde_json::json!({
        "schema": CONFIG_SCHEMA,
        "output_dir": ".",
        "profile_file": PROFILE_FILE,
        "profile_sha256": profile_sha256,
        "capture_contract": CONTRACT,
        "duration_ms": DURATION_MS,
        "manual_stop": true,
        "max_frames": 4096,
        "present_stride": 4,
        "recording_ready": false,
        "non_trainable": true,
        "upload_allowed": false,
        "formal_start_run": false
    })
}

pub fn start_recording() -> Result<TrialStatus, String> {
    Err("CP2077 depth now records through the main collector Start button".into())
}

pub fn stop_recording() -> Result<TrialStatus, String> {
    Err("CP2077 depth now stops through the main collector Stop button".into())
}

pub fn begin_main_collector(
    game_id: &str,
    process_id: u32,
    session_id: &str,
    run_id: &str,
    generation: u64,
) -> Result<Option<MainCollectorSidecarGuard>, String> {
    if game_id != "cyberpunk-2077-steam" {
        return Ok(None);
    }
    let prepared = {
        let guard = runtime()
            .lock()
            .map_err(|_| "CP2077 trial runtime lock is poisoned")?;
        if guard.phase != TrialPhase::Prepared {
            return match guard.phase {
                TrialPhase::Recording | TrialPhase::Processing => {
                    Err("a CP2077 Depth trial is already active".into())
                }
                TrialPhase::Idle | TrialPhase::Completed | TrialPhase::Faulted => Ok(None),
                TrialPhase::Preparing => Err("a CP2077 Depth trial is still preparing".into()),
                TrialPhase::Prepared => unreachable!(),
            };
        }
        let prepared = guard
            .prepared
            .clone()
            .ok_or("prepared CP2077 Depth session has no launch provenance")?;
        if prepared.process_id != process_id {
            return Err("the bound CP2077 window is not the early-launched Depth process".into());
        }
        validate_prepared_process_identity(&prepared)?;
        prepared
    };
    Ok(Some(MainCollectorSidecarGuard {
        prepared,
        session_id: session_id.to_owned(),
        run_id: run_id.to_owned(),
        generation,
        armed: false,
        stopped: false,
    }))
}

impl MainCollectorSidecarGuard {
    pub fn arm_after_journal_commit(&mut self) -> Result<(), String> {
        if self.armed {
            return Err("CP2077 Depth sidecar was already armed".into());
        }
        prepare_main_collector_sidecar(
            &self.prepared.session_dir,
            self.prepared.process_id,
            &self.prepared.launch_id,
            self.prepared.purpose,
        )
        .map_err(|error| format!("prepare main-collector Depth sidecar failed: {error:#}"))?;
        arm_main_collector_sidecar(
            &self.prepared.session_dir,
            &self.prepared.launch_id,
            self.prepared.process_id,
            &self.session_id,
            &self.run_id,
            self.generation,
            self.prepared.purpose,
        )
        .map_err(|error| format!("arm main-collector Depth sidecar failed: {error:#}"))?;
        self.armed = true;
        let mut guard = runtime()
            .lock()
            .map_err(|_| "CP2077 trial runtime lock is poisoned")?;
        guard.phase = TrialPhase::Recording;
        guard.recording_started = Some(Instant::now());
        guard.last_error = None;
        Ok(())
    }

    pub fn stop_probe(&mut self) -> Result<(), String> {
        if !self.armed || self.stopped {
            return Ok(());
        }
        stop_main_collector_sidecar(&self.prepared.session_dir, &self.prepared.launch_id)
            .map_err(|error| format!("stop main-collector Depth sidecar failed: {error:#}"))?;
        self.stopped = true;
        Ok(())
    }

    pub fn finalize(
        &mut self,
        outcome: &RecordingOutcome,
        journal: &SessionJournal,
    ) -> Result<(), String> {
        self.stop_probe()?;
        {
            let mut guard = runtime()
                .lock()
                .map_err(|_| "CP2077 trial runtime lock is poisoned")?;
            guard.phase = TrialPhase::Processing;
        }
        let markers = journal
            .markers_for_session(&self.session_id)
            .map_err(|error| format!("read CP2077 main-run markers failed: {error:#}"))?
            .into_iter()
            .filter(|marker| marker.run_id == self.run_id)
            .collect::<Vec<_>>();
        let summary = finalize_main_collector_sidecar(
            &self.prepared.session_dir,
            &self.prepared.launch_id,
            self.prepared.process_id,
            &self.session_id,
            &self.run_id,
            self.generation,
            outcome,
            self.prepared.purpose,
            &markers,
        )
        .map_err(|error| format!("finalize main-collector Depth sidecar failed: {error:#}"))?;
        let delivery_dir = if self.prepared.purpose == MainCollectorPurpose::FiveStream {
            let delivery_dir = package_local_fivepiece(&self.prepared.session_dir)?;
            let result_sha256 = write_main_collector_manifest(
                outcome,
                &self.prepared,
                &self.session_id,
                &self.run_id,
                self.generation,
                &summary,
                &delivery_dir,
            )?;
            let sidecar_locator = private_locator(&self.prepared.session_dir)?;
            let delivery_locator = private_delivery_locator(&delivery_dir)?;
            journal
                .commit_cp2077_main_run(&Cp2077MainRunCommit {
                    session_id: &self.session_id,
                    run_id: &self.run_id,
                    generation: self.generation,
                    process_id: self.prepared.process_id,
                    process_launch_id: &self.prepared.launch_id,
                    result_relative_path: "cp2077-main-result.json",
                    result_sha256: &result_sha256,
                    sidecar_locator: &sidecar_locator,
                    delivery_locator: &delivery_locator,
                    rgb_frames: summary.rgb_frames,
                    source_depth_frames: summary.source_depth_frames,
                    paired_depth_frames: summary.paired_depth_frames,
                    duration_ms: summary.rgb_duration_ms,
                    capture_queue_drops: summary.capture_queue_drops,
                    input_queue_overflows: summary.input_queue_overflows,
                    exact_present_matches: u64::try_from(summary.exact_present_matches)
                        .map_err(|_| "CP2077 Present match count overflow")?,
                    exact_present_pairing_rate: summary.exact_present_pairing_rate,
                    raw_input_events: u64::try_from(summary.raw_input_events)
                        .map_err(|_| "CP2077 Raw Input count overflow")?,
                    keyboard_input_events: u64::try_from(summary.keyboard_input_events)
                        .map_err(|_| "CP2077 keyboard input count overflow")?,
                    mouse_input_events: u64::try_from(summary.mouse_input_events)
                        .map_err(|_| "CP2077 mouse input count overflow")?,
                    player_pose_samples: u64::try_from(summary.player_pose_samples)
                        .map_err(|_| "CP2077 Player Pose count overflow")?,
                    render_camera_samples: u64::try_from(summary.render_camera_samples)
                        .map_err(|_| "CP2077 Render Camera count overflow")?,
                })
                .map_err(|error| {
                    format!("commit CP2077 main run journal fact failed: {error:#}")
                })?;
            Some(delivery_dir)
        } else {
            write_metric_calibration_manifest(outcome, &self.prepared, &summary)?;
            None
        };
        let mut guard = runtime()
            .lock()
            .map_err(|_| "CP2077 trial runtime lock is poisoned")?;
        guard.phase = TrialPhase::Completed;
        guard.video_frames = Some(summary.rgb_frames);
        guard.depth_frames = Some(summary.paired_depth_frames);
        guard.keyboard_input_events = Some(summary.keyboard_input_events as u64);
        guard.mouse_input_events = Some(summary.mouse_input_events as u64);
        guard.metric_calibration_samples = summary.metric_calibration_samples;
        if let Some(current) = guard.prepared.as_mut() {
            current.delivery_dir = delivery_dir;
        }
        Ok(())
    }

    pub fn mark_faulted(&mut self, error: &str) {
        let _ = self.stop_probe();
        if let Ok(mut guard) = runtime().lock() {
            guard.phase = TrialPhase::Faulted;
            guard.last_error = Some(error.to_owned());
        }
    }
}

impl Drop for MainCollectorSidecarGuard {
    fn drop(&mut self) {
        let _ = self.stop_probe();
        if self.armed
            && let Ok(mut guard) = runtime().lock()
            && matches!(guard.phase, TrialPhase::Recording | TrialPhase::Processing)
        {
            guard.phase = TrialPhase::Faulted;
            guard.last_error.get_or_insert_with(|| {
                "CP2077 main collector ended before five-stream sealing".into()
            });
        }
    }
}

fn write_metric_calibration_manifest(
    outcome: &RecordingOutcome,
    prepared: &PreparedTrial,
    summary: &MainCollectorDepthSummary,
) -> Result<(), String> {
    let sample_count = summary
        .metric_calibration_samples
        .ok_or("metric calibration finalized without F9 sample evidence")?;
    let path = outcome
        .run_directory
        .join("cp2077-metric-calibration-result.json");
    let value = serde_json::json!({
        "schema": "dm-cp2077-main-metric-calibration-result/v1",
        "session_id": outcome.session_id,
        "run_id": outcome.run_id,
        "process_id": prepared.process_id,
        "process_launch_id": prepared.launch_id,
        "purpose": "metric_calibration",
        "sample_count": sample_count,
        "sidecar": private_locator(&prepared.session_dir)?,
        "review_state": "awaiting_independent_hud_review",
        "metric_evidence_sealed": false,
        "hud_visible_for_calibration_only": true,
        "non_trainable": true,
        "production_certified": false,
        "upload_allowed": false,
        "deliverable": false,
        "recording_ready": false
    });
    let mut bytes = serde_json::to_vec_pretty(&value)
        .map_err(|error| format!("serialize metric calibration manifest failed: {error}"))?;
    bytes.push(b'\n');
    write_create_new(&path, &bytes, "main-collector CP2077 metric result")
}

fn write_main_collector_manifest(
    outcome: &RecordingOutcome,
    prepared: &PreparedTrial,
    session_id: &str,
    run_id: &str,
    generation: u64,
    summary: &MainCollectorDepthSummary,
    delivery_dir: &Path,
) -> Result<String, String> {
    if outcome.session_id != session_id || outcome.run_id != run_id {
        return Err("CP2077 main result identity conflicts with recorder outcome".into());
    }
    let path = outcome.run_directory.join("cp2077-main-result.json");
    let value = serde_json::json!({
        "schema": "dm-cp2077-main-collector-result/v1",
        "session_id": session_id,
        "run_id": run_id,
        "generation": generation,
        "process_id": prepared.process_id,
        "process_launch_id": prepared.launch_id,
        "capture_owner": "dataminer_main_collector",
        "rgb_segment": "segments/00000",
        "rgb_frames": summary.rgb_frames,
        "source_depth_frames": summary.source_depth_frames,
        "paired_depth_frames": summary.paired_depth_frames,
        "rgb_duration_ms": summary.rgb_duration_ms,
        "depth_duration_ms": summary.depth_duration_ms,
        "exact_present_matches": summary.exact_present_matches,
        "exact_present_pairing_rate": summary.exact_present_pairing_rate,
        "raw_input_events": summary.raw_input_events,
        "keyboard_input_events": summary.keyboard_input_events,
        "mouse_input_events": summary.mouse_input_events,
        "player_pose_samples": summary.player_pose_samples,
        "render_camera_samples": summary.render_camera_samples,
        "capture_queue_drops": summary.capture_queue_drops,
        "input_queue_overflows": summary.input_queue_overflows,
        "sidecar": private_locator(&prepared.session_dir)?,
        "local_fivepiece": private_delivery_locator(delivery_dir)?,
        "confirmed_flush": true,
        "main_collector_started": true,
        "main_collector_recording_ready": false,
        "sidecar_probe_recording_ready": false,
        "main_collector_formal_start_run": true,
        "sidecar_probe_formal_start_run": false,
        "classification": "local_non_trainable_fivepiece",
        "non_trainable": true,
        "production_certified": false,
        "upload_allowed": false,
        "deliverable": false,
        "blockers": [
            "native reverse-Z D32 is not metric PR405 EXR",
            "production camera binding, metric scale and E-geo remain unresolved"
        ]
    });
    let mut bytes = serde_json::to_vec_pretty(&value)
        .map_err(|error| format!("serialize main-collector depth manifest failed: {error}"))?;
    bytes.push(b'\n');
    write_create_new(&path, &bytes, "main-collector CP2077 result")?;
    Ok(sha256_bytes(&bytes))
}

pub fn request_main_collector_stop() -> Result<bool, String> {
    let prepared = {
        let guard = runtime()
            .lock()
            .map_err(|_| "CP2077 trial runtime lock is poisoned")?;
        if guard.phase != TrialPhase::Recording {
            return Ok(false);
        }
        guard
            .prepared
            .clone()
            .ok_or("recording CP2077 main run has no launch provenance")?
    };
    stop_main_collector_sidecar(&prepared.session_dir, &prepared.launch_id)
        .map_err(|error| format!("stop main-collector Depth sidecar failed: {error:#}"))?;
    Ok(true)
}

pub fn open_output() -> Result<(), String> {
    let guard = runtime()
        .lock()
        .map_err(|_| "CP2077 trial runtime lock is poisoned")?;
    let directory = guard
        .prepared
        .as_ref()
        .map(|value| {
            value
                .delivery_dir
                .clone()
                .unwrap_or_else(|| value.session_dir.clone())
        })
        .or_else(|| {
            guard
                .metric_evidence
                .as_ref()
                .map(|value| value.session_dir.clone())
        })
        .ok_or("no CP2077 trial output is available")?;
    drop(guard);
    shell::open_directory(&directory).map_err(|error| format!("open trial output failed: {error}"))
}

fn package_local_fivepiece(session_dir: &Path) -> Result<PathBuf, String> {
    let packager = default_tools_dir().join(PACKAGER_FILE);
    if !packager.is_file() {
        return Err("packaged CP2077 five-piece tool is missing".into());
    }
    let output = delivery_root()?.join(format!(
        "client-fivepiece-{}",
        uuid::Uuid::new_v4().simple()
    ));
    let attempts = [("python.exe", Vec::<&str>::new()), ("py.exe", vec!["-3"])];
    let mut failures = Vec::new();
    for (program, prefix) in attempts {
        let result = std::process::Command::new(program)
            .creation_flags(CREATE_NO_WINDOW.0)
            .args(prefix)
            .arg(&packager)
            .arg(session_dir)
            .arg(&output)
            .output();
        match result {
            Ok(result) if result.status.success() => return Ok(output),
            Ok(result) => failures.push(format!(
                "{program} exited {:?}: {}",
                result.status.code(),
                String::from_utf8_lossy(&result.stderr).trim()
            )),
            Err(error) => failures.push(format!("{program}: {error}")),
        }
    }
    Err(format!(
        "CP2077 five-piece packaging requires a local Python 3 runtime with zstandard: {}",
        failures.join("; ")
    ))
}

fn verify_packager_runtime(packager: &Path) -> Result<(), String> {
    if !packager.is_file() {
        return Err("packaged CP2077 five-piece tool is missing".into());
    }
    let attempts = [("python.exe", Vec::<&str>::new()), ("py.exe", vec!["-3"])];
    let mut failures = Vec::new();
    for (program, prefix) in attempts {
        let result = std::process::Command::new(program)
            .creation_flags(CREATE_NO_WINDOW.0)
            .args(prefix)
            .args(["-c", "import zstandard"])
            .output();
        match result {
            Ok(result) if result.status.success() => return Ok(()),
            Ok(result) => failures.push(format!("{program} exited {:?}", result.status.code())),
            Err(error) => failures.push(format!("{program}: {error}")),
        }
    }
    Err(format!(
        "CP2077 five-piece packaging requires a local Python 3 runtime with zstandard: {}",
        failures.join("; ")
    ))
}

fn apply_settings(setter: &Path, purpose: MainCollectorPurpose) -> Result<(), String> {
    let output = std::process::Command::new("powershell.exe")
        .creation_flags(CREATE_NO_WINDOW.0)
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(setter)
        .args([
            "-Purpose",
            match purpose {
                MainCollectorPurpose::FiveStream => "formal",
                MainCollectorPurpose::MetricCalibration => "metric_calibration",
            },
        ])
        .output()
        .map_err(|error| format!("run packaged PR727 settings recipe failed: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "PR727 settings recipe failed ({:?}): {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

fn find_matching_profile(probe_sha256: &str) -> Result<PathBuf, String> {
    let root = discovery_profile_root()?;
    find_matching_profile_in(&root, probe_sha256)
}

fn find_matching_profile_in(root: &Path, probe_sha256: &str) -> Result<PathBuf, String> {
    if !root.is_dir() {
        return Err("no CP2077 discovery profile directory exists; run discovery first".into());
    }
    let mut candidates = Vec::new();
    for entry in fs::read_dir(root).map_err(|error| format!("read trial root failed: {error}"))? {
        let entry = entry.map_err(|error| format!("read trial directory entry failed: {error}"))?;
        let path = entry.path().join(PROFILE_FILE);
        let Ok(metadata) = path.metadata() else {
            continue;
        };
        if !metadata.is_file() || metadata.len() > MAX_PROFILE_BYTES {
            continue;
        }
        let Ok(bytes) = fs::read(&path) else {
            continue;
        };
        let Ok(profile) = serde_json::from_slice::<depth_lab::RawDepthDemoProfile>(&bytes) else {
            continue;
        };
        if profile.validate().is_ok()
            && profile.resident_probe_sha256 == probe_sha256
            && profile.capture_contract == CONTRACT
            && profile.duration_ms == DURATION_MS
            && profile.selector.width == 1920
            && profile.selector.height == 1080
            && profile.selector.resource_format == 19
            && profile.selector.dsv_format == 20
        {
            candidates.push((metadata.modified().ok(), path));
        }
    }
    candidates.sort_by_key(|(modified, _)| *modified);
    candidates
        .pop()
        .map(|(_, path)| path)
        .ok_or_else(|| {
            "no sealed 1920x1080 D32S8 profile matches the packaged probe; run discovery with this client build first".into()
        })
}

fn discovery_profile_root() -> Result<PathBuf, String> {
    local_app_data_root().map(|local| local.join("DataMiner/depth-discovery/cp2077"))
}

fn private_root() -> Result<PathBuf, String> {
    local_app_data_root().map(|local| local.join("DataMiner/depth-raw-demo/cp2077"))
}

fn latest_valid_metric_evidence() -> Result<Option<SealedMetricEvidence>, String> {
    latest_valid_metric_evidence_in(&private_root()?)
}

fn latest_valid_metric_evidence_in(root: &Path) -> Result<Option<SealedMetricEvidence>, String> {
    if !root.is_dir() {
        return Ok(None);
    }
    let mut candidates = fs::read_dir(root)
        .map_err(|error| format!("read private CP2077 trial root failed: {error}"))?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let file_type = entry.file_type().ok()?;
            if !file_type.is_dir() {
                return None;
            }
            let session_dir = entry.path();
            let evidence_path = session_dir.join(METRIC_EVIDENCE_FILE);
            let modified = fs::metadata(&evidence_path).ok()?.modified().ok()?;
            Some((modified, session_dir, evidence_path))
        })
        .collect::<Vec<_>>();
    candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.0));

    for (_, session_dir, evidence_path) in candidates {
        let Ok(validation) = depth_lab::cp2077_metric_evidence::validate_cp2077_metric_evidence(
            &session_dir,
            &evidence_path,
        ) else {
            continue;
        };
        let Some(session_name) = session_dir
            .file_name()
            .and_then(|value| value.to_str())
            .map(str::to_owned)
        else {
            continue;
        };
        return Ok(Some(SealedMetricEvidence {
            session_dir,
            session_locator: format!("DataMiner/depth-raw-demo/cp2077/{session_name}"),
            metres_per_world_unit: validation.adopted_scale,
        }));
    }
    Ok(None)
}

fn delivery_root() -> Result<PathBuf, String> {
    local_app_data_root().map(|local| local.join("DataMiner/depth-delivery/cp2077"))
}

fn local_app_data_root() -> Result<PathBuf, String> {
    let local = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .ok_or("LOCALAPPDATA is unavailable")?;
    if !local.is_absolute() {
        return Err("LOCALAPPDATA is not absolute".into());
    }
    Ok(local)
}

fn private_locator(path: &Path) -> Result<String, String> {
    let root = private_root()?;
    let name = path
        .strip_prefix(root)
        .map_err(|_| "trial output escaped private root")?
        .to_string_lossy();
    Ok(format!("DataMiner/depth-raw-demo/cp2077/{name}"))
}

fn private_delivery_locator(path: &Path) -> Result<String, String> {
    let root = delivery_root()?;
    let name = path
        .strip_prefix(root)
        .map_err(|_| "trial delivery escaped private root")?
        .to_string_lossy();
    Ok(format!("DataMiner/depth-delivery/cp2077/{name}"))
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
        .map_err(|error| format!("sync {label} failed: {error}"))
}

fn sha256_file(path: &Path) -> Result<String, String> {
    fs::read(path)
        .map(|bytes| sha256_bytes(&bytes))
        .map_err(|error| format!("hash {} failed: {error}", path.display()))
}

fn sha256_bytes(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tempfile::tempdir;

    #[test]
    fn status_never_claims_recording_ready() {
        let status = status_from(&TrialRuntime::default());
        assert!(status.non_trainable);
        assert!(!status.recording_ready);
        assert_eq!(status.elapsed_target_seconds, 64);
    }

    #[test]
    fn manual_stop_is_enabled_only_after_the_minimum_window() {
        let mut runtime = TrialRuntime {
            phase: TrialPhase::Recording,
            ..TrialRuntime::default()
        };
        runtime.recording_started = Some(Instant::now());
        assert!(!status_from(&runtime).can_stop);
        runtime.recording_started = Some(Instant::now() - Duration::from_secs(65));
        assert!(status_from(&runtime).can_stop);
    }

    #[test]
    fn recording_clock_waits_for_the_probe_arm_request() {
        let output = tempdir().unwrap();
        let mut runtime = TrialRuntime {
            phase: TrialPhase::Recording,
            prepared: Some(PreparedTrial {
                process_id: 42,
                process_creation_time_100ns: 1,
                launch_id: "0".repeat(32),
                session_dir: output.path().to_owned(),
                candidate_id: "dsv-r19-g1-e1".into(),
                purpose: MainCollectorPurpose::FiveStream,
                delivery_dir: None,
            }),
            ..TrialRuntime::default()
        };
        observe_recording_start(&mut runtime);
        assert!(runtime.recording_started.is_none());
        fs::write(output.path().join(ARM_FILE), b"{}\n").unwrap();
        observe_recording_start(&mut runtime);
        assert!(runtime.recording_started.is_some());
        assert_eq!(status_from(&runtime).recording_elapsed_seconds, 0);
    }

    #[test]
    fn trial_config_matches_the_manual_stop_probe_contract() {
        let config = trial_config(&format!("sha256:{}", "0".repeat(64)));
        assert_eq!(config["capture_contract"], CONTRACT);
        assert_eq!(config["duration_ms"], DURATION_MS);
        assert_eq!(config["manual_stop"], true);
        assert_eq!(config["max_frames"], 4096);
        assert_eq!(config["recording_ready"], false);
        assert_eq!(config["non_trainable"], true);
        assert_eq!(config["upload_allowed"], false);
        assert_eq!(config["formal_start_run"], false);
    }

    #[test]
    fn trial_prepare_requires_risk_acknowledgement_and_an_idle_collector() {
        assert!(validate_prepare_request(false, CollectorState::Idle).is_err());
        assert!(validate_prepare_request(true, CollectorState::Idle).is_ok());
        assert!(validate_prepare_request(true, CollectorState::Completed).is_ok());
        assert!(validate_prepare_request(true, CollectorState::Faulted).is_ok());
        for state in [
            CollectorState::Discovering,
            CollectorState::Bound,
            CollectorState::Preflighting,
            CollectorState::Starting,
            CollectorState::Recording,
            CollectorState::Pausing,
            CollectorState::Paused,
            CollectorState::Stopping,
        ] {
            assert!(validate_prepare_request(true, state).is_err(), "{state:?}");
        }
    }

    #[test]
    fn preparing_phase_reserves_the_single_launch_slot() {
        let mut runtime = TrialRuntime::default();
        reserve_prepare(&mut runtime, MainCollectorPurpose::MetricCalibration).unwrap();
        assert_eq!(runtime.phase, TrialPhase::Preparing);
        assert!(reserve_prepare(&mut runtime, MainCollectorPurpose::FiveStream).is_err());
    }

    #[test]
    fn embedded_rights_carry_a_live_unified_cp2077_approval() {
        validate_cp2077_rights_manifest(RIGHTS_MANIFEST_BYTES).unwrap();
    }

    #[test]
    fn cp2077_rights_gate_fails_closed_on_a_withdrawn_or_unbounded_approval() {
        let base: serde_json::Value = serde_json::from_slice(RIGHTS_MANIFEST_BYTES).unwrap();
        let cp2077_index = base["records"]
            .as_array()
            .unwrap()
            .iter()
            .position(|record| record["app_id"] == "1091500")
            .unwrap();

        for decision in ["not_requested", "requested", "denied", "expired"] {
            let mut manifest = base.clone();
            manifest["records"][cp2077_index]["decision"] = serde_json::json!(decision);
            assert!(
                validate_cp2077_rights_manifest(&serde_json::to_vec(&manifest).unwrap()).is_err(),
                "{decision} must not open the CP2077 launch gate"
            );
        }

        let mut unbounded = base.clone();
        unbounded["records"][cp2077_index]["scopes"]["delivery"]["approved"] =
            serde_json::json!(false);
        assert!(validate_cp2077_rights_manifest(&serde_json::to_vec(&unbounded).unwrap()).is_err());

        let mut expired = base.clone();
        expired["records"][cp2077_index]["expiry_or_review_date"] = serde_json::json!("2000-01-01");
        assert!(validate_cp2077_rights_manifest(&serde_json::to_vec(&expired).unwrap()).is_err());

        let mut not_yet = base.clone();
        not_yet["records"][cp2077_index]["effective_date"] = serde_json::json!("2999-01-01");
        assert!(validate_cp2077_rights_manifest(&serde_json::to_vec(&not_yet).unwrap()).is_err());

        let mut no_anti_cheat_ceiling = base.clone();
        no_anti_cheat_ceiling["records"][cp2077_index]["mode_restrictions"] =
            serde_json::json!("Offline single-player only.");
        assert!(
            validate_cp2077_rights_manifest(&serde_json::to_vec(&no_anti_cheat_ceiling).unwrap())
                .is_err(),
            "the anti-cheat ceiling must survive the wider rights approval"
        );

        // The retired research-only lane must never come back alongside the unified decision.
        let mut resurrected = base.clone();
        resurrected["project_research_only"] =
            serde_json::json!({ "decision": "project_owner_research_only" });
        assert!(
            validate_cp2077_rights_manifest(&serde_json::to_vec(&resurrected).unwrap()).is_err()
        );
    }

    #[test]
    fn cp2077_rights_gate_accepts_a_pending_evidence_link_but_not_a_broken_one() {
        let base: serde_json::Value = serde_json::from_slice(RIGHTS_MANIFEST_BYTES).unwrap();
        let cp2077_index = base["records"]
            .as_array()
            .unwrap()
            .iter()
            .position(|record| record["app_id"] == "1091500")
            .unwrap();

        for pending in [
            serde_json::Value::Null,
            serde_json::json!(""),
            serde_json::json!("  "),
        ] {
            let mut manifest = base.clone();
            manifest["records"][cp2077_index]["evidence_url"] = pending;
            validate_cp2077_rights_manifest(&serde_json::to_vec(&manifest).unwrap()).unwrap();
        }

        let mut broken = base.clone();
        broken["records"][cp2077_index]["evidence_url"] = serde_json::json!("not-a-url");
        assert!(validate_cp2077_rights_manifest(&serde_json::to_vec(&broken).unwrap()).is_err());
    }

    #[test]
    fn embedded_runtime_evidence_allows_observed_game_networking() {
        validate_runtime_launch_evidence(RUNTIME_EVIDENCE_BYTES).unwrap();
    }

    #[test]
    fn game_network_is_observed_while_anticheat_remains_fail_closed() {
        let mut evidence: serde_json::Value =
            serde_json::from_slice(RUNTIME_EVIDENCE_BYTES).unwrap();
        evidence["protection_and_network"]["established_external_tcp_connections"] =
            serde_json::json!(7);
        validate_runtime_launch_evidence(&serde_json::to_vec(&evidence).unwrap()).unwrap();

        evidence["protection_and_network"]["known_anticheat_modules_observed"] =
            serde_json::json!(["example-anticheat.dll"]);
        assert!(validate_runtime_launch_evidence(&serde_json::to_vec(&evidence).unwrap()).is_err());
    }

    #[test]
    fn prepared_process_identity_binds_pid_to_windows_creation_time() {
        let output = tempdir().unwrap();
        let creation = process_creation_time_100ns(std::process::id()).unwrap();
        let mut prepared = PreparedTrial {
            process_id: std::process::id(),
            process_creation_time_100ns: creation,
            launch_id: "0".repeat(32),
            session_dir: output.path().to_owned(),
            candidate_id: "dsv-r19-g1-e1".into(),
            purpose: MainCollectorPurpose::FiveStream,
            delivery_dir: None,
        };
        validate_prepared_process_identity(&prepared).unwrap();
        prepared.process_creation_time_100ns = creation.saturating_add(1);
        assert!(validate_prepared_process_identity(&prepared).is_err());
    }

    #[test]
    fn matching_profile_is_read_from_discovery_root_not_trial_output_root() {
        let local = tempdir().unwrap();
        let discovery = local.path().join("DataMiner/depth-discovery/cp2077");
        let session = discovery.join("sealed-session");
        fs::create_dir_all(&session).unwrap();
        let probe = format!("sha256:{}", "1".repeat(64));
        let profile = serde_json::json!({
            "schema": "dm-local-raw-depth-demo-profile/v1",
            "intent": "local_raw_depth_rgb_alignment_smoke",
            "game_id": "cyberpunk-2077-steam",
            "executable_sha256": depth_lab::CP2077_BUILD_20383525_EXE_SHA256,
            "process_launch_id": "0123456789abcdef0123456789abcdef",
            "resident_probe_sha256": probe,
            "source_d3d12_report_sha256": format!("sha256:{}", "3".repeat(64)),
            "source_sealed_marker_sha256": format!("sha256:{}", "4".repeat(64)),
            "discovery_provenance_mode": "manual_local_smoke",
            "capture_contract": CONTRACT,
            "duration_ms": DURATION_MS,
            "selector": {
                "candidate_id": "dsv-r15-g1-e1",
                "width": 1920,
                "height": 1080,
                "sample_count": 1,
                "resource_format": 19,
                "dsv_format": 20,
                "readback_slots": 4
            },
            "non_trainable": true,
            "production_certified": false,
            "upload_allowed": false,
            "deliverable": false,
            "recording_ready": false,
            "projection_resolved": false,
            "metric_scale_resolved": false,
            "camera_pose_resolved": false,
            "rgb_depth_egeo_passed": false,
            "performance_certified": false,
            "blockers": ["local discovery profile is not a production binding"]
        });
        fs::write(
            session.join(PROFILE_FILE),
            serde_json::to_vec_pretty(&profile).unwrap(),
        )
        .unwrap();

        let selected = find_matching_profile_in(&discovery, &probe).unwrap();
        assert_eq!(selected, session.join(PROFILE_FILE));
        assert!(
            !local
                .path()
                .join("DataMiner/depth-raw-demo/cp2077")
                .exists()
        );
    }
}

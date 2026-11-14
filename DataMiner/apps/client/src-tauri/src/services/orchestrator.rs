use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, ensure};
use capture_core::ingest_contract::SessionContract;
use capture_core::input::{
    RAW_INPUT_QUEUE_CAPACITY, RawInputControl, RawInputRecord, RawInputStatus,
};
use capture_core::qpc::QpcClock;
use capture_core::recording::{
    RecorderEvent, RecordingProgress, RecordingRequest, RecordingStopReason, RecordingStopSignal,
    RecordingTarget, probe_hardware_path, record_until_stopped,
};
use crossbeam_channel::{Sender, bounded};
use parking_lot::{Mutex, MutexGuard};
use tauri::{AppHandle, Emitter, Manager};
use view_scale::evidence::CalibrationEvidence;

use crate::domain::collector::{
    BoundTargetSummary, CaptureStatus, CollectorFault, CollectorSnapshot, CollectorState,
    GameSummary, MarkerAudit, MarkerKind, OverlayAction, OverlaySnapshot, PreflightStatus,
    StorageState, StorageStatus, UploadStatus, WindowCandidate,
};
use crate::platform::windows::window_tracker::{BoundTarget, TargetWindowMonitor, WindowTracker};
use crate::platform::windows::{folder_picker, shell};
use crate::services::capture_storage::{CaptureStorageManager, CaptureStorageStatus};
#[cfg(not(feature = "local-catalog-only"))]
use crate::services::catalog_delivery::ensure_catalog_set_does_not_rollback;
use crate::services::data_model::DataModelService;
use crate::services::diagnostics::{
    DiagnosticCorrelation, DiagnosticExportResult, DiagnosticPayload, DiagnosticService,
    RuntimeCaptureDiagnostic, RuntimeDiagnosticSnapshot, RuntimeTargetDiagnostic,
};
use crate::services::heartbeats::HeartbeatStore;
use crate::services::journal::{
    DataModelJobStatus, HeartbeatWrite, JOURNAL_SCHEMA_VERSION, MarkerWrite, PreflightStart,
    RunStart, RunTerminalState, SessionHistoryDetail, SessionHistorySummary, SessionJournal,
};
use crate::services::markers::MarkerStore;
use crate::services::overlay::{OverlayWindowController, emit_unavailable_warning};
use crate::services::preflight;
use crate::services::recovery;
use crate::services::sentry::{
    DiagnosticAttachmentStatus, MAX_DIAGNOSTIC_ATTACHMENT_BYTES, SentryRecordingFailureEvent,
    SentryReportReservation, SentryReporter,
};
use crate::services::session_history::{self, SessionDeletionResult};
use crate::services::storage::{self, StartKind, StorageBudget};

static SESSION_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static MARKER_REQUEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static CONTROL_REQUEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);
const RECORDER_EVENT_QUEUE_CAPACITY: usize = 16;
const HARDWARE_PREFLIGHT_SCRATCH_DIRECTORY: &str = ".preflight-scratch";

#[cfg(feature = "depth-lab")]
const fn should_finalize_depth_run(performance_validation_only: bool) -> bool {
    !performance_validation_only
}

struct RecorderEventWorker {
    sender: Sender<QueuedRecorderEvent>,
    join: JoinHandle<anyhow::Result<()>>,
}

struct QueuedRecorderEvent {
    event: RecorderEvent,
    queued_at: Instant,
}

#[derive(Clone, Copy)]
struct RecorderEventDispatch {
    queue_depth: usize,
    queue_wait_ms: u64,
}

impl RecorderEventWorker {
    fn start(
        thread_name: String,
        mut apply: impl FnMut(RecorderEvent, RecorderEventDispatch) -> anyhow::Result<()>
        + Send
        + 'static,
        on_failure: impl FnOnce() + Send + 'static,
    ) -> anyhow::Result<Self> {
        let (sender, receiver) = bounded::<QueuedRecorderEvent>(RECORDER_EVENT_QUEUE_CAPACITY);
        let join = std::thread::Builder::new()
            .name(thread_name)
            .spawn(move || {
                let mut on_failure = Some(on_failure);
                while let Ok(queued) = receiver.recv() {
                    let dispatch = RecorderEventDispatch {
                        queue_depth: receiver.len(),
                        queue_wait_ms: u64::try_from(queued.queued_at.elapsed().as_millis())
                            .unwrap_or(u64::MAX),
                    };
                    let event_name = recorder_event_name(&queued.event);
                    let apply_started = Instant::now();
                    if let Err(error) = apply(queued.event, dispatch) {
                        if let Some(on_failure) = on_failure.take() {
                            on_failure();
                        }
                        return Err(error
                            .context("JOURNAL_WRITE_FAILED in recorder event persistence worker"));
                    }
                    let apply_elapsed_ms = apply_started.elapsed().as_millis();
                    if apply_elapsed_ms >= 50 {
                        tracing::warn!(
                            event = event_name,
                            apply_elapsed_ms,
                            queue_wait_ms = dispatch.queue_wait_ms,
                            queue_depth = dispatch.queue_depth,
                            "recorder event persistence was slow"
                        );
                    }
                }
                Ok(())
            })
            .context("JOURNAL_WRITE_FAILED while starting recorder event persistence worker")?;
        Ok(Self { sender, join })
    }

    fn submit(&self, event: RecorderEvent) -> anyhow::Result<()> {
        let queued = QueuedRecorderEvent {
            event,
            queued_at: Instant::now(),
        };
        match self.sender.try_send(queued) {
            Ok(()) => Ok(()),
            Err(crossbeam_channel::TrySendError::Full(_)) => anyhow::bail!(
                "JOURNAL_WRITE_FAILED: recorder event persistence queue reached its bounded capacity of {RECORDER_EVENT_QUEUE_CAPACITY}; capture was stopped instead of blocking"
            ),
            Err(crossbeam_channel::TrySendError::Disconnected(_)) => anyhow::bail!(
                "JOURNAL_WRITE_FAILED: recorder event persistence worker disconnected"
            ),
        }
    }

    fn finish(self) -> anyhow::Result<()> {
        let Self { sender, join } = self;
        drop(sender);
        match join.join() {
            Ok(result) => result,
            Err(_) => {
                anyhow::bail!("JOURNAL_WRITE_FAILED: recorder event persistence worker panicked")
            }
        }
    }
}

fn recorder_event_name(event: &RecorderEvent) -> &'static str {
    match event {
        RecorderEvent::Started { .. } => "started",
        RecorderEvent::SegmentStarted { .. } => "segment_started",
        RecorderEvent::SegmentFinalizing { .. } => "segment_finalizing",
        RecorderEvent::Progress(_) => "progress",
        RecorderEvent::SegmentReady { .. } => "segment_ready",
    }
}

/// 视角标定用的绑定目录上下文（`view_scale_context()` 的返回值，
/// docs/46 §4）。身份字段与 `view_calibrations` 的 scope 三元组对齐。
#[derive(Clone, Debug)]
pub(crate) struct ViewScaleContext {
    pub game_id: String,
    pub catalog_version: String,
    pub recording_profile_id: String,
    pub recording_profile_hash: String,
    pub device_profile_id: String,
    pub device_profile_hash: String,
    pub collector_state: CollectorState,
    pub policy: Option<crate::platform::windows::catalog::ViewScalePolicy>,
}

/// 视角标定 preflight 门的评估结果（C4，docs/45 §6.2、docs/46 §4.2）。
struct ViewScaleGate {
    /// 写入 preflight 证据的事实（重放字节相等：不含任何随时间漂移的量）。
    facts: serde_json::Value,
    /// Some = first-party 车道必须拦截：(failure_code, message, action)。
    block: Option<(&'static str, String, String)>,
    /// 门通过时写入 session_view_calibrations 的 (calibration_id, sha256)。
    reference: Option<(String, String)>,
}

/// 抽检时效（分钟）。工程初值：docs/45 §4.3 的"每次开录前抽检"落成
/// "开录前 30 分钟内做过一次 pass 抽检"；随阈值定版一起由研究调整。
const VIEW_SCALE_SPOT_CHECK_MAX_AGE_MINUTES: i64 = 30;
/// 目录政策未给 max_calibration_age_days 时的缺省全量标定有效期（天）。
const VIEW_SCALE_DEFAULT_MAX_AGE_DAYS: u32 = 30;
/// 目录政策未给 min_polling_hz 时的缺省回报率下限（已裁决 ≥500Hz）。
const VIEW_SCALE_DEFAULT_MIN_POLLING_HZ: f64 = 500.0;

fn evaluate_view_scale_gate(
    journal: &SessionJournal,
    app_data_root: &Path,
    policy: Option<&crate::platform::windows::catalog::ViewScalePolicy>,
    game_id: &str,
    recording_profile_hash: &str,
    device_profile_hash: &str,
) -> anyhow::Result<ViewScaleGate> {
    if let Some(policy) = policy
        && !policy.applicable
    {
        return Ok(ViewScaleGate {
            facts: serde_json::json!({ "state": "not_applicable" }),
            block: None,
            reference: None,
        });
    }
    let blocked = |facts: serde_json::Value, code: &'static str, message: String| ViewScaleGate {
        facts,
        block: Some((
            code,
            message,
            "在采集页完成/更新视角标定（全量或抽检）后重试。".to_string(),
        )),
        reference: None,
    };

    let Some(full) = journal.latest_valid_view_calibration(
        game_id,
        recording_profile_hash,
        device_profile_hash,
    )?
    else {
        return Ok(blocked(
            serde_json::json!({ "state": "missing" }),
            "VIEW_SCALE_CALIBRATION_MISSING",
            "当前目录三元组没有有效的视角标定。".to_string(),
        ));
    };

    // 先复核证据字节，再读取任何派生值。旧 schema/旧 125Hz 算法即使
    // journal 曾登记为 pass，也必须 fail-closed 要求重新全量标定。
    let evidence_bytes = match std::fs::read(app_data_root.join(&full.file_path)) {
        Ok(bytes) => bytes,
        Err(_) => {
            return Ok(blocked(
                serde_json::json!({
                    "state": "evidence_missing",
                    "calibrationId": full.calibration_id,
                }),
                "VIEW_SCALE_EVIDENCE_HASH_MISMATCH",
                "标定证据文件缺失或与登记指纹不一致，需要重新对表。".to_string(),
            ));
        }
    };
    let actual_hash = {
        use sha2::Digest as _;
        let mut hasher = sha2::Sha256::new();
        hasher.update(&evidence_bytes);
        format!("sha256:{:x}", hasher.finalize())
    };
    if actual_hash != full.file_sha256 {
        return Ok(blocked(
            serde_json::json!({
                "state": "evidence_hash_mismatch",
                "calibrationId": full.calibration_id,
            }),
            "VIEW_SCALE_EVIDENCE_HASH_MISMATCH",
            "标定证据文件缺失或与登记指纹不一致，需要重新对表。".to_string(),
        ));
    }
    let evidence = std::str::from_utf8(&evidence_bytes)
        .ok()
        .and_then(|text| CalibrationEvidence::from_json(text).ok());
    let Some(evidence) = evidence.filter(|evidence| evidence.calibration_id == full.calibration_id)
    else {
        return Ok(blocked(
            serde_json::json!({
                "state": "evidence_version_mismatch",
                "calibrationId": full.calibration_id,
            }),
            "VIEW_SCALE_CALIBRATION_VERSION_MISMATCH",
            "这份标定来自旧版回报率算法或证据格式，需要重新做一次全量对表。".to_string(),
        ));
    };
    let mut facts = serde_json::json!({
        "state": "calibrated",
        "calibrationId": full.calibration_id,
        "fileSha256": full.file_sha256,
        "verdict": full.verdict,
        "countsPer360Measured": evidence.derived.counts_per_360_measured,
        "deviationPct": evidence.derived.deviation_pct,
        "pollingHzP50": evidence.derived.polling_hz_p50,
        "evidenceSchemaVersion": evidence.schema_version,
        "algorithmVersion": evidence.method.algorithm_version,
        "pollingEstimatorVersion": evidence.method.polling_estimator_version,
        "inputTimestampSemantics": evidence.method.input_timestamp_semantics,
        "createdUtc": full.created_utc,
        "policy": {
            "expectedCountsPer360": policy.and_then(|p| p.counts_per_360),
            "minPollingHz": policy
                .and_then(|p| p.min_polling_hz)
                .unwrap_or(VIEW_SCALE_DEFAULT_MIN_POLLING_HZ),
            "maxCalibrationAgeDays": policy
                .and_then(|p| p.max_calibration_age_days)
                .unwrap_or(VIEW_SCALE_DEFAULT_MAX_AGE_DAYS),
            "configWatchPaths": policy
                .map(|p| p.config_watch_paths.clone())
                .unwrap_or_default(),
        },
    });

    let max_age_days = policy
        .and_then(|p| p.max_calibration_age_days)
        .unwrap_or(VIEW_SCALE_DEFAULT_MAX_AGE_DAYS);
    let now_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0);
    match parse_utc_to_unix_seconds(&full.created_utc) {
        Some(created) if now_unix - created <= i64::from(max_age_days) * 86_400 => {}
        _ => {
            return Ok(blocked(
                facts,
                "VIEW_SCALE_CALIBRATION_STALE",
                format!("全量标定已超过 {max_age_days} 天有效期，需要重新对表。"),
            ));
        }
    }

    let min_polling = policy
        .and_then(|p| p.min_polling_hz)
        .unwrap_or(VIEW_SCALE_DEFAULT_MIN_POLLING_HZ);
    if !evidence
        .derived
        .polling_hz_p50
        .is_some_and(|hz| hz.is_finite() && hz >= min_polling)
    {
        return Ok(blocked(
            facts,
            "VIEW_SCALE_POLLING_BELOW_FLOOR",
            format!(
                "标定时实测鼠标回报率低于政策下限 {min_polling}Hz，请更换游戏鼠标或调高回报率。"
            ),
        ));
    }

    let spot = journal.latest_valid_spot_check(&full.calibration_id)?;
    let qpc_frequency = QpcClock::new()?.frequency();
    let now_qpc = QpcClock::new()?.now()?;
    let fresh_spot = spot.as_ref().is_some_and(|spot| {
        now_qpc.saturating_sub(spot.created_qpc)
            <= VIEW_SCALE_SPOT_CHECK_MAX_AGE_MINUTES * 60 * qpc_frequency
    });
    let Some(spot) = spot.filter(|_| fresh_spot) else {
        return Ok(blocked(
            facts,
            "VIEW_SCALE_SPOT_CHECK_REQUIRED",
            format!(
                "开录前需要一次 {VIEW_SCALE_SPOT_CHECK_MAX_AGE_MINUTES} 分钟内的抽检（单段对表）。"
            ),
        ));
    };
    let spot_evidence_is_current = std::fs::read(app_data_root.join(&spot.file_path))
        .ok()
        .filter(|bytes| {
            use sha2::Digest as _;
            let mut hasher = sha2::Sha256::new();
            hasher.update(bytes);
            format!("sha256:{:x}", hasher.finalize()) == spot.file_sha256
        })
        .and_then(|bytes| {
            std::str::from_utf8(&bytes)
                .ok()
                .and_then(|text| CalibrationEvidence::from_json(text).ok())
        })
        .is_some_and(|spot_evidence| {
            spot_evidence.calibration_id == spot.calibration_id
                && spot_evidence.parent_calibration_id.as_deref()
                    == Some(full.calibration_id.as_str())
        });
    if !spot_evidence_is_current {
        return Ok(blocked(
            facts,
            "VIEW_SCALE_SPOT_CHECK_VERSION_MISMATCH",
            "开录前抽检来自旧版回报率算法或证据已损坏，请重新做一次抽检。".to_string(),
        ));
    }
    if let Some(object) = facts.as_object_mut() {
        object.insert("spotCheckId".into(), spot.calibration_id.clone().into());
        object.insert(
            "spotCheckCreatedUtc".into(),
            spot.created_utc.clone().into(),
        );
    }
    Ok(ViewScaleGate {
        facts,
        block: None,
        reference: Some((full.calibration_id, full.file_sha256)),
    })
}

/// `strftime('%Y-%m-%dT%H:%M:%fZ')` → unix 秒（无外部时间库依赖）。
fn parse_utc_to_unix_seconds(value: &str) -> Option<i64> {
    let year: i64 = value.get(0..4)?.parse().ok()?;
    let month: u32 = value.get(5..7)?.parse().ok()?;
    let day: u32 = value.get(8..10)?.parse().ok()?;
    let hour: i64 = value.get(11..13)?.parse().ok()?;
    let minute: i64 = value.get(14..16)?.parse().ok()?;
    let second: i64 = value.get(17..19)?.parse().ok()?;
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second)
}

/// Howard Hinnant days_from_civil：(年, 月, 日) → epoch 天数。
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let adjusted_year = if month <= 2 { year - 1 } else { year };
    let era = adjusted_year.div_euclid(400);
    let year_of_era = adjusted_year - era * 400;
    let month_prime = i64::from(if month > 2 { month - 3 } else { month + 9 });
    let day_of_year = (153 * month_prime + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

struct Inner {
    snapshot: CollectorSnapshot,
    bound: Option<BoundTarget>,
    stop_tx: Option<Sender<RecordingStopSignal>>,
    /// 录制期设置文件监视（C4）；Drop 即停线程，随 run 生命周期同生死。
    settings_watch: Option<crate::services::settings_watch::SettingsWatch>,
    input_control: Option<RawInputControl>,
    input_status: Option<RawInputStatus>,
    worker: Option<JoinHandle<()>>,
    started_at: Option<Instant>,
    last_request_id: Option<String>,
    session_id: Option<String>,
    next_run_seq: u32,
    next_session_frame: u64,
    finish_as_completed: bool,
    storage_warning_emitted: bool,
    rebind_required: Option<RebindIdentity>,
    start_reservation: Option<String>,
    catalog_invalidation: Option<RecordingStopReason>,
}

struct Shared {
    app: AppHandle,
    overlay: Option<Arc<OverlayWindowController>>,
    journal: Arc<SessionJournal>,
    diagnostics: Arc<DiagnosticService>,
    sentry: Arc<SentryReporter>,
    heartbeats: Arc<HeartbeatStore>,
    markers: Arc<MarkerStore>,
    data_model: Arc<DataModelService>,
    /// 启动期产生、等首个 `collector_get_snapshot` 命令代发的通知（docs/65
    /// J-24）。setup 里直接 emit 会与 WebView 挂监听赛跑而丢失；前端先挂监听再
    /// 发首次 snapshot，命令到来即证明监听已就位。
    startup_notifications: Mutex<Vec<CollectorFault>>,
    inner: Mutex<Inner>,
}

struct PendingSentryRecordingFailureReport {
    session_id: String,
    run_id: String,
    fault_code: String,
    details_id: String,
    runtime: RuntimeDiagnosticSnapshot,
    _reservation: SentryReportReservation,
}

struct CompletedSentryRecordingFailureReport {
    sentry_event_id: String,
    export_id: Option<String>,
    attachment_status: DiagnosticAttachmentStatus,
    flushed: bool,
    warning: Option<String>,
}

impl Shared {
    fn update(self: &Arc<Self>, mutate: impl FnOnce(&mut Inner)) -> CollectorSnapshot {
        let (snapshot, pending_report) = {
            let mut inner = self.inner.lock();
            let previous_state = inner.snapshot.state;
            let recording_run_was_active = inner.started_at.is_some();
            mutate(&mut inner);
            // 每次变更后统一重派生车道分类（docs/65 J-26）：单点强制，任何一处
            // 改绑定/换目录的 mutate 都不可能忘了同步这两项。
            inner.snapshot.derive_lane_classification();
            inner.snapshot.revision = inner.snapshot.revision.saturating_add(1);
            let pending_report = should_report_recording_failure(
                previous_state,
                inner.snapshot.state,
                recording_run_was_active,
            )
            .then(|| {
                let capture = inner.snapshot.capture.as_ref()?;
                let fault = inner.snapshot.fault.as_ref()?;
                let session_id = capture.session_id.clone();
                let run_id = capture.run_id.clone();
                let fault_code = fault.code.clone();
                let details_id = fault.details_id.clone();
                let runtime = runtime_diagnostic_snapshot(&inner.snapshot, Some(&session_id));
                self.prepare_sentry_recording_failure_report(
                    session_id, run_id, fault_code, details_id, runtime,
                )
            })
            .flatten();
            (inner.snapshot.clone(), pending_report)
        };
        self.publish_snapshot(snapshot.clone());
        if let Some(report) = pending_report {
            self.spawn_sentry_recording_failure_report(report);
        }
        snapshot
    }

    fn publish_snapshot(&self, snapshot: CollectorSnapshot) {
        if let Err(error) = self.app.emit("collector://snapshot", &snapshot) {
            tracing::warn!(error = %error, "failed to emit collector snapshot");
        }
        if let Some(overlay) = &self.overlay {
            overlay.update_collector_snapshot(snapshot);
        }
    }

    fn snapshot(&self) -> CollectorSnapshot {
        self.inner.lock().snapshot.clone()
    }

    fn current_correlation(&self) -> DiagnosticCorrelation {
        self.inner
            .lock()
            .snapshot
            .capture
            .as_ref()
            .map_or_else(DiagnosticCorrelation::default, |capture| {
                DiagnosticCorrelation::run(&capture.session_id, &capture.run_id)
            })
    }

    fn fault(&self, spec: FaultSpec, diagnostic: Option<&str>) -> CollectorFault {
        self.diagnostics.create_fault(
            spec.code,
            spec.message,
            spec.action,
            spec.retryable,
            self.current_correlation(),
            diagnostic,
        )
    }

    fn run_fault(
        &self,
        session_id: &str,
        run_id: &str,
        spec: FaultSpec,
        diagnostic: Option<&str>,
    ) -> CollectorFault {
        self.diagnostics.create_fault(
            spec.code,
            spec.message,
            spec.action,
            spec.retryable,
            DiagnosticCorrelation::run(session_id, run_id),
            diagnostic,
        )
    }

    fn export_diagnostics(
        &self,
        requested_session_id: Option<&str>,
    ) -> anyhow::Result<DiagnosticExportResult> {
        let snapshot = self.snapshot();
        let runtime = runtime_diagnostic_snapshot(&snapshot, requested_session_id);
        self.export_diagnostics_with_runtime(requested_session_id, &runtime)
    }

    fn export_diagnostics_with_runtime(
        &self,
        requested_session_id: Option<&str>,
        runtime: &RuntimeDiagnosticSnapshot,
    ) -> anyhow::Result<DiagnosticExportResult> {
        let session_id = requested_session_id.or_else(|| {
            runtime
                .capture
                .as_ref()
                .map(|capture| capture.session_id.as_str())
        });
        let journal = self.journal.diagnostic_snapshot(session_id)?;
        self.diagnostics.export(runtime, &journal, session_id)
    }

    fn prepare_sentry_recording_failure_report(
        &self,
        session_id: String,
        run_id: String,
        fault_code: String,
        details_id: String,
        runtime: RuntimeDiagnosticSnapshot,
    ) -> Option<PendingSentryRecordingFailureReport> {
        self.sentry
            .reserve_report()
            .map(|reservation| PendingSentryRecordingFailureReport {
                session_id,
                run_id,
                fault_code,
                details_id,
                runtime,
                _reservation: reservation,
            })
    }

    fn spawn_sentry_recording_failure_report(
        self: &Arc<Self>,
        pending: PendingSentryRecordingFailureReport,
    ) {
        let shared = Arc::clone(self);
        let thread_name = format!("dm-sentry-failure-{}", pending.run_id);
        if let Err(error) = std::thread::Builder::new().name(thread_name).spawn(move || {
            let PendingSentryRecordingFailureReport {
                session_id,
                run_id,
                fault_code,
                details_id,
                runtime,
                _reservation,
            } = pending;
            let correlation = DiagnosticCorrelation::run(&session_id, &run_id);
            let report = (|| -> anyhow::Result<CompletedSentryRecordingFailureReport> {
                let (diagnostic, attachment, attachment_status, omission_error) =
                    match shared.export_diagnostics_with_runtime(Some(&session_id), &runtime) {
                        Ok(diagnostic) => match shared.diagnostics.read_export_attachment(
                            &diagnostic,
                            MAX_DIAGNOSTIC_ATTACHMENT_BYTES,
                        ) {
                            Ok(Some(attachment)) => (
                                Some(diagnostic),
                                Some(attachment),
                                DiagnosticAttachmentStatus::Attached,
                                None,
                            ),
                            Ok(None) => (
                                Some(diagnostic),
                                None,
                                DiagnosticAttachmentStatus::OmittedOversize,
                                Some("diagnostic export exceeded the attachment limit".into()),
                            ),
                            Err(error) => (
                                Some(diagnostic),
                                None,
                                DiagnosticAttachmentStatus::OmittedValidationFailed,
                                Some(format!("diagnostic attachment validation failed: {error:#}")),
                            ),
                        },
                        Err(error) => (
                            None,
                            None,
                            DiagnosticAttachmentStatus::OmittedExportFailed,
                            Some(format!("diagnostic export failed: {error:#}")),
                        ),
                    };
                if let Some(error) = &omission_error {
                    tracing::warn!(fault_code, error, "sending Sentry failure event without diagnostic attachment");
                }
                let receipt = shared.sentry.report_recording_failure(SentryRecordingFailureEvent {
                    fault_code: &fault_code,
                    session_id: &session_id,
                    run_id: &run_id,
                    details_id: &details_id,
                    diagnostic: diagnostic.as_ref(),
                    attachment,
                    attachment_status,
                })?;
                let report_warning = if receipt.flushed {
                    omission_error
                } else {
                    Some(match omission_error {
                        Some(error) => format!(
                            "{error}; Sentry transport did not drain before the best-effort timeout"
                        ),
                        None => {
                            "Sentry transport did not drain before the best-effort timeout".into()
                        }
                    })
                };
                Ok(CompletedSentryRecordingFailureReport {
                    sentry_event_id: receipt.event_id,
                    export_id: diagnostic.map(|value| value.export_id),
                    attachment_status,
                    flushed: receipt.flushed,
                    warning: report_warning,
                })
            })();
            match report {
                Ok(report) => {
                    let diagnostic_level = if report.flushed { "info" } else { "warning" };
                    let _ = shared.diagnostics.record(
                        diagnostic_level,
                        None,
                        correlation,
                        DiagnosticPayload::SentryRecordingFailureReport {
                            fault_code,
                            sentry_event_id: Some(report.sentry_event_id),
                            export_id: report.export_id,
                            outcome: if report.flushed {
                                report.attachment_status.outcome().into()
                            } else {
                                "transport_flush_timed_out".into()
                            },
                            error: report.warning,
                        },
                        true,
                    );
                }
                Err(error) => {
                    tracing::warn!(error = %error, "Sentry recording-failure report did not drain; local capture failure remains authoritative");
                    let _ = shared.diagnostics.record(
                        "warning",
                        None,
                        correlation,
                        DiagnosticPayload::SentryRecordingFailureReport {
                            fault_code,
                            sentry_event_id: None,
                            export_id: None,
                            outcome: "failed".into(),
                            error: Some(format!("{error:#}")),
                        },
                        true,
                    );
                }
            }
        }) {
            tracing::warn!(error = %error, "failed to start Sentry recording-failure reporter; local capture failure remains authoritative");
        }
    }
}

fn runtime_diagnostic_snapshot(
    snapshot: &CollectorSnapshot,
    requested_session_id: Option<&str>,
) -> RuntimeDiagnosticSnapshot {
    let runtime_matches_scope = requested_session_id.is_none_or(|session_id| {
        snapshot
            .capture
            .as_ref()
            .is_some_and(|capture| capture.session_id == session_id)
    });
    RuntimeDiagnosticSnapshot {
        collector_state: if runtime_matches_scope {
            snapshot.state
        } else {
            CollectorState::Idle
        },
        target: if runtime_matches_scope {
            snapshot
                .bound_target
                .as_ref()
                .map(|target| RuntimeTargetDiagnostic {
                    game_id: target.game_id.clone(),
                    executable_name: target.executable_name.clone(),
                    client_width: target.client_width,
                    client_height: target.client_height,
                    dpi: target.dpi,
                    recording_profile_id: target.recording_profile_id.clone(),
                    mapping_id: target.mapping_id.clone(),
                    mapping_version: target.mapping_version.clone(),
                })
        } else {
            None
        },
        capture: if runtime_matches_scope {
            snapshot
                .capture
                .as_ref()
                .map(|capture| RuntimeCaptureDiagnostic {
                    session_id: capture.session_id.clone(),
                    run_id: capture.run_id.clone(),
                    run_seq: capture.run_seq,
                    frame_count: capture.frame_count,
                    wgc_callback_count: capture.wgc_callback_count,
                    active_segment: capture.active_segment,
                    ready_segments: capture.ready_segments,
                    duplicate_frames: capture.duplicate_frames,
                    source_superseded: capture.source_superseded,
                    capture_queue_depth: capture.capture_queue_depth,
                    capture_queue_drops: capture.capture_queue_drops,
                    input_event_count: capture.input_event_count,
                    input_queue_depth: capture.input_queue_depth,
                    input_queue_overflows: capture.input_queue_overflows,
                    elapsed_ms: capture.elapsed_ms,
                })
        } else {
            None
        },
        storage: if runtime_matches_scope {
            snapshot.storage.clone()
        } else {
            StorageStatus::default()
        },
    }
}

struct StartReservation {
    shared: Arc<Shared>,
    request_id: String,
    fallback_state: CollectorState,
    armed: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReservationCheck {
    Available,
    Replay,
}

fn check_start_reservation(
    reservation: &Option<String>,
    request_id: &str,
) -> anyhow::Result<ReservationCheck> {
    match reservation.as_deref() {
        None => Ok(ReservationCheck::Available),
        Some(existing) if existing == request_id => Ok(ReservationCheck::Replay),
        Some(_) => anyhow::bail!("another start preflight is already in progress"),
    }
}

fn release_start_reservation(
    reservation: &mut Option<String>,
    state: &mut CollectorState,
    request_id: &str,
    fallback_state: CollectorState,
) {
    if reservation.as_deref() == Some(request_id) {
        *reservation = None;
        if *state == CollectorState::Preflighting {
            *state = fallback_state;
        }
    }
}

fn preflight_failure_state(start_kind: StartKind) -> CollectorState {
    if start_kind == StartKind::Resume {
        CollectorState::Paused
    } else {
        CollectorState::Faulted
    }
}

impl StartReservation {
    fn new(shared: Arc<Shared>, request_id: &str, fallback_state: CollectorState) -> Self {
        Self {
            shared,
            request_id: request_id.into(),
            fallback_state,
            armed: true,
        }
    }

    fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for StartReservation {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let owns_reservation =
            self.shared.inner.lock().start_reservation.as_deref() == Some(self.request_id.as_str());
        if owns_reservation {
            self.shared.update(|inner| {
                release_start_reservation(
                    &mut inner.start_reservation,
                    &mut inner.snapshot.state,
                    &self.request_id,
                    self.fallback_state,
                );
            });
        }
    }
}

#[derive(Clone, Copy)]
struct FaultSpec {
    code: &'static str,
    message: &'static str,
    action: &'static str,
    retryable: bool,
}

struct PreflightAttempt {
    shared: Arc<Shared>,
    evidence: PreflightStatus,
    failure_state: CollectorState,
    armed: bool,
}

impl PreflightAttempt {
    fn new(shared: Arc<Shared>, evidence: PreflightStatus, failure_state: CollectorState) -> Self {
        Self {
            shared,
            evidence,
            failure_state,
            armed: true,
        }
    }

    fn evidence(&self) -> &PreflightStatus {
        &self.evidence
    }

    fn append_check(&mut self, check_id: &str, facts: serde_json::Value) -> anyhow::Result<()> {
        // 通过词由这次预检的契约决定（`preflight::pass_result_for`），不由调用方
        // 每次重报一遍——证据得说得清自己是哪一代走完的。
        let pass_result = preflight::pass_result_for(self.evidence.session_contract);
        preflight::append_check(&mut self.evidence, check_id, pass_result, facts)?;
        self.shared.journal.update_preflight(&self.evidence)?;
        self.shared.update(|inner| {
            inner.snapshot.preflight = Some(self.evidence.clone());
        });
        Ok(())
    }

    /// 终态词由预检自己的契约决定（[`preflight::pass`]），不由调用方选一个字符串。
    fn completed_evidence(
        &self,
        session_id: &str,
        run_id: &str,
    ) -> anyhow::Result<PreflightStatus> {
        let mut completed = self.evidence.clone();
        preflight::link_run(&mut completed, session_id, run_id)?;
        preflight::pass(&mut completed)?;
        Ok(completed)
    }

    fn commit(mut self, completed: PreflightStatus) {
        self.evidence = completed;
        self.shared.update(|inner| {
            inner.snapshot.preflight = Some(self.evidence.clone());
        });
        self.armed = false;
    }

    #[allow(clippy::too_many_arguments)]
    fn fail(
        &mut self,
        phase: &str,
        result: &str,
        failure_code: &str,
        message: &str,
        action: &str,
        retryable: bool,
        diagnostic: &str,
        mut facts: serde_json::Value,
    ) {
        if let Some(object) = facts.as_object_mut() {
            object.insert(
                "failureCode".into(),
                serde_json::Value::String(failure_code.into()),
            );
        } else {
            facts = serde_json::json!({
                "failureCode": failure_code,
                "observed": facts,
            });
        }
        let mut failed = self.evidence.clone();
        if failed.result == "running"
            && let Err(error) = preflight::append_check(&mut failed, phase, result, facts)
        {
            tracing::error!(error = %error, "failed to append terminal preflight check");
        }
        let details_id = preflight::details_id(&failed, phase, failure_code);
        let terminalized = preflight::fail(&mut failed, phase, result, failure_code, &details_id)
            .and_then(|()| self.shared.journal.update_preflight(&failed));
        if let Err(error) = &terminalized {
            tracing::error!(error = %error, "failed to persist terminal preflight evidence");
        }
        let fault = self.shared.diagnostics.create_fault_with_details_id(
            &details_id,
            failure_code,
            message,
            action,
            retryable,
            DiagnosticCorrelation::default(),
            Some(diagnostic),
        );
        let display_evidence = failed.clone();
        if terminalized.is_ok() {
            self.evidence = failed;
        }
        self.shared.update(|inner| {
            inner.snapshot.state = self.failure_state;
            inner.snapshot.preflight = Some(display_evidence);
            inner.snapshot.fault = Some(fault);
            inner.last_request_id = Some(self.evidence.request_id.clone());
        });
        self.armed = terminalized.is_err();
    }
}

impl Drop for PreflightAttempt {
    fn drop(&mut self) {
        if self.armed && self.evidence.result == "running" {
            self.fail(
                "internal",
                "failed",
                "PREFLIGHT_ABORTED",
                "启动前检查被中断，未开始录制。",
                "检查诊断详情后重试；不会生成 session/run 目录。",
                true,
                "preflight scope exited before terminal evidence was persisted",
                serde_json::json!({ "reason": "scope_exited_before_terminal_persist" }),
            );
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RebindIdentity {
    game_id: String,
    /// 可空（docs/55 §3.1）。`Option` 的相等语义正是这里要的：测到过指纹的
    /// 重绑必须测到同一个，没测到过的重绑也必须仍然没测到——「本来测不到，
    /// 重绑后忽然测到了」同样是身份变化，不该悄悄放行。
    executable_sha256: Option<String>,
    session_contract: SessionContract,
    catalog_id: String,
    catalog_version: String,
    catalog_hash: String,
    device_profile_id: String,
    device_profile_version: String,
    device_profile_hash: String,
    recording_profile_id: String,
    recording_profile_version: String,
    recording_profile_hash: String,
    game_profile_version: String,
    game_profile_hash: String,
    mapping_id: String,
    mapping_version: String,
    mapping_hash: String,
    target_id: String,
}

impl RebindIdentity {
    fn from_target(target: &BoundTarget) -> Self {
        Self {
            game_id: target.summary.game_id.clone(),
            executable_sha256: target.executable_sha256.clone(),
            session_contract: target.summary.session_contract,
            catalog_id: target.summary.catalog_id.clone(),
            catalog_version: target.summary.catalog_version.clone(),
            catalog_hash: target.summary.catalog_hash.clone(),
            device_profile_id: target.summary.device_profile_id.clone(),
            device_profile_version: target.summary.device_profile_version.clone(),
            device_profile_hash: target.summary.device_profile_hash.clone(),
            recording_profile_id: target.summary.recording_profile_id.clone(),
            recording_profile_version: target.summary.recording_profile_version.clone(),
            recording_profile_hash: target.summary.recording_profile_hash.clone(),
            game_profile_version: target.summary.game_profile_version.clone(),
            game_profile_hash: target.summary.game_profile_hash.clone(),
            mapping_id: target.summary.mapping_id.clone(),
            mapping_version: target.summary.mapping_version.clone(),
            mapping_hash: target.mapping.content_hash().into(),
            target_id: target.summary.target_id.clone(),
        }
    }
}

enum DiskNotice {
    Warning(Option<u64>),
    Stop(Option<u64>),
    Unavailable,
}

#[derive(Default)]
struct ControlOperationGate {
    operation: Mutex<()>,
    safe_quit_requested: AtomicBool,
}

impl ControlOperationGate {
    fn lock(&self) -> MutexGuard<'_, ()> {
        self.operation.lock()
    }

    fn try_lock(&self) -> Option<MutexGuard<'_, ()>> {
        self.operation.try_lock()
    }

    fn lock_for_start(&self) -> anyhow::Result<MutexGuard<'_, ()>> {
        let guard = self.operation.lock();
        ensure!(
            !self.safe_quit_requested.load(Ordering::Acquire),
            "APPLICATION_SHUTTING_DOWN: recording cannot start during safe quit"
        );
        Ok(guard)
    }

    fn begin_safe_quit(&self) -> MutexGuard<'_, ()> {
        let guard = self.operation.lock();
        self.safe_quit_requested.store(true, Ordering::Release);
        guard
    }

    fn cancel_safe_quit(&self) {
        let _guard = self.operation.lock();
        self.safe_quit_requested.store(false, Ordering::Release);
    }
}

pub struct SessionOrchestrator {
    tracker: Arc<WindowTracker>,
    shared: Arc<Shared>,
    control_operation: ControlOperationGate,
    overlay: Option<Arc<OverlayWindowController>>,
    app_data_root: PathBuf,
    artifact_root: PathBuf,
    output_root: PathBuf,
    capture_storage: Mutex<CaptureStorageManager>,
    pending_catalog: Mutex<Option<crate::platform::windows::catalog::CatalogStore>>,
    active_catalog_generation: AtomicU64,
    cached_catalog_generation: AtomicU64,
    // 行为遥测直报点（docs/23 §4.3，B 类 preflight 事件）。启动后由 AppState
    // 注入；缺席（wiring 失败）时所有直报静默跳过——旁路纪律。
    client_events: Mutex<Option<Arc<crate::services::client_events::ClientEventRecorder>>>,
}

impl SessionOrchestrator {
    #[cfg(feature = "depth-lab")]
    pub(crate) fn depth_catalog_selection(
        &self,
        game_id: &str,
    ) -> anyhow::Result<crate::platform::windows::catalog::CatalogSelection> {
        self.tracker.catalog_selection(game_id)
    }

    #[cfg(feature = "depth-lab")]
    pub(crate) fn check_depth_performance_certification(
        &self,
        game_id: &str,
    ) -> anyhow::Result<crate::services::depth_lab::PerfCertificationGateStatus> {
        let _control_operation = self.control_operation.lock_for_start()?;
        self.reap_finished_worker();
        let target = {
            let inner = self.shared.inner.lock();
            ensure!(
                matches!(
                    inner.snapshot.state,
                    CollectorState::Bound
                        | CollectorState::Paused
                        | CollectorState::Completed
                        | CollectorState::Faulted
                ),
                "请先绑定游戏窗口，并在未录制时检查性能认证"
            );
            inner.bound.clone().context("当前没有已绑定的游戏窗口")?
        };
        ensure!(
            target.summary.game_id == game_id,
            "当前绑定游戏已变化，请重新选择后再检查性能认证"
        );
        // 这里只展示当前绑定配置与本机硬件是否命中签名认证，不会开始录制。
        // Fallout 4 等独占全屏游戏在切回控制台时可能销毁顶层 HWND；若在这里
        // 重验窗口，操作员将永远只能看到“检查失败”。正式 start 仍会执行严格的
        // window_revalidate，因此去掉此处的活窗口要求不会放宽录制门禁。
        let hardware = probe_hardware_path(
            &target.capture_profile,
            self.artifact_root
                .join(HARDWARE_PREFLIGHT_SCRATCH_DIRECTORY),
        )
        .context("读取当前 GPU 与硬件采集能力失败")?;
        crate::services::depth_lab::check_performance_certification(
            crate::services::depth_lab::DepthPerformanceCertificationContext {
                catalog_selection: &target.catalog_selection,
                hardware_evidence: Some(&hardware),
                game_id: &target.summary.game_id,
                executable_sha256: target.executable_sha256.as_deref(),
                capture_fps_num: target.capture_profile.fps_num,
                capture_fps_den: target.capture_profile.fps_den,
                gpu_adapter_description: &hardware.adapter_description,
                gpu_driver_version: hardware.adapter_driver_version.as_deref(),
                gpu_adapter_luid_high: hardware.adapter_luid_high,
                gpu_adapter_luid_low: hardware.adapter_luid_low,
            },
        )
        .map_err(anyhow::Error::msg)
    }

    pub fn new(app: AppHandle) -> anyhow::Result<Self> {
        let app_data_root = app
            .path()
            .app_local_data_dir()
            .context("DataMiner app-local data directory is unavailable")?;
        let diagnostics = Arc::new(DiagnosticService::open(&app_data_root)?);
        let capture_storage = CaptureStorageManager::load(&app_data_root)?;
        let artifact_root = capture_storage.layout().artifact_root.clone();
        let output_root = capture_storage.layout().output_root.clone();
        let sentry = match SentryReporter::for_dataminer(env!("CARGO_PKG_VERSION")) {
            Ok(reporter) => Arc::new(reporter),
            Err(error) => {
                tracing::error!(error = %error, "Sentry recording-failure reporting is disabled; local diagnostics remain available");
                Arc::new(SentryReporter::disabled())
            }
        };
        let journal = Arc::new(SessionJournal::open(app_data_root.join("journal.sqlite3"))?);
        let recovered_deletions =
            session_history::recover_pending_deletions(&journal, &artifact_root, &output_root)?;
        let recovery =
            recovery::recover_with_roots(&journal, &app_data_root, &artifact_root, &output_root)?;
        let heartbeats = Arc::new(HeartbeatStore::new(output_root.clone()));
        let markers = Arc::new(MarkerStore::new(output_root.clone()));
        diagnostics.record(
            "info",
            None,
            DiagnosticCorrelation::default(),
            DiagnosticPayload::AppStarted {
                client_version: env!("CARGO_PKG_VERSION").into(),
                journal_schema_version: u32::try_from(JOURNAL_SCHEMA_VERSION)
                    .context("journal schema version exceeds u32")?,
            },
            true,
        )?;
        let recovery_severity = recovery.diagnostic_severity();
        diagnostics.record(
            recovery_severity,
            None,
            DiagnosticCorrelation::default(),
            DiagnosticPayload::RecoveryCompleted {
                recovery_id: recovery.recovery_id.clone(),
                scanned_sessions: recovery.scanned_sessions,
                verified_ready_segments: recovery.verified_ready_segments,
                quarantined_items: recovery.quarantined_items,
                corrupt_segments: recovery.corrupt_segments,
                orphan_items: recovery.orphan_items,
                verified_bytes: recovery.verified_bytes,
                verification_ms: recovery.verification_ms,
            },
            true,
        )?;
        if recovered_deletions > 0 {
            tracing::info!(
                recovered_deletions,
                "completed pending session deletions before artifact recovery"
            );
        }
        if recovery_severity == "error" {
            tracing::error!(
                recovery_id = %recovery.recovery_id,
                corrupt_segments = recovery.corrupt_segments,
                quarantined_items = recovery.quarantined_items,
                orphan_items = recovery.orphan_items,
                "startup recovery completed with error findings"
            );
        } else if recovery_severity == "warning" {
            tracing::warn!(
                recovery_id = %recovery.recovery_id,
                corrupt_segments = recovery.corrupt_segments,
                quarantined_items = recovery.quarantined_items,
                orphan_items = recovery.orphan_items,
                "startup recovery completed with retained diagnostics"
            );
        } else {
            tracing::info!(
                recovery_id = %recovery.recovery_id,
                verified_ready_segments = recovery.verified_ready_segments,
                "startup recovery completed"
            );
        }
        let overlay = match OverlayWindowController::new(app.clone(), Arc::clone(&diagnostics)) {
            Ok(overlay) => Some(overlay),
            Err(error) => {
                tracing::warn!(error = %error, "Overlay initialization failed; collector remains available");
                None
            }
        };
        let tracker = Arc::new(WindowTracker::new()?);
        let data_model = DataModelService::start(
            app.clone(),
            Arc::clone(&journal),
            &artifact_root,
            &output_root,
        )?;
        let mut snapshot = CollectorSnapshot {
            catalog: tracker.catalog_status(),
            ..CollectorSnapshot::default()
        };
        // 车道分类从第一份快照起就是派生事实，而不是 Default 的常量（docs/65 J-26）。
        snapshot.derive_lane_classification();
        Ok(Self {
            tracker,
            shared: Arc::new(Shared {
                app,
                overlay: overlay.clone(),
                journal,
                diagnostics,
                sentry,
                heartbeats,
                markers,
                data_model,
                startup_notifications: Mutex::new(Vec::new()),
                inner: Mutex::new(Inner {
                    snapshot,
                    bound: None,
                    stop_tx: None,
                    settings_watch: None,
                    input_control: None,
                    input_status: None,
                    worker: None,
                    started_at: None,
                    last_request_id: None,
                    session_id: None,
                    next_run_seq: 0,
                    next_session_frame: 0,
                    finish_as_completed: false,
                    storage_warning_emitted: false,
                    rebind_required: None,
                    start_reservation: None,
                    catalog_invalidation: None,
                }),
            }),
            control_operation: ControlOperationGate::default(),
            overlay,
            app_data_root,
            artifact_root,
            output_root,
            capture_storage: Mutex::new(capture_storage),
            pending_catalog: Mutex::new(None),
            active_catalog_generation: AtomicU64::new(0),
            cached_catalog_generation: AtomicU64::new(0),
            client_events: Mutex::new(None),
        })
    }

    pub fn set_client_event_recorder(
        &self,
        recorder: Arc<crate::services::client_events::ClientEventRecorder>,
    ) {
        self.client_events.lock().replace(recorder);
    }

    pub fn client_event_recorder(
        &self,
    ) -> Option<Arc<crate::services::client_events::ClientEventRecorder>> {
        self.client_events.lock().clone()
    }

    /// preflight 结果直报（docs/23 §3.1 B 类）。details 只放白名单字段；
    /// recorder 缺席或入队失败都静默——遥测绝不影响录制。
    fn report_preflight_event(&self, kind: &str, severity: &str, details: serde_json::Value) {
        if let Some(recorder) = self.client_event_recorder() {
            recorder.record(kind, severity, details);
        }
    }

    pub fn snapshot(&self) -> CollectorSnapshot {
        self.reap_finished_worker();
        self.activate_pending_catalog_if_safe();
        self.shared.snapshot()
    }

    /// 只由前端的 `collector_get_snapshot` 命令调用（docs/65 J-24）。排空是一次性
    /// 的，所以触发面必须收窄到「确证前端在轮询」的那一条路径——`snapshot()` 还被
    /// 托盘事件与诊断导出调用，若在那里排空，通知会在 WebView 挂上监听之前发进
    /// 空场并永久丢失。锁先释放再 emit：parking_lot 不可重入。
    pub fn deliver_startup_notifications(&self) {
        let pending: Vec<CollectorFault> = {
            let mut queue = self.shared.startup_notifications.lock();
            queue.drain(..).collect()
        };
        for notification in &pending {
            emit_collector_notification(&self.shared.app, notification);
        }
    }

    pub(crate) fn upload_journal(&self) -> Arc<SessionJournal> {
        Arc::clone(&self.shared.journal)
    }

    pub(crate) fn raw_output_root(&self) -> PathBuf {
        self.output_root.clone()
    }

    pub(crate) fn app_data_root(&self) -> PathBuf {
        self.app_data_root.clone()
    }

    /// uploader worker 每轮推一次上传面（docs/65 J-29/J-30）。无变化不发布，
    /// 避免 3 秒一跳的空转把快照 revision 刷出噪声。
    pub(crate) fn set_upload_status(&self, status: UploadStatus) {
        if self.shared.inner.lock().snapshot.upload == status {
            return;
        }
        self.shared.update(|inner| inner.snapshot.upload = status);
    }

    /// 视角标定所需的绑定目录上下文（docs/46 §4）。None = 当前未绑定。
    /// 只克隆身份字段与政策块，不把持有 VerifiedExecutable 的 BoundTarget
    /// 泄出编排器。
    #[must_use]
    pub(crate) fn view_scale_context(&self) -> Option<ViewScaleContext> {
        let inner = self.shared.inner.lock();
        let bound = inner.bound.as_ref()?;
        let policy = bound
            .catalog_selection
            .recording_profile()
            .ok()
            .and_then(|profile| profile.view_scale);
        let summary = &bound.summary;
        Some(ViewScaleContext {
            game_id: summary.game_id.clone(),
            catalog_version: summary.catalog_version.clone(),
            recording_profile_id: summary.recording_profile_id.clone(),
            recording_profile_hash: summary.recording_profile_hash.clone(),
            device_profile_id: summary.device_profile_id.clone(),
            device_profile_hash: summary.device_profile_hash.clone(),
            collector_state: inner.snapshot.state,
            policy,
        })
    }

    pub fn capture_storage_status(&self) -> anyhow::Result<CaptureStorageStatus> {
        self.reap_finished_worker();
        let can_change = self.capture_storage_can_change()?;
        Ok(self.capture_storage.lock().status(can_change))
    }

    pub fn choose_capture_storage_location(&self) -> anyhow::Result<CaptureStorageStatus> {
        let _control_operation = self.control_operation.lock();
        self.reap_finished_worker();
        ensure!(
            self.capture_storage_can_change()?,
            "CAPTURE_STORAGE_CHANGE_BLOCKED: stop capture and delete all local sessions first"
        );
        let window = self
            .shared
            .app
            .get_webview_window("main")
            .context("main window is unavailable")?;
        if let Some(selected) = folder_picker::choose_folder(&window)? {
            self.capture_storage.lock().configure_parent(&selected)?;
        }
        self.capture_storage_status()
    }

    fn capture_storage_can_change(&self) -> anyhow::Result<bool> {
        let state = self.shared.inner.lock().snapshot.state;
        let inactive = !matches!(
            state,
            CollectorState::Preflighting
                | CollectorState::Starting
                | CollectorState::Recording
                | CollectorState::Pausing
                | CollectorState::Paused
                | CollectorState::Stopping
        );
        Ok(inactive && self.shared.journal.list_sessions()?.is_empty())
    }

    /// 激活一份已重新验签的下发 catalog。
    ///
    /// 正在 preflight/录制/暂停/停止时不切换，调用方稍后重试；安全窗口内切换会作废
    /// 旧候选与旧绑定，避免用新授权门启动一份由旧 catalog 选出的目标。
    #[cfg(not(feature = "local-catalog-only"))]
    pub(crate) fn try_activate_catalog(
        &self,
        catalog: crate::platform::windows::catalog::CatalogStore,
    ) -> anyhow::Result<bool> {
        let _control_operation = self.control_operation.lock();
        let incoming_identity = catalog.status().active_catalogs;
        let active_identity = self.tracker.catalog_status().active_catalogs;
        ensure_catalog_set_does_not_rollback(&active_identity, &incoming_identity)?;
        let pending_matches = self
            .pending_catalog
            .lock()
            .as_ref()
            .is_some_and(|pending| pending.status().active_catalogs == incoming_identity);
        if active_identity == incoming_identity && !pending_matches {
            return Ok(true);
        }
        if pending_matches {
            if catalog_activation_is_locked(&self.shared.inner.lock()) {
                return Ok(false);
            }
            let pending = self
                .pending_catalog
                .lock()
                .take()
                .expect("matching pending catalog disappeared");
            let generation = self.cached_catalog_generation.load(Ordering::Acquire);
            self.activate_catalog(pending, generation);
            return Ok(true);
        }
        let cached_generation = self
            .cached_catalog_generation
            .fetch_add(1, Ordering::AcqRel)
            .saturating_add(1);
        // ⚠ 运维后果：**切换目录 current 会掐断正在录制的那一场。**
        //
        // 下面这三行不只是"收紧了就作废"。`preserves_selection` 比的是绑定时选中的那份
        // payload，所以只要新目录的 payload hash 变了——换代、重签、改任一 profile——它
        // 就必然为 false，于是走 `invalidate_catalog_session(CatalogRetired)`：该场
        // 判 `Faulted`、`retryable: false`、旧 session 禁止恢复。
        //
        // 已 ready 的段保留（不丢数据），但采集员会看到录制中断且恢复不了，而现场看不出
        // 原因出在服务端切了 current。**所以发布 current 的窗口要选在无人采集时。**
        //
        // 这条对任何一次 current 切换都成立，不限于 issue #855 那次断代。
        let (activation_locked, tightens_active_session) = {
            let inner = self.shared.inner.lock();
            let locked = catalog_activation_is_locked(&inner);
            let tightens = locked
                && inner
                    .bound
                    .as_ref()
                    .is_none_or(|bound| !catalog.preserves_selection(&bound.catalog_selection));
            (locked, tightens)
        };
        if activation_locked {
            self.pending_catalog.lock().replace(catalog);
            self.refresh_catalog_runtime_status();
            if tightens_active_session {
                self.invalidate_catalog_session(RecordingStopReason::CatalogRetired)?;
            }
            return Ok(false);
        }
        self.pending_catalog.lock().take();
        self.activate_catalog(catalog, cached_generation);
        Ok(true)
    }

    fn activate_catalog(
        &self,
        catalog: crate::platform::windows::catalog::CatalogStore,
        generation: u64,
    ) {
        self.tracker.replace_catalog(catalog);
        self.active_catalog_generation
            .store(generation, Ordering::Release);
        self.refresh_catalog_runtime_status();
        self.shared.update(|inner| {
            inner.snapshot.bound_target = None;
            inner.bound = None;
            inner.input_control = None;
            inner.input_status = None;
            inner.rebind_required = None;
            inner.catalog_invalidation = None;
            inner.snapshot.rebind_required = false;
            if !matches!(
                inner.snapshot.state,
                CollectorState::Idle | CollectorState::Completed | CollectorState::Faulted
            ) {
                inner.snapshot.state = CollectorState::Idle;
            }
        });
    }

    fn activate_pending_catalog_if_safe(&self) {
        let Some(_control_operation) = self.control_operation.try_lock() else {
            return;
        };
        if catalog_activation_is_locked(&self.shared.inner.lock()) {
            return;
        }
        let Some(catalog) = self.pending_catalog.lock().take() else {
            return;
        };
        let generation = self.cached_catalog_generation.load(Ordering::Acquire);
        self.activate_catalog(catalog, generation);
    }

    fn refresh_catalog_runtime_status(&self) {
        let mut status = self.tracker.catalog_status();
        status.active_generation = self.active_catalog_generation.load(Ordering::Acquire);
        status.cached_generation = self.cached_catalog_generation.load(Ordering::Acquire);
        // `result` 不在这里覆写：store 自己判空（catalog.rs `status()`），两处各写
        // 一份就是分叉的开始——这一版正是那么写的，于是首个快照与刷新后的快照对
        // 同一个空目录给出两种说法。
        self.shared.update(|inner| inner.snapshot.catalog = status);
    }

    #[cfg(not(feature = "local-catalog-only"))]
    fn invalidate_catalog_session(&self, reason: RecordingStopReason) -> anyhow::Result<()> {
        let cutoff_qpc = QpcClock::new()?.now()?;
        let (stop_tx, input_control, must_signal, paused_session_id) = {
            let inner = self.shared.inner.lock();
            if inner.catalog_invalidation.is_some() {
                return Ok(());
            }
            let must_signal = matches!(
                inner.snapshot.state,
                CollectorState::Starting | CollectorState::Recording
            );
            if !matches!(
                inner.snapshot.state,
                CollectorState::Preflighting
                    | CollectorState::Starting
                    | CollectorState::Recording
                    | CollectorState::Pausing
                    | CollectorState::Paused
                    | CollectorState::Stopping
            ) {
                return Ok(());
            }
            (
                inner.stop_tx.clone(),
                inner.input_control.clone(),
                must_signal,
                (inner.snapshot.state == CollectorState::Paused)
                    .then(|| inner.session_id.clone())
                    .flatten(),
            )
        };
        if let Some(session_id) = paused_session_id {
            self.shared
                .journal
                .fault_paused_session(&session_id, stop_reason_label(reason))?;
        }
        self.shared.update(|inner| {
            inner.catalog_invalidation = Some(reason);
            inner.finish_as_completed = false;
            inner.rebind_required = None;
            inner.snapshot.rebind_required = false;
            inner.snapshot.rebind_game_id = None;
            if let Some(next_state) = catalog_invalidation_next_state(inner.snapshot.state) {
                inner.snapshot.state = next_state;
            }
        });
        if must_signal {
            stop_tx
                .context("catalog safety cutoff lost the capture control channel")?
                .try_send(RecordingStopSignal { reason, cutoff_qpc })
                .context("catalog safety cutoff could not reach the capture pipeline")?;
        }
        if let Some(input_control) = input_control {
            input_control
                .close_gate()
                .context("catalog safety cutoff could not close the Raw Input gate")?;
        }
        Ok(())
    }

    pub fn diagnostics(&self) -> Arc<DiagnosticService> {
        Arc::clone(&self.shared.diagnostics)
    }

    /// Linearize safe quit with every start/resume operation. If a start already
    /// owns the control gate, quit waits for that operation to reach its committed
    /// state; once quit owns the gate, every later start fails closed.
    pub fn begin_safe_quit(&self) -> CollectorState {
        let _control_operation = self.control_operation.begin_safe_quit();
        self.snapshot().state
    }

    pub fn cancel_safe_quit(&self) {
        self.control_operation.cancel_safe_quit();
    }

    #[must_use]
    pub fn data_model_quiescent(&self) -> bool {
        self.shared.data_model.is_quiescent()
    }

    #[must_use]
    pub fn sentry_quiescent(&self) -> bool {
        self.shared.sentry.is_quiescent()
    }

    pub fn begin_data_model_shutdown(&self) {
        self.shared.data_model.begin_shutdown();
    }

    #[must_use]
    pub fn games(&self) -> Vec<GameSummary> {
        self.tracker.games()
    }

    pub fn discover(&self, game_id: &str) -> anyhow::Result<Vec<WindowCandidate>> {
        let _control_operation = self.control_operation.lock();
        self.ensure_not_recording()?;
        let rebind = self.shared.inner.lock().rebind_required.clone();
        if let Some(expected) = &rebind
            && expected.game_id != game_id
        {
            let error = anyhow::anyhow!(
                "rebind game identity mismatch: expected {}, got {game_id}",
                expected.game_id
            );
            self.set_rebind_fault("WINDOW_REBIND_IDENTITY_MISMATCH", &error);
            return Err(error);
        }
        if let Some(overlay) = &self.overlay {
            overlay.clear_target();
        }
        self.shared.update(|inner| {
            inner.snapshot.state = CollectorState::Discovering;
            inner.snapshot.fault = None;
            inner.snapshot.bound_target = None;
            inner.bound = None;
            inner.input_control = None;
            inner.input_status = None;
            inner.finish_as_completed = false;
            inner.storage_warning_emitted = false;
            inner.snapshot.storage = StorageStatus::default();
            if rebind.is_none() {
                inner.snapshot.capture = None;
                inner.snapshot.rebind_required = false;
                inner.snapshot.rebind_game_id = None;
                inner.session_id = None;
                inner.next_run_seq = 0;
                inner.next_session_frame = 0;
                inner.last_request_id = None;
                inner.rebind_required = None;
            }
        });
        match timed_control_phase(&self.shared, "discover", "window_scan", || {
            self.tracker.discover(game_id)
        }) {
            Ok(candidates) => {
                self.shared.update(|inner| {
                    inner.snapshot.state = if rebind.is_some() {
                        CollectorState::Paused
                    } else {
                        CollectorState::Idle
                    };
                });
                Ok(candidates)
            }
            Err(error) => {
                if let Some(spec) = inactive_catalog_game_fault_spec(&error) {
                    self.set_fault_spec(spec, &error);
                } else if rebind.is_some() {
                    self.set_rebind_fault("WINDOW_REBIND_DISCOVERY_FAILED", &error);
                } else {
                    self.set_fault("WINDOW_DISCOVERY_FAILED", &error, true);
                }
                Err(error)
            }
        }
    }

    pub fn bind(&self, candidate_id: &str) -> anyhow::Result<CollectorSnapshot> {
        let _control_operation = self.control_operation.lock();
        self.ensure_not_recording()?;
        let rebind = self.shared.inner.lock().rebind_required.clone();
        match timed_control_phase(&self.shared, "bind", "window_bind", || {
            self.tracker.bind(candidate_id)
        }) {
            Ok(target) => {
                if let Some(expected) = &rebind {
                    let observed = RebindIdentity::from_target(&target);
                    if let Err(error) = validate_rebind_identity(expected, &observed) {
                        self.set_rebind_fault("WINDOW_REBIND_IDENTITY_MISMATCH", &error);
                        return Err(error);
                    }
                }
                if let Some(overlay) = &self.overlay {
                    overlay.set_target(&target);
                }
                let correlation = if rebind.is_some() {
                    self.shared.current_correlation()
                } else {
                    DiagnosticCorrelation::default()
                };
                record_diagnostic(
                    &self.shared,
                    "info",
                    correlation,
                    DiagnosticPayload::TargetBound {
                        game_id: target.summary.game_id.clone(),
                        executable_name: target.summary.executable_name.clone(),
                        client_width: target.summary.client_width,
                        client_height: target.summary.client_height,
                        dpi: target.summary.dpi,
                        recording_profile_id: target.summary.recording_profile_id.clone(),
                        mapping_id: target.summary.mapping_id.clone(),
                        mapping_version: target.summary.mapping_version.clone(),
                    },
                );
                Ok(self.shared.update(|inner| {
                    inner.snapshot.state = if rebind.is_some() {
                        CollectorState::Paused
                    } else {
                        CollectorState::Bound
                    };
                    inner.snapshot.bound_target = Some(target.summary.clone());
                    inner.snapshot.fault = None;
                    inner.snapshot.rebind_required = false;
                    inner.snapshot.rebind_game_id = None;
                    inner.bound = Some(target);
                    inner.input_control = None;
                    inner.input_status = None;
                    inner.finish_as_completed = false;
                    inner.storage_warning_emitted = false;
                    inner.snapshot.storage = StorageStatus::default();
                    inner.rebind_required = None;
                    if rebind.is_none() {
                        inner.snapshot.capture = None;
                        inner.session_id = None;
                        inner.next_run_seq = 0;
                        inner.next_session_frame = 0;
                        inner.last_request_id = None;
                    }
                }))
            }
            Err(error) => {
                if rebind.is_some() {
                    self.set_rebind_fault("WINDOW_REBIND_IDENTITY_INVALID", &error);
                } else {
                    self.set_fault("WINDOW_IDENTITY_INVALID", &error, true);
                }
                Err(error)
            }
        }
    }

    pub fn start(&self, request_id: &str) -> anyhow::Result<CollectorSnapshot> {
        self.start_internal(request_id, false)
    }

    #[cfg(feature = "depth-lab")]
    pub fn start_perf_certification_stage(
        &self,
        request_id: &str,
    ) -> anyhow::Result<CollectorSnapshot> {
        use depth_lab::perf_run::Stage;

        let stage = crate::services::depth_lab::prepare_perf_certification_stage_start()
            .map_err(anyhow::Error::msg)?;
        let state = self.snapshot().state;
        match stage {
            Stage::Baseline => ensure!(
                matches!(state, CollectorState::Bound | CollectorState::Paused),
                "基线档只能从已绑定状态启动，或在提前暂停后重跑"
            ),
            Stage::WatermarkOnly | Stage::ReadbackOnly | Stage::Full => ensure!(
                state == CollectorState::Paused,
                "下一性能认证档只能在上一档完全暂停后启动"
            ),
        }
        self.start_internal(request_id, true)
    }

    fn start_internal(
        &self,
        request_id: &str,
        performance_certification_stage: bool,
    ) -> anyhow::Result<CollectorSnapshot> {
        #[cfg(feature = "depth-lab")]
        if !performance_certification_stage {
            let collector_paused = self.snapshot().state == CollectorState::Paused;
            ensure!(
                !crate::services::depth_lab::perf_certification_blocks_generic_start(
                    collector_paused
                )
                .map_err(anyhow::Error::msg)?,
                "DEPTH_PERF_STAGE_REQUIRED: 性能认证会话必须从深度认证卡片继续，完成后请先结束验证会话"
            );
        }
        #[cfg(not(feature = "depth-lab"))]
        let _ = performance_certification_stage;
        let _control_operation = self.control_operation.lock_for_start()?;
        ensure!(
            !request_id.trim().is_empty() && request_id.len() <= 128,
            "request_id is invalid"
        );
        self.reap_finished_worker();
        self.join_terminal_worker()?;
        ensure!(
            !self.capture_storage.lock().restart_required(),
            "CAPTURE_STORAGE_RESTART_REQUIRED: restart the client before starting capture"
        );
        let (start_kind, target, fallback_state, reserved_snapshot) = {
            let mut inner = self.shared.inner.lock();
            if inner.last_request_id.as_deref() == Some(request_id) {
                return Ok(inner.snapshot.clone());
            }
            if check_start_reservation(&inner.start_reservation, request_id)?
                == ReservationCheck::Replay
            {
                return Ok(inner.snapshot.clone());
            }
            ensure!(
                matches!(
                    inner.snapshot.state,
                    CollectorState::Bound
                        | CollectorState::Paused
                        | CollectorState::Completed
                        | CollectorState::Faulted
                ),
                "a validated game window must be bound before recording"
            );
            ensure!(
                !inner.snapshot.rebind_required,
                "a replacement game window must be discovered and bound before resume"
            );
            ensure!(
                inner.catalog_invalidation.is_none(),
                "CATALOG_GENERATION_RETIRED: the prior session cannot resume under a newer catalog"
            );
            let fallback_state = inner.snapshot.state;
            let start_kind = if inner.snapshot.state == CollectorState::Paused {
                StartKind::Resume
            } else {
                StartKind::NewSession
            };
            let target = inner.bound.clone().context("bound target disappeared")?;
            inner.start_reservation = Some(request_id.to_owned());
            inner.snapshot.state = CollectorState::Preflighting;
            inner.snapshot.fault = None;
            inner.snapshot.revision = inner.snapshot.revision.saturating_add(1);
            (start_kind, target, fallback_state, inner.snapshot.clone())
        };
        self.shared.publish_snapshot(reserved_snapshot);
        let reservation =
            StartReservation::new(Arc::clone(&self.shared), request_id, fallback_state);
        let initial_preflight = preflight::start(
            request_id,
            &target.summary.target_id,
            &target.catalog_selection.catalog.content_hash,
            target.catalog_selection.session_contract,
        )?;
        let persisted_preflight =
            timed_control_phase(&self.shared, "start", "journal_begin_preflight", || {
                self.shared.journal.begin_preflight(&PreflightStart {
                    evidence: &initial_preflight,
                    target_window_id: &target.summary.target_id,
                    game_id: &target.summary.game_id,
                    catalog: &target.catalog_selection,
                })
            })?;
        if persisted_preflight.result != "running" {
            return Ok(self.shared.update(|inner| {
                inner.snapshot.state =
                    if matches!(persisted_preflight.result.as_str(), "blocked" | "failed") {
                        if start_kind == StartKind::Resume {
                            CollectorState::Paused
                        } else {
                            CollectorState::Faulted
                        }
                    } else if start_kind == StartKind::Resume {
                        CollectorState::Paused
                    } else {
                        fallback_state
                    };
                inner.snapshot.preflight = Some(persisted_preflight.clone());
                inner.snapshot.fault =
                    persisted_preflight
                        .failure_code
                        .as_ref()
                        .map(|code| CollectorFault {
                            code: code.clone(),
                            message: "启动前检查已完成，复用了已落盘的终态结果。".into(),
                            action: "查看检查详情并使用新的 request_id 重试。".into(),
                            retryable: true,
                            details_id: persisted_preflight
                                .details_id
                                .clone()
                                .unwrap_or_else(|| "details-preflight-missing".into()),
                        });
                inner.last_request_id = Some(request_id.to_owned());
            }));
        }
        let failure_state = preflight_failure_state(start_kind);
        let mut preflight_attempt =
            PreflightAttempt::new(Arc::clone(&self.shared), persisted_preflight, failure_state);
        self.shared.update(|inner| {
            inner.snapshot.preflight = Some(preflight_attempt.evidence().clone());
            inner.snapshot.fault = None;
        });
        let device_profile = target.catalog_selection.device_profile()?;
        let recording_profile = target.catalog_selection.recording_profile()?;
        preflight_attempt.append_check(
            "catalog_selection",
            serde_json::json!({
                "catalog": &target.catalog_selection.catalog,
                "device": &target.catalog_selection.device.identity,
                "capture": &target.catalog_selection.recording.identity,
                "game": &target.catalog_selection.game.identity,
                "settings": &target.catalog_selection.recording.identity,
                "mapping": &target.catalog_selection.mapping.identity,
            }),
        )?;
        if let Err(error) = self
            .tracker
            .ensure_start_authorized(&target.summary.game_id)
        {
            preflight_attempt.fail(
                "authorization",
                "blocked",
                "CATALOG_START_NOT_AUTHORIZED",
                "当前目录不允许开始录制。",
                "等待目录同步完成，或联系管理员把该游戏发布进目录后再重新选择。",
                false,
                &format!("{error:#}"),
                serde_json::json!({
                    "gameId": target.summary.game_id,
                    "productionCertified": false,
                }),
            );
            return Err(error);
        }
        // 「这次为什么被允许」仍要能离线核对（docs/26 §3 留下的纪律），只是答案
        // 变了：不再是本机出示的一纸凭证，而是「这个游戏在这份已验签目录里」。
        preflight_attempt.append_check(
            "authorization",
            serde_json::json!({
                "catalog": target.catalog_selection.catalog,
                // production_certified 是 first_party 时代的会话声明，恒 false。
                "productionCertified": false,
            }),
        )?;
        // consent 通路保留、值恒 None（docs/100 §3 推论 3）：字段是「如实记录」的
        // 载体，下一刀（可训练性开通）会给它接上账号/设备登记级的新来源。今天没有
        // 任何来源能产出一个 consent id，所以这里恒 None——凭空造一个出来，看起来
        // 就像谁授权过这次录制，那是要在写入口就拒绝的错觉。
        let session_consent_id: Option<String> = None;
        preflight_attempt.append_check(
            "device",
            serde_json::json!({
                "identity": target.catalog_selection.device.identity,
                "requirements": &device_profile,
            }),
        )?;
        preflight_attempt.append_check(
            "capture",
            serde_json::json!({
                "identity": target.catalog_selection.recording.identity,
                "profile": target.capture_profile,
            }),
        )?;
        preflight_attempt.append_check(
            "game",
            serde_json::json!({
                "identity": target.catalog_selection.game.identity,
                "gameId": target.summary.game_id,
                "executableSha256": target.executable_sha256,
            }),
        )?;
        preflight_attempt.append_check(
            "settings",
            serde_json::json!({
                "identity": target.catalog_selection.recording.identity,
                "requirements": &recording_profile,
            }),
        )?;
        // 视角标定门（C4，docs/45 §6.2）：dev 车道记录事实与警告不拦截；
        // 门通过时引用随 begin_run 同事务落库；拦截车道见下方注释。
        let view_scale_reference = match evaluate_view_scale_gate(
            &self.shared.journal,
            &self.app_data_root,
            recording_profile.view_scale.as_ref(),
            &target.summary.game_id,
            &target.summary.recording_profile_hash,
            &target.summary.device_profile_hash,
        ) {
            Ok(gate) => {
                // ⚠ 这个门今天**只记证据、不拦截**。first_party 是它原来唯一的拦截
                // 车道，随 docs/57 §3.3 退役；certified 那条曾被考虑过的拦截随车道
                // 本身退役（docs/100）。真要让它拦，是一次独立的、要 owner 拍板的收紧
                // （归「可训练性开通」那一刀），不夹带在删除里。承重没丢：可训练审批
                // 的完备性门在云端（register-catalog 的 assertViewScaleApprovable，
                // docs/46 §5.1）。
                if let Some((code, message, _action)) = &gate.block {
                    let mut facts = gate.facts;
                    if let Some(object) = facts.as_object_mut() {
                        object.insert("wouldBlockTrainable".into(), true.into());
                        object.insert("blockCode".into(), (*code).into());
                        object.insert("blockMessage".into(), message.clone().into());
                    }
                    preflight_attempt.append_check("view_scale_calibration", facts)?;
                    None
                } else {
                    preflight_attempt.append_check("view_scale_calibration", gate.facts)?;
                    gate.reference
                }
            }
            Err(error) => {
                preflight_attempt.fail(
                    "view_scale_calibration",
                    "failed",
                    "VIEW_SCALE_GATE_ERROR",
                    "视角标定门无法完成评估。",
                    "查看诊断后重试；标定记录本身不会被本次失败改动。",
                    true,
                    &format!("{error:#}"),
                    serde_json::json!({}),
                );
                return Err(error).context("view scale gate evaluation failed");
            }
        };
        preflight_attempt.append_check(
            "mapping",
            serde_json::json!({
                "identity": target.catalog_selection.mapping.identity,
                "mapping": target.mapping.mapping(),
            }),
        )?;
        if let Err(error) = timed_control_phase(&self.shared, "start", "window_revalidate", || {
            self.tracker.revalidate(&target)
        }) {
            preflight_attempt.fail(
                "window",
                "blocked",
                "WINDOW_PREFLIGHT_FAILED",
                "目标窗口身份或尺寸检查失败，未开始录制。",
                "恢复游戏窗口到目录要求的状态后重试。",
                true,
                &format!("{error:#}"),
                serde_json::json!({
                    "targetId": target.summary.target_id,
                    "gameId": target.summary.game_id,
                    "clientWidth": target.summary.client_width,
                    "clientHeight": target.summary.client_height,
                    "dpi": target.summary.dpi,
                    "expectedWidth": recording_profile.width,
                    "expectedHeight": recording_profile.height,
                }),
            );
            return Err(error);
        }
        preflight_attempt.append_check(
            "window",
            serde_json::json!({
                "targetId": target.summary.target_id,
                "gameId": target.summary.game_id,
                "processId": target.process_id,
                "executableSha256": target.executable_sha256,
                "clientWidth": target.summary.client_width,
                "clientHeight": target.summary.client_height,
                "dpi": target.summary.dpi,
            }),
        )?;
        let storage_budget = StorageBudget::from_profile(&target.capture_profile);
        let storage_status = match timed_control_phase(&self.shared, "start", "storage", || {
            storage::inspect(&self.output_root, storage_budget)
        }) {
            Ok(status) => status,
            Err(error) => {
                let status = storage::unavailable(storage_budget);
                self.shared.update(|inner| {
                    inner.snapshot.storage = status.clone();
                });
                preflight_attempt.fail(
                    "storage",
                    "failed",
                    "DISK_SPACE_UNAVAILABLE",
                    "无法读取采集目录所在磁盘的剩余空间。",
                    "确认本地磁盘在线且采集目录可访问，然后重试。",
                    true,
                    &format!("{error:#}"),
                    serde_json::json!({
                        "storage": status,
                        "startKind": if start_kind == StartKind::Resume { "resume" } else { "new_session" },
                    }),
                );
                return Err(error).context("DISK_SPACE_UNAVAILABLE before capture start");
            }
        };
        if !storage::allowed(&storage_status, storage_budget, start_kind) {
            let required = storage_budget.required_for(start_kind);
            let available = storage_status.free_bytes.unwrap_or(0);
            self.shared.update(|inner| {
                inner.snapshot.storage = storage_status.clone();
            });
            preflight_attempt.fail(
                "storage",
                "blocked",
                "DISK_SPACE_INSUFFICIENT",
                "本地磁盘空间不足，未开始录制。",
                "释放采集目录所在磁盘的空间后重试；不会自动删除已有 ready 数据。",
                true,
                &format!("available {available} bytes, required {required} bytes"),
                serde_json::json!({
                    "storage": &storage_status,
                    "availableBytes": available,
                    "requiredBytes": required,
                    "startKind": if start_kind == StartKind::Resume { "resume" } else { "new_session" },
                }),
            );
            anyhow::bail!(
                "DISK_SPACE_INSUFFICIENT: available {available} bytes, required {required} bytes"
            );
        }
        let preflight_warning = storage_status.state == StorageState::Warning;
        self.shared.update(|inner| {
            inner.snapshot.storage = storage_status.clone();
        });
        preflight_attempt.append_check("storage", serde_json::to_value(&storage_status)?)?;
        if let Err(error) = timed_control_phase(&self.shared, "start", "window_focus", || {
            self.tracker.focus_revalidated(&target)
        }) {
            preflight_attempt.fail(
                "window_focus",
                "blocked",
                "WINDOW_FOCUS_FAILED",
                "无法将已绑定游戏窗口切到前台，未开始录制。",
                "恢复窗口并确认没有系统对话框阻挡后重试。",
                true,
                &format!("{error:#}"),
                serde_json::json!({
                    "targetId": target.summary.target_id,
                    "foregroundRequired": true,
                }),
            );
            return Err(error).context("failed to focus the bound game before recording");
        }
        preflight_attempt
            .append_check("window_focus", serde_json::json!({ "foreground": true }))?;
        let hardware_evidence = match timed_control_phase(
            &self.shared,
            "start",
            "hardware_probe",
            || {
                probe_hardware_path(
                    &target.capture_profile,
                    self.artifact_root
                        .join(HARDWARE_PREFLIGHT_SCRATCH_DIRECTORY),
                )
            },
        ) {
            Ok(evidence) => evidence,
            Err(error) => {
                preflight_attempt.fail(
                        "gpu_encoder_path",
                        "failed",
                        "HARDWARE_PATH_PREFLIGHT_FAILED",
                        "当前设备不满足所选游戏采集要求：Windows、GPU、驱动或硬件 H.264 编码检查失败，未开始录制。",
                        "检查 Windows 版本、NVIDIA GPU、驱动与硬件编码能力后重试。",
                        true,
                        &format!("{error:#}"),
                        serde_json::json!({
                            "deviceRequirements": &device_profile,
                            "captureIdentity": target.catalog_selection.recording.identity,
                            "profileId": target.capture_profile.profile_id,
                        }),
                    );
                // RCA 直报（docs/23 §0）：让失败按 build 在仓库可聚合。
                // 探测失败拿不到 GPU 证据，Windows 版本单独取。
                self.report_preflight_event(
                    "preflight_failed",
                    "error",
                    serde_json::json!({
                        "schema_version": crate::services::client_events::PREFLIGHT_SCHEMA,
                        "failureCode": "HARDWARE_PATH_PREFLIGHT_FAILED",
                        "checkId": "gpu_encoder_path",
                        "windowsBuild": capture_core::platform::WindowsVersion::current()
                            .map(|windows| windows.build.to_string())
                            .unwrap_or_else(|_| "unknown".to_string()),
                        "retryable": true,
                    }),
                );
                return Err(error).context("hardware path preflight failed before capture start");
            }
        };
        preflight_attempt.append_check(
            "gpu_encoder_path",
            serde_json::to_value(&hardware_evidence)?,
        )?;
        if let Err(error) = device_profile.verify_hardware_evidence(&hardware_evidence) {
            preflight_attempt.fail(
                "device_runtime",
                "blocked",
                "DEVICE_PROFILE_RUNTIME_MISMATCH",
                "当前设备不满足所选游戏采集要求，未开始录制。",
                "检查 Windows 版本、NVIDIA GPU、驱动与硬件 H.264 编码能力，满足该游戏要求后重试。",
                true,
                &format!("{error:#}"),
                serde_json::json!({
                    "deviceIdentity": target.catalog_selection.device.identity,
                    "requirements": &device_profile,
                    "observed": &hardware_evidence,
                }),
            );
            self.report_preflight_event(
                "preflight_failed",
                "error",
                preflight_report_details(
                    Some("DEVICE_PROFILE_RUNTIME_MISMATCH"),
                    "device_runtime",
                    &hardware_evidence,
                ),
            );
            return Err(error)
                .context("hardware evidence does not satisfy the game's catalog device profile");
        }
        preflight_attempt.append_check(
            "device_runtime",
            serde_json::json!({
                "deviceIdentity": target.catalog_selection.device.identity,
                "windows": hardware_evidence.windows,
                "adapterVendorId": hardware_evidence.adapter_vendor_id,
                "adapterDeviceId": hardware_evidence.adapter_device_id,
                "adapterDriverVersion": hardware_evidence.adapter_driver_version,
            }),
        )?;
        // 硬件路径全部通过：pass 与 fail 同源直报，RCA 查询才有分母。
        // 同时把运行上下文盖进后续所有遥测（docs/23 §4.3）。
        if let Some(recorder) = self.client_event_recorder() {
            recorder.set_context(crate::services::client_events::StampContext {
                session_contract: Some(
                    target
                        .catalog_selection
                        .session_contract
                        .source_family()
                        .to_string(),
                ),
                // 恒 false：能产出 consent id 的只有 first_party 那条通道，它随
                // docs/57 §3.3 整条退役了。写死 false 而不是 None——None 的语义是
                // 「没评估过」，而这里是评估过且确定没有。
                consent_present: Some(false),
            });
        }
        self.report_preflight_event(
            "preflight_passed",
            "info",
            preflight_report_details(None, "gpu_encoder_path", &hardware_evidence),
        );
        if game_requires_depth(&target.summary.game_id) && !depth_feature_compiled_in() {
            preflight_attempt.fail(
                "depth_probe_ready",
                "blocked",
                "DEPTH_REQUIRED_BUILD_MISSING",
                "该游戏只允许录制带深度的数据；当前客户端没有 Depth 功能，录制未开始。",
                "使用包含 Depth 功能和对应渲染后端的本地实验构建后重试。",
                false,
                "the selected local CP2077 lane requires depth-lab, but this client was built without it",
                serde_json::json!({
                    "gameId": target.summary.game_id,
                    "depthRequired": true,
                    "depthFeatureCompiledIn": false,
                }),
            );
            anyhow::bail!("DEPTH_REQUIRED_BUILD_MISSING");
        }
        let (session_id, run_seq, session_frame_start) = {
            let inner = self.shared.inner.lock();
            if start_kind == StartKind::Resume {
                (
                    inner
                        .session_id
                        .clone()
                        .context("paused session identity is missing")?,
                    inner.next_run_seq,
                    inner.next_session_frame,
                )
            } else {
                (next_session_id(target.summary.session_contract)?, 0, 0)
            }
        };
        // A failed Depth start may already have written ACKs without creating a
        // capture_runs row. Keep that attempt intact and never replay its ACKs
        // into a later generation, even if next_run_seq has not advanced yet.
        let run_seq = crate::services::run_directory::next_available_run_seq(
            &self.output_root.join(&session_id).join("runs"),
            run_seq,
        )
        .context("failed to select a fresh run directory before startup")?;
        let run_id = format!("r{run_seq:04}");
        let cutoff_qpc = Arc::new(AtomicI64::new(i64::MAX));
        #[cfg(feature = "depth-lab")]
        let depth_cutoff_qpc = Arc::clone(&cutoff_qpc);
        let recorder_event_cutoff_qpc = Arc::clone(&cutoff_qpc);
        let (stop_tx, stop_rx) = bounded::<RecordingStopSignal>(8);
        let (input_tx, input_rx) = bounded::<RawInputRecord>(RAW_INPUT_QUEUE_CAPACITY);
        let overlay_placement_dirty = self.overlay.as_ref().map_or_else(
            || Arc::new(AtomicBool::new(false)),
            |overlay| overlay.placement_dirty_flag(),
        );
        let monitor =
            match timed_control_phase(&self.shared, "start", "input_monitor_ready", || {
                TargetWindowMonitor::start(
                    &target,
                    format!("{session_id}:{run_id}"),
                    Arc::clone(&cutoff_qpc),
                    stop_tx.clone(),
                    input_tx,
                    overlay_placement_dirty,
                )
            }) {
                Ok(monitor) => monitor,
                Err(error) => {
                    preflight_attempt.fail(
                        "input_monitor_ready",
                        "failed",
                        "INPUT_MONITOR_PREFLIGHT_FAILED",
                        "Raw Input 或窗口监视器启动失败，未开始录制。",
                        "关闭占用输入监视器的采集任务并确认目标窗口仍在前台，然后重试。",
                        true,
                        &format!("{error:#}"),
                        serde_json::json!({
                            "inputBackend": device_profile.input_backend,
                            "pointerMode": device_profile.pointer_mode,
                            "eventGate": "closed",
                        }),
                    );
                    return Err(error)
                        .context("input monitor preflight failed before capture start");
                }
            };
        let input_control = monitor.input_control();
        let input_status = monitor.input_status();
        if input_status.is_accepting() || input_status.faulted() || !input_rx.is_empty() {
            let diagnostic = format!(
                "accepting={}, faulted={}, queued_events={}",
                input_status.is_accepting(),
                input_status.faulted(),
                input_rx.len()
            );
            preflight_attempt.fail(
                "input_monitor_ready",
                "failed",
                "INPUT_MONITOR_GATE_INVALID",
                "Raw Input 启动闸门状态异常，未开始录制。",
                "重启客户端后重新绑定目标窗口并重试。",
                true,
                &diagnostic,
                serde_json::json!({
                    "accepting": input_status.is_accepting(),
                    "faulted": input_status.faulted(),
                    "queuedEvents": input_rx.len(),
                    "eventGate": "closed",
                }),
            );
            anyhow::bail!("INPUT_MONITOR_GATE_INVALID: {diagnostic}");
        }
        preflight_attempt.append_check(
            "input_monitor_ready",
            serde_json::json!({
                "inputBackend": device_profile.input_backend,
                "pointerMode": device_profile.pointer_mode,
                "rawInputRegistered": true,
                "windowEventHooksInstalled": true,
                "eventGate": "closed_until_begin_run",
                "queuedEvents": input_rx.len(),
            }),
        )?;
        let recording_target = RecordingTarget {
            hwnd_value: target.hwnd_value,
            process_id: target.process_id,
            target_id: target.summary.target_id.clone(),
            game_id: target.summary.game_id.clone(),
            executable_sha256: target.executable_sha256.clone(),
        };
        let request = RecordingRequest {
            session_id: session_id.clone(),
            run_id: run_id.clone(),
            session_frame_start,
            output_root: self.output_root.clone(),
            target: recording_target,
            profile: target.capture_profile.clone(),
            segment_policy_override: None,
            mapping: target.mapping.clone(),
            catalog: target.catalog_selection.recording_context(),
            control_vks: target.control_vks(),
            // 恒 None（上面 session_consent_id 的注释）。通路保留，等下一刀接新来源。
            consent_id: session_consent_id.clone(),
            present_identity: None,
            selected_present_feedback: None,
            selected_depth_cadence: Default::default(),
            cutoff_qpc,
            input_records: input_rx,
            input_control: input_control.clone(),
            input_status: input_status.clone(),
        };
        #[cfg(feature = "depth-lab")]
        let mut request = request;
        let run_directory = self
            .output_root
            .join(&session_id)
            .join("runs")
            .join(&run_id);
        let reservation_valid = {
            let inner = self.shared.inner.lock();
            inner.start_reservation.as_deref() == Some(request_id)
                && inner.snapshot.state == CollectorState::Preflighting
                && inner
                    .bound
                    .as_ref()
                    .is_some_and(|bound| bound.summary.target_id == target.summary.target_id)
        };
        if !reservation_valid {
            preflight_attempt.fail(
                "reservation",
                "failed",
                "START_RESERVATION_INVALID",
                "启动预留身份已变化，未开始录制。",
                "重新扫描并绑定目标窗口后使用新的请求标识重试。",
                true,
                "start reservation or bound target changed before commit",
                serde_json::json!({
                    "requestIdMatches": false,
                    "targetIdentityMatches": false,
                }),
            );
            anyhow::bail!("START_RESERVATION_INVALID before begin_run");
        }
        let monitor_commit = match monitor.prepare_commit() {
            Ok(commit) => commit,
            Err(error) => {
                preflight_attempt.fail(
                    "input_monitor_ready",
                    "blocked",
                    "WINDOW_INVALIDATED_DURING_PREFLIGHT",
                    "目标窗口在启动提交前失效，未开始录制。",
                    "恢复目标窗口到前台且保持尺寸稳定后，使用新的请求标识重试。",
                    true,
                    &format!("{error:#}"),
                    serde_json::json!({
                        "eventGate": "closed",
                        "journalCommitted": false,
                    }),
                );
                return Err(error).context("target invalidated before begin_run commit");
            }
        };
        #[cfg(feature = "depth-lab")]
        let performance_validation_only =
            crate::services::depth_lab::configured_performance_validation_only()
                .map_err(anyhow::Error::msg)?;
        #[cfg(feature = "depth-lab")]
        let generation =
            crate::services::depth_lab::next_generation().map_err(anyhow::Error::msg)?;
        #[cfg(all(feature = "depth-lab", any(feature = "dev-catalog", test)))]
        let mut cp2077_sidecar = crate::services::cp2077_trial::begin_main_collector(
            &target.summary.game_id,
            target.process_id,
            &session_id,
            &run_id,
            generation,
        )
        .map_err(anyhow::Error::msg)?;
        #[cfg(all(feature = "depth-lab", any(feature = "dev-catalog", test)))]
        let performance_validation_only = performance_validation_only || cp2077_sidecar.is_some();
        #[cfg(all(feature = "depth-lab", any(feature = "dev-catalog", test)))]
        if cp2077_sidecar.is_some() {
            let (profile, segment_policy_override) =
                crate::cp2077_raw_demo::local_depth_trial_profile(&request.profile)?;
            request.profile = profile;
            request.segment_policy_override = Some(segment_policy_override);
        }
        #[cfg(all(feature = "depth-lab", any(feature = "dev-catalog", test)))]
        if cp2077_sidecar.is_some() {
            preflight_attempt.append_check(
                "depth_probe_ready",
                serde_json::json!({
                    "enabled": true,
                    "state": "main_collector_raw_sidecar_ready",
                    "generation": generation,
                    "performanceValidationOnly": true,
                    "depthDeliverable": false,
                    "mainCollectorFormalStartRun": true,
                    "sidecarProbeFormalStartRun": false,
                    "captureOwner": "dataminer_main_collector",
                    "gpuAdapterDescription": hardware_evidence.adapter_description,
                }),
            )?;
        }
        #[cfg(feature = "depth-lab")]
        let mut depth_run = {
            let depth_output_dir = run_directory.join("depth-lab");
            // 外层块已在 depth-lab 门内，这里无需再分派（cfg 归并前第二臂是
            // not(cp2077-preflight)，有真实取值路径；归并后恒不可达，已删）。
            #[cfg(any(feature = "dev-catalog", test))]
            let use_raw_sidecar = cp2077_sidecar.is_some();
            #[cfg(not(any(feature = "dev-catalog", test)))]
            let use_raw_sidecar = false;
            if use_raw_sidecar {
                None
            } else {
                match crate::services::depth_lab::start_for_run(
                    crate::services::depth_lab::DepthStartContext {
                        catalog_selection: &target.catalog_selection,
                        hardware_evidence: Some(&hardware_evidence),
                        session_id: &session_id,
                        run_id: &run_id,
                        generation,
                        game_id: &target.summary.game_id,
                        game_pid: target.process_id,
                        executable_sha256: target.executable_sha256.as_deref(),
                        output_dir: &depth_output_dir,
                        capture_fps_num: target.capture_profile.fps_num,
                        capture_fps_den: target.capture_profile.fps_den,
                        free_disk_bytes: storage_status.free_bytes.unwrap_or(0),
                        gpu_adapter_description: &hardware_evidence.adapter_description,
                        gpu_driver_version: hardware_evidence.adapter_driver_version.as_deref(),
                        gpu_adapter_luid_high: hardware_evidence.adapter_luid_high,
                        gpu_adapter_luid_low: hardware_evidence.adapter_luid_low,
                    },
                ) {
                    Ok(run) => {
                        if game_requires_depth(&target.summary.game_id) && run.is_none() {
                            preflight_attempt.fail(
                            "depth_probe_ready",
                            "blocked",
                            "DEPTH_REQUIRED_NOT_ENABLED",
                            "该游戏只允许录制带深度的数据；Depth 未开启，录制未开始。",
                            "配置并开启 Depth，等待真实 Ready 后再开始。",
                            false,
                            "the selected local CP2077 lane requires a Ready depth run, but start_for_run returned disabled",
                            serde_json::json!({
                                "gameId": target.summary.game_id,
                                "depthRequired": true,
                                "depthEnabled": false,
                            }),
                        );
                            anyhow::bail!("DEPTH_REQUIRED_NOT_ENABLED");
                        }
                        preflight_attempt.append_check(
                            "depth_probe_ready",
                            serde_json::json!({
                                "enabled": run.is_some(),
                                "state": if run.is_some() { "ready" } else { "disabled" },
                                "generation": run.as_ref().map(|run| run.identity().generation),
                                "performanceValidationOnly": run
                                    .as_ref()
                                    .is_some_and(|run| run.performance_validation_only()),
                                "depthDeliverable": run
                                    .as_ref()
                                    .is_none_or(|run| !run.performance_validation_only()),
                                "gpuAdapterDescription": hardware_evidence.adapter_description,
                            }),
                        )?;
                        run
                    }
                    Err(error) => {
                        let fault = depth_startup_fault_spec(&error);
                        preflight_attempt.fail(
                            "depth_probe_ready",
                            "failed",
                            fault.code,
                            fault.message,
                            fault.action,
                            fault.retryable,
                            &error,
                            serde_json::json!({
                                "configurationWrittenIsNotReady": true,
                                "gpuAdapterDescription": hardware_evidence.adapter_description,
                            }),
                        );
                        anyhow::bail!("{}: {error}", fault.code);
                    }
                }
            }
        };
        #[cfg(feature = "depth-lab")]
        if let Some(run) = depth_run
            .as_ref()
            .filter(|run| run.present_identity_required())
        {
            let (watermark_source_width, watermark_source_height) =
                run.watermark_source_dimensions();
            request.present_identity = Some(capture_core::capture::PresentIdentitySession {
                generation: run.identity().generation,
                resource_epoch: 1,
                first_present_seq: run.first_present_seq(),
                unwrap_accept_budget: capture_core::capture::DEFAULT_PRESENT_IDENTITY_UNWRAP_BUDGET,
                watermark_source_width,
                watermark_source_height,
            });
        }
        #[cfg(feature = "depth-lab")]
        if let Some(run) = depth_run.as_mut() {
            let feedback_required = run.selected_present_feedback_required();
            request.selected_present_feedback = run.take_feedback();
            ensure!(
                request.selected_present_feedback.is_some() == feedback_required,
                "depth Ready selected-Present feedback transport does not match the run contract"
            );
        }
        #[cfg(feature = "depth-lab")]
        let depth_expected = depth_run.is_some();
        #[cfg(all(feature = "depth-lab", any(feature = "dev-catalog", test)))]
        let depth_expected = depth_expected || cp2077_sidecar.is_some();
        #[cfg(not(feature = "depth-lab"))]
        let depth_expected = false;
        #[cfg(not(feature = "depth-lab"))]
        let performance_validation_only = false;
        let completed_preflight = preflight_attempt.completed_evidence(&session_id, &run_id)?;
        if let Err(error) = timed_control_phase(&self.shared, "start", "journal_begin_run", || {
            self.shared.journal.begin_run(&RunStart {
                request_id,
                session_id: &session_id,
                run_id: &run_id,
                run_seq,
                session_frame_start,
                depth_expected,
                performance_validation_only,
                process_id: target.process_id,
                target_window_id: &target.summary.target_id,
                executable_sha256: target.executable_sha256.as_deref(),
                executable_sha256_missing_reason: target
                    .executable_sha256_missing_reason
                    .as_deref(),
                profile: &request.profile,
                mapping: &target.mapping,
                catalog: &target.catalog_selection,
                preflight: &completed_preflight,
                view_calibration: view_scale_reference
                    .as_ref()
                    .map(|(id, sha256)| (id.as_str(), sha256.as_str())),
                consent_id: session_consent_id.as_deref(),
            })
        }) {
            preflight_attempt.fail(
                "journal",
                "failed",
                "JOURNAL_WRITE_FAILED",
                "启动事实无法原子写入本地 journal，未开始录制。",
                "保留诊断并重试；不会留下通过态 preflight 或空 session/run。",
                true,
                &format!("{error:#}"),
                serde_json::json!({
                    "transaction": "preflight_pass_and_begin_run",
                    "committed": false,
                }),
            );
            return Err(error).context("failed to persist capture run start");
        }
        #[cfg(all(feature = "depth-lab", any(feature = "dev-catalog", test)))]
        if let Some(sidecar) = cp2077_sidecar.as_mut()
            && let Err(error) = sidecar.arm_after_journal_commit()
        {
            sidecar.mark_faulted(&error);
            let _ = self.shared.journal.fail_run(
                &session_id,
                &run_id,
                "CP2077_DEPTH_SIDECAR_ARM_FAILED",
            );
            anyhow::bail!("CP2077_DEPTH_SIDECAR_ARM_FAILED: {error}");
        }
        let input_gate_opened = monitor_commit.activate_and_open();
        preflight_attempt.commit(completed_preflight);
        if !input_gate_opened {
            let error = anyhow::anyhow!(
                "INPUT_GATE_OPEN_FAILED: Raw Input faulted before the post-journal gate opened"
            );
            let _ = self
                .shared
                .journal
                .fail_run(&session_id, &run_id, "INPUT_GATE_OPEN_FAILED");
            let diagnostic = format!("{error:#}");
            let fault = self.shared.run_fault(
                &session_id,
                &run_id,
                FaultSpec {
                    code: "INPUT_GATE_OPEN_FAILED",
                    message: "Raw Input 启动闸门异常，录制未能开始。",
                    action: "重启客户端后重新绑定目标窗口并重试。",
                    retryable: true,
                },
                Some(&diagnostic),
            );
            let mut report_snapshot = self.shared.snapshot();
            report_snapshot.state = CollectorState::Faulted;
            report_snapshot.capture = Some(CaptureStatus {
                session_id: session_id.clone(),
                run_id: run_id.clone(),
                run_seq,
                ..CaptureStatus::default()
            });
            let runtime = runtime_diagnostic_snapshot(&report_snapshot, Some(&session_id));
            let pending_report = self.shared.prepare_sentry_recording_failure_report(
                session_id.clone(),
                run_id.clone(),
                fault.code.clone(),
                fault.details_id.clone(),
                runtime,
            );
            self.shared.update(|inner| {
                inner.snapshot.state = CollectorState::Faulted;
                inner.snapshot.fault = Some(fault);
            });
            if let Some(report) = pending_report {
                self.shared.spawn_sentry_recording_failure_report(report);
            }
            return Err(error);
        }
        if let Some(overlay) = &self.overlay {
            overlay.attach_input_status(input_status.clone());
        } else {
            emit_unavailable_warning(
                &self.shared.app,
                &self.shared.diagnostics,
                DiagnosticCorrelation::run(&session_id, &run_id),
            );
        }
        let recorder_event_stop_tx = stop_tx.clone();
        let recorder_event_input_control = input_control.clone();
        // 录制期设置文件监视（C4，docs/46 §4.3）：政策声明了监视路径才起
        // 线程；路径解析失败按"监视缺席"如实记录（日志），不阻塞录制。
        let settings_watch = recording_profile
            .view_scale
            .as_ref()
            .filter(|policy| policy.applicable && !policy.config_watch_paths.is_empty())
            .and_then(|policy| {
                crate::services::settings_watch::SettingsWatch::start(
                    crate::services::settings_watch::SettingsWatchSpec {
                        process_id: target.process_id,
                        relative_paths: policy.config_watch_paths.clone(),
                        calibration_id: view_scale_reference.as_ref().map(|(id, _)| id.clone()),
                    },
                    stop_tx.clone(),
                    Arc::clone(&self.shared.journal),
                )
            });
        self.shared.update(|inner| {
            inner.snapshot.state = CollectorState::Starting;
            inner.snapshot.fault = None;
            inner.snapshot.capture = Some(CaptureStatus {
                session_id: session_id.clone(),
                run_id: run_id.clone(),
                run_seq,
                ..CaptureStatus::default()
            });
            inner.stop_tx = Some(stop_tx);
            inner.settings_watch = settings_watch;
            inner.input_control = Some(input_control);
            inner.input_status = Some(input_status);
            inner.started_at = Some(Instant::now());
            inner.last_request_id = Some(request_id.to_owned());
            if inner.start_reservation.as_deref() == Some(request_id) {
                inner.start_reservation = None;
            }
            inner.session_id = Some(session_id.clone());
            inner.next_run_seq = run_seq.saturating_add(1);
            inner.finish_as_completed = false;
            inner.storage_warning_emitted = preflight_warning;
        });
        reservation.disarm();

        let shared = Arc::clone(&self.shared);
        let worker_session_id = session_id.clone();
        let worker_run_id = run_id.clone();
        #[cfg(all(feature = "depth-lab", any(feature = "dev-catalog", test)))]
        let cp2077_started_beep = cp2077_sidecar.is_some();
        let worker = match std::thread::Builder::new()
            .name("dm-capture-pipeline".into())
            .spawn(move || {
                let event_shared = Arc::clone(&shared);
                let event_session_id = worker_session_id.clone();
                let event_run_id = worker_run_id.clone();
                let event_run_directory = run_directory.clone();
                let event_worker = RecorderEventWorker::start(
                    format!("dm-recorder-events-{event_run_id}"),
                    move |event, dispatch| {
                        apply_recorder_event(
                            &event_shared,
                            &event_session_id,
                            &event_run_id,
                            &event_run_directory,
                            event,
                            dispatch,
                        )
                    },
                    move || {
                        request_capture_stop_after_recorder_event_failure(
                            &recorder_event_stop_tx,
                            &recorder_event_input_control,
                            &recorder_event_cutoff_qpc,
                        );
                    },
                );
                let result = match event_worker {
                    Ok(event_worker) => {
                        #[cfg(all(feature = "depth-lab", any(feature = "dev-catalog", test)))]
                        let cp2077_started_beep_emitted = AtomicBool::new(false);
                        let capture_result = record_until_stopped(request, &stop_rx, |event| {
                            #[cfg(all(
                                feature = "depth-lab",
                                any(feature = "dev-catalog", test)
                            ))]
                            if cp2077_started_beep
                                && matches!(&event, RecorderEvent::Started { .. })
                            {
                                ensure!(
                                    !cp2077_started_beep_emitted.swap(true, Ordering::AcqRel),
                                    "CP2077 main collector emitted Started twice"
                                );
                                crate::cp2077_raw_demo::play_main_collector_started_beep();
                            }
                            event_worker.submit(event)
                        });
                        let event_result = event_worker.finish();
                        combine_capture_and_recorder_event_results(capture_result, event_result)
                    }
                    Err(error) => Err(error),
                };
                drop(monitor);
                #[cfg(feature = "depth-lab")]
                let result = {
                    let mut result = result;
                    if let Some(run) = depth_run.as_mut() {
                        let generation = run.identity().generation;
                        let probe_root = run.output_dir().to_path_buf();
                        let observed_cutoff = depth_cutoff_qpc.load(Ordering::Acquire);
                        let cutoff = if observed_cutoff == i64::MAX || observed_cutoff <= 0 {
                            crate::services::depth_lab::current_qpc_for_cleanup()
                        } else {
                            observed_cutoff
                        };
                        let reason = match &result {
                            Ok(outcome) => format!("{:?}", outcome.stop_reason),
                            Err(_) => "recording_failure".to_string(),
                        };
                        // 深度连续异常停录时，探针**已经自己拆掉了 run**，这次
                        // StopRun 注定收不到 Flushed。那不是一个新失败，而是同一件
                        // 事的下游表现——把它当失败会盖掉 `stop_reason`，
                        // 而 `finish_run` 正是靠 outcome 才把停录理由写进 session。
                        let health_abort_stop = matches!(
                            &result,
                            Ok(outcome)
                                if outcome.stop_reason == RecordingStopReason::DepthFrameHealth
                        );
                        let performance_validation_only = run.performance_validation_only();
                        match run.stop_and_flush(cutoff, &reason).cloned() {
                            Ok(flushed) if result.is_ok()
                                && !should_finalize_depth_run(performance_validation_only) => {
                                if run.local_cp2077_audit_required()
                                    && let Err(error) = crate::services::depth_finalize::audit_local_cp2077_run(
                                        &crate::services::depth_finalize::FinalizeRecordedDepthRun {
                                            journal: &shared.journal,
                                            session_id: &worker_session_id,
                                            run_id: &worker_run_id,
                                            generation,
                                            run_directory: &run_directory,
                                            probe_root: &probe_root,
                                            flushed: &flushed,
                                        },
                                        run.first_present_seq(),
                                    ) {
                                        result = Err(anyhow::anyhow!("CP2077_LOCAL_AUDIT_FAILED: {error}"));
                                }
                                tracing::info!(
                                    session_id = %worker_session_id,
                                    run_id = %worker_run_id,
                                    generation,
                                    "performance-validation-only depth run flushed without deliverable finalization"
                                );
                            }
                            Ok(flushed) if result.is_ok() => {
                                let finalization =
                                    crate::services::depth_finalize::finalize_recorded_depth_run(
                                        &crate::services::depth_finalize::FinalizeRecordedDepthRun {
                                            journal: &shared.journal,
                                            session_id: &worker_session_id,
                                            run_id: &worker_run_id,
                                            generation,
                                            run_directory: &run_directory,
                                            probe_root: &probe_root,
                                            flushed: &flushed,
                                        },
                                    );
                                if let Err(finalize_error) = finalization {
                                    let reject_error = shared
                                        .journal
                                        .reject_depth_finalization(
                                            &worker_session_id,
                                            &worker_run_id,
                                        )
                                        .err();
                                    result = Err(anyhow::anyhow!(
                                        "DEPTH_FINALIZE_FAILED: {finalize_error}{}",
                                        reject_error.map_or_else(String::new, |error| format!(
                                            "; failed to reject blocked uploads: {error:#}"
                                        ))
                                    ));
                                }
                            }
                            Ok(_) => {}
                            Err(flush_error) if health_abort_stop => {
                                // 深度不可交付这件事不必再判一次：上传闸对
                                // `depth_expected = 1` 的 run 要求 segment 带齐
                                // depth 路径与校验和，而这次收口根本没产出它们，
                                // 所以这些 segment 天然出不去（fail closed）。
                                tracing::warn!(
                                    error = %flush_error,
                                    session_id = %worker_session_id,
                                    run_id = %worker_run_id,
                                    "depth probe had already aborted on frame health; \
                                     the stop/flush failure is that abort's downstream effect"
                                );
                            }
                            Err(flush_error) => {
                                result = match result {
                                    Ok(_) => Err(anyhow::anyhow!(
                                        "DEPTH_PROBE_NOT_FLUSHED: {flush_error}"
                                    )),
                                    Err(recording_error) => Err(recording_error.context(format!(
                                        "depth probe also failed to flush: {flush_error}"
                                    ))),
                                };
                            }
                        }
                    }
                    result
                };
                #[cfg(all(feature = "depth-lab", any(feature = "dev-catalog", test)))]
                let mut cp2077_sidecar = cp2077_sidecar;
                #[cfg(all(feature = "depth-lab", any(feature = "dev-catalog", test)))]
                let result = match result {
                    Ok(outcome) => {
                        if let Some(sidecar) = cp2077_sidecar.as_mut() {
                            match sidecar
                                .stop_probe()
                                .and_then(|()| sidecar.finalize(&outcome, &shared.journal))
                            {
                                Ok(()) => Ok(outcome),
                                Err(error) => {
                                    sidecar.mark_faulted(&error);
                                    Err(anyhow::anyhow!(
                                        "CP2077_DEPTH_SIDECAR_FINALIZE_FAILED: {error}"
                                    ))
                                }
                            }
                        } else {
                            Ok(outcome)
                        }
                    }
                    Err(error) => {
                        if let Some(sidecar) = cp2077_sidecar.as_mut() {
                            let _ = sidecar.stop_probe();
                            sidecar.mark_faulted(&format!("{error:#}"));
                        }
                        Err(error)
                    }
                };
                match result {
                    Ok(mut outcome) => {
                        let (terminal_state, rebind_identity) = {
                            let inner = shared.inner.lock();
                            if let Some(invalidation) = inner.catalog_invalidation {
                                outcome.stop_reason = invalidation;
                            }
                            let mut state = clean_stop_state(outcome.stop_reason);
                            if inner.finish_as_completed && state == CollectorState::Paused {
                                state = CollectorState::Completed;
                            }
                            let rebind = (state == CollectorState::Paused
                                && outcome.stop_reason == RecordingStopReason::WindowDestroyed)
                                .then(|| inner.bound.as_ref().map(RebindIdentity::from_target))
                                .flatten();
                            (state, rebind)
                        };
                        if let Err(error) = shared
                            .journal
                            .finish_run(&outcome, journal_terminal_state(terminal_state))
                        {
                            tracing::error!(
                                error = %error,
                                session_id = %outcome.session_id,
                                run_id = %outcome.run_id,
                                "failed to commit terminal capture state to SQLite journal"
                            );
                            let _ = shared.journal.fail_run(
                                &outcome.session_id,
                                &outcome.run_id,
                                "JOURNAL_COMMIT_FAILED",
                            );
                            let diagnostic = format!("{error:#}");
                            let fault = shared.run_fault(
                                &outcome.session_id,
                                &outcome.run_id,
                                FaultSpec {
                                    code: "JOURNAL_COMMIT_FAILED",
                                    message: "本地采集日志提交失败，录制已停止。",
                                    action: "保留本地文件并重启客户端执行恢复核验。",
                                    retryable: true,
                                },
                                Some(&diagnostic),
                            );
                            shared.update(|inner| {
                                inner.snapshot.state = CollectorState::Faulted;
                                inner.snapshot.fault = Some(fault);
                                inner.stop_tx = None;
                            inner.settings_watch = None;
                                inner.input_control = None;
                                inner.input_status = None;
                                inner.started_at = None;
                                inner.finish_as_completed = false;
                            });
                            if let Some(overlay) = &shared.overlay {
                                overlay.detach_input_status();
                            }
                            return;
                        }
                        sync_heartbeat_artifact(
                            &shared,
                            &outcome.session_id,
                            &outcome.run_id,
                        );
                        shared.data_model.wake();
                        record_run_stopped(&shared, &outcome);
                        let terminal_fault =
                            terminal_fault_spec(outcome.stop_reason, terminal_state).map(|spec| {
                                shared.run_fault(&outcome.session_id, &outcome.run_id, spec, None)
                            });
                        let rebind_notification = rebind_identity.as_ref().map(|_| {
                            shared.diagnostics.create_notification(
                                "WINDOW_REBIND_REQUIRED",
                                "原采集窗口已关闭，当前 run 已封口。",
                                "重新启动同一合成窗口，扫描并绑定后可在同一 session 中恢复为新 run。",
                                true,
                                DiagnosticCorrelation::run(&outcome.session_id, &outcome.run_id),
                                None,
                            )
                        });
                        // docs/65 J-23：自动暂停要说清原因。guard 在 Paused 上——
                        // finish_as_completed 把 Paused 升为 Completed 时属于用户
                        // 主动收尾，不再打扰。
                        let pause_notification = (terminal_state == CollectorState::Paused)
                            .then(|| auto_pause_notification_spec(outcome.stop_reason))
                            .flatten()
                            .map(|(code, message, action)| {
                                shared.diagnostics.create_notification(
                                    code,
                                    message,
                                    action,
                                    true,
                                    DiagnosticCorrelation::run(
                                        &outcome.session_id,
                                        &outcome.run_id,
                                    ),
                                    None,
                                )
                            });
                        shared.update(|inner| {
                            inner.snapshot.state = terminal_state;
                            if let Some(capture) = &mut inner.snapshot.capture {
                                capture.frame_count = outcome.frame_count;
                                capture.ready_segments = outcome.segment_count;
                                capture.duplicate_frames = outcome.duplicate_frames;
                                capture.source_superseded = outcome.source_superseded;
                                capture.capture_queue_depth = 0;
                                capture.input_event_count = outcome.input_event_count;
                                capture.input_queue_depth = 0;
                                capture.input_queue_overflows = outcome.input_queue_overflows;
                                capture.pressed_contract_keys.clear();
                                capture.mouse_buttons.clear();
                            }
                            inner.snapshot.fault = terminal_fault;
                            inner.snapshot.rebind_required = rebind_identity.is_some();
                            inner.snapshot.rebind_game_id =
                                rebind_identity.as_ref().map(|identity| identity.game_id.clone());
                            inner.stop_tx = None;
                            inner.settings_watch = None;
                            inner.next_session_frame = outcome.session_frame_end;
                            inner.input_control = None;
                            inner.input_status = None;
                            inner.started_at = None;
                            inner.finish_as_completed = false;
                            inner.storage_warning_emitted = false;
                            if let Some(identity) = rebind_identity.clone() {
                                inner.rebind_required = Some(identity);
                                inner.snapshot.bound_target = None;
                                inner.bound = None;
                            } else {
                                inner.rebind_required = None;
                            }
                        });
                        if let Some(notification) = rebind_notification {
                            emit_collector_notification(&shared.app, &notification);
                        }
                        if let Some(notification) = pause_notification {
                            emit_collector_notification(&shared.app, &notification);
                        }
                        if let Some(overlay) = &shared.overlay {
                            if rebind_identity.is_some() {
                                overlay.clear_target();
                            } else {
                                overlay.detach_input_status();
                            }
                        }
                    }
                    Err(error) => {
                        let ready_segments = ready_segment_count(&run_directory);
                        let diagnostic = format!("{error:#}");
                        let fault = shared.run_fault(
                            &worker_session_id,
                            &worker_run_id,
                            recording_fault_spec(&error),
                            Some(&diagnostic),
                        );
                        if let Err(journal_error) =
                            shared
                                .journal
                                .fail_run(&worker_session_id, &worker_run_id, &fault.code)
                        {
                            tracing::error!(
                                error = %journal_error,
                                session_id = %worker_session_id,
                                run_id = %worker_run_id,
                                "failed to persist capture failure to SQLite journal"
                            );
                        }
                        sync_heartbeat_artifact(
                            &shared,
                            &worker_session_id,
                            &worker_run_id,
                        );
                        tracing::error!(
                            error = %error,
                            fault_code = %fault.code,
                            "recording session failed; local diagnostics retained"
                        );
                        shared.update(|inner| {
                            inner.snapshot.state = CollectorState::Faulted;
                            inner.snapshot.fault = Some(fault);
                            if let Some(capture) = &mut inner.snapshot.capture {
                                capture.ready_segments = capture.ready_segments.max(ready_segments);
                            }
                            inner.stop_tx = None;
                            inner.settings_watch = None;
                            inner.input_control = None;
                            inner.input_status = None;
                            inner.started_at = None;
                            inner.finish_as_completed = false;
                            inner.storage_warning_emitted = false;
                        });
                        if let Some(overlay) = &shared.overlay {
                            overlay.detach_input_status();
                        }
                    }
                }
            }) {
            Ok(worker) => worker,
            Err(error) => {
                let _ = self.shared.journal.fail_run(
                    &session_id,
                    &run_id,
                    "CAPTURE_THREAD_START_FAILED",
                );
                let diagnostic = format!("{error:#}");
                let fault = self.shared.run_fault(
                    &session_id,
                    &run_id,
                    FaultSpec {
                        code: "CAPTURE_THREAD_START_FAILED",
                        message: "无法启动采集线程。",
                        action: "重启客户端后重新绑定游戏窗口。",
                        retryable: true,
                    },
                    Some(&diagnostic),
                );
                self.shared.update(|inner| {
                    inner.snapshot.state = CollectorState::Faulted;
                    inner.snapshot.fault = Some(fault);
                    inner.stop_tx = None;
                            inner.settings_watch = None;
                    inner.input_control = None;
                    inner.input_status = None;
                    inner.started_at = None;
                    inner.storage_warning_emitted = false;
                });
                if let Some(overlay) = &self.overlay {
                    overlay.detach_input_status();
                }
                return Err(error).context("failed to spawn capture pipeline");
            }
        };
        self.shared.inner.lock().worker = Some(worker);
        if preflight_warning {
            let remaining_seconds = self
                .shared
                .inner
                .lock()
                .snapshot
                .storage
                .estimated_remaining_seconds;
            emit_collector_notification(
                &self.shared.app,
                &disk_warning_notification(&self.shared, remaining_seconds),
            );
        }
        Ok(self.shared.snapshot())
    }

    pub fn add_marker(
        &self,
        kind: MarkerKind,
        request_id: &str,
        marker_qpc: Option<i64>,
    ) -> Result<MarkerAudit, CollectorFault> {
        if request_id.trim().is_empty() || request_id.len() > 128 {
            return Err(self.marker_fault(
                "MARKER_REQUEST_INVALID",
                "Marker 请求标识无效。",
                "重新触发一次 marker；客户端不会写入无幂等标识的记录。",
                false,
                None,
            ));
        }

        let existing = self
            .shared
            .journal
            .marker_by_request_id(request_id)
            .map_err(|error| {
                self.marker_fault(
                    "MARKER_JOURNAL_READ_FAILED",
                    "无法确认 marker 请求是否已记录。",
                    "保留当前会话，导出诊断后使用同一请求标识重试。",
                    true,
                    Some(&error),
                )
            })?;
        if let Some(existing) = existing {
            if existing.kind != kind.as_str() {
                return Err(self.marker_fault(
                    "MARKER_REQUEST_CONFLICT",
                    "Marker 请求标识已用于其他语义。",
                    "生成新的请求标识后重试，不会覆盖原 marker。",
                    false,
                    None,
                ));
            }
            let replay_conflicts = {
                let inner = self.shared.inner.lock();
                marker_replay_conflicts_with_active_run(
                    inner.snapshot.state,
                    inner
                        .snapshot
                        .capture
                        .as_ref()
                        .map(|capture| (capture.session_id.as_str(), capture.run_id.as_str())),
                    &existing,
                )
            };
            if replay_conflicts {
                return Err(self.marker_fault(
                    "MARKER_REQUEST_CONFLICT",
                    "Marker 请求标识属于其他 Recording run。",
                    "生成新的请求标识后重试；不会把旧 run 的 marker 冒充为本次打点。",
                    false,
                    None,
                ));
            }
            let artifact_synced = self.sync_marker_artifact(&existing);
            self.record_marker_diagnostic(&existing, artifact_synced, true);
            return Ok(existing);
        }

        let active_run = {
            let inner = self.shared.inner.lock();
            marker_state_allows_new(inner.snapshot.state)
                .then(|| inner.snapshot.capture.as_ref())
                .flatten()
                .map(|capture| (capture.session_id.clone(), capture.run_id.clone()))
        };
        let (session_id, run_id) = active_run.ok_or_else(|| {
            self.marker_fault(
                "MARKER_NO_ACTIVE_RUN",
                "当前没有可打点的 Recording run。",
                "等待状态进入正在录制后再按 Ctrl+Alt+F9；暂停、重绑定和封口期间不会补记 marker。",
                true,
                None,
            )
        })?;
        let qpc = match marker_qpc {
            Some(qpc) => qpc,
            None => QpcClock::new().and_then(QpcClock::now).map_err(|error| {
                self.marker_fault(
                    "MARKER_CLOCK_FAILED",
                    "无法读取 marker 的 QPC 时间。",
                    "本次 marker 未写入；确认系统计时器可用后重试。",
                    true,
                    Some(&error),
                )
            })?,
        };
        let marker = self
            .shared
            .journal
            .record_marker(&MarkerWrite {
                request_id,
                session_id: &session_id,
                run_id: &run_id,
                kind,
                qpc,
            })
            .map_err(|error| {
                self.marker_fault(
                    "MARKER_JOURNAL_WRITE_FAILED",
                    "Marker 未能写入本地事实库。",
                    "本次 marker 未成立；保留会话并按详情编号排查后重试。",
                    true,
                    Some(&error),
                )
            })?;
        let artifact_synced = self.sync_marker_artifact(&marker);
        self.record_marker_diagnostic(&marker, artifact_synced, false);
        Ok(marker)
    }

    pub fn add_hotkey_marker(&self, qpc: i64) -> Result<MarkerAudit, CollectorFault> {
        let request_id = next_marker_request_id("hotkey", qpc);
        self.add_marker(MarkerKind::Manual, &request_id, Some(qpc))
    }

    /// docs/65 J-24：主控热键（Ctrl+Alt+F8 / F10）注册失败此前只写 tracing——采集页
    /// 仍写死热键提示，采集员在全屏游戏里按键没反应且无从得知。启动期 WebView 大
    /// 概率还没挂上事件监听，直接 emit 会丢，所以入队由首个 `collector_get_snapshot`
    /// 命令代发；诊断事件照常立即落盘，导出诊断里始终可见。
    pub fn report_control_hotkeys_unavailable(&self, diagnostic: &str) {
        let notification = self.shared.diagnostics.create_notification(
            "CONTROL_HOTKEYS_UNAVAILABLE",
            "全局热键注册失败：Ctrl+Alt+F8 / F10 在本机不可用。",
            "关闭占用该组合键的程序后重启 DataMiner；期间请用主窗口按钮或托盘菜单暂停与结束录制。",
            true,
            DiagnosticCorrelation::default(),
            Some(diagnostic),
        );
        self.shared.startup_notifications.lock().push(notification);
    }

    pub fn report_marker_hotkey_unavailable(&self, error: &windows::core::Error) {
        let diagnostic = error.to_string();
        let notification = self.shared.diagnostics.create_notification(
            "MARKER_HOTKEY_UNAVAILABLE",
            "Ctrl+Alt+F9 marker 热键被其他程序占用。",
            "关闭占用该组合键的程序并重启 DataMiner；暂停与结束热键仍可用。",
            true,
            DiagnosticCorrelation::default(),
            Some(&diagnostic),
        );
        // 与 CONTROL_HOTKEYS_UNAVAILABLE 同一时序问题（注册发生在 setup 期），
        // 同走启动队列，由首个 collector_get_snapshot 命令代发。
        self.shared.startup_notifications.lock().push(notification);
    }

    fn sync_marker_artifact(&self, marker: &MarkerAudit) -> bool {
        match self
            .shared
            .markers
            .publish_session(&self.shared.journal, &marker.session_id)
        {
            Ok(_) => true,
            Err(error) => {
                let diagnostic = format!("{error:#}");
                let notification = self.shared.diagnostics.create_notification(
                    "MARKER_ARTIFACT_PENDING_RECOVERY",
                    "Marker 已写入 SQLite，但 markers.jsonl 尚未同步。",
                    "不要重复打点；重启时会从 journal 原子重建 session marker artifact。",
                    true,
                    DiagnosticCorrelation::run(&marker.session_id, &marker.run_id),
                    Some(&diagnostic),
                );
                emit_collector_notification(&self.shared.app, &notification);
                false
            }
        }
    }

    fn record_marker_diagnostic(
        &self,
        marker: &MarkerAudit,
        artifact_synced: bool,
        replayed: bool,
    ) {
        record_diagnostic(
            &self.shared,
            "info",
            DiagnosticCorrelation::run(&marker.session_id, &marker.run_id),
            DiagnosticPayload::MarkerAdded {
                marker_id: marker.marker_id.clone(),
                kind: marker.kind.clone(),
                qpc: marker.qpc,
                run_frame: marker.run_frame,
                session_frame: marker.session_frame,
                video_time_us: marker.video_time_us,
                artifact_synced,
                replayed,
            },
        );
    }

    fn marker_fault(
        &self,
        code: &str,
        message: &str,
        action: &str,
        retryable: bool,
        error: Option<&anyhow::Error>,
    ) -> CollectorFault {
        let diagnostic = error.map(|error| format!("{error:#}"));
        self.shared.diagnostics.create_fault(
            code,
            message,
            action,
            retryable,
            self.shared.current_correlation(),
            diagnostic.as_deref(),
        )
    }

    pub fn export_diagnostics(
        &self,
        session_id: Option<&str>,
    ) -> Result<DiagnosticExportResult, CollectorFault> {
        self.try_export_diagnostics(session_id).map_err(|error| {
            let diagnostic = format!("{error:#}");
            let correlation = session_id.map_or_else(
                || self.shared.current_correlation(),
                |session_id| DiagnosticCorrelation {
                    session_id: Some(session_id.into()),
                    ..DiagnosticCorrelation::default()
                },
            );
            self.shared.diagnostics.create_fault(
                "DIAGNOSTIC_EXPORT_FAILED",
                "诊断导出失败。",
                "确认本地应用数据目录可写后重试；录制与已有 ready 数据不受影响。",
                true,
                correlation,
                Some(&diagnostic),
            )
        })
    }

    fn try_export_diagnostics(
        &self,
        requested_session_id: Option<&str>,
    ) -> anyhow::Result<DiagnosticExportResult> {
        self.reap_finished_worker();
        if let Some(session_id) = requested_session_id {
            ensure!(
                self.shared.journal.session_exists(session_id)?,
                "requested diagnostic session does not exist"
            );
        }
        self.shared.export_diagnostics(requested_session_id)
    }

    pub fn session_history(&self) -> Result<Vec<SessionHistorySummary>, CollectorFault> {
        self.shared
            .journal
            .list_sessions()
            .map_err(|error| self.history_fault("SESSION_HISTORY_READ_FAILED", &error))
    }

    pub fn request_session_upload(
        &self,
        session_id: &str,
    ) -> Result<SessionHistorySummary, CollectorFault> {
        let summary = self
            .shared
            .journal
            .request_session_upload(session_id)
            .map_err(|error| {
                self.history_session_fault("SESSION_UPLOAD_REQUEST_FAILED", session_id, &error)
            })?;
        record_diagnostic(
            &self.shared,
            "info",
            DiagnosticCorrelation {
                session_id: Some(session_id.into()),
                ..DiagnosticCorrelation::default()
            },
            DiagnosticPayload::R2UploadProgress {
                scope: "session",
                phase: "manual_upload_requested",
                state: "requested",
                attempt_count: 0,
                artifact_kind: None,
                artifact_count: Some(summary.upload_job_segments),
                part_number: None,
                part_count: None,
                byte_size: None,
                error_code: None,
                retry_delay_ms: None,
                http_status: None,
                server_code: None,
                server_message: None,
                request_id: None,
            },
        );
        Ok(summary)
    }

    pub fn session_history_detail(
        &self,
        session_id: &str,
    ) -> Result<SessionHistoryDetail, CollectorFault> {
        self.shared
            .journal
            .session_detail(session_id)
            .map_err(|error| {
                self.history_session_fault("SESSION_HISTORY_READ_FAILED", session_id, &error)
            })
    }

    pub fn data_model_status(
        &self,
        session_id: &str,
    ) -> Result<Option<DataModelJobStatus>, CollectorFault> {
        self.shared.data_model.status(session_id).map_err(|error| {
            self.history_session_fault("DATA_MODEL_STATUS_READ_FAILED", session_id, &error)
        })
    }

    pub fn open_session_directory(&self, session_id: &str) -> Result<(), CollectorFault> {
        (|| -> anyhow::Result<()> {
            ensure!(
                self.shared.journal.session_exists(session_id)?,
                "session does not exist"
            );
            let directory = session_history::session_directory(
                &self.artifact_root,
                &self.output_root,
                session_id,
            )?;
            shell::open_directory(&directory)
        })()
        .map_err(|error| {
            self.history_session_fault("SESSION_DIRECTORY_OPEN_FAILED", session_id, &error)
        })
    }

    pub fn open_data_model_directory(&self, session_id: &str) -> Result<(), CollectorFault> {
        self.shared
            .data_model
            .with_session_artifact_operation(session_id, || {
                let directory = self.shared.data_model.output_directory(session_id)?;
                shell::open_directory(&directory)
            })
            .map_err(|error| {
                self.history_session_fault("DATA_MODEL_DIRECTORY_OPEN_FAILED", session_id, &error)
            })
    }

    pub fn delete_session(
        &self,
        session_id: &str,
        confirmation: &str,
    ) -> Result<SessionDeletionResult, CollectorFault> {
        let result = (|| -> anyhow::Result<SessionDeletionResult> {
            let inner = self.shared.inner.lock();
            let is_current = inner
                .snapshot
                .capture
                .as_ref()
                .is_some_and(|capture| capture.session_id == session_id);
            ensure!(
                !(is_current
                    && matches!(
                        inner.snapshot.state,
                        CollectorState::Starting
                            | CollectorState::Recording
                            | CollectorState::Pausing
                            | CollectorState::Paused
                            | CollectorState::Stopping
                    )),
                "the current recording or resumable session cannot be deleted"
            );
            drop(inner);
            self.shared
                .markers
                .with_session_artifact_operation(session_id, || {
                    self.shared
                        .data_model
                        .with_session_artifact_operation(session_id, || {
                            // The deletion transaction owns the terminal decision: a successful
                            // journal commit removes the job through the session FK. Cancelling it
                            // here would survive a staging rollback and strand an otherwise intact
                            // session, so the lease only blocks concurrent file assembly.
                            session_history::delete_session(
                                &self.shared.journal,
                                &self.artifact_root,
                                &self.output_root,
                                session_id,
                                confirmation,
                            )
                        })
                })
        })()
        .map_err(|error| self.history_session_fault("SESSION_DELETE_FAILED", session_id, &error))?;
        self.shared.update(|inner| {
            if inner
                .snapshot
                .capture
                .as_ref()
                .is_some_and(|capture| capture.session_id == result.session_id)
            {
                inner.snapshot.capture = None;
                inner.snapshot.fault = None;
                inner.snapshot.rebind_required = false;
                inner.snapshot.rebind_game_id = None;
                inner.snapshot.state = if inner.bound.is_some() {
                    CollectorState::Bound
                } else {
                    CollectorState::Idle
                };
                inner.session_id = None;
                inner.next_run_seq = 0;
                inner.next_session_frame = 0;
                inner.last_request_id = None;
                inner.finish_as_completed = false;
                inner.storage_warning_emitted = false;
                inner.rebind_required = None;
            }
        });
        record_diagnostic(
            &self.shared,
            "info",
            DiagnosticCorrelation {
                session_id: Some(result.session_id.clone()),
                ..DiagnosticCorrelation::default()
            },
            DiagnosticPayload::SessionDeleted {
                deletion_id: result.deletion_id.clone(),
                artifacts_removed: result.artifacts_removed,
            },
        );
        Ok(result)
    }

    pub fn stop(&self, _reason: &str) -> anyhow::Result<CollectorSnapshot> {
        let state = self.snapshot().state;
        if state == CollectorState::Paused {
            self.join_terminal_worker()?;
            if let Some(session_id) = self.shared.inner.lock().session_id.clone() {
                self.shared.journal.complete_session(&session_id)?;
                self.shared.data_model.wake();
            }
            return Ok(self.shared.update(|inner| {
                inner.snapshot.state = CollectorState::Completed;
                inner.snapshot.fault = None;
                inner.snapshot.rebind_required = false;
                inner.snapshot.rebind_game_id = None;
                inner.finish_as_completed = false;
                inner.rebind_required = None;
            }));
        }
        if state == CollectorState::Pausing {
            return Ok(self.shared.update(|inner| {
                inner.snapshot.state = CollectorState::Stopping;
                inner.finish_as_completed = true;
            }));
        }
        #[cfg(feature = "depth-lab")]
        let depth_stop_target = {
            let status = crate::services::depth_lab::runtime_status();
            (status.enabled && status.active_identity.is_some())
                .then(|| self.shared.inner.lock().bound.clone())
                .flatten()
        };
        let cutoff_qpc = QpcClock::new()?.now()?;
        #[cfg(all(feature = "depth-lab", any(feature = "dev-catalog", test)))]
        let cp2077_stop = crate::services::cp2077_trial::request_main_collector_stop();
        let snapshot = self.request_cutoff(
            RecordingStopReason::Manual,
            cutoff_qpc,
            CollectorState::Stopping,
            true,
        )?;
        #[cfg(all(feature = "depth-lab", any(feature = "dev-catalog", test)))]
        cp2077_stop.map_err(anyhow::Error::msg)?;
        #[cfg(feature = "depth-lab")]
        if let Some(target) = depth_stop_target
            && let Err(error) = self.tracker.focus_revalidated(&target)
        {
            tracing::warn!(
                error = %error,
                "manual depth stop could not resume game Presents for GenerationSealed"
            );
        }
        Ok(snapshot)
    }

    pub fn pause(&self, cutoff_qpc: Option<i64>) -> anyhow::Result<CollectorSnapshot> {
        let state = self.snapshot().state;
        if matches!(state, CollectorState::Pausing | CollectorState::Paused) {
            return Ok(self.snapshot());
        }
        if !matches!(state, CollectorState::Starting | CollectorState::Recording) {
            return Ok(self.snapshot());
        }
        #[cfg(all(feature = "depth-lab", any(feature = "dev-catalog", test)))]
        ensure!(
            crate::services::cp2077_trial::status().phase
                != crate::services::cp2077_trial::TrialPhase::Recording,
            "CP2077 five-stream main runs cannot pause or resume; use the main Stop button"
        );
        let cutoff_qpc = cutoff_qpc.map_or_else(|| QpcClock::new()?.now(), Ok)?;
        self.request_cutoff(
            RecordingStopReason::Pause,
            cutoff_qpc,
            CollectorState::Pausing,
            false,
        )
    }

    pub fn resume(&self, request_id: &str) -> anyhow::Result<CollectorSnapshot> {
        let snapshot = self.snapshot();
        if matches!(
            snapshot.state,
            CollectorState::Starting | CollectorState::Recording
        ) {
            return Ok(snapshot);
        }
        ensure!(
            snapshot.state == CollectorState::Paused,
            "collector must be paused before resume"
        );
        self.start(request_id)
    }

    pub fn toggle_pause_resume(
        &self,
        cutoff_qpc: Option<i64>,
    ) -> anyhow::Result<CollectorSnapshot> {
        match self.snapshot().state {
            CollectorState::Starting | CollectorState::Recording => {
                if let Some(overlay) = &self.overlay {
                    overlay.set_visible(true);
                }
                self.pause(cutoff_qpc)
            }
            CollectorState::Paused => self.resume(&next_control_request_id("hotkey-resume")?),
            _ => Ok(self.snapshot()),
        }
    }

    fn request_cutoff(
        &self,
        reason: RecordingStopReason,
        cutoff_qpc: i64,
        next_state: CollectorState,
        finish_as_completed: bool,
    ) -> anyhow::Result<CollectorSnapshot> {
        let (stop_tx, input_control) = {
            let inner = self.shared.inner.lock();
            match inner.snapshot.state {
                CollectorState::Starting | CollectorState::Recording => {
                    (inner.stop_tx.clone(), inner.input_control.clone())
                }
                CollectorState::Pausing
                | CollectorState::Paused
                | CollectorState::Stopping
                | CollectorState::Completed
                | CollectorState::Faulted => {
                    return Ok(inner.snapshot.clone());
                }
                _ => return Ok(inner.snapshot.clone()),
            }
        };
        let input_gate_result = input_control.map(|control| control.close_gate());
        if let Some(stop_tx) = stop_tx {
            stop_tx
                .try_send(RecordingStopSignal { reason, cutoff_qpc })
                .context("capture pipeline control channel is unavailable")?;
        }
        if let Some(result) = input_gate_result {
            result?;
        }
        Ok(self.shared.update(|inner| {
            inner.snapshot.state = next_state;
            inner.finish_as_completed = finish_as_completed;
            if let Some(capture) = &mut inner.snapshot.capture {
                capture.pressed_contract_keys.clear();
                capture.mouse_buttons.clear();
            }
        }))
    }

    pub fn overlay_snapshot(&self) -> OverlaySnapshot {
        self.reap_finished_worker();
        let inner = self.shared.inner.lock();
        let mut overlay = inner.snapshot.overlay_snapshot();
        if matches!(
            inner.snapshot.state,
            CollectorState::Starting | CollectorState::Recording
        ) && let Some(status) = &inner.input_status
        {
            let input = status.snapshot();
            overlay.pressed_contract_keys = input.pressed_contract_keys;
            overlay.mouse_buttons = input.mouse_buttons;
        }
        overlay
    }

    pub fn set_overlay_visible(&self, visible: bool) -> CollectorSnapshot {
        if let Some(overlay) = &self.overlay {
            overlay.set_visible(visible);
        }
        self.snapshot()
    }

    pub fn overlay_webview_ready(&self) -> anyhow::Result<OverlaySnapshot> {
        let overlay = self.overlay.as_ref().context("Overlay is unavailable")?;
        overlay.webview_ready()?;
        Ok(self.overlay_snapshot())
    }

    pub fn overlay_heartbeat(&self) {
        if let Some(overlay) = &self.overlay {
            overlay.heartbeat();
        }
    }

    pub fn report_overlay_client_fault(&self) {
        if let Some(overlay) = &self.overlay {
            overlay.report_client_fault();
        }
    }

    pub fn route_overlay_action(&self, action: OverlayAction) -> anyhow::Result<CollectorSnapshot> {
        match action {
            OverlayAction::Stop => self.stop("overlay"),
            OverlayAction::ShowMainWindow => {
                if matches!(
                    self.snapshot().state,
                    CollectorState::Starting | CollectorState::Recording
                ) {
                    self.pause(None)?;
                }
                if let Some(window) = self.shared.app.get_webview_window("main") {
                    window.show()?;
                    window.set_focus()?;
                }
                Ok(self.snapshot())
            }
            OverlayAction::PauseResume => match self.snapshot().state {
                CollectorState::Paused => self.resume(&next_control_request_id("overlay-resume")?),
                CollectorState::Starting | CollectorState::Recording => self.pause(None),
                _ => Ok(self.snapshot()),
            },
            OverlayAction::AddMarker => {
                let qpc = QpcClock::new()?.now()?;
                let request_id = next_marker_request_id("semantic", qpc);
                self.add_marker(MarkerKind::Manual, &request_id, Some(qpc))
                    .map(|_| self.snapshot())
                    .map_err(|fault| anyhow::anyhow!("{}: {}", fault.code, fault.message))
            }
        }
    }

    fn ensure_not_recording(&self) -> anyhow::Result<()> {
        self.reap_finished_worker();
        let inner = self.shared.inner.lock();
        let state = inner.snapshot.state;
        let rebind_allowed = state == CollectorState::Paused && inner.rebind_required.is_some();
        ensure!(
            inner.start_reservation.is_none()
                && (rebind_allowed
                    || !matches!(
                        state,
                        CollectorState::Preflighting
                            | CollectorState::Starting
                            | CollectorState::Recording
                            | CollectorState::Pausing
                            | CollectorState::Paused
                            | CollectorState::Stopping
                    )),
            "window discovery/binding is locked while capture is active"
        );
        Ok(())
    }

    fn reap_finished_worker(&self) {
        let worker = {
            let mut inner = self.shared.inner.lock();
            if inner.worker.as_ref().is_some_and(JoinHandle::is_finished) {
                inner.worker.take()
            } else {
                None
            }
        };
        if let Some(worker) = worker
            && worker.join().is_err()
        {
            let fault = self.shared.fault(
                FaultSpec {
                    code: "CAPTURE_THREAD_PANIC",
                    message: "采集线程异常退出。",
                    action: "重启客户端并重新绑定游戏窗口。",
                    retryable: true,
                },
                Some("capture worker panicked"),
            );
            self.shared.update(|inner| {
                inner.snapshot.state = CollectorState::Faulted;
                inner.snapshot.fault = Some(fault);
            });
        }
    }

    fn join_terminal_worker(&self) -> anyhow::Result<()> {
        let worker = {
            let mut inner = self.shared.inner.lock();
            ensure!(
                !matches!(
                    inner.snapshot.state,
                    CollectorState::Starting
                        | CollectorState::Recording
                        | CollectorState::Pausing
                        | CollectorState::Stopping
                ),
                "capture worker is still active"
            );
            inner.worker.take()
        };
        if let Some(worker) = worker {
            worker
                .join()
                .map_err(|_| anyhow::anyhow!("capture worker panicked before the next run"))?;
        }
        Ok(())
    }

    fn set_fault(&self, code: &'static str, error: &anyhow::Error, retryable: bool) {
        self.set_fault_spec(
            FaultSpec {
                code,
                message: "采集操作失败。",
                action: "修复后刷新窗口并重新绑定。",
                retryable,
            },
            error,
        );
    }

    fn set_fault_spec(&self, spec: FaultSpec, error: &anyhow::Error) {
        tracing::warn!(code = spec.code, "collector operation failed");
        let diagnostic = format!("{error:#}");
        let fault = self.shared.fault(spec, Some(&diagnostic));
        self.shared.update(|inner| {
            inner.snapshot.state = CollectorState::Faulted;
            inner.snapshot.fault = Some(fault);
        });
    }

    fn set_rebind_fault(&self, code: &str, error: &anyhow::Error) {
        let diagnostic = format!("{error:#}");
        let fault = self.shared.diagnostics.create_fault(
            code,
            "新窗口未通过当前会话的身份复核。",
            "保持合成窗口为原 profile 的分辨率与程序身份，重新扫描并绑定；也可直接结束会话。",
            true,
            self.shared.current_correlation(),
            Some(&diagnostic),
        );
        self.shared.update(|inner| {
            inner.snapshot.state = CollectorState::Paused;
            inner.snapshot.rebind_required = true;
            inner.snapshot.fault = Some(fault);
        });
    }

    fn history_fault(&self, code: &str, error: &anyhow::Error) -> CollectorFault {
        let diagnostic = format!("{error:#}");
        self.shared.diagnostics.create_fault(
            code,
            "本地会话操作失败。",
            "保留本地文件，按详情编号导出诊断后重试。",
            true,
            self.shared.current_correlation(),
            Some(&diagnostic),
        )
    }

    fn history_session_fault(
        &self,
        code: &str,
        session_id: &str,
        error: &anyhow::Error,
    ) -> CollectorFault {
        let diagnostic = format!("{error:#}");
        self.shared.diagnostics.create_fault(
            code,
            "本地会话操作失败。",
            "保留本地文件，按详情编号导出诊断后重试。",
            true,
            DiagnosticCorrelation {
                session_id: Some(session_id.into()),
                ..DiagnosticCorrelation::default()
            },
            Some(&diagnostic),
        )
    }
}

fn game_requires_depth(game_id: &str) -> bool {
    let _ = game_id;
    false
}

const fn depth_feature_compiled_in() -> bool {
    cfg!(feature = "depth-lab")
}

fn timed_control_phase<T>(
    shared: &Shared,
    operation: &'static str,
    phase: &'static str,
    task: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let started_at = Instant::now();
    let result = task();
    let elapsed_ms = u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
    let success = result.is_ok();
    tracing::info!(
        operation,
        phase,
        elapsed_ms,
        success,
        "collector control phase completed"
    );
    record_diagnostic(
        shared,
        if success { "info" } else { "warning" },
        DiagnosticCorrelation::default(),
        DiagnosticPayload::ControlPhaseCompleted {
            operation: operation.into(),
            phase: phase.into(),
            outcome: if success { "passed" } else { "failed" }.into(),
            elapsed_ms,
        },
    );
    result
}

fn record_diagnostic(
    shared: &Shared,
    level: &'static str,
    correlation: DiagnosticCorrelation,
    payload: DiagnosticPayload,
) {
    if let Err(error) = shared
        .diagnostics
        .record(level, None, correlation, payload, false)
    {
        tracing::warn!(error = %error, "failed to persist structured diagnostic event");
    }
}

fn record_run_stopped(shared: &Shared, outcome: &capture_core::recording::RecordingOutcome) {
    record_diagnostic(
        shared,
        "info",
        DiagnosticCorrelation::run(&outcome.session_id, &outcome.run_id),
        DiagnosticPayload::RunStopped {
            stop_reason: stop_reason_label(outcome.stop_reason).into(),
            frame_count: outcome.frame_count,
            ready_segments: outcome.segment_count,
            input_event_count: outcome.input_event_count,
            input_queue_overflows: outcome.input_queue_overflows,
            wgc_callback_count: outcome.wgc_callback_count,
            watermark_decoded_observations: outcome.watermark_decoded_observations,
            watermark_rejected_observations: outcome.watermark_rejected_observations,
            watermark_integrity_rejections: outcome.watermark_integrity_rejections,
            watermark_ambiguous_rejections: outcome.watermark_ambiguous_rejections,
            max_consecutive_watermark_rejections: outcome.max_consecutive_watermark_rejections,
            watermark_stale_observations: outcome.watermark_stale_observations,
            max_watermark_stale_lag_presents: outcome.max_watermark_stale_lag_presents,
        },
    );
}

fn stop_reason_label(reason: RecordingStopReason) -> &'static str {
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
        RecordingStopReason::InputFault => "input_fault",
        RecordingStopReason::SettingsChanged => "settings_changed",
        RecordingStopReason::CatalogRetired => "catalog_retired",
        RecordingStopReason::DepthFrameHealth => "depth_frame_health",
    }
}

fn storage_state_label(state: StorageState) -> &'static str {
    match state {
        StorageState::Unknown => "unknown",
        StorageState::Ready => "ready",
        StorageState::Warning => "warning",
        StorageState::Critical => "critical",
        StorageState::Unavailable => "unavailable",
    }
}

fn validate_rebind_identity(
    expected: &RebindIdentity,
    observed: &RebindIdentity,
) -> anyhow::Result<()> {
    ensure!(
        expected.game_id == observed.game_id
            && expected.executable_sha256 == observed.executable_sha256
            && expected.session_contract == observed.session_contract
            && expected.catalog_id == observed.catalog_id
            && expected.catalog_version == observed.catalog_version
            && expected.catalog_hash == observed.catalog_hash
            && expected.device_profile_id == observed.device_profile_id
            && expected.device_profile_version == observed.device_profile_version
            && expected.device_profile_hash == observed.device_profile_hash
            && expected.recording_profile_id == observed.recording_profile_id
            && expected.recording_profile_version == observed.recording_profile_version
            && expected.recording_profile_hash == observed.recording_profile_hash
            && expected.game_profile_version == observed.game_profile_version
            && expected.game_profile_hash == observed.game_profile_hash
            && expected.recording_profile_id == observed.recording_profile_id
            && expected.recording_profile_version == observed.recording_profile_version
            && expected.recording_profile_hash == observed.recording_profile_hash
            && expected.mapping_id == observed.mapping_id
            && expected.mapping_version == observed.mapping_version
            && expected.mapping_hash == observed.mapping_hash,
        "new window does not match the prior run catalog/profile/mapping/executable identity"
    );
    ensure!(
        expected.target_id != observed.target_id,
        "window rebind must select a newly discovered target identity"
    );
    Ok(())
}

fn clean_stop_state(reason: RecordingStopReason) -> CollectorState {
    match reason {
        RecordingStopReason::Manual | RecordingStopReason::DiskLow => CollectorState::Completed,
        RecordingStopReason::Pause
        | RecordingStopReason::ForegroundLost
        | RecordingStopReason::WindowMinimized
        | RecordingStopReason::WindowDestroyed
        | RecordingStopReason::GeometryChanged
        // 配置变更与几何变更同组：run 安全封口进 Paused，重新对表后可恢复。
        | RecordingStopReason::SettingsChanged => CollectorState::Paused,
        RecordingStopReason::SourceUnavailable
        | RecordingStopReason::InputFault
        | RecordingStopReason::DiskStatusUnavailable
        | RecordingStopReason::CatalogRetired
        // 深度连续异常**不能**归到 Completed/Paused：那两档都在暗示「接着录就是了」，
        // 而 130 号 §6.5 的处置恰恰相反——停下来找人，别再废一次（也别冲掉现场）。
        | RecordingStopReason::DepthFrameHealth => CollectorState::Faulted,
    }
}

/// docs/65 J-23：四种自动暂停此前一律只显示「已暂停」——采集员 Alt-Tab 一下录制
/// 就停了，不知道为什么、也不知道 SettingsChanged 连带作废了该 run 的视角标定。
/// 手动暂停（Pause）不通知：那是用户自己按的；WindowDestroyed 不在此处：它已有
/// WINDOW_REBIND_REQUIRED 专属通知与常驻横幅。
fn auto_pause_notification_spec(
    reason: RecordingStopReason,
) -> Option<(&'static str, &'static str, &'static str)> {
    match reason {
        RecordingStopReason::ForegroundLost => Some((
            "RECORDING_PAUSED_FOREGROUND_LOST",
            "游戏窗口失去前台（例如 Alt+Tab），录制已自动暂停，当前 run 已安全封口。",
            "回到游戏窗口后点恢复或按 Ctrl+Alt+F8，同一 session 会以新 run 继续。",
        )),
        RecordingStopReason::WindowMinimized => Some((
            "RECORDING_PAUSED_WINDOW_MINIMIZED",
            "游戏窗口被最小化，录制已自动暂停，当前 run 已安全封口。",
            "还原游戏窗口后恢复录制，同一 session 会以新 run 继续。",
        )),
        RecordingStopReason::GeometryChanged => Some((
            "RECORDING_PAUSED_GEOMETRY_CHANGED",
            "游戏窗口尺寸或位置发生变化，录制已自动暂停，当前 run 已安全封口。",
            "恢复到绑定时的窗口模式与客户区分辨率后再恢复录制。",
        )),
        RecordingStopReason::SettingsChanged => Some((
            "RECORDING_PAUSED_SETTINGS_CHANGED",
            "检测到游戏设置文件变化，录制已自动暂停；本 run 的视角标定证据已随之失效。",
            "核对游戏设置未偏离手册要求；如动过视角/灵敏度相关设置，恢复前请重新抽检标定。",
        )),
        _ => None,
    }
}

fn should_report_recording_failure(
    previous_state: CollectorState,
    next_state: CollectorState,
    recording_run_was_active: bool,
) -> bool {
    recording_run_was_active
        && previous_state != CollectorState::Faulted
        && next_state == CollectorState::Faulted
}

fn marker_state_allows_new(state: CollectorState) -> bool {
    state == CollectorState::Recording
}

fn marker_replay_conflicts_with_active_run(
    state: CollectorState,
    active_run: Option<(&str, &str)>,
    existing: &MarkerAudit,
) -> bool {
    state == CollectorState::Recording
        && active_run.is_some_and(|(session_id, run_id)| {
            session_id != existing.session_id.as_str() || run_id != existing.run_id.as_str()
        })
}

fn journal_terminal_state(state: CollectorState) -> RunTerminalState {
    match state {
        CollectorState::Paused => RunTerminalState::Paused,
        CollectorState::Completed => RunTerminalState::Completed,
        CollectorState::Faulted => RunTerminalState::Faulted,
        _ => unreachable!("capture outcome produced a non-terminal collector state"),
    }
}

fn terminal_fault_spec(
    reason: RecordingStopReason,
    terminal_state: CollectorState,
) -> Option<FaultSpec> {
    if terminal_state != CollectorState::Faulted {
        return None;
    }
    Some(match reason {
        RecordingStopReason::DiskStatusUnavailable => FaultSpec {
            code: "DISK_SPACE_UNAVAILABLE",
            message: "录制中无法继续确认本地磁盘剩余空间，采集已安全停止。",
            action: "确认磁盘在线且采集目录可访问，再重新绑定并开始录制。",
            retryable: true,
        },
        RecordingStopReason::CatalogRetired => FaultSpec {
            code: "CATALOG_GENERATION_RETIRED",
            message: "当前游戏的目录身份或 profile 已被收紧，录制已安全封口。",
            action: "等待新目录激活后重新扫描、绑定并创建新 session；旧 session 禁止恢复。",
            retryable: false,
        },
        // `retryable: false`：连续异常是系统性问题，重试只会再废一次（130 号 §6.5）。
        // 带真实连续帧数的那句话由检测时的通知给（`apply_depth_health_abort`）——
        // FaultSpec 只能装 `&'static str`，塞不进那个数字。
        RecordingStopReason::DepthFrameHealth => FaultSpec {
            code: "DEPTH_FRAME_HEALTH_ABORT",
            message: "深度采集连续多帧没有有效数据，录制已自动停止并保留了现场。",
            action: "请截图本页并联系工程，先不要继续录制；现场文件已随本次采集保存。",
            retryable: false,
        },
        _ => FaultSpec {
            code: "CAPTURE_STOPPED_UNSAFE",
            message: "采集链路因不可继续的本地条件停止。",
            action: "检查窗口、输入设备和本地诊断后重新绑定。",
            retryable: true,
        },
    })
}

fn catalog_activation_is_locked(inner: &Inner) -> bool {
    inner.start_reservation.is_some()
        || matches!(
            inner.snapshot.state,
            CollectorState::Preflighting
                | CollectorState::Starting
                | CollectorState::Recording
                | CollectorState::Pausing
                | CollectorState::Paused
                | CollectorState::Stopping
        )
}

#[cfg(not(feature = "local-catalog-only"))]
fn catalog_invalidation_next_state(state: CollectorState) -> Option<CollectorState> {
    match state {
        CollectorState::Starting | CollectorState::Recording => Some(CollectorState::Stopping),
        CollectorState::Paused => Some(CollectorState::Faulted),
        CollectorState::Preflighting | CollectorState::Pausing | CollectorState::Stopping => {
            Some(state)
        }
        CollectorState::Idle
        | CollectorState::Discovering
        | CollectorState::Bound
        | CollectorState::Completed
        | CollectorState::Faulted => None,
    }
}

#[cfg(any(feature = "depth-lab", test))]
fn depth_startup_fault_spec(message: &str) -> FaultSpec {
    // Explicit Present-start gate codes (rate, certification, device, local validation) keep
    // their own guidance; camera/projection evidence keeps its non-retryable classification.
    #[cfg(feature = "depth-lab")]
    {
        let (code, text, action) = crate::services::depth_lab::present_start_failure(message);
        if code != "DEPTH_PROBE_NOT_READY" {
            return FaultSpec {
                code,
                message: text,
                action,
                retryable: true,
            };
        }
    }
    let fault = recording_fault_spec(&anyhow::anyhow!("{message}"));
    match fault.code {
        "DEPTH_CAMERA_EVIDENCE_FAILED" => FaultSpec {
            message: "相机或深度证据没有通过启动校验，录制未开始。",
            ..fault
        },
        "DEPTH_PROJECTION_MISMATCH" => FaultSpec {
            message: "深度投影族没有通过启动校验，录制未开始。",
            ..fault
        },
        _ => FaultSpec {
            code: "DEPTH_PROBE_NOT_READY",
            message: "深度探针没有确认 Ready，录制未开始。",
            action: "保留详情编号并查看诊断，确认失败原因后再重试。",
            retryable: true,
        },
    }
}

fn recording_fault_spec(error: &anyhow::Error) -> FaultSpec {
    if error
        .chain()
        .any(|cause| cause.to_string().contains("JOURNAL_WRITE_FAILED"))
    {
        return FaultSpec {
            code: "JOURNAL_WRITE_FAILED",
            message: "本地采集日志写入或 ready 对账失败，录制已停止。",
            action: "保留本地文件并重启客户端执行恢复核验。",
            retryable: true,
        };
    }

    if error
        .chain()
        .any(|cause| cause.to_string().contains("INPUT_NOT_NEUTRAL"))
    {
        return FaultSpec {
            code: "INPUT_NOT_NEUTRAL",
            message: "录制开始时检测到游戏输入仍处于按下状态。",
            action: "松开映射中的键盘按键和鼠标按键，然后重新开始录制。",
            retryable: true,
        };
    }

    if error.chain().any(|cause| {
        let message = cause.to_string();
        message.contains("WGC callback queue dropped")
            || message.contains("source queue dropped")
            || message.contains("capture queue exhausted")
    }) {
        return FaultSpec {
            code: "CAPTURE_QUEUE_DROPPED",
            message: "画面采集队列发生丢帧，录制已安全停止。",
            action: "保留当前诊断并重新开始；如果重复发生，请停止测试并导出诊断。",
            retryable: true,
        };
    }

    let cause_text = error
        .chain()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n")
        .to_ascii_lowercase();
    if cause_text.contains("player_pose_sampling_failed")
        || cause_text.contains("player pose sample")
        || cause_text.contains("actor-root transform")
    {
        return FaultSpec {
            code: "DEPTH_PLAYER_POSE_FAILED",
            message: "玩家位姿的同帧采样没有通过完整性校验，本档已停止。",
            action: "保留并导出本档诊断；修正采样链后只重试当前认证档，持续失败时核对游戏精确版本。",
            retryable: true,
        };
    }
    if cause_text.contains("depth_finalize_failed") {
        return FaultSpec {
            code: "DEPTH_FINALIZE_FAILED",
            message: "Depth 原始帧已安全落盘，但本地分段收口失败，上传未放行。",
            action: "保留本次 run 并导出诊断；当前版本不会自动恢复 faulted run，请勿发起上传。",
            retryable: true,
        };
    }
    if cause_text.contains("same-present projection family mismatch") {
        return FaultSpec {
            code: "DEPTH_PROJECTION_MISMATCH",
            message: "当前画面的深度投影族尚未认证，正式录制已安全停止。",
            action: "先用性能验证模式保留同帧投影证据；将该投影族验证并加入 profile 后再正式录制。",
            retryable: false,
        };
    }
    if cause_text.contains("depth projection evidence failed")
        || cause_text.contains("camera_production_failed")
    {
        return FaultSpec {
            code: "DEPTH_CAMERA_EVIDENCE_FAILED",
            message: "同帧相机或深度证据没有通过校验，录制未完成。",
            action: "保留本次记录并反馈详情编号；先定位失败原因，再安排重录。",
            retryable: false,
        };
    }
    if [
        "selected-present feedback",
        "generation_armed_ack",
        "stop_before_generation_sealed",
    ]
    .iter()
    .any(|needle| cause_text.contains(needle))
    {
        return FaultSpec {
            code: "DEPTH_FEEDBACK_FAILED",
            message: "Depth 同帧反馈链路异常，当前录制已停止。",
            action: "保留并导出诊断后重新开始本档；若重复发生，请反馈详情编号。",
            retryable: true,
        };
    }
    if cause_text.contains("raw input") {
        return FaultSpec {
            code: "INPUT_PIPELINE_FAILED",
            message: "输入采集链路异常，录制已停止。",
            action: "检查输入设备状态并重启客户端后重新录制。",
            retryable: true,
        };
    }
    if [
        "wgc",
        "capture ingress",
        "capture control",
        "source frame",
        "first cfr slot",
    ]
    .iter()
    .any(|needle| cause_text.contains(needle))
    {
        return FaultSpec {
            code: "CAPTURE_SOURCE_FAILED",
            message: "窗口画面采集链路异常，录制已停止。",
            action: "检查目标窗口和图形驱动后重新绑定并录制。",
            retryable: true,
        };
    }
    if [
        "selected-present attributable gpu allocation",
        "selected-present post-allocation gpu headroom",
        "selected-present run-end gpu headroom",
        "candidate allocation needs",
        "configured vram limit",
        "video-memory budget interface",
        "final gpu headroom",
    ]
    .iter()
    .any(|needle| cause_text.contains(needle))
    {
        return FaultSpec {
            code: "DEPTH_GPU_MEMORY_BUDGET_FAILED",
            message: "Depth 候选环的显存分配或运行结束余量未通过门禁，本档已停止。",
            action: "保留并导出本档诊断，核对其中的显存实际增量和余量；修正后只重试当前认证档。",
            retryable: true,
        };
    }
    if [
        "media foundation",
        "h.264",
        "encoder",
        "nv12",
        "mp4 muxer",
        "video processor",
        "d3d11",
        "gpu",
    ]
    .iter()
    .any(|needle| cause_text.contains(needle))
    {
        return FaultSpec {
            code: "ENCODER_PIPELINE_FAILED",
            message: "GPU 编码或封装链路异常，录制已停止。",
            action: "检查显卡驱动和硬件编码能力后重新录制。",
            retryable: true,
        };
    }
    if [
        "finalize",
        "segment",
        "staging",
        "durable-sync",
        "movefileexw",
    ]
    .iter()
    .any(|needle| cause_text.contains(needle))
    {
        return FaultSpec {
            code: "ARTIFACT_FINALIZATION_FAILED",
            message: "录制分段文件封口失败，录制已停止。",
            action: "保留本地文件和诊断，检查磁盘后重启客户端执行恢复核验。",
            retryable: true,
        };
    }

    FaultSpec {
        code: "CAPTURE_PIPELINE_FAILED",
        message: "采集链路已停止，当前会话不可用于训练。",
        action: "检查游戏窗口、显卡驱动和本地诊断文件后重新绑定。",
        retryable: true,
    }
}

fn inactive_catalog_game_fault_spec(error: &anyhow::Error) -> Option<FaultSpec> {
    error
        .chain()
        .any(|cause| cause.to_string().contains("CATALOG_NO_MATCH:"))
        .then_some(FaultSpec {
            code: "CATALOG_GAME_NOT_ACTIVE",
            message: "所选游戏不在当前活动目录中。",
            action: "等待目录同步完成，或联系管理员发布该游戏后再重新选择。",
            retryable: true,
        })
}

fn ready_segment_count(run_directory: &Path) -> u32 {
    let Ok(entries) = fs::read_dir(run_directory.join("segments")) else {
        return 0;
    };
    let count = entries
        .filter_map(Result::ok)
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.len() == 5
                && name.bytes().all(|byte| byte.is_ascii_digit())
                && [
                    "video.mp4",
                    "events.jsonl.zst",
                    "frames.jsonl.zst",
                    "meta.json",
                    "sha256.json",
                ]
                .iter()
                .all(|file| entry.path().join(file).is_file())
        })
        .count();
    u32::try_from(count).unwrap_or(u32::MAX)
}

fn request_capture_stop_after_recorder_event_failure(
    stop_tx: &Sender<RecordingStopSignal>,
    input_control: &RawInputControl,
    cutoff_qpc: &AtomicI64,
) {
    let cutoff = QpcClock::new().and_then(|clock| clock.now()).unwrap_or(0);
    cutoff_qpc.fetch_min(cutoff, Ordering::AcqRel);
    if let Err(error) = input_control.close_gate() {
        tracing::error!(
            error = %error,
            "failed to close Raw Input gate after recorder event persistence failure"
        );
    }
    match stop_tx.try_send(RecordingStopSignal {
        reason: RecordingStopReason::SourceUnavailable,
        cutoff_qpc: cutoff,
    }) {
        Ok(()) | Err(crossbeam_channel::TrySendError::Full(_)) => {}
        Err(crossbeam_channel::TrySendError::Disconnected(_)) => {
            tracing::debug!(
                "capture pipeline was already disconnected after recorder event persistence failure"
            );
        }
    }
}

fn combine_capture_and_recorder_event_results<T>(
    capture_result: anyhow::Result<T>,
    event_result: anyhow::Result<()>,
) -> anyhow::Result<T> {
    match (capture_result, event_result) {
        (Ok(outcome), Ok(())) => Ok(outcome),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Err(capture_error), Err(event_error)) => Err(event_error.context(format!(
            "capture pipeline also failed while recorder event persistence was stopping it: {capture_error:#}"
        ))),
    }
}

fn apply_recorder_event(
    shared: &Arc<Shared>,
    session_id: &str,
    run_id: &str,
    run_directory: &Path,
    event: RecorderEvent,
    dispatch: RecorderEventDispatch,
) -> anyhow::Result<()> {
    match event {
        RecorderEvent::Started {
            anchor_qpc,
            anchor_at_utc,
            qpc_frequency,
        } => {
            shared
                .journal
                .mark_run_started_with_wall_clock(
                    session_id,
                    run_id,
                    anchor_qpc,
                    anchor_at_utc.as_deref(),
                    qpc_frequency,
                )
                .context("JOURNAL_WRITE_FAILED while starting capture run")?;
            shared
                .journal
                .record_heartbeat(&HeartbeatWrite {
                    session_id,
                    run_id,
                    qpc: anchor_qpc,
                })
                .context("JOURNAL_WRITE_FAILED while recording initial heartbeat")?;
            shared.update(|inner| {
                if inner.snapshot.state == CollectorState::Starting {
                    inner.snapshot.state = CollectorState::Recording;
                }
                inner.started_at = Some(Instant::now());
            });
            let payload = {
                let inner = shared.inner.lock();
                let profile = &inner
                    .bound
                    .as_ref()
                    .context("bound capture profile disappeared while logging run start")?
                    .capture_profile;
                let run_seq = inner
                    .snapshot
                    .capture
                    .as_ref()
                    .context("capture status disappeared while logging run start")?
                    .run_seq;
                DiagnosticPayload::RunStarted {
                    run_seq,
                    capture_backend: profile.capture_backend.clone(),
                    encoder_backend: profile.encoder_backend.clone(),
                    fps_num: profile.fps_num,
                    fps_den: profile.fps_den,
                    width: profile.width,
                    height: profile.height,
                    max_bitrate_bps: profile.max_bitrate_bps,
                    segment_seconds: profile.segment_seconds,
                }
            };
            record_diagnostic(
                shared,
                "info",
                DiagnosticCorrelation::run(session_id, run_id),
                payload,
            );
        }
        RecorderEvent::SegmentStarted {
            segment_index,
            session_frame_start,
        } => {
            shared
                .journal
                .mark_segment_started(session_id, run_id, segment_index, session_frame_start)
                .context("JOURNAL_WRITE_FAILED while starting segment")?;
        }
        RecorderEvent::SegmentFinalizing {
            segment_index,
            session_frame_start,
            frame_count,
        } => {
            shared
                .journal
                .mark_segment_finalizing(
                    session_id,
                    run_id,
                    segment_index,
                    session_frame_start,
                    frame_count,
                )
                .context("JOURNAL_WRITE_FAILED while finalizing segment")?;
        }
        RecorderEvent::Progress(progress) => {
            shared
                .journal
                .record_heartbeat(&HeartbeatWrite {
                    session_id,
                    run_id,
                    qpc: progress.observed_qpc,
                })
                .context("JOURNAL_WRITE_FAILED while recording heartbeat")?;
            apply_progress_and_storage(
                shared,
                session_id,
                run_id,
                &progress,
                run_directory,
                dispatch,
            )?;
            #[cfg(feature = "depth-lab")]
            apply_depth_health_abort(shared, session_id, run_id, run_directory)?;
        }
        RecorderEvent::SegmentReady {
            segment,
            ready_segments,
        } => {
            shared
                .journal
                .mark_segment_ready(session_id, run_id, &segment, run_directory)
                .context("JOURNAL_WRITE_FAILED while committing ready segment")?;
            shared.data_model.wake();
            shared.update(|inner| {
                if let Some(capture) = &mut inner.snapshot.capture {
                    capture.active_segment = segment.index.saturating_add(1);
                    capture.ready_segments = ready_segments;
                    capture.elapsed_ms = inner
                        .started_at
                        .map_or(0, |started| started.elapsed().as_millis() as u64);
                }
            });
            record_diagnostic(
                shared,
                "info",
                DiagnosticCorrelation::segment(session_id, run_id, segment.index),
                DiagnosticPayload::SegmentReady {
                    frame_count: segment.frame_count,
                    event_count: segment.event_count,
                    qpc_start: segment.qpc_start,
                    qpc_end: segment.qpc_end,
                    video_bytes: segment.video_bytes,
                },
            );
        }
    }
    Ok(())
}

fn sync_heartbeat_artifact(shared: &Shared, session_id: &str, run_id: &str) {
    let heartbeats = match shared.journal.heartbeats_for_session(session_id) {
        Ok(heartbeats) => heartbeats,
        Err(error) => {
            tracing::warn!(
                error = %error,
                session_id,
                run_id,
                "failed to load durable heartbeat facts for projection"
            );
            return;
        }
    };
    if heartbeats.is_empty() {
        return;
    }
    let result = shared
        .markers
        .with_session_artifact_operation(session_id, || {
            shared
                .heartbeats
                .publish_session(&shared.journal, session_id)
        });
    if let Err(error) = result {
        let diagnostic = format!("{error:#}");
        let notification = shared.diagnostics.create_notification(
            "HEARTBEAT_ARTIFACT_PENDING_RECOVERY",
            "Heartbeat 已写入 SQLite，但 heartbeat.jsonl 尚未同步。",
            "不要重复录制；重启客户端会从 journal 原子重建 heartbeat artifact。",
            true,
            DiagnosticCorrelation::run(session_id, run_id),
            Some(&diagnostic),
        );
        emit_collector_notification(&shared.app, &notification);
    }
}

fn apply_progress_and_storage(
    shared: &Arc<Shared>,
    session_id: &str,
    run_id: &str,
    progress: &RecordingProgress,
    run_directory: &Path,
    dispatch: RecorderEventDispatch,
) -> anyhow::Result<()> {
    let budget = {
        let inner = shared.inner.lock();
        let target = inner
            .bound
            .as_ref()
            .context("bound capture profile disappeared during storage monitoring")?;
        StorageBudget::from_profile(&target.capture_profile)
    };
    let (storage_status, storage_probe_failed) = match storage::inspect(run_directory, budget) {
        Ok(status) => (status, false),
        Err(error) => {
            tracing::warn!(error = %error, "capture storage capacity probe failed during recording");
            (storage::unavailable(budget), true)
        }
    };
    let requires_stop = matches!(
        storage_status.state,
        StorageState::Critical | StorageState::Unavailable
    );
    let cutoff_qpc = if requires_stop {
        QpcClock::new()?.now()?
    } else {
        0
    };
    let mut stop_control = None;
    let mut notice = None;
    let diagnostic_storage = storage_status.clone();
    shared.update(|inner| {
        update_progress(inner, progress);
        let active = matches!(
            inner.snapshot.state,
            CollectorState::Starting | CollectorState::Recording
        );
        let already_stopping_for_storage = inner.snapshot.storage.stop_requested;
        let mut next_storage = storage_status;
        next_storage.stop_requested = already_stopping_for_storage;

        if active && next_storage.state == StorageState::Warning && !inner.storage_warning_emitted {
            inner.storage_warning_emitted = true;
            notice = Some(DiskNotice::Warning(
                next_storage.estimated_remaining_seconds,
            ));
        }
        if active && requires_stop && !already_stopping_for_storage {
            let reason = if storage_probe_failed {
                RecordingStopReason::DiskStatusUnavailable
            } else {
                RecordingStopReason::DiskLow
            };
            next_storage.stop_requested = true;
            inner.snapshot.state = CollectorState::Stopping;
            inner.finish_as_completed = reason == RecordingStopReason::DiskLow;
            inner.storage_warning_emitted = true;
            stop_control = Some((
                inner.stop_tx.clone(),
                inner.input_control.clone(),
                reason,
                cutoff_qpc,
            ));
            notice = Some(if storage_probe_failed {
                DiskNotice::Unavailable
            } else {
                DiskNotice::Stop(next_storage.estimated_remaining_seconds)
            });
        }
        inner.snapshot.storage = next_storage;
    });

    record_diagnostic(
        shared,
        "info",
        DiagnosticCorrelation::run(session_id, run_id),
        DiagnosticPayload::CaptureProgress {
            frame_count: progress.frame_count,
            wgc_callback_count: progress.wgc_callback_count,
            active_segment: progress.active_segment,
            ready_segments: progress.ready_segments,
            duplicate_frames: progress.duplicate_frames,
            source_superseded: progress.source_superseded,
            capture_queue_depth: progress.capture_queue_depth,
            capture_queue_drops: progress.capture_queue_drops,
            input_event_count: progress.input_event_count,
            input_queue_depth: progress.input_queue_depth,
            input_queue_overflows: progress.input_queue_overflows,
            recorder_event_queue_depth: dispatch.queue_depth,
            recorder_event_queue_wait_ms: dispatch.queue_wait_ms,
            storage_state: storage_state_label(diagnostic_storage.state).into(),
            storage_free_bytes: diagnostic_storage.free_bytes,
            storage_remaining_seconds: diagnostic_storage.estimated_remaining_seconds,
        },
    );

    if let Some(notice) = notice {
        let notification = match notice {
            DiskNotice::Warning(remaining) => disk_warning_notification(shared, remaining),
            DiskNotice::Stop(remaining) => disk_stop_notification(shared, remaining),
            DiskNotice::Unavailable => disk_unavailable_notification(shared),
        };
        emit_collector_notification(&shared.app, &notification);
    }
    if let Some((stop_tx, input_control, reason, cutoff_qpc)) = stop_control {
        let stop_tx = stop_tx.context("disk safety stop lost the capture control channel")?;
        stop_tx
            .try_send(RecordingStopSignal { reason, cutoff_qpc })
            .context("disk safety stop could not reach the capture pipeline")?;
        if let Some(input_control) = input_control {
            input_control
                .close_gate()
                .context("disk safety stop could not close the Raw Input gate")?;
        }
    }
    Ok(())
}

/// 探针连续异常自动停录（130 号 §6.5）。
///
/// 挂在 `Progress` 上（每秒一次）而不是另起一条线程：判据本身要 20 秒才到线，
/// 再多一秒的发现延迟无关紧要，而多一条录制期线程是要付代价的。
///
/// 与磁盘水位那条**同一个形状**：置 `Stopping`、发停录信号、关输入闸、发通知。
/// 差别只在终态——磁盘满封口进 Completed，这条进 Faulted（见 `clean_stop_state`）。
#[cfg(feature = "depth-lab")]
fn apply_depth_health_abort(
    shared: &Arc<Shared>,
    session_id: &str,
    run_id: &str,
    run_directory: &Path,
) -> anyhow::Result<()> {
    if !crate::services::depth_lab::runtime_status().enabled {
        return Ok(());
    }
    let Some(notice) =
        crate::services::depth_lab::poll_health_abort(&run_directory.join("depth-lab"))
    else {
        return Ok(());
    };
    let cutoff_qpc = QpcClock::new()?.now()?;
    let mut stop_control = None;
    shared.update(|inner| {
        if !matches!(
            inner.snapshot.state,
            CollectorState::Starting | CollectorState::Recording
        ) {
            return;
        }
        // 探针已经自己拆完了 run，客户端这边只剩「把录制停下来」。
        // 不重开采集：连续 300 帧异常是系统性问题，重启只会再废一次并冲掉现场。
        inner.snapshot.state = CollectorState::Stopping;
        inner.finish_as_completed = false;
        stop_control = Some((inner.stop_tx.clone(), inner.input_control.clone()));
    });
    let Some((stop_tx, input_control)) = stop_control else {
        // 已经在收口了：探针的 Fault 会在 `stop_and_flush` 时被认出来，不必重复处置。
        return Ok(());
    };
    record_diagnostic(
        shared,
        "error",
        DiagnosticCorrelation::run(session_id, run_id),
        DiagnosticPayload::DepthFrameHealthAbort {
            diagnostic: notice.diagnostic.clone(),
        },
    );
    emit_collector_notification(
        &shared.app,
        &shared.diagnostics.create_notification(
            "DEPTH_FRAME_HEALTH_ABORT",
            notice.annotator,
            "请截图本页并联系工程，先不要继续录制；现场文件已随本次采集保存。",
            false,
            DiagnosticCorrelation::run(session_id, run_id),
            Some(&notice.diagnostic),
        ),
    );
    let stop_tx = stop_tx.context("depth health abort lost the capture control channel")?;
    stop_tx
        .try_send(RecordingStopSignal {
            reason: RecordingStopReason::DepthFrameHealth,
            cutoff_qpc,
        })
        .context("depth health abort could not reach the capture pipeline")?;
    if let Some(input_control) = input_control {
        input_control
            .close_gate()
            .context("depth health abort could not close the Raw Input gate")?;
    }
    Ok(())
}

fn disk_warning_notification(shared: &Shared, remaining_seconds: Option<u64>) -> CollectorFault {
    shared.diagnostics.create_notification(
        "DISK_SPACE_WARNING_30_MIN",
        format!(
            "本地磁盘预计还可录制约 {} 分钟。",
            remaining_seconds.unwrap_or(0).div_ceil(60)
        ),
        "尽快结束会话并释放空间；客户端不会自动删除已有 ready 数据。",
        true,
        shared.current_correlation(),
        None,
    )
}

fn disk_stop_notification(shared: &Shared, remaining_seconds: Option<u64>) -> CollectorFault {
    shared.diagnostics.create_notification(
        "DISK_STOP_WATERMARK",
        format!(
            "本地磁盘预计剩余 {} 分钟，已开始安全封口当前数据。",
            remaining_seconds.unwrap_or(0).div_ceil(60)
        ),
        "等待状态变为采集完成，再释放磁盘空间后开始新会话。",
        true,
        shared.current_correlation(),
        None,
    )
}

fn disk_unavailable_notification(shared: &Shared) -> CollectorFault {
    shared.diagnostics.create_notification(
        "DISK_SPACE_UNAVAILABLE",
        "无法继续读取采集磁盘水位，已开始安全停止。",
        "等待当前数据封口完成，确认磁盘在线且目录可访问后重试。",
        true,
        shared.current_correlation(),
        None,
    )
}

fn emit_collector_notification(app: &AppHandle, notification: &CollectorFault) {
    if let Err(error) = app.emit("collector://notification", notification) {
        tracing::warn!(error = %error, code = %notification.code, "failed to emit collector notification");
    }
}

fn update_progress(inner: &mut Inner, progress: &RecordingProgress) {
    if let Some(capture) = &mut inner.snapshot.capture {
        capture.frame_count = progress.frame_count;
        capture.wgc_callback_count = progress.wgc_callback_count;
        capture.active_segment = progress.active_segment;
        capture.ready_segments = progress.ready_segments;
        capture.duplicate_frames = progress.duplicate_frames;
        capture.source_superseded = progress.source_superseded;
        capture.capture_queue_depth = progress.capture_queue_depth;
        capture.capture_queue_drops = progress.capture_queue_drops;
        capture.input_event_count = progress.input_event_count;
        capture.input_queue_depth = progress.input_queue_depth;
        capture.input_queue_overflows = progress.input_queue_overflows;
        capture.pressed_contract_keys = progress.pressed_contract_keys.clone();
        capture.mouse_buttons = progress.mouse_buttons.clone();
        capture.elapsed_ms = inner
            .started_at
            .map_or(0, |started| started.elapsed().as_millis() as u64);
    }
}

/// preflight 直报的 details（docs/23 §3.1）：只放白名单字段，形状与
/// console-api 的 `dm-client-preflight-1` strict 契约一字不差。
fn preflight_report_details(
    failure_code: Option<&str>,
    check_id: &str,
    evidence: &capture_core::recording::HardwarePreflightEvidence,
) -> serde_json::Value {
    let mut details = serde_json::json!({
        "schema_version": crate::services::client_events::PREFLIGHT_SCHEMA,
        "checkId": check_id,
        "windowsBuild": evidence.windows.build.to_string(),
        "gpuVendor": format!("0x{:04x}", evidence.adapter_vendor_id),
        "gpuDeviceId": format!("0x{:04x}", evidence.adapter_device_id),
    });
    if let Some(map) = details.as_object_mut() {
        if let Some(driver) = &evidence.adapter_driver_version {
            map.insert("driverVersion".into(), serde_json::json!(driver));
        }
        if let Some(code) = failure_code {
            map.insert("failureCode".into(), serde_json::json!(code));
            map.insert("retryable".into(), serde_json::json!(true));
        }
    }
    details
}

fn next_session_id(contract: SessionContract) -> anyhow::Result<String> {
    let millis = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();
    let sequence = SESSION_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    Ok(format_session_id(contract, millis, sequence))
}

fn format_session_id(contract: SessionContract, millis: u128, sequence: u64) -> String {
    let prefix = match contract {
        SessionContract::LocalDevV1 => "dev",
        SessionContract::IngestV2 => "session",
    };
    format!("{prefix}-{millis}-{sequence:04}")
}

fn next_control_request_id(source: &str) -> anyhow::Result<String> {
    let millis = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();
    Ok(format!(
        "control-{source}-{millis}-{:04}",
        CONTROL_REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ))
}

fn next_marker_request_id(source: &str, qpc: i64) -> String {
    format!(
        "marker-{source}-{qpc}-{:04}",
        MARKER_REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )
}

#[allow(dead_code)]
fn _keep_summary_type(_: BoundTargetSummary) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_ids_name_their_contract_generation() {
        assert_eq!(
            format_session_id(SessionContract::IngestV2, 1_787_818_747_621, 0),
            "session-1787818747621-0000"
        );
        assert_eq!(
            format_session_id(SessionContract::LocalDevV1, 1_787_818_747_621, 7),
            "dev-1787818747621-0007"
        );
    }

    #[cfg(feature = "depth-lab")]
    #[test]
    fn performance_validation_run_never_enters_deliverable_finalizer() {
        assert!(!should_finalize_depth_run(true));
        assert!(should_finalize_depth_run(false));
    }

    #[test]
    fn canonical_cp2077_rgb_lane_does_not_require_depth() {
        assert!(!game_requires_depth("cyberpunk-2077-steam"));
        assert!(!game_requires_depth("fallout4-steam-release-test"));
        assert!(!game_requires_depth("synthetic"));
    }

    #[test]
    fn safe_quit_control_gate_serializes_with_start_and_blocks_future_starts() {
        let gate = Arc::new(ControlOperationGate::default());
        let active_start = gate.lock_for_start().unwrap();
        let (attempting_tx, attempting_rx) = bounded::<()>(1);
        let (quit_started_tx, quit_started_rx) = bounded::<()>(1);
        let quit_gate = Arc::clone(&gate);
        let quit_thread = std::thread::spawn(move || {
            attempting_tx.send(()).unwrap();
            let _quit_guard = quit_gate.begin_safe_quit();
            quit_started_tx.send(()).unwrap();
        });
        attempting_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();

        assert!(
            quit_started_rx
                .recv_timeout(std::time::Duration::from_millis(100))
                .is_err(),
            "safe quit did not serialize behind an in-flight start"
        );
        drop(active_start);
        quit_started_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        quit_thread.join().unwrap();

        assert!(
            gate.lock_for_start().is_err(),
            "a start was admitted after safe quit acquired the control gate"
        );
    }

    #[test]
    fn inactive_catalog_game_is_not_reported_as_a_window_failure() {
        let error = anyhow::anyhow!(
            "CATALOG_NO_MATCH: game fallout4-steam-release-test is not in the active catalog"
        );
        let spec = inactive_catalog_game_fault_spec(&error).expect("stable catalog fault");

        assert_eq!(spec.code, "CATALOG_GAME_NOT_ACTIVE");
        assert!(spec.message.contains("活动目录"));
        assert!(spec.action.contains("发布"));
        assert!(spec.retryable);
        assert!(
            inactive_catalog_game_fault_spec(&anyhow::anyhow!(
                "no matching visible Fallout 4 window"
            ))
            .is_none()
        );
    }

    // docs/65 J-23：自动暂停必须逐原因通知；手动暂停与 WindowDestroyed（已有
    // WINDOW_REBIND_REQUIRED）必须保持沉默。code 是前端与诊断的契约，钉死。
    #[test]
    fn auto_pause_reasons_notify_and_deliberate_stops_stay_silent() {
        let cases = [
            (
                RecordingStopReason::ForegroundLost,
                Some("RECORDING_PAUSED_FOREGROUND_LOST"),
            ),
            (
                RecordingStopReason::WindowMinimized,
                Some("RECORDING_PAUSED_WINDOW_MINIMIZED"),
            ),
            (
                RecordingStopReason::GeometryChanged,
                Some("RECORDING_PAUSED_GEOMETRY_CHANGED"),
            ),
            (
                RecordingStopReason::SettingsChanged,
                Some("RECORDING_PAUSED_SETTINGS_CHANGED"),
            ),
            (RecordingStopReason::Pause, None),
            (RecordingStopReason::WindowDestroyed, None),
            (RecordingStopReason::Manual, None),
        ];
        for (reason, expected) in cases {
            assert_eq!(
                auto_pause_notification_spec(reason).map(|(code, _, _)| code),
                expected,
                "unexpected notification mapping for {reason:?}"
            );
        }
        // SettingsChanged 的文案必须点明标定失效——这是 J-23 里最贵的信息。
        let (_, message, _) =
            auto_pause_notification_spec(RecordingStopReason::SettingsChanged).unwrap();
        assert!(
            message.contains("标定"),
            "settings-changed copy must mention calibration"
        );
    }

    #[test]
    fn legacy_calibration_is_rejected_before_its_stale_polling_value() {
        use crate::services::journal::{ViewCalibrationKind, ViewCalibrationWrite};
        use sha2::Digest as _;

        const RECORDING_HASH: &str =
            "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        const DEVICE_HASH: &str =
            "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let root = tempfile::tempdir().expect("temp dir");
        let journal = SessionJournal::open(root.path().join("journal.sqlite3")).expect("journal");
        let relative_path = "calibrations/legacy/input-calibration.json";
        let absolute_path = root.path().join(relative_path);
        std::fs::create_dir_all(absolute_path.parent().expect("evidence parent"))
            .expect("creates evidence parent");
        let legacy = br#"{"schema_version":"dm-input-calibration-1"}"#;
        std::fs::write(&absolute_path, legacy).expect("writes legacy evidence");
        let mut hasher = sha2::Sha256::new();
        hasher.update(legacy);
        let hash = format!("sha256:{:x}", hasher.finalize());
        journal
            .record_view_calibration(&ViewCalibrationWrite {
                calibration_id: "11111111-2222-3333-4444-555555555555",
                kind: ViewCalibrationKind::Full,
                parent_calibration_id: None,
                game_id: "game",
                recording_profile_hash: RECORDING_HASH,
                device_profile_hash: DEVICE_HASH,
                created_qpc: 1,
                verdict_pass: Some(true),
                counts_per_360_measured: Some(18_000.0),
                deviation_pct: Some(0.0),
                polling_hz_p50: Some(125.0),
                file_path: relative_path,
                file_sha256: &hash,
            })
            .expect("records legacy calibration");

        let gate = evaluate_view_scale_gate(
            &journal,
            root.path(),
            None,
            "game",
            RECORDING_HASH,
            DEVICE_HASH,
        )
        .expect("evaluates gate");
        let (code, _, _) = gate.block.expect("legacy evidence must block");
        assert_eq!(code, "VIEW_SCALE_CALIBRATION_VERSION_MISMATCH");
    }

    fn rebind_identity(target_id: &str) -> RebindIdentity {
        RebindIdentity {
            game_id: "capture-lab.synthetic".into(),
            executable_sha256: Some("sha256:exe".into()),
            session_contract: SessionContract::LocalDevV1,
            catalog_id: "test-catalog".into(),
            catalog_version: "1".into(),
            catalog_hash: "sha256:catalog".into(),
            device_profile_id: "test-device".into(),
            device_profile_version: "1".into(),
            device_profile_hash: "sha256:device".into(),
            recording_profile_id: "synthetic-1080p30".into(),
            recording_profile_version: "1".into(),
            recording_profile_hash: "sha256:recording".into(),
            game_profile_version: "1".into(),
            game_profile_hash: "sha256:game".into(),
            mapping_id: "capture-lab.synthetic.pc".into(),
            mapping_version: "1".into(),
            mapping_hash: "sha256:mapping".into(),
            target_id: target_id.into(),
        }
    }

    #[test]
    fn rebind_requires_the_same_capture_identity_and_a_new_target() {
        let expected = rebind_identity("target-old");
        let replacement = rebind_identity("target-new");

        validate_rebind_identity(&expected, &replacement).unwrap();

        let same_target = rebind_identity("target-old");
        assert!(
            validate_rebind_identity(&expected, &same_target)
                .unwrap_err()
                .to_string()
                .contains("newly discovered")
        );

        for mismatched in [
            RebindIdentity {
                executable_sha256: Some("sha256:other-exe".into()),
                ..replacement.clone()
            },
            RebindIdentity {
                recording_profile_id: "other-profile".into(),
                ..replacement.clone()
            },
            RebindIdentity {
                mapping_hash: "sha256:other-mapping".into(),
                ..replacement.clone()
            },
        ] {
            assert!(
                validate_rebind_identity(&expected, &mismatched)
                    .unwrap_err()
                    .to_string()
                    .contains("does not match")
            );
        }
    }

    #[cfg(not(feature = "local-catalog-only"))]
    #[test]
    fn catalog_invalidation_seals_active_runs_and_never_leaves_a_resumable_pause() {
        assert_eq!(
            catalog_invalidation_next_state(CollectorState::Recording),
            Some(CollectorState::Stopping)
        );
        assert_eq!(
            catalog_invalidation_next_state(CollectorState::Paused),
            Some(CollectorState::Faulted)
        );
        assert_eq!(
            clean_stop_state(RecordingStopReason::CatalogRetired),
            CollectorState::Faulted
        );
        assert_eq!(
            catalog_invalidation_next_state(CollectorState::Completed),
            None
        );
    }

    #[test]
    fn start_reservation_is_single_owner_and_restores_resume_state_on_abort() {
        assert_eq!(
            check_start_reservation(&None, "resume-1").unwrap(),
            ReservationCheck::Available
        );
        let mut reservation = Some("resume-1".to_owned());
        assert_eq!(
            check_start_reservation(&reservation, "resume-1").unwrap(),
            ReservationCheck::Replay
        );
        assert!(check_start_reservation(&reservation, "resume-2").is_err());

        let mut state = CollectorState::Preflighting;
        release_start_reservation(
            &mut reservation,
            &mut state,
            "resume-2",
            CollectorState::Paused,
        );
        assert_eq!(reservation.as_deref(), Some("resume-1"));
        assert_eq!(state, CollectorState::Preflighting);
        release_start_reservation(
            &mut reservation,
            &mut state,
            "resume-1",
            CollectorState::Paused,
        );
        assert!(reservation.is_none());
        assert_eq!(state, CollectorState::Paused);
        assert_eq!(
            preflight_failure_state(StartKind::Resume),
            CollectorState::Paused
        );
        assert_eq!(
            preflight_failure_state(StartKind::NewSession),
            CollectorState::Faulted
        );
    }

    #[test]
    fn every_resume_control_request_creates_a_distinct_preflight_identity() {
        let first = next_control_request_id("overlay-resume").unwrap();
        let second = next_control_request_id("overlay-resume").unwrap();

        assert_ne!(first, second);
        assert_ne!(
            preflight::preflight_id(&first, "target-1", "sha256:catalog"),
            preflight::preflight_id(&second, "target-1", "sha256:catalog")
        );
    }

    #[test]
    fn pause_and_window_lifecycle_cutoffs_preserve_the_session() {
        for reason in [
            RecordingStopReason::Pause,
            RecordingStopReason::ForegroundLost,
            RecordingStopReason::WindowMinimized,
            RecordingStopReason::WindowDestroyed,
            RecordingStopReason::GeometryChanged,
        ] {
            assert_eq!(clean_stop_state(reason), CollectorState::Paused);
        }
    }

    #[test]
    fn only_recording_accepts_a_new_marker() {
        for state in [
            CollectorState::Idle,
            CollectorState::Discovering,
            CollectorState::Bound,
            CollectorState::Starting,
            CollectorState::Pausing,
            CollectorState::Paused,
            CollectorState::Stopping,
            CollectorState::Completed,
            CollectorState::Faulted,
        ] {
            assert!(!marker_state_allows_new(state), "state {state:?}");
        }
        assert!(marker_state_allows_new(CollectorState::Recording));
    }

    #[test]
    fn marker_replay_rejects_a_different_active_recording_run() {
        let existing = MarkerAudit {
            marker_id: "session-1:m000000".into(),
            session_id: "session-1".into(),
            run_id: "r0000".into(),
            seq: 0,
            kind: "manual".into(),
            qpc: 100,
            qpc_frequency: 10_000_000,
            run_frame: 0,
            session_frame: 0,
            video_time_us: 0,
            created_at_utc: "2026-07-23T00:00:00.000Z".into(),
        };

        assert!(!marker_replay_conflicts_with_active_run(
            CollectorState::Recording,
            Some(("session-1", "r0000")),
            &existing,
        ));
        assert!(marker_replay_conflicts_with_active_run(
            CollectorState::Recording,
            Some(("session-1", "r0001")),
            &existing,
        ));
        assert!(marker_replay_conflicts_with_active_run(
            CollectorState::Recording,
            Some(("session-2", "r0000")),
            &existing,
        ));
        assert!(!marker_replay_conflicts_with_active_run(
            CollectorState::Completed,
            None,
            &existing,
        ));
    }

    #[test]
    fn unsafe_pipeline_stops_do_not_masquerade_as_a_pause() {
        assert_eq!(
            clean_stop_state(RecordingStopReason::SourceUnavailable),
            CollectorState::Faulted
        );
        assert_eq!(
            clean_stop_state(RecordingStopReason::InputFault),
            CollectorState::Faulted
        );
        assert_eq!(
            clean_stop_state(RecordingStopReason::Manual),
            CollectorState::Completed
        );
        assert_eq!(
            clean_stop_state(RecordingStopReason::DiskLow),
            CollectorState::Completed
        );
        assert_eq!(
            clean_stop_state(RecordingStopReason::DiskStatusUnavailable),
            CollectorState::Faulted
        );
        assert_eq!(
            terminal_fault_spec(
                RecordingStopReason::DiskStatusUnavailable,
                CollectorState::Faulted
            )
            .unwrap()
            .code,
            "DISK_SPACE_UNAVAILABLE"
        );
    }

    /// 130 号 §6.5：连续异常停录后**不能**表现成「录完了」或「暂停了」——
    /// 那两档都在暗示接着录就行，而这次恰恰要停下来找人。
    #[test]
    fn a_depth_health_abort_lands_in_faulted_and_says_so() {
        assert_eq!(
            clean_stop_state(RecordingStopReason::DepthFrameHealth),
            CollectorState::Faulted
        );
        let spec = terminal_fault_spec(
            RecordingStopReason::DepthFrameHealth,
            CollectorState::Faulted,
        )
        .expect("Faulted 终态必须带 fault");
        assert_eq!(spec.code, "DEPTH_FRAME_HEALTH_ABORT");
        // 不许劝重试：重启只会再废一次并冲掉现场
        assert!(!spec.retryable);
        assert!(spec.action.contains("联系工程"), "{}", spec.action);
        // 采集员文案不带内部术语（与 GateDenial 同一条纪律）
        for jargon in ["DSV", "probe", "fp16", "streak"] {
            assert!(
                !spec.message.contains(jargon) && !spec.action.contains(jargon),
                "给采集员的文案不该出现内部术语 `{jargon}`"
            );
        }
        // 停录理由必须能写进 session，且与其它理由不同名
        assert_eq!(
            stop_reason_label(RecordingStopReason::DepthFrameHealth),
            "depth_frame_health"
        );
    }

    #[test]
    fn only_the_first_active_run_transition_to_faulted_reports() {
        assert!(should_report_recording_failure(
            CollectorState::Recording,
            CollectorState::Faulted,
            true
        ));
        assert!(!should_report_recording_failure(
            CollectorState::Faulted,
            CollectorState::Faulted,
            true
        ));
        assert!(!should_report_recording_failure(
            CollectorState::Recording,
            CollectorState::Completed,
            true
        ));
        assert!(!should_report_recording_failure(
            CollectorState::Bound,
            CollectorState::Faulted,
            false
        ));
    }

    #[test]
    fn recording_failure_runtime_is_frozen_at_the_fault_transition() {
        let mut snapshot = CollectorSnapshot {
            state: CollectorState::Faulted,
            capture: Some(CaptureStatus {
                session_id: "session-1".into(),
                run_id: "r0000".into(),
                frame_count: 42,
                capture_queue_drops: 3,
                ..CaptureStatus::default()
            }),
            storage: StorageStatus {
                free_bytes: Some(123),
                ..StorageStatus::default()
            },
            ..CollectorSnapshot::default()
        };

        let frozen = runtime_diagnostic_snapshot(&snapshot, Some("session-1"));
        snapshot.state = CollectorState::Recording;
        snapshot.capture.as_mut().unwrap().frame_count = 999;
        snapshot.storage.free_bytes = Some(456);

        assert_eq!(frozen.collector_state, CollectorState::Faulted);
        assert_eq!(frozen.capture.as_ref().unwrap().frame_count, 42);
        assert_eq!(frozen.capture.as_ref().unwrap().capture_queue_drops, 3);
        assert_eq!(frozen.storage.free_bytes, Some(123));
    }

    #[test]
    fn input_not_neutral_failure_is_actionable() {
        let error = anyhow::anyhow!("INPUT_NOT_NEUTRAL at first video frame")
            .context("Raw Input arming failed");

        let fault = recording_fault_spec(&error);

        assert_eq!(fault.code, "INPUT_NOT_NEUTRAL");
        assert!(fault.message.contains("输入仍处于按下状态"));
        assert!(fault.action.contains("松开"));
        assert!(fault.retryable);
    }

    #[test]
    fn encoder_recording_failures_get_a_stable_category() {
        let fault = recording_fault_spec(&anyhow::anyhow!("encoder failed"));

        assert_eq!(fault.code, "ENCODER_PIPELINE_FAILED");
    }

    #[test]
    fn source_recording_failures_get_a_stable_category() {
        let fault = recording_fault_spec(&anyhow::anyhow!(
            "WGC callback channel disconnected before first frame"
        ));

        assert_eq!(fault.code, "CAPTURE_SOURCE_FAILED");
    }

    #[test]
    fn finalization_failures_get_a_stable_category() {
        let fault = recording_fault_spec(&anyhow::anyhow!("finalize worker panicked"));

        assert_eq!(fault.code, "ARTIFACT_FINALIZATION_FAILED");
    }

    #[test]
    fn projection_mismatch_is_not_masked_by_secondary_wgc_timeout() {
        let error = anyhow::anyhow!("timed out waiting for first eligible WGC frame").context(
            "candidate materialization failed: same-Present projection family mismatch at 42",
        );

        let fault = recording_fault_spec(&error);

        assert_eq!(fault.code, "DEPTH_PROJECTION_MISMATCH");
        assert!(fault.message.contains("投影族"));
        assert!(fault.action.contains("性能验证"));
        assert!(!fault.retryable);
    }

    #[test]
    fn missing_camera_evidence_does_not_instruct_projection_recertification() {
        for cause in [
            "depth projection evidence failed: depth frame 140 has no same-Present exact-draw viewport observation",
            "camera_production_failed: bound camera warmup budget exceeded",
            "depth projection evidence failed: D3D12 production lost draw/queue observation coverage",
        ] {
            let error = anyhow::anyhow!("WGC/selected-Present feedback failed: ReadbackMapFailed")
                .context(cause);
            let fault = recording_fault_spec(&error);
            assert_eq!(fault.code, "DEPTH_CAMERA_EVIDENCE_FAILED");
            assert!(!fault.message.contains("投影族"));
            assert!(!fault.action.contains("认证"));
            assert!(!fault.retryable);
            let startup = depth_startup_fault_spec(cause);
            assert_eq!(startup.code, fault.code);
            assert!(startup.message.contains("录制未开始"));
            assert!(!startup.retryable);
        }
        assert_eq!(
            depth_startup_fault_spec("injector could not start").code,
            "DEPTH_PROBE_NOT_READY"
        );
        assert_eq!(
            depth_startup_fault_spec("same-Present projection family mismatch").code,
            "DEPTH_PROJECTION_MISMATCH"
        );
    }

    #[test]
    fn selected_present_failure_is_not_misreported_as_a_window_capture_failure() {
        let error = anyhow::anyhow!(
            "timed out waiting for WGC/selected-Present startup phase=generation_armed_ack"
        )
        .context("selected-Present feedback failed: StopRun arrived before GenerationSealed");

        let fault = recording_fault_spec(&error);

        assert_eq!(fault.code, "DEPTH_FEEDBACK_FAILED");
        assert!(fault.message.contains("同帧反馈"));
        assert!(fault.action.contains("详情编号"));
        assert!(fault.retryable);
    }

    #[test]
    fn depth_gpu_allocation_failure_is_not_misreported_as_an_encoder_failure() {
        let error = anyhow::anyhow!(
            "DEPTH_PROBE_NOT_FLUSHED: probe fault flush_validation_failed: flush summary validation failed: contract violation: selected-Present attributable GPU allocation delta 537919488 exceeds configured limit 536870912 (usage_before=1 usage_after=537919489)"
        );

        let fault = recording_fault_spec(&error);

        assert_eq!(fault.code, "DEPTH_GPU_MEMORY_BUDGET_FAILED");
        assert!(fault.message.contains("显存"));
        assert!(fault.action.contains("只重试当前认证档"));
        assert!(fault.retryable);
    }

    #[test]
    fn player_pose_failure_is_not_misreported_as_selected_present_feedback() {
        let error = anyhow::anyhow!(
            "DEPTH_PROBE_NOT_FLUSHED: probe fault player_pose_sampling_failed: Camera production failed: player pose sample 50622 failed: Fallout actor-root transform did not produce a stable valid snapshot after 8 reads"
        );

        let fault = recording_fault_spec(&error);

        assert_eq!(fault.code, "DEPTH_PLAYER_POSE_FAILED");
        assert!(fault.message.contains("玩家位姿"));
        assert!(fault.action.contains("当前认证档"));
        assert!(fault.retryable);
    }

    #[test]
    fn depth_finalization_failure_preserves_raw_without_promising_faulted_run_recovery() {
        let fault = recording_fault_spec(&anyhow::anyhow!(
            "DEPTH_FINALIZE_FAILED: watermark evidence missing"
        ));

        assert_eq!(fault.code, "DEPTH_FINALIZE_FAILED");
        assert!(fault.message.contains("上传未放行"));
        assert!(fault.action.contains("不会自动恢复 faulted run"));
        assert!(!fault.action.contains("不需要重新录制"));
    }

    #[test]
    fn capture_queue_drop_failure_is_actionable() {
        let fault = recording_fault_spec(&anyhow::anyhow!("WGC callback queue dropped 1 frame(s)"));

        assert_eq!(fault.code, "CAPTURE_QUEUE_DROPPED");
        assert!(fault.message.contains("丢帧"));
        assert!(fault.retryable);
    }

    #[test]
    fn journal_failure_is_actionable() {
        let error = anyhow::anyhow!("database is busy")
            .context("JOURNAL_WRITE_FAILED while committing ready segment");

        let fault = recording_fault_spec(&error);

        assert_eq!(fault.code, "JOURNAL_WRITE_FAILED");
        assert!(fault.message.contains("ready"));
        assert!(fault.action.contains("恢复"));
    }

    #[test]
    fn recorder_event_worker_does_not_block_capture_on_slow_persistence() {
        use std::sync::mpsc;
        use std::time::Duration;

        let (applied_tx, applied_rx) = mpsc::channel();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let mut first_event = true;
        let worker = RecorderEventWorker::start(
            "dm-recorder-events-test".into(),
            move |event, _dispatch| {
                if first_event {
                    first_event = false;
                    entered_tx.send(()).unwrap();
                    release_rx.recv_timeout(Duration::from_secs(1)).unwrap();
                }
                applied_tx.send(event).unwrap();
                Ok(())
            },
            || panic!("slow persistence alone must not fail the event worker"),
        )
        .unwrap();

        worker
            .submit(RecorderEvent::Progress(RecordingProgress::default()))
            .unwrap();
        entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();

        let started = Instant::now();
        worker
            .submit(RecorderEvent::Progress(RecordingProgress::default()))
            .unwrap();
        assert!(
            started.elapsed() < Duration::from_millis(250),
            "capture-side event submission blocked on slow persistence"
        );

        release_tx.send(()).unwrap();
        worker.finish().unwrap();
        for _ in 0..2 {
            assert!(matches!(
                applied_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
                RecorderEvent::Progress(_)
            ));
        }
    }

    #[test]
    fn recorder_event_failure_wins_over_the_follow_on_capture_stop() {
        let capture_error = anyhow::anyhow!("capture stopped after control signal");
        let event_error = anyhow::anyhow!("JOURNAL_WRITE_FAILED while recording heartbeat");

        let error =
            combine_capture_and_recorder_event_results::<()>(Err(capture_error), Err(event_error))
                .unwrap_err();

        assert!(format!("{error:#}").contains("JOURNAL_WRITE_FAILED"));
        assert_eq!(recording_fault_spec(&error).code, "JOURNAL_WRITE_FAILED");
    }
}

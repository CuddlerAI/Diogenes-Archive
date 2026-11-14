use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, ensure};
use parking_lot::Mutex;
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::domain::collector::{CollectorFault, CollectorState, StorageStatus};
use crate::services::journal::JournalDiagnosticSnapshot;

const DIAGNOSTIC_SCHEMA_VERSION: &str = "dm-diagnostic-event-1";
const EXPORT_SCHEMA_VERSION: &str = "dm-diagnostic-export-1";
const LOG_ROTATE_BYTES: u64 = 16 * 1024 * 1024;
const LOG_ARCHIVES: usize = 3;
const EXPORT_EVENT_LIMIT: usize = 10_000;
static DETAILS_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static EXPORT_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Default, Serialize)]
pub struct DiagnosticCorrelation {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub segment_index: Option<u32>,
}

impl DiagnosticCorrelation {
    #[must_use]
    pub fn run(session_id: &str, run_id: &str) -> Self {
        Self {
            session_id: Some(session_id.into()),
            run_id: Some(run_id.into()),
            segment_index: None,
        }
    }

    #[must_use]
    pub fn segment(session_id: &str, run_id: &str, segment_index: u32) -> Self {
        Self {
            session_id: Some(session_id.into()),
            run_id: Some(run_id.into()),
            segment_index: Some(segment_index),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum DiagnosticPayload {
    AppStarted {
        client_version: String,
        journal_schema_version: u32,
    },
    /// Local lifecycle breadcrumbs for the installer smoke and normal safe quit.
    /// These deliberately contain no paths, identities, credentials, or session data.
    SafeQuitRequested {
        quiescence_timeout_ms: Option<u64>,
    },
    SafeQuitCompleted {
        elapsed_ms: u64,
    },
    SafeQuitTimedOut {
        timeout_ms: u64,
        blocked_gates: Vec<String>,
    },
    RecoveryCompleted {
        recovery_id: String,
        scanned_sessions: u32,
        verified_ready_segments: u32,
        quarantined_items: u32,
        corrupt_segments: u32,
        orphan_items: u32,
        /// Startup blocks on re-hashing every ready artifact, so this cost tracks
        /// the whole local archive rather than what changed. Exported so the growth
        /// is measurable across launches.
        verified_bytes: u64,
        verification_ms: u64,
    },
    /// 服务构造期的 fail-closed 降级。只能记录稳定服务名、错误码和已经脱敏的真实原因；
    /// endpoint、环境变量原值、token 与请求正文都不得进入这里。
    ServiceStartupDegraded {
        service: String,
        code: String,
        reason: String,
    },
    TargetBound {
        game_id: String,
        executable_name: String,
        client_width: u32,
        client_height: u32,
        dpi: u32,
        recording_profile_id: String,
        mapping_id: String,
        mapping_version: String,
    },
    ControlPhaseCompleted {
        operation: String,
        phase: String,
        outcome: String,
        elapsed_ms: u64,
    },
    RunStarted {
        run_seq: u32,
        capture_backend: String,
        encoder_backend: String,
        fps_num: u32,
        fps_den: u32,
        width: u32,
        height: u32,
        max_bitrate_bps: u32,
        segment_seconds: u32,
    },
    CaptureProgress {
        frame_count: u64,
        wgc_callback_count: u64,
        active_segment: u32,
        ready_segments: u32,
        duplicate_frames: u64,
        source_superseded: u64,
        capture_queue_depth: usize,
        capture_queue_drops: u64,
        input_event_count: u64,
        input_queue_depth: usize,
        input_queue_overflows: u64,
        recorder_event_queue_depth: usize,
        recorder_event_queue_wait_ms: u64,
        storage_state: String,
        storage_free_bytes: Option<u64>,
        storage_remaining_seconds: Option<u64>,
    },
    SegmentReady {
        frame_count: u64,
        event_count: u64,
        qpc_start: i64,
        qpc_end: i64,
        video_bytes: u64,
    },
    /// Durable local-only breadcrumb for the R2 state machine. The payload is
    /// deliberately metadata-only: never add credentials, signed URLs, object
    /// keys, request/response bodies, absolute paths, or raw content hashes.
    R2UploadProgress {
        scope: &'static str,
        phase: &'static str,
        state: &'static str,
        attempt_count: u32,
        artifact_kind: Option<String>,
        artifact_count: Option<u32>,
        part_number: Option<u32>,
        part_count: Option<u32>,
        byte_size: Option<u64>,
        error_code: Option<String>,
        retry_delay_ms: Option<u64>,
        http_status: Option<u16>,
        server_code: Option<String>,
        server_message: Option<String>,
        request_id: Option<String>,
    },
    MarkerAdded {
        marker_id: String,
        kind: String,
        qpc: i64,
        run_frame: u64,
        session_frame: u64,
        video_time_us: u64,
        artifact_synced: bool,
        replayed: bool,
    },
    RunStopped {
        stop_reason: String,
        frame_count: u64,
        ready_segments: u32,
        input_event_count: u64,
        input_queue_overflows: u64,
        wgc_callback_count: u64,
        watermark_decoded_observations: u64,
        watermark_rejected_observations: u64,
        watermark_integrity_rejections: u64,
        watermark_ambiguous_rejections: u64,
        max_consecutive_watermark_rejections: u64,
        watermark_stale_observations: u64,
        max_watermark_stale_lag_presents: u64,
    },
    SessionDeleted {
        deletion_id: String,
        artifacts_removed: bool,
    },
    /// 探针因连续异常自行中止，客户端随之停录（docs/130 §6.5）。
    ///
    /// 这里落的是**给工程的那一路**文案（缺陷码、连续段起点、现场位置、下一步）。
    /// 给采集员的那一路走通知，不进诊断日志——两路的读者与用途都不同。
    ///
    /// 与唯一的构造点（`orchestrator::apply_depth_health_abort`）同门：正式安装包
    /// 不含 depth，不 gate 就会在默认特性下留下一条 `never constructed`——
    /// 而 CI 的 clippy 恒定带着 `depth-lab` 跑，看不见它。
    #[cfg(feature = "depth-lab")]
    DepthFrameHealthAbort {
        diagnostic: String,
    },
    Fault {
        code: String,
        message: String,
        action: String,
        retryable: bool,
        diagnostic: Option<String>,
    },
    ExportCreated {
        export_id: String,
        file_name: String,
        event_count: u32,
    },
    SentryRecordingFailureReport {
        fault_code: String,
        sentry_event_id: Option<String>,
        export_id: Option<String>,
        outcome: String,
        error: Option<String>,
    },
    // M-A 版本检查留痕（docs/15 §2.6）。检查请求本身不携带任何身份；这两个
    // 事件只落本地诊断日志，进入 mirror 投影前仍需过白名单（docs/13 §0）。
    UpdateCheck {
        trigger: String,
        current_version: String,
        latest_version: Option<String>,
        update_available: bool,
    },
    UpdateCheckFailed {
        trigger: String,
        error: String,
    },
    /// 账号接口失败的脱敏关联信息。这里刻意只有固定路径、状态码和服务端
    /// request id；请求体、响应正文、邮箱、验证码、邀请码与 token 永不落盘。
    AccountApiRequestFailed {
        operation: String,
        method: String,
        path: String,
        http_status: Option<u16>,
        server_code: String,
        request_id: Option<String>,
        retryable: bool,
        environment: String,
    },
    /// 一次签名 catalog 下发尝试的结果（issue #323）。
    ///
    /// 与 [`Self::AccountApiRequestFailed`] 同一条纪律，而且这条更要紧：绑定前通道
    /// 带的是**标注员会话**。所以这里只有稳定错误码、走的哪条通道，以及来自**已验签**
    /// store 的 catalog 身份；Authorization 头、会话 token、设备凭据、URL 与响应正文
    /// 一律不落盘。
    ///
    /// 成功也记：#323 的排障困难有一半来自「什么都没发生过」——日志里既没有失败也
    /// 没有成功，无法判断这条链路到底跑没跑。
    #[cfg(not(feature = "local-catalog-only"))]
    CatalogSyncCompleted {
        /// `device_credential` 或 `annotator_session`（绑定前）。
        auth_channel: &'static str,
        /// `accepted` / `not_modified` / `rejected` / `failed`。
        outcome: &'static str,
        error_code: Option<String>,
        /// 本次采纳后活动集合里的每一份，形如 `catalog_id@catalog_version`。集合下发
        /// 之后一次采纳涉及多份（docs/57 §3.3）；非 `accepted` 的结果这里是空的。
        catalogs: Vec<String>,
        activated: bool,
    },
}

#[derive(Serialize)]
struct DiagnosticEvent {
    schema_version: &'static str,
    timestamp_unix_ms: u64,
    level: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    details_id: Option<String>,
    #[serde(flatten)]
    correlation: DiagnosticCorrelation,
    #[serde(flatten)]
    payload: DiagnosticPayload,
}

#[derive(Clone, Debug, Serialize)]
pub struct RuntimeDiagnosticSnapshot {
    pub collector_state: CollectorState,
    pub target: Option<RuntimeTargetDiagnostic>,
    pub capture: Option<RuntimeCaptureDiagnostic>,
    pub storage: StorageStatus,
}

#[derive(Clone, Debug, Serialize)]
pub struct RuntimeTargetDiagnostic {
    pub game_id: String,
    pub executable_name: String,
    pub client_width: u32,
    pub client_height: u32,
    pub dpi: u32,
    pub recording_profile_id: String,
    pub mapping_id: String,
    pub mapping_version: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct RuntimeCaptureDiagnostic {
    pub session_id: String,
    pub run_id: String,
    pub run_seq: u32,
    pub frame_count: u64,
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
    pub elapsed_ms: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticExportResult {
    pub export_id: String,
    pub file_name: String,
    pub sha256: String,
    pub event_count: u32,
}

#[derive(Serialize)]
struct DiagnosticExport<'a> {
    schema_version: &'static str,
    export_id: &'a str,
    created_unix_ms: u64,
    non_trainable: bool,
    production_certified: bool,
    runtime: &'a RuntimeDiagnosticSnapshot,
    journal: &'a JournalDiagnosticSnapshot,
    events: &'a [Value],
}

/// 诊断事件的旁路订阅者（docs/23 §4.3）：吃 record() 产出的**已脱敏** JSON。
/// 回调必须自吞错误、绝不 panic——它挂在录制主链路的日志落点上。
pub type DiagnosticEventSink = dyn Fn(&Value) + Send + Sync;

pub struct DiagnosticService {
    root: PathBuf,
    log_path: PathBuf,
    log: Mutex<File>,
    redaction_prefixes: Vec<String>,
    event_sink: Mutex<Option<std::sync::Arc<DiagnosticEventSink>>>,
}

impl DiagnosticService {
    pub fn open(app_data_root: &Path) -> anyhow::Result<Self> {
        let root = app_data_root.join("diagnostics");
        let logs = root.join("logs");
        fs::create_dir_all(&logs)?;
        let log_path = logs.join("events.jsonl");
        rotate_logs(&log_path)?;
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .with_context(|| format!("failed to open diagnostic log {}", log_path.display()))?;
        let mut redaction_prefixes = vec![app_data_root.to_string_lossy().into_owned()];
        if let Ok(workspace) = std::env::current_dir() {
            redaction_prefixes.push(workspace.to_string_lossy().into_owned());
        }
        if let Some(user_profile) = std::env::var_os("USERPROFILE") {
            redaction_prefixes.push(user_profile.to_string_lossy().into_owned());
        }
        redaction_prefixes.sort_by_key(|prefix| std::cmp::Reverse(prefix.len()));
        redaction_prefixes.dedup();
        Ok(Self {
            root,
            log_path,
            log: Mutex::new(log),
            redaction_prefixes,
            event_sink: Mutex::new(None),
        })
    }

    /// 注册旁路订阅者（当前唯一订阅者：client_events 的上云白名单映射）。
    pub fn set_event_sink(&self, sink: std::sync::Arc<DiagnosticEventSink>) {
        self.event_sink.lock().replace(sink);
    }

    pub fn record(
        &self,
        level: &'static str,
        details_id: Option<&str>,
        correlation: DiagnosticCorrelation,
        payload: DiagnosticPayload,
        durable: bool,
    ) -> anyhow::Result<()> {
        ensure!(
            matches!(level, "info" | "warning" | "error"),
            "invalid diagnostic log level"
        );
        let event = DiagnosticEvent {
            schema_version: DIAGNOSTIC_SCHEMA_VERSION,
            timestamp_unix_ms: unix_ms()?,
            level,
            details_id: details_id.map(str::to_owned),
            correlation,
            payload,
        };
        let mut value = serde_json::to_value(event)?;
        self.sanitize_value(&mut value);
        let mut line = serde_json::to_vec(&value)?;
        line.push(b'\n');
        {
            let mut log = self.log.lock();
            log.write_all(&line)?;
            log.flush()?;
            if durable {
                log.sync_data()?;
            }
        }
        // 旁路订阅在本地落盘之后：本地诊断永远是权威留痕，sink 只吃已脱敏
        // 的副本，其失败（由 sink 自吞）不影响 record 的结果。
        let sink = self.event_sink.lock().clone();
        if let Some(sink) = sink {
            sink(&value);
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_fault(
        &self,
        code: &str,
        message: impl Into<String>,
        action: impl Into<String>,
        retryable: bool,
        correlation: DiagnosticCorrelation,
        diagnostic: Option<&str>,
    ) -> CollectorFault {
        self.create_correlated(
            "error",
            code,
            message,
            action,
            retryable,
            correlation,
            diagnostic,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_fault_with_details_id(
        &self,
        details_id: &str,
        code: &str,
        message: impl Into<String>,
        action: impl Into<String>,
        retryable: bool,
        correlation: DiagnosticCorrelation,
        diagnostic: Option<&str>,
    ) -> CollectorFault {
        self.create_correlated_with_details_id(
            "error",
            details_id,
            code,
            message,
            action,
            retryable,
            correlation,
            diagnostic,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_notification(
        &self,
        code: &str,
        message: impl Into<String>,
        action: impl Into<String>,
        retryable: bool,
        correlation: DiagnosticCorrelation,
        diagnostic: Option<&str>,
    ) -> CollectorFault {
        self.create_correlated(
            "warning",
            code,
            message,
            action,
            retryable,
            correlation,
            diagnostic,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn create_correlated(
        &self,
        level: &'static str,
        code: &str,
        message: impl Into<String>,
        action: impl Into<String>,
        retryable: bool,
        correlation: DiagnosticCorrelation,
        diagnostic: Option<&str>,
    ) -> CollectorFault {
        let details_id = self.next_details_id();
        self.create_correlated_with_details_id(
            level,
            &details_id,
            code,
            message,
            action,
            retryable,
            correlation,
            diagnostic,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn create_correlated_with_details_id(
        &self,
        level: &'static str,
        details_id: &str,
        code: &str,
        message: impl Into<String>,
        action: impl Into<String>,
        retryable: bool,
        correlation: DiagnosticCorrelation,
        diagnostic: Option<&str>,
    ) -> CollectorFault {
        let details_id = details_id.to_owned();
        let message = message.into();
        let action = action.into();
        let payload = DiagnosticPayload::Fault {
            code: code.into(),
            message: message.clone(),
            action: action.clone(),
            retryable,
            diagnostic: diagnostic.map(|value| self.sanitize_text(value)),
        };
        if let Err(error) = self.record(level, Some(&details_id), correlation, payload, true) {
            tracing::error!(code, error = %error, "failed to persist correlated diagnostic fault");
        }
        CollectorFault {
            code: code.into(),
            message,
            action,
            retryable,
            details_id,
        }
    }

    pub fn export(
        &self,
        runtime: &RuntimeDiagnosticSnapshot,
        journal: &JournalDiagnosticSnapshot,
        session_id: Option<&str>,
    ) -> anyhow::Result<DiagnosticExportResult> {
        self.log.lock().flush()?;
        let events = self.read_events(session_id)?;
        let created_unix_ms = unix_ms()?;
        let export_id = format!(
            "diagnostic-{created_unix_ms}-{:04}",
            EXPORT_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        );
        let file_name = format!("{export_id}.json");
        let export_dir = self.root.join("exports");
        fs::create_dir_all(&export_dir)?;
        let path = export_dir.join(&file_name);
        let mut export = serde_json::to_value(DiagnosticExport {
            schema_version: EXPORT_SCHEMA_VERSION,
            export_id: &export_id,
            created_unix_ms,
            non_trainable: true,
            production_certified: false,
            runtime,
            journal,
            events: &events,
        })?;
        self.sanitize_value(&mut export);
        let mut bytes = serde_json::to_vec_pretty(&export)?;
        bytes.push(b'\n');
        write_atomic(&path, &bytes)?;
        let sha256 = bytes_sha256(&bytes);
        let event_count =
            u32::try_from(events.len()).context("diagnostic event count exceeds u32")?;
        self.record(
            "info",
            None,
            DiagnosticCorrelation {
                session_id: session_id.map(str::to_owned),
                ..DiagnosticCorrelation::default()
            },
            DiagnosticPayload::ExportCreated {
                export_id: export_id.clone(),
                file_name: file_name.clone(),
                event_count,
            },
            true,
        )?;
        Ok(DiagnosticExportResult {
            export_id,
            file_name,
            sha256,
            event_count,
        })
    }

    /// Reads an export produced by this service for a trusted in-process
    /// reporter. The UI still receives only metadata through `export`.
    pub(crate) fn read_export_attachment(
        &self,
        export: &DiagnosticExportResult,
        max_bytes: usize,
    ) -> anyhow::Result<Option<Vec<u8>>> {
        ensure!(
            Path::new(&export.file_name).components().count() == 1
                && export.file_name.ends_with(".json"),
            "diagnostic export file name is invalid"
        );
        let path = self.root.join("exports").join(&export.file_name);
        let bytes = fs::read(&path)
            .with_context(|| format!("failed to read diagnostic export {}", path.display()))?;
        if bytes.len() > max_bytes {
            return Ok(None);
        }
        ensure!(
            bytes_sha256(&bytes) == export.sha256,
            "diagnostic export changed after atomic publication"
        );
        Ok(Some(bytes))
    }

    fn next_details_id(&self) -> String {
        let millis = unix_ms().unwrap_or(0);
        format!(
            "details-{millis}-{:04}",
            DETAILS_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        )
    }

    /// Archives are walked oldest-first and the live log last, so the retained
    /// window is always the newest `EXPORT_EVENT_LIMIT` matching events.
    ///
    /// Eviction uses a deque rather than `Vec::remove(0)`, which was O(n) per
    /// evicted event; and sanitization is deferred until the window is final, so a
    /// long log no longer pays the recursive string scan for events it discards.
    fn read_events(&self, session_id: Option<&str>) -> anyhow::Result<Vec<Value>> {
        let mut events = VecDeque::with_capacity(EXPORT_EVENT_LIMIT + 1);
        for index in (1..=LOG_ARCHIVES)
            .rev()
            .map(Some)
            .chain(std::iter::once(None))
        {
            let path = index.map_or_else(
                || self.log_path.clone(),
                |index| {
                    self.log_path
                        .with_file_name(format!("events.{index}.jsonl"))
                },
            );
            if !path.is_file() {
                continue;
            }
            let reader = BufReader::new(File::open(path)?);
            for line in reader.lines() {
                let Ok(line) = line else {
                    continue;
                };
                let Ok(value) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                let matches_scope = session_id.is_none_or(|session_id| {
                    value.get("session_id").and_then(Value::as_str) == Some(session_id)
                        || (value.get("session_id").is_none()
                            && matches!(
                                value.get("kind").and_then(Value::as_str),
                                Some(
                                    "app_started"
                                        | "recovery_completed"
                                        | "service_startup_degraded"
                                )
                            ))
                });
                if matches_scope {
                    events.push_back(value);
                    if events.len() > EXPORT_EVENT_LIMIT {
                        events.pop_front();
                    }
                }
            }
        }
        let mut events = Vec::from(events);
        for value in &mut events {
            self.sanitize_value(value);
        }
        Ok(events)
    }

    fn sanitize_value(&self, value: &mut Value) {
        self.sanitize_value_at_key(value, None);
    }

    fn sanitize_value_at_key(&self, value: &mut Value, key: Option<&str>) {
        match value {
            Value::Object(object) => {
                object.retain(|key, _| !forbidden_key(key));
                for (key, value) in object.iter_mut() {
                    self.sanitize_value_at_key(value, Some(key));
                }
            }
            Value::Array(values) => {
                for value in values {
                    self.sanitize_value_at_key(value, key);
                }
            }
            Value::String(text) if key == Some("server_code") && is_stable_machine_code(text) => {}
            Value::String(text) => *text = self.sanitize_text(text),
            _ => {}
        }
    }

    fn sanitize_text(&self, value: &str) -> String {
        let lower = value.to_ascii_lowercase();
        if [
            "authorization",
            "bearer ",
            "credential",
            "password",
            "client_secret",
            "presigned",
            "x-amz-signature",
            "device handle",
            "raw handle",
            "hwnd",
        ]
        .iter()
        .any(|needle| lower.contains(needle))
        {
            return "[redacted-sensitive-diagnostic]".into();
        }
        let mut sanitized = value.to_owned();
        for prefix in &self.redaction_prefixes {
            sanitized = replace_case_insensitive(&sanitized, prefix, "[redacted-path]");
        }
        sanitized = redact_absolute_windows_paths(&sanitized);
        if sanitized.contains("://")
            && let Some(query) = sanitized.find('?')
        {
            sanitized.truncate(query);
            sanitized.push_str("?[redacted-query]");
        }
        sanitized.chars().take(2_048).collect()
    }
}

fn rotate_logs(path: &Path) -> anyhow::Result<()> {
    if !path.is_file() || fs::metadata(path)?.len() < LOG_ROTATE_BYTES {
        return Ok(());
    }
    let oldest = path.with_file_name(format!("events.{LOG_ARCHIVES}.jsonl"));
    if oldest.exists() {
        fs::remove_file(&oldest)?;
    }
    for index in (1..LOG_ARCHIVES).rev() {
        let source = path.with_file_name(format!("events.{index}.jsonl"));
        if source.exists() {
            fs::rename(
                source,
                path.with_file_name(format!("events.{}.jsonl", index + 1)),
            )?;
        }
    }
    fs::rename(path, path.with_file_name("events.1.jsonl"))?;
    Ok(())
}

fn write_atomic(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    ensure!(!path.exists(), "diagnostic export id collision");
    let file_name = path
        .file_name()
        .context("diagnostic export path has no file name")?
        .to_string_lossy();
    let staging = path.with_file_name(format!(".{file_name}.tmp"));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&staging)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(staging, path)?;
    Ok(())
}

fn forbidden_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    [
        "hwnd",
        "handle",
        "authorization",
        "credential",
        "password",
        "secret",
        "token",
        "presigned",
        "target_id",
        "process_id",
        "pressed_contract_keys",
        "mouse_buttons",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn is_stable_machine_code(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 96
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

fn replace_case_insensitive(value: &str, needle: &str, replacement: &str) -> String {
    if needle.is_empty() {
        return value.into();
    }
    let lower_value = value.to_ascii_lowercase();
    let lower_needle = needle.to_ascii_lowercase();
    let mut result = String::new();
    let mut cursor = 0;
    while let Some(offset) = lower_value[cursor..].find(&lower_needle) {
        let start = cursor + offset;
        result.push_str(&value[cursor..start]);
        result.push_str(replacement);
        cursor = start + needle.len();
    }
    result.push_str(&value[cursor..]);
    result
}

fn redact_absolute_windows_paths(value: &str) -> String {
    let chars = value.char_indices().collect::<Vec<_>>();
    let mut result = String::with_capacity(value.len());
    let mut cursor = 0;
    let mut index = 0;
    while index < chars.len() {
        let (byte_index, character) = chars[index];
        let drive_path = character.is_ascii_alphabetic()
            && chars.get(index + 1).is_some_and(|(_, next)| *next == ':')
            && chars
                .get(index + 2)
                .is_some_and(|(_, next)| matches!(*next, '\\' | '/'));
        let unc_path =
            character == '\\' && chars.get(index + 1).is_some_and(|(_, next)| *next == '\\');
        if drive_path || unc_path {
            result.push_str(&value[cursor..byte_index]);
            result.push_str("[redacted-path]");
            index += if drive_path { 3 } else { 2 };
            index = end_of_windows_path(&chars, index);
            cursor = chars.get(index).map_or(value.len(), |(offset, _)| *offset);
            continue;
        }
        index += 1;
    }
    result.push_str(&value[cursor..]);
    result
}

/// Finds where a Windows path that began at `start` ends.
///
/// Stopping at the first space would leave most of `C:\Program Files\Some Game\
/// game.exe` in the export — the very directory names worth redacting. So spaces
/// are consumed, and the run is then trimmed back to the last path separator plus
/// the one component that follows it. That keeps `C:\logs\a.txt failed to open`
/// from swallowing the diagnostic text after the path, while still redacting every
/// component of a path that genuinely contains spaces.
fn end_of_windows_path(chars: &[(usize, char)], start: usize) -> usize {
    let mut greedy = start;
    while greedy < chars.len()
        && !matches!(
            chars[greedy].1,
            '\t' | '\r'
                | '\n'
                | '"'
                | '\''
                | '<'
                | '>'
                | '|'
                | '?'
                | '*'
                | ')'
                | ']'
                | '}'
                | ','
                | ';'
        )
    {
        greedy += 1;
    }
    let Some(last_separator) = (start..greedy)
        .rev()
        .find(|index| matches!(chars[*index].1, '\\' | '/'))
    else {
        // No component beyond the drive/UNC prefix: redact the first word only.
        return (start..greedy)
            .find(|index| chars[*index].1 == ' ')
            .unwrap_or(greedy);
    };
    let component_start = last_separator + 1;
    let mut token_start = component_start;
    while token_start < greedy {
        let token_end = (token_start..greedy)
            .find(|index| chars[*index].1 == ' ')
            .unwrap_or(greedy);
        if token_looks_like_filename(chars, token_start, token_end) {
            return token_end;
        }
        if token_end == greedy {
            break;
        }
        token_start = token_end + 1;
    }

    // A terminal directory can legitimately contain spaces (for example a user
    // profile). Without a filename-shaped boundary, consuming the rest of the
    // line is the privacy-safe choice; preserving ambiguous prose would leak the
    // exact final component the redactor exists to hide.
    greedy
}

fn token_looks_like_filename(chars: &[(usize, char)], start: usize, end: usize) -> bool {
    let significant_end = (start..end)
        .rev()
        .find(|index| !matches!(chars[*index].1, ':' | '.'))
        .map_or(start, |index| index + 1);
    let Some(dot) = (start..significant_end)
        .rev()
        .find(|index| chars[*index].1 == '.')
    else {
        return false;
    };
    dot > start
        && dot + 1 < significant_end
        && significant_end - dot <= 17
        && chars[dot + 1..significant_end]
            .iter()
            .all(|(_, character)| character.is_ascii_alphanumeric())
}

fn unix_ms() -> anyhow::Result<u64> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before UNIX epoch")?
        .as_millis();
    u64::try_from(millis).context("system timestamp exceeds u64")
}

fn bytes_sha256(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("sha256:{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_quit_timeout_is_durable_and_names_blocked_gates() {
        let temp = tempfile::tempdir().unwrap();
        let service = DiagnosticService::open(temp.path()).unwrap();
        service
            .record(
                "error",
                None,
                DiagnosticCorrelation::default(),
                DiagnosticPayload::SafeQuitTimedOut {
                    timeout_ms: 8_000,
                    blocked_gates: vec!["client_events_worker".into()],
                },
                true,
            )
            .unwrap();

        let line = fs::read_to_string(&service.log_path).unwrap();
        let event: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(event["kind"], "safe_quit_timed_out");
        assert_eq!(event["level"], "error");
        assert_eq!(event["data"]["timeout_ms"], 8_000);
        assert_eq!(
            event["data"]["blocked_gates"],
            serde_json::json!(["client_events_worker"])
        );
    }

    #[test]
    fn run_stopped_json_pins_wgc_stale_observation_fields() {
        let value = serde_json::to_value(DiagnosticPayload::RunStopped {
            stop_reason: "manual".into(),
            frame_count: 600,
            ready_segments: 2,
            input_event_count: 0,
            input_queue_overflows: 0,
            wgc_callback_count: 4_800,
            watermark_decoded_observations: 4_799,
            watermark_rejected_observations: 0,
            watermark_integrity_rejections: 0,
            watermark_ambiguous_rejections: 0,
            max_consecutive_watermark_rejections: 0,
            watermark_stale_observations: 1,
            max_watermark_stale_lag_presents: 1,
        })
        .unwrap();

        assert_eq!(value["kind"], "run_stopped");
        assert_eq!(value["data"]["watermark_stale_observations"], 1);
        assert_eq!(value["data"]["max_watermark_stale_lag_presents"], 1);
    }
    use crate::services::journal::{JournalDiagnosticRecoveryFinding, JournalDiagnosticSnapshot};

    fn empty_journal() -> JournalDiagnosticSnapshot {
        JournalDiagnosticSnapshot {
            scoped_session_id: Some("session-1".into()),
            sessions: Vec::new(),
            runs: Vec::new(),
            segments: Vec::new(),
            markers: Vec::new(),
            preflights: Vec::new(),
            data_model_jobs: Vec::new(),
            latest_recovery: None,
            recovery_findings: Vec::new(),
        }
    }

    fn runtime() -> RuntimeDiagnosticSnapshot {
        RuntimeDiagnosticSnapshot {
            collector_state: CollectorState::Faulted,
            target: Some(RuntimeTargetDiagnostic {
                game_id: "synthetic-win32-demo".into(),
                executable_name: "capture-lab.exe".into(),
                client_width: 1920,
                client_height: 1080,
                dpi: 96,
                recording_profile_id: "synthetic-win32-dev-v1".into(),
                mapping_id: "synthetic-win32-demo.pc".into(),
                mapping_version: "1".into(),
            }),
            capture: Some(RuntimeCaptureDiagnostic {
                session_id: "session-1".into(),
                run_id: "r0000".into(),
                run_seq: 0,
                frame_count: 300,
                wgc_callback_count: 502,
                active_segment: 1,
                ready_segments: 1,
                duplicate_frames: 0,
                source_superseded: 2,
                capture_queue_depth: 0,
                capture_queue_drops: 0,
                input_event_count: 42,
                input_queue_depth: 0,
                input_queue_overflows: 0,
                elapsed_ms: 10_000,
            }),
            storage: StorageStatus::default(),
        }
    }

    #[test]
    fn details_id_maps_ui_fault_to_persisted_event() {
        let temp = tempfile::tempdir().unwrap();
        let service = DiagnosticService::open(temp.path()).unwrap();
        let fault = service.create_fault(
            "CAPTURE_PIPELINE_FAILED",
            "采集链路已停止。",
            "导出诊断后重试。",
            true,
            DiagnosticCorrelation::run("session-1", "r0000"),
            Some(r"encoder failed at C:\Users\Alice\private\trace.txt"),
        );

        let log = fs::read_to_string(&service.log_path).unwrap();
        assert!(fault.details_id.starts_with("details-"));
        assert!(log.contains(&fault.details_id));
        assert!(log.contains("CAPTURE_PIPELINE_FAILED"));
        assert!(log.contains("session-1"));
        assert!(!log.contains("Alice"));
        assert!(!log.contains(r"C:\Users"));
    }

    #[test]
    fn r2_progress_is_session_scoped_and_contains_only_safe_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let service = DiagnosticService::open(temp.path()).unwrap();
        service
            .record(
                "info",
                None,
                DiagnosticCorrelation::segment("session-1", "r0000", 3),
                DiagnosticPayload::R2UploadProgress {
                    scope: "segment",
                    phase: "upload_rejected",
                    state: "rejected",
                    attempt_count: 2,
                    artifact_kind: Some("video".into()),
                    artifact_count: Some(5),
                    part_number: Some(4),
                    part_count: Some(7),
                    byte_size: Some(262_144),
                    error_code: Some("UPLOAD_REQUEST_REJECTED".into()),
                    retry_delay_ms: None,
                    http_status: Some(409),
                    server_code: Some("device_profile_mismatch".into()),
                    server_message: Some(
                        "device profile does not match the registered device".into(),
                    ),
                    request_id: Some("request-profile".into()),
                },
                true,
            )
            .unwrap();
        let events = service.read_events(Some("session-1")).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["kind"], "r2_upload_progress");
        assert_eq!(events[0]["data"]["phase"], "upload_rejected");
        assert_eq!(events[0]["data"]["part_number"], 4);
        assert_eq!(events[0]["data"]["http_status"], 409);
        assert_eq!(events[0]["data"]["server_code"], "device_profile_mismatch");
        assert_eq!(
            events[0]["data"]["server_message"],
            "device profile does not match the registered device"
        );
        assert_eq!(events[0]["data"]["request_id"], "request-profile");
        let serialized = serde_json::to_string(&events[0]).unwrap();
        for forbidden in [
            "authorization",
            "credential",
            "signed_url",
            "object_key",
            "local_path",
        ] {
            assert!(!serialized.contains(forbidden));
        }
    }

    #[test]
    fn export_is_atomic_hashed_and_redacts_sensitive_diagnostics() {
        let temp = tempfile::tempdir().unwrap();
        let service = DiagnosticService::open(temp.path()).unwrap();
        service
            .record(
                "info",
                None,
                DiagnosticCorrelation::run("session-1", "r0000"),
                DiagnosticPayload::CaptureProgress {
                    frame_count: 300,
                    wgc_callback_count: 502,
                    active_segment: 1,
                    ready_segments: 1,
                    duplicate_frames: 0,
                    source_superseded: 2,
                    capture_queue_depth: 0,
                    capture_queue_drops: 0,
                    input_event_count: 42,
                    input_queue_depth: 0,
                    input_queue_overflows: 0,
                    recorder_event_queue_depth: 0,
                    recorder_event_queue_wait_ms: 1,
                    storage_state: "ready".into(),
                    storage_free_bytes: Some(100_000_000_000),
                    storage_remaining_seconds: Some(3_600),
                },
                true,
            )
            .unwrap();
        service
            .record(
                "error",
                None,
                DiagnosticCorrelation::default(),
                DiagnosticPayload::ServiceStartupDegraded {
                    service: "account".into(),
                    code: "account_base_url_invalid".into(),
                    reason: "account base URL must be https".into(),
                },
                true,
            )
            .unwrap();
        service
            .record(
                "info",
                None,
                DiagnosticCorrelation::default(),
                DiagnosticPayload::TargetBound {
                    game_id: "unrelated-session-target".into(),
                    executable_name: "other.exe".into(),
                    client_width: 1920,
                    client_height: 1080,
                    dpi: 96,
                    recording_profile_id: "other-profile".into(),
                    mapping_id: "other-mapping".into(),
                    mapping_version: "1".into(),
                },
                true,
            )
            .unwrap();
        let mut journal = empty_journal();
        journal
            .recovery_findings
            .push(JournalDiagnosticRecoveryFinding {
                severity: "error".into(),
                code: "TEST".into(),
                action: "inspect".into(),
                session_id: Some("session-1".into()),
                run_name: Some("r0000".into()),
                segment_seq: Some(0),
                message: r"credential failed at C:\Users\Alice\secret.txt".into(),
            });

        let result = service
            .export(&runtime(), &journal, Some("session-1"))
            .unwrap();
        let bytes = fs::read(service.root.join("exports").join(&result.file_name)).unwrap();
        let text = String::from_utf8(bytes.clone()).unwrap();

        assert_eq!(result.sha256, bytes_sha256(&bytes));
        assert_eq!(result.event_count, 2);
        assert!(text.contains("dm-diagnostic-export-1"));
        assert!(text.contains("capture_queue_drops"));
        assert!(text.contains("service_startup_degraded"));
        assert!(text.contains("account_base_url_invalid"));
        assert!(text.contains("account base URL must be https"));
        assert!(!text.contains("unrelated-session-target"));
        assert!(text.contains("[redacted-sensitive-diagnostic]"));
        let attachment = service
            .read_export_attachment(&result, 8 * 1024 * 1024)
            .unwrap()
            .unwrap();
        assert_eq!(bytes_sha256(&attachment), result.sha256);
        assert!(
            service
                .read_export_attachment(&result, 1)
                .unwrap()
                .is_none()
        );
        assert!(!text.to_ascii_lowercase().contains("credential failed"));
        assert!(!text.contains("Alice"));
        assert!(!text.to_ascii_lowercase().contains("hwnd"));
        assert!(!text.to_ascii_lowercase().contains("process_id"));
        assert!(
            !service
                .root
                .join("exports")
                .join(format!(".{}.tmp", result.file_name))
                .exists()
        );
    }

    #[test]
    fn sanitizer_drops_forbidden_keys_recursively() {
        let temp = tempfile::tempdir().unwrap();
        let service = DiagnosticService::open(temp.path()).unwrap();
        let mut value = serde_json::json!({
            "safe": "ok",
            "nested": {
                "hwnd_value": 123,
                "device_handle": 456,
                "authorization": "Bearer secret"
            }
        });
        service.sanitize_value(&mut value);

        assert_eq!(value["safe"], "ok");
        assert_eq!(value["nested"], serde_json::json!({}));
    }

    #[test]
    fn sanitizer_keeps_bounded_server_codes_but_not_free_text() {
        let temp = tempfile::tempdir().unwrap();
        let service = DiagnosticService::open(temp.path()).unwrap();
        let mut value = serde_json::json!({
            "server_code": "invalid_credentials",
            "nested": {
                "server_code": "credential leaked from an upstream response"
            }
        });
        service.sanitize_value(&mut value);

        assert_eq!(value["server_code"], "invalid_credentials");
        assert_eq!(
            value["nested"]["server_code"],
            "[redacted-sensitive-diagnostic]"
        );
    }

    #[test]
    fn redaction_covers_windows_paths_that_contain_spaces() {
        // The regression: stopping at the first space left everything after
        // `C:\Program` — including the game and user directory names — in the export.
        for path in [
            r"C:\Program Files\Some Game\game.exe",
            r"C:\Users\First Last\AppData\Local\DataMiner\journal.sqlite3",
            r"\\build server\share\Some Game\game.exe",
        ] {
            let redacted = redact_absolute_windows_paths(path);
            assert_eq!(redacted, "[redacted-path]", "leaked from {path}");
        }
        assert_eq!(
            redact_absolute_windows_paths(r"home C:\Users\First Last"),
            "home [redacted-path]"
        );
    }

    #[test]
    fn redaction_keeps_the_diagnostic_text_after_a_path() {
        assert_eq!(
            redact_absolute_windows_paths(r"failed to open C:\logs\a.txt: not found"),
            "failed to open [redacted-path] not found"
        );
        assert_eq!(
            redact_absolute_windows_paths(r"C:\logs\a.txt failed"),
            "[redacted-path] failed"
        );
        assert_eq!(
            redact_absolute_windows_paths(r"C:\logs\my report.txt failed"),
            "[redacted-path] failed"
        );
        assert_eq!(
            redact_absolute_windows_paths("no path here"),
            "no path here"
        );
    }

    #[test]
    fn export_window_keeps_the_newest_events_without_quadratic_eviction() {
        let temp = tempfile::tempdir().unwrap();
        let service = DiagnosticService::open(temp.path()).unwrap();
        let overflow = EXPORT_EVENT_LIMIT + 25;
        for index in 0..overflow {
            service
                .record(
                    "info",
                    None,
                    DiagnosticCorrelation::default(),
                    DiagnosticPayload::AppStarted {
                        client_version: format!("{index}"),
                        journal_schema_version: 1,
                    },
                    false,
                )
                .unwrap();
        }

        let events = service.read_events(None).unwrap();

        let version_of = |value: &Value| {
            value
                .pointer("/data/client_version")
                .and_then(Value::as_str)
                .map(str::to_owned)
        };
        assert_eq!(events.len(), EXPORT_EVENT_LIMIT);
        assert_eq!(
            events.first().and_then(version_of),
            Some((overflow - EXPORT_EVENT_LIMIT).to_string())
        );
        assert_eq!(
            events.last().and_then(version_of),
            Some((overflow - 1).to_string())
        );
    }
}

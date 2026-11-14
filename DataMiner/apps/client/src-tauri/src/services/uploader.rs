use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, ensure};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use capture_core::ingest_contract::SessionContract;
use crossbeam_channel::{Receiver, Sender, bounded};
use md5::Md5;
use parking_lot::Mutex;
use reqwest::Method;
use reqwest::blocking::{Body, Client, Response};
use reqwest::header::{AUTHORIZATION, CONTENT_LENGTH, ETAG, HeaderMap, HeaderName, HeaderValue};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::domain::collector::UploadStatus;
use crate::services::credentials::{
    self, CredentialKind, CredentialStore, DeviceCredential, PRODUCTION_CREDENTIAL_SCOPE,
};
use crate::services::diagnostics::{DiagnosticCorrelation, DiagnosticPayload, DiagnosticService};
use crate::services::journal::{
    SessionArtifactKind, SessionArtifactRemoteStatus, SessionArtifactUploadJob,
    SessionArtifactUploadState, SessionJournal, SessionRegistrationCandidate,
    SessionViewCalibrationEvidence, UploadArtifactSource, UploadCandidate, UploadFailureDetails,
    UploadJob, UploadedPart,
};
use crate::services::session_artifacts;

const DEFAULT_PRODUCTION_API_BASE_URL: &str = "https://api.diogenesmodel.com/";
const API_BASE_URL_ENV: &str = "DATAMINER_API_BASE_URL";
/// Debug-only escape hatch for the isolated upload lab. Production/release
/// builds never accept an HTTP presigned URL, even when this variable is set.
#[cfg(feature = "dev-catalog")]
const ALLOW_INSECURE_LOOPBACK_R2_ENV: &str = "DATAMINER_ALLOW_INSECURE_LOOPBACK_R2";
/// Which server-side environment the classification envelope declares.
/// `production` (default) or `staging`; the server refuses a mismatch, so this
/// replaces the previous compile-time `"production"` literal that made staging
/// uploads impossible to even attempt.
const API_ENVIRONMENT_ENV: &str = "DATAMINER_API_ENVIRONMENT";
const UPLOAD_LAB_DEVICE_CREDENTIAL: &str = "dataminer-upload-lab-device";
const WORKER_POLL_INTERVAL: Duration = Duration::from_secs(3);
const STATUS_POLL_INTERVAL_MS: u64 = 30_000;
const SESSION_SEAL_MAX_ATTEMPTS: u32 = 8;
const STATUS_POLL_MAX_ERRORS: u32 = 8;
/// Control-plane JSON: fail fast.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// R2 part PUT body: the sensible duration is set by part size over link speed,
/// not by the control-plane budget (docs/30 §5.5). An 8 MiB part at 1 Mbit/s is
/// ~67 s; 600 s covers 8 MiB down to ~0.11 Mbit/s while staying inside the
/// 900 s presign TTL with margin.
const PUT_TIMEOUT: Duration = Duration::from_secs(600);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// 一个 segment 内同时在飞的 part 数。单条 TCP 连接吃不满上行带宽，而
/// 「签名往返 + PUT」串成一条链时每个 part 都要等一个完整 RTT——4 条并发把
/// 这两项一起摊掉。
///
/// 代价必须写清楚（docs/30 §5.5 的续作）：[`PUT_TIMEOUT`] 是**每 part** 的预算，
/// 并发 C 条会让每条各自变慢约 C 倍。串行时代 8 MiB / 600 s 能容忍到
/// ~0.11 Mbit/s，C=4 时这个下限抬到 ~0.45 Mbit/s。比这更慢的链路把
/// [`PART_CONCURRENCY_ENV`] 设成 1 即可逐字节退回串行行为。
const DEFAULT_PART_CONCURRENCY: usize = 4;
/// 上限。再高就只是把弱网下限继续往上抬，换不到吞吐——R2 单对象的
/// multipart 写入早就不是瓶颈了。
const MAX_PART_CONCURRENCY: usize = 8;
const PART_CONCURRENCY_ENV: &str = "DATAMINER_UPLOAD_PART_CONCURRENCY";

/// 上传面快照的出口（docs/65 J-29）。worker 每轮把 [`UploadStatus`] 推给它；
/// 生产接线是 `SessionOrchestrator::set_upload_status`，测试可以塞任何闭包。
/// 做成闭包而不是直接握 orchestrator，是让 uploader 不依赖编排器类型。
pub type UploadStatusSink = Arc<dyn Fn(UploadStatus) + Send + Sync>;

struct UploaderInner {
    journal: Arc<SessionJournal>,
    credential_store: Arc<dyn CredentialStore>,
    diagnostics: Arc<DiagnosticService>,
    raw_root: PathBuf,
    app_data_root: PathBuf,
    transport: HttpTransport,
    wake_tx: Sender<()>,
    stopping: AtomicBool,
    busy: AtomicBool,
    prefer_session_artifact: AtomicBool,
    status_sink: UploadStatusSink,
}

pub struct UploaderService {
    inner: Arc<UploaderInner>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl UploaderService {
    pub fn start(
        journal: Arc<SessionJournal>,
        raw_root: PathBuf,
        app_data_root: PathBuf,
        credential_store: Arc<dyn CredentialStore>,
        diagnostics: Arc<DiagnosticService>,
        status_sink: UploadStatusSink,
    ) -> anyhow::Result<Arc<Self>> {
        let transport = HttpTransport::production()?;
        let (wake_tx, wake_rx) = bounded(1);
        let inner = Arc::new(UploaderInner {
            journal,
            credential_store,
            diagnostics,
            raw_root,
            app_data_root,
            transport,
            wake_tx,
            stopping: AtomicBool::new(false),
            busy: AtomicBool::new(false),
            prefer_session_artifact: AtomicBool::new(true),
            status_sink,
        });
        let worker_inner = Arc::clone(&inner);
        let worker = std::thread::Builder::new()
            .name("dm-r2-uploader".into())
            .spawn(move || worker_loop(&worker_inner, &wake_rx))
            .context("failed to start the R2 uploader worker")?;
        Ok(Arc::new(Self {
            inner,
            worker: Mutex::new(Some(worker)),
        }))
    }

    pub fn wake(&self) {
        let _ = self.inner.wake_tx.try_send(());
    }

    #[must_use]
    pub fn shutdown_complete(&self) -> bool {
        self.worker
            .lock()
            .as_ref()
            .is_none_or(JoinHandle::is_finished)
    }

    pub fn begin_shutdown(&self) {
        self.inner.stopping.store(true, Ordering::Release);
        let _ = self.inner.wake_tx.try_send(());
    }
}

impl Drop for UploaderService {
    fn drop(&mut self) {
        self.begin_shutdown();
        if let Some(worker) = self.worker.lock().take()
            && worker.thread().id() != std::thread::current().id()
            && let Err(error) = worker.join()
        {
            tracing::error!(?error, "R2 uploader worker panicked during shutdown");
        }
    }
}

struct BusyGuard<'a>(&'a AtomicBool);

impl BusyGuard<'_> {
    fn enter(flag: &AtomicBool) -> BusyGuard<'_> {
        flag.store(true, Ordering::Release);
        BusyGuard(flag)
    }
}

impl Drop for BusyGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

#[derive(Clone, Copy)]
struct UploadDiagnostic<'a> {
    scope: &'static str,
    phase: &'static str,
    state: &'static str,
    attempt_count: u32,
    artifact_kind: Option<&'a str>,
    artifact_count: Option<u32>,
    part_number: Option<u32>,
    part_count: Option<u32>,
    byte_size: Option<u64>,
    error_code: Option<&'a str>,
    retry_delay_ms: Option<u64>,
    http_status: Option<u16>,
    server_code: Option<&'a str>,
    server_message: Option<&'a str>,
    request_id: Option<&'a str>,
}

impl UploadDiagnostic<'_> {
    fn new(
        scope: &'static str,
        phase: &'static str,
        state: &'static str,
        attempt_count: u32,
    ) -> Self {
        Self {
            scope,
            phase,
            state,
            attempt_count,
            artifact_kind: None,
            artifact_count: None,
            part_number: None,
            part_count: None,
            byte_size: None,
            error_code: None,
            retry_delay_ms: None,
            http_status: None,
            server_code: None,
            server_message: None,
            request_id: None,
        }
    }
}

fn record_upload_progress(
    inner: &UploaderInner,
    correlation: DiagnosticCorrelation,
    level: &'static str,
    durable: bool,
    event: UploadDiagnostic<'_>,
) {
    let payload = DiagnosticPayload::R2UploadProgress {
        scope: event.scope,
        phase: event.phase,
        state: event.state,
        attempt_count: event.attempt_count,
        artifact_kind: event.artifact_kind.map(str::to_owned),
        artifact_count: event.artifact_count,
        part_number: event.part_number,
        part_count: event.part_count,
        byte_size: event.byte_size,
        error_code: event.error_code.map(str::to_owned),
        retry_delay_ms: event.retry_delay_ms,
        http_status: event.http_status,
        server_code: event.server_code.map(str::to_owned),
        server_message: event.server_message.map(str::to_owned),
        request_id: event.request_id.map(str::to_owned),
    };
    if let Err(error) = inner
        .diagnostics
        .record(level, None, correlation, payload, durable)
    {
        tracing::warn!(
            error = %error,
            scope = event.scope,
            phase = event.phase,
            "failed to persist local R2 upload diagnostic"
        );
    }
}

fn segment_correlation(job: &UploadJob) -> DiagnosticCorrelation {
    DiagnosticCorrelation::segment(&job.session_id, &job.run_name, job.segment_index)
}

fn session_correlation(session_id: &str) -> DiagnosticCorrelation {
    DiagnosticCorrelation {
        session_id: Some(session_id.to_owned()),
        ..DiagnosticCorrelation::default()
    }
}

fn worker_loop(inner: &Arc<UploaderInner>, wake_rx: &Receiver<()>) {
    while !inner.stopping.load(Ordering::Acquire) {
        let freeze_result = catch_unwind(AssertUnwindSafe(|| {
            let _busy = BusyGuard::enter(&inner.busy);
            session_artifacts::freeze_next_eligible_session(
                &inner.journal,
                &inner.raw_root,
                &inner.app_data_root,
            )
        }));
        match freeze_result {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => {
                tracing::warn!(
                    error = %error,
                    "completed session artifact bundle remains pending local freeze"
                );
            }
            Err(_) => {
                tracing::error!("session artifact local freeze recovered from a worker panic");
            }
        }

        let credential = match load_upload_credential(inner) {
            Ok(Some(credential)) => credential,
            Ok(None) => {
                // docs/65 J-30：这个 3s 空转此前对界面完全不可见——生产
                // `upload_parts=0` 三十多天无人从客户端侧察觉。降级可以继续
                // 静默地等，但不可以静默地存在：每轮都把「凭据缺席」推进快照。
                publish_upload_status(inner, true);
                let _ = wake_rx.recv_timeout(WORKER_POLL_INTERVAL);
                continue;
            }
            Err(error) => {
                tracing::warn!(error = %error, "production device credential is unavailable; upload remains queued");
                publish_upload_status(inner, true);
                let _ = wake_rx.recv_timeout(WORKER_POLL_INTERVAL);
                continue;
            }
        };

        let worked = match catch_unwind(AssertUnwindSafe(|| process_one(inner, &credential))) {
            Ok(Ok(worked)) => worked,
            Ok(Err(error)) => {
                tracing::warn!(error = %error, "R2 uploader loop reached a durable retry boundary");
                false
            }
            Err(_) => {
                if let Err(error) = inner.journal.requeue_interrupted_upload_jobs() {
                    tracing::error!(error = %error, "R2 uploader panic recovery could not requeue its durable claim");
                }
                tracing::error!("R2 uploader recovered from a worker panic");
                false
            }
        };
        // 每轮收尾把队列读数推进快照（docs/65 J-29）。sink 端做了等值去重，
        // 空转轮不会刷出快照噪声。
        publish_upload_status(inner, false);
        if !worked {
            let _ = wake_rx.recv_timeout(WORKER_POLL_INTERVAL);
        }
    }
}

/// 读一次 journal 聚合并推给快照 sink。读失败只留日志——上传面是展示事实，
/// 它的任何失败都不允许影响上传本身，更不允许影响采集。
fn publish_upload_status(inner: &UploaderInner, credentials_missing: bool) {
    if inner.stopping.load(Ordering::Acquire) {
        return;
    }
    match inner.journal.upload_queue_status() {
        Ok(queue) => (inner.status_sink)(UploadStatus {
            uploader_running: true,
            credentials_missing,
            pending_segments: queue.pending_segments,
            inflight_segments: queue.inflight_segments,
            awaiting_verification_segments: queue.awaiting_verification_segments,
            rejected_segments: queue.rejected_segments,
            pending_session_artifacts: queue.pending_session_artifacts,
            last_error_code: queue.last_error_code,
            next_retry_unix_ms: queue.next_retry_unix_ms,
        }),
        Err(error) => {
            tracing::warn!(error = %error, "upload queue status is unreadable; the snapshot keeps its last value");
        }
    }
}

fn load_upload_credential(inner: &UploaderInner) -> anyhow::Result<Option<DeviceCredential>> {
    if upload_lab_control_plane_enabled(&inner.transport) {
        // The loopback lab validates request flow, not production identity. A
        // synthetic bearer keeps the real device credential inside Credential
        // Manager while still exercising the exact Authorization code path.
        return DeviceCredential::new(UPLOAD_LAB_DEVICE_CREDENTIAL).map(Some);
    }
    credentials::load(
        inner.credential_store.as_ref(),
        CredentialKind::Device,
        PRODUCTION_CREDENTIAL_SCOPE,
    )
}

fn process_one(inner: &UploaderInner, credential: &DeviceCredential) -> anyhow::Result<bool> {
    if process_one_session_seal(inner, credential)? {
        return Ok(true);
    }
    let prefer_session_artifact = inner
        .prefer_session_artifact
        .fetch_xor(true, Ordering::AcqRel);
    if prefer_session_artifact && process_one_session_artifact(inner, credential)? {
        return Ok(true);
    }
    if let Some(stored) = inner.journal.next_stored_upload()? {
        let _busy = BusyGuard::enter(&inner.busy);
        poll_status(inner, credential, &stored)?;
        return Ok(true);
    }
    if let Some(job) = inner.journal.claim_next_upload_job()? {
        let _busy = BusyGuard::enter(&inner.busy);
        record_upload_progress(
            inner,
            segment_correlation(&job),
            "info",
            false,
            UploadDiagnostic::new("segment", "job_started", "running", job.attempt_count),
        );
        match upload_job(inner, credential, &job) {
            Ok(()) => {}
            Err(error) => {
                let code = error.code();
                let retryable = error.retryable();
                let http_status = error.http_status();
                let server_code = error.server_code();
                let server_message = error.server_message();
                let request_id = error.request_id();
                if retryable {
                    let delay_ms = retry_delay_ms(job.attempt_count, &job.segment_id);
                    inner.journal.retry_upload_job(
                        &job.segment_id,
                        delay_ms,
                        UploadFailureDetails {
                            error_code: code,
                            http_status,
                            server_code,
                            server_message,
                            request_id,
                        },
                    )?;
                    let mut event = UploadDiagnostic::new(
                        "segment",
                        "retry_scheduled",
                        "retry_wait",
                        job.attempt_count,
                    );
                    event.error_code = Some(code);
                    event.retry_delay_ms = Some(delay_ms);
                    event.http_status = http_status;
                    event.server_code = server_code;
                    event.server_message = server_message;
                    event.request_id = request_id;
                    record_upload_progress(
                        inner,
                        segment_correlation(&job),
                        "warning",
                        true,
                        event,
                    );
                    tracing::warn!(
                        segment_id = %job.segment_id,
                        error_code = code,
                        delay_ms,
                        http_status,
                        server_code,
                        request_id,
                        "R2 upload scheduled for retry"
                    );
                } else {
                    inner.journal.reject_running_upload_job(
                        &job.segment_id,
                        code,
                        http_status,
                        server_code,
                        server_message,
                        request_id,
                    )?;
                    let mut event = UploadDiagnostic::new(
                        "segment",
                        "upload_rejected",
                        "rejected",
                        job.attempt_count,
                    );
                    event.error_code = Some(code);
                    event.http_status = http_status;
                    event.server_code = server_code;
                    event.server_message = server_message;
                    event.request_id = request_id;
                    record_upload_progress(inner, segment_correlation(&job), "error", true, event);
                    tracing::error!(
                        segment_id = %job.segment_id,
                        error_code = code,
                        http_status,
                        server_code,
                        request_id,
                        "R2 upload rejected without changing local RAW facts"
                    );
                }
            }
        }
        return Ok(true);
    }
    if !prefer_session_artifact && process_one_session_artifact(inner, credential)? {
        return Ok(true);
    }
    Ok(false)
}

fn process_one_session_seal(
    inner: &UploaderInner,
    credential: &DeviceCredential,
) -> anyhow::Result<bool> {
    let Some(job) = inner.journal.claim_upload_session_seal()? else {
        return Ok(false);
    };
    let _busy = BusyGuard::enter(&inner.busy);
    let mut body = json!({
        "endedAt": job.ended_at_utc,
        "localFinalState": job.local_final_state,
    });
    if let Some(stop_reason) = &job.stop_reason {
        body["stopReason"] = Value::String(stop_reason.clone());
    }
    let result = inner.transport.json::<Value>(
        Method::POST,
        &format!("v1/sessions/{}:seal", job.remote_session_id),
        credential,
        Some(&idempotency_key("seal", &job.local_session_id)),
        &body,
    );
    match result {
        Ok(_) => {
            inner
                .journal
                .mark_upload_session_sealed(&job.local_session_id)?;
            record_upload_progress(
                inner,
                session_correlation(&job.local_session_id),
                "info",
                true,
                UploadDiagnostic::new("session", "seal_completed", "sealed", job.attempt_count),
            );
        }
        Err(error) => {
            let retryable = error.retryable && job.attempt_count < SESSION_SEAL_MAX_ATTEMPTS;
            let (state, level, error_code, retry_delay_ms) = if retryable {
                let delay_ms = retry_delay_ms(job.attempt_count, &job.local_session_id);
                inner.journal.retry_upload_session_seal(
                    &job.local_session_id,
                    "UPLOAD_SESSION_SEAL_RETRYABLE",
                    delay_ms,
                    error.http_status,
                    error.server_code.as_deref(),
                    error.server_message.as_deref(),
                    error.request_id.as_deref(),
                )?;
                (
                    "retry_wait",
                    "warning",
                    "UPLOAD_SESSION_SEAL_RETRYABLE",
                    Some(delay_ms),
                )
            } else {
                let code = if error.retryable {
                    "UPLOAD_SESSION_SEAL_RETRY_EXHAUSTED"
                } else {
                    "UPLOAD_SESSION_SEAL_REJECTED"
                };
                inner.journal.reject_upload_session_seal(
                    &job.local_session_id,
                    code,
                    error.http_status,
                    error.server_code.as_deref(),
                    error.server_message.as_deref(),
                    error.request_id.as_deref(),
                )?;
                ("rejected", "error", code, None)
            };
            let mut event = UploadDiagnostic::new(
                "session",
                if retryable {
                    "seal_retry_scheduled"
                } else {
                    "seal_rejected"
                },
                state,
                job.attempt_count,
            );
            event.error_code = Some(error_code);
            event.retry_delay_ms = retry_delay_ms;
            event.http_status = error.http_status;
            event.server_code = error.server_code.as_deref();
            event.server_message = error.server_message.as_deref();
            event.request_id = error.request_id.as_deref();
            record_upload_progress(
                inner,
                session_correlation(&job.local_session_id),
                level,
                true,
                event,
            );
            tracing::warn!(
                session_id = %job.local_session_id,
                attempt_count = job.attempt_count,
                retryable,
                http_status = error.http_status,
                server_code = error.server_code.as_deref(),
                request_id = error.request_id.as_deref(),
                "remote session seal attempt failed"
            );
        }
    }
    Ok(true)
}

fn process_one_session_artifact(
    inner: &UploaderInner,
    credential: &DeviceCredential,
) -> anyhow::Result<bool> {
    let Some(job) = inner.journal.claim_next_session_artifact_job()? else {
        return Ok(false);
    };
    let _busy = BusyGuard::enter(&inner.busy);
    let mut started =
        UploadDiagnostic::new("session_artifact", "job_started", "running", job.attempts);
    started.artifact_kind = Some(job.kind.as_str());
    record_upload_progress(
        inner,
        DiagnosticCorrelation {
            session_id: Some(job.session_id.clone()),
            ..DiagnosticCorrelation::default()
        },
        "info",
        false,
        started,
    );
    match upload_session_artifact(inner, credential, &job) {
        Ok(()) => {}
        Err(error) => {
            let code = error.code();
            let retryable = error.retryable();
            let http_status = error.http_status();
            let server_code = error.server_code();
            let server_message = error.server_message();
            let request_id = error.request_id();
            if retryable {
                let delay_ms = retry_delay_ms(
                    job.attempts,
                    &format!("{}:{}", job.session_id, job.kind.as_str()),
                );
                inner
                    .journal
                    .retry_session_artifact(&job.session_id, job.kind, code, delay_ms)?;
                let mut event = UploadDiagnostic::new(
                    "session_artifact",
                    "retry_scheduled",
                    "retry_wait",
                    job.attempts,
                );
                event.artifact_kind = Some(job.kind.as_str());
                event.error_code = Some(code);
                event.retry_delay_ms = Some(delay_ms);
                event.http_status = http_status;
                event.server_code = server_code;
                event.server_message = server_message;
                event.request_id = request_id;
                record_upload_progress(
                    inner,
                    DiagnosticCorrelation {
                        session_id: Some(job.session_id.clone()),
                        ..DiagnosticCorrelation::default()
                    },
                    "warning",
                    true,
                    event,
                );
                tracing::warn!(
                    session_id = %job.session_id,
                    kind = job.kind.as_str(),
                    error_code = code,
                    delay_ms,
                    "session artifact upload scheduled for retry"
                );
            } else {
                inner
                    .journal
                    .reject_session_artifact(&job.session_id, job.kind, code)?;
                let mut event = UploadDiagnostic::new(
                    "session_artifact",
                    "upload_rejected",
                    "rejected",
                    job.attempts,
                );
                event.artifact_kind = Some(job.kind.as_str());
                event.error_code = Some(code);
                event.http_status = http_status;
                event.server_code = server_code;
                event.server_message = server_message;
                event.request_id = request_id;
                record_upload_progress(
                    inner,
                    DiagnosticCorrelation {
                        session_id: Some(job.session_id.clone()),
                        ..DiagnosticCorrelation::default()
                    },
                    "error",
                    true,
                    event,
                );
                tracing::error!(
                    session_id = %job.session_id,
                    kind = job.kind.as_str(),
                    error_code = code,
                    "session artifact upload rejected without changing local RAW facts"
                );
            }
        }
    }
    Ok(true)
}

fn upload_session_artifact(
    inner: &UploaderInner,
    credential: &DeviceCredential,
    claimed: &SessionArtifactUploadJob,
) -> Result<(), AttemptError> {
    let registration = inner
        .journal
        .session_registration_candidate(&claimed.session_id)
        .map_err(|_| AttemptError::Permanent("UPLOAD_JOURNAL_INVALID"))?;
    let remote_session_id = ensure_remote_session(inner, credential, &registration)?;
    ensure_remote_session_sealed(inner, &registration)?;
    let mut job = claimed.clone();
    if job.state == SessionArtifactUploadState::Pending {
        let jobs = inner
            .journal
            .session_artifact_jobs(&job.session_id)
            .map_err(|_| AttemptError::Permanent("UPLOAD_JOURNAL_INVALID"))?;
        let artifacts = resolve_session_artifacts(&inner.raw_root, &jobs)?;
        let declarations = artifacts
            .iter()
            .map(|artifact| {
                (
                    artifact.kind.as_str().to_owned(),
                    json!({
                        "byteSize": artifact.byte_size,
                        "sha256": artifact.sha256,
                    }),
                )
            })
            .collect::<serde_json::Map<_, _>>();
        let response: SessionArtifactsResponse = inner.transport.json(
            Method::PUT,
            &format!("v1/sessions/{remote_session_id}/artifacts"),
            credential,
            None,
            &json!({ "artifacts": declarations }),
        )?;
        if response.session_id != remote_session_id {
            return Err(AttemptError::Permanent("UPLOAD_REMOTE_IDENTITY_CONFLICT"));
        }
        let statuses = response
            .artifacts
            .into_iter()
            .map(|status| SessionArtifactRemoteStatus {
                kind: status.kind,
                object_key: status.object_key,
                expected_sha256: status.expected_sha256,
                expected_size: status.expected_bytes,
                verification_state: status.verification_state,
            })
            .collect::<Vec<_>>();
        inner
            .journal
            .set_session_artifacts_declared(&job.session_id, &statuses)
            .map_err(|_| AttemptError::Permanent("UPLOAD_JOURNAL_INVALID"))?;
        job = inner
            .journal
            .session_artifact_jobs(&job.session_id)
            .map_err(|_| AttemptError::Permanent("UPLOAD_JOURNAL_INVALID"))?
            .into_iter()
            .find(|candidate| candidate.kind == job.kind)
            .ok_or(AttemptError::Permanent("UPLOAD_JOURNAL_INVALID"))?;
    }

    if matches!(
        job.state,
        SessionArtifactUploadState::Verified | SessionArtifactUploadState::Rejected
    ) {
        return Ok(());
    }
    let artifact = resolve_session_artifact(&inner.raw_root, &job)?;
    if matches!(
        job.state,
        SessionArtifactUploadState::Declared | SessionArtifactUploadState::Signed
    ) {
        let object_key = job
            .object_key
            .as_deref()
            .ok_or(AttemptError::Permanent("UPLOAD_JOURNAL_INVALID"))?;
        let signed: SessionArtifactSignResponse = inner.transport.json(
            Method::POST,
            &format!(
                "v1/sessions/{remote_session_id}/artifacts/{}/sign",
                job.kind.as_str()
            ),
            credential,
            None,
            &json!({ "contentMd5": artifact.content_md5 }),
        )?;
        if signed.kind != job.kind || signed.object_key != object_key {
            return Err(AttemptError::Permanent("UPLOAD_REMOTE_IDENTITY_CONFLICT"));
        }
        // The URL and signed headers remain in memory only. Persisting `signed`
        // before the PUT makes a crash resume by asking the API for a fresh URL.
        inner
            .journal
            .mark_session_artifact_signed(&job.session_id, job.kind)
            .map_err(|_| AttemptError::Permanent("UPLOAD_JOURNAL_INVALID"))?;
        let mut started = UploadDiagnostic::new(
            "session_artifact",
            "part_upload_started",
            "running",
            job.attempts,
        );
        started.artifact_kind = Some(job.kind.as_str());
        started.part_number = Some(1);
        started.part_count = Some(1);
        started.byte_size = Some(artifact.byte_size);
        record_upload_progress(
            inner,
            session_correlation(&job.session_id),
            "info",
            false,
            started,
        );
        inner.transport.put_session_artifact(&signed, &artifact)?;
        inner
            .journal
            .mark_session_artifact_put(&job.session_id, job.kind)
            .map_err(|_| AttemptError::Permanent("UPLOAD_JOURNAL_INVALID"))?;
        let mut uploaded =
            UploadDiagnostic::new("session_artifact", "part_uploaded", "running", job.attempts);
        uploaded.artifact_kind = Some(job.kind.as_str());
        uploaded.part_number = Some(1);
        uploaded.part_count = Some(1);
        uploaded.byte_size = Some(artifact.byte_size);
        record_upload_progress(
            inner,
            session_correlation(&job.session_id),
            "info",
            false,
            uploaded,
        );
        job.state = SessionArtifactUploadState::Put;
    }
    if job.state == SessionArtifactUploadState::Put {
        inner
            .journal
            .mark_session_artifact_completing(&job.session_id, job.kind)
            .map_err(|_| AttemptError::Permanent("UPLOAD_JOURNAL_INVALID"))?;
        job.state = SessionArtifactUploadState::Completing;
    }
    if job.state != SessionArtifactUploadState::Completing {
        return Err(AttemptError::Permanent("UPLOAD_JOURNAL_INVALID"));
    }
    let completed: SessionArtifactStatusResponse = inner.transport.post_empty(
        &format!(
            "v1/sessions/{remote_session_id}/artifacts/{}/complete",
            job.kind.as_str()
        ),
        credential,
    )?;
    if completed.kind != job.kind {
        return Err(AttemptError::Permanent("UPLOAD_REMOTE_IDENTITY_CONFLICT"));
    }
    let (verified, state, level, error_code) = match completed.verification_state.as_str() {
        "verified" => (true, "verified", "info", None),
        "rejected" => (false, "rejected", "error", Some("UPLOAD_REMOTE_REJECTED")),
        _ => return Err(AttemptError::Permanent("UPLOAD_RESPONSE_INVALID")),
    };
    inner
        .journal
        .finish_session_artifact(&job.session_id, job.kind, verified)
        .map_err(|_| AttemptError::Permanent("UPLOAD_JOURNAL_INVALID"))?;
    let mut event = UploadDiagnostic::new(
        "session_artifact",
        "verification_completed",
        state,
        job.attempts,
    );
    event.artifact_kind = Some(job.kind.as_str());
    event.byte_size = Some(artifact.byte_size);
    event.error_code = error_code;
    record_upload_progress(
        inner,
        session_correlation(&job.session_id),
        level,
        true,
        event,
    );
    Ok(())
}

fn ensure_remote_session(
    inner: &UploaderInner,
    credential: &DeviceCredential,
    candidate: &SessionRegistrationCandidate,
) -> Result<String, AttemptError> {
    if let Some(remote_session_id) = candidate.remote_session_id.clone() {
        return Ok(remote_session_id);
    }
    let response: SessionResponse = inner.transport.json(
        Method::POST,
        "v1/sessions",
        credential,
        Some(&idempotency_key("session", &candidate.local_session_id)),
        &session_body(
            candidate,
            inner
                .journal
                .session_view_calibration_evidence(&candidate.local_session_id)
                .map_err(|_| AttemptError::Permanent("UPLOAD_JOURNAL_INVALID"))?
                .as_ref(),
            inner.transport.environment,
        )?,
    )?;
    inner
        .journal
        .set_remote_session_id(&candidate.local_session_id, &response.session_id)
        .map_err(|_| AttemptError::Permanent("UPLOAD_JOURNAL_INVALID"))?;
    Ok(response.session_id)
}

fn ensure_remote_session_sealed(
    inner: &UploaderInner,
    candidate: &SessionRegistrationCandidate,
) -> Result<(), AttemptError> {
    if candidate.remote_session_sealed {
        return Ok(());
    }
    let (seal_state, _) = inner
        .journal
        .upload_session_seal_state(&candidate.local_session_id)
        .map_err(|_| AttemptError::Permanent("UPLOAD_JOURNAL_INVALID"))?;
    if seal_state == "rejected" {
        return Err(AttemptError::Permanent("UPLOAD_SESSION_SEAL_REJECTED"));
    }
    if !inner
        .journal
        .session_can_be_sealed(&candidate.local_session_id)
        .map_err(|_| AttemptError::Permanent("UPLOAD_JOURNAL_INVALID"))?
    {
        return Err(AttemptError::Retryable("UPLOAD_SESSION_NOT_SEALED"));
    }
    Err(AttemptError::Retryable("UPLOAD_SESSION_SEAL_PENDING"))
}

#[derive(Debug)]
enum AttemptError {
    Retryable(&'static str),
    Permanent(&'static str),
    Transport(TransportError),
}

impl From<TransportError> for AttemptError {
    fn from(error: TransportError) -> Self {
        Self::Transport(error)
    }
}

impl AttemptError {
    fn code(&self) -> &'static str {
        match self {
            Self::Retryable(code) | Self::Permanent(code) => code,
            Self::Transport(error) => error.code,
        }
    }

    fn retryable(&self) -> bool {
        match self {
            Self::Retryable(_) => true,
            Self::Permanent(_) => false,
            Self::Transport(error) => error.retryable,
        }
    }

    fn http_status(&self) -> Option<u16> {
        match self {
            Self::Transport(error) => error.http_status,
            _ => None,
        }
    }

    fn server_code(&self) -> Option<&str> {
        match self {
            Self::Transport(error) => error.server_code.as_deref(),
            _ => None,
        }
    }

    fn server_message(&self) -> Option<&str> {
        match self {
            Self::Transport(error) => error.server_message.as_deref(),
            _ => None,
        }
    }

    fn request_id(&self) -> Option<&str> {
        match self {
            Self::Transport(error) => error.request_id.as_deref(),
            _ => None,
        }
    }
}

fn upload_job(
    inner: &UploaderInner,
    credential: &DeviceCredential,
    job: &UploadJob,
) -> Result<(), AttemptError> {
    let mut candidate = inner
        .journal
        .upload_candidate(&job.segment_id)
        .map_err(|_| AttemptError::Permanent("UPLOAD_JOURNAL_INVALID"))?;
    let artifacts = resolve_artifacts(
        &inner.raw_root,
        &candidate.local_session_id,
        &candidate.artifacts,
    )?;
    let mut sources_verified =
        UploadDiagnostic::new("segment", "sources_verified", "running", job.attempt_count);
    sources_verified.artifact_count = Some(artifacts.len().try_into().unwrap_or(u32::MAX));
    sources_verified.byte_size = Some(artifacts.iter().fold(0_u64, |total, artifact| {
        total.saturating_add(artifact.byte_size)
    }));
    record_upload_progress(
        inner,
        segment_correlation(job),
        "info",
        false,
        sources_verified,
    );

    let registration = session_registration_from_upload(&candidate);
    let remote_session_id = ensure_remote_session(inner, credential, &registration)?;
    candidate.remote_session_id = Some(remote_session_id.clone());

    let remote_run_id = match candidate.remote_run_id.clone() {
        Some(id) => id,
        None => {
            let path = format!(
                "v1/sessions/{remote_session_id}/runs/{}",
                candidate.local_run_id
            );
            let response: RunResponse = inner.transport.json(
                Method::PUT,
                &path,
                credential,
                Some(&idempotency_key(
                    "run",
                    &format!("{}:{}", candidate.local_session_id, candidate.local_run_id),
                )),
                &run_body(&candidate),
            )?;
            inner
                .journal
                .set_remote_run_id(&candidate.run_journal_id, &response.run_id)
                .map_err(|_| AttemptError::Permanent("UPLOAD_JOURNAL_INVALID"))?;
            candidate.remote_run_id = Some(response.run_id.clone());
            response.run_id
        }
    };

    let (remote_clip_id, clip_revision) =
        match (candidate.remote_clip_id.clone(), candidate.clip_revision) {
            (Some(id), Some(revision)) => (id, revision),
            (None, None) => {
                let path = format!(
                    "v1/sessions/{remote_session_id}/runs/{}/clips/{}",
                    candidate.local_run_id, candidate.segment_index
                );
                let response: ClipResponse = inner.transport.json(
                    Method::PUT,
                    &path,
                    credential,
                    Some(&idempotency_key("clip", &candidate.segment_id)),
                    &clip_body(&candidate, &artifacts, inner.transport.environment)?,
                )?;
                ensure_remote_run(&response, &remote_run_id)?;
                inner
                    .journal
                    .set_remote_clip(&candidate.segment_id, &response.clip_id, response.revision)
                    .map_err(|_| AttemptError::Permanent("UPLOAD_JOURNAL_INVALID"))?;
                (response.clip_id, response.revision)
            }
            _ => return Err(AttemptError::Permanent("UPLOAD_JOURNAL_INVALID")),
        };

    let intent = match candidate.remote_upload_intent_id.clone() {
        Some(id) => {
            // The server returns the same durable intent for a clip. Replaying
            // this operation also recovers its server-owned part sizing.
            let response: UploadIntentResponse = inner.transport.json(
                Method::POST,
                &format!("v1/clips/{remote_clip_id}/upload-intents"),
                credential,
                Some(&idempotency_key("intent", &candidate.segment_id)),
                &json!({ "clipRevision": clip_revision, "transferMode": "server_owned_r2_multipart" }),
            )?;
            if response.upload_intent_id != id {
                return Err(AttemptError::Permanent("UPLOAD_REMOTE_IDENTITY_CONFLICT"));
            }
            response
        }
        None => {
            let response: UploadIntentResponse = inner.transport.json(
                Method::POST,
                &format!("v1/clips/{remote_clip_id}/upload-intents"),
                credential,
                Some(&idempotency_key("intent", &candidate.segment_id)),
                &json!({ "clipRevision": clip_revision, "transferMode": "server_owned_r2_multipart" }),
            )?;
            inner
                .journal
                .set_remote_upload_intent(&candidate.segment_id, &response.upload_intent_id)
                .map_err(|_| AttemptError::Permanent("UPLOAD_JOURNAL_INVALID"))?;
            response
        }
    };

    let mut plan_ready =
        UploadDiagnostic::new("segment", "upload_plan_ready", "running", job.attempt_count);
    plan_ready.artifact_count = Some(intent.artifacts.len().try_into().unwrap_or(u32::MAX));
    plan_ready.part_count = Some(
        intent
            .artifacts
            .values()
            .fold(0_u32, |total, plan| total.saturating_add(plan.part_count)),
    );
    record_upload_progress(inner, segment_correlation(job), "info", false, plan_ready);

    upload_parts(
        inner,
        credential,
        &candidate,
        &artifacts,
        &intent,
        job.attempt_count,
    )?;
    let completion = completion_body(
        &inner
            .journal
            .uploaded_parts(&candidate.segment_id)
            .map_err(|_| AttemptError::Permanent("UPLOAD_JOURNAL_INVALID"))?,
        &candidate.artifacts,
    )?;
    let _: StoredResponse = inner.transport.json(
        Method::POST,
        &format!("v1/upload-intents/{}/complete", intent.upload_intent_id),
        credential,
        Some(&idempotency_key("complete", &candidate.segment_id)),
        &completion,
    )?;
    inner
        .journal
        .mark_upload_stored(&candidate.segment_id)
        .map_err(|_| AttemptError::Permanent("UPLOAD_JOURNAL_INVALID"))?;
    let mut stored =
        UploadDiagnostic::new("segment", "upload_completed", "stored", job.attempt_count);
    stored.artifact_count = Some(artifacts.len().try_into().unwrap_or(u32::MAX));
    stored.part_count = plan_ready.part_count;
    stored.byte_size = sources_verified.byte_size;
    record_upload_progress(inner, segment_correlation(job), "info", true, stored);
    Ok(())
}

fn poll_status(
    inner: &UploaderInner,
    credential: &DeviceCredential,
    job: &UploadJob,
) -> anyhow::Result<()> {
    record_upload_progress(
        inner,
        segment_correlation(job),
        "info",
        false,
        UploadDiagnostic::new(
            "segment",
            "verification_started",
            "stored",
            job.attempt_count,
        ),
    );
    let Some(remote_clip_id) = job.remote_clip_id.as_deref() else {
        inner.journal.finish_upload_status(
            &job.segment_id,
            false,
            Some("UPLOAD_JOURNAL_INVALID"),
        )?;
        let mut event = UploadDiagnostic::new(
            "segment",
            "verification_completed",
            "rejected",
            job.attempt_count,
        );
        event.error_code = Some("UPLOAD_JOURNAL_INVALID");
        record_upload_progress(inner, segment_correlation(job), "error", true, event);
        return Ok(());
    };
    match inner.transport.get_status(remote_clip_id, credential) {
        Ok(status) if status.retention_state == "purged" => {
            inner.journal.finish_upload_status_with_retention(
                &job.segment_id,
                false,
                Some("UPLOAD_REMOTE_PURGED"),
                Some(&status.retention_state),
            )?;
            let mut event = UploadDiagnostic::new(
                "segment",
                "verification_completed",
                "purged",
                job.attempt_count,
            );
            event.error_code = Some("UPLOAD_REMOTE_PURGED");
            event.server_code = status.rejection_code.as_deref();
            record_upload_progress(inner, segment_correlation(job), "error", true, event);
        }
        Ok(status) if status.verification_state == "verified" => {
            inner.journal.finish_upload_status_with_retention(
                &job.segment_id,
                true,
                None,
                Some(&status.retention_state),
            )?;
            record_upload_progress(
                inner,
                segment_correlation(job),
                "info",
                true,
                UploadDiagnostic::new(
                    "segment",
                    "verification_completed",
                    "verified",
                    job.attempt_count,
                ),
            );
        }
        Ok(status) if status.verification_state == "rejected" => {
            inner.journal.finish_upload_status_with_retention(
                &job.segment_id,
                false,
                Some("UPLOAD_REMOTE_REJECTED"),
                Some(&status.retention_state),
            )?;
            let mut event = UploadDiagnostic::new(
                "segment",
                "verification_completed",
                "rejected",
                job.attempt_count,
            );
            event.error_code = Some("UPLOAD_REMOTE_REJECTED");
            event.server_code = status.rejection_code.as_deref();
            record_upload_progress(inner, segment_correlation(job), "error", true, event);
        }
        Ok(_) => {
            inner
                .journal
                .reschedule_upload_status(&job.segment_id, STATUS_POLL_INTERVAL_MS)?;
            let mut event = UploadDiagnostic::new(
                "segment",
                "verification_pending",
                "stored",
                job.attempt_count,
            );
            event.retry_delay_ms = Some(STATUS_POLL_INTERVAL_MS);
            record_upload_progress(inner, segment_correlation(job), "info", false, event);
        }
        Err(error) => {
            let status_attempt = job.status_attempt_count.saturating_add(1);
            let retryable = error.retryable && status_attempt < STATUS_POLL_MAX_ERRORS;
            let (state, error_code, delay_ms) = if retryable {
                let delay_ms = retry_delay_ms(status_attempt, &job.segment_id);
                inner.journal.retry_upload_status(
                    &job.segment_id,
                    "UPLOAD_STATUS_RETRYABLE",
                    delay_ms,
                )?;
                ("stored", "UPLOAD_STATUS_RETRYABLE", Some(delay_ms))
            } else {
                let code = if error.retryable {
                    "UPLOAD_STATUS_RETRY_EXHAUSTED"
                } else {
                    "UPLOAD_STATUS_REQUEST_REJECTED"
                };
                inner
                    .journal
                    .finish_upload_status(&job.segment_id, false, Some(code))?;
                ("rejected", code, None)
            };
            let mut event = UploadDiagnostic::new(
                "segment",
                if retryable {
                    "verification_retry_scheduled"
                } else {
                    "verification_status_rejected"
                },
                state,
                status_attempt,
            );
            event.error_code = Some(error_code);
            event.retry_delay_ms = delay_ms;
            event.http_status = error.http_status;
            event.server_code = error.server_code.as_deref();
            event.server_message = error.server_message.as_deref();
            event.request_id = error.request_id.as_deref();
            record_upload_progress(
                inner,
                segment_correlation(job),
                if retryable { "warning" } else { "error" },
                true,
                event,
            );
        }
    }
    Ok(())
}

#[derive(Clone)]
struct ResolvedArtifact {
    kind: &'static str,
    path: PathBuf,
    byte_size: u64,
    sha256: String,
}

#[derive(Clone)]
struct ResolvedSessionArtifact {
    kind: SessionArtifactKind,
    path: PathBuf,
    byte_size: u64,
    sha256: String,
    content_md5: String,
}

fn resolve_session_artifacts(
    raw_root: &Path,
    jobs: &[SessionArtifactUploadJob],
) -> Result<Vec<ResolvedSessionArtifact>, AttemptError> {
    jobs.iter()
        .map(|job| resolve_session_artifact(raw_root, job))
        .collect()
}

fn resolve_session_artifact(
    raw_root: &Path,
    job: &SessionArtifactUploadJob,
) -> Result<ResolvedSessionArtifact, AttemptError> {
    let canonical_root = raw_root
        .canonicalize()
        .map_err(|_| AttemptError::Permanent("UPLOAD_SOURCE_MISSING"))?;
    let relative = Path::new(&job.local_path);
    if relative.is_absolute()
        || relative
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(AttemptError::Permanent("UPLOAD_SOURCE_PATH_INVALID"));
    }
    let path = raw_root.join(relative);
    let canonical = path
        .canonicalize()
        .map_err(|_| AttemptError::Permanent("UPLOAD_SOURCE_MISSING"))?;
    if !canonical.starts_with(&canonical_root) || !canonical.is_file() {
        return Err(AttemptError::Permanent("UPLOAD_SOURCE_PATH_INVALID"));
    }
    let (byte_size, sha256) = sha256_file(&canonical)
        .map_err(|_| AttemptError::Permanent("UPLOAD_SOURCE_READ_FAILED"))?;
    if byte_size != job.expected_size || sha256 != job.expected_sha256 {
        return Err(AttemptError::Permanent("UPLOAD_SOURCE_CHANGED"));
    }
    let content_md5 = part_md5(&canonical, 0, byte_size)
        .map_err(|_| AttemptError::Permanent("UPLOAD_SOURCE_READ_FAILED"))?;
    if content_md5 != job.content_md5 {
        return Err(AttemptError::Permanent("UPLOAD_SOURCE_CHANGED"));
    }
    Ok(ResolvedSessionArtifact {
        kind: job.kind,
        path: canonical,
        byte_size,
        sha256,
        content_md5,
    })
}

fn resolve_artifacts(
    raw_root: &Path,
    local_session_id: &str,
    sources: &[UploadArtifactSource],
) -> Result<Vec<ResolvedArtifact>, AttemptError> {
    let canonical_root = raw_root
        .canonicalize()
        .map_err(|_| AttemptError::Permanent("UPLOAD_SOURCE_MISSING"))?;
    let mut session_components = Path::new(local_session_id).components();
    if !matches!(session_components.next(), Some(Component::Normal(_)))
        || session_components.next().is_some()
    {
        return Err(AttemptError::Permanent("UPLOAD_SOURCE_PATH_INVALID"));
    }
    let canonical_session_root = raw_root
        .join(local_session_id)
        .canonicalize()
        .map_err(|_| AttemptError::Permanent("UPLOAD_SOURCE_MISSING"))?;
    if !canonical_session_root.starts_with(&canonical_root) || !canonical_session_root.is_dir() {
        return Err(AttemptError::Permanent("UPLOAD_SOURCE_PATH_INVALID"));
    }
    sources
        .iter()
        .map(|source| {
            let relative = Path::new(&source.relative_path);
            if relative.is_absolute()
                || relative
                    .components()
                    .any(|part| !matches!(part, Component::Normal(_)))
            {
                return Err(AttemptError::Permanent("UPLOAD_SOURCE_PATH_INVALID"));
            }
            let path = canonical_session_root.join(relative);
            let canonical = path
                .canonicalize()
                .map_err(|_| AttemptError::Permanent("UPLOAD_SOURCE_MISSING"))?;
            if !canonical.starts_with(&canonical_session_root) || !canonical.is_file() {
                return Err(AttemptError::Permanent("UPLOAD_SOURCE_PATH_INVALID"));
            }
            let (byte_size, sha256) = sha256_file(&canonical)
                .map_err(|_| AttemptError::Permanent("UPLOAD_SOURCE_READ_FAILED"))?;
            if source
                .expected_sha256
                .as_deref()
                .is_some_and(|expected| expected != sha256)
            {
                return Err(AttemptError::Permanent("UPLOAD_SOURCE_CHANGED"));
            }
            Ok(ResolvedArtifact {
                kind: source.kind,
                path: canonical,
                byte_size,
                sha256,
            })
        })
        .collect()
}

/// 服务端 intent 展开后的一个 part。持有 artifact 的路径借用而不是索引，
/// 是让并发驱动只依赖这一个扁平列表，不必再回头查 artifact 表。
#[derive(Debug)]
struct PlannedPart<'a> {
    artifact_kind: &'static str,
    path: &'a Path,
    provider_attempt_no: u32,
    part_number: u32,
    part_count: u32,
    offset: u64,
    size: u64,
}

/// 把 intent 的每 artifact 计划摊平成一条 part 列表。这一步只算边界、不碰
/// 磁盘也不碰网络：并发驱动需要一个**长度确定**的工作队列，而分片边界的
/// 全部校验（part_count 对账、offset 溢出、末片非空）都必须在派活之前一次
/// 性做完——否则一个坏计划会先烧掉几条并发连接才被发现。
fn plan_parts<'a>(
    artifacts: &'a [ResolvedArtifact],
    intent: &UploadIntentResponse,
) -> Result<Vec<PlannedPart<'a>>, AttemptError> {
    let mut planned = Vec::new();
    for artifact in artifacts {
        let plan = intent
            .artifacts
            .get(artifact.kind)
            .ok_or(AttemptError::Permanent("UPLOAD_PLAN_INVALID"))?;
        if plan.part_count == 0 || plan.part_size_bytes == 0 {
            return Err(AttemptError::Permanent("UPLOAD_PLAN_INVALID"));
        }
        if plan.provider_attempt_number == 0 {
            return Err(AttemptError::Permanent("UPLOAD_PLAN_INVALID"));
        }
        let expected_part_count: u32 = artifact
            .byte_size
            .div_ceil(plan.part_size_bytes)
            .try_into()
            .map_err(|_| AttemptError::Permanent("UPLOAD_PLAN_INVALID"))?;
        if plan.part_count != expected_part_count {
            return Err(AttemptError::Permanent("UPLOAD_PLAN_INVALID"));
        }
        for part_number in 1..=plan.part_count {
            let offset = u64::from(part_number - 1)
                .checked_mul(plan.part_size_bytes)
                .ok_or(AttemptError::Permanent("UPLOAD_PLAN_INVALID"))?;
            let size = artifact
                .byte_size
                .saturating_sub(offset)
                .min(plan.part_size_bytes);
            if size == 0 {
                return Err(AttemptError::Permanent("UPLOAD_PLAN_INVALID"));
            }
            planned.push(PlannedPart {
                artifact_kind: artifact.kind,
                path: &artifact.path,
                provider_attempt_no: plan.provider_attempt_number,
                part_number,
                part_count: plan.part_count,
                offset,
                size,
            });
        }
    }
    Ok(planned)
}

fn part_concurrency() -> usize {
    part_concurrency_from(std::env::var(PART_CONCURRENCY_ENV).ok().as_deref())
}

/// 无法解析、越界或为 0 的覆盖值一律退回默认，而不是报错：并发度是性能旋钮，
/// 一个打错的环境变量不该把上传变成不可用。
fn part_concurrency_from(raw: Option<&str>) -> usize {
    raw.and_then(|raw| raw.trim().parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_PART_CONCURRENCY)
        .min(MAX_PART_CONCURRENCY)
}

/// 并发度随本 segment 已重试的次数减半。
///
/// 这是并发化唯一一处真正的行为回退的兜底。[`PUT_TIMEOUT`] 是每 part 的预算，
/// C 条并发让每条各自慢约 C 倍，于是存在一段链路速度——8 MiB / 600 s 算下来
/// 约 0.11–0.45 Mbit/s——串行时代每次 attempt 至少能传完一片、有净进度，C=4
/// 却是四片一起撞超时、一片都传不完。segment 上传的重试**没有次数上限**
/// （只有 seal 有 [`SESSION_SEAL_MAX_ATTEMPTS`]），所以那不会变成一次可见的
/// 失败，而是一个不报错也不前进的活锁。
///
/// 每次重试减半，两轮之内收敛回串行——那正是这条链路当初能前进的配置。计数是
/// per-segment 的：健康的 segment 永远从满并发起步，一次偶发 5xx 只让这一个
/// segment 慢一轮。
fn part_concurrency_for_attempt(configured: usize, attempt_count: u32) -> usize {
    configured
        .checked_shr(attempt_count.saturating_sub(1))
        .unwrap_or(0)
        .max(1)
}

/// 固定并发度的 part 调度器。工作线程从同一个游标认领 part，谁先空谁先取，
/// 因此一个慢 part 不会挡住后面的队列——这正是串行版本做不到的事。
///
/// 出错即停止认领新 part：一次 attempt 只需要一个失败结论，继续把上行带宽
/// 烧在注定要整体重来的 part 上没有意义。已经在飞的 part 仍会跑完，它们写进
/// journal 的 ETag 是下一次 attempt 的净进度，丢掉才是浪费。
///
/// 保留下来的错误取更严重的那个，见 [`keep_worse_failure`]。
fn upload_parts_concurrently<F>(
    planned: &[PlannedPart<'_>],
    concurrency: usize,
    upload_one: F,
) -> Result<(), AttemptError>
where
    F: Fn(&PlannedPart<'_>) -> Result<(), AttemptError> + Sync,
{
    let workers = concurrency
        .clamp(1, MAX_PART_CONCURRENCY)
        .min(planned.len());
    if workers <= 1 {
        for part in planned {
            upload_one(part)?;
        }
        return Ok(());
    }
    let cursor = AtomicUsize::new(0);
    let failure: Mutex<Option<AttemptError>> = Mutex::new(None);
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    // 锁只在这一行的生命周期内持有：`upload_one` 会阻塞几十秒到
                    // 几分钟，绝不能在持锁状态下进去。
                    let already_failed = failure.lock().is_some();
                    if already_failed {
                        return;
                    }
                    let index = cursor.fetch_add(1, Ordering::AcqRel);
                    let Some(part) = planned.get(index) else {
                        return;
                    };
                    if let Err(error) = upload_one(part) {
                        keep_worse_failure(&mut failure.lock(), error);
                        return;
                    }
                }
            });
        }
    });
    match failure.into_inner() {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// 一批并发 part 可以同时以不同方式失败，但一次 attempt 只能有一个结论。
/// 取更严重的那个：`Permanent` 是终局判决（计划变了、源文件变了），必须压过
/// 同批里偶发的 `Retryable`——把 `UPLOAD_PLAN_CHANGED` 误报成弱网，这个
/// segment 会带着一个永远拼不回去的分片列表无限重试。同严重度先到先得。
fn keep_worse_failure(slot: &mut Option<AttemptError>, error: AttemptError) {
    if slot
        .as_ref()
        .is_none_or(|kept| kept.retryable() && !error.retryable())
    {
        *slot = Some(error);
    }
}

fn upload_parts(
    inner: &UploaderInner,
    credential: &DeviceCredential,
    candidate: &UploadCandidate,
    artifacts: &[ResolvedArtifact],
    intent: &UploadIntentResponse,
    attempt_count: u32,
) -> Result<(), AttemptError> {
    let saved = inner
        .journal
        .uploaded_parts(&candidate.segment_id)
        .map_err(|_| AttemptError::Permanent("UPLOAD_JOURNAL_INVALID"))?;
    let planned = plan_parts(artifacts, intent)?;
    let concurrency = part_concurrency_for_attempt(part_concurrency(), attempt_count);
    upload_parts_concurrently(&planned, concurrency, |part| {
        upload_one_part(
            inner,
            credential,
            candidate,
            intent,
            attempt_count,
            &saved,
            part,
        )
    })
}

/// 一个 part 的完整生命周期：算 MD5 → 认领或跳过 → 签名 → PUT → 落 journal。
///
/// 签名留在这里而不是提前批量签，是因为 presign TTL 上限 900 s 而
/// [`PUT_TIMEOUT`] 是 600 s：每个 part 在自己即将上传前才签，TTL 时钟就只覆盖
/// 它自己的传输窗口。提前把上百个 URL 一次签出来，排在队尾的那些会在还没轮到
/// 时就过期。
fn upload_one_part(
    inner: &UploaderInner,
    credential: &DeviceCredential,
    candidate: &UploadCandidate,
    intent: &UploadIntentResponse,
    attempt_count: u32,
    saved: &[UploadedPart],
    part: &PlannedPart<'_>,
) -> Result<(), AttemptError> {
    let PlannedPart {
        artifact_kind,
        path,
        provider_attempt_no,
        part_number,
        part_count,
        offset,
        size,
    } = *part;
    let content_md5 = part_md5(path, offset, size)
        .map_err(|_| AttemptError::Permanent("UPLOAD_SOURCE_READ_FAILED"))?;
    if let Some(existing) = saved
        .iter()
        .find(|saved| saved.artifact_kind == artifact_kind && saved.part_number == part_number)
        && reusable_uploaded_part(existing, provider_attempt_no, offset, size, &content_md5)?
    {
        return Ok(());
    }
    let signed: SignPartsResponse = inner.transport.json(
        Method::POST,
        &format!("v1/upload-intents/{}/parts:sign", intent.upload_intent_id),
        credential,
        Some(&idempotency_key(
            "sign",
            &format!(
                "{}:{artifact_kind}:{part_number}:{content_md5}",
                candidate.segment_id
            ),
        )),
        &json!({
            "parts": [{
                "artifactType": artifact_kind,
                "partNumber": part_number,
                "byteSize": size,
                "contentMd5": content_md5,
            }]
        }),
    )?;
    let [signed_part] = signed
        .parts
        .try_into()
        .map_err(|_| AttemptError::Permanent("UPLOAD_PLAN_INVALID"))?;
    if signed_part.artifact_type != artifact_kind || signed_part.part_number != part_number {
        return Err(AttemptError::Permanent("UPLOAD_PLAN_INVALID"));
    }
    let correlation = || {
        DiagnosticCorrelation::segment(
            &candidate.local_session_id,
            &candidate.local_run_id,
            candidate.segment_index,
        )
    };
    let mut started =
        UploadDiagnostic::new("segment", "part_upload_started", "running", attempt_count);
    started.artifact_kind = Some(artifact_kind);
    started.part_number = Some(part_number);
    started.part_count = Some(part_count);
    started.byte_size = Some(size);
    record_upload_progress(inner, correlation(), "info", false, started);
    let etag = inner
        .transport
        .put_part(&signed_part, path, offset, size, &content_md5)?;
    inner
        .journal
        .save_uploaded_part(
            &candidate.segment_id,
            &UploadedPart {
                artifact_kind: artifact_kind.into(),
                provider_attempt_no,
                part_number,
                byte_offset: offset,
                byte_size: size,
                content_md5,
                etag,
            },
        )
        .map_err(|_| AttemptError::Permanent("UPLOAD_JOURNAL_INVALID"))?;
    let mut uploaded = UploadDiagnostic::new("segment", "part_uploaded", "running", attempt_count);
    uploaded.artifact_kind = Some(artifact_kind);
    uploaded.part_number = Some(part_number);
    uploaded.part_count = Some(part_count);
    uploaded.byte_size = Some(size);
    record_upload_progress(inner, correlation(), "info", false, uploaded);
    Ok(())
}

fn reusable_uploaded_part(
    existing: &UploadedPart,
    provider_attempt_no: u32,
    offset: u64,
    size: u64,
    content_md5: &str,
) -> Result<bool, AttemptError> {
    if existing.byte_offset != offset
        || existing.byte_size != size
        || existing.content_md5 != content_md5
    {
        return Err(AttemptError::Permanent("UPLOAD_PLAN_CHANGED"));
    }
    if existing.provider_attempt_no > provider_attempt_no {
        return Err(AttemptError::Retryable("UPLOAD_PLAN_STALE"));
    }
    Ok(existing.provider_attempt_no == provider_attempt_no)
}

fn session_registration_from_upload(candidate: &UploadCandidate) -> SessionRegistrationCandidate {
    SessionRegistrationCandidate {
        local_session_id: candidate.local_session_id.clone(),
        registration_state: candidate.session_registration_state.clone(),
        session_state: candidate.session_state.clone(),
        session_schema_version: candidate.session_schema_version.clone(),
        started_at_utc: candidate.session_started_at_utc.clone(),
        ended_at_utc: candidate.session_ended_at_utc.clone(),
        stop_reason: candidate.session_stop_reason.clone(),
        non_trainable: candidate.non_trainable,
        consent_id: candidate.consent_id.clone(),
        session_contract: candidate.session_contract.clone(),
        catalog_id: candidate.catalog_id.clone(),
        catalog_version: candidate.catalog_version.clone(),
        catalog_hash: candidate.catalog_hash.clone(),
        device_profile_id: candidate.device_profile_id.clone(),
        device_profile_version: candidate.device_profile_version.clone(),
        device_profile_hash: candidate.device_profile_hash.clone(),
        recording_profile_id: candidate.recording_profile_id.clone(),
        recording_profile_version: candidate.recording_profile_version.clone(),
        recording_profile_hash: candidate.recording_profile_hash.clone(),
        game_id: candidate.game_id.clone(),
        game_profile_version: candidate.game_profile_version.clone(),
        game_profile_hash: candidate.game_profile_hash.clone(),
        game_executable_sha256: candidate.game_executable_sha256.clone(),
        game_executable_sha256_missing_reason: candidate
            .game_executable_sha256_missing_reason
            .clone(),
        mapping_id: candidate.mapping_id.clone(),
        mapping_version: candidate.mapping_version.clone(),
        mapping_hash: candidate.mapping_hash.clone(),
        width: candidate.width,
        height: candidate.height,
        fps_num: candidate.fps_num,
        fps_den: candidate.fps_den,
        capture_engine: candidate.capture_engine.clone(),
        remote_session_id: candidate.remote_session_id.clone(),
        remote_session_sealed: false,
    }
}

fn session_body(
    candidate: &SessionRegistrationCandidate,
    view_scale: Option<&SessionViewCalibrationEvidence>,
    environment: ApiEnvironment,
) -> Result<Value, AttemptError> {
    // 这一场属于哪一代，只由**持久化的** session schema 回答（issue #855 §B：
    // v1/v2 并存期的唯一路由键）。不读目录、不读今天的构建，也不接受重新声明。
    let contract = SessionContract::for_session_schema_version(&candidate.session_schema_version)
        .ok_or(AttemptError::Permanent("UPLOAD_JOURNAL_INVALID"))?;
    // journal 里那一列必须与 schema 版本自洽——这是本地 v33 的成对 CHECK 在出境
    // 前的最后一次复核。一份两列打架的会话宁可停在这里，也不要按其中一列上传：
    // 挑错了就是把一整场采集送进错误的车道。
    if candidate.session_contract != contract.source_family() {
        return Err(AttemptError::Permanent("UPLOAD_JOURNAL_INVALID"));
    }
    let mut body = json!({
        "localSessionId": candidate.local_session_id,
        "sessionSchemaVersion": candidate.session_schema_version,
        "startedAt": candidate.started_at_utc,
        "localState": candidate.registration_state,
        "classification": classification_for(
            candidate.non_trainable,
            environment,
            &candidate.session_schema_version,
        )?,
        "clientVersion": env!("CARGO_PKG_VERSION"),
        // 中性契约的 Catalog 身份只有 id + version + contentHash：**provenance 一个
        // 字节没少，车道词没有了**（issue #855 §A.4）。服务端的
        // `catalogIdentitySchemaFor` 按家族选形状，多带 `mode` 会被 `.strict()` 拒。
        "catalog": snapshot(
            &candidate.catalog_id,
            &candidate.catalog_version,
            &candidate.catalog_hash,
            contract.legacy_catalog_mode(),
        ),
        // 身份观察（docs/55 §3.1）：指纹与缺席原因严格互斥，服务端 contracts
        // 的 refine 会拒收「都在」或「都缺」。`Option` 序列化成 null 正是契约
        // 里的 nullish，不需要在这里挑字段。
        "game": {
            "id": candidate.game_id,
            "version": candidate.game_profile_version,
            "contentHash": candidate.game_profile_hash,
            "executableSha256": candidate.game_executable_sha256,
            "executableSha256MissingReason": candidate.game_executable_sha256_missing_reason,
        },
        "deviceProfile": snapshot(&candidate.device_profile_id, &candidate.device_profile_version, &candidate.device_profile_hash, None),
        "recordingProfile": {
            "id": candidate.recording_profile_id,
            "version": candidate.recording_profile_version,
            "contentHash": candidate.recording_profile_hash,
            "width": candidate.width,
            "height": candidate.height,
            "fpsNum": candidate.fps_num,
            "fpsDen": candidate.fps_den,
            "captureEngine": candidate.capture_engine,
        },
        "mapping": snapshot(&candidate.mapping_id, &candidate.mapping_version, &candidate.mapping_hash, None),
    });
    // 视角标定声明（docs/46 §5.2）：evidence-only 摘要，无引用则整字段缺席。
    if let Some(view_scale) = view_scale {
        body["viewScaleCalibration"] = json!({
            "sha256": view_scale.file_sha256,
            "verdict": view_scale.verdict,
            "deviationPct": view_scale.deviation_pct,
            "pollingHzP50": view_scale.polling_hz_p50,
        });
    }
    // The consented lane must name the consent it recorded under; the server
    // cross-checks it against the grant's consent and refuses a mismatch. A
    // consented session with no local consent id cannot exist (schema CHECK),
    // so its absence here is journal corruption, not a lane choice.
    if !candidate.non_trainable {
        let consent_id = candidate
            .consent_id
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .ok_or(AttemptError::Permanent("UPLOAD_JOURNAL_INVALID"))?;
        body["declaredConsentId"] = Value::String(consent_id.to_owned());
    }
    Ok(body)
}

fn run_body(candidate: &UploadCandidate) -> Value {
    let mut body = json!({
        "sequence": candidate.run_sequence,
        "localState": candidate.run_registration_state,
        "startedAt": candidate.run_started_at_utc,
        "endedAt": candidate.run_ended_at_utc,
        "qpcFrequency": candidate.qpc_frequency,
        "sessionFrameStart": candidate.session_frame_start,
        "preflight": {
            "schemaVersion": "dm-preflight-evidence-1",
            "id": candidate.preflight_id,
            "result": candidate.preflight_result,
            "evidenceHash": candidate.preflight_evidence_hash,
        },
        "capture": {
            "width": candidate.width,
            "height": candidate.height,
            "fpsNum": candidate.fps_num,
            "fpsDen": candidate.fps_den,
            "captureEngine": candidate.capture_engine,
            "encoderBackend": candidate.encoder_backend,
        },
    });
    // Run outcome (docs/67 §3.2). Omitted rather than sent as null when the
    // journal has nothing: the server's fill-once update treats absent and null
    // alike, and an omitted field keeps the payload identical to what older
    // clients send — which is what the server's optional fields are for.
    if let Some(frame_count) = candidate.run_frame_count {
        body["frameCount"] = json!(frame_count);
    }
    if let Some(stop_reason) = non_empty(candidate.run_stop_reason.as_deref()) {
        body["stopReason"] = Value::String(stop_reason.to_owned());
    }
    if let Some(error_code) = non_empty(candidate.run_error_code.as_deref()) {
        body["errorCode"] = Value::String(error_code.to_owned());
    }
    body
}

/// The server rejects blank strings on these fields, and a blank is what an
/// empty journal column looks like after `TEXT NOT NULL DEFAULT ''`-style
/// writes elsewhere in the codebase. Treat it as absent.
fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

/// 逐段 meta.json 里上传时还要读的那一角：`health.input_overflow`。
/// 其余字段按 serde 缺省忽略——这里不是 meta 契约的看门人（ready 门禁才是），
/// 只取要透传的值。
#[derive(Deserialize)]
struct SegmentMetaHealthEnvelope {
    health: SegmentMetaHealth,
}

#[derive(Deserialize)]
struct SegmentMetaHealth {
    input_overflow: u64,
}

/// 引擎逐段 meta 里的 `health.input_overflow` 真值（docs/146 §5）。
///
/// journal 的 segments 表没有这一列（ready 门禁要求它恒零，见 journal.rs 的
/// "unhealthy segment cannot enter ready journal state"），此前上传体里被硬编码
/// 为 0。硬编码的问题不在今天的值——门禁下确实恒零——而在它会把「门禁被绕过
/// 或日后放宽时放出的非零段」伪装成健康段，服务端 `inputOverflow` 硬故障位
/// 永远打不响。真值从已解析的 meta artifact 读：`resolve_artifacts` 刚按 journal
/// 的 meta_sha256 核验过这份文件，读到的就是 ready 时验过的那份引擎事实。
fn meta_input_overflow(artifacts: &[ResolvedArtifact]) -> Result<u64, AttemptError> {
    let meta = artifacts
        .iter()
        .find(|artifact| artifact.kind == "meta")
        .ok_or(AttemptError::Permanent("UPLOAD_JOURNAL_INVALID"))?;
    let raw = std::fs::read(&meta.path)
        .map_err(|_| AttemptError::Permanent("UPLOAD_SOURCE_READ_FAILED"))?;
    let parsed: SegmentMetaHealthEnvelope = serde_json::from_slice(&raw)
        .map_err(|_| AttemptError::Permanent("UPLOAD_SOURCE_CHANGED"))?;
    Ok(parsed.health.input_overflow)
}

fn clip_body(
    candidate: &UploadCandidate,
    artifacts: &[ResolvedArtifact],
    environment: ApiEnvironment,
) -> Result<Value, AttemptError> {
    let declarations = artifacts
        .iter()
        .map(|artifact| {
            (
                artifact.kind.to_string(),
                json!({ "byteSize": artifact.byte_size, "sha256": artifact.sha256 }),
            )
        })
        .collect::<serde_json::Map<_, _>>();
    let mut body = json!({
        "classification": classification(candidate, environment)?,
        "localArtifactState": "ready",
        "sessionFrameStart": candidate.segment_session_frame_start,
        "nFrames": candidate.frame_count,
        "qpcStart": candidate.qpc_start,
        "qpcEnd": candidate.qpc_end,
        "eventCount": candidate.event_count,
        "health": {
            "duplicateFrames": candidate.duplicate_frames,
            "sourceSuperseded": candidate.source_superseded,
            "queueDrops": candidate.source_drops,
            "inputOverflow": meta_input_overflow(artifacts)?,
        },
        "artifacts": declarations,
    });
    if let Some(audio) = &candidate.audio {
        body["audio"] = serde_json::to_value(audio)
            .map_err(|_| AttemptError::Permanent("UPLOAD_JOURNAL_INVALID"))?;
    }
    if let Some(depth) = artifacts.iter().find(|artifact| artifact.kind == "depth") {
        let format = depth_upload_format(&depth.path)?;
        body["depth"] = json!({
            "state": "ready",
            "source": "depth_probe",
            "path": "depth.zip",
            "sha256": depth.sha256,
            "bytes": depth.byte_size,
            "format": format,
        });
    }
    Ok(body)
}

fn depth_upload_format(path: &Path) -> Result<&'static str, AttemptError> {
    #[cfg(feature = "depth-lab")]
    {
        depth_lab::client_archive::read_segment_upload_format(path)
            .map_err(|_| AttemptError::Permanent("UPLOAD_DEPTH_ARCHIVE_INVALID"))
    }
    #[cfg(not(feature = "depth-lab"))]
    {
        let _ = path;
        Err(AttemptError::Permanent("UPLOAD_DEPTH_ARCHIVE_UNSUPPORTED"))
    }
}

/// Which server environment the envelope declares. Parsed once at uploader
/// startup from `DATAMINER_API_ENVIRONMENT`; the server refuses an envelope
/// naming another environment, so this must reflect where the requests go.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ApiEnvironment {
    Production,
    Staging,
}

impl ApiEnvironment {
    fn from_env() -> anyhow::Result<Self> {
        match std::env::var(API_ENVIRONMENT_ENV) {
            Err(_) => Ok(Self::Production),
            Ok(value) if value.eq_ignore_ascii_case("production") => Ok(Self::Production),
            Ok(value) if value.eq_ignore_ascii_case("staging") => Ok(Self::Staging),
            Ok(other) => {
                anyhow::bail!("{API_ENVIRONMENT_ENV} must be production or staging, not {other:?}")
            }
        }
    }
}

/// The classification envelope for one session, derived from the session's own
/// journal facts rather than a compile-time constant (docs/49 E7).
///
/// - 两个家族，由持久化的 session schema 唯一决定：`dm-local-dev`（legacy v1）与
///   `dm-session-ingest`（中性 v2，issue #855 §B）。`dm-certified` 随车道退役
///   （docs/100 §4.3）。
/// - `non_trainable=0` 或未知 schema 都是 journal 损坏，永久拒绝而不是猜一个家族
///   贴上去——今天没有任何写入路径能产出前者（journal 的 `non_trainable = 1` CHECK
///   挡着，服务端迁移 0036 的词表也不收）。
/// - 中性契约**整体不携带**三个训练布尔（§B：不重复携带互相可能打架的字段）。
///   服务端的 `retiredClassificationKeys` 会把带了的请求当场 403，所以这里少写
///   不是省事，是契约。
/// - `isTrainable` 在 legacy 信封里恒 false：可训练性只能是服务端派生的
///   `clip_trainability` 裁决，永远不是客户端的自称。
fn classification(
    candidate: &UploadCandidate,
    environment: ApiEnvironment,
) -> Result<Value, AttemptError> {
    classification_for(
        candidate.non_trainable,
        environment,
        &candidate.session_schema_version,
    )
}

fn classification_for(
    non_trainable: bool,
    environment: ApiEnvironment,
    session_schema_version: &str,
) -> Result<Value, AttemptError> {
    let (environment_name, namespace) = match environment {
        ApiEnvironment::Production => ("production", "raw"),
        ApiEnvironment::Staging => ("staging", "raw-staging"),
    };
    if !non_trainable {
        return Err(AttemptError::Permanent("UPLOAD_JOURNAL_INVALID"));
    }
    let contract = SessionContract::for_session_schema_version(session_schema_version)
        .ok_or(AttemptError::Permanent("UPLOAD_JOURNAL_INVALID"))?;
    // legacy 的 artifact 世代随音频分两代；中性只有一个值（音频改由 clip 的
    // artifact 事实回答）。`session_schema_declares_audio` 在中性一代返回 `None`，
    // 那一代的 `recording_schema_version` 也就与入参无关。
    let audio =
        SessionContract::session_schema_declares_audio(session_schema_version).unwrap_or(false);
    let mut classification = json!({
        "sourceFamily": contract.source_family(),
        "contractVersion": "dm-1",
        "artifactSchemaVersion": contract.recording_schema_version(audio),
        "environment": environment_name,
        "storageNamespace": namespace,
    });
    if contract.declares_training_classification() {
        classification["nonTrainable"] = json!(true);
        classification["isTrainable"] = json!(false);
        classification["productionCertified"] = json!(false);
    }
    Ok(classification)
}

const fn session_artifact_content_type(kind: SessionArtifactKind) -> &'static str {
    if matches!(kind, SessionArtifactKind::Markers) {
        "application/x-ndjson"
    } else {
        "application/json"
    }
}

fn snapshot(id: &str, version: &str, content_hash: &str, mode: Option<&str>) -> Value {
    let mut value = json!({ "id": id, "version": version, "contentHash": content_hash });
    if let Some(mode) = mode {
        value["mode"] = Value::String(mode.into());
    }
    value
}

fn completion_body(
    parts: &[UploadedPart],
    sources: &[UploadArtifactSource],
) -> Result<Value, AttemptError> {
    let mut artifacts = serde_json::Map::new();
    for source in sources {
        let kind = source.kind;
        let selected = parts
            .iter()
            .filter(|part| part.artifact_kind == kind)
            .map(|part| json!({ "partNumber": part.part_number, "etag": part.etag }))
            .collect::<Vec<_>>();
        if selected.is_empty()
            || selected
                .iter()
                .enumerate()
                .any(|(index, value)| value["partNumber"].as_u64() != Some(index as u64 + 1))
        {
            return Err(AttemptError::Permanent("UPLOAD_PARTS_INCOMPLETE"));
        }
        artifacts.insert(kind.into(), json!({ "parts": selected }));
    }
    if artifacts.len() != sources.len() {
        return Err(AttemptError::Permanent("UPLOAD_PARTS_INCOMPLETE"));
    }
    Ok(json!({ "artifacts": artifacts }))
}

fn ensure_remote_run(response: &ClipResponse, expected_run_id: &str) -> Result<(), AttemptError> {
    (response.run_id == expected_run_id)
        .then_some(())
        .ok_or(AttemptError::Permanent("UPLOAD_REMOTE_IDENTITY_CONFLICT"))
}

fn sha256_file(path: &Path) -> anyhow::Result<(u64, String)> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 1024 * 1024];
    let mut bytes = 0_u64;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        bytes = bytes
            .checked_add(read as u64)
            .context("artifact size overflow")?;
    }
    ensure!(bytes > 0, "empty artifacts are not uploadable");
    Ok((bytes, format!("sha256:{:x}", hasher.finalize())))
}

fn part_md5(path: &Path, offset: u64, size: u64) -> anyhow::Result<String> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut take = file.take(size);
    let mut hasher = Md5::new();
    let copied = std::io::copy(&mut take, &mut HashWriter(&mut hasher))?;
    ensure!(
        copied == size,
        "artifact ended before the planned part boundary"
    );
    Ok(BASE64_STANDARD.encode(hasher.finalize()))
}

struct HashWriter<'a, D: Digest>(&'a mut D);

impl<D: Digest> std::io::Write for HashWriter<'_, D> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.0.update(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn idempotency_key(scope: &str, identity: &str) -> String {
    let digest = Sha256::digest(format!("dataminer-upload-v1\0{scope}\0{identity}").as_bytes());
    format!("dm1-{scope}-{:x}", digest)[..(scope.len() + 37)].to_string()
}

/// Exponential backoff with stable jitter (docs/30 §5.5). The jitter is derived
/// from the segment identity and the attempt number, not from a clock or RNG:
/// concurrent capture rigs still de-synchronize, a single rig replays the same
/// schedule (testable), and the total stays within [5 s, 300 s].
fn retry_delay_ms(attempt_count: u32, segment_id: &str) -> u64 {
    let exponent = attempt_count.saturating_sub(1).min(6);
    let base = 5_000_u64.saturating_mul(1_u64 << exponent).min(300_000);
    let digest =
        Sha256::digest(format!("dm-retry-jitter-1\0{segment_id}\0{attempt_count}").as_bytes());
    let seed = u64::from_be_bytes(digest[..8].try_into().expect("digest is 32 bytes"));
    // Up to +50% of the base delay, capped at the 300 s ceiling.
    let jitter = seed % (base / 2).max(1);
    base.saturating_add(jitter).clamp(5_000, 300_000)
}

struct HttpTransport {
    client: Client,
    /// Body PUTs to R2 get their own client: a large part on a slow link is not
    /// a stuck request, and must not inherit the 60 s control-plane budget.
    put_client: Client,
    base_url: reqwest::Url,
    environment: ApiEnvironment,
}

#[derive(Debug)]
struct TransportError {
    code: &'static str,
    retryable: bool,
    http_status: Option<u16>,
    server_code: Option<String>,
    server_message: Option<String>,
    request_id: Option<String>,
}

impl TransportError {
    fn local(code: &'static str, retryable: bool) -> Self {
        Self {
            code,
            retryable,
            http_status: None,
            server_code: None,
            server_message: None,
            request_id: None,
        }
    }
}

impl HttpTransport {
    fn production() -> anyhow::Result<Self> {
        let configured = std::env::var(API_BASE_URL_ENV)
            .unwrap_or_else(|_| DEFAULT_PRODUCTION_API_BASE_URL.into());
        let mut base_url =
            reqwest::Url::parse(&configured).context("invalid production API base URL")?;
        ensure!(
            base_url.scheme() == "https"
                || (cfg!(debug_assertions)
                    && base_url.scheme() == "http"
                    && matches!(base_url.host_str(), Some("127.0.0.1" | "localhost"))),
            "production API base URL must use HTTPS"
        );
        ensure!(
            base_url.query().is_none() && base_url.fragment().is_none(),
            "API base URL cannot contain query or fragment"
        );
        if !base_url.path().ends_with('/') {
            let normalized_path = format!("{}/", base_url.path());
            base_url.set_path(&normalized_path);
        }
        let client = Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .user_agent(concat!("DataMiner/", env!("CARGO_PKG_VERSION")))
            .build()?;
        let put_client = Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(PUT_TIMEOUT)
            .user_agent(concat!("DataMiner/", env!("CARGO_PKG_VERSION")))
            .build()?;
        let environment = ApiEnvironment::from_env()?;
        Ok(Self {
            client,
            put_client,
            base_url,
            environment,
        })
    }

    fn json<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        credential: &DeviceCredential,
        idempotency_key: Option<&str>,
        body: &Value,
    ) -> Result<T, TransportError> {
        let url = self
            .base_url
            .join(path)
            .map_err(|_| TransportError::local("UPLOAD_REQUEST_INVALID", false))?;
        let bearer = HeaderValue::from_str(&format!("Bearer {}", credential.expose()))
            .map_err(|_| TransportError::local("UPLOAD_CREDENTIAL_INVALID", true))?;
        let mut request = self
            .client
            .request(method, url)
            .header(AUTHORIZATION, bearer)
            .json(body);
        if let Some(key) = idempotency_key {
            request = request.header("Idempotency-Key", key);
        }
        let response = request
            .send()
            .map_err(|_| TransportError::local("UPLOAD_API_UNAVAILABLE", true))?;
        decode_json(response)
    }

    fn post_empty<T: DeserializeOwned>(
        &self,
        path: &str,
        credential: &DeviceCredential,
    ) -> Result<T, TransportError> {
        let url = self
            .base_url
            .join(path)
            .map_err(|_| TransportError::local("UPLOAD_REQUEST_INVALID", false))?;
        let bearer = HeaderValue::from_str(&format!("Bearer {}", credential.expose()))
            .map_err(|_| TransportError::local("UPLOAD_CREDENTIAL_INVALID", true))?;
        let response = self
            .client
            .post(url)
            .header(AUTHORIZATION, bearer)
            .send()
            .map_err(|_| TransportError::local("UPLOAD_API_UNAVAILABLE", true))?;
        decode_json(response)
    }

    fn put_session_artifact(
        &self,
        signed: &SessionArtifactSignResponse,
        artifact: &ResolvedSessionArtifact,
    ) -> Result<(), AttemptError> {
        let signed_url = parse_signed_upload_url(&signed.url)?;
        let expected_headers = session_artifact_put_headers(
            artifact.kind,
            artifact.byte_size,
            &artifact.content_md5,
            self.environment,
        );
        if signed.headers != expected_headers {
            return Err(AttemptError::Permanent("UPLOAD_PLAN_INVALID"));
        }
        let mut headers = HeaderMap::new();
        for (name, value) in &signed.headers {
            let name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| AttemptError::Permanent("UPLOAD_PLAN_INVALID"))?;
            let value = HeaderValue::from_str(value)
                .map_err(|_| AttemptError::Permanent("UPLOAD_PLAN_INVALID"))?;
            headers.insert(name, value);
        }
        let file = File::open(&artifact.path)
            .map_err(|_| AttemptError::Permanent("UPLOAD_SOURCE_READ_FAILED"))?;
        let response = self
            .put_client
            .put(signed_url)
            .headers(headers)
            .body(Body::sized(file, artifact.byte_size))
            .send()
            .map_err(|_| AttemptError::Retryable("UPLOAD_R2_UNAVAILABLE"))?;
        if !response.status().is_success() {
            return Err(AttemptError::Retryable("UPLOAD_R2_REJECTED"));
        }
        Ok(())
    }

    fn get_status(
        &self,
        remote_clip_id: &str,
        credential: &DeviceCredential,
    ) -> Result<ClipStatusResponse, TransportError> {
        let url = self
            .base_url
            .join(&format!("v1/clips/{remote_clip_id}/status"))
            .map_err(|_| TransportError::local("UPLOAD_REQUEST_INVALID", false))?;
        let bearer = HeaderValue::from_str(&format!("Bearer {}", credential.expose()))
            .map_err(|_| TransportError::local("UPLOAD_CREDENTIAL_INVALID", true))?;
        let response = self
            .client
            .get(url)
            .header(AUTHORIZATION, bearer)
            .send()
            .map_err(|_| TransportError::local("UPLOAD_API_UNAVAILABLE", true))?;
        decode_json(response)
    }

    fn put_part(
        &self,
        signed: &SignedPart,
        path: &Path,
        offset: u64,
        size: u64,
        content_md5: &str,
    ) -> Result<String, AttemptError> {
        let mut file =
            File::open(path).map_err(|_| AttemptError::Permanent("UPLOAD_SOURCE_READ_FAILED"))?;
        file.seek(SeekFrom::Start(offset))
            .map_err(|_| AttemptError::Permanent("UPLOAD_SOURCE_READ_FAILED"))?;
        let signed_url = parse_signed_upload_url(&signed.url)?;
        let size_text = size.to_string();
        if signed.headers.len() != 2
            || signed.headers.get("content-md5").map(String::as_str) != Some(content_md5)
            || signed.headers.get("content-length").map(String::as_str) != Some(size_text.as_str())
        {
            return Err(AttemptError::Permanent("UPLOAD_PLAN_INVALID"));
        }
        let mut headers = HeaderMap::new();
        for (name, value) in &signed.headers {
            let name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| AttemptError::Permanent("UPLOAD_PLAN_INVALID"))?;
            let value = HeaderValue::from_str(value)
                .map_err(|_| AttemptError::Permanent("UPLOAD_PLAN_INVALID"))?;
            headers.insert(name, value);
        }
        headers.insert(
            CONTENT_LENGTH,
            HeaderValue::from_str(&size_text)
                .map_err(|_| AttemptError::Permanent("UPLOAD_PLAN_INVALID"))?,
        );
        let response = self
            .put_client
            .put(signed_url)
            .headers(headers)
            .body(Body::sized(file.take(size), size))
            .send()
            .map_err(|_| AttemptError::Retryable("UPLOAD_R2_UNAVAILABLE"))?;
        if !response.status().is_success() {
            return Err(AttemptError::Retryable("UPLOAD_R2_REJECTED"));
        }
        response
            .headers()
            .get(ETAG)
            .and_then(|value| value.to_str().ok())
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .ok_or(AttemptError::Retryable("UPLOAD_R2_ETAG_MISSING"))
    }
}

fn parse_signed_upload_url(raw: &str) -> Result<reqwest::Url, AttemptError> {
    let url = reqwest::Url::parse(raw)
        .ok()
        .filter(|url| url.host_str().is_some())
        .ok_or(AttemptError::Permanent("UPLOAD_PLAN_INVALID"))?;
    if signed_upload_url_allowed(&url, insecure_loopback_r2_enabled()) {
        return Ok(url);
    }

    Err(AttemptError::Permanent("UPLOAD_PLAN_INVALID"))
}

fn insecure_loopback_r2_enabled() -> bool {
    #[cfg(feature = "dev-catalog")]
    {
        cfg!(debug_assertions)
            && std::env::var(ALLOW_INSECURE_LOOPBACK_R2_ENV).as_deref() == Ok("1")
    }

    #[cfg(not(feature = "dev-catalog"))]
    {
        false
    }
}

fn upload_lab_control_plane_enabled(transport: &HttpTransport) -> bool {
    upload_lab_control_plane_allowed(&transport.base_url, insecure_loopback_r2_enabled())
}

fn upload_lab_control_plane_allowed(url: &reqwest::Url, allow_insecure_loopback: bool) -> bool {
    allow_insecure_loopback
        && url.scheme() == "http"
        && matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "::1"))
}

fn signed_upload_url_allowed(url: &reqwest::Url, allow_insecure_loopback: bool) -> bool {
    url.scheme() == "https"
        || (allow_insecure_loopback
            && url.scheme() == "http"
            && matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "::1")))
}

fn session_artifact_put_headers(
    kind: SessionArtifactKind,
    byte_size: u64,
    content_md5: &str,
    environment: ApiEnvironment,
) -> HashMap<String, String> {
    HashMap::from([
        (
            "content-type".to_owned(),
            session_artifact_content_type(kind).to_owned(),
        ),
        ("content-md5".to_owned(), content_md5.to_owned()),
        ("content-length".to_owned(), byte_size.to_string()),
        (
            "x-amz-meta-dataminer_class".to_owned(),
            match environment {
                ApiEnvironment::Production => "raw-non-trainable",
                ApiEnvironment::Staging => "raw-staging-non-trainable",
            }
            .to_owned(),
        ),
    ])
}

fn decode_json<T: DeserializeOwned>(response: Response) -> Result<T, TransportError> {
    let status = response.status();
    if !status.is_success() {
        let server_error = response.json::<ApiErrorResponse>().ok();
        let server_requires_retry = server_error.as_ref().is_some_and(|error| {
            matches!(
                error.code.as_str(),
                "multipart_completion_in_progress" | "multipart_upload_restarted"
            )
        });
        let retryable = server_error.as_ref().is_some_and(|error| error.retryable)
            || server_requires_retry
            || status.is_server_error()
            || matches!(status.as_u16(), 401 | 408 | 429);
        let (server_code, server_message, request_id) = server_error
            .map(|error| {
                (
                    non_empty_bounded(error.code, 128),
                    non_empty_bounded(error.message, 512),
                    non_empty_bounded(error.request_id, 128),
                )
            })
            .unwrap_or_default();
        return Err(TransportError {
            code: if retryable {
                "UPLOAD_API_RETRYABLE"
            } else {
                "UPLOAD_REQUEST_REJECTED"
            },
            retryable,
            http_status: Some(status.as_u16()),
            server_code,
            server_message,
            request_id,
        });
    }
    response.json().map_err(|_| TransportError {
        http_status: Some(status.as_u16()),
        ..TransportError::local("UPLOAD_RESPONSE_INVALID", true)
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApiErrorResponse {
    #[serde(default)]
    code: String,
    #[serde(default)]
    message: String,
    #[serde(default)]
    request_id: String,
    retryable: bool,
}

fn non_empty_bounded(value: String, max_chars: usize) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.chars().take(max_chars).collect())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionArtifactsResponse {
    session_id: String,
    artifacts: Vec<SessionArtifactDeclarationStatus>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionArtifactDeclarationStatus {
    kind: SessionArtifactKind,
    object_key: String,
    verification_state: String,
    expected_bytes: u64,
    expected_sha256: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionArtifactSignResponse {
    kind: SessionArtifactKind,
    object_key: String,
    url: String,
    headers: HashMap<String, String>,
    #[allow(dead_code)]
    expires_in_seconds: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionArtifactStatusResponse {
    kind: SessionArtifactKind,
    verification_state: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionResponse {
    session_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RunResponse {
    run_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClipResponse {
    clip_id: String,
    run_id: String,
    revision: u32,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UploadIntentResponse {
    upload_intent_id: String,
    artifacts: HashMap<String, ArtifactPlan>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ArtifactPlan {
    part_size_bytes: u64,
    part_count: u32,
    #[serde(default = "default_provider_attempt_number")]
    provider_attempt_number: u32,
}

fn default_provider_attempt_number() -> u32 {
    1
}

#[derive(Deserialize)]
struct SignPartsResponse {
    parts: Vec<SignedPart>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SignedPart {
    artifact_type: String,
    part_number: u32,
    url: String,
    headers: HashMap<String, String>,
}

#[derive(Deserialize)]
struct StoredResponse {
    #[allow(dead_code)]
    state: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClipStatusResponse {
    verification_state: String,
    retention_state: String,
    rejection_code: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpListener;

    fn api_response(status: u16, body: &str) -> Response {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let body = body.to_owned();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 2048];
            let _ = stream.read(&mut request);
            write!(
                stream,
                "HTTP/1.1 {status} test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        });
        let response = Client::new()
            .get(format!("http://{address}/test"))
            .send()
            .unwrap();
        server.join().unwrap();
        response
    }

    #[test]
    fn api_error_diagnostics_keep_safe_server_correlation() {
        let error = decode_json::<Value>(api_response(
            400,
            r#"{"code":"invalid_request","message":"endedAt violates the session contract","requestId":"request-2","retryable":false}"#,
        ))
        .unwrap_err();
        assert_eq!(error.code, "UPLOAD_REQUEST_REJECTED");
        assert!(!error.retryable);
        assert_eq!(error.http_status, Some(400));
        assert_eq!(error.server_code.as_deref(), Some("invalid_request"));
        assert_eq!(
            error.server_message.as_deref(),
            Some("endedAt violates the session contract")
        );
        assert_eq!(error.request_id.as_deref(), Some("request-2"));
    }

    #[test]
    fn legacy_non_retryable_completion_lease_conflict_is_retried_defensively() {
        let error = decode_json::<Value>(api_response(
            409,
            r#"{"code":"multipart_completion_in_progress","message":"completion is already owned","requestId":"request-complete","retryable":false}"#,
        ))
        .unwrap_err();

        assert_eq!(error.code, "UPLOAD_API_RETRYABLE");
        assert!(error.retryable);
        assert_eq!(
            error.server_code.as_deref(),
            Some("multipart_completion_in_progress")
        );
    }

    #[test]
    fn transport_error_keeps_server_details_through_attempt_classification() {
        let attempt = AttemptError::from(
            decode_json::<Value>(api_response(
                409,
                r#"{"code":"device_profile_mismatch","message":"device profile does not match the registered device","requestId":"request-profile","retryable":false}"#,
            ))
            .unwrap_err(),
        );

        assert_eq!(attempt.code(), "UPLOAD_REQUEST_REJECTED");
        assert!(!attempt.retryable());
        assert_eq!(attempt.http_status(), Some(409));
        assert_eq!(attempt.server_code(), Some("device_profile_mismatch"));
        assert_eq!(
            attempt.server_message(),
            Some("device profile does not match the registered device")
        );
        assert_eq!(attempt.request_id(), Some("request-profile"));
    }

    #[test]
    fn insecure_presigned_urls_are_limited_to_the_explicit_loopback_lab() {
        let https = reqwest::Url::parse("https://example.invalid/upload").unwrap();
        let loopback = reqwest::Url::parse("http://127.0.0.1:43199/upload").unwrap();
        let localhost = reqwest::Url::parse("http://localhost:43199/upload").unwrap();
        let public_http = reqwest::Url::parse("http://example.invalid/upload").unwrap();
        let wildcard = reqwest::Url::parse("http://0.0.0.0:43199/upload").unwrap();

        assert!(signed_upload_url_allowed(&https, false));
        assert!(!signed_upload_url_allowed(&loopback, false));
        assert!(signed_upload_url_allowed(&loopback, true));
        assert!(signed_upload_url_allowed(&localhost, true));
        assert!(!signed_upload_url_allowed(&public_http, true));
        assert!(!signed_upload_url_allowed(&wildcard, true));
        assert!(!upload_lab_control_plane_allowed(&https, true));
        assert!(!upload_lab_control_plane_allowed(&loopback, false));
        assert!(upload_lab_control_plane_allowed(&loopback, true));
        assert!(upload_lab_control_plane_allowed(&localhost, true));
        assert!(!upload_lab_control_plane_allowed(&public_http, true));
        assert!(!upload_lab_control_plane_allowed(&wildcard, true));
    }

    #[test]
    fn segment_artifacts_resolve_from_their_session_root() {
        let temp = tempfile::tempdir().unwrap();
        let relative_path = "runs/r0000/segments/00000/video.mp4";
        let artifact_path = temp.path().join("session-1").join(relative_path);
        std::fs::create_dir_all(artifact_path.parent().unwrap()).unwrap();
        std::fs::write(&artifact_path, b"video-bytes").unwrap();
        let expected_sha256 = sha256_file(&artifact_path).unwrap().1;
        let sources = [UploadArtifactSource {
            kind: "video",
            relative_path: relative_path.into(),
            expected_sha256: Some(expected_sha256.clone()),
        }];

        let resolved = resolve_artifacts(temp.path(), "session-1", &sources).unwrap();

        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].kind, "video");
        assert_eq!(resolved[0].path, artifact_path.canonicalize().unwrap());
        assert_eq!(resolved[0].byte_size, 11);
        assert_eq!(resolved[0].sha256, expected_sha256);
    }

    #[test]
    fn segment_artifacts_reject_unsafe_session_and_relative_paths() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join("session-1")).unwrap();
        let safe_source = [UploadArtifactSource {
            kind: "video",
            relative_path: "video.mp4".into(),
            expected_sha256: None,
        }];
        assert!(matches!(
            resolve_artifacts(temp.path(), "../session-1", &safe_source),
            Err(AttemptError::Permanent("UPLOAD_SOURCE_PATH_INVALID"))
        ));

        let unsafe_source = [UploadArtifactSource {
            kind: "video",
            relative_path: "../video.mp4".into(),
            expected_sha256: None,
        }];
        assert!(matches!(
            resolve_artifacts(temp.path(), "session-1", &unsafe_source),
            Err(AttemptError::Permanent("UPLOAD_SOURCE_PATH_INVALID"))
        ));
    }

    #[test]
    fn idempotency_keys_are_stable_scoped_and_short() {
        let first = idempotency_key("clip", "session-1:r0000:0");
        assert_eq!(first, idempotency_key("clip", "session-1:r0000:0"));
        assert_ne!(first, idempotency_key("run", "session-1:r0000:0"));
        assert!(first.len() < 80);
        assert!(first.is_ascii());
    }

    #[test]
    fn retry_backoff_is_bounded_and_deterministically_jittered() {
        // Stable per (segment, attempt): the same pair replays the same delay.
        assert_eq!(
            retry_delay_ms(1, "segment-a"),
            retry_delay_ms(1, "segment-a")
        );
        // Jitter separates rigs retrying in lockstep.
        assert_ne!(
            retry_delay_ms(3, "segment-a"),
            retry_delay_ms(3, "segment-b")
        );
        // Bounds: base 5 s..300 s, jitter up to +50%, ceiling 300 s, floor 5 s.
        for attempt in 1..=10 {
            for segment in ["segment-a", "segment-b", "segment-c"] {
                let delay = retry_delay_ms(attempt, segment);
                assert!(
                    (5_000..=300_000).contains(&delay),
                    "attempt {attempt}: {delay}"
                );
                let exponent = attempt.saturating_sub(1).min(6);
                let base = 5_000_u64.saturating_mul(1 << exponent).min(300_000);
                assert!(
                    delay >= base.min(300_000),
                    "delay below base: {delay} < {base}"
                );
            }
        }
    }

    fn planned_artifact(kind: &'static str, byte_size: u64) -> ResolvedArtifact {
        ResolvedArtifact {
            kind,
            path: PathBuf::from(kind),
            byte_size,
            sha256: "sha256:aa".into(),
        }
    }

    /// `clip_body` 会真读 meta artifact 的文件取 `health.input_overflow`，
    /// 所以这个 stub 必须落盘，不能像 [`planned_artifact`] 那样只造路径。
    fn meta_artifact(dir: &tempfile::TempDir, input_overflow: u64) -> ResolvedArtifact {
        let path = dir.path().join("meta.json");
        std::fs::write(
            &path,
            format!(r#"{{"schema_version":"dm-local-dev-recording-1","health":{{"duplicate_frames":0,"source_superseded":0,"queue_drops":0,"input_overflow":{input_overflow}}}}}"#),
        )
        .unwrap();
        ResolvedArtifact {
            kind: "meta",
            path,
            byte_size: 0,
            sha256: format!("sha256:{}", "c".repeat(64)),
        }
    }

    fn planned_intent(plans: &[(&str, u64, u32)]) -> UploadIntentResponse {
        UploadIntentResponse {
            upload_intent_id: "intent-1".into(),
            artifacts: plans
                .iter()
                .map(|(kind, part_size_bytes, part_count)| {
                    (
                        (*kind).to_owned(),
                        ArtifactPlan {
                            part_size_bytes: *part_size_bytes,
                            part_count: *part_count,
                            provider_attempt_number: 1,
                        },
                    )
                })
                .collect(),
        }
    }

    /// 摊平后的队列必须逐字节还原串行版本走过的分片边界：同 artifact 内 part
    /// 递增、offset 连续、末片是余数。`completion_body` 只接受按 partNumber
    /// 升序且从 1 开始的列表，队列本身乱一位就再也拼不回一个完整对象。
    #[test]
    fn planned_parts_cover_each_artifact_contiguously_to_its_remainder() {
        let artifacts = [planned_artifact("video", 25), planned_artifact("meta", 4)];
        let intent = planned_intent(&[("video", 10, 3), ("meta", 10, 1)]);
        let planned = plan_parts(&artifacts, &intent).unwrap();
        let boundaries = planned
            .iter()
            .map(|part| (part.artifact_kind, part.part_number, part.offset, part.size))
            .collect::<Vec<_>>();
        assert_eq!(
            boundaries,
            vec![
                ("video", 1, 0, 10),
                ("video", 2, 10, 10),
                ("video", 3, 20, 5),
                ("meta", 1, 0, 4),
            ]
        );
        assert!(
            planned
                .iter()
                .all(|part| part.path == Path::new(part.artifact_kind))
        );
    }

    #[test]
    fn provider_attempt_change_reuploads_only_parts_from_the_replaced_artifact() {
        let saved = UploadedPart {
            artifact_kind: "events".into(),
            provider_attempt_no: 1,
            part_number: 1,
            byte_offset: 0,
            byte_size: 7,
            content_md5: "same-md5".into(),
            etag: "stale-etag".into(),
        };

        assert!(reusable_uploaded_part(&saved, 1, 0, 7, "same-md5").unwrap());
        assert!(!reusable_uploaded_part(&saved, 2, 0, 7, "same-md5").unwrap());
        let stale_plan = reusable_uploaded_part(
            &UploadedPart {
                provider_attempt_no: 2,
                ..saved.clone()
            },
            1,
            0,
            7,
            "same-md5",
        )
        .unwrap_err();
        assert!(stale_plan.retryable());
        assert_eq!(stale_plan.code(), "UPLOAD_PLAN_STALE");
    }

    /// 服务端计划与本地字节数对不上时必须在派活**之前**就判死。并发放大了
    /// 这条的代价：晚一步发现就是几条连接白跑。
    #[test]
    fn a_plan_that_disagrees_with_the_local_byte_size_never_reaches_the_queue() {
        let artifacts = [planned_artifact("video", 25)];
        assert_eq!(
            plan_parts(&artifacts, &planned_intent(&[("video", 10, 2)]))
                .unwrap_err()
                .code(),
            "UPLOAD_PLAN_INVALID"
        );
        assert_eq!(
            plan_parts(&artifacts, &planned_intent(&[("video", 0, 3)]))
                .unwrap_err()
                .code(),
            "UPLOAD_PLAN_INVALID"
        );
        assert_eq!(
            plan_parts(&artifacts, &planned_intent(&[("frames", 10, 3)]))
                .unwrap_err()
                .code(),
            "UPLOAD_PLAN_INVALID"
        );
    }

    fn concurrency_probe_parts(count: usize) -> Vec<PlannedPart<'static>> {
        (1..=count)
            .map(|part_number| PlannedPart {
                artifact_kind: "video",
                path: Path::new("video"),
                provider_attempt_no: 1,
                part_number: part_number as u32,
                part_count: count as u32,
                offset: 0,
                size: 1,
            })
            .collect()
    }

    /// 这条钉的就是本次改动的全部意义：队列真的会同时在飞多个 part。
    ///
    /// 每个 part 进来先自增在飞计数，再等到峰值够到并发度才放行——够到之后所有
    /// 后续 part 直接通过，所以正常路径是毫秒级。串行实现下峰值永远是 1，等待
    /// 会走满这个共享的 2 秒上限然后断言失败：是失败，不是挂死，因此这里用
    /// 带上限的自旋而不是 `Barrier`。
    #[test]
    fn the_queue_keeps_the_configured_number_of_parts_in_flight() {
        const CONCURRENCY: usize = 4;
        let planned = concurrency_probe_parts(12);
        let in_flight = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let completed = AtomicUsize::new(0);
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        upload_parts_concurrently(&planned, CONCURRENCY, |_| {
            let current = in_flight.fetch_add(1, Ordering::AcqRel) + 1;
            peak.fetch_max(current, Ordering::AcqRel);
            while peak.load(Ordering::Acquire) < CONCURRENCY && std::time::Instant::now() < deadline
            {
                std::thread::sleep(Duration::from_millis(1));
            }
            in_flight.fetch_sub(1, Ordering::AcqRel);
            completed.fetch_add(1, Ordering::AcqRel);
            Ok(())
        })
        .unwrap();
        assert_eq!(peak.load(Ordering::Acquire), CONCURRENCY);
        assert_eq!(completed.load(Ordering::Acquire), planned.len());
    }

    /// 并发度 1 必须逐字节退回串行时代：按列表顺序走，失败即停，后面的 part
    /// 一个都不碰。这是弱网现场把 `DATAMINER_UPLOAD_PART_CONCURRENCY=1`
    /// 设回去时买到的东西。
    #[test]
    fn a_single_worker_walks_the_queue_in_order_and_stops_at_the_first_failure() {
        let planned = concurrency_probe_parts(5);
        let seen = Mutex::new(Vec::new());
        let error = upload_parts_concurrently(&planned, 1, |part| {
            seen.lock().push(part.part_number);
            if part.part_number == 3 {
                return Err(AttemptError::Retryable("UPLOAD_R2_UNAVAILABLE"));
            }
            Ok(())
        })
        .unwrap_err();
        assert_eq!(error.code(), "UPLOAD_R2_UNAVAILABLE");
        assert_eq!(seen.into_inner(), vec![1, 2, 3]);
    }

    /// 同一批并发 part 可以同时以不同方式失败，留下的必须是终局的那个。
    /// 这里直接钉合并规则本身——哪个 part 先失败取决于线程调度，把断言压在
    /// 调度顺序上只会换来一个偶发红灯。
    #[test]
    fn a_permanent_failure_outranks_any_retryable_one_in_the_same_batch() {
        let mut slot = None;
        keep_worse_failure(&mut slot, AttemptError::Retryable("UPLOAD_R2_UNAVAILABLE"));
        assert_eq!(slot.as_ref().unwrap().code(), "UPLOAD_R2_UNAVAILABLE");

        keep_worse_failure(&mut slot, AttemptError::Permanent("UPLOAD_PLAN_CHANGED"));
        assert_eq!(slot.as_ref().unwrap().code(), "UPLOAD_PLAN_CHANGED");

        // 终局判决不会被后到的弱网错误顶掉。
        keep_worse_failure(&mut slot, AttemptError::Retryable("UPLOAD_R2_UNAVAILABLE"));
        assert_eq!(slot.as_ref().unwrap().code(), "UPLOAD_PLAN_CHANGED");

        // 同严重度先到先得。
        keep_worse_failure(&mut slot, AttemptError::Permanent("UPLOAD_PLAN_INVALID"));
        assert_eq!(slot.unwrap().code(), "UPLOAD_PLAN_CHANGED");
    }

    /// 一个 part 失败之后，其余 worker 不再认领新活。已经在飞的会跑完——它们
    /// 写进 journal 的 ETag 是下一次 attempt 的净进度——但队列不会被抽干。
    #[test]
    fn a_failing_part_stops_the_queue_from_claiming_more_work() {
        let planned = concurrency_probe_parts(64);
        let attempted = AtomicUsize::new(0);
        let error = upload_parts_concurrently(&planned, 4, |_| {
            // 第一个进来的直接判死，其余的都慢——失败结论一定先于任何一个
            // worker 回头认领第二件活。
            if attempted.fetch_add(1, Ordering::AcqRel) == 0 {
                return Err(AttemptError::Permanent("UPLOAD_PLAN_CHANGED"));
            }
            std::thread::sleep(Duration::from_millis(50));
            Ok(())
        })
        .unwrap_err();
        assert_eq!(error.code(), "UPLOAD_PLAN_CHANGED");
        assert!(
            attempted.load(Ordering::Acquire) < planned.len(),
            "a failed attempt drained the whole queue anyway"
        );
    }

    /// 空队列不该起线程，全成功不该造出错误。
    #[test]
    fn an_empty_queue_is_a_no_op() {
        assert!(upload_parts_concurrently(&[], 4, |_| unreachable!()).is_ok());
    }

    /// 并发度是性能旋钮，不是正确性开关：打错的覆盖值退回默认，越界的钳到
    /// 上限，写 1 就真的是串行。
    #[test]
    fn the_concurrency_override_is_clamped_and_never_fails_closed() {
        assert_eq!(part_concurrency_from(None), DEFAULT_PART_CONCURRENCY);
        assert_eq!(part_concurrency_from(Some(" 2 ")), 2);
        assert_eq!(part_concurrency_from(Some("1")), 1);
        assert_eq!(part_concurrency_from(Some("999")), MAX_PART_CONCURRENCY);
        for bad in ["", "0", "-1", "four", "2.5"] {
            assert_eq!(
                part_concurrency_from(Some(bad)),
                DEFAULT_PART_CONCURRENCY,
                "{bad:?} should fall back to the default"
            );
        }
    }

    /// 反复失败的 segment 必须收敛回串行。约 0.11–0.45 Mbit/s 的链路上 C=4 是
    /// 四片一起撞 `PUT_TIMEOUT`、零净进度，而 segment 重试没有次数上限——不退让
    /// 就是一个不报错也不前进的活锁。
    #[test]
    fn concurrency_backs_off_to_serial_as_one_segment_keeps_failing() {
        assert_eq!(part_concurrency_for_attempt(4, 1), 4);
        assert_eq!(part_concurrency_for_attempt(4, 2), 2);
        assert_eq!(part_concurrency_for_attempt(4, 3), 1);
        assert_eq!(part_concurrency_for_attempt(4, 4), 1);
        // 上限并发也在两轮内到底，不会因为起点高就多熬几轮。
        assert_eq!(part_concurrency_for_attempt(MAX_PART_CONCURRENCY, 3), 2);
        // 卡了很久的 segment 不能因为移位溢出而弹回满并发。
        assert_eq!(
            part_concurrency_for_attempt(MAX_PART_CONCURRENCY, u32::MAX),
            1
        );
        // 已经是串行的配置不会被退让改成 0。
        assert_eq!(part_concurrency_for_attempt(1, 9), 1);
    }

    #[test]
    fn completion_requires_every_declared_contiguous_part_list() {
        let mut parts = Vec::new();
        let sources = ["video", "frames", "events", "meta", "manifest", "depth"]
            .into_iter()
            .map(|kind| UploadArtifactSource {
                kind,
                relative_path: kind.into(),
                expected_sha256: None,
            })
            .collect::<Vec<_>>();
        for source in &sources {
            parts.push(UploadedPart {
                artifact_kind: source.kind.into(),
                provider_attempt_no: 1,
                part_number: 1,
                byte_offset: 0,
                byte_size: 1,
                content_md5: "x".into(),
                etag: "etag".into(),
            });
        }
        let complete = completion_body(&parts, &sources).unwrap();
        assert_eq!(complete["artifacts"]["depth"]["parts"][0]["partNumber"], 1);
        parts.pop();
        assert!(completion_body(&parts, &sources).is_err());
    }

    fn candidate_stub(non_trainable: bool, consent_id: Option<&str>) -> UploadCandidate {
        UploadCandidate {
            segment_id: "seg-1".into(),
            local_session_id: "dev-1-0000".into(),
            session_registration_state: "completed".into(),
            session_state: "completed".into(),
            session_schema_version: "dm-local-dev-session-2".into(),
            session_started_at_utc: "2026-07-26T00:00:00.000Z".into(),
            session_ended_at_utc: Some("2026-07-26T00:01:00.000Z".into()),
            session_stop_reason: None,
            non_trainable,
            consent_id: consent_id.map(str::to_owned),
            session_contract: "dm-local-dev".into(),
            catalog_id: "catalog".into(),
            catalog_version: "1".into(),
            catalog_hash: "sha256:aa".into(),
            device_profile_id: "device".into(),
            device_profile_version: "1".into(),
            device_profile_hash: "sha256:bb".into(),
            recording_profile_id: "capture".into(),
            recording_profile_version: "1".into(),
            recording_profile_hash: "sha256:cc".into(),
            game_id: "game".into(),
            game_profile_version: "1".into(),
            game_profile_hash: "sha256:dd".into(),
            game_executable_sha256: Some("sha256:ee".into()),
            game_executable_sha256_missing_reason: None,
            mapping_id: "mapping".into(),
            mapping_version: "1".into(),
            mapping_hash: "sha256:11".into(),
            remote_session_id: None,
            run_journal_id: "run-journal".into(),
            local_run_id: "r0000".into(),
            run_registration_state: "completed".into(),
            run_sequence: 0,
            run_started_at_utc: "2026-07-26T00:00:00.000Z".into(),
            run_ended_at_utc: "2026-07-26T00:01:00.000Z".into(),
            qpc_frequency: 10_000_000,
            session_frame_start: 0,
            preflight_id: "pf-1".into(),
            preflight_result: "dev_non_trainable_pass".into(),
            preflight_evidence_hash: "sha256:22".into(),
            capture_engine: "native_wgc_window_v1".into(),
            encoder_backend: "mf_nvidia_hardware_v1".into(),
            width: 1920,
            height: 1080,
            fps_num: 30,
            fps_den: 1,
            remote_run_id: None,
            run_frame_count: Some(300),
            run_stop_reason: Some("manual".into()),
            run_error_code: None,
            segment_index: 0,
            segment_session_frame_start: 0,
            frame_count: 300,
            qpc_start: 0,
            qpc_end: 100_000,
            event_count: 5,
            duplicate_frames: 0,
            source_superseded: 0,
            source_drops: 0,
            remote_clip_id: None,
            clip_revision: None,
            remote_upload_intent_id: None,
            audio: None,
            artifacts: Vec::new(),
        }
    }

    #[test]
    fn dev_classification_stays_blocked_in_either_environment() {
        for environment in [ApiEnvironment::Production, ApiEnvironment::Staging] {
            let envelope = classification(&candidate_stub(true, None), environment).unwrap();
            assert_eq!(envelope["sourceFamily"], "dm-local-dev");
            assert_eq!(envelope["nonTrainable"], true);
            assert_eq!(envelope["isTrainable"], false);
            assert_eq!(envelope["productionCertified"], false);
        }
        let staging = classification(&candidate_stub(true, None), ApiEnvironment::Staging).unwrap();
        assert_eq!(staging["environment"], "staging");
        assert_eq!(staging["storageNamespace"], "raw-staging");
        let production =
            classification(&candidate_stub(true, None), ApiEnvironment::Production).unwrap();
        assert_eq!(production["environment"], "production");
        assert_eq!(production["storageNamespace"], "raw");
    }

    /// run 结果字段随 run PUT 上报（docs/67 §3.2）。journal 里没有的字段**不发**：
    /// 服务端拒绝空串，而「缺字段」与老客户端的载荷逐字节一致，这正是服务端把
    /// 三个字段设为可选的用意。
    #[test]
    fn run_body_carries_the_run_outcome_and_omits_what_the_journal_lacks() {
        let mut candidate = candidate_stub(true, None);
        let body = run_body(&candidate);
        assert_eq!(body["frameCount"], 300);
        assert_eq!(body["stopReason"], "manual");
        assert!(body.get("errorCode").is_none());

        candidate.run_frame_count = None;
        candidate.run_stop_reason = Some("   ".into());
        candidate.run_error_code = Some("CAPTURE_QUEUE_DROPPED".into());
        let body = run_body(&candidate);
        assert!(body.get("frameCount").is_none());
        assert!(body.get("stopReason").is_none());
        assert_eq!(body["errorCode"], "CAPTURE_QUEUE_DROPPED");
    }

    /// `non_trainable=0` 与退役的 certified 词表都是 journal 损坏，永久拒绝而不是
    /// 猜一个家族贴上去（docs/100 之后两者都没有任何写入路径能产出）。
    #[test]
    fn a_retired_lane_marker_or_vocabulary_is_refused() {
        for environment in [ApiEnvironment::Production, ApiEnvironment::Staging] {
            // non_trainable=0：本地永远不主张可训练。
            assert!(matches!(
                classification(
                    &candidate_stub(false, Some("33333333-3333-4333-8333-333333333333")),
                    environment
                ),
                Err(AttemptError::Permanent("UPLOAD_JOURNAL_INVALID"))
            ));
            // 退役的 certified 词表。
            let mut retired = candidate_stub(true, None);
            retired.session_schema_version = "dm-certified-session-2".into();
            assert!(matches!(
                classification(&retired, environment),
                Err(AttemptError::Permanent("UPLOAD_JOURNAL_INVALID"))
            ));
        }
    }

    #[test]
    fn audio_session_declares_the_versioned_recording_contract() {
        let mut candidate = candidate_stub(true, None);
        candidate.session_schema_version = "dm-local-dev-session-3".into();
        let envelope = classification(&candidate, ApiEnvironment::Production).unwrap();
        assert_eq!(
            envelope["artifactSchemaVersion"],
            "dm-local-dev-recording-2"
        );
    }

    /// 中性契约的信封：换家族、换 artifact 世代，而三个训练布尔**整体缺席**
    /// （issue #855 §B）。缺席不是省事——服务端的 `retiredClassificationKeys`
    /// 会把带了其中任何一个的请求当场 403。
    #[test]
    fn the_neutral_contract_declares_its_own_family_and_no_training_booleans() {
        let mut candidate = candidate_stub(true, None);
        candidate.session_schema_version = "dm-session-ingest-2".into();
        let envelope = classification(&candidate, ApiEnvironment::Production).unwrap();
        assert_eq!(envelope["sourceFamily"], "dm-session-ingest");
        assert_eq!(envelope["artifactSchemaVersion"], "dm-raw-recording-1");
        assert_eq!(envelope["environment"], "production");
        assert_eq!(envelope["storageNamespace"], "raw");
        for retired in ["nonTrainable", "isTrainable", "productionCertified"] {
            assert!(
                envelope.get(retired).is_none(),
                "the neutral envelope must not carry {retired}"
            );
        }
        // 音频不再分世代：v2 只有一个 artifact schema，与 session 里有没有音频无关。
        candidate.audio = None;
        assert_eq!(
            classification(&candidate, ApiEnvironment::Production).unwrap()["artifactSchemaVersion"],
            "dm-raw-recording-1"
        );
    }

    /// 会话注册体的 Catalog 身份：legacy 带 `mode`，中性只有 id/version/contentHash。
    /// **provenance 一个字节没少，车道词没有了**（issue #855 §A.4）。
    #[test]
    fn the_catalog_identity_drops_its_lane_word_on_the_neutral_contract() {
        let legacy = session_registration_from_upload(&candidate_stub(true, None));
        let body = session_body(&legacy, None, ApiEnvironment::Production).unwrap();
        assert_eq!(body["catalog"]["mode"], "dev_non_trainable");

        let mut neutral = legacy.clone();
        neutral.session_schema_version = "dm-session-ingest-2".into();
        neutral.session_contract = "dm-session-ingest".into();
        let body = session_body(&neutral, None, ApiEnvironment::Production).unwrap();
        assert!(body["catalog"].get("mode").is_none());
        assert_eq!(body["sessionSchemaVersion"], "dm-session-ingest-2");
        assert_eq!(body["classification"]["sourceFamily"], "dm-session-ingest");
    }

    /// journal 那两列打架时宁可停下，也不要挑一列继续传：挑错了就是把一整场采集
    /// 送进错误的车道。这是本地 journal v33 成对 CHECK 在出境前的最后一道镜像。
    #[test]
    fn a_session_whose_journal_columns_disagree_never_reaches_the_wire() {
        let mut drifted = session_registration_from_upload(&candidate_stub(true, None));
        drifted.session_schema_version = "dm-session-ingest-2".into();
        // session_contract 仍停在 dm-local-dev——半迁移的形状。
        assert!(matches!(
            session_body(&drifted, None, ApiEnvironment::Production),
            Err(AttemptError::Permanent("UPLOAD_JOURNAL_INVALID"))
        ));

        let mut reversed = session_registration_from_upload(&candidate_stub(true, None));
        reversed.session_contract = "dm-session-ingest".into();
        assert!(matches!(
            session_body(&reversed, None, ApiEnvironment::Production),
            Err(AttemptError::Permanent("UPLOAD_JOURNAL_INVALID"))
        ));
    }

    #[test]
    #[cfg(feature = "depth-lab")]
    fn depth_clip_body_declares_the_same_bytes_as_the_seventh_artifact() {
        let temp = tempfile::tempdir().unwrap();
        let candidate = candidate_stub(true, None);
        let depth_path = temp.path().join("depth.zip");
        let mut zip = zip::ZipWriter::new(File::create(&depth_path).unwrap());
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        zip.start_file("depth_0.exr", options).unwrap();
        std::io::Write::write_all(&mut zip, b"exr").unwrap();
        zip.start_file("depth-files.json", options).unwrap();
        let inventory = serde_json::to_vec(&json!({
            "schema": "dm-depth-variant-r-zip-1", "segment_index": 0,
            "frames": [{"session_frame": 0, "present_seq": 10, "path": "depth_0.exr", "size_bytes": 3, "sha256": format!("sha256:{}", "b".repeat(64))}]
        })).unwrap();
        std::io::Write::write_all(&mut zip, &inventory).unwrap();
        zip.finish().unwrap();
        let artifacts = vec![
            ResolvedArtifact {
                kind: "video",
                path: PathBuf::from("video.mp4"),
                byte_size: 11,
                sha256: format!("sha256:{}", "a".repeat(64)),
            },
            ResolvedArtifact {
                kind: "depth",
                path: depth_path,
                byte_size: 22,
                sha256: format!("sha256:{}", "b".repeat(64)),
            },
            meta_artifact(&temp, 0),
        ];
        let body = clip_body(&candidate, &artifacts, ApiEnvironment::Production).unwrap();
        assert_eq!(body["depth"]["state"], "ready");
        assert_eq!(body["depth"]["format"], "dm-depth-variant-r-zip-1");
        assert_eq!(
            body["depth"]["bytes"],
            body["artifacts"]["depth"]["byteSize"]
        );
        assert_eq!(
            body["depth"]["sha256"],
            body["artifacts"]["depth"]["sha256"]
        );
        assert!(body["artifacts"].get("video").is_some());
    }

    /// 服务端的 `inputOverflow` 硬故障位曾因上传体硬编码 0 而永远打不响
    /// （docs/146 §5）：上传体必须逐字透传引擎 meta 的 `health.input_overflow`。
    #[test]
    fn clip_body_passes_the_engine_input_overflow_through() {
        let temp = tempfile::tempdir().unwrap();
        let candidate = candidate_stub(true, None);

        let artifacts = vec![meta_artifact(&temp, 7)];
        let body = clip_body(&candidate, &artifacts, ApiEnvironment::Production).unwrap();
        assert_eq!(body["health"]["inputOverflow"], 7);

        // meta 缺席是 journal 损坏——permanent 拒绝，而不是悄悄当 0 处理：
        // 后者正是这次修掉的那种「检查永远失效」。
        assert!(matches!(
            clip_body(&candidate, &[], ApiEnvironment::Production),
            Err(AttemptError::Permanent("UPLOAD_JOURNAL_INVALID"))
        ));
    }

    #[test]
    fn dev_sessions_never_carry_a_declared_consent_id() {
        // Dev sessions must not carry the field at all (strict server schema).
        let dev = session_body(
            &session_registration_from_upload(&candidate_stub(true, None)),
            None,
            ApiEnvironment::Production,
        )
        .unwrap();
        assert!(dev.get("declaredConsentId").is_none());
        // 车道/词表交叉配对的拒绝由 `a_lane_and_vocabulary_cross_pairing_is_refused`
        // 单独钉。这里只管 dev 那一支不带该字段——两处各断言一件事。
    }

    #[test]
    fn session_artifact_put_replays_the_complete_signed_header_contract() {
        assert_eq!(
            session_artifact_put_headers(
                SessionArtifactKind::Session,
                42,
                "md5-base64==",
                ApiEnvironment::Production,
            ),
            HashMap::from([
                ("content-type".into(), "application/json".into()),
                ("content-md5".into(), "md5-base64==".into()),
                ("content-length".into(), "42".into()),
                (
                    "x-amz-meta-dataminer_class".into(),
                    "raw-non-trainable".into()
                ),
            ])
        );
        assert_eq!(
            session_artifact_put_headers(
                SessionArtifactKind::Markers,
                7,
                "marker-md5==",
                ApiEnvironment::Staging,
            )["content-type"],
            "application/x-ndjson"
        );
        assert_eq!(
            session_artifact_put_headers(
                SessionArtifactKind::Session,
                42,
                "md5-base64==",
                ApiEnvironment::Staging,
            )["x-amz-meta-dataminer_class"],
            "raw-staging-non-trainable"
        );
    }
}

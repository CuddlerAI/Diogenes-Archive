use std::collections::HashMap;
use std::fs;
use std::os::windows::fs::MetadataExt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, ensure};
use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};
use data_demo_converter::{
    IncrementalWallClockContext, ReadySegmentRef, advance_incremental, finalize_incremental,
    verify_dataset_against_raw_session,
};
use parking_lot::Mutex;
use tauri::{AppHandle, Emitter};

use crate::services::journal::{
    DataModelJobState, DataModelJobStatus, DataModelProgress, SessionJournal,
};

pub const DATA_MODEL_ROOT_NAME: &str = "non-trainable-data-model";
const DATA_MODEL_EVENT: &str = "collector://data-model";
const WAKE_POLL_INTERVAL: Duration = Duration::from_secs(2);
const INVALIDATED_ROOT_NAME: &str = ".invalidated";
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;

struct DataModelInner {
    app: AppHandle,
    journal: Arc<SessionJournal>,
    raw_root: PathBuf,
    work_root: PathBuf,
    ready_root: PathBuf,
    wake_tx: Sender<()>,
    stopping: AtomicBool,
    busy: AtomicBool,
    session_leases: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

pub struct DataModelService {
    inner: Arc<DataModelInner>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl DataModelService {
    pub fn start(
        app: AppHandle,
        journal: Arc<SessionJournal>,
        artifact_root: &Path,
        raw_root: &Path,
    ) -> anyhow::Result<Arc<Self>> {
        let derived_root = artifact_root.join(DATA_MODEL_ROOT_NAME);
        let work_root = derived_root.join("work");
        let ready_root = derived_root.join("ready");
        fs::create_dir_all(&work_root)?;
        fs::create_dir_all(&ready_root)?;
        cleanup_invalidated_outputs(&journal, &derived_root, &work_root, &ready_root)?;

        let (wake_tx, wake_rx) = bounded(1);
        let inner = Arc::new(DataModelInner {
            app,
            journal,
            raw_root: raw_root.to_path_buf(),
            work_root,
            ready_root,
            wake_tx,
            stopping: AtomicBool::new(false),
            busy: AtomicBool::new(false),
            session_leases: Mutex::new(HashMap::new()),
        });
        let worker_inner = Arc::clone(&inner);
        let worker = std::thread::Builder::new()
            .name("dm-data-model-assembler".into())
            .spawn(move || worker_loop(&worker_inner, &wake_rx))
            .context("failed to start DataModel assembler worker")?;
        let service = Arc::new(Self {
            inner,
            worker: Mutex::new(Some(worker)),
        });
        service.wake();
        Ok(service)
    }

    /// Waking is intentionally best-effort: SQLite is the durable queue and the
    /// worker polls it as a fallback. Capture callbacks must never wait here.
    pub fn wake(&self) {
        match self.inner.wake_tx.try_send(()) {
            Ok(()) | Err(TrySendError::Full(())) => {}
            Err(TrySendError::Disconnected(())) => {
                tracing::warn!("DataModel assembler wake channel is disconnected");
            }
        }
    }

    #[must_use]
    pub fn is_quiescent(&self) -> bool {
        !self.inner.busy.load(Ordering::Acquire)
    }

    /// Atomically prevents future durable claims. An in-flight job is allowed
    /// to reach its next persisted terminal/retry checkpoint before shutdown.
    pub fn begin_shutdown(&self) {
        self.inner.stopping.store(true, Ordering::Release);
        let _ = self.inner.wake_tx.try_send(());
    }

    pub fn status(&self, session_id: &str) -> anyhow::Result<Option<DataModelJobStatus>> {
        self.inner.journal.data_model_job(session_id)
    }

    pub fn output_directory(&self, session_id: &str) -> anyhow::Result<PathBuf> {
        ensure_safe_component(session_id, "session id")?;
        let status = self
            .inner
            .journal
            .data_model_job(session_id)?
            .context("DataModel job does not exist")?;
        resolve_output_directory(&status, &self.inner.ready_root)
    }

    pub fn with_session_artifact_operation<T>(
        &self,
        session_id: &str,
        operation: impl FnOnce() -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        ensure_safe_component(session_id, "session id")?;
        let lease = session_lease(&self.inner, session_id);
        let _guard = lease.lock();
        operation()
    }
}

impl Drop for DataModelService {
    fn drop(&mut self) {
        self.begin_shutdown();
        if let Some(worker) = self.worker.lock().take()
            && worker.thread().id() != std::thread::current().id()
            && let Err(error) = worker.join()
        {
            tracing::error!(
                ?error,
                "DataModel assembler worker panicked during shutdown"
            );
        }
    }
}

struct BusyGuard<'a> {
    busy: &'a AtomicBool,
}

impl<'a> BusyGuard<'a> {
    fn acquire(busy: &'a AtomicBool) -> Self {
        busy.store(true, Ordering::Release);
        Self { busy }
    }
}

impl Drop for BusyGuard<'_> {
    fn drop(&mut self) {
        self.busy.store(false, Ordering::Release);
    }
}

fn cleanup_invalidated_outputs(
    journal: &SessionJournal,
    derived_root: &Path,
    work_root: &Path,
    ready_root: &Path,
) -> anyhow::Result<()> {
    for session_id in journal.invalidated_data_model_sessions()? {
        cleanup_invalidated_session_output(&session_id, derived_root, work_root, ready_root)?;
    }
    Ok(())
}

fn cleanup_invalidated_session_output(
    session_id: &str,
    derived_root: &Path,
    work_root: &Path,
    ready_root: &Path,
) -> anyhow::Result<()> {
    ensure_safe_component(session_id, "invalidated session id")?;
    let staging = derived_root.join(INVALIDATED_ROOT_NAME).join(session_id);
    remove_invalidated_staging(&staging, session_id)?;
    stage_invalidated_output(&work_root.join(session_id), &staging.join("work"))?;
    stage_invalidated_output(&ready_root.join(session_id), &staging.join("ready"))?;
    remove_invalidated_staging(&staging, session_id)
}

fn remove_invalidated_staging(staging: &Path, session_id: &str) -> anyhow::Result<()> {
    if staging.exists() {
        let metadata = fs::symlink_metadata(staging)?;
        ensure!(
            metadata.is_dir(),
            "invalidated DataModel staging path is not a directory"
        );
        ensure_tree_has_no_reparse_points(staging)?;
        fs::remove_dir_all(staging).with_context(|| {
            format!("failed to remove invalidated DataModel output for {session_id}")
        })?;
    }
    Ok(())
}

fn stage_invalidated_output(source: &Path, target: &Path) -> anyhow::Result<()> {
    if !source.exists() {
        return Ok(());
    }
    ensure!(
        !target.exists(),
        "invalidated DataModel staging target already exists"
    );
    let parent = target
        .parent()
        .context("invalidated DataModel staging target has no parent")?;
    fs::create_dir_all(parent)?;
    fs::rename(source, target).with_context(|| {
        format!(
            "failed to stage invalidated DataModel output {}",
            source.display()
        )
    })
}

fn ensure_tree_has_no_reparse_points(path: &Path) -> anyhow::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0,
        "invalidated DataModel cleanup refuses filesystem reparse points"
    );
    if metadata.is_dir() {
        for entry in fs::read_dir(path)? {
            ensure_tree_has_no_reparse_points(&entry?.path())?;
        }
    }
    Ok(())
}

fn worker_loop(inner: &Arc<DataModelInner>, wake_rx: &Receiver<()>) {
    while !inner.stopping.load(Ordering::Acquire) {
        let _ = wake_rx.recv_timeout(WAKE_POLL_INTERVAL);
        if inner.stopping.load(Ordering::Acquire) {
            break;
        }
        drain_jobs(inner);
    }
}

fn drain_jobs(inner: &Arc<DataModelInner>) {
    loop {
        // Quiescence covers the durable claim itself, not only converter I/O.
        // Otherwise safe-exit can observe an already-running DB job as idle.
        let _busy = BusyGuard::acquire(&inner.busy);
        if inner.stopping.load(Ordering::Acquire) {
            return;
        }
        let job = match inner.journal.claim_next_data_model_job() {
            Ok(Some(job)) => job,
            Ok(None) => return,
            Err(error) => {
                tracing::error!(error = %error, "failed to claim a durable DataModel job");
                return;
            }
        };
        emit_status(inner, &job);
        if inner.stopping.load(Ordering::Acquire) {
            match inner
                .journal
                .release_data_model_job(&job.session_id, Some("DATAMODEL_WORKER_STOPPING"))
            {
                Ok(status) => emit_status(inner, &status),
                Err(error) => tracing::error!(
                    error = %error,
                    session_id = job.session_id,
                    "failed to release DataModel job during shutdown"
                ),
            }
            return;
        }
        match catch_unwind(AssertUnwindSafe(|| process_job(inner, &job))) {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                handle_job_error(inner, &job.session_id, &error);
                return;
            }
            Err(payload) => {
                handle_job_panic(inner, &job.session_id, payload);
                return;
            }
        }
        if inner.stopping.load(Ordering::Acquire) {
            return;
        }
    }
}

fn process_job(inner: &Arc<DataModelInner>, job: &DataModelJobStatus) -> anyhow::Result<()> {
    ensure_safe_component(&job.session_id, "session id")?;
    ensure!(
        matches!(
            job.state,
            DataModelJobState::Running | DataModelJobState::Finalizing
        ),
        "claimed DataModel job is not running"
    );
    let lease = session_lease(inner, &job.session_id);
    let _guard = lease.lock();

    let refs = inner
        .journal
        .ready_segments_for_data_model(&job.session_id)?;
    ensure!(
        refs.len() >= job.ready_segments as usize,
        "journal ready segment count moved backwards after DataModel claim"
    );
    let converter_refs = refs
        .iter()
        .map(|ready| ReadySegmentRef {
            run_id: ready.run_name.clone(),
            segment_index: ready.segment_index,
        })
        .collect::<Vec<_>>();
    let raw_session = inner.raw_root.join(&job.session_id);
    let work_output = inner.work_root.join(&job.session_id);
    let ready_output = inner.ready_root.join(&job.session_id);

    if job.state == DataModelJobState::Finalizing && converter_refs.is_empty() {
        let status = inner.journal.mark_data_model_empty(&job.session_id)?;
        emit_status(inner, &status);
        return Ok(());
    }

    let status = if job.state == DataModelJobState::Finalizing {
        let report = if ready_output.is_dir() {
            // Always enter the converter's idempotent finalized-output path: it
            // verifies/binds ready bytes, performs the narrowly recognized
            // legacy catalog migration, and cleans crash-left migration staging
            // even when the visible ready dataset is already valid.
            let rebuilt = finalize_incremental(&raw_session, &ready_output, &converter_refs)
                .context("published DataModel output does not match RAW during recovery")?;
            let verified = verify_dataset_against_raw_session(&raw_session, &ready_output)
                .context("published DataModel output does not match RAW after recovery")?;
            ensure!(
                rebuilt.session_id == verified.session_id
                    && rebuilt.frame_count == verified.frame_count
                    && rebuilt.chunk_count == verified.chunk_count,
                "recovered published DataModel output identity changed"
            );
            data_demo_converter::ConversionReport {
                session_id: verified.session_id,
                frame_count: verified.frame_count,
                chunk_count: verified.chunk_count,
                output: ready_output.clone(),
            }
        } else {
            let report = finalize_incremental(&raw_session, &work_output, &converter_refs)?;
            publish_final_output(&work_output, &ready_output)?;
            let verified = verify_dataset_against_raw_session(&raw_session, &ready_output)
                .context("atomically published DataModel output does not match RAW")?;
            ensure!(
                verified.session_id == report.session_id
                    && verified.frame_count == report.frame_count
                    && verified.chunk_count == report.chunk_count,
                "published DataModel output identity changed after rename"
            );
            data_demo_converter::ConversionReport {
                output: ready_output.clone(),
                ..report
            }
        };
        ensure!(
            report.session_id == job.session_id,
            "DataModel output belongs to another session"
        );
        let progress = DataModelProgress {
            processed_segments: u32::try_from(converter_refs.len())?,
            published_chunks: report.chunk_count,
            published_frames: report.frame_count,
            pending_frames: 0,
        };
        inner.journal.mark_data_model_completed(
            &job.session_id,
            &progress,
            &format!("ready/{}", job.session_id),
        )?
    } else {
        ensure!(
            !converter_refs.is_empty(),
            "pending DataModel job contains no journal-ready segments"
        );
        let wall_clock = inner
            .journal
            .data_model_wall_clock_context(&job.session_id)?
            .map(|context| IncrementalWallClockContext {
                anchor_at_utc: context.anchor_at_utc,
                anchor_qpc: context.anchor_qpc,
                qpc_frequency: context.qpc_frequency,
            });
        let report = advance_incremental(
            &raw_session,
            &work_output,
            &converter_refs,
            wall_clock.as_ref(),
        )?;
        ensure!(
            report.session_id == job.session_id,
            "incremental DataModel output belongs to another session"
        );
        inner.journal.save_data_model_progress(
            &job.session_id,
            &DataModelProgress {
                processed_segments: report.ready_segments,
                published_chunks: report.published_chunks,
                published_frames: report.published_frames,
                pending_frames: report.pending_frames,
            },
        )?
    };
    emit_status(inner, &status);
    Ok(())
}

fn publish_final_output(work_output: &Path, ready_output: &Path) -> anyhow::Result<()> {
    ensure!(
        work_output.is_dir(),
        "final DataModel work output is missing"
    );
    ensure!(
        !ready_output.exists(),
        "final DataModel ready output already exists"
    );
    let ready_root = ready_output
        .parent()
        .context("DataModel ready output has no parent")?;
    fs::create_dir_all(ready_root)?;
    fs::rename(work_output, ready_output).with_context(|| {
        format!(
            "failed to publish DataModel output {}",
            ready_output.display()
        )
    })
}

fn handle_job_error(inner: &Arc<DataModelInner>, session_id: &str, error: &anyhow::Error) {
    let (retryable, code) = if error
        .chain()
        .any(|cause| cause.downcast_ref::<std::io::Error>().is_some())
    {
        (true, "DATA_MODEL_RETRYABLE_IO")
    } else {
        (false, "DATA_MODEL_VALIDATION_FAILED")
    };
    let status = if retryable {
        inner.journal.release_data_model_job(session_id, Some(code))
    } else {
        inner.journal.mark_data_model_failed(session_id, code)
    };
    match status {
        Ok(status) => emit_status(inner, &status),
        Err(journal_error) => tracing::error!(
            error = %journal_error,
            session_id,
            "failed to persist DataModel assembler error"
        ),
    }
    if retryable {
        tracing::warn!(error = %error, session_id, "DataModel assembler will retry after I/O failure");
    } else {
        tracing::error!(error = %error, session_id, "DataModel assembler rejected invalid source data");
    }
}

fn handle_job_panic(
    inner: &Arc<DataModelInner>,
    session_id: &str,
    payload: Box<dyn std::any::Any + Send + 'static>,
) {
    let detail = payload
        .downcast_ref::<&str>()
        .map(|value| (*value).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "non-string panic payload".into());
    match inner
        .journal
        .mark_data_model_failed(session_id, "DATA_MODEL_WORKER_PANIC")
    {
        Ok(status) => emit_status(inner, &status),
        Err(error) => tracing::error!(
            error = %error,
            session_id,
            "failed to persist DataModel worker panic"
        ),
    }
    tracing::error!(
        session_id,
        panic = %detail,
        "DataModel assembler recovered from a worker panic"
    );
}

fn emit_status(inner: &DataModelInner, status: &DataModelJobStatus) {
    if let Err(error) = inner.app.emit(DATA_MODEL_EVENT, status) {
        tracing::warn!(error = %error, "failed to emit DataModel assembler status");
    }
}

fn session_lease(inner: &DataModelInner, session_id: &str) -> Arc<Mutex<()>> {
    Arc::clone(
        inner
            .session_leases
            .lock()
            .entry(session_id.to_owned())
            .or_insert_with(|| Arc::new(Mutex::new(()))),
    )
}

fn ensure_safe_component(value: &str, label: &str) -> anyhow::Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 128
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')),
        "{label} is not a safe path component"
    );
    Ok(())
}

fn resolve_output_directory(
    status: &DataModelJobStatus,
    ready_root: &Path,
) -> anyhow::Result<PathBuf> {
    ensure_safe_component(&status.session_id, "session id")?;
    ensure!(
        status.state == DataModelJobState::Completed,
        "DataModel output is not completed"
    );
    let expected = format!("ready/{}", status.session_id);
    ensure!(
        status.output_relative_path.as_deref() == Some(expected.as_str()),
        "completed DataModel job has an unexpected output path"
    );
    canonical_child_directory(ready_root, &ready_root.join(&status.session_id))
}

fn canonical_child_directory(root: &Path, directory: &Path) -> anyhow::Result<PathBuf> {
    let root = fs::canonicalize(root).context("DataModel output root is unavailable")?;
    let directory =
        fs::canonicalize(directory).context("DataModel output directory is unavailable")?;
    ensure!(
        directory.is_dir() && directory.starts_with(&root) && directory != root,
        "DataModel output directory escaped its configured root"
    );
    Ok(directory)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_path_component_is_strict() {
        assert!(ensure_safe_component("dev-1234_0000", "session id").is_ok());
        for invalid in [
            "",
            "../session",
            "session/name",
            r"session\name",
            "session.name",
        ] {
            assert!(ensure_safe_component(invalid, "session id").is_err());
        }
    }

    #[test]
    fn busy_guard_clears_flag_during_panic_unwind() {
        let busy = AtomicBool::new(false);
        let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let _guard = BusyGuard::acquire(&busy);
            assert!(busy.load(Ordering::Acquire));
            panic!("worker panic fixture");
        }));

        assert!(result.is_err());
        assert!(!busy.load(Ordering::Acquire));
    }

    #[test]
    fn completed_output_directory_requires_the_durable_ready_path() {
        let temp = tempfile::tempdir().unwrap();
        let ready = temp.path().join("ready");
        fs::create_dir_all(ready.join("session-1")).unwrap();
        let mut status = output_status(DataModelJobState::Completed);

        let resolved = resolve_output_directory(&status, &ready).unwrap();
        assert_eq!(resolved, fs::canonicalize(ready.join("session-1")).unwrap());

        status.output_relative_path = Some("ready/another-session".into());
        assert!(resolve_output_directory(&status, &ready).is_err());
    }

    #[test]
    fn output_directory_rejects_every_non_completed_state() {
        let temp = tempfile::tempdir().unwrap();
        let ready = temp.path().join("ready");
        fs::create_dir_all(ready.join("session-1")).unwrap();

        for state in [
            DataModelJobState::Waiting,
            DataModelJobState::Pending,
            DataModelJobState::Running,
            DataModelJobState::Finalizing,
            DataModelJobState::Failed,
            DataModelJobState::Cancelled,
        ] {
            assert!(resolve_output_directory(&output_status(state), &ready).is_err());
        }
    }

    fn output_status(state: DataModelJobState) -> DataModelJobStatus {
        DataModelJobStatus {
            session_id: "session-1".into(),
            state,
            ready_segments: 1,
            processed_segments: 1,
            published_chunks: 1,
            published_frames: 120,
            pending_frames: 0,
            finalize_requested: state == DataModelJobState::Completed,
            output_relative_path: (state == DataModelJobState::Completed)
                .then(|| "ready/session-1".into()),
            error_code: None,
        }
    }

    #[test]
    fn invalidated_cleanup_replays_stale_staging_with_current_outputs() {
        let temp = tempfile::tempdir().unwrap();
        let derived = temp.path().join(DATA_MODEL_ROOT_NAME);
        let work = derived.join("work");
        let ready = derived.join("ready");
        let staged = derived
            .join(INVALIDATED_ROOT_NAME)
            .join("session-1")
            .join("work");
        fs::create_dir_all(work.join("session-1")).unwrap();
        fs::create_dir_all(ready.join("session-1")).unwrap();
        fs::create_dir_all(&staged).unwrap();
        fs::write(staged.join("stale.bin"), b"stale").unwrap();
        fs::write(work.join("session-1/chunk.bin"), b"work").unwrap();
        fs::write(ready.join("session-1/metadata.jsonl"), b"ready").unwrap();

        cleanup_invalidated_session_output("session-1", &derived, &work, &ready).unwrap();
        assert!(!work.join("session-1").exists());
        assert!(!ready.join("session-1").exists());
        assert!(
            !derived
                .join(INVALIDATED_ROOT_NAME)
                .join("session-1")
                .exists()
        );
    }
}

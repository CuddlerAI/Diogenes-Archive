//! Render-thread-safe bridge for bounded D3D11 DSV discovery.

use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Condvar, Mutex, RwLock, mpsc};

use depth_lab::{
    DsvDiscoveryBuilder, DsvDiscoveryConfig, DsvDiscoveryFrameObservation,
    DsvDiscoveryHookIntegrityFailureReasons, DsvDiscoveryLimits, DsvDiscoveryRuntimeCounters,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use windows::Wdk::Storage::FileSystem::{
    FILE_DISPOSITION_INFORMATION, FILE_RENAME_INFORMATION, FileDispositionInformation,
    FileRenameInformation, NtSetInformationFile,
};
use windows::Win32::Foundation::{FILETIME, GENERIC_READ, GENERIC_WRITE, HANDLE};
use windows::Win32::Storage::FileSystem::{
    DELETE, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TAG_INFO,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_INFO, FILE_SHARE_READ,
    FILE_SHARE_WRITE, FileAttributeTagInfo, FileIdInfo, GetFileInformationByHandleEx,
};
use windows::Win32::System::IO::IO_STATUS_BLOCK;
use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

const QUEUE_CAPACITY: usize = 8;
pub const DSV_DISCOVERY_ACTIVATION_ACK_SCHEMA_V1: &str = "d3d11-dsv-discovery-activation-ack/v1";
pub const DSV_DISCOVERY_ACTIVATION_ACK_FILE_NAME: &str = "d3d11-dsv-discovery-activation-ack.json";
const MAX_DSV_DISCOVERY_ACTIVATION_ACK_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DsvDiscoveryActivationAck {
    pub schema: String,
    pub status: String,
    pub discovery_id: String,
    pub session_id: String,
    pub process_id: u32,
    /// Decimal FILETIME string so JSON implementations cannot round the 64-bit value.
    pub process_start_time_filetime: String,
    pub executable_sha256: String,
    /// SHA-256 of the exact bounded probe-config bytes consumed by the probe parser.
    pub probe_config_sha256: String,
    pub output_leaf: String,
    pub limits: DsvDiscoveryLimits,
    pub runtime: DsvDiscoveryActivationRuntime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DsvDiscoveryActivationRuntime {
    pub writer_started: bool,
    pub present_hook_installed: bool,
    pub config_watcher_started: bool,
    pub discovery_runtime_enabled: bool,
    pub discovery_complete: bool,
    pub recording_ready: bool,
    pub formal_capture_armed: bool,
    pub selection_ready: bool,
    pub binding_ready: bool,
}

enum Command {
    Frame(DsvDiscoveryFrameObservation),
    Stop(mpsc::SyncSender<Result<PathBuf, String>>),
}

struct Runtime {
    tx: SyncSender<Command>,
    join: std::thread::JoinHandle<()>,
    config: DsvDiscoveryConfig,
    output_dir: PathBuf,
    /// Keeps the verified output directory object pinned against rename/delete for the entire
    /// discovery generation. ACK publication independently compares this object identity.
    _output_dir_handle: Arc<fs::File>,
    output_dir_identity: OutputDirectoryIdentity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OutputDirectoryIdentity {
    volume_serial_number: u64,
    file_id: [u8; 16],
}

// Render observations and activation identity checks are both read-only and may coexist. Lifecycle
// start/stop transitions take the write side, so ACK authoring can never manufacture a permanent
// queue-drop merely by validating the active generation.
static RUNTIME: RwLock<Option<Runtime>> = RwLock::new(None);
/// Serializes start/stop with the short activation-ACK commit point. Render callbacks never take
/// this lock; ACK staging I/O also happens outside it.
static ACTIVATION_LIFECYCLE_LOCK: Mutex<()> = Mutex::new(());
static ENABLED: AtomicBool = AtomicBool::new(false);
static MAX_CANDIDATES: AtomicU32 = AtomicU32::new(0);
const ENTRY_GATE_CLOSED: u64 = 1 << 63;
const ENTRY_GATE_COUNT_MASK: u64 = !ENTRY_GATE_CLOSED;
/// High bit closes admission; low bits count callbacks that have crossed the linearization point.
/// Render callbacks use one lock-free CAS and never wait here. Stop closes the high bit first, then
/// waits off the render thread until every admitted callback has released its guard.
static ENTRY_GATE_STATE: AtomicU64 = AtomicU64::new(ENTRY_GATE_CLOSED);
static ENTRY_WAIT_LOCK: Mutex<()> = Mutex::new(());
static ENTRY_WAIT_CVAR: Condvar = Condvar::new();
static QUEUE_DROPPED_FRAMES: AtomicU64 = AtomicU64::new(0);
static QUEUE_DROPPED_CANDIDATE_ROWS: AtomicU64 = AtomicU64::new(0);
static HOOK_DROPPED_EVENTS: AtomicU64 = AtomicU64::new(0);
static HOOK_INTEGRITY_FAILURES: AtomicU64 = AtomicU64::new(0);
static HOOK_INTEGRITY_UNSUPPORTED_EXECUTE_COMMAND_LIST: AtomicU64 = AtomicU64::new(0);
static HOOK_INTEGRITY_VTABLE_OR_SLOT_DRIFT: AtomicU64 = AtomicU64::new(0);
static HOOK_INTEGRITY_CONTEXT_DRIFT: AtomicU64 = AtomicU64::new(0);
static HOOK_INTEGRITY_TRACKER_FAILURE: AtomicU64 = AtomicU64::new(0);
static HOOK_INTEGRITY_LIFECYCLE_OR_QUEUE_FAILURE: AtomicU64 = AtomicU64::new(0);
/// Poison latch for any runtime counter that cannot represent another observation exactly.
/// A poisoned generation is never allowed to publish a sealed report.
static COUNTER_OVERFLOWED: AtomicBool = AtomicBool::new(false);
static DESCRIPTOR_IDENTITY_CONFLICTS: AtomicU64 = AtomicU64::new(0);
static CANDIDATE_LIMIT_EVENTS: AtomicU64 = AtomicU64::new(0);
static PASS_LEDGER_DROPPED_ROWS: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookIntegrityFailureReason {
    UnsupportedExecuteCommandList,
    VtableOrSlotDrift,
    ContextDrift,
    TrackerFailure,
    LifecycleOrQueueFailure,
}

#[cfg(test)]
static TEST_ACTIVATION_ACK_FAIL_BEFORE_COMMIT: AtomicBool = AtomicBool::new(false);

#[cfg(test)]
#[derive(Clone)]
struct ActivationAckCommitBarrierHook {
    reached: Arc<std::sync::Barrier>,
    resume: Arc<std::sync::Barrier>,
}

#[cfg(test)]
static TEST_ACTIVATION_ACK_COMMIT_BARRIER: Mutex<Option<ActivationAckCommitBarrierHook>> =
    Mutex::new(None);

#[cfg(test)]
static TEST_ACTIVATION_ACK_IDENTITY_BARRIER: Mutex<Option<ActivationAckCommitBarrierHook>> =
    Mutex::new(None);

#[cfg(test)]
fn pause_activation_ack_commit_for_test() {
    let hook = TEST_ACTIVATION_ACK_COMMIT_BARRIER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    if let Some(hook) = hook {
        hook.reached.wait();
        hook.resume.wait();
    }
}

#[cfg(test)]
fn pause_activation_ack_identity_for_test() {
    let hook = TEST_ACTIVATION_ACK_IDENTITY_BARRIER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    if let Some(hook) = hook {
        hook.reached.wait();
        hook.resume.wait();
    }
}

#[cfg(not(test))]
fn pause_activation_ack_identity_for_test() {}

#[cfg(not(test))]
fn pause_activation_ack_commit_for_test() {}

#[cfg(test)]
static TEST_RUNTIME_SERIAL: Mutex<()> = Mutex::new(());

#[cfg(test)]
pub(crate) fn lock_test_runtime() -> std::sync::MutexGuard<'static, ()> {
    TEST_RUNTIME_SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

struct EntryGuard;

impl Drop for EntryGuard {
    fn drop(&mut self) {
        let previous = ENTRY_GATE_STATE.fetch_sub(1, Ordering::Release);
        debug_assert_ne!(previous & ENTRY_GATE_COUNT_MASK, 0);
        if previous == (ENTRY_GATE_CLOSED | 1) {
            // Pair the zero transition with the waiter's mutex so a notification cannot be lost
            // between its state check and Condvar::wait. Recover poison without panicking on a
            // render callback; stop will record the poisoned barrier as an integrity failure.
            let guard = ENTRY_WAIT_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            ENTRY_WAIT_CVAR.notify_all();
            drop(guard);
        }
    }
}

fn try_enter() -> Option<EntryGuard> {
    #[cfg(test)]
    if current_test_entry_barrier()
        .is_some_and(|hook| hook.target_thread != std::thread::current().id())
    {
        return None;
    }
    let mut state = ENTRY_GATE_STATE.load(Ordering::Acquire);
    loop {
        if state & ENTRY_GATE_CLOSED != 0 {
            return None;
        }
        if state & ENTRY_GATE_COUNT_MASK == ENTRY_GATE_COUNT_MASK {
            // Physically unreachable for the bounded hook surface, but never wrap into the closed
            // bit and accidentally admit an untracked callback.
            increment_integrity_failure_raw(HookIntegrityFailureReason::LifecycleOrQueueFailure);
            return None;
        }
        match ENTRY_GATE_STATE.compare_exchange_weak(
            state,
            state + 1,
            Ordering::Acquire,
            Ordering::Relaxed,
        ) {
            Ok(_) => {
                let guard = EntryGuard;
                #[cfg(test)]
                pause_admitted_entry_for_test();
                return Some(guard);
            }
            Err(actual) => state = actual,
        }
    }
}

/// Keeps probe-side inspection and tracker/ledger mutation inside the same lifecycle epoch as the
/// report counter it may publish. `stop` closes this gate and waits for every admitted guard before
/// sealing, so a callback cannot mutate evidence and then lose its failure after the report closes.
pub(crate) struct ObservationEntryGuard(#[allow(dead_code)] EntryGuard);

pub(crate) fn try_enter_observation() -> Option<ObservationEntryGuard> {
    try_enter().map(ObservationEntryGuard)
}

impl ObservationEntryGuard {
    pub(crate) fn record_hook_drop(&self) {
        checked_increment_counter_by(&HOOK_DROPPED_EVENTS, 1);
    }

    pub(crate) fn record_integrity_failure(&self, reason: HookIntegrityFailureReason) {
        increment_integrity_failure_raw(reason);
    }

    pub(crate) fn observe_frame(&self, frame: DsvDiscoveryFrameObservation) {
        observe_frame_admitted(frame);
    }
}

/// Close admission and wait for the finite set of already admitted callbacks. Returns whether the
/// barrier itself showed corruption/poison so stop can seal that fact into the report.
fn close_entry_gate_and_wait() -> bool {
    let previous = ENTRY_GATE_STATE.fetch_or(ENTRY_GATE_CLOSED, Ordering::AcqRel);
    let mut barrier_failed = previous & ENTRY_GATE_CLOSED != 0;
    #[cfg(test)]
    notify_closed_gate_for_test();

    let mut wait_guard = match ENTRY_WAIT_LOCK.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            barrier_failed = true;
            poisoned.into_inner()
        }
    };
    while ENTRY_GATE_STATE.load(Ordering::Acquire) & ENTRY_GATE_COUNT_MASK != 0 {
        wait_guard = match ENTRY_WAIT_CVAR.wait(wait_guard) {
            Ok(guard) => guard,
            Err(poisoned) => {
                barrier_failed = true;
                poisoned.into_inner()
            }
        };
    }
    barrier_failed
}

pub fn start(
    config: DsvDiscoveryConfig,
    output_dir: &Path,
    actual_executable_sha256: &str,
) -> Result<(), String> {
    let _lifecycle = ACTIVATION_LIFECYCLE_LOCK
        .lock()
        .map_err(|_| "D3D11 DSV discovery lifecycle lock poisoned".to_string())?;
    config.validate()?;
    if config.executable_sha256 != actual_executable_sha256 {
        return Err(format!(
            "D3D11 DSV discovery executable hash mismatch: expected={} actual={actual_executable_sha256}",
            config.executable_sha256
        ));
    }
    let mut runtime = RUNTIME
        .write()
        .map_err(|_| "D3D11 DSV discovery runtime lock poisoned".to_string())?;
    if runtime.is_some() || ENABLED.load(Ordering::Acquire) {
        return Err("D3D11 DSV discovery runtime is already active".into());
    }
    if ENTRY_GATE_STATE.load(Ordering::Acquire) != ENTRY_GATE_CLOSED {
        return Err("D3D11 DSV discovery entry barrier was not cleanly closed".into());
    }
    let output_dir =
        depth_lab::create_new_evidence_directory(Path::new(&config.evidence_root), output_dir)?;
    let (output_dir_handle, output_dir_identity) = open_pinned_directory(&output_dir)?;
    let evidence_root = PathBuf::from(&config.evidence_root);
    let runtime_config = config.clone();
    let runtime_output_dir = output_dir.clone();
    reset_counters();
    MAX_CANDIDATES.store(config.limits.max_candidates, Ordering::Release);
    let (tx, rx) = sync_channel::<Command>(QUEUE_CAPACITY);
    let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<(), String>>(1);
    let join = std::thread::Builder::new()
        .name("dm-dsv-discovery-writer".into())
        .spawn(move || {
            let mut builder = match DsvDiscoveryBuilder::new(config) {
                Ok(builder) => builder,
                Err(error) => {
                    let _ = ready_tx.send(Err(error.clone()));
                    crate::probe::log(&format!("D3D11 DSV discovery builder failed: {error}"));
                    return;
                }
            };
            if ready_tx.send(Ok(())).is_err() {
                crate::probe::log(
                    "D3D11 DSV discovery activation cancelled before writer readiness",
                );
                return;
            }
            while let Ok(command) = rx.recv() {
                match command {
                    Command::Frame(frame) => builder.observe_frame(frame),
                    Command::Stop(reply) => {
                        #[cfg(test)]
                        pause_writer_seal_for_test();
                        let result = counters_checked()
                            .and_then(|runtime| builder.finish(runtime))
                            .and_then(|report| {
                                depth_lab::write_report_create_new_under_root(
                                    &evidence_root,
                                    &output_dir,
                                    &report,
                                )
                            });
                        let _ = reply.send(result);
                        return;
                    }
                }
            }
            crate::probe::log(
                "D3D11 DSV discovery writer disconnected before a sealed report was requested",
            );
        })
        .map_err(|error| format!("cannot start D3D11 DSV discovery writer: {error}"))?;
    match ready_rx.recv() {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            let _ = join.join();
            return Err(format!(
                "cannot initialize D3D11 DSV discovery writer: {error}"
            ));
        }
        Err(_) => {
            let _ = join.join();
            return Err("D3D11 DSV discovery writer returned no readiness signal".into());
        }
    }
    *runtime = Some(Runtime {
        tx,
        join,
        config: runtime_config,
        output_dir: runtime_output_dir,
        _output_dir_handle: Arc::new(output_dir_handle),
        output_dir_identity,
    });
    ENTRY_GATE_STATE.store(0, Ordering::Release);
    ENABLED.store(true, Ordering::Release);
    Ok(())
}

/// Seal a probe-authored acknowledgement only after the Present hook, writer runtime and (for
/// managed injection) config watcher are live. The acknowledgement deliberately does not claim
/// that a game Present was observed or that the eventual report is complete/usable.
pub(crate) fn write_activation_ack(
    config: &DsvDiscoveryConfig,
    session_id: &str,
    output_dir: &Path,
    actual_executable_sha256: &str,
    probe_config_sha256: &str,
    config_watcher_started: bool,
) -> Result<PathBuf, String> {
    config.validate()?;
    if config.executable_sha256 != actual_executable_sha256 {
        return Err("D3D11 DSV activation executable hash disagrees with the active config".into());
    }
    if !is_canonical_sha256(probe_config_sha256) {
        return Err("D3D11 DSV activation config hash is not canonical SHA-256".into());
    }
    if session_id.is_empty()
        || session_id.len() > 160
        || !session_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err("D3D11 DSV activation session id is unsafe".into());
    }

    let evidence_root = Path::new(&config.evidence_root);
    reject_reparse_chain(evidence_root, "activation evidence root")?;
    reject_reparse_chain(output_dir, "activation output directory")?;
    let canonical_root = evidence_root
        .canonicalize()
        .map_err(|error| format!("cannot resolve DSV activation evidence root: {error}"))?;
    let output_metadata = fs::symlink_metadata(output_dir)
        .map_err(|error| format!("cannot inspect DSV activation output directory: {error}"))?;
    if is_reparse_point(&output_metadata) || !output_metadata.is_dir() {
        return Err("D3D11 DSV activation output is not a plain directory".into());
    }
    let canonical_output = output_dir
        .canonicalize()
        .map_err(|error| format!("cannot resolve DSV activation output directory: {error}"))?;
    if canonical_output.parent() != Some(canonical_root.as_path()) {
        return Err("D3D11 DSV activation output escaped evidence_root".into());
    }
    // Pin the exact directory object before any path-based staging work. Excluding delete sharing
    // prevents this directory from being renamed, replaced or converted to a junction before the
    // final handle-relative commit.
    let (output_dir_handle, output_dir_identity) = open_pinned_directory(&canonical_output)?;
    let output_leaf = canonical_output
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| is_safe_leaf(name))
        .ok_or("D3D11 DSV activation output leaf is unsafe")?
        .to_string();

    let ack = DsvDiscoveryActivationAck {
        schema: DSV_DISCOVERY_ACTIVATION_ACK_SCHEMA_V1.into(),
        status: "probe_discovery_runtime_activated".into(),
        discovery_id: config.discovery_id.clone(),
        session_id: session_id.to_string(),
        process_id: std::process::id(),
        process_start_time_filetime: current_process_start_filetime()?.to_string(),
        executable_sha256: actual_executable_sha256.to_string(),
        probe_config_sha256: probe_config_sha256.to_string(),
        output_leaf,
        limits: config.limits,
        runtime: DsvDiscoveryActivationRuntime {
            writer_started: true,
            present_hook_installed: true,
            config_watcher_started,
            discovery_runtime_enabled: true,
            discovery_complete: false,
            recording_ready: false,
            formal_capture_armed: false,
            selection_ready: false,
            binding_ready: false,
        },
    };
    let mut bytes = serde_json::to_vec_pretty(&ack)
        .map_err(|error| format!("cannot serialize DSV discovery activation ACK: {error}"))?;
    bytes.push(b'\n');
    if bytes.len() > MAX_DSV_DISCOVERY_ACTIVATION_ACK_BYTES {
        return Err("serialized DSV discovery activation ACK exceeds its byte bound".into());
    }

    let final_path = canonical_output.join(DSV_DISCOVERY_ACTIVATION_ACK_FILE_NAME);
    if final_path.exists() {
        return Err("D3D11 DSV discovery activation ACK already exists".into());
    }
    let pending_leaf = format!(".dm-dsv-activation-ack-pending-{}", Uuid::new_v4().simple());
    let pending_path = canonical_output.join(&pending_leaf);
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .access_mode(GENERIC_READ.0 | GENERIC_WRITE.0 | DELETE.0)
        .share_mode(0)
        .open(&pending_path)
        .map_err(|error| format!("cannot create staged DSV activation ACK: {error}"))?;
    let write_result = (|| -> Result<(), String> {
        file.write_all(&bytes)
            .and_then(|()| file.sync_all())
            .map_err(|error| format!("cannot sync staged DSV activation ACK: {error}"))?;
        if file
            .metadata()
            .map_err(|error| format!("cannot inspect staged DSV activation ACK: {error}"))?
            .len()
            != bytes.len() as u64
        {
            return Err("staged DSV activation ACK length changed before commit".into());
        }
        file.seek(SeekFrom::Start(0))
            .map_err(|error| format!("cannot rewind staged DSV activation ACK: {error}"))?;
        let mut observed = Vec::with_capacity(bytes.len());
        (&mut file)
            .take(MAX_DSV_DISCOVERY_ACTIVATION_ACK_BYTES as u64 + 1)
            .read_to_end(&mut observed)
            .map_err(|error| format!("cannot verify staged DSV activation ACK: {error}"))?;
        if observed != bytes {
            return Err("staged DSV activation ACK bytes changed before commit".into());
        }
        #[cfg(test)]
        if TEST_ACTIVATION_ACK_FAIL_BEFORE_COMMIT.swap(false, Ordering::AcqRel) {
            return Err("injected DSV activation ACK pre-commit failure".into());
        }
        // Only this short identity check + handle rename is serialized with start/stop. Render
        // callbacks use neither lock and cannot self-induce a dropped-frame blocker during ACK I/O.
        let _lifecycle = ACTIVATION_LIFECYCLE_LOCK
            .lock()
            .map_err(|_| "D3D11 DSV activation lifecycle lock poisoned".to_string())?;
        let runtime_guard = RUNTIME
            .read()
            .map_err(|_| "D3D11 DSV activation runtime lock poisoned".to_string())?;
        let active_runtime = runtime_guard
            .as_ref()
            .ok_or("cannot acknowledge a missing D3D11 DSV discovery runtime")?;
        if !enabled()
            || &active_runtime.config != config
            || active_runtime.config.executable_sha256 != actual_executable_sha256
            || active_runtime.output_dir != canonical_output
            || active_runtime.output_dir_identity != output_dir_identity
        {
            return Err("D3D11 DSV activation does not match the active runtime identity".into());
        }
        if final_path.exists() {
            return Err("D3D11 DSV discovery activation ACK appeared before commit".into());
        }
        pause_activation_ack_identity_for_test();
        // The lifecycle lock keeps start/stop from changing generations. Release the render-used
        // runtime read guard before the commit so a delayed filesystem operation cannot manufacture a
        // queue-dropped observation.
        drop(runtime_guard);
        pause_activation_ack_commit_for_test();
        rename_open_file_no_replace(
            &file,
            &output_dir_handle,
            DSV_DISCOVERY_ACTIVATION_ACK_FILE_NAME,
        )?;
        Ok(())
    })();
    if let Err(error) = write_result {
        return match discard_open_file(&file) {
            Ok(()) => Err(error),
            Err(cleanup) => Err(format!(
                "{error}; cannot discard owned staged DSV activation ACK: {cleanup}"
            )),
        };
    }

    // Atomic rename is the commit point. There are deliberately no fallible post-commit checks
    // that could return Err while leaving a success-shaped final ACK behind.
    Ok(final_path)
}

fn discard_open_file(file: &fs::File) -> Result<(), String> {
    let mut disposition = FILE_DISPOSITION_INFORMATION { DeleteFile: true };
    let mut io_status = IO_STATUS_BLOCK::default();
    unsafe {
        NtSetInformationFile(
            HANDLE(file.as_raw_handle()),
            &mut io_status,
            (&raw mut disposition).cast(),
            u32::try_from(std::mem::size_of::<FILE_DISPOSITION_INFORMATION>())
                .map_err(|_| "D3D11 DSV activation ACK disposition buffer is too large")?,
            FileDispositionInformation,
        )
    }
    .ok()
    .map_err(|error| format!("cannot mark staged DSV activation ACK for deletion: {error}"))
}

/// Rename the still-open, share-none staging handle. Path-based validation followed by rename has
/// a replace window; FileRenameInfo keeps the verified file object pinned through the commit.
fn rename_open_file_no_replace(
    file: &fs::File,
    output_dir: &fs::File,
    final_leaf: &str,
) -> Result<(), String> {
    if !is_safe_evidence_file_leaf(final_leaf) {
        return Err("D3D11 DSV activation ACK target leaf is unsafe".into());
    }
    let name = std::ffi::OsStr::new(final_leaf)
        .encode_wide()
        .collect::<Vec<_>>();
    let name_bytes = name
        .len()
        .checked_mul(std::mem::size_of::<u16>())
        .and_then(|length| u32::try_from(length).ok())
        .ok_or("D3D11 DSV activation ACK target path is too long")?;
    let header_bytes = std::mem::offset_of!(FILE_RENAME_INFORMATION, FileName);
    let buffer_bytes = header_bytes
        .checked_add(name_bytes as usize)
        .ok_or("D3D11 DSV activation ACK rename buffer overflow")?;
    let words = buffer_bytes
        .checked_add(std::mem::size_of::<usize>() - 1)
        .ok_or("D3D11 DSV activation ACK rename buffer overflow")?
        / std::mem::size_of::<usize>();
    let mut buffer = vec![0usize; words];
    let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFORMATION>();
    // SAFETY: `buffer` has pointer-width alignment and is sized through the flexible FileName
    // payload. `file` was opened with DELETE access and stays alive for the entire call.
    unsafe {
        (*info).Anonymous.ReplaceIfExists = false;
        (*info).RootDirectory = HANDLE(output_dir.as_raw_handle());
        (*info).FileNameLength = name_bytes;
        std::ptr::copy_nonoverlapping(name.as_ptr(), (*info).FileName.as_mut_ptr(), name.len());
        let mut io_status = IO_STATUS_BLOCK::default();
        NtSetInformationFile(
            HANDLE(file.as_raw_handle()),
            &mut io_status,
            info.cast(),
            u32::try_from(buffer_bytes)
                .map_err(|_| "D3D11 DSV activation ACK rename buffer is too large")?,
            FileRenameInformation,
        )
        .ok()
        .map_err(|error| format!("cannot atomically publish DSV activation ACK: {error}"))?;
    }
    Ok(())
}

fn open_pinned_directory(directory: &Path) -> Result<(fs::File, OutputDirectoryIdentity), String> {
    let file = OpenOptions::new()
        // RootDirectory for FileRenameInfo needs directory-list/add-file rights. Keep delete
        // sharing excluded so the verified directory object cannot be renamed or replaced.
        .access_mode(GENERIC_READ.0 | GENERIC_WRITE.0)
        .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0)
        // Open the directory object itself. Following a junction introduced between the path
        // checks and this open would otherwise turn the pinned handle into an escape primitive.
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0 | FILE_FLAG_OPEN_REPARSE_POINT.0)
        .open(directory)
        .map_err(|error| {
            format!(
                "cannot pin D3D11 DSV activation output directory {}: {error}",
                directory.display()
            )
        })?;
    let mut attributes = FILE_ATTRIBUTE_TAG_INFO::default();
    let mut file_id = FILE_ID_INFO::default();
    unsafe {
        GetFileInformationByHandleEx(
            HANDLE(file.as_raw_handle()),
            FileAttributeTagInfo,
            (&raw mut attributes).cast(),
            u32::try_from(std::mem::size_of::<FILE_ATTRIBUTE_TAG_INFO>())
                .map_err(|_| "D3D11 DSV output directory attribute buffer is too large")?,
        )
    }
    .map_err(|error| format!("cannot inspect D3D11 DSV activation output directory: {error}"))?;
    if attributes.FileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 == 0
        || attributes.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
    {
        return Err("D3D11 DSV activation output handle is not a plain directory".into());
    }
    unsafe {
        GetFileInformationByHandleEx(
            HANDLE(file.as_raw_handle()),
            FileIdInfo,
            (&raw mut file_id).cast(),
            u32::try_from(std::mem::size_of::<FILE_ID_INFO>())
                .map_err(|_| "D3D11 DSV output directory identity buffer is too large")?,
        )
    }
    .map_err(|error| format!("cannot identify D3D11 DSV activation output directory: {error}"))?;
    Ok((
        file,
        OutputDirectoryIdentity {
            volume_serial_number: file_id.VolumeSerialNumber,
            file_id: file_id.FileId.Identifier,
        },
    ))
}

fn is_safe_evidence_file_leaf(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 160
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        && !value.starts_with('.')
        && !value.ends_with('.')
        && !value.contains("..")
}

fn is_safe_leaf(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn is_canonical_sha256(value: &str) -> bool {
    value.len() == 71
        && value.starts_with("sha256:")
        && value[7..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn current_process_start_filetime() -> Result<u64, String> {
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &mut creation,
            &mut exit,
            &mut kernel,
            &mut user,
        )
    }
    .map_err(|error| format!("GetProcessTimes for DSV activation ACK failed: {error}"))?;
    let value = (u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime);
    if value == 0 {
        return Err("current process creation FILETIME is zero".into());
    }
    Ok(value)
}

fn is_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_type().is_symlink()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

fn reject_reparse_chain(path: &Path, label: &str) -> Result<(), String> {
    for ancestor in path.ancestors() {
        let metadata = fs::symlink_metadata(ancestor).map_err(|error| {
            format!(
                "cannot inspect D3D11 DSV {label} ancestor {}: {error}",
                ancestor.display()
            )
        })?;
        if is_reparse_point(&metadata) {
            return Err(format!(
                "D3D11 DSV {label} ancestor is a reparse point: {}",
                ancestor.display()
            ));
        }
    }
    Ok(())
}

#[must_use]
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Acquire)
}

#[must_use]
pub fn max_candidates() -> usize {
    MAX_CANDIDATES.load(Ordering::Acquire) as usize
}

/// Called from Present. It never waits for the writer.
pub fn observe_frame(frame: DsvDiscoveryFrameObservation) {
    if !enabled() {
        return;
    }
    let Some(_entry) = try_enter() else {
        return;
    };
    observe_frame_admitted(frame);
}

fn observe_frame_admitted(frame: DsvDiscoveryFrameObservation) {
    let candidate_rows = frame.candidates.len() as u64;
    let result = RUNTIME.try_read().ok().and_then(|runtime| {
        runtime
            .as_ref()
            .map(|runtime| runtime.tx.try_send(Command::Frame(frame)))
    });
    match result {
        Some(Ok(())) => {}
        Some(Err(TrySendError::Full(_))) | None => {
            checked_increment_counter_by(&QUEUE_DROPPED_FRAMES, 1);
            checked_increment_counter_by(&QUEUE_DROPPED_CANDIDATE_ROWS, candidate_rows);
        }
        Some(Err(TrySendError::Disconnected(_))) => {
            checked_increment_counter_by(&QUEUE_DROPPED_FRAMES, 1);
            checked_increment_counter_by(&QUEUE_DROPPED_CANDIDATE_ROWS, candidate_rows);
            increment_integrity_failure_raw(HookIntegrityFailureReason::LifecycleOrQueueFailure);
        }
    }
}

pub fn record_hook_drop() {
    record_counter(&HOOK_DROPPED_EVENTS);
}

pub fn record_integrity_failure(reason: HookIntegrityFailureReason) {
    if !enabled() {
        return;
    }
    let Some(_entry) = try_enter() else {
        return;
    };
    increment_integrity_failure_raw(reason);
}

pub fn record_descriptor_identity_conflict() {
    record_counter(&DESCRIPTOR_IDENTITY_CONFLICTS);
}

pub fn record_candidate_limit() {
    record_counter(&CANDIDATE_LIMIT_EVENTS);
}

pub fn record_pass_ledger_drop() {
    record_counter(&PASS_LEDGER_DROPPED_ROWS);
}

fn record_counter(counter: &AtomicU64) {
    if !enabled() {
        return;
    }
    let Some(_entry) = try_enter() else {
        return;
    };
    checked_increment_counter_by(counter, 1);
}

fn checked_increment_counter_by(counter: &AtomicU64, amount: u64) {
    if counter
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            current.checked_add(amount)
        })
        .is_err()
    {
        COUNTER_OVERFLOWED.store(true, Ordering::Release);
    }
}

fn increment_integrity_failure_raw(reason: HookIntegrityFailureReason) {
    checked_increment_counter_by(&HOOK_INTEGRITY_FAILURES, 1);
    match reason {
        HookIntegrityFailureReason::UnsupportedExecuteCommandList => {
            checked_increment_counter_by(&HOOK_INTEGRITY_UNSUPPORTED_EXECUTE_COMMAND_LIST, 1);
        }
        HookIntegrityFailureReason::VtableOrSlotDrift => {
            checked_increment_counter_by(&HOOK_INTEGRITY_VTABLE_OR_SLOT_DRIFT, 1);
        }
        HookIntegrityFailureReason::ContextDrift => {
            checked_increment_counter_by(&HOOK_INTEGRITY_CONTEXT_DRIFT, 1);
        }
        HookIntegrityFailureReason::TrackerFailure => {
            checked_increment_counter_by(&HOOK_INTEGRITY_TRACKER_FAILURE, 1);
        }
        HookIntegrityFailureReason::LifecycleOrQueueFailure => {
            checked_increment_counter_by(&HOOK_INTEGRITY_LIFECYCLE_OR_QUEUE_FAILURE, 1);
        }
    }
}

pub fn stop() -> Result<PathBuf, String> {
    let _lifecycle = ACTIVATION_LIFECYCLE_LOCK
        .lock()
        .map_err(|_| "D3D11 DSV discovery lifecycle lock poisoned".to_string())?;
    // Serialize the control transition against start before closing admission. The write guard is
    // deliberately exclusive: a Present already racing the stop boundary records an explicit
    // dropped frame, and its entry guard keeps stop from sealing counters early.
    let (mut runtime_slot, runtime_lock_poisoned) = match RUNTIME.write() {
        Ok(runtime) => (runtime, false),
        Err(poisoned) => (poisoned.into_inner(), true),
    };
    if runtime_slot.is_none() {
        return Err("D3D11 DSV discovery runtime is not active".into());
    }
    ENABLED.store(false, Ordering::Release);
    MAX_CANDIDATES.store(0, Ordering::Release);
    let barrier_failed = close_entry_gate_and_wait();
    if runtime_lock_poisoned || barrier_failed {
        increment_integrity_failure_raw(HookIntegrityFailureReason::LifecycleOrQueueFailure);
    }
    let runtime = runtime_slot
        .take()
        .expect("active D3D11 DSV discovery runtime checked above");

    let (reply_tx, reply_rx) = mpsc::sync_channel(1);
    let send_result = runtime.tx.send(Command::Stop(reply_tx));
    let reply_result = if send_result.is_ok() {
        Some(
            reply_rx
                .recv()
                .map_err(|_| "D3D11 DSV discovery writer returned no report".to_string()),
        )
    } else {
        None
    };
    let join_result = runtime.join.join();
    let result = if join_result.is_err() {
        Err("D3D11 DSV discovery writer panicked".into())
    } else if send_result.is_err() {
        Err("D3D11 DSV discovery writer exited before Stop".into())
    } else {
        match reply_result {
            Some(Ok(writer_result)) => writer_result,
            Some(Err(error)) => Err(error),
            None => Err("D3D11 DSV discovery Stop lost its reply state".into()),
        }
    };
    // Keep runtime_slot locked until the old writer has replied and joined. A concurrent start may
    // proceed only after every old counter read and report write is finished, so its reset_counters
    // cannot contaminate the previous generation.
    if runtime_lock_poisoned {
        RUNTIME.clear_poison();
    }
    if barrier_failed {
        ENTRY_WAIT_LOCK.clear_poison();
    }
    drop(runtime_slot);
    result
}

fn counters_checked() -> Result<DsvDiscoveryRuntimeCounters, String> {
    if COUNTER_OVERFLOWED.load(Ordering::Acquire) {
        return Err(
            "D3D11 DSV discovery runtime counter overflowed; refusing to seal report".into(),
        );
    }
    Ok(counters())
}

fn counters() -> DsvDiscoveryRuntimeCounters {
    DsvDiscoveryRuntimeCounters {
        queue_dropped_frames: QUEUE_DROPPED_FRAMES.load(Ordering::Acquire),
        queue_dropped_candidate_rows: QUEUE_DROPPED_CANDIDATE_ROWS.load(Ordering::Acquire),
        hook_dropped_events: HOOK_DROPPED_EVENTS.load(Ordering::Acquire),
        hook_integrity_failures: HOOK_INTEGRITY_FAILURES.load(Ordering::Acquire),
        hook_integrity_failure_reasons: Some(DsvDiscoveryHookIntegrityFailureReasons {
            unsupported_execute_command_list: HOOK_INTEGRITY_UNSUPPORTED_EXECUTE_COMMAND_LIST
                .load(Ordering::Acquire),
            vtable_or_slot_drift: HOOK_INTEGRITY_VTABLE_OR_SLOT_DRIFT.load(Ordering::Acquire),
            context_drift: HOOK_INTEGRITY_CONTEXT_DRIFT.load(Ordering::Acquire),
            tracker_failure: HOOK_INTEGRITY_TRACKER_FAILURE.load(Ordering::Acquire),
            lifecycle_or_queue_failure: HOOK_INTEGRITY_LIFECYCLE_OR_QUEUE_FAILURE
                .load(Ordering::Acquire),
        }),
        descriptor_identity_conflicts: DESCRIPTOR_IDENTITY_CONFLICTS.load(Ordering::Acquire),
        candidate_limit_events: CANDIDATE_LIMIT_EVENTS.load(Ordering::Acquire),
        pass_ledger_dropped_rows: PASS_LEDGER_DROPPED_ROWS.load(Ordering::Acquire),
    }
}

fn reset_counters() {
    QUEUE_DROPPED_FRAMES.store(0, Ordering::Release);
    QUEUE_DROPPED_CANDIDATE_ROWS.store(0, Ordering::Release);
    HOOK_DROPPED_EVENTS.store(0, Ordering::Release);
    HOOK_INTEGRITY_FAILURES.store(0, Ordering::Release);
    HOOK_INTEGRITY_UNSUPPORTED_EXECUTE_COMMAND_LIST.store(0, Ordering::Release);
    HOOK_INTEGRITY_VTABLE_OR_SLOT_DRIFT.store(0, Ordering::Release);
    HOOK_INTEGRITY_CONTEXT_DRIFT.store(0, Ordering::Release);
    HOOK_INTEGRITY_TRACKER_FAILURE.store(0, Ordering::Release);
    HOOK_INTEGRITY_LIFECYCLE_OR_QUEUE_FAILURE.store(0, Ordering::Release);
    COUNTER_OVERFLOWED.store(false, Ordering::Release);
    DESCRIPTOR_IDENTITY_CONFLICTS.store(0, Ordering::Release);
    CANDIDATE_LIMIT_EVENTS.store(0, Ordering::Release);
    PASS_LEDGER_DROPPED_ROWS.store(0, Ordering::Release);
}

#[cfg(test)]
#[derive(Clone)]
struct EntryBarrierHook {
    target_thread: std::thread::ThreadId,
    admitted: std::sync::Arc<std::sync::Barrier>,
    resume: std::sync::Arc<std::sync::Barrier>,
    stop_closed: std::sync::Arc<std::sync::Barrier>,
}

#[cfg(test)]
static TEST_ENTRY_BARRIER: Mutex<Option<EntryBarrierHook>> = Mutex::new(None);

#[cfg(test)]
fn current_test_entry_barrier() -> Option<EntryBarrierHook> {
    TEST_ENTRY_BARRIER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

#[cfg(test)]
fn pause_admitted_entry_for_test() {
    if let Some(hook) = current_test_entry_barrier() {
        hook.admitted.wait();
        hook.resume.wait();
    }
}

#[cfg(test)]
fn notify_closed_gate_for_test() {
    if let Some(hook) = current_test_entry_barrier() {
        hook.stop_closed.wait();
    }
}

#[cfg(test)]
#[derive(Clone)]
struct WriterSealBarrierHook {
    reached: std::sync::Arc<std::sync::Barrier>,
    resume: std::sync::Arc<std::sync::Barrier>,
}

#[cfg(test)]
static TEST_WRITER_SEAL_BARRIER: Mutex<Option<WriterSealBarrierHook>> = Mutex::new(None);

#[cfg(test)]
fn pause_writer_seal_for_test() {
    let hook = TEST_WRITER_SEAL_BARRIER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    if let Some(hook) = hook {
        hook.reached.wait();
        hook.resume.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_root() -> PathBuf {
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "dm-dsv-discovery-bridge-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir(&root).unwrap();
        root
    }

    fn test_frame(present_seq: u64) -> DsvDiscoveryFrameObservation {
        DsvDiscoveryFrameObservation {
            present_seq,
            candidates: vec![depth_lab::DsvDiscoveryCandidateObservation {
                candidate_id: "dsv-0001".into(),
                descriptor: depth_lab::DsvDiscoveryDescriptor {
                    view_format: 45,
                    view_dimension: 3,
                    view_flags: 0,
                    mip_slice: 0,
                    first_array_slice: 0,
                    view_array_size: 1,
                    resource_format: 44,
                    width: 1920,
                    height: 1080,
                    resource_mip_levels: 1,
                    resource_array_size: 1,
                    sample_count: 1,
                    sample_quality: 0,
                    usage: 0,
                    bind_flags: 0x40,
                    cpu_access_flags: 0,
                    misc_flags: 0,
                },
                bind_count: 1,
                unbind_count: 1,
                pass_count: 1,
                draw_calls: 10,
                clear_count: 1,
                completed_passes: vec![depth_lab::DsvDiscoveryCompletedPassObservation(
                    10, 1, false,
                )],
                dropped_completed_passes: 0,
                first_bound_order: Some(1),
                last_bound_order: Some(1),
                carried_from_previous_present: false,
            }],
            ambiguous_descriptor_groups: Vec::new(),
        }
    }

    #[test]
    fn inactive_bridge_only_exposes_zero_capacity() {
        let _serial = lock_test_runtime();
        assert!(!enabled());
        assert_eq!(max_candidates(), 0);
        record_hook_drop();
        record_integrity_failure(HookIntegrityFailureReason::TrackerFailure);
        record_descriptor_identity_conflict();
        record_candidate_limit();
        record_pass_ledger_drop();
        assert_eq!(
            counters(),
            DsvDiscoveryRuntimeCounters {
                hook_integrity_failure_reasons: Some(
                    DsvDiscoveryHookIntegrityFailureReasons::default(),
                ),
                ..DsvDiscoveryRuntimeCounters::default()
            }
        );
    }

    #[test]
    fn runtime_counter_overflow_poison_refuses_the_sealed_snapshot() {
        let _serial = lock_test_runtime();
        reset_counters();
        HOOK_INTEGRITY_FAILURES.store(u64::MAX, Ordering::Release);

        increment_integrity_failure_raw(HookIntegrityFailureReason::TrackerFailure);

        assert_eq!(HOOK_INTEGRITY_FAILURES.load(Ordering::Acquire), u64::MAX);
        assert_eq!(HOOK_INTEGRITY_TRACKER_FAILURE.load(Ordering::Acquire), 1);
        assert!(
            counters_checked()
                .unwrap_err()
                .contains("counter overflowed")
        );
        reset_counters();
    }

    #[test]
    fn active_bridge_seals_each_integrity_reason_into_the_exact_aggregate() {
        let _serial = lock_test_runtime();
        let root = test_root();
        let output = root.join("integrity-reasons");
        let executable_sha256 = format!("sha256:{}", "a".repeat(64));
        start(
            DsvDiscoveryConfig {
                schema: depth_lab::DSV_DISCOVERY_CONFIG_SCHEMA_V2.into(),
                discovery_id: "integrity-reasons".into(),
                executable_sha256: executable_sha256.clone(),
                evidence_root: root.display().to_string(),
                limits: depth_lab::DsvDiscoveryLimits::default(),
            },
            &output,
            &executable_sha256,
        )
        .unwrap();
        for reason in [
            HookIntegrityFailureReason::UnsupportedExecuteCommandList,
            HookIntegrityFailureReason::VtableOrSlotDrift,
            HookIntegrityFailureReason::ContextDrift,
            HookIntegrityFailureReason::TrackerFailure,
            HookIntegrityFailureReason::LifecycleOrQueueFailure,
        ] {
            record_integrity_failure(reason);
        }
        let report_path = stop().unwrap();
        let report = depth_lab::read_report_bounded(&report_path, &root).unwrap();
        assert_eq!(report.summary.runtime.hook_integrity_failures, 5);
        assert_eq!(
            report.summary.runtime.hook_integrity_failure_reasons,
            Some(DsvDiscoveryHookIntegrityFailureReasons {
                unsupported_execute_command_list: 1,
                vtable_or_slot_drift: 1,
                context_drift: 1,
                tracker_failure: 1,
                lifecycle_or_queue_failure: 1,
            })
        );
        report.validate().unwrap();
        reset_counters();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn activation_ack_is_create_new_and_never_claims_observation_completion() {
        let _serial = lock_test_runtime();
        let root = test_root();
        let output = root.join("activation_run");
        let executable_sha256 = format!("sha256:{}", "a".repeat(64));
        let config_sha256 = format!("sha256:{}", "b".repeat(64));
        let config = DsvDiscoveryConfig {
            schema: depth_lab::DSV_DISCOVERY_CONFIG_SCHEMA_V2.into(),
            discovery_id: "activation-test".into(),
            executable_sha256: executable_sha256.clone(),
            evidence_root: root.display().to_string(),
            limits: depth_lab::DsvDiscoveryLimits {
                max_frames: 2,
                max_candidates: 2,
                max_candidates_per_frame: 2,
            },
        };
        start(config.clone(), &output, &executable_sha256).unwrap();
        assert!(
            std::fs::rename(&output, root.join("activation_run_replaced")).is_err(),
            "the active runtime must pin the verified output directory against replacement"
        );
        assert!(
            write_activation_ack(
                &config,
                "dsv-discovery-activation-test",
                &output,
                &executable_sha256,
                "sha256:NOT-CANONICAL",
                true,
            )
            .is_err()
        );
        assert!(!output.join(DSV_DISCOVERY_ACTIVATION_ACK_FILE_NAME).exists());

        TEST_ACTIVATION_ACK_FAIL_BEFORE_COMMIT.store(true, Ordering::Release);
        assert!(
            write_activation_ack(
                &config,
                "dsv-discovery-activation-test",
                &output,
                &executable_sha256,
                &config_sha256,
                true,
            )
            .is_err()
        );
        assert!(!output.join(DSV_DISCOVERY_ACTIVATION_ACK_FILE_NAME).exists());
        assert!(std::fs::read_dir(&output).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".dm-dsv-activation-ack-pending-")
        }));

        let ack_path = write_activation_ack(
            &config,
            "dsv-discovery-activation-test",
            &output,
            &executable_sha256,
            &config_sha256,
            true,
        )
        .unwrap();
        let bytes = std::fs::read(&ack_path).unwrap();
        assert!(bytes.len() <= MAX_DSV_DISCOVERY_ACTIVATION_ACK_BYTES);
        let ack: DsvDiscoveryActivationAck = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(ack.schema, DSV_DISCOVERY_ACTIVATION_ACK_SCHEMA_V1);
        assert_eq!(ack.status, "probe_discovery_runtime_activated");
        assert_eq!(ack.discovery_id, config.discovery_id);
        assert_eq!(ack.session_id, "dsv-discovery-activation-test");
        assert_eq!(ack.process_id, std::process::id());
        assert!(ack.process_start_time_filetime.parse::<u64>().unwrap() > 0);
        assert_eq!(ack.executable_sha256, executable_sha256);
        assert_eq!(ack.probe_config_sha256, config_sha256);
        assert_eq!(ack.output_leaf, "activation_run");
        assert!(ack.runtime.writer_started);
        assert!(ack.runtime.present_hook_installed);
        assert!(ack.runtime.config_watcher_started);
        assert!(ack.runtime.discovery_runtime_enabled);
        assert!(!ack.runtime.discovery_complete);
        assert!(!ack.runtime.recording_ready);
        assert!(!ack.runtime.formal_capture_armed);
        assert!(!ack.runtime.selection_ready);
        assert!(!ack.runtime.binding_ready);
        assert!(
            write_activation_ack(
                &config,
                "dsv-discovery-activation-test",
                &output,
                &ack.executable_sha256,
                &ack.probe_config_sha256,
                true,
            )
            .is_err(),
            "activation ACK must never overwrite the first probe-authored identity"
        );
        assert!(
            !output
                .join(depth_lab::DSV_DISCOVERY_REPORT_FILE_NAME)
                .exists()
        );
        stop().unwrap();
        reset_counters();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn activation_ack_identity_and_commit_never_block_render_reads() {
        let _serial = lock_test_runtime();
        let root = test_root();
        let output = root.join("activation_commit_barrier_run");
        let executable_sha256 = format!("sha256:{}", "c".repeat(64));
        let config_sha256 = format!("sha256:{}", "d".repeat(64));
        let config = DsvDiscoveryConfig {
            schema: depth_lab::DSV_DISCOVERY_CONFIG_SCHEMA_V2.into(),
            discovery_id: "activation-commit-barrier-test".into(),
            executable_sha256: executable_sha256.clone(),
            evidence_root: root.display().to_string(),
            limits: depth_lab::DsvDiscoveryLimits {
                max_frames: 2,
                max_candidates: 2,
                max_candidates_per_frame: 2,
            },
        };
        start(config.clone(), &output, &executable_sha256).unwrap();

        let identity_hook = ActivationAckCommitBarrierHook {
            reached: Arc::new(std::sync::Barrier::new(2)),
            resume: Arc::new(std::sync::Barrier::new(2)),
        };
        *TEST_ACTIVATION_ACK_IDENTITY_BARRIER
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(identity_hook.clone());
        let commit_hook = ActivationAckCommitBarrierHook {
            reached: Arc::new(std::sync::Barrier::new(2)),
            resume: Arc::new(std::sync::Barrier::new(2)),
        };
        *TEST_ACTIVATION_ACK_COMMIT_BARRIER
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(commit_hook.clone());
        let ack_config = config.clone();
        let ack_output = output.clone();
        let ack_executable_sha256 = executable_sha256.clone();
        let ack_thread = std::thread::spawn(move || {
            write_activation_ack(
                &ack_config,
                "dsv-discovery-activation-commit-barrier-test",
                &ack_output,
                &ack_executable_sha256,
                &config_sha256,
                true,
            )
        });

        // The ACK holds an identity read guard here. Present must acquire a concurrent read guard,
        // enqueue the frame and preserve zero-drop authority.
        identity_hook.reached.wait();
        assert!(RUNTIME.try_read().is_ok());
        observe_frame(test_frame(1));
        assert_eq!(counters().queue_dropped_frames, 0);
        assert_eq!(counters().queue_dropped_candidate_rows, 0);
        identity_hook.resume.wait();

        // The potentially slow filesystem commit happens after the identity read guard is gone.
        commit_hook.reached.wait();
        assert!(RUNTIME.try_read().is_ok());
        observe_frame(test_frame(2));
        assert_eq!(counters().queue_dropped_frames, 0);
        assert_eq!(counters().queue_dropped_candidate_rows, 0);
        commit_hook.resume.wait();
        ack_thread.join().unwrap().unwrap();
        *TEST_ACTIVATION_ACK_IDENTITY_BARRIER
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        *TEST_ACTIVATION_ACK_COMMIT_BARRIER
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;

        let report_path = stop().unwrap();
        let report = depth_lab::read_report_bounded(&report_path, &root).unwrap();
        assert_eq!(report.summary.runtime.queue_dropped_frames, 0);
        assert_eq!(report.summary.runtime.queue_dropped_candidate_rows, 0);
        reset_counters();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn stop_waits_for_an_admitted_observer_before_sealing_runtime_counters() {
        let _serial = lock_test_runtime();
        let root = test_root();
        let executable_sha256 = format!("sha256:{}", "a".repeat(64));
        let (observer_start_tx, observer_start_rx) = mpsc::sync_channel(0);
        let (observer_id_tx, observer_id_rx) = mpsc::sync_channel(1);
        let observer = std::thread::spawn(move || {
            observer_id_tx.send(std::thread::current().id()).unwrap();
            observer_start_rx.recv().unwrap();
            observe_frame(test_frame(2));
        });
        let observer_thread = observer_id_rx.recv().unwrap();
        let hook = EntryBarrierHook {
            target_thread: observer_thread,
            admitted: std::sync::Arc::new(std::sync::Barrier::new(2)),
            resume: std::sync::Arc::new(std::sync::Barrier::new(2)),
            stop_closed: std::sync::Arc::new(std::sync::Barrier::new(2)),
        };
        *TEST_ENTRY_BARRIER
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(hook.clone());
        start(
            DsvDiscoveryConfig {
                schema: depth_lab::DSV_DISCOVERY_CONFIG_SCHEMA_V2.into(),
                discovery_id: "stop-barrier-test".into(),
                executable_sha256: executable_sha256.clone(),
                evidence_root: root.display().to_string(),
                limits: depth_lab::DsvDiscoveryLimits {
                    max_frames: 2,
                    max_candidates: 2,
                    max_candidates_per_frame: 2,
                },
            },
            &root.join("run"),
            &executable_sha256,
        )
        .unwrap();
        observer_start_tx.send(()).unwrap();
        hook.admitted.wait();
        let (stop_tx, stop_rx) = mpsc::sync_channel(1);
        let stopper = std::thread::spawn(move || {
            let _ = stop_tx.send(stop());
        });
        hook.stop_closed.wait();
        assert_eq!(stop_rx.try_recv(), Err(mpsc::TryRecvError::Empty));

        hook.resume.wait();
        observer.join().unwrap();
        let report_path = stop_rx.recv().unwrap().unwrap();
        stopper.join().unwrap();
        *TEST_ENTRY_BARRIER
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;

        let report = depth_lab::read_report_bounded(&report_path, &root).unwrap();
        assert_eq!(report.summary.runtime.queue_dropped_frames, 1);
        assert_eq!(report.summary.runtime.queue_dropped_candidate_rows, 1);
        assert!(
            report
                .blockers
                .contains(&depth_lab::DsvDiscoveryBlocker::QueueSaturated)
        );
        assert!(!report.complete);
        reset_counters();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn admitted_guard_records_failure_after_stop_closes_before_sealing() {
        let _serial = lock_test_runtime();
        let root = test_root();
        let executable_sha256 = format!("sha256:{}", "b".repeat(64));
        let (recorder_start_tx, recorder_start_rx) = mpsc::sync_channel(0);
        let (recorder_id_tx, recorder_id_rx) = mpsc::sync_channel(1);
        let recorder = std::thread::spawn(move || {
            recorder_id_tx.send(std::thread::current().id()).unwrap();
            recorder_start_rx.recv().unwrap();
            let entry = try_enter_observation().unwrap();
            entry.record_integrity_failure(HookIntegrityFailureReason::TrackerFailure);
        });
        let recorder_thread = recorder_id_rx.recv().unwrap();
        let hook = EntryBarrierHook {
            target_thread: recorder_thread,
            admitted: std::sync::Arc::new(std::sync::Barrier::new(2)),
            resume: std::sync::Arc::new(std::sync::Barrier::new(2)),
            stop_closed: std::sync::Arc::new(std::sync::Barrier::new(2)),
        };
        *TEST_ENTRY_BARRIER
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(hook.clone());
        start(
            DsvDiscoveryConfig {
                schema: depth_lab::DSV_DISCOVERY_CONFIG_SCHEMA_V2.into(),
                discovery_id: "stop-counter-barrier-test".into(),
                executable_sha256: executable_sha256.clone(),
                evidence_root: root.display().to_string(),
                limits: depth_lab::DsvDiscoveryLimits {
                    max_frames: 2,
                    max_candidates: 2,
                    max_candidates_per_frame: 2,
                },
            },
            &root.join("run"),
            &executable_sha256,
        )
        .unwrap();
        recorder_start_tx.send(()).unwrap();
        hook.admitted.wait();
        let (stop_tx, stop_rx) = mpsc::sync_channel(1);
        let stopper = std::thread::spawn(move || {
            let _ = stop_tx.send(stop());
        });
        hook.stop_closed.wait();
        assert_eq!(stop_rx.try_recv(), Err(mpsc::TryRecvError::Empty));

        hook.resume.wait();
        recorder.join().unwrap();
        let report_path = stop_rx.recv().unwrap().unwrap();
        stopper.join().unwrap();
        *TEST_ENTRY_BARRIER
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;

        let report = depth_lab::read_report_bounded(&report_path, &root).unwrap();
        assert_eq!(report.summary.runtime.hook_integrity_failures, 1);
        assert_eq!(
            report
                .summary
                .runtime
                .hook_integrity_failure_reasons
                .as_ref()
                .unwrap()
                .tracker_failure,
            1
        );
        assert!(
            report
                .blockers
                .contains(&depth_lab::DsvDiscoveryBlocker::HookIntegrityFailure)
        );
        assert!(!report.complete);
        reset_counters();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn start_cannot_reset_counters_while_the_previous_writer_is_sealing() {
        let _serial = lock_test_runtime();
        let root = test_root();
        let old_executable_sha256 = format!("sha256:{}", "c".repeat(64));
        start(
            DsvDiscoveryConfig {
                schema: depth_lab::DSV_DISCOVERY_CONFIG_SCHEMA_V2.into(),
                discovery_id: "old-sealing-generation".into(),
                executable_sha256: old_executable_sha256.clone(),
                evidence_root: root.display().to_string(),
                limits: depth_lab::DsvDiscoveryLimits {
                    max_frames: 2,
                    max_candidates: 2,
                    max_candidates_per_frame: 2,
                },
            },
            &root.join("old_run"),
            &old_executable_sha256,
        )
        .unwrap();
        observe_frame(test_frame(1));
        record_hook_drop();

        let seal_hook = WriterSealBarrierHook {
            reached: std::sync::Arc::new(std::sync::Barrier::new(2)),
            resume: std::sync::Arc::new(std::sync::Barrier::new(2)),
        };
        *TEST_WRITER_SEAL_BARRIER
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(seal_hook.clone());
        let (stop_tx, stop_rx) = mpsc::sync_channel(1);
        let stopper = std::thread::spawn(move || {
            let _ = stop_tx.send(stop());
        });
        seal_hook.reached.wait();
        assert!(matches!(
            RUNTIME.try_write(),
            Err(std::sync::TryLockError::WouldBlock)
        ));

        let new_executable_sha256 = format!("sha256:{}", "d".repeat(64));
        let new_root = root.clone();
        let (start_tx, start_rx) = mpsc::sync_channel(1);
        let starter = std::thread::spawn(move || {
            let result = start(
                DsvDiscoveryConfig {
                    schema: depth_lab::DSV_DISCOVERY_CONFIG_SCHEMA_V2.into(),
                    discovery_id: "new-generation".into(),
                    executable_sha256: new_executable_sha256.clone(),
                    evidence_root: new_root.display().to_string(),
                    limits: depth_lab::DsvDiscoveryLimits {
                        max_frames: 1,
                        max_candidates: 1,
                        max_candidates_per_frame: 1,
                    },
                },
                &new_root.join("new_run"),
                &new_executable_sha256,
            );
            let _ = start_tx.send(result);
        });
        assert!(matches!(
            start_rx.recv_timeout(std::time::Duration::from_millis(50)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));

        seal_hook.resume.wait();
        let old_report_path = stop_rx.recv().unwrap().unwrap();
        stopper.join().unwrap();
        start_rx.recv().unwrap().unwrap();
        starter.join().unwrap();
        *TEST_WRITER_SEAL_BARRIER
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;

        let old_report = depth_lab::read_report_bounded(&old_report_path, &root).unwrap();
        assert_eq!(old_report.summary.runtime.hook_dropped_events, 1);
        stop().unwrap();
        reset_counters();
        let _ = std::fs::remove_dir_all(root);
    }
}

//! Exact-build CP2077 pose lifecycle wiring.
//!
//! This path is opt-in through the early launcher environment. It never participates in capture
//! Ready. The raw-demo route installs one exact-instance shadow-vtable observer for the audited
//! first-party player event and emits only a bounded, redacted create-new report.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::c_void;
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicPtr, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::Deserialize;
use windows::Win32::System::Diagnostics::Debug::ReadProcessMemory;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{
    MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_EXECUTE_READ, PAGE_EXECUTE_READWRITE,
    PAGE_EXECUTE_WRITECOPY, PAGE_GUARD, PAGE_NOACCESS, PAGE_READONLY, PAGE_READWRITE,
    PAGE_WRITECOPY, VirtualQuery,
};
use windows::Win32::System::ProcessStatus::{K32GetModuleInformation, MODULEINFO};
use windows::Win32::System::Threading::GetCurrentProcess;

use crate::cp2077_pose_abi::{CP2077_PLAYER_ABI_EVIDENCE_ID, CP2077_PLAYER_VCALL_SLOT_INDEX};
use crate::cp2077_pose_probe::{CP2077_EXE_SHA256, DiscoveryScenario, PoseRole};
use crate::cp2077_pose_runtime::{
    CP2077_CAMERA_SYSTEM_CNAME_HASH, CP2077_CAMERA_SYSTEM_TYPE_BYTES, CP2077_CGAME_ENGINE_SYMBOL,
    CP2077_PLAYER_SPAWN_SYMBOL, CP2077_PLAYER_SYSTEM_CNAME_HASH, CP2077_PLAYER_SYSTEM_TYPE_BYTES,
    Cp2077PoseRuntimeSession, HookAbiEvidence, MAX_OBJECT_SNAPSHOT_BYTES,
    RenderPresentDiscoveryInput, ResolverObservation, RuntimeResolverSource,
    RuntimeSystemCandidateObservation, RuntimeSystemEnumerationObservation,
    SnapshotObservationInput, bootstrap_current_process, scan_world_transform_candidates,
    write_report_create_new,
};

pub const DISCOVERY_CONFIG_SCHEMA_V1: &str = "dm-cp2077-discovery-smoke-config/v1";
pub const DISCOVERY_REPORT_FILE: &str = "cp2077-pose-runtime-observation.json";
pub const DISCOVERY_STOP_REQUEST_FILE: &str = "discovery-stop-request.json";
const PRESENT_QUEUE_CAPACITY: usize = 512;
const POSE_INGRESS_HZ: i64 = 5;
const SYSTEM_ARRAY_OFFSET: usize = 0x38;
const SYSTEM_ENTRY_BYTES: usize = 0x10;
const CGAME_ENGINE_FRAMEWORK_OFFSET: usize = 0x308;
const FRAMEWORK_GAME_INSTANCE_OFFSET: usize = 0x10;
const MAX_DECLARED_SYSTEM_CAPACITY: u32 = 4_096;
const SCRIPTABLE_NATIVE_TYPE_OFFSET: usize = 0x30;
const CCLASS_NAME_OFFSET: usize = 0x18;
const CCLASS_SIZE_OFFSET: usize = 0x68;
const MAX_PLAUSIBLE_REFERENCE_COUNT: u32 = 1_000_000;
const DISCOVERY_OBJECT_SNAPSHOT_INTERVAL: Duration = Duration::from_secs(1);
const RAW_DEMO_OBJECT_SNAPSHOT_INTERVAL: Duration = Duration::from_millis(100);
const SYSTEM_ENUMERATION_RETRY_INTERVAL: Duration = Duration::from_millis(100);
// The cached raw-demo player chain is revalidated (exact cpPlayerSystem RTTI,
// player/component identity, owner backlink, and WorldTransform structure) on
// every sample. A failed validation clears the cache and forces an immediate
// full system enumeration below. Keep the defensive periodic enumeration well
// outside a 30/64 second capture window: on this build a full enumeration can
// take several seconds and would otherwise create deterministic pose gaps.
const RAW_DEMO_SYSTEM_REFRESH_INTERVAL: Duration = Duration::from_secs(60 * 60);
const PLAYER_WORLD_TRANSFORM_RESOLVER_ID: &str =
    "rtti-cp-player-system-player-puppet-world-transform-v1";
const PLAYER_EVENT_WORLD_TRANSFORM_RESOLVER_ID: &str = "player-spawn-slot63-world-transform-v1";
const PLAYER_EVENT_MIN_VTABLE_ENTRIES: usize = CP2077_PLAYER_VCALL_SLOT_INDEX as usize + 1;
const PLAYER_EVENT_MAX_VTABLE_ENTRIES: usize = 512;
const CAMERA_WORLD_TRANSFORM_RESOLVER_ID: &str =
    "rtti-game-camera-system-camera-component-world-transform-v1";
const PLAYER_PUPPET_CNAME_HASH: u64 = 0xcebe_cae8_98e5_5b86;
// RED4ext's generated 2.31 hierarchy exposes the native base as `gamePuppet`; there is no
// `gamePlayerPuppet` native class. Scripted PlayerPuppet instances may therefore surface either
// their scripted leaf or this native base through IScriptable.nativeType.
const GAME_PUPPET_CNAME_HASH: u64 = 0xd21f_8121_e9ea_a971;
// RED4ext.SDK 2.31 layout evidence (ad727771): ent::Entity::transformComponent is a raw
// ent::IPlacedComponent* at 0xB0. IComponent::owner at 0x50 gives a read-only back-reference
// that lets us reject unrelated/stale pointers without calling engine or RTTI functions.
const ENT_ENTITY_TRANSFORM_COMPONENT_OFFSET: usize = 0xb0;
const ENT_COMPONENT_OWNER_OFFSET: usize = 0x50;
const ENT_PLACED_COMPONENT_WORLD_TRANSFORM_OFFSET: usize = 0xe0;
const ENT_PLACED_COMPONENT_CNAME_HASH: u64 = 0xbaa1_bc41_bff7_e993;
const ENT_BASE_CAMERA_COMPONENT_CNAME_HASH: u64 = 0x9e74_ea04_0df2_bb47;
const GAME_CAMERA_COMPONENT_CNAME_HASH: u64 = 0xe0e1_a8ad_1994_6bd7;
const WORLD_TRANSFORM_BYTES: usize = 0x20;
const MAX_RTTI_PARENT_DEPTH: usize = 32;
// 一次游走里同时记住多少个 region。系统枚举会在「游戏镜像、RTTI 描述符堆、对象堆」这
// 几段之间来回跳，条数少了会互相挤掉；这是纯栈上数组，16 条足够覆盖实测到的段数。
const REGION_MEMO_ENTRIES: usize = 16;
const POINTER_SCAN_PAGE_BYTES: usize = 4096;
const MAX_X64_USER_POINTER: usize = 0x0000_7fff_ffff_ffff;

#[derive(Debug, Clone, Copy)]
struct SystemSnapshotTarget {
    object: usize,
    role: PoseRole,
    system_index: u32,
    type_bytes: u32,
}

struct SystemEnumerationResult {
    observation: RuntimeSystemEnumerationObservation,
    snapshot_targets: Vec<SystemSnapshotTarget>,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct SnapshotAttemptOutcome {
    any_observed: bool,
    player_observed: bool,
}

fn unique_role_target(targets: &[SystemSnapshotTarget], role: PoseRole) -> bool {
    targets.iter().filter(|target| target.role == role).count() == 1
}

fn newer_present_identity(previous: Option<(u64, i64)>, current: (u64, i64)) -> bool {
    current.0 != 0
        && current.1 > 0
        && previous.is_none_or(|previous| current.0 > previous.0 && current.1 > previous.1)
}

/// RED4ext `DynArray<T>` is `{ entries, capacity, size }`, not
/// `{ entries, size, capacity }`. Reading the two counters in one bounded copy also avoids
/// manufacturing an impossible pair from separate reads while the game is initializing.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RedengineDynArrayHeader {
    entries: usize,
    capacity: u32,
    size: u32,
}

fn admit_system_array_header(header: RedengineDynArrayHeader) -> Option<(usize, u32, u32)> {
    (header.entries != 0
        && header.size <= header.capacity
        && header.capacity <= MAX_DECLARED_SYSTEM_CAPACITY)
        .then_some((header.entries, header.size, header.capacity))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DiscoveryConfig {
    schema: String,
    output_dir: String,
    recording_ready: bool,
}

#[derive(Debug, Clone, Copy)]
struct PresentInput {
    present_seq: u64,
    qpc: i64,
    resource_epoch: u64,
    queue_count: u32,
    command_list_count: u32,
    depth_resource_count: u32,
    eligible_pre_clear_snapshot_count: u64,
    capture_safe: bool,
    physical_view_matrix_source_observed: bool,
    physical_projection_matrix_source_observed: bool,
    camera_candidate_admitted: bool,
    render_observation: bool,
    player_snapshot: Option<PlayerPresentSnapshot>,
}

#[derive(Debug, Clone, Copy)]
struct PlayerPresentSnapshot {
    player_object: usize,
    transform_component: usize,
    bytes: [u8; WORLD_TRANSFORM_BYTES],
}

struct DiscoveryIngress {
    sender: SyncSender<PresentInput>,
    dropped_presents: Arc<AtomicU64>,
    latest_present: Arc<LatestPresent>,
    last_enqueued_qpc: AtomicI64,
    qpc_frequency: i64,
}

#[derive(Default)]
struct WorkerCompletion {
    result: Mutex<Option<Result<(), String>>>,
    changed: Condvar,
}

impl WorkerCompletion {
    fn publish(&self, result: Result<(), String>) {
        if let Ok(mut slot) = self.result.lock() {
            *slot = Some(result);
            self.changed.notify_all();
        }
    }
}

enum WorkerStop {
    DiscoveryFile(PathBuf),
    RawDemoSignal(Arc<AtomicBool>),
}

struct PoseWorkerOutput {
    report_path: PathBuf,
    stop: WorkerStop,
    game_root: PathBuf,
    completion: Option<Arc<WorkerCompletion>>,
}

impl WorkerStop {
    fn requested(&self) -> bool {
        match self {
            Self::DiscoveryFile(path) => path.is_file(),
            Self::RawDemoSignal(requested) => requested.load(Ordering::Acquire),
        }
    }

    fn object_snapshot_interval(&self) -> Duration {
        match self {
            Self::DiscoveryFile(_) => DISCOVERY_OBJECT_SNAPSHOT_INTERVAL,
            Self::RawDemoSignal(_) => RAW_DEMO_OBJECT_SNAPSHOT_INTERVAL,
        }
    }

    fn system_refresh_interval(&self, has_unique_player_target: bool) -> Duration {
        match self {
            Self::DiscoveryFile(_) => SYSTEM_ENUMERATION_RETRY_INTERVAL,
            Self::RawDemoSignal(_) if has_unique_player_target => RAW_DEMO_SYSTEM_REFRESH_INTERVAL,
            Self::RawDemoSignal(_) => SYSTEM_ENUMERATION_RETRY_INTERVAL,
        }
    }

    fn observe_legacy_camera_object(&self) -> bool {
        matches!(self, Self::DiscoveryFile(_))
    }

    fn install_player_event_hook(&self) -> bool {
        matches!(self, Self::RawDemoSignal(_))
    }
}

pub struct RawDemoPoseWorkerHandle {
    launch_id: String,
    output_dir: PathBuf,
    stop_requested: Arc<AtomicBool>,
    completion: Arc<WorkerCompletion>,
}

impl RawDemoPoseWorkerHandle {
    pub fn stop_and_seal(&self, launch_id: &str, output_dir: &Path) -> Result<(), String> {
        validate_raw_demo_stop_binding(&self.launch_id, &self.output_dir, launch_id, output_dir)?;
        self.stop_requested.store(true, Ordering::Release);
        let result = self
            .completion
            .result
            .lock()
            .map_err(|_| "CP2077 raw-demo pose completion lock poisoned".to_string())?;
        let (result, timeout) = self
            .completion
            .changed
            .wait_timeout_while(result, Duration::from_secs(15), |slot| slot.is_none())
            .map_err(|_| "CP2077 raw-demo pose completion wait poisoned".to_string())?;
        if timeout.timed_out() && result.is_none() {
            return Err("CP2077 raw-demo pose report did not seal within fifteen seconds".into());
        }
        result
            .as_ref()
            .cloned()
            .ok_or_else(|| "CP2077 raw-demo pose worker completed without a result".to_string())?
    }
}

#[derive(Debug, Default)]
struct LatestPresent {
    version: AtomicU64,
    present_seq: AtomicU64,
    qpc: AtomicI64,
}

impl LatestPresent {
    fn store(&self, present_seq: u64, qpc: i64) {
        self.version.fetch_add(1, Ordering::AcqRel);
        self.qpc.store(qpc, Ordering::Relaxed);
        self.present_seq.store(present_seq, Ordering::Relaxed);
        self.version.fetch_add(1, Ordering::Release);
    }

    fn load_consistent(&self) -> Option<(u64, i64)> {
        for _ in 0..3 {
            let before = self.version.load(Ordering::Acquire);
            if before == 0 || !before.is_multiple_of(2) {
                continue;
            }
            let present_seq = self.present_seq.load(Ordering::Relaxed);
            let qpc = self.qpc.load(Ordering::Relaxed);
            let after = self.version.load(Ordering::Acquire);
            if before == after && present_seq != 0 && qpc > 0 {
                return Some((present_seq, qpc));
            }
        }
        None
    }
}

static DISCOVERY_INGRESS: OnceLock<DiscoveryIngress> = OnceLock::new();
static PLAYER_EVENT_ORIGINAL: AtomicUsize = AtomicUsize::new(0);
static PLAYER_EVENT_OWNER: AtomicUsize = AtomicUsize::new(0);
static PLAYER_EVENT_OBJECT: AtomicUsize = AtomicUsize::new(0);
static PLAYER_EVENT_GENERATION: AtomicU64 = AtomicU64::new(0);
static VALIDATED_PLAYER_OBJECT: AtomicUsize = AtomicUsize::new(0);
static VALIDATED_PLAYER_COMPONENT: AtomicUsize = AtomicUsize::new(0);
static PLAYER_EVENT_SHADOW: OnceLock<CppVtableShadow> = OnceLock::new();

/// A shadow for an MSVC C++ object vftable. Unlike the COM-only helper, this preserves the
/// CompleteObjectLocator pointer at `vftable[-1]`; omitting that prefix makes RTTI/dynamic_cast
/// read outside the allocation and can crash the game.
struct CppVtableShadow {
    object_vtable: *mut *mut usize,
    original_vtable: *mut usize,
    /// `[CompleteObjectLocator, function_0, function_1, ...]`.
    shadow: Box<[usize]>,
}

impl CppVtableShadow {
    fn capture_and_activate(object: usize, entry_count: usize) -> Result<Self, String> {
        if object == 0 || entry_count < PLAYER_EVENT_MIN_VTABLE_ENTRIES {
            return Err("C++ shadow vtable owner/entry count is invalid".into());
        }
        let object_vtable = object as *mut *mut usize;
        if !(object_vtable as usize).is_multiple_of(std::mem::align_of::<AtomicPtr<usize>>()) {
            return Err("C++ shadow vtable owner pointer is not atomically aligned".into());
        }
        // 影子 vtable 的捕获是一次有界游走：整张表和 CompleteObjectLocator 基本都落在
        // 游戏镜像的同一段里，共用一次 region 裁定。
        let mut memo = RegionMemo::new();
        let original_vtable = memo.value::<usize>(object)? as *mut usize;
        let locator_address = (original_vtable as usize)
            .checked_sub(size_of::<usize>())
            .ok_or("C++ vtable CompleteObjectLocator address underflow")?;
        let locator = memo.value::<usize>(locator_address)?;
        let mut entries = Vec::with_capacity(entry_count + 1);
        entries.push(locator);
        for index in 0..entry_count {
            entries.push(memo.value::<usize>(add_address(
                original_vtable as usize,
                index * size_of::<usize>(),
            )?)?);
        }
        let value = Self {
            object_vtable,
            original_vtable,
            shadow: entries.into_boxed_slice(),
        };
        let function_table = unsafe { value.shadow.as_ptr().add(1).cast_mut() };
        match unsafe { AtomicPtr::from_ptr(object_vtable) }.compare_exchange(
            original_vtable,
            function_table,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => Ok(value),
            Err(current) => Err(format!(
                "refuse to replace unknown C++ object vtable: original={:#x} current={:#x}",
                original_vtable as usize, current as usize
            )),
        }
    }

    fn rearm_slot_if_restored(
        &self,
        index: usize,
        original: usize,
        replacement: usize,
    ) -> Result<(), String> {
        if index + 1 >= self.shadow.len() {
            return Err("C++ shadow vtable slot is outside the complete table".into());
        }
        let slot = unsafe { self.shadow.as_ptr().add(index + 1).cast_mut() };
        let atomic = unsafe { AtomicUsize::from_ptr(slot) };
        match atomic.compare_exchange(original, replacement, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => Ok(()),
            Err(current) if current == replacement => Ok(()),
            Err(current) => Err(format!(
                "refuse to replace unknown C++ shadow slot[{index}]: original={original:#x} replacement={replacement:#x} current={current:#x}"
            )),
        }
    }
}

impl Drop for CppVtableShadow {
    fn drop(&mut self) {
        let function_table = unsafe { self.shadow.as_ptr().add(1).cast_mut() };
        let _ = unsafe { AtomicPtr::from_ptr(self.object_vtable) }.compare_exchange(
            function_table,
            self.original_vtable,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }
}

// SAFETY: the exact cpPlayerSystem object and its original table are process-lifetime objects;
// installation is single-threaded and all later owner/slot access uses pointer-width atomics.
unsafe impl Send for CppVtableShadow {}
unsafe impl Sync for CppVtableShadow {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveryActivation {
    pub enabled: bool,
    pub exact_cp2077: bool,
    pub early_d3d12: bool,
    pub launch_id_valid: bool,
}

#[must_use]
pub fn classify_activation(
    discovery_config_present: bool,
    executable_sha256: &str,
    early_d3d12: bool,
    launch_id: Option<&str>,
) -> DiscoveryActivation {
    DiscoveryActivation {
        enabled: discovery_config_present,
        exact_cp2077: executable_sha256 == CP2077_EXE_SHA256,
        early_d3d12,
        launch_id_valid: launch_id.is_some_and(valid_launch_id),
    }
}

fn valid_launch_id(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn present_enqueue_due(previous_qpc: i64, current_qpc: i64, qpc_frequency: i64) -> bool {
    let minimum_interval = (qpc_frequency / POSE_INGRESS_HZ).max(1);
    previous_qpc <= 0 || current_qpc.saturating_sub(previous_qpc) >= minimum_interval
}

#[cfg(test)]
fn exact_present_render_camera_fields(
    snapshot: &crate::d3d12_render_camera::D3d12RenderCameraSnapshot,
    present_seq: u64,
) -> (bool, bool, bool) {
    let mut view = false;
    let mut projection = false;
    for observation in snapshot.observations.iter().filter(|observation| {
        observation.present_seq == present_seq
            && observation.copy_before_clear_submitted
            && observation.selected_present_identity_passed
    }) {
        view |= observation
            .matrices
            .iter()
            .any(|matrix| matrix.matrix_kind == "view");
        projection |= observation
            .matrices
            .iter()
            .any(|matrix| matrix.matrix_kind == "projection");
    }
    (
        view,
        projection,
        view && projection && snapshot.candidate_admitted_for_cross_launch_discovery,
    )
}

#[must_use]
pub fn discovery_requested() -> bool {
    std::env::var_os("DM_DEPTH_DISCOVERY_CONFIG").is_some()
}

fn validate_discovery_config(config: &DiscoveryConfig) -> Result<(), String> {
    if config.schema != DISCOVERY_CONFIG_SCHEMA_V1
        || config.output_dir != "."
        || config.recording_ready
    {
        return Err("CP2077 discovery config is not the discovery-only v1 contract".into());
    }
    Ok(())
}

fn validate_raw_demo_pose_paths(
    config_path: &Path,
    output_dir: &Path,
    stop_path: &Path,
) -> Result<(), String> {
    if !config_path.is_absolute() || !output_dir.is_absolute() || !stop_path.is_absolute() {
        return Err("CP2077 raw-demo pose paths must be absolute".into());
    }
    if config_path.parent() != Some(output_dir) {
        return Err(
            "CP2077 raw-demo pose config is not an immediate child of its output directory".into(),
        );
    }
    if stop_path != output_dir.join(crate::raw_depth_demo::STOP_FILE) {
        return Err("CP2077 raw-demo pose stop path is not the exact raw-demo stop request".into());
    }
    Ok(())
}

fn validate_raw_demo_stop_binding(
    expected_launch_id: &str,
    expected_output_dir: &Path,
    launch_id: &str,
    output_dir: &Path,
) -> Result<(), String> {
    if launch_id != expected_launch_id {
        return Err("CP2077 raw-demo pose stop targets a different process launch".into());
    }
    if output_dir != expected_output_dir {
        return Err("CP2077 raw-demo pose stop targets a different output directory".into());
    }
    Ok(())
}

fn current_process_pose_inputs(
    executable_sha256: &str,
    launch_id: String,
    output_dir: &Path,
) -> Result<(Cp2077PoseRuntimeSession, PathBuf), String> {
    let executable = std::env::current_exe()
        .map_err(|error| format!("resolve CP2077 executable failed: {error}"))?;
    let executable = executable
        .canonicalize()
        .map_err(|error| format!("canonicalize CP2077 executable failed: {error}"))?;
    let binary_dir = executable
        .parent()
        .ok_or("CP2077 executable has no binary directory")?;
    let catalog_path = binary_dir.join("cyberpunk2077_addresses.json");
    let game_root = binary_dir
        .parent()
        .and_then(Path::parent)
        .ok_or("CP2077 executable is not under bin/x64")?
        .to_path_buf();
    if path_is_within_case_insensitive(output_dir, &game_root) {
        return Err("CP2077 pose output directory must be outside the game tree".into());
    }
    let session = bootstrap_current_process(executable_sha256, launch_id, &catalog_path)?;
    Ok((session, game_root))
}

fn start_pose_worker(
    session: Cp2077PoseRuntimeSession,
    report_path: PathBuf,
    stop: WorkerStop,
    game_root: PathBuf,
    completion: Option<Arc<WorkerCompletion>>,
    thread_name: &str,
) -> Result<(), String> {
    if DISCOVERY_INGRESS.get().is_some() {
        return Err("CP2077 pose worker was already initialized".into());
    }
    let frequency = qpc_frequency();
    if frequency <= 0 {
        return Err("CP2077 pose worker could not query QPC frequency".into());
    }
    let (sender, receiver) = sync_channel(PRESENT_QUEUE_CAPACITY);
    let dropped_presents = Arc::new(AtomicU64::new(0));
    let latest_present = Arc::new(LatestPresent::default());
    let worker_drops = Arc::clone(&dropped_presents);
    let worker_latest_present = Arc::clone(&latest_present);
    std::thread::Builder::new()
        .name(thread_name.into())
        .spawn(move || {
            pose_worker(
                receiver,
                session,
                worker_drops,
                worker_latest_present,
                PoseWorkerOutput {
                    report_path,
                    stop,
                    game_root,
                    completion,
                },
            );
        })
        .map_err(|error| format!("start CP2077 pose worker failed: {error}"))?;
    DISCOVERY_INGRESS
        .set(DiscoveryIngress {
            sender,
            dropped_presents,
            latest_present,
            last_enqueued_qpc: AtomicI64::new(0),
            qpc_frequency: frequency,
        })
        .map_err(|_| "CP2077 pose worker was already initialized".to_string())
}

/// Starts the discovery worker only for the explicit exact-build early-launch contract.
pub fn try_start_from_environment(
    executable_sha256: &str,
    early_d3d12: bool,
) -> Result<bool, String> {
    let Some(config_path) = std::env::var_os("DM_DEPTH_DISCOVERY_CONFIG") else {
        return Ok(false);
    };
    let launch_id = std::env::var("DM_DEPTH_LAUNCH_ID")
        .map_err(|_| "CP2077 pose discovery requires DM_DEPTH_LAUNCH_ID".to_string())?;
    let activation = classify_activation(true, executable_sha256, early_d3d12, Some(&launch_id));
    if !activation.exact_cp2077 {
        return Ok(false);
    }
    if !activation.early_d3d12 {
        return Err("CP2077 pose discovery requires the early D3D12 launcher path".into());
    }
    if !activation.launch_id_valid {
        return Err("CP2077 pose discovery launch id is not 32 lowercase hex digits".into());
    }
    if DISCOVERY_INGRESS.get().is_some() {
        return Err("CP2077 pose discovery was already initialized".into());
    }
    let config_path = PathBuf::from(config_path);
    if !config_path.is_absolute() {
        return Err("CP2077 pose discovery config path must be absolute".into());
    }
    let config_path = config_path
        .canonicalize()
        .map_err(|error| format!("resolve CP2077 discovery config failed: {error}"))?;
    let config: DiscoveryConfig = serde_json::from_slice(
        &std::fs::read(&config_path)
            .map_err(|error| format!("read CP2077 discovery config failed: {error}"))?,
    )
    .map_err(|error| format!("parse CP2077 discovery config failed: {error}"))?;
    validate_discovery_config(&config)?;
    let output_dir = config_path
        .parent()
        .ok_or("CP2077 discovery config has no parent")?
        .canonicalize()
        .map_err(|error| format!("resolve CP2077 discovery output directory failed: {error}"))?;
    let report_path = output_dir.join(DISCOVERY_REPORT_FILE);
    let stop_path = output_dir.join(DISCOVERY_STOP_REQUEST_FILE);
    if report_path.exists() || stop_path.exists() {
        return Err("CP2077 discovery output contains a stale report or stop request".into());
    }

    let (session, game_root) =
        current_process_pose_inputs(executable_sha256, launch_id, &output_dir)?;
    start_pose_worker(
        session,
        report_path,
        WorkerStop::DiscoveryFile(stop_path),
        game_root,
        None,
        "dm-cp2077-pose-discovery",
    )?;
    Ok(true)
}

/// Starts the same read-only exact-build pose worker inside a raw-demo session. This path receives
/// every identity and path from the already validated raw config; it never reads the discovery env
/// and never enables formal capture control.
pub fn start_for_raw_demo(
    executable_sha256: &str,
    launch_id: &str,
    config_path: &Path,
    output_dir: &Path,
    stop_path: &Path,
) -> Result<RawDemoPoseWorkerHandle, String> {
    if executable_sha256 != CP2077_EXE_SHA256 {
        return Err("CP2077 raw-demo pose worker requires the exact 2.31 executable".into());
    }
    if !valid_launch_id(launch_id) {
        return Err("CP2077 raw-demo pose launch id is not 32 lowercase hex digits".into());
    }
    validate_raw_demo_pose_paths(config_path, output_dir, stop_path)?;
    let report_path = output_dir.join(DISCOVERY_REPORT_FILE);
    if report_path.exists() {
        return Err("CP2077 raw-demo output contains a stale pose report".into());
    }
    let (session, game_root) =
        current_process_pose_inputs(executable_sha256, launch_id.to_string(), output_dir)?;
    let stop_requested = Arc::new(AtomicBool::new(false));
    let completion = Arc::new(WorkerCompletion::default());
    start_pose_worker(
        session,
        report_path,
        WorkerStop::RawDemoSignal(Arc::clone(&stop_requested)),
        game_root,
        Some(Arc::clone(&completion)),
        "dm-cp2077-pose-raw-demo",
    )?;
    Ok(RawDemoPoseWorkerHandle {
        launch_id: launch_id.into(),
        output_dir: output_dir.to_path_buf(),
        stop_requested,
        completion,
    })
}

/// Non-blocking Present ingress. It is a no-op unless discovery or raw-demo pose is active.
/// `canonical_present_qpc` is supplied by the raw-depth owner so RGB, Depth and player pose use
/// one exact timestamp rather than three nearby QueryPerformanceCounter calls.
pub fn observe_present(
    present_seq: u64,
    canonical_present_qpc: Option<i64>,
    backend: &crate::d3d12_backend::D3d12ObservationBackend,
) {
    let Some(ingress) = DISCOVERY_INGRESS.get() else {
        return;
    };
    let qpc = canonical_present_qpc.unwrap_or_else(current_qpc);
    if qpc <= 0 {
        return;
    }
    ingress.latest_present.store(present_seq, qpc);
    let player_snapshot = read_same_present_player_snapshot();
    // Pose discovery only needs a bounded sample of the selected-Present
    // timeline.  CP2077 can call Present hundreds of times per second; feeding
    // every callback creates a false queue-drop blocker without adding pose or
    // matrix evidence. Keep the latest seqlock exact, but enqueue at a worker-safe rate using the
    // same QPC clock. Every admitted item can trigger synchronous system enumeration, so 60 Hz
    // can overwhelm the consumer even though object snapshots themselves are one hertz.
    let previous = ingress.last_enqueued_qpc.load(Ordering::Acquire);
    let render_observation = present_enqueue_due(previous, qpc, ingress.qpc_frequency);
    if !render_observation && player_snapshot.is_none() {
        return;
    }
    let admitted_render = render_observation
        && ingress
            .last_enqueued_qpc
            .compare_exchange(previous, qpc, Ordering::AcqRel, Ordering::Acquire)
            .is_ok();
    let (snapshot, render_camera) = if admitted_render {
        (
            Some(backend.snapshot()),
            Some(backend.render_camera_present_fields(present_seq)),
        )
    } else {
        (None, None)
    };
    let (
        physical_view_matrix_source_observed,
        physical_projection_matrix_source_observed,
        camera_candidate_admitted,
    ) = render_camera.unwrap_or((false, false, false));
    let input = PresentInput {
        present_seq,
        qpc,
        resource_epoch: snapshot.as_ref().map_or(0, |value| value.resource_epoch),
        queue_count: snapshot
            .as_ref()
            .map_or(0, |value| usize_to_u32(value.queue_count)),
        command_list_count: snapshot
            .as_ref()
            .map_or(0, |value| usize_to_u32(value.command_list_count)),
        depth_resource_count: snapshot
            .as_ref()
            .map_or(0, |value| usize_to_u32(value.depth_resource_count)),
        eligible_pre_clear_snapshot_count: snapshot
            .as_ref()
            .map_or(0, |value| value.eligible_pre_clear_snapshots),
        // The raw lane already pins one DSV. Other observed queues/resources do not
        // decide the safety of that selected capture; use its actual backend gate.
        capture_safe: if crate::raw_depth_demo::active() {
            admitted_render && backend.selected_capture_safe()
        } else {
            snapshot.as_ref().is_some_and(|value| value.capture_safe)
        },
        physical_view_matrix_source_observed,
        physical_projection_matrix_source_observed,
        camera_candidate_admitted,
        render_observation: admitted_render,
        player_snapshot,
    };
    match ingress.sender.try_send(input) {
        Ok(()) => {}
        Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
            ingress.dropped_presents.fetch_add(1, Ordering::Relaxed);
        }
    }
}

type PlayerVirtualEventFn = unsafe extern "system" fn(*mut c_void, usize, *mut c_void);

unsafe extern "system" fn observe_player_virtual_event(
    this: *mut c_void,
    event_flags: usize,
    player_object: *mut c_void,
) {
    let original = PLAYER_EVENT_ORIGINAL.load(Ordering::Acquire);
    if original == 0 {
        return;
    }
    let original: PlayerVirtualEventFn = unsafe { std::mem::transmute(original) };
    // The audited adapter forwards the game callback exactly once. Publishing the opaque player
    // pointer happens only after the original returns and performs no allocation or engine call.
    unsafe { original(this, event_flags, player_object) };
    if this as usize == PLAYER_EVENT_OWNER.load(Ordering::Acquire) && !player_object.is_null() {
        PLAYER_EVENT_OBJECT.store(player_object as usize, Ordering::Release);
        PLAYER_EVENT_GENERATION.fetch_add(1, Ordering::AcqRel);
    }
}

fn current_module_bounds() -> Result<(usize, usize), String> {
    let module = unsafe { GetModuleHandleW(None) }
        .map_err(|error| format!("locate CP2077 module failed: {error}"))?;
    let mut module_info = MODULEINFO::default();
    if !unsafe {
        K32GetModuleInformation(
            GetCurrentProcess(),
            module,
            &mut module_info,
            size_of::<MODULEINFO>() as u32,
        )
    }
    .as_bool()
    {
        return Err("query CP2077 module failed".into());
    }
    let base = module_info.lpBaseOfDll as usize;
    Ok((base, base.saturating_add(module_info.SizeOfImage as usize)))
}

fn executable_process_address(address: usize, module_base: usize, module_end: usize) -> bool {
    (module_base..module_end).contains(&address) && executable_committed_address(address)
}

fn executable_committed_address(address: usize) -> bool {
    if address == 0 {
        return false;
    }
    let mut info = MEMORY_BASIC_INFORMATION::default();
    let queried = unsafe {
        VirtualQuery(
            Some(address as *const c_void),
            &mut info,
            size_of::<MEMORY_BASIC_INFORMATION>(),
        )
    };
    if queried != size_of::<MEMORY_BASIC_INFORMATION>()
        || info.State != MEM_COMMIT
        || info.Protect.contains(PAGE_GUARD)
        || info.Protect.contains(PAGE_NOACCESS)
    {
        return false;
    }
    matches!(
        info.Protect.0 & 0xff,
        value if value == PAGE_EXECUTE_READ.0
            || value == PAGE_EXECUTE_READWRITE.0
            || value == PAGE_EXECUTE_WRITECOPY.0
    )
}

fn complete_player_vtable_entry_count(
    memo: &mut RegionMemo,
    vtable: usize,
) -> Result<usize, String> {
    for index in 0..PLAYER_EVENT_MAX_VTABLE_ENTRIES {
        let target = memo.value::<usize>(add_address(
            vtable,
            index
                .checked_mul(size_of::<usize>())
                .ok_or("player virtual-event vtable offset overflow")?,
        )?)?;
        if !executable_committed_address(target) {
            if index < PLAYER_EVENT_MIN_VTABLE_ENTRIES {
                return Err(format!(
                    "player virtual-event vtable ended before audited slot 63 at entry {index}"
                ));
            }
            return Ok(index);
        }
    }
    Err(format!(
        "player virtual-event vtable has no bounded terminator within {PLAYER_EVENT_MAX_VTABLE_ENTRIES} entries"
    ))
}

fn install_player_event_hook(
    session: &Cp2077PoseRuntimeSession,
    target: SystemSnapshotTarget,
) -> Result<(), String> {
    if target.role != PoseRole::Player || target.type_bytes != CP2077_PLAYER_SYSTEM_TYPE_BYTES {
        return Err("player virtual-event hook target is not exact cpPlayerSystem".into());
    }
    let audit = session
        .player_abi_audit()
        .ok_or("player virtual-event hook has no loaded-image ABI audit")?;
    if audit.evidence_id != CP2077_PLAYER_ABI_EVIDENCE_ID
        || audit.vcall_slot_index != CP2077_PLAYER_VCALL_SLOT_INDEX
        || !audit.rdx_plus_0x10_forwarded_to_vcall_r8
        || !audit.byte_from_r8_plus_0x52_forwarded_to_vcall_dl
    {
        return Err("player virtual-event hook ABI evidence does not match slot 63".into());
    }
    let (module_base, module_end) = current_module_bounds()?;
    let mut memo = RegionMemo::new();
    let Some((class_chain, type_bytes)) =
        native_class_chain(&mut memo, target.object, module_base, module_end)
    else {
        return Err("player virtual-event owner has no exact readable RTTI".into());
    };
    if class_chain.first().copied() != Some(CP2077_PLAYER_SYSTEM_CNAME_HASH)
        || type_bytes != CP2077_PLAYER_SYSTEM_TYPE_BYTES
    {
        return Err("player virtual-event owner RTTI changed before hook install".into());
    }
    let vtable = memo.value::<usize>(target.object)?;
    let locator_address = vtable
        .checked_sub(size_of::<usize>())
        .ok_or("player virtual-event CompleteObjectLocator address underflow")?;
    let locator = memo.value::<usize>(locator_address)?;
    if !(module_base..module_end).contains(&locator) || !memo.readable(locator, size_of::<u32>()) {
        return Err("player virtual-event vtable has no exact-image CompleteObjectLocator".into());
    }
    let vtable_entries = complete_player_vtable_entry_count(&mut memo, vtable)?;
    let vtable_bytes = vtable_entries
        .checked_mul(size_of::<usize>())
        .ok_or("player virtual-event vtable span overflow")?;
    if !memo.readable(vtable, vtable_bytes) {
        return Err("player virtual-event vtable span is not readable".into());
    }
    let original = memo.value::<usize>(add_address(
        vtable,
        CP2077_PLAYER_VCALL_SLOT_INDEX as usize * size_of::<usize>(),
    )?)?;
    if !executable_process_address(original, module_base, module_end) {
        return Err("player virtual-event slot 63 target is not exact game executable code".into());
    }
    PLAYER_EVENT_OWNER.store(target.object, Ordering::Release);
    PLAYER_EVENT_ORIGINAL.store(original, Ordering::Release);
    let shadow = CppVtableShadow::capture_and_activate(target.object, vtable_entries)?;
    if let Err(error) = shadow.rearm_slot_if_restored(
        CP2077_PLAYER_VCALL_SLOT_INDEX as usize,
        original,
        observe_player_virtual_event as *const () as usize,
    ) {
        PLAYER_EVENT_ORIGINAL.store(0, Ordering::Release);
        PLAYER_EVENT_OWNER.store(0, Ordering::Release);
        return Err(error);
    }
    if PLAYER_EVENT_SHADOW.set(shadow).is_err() {
        PLAYER_EVENT_ORIGINAL.store(0, Ordering::Release);
        PLAYER_EVENT_OWNER.store(0, Ordering::Release);
        return Err("player virtual-event shadow was installed concurrently".into());
    }
    Ok(())
}

fn refresh_validated_player_event_chain(last_generation: &mut u64) {
    let generation = PLAYER_EVENT_GENERATION.load(Ordering::Acquire);
    let player = PLAYER_EVENT_OBJECT.load(Ordering::Acquire);
    if generation == 0
        || player == 0
        || (generation == *last_generation
            && VALIDATED_PLAYER_OBJECT.load(Ordering::Acquire) == player)
    {
        return;
    }
    let validated = (|| -> Result<usize, String> {
        let mut memo = RegionMemo::new();
        let (module_base, module_end) = current_module_bounds()?;
        let Some((player_chain, player_bytes)) =
            native_class_chain(&mut memo, player, module_base, module_end)
        else {
            return Err("player event object has no readable exact RTTI".into());
        };
        if player_bytes < 0x160
            || !player_chain
                .iter()
                .any(|hash| matches!(*hash, PLAYER_PUPPET_CNAME_HASH | GAME_PUPPET_CNAME_HASH))
        {
            return Err("player event object is not PlayerPuppet/gamePuppet".into());
        }
        let component =
            memo.value::<usize>(add_address(player, ENT_ENTITY_TRANSFORM_COMPONENT_OFFSET)?)?;
        read_exact_player_transform_component(
            &mut memo,
            player,
            component,
            module_base,
            module_end,
        )?;
        Ok(component)
    })();
    match validated {
        Ok(component) => {
            VALIDATED_PLAYER_COMPONENT.store(component, Ordering::Release);
            VALIDATED_PLAYER_OBJECT.store(player, Ordering::Release);
            *last_generation = generation;
        }
        Err(_) => {
            VALIDATED_PLAYER_OBJECT.store(0, Ordering::Release);
            VALIDATED_PLAYER_COMPONENT.store(0, Ordering::Release);
        }
    }
}

fn read_same_present_player_snapshot() -> Option<PlayerPresentSnapshot> {
    let player = VALIDATED_PLAYER_OBJECT.load(Ordering::Acquire);
    let component = VALIDATED_PLAYER_COMPONENT.load(Ordering::Acquire);
    if player == 0 || component == 0 {
        return None;
    }
    let snapshot = (|| -> Result<PlayerPresentSnapshot, String> {
        let mut memo = RegionMemo::new();
        let current_component =
            memo.value::<usize>(add_address(player, ENT_ENTITY_TRANSFORM_COMPONENT_OFFSET)?)?;
        if current_component != component {
            return Err("player transform component changed".into());
        }
        let owner = memo.value::<usize>(add_address(component, ENT_COMPONENT_OWNER_OFFSET)?)?;
        if owner != player {
            return Err("player transform owner backlink changed".into());
        }
        let bytes: [u8; WORLD_TRANSFORM_BYTES] = memo
            .bytes(
                add_address(component, ENT_PLACED_COMPONENT_WORLD_TRANSFORM_OFFSET)?,
                WORLD_TRANSFORM_BYTES,
            )?
            .try_into()
            .map_err(|_| "player WorldTransform byte width changed".to_string())?;
        if !exact_world_transform_bytes_are_structural(&bytes) {
            return Err("player WorldTransform lost its exact structural shape".into());
        }
        Ok(PlayerPresentSnapshot {
            player_object: player,
            transform_component: component,
            bytes,
        })
    })();
    if snapshot.is_err() {
        let _ = VALIDATED_PLAYER_OBJECT.compare_exchange(
            player,
            0,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        VALIDATED_PLAYER_COMPONENT.store(0, Ordering::Release);
    }
    snapshot.ok()
}

fn observe_same_present_player_snapshot(
    session: &mut Cp2077PoseRuntimeSession,
    present_seq: u64,
    present_qpc: i64,
    snapshot: PlayerPresentSnapshot,
) -> Result<(), String> {
    if snapshot.player_object == 0 || snapshot.transform_component == 0 {
        return Err("same-Present player snapshot has a null object chain".into());
    }
    if !session.has_resolver(PLAYER_EVENT_WORLD_TRANSFORM_RESOLVER_ID) {
        let anchor_rva = session
            .address_catalog()
            .retained_symbols
            .iter()
            .find(|symbol| symbol.symbol == CP2077_PLAYER_SPAWN_SYMBOL)
            .and_then(|symbol| symbol.rva);
        session.register_resolver(ResolverObservation {
            resolver_id: PLAYER_EVENT_WORLD_TRANSFORM_RESOLVER_ID.into(),
            role: PoseRole::Player,
            source: RuntimeResolverSource::FirstPartyPlayerSpawnEvent,
            anchor_symbol: Some(CP2077_PLAYER_SPAWN_SYMBOL.into()),
            anchor_rva,
            pointer_offsets: vec![
                0,
                ENT_ENTITY_TRANSFORM_COMPONENT_OFFSET as u32,
                ENT_PLACED_COMPONENT_WORLD_TRANSFORM_OFFSET as u32,
            ],
            object_type_name: Some("PlayerPuppet.entIPlacedComponent.WorldTransform".into()),
            object_type_bytes: Some(WORLD_TRANSFORM_BYTES as u32),
            hook_abi: HookAbiEvidence::Validated {
                evidence_id: CP2077_PLAYER_ABI_EVIDENCE_ID.into(),
                original_forwarded_exactly_once: true,
            },
        })?;
    }
    session
        .clear_provisional_discovery_blocker("player_world_transform_read_only_chain_not_unique");
    session.clear_provisional_discovery_blocker("player_snapshot_asynchronous_to_selected_present");
    let object_tag = session.register_ephemeral_object(snapshot.transform_component)?;
    session.observe_snapshot(SnapshotObservationInput {
        role: PoseRole::Player,
        resolver_id: PLAYER_EVENT_WORLD_TRANSFORM_RESOLVER_ID,
        object_tag,
        present_seq,
        qpc: present_qpc,
        scenario: DiscoveryScenario::OnFootTranslation,
        bytes: &snapshot.bytes,
    })?;
    Ok(())
}

fn pose_worker(
    receiver: std::sync::mpsc::Receiver<PresentInput>,
    mut session: Cp2077PoseRuntimeSession,
    dropped_presents: Arc<AtomicU64>,
    latest_present: Arc<LatestPresent>,
    output: PoseWorkerOutput,
) {
    let mut previous_eligible = 0;
    let mut poll_ordinal = 0;
    let mut last_poll = Instant::now() - SYSTEM_ENUMERATION_RETRY_INTERVAL;
    let object_snapshot_interval = output.stop.object_snapshot_interval();
    let player_event_hook_allowed = output.stop.install_player_event_hook();
    let mut last_object_snapshot = Instant::now() - object_snapshot_interval;
    let mut last_system_signature = None;
    let mut snapshot_targets = Vec::new();
    let mut player_cache = None;
    let mut last_snapshot_present = None;
    let mut last_player_event_generation = 0;
    loop {
        match receiver.recv_timeout(Duration::from_millis(25)) {
            Ok(input) => {
                if let Some(snapshot) = input.player_snapshot
                    && let Err(error) = observe_same_present_player_snapshot(
                        &mut session,
                        input.present_seq,
                        input.qpc,
                        snapshot,
                    )
                {
                    session
                        .record_discovery_blocker("same_present_player_snapshot_rejected", error);
                }
                if input.render_observation {
                    let new_pre_clear = input.eligible_pre_clear_snapshot_count > previous_eligible;
                    previous_eligible = input.eligible_pre_clear_snapshot_count;
                    let observation = RenderPresentDiscoveryInput {
                        present_seq: input.present_seq,
                        qpc: input.qpc,
                        generation: 1,
                        resource_epoch: input.resource_epoch,
                        queue_count: input.queue_count,
                        command_list_count: input.command_list_count,
                        depth_resource_count: input.depth_resource_count,
                        eligible_pre_clear_snapshot_count: input.eligible_pre_clear_snapshot_count,
                        new_pre_clear_snapshot_since_previous_present: new_pre_clear,
                        capture_safe: input.capture_safe,
                        physical_view_matrix_source_observed: input
                            .physical_view_matrix_source_observed,
                        physical_projection_matrix_source_observed: input
                            .physical_projection_matrix_source_observed,
                        camera_candidate_admitted: input.camera_candidate_admitted,
                    };
                    if let Err(error) = session.observe_render_present_input(observation) {
                        session
                            .record_discovery_blocker("render_present_observation_failed", format!(
                                "{error}; present={} new_copy={new_pre_clear} capture_safe={} view={} projection={} authoring_admitted={}",
                                input.present_seq,input.capture_safe,input.physical_view_matrix_source_observed,
                                input.physical_projection_matrix_source_observed,input.camera_candidate_admitted));
                    }
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                session.record_discovery_blocker(
                    "render_present_ingress_disconnected",
                    "the process-wide Present ingress disconnected before pose-worker stop",
                );
                break;
            }
        }
        refresh_validated_player_event_chain(&mut last_player_event_generation);
        let has_unique_player_target = unique_role_target(&snapshot_targets, PoseRole::Player);
        let event_hook_active = PLAYER_EVENT_ORIGINAL.load(Ordering::Acquire) != 0;
        if has_unique_player_target
            && (!event_hook_active || output.stop.observe_legacy_camera_object())
            && last_object_snapshot.elapsed() >= object_snapshot_interval
            && let Some(present) = latest_present
                .load_consistent()
                .filter(|present| newer_present_identity(last_snapshot_present, *present))
        {
            last_object_snapshot = Instant::now();
            let outcome = observe_system_object_snapshots(
                &mut session,
                &snapshot_targets,
                &mut player_cache,
                output.stop.observe_legacy_camera_object(),
                !event_hook_active,
                present.0,
                present.1,
            );
            if outcome.any_observed {
                last_snapshot_present = Some(present);
            }
            if !outcome.player_observed {
                player_cache = None;
                snapshot_targets.clear();
                last_poll = Instant::now() - SYSTEM_ENUMERATION_RETRY_INTERVAL;
            }
        }
        let refresh_interval = output
            .stop
            .system_refresh_interval(unique_role_target(&snapshot_targets, PoseRole::Player));
        if last_poll.elapsed() >= refresh_interval {
            last_poll = Instant::now();
            poll_ordinal += 1;
            match enumerate_current_process_systems(&mut session, poll_ordinal, current_qpc()) {
                Ok(result) => {
                    let observation = result.observation;
                    let signature = (
                        observation.cgame_engine_resolved,
                        observation.framework_resolved,
                        observation.game_instance_resolved,
                        observation.declared_system_count,
                        observation.candidates.len(),
                        observation.blocker.clone(),
                    );
                    if last_system_signature.as_ref() != Some(&signature) {
                        last_system_signature = Some(signature);
                        if let Err(error) = session.observe_system_enumeration(observation) {
                            session.record_discovery_blocker("system_enumeration_rejected", error);
                        }
                    }
                    snapshot_targets = result.snapshot_targets;
                    if player_event_hook_allowed
                        && unique_role_target(&snapshot_targets, PoseRole::Player)
                        && PLAYER_EVENT_ORIGINAL.load(Ordering::Acquire) == 0
                        && let Some(target) = snapshot_targets
                            .iter()
                            .find(|target| target.role == PoseRole::Player)
                            .copied()
                        && let Err(error) = install_player_event_hook(&session, target)
                    {
                        session.record_discovery_blocker(
                            "player_virtual_event_hook_install_failed",
                            error,
                        );
                    }
                    // A periodic full system enumeration is also the lifetime boundary for the
                    // fast chain. The next 100 ms sample must rediscover the unique player from
                    // the refreshed exact cpPlayerSystem object before caching it again.
                    player_cache = None;
                }
                Err(error) => {
                    session.record_discovery_blocker("system_enumeration_failed", error);
                }
            }
        }
        if output.stop.requested() {
            break;
        }
    }
    let dropped = dropped_presents.load(Ordering::Acquire);
    if dropped != 0 {
        session.record_discovery_blocker(
            "render_present_inputs_dropped",
            format!("bounded discovery ingress dropped {dropped} Present inputs"),
        );
    }
    let report = session.finish();
    let write_result = write_report_create_new(&output.report_path, &output.game_root, &report);
    if let Err(error) = &write_result {
        crate::probe::log(&format!(
            "CP2077 pose observation report creation failed: {error}"
        ));
    }
    if let Some(completion) = output.completion {
        completion.publish(write_result);
    }
}

fn usize_to_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

fn current_qpc() -> i64 {
    use windows::Win32::System::Performance::QueryPerformanceCounter;
    let mut qpc = 0;
    if unsafe { QueryPerformanceCounter(&mut qpc) }.is_ok() {
        qpc
    } else {
        0
    }
}

fn qpc_frequency() -> i64 {
    use windows::Win32::System::Performance::QueryPerformanceFrequency;
    let mut frequency = 0;
    if unsafe { QueryPerformanceFrequency(&mut frequency) }.is_ok() {
        frequency
    } else {
        0
    }
}

/// 一次控制线程结构游走内共享的 region 裁定：同一个 `VirtualQuery` region 只查一次。
///
/// CP2077 的对象定位要走完整条 RTTI 父链，再对每个候选对象逐字段取证；同一对象的相邻
/// 字段、同一批 CClass 描述符几乎必然落在同一个 region 里。真机实测：探针从收到开始
/// 命令到发出 `Started` 的 10.34 秒里，5 827 次 `VirtualQuery` 占了 9.62 秒——游戏自身
/// 的内存活动把单次查询拖到毫秒级，与 #504 在渲染线程上量到的是同一个现象。
///
/// 裁定式与单次查询逐条相同（单 region、`MEM_COMMIT`、非 GUARD/NOACCESS、基础保护属于
/// 可读集合、整个跨度落在该 region 内）。`VirtualQuery` 返回的就是包含目标地址的那一整段
/// 同质 region，所以落在已裁定 region 内的后续地址重新查一次也只会得到同一个
/// `[base, limit)`：命中缓存与真查一次的结论相同，边界、类型、归属、唯一性各项校验一个
/// 都没有放松。
///
/// 为什么命中缓存仍然安全：真正的数据一律走可失败的 `ReadProcessMemory`。若 region 在一次
/// 游走途中被改保护或解提交，RPM 会返回错误，取证照常 fail-closed，不会出现裸指针解引用的
/// 访问违例。缓存**不跨游走**：每轮周期审计、每次重新录制都从空缓存重新验证整条链。
pub(crate) struct RegionMemo {
    regions: [(usize, usize); REGION_MEMO_ENTRIES],
    len: usize,
    queries: u64,
    hits: u64,
}

impl RegionMemo {
    pub(crate) const fn new() -> Self {
        Self {
            regions: [(0, 0); REGION_MEMO_ENTRIES],
            len: 0,
            queries: 0,
            hits: 0,
        }
    }

    /// 与单次 `VirtualQuery` 裁定同义的可读性判断，只是把通过的 region 边界留给本次游走复用。
    pub(crate) fn readable(&mut self, address: usize, bytes: usize) -> bool {
        if address == 0 || bytes == 0 {
            return false;
        }
        let Some(request_end) = address.checked_add(bytes) else {
            return false;
        };
        if let Some(index) = self.regions[..self.len]
            .iter()
            .position(|(base, limit)| address >= *base && request_end <= *limit)
        {
            self.hits += 1;
            // 最近命中的 region 挪到队首：游走是成批落在同一段内存上的。
            self.regions[..=index].rotate_right(1);
            return true;
        }
        self.queries += 1;
        let Some((base, limit)) = query_readable_region(address) else {
            return false;
        };
        if address < base || request_end > limit {
            return false;
        }
        self.remember(base, limit);
        true
    }

    fn remember(&mut self, base: usize, limit: usize) {
        if self.len < REGION_MEMO_ENTRIES {
            self.len += 1;
        }
        // 满了就挤掉最久没命中的那条。
        self.regions[..self.len].rotate_right(1);
        self.regions[0] = (base, limit);
    }

    /// 结构游走里的定长读：可读性走共享裁定，数据仍走可失败的 RPM。
    pub(crate) fn value<T: Copy>(&mut self, address: usize) -> Result<T, String> {
        if address == 0 {
            return Err("null process-memory address".into());
        }
        if !self.readable(address, size_of::<T>()) {
            return Err("process-memory range is not committed and readable".into());
        }
        read_process_value_no_query(address)
    }

    /// 结构游走里的有界字节快照：可读性走共享裁定，数据仍走可失败的 RPM。
    pub(crate) fn bytes(&mut self, address: usize, bytes: usize) -> Result<Vec<u8>, String> {
        if address == 0 || bytes == 0 || bytes > MAX_OBJECT_SNAPSHOT_BYTES {
            return Err("invalid bounded object snapshot request".into());
        }
        if !self.readable(address, bytes) {
            return Err("bounded object snapshot range is not committed and readable".into());
        }
        let mut snapshot = vec![0_u8; bytes];
        let mut bytes_read = 0;
        unsafe {
            ReadProcessMemory(
                GetCurrentProcess(),
                address as *const c_void,
                snapshot.as_mut_ptr().cast(),
                bytes,
                Some(&mut bytes_read),
            )
        }
        .map_err(|error| format!("bounded object snapshot failed: {error}"))?;
        if bytes_read != bytes {
            return Err("bounded object snapshot was short".into());
        }
        Ok(snapshot)
    }
}

/// 走完一次结构游走就把省下的查询数交给启动诊断 harness。
///
/// 放在 `Drop` 里而不是每个调用点手写一行：`RegionMemo` 一律是单次游走的局部变量，
/// 析构点就是游走边界，不会漏也不会重复。harness 的 `ACTIVE` 是线程局部的，所以启动
/// 之外的周期审计和渲染线程上的读取都落在报告之外。
impl Drop for RegionMemo {
    fn drop(&mut self) {
        crate::startup_timing::region_memo_walk(self.queries, self.hits);
    }
}

/// Fallible current-process read for the render hot path. Unlike a Rust pointer
/// dereference, `ReadProcessMemory` reports a concurrently decommitted range as
/// an error instead of raising an access violation in the game process. The
/// background audit remains responsible for structural admission; deliberately
/// omit `VirtualQuery` here because it was measured as unsafe for Present latency.
pub(crate) fn read_process_value_no_query<T: Copy>(address: usize) -> Result<T, String> {
    if address == 0 {
        return Err("null process-memory address".into());
    }
    let mut value = std::mem::MaybeUninit::<T>::uninit();
    let mut bytes_read = 0;
    unsafe {
        ReadProcessMemory(
            GetCurrentProcess(),
            address as *const c_void,
            value.as_mut_ptr().cast(),
            size_of::<T>(),
            Some(&mut bytes_read),
        )
    }
    .map_err(|error| format!("fallible current-process read failed: {error}"))?;
    if bytes_read != size_of::<T>() {
        return Err("fallible current-process read was short".into());
    }
    Ok(unsafe { value.assume_init() })
}

#[derive(Debug)]
struct PlayerWorldTransformSnapshot {
    system_field_offset: u32,
    object_tag: u64,
    bytes: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CachedPlayerChain {
    system_object: usize,
    system_type_bytes: u32,
    system_field_offset: u32,
    player_object: usize,
    transform_component: usize,
}

/// Exact current-process addresses retained by the formal sampler after the bounded RTTI scan.
/// They are never serialized and are re-resolved for every production generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FormalPlayerAddresses {
    pub system_object: usize,
    pub player_slot: *const usize,
    pub player_object: usize,
    pub transform_component_slot: *const usize,
    pub transform_component: usize,
    pub component_owner_slot: *const usize,
    pub world_transform: *const [u32; WORLD_TRANSFORM_BYTES / size_of::<u32>()],
}

// SAFETY: these are immutable current-process address values. The control-thread audit validates
// all pointees and the render-thread sampler accesses them only through fallible ReadProcessMemory.
unsafe impl Send for FormalPlayerAddresses {}
unsafe impl Sync for FormalPlayerAddresses {}

#[derive(Debug)]
struct CameraWorldTransformSnapshot {
    camera_system_field_offset: u32,
    object_tag: u64,
    bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PlayerChainCandidate {
    player_object: usize,
    transform_component: usize,
    system_field_offsets: BTreeSet<u32>,
}

fn unique_player_chain(
    candidates: BTreeMap<(usize, usize), BTreeSet<u32>>,
) -> Result<PlayerChainCandidate, String> {
    if candidates.len() != 1 {
        return Err(format!(
            "bounded cpPlayerSystem scan expected one PlayerPuppet/transform chain, observed {}",
            candidates.len()
        ));
    }
    let ((player_object, transform_component), system_field_offsets) =
        candidates.into_iter().next().expect("length checked above");
    if system_field_offsets.is_empty() {
        return Err("bounded cpPlayerSystem scan produced no stable field offset".into());
    }
    Ok(PlayerChainCandidate {
        player_object,
        transform_component,
        system_field_offsets,
    })
}

fn canonical_system_field_offset(offsets: &BTreeSet<u32>) -> Result<u32, String> {
    // cpPlayerSystem may retain the same live PlayerPuppet in more than one field. That is one
    // object chain, not an ambiguous player. Pick the lowest alias deterministically; the cached
    // reader and formal audit keep re-reading this exact slot and fail closed if it stops pointing
    // at the independently verified PlayerPuppet.
    offsets
        .iter()
        .next()
        .copied()
        .ok_or_else(|| "bounded cpPlayerSystem scan produced no stable field offset".into())
}

fn unique_direct_object(
    candidates: BTreeMap<usize, BTreeSet<u32>>,
    label: &str,
) -> Result<(usize, BTreeSet<u32>), String> {
    if candidates.len() != 1 {
        return Err(format!(
            "bounded {label} scan expected one exact object, observed {}",
            candidates.len()
        ));
    }
    let (object, offsets) = candidates.into_iter().next().expect("length checked above");
    if offsets.is_empty() {
        return Err(format!(
            "bounded {label} scan produced no stable field offset"
        ));
    }
    Ok((object, offsets))
}

fn read_exact_object_extent(
    memo: &mut RegionMemo,
    address: usize,
    bytes: usize,
) -> Result<Vec<u8>, String> {
    if address == 0 || bytes == 0 || bytes > CP2077_PLAYER_SYSTEM_TYPE_BYTES as usize {
        return Err("invalid bounded exact-object extent".into());
    }
    let mut result = Vec::with_capacity(bytes);
    while result.len() < bytes {
        let current = add_address(address, result.len())?;
        let page_remaining = POINTER_SCAN_PAGE_BYTES - (current % POINTER_SCAN_PAGE_BYTES);
        let chunk = page_remaining.min(bytes - result.len());
        result.extend(memo.bytes(current, chunk)?);
    }
    Ok(result)
}

pub(crate) fn native_class_chain(
    memo: &mut RegionMemo,
    object: usize,
    module_base: usize,
    module_end: usize,
) -> Option<(Vec<u64>, u32)> {
    let vtable = memo.value::<usize>(object).ok()?;
    if vtable < module_base || vtable >= module_end {
        return None;
    }
    let mut class = memo
        .value::<usize>(add_address(object, SCRIPTABLE_NATIVE_TYPE_OFFSET).ok()?)
        .ok()?;
    if class == 0 {
        return None;
    }
    let mut hashes = Vec::new();
    let mut leaf_size = None;
    for _ in 0..MAX_RTTI_PARENT_DEPTH {
        let class_vtable = memo.value::<usize>(class).ok()?;
        if class_vtable < module_base || class_vtable >= module_end {
            return None;
        }
        hashes.push(
            memo.value::<u64>(add_address(class, CCLASS_NAME_OFFSET).ok()?)
                .ok()?,
        );
        if leaf_size.is_none() {
            leaf_size = Some(
                memo.value::<u32>(add_address(class, CCLASS_SIZE_OFFSET).ok()?)
                    .ok()?,
            );
        }
        class = memo.value::<usize>(add_address(class, 0x10).ok()?).ok()?;
        if class == 0 {
            return Some((hashes, leaf_size?));
        }
    }
    None
}

fn exact_world_transform_bytes_are_structural(bytes: &[u8]) -> bool {
    let (candidates, truncated) = scan_world_transform_candidates(bytes, 2);
    !truncated
        && candidates.len() == 1
        && candidates[0].transform_offset == 0
        && bytes.len() == WORLD_TRANSFORM_BYTES
}

fn read_exact_player_transform_component(
    memo: &mut RegionMemo,
    player: usize,
    transform_component: usize,
    module_base: usize,
    module_end: usize,
) -> Result<Vec<u8>, String> {
    let component_vtable = memo
        .value::<usize>(transform_component)
        .map_err(|_| "PlayerPuppet transform component vtable is unreadable".to_string())?;
    if component_vtable < module_base || component_vtable >= module_end {
        return Err(
            "PlayerPuppet transform component vtable is outside the exact game image".into(),
        );
    }
    let owner = memo
        .value::<usize>(add_address(
            transform_component,
            ENT_COMPONENT_OWNER_OFFSET,
        )?)
        .map_err(|_| "PlayerPuppet transform component owner is unreadable".to_string())?;
    if owner != player {
        return Err(
            "PlayerPuppet transform component owner does not point back to the player".into(),
        );
    }
    let bytes = memo.bytes(
        add_address(
            transform_component,
            ENT_PLACED_COMPONENT_WORLD_TRANSFORM_OFFSET,
        )?,
        WORLD_TRANSFORM_BYTES,
    )?;
    if !exact_world_transform_bytes_are_structural(&bytes) {
        return Err(
            "PlayerPuppet transform component worldTransform is not one exact structural WorldTransform"
                .into(),
        );
    }
    Ok(bytes)
}

fn read_cached_player_world_transform(
    session: &mut Cp2077PoseRuntimeSession,
    target: SystemSnapshotTarget,
    cached: CachedPlayerChain,
) -> Result<PlayerWorldTransformSnapshot, String> {
    if cached.system_object != target.object || cached.system_type_bytes != target.type_bytes {
        return Err("cached player chain belongs to a different cpPlayerSystem target".into());
    }
    let mut memo = RegionMemo::new();
    let module = unsafe { GetModuleHandleW(None) }
        .map_err(|error| format!("locate CP2077 module for cached player read failed: {error}"))?;
    let mut module_info = MODULEINFO::default();
    if !unsafe {
        K32GetModuleInformation(
            GetCurrentProcess(),
            module,
            &mut module_info,
            size_of::<MODULEINFO>() as u32,
        )
    }
    .as_bool()
    {
        return Err("query CP2077 module for cached player read failed".into());
    }
    let module_base = module_info.lpBaseOfDll as usize;
    let module_end = module_base.saturating_add(module_info.SizeOfImage as usize);
    let Some((system_chain, current_type_bytes)) =
        native_class_chain(&mut memo, target.object, module_base, module_end)
    else {
        return Err("cached cpPlayerSystem target no longer has exact readable RTTI".into());
    };
    if current_type_bytes != target.type_bytes
        || system_chain.first().copied() != Some(CP2077_PLAYER_SYSTEM_CNAME_HASH)
    {
        return Err("cached cpPlayerSystem target identity changed".into());
    }
    let field_end = usize::try_from(cached.system_field_offset)
        .ok()
        .and_then(|offset| offset.checked_add(size_of::<usize>()))
        .ok_or_else(|| "cached cpPlayerSystem field offset overflowed".to_string())?;
    if field_end > target.type_bytes as usize {
        return Err("cached cpPlayerSystem player field is outside the exact object".into());
    }
    let current_player = memo
        .value::<usize>(add_address(
            cached.system_object,
            cached.system_field_offset as usize,
        )?)
        .map_err(|_| "cached cpPlayerSystem player field became unreadable".to_string())?;
    if current_player != cached.player_object {
        return Err("cached cpPlayerSystem now points to a different PlayerPuppet".into());
    }
    let Some((player_chain, player_bytes)) =
        native_class_chain(&mut memo, cached.player_object, module_base, module_end)
    else {
        return Err("cached PlayerPuppet no longer has readable RTTI".into());
    };
    if player_bytes < 0x160
        || !player_chain
            .iter()
            .any(|hash| matches!(*hash, PLAYER_PUPPET_CNAME_HASH | GAME_PUPPET_CNAME_HASH))
    {
        return Err("cached PlayerPuppet identity changed".into());
    }
    let current_component = memo
        .value::<usize>(add_address(
            cached.player_object,
            ENT_ENTITY_TRANSFORM_COMPONENT_OFFSET,
        )?)
        .map_err(|_| "cached PlayerPuppet transform component became unreadable".to_string())?;
    if current_component != cached.transform_component {
        return Err("cached PlayerPuppet transform component changed".into());
    }
    let bytes = read_exact_player_transform_component(
        &mut memo,
        cached.player_object,
        cached.transform_component,
        module_base,
        module_end,
    )?;
    let object_tag = session.register_ephemeral_object(cached.transform_component)?;
    Ok(PlayerWorldTransformSnapshot {
        system_field_offset: cached.system_field_offset,
        object_tag,
        bytes,
    })
}

fn resolve_unique_player_world_transform(
    session: &mut Cp2077PoseRuntimeSession,
    target: SystemSnapshotTarget,
) -> Result<(PlayerWorldTransformSnapshot, CachedPlayerChain), String> {
    // cpPlayerSystem 里每个像指针的字长都要走一条 RTTI 父链取证，是定位期第二重的一段。
    let mut memo = RegionMemo::new();
    let module = unsafe { GetModuleHandleW(None) }
        .map_err(|error| format!("locate CP2077 module for player scan failed: {error}"))?;
    let mut module_info = MODULEINFO::default();
    if !unsafe {
        K32GetModuleInformation(
            GetCurrentProcess(),
            module,
            &mut module_info,
            size_of::<MODULEINFO>() as u32,
        )
    }
    .as_bool()
    {
        return Err("query CP2077 module for player scan failed".into());
    }
    let module_base = module_info.lpBaseOfDll as usize;
    let module_end = module_base.saturating_add(module_info.SizeOfImage as usize);
    let Some((system_chain, current_type_bytes)) =
        native_class_chain(&mut memo, target.object, module_base, module_end)
    else {
        return Err("cached cpPlayerSystem target no longer has exact readable RTTI".into());
    };
    if current_type_bytes != target.type_bytes
        || system_chain.first().copied() != Some(CP2077_PLAYER_SYSTEM_CNAME_HASH)
    {
        return Err(
            "cached cpPlayerSystem target identity changed; re-enumeration required".into(),
        );
    }
    let system_bytes =
        read_exact_object_extent(&mut memo, target.object, target.type_bytes as usize)?;
    let mut pointer_offsets: BTreeMap<usize, BTreeSet<u32>> = BTreeMap::new();
    let mut plausible_pointer_words = 0_usize;
    let mut rtti_objects = 0_usize;
    let mut player_class_matches = 0_usize;
    let mut transform_component_pointers = 0_usize;
    let mut component_vtable_matches = 0_usize;
    let mut component_owner_matches = 0_usize;
    let mut world_transform_shape_matches = 0_usize;
    // Diagnostic-only deduplication: preserve every original validation and only count addresses.
    // Addresses stay on this thread and are never written to the summary.
    let mut startup_candidates = crate::startup_timing::active().then(BTreeSet::new);
    for offset in
        (0..=system_bytes.len().saturating_sub(size_of::<usize>())).step_by(size_of::<usize>())
    {
        let player = usize::from_le_bytes(
            system_bytes[offset..offset + size_of::<usize>()]
                .try_into()
                .expect("pointer-sized slice"),
        );
        if !(0x1_0000..=MAX_X64_USER_POINTER).contains(&player)
            || !player.is_multiple_of(size_of::<usize>())
        {
            continue;
        }
        plausible_pointer_words += 1;
        if let Some(candidates) = startup_candidates.as_mut() {
            candidates.insert(player);
        }
        let Some((player_chain, player_bytes)) =
            native_class_chain(&mut memo, player, module_base, module_end)
        else {
            continue;
        };
        rtti_objects += 1;
        if player_bytes < 0x160
            || !player_chain
                .iter()
                .any(|hash| matches!(*hash, PLAYER_PUPPET_CNAME_HASH | GAME_PUPPET_CNAME_HASH))
        {
            continue;
        }
        player_class_matches += 1;
        let transform_component = match memo
            .value::<usize>(add_address(player, ENT_ENTITY_TRANSFORM_COMPONENT_OFFSET)?)
        {
            Ok(value) if value != 0 => value,
            _ => continue,
        };
        transform_component_pointers += 1;
        let Ok(component_vtable) = memo.value::<usize>(transform_component) else {
            continue;
        };
        if component_vtable < module_base || component_vtable >= module_end {
            continue;
        }
        component_vtable_matches += 1;
        let Ok(owner) = memo.value::<usize>(add_address(
            transform_component,
            ENT_COMPONENT_OWNER_OFFSET,
        )?) else {
            continue;
        };
        if owner != player {
            continue;
        }
        component_owner_matches += 1;
        let Ok(transform_bytes) = memo.bytes(
            add_address(
                transform_component,
                ENT_PLACED_COMPONENT_WORLD_TRANSFORM_OFFSET,
            )?,
            WORLD_TRANSFORM_BYTES,
        ) else {
            continue;
        };
        if !exact_world_transform_bytes_are_structural(&transform_bytes) {
            continue;
        }
        world_transform_shape_matches += 1;
        pointer_offsets
            .entry(player)
            .or_default()
            .insert(offset as u32);
    }
    if let Some(candidates) = startup_candidates {
        crate::startup_timing::scan_counts(
            system_bytes.len() / size_of::<usize>(),
            plausible_pointer_words,
            candidates.len(),
            rtti_objects,
        );
    }
    let mut chains: BTreeMap<(usize, usize), BTreeSet<u32>> = BTreeMap::new();
    for (player, offsets) in pointer_offsets {
        let transform_component = memo
            .value::<usize>(add_address(player, ENT_ENTITY_TRANSFORM_COMPONENT_OFFSET)?)
            .map_err(|_| "PlayerPuppet transform component became unreadable".to_string())?;
        chains.insert((player, transform_component), offsets);
    }
    let selected = unique_player_chain(chains).map_err(|error| {
        format!(
            "{error}; diagnostics plausible_pointer_words={plausible_pointer_words}, rtti_objects={rtti_objects}, player_puppet_or_game_puppet={player_class_matches}, transform_component_pointers={transform_component_pointers}, component_vtables_in_game_image={component_vtable_matches}, owner_back_references={component_owner_matches}, structural_world_transforms={world_transform_shape_matches}"
        )
    })?;
    let system_field_offset = canonical_system_field_offset(&selected.system_field_offsets)?;
    let current_transform_component = memo
        .value::<usize>(add_address(
            selected.player_object,
            ENT_ENTITY_TRANSFORM_COMPONENT_OFFSET,
        )?)
        .map_err(|_| "PlayerPuppet transform component became unreadable".to_string())?;
    if current_transform_component != selected.transform_component {
        return Err("PlayerPuppet transform component changed during bounded scan".into());
    }
    let bytes = read_exact_player_transform_component(
        &mut memo,
        selected.player_object,
        selected.transform_component,
        module_base,
        module_end,
    )?;
    let object_tag = session.register_ephemeral_object(selected.transform_component)?;
    Ok((
        PlayerWorldTransformSnapshot {
            system_field_offset,
            object_tag,
            bytes,
        },
        CachedPlayerChain {
            system_object: target.object,
            system_type_bytes: target.type_bytes,
            system_field_offset,
            player_object: selected.player_object,
            transform_component: selected.transform_component,
        },
    ))
}

/// Resolve the one exact cpPlayerSystem -> PlayerPuppet -> transformComponent chain for a formal
/// production generation. This intentionally reuses the discovery path's bounded RTTI proof but
/// returns only process-local addresses; no discovery report or event hook is started.
pub(crate) fn locate_formal_player(
    actual_executable_sha256: &str,
    process_launch_id: String,
    output_dir: &Path,
) -> Result<FormalPlayerAddresses, String> {
    let (mut session, _) = crate::startup_timing::phase("pose_bootstrap", || {
        current_process_pose_inputs(actual_executable_sha256, process_launch_id, output_dir)
    })?;
    let enumeration = crate::startup_timing::phase("system_enumeration", || {
        enumerate_current_process_systems(&mut session, 1, current_qpc())
    })?;
    let target = select_formal_player_target(enumeration)?;
    let (_, chain) = crate::startup_timing::phase("player_scan", || {
        resolve_unique_player_world_transform(&mut session, target)
    })?;
    let player_slot =
        add_address(chain.system_object, chain.system_field_offset as usize)? as *const usize;
    let transform_component_slot =
        add_address(chain.player_object, ENT_ENTITY_TRANSFORM_COMPONENT_OFFSET)? as *const usize;
    let component_owner_slot =
        add_address(chain.transform_component, ENT_COMPONENT_OWNER_OFFSET)? as *const usize;
    let world_transform = add_address(
        chain.transform_component,
        ENT_PLACED_COMPONENT_WORLD_TRANSFORM_OFFSET,
    )? as *const [u32; WORLD_TRANSFORM_BYTES / size_of::<u32>()];
    Ok(FormalPlayerAddresses {
        system_object: chain.system_object,
        player_slot,
        player_object: chain.player_object,
        transform_component_slot,
        transform_component: chain.transform_component,
        component_owner_slot,
        world_transform,
    })
}

fn select_formal_player_target(
    enumeration: SystemEnumerationResult,
) -> Result<SystemSnapshotTarget, String> {
    if enumeration.observation.candidates_truncated {
        return Err("CP2077 formal player system enumeration was truncated".into());
    }
    let discovery_blocker = enumeration.observation.blocker.as_deref();
    let targets = enumeration
        .snapshot_targets
        .into_iter()
        .filter(|target| target.role == PoseRole::Player)
        .collect::<Vec<_>>();
    if targets.len() != 1 {
        return Err(format!(
            "CP2077 formal player resolver expected one exact cpPlayerSystem, observed {}; discovery detail: {}",
            targets.len(),
            discovery_blocker.unwrap_or("none")
        ));
    }
    // Discovery deliberately keeps a blocker until the live owner/transform chain has been
    // proved. The formal path below is that proof, so the discovery-only blocker must not stop us
    // before resolve_unique_player_world_transform gets a chance to run.
    Ok(targets[0])
}

/// Control-thread proof for the formal sampler. Every check may use VirtualQuery/RPM here; the
/// Present hot path consumes only a fresh verdict and performs fallible RPM reads without another
/// VirtualQuery.
pub(crate) fn audit_formal_player(addresses: FormalPlayerAddresses) -> Result<(), String> {
    let mut memo = RegionMemo::new();
    let module = unsafe { GetModuleHandleW(None) }
        .map_err(|error| format!("locate CP2077 module for formal player audit failed: {error}"))?;
    let mut module_info = MODULEINFO::default();
    if !unsafe {
        K32GetModuleInformation(
            GetCurrentProcess(),
            module,
            &mut module_info,
            size_of::<MODULEINFO>() as u32,
        )
    }
    .as_bool()
    {
        return Err("query CP2077 module for formal player audit failed".into());
    }
    let module_base = module_info.lpBaseOfDll as usize;
    let module_end = module_base.saturating_add(module_info.SizeOfImage as usize);
    let Some((system_chain, system_bytes)) =
        native_class_chain(&mut memo, addresses.system_object, module_base, module_end)
    else {
        return Err("formal cpPlayerSystem no longer has readable exact RTTI".into());
    };
    if system_bytes != CP2077_PLAYER_SYSTEM_TYPE_BYTES
        || system_chain.first().copied() != Some(CP2077_PLAYER_SYSTEM_CNAME_HASH)
    {
        return Err("formal cpPlayerSystem identity changed".into());
    }
    let player = memo
        .value::<usize>(addresses.player_slot as usize)
        .map_err(|_| "formal cpPlayerSystem player field is unreadable".to_string())?;
    if player != addresses.player_object {
        return Err("formal cpPlayerSystem now points to a different PlayerPuppet".into());
    }
    let Some((player_chain, player_bytes)) =
        native_class_chain(&mut memo, player, module_base, module_end)
    else {
        return Err("formal PlayerPuppet no longer has readable exact RTTI".into());
    };
    if player_bytes < 0x160
        || !player_chain
            .iter()
            .any(|hash| matches!(*hash, PLAYER_PUPPET_CNAME_HASH | GAME_PUPPET_CNAME_HASH))
    {
        return Err("formal PlayerPuppet identity changed".into());
    }
    let component = memo
        .value::<usize>(addresses.transform_component_slot as usize)
        .map_err(|_| "formal PlayerPuppet transform component is unreadable".to_string())?;
    if component != addresses.transform_component {
        return Err("formal PlayerPuppet transform component changed".into());
    }
    read_exact_player_transform_component(&mut memo, player, component, module_base, module_end)?;
    Ok(())
}

fn resolve_unique_camera_world_transform(
    session: &mut Cp2077PoseRuntimeSession,
    target: SystemSnapshotTarget,
) -> Result<CameraWorldTransformSnapshot, String> {
    let mut memo = RegionMemo::new();
    let module = unsafe { GetModuleHandleW(None) }
        .map_err(|error| format!("locate CP2077 module for camera scan failed: {error}"))?;
    let mut module_info = MODULEINFO::default();
    if !unsafe {
        K32GetModuleInformation(
            GetCurrentProcess(),
            module,
            &mut module_info,
            size_of::<MODULEINFO>() as u32,
        )
    }
    .as_bool()
    {
        return Err("query CP2077 module for camera scan failed".into());
    }
    let module_base = module_info.lpBaseOfDll as usize;
    let module_end = module_base.saturating_add(module_info.SizeOfImage as usize);
    let system_bytes =
        read_exact_object_extent(&mut memo, target.object, target.type_bytes as usize)?;
    let mut candidates: BTreeMap<usize, BTreeSet<u32>> = BTreeMap::new();
    let mut plausible_pointer_words = 0_usize;
    let mut rtti_objects = 0_usize;
    let mut camera_component_matches = 0_usize;
    for offset in
        (0..=system_bytes.len().saturating_sub(size_of::<usize>())).step_by(size_of::<usize>())
    {
        let component = usize::from_le_bytes(
            system_bytes[offset..offset + size_of::<usize>()]
                .try_into()
                .expect("pointer-sized slice"),
        );
        if !(0x1_0000..=MAX_X64_USER_POINTER).contains(&component)
            || !component.is_multiple_of(size_of::<usize>())
        {
            continue;
        }
        plausible_pointer_words += 1;
        let Some((class_chain, component_bytes)) =
            native_class_chain(&mut memo, component, module_base, module_end)
        else {
            continue;
        };
        rtti_objects += 1;
        if component_bytes < 0x1e0
            || !class_chain.iter().any(|hash| {
                matches!(
                    *hash,
                    ENT_BASE_CAMERA_COMPONENT_CNAME_HASH | GAME_CAMERA_COMPONENT_CNAME_HASH
                )
            })
            || !class_chain.contains(&ENT_PLACED_COMPONENT_CNAME_HASH)
        {
            continue;
        }
        camera_component_matches += 1;
        candidates
            .entry(component)
            .or_default()
            .insert(offset as u32);
    }
    let (component, offsets) =
        unique_direct_object(candidates, "gameCameraSystem").map_err(|error| {
            format!(
                "{error}; diagnostics plausible_pointer_words={plausible_pointer_words}, rtti_objects={rtti_objects}, placed_camera_components={camera_component_matches}"
            )
        })?;
    let camera_system_field_offset = *offsets
        .first()
        .expect("unique camera component has at least one field offset");
    let bytes = memo.bytes(
        add_address(component, ENT_PLACED_COMPONENT_WORLD_TRANSFORM_OFFSET)?,
        WORLD_TRANSFORM_BYTES,
    )?;
    let object_tag = session.register_ephemeral_object(component)?;
    Ok(CameraWorldTransformSnapshot {
        camera_system_field_offset,
        object_tag,
        bytes,
    })
}

fn observe_system_object_snapshots(
    session: &mut Cp2077PoseRuntimeSession,
    targets: &[SystemSnapshotTarget],
    player_cache: &mut Option<CachedPlayerChain>,
    observe_legacy_camera_object: bool,
    observe_legacy_player_object: bool,
    present_seq: u64,
    _present_qpc: i64,
) -> SnapshotAttemptOutcome {
    let mut outcome = SnapshotAttemptOutcome::default();
    let mut roles = Vec::with_capacity(2);
    if observe_legacy_player_object {
        roles.push(PoseRole::Player);
    }
    if observe_legacy_camera_object {
        roles.push(PoseRole::Camera);
    }
    for role in roles {
        let matching = targets
            .iter()
            .filter(|target| target.role == role)
            .copied()
            .collect::<Vec<_>>();
        let role_label = match role {
            PoseRole::Camera => "camera",
            PoseRole::Player => "player",
        };
        if matching.len() != 1 {
            // Zero matches is normal while GameInstance.systemInstances is still initializing;
            // `finish` reports a role-local RTTI-missing blocker only if no later enumeration
            // ever finds the exact system. Multiple matches are immediately ambiguous.
            if matching.len() > 1 {
                session.record_discovery_blocker(
                    &format!("{role_label}_system_snapshot_target_not_unique"),
                    format!(
                        "expected one exact {role_label} system object, observed {}",
                        matching.len()
                    ),
                );
            }
            continue;
        }
        let target = matching[0];
        let player_transform = if role == PoseRole::Player {
            let resolved = match *player_cache {
                Some(cached) => read_cached_player_world_transform(session, target, cached)
                    .map(|snapshot| (snapshot, cached))
                    .or_else(|_| {
                        *player_cache = None;
                        resolve_unique_player_world_transform(session, target)
                    }),
                None => resolve_unique_player_world_transform(session, target),
            };
            match resolved {
                Ok((transform, cache)) => {
                    *player_cache = Some(cache);
                    Some(transform)
                }
                Err(error) => {
                    *player_cache = None;
                    session.record_discovery_blocker(
                        "player_world_transform_read_only_chain_not_unique",
                        error,
                    );
                    continue;
                }
            }
        } else {
            None
        };
        let camera_transform = if role == PoseRole::Camera {
            match resolve_unique_camera_world_transform(session, target) {
                Ok(transform) => Some(transform),
                Err(error) => {
                    session.record_discovery_blocker(
                        "camera_world_transform_read_only_chain_not_unique",
                        error,
                    );
                    continue;
                }
            }
        } else {
            None
        };
        let (resolver_id, type_name, object_tag, bytes, extra_offsets) = match role {
            PoseRole::Camera => {
                let transform = camera_transform.expect("camera transform was resolved above");
                (
                    CAMERA_WORLD_TRANSFORM_RESOLVER_ID,
                    "gameCameraSystem.entBaseCameraComponent.WorldTransform",
                    transform.object_tag,
                    transform.bytes,
                    vec![
                        transform.camera_system_field_offset,
                        ENT_PLACED_COMPONENT_WORLD_TRANSFORM_OFFSET as u32,
                    ],
                )
            }
            PoseRole::Player => {
                let transform = player_transform.expect("player transform was resolved above");
                (
                    PLAYER_WORLD_TRANSFORM_RESOLVER_ID,
                    "PlayerPuppet.entIPlacedComponent.WorldTransform",
                    transform.object_tag,
                    transform.bytes,
                    vec![
                        transform.system_field_offset,
                        ENT_ENTITY_TRANSFORM_COMPONENT_OFFSET as u32,
                        ENT_PLACED_COMPONENT_WORLD_TRANSFORM_OFFSET as u32,
                    ],
                )
            }
        };
        session.clear_provisional_discovery_blocker(&format!(
            "{role_label}_world_transform_read_only_chain_not_unique"
        ));
        let entry_offset = target
            .system_index
            .saturating_mul(SYSTEM_ENTRY_BYTES as u32);
        let mut pointer_offsets = vec![
            0,
            CGAME_ENGINE_FRAMEWORK_OFFSET as u32,
            FRAMEWORK_GAME_INSTANCE_OFFSET as u32,
            SYSTEM_ARRAY_OFFSET as u32,
            entry_offset,
        ];
        pointer_offsets.extend(extra_offsets);
        if !session.has_resolver(resolver_id) {
            let anchor_rva = session
                .address_catalog()
                .retained_symbols
                .iter()
                .find(|symbol| symbol.symbol == CP2077_CGAME_ENGINE_SYMBOL)
                .and_then(|symbol| symbol.rva);
            let resolver = ResolverObservation {
                resolver_id: resolver_id.into(),
                role,
                source: RuntimeResolverSource::FirstPartyRttiGameSystem,
                anchor_symbol: Some(CP2077_CGAME_ENGINE_SYMBOL.into()),
                anchor_rva,
                pointer_offsets: pointer_offsets.clone(),
                object_type_name: Some(type_name.into()),
                object_type_bytes: Some(bytes.len() as u32),
                hook_abi: HookAbiEvidence::NotApplicable,
            };
            if let Err(error) = session.register_resolver(resolver) {
                session.record_discovery_blocker(
                    &format!("{role_label}_system_resolver_registration_failed"),
                    error,
                );
                continue;
            }
        } else if !session.resolver_has_pointer_offsets(resolver_id, &pointer_offsets) {
            session.record_discovery_blocker(
                &format!("{role_label}_system_resolver_chain_drift"),
                "read-only object chain offsets changed within one process launch",
            );
            continue;
        }
        let sample_qpc = current_qpc();
        if role == PoseRole::Player {
            session.record_discovery_blocker(
                "player_snapshot_asynchronous_to_selected_present",
                "player memory is sampled on the worker after Present and cannot prove exact same-Present pose",
            );
        }
        match session.observe_snapshot(SnapshotObservationInput {
            role,
            resolver_id,
            object_tag,
            present_seq,
            // The read happens on the worker after Present. Preserve the actual
            // sample time instead of relabeling asynchronous memory as same-Present data.
            qpc: sample_qpc,
            // The second smoke is an operator-directed walk/turn run. This label is only a
            // discovery grouping; without explicit calibration markers it proves no axis/scale.
            scenario: DiscoveryScenario::OnFootTranslation,
            bytes: &bytes,
        }) {
            Ok(_) => {
                outcome.any_observed = true;
                outcome.player_observed |= role == PoseRole::Player;
            }
            Err(error) => session
                .record_discovery_blocker(&format!("{role_label}_system_snapshot_rejected"), error),
        }
    }
    outcome
}

/// 单次 `VirtualQuery` 的可读裁定：通过则返回该 region 的 `[base, limit)`。
///
/// 这是可读性裁定式**唯一**的定义处，`RegionMemo` 的缓存命中分支必须与它等价。
fn query_readable_region(address: usize) -> Option<(usize, usize)> {
    let mut info = MEMORY_BASIC_INFORMATION::default();
    let queried = crate::startup_timing::virtual_query(|| unsafe {
        VirtualQuery(
            Some(address as *const c_void),
            &mut info,
            size_of::<MEMORY_BASIC_INFORMATION>(),
        )
    });
    if queried != size_of::<MEMORY_BASIC_INFORMATION>()
        || info.State != MEM_COMMIT
        || info.Protect.contains(PAGE_GUARD)
        || info.Protect.contains(PAGE_NOACCESS)
    {
        return None;
    }
    let base_protection = info.Protect.0 & 0xff;
    if ![
        PAGE_READONLY.0,
        PAGE_READWRITE.0,
        PAGE_WRITECOPY.0,
        PAGE_EXECUTE_READ.0,
        PAGE_EXECUTE_READWRITE.0,
        PAGE_EXECUTE_WRITECOPY.0,
    ]
    .contains(&base_protection)
    {
        return None;
    }
    let region_start = info.BaseAddress as usize;
    let region_end = region_start.checked_add(info.RegionSize)?;
    Some((region_start, region_end))
}

fn add_address(base: usize, offset: usize) -> Result<usize, String> {
    base.checked_add(offset)
        .ok_or_else(|| "process-memory address overflow".into())
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

fn classify_native_system_identity(
    name_hash: u64,
    type_bytes: u32,
) -> (Option<&'static str>, bool, bool) {
    match (name_hash, type_bytes) {
        (CP2077_PLAYER_SYSTEM_CNAME_HASH, CP2077_PLAYER_SYSTEM_TYPE_BYTES) => {
            (Some("cpPlayerSystem"), true, false)
        }
        (CP2077_CAMERA_SYSTEM_CNAME_HASH, CP2077_CAMERA_SYSTEM_TYPE_BYTES) => {
            (Some("gameCameraSystem"), false, true)
        }
        _ => (None, false, false),
    }
}

fn enumerate_current_process_systems(
    session: &mut Cp2077PoseRuntimeSession,
    poll_ordinal: u64,
    qpc: i64,
) -> Result<SystemEnumerationResult, String> {
    // 一次枚举就是一次有界游走：声明的每个系统对象都要读句柄、引用计数、vtable，
    // 再走一段 RTTI。这些对象成批落在同几段内存里，共用一次 region 裁定。
    let mut memo = RegionMemo::new();
    let module = unsafe { GetModuleHandleW(None) }
        .map_err(|error| format!("locate CP2077 module for system enumeration failed: {error}"))?;
    let mut module_info = MODULEINFO::default();
    if !unsafe {
        K32GetModuleInformation(
            GetCurrentProcess(),
            module,
            &mut module_info,
            size_of::<MODULEINFO>() as u32,
        )
    }
    .as_bool()
    {
        return Err("query CP2077 module for system enumeration failed".into());
    }
    let module_base = module_info.lpBaseOfDll as usize;
    let module_end = module_base.saturating_add(module_info.SizeOfImage as usize);
    let engine_rva = session
        .address_catalog()
        .retained_symbols
        .iter()
        .find(|symbol| symbol.symbol == CP2077_CGAME_ENGINE_SYMBOL)
        .and_then(|symbol| symbol.rva)
        .ok_or("exact CGameEngine RVA is absent")?;
    let mut observation = RuntimeSystemEnumerationObservation {
        poll_ordinal,
        qpc,
        cgame_engine_resolved: false,
        framework_resolved: false,
        game_instance_resolved: false,
        declared_system_count: 0,
        candidates_truncated: false,
        candidates: Vec::new(),
        blocker: None,
    };
    let mut snapshot_targets = Vec::new();
    let engine_global = add_address(module_base, engine_rva as usize)?;
    let engine = match memo.value::<usize>(engine_global) {
        Ok(value) if value != 0 => value,
        _ => {
            observation.blocker = Some("CGameEngine global pointer is not initialized".into());
            return Ok(SystemEnumerationResult {
                observation,
                snapshot_targets,
            });
        }
    };
    observation.cgame_engine_resolved = true;
    let framework = match memo.value::<usize>(add_address(engine, CGAME_ENGINE_FRAMEWORK_OFFSET)?) {
        Ok(value) if value != 0 => value,
        _ => {
            observation.blocker = Some("CGameEngine.framework is not initialized".into());
            return Ok(SystemEnumerationResult {
                observation,
                snapshot_targets,
            });
        }
    };
    observation.framework_resolved = true;
    let game_instance =
        match memo.value::<usize>(add_address(framework, FRAMEWORK_GAME_INSTANCE_OFFSET)?) {
            Ok(value) if value != 0 => value,
            _ => {
                observation.blocker = Some("framework.gameInstance is not initialized".into());
                return Ok(SystemEnumerationResult {
                    observation,
                    snapshot_targets,
                });
            }
        };
    observation.game_instance_resolved = true;
    let array_header = add_address(game_instance, SYSTEM_ARRAY_OFFSET)?;
    let (array, count, capacity) = match memo
        .value::<RedengineDynArrayHeader>(array_header)
        .ok()
        .and_then(admit_system_array_header)
    {
        Some(header) => header,
        _ => {
            observation.blocker = Some("GameInstance.systemInstances header is invalid".into());
            return Ok(SystemEnumerationResult {
                observation,
                snapshot_targets,
            });
        }
    };
    observation.declared_system_count = count;
    let admitted = usize::try_from(count)
        .unwrap_or(usize::MAX)
        .min(crate::cp2077_pose_runtime::MAX_SYSTEM_CANDIDATES_PER_ENUMERATION);
    observation.candidates_truncated = usize::try_from(count).unwrap_or(usize::MAX) > admitted;
    let admitted_bytes = admitted
        .checked_mul(SYSTEM_ENTRY_BYTES)
        .ok_or("system candidate array byte count overflow")?;
    if admitted != 0 && !memo.readable(array, admitted_bytes) {
        observation.blocker = Some("GameInstance.systemInstances array is not readable".into());
        return Ok(SystemEnumerationResult {
            observation,
            snapshot_targets,
        });
    }
    for index in 0..admitted {
        let entry_offset = index
            .checked_mul(SYSTEM_ENTRY_BYTES)
            .ok_or("system candidate offset overflow")?;
        let entry = add_address(array, entry_offset)?;
        let object = match memo.value::<usize>(entry) {
            Ok(value) if value != 0 => value,
            _ => continue,
        };
        let refcount = match memo.value::<usize>(add_address(entry, size_of::<usize>())?) {
            Ok(value) if value != 0 => value,
            _ => continue,
        };
        if !memo.readable(object, size_of::<usize>())
            || !memo.readable(refcount, size_of::<[u32; 2]>())
        {
            continue;
        }
        let [strong_refs, weak_refs] = match memo.value::<[u32; 2]>(refcount) {
            Ok(counts)
                if counts[0] != 0
                    && counts[1] != 0
                    && counts[0] <= MAX_PLAUSIBLE_REFERENCE_COUNT
                    && counts[1] <= MAX_PLAUSIBLE_REFERENCE_COUNT =>
            {
                counts
            }
            _ => continue,
        };
        let vtable = match memo.value::<usize>(object) {
            Ok(value)
                if value >= module_base
                    && value < module_end
                    && memo.readable(value, size_of::<usize>()) =>
            {
                value
            }
            _ => continue,
        };
        let object_tag = session.register_ephemeral_object(object)?;
        let vtable_module_rva = Some((vtable - module_base) as u32);
        let native_type = memo
            .value::<usize>(add_address(object, SCRIPTABLE_NATIVE_TYPE_OFFSET)?)
            .ok()
            .filter(|address| *address != 0);
        let native_identity = native_type.and_then(|address| {
            let class_vtable = memo.value::<usize>(address).ok()?;
            if class_vtable < module_base
                || class_vtable >= module_end
                || !memo.readable(class_vtable, size_of::<usize>())
            {
                return None;
            }
            let name_address = add_address(address, CCLASS_NAME_OFFSET).ok()?;
            let size_address = add_address(address, CCLASS_SIZE_OFFSET).ok()?;
            let name_hash = memo.value::<u64>(name_address).ok()?;
            let type_bytes = memo.value::<u32>(size_address).ok()?;
            Some((name_hash, type_bytes))
        });
        let (native_type_name, player_system_type_match, camera_system_type_match) =
            native_identity
                .map(|(hash, bytes)| classify_native_system_identity(hash, bytes))
                .unwrap_or((None, false, false));
        let native_type_bytes = native_type_name.map(|_| native_identity.unwrap().1);
        if player_system_type_match {
            snapshot_targets.push(SystemSnapshotTarget {
                object,
                role: PoseRole::Player,
                system_index: index as u32,
                type_bytes: CP2077_PLAYER_SYSTEM_TYPE_BYTES,
            });
        }
        if camera_system_type_match {
            snapshot_targets.push(SystemSnapshotTarget {
                object,
                role: PoseRole::Camera,
                system_index: index as u32,
                type_bytes: CP2077_CAMERA_SYSTEM_TYPE_BYTES,
            });
        }
        observation
            .candidates
            .push(RuntimeSystemCandidateObservation {
                system_index: index as u32,
                object_tag,
                handle_instance_readable: true,
                handle_refcount_readable: true,
                handle_strong_ref_count: strong_refs,
                handle_weak_ref_count: weak_refs,
                object_vtable_readable: true,
                native_type_descriptor_readable: native_identity.is_some(),
                vtable_module_rva,
                // The label comes only from an exact CName hash+size pair. No engine function,
                // virtual call, string pointer or third-party runtime is invoked.
                native_type_name: native_type_name.map(str::to_string),
                native_type_bytes,
                player_system_type_match,
                camera_system_type_match,
            });
    }
    if observation.candidates.is_empty() {
        observation.blocker = Some(format!(
            "GameInstance.systemInstances declared {count}/{capacity} but no readable object candidates"
        ));
    } else {
        observation.blocker = Some(
            "system candidates use read-only nativeType CName/size identity; live Player owner and camera field chains remain unproven"
                .into(),
        );
    }
    Ok(SystemEnumerationResult {
        observation,
        snapshot_targets,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn formal_target_enumeration(
        candidates_truncated: bool,
        blocker: Option<&str>,
        snapshot_targets: Vec<SystemSnapshotTarget>,
    ) -> SystemEnumerationResult {
        SystemEnumerationResult {
            observation: RuntimeSystemEnumerationObservation {
                poll_ordinal: 1,
                qpc: 2,
                cgame_engine_resolved: true,
                framework_resolved: true,
                game_instance_resolved: true,
                declared_system_count: snapshot_targets.len() as u32,
                candidates_truncated,
                candidates: Vec::new(),
                blocker: blocker.map(str::to_owned),
            },
            snapshot_targets,
        }
    }

    #[test]
    fn formal_player_selection_continues_past_discovery_only_blocker() {
        let player = SystemSnapshotTarget {
            object: 0x1000,
            role: PoseRole::Player,
            system_index: 23,
            type_bytes: CP2077_PLAYER_SYSTEM_TYPE_BYTES,
        };
        let camera = SystemSnapshotTarget {
            object: 0x2000,
            role: PoseRole::Camera,
            system_index: 31,
            type_bytes: CP2077_CAMERA_SYSTEM_TYPE_BYTES,
        };
        let selected = select_formal_player_target(formal_target_enumeration(
            false,
            Some("live Player owner and camera field chains remain unproven"),
            vec![player, camera],
        ))
        .expect("the formal resolver must perform the live-chain proof itself");

        assert_eq!(selected.object, player.object);
        assert_eq!(selected.system_index, player.system_index);
    }

    #[test]
    fn formal_player_selection_still_rejects_missing_or_truncated_candidates() {
        let missing = select_formal_player_target(formal_target_enumeration(
            false,
            Some("system identity unavailable"),
            Vec::new(),
        ))
        .expect_err("a missing exact cpPlayerSystem must stay fail-closed");
        assert!(missing.contains("observed 0"));
        assert!(missing.contains("system identity unavailable"));

        let player = SystemSnapshotTarget {
            object: 0x1000,
            role: PoseRole::Player,
            system_index: 23,
            type_bytes: CP2077_PLAYER_SYSTEM_TYPE_BYTES,
        };
        let truncated =
            select_formal_player_target(formal_target_enumeration(true, None, vec![player]))
                .expect_err("a truncated enumeration cannot prove uniqueness");
        assert!(truncated.contains("truncated"));
    }

    fn fnv1a64(value: &str) -> u64 {
        value.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x100_0000_01b3)
        })
    }

    #[test]
    fn exact_object_route_cname_hashes_match_red4ext_type_names() {
        assert_eq!(fnv1a64("PlayerPuppet"), PLAYER_PUPPET_CNAME_HASH);
        assert_eq!(fnv1a64("gamePuppet"), GAME_PUPPET_CNAME_HASH);
        assert_eq!(
            fnv1a64("entIPlacedComponent"),
            ENT_PLACED_COMPONENT_CNAME_HASH
        );
        assert_eq!(
            fnv1a64("entBaseCameraComponent"),
            ENT_BASE_CAMERA_COMPONENT_CNAME_HASH
        );
        assert_eq!(
            fnv1a64("gameCameraComponent"),
            GAME_CAMERA_COMPONENT_CNAME_HASH
        );
    }

    #[test]
    fn red4ext_231_player_world_transform_layout_is_pinned() {
        assert_eq!(ENT_ENTITY_TRANSFORM_COMPONENT_OFFSET, 0xb0);
        assert_eq!(ENT_COMPONENT_OWNER_OFFSET, 0x50);
        assert_eq!(ENT_PLACED_COMPONENT_WORLD_TRANSFORM_OFFSET, 0xe0);
        assert_eq!(WORLD_TRANSFORM_BYTES, 0x20);
    }

    #[test]
    fn redengine_dyn_array_header_is_entries_capacity_size() {
        assert_eq!(size_of::<RedengineDynArrayHeader>(), 0x10);
        assert_eq!(std::mem::offset_of!(RedengineDynArrayHeader, entries), 0x00);
        assert_eq!(
            std::mem::offset_of!(RedengineDynArrayHeader, capacity),
            0x08
        );
        assert_eq!(std::mem::offset_of!(RedengineDynArrayHeader, size), 0x0c);

        assert_eq!(
            admit_system_array_header(RedengineDynArrayHeader {
                entries: 0x1000,
                capacity: 128,
                size: 73,
            }),
            Some((0x1000, 73, 128))
        );
        assert!(
            admit_system_array_header(RedengineDynArrayHeader {
                entries: 0x1000,
                capacity: 73,
                size: 128,
            })
            .is_none()
        );
    }

    #[test]
    fn activation_is_exact_cp_early_and_launch_id_gated() {
        let id = "0123456789abcdef0123456789abcdef";
        let exact = classify_activation(true, CP2077_EXE_SHA256, true, Some(id));
        assert!(exact.enabled && exact.exact_cp2077 && exact.early_d3d12);
        assert!(exact.launch_id_valid);

        assert!(!classify_activation(false, CP2077_EXE_SHA256, false, None).enabled);
        assert!(!classify_activation(true, "sha256:other", true, Some(id)).exact_cp2077);
        assert!(!classify_activation(true, CP2077_EXE_SHA256, false, Some(id)).early_d3d12);
        assert!(!classify_activation(true, CP2077_EXE_SHA256, true, Some("ABC")).launch_id_valid);
    }

    #[test]
    fn raw_demo_pose_paths_are_config_parent_and_exact_stop_bound() {
        let output =
            PathBuf::from(r"C:\Users\operator\AppData\Local\DataMiner\depth-raw-demo\cp2077\run-a");
        let config = output.join("raw-depth-demo-config.json");
        let stop = output.join(crate::raw_depth_demo::STOP_FILE);
        assert!(validate_raw_demo_pose_paths(&config, &output, &stop).is_ok());

        assert!(
            validate_raw_demo_pose_paths(
                &output.join("nested").join("raw-depth-demo-config.json"),
                &output,
                &stop,
            )
            .is_err()
        );
        assert!(
            validate_raw_demo_pose_paths(
                &config,
                &output,
                &output.join("discovery-stop-request.json"),
            )
            .is_err()
        );
    }

    #[test]
    fn raw_demo_pose_stop_rejects_launch_or_output_drift() {
        let launch = "0123456789abcdef0123456789abcdef";
        let output = PathBuf::from(r"C:\private\cp2077\run-a");
        assert!(validate_raw_demo_stop_binding(launch, &output, launch, &output).is_ok());
        assert!(
            validate_raw_demo_stop_binding(
                launch,
                &output,
                "fedcba9876543210fedcba9876543210",
                &output,
            )
            .is_err()
        );
        assert!(
            validate_raw_demo_stop_binding(
                launch,
                &output,
                launch,
                &PathBuf::from(r"C:\private\cp2077\run-b"),
            )
            .is_err()
        );
    }

    #[test]
    fn raw_demo_pose_snapshot_cadence_can_cover_coordinator_minimum() {
        const COORDINATOR_MIN_PLAYER_POSE_SAMPLES: u64 = 120;
        const CONSERVATIVE_ENUMERATION_BLOCK_MS: u64 = 2_000;
        const RECORDING_WINDOW_MS: u64 = 64_000;
        let raw_stop = WorkerStop::RawDemoSignal(Arc::new(AtomicBool::new(false)));
        let discovery_stop = WorkerStop::DiscoveryFile(PathBuf::from(
            r"C:\private\cp2077\discovery-stop-request.json",
        ));
        assert_eq!(
            raw_stop.object_snapshot_interval(),
            RAW_DEMO_OBJECT_SNAPSHOT_INTERVAL
        );
        assert_eq!(
            discovery_stop.object_snapshot_interval(),
            DISCOVERY_OBJECT_SNAPSHOT_INTERVAL
        );
        assert_eq!(
            raw_stop.system_refresh_interval(true),
            RAW_DEMO_SYSTEM_REFRESH_INTERVAL
        );
        assert_eq!(
            raw_stop.system_refresh_interval(false),
            SYSTEM_ENUMERATION_RETRY_INTERVAL
        );
        let refresh_ms = u64::try_from(RAW_DEMO_SYSTEM_REFRESH_INTERVAL.as_millis()).unwrap();
        let maximum_stable_refreshes = RECORDING_WINDOW_MS.div_ceil(refresh_ms);
        let bounded_read_budget_ms = RECORDING_WINDOW_MS.saturating_sub(
            maximum_stable_refreshes.saturating_mul(CONSERVATIVE_ENUMERATION_BLOCK_MS),
        );
        let theoretical_raw_samples = bounded_read_budget_ms
            / u64::try_from(raw_stop.object_snapshot_interval().as_millis()).unwrap();
        // One initial exact enumeration is conservatively charged against the
        // window. Stable cached sampling does not schedule another periodic
        // enumeration during either supported raw-demo duration.
        assert_eq!(maximum_stable_refreshes, 1);
        assert_eq!(theoretical_raw_samples, 620);
        assert!(theoretical_raw_samples >= COORDINATOR_MIN_PLAYER_POSE_SAMPLES);
    }

    #[test]
    fn cached_player_target_requires_exactly_one_current_system() {
        let player = |object| SystemSnapshotTarget {
            object,
            role: PoseRole::Player,
            system_index: 1,
            type_bytes: CP2077_PLAYER_SYSTEM_TYPE_BYTES,
        };
        assert!(!unique_role_target(&[], PoseRole::Player));
        assert!(unique_role_target(&[player(0x1000)], PoseRole::Player));
        assert!(!unique_role_target(
            &[player(0x1000), player(0x2000)],
            PoseRole::Player
        ));
    }

    #[test]
    fn cached_sampling_never_reuses_or_reorders_present_identity() {
        assert!(newer_present_identity(None, (100, 1_000)));
        assert!(!newer_present_identity(Some((100, 1_000)), (100, 1_000)));
        assert!(!newer_present_identity(Some((100, 1_000)), (99, 1_001)));
        assert!(!newer_present_identity(Some((100, 1_000)), (101, 999)));
        assert!(newer_present_identity(Some((100, 1_000)), (101, 1_001)));
    }

    #[test]
    fn config_validation_rejects_any_ready_or_non_local_output_contract() {
        let valid = DiscoveryConfig {
            schema: DISCOVERY_CONFIG_SCHEMA_V1.into(),
            output_dir: ".".into(),
            recording_ready: false,
        };
        assert!(validate_discovery_config(&valid).is_ok());
        assert!(
            validate_discovery_config(&DiscoveryConfig {
                recording_ready: true,
                ..valid
            })
            .is_err()
        );
        assert!(
            validate_discovery_config(&DiscoveryConfig {
                schema: DISCOVERY_CONFIG_SCHEMA_V1.into(),
                output_dir: "evidence".into(),
                recording_ready: false,
            })
            .is_err()
        );
    }

    #[test]
    fn native_system_identity_requires_exact_cname_hash_and_size_pair() {
        assert_eq!(
            classify_native_system_identity(
                CP2077_PLAYER_SYSTEM_CNAME_HASH,
                CP2077_PLAYER_SYSTEM_TYPE_BYTES
            ),
            (Some("cpPlayerSystem"), true, false)
        );
        assert_eq!(
            classify_native_system_identity(
                CP2077_CAMERA_SYSTEM_CNAME_HASH,
                CP2077_CAMERA_SYSTEM_TYPE_BYTES
            ),
            (Some("gameCameraSystem"), false, true)
        );
        assert_eq!(
            classify_native_system_identity(
                CP2077_PLAYER_SYSTEM_CNAME_HASH,
                CP2077_PLAYER_SYSTEM_TYPE_BYTES - 1
            ),
            (None, false, false)
        );
    }

    #[test]
    fn player_chain_selection_deduplicates_offsets_but_rejects_multiple_objects() {
        let one = unique_player_chain(BTreeMap::from([(
            (0x1000, 0x2000),
            BTreeSet::from([0x80, 0x180]),
        )]))
        .unwrap();
        assert_eq!(one.player_object, 0x1000);
        assert_eq!(one.transform_component, 0x2000);
        assert_eq!(one.system_field_offsets, BTreeSet::from([0x80, 0x180]));
        assert_eq!(
            canonical_system_field_offset(&BTreeSet::from([0x180, 0x80])).unwrap(),
            0x80
        );
        assert!(canonical_system_field_offset(&BTreeSet::new()).is_err());

        assert!(
            unique_player_chain(BTreeMap::from([
                ((0x1000, 0x2000), BTreeSet::from([0x80])),
                ((0x3000, 0x4000), BTreeSet::from([0x180])),
            ]))
            .is_err()
        );
        assert!(unique_player_chain(BTreeMap::new()).is_err());
    }

    #[test]
    fn exact_player_world_transform_shape_requires_one_candidate_at_zero() {
        let mut bytes = vec![0_u8; WORLD_TRANSFORM_BYTES];
        for (axis, value) in [128_i32, -256, 512].into_iter().enumerate() {
            bytes[axis * 4..axis * 4 + 4].copy_from_slice(&value.to_le_bytes());
        }
        bytes[0x1c..0x20].copy_from_slice(&1.0_f32.to_le_bytes());
        assert!(exact_world_transform_bytes_are_structural(&bytes));

        bytes[0x1c..0x20].copy_from_slice(&0.0_f32.to_le_bytes());
        assert!(!exact_world_transform_bytes_are_structural(&bytes));
        assert!(!exact_world_transform_bytes_are_structural(
            &bytes[..WORLD_TRANSFORM_BYTES - 1]
        ));
    }

    #[test]
    fn direct_object_selection_deduplicates_offsets_but_rejects_multiple_objects() {
        let (object, offsets) = unique_direct_object(
            BTreeMap::from([(0x1000, BTreeSet::from([0x80, 0x180]))]),
            "camera-test",
        )
        .unwrap();
        assert_eq!(object, 0x1000);
        assert_eq!(offsets, BTreeSet::from([0x80, 0x180]));
        assert!(
            unique_direct_object(
                BTreeMap::from([
                    (0x1000, BTreeSet::from([0x80])),
                    (0x2000, BTreeSet::from([0x180])),
                ]),
                "camera-test",
            )
            .is_err()
        );
    }

    // 肯定没被映射的一段地址：用来验证「缓存没命中就必须真查一次，并且真查会拒」。
    const UNMAPPED_BASE: usize = 0x0000_6000_0000_0000;

    /// 一整页都落在同一个 region 里（`VirtualQuery` 的 region 本来就是按页成段的），
    /// 所以把测试用的「对象」对齐到页首、字段全落在这一页内，断言才是确定的——
    /// 换成随便一个 `Vec` 的首地址，它可能正好坐在某个 region 的末尾。
    fn page_aligned_object() -> (Vec<u8>, usize) {
        let backing = vec![0_u8; 3 * POINTER_SCAN_PAGE_BYTES];
        let base = (backing.as_ptr() as usize).next_multiple_of(POINTER_SCAN_PAGE_BYTES);
        let offset = base - backing.as_ptr() as usize;
        (backing, offset)
    }

    #[test]
    fn region_memo_serves_neighbouring_fields_from_one_query() {
        let mut memo = RegionMemo::new();
        let (backing, offset) = page_aligned_object();
        let base = backing.as_ptr() as usize + offset;
        assert!(memo.readable(base, 8));
        assert_eq!((memo.queries, memo.hits), (1, 0));
        // 同一对象的相邻字段：命中缓存，不再发起 VirtualQuery。
        assert!(memo.readable(base + SCRIPTABLE_NATIVE_TYPE_OFFSET, 8));
        assert!(memo.readable(base + CCLASS_SIZE_OFFSET, 4));
        assert_eq!((memo.queries, memo.hits), (1, 2));
    }

    #[test]
    fn region_memo_reads_real_bytes_through_a_cache_hit() {
        let mut memo = RegionMemo::new();
        let (mut backing, offset) = page_aligned_object();
        backing[offset + 0x40..offset + 0x48]
            .copy_from_slice(&0x0123_4567_89ab_cdef_u64.to_le_bytes());
        let base = backing.as_ptr() as usize + offset;
        assert!(memo.readable(base, 8));
        let value = memo.value::<u64>(base + 0x40).unwrap();
        assert_eq!(value, 0x0123_4567_89ab_cdef);
        // 命中缓存省掉的只是 VirtualQuery，数据仍然是 ReadProcessMemory 真读回来的。
        assert_eq!((memo.queries, memo.hits), (1, 1));
    }

    #[test]
    fn region_memo_rejects_null_and_empty_spans_without_querying() {
        let mut memo = RegionMemo::new();
        assert!(!memo.readable(0, 8));
        assert!(!memo.readable(0x1000, 0));
        assert!(!memo.readable(usize::MAX, 8));
        assert_eq!(memo.queries, 0);
        assert!(memo.value::<u64>(0).is_err());
        assert!(memo.bytes(0x1000, 0).is_err());
        assert!(memo.bytes(0x1000, MAX_OBJECT_SNAPSHOT_BYTES + 1).is_err());
    }

    #[test]
    fn region_memo_never_stretches_a_cached_region_past_its_limit() {
        let mut memo = RegionMemo::new();
        memo.remember(UNMAPPED_BASE, UNMAPPED_BASE + 0x1000);
        // 整段落在已裁定 region 内：命中。
        assert!(memo.readable(UNMAPPED_BASE, 0x1000));
        assert_eq!((memo.queries, memo.hits), (0, 1));
        // 越过 limit 一个字节就不算命中，必须真查一次——这里真查会拒。
        assert!(!memo.readable(UNMAPPED_BASE, 0x1001));
        assert!(!memo.readable(UNMAPPED_BASE + 0xffc, 8));
        assert_eq!((memo.queries, memo.hits), (2, 1));
    }

    #[test]
    fn region_memo_evicts_the_least_recently_used_region() {
        let mut memo = RegionMemo::new();
        let region = |index: usize| UNMAPPED_BASE + index * 0x10_0000;
        for index in 0..REGION_MEMO_ENTRIES {
            memo.remember(region(index), region(index) + 0x1000);
        }
        assert!(memo.readable(region(0), 8));
        // 再塞一条就挤掉最久没命中的那条。刚命中过的 0 号已挪到队首，被挤掉的是 1 号。
        memo.remember(
            region(REGION_MEMO_ENTRIES),
            region(REGION_MEMO_ENTRIES) + 0x1000,
        );
        let before = memo.queries;
        assert!(memo.readable(region(0), 8));
        assert!(memo.readable(region(REGION_MEMO_ENTRIES), 8));
        assert_eq!(memo.queries, before, "队首与新插入的两条都该还在");
        // 1 号已被挤掉：不再命中，于是真查一次并被拒。
        assert!(!memo.readable(region(1), 8));
        assert_eq!(memo.queries, before + 1);
    }

    #[test]
    fn current_process_snapshot_copy_is_exact_and_hard_bounded() {
        let mut memo = RegionMemo::new();
        let source = vec![0x5a_u8; 4_096];
        let copied = memo.bytes(source.as_ptr() as usize, source.len()).unwrap();
        assert_eq!(copied, source);
        assert!(
            memo.bytes(source.as_ptr() as usize, MAX_OBJECT_SNAPSHOT_BYTES + 1)
                .is_err()
        );
    }

    #[test]
    fn latest_present_identity_is_not_taken_from_the_bounded_queue_backlog() {
        let latest = LatestPresent::default();
        assert_eq!(latest.load_consistent(), None);
        latest.store(735, 10_000);
        assert_eq!(latest.load_consistent(), Some((735, 10_000)));
        latest.store(736, 10_010);
        assert_eq!(latest.load_consistent(), Some((736, 10_010)));
    }

    #[test]
    fn discovery_present_ingress_is_qpc_limited_without_staling_latest_identity() {
        let latest = LatestPresent::default();
        let frequency = 10_000_000_i64;
        let (sender, receiver) = sync_channel(PRESENT_QUEUE_CAPACITY);
        let mut last_enqueued = 0;
        let mut dropped = 0;
        for ordinal in 1..=1_000_i64 {
            let qpc = ordinal * (frequency / 1_000);
            latest.store(ordinal as u64, qpc);
            if present_enqueue_due(last_enqueued, qpc, frequency) {
                if sender.try_send(ordinal).is_err() {
                    dropped += 1;
                } else {
                    last_enqueued = qpc;
                }
            }
        }
        assert_eq!(latest.load_consistent(), Some((1_000, frequency)));
        assert_eq!(dropped, 0);
        assert_eq!(receiver.try_iter().count(), POSE_INGRESS_HZ as usize);
    }

    #[test]
    fn render_camera_fields_are_joined_to_the_exact_present() {
        use crate::d3d12_render_camera::{
            D3d12RenderDrawObservation, D3d12RenderMatrixObservation,
        };

        let matrix = |kind: &str| D3d12RenderMatrixObservation {
            copy_receipt: None,
            source_tag: 1,
            root_parameter_index: 2,
            cbv_resource_offset: 0,
            matrix_offset: 0,
            transpose: false,
            matrix_kind: kind.into(),
            matrix: [[0.0; 4]; 4],
            translation_slot: None,
            basis_determinant: None,
            clip_w_sign: None,
            projection_a: None,
            projection_b: None,
            vertical_fov_degrees: None,
            horizontal_fov_degrees: None,
            configured_vertical_fov_matches: None,
            handedness_observation: "test".into(),
            axis_mapping: "test".into(),
            physical_source: "mapped-root-cbv".into(),
            stable_source: None,
        };
        let draw = |present_seq, matrices| D3d12RenderDrawObservation {
            pass_context: None,
            pipeline_depth_enable: None,
            pipeline_depth_write: None,
            present_seq,
            present_qpc: present_seq as i64 * 10,
            queue_tag: 1,
            command_list_tag: 2,
            command_list_generation: 3,
            selected_depth_resource_tag: 4,
            selected_depth_resource_epoch: 5,
            selected_depth_descriptor_generation: 6,
            draw_ordinal: 7,
            pass_ordinal: Some(0),
            draw_ordinal_in_pass: Some(7),
            root_signature_sha256: None,
            execute_batch: 1,
            list_index_in_batch: 0,
            depth_copy_execute_batch: Some(1),
            depth_copy_list_index_in_batch: Some(0),
            depth_copy_command_list_tag: None,
            depth_copy_draws_before_in_list: Some(7),
            copy_before_clear_submitted: true,
            selected_present_identity_passed: true,
            physical_source: "mapped-root-cbv".into(),
            matrices,
        };
        let snapshot = crate::d3d12_render_camera::D3d12RenderCameraSnapshot {
            candidate_admitted_for_cross_launch_discovery: true,
            observations: vec![
                draw(735, vec![matrix("view")]),
                draw(735, vec![matrix("projection")]),
                draw(736, vec![matrix("unknown")]),
            ],
            ..Default::default()
        };
        assert_eq!(
            exact_present_render_camera_fields(&snapshot, 735),
            (true, true, true)
        );
        assert_eq!(
            exact_present_render_camera_fields(&snapshot, 736),
            (false, false, false)
        );
    }

    #[test]
    fn non_discovery_present_ingress_is_a_no_op() {
        if DISCOVERY_INGRESS.get().is_none() {
            let backend = crate::d3d12_backend::D3d12ObservationBackend::new();
            observe_present(1, None, &backend);
            assert!(DISCOVERY_INGRESS.get().is_none());
        }
    }
}

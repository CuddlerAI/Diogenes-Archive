use std::collections::HashMap;
use std::ffi::c_void;
use std::fs::{File, OpenOptions};
use std::io::{BufReader, Read};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, MutexGuard as StdMutexGuard, OnceLock, Weak, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, ensure};
use capture_core::input::{
    RAW_INPUT_COMMAND_CAPACITY, RAW_INPUT_CONTROL_MESSAGE, RawInputControl, RawInputKind,
    RawInputPacket, RawInputRecord, RawInputStatus, RawInputThreadCommand,
};
use capture_core::mapping::MappingSnapshot;
use capture_core::profile::CaptureProfile;
use capture_core::recording::{RecordingStopReason, RecordingStopSignal};
use crossbeam_channel::{Receiver, Sender, bounded};
use parking_lot::{Mutex, RwLock};
use sha2::{Digest, Sha256};
use windows::Win32::Foundation::{CloseHandle, FILETIME, HANDLE, HWND, LPARAM, RECT, TRUE, WPARAM};
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
use windows::Win32::Storage::FileSystem::{
    FILE_BASIC_INFO, FILE_FLAG_SEQUENTIAL_SCAN, FILE_ID_INFO, FILE_SHARE_READ, FILE_STANDARD_INFO,
    FileBasicInfo, FileIdInfo, FileStandardInfo, GetFileInformationByHandleEx,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Performance::QueryPerformanceCounter;
use windows::Win32::System::Threading::{
    GetCurrentThreadId, GetProcessTimes, OpenProcess, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
#[cfg(all(test, feature = "dev-catalog"))]
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::Input::{
    HRAWINPUT, MOUSE_MOVE_ABSOLUTE, RAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER, RIDEV_DEVNOTIFY,
    RIDEV_INPUTSINK, RIDEV_REMOVE, RIM_TYPEKEYBOARD, RIM_TYPEMOUSE, RegisterRawInputDevices,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, EVENT_OBJECT_DESTROY,
    EVENT_OBJECT_LOCATIONCHANGE, EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_MINIMIZEEND,
    EVENT_SYSTEM_MINIMIZESTART, EnumWindows, GA_ROOTOWNER, GIDC_REMOVAL, GetAncestor,
    GetClassNameW, GetClientRect, GetForegroundWindow, GetMessageW, GetWindowThreadProcessId,
    HWND_MESSAGE, IsIconic, IsWindow, IsWindowVisible, MSG, PM_NOREMOVE, PeekMessageW,
    PostThreadMessageW, RI_KEY_BREAK, RI_MOUSE_BUTTON_1_DOWN, RI_MOUSE_BUTTON_1_UP,
    RI_MOUSE_BUTTON_2_DOWN, RI_MOUSE_BUTTON_2_UP, RI_MOUSE_BUTTON_3_DOWN, RI_MOUSE_BUTTON_3_UP,
    RI_MOUSE_BUTTON_4_DOWN, RI_MOUSE_BUTTON_4_UP, RI_MOUSE_BUTTON_5_DOWN, RI_MOUSE_BUTTON_5_UP,
    RI_MOUSE_HWHEEL, RI_MOUSE_WHEEL, SetForegroundWindow, TranslateMessage, WINDOW_EX_STYLE,
    WINDOW_STYLE, WINEVENT_OUTOFCONTEXT, WM_INPUT, WM_INPUT_DEVICE_CHANGE, WM_QUIT, WM_USER,
};
use windows::core::{BOOL, PWSTR, w};

use crate::domain::collector::{BoundTargetSummary, GameAdmission, GameSummary, WindowCandidate};
use crate::platform::windows::catalog::{
    CatalogSelection, CatalogStatus, CatalogStore, GameInputContract, GameProfile,
};
use crate::platform::windows::raw_input_reader::RawInputReader;

const FOREGROUND_ACTIVATION_TIMEOUT: Duration = Duration::from_secs(1);
const FOREGROUND_ACTIVATION_POLL_INTERVAL: Duration = Duration::from_millis(10);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct ExecutableFileIdentity {
    volume_serial_number: u64,
    file_id: [u8; 16],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ExecutableFileStamp {
    creation_time: i64,
    last_write_time: i64,
    change_time: i64,
    end_of_file: i64,
    allocation_size: i64,
    file_attributes: u32,
    number_of_links: u32,
    delete_pending: bool,
}

#[derive(Debug)]
struct VerifiedExecutable {
    // Retaining this strict-share handle prevents the verified file from being
    // modified, deleted, or replaced while a candidate/bound target uses it.
    _file: File,
    identity: ExecutableFileIdentity,
    stamp: ExecutableFileStamp,
    sha256: String,
}

#[derive(Default)]
struct ExecutableHashCache {
    entries: Mutex<HashMap<ExecutableFileIdentity, Weak<VerifiedExecutable>>>,
}

#[derive(Debug)]
struct ProcessImageIdentity {
    path: PathBuf,
    creation_time_100ns: u64,
}

impl ExecutableHashCache {
    fn verify(
        &self,
        path: &Path,
        expected_sha256: Option<&str>,
    ) -> anyhow::Result<Arc<VerifiedExecutable>> {
        let file = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ.0)
            .custom_flags(FILE_FLAG_SEQUENTIAL_SCAN.0)
            .open(path)
            .with_context(|| {
                format!(
                    "failed to open {} with immutable verification sharing",
                    path.display()
                )
            })?;
        let (identity, before) = executable_file_metadata(&file)?;
        let mut entries = self.entries.lock();
        entries.retain(|_, entry| entry.strong_count() > 0);
        if let Some(verified) = entries.get(&identity).and_then(Weak::upgrade) {
            ensure!(
                verified.stamp == before,
                "verified executable metadata changed while its identity was retained"
            );
            ensure_expected_executable_hash(expected_sha256, &verified.sha256)?;
            return Ok(verified);
        }

        let sha256 = sha256_file_handle(&file)?;
        let (after_identity, after) = executable_file_metadata(&file)?;
        ensure!(
            identity == after_identity && before == after,
            "executable changed while its SHA-256 was being verified"
        );
        ensure_expected_executable_hash(expected_sha256, &sha256)?;
        let verified = Arc::new(VerifiedExecutable {
            _file: file,
            identity,
            stamp: after,
            sha256,
        });
        entries.insert(identity, Arc::downgrade(&verified));
        Ok(verified)
    }
}

/// 绑定时对游戏程序的只读观察结果（docs/55 §3.1）。
///
/// 观察分两步——`OpenProcess(QUERY_LIMITED)` 取镜像路径与进程创建时刻，然后读
/// 共享打开该文件并全量哈希——两步都可能被受保护进程或文件系统拒绝。**未钉指纹
/// 的游戏拿到多少算多少**：失败降级留痕而不是淘汰候选，采集不中断。钉了指纹的
/// 游戏不参与降级，见 [`WindowTracker::observe_executable`]。
#[derive(Clone, Debug, Default)]
struct ExecutableIdentity {
    /// 进程创建时刻。PID 会被系统回收，`(PID, 创建时刻)` 才是进程实例的唯一
    /// 标识——continuity 靠它发现「游戏崩了又起了一个同名进程」。
    creation_time_100ns: Option<u64>,
    /// 全文件 SHA-256。缺席时 `missing_reason` 必有值，二者严格互斥。
    sha256: Option<String>,
    /// 缺席原因的可读说明：阶段前缀 + 整条错误链。
    ///
    /// **结构上装不下路径**：完整镜像路径带 Windows 用户名，只留在本地日志，
    /// 不进会话记录、不上云（docs/55 §3.1；服务端 0019 的 CHECK 同款正则再拦一道）。
    missing_reason: Option<String>,
    /// 观察期间持有的读共享句柄，防止文件在校验后被替换。
    verified: Option<Arc<VerifiedExecutable>>,
}

/// 身份观察失败。
///
/// `partial` 是失败**之前**已经拿到的证据：L1 成功、读文件失败时，进程创建时刻
/// 仍然有效，丢掉它等于白白把 continuity 降到只剩进程名（docs/55 §3.1
/// 「带着已拿到的部分继续」）。`error` 留给钉了指纹时的致命路径。
struct IdentityProbeFailure {
    reason: String,
    partial: ExecutableIdentity,
    error: anyhow::Error,
}

#[derive(Clone, Debug)]
struct WindowObservation {
    hwnd_value: usize,
    root_owner_value: usize,
    process_id: u32,
    executable_name: String,
    identity: ExecutableIdentity,
    client_width: u32,
    client_height: u32,
    dpi: u32,
    foreground: bool,
}

#[derive(Clone, Debug)]
struct CandidateRecord {
    generation: u64,
    profile: GameProfile,
    observation: WindowObservation,
    candidate_id: String,
}

#[derive(Clone, Debug)]
pub struct BoundTarget {
    pub hwnd_value: usize,
    pub root_owner_value: usize,
    pub process_id: u32,
    /// 实测 build 指纹。受保护进程拒绝观察时为 `None`，改由
    /// [`Self::executable_sha256_missing_reason`] 说明原因（docs/55 §3.1）。
    /// 缺席不影响录制，只让云端的审批 join 匹配不上 → 数据永不可训练。
    pub executable_sha256: Option<String>,
    pub executable_sha256_missing_reason: Option<String>,
    pub client_width: u32,
    pub client_height: u32,
    pub dpi: u32,
    pub capture_profile: CaptureProfile,
    pub mapping: MappingSnapshot,
    pub catalog_selection: CatalogSelection,
    input_contract: GameInputContract,
    pub summary: BoundTargetSummary,
    profile: GameProfile,
    identity: ExecutableIdentity,
}

impl BoundTarget {
    /// Application control keys declared by this session's contract.
    ///
    /// The projector needs the set itself, not the per-event verdict baked into
    /// the stream, so a later contract fix can be replayed over archived RAW.
    #[must_use]
    pub fn control_vks(&self) -> Vec<u16> {
        self.input_contract.control_vks.clone()
    }
}

pub struct WindowTracker {
    catalog: RwLock<CatalogStore>,
    generation: AtomicU64,
    candidates: Mutex<HashMap<String, CandidateRecord>>,
    executable_cache: ExecutableHashCache,
}

/// 一轮扫描里被淘汰的候选窗口的原因。
///
/// 存在的理由很具体：淘汰原因不同，人要做的事完全不同——「进程名对不上」是游戏没开，
/// 「几何不匹配」是分辨率要改。以前两者共用一条静默 `Ok(None)`，最后都变成
/// 「扫描不到窗口」，把人推去查错的方向（docs/57 讨论，2026-07-29）。
#[derive(Default)]
struct ScanRejections {
    /// 钉了指纹但观察失败的窗口数。
    identity_failures: u32,
    /// 最后一个因源几何被拒的窗口的实测客户区尺寸。
    geometry: Option<(u32, u32)>,
}

impl WindowTracker {
    pub fn new() -> anyhow::Result<Self> {
        Self::build()
    }

    /// 目录里有哪些游戏，只由 `shipped-games.json` 派生的内嵌清单回答。
    ///
    /// 原神那条 debug-only 真实游戏测试开关（环境变量 + 额外内嵌 bundle）随该
    /// bundle 一起删除（issue #855 PR 2）：一个只能打开一份已不存在的目录的开关，
    /// 留着就只是一条能被误读成「还有别的加载路径」的死代码。
    fn build() -> anyhow::Result<Self> {
        let catalog = CatalogStore::load()?;
        Ok(Self {
            catalog: RwLock::new(catalog),
            generation: AtomicU64::new(0),
            candidates: Mutex::new(HashMap::new()),
            executable_cache: ExecutableHashCache::default(),
        })
    }

    #[must_use]
    pub fn catalog_status(&self) -> CatalogStatus {
        self.catalog.read().status()
    }

    #[cfg(feature = "depth-lab")]
    pub(crate) fn catalog_selection(&self, game_id: &str) -> anyhow::Result<CatalogSelection> {
        self.catalog.read().selection(game_id)
    }

    /// 用已验签的下发 catalog 替换运行时 store，并让所有旧 discovery 候选立即失效。
    ///
    /// 调用方必须先持有 orchestrator 的控制操作门并确认没有活动采集；这里的写锁只负责
    /// 数据竞争，不替代那条会话级安全门。
    pub fn replace_catalog(&self, catalog: CatalogStore) {
        *self.catalog.write() = catalog;
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.candidates.lock().clear();
    }

    /// 启动门禁：游戏必须在当前已验签目录里有活跃条目（docs/100）。
    pub fn ensure_start_authorized(&self, game_id: &str) -> anyhow::Result<()> {
        self.catalog.read().ensure_start_authorized(game_id)
    }

    #[must_use]
    pub fn games(&self) -> Vec<GameSummary> {
        let catalog = self.catalog.read();
        let mut games = catalog
            .games()
            .map(|game| {
                let capture_profile = catalog
                    .capture(&game.recording_profile_id)
                    .expect("validated game recording profile disappeared");
                let selection = catalog
                    .selection(&game.game_id)
                    .expect("validated catalog selection disappeared");
                GameSummary {
                    game_id: game.game_id.clone(),
                    display_name: game.display_name.clone(),
                    recording_profile_id: game.recording_profile_id.clone(),
                    recording_profile_version: selection.recording.identity.version.clone(),
                    recording_profile_hash: selection.recording.identity.content_hash.clone(),
                    device_profile_id: selection.device.identity.id.clone(),
                    mapping_id: game.mapping_id.clone(),
                    // 引用去版本化之后，会话记的仍是**这次实际用的**那份 mapping 的版本
                    // （docs/57 §3.2：id+version+hash 三元组逐会话记录不变）。
                    mapping_version: selection.mapping.identity.version.clone(),
                    width: capture_profile.width,
                    height: capture_profile.height,
                    fps_num: capture_profile.fps_num,
                    fps_den: capture_profile.fps_den,
                    non_trainable: game.is_non_trainable(),
                    session_contract: selection.session_contract,
                    // A local-catalog-only binary is an explicit non-production
                    // capture lane: it never syncs a server catalog, and every
                    // bundled entry remains non-trainable. Marking it selectable
                    // authorizes only this local UI path; it does not create an
                    // ingest grant. Normal builds still require the authenticated
                    // server overlay below to turn Unknown into Admitted.
                    #[cfg(feature = "local-catalog-only")]
                    admission: GameAdmission::Admitted,
                    #[cfg(not(feature = "local-catalog-only"))]
                    admission: GameAdmission::Unknown,
                    admission_reason: None,
                }
            })
            .collect::<Vec<_>>();
        games.sort_by(|left, right| left.game_id.cmp(&right.game_id));
        games
    }

    pub fn discover(&self, game_id: &str) -> anyhow::Result<Vec<WindowCandidate>> {
        let catalog = self.catalog.read();
        let generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        // Keep the previous verified handles alive just for this scan so an
        // unchanged executable can reuse its digest. The public candidate map
        // is empty immediately, and every early return releases old handles.
        let _previous_candidates = std::mem::take(&mut *self.candidates.lock());
        let game = catalog.game(game_id).cloned().with_context(|| {
            format!("CATALOG_NO_MATCH: game {game_id} is not in the active catalog")
        })?;
        let capture_profile = catalog
            .capture(&game.recording_profile_id)
            .context("selected game references an unavailable recording profile")?;
        // 一次只读的 Tool Help 快照供整轮发现使用：它给出 PID -> exe 文件名，
        // 而按文件名匹配 `executable_names` 是**所有游戏统一的选窗谓词**
        // （docs/55 §3.1）。快照不碰任何目标进程，受保护进程也在其中。
        let process_names = process_name_snapshot()?;
        let mut records = Vec::new();
        let mut rejections = ScanRejections::default();
        for hwnd in enumerate_top_level_windows()? {
            // 窗口类是可选的窄化条件：钉了就必须精确命中，没钉就跳过（#278 起
            // 全车道选填）。它不是身份证据，只是少开几次句柄的预过滤。
            if let Some(expected_window_class) = game.window_class.as_deref()
                && !window_class(hwnd)
                    .is_ok_and(|observed| window_class_matches(expected_window_class, &observed))
            {
                continue;
            }
            let observation = match self.inspect_window_with_process_names(
                hwnd,
                &game,
                capture_profile,
                &process_names,
                &mut rejections,
            ) {
                Ok(Some(observation)) => observation,
                Ok(None) => continue,
                Err(error) => {
                    let _ = error;
                    rejections.identity_failures += 1;
                    tracing::debug!("window candidate was rejected");
                    continue;
                }
            };
            let candidate_id = candidate_id(generation, &observation);
            records.push(CandidateRecord {
                generation,
                profile: game.clone(),
                observation,
                candidate_id,
            });
        }
        // 几何不匹配优先报：它是最常撞上的一条，且修法明确（改分辨率），
        // 而"扫不到窗口"这个笼统结论会把人推去查完全无关的地方。
        if records.is_empty()
            && let Some((width, height)) = rejections.geometry
        {
            self.candidates.lock().clear();
            anyhow::bail!(
                "WINDOW_SOURCE_GEOMETRY_MISMATCH: 找到了游戏窗口，但它的客户区是 \
                 {width}x{height}，本目录要求{}。改 Windows 桌面分辨率（不是游戏里的\
                 渲染分辨率——无边框全屏的窗口大小等于桌面分辨率），然后重新扫描。",
                capture_profile.source_geometry_rule().describe()
            );
        }
        // 只有钉了指纹的 profile 才会走到这里：未钉指纹的观察失败已经在
        // inspect 内部降级成 identity.missing_reason，不再淘汰候选（docs/55 §3.1）。
        if records.is_empty() && rejections.identity_failures > 0 {
            self.candidates.lock().clear();
            anyhow::bail!(
                "matching game window was found but its process identity could not be verified"
            );
        }
        records.sort_by_key(|record| {
            (
                !record.observation.foreground,
                std::cmp::Reverse(
                    u64::from(record.observation.client_width)
                        * u64::from(record.observation.client_height),
                ),
                record.observation.process_id,
                record.observation.hwnd_value,
            )
        });
        let candidates = records
            .iter()
            .map(|record| WindowCandidate {
                candidate_id: record.candidate_id.clone(),
                game_id: record.profile.game_id.clone(),
                display_name: record.profile.display_name.clone(),
                executable_name: record.observation.executable_name.clone(),
                client_width: record.observation.client_width,
                client_height: record.observation.client_height,
                dpi: record.observation.dpi,
                foreground: record.observation.foreground,
                non_trainable: record.profile.is_non_trainable(),
            })
            .collect();
        let mut current = self.candidates.lock();
        current.clear();
        current.extend(
            records
                .into_iter()
                .map(|record| (record.candidate_id.clone(), record)),
        );
        Ok(candidates)
    }

    pub fn bind(&self, candidate_id: &str) -> anyhow::Result<BoundTarget> {
        let catalog = self.catalog.read();
        let record = self
            .candidates
            .lock()
            .get(candidate_id)
            .cloned()
            .context("window candidate expired; refresh the game window list")?;
        ensure!(
            record.generation == self.generation.load(Ordering::Acquire),
            "window candidate belongs to an older discovery generation"
        );
        let capture_profile = catalog
            .capture(&record.profile.recording_profile_id)
            .context("bound game references an unavailable recording profile")?;
        let mapping = catalog
            .mapping(&record.profile.mapping_id)
            .context("bound game references an unavailable mapping")?;
        let catalog_selection = catalog.selection(&record.profile.game_id)?;
        let observation = self
            .inspect_window(
                HWND(record.observation.hwnd_value as *mut _),
                &record.profile,
                capture_profile,
            )?
            .context("window no longer satisfies the selected game profile")?;
        ensure!(
            identity_unchanged(&record.observation, &observation),
            "window identity changed between discovery and binding"
        );
        let target_id = target_id(&record.candidate_id, &observation);
        let summary = BoundTargetSummary {
            target_id,
            game_id: record.profile.game_id.clone(),
            display_name: record.profile.display_name.clone(),
            executable_name: observation.executable_name.clone(),
            executable_sha256: observation.identity.sha256.clone(),
            executable_sha256_missing_reason: observation.identity.missing_reason.clone(),
            process_id: observation.process_id,
            client_width: observation.client_width,
            client_height: observation.client_height,
            dpi: observation.dpi,
            recording_profile_id: record.profile.recording_profile_id.clone(),
            recording_profile_version: catalog_selection.recording.identity.version.clone(),
            recording_profile_hash: catalog_selection.recording.identity.content_hash.clone(),
            device_profile_id: catalog_selection.device.identity.id.clone(),
            device_profile_version: catalog_selection.device.identity.version.clone(),
            device_profile_hash: catalog_selection.device.identity.content_hash.clone(),
            game_profile_version: catalog_selection.game.identity.version.clone(),
            game_profile_hash: catalog_selection.game.identity.content_hash.clone(),
            mapping_id: record.profile.mapping_id.clone(),
            mapping_version: catalog_selection.mapping.identity.version.clone(),
            mapping_hash: catalog_selection.mapping.identity.content_hash.clone(),
            session_contract: catalog_selection.session_contract,
            catalog_id: catalog_selection.catalog.catalog_id.clone(),
            catalog_version: catalog_selection.catalog.catalog_version.clone(),
            catalog_hash: catalog_selection.catalog.content_hash.clone(),
            non_trainable: record.profile.is_non_trainable(),
        };
        Ok(BoundTarget {
            hwnd_value: observation.hwnd_value,
            root_owner_value: observation.root_owner_value,
            process_id: observation.process_id,
            executable_sha256: observation.identity.sha256.clone(),
            executable_sha256_missing_reason: observation.identity.missing_reason.clone(),
            client_width: observation.client_width,
            client_height: observation.client_height,
            dpi: observation.dpi,
            capture_profile: capture_profile.clone(),
            mapping: mapping.clone(),
            catalog_selection,
            input_contract: record.profile.input_contract.clone(),
            summary,
            profile: record.profile,
            identity: observation.identity,
        })
    }

    pub fn revalidate(&self, target: &BoundTarget) -> anyhow::Result<()> {
        let observation = self
            .inspect_window(
                HWND(target.hwnd_value as *mut _),
                &target.profile,
                &target.capture_profile,
            )?
            .context("bound game window is no longer valid")?;
        // continuity 分两层：几何是窗口面（全部游戏一视同仁），身份按绑定时实际
        // 拿到的证据分档（docs/55 §3.2）——同一段代码，不按 game_id 分叉。
        ensure!(
            observation.client_width == target.client_width
                && observation.client_height == target.client_height
                && observation.dpi == target.dpi,
            "bound game window geometry changed"
        );
        ensure!(
            bound_identity_unchanged(target, &observation),
            "bound game window identity changed"
        );
        Ok(())
    }

    pub fn focus_revalidated(&self, target: &BoundTarget) -> anyhow::Result<()> {
        let hwnd = HWND(target.hwnd_value as *mut _);
        ensure!(
            unsafe { IsWindow(Some(hwnd)) }.as_bool()
                && unsafe { IsWindowVisible(hwnd) }.as_bool()
                && !unsafe { IsIconic(hwnd) }.as_bool(),
            "bound game window is no longer available"
        );
        let mut process_id = 0_u32;
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut process_id)) };
        ensure!(
            process_id == target.process_id && root_owner_value(hwnd) == target.root_owner_value,
            "bound game window identity changed before focus"
        );
        // 进程创建时刻只有绑定时拿到过才复查。拿不到就说明这个进程从一开始就
        // 拒绝 OpenProcess——此刻再要一次只会再被拒一次，而 focus 是采集准备
        // 步骤，不是又一道身份门（docs/55 §3.2）。
        if let Some(expected_creation_time) = target.identity.creation_time_100ns {
            ensure!(
                process_creation_time_100ns(process_id)? == expected_creation_time,
                "bound game process was replaced before focus"
            );
        } else {
            let process_names = process_name_snapshot()?;
            let executable_name = window_process_name(hwnd, &process_names)
                .context("bound window no longer has a process snapshot entry")?;
            ensure!(
                executable_name_matches(&target.profile, executable_name),
                "bound window is no longer owned by the configured process name"
            );
        }
        if root_owner_value(unsafe { GetForegroundWindow() }) == target.root_owner_value {
            return Ok(());
        }
        let activation_requested = unsafe { SetForegroundWindow(hwnd) }.as_bool();
        let activated = wait_until(
            FOREGROUND_ACTIVATION_TIMEOUT,
            FOREGROUND_ACTIVATION_POLL_INTERVAL,
            || root_owner_value(unsafe { GetForegroundWindow() }) == target.root_owner_value,
        );
        ensure!(
            activation_requested,
            "Windows refused to activate the bound game window"
        );
        ensure!(
            activated,
            "bound game window did not become foreground within 1 second"
        );
        Ok(())
    }

    /// 观察游戏程序身份（docs/55 §3.1）。
    ///
    /// **钉了指纹的 profile 不参与降级**：否则冒充者只要让自己的 exe 读不出来
    /// （改权限、加独占锁），就能整个跳过 pin 比对——降级会把一道校验变成一道
    /// 可以主动触发的旁路。未钉指纹的游戏才降级，因为它们本来就没有可比对的
    /// 期望值，观察不到只是少一份证据。
    fn observe_executable(
        &self,
        process_id: u32,
        game: &GameProfile,
    ) -> anyhow::Result<ExecutableIdentity> {
        let pinned_sha256 = game.executable_sha256.as_deref();
        match self.measure_executable(process_id, pinned_sha256) {
            Ok(identity) => Ok(identity),
            Err(failure) if pinned_sha256.is_none() => {
                tracing::debug!(
                    reason = %failure.reason,
                    "game executable identity unavailable; recording without a build fingerprint"
                );
                Ok(ExecutableIdentity {
                    missing_reason: Some(failure.reason),
                    ..failure.partial
                })
            }
            Err(failure) => Err(failure.error),
        }
    }

    fn measure_executable(
        &self,
        process_id: u32,
        pinned_sha256: Option<&str>,
    ) -> Result<ExecutableIdentity, IdentityProbeFailure> {
        // 一次 OpenProcess(QUERY_LIMITED)，拿完即放；被拒即终态，永不重试
        // ——句柄请求风暴本身才是异常信号（docs/55 §2.3 探测卫生）。
        let image = process_image_identity(process_id).map_err(|error| IdentityProbeFailure {
            reason: probe_failure_reason("open_process_denied", &error),
            // L1 就被拒，什么都没拿到。
            partial: ExecutableIdentity::default(),
            error,
        })?;
        // L1 拿到了。即使下一步读文件失败，进程创建时刻也必须留下——continuity
        // 靠 (PID, 创建时刻) 发现「游戏崩了又起了一个同名进程」。
        let creation_time_100ns = Some(image.creation_time_100ns);
        let verified = self
            .executable_cache
            .verify(&image.path, pinned_sha256)
            .map_err(|error| IdentityProbeFailure {
                reason: probe_failure_reason("executable_read_failed", &error),
                partial: ExecutableIdentity {
                    creation_time_100ns,
                    ..ExecutableIdentity::default()
                },
                error,
            })?;
        Ok(ExecutableIdentity {
            creation_time_100ns,
            sha256: Some(verified.sha256.clone()),
            missing_reason: None,
            verified: Some(verified),
        })
    }

    fn inspect_window(
        &self,
        hwnd: HWND,
        game: &GameProfile,
        capture_profile: &CaptureProfile,
    ) -> anyhow::Result<Option<WindowObservation>> {
        let process_names = process_name_snapshot()?;
        let mut rejections = ScanRejections::default();
        self.inspect_window_with_process_names(
            hwnd,
            game,
            capture_profile,
            &process_names,
            &mut rejections,
        )
    }

    fn inspect_window_with_process_names(
        &self,
        hwnd: HWND,
        game: &GameProfile,
        capture_profile: &CaptureProfile,
        process_names: &HashMap<u32, String>,
        rejections: &mut ScanRejections,
    ) -> anyhow::Result<Option<WindowObservation>> {
        if !unsafe { IsWindow(Some(hwnd)) }.as_bool()
            || !unsafe { IsWindowVisible(hwnd) }.as_bool()
            || unsafe { IsIconic(hwnd) }.as_bool()
        {
            return Ok(None);
        }
        let mut cloaked = 0_u32;
        unsafe {
            DwmGetWindowAttribute(
                hwnd,
                DWMWA_CLOAKED,
                (&raw mut cloaked).cast(),
                std::mem::size_of::<u32>() as u32,
            )
        }
        .context("DwmGetWindowAttribute(DWMWA_CLOAKED) failed")?;
        if cloaked != 0 {
            return Ok(None);
        }
        // 绑定与续验时重复这道可选的窗口类窄化，使发现阶段与后续阶段用同一组
        // 条件；没钉窗口类的 profile 天然跳过（#278 起全车道选填）。
        if let Some(expected_window_class) = game.window_class.as_deref() {
            let observed_window_class = window_class(hwnd)?;
            if !window_class_matches(expected_window_class, &observed_window_class) {
                return Ok(None);
            }
        }
        let mut process_id = 0_u32;
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut process_id)) };
        if process_id == 0 {
            return Ok(None);
        }
        let root_owner = unsafe { GetAncestor(hwnd, GA_ROOTOWNER) };
        let root_owner = if root_owner.0.is_null() {
            hwnd
        } else {
            root_owner
        };
        let mut root_process_id = 0_u32;
        unsafe { GetWindowThreadProcessId(root_owner, Some(&mut root_process_id)) };
        if root_process_id != process_id {
            return Ok(None);
        }
        // L0：进程快照给的 exe 文件名是统一的选窗谓词。放在打开句柄**之前**，
        // 所以只对名字已经对上的窗口做后续观察——少开句柄本身就是探测卫生。
        let Some(executable_name) = process_names.get(&process_id) else {
            return Ok(None);
        };
        if !executable_name_matches(game, executable_name) {
            return Ok(None);
        }
        let executable_name = executable_name.clone();
        // L1–L3：只读身份观察。未钉指纹时失败降级留痕，钉了指纹时失败即拒绝。
        let identity = self.observe_executable(process_id, game)?;
        let mut client = RECT::default();
        unsafe { GetClientRect(hwnd, &mut client) }.context("GetClientRect failed")?;
        let width = (client.right - client.left).max(0) as u32;
        let height = (client.bottom - client.top).max(0) as u32;
        if !capture_profile.accepts_source_dimensions(width, height) {
            // 这里曾经和"进程名对不上"共用一条静默 `Ok(None)`，于是标注员看到的是
            // 「扫描不到窗口」，去查游戏开没开、进程名对不对，唯独想不到是分辨率
            // ——而这恰恰是最常撞上的一条（目录几何要求客户区精确匹配）。
            // 照 docs/55 那套"降级留痕、如实记录"的先例，把观测到的几何留下来。
            rejections.geometry = Some((width, height));
            tracing::warn!(
                observed_width = width,
                observed_height = height,
                rule = %capture_profile.source_geometry_rule().describe(),
                "game window rejected on source geometry"
            );
            return Ok(None);
        }
        let dpi = unsafe { GetDpiForWindow(hwnd) };
        if dpi == 0 {
            return Ok(None);
        }
        let foreground =
            root_owner_value(unsafe { GetForegroundWindow() }) == root_owner.0 as usize;
        Ok(Some(WindowObservation {
            hwnd_value: hwnd.0 as usize,
            root_owner_value: root_owner.0 as usize,
            process_id,
            executable_name,
            identity,
            client_width: width,
            client_height: height,
            dpi,
            foreground,
        }))
    }
}

fn enumerate_top_level_windows() -> anyhow::Result<Vec<HWND>> {
    unsafe extern "system" fn callback(hwnd: HWND, state: LPARAM) -> BOOL {
        let windows = unsafe { &mut *(state.0 as *mut Vec<HWND>) };
        windows.push(hwnd);
        TRUE
    }
    let mut windows = Vec::new();
    unsafe {
        EnumWindows(
            Some(callback),
            LPARAM((&raw mut windows).cast::<std::ffi::c_void>() as isize),
        )
    }
    .context("EnumWindows failed")?;
    Ok(windows)
}

fn process_name_snapshot() -> anyhow::Result<HashMap<u32, String>> {
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }
        .context("CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS) failed")?;
    let result = (|| {
        let mut names = HashMap::new();
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        unsafe { Process32FirstW(snapshot, &raw mut entry) }.context("Process32FirstW failed")?;
        loop {
            let terminator = entry
                .szExeFile
                .iter()
                .position(|character| *character == 0)
                .unwrap_or(entry.szExeFile.len());
            let executable_name = String::from_utf16(&entry.szExeFile[..terminator])
                .context("process snapshot executable name is not valid UTF-16")?;
            if entry.th32ProcessID != 0 && !executable_name.is_empty() {
                names.insert(entry.th32ProcessID, executable_name);
            }
            entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
            if unsafe { Process32NextW(snapshot, &raw mut entry) }.is_err() {
                break;
            }
        }
        Ok(names)
    })();
    let _ = unsafe { CloseHandle(snapshot) };
    result
}

fn window_process_name(hwnd: HWND, process_names: &HashMap<u32, String>) -> Option<&str> {
    let mut process_id = 0_u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut process_id)) };
    process_names.get(&process_id).map(String::as_str)
}

fn process_image_identity(process_id: u32) -> anyhow::Result<ProcessImageIdentity> {
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id) }
        .with_context(|| format!("OpenProcess({process_id}) failed"))?;
    let result = (|| {
        let mut buffer = vec![0_u16; 32_768];
        let mut length = buffer.len() as u32;
        unsafe {
            QueryFullProcessImageNameW(
                process,
                PROCESS_NAME_WIN32,
                PWSTR(buffer.as_mut_ptr()),
                &mut length,
            )
        }
        .context("QueryFullProcessImageNameW failed")?;
        buffer.truncate(length as usize);
        Ok(ProcessImageIdentity {
            path: PathBuf::from(String::from_utf16(&buffer)?),
            creation_time_100ns: query_process_creation_time(process)?,
        })
    })();
    let _ = unsafe { CloseHandle(process) };
    result
}

fn process_creation_time_100ns(process_id: u32) -> anyhow::Result<u64> {
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id) }
        .with_context(|| format!("OpenProcess({process_id}) failed"))?;
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
    .context("GetProcessTimes failed")?;
    Ok((u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime))
}

fn window_class(hwnd: HWND) -> anyhow::Result<String> {
    let mut buffer = vec![0_u16; 256];
    let length = unsafe { GetClassNameW(hwnd, &mut buffer) };
    ensure!(length > 0, "GetClassNameW failed");
    buffer.truncate(length as usize);
    String::from_utf16(&buffer).context("window class is not valid UTF-16")
}

fn window_class_matches(expected: &str, observed: &str) -> bool {
    expected == observed
}

fn executable_name_matches(game: &GameProfile, observed: &str) -> bool {
    game.executable_names
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(observed))
}

/// 观察失败说明的长度上限（字符）。与隐私无关——这条值是整条错误链拼出来的，
/// 失控的嵌套 context 能撑到任意长度，而它要进本地 journal、API payload 和云端
/// 一列 text。2000 对真实错误链绰绰有余。服务端 0019 的 CHECK 用同一个数。
const EXECUTABLE_MISSING_REASON_MAX_CHARS: usize = 2000;

/// 把观察失败写成**给人看的完整说明**：阶段前缀 + 整条错误链。
///
/// 有意保留完整镜像路径与系统错误原文（负责人裁决 2026-07-28，docs/55 §3.1）：
/// 排查"这台机器的指纹为什么没测到"时，路径与原文远比一个错误码有用，而标注员
/// 都是内部员工。阶段前缀保留下来只是为了让同类失败好归类。
///
/// 例：`open_process_denied: OpenProcess(9284) failed: Access is denied. (os error 5)`
fn probe_failure_reason(stage: &str, error: &anyhow::Error) -> String {
    // anyhow 的 `{:#}` 把整条 context 链用 ": " 连起来，正是我们要的可读形式。
    truncate_on_char_boundary(
        format!("{stage}: {error:#}"),
        EXECUTABLE_MISSING_REASON_MAX_CHARS,
    )
}

fn truncate_on_char_boundary(mut value: String, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value;
    }
    // 系统错误消息是本地化的（中文 Windows 上就是中文），按字节截断会切碎
    // UTF-8；按字符边界截断，并留个记号说明被截过。
    const ELLIPSIS: &str = "…";
    let keep = max_chars.saturating_sub(ELLIPSIS.chars().count());
    let end = value
        .char_indices()
        .nth(keep)
        .map_or(value.len(), |(index, _)| index);
    value.truncate(end);
    value.push_str(ELLIPSIS);
    value
}

/// 身份证据比对：拿到多少比多少（docs/55 §3.2）。
///
/// 直接用 `Option` 的相等语义，不按"测没测到"分支——「当时测到 X，现在必须还是
/// X」「当时没测到，现在也必须没测到」都是它自然表达的。分支只会让某些证据在
/// 某些情况下被无声跳过，这正是上一版的毛病。
fn identity_evidence_matches(expected: &ExecutableIdentity, observed: &ExecutableIdentity) -> bool {
    // missing_reason 不参与比对：它是给人看的错误原文，同一种失败的措辞可能变。
    expected.creation_time_100ns == observed.creation_time_100ns
        && expected.sha256 == observed.sha256
        && verified_executable_matches(observed.verified.as_deref(), expected.verified.as_deref())
}

/// 发现 → 绑定之间的身份比对。
fn identity_unchanged(expected: &WindowObservation, observed: &WindowObservation) -> bool {
    // PID **无条件**比。一个 HWND 的属主进程终其一生不变，所以 PID 变了只可能是
    // 原窗口已销毁、句柄值被新进程复用——这恰恰是 continuity 要抓的掉包。上一版
    // 把它塞进了"测到了指纹"分支里，等于对证据最弱的会话放弃了最便宜的一道检查。
    observed.process_id == expected.process_id
        && observed.root_owner_value == expected.root_owner_value
        && observed
            .executable_name
            .eq_ignore_ascii_case(&expected.executable_name)
        && identity_evidence_matches(&expected.identity, &observed.identity)
}

/// 录制中的 continuity 身份比对，与 [`identity_unchanged`] 同一套规则。
fn bound_identity_unchanged(target: &BoundTarget, observed: &WindowObservation) -> bool {
    observed.process_id == target.process_id
        && observed.root_owner_value == target.root_owner_value
        && observed
            .executable_name
            .eq_ignore_ascii_case(&target.summary.executable_name)
        && identity_evidence_matches(&target.identity, &observed.identity)
}

fn verified_executable_matches(
    observed: Option<&VerifiedExecutable>,
    expected: Option<&VerifiedExecutable>,
) -> bool {
    match (observed, expected) {
        (Some(observed), Some(expected)) => {
            observed.identity == expected.identity && observed.stamp == expected.stamp
        }
        (None, None) => true,
        _ => false,
    }
}

fn executable_file_metadata(
    file: &File,
) -> anyhow::Result<(ExecutableFileIdentity, ExecutableFileStamp)> {
    let handle = HANDLE(file.as_raw_handle());
    let mut file_id = FILE_ID_INFO::default();
    let mut basic = FILE_BASIC_INFO::default();
    let mut standard = FILE_STANDARD_INFO::default();
    unsafe {
        GetFileInformationByHandleEx(
            handle,
            FileIdInfo,
            (&raw mut file_id).cast(),
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        )
    }
    .context("GetFileInformationByHandleEx(FileIdInfo) failed")?;
    unsafe {
        GetFileInformationByHandleEx(
            handle,
            FileBasicInfo,
            (&raw mut basic).cast(),
            std::mem::size_of::<FILE_BASIC_INFO>() as u32,
        )
    }
    .context("GetFileInformationByHandleEx(FileBasicInfo) failed")?;
    unsafe {
        GetFileInformationByHandleEx(
            handle,
            FileStandardInfo,
            (&raw mut standard).cast(),
            std::mem::size_of::<FILE_STANDARD_INFO>() as u32,
        )
    }
    .context("GetFileInformationByHandleEx(FileStandardInfo) failed")?;
    ensure!(!standard.Directory, "game process image is a directory");
    ensure!(
        !standard.DeletePending,
        "game process image is pending deletion"
    );
    Ok((
        ExecutableFileIdentity {
            volume_serial_number: file_id.VolumeSerialNumber,
            file_id: file_id.FileId.Identifier,
        },
        ExecutableFileStamp {
            creation_time: basic.CreationTime,
            last_write_time: basic.LastWriteTime,
            change_time: basic.ChangeTime,
            end_of_file: standard.EndOfFile,
            allocation_size: standard.AllocationSize,
            file_attributes: basic.FileAttributes,
            number_of_links: standard.NumberOfLinks,
            delete_pending: standard.DeletePending,
        },
    ))
}

fn sha256_file_handle(file: &File) -> anyhow::Result<String> {
    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("sha256:{:x}", hasher.finalize()))
}

fn ensure_expected_executable_hash(expected: Option<&str>, observed: &str) -> anyhow::Result<()> {
    if let Some(expected) = expected {
        ensure!(
            expected == observed,
            "game executable SHA-256 does not match the selected profile"
        );
    }
    Ok(())
}

/// 把身份证据混进 id：拿到多少混多少，缺席的层用固定标签占位，使
/// 「测到了指纹」与「没测到」永远散列到不同的 id。
fn hash_identity_evidence(hasher: &mut Sha256, observation: &WindowObservation) {
    match observation.identity.creation_time_100ns {
        Some(creation_time) => hasher.update(creation_time.to_le_bytes()),
        None => hasher.update(b"no-process-creation-time"),
    }
    match observation.identity.verified.as_deref() {
        Some(verified) => {
            hasher.update(verified.identity.volume_serial_number.to_le_bytes());
            hasher.update(verified.identity.file_id);
        }
        None => {
            hasher.update(b"no-verified-executable");
            hasher.update(observation.executable_name.to_ascii_lowercase().as_bytes());
        }
    }
    match observation.identity.sha256.as_deref() {
        Some(sha256) => hasher.update(sha256.as_bytes()),
        None => hasher.update(b"no-executable-sha256"),
    }
}

fn candidate_id(generation: u64, observation: &WindowObservation) -> String {
    let mut hasher = Sha256::new();
    hasher.update(generation.to_le_bytes());
    hasher.update(observation.hwnd_value.to_le_bytes());
    hasher.update(observation.process_id.to_le_bytes());
    hash_identity_evidence(&mut hasher, observation);
    format!("candidate-{:x}", hasher.finalize())
}

fn target_id(candidate_id: &str, observation: &WindowObservation) -> String {
    let mut hasher = Sha256::new();
    hasher.update(candidate_id.as_bytes());
    hasher.update(observation.root_owner_value.to_le_bytes());
    hash_identity_evidence(&mut hasher, observation);
    format!("target-{:x}", hasher.finalize())
}

fn root_owner_value(hwnd: HWND) -> usize {
    if hwnd.0.is_null() {
        return 0;
    }
    let root = unsafe { GetAncestor(hwnd, GA_ROOTOWNER) };
    if root.0.is_null() {
        hwnd.0 as usize
    } else {
        root.0 as usize
    }
}

fn wait_until(
    timeout: Duration,
    poll_interval: Duration,
    mut predicate: impl FnMut() -> bool,
) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if predicate() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(poll_interval);
    }
}

#[derive(Clone)]
struct MonitorContext {
    target_hwnd: usize,
    target_root: usize,
    expected_width: u32,
    expected_height: u32,
    expected_dpi: u32,
    cutoff_qpc: Arc<AtomicI64>,
    controls: Sender<RecordingStopSignal>,
    input_status: RawInputStatus,
    overlay_placement_dirty: Arc<AtomicBool>,
    active: Arc<AtomicBool>,
    startup_invalid: Arc<StdMutex<Option<RecordingStopSignal>>>,
    foreground_event_epoch: Arc<AtomicU64>,
}

struct InputFaultSpec {
    overflow: bool,
    reason: String,
}

static MONITOR_CONTEXT: OnceLock<StdMutex<Option<MonitorContext>>> = OnceLock::new();

pub struct TargetWindowMonitor {
    thread_id: u32,
    join: Option<JoinHandle<()>>,
    input_control: RawInputControl,
    input_status: RawInputStatus,
    active: Arc<AtomicBool>,
    startup_invalid: Arc<StdMutex<Option<RecordingStopSignal>>>,
}

pub struct TargetMonitorCommitGuard<'a> {
    active: &'a AtomicBool,
    input_status: &'a RawInputStatus,
    startup_invalid: StdMutexGuard<'a, Option<RecordingStopSignal>>,
}

impl TargetMonitorCommitGuard<'_> {
    #[must_use]
    pub fn activate_and_open(self) -> bool {
        debug_assert!(self.startup_invalid.is_none());
        self.active.store(true, Ordering::Release);
        self.input_status.open_gate()
    }
}

impl TargetWindowMonitor {
    pub fn start(
        target: &BoundTarget,
        session_salt: String,
        cutoff_qpc: Arc<AtomicI64>,
        controls: Sender<RecordingStopSignal>,
        input_records: Sender<RawInputRecord>,
        overlay_placement_dirty: Arc<AtomicBool>,
    ) -> anyhow::Result<Self> {
        let input_status = RawInputStatus::default();
        // Registration and hook installation are part of preflight. Keep the
        // event gate closed until the journal has accepted begin_run so no
        // pre-start input can enter the run queue.
        input_status.close_gate();
        let active = Arc::new(AtomicBool::new(false));
        let startup_invalid = Arc::new(StdMutex::new(None));
        let context = MonitorContext {
            target_hwnd: target.hwnd_value,
            target_root: target.root_owner_value,
            expected_width: target.client_width,
            expected_height: target.client_height,
            expected_dpi: target.dpi,
            cutoff_qpc,
            controls,
            input_status: input_status.clone(),
            overlay_placement_dirty,
            active: Arc::clone(&active),
            startup_invalid: Arc::clone(&startup_invalid),
            foreground_event_epoch: Arc::new(AtomicU64::new(0)),
        };
        let global = MONITOR_CONTEXT.get_or_init(|| StdMutex::new(None));
        {
            let mut slot = global.lock().expect("monitor context poisoned");
            ensure!(slot.is_none(), "a target window monitor is already running");
            *slot = Some(context);
        }
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let (command_tx, command_rx) = bounded(RAW_INPUT_COMMAND_CAPACITY);
        let thread_config = MonitorThreadConfig {
            session_salt,
            input_contract: target.input_contract.clone(),
            input_records: input_records.clone(),
            commands: command_rx,
            input_status: input_status.clone(),
        };
        let worker_input_status = input_status.clone();
        let join = match thread::Builder::new()
            .name("dm-raw-input-window-events".into())
            .spawn(move || {
                let result = run_monitor_thread(ready_tx, thread_config);
                if let Err(error) = result {
                    let reason = format!("target window monitor failed: {error:#}");
                    let mut qpc = 0_i64;
                    let _ = unsafe { QueryPerformanceCounter(&mut qpc) };
                    if !request_input_fault_stop(qpc, false, reason.clone()) {
                        worker_input_status.mark_fault(false, reason);
                    }
                    tracing::error!(error = %error, "target window monitor failed");
                }
            }) {
            Ok(join) => join,
            Err(error) => {
                *global.lock().expect("monitor context poisoned") = None;
                return Err(error).context("failed to spawn target window monitor");
            }
        };
        let thread_id = match ready_rx.recv() {
            Ok(Ok(thread_id)) => thread_id,
            Ok(Err(error)) => {
                let _ = join.join();
                *global.lock().expect("monitor context poisoned") = None;
                return Err(error).context("window monitor failed during startup");
            }
            Err(error) => {
                let _ = join.join();
                *global.lock().expect("monitor context poisoned") = None;
                return Err(error).context("window monitor exited during startup");
            }
        };
        let input_control =
            RawInputControl::new(thread_id, command_tx, input_records, input_status.clone());
        let monitor = Self {
            thread_id,
            join: Some(join),
            input_control,
            input_status,
            active,
            startup_invalid,
        };
        ensure!(
            root_owner_value(unsafe { GetForegroundWindow() }) == target.root_owner_value,
            "bound game window lost foreground while its event monitor was starting"
        );
        ensure!(
            !geometry_changed(
                MONITOR_CONTEXT
                    .get()
                    .expect("monitor context was not initialized")
                    .lock()
                    .expect("monitor context poisoned")
                    .as_ref()
                    .expect("monitor context disappeared")
            ),
            "bound game window geometry changed while its event monitor was starting"
        );
        Ok(monitor)
    }

    #[must_use]
    pub fn input_control(&self) -> RawInputControl {
        self.input_control.clone()
    }

    #[must_use]
    pub fn input_status(&self) -> RawInputStatus {
        self.input_status.clone()
    }

    pub fn prepare_commit(&self) -> anyhow::Result<TargetMonitorCommitGuard<'_>> {
        let startup_invalid = self
            .startup_invalid
            .lock()
            .expect("monitor startup state poisoned");
        if let Some(signal) = *startup_invalid {
            anyhow::bail!(
                "target invalidated during monitor preflight: {:?} at QPC {}",
                signal.reason,
                signal.cutoff_qpc
            );
        }
        ensure!(
            !self.input_status.faulted(),
            "target input monitor faulted during preflight"
        );
        Ok(TargetMonitorCommitGuard {
            active: &self.active,
            input_status: &self.input_status,
            startup_invalid,
        })
    }
}

impl Drop for TargetWindowMonitor {
    fn drop(&mut self) {
        self.input_status.close_gate();
        let _ = unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) };
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
        if let Some(global) = MONITOR_CONTEXT.get() {
            *global.lock().expect("monitor context poisoned") = None;
        }
    }
}

struct MonitorThreadConfig {
    session_salt: String,
    input_contract: GameInputContract,
    input_records: Sender<RawInputRecord>,
    commands: Receiver<RawInputThreadCommand>,
    input_status: RawInputStatus,
}

fn run_monitor_thread(
    ready: mpsc::SyncSender<anyhow::Result<u32>>,
    config: MonitorThreadConfig,
) -> anyhow::Result<()> {
    let sink = match create_raw_input_sink() {
        Ok(sink) => sink,
        Err(error) => {
            let _ = ready.send(Err(error));
            return Ok(());
        }
    };
    if let Err(error) = register_raw_input(sink) {
        let _ = unsafe { DestroyWindow(sink) };
        let _ = ready.send(Err(error));
        return Ok(());
    }
    let hooks = match install_hooks() {
        Ok(hooks) => hooks,
        Err(error) => {
            unregister_raw_input();
            let _ = unsafe { DestroyWindow(sink) };
            let _ = ready.send(Err(error));
            return Ok(());
        }
    };
    let thread_id = unsafe { GetCurrentThreadId() };
    let mut message = MSG::default();
    let _ = unsafe { PeekMessageW(&mut message, None, WM_USER, WM_USER, PM_NOREMOVE) };
    let _ = ready.send(Ok(thread_id));
    let mut input = RawInputThreadState::new(config);
    let mut raw_input_reader = RawInputReader::new();
    let mut raw_input_batch = Vec::new();
    let message_result = (|| -> anyhow::Result<()> {
        loop {
            let result = unsafe { GetMessageW(&mut message, None, 0, 0) };
            if result.0 == 0 {
                break;
            }
            ensure!(result.0 > 0, "GetMessageW failed in target monitor");
            if message.message == WM_INPUT {
                let mut qpc = 0_i64;
                let read = raw_input_reader.read_available(
                    HRAWINPUT(message.lParam.0 as *mut c_void),
                    &mut raw_input_batch,
                );
                if let Err(error) = read {
                    let _ = unsafe { QueryPerformanceCounter(&mut qpc) };
                    input.fail(
                        qpc,
                        false,
                        format!("Raw Input batch read failed: {error:#}"),
                    );
                } else if unsafe { QueryPerformanceCounter(&mut qpc) }.is_err() {
                    input.fail(qpc, false, "QueryPerformanceCounter failed in WM_INPUT");
                } else {
                    input.input_status.note_raw_batch(
                        raw_input_batch.len(),
                        raw_input_batch
                            .iter()
                            .filter(|packet| packet.buffered)
                            .count(),
                    );
                    for packet in &raw_input_batch {
                        if let Err(error) = input.handle_raw_input(&packet.raw, qpc) {
                            input.fail(qpc, false, format!("WM_INPUT packet rejected: {error:#}"));
                            break;
                        }
                    }
                }
                let _ = unsafe {
                    DefWindowProcW(
                        message.hwnd,
                        message.message,
                        message.wParam,
                        message.lParam,
                    )
                };
            } else if message.message == RAW_INPUT_CONTROL_MESSAGE {
                input.handle_commands()?;
            } else if message.message == WM_INPUT_DEVICE_CHANGE
                && message.wParam.0 as u32 == GIDC_REMOVAL
            {
                let mut qpc = 0_i64;
                if unsafe { QueryPerformanceCounter(&mut qpc) }.is_ok() {
                    input.fail(qpc, false, "Raw Input device was removed during recording");
                }
            } else {
                let _ = unsafe { TranslateMessage(&message) };
                unsafe { DispatchMessageW(&message) };
            }
        }
        Ok(())
    })();
    input.input_status.close_gate();
    for hook in hooks {
        let _ = unsafe { UnhookWinEvent(hook) };
    }
    unregister_raw_input();
    let _ = unsafe { DestroyWindow(sink) };
    message_result
}

fn create_raw_input_sink() -> anyhow::Result<HWND> {
    unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("STATIC"),
            w!("DataMinerRawInputSink"),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            None,
            None,
        )
    }
    .context("failed to create the Raw Input message-only window")
}

fn register_raw_input(sink: HWND) -> anyhow::Result<()> {
    let flags = RIDEV_INPUTSINK | RIDEV_DEVNOTIFY;
    let devices = [
        RAWINPUTDEVICE {
            usUsagePage: 0x01,
            usUsage: 0x02,
            dwFlags: flags,
            hwndTarget: sink,
        },
        RAWINPUTDEVICE {
            usUsagePage: 0x01,
            usUsage: 0x06,
            dwFlags: flags,
            hwndTarget: sink,
        },
    ];
    unsafe { RegisterRawInputDevices(&devices, std::mem::size_of::<RAWINPUTDEVICE>() as u32) }
        .context("RegisterRawInputDevices(mouse/keyboard) failed")?;
    Ok(())
}

fn unregister_raw_input() {
    let devices = [
        RAWINPUTDEVICE {
            usUsagePage: 0x01,
            usUsage: 0x02,
            dwFlags: RIDEV_REMOVE,
            hwndTarget: HWND::default(),
        },
        RAWINPUTDEVICE {
            usUsagePage: 0x01,
            usUsage: 0x06,
            dwFlags: RIDEV_REMOVE,
            hwndTarget: HWND::default(),
        },
    ];
    let _ =
        unsafe { RegisterRawInputDevices(&devices, std::mem::size_of::<RAWINPUTDEVICE>() as u32) };
}

#[derive(Clone, Copy)]
enum ArmingControl {
    Key(u16),
    Mouse(u16),
}

#[derive(Clone, Copy)]
struct ArmingTransition {
    qpc: i64,
    control: ArmingControl,
    down: bool,
    application_control: bool,
}

struct RawInputThreadState {
    session_salt: String,
    contract: GameInputContract,
    records: Sender<RawInputRecord>,
    commands: Receiver<RawInputThreadCommand>,
    input_status: RawInputStatus,
    arming_start_qpc: Option<i64>,
    arming_transitions: Vec<ArmingTransition>,
    armed: bool,
}

impl RawInputThreadState {
    fn new(config: MonitorThreadConfig) -> Self {
        Self {
            session_salt: config.session_salt,
            contract: config.input_contract,
            records: config.input_records,
            commands: config.commands,
            input_status: config.input_status,
            arming_start_qpc: None,
            arming_transitions: Vec::new(),
            armed: false,
        }
    }

    fn handle_raw_input(&mut self, raw: &RAWINPUT, qpc: i64) -> anyhow::Result<()> {
        if !self.input_status.is_accepting() {
            return Ok(());
        }
        let read = usize::try_from(raw.header.dwSize).context("Raw Input dwSize overflow")?;
        if raw.header.dwType == RIM_TYPEKEYBOARD.0 {
            ensure!(
                read >= std::mem::size_of::<RAWINPUTHEADER>()
                    + std::mem::size_of::<windows::Win32::UI::Input::RAWKEYBOARD>(),
                "Raw Input keyboard packet is truncated"
            );
            let keyboard = unsafe { raw.data.keyboard };
            self.handle_keyboard(raw.header.hDevice, keyboard, qpc)?;
        } else if raw.header.dwType == RIM_TYPEMOUSE.0 {
            ensure!(
                read >= std::mem::size_of::<RAWINPUTHEADER>()
                    + std::mem::size_of::<windows::Win32::UI::Input::RAWMOUSE>(),
                "Raw Input mouse packet is truncated"
            );
            let mouse = unsafe { raw.data.mouse };
            self.handle_mouse(raw.header.hDevice, mouse, qpc)?;
        }
        Ok(())
    }

    fn handle_keyboard(
        &mut self,
        device: HANDLE,
        keyboard: windows::Win32::UI::Input::RAWKEYBOARD,
        qpc: i64,
    ) -> anyhow::Result<()> {
        let vk = keyboard.VKey;
        if vk == 255 || !self.contract.allows_key(vk) {
            return Ok(());
        }
        let down = u32::from(keyboard.Flags) & RI_KEY_BREAK == 0;
        let repeat = self.input_status.set_key(vk, down) == down;
        let application_control = self.contract.is_control_key(vk);
        if !self.armed {
            self.arming_transitions.push(ArmingTransition {
                qpc,
                control: ArmingControl::Key(vk),
                down,
                application_control,
            });
        }
        self.send_packet(RawInputPacket {
            qpc,
            device: device_id(&self.session_salt, "kbd", device),
            control: application_control,
            kind: RawInputKind::Key {
                down,
                vk,
                scan_code: keyboard.MakeCode,
                flags: keyboard.Flags,
                repeat: down && repeat,
            },
        })
    }

    fn handle_mouse(
        &mut self,
        device: HANDLE,
        mouse: windows::Win32::UI::Input::RAWMOUSE,
        qpc: i64,
    ) -> anyhow::Result<()> {
        if mouse.usFlags.0 & MOUSE_MOVE_ABSOLUTE.0 != 0 {
            anyhow::bail!("absolute mouse input does not match the relative-input profile");
        }
        let device = device_id(&self.session_salt, "mouse", device);
        if self.contract.capture_mouse_move && (mouse.lLastX != 0 || mouse.lLastY != 0) {
            self.send_packet(RawInputPacket {
                qpc,
                device: device.clone(),
                control: false,
                kind: RawInputKind::MouseMove {
                    dx: mouse.lLastX,
                    dy: mouse.lLastY,
                    absolute: false,
                },
            })?;
        }
        let buttons = unsafe { mouse.Anonymous.Anonymous };
        for (flag, vk, down) in [
            (RI_MOUSE_BUTTON_1_DOWN, 1, true),
            (RI_MOUSE_BUTTON_1_UP, 1, false),
            (RI_MOUSE_BUTTON_2_DOWN, 2, true),
            (RI_MOUSE_BUTTON_2_UP, 2, false),
            (RI_MOUSE_BUTTON_3_DOWN, 4, true),
            (RI_MOUSE_BUTTON_3_UP, 4, false),
            (RI_MOUSE_BUTTON_4_DOWN, 5, true),
            (RI_MOUSE_BUTTON_4_UP, 5, false),
            (RI_MOUSE_BUTTON_5_DOWN, 6, true),
            (RI_MOUSE_BUTTON_5_UP, 6, false),
        ] {
            if u32::from(buttons.usButtonFlags) & flag == 0
                || !self.contract.allows_mouse_button(vk)
            {
                continue;
            }
            self.input_status.set_mouse_button(vk, down);
            if !self.armed {
                self.arming_transitions.push(ArmingTransition {
                    qpc,
                    control: ArmingControl::Mouse(vk),
                    down,
                    application_control: false,
                });
            }
            self.send_packet(RawInputPacket {
                qpc,
                device: device.clone(),
                control: false,
                kind: RawInputKind::MouseButton { down, vk },
            })?;
        }
        if self.contract.capture_wheel {
            if u32::from(buttons.usButtonFlags) & RI_MOUSE_WHEEL != 0 {
                self.send_packet(RawInputPacket {
                    qpc,
                    device: device.clone(),
                    control: false,
                    kind: RawInputKind::Wheel {
                        delta: buttons.usButtonData as i16,
                        horizontal: false,
                    },
                })?;
            }
            if u32::from(buttons.usButtonFlags) & RI_MOUSE_HWHEEL != 0 {
                self.send_packet(RawInputPacket {
                    qpc,
                    device,
                    control: false,
                    kind: RawInputKind::Wheel {
                        delta: buttons.usButtonData as i16,
                        horizontal: true,
                    },
                })?;
            }
        }
        Ok(())
    }

    fn send_packet(&mut self, packet: RawInputPacket) -> anyhow::Result<()> {
        self.arming_start_qpc.get_or_insert(packet.qpc);
        let callback_start = packet.qpc;
        match self.records.try_send(RawInputRecord::Event(packet)) {
            Ok(()) => {
                let mut callback_end = callback_start;
                let _ = unsafe { QueryPerformanceCounter(&mut callback_end) };
                self.input_status.note_received(
                    self.records.len(),
                    callback_end.saturating_sub(callback_start) as u64,
                );
                Ok(())
            }
            Err(_) => {
                self.fail(callback_start, true, "Raw Input bounded queue overflowed");
                anyhow::bail!("Raw Input bounded queue overflowed")
            }
        }
    }

    fn handle_commands(&mut self) -> anyhow::Result<()> {
        while let Ok(command) = self.commands.try_recv() {
            match command {
                RawInputThreadCommand::Arm {
                    anchor_qpc,
                    first_segment_end_qpc,
                    response,
                } => {
                    let neutral_at_anchor = self.neutral_at(anchor_qpc);
                    self.armed = true;
                    self.arming_transitions.clear();
                    self.send_control_record(
                        RawInputRecord::Arm {
                            anchor_qpc,
                            first_segment_end_qpc,
                            neutral_at_anchor,
                            arming_start_qpc: self.arming_start_qpc,
                            response,
                        },
                        anchor_qpc,
                    )?;
                }
                RawInputThreadCommand::Seal {
                    segment_index,
                    qpc_end,
                    next_qpc_end,
                    response,
                } => self.send_control_record(
                    RawInputRecord::Seal {
                        segment_index,
                        qpc_end,
                        next_qpc_end,
                        response,
                    },
                    qpc_end,
                )?,
                RawInputThreadCommand::Finish {
                    segment_index,
                    effective_end_qpc,
                    response,
                } => {
                    self.input_status.close_gate();
                    self.send_control_record(
                        RawInputRecord::Finish {
                            segment_index,
                            effective_end_qpc,
                            response,
                        },
                        effective_end_qpc,
                    )?;
                }
                RawInputThreadCommand::Abort { response } => {
                    self.input_status.close_gate();
                    self.send_control_record(RawInputRecord::Abort { response }, 0)?;
                }
                RawInputThreadCommand::ClearPressed => {
                    self.input_status.clear_pressed();
                    self.arming_transitions.clear();
                }
            }
        }
        Ok(())
    }

    fn send_control_record(&mut self, record: RawInputRecord, qpc: i64) -> anyhow::Result<()> {
        if self.records.try_send(record).is_err() {
            self.fail(
                qpc,
                true,
                "Raw Input queue could not accept a segment barrier",
            );
            anyhow::bail!("Raw Input queue could not accept a segment barrier");
        }
        Ok(())
    }

    fn neutral_at(&self, anchor_qpc: i64) -> bool {
        let (keys, mouse) = self.held_at(anchor_qpc);
        let neutral = keys == [0; 4] && mouse == 0;
        if !neutral {
            // Report the same pre-anchor Raw Input transitions used by the
            // existing gate. Do not poll, clear state or synthesize releases.
            let pressed_keys: Vec<_> = (0_u16..256)
                .filter(|vk| keys[usize::from(vk / 64)] & (1_u64 << (vk % 64)) != 0)
                .collect();
            let pressed_mouse: Vec<_> = [1_u16, 2, 4, 5, 6]
                .into_iter()
                .enumerate()
                .filter_map(|(index, vk)| (mouse & (1 << index) != 0).then_some(vk))
                .collect();
            eprintln!(
                "[Raw Input arming] INPUT_NOT_NEUTRAL anchor_qpc={anchor_qpc} \
                 pressed_contract_key_vks={pressed_keys:?} mouse_button_vks={pressed_mouse:?}"
            );
        }
        neutral
    }

    fn held_at(&self, anchor_qpc: i64) -> ([u64; 4], u8) {
        let mut keys = [0_u64; 4];
        let mut mouse = 0_u8;
        for transition in self
            .arming_transitions
            .iter()
            .filter(|event| event.qpc < anchor_qpc && !event.application_control)
        {
            match transition.control {
                ArmingControl::Key(vk) => {
                    let bit = 1_u64 << (vk % 64);
                    let slot = &mut keys[usize::from(vk / 64)];
                    if transition.down {
                        *slot |= bit;
                    } else {
                        *slot &= !bit;
                    }
                }
                ArmingControl::Mouse(vk) => {
                    let bit = match vk {
                        1 => 1 << 0,
                        2 => 1 << 1,
                        4 => 1 << 2,
                        5 => 1 << 3,
                        6 => 1 << 4,
                        _ => 0,
                    };
                    if transition.down {
                        mouse |= bit;
                    } else {
                        mouse &= !bit;
                    }
                }
            }
        }
        (keys, mouse)
    }

    fn fail(&mut self, qpc: i64, overflow: bool, reason: impl Into<String>) {
        let reason = reason.into();
        if !request_input_fault_stop(qpc, overflow, reason.clone()) {
            self.input_status.mark_fault(overflow, reason);
        }
    }
}

fn device_id(session_salt: &str, kind: &str, device: HANDLE) -> String {
    let mut hasher = Sha256::new();
    hasher.update(session_salt.as_bytes());
    hasher.update(kind.as_bytes());
    hasher.update((device.0 as usize).to_le_bytes());
    let digest = format!("{:x}", hasher.finalize());
    format!("{kind}:{}", &digest[..16])
}

fn request_input_fault_stop(qpc: i64, overflow: bool, reason: String) -> bool {
    let Some(global) = MONITOR_CONTEXT.get() else {
        return false;
    };
    let Some(context) = global
        .lock()
        .expect("monitor context poisoned")
        .as_ref()
        .cloned()
    else {
        return false;
    };
    route_monitor_stop(
        &context,
        RecordingStopReason::InputFault,
        qpc,
        Some(InputFaultSpec { overflow, reason }),
    );
    true
}

fn install_hooks() -> anyhow::Result<Vec<HWINEVENTHOOK>> {
    let mut hooks = Vec::new();
    for event in [
        EVENT_SYSTEM_FOREGROUND,
        EVENT_SYSTEM_MINIMIZESTART,
        EVENT_SYSTEM_MINIMIZEEND,
        EVENT_OBJECT_LOCATIONCHANGE,
        EVENT_OBJECT_DESTROY,
    ] {
        match install_hook(event) {
            Ok(hook) => hooks.push(hook),
            Err(error) => {
                for hook in hooks {
                    let _ = unsafe { UnhookWinEvent(hook) };
                }
                return Err(error);
            }
        }
    }
    Ok(hooks)
}

fn install_hook(event: u32) -> anyhow::Result<HWINEVENTHOOK> {
    let hook = unsafe {
        SetWinEventHook(
            event,
            event,
            None,
            Some(win_event_callback),
            0,
            0,
            WINEVENT_OUTOFCONTEXT,
        )
    };
    ensure!(!hook.0.is_null(), "SetWinEventHook({event:#x}) failed");
    Ok(hook)
}

/// `OBJID_WINDOW` / `CHILDID_SELF` from `winuser.h`. WinEvent object notifications
/// also fire for non-window accessible objects that belong to the same HWND —
/// caret, cursor, client area — so a destroy of the target window's caret is
/// indistinguishable from the window closing unless these are checked.
const OBJID_WINDOW: i32 = 0;
const CHILDID_SELF: i32 = 0;

/// How one WinEvent notification relates to the bound capture target.
#[derive(Clone, Copy, Debug)]
struct WinEventScope {
    /// The notification describes the window itself, not a caret/cursor/client
    /// area object that merely belongs to it.
    window_object: bool,
    hwnd_is_target: bool,
    root_is_target: bool,
}

impl WinEventScope {
    fn affects_overlay_placement(self, event: u32) -> bool {
        let target_event = self.hwnd_is_target || self.root_is_target;
        event == EVENT_SYSTEM_FOREGROUND
            || (target_event
                && matches!(event, EVENT_SYSTEM_MINIMIZESTART | EVENT_SYSTEM_MINIMIZEEND))
            || (target_event
                && self.window_object
                && matches!(event, EVENT_OBJECT_LOCATIONCHANGE | EVENT_OBJECT_DESTROY))
    }

    /// `geometry_changed` is only consulted for location changes, so the caller
    /// can keep the Win32 geometry round-trip out of the common path.
    fn stop_reason(
        self,
        event: u32,
        geometry_changed: impl FnOnce() -> bool,
    ) -> Option<RecordingStopReason> {
        // The `EVENT_SYSTEM_*` arms deliberately ignore `window_object`: missing a
        // foreground loss keeps Raw Input recording keystrokes aimed at another
        // application, so those stay maximally sensitive. The `EVENT_OBJECT_*` arms
        // must not — a caret destroy on the target HWND would otherwise seal the run
        // and force a rebind for no reason.
        if event == EVENT_SYSTEM_FOREGROUND {
            return (!self.root_is_target).then_some(RecordingStopReason::ForegroundLost);
        }
        if event == EVENT_SYSTEM_MINIMIZESTART && self.root_is_target {
            return Some(RecordingStopReason::WindowMinimized);
        }
        if !(self.window_object && self.hwnd_is_target) {
            return None;
        }
        match event {
            EVENT_OBJECT_DESTROY => Some(RecordingStopReason::WindowDestroyed),
            EVENT_OBJECT_LOCATIONCHANGE => {
                geometry_changed().then_some(RecordingStopReason::GeometryChanged)
            }
            _ => None,
        }
    }
}

unsafe extern "system" fn win_event_callback(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    object_id: i32,
    child_id: i32,
    _event_thread: u32,
    _event_time_ms: u32,
) {
    let Some(global) = MONITOR_CONTEXT.get() else {
        return;
    };
    let Some(context) = global
        .lock()
        .expect("monitor context poisoned")
        .as_ref()
        .cloned()
    else {
        return;
    };
    let scope = WinEventScope {
        window_object: object_id == OBJID_WINDOW && child_id == CHILDID_SELF,
        hwnd_is_target: hwnd.0 as usize == context.target_hwnd,
        root_is_target: root_owner_value(hwnd) == context.target_root,
    };
    if scope.affects_overlay_placement(event) {
        context
            .overlay_placement_dirty
            .store(true, Ordering::Release);
    }
    // The local PR405 collector runs unattended for five minutes. Windows can
    // briefly publish another root as foreground for a notification or secure
    // desktop transition even though Fallout immediately regains focus. Keep
    // production builds fail-fast, but require a continuous loss in the
    // explicitly non-production local build so one transient event cannot seal
    // an otherwise valid long capture.
    if cfg!(feature = "local-catalog-only") && event == EVENT_SYSTEM_FOREGROUND {
        let epoch = context
            .foreground_event_epoch
            .fetch_add(1, Ordering::AcqRel)
            .saturating_add(1);
        if scope.root_is_target {
            return;
        }
        let delayed = context.clone();
        let _ = thread::Builder::new()
            .name("dm-foreground-loss-grace".into())
            .spawn(move || {
                thread::sleep(Duration::from_secs(3));
                if delayed.foreground_event_epoch.load(Ordering::Acquire) != epoch
                    || root_owner_value(unsafe { GetForegroundWindow() }) == delayed.target_root
                {
                    return;
                }
                let mut qpc = 0_i64;
                if unsafe { QueryPerformanceCounter(&mut qpc) }.is_ok() {
                    route_monitor_stop(&delayed, RecordingStopReason::ForegroundLost, qpc, None);
                }
            });
        return;
    }
    let reason = scope.stop_reason(event, || geometry_changed(&context));
    if let Some(reason) = reason {
        let mut qpc = 0_i64;
        if unsafe { QueryPerformanceCounter(&mut qpc) }.is_ok() {
            route_monitor_stop(&context, reason, qpc, None);
        }
    }
}

fn route_monitor_stop(
    context: &MonitorContext,
    reason: RecordingStopReason,
    qpc: i64,
    input_fault: Option<InputFaultSpec>,
) {
    let mut startup_invalid = context
        .startup_invalid
        .lock()
        .expect("monitor startup state poisoned");
    if !context.active.load(Ordering::Acquire) {
        if let Some(fault) = &input_fault {
            context
                .input_status
                .mark_fault(fault.overflow, fault.reason.clone());
        }
        context.input_status.close_gate();
        if startup_invalid
            .as_ref()
            .is_none_or(|existing| qpc < existing.cutoff_qpc)
        {
            *startup_invalid = Some(RecordingStopSignal {
                reason,
                cutoff_qpc: qpc,
            });
        }
        return;
    }
    if let Some(fault) = &input_fault {
        context
            .input_status
            .mark_fault(fault.overflow, fault.reason.clone());
    }
    // The startup mutex serializes this close with activate_and_open, so a stop
    // can never be followed by a stale gate reopen.
    context.input_status.close_gate();
    context.cutoff_qpc.fetch_min(qpc, Ordering::AcqRel);
    let _ = context.controls.try_send(RecordingStopSignal {
        reason,
        cutoff_qpc: qpc,
    });
}

fn geometry_changed(context: &MonitorContext) -> bool {
    let hwnd = HWND(context.target_hwnd as *mut _);
    if !unsafe { IsWindow(Some(hwnd)) }.as_bool() || unsafe { IsIconic(hwnd) }.as_bool() {
        return true;
    }
    let mut client = RECT::default();
    if unsafe { GetClientRect(hwnd, &mut client) }.is_err() {
        return true;
    }
    let width = (client.right - client.left).max(0) as u32;
    let height = (client.bottom - client.top).max(0) as u32;
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    width != context.expected_width
        || height != context.expected_height
        || dpi != context.expected_dpi
}

#[cfg(test)]
mod tests {
    // build.rs 生成的全量 game_id 名单（roster 守卫的期望集来源）。
    #[cfg(feature = "dev-catalog")]
    include!(concat!(env!("OUT_DIR"), "/embedded_dev_game_ids.rs"));

    use super::*;

    fn temp_executable_path(label: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "dataminer-{label}-{}-{unique}.exe",
            std::process::id()
        ))
    }

    fn monitor_context_for_test(active: bool) -> (MonitorContext, Receiver<RecordingStopSignal>) {
        let (controls, receiver) = bounded(4);
        (
            MonitorContext {
                target_hwnd: 1,
                target_root: 1,
                expected_width: 1920,
                expected_height: 1080,
                expected_dpi: 96,
                cutoff_qpc: Arc::new(AtomicI64::new(i64::MAX)),
                controls,
                input_status: RawInputStatus::default(),
                overlay_placement_dirty: Arc::new(AtomicBool::new(false)),
                active: Arc::new(AtomicBool::new(active)),
                startup_invalid: Arc::new(StdMutex::new(None)),
                foreground_event_epoch: Arc::new(AtomicU64::new(0)),
            },
            receiver,
        )
    }

    fn input_thread_for_test() -> (RawInputThreadState, Receiver<RawInputRecord>) {
        let (records, receiver) = bounded(32);
        let (_commands, command_receiver) = bounded(4);
        let config = MonitorThreadConfig {
            session_salt: "session-test".into(),
            input_contract: GameInputContract {
                keyboard_vks: vec![87],
                control_vks: Vec::new(),
                mouse_buttons: vec![1],
                capture_mouse_move: true,
                capture_wheel: true,
            },
            input_records: records,
            commands: command_receiver,
            input_status: RawInputStatus::default(),
        };
        (RawInputThreadState::new(config), receiver)
    }

    fn fake_device() -> HANDLE {
        HANDLE(std::ptr::dangling_mut::<c_void>())
    }

    #[cfg(feature = "dev-catalog")]
    #[test]
    fn the_bundled_catalog_is_non_trainable_throughout() {
        let tracker = WindowTracker::build().unwrap();
        assert!(tracker.games().iter().all(|game| game.non_trainable));
        // 编译内嵌 ⇒ 每一款都停在 legacy v1，永久 non-trainable（issue #855 §D）。
        // 内置目录已经是中性 schema（PR 2），所以这条断言证明的正是"schema 断代
        // **不会**把本地路径也拖进 v2"。
        assert!(tracker.games().iter().all(|game| {
            game.session_contract == capture_core::ingest_contract::SessionContract::LocalDevV1
        }));
        let game_ids = tracker
            .games()
            .into_iter()
            .map(|game| game.game_id)
            .collect::<Vec<_>>();
        assert!(
            game_ids
                .iter()
                .any(|game_id| game_id == "synthetic-win32-dev")
        );
        // expected 从 build.rs 生成的全量名单派生（shipped-games.json 唯一登记）：
        // 运行时 roster 必须与 manifest 派生集精确相等——manifest 之外的目录混进
        // 二进制、或登记了却没被装载，这里都第一个红。顺序无关（装载顺序不是
        // 本测试守的契约，non_trainable 才是——上下两处断言）。
        let mut expected = vec!["synthetic-win32-dev".to_owned()];
        expected.extend(EMBEDDED_DEV_GAME_IDS.iter().map(|id| (*id).to_owned()));
        expected.sort();
        let mut actual = game_ids.clone();
        actual.sort();
        assert_eq!(actual, expected);
        let catalog = tracker.catalog.read();
        assert!(catalog.games().all(|game| {
            catalog
                .capture(&game.recording_profile_id)
                .is_some_and(|profile| profile.non_trainable)
        }));
    }

    #[cfg(feature = "dev-catalog")]
    #[test]
    #[ignore = "requires capture-lab --release -- --window-only on an interactive Windows desktop"]
    fn current_synthetic_window_discovery_smoke() {
        let tracker = WindowTracker::build().unwrap();
        let candidates = tracker.discover("synthetic-win32-dev").unwrap();

        assert!(
            candidates.iter().any(|candidate| {
                candidate
                    .executable_name
                    .eq_ignore_ascii_case("capture-lab.exe")
                    && candidate.client_width == 1920
                    && candidate.client_height == 1080
            }),
            "live capture-lab window was not discovered: {candidates:#?}"
        );
    }

    #[cfg(feature = "dev-catalog")]
    #[test]
    #[ignore = "requires Cyberpunk 2077 Steam build 20383525 running on an interactive Windows desktop"]
    fn current_cp2077_local_d7_window_discovery_and_binding_smoke() {
        let _ =
            unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        let tracker = WindowTracker::build().unwrap();
        let candidates = tracker.discover("cyberpunk-2077-steam").unwrap();
        let candidate = candidates
            .iter()
            .find(|candidate| {
                candidate
                    .executable_name
                    .eq_ignore_ascii_case("Cyberpunk2077.exe")
                    && candidate.client_width == 1920
                    && candidate.client_height == 1080
            })
            .unwrap_or_else(|| {
                let process_names = process_name_snapshot().unwrap();
                let observed = enumerate_top_level_windows()
                    .unwrap()
                    .into_iter()
                    .filter(|hwnd| {
                        window_process_name(*hwnd, &process_names)
                            .is_some_and(|name| name.eq_ignore_ascii_case("Cyberpunk2077.exe"))
                    })
                    .map(|hwnd| {
                        (
                            window_class(hwnd),
                            tracker.inspect_window(hwnd, tracker.catalog.read().game("cyberpunk-2077-steam").unwrap(), tracker.catalog.read().capture("cyberpunk-2077-steam-recording-v1").unwrap()),
                        )
                    })
                    .collect::<Vec<_>>();
                panic!(
                    "live Cyberpunk 2077 window was not discovered: {candidates:#?}; observations: {observed:#?}"
                )
            });

        let bound = tracker.bind(&candidate.candidate_id).unwrap();
        assert_eq!(bound.summary.game_id, "cyberpunk-2077-steam");
        assert_eq!(bound.summary.executable_name, "Cyberpunk2077.exe");
        assert_eq!(
            (bound.summary.client_width, bound.summary.client_height),
            (1920, 1080)
        );
        assert_eq!(
            bound.executable_sha256,
            Some(
                "sha256:a7de82945c03e041fc7339fcf9066224d98db2f5d80fea50f7947bb350a60991"
                    .to_owned()
            )
        );
        assert!(bound.summary.non_trainable);
    }

    #[cfg(feature = "dev-catalog")]
    #[test]
    #[ignore = "requires Nioh 2 Steam build 10049618 running on an interactive Windows desktop"]
    fn current_nioh2_window_discovery_and_binding_smoke() {
        let tracker = WindowTracker::build().unwrap();
        let candidates = tracker.discover("nioh2-steam").unwrap();
        let candidate = candidates
            .iter()
            .find(|candidate| {
                candidate.executable_name.eq_ignore_ascii_case("nioh2.exe")
                    && candidate.client_width == 2048
                    && candidate.client_height == 1152
            })
            .unwrap_or_else(|| panic!("live Nioh 2 window was not discovered: {candidates:#?}"));

        let bound = tracker.bind(&candidate.candidate_id).unwrap();
        assert_eq!(bound.summary.game_id, "nioh2-steam");
        assert_eq!(bound.summary.executable_name, "nioh2.exe");
        assert_eq!(
            (bound.summary.client_width, bound.summary.client_height),
            (2048, 1152)
        );
        assert_eq!(
            bound.executable_sha256,
            Some(
                "sha256:d8aedf1a5ba4d5bd5dc5cb84904c60988ad1574c06ed214d254fd0c673aa39a7"
                    .to_owned()
            )
        );
        assert!(bound.summary.non_trainable);
    }

    #[cfg(feature = "dev-catalog")]
    #[test]
    #[ignore = "requires Bannerlord Steam build 24127665 running on an interactive Windows desktop"]
    fn current_bannerlord_window_discovery_and_binding_smoke() {
        let tracker = WindowTracker::build().unwrap();
        let candidates = tracker
            .discover("mount-and-blade-2-bannerlord-steam")
            .unwrap();
        let candidate = candidates
            .iter()
            .find(|candidate| {
                candidate
                    .executable_name
                    .eq_ignore_ascii_case("Bannerlord.exe")
                    && candidate.client_width == 2048
                    && candidate.client_height == 1152
            })
            .unwrap_or_else(|| {
                panic!("live Bannerlord window was not discovered: {candidates:#?}")
            });

        let bound = tracker.bind(&candidate.candidate_id).unwrap();
        assert_eq!(bound.summary.game_id, "mount-and-blade-2-bannerlord-steam");
        assert_eq!(bound.summary.executable_name, "Bannerlord.exe");
        assert_eq!(
            (bound.summary.client_width, bound.summary.client_height),
            (2048, 1152)
        );
        assert_eq!(
            bound.executable_sha256,
            Some(
                "sha256:5fc59a9da65474a707fd3e868a24c6f0a782e9364edd4d17a8f8813171237de0"
                    .to_owned()
            )
        );
        assert!(bound.summary.non_trainable);
    }

    #[cfg(feature = "dev-catalog")]
    #[test]
    #[ignore = "requires Fallout 4 Steam build 22848228 running on an interactive Windows desktop"]
    fn current_fallout4_window_discovery_and_binding_smoke() {
        let _ =
            unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        let tracker = WindowTracker::build().unwrap();
        let candidates = tracker.discover("fallout4-steam-release-test").unwrap();
        let candidate = candidates
            .iter()
            .find(|candidate| {
                candidate.executable_name.eq_ignore_ascii_case("Fallout4.exe")
                    && candidate.client_width == 1920
                    && candidate.client_height == 1080
            })
            .unwrap_or_else(|| {
                let process_names = process_name_snapshot().unwrap();
                let observed = enumerate_top_level_windows()
                    .unwrap()
                    .into_iter()
                    .filter(|hwnd| {
                        window_process_name(*hwnd, &process_names)
                            .is_some_and(|name| name.eq_ignore_ascii_case("Fallout4.exe"))
                    })
                    .map(window_class)
                    .collect::<Vec<_>>();
                panic!(
                    "live Fallout 4 window was not discovered: {candidates:#?}; classes: {observed:#?}"
                )
            });

        let bound = tracker.bind(&candidate.candidate_id).unwrap();
        assert_eq!(bound.summary.game_id, "fallout4-steam-release-test");
        assert_eq!(bound.summary.executable_name, "Fallout4.exe");
        assert_eq!(
            (bound.summary.client_width, bound.summary.client_height),
            (1920, 1080)
        );
        assert_eq!(
            bound.executable_sha256,
            Some(
                "sha256:428f9996cc4248e26c0f62f9fdd3eaf0e5eb305834b67ee5996538e593218b61"
                    .to_owned()
            )
        );
        assert!(bound.summary.non_trainable);
    }

    #[cfg(feature = "dev-catalog")]
    #[test]
    #[ignore = "requires Hades II Steam build 24432219 running on an interactive Windows desktop"]
    fn current_hades_ii_window_discovery_and_binding_smoke() {
        let _ =
            unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        let tracker = WindowTracker::build().unwrap();
        let candidates = tracker.discover("hades-ii-steam-release-test").unwrap();
        let candidate = candidates
            .iter()
            .find(|candidate| {
                candidate.executable_name.eq_ignore_ascii_case("Hades2.exe")
                    && candidate.client_width == 2560
                    && candidate.client_height == 1440
            })
            .unwrap_or_else(|| panic!("live Hades II window was not discovered: {candidates:#?}"));

        let bound = tracker.bind(&candidate.candidate_id).unwrap();
        assert_eq!(bound.summary.game_id, "hades-ii-steam-release-test");
        assert_eq!(bound.summary.executable_name, "Hades2.exe");
        assert_eq!(
            (bound.summary.client_width, bound.summary.client_height),
            (2560, 1440)
        );
        assert_eq!(
            bound.executable_sha256,
            Some(
                "sha256:df11c588771105d6e90885a949c512044268ba15b175220decb168da93763ecf"
                    .to_owned()
            )
        );
        assert!(bound.summary.non_trainable);
    }

    #[cfg(feature = "dev-catalog")]
    #[test]
    #[ignore = "requires A Plague Tale: Innocence Steam build 4336652 running on an interactive Windows desktop"]
    fn current_a_plague_tale_window_discovery_and_binding_smoke() {
        let _ =
            unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        let tracker = WindowTracker::build().unwrap();
        let candidates = tracker
            .discover("a-plague-tale-innocence-steam-release-test")
            .unwrap();
        let candidate = candidates
            .iter()
            .find(|candidate| {
                candidate
                    .executable_name
                    .eq_ignore_ascii_case("APlagueTaleInnocence_x64.exe")
                    && candidate.client_width == 2560
                    && candidate.client_height == 1440
            })
            .unwrap_or_else(|| {
                panic!("live A Plague Tale window was not discovered: {candidates:#?}")
            });

        let bound = tracker.bind(&candidate.candidate_id).unwrap();
        assert_eq!(
            bound.summary.game_id,
            "a-plague-tale-innocence-steam-release-test"
        );
        assert_eq!(
            bound.summary.executable_name,
            "APlagueTaleInnocence_x64.exe"
        );
        assert_eq!(
            (bound.summary.client_width, bound.summary.client_height),
            (2560, 1440)
        );
        assert_eq!(
            bound.executable_sha256,
            Some(
                "sha256:fb5f31c3bd07f89142281fa8469fc4db7811e94992c7a46fdc14e0f5af07b329"
                    .to_owned()
            )
        );
        assert!(bound.summary.non_trainable);
    }

    #[cfg(feature = "dev-catalog")]
    #[test]
    #[ignore = "requires Halo Infinite Steam build 22709428 running on an interactive Windows desktop"]
    fn current_halo_infinite_window_discovery_and_binding_smoke() {
        let _ =
            unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        let tracker = WindowTracker::build().unwrap();
        let candidates = tracker
            .discover("halo-infinite-steam-release-test")
            .unwrap();
        let candidate = candidates
            .iter()
            .find(|candidate| {
                candidate
                    .executable_name
                    .eq_ignore_ascii_case("HaloInfinite.exe")
                    && candidate.client_width == 2560
                    && candidate.client_height == 1440
            })
            .unwrap_or_else(|| {
                panic!("live Halo Infinite window was not discovered: {candidates:#?}")
            });

        let process_names = process_name_snapshot().unwrap();
        let observed_classes = enumerate_top_level_windows()
            .unwrap()
            .into_iter()
            .filter(|hwnd| {
                window_process_name(*hwnd, &process_names)
                    .is_some_and(|name| name.eq_ignore_ascii_case("HaloInfinite.exe"))
            })
            .map(window_class)
            .collect::<Vec<_>>();
        assert!(
            observed_classes
                .iter()
                .any(|class| { class.as_deref().is_ok_and(|class| class == "Halo Infinite") }),
            "unexpected Halo Infinite window classes: {observed_classes:#?}"
        );

        let bound = tracker.bind(&candidate.candidate_id).unwrap();
        assert_eq!(bound.summary.game_id, "halo-infinite-steam-release-test");
        assert_eq!(bound.summary.executable_name, "HaloInfinite.exe");
        assert_eq!(
            (bound.summary.client_width, bound.summary.client_height),
            (2560, 1440)
        );
        assert_eq!(
            bound.executable_sha256,
            Some(
                "sha256:5da518ec21f5ab7baeb021aeed2b9892eeb65320dc17d2682f8f55271c203da8"
                    .to_owned()
            )
        );
        assert_eq!(bound.executable_sha256_missing_reason, None);
        assert!(bound.summary.non_trainable);
    }

    #[cfg(feature = "dev-catalog")]
    #[test]
    #[ignore = "requires Halo: The Master Chief Collection Steam build 19905945 running on an interactive Windows desktop"]
    fn current_halo_reach_mcc_window_discovery_and_binding_smoke() {
        let _ =
            unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        let tracker = WindowTracker::build().unwrap();
        let candidates = tracker.discover("halo-reach-mcc-s0").unwrap();
        let candidate = candidates
            .iter()
            .find(|candidate| {
                candidate
                    .executable_name
                    .eq_ignore_ascii_case("MCC-Win64-Shipping.exe")
                    && candidate.client_width == 2560
                    && candidate.client_height == 1440
            })
            .unwrap_or_else(|| panic!("live Halo MCC window was not discovered: {candidates:#?}"));

        let process_names = process_name_snapshot().unwrap();
        let observed_classes = enumerate_top_level_windows()
            .unwrap()
            .into_iter()
            .filter(|hwnd| {
                window_process_name(*hwnd, &process_names)
                    .is_some_and(|name| name.eq_ignore_ascii_case("MCC-Win64-Shipping.exe"))
            })
            .map(window_class)
            .collect::<Vec<_>>();
        assert!(
            observed_classes
                .iter()
                .any(|class| { class.as_deref().is_ok_and(|class| class == "UnrealWindow") }),
            "unexpected Halo MCC window classes: {observed_classes:#?}"
        );

        let bound = tracker.bind(&candidate.candidate_id).unwrap();
        assert_eq!(bound.summary.game_id, "halo-reach-mcc-s0");
        assert_eq!(bound.summary.executable_name, "MCC-Win64-Shipping.exe");
        assert_eq!(
            (bound.summary.client_width, bound.summary.client_height),
            (2560, 1440)
        );
        assert_eq!(
            bound.executable_sha256,
            Some(
                "sha256:be70d6dcd1a884f10ceb342a7a2dcb35ee0fa43181b66a1d19c3d830e9834691"
                    .to_owned()
            )
        );
        assert_eq!(bound.executable_sha256_missing_reason, None);
        assert!(bound.summary.non_trainable);
    }

    #[cfg(feature = "dev-catalog")]
    #[test]
    #[ignore = "requires ELDEN RING NIGHTREIGN Steam build 22818764 running on an interactive Windows desktop"]
    fn current_nightreign_window_discovery_and_binding_smoke() {
        let _ =
            unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        let tracker = WindowTracker::build().unwrap();
        let candidates = tracker
            .discover("elden-ring-nightreign-steam-release-test")
            .unwrap();
        let candidate = candidates
            .iter()
            .find(|candidate| {
                candidate
                    .executable_name
                    .eq_ignore_ascii_case("nightreign.exe")
                    && candidate.client_width == 1920
                    && candidate.client_height == 1080
            })
            .unwrap_or_else(|| {
                panic!("live NIGHTREIGN window was not discovered: {candidates:#?}")
            });

        let bound = tracker.bind(&candidate.candidate_id).unwrap();
        assert_eq!(
            bound.summary.game_id,
            "elden-ring-nightreign-steam-release-test"
        );
        assert_eq!(bound.summary.executable_name, "nightreign.exe");
        assert_eq!(
            (bound.summary.client_width, bound.summary.client_height),
            (1920, 1080)
        );
        assert_eq!(
            bound.executable_sha256,
            Some(
                "sha256:9545e8732919bb6a63904cb73223641b3c4f9a67869be5722612acca413e8b3a"
                    .to_owned()
            )
        );
        assert_eq!(bound.executable_sha256_missing_reason, None);
        assert!(bound.summary.non_trainable);
    }

    #[cfg(feature = "dev-catalog")]
    #[test]
    fn marker_hotkey_is_not_persisted_as_raw_game_input() {
        let tracker = WindowTracker::build().unwrap();

        assert!(tracker.catalog.read().games().all(|game| {
            !game.input_contract.allows_key(120) && !game.input_contract.is_control_key(120)
        }));
    }

    #[test]
    fn foreground_activation_wait_accepts_a_delayed_success() {
        let mut attempts = 0;
        assert!(wait_until(Duration::from_secs(1), Duration::ZERO, || {
            attempts += 1;
            attempts == 3
        }));
        assert_eq!(attempts, 3);
    }

    #[test]
    fn foreground_activation_wait_rejects_a_timeout() {
        assert!(!wait_until(Duration::ZERO, Duration::ZERO, || false));
    }

    #[test]
    fn configured_window_class_remains_exact_when_present() {
        assert!(window_class_matches("IW8", "IW8"));
        assert!(!window_class_matches("IW8", "OtherWindowClass"));
    }

    #[cfg(feature = "dev-catalog")]
    #[test]
    fn cod_release_test_recognizes_only_the_configured_executable_name() {
        // 名单匹配是**全部游戏**共用的选窗谓词（docs/55 §3.1）；这里仍单独测
        // COD，是因为它的名单最长、最容易在改目录时漏掉一项。
        let tracker = WindowTracker::build().unwrap();
        let catalog = tracker.catalog.read();
        let game = catalog.game("call-of-duty-steam-release-test").unwrap();
        assert!(executable_name_matches(game, "cod.exe"));
        assert!(executable_name_matches(game, "COD.EXE"));
        assert!(executable_name_matches(game, "cod22-cod.exe"));
        assert!(executable_name_matches(game, "COD23-COD.EXE"));
        assert!(executable_name_matches(game, "cod24-cod.exe"));
        assert!(executable_name_matches(game, "sp22-cod.exe"));
        assert!(executable_name_matches(game, "SP22-COD.EXE"));
        assert!(executable_name_matches(game, "ModernWarfare.exe"));
        assert!(executable_name_matches(game, "modernwarfare.exe"));
        assert!(!executable_name_matches(game, "cod25-cod.exe"));
        assert!(!executable_name_matches(game, "codCrashHandler.exe"));
        // 目录里没钉指纹 —— 于是观察失败时走降级路径而不是拒绝绑定。
        assert_eq!(game.executable_sha256, None);
    }

    #[test]
    fn probe_failure_reason_keeps_the_whole_error_chain_for_troubleshooting() {
        // 负责人裁决 2026-07-28：标注员是内部员工，排查"指纹为什么没测到"时
        // 完整路径与系统错误原文远比一个错误码有用（docs/55 §3.1）。
        let denied = anyhow::Error::new(std::io::Error::from_raw_os_error(5))
            .context("failed to open C:\\Users\\Example\\Games\\cod.exe");
        let reason = probe_failure_reason("open_process_denied", &denied);
        assert!(reason.starts_with("open_process_denied: "));
        // 路径要留着——这正是这条值的用处。
        assert!(reason.contains("C:\\Users\\Example\\Games\\cod.exe"));
        // 整条 context 链都要在，不只是最外层那一句。
        assert!(reason.contains("os error 5"));
    }

    #[test]
    fn probe_failure_reason_stays_within_the_column_bound() {
        // 上限与隐私无关，只防失控的错误链把一列撑爆；截断必须落在字符边界上
        // ——系统错误消息是本地化的，按字节切会切碎 UTF-8。
        let bloated = anyhow::anyhow!("路径".repeat(4_000));
        let reason = probe_failure_reason("executable_read_failed", &bloated);
        let chars = reason.chars().count();
        assert_eq!(chars, EXECUTABLE_MISSING_REASON_MAX_CHARS);
        assert!(reason.ends_with('…'));
        // 能重新解析成 &str 就说明没有切碎多字节字符（String 本身即保证 UTF-8，
        // 这里断言的是截断点没有 panic 且长度按字符算）。
        assert!(reason.is_char_boundary(reason.len()));

        let short = probe_failure_reason("open_process_denied", &anyhow::anyhow!("拒绝访问"));
        assert_eq!(short, "open_process_denied: 拒绝访问");
    }

    #[test]
    fn identity_comparison_uses_every_piece_of_evidence_that_was_captured() {
        let observation = |pid: u32, name: &str, identity: ExecutableIdentity| WindowObservation {
            hwnd_value: 0x1234,
            root_owner_value: 0x1234,
            process_id: pid,
            executable_name: name.into(),
            identity,
            client_width: 1920,
            client_height: 1080,
            dpi: 96,
            foreground: true,
        };
        let measured = |sha: &str| ExecutableIdentity {
            creation_time_100ns: Some(99),
            sha256: Some(sha.to_owned()),
            missing_reason: None,
            verified: None,
        };
        let nothing = || ExecutableIdentity {
            missing_reason: Some("open_process_denied: …".into()),
            ..ExecutableIdentity::default()
        };
        // L1 成功、读文件失败：创建时刻仍然是有效证据，必须参与比对。
        let partial = || ExecutableIdentity {
            creation_time_100ns: Some(99),
            missing_reason: Some("executable_read_failed: …".into()),
            ..ExecutableIdentity::default()
        };

        let same = |a: &WindowObservation, b: &WindowObservation| identity_unchanged(a, b);
        assert!(same(
            &observation(42, "cod.exe", measured("sha256:a")),
            &observation(42, "COD.EXE", measured("sha256:a"))
        ));
        // 换了 build。
        assert!(!same(
            &observation(42, "cod.exe", measured("sha256:a")),
            &observation(42, "cod.exe", measured("sha256:b"))
        ));
        // 换了进程——即使证据一层都没测到，PID 也必须比。这是句柄值被复用的信号。
        assert!(!same(
            &observation(42, "cod.exe", nothing()),
            &observation(43, "cod.exe", nothing())
        ));
        // 什么都没测到时，同名同 PID 视为未变。
        assert!(same(
            &observation(42, "cod.exe", nothing()),
            &observation(42, "COD.EXE", nothing())
        ));
        assert!(!same(
            &observation(42, "cod.exe", nothing()),
            &observation(42, "notepad.exe", nothing())
        ));
        // 部分证据不能被丢：创建时刻变了要能发现。
        assert!(same(
            &observation(42, "cod.exe", partial()),
            &observation(42, "cod.exe", partial())
        ));
        assert!(!same(
            &observation(42, "cod.exe", partial()),
            &observation(42, "cod.exe", nothing())
        ));
        // 曾经测到过指纹，现在测不到了，同样是变化。
        assert!(!same(
            &observation(42, "cod.exe", measured("sha256:a")),
            &observation(42, "cod.exe", partial())
        ));
    }

    #[test]
    fn process_snapshot_reports_the_current_executable_without_opening_it() {
        let names = process_name_snapshot().unwrap();
        let executable_name = names
            .get(&std::process::id())
            .expect("current process must appear in the Tool Help snapshot");
        assert!(executable_name.to_ascii_lowercase().ends_with(".exe"));
    }

    #[test]
    fn candidate_token_does_not_expose_native_handle() {
        let path = temp_executable_path("candidate-token");
        std::fs::write(&path, b"candidate token fixture").unwrap();
        let verified_executable = ExecutableHashCache::default().verify(&path, None).unwrap();
        let observation = WindowObservation {
            hwnd_value: 0x1234,
            root_owner_value: 0x1234,
            process_id: 42,
            executable_name: "capture-lab.exe".into(),
            identity: ExecutableIdentity {
                creation_time_100ns: Some(99),
                sha256: Some(verified_executable.sha256.clone()),
                missing_reason: None,
                verified: Some(verified_executable),
            },
            client_width: 1920,
            client_height: 1080,
            dpi: 96,
            foreground: true,
        };
        let token = candidate_id(7, &observation);
        // Raw HWND/PID values are inputs to SHA-256, so their decimal or hex
        // digits may still occur by chance inside the digest. Verify the
        // opaque token contract instead of using a flaky substring check.
        let digest = token.strip_prefix("candidate-").unwrap();
        assert_eq!(digest.len(), 64);
        assert!(
            digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        );
        drop(observation);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn executable_hash_cache_reuses_and_pins_verified_file() {
        let path = temp_executable_path("executable-cache");
        std::fs::write(&path, b"first executable bytes").unwrap();
        let cache = ExecutableHashCache::default();
        let first = cache.verify(&path, None).unwrap();
        let second = cache.verify(&path, None).unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(first.sha256, second.sha256);
        assert!(std::fs::write(&path, b"replacement bytes").is_err());

        let first_sha256 = first.sha256.clone();
        drop((first, second));
        std::fs::write(&path, b"replacement bytes").unwrap();
        let replacement = cache.verify(&path, None).unwrap();
        assert_ne!(replacement.sha256, first_sha256);
        drop(replacement);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn monitor_stop_is_linearized_on_both_sides_of_commit_gate() {
        let (precommit, precommit_controls) = monitor_context_for_test(false);
        precommit.input_status.close_gate();
        route_monitor_stop(&precommit, RecordingStopReason::WindowDestroyed, 100, None);
        assert!(!precommit.input_status.is_accepting());
        assert!(precommit_controls.try_recv().is_err());
        assert_eq!(
            precommit
                .startup_invalid
                .lock()
                .unwrap()
                .expect("startup invalidation was not retained")
                .cutoff_qpc,
            100
        );

        let (postcommit, postcommit_controls) = monitor_context_for_test(false);
        postcommit.input_status.close_gate();
        let startup_invalid = postcommit.startup_invalid.lock().unwrap();
        assert!(
            TargetMonitorCommitGuard {
                active: &postcommit.active,
                input_status: &postcommit.input_status,
                startup_invalid,
            }
            .activate_and_open()
        );
        assert!(postcommit.input_status.is_accepting());
        route_monitor_stop(&postcommit, RecordingStopReason::GeometryChanged, 200, None);
        assert!(!postcommit.input_status.is_accepting());
        assert_eq!(postcommit.cutoff_qpc.load(Ordering::Acquire), 200);
        assert_eq!(postcommit_controls.try_recv().unwrap().cutoff_qpc, 200);

        let (input_fault, input_fault_controls) = monitor_context_for_test(false);
        input_fault.input_status.close_gate();
        route_monitor_stop(
            &input_fault,
            RecordingStopReason::InputFault,
            300,
            Some(InputFaultSpec {
                overflow: false,
                reason: "device removed".into(),
            }),
        );
        assert!(input_fault.input_status.faulted());
        assert!(!input_fault.input_status.is_accepting());
        assert!(input_fault_controls.try_recv().is_err());
        assert_eq!(
            input_fault
                .startup_invalid
                .lock()
                .unwrap()
                .expect("input fault was not retained before commit")
                .cutoff_qpc,
            300
        );

        let (race, race_controls) = monitor_context_for_test(false);
        race.input_status.close_gate();
        let startup_invalid = race.startup_invalid.lock().unwrap();
        let race_callback = race.clone();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let callback_barrier = Arc::clone(&barrier);
        let callback = std::thread::spawn(move || {
            callback_barrier.wait();
            route_monitor_stop(
                &race_callback,
                RecordingStopReason::WindowDestroyed,
                400,
                None,
            );
        });
        barrier.wait();
        assert!(
            TargetMonitorCommitGuard {
                active: &race.active,
                input_status: &race.input_status,
                startup_invalid,
            }
            .activate_and_open()
        );
        callback.join().unwrap();
        assert!(!race.input_status.is_accepting());
        assert_eq!(race_controls.try_recv().unwrap().cutoff_qpc, 400);
    }

    #[test]
    fn raw_keyboard_keeps_each_contract_event_and_filters_other_keys() {
        let (mut input, records) = input_thread_for_test();
        let device = fake_device();
        let key = windows::Win32::UI::Input::RAWKEYBOARD {
            MakeCode: 17,
            VKey: 87,
            ..Default::default()
        };
        input.handle_keyboard(device, key, 100).unwrap();
        input.handle_keyboard(device, key, 101).unwrap();
        input
            .handle_keyboard(
                device,
                windows::Win32::UI::Input::RAWKEYBOARD {
                    Flags: RI_KEY_BREAK as u16,
                    ..key
                },
                102,
            )
            .unwrap();
        input
            .handle_keyboard(
                device,
                windows::Win32::UI::Input::RAWKEYBOARD { VKey: 66, ..key },
                103,
            )
            .unwrap();

        let packets = records
            .try_iter()
            .filter_map(|record| match record {
                RawInputRecord::Event(packet) => Some(packet),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(packets.len(), 3);
        assert_eq!(
            packets.iter().map(|packet| packet.qpc).collect::<Vec<_>>(),
            [100, 101, 102]
        );
        assert!(matches!(
            packets[1].kind,
            RawInputKind::Key { repeat: true, .. }
        ));
        assert!(input.neutral_at(103));
    }

    #[test]
    fn raw_input_arming_rejects_a_pressed_contract_key() {
        let (mut input, _records) = input_thread_for_test();
        input
            .handle_keyboard(
                fake_device(),
                windows::Win32::UI::Input::RAWKEYBOARD {
                    MakeCode: 17,
                    VKey: 87,
                    ..Default::default()
                },
                90,
            )
            .unwrap();
        assert!(!input.neutral_at(100));
        assert_eq!(input.held_at(100), ([0, 1 << (87 % 64), 0, 0], 0));
    }

    #[test]
    fn arming_diagnostics_use_pre_anchor_state_without_repairing_input() {
        let (mut input, _records) = input_thread_for_test();
        input.arming_transitions = vec![
            ArmingTransition {
                qpc: 90,
                control: ArmingControl::Key(87),
                down: true,
                application_control: false,
            },
            ArmingTransition {
                qpc: 91,
                control: ArmingControl::Key(87),
                down: true,
                application_control: false,
            },
            ArmingTransition {
                qpc: 92,
                control: ArmingControl::Mouse(2),
                down: true,
                application_control: false,
            },
            ArmingTransition {
                qpc: 93,
                control: ArmingControl::Key(18),
                down: true,
                application_control: true,
            },
            ArmingTransition {
                qpc: 100,
                control: ArmingControl::Key(87),
                down: false,
                application_control: false,
            },
            ArmingTransition {
                qpc: 101,
                control: ArmingControl::Mouse(2),
                down: false,
                application_control: false,
            },
        ];
        assert_eq!(input.held_at(100), ([0, 1 << (87 % 64), 0, 0], 2));
        assert!(!input.neutral_at(100));
        assert_eq!(input.held_at(101), ([0; 4], 2));
        assert!(input.neutral_at(102));
        // Looking back after the later releases must still report the original
        // non-neutral anchor, without clearing or rewriting the transitions.
        assert!(!input.neutral_at(100));
        assert_eq!(input.arming_transitions.len(), 6);
    }

    fn target_window_scope() -> WinEventScope {
        WinEventScope {
            window_object: true,
            hwnd_is_target: true,
            root_is_target: true,
        }
    }

    #[test]
    fn caret_and_cursor_lifecycle_is_not_a_window_destroy() {
        let caret = WinEventScope {
            window_object: false,
            ..target_window_scope()
        };

        assert_eq!(
            caret.stop_reason(EVENT_OBJECT_DESTROY, || -> bool {
                unreachable!("geometry must not be probed for a non-window object")
            }),
            None
        );
        assert_eq!(
            caret.stop_reason(EVENT_OBJECT_LOCATIONCHANGE, || true),
            None
        );
        assert!(!caret.affects_overlay_placement(EVENT_OBJECT_LOCATIONCHANGE));

        assert_eq!(
            target_window_scope().stop_reason(EVENT_OBJECT_DESTROY, || false),
            Some(RecordingStopReason::WindowDestroyed)
        );
    }

    #[test]
    fn foreground_loss_stays_sensitive_to_non_window_objects() {
        // Missing a foreground loss keeps Raw Input recording keystrokes aimed at
        // another application, so the system events must not inherit the object gate.
        let foreign = WinEventScope {
            window_object: false,
            hwnd_is_target: false,
            root_is_target: false,
        };

        assert_eq!(
            foreign.stop_reason(EVENT_SYSTEM_FOREGROUND, || false),
            Some(RecordingStopReason::ForegroundLost)
        );
        assert_eq!(
            target_window_scope().stop_reason(EVENT_SYSTEM_FOREGROUND, || false),
            None
        );
        assert_eq!(
            WinEventScope {
                window_object: false,
                ..target_window_scope()
            }
            .stop_reason(EVENT_SYSTEM_MINIMIZESTART, || false),
            Some(RecordingStopReason::WindowMinimized)
        );
    }

    #[test]
    fn geometry_is_only_probed_for_the_target_window_itself() {
        assert_eq!(
            target_window_scope().stop_reason(EVENT_OBJECT_LOCATIONCHANGE, || true),
            Some(RecordingStopReason::GeometryChanged)
        );
        assert_eq!(
            target_window_scope().stop_reason(EVENT_OBJECT_LOCATIONCHANGE, || false),
            None
        );
        assert_eq!(
            WinEventScope {
                hwnd_is_target: false,
                ..target_window_scope()
            }
            .stop_reason(EVENT_OBJECT_LOCATIONCHANGE, || -> bool {
                unreachable!("geometry must not be probed for another window")
            }),
            None
        );
    }

    #[test]
    fn absolute_mouse_is_a_profile_mismatch() {
        let (mut input, _records) = input_thread_for_test();
        let mouse = windows::Win32::UI::Input::RAWMOUSE {
            usFlags: MOUSE_MOVE_ABSOLUTE,
            lLastX: 1,
            ..Default::default()
        };
        assert!(input.handle_mouse(fake_device(), mouse, 100).is_err());
    }
}

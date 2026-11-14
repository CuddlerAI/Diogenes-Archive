//! 录制期设置文件监视（C4，docs/45 §6.1、docs/46 §4.3）：对目录政策
//! 声明的白名单配置文件做内容指纹轮询；录制期间文件变更（含出现/消失）
//! 立即经既有 cutoff 通道送 `SettingsChanged` 停止当前 run，并把关联的
//! 视角标定标记为失效（`config_watch`）。
//!
//! 设计边界：
//! - 轮询（2 秒）而非 ReadDirectoryChangesW——文件少且小，轮询实现无
//!   平台回调复杂度；变更检测按内容 SHA-256，写而未变不触发；
//! - 配置根 = 游戏进程可执行文件所在目录（由 pid 现场解析，不改动
//!   BoundTarget 的审计面）；Documents 型配置根待 H-1 确认真实路径后
//!   再扩展；解析失败按"监视缺席"记日志并不起线程，不阻塞录制；
//! - 误报安全：宁可多停一次录制（重新对表即可），不可漏报设置变更。

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use capture_core::qpc::QpcClock;
use capture_core::recording::{RecordingStopReason, RecordingStopSignal};
use crossbeam_channel::Sender;
use sha2::{Digest, Sha256};
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_FORMAT, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};

use crate::services::journal::SessionJournal;

const POLL_INTERVAL: Duration = Duration::from_millis(500);
const CHECK_EVERY_TICKS: u32 = 4; // 500ms × 4 = 2 秒一次指纹核对

pub struct SettingsWatchSpec {
    pub process_id: u32,
    /// 相对游戏安装根（可执行文件所在目录）的路径。
    pub relative_paths: Vec<String>,
    /// 门通过时的全量标定 id；变更时据此失效。
    pub calibration_id: Option<String>,
}

/// 随 run 生命周期同生死的监视线程句柄；Drop 即停并 join。
pub struct SettingsWatch {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl SettingsWatch {
    /// 起监视线程。路径解析失败或列表为空 → None（监视缺席，只记日志）。
    pub fn start(
        spec: SettingsWatchSpec,
        stop_tx: Sender<RecordingStopSignal>,
        journal: Arc<SessionJournal>,
    ) -> Option<Self> {
        if spec.relative_paths.is_empty() {
            return None;
        }
        let Some(install_root) = resolve_install_root(spec.process_id) else {
            tracing::warn!(
                process_id = spec.process_id,
                "settings watch absent: could not resolve the game install root"
            );
            return None;
        };
        let files: Vec<PathBuf> = spec
            .relative_paths
            .iter()
            .map(|relative| install_root.join(relative))
            .collect();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let calibration_id = spec.calibration_id;
        tracing::info!(files = files.len(), root = %install_root.display(), "settings watch armed");
        let thread = std::thread::Builder::new()
            .name("dm-settings-watch".into())
            .spawn(move || watch_loop(&files, &thread_stop, &stop_tx, &journal, calibration_id))
            .ok()?;
        Some(Self {
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for SettingsWatch {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn watch_loop(
    files: &[PathBuf],
    stop: &AtomicBool,
    stop_tx: &Sender<RecordingStopSignal>,
    journal: &SessionJournal,
    calibration_id: Option<String>,
) {
    let baseline: Vec<Option<String>> = files.iter().map(fingerprint).collect();
    let mut tick = 0_u32;
    loop {
        if stop.load(Ordering::Acquire) {
            return;
        }
        std::thread::sleep(POLL_INTERVAL);
        tick += 1;
        if !tick.is_multiple_of(CHECK_EVERY_TICKS) {
            continue;
        }
        for (path, expected) in files.iter().zip(&baseline) {
            if stop.load(Ordering::Acquire) {
                return;
            }
            if fingerprint(path) != *expected {
                tracing::warn!(path = %path.display(), "settings file changed during recording");
                let cutoff_qpc = QpcClock::new()
                    .and_then(|clock| clock.now())
                    .unwrap_or_default();
                let _ = stop_tx.try_send(RecordingStopSignal {
                    reason: RecordingStopReason::SettingsChanged,
                    cutoff_qpc,
                });
                if let Some(calibration_id) = &calibration_id
                    && let Err(error) =
                        journal.invalidate_view_calibration(calibration_id, "config_watch")
                {
                    tracing::warn!(error = %error, "failed to invalidate the view calibration");
                }
                return;
            }
        }
    }
}

/// 内容指纹：Some(sha256) / None（缺失或不可读，与存在态区分）。
fn fingerprint(path: &PathBuf) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Some(format!("{:x}", hasher.finalize()))
}

/// 由 pid 现场解析可执行文件所在目录，不触碰 BoundTarget 的审计面。
fn resolve_install_root(process_id: u32) -> Option<PathBuf> {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id).ok()?;
        let mut buffer = [0_u16; 1024];
        let mut length = buffer.len() as u32;
        let result = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_FORMAT(0),
            windows::core::PWSTR(buffer.as_mut_ptr()),
            &mut length,
        );
        let _ = CloseHandle(handle);
        result.ok()?;
        let path = String::from_utf16_lossy(&buffer[..length as usize]);
        PathBuf::from(path).parent().map(PathBuf::from)
    }
}

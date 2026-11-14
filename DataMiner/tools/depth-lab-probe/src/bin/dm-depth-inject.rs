//! `dm-depth-inject` —— 把 `dm_depth_probe.dll` 注入目标游戏进程。
//!
//! ```text
//! dm-depth-inject --pid <PID> --dll <绝对路径> --expected-sha256 <sha256:...>
//!                 [--expected-exe <绝对路径> --expected-exe-sha256 <sha256:...>]
//!                 [--expected-start-time-filetime <u64>]
//!                 [--allow-existing]
//! ```
//!
//! # 为什么注入器是独立二进制而不是客户端里的一个函数
//!
//! DataMiner 客户端（`apps/client`）**在开发机上无法交叉编译**——
//! `zstd-sys` 需要 MSVC 的 C 交叉工具链，实测 `cargo check --target x86_64-pc-windows-msvc`
//! 在 `zstd_v07.c` 处失败。把注入这段 `unsafe` 放进客户端就等于放进一个
//! 本机完全无法验证的地方。
//!
//! 独立成二进制之后：注入代码随 `dm-depth-probe` 一起被交叉编译验证，
//! 客户端只剩「起一个进程、把 env 摆对」这件不会出错的事。
//!
//! # 位数必须匹配
//!
//! DLL 与目标进程的位数不一致时**根本注不进去**（BioShock Infinite 是 32 位，#425）。
//! 本工具会先核对再动手，而不是让 `CreateRemoteThread` 以一个难解释的错误码失败。

#![cfg(windows)]

use std::io::Write;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use windows::Win32::Foundation::{CloseHandle, FILETIME, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::System::Diagnostics::Debug::WriteProcessMemory;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, MODULEENTRY32W, Module32FirstW, TH32CS_SNAPMODULE,
    TH32CS_SNAPMODULE32,
};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::System::Memory::{
    MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAllocEx, VirtualFreeEx,
};
use windows::Win32::System::ProcessStatus::K32GetModuleFileNameExW;
use windows::Win32::System::Threading::{
    CreateRemoteThread, GetProcessTimes, IsWow64Process, OpenProcess, PROCESS_CREATE_THREAD,
    PROCESS_QUERY_INFORMATION, PROCESS_VM_OPERATION, PROCESS_VM_READ, PROCESS_VM_WRITE,
    WaitForSingleObject,
};
use windows::core::w;

const LOAD_LIBRARY_TIMEOUT_MS: u32 = 30_000;
const GUARDED_INJECTION_CONTRACT: &str = "dm-depth-inject-guard/v2:start-filetime+loaded-anticheat-module-recheck+single-canonical-stdout";

fn usage() -> &'static str {
    "用法：dm-depth-inject --pid <PID> --dll <DLL 绝对路径> --expected-sha256 <sha256:...> \
     [--expected-exe <EXE 绝对路径> --expected-exe-sha256 <sha256:...>] \
     [--expected-start-time-filetime <u64>] [--allow-existing]"
}

enum InjectionSuccess {
    NewlyLoaded(String),
    ReusedResident(String),
}

impl InjectionSuccess {
    fn loaded_sha256(&self) -> &str {
        match self {
            Self::NewlyLoaded(hash) | Self::ReusedResident(hash) => hash,
        }
    }
}

fn write_successful_injection_stdout(
    mut writer: impl Write,
    success: &InjectionSuccess,
) -> std::io::Result<()> {
    writeln!(writer, "loaded_probe_sha256={}", success.loaded_sha256())
}

fn arg(args: &[String], key: &str) -> Option<String> {
    args.iter()
        .position(|a| a == key)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

/// 目标进程是不是 32 位。
///
/// `IsWow64Process` 在 64 位 Windows 上对 32 位进程返回 true。
fn target_is_32bit(process: HANDLE) -> Result<bool, String> {
    let mut wow64 = windows::core::BOOL(0);
    unsafe { IsWow64Process(process, &mut wow64) }
        .map_err(|e| format!("IsWow64Process 失败: {e}"))?;
    Ok(wow64.as_bool())
}

/// 本工具自身是不是 32 位构建。DLL 与本工具同架构，所以用它代表 DLL 位数。
const fn self_is_32bit() -> bool {
    cfg!(target_pointer_width = "32")
}

#[derive(Debug)]
struct LoadedModule {
    name: String,
    path: PathBuf,
}

struct SnapshotHandle(HANDLE);

impl Drop for SnapshotHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

fn module_path(
    process: HANDLE,
    module: windows::Win32::Foundation::HMODULE,
) -> Result<PathBuf, String> {
    let mut capacity = 512usize;
    loop {
        let mut buffer = vec![0u16; capacity];
        let written = unsafe { K32GetModuleFileNameExW(Some(process), Some(module), &mut buffer) };
        if written == 0 {
            return Err("K32GetModuleFileNameExW failed".into());
        }
        let written = written as usize;
        if written < buffer.len() {
            buffer.truncate(written);
            return Ok(PathBuf::from(String::from_utf16_lossy(&buffer)));
        }
        capacity = capacity
            .checked_mul(2)
            .filter(|next| *next <= 32_768)
            .ok_or("loaded module path exceeds 32768 UTF-16 code units")?;
    }
}

fn loaded_modules(pid: u32, process: HANDLE) -> Result<Vec<LoadedModule>, String> {
    let snapshot = SnapshotHandle(
        unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPMODULE | TH32CS_SNAPMODULE32, pid) }
            .map_err(|e| format!("CreateToolhelp32Snapshot 失败: {e}"))?,
    );
    let mut entry = MODULEENTRY32W {
        dwSize: core::mem::size_of::<MODULEENTRY32W>() as u32,
        ..Default::default()
    };
    let mut modules = Vec::new();
    if unsafe { Module32FirstW(snapshot.0, &mut entry) }.is_ok() {
        loop {
            let name = String::from_utf16_lossy(&entry.szModule)
                .trim_end_matches('\0')
                .to_ascii_lowercase();
            if is_probe_module(&name) {
                let path = module_path(process, entry.hModule)?;
                modules.push(LoadedModule { name, path });
            }
            if unsafe {
                windows::Win32::System::Diagnostics::ToolHelp::Module32NextW(snapshot.0, &mut entry)
            }
            .is_err()
            {
                break;
            }
        }
    }
    Ok(modules)
}

fn is_anti_cheat_module_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    let compact = name
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect::<String>();
    let first_token = lower
        .split(|character: char| !character.is_ascii_alphanumeric())
        .next()
        .unwrap_or_default();
    compact.contains("easyanticheat")
        || matches!(
            first_token,
            "eac"
                | "eaclauncher"
                | "beclient"
                | "belauncher"
                | "beservice"
                | "bedaisy"
                | "equ8"
                | "vgk"
                | "vgc"
        )
        || compact.contains("battleye")
}

fn loaded_anti_cheat_module_hits(pid: u32) -> Result<Vec<String>, String> {
    let snapshot = SnapshotHandle(
        unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPMODULE | TH32CS_SNAPMODULE32, pid) }
            .map_err(|error| format!("CreateToolhelp32Snapshot 失败: {error}"))?,
    );
    let mut entry = MODULEENTRY32W {
        dwSize: core::mem::size_of::<MODULEENTRY32W>() as u32,
        ..Default::default()
    };
    let mut hits = Vec::new();
    unsafe { Module32FirstW(snapshot.0, &mut entry) }
        .map_err(|error| format!("Module32FirstW protection scan failed: {error}"))?;
    loop {
        let name = String::from_utf16_lossy(&entry.szModule)
            .trim_end_matches('\0')
            .to_ascii_lowercase();
        if is_anti_cheat_module_name(&name) {
            hits.push(name);
        }
        match unsafe {
            windows::Win32::System::Diagnostics::ToolHelp::Module32NextW(snapshot.0, &mut entry)
        } {
            Ok(()) => {}
            // ERROR_NO_MORE_FILES (18) is the only successful enumeration terminator.
            Err(error) if error.code().0 as u32 == 0x8007_0012 => break,
            Err(error) => {
                return Err(format!("Module32NextW protection scan failed: {error}"));
            }
        }
    }
    hits.sort();
    hits.dedup();
    Ok(hits)
}

fn validate_no_loaded_anti_cheat_modules(pid: u32) -> Result<(), String> {
    let hits = loaded_anti_cheat_module_hits(pid)?;
    if !hits.is_empty() {
        return Err(format!(
            "目标进程加载了反作弊模块，拒绝注入: {}",
            hits.join(", ")
        ));
    }
    Ok(())
}

fn sha256_file(path: &Path, label: &str) -> Result<String, String> {
    let bytes = std::fs::read(path)
        .map_err(|error| format!("读取{label} {} 失败: {error}", path.display()))?;
    Ok(format!("sha256:{:x}", Sha256::digest(bytes)))
}

fn normalized_path(path: &Path) -> String {
    let rendered = path.to_string_lossy();
    rendered
        .strip_prefix(r"\\?\")
        .unwrap_or(&rendered)
        .replace('/', r"\")
        .to_ascii_lowercase()
}

fn process_image_path(process: HANDLE) -> Result<PathBuf, String> {
    let mut capacity = 512usize;
    loop {
        let mut buffer = vec![0u16; capacity];
        let written = unsafe { K32GetModuleFileNameExW(Some(process), None, &mut buffer) };
        if written == 0 {
            return Err("无法读取目标进程映像路径".into());
        }
        let written = written as usize;
        if written < buffer.len() {
            buffer.truncate(written);
            return Ok(PathBuf::from(String::from_utf16_lossy(&buffer)));
        }
        capacity = capacity
            .checked_mul(2)
            .filter(|next| *next <= 32_768)
            .ok_or("目标进程映像路径超过 32768 个 UTF-16 code unit")?;
    }
}

#[derive(Debug, Clone)]
struct ExpectedExecutable {
    path: PathBuf,
    sha256: String,
}

fn validate_expected_executable_args(
    path: Option<String>,
    sha256: Option<String>,
) -> Result<Option<ExpectedExecutable>, String> {
    match (path, sha256) {
        (None, None) => Ok(None),
        (Some(path), Some(sha256)) => {
            let path = PathBuf::from(path);
            if !path.is_absolute() {
                return Err("--expected-exe 必须是绝对路径".into());
            }
            let Some(hex) = sha256.strip_prefix("sha256:") else {
                return Err("--expected-exe-sha256 必须使用 sha256:<64 hex> 格式".into());
            };
            if hex.len() != 64 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err("--expected-exe-sha256 必须包含 64 个十六进制字符".into());
            }
            Ok(Some(ExpectedExecutable {
                path,
                sha256: format!("sha256:{}", hex.to_ascii_lowercase()),
            }))
        }
        _ => Err("--expected-exe 与 --expected-exe-sha256 必须成对提供".into()),
    }
}

fn validate_target_executable(
    process: HANDLE,
    expected: &ExpectedExecutable,
) -> Result<(), String> {
    let expected_path = std::fs::canonicalize(&expected.path)
        .map_err(|error| format!("无法解析 expected EXE: {error}"))?;
    if !expected_path.is_file() {
        return Err("expected EXE 不是普通文件".into());
    }
    let actual_path = process_image_path(process)?;
    let actual_path = std::fs::canonicalize(&actual_path)
        .map_err(|error| format!("无法解析目标进程映像: {error}"))?;
    if normalized_path(&actual_path) != normalized_path(&expected_path) {
        return Err(format!(
            "目标进程映像路径不匹配: expected={} actual={}",
            expected_path.display(),
            actual_path.display()
        ));
    }
    let actual_sha256 = sha256_file(&actual_path, "目标 EXE")?;
    if actual_sha256 != expected.sha256 {
        return Err(format!(
            "目标进程 EXE hash 不匹配: expected={} actual={actual_sha256}",
            expected.sha256
        ));
    }
    Ok(())
}

fn validate_expected_start_filetime(value: Option<String>) -> Result<Option<u64>, String> {
    let Some(value) = value else {
        return Ok(None);
    };
    let ticks = value
        .parse::<u64>()
        .map_err(|error| format!("--expected-start-time-filetime 不是 u64: {error}"))?;
    if ticks == 0 {
        return Err("--expected-start-time-filetime 必须大于 0".into());
    }
    Ok(Some(ticks))
}

fn process_start_filetime(process: HANDLE) -> Result<u64, String> {
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    unsafe { GetProcessTimes(process, &mut creation, &mut exit, &mut kernel, &mut user) }
        .map_err(|error| format!("GetProcessTimes 失败: {error}"))?;
    Ok((u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime))
}

fn validate_process_start_filetime(process: HANDLE, expected: u64) -> Result<(), String> {
    let actual = process_start_filetime(process)?;
    if actual != expected {
        return Err(format!(
            "目标进程启动时刻不匹配: expected_filetime={expected} actual_filetime={actual}"
        ));
    }
    Ok(())
}

fn is_probe_module(name: &str) -> bool {
    name.starts_with("dm_depth_probe") && name.ends_with(".dll")
}

fn validate_resident_reuse(
    allow_existing: bool,
    expected_sha256: &str,
    resident_sha256: &str,
) -> Result<(), String> {
    if !allow_existing {
        return Err("目标进程已有 depth probe；未授权复用".into());
    }
    if !resident_sha256.eq_ignore_ascii_case(expected_sha256) {
        return Err(format!(
            "目标进程驻留 probe hash 不匹配: expected={expected_sha256} resident={resident_sha256}"
        ));
    }
    Ok(())
}

fn inject(
    pid: u32,
    dll: &Path,
    allow_existing: bool,
    expected_sha256: &str,
    expected_executable: Option<&ExpectedExecutable>,
    expected_start_filetime: Option<u64>,
) -> Result<InjectionSuccess, String> {
    if !dll.is_absolute() {
        return Err(format!("DLL 路径必须是绝对路径：{}", dll.display()));
    }
    if !dll.exists() {
        return Err(format!("DLL 不存在：{}", dll.display()));
    }
    let dll_name = dll
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .ok_or("DLL 路径没有文件名")?;
    let requested_sha256 = sha256_file(dll, "DLL")?;
    if !requested_sha256.eq_ignore_ascii_case(expected_sha256) {
        return Err(format!(
            "待注入 DLL hash 与请求不一致: expected={expected_sha256} actual={requested_sha256}"
        ));
    }

    let process = unsafe {
        OpenProcess(
            PROCESS_CREATE_THREAD
                | PROCESS_QUERY_INFORMATION
                | PROCESS_VM_OPERATION
                | PROCESS_VM_WRITE
                | PROCESS_VM_READ,
            false,
            pid,
        )
    }
    .map_err(|e| format!("OpenProcess({pid}) 失败: {e}（游戏是否以管理员运行？）"))?;

    let result = (|| -> Result<InjectionSuccess, String> {
        if let Some(expected) = expected_start_filetime {
            validate_process_start_filetime(process, expected)?;
            validate_no_loaded_anti_cheat_modules(pid)?;
        }
        if let Some(expected) = expected_executable {
            validate_target_executable(process, expected)?;
        }
        // 位数不匹配时先说人话，而不是让 CreateRemoteThread 以一个难解释的错误码失败
        let target32 = target_is_32bit(process)?;
        if target32 != self_is_32bit() {
            return Err(format!(
                "位数不匹配：目标进程是 {} 位，本注入器与 DLL 是 {} 位。\
                 请用对应架构重新构建（BioShock Infinite 需要 i686-pc-windows-msvc）",
                if target32 { 32 } else { 64 },
                if self_is_32bit() { 32 } else { 64 }
            ));
        }

        if let Some(resident) = loaded_modules(pid, process)?
            .into_iter()
            .find(|module| is_probe_module(&module.name))
        {
            let resident_sha256 = sha256_file(&resident.path, "驻留 DLL")?;
            validate_resident_reuse(allow_existing, expected_sha256, &resident_sha256)?;
            if let Some(expected) = expected_executable {
                validate_target_executable(process, expected)?;
            }
            if let Some(expected) = expected_start_filetime {
                validate_process_start_filetime(process, expected)?;
            }
            return Ok(InjectionSuccess::ReusedResident(resident_sha256));
        }

        // 把 DLL 路径写进目标进程，再让它自己 LoadLibraryW
        let mut wide: Vec<u16> = dll.as_os_str().encode_wide().collect();
        wide.push(0);
        let bytes = wide.len() * 2;

        let remote = unsafe {
            VirtualAllocEx(
                process,
                None,
                bytes,
                MEM_COMMIT | MEM_RESERVE,
                PAGE_READWRITE,
            )
        };
        if remote.is_null() {
            return Err("VirtualAllocEx 失败（目标进程可能有内存保护）".into());
        }

        let write_result =
            unsafe { WriteProcessMemory(process, remote, wide.as_ptr().cast(), bytes, None) };
        let mut remote_thread_may_use_buffer = false;
        let outcome = (|| -> Result<(), String> {
            write_result.map_err(|e| format!("WriteProcessMemory 失败: {e}"))?;

            let kernel32 = unsafe { GetModuleHandleW(w!("kernel32.dll")) }
                .map_err(|e| format!("拿不到 kernel32: {e}"))?;
            let load_library =
                unsafe { GetProcAddress(kernel32, windows::core::s!("LoadLibraryW")) }
                    .ok_or("拿不到 LoadLibraryW 地址")?;

            let start: unsafe extern "system" fn(*mut core::ffi::c_void) -> u32 =
                unsafe { core::mem::transmute(load_library) };

            // The guarded generic D3D11 lane supplies the process creation FILETIME. Recheck both
            // that instance token and the target's loaded-module protection denylist immediately
            // before the irreversible CreateRemoteThread/LoadLibraryW boundary.
            if let Some(expected) = expected_start_filetime {
                validate_process_start_filetime(process, expected)?;
                validate_no_loaded_anti_cheat_modules(pid)?;
            }

            let thread =
                unsafe { CreateRemoteThread(process, None, 0, Some(start), Some(remote), 0, None) }
                    .map_err(|e| format!("CreateRemoteThread 失败: {e}"))?;
            remote_thread_may_use_buffer = true;

            // 有界等待 LoadLibraryW。目标进程或 DllMain 卡住时，客户端不能永久卡死。
            let wait = unsafe { WaitForSingleObject(thread, LOAD_LIBRARY_TIMEOUT_MS) };
            if wait == WAIT_OBJECT_0 {
                remote_thread_may_use_buffer = false;
            }
            unsafe {
                let _ = CloseHandle(thread);
            }
            if wait == WAIT_TIMEOUT {
                return Err(format!(
                    "等待 LoadLibraryW 超过 {} 秒；目标进程或 probe 初始化可能卡住",
                    LOAD_LIBRARY_TIMEOUT_MS / 1_000
                ));
            }
            if wait != WAIT_OBJECT_0 {
                return Err(format!("等待 LoadLibraryW 失败: {wait:?}"));
            }
            Ok(())
        })();

        if !remote_thread_may_use_buffer {
            unsafe {
                let _ = VirtualFreeEx(process, remote, 0, MEM_RELEASE);
            }
        }
        outcome?;

        // 复核：LoadLibraryW 可能返回 NULL（DLL 依赖缺失等），线程照样正常结束。
        // 不复核就会把「注入失败」报成「注入成功」。
        let loaded = loaded_modules(pid, process)?
            .into_iter()
            .find(|module| module.name.eq_ignore_ascii_case(&dll_name))
            .ok_or_else(|| {
                format!(
                    "远程线程已结束但 {dll_name} 未出现在进程 {pid} 的模块表里——\
                     LoadLibraryW 大概率失败（依赖缺失或位数不符）"
                )
            })?;
        let loaded_sha256 = sha256_file(&loaded.path, "已加载 DLL")?;
        if !loaded_sha256.eq_ignore_ascii_case(expected_sha256) {
            return Err(format!(
                "实际加载 DLL hash 不匹配: expected={expected_sha256} loaded={loaded_sha256} path={}",
                loaded.path.display()
            ));
        }
        if let Some(expected) = expected_executable {
            // PID reuse or an on-disk identity change between the runtime gate and LoadLibraryW
            // must not be reported as a successful exact-build injection. The probe repeats its
            // own executable hash check before DSV discovery activation.
            validate_target_executable(process, expected)?;
        }
        if let Some(expected) = expected_start_filetime {
            validate_process_start_filetime(process, expected)?;
            validate_no_loaded_anti_cheat_modules(pid)?;
        }
        Ok(InjectionSuccess::NewlyLoaded(loaded_sha256))
    })();

    unsafe {
        let _ = CloseHandle(process);
    }
    result
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.as_slice() == ["--print-guard-contract"] {
        println!("{GUARDED_INJECTION_CONTRACT}");
        return Ok(());
    }
    let pid: u32 = arg(&args, "--pid")
        .ok_or_else(|| format!("缺少 --pid\n{}", usage()))?
        .parse()
        .map_err(|e| format!("--pid 不是数字: {e}"))?;
    let dll = arg(&args, "--dll").ok_or_else(|| format!("缺少 --dll\n{}", usage()))?;
    let expected_sha256 = arg(&args, "--expected-sha256")
        .ok_or_else(|| format!("缺少 --expected-sha256\n{}", usage()))?;
    let expected_executable = validate_expected_executable_args(
        arg(&args, "--expected-exe"),
        arg(&args, "--expected-exe-sha256"),
    )?;
    let expected_start_filetime =
        validate_expected_start_filetime(arg(&args, "--expected-start-time-filetime"))?;
    let allow_existing = args.iter().any(|arg| arg == "--allow-existing");
    let success = inject(
        pid,
        Path::new(&dll),
        allow_existing,
        &expected_sha256,
        expected_executable.as_ref(),
        expected_start_filetime,
    )?;
    // This stdout line is a machine-authenticated boundary consumed by
    // Invoke-GenericD3D11DsvInjection.ps1. Keep stdout to exactly this one line: any
    // human-oriented success message makes the guarded wrapper reject the already-loaded DLL and
    // withhold its receipt.
    write_successful_injection_stdout(std::io::stdout().lock(), &success)
        .map_err(|error| format!("写入 guarded injection 回执失败: {error}"))?;
    Ok(())
}

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("错误: {e}");
            std::process::ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resident_probe_reuse_requires_explicit_permission_and_exact_hash() {
        let expected = format!("sha256:{}", "a".repeat(64));
        assert!(validate_resident_reuse(true, &expected, &expected).is_ok());
        assert!(validate_resident_reuse(false, &expected, &expected).is_err());
        assert!(
            validate_resident_reuse(true, &expected, &format!("sha256:{}", "b".repeat(64)))
                .is_err()
        );
    }

    #[test]
    fn only_depth_probe_named_modules_are_candidates_for_safe_reuse() {
        assert!(is_probe_module("dm_depth_probe.dll"));
        assert!(is_probe_module("dm_depth_probe-0123.dll"));
        assert!(!is_probe_module("unrelated.dll"));
        assert!(!is_probe_module("dm_depth_probe.exe"));
    }

    #[test]
    fn exact_executable_args_are_optional_but_never_partial() {
        assert!(
            validate_expected_executable_args(None, None)
                .unwrap()
                .is_none()
        );
        assert!(
            validate_expected_executable_args(Some(r"C:\Games\AC3\ACIII.exe".into()), None)
                .is_err()
        );
        assert!(
            validate_expected_executable_args(None, Some(format!("sha256:{}", "a".repeat(64))))
                .is_err()
        );
        let identity = validate_expected_executable_args(
            Some(r"C:\Games\AC3\ACIII.exe".into()),
            Some(format!("sha256:{}", "A".repeat(64))),
        )
        .unwrap()
        .unwrap();
        assert_eq!(identity.sha256, format!("sha256:{}", "a".repeat(64)));
    }

    #[test]
    fn expected_process_start_filetime_is_optional_but_must_be_positive_u64() {
        assert_eq!(validate_expected_start_filetime(None).unwrap(), None);
        assert_eq!(
            validate_expected_start_filetime(Some("134174438400000000".into())).unwrap(),
            Some(134_174_438_400_000_000)
        );
        assert!(validate_expected_start_filetime(Some("0".into())).is_err());
        assert!(validate_expected_start_filetime(Some("not-a-number".into())).is_err());
    }

    #[test]
    fn guarded_injection_module_denylist_is_narrow_and_case_insensitive() {
        assert!(is_anti_cheat_module_name("EasyAntiCheat_EOS.dll"));
        assert!(is_anti_cheat_module_name("BEDaisy.sys"));
        assert!(is_anti_cheat_module_name("BEClient_x64.dll"));
        assert!(is_anti_cheat_module_name("BELauncher.exe"));
        assert!(is_anti_cheat_module_name("BEService_x64.dll"));
        assert!(is_anti_cheat_module_name("BattlEye.dll"));
        assert!(is_anti_cheat_module_name("equ8_client.dll"));
        assert!(is_anti_cheat_module_name("EAC_x64.dll"));
        assert!(is_anti_cheat_module_name("vgk_x64.dll"));
        assert!(!is_anti_cheat_module_name("d3d11.dll"));
        assert!(!is_anti_cheat_module_name("dm_depth_probe.dll"));
    }

    #[test]
    fn guarded_injection_contract_names_both_enforced_boundaries() {
        assert!(GUARDED_INJECTION_CONTRACT.contains("start-filetime"));
        assert!(GUARDED_INJECTION_CONTRACT.contains("loaded-anticheat-module-recheck"));
        assert!(GUARDED_INJECTION_CONTRACT.contains("single-canonical-stdout"));
    }

    #[test]
    fn every_success_path_writes_exactly_one_canonical_cli_line() {
        let hash = format!("sha256:{}", "a".repeat(64));
        for success in [
            InjectionSuccess::NewlyLoaded(hash.clone()),
            InjectionSuccess::ReusedResident(hash.clone()),
        ] {
            let mut stdout = Vec::new();
            write_successful_injection_stdout(&mut stdout, &success).unwrap();
            assert_eq!(stdout, format!("loaded_probe_sha256={hash}\n").as_bytes());
            assert!(stdout.is_ascii());
            assert_eq!(stdout.iter().filter(|byte| **byte == b'\n').count(), 1);
        }
    }
}

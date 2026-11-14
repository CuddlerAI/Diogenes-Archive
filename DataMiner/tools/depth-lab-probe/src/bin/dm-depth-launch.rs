//! Fail-closed early launcher for games whose graphics resources must be observed from process
//! startup. The child is created suspended, the exact probe is loaded and verified, and only
//! then is the primary thread resumed.
#![cfg(windows)]

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::Read;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, ERROR_BAD_LENGTH, GetLastError, HANDLE, WAIT_OBJECT_0,
    WAIT_TIMEOUT,
};
use windows::Win32::System::Diagnostics::Debug::WriteProcessMemory;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, MODULEENTRY32W, Module32FirstW, Module32NextW, PROCESSENTRY32W,
    Process32FirstW, Process32NextW, TH32CS_SNAPMODULE, TH32CS_SNAPMODULE32, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::System::Memory::{
    MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAllocEx, VirtualFreeEx,
};
use windows::Win32::System::ProcessStatus::K32GetModuleFileNameExW;
use windows::Win32::System::Threading::{
    CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateEventW, CreateProcessW, CreateRemoteThread,
    GetExitCodeThread, OpenProcess, PROCESS_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION,
    QueryFullProcessImageNameW, ResumeThread, STARTUPINFOW, TerminateProcess, WaitForSingleObject,
};
use windows::core::{HRESULT, PCWSTR, PWSTR, w};

const LOAD_LIBRARY_TIMEOUT_MS: u32 = 30_000;
// Exact CP2077 discovery performs a pinned executable/catalog/loaded-image ABI
// audit while the main thread is suspended. A cold 132 MB catalog plus
// antivirus inspection can exceed the former 30 s budget even though the
// normal local audit takes only a few seconds. Keep the child fail-closed, but
// allow a bounded two-minute startup window before terminating it.
const EARLY_READY_TIMEOUT_MS: u32 = 120_000;
const TERMINATE_TIMEOUT_MS: u32 = 10_000;
const ABORT_EXIT_CODE: u32 = 0xD735;
const MODULE_SNAPSHOT_ATTEMPTS: usize = 32;

fn usage() -> &'static str {
    "usage: dm-depth-launch --exe <absolute-exe> --expected-exe-sha256 <sha256:...> \\
     --dll <absolute-dll> --expected-dll-sha256 <sha256:...> \\
     [--working-directory <absolute-directory>] \\
     [--discovery-config <absolute-json> | --raw-depth-demo-config <absolute-json>]"
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LaunchConfig {
    exe: PathBuf,
    expected_exe_sha256: String,
    dll: PathBuf,
    expected_dll_sha256: String,
    working_directory: PathBuf,
    discovery_config: Option<PathBuf>,
    raw_depth_demo_config: Option<PathBuf>,
}

fn validated_sha256(value: &str, flag: &str) -> Result<String, String> {
    let Some(hex) = value.strip_prefix("sha256:") else {
        return Err(format!("{flag} must use the sha256:<64 hex digits> form"));
    };
    if hex.len() != 64 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(format!("{flag} must contain exactly 64 hexadecimal digits"));
    }
    Ok(format!("sha256:{}", hex.to_ascii_lowercase()))
}

fn parse_args(args: &[String]) -> Result<LaunchConfig, String> {
    let mut exe = None;
    let mut expected_exe_sha256 = None;
    let mut dll = None;
    let mut expected_dll_sha256 = None;
    let mut working_directory = None;
    let mut discovery_config = None;
    let mut raw_depth_demo_config = None;

    let mut index = 0;
    while index < args.len() {
        let flag = args[index].as_str();
        let slot = match flag {
            "--exe" => &mut exe,
            "--expected-exe-sha256" => &mut expected_exe_sha256,
            "--dll" => &mut dll,
            "--expected-dll-sha256" => &mut expected_dll_sha256,
            "--working-directory" => &mut working_directory,
            "--discovery-config" => &mut discovery_config,
            "--raw-depth-demo-config" => &mut raw_depth_demo_config,
            _ => return Err(format!("unknown argument {flag}\n{}", usage())),
        };
        if slot.is_some() {
            return Err(format!("duplicate argument {flag}"));
        }
        let value = args
            .get(index + 1)
            .filter(|value| !value.starts_with("--"))
            .ok_or_else(|| format!("missing value for {flag}"))?;
        *slot = Some(value.clone());
        index += 2;
    }

    let exe = PathBuf::from(exe.ok_or_else(|| format!("missing --exe\n{}", usage()))?);
    let dll = PathBuf::from(dll.ok_or_else(|| format!("missing --dll\n{}", usage()))?);
    validate_absolute_extension(&exe, "--exe", "exe")?;
    validate_absolute_extension(&dll, "--dll", "dll")?;

    let working_directory = working_directory
        .map(PathBuf::from)
        .unwrap_or_else(|| exe.parent().unwrap_or(Path::new("")).to_path_buf());
    if !working_directory.is_absolute() {
        return Err("--working-directory must be an absolute path".into());
    }
    let discovery_config = discovery_config.map(PathBuf::from);
    if let Some(path) = discovery_config.as_deref() {
        validate_absolute_extension(path, "--discovery-config", "json")?;
    }
    let raw_depth_demo_config = raw_depth_demo_config.map(PathBuf::from);
    if let Some(path) = raw_depth_demo_config.as_deref() {
        validate_absolute_extension(path, "--raw-depth-demo-config", "json")?;
    }
    if discovery_config.is_some() && raw_depth_demo_config.is_some() {
        return Err("discovery and raw-depth-demo modes are mutually exclusive".into());
    }

    Ok(LaunchConfig {
        exe,
        expected_exe_sha256: validated_sha256(
            &expected_exe_sha256
                .ok_or_else(|| format!("missing --expected-exe-sha256\n{}", usage()))?,
            "--expected-exe-sha256",
        )?,
        dll,
        expected_dll_sha256: validated_sha256(
            &expected_dll_sha256
                .ok_or_else(|| format!("missing --expected-dll-sha256\n{}", usage()))?,
            "--expected-dll-sha256",
        )?,
        working_directory,
        discovery_config,
        raw_depth_demo_config,
    })
}

fn valid_launch_id(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn new_launch_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// Builds a private Unicode environment for the child. The launch identity and
/// optional discovery config are not written into the command line, and any
/// inherited variables with those names are replaced rather than duplicated.
fn child_environment_block(
    launch_id: &str,
    discovery_config: Option<&Path>,
    raw_depth_demo_config: Option<&Path>,
    expected_probe_sha256: &str,
) -> Result<Vec<u16>, String> {
    if !valid_launch_id(launch_id) {
        return Err("launch id must contain exactly 32 lowercase hexadecimal digits".into());
    }
    let mut entries: Vec<OsString> = std::env::vars_os()
        .filter(|(key, _)| {
            !key.to_string_lossy()
                .eq_ignore_ascii_case("DM_DEPTH_LAUNCH_ID")
                && !key
                    .to_string_lossy()
                    .eq_ignore_ascii_case("DM_DEPTH_DISCOVERY_CONFIG")
                && !key
                    .to_string_lossy()
                    .eq_ignore_ascii_case("DM_DEPTH_RAW_DEMO_CONFIG")
                && !key
                    .to_string_lossy()
                    .eq_ignore_ascii_case("DM_DEPTH_EXPECTED_PROBE_SHA256")
        })
        .map(|(key, value)| {
            let mut entry = key;
            entry.push("=");
            entry.push(value);
            entry
        })
        .collect();
    entries.push(OsString::from(format!("DM_DEPTH_LAUNCH_ID={launch_id}")));
    entries.push(OsString::from(format!(
        "DM_DEPTH_EXPECTED_PROBE_SHA256={expected_probe_sha256}"
    )));
    if let Some(path) = discovery_config {
        if !path.is_absolute() {
            return Err("discovery config environment path must be absolute".into());
        }
        let mut entry = OsString::from("DM_DEPTH_DISCOVERY_CONFIG=");
        entry.push(path.as_os_str());
        entries.push(entry);
    }
    if let Some(path) = raw_depth_demo_config {
        if !path.is_absolute() {
            return Err("raw depth demo config environment path must be absolute".into());
        }
        let mut entry = OsString::from("DM_DEPTH_RAW_DEMO_CONFIG=");
        entry.push(path.as_os_str());
        entries.push(entry);
    }
    entries.sort_by_cached_key(|entry| entry.to_string_lossy().to_ascii_lowercase());
    let mut block = Vec::new();
    for entry in entries {
        block.extend(entry.encode_wide());
        block.push(0);
    }
    block.push(0);
    Ok(block)
}

fn validate_absolute_extension(path: &Path, flag: &str, extension: &str) -> Result<(), String> {
    if !path.is_absolute() {
        return Err(format!("{flag} must be an absolute path"));
    }
    if !path
        .extension()
        .is_some_and(|actual| actual.eq_ignore_ascii_case(extension))
    {
        return Err(format!("{flag} must name a .{extension} file"));
    }
    Ok(())
}

fn canonical_existing_file(path: &Path, label: &str) -> Result<PathBuf, String> {
    let canonical =
        std::fs::canonicalize(path).map_err(|error| format!("cannot resolve {label}: {error}"))?;
    if !canonical
        .metadata()
        .map_err(|error| format!("cannot inspect {label}: {error}"))?
        .is_file()
    {
        return Err(format!("{label} is not a regular file"));
    }
    Ok(canonical)
}

fn canonical_existing_directory(path: &Path) -> Result<PathBuf, String> {
    let canonical = std::fs::canonicalize(path)
        .map_err(|error| format!("cannot resolve working directory: {error}"))?;
    if !canonical
        .metadata()
        .map_err(|error| format!("cannot inspect working directory: {error}"))?
        .is_dir()
    {
        return Err("working directory is not a directory".into());
    }
    Ok(canonical)
}

fn sha256_file(path: &Path, label: &str) -> Result<String, String> {
    let mut file = File::open(path).map_err(|error| format!("cannot open {label}: {error}"))?;
    let mut hasher = Sha256::new();
    // Windows GUI entry points commonly receive a 1 MiB main-thread stack.
    // Keeping a 1 MiB hash buffer inline can overflow before the child is ever
    // created, so the bounded buffer must live on the heap.
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("cannot hash {label}: {error}"))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("sha256:{:x}", hasher.finalize()))
}

fn require_hash(path: &Path, expected: &str, label: &str) -> Result<(), String> {
    let actual = sha256_file(path, label)?;
    if actual != expected {
        return Err(format!(
            "{label} hash mismatch: expected={expected} actual={actual}"
        ));
    }
    Ok(())
}

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

fn wide_null(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(std::iter::once(0)).collect()
}

fn early_ready_event_name(pid: u32) -> String {
    format!(r"Local\dm-depth-early-ready-{pid}")
}

fn create_early_ready_event(pid: u32) -> Result<OwnedHandle, String> {
    let name = early_ready_event_name(pid);
    let name_wide = wide_null(OsStr::new(&name));
    let event = unsafe { CreateEventW(None, true, false, PCWSTR(name_wide.as_ptr())) }
        .map_err(|error| format!("cannot create early-ready event: {error}"))?;
    let event = OwnedHandle(event);
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        return Err(
            "early-ready event unexpectedly already exists; refusing spoofable launch".into(),
        );
    }
    Ok(event)
}

fn wait_for_early_ready(event: HANDLE) -> Result<(), String> {
    let wait = unsafe { WaitForSingleObject(event, EARLY_READY_TIMEOUT_MS) };
    if wait == WAIT_OBJECT_0 {
        return Ok(());
    }
    if wait == WAIT_TIMEOUT {
        return Err(
            "probe early-ready handshake timed out; suspended child will be terminated".into(),
        );
    }
    Err(format!(
        "waiting for probe early-ready handshake failed: {wait:?}"
    ))
}

fn utf16_array(value: &[u16]) -> String {
    String::from_utf16_lossy(value)
        .trim_end_matches('\0')
        .to_owned()
}

fn process_image_path(process: HANDLE) -> Result<PathBuf, String> {
    let mut buffer = vec![0u16; 32_768];
    let mut size = buffer.len() as u32;
    unsafe {
        QueryFullProcessImageNameW(
            process,
            Default::default(),
            PWSTR(buffer.as_mut_ptr()),
            &mut size,
        )
    }
    .map_err(|error| format!("cannot query process image: {error}"))?;
    buffer.truncate(size as usize);
    Ok(PathBuf::from(String::from_utf16_lossy(&buffer)))
}

fn normalized_path(path: &Path) -> String {
    path.to_string_lossy()
        .strip_prefix(r"\\?\")
        .unwrap_or(&path.to_string_lossy())
        .replace('/', r"\")
        .to_ascii_lowercase()
}

fn reject_existing_process(exe: &Path, except_pid: Option<u32>) -> Result<(), String> {
    let wanted_name = exe
        .file_name()
        .ok_or("executable has no file name")?
        .to_string_lossy()
        .to_ascii_lowercase();
    let wanted_path = normalized_path(exe);
    let snapshot = OwnedHandle(
        unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }
            .map_err(|error| format!("cannot enumerate processes: {error}"))?,
    );
    let mut entry = PROCESSENTRY32W {
        dwSize: core::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    if unsafe { Process32FirstW(snapshot.0, &mut entry) }.is_err() {
        return Ok(());
    }
    loop {
        let pid = entry.th32ProcessID;
        let name = utf16_array(&entry.szExeFile).to_ascii_lowercase();
        if Some(pid) != except_pid && name == wanted_name {
            let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }
                .map_err(
                    |_| "a same-named process exists but its exact image cannot be verified",
                )?;
            let process = OwnedHandle(process);
            let actual = process_image_path(process.0)?;
            let actual = std::fs::canonicalize(&actual).unwrap_or(actual);
            if normalized_path(&actual) == wanted_path {
                return Err(
                    "the exact executable is already running; refusing to attach or replace it"
                        .into(),
                );
            }
        }
        if unsafe { Process32NextW(snapshot.0, &mut entry) }.is_err() {
            break;
        }
    }
    Ok(())
}

struct SuspendedChild {
    process: OwnedHandle,
    primary_thread: OwnedHandle,
    pid: u32,
    resumed: bool,
    cleanup_complete: bool,
}

impl SuspendedChild {
    fn create(
        exe: &Path,
        working_directory: &Path,
        launch_id: &str,
        discovery_config: Option<&Path>,
        raw_depth_demo_config: Option<&Path>,
        expected_probe_sha256: &str,
    ) -> Result<Self, String> {
        let exe_wide = wide_null(exe.as_os_str());
        let cwd_wide = wide_null(working_directory.as_os_str());
        let environment = child_environment_block(
            launch_id,
            discovery_config,
            raw_depth_demo_config,
            expected_probe_sha256,
        )?;
        let startup = STARTUPINFOW {
            cb: core::mem::size_of::<STARTUPINFOW>() as u32,
            ..Default::default()
        };
        let mut process_info = PROCESS_INFORMATION::default();

        // lpApplicationName carries the exact executable. There is deliberately no shell and no
        // mutable command-line string to parse or concatenate.
        unsafe {
            CreateProcessW(
                PCWSTR(exe_wide.as_ptr()),
                None,
                None,
                None,
                false,
                CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT,
                Some(environment.as_ptr().cast()),
                PCWSTR(cwd_wide.as_ptr()),
                &startup,
                &mut process_info,
            )
        }
        .map_err(|error| format!("CreateProcessW(CREATE_SUSPENDED) failed: {error}"))?;

        Ok(Self {
            process: OwnedHandle(process_info.hProcess),
            primary_thread: OwnedHandle(process_info.hThread),
            pid: process_info.dwProcessId,
            resumed: false,
            cleanup_complete: false,
        })
    }

    fn terminate_and_wait(&mut self) -> Result<(), String> {
        if self.resumed || self.cleanup_complete {
            return Ok(());
        }
        unsafe { TerminateProcess(self.process.0, ABORT_EXIT_CODE) }
            .map_err(|error| format!("failed to terminate suspended child: {error}"))?;
        let wait = unsafe { WaitForSingleObject(self.process.0, TERMINATE_TIMEOUT_MS) };
        if wait != WAIT_OBJECT_0 {
            return Err(format!(
                "terminated child did not exit within timeout: {wait:?}"
            ));
        }
        self.cleanup_complete = true;
        Ok(())
    }

    fn resume(&mut self) -> Result<(), String> {
        let previous = unsafe { ResumeThread(self.primary_thread.0) };
        if previous == u32::MAX {
            return Err("ResumeThread failed".into());
        }
        if previous != 1 {
            return Err(format!(
                "unexpected primary-thread suspend count {previous}; child will be terminated"
            ));
        }
        self.resumed = true;
        Ok(())
    }
}

impl Drop for SuspendedChild {
    fn drop(&mut self) {
        if !self.resumed && !self.cleanup_complete {
            let _ = self.terminate_and_wait();
        }
    }
}

#[derive(Debug)]
struct LoadedModule {
    name: String,
    path: PathBuf,
}

fn module_path(
    process: HANDLE,
    module: windows::Win32::Foundation::HMODULE,
) -> Result<PathBuf, String> {
    let mut buffer = vec![0u16; 32_768];
    let written = unsafe { K32GetModuleFileNameExW(Some(process), Some(module), &mut buffer) };
    if written == 0 {
        return Err("cannot obtain resident module path".into());
    }
    buffer.truncate(written as usize);
    Ok(PathBuf::from(String::from_utf16_lossy(&buffer)))
}

fn loaded_modules(pid: u32, process: HANDLE) -> Result<Vec<LoadedModule>, String> {
    // Toolhelp explicitly documents ERROR_BAD_LENGTH as a transient module-list
    // race and requires callers to retry. LoadLibraryW has just changed that
    // list, so a one-shot snapshot makes valid early launches flaky. Keep the
    // retry bounded and preserve every resident path/hash check below.
    let mut snapshot = None;
    let mut last_error = None;
    for attempt in 0..MODULE_SNAPSHOT_ATTEMPTS {
        match unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPMODULE | TH32CS_SNAPMODULE32, pid) } {
            Ok(handle) => {
                snapshot = Some(OwnedHandle(handle));
                break;
            }
            Err(error) if error.code() == HRESULT::from_win32(ERROR_BAD_LENGTH.0) => {
                last_error = Some(error);
                if attempt + 1 < MODULE_SNAPSHOT_ATTEMPTS {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            }
            Err(error) => return Err(format!("cannot enumerate child modules: {error}")),
        }
    }
    let snapshot = snapshot.ok_or_else(|| {
        format!(
            "cannot enumerate child modules after {MODULE_SNAPSHOT_ATTEMPTS} bounded retries: {}",
            last_error
                .map(|error| error.to_string())
                .unwrap_or_else(|| "unknown Toolhelp error".into())
        )
    })?;
    let mut entry = MODULEENTRY32W {
        dwSize: core::mem::size_of::<MODULEENTRY32W>() as u32,
        ..Default::default()
    };
    let mut modules = Vec::new();
    if unsafe { Module32FirstW(snapshot.0, &mut entry) }.is_ok() {
        loop {
            let name = utf16_array(&entry.szModule);
            // K32GetModuleFileNameExW can transiently fail for unrelated
            // modules while the loader list is changing.  The resident probe
            // itself is still mandatory below; skip only entries whose path
            // cannot be queried and let the exact-name/path/hash lookup fail
            // closed if that skipped entry was the requested DLL.
            if let Ok(path) = module_path(process, entry.hModule) {
                modules.push(LoadedModule { name, path });
            }
            if unsafe { Module32NextW(snapshot.0, &mut entry) }.is_err() {
                break;
            }
        }
    }
    Ok(modules)
}

fn inject_and_verify(child: &SuspendedChild, dll: &Path, expected: &str) -> Result<String, String> {
    let dll_name = dll
        .file_name()
        .ok_or("DLL has no file name")?
        .to_string_lossy()
        .into_owned();
    let dll_wide = wide_null(dll.as_os_str());
    let byte_len = dll_wide.len() * core::mem::size_of::<u16>();
    let remote = unsafe {
        VirtualAllocEx(
            child.process.0,
            None,
            byte_len,
            MEM_COMMIT | MEM_RESERVE,
            PAGE_READWRITE,
        )
    };
    if remote.is_null() {
        return Err("VirtualAllocEx failed".into());
    }

    let mut remote_thread_completed = false;
    let outcome = (|| -> Result<String, String> {
        unsafe {
            WriteProcessMemory(
                child.process.0,
                remote,
                dll_wide.as_ptr().cast(),
                byte_len,
                None,
            )
        }
        .map_err(|error| format!("WriteProcessMemory failed: {error}"))?;

        let kernel32 = unsafe { GetModuleHandleW(w!("kernel32.dll")) }
            .map_err(|error| format!("cannot resolve kernel32: {error}"))?;
        let load_library = unsafe { GetProcAddress(kernel32, windows::core::s!("LoadLibraryW")) }
            .ok_or("cannot resolve LoadLibraryW")?;
        let start: unsafe extern "system" fn(*mut core::ffi::c_void) -> u32 =
            unsafe { core::mem::transmute(load_library) };
        let remote_thread = unsafe {
            CreateRemoteThread(child.process.0, None, 0, Some(start), Some(remote), 0, None)
        }
        .map_err(|error| format!("CreateRemoteThread failed: {error}"))?;
        let remote_thread = OwnedHandle(remote_thread);
        let wait = unsafe { WaitForSingleObject(remote_thread.0, LOAD_LIBRARY_TIMEOUT_MS) };
        if wait == WAIT_TIMEOUT {
            return Err("LoadLibraryW timed out; suspended child will be terminated".into());
        }
        if wait != WAIT_OBJECT_0 {
            return Err(format!("waiting for LoadLibraryW failed: {wait:?}"));
        }
        remote_thread_completed = true;
        let mut exit_code = 0u32;
        unsafe { GetExitCodeThread(remote_thread.0, &mut exit_code) }
            .map_err(|error| format!("cannot read LoadLibraryW result: {error}"))?;
        if exit_code == 0 {
            return Err("LoadLibraryW returned NULL".into());
        }

        let resident = loaded_modules(child.pid, child.process.0)?
            .into_iter()
            .find(|module| module.name.eq_ignore_ascii_case(&dll_name))
            .ok_or("LoadLibraryW completed but the requested DLL is not resident")?;
        let resident_path = std::fs::canonicalize(&resident.path)
            .map_err(|error| format!("cannot resolve resident DLL: {error}"))?;
        if normalized_path(&resident_path) != normalized_path(dll) {
            return Err("resident DLL path differs from the requested exact DLL".into());
        }
        let resident_sha256 = sha256_file(&resident_path, "resident DLL")?;
        if resident_sha256 != expected {
            return Err(format!(
                "resident DLL hash mismatch: expected={expected} actual={resident_sha256}"
            ));
        }
        Ok(resident_sha256)
    })();

    // The remote thread no longer uses the path buffer after WAIT_OBJECT_0. On timeout the child
    // guard terminates the whole process, which also releases this allocation.
    if remote_thread_completed {
        unsafe {
            let _ = VirtualFreeEx(child.process.0, remote, 0, MEM_RELEASE);
        }
    }
    outcome
}

fn launch(config: LaunchConfig) -> Result<(u32, String, String), String> {
    let exe = canonical_existing_file(&config.exe, "executable")?;
    let dll = canonical_existing_file(&config.dll, "probe DLL")?;
    let working_directory = canonical_existing_directory(&config.working_directory)?;
    let discovery_config = config
        .discovery_config
        .as_deref()
        .map(|path| canonical_existing_file(path, "discovery config"))
        .transpose()?;
    let raw_depth_demo_config = config
        .raw_depth_demo_config
        .as_deref()
        .map(|path| canonical_existing_file(path, "raw depth demo config"))
        .transpose()?;
    require_hash(&exe, &config.expected_exe_sha256, "executable")?;
    require_hash(&dll, &config.expected_dll_sha256, "probe DLL")?;
    reject_existing_process(&exe, None)?;

    let launch_id = new_launch_id();
    let mut child = SuspendedChild::create(
        &exe,
        &working_directory,
        &launch_id,
        discovery_config.as_deref(),
        raw_depth_demo_config.as_deref(),
        &config.expected_dll_sha256,
    )?;
    let result = (|| -> Result<(u32, String, String), String> {
        let created_image = process_image_path(child.process.0)?;
        let created_image = std::fs::canonicalize(&created_image)
            .map_err(|error| format!("cannot resolve created process image: {error}"))?;
        if normalized_path(&created_image) != normalized_path(&exe) {
            return Err("created process image differs from the requested exact executable".into());
        }
        require_hash(
            &created_image,
            &config.expected_exe_sha256,
            "created executable",
        )?;
        reject_existing_process(&exe, Some(child.pid))?;

        // The event is created before injection, so a fast probe cannot signal between
        // LoadLibraryW completion and the launcher's OpenEvent call. The injected probe opens this
        // exact name, completes its early hooks, then SetEvent. LoadLibraryW alone is not readiness.
        let early_ready = create_early_ready_event(child.pid)?;
        let resident_sha256 = inject_and_verify(&child, &dll, &config.expected_dll_sha256)?;
        wait_for_early_ready(early_ready.0)?;
        child.resume()?;
        Ok((child.pid, resident_sha256, launch_id))
    })();

    match result {
        Ok(value) => Ok(value),
        Err(error) => {
            if !child.resumed
                && let Err(abort_error) = child.terminate_and_wait()
            {
                return Err(format!(
                    "{error}; additionally, fail-closed child cleanup failed: {abort_error}"
                ));
            }
            Err(error)
        }
    }
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let config = parse_args(&args)?;
    let (pid, resident_sha256, launch_id) = launch(config)?;
    println!("launched_pid={pid}");
    println!("resident_probe_sha256={resident_sha256}");
    println!("launch_id={launch_id}");
    Ok(())
}

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            std::process::ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(digit: char) -> String {
        format!("sha256:{}", digit.to_string().repeat(64))
    }

    fn required_args() -> Vec<String> {
        vec![
            "--exe".into(),
            r"C:\Games\CP2077\bin\x64\Cyberpunk2077.exe".into(),
            "--expected-exe-sha256".into(),
            hash('a'),
            "--dll".into(),
            r"C:\Probe\dm_depth_probe.dll".into(),
            "--expected-dll-sha256".into(),
            hash('b'),
        ]
    }

    #[test]
    fn accepts_exact_absolute_paths_and_defaults_working_directory() {
        let config = parse_args(&required_args()).unwrap();
        assert_eq!(
            config.working_directory,
            PathBuf::from(r"C:\Games\CP2077\bin\x64")
        );
        assert_eq!(config.expected_exe_sha256, hash('a'));
    }

    #[test]
    fn rejects_relative_paths_unknown_arguments_and_duplicates() {
        let mut relative = required_args();
        relative[1] = "Cyberpunk2077.exe".into();
        assert!(parse_args(&relative).unwrap_err().contains("absolute"));

        let mut unknown = required_args();
        unknown.extend(["--args".into(), "-modded".into()]);
        assert!(parse_args(&unknown).unwrap_err().contains("unknown"));

        let mut duplicate = required_args();
        duplicate.extend(["--dll".into(), r"C:\Other\probe.dll".into()]);
        assert!(parse_args(&duplicate).unwrap_err().contains("duplicate"));
    }

    #[test]
    fn validates_hash_shape_and_canonicalizes_hex_case() {
        assert_eq!(validated_sha256(&hash('A'), "hash").unwrap(), hash('a'));
        assert!(validated_sha256("abc", "hash").is_err());
        assert!(validated_sha256("sha256:abcd", "hash").is_err());
        assert!(validated_sha256(&format!("sha256:{}", "z".repeat(64)), "hash").is_err());
    }

    #[test]
    fn normalized_windows_paths_compare_without_account_path_persistence() {
        assert_eq!(
            normalized_path(Path::new(r"\\?\C:\Games\CP2077\Cyberpunk2077.exe")),
            normalized_path(Path::new(r"c:/games/cp2077/Cyberpunk2077.exe"))
        );
    }

    #[test]
    fn early_ready_event_name_is_stable_and_pid_scoped() {
        assert_eq!(
            early_ready_event_name(735),
            r"Local\dm-depth-early-ready-735"
        );
        assert_ne!(early_ready_event_name(735), early_ready_event_name(736));
    }

    #[test]
    fn discovery_config_is_structured_and_requires_an_absolute_json_path() {
        let mut args = required_args();
        args.extend([
            "--discovery-config".into(),
            r"C:\ProgramData\DataMiner\discovery\config.json".into(),
        ]);
        assert_eq!(
            parse_args(&args).unwrap().discovery_config,
            Some(PathBuf::from(
                r"C:\ProgramData\DataMiner\discovery\config.json"
            ))
        );
        let mut relative = required_args();
        relative.extend(["--discovery-config".into(), "config.json".into()]);
        assert!(parse_args(&relative).is_err());
    }

    #[test]
    fn launch_identity_is_random_128_bit_lowercase_hex() {
        let first = new_launch_id();
        let second = new_launch_id();
        assert!(valid_launch_id(&first));
        assert!(valid_launch_id(&second));
        assert_ne!(first, second);
        assert!(!valid_launch_id("735"));
        assert!(!valid_launch_id(&"A".repeat(32)));
    }

    #[test]
    fn child_environment_carries_identity_and_discovery_config_without_stdout_paths() {
        let launch_id = "d".repeat(32);
        let config = Path::new(r"C:\ProgramData\DataMiner\discovery\config.json");
        let block = child_environment_block(&launch_id, Some(config), None, &hash('b')).unwrap();
        let rendered = String::from_utf16_lossy(&block).replace('\0', "\n");
        assert!(rendered.contains(&format!("DM_DEPTH_LAUNCH_ID={launch_id}")));
        assert!(rendered.contains(&format!("DM_DEPTH_DISCOVERY_CONFIG={}", config.display())));
        assert!(rendered.contains(&format!("DM_DEPTH_EXPECTED_PROBE_SHA256={}", hash('b'))));
        assert!(!rendered.contains("launch_id="));
    }

    #[test]
    fn raw_depth_demo_is_an_explicit_mode_separate_from_discovery() {
        let mut args = required_args();
        args.extend([
            "--raw-depth-demo-config".into(),
            r"C:\ProgramData\DataMiner\raw-demo\config.json".into(),
        ]);
        assert!(parse_args(&args).unwrap().raw_depth_demo_config.is_some());

        args.extend([
            "--discovery-config".into(),
            r"C:\ProgramData\DataMiner\discovery\config.json".into(),
        ]);
        assert!(
            parse_args(&args)
                .unwrap_err()
                .contains("mutually exclusive")
        );
    }
}

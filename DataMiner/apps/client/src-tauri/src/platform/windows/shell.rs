use std::path::Path;

use anyhow::ensure;
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::HSTRING;

pub fn open_directory(path: &Path) -> anyhow::Result<()> {
    ensure!(path.is_dir(), "local session directory is unavailable");
    let operation = HSTRING::from("open");
    let path = HSTRING::from(path.as_os_str());
    let result = unsafe { ShellExecuteW(None, &operation, &path, None, None, SW_SHOWNORMAL) };
    let code = result.0 as usize;
    ensure!(
        code > 32,
        "Windows shell rejected the local session directory"
    );
    Ok(())
}

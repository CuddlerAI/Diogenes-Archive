use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use anyhow::{Context, ensure};
use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
use windows::core::PCWSTR;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VolumeSpace {
    pub available_bytes: u64,
    pub total_bytes: u64,
    pub total_free_bytes: u64,
}

pub fn volume_space(path: &Path) -> anyhow::Result<VolumeSpace> {
    ensure!(path.exists(), "disk-space probe path does not exist");
    let wide = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let mut available_bytes = 0_u64;
    let mut total_bytes = 0_u64;
    let mut total_free_bytes = 0_u64;
    unsafe {
        GetDiskFreeSpaceExW(
            PCWSTR(wide.as_ptr()),
            Some(&mut available_bytes),
            Some(&mut total_bytes),
            Some(&mut total_free_bytes),
        )
    }
    .context("GetDiskFreeSpaceExW failed for the app-local capture volume")?;
    ensure!(total_bytes > 0, "capture volume reported zero total bytes");
    Ok(VolumeSpace {
        available_bytes,
        total_bytes,
        total_free_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queries_the_volume_containing_an_existing_directory() {
        let directory = tempfile::tempdir().unwrap();
        let space = volume_space(directory.path()).unwrap();

        assert!(space.total_bytes > 0);
        assert!(space.available_bytes <= space.total_free_bytes);
        assert!(space.total_free_bytes <= space.total_bytes);
    }
}

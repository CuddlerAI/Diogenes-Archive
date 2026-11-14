use anyhow::{Context, ensure};
use serde::Serialize;
use windows::Wdk::System::SystemServices::RtlGetVersion;
use windows::Win32::System::SystemInformation::OSVERSIONINFOW;

/// The first Windows build that documents
/// `MF_READWRITE_USE_ONLY_HARDWARE_TRANSFORMS`.
pub const MIN_HARDWARE_ONLY_MF_BUILD: u32 = 26_200;

#[derive(Clone, Copy, Debug, Serialize)]
pub struct WindowsVersion {
    pub major: u32,
    pub minor: u32,
    pub build: u32,
}

impl WindowsVersion {
    pub fn current() -> anyhow::Result<Self> {
        let mut value = OSVERSIONINFOW {
            dwOSVersionInfoSize: std::mem::size_of::<OSVERSIONINFOW>() as u32,
            ..Default::default()
        };
        let status = unsafe { RtlGetVersion(&mut value) };
        status.ok().context("RtlGetVersion failed")?;
        Ok(Self {
            major: value.dwMajorVersion,
            minor: value.dwMinorVersion,
            build: value.dwBuildNumber,
        })
    }

    pub fn require_hardware_only_media_foundation(self) -> anyhow::Result<()> {
        ensure!(
            self.major >= 10 && self.build >= MIN_HARDWARE_ONLY_MF_BUILD,
            "Windows build {}.{}.{} is below the {} hardware-only Media Foundation floor; recording stopped",
            self.major,
            self.minor,
            self.build,
            MIN_HARDWARE_ONLY_MF_BUILD,
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_build_before_25h2_hardware_only_contract() {
        let version = WindowsVersion {
            major: 10,
            minor: 0,
            build: MIN_HARDWARE_ONLY_MF_BUILD - 1,
        };
        assert!(version.require_hardware_only_media_foundation().is_err());
    }
}

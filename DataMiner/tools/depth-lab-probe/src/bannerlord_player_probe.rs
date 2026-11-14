//! Bannerlord single-player campaign telemetry sampler.
//!
//! A local MBSubModule publishes the authoritative `MobileParty.MainParty` transform into a
//! same-machine named mapping. The injected depth probe only reads that mapping at Present and
//! refuses torn, stale, future, or wrong-build samples.

use crate::player_writer::PlayerRootSample;
use std::sync::atomic::{Ordering, fence};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Memory::{
    FILE_MAP_READ, MapViewOfFile, OpenFileMappingW, UnmapViewOfFile,
};
use windows::Win32::System::Performance::QueryPerformanceFrequency;
use windows::core::w;

pub const BANNERLORD_EXE_SHA256: &str =
    "sha256:5fc59a9da65474a707fd3e868a24c6f0a782e9364edd4d17a8f8813171237de0";
pub const PROVIDER: &str = "bannerlord-main-party-shared-memory/v1";
const MAPPING_NAME: windows::core::PCWSTR = w!("Local\\DataMinerBannerlordTelemetryV1");
const MAGIC: u32 = 0x3142_4d44;
const VERSION: u32 = 1;
const MAPPING_BYTES: usize = 80;
const MAX_SAMPLE_AGE_MS: i64 = 100;

pub struct BannerlordPlayerSampler {
    mapping: HANDLE,
    view: *const u8,
    max_sample_age_qpc: i64,
}

unsafe impl Send for BannerlordPlayerSampler {}
unsafe impl Sync for BannerlordPlayerSampler {}

impl BannerlordPlayerSampler {
    pub fn new(actual_executable_sha256: &str) -> Result<Self, String> {
        if actual_executable_sha256 != BANNERLORD_EXE_SHA256 {
            return Err(format!(
                "Bannerlord telemetry exact-build mismatch: expected {BANNERLORD_EXE_SHA256}, got {actual_executable_sha256}"
            ));
        }
        let mapping = unsafe { OpenFileMappingW(FILE_MAP_READ.0, false, MAPPING_NAME) }
            .map_err(|error| format!("open Bannerlord telemetry mapping failed: {error}"))?;
        let mapped = unsafe { MapViewOfFile(mapping, FILE_MAP_READ, 0, 0, MAPPING_BYTES) };
        if mapped.Value.is_null() {
            unsafe { CloseHandle(mapping) }.ok();
            return Err("map Bannerlord telemetry view failed".into());
        }
        let mut frequency = 0i64;
        unsafe { QueryPerformanceFrequency(&mut frequency) }
            .map_err(|error| format!("query Bannerlord telemetry QPC frequency failed: {error}"))?;
        if frequency <= 0 {
            unsafe {
                UnmapViewOfFile(mapped).ok();
                CloseHandle(mapping).ok();
            }
            return Err("Bannerlord telemetry QPC frequency is invalid".into());
        }
        Ok(Self {
            mapping,
            view: mapped.Value.cast(),
            max_sample_age_qpc: frequency.saturating_mul(MAX_SAMPLE_AGE_MS) / 1_000,
        })
    }

    pub fn sample(
        &mut self,
        present_seq: u64,
        present_qpc: i64,
        _generation: u64,
    ) -> Result<PlayerRootSample, String> {
        if present_qpc <= 0 {
            return Err("Bannerlord player Present QPC is invalid".into());
        }
        for _ in 0..4 {
            let before = unsafe { self.view.cast::<u64>().read_volatile() };
            if before == 0 || before & 1 != 0 {
                std::hint::spin_loop();
                continue;
            }
            fence(Ordering::Acquire);
            let magic = unsafe { self.view.add(8).cast::<u32>().read_unaligned() };
            let version = unsafe { self.view.add(12).cast::<u32>().read_unaligned() };
            let sample_qpc = unsafe { self.view.add(16).cast::<i64>().read_unaligned() };
            let position = unsafe { self.view.add(24).cast::<[f64; 3]>().read_unaligned() };
            let rotation = unsafe { self.view.add(48).cast::<[f64; 4]>().read_unaligned() };
            fence(Ordering::Acquire);
            let after = unsafe { self.view.cast::<u64>().read_volatile() };
            if before != after || after & 1 != 0 {
                std::hint::spin_loop();
                continue;
            }
            if magic != MAGIC || version != VERSION {
                return Err("Bannerlord telemetry mapping identity mismatch".into());
            }
            if sample_qpc <= 0 || sample_qpc > present_qpc {
                return Err("Bannerlord telemetry QPC is invalid or from the future".into());
            }
            if present_qpc - sample_qpc > self.max_sample_age_qpc {
                return Err("Bannerlord telemetry sample is stale".into());
            }
            if !position.iter().all(|value| value.is_finite())
                || !rotation.iter().all(|value| value.is_finite())
            {
                return Err("Bannerlord telemetry contains a non-finite value".into());
            }
            let norm = rotation.iter().map(|value| value * value).sum::<f64>();
            if (norm - 1.0).abs() > 0.002 {
                return Err("Bannerlord telemetry quaternion is not normalized".into());
            }
            return Ok(PlayerRootSample {
                present_seq,
                present_qpc,
                kcd2_root_source: None,
                cp2077_camera_join: None,
                player_position: position,
                player_rotation_quaternion: rotation,
            });
        }
        Err("Bannerlord telemetry seqlock remained unstable".into())
    }
}

impl Drop for BannerlordPlayerSampler {
    fn drop(&mut self) {
        unsafe {
            UnmapViewOfFile(windows::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS {
                Value: self.view.cast_mut().cast(),
            })
            .ok();
            CloseHandle(self.mapping).ok();
        }
    }
}

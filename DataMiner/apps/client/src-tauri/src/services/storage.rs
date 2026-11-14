use std::path::Path;

use anyhow::Context;
use capture_core::audio_contract::AUDIO_PCM_BYTES_PER_SECOND;
use capture_core::profile::CaptureProfile;

use crate::domain::collector::{StorageState, StorageStatus};
use crate::platform::windows::storage::volume_space;

pub const STORAGE_RESERVE_BYTES: u64 = 10 * 1024 * 1024 * 1024;
pub const NEW_SESSION_BUDGET_SECONDS: u64 = 2 * 60 * 60;
pub const WARNING_WATERMARK_SECONDS: u64 = 30 * 60;
pub const STOP_WATERMARK_SECONDS: u64 = 10 * 60;
/// Every ready RAW segment is losslessly re-packaged by the DataModel assembler
/// into a data_demo chunk on the *same volume* (no decode, no re-encode: the
/// H.264 samples are byte-for-byte copies, plus small JSONL). One recorded
/// second therefore consumes roughly two seconds' worth of peak-rate bytes.
/// Budgeting only the RAW half made every watermark read twice as generous as
/// reality: "30 minutes left" was ~15, and the 2 h new-session budget ~1 h.
pub const DATA_MODEL_COPY_FACTOR: u64 = 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StartKind {
    NewSession,
    Resume,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StorageBudget {
    bytes_per_second: u64,
    segment_seconds: u64,
}

impl StorageBudget {
    #[must_use]
    pub fn from_profile(profile: &CaptureProfile) -> Self {
        let video_bytes_per_second = u64::from(profile.max_bitrate_bps).div_ceil(8);
        let audio_bytes_per_second = if profile.audio.is_some() {
            // Budget RAW PCM at its uncompressed rate even though zstd normally
            // shrinks it. The FLAC derivative is materialized in the cloud, so
            // it must not consume this collector-volume budget.
            AUDIO_PCM_BYTES_PER_SECOND
        } else {
            0
        };
        Self {
            // Video keeps its same-volume local DataModel copy; optional audio
            // is one RAW sidecar only.
            bytes_per_second: video_bytes_per_second
                .saturating_mul(DATA_MODEL_COPY_FACTOR)
                .saturating_add(audio_bytes_per_second),
            segment_seconds: u64::from(profile.segment_seconds),
        }
    }

    #[must_use]
    pub fn required_start_bytes(self) -> u64 {
        STORAGE_RESERVE_BYTES.saturating_add(
            self.bytes_per_second
                .saturating_mul(NEW_SESSION_BUDGET_SECONDS),
        )
    }

    #[must_use]
    pub fn required_resume_bytes(self) -> u64 {
        STORAGE_RESERVE_BYTES.saturating_add(
            self.bytes_per_second
                .saturating_mul(STOP_WATERMARK_SECONDS.saturating_add(self.segment_seconds)),
        )
    }

    #[must_use]
    pub fn required_for(self, kind: StartKind) -> u64 {
        match kind {
            StartKind::NewSession => self.required_start_bytes(),
            StartKind::Resume => self.required_resume_bytes(),
        }
    }
}

pub fn inspect(path: &Path, budget: StorageBudget) -> anyhow::Result<StorageStatus> {
    let space = volume_space(path).context("capture storage capacity is unavailable")?;
    Ok(assess_available(space.available_bytes, budget))
}

#[must_use]
pub fn unavailable(budget: StorageBudget) -> StorageStatus {
    StorageStatus {
        state: StorageState::Unavailable,
        free_bytes: None,
        required_start_bytes: budget.required_start_bytes(),
        reserve_bytes: STORAGE_RESERVE_BYTES,
        estimated_remaining_seconds: None,
        start_allowed: false,
        stop_requested: false,
    }
}

#[must_use]
pub fn allowed(status: &StorageStatus, budget: StorageBudget, kind: StartKind) -> bool {
    status
        .free_bytes
        .is_some_and(|free| free >= budget.required_for(kind))
}

#[must_use]
fn assess_available(available_bytes: u64, budget: StorageBudget) -> StorageStatus {
    let usable_bytes = available_bytes.saturating_sub(STORAGE_RESERVE_BYTES);
    let remaining_seconds = usable_bytes / budget.bytes_per_second.max(1);
    let state = if remaining_seconds <= STOP_WATERMARK_SECONDS {
        StorageState::Critical
    } else if remaining_seconds <= WARNING_WATERMARK_SECONDS {
        StorageState::Warning
    } else {
        StorageState::Ready
    };
    StorageStatus {
        state,
        free_bytes: Some(available_bytes),
        required_start_bytes: budget.required_start_bytes(),
        reserve_bytes: STORAGE_RESERVE_BYTES,
        estimated_remaining_seconds: Some(remaining_seconds),
        start_allowed: available_bytes >= budget.required_start_bytes(),
        stop_requested: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn budget() -> StorageBudget {
        StorageBudget {
            bytes_per_second: 2_250_000,
            segment_seconds: 10,
        }
    }

    #[test]
    fn new_session_budget_uses_peak_profile_rate_plus_ten_gibibytes() {
        let budget = budget();

        assert_eq!(
            budget.required_start_bytes(),
            STORAGE_RESERVE_BYTES + 2_250_000 * 7_200
        );
        assert_eq!(
            budget.required_resume_bytes(),
            STORAGE_RESERVE_BYTES + 2_250_000 * 610
        );
    }

    #[test]
    fn profile_budget_counts_the_same_volume_data_model_copy() {
        // 18 Mbit/s peak → 2.25 MB/s of RAW video, but every ready segment is
        // re-packaged into a DataModel chunk on the same volume, so the budget
        // and watermarks must burn bytes at 2× the encoder rate.
        let profile = CaptureProfile {
            profile_id: "test-profile".into(),
            non_trainable: true,
            capture_backend: "native_wgc_window_v1".into(),
            encoder_backend: "mf_nvidia_hardware_v1".into(),
            width: 1920,
            height: 1080,
            fps_num: 30,
            fps_den: 1,
            bitrate_bps: 16_000_000,
            max_bitrate_bps: 18_000_000,
            gop_frames: 120,
            segment_seconds: 10,
            test_segments: 30,
            accepted_encoder_relaxations: None,
            audio: None,
        };
        let budget = StorageBudget::from_profile(&profile);
        assert_eq!(budget.bytes_per_second, 2_250_000 * DATA_MODEL_COPY_FACTOR);

        let mut audio_profile = profile;
        audio_profile.audio = Some(capture_core::profile::CaptureAudioProfile::process_loopback());
        let audio_budget = StorageBudget::from_profile(&audio_profile);
        assert_eq!(
            audio_budget.bytes_per_second,
            2_250_000 * DATA_MODEL_COPY_FACTOR + AUDIO_PCM_BYTES_PER_SECOND
        );
    }

    #[test]
    fn watermarks_are_deterministic_and_keep_the_reserve_out_of_recording_time() {
        let budget = budget();
        let ready = assess_available(
            STORAGE_RESERVE_BYTES + budget.bytes_per_second * (WARNING_WATERMARK_SECONDS + 1),
            budget,
        );
        let warning = assess_available(
            STORAGE_RESERVE_BYTES + budget.bytes_per_second * WARNING_WATERMARK_SECONDS,
            budget,
        );
        let critical = assess_available(
            STORAGE_RESERVE_BYTES + budget.bytes_per_second * STOP_WATERMARK_SECONDS,
            budget,
        );

        assert_eq!(ready.state, StorageState::Ready);
        assert_eq!(warning.state, StorageState::Warning);
        assert_eq!(critical.state, StorageState::Critical);
        assert_eq!(critical.estimated_remaining_seconds, Some(600));
    }

    #[test]
    fn new_session_and_resume_have_distinct_preflight_floors() {
        let budget = budget();
        let resume_only = assess_available(budget.required_resume_bytes(), budget);

        assert!(!allowed(&resume_only, budget, StartKind::NewSession));
        assert!(allowed(&resume_only, budget, StartKind::Resume));
    }
}

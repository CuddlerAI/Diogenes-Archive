#[cfg(feature = "process-loopback-audio")]
pub mod audio;
pub mod audio_contract;
pub mod audio_segment;
mod cadence_trace;
pub mod capture;
pub mod crop;
pub mod d3d;
pub mod device_snapshot;
pub mod gpu;
pub mod ingest_contract;
pub mod input;
pub mod mapping;
pub mod mf;
pub mod platform;
pub mod probe;
pub mod profile;
pub mod projection;
pub mod qpc;
pub mod recording;
pub mod wall_clock;

use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct P0Status {
    pub phase: &'static str,
    pub trainable: bool,
    pub capture_backend: &'static str,
    pub release_surface: &'static str,
    pub note: &'static str,
}

impl P0Status {
    #[must_use]
    pub const fn current() -> Self {
        Self {
            phase: "p0-native-capture-lab",
            trainable: false,
            capture_backend: "native_wgc_window_v1",
            release_surface: "hwnd-only",
            note: "P0 output is unconditionally non-trainable; trainability is a server-side release decision.",
        }
    }
}

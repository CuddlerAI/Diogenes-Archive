//! HWND-only Windows Graphics Capture lifecycle used by DataMiner.
//!
//! The public surface intentionally cannot represent a display, picker result,
//! desktop duplication source, CPU frame buffer, or built-in encoder.

#![cfg(windows)]

pub mod capture;
mod d3d11;
pub mod frame;
pub mod window;

pub use capture::{
    CaptureContext, CaptureControl, CaptureError, FrameControl, GraphicsCaptureHandler,
    WGC_FRAME_POOL_BUFFER_COUNT, WgcSettings,
};
pub use frame::Frame;
pub use window::Window;

/// Stable fork identity persisted in P0 reports and later run metadata.
pub const FORK_ID: &str = "windows-capture-2.0.0-dataminer-wgc-only-v1";

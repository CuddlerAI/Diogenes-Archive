use std::ffi::c_void;

/// ABI version for [`NonBlockingSinkAbiV1`].
pub const FALL_OUT_PLAYER_TELEMETRY_SINK_ABI_V1: u32 = 1;

/// Return codes for a transport-owned, non-blocking publication callback.
#[repr(u32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TryPublishCodeV1 {
    Accepted = 0,
    WouldBlock = 1,
    Closed = 2,
    InvalidArgument = 3,
}

/// Cross-language callback implemented by the replaceable transport.
///
/// The callback must copy or consume `payload` before returning and must never
/// wait for a reader, disk, network, lock convoy, or capacity to become available.
pub type TryPublishFnV1 = unsafe extern "C" fn(
    context: *mut c_void,
    payload: *const u8,
    payload_len: usize,
) -> TryPublishCodeV1;

/// Stable x64 C ABI boundary between the future F4SE producer and a transport.
///
/// The payload is one UTF-8 JSON line using `fallout-player-telemetry/v1`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct NonBlockingSinkAbiV1 {
    pub abi_version: u32,
    pub reserved: u32,
    pub context: *mut c_void,
    pub try_publish: TryPublishFnV1,
}

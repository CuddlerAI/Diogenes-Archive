//! Shared high-frequency Raw Input reader.
//!
//! Microsoft documents the hybrid pattern used here: read the `WM_INPUT`
//! handle with `GetRawInputData`, then drain reports that accumulated between
//! message-loop iterations with `GetRawInputBuffer`. The current message and
//! its buffered tail form one observable delivery batch; callers stamp that
//! whole batch once instead of inventing hardware timestamps that `RAWINPUT`
//! does not provide.

use std::ffi::c_void;
use std::mem::size_of;

use anyhow::{Context, ensure};
use windows::Win32::UI::Input::{
    GetRawInputBuffer, GetRawInputData, HRAWINPUT, RAWINPUT, RAWINPUTHEADER, RID_INPUT,
};

const INITIAL_BUFFERED_REPORTS: usize = 256;
const MAX_RAW_INPUT_BUFFER_BYTES: usize = 16 * 1024 * 1024;
const RAW_INPUT_BUFFER_ALIGNMENT: usize = 8;

#[derive(Clone, Copy)]
pub struct CapturedRawInput {
    pub raw: RAWINPUT,
    pub buffered: bool,
}

pub struct RawInputReader {
    current_storage: Vec<u64>,
    buffered_storage: Vec<u64>,
}

impl RawInputReader {
    #[must_use]
    pub fn new() -> Self {
        let initial_bytes = INITIAL_BUFFERED_REPORTS * std::mem::size_of::<RAWINPUT>();
        Self {
            current_storage: Vec::new(),
            buffered_storage: vec![0; words_for(initial_bytes)],
        }
    }

    /// Reads the event referenced by `handle` and every Raw Input report that
    /// accumulated behind it. `output` is cleared first and remains reusable by
    /// the caller so the hot path does not allocate for normal-size batches.
    pub fn read_available(
        &mut self,
        handle: HRAWINPUT,
        output: &mut Vec<CapturedRawInput>,
    ) -> anyhow::Result<()> {
        output.clear();
        output.push(CapturedRawInput {
            raw: self.read_current(handle)?,
            buffered: false,
        });
        self.drain_buffered(output)?;
        Ok(())
    }

    fn read_current(&mut self, handle: HRAWINPUT) -> anyhow::Result<RAWINPUT> {
        let header_size = std::mem::size_of::<RAWINPUTHEADER>() as u32;
        let mut byte_count = 0_u32;
        let query =
            unsafe { GetRawInputData(handle, RID_INPUT, None, &mut byte_count, header_size) };
        ensure!(query != u32::MAX, "GetRawInputData(size) failed");
        let byte_count = usize::try_from(byte_count).context("Raw Input packet size overflow")?;
        validate_allocation_size(byte_count)?;
        self.current_storage.resize(words_for(byte_count), 0);
        let mut read_size = u32::try_from(self.current_storage.len() * size_of::<u64>())
            .context("Raw Input current buffer size overflow")?;
        let copied = unsafe {
            GetRawInputData(
                handle,
                RID_INPUT,
                Some(self.current_storage.as_mut_ptr().cast::<c_void>()),
                &mut read_size,
                header_size,
            )
        };
        ensure!(copied != u32::MAX, "GetRawInputData(payload) failed");
        ensure!(
            usize::try_from(copied).ok() == Some(byte_count),
            "GetRawInputData returned a partial packet"
        );
        copy_raw_input(self.current_storage.as_ptr().cast::<u8>(), byte_count)
    }

    fn drain_buffered(&mut self, output: &mut Vec<CapturedRawInput>) -> anyhow::Result<()> {
        let header_size = std::mem::size_of::<RAWINPUTHEADER>() as u32;
        loop {
            let buffer_bytes = self.buffered_storage.len() * size_of::<u64>();
            let mut byte_count = u32::try_from(buffer_bytes)
                .context("Raw Input buffered allocation exceeds Win32 UINT")?;
            let count = unsafe {
                GetRawInputBuffer(
                    Some(self.buffered_storage.as_mut_ptr().cast::<RAWINPUT>()),
                    &mut byte_count,
                    header_size,
                )
            };
            if count == 0 {
                return Ok(());
            }
            if count == u32::MAX {
                let reported = usize::try_from(byte_count)
                    .context("GetRawInputBuffer required-size overflow")?;
                // Microsoft requires an extra x8 adjustment under WOW64. It is
                // harmlessly conservative for the supported 64-bit build too.
                let required = reported
                    .saturating_mul(8)
                    .max(buffer_bytes.saturating_mul(2));
                validate_allocation_size(required)?;
                self.buffered_storage.resize(words_for(required), 0);
                continue;
            }
            copy_buffered_packets(
                self.buffered_storage.as_ptr().cast::<u8>(),
                buffer_bytes,
                usize::try_from(count).context("Raw Input buffered count overflow")?,
                output,
            )?;
            // Do not stop after a successful buffer read: more reports may have
            // arrived while this batch was being processed.
        }
    }
}

fn words_for(byte_count: usize) -> usize {
    byte_count.div_ceil(size_of::<u64>()).max(1)
}

fn validate_allocation_size(byte_count: usize) -> anyhow::Result<()> {
    ensure!(
        byte_count >= size_of::<RAWINPUTHEADER>(),
        "Raw Input packet is smaller than RAWINPUTHEADER"
    );
    ensure!(
        byte_count <= MAX_RAW_INPUT_BUFFER_BYTES,
        "Raw Input buffer request exceeds the 16 MiB safety bound"
    );
    Ok(())
}

fn aligned_packet_size(byte_count: usize) -> usize {
    byte_count.div_ceil(RAW_INPUT_BUFFER_ALIGNMENT) * RAW_INPUT_BUFFER_ALIGNMENT
}

fn copy_raw_input(source: *const u8, available_bytes: usize) -> anyhow::Result<RAWINPUT> {
    ensure!(
        available_bytes >= size_of::<RAWINPUTHEADER>(),
        "Raw Input buffer ends before RAWINPUTHEADER"
    );
    let header = unsafe { source.cast::<RAWINPUTHEADER>().read_unaligned() };
    let packet_size = usize::try_from(header.dwSize).context("Raw Input dwSize overflow")?;
    ensure!(
        packet_size >= size_of::<RAWINPUTHEADER>(),
        "Raw Input dwSize is smaller than RAWINPUTHEADER"
    );
    ensure!(
        packet_size <= size_of::<RAWINPUT>(),
        "unexpected variable-size Raw Input packet for mouse/keyboard registration"
    );
    ensure!(
        packet_size <= available_bytes,
        "Raw Input packet extends beyond the supplied buffer"
    );
    let mut raw = RAWINPUT::default();
    unsafe {
        std::ptr::copy_nonoverlapping(
            source,
            (&mut raw as *mut RAWINPUT).cast::<u8>(),
            packet_size,
        );
    }
    Ok(raw)
}

fn copy_buffered_packets(
    buffer: *const u8,
    buffer_bytes: usize,
    count: usize,
    output: &mut Vec<CapturedRawInput>,
) -> anyhow::Result<()> {
    let mut offset = 0_usize;
    for _ in 0..count {
        ensure!(
            offset < buffer_bytes,
            "GetRawInputBuffer count exceeds its buffer"
        );
        let raw = copy_raw_input(unsafe { buffer.add(offset) }, buffer_bytes - offset)?;
        let packet_size =
            usize::try_from(raw.header.dwSize).context("Raw Input dwSize overflow")?;
        output.push(CapturedRawInput {
            raw,
            buffered: true,
        });
        offset = offset
            .checked_add(aligned_packet_size(packet_size))
            .context("Raw Input buffer offset overflow")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::Input::{RIM_TYPEKEYBOARD, RIM_TYPEMOUSE};

    #[test]
    fn buffered_iterator_preserves_packet_order_and_alignment() {
        let packet_size = size_of::<RAWINPUT>();
        let stride = aligned_packet_size(packet_size);
        let mut storage = vec![0_u64; words_for(stride * 2)];
        let base = storage.as_mut_ptr().cast::<u8>();
        for (index, kind) in [RIM_TYPEMOUSE.0, RIM_TYPEKEYBOARD.0]
            .into_iter()
            .enumerate()
        {
            let mut raw = RAWINPUT::default();
            raw.header.dwType = kind;
            raw.header.dwSize = packet_size as u32;
            unsafe {
                std::ptr::copy_nonoverlapping(
                    (&raw as *const RAWINPUT).cast::<u8>(),
                    base.add(index * stride),
                    packet_size,
                );
            }
        }

        let mut output = Vec::new();
        copy_buffered_packets(base, storage.len() * size_of::<u64>(), 2, &mut output).unwrap();
        assert_eq!(output.len(), 2);
        assert!(output.iter().all(|packet| packet.buffered));
        assert_eq!(output[0].raw.header.dwType, RIM_TYPEMOUSE.0);
        assert_eq!(output[1].raw.header.dwType, RIM_TYPEKEYBOARD.0);
    }

    #[test]
    fn malformed_packet_size_fails_closed() {
        let mut raw = RAWINPUT::default();
        raw.header.dwSize = (size_of::<RAWINPUTHEADER>() - 1) as u32;
        let result = copy_raw_input(
            (&raw as *const RAWINPUT).cast::<u8>(),
            size_of::<RAWINPUT>(),
        );
        assert!(result.is_err());
    }
}

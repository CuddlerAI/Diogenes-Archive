//! Per-process handle census: the total count plus a kernel object-type histogram.
//!
//! `GetProcessHandleCount` returns the same number the existing PowerShell
//! resource sampler already records, so a census total lines up directly with
//! `capture-24-resources.jsonl`. The type histogram is the part the current
//! evidence is missing: a bare count cannot say *which* kernel object keeps
//! accumulating, and the collector process from Capture 24 has already exited,
//! so its handle table can no longer be recovered.
//!
//! Win32 exposes no per-process handle enumeration, so handle values are walked
//! by brute force; a process handle value is always a multiple of four. Only
//! `ObjectTypeInformation` is requested. `ObjectNameInformation` is deliberately
//! never requested: a name query can block indefinitely on a synchronous pipe
//! handle with pending I/O, and object names would put local paths and device
//! identifiers into an evidence file that is meant to be shareable.

use std::collections::BTreeMap;
use std::ffi::c_void;
use std::time::{Duration, Instant};

use anyhow::Context;
use serde::Serialize;
use windows::Win32::Foundation::{GetHandleInformation, HANDLE};
use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessHandleCount};

/// `OBJECT_INFORMATION_CLASS::ObjectTypeInformation`.
const OBJECT_TYPE_INFORMATION: i32 = 2;

/// Upper bound for the brute-force walk. The scan stops as soon as it has found
/// as many live handles as `GetProcessHandleCount` reports, so this ceiling only
/// matters when the handle table is unusually sparse.
pub const DEFAULT_MAX_HANDLE_VALUE: u32 = 4 * 1024 * 1024;

#[repr(C)]
struct UnicodeString {
    length: u16,
    maximum_length: u16,
    buffer: *mut u16,
}

#[repr(C)]
struct PublicObjectTypeInformation {
    type_name: UnicodeString,
    reserved: [u32; 22],
}

#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtQueryObject(
        handle: *mut c_void,
        information_class: i32,
        information: *mut c_void,
        information_length: u32,
        return_length: *mut u32,
    ) -> i32;
}

#[derive(Clone, Debug, Serialize)]
pub struct HandleCensus {
    /// `GetProcessHandleCount`, i.e. the number the resource sampler records.
    pub total: u32,
    /// Handle values the walk confirmed live.
    pub found: u32,
    /// Handles whose object type name came back.
    pub typed: u32,
    pub total_after_scan: u32,
    pub query_failures: u32,
    pub timed_out: bool,
    /// Object type name -> count.
    pub by_type: BTreeMap<String, u32>,
    /// True when the walk hit its ceiling before accounting for `total`.
    /// A census with this set is incomplete and its histogram is a lower bound.
    pub incomplete: bool,
    pub scan_micros: u64,
}

fn unicode_string_to_owned(value: &UnicodeString, storage: &[usize]) -> Option<String> {
    let start = storage.as_ptr() as usize;
    let end = start.checked_add(std::mem::size_of_val(storage))?;
    let address = value.buffer as usize;
    let length = usize::from(value.length);
    if length == 0
        || !length.is_multiple_of(2)
        || length > usize::from(value.maximum_length)
        || address < start
        || address.checked_add(length)? > end
        || !address.is_multiple_of(std::mem::align_of::<u16>())
    {
        return None;
    }
    // SAFETY: the returned UTF-16 range is aligned and wholly within storage.
    let units = unsafe { std::slice::from_raw_parts(value.buffer, length / 2) };
    String::from_utf16(units).ok()
}

pub fn process_handle_count() -> anyhow::Result<u32> {
    let mut count = 0_u32;
    // SAFETY: the pseudo-handle from GetCurrentProcess is always valid and
    // `count` is a live local.
    unsafe { GetProcessHandleCount(GetCurrentProcess(), &raw mut count) }
        .context("GetProcessHandleCount failed")?;
    Ok(count)
}

/// Walk this process's handle table and bucket every live handle by object type.
pub fn census(max_handle_value: u32) -> anyhow::Result<HandleCensus> {
    let started = Instant::now();
    let total = process_handle_count()?;
    let mut by_type: BTreeMap<String, u32> = BTreeMap::new();
    let mut found = 0_u32;
    let mut typed = 0_u32;
    let mut query_failures = 0;
    let mut timed_out = false;
    // Pointer-aligned storage: Vec<u8> does not guarantee struct alignment.
    let mut buffer = vec![0_usize; 4096 / std::mem::size_of::<usize>()];
    let mut value = 4_u32;
    while value <= max_handle_value && found < total {
        if value.is_multiple_of(1024) && started.elapsed() >= Duration::from_millis(500) {
            timed_out = true;
            break;
        }
        let raw = value as usize as *mut c_void;
        let mut flags = 0_u32;
        // SAFETY: an unowned handle value only makes GetHandleInformation fail;
        // it neither closes nor mutates anything.
        if unsafe { GetHandleInformation(HANDLE(raw), &raw mut flags) }.is_ok() {
            found += 1;
            let mut returned = 0_u32;
            // SAFETY: `raw` was just confirmed to be a live handle in this
            // process and `buffer` outlives the call.
            let status = unsafe {
                NtQueryObject(
                    raw,
                    OBJECT_TYPE_INFORMATION,
                    buffer.as_mut_ptr().cast::<c_void>(),
                    std::mem::size_of_val(buffer.as_slice()) as u32,
                    &raw mut returned,
                )
            };
            if status >= 0
                && returned as usize >= std::mem::size_of::<PublicObjectTypeInformation>()
            {
                // SAFETY: a successful ObjectTypeInformation query fills the
                // buffer with a PUBLIC_OBJECT_TYPE_INFORMATION.
                let info = unsafe { &*buffer.as_ptr().cast::<PublicObjectTypeInformation>() };
                if let Some(name) = unicode_string_to_owned(&info.type_name, &buffer) {
                    typed += 1;
                    *by_type.entry(name).or_insert(0) += 1;
                } else {
                    query_failures += 1;
                }
            } else {
                query_failures += 1;
            }
        }
        let Some(next) = value.checked_add(4) else {
            break;
        };
        value = next;
    }
    let total_after_scan = process_handle_count()?;
    Ok(HandleCensus {
        total,
        found,
        typed,
        total_after_scan,
        query_failures,
        timed_out,
        by_type,
        incomplete: found < total || typed < found || total_after_scan != total || timed_out,
        scan_micros: started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64,
    })
}

/// `after - before` per object type. Types that only appear in one census are
/// still reported, so a type that appeared or vanished is never silently lost.
#[must_use]
pub fn diff_by_type(
    before: &BTreeMap<String, u32>,
    after: &BTreeMap<String, u32>,
) -> BTreeMap<String, i64> {
    let mut delta = BTreeMap::new();
    for (name, count) in after {
        let previous = before.get(name).copied().unwrap_or(0);
        delta.insert(name.clone(), i64::from(*count) - i64::from(previous));
    }
    for (name, count) in before {
        delta.entry(name.clone()).or_insert(-i64::from(*count));
    }
    delta.retain(|_, value| *value != 0);
    delta
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::CreateEventW;

    #[test]
    fn decodes_only_valid_utf16_ranges_inside_query_storage() {
        let mut storage = [0_usize; 64];
        let pointer = storage.as_mut_ptr().cast::<u16>();
        unsafe {
            pointer.write(b'E' as u16);
        }
        let mut value = UnicodeString {
            length: 2,
            maximum_length: 2,
            buffer: pointer,
        };
        assert_eq!(
            unicode_string_to_owned(&value, &storage).as_deref(),
            Some("E")
        );
        value.length = 1;
        assert!(unicode_string_to_owned(&value, &storage).is_none());
        value.length = 2;
        value.buffer = std::ptr::null_mut();
        assert!(unicode_string_to_owned(&value, &storage).is_none());
        value.buffer = unsafe { storage.as_mut_ptr().add(storage.len()).cast::<u16>() };
        assert!(unicode_string_to_owned(&value, &storage).is_none());
    }

    #[test]
    fn census_identifies_real_owned_event_handles() {
        struct OwnedEvent(HANDLE);
        impl Drop for OwnedEvent {
            fn drop(&mut self) {
                let _ = unsafe { CloseHandle(self.0) };
            }
        }
        let events: Vec<_> = (0..3)
            .map(|_| OwnedEvent(unsafe { CreateEventW(None, false, false, None).unwrap() }))
            .collect();
        let snapshot = census(DEFAULT_MAX_HANDLE_VALUE).unwrap();
        assert!(snapshot.by_type.get("Event").copied().unwrap_or(0) >= events.len() as u32);
        assert_eq!(snapshot.typed, snapshot.by_type.values().sum::<u32>());
        assert!(snapshot.typed <= snapshot.found);
    }

    #[test]
    fn limited_scan_is_explicitly_incomplete() {
        let snapshot = census(4).unwrap();
        assert!(snapshot.incomplete);
    }
}

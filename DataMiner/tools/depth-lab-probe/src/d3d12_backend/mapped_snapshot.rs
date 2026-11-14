//! Exact-range volatile reads of caller-pinned mapped upload bytes. Never rounds the source
//! range outwards, never borrows memory across Execute, and never replaces lifetime ownership
//! with VirtualQuery. Wide reads are enabled only by the bound CP2077 caller.
use std::ffi::c_void;
use std::sync::OnceLock;
use std::time::Instant;

use windows::Win32::System::Diagnostics::Debug::ReadProcessMemory;
use windows::Win32::System::Memory::{PAGE_NOCACHE, PAGE_PROTECTION_FLAGS, PAGE_WRITECOMBINE};
use windows::Win32::System::Threading::GetCurrentProcess;

use crate::d3d12_render_camera::ObserverMappedIdentity;
use crate::execute_prepare::{PrepareSample, elapsed};

mod query_cache;
pub(super) use query_cache::CAPACITY as QUERY_CACHE_CAPACITY;
pub(super) use query_cache::QueryCache;

/// Manual A/B switch for the hardware comparison only: set to restore the per-Execute
/// VirtualQuery path for observer-owned mappings. Read once; remove together with the query
/// path after the fallible owned read is confirmed in game.
pub(super) fn owned_query_path_requested() -> bool {
    static REQUESTED: OnceLock<bool> = OnceLock::new();
    *REQUESTED.get_or_init(|| std::env::var_os("DM_DEPTH_PROBE_OWNED_SNAPSHOT_QUERY").is_some())
}

/// Fallible exact-range read of an observer-OWNED upload mapping, without VirtualQuery.
///
/// Admission is the ownership itself: `observer_upload_mappings` keeps a COM reference and a
/// nested Map for the whole ACTIVE_BACKEND lifetime (no retirement path), so the runtime cannot
/// release or unmap this range while the backend is alive. `ReadProcessMemory` on the current
/// process reports any unexpected protection change as a rejected read instead of raising an
/// access violation in the game, which query-then-dereference could not (its check and its
/// load were separate steps). It never walks the VM region either: VirtualQuery on the 192 MiB
/// CP2077 upload source measured 1.5 ms average / 8.7 ms max per call, several calls per
/// frame, all before the game's Execute was forwarded. Same precedent as the formal player
/// sampler (`cp2077_pose_wiring::read_process_value_no_query`).
///
/// Borrowed mappings must keep using `read`: a blind fallible read of memory this backend does
/// not pin could consume a guard page.
pub(super) fn read_owned(
    owner: ObserverMappedIdentity,
    address: usize,
    bytes: usize,
    mut metrics: Option<&mut PrepareSample>,
) -> Option<Vec<u8>> {
    let request_end = address.checked_add(bytes)?;
    let owner_end = owner.base.checked_add(usize::try_from(owner.bytes).ok()?)?;
    if address < owner.base || request_end > owner_end {
        return None;
    }
    let start = metrics.is_some().then(Instant::now);
    let mut out = vec![0_u8; bytes];
    if let Some(metrics) = metrics.as_deref_mut() {
        metrics.allocation_ns += elapsed(start);
    }
    let start = metrics.is_some().then(Instant::now);
    let mut copied = 0;
    let complete = bytes == 0
        || (unsafe {
            ReadProcessMemory(
                GetCurrentProcess(),
                address as *const c_void,
                out.as_mut_ptr().cast(),
                bytes,
                Some(&mut copied),
            )
        }
        .is_ok()
            && copied == bytes);
    if let Some(metrics) = metrics {
        metrics.memory_read_ns += elapsed(start);
        metrics.owned_fallible_reads += 1;
        if complete {
            metrics.read_bytes += bytes as u64;
        } else {
            metrics.owned_fallible_rejected += 1;
        }
    }
    complete.then_some(out)
}

/// # Safety
/// The caller must keep the entire mapped range alive and readable for this call and ensure
/// no unsynchronized CPU/GPU writer mutates it. VirtualQuery only validates current protection.
/// The tracker lock serializes Unmap retirement but is not itself a COM lifetime lease.
/// Borrowed pointers use fresh queries; caching additionally requires an owned observer Map.
pub(super) unsafe fn read(
    address: usize,
    bytes: usize,
    wide: bool,
    mut metrics: Option<&mut PrepareSample>,
) -> Option<Vec<u8>> {
    address.checked_add(bytes)?;
    let start = metrics.is_some().then(Instant::now);
    let info = super::query_readable_mapped_range(address, bytes);
    if let Some(metrics) = metrics.as_deref_mut() {
        metrics.memory_query_ns += elapsed(start);
        metrics.memory_query_calls += 1;
        metrics.memory_query_rejected += u64::from(info.is_none());
        metrics.query_cache_ineligible_reads += 1;
    }
    let info = info?;
    unsafe { read_validated(address, bytes, wide, info.Protect, metrics) }
}

/// Same safety contract as read, with caller-proven page protection for this exact range.
unsafe fn read_validated(
    address: usize,
    bytes: usize,
    wide: bool,
    protection: PAGE_PROTECTION_FLAGS,
    mut metrics: Option<&mut PrepareSample>,
) -> Option<Vec<u8>> {
    let start = metrics.is_some().then(Instant::now);
    let mut out = Vec::<u8>::with_capacity(bytes);
    if let Some(metrics) = metrics.as_deref_mut() {
        metrics.allocation_ns += elapsed(start);
    }
    let start = metrics.is_some().then(Instant::now);
    let destination = out.as_mut_ptr();
    let mut offset = 0;
    let mut wide_loads = 0;
    // Scalar prefix establishes u64 alignment without reading BEFORE the requested range.
    if wide {
        while offset < bytes && !(address + offset).is_multiple_of(8) {
            unsafe {
                destination
                    .add(offset)
                    .write(std::ptr::read_volatile((address + offset) as *const u8));
            }
            offset += 1;
        }
        while bytes - offset >= 8 {
            let value = unsafe { std::ptr::read_volatile((address + offset) as *const u64) };
            // Vec<u8> makes no alignment promise; never impose the source alignment on it.
            unsafe {
                std::ptr::write_unaligned(destination.add(offset).cast::<u64>(), value);
            }
            offset += 8;
            wide_loads += 1;
        }
    }
    while offset < bytes {
        unsafe {
            destination
                .add(offset)
                .write(std::ptr::read_volatile((address + offset) as *const u8));
        }
        offset += 1;
    }
    // Every byte was initialized above; neither the source nor destination overreads the tail.
    unsafe {
        out.set_len(bytes);
    }
    if let Some(metrics) = metrics {
        metrics.memory_read_ns += elapsed(start);
        metrics.read_bytes += bytes as u64;
        metrics.wide_loads += wide_loads;
        metrics.byte_loads += bytes as u64 - wide_loads * 8;
        if protection.contains(PAGE_WRITECOMBINE) {
            metrics.write_combine_reads += 1;
        } else if protection.contains(PAGE_NOCACHE) {
            metrics.no_cache_reads += 1;
        } else {
            // Absence of WC/NC flags is an observation, not a claim about GPU cache coherence.
            metrics.unflagged_reads += 1;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::Memory::{
        MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_NOACCESS, PAGE_PROTECTION_FLAGS, PAGE_READWRITE,
        VirtualAlloc, VirtualFree, VirtualProtect,
    };

    #[test]
    fn every_alignment_and_tail_matches_scalar_without_rounding() {
        let input: Vec<u8> = (0..512).map(|i| (i % 251) as u8).collect();
        for offset in 0..8 {
            for bytes in 0..=256 {
                let address = input.as_ptr() as usize + offset;
                let mut metrics = PrepareSample::default();
                let actual = unsafe { read(address, bytes, true, Some(&mut metrics)) }.unwrap();
                assert_eq!(actual, input[offset..offset + bytes]);
                assert_eq!(
                    actual,
                    unsafe { read(address, bytes, false, None) }.unwrap()
                );
                assert_eq!(metrics.read_bytes, bytes as u64);
                assert_eq!(metrics.wide_loads * 8 + metrics.byte_loads, bytes as u64);
            }
        }
    }

    #[test]
    fn rejects_unreadable_and_overflow_ranges_before_dereference() {
        for (address, bytes) in [(0, 256), (usize::MAX - 3, 8)] {
            assert!(unsafe { read(address, bytes, true, None) }.is_none());
        }
    }

    #[test]
    fn page_edges_are_exact_and_guard_pages_are_never_touched() {
        unsafe {
            // Three private CPU pages, no D3D device or hardware work.
            let allocation = VirtualAlloc(None, 12288, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);
            assert!(!allocation.is_null());
            struct Allocation(*mut std::ffi::c_void);
            impl Drop for Allocation {
                fn drop(&mut self) {
                    unsafe {
                        VirtualFree(self.0, 0, MEM_RELEASE).unwrap();
                    }
                }
            }
            let _allocation = Allocation(allocation);
            let base = allocation as usize;
            let middle = base + 4096;
            std::ptr::write_bytes(middle as *mut u8, 0x5a, 4096);
            let mut old = PAGE_PROTECTION_FLAGS::default();
            VirtualProtect(allocation, 4096, PAGE_NOACCESS, &mut old).unwrap();
            VirtualProtect((base + 8192) as *const _, 4096, PAGE_NOACCESS, &mut old).unwrap();
            for bytes in 0..=256 {
                assert_eq!(read(middle, bytes, true, None).unwrap(), vec![0x5a; bytes]);
                if bytes > 0 {
                    assert_eq!(
                        read(middle + 4096 - bytes, bytes, true, None).unwrap(),
                        vec![0x5a; bytes]
                    );
                }
            }
            assert!(read(middle - 1, 8, true, None).is_none());
            assert!(read(middle + 4093, 8, true, None).is_none());
            // Test-only stand-in for an owned mapping: this private allocation is retained
            // throughout the cache scope, with no protection changes or concurrent writers.
            {
                let owner = crate::d3d12_render_camera::ObserverMappedIdentity {
                    resource_raw: 1,
                    source_tag: 1,
                    content_generation: 1,
                    base,
                    bytes: 12288,
                };
                let mut cache = QueryCache::default();
                let mut metrics = PrepareSample::default();
                for address in [middle, middle + 4096 - 256] {
                    assert_eq!(
                        cache
                            .read(Some(owner), address, 256, true, Some(&mut metrics))
                            .unwrap(),
                        vec![0x5a; 256]
                    );
                }
                assert_eq!(
                    (metrics.memory_query_calls, metrics.query_cache_hits),
                    (1, 1)
                );
                assert!(
                    cache
                        .read(Some(owner), middle - 1, 8, true, Some(&mut metrics))
                        .is_none()
                );
                assert!(
                    cache
                        .read(Some(owner), middle + 4093, 8, true, Some(&mut metrics))
                        .is_none()
                );
                assert_eq!(
                    (metrics.memory_query_calls, metrics.memory_query_rejected),
                    (3, 2)
                );
            }
            // New Execute must query again, even when the address and identity repeat.
            VirtualProtect(middle as *const _, 4096, PAGE_NOACCESS, &mut old).unwrap();
            let owner = crate::d3d12_render_camera::ObserverMappedIdentity {
                resource_raw: 1,
                source_tag: 1,
                content_generation: 1,
                base,
                bytes: 12288,
            };
            let mut metrics = PrepareSample::default();
            assert!(
                QueryCache::default()
                    .read(Some(owner), middle, 256, true, Some(&mut metrics))
                    .is_none()
            );
            assert_eq!(
                (metrics.memory_query_calls, metrics.memory_query_rejected),
                (1, 1)
            );
        }
    }

    #[test]
    fn owned_read_is_exact_bounded_and_rejects_revoked_pages_without_faulting() {
        unsafe {
            // Two private CPU pages stand in for an owned mapping; no D3D device involved.
            let allocation = VirtualAlloc(None, 8192, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);
            assert!(!allocation.is_null());
            struct Allocation(*mut std::ffi::c_void);
            impl Drop for Allocation {
                fn drop(&mut self) {
                    unsafe {
                        VirtualFree(self.0, 0, MEM_RELEASE).unwrap();
                    }
                }
            }
            let _allocation = Allocation(allocation);
            let base = allocation as usize;
            for index in 0..8192 {
                std::ptr::write((base + index) as *mut u8, (index % 251) as u8);
            }
            let owner = ObserverMappedIdentity {
                resource_raw: 1,
                source_tag: 1,
                content_generation: 1,
                base,
                bytes: 8192,
            };
            let expected = |offset: usize, bytes: usize| -> Vec<u8> {
                (offset..offset + bytes).map(|i| (i % 251) as u8).collect()
            };
            let mut metrics = PrepareSample::default();
            // Page-straddling and tail-exact ranges match; no query is ever issued.
            for (offset, bytes) in [(0, 256), (4096 - 100, 256), (8192 - 256, 256), (17, 0)] {
                assert_eq!(
                    read_owned(owner, base + offset, bytes, Some(&mut metrics)).unwrap(),
                    expected(offset, bytes)
                );
            }
            assert_eq!(
                (
                    metrics.owned_fallible_reads,
                    metrics.owned_fallible_rejected,
                    metrics.memory_query_calls,
                    metrics.read_bytes,
                ),
                (4, 0, 0, 768)
            );
            // Owner bounds are enforced before any read is attempted.
            for (address, bytes) in [(base - 1, 8), (base + 8192 - 4, 8), (usize::MAX - 3, 8)] {
                assert!(read_owned(owner, address, bytes, Some(&mut metrics)).is_none());
            }
            assert_eq!(metrics.owned_fallible_reads, 4);
            // A page revoked underneath the owner is a rejected read, never an access violation.
            let mut old = PAGE_PROTECTION_FLAGS::default();
            VirtualProtect((base + 4096) as *const _, 4096, PAGE_NOACCESS, &mut old).unwrap();
            assert!(read_owned(owner, base + 4096, 256, Some(&mut metrics)).is_none());
            assert!(read_owned(owner, base + 4096 - 100, 256, Some(&mut metrics)).is_none());
            assert_eq!(
                read_owned(owner, base, 256, Some(&mut metrics)).unwrap(),
                expected(0, 256)
            );
            assert_eq!(
                (
                    metrics.owned_fallible_reads,
                    metrics.owned_fallible_rejected
                ),
                (7, 2)
            );
        }
    }

    #[test]
    #[ignore = "explicit CPU-only WC microbenchmark; not a GPU/game performance result"]
    fn benchmark_private_wc_reader() {
        unsafe {
            let allocation = VirtualAlloc(
                None,
                4096,
                MEM_COMMIT | MEM_RESERVE,
                PAGE_READWRITE | PAGE_WRITECOMBINE,
            );
            assert!(!allocation.is_null());
            for i in 0..512 {
                std::ptr::write_volatile((allocation as *mut u64).add(i), 0x5a5a5a5a5a5a5a5a);
            }
            #[cfg(target_arch = "x86_64")]
            std::arch::x86_64::_mm_sfence();
            for wide in [false, true] {
                for diagnostics in [false, true] {
                    let mut samples = Vec::new();
                    for _ in 0..200 {
                        let start = Instant::now();
                        for _ in 0..10 {
                            let mut metrics = PrepareSample::default();
                            let bytes = read(
                                allocation as usize,
                                256,
                                wide,
                                diagnostics.then_some(&mut metrics),
                            )
                            .unwrap();
                            std::hint::black_box(bytes);
                            if diagnostics {
                                assert_eq!(metrics.write_combine_reads, 1);
                                assert_eq!(metrics.read_bytes, 256);
                                assert_eq!(metrics.wide_loads, if wide { 32 } else { 0 });
                            }
                        }
                        samples.push(start.elapsed().as_secs_f64() * 100_000.0);
                    }
                    samples.sort_by(f64::total_cmp);
                    println!(
                        "wide={wide} diagnostics={diagnostics} per_call_us_p50={:.3} p95={:.3}",
                        samples[100], samples[189]
                    );
                }
            }
            VirtualFree(allocation, 0, MEM_RELEASE).unwrap();
        }
    }
}

//! Single-thread, balanced metadata-query experiments on privately owned allocations.
//! Never changes the protection/lifetime of a game or driver-owned allocation.
use super::{Options, device, nanos};
use serde::Serialize;
use std::{
    ffi::c_void,
    time::{Duration, Instant},
};
use windows::Win32::System::{
    Memory::{
        MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, PAGE_WRITECOMBINE, VirtualAlloc,
        VirtualFree,
    },
    Threading::{GetCurrentProcessorNumber, GetCurrentThread},
    WindowsProgramming::QueryThreadCycleTime,
};

pub(super) struct PrivateMemory(*mut c_void);
impl PrivateMemory {
    pub(super) fn new(bytes: usize, wc: bool) -> Result<Self, String> {
        let protection = if wc {
            PAGE_READWRITE | PAGE_WRITECOMBINE
        } else {
            PAGE_READWRITE
        };
        let address = unsafe { VirtualAlloc(None, bytes, MEM_RESERVE | MEM_COMMIT, protection) };
        if address.is_null() {
            return Err(format!(
                "private allocation failed: {}",
                windows::core::Error::from_thread()
            ));
        }
        let owned = Self(address);
        // Touch the entire allocation before warmup, just like Upload::new. No later writes.
        // These are dedicated allocations, not heap pages or another owner's D3D12 Map.
        unsafe { std::ptr::write_bytes(address, 0x5a, bytes) };
        #[cfg(target_arch = "x86_64")]
        unsafe {
            std::arch::x86_64::_mm_sfence()
        };
        Ok(owned)
    }

    pub(super) fn base(&self) -> usize {
        self.0 as usize
    }
}
impl Drop for PrivateMemory {
    fn drop(&mut self) {
        // Only the exact base returned by our own VirtualAlloc, never a queried pointer.
        let _ = unsafe { VirtualFree(self.0, 0, MEM_RELEASE) };
    }
}

fn cycles() -> Result<u64, String> {
    let mut value = 0;
    unsafe { QueryThreadCycleTime(GetCurrentThread(), &mut value) }
        .map_err(|e| format!("thread cycle counter unavailable: {e}"))?;
    Ok(value)
}

#[derive(Serialize)]
struct Sample {
    round: usize,
    order_index: usize,
    kind: &'static str,
    resource_offset: usize,
    region_resource_offset: i64,
    region_bytes: usize,
    protection: u32,
    allocation_protection: u32,
    state: u32,
    memory_type: u32,
    query_ns: u64,
    thread_cycles: u64,
    cpu_before: u32,
    cpu_after: u32,
}

#[derive(Serialize)]
struct ClockFloor {
    empty_bracket_ns: u64,
    thread_cycles: u64,
}

#[derive(Serialize)]
pub struct Report {
    schema: &'static str,
    non_trainable: bool,
    certified: bool,
    completed: bool,
    adapter: String,
    heap_mib_per_kind: usize,
    repeats_per_cell: usize,
    warmup_rounds: usize,
    order: &'static str,
    timing_scope: &'static str,
    clock_floor: Vec<ClockFloor>,
    samples: Vec<Sample>,
}

pub fn run(options: &Options, repeats: usize) -> Result<Report, String> {
    // 3 allocation kinds * 8 offsets. Each group of 24 rounds puts every cell in
    // every order position; reverse the next group. No latency-based exclusions.
    const CELLS: usize = 24;
    if !(1..=256).contains(&options.heap_mib)
        || !(1..=180).contains(&options.budget_seconds)
        || !(24..=240).contains(&repeats)
        || !repeats.is_multiple_of(CELLS)
    {
        return Err("mapping sweep needs heap 1..256 MiB, budget 1..180 s, repeats 24..240 and a multiple of 24".into());
    }
    let deadline = Instant::now() + Duration::from_secs(options.budget_seconds);
    let bytes = options.heap_mib * 1024 * 1024;
    // Retain all three simultaneously so all measured query calls can be interleaved.
    let upload = device::Upload::new(bytes, options.warp)?;
    let wb = PrivateMemory::new(bytes, false)?;
    let wc = PrivateMemory::new(bytes, true)?;
    let mappings = [
        ("d3d12_upload", upload.owner.base),
        ("private_wb", wb.0 as usize),
        ("private_wc", wc.0 as usize),
    ];
    let offsets = [
        0,
        bytes / 8,
        bytes / 4,
        bytes * 3 / 8,
        bytes / 2,
        bytes * 5 / 8,
        bytes * 3 / 4,
        bytes - 4096,
    ];
    let mut clock_floor = Vec::with_capacity(64);
    for _ in 0..64 {
        let before = cycles()?;
        let start = Instant::now();
        let empty_bracket_ns = nanos(start.elapsed());
        let delta = cycles()?
            .checked_sub(before)
            .ok_or("cycle counter regressed")?;
        clock_floor.push(ClockFloor {
            empty_bracket_ns,
            thread_cycles: delta,
        });
    }
    let warmup_rounds = 2;
    let mut samples = Vec::with_capacity(repeats * CELLS);
    for round in 0..warmup_rounds + repeats {
        let measured_round = round.saturating_sub(warmup_rounds);
        for order_index in 0..CELLS {
            if Instant::now() >= deadline {
                return Err("mapping sweep time budget exhausted; no complete result".into());
            }
            let rotated = (order_index + measured_round) % CELLS;
            let cell = if (measured_round / CELLS) % 2 == 1 {
                CELLS - 1 - rotated
            } else {
                rotated
            };
            let (kind, base) = mappings[cell % mappings.len()];
            let offset = offsets[cell / mappings.len()];
            let cpu_before = unsafe { GetCurrentProcessorNumber() };
            let before = cycles()?;
            let start = Instant::now();
            let info = super::super::query_readable_mapped_range(base + offset, 256);
            let query_ns = nanos(start.elapsed());
            let delta = cycles()?
                .checked_sub(before)
                .ok_or("cycle counter regressed")?;
            let cpu_after = unsafe { GetCurrentProcessorNumber() };
            let info = info.ok_or("owned mapping rejected by production range validator")?;
            let sample = Sample {
                round: measured_round,
                order_index,
                kind,
                resource_offset: offset,
                region_resource_offset: i64::try_from(
                    info.BaseAddress as usize as i128 - base as i128,
                )
                .map_err(|_| "relative region offset overflow")?,
                region_bytes: info.RegionSize,
                protection: info.Protect.0,
                allocation_protection: info.AllocationProtect.0,
                state: info.State.0,
                memory_type: info.Type.0,
                query_ns,
                thread_cycles: delta,
                cpu_before,
                cpu_after,
            };
            // No byte load inside query timing. Validate each 256-byte slice afterwards.
            for byte in 0..256 {
                if unsafe { std::ptr::read_volatile((base + offset + byte) as *const u8) } != 0x5a {
                    return Err("owned mapping payload mismatch".into());
                }
            }
            if round >= warmup_rounds {
                samples.push(sample);
            }
        }
    }
    Ok(Report {
        schema: "dm-local-mapping-sweep/v1",
        non_trainable: true,
        certified: false,
        completed: true,
        adapter: upload.adapter.clone(),
        heap_mib_per_kind: options.heap_mib,
        repeats_per_cell: repeats,
        warmup_rounds,
        order: "24-cell cyclic Latin order; alternate forward/reverse blocks of 24 rounds",
        timing_scope: "query_ns excludes cycle counter calls; thread_cycles includes clock overhead, is not elapsed time and is never converted to ns; no CPU affinity/priority changes; CPU endpoints do not prove no migration/preemption",
        clock_floor,
        samples,
    })
}

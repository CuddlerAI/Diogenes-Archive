//! LAB ONLY: compare specific-page metadata against the unmodified range validator.
//! No game memory, no production reader integration, no cross-call cache, no byte
//! dereference based on experimental evidence. Unknown/cross-page cases use VQ.
use super::{Options, device, mapping::PrivateMemory, nanos};
use serde::Serialize;
use std::time::{Duration, Instant};
use windows::Win32::System::{
    Memory::{
        MEM_DECOMMIT, MEMORY_BASIC_INFORMATION, PAGE_EXECUTE, PAGE_EXECUTE_READ,
        PAGE_EXECUTE_READWRITE, PAGE_EXECUTE_WRITECOPY, PAGE_GUARD, PAGE_NOACCESS,
        PAGE_PROTECTION_FLAGS, PAGE_READONLY, PAGE_READWRITE, PAGE_WRITECOPY, VirtualFree,
        VirtualProtect, VirtualQuery,
    },
    ProcessStatus::{K32QueryWorkingSetEx, PSAPI_WORKING_SET_EX_INFORMATION},
    SystemInformation::{GetSystemInfo, SYSTEM_INFO},
    Threading::GetCurrentProcess,
};

#[derive(Serialize)]
struct Page {
    success: bool,
    error_hresult: Option<i32>,
    valid: bool,
    protection: Option<u32>,
    bad: bool,
    flags: usize,
}

fn page(address: usize) -> Page {
    let mut info = PSAPI_WORKING_SET_EX_INFORMATION {
        VirtualAddress: address as *mut _,
        ..Default::default()
    };
    let success = unsafe {
        K32QueryWorkingSetEx(
            GetCurrentProcess(),
            (&mut info as *mut PSAPI_WORKING_SET_EX_INFORMATION).cast(),
            size_of::<PSAPI_WORKING_SET_EX_INFORMATION>() as u32,
        )
    }
    .as_bool();
    let error_hresult = (!success).then(|| windows::core::Error::from_thread().code().0);
    // Published PSAPI_WORKING_SET_EX_BLOCK layout: Valid bit 0; protection bits
    // 4..14; Bad bit 31. Never interpret protection when Valid is clear.
    // https://learn.microsoft.com/windows/win32/api/psapi/ns-psapi-psapi_working_set_ex_block
    let flags = unsafe { info.VirtualAttributes.Flags };
    let valid = success && flags & 1 != 0;
    Page {
        success,
        error_hresult,
        valid,
        protection: valid.then_some(((flags >> 4) & 0x7ff) as u32),
        bad: success && flags & (1 << 31) != 0,
        flags,
    }
}

#[derive(Serialize)]
struct Candidate {
    readable: bool,
    fast_accepted: bool,
    page: Option<Page>,
}

fn candidate(address: usize, bytes: usize, page_size: usize) -> Candidate {
    let in_one_page = bytes != 0
        && address
            .checked_add(bytes - 1)
            .is_some_and(|end| address / page_size == end / page_size);
    let info = in_one_page.then(|| page(address));
    let fast_accepted = info.as_ref().is_some_and(|info| {
        info.valid
            && !info.bad
            && info.protection.is_some_and(|protection| {
                protection & (PAGE_GUARD.0 | PAGE_NOACCESS.0) == 0
                    && [
                        PAGE_READONLY.0,
                        PAGE_READWRITE.0,
                        PAGE_WRITECOPY.0,
                        PAGE_EXECUTE_READ.0,
                        PAGE_EXECUTE_READWRITE.0,
                        PAGE_EXECUTE_WRITECOPY.0,
                    ]
                    .contains(&(protection & 0xff))
            })
    });
    Candidate {
        readable: fast_accepted
            || super::super::query_readable_mapped_range(address, bytes).is_some(),
        fast_accepted,
        page: info,
    }
}

#[derive(Serialize)]
struct Check {
    name: &'static str,
    reference_readable: bool,
    candidate: Candidate,
}

fn check(
    name: &'static str,
    address: usize,
    bytes: usize,
    page_size: usize,
    expected: bool,
) -> Result<Check, String> {
    let reference = super::super::query_readable_mapped_range(address, bytes).is_some();
    let result = Check {
        name,
        reference_readable: reference,
        candidate: candidate(address, bytes, page_size),
    };
    if reference != expected || result.candidate.readable != reference {
        return Err(format!(
            "Specific-page safety mismatch; NO integration: {}",
            serde_json::to_string(&result).map_err(|e| e.to_string())?
        ));
    }
    Ok(result)
}

fn protect(address: usize, bytes: usize, flags: PAGE_PROTECTION_FLAGS) -> Result<(), String> {
    let mut previous = PAGE_PROTECTION_FLAGS::default();
    unsafe { VirtualProtect(address as *const _, bytes, flags, &mut previous) }
        .map_err(|e| format!("private fixture VirtualProtect failed: {e}"))
}

fn preflight(page_size: usize) -> Result<Vec<Check>, String> {
    // Only this private VirtualAlloc fixture changes protection. NEVER touch the
    // D3D12 mapping's protection or another process/allocation.
    let owned = PrivateMemory::new(3 * page_size, false)?;
    let address = owned.base();
    let mut checks = Vec::new();
    for (name, flags, expected) in [
        ("readwrite", PAGE_READWRITE, true),
        ("readonly", PAGE_READONLY, true),
        ("noaccess", PAGE_NOACCESS, false),
        ("guard", PAGE_READWRITE | PAGE_GUARD, false),
        ("execute_only", PAGE_EXECUTE, false),
        ("execute_read", PAGE_EXECUTE_READ, true),
        ("readwrite_restored", PAGE_READWRITE, true),
    ] {
        protect(address, page_size, flags)?;
        checks.push(check(name, address + 128, 256, page_size, expected)?);
        let mut after = MEMORY_BASIC_INFORMATION::default();
        let count = unsafe {
            VirtualQuery(
                Some(address as *const _),
                &mut after,
                size_of::<MEMORY_BASIC_INFORMATION>(),
            )
        };
        if count != size_of::<MEMORY_BASIC_INFORMATION>() || after.Protect != flags {
            return Err("Page metadata observation changed private page protection/guard".into());
        }
    }
    // Both pages readable but different VQ regions: preserve the original rejection.
    protect(address + page_size, page_size, PAGE_READONLY)?;
    checks.push(check(
        "cross_protection_boundary",
        address + page_size - 128,
        256,
        page_size,
        false,
    )?);
    protect(address + page_size, page_size, PAGE_READWRITE)?;
    checks.push(check(
        "cross_uniform_pages",
        address + page_size - 128,
        256,
        page_size,
        true,
    )?);
    unsafe { VirtualFree((address + 2 * page_size) as *mut _, page_size, MEM_DECOMMIT) }
        .map_err(|e| format!("private decommit failed: {e}"))?;
    checks.push(check(
        "decommitted",
        address + 2 * page_size + 128,
        256,
        page_size,
        false,
    )?);
    checks.push(check("overflow", usize::MAX - 128, 256, page_size, false)?);
    // Repeated same-address transitions catch a stale-protection fast admission.
    for i in 0..32 {
        let readable = i % 2 == 0;
        protect(
            address,
            page_size,
            if readable {
                PAGE_READWRITE
            } else {
                PAGE_NOACCESS
            },
        )?;
        checks.push(check(
            "protection_transition",
            address + 128,
            256,
            page_size,
            readable,
        )?);
    }
    Ok(checks)
}

#[derive(Serialize)]
struct Sample {
    round: usize,
    order_index: usize,
    kind: &'static str,
    resource_offset: usize,
    region_bytes: usize,
    reference_protection: u32,
    original_first: bool,
    original_ns: u64,
    candidate_joint_ns: u64,
    candidate: Candidate,
}

#[derive(Serialize)]
pub struct Report {
    schema: &'static str,
    completed: bool,
    certified: bool,
    non_trainable: bool,
    scope: &'static str,
    not_proven: &'static str,
    adapter: String,
    page_size: usize,
    heap_mib_per_kind: usize,
    repeats: usize,
    preflight: Vec<Check>,
    samples: Vec<Sample>,
}

pub fn run(options: &Options, repeats: usize) -> Result<Report, String> {
    const CELLS: usize = 24;
    if !(1..=256).contains(&options.heap_mib)
        || !(1..=180).contains(&options.budget_seconds)
        || !(24..=240).contains(&repeats)
        || !repeats.is_multiple_of(CELLS)
    {
        return Err("page-query-compare requires heap 1..256 MiB, budget 1..180s, repeats 24..240 divisible by 24".into());
    }
    let mut system = SYSTEM_INFO::default();
    unsafe { GetSystemInfo(&mut system) };
    let page_size = system.dwPageSize as usize;
    if !page_size.is_power_of_two() || !(4096..=65536).contains(&page_size) {
        return Err("Unsupported host page size for this bounded lab".into());
    }
    let checks = preflight(page_size)?;
    let deadline = Instant::now() + Duration::from_secs(options.budget_seconds);
    let bytes = options.heap_mib * 1024 * 1024;
    let upload = device::Upload::new(bytes, options.warp)?;
    let wb = PrivateMemory::new(bytes, false)?;
    let wc = PrivateMemory::new(bytes, true)?;
    let mappings = [
        ("d3d12_upload", upload.owner.base),
        ("private_wb", wb.base()),
        ("private_wc", wc.base()),
    ];
    let offsets = [
        0,
        bytes / 8,
        bytes / 4,
        bytes * 3 / 8,
        bytes / 2,
        bytes * 5 / 8,
        bytes * 3 / 4,
        bytes - page_size,
    ];
    let mut samples = Vec::with_capacity(repeats * CELLS);
    for round in 0..repeats + 2 {
        let measured_round = round.saturating_sub(2);
        for order_index in 0..CELLS {
            if Instant::now() >= deadline {
                return Err("page-query-compare time budget exhausted".into());
            }
            let rotated = (order_index + measured_round) % CELLS;
            let cell = if (measured_round / CELLS).is_multiple_of(2) {
                rotated
            } else {
                CELLS - 1 - rotated
            };
            let (kind, base) = mappings[cell % 3];
            let offset = offsets[cell / 3];
            let address = base + offset;
            let original_first = (round + order_index) % 2 == 0;
            let mut original = None;
            let mut result = None;
            let mut original_ns = 0;
            let mut candidate_joint_ns = 0;
            for which in if original_first { [0, 1] } else { [1, 0] } {
                let start = Instant::now();
                if which == 0 {
                    original = super::super::query_readable_mapped_range(address, 256);
                    original_ns = nanos(start.elapsed());
                } else {
                    result = Some(candidate(address, 256, page_size));
                    candidate_joint_ns = nanos(start.elapsed());
                }
            }
            let original = original.ok_or("owned mapping rejected by original validator")?;
            let result = result.ok_or("missing candidate measurement")?;
            if !result.readable {
                return Err("candidate rejected owned readable mapping".into());
            }
            // Reference proof plus retained ownership, NEVER experimental proof, permits
            // these exact byte reads. Keep them outside both query timers.
            for byte in 0..256 {
                if unsafe { std::ptr::read_volatile((address + byte) as *const u8) } != 0x5a {
                    return Err("owned page payload mismatch".into());
                }
            }
            if round >= 2 {
                samples.push(Sample {
                    round: measured_round,
                    order_index,
                    kind,
                    resource_offset: offset,
                    region_bytes: original.RegionSize,
                    reference_protection: original.Protect.0,
                    original_first,
                    original_ns,
                    candidate_joint_ns,
                    candidate: result,
                });
            }
        }
    }
    Ok(Report {
        schema: "dm-local-specific-page-query/v1",
        completed: true,
        certified: false,
        non_trainable: true,
        scope: "private retained D3D12/WB/WC mappings; single-thread; fresh single-page working-set metadata or original VQ fallback; no production integration",
        not_proven: "game speed/correctness, concurrent protection changes, all supported OS/drivers; no cache or fabricated VQ region; candidate time includes its actual fallback",
        adapter: upload.adapter.clone(),
        page_size,
        heap_mib_per_kind: options.heap_mib,
        repeats,
        preflight: checks,
        samples,
    })
}

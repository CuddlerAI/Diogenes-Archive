//! Bounded unlocked-prefetch diagnostics. Never changes a query result or observes bytes.
use super::{ACTIVE, Event, Ordering, STATE, note_loss, qpc, write_line};
use crate::{d3d12_render_camera::ObserverMappedIdentity, execute_prepare::PrepareSample};
use serde::Serialize;
use std::{fs::OpenOptions, io::BufWriter, path::Path, sync::atomic::AtomicU64};
use windows::Win32::System::{
    Memory::MEMORY_BASIC_INFORMATION,
    Threading::{GetCurrentThread, GetCurrentThreadId},
    WindowsProgramming::QueryThreadCycleTime,
};

const MAX_QUERIES: usize = 32;
static NEXT_BATCH: AtomicU64 = AtomicU64::new(1);

pub(crate) struct QuerySpan {
    begin_qpc: i64,
    cycles: Result<u64, i32>,
}

fn cycles() -> Result<u64, i32> {
    let mut value = 0;
    // A pseudo-handle to this calling thread: no OpenThread, wait, or handle ownership.
    unsafe { QueryThreadCycleTime(GetCurrentThread(), &mut value) }
        .map(|()| value)
        .map_err(|error| error.code().0)
}

impl QuerySpan {
    pub(crate) fn begin() -> Self {
        let cycles = cycles();
        Self {
            begin_qpc: qpc(),
            cycles,
        }
    }
}

#[derive(Serialize)]
struct Query {
    source_tag: Option<u64>,
    content_generation: Option<u64>,
    resource_bytes: Option<u64>,
    resource_offset: Option<usize>,
    requested_bytes: usize,
    begin_qpc: i64,
    end_qpc: i64,
    qpc_valid: bool,
    query_ns: u64,
    thread_cycles: Option<u64>,
    cycle_begin_error_hresult: Option<i32>,
    cycle_end_error_hresult: Option<i32>,
    cycle_counter_regressed: bool,
    readable: bool,
    region: Option<Region>,
}

#[derive(Serialize)]
struct Region {
    // Signed relative offset: a VM page may start before the resource. Never emit raw VA.
    base_resource_offset: Option<i64>,
    bytes: usize,
    protection: u32,
    allocation_protection: u32,
    state: u32,
    memory_type: u32,
}

#[derive(Serialize)]
pub(crate) struct Batch {
    schema: &'static str,
    #[serde(skip)]
    run: u64,
    batch_id: u64,
    thread_id: u32,
    begin_qpc: i64,
    end_qpc: i64,
    planned_hints: usize,
    requests: u64,
    cache_hits: u64,
    calls: u64,
    rejected: u64,
    omitted_queries: u64,
    queries: Vec<Query>,
}

impl Batch {
    pub(crate) fn begin(hints: usize) -> Option<Self> {
        let run = ACTIVE.load(Ordering::Acquire);
        (run != 0 && hints != 0).then(|| Self {
            schema: "dm-cp2077-page-query-batch/v1",
            run,
            batch_id: NEXT_BATCH.fetch_add(1, Ordering::Relaxed),
            thread_id: unsafe { GetCurrentThreadId() },
            begin_qpc: qpc(),
            end_qpc: 0,
            planned_hints: hints,
            requests: 0,
            cache_hits: 0,
            calls: 0,
            rejected: 0,
            omitted_queries: 0,
            queries: Vec::with_capacity(hints.min(MAX_QUERIES)),
        })
    }

    pub(crate) fn record(
        &mut self,
        span: QuerySpan,
        owner: Option<ObserverMappedIdentity>,
        address: usize,
        bytes: usize,
        query_ns: u64,
        info: Option<&MEMORY_BASIC_INFORMATION>,
    ) {
        let end_qpc = qpc();
        let end_cycles = cycles();
        if self.queries.len() == MAX_QUERIES {
            self.omitted_queries += 1;
            return;
        }
        let thread_cycles = span
            .cycles
            .ok()
            .zip(end_cycles.ok())
            .and_then(|(begin, end)| end.checked_sub(begin));
        self.queries.push(Query {
            source_tag: owner.map(|o| o.source_tag),
            content_generation: owner.map(|o| o.content_generation),
            resource_bytes: owner.map(|o| o.bytes),
            resource_offset: owner.and_then(|o| address.checked_sub(o.base)),
            requested_bytes: bytes,
            begin_qpc: span.begin_qpc,
            end_qpc,
            qpc_valid: span.begin_qpc > 0 && end_qpc >= span.begin_qpc,
            query_ns,
            thread_cycles,
            cycle_begin_error_hresult: span.cycles.err(),
            cycle_end_error_hresult: end_cycles.err(),
            cycle_counter_regressed: span.cycles.is_ok()
                && end_cycles.is_ok()
                && thread_cycles.is_none(),
            readable: info.is_some(),
            region: info.map(|info| Region {
                base_resource_offset: owner.and_then(|o| {
                    i64::try_from(info.BaseAddress as usize as i128 - o.base as i128).ok()
                }),
                bytes: info.RegionSize,
                protection: info.Protect.0,
                allocation_protection: info.AllocationProtect.0,
                state: info.State.0,
                memory_type: info.Type.0,
            }),
        });
    }

    pub(crate) fn submit(mut self, sample: &PrepareSample) {
        self.end_qpc = qpc();
        self.requests = sample.query_prefetch_requests;
        self.cache_hits = sample.query_prefetch_cache_hits;
        self.calls = sample.query_prefetch_calls;
        self.rejected = sample.query_prefetch_rejected;
        // No sender clone: finish can close/drain the worker without waiting for a hook.
        if let Ok(state) = STATE.try_lock()
            && let Some(state) = state.as_ref()
            && state.run == self.run
            && ACTIVE.load(Ordering::Acquire) == self.run
        {
            if state.sender.try_send(Event::PageQuery(self)).is_err() {
                note_loss();
            }
        } else if ACTIVE.load(Ordering::Acquire) == self.run {
            note_loss();
        }
    }
}

#[derive(Default, Serialize)]
pub(super) struct Counts {
    batches: u64,
    calls: u64,
    cache_hits: u64,
    rejected: u64,
    omitted_queries: u64,
    invalid_cycles: u64,
    invalid_qpc: u64,
    // ALL successfully written query details, not just >=2ms slow calls.
    query_ns_histogram: [u64; 7],
}

pub(super) struct Writer {
    file: BufWriter<std::fs::File>,
    pub counts: Counts,
}

impl Writer {
    pub(super) fn new(
        root: &Path,
        identity: &depth_lab::StreamIdentity,
        frequency: i64,
    ) -> std::io::Result<Self> {
        let mut file = BufWriter::new(
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(root.join("cp2077-page-queries.jsonl"))?,
        );
        write_line(
            &mut file,
            &serde_json::json!({
                "schema":"dm-cp2077-page-query-header/v1", "identity":identity,
                "qpc_frequency":frequency, "diagnostic_only":true, "non_trainable":true,
                "production_rules_unchanged":true, "max_queries_per_batch":MAX_QUERIES,
                "scope":"A3_enabled_unlocked_prefetch_only; locked_fallback_remains_in_existing_prepare_counters",
                "ordering":"full_owner_then_ascending_address; actual_read_and_submission_order_unchanged",
                "batch_id_semantics":"diagnostic_only_not_native_execute_batch_or_present; no_guessed_receipt_join",
                "query_ns_scope":"existing_validated_VirtualQuery_wrapper; excludes_added_QPC_and_cycle_calls",
                "thread_cycles_scope":"calling_thread_user_plus_kernel; bracket_includes_clock_overhead; not_wall_time_or_wait_time",
                "cycles_to_time_conversion_allowed":false,
                "region_scope":"actual_returned_region_before_resource_clipping; null_on_query_or_validation_rejection",
                "raw_virtual_addresses_recorded":false,
                "instrumentation_cost":"2_cycle_calls_and_2_QPC_per_real_query_plus_bounded_batch_allocation_and_try_send; included_in_prepare_and_overall_FPS",
                "query_ns_histogram_upper_bounds_exclusive":[10000,100000,500000,1000000,2000000,5000000,null],
                "loss_counter":"cp2077-camera-diagnostics-summary.json:diagnostic_losses"
            }),
        )?;
        let mut writer = Self {
            file,
            counts: Counts::default(),
        };
        writer.flush()?;
        Ok(writer)
    }

    pub(super) fn batch(&mut self, row: Batch) -> std::io::Result<()> {
        write_line(&mut self.file, &row)?;
        self.counts.batches += 1;
        self.counts.calls += row.calls;
        self.counts.cache_hits += row.cache_hits;
        self.counts.rejected += row.rejected;
        self.counts.omitted_queries += row.omitted_queries;
        for query in row.queries {
            self.counts.invalid_cycles += u64::from(query.thread_cycles.is_none());
            self.counts.invalid_qpc += u64::from(!query.qpc_valid);
            let bucket = [10_000, 100_000, 500_000, 1_000_000, 2_000_000, 5_000_000]
                .partition_point(|&bound| bound <= query.query_ns);
            self.counts.query_ns_histogram[bucket] += 1;
        }
        Ok(())
    }

    pub(super) fn flush(&mut self) -> std::io::Result<()> {
        use std::io::Write;
        self.file.flush()
    }
}

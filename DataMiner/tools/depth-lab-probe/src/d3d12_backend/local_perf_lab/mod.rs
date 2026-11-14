//! Opt-in, debug-assertions-only CPU hot-path lab on a real, privately owned D3D12 Map.
//! No hook install, game access, swapchain, capture, admission, or certification.
mod descriptor;
mod device;
pub mod mapping;
mod native_descriptors;
pub mod working_set;

use super::{execute_query_prefetch::sort_hints, mapped_snapshot};
use crate::d3d12_render_camera::{ObserverMappedIdentity, ObserverMappedQuery};
use crate::execute_prepare::{PrepareSample, nanos};
use serde::{Deserialize, Serialize};
use std::sync::{Barrier, Mutex, MutexGuard, TryLockError};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Serialize)]
pub struct Options {
    pub warp: bool,
    pub heap_mib: usize,
    pub threads: Vec<usize>,
    pub batches: usize,
    pub rounds: usize,
    pub descriptor_copies: usize,
    pub descriptors_per_copy: usize,
    pub descriptor_lock: String,
    pub descriptor_layout: String,
    pub descriptor_handles: String,
    pub descriptor_owner_trace: bool,
    pub disable_source_cache: bool,
    pub descriptor_refresh_every: usize,
    pub budget_seconds: u64,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            warp: false,
            heap_mib: 192,
            threads: vec![1, 4, 8],
            batches: 64,
            rounds: 4,
            descriptor_copies: 2000,
            descriptors_per_copy: 8,
            descriptor_lock: "std".into(),
            descriptor_layout: "balanced".into(),
            descriptor_handles: "synthetic".into(),
            descriptor_owner_trace: false,
            disable_source_cache: false,
            descriptor_refresh_every: 0,
            budget_seconds: 90,
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Workload {
    pub schema: String,
    pub scope: String,
    pub source_sha256: Option<String>,
    pub batches: Vec<Vec<usize>>,
}
impl Workload {
    pub fn synthetic(heap_bytes: usize) -> Self {
        Self {
            schema: "dm-local-query-workload/v1".into(),
            scope: "deterministic_generated_offsets; not_a_complete_game_trace".into(),
            source_sha256: None,
            batches: (0..64)
                .map(|i| {
                    let base = (i * 2_752_512) % (heap_bytes - 65_536);
                    [
                        49_152, 32_768, 50_176, 16_384, 40_960, 8_192, 57_344, 17_408, 24_576,
                    ]
                    .map(|v| base + v)
                    .to_vec()
                })
                .collect(),
        }
    }
}

#[derive(Serialize)]
pub struct Stats {
    samples: usize,
    mean_ns: f64,
    p50_ns: u64,
    p95_ns: u64,
    p99_ns: u64,
    max_ns: u64,
}
impl Stats {
    fn from(mut values: Vec<u64>) -> Self {
        values.sort_unstable();
        let percentile = |p: usize| {
            values[(values.len() * p)
                .div_ceil(100)
                .saturating_sub(1)
                .min(values.len() - 1)]
        };
        Self {
            samples: values.len(),
            mean_ns: values.iter().map(|&v| v as f64).sum::<f64>() / values.len() as f64,
            p50_ns: percentile(50),
            p95_ns: percentile(95),
            p99_ns: percentile(99),
            max_ns: *values.last().unwrap(),
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum Mode {
    QueryInsideModelLock,
    OriginalOrder,
    SortedOrder,
}

#[derive(Serialize)]
struct QueryCase {
    round: usize,
    threads: usize,
    mode: Mode,
    wall_ns: u64,
    batch_latency: Stats,
    model_lock_wait: Stats,
    contended: u64,
    queries: u64,
    query_ns: u64,
    cache_hits: u64,
    read_hits: u64,
    read_bytes: u64,
}
#[derive(Default)]
struct QueryWorker {
    latency: Vec<u64>,
    wait: Vec<u64>,
    contended: u64,
    queries: u64,
    query_ns: u64,
    cache_hits: u64,
    read_hits: u64,
    read_bytes: u64,
}

#[derive(Serialize)]
struct MappingProbe {
    resource_offset: usize,
    region_resource_offset: i64,
    region_bytes: usize,
    protection: u32,
    memory_type: u32,
    query_ns: u64,
}

#[derive(Serialize)]
pub struct Report {
    schema: &'static str,
    non_trainable: bool,
    certified: bool,
    scope: &'static str,
    not_proven: [&'static str; 5],
    options: Options,
    adapter: String,
    vendor_id: u32,
    device_id: u32,
    logical_processors: usize,
    mapping_probes: Vec<MappingProbe>,
    workload: Workload,
    query_cases: Vec<QueryCase>,
    descriptor_cases: Vec<descriptor::Case>,
    completed: bool,
    source_cache_preflight: Option<bool>,
}

pub fn run(options: Options, workload: Workload) -> Result<Report, String> {
    let o = &options;
    let descriptor_lock = crate::d3d12_render_camera::LabDescriptorLock::parse(&o.descriptor_lock)?;
    let source_only_hot = match o.descriptor_layout.as_str() {
        "balanced" => false,
        "source_hot" => true,
        _ => return Err("descriptor-layout must be balanced or source_hot".into()),
    };
    if !matches!(o.descriptor_handles.as_str(), "synthetic" | "native")
        || (o.descriptor_handles == "native" && source_only_hot)
    {
        return Err(
            "descriptor-handles must be synthetic or native; native requires balanced layout"
                .into(),
        );
    }
    if o.descriptor_owner_trace
        && (!cfg!(feature = "local-perf-lab-lock-trace")
            || (cfg!(feature = "local-perf-lab-source-cache") && !o.disable_source_cache)
            || o.descriptor_copies > 2000
            || o.descriptors_per_copy > 8
            || o.rounds > 2
            || o.threads.len() > 3)
    {
        return Err("descriptor-owner-trace requires local-perf-lab-lock-trace without source-cache; at most 2000 copies, 8 descriptors/copy, 2 rounds and 3 thread counts".into());
    }
    if o.disable_source_cache && !cfg!(feature = "local-perf-lab-source-cache") {
        return Err(
            "no-descriptor-source-cache requires the source-cache feature for same-layout A/B"
                .into(),
        );
    }
    if o.descriptor_refresh_every > 10_000
        || (o.descriptor_refresh_every != 0
            && (o.descriptor_handles != "synthetic" || o.descriptor_owner_trace))
    {
        return Err("descriptor-refresh-every must be 0..10000; nonzero requires synthetic handles without owner trace".into());
    }
    if !(1..=256).contains(&o.heap_mib)
        || o.threads.is_empty()
        || o.threads.len() > 6
        || o.threads.iter().any(|v| !(1..=16).contains(v))
        || !(1..=256).contains(&o.batches)
        || !(1..=8).contains(&o.rounds)
        || !(1..=10_000).contains(&o.descriptor_copies)
        || !(1..=64).contains(&o.descriptors_per_copy)
        || !(1..=180).contains(&o.budget_seconds)
    {
        return Err("Invalid/beyond-bounds local lab options".into());
    }
    let heap_bytes = o.heap_mib * 1024 * 1024;
    if workload.schema != "dm-local-query-workload/v1"
        || workload.batches.is_empty()
        || workload.batches.len() > 4096
        || workload.batches.iter().any(|batch| {
            batch.is_empty()
                || batch.len() > mapped_snapshot::QUERY_CACHE_CAPACITY
                || batch
                    .iter()
                    .any(|&v| v.checked_add(256).is_none_or(|end| end > heap_bytes))
        })
    {
        return Err(
            "Invalid/beyond-bounds query workload; expected bounded 256-byte slices".into(),
        );
    }
    #[cfg(feature = "local-perf-lab-source-cache")]
    let source_cache_preflight = {
        crate::d3d12_render_camera::CbvDescriptorStore::lab_validate_source_cache()?;
        Some(true)
    };
    #[cfg(not(feature = "local-perf-lab-source-cache"))]
    let source_cache_preflight = None;
    let upload = device::Upload::new(heap_bytes, o.warp)?;
    let mut mapping_probes = Vec::new();
    for offset in [0, heap_bytes / 4, heap_bytes / 2, heap_bytes * 3 / 4] {
        let start = Instant::now();
        let info = super::query_readable_mapped_range(upload.owner.base + offset, 256)
            .ok_or("mapping metadata probe rejected")?;
        let query_ns = nanos(start.elapsed());
        mapping_probes.push(MappingProbe {
            resource_offset: offset,
            region_resource_offset: i64::try_from(
                info.BaseAddress as usize as i128 - upload.owner.base as i128,
            )
            .map_err(|_| "relative region offset overflow")?,
            region_bytes: info.RegionSize,
            protection: info.Protect.0,
            memory_type: info.Type.0,
            query_ns,
        });
    }
    let deadline = Instant::now() + Duration::from_secs(o.budget_seconds);
    let mut query_cases = Vec::new();
    let mut descriptor_cases = Vec::new();
    for &threads in &o.threads {
        for mode in [
            Mode::QueryInsideModelLock,
            Mode::OriginalOrder,
            Mode::SortedOrder,
        ] {
            query_case(upload.owner, &workload, threads, 2, 0, mode, deadline)?;
        }
        for round in 0..o.rounds {
            // Reverse the complete case order on alternate rounds (ABCCBA), no winner picking.
            let mut modes = [
                Mode::QueryInsideModelLock,
                Mode::OriginalOrder,
                Mode::SortedOrder,
            ];
            if round % 2 == 1 {
                modes.reverse();
            }
            for mode in modes {
                query_cases.push(query_case(
                    upload.owner,
                    &workload,
                    threads,
                    o.batches,
                    round,
                    mode,
                    deadline,
                )?);
            }
            let mut cases = [(false, false), (false, true), (true, false), (true, true)];
            if round % 2 == 1 {
                cases.reverse();
            }
            for (hot_shard, churn) in cases {
                descriptor_cases.push(descriptor::run(
                    (o, &upload),
                    threads,
                    round,
                    (hot_shard, churn, source_only_hot),
                    descriptor_lock,
                    deadline,
                )?);
            }
        }
    }
    Ok(Report {
        schema: "dm-local-depth-perf-lab/v1",
        non_trainable: true,
        certified: false,
        scope: "real_D3D12_upload_Map_and_production_query_cache_reader_sort_descriptor_store; optional_private_native_descriptor_copies; modeled_work_scheduling",
        not_proven: [
            "CP2077_FPS_or_1percent_low",
            "GPU_draw_WGC_or_game_driver_CopyDescriptors_cost",
            "full_Execute_tracker_or_30percent_game_lock_contention",
            "player_camera_same_engine_frame",
            "four_stage_certification_or_new_diagnostic_sidecar_cost",
        ],
        adapter: upload.adapter.clone(),
        vendor_id: upload.vendor_id,
        device_id: upload.device_id,
        logical_processors: std::thread::available_parallelism().map_or(0, |n| n.get()),
        mapping_probes,
        options,
        workload,
        query_cases,
        descriptor_cases,
        completed: true,
        source_cache_preflight,
    })
}

fn lock_model<'a>(
    lock: &'a Mutex<()>,
    worker: &mut QueryWorker,
) -> Result<MutexGuard<'a, ()>, String> {
    let start = Instant::now();
    let guard = match lock.try_lock() {
        Ok(guard) => guard,
        Err(TryLockError::WouldBlock) => {
            worker.contended += 1;
            lock.lock().map_err(|_| "model lock poisoned")?
        }
        Err(_) => return Err("model lock poisoned".into()),
    };
    worker.wait.push(nanos(start.elapsed()));
    Ok(guard)
}

fn query_case(
    owner: ObserverMappedIdentity,
    workload: &Workload,
    threads: usize,
    batches: usize,
    round: usize,
    mode: Mode,
    deadline: Instant,
) -> Result<QueryCase, String> {
    let lock = Mutex::new(());
    let ready = Barrier::new(threads + 1);
    let start_gate = Barrier::new(threads + 1);
    let (workers, wall) = std::thread::scope(|scope| -> Result<_, String> {
        let mut handles = Vec::new();
        for thread in 0..threads {
            let (ready, start_gate, lock) = (&ready, &start_gate, &lock);
            handles.push(scope.spawn(move || -> Result<QueryWorker, String> {
                let mut worker = QueryWorker {
                    latency: Vec::with_capacity(batches),
                    wait: Vec::with_capacity(batches),
                    ..Default::default()
                };
                ready.wait();
                start_gate.wait();
                for batch in 0..batches {
                    if Instant::now() >= deadline {
                        return Err("Lab time budget exhausted; no complete result".into());
                    }
                    // Same evenly spread offset multiset for EVERY worker/thread-count; only
                    // rotate its order. Do not confound more threads with more high addresses.
                    let index =
                        ((batch + thread * 17) % batches) * workload.batches.len() / batches;
                    let offsets = &workload.batches[index];
                    let started = Instant::now();
                    let mut hints: Vec<_> = offsets
                        .iter()
                        .map(|&offset| ObserverMappedQuery {
                            owner,
                            address: owner.base + offset,
                            bytes: 256,
                        })
                        .collect();
                    let early_guard = matches!(mode, Mode::QueryInsideModelLock)
                        .then(|| lock_model(lock, &mut worker))
                        .transpose()?;
                    if matches!(mode, Mode::SortedOrder) {
                        sort_hints(&mut hints);
                    }
                    let mut sample = PrepareSample::default();
                    let mut cache = mapped_snapshot::QueryCache::default();
                    for hint in hints {
                        cache.prefetch(hint, Some(&mut sample));
                    }
                    let late_guard = early_guard
                        .is_none()
                        .then(|| lock_model(lock, &mut worker))
                        .transpose()?;
                    let mut reads = Vec::with_capacity(offsets.len());
                    // The lab owns the COM resource and Map until ALL workers join. No writer
                    // or lifecycle mutation exists. Read in original order, even in sorted mode.
                    for &offset in offsets {
                        let bytes = unsafe {
                            cache.read(
                                Some(owner),
                                owner.base + offset,
                                256,
                                true,
                                Some(&mut sample),
                            )
                        }
                        .ok_or("mapped snapshot rejected")?;
                        reads.push(bytes);
                    }
                    drop(late_guard);
                    drop(early_guard);
                    worker.latency.push(nanos(started.elapsed()));
                    // Correctness checks outside batch latency; end-to-end wall includes them.
                    if reads
                        .iter()
                        .any(|bytes| bytes.len() != 256 || bytes.iter().any(|&b| b != 0x5a))
                    {
                        return Err("mapped bytes mismatch".into());
                    }
                    if sample.memory_query_rejected + sample.query_prefetch_rejected != 0 {
                        return Err("unexpected query rejection".into());
                    }
                    worker.queries += sample.memory_query_calls + sample.query_prefetch_calls;
                    worker.query_ns += sample.memory_query_ns + sample.query_prefetch_query_ns;
                    worker.cache_hits += sample.query_prefetch_cache_hits;
                    worker.read_hits += sample.query_prefetch_read_hits;
                    worker.read_bytes += sample.read_bytes;
                }
                Ok(worker)
            }));
        }
        ready.wait();
        let start = Instant::now();
        start_gate.wait();
        let workers = handles
            .into_iter()
            .map(|h| h.join().map_err(|_| "query worker panic".to_string())?)
            .collect::<Result<Vec<_>, _>>()?;
        Ok((workers, nanos(start.elapsed())))
    })?;
    let mut all = QueryWorker::default();
    for worker in workers {
        all.latency.extend(worker.latency);
        all.wait.extend(worker.wait);
        all.contended += worker.contended;
        all.queries += worker.queries;
        all.query_ns += worker.query_ns;
        all.cache_hits += worker.cache_hits;
        all.read_hits += worker.read_hits;
        all.read_bytes += worker.read_bytes;
    }
    Ok(QueryCase {
        round,
        threads,
        mode,
        wall_ns: wall,
        batch_latency: Stats::from(all.latency),
        model_lock_wait: Stats::from(all.wait),
        contended: all.contended,
        queries: all.queries,
        query_ns: all.query_ns,
        cache_hits: all.cache_hits,
        read_hits: all.read_hits,
        read_bytes: all.read_bytes,
    })
}

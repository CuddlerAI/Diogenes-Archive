use super::*;
#[cfg(feature = "local-perf-lab-lock-trace")]
use crate::d3d12_render_camera::descriptor_lab_trace as trace;
use crate::d3d12_render_camera::{CbvDescriptorStore, CopyTiming, LabDescriptorLock};

#[derive(Serialize)]
pub(super) struct Case {
    round: usize,
    threads: usize,
    hot_shard: bool,
    source_only_hot: bool,
    gpuva_churn: bool,
    lock_kind: LabDescriptorLock,
    shard_policy: &'static str,
    source_shards_used: usize,
    destination_shards_used: usize,
    handle_workload: &'static str,
    native_increment: Option<usize>,
    native_worker_stride_descriptors: Option<usize>,
    driver_copy_latency: Option<Stats>,
    checked_index_and_generations: bool,
    shard_cell_bytes: usize,
    shard_cell_alignment: usize,
    copy_plan_inline_capacity: usize,
    final_validation_in_wall: bool,
    source_cache_enabled: bool,
    source_cache_hits: u64,
    source_rewrites: u64,
    source_refresh_latency: Option<Stats>,
    #[cfg(feature = "local-perf-lab-lock-trace")]
    owner_trace: Option<trace::Correlation>,
    wall_ns: u64,
    copy_latency: Stats,
    shard_wait: Stats,
    shard_apply: Stats,
    descriptors_per_copy: usize,
    same_gpuva_updates: u64,
    reindexed_updates: u64,
    worst_copy: CopySample,
}

/// Joint metrics for one actual operation, not independently selected histogram maxima.
#[derive(Clone, Copy, Default, Serialize)]
struct CopySample {
    worker: usize,
    copy_index: usize,
    total_ns: u64,
    wait_ns: u64,
    apply_ns: u64,
}

struct Worker {
    cache_hits: u64,
    latency: Vec<u64>,
    wait: Vec<u64>,
    apply: Vec<u64>,
    driver: Vec<u64>,
    refresh: Vec<u64>,
    worst_copy: CopySample,
    #[cfg(feature = "local-perf-lab-lock-trace")]
    trace: Option<trace::ThreadTrace>,
}

fn handle(
    thread: usize,
    bank: usize,
    index: usize,
    per_copy: usize,
    hot: bool,
    source_only: bool,
) -> u64 {
    let shard = if hot && source_only && bank == 0 {
        // Each worker's destination stays in a separate shard, all sources share shard 1.
        2 + thread
    } else if hot {
        1
    } else {
        (thread * per_copy + index) % 64
    };
    0x1000_0000
        + ((thread * 4 + bank) as u64 * 0x40000)
        + (shard as u64 * 4096)
        + index as u64 * 32
        + 256
}

pub(super) fn run(
    context: (&Options, &device::Upload),
    threads: usize,
    round: usize,
    workload: (bool, bool, bool),
    lock_kind: LabDescriptorLock,
    deadline: Instant,
) -> Result<Case, String> {
    let (options, upload) = context;
    let copies = options.descriptor_copies;
    let per_copy = options.descriptors_per_copy;
    let refresh_every = options.descriptor_refresh_every;
    let refresh_batches = if refresh_every == 0 {
        0
    } else {
        copies.div_ceil(refresh_every)
    };
    let (hot, churn, source_only_hot) = workload;
    let native = if options.descriptor_handles == "native" {
        Some(native_descriptors::NativeDescriptors::new(
            upload, threads, per_copy, hot,
        )?)
    } else {
        None
    };
    let get_handle = |thread, bank, index| {
        native.as_ref().map_or_else(
            || handle(thread, bank, index, per_copy, hot, source_only_hot),
            |n| n.handle(thread, bank, index),
        )
    };
    let gpuva = |thread, bank, index| {
        native.as_ref().map_or(
            0x1000000 + ((thread * per_copy + index) * 2 + bank) as u64 * 256,
            |n| n.gpuva(thread, bank, index),
        )
    };
    let store = CbvDescriptorStore::new_lab_lock(lock_kind);
    #[cfg(feature = "local-perf-lab-source-cache")]
    let store = store.lab_source_cache_policy(!options.disable_source_cache);
    let sets: Vec<_> = (0..threads)
        .map(|thread| {
            let dest: Vec<_> = (0..per_copy).map(|i| get_handle(thread, 0, i)).collect();
            let sources: [Vec<u64>; 2] = std::array::from_fn(|bank| {
                (0..per_copy)
                    .map(|i| {
                        let h = get_handle(thread, bank + 1, i);
                        store.write_cbv_descriptor(h, gpuva(thread, bank, i), 256);
                        h
                    })
                    .collect()
            });
            (dest, sources)
        })
        .collect();
    let source_shards_used = sets
        .iter()
        .flat_map(|(_, sources)| sources.iter().flatten())
        .map(|&h| CbvDescriptorStore::lab_shard(h))
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    let destination_shards_used = sets
        .iter()
        .flat_map(|(dest, _)| dest)
        .map(|&h| CbvDescriptorStore::lab_shard(h))
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    // Warm both source banks and allocate every destination before measuring.
    for (worker, (dest, sources)) in sets.iter().enumerate() {
        for bank in [0, usize::from(churn)] {
            if let Some(n) = &native {
                n.copy(worker, bank);
            }
            if !store.copy_cbv_descriptors(dest, &sources[bank]) {
                return Err("descriptor warmup failed".into());
            }
        }
    }
    let before = store
        .update_counts()
        .ok_or("descriptor store unavailable")?;
    let validate = |worker: usize, dest: &[u64]| -> Result<(), String> {
        if dest.iter().any(|&h| store.generation(h).is_none()) {
            return Err("descriptor destination lost".into());
        }
        let final_bank = if churn { (copies - 1) % 2 } else { 0 };
        for (i, &h) in dest.iter().enumerate() {
            if store.lab_value(h) != Some((gpuva(worker, final_bank, i), 256)) {
                return Err("descriptor final payload mismatch".into());
            }
        }
        Ok(())
    };
    #[cfg(feature = "local-perf-lab-lock-trace")]
    let trace_origin = Instant::now();
    let trace_enabled = options.descriptor_owner_trace;
    let ready = Barrier::new(threads + 1);
    let start_gate = Barrier::new(threads + 1);
    let (workers, wall_ns) = std::thread::scope(|scope| -> Result<_, String> {
        let mut handles = Vec::new();
        for (worker, (dest, sources)) in sets.iter().enumerate() {
            let (store, ready, start_gate, native, validate, gpuva) = (
                &store,
                &ready,
                &start_gate,
                native.as_ref(),
                &validate,
                &gpuva,
            );
            handles.push(scope.spawn(move || -> Result<_, String> {
                let mut latency = Vec::with_capacity(copies);
                let mut wait = Vec::with_capacity(copies);
                let mut apply = Vec::with_capacity(copies);
                let mut driver = Vec::with_capacity(if native.is_some() { copies } else { 0 });
                let mut refresh = Vec::with_capacity(refresh_batches);
                let mut worst_copy = CopySample::default();
                #[cfg(feature = "local-perf-lab-source-cache")]
                let mut cache_hits = 0;
                #[cfg(not(feature = "local-perf-lab-source-cache"))]
                let cache_hits = 0;
                #[cfg(feature = "local-perf-lab-lock-trace")]
                if trace_enabled {
                    trace::start(worker, trace_origin, copies * per_copy * 2);
                }
                ready.wait();
                start_gate.wait();
                for index in 0..copies {
                    if Instant::now() >= deadline {
                        return Err("Lab time budget exhausted; no complete result".into());
                    }
                    let bank = if churn { index % 2 } else { 0 };
                    #[cfg(feature = "local-perf-lab-lock-trace")]
                    if trace_enabled {
                        trace::begin_copy(index);
                    }
                    let source = &sources[bank];
                    // Model a new descriptor generation with identical GPUVA/size. Each
                    // worker owns its handles, but epochs can contend on a shared shard.
                    // This is CPU bookkeeping only (native mode rejects this option).
                    // Account separately; wall time includes writes, copy latency does not.
                    if refresh_every != 0 && index % refresh_every == 0 {
                        let start = Instant::now();
                        for (i, &handle) in source.iter().enumerate() {
                            store.write_cbv_descriptor(handle, gpuva(worker, bank, i), 256);
                        }
                        refresh.push(nanos(start.elapsed()));
                    }
                    if let Some(n) = native {
                        let start = Instant::now();
                        n.copy(worker, bank);
                        driver.push(nanos(start.elapsed()));
                    }
                    let started = Instant::now();
                    let plan = CbvDescriptorStore::prepare_copy(dest, source)
                        .ok_or("descriptor plan rejected")?;
                    let mut timing = CopyTiming::default();
                    if store.apply_copy(plan, &mut timing) != Some(true) {
                        return Err("descriptor copy failed".into());
                    }
                    let sample = CopySample {
                        worker,
                        copy_index: index,
                        total_ns: nanos(started.elapsed()),
                        wait_ns: nanos(timing.wait),
                        apply_ns: nanos(timing.apply),
                    };
                    latency.push(sample.total_ns);
                    #[cfg(feature = "local-perf-lab-source-cache")]
                    {
                        cache_hits += u64::from(timing.source_cache_hit);
                    }
                    wait.push(sample.wait_ns);
                    apply.push(sample.apply_ns);
                    if index == 0 || sample.total_ns > worst_copy.total_ns {
                        worst_copy = sample;
                    }
                }
                #[cfg(feature = "local-perf-lab-lock-trace")]
                let trace = if trace_enabled { trace::finish() } else { None };
                // Trace mode must not introduce unobserved validation holders while another
                // worker is still copying. Validate only after ALL workers have joined.
                if !trace_enabled {
                    validate(worker, dest)?;
                }
                Ok(Worker {
                    cache_hits,
                    latency,
                    wait,
                    apply,
                    worst_copy,
                    driver,
                    refresh,
                    #[cfg(feature = "local-perf-lab-lock-trace")]
                    trace,
                })
            }));
        }
        ready.wait();
        let started = Instant::now();
        start_gate.wait();
        let workers = handles
            .into_iter()
            .map(|h| {
                h.join()
                    .map_err(|_| "descriptor worker panic".to_string())?
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok((workers, nanos(started.elapsed())))
    })?;
    if trace_enabled {
        for (worker, (dest, _)) in sets.iter().enumerate() {
            validate(worker, dest)?;
        }
    }
    let after = store
        .update_counts()
        .ok_or("descriptor store unavailable")?;
    if !store.lab_validate() {
        return Err("descriptor reverse index or generation invariant failed".into());
    }
    if let Some(n) = &native {
        n.check_device()?;
    }
    let same = after.0 - before.0;
    let reindexed = after.1 - before.1;
    let expected = (threads * copies * per_copy) as u64;
    let source_rewrites = (threads * refresh_batches * per_copy) as u64;
    if same != source_rewrites + if churn { 0 } else { expected }
        || reindexed != if churn { expected } else { 0 }
    {
        return Err("descriptor update accounting mismatch".into());
    }
    let (mut latency, mut wait, mut apply) = (Vec::new(), Vec::new(), Vec::new());
    let mut driver_latency = Vec::new();
    let mut refresh_latency = Vec::new();
    let mut worst_copy = CopySample::default();
    let mut source_cache_hits = 0;
    #[cfg(feature = "local-perf-lab-lock-trace")]
    let mut traces = Vec::new();
    for worker in workers {
        source_cache_hits += worker.cache_hits;
        latency.extend(worker.latency);
        wait.extend(worker.wait);
        apply.extend(worker.apply);
        driver_latency.extend(worker.driver);
        refresh_latency.extend(worker.refresh);
        #[cfg(feature = "local-perf-lab-lock-trace")]
        if let Some(trace) = worker.trace {
            traces.push(trace);
        }
        if worker.worst_copy.total_ns > worst_copy.total_ns {
            worst_copy = worker.worst_copy;
        }
    }
    if refresh_every == 1 && source_cache_hits != 0 {
        return Err("source cache reused metadata after a source rewrite".into());
    }
    if refresh_latency.len() != threads * refresh_batches {
        return Err("source refresh sample accounting mismatch".into());
    }
    #[cfg(feature = "local-perf-lab-lock-trace")]
    let owner_trace = if trace_enabled {
        let count_shards = |handles: &[u64]| {
            handles
                .iter()
                .map(|&h| CbvDescriptorStore::lab_shard(h))
                .collect::<std::collections::BTreeSet<_>>()
                .len()
        };
        let expected_visits = sets
            .iter()
            .map(|(dest, sources)| {
                let source_visits = if churn {
                    copies.div_ceil(2) * count_shards(&sources[0])
                        + copies / 2 * count_shards(&sources[1])
                } else {
                    copies * count_shards(&sources[0])
                };
                copies * count_shards(dest) + source_visits
            })
            .sum();
        Some(trace::correlate(traces, expected_visits)?)
    } else {
        None
    };
    Ok(Case {
        round,
        threads,
        hot_shard: hot,
        source_only_hot: hot && source_only_hot,
        gpuva_churn: churn,
        lock_kind,
        shard_policy: CbvDescriptorStore::lab_shard_policy(),
        source_shards_used,
        destination_shards_used,
        checked_index_and_generations: true,
        shard_cell_bytes: CbvDescriptorStore::lab_shard_layout().0,
        shard_cell_alignment: CbvDescriptorStore::lab_shard_layout().1,
        copy_plan_inline_capacity: if cfg!(feature = "local-perf-lab-inline-plan") {
            8
        } else {
            0
        },
        final_validation_in_wall: !trace_enabled,
        source_cache_enabled: cfg!(feature = "local-perf-lab-source-cache")
            && !options.disable_source_cache,
        source_cache_hits,
        source_rewrites,
        source_refresh_latency: if refresh_latency.is_empty() {
            None
        } else {
            Some(Stats::from(refresh_latency))
        },
        #[cfg(feature = "local-perf-lab-lock-trace")]
        owner_trace,
        handle_workload: if native.is_none() {
            "synthetic"
        } else if hot {
            "native_compact"
        } else {
            "native_spaced"
        },
        native_increment: native.as_ref().map(|n| n.increment),
        native_worker_stride_descriptors: native.as_ref().map(|n| n.worker_stride_descriptors),
        driver_copy_latency: if driver_latency.is_empty() {
            None
        } else {
            Some(Stats::from(driver_latency))
        },
        wall_ns,
        copy_latency: Stats::from(latency),
        shard_wait: Stats::from(wait),
        shard_apply: Stats::from(apply),
        descriptors_per_copy: per_copy,
        same_gpuva_updates: same,
        reindexed_updates: reindexed,
        worst_copy,
    })
}

//! Shared CBV state. At most one shard lock is held by any thread at a time.
//! Hot descriptor hooks never take the render-camera mutex. Cold callers may
//! hold that mutex before visiting one shard, never the reverse.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
#[cfg(not(all(feature = "local-perf-lab", debug_assertions)))]
use std::sync::{Mutex, MutexGuard};
#[cfg(all(feature = "local-perf-lab", debug_assertions))]
pub(crate) mod lab_lock;
#[cfg(all(feature = "local-perf-lab-lock-trace", debug_assertions))]
pub(crate) mod lab_trace;
#[cfg(all(feature = "local-perf-lab-source-cache", debug_assertions))]
mod source_cache;
#[cfg(all(feature = "local-perf-lab", debug_assertions))]
use lab_lock::{LabGuard as MutexGuard, LabMutex as Mutex};
use std::time::{Duration, Instant};

use super::{CbvDescriptor, MAX_DESCRIPTOR_COPY_HANDLES};
use crate::descriptor_timing::{CopyPhase, DescriptorCopySample};

const SHARDS: usize = 64;
// Packed sort keys cache the shard without two extra per-copy allocations. The low
// bits retain ORIGINAL descriptor order within a shard, including duplicate targets.
const COPY_INDEX_BITS: usize = 12;
const COPY_INDEX_MASK: usize = (1 << COPY_INDEX_BITS) - 1;
const _: () = assert!(MAX_DESCRIPTOR_COPY_HANDLES <= (1 << COPY_INDEX_BITS));

#[cfg(all(feature = "local-perf-lab-inline-plan", debug_assertions))]
type CopyOrder = smallvec::SmallVec<[usize; 8]>;
#[cfg(not(all(feature = "local-perf-lab-inline-plan", debug_assertions)))]
type CopyOrder = Vec<usize>;
#[cfg(all(feature = "local-perf-lab-inline-plan", debug_assertions))]
type CopyValues = smallvec::SmallVec<[Option<CbvDescriptor>; 8]>;
#[cfg(not(all(feature = "local-perf-lab-inline-plan", debug_assertions)))]
type CopyValues = Vec<Option<CbvDescriptor>>;

#[derive(Default)]
struct Shard {
    descriptors: HashMap<u64, CbvDescriptor>,
    // Only range membership is queried: no caller needs the reverse handle list.
    // Count live forward entries instead of allocating a BTreeSet for every GPUVA.
    descriptor_counts_by_gpuva: BTreeMap<u64, usize>,
    // Updated under the existing shard guard; no new hot-path atomics/locks.
    same_gpuva_updates: u64,
    reindexed_updates: u64,
}

impl Shard {
    fn remove(&mut self, handle: u64) {
        let Some(descriptor) = self.descriptors.remove(&handle) else {
            return;
        };
        self.remove_gpuva_member(descriptor.gpu_virtual_address);
    }

    fn remove_gpuva_member(&mut self, gpuva: u64) {
        if let Some(count) = self.descriptor_counts_by_gpuva.get_mut(&gpuva) {
            *count -= 1;
            if *count == 0 {
                self.descriptor_counts_by_gpuva.remove(&gpuva);
            }
        }
    }

    fn insert(&mut self, handle: u64, descriptor: CbvDescriptor) {
        // Always replace the complete forward value (including size and generation).
        // One lookup also avoids making the changed-GPUVA path more expensive.
        if let Some(previous) = self.descriptors.insert(handle, descriptor) {
            if previous.gpu_virtual_address == descriptor.gpu_virtual_address {
                // Membership is unchanged, NOT descriptor identity.
                self.same_gpuva_updates = self.same_gpuva_updates.saturating_add(1);
                return;
            }
            self.remove_gpuva_member(previous.gpu_virtual_address);
        }
        *self
            .descriptor_counts_by_gpuva
            .entry(descriptor.gpu_virtual_address)
            .or_default() += 1;
        self.reindexed_updates = self.reindexed_updates.saturating_add(1);
    }
}

#[cfg(test)]
thread_local! {
    static HELD_SHARD_LOCKS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static PEAK_SHARD_LOCKS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

struct ShardGuard<'a>(MutexGuard<'a, Shard>);
#[cfg_attr(
    all(feature = "local-perf-lab-pad-shards", debug_assertions),
    repr(align(64))
)]
struct ShardCell {
    mutex: Mutex<Shard>,
    #[cfg(all(feature = "local-perf-lab-source-cache", debug_assertions))]
    cache_epoch: source_cache::Epoch,
}
impl ShardCell {
    fn new(mutex: Mutex<Shard>) -> Self {
        Self {
            mutex,
            #[cfg(all(feature = "local-perf-lab-source-cache", debug_assertions))]
            cache_epoch: source_cache::Epoch::default(),
        }
    }
}
#[cfg(not(all(feature = "local-perf-lab", debug_assertions)))]
type ShardReadGuard<'a> = ShardGuard<'a>;
#[cfg(all(feature = "local-perf-lab", debug_assertions))]
struct ShardReadGuard<'a>(lab_lock::LabReadGuard<'a, Shard>);
#[cfg(all(feature = "local-perf-lab", debug_assertions))]
impl Deref for ShardReadGuard<'_> {
    type Target = Shard;
    fn deref(&self) -> &Shard {
        &self.0
    }
}
#[cfg(all(feature = "local-perf-lab", debug_assertions))]
impl Drop for ShardReadGuard<'_> {
    fn drop(&mut self) {
        #[cfg(test)]
        HELD_SHARD_LOCKS.with(|held| {
            assert_eq!(held.get(), 1);
            held.set(0);
        });
    }
}
impl Deref for ShardGuard<'_> {
    type Target = Shard;
    fn deref(&self) -> &Shard {
        &self.0
    }
}
impl DerefMut for ShardGuard<'_> {
    fn deref_mut(&mut self) -> &mut Shard {
        &mut self.0
    }
}
impl Drop for ShardGuard<'_> {
    fn drop(&mut self) {
        #[cfg(test)]
        HELD_SHARD_LOCKS.with(|held| {
            assert_eq!(held.get(), 1);
            held.set(0);
        });
    }
}

/// Sum of wait and held-work durations for ONE copy, not one sample per shard.
#[derive(Default)]
pub(crate) struct CopyTiming {
    pub(crate) wait: Duration,
    pub(crate) apply: Duration,
    pub(crate) detail: Option<DescriptorCopySample>,
    #[cfg(all(feature = "local-perf-lab-source-cache", debug_assertions))]
    pub(crate) source_cache_hit: bool,
}

/// Grouping/allocation belongs to descriptor_copy_prepare, before lock timing.
pub(crate) struct PreparedCopy<'a> {
    sources: &'a [u64],
    destinations: &'a [u64],
    source_order: CopyOrder,
    destination_order: CopyOrder,
    copied: CopyValues,
    pub(crate) shard_count: u32,
}

/// Independently locked descriptor and GPU-VA indices. The original single lock
/// queued copies/writes; holding several shards merely chained that queue across
/// shards. Every operation now releases one shard before acquiring another.
pub(crate) struct CbvDescriptorStore {
    #[cfg(all(feature = "local-perf-lab-source-cache", debug_assertions))]
    cache_owner: u64,
    #[cfg(all(feature = "local-perf-lab-source-cache", debug_assertions))]
    cache_enabled: bool,
    shards: [ShardCell; SHARDS],
    next_descriptor_generation: AtomicU64,
    // A poisoned later shard can interrupt a staged copy after earlier writes.
    // Never expose that partially updated table as valid evidence afterward.
    unusable: AtomicBool,
}

impl CbvDescriptorStore {
    #[cfg(all(feature = "local-perf-lab", debug_assertions))]
    pub(crate) fn new_lab_lock(kind: lab_lock::LockKind) -> Self {
        Self {
            shards: std::array::from_fn(|_| {
                ShardCell::new(Mutex::with_kind(Shard::default(), kind))
            }),
            #[cfg(all(feature = "local-perf-lab-source-cache", debug_assertions))]
            cache_owner: source_cache::new_owner(),
            #[cfg(all(feature = "local-perf-lab-source-cache", debug_assertions))]
            cache_enabled: true,
            next_descriptor_generation: AtomicU64::new(1),
            unusable: AtomicBool::new(false),
        }
    }

    pub(crate) fn new() -> Self {
        Self {
            shards: std::array::from_fn(|_| ShardCell::new(Mutex::new(Shard::default()))),
            #[cfg(all(feature = "local-perf-lab-source-cache", debug_assertions))]
            cache_owner: source_cache::new_owner(),
            #[cfg(all(feature = "local-perf-lab-source-cache", debug_assertions))]
            cache_enabled: true,
            next_descriptor_generation: AtomicU64::new(1),
            unusable: AtomicBool::new(false),
        }
    }

    fn shard(handle: u64) -> usize {
        #[cfg(not(all(feature = "local-perf-lab-fine-hash", debug_assertions)))]
        let page = handle >> 12;
        #[cfg(all(feature = "local-perf-lab-fine-hash", debug_assertions))]
        let page = handle >> 8;
        #[cfg(all(feature = "local-perf-lab-page-hash", debug_assertions))]
        let page = {
            // XOR every six-bit group. Page/fine granularity are lab-only alternatives.
            let page = page ^ (page >> 48);
            let page = page ^ (page >> 24);
            let page = page ^ (page >> 12);
            page ^ (page >> 6)
        };
        (page as usize) & (SHARDS - 1)
    }

    #[cfg(all(feature = "local-perf-lab", debug_assertions))]
    pub(crate) fn lab_shard(handle: u64) -> usize {
        Self::shard(handle)
    }

    #[cfg(all(feature = "local-perf-lab", debug_assertions))]
    pub(crate) fn lab_shard_layout() -> (usize, usize) {
        (
            std::mem::size_of::<ShardCell>(),
            std::mem::align_of::<ShardCell>(),
        )
    }

    #[cfg(all(feature = "local-perf-lab", debug_assertions))]
    pub(crate) fn lab_shard_policy() -> &'static str {
        if cfg!(feature = "local-perf-lab-fine-hash") {
            "xor_fold_256_handle_group"
        } else if cfg!(feature = "local-perf-lab-page-hash") {
            "xor_fold_full_page"
        } else {
            "low_page_bits"
        }
    }

    #[cfg(all(feature = "local-perf-lab", debug_assertions))]
    pub(crate) fn lab_value(&self, handle: u64) -> Option<(u64, u32)> {
        self.get(&handle)
            .map(|d| (d.gpu_virtual_address, d.size_in_bytes))
    }

    fn lock_shard(&self, index: usize) -> Option<ShardGuard<'_>> {
        #[cfg(test)]
        HELD_SHARD_LOCKS.with(|held| assert_eq!(held.get(), 0, "nested shard acquisition"));
        if self.unusable.load(Ordering::Acquire) {
            return None;
        }
        let guard = match self.shards[index].mutex.lock() {
            Ok(guard) => guard,
            Err(_) => {
                self.unusable.store(true, Ordering::Release);
                return None;
            }
        };
        if self.unusable.load(Ordering::Acquire) {
            return None;
        }
        #[cfg(test)]
        {
            HELD_SHARD_LOCKS.with(|held| held.set(1));
            PEAK_SHARD_LOCKS.with(|peak| peak.set(peak.get().max(1)));
        }
        Some(ShardGuard(guard))
    }

    fn read_shard(&self, index: usize) -> Option<ShardReadGuard<'_>> {
        #[cfg(not(all(feature = "local-perf-lab", debug_assertions)))]
        {
            self.lock_shard(index)
        }
        #[cfg(all(feature = "local-perf-lab", debug_assertions))]
        {
            #[cfg(test)]
            HELD_SHARD_LOCKS.with(|held| assert_eq!(held.get(), 0, "nested shard acquisition"));
            if self.unusable.load(Ordering::Acquire) {
                return None;
            }
            let guard = match self.shards[index].mutex.read() {
                Ok(g) => g,
                Err(_) => {
                    self.unusable.store(true, Ordering::Release);
                    return None;
                }
            };
            if self.unusable.load(Ordering::Acquire) {
                return None;
            }
            #[cfg(test)]
            {
                HELD_SHARD_LOCKS.with(|held| held.set(1));
                PEAK_SHARD_LOCKS.with(|peak| peak.set(peak.get().max(1)));
            }
            Some(ShardReadGuard(guard))
        }
    }

    pub(crate) fn prepare_copy<'a>(
        destinations: &'a [u64],
        sources: &'a [u64],
    ) -> Option<PreparedCopy<'a>> {
        if destinations.len() != sources.len() || destinations.len() > MAX_DESCRIPTOR_COPY_HANDLES {
            return None;
        }
        let order = |handles: &[u64]| {
            let mut indices: CopyOrder = handles
                .iter()
                .enumerate()
                .map(|(i, &handle)| (Self::shard(handle) << COPY_INDEX_BITS) | i)
                .collect();
            indices.sort_unstable();
            indices
        };
        let source_order = order(sources);
        let destination_order = order(destinations);
        let mask = source_order
            .iter()
            .chain(&destination_order)
            .fold(0u64, |mask, &key| mask | (1u64 << (key >> COPY_INDEX_BITS)));
        #[cfg(all(feature = "local-perf-lab-inline-plan", debug_assertions))]
        let copied = smallvec::smallvec![None; sources.len()];
        #[cfg(not(all(feature = "local-perf-lab-inline-plan", debug_assertions)))]
        let copied = vec![None; sources.len()];
        Some(PreparedCopy {
            sources,
            destinations,
            source_order,
            destination_order,
            copied,
            shard_count: mask.count_ones(),
        })
    }

    fn visit_copy_shard<R>(
        &self,
        index: usize,
        phase: CopyPhase,
        count: usize,
        timing: &mut CopyTiming,
        work: impl FnOnce(&mut Shard) -> R,
    ) -> Option<R> {
        #[cfg(all(feature = "local-perf-lab-lock-trace", debug_assertions))]
        let mut trace = lab_trace::Attempt::begin(index, phase);
        let start = Instant::now();
        let mut guard = self.lock_shard(index);
        #[cfg(all(feature = "local-perf-lab-lock-trace", debug_assertions))]
        if let Some(trace) = trace.as_mut() {
            trace.acquired(guard.is_some());
        }
        let wait = start.elapsed();
        timing.wait += wait;
        let result = guard.as_mut().map(|shard| {
            #[cfg(all(feature = "local-perf-lab-source-cache", debug_assertions))]
            self.shards[index].cache_epoch.disable();
            let start = Instant::now();
            let result = work(shard);
            timing.apply += start.elapsed();
            result
        });
        #[cfg(all(feature = "local-perf-lab-lock-trace", debug_assertions))]
        if let Some(trace) = trace.as_mut() {
            trace.unlocking();
        }
        drop(guard);
        #[cfg(all(feature = "local-perf-lab-lock-trace", debug_assertions))]
        if let Some(trace) = trace {
            trace.released();
        }
        // Diagnostic bookkeeping must not extend the shard's critical section.
        if let Some(detail) = timing.detail.as_mut() {
            detail.visit(index, phase, count, wait);
        }
        result
    }

    /// All sources see the pre-copy values before ANY destination changes.
    /// D3D12 permits concurrent descriptor copies only without conflicting
    /// writes to descriptors involved in those calls. Thus a cross-shard atomic
    /// snapshot is unnecessary for a correctly synchronized application:
    /// https://learn.microsoft.com/en-us/windows/win32/direct3d12/copying-descriptors
    /// Each shard's forward/reverse indices remain atomic under its own lock.
    pub(crate) fn apply_copy(
        &self,
        mut plan: PreparedCopy<'_>,
        timing: &mut CopyTiming,
    ) -> Option<bool> {
        if let Some(detail) = timing.detail.as_mut() {
            detail.descriptors = plan.destinations.len() as u32;
            detail.shard_count = plan.shard_count;
        }
        #[cfg(all(feature = "local-perf-lab-source-cache", debug_assertions))]
        let stamp = self.source_stamp(&plan);
        #[cfg(all(feature = "local-perf-lab-source-cache", debug_assertions))]
        let cached = stamp.is_some_and(|s| self.cached_sources(&mut plan, s));
        #[cfg(not(all(feature = "local-perf-lab-source-cache", debug_assertions)))]
        let cached = false;
        if !cached {
            for group in plan
                .source_order
                .chunk_by(|&a, &b| (a >> COPY_INDEX_BITS) == (b >> COPY_INDEX_BITS))
            {
                let index = group[0] >> COPY_INDEX_BITS;
                #[cfg(all(feature = "local-perf-lab-lock-trace", debug_assertions))]
                let mut trace = lab_trace::Attempt::begin(index, CopyPhase::Source);
                let start = Instant::now();
                let guard = self.read_shard(index);
                #[cfg(all(feature = "local-perf-lab-lock-trace", debug_assertions))]
                if let Some(trace) = trace.as_mut() {
                    trace.acquired(guard.is_some());
                }
                let wait = start.elapsed();
                timing.wait += wait;
                let result = guard.as_ref().map(|shard| {
                    let start = Instant::now();
                    for &key in group {
                        let i = key & COPY_INDEX_MASK;
                        plan.copied[i] = shard.descriptors.get(&plan.sources[i]).copied();
                    }
                    timing.apply += start.elapsed();
                });
                #[cfg(all(feature = "local-perf-lab-lock-trace", debug_assertions))]
                if let Some(trace) = trace.as_mut() {
                    trace.unlocking();
                }
                drop(guard);
                #[cfg(all(feature = "local-perf-lab-lock-trace", debug_assertions))]
                if let Some(trace) = trace {
                    trace.released();
                }
                if let Some(detail) = timing.detail.as_mut() {
                    detail.visit(index, CopyPhase::Source, group.len(), wait);
                }
                result?;
            }
            #[cfg(all(feature = "local-perf-lab-source-cache", debug_assertions))]
            self.remember_sources(&plan, stamp);
        }
        #[cfg(all(feature = "local-perf-lab-source-cache", debug_assertions))]
        {
            timing.source_cache_hit = cached;
        }
        let copied_count = plan.copied.iter().filter(|d| d.is_some()).count() as u64;
        let mut tracked = copied_count != 0;
        // Allocate in ORIGINAL destination order, including overwritten duplicate
        // destinations. Missing sources consume no generation. Do this after all
        // source reads, before grouping destination writes by shard.
        // Generations are equality-only identities, not cross-thread completion times.
        // Reserve one disjoint range per call instead of bouncing this global cache line
        // for every descriptor. Serial order, duplicate targets and absent sources retain
        // the original numbering. Wrapping matches the old atomic fetch_add semantics.
        if copied_count != 0 {
            let mut generation = self
                .next_descriptor_generation
                .fetch_add(copied_count, Ordering::Relaxed);
            for descriptor in plan.copied.iter_mut().flatten() {
                descriptor.generation = generation;
                generation = generation.wrapping_add(1);
            }
        }
        for group in plan
            .destination_order
            .chunk_by(|&a, &b| (a >> COPY_INDEX_BITS) == (b >> COPY_INDEX_BITS))
        {
            self.visit_copy_shard(
                group[0] >> COPY_INDEX_BITS,
                CopyPhase::Destination,
                group.len(),
                timing,
                |shard| {
                    for &key in group {
                        let i = key & COPY_INDEX_MASK;
                        let destination = plan.destinations[i];
                        if let Some(descriptor) = plan.copied[i] {
                            shard.insert(destination, descriptor);
                        } else {
                            tracked |= shard.descriptors.contains_key(&destination);
                            shard.remove(destination);
                        }
                    }
                },
            )?;
        }
        // Both sides absent: no insert/remove changed state and no generation was
        // consumed. Inspect destinations even when every source was absent.
        Some(tracked)
    }

    pub(crate) fn copy_cbv_descriptors(&self, destinations: &[u64], sources: &[u64]) -> bool {
        Self::prepare_copy(destinations, sources)
            .and_then(|plan| self.apply_copy(plan, &mut CopyTiming::default()))
            .unwrap_or(false)
    }

    pub(crate) fn write_cbv_descriptor(&self, handle: u64, gpu_va: u64, size: u32) {
        if handle == 0 {
            return;
        }
        let Some(mut shard) = self.lock_shard(Self::shard(handle)) else {
            return;
        };
        #[cfg(all(feature = "local-perf-lab-source-cache", debug_assertions))]
        let _cache_write = self.shards[Self::shard(handle)].cache_epoch.write();
        if gpu_va == 0 || size < 64 || !size.is_multiple_of(256) {
            shard.remove(handle);
            return;
        }
        shard.insert(
            handle,
            CbvDescriptor {
                gpu_virtual_address: gpu_va,
                size_in_bytes: size,
                generation: self
                    .next_descriptor_generation
                    .fetch_add(1, Ordering::Relaxed),
            },
        );
    }

    pub(crate) fn invalidate_cbv_descriptor(&self, handle: u64) {
        if let Some(mut shard) = self.lock_shard(Self::shard(handle)) {
            #[cfg(all(feature = "local-perf-lab-source-cache", debug_assertions))]
            let _cache_write = self.shards[Self::shard(handle)].cache_epoch.write();
            shard.remove(handle);
        }
    }

    pub(super) fn get(&self, handle: &u64) -> Option<CbvDescriptor> {
        self.read_shard(Self::shard(*handle))?
            .descriptors
            .get(handle)
            .copied()
    }

    /// Only the opt-in Execute diagnostic uses these clocks. Ordinary descriptor hot paths
    /// retain get() unchanged. Release this shard before returning to the camera tracker.
    pub(super) fn get_measured(&self, handle: &u64) -> (Option<CbvDescriptor>, CopyTiming) {
        let start = Instant::now();
        let shard = self.read_shard(Self::shard(*handle));
        let wait = start.elapsed();
        let start = Instant::now();
        let result = shard
            .as_ref()
            .and_then(|shard| shard.descriptors.get(handle).copied());
        drop(shard);
        (
            result,
            CopyTiming {
                wait,
                apply: start.elapsed(),
                ..Default::default()
            },
        )
    }

    /// Lock only the descriptor's own shard and expose no CBV contents. The Draw hot path uses
    /// this exact generation to reject a stale redundancy cache before taking the camera mutex.
    pub(crate) fn generation(&self, handle: u64) -> Option<u64> {
        self.get(&handle).map(|descriptor| descriptor.generation)
    }

    #[cfg(test)]
    pub(super) fn contains_key(&self, handle: &u64) -> bool {
        self.get(handle).is_some()
    }

    /// Approximate under concurrent mutations; used only for diagnostics.
    pub(crate) fn len(&self) -> usize {
        let mut total = 0;
        for index in 0..SHARDS {
            let Some(shard) = self.read_shard(index) else {
                return 0;
            };
            total += shard.descriptors.len();
        }
        total
    }

    /// Cold Ready/Stop counter checkpoint. Each shard is released before the next;
    /// concurrent totals are approximate, just like the existing timing checkpoints.
    pub(crate) fn update_counts(&self) -> Option<(u64, u64)> {
        let mut same = 0u64;
        let mut reindexed = 0u64;
        for index in 0..SHARDS {
            let shard = self.read_shard(index)?;
            same = same.saturating_add(shard.same_gpuva_updates);
            reindexed = reindexed.saturating_add(shard.reindexed_updates);
        }
        Some((same, reindexed))
    }

    /// Cold lab-only invariant check AFTER all writers have joined. No hot-path cost.
    #[cfg(all(feature = "local-perf-lab", debug_assertions))]
    pub(crate) fn lab_validate(&self) -> bool {
        let mut generations = BTreeSet::new();
        for index in 0..SHARDS {
            let Some(shard) = self.read_shard(index) else {
                return false;
            };
            let mut expected = BTreeMap::<u64, usize>::new();
            for (&handle, descriptor) in &shard.descriptors {
                if Self::shard(handle) != index
                    || descriptor.generation == 0
                    || !generations.insert(descriptor.generation)
                {
                    return false;
                }
                *expected.entry(descriptor.gpu_virtual_address).or_default() += 1;
            }
            if expected != shard.descriptor_counts_by_gpuva {
                return false;
            }
        }
        true
    }

    /// Union of individually locked scans, not an atomic whole-table snapshot.
    /// A CBV created concurrently with unmap can miss its shard's scan. The old
    /// atomic scan likewise missed CBVs created after it released its locks; this
    /// changes the interleaving window, not the class of missed snapshots. Missing
    /// upload bytes fail closed in resolve_descriptor_table_binding.
    pub(super) fn gpuvas_in_range(&self, start: u64, end: u64) -> BTreeSet<u64> {
        let mut result = BTreeSet::new();
        for index in 0..SHARDS {
            let Some(shard) = self.read_shard(index) else {
                return BTreeSet::new();
            };
            result.extend(shard.descriptor_counts_by_gpuva.range(start..end).map(
                |(&v, &count)| {
                    debug_assert!(count > 0);
                    v
                },
            ));
        }
        result
    }

    pub(super) fn release_descriptor_heap(&self, start: u64, end: u64) {
        for index in 0..SHARDS {
            let Some(mut shard) = self.lock_shard(index) else {
                return;
            };
            #[cfg(all(feature = "local-perf-lab-source-cache", debug_assertions))]
            let _cache_write = self.shards[index].cache_epoch.write();
            let removed: Vec<_> = shard
                .descriptors
                .keys()
                .copied()
                .filter(|h| (start..end).contains(h))
                .collect();
            for handle in removed {
                shard.remove(handle);
            }
        }
    }

    #[cfg(test)]
    fn entries(&self) -> BTreeMap<u64, CbvDescriptor> {
        let mut entries = BTreeMap::new();
        for index in 0..SHARDS {
            let shard = self.lock_shard(index).unwrap();
            entries.extend(shard.descriptors.iter().map(|(&h, &d)| (h, d)));
        }
        entries
    }

    #[cfg(test)]
    fn assert_indices_consistent(&self) {
        let mut generations = BTreeSet::new();
        for index in 0..SHARDS {
            let shard = self.lock_shard(index).unwrap();
            let mut expected_counts = BTreeMap::<u64, usize>::new();
            for (&handle, descriptor) in &shard.descriptors {
                assert_eq!(Self::shard(handle), index);
                *expected_counts
                    .entry(descriptor.gpu_virtual_address)
                    .or_default() += 1;
                assert!(
                    generations.insert(descriptor.generation),
                    "duplicate live generation"
                );
            }
            assert_eq!(shard.descriptor_counts_by_gpuva, expected_counts);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::d3d12_render_camera::RenderCameraTracker;
    // The outer tracker is never part of the descriptor-lock experiment.
    use std::sync::{Arc, Barrier, Mutex, mpsc};
    use std::time::{Duration, Instant};

    struct Reference {
        entries: BTreeMap<u64, CbvDescriptor>,
        next: u64,
    }
    impl Reference {
        fn new() -> Self {
            Self {
                entries: BTreeMap::new(),
                next: 1,
            }
        }
        fn write(&mut self, handle: u64, gpuva: u64, size: u32) {
            if handle == 0 {
                return;
            }
            if gpuva == 0 || size < 64 || !size.is_multiple_of(256) {
                self.entries.remove(&handle);
            } else {
                self.entries.insert(
                    handle,
                    CbvDescriptor {
                        gpu_virtual_address: gpuva,
                        size_in_bytes: size,
                        generation: self.next,
                    },
                );
                self.next += 1;
            }
        }
        fn copy(&mut self, destinations: &[u64], sources: &[u64]) -> bool {
            if destinations.len() != sources.len()
                || destinations.len() > MAX_DESCRIPTOR_COPY_HANDLES
            {
                return false;
            }
            if !sources
                .iter()
                .chain(destinations)
                .any(|h| self.entries.contains_key(h))
            {
                return false;
            }
            let before = self.entries.clone();
            for (&destination, source) in destinations.iter().zip(sources) {
                if let Some(mut descriptor) = before.get(source).copied() {
                    descriptor.generation = self.next;
                    self.next += 1;
                    self.entries.insert(destination, descriptor);
                } else {
                    self.entries.remove(&destination);
                }
            }
            true
        }
        fn assert_matches(&self, store: &CbvDescriptorStore) {
            assert_eq!(store.entries(), self.entries);
            assert_eq!(
                store.next_descriptor_generation.load(Ordering::Relaxed),
                self.next
            );
            store.assert_indices_consistent();
        }
    }

    #[test]
    fn copy_scans_release_and_diagnostics_never_hold_two_shards() {
        HELD_SHARD_LOCKS.with(|held| assert_eq!(held.get(), 0));
        PEAK_SHARD_LOCKS.with(|peak| peak.set(0));
        let store = CbvDescriptorStore::new();
        let mut reference = Reference::new();
        for i in 0..64 {
            let h = 0x100000 + i * 4096;
            store.write_cbv_descriptor(h, 0x200000 + i * 256, 256);
            reference.write(h, 0x200000 + i * 256, 256);
        }
        // Shuffled shard order, a cycle, missing sources and duplicate targets.
        let sources = [0x13f000, 0x100000, 0x101000, 0x102020, 0x102000];
        let destinations = [0x101000, 0x13f000, 0x100000, 0x102000, 0x101000];
        assert_eq!(
            store.copy_cbv_descriptors(&destinations, &sources),
            reference.copy(&destinations, &sources)
        );
        reference.assert_matches(&store);
        assert_eq!(store.len(), reference.entries.len());
        assert!(!store.gpuvas_in_range(0x200000, 0x204000).is_empty());
        store.release_descriptor_heap(0x100000, 0x110000);
        reference
            .entries
            .retain(|&h, _| !(0x100000..0x110000).contains(&h));
        reference.assert_matches(&store);
        HELD_SHARD_LOCKS.with(|held| assert_eq!(held.get(), 0));
        PEAK_SHARD_LOCKS.with(|peak| assert_eq!(peak.get(), 1));
    }

    #[test]
    fn poisoned_later_shard_never_exposes_a_partially_copied_table() {
        let store = Arc::new(CbvDescriptorStore::new());
        store.write_cbv_descriptor(0x100000, 0x200000, 256);
        let poison = Arc::clone(&store);
        let poison_index = CbvDescriptorStore::shard(0x13f000);
        assert!(poison_index > CbvDescriptorStore::shard(0x101000));
        assert!(
            std::thread::spawn(move || {
                let _held = poison.lock_shard(poison_index).unwrap();
                panic!("test shard poison");
            })
            .join()
            .is_err()
        );
        let destinations = [0x101000, 0x13f000];
        let sources = [0x100000, 0x100000];
        let plan = CbvDescriptorStore::prepare_copy(&destinations, &sources).unwrap();
        assert_eq!(store.apply_copy(plan, &mut CopyTiming::default()), None);
        assert!(!store.copy_cbv_descriptors(&destinations, &sources));
        assert!(store.get(&0x101000).is_none());
        assert!(store.get(&0x100000).is_none());
        assert!(store.gpuvas_in_range(0, u64::MAX).is_empty());
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn cross_shard_copy_matches_single_table_and_generation_order() {
        let store = CbvDescriptorStore::new();
        let mut reference = Reference::new();
        for i in 0..256 {
            let handle = 0x10_fc00 + i * 64;
            store.write_cbv_descriptor(handle, 0x20_0000 + (i % 17) * 256, 256);
            reference.write(handle, 0x20_0000 + (i % 17) * 256, 256);
        }
        for (source, destination, count) in [
            (0x10_fc00, 0x18_ffc0, 128), // Multiple source/destination shards.
            (0x10_ffc0, 0x11_0000, 128), // Overlap across a boundary.
            (0x11_0040, 0x10_ffc0, 64),  // Reverse direction.
        ] {
            let sources: Vec<_> = (0..count).map(|i| source + i * 64).collect();
            let destinations: Vec<_> = (0..count).map(|i| destination + i * 64).collect();
            assert_eq!(
                store.copy_cbv_descriptors(&destinations, &sources),
                reference.copy(&destinations, &sources)
            );
            reference.assert_matches(&store);
        }
        let sources = [0x10_fc00, 0x10_fc40, 0xdead_beef, 0x10_fc80];
        let destinations = [0x11_0000, 0x11_0000, 0x10_fc40, 0];
        assert_eq!(
            store.copy_cbv_descriptors(&destinations, &sources),
            reference.copy(&destinations, &sources)
        );
        reference.assert_matches(&store);
    }

    #[test]
    fn adjacent_boundary_and_untracked_copies_preserve_exact_state() {
        let store = CbvDescriptorStore::new();
        let mut reference = Reference::new();
        for handle in [0xfc0, 0x1000, 0x1040, 0x3ffc0, 0x40000] {
            store.write_cbv_descriptor(handle, handle * 256, 256);
            reference.write(handle, handle * 256, 256);
        }
        for (destinations, sources) in [
            (vec![0x1000, 0x1040], vec![0xfc0, 0x1000]),
            (vec![0x40000, 0x3ffc0], vec![0x3ffc0, 0x40000]),
            (vec![0xfe0, 0x1020], vec![0x4020, 0x3ffe0]),
            (vec![], vec![]),
            (vec![0xfc0], vec![]),
        ] {
            assert_eq!(
                store.copy_cbv_descriptors(&destinations, &sources),
                reference.copy(&destinations, &sources)
            );
            reference.assert_matches(&store);
        }
        let before = store.entries();
        let generation = store.next_descriptor_generation.load(Ordering::Relaxed);
        assert!(!store.copy_cbv_descriptors(&[0x1020], &[0x3ffe0]));
        assert!(!store.copy_cbv_descriptors(&vec![0x1000; 4097], &vec![0xfc0; 4097]));
        assert_eq!(before, store.entries());
        assert_eq!(
            generation,
            store.next_descriptor_generation.load(Ordering::Relaxed)
        );
        store.assert_indices_consistent();
    }

    #[test]
    fn writes_invalidation_release_and_gpuva_ranges_keep_both_indices() {
        let store = CbvDescriptorStore::new();
        let mut reference = Reference::new();
        for (handle, gpuva, size) in [
            (0, 0x1000, 256),
            (0xfc0, 0x1000, 256),
            (0x1000, 0x1000, 512),
            (0x40000, 0x1200, 256),
            (0x1000, 0x1100, 256),
            (0xfc0, 0, 256),
            (0x1000, 0x1000, 128),
            (0xfc0, 0x1000, 256),
            (0x1000, 0x1200, 256),
        ] {
            store.write_cbv_descriptor(handle, gpuva, size);
            reference.write(handle, gpuva, size);
            reference.assert_matches(&store);
        }
        assert_eq!(
            store.gpuvas_in_range(0x1000, 0x1200),
            BTreeSet::from([0x1000])
        );
        assert_eq!(
            store.gpuvas_in_range(0x1000, 0x1201),
            BTreeSet::from([0x1000, 0x1200])
        );
        store.invalidate_cbv_descriptor(0x1000);
        reference.entries.remove(&0x1000);
        reference.assert_matches(&store);
        store.release_descriptor_heap(0xfc0, 0x1000);
        reference
            .entries
            .retain(|&h, _| !(0xfc0..0x1000).contains(&h));
        reference.assert_matches(&store);
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn overlapping_copies_full_scans_and_main_lock_snapshots_hold_one_shard() {
        let store = Arc::new(CbvDescriptorStore::new());
        for i in 0..256 {
            store.write_cbv_descriptor(0xfc0 + i * 64, 0x10_0000 + i * 256, 256);
        }
        let tracker = Arc::new(Mutex::new(RenderCameraTracker::with_descriptor_store(
            1920,
            1080,
            75.0,
            Arc::clone(&store),
        )));
        let barrier = Arc::new(Barrier::new(9));
        let (tx, rx) = mpsc::channel();
        let mut threads = Vec::new();
        for worker in 0..8u64 {
            let store = Arc::clone(&store);
            let barrier = Arc::clone(&barrier);
            let tx = tx.clone();
            threads.push(std::thread::spawn(move || {
                PEAK_SHARD_LOCKS.with(|peak| peak.set(0));
                barrier.wait();
                for i in 0..500u64 {
                    let source = 0xfc0 + ((i + worker * 7) % 192) * 64;
                    let destination = 0xfc0 + ((i + worker * 13 + 64) % 192) * 64;
                    store.write_cbv_descriptor(source, 0x20_0000 + (i % 31) * 256, 256);
                    assert!(store.copy_cbv_descriptors(
                        &[destination, destination + 64],
                        &[source, source + 64]
                    ));
                }
                tx.send(()).unwrap();
                HELD_SHARD_LOCKS.with(|held| assert_eq!(held.get(), 0));
                PEAK_SHARD_LOCKS.with(|peak| assert_eq!(peak.get(), 1));
            }));
        }
        let snapshot_tracker = Arc::clone(&tracker);
        let snapshot_barrier = Arc::clone(&barrier);
        let snapshot_tx = tx.clone();
        let scan_store = Arc::clone(&store);
        threads.push(std::thread::spawn(move || {
            PEAK_SHARD_LOCKS.with(|peak| peak.set(0));
            snapshot_barrier.wait();
            for _ in 0..200 {
                // Same main-lock -> one-shard order as final Unmap.
                let tracker = snapshot_tracker.lock().unwrap();
                assert!(!scan_store.gpuvas_in_range(0, u64::MAX).is_empty());
                assert_eq!(tracker.snapshot().tracked_cbv_descriptors, 256);
            }
            snapshot_tx.send(()).unwrap();
            HELD_SHARD_LOCKS.with(|held| assert_eq!(held.get(), 0));
            PEAK_SHARD_LOCKS.with(|peak| assert_eq!(peak.get(), 1));
        }));
        drop(tx);
        let deadline = Instant::now() + Duration::from_secs(10);
        for _ in 0..9 {
            rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("shard/main lock order deadlocked or worker failed");
        }
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(store.len(), 256);
        store.assert_indices_consistent();
    }
}

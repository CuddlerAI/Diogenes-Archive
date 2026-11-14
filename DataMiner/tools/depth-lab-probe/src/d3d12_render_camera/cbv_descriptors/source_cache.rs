//! Experimental, bounded TLS cache of CBV metadata, NEVER mapped camera bytes.
//! Only source-only shards are eligible. Any copy destination permanently disables
//! that shard; every other mutation brackets its state with a version transition.
use super::{CbvDescriptor, CbvDescriptorStore, PreparedCopy};
use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

static NEXT_OWNER: AtomicU64 = AtomicU64::new(1);
pub(super) fn new_owner() -> u64 {
    // Exhaustion disables caching permanently rather than reusing an old TLS owner id.
    NEXT_OWNER
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
        .unwrap_or(0)
}

#[derive(Default)]
pub(super) struct Epoch {
    sequence: AtomicU64,
    disabled: AtomicBool,
}
impl Epoch {
    pub(super) fn disable(&self) {
        if !self.disabled.load(Ordering::Relaxed) {
            // Caller holds this shard's exclusive mutex; invalidate in-flight readers first.
            self.sequence.fetch_or(1, Ordering::AcqRel);
            self.disabled.store(true, Ordering::Release);
        }
    }

    pub(super) fn write(&self) -> Option<WriteEpoch<'_>> {
        if self.disabled.load(Ordering::Relaxed) {
            return None;
        }
        let version = self.sequence.load(Ordering::Relaxed);
        if !version.is_multiple_of(2) || version >= u64::MAX - 1 {
            self.disable();
            return None;
        }
        self.sequence.fetch_add(1, Ordering::AcqRel);
        Some(WriteEpoch {
            epoch: self,
            next: version + 2,
        })
    }

    fn snapshot(&self) -> Option<u64> {
        if self.disabled.load(Ordering::Acquire) {
            return None;
        }
        let version = self.sequence.load(Ordering::Acquire);
        version.is_multiple_of(2).then_some(version)
    }
}

// Must be dropped BEFORE releasing the corresponding mutex, including on unwind.
pub(super) struct WriteEpoch<'a> {
    epoch: &'a Epoch,
    next: u64,
}
impl Drop for WriteEpoch<'_> {
    fn drop(&mut self) {
        self.epoch.sequence.store(self.next, Ordering::Release);
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub(super) struct Stamp {
    shards: [usize; 4],
    versions: [u64; 4],
    count: usize,
}

#[derive(Default)]
struct Entry {
    owner: u64,
    sources: Vec<u64>,
    values: Vec<Option<CbvDescriptor>>,
    stamp: Stamp,
}
thread_local! {
    static CACHE: RefCell<[Entry; 8]> = RefCell::new(std::array::from_fn(|_| Entry::default()));
}

fn slot(sources: &[u64]) -> usize {
    let key = sources[0] ^ sources[sources.len() - 1].rotate_right(17) ^ sources.len() as u64;
    (key.wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 61) as usize
}

impl CbvDescriptorStore {
    pub(crate) fn lab_source_cache_policy(mut self, enabled: bool) -> Self {
        self.cache_enabled = enabled;
        self
    }
    pub(super) fn source_stamp(&self, plan: &PreparedCopy<'_>) -> Option<Stamp> {
        if !self.cache_enabled
            || self.cache_owner == 0
            || plan.sources.is_empty()
            || plan.sources.len() > 64
            || self.unusable.load(Ordering::Acquire)
        {
            return None;
        }
        let mut stamp = Stamp::default();
        for group in plan
            .source_order
            .chunk_by(|a, b| (a >> super::COPY_INDEX_BITS) == (b >> super::COPY_INDEX_BITS))
        {
            if stamp.count == 4 {
                return None;
            }
            let index = group[0] >> super::COPY_INDEX_BITS;
            let cell = &self.shards[index];
            let version = cell.cache_epoch.snapshot()?;
            if cell.mutex.is_poisoned() {
                self.unusable.store(true, Ordering::Release);
                return None;
            }
            stamp.shards[stamp.count] = index;
            stamp.versions[stamp.count] = version;
            stamp.count += 1;
        }
        Some(stamp)
    }

    pub(super) fn cached_sources(&self, plan: &mut PreparedCopy<'_>, stamp: Stamp) -> bool {
        let copied = CACHE.with(|cache| {
            let Ok(cache) = cache.try_borrow() else {
                return false;
            };
            let entry = &cache[slot(plan.sources)];
            if entry.owner != self.cache_owner
                || entry.stamp != stamp
                || entry.sources != plan.sources
                || entry.values.len() != plan.copied.len()
            {
                return false;
            }
            plan.copied.copy_from_slice(&entry.values);
            true
        });
        copied && self.source_stamp(plan) == Some(stamp) && !self.unusable.load(Ordering::Acquire)
    }

    pub(super) fn remember_sources(&self, plan: &PreparedCopy<'_>, before: Option<Stamp>) {
        let Some(before) = before else { return };
        if self.source_stamp(plan) != Some(before) || self.unusable.load(Ordering::Acquire) {
            return;
        }
        CACHE.with(|cache| {
            let Ok(mut cache) = cache.try_borrow_mut() else {
                return;
            };
            let entry = &mut cache[slot(plan.sources)];
            entry.sources.clear();
            entry.sources.extend_from_slice(plan.sources);
            entry.values.clear();
            entry.values.extend_from_slice(&plan.copied);
            entry.stamp = before;
            entry.owner = self.cache_owner;
        });
    }

    /// Standalone lab preflight, outside all performance timers. Exercise cache hits
    /// AND invalidation using ordinary store entry points, not a parallel data model.
    pub(crate) fn lab_validate_source_cache() -> Result<(), String> {
        use super::CopyTiming;
        const SOURCE: u64 = 0x101100;
        const SECOND: u64 = SOURCE + 32;
        const DEST: u64 = 0x102100;
        const OTHER: u64 = 0x104100;
        let copy = |store: &Self, dest: &[u64], src: &[u64], hit: bool| -> Result<(), String> {
            let mut timing = CopyTiming::default();
            let plan = Self::prepare_copy(dest, src).ok_or("cache validation plan")?;
            if store.apply_copy(plan, &mut timing).is_none()
                || timing.source_cache_hit != hit
                || !store.lab_validate()
            {
                return Err("source cache preflight copy/hit/index mismatch".into());
            }
            Ok(())
        };
        let check = |store: &Self, expected| -> Result<(), String> {
            if store.lab_value(DEST) != expected {
                return Err("source cache preflight payload mismatch".into());
            }
            Ok(())
        };
        let store = Self::new();
        store.write_cbv_descriptor(SOURCE, 0x200000, 256);
        store.write_cbv_descriptor(SECOND, 0x200100, 256);
        store.write_cbv_descriptor(OTHER, 0x300000, 256);
        copy(&store, &[DEST], &[SOURCE], false)?;
        let first = store.generation(DEST);
        copy(&store, &[DEST], &[SOURCE], true)?;
        if first == store.generation(DEST) {
            return Err("cached source reused destination generation".into());
        }
        for (gpuva, size) in [(0x200000, 512), (0x201000, 256)] {
            store.write_cbv_descriptor(SOURCE, gpuva, size);
            copy(&store, &[DEST], &[SOURCE], false)?;
            check(&store, Some((gpuva, size)))?;
            copy(&store, &[DEST], &[SOURCE], true)?;
        }
        store.invalidate_cbv_descriptor(SOURCE);
        copy(&store, &[DEST], &[SOURCE], false)?;
        check(&store, None)?;
        copy(&store, &[DEST], &[SOURCE], true)?;
        store.write_cbv_descriptor(SOURCE, 0x202000, 256);
        copy(&store, &[DEST], &[SOURCE], false)?;
        copy(&store, &[DEST], &[SOURCE], true)?;
        store.release_descriptor_heap(SOURCE, SOURCE + 1);
        copy(&store, &[DEST], &[SOURCE], false)?;
        check(&store, None)?;
        store.write_cbv_descriptor(SOURCE, 0x203000, 256);
        copy(&store, &[DEST, DEST], &[SOURCE, SECOND], false)?;
        copy(&store, &[DEST, DEST], &[SOURCE, SECOND], true)?;
        check(&store, Some((0x200100, 256)))?;
        let other = Self::new();
        other.write_cbv_descriptor(SOURCE, 0x400000, 256);
        copy(&other, &[DEST], &[SOURCE], false)?;
        check(&other, Some((0x400000, 256)))?;
        // A previously source-only shard becomes a copy target. Never use its cache again.
        copy(&store, &[SOURCE + 64], &[OTHER], false)?;
        copy(&store, &[DEST], &[SOURCE], false)?;
        copy(&store, &[DEST], &[SOURCE], false)?;
        check(&store, Some((0x203000, 256)))?;
        // Force the version boundary in a private, exclusively held fixture only.
        let wrap = Self::new();
        wrap.write_cbv_descriptor(SOURCE, 0x500000, 256);
        copy(&wrap, &[DEST], &[SOURCE], false)?;
        copy(&wrap, &[DEST], &[SOURCE], true)?;
        {
            let _held = wrap
                .lock_shard(Self::shard(SOURCE))
                .ok_or("wrap fixture lock")?;
            wrap.shards[Self::shard(SOURCE)]
                .cache_epoch
                .sequence
                .store(u64::MAX - 1, Ordering::Release);
        }
        wrap.write_cbv_descriptor(SOURCE, 0x500100, 256);
        copy(&wrap, &[DEST], &[SOURCE], false)?;
        copy(&wrap, &[DEST], &[SOURCE], false)?;
        check(&wrap, Some((0x500100, 256)))?;
        // Cross-thread writes are externally ordered, as required by D3D12 for the
        // SAME source. First copy must see each new identity; second must be a cache hit.
        let ordered = Self::new();
        let ready = std::sync::Barrier::new(2);
        let consumed = std::sync::Barrier::new(2);
        let mut failed = false;
        std::thread::scope(|scope| {
            scope.spawn(|| {
                for i in 0..1000u64 {
                    ordered.write_cbv_descriptor(
                        SOURCE,
                        0x700000 + (i % 64) * 256,
                        if i % 2 == 0 { 256 } else { 512 },
                    );
                    ready.wait();
                    consumed.wait();
                }
            });
            for i in 0..1000u64 {
                ready.wait();
                for expected_hit in [false, true] {
                    let mut timing = CopyTiming::default();
                    let result = Self::prepare_copy(&[DEST], &[SOURCE])
                        .and_then(|plan| ordered.apply_copy(plan, &mut timing));
                    failed |= result != Some(true) || timing.source_cache_hit != expected_hit;
                    failed |= ordered.lab_value(DEST)
                        != Some((
                            0x700000 + (i % 64) * 256,
                            if i % 2 == 0 { 256 } else { 512 },
                        ));
                }
                // Even a validation mismatch completes the rendezvous before reporting failure.
                consumed.wait();
            }
        });
        if failed || !ordered.lab_validate() {
            return Err("source cache cross-thread refresh mismatch".into());
        }
        // Poison AFTER a genuine cache hit: cached metadata must not bypass failure closure.
        let poison = Self::new();
        poison.write_cbv_descriptor(SOURCE, 0x600000, 256);
        copy(&poison, &[DEST], &[SOURCE], false)?;
        copy(&poison, &[DEST], &[SOURCE], true)?;
        let panicked = std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    let _held = poison.lock_shard(Self::shard(SOURCE)).unwrap();
                    panic!("intentional local source-cache preflight poison");
                })
                .join()
                .is_err()
        });
        let plan = Self::prepare_copy(&[DEST], &[SOURCE]).ok_or("poison fixture plan")?;
        if !panicked
            || poison
                .apply_copy(plan, &mut CopyTiming::default())
                .is_some()
            || poison.get(&DEST).is_some()
        {
            return Err("source cache bypassed poisoned source".into());
        }
        Ok(())
    }
}

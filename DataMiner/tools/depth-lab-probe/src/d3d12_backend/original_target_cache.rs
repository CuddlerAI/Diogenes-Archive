//! Positive-only forwarding cache owned by one process-lived backend.
//!
//! Publication follows successful hook installation and registry insertion, under the registry
//! mutex. Entries never change or disappear. A miss (including an in-progress publication or a
//! full probe bucket) must use that same mutex, so installation races still wait for the original.
use super::HookKey;
use std::sync::OnceLock;

const CAPACITY: usize = 256;
const PROBES: usize = 8;

pub(super) struct OriginalTargetCache {
    entries: [OnceLock<(HookKey, usize)>; CAPACITY],
}

impl Default for OriginalTargetCache {
    fn default() -> Self {
        Self {
            entries: std::array::from_fn(|_| OnceLock::new()),
        }
    }
}

impl OriginalTargetCache {
    fn bucket(key: HookKey) -> usize {
        ((key.slot >> 3).wrapping_mul(0x9e37_79b1).rotate_right(16) ^ key.method as usize)
            % CAPACITY
    }

    pub(super) fn get(&self, key: HookKey) -> Option<usize> {
        let bucket = Self::bucket(key);
        for offset in 0..PROBES {
            let &(stored_key, target) = self.entries[(bucket + offset) % CAPACITY].get()?;
            if stored_key == key {
                return Some(target);
            }
        }
        None
    }

    /// Only the registry's serialized successful-install path may publish. Saturation is a
    /// performance miss, never an observation drop or permission to use another slot's target.
    pub(super) fn publish(&self, key: HookKey, target: usize) {
        let bucket = Self::bucket(key);
        for offset in 0..PROBES {
            let entry = self.entries[(bucket + offset) % CAPACITY].get_or_init(|| (key, target));
            if entry.0 == key {
                debug_assert_eq!(entry.1, target, "original targets are immutable");
                return;
            }
        }
    }

    pub(super) fn len(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| entry.get().is_some())
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::d3d12_backend::{D3d12ObservationBackend, HookMethod};
    use std::collections::BTreeMap;
    use std::hint::black_box;
    use std::sync::{Arc, Barrier, Mutex, mpsc};
    use std::time::{Duration, Instant};

    fn key(slot: usize) -> HookKey {
        HookKey {
            slot,
            method: HookMethod::DrawInstanced,
        }
    }

    #[test]
    fn exact_slot_and_method_and_backend_are_required() {
        let cache = OriginalTargetCache::default();
        cache.publish(key(0x1000), 11);
        assert_eq!(cache.get(key(0x1000)), Some(11));
        assert_eq!(cache.get(key(0x1008)), None);
        assert_eq!(
            cache.get(HookKey {
                method: HookMethod::DrawIndexedInstanced,
                ..key(0x1000)
            }),
            None
        );
        assert_eq!(OriginalTargetCache::default().get(key(0x1000)), None);
        cache.publish(key(0x1000), 11);
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn collisions_and_saturation_never_return_another_original() {
        let cache = OriginalTargetCache::default();
        let keys: Vec<_> = (1..)
            .map(|n| key(n * 8))
            .filter(|candidate| OriginalTargetCache::bucket(*candidate) == 0)
            .take(PROBES + 1)
            .collect();
        for (index, candidate) in keys.iter().enumerate() {
            cache.publish(*candidate, index + 1);
        }
        for (index, candidate) in keys.iter().enumerate() {
            assert_eq!(cache.get(*candidate), (index < PROBES).then_some(index + 1));
        }
        assert_eq!(cache.len(), PROBES);
    }

    #[test]
    fn concurrent_publication_never_exposes_a_mixed_key_and_target() {
        let cache = Arc::new(OriginalTargetCache::default());
        let start = Arc::new(Barrier::new(5));
        std::thread::scope(|scope| {
            for _ in 0..4 {
                let cache = Arc::clone(&cache);
                let start = Arc::clone(&start);
                scope.spawn(move || {
                    start.wait();
                    for _ in 0..1000 {
                        for slot in 1..128 {
                            if let Some(target) = cache.get(key(slot * 8)) {
                                assert_eq!(target, slot + 1000);
                            }
                        }
                    }
                });
            }
            start.wait();
            for slot in 1..128 {
                cache.publish(key(slot * 8), slot + 1000);
            }
        });
        assert!(cache.len() > 0);
    }

    // A fake one-slot COM layout exercises the actual backend lookup without touching a GPU,
    // a real vtable, the process-wide ACTIVE_BACKEND, or invoking the synthetic target.
    fn lookup(backend: &D3d12ObservationBackend, vtable: &[usize; 1]) -> Option<usize> {
        let mut object = vtable.as_ptr();
        backend.original_target(
            (&mut object as *mut *const usize).cast(),
            0,
            HookMethod::DrawInstanced,
        )
    }

    #[test]
    fn cached_lookup_does_not_wait_for_unrelated_installation() {
        let backend = D3d12ObservationBackend::new();
        let table = Arc::new([123]);
        backend
            .original_targets
            .publish(key(table.as_ptr() as usize), 123);
        let held = backend.hooks.lock().unwrap();
        let worker_backend = Arc::clone(&backend);
        let (tx, rx) = mpsc::channel();
        let worker = std::thread::spawn(move || tx.send(lookup(&worker_backend, &table)).unwrap());
        let result = rx.recv_timeout(Duration::from_secs(2));
        drop(held);
        worker.join().unwrap();
        assert_eq!(result.unwrap(), Some(123));
    }

    #[test]
    fn cache_miss_waits_for_registry_publication_and_is_not_negative_cached() {
        let backend = D3d12ObservationBackend::new();
        let table = Arc::new([123]);
        let candidate = key(table.as_ptr() as usize);
        assert_eq!(lookup(&backend, &table), None);
        let mut held = backend.hooks.lock().unwrap();
        let worker_backend = Arc::clone(&backend);
        let worker_table = Arc::clone(&table);
        let (started_tx, started_rx) = mpsc::channel();
        let (tx, rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            tx.send(lookup(&worker_backend, &worker_table)).unwrap();
        });
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let before_publication = rx.recv_timeout(Duration::from_millis(50));
        held.originals.insert(candidate, 123);
        backend.original_targets.publish(candidate, 123);
        drop(held);
        let result = rx.recv_timeout(Duration::from_secs(2));
        worker.join().unwrap();
        assert!(before_publication.is_err());
        assert_eq!(result.unwrap(), Some(123));
        assert_eq!(lookup(&backend, &table), Some(123));
    }

    #[test]
    fn uncached_registry_entry_still_forwards_and_poisoned_miss_rejects() {
        let backend = D3d12ObservationBackend::new();
        let table = [321];
        backend
            .hooks
            .lock()
            .unwrap()
            .originals
            .insert(key(table.as_ptr() as usize), 321);
        assert_eq!(lookup(&backend, &table), Some(321));
        backend
            .original_targets
            .publish(key(table.as_ptr() as usize), 321);
        let worker_backend = Arc::clone(&backend);
        assert!(
            std::thread::spawn(move || {
                let _held = worker_backend.hooks.lock().unwrap();
                panic!("test poison");
            })
            .join()
            .is_err()
        );
        assert_eq!(lookup(&backend, &table), None);
    }

    #[test]
    #[ignore = "manual synthetic lookup benchmark; no game or GPU, run in release mode"]
    fn original_target_lookup_benchmark() {
        for threads in [1, 8] {
            for cached in [false, true] {
                let cache = OriginalTargetCache::default();
                let registry = Mutex::new(BTreeMap::new());
                for slot in 1..65 {
                    cache.publish(key(slot * 8), slot + 1000);
                    registry.lock().unwrap().insert(key(slot * 8), slot + 1000);
                }
                let start = Barrier::new(threads + 1);
                let iterations = 500_000;
                let elapsed = std::thread::scope(|scope| {
                    let workers: Vec<_> = (0..threads)
                        .map(|_| {
                            scope.spawn(|| {
                                start.wait();
                                let began = Instant::now();
                                for n in 0..iterations {
                                    let candidate = black_box(key((n % 64 + 1) * 8));
                                    let value = if cached {
                                        cache.get(candidate).or_else(|| {
                                            registry.lock().unwrap().get(&candidate).copied()
                                        })
                                    } else {
                                        registry.lock().unwrap().get(&candidate).copied()
                                    };
                                    assert_eq!(black_box(value), Some(n % 64 + 1001));
                                }
                                began.elapsed()
                            })
                        })
                        .collect();
                    start.wait();
                    workers
                        .into_iter()
                        .map(|worker| worker.join().unwrap())
                        .max()
                        .unwrap()
                });
                println!(
                    "lookup_benchmark threads={threads} cached={cached} operations={} cached_entries={} elapsed_ms={:.3}",
                    threads * iterations,
                    cache.len(),
                    elapsed.as_secs_f64() * 1000.0
                );
            }
        }
    }
}

//! Per-Execute diagnostics. Stack-local accumulation; publish only after releasing camera locks.
//! All times are wall elapsed. Descriptor times are SUBSETS of validation; prefetch_query is a
//! subset of unlocked prefetch. Lock-held sums the locked stages, excluding prefetch. Neither
//! nested stages nor concurrent calls are additive CPU accounting.
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::hook_timing::{TimingCounter, TimingSnapshot};

#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
pub struct PrepareSample {
    pub lists: u64,
    pub requests: u64,
    pub live_requests: u64,
    pub validation_rejected: u64,
    pub read_attempts: u64,
    pub read_rejected: u64,
    pub reused_snapshots: u64,
    pub read_bytes: u64,
    pub reused_bytes: u64,
    pub write_combine_reads: u64,
    pub no_cache_reads: u64,
    pub unflagged_reads: u64,
    pub byte_loads: u64,
    pub wide_loads: u64,
    pub memory_query_calls: u64,
    pub memory_query_rejected: u64,
    pub query_cache_hits: u64,
    pub query_cache_ineligible_reads: u64,
    pub query_cache_inserts: u64,
    pub query_cache_full_reads: u64,
    // Prefetch queries run without the tracker guard; they need not become actual reads.
    // memory_query_* remains the locked, on-demand path for v3-compatible read accounting.
    pub query_prefetch_requests: u64,
    pub query_prefetch_calls: u64,
    pub query_prefetch_rejected: u64,
    pub query_prefetch_cache_hits: u64,
    pub query_prefetch_inserts: u64,
    pub query_prefetch_full: u64,
    pub query_prefetch_read_hits: u64,
    // Observer-owned mappings read through the fallible path: no VirtualQuery at all.
    pub owned_fallible_reads: u64,
    pub owned_fallible_rejected: u64,
    pub total_ns: u64,
    pub outer_lock_wait_ns: u64,
    pub outer_lock_held_ns: u64,
    pub mapped_ranges_ns: u64,
    pub validation_ns: u64,
    pub descriptor_wait_ns: u64,
    pub descriptor_lookup_ns: u64,
    pub memory_query_ns: u64,
    pub query_prefetch_ns: u64,
    pub query_prefetch_query_ns: u64,
    pub allocation_ns: u64,
    pub memory_read_ns: u64,
    pub reuse_clone_ns: u64,
    pub residual_ns: u64,
}

pub(crate) fn nanos(duration: Duration) -> u64 {
    duration.as_nanos().min(u128::from(u64::MAX)) as u64
}

pub(crate) fn elapsed(start: Option<Instant>) -> u64 {
    start.map_or(0, |start| nanos(start.elapsed()))
}

impl PrepareSample {
    pub(crate) fn include_reads(&mut self, reads: Self) {
        self.read_bytes += reads.read_bytes;
        self.write_combine_reads += reads.write_combine_reads;
        self.no_cache_reads += reads.no_cache_reads;
        self.unflagged_reads += reads.unflagged_reads;
        self.byte_loads += reads.byte_loads;
        self.wide_loads += reads.wide_loads;
        self.memory_query_calls += reads.memory_query_calls;
        self.memory_query_rejected += reads.memory_query_rejected;
        self.query_cache_hits += reads.query_cache_hits;
        self.query_cache_ineligible_reads += reads.query_cache_ineligible_reads;
        self.query_cache_inserts += reads.query_cache_inserts;
        self.query_cache_full_reads += reads.query_cache_full_reads;
        self.query_prefetch_read_hits += reads.query_prefetch_read_hits;
        self.owned_fallible_reads += reads.owned_fallible_reads;
        self.owned_fallible_rejected += reads.owned_fallible_rejected;
        self.memory_query_ns += reads.memory_query_ns;
        self.allocation_ns += reads.allocation_ns;
        self.memory_read_ns += reads.memory_read_ns;
    }

    pub(crate) fn finish(&mut self, duration: Duration) {
        self.total_ns = nanos(duration);
        // Non-overlapping measured children only. Descriptor wait/lookup and lock-held are
        // deliberately excluded here. Residual retains container work and instrumentation.
        let children = [
            self.outer_lock_wait_ns,
            self.mapped_ranges_ns,
            self.validation_ns,
            self.memory_query_ns,
            self.query_prefetch_ns,
            self.allocation_ns,
            self.memory_read_ns,
            self.reuse_clone_ns,
        ]
        .into_iter()
        .fold(0u64, u64::saturating_add);
        self.residual_ns = self.total_ns.saturating_sub(children);
    }

    fn times(self) -> [(&'static str, u64); 14] {
        [
            ("total", self.total_ns),
            ("outer_lock_wait", self.outer_lock_wait_ns),
            ("outer_lock_held", self.outer_lock_held_ns),
            ("mapped_ranges", self.mapped_ranges_ns),
            ("validation", self.validation_ns),
            ("descriptor_wait", self.descriptor_wait_ns),
            ("descriptor_lookup", self.descriptor_lookup_ns),
            ("memory_query", self.memory_query_ns),
            ("query_prefetch", self.query_prefetch_ns),
            ("query_prefetch_query", self.query_prefetch_query_ns),
            ("allocation", self.allocation_ns),
            ("memory_read", self.memory_read_ns),
            ("reuse_clone", self.reuse_clone_ns),
            ("residual", self.residual_ns),
        ]
    }

    fn counts(self) -> [(&'static str, u64); 29] {
        [
            ("lists", self.lists),
            ("requests", self.requests),
            ("live_requests", self.live_requests),
            ("validation_rejected", self.validation_rejected),
            ("read_attempts", self.read_attempts),
            ("read_rejected", self.read_rejected),
            ("reused_snapshots", self.reused_snapshots),
            ("read_bytes", self.read_bytes),
            ("reused_bytes", self.reused_bytes),
            ("write_combine_reads", self.write_combine_reads),
            ("no_cache_reads", self.no_cache_reads),
            ("unflagged_reads", self.unflagged_reads),
            ("byte_loads", self.byte_loads),
            ("wide_loads", self.wide_loads),
            ("memory_query_calls", self.memory_query_calls),
            ("memory_query_rejected", self.memory_query_rejected),
            ("query_cache_hits", self.query_cache_hits),
            (
                "query_cache_ineligible_reads",
                self.query_cache_ineligible_reads,
            ),
            ("query_cache_inserts", self.query_cache_inserts),
            ("query_cache_full_reads", self.query_cache_full_reads),
            ("query_prefetch_requests", self.query_prefetch_requests),
            ("query_prefetch_calls", self.query_prefetch_calls),
            ("query_prefetch_rejected", self.query_prefetch_rejected),
            ("query_prefetch_cache_hits", self.query_prefetch_cache_hits),
            ("query_prefetch_inserts", self.query_prefetch_inserts),
            ("query_prefetch_full", self.query_prefetch_full),
            ("query_prefetch_read_hits", self.query_prefetch_read_hits),
            ("owned_fallible_reads", self.owned_fallible_reads),
            ("owned_fallible_rejected", self.owned_fallible_rejected),
        ]
    }
}

#[derive(Default)]
pub(crate) struct PrepareCounters {
    times: [TimingCounter; 14],
    counts: [AtomicU64; 29],
}

impl PrepareCounters {
    pub(crate) fn record(&self, sample: PrepareSample) {
        for (counter, (_, ns)) in self.times.iter().zip(sample.times()) {
            counter.record_duration(Duration::from_nanos(ns));
        }
        for (counter, (_, count)) in self.counts.iter().zip(sample.counts()) {
            counter.fetch_add(count, Ordering::Relaxed);
        }
    }

    pub(crate) fn append_snapshot(&self, out: &mut BTreeMap<String, TimingSnapshot>) {
        for (counter, (name, _)) in self.times.iter().zip(PrepareSample::default().times()) {
            out.insert(format!("execute_prepare_detail_{name}"), counter.snapshot());
        }
        for (counter, (name, _)) in self.counts.iter().zip(PrepareSample::default().counts()) {
            out.insert(
                format!("execute_prepare_detail_{name}"),
                TimingSnapshot {
                    calls: counter.load(Ordering::Relaxed),
                    ..Default::default()
                },
            );
        }
    }
}

#[test]
fn residual_excludes_nested_timings_and_counters_publish_explicit_units() {
    let mut sample = PrepareSample {
        lists: 2,
        requests: 3,
        reused_snapshots: 1,
        outer_lock_wait_ns: 10_000,
        outer_lock_held_ns: 80_000,
        mapped_ranges_ns: 5_000,
        validation_ns: 20_000,
        descriptor_wait_ns: 15_000,
        descriptor_lookup_ns: 1_000,
        memory_query_ns: 5_000,
        allocation_ns: 5_000,
        memory_read_ns: 20_000,
        reuse_clone_ns: 5_000,
        ..Default::default()
    };
    sample.finish(Duration::from_micros(100));
    assert_eq!(sample.residual_ns, 30_000);
    let counters = PrepareCounters::default();
    counters.record(sample);
    let mut snapshot = BTreeMap::new();
    counters.append_snapshot(&mut snapshot);
    assert_eq!(snapshot["execute_prepare_detail_requests"].calls, 3);
    assert_eq!(snapshot["execute_prepare_detail_memory_read"].total_us, 20);
    assert_eq!(snapshot["execute_prepare_detail_total"].calls, 1);
}

#[test]
fn query_counts_are_included_and_published_without_changing_timing_units() {
    let mut sample = PrepareSample::default();
    sample.include_reads(PrepareSample {
        memory_query_calls: 7,
        memory_query_rejected: 1,
        query_cache_hits: 9,
        query_cache_ineligible_reads: 2,
        query_cache_inserts: 4,
        query_cache_full_reads: 1,
        ..Default::default()
    });
    let counters = PrepareCounters::default();
    counters.record(sample);
    let mut snapshot = BTreeMap::new();
    counters.append_snapshot(&mut snapshot);
    for (name, count) in [
        ("memory_query_calls", 7),
        ("memory_query_rejected", 1),
        ("query_cache_hits", 9),
        ("query_cache_ineligible_reads", 2),
        ("query_cache_inserts", 4),
        ("query_cache_full_reads", 1),
    ] {
        let value = &snapshot[&format!("execute_prepare_detail_{name}")];
        assert_eq!(value.calls, count);
        assert_eq!(value.total_us, 0);
    }
}

//! Page queries only, outside the tracker mutex and inside one pre-Execute invocation.
use super::{D3d12ObservationBackend, mapped_snapshot};
use crate::execute_prepare::{PrepareSample, elapsed};

pub(super) fn prefetch(
    backend: &D3d12ObservationBackend,
    lists: &[usize],
    measure: bool,
    sample: &mut PrepareSample,
) -> Option<mapped_snapshot::QueryCache> {
    let wait = measure.then(std::time::Instant::now);
    let locked = backend.lock_render_camera_measured(
        measure,
        &backend.camera_execute_prepare_lock_immediate,
        &backend.camera_execute_prepare_lock_contended,
    );
    sample.outer_lock_wait_ns += elapsed(wait);
    let Ok(tracker) = locked else {
        backend.block("D3D12 render-camera tracker poisoned planning page queries");
        return None;
    };
    let held = measure.then(std::time::Instant::now);
    let held_scope =
        crate::hook_timing::slow_scope(crate::perf::SlowHookKind::ExecutePrepareLocked);
    let plan = measure.then(std::time::Instant::now);
    let mut hints = tracker.exact_copy_query_hints(lists, mapped_snapshot::QUERY_CACHE_CAPACITY);
    sample.mapped_ranges_ns += elapsed(plan);
    drop(tracker);
    sample.outer_lock_held_ns += elapsed(held);
    drop(held_scope);

    let mut cache = mapped_snapshot::QueryCache::default();
    let query = measure.then(std::time::Instant::now);
    let query_scope =
        crate::hook_timing::slow_scope(crate::perf::SlowHookKind::ExecuteQueryPrefetch);
    // VirtualQuery evidence begins at the requested page, not AllocationBase. Visit the
    // lowest hinted address first per FULL owner so later hints can reuse forward coverage.
    // Sort only this bounded metadata plan, outside the tracker. Actual request/read/receipt
    // order is untouched. No extra allocation, query, Map, or backwards range widening.
    sort_hints(&mut hints);
    #[cfg(all(feature = "cp2077-diagnostics", debug_assertions))]
    if measure {
        cache.begin_prefetch_diagnostics(hints.len());
    }
    // No backend mutex or D3D call here. Existing observer_upload_mappings owns the COM
    // resources and nested Maps for the entire ACTIVE_BACKEND lifetime (no retirement path).
    // This only queries metadata; it never dereferences the hinted mapped address.
    // The caller reacquires the tracker, rebuilds current mappings/identities, validates every
    // list/copy/descriptor generation, then reads fresh bytes before forwarding Execute.
    for hint in hints {
        cache.prefetch(hint, measure.then_some(&mut *sample));
    }
    sample.query_prefetch_ns += elapsed(query);
    drop(query_scope);
    #[cfg(all(feature = "cp2077-diagnostics", debug_assertions))]
    cache.finish_prefetch_diagnostics(sample);
    Some(cache)
}

// Shared with the explicitly gated local harness; never a second sorting implementation.
pub(super) fn sort_hints(hints: &mut [crate::d3d12_render_camera::ObserverMappedQuery]) {
    hints.sort_unstable_by_key(|hint| {
        let owner = hint.owner;
        (
            owner.resource_raw,
            owner.source_tag,
            owner.content_generation,
            owner.base,
            owner.bytes,
            hint.address,
            hint.bytes,
        )
    });
}

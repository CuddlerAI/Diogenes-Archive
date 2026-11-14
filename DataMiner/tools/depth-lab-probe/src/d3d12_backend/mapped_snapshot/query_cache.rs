//! Query evidence for ONE prepare/Execute, never cached camera bytes or a resource lease.
use windows::Win32::System::Memory::{MEMORY_BASIC_INFORMATION, PAGE_PROTECTION_FLAGS};

use super::{PrepareSample, elapsed};
use crate::d3d12_render_camera::ObserverMappedIdentity;

pub(in crate::d3d12_backend) const CAPACITY: usize = 32;

#[derive(Clone, Copy)]
struct Region {
    owner: ObserverMappedIdentity,
    begin: usize,
    end: usize,
    protection: PAGE_PROTECTION_FLAGS,
    prefetched: bool,
}

pub(in crate::d3d12_backend) struct QueryCache {
    regions: [Option<Region>; CAPACITY],
    #[cfg(all(feature = "cp2077-diagnostics", debug_assertions))]
    diagnostics: Option<crate::cp2077_camera_diagnostics::page_query::Batch>,
}

impl Default for QueryCache {
    fn default() -> Self {
        Self {
            regions: [None; CAPACITY],
            #[cfg(all(feature = "cp2077-diagnostics", debug_assertions))]
            diagnostics: None,
        }
    }
}

impl QueryCache {
    #[cfg(all(feature = "cp2077-diagnostics", debug_assertions))]
    pub(in crate::d3d12_backend) fn begin_prefetch_diagnostics(&mut self, hints: usize) {
        self.diagnostics = crate::cp2077_camera_diagnostics::page_query::Batch::begin(hints);
    }

    #[cfg(all(feature = "cp2077-diagnostics", debug_assertions))]
    pub(in crate::d3d12_backend) fn finish_prefetch_diagnostics(&mut self, sample: &PrepareSample) {
        if let Some(batch) = self.diagnostics.take() {
            batch.submit(sample);
        }
    }

    /// Metadata only; never dereferences mapped bytes or calls Map/AddRef. The backend keeps
    /// its existing observer COM reference and nested Map alive throughout this Execute.
    /// A later read must supply a fresh tracker-validated identity, not the hint identity.
    pub(in crate::d3d12_backend) fn prefetch(
        &mut self,
        hint: crate::d3d12_render_camera::ObserverMappedQuery,
        metrics: Option<&mut PrepareSample>,
    ) {
        let mut sample = PrepareSample::default();
        let measure = metrics.is_some();
        let _ = self.resolve_with_source(
            Some(hint.owner),
            hint.address,
            hint.bytes,
            measure.then_some(&mut sample),
            super::super::query_readable_mapped_range,
            true,
        );
        if let Some(metrics) = metrics {
            metrics.query_prefetch_requests += 1;
            metrics.query_prefetch_calls += sample.memory_query_calls;
            metrics.query_prefetch_rejected += sample.memory_query_rejected;
            metrics.query_prefetch_cache_hits += sample.query_cache_hits;
            metrics.query_prefetch_inserts += sample.query_cache_inserts;
            metrics.query_prefetch_full += sample.query_cache_full_reads;
            metrics.query_prefetch_query_ns += sample.memory_query_ns;
        }
    }

    /// # Safety
    /// The read safety contract still applies. Every supplied owner must have a retained COM
    /// reference and nested Map for the entire cache lifetime, which must stay inside ONE
    /// prepare/Execute. The caller holds the tracker guard and supplies a fresh owner identity
    /// after any unlocked prefetch. No external protection mutation may race mapped reads.
    /// None retains the original per-read query; it does not create or pin any mapping.
    pub(in crate::d3d12_backend) unsafe fn read(
        &mut self,
        owner: Option<ObserverMappedIdentity>,
        address: usize,
        bytes: usize,
        wide: bool,
        mut metrics: Option<&mut PrepareSample>,
    ) -> Option<Vec<u8>> {
        let protection = self.resolve(
            owner,
            address,
            bytes,
            metrics.as_deref_mut(),
            super::super::query_readable_mapped_range,
        )?;
        unsafe { super::read_validated(address, bytes, wide, protection, metrics) }
    }

    fn resolve(
        &mut self,
        owner: Option<ObserverMappedIdentity>,
        address: usize,
        bytes: usize,
        metrics: Option<&mut PrepareSample>,
        query: impl FnOnce(usize, usize) -> Option<MEMORY_BASIC_INFORMATION>,
    ) -> Option<PAGE_PROTECTION_FLAGS> {
        self.resolve_with_source(owner, address, bytes, metrics, query, false)
    }

    fn resolve_with_source(
        &mut self,
        owner: Option<ObserverMappedIdentity>,
        address: usize,
        bytes: usize,
        mut metrics: Option<&mut PrepareSample>,
        query: impl FnOnce(usize, usize) -> Option<MEMORY_BASIC_INFORMATION>,
        prefetched: bool,
    ) -> Option<PAGE_PROTECTION_FLAGS> {
        let request_end = address.checked_add(bytes)?;
        let owner_end = if let Some(owner) = owner {
            let end = owner.base.checked_add(usize::try_from(owner.bytes).ok()?)?;
            if address < owner.base || request_end > end {
                return None;
            }
            if let Some(region) = self.regions.iter().flatten().find(|region| {
                region.owner == owner && address >= region.begin && request_end <= region.end
            }) {
                if let Some(metrics) = metrics.as_deref_mut() {
                    metrics.query_cache_hits += 1;
                    metrics.query_prefetch_read_hits += u64::from(region.prefetched);
                }
                return Some(region.protection);
            }
            Some(end)
        } else {
            if let Some(metrics) = metrics.as_deref_mut() {
                metrics.query_cache_ineligible_reads += 1;
            }
            None
        };

        // Only the real query is timed here. Lookup/insert time remains in prepare residual;
        // a cache hit must not appear as a zero-duration VirtualQuery call.
        #[cfg(all(feature = "cp2077-diagnostics", debug_assertions))]
        let trace = self
            .diagnostics
            .as_ref()
            .map(|_| crate::cp2077_camera_diagnostics::page_query::QuerySpan::begin());
        let start = metrics.is_some().then(std::time::Instant::now);
        let info = query(address, bytes);
        let query_ns = elapsed(start);
        // Added clocks and serialization preparation remain OUTSIDE the existing query timer.
        #[cfg(all(feature = "cp2077-diagnostics", debug_assertions))]
        if let (Some(batch), Some(trace)) = (self.diagnostics.as_mut(), trace) {
            batch.record(trace, owner, address, bytes, query_ns, info.as_ref());
        }
        let info = info.filter(|info| {
            let begin = info.BaseAddress as usize;
            begin <= address
                && begin
                    .checked_add(info.RegionSize)
                    .is_some_and(|end| request_end <= end)
        });
        if let Some(metrics) = metrics.as_deref_mut() {
            metrics.memory_query_ns += query_ns;
            metrics.memory_query_calls += 1;
            metrics.memory_query_rejected += u64::from(info.is_none());
        }
        let info = info?;
        if let (Some(owner), Some(owner_end)) = (owner, owner_end) {
            // Never use AllocationBase to widen the queried region. Restrict the evidence
            // further to this exact resource, even if the VM region contains adjacent buffers.
            let begin = (info.BaseAddress as usize).max(owner.base);
            let end = (info.BaseAddress as usize)
                .checked_add(info.RegionSize)?
                .min(owner_end);
            if address < begin || request_end > end {
                return None;
            }
            if let Some(slot) = self.regions.iter_mut().find(|slot| slot.is_none()) {
                *slot = Some(Region {
                    owner,
                    begin,
                    end,
                    protection: info.Protect,
                    prefetched,
                });
                if let Some(metrics) = metrics.as_deref_mut() {
                    metrics.query_cache_inserts += 1;
                }
            } else if let Some(metrics) = metrics {
                metrics.query_cache_full_reads += 1;
            }
        }
        Some(info.Protect)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::Memory::PAGE_READWRITE;

    fn owner() -> ObserverMappedIdentity {
        ObserverMappedIdentity {
            resource_raw: 7,
            source_tag: 11,
            content_generation: 2,
            base: 0x1000,
            bytes: 0x2000,
        }
    }

    fn region(begin: usize, bytes: usize) -> MEMORY_BASIC_INFORMATION {
        MEMORY_BASIC_INFORMATION {
            BaseAddress: begin as *mut _,
            RegionSize: bytes,
            Protect: PAGE_READWRITE,
            ..Default::default()
        }
    }

    #[test]
    fn same_owner_reuses_region_not_allocation_base_and_next_execute_queries_again() {
        let mut cache = QueryCache::default();
        let mut metrics = PrepareSample::default();
        let query = |_, _| Some(region(0x2000, 0x1000));
        for address in [0x2080, 0x2800, 0x2000] {
            assert!(
                cache
                    .resolve(Some(owner()), address, 256, Some(&mut metrics), query)
                    .is_some()
            );
        }
        assert_eq!(
            (metrics.memory_query_calls, metrics.query_cache_hits),
            (1, 2)
        );
        // A later request BEFORE BaseAddress is not covered, even in the same resource.
        assert!(
            cache
                .resolve(Some(owner()), 0x1000, 256, Some(&mut metrics), |_, _| None)
                .is_none()
        );
        let mut next = QueryCache::default();
        assert!(
            next.resolve(Some(owner()), 0x2080, 256, Some(&mut metrics), |_, _| None)
                .is_none()
        );
        assert_eq!(
            (metrics.memory_query_calls, metrics.memory_query_rejected),
            (3, 2)
        );
    }

    #[test]
    fn identities_never_borrow_other_resources_generations_or_mapping_extents() {
        let mut cache = QueryCache::default();
        assert!(
            cache
                .resolve(Some(owner()), 0x1800, 256, None, |_, _| Some(region(
                    0x1000, 0x2000
                )))
                .is_some()
        );
        for changed in [
            ObserverMappedIdentity {
                resource_raw: 8,
                ..owner()
            },
            ObserverMappedIdentity {
                source_tag: 12,
                ..owner()
            },
            ObserverMappedIdentity {
                content_generation: 3,
                ..owner()
            },
            ObserverMappedIdentity {
                base: 0x1100,
                ..owner()
            },
            ObserverMappedIdentity {
                bytes: 0x1800,
                ..owner()
            },
        ] {
            assert!(
                cache
                    .resolve(Some(changed), 0x1800, 256, None, |_, _| None)
                    .is_none()
            );
        }
    }

    #[test]
    fn borrowed_mappings_and_failed_queries_are_never_cached() {
        let mut cache = QueryCache::default();
        let mut metrics = PrepareSample::default();
        for _ in 0..3 {
            assert!(
                cache
                    .resolve(None, 0x1000, 256, Some(&mut metrics), |_, _| Some(region(
                        0x1000, 0x1000
                    )))
                    .is_some()
            );
        }
        for _ in 0..2 {
            assert!(
                cache
                    .resolve(Some(owner()), 0x1000, 256, Some(&mut metrics), |_, _| None)
                    .is_none()
            );
        }
        assert_eq!(metrics.memory_query_calls, 5);
        assert_eq!(metrics.query_cache_ineligible_reads, 3);
        assert_eq!(metrics.memory_query_rejected, 2);
        assert_eq!(metrics.query_cache_hits, 0);
        assert_eq!(metrics.query_cache_inserts, 0);
    }

    #[test]
    fn range_checks_remain_exact_on_hits_and_overflow_never_queries() {
        let mut cache = QueryCache::default();
        let mut metrics = PrepareSample::default();
        assert!(
            cache
                .resolve(Some(owner()), 0x1f00, 256, Some(&mut metrics), |_, _| Some(
                    region(0x1000, 0x1000)
                ))
                .is_some()
        );
        assert!(
            cache
                .resolve(Some(owner()), 0x1fff, 2, Some(&mut metrics), |_, _| None)
                .is_none()
        );
        assert!(
            cache
                .resolve(
                    Some(owner()),
                    0x0fff,
                    256,
                    Some(&mut metrics),
                    |_, _| panic!("outside resource")
                )
                .is_none()
        );
        assert!(
            cache
                .resolve(Some(owner()), 0x2fff, 2, Some(&mut metrics), |_, _| panic!(
                    "outside resource"
                ))
                .is_none()
        );
        assert!(
            cache
                .resolve(
                    Some(owner()),
                    usize::MAX - 3,
                    8,
                    Some(&mut metrics),
                    |_, _| panic!("overflow")
                )
                .is_none()
        );
        assert_eq!(metrics.memory_query_calls, 2);
        assert_eq!(metrics.query_cache_hits, 0);
    }

    #[test]
    fn full_cache_falls_back_to_queries_without_rejecting_readable_ranges() {
        let mut cache = QueryCache::default();
        let mut metrics = PrepareSample::default();
        for tag in 0..CAPACITY + 2 {
            let key = ObserverMappedIdentity {
                source_tag: tag as u64 + 1,
                ..owner()
            };
            assert!(
                cache
                    .resolve(Some(key), 0x1000, 256, Some(&mut metrics), |_, _| Some(
                        region(0x1000, 0x2000)
                    ))
                    .is_some()
            );
        }
        assert_eq!(metrics.memory_query_calls, CAPACITY as u64 + 2);
        assert_eq!(metrics.query_cache_inserts, CAPACITY as u64);
        assert_eq!(metrics.query_cache_full_reads, 2);
        assert_eq!(metrics.memory_query_rejected, 0);
    }

    #[test]
    fn invalid_query_region_is_rejected_and_never_cached() {
        let mut cache = QueryCache::default();
        let mut metrics = PrepareSample::default();
        for info in [
            region(0x1801, 0x1000),
            region(0x1000, 0x800),
            region(0x1000, usize::MAX),
        ] {
            assert!(
                cache
                    .resolve(Some(owner()), 0x1800, 256, Some(&mut metrics), |_, _| Some(
                        info
                    ))
                    .is_none()
            );
        }
        assert_eq!(metrics.memory_query_calls, 3);
        assert_eq!(metrics.memory_query_rejected, 3);
        assert_eq!(metrics.query_cache_inserts, 0);
    }

    #[test]
    fn cached_protection_still_reads_fresh_bytes_and_uses_wide_loads() {
        let mut bytes = [0x42u8; 512];
        let key = ObserverMappedIdentity {
            base: bytes.as_mut_ptr() as usize,
            bytes: 512,
            ..owner()
        };
        let mut cache = QueryCache::default();
        let mut metrics = PrepareSample::default();
        for value in [0x42, 0x75] {
            bytes.fill(value);
            let output =
                unsafe { cache.read(Some(key), key.base, 256, true, Some(&mut metrics)) }.unwrap();
            assert_eq!(output, vec![value; 256]);
        }
        assert_eq!(metrics.memory_query_calls, 1);
        assert_eq!(metrics.query_cache_hits, 1);
        assert_eq!(metrics.read_bytes, 512);
        assert_eq!(metrics.wide_loads * 8 + metrics.byte_loads, 512);
    }
}

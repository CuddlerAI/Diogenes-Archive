//! Frozen pre-cache descriptor resolver is the independent equivalence/benchmark oracle.
use super::*;

impl RenderCameraTracker {
    fn uncached_descriptor_gpuva(
        &self,
        root_parameter_index: u32,
        gpu_va: u64,
        snapshot_bytes: usize,
    ) -> DescriptorGpuVaResolution {
        let mut matches = self.resources.iter().filter_map(|(raw, resource)| {
            let end = resource
                .gpu_virtual_address
                .checked_add(resource.buffer_bytes)?;
            ((resource.gpu_virtual_address..end).contains(&gpu_va)).then_some((*raw, resource))
        });
        let Some((raw, resource)) = matches.next() else {
            return DescriptorGpuVaResolution::WithoutResource;
        };
        if matches.next().is_some() {
            return DescriptorGpuVaResolution::ResourceAmbiguous;
        }
        let resource_offset = gpu_va - resource.gpu_virtual_address;
        let (content_source, copy_shadow_queue) = if resource.cpu_address().is_some() {
            (BufferContentSource::LiveMapped, None)
        } else if resource.upload_heap {
            let Some(snapshot) = resource.unmap_snapshots.get(&resource_offset) else {
                return DescriptorGpuVaResolution::UploadSnapshotMissing;
            };
            if snapshot.content_generation != resource.content_generation
                || snapshot.bytes.len() < snapshot_bytes
            {
                return DescriptorGpuVaResolution::UploadSnapshotMissing;
            }
            (BufferContentSource::UploadUnmapSnapshot, None)
        } else {
            return DescriptorGpuVaResolution::ResourceUnmapped {
                raw,
                offset: resource_offset,
                content_generation: resource.content_generation,
            };
        };
        DescriptorGpuVaResolution::Unique(BufferBinding {
            source_tag: resource.source_tag,
            resource_raw: raw,
            resource_offset,
            root_parameter_index,
            source: RootBindingSource::DescriptorTableBase,
            declared_bytes: None,
            stable_descriptor: None,
            descriptor_identity: None,
            content_source,
            content_generation: resource.content_generation,
            copy_shadow_queue,
            pending_copy_request: None,
        })
    }
}

fn compare(tracker: &RenderCameraTracker, addresses: &[u64]) {
    for &address in addresses {
        for size in [64, 512] {
            assert_eq!(
                tracker.resolve_descriptor_gpuva(2, address, size),
                tracker.uncached_descriptor_gpuva(2, address, size),
                "address {address:x}, bytes {size}"
            );
        }
    }
}

#[test]
fn descriptor_gpuva_cache_keeps_negative_ambiguous_and_recycled_identity_live() {
    let mut tracker = RenderCameraTracker::new(1920, 1080, 75.0);
    let addresses = [
        0,
        0xfff,
        0x1000,
        0x1010,
        0x11ff,
        0x1200,
        0x13ff,
        0x1400,
        0x2010,
        u64::MAX,
    ];
    compare(&tracker, &addresses); // cached missing addresses must invalidate on registration
    tracker.register_buffer_with_heap_kind(1, 0x1000, 1024, true);
    compare(&tracker, &addresses);
    tracker.map_buffer(1, 0xa000);
    compare(&tracker, &addresses);
    tracker.unmap_buffer(1);
    compare(&tracker, &addresses);
    tracker.register_buffer(2, 0x1008, 512); // ambiguity counts even if the second buffer is unmapped
    compare(&tracker, &addresses);
    assert_eq!(
        tracker.resolve_descriptor_gpuva(2, 0x1010, 64),
        DescriptorGpuVaResolution::ResourceAmbiguous
    );
    tracker.release_resource(2);
    compare(&tracker, &addresses);
    tracker.register_buffer_with_heap_kind(1, 0x2000, 1024, true); // same COM address, new ownership
    tracker.map_buffer(1, 0xb000);
    compare(&tracker, &addresses);
    tracker.register_buffer(3, u64::MAX - 16, 128); // overflowing interval remains rejected
    compare(&tracker, &addresses);
    tracker.release_resource(1);
    compare(&tracker, &addresses);
    for address in 0x8000..0x8400 {
        compare(&tracker, &[address]);
    }
    assert!(tracker.gpuva_ownership_cache.borrow().entries.len() <= 256);
    assert!(tracker.gpuva_ownership_cache.borrow().evictions > 0);
    compare(&tracker, &addresses);
}

#[test]
fn descriptor_gpuva_cache_never_caches_upload_bytes_mapping_or_generation() {
    let mut tracker = RenderCameraTracker::new(1920, 1080, 75.0);
    tracker.register_buffer_with_heap_kind(1, 0x1000, 1024, true);
    assert_eq!(
        tracker.resolve_descriptor_gpuva(2, 0x1010, 64),
        DescriptorGpuVaResolution::UploadSnapshotMissing
    );
    tracker.map_buffer(1, 0xa000);
    let DescriptorGpuVaResolution::Unique(first) = tracker.resolve_descriptor_gpuva(2, 0x1010, 64)
    else {
        panic!()
    };
    tracker.write_cbv_descriptor(0x8000, 0x1010, 512);
    tracker.snapshot_upload_cbvs_before_final_unmap(1, None, |_, size| Some(vec![1; size]));
    tracker.unmap_buffer(1);
    let DescriptorGpuVaResolution::Unique(unmapped) =
        tracker.resolve_descriptor_gpuva(2, 0x1010, 64)
    else {
        panic!()
    };
    assert_eq!(
        unmapped.content_source,
        BufferContentSource::UploadUnmapSnapshot
    );
    tracker.map_buffer(1, 0xb000);
    let DescriptorGpuVaResolution::Unique(second) = tracker.resolve_descriptor_gpuva(2, 0x1010, 64)
    else {
        panic!()
    };
    assert!(second.content_generation > first.content_generation);
    assert_eq!(second.content_source, BufferContentSource::LiveMapped);
    assert_eq!(tracker.mapped_read_address(1, 16, 64), Some(0xb010));
    tracker.unmap_buffer(1);
    assert_eq!(
        tracker.resolve_descriptor_gpuva(2, 0x1010, 64),
        DescriptorGpuVaResolution::UploadSnapshotMissing
    );
    compare(&tracker, &[0x1010]);
}

#[test]
fn descriptor_gpuva_cache_removes_repeated_full_resource_walks() {
    let mut tracker = RenderCameraTracker::new(1920, 1080, 75.0);
    for raw in 1..=3589 {
        tracker.register_buffer(raw, raw as u64 * 4096, 1024);
    }
    for i in 0..10000 {
        tracker.resolve_descriptor_gpuva(2, (i % 32 + 1) * 4096, 512);
    }
    let snapshot = tracker.snapshot_without_admission();
    assert_eq!(snapshot.gpuva_ownership_cache_misses, 32);
    assert_eq!(snapshot.gpuva_ownership_cache_hits, 9968);
    assert_eq!(snapshot.gpuva_ownership_resources_scanned, 32 * 3589);
    // A Direct lookup shares ownership only and still rejects an unmapped source.
    assert!(tracker.resolve_binding(2, 4096).is_none());
    assert_eq!(tracker.gpuva_ownership_cache.borrow().misses, 32);
}

#[test]
#[ignore = "explicit local CPU microbenchmark; not a game FPS measurement"]
fn benchmark_descriptor_gpuva_ownership_cache() {
    use std::{hint::black_box, time::Instant};
    let output = std::env::var("CP2077_GPUVA_BENCH_OUTPUT").expect("new output path required");
    let mut rows = Vec::new();
    for resources in [256, 3589] {
        let mut tracker = RenderCameraTracker::new(1920, 1080, 75.0);
        for raw in 1..=resources {
            tracker.register_buffer(raw, raw as u64 * 4096, 1024);
        }
        let queries: Vec<_> = (0..32).map(|i| (i + 1) * 4096).collect();
        compare(&tracker, &queries);
        let mut timings = [Vec::new(), Vec::new()];
        let iterations = 50000;
        for round in 0..6 {
            for which in [round % 2, 1 - round % 2] {
                let start = Instant::now();
                for i in 0..iterations {
                    let addr = black_box(queries[i % queries.len()]);
                    let value = if which == 0 {
                        tracker.uncached_descriptor_gpuva(2, addr, 512)
                    } else {
                        tracker.resolve_descriptor_gpuva(2, addr, 512)
                    };
                    black_box(value);
                }
                timings[which].push(start.elapsed().as_nanos() as f64 / iterations as f64);
            }
        }
        rows.push(
            serde_json::json!({"resources":resources,"distinct_addresses":32,
            "queries_per_repeat":iterations,"repeats":6,
            "uncached_ns_per_query":timings[0],"cached_ns_per_query":timings[1]}),
        );
    }
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)
        .unwrap();
    serde_json::to_writer_pretty(
        file,
        &serde_json::json!({"schema":"cp2077-gpuva-ownership-microbenchmark/v1",
        "non_trainable":true,"acceptance":false,"synthetic_cpu_workload":true,
        "actual_game_fps_measured":false,"rows":rows}),
    )
    .unwrap();
}

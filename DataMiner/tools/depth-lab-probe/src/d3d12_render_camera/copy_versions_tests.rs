use super::*;
use crate::d3d12_abi::{DescriptorHandle, ResourceId};

const UPLOAD: usize = 0x1000;
const DESTINATION: usize = 0x1100;
const LIST_A: usize = 0x2000;
const LIST_B: usize = 0x2100;
const QUEUE: QueueId = QueueId(5);

#[cfg(all(feature = "cp2077-diagnostics", debug_assertions))]
#[test]
fn early_player_diagnostic_joins_real_copy_ids_and_never_reuses_a_previous_execute() {
    const CHILD: &str = "DM_TEST_CP2077_EARLY_PLAYER_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let output=std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact","d3d12_render_camera::copy_versions_tests::early_player_diagnostic_joins_real_copy_ids_and_never_reuses_a_previous_execute","--test-threads=1"])
            .env(CHILD,"1").output().unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    use crate::cp2077_camera_diagnostics as diag;
    let root = std::env::temp_dir().join(format!(
        "dm-cp2077-early-player-test-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir(&root).unwrap();
    let binding = serde_json::from_str(include_str!(
        "../../../../config/catalogs/dev/cyberpunk-2077-steam/camera-binding.json"
    ))
    .unwrap();
    let identity = depth_lab::StreamIdentity {
        session_id: "synthetic".into(),
        run_id: "r0".into(),
        generation: 7,
    };
    diag::begin(&root, identity.clone(), &binding, None);
    assert!(diag::active());
    let mut tracker = fixture();
    for present in [42, 43] {
        tracker.reset_list(LIST_A);
        copy(&mut tracker, LIST_A, 0);
        draw_with_two_descriptor_bindings(&mut tracker);
        let mut reads = 0;
        let mut prepared =
            tracker.prepare_exact_cbv_copy_shadows(&[LIST_A], |raw, offset, bytes| {
                reads += 1;
                upload_bytes(raw, offset, bytes)
            });
        assert_eq!(reads, 1);
        assert_eq!(prepared.len(), 2);
        diag::early_player::sample_after_prepare(QUEUE.0, &mut prepared);
        assert!(prepared.iter().all(|p| p.diagnostic_player.is_some()));
        tracker.commit_exact_cbv_copy_shadows(QUEUE, prepared);
        tracker.materialize_deferred_copy_draws(QUEUE, &[LIST_A]);
        tracker.execute(QUEUE, &[LIST_A]);
        tracker.mark_depth_copy_submitted_in_list(QUEUE, present, Some(LIST_A), Some(0), Some(2));
        let mut now = 0;
        unsafe { windows::Win32::System::Performance::QueryPerformanceCounter(&mut now) }.unwrap();
        tracker.present(QUEUE, present, now);
    }
    // Read succeeded but a Reset between prepare and commit rejects the production request.
    // The diagnostic sample must remain unlinked, not borrow the earlier accepted receipt.
    copy(&mut tracker, LIST_A, 0);
    draw_with_two_descriptor_bindings(&mut tracker);
    let mut rejected = tracker.prepare_exact_cbv_copy_shadows(&[LIST_A], upload_bytes);
    diag::early_player::sample_after_prepare(QUEUE.0, &mut rejected);
    let old = rejected.clone();
    tracker.reset_list(LIST_A);
    tracker.commit_exact_cbv_copy_shadows(QUEUE, rejected);
    diag::finish();
    let path = root.join("cp2077-player-cbv-samples.jsonl");
    let file = std::fs::read_to_string(&path).unwrap();
    let rows: Vec<serde_json::Value> = file
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    let samples: Vec<_> = rows
        .iter()
        .filter(|r| r["schema"] == "dm-cp2077-player-cbv-sample/v1")
        .collect();
    let batches: Vec<_> = rows
        .iter()
        .filter(|r| r["schema"] == "dm-cp2077-player-cbv-bindings/v1")
        .collect();
    let links: Vec<_> = rows
        .iter()
        .filter(|r| r["schema"] == "dm-cp2077-player-cbv-present-links/v1")
        .collect();
    assert_eq!((samples.len(), batches.len(), links.len()), (3, 2, 2));
    assert!(samples.iter().all(|s| s["status"] == "unavailable"
        && s["present_seq"].is_null()
        && s["position"].is_null()));
    assert_ne!(samples[0]["sample_id"], samples[1]["sample_id"]);
    assert_ne!(
        batches[0]["copies"][0]["copy_receipt"]["copy_submission"],
        batches[1]["copies"][0]["copy_receipt"]["copy_submission"]
    );
    for i in 0..2 {
        assert_eq!(batches[i]["sample_id"], samples[i]["sample_id"]);
        assert_eq!(links[i]["present_seq"], 42 + i);
        assert_eq!(links[i]["copies"].as_array().unwrap().len(), 1);
        let copies = batches[i]["copies"].as_array().unwrap();
        assert_eq!(copies.len(), 2);
        assert_eq!(
            copies[0]["snapshot_read_begin_qpc"],
            copies[1]["snapshot_read_begin_qpc"]
        );
        assert_eq!(
            copies[0]["snapshot_read_end_qpc"],
            copies[1]["snapshot_read_end_qpc"]
        );
        assert_eq!(
            copies[0]["copy_receipt"]["copy_submission"],
            links[i]["copies"][0]["copy_submission"]
        );
    }
    let resumed = root.join("resumed");
    std::fs::create_dir(&resumed).unwrap();
    diag::begin(&resumed, identity, &binding, None);
    // Reused generation cannot attach an old run's stamps, even if passed back to the hook.
    let mut stale = old;
    diag::early_player::sample_after_prepare(QUEUE.0, &mut stale);
    diag::finish();
    assert_eq!(
        std::fs::read_to_string(resumed.join("cp2077-player-cbv-samples.jsonl"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    assert_eq!(std::fs::read_to_string(path).unwrap(), file);
    std::fs::remove_dir_all(root).unwrap(); // Only this UUID-named fixture directory.
}

#[test]
fn query_cache_identity_requires_existing_observer_map_at_the_actual_read_address() {
    let mut tracker = RenderCameraTracker::new(1280, 720, 75.0);
    tracker.register_buffer_with_heap_kind(UPLOAD, 0x10_0000, 512, true);
    tracker.map_buffer(UPLOAD, 0x20_0000);
    assert!(
        tracker
            .observer_mapped_identity(UPLOAD, 0x20_0000)
            .is_none()
    );
    tracker
        .register_observer_mapping(UPLOAD, 0x20_0000)
        .unwrap();
    let first = tracker.observer_mapped_identity(UPLOAD, 0x20_0000).unwrap();
    assert_eq!(
        (first.resource_raw, first.base, first.bytes),
        (UPLOAD, 0x20_0000, 512)
    );
    assert!(tracker.observer_mapped_identity(UPLOAD, 0).is_none());
    assert!(
        tracker
            .observer_mapped_identity(UPLOAD, 0x20_0100)
            .is_none()
    );

    // App final Unmap does not retire the observer's separately nested Map.
    tracker.unmap_buffer(UPLOAD);
    assert_eq!(
        tracker.observer_mapped_identity(UPLOAD, 0x20_0000),
        Some(first)
    );
    tracker.map_buffer(UPLOAD, 0x20_0000);
    let remapped = tracker.observer_mapped_identity(UPLOAD, 0x20_0000).unwrap();
    assert_ne!(first.content_generation, remapped.content_generation);

    // A different application pointer takes priority and cannot inherit observer evidence.
    tracker.map_buffer(UPLOAD, 0x30_0000);
    assert!(
        tracker
            .observer_mapped_identity(UPLOAD, 0x30_0000)
            .is_none()
    );
    assert!(
        tracker
            .observer_mapped_identity(UPLOAD, 0x20_0000)
            .is_none()
    );
    tracker.unmap_buffer(UPLOAD);
    tracker.unmap_buffer(UPLOAD);
    assert_eq!(
        tracker.observer_mapped_identity(UPLOAD, 0x20_0000),
        Some(remapped)
    );

    tracker.release_resource(UPLOAD);
    assert!(
        tracker
            .observer_mapped_identity(UPLOAD, 0x20_0000)
            .is_none()
    );
    tracker.register_buffer_with_heap_kind(UPLOAD, 0x10_0000, 512, true);
    tracker
        .register_observer_mapping(UPLOAD, 0x20_0000)
        .unwrap();
    let replaced = tracker.observer_mapped_identity(UPLOAD, 0x20_0000).unwrap();
    assert_ne!(first.source_tag, replaced.source_tag);
    tracker.register_buffer_with_heap_kind(UPLOAD, 0x10_0000, 1024, true);
    assert!(
        tracker
            .observer_mapped_identity(UPLOAD, 0x20_0000)
            .is_none()
    );
    tracker.register_buffer_with_heap_kind(UPLOAD, 0x10_0000, 1024, false);
    assert!(
        tracker
            .register_observer_mapping(UPLOAD, 0x20_0000)
            .is_err()
    );
    assert!(
        tracker
            .observer_mapped_identity(UPLOAD, 0x20_0000)
            .is_none()
    );
}

fn fixture() -> RenderCameraTracker {
    let mut tracker = RenderCameraTracker::new(1280, 720, 75.0);
    tracker.register_buffer_with_heap_kind(UPLOAD, 0x10_0000, 512, true);
    tracker.map_buffer(UPLOAD, 0x20_0000);
    tracker.register_buffer_with_heap_kind(DESTINATION, 0x11_0000, 512, false);
    tracker.register_descriptor_heap(0x3000, 0x40_0000, 0x50_0000, 8, 32, true);
    tracker.write_cbv_descriptor(0x40_0040, 0x11_0000, 256);
    tracker.register_root_signature_with_fingerprint(
        0x6000,
        format!("sha256:{}", "a".repeat(64)),
        [4, 18]
            .into_iter()
            .map(|root| {
                (
                    root,
                    vec![RootCbvRange {
                        table_offset: 2,
                        descriptor_count: 1,
                        descriptor_range_index: 0,
                        base_shader_register: 0,
                        register_space: 0,
                    }],
                )
            })
            .collect(),
    );
    tracker.register_list(LIST_A, 9);
    tracker.register_list(LIST_B, 10);
    tracker.arm_selected_depth(DsvIdentity {
        handle: DescriptorHandle(4),
        descriptor_generation: 2,
        resource: ResourceId(7),
        resource_epoch: 3,
    });
    tracker
}

fn copy(tracker: &mut RenderCameraTracker, list: usize, source_offset: u64) {
    tracker.record_copy_buffer_region(list, DESTINATION, 0, UPLOAD, source_offset, 256);
}

fn draw(tracker: &mut RenderCameraTracker, list: usize, ordinal: u32, root: u32, write: bool) {
    let bindings = SelectedDrawBindings {
        pipeline_state: if write { 0x8000 } else { 0x8001 },
        pass_context: None,
        pipeline_depth: Some(PipelineDepthState {
            enable: true,
            write,
        }),
        list_generation: tracker.lists[&list].list_generation,
        draw_ordinal: ordinal,
        pass_ordinal: 0,
        draw_ordinal_in_pass: ordinal - 1,
        descriptor_heaps: [0x3000, 0],
        root_signature: 0x6000,
        roots: vec![ObservedRootBinding {
            root_parameter_index: root,
            source: RootBindingSource::DescriptorTableBase,
            value: 0x50_0000,
        }],
    };
    tracker.observe_selected_draw(list, &bindings, |_, _, _| None);
}

fn upload_bytes(_: usize, offset: u64, count: usize) -> Option<Vec<u8>> {
    let x = 1.0_f32 / (75.0_f32.to_radians() * 0.5).tan();
    let m22 = if offset == 0 { 0.0001 } else { 0.0002 };
    let matrices: [[f32; 4]; 8] = [
        [1., 0., 0., 0.],
        [0., 1., 0., 0.],
        [0., 0., 1., 0.],
        [1., 2., 3., 1.],
        [x, 0., 0., 0.],
        [0., x * (16. / 9.), 0., 0.],
        [0., 0., m22, 1.],
        [0., 0., 0.1, 0.],
    ];
    let mut bytes: Vec<_> = matrices
        .into_iter()
        .flatten()
        .flat_map(f32::to_le_bytes)
        .collect();
    bytes.resize(count, 0);
    Some(bytes)
}

fn submit(tracker: &mut RenderCameraTracker, lists: &[usize]) -> D3d12RenderCameraSnapshot {
    submit_at(tracker, lists, 44)
}

fn submit_at(
    tracker: &mut RenderCameraTracker,
    lists: &[usize],
    present: u64,
) -> D3d12RenderCameraSnapshot {
    let prepared = tracker.prepare_exact_cbv_copy_shadows(lists, upload_bytes);
    tracker.commit_exact_cbv_copy_shadows(QUEUE, prepared);
    tracker.materialize_deferred_copy_draws(QUEUE, lists);
    tracker.execute(QUEUE, lists);
    tracker.mark_depth_copy_submitted_in_list(
        QUEUE,
        present,
        lists.last().copied(),
        Some((lists.len() - 1) as u32),
        Some(2),
    );
    tracker.present(QUEUE, present, 1234 + present as i64);
    tracker.snapshot()
}

fn draw_with_two_descriptor_bindings(tracker: &mut RenderCameraTracker) {
    tracker.write_cbv_descriptor(0x40_0080, 0x11_0000, 256);
    tracker.observe_selected_draw(
        LIST_A,
        &SelectedDrawBindings {
            pipeline_state: 0x8000,
            pass_context: None,
            pipeline_depth: Some(PipelineDepthState {
                enable: true,
                write: true,
            }),
            list_generation: tracker.lists[&LIST_A].list_generation,
            draw_ordinal: 1,
            pass_ordinal: 0,
            draw_ordinal_in_pass: 0,
            descriptor_heaps: [0x3000, 0],
            root_signature: 0x6000,
            roots: [(4, 0x50_0000), (18, 0x50_0040)]
                .into_iter()
                .map(|(root_parameter_index, value)| ObservedRootBinding {
                    root_parameter_index,
                    source: RootBindingSource::DescriptorTableBase,
                    value,
                })
                .collect(),
        },
        |_, _, _| None,
    );
}

#[test]
fn prepare_reuses_physical_bytes_but_preserves_independent_alias_outputs() {
    let mut tracker = fixture();
    copy(&mut tracker, LIST_A, 0);
    draw_with_two_descriptor_bindings(&mut tracker);
    let mut reads = 0;
    let mut metrics = crate::execute_prepare::PrepareSample::default();
    let mut prepared = tracker.prepare_exact_cbv_copy_shadows_measured(
        &[LIST_A],
        Some(&mut metrics),
        |_, _, bytes| {
            reads += 1;
            Some(vec![42; bytes])
        },
    );
    assert_eq!(reads, 1);
    assert_eq!(prepared.len(), 2);
    assert_eq!(
        prepared[0].request.physical_key(),
        prepared[1].request.physical_key()
    );
    assert_ne!(
        prepared[0].request.descriptor_identity,
        prepared[1].request.descriptor_identity
    );
    assert_eq!(metrics.requests, 2);
    assert_eq!(metrics.live_requests, 2);
    assert_eq!(metrics.read_attempts, 1);
    assert_eq!(metrics.reused_snapshots, 1);
    assert_eq!(metrics.reused_bytes, 256);
    prepared[0].bytes[0] = 1;
    assert_eq!(prepared[1].bytes[0], 42);
    // Cache is local to one prepare, even if list generation did not change.
    let again = tracker.prepare_exact_cbv_copy_shadows(&[LIST_A], |_, _, bytes| {
        reads += 1;
        Some(vec![77; bytes])
    });
    assert_eq!(reads, 2);
    assert!(again.iter().all(|shadow| shadow.bytes == vec![77; 256]));
}

#[test]
fn prepare_never_reuses_across_lists_or_distinct_copy_commands() {
    for same_list in [false, true] {
        let mut tracker = fixture();
        copy(&mut tracker, LIST_A, 0);
        draw(&mut tracker, LIST_A, 1, 4, true);
        let second = if same_list { LIST_A } else { LIST_B };
        copy(&mut tracker, second, 0);
        draw(&mut tracker, second, 2, 18, true);
        let lists = if same_list {
            vec![LIST_A]
        } else {
            vec![LIST_A, LIST_B]
        };
        let mut reads = 0;
        let prepared = tracker.prepare_exact_cbv_copy_shadows(&lists, |_, _, bytes| {
            reads += 1;
            Some(vec![42; bytes])
        });
        assert_eq!(prepared.len(), 2);
        assert_eq!(reads, 2);
        assert_ne!(
            prepared[0].request.physical_key(),
            prepared[1].request.physical_key()
        );
    }
}

#[test]
fn prepare_still_checks_each_alias_descriptor_after_the_first_read() {
    let mut tracker = fixture();
    copy(&mut tracker, LIST_A, 0);
    draw_with_two_descriptor_bindings(&mut tracker);
    let second_handle = tracker.lists[&LIST_A].exact_cbv_copy_requests[1]
        .descriptor_identity
        .0;
    let store = tracker.cbv_descriptor_store.clone();
    let mut metrics = crate::execute_prepare::PrepareSample::default();
    let prepared = tracker.prepare_exact_cbv_copy_shadows_measured(
        &[LIST_A],
        Some(&mut metrics),
        |_, _, bytes| {
            store.write_cbv_descriptor(second_handle, 0x11_0000, 256);
            Some(vec![42; bytes])
        },
    );
    assert_eq!(prepared.len(), 1);
    assert_eq!(metrics.validation_rejected, 1);
    assert_eq!(metrics.reused_snapshots, 0);
}

#[test]
fn failed_or_short_read_is_not_cached_for_a_sibling_alias() {
    for short in [false, true] {
        let mut tracker = fixture();
        copy(&mut tracker, LIST_A, 0);
        draw_with_two_descriptor_bindings(&mut tracker);
        let mut reads = 0;
        let mut metrics = crate::execute_prepare::PrepareSample::default();
        let prepared = tracker.prepare_exact_cbv_copy_shadows_measured(
            &[LIST_A],
            Some(&mut metrics),
            |_, _, bytes| {
                reads += 1;
                if reads == 1 {
                    if short {
                        Some(vec![0; bytes - 1])
                    } else {
                        None
                    }
                } else {
                    Some(vec![42; bytes])
                }
            },
        );
        assert_eq!(reads, 2);
        assert_eq!(prepared.len(), 1);
        assert_eq!(metrics.read_rejected, 1);
        assert_eq!(metrics.reused_snapshots, 0);
    }
}

#[test]
fn one_physical_copy_across_two_descriptors_survives_serialization_and_alias_validation() {
    use depth_lab::d3d12_camera_alias::{self, Sample};
    let mut tracker = fixture();
    for present in 1..=8 {
        tracker.reset_list(LIST_A);
        copy(&mut tracker, LIST_A, 0);
        draw_with_two_descriptor_bindings(&mut tracker);
        submit_at(&mut tracker, &[LIST_A], present);
    }
    let snapshot = tracker.snapshot();
    assert_eq!(snapshot.observations.len(), 8);
    let mut views = Vec::new();
    let mut projections = Vec::new();
    let mut submissions = BTreeSet::new();
    for draw in &snapshot.observations {
        assert!(draw.selected_present_identity_passed);
        assert_eq!(draw.matrices.len(), 4);
        let a = draw.matrices[0].copy_receipt.as_ref().unwrap();
        let b = draw
            .matrices
            .iter()
            .find(|m| m.root_parameter_index != draw.matrices[0].root_parameter_index)
            .unwrap()
            .copy_receipt
            .as_ref()
            .unwrap();
        assert_ne!(a.descriptor_generation, b.descriptor_generation);
        assert_eq!(a.copy_submission, b.copy_submission);
        assert_eq!(a.copy_command, b.copy_command);
        assert!(a.same_physical_copy(b));
        assert!(submissions.insert(a.copy_submission));
        for matrix in &draw.matrices {
            // Exercise the actual JSON boundary consumed by the offline resolver.
            let json = serde_json::to_value(matrix).unwrap();
            let sample = Sample {
                present: draw.present_seq,
                source: serde_json::from_value(json["stable_source"].clone()).unwrap(),
                matrix: serde_json::from_value(json["matrix"].clone()).unwrap(),
                receipt: serde_json::from_value(json["copy_receipt"].clone()).unwrap(),
            };
            if matrix.matrix_kind == "view" {
                views.push(sample);
            } else {
                projections.push(sample);
            }
        }
    }
    let view = d3d12_camera_alias::resolve(views).unwrap().remove(0);
    let projection = d3d12_camera_alias::resolve(projections).unwrap().remove(0);
    assert_eq!(view.sources.len(), 2);
    assert_eq!(view.samples.len(), 8);
    d3d12_camera_alias::validate_pair(&view, &projection).unwrap();
}

#[test]
fn physical_copy_id_is_new_for_each_execute_even_without_list_reset() {
    let mut tracker = fixture();
    copy(&mut tracker, LIST_A, 0);
    draw_with_two_descriptor_bindings(&mut tracker);
    let first = tracker.prepare_exact_cbv_copy_shadows(&[LIST_A], upload_bytes);
    tracker.commit_exact_cbv_copy_shadows(QUEUE, first.clone());
    let a = tracker
        .exact_cbv_copy_shadows
        .values()
        .next()
        .unwrap()
        .receipt
        .clone();
    tracker.commit_exact_cbv_copy_shadows(QUEUE, first);
    let b = &tracker
        .exact_cbv_copy_shadows
        .values()
        .next()
        .unwrap()
        .receipt;
    assert_eq!(a.copy_command, b.copy_command);
    assert_ne!(a.copy_submission, b.copy_submission);
    assert!(!a.same_physical_copy(b));
}

#[test]
fn aliased_binding_cannot_borrow_a_live_siblings_descriptor_generation() {
    let mut tracker = fixture();
    copy(&mut tracker, LIST_A, 0);
    draw_with_two_descriptor_bindings(&mut tracker);
    let prepared = tracker.prepare_exact_cbv_copy_shadows(&[LIST_A], upload_bytes);
    assert_eq!(prepared.len(), 2);
    tracker.write_cbv_descriptor(0x40_0080, 0x11_0000, 256);
    tracker.commit_exact_cbv_copy_shadows(QUEUE, prepared);
    assert_eq!(tracker.exact_cbv_copy_shadows.len(), 1);
    tracker.materialize_deferred_copy_draws(QUEUE, &[LIST_A]);
    tracker.execute(QUEUE, &[LIST_A]);
    tracker.mark_depth_copy_submitted_in_list(QUEUE, 44, Some(LIST_A), Some(0), Some(1));
    tracker.present(QUEUE, 44, 1234);
    let snapshot = tracker.snapshot();
    assert!(snapshot.exact_cbv_copy_shadow_generation_rejections > 0);
    assert_eq!(snapshot.observations.len(), 1);
    assert_eq!(snapshot.observations[0].matrices.len(), 2);
    assert!(
        snapshot.observations[0]
            .matrices
            .iter()
            .all(|m| m.root_parameter_index == 4)
    );
}

#[test]
fn equal_command_identity_does_not_conceal_different_binding_payloads() {
    let mut tracker = fixture();
    copy(&mut tracker, LIST_A, 0);
    draw_with_two_descriptor_bindings(&mut tracker);
    let mut prepared = tracker.prepare_exact_cbv_copy_shadows(&[LIST_A], upload_bytes);
    assert_eq!(prepared.len(), 2);
    prepared[1].bytes[255] = 1;
    tracker.commit_exact_cbv_copy_shadows(QUEUE, prepared);
    let receipts: Vec<_> = tracker
        .exact_cbv_copy_shadows
        .values()
        .map(|s| &s.receipt)
        .collect();
    assert_eq!(receipts[0].copy_submission, receipts[1].copy_submission);
    assert_ne!(receipts[0].bytes_sha256, receipts[1].bytes_sha256);
    assert!(!receipts[0].same_physical_copy(receipts[1]));
}

fn projection(draw: &D3d12RenderDrawObservation) -> &D3d12RenderMatrixObservation {
    draw.matrices
        .iter()
        .find(|matrix| matrix.matrix_kind == "projection")
        .unwrap()
}

fn assert_distinct_copies(snapshot: &D3d12RenderCameraSnapshot) {
    assert_eq!(snapshot.observations.len(), 2);
    #[cfg(all(feature = "cp2077-diagnostics", debug_assertions))]
    {
        let first = &snapshot.observations[0];
        let mut families = crate::cp2077_camera_diagnostics::FamilyFrame::new(
            first.present_seq,
            first.present_qpc,
        );
        for draw in &snapshot.observations {
            families.observe(draw, Some(1));
        }
        // Aggregation consumes the corrected copy-version observations without collapsing
        // different lists or later copies into the first projection family.
        assert_eq!(families.families.len(), 2);
        let json = serde_json::to_value(families).unwrap();
        assert_eq!(json["family_count_observed"], 2);
        assert_eq!(json["represented_draws"], 2);
    }
    let a = projection(&snapshot.observations[0]);
    let b = projection(&snapshot.observations[1]);
    assert_eq!(a.matrix[2][2], 0.0001_f32);
    assert_eq!(b.matrix[2][2], 0.0002_f32);
    let a = a.copy_receipt.as_ref().unwrap();
    let b = b.copy_receipt.as_ref().unwrap();
    assert_eq!((a.source_offset, b.source_offset), (0, 256));
    assert_ne!(a.copy_submission, b.copy_submission);
    assert_ne!(a.bytes_sha256, b.bytes_sha256);
    assert!(
        snapshot
            .observations
            .iter()
            .all(|draw| draw.selected_present_identity_passed)
    );
}

#[test]
fn same_list_overwrite_binds_each_draw_to_its_own_copy() {
    let mut tracker = fixture();
    copy(&mut tracker, LIST_A, 0);
    draw(&mut tracker, LIST_A, 1, 4, true);
    copy(&mut tracker, LIST_A, 256);
    draw(&mut tracker, LIST_A, 2, 18, true);
    assert_distinct_copies(&submit(&mut tracker, &[LIST_A]));
    assert!(tracker.exact_cbv_copy_shadows.is_empty());
    assert_eq!(tracker.exact_cbv_copy_shadow_bytes, 0);
}

#[test]
fn same_execute_lists_do_not_overwrite_each_others_copy_versions() {
    let mut tracker = fixture();
    copy(&mut tracker, LIST_A, 0);
    draw(&mut tracker, LIST_A, 1, 4, true);
    copy(&mut tracker, LIST_B, 256);
    draw(&mut tracker, LIST_B, 1, 18, true);
    assert_distinct_copies(&submit(&mut tracker, &[LIST_A, LIST_B]));
}

#[test]
fn equal_bytes_and_receipt_preserve_each_draws_pso_state() {
    for writes in [[false, true], [true, false]] {
        let mut tracker = fixture();
        copy(&mut tracker, LIST_A, 0);
        for (index, write) in writes.into_iter().enumerate() {
            draw(&mut tracker, LIST_A, index as u32 + 1, 4, write);
        }
        let snapshot = submit(&mut tracker, &[LIST_A]);
        assert_eq!(snapshot.draw_scan_budget_exhausted, 0);
        assert_eq!(snapshot.observations.len(), 2);
        for (index, observation) in snapshot.observations.iter().enumerate() {
            assert_eq!(observation.draw_ordinal, index as u32 + 1);
            assert_eq!(observation.pipeline_depth_write, Some(writes[index]));
        }
        assert_eq!(
            projection(&snapshot.observations[0]).copy_receipt,
            projection(&snapshot.observations[1]).copy_receipt
        );
    }
}

#[test]
fn repeated_same_source_bytes_are_still_distinct_copy_commands() {
    let mut tracker = fixture();
    for ordinal in [1, 2] {
        copy(&mut tracker, LIST_A, 0);
        draw(&mut tracker, LIST_A, ordinal, 4, true);
    }
    let snapshot = submit(&mut tracker, &[LIST_A]);
    assert_eq!(snapshot.observations.len(), 2);
    let a = projection(&snapshot.observations[0])
        .copy_receipt
        .as_ref()
        .unwrap();
    let b = projection(&snapshot.observations[1])
        .copy_receipt
        .as_ref()
        .unwrap();
    assert_eq!(a.bytes_sha256, b.bytes_sha256);
    assert_ne!(a.copy_submission, b.copy_submission);
}

#[test]
fn equal_camera_bytes_do_not_hide_a_draw_after_the_depth_copy() {
    let mut tracker = fixture();
    copy(&mut tracker, LIST_A, 0);
    draw(&mut tracker, LIST_A, 1, 4, true);
    draw(&mut tracker, LIST_A, 2, 4, true);
    submit(&mut tracker, &[LIST_A]);
    tracker.mark_depth_copy_submitted_in_list(QUEUE, 44, Some(LIST_A), Some(0), Some(1));
    let snapshot = tracker.snapshot();
    assert_eq!(snapshot.observations.len(), 2);
    assert!(snapshot.observations[0].selected_present_identity_passed);
    assert!(!snapshot.observations[1].selected_present_identity_passed);
}

#[test]
fn unreadable_new_copy_never_borrows_a_successful_sibling_shadow() {
    let mut tracker = fixture();
    copy(&mut tracker, LIST_A, 0);
    draw(&mut tracker, LIST_A, 1, 4, true);
    copy(&mut tracker, LIST_B, 256);
    draw(&mut tracker, LIST_B, 1, 18, true);
    let lists = [LIST_A, LIST_B];
    let prepared = tracker.prepare_exact_cbv_copy_shadows(&lists, |raw, offset, bytes| {
        if offset == 0 {
            upload_bytes(raw, offset, bytes)
        } else {
            None
        }
    });
    assert_eq!(prepared.len(), 1);
    tracker.commit_exact_cbv_copy_shadows(QUEUE, prepared);
    tracker.materialize_deferred_copy_draws(QUEUE, &lists);
    tracker.execute(QUEUE, &lists);
    tracker.present(QUEUE, 44, 1234);
    let snapshot = tracker.snapshot();
    assert_eq!(snapshot.observations.len(), 1);
    assert_eq!(snapshot.observations[0].command_list_tag, 9);
    assert!(snapshot.exact_cbv_copy_prepare_read_rejections > 0);
    assert!(snapshot.exact_cbv_copy_shadow_generation_rejections > 0);
}

#[test]
fn partial_or_unreadable_overwrite_never_falls_back_to_an_old_copy() {
    for (source, destination_offset, bytes) in
        [(UPLOAD, 64, 32), (DESTINATION, 0, 256), (0xdead, 0, 256)]
    {
        let mut tracker = fixture();
        copy(&mut tracker, LIST_A, 0);
        draw(&mut tracker, LIST_A, 1, 4, true);
        tracker.record_copy_buffer_region(
            LIST_A,
            DESTINATION,
            destination_offset,
            source,
            0,
            bytes,
        );
        draw(&mut tracker, LIST_A, 2, 4, true);
        let snapshot = submit(&mut tracker, &[LIST_A]);
        assert_eq!(snapshot.observations.len(), 1);
        assert_eq!(snapshot.observations[0].draw_ordinal, 1);
        assert_eq!(
            projection(&snapshot.observations[0])
                .copy_receipt
                .as_ref()
                .unwrap()
                .source_offset,
            0
        );
    }
}

#[test]
fn non_overlapping_write_does_not_invalidate_a_bound_copy() {
    let mut tracker = fixture();
    copy(&mut tracker, LIST_A, 0);
    draw(&mut tracker, LIST_A, 1, 4, true);
    tracker.record_copy_buffer_region(LIST_A, DESTINATION, 256, UPLOAD, 256, 256);
    draw(&mut tracker, LIST_A, 2, 4, true);
    let snapshot = submit(&mut tracker, &[LIST_A]);
    assert_eq!(snapshot.observations.len(), 2);
    assert_eq!(
        projection(&snapshot.observations[0]).copy_receipt,
        projection(&snapshot.observations[1]).copy_receipt
    );
}

#[test]
fn prepared_copy_rejects_reset_lists_and_reused_resource_addresses() {
    for invalidate in 0..3 {
        let mut tracker = fixture();
        copy(&mut tracker, LIST_A, 0);
        draw(&mut tracker, LIST_A, 1, 4, true);
        let prepared = tracker.prepare_exact_cbv_copy_shadows(&[LIST_A], upload_bytes);
        assert_eq!(prepared.len(), 1);
        match invalidate {
            0 => tracker.reset_list(LIST_A),
            1 => {
                tracker.release_resource(DESTINATION);
                tracker.register_buffer_with_heap_kind(DESTINATION, 0x11_0000, 512, false);
            }
            _ => tracker.register_list(LIST_A, 99),
        }
        tracker.commit_exact_cbv_copy_shadows(QUEUE, prepared);
        assert!(tracker.exact_cbv_copy_shadows.is_empty());
        assert_eq!(tracker.snapshot.exact_cbv_copy_shadows_committed, 0);
        assert!(tracker.snapshot.exact_cbv_copy_shadow_generation_rejections > 0);
    }
}

#[test]
fn copy_span_overflow_cannot_hide_a_later_write() {
    let mut tracker = fixture();
    for _ in 0..=MAX_COPY_BUFFER_SPANS_PER_LIST {
        copy(&mut tracker, LIST_A, 0);
    }
    draw(&mut tracker, LIST_A, 1, 4, true);
    let snapshot = submit(&mut tracker, &[LIST_A]);
    assert!(snapshot.observations.is_empty());
    assert!(snapshot.exact_cbv_copy_span_budget_exhausted > 0);
    assert!(!snapshot.candidate_admitted_for_cross_launch_discovery);
}

#[test]
fn present_summary_matches_full_admission_and_never_reuses_a_stale_pass() {
    let mut tracker = fixture();
    copy(&mut tracker, LIST_A, 0);
    draw(&mut tracker, LIST_A, 1, 4, true);
    let template = submit(&mut tracker, &[LIST_A]).observations.remove(0);
    tracker.snapshot.observations = (1..=MIN_ADMITTED_PRESENTS_PER_MATRIX_SOURCE)
        .map(|present| {
            let mut draw = template.clone();
            draw.present_seq = u64::try_from(present).unwrap();
            draw
        })
        .collect();
    let present = u64::try_from(MIN_ADMITTED_PRESENTS_PER_MATRIX_SOURCE).unwrap();
    let full = tracker.snapshot();
    assert!(full.candidate_admitted_for_cross_launch_discovery);
    assert_eq!(
        tracker.exact_present_fields(present, false),
        (true, true, true)
    );
    assert_eq!(
        tracker.exact_present_fields(present + 1, false),
        (false, false, false)
    );
    assert_eq!(
        tracker.exact_present_fields(present, true),
        (true, true, false)
    );

    tracker.snapshot.pending_draw_overflow = 1;
    assert!(
        !tracker
            .snapshot()
            .candidate_admitted_for_cross_launch_discovery
    );
    assert_eq!(
        tracker.exact_present_fields(present, false),
        (true, true, false)
    );
    tracker.snapshot.pending_draw_overflow = 0;
    tracker
        .snapshot
        .observations
        .last_mut()
        .unwrap()
        .selected_present_identity_passed = false;
    assert_eq!(
        tracker.exact_present_fields(present, false),
        (false, false, false)
    );
    tracker
        .snapshot
        .observations
        .last_mut()
        .unwrap()
        .selected_present_identity_passed = true;
    assert_eq!(
        tracker.exact_present_fields(present, false),
        (true, true, true)
    );

    // Changing a historical source must immediately affect admission, even though this
    // Present's two physical matrices are still present and unchanged.
    tracker.snapshot.observations[0].matrices[0].stable_source = None;
    assert!(
        !tracker
            .snapshot()
            .candidate_admitted_for_cross_launch_discovery
    );
    assert_eq!(
        tracker.exact_present_fields(present, false),
        (true, true, false)
    );
}

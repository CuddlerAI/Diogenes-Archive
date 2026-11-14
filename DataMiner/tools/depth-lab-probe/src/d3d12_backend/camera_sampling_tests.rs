//! Exercise the real draw-hook admission path, including its outer sampling budget.
use super::*;
use crate::d3d12_render_camera::{CapturedViewport, RootCbvRange};
use windows::Win32::Graphics::Direct3D12::D3D12_VIEWPORT;

const EARLY: usize = 0x2000;
const LATE: usize = 0x2100;
const UPLOAD: usize = 0x1000;
const DEST: usize = 0x1100;
const QUEUE: QueueId = QueueId(5);

fn identity() -> DsvIdentity {
    DsvIdentity {
        handle: DescriptorHandle(4),
        descriptor_generation: 2,
        resource: ResourceId(7),
        resource_epoch: 3,
    }
}

fn fixture() -> Arc<D3d12ObservationBackend> {
    let backend = D3d12ObservationBackend::new();
    backend.pso_registry.register_test_writer(0x7000);
    let root_signature_sha256 = format!("sha256:{}", "a".repeat(64));
    {
        let mut t = backend.render_camera.lock().unwrap();
        t.register_buffer_with_heap_kind(UPLOAD, 0x10_0000, 512, true);
        t.map_buffer(UPLOAD, 0x20_0000);
        t.register_buffer_with_heap_kind(DEST, 0x11_0000, 512, false);
        t.register_descriptor_heap(0x3000, 0x40_0000, 0x50_0000, 8, 32, true);
        t.write_cbv_descriptor(0x40_0040, 0x11_0000, 256);
        t.register_root_signature_with_fingerprint(
            0x6000,
            root_signature_sha256.clone(),
            BTreeMap::from([(
                4,
                vec![RootCbvRange {
                    table_offset: 2,
                    descriptor_count: 1,
                    descriptor_range_index: 0,
                    base_shader_register: 0,
                    register_space: 0,
                }],
            )]),
        );
        for (raw, tag) in [(EARLY, 9), (LATE, 10)] {
            t.register_list(raw, tag);
            backend.hot_lists.register(raw, tag);
        }
        t.arm_selected_depth(identity());
        t.enable_depth_order_diagnostics();
    }
    backend
        .root_fingerprints
        .write()
        .unwrap()
        .insert(0x6000, root_signature_sha256);
    backend.arm_selected_depth_hot_path(identity());
    backend
        .depth_order_diagnostics
        .store(true, Ordering::Release);
    backend.set_runtime_observation_active(true);
    backend
}

fn begin_list(backend: &D3d12ObservationBackend, raw: usize) {
    backend.hot_lists.reset(raw);
    backend.hot_lists.set_pipeline_hook_available(raw, true);
    backend.hot_lists.bind_pipeline_state(raw, 0x7000);
    backend
        .hot_lists
        .bind_selected_depth(raw, backend.selected_depth_token.load(Ordering::Acquire));
    backend.hot_lists.set_bound_dsv_flags(raw, Some(0));
    backend.hot_lists.bind_heaps(raw, [0x3000, 0]);
    backend
        .hot_lists
        .find(raw)
        .unwrap()
        .root_signature
        .store(0x6000, Ordering::Release);
    backend
        .hot_lists
        .bind_root(raw, 4, ROOT_DESCRIPTOR_TABLE, 0x50_0000);
    let mut t = backend.render_camera.lock().unwrap();
    t.reset_list(raw);
    t.set_viewport(
        raw,
        CapturedViewport {
            count: 1,
            first: D3D12_VIEWPORT {
                Width: 1920.0,
                Height: 1080.0,
                MaxDepth: 1.0,
                ..Default::default()
            },
        },
    );
    t.record_copy_buffer_region(raw, DEST, 0, UPLOAD, 0, 256);
}

fn upload_bytes(_: usize, _: u64, count: usize) -> Option<Vec<u8>> {
    let x = 1.0_f32 / (75.0_f32.to_radians() * 0.5).tan();
    let matrices: [[f32; 4]; 8] = [
        [1., 0., 0., 0.],
        [0., 1., 0., 0.],
        [0., 0., 1., 0.],
        [1., 2., 3., 1.],
        [x, 0., 0., 0.],
        [0., x * (16. / 9.), 0., 0.],
        [0., 0., 0.0001, 1.],
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

fn execute(backend: &D3d12ObservationBackend, lists: &[usize]) {
    let mut t = backend.render_camera.lock().unwrap();
    let prepared = t.prepare_exact_cbv_copy_shadows(lists, upload_bytes);
    t.commit_exact_cbv_copy_shadows(QUEUE, prepared);
    t.materialize_deferred_copy_draws(QUEUE, lists);
    t.execute(QUEUE, lists);
}

#[test]
fn bound_camera_survives_ready_wait_recording_and_sealed_feedback() {
    let backend = fixture();
    // Learn only this synthetic fixture's authored descriptors, then run the
    // actual production resolver with fresh per-run physical-copy admission.
    begin_list(&backend, EARLY);
    observe_render_camera_draw(&backend, EARLY);
    execute(&backend, &[EARLY]);
    let mut binding: depth_lab::D3d12CameraBinding = serde_json::from_str(include_str!(
        "../../../depth-lab/tests/fixtures/camera-binding-v2.json"
    ))
    .unwrap();
    {
        let mut t = backend.render_camera.lock().unwrap();
        t.mark_depth_copy_submitted_in_list(QUEUE, 1, Some(EARLY), Some(0), Some(1));
        t.present(QUEUE, 1, 1001);
        let snapshot = t.snapshot();
        let (v, p) = snapshot.local_camera_samples(1, 1920, 1080).unwrap();
        binding.view_sources = vec![depth_lab::D3d12ViewSourceBinding {
            source: v[0].sample.source.clone(),
            translation: depth_lab::camera::TranslationSlot::Row3,
        }];
        binding.projection_sources = vec![p[0].sample.source.clone()];
    }
    binding.schema = depth_lab::CAMERA_BINDING_SCHEMA_V3.into();
    binding.executable_sha256 = depth_lab::CP2077_BUILD_20383525_EXE_SHA256.into();
    binding.render_width = 1920;
    binding.render_height = 1080;
    binding.holstered_camera = Some(depth_lab::Cp2077HolsteredCamera {
        schema: depth_lab::cp2077_bound_camera::POLICY.into(),
        camera_axis_mapping: binding.axis_mapping,
    });
    backend
        .configure_production_camera(
            binding,
            depth_lab::CameraPoseStreamContext {
                identity: depth_lab::StreamIdentity {
                    session_id: "copy-lifecycle".into(),
                    run_id: "fresh".into(),
                    generation: 1,
                },
                dsv_id: "synthetic-depth".into(),
            },
        )
        .unwrap();
    let mut emitted = 0;
    for present in 2..=150 {
        // First 120 Presents warm up. Then delay the client Arm, accept a
        // short recording, and delay StopRun after GenerationSealed.
        let formal = present >= 122;
        let accepting = (135..140).contains(&present);
        let copy_requested = !formal || backend.selected_interval_capture_requested(accepting);
        begin_list(&backend, EARLY);
        observe_render_camera_draw(&backend, EARLY);
        execute(&backend, &[EARLY]);
        {
            let mut t = backend.render_camera.lock().unwrap();
            if copy_requested {
                t.mark_depth_copy_submitted_in_list(QUEUE, present, Some(EARLY), Some(0), Some(1));
            }
            t.present(QUEUE, present, 1000 + present as i64);
        }
        let row = backend.take_production_camera(present).unwrap_or_else(|e| {
            panic!("Present {present}, formal={formal}, accepting={accepting}: {e}")
        });
        if let Some(sample) = row {
            assert!(sample.depth_copied);
            let viewport = sample
                .viewport
                .expect("admitted draw viewport must reach the caller");
            assert_eq!(viewport.count, 1);
            assert_eq!(
                viewport.single_bits.unwrap().map(f32::from_bits),
                [0.0, 0.0, 1920.0, 1080.0, 0.0, 1.0]
            );
            let row = sample.row;
            assert_eq!(row.present_seq, present);
            depth_lab::validate_camera_pose_row(&row).unwrap();
            emitted += 1;
        } else {
            assert!(present < 121);
        }
    }
    assert_eq!(emitted, 30);
    // A genuinely absent copy still fails: never borrow the previous pose.
    begin_list(&backend, EARLY);
    observe_render_camera_draw(&backend, EARLY);
    execute(&backend, &[EARLY]);
    backend
        .render_camera
        .lock()
        .unwrap()
        .present(QUEUE, 151, 1151);
    assert!(backend.take_production_camera(151).is_err());
    backend.disarm_selected_depth_hot_path();
    assert!(!backend.selected_interval_capture_requested(false));
    let unrelated = D3d12ObservationBackend::new();
    assert!(!unrelated.selected_interval_capture_requested(false));
    assert!(unrelated.selected_interval_capture_requested(true));
}

#[test]
fn camera_sampling_recorded_late_pass_cannot_starve_precopy_list() {
    for same_batch in [false, true] {
        let backend = fixture();
        // The old before_present path replenishes just two global samples.
        backend
            .render_camera_draw_budget
            .store(2, Ordering::Release);
        // CPU records the late pass first; GPU submission below has the opposite order.
        for raw in [LATE, EARLY] {
            begin_list(&backend, raw);
            for _ in 0..2 {
                observe_render_camera_draw(&backend, raw);
            }
        }
        execute(&backend, if same_batch { &[EARLY, LATE] } else { &[EARLY] });
        backend
            .render_camera
            .lock()
            .unwrap()
            .mark_depth_copy_submitted_in_list(QUEUE, 1, Some(EARLY), Some(0), Some(2));
        if !same_batch {
            execute(&backend, &[LATE]);
        }
        let mut t = backend.render_camera.lock().unwrap();
        t.present(QUEUE, 1, 1000);
        let snapshot = t.snapshot();
        let samples = snapshot.local_camera_samples(1, 1920, 1080);
        assert!(samples.is_ok(), "same_batch={same_batch}: {samples:?}");
        assert_eq!(snapshot.observations.len(), 4);
        for draw in &snapshot.observations {
            assert_eq!(
                draw.selected_present_identity_passed,
                draw.command_list_tag == 9
            );
            assert_eq!(draw.copy_before_clear_submitted, draw.command_list_tag == 9);
        }
        assert_eq!(snapshot.draw_scan_budget_exhausted, 0);
    }
}

#[test]
fn camera_sampling_quota_is_bounded_to_recording_not_present_or_dsv_rebind() {
    let backend = fixture();
    begin_list(&backend, EARLY);
    for _ in 0..100 {
        observe_render_camera_draw(&backend, EARLY);
    }
    // Rebinding another pass in the same recording cannot reset its scan allowance.
    backend
        .hot_lists
        .bind_selected_depth(EARLY, backend.selected_depth_token.load(Ordering::Acquire));
    for _ in 0..100 {
        observe_render_camera_draw(&backend, EARLY);
    }
    // A CPU Present/budget update does not reopen the generation either.
    backend
        .render_camera_draw_budget
        .store(4096, Ordering::Release);
    observe_render_camera_draw(&backend, EARLY);
    let snapshot = backend.render_camera_snapshot();
    assert_eq!(
        snapshot.hook_cpu_timing["camera_discovery_draw_sampled"].calls,
        2
    );
    assert_eq!(
        snapshot.hook_cpu_timing["camera_discovery_list_budget_skipped"].calls,
        199
    );
    assert_eq!(snapshot.selected_depth_draw_calls, 2);
    assert_eq!(snapshot.draw_scan_budget_exhausted, 0);
    // Unrelated/inactive draws do not consume another recording's allowance.
    begin_list(&backend, LATE);
    backend.set_runtime_observation_active(false);
    observe_render_camera_draw(&backend, LATE);
    backend.set_runtime_observation_active(true);
    backend.hot_lists.unbind_depth(LATE);
    observe_render_camera_draw(&backend, LATE);
    backend
        .hot_lists
        .bind_selected_depth(LATE, backend.selected_depth_token.load(Ordering::Acquire));
    observe_render_camera_draw(&backend, LATE);
    assert_eq!(
        backend
            .camera_discovery_draw_sampled
            .load(Ordering::Relaxed),
        3
    );
    // Reset and release/re-register give new recording ownership without growing the table.
    begin_list(&backend, EARLY);
    observe_render_camera_draw(&backend, EARLY);
    backend.hot_lists.release(EARLY);
    backend.render_camera.lock().unwrap().release_list(EARLY);
    backend.hot_lists.register(EARLY, 19);
    backend
        .render_camera
        .lock()
        .unwrap()
        .register_list(EARLY, 19);
    begin_list(&backend, EARLY);
    observe_render_camera_draw(&backend, EARLY);
    assert_eq!(
        backend
            .camera_discovery_draw_sampled
            .load(Ordering::Relaxed),
        5
    );
    assert_eq!(backend.hot_lists.drops.load(Ordering::Relaxed), 0);
}

#[test]
fn camera_sampling_warms_and_continues_without_lending_camera_to_missing_present() {
    let backend = fixture();
    let mut views = Vec::new();
    let mut projections = Vec::new();
    let mut presents = BTreeSet::new();
    let mut camera = None;
    for present in 1..=130 {
        // An empty global quota cannot starve a freshly reset discovery list.
        backend
            .render_camera_draw_budget
            .store(0, Ordering::Release);
        for raw in [LATE, EARLY] {
            begin_list(&backend, raw);
            for _ in 0..20 {
                observe_render_camera_draw(&backend, raw);
            }
        }
        execute(&backend, &[EARLY]);
        backend
            .render_camera
            .lock()
            .unwrap()
            .mark_depth_copy_submitted_in_list(QUEUE, present, Some(EARLY), Some(0), Some(20));
        execute(&backend, &[LATE]);
        let mut t = backend.render_camera.lock().unwrap();
        t.present(QUEUE, present, 1000 + present as i64);
        let (v, p) = t.local_camera_samples(present, 1920, 1080).unwrap();
        if let Some(camera) = camera.as_mut() {
            depth_lab::d3d12_camera_local::LocalCamera::observe_present(camera, v, p).unwrap();
        } else {
            views.extend(v);
            projections.extend(p);
            presents.insert(present);
            if presents.len() == 120 {
                camera = Some(
                    depth_lab::d3d12_camera_local::admit(
                        std::mem::take(&mut views),
                        std::mem::take(&mut projections),
                        &presents,
                    )
                    .unwrap(),
                );
            }
        }
    }
    assert!(camera.is_some());
    // Next copied Present has only late draws: no reuse of the previously admitted camera.
    begin_list(&backend, EARLY);
    execute(&backend, &[EARLY]);
    backend
        .render_camera
        .lock()
        .unwrap()
        .mark_depth_copy_submitted_in_list(QUEUE, 131, Some(EARLY), Some(0), Some(0));
    begin_list(&backend, LATE);
    observe_render_camera_draw(&backend, LATE);
    execute(&backend, &[LATE]);
    let mut t = backend.render_camera.lock().unwrap();
    t.present(QUEUE, 131, 1131);
    assert_eq!(
        t.local_camera_samples(131, 1920, 1080).unwrap_err(),
        "local camera is absent at the exact copied Present"
    );
    assert_eq!(t.snapshot().draw_scan_budget_exhausted, 0);
}

#[test]
fn camera_sampling_depth_writer_policy_keeps_conflict_and_provenance_guards() {
    let b = fixture();
    begin_list(&b, EARLY);
    observe_render_camera_draw(&b, EARLY);
    execute(&b, &[EARLY]);
    let mut t = b.render_camera.lock().unwrap();
    t.mark_depth_copy_submitted_in_list(QUEUE, 1, Some(EARLY), Some(0), Some(20));
    t.present(QUEUE, 1, 1001);
    let mut base = t.snapshot_without_admission();
    let mut nonwriter = base.observations[0].clone();
    nonwriter.command_list_tag = 99;
    nonwriter.pipeline_depth_write = Some(false);
    nonwriter
        .pass_context
        .as_mut()
        .unwrap()
        .pipeline
        .as_mut()
        .unwrap()
        .depth_write_mask = 0;
    for m in &mut nonwriter.matrices {
        let r = m.copy_receipt.as_mut().unwrap();
        r.copy_submission = 999;
        r.copy_command.as_mut().unwrap().command_list_tag = 99;
        r.bytes_sha256 = format!("sha256:{}", "b".repeat(64));
    }
    base.observations.push(nonwriter);
    let (v, p) = base.local_camera_samples(1, 1920, 1080).unwrap();
    assert_eq!((v.len(), p.len()), (1, 1));
    assert_eq!(base.observations.len(), 2); // evidence is not deleted
    for mutation in 0..7 {
        let mut changed = base.clone();
        let d = &mut changed.observations[1];
        match mutation {
            0 => d.pass_context.as_mut().unwrap().pipeline = None,
            1 => d.matrices[0].matrix[0][0] += 1.0,
            2 => d.matrices[0].copy_receipt = None,
            3 => {
                d.pass_context
                    .as_mut()
                    .unwrap()
                    .viewport
                    .as_mut()
                    .unwrap()
                    .single_bits
                    .as_mut()
                    .unwrap()[4] = 0.9f32.to_bits()
            }
            4 => d.pipeline_depth_write = Some(true), // contradicts original mask
            5 => {
                changed.observations.remove(0);
            }
            _ => {
                // A non-writer cannot contradict the very same physical copy of a writer.
                let writer = changed.observations[0].clone();
                let d = &mut changed.observations[1];
                d.command_list_tag = writer.command_list_tag;
                for (m, w) in d.matrices.iter_mut().zip(writer.matrices) {
                    let hash = m.copy_receipt.as_ref().unwrap().bytes_sha256.clone();
                    m.copy_receipt = w.copy_receipt;
                    m.copy_receipt.as_mut().unwrap().bytes_sha256 = hash;
                }
            }
        }
        assert!(
            changed.local_camera_samples(1, 1920, 1080).is_err(),
            "mutation {mutation}"
        );
    }
    let mut writers = base.clone();
    let d = &mut writers.observations[1];
    d.pipeline_depth_write = Some(true);
    d.pass_context
        .as_mut()
        .unwrap()
        .pipeline
        .as_mut()
        .unwrap()
        .depth_write_mask = 1;
    let (v, _) = writers.local_camera_samples(1, 1920, 1080).unwrap();
    assert!(
        depth_lab::d3d12_camera_copy_series::resolve(v)
            .unwrap_err()
            .to_string()
            .contains("full CBV payloads disagree")
    );
}

//! Regression for a camera that reached Ready but lost its viewport before the first depth index.
use super::*;
use crate::camera_probe;
use crate::d3d12_render_camera::{ProductionCameraSample, draw_pass::ViewportObservation};
use depth_lab::capture_timing::{IndexProjection, ProjectionScope};
use depth_lab::decode::DepthFormat;

struct Cleanup;
impl Drop for Cleanup {
    fn drop(&mut self) {
        camera_probe::begin_stop_production();
        let _ = camera_probe::finish_stop_production();
        ACTIVE_PROJECTION.lock().unwrap().active = None;
        OBSERVED_DRAW_VIEWPORTS.lock().unwrap().clear();
        CAPTURE_PASS_EVIDENCE.lock().unwrap().clear();
    }
}

const TEST_DSV_ID: &str = "cp2077-holstered-d32s8-local-candidate";

#[test]
fn present_teardown_waits_for_the_admitted_callback_before_retiring_its_runtime() {
    let gate = std::sync::Arc::new(Mutex::new(()));
    let callback = gate.lock().unwrap();
    let worker_gate = gate.clone();
    let (entered_tx, entered_rx) = sync_channel(1);
    let (retired_tx, retired_rx) = sync_channel(1);
    let worker = std::thread::spawn(move || {
        entered_tx.send(()).unwrap();
        let _guard = lock_d3d12_teardown(&worker_gate, true);
        retired_tx.send(()).unwrap();
    });
    entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let early = retired_rx.recv_timeout(Duration::from_millis(50));
    drop(callback);
    if early.is_err() {
        retired_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    }
    worker.join().unwrap();
    assert!(
        matches!(early, Err(std::sync::mpsc::RecvTimeoutError::Timeout)),
        "teardown retired resources while a Present still owned the lifecycle lock"
    );
    // D3D11 keeps its existing lifecycle; this fix only quiesces the D3D12 path.
    let _held = gate.lock().unwrap();
    assert!(lock_d3d12_teardown(&gate, false).is_none());
}

#[test]
fn present_waiting_at_stop_rechecks_admission_before_touching_retired_state() {
    let gate = std::sync::Arc::new(Mutex::new(()));
    let retired = std::sync::Arc::new(AtomicBool::new(false));
    let teardown = gate.lock().unwrap();
    let worker_gate = gate.clone();
    let worker_retired = retired.clone();
    let (checked_tx, checked_rx) = sync_channel(1);
    let worker = std::thread::spawn(move || {
        let first = AtomicBool::new(true);
        lock_present_observation(&worker_gate, || {
            let bypass = worker_retired.load(Ordering::Acquire);
            if first.swap(false, Ordering::AcqRel) {
                checked_tx.send(()).unwrap();
            }
            bypass
        })
        .is_some()
    });
    checked_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    // The callback passed its fast check but is waiting for teardown's lock.
    retired.store(true, Ordering::Release);
    drop(teardown);
    assert!(
        !worker.join().unwrap(),
        "late Present re-entered a retired runtime"
    );
    // Active recording/discovery still enters; already-retired callbacks bypass the lock.
    assert!(lock_present_observation(&gate, || false).is_some());
    let _held = gate.lock().unwrap();
    assert!(lock_present_observation(&gate, || true).is_none());
}

#[test]
fn stopped_present_callback_cannot_resume_into_the_next_generation() {
    let gate = std::sync::Arc::new(Mutex::new(()));
    let generation = std::sync::Arc::new(AtomicU64::new(1));
    let lifecycle_guard = gate.lock().unwrap();
    let (entered_tx, entered_rx) = sync_channel(1);
    let worker_gate = gate.clone();
    let worker_generation = generation.clone();
    let worker = std::thread::spawn(move || {
        entered_tx.send(()).unwrap();
        lock_current_present_completion(&worker_gate, &worker_generation, 1).is_some()
    });
    entered_rx.recv().unwrap();
    // While the old Present is returning, StopRun/StartRun replaces generation 1.
    generation.store(2, Ordering::Release);
    drop(lifecycle_guard);
    assert!(!worker.join().unwrap());
    assert!(lock_current_present_completion(&gate, &generation, 2).is_some());
    generation.store(0, Ordering::Release);
    assert!(lock_current_present_completion(&gate, &generation, 2).is_none());
}

fn queue_synthetic_readback(
    generation: u64,
    seq: u64,
    projection: IndexProjection,
    tx: &SyncSender<DepthWriterCommand>,
) {
    let pass = depth_lab::capture_timing::DsvPassEvidence {
        unbind_ordinal_from_end: 1,
        unbind_total: 1,
        draw_calls: 1,
        selection: depth_lab::DsvSelectionAudit {
            selected_id: TEST_DSV_ID.into(),
            ranked_top_id: Some(TEST_DSV_ID.into()),
            score_margin: None,
            selected_matches_top: true,
            ranking_confident: true,
            any_msaa: false,
            operator_exact_pin: true,
        },
    };
    CAPTURE_PASS_EVIDENCE.lock().unwrap().insert(
        seq,
        PendingCaptureEvidence {
            pass,
            projection: Some(projection),
            identity_trace: None,
        },
    );
    // GPU completion is the synthetic boundary. From here use the real queue,
    // decoder, EXR writer, index and Stop FIFO, with the catalog's delivery size.
    let bytes = (0..1280 * 720)
        .flat_map(|i| (0.01_f32 + (i % 1000) as f32 * 0.0001).to_le_bytes())
        .collect();
    queue_ready_depth_frames(
        vec![RawDepthFrame {
            present_seq: seq,
            src_width: 1280,
            src_height: 720,
            row_pitch: 1280 * 4,
            format: DepthFormat::D32FloatS8x24,
            dst_width: 1280,
            dst_height: 720,
            conv: ZConvention::Reversed,
            bytes,
            pooled: true,
            verify_pooled: None,
            verify_row_pitch: 0,
        }],
        seq + 2,
        generation,
        tx,
    )
    .unwrap();
}

#[test]
fn production_camera_viewport_reaches_exact_depth_index_across_generations() {
    let _guard = camera_probe::tests::CAMERA_TEST_LOCK.lock().unwrap();
    let _cleanup = Cleanup;
    let _ = camera_probe::finish_stop_production();
    let binding: depth_lab::D3d12CameraBinding = serde_json::from_str(include_str!(
        "../../../../config/catalogs/dev/cyberpunk-2077-steam/camera-binding.json"
    ))
    .unwrap();
    let profile = load_depth_profile(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../config/catalogs/dev/cyberpunk-2077-steam/depth-profile.json"),
        None,
    )
    .unwrap();
    let projection = [
        [1.2891924, 0., 0., 0.],
        [0., 2.2918978, 0., 0.],
        [-0.00034243267, 0.00044058342, -0.0000011920929, 1.],
        [0., 0., 0.020000024, 0.],
    ];
    let view = [
        [1., 0., 0., 0.],
        [0., -1., 0., 0.],
        [0., 0., 1., 0.],
        [-1., -2., -3., 1.],
    ];
    let observed = depth_lab::camera_track::projection_from_matrix(&projection).unwrap();
    let viewport = |min: f32, max: f32| ViewportObservation {
        count: 1,
        single_bits: Some([0., 0., 1920., 1080., min, max].map(f32::to_bits)),
    };
    for generation in [91, 92] {
        let root = std::env::temp_dir().join(format!("dm-viewport-{}", uuid::Uuid::new_v4()));
        let context = depth_lab::CameraPoseStreamContext {
            identity: depth_lab::StreamIdentity {
                session_id: "viewport-regression".into(),
                run_id: format!("run-{generation}"),
                generation,
            },
            dsv_id: TEST_DSV_ID.into(),
        };
        ACTIVE_PROJECTION
            .lock()
            .unwrap()
            .start(generation, Some(profile), false)
            .unwrap();
        camera_probe::init_d3d12_production_camera_only(
            root.clone(),
            binding.clone(),
            context.clone(),
            &binding.executable_sha256,
        )
        .unwrap();
        let (depth_tx, depth_rx) = sync_channel(8);
        let worker = std::thread::spawn(move || depth_writer_loop(depth_rx));
        let (start_tx, start_rx) = sync_channel(1);
        depth_tx
            .send(DepthWriterCommand::Start {
                generation,
                root: root.clone(),
                projection: profile.index,
                d3d12_dsv_id: Some(TEST_DSV_ID.into()),
                pass_identity_trace: false,
                reply: start_tx,
            })
            .unwrap();
        start_rx.recv().unwrap().unwrap();
        let sample = |seq, viewport| ProductionCameraSample {
            row: depth_lab::d3d12_camera::assemble_pose(
                &binding,
                &context,
                seq,
                seq as i64 * 100,
                view,
                projection,
            )
            .unwrap(),
            depth_copied: true,
            viewport,
        };
        camera_probe::begin_production_warmup(generation).unwrap();
        assert!(
            !submit_d3d12_camera_sample(sample(10, Some(viewport(0., 1.))), true, true).unwrap()
        );
        assert!(camera_probe::warmup_present_ready(generation, 10));
        camera_probe::arm_production(generation).unwrap();
        for seq in 11..140 {
            assert!(
                submit_d3d12_camera_sample(sample(seq, Some(viewport(0., 1.))), true, false)
                    .unwrap()
            );
        }
        assert!(OBSERVED_DRAW_VIEWPORTS.lock().unwrap().is_empty());
        // The first actual selected depth must go through the same consumer as the GPU drain.
        assert!(
            submit_d3d12_camera_sample(sample(140, Some(viewport(0., 1.))), true, true).unwrap()
        );
        let index = observed_index_projection(generation, 140).unwrap();
        assert_eq!(
            index,
            IndexProjection::observed_with_viewport(observed, Some(1.), 0., 1.)
        );
        assert_eq!(index.proj_scope, ProjectionScope::ObservedThisFrame);
        assert!(observed_index_projection(generation, 140).is_err()); // consumed once
        assert!(take_exact_draw_viewport(generation, 140).is_err());
        queue_synthetic_readback(generation, 140, index, &depth_tx);

        // Synthetic common-path viewport: prove the handoff carries observed bits, not 0..1 defaults.
        assert!(
            submit_d3d12_camera_sample(sample(141, Some(viewport(0.25, 0.75))), true, true)
                .unwrap()
        );
        let index = observed_index_projection(generation, 141).unwrap();
        assert_eq!(index.viewport_min_depth, Some(0.25));
        assert_eq!(index.viewport_max_depth, Some(0.75));
        assert_eq!(index.proj_a, Some(0.25 + 0.5 * observed.a));
        assert_eq!(index.proj_b, Some(0.5 * observed.b));
        queue_synthetic_readback(generation, 141, index, &depth_tx);

        // Keep one accepted frame pending while feedback seals; StopRun must still drain it.
        assert!(
            submit_d3d12_camera_sample(sample(142, Some(viewport(0., 1.))), true, true).unwrap()
        );
        assert!(
            submit_d3d12_camera_sample(sample(143, Some(viewport(0., 1.))), true, false).unwrap()
        );
        let mut stale = sample(144, Some(viewport(0., 1.)));
        stale.row.identity.generation -= 1;
        assert!(submit_d3d12_camera_sample(stale, true, true).is_err());
        assert!(take_exact_draw_viewport(generation, 144).is_err());
        camera_probe::begin_stop_production();
        let tail = observed_index_projection(generation, 142).unwrap();
        queue_synthetic_readback(generation, 142, tail, &depth_tx);
        assert!(observed_index_projection(generation, 143).is_err());
        assert!(take_exact_draw_viewport(generation, 143).is_err());
        let summary = camera_probe::finish_stop_production();
        assert!(summary.errors.is_empty(), "{:?}", summary.errors);
        assert_eq!(summary.pose_row_count, 133);
        assert_eq!(summary.player_pose_row_count, None);
        assert_eq!(summary.last_present_seq, Some(143));
        let (stop_tx, stop_rx) = sync_channel(1);
        depth_tx
            .send(DepthWriterCommand::Stop {
                generation,
                reply: stop_tx,
            })
            .unwrap();
        let depth_summary = stop_rx.recv().unwrap().unwrap();
        assert!(
            depth_summary.errors.is_empty(),
            "{:?}",
            depth_summary.errors
        );
        assert_eq!(depth_summary.row_count, 3);
        assert_eq!(depth_summary.file_count, 3);
        assert_eq!(depth_summary.last_depth_present_seq, Some(142));
        let index_bytes = std::fs::read(root.join("depth-index.jsonl")).unwrap();
        let rows: Vec<depth_lab::capture_timing::DepthIndexEntry> =
            std::str::from_utf8(&index_bytes)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
        assert_eq!(
            rows.iter().map(|row| row.present_seq).collect::<Vec<_>>(),
            vec![140, 141, 142]
        );
        for row in &rows {
            assert_eq!(row.dsv_id, TEST_DSV_ID);
            assert_eq!(row.schema, depth_lab::capture_timing::INDEX_SCHEMA_V6);
            assert_eq!((row.width, row.height), (1280, 720));
            assert_eq!(
                row.projection.proj_scope,
                ProjectionScope::ObservedThisFrame
            );
            assert!(row.health.is_some());
            assert!(root.join(&row.path).is_file());
        }
        assert!(CAPTURE_PASS_EVIDENCE.lock().unwrap().is_empty());
        drop(depth_tx);
        worker.join().unwrap();
        assert_eq!(
            std::fs::read(root.join("depth-index.jsonl")).unwrap(),
            index_bytes
        );
        stop_active_projection(generation).unwrap();
        assert!(OBSERVED_DRAW_VIEWPORTS.lock().unwrap().is_empty());
    }

    // Missing/foreign identities and malformed observations retain the existing fail-closed gates.
    record_external_draw_viewport(93, 150, 1, 0., 1.).unwrap();
    assert!(
        take_exact_draw_viewport(93, 151)
            .unwrap_err()
            .contains("no same-Present")
    );
    assert!(
        take_exact_draw_viewport(94, 150)
            .unwrap_err()
            .contains("generation mismatch")
    );
    for (count, min, max) in [(2, 0., 1.), (1, f64::NAN, 1.), (1, 0.8, 0.2), (1, 0., 1.1)] {
        assert!(record_external_draw_viewport(93, 150, count, min, max).is_err());
        assert!(take_exact_draw_viewport(93, 150).is_err());
    }
}

#[test]
fn ready_requires_the_same_viewport_and_profile_evidence_as_recording() {
    let _guard = camera_probe::tests::CAMERA_TEST_LOCK.lock().unwrap();
    let _cleanup = Cleanup;
    let _ = camera_probe::finish_stop_production();
    let binding: depth_lab::D3d12CameraBinding = serde_json::from_str(include_str!(
        "../../../../config/catalogs/dev/cyberpunk-2077-steam/camera-binding.json"
    ))
    .unwrap();
    let profile = load_depth_profile(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../config/catalogs/dev/cyberpunk-2077-steam/depth-profile.json"),
        None,
    )
    .unwrap();
    for (case, expected) in [
        (0, "viewport"),
        (1, "projection family mismatch"),
        (2, "metric scale"),
    ] {
        let generation = 100 + case;
        let root = std::env::temp_dir().join(format!("dm-ready-evidence-{}", uuid::Uuid::new_v4()));
        let context = depth_lab::CameraPoseStreamContext {
            identity: depth_lab::StreamIdentity {
                session_id: "ready-regression".into(),
                run_id: format!("run-{generation}"),
                generation,
            },
            dsv_id: TEST_DSV_ID.into(),
        };
        let mut reference = profile;
        if case == 2 {
            reference.index.metric_scale = None;
        }
        ACTIVE_PROJECTION
            .lock()
            .unwrap()
            .start(generation, Some(reference), false)
            .unwrap();
        camera_probe::init_d3d12_production_camera_only(
            root,
            binding.clone(),
            context.clone(),
            &binding.executable_sha256,
        )
        .unwrap();
        camera_probe::begin_production_warmup(generation).unwrap();
        let projection = [
            [1.2891924, 0., 0., 0.],
            [0., 2.2918978, 0., 0.],
            [0., 0., -0.0000011920929, 1.],
            [0., 0., if case == 1 { 0.04 } else { 0.020000024 }, 0.],
        ];
        let view = [
            [1., 0., 0., 0.],
            [0., -1., 0., 0.],
            [0., 0., 1., 0.],
            [-1., -2., -3., 1.],
        ];
        let row =
            depth_lab::d3d12_camera::assemble_pose(&binding, &context, 10, 1000, view, projection)
                .unwrap();
        let error = submit_d3d12_camera_sample(
            ProductionCameraSample {
                row,
                depth_copied: true,
                viewport: (case != 0).then_some(ViewportObservation {
                    count: 1,
                    single_bits: Some([0., 0., 1920., 1080., 0., 1.].map(f32::to_bits)),
                }),
            },
            true,
            false,
        )
        .unwrap_err();
        assert!(error.contains(expected), "{error}");
        assert!(!camera_probe::warmup_present_ready(generation, 10));
        assert!(OBSERVED_DRAW_VIEWPORTS.lock().unwrap().is_empty());
        camera_probe::begin_stop_production();
        let summary = camera_probe::finish_stop_production();
        assert_eq!(summary.pose_row_count, 0);
        stop_active_projection(generation).unwrap();
    }
}

use super::*;
use crate::d3d12_render_camera::D3d12RenderMatrixObservation;

fn projection(a: f32, b: f32) -> D3d12RenderMatrixObservation {
    let mut matrix = [[0.0; 4]; 4];
    matrix[0][0] = 1.0;
    matrix[1][1] = 1.0;
    matrix[2][2] = a;
    matrix[3][2] = b;
    D3d12RenderMatrixObservation {
        copy_receipt: None,
        source_tag: 1,
        root_parameter_index: 4,
        cbv_resource_offset: 256,
        matrix_offset: 128,
        transpose: true,
        matrix_kind: "projection".into(),
        matrix,
        translation_slot: None,
        basis_determinant: None,
        clip_w_sign: None,
        projection_a: None,
        projection_b: None,
        vertical_fov_degrees: None,
        horizontal_fov_degrees: None,
        configured_vertical_fov_matches: None,
        handedness_observation: String::new(),
        axis_mapping: String::new(),
        physical_source: String::new(),
        stable_source: None,
    }
}

fn draw(a: f32) -> D3d12RenderDrawObservation {
    D3d12RenderDrawObservation {
        present_seq: 42,
        present_qpc: 42000,
        queue_tag: 1,
        command_list_tag: 2,
        command_list_generation: 3,
        execute_batch: 4,
        draw_ordinal: 5,
        pass_ordinal: Some(0),
        draw_ordinal_in_pass: Some(1),
        pipeline_depth_enable: Some(true),
        pipeline_depth_write: Some(true),
        matrices: vec![projection(a, 0.02)],
        ..Default::default()
    }
}

#[test]
fn projection_families_keep_second_list_and_deweighted_draw_counts() {
    let mut row = FamilyFrame::new(42, 42000);
    let a = draw(-5.00679e-6);
    let mut b = draw(-1.192_092_9e-6);
    b.command_list_tag = 9;
    row.observe(&a, Some(23));
    row.observe(&b, Some(7));
    assert_eq!(row.families.len(), 2);
    assert_eq!(row.representative_draws, 2);
    assert_eq!(row.represented_draws, 30);
    assert_eq!(row.families[0].represented_draws, 23);
    assert_eq!(row.families[1].represented_draws, 7);
    assert_eq!(row.families[1].locations[0].command_list_tag, 9);
    // The materialized matrix is already canonical: transpose=true is provenance, not a
    // request to transpose again. m32 must remain the original canonical value.
    assert_eq!(row.families[0].m32, 0.02);
}

#[test]
fn aliases_count_one_draw_but_conflicting_aliases_preserve_both_families() {
    let mut row = FamilyFrame::new(42, 42000);
    let mut a = draw(-5.00679e-6);
    a.matrices.push(a.matrices[0].clone());
    row.observe(&a, Some(10));
    assert_eq!(row.families.len(), 1);
    assert_eq!(row.families[0].represented_draws, 10);
    assert_eq!(row.alias_conflict_representatives, 0);
    a.matrices.push(projection(-1.192_092_9e-6, 0.02));
    row.observe(&a, Some(1));
    assert_eq!(row.alias_conflict_representatives, 1);
    assert_eq!(row.represented_draws, 11);
    assert_eq!(row.families[0].represented_draws, 11);
    assert_eq!(row.families[1].represented_draws, 1);
}

#[test]
fn pso_unknown_nonwriting_and_missing_projection_are_not_single_family_evidence() {
    let mut row = FamilyFrame::new(42, 42000);
    let mut a = draw(-5.00679e-6);
    a.pipeline_depth_write = Some(false);
    row.observe(&a, Some(5));
    a.pipeline_depth_write = None;
    row.observe(&a, Some(2));
    a.pipeline_depth_write = Some(true);
    a.matrices.clear();
    row.observe(&a, None);
    assert!(row.families.is_empty());
    assert_eq!(row.nonwriting_representatives, 1);
    assert_eq!(row.unknown_pso_representatives, 1);
    assert_eq!(row.missing_projection_representatives, 1);
    assert_eq!(row.missing_weight_representatives, 1);
    assert_eq!(row.represented_draws, 8);
}

#[test]
fn sequence_mismatch_and_nonfinite_projection_cannot_silently_disappear() {
    let mut row = FamilyFrame::new(42, 42000);
    let mut a = draw(f32::NAN);
    row.observe(&a, Some(1));
    assert_eq!(row.missing_projection_representatives, 1);
    a.present_seq += 1;
    row.observe(&a, Some(1));
    assert!(row.tracker_loss_observed);
    assert_eq!(row.representative_draws, 1);
}

#[test]
fn copy_boundary_and_list_generations_remain_distinct_locations() {
    let mut row = FamilyFrame::new(42, 42000);
    let mut a = draw(-5.00679e-6);
    row.observe(&a, Some(2));
    a.copy_before_clear_submitted = true;
    row.observe(&a, Some(3));
    a.command_list_generation += 1;
    row.observe(&a, Some(4));
    assert_eq!(row.families.len(), 1);
    assert_eq!(row.families[0].locations.len(), 3);
    assert_eq!(row.families[0].represented_draws, 9);
}

#[test]
fn bounded_locations_and_families_are_explicitly_incomplete() {
    let mut row = FamilyFrame::new(42, 42000);
    for list in 0..MAX_LOCATIONS + 1 {
        let mut a = draw(-5.00679e-6);
        a.command_list_tag = list as u64;
        row.observe(&a, Some(1));
    }
    assert_eq!(row.location_count, MAX_LOCATIONS);
    assert!(row.bounded_overflow);
    assert_eq!(row.represented_draws, MAX_LOCATIONS as u64 + 1);
    let mut row = FamilyFrame::new(42, 42000);
    for family in 0..MAX_FAMILIES + 1 {
        row.observe(&draw(-(family as f32 + 1.0) * 1e-6), Some(1));
    }
    assert_eq!(row.families.len(), MAX_FAMILIES);
    assert!(row.bounded_overflow);
}

#[test]
fn memory_json_contract_is_flat_and_unavailable_never_carries_stale_pose() {
    let row = MemoryRow {
        schema: "dm-cp2077-memory-camera/v1",
        generation: 7,
        present_seq: 42,
        qpc: 42000,
        read_begin_qpc: 42001,
        read_end_qpc: 42002,
        sample_phase: "after_render_tracker_present_before_original_present",
        source_chain_id: memory::CHAIN_ID,
        sample: memory::MemorySample::unavailable("chain changed"),
    };
    let mut bytes = Vec::new();
    write_line(&mut bytes, &row).unwrap();
    assert_eq!(bytes.last(), Some(&b'\n'));
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["present_seq"], 42);
    assert_eq!(value["status"], "unavailable");
    assert!(value["position"].is_null());
    assert!(value["rotation"].is_null());
    assert!(value["fov_deg"].is_null());
    assert!(value.get("sample").is_none());
}

#[test]
fn bounded_queue_refuses_instead_of_waiting_or_changing_production() {
    let (tx, _rx) = sync_channel(1);
    assert!(tx.try_send(Event::Family(FamilyFrame::new(1, 1))).is_ok());
    assert!(matches!(
        tx.try_send(Event::Family(FamilyFrame::new(2, 2))),
        Err(std::sync::mpsc::TrySendError::Full(_))
    ));
}

#[test]
fn sidecars_drain_with_missing_memory_chain_and_refuse_overwrite() {
    // Tracker tests can issue synthetic Presents without the camera suite mutex. Run this
    // one lifecycle/IO test in an isolated harness so activating the diagnostic singleton
    // cannot collect those unrelated fixtures during a normal parallel `cargo test`.
    const CHILD: &str = "DM_TEST_CP2077_CAMERA_SIDECAR_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "cp2077_camera_diagnostics::tests::sidecars_drain_with_missing_memory_chain_and_refuse_overwrite", "--test-threads=1"])
            .env(CHILD, "1").output().unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    // This test touches the shared camera lifecycle, so take the existing suite guard.
    let _guard = crate::camera_probe::tests::CAMERA_TEST_LOCK.lock().unwrap();
    let binding: depth_lab::D3d12CameraBinding = serde_json::from_str(include_str!(
        "../../../../config/catalogs/dev/cyberpunk-2077-steam/camera-binding.json"
    ))
    .unwrap();
    let root = std::env::temp_dir().join(format!(
        "dm-cp2077-camera-diagnostic-test-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir(&root).unwrap();
    let identity = depth_lab::StreamIdentity {
        session_id: "synthetic-diagnostic-test".into(),
        run_id: "r0".into(),
        generation: 0,
    };
    start(&root, identity.clone(), &binding, None).unwrap();
    assert!(active());
    let weight = DrawWeight::new().unwrap();
    weight.increment();
    assert_eq!(weight.snapshot(), Some(2));
    submit_family({
        let mut row = FamilyFrame::new(42, 42000);
        row.observe(&draw(-5.00679e-6), weight.snapshot());
        row
    });
    sample_memory(42, 42000);
    finish();
    assert!(!active());
    assert_eq!(weight.snapshot(), None);
    let summary: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join(SUMMARY_FILE)).unwrap()).unwrap();
    assert_eq!(summary["family_present_rows"], 1);
    assert_eq!(summary["memory_rows"], 1);
    assert_eq!(summary["multiple_family_presents"], 0);
    assert_eq!(summary["diagnostic_losses"], 0);
    assert!(summary["io_error"].is_null());
    let memory_file = std::fs::read_to_string(root.join(MEMORY_FILE)).unwrap();
    let memory: serde_json::Value =
        serde_json::from_str(memory_file.lines().nth(1).unwrap()).unwrap();
    assert_eq!(memory["status"], "unavailable");
    assert!(memory["position"].is_null());
    assert!(start(&root, identity.clone(), &binding, None).is_err());
    assert_eq!(
        std::fs::read_to_string(root.join(MEMORY_FILE)).unwrap(),
        memory_file
    );
    let resumed = root.join("resumed");
    std::fs::create_dir(&resumed).unwrap();
    start(&resumed, identity, &binding, None).unwrap();
    // Even a caller reusing generation zero cannot revive an old diagnostic run's weight.
    assert_eq!(weight.snapshot(), None);
    weight.increment();
    assert_eq!(DrawWeight::new().unwrap().snapshot(), Some(1));
    sample_memory(42, 42000);
    sample_memory(42, 42000);
    let mut family = FamilyFrame::new(42, 42000);
    family.observe(&draw(-5.00679e-6), Some(1));
    family.observe(&draw(-1.192_092_9e-6), Some(1));
    submit_family(family);
    finish();
    let summary: serde_json::Value =
        serde_json::from_slice(&std::fs::read(resumed.join(SUMMARY_FILE)).unwrap()).unwrap();
    assert_eq!(summary["memory_rows"], 1);
    assert_eq!(summary["multiple_family_presents"], 1);
    assert_eq!(summary["multiple_family_fraction"], 1.0);
    assert_eq!(summary["diagnostic_losses"], 2);
    // Only this UUID-named directory created above belongs to this test.
    std::fs::remove_dir_all(root).unwrap();
}

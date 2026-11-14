use super::*;

fn token() -> SampleToken {
    SampleToken {
        run: 1,
        generation: 7,
        sample_id: 9,
        queue_tag: 5,
        begin: 30,
    }
}
fn stamp() -> SnapshotTiming {
    SnapshotTiming {
        run: 1,
        begin: 10,
        end: 20,
    }
}
fn receipt(id: u64) -> CopyReceipt {
    CopyReceipt {
        schema: "dm-d3d12-executed-cbv-copy/v2".into(),
        copy_submission: id,
        queue_tag: 5,
        destination_source_tag: 3,
        destination_content_generation: 4,
        destination_offset: 0,
        source_source_tag: 1,
        source_content_generation: 2,
        source_offset: 256,
        descriptor_generation: 6,
        copied_bytes: 256,
        bytes_sha256: format!("sha256:{}", "0".repeat(64)),
        copy_command: Some(depth_lab::d3d12_camera_alias::CopyCommandIdentity {
            command_list_tag: 11,
            command_list_generation: 12,
            copy_ordinal: 1,
        }),
    }
}
fn draw(receipt: CopyReceipt) -> D3d12RenderDrawObservation {
    let matrix = serde_json::from_value(serde_json::json!({
        "copy_receipt":receipt,"source_tag":1,"root_parameter_index":0,"cbv_resource_offset":0,
        "matrix_offset":128,"transpose":false,"matrix_kind":"projection","matrix":([[0.;4];4]),
        "handedness_observation":"test","axis_mapping":"test","physical_source":"test"
    }))
    .unwrap();
    D3d12RenderDrawObservation {
        present_seq: 42,
        present_qpc: 42000,
        queue_tag: 5,
        pipeline_depth_write: Some(true),
        copy_before_clear_submitted: true,
        matrices: vec![matrix],
        ..Default::default()
    }
}

#[test]
fn bindings_keep_alias_receipts_without_rereading_or_inventing_a_present() {
    let mut batch = Bindings::default();
    let a = receipt(80);
    let mut b = a.clone();
    b.descriptor_generation += 1;
    batch.observe(Some(token()), Some(stamp()), &a);
    batch.observe(Some(token()), Some(stamp()), &b);
    assert_eq!(batch.copies.len(), 2);
    assert_eq!(
        batch.copies[0].copy_receipt.copy_submission,
        batch.copies[1].copy_receipt.copy_submission
    );
    assert_eq!(
        batch.copies[0].snapshot_read_begin_qpc,
        batch.copies[1].snapshot_read_begin_qpc
    );
    let json = serde_json::to_value(batch).unwrap();
    assert!(json.get("present_seq").is_none());
    assert!(json.get("token").is_none());
    assert_eq!(json["generation"], 7);
    assert_eq!(json["sample_id"], 9);
}

#[test]
fn bindings_refuse_cross_run_queue_sample_and_reversed_time() {
    let mut batch = Bindings::default();
    batch.observe(Some(token()), Some(stamp()), &receipt(1));
    let mut old = stamp();
    old.run = 2;
    batch.observe(Some(token()), Some(old), &receipt(2));
    let mut wrong_queue = receipt(3);
    wrong_queue.queue_tag = 6;
    batch.observe(Some(token()), Some(stamp()), &wrong_queue);
    let mut later = stamp();
    later.end = 31;
    batch.observe(Some(token()), Some(later), &receipt(4));
    let mut other = token();
    other.sample_id = 10;
    batch.observe(Some(other), Some(stamp()), &receipt(5));
    assert_eq!(batch.copies.len(), 1);
    assert_eq!(batch.omitted_bindings, 4);
    for id in 1..=MAX_COPIES {
        batch.observe(Some(token()), Some(stamp()), &receipt(id as u64));
    }
    assert_eq!(batch.copies.len(), MAX_COPIES);
    assert_eq!(batch.omitted_bindings, 5);
}

#[test]
fn links_are_actual_selected_depth_writing_uses_with_bounded_unique_copy_ids() {
    let mut links = PresentLinks::default();
    let mut d = draw(receipt(1));
    links.observe(&d);
    links.observe(&d);
    assert_eq!(links.copies.len(), 1);
    d.matrices[0].copy_receipt = Some(receipt(2));
    d.pipeline_depth_write = Some(false);
    links.observe(&d);
    d.pipeline_depth_write = None;
    links.observe(&d);
    d.pipeline_depth_write = Some(true);
    d.copy_before_clear_submitted = false;
    links.observe(&d);
    assert_eq!(links.copies.len(), 1);
    d.copy_before_clear_submitted = true;
    d.queue_tag = 6;
    links.observe(&d);
    assert_eq!(links.omitted_references, 1);
    d.queue_tag = 5;
    for id in 2..=MAX_COPIES + 1 {
        d.matrices[0].copy_receipt = Some(receipt(id as u64));
        links.observe(&d);
    }
    assert_eq!(links.copies.len(), MAX_COPIES);
    assert_eq!(links.omitted_references, 2);
}

#[test]
fn native_fingerprint_is_worker_only_and_no_pose_or_tick_is_fabricated_on_failure() {
    let mut row = SampleRow {
        schema: "dm-cp2077-player-cbv-sample/v1",
        generation: 7,
        sample_id: 9,
        queue_tag: 5,
        present_seq: None,
        engine_state_frame: None,
        sample_phase: "test",
        source_chain_id: "test",
        sample_begin_qpc: 30,
        sample_end_qpc: 40,
        snapshot_batch_read_begin_qpc: 10,
        snapshot_batch_read_end_qpc: 20,
        prepared_binding_count: 2,
        status: "unavailable",
        reason: Some("no audit".into()),
        position: None,
        rotation: None,
        stable_read_attempts: None,
        audit_qpc: None,
        native_transform_sha256: None,
        native_words: None,
    };
    row.fingerprint();
    let j = serde_json::to_value(&row).unwrap();
    for key in [
        "position",
        "rotation",
        "present_seq",
        "engine_state_frame",
        "native_transform_sha256",
    ] {
        assert!(j[key].is_null());
    }
    assert!(j.get("native_words").is_none());
    row.native_words = Some([0; 8]);
    row.fingerprint();
    assert_eq!(
        row.native_transform_sha256,
        Some(format!("sha256:{:x}", Sha256::digest([0u8; 32])))
    );
    assert!(row.native_words.is_none());
}

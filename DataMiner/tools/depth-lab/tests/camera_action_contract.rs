use std::fs;
use std::path::Path;

use depth_lab::pairing::FramePair;
use depth_lab::pipeline::DepthIndexRow;
use depth_lab::{
    ActionFrameRow, CameraActionJoinError, CameraActionJoinInputs, CameraPoseRow,
    FinalCameraActionInputs, IdentifiedRows, MouseNormalization, PLAYER_POSE_INDEX_SCHEMA_V1,
    PlayerPoseRow, StreamIdentity, assemble_final_pose_rows, join_camera_action,
};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Fixture {
    depth_identity: StreamIdentity,
    camera_identity: StreamIdentity,
    action_identity: StreamIdentity,
    pair_identity: StreamIdentity,
    depth: Vec<DepthIndexRow>,
    camera: Vec<CameraPoseRow>,
    actions: Vec<ActionFrameRow>,
    frame_pairs: Vec<FramePair>,
}

fn fixture() -> Fixture {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/camera-action-join-v1.json");
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

fn join(value: &Fixture) -> Result<Vec<depth_lab::JoinedCameraActionRow>, CameraActionJoinError> {
    join_camera_action(CameraActionJoinInputs {
        depth: IdentifiedRows::new(&value.depth_identity, &value.depth),
        camera: IdentifiedRows::new(&value.camera_identity, &value.camera),
        actions: IdentifiedRows::new(&value.action_identity, &value.actions),
        frame_pairs: IdentifiedRows::new(&value.pair_identity, &value.frame_pairs),
    })
}

#[test]
fn successful_join_is_rate_neutral_and_uses_depth_rows() {
    let value = fixture();
    let rows = join(&value).unwrap();

    assert_eq!(rows.len(), value.depth.len());
    assert_eq!(rows[0].present_seq, 100);
    assert_eq!(rows[0].session_frame, 200);
    assert_eq!(rows[1].present_seq, 102);
    assert_eq!(rows[1].session_frame, 202);
    assert_eq!(rows[1].camera_position, [10.2, 2.0, -5.0]);
}

#[test]
fn kcd2_camera_join_requires_the_same_exact_dsv_identity_as_depth() {
    let mut value = fixture();
    for camera in &mut value.camera {
        camera.source = depth_lab::CameraPoseSource::ResolvedCombinedViewProjection;
        camera.provenance = depth_lab::CameraPoseProvenance::ResolvedCombinedViewProjection {
            binding_id: depth_lab::KCD2_COMBINED_BINDING_ID.into(),
            binding_version: depth_lab::KCD2_COMBINED_BINDING_VERSION.into(),
            profile_id: depth_lab::KCD2_MAINLINE_GAME_ID.into(),
            profile_version: depth_lab::DepthProfile::D3D12_STABLE_SCHEMA_VERSION.into(),
            executable_sha256: depth_lab::KCD2_EXECUTABLE_SHA256.into(),
            dsv_id: depth_lab::KCD2_MAIN_SCENE_DSV_ID.into(),
            render_width: 1920,
            render_height: 1080,
            view_metres_per_unit: "1.0".into(),
            axis_mapping: depth_lab::D3d12AxisMapping {
                delivery_x_from: depth_lab::D3d12SignedAxis::PositiveX,
                delivery_y_from: depth_lab::D3d12SignedAxis::PositiveZ,
                delivery_z_from: depth_lab::D3d12SignedAxis::PositiveY,
            },
            source_qpc: camera.present_qpc,
            root_signature_sha256: format!(
                "sha256:{}",
                depth_lab::KCD2_VIEW_PROJECTION_ROOT_SIGNATURE_SHA256
            ),
            root_parameter_index: 2,
            pass_ordinal: 0,
            draw_ordinal: 0,
            projection_a: depth_lab::KCD2_PROJECTION_A,
            projection_b: depth_lab::KCD2_PROJECTION_B,
            projection_decomposition_residual: 0.0,
            held_out_depth_row_residual: 0.0,
        };
    }
    for depth in &mut value.depth {
        depth.dsv_id = depth_lab::KCD2_MAIN_SCENE_DSV_ID.into();
    }
    for pair in &mut value.frame_pairs {
        if let Some(depth) = &mut pair.depth {
            depth.dsv_id = depth_lab::KCD2_MAIN_SCENE_DSV_ID.into();
        }
    }
    assert_eq!(join(&value).unwrap().len(), value.depth.len());

    // Reproduce the old writer's generic alias while leaving the camera's
    // exact source intact. The join must reject it rather than relabel evidence.
    for depth in &mut value.depth {
        depth.dsv_id = "main-scene-v1".into();
    }
    for pair in &mut value.frame_pairs {
        if let Some(depth) = &mut pair.depth {
            depth.dsv_id = "main-scene-v1".into();
        }
    }
    assert!(matches!(
        join(&value),
        Err(CameraActionJoinError::CameraDsvIdentityMismatch { .. })
    ));
}

#[test]
fn missing_frame_fails_closed() {
    let mut value = fixture();
    value.actions.remove(1);

    assert!(matches!(
        join(&value),
        Err(CameraActionJoinError::SequenceGap {
            stream: "actions.session_frame",
            previous: 200,
            current: 202,
        })
    ));
}

#[test]
fn duplicate_key_fails_closed_instead_of_overwriting() {
    let mut value = fixture();
    value.depth.insert(1, value.depth[0].clone());

    assert!(matches!(
        join(&value),
        Err(CameraActionJoinError::DuplicateKey {
            stream: "depth.present_seq",
            key: 100,
        })
    ));
}

#[test]
fn out_of_order_rows_fail_closed() {
    let mut value = fixture();
    value.camera.swap(0, 1);

    assert!(matches!(
        join(&value),
        Err(CameraActionJoinError::SequenceNotIncreasing {
            stream: "camera.present_seq",
            previous: 101,
            current: 100,
        })
    ));
}

#[test]
fn wrapped_present_seq_is_not_guessed_or_unwrapped_again() {
    let mut value = fixture();
    value.depth[0].present_seq = u16::MAX.into();
    value.depth[1].present_seq = 0;

    assert!(matches!(
        join(&value),
        Err(CameraActionJoinError::SequenceNotIncreasing {
            stream: "depth.present_seq",
            previous: 65535,
            current: 0,
        })
    ));
}

#[test]
fn qpc_mismatch_between_pair_and_action_fails_closed() {
    let mut value = fixture();
    value.frame_pairs[0].source_qpc += 1;

    assert!(matches!(
        join(&value),
        Err(CameraActionJoinError::SourceQpcMismatch {
            session_frame: 200,
            pair_qpc: 1011,
            action_qpc: 1010,
        })
    ));
}

#[test]
fn inherited_duplicate_is_validated_but_not_emitted_as_a_second_depth() {
    let mut value = fixture();
    value.actions[1].duplicate = true;
    value.actions[1].source_qpc = value.actions[0].source_qpc;
    value.actions[1].source_seq = value.actions[0].source_seq;
    value.frame_pairs[1].duplicate = true;
    value.frame_pairs[1].source_qpc = value.frame_pairs[0].source_qpc;
    let mut inherited = value.frame_pairs[0].depth.clone().unwrap();
    inherited.inherited = true;
    value.frame_pairs[1].depth = Some(inherited);
    value.frame_pairs[1].depth_absent_reason = None;

    let rows = join(&value).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].session_frame, 200);
    assert_eq!(rows[1].session_frame, 202);
}

#[test]
fn stream_identity_mismatch_fails_closed() {
    let mut value = fixture();
    value.camera_identity.generation += 1;

    assert!(matches!(
        join(&value),
        Err(CameraActionJoinError::IdentityMismatch {
            stream: "camera",
            ..
        })
    ));
}

#[test]
fn raw_mouse_delta_is_aggregated_from_events_not_clamped_pointer_fields() {
    let value = fixture();
    assert_eq!(value.actions[0].clamped_pointer_dx, 200);
    assert_eq!(value.actions[0].clamped_pointer_dy, -200);

    let rows = join(&value).unwrap();
    assert_eq!(rows[0].raw_mouse_delta.dx, 360);
    assert_eq!(rows[0].raw_mouse_delta.dy, -230);
}

#[test]
fn joined_core_does_not_claim_unavailable_final_delivery_fields() {
    let value = fixture();
    let row = serde_json::to_value(&join(&value).unwrap()[0]).unwrap();

    for blocked in [
        "player_position",
        "player_rotation_quaternion",
        "player_speed",
        "camera_follow_offset",
        "mouseX",
        "mouseY",
        "mouse_dx",
        "mouse_dy",
    ] {
        assert!(row.get(blocked).is_none(), "unexpected field: {blocked}");
    }
}

#[test]
fn aggregate_camera_candidate_cannot_deserialize_as_a_pose_row() {
    let candidate = serde_json::json!({
        "present_seq": 100,
        "present_qpc": 1005,
        "source": "camera_candidate",
        "camera_position": [10.0, 2.0, -5.0],
        "camera_rotation_quaternion": [0.0, 0.0, 0.0, 1.0],
        "camera_intrinsics": {"fx": 960.0, "fy": 960.0, "cx": 960.0, "cy": 540.0},
        "camera_speed": null
    });

    assert!(serde_json::from_value::<CameraPoseRow>(candidate).is_err());
}

fn player_rows(value: &Fixture) -> Vec<PlayerPoseRow> {
    value
        .camera
        .iter()
        .map(|camera| PlayerPoseRow {
            schema: PLAYER_POSE_INDEX_SCHEMA_V1.into(),
            identity: value.camera_identity.clone(),
            present_seq: camera.present_seq,
            present_qpc: camera.present_qpc + 1,
            executable_sha256:
                "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
            source: "fallout4-player-root-exact-build/v1".into(),
            kcd2_root_source: None,
            cp2077_camera_join: None,
            player_position: [9.0, 1.0, -4.0],
            player_rotation_quaternion: [0.0, 0.0, 0.0, 1.0],
            player_speed: Some([1.0, 0.0, 0.0]),
        })
        .collect()
}

#[test]
fn cp2077_player_source_requires_a_causal_camera_join_within_tolerance() {
    let value = fixture();
    let mut row = player_rows(&value).remove(0);
    let legacy_json = serde_json::to_string(&row).unwrap();
    assert!(!legacy_json.contains("cp2077_camera_join"));
    row.source = depth_lab::CP2077_PLAYER_POSE_SOURCE.into();
    row.executable_sha256 = depth_lab::CP2077_BUILD_20383525_EXE_SHA256.into();
    // The actor root read at Present time is no longer evidence of which frame it belongs to.
    assert!(depth_lab::validate_player_pose_row(&row).is_err());
    row.cp2077_camera_join = Some(depth_lab::Cp2077CameraJoinReceipt {
        state_first_qpc: row.present_qpc - 30,
        state_last_qpc: row.present_qpc - 1,
        state_reads: 29,
        position_residual_metres: 0.0011,
        rotation_residual_degrees: 1e-5,
        candidates: 2,
        torn_candidates: 1,
    });
    depth_lab::validate_player_pose_row(&row).unwrap();
    let decoded: PlayerPoseRow =
        serde_json::from_str(&serde_json::to_string(&row).unwrap()).unwrap();
    assert_eq!(decoded, row);
    for defect in 0..9 {
        let mut bad = row.clone();
        let join = bad.cp2077_camera_join.as_mut().unwrap();
        match defect {
            0 => join.position_residual_metres = 0.0031,
            1 => join.rotation_residual_degrees = 2.1e-4,
            2 => join.position_residual_metres = f64::NAN,
            3 => join.rotation_residual_degrees = -1e-9,
            // A state first seen after this Present could not have been rendered by it.
            4 => join.state_first_qpc = bad.present_qpc + 1,
            5 => join.state_last_qpc = join.state_first_qpc - 1,
            6 => join.candidates = 0,
            7 => bad.executable_sha256 = format!("sha256:{}", "0".repeat(64)),
            _ => bad.source = "cp2077-player-puppet-world-transform-exact-build/v1".into(),
        }
        assert!(
            depth_lab::validate_player_pose_row(&bad).is_err(),
            "defect {defect}"
        );
    }
    let mut foreign = player_rows(&value).remove(0);
    foreign.cp2077_camera_join = row.cp2077_camera_join.clone();
    assert!(depth_lab::validate_player_pose_row(&foreign).is_err());
}

#[test]
fn kcd2_observed_player_source_requires_exact_build_and_pinned_draw_identity() {
    let value = fixture();
    let mut row = player_rows(&value).remove(0);
    let legacy_json = serde_json::to_string(&row).unwrap();
    assert!(!legacy_json.contains("kcd2_root_source"));
    assert!(
        serde_json::from_str::<PlayerPoseRow>(&legacy_json)
            .unwrap()
            .kcd2_root_source
            .is_none()
    );
    let plan = depth_lab::Kcd2LivePosePlan::pinned_v3();
    row.source = "kcd2-horizontal-player-root-exact-build/v2".into();
    row.executable_sha256 = depth_lab::KCD2_EXECUTABLE_SHA256.into();
    assert!(depth_lab::validate_player_pose_row(&row).is_err());
    row.kcd2_root_source = Some(depth_lab::Kcd2LivePoseSourceIdentity {
        root_signature_sha256: plan.horizontal_player_root_binding.root_signature_sha256,
        root_parameter_index: 0,
        pass_ordinal: 15,
        draw_ordinal: 2,
    });
    depth_lab::validate_player_pose_row(&row).unwrap();
    let decoded: PlayerPoseRow =
        serde_json::from_str(&serde_json::to_string(&row).unwrap()).unwrap();
    depth_lab::validate_player_pose_row(&decoded).unwrap();
    assert_eq!(decoded.kcd2_root_source, row.kcd2_root_source);
    assert_eq!(
        (decoded.present_seq, decoded.present_qpc),
        (row.present_seq, row.present_qpc)
    );
    for defect in 0..8 {
        let mut bad = row.clone();
        match defect {
            0 => bad.kcd2_root_source = None,
            1 => bad.kcd2_root_source.as_mut().unwrap().root_signature_sha256 = "0".repeat(64),
            2 => bad.kcd2_root_source.as_mut().unwrap().root_parameter_index = 1,
            3 => bad.kcd2_root_source.as_mut().unwrap().draw_ordinal = 16,
            4 => bad.executable_sha256 = format!("sha256:{}", "0".repeat(64)),
            5 => bad.source = "kcd2-horizontal-player-root-exact-build/v1".into(),
            6 => bad.source = "fallout4-player-root-exact-build/v1".into(),
            _ => bad.source = "bannerlord-main-party-shared-memory/v1".into(),
        }
        assert!(
            depth_lab::validate_player_pose_row(&bad).is_err(),
            "defect {defect}"
        );
    }
}

#[test]
fn final_pose_requires_real_player_mouse_and_wallclock_facts() {
    let value = fixture();
    let joined = join(&value).unwrap();
    let players = player_rows(&value);
    let wallclock = joined
        .iter()
        .map(|row| (row.session_frame, "2026-08-05 12:00:00.000".into()))
        .collect();
    let mouse = MouseNormalization {
        evidence_id: "fallout4-view-scale-preflight-1".into(),
        initial_x: 0.5,
        initial_y: 0.5,
        horizontal_counts_per_unit: 1_000.0,
        vertical_counts_per_unit: 1_000.0,
        delta_counts_full_scale: 200.0,
    };
    let rows = assemble_final_pose_rows(FinalCameraActionInputs {
        identity: &value.camera_identity,
        joined: &joined,
        player: IdentifiedRows::new(&value.camera_identity, &players),
        wallclock_by_session_frame: &wallclock,
        qpc_frequency_hz: 10_000,
        mouse: &mouse,
    })
    .unwrap();
    assert_eq!(rows.len(), joined.len());
    assert_eq!(rows[0].camera_follow_offset, [1.0, 1.0, -1.0]);
    assert_eq!(rows[0].player_speed, [1.0, 0.0, 0.0]);
    assert!((rows[0].mouse_dx - 1.0).abs() < f64::EPSILON);
    assert_eq!(rows[0].key_code, vec![87]);
}

#[test]
fn final_pose_rejects_a_missing_player_frame() {
    let value = fixture();
    let joined = join(&value).unwrap();
    let mut players = player_rows(&value);
    players.pop();
    let wallclock = joined
        .iter()
        .map(|row| (row.session_frame, "2026-08-05 12:00:00.000".into()))
        .collect();
    let mouse = MouseNormalization {
        evidence_id: "evidence".into(),
        initial_x: 0.5,
        initial_y: 0.5,
        horizontal_counts_per_unit: 1_000.0,
        vertical_counts_per_unit: 1_000.0,
        delta_counts_full_scale: 200.0,
    };
    assert!(
        assemble_final_pose_rows(FinalCameraActionInputs {
            identity: &value.camera_identity,
            joined: &joined,
            player: IdentifiedRows::new(&value.camera_identity, &players),
            wallclock_by_session_frame: &wallclock,
            qpc_frequency_hz: 10_000,
            mouse: &mouse,
        })
        .is_err()
    );
}

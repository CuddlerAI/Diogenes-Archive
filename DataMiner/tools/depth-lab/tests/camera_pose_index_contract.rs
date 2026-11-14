use depth_lab::camera::Mat4;
use depth_lab::{
    BoundBufferAtDraw, CAMERA_BINDING_SCHEMA_V1, CAMERA_POSE_INDEX_SCHEMA_V1, CameraBinding,
    CameraBindingError, CameraDrawSample, CameraPoseAssembler, CameraPoseAssemblyError,
    CameraPoseProvenance, CameraPoseSource, CameraPoseStreamContext, GameCameraBinding,
    MatrixEncoding, MatrixLayout, MatrixRole, MatrixSourceBinding, ShaderStage, StreamIdentity,
    ViewSourceBinding, fallout_render_rotation_from_basis, validate_camera_pose_row,
};
use std::path::Path;

const EXE_SHA256: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn source(role: MatrixRole, bind_slot: u32) -> MatrixSourceBinding {
    MatrixSourceBinding {
        shader_stage: ShaderStage::Vertex,
        bind_slot,
        pass_ordinal: 2,
        draw_ordinal: 7,
        buffer_bytes: 64,
        offset: 0,
        encoding: MatrixEncoding::F32LittleEndian,
        layout: MatrixLayout::RowMajor,
        transpose: false,
        role,
    }
}

fn binding() -> CameraBinding {
    CameraBinding {
        schema: CAMERA_BINDING_SCHEMA_V1.into(),
        binding_id: "synthetic-camera-binding-v1".into(),
        binding_version: "1".into(),
        profile_id: "synthetic-game".into(),
        profile_version: "dm-depth-profile-1".into(),
        executable_sha256: EXE_SHA256.into(),
        render_width: 1920,
        render_height: 1080,
        view_metres_per_unit: Some("1".into()),
        game_camera: None,
        view_sources: vec![ViewSourceBinding {
            source: source(MatrixRole::View, 0),
            translation: depth_lab::camera::TranslationSlot::Row3,
        }],
        projection_sources: vec![source(MatrixRole::Projection, 1)],
    }
}

fn game_camera_binding() -> CameraBinding {
    let mut binding = binding();
    binding.binding_id = "synthetic-game-camera-binding-v1".into();
    binding.game_camera = Some(GameCameraBinding {
        provider: "synthetic-exact-build-camera/v1".into(),
        camera_singleton_relocation: 0x1234,
        camera_root_offset: 0x20,
        world_transform_offset: 0x70,
        metres_per_game_unit: "0.0142875".into(),
        axis_mapping: "x_z_y".into(),
    });
    binding.view_metres_per_unit = None;
    binding.view_sources.clear();
    binding
}

fn hybrid_render_camera_binding() -> CameraBinding {
    let mut binding = game_camera_binding();
    let mut view = source(MatrixRole::View, 12);
    view.buffer_bytes = 128;
    let mut projection = source(MatrixRole::Projection, 12);
    projection.buffer_bytes = 128;
    projection.offset = 64;
    binding.view_sources = vec![ViewSourceBinding {
        source: view,
        translation: depth_lab::camera::TranslationSlot::Row3,
    }];
    binding.projection_sources = vec![projection];
    binding
}

#[test]
fn formal_binding_fixture_is_strict_and_contains_no_transient_buffer_tag() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/camera-binding-v1.json");
    let text = std::fs::read_to_string(&path).unwrap();
    let fixture: CameraBinding = serde_json::from_str(&text).unwrap();

    assert_eq!(fixture, binding());
    assert!(!text.contains("buffer_tag"));
}

#[test]
fn fallout4_production_binding_is_the_egeo_authorized_render_hybrid() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../config/catalogs/dev/fallout4-steam-release-test/camera-binding.json");
    let binding = CameraBinding::read(&path).unwrap();

    assert_eq!(
        binding.binding_id,
        "fallout4-steam-1.11.221-main-camera-v3-render-hybrid"
    );
    assert_eq!(binding.binding_version, "3");
    assert_eq!(binding.render_width, 1920);
    assert_eq!(binding.render_height, 1080);
    assert_eq!(binding.view_sources.len(), 1);
    assert_eq!(binding.projection_sources.len(), 1);

    let view = binding.view_sources[0];
    let projection = binding.projection_sources[0];
    assert_eq!(view.source.shader_stage, ShaderStage::Vertex);
    assert_eq!(view.source.bind_slot, 12);
    assert_eq!(view.source.pass_ordinal, 0);
    assert_eq!(view.source.draw_ordinal, 6);
    assert_eq!(view.source.buffer_bytes, 752);
    assert_eq!(view.source.offset, 0);
    assert_eq!(view.source.role, MatrixRole::View);
    assert_eq!(projection.shader_stage, ShaderStage::Vertex);
    assert_eq!(projection.bind_slot, 12);
    assert_eq!(projection.pass_ordinal, view.source.pass_ordinal);
    assert_eq!(projection.draw_ordinal, view.source.draw_ordinal);
    assert_eq!(projection.buffer_bytes, view.source.buffer_bytes);
    assert_eq!(projection.offset, 64);
    assert_eq!(projection.role, MatrixRole::Projection);

    CameraPoseAssembler::new(
        &binding,
        context(),
        "sha256:428f9996cc4248e26c0f62f9fdd3eaf0e5eb305834b67ee5996538e593218b61",
    )
    .unwrap();
}

fn context() -> CameraPoseStreamContext {
    CameraPoseStreamContext {
        identity: StreamIdentity {
            session_id: "session-camera-v1".into(),
            run_id: "r0001".into(),
            generation: 1,
        },
        dsv_id: "main-scene-v1".into(),
    }
}

fn view_matrix(yaw: f32, position: [f32; 3]) -> Mat4 {
    let (cos, sin) = (yaw.cos(), yaw.sin());
    let rotation = [[cos, 0.0, -sin], [0.0, 1.0, 0.0], [sin, 0.0, cos]];
    let mut matrix = [[0.0; 4]; 4];
    for row in 0..3 {
        for column in 0..3 {
            matrix[row][column] = rotation[column][row];
        }
    }
    let translation: [f32; 3] = std::array::from_fn(|column| {
        -(position[0] * matrix[0][column]
            + position[1] * matrix[1][column]
            + position[2] * matrix[2][column])
    });
    matrix[3][..3].copy_from_slice(&translation);
    matrix[3][3] = 1.0;
    matrix
}

fn projection_matrix(depth_b: f32) -> Mat4 {
    [
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 16.0 / 9.0, 0.0, 0.0],
        [0.0, 0.0, 1.001, 1.0],
        [0.0, 0.0, depth_b, 0.0],
    ]
}

fn matrix_bytes(matrix: Mat4) -> Vec<u8> {
    matrix
        .into_iter()
        .flatten()
        .flat_map(f32::to_le_bytes)
        .collect()
}

fn observe(
    assembler: &mut CameraPoseAssembler,
    present_seq: u64,
    qpc: i64,
    view_tag: u64,
    projection_tag: u64,
    view: &[u8],
    projection: &[u8],
) -> Result<(), CameraPoseAssemblyError> {
    let buffers = [
        BoundBufferAtDraw {
            shader_stage: ShaderStage::Vertex,
            bind_slot: 0,
            buffer_tag: view_tag,
            bytes: view,
        },
        BoundBufferAtDraw {
            shader_stage: ShaderStage::Vertex,
            bind_slot: 1,
            buffer_tag: projection_tag,
            bytes: projection,
        },
    ];
    assembler.observe_draw(CameraDrawSample {
        present_seq,
        sample_qpc: qpc,
        pass_ordinal: 2,
        draw_ordinal: 7,
        bound_buffers: &buffers,
    })
}

#[test]
fn valid_view_and_projection_preserve_pose_intrinsics_and_provenance() {
    let mut assembler = CameraPoseAssembler::new(&binding(), context(), EXE_SHA256).unwrap();
    let view = matrix_bytes(view_matrix(std::f32::consts::FRAC_PI_2, [10.0, 2.0, -5.0]));
    let projection = matrix_bytes(projection_matrix(-0.1));

    observe(
        &mut assembler,
        100,
        123_456,
        0x111,
        0x222,
        &view,
        &projection,
    )
    .unwrap();
    let row = assembler.finish_present(100).unwrap().unwrap();

    assert_eq!(row.schema, CAMERA_POSE_INDEX_SCHEMA_V1);
    assert_eq!(row.present_seq, 100);
    assert_eq!(row.present_qpc, 123_456);
    for (actual, expected) in row.camera_position.iter().zip([10.0, 2.0, -5.0]) {
        assert!((actual - expected).abs() < 1.0e-5);
    }
    let quaternion_norm = row
        .camera_rotation_quaternion
        .iter()
        .map(|value| value * value)
        .sum::<f64>();
    assert!((quaternion_norm - 1.0).abs() < 1.0e-6);
    assert_eq!(row.camera_intrinsics.fx, 960.0);
    assert_eq!(row.camera_intrinsics.fy, 960.0);
    assert_eq!(row.camera_intrinsics.cx, 960.0);
    assert_eq!(row.camera_intrinsics.cy, 540.0);
    assert!(validate_camera_pose_row(&row).is_ok());
    assert!(!serde_json::to_string(&row).unwrap().contains("buffer_tag"));

    let CameraPoseProvenance::ResolvedMainCameraHook {
        executable_sha256,
        render_width,
        render_height,
        view_source,
        projection_source,
        ..
    } = row.provenance
    else {
        panic!("production hook row lost its provenance");
    };
    assert_eq!(executable_sha256, EXE_SHA256);
    assert_eq!((render_width, render_height), (1920, 1080));
    assert_eq!(view_source.source.bind_slot, 0);
    assert_eq!(projection_source.bind_slot, 1);
}

#[test]
fn gpu_view_position_is_scaled_to_metres_and_provenance_revalidates_it() {
    let mut metric_binding = binding();
    metric_binding.view_metres_per_unit = Some("0.5".into());
    let mut assembler = CameraPoseAssembler::new(&metric_binding, context(), EXE_SHA256).unwrap();
    let view = matrix_bytes(view_matrix(0.0, [10.0, 2.0, -5.0]));
    let projection = matrix_bytes(projection_matrix(-0.1));

    observe(
        &mut assembler,
        101,
        123_457,
        0x111,
        0x222,
        &view,
        &projection,
    )
    .unwrap();
    let mut row = assembler.finish_present(101).unwrap().unwrap();
    assert_eq!(row.camera_position, [5.0, 1.0, -2.5]);
    assert!(validate_camera_pose_row(&row).is_ok());

    row.camera_position[0] = 10.0;
    assert_eq!(
        validate_camera_pose_row(&row),
        Err("hook camera pose disagrees with its view matrix")
    );
}

#[test]
fn exact_game_camera_pose_and_gpu_projection_share_one_auditable_row() {
    let mut assembler =
        CameraPoseAssembler::new(&game_camera_binding(), context(), EXE_SHA256).unwrap();
    let projection = matrix_bytes(projection_matrix(-0.1));
    let buffers = [BoundBufferAtDraw {
        shader_stage: ShaderStage::Vertex,
        bind_slot: 1,
        buffer_tag: 0x222,
        bytes: &projection,
    }];
    assembler
        .observe_draw(CameraDrawSample {
            present_seq: 100,
            sample_qpc: 123_400,
            pass_ordinal: 2,
            draw_ordinal: 7,
            bound_buffers: &buffers,
        })
        .unwrap();
    let row = assembler
        .finish_present_game_telemetry(100, 123_456, [-974.0, 107.7, 1176.5], [0.0, 0.0, 0.0, 1.0])
        .unwrap()
        .unwrap();

    assert_eq!(row.camera_position, [-974.0, 107.7, 1176.5]);
    assert_eq!(row.camera_intrinsics.fx, 960.0);
    assert!(validate_camera_pose_row(&row).is_ok());
    let CameraPoseProvenance::GameTelemetryMainCameraProjection {
        game_camera,
        projection_source,
        projection_matrix,
        ..
    } = row.provenance
    else {
        panic!("game camera row lost exact-build/projection provenance");
    };
    assert_eq!(game_camera.provider, "synthetic-exact-build-camera/v1");
    assert_eq!(projection_source.bind_slot, 1);
    assert_eq!(*projection_matrix, projection_matrix_for_assertion());
}

#[test]
fn hybrid_camera_keeps_world_position_but_uses_the_exact_draw_render_basis() {
    let mut assembler =
        CameraPoseAssembler::new(&hybrid_render_camera_binding(), context(), EXE_SHA256).unwrap();
    // Fallout's render basis is left-handed. Swapping its Y/Z columns produces identity here.
    let render_basis = [
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [0.0, 0.0, 0.0, 1.0],
    ];
    let mut bytes = matrix_bytes(render_basis);
    bytes.extend(matrix_bytes(projection_matrix(-0.1)));
    let buffers = [BoundBufferAtDraw {
        shader_stage: ShaderStage::Vertex,
        bind_slot: 12,
        buffer_tag: 0x123,
        bytes: &bytes,
    }];
    assembler
        .observe_draw(CameraDrawSample {
            present_seq: 100,
            sample_qpc: 123_400,
            pass_ordinal: 2,
            draw_ordinal: 7,
            bound_buffers: &buffers,
        })
        .unwrap();
    let row = assembler
        .finish_present_game_telemetry(100, 123_456, [-974.0, 107.7, 1176.5], [0.0, 0.0, 1.0, 0.0])
        .unwrap()
        .unwrap();

    assert_eq!(row.source, CameraPoseSource::ResolvedRenderCameraHybrid);
    assert_eq!(row.camera_position, [-974.0, 107.7, 1176.5]);
    assert_eq!(row.camera_rotation_quaternion, [0.0, 0.0, 0.0, 1.0]);
    assert!(validate_camera_pose_row(&row).is_ok());
    let CameraPoseProvenance::ResolvedRenderCameraHybrid {
        game_camera,
        render_basis_source,
        projection_source,
        render_basis_matrix,
        ..
    } = row.provenance
    else {
        panic!("hybrid camera row lost its split position/render provenance");
    };
    assert_eq!(game_camera.provider, "synthetic-exact-build-camera/v1");
    assert_eq!(render_basis_source.source.bind_slot, 12);
    assert_eq!(projection_source.bind_slot, 12);
    assert_eq!(*render_basis_matrix, render_basis);
}

#[test]
fn fallout_render_basis_is_inverted_to_camera_to_world() {
    // Swapping raw Y/Z columns yields this world-to-camera yaw rotation:
    // [[0,0,1],[0,1,0],[-1,0,0]]. Delivery requires its transpose.
    let raw_render_basis = [
        [0.0, 1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [-1.0, 0.0, 0.0, 0.0],
        [0.0, 0.0, 0.0, 1.0],
    ];
    let quaternion = fallout_render_rotation_from_basis(
        &raw_render_basis,
        depth_lab::camera::TranslationSlot::Row3,
    )
    .unwrap();
    let root_half = 0.5_f32.sqrt();
    assert!((quaternion[1] + root_half).abs() < 1e-6, "{quaternion:?}");
    assert!((quaternion[3] - root_half).abs() < 1e-6, "{quaternion:?}");
}

#[test]
fn hybrid_camera_rejects_an_ordinary_object_view_matrix() {
    let mut assembler =
        CameraPoseAssembler::new(&hybrid_render_camera_binding(), context(), EXE_SHA256).unwrap();
    let mut bytes = matrix_bytes(view_matrix(0.0, [10.0, 20.0, 30.0]));
    bytes.extend(matrix_bytes(projection_matrix(-0.1)));
    let buffers = [BoundBufferAtDraw {
        shader_stage: ShaderStage::Vertex,
        bind_slot: 12,
        buffer_tag: 0x123,
        bytes: &bytes,
    }];
    assert!(matches!(
        assembler.observe_draw(CameraDrawSample {
            present_seq: 100,
            sample_qpc: 123_400,
            pass_ordinal: 2,
            draw_ordinal: 7,
            bound_buffers: &buffers,
        }),
        Err(CameraPoseAssemblyError::InvalidViewMatrix { present_seq: 100 })
    ));
}

#[test]
fn non_depth_presents_keep_pose_continuity_with_the_last_exact_projection() {
    let mut assembler =
        CameraPoseAssembler::new(&game_camera_binding(), context(), EXE_SHA256).unwrap();
    assert!(matches!(
        assembler.finish_present_game_telemetry_with_held_projection(
            99,
            123_399,
            [0.0; 3],
            [0.0, 0.0, 0.0, 1.0]
        ),
        Err(CameraPoseAssemblyError::MissingProjection { present_seq: 99 })
    ));

    let projection = matrix_bytes(projection_matrix(-0.1));
    let buffers = [BoundBufferAtDraw {
        shader_stage: ShaderStage::Vertex,
        bind_slot: 1,
        buffer_tag: 0x222,
        bytes: &projection,
    }];
    assembler
        .observe_draw(CameraDrawSample {
            present_seq: 100,
            sample_qpc: 123_400,
            pass_ordinal: 2,
            draw_ordinal: 7,
            bound_buffers: &buffers,
        })
        .unwrap();
    assembler
        .finish_present_game_telemetry(100, 123_456, [1.0, 2.0, 3.0], [0.0, 0.0, 0.0, 1.0])
        .unwrap()
        .unwrap();

    let held = assembler
        .finish_present_game_telemetry_with_held_projection(
            101,
            123_500,
            [4.0, 5.0, 6.0],
            [0.0, 0.0, 0.0, 1.0],
        )
        .unwrap()
        .unwrap();
    assert_eq!(held.present_seq, 101);
    assert_eq!(held.camera_position, [4.0, 5.0, 6.0]);
    let CameraPoseProvenance::GameTelemetryMainCameraProjection {
        projection_matrix, ..
    } = held.provenance
    else {
        panic!("held pose lost its exact production projection source");
    };
    assert_eq!(*projection_matrix, projection_matrix_for_assertion());
}

fn projection_matrix_for_assertion() -> Mat4 {
    projection_matrix(-0.1)
}

#[test]
fn packed_col3_view_does_not_require_a_homogeneous_fourth_row() {
    let mut fallout_binding = binding();
    fallout_binding.view_sources[0].translation = depth_lab::camera::TranslationSlot::Col3;
    let mut assembler = CameraPoseAssembler::new(&fallout_binding, context(), EXE_SHA256).unwrap();
    let view = matrix_bytes([
        [1.0, 0.0, 0.0, -10.0],
        [0.0, 1.0, 0.0, -2.0],
        [0.0, 0.0, 1.0, 5.0],
        [0.5, 0.5, 0.5, 0.5],
    ]);
    let projection = matrix_bytes(projection_matrix(-0.1));

    observe(
        &mut assembler,
        100,
        123_456,
        0x111,
        0x222,
        &view,
        &projection,
    )
    .unwrap();
    let row = assembler.finish_present(100).unwrap().unwrap();

    for (actual, expected) in row.camera_position.iter().zip([10.0, 2.0, -5.0]) {
        assert!((actual - expected).abs() < 1.0e-5);
    }
}

#[test]
fn binding_requires_one_view_and_one_projection_and_exact_executable() {
    let mut missing_projection = binding();
    missing_projection.projection_sources.clear();
    assert!(matches!(
        CameraPoseAssembler::new(&missing_projection, context(), EXE_SHA256),
        Err(CameraPoseAssemblyError::Binding(
            CameraBindingError::ProjectionSourceCount { count: 0 }
        ))
    ));

    let mut ambiguous_view = binding();
    ambiguous_view
        .view_sources
        .push(ambiguous_view.view_sources[0]);
    assert!(matches!(
        CameraPoseAssembler::new(&ambiguous_view, context(), EXE_SHA256),
        Err(CameraPoseAssemblyError::Binding(
            CameraBindingError::ViewSourceCount { count: 2 }
        ))
    ));

    let other_hash = "sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";
    assert!(matches!(
        CameraPoseAssembler::new(&binding(), context(), other_hash),
        Err(CameraPoseAssemblyError::Binding(
            CameraBindingError::ExecutableHashMismatch { .. }
        ))
    ));
}

#[test]
fn missing_or_conflicting_source_in_the_same_draw_window_fails_closed() {
    let mut assembler = CameraPoseAssembler::new(&binding(), context(), EXE_SHA256).unwrap();
    let view = matrix_bytes(view_matrix(0.0, [0.0, 0.0, 0.0]));
    let projection = matrix_bytes(projection_matrix(-0.1));
    let view_only = [BoundBufferAtDraw {
        shader_stage: ShaderStage::Vertex,
        bind_slot: 0,
        buffer_tag: 1,
        bytes: &view,
    }];
    assert!(matches!(
        assembler.observe_draw(CameraDrawSample {
            present_seq: 100,
            sample_qpc: 10,
            pass_ordinal: 2,
            draw_ordinal: 7,
            bound_buffers: &view_only,
        }),
        Err(CameraPoseAssemblyError::MissingProjection { present_seq: 100 })
    ));

    let mut assembler = CameraPoseAssembler::new(&binding(), context(), EXE_SHA256).unwrap();
    let conflict = [
        BoundBufferAtDraw {
            shader_stage: ShaderStage::Vertex,
            bind_slot: 0,
            buffer_tag: 1,
            bytes: &view,
        },
        BoundBufferAtDraw {
            shader_stage: ShaderStage::Vertex,
            bind_slot: 0,
            buffer_tag: 2,
            bytes: &view,
        },
        BoundBufferAtDraw {
            shader_stage: ShaderStage::Vertex,
            bind_slot: 1,
            buffer_tag: 3,
            bytes: &projection,
        },
    ];
    assert!(matches!(
        assembler.observe_draw(CameraDrawSample {
            present_seq: 100,
            sample_qpc: 10,
            pass_ordinal: 2,
            draw_ordinal: 7,
            bound_buffers: &conflict,
        }),
        Err(CameraPoseAssemblyError::AmbiguousMatrixSource {
            present_seq: 100,
            kind: "view",
            ..
        })
    ));
}

#[test]
fn ring_buffer_rotation_and_normal_matrix_change_across_frames_are_allowed() {
    let mut assembler = CameraPoseAssembler::new(&binding(), context(), EXE_SHA256).unwrap();
    let projection_100 = matrix_bytes(projection_matrix(-0.1));
    let projection_101 = matrix_bytes(projection_matrix(-0.2));
    let view_100 = matrix_bytes(view_matrix(0.0, [1.0, 0.0, 0.0]));
    let view_101 = matrix_bytes(view_matrix(0.25, [2.0, 0.0, 0.0]));

    observe(
        &mut assembler,
        100,
        10,
        0x100,
        0x200,
        &view_100,
        &projection_100,
    )
    .unwrap();
    assembler.finish_present(100).unwrap().unwrap();

    observe(
        &mut assembler,
        101,
        20,
        0x101,
        0x201,
        &view_101,
        &projection_101,
    )
    .unwrap();
    let row = assembler.finish_present(101).unwrap().unwrap();
    assert!((row.camera_position[0] - 2.0).abs() < 1.0e-5);

    let view_102 = matrix_bytes(view_matrix(0.5, [3.0, 0.0, 0.0]));
    observe(
        &mut assembler,
        102,
        30,
        0x101,
        0x201,
        &view_102,
        &projection_101,
    )
    .unwrap();
    assembler.finish_present(102).unwrap().unwrap();
}

#[test]
fn invalid_matrices_and_present_sequence_are_rejected() {
    let projection = matrix_bytes(projection_matrix(-0.1));
    let singular = matrix_bytes([[0.0; 4]; 4]);
    let mut assembler = CameraPoseAssembler::new(&binding(), context(), EXE_SHA256).unwrap();
    assert!(matches!(
        observe(&mut assembler, 100, 10, 1, 2, &singular, &projection),
        Err(CameraPoseAssemblyError::InvalidViewMatrix { present_seq: 100 })
    ));

    let mut nan_projection = projection_matrix(-0.1);
    nan_projection[0][0] = f32::NAN;
    let nan_projection = matrix_bytes(nan_projection);
    let view = matrix_bytes(view_matrix(0.0, [0.0, 0.0, 0.0]));
    let mut assembler = CameraPoseAssembler::new(&binding(), context(), EXE_SHA256).unwrap();
    assert!(matches!(
        observe(&mut assembler, 100, 10, 1, 2, &view, &nan_projection),
        Err(CameraPoseAssemblyError::InvalidProjectionMatrix { present_seq: 100 })
    ));

    let mut assembler = CameraPoseAssembler::new(&binding(), context(), EXE_SHA256).unwrap();
    observe(&mut assembler, 100, 10, 1, 2, &view, &projection).unwrap();
    assembler.finish_present(100).unwrap().unwrap();
    assert!(matches!(
        assembler.finish_present(101),
        Err(CameraPoseAssemblyError::MissingView { present_seq: 101 })
    ));

    let mut assembler = CameraPoseAssembler::new(&binding(), context(), EXE_SHA256).unwrap();
    observe(&mut assembler, 100, 10, 1, 2, &view, &projection).unwrap();
    assembler.finish_present(100).unwrap().unwrap();
    assert!(matches!(
        observe(&mut assembler, 100, 20, 1, 2, &view, &projection),
        Err(CameraPoseAssemblyError::DuplicatePresentSeq { present_seq: 100 })
    ));

    let mut assembler = CameraPoseAssembler::new(&binding(), context(), EXE_SHA256).unwrap();
    observe(&mut assembler, 100, 10, 1, 2, &view, &projection).unwrap();
    assembler.finish_present(100).unwrap().unwrap();
    assert!(matches!(
        observe(&mut assembler, 99, 20, 1, 2, &view, &projection),
        Err(CameraPoseAssemblyError::PresentSeqOutOfOrder {
            previous: 100,
            current: 99
        })
    ));

    let mut assembler = CameraPoseAssembler::new(&binding(), context(), EXE_SHA256).unwrap();
    observe(&mut assembler, 100, 10, 1, 2, &view, &projection).unwrap();
    assembler.finish_present(100).unwrap().unwrap();
    assert!(matches!(
        observe(&mut assembler, 102, 20, 1, 2, &view, &projection),
        Err(CameraPoseAssemblyError::PresentSeqGap {
            previous: 100,
            current: 102
        })
    ));
}

#[test]
fn an_explicit_absent_present_advances_the_timeline_without_reusing_a_pose() {
    let projection = matrix_bytes(projection_matrix(-0.1));
    let view_100 = matrix_bytes(view_matrix(0.0, [1.0, 0.0, 0.0]));
    let view_102 = matrix_bytes(view_matrix(0.5, [3.0, 0.0, 0.0]));
    let mut assembler = CameraPoseAssembler::new(&binding(), context(), EXE_SHA256).unwrap();

    observe(
        &mut assembler,
        100,
        10,
        0x100,
        0x200,
        &view_100,
        &projection,
    )
    .unwrap();
    let row_100 = assembler.finish_present(100).unwrap().unwrap();

    assembler.skip_present(101).unwrap();

    observe(
        &mut assembler,
        102,
        30,
        0x102,
        0x202,
        &view_102,
        &projection,
    )
    .unwrap();
    let row_102 = assembler.finish_present(102).unwrap().unwrap();

    assert_eq!(row_100.present_seq, 100);
    assert_eq!(row_102.present_seq, 102);
    assert_ne!(row_100.camera_position, row_102.camera_position);
}

#[test]
fn discovery_candidate_report_is_not_a_production_binding() {
    let discovery = serde_json::json!({
        "frames_seen": 600,
        "dropped_samples": 0,
        "buffer_sizes": {"3735928559": 256},
        "view_candidates": [],
        "projection_candidates": []
    });

    assert!(serde_json::from_value::<CameraBinding>(discovery).is_err());
}

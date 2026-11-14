//! Production D3D12 camera math. Sources are resolved by the backend from an exact v2 binding;
//! this module never scans for candidates or infers an axis mapping from a single frame.

use crate::camera::{
    Intrinsics, Mat4, MatrixKind, ORTHONORMAL_TOLERANCE, TranslationSlot, classify,
    intrinsics_from_projection, mat3_to_quat, orthonormality_error, pose_from_view,
};
use crate::camera_action::{
    CAMERA_POSE_INDEX_SCHEMA_V2, CameraPoseProvenance, CameraPoseRow, CameraPoseSource,
};
use crate::camera_binding::{
    CameraPoseStreamContext, D3d12AxisMapping, D3d12CameraBinding, D3d12MatrixSourceBinding,
    MatrixLayout,
};
use crate::camera_track::projection_from_matrix;

/// Convert a native world-space pose into the delivery coordinate system declared by a D3D12
/// binding. Position uses the authored metric scale; rotation changes basis as `P * R * P^T`.
/// This remains valid for reflected signed permutations, where permuting quaternion components
/// directly would not.
pub fn transform_world_pose(
    axis_mapping: D3d12AxisMapping,
    metres_per_unit: f64,
    source_position: [f64; 3],
    source_rotation_quaternion: [f64; 4],
) -> Result<([f64; 3], [f64; 4]), &'static str> {
    axis_mapping
        .validate()
        .map_err(|_| "invalid world-pose axis mapping")?;
    if !metres_per_unit.is_finite() || metres_per_unit <= 0.0 {
        return Err("invalid world-pose metric scale");
    }
    if !source_position.iter().all(|value| value.is_finite())
        || !source_rotation_quaternion
            .iter()
            .all(|value| value.is_finite())
    {
        return Err("world pose contains a non-finite value");
    }
    let norm_squared = source_rotation_quaternion
        .iter()
        .map(|value| value * value)
        .sum::<f64>();
    if norm_squared <= f64::EPSILON || (norm_squared.sqrt() - 1.0).abs() > 0.02 {
        return Err("world-pose quaternion is not approximately normalized");
    }
    let inverse_norm = norm_squared.sqrt().recip();
    let [x, y, z, w] = source_rotation_quaternion.map(|value| value * inverse_norm);
    let source_rotation = [
        [
            1.0 - 2.0 * (y * y + z * z),
            2.0 * (x * y - z * w),
            2.0 * (x * z + y * w),
        ],
        [
            2.0 * (x * y + z * w),
            1.0 - 2.0 * (x * x + z * z),
            2.0 * (y * z - x * w),
        ],
        [
            2.0 * (x * z - y * w),
            2.0 * (y * z + x * w),
            1.0 - 2.0 * (x * x + y * y),
        ],
    ];
    let basis: [[f64; 3]; 3] = std::array::from_fn(|row| {
        std::array::from_fn(|column| {
            axis_mapping.source_to_delivery(std::array::from_fn(|axis| {
                if axis == column { 1.0 } else { 0.0 }
            }))[row]
        })
    });
    let multiply = |left: [[f64; 3]; 3], right: [[f64; 3]; 3]| {
        std::array::from_fn(|row| {
            std::array::from_fn(|column| (0..3).map(|k| left[row][k] * right[k][column]).sum())
        })
    };
    let basis_transpose =
        std::array::from_fn(|row| std::array::from_fn(|column| basis[column][row]));
    let delivery_rotation = multiply(multiply(basis, source_rotation), basis_transpose);
    let mut delivery_quaternion =
        mat3_to_quat(&delivery_rotation.map(|row| row.map(|value| value as f32))).map(f64::from);
    let delivery_norm = delivery_quaternion
        .iter()
        .map(|value| value * value)
        .sum::<f64>()
        .sqrt();
    if !delivery_norm.is_finite() || delivery_norm <= f64::EPSILON {
        return Err("world-pose basis conversion produced an invalid quaternion");
    }
    delivery_quaternion = delivery_quaternion.map(|value| value / delivery_norm);
    if delivery_quaternion[3] < 0.0 {
        delivery_quaternion = delivery_quaternion.map(|value| -value);
    }
    Ok((
        axis_mapping
            .source_to_delivery(source_position)
            .map(|value| value * metres_per_unit),
        delivery_quaternion,
    ))
}

/// Decode exactly the declared matrix. `bytes` begins at the CBV resource offset, not the resource
/// origin. Layout/transpose are applied once here; durable provenance stores the decoded matrices.
pub fn read_bound_matrix(source: &D3d12MatrixSourceBinding, bytes: &[u8]) -> Option<Mat4> {
    source.validate_for_role(source.role).ok()?;
    let start = usize::try_from(source.matrix_offset).ok()?;
    let raw = bytes.get(start..start.checked_add(64)?)?;
    let mut matrix = [[0.0; 4]; 4];
    for (index, chunk) in raw.chunks_exact(4).enumerate() {
        let value = f32::from_le_bytes(chunk.try_into().ok()?);
        if !value.is_finite() {
            return None;
        }
        let (row, col) = match source.layout {
            MatrixLayout::RowMajor => (index / 4, index % 4),
            MatrixLayout::ColumnMajor => (index % 4, index / 4),
        };
        let (row, col) = if source.transpose {
            (col, row)
        } else {
            (row, col)
        };
        matrix[row][col] = value;
    }
    Some(matrix)
}

/// Decode a matrix from an already validated production binding. Only the
/// explicit CP2077 row-VP policy derives a view; legacy policies still read V
/// directly. Both inputs must belong to the same authored CBV alias and bytes.
pub fn read_bound_camera_matrix(
    binding: &D3d12CameraBinding,
    source: &D3d12MatrixSourceBinding,
    bytes: &[u8],
) -> Option<Mat4> {
    let matrix = read_bound_matrix(source, bytes)?;
    let policy = binding.holstered_camera.as_ref().map(|p| p.schema.as_str());
    match policy {
        None | Some(crate::cp2077_bound_camera::POLICY) => Some(matrix),
        Some(crate::cp2077_bound_camera::ROW_VIEW_PROJECTION_POLICY) => {
            if binding.schema != crate::CAMERA_BINDING_SCHEMA_V3 {
                return None;
            }
            if source.role == crate::MatrixRole::Projection {
                return binding
                    .projection_sources
                    .contains(source)
                    .then_some(matrix);
            }
            let (_, projection_source) = binding
                .view_sources
                .iter()
                .zip(&binding.projection_sources)
                .find(|(view, _)| &view.source == source)?;
            if source.descriptor_table != projection_source.descriptor_table
                || source.cbv_resource_offset != projection_source.cbv_resource_offset
                || source.declared_cbv_bytes != projection_source.declared_cbv_bytes
                || source.matrix_offset == projection_source.matrix_offset
                || projection_source.role != crate::MatrixRole::Projection
            {
                return None;
            }
            let projection = read_bound_matrix(projection_source, bytes)?;
            crate::camera::recover_row_view(&matrix, &projection)
        }
        Some(_) => None,
    }
}

pub fn resolve_pose(
    binding: &D3d12CameraBinding,
    view: &Mat4,
    projection: &Mat4,
) -> Result<([f64; 3], [f64; 4], Intrinsics), &'static str> {
    binding
        .validate_for_executable(&binding.executable_sha256)
        .map_err(|_| "invalid D3D12 camera binding")?;
    let translation = binding.view_sources[0].translation;
    let affine = (view[3][3] - 1.0).abs() <= 1e-5
        && match translation {
            TranslationSlot::Row3 => (0..3).all(|i| view[i][3].abs() <= 1e-5),
            TranslationSlot::Col3 => (0..3).all(|i| view[3][i].abs() <= 1e-5),
        };
    if !view
        .iter()
        .flatten()
        .chain(projection.iter().flatten())
        .all(|v| v.is_finite())
    {
        return Err("invalid D3D12 camera matrices: non-finite input");
    }
    if !affine {
        return Err("invalid D3D12 camera matrices: view is not affine in the declared layout");
    }
    if orthonormality_error(view) > ORTHONORMAL_TOLERANCE {
        return Err("invalid D3D12 camera matrices: view basis is not orthonormal");
    }
    if classify(projection) != MatrixKind::Projection
        || projection_from_matrix(projection).is_none()
    {
        return Err("invalid D3D12 camera matrices: projection is not a linearizable perspective");
    }
    let pose = pose_from_view(view, translation);
    let scale = binding
        .view_metres_per_unit
        .parse::<f64>()
        .map_err(|_| "invalid camera scale")?;
    let position = binding
        .axis_mapping
        .source_to_delivery(pose.position.map(f64::from))
        .map(|v| v * scale);
    // Change coordinates for both sides of the camera-to-world rotation: P * R * P^T.
    // Signed permutations may reverse handedness; conjugation still preserves a proper rotation.
    let basis: [[f64; 3]; 3] = std::array::from_fn(|column| {
        binding
            .axis_mapping
            .source_to_delivery(std::array::from_fn(
                |row| if row == column { 1.0 } else { 0.0 },
            ))
    });
    let camera_basis = binding.holstered_camera.as_ref().map(|policy| {
        std::array::from_fn::<_, 3, _>(|column| {
            policy
                .camera_axis_mapping
                .source_to_delivery(std::array::from_fn(
                    |row| if row == column { 1.0 } else { 0.0 },
                ))
        })
    });
    let rotation: [[f32; 3]; 3] = std::array::from_fn(|row| {
        std::array::from_fn(|col| {
            (0..3)
                .flat_map(|a| {
                    (0..3).map(move |b| {
                        // V3 is explicitly row-vector world->camera: inverse rotation in
                        // column coordinates is V's upper block. World and camera bases
                        // are independent. Retain the old v2 convention unchanged.
                        if let Some(camera) = camera_basis {
                            basis[a][row] * f64::from(view[a][b]) * camera[b][col]
                        } else {
                            basis[a][row] * f64::from(view[b][a]) * basis[b][col]
                        }
                    })
                })
                .sum::<f64>() as f32
        })
    });
    let determinant = rotation[0][0]
        * (rotation[1][1] * rotation[2][2] - rotation[1][2] * rotation[2][1])
        - rotation[0][1] * (rotation[1][0] * rotation[2][2] - rotation[1][2] * rotation[2][0])
        + rotation[0][2] * (rotation[1][0] * rotation[2][1] - rotation[1][1] * rotation[2][0]);
    if (determinant - 1.0).abs() > 0.01 {
        return Err("D3D12 camera basis is not a proper rotation");
    }
    let quaternion = mat3_to_quat(&rotation).map(f64::from);
    let intrinsics = intrinsics_from_projection(
        projection,
        binding.render_width as f32,
        binding.render_height as f32,
    );
    if ![intrinsics.fx, intrinsics.fy, intrinsics.cx, intrinsics.cy]
        .iter()
        .all(|v| v.is_finite())
        || intrinsics.fx <= 0.0
        || intrinsics.fy <= 0.0
    {
        return Err("invalid D3D12 camera intrinsics");
    }
    Ok((position, quaternion, intrinsics))
}

pub fn assemble_pose(
    binding: &D3d12CameraBinding,
    context: &CameraPoseStreamContext,
    present_seq: u64,
    present_qpc: i64,
    view: Mat4,
    projection: Mat4,
) -> Result<CameraPoseRow, &'static str> {
    if context.identity.generation == 0
        || context.identity.session_id.trim().is_empty()
        || context.identity.run_id.trim().is_empty()
        || context.dsv_id.is_empty()
        || present_qpc <= 0
        || present_seq == u64::MAX
    {
        return Err("invalid D3D12 camera frame identity");
    }
    let (camera_position, camera_rotation_quaternion, camera_intrinsics) =
        resolve_pose(binding, &view, &projection)?;
    Ok(CameraPoseRow {
        schema: CAMERA_POSE_INDEX_SCHEMA_V2.into(),
        identity: context.identity.clone(),
        present_seq,
        present_qpc,
        source: CameraPoseSource::ResolvedD3d12CameraHook,
        provenance: CameraPoseProvenance::ResolvedD3d12CameraHook {
            binding: Box::new(binding.clone()),
            dsv_id: context.dsv_id.clone(),
            view_matrix: Box::new(view),
            projection_matrix: Box::new(projection),
        },
        camera_position,
        camera_rotation_quaternion,
        camera_intrinsics,
        camera_speed: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::camera_binding::D3d12SignedAxis;
    use crate::{CameraBindingDocument, StreamIdentity, validate_camera_pose_row};

    fn binding() -> D3d12CameraBinding {
        serde_json::from_str(include_str!("../tests/fixtures/camera-binding-v2.json")).unwrap()
    }
    fn matrices() -> (Mat4, Mat4) {
        (
            [
                [1., 0., 0., 0.],
                [0., 1., 0., 0.],
                [0., 0., 1., 0.],
                [-1., -2., -3., 1.],
            ],
            [
                [1., 0., 0., 0.],
                [0., 16. / 9., 0., 0.],
                [0., 0., 1.001, 1.],
                [0., 0., -0.1, 0.],
            ],
        )
    }

    #[test]
    fn world_pose_conversion_conjugates_reflected_axis_mappings() {
        let mapping = D3d12AxisMapping {
            delivery_x_from: D3d12SignedAxis::PositiveX,
            delivery_y_from: D3d12SignedAxis::PositiveZ,
            delivery_z_from: D3d12SignedAxis::PositiveY,
        };
        let half = 0.5_f64.sqrt();
        let (position, quaternion) =
            transform_world_pose(mapping, 2.0, [1.0, 2.0, 3.0], [0.0, half, 0.0, half]).unwrap();
        assert_eq!(position, [2.0, 6.0, 4.0]);
        assert!(quaternion[0].abs() < 1e-6, "{quaternion:?}");
        assert!(quaternion[1].abs() < 1e-6, "{quaternion:?}");
        assert!((quaternion[2] + half).abs() < 1e-6, "{quaternion:?}");
        assert!((quaternion[3] - half).abs() < 1e-6, "{quaternion:?}");
    }

    #[test]
    fn world_pose_conversion_rejects_bad_scale_and_quaternion() {
        let mapping = D3d12AxisMapping {
            delivery_x_from: D3d12SignedAxis::PositiveX,
            delivery_y_from: D3d12SignedAxis::PositiveY,
            delivery_z_from: D3d12SignedAxis::PositiveZ,
        };
        assert!(transform_world_pose(mapping, 0.0, [0.0; 3], [0.0, 0.0, 0.0, 1.0]).is_err());
        assert!(transform_world_pose(mapping, 1.0, [0.0; 3], [0.0; 4]).is_err());
    }
    fn row() -> CameraPoseRow {
        let (view, projection) = matrices();
        assemble_pose(
            &binding(),
            &CameraPoseStreamContext {
                identity: StreamIdentity {
                    session_id: "session".into(),
                    run_id: "run".into(),
                    generation: 1,
                },
                dsv_id: "scene".into(),
            },
            10,
            1000,
            view,
            projection,
        )
        .unwrap()
    }
    #[test]
    fn row_vp_policy_is_explicit_and_cannot_borrow_a_foreign_projection() {
        let mut b = crate::cp2077_bound_camera::tests::binding();
        b.holstered_camera.as_mut().unwrap().schema =
            crate::cp2077_bound_camera::ROW_VIEW_PROJECTION_POLICY.into();
        let (mut view, projection) = matrices();
        view[1][1] = -1.;
        let vp: Mat4 = std::array::from_fn(|r| {
            std::array::from_fn(|c| (0..4).map(|i| view[r][i] * projection[i][c]).sum())
        });
        let source = b.view_sources[0].source.clone();
        let projection_source = b.projection_sources[0].clone();
        let mut bytes = vec![0u8; 256];
        fn store(bytes: &mut [u8], s: &D3d12MatrixSourceBinding, matrix: Mat4) {
            for (r, row) in matrix.iter().enumerate() {
                for (c, value) in row.iter().enumerate() {
                    let (r, c) = if s.transpose { (c, r) } else { (r, c) };
                    let (r, c) = match s.layout {
                        MatrixLayout::RowMajor => (r, c),
                        MatrixLayout::ColumnMajor => (c, r),
                    };
                    let k = s.matrix_offset as usize + (r * 4 + c) * 4;
                    bytes[k..k + 4].copy_from_slice(&value.to_le_bytes());
                }
            }
        }
        store(&mut bytes, &source, vp);
        store(&mut bytes, &projection_source, projection);
        let decoded = read_bound_camera_matrix(&b, &source, &bytes).unwrap();
        for (actual, expected) in decoded.iter().flatten().zip(view.iter().flatten()) {
            assert!((actual - expected).abs() < 1e-5);
        }
        assert_eq!(
            read_bound_camera_matrix(&b, &projection_source, &bytes),
            Some(projection)
        );

        let mut legacy = b.clone();
        legacy.holstered_camera.as_mut().unwrap().schema =
            crate::cp2077_bound_camera::POLICY.into();
        assert_eq!(read_bound_camera_matrix(&legacy, &source, &bytes), Some(vp));
        assert!(resolve_pose(&legacy, &vp, &projection).is_err());
        for fault in 0..8 {
            let mut bad = b.clone();
            match fault {
                0 => bad.projection_sources.clear(),
                1 => {
                    bad.projection_sources[0]
                        .descriptor_table
                        .root_parameter_index += 1
                }
                2 => bad.projection_sources[0].cbv_resource_offset += 256,
                3 => bad.projection_sources[0].declared_cbv_bytes += 256,
                4 => bad.projection_sources[0].matrix_offset = source.matrix_offset,
                5 => bad.projection_sources[0].role = crate::MatrixRole::View,
                6 => bad.schema = crate::CAMERA_BINDING_SCHEMA_V2.into(),
                _ => bad.holstered_camera.as_mut().unwrap().schema = "unknown-policy".into(),
            }
            assert!(
                read_bound_camera_matrix(&bad, &source, &bytes).is_none(),
                "fault {fault}"
            );
        }
        assert!(read_bound_camera_matrix(&b, &source, &bytes[..127]).is_none());
        for (r, c, value) in [(0, 0, 0.), (2, 3, 0.), (3, 2, 0.), (0, 0, f32::NAN)] {
            let mut malformed = bytes.clone();
            let mut bad_projection = projection;
            bad_projection[r][c] = value;
            store(&mut malformed, &projection_source, bad_projection);
            assert!(read_bound_camera_matrix(&b, &source, &malformed).is_none());
        }
    }

    #[test]
    fn v2_pose_round_trip_retains_provenance_and_detects_tampering() {
        let row = row();
        assert_eq!(row.camera_position, [1., 2., 3.]);
        assert_eq!(row.camera_intrinsics.fx, 960.);
        let encoded = serde_json::to_vec(&row).unwrap();
        let decoded = serde_json::from_slice(&encoded).unwrap();
        validate_camera_pose_row(&decoded).unwrap();
        let mut forged = row.clone();
        forged.camera_position[0] += 0.1;
        assert!(validate_camera_pose_row(&forged).is_err());
        forged = row;
        forged.schema = crate::CAMERA_POSE_INDEX_SCHEMA_V1.into();
        assert!(validate_camera_pose_row(&forged).is_err());
    }
    #[test]
    fn signed_axis_and_metric_mapping_apply_to_position_and_rotation() {
        let mut binding = binding();
        binding.view_metres_per_unit = "2".into();
        binding.axis_mapping.delivery_y_from = D3d12SignedAxis::PositiveZ;
        binding.axis_mapping.delivery_z_from = D3d12SignedAxis::NegativeY;
        let (mut view, projection) = matrices();
        let (position, _, _) = resolve_pose(&binding, &view, &projection).unwrap();
        assert_eq!(position, [2., 6., -4.]);
        view = [
            [0., 0., -1., 0.],
            [0., 1., 0., 0.],
            [1., 0., 0., 0.],
            [0., 0., 0., 1.],
        ];
        let (_, q, _) = resolve_pose(&binding, &view, &projection).unwrap();
        assert!(q[0].abs() < 1e-6 && q[1].abs() < 1e-6);
        assert!((q[2] + std::f64::consts::FRAC_1_SQRT_2).abs() < 1e-6);
        assert!((q[3] - std::f64::consts::FRAC_1_SQRT_2).abs() < 1e-6);
    }

    #[test]
    fn declared_translation_accepts_the_origin_but_rejects_non_affine_views() {
        let mut binding = binding();
        binding.view_sources[0].translation = TranslationSlot::Col3;
        let (mut view, projection) = matrices();
        view[3] = [0., 0., 0., 1.];
        let (position, _, _) = resolve_pose(&binding, &view, &projection).unwrap();
        assert_eq!(position, [0.; 3]);
        view[0][3] = -2.;
        assert_eq!(
            resolve_pose(&binding, &view, &projection).unwrap().0,
            [2., 0., 0.]
        );
        view[3][3] = 2.;
        assert!(resolve_pose(&binding, &view, &projection).is_err());
        view[3][3] = 1.;
        view[3][0] = 1.;
        assert!(resolve_pose(&binding, &view, &projection).is_err());
    }
    #[test]
    fn bound_reads_respect_offset_layout_transpose_and_length() {
        let (_, projection) = matrices();
        let mut source = binding().projection_sources.remove(0);
        let mut bytes = vec![0; 256];
        for (chunk, value) in bytes[64..128]
            .chunks_exact_mut(4)
            .zip(projection.iter().flatten())
        {
            chunk.copy_from_slice(&value.to_le_bytes());
        }
        assert_eq!(read_bound_matrix(&source, &bytes), Some(projection));
        source.layout = MatrixLayout::ColumnMajor;
        source.transpose = true;
        assert_eq!(read_bound_matrix(&source, &bytes), Some(projection));
        assert!(read_bound_matrix(&source, &bytes[..127]).is_none());
        bytes[64..68].copy_from_slice(&f32::NAN.to_le_bytes());
        assert!(read_bound_matrix(&source, &bytes).is_none());
    }

    #[test]
    fn holstered_row_view_uses_independent_camera_basis_and_preserves_native_matrices() {
        let b = crate::cp2077_bound_camera::tests::binding();
        // Known camera: position (2,3,4), +90 degrees about world Z. Native camera Y
        // points down. A delivery camera point (1,2,3) must land at world (0,4,7).
        let view = [
            [0., 1., 0., 0.],
            [1., 0., 0., 0.],
            [0., 0., 1., 0.],
            [-3., -2., -4., 1.],
        ];
        let (_, mut projection) = matrices();
        projection[2][0] = 0.0002;
        let context = CameraPoseStreamContext {
            identity: StreamIdentity {
                session_id: "synthetic".into(),
                run_id: "holstered".into(),
                generation: 1,
            },
            dsv_id: "scene".into(),
        };
        let row = assemble_pose(&b, &context, 120, 1000, view, projection).unwrap();
        assert_eq!(row.camera_position, [2., 3., 4.]);
        let q = row.camera_rotation_quaternion;
        assert!(q[0].abs() < 1e-6 && q[1].abs() < 1e-6);
        assert!((q[2] - std::f64::consts::FRAC_1_SQRT_2).abs() < 1e-6);
        assert!((q[3] - std::f64::consts::FRAC_1_SQRT_2).abs() < 1e-6);
        validate_camera_pose_row(&row).unwrap();
        let CameraPoseProvenance::ResolvedD3d12CameraHook {
            view_matrix,
            projection_matrix,
            ..
        } = &row.provenance
        else {
            panic!()
        };
        assert_eq!(**view_matrix, view);
        assert_eq!(**projection_matrix, projection);
        let mut wrong = b.clone();
        wrong.holstered_camera.as_mut().unwrap().camera_axis_mapping = b.axis_mapping;
        assert!(resolve_pose(&wrong, &view, &projection).is_err());
        let mut forged = row;
        forged.camera_rotation_quaternion[2] = -forged.camera_rotation_quaternion[2];
        assert!(validate_camera_pose_row(&forged).is_err());

        // Independent non-symmetric 3-axis case: +90 around X then +90 around Z.
        // This catches mistakenly transposing a row-vector view's rotation block.
        let view = [
            [0., 0., 1., 0.],
            [1., 0., 0., 0.],
            [0., -1., 0., 0.],
            [-3., 4., -2., 1.],
        ];
        let (position, q, _) = resolve_pose(&b, &view, &projection).unwrap();
        assert_eq!(position, [2., 3., 4.]);
        assert!(q.iter().all(|v| (*v - 0.5).abs() < 1e-6));
    }
    #[test]
    fn formal_pair_rejects_cross_backend_and_wrong_build() {
        let binding = binding();
        let mut profile: crate::DepthProfile = serde_json::from_str(include_str!(
            "../../../config/catalogs/dev/fallout4-steam-release-test/depth-profile.json"
        ))
        .unwrap();
        profile.game_id = binding.profile_id.clone();
        profile.schema_version = binding.profile_version.clone();
        profile.executable_sha256 = binding.executable_sha256.clone();
        profile.renderer_backend = crate::RendererBackend::D3d12;
        profile.readback_path = crate::profile::ReadbackPath::D3d12CopyTextureRegion;
        profile.metric_scale.evidence_sha256 = Some(format!("sha256:{}", "c".repeat(64)));
        let document = CameraBindingDocument::V2(binding.clone());
        document
            .validate_for_profile(&profile, &binding.executable_sha256)
            .unwrap();
        assert!(
            document
                .validate_for_profile(&profile, &format!("sha256:{}", "d".repeat(64)))
                .is_err()
        );
        profile.renderer_backend = crate::RendererBackend::D3d11;
        assert!(
            document
                .validate_for_profile(&profile, &binding.executable_sha256)
                .is_err()
        );
    }
}

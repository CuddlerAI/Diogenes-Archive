//! Local fixed-view diagnostic policy. Never removes jitter from source matrices.
use anyhow::{Context as _, ensure};
use serde::Serialize;

use super::{RenderMatrix, matrix_delta};

const OPTICAL_DELTA_LIMIT: f64 = 0.001;
const HALF_PIXEL_LIMIT: f64 = 0.5;

#[derive(Debug, Serialize)]
pub(super) struct ProjectionValidation {
    pub policy: &'static str,
    pub maximum_matrix_delta: f64,
    pub maximum_non_offset_delta: f64,
    pub principal_offset_x_pixels: [f64; 2],
    pub principal_offset_y_pixels: [f64; 2],
    pub source_matrices_preserved: bool,
}

pub(super) fn validate(
    samples: &[RenderMatrix],
    width: u32,
    height: u32,
    allow_subpixel_offsets: bool,
) -> anyhow::Result<ProjectionValidation> {
    ensure!(
        width > 0 && height > 0,
        "projection dimensions must be nonzero"
    );
    let first = samples.first().context("projection samples are empty")?;
    ensure!(
        samples.iter().flatten().flatten().all(|v| v.is_finite()),
        "projection contains non-finite values"
    );
    let mut report = ProjectionValidation {
        policy: if allow_subpixel_offsets {
            "fixed_view_centered_half_pixel/v1"
        } else {
            "full_matrix_stability/v1"
        },
        maximum_matrix_delta: 0.0,
        maximum_non_offset_delta: 0.0,
        principal_offset_x_pixels: [f64::INFINITY, f64::NEG_INFINITY],
        principal_offset_y_pixels: [f64::INFINITY, f64::NEG_INFINITY],
        source_matrices_preserved: true,
    };
    for matrix in samples {
        // This exception is only defined for the observed row-vector perspective
        // family. A transpose, oblique or non-normalized matrix is not "jitter".
        if allow_subpixel_offsets {
            ensure!(
                matrix[0][0] > 0.0
                    && matrix[1][1] > 0.0
                    && (matrix[2][3] - 1.0).abs() <= 1e-6
                    && matrix[3][2].abs() > f64::EPSILON,
                "subpixel projection policy requires normalized perspective geometry"
            );
            for (row, column) in [
                (0, 1),
                (0, 2),
                (0, 3),
                (1, 0),
                (1, 2),
                (1, 3),
                (3, 0),
                (3, 1),
                (3, 3),
            ] {
                ensure!(
                    matrix[row][column].abs() <= 1e-6,
                    "subpixel projection policy rejects oblique/transposed geometry at [{row}][{column}]"
                );
            }
        }
        report.maximum_matrix_delta = report
            .maximum_matrix_delta
            .max(matrix_delta(*first, *matrix));
        for row in 0..4 {
            for column in 0..4 {
                if row == 2 && column < 2 {
                    continue;
                }
                report.maximum_non_offset_delta = report
                    .maximum_non_offset_delta
                    .max((matrix[row][column] - first[row][column]).abs());
            }
        }
        // Same signed principal-point convention as camera::intrinsics_from_projection.
        // No average, zeroing, timestamp repair or source rewrite is performed.
        let x = -matrix[2][0] * f64::from(width) / 2.0;
        let y = matrix[2][1] * f64::from(height) / 2.0;
        report.principal_offset_x_pixels[0] = report.principal_offset_x_pixels[0].min(x);
        report.principal_offset_x_pixels[1] = report.principal_offset_x_pixels[1].max(x);
        report.principal_offset_y_pixels[0] = report.principal_offset_y_pixels[0].min(y);
        report.principal_offset_y_pixels[1] = report.principal_offset_y_pixels[1].max(y);
        if allow_subpixel_offsets {
            ensure!(
                x.abs() <= HALF_PIXEL_LIMIT + 1e-6 && y.abs() <= HALF_PIXEL_LIMIT + 1e-6,
                "projection principal-point offset exceeds half a native pixel: x={x:.6}, y={y:.6}"
            );
        }
    }
    ensure!(
        report.maximum_non_offset_delta <= OPTICAL_DELTA_LIMIT,
        "projection focal/depth/homogeneous coefficients changed: delta={:.9}, limit={OPTICAL_DELTA_LIMIT}",
        report.maximum_non_offset_delta
    );
    if !allow_subpixel_offsets {
        ensure!(
            report.maximum_matrix_delta <= OPTICAL_DELTA_LIMIT,
            "projection changed under the full-matrix policy: delta={:.9}, limit={OPTICAL_DELTA_LIMIT}",
            report.maximum_matrix_delta
        );
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn perspective() -> RenderMatrix {
        [
            [1.2891924, 0., 0., 0.],
            [0., 2.2918978, 0., 0.],
            [0., 0., -0.0000011920929, 1.],
            [0., 0., 0.020000024, 0.],
        ]
    }
    #[test]
    fn native_half_pixel_offsets_do_not_change_focal_or_depth_coefficients() {
        let mut left = perspective();
        let mut right = perspective();
        left[2][0] = 0.5 * 2. / 1920.;
        left[2][1] = -0.5 * 2. / 1080.;
        right[2][0] = -0.5 * 2. / 1920.;
        right[2][1] = 0.5 * 2. / 1080.;
        let source = [left, right];
        let audit = validate(&source, 1920, 1080, true).unwrap();
        assert!(audit.maximum_matrix_delta > 0.001);
        assert_eq!(audit.maximum_non_offset_delta, 0.);
        assert_eq!(audit.principal_offset_y_pixels, [-0.5, 0.5]);
        assert_eq!(source, [left, right]);
        assert!(validate(&source, 1920, 1080, false).is_err());
    }
    #[test]
    fn optical_changes_and_large_static_offsets_are_not_jitter() {
        for (r, c) in [(0, 0), (1, 1), (2, 2), (3, 2), (2, 3)] {
            let initial = perspective();
            let mut changed = initial;
            changed[r][c] += 0.01;
            assert!(validate(&[initial, changed], 1920, 1080, true).is_err());
        }
        for (axis, size) in [(0, 1920.), (1, 1080.)] {
            let mut invalid = perspective();
            invalid[2][axis] = 0.51 * 2. / size;
            assert!(validate(&[invalid, invalid], 1920, 1080, true).is_err());
        }
    }
    #[test]
    fn malformed_and_unknown_projection_families_fail_closed() {
        assert!(validate(&[], 1920, 1080, true).is_err());
        assert!(validate(&[perspective()], 0, 1080, true).is_err());
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let mut invalid = perspective();
            invalid[0][0] = value;
            assert!(validate(&[invalid], 1920, 1080, true).is_err());
        }
        let mut transposed = perspective();
        transposed[2][3] = 0.02;
        transposed[3][2] = 1.;
        assert!(validate(&[transposed], 1920, 1080, true).is_err());
        let mut opposite_handedness = perspective();
        opposite_handedness[2][3] = -1.;
        assert!(validate(&[opposite_handedness], 1920, 1080, true).is_err());
        let mut oblique = perspective();
        oblique[0][2] = 0.02;
        assert!(validate(&[oblique], 1920, 1080, true).is_err());
    }
}

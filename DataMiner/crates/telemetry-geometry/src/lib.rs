//! 游戏无关的位姿几何:正交性校验、轴变换、矩阵→四元数。
//!
//! 本 crate 从 `fallout-player-telemetry` 抽出,因为它一个字都不涉及具体游戏——
//! 它做的是「已知源坐标系与目标坐标系的轴对应关系,把点/旋转/速度搬过去,
//! 并在搬之前搬之后都拒绝不合法的旋转」。第二款游戏(ETS2)要用同一套,
//! 复制一份四元数代码是错的。
//!
//! # 这里刻意**不**做的事
//!
//! **本 crate 不知道任何游戏的轴语义。** 轴对应矩阵 `axes` 永远由调用方传入,
//! 因为它是每款游戏必须独立取证的量——[98 号 §4.3.2](../../../docs/98-steam-wide-map-metric-actioncamera-source-survey.md)
//! 第 3 条:「左右手系与轴序几乎没有一款与 68 号一致……轴变换必须逐款验证符号,不能只重排」。
//! 把某一款的轴矩阵写成本 crate 的默认值,就是把「必须逐款取证」偷偷降级成「默认值」。
//!
//! # 手性翻转是允许的,退化不是
//!
//! [`transform_rotation`] 只要求 `axes` 正交,**不要求 `det(axes) = +1`**:
//! 源是右手系、目标是左手系时 `det = −1` 正是对的(Fallout 与 ETS2 两款都是这种情形)。
//! 但它要求**源旋转**与**结果旋转**都是 `det = +1` 的纯旋转——
//! 相似变换 `axes · R · axesᵀ` 会把 `det = −1` 的镜像原样带过去,
//! 那种输入进来只会静默产出一个镜像的四元数。

pub fn determinant3(matrix: [[f64; 3]; 3]) -> f64 {
    matrix[0][0] * (matrix[1][1] * matrix[2][2] - matrix[1][2] * matrix[2][1])
        - matrix[0][1] * (matrix[1][0] * matrix[2][2] - matrix[1][2] * matrix[2][0])
        + matrix[0][2] * (matrix[1][0] * matrix[2][1] - matrix[1][1] * matrix[2][0])
}

pub fn matrix_is_orthonormal(matrix: [[f64; 3]; 3], tolerance: f64) -> bool {
    if !matrix.iter().flatten().all(|value| value.is_finite()) {
        return false;
    }
    for row in 0..3 {
        for other in 0..3 {
            let dot = (0..3)
                .map(|column| matrix[row][column] * matrix[other][column])
                .sum::<f64>();
            let expected = if row == other { 1.0 } else { 0.0 };
            if (dot - expected).abs() > tolerance {
                return false;
            }
        }
    }
    true
}

pub fn transform_point(axes: [[f64; 3]; 3], scale: f64, point: [f64; 3]) -> Option<[f64; 3]> {
    if !point.iter().all(|value| value.is_finite()) {
        return None;
    }
    let result = multiply_vector(axes, point).map(|value| value * scale);
    result
        .iter()
        .all(|value| value.is_finite())
        .then_some(result)
}

pub fn transform_rotation(axes: [[f64; 3]; 3], source_rotation: [[f64; 3]; 3]) -> Option<[f64; 4]> {
    if !matrix_is_orthonormal(source_rotation, 1e-3)
        || (determinant3(source_rotation) - 1.0).abs() > 1e-3
    {
        return None;
    }
    let target_rotation = multiply_matrix(multiply_matrix(axes, source_rotation), transpose(axes));
    if !matrix_is_orthonormal(target_rotation, 2e-3)
        || (determinant3(target_rotation) - 1.0).abs() > 2e-3
    {
        return None;
    }
    matrix_to_quaternion_xyzw(target_rotation)
}

pub fn transform_speed_magnitude(
    axes: [[f64; 3]; 3],
    scale: f64,
    velocity: [f64; 3],
) -> Option<f64> {
    let velocity_mps = transform_point(axes, scale, velocity)?;
    let speed = velocity_mps
        .iter()
        .map(|value| value * value)
        .sum::<f64>()
        .sqrt();
    (speed.is_finite() && speed >= 0.0).then_some(speed)
}

fn multiply_vector(matrix: [[f64; 3]; 3], vector: [f64; 3]) -> [f64; 3] {
    matrix.map(|row| {
        row.iter()
            .zip(vector)
            .map(|(left, right)| left * right)
            .sum()
    })
}

fn multiply_matrix(left: [[f64; 3]; 3], right: [[f64; 3]; 3]) -> [[f64; 3]; 3] {
    let right_t = transpose(right);
    left.map(|row| {
        right_t.map(|column| {
            row.iter()
                .zip(column)
                .map(|(left, right)| left * right)
                .sum()
        })
    })
}

fn transpose(matrix: [[f64; 3]; 3]) -> [[f64; 3]; 3] {
    [
        [matrix[0][0], matrix[1][0], matrix[2][0]],
        [matrix[0][1], matrix[1][1], matrix[2][1]],
        [matrix[0][2], matrix[1][2], matrix[2][2]],
    ]
}

fn matrix_to_quaternion_xyzw(matrix: [[f64; 3]; 3]) -> Option<[f64; 4]> {
    let trace = matrix[0][0] + matrix[1][1] + matrix[2][2];
    let (x, y, z, w) = if trace > 0.0 {
        let s = (trace + 1.0).sqrt() * 2.0;
        (
            (matrix[2][1] - matrix[1][2]) / s,
            (matrix[0][2] - matrix[2][0]) / s,
            (matrix[1][0] - matrix[0][1]) / s,
            0.25 * s,
        )
    } else if matrix[0][0] > matrix[1][1] && matrix[0][0] > matrix[2][2] {
        let s = (1.0 + matrix[0][0] - matrix[1][1] - matrix[2][2]).sqrt() * 2.0;
        (
            0.25 * s,
            (matrix[0][1] + matrix[1][0]) / s,
            (matrix[0][2] + matrix[2][0]) / s,
            (matrix[2][1] - matrix[1][2]) / s,
        )
    } else if matrix[1][1] > matrix[2][2] {
        let s = (1.0 + matrix[1][1] - matrix[0][0] - matrix[2][2]).sqrt() * 2.0;
        (
            (matrix[0][1] + matrix[1][0]) / s,
            0.25 * s,
            (matrix[1][2] + matrix[2][1]) / s,
            (matrix[0][2] - matrix[2][0]) / s,
        )
    } else {
        let s = (1.0 + matrix[2][2] - matrix[0][0] - matrix[1][1]).sqrt() * 2.0;
        (
            (matrix[0][2] + matrix[2][0]) / s,
            (matrix[1][2] + matrix[2][1]) / s,
            0.25 * s,
            (matrix[1][0] - matrix[0][1]) / s,
        )
    };
    let mut quaternion = [x, y, z, w];
    if !quaternion.iter().all(|value| value.is_finite()) {
        return None;
    }
    let norm = quaternion
        .iter()
        .map(|value| value * value)
        .sum::<f64>()
        .sqrt();
    if !norm.is_finite() || norm <= f64::EPSILON {
        return None;
    }
    for value in &mut quaternion {
        *value /= norm;
    }
    if quaternion[3] < 0.0 {
        for value in &mut quaternion {
            *value = -*value;
        }
    }
    Some(quaternion)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 源右手系 → 目标左手系时 `det(axes) = −1`，这是**正确**而不是错误。
    /// 若哪天给 `axes` 也加上 `det = +1` 的校验，Fallout 与 ETS2 两款会同时静默失效。
    #[test]
    fn handedness_flipping_axes_are_accepted() {
        // Fallout：右/前/上（RH）→ 右/上/前（LH），交换源 Y、Z。
        let fallout = [[1.0, 0.0, 0.0], [0.0, 0.0, 1.0], [0.0, 1.0, 0.0]];
        // ETS2：东/上/南（RH）→ 右/上/前（LH），翻转源 Z。
        let ets2 = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, -1.0]];
        for axes in [fallout, ets2] {
            assert!(matrix_is_orthonormal(axes, 1e-12));
            assert!((determinant3(axes) + 1.0).abs() < 1e-12, "两款都应是 −1");
            assert!(transform_rotation(axes, IDENTITY).is_some());
        }
    }

    /// 单位旋转经任何合法轴变换后仍是单位旋转——四元数必须是 `(0,0,0,1)`。
    #[test]
    fn identity_rotation_survives_axis_change() {
        let ets2 = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, -1.0]];
        let q = transform_rotation(ets2, IDENTITY).expect("单位旋转必须可转换");
        assert!((q[0]).abs() < 1e-12 && (q[1]).abs() < 1e-12 && (q[2]).abs() < 1e-12);
        assert!((q[3] - 1.0).abs() < 1e-12);
    }

    /// 镜像矩阵（`det = −1`）不是旋转。它进来只会静默产出一个镜像四元数，
    /// 所以必须在入口就拒绝——这正是 88 号那次垂直翻转事故的同类形状。
    #[test]
    fn mirrored_source_rotation_is_rejected() {
        let mirror = [[-1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
        assert!(matrix_is_orthonormal(mirror, 1e-12), "镜像仍然是正交的");
        assert!((determinant3(mirror) + 1.0).abs() < 1e-12);
        assert!(
            transform_rotation(IDENTITY, mirror).is_none(),
            "正交但 det=−1 的源旋转必须被拒"
        );
    }

    /// 速度大小是标量，任何**保长度**的轴变换都不该改变它。
    #[test]
    fn speed_magnitude_is_invariant_under_axis_change() {
        let ets2 = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, -1.0]];
        let v = [3.0, -4.0, 12.0];
        let speed = transform_speed_magnitude(ets2, 1.0, v).expect("有限输入必须有解");
        assert!((speed - 13.0).abs() < 1e-12);
    }

    /// 非有限值一律走 `None`，不允许变成 0 或 NaN 继续往下流。
    #[test]
    fn non_finite_inputs_fail_closed() {
        assert!(transform_point(IDENTITY, 1.0, [f64::NAN, 0.0, 0.0]).is_none());
        assert!(transform_speed_magnitude(IDENTITY, 1.0, [f64::INFINITY, 0.0, 0.0]).is_none());
        let nan_rotation = [[f64::NAN, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
        assert!(transform_rotation(IDENTITY, nan_rotation).is_none());
    }

    const IDENTITY: [[f64; 3]; 3] = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
}

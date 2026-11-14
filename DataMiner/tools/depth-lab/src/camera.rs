//! P4：从常量缓冲里认出 view / projection 矩阵，并提取交付所需的相机量。
//!
//! 对应 [77 号 §5.11](../../../docs/77-depth-ground-truth-hook-experiment-plan.md)。
//!
//! # 这里是 P4 的全部判断，probe 侧只负责把字节递进来
//!
//! probe 在主场景 pass 期间 hook `Map`/`UpdateSubresource`，把常量缓冲的字节交给
//! [`scan_candidates`]。**认矩阵、验矩阵、算位姿全在这个模块**，
//! 因为这些是纯计算、可以在任意平台测死，而 Windows 侧的 hook 无法在开发机验证。
//!
//! # ⚠️ 本模块给不了 `player_*`
//!
//! 相机矩阵是渲染状态（每帧必须上传 GPU），玩家位姿是游戏逻辑状态（不必出现在
//! 任何 GPU 资源里）。第三人称下也无法从相机反推——实测相机与玩家连线相对
//! 相机前向的夹角为 158.22° ± 5.45°，不是稳定的正后方
//! （[80 号 §5](../../../docs/80-action-camera-deliverable-gap-analysis.md)）。

use serde::{Deserialize, Serialize};

/// 行优先的 4×4 矩阵，`m[行][列]`。
pub type Mat4 = [[f32; 4]; 4];

/// 平移分量存在哪里。
///
/// D3D 传统用行向量（`v·M`）、平移在**第 3 行**；GL 用列向量（`M·v`）、平移在**第 3 列**。
/// 两者对同一个变换的字节布局其实一致，所以实操上不猜约定，**看哪一侧非零**。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TranslationSlot {
    /// 平移在 `m[3][0..2]`（行向量约定）。
    Row3,
    /// 平移在 `m[0..2][3]`（列向量约定）。
    Col3,
}

/// 一个候选矩阵的分类结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatrixKind {
    /// 透视投影矩阵。
    Projection,
    /// 刚体视图矩阵（左上 3×3 正交）。
    View { translation: TranslationSlot },
    /// 都不像。
    Unknown,
}

/// A matrix found in raw constant-buffer bytes.
///
/// `matrix` is normalized to the orientation consumed by the camera math in
/// this crate.  `transpose` records whether production decoding must transpose
/// the raw 16 floats to reproduce that normalized matrix.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScannedMatrix {
    pub offset: usize,
    pub matrix: Mat4,
    pub kind: MatrixKind,
    pub transpose: bool,
}

/// 左上 3×3 偏离正交的程度：`max |R·Rᵀ − I|`。
///
/// **这是视图矩阵最可靠的单条判据**——随机 float 几乎不可能满足。
/// 实测需求方样本的相机旋转矩阵，该值约 1.4e-8。
#[must_use]
pub fn orthonormality_error(m: &Mat4) -> f32 {
    let mut worst = 0.0f32;
    for i in 0..3 {
        for j in 0..3 {
            let dot: f32 = (0..3).map(|k| m[i][k] * m[j][k]).sum();
            let want = if i == j { 1.0 } else { 0.0 };
            worst = worst.max((dot - want).abs());
        }
    }
    worst
}

/// 正交判据的门限。放宽到 1e-3 是给 fp32 累积误差留余量；
/// 实测真实矩阵在 1e-8 量级，随机数据远超 1e-3，中间隔着好几个数量级。
pub const ORTHONORMAL_TOLERANCE: f32 = 1e-3;

/// 判断一个候选是什么。
///
/// 顺序有意义：**先判投影**。透视投影矩阵的 `m[3][3] == 0` 是个极强的结构标志，
/// 而它的左上 3×3 恰好也可能碰巧接近正交（各向同性 FOV 时），先判视图会误分类。
#[must_use]
pub fn classify(m: &Mat4) -> MatrixKind {
    if is_perspective_projection(m) {
        return MatrixKind::Projection;
    }
    if orthonormality_error(m) <= ORTHONORMAL_TOLERANCE {
        // 平移在哪一侧：取非零的那侧。都为零（纯旋转）时按行向量约定。
        let row = m[3][0].abs() + m[3][1].abs() + m[3][2].abs();
        let col = m[0][3].abs() + m[1][3].abs() + m[2][3].abs();
        return MatrixKind::View {
            translation: if col > row {
                TranslationSlot::Col3
            } else {
                TranslationSlot::Row3
            },
        };
    }
    MatrixKind::Unknown
}

/// 透视投影矩阵的结构判据。
fn is_perspective_projection(m: &Mat4) -> bool {
    const EPS: f32 = 1e-5;
    let zeroish = |v: f32| v.abs() < EPS;
    // m[3][3] == 0 是透视的标志（正交投影是 1）
    if !zeroish(m[3][3]) {
        return false;
    }
    // m[2][3] == ±1
    if (m[2][3].abs() - 1.0).abs() > 1e-3 {
        return false;
    }
    // 对角占优、且几处固定零元
    if m[0][0].abs() < EPS || m[1][1].abs() < EPS {
        return false;
    }
    zeroish(m[0][1]) && zeroish(m[0][2]) && zeroish(m[1][0]) && zeroish(m[1][2])
}

/// 相机在世界中的位姿。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CameraPose {
    /// 世界坐标。
    pub position: [f32; 3],
    /// 世界旋转，四元数 **`[x, y, z, w]`（w 在最后）**——与需求方样本一致。
    pub rotation: [f32; 4],
}

/// 从视图矩阵还原相机位姿。
///
/// 视图矩阵把世界变换到相机空间。列向量矩阵满足
/// `V = [Rv | tv]`、`p = −Rvᵀ·tv`；行向量矩阵满足
/// `V = [Rv; tv]`、`p = −tv·Rvᵀ`。两者的世界旋转均为 `Rvᵀ`。
/// **刚体不必真求逆**——转置就够了。
#[must_use]
pub fn pose_from_view(m: &Mat4, translation: TranslationSlot) -> CameraPose {
    let rv = [
        [m[0][0], m[0][1], m[0][2]],
        [m[1][0], m[1][1], m[1][2]],
        [m[2][0], m[2][1], m[2][2]],
    ];
    let tv = match translation {
        TranslationSlot::Row3 => [m[3][0], m[3][1], m[3][2]],
        TranslationSlot::Col3 => [m[0][3], m[1][3], m[2][3]],
    };
    // R = Rvᵀ
    let r = [
        [rv[0][0], rv[1][0], rv[2][0]],
        [rv[0][1], rv[1][1], rv[2][1]],
        [rv[0][2], rv[1][2], rv[2][2]],
    ];
    let position = match translation {
        // Row-vector convention: p = −tv·Rvᵀ. Each result component is the
        // dot product of tv with one row of Rv.
        TranslationSlot::Row3 => [
            -(tv[0] * rv[0][0] + tv[1] * rv[0][1] + tv[2] * rv[0][2]),
            -(tv[0] * rv[1][0] + tv[1] * rv[1][1] + tv[2] * rv[1][2]),
            -(tv[0] * rv[2][0] + tv[1] * rv[2][1] + tv[2] * rv[2][2]),
        ],
        // Column-vector convention: p = −Rvᵀ·tv = −R·tv.
        TranslationSlot::Col3 => [
            -(r[0][0] * tv[0] + r[0][1] * tv[1] + r[0][2] * tv[2]),
            -(r[1][0] * tv[0] + r[1][1] * tv[1] + r[1][2] * tv[2]),
            -(r[2][0] * tv[0] + r[2][1] * tv[1] + r[2][2] * tv[2]),
        ],
    };
    CameraPose {
        position,
        rotation: mat3_to_quat(&r),
    }
}

/// 旋转矩阵 → 四元数 `[x, y, z, w]`。
///
/// 用 Shepperd 分支法（取最大对角项）而不是单一公式——后者在 `w ≈ 0`
/// 附近会做 `sqrt` 于接近零的数再相除，精度塌掉。
#[must_use]
pub fn mat3_to_quat(r: &[[f32; 3]; 3]) -> [f32; 4] {
    let trace = r[0][0] + r[1][1] + r[2][2];
    if trace > 0.0 {
        let s = (trace + 1.0).sqrt() * 2.0;
        [
            (r[2][1] - r[1][2]) / s,
            (r[0][2] - r[2][0]) / s,
            (r[1][0] - r[0][1]) / s,
            0.25 * s,
        ]
    } else if r[0][0] > r[1][1] && r[0][0] > r[2][2] {
        let s = (1.0 + r[0][0] - r[1][1] - r[2][2]).sqrt() * 2.0;
        [
            0.25 * s,
            (r[0][1] + r[1][0]) / s,
            (r[0][2] + r[2][0]) / s,
            (r[2][1] - r[1][2]) / s,
        ]
    } else if r[1][1] > r[2][2] {
        let s = (1.0 + r[1][1] - r[0][0] - r[2][2]).sqrt() * 2.0;
        [
            (r[0][1] + r[1][0]) / s,
            0.25 * s,
            (r[1][2] + r[2][1]) / s,
            (r[0][2] - r[2][0]) / s,
        ]
    } else {
        let s = (1.0 + r[2][2] - r[0][0] - r[1][1]).sqrt() * 2.0;
        [
            (r[0][2] + r[2][0]) / s,
            (r[1][2] + r[2][1]) / s,
            0.25 * s,
            (r[1][0] - r[0][1]) / s,
        ]
    }
}

/// 需求方样本的欧拉角 yaw 相对四元数的**恒定偏移**。
///
/// 实测：对全部 6011 帧做四元数→欧拉转换后与其 `camera_rotation_oula` 逐帧比对，
/// **pitch 与 roll 偏差恒为 0.0000°，yaw 偏差恒为 +90.0000°，标准差 0.0000°**，
/// 相机与玩家两组字段同偏移（[77 号 §5.11.4](../../../docs/77-depth-ground-truth-hook-experiment-plan.md)）。
///
/// **不复制这个偏移，产出的欧拉角与四元数会互相矛盾**——
/// 用欧拉角的消费方会拿到转了 90° 的相机，而用四元数的一切正常。
pub const DELIVERY_YAW_OFFSET_DEG: f32 = -90.0;

/// 四元数 → 交付格式的欧拉角 `[pitch, yaw, roll]`（度），**已含 90° yaw 偏移**。
#[must_use]
pub fn quat_to_delivery_euler(q: [f32; 4]) -> [f32; 3] {
    let [x, y, z, w] = q;
    let pitch = (2.0 * (w * x - y * z)).clamp(-1.0, 1.0).asin();
    let yaw = (2.0 * (w * y + x * z)).atan2(1.0 - 2.0 * (x * x + y * y));
    let roll = (2.0 * (w * z + x * y)).atan2(1.0 - 2.0 * (x * x + z * z));
    let wrap = |d: f32| {
        let m = (d + 180.0).rem_euclid(360.0) - 180.0;
        if m == -180.0 { 180.0 } else { m }
    };
    [
        wrap(pitch.to_degrees()),
        wrap(yaw.to_degrees() + DELIVERY_YAW_OFFSET_DEG),
        wrap(roll.to_degrees()),
    ]
}

/// 针孔内参。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Intrinsics {
    pub fx: f32,
    pub fy: f32,
    pub cx: f32,
    pub cy: f32,
}

/// 方形像素判定的容差。`fx`/`fy` 由同一个投影矩阵的两个对角项算出，
/// 方形像素时二者在数值上应当很接近；1e-3 的相对偏差足以容纳 fp32 误差，
/// 而需求方样本那种 16/9 的偏差是它的一千多倍。
pub const SQUARE_PIXEL_TOLERANCE: f32 = 1e-3;

impl Intrinsics {
    /// 像素长宽比 = `fx / fy`。
    ///
    /// 物理上相机只有一个焦距 `f`，`fx = f/像素宽`、`fy = f/像素高`，
    /// 所以 `fx/fy = 像素高/像素宽`。**方形像素 ⟺ 该值为 1**。
    #[must_use]
    pub fn pixel_aspect_ratio(&self) -> f32 {
        if self.fy == 0.0 {
            return f32::NAN;
        }
        self.fx / self.fy
    }

    /// 是否为方形像素。
    #[must_use]
    pub fn is_square_pixels(&self) -> bool {
        let r = self.pixel_aspect_ratio();
        r.is_finite() && (r - 1.0).abs() <= SQUARE_PIXEL_TOLERANCE
    }

    /// 交付前的自检。
    ///
    /// **我们按方形像素交付**（owner 2026-08-05 裁决）。若从投影矩阵算出的
    /// `fx` 与 `fy` 并不相等，说明二者之一成立：
    ///
    /// - 传进来的 `width`/`height` 不是实际渲染分辨率；
    /// - 这个游戏真的不用方形像素（极罕见）。
    ///
    /// 两种都必须**报出来**，而不是照样写一个 `pixel_aspect_ratio = 1.0` 了事——
    /// 那会让 manifest 里的声明与数据本身不符，而下游没有任何办法察觉。
    ///
    /// 需求方样本正是这种不符：`fx/fy = 16/9`、两轴 FOV 同为 48°
    /// （[75 号 §2.2](../../../docs/75-sfm-depth-spec-clarification-request.md)）。
    pub fn check_square_pixels(&self) -> Result<(), String> {
        if self.is_square_pixels() {
            return Ok(());
        }
        Err(format!(
            "内参不是方形像素：fx={:.4} fy={:.4}，比值 {:.6}（期望 1.0±{SQUARE_PIXEL_TOLERANCE}）。\
             我方按方形像素交付，此处不符说明渲染分辨率传错、或该游戏确实使用非方形像素——\
             两种都必须先查清，不得照写 pixel_aspect_ratio = 1.0",
            self.fx,
            self.fy,
            self.pixel_aspect_ratio()
        ))
    }
}

/// 从透视投影矩阵取内参。
///
/// **按方形像素给出**（`fx` 与 `fy` 各自由对应轴算，方形像素下二者相等）。
/// 需求方样本里 `fx/fy = 16/9`、两轴 FOV 同为 48°，与方形像素矛盾——
/// 已按 owner 裁决取方形像素，并在 manifest 记 `pixel_aspect_ratio = 1.0`，
/// 但首批交付前仍须确认（[75 号 §2.2](../../../docs/75-sfm-depth-spec-clarification-request.md)）。
#[must_use]
pub fn intrinsics_from_projection(m: &Mat4, width: f32, height: f32) -> Intrinsics {
    Intrinsics {
        fx: m[0][0] * width / 2.0,
        fy: m[1][1] * height / 2.0,
        cx: width / 2.0 * (1.0 - m[2][0]),
        cy: height / 2.0 * (1.0 + m[2][1]),
    }
}

/// 从常量缓冲字节里扫出所有像矩阵的 16 个连续 float。
///
/// **按 4 字节步进而不是 64 字节**：矩阵在常量缓冲里的偏移由着色器布局决定，
/// 不保证 64 字节对齐。步进 64 会漏掉大部分候选。
#[must_use]
pub fn scan_candidates(bytes: &[u8]) -> Vec<ScannedMatrix> {
    let mut out = Vec::new();
    if bytes.len() < 48 {
        return out;
    }
    for off in (0..=bytes.len().saturating_sub(64)).step_by(4) {
        if off + 64 > bytes.len() {
            break;
        }
        let mut m: Mat4 = [[0.0; 4]; 4];
        let mut sane = true;
        // 这里的 i/j 不只是索引 m，还要算字节偏移 `off + (i*4+j)*4`，
        // 换成迭代器反而要另外维护一个计数器。
        #[allow(clippy::needless_range_loop)]
        for i in 0..4 {
            for j in 0..4 {
                let k = off + (i * 4 + j) * 4;
                let v = f32::from_le_bytes([bytes[k], bytes[k + 1], bytes[k + 2], bytes[k + 3]]);
                // NaN / Inf / 荒谬量级直接否掉，省得后面白算
                if !v.is_finite() || v.abs() > 1.0e6 {
                    sane = false;
                }
                m[i][j] = v;
            }
        }
        if !sane {
            continue;
        }
        let kind = classify(&m);
        if kind != MatrixKind::Unknown
            && (!matches!(kind, MatrixKind::View { .. }) || (m[3][3] - 1.0).abs() <= 1e-5)
        {
            out.push(ScannedMatrix {
                offset: off,
                matrix: m,
                kind,
                transpose: false,
            });
            continue;
        }

        // HLSL constant buffers commonly contain the column-major byte image
        // of a matrix.  A perspective matrix then has its structural +/-1 at
        // m[3][2], while the canonical classifier expects it at m[2][3].
        // Normalize the candidate for all downstream math, but retain the
        // required production decode operation as provenance.
        let transposed = std::array::from_fn(|row| std::array::from_fn(|column| m[column][row]));
        let transposed_kind = classify(&transposed);
        if transposed_kind != MatrixKind::Unknown
            && (!matches!(transposed_kind, MatrixKind::View { .. })
                || (transposed[3][3] - 1.0).abs() <= 1e-5)
        {
            out.push(ScannedMatrix {
                offset: off,
                matrix: transposed,
                kind: transposed_kind,
                transpose: true,
            });
        }
    }

    // A production MatrixSourceBinding currently replays one physical 64-byte
    // F32 matrix. Do not discover synthetic 48-byte affine candidates until the
    // binding contract can encode and reproduce their implicit row or column.
    out
}

/// Recover strict rigid view matrices from a physical constant buffer that stores a
/// view-projection matrix next to its projection matrix.
///
/// Both multiplication conventions and both HLSL byte orientations are considered. A result is
/// retained only when the recovered matrix passes the existing rigid-view classifier; projection
/// bytes are never accepted as their own pair (which would trivially yield identity).
#[must_use]
pub fn scan_view_projection_pairs(bytes: &[u8]) -> Vec<ScannedMatrix> {
    if bytes.len() < 64 {
        return Vec::new();
    }
    let mut raw = Vec::<(usize, bool, Mat4, MatrixKind)>::new();
    for off in (0..=bytes.len() - 64).step_by(4) {
        let mut matrix = [[0.0_f32; 4]; 4];
        let mut sane = true;
        for (index, value) in matrix.iter_mut().flatten().enumerate() {
            let k = off + index * 4;
            *value = f32::from_le_bytes([bytes[k], bytes[k + 1], bytes[k + 2], bytes[k + 3]]);
            sane &= value.is_finite() && value.abs() <= 1.0e6;
        }
        if !sane {
            continue;
        }
        let transposed = std::array::from_fn(|row| std::array::from_fn(|col| matrix[col][row]));
        raw.push((off, false, matrix, classify(&matrix)));
        raw.push((off, true, transposed, classify(&transposed)));
    }

    let mut out = Vec::new();
    for &(projection_offset, projection_transpose, projection, projection_kind) in &raw {
        if projection_kind != MatrixKind::Projection {
            continue;
        }
        let Some(inverse_projection) = inverse4(&projection) else {
            continue;
        };
        for &(vp_offset, vp_transpose, view_projection, vp_kind) in &raw {
            if vp_offset == projection_offset && vp_transpose == projection_transpose {
                continue;
            }
            if vp_kind != MatrixKind::Unknown {
                continue;
            }
            for view in [
                multiply4(&view_projection, &inverse_projection),
                multiply4(&inverse_projection, &view_projection),
            ] {
                let kind = classify(&view);
                if !matches!(kind, MatrixKind::View { .. }) {
                    continue;
                }
                if out.iter().any(|candidate: &ScannedMatrix| {
                    candidate.offset == vp_offset
                        && candidate.transpose == vp_transpose
                        && candidate.matrix == view
                }) {
                    continue;
                }
                out.push(ScannedMatrix {
                    offset: vp_offset,
                    matrix: view,
                    kind,
                    transpose: vp_transpose,
                });
            }
        }
    }
    out
}

/// Recover the explicitly declared row-vector VP/P pair without scanning for
/// offsets, orientations or multiplication order. Use the same arithmetic as
/// discovery so its decoded matrices can be replayed from the actual bytes.
pub(crate) fn recover_row_view(view_projection: &Mat4, projection: &Mat4) -> Option<Mat4> {
    if classify(projection) != MatrixKind::Projection {
        return None;
    }
    let view = multiply4(view_projection, &inverse4(projection)?);
    (view.iter().flatten().all(|value| value.is_finite())
        && (view[3][3] - 1.0).abs() <= 1e-5
        && (0..3).all(|i| view[i][3].abs() <= 1e-5)
        && orthonormality_error(&view) <= ORTHONORMAL_TOLERANCE)
        .then_some(view)
}

fn multiply4(left: &Mat4, right: &Mat4) -> Mat4 {
    std::array::from_fn(|row| {
        std::array::from_fn(|column| {
            (0..4)
                .map(|inner| left[row][inner] * right[inner][column])
                .sum()
        })
    })
}

fn inverse4(matrix: &Mat4) -> Option<Mat4> {
    let mut augmented = [[0.0_f64; 8]; 4];
    for row in 0..4 {
        for column in 0..4 {
            augmented[row][column] = f64::from(matrix[row][column]);
        }
        augmented[row][4 + row] = 1.0;
    }
    for column in 0..4 {
        let pivot = (column..4).max_by(|&a, &b| {
            augmented[a][column]
                .abs()
                .total_cmp(&augmented[b][column].abs())
        })?;
        if augmented[pivot][column].abs() < 1.0e-12 {
            return None;
        }
        augmented.swap(column, pivot);
        let divisor = augmented[column][column];
        for value in &mut augmented[column] {
            *value /= divisor;
        }
        for row in 0..4 {
            if row == column {
                continue;
            }
            let factor = augmented[row][column];
            let pivot_row = augmented[column];
            for (value, pivot_value) in augmented[row].iter_mut().zip(pivot_row) {
                *value -= factor * pivot_value;
            }
        }
    }
    Some(std::array::from_fn(|row| {
        std::array::from_fn(|column| augmented[row][4 + column] as f32)
    }))
}

/// 逐帧相机旋转增量与鼠标位移的相关系数。
///
/// **这是 P4 的决定性验证**（77 号 §5.11.2 判据 3）：它不依赖任何游戏知识，
/// 纯靠「玩家转鼠标、画面就转」这个物理事实。相关性低即说明认错了矩阵。
///
/// 返回 `None` 表示样本不足或某一侧完全没有变化（无法定义相关性）。
#[must_use]
pub fn correlation(a: &[f32], b: &[f32]) -> Option<f32> {
    if a.len() != b.len() || a.len() < 2 {
        return None;
    }
    let n = a.len() as f32;
    let (ma, mb) = (a.iter().sum::<f32>() / n, b.iter().sum::<f32>() / n);
    let mut num = 0.0;
    let mut da = 0.0;
    let mut db = 0.0;
    for (x, y) in a.iter().zip(b) {
        let (u, v) = (x - ma, y - mb);
        num += u * v;
        da += u * u;
        db += v * v;
    }
    if da <= 0.0 || db <= 0.0 {
        return None;
    }
    Some(num / (da.sqrt() * db.sqrt()))
}

/// §5.11.7 验收项 P4-3 的门限：**低于即判识别错误，不得交付**。
pub const MIN_MOUSE_CORRELATION: f32 = 0.95;

#[cfg(test)]
mod tests {
    use super::*;

    /// 需求方真实样本的五帧（`camera_position`, `camera_rotation_quaternion`,
    /// `camera_rotation_oula`）。**用真值验，不是自己造数据自己验。**
    const SAMPLE: [([f64; 3], [f64; 4], [f64; 3]); 5] = [
        (
            [221.5546, 20.6489, 87.36293],
            [-0.1402371, 0.06679486, 0.009482644, 0.9878169],
            [-16.16019, -82.26324, 0.0],
        ),
        (
            [246.8531, 23.07949, 201.0828],
            [-0.1102587, -0.6479304, -0.09555978, 0.7475945],
            [-16.77954, -171.8301, 0.0],
        ),
        (
            [326.1782, 31.68708, 242.3104],
            [-0.06374509, -0.02521992, -0.001611439, 0.9976462],
            [-7.311951, -92.89618, 0.0],
        ),
        (
            [373.1933, 20.19283, 178.9216],
            [-0.06465909, 0.7623535, 0.07710605, 0.6392895],
            [-11.55075, 10.03539, 4.357109e-7],
        ),
        (
            [375.5424, 26.24112, 296.5677],
            [-0.0988746, 0.3131311, 0.03279808, 0.9439794],
            [-11.95898, -53.29718, 0.0],
        ),
    ];

    fn quat_to_mat3(q: [f32; 4]) -> [[f32; 3]; 3] {
        let [x, y, z, w] = q;
        [
            [
                1.0 - 2.0 * (y * y + z * z),
                2.0 * (x * y - w * z),
                2.0 * (x * z + w * y),
            ],
            [
                2.0 * (x * y + w * z),
                1.0 - 2.0 * (x * x + z * z),
                2.0 * (y * z - w * x),
            ],
            [
                2.0 * (x * z - w * y),
                2.0 * (y * z + w * x),
                1.0 - 2.0 * (x * x + y * y),
            ],
        ]
    }

    /// 由相机位姿合成游戏会上传的行向量视图矩阵：`V = [Rᵀ; −p·Rᵀ]`。
    fn view_from_pose(p: [f32; 3], q: [f32; 4]) -> Mat4 {
        let r = quat_to_mat3(q);
        let rt = [
            [r[0][0], r[1][0], r[2][0]],
            [r[0][1], r[1][1], r[2][1]],
            [r[0][2], r[1][2], r[2][2]],
        ];
        let t = [
            -(p[0] * rt[0][0] + p[1] * rt[1][0] + p[2] * rt[2][0]),
            -(p[0] * rt[0][1] + p[1] * rt[1][1] + p[2] * rt[2][1]),
            -(p[0] * rt[0][2] + p[1] * rt[1][2] + p[2] * rt[2][2]),
        ];
        [
            [rt[0][0], rt[0][1], rt[0][2], 0.0],
            [rt[1][0], rt[1][1], rt[1][2], 0.0],
            [rt[2][0], rt[2][1], rt[2][2], 0.0],
            [t[0], t[1], t[2], 1.0],
        ]
    }

    fn perspective(hfov_deg: f32, aspect: f32, near: f32, far: f32) -> Mat4 {
        let f = 1.0 / (hfov_deg.to_radians() / 2.0).tan();
        [
            [f, 0.0, 0.0, 0.0],
            [0.0, f * aspect, 0.0, 0.0],
            [0.0, 0.0, far / (far - near), 1.0],
            [0.0, 0.0, -near * far / (far - near), 0.0],
        ]
    }

    #[test]
    fn pose_round_trips_on_the_real_sample() {
        // 本模块最重要的一条：用需求方真实数据做往返，不是自造数据自验。
        for (p, q, _) in SAMPLE {
            let pf = [p[0] as f32, p[1] as f32, p[2] as f32];
            let qf = [q[0] as f32, q[1] as f32, q[2] as f32, q[3] as f32];
            let v = view_from_pose(pf, qf);

            assert_eq!(
                classify(&v),
                MatrixKind::View {
                    translation: TranslationSlot::Row3
                }
            );
            let got = pose_from_view(&v, TranslationSlot::Row3);

            for (i, (g, w)) in got.position.iter().zip(pf.iter()).enumerate() {
                assert!((g - w).abs() < 1e-2, "位置分量 {i}: 得到 {g} 期望 {w}");
            }
            // 四元数与其相反数表示同一旋转，比对时取同号
            let sign = if got.rotation[3] * qf[3] < 0.0 {
                -1.0
            } else {
                1.0
            };
            for (i, (g, w)) in got.rotation.iter().zip(qf.iter()).enumerate() {
                assert!(
                    (g * sign - w).abs() < 1e-3,
                    "四元数分量 {i}: 得到 {} 期望 {w}",
                    g * sign
                );
            }
        }
    }

    #[test]
    fn bannerlord_row_vector_view_recovers_the_observed_world_position() {
        let view = [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 0.593_177_44, -0.805_071_7, 0.0],
            [0.0, 0.805_071_7, 0.593_177_44, 0.0],
            [-236.105_7, -298.176_2, 333.544_2, 1.0],
        ];

        let pose = pose_from_view(&view, TranslationSlot::Row3);
        let expected = [236.105_7, 445.398_4, 42.202_34];
        for (index, (got, want)) in pose.position.iter().zip(expected).enumerate() {
            assert!(
                (got - want).abs() < 1e-3,
                "Bannerlord position component {index}: got {got}, expected {want}"
            );
        }
    }

    #[test]
    fn delivery_euler_reproduces_the_samples_ninety_degree_offset() {
        // 不复制这个偏移，欧拉角与四元数会互相矛盾，
        // 而用四元数的下游一切正常——极难被发现。
        for (_, q, oula) in SAMPLE {
            let qf = [q[0] as f32, q[1] as f32, q[2] as f32, q[3] as f32];
            let got = quat_to_delivery_euler(qf);
            let want = [oula[0] as f32, oula[1] as f32, oula[2] as f32];
            for (i, (g, w)) in got.iter().zip(want.iter()).enumerate() {
                let diff = ((g - w + 180.0).rem_euclid(360.0) - 180.0).abs();
                assert!(diff < 0.02, "欧拉分量 {i}: 得到 {g} 期望 {w}（差 {diff}）");
            }
        }
    }

    #[test]
    fn orthonormality_is_the_discriminating_test() {
        // 真实旋转矩阵 vs 随机数据，中间隔着好几个数量级
        let (_, q, _) = SAMPLE[0];
        let qf = [q[0] as f32, q[1] as f32, q[2] as f32, q[3] as f32];
        let v = view_from_pose([1.0, 2.0, 3.0], qf);
        assert!(
            orthonormality_error(&v) < 1e-5,
            "真实矩阵实测 {}",
            orthonormality_error(&v)
        );

        let junk: Mat4 = [
            [0.3, 1.7, -0.2, 0.0],
            [2.1, 0.4, 0.9, 0.0],
            [-1.1, 0.6, 1.3, 0.0],
            [5.0, 6.0, 7.0, 1.0],
        ];
        assert!(orthonormality_error(&junk) > ORTHONORMAL_TOLERANCE);
        assert_eq!(classify(&junk), MatrixKind::Unknown);
    }

    #[test]
    fn projection_is_classified_before_view() {
        // 各向同性 FOV 的投影矩阵左上 3×3 可能碰巧接近正交；
        // 先判视图会把它误分类。顺序不是随意的。
        let p = perspective(90.0, 1.0, 0.1, 1000.0);
        assert_eq!(classify(&p), MatrixKind::Projection);
    }

    #[test]
    fn intrinsics_recover_a_known_fov() {
        let (w, h) = (1920.0f32, 1080.0f32);
        // 方形像素：hFOV=48° → f = 1/tan(24°)，且 m[1][1] = f * (w/h)
        let m = perspective(48.0, w / h, 0.1, 1000.0);
        let k = intrinsics_from_projection(&m, w, h);
        let want = w / 2.0 / (24.0f32.to_radians().tan());
        assert!((k.fx - want).abs() < 1.0, "fx={} 期望≈{want}", k.fx);
        // 方形像素下 fx 必须等于 fy —— 这正是需求方样本对不上的那条
        assert!(
            (k.fx - k.fy).abs() < 1.0,
            "方形像素下 fx({}) 应等于 fy({})",
            k.fx,
            k.fy
        );
        assert!((k.cx - 960.0).abs() < 0.5 && (k.cy - 540.0).abs() < 0.5);
    }

    #[test]
    fn square_pixels_are_verified_not_assumed() {
        // 我们按方形像素交付，但不能因此就假设算出来一定是方形。
        let (w, h) = (1920.0f32, 1080.0);
        let square = intrinsics_from_projection(&perspective(48.0, w / h, 0.1, 1000.0), w, h);
        assert!(square.is_square_pixels());
        assert!(square.check_square_pixels().is_ok());
        assert!((square.pixel_aspect_ratio() - 1.0).abs() < SQUARE_PIXEL_TOLERANCE);
    }

    #[test]
    fn the_requesters_sample_ratio_would_be_rejected() {
        // 需求方样本的 fx/fy = 16/9。若我方某天算出同样的比值，
        // 必须报错而不是照写 pixel_aspect_ratio = 1.0 —— 那会让
        // manifest 的声明与数据不符，而下游无从察觉。
        let bad = Intrinsics {
            fx: 2156.195,
            fy: 1212.86,
            cx: 960.0,
            cy: 540.0,
        };
        assert!((bad.pixel_aspect_ratio() - 16.0 / 9.0).abs() < 1e-4);
        assert!(!bad.is_square_pixels());
        let err = bad.check_square_pixels().unwrap_err();
        assert!(err.contains("不是方形像素"), "实际错误: {err}");
    }

    #[test]
    fn a_degenerate_fy_does_not_pass_as_square() {
        let zero_fy = Intrinsics {
            fx: 1000.0,
            fy: 0.0,
            cx: 0.0,
            cy: 0.0,
        };
        assert!(zero_fy.pixel_aspect_ratio().is_nan());
        assert!(!zero_fy.is_square_pixels());
        assert!(zero_fy.check_square_pixels().is_err());
    }

    #[test]
    fn scanning_finds_a_matrix_at_an_unaligned_offset() {
        // 矩阵在常量缓冲里的偏移由着色器布局决定，不保证 64 字节对齐。
        // 按 64 步进会漏掉大部分候选——这个用例把它钉住。
        let (_, q, _) = SAMPLE[0];
        let qf = [q[0] as f32, q[1] as f32, q[2] as f32, q[3] as f32];
        let v = view_from_pose([10.0, 20.0, 30.0], qf);

        let mut buf = vec![0u8; 12]; // 12 字节前缀 → 偏移 12，不是 64 的倍数
        for row in &v {
            for x in row {
                buf.extend_from_slice(&x.to_le_bytes());
            }
        }
        buf.extend_from_slice(&[0u8; 20]);

        let found = scan_candidates(&buf);
        assert!(
            found.iter().any(|candidate| candidate.offset == 12
                && matches!(candidate.kind, MatrixKind::View { .. })),
            "没在偏移 12 处找到视图矩阵，实得 {:?}",
            found
                .iter()
                .map(|candidate| (candidate.offset, candidate.kind, candidate.transpose))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn scanning_normalizes_a_column_major_projection_and_records_transpose() {
        let projection = perspective(48.0, 16.0 / 9.0, 0.1, 1000.0);
        let stored = std::array::from_fn::<_, 4, _>(|row| {
            std::array::from_fn(|column| projection[column][row])
        });
        assert_eq!(classify(&stored), MatrixKind::Unknown);

        let mut bytes = Vec::new();
        for row in stored {
            for value in row {
                bytes.extend_from_slice(&value.to_le_bytes());
            }
        }

        let found = scan_candidates(&bytes);
        let candidate = found
            .iter()
            .find(|candidate| candidate.offset == 0 && candidate.kind == MatrixKind::Projection)
            .expect("transposed projection must be discovered");
        assert!(candidate.transpose);
        assert_eq!(candidate.matrix, projection);
    }

    #[test]
    fn scanning_rejects_nan_and_absurd_magnitudes() {
        let mut buf = Vec::new();
        for _ in 0..16 {
            buf.extend_from_slice(&f32::NAN.to_le_bytes());
        }
        assert!(scan_candidates(&buf).is_empty());

        let mut big = Vec::new();
        for _ in 0..16 {
            big.extend_from_slice(&1.0e30f32.to_le_bytes());
        }
        assert!(scan_candidates(&big).is_empty());
    }

    #[test]
    fn scanning_rejects_affine_48_byte_candidates_without_replayable_encoding() {
        for values in [
            [
                0.0_f32, -1.0, 0.0, 12.5, 1.0, 0.0, 0.0, -3.25, 0.0, 0.0, 1.0, 8.0,
            ],
            [
                0.0_f32, -1.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 12.5, -3.25, 8.0,
            ],
        ] {
            let mut bytes = values
                .into_iter()
                .flat_map(f32::to_le_bytes)
                .collect::<Vec<_>>();
            bytes.resize(64, 0);
            assert!(scan_candidates(&bytes).is_empty());
        }
    }

    #[test]
    fn scanning_recovers_view_from_view_projection_and_projection_pair() {
        let view = view_from_pose(
            [12.0, -7.0, 3.5],
            [0.0, 0.0, (0.25_f32).sin(), (0.25_f32).cos()],
        );
        let projection = perspective(75.0, 16.0 / 9.0, 0.02, 10_000.0);
        let view_projection = multiply4(&view, &projection);
        let mut bytes = vec![0_u8; 256];
        for (base, matrix) in [(0, view_projection), (128, projection)] {
            for (index, value) in matrix.iter().flatten().enumerate() {
                bytes[base + index * 4..base + index * 4 + 4].copy_from_slice(&value.to_le_bytes());
            }
        }
        let recovered = scan_view_projection_pairs(&bytes);
        assert!(recovered.iter().any(|candidate| {
            candidate.offset == 0
                && matches!(candidate.kind, MatrixKind::View { .. })
                && candidate
                    .matrix
                    .iter()
                    .flatten()
                    .zip(view.iter().flatten())
                    .all(|(actual, expected)| (actual - expected).abs() < 2.0e-3)
        }));
    }

    #[test]
    fn scanning_does_not_invent_view_from_projection_alone() {
        let projection = perspective(75.0, 16.0 / 9.0, 0.02, 10_000.0);
        let mut bytes = vec![0_u8; 128];
        for (index, value) in projection.iter().flatten().enumerate() {
            bytes[index * 4..index * 4 + 4].copy_from_slice(&value.to_le_bytes());
        }
        assert!(scan_view_projection_pairs(&bytes).is_empty());
    }

    #[test]
    fn correlation_separates_signal_from_noise() {
        // P4-3 的判据：旋转增量 ↔ 鼠标 delta
        let mouse: Vec<f32> = (0..64).map(|i| ((i * 37) % 21) as f32 - 10.0).collect();
        let rotated: Vec<f32> = mouse.iter().map(|m| m * 0.013).collect();
        let c = correlation(&mouse, &rotated).unwrap();
        assert!(c > MIN_MOUSE_CORRELATION, "相关但只得 {c}");

        let unrelated: Vec<f32> = (0..64).map(|i| ((i * 11) % 7) as f32).collect();
        let c2 = correlation(&mouse, &unrelated).unwrap().abs();
        assert!(c2 < MIN_MOUSE_CORRELATION, "不相关却得 {c2}");
    }

    #[test]
    fn correlation_refuses_degenerate_input() {
        assert!(correlation(&[1.0], &[1.0]).is_none());
        assert!(correlation(&[1.0, 2.0], &[3.0]).is_none());
        // 一侧完全不动 → 相关性无定义，不该返回 0 让调用方误判为「不相关」
        assert!(correlation(&[1.0, 2.0, 3.0], &[5.0, 5.0, 5.0]).is_none());
    }

    #[test]
    fn column_major_translation_is_detected() {
        let (_, q, _) = SAMPLE[2];
        let qf = [q[0] as f32, q[1] as f32, q[2] as f32, q[3] as f32];
        let p = [7.0f32, -3.0, 11.0];
        let row = view_from_pose(p, qf);
        // 转成列向量约定：把平移搬到第 3 列
        let mut col = row;
        col[3] = [0.0, 0.0, 0.0, 1.0];
        col[0][3] = row[3][0];
        col[1][3] = row[3][1];
        col[2][3] = row[3][2];
        // 左上 3×3 也要转置才是同一个变换
        for i in 0..3 {
            for j in 0..3 {
                col[i][j] = row[j][i];
            }
        }
        match classify(&col) {
            MatrixKind::View { translation } => {
                assert_eq!(translation, TranslationSlot::Col3);
            }
            other => panic!("应判为 View/Col3，实得 {other:?}"),
        }
    }
}

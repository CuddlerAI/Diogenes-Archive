//! `metric_scale`（游戏单位 → 米）的标定与来源记账
//! （[77 号 §5.5.2](../../../docs/77-depth-ground-truth-hook-experiment-plan.md)，路①②）。
//!
//! # 为什么这个量要单开一个模块：它不在渲染管线里
//!
//! 深度缓冲值与 view-space 深度的关系是 `d = A + B/z`，其中 `z` 的单位是**游戏单位**。
//! 投影矩阵（§5.11 / P4）给出的 `A`、`B` 同样以游戏单位计——**渲染管线自始至终不知道米**。
//! 所以 P4 关闭的是位姿，**不是** `metric_scale`：后者必须由一个**管线之外的米制锚点**给出。
//! 77 号 §8.2 初稿把它归进「P4 可关闭的位姿字段」，v6.1 已修正。
//!
//! # 可辨识性：能解出什么、解不出什么
//!
//! 交付需要的是米制深度 `L = metric_scale · z`。代入上式并令 `u = 1/L`（米制视差）：
//!
//! ```text
//! d = A + (B · metric_scale) / L = A + B_m · u        B_m ≜ B · metric_scale
//! ```
//!
//! 即 `(u, d)` 平面上的一条直线。由「缓冲值 ↔ 已知米制距离」的观测能解出的是
//! **`A` 与 `B_m`**，而 **`B` 与 `metric_scale` 永远无法分离**——只有乘积可辨识。
//! 这不是本实现的缺陷，是问题本身的结构，[`b_and_metric_scale_are_not_separately_identifiable`]
//! 把它钉成测试。
//!
//! 两个后果：
//!
//! 1. **交付层不受影响**：米制深度只用 `L = B_m/(d − A)`，复合量就够
//!    （[`AffineFit::projection`] 直接给出可用的 [`Projection`]）。
//! 2. **但 68 号 §6.3 要求 `metric_scale` 是独立字段**，要单独报这个数，
//!    必须另有 `B` 的来源：引擎默认 near/far，或 P4 的投影矩阵
//!    （[`metric_scale_from_fit`]）。
//!
//! # 纪律：没有来源的比例值不许进交付
//!
//! [`MetricScaleCalibration`] 强制携带来源与证据路径，路①（引擎惯例常数）单独使用时
//! 还必须有一次锚点交叉验证——与 `profile::DsvSelection`「没有证据不许上线」同一条纪律。
//! 校验在 `profile` 侧执行。

use serde::{Deserialize, Serialize};

use crate::depth::Projection;

/// 引擎惯例的「1 游戏单位 = 多少米」。
///
/// **这些是惯例值，不是测量值。** 单个游戏完全可能改掉世界缩放，
/// 因此路①永远需要一次锚点交叉验证才能进交付（见 [`MetricScaleCalibration::validate`]）。
/// UE3 的 2 cm/uu 即 §5.5.2 表格里那条，也是 BioShock Infinite（P1 首个目标）的适用值。
///
/// 逐条出处与置信度见 [82 号逐款调研](../../../docs/82-depth-map-bounds-public-data-survey.md) §2
/// 与 `config/depth-map-bounds/public-survey.json`。
///
/// **公开来源互相矛盾的引擎刻意不在表内**，它们列在 [`DISPUTED_ENGINE_SCALES`]：
/// Creation Engine（Fallout 4 / Skyrim SE）差 12.5%，Source 系差 33%。
/// 98 号 §7 建议「Source 必须标不确定度而不是给一个值」，本表按该建议移除了 `source`。
pub const ENGINE_CONVENTIONS: &[(&str, f64)] = &[
    ("unity", 1.0),
    ("ue3", 0.02),
    ("ue4", 0.01),
    ("ue5", 0.01),
    // 82 号新增三条：
    ("prism3d", 1.0),       // ETS2 / ATS，坐标系即米
    ("cryengine", 1.0),     // KCD2，同上
    ("blam", 3.048),        // Halo MCC，1 wu = 100 JMS units ≈ 10 ft
    ("slipspace", 0.304_8), // Halo Infinite（Forge 设计集），1 wu = 1 ft——与 blam 差 10 倍，勿互推
];

/// **公开来源互相矛盾，因而不允许作为路①惯例值使用**的引擎。
///
/// 元组是 `(引擎名, 相对分歧幅度, 分歧出处)`。这张表存在的意义是把
/// 「这个引擎没有惯例值」与「我们还没查」区分开——前者是**已查明的结论**，
/// 后者才是待办。[`engine_convention`] 对这些引擎返回 `None`，
/// 于是它们只能走路②（锚点实测）进交付。
///
/// 分歧幅度全部超过 [`DEFAULT_CROSS_CHECK_TOLERANCE`]，这不是巧合而是入表判据：
/// 分歧小于容差时，取哪个值都能通过交叉验证，那才可以先用着；
/// 分歧大于容差时，**给出任何单值都是在替读者做一个没有依据的选择**——
/// 更糟的是交叉验证还会「通过」，因为总有一个分支离实测值更近。
///
/// 逐条依据见 [98 号 §4.2 陷阱 1](../../../docs/98-steam-wide-map-metric-actioncamera-source-survey.md)。
pub const DISPUTED_ENGINE_SCALES: &[(&str, f64, &str)] = &[
    (
        "source",
        0.33,
        "Valve 官方《Unit scales》同页给出 1 grid = 0.75 in（1.905 cm）与 1 in = 2.54 cm 两个值；\
         官方《Unit》页自述 \"not set in stone or precise\"",
    ),
    (
        "creation",
        0.125,
        "Fallout 4 的两个公开来源相差 12.5%，见 fallout4_public_scale_sources_disagree_beyond_tolerance",
    ),
    (
        "creation_skyrim_se",
        0.125,
        "Skyrim SE 与 Fallout 4 是同一套 CK 字段与同一个 0.0142875，因而**继承同一处分歧**——\
         CK wiki 的 FT = Unit × 0.046875 是独立佐证，但它佐证的是那个有争议的值本身",
    ),
];

/// 该引擎的公开来源是否已知互相矛盾。返回 `(相对分歧幅度, 出处)`。
#[must_use]
pub fn disputed_engine_scale(engine: &str) -> Option<(f64, &'static str)> {
    let key = engine.to_ascii_lowercase();
    DISPUTED_ENGINE_SCALES
        .iter()
        .find(|(name, _, _)| *name == key)
        .map(|(_, spread, source)| (*spread, *source))
}

/// 路①②交叉验证的默认容差（相对偏差）。
///
/// 🟨 **暂定值，待 P1d 实测修正**：路①是惯例常数，路②的锚点法受像素跨度测量精度限制，
/// 两者对上 10% 以内即认为互相印证；超出说明其中一条有系统性问题，必须查清而不是取平均。
pub const DEFAULT_CROSS_CHECK_TOLERANCE: f64 = 0.10;

/// 按引擎名查惯例值（大小写不敏感）。
#[must_use]
pub fn engine_convention(engine: &str) -> Option<f64> {
    let key = engine.to_ascii_lowercase();
    ENGINE_CONVENTIONS
        .iter()
        .find(|(name, _)| *name == key)
        .map(|(_, v)| *v)
}

/// 一个米制锚点：某像素处的深度缓冲原始值，配上该处**已知的**真实米制距离。
///
/// 「已知」只有两种正当来源，别的都是在编数字：
///
/// - [`MetricAnchor::from_known_size`]：画面中某物体的真实尺寸已知（门高、车长、人体高），
///   用小孔成像由像素跨度反算距离——**推荐路径**，不需要预先知道任何米制距离；
/// - [`MetricAnchor::direct`]：距离由游戏自身以真实单位显示（如模拟类游戏的里程/车速读数）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricAnchor {
    /// 人可读标签，进证据文件（如 `"车库卷帘门-高3.0m"`）。
    pub label: String,
    /// 该像素的深度缓冲原始值 `d`。
    pub buffer_value: f64,
    /// 该处的真实距离（米）。
    pub metres: f64,
}

impl MetricAnchor {
    /// 直接给定距离的锚点。
    #[must_use]
    pub fn direct(label: impl Into<String>, buffer_value: f64, metres: f64) -> Self {
        Self {
            label: label.into(),
            buffer_value,
            metres,
        }
    }

    /// 由已知真实尺寸的物体反算距离：`L = focal_px · real_size_m / pixel_span`。
    ///
    /// `focal_px` 与交付物 `camera_intrinsics.fx` 同源（68 号 §6.3），
    /// 由 FOV 与横向分辨率得出，或 P4 起直接取投影矩阵。
    ///
    /// 返回 `None` 表示入参退化（非有限、非正），调用方应丢弃该锚点而不是兜底一个值。
    #[must_use]
    pub fn from_known_size(
        label: impl Into<String>,
        buffer_value: f64,
        focal_px: f64,
        real_size_m: f64,
        pixel_span: f64,
    ) -> Option<Self> {
        let usable = buffer_value.is_finite()
            && focal_px.is_finite()
            && focal_px > 0.0
            && real_size_m.is_finite()
            && real_size_m > 0.0
            && pixel_span.is_finite()
            && pixel_span > 0.0;
        if !usable {
            return None;
        }
        let metres = focal_px * real_size_m / pixel_span;
        if !metres.is_finite() || metres <= 0.0 {
            return None;
        }
        Some(Self::direct(label, buffer_value, metres))
    }

    fn is_usable(&self) -> bool {
        self.buffer_value.is_finite() && self.metres.is_finite() && self.metres > 0.0
    }
}

/// 标定失败的原因。每一条都对应一种「继续算下去就会得到看似合理的错数」的情形。
#[derive(Debug, Clone, PartialEq)]
pub enum CalibrationError {
    /// 可用锚点不足。仿射拟合要 ≥2，且必须落在不同距离上。
    NotEnoughAnchors { usable: usize, required: usize },
    /// 全部锚点距离几乎相同——直线只有一个点，斜率不可解。
    DegenerateAnchorSpread,
    /// `B`（游戏单位下的 `b` 系数）为零或非有限，无法从 `B_m` 反解比例。
    UnusableProjectionB { b: f64 },
    /// 解出的比例非正——多半是锚点的 `d` 落在 `A` 的错误一侧。
    NonPositiveScale { scale: f64 },
}

impl core::fmt::Display for CalibrationError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotEnoughAnchors { usable, required } => {
                write!(f, "可用锚点 {usable} 个，至少需要 {required} 个")
            }
            Self::DegenerateAnchorSpread => {
                write!(f, "锚点距离几乎相同，无法定斜率——需要覆盖不同距离的观测")
            }
            Self::UnusableProjectionB { b } => write!(f, "投影系数 b={b} 不可用于反解比例"),
            Self::NonPositiveScale { scale } => write!(f, "解出的 metric_scale={scale} 非正"),
        }
    }
}

impl std::error::Error for CalibrationError {}

/// 仿射拟合结果：`d = a + b_metric · (1/L)`。
///
/// `b_metric` 是复合量 `B · metric_scale`，**不是** `B` 本身。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct AffineFit {
    pub a: f64,
    pub b_metric: f64,
    /// 拟合残差 RMS（在缓冲值空间）。锚点互相矛盾时这个数会显著变大。
    pub residual_rms: f64,
    pub n: usize,
}

impl AffineFit {
    /// 直接给出可用于交付的 [`Projection`]。
    ///
    /// 这里 `metric_scale` 填 `1.0` **不是偷懒**：`b_metric` 已经含了比例，
    /// `linearize` 算的 `b/(d−a)·1.0` 就是米制深度。可辨识性决定了这是唯一诚实的填法。
    #[must_use]
    pub fn projection(&self) -> Projection {
        Projection {
            a: self.a,
            b: self.b_metric,
            metric_scale: 1.0,
        }
    }
}

/// 路②：由锚点最小二乘拟合 `(A, B_m)`。**不需要预先知道 `(A,B)`。**
///
/// 至少 2 个锚点，且必须落在不同距离上（否则 [`CalibrationError::DegenerateAnchorSpread`]）。
///
/// # Errors
///
/// 锚点不足或距离不可分辨时返回 [`CalibrationError`]。
pub fn fit_affine(anchors: &[MetricAnchor]) -> Result<AffineFit, CalibrationError> {
    let pts: Vec<(f64, f64)> = anchors
        .iter()
        .filter(|a| a.is_usable())
        .map(|a| (1.0 / a.metres, a.buffer_value))
        .collect();
    let n = pts.len();
    if n < 2 {
        return Err(CalibrationError::NotEnoughAnchors {
            usable: n,
            required: 2,
        });
    }
    let nf = n as f64;
    let sum_u: f64 = pts.iter().map(|(u, _)| *u).sum();
    let sum_d: f64 = pts.iter().map(|(_, d)| *d).sum();
    let sum_uu: f64 = pts.iter().map(|(u, _)| u * u).sum();
    let sum_ud: f64 = pts.iter().map(|(u, d)| u * d).sum();

    let denom = nf * sum_uu - sum_u * sum_u;
    // 相对判据：绝对阈值会随距离量级误判（毫米级场景与公里级场景的 u 差好几个数量级）。
    if denom.abs() <= f64::EPSILON * nf * sum_uu.max(1.0) {
        return Err(CalibrationError::DegenerateAnchorSpread);
    }
    let b_metric = (nf * sum_ud - sum_u * sum_d) / denom;
    let a = (sum_d - b_metric * sum_u) / nf;
    let residual_rms = (pts
        .iter()
        .map(|(u, d)| (a + b_metric * u - d).powi(2))
        .sum::<f64>()
        / nf)
        .sqrt();

    Ok(AffineFit {
        a,
        b_metric,
        residual_rms,
        n,
    })
}

/// 由拟合的 `B_m` 与**独立来源的** `B` 反解 `metric_scale = B_m / B`。
///
/// `b_game` 只有两个正当来源：引擎默认 near/far 推出的 `B`（路①），
/// 或 P4 从投影矩阵取到的 `B`（路③）。
///
/// # Errors
///
/// `b_game` 不可用或解出的比例非正时返回 [`CalibrationError`]。
pub fn metric_scale_from_fit(fit: &AffineFit, b_game: f64) -> Result<f64, CalibrationError> {
    if !b_game.is_finite() || b_game == 0.0 {
        return Err(CalibrationError::UnusableProjectionB { b: b_game });
    }
    let scale = fit.b_metric / b_game;
    if !scale.is_finite() || scale <= 0.0 {
        return Err(CalibrationError::NonPositiveScale { scale });
    }
    Ok(scale)
}

/// 逐锚点的比例估计（`(A,B)` 已知时的直接解法）。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ScaleEstimate {
    pub value: f64,
    pub n: usize,
    /// 逐锚点估计的相对离散度 `(max − min)/mean`。锚点互相矛盾时这个数会很大。
    pub spread: f64,
}

/// `(A,B)` 已知（路①或路③）时，直接由每个锚点解一个比例并汇总。
///
/// 逐锚点 `sᵢ = Lᵢ·(dᵢ − A)/B`。汇总取均值，同时报离散度——
/// **离散度是这条路唯一的自检信号**，不报出来就等于把互相矛盾的锚点平均成一个漂亮数字。
///
/// # Errors
///
/// 无可用锚点、`b_game` 不可用或均值非正时返回 [`CalibrationError`]。
pub fn metric_scale_from_anchors(
    projection_a: f64,
    b_game: f64,
    anchors: &[MetricAnchor],
) -> Result<ScaleEstimate, CalibrationError> {
    if !b_game.is_finite() || b_game == 0.0 {
        return Err(CalibrationError::UnusableProjectionB { b: b_game });
    }
    let per: Vec<f64> = anchors
        .iter()
        .filter(|a| a.is_usable())
        .map(|a| a.metres * (a.buffer_value - projection_a) / b_game)
        .filter(|s| s.is_finite())
        .collect();
    if per.is_empty() {
        return Err(CalibrationError::NotEnoughAnchors {
            usable: 0,
            required: 1,
        });
    }
    let n = per.len();
    let mean = per.iter().sum::<f64>() / n as f64;
    if !mean.is_finite() || mean <= 0.0 {
        return Err(CalibrationError::NonPositiveScale { scale: mean });
    }
    let lo = per.iter().copied().fold(f64::INFINITY, f64::min);
    let hi = per.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    Ok(ScaleEstimate {
        value: mean,
        n,
        spread: (hi - lo) / mean,
    })
}

/// 路①（惯例常数）与路②（锚点实测）的交叉验证结果。§5.5.2 要求 P1d 做这一步。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CrossCheck {
    /// 路①的惯例值。
    pub convention: f64,
    /// 路②实测值。
    pub measured: f64,
    /// `measured / convention`。
    pub ratio: f64,
    pub tolerance: f64,
}

impl CrossCheck {
    #[must_use]
    pub fn new(convention: f64, measured: f64, tolerance: f64) -> Self {
        Self {
            convention,
            measured,
            ratio: measured / convention,
            tolerance,
        }
    }

    /// 两条路是否互相印证。**不一致时不许取平均**——必须查清哪条错了。
    #[must_use]
    pub fn agrees(&self) -> bool {
        let ratio = self.measured / self.convention;
        self.convention.is_finite()
            && self.convention > 0.0
            && self.measured.is_finite()
            && self.measured > 0.0
            && self.tolerance.is_finite()
            && self.tolerance >= 0.0
            && ratio.is_finite()
            && (ratio - 1.0).abs() <= self.tolerance
    }
}

/// `metric_scale` 的来源。交付里出现的每一个比例值都必须能指回其中一条。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "path")]
pub enum MetricScaleSource {
    /// 路①：引擎惯例常数。零成本，但是**假设**——单独使用必须有交叉验证。
    EngineConvention { engine: String },
    /// 路②：游戏内已知真实尺寸锚点标定。
    KnownSizeAnchors { anchors: usize, residual_rms: f64 },
    /// 路③+②：`(A,B)` 取自投影矩阵，再与锚点合解出比例。
    ///
    /// 注意路③**单独给不出**比例（投影矩阵的 near/far 也是游戏单位），所以这一支同样带锚点数。
    ProjectionMatrixWithAnchors { anchors: usize, spread: f64 },
    /// 发行关卡的 authored metadata 直接声明 metres-per-unit，且字段语义由
    /// 引擎文档确认。它是逐游戏、逐 build 的证据，不是可被游戏改写的通用惯例猜测。
    AuthoredLevelMetadata {
        levels: usize,
        unit_size_metres: f64,
    },
}

/// 带来源与证据的 `metric_scale`。**profile 里不再允许出现裸 `f64`。**
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricScaleCalibration {
    /// 游戏单位 → 米。
    pub value: f64,
    pub source: MetricScaleSource,
    /// 路①②交叉验证结果。`None` = 未做。
    pub cross_check: Option<CrossCheck>,
    /// 证据文件（锚点观测表 / 标定截图）相对路径。**空即拒绝上线。**
    pub evidence_path: String,
    /// 证据文件 exact bytes 的 `sha256:<64 lowercase hex>` 钉扣。
    ///
    /// v1 profile 没有这个字段，因此保持可选；v2 是否必须携带由
    /// [`crate::profile::DepthProfile::validate`] 按 schema fail-closed 判断。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_sha256: Option<String>,
}

/// [`MetricScaleCalibration`] 的校验失败原因。
#[derive(Debug, Clone, PartialEq)]
pub enum MetricScaleError {
    /// 比例必须为正——非正会让米制深度符号错乱。
    NonPositive { value: f64 },
    /// 没有证据文件。与 DSV 选择同一条纪律。
    MissingEvidence,
    /// 只有路①的惯例常数、没有任何实测交叉验证。惯例可能被单个游戏改掉。
    ConventionNotCrossChecked { engine: String },
    /// 交叉验证做了但没过。**此时不许取平均、不许择一，必须查清。**
    CrossCheckFailed { ratio: f64, tolerance: f64 },
    /// 最终采用值与交叉验证中该来源对应的值不一致。否则可伪造一个能通过的 ratio，
    /// 再把任意值塞进 profile。
    CrossCheckValueMismatch { value: f64, expected: f64 },
    /// 发行关卡证据为空、非正或与最终采用值不一致。
    InvalidAuthoredLevelMetadata {
        levels: usize,
        unit_size_metres: f64,
    },
}

impl core::fmt::Display for MetricScaleError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NonPositive { value } => write!(f, "metric_scale={value} 必须为正"),
            Self::MissingEvidence => write!(f, "metric_scale 缺证据文件路径，拒绝上线"),
            Self::ConventionNotCrossChecked { engine } => write!(
                f,
                "metric_scale 仅取自 {engine} 引擎惯例值且未做锚点交叉验证（§5.5.2 要求 P1d 交叉）"
            ),
            Self::CrossCheckFailed { ratio, tolerance } => write!(
                f,
                "metric_scale 路①②交叉验证不通过：比值 {ratio:.4}，容差 ±{tolerance:.4}"
            ),
            Self::CrossCheckValueMismatch { value, expected } => write!(
                f,
                "metric_scale={value} 与交叉验证对应来源的值 {expected} 不一致"
            ),
            Self::InvalidAuthoredLevelMetadata {
                levels,
                unit_size_metres,
            } => write!(
                f,
                "发行关卡 metric metadata 无效：levels={levels}, unit_size_metres={unit_size_metres}"
            ),
        }
    }
}

impl std::error::Error for MetricScaleError {}

impl MetricScaleCalibration {
    /// 路②：由锚点拟合 + 独立来源的 `B` 直接构造。
    ///
    /// # Errors
    ///
    /// 拟合失败或反解失败时返回 [`CalibrationError`]。
    pub fn from_anchor_fit(
        anchors: &[MetricAnchor],
        b_game: f64,
        evidence_path: impl Into<String>,
    ) -> Result<Self, CalibrationError> {
        let fit = fit_affine(anchors)?;
        let value = metric_scale_from_fit(&fit, b_game)?;
        Ok(Self {
            value,
            source: MetricScaleSource::KnownSizeAnchors {
                anchors: fit.n,
                residual_rms: fit.residual_rms,
            },
            cross_check: None,
            evidence_path: evidence_path.into(),
            evidence_sha256: None,
        })
    }

    /// 路①：取引擎惯例常数。**必须再调 [`Self::with_cross_check`] 才能过校验。**
    #[must_use]
    pub fn from_engine_convention(
        engine: &str,
        value: f64,
        evidence_path: impl Into<String>,
    ) -> Self {
        Self {
            value,
            source: MetricScaleSource::EngineConvention {
                engine: engine.to_string(),
            },
            cross_check: None,
            evidence_path: evidence_path.into(),
            evidence_sha256: None,
        }
    }

    /// 由发行关卡内的 authored metres-per-unit 元数据直接构造。
    #[must_use]
    pub fn from_authored_level_metadata(
        levels: usize,
        unit_size_metres: f64,
        evidence_path: impl Into<String>,
    ) -> Self {
        Self {
            value: unit_size_metres,
            source: MetricScaleSource::AuthoredLevelMetadata {
                levels,
                unit_size_metres,
            },
            cross_check: None,
            evidence_path: evidence_path.into(),
            evidence_sha256: None,
        }
    }

    /// 附上与另一条路的交叉验证结果。
    #[must_use]
    pub fn with_cross_check(mut self, cross: CrossCheck) -> Self {
        self.cross_check = Some(cross);
        self
    }

    /// 钉住本次标定实际采用的证据文件 exact bytes。
    #[must_use]
    pub fn with_evidence_sha256(mut self, evidence_sha256: impl Into<String>) -> Self {
        self.evidence_sha256 = Some(evidence_sha256.into());
        self
    }

    /// 交付前校验。**宁可拒绝启动，不可静默跑偏。**
    ///
    /// # Errors
    ///
    /// 逐条见 [`MetricScaleError`]。
    pub fn validate(&self) -> Result<(), MetricScaleError> {
        if !self.value.is_finite() || self.value <= 0.0 {
            return Err(MetricScaleError::NonPositive { value: self.value });
        }
        if self.evidence_path.trim().is_empty() {
            return Err(MetricScaleError::MissingEvidence);
        }
        if let MetricScaleSource::AuthoredLevelMetadata {
            levels,
            unit_size_metres,
        } = &self.source
        {
            let relative_error = (self.value - *unit_size_metres).abs() / self.value.abs();
            if *levels == 0
                || !unit_size_metres.is_finite()
                || *unit_size_metres <= 0.0
                || !relative_error.is_finite()
                || relative_error > f64::EPSILON * 8.0
            {
                return Err(MetricScaleError::InvalidAuthoredLevelMetadata {
                    levels: *levels,
                    unit_size_metres: *unit_size_metres,
                });
            }
        }
        if let Some(cross) = self.cross_check {
            if !cross.agrees() {
                return Err(MetricScaleError::CrossCheckFailed {
                    ratio: cross.measured / cross.convention,
                    tolerance: cross.tolerance,
                });
            }
            let expected = match &self.source {
                MetricScaleSource::EngineConvention { .. } => cross.convention,
                MetricScaleSource::KnownSizeAnchors { .. }
                | MetricScaleSource::ProjectionMatrixWithAnchors { .. }
                | MetricScaleSource::AuthoredLevelMetadata { .. } => cross.measured,
            };
            let relative_error = (self.value - expected).abs() / expected.abs();
            if !relative_error.is_finite() || relative_error > f64::EPSILON * 8.0 {
                return Err(MetricScaleError::CrossCheckValueMismatch {
                    value: self.value,
                    expected,
                });
            }
        } else if let MetricScaleSource::EngineConvention { engine } = &self.source {
            return Err(MetricScaleError::ConventionNotCrossChecked {
                engine: engine.clone(),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一组锚点：给定真实的 (A, B, s)，按 `d = A + B·s/L` 反推缓冲值。
    fn planted(a: f64, b: f64, s: f64, dists: &[f64]) -> Vec<MetricAnchor> {
        dists
            .iter()
            .enumerate()
            .map(|(i, &l)| MetricAnchor::direct(format!("anchor-{i}"), a + b * s / l, l))
            .collect()
    }

    #[test]
    fn affine_fit_recovers_planted_constants() {
        let (a, b, s) = (0.0, 10.0, 0.02);
        let fit = fit_affine(&planted(a, b, s, &[2.0, 5.0, 12.0, 40.0])).unwrap();
        assert!((fit.a - a).abs() < 1e-9, "a={}", fit.a);
        assert!((fit.b_metric - b * s).abs() < 1e-9, "b_m={}", fit.b_metric);
        assert!(fit.residual_rms < 1e-9);
        assert_eq!(fit.n, 4);
    }

    /// 本模块最重要的一条：`B` 与 `metric_scale` 只有乘积可辨识。
    ///
    /// 两组完全不同的 (B, s)、乘积相同 —— 观测一模一样，拟合结果一模一样，
    /// 交付的米制深度也一模一样。任何声称「从深度观测解出 metric_scale」的做法都必然是
    /// 偷偷引入了 B 的假设。
    #[test]
    fn b_and_metric_scale_are_not_separately_identifiable() {
        // UE3 惯例：B=10 游戏单位、1uu=2cm；另一组：B=200、1uu=0.1cm。乘积同为 0.2。
        let left = planted(0.3, 10.0, 0.02, &[3.0, 9.0, 25.0]);
        let right = planted(0.3, 200.0, 0.001, &[3.0, 9.0, 25.0]);
        for (l, r) in left.iter().zip(&right) {
            assert!((l.buffer_value - r.buffer_value).abs() < 1e-12);
        }

        let fl = fit_affine(&left).unwrap();
        let fr = fit_affine(&right).unwrap();
        assert!((fl.b_metric - fr.b_metric).abs() < 1e-9);

        // 交付层等价：同一个 d 线性化出同一个米数。
        let d = 0.3 + 10.0 * 0.02 / 7.0;
        let zl = fl.projection().linearize(d).unwrap();
        let zr = fr.projection().linearize(d).unwrap();
        assert!((zl - 7.0).abs() < 1e-9 && (zr - 7.0).abs() < 1e-9);

        // 只有引入独立的 B，两者才分得开——而 B 是假设，不是观测。
        assert!((metric_scale_from_fit(&fl, 10.0).unwrap() - 0.02).abs() < 1e-12);
        assert!((metric_scale_from_fit(&fl, 200.0).unwrap() - 0.001).abs() < 1e-12);
    }

    #[test]
    fn fit_needs_two_distinct_distances() {
        let same = planted(0.0, 10.0, 0.02, &[6.0, 6.0, 6.0]);
        assert_eq!(
            fit_affine(&same).unwrap_err(),
            CalibrationError::DegenerateAnchorSpread
        );
        let one = planted(0.0, 10.0, 0.02, &[6.0]);
        assert_eq!(
            fit_affine(&one).unwrap_err(),
            CalibrationError::NotEnoughAnchors {
                usable: 1,
                required: 2
            }
        );
    }

    #[test]
    fn known_size_anchor_matches_pinhole_geometry() {
        // fx=960px（1920 宽、90° 水平 FOV）、3 m 高的门占 144 px → 20 m。
        let a = MetricAnchor::from_known_size("门-3m", 0.5, 960.0, 3.0, 144.0).unwrap();
        assert!((a.metres - 20.0).abs() < 1e-9, "{}", a.metres);
        // 退化入参必须丢弃而不是兜底。
        assert!(MetricAnchor::from_known_size("x", 0.5, 960.0, 3.0, 0.0).is_none());
        assert!(MetricAnchor::from_known_size("x", 0.5, 0.0, 3.0, 144.0).is_none());
        assert!(MetricAnchor::from_known_size("x", f64::NAN, 960.0, 3.0, 144.0).is_none());
    }

    #[test]
    fn scale_from_known_projection_agrees_with_planted_scale() {
        let (a, b, s) = (0.0, 10.0, 0.02);
        let est = metric_scale_from_anchors(a, b, &planted(a, b, s, &[2.0, 8.0, 30.0])).unwrap();
        assert!((est.value - s).abs() < 1e-12);
        assert_eq!(est.n, 3);
        assert!(
            est.spread < 1e-9,
            "一致锚点的离散度应接近 0，实为 {}",
            est.spread
        );
    }

    #[test]
    fn contradictory_anchors_show_up_as_spread_not_a_pretty_average() {
        let mut anchors = planted(0.0, 10.0, 0.02, &[2.0, 8.0]);
        // 第三个锚点的距离量错了一倍（把 30 m 的物体当成 15 m）。
        anchors.push(MetricAnchor::direct(
            "量错的",
            0.0 + 10.0 * 0.02 / 30.0,
            15.0,
        ));
        let est = metric_scale_from_anchors(0.0, 10.0, &anchors).unwrap();
        assert!(
            est.spread > 0.3,
            "矛盾锚点必须体现在离散度上，实为 {}",
            est.spread
        );
    }

    #[test]
    fn ue3_convention_is_two_centimetres_per_unit() {
        // §5.5.2 路①引用的就是这个数，BioShock Infinite（P1 首个目标）适用。
        assert_eq!(engine_convention("ue3"), Some(0.02));
        assert_eq!(engine_convention("UE3"), Some(0.02));
        assert_eq!(engine_convention("unity"), Some(1.0));
        assert_eq!(engine_convention("没这个引擎"), None);
    }

    /// 82 号调研新增的三条，各自的出处见该文 §2 与 §8。
    ///
    /// 两代 Halo 的世界单位定义**差 10 倍**（Blam 1 wu ≈ 10 ft，Infinite Forge 1 wu = 1 ft），
    /// 这一条单独断言，是因为「同一个系列所以单位相同」是最容易犯的想当然。
    #[test]
    fn survey_added_conventions_including_the_ten_fold_halo_split() {
        assert_eq!(engine_convention("prism3d"), Some(1.0));
        assert_eq!(engine_convention("cryengine"), Some(1.0));
        let blam = engine_convention("blam").unwrap();
        let slipspace = engine_convention("slipspace").unwrap();
        assert!((blam - 3.048).abs() < 1e-9);
        assert!((slipspace - 0.3048).abs() < 1e-9);
        assert!(
            (blam / slipspace - 10.0).abs() < 1e-9,
            "两代 Halo 的 wu 相差正好 10 倍，实为 {}",
            blam / slipspace
        );
    }

    /// 82 号 §2.3：Fallout 4 的两个公开比例来源互相矛盾，幅度刚好越过交叉验证容差。
    ///
    /// 这是本仓第一次拿交叉验证规则去筛真实公开数据，**它筛出了东西**——
    /// 所以 Creation Engine 不进 [`ENGINE_CONVENTIONS`]，等路②锚点裁掉一个再说。
    #[test]
    fn fallout4_public_scale_sources_disagree_beyond_tolerance() {
        // ① CK wiki：外部 cell = 4096 units = 58.5 m
        let from_cell_size = 58.5 / 4096.0;
        // ② Steam 讨论区 yardstick 法：1 unit = 1/2 inch
        let from_yardstick = 0.0254 / 2.0;

        let cross = CrossCheck::new(
            from_cell_size,
            from_yardstick,
            DEFAULT_CROSS_CHECK_TOLERANCE,
        );
        assert!(
            !cross.agrees(),
            "两个公开来源相差 {:.1}%，应判不通过",
            (cross.ratio - 1.0).abs() * 100.0
        );
        assert!(
            (cross.ratio - 1.0).abs() > 0.10 && (cross.ratio - 1.0).abs() < 0.15,
            "差异应在 10%~15% 之间（实测 12.5%），实为 {:.4}",
            cross.ratio
        );
        // 且这个矛盾必须挡在交付之前，不能被平均掉。
        let cal = MetricScaleCalibration::from_engine_convention(
            "creation-engine",
            from_cell_size,
            "e.md",
        )
        .with_cross_check(cross);
        assert!(matches!(
            cal.validate().unwrap_err(),
            MetricScaleError::CrossCheckFailed { .. }
        ));
        assert_eq!(engine_convention("creation-engine"), None);
    }

    #[test]
    fn cross_check_flags_a_wrong_convention() {
        let ok = CrossCheck::new(0.02, 0.0206, DEFAULT_CROSS_CHECK_TOLERANCE);
        assert!(ok.agrees(), "3% 偏差应通过");
        let bad = CrossCheck::new(0.02, 0.01, DEFAULT_CROSS_CHECK_TOLERANCE);
        assert!(!bad.agrees(), "差一倍必须判失败");
    }

    #[test]
    fn engine_convention_alone_is_rejected() {
        let cal = MetricScaleCalibration::from_engine_convention("ue3", 0.02, "evidence/ue3.md");
        assert_eq!(
            cal.validate().unwrap_err(),
            MetricScaleError::ConventionNotCrossChecked {
                engine: "ue3".to_string()
            }
        );
        let crossed =
            cal.with_cross_check(CrossCheck::new(0.02, 0.0203, DEFAULT_CROSS_CHECK_TOLERANCE));
        assert!(crossed.validate().is_ok());
    }

    #[test]
    fn authored_level_metadata_is_a_direct_game_specific_scale_source() {
        let calibration = MetricScaleCalibration::from_authored_level_metadata(
            3,
            1.0,
            "config/depth-map-bounds/kcd2-metric-scale-evidence.json",
        )
        .with_evidence_sha256(format!("sha256:{}", "a".repeat(64)));
        assert!(calibration.validate().is_ok());
        assert!(matches!(
            calibration.source,
            MetricScaleSource::AuthoredLevelMetadata {
                levels: 3,
                unit_size_metres: 1.0
            }
        ));

        let mut invalid = calibration;
        invalid.value = 0.5;
        assert!(matches!(
            invalid.validate(),
            Err(MetricScaleError::InvalidAuthoredLevelMetadata { .. })
        ));
    }

    #[test]
    fn evidence_path_is_mandatory() {
        let cal = MetricScaleCalibration::from_anchor_fit(
            &planted(0.0, 10.0, 0.02, &[3.0, 20.0]),
            10.0,
            "   ",
        )
        .unwrap();
        assert_eq!(
            cal.validate().unwrap_err(),
            MetricScaleError::MissingEvidence
        );
    }

    #[test]
    fn failed_cross_check_is_rejected_rather_than_averaged() {
        let cal = MetricScaleCalibration::from_anchor_fit(
            &planted(0.0, 10.0, 0.02, &[3.0, 20.0]),
            10.0,
            "evidence/anchors.json",
        )
        .unwrap()
        .with_cross_check(CrossCheck::new(0.01, 0.02, DEFAULT_CROSS_CHECK_TOLERANCE));
        assert!(matches!(
            cal.validate().unwrap_err(),
            MetricScaleError::CrossCheckFailed { .. }
        ));
    }

    #[test]
    fn deserialised_cross_check_cannot_forge_the_cached_ratio() {
        let mut cal = MetricScaleCalibration::from_engine_convention("ue3", 0.02, "evidence.md")
            .with_cross_check(CrossCheck::new(0.02, 0.2, DEFAULT_CROSS_CHECK_TOLERANCE));
        cal.cross_check.as_mut().unwrap().ratio = 1.0;
        assert!(matches!(
            cal.validate().unwrap_err(),
            MetricScaleError::CrossCheckFailed { .. }
        ));
    }

    #[test]
    fn cross_check_must_describe_the_value_that_will_ship() {
        let cal = MetricScaleCalibration::from_engine_convention("ue3", 9.99, "evidence.md")
            .with_cross_check(CrossCheck::new(0.02, 0.0203, DEFAULT_CROSS_CHECK_TOLERANCE));
        assert!(matches!(
            cal.validate().unwrap_err(),
            MetricScaleError::CrossCheckValueMismatch { .. }
        ));
    }

    #[test]
    fn anchor_fit_end_to_end_yields_a_valid_calibration() {
        let anchors = vec![
            MetricAnchor::from_known_size("门-3m", 0.02 * 10.0 / 20.0, 960.0, 3.0, 144.0).unwrap(),
            MetricAnchor::from_known_size("车-4.5m", 0.02 * 10.0 / 9.0, 960.0, 4.5, 480.0).unwrap(),
        ];
        let cal =
            MetricScaleCalibration::from_anchor_fit(&anchors, 10.0, "evidence/p1d.json").unwrap();
        assert!((cal.value - 0.02).abs() < 1e-9, "{}", cal.value);
        assert!(cal.validate().is_ok());
        assert!(matches!(
            cal.source,
            MetricScaleSource::KnownSizeAnchors { anchors: 2, .. }
        ));
    }
}

#[cfg(test)]
mod disputed_scale_tests {
    use super::*;

    /// Source 系不给单值。
    ///
    /// Valve 官方同一页写着两个相差 33% 的值，给出其中任何一个都是替读者
    /// 做一个没有依据的选择。更隐蔽的危害是：33% 的分歧下交叉验证仍会「通过」，
    /// 因为总有一个分支离实测值更近——于是错误会带着一份看似合格的验证记录进交付。
    #[test]
    fn source_engine_has_no_convention_value_only_a_disputed_range() {
        assert_eq!(engine_convention("source"), None, "不许给单值");
        let (spread, why) = disputed_engine_scale("source").expect("必须在分歧表里");
        assert!(spread > DEFAULT_CROSS_CHECK_TOLERANCE, "分歧必须超出容差");
        assert!(why.contains("2.54"), "出处要能指回原文");
    }

    /// Skyrim SE 与 Fallout 4 共用同一个常数，因而**继承同一处分歧**。
    ///
    /// 98 号 §5.7.4 说 Skyrim 是「边际成本最低的第二款」，那说的是取值路径可复用；
    /// 但可复用的既包括路径也包括路径上的问题——不能因为「FO4 已经做过」
    /// 就把这个有争议的比例当成已确证。
    #[test]
    fn skyrim_se_inherits_the_fallout4_dispute_rather_than_reusing_it_as_settled() {
        assert_eq!(engine_convention("creation_skyrim_se"), None);
        let (spread, _) = disputed_engine_scale("creation_skyrim_se").expect("必须在分歧表里");
        let (fo4_spread, _) = disputed_engine_scale("creation").expect("必须在分歧表里");
        assert!(
            (spread - fo4_spread).abs() < f64::EPSILON,
            "同一个常数应当继承同一个分歧幅度"
        );
    }

    /// 入表判据是「分歧超出交叉验证容差」。分歧小于容差的引擎不该进这张表，
    /// 否则它会把「先用着、由交叉验证兜底」这条正常路径也堵死。
    #[test]
    fn every_disputed_entry_exceeds_the_cross_check_tolerance() {
        for (engine, spread, why) in DISPUTED_ENGINE_SCALES {
            assert!(
                *spread > DEFAULT_CROSS_CHECK_TOLERANCE,
                "{engine} 的分歧 {spread} 未超出容差，不该进分歧表"
            );
            assert!(!why.trim().is_empty(), "{engine} 缺出处");
        }
    }

    /// 两张表互斥：一个引擎不能既有惯例值又在分歧表里。
    #[test]
    fn convention_table_and_disputed_table_are_disjoint() {
        for (engine, _) in ENGINE_CONVENTIONS {
            assert!(
                disputed_engine_scale(engine).is_none(),
                "{engine} 同时出现在两张表里"
            );
        }
    }

    /// 查表大小写不敏感，与 `engine_convention` 一致。
    #[test]
    fn disputed_lookup_is_case_insensitive() {
        assert!(disputed_engine_scale("SOURCE").is_some());
        assert!(disputed_engine_scale("Creation").is_some());
        assert!(disputed_engine_scale("prism3d").is_none());
    }
}

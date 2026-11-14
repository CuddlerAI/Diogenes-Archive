//! Depth frame health evidence captured beside each index row.
//!
//! The probe records two views of the same decoded frame:
//!
//! - `raw`: decoded f32 buffer values before any storage transform;
//! - `stored`: values after `identity` / `one_minus_d` and an fp16 round trip.
//!
//! Keeping both is important. A naturally simple frame can have few values in
//! both views, while the Fallout 4 failure from #518 had rich raw values that
//! collapsed to a few hundred values only after fp16(`d`).

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::capture_timing::StorageTransform;

pub const FRAME_HEALTH_SCHEMA_V1: &str = "depth-frame-health/v1";

/// Aim for roughly this many deterministic samples per frame.
pub const TARGET_SAMPLES: usize = 8_192;

/// Only evaluate storage collapse when the raw frame itself has ample detail.
pub const MIN_RAW_UNIQUE_FOR_COLLAPSE_CHECK: u32 = 4_096;

/// #518 calibration boundary — now only a **pre-filter**, see [`MAX_STORED_RELATIVE_ERROR`].
///
/// The broken fp16(`d`) Fallout frame had 185 unique values, while the same
/// full-precision source stored as fp16(`1-d`) had 6,112 (docs/94 section 1).
///
/// # 为什么它不能单独定案
///
/// 唯一值数不知道**塌掉的那些值原本相差多远**，而那正是全部区别所在。实测三帧
/// 真实 ETS2（reversed-Z 存 `d`，编码完全正确）：raw 7 258 / stored **1 057**——
/// 低于这条线，会被判成 collapse。同一条线上 FO4 的坏帧是 226。
///
/// 两者都「唯一值少」，但前者合并掉的值在 `z` 上相差 2⁻¹¹（0.05%，可接受的舍入），
/// 后者在 20 m 处相差 5%（把深度层次抹掉）。所以这条线只用来**筛出候选**，
/// 定案交给 [`measure_stored_relative_error`]。
pub const MIN_STORED_UNIQUE_SAMPLED: u32 = 2_048;

/// fp16 的理论相对精度：尾数 10 位 ⇒ 2⁻¹¹ ≈ 4.88e-4。
/// **存逆深度时每个像素都应该落在这个数上**（94 号裁决的全部意义）。
pub const FP16_RELATIVE_PRECISION: f32 = 1.0 / 2048.0;

/// 编码误差报警线：理论值的 20 倍，取整到 1%。
///
/// # 为什么这个阈值不需要标定
///
/// 它不是从数据拟合的，是从 fp16 的定义推出来的：编码正确时**每个像素**都是
/// 2⁻¹¹，与场景、游戏、距离无关；编码错误时是 `2⁻¹¹·z/n`——FO4 的
/// `n = 0.2143 m` 在 5 m 处就 1.1%、20 m 处 4.5%。两边差一到两个数量级。
///
/// 20 倍的余量留给 `1−d` 的浮点相减与降采样带来的抖动。
pub const MAX_STORED_RELATIVE_ERROR: f32 = 0.01;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameHealthStats {
    pub pixels: u64,
    pub finite: u64,
    pub nan: u64,
    pub infinite: u64,
    /// Whether every pixel had the same exact bit pattern.
    pub uniform: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_finite_bits: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_finite_bits: Option<u32>,
    pub finite_samples: u32,
    pub unique_sampled: u32,
}

impl FrameHealthStats {
    fn inspect_raw(values: &[f32], width: usize) -> Self {
        inspect(values, width, |value| value)
    }

    fn inspect_for_storage(values: &[f32], width: usize, transform: StorageTransform) -> Self {
        inspect(values, width, |value| {
            crate::half::quantize_to_f16(transform.apply(value))
        })
    }

    fn validate(&self, label: &str) -> Result<(), String> {
        if self.finite + self.nan + self.infinite != self.pixels {
            return Err(format!(
                "{label} health counts do not add up: finite={} nan={} infinite={} pixels={}",
                self.finite, self.nan, self.infinite, self.pixels
            ));
        }
        if self.unique_sampled > self.finite_samples {
            return Err(format!(
                "{label} unique_sampled {} exceeds finite_samples {}",
                self.unique_sampled, self.finite_samples
            ));
        }
        if self.finite_samples as usize > TARGET_SAMPLES {
            return Err(format!(
                "{label} finite_samples {} exceeds target {TARGET_SAMPLES}",
                self.finite_samples
            ));
        }
        match (self.min_finite_bits, self.max_finite_bits) {
            (None, None) if self.finite == 0 => {}
            (Some(min), Some(max)) if self.finite != 0 => {
                let min = f32::from_bits(min);
                let max = f32::from_bits(max);
                if !min.is_finite() || !max.is_finite() || min > max {
                    return Err(format!(
                        "{label} finite range is invalid: min={min:?} max={max:?}"
                    ));
                }
            }
            _ => {
                return Err(format!(
                    "{label} finite range presence disagrees with finite count {}",
                    self.finite
                ));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "code")]
pub enum FrameDefect {
    /// No finite value survived decode. A few NaNs are allowed; an entire frame is not.
    RawAllNonFinite,
    /// The decoded frame is exactly one value (for example a clear-only DSV snapshot).
    RawUniform,
    /// The fp16 storage result contains no finite value.
    StoredAllNonFinite,
    /// Raw input had geometry, but storage collapsed it to one exact value.
    StoredUniform,
    /// Raw input was detailed, but the transformed fp16 representation lost most layers.
    StoredQuantizationCollapse {
        raw_unique_sampled: u32,
        stored_unique_sampled: u32,
    },
}

/// 不派生 `Eq`：[`Self::stored_relative_error`] 带 f32，而 f32 的相等
/// 本来就不是等价关系（NaN ≠ NaN）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FrameHealthEvidence {
    pub schema: String,
    pub raw: FrameHealthStats,
    pub stored: FrameHealthStats,
    /// 落盘编码引入的相对深度误差。
    ///
    /// **与 `raw` 同属「采集时事实」**：两者都无法从盘上的 EXR 重算——
    /// 原始缓冲值那时已经不在了。所以下游只能校验它与 `defects` 自洽
    /// （[`Self::validate`]），不能独立复核它诚不诚实，这一点与 #550 对 `raw`
    /// 的处理是同一档。
    ///
    /// Z 约定判不出来时为 `None`（例如整帧清除值），此时 collapse 不定罪。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stored_relative_error: Option<StoredRelativeError>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub defects: Vec<FrameDefect>,
}

impl FrameHealthEvidence {
    #[must_use]
    pub fn inspect(depth: &[f32], width: usize, transform: StorageTransform) -> Self {
        Self::inspect_with_convention(depth, width, transform, None)
    }

    /// Inspect a frame with an optional independently observed Z convention.
    ///
    /// A projection constant buffer is stronger evidence than searching the pixels for a clear
    /// region: full-screen geometry can legitimately contain neither clear endpoint. `None`
    /// preserves the legacy inference path for captures that have no projection observation.
    #[must_use]
    pub fn inspect_with_convention(
        depth: &[f32],
        width: usize,
        transform: StorageTransform,
        convention: Option<crate::decode::ZConvention>,
    ) -> Self {
        let (raw, stored) = inspect_normalized_pair(depth, width, transform).unwrap_or_else(|| {
            (
                FrameHealthStats::inspect_raw(depth, width),
                FrameHealthStats::inspect_for_storage(depth, width, transform),
            )
        });
        // 只在唯一值数已经可疑时才算——它比两次线性扫描贵，
        // 而绝大多数帧根本走不到 collapse 这一步。
        let relative = (raw.unique_sampled >= MIN_RAW_UNIQUE_FOR_COLLAPSE_CHECK
            && stored.unique_sampled < MIN_STORED_UNIQUE_SAMPLED)
            .then(|| match convention {
                Some(convention) => measure_stored_relative_error_with_convention(
                    depth, width, transform, convention,
                ),
                None => measure_stored_relative_error(depth, width, transform),
            })
            .flatten();
        let defects = classify(&raw, &stored, relative);
        Self {
            schema: FRAME_HEALTH_SCHEMA_V1.into(),
            raw,
            stored,
            stored_relative_error: relative,
            defects,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.schema != FRAME_HEALTH_SCHEMA_V1 {
            return Err(format!("unsupported frame health schema `{}`", self.schema));
        }
        self.raw.validate("raw")?;
        self.stored.validate("stored")?;
        if self.raw.pixels != self.stored.pixels {
            return Err(format!(
                "raw/stored health pixel counts differ: {}/{}",
                self.raw.pixels, self.stored.pixels
            ));
        }
        let expected = classify(&self.raw, &self.stored, self.stored_relative_error);
        if self.defects != expected {
            return Err(format!(
                "frame health defects disagree with metrics: declared={:?} expected={expected:?}",
                self.defects
            ));
        }
        Ok(())
    }

    /// Recompute the stored side from the actual decoded EXR and require a bit-exact match.
    ///
    /// `stored_values` are already fp16 samples expanded to f32, so this path must not apply
    /// the storage transform or quantize a second time.
    pub fn validate_stored_values(
        &self,
        stored_values: &[f32],
        width: usize,
    ) -> Result<(), String> {
        self.validate()?;
        let actual = FrameHealthStats::inspect_raw(stored_values, width);
        if actual != self.stored {
            return Err(format!(
                "stored frame health differs from actual EXR: declared={:?} actual={actual:?}",
                self.stored
            ));
        }
        Ok(())
    }

    #[must_use]
    pub fn is_defective(&self) -> bool {
        !self.defects.is_empty()
    }
}

/// 落盘编码引入的相对深度误差 `|Δz|/z`，第 50 / 99 百分位。
///
/// # 为什么不需要投影参数
///
/// `z ∝ 1/w`，`w` 是逆深度量（传统 Z 是 `1−d`，reversed-Z 是 `d`）。比例系数
/// （`B_m`、`metric_scale`）在比值里约掉了，所以这个量与标定完全无关——
/// `proj_source` 至今可能是 `unknown`，任何需要参数的判据今天都跑不了。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct StoredRelativeError {
    pub p50: f32,
    pub p99: f32,
    /// 参与统计的像素数。清除值（`w ≤ 0`，z 无穷）与非有限值不算——
    /// 那些像素上这个量没有定义。
    pub samples: u32,
}

/// 按给定变换落盘再读回来，量 `|Δz|/z`。
///
/// # `conv` 必须独立于 `transform`
///
/// **不许**从 `transform` 反推 `conv`（`Identity ⇒ reversed`）：那样这条判据在
/// 变换选错时会跟着一起瞎，而变换选错正是它要抓的东西。这里由
/// [`crate::capture_timing::infer_z_convention`] 从数据本身推。
///
/// 推不出来时返回 `None`——**不猜**。
#[must_use]
pub fn measure_stored_relative_error(
    values: &[f32],
    width: usize,
    transform: StorageTransform,
) -> Option<StoredRelativeError> {
    let conv = crate::capture_timing::infer_z_convention(values)?;
    measure_stored_relative_error_with_convention(values, width, transform, conv)
}

/// Variant of [`measure_stored_relative_error`] whose convention comes from independent
/// projection evidence instead of the sampled pixels.
#[must_use]
pub fn measure_stored_relative_error_with_convention(
    values: &[f32],
    width: usize,
    transform: StorageTransform,
    conv: crate::decode::ZConvention,
) -> Option<StoredRelativeError> {
    let inverse_depth = |d: f32| match conv {
        crate::decode::ZConvention::Traditional => 1.0 - d,
        crate::decode::ZConvention::Reversed => d,
    };

    let stride = sample_stride(values.len(), width);
    let mut errors: Vec<f32> = Vec::new();
    for &d in values.iter().step_by(stride) {
        if !d.is_finite() {
            continue;
        }
        let w = inverse_depth(d);
        // w ≤ 0 是清除值那一端（z = ∞），相对误差在那里没有定义。
        if !w.is_finite() || w <= 0.0 {
            continue;
        }
        let stored = crate::half::quantize_to_f16(transform.apply(d));
        let back = inverse_depth(transform.invert(stored));
        let err = ((back - w) / w).abs();
        if err.is_finite() {
            errors.push(err);
        }
    }
    if errors.is_empty() {
        return None;
    }
    errors.sort_by(f32::total_cmp);
    let at =
        |q: f64| errors[((q * (errors.len() - 1) as f64).round() as usize).min(errors.len() - 1)];
    Some(StoredRelativeError {
        p50: at(0.50),
        p99: at(0.99),
        samples: errors.len() as u32,
    })
}

fn classify(
    raw: &FrameHealthStats,
    stored: &FrameHealthStats,
    relative: Option<StoredRelativeError>,
) -> Vec<FrameDefect> {
    if raw.pixels != 0 && raw.finite == 0 {
        return vec![FrameDefect::RawAllNonFinite];
    }
    if raw.pixels != 0 && raw.uniform {
        return vec![FrameDefect::RawUniform];
    }
    if stored.pixels != 0 && stored.finite == 0 {
        return vec![FrameDefect::StoredAllNonFinite];
    }
    if stored.pixels != 0 && stored.uniform {
        return vec![FrameDefect::StoredUniform];
    }
    // 唯一值数只筛候选。**定案看相对误差**——两种「唯一值少」在数值上不可区分，
    // 而处置完全相反：reversed-Z 存 d 合并掉的值相差 0.05%（正常舍入），
    // 传统 Z 存 d 合并掉的值在 20 m 处相差 5%（把深度层次抹掉）。
    if raw.unique_sampled >= MIN_RAW_UNIQUE_FOR_COLLAPSE_CHECK
        && stored.unique_sampled < MIN_STORED_UNIQUE_SAMPLED
    {
        // 分两档，因为可用的证据不同：
        //
        // - **Z 约定判得出**（帧里有成片的清除值，真实场景几乎总有天空/远平面）：
        //   用相对误差定案。它能分开「合并的值差 0.05%」与「差 5%」，
        //   而唯一值数分不开——实测三帧真实 ETS2 会被唯一值数判成 collapse。
        // - **判不出**（整帧被几何铺满，没有清除区）：退回唯一值数，
        //   即 #550 的原判据。证据不足时宁可保守，但**要知道这是保守**：
        //   这一档上「reversed-Z 正确编码但场景简单」仍会被误判。
        let over = match relative {
            Some(r) => r.p99 > MAX_STORED_RELATIVE_ERROR,
            None => true,
        };
        if over {
            return vec![FrameDefect::StoredQuantizationCollapse {
                raw_unique_sampled: raw.unique_sampled,
                stored_unique_sampled: stored.unique_sampled,
            }];
        }
    }
    Vec::new()
}

/// A full bitwise extrema scan proves every value is finite, nonnegative and <= 1.
/// Within this domain both storage transforms and HALF rounding are monotone: stored
/// extrema are the rounded transformed endpoints, and stored uniformity is exactly
/// equality of those endpoints. Only the unchanged uniqueness sample needs individual
/// quantization. Out-of-domain data, including negative zero, uses the scalar reference.
fn inspect_normalized_pair(
    values: &[f32],
    width: usize,
    transform: StorageTransform,
) -> Option<(FrameHealthStats, FrameHealthStats)> {
    if values.is_empty() {
        return None;
    }
    let (mut min_bits, mut max_bits) = (u32::MAX, 0u32);
    for &value in values {
        min_bits = min_bits.min(value.to_bits());
        max_bits = max_bits.max(value.to_bits());
    }
    if max_bits > 1.0f32.to_bits() {
        return None;
    }
    let stored_at_min = crate::half::quantize_to_f16(transform.apply(f32::from_bits(min_bits)));
    let stored_at_max = crate::half::quantize_to_f16(transform.apply(f32::from_bits(max_bits)));
    let stored_min = stored_at_min.min(stored_at_max).to_bits();
    let stored_max = stored_at_min.max(stored_at_max).to_bits();
    let stride = sample_stride(values.len(), width);
    let samples = values.len().div_ceil(stride);
    let mut raw_seen = HashSet::with_capacity(samples);
    let mut stored_seen = HashSet::with_capacity(samples);
    for &value in values.iter().step_by(stride) {
        raw_seen.insert(value.to_bits());
        stored_seen.insert(crate::half::quantize_to_f16(transform.apply(value)).to_bits());
    }
    let raw = FrameHealthStats {
        pixels: values.len() as u64,
        finite: values.len() as u64,
        nan: 0,
        infinite: 0,
        uniform: min_bits == max_bits,
        min_finite_bits: Some(min_bits),
        max_finite_bits: Some(max_bits),
        finite_samples: samples as u32,
        unique_sampled: raw_seen.len() as u32,
    };
    let stored = FrameHealthStats {
        uniform: stored_min == stored_max,
        min_finite_bits: Some(stored_min),
        max_finite_bits: Some(stored_max),
        unique_sampled: stored_seen.len() as u32,
        ..raw.clone()
    };
    Some((raw, stored))
}

fn inspect(values: &[f32], width: usize, map: impl Fn(f32) -> f32) -> FrameHealthStats {
    let stride = sample_stride(values.len(), width);
    let mut finite = 0u64;
    let mut nan = 0u64;
    let mut infinite = 0u64;
    let mut min: Option<f32> = None;
    let mut max: Option<f32> = None;
    let mut first_bits = None;
    let mut uniform = !values.is_empty();
    for &value in values {
        let value = map(value);
        let bits = value.to_bits();
        match first_bits {
            None => first_bits = Some(bits),
            Some(first) if first != bits => uniform = false,
            Some(_) => {}
        }
        if value.is_nan() {
            nan += 1;
        } else if value.is_infinite() {
            infinite += 1;
        } else {
            finite += 1;
            min = Some(min.map_or(value, |current| current.min(value)));
            max = Some(max.map_or(value, |current| current.max(value)));
        }
    }

    // Sample in a separate bounded pass. Testing a variable stride for every pixel
    // made both full-frame scans pay an integer division, even for unsampled pixels.
    // These are exactly the same indices as index % stride == 0 in the full scan.
    let mut seen = HashSet::with_capacity(values.len().div_ceil(stride));
    let mut finite_samples = 0u32;
    for &value in values.iter().step_by(stride) {
        let value = map(value);
        if value.is_finite() {
            finite_samples += 1;
            seen.insert(value.to_bits());
        }
    }

    FrameHealthStats {
        pixels: values.len() as u64,
        finite,
        nan,
        infinite,
        uniform,
        min_finite_bits: min.map(f32::to_bits),
        max_finite_bits: max.map(f32::to_bits),
        finite_samples,
        unique_sampled: seen.len() as u32,
    }
}

/// Deterministic stride that stays coprime with the image width.
///
/// A stride sharing a factor with 1280 repeatedly visits the same columns and systematically
/// undercounts depth layers. Small frames are scanned in full.
#[must_use]
pub fn sample_stride(pixel_count: usize, width: usize) -> usize {
    if pixel_count <= TARGET_SAMPLES {
        return 1;
    }
    let mut stride = pixel_count.div_ceil(TARGET_SAMPLES).max(2);
    let width = width.max(1);
    while gcd(stride, width) != 1 {
        stride += 1;
    }
    stride
}

fn gcd(mut left: usize, mut right: usize) -> usize {
    while right != 0 {
        let remainder = left % right;
        left = right;
        right = remainder;
    }
    left
}

/// Runtime alert state. It intentionally does not decide deliverability; segment finalization
/// combines the durable evidence with gameplay state and verified frame pairs.
#[derive(Debug, Clone)]
pub struct DefectStreak {
    alert_at: u32,
    /// 连续到这个数就中止。见 [`DEFAULT_ABORT_STREAK`]。
    abort_at: u32,
    consecutive: u32,
    /// 当前这段连续异常的**第一帧**。中止时的现场就以它为中心取（前后各 10 帧）。
    ///
    /// 排障要看的是**转折点**，不是第 300 帧：第 300 帧和第 2 帧长得一模一样，
    /// 而「最后一帧正常的画面」旁边那几帧才说得清是什么把深度弄没的。
    first_defective_seq: Option<u64>,
    longest: u32,
    alerted_at_streak: u32,
    defective_frames: u64,
    total_frames: u64,
}

pub const DEFAULT_ALERT_STREAK: u32 = 60;

impl Default for DefectStreak {
    fn default() -> Self {
        Self::new(DEFAULT_ALERT_STREAK)
    }
}

/// `record` 的结论。
///
/// **告警与中止分开**，因为处置完全不同：告警要采集员留意，
/// 中止要采集员停下来找人。混成一个数字会让「该停了」淹没在告警流里。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreakVerdict {
    /// 正常，或异常尚未到告警线。
    Continue,
    /// 该告警了，附当前连续段长度。升级式——见 [`DefectStreak::record`]。
    Alert(u32),
    /// **该停录了**，附当前连续段长度。
    ///
    /// probe 的 writer 线程收到它会把一份 [`crate::health_abort::HealthAbort`]
    /// latch 给控制线程，由控制线程留存现场并发 `Faulted`；客户端看到 Fault
    /// 就以 `DepthFrameHealth` 停录。**只发一次**——同一次采集里第二次到线
    /// 已经没有意义了，run 那时正在拆。
    Abort(u32),
}

/// 连续多少帧异常就该停录。
///
/// # 为什么是「停」而不是「继续并告警」
///
/// #518 那次第二段 1149 帧连续空白**一直烧到采集结束**——有效数据在 seq 9623 就停了，
/// 之后的 4 分钟全废。告警当时并不存在，但即使存在，光告警也救不回那 4 分钟：
/// 采集员正在专注开车，不会盯着日志。
///
/// 300 帧 ≈ Lane-30 下 20 秒（15 Hz 采样）。短到不至于浪费一整段录制，
/// 长到不会被过场动画或读盘误触——#518 实测最长的**正常**空白段远短于此。
///
/// # 为什么不自动重启
///
/// 连续 300 帧异常说明有系统性问题（DSV 选错、快照环失效、场景切换未适配）。
/// 重启只会再废一次，而且会把现场冲掉。停下来保留现场才是对的。
pub const DEFAULT_ABORT_STREAK: u32 = 300;

impl DefectStreak {
    #[must_use]
    pub fn new(alert_at: u32) -> Self {
        Self::with_thresholds(alert_at, DEFAULT_ABORT_STREAK)
    }

    /// 显式给两条线。**中止线必须严格大于告警线**——否则永远不会告警，
    /// 采集员会在毫无预兆的情况下被停录。
    #[must_use]
    pub fn with_thresholds(alert_at: u32, abort_at: u32) -> Self {
        let alert_at = alert_at.max(1);
        Self {
            alert_at,
            abort_at: abort_at.max(alert_at.saturating_add(1)),
            consecutive: 0,
            first_defective_seq: None,
            longest: 0,
            alerted_at_streak: 0,
            defective_frames: 0,
            total_frames: 0,
        }
    }

    /// Return the current streak when an escalating alert should be emitted.
    ///
    /// `present_seq` 只用来记这一段异常是从哪一帧起的（[`Self::first_defective_seq`]）。
    /// **不能由调用方事后用 `seq − streak + 1` 反推**：深度是从 Present 流里降频
    /// 采样的，序号本来就不连续，那样算出来的帧号在盘上根本不存在。
    pub fn record(&mut self, defective: bool, present_seq: u64) -> StreakVerdict {
        self.total_frames += 1;
        if !defective {
            self.consecutive = 0;
            self.first_defective_seq = None;
            self.alerted_at_streak = 0;
            return StreakVerdict::Continue;
        }
        self.defective_frames += 1;
        self.consecutive += 1;
        if self.consecutive == 1 {
            self.first_defective_seq = Some(present_seq);
        }
        self.longest = self.longest.max(self.consecutive);
        // 中止先判：到了这条线就不该再纠结要不要告警了。
        if self.consecutive >= self.abort_at {
            return StreakVerdict::Abort(self.consecutive);
        }
        if self.consecutive < self.alert_at {
            return StreakVerdict::Continue;
        }
        if self.alerted_at_streak == 0
            || self.consecutive >= self.alerted_at_streak.saturating_mul(2)
        {
            self.alerted_at_streak = self.consecutive;
            return StreakVerdict::Alert(self.consecutive);
        }
        StreakVerdict::Continue
    }

    #[must_use]
    pub fn totals(&self) -> (u64, u64) {
        (self.defective_frames, self.total_frames)
    }

    #[must_use]
    pub fn longest(&self) -> u32 {
        self.longest
    }

    /// 当前这段连续异常的第一帧；没有正在进行的异常段时为 `None`。
    #[must_use]
    pub fn first_defective_seq(&self) -> Option<u64> {
        self.first_defective_seq
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalized_extrema_match_scalar_stats_at_half_rounding_boundaries() {
        let mut values = vec![0.0, 1.0];
        // Every positive HALF value in [0,1], and adjacent f32 values around each
        // rounding midpoint. This includes subnormal/normal transitions and ties.
        for bits in 0..=0x3c00u16 {
            let value = crate::half::f16_bits_to_f32(bits);
            values.push(value);
            if bits < 0x3c00 {
                let next = crate::half::f16_bits_to_f32(bits + 1);
                let midpoint = (value + next) * 0.5;
                values.extend([
                    f32::from_bits(midpoint.to_bits() - 1),
                    midpoint,
                    f32::from_bits(midpoint.to_bits() + 1),
                ]);
            }
        }
        let mut seed = 42u32;
        for _ in 0..1280 * 720 {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            values.push(f32::from_bits(seed % (1.0f32.to_bits() + 1)));
        }
        for transform in [StorageTransform::Identity, StorageTransform::OneMinusD] {
            for width in [1, 127, 1280] {
                for slice in [
                    values.as_slice(),
                    &[0.0; 17],
                    &[1.0; 17],
                    &[0.5, f32::from_bits(0.5f32.to_bits() + 1)],
                ] {
                    let (raw, stored) = inspect_normalized_pair(slice, width, transform).unwrap();
                    assert_eq!(raw, FrameHealthStats::inspect_raw(slice, width));
                    assert_eq!(
                        stored,
                        FrameHealthStats::inspect_for_storage(slice, width, transform)
                    );
                }
            }
        }
    }

    #[test]
    fn normalized_stats_never_hide_an_unsampled_invalid_value() {
        let mut values = vec![0.5; 16_384];
        assert!(sample_stride(values.len(), 128) > 1);
        for invalid in [
            f32::NAN,
            f32::INFINITY,
            f32::NEG_INFINITY,
            -0.0,
            -0.01,
            f32::from_bits(1.0f32.to_bits() + 1),
        ] {
            values[1] = invalid; // Deliberately outside the uniqueness sample.
            for transform in [StorageTransform::Identity, StorageTransform::OneMinusD] {
                assert!(inspect_normalized_pair(&values, 128, transform).is_none());
                let actual = FrameHealthEvidence::inspect(&values, 128, transform);
                assert_eq!(actual.raw, FrameHealthStats::inspect_raw(&values, 128));
                assert_eq!(
                    actual.stored,
                    FrameHealthStats::inspect_for_storage(&values, 128, transform)
                );
            }
        }
        assert!(inspect_normalized_pair(&[], 0, StorageTransform::Identity).is_none());
    }

    #[test]
    fn separated_samples_keep_the_full_scan_indices_and_non_finite_rules() {
        for (pixels, width) in [(0, 0), (17, 4), (16_385, 128), (1280 * 720, 1280)] {
            let depth: Vec<f32> = (0..pixels)
                .map(|index| match index % 131 {
                    0 => f32::NAN,
                    1 => f32::INFINITY,
                    2 => f32::NEG_INFINITY,
                    3 => -0.0,
                    _ => (index % 16_384) as f32 / 16_384.0,
                })
                .collect();
            let stride = sample_stride(pixels, width);
            for transform in [StorageTransform::Identity, StorageTransform::OneMinusD] {
                let stored: Vec<f32> = depth
                    .iter()
                    .map(|&value| crate::half::quantize_to_f16(transform.apply(value)))
                    .collect();
                let observed = FrameHealthStats::inspect_for_storage(&depth, width, transform);
                let sampled: Vec<u32> = stored
                    .iter()
                    .enumerate()
                    .filter(|(index, value)| index % stride == 0 && value.is_finite())
                    .map(|(_, value)| value.to_bits())
                    .collect();
                assert_eq!(observed.finite_samples as usize, sampled.len());
                assert_eq!(
                    observed.unique_sampled as usize,
                    sampled.into_iter().collect::<HashSet<_>>().len()
                );
                assert_eq!(
                    observed.finite as usize,
                    stored.iter().filter(|value| value.is_finite()).count()
                );
                assert_eq!(
                    observed.nan as usize,
                    stored.iter().filter(|value| value.is_nan()).count()
                );
                assert_eq!(
                    observed.infinite as usize,
                    stored.iter().filter(|value| value.is_infinite()).count()
                );
            }
        }
    }

    #[test]
    fn all_clear_and_all_nan_have_distinct_root_causes() {
        let clear =
            FrameHealthEvidence::inspect(&vec![1.0; 1280 * 720], 1280, StorageTransform::Identity);
        assert_eq!(clear.defects, vec![FrameDefect::RawUniform]);

        let nan = FrameHealthEvidence::inspect(
            &vec![f32::NAN; 1280 * 720],
            1280,
            StorageTransform::Identity,
        );
        assert_eq!(nan.defects, vec![FrameDefect::RawAllNonFinite]);
        assert!(
            serde_json::to_string(&nan).is_ok(),
            "NaN must not leak into JSON numbers"
        );
    }

    #[test]
    fn fallout_style_traditional_z_collapses_with_identity_but_not_one_minus_d() {
        // Full-precision traditional-Z values, concentrated near d=1 like the #518 outdoor
        // frame. This is a reproducible shape test; docs/94 carries the real-frame calibration
        // numbers (185 broken vs 6,112 after the correct transform).
        let pixels = 1280 * 720;
        let near = 0.2143f32;
        let depth: Vec<f32> = (0..pixels)
            .map(|index| {
                let t = index as f32 / (pixels - 1) as f32;
                let z = 2.0 + 198.0 * t;
                1.0 - near / z
            })
            .collect();

        let broken = FrameHealthEvidence::inspect(&depth, 1280, StorageTransform::Identity);
        assert!(matches!(
            broken.defects.as_slice(),
            [FrameDefect::StoredQuantizationCollapse { .. }]
        ));
        assert!(broken.stored.unique_sampled < MIN_STORED_UNIQUE_SAMPLED);

        let repaired = FrameHealthEvidence::inspect(&depth, 1280, StorageTransform::OneMinusD);
        assert_eq!(repaired.defects, Vec::<FrameDefect>::new());
        assert!(repaired.stored.unique_sampled >= MIN_STORED_UNIQUE_SAMPLED);
    }

    #[test]
    fn a_simple_scene_is_not_called_quantization_collapse() {
        let depth: Vec<f32> = (0..1280 * 720)
            .map(|index| 0.25 + (index % 64) as f32 / 256.0)
            .collect();
        let health = FrameHealthEvidence::inspect(&depth, 1280, StorageTransform::Identity);
        assert_eq!(health.raw.unique_sampled, 64);
        assert!(health.defects.is_empty());
    }

    #[test]
    fn actual_stored_values_must_match_the_declared_roundtrip() {
        let depth: Vec<f32> = (0..16_384).map(|index| index as f32 / 16_384.0).collect();
        let health = FrameHealthEvidence::inspect(&depth, 128, StorageTransform::Identity);
        let mut stored: Vec<f32> = depth
            .iter()
            .map(|&value| crate::half::quantize_to_f16(value))
            .collect();
        health.validate_stored_values(&stored, 128).unwrap();
        stored.fill(0.0);
        assert!(health.validate_stored_values(&stored, 128).is_err());
    }

    #[test]
    fn stride_does_not_lock_to_common_frame_widths() {
        for (width, height) in [(1280, 720), (1920, 1080), (640, 360)] {
            let stride = sample_stride(width * height, width);
            assert_eq!(gcd(stride, width), 1);
            let samples = (width * height).div_ceil(stride);
            assert!(
                (4_000..=TARGET_SAMPLES).contains(&samples),
                "{width}x{height}: {samples}"
            );
        }
    }

    #[test]
    fn a_long_failure_alerts_early_without_log_spam() {
        let mut streak = DefectStreak::new(60);
        let mut alerts = Vec::new();
        for seq in 0..1149 {
            if let StreakVerdict::Alert(value) = streak.record(true, seq) {
                alerts.push(value);
            }
        }
        // 480 / 960 那两级不再出现——300 帧处已经中止，采集不该跑到那里。
        // 这个期望值原本编码的正是 #518「烧到采集结束」的旧行为。
        assert_eq!(alerts, vec![60, 120, 240]);
        assert_eq!(streak.totals(), (1149, 1149));
        assert_eq!(streak.longest(), 1149);
    }

    #[test]
    fn recovery_arms_the_next_streak() {
        let mut streak = DefectStreak::new(3);
        assert_eq!(streak.record(true, 10), StreakVerdict::Continue);
        assert_eq!(streak.record(true, 12), StreakVerdict::Continue);
        assert_eq!(streak.record(true, 14), StreakVerdict::Alert(3));
        assert_eq!(streak.record(false, 16), StreakVerdict::Continue);
        assert_eq!(streak.record(true, 18), StreakVerdict::Continue);
        assert_eq!(streak.record(true, 20), StreakVerdict::Continue);
        assert_eq!(streak.record(true, 22), StreakVerdict::Alert(3));
    }

    /// 现场以异常段的**第一帧**为中心，所以那个帧号必须是实际写盘的序号。
    ///
    /// 深度是从 Present 流里降频采样的，序号天然不连续；`seq − streak + 1`
    /// 算出来的是一个盘上没有的帧号，排障时会指着不存在的文件。
    #[test]
    fn the_streak_remembers_the_frame_it_actually_started_on() {
        let mut streak = DefectStreak::new(3);
        assert_eq!(streak.first_defective_seq(), None);
        streak.record(false, 9_620);
        assert_eq!(streak.first_defective_seq(), None);
        // 步长 2（30 fps 采 15 Hz）：起点是 9 625，不是 9 627 − 3 + 1 = 9 625 的巧合
        streak.record(true, 9_625);
        streak.record(true, 9_627);
        assert_eq!(streak.record(true, 9_631), StreakVerdict::Alert(3));
        assert_eq!(streak.first_defective_seq(), Some(9_625));
        // 恢复一帧就重新起算，下一段的起点是下一段自己的第一帧
        streak.record(false, 9_633);
        assert_eq!(streak.first_defective_seq(), None);
        streak.record(true, 9_700);
        assert_eq!(streak.first_defective_seq(), Some(9_700));
    }

    // --- 相对误差判据（复核 #550 的 collapse 口径）---

    /// 造两帧只差 Z 约定的数据，量各自的相对深度误差。
    ///
    /// **这是 94 号裁决的可执行形式**：同样一份数据、同样的 fp16，
    /// 存逆深度就是 2⁻¹¹，存原始 `d` 就是百分之几。
    #[test]
    fn the_relative_error_separates_correct_encoding_from_collapse() {
        let pixels = 20_000usize;
        // 传统 Z：9% 清除值 1.0（让 Z 约定判得出）+ 几何。near 用 FO4 实测的 0.2143 m
        let trad: Vec<f32> = (0..pixels)
            .map(|i| {
                if i % 100 < 9 {
                    1.0
                } else {
                    1.0 - 0.2143 / (2.0 + 198.0 * (i as f32 / pixels as f32))
                }
            })
            .collect();
        let bad = measure_stored_relative_error(&trad, 200, StorageTransform::Identity)
            .expect("有清除区，Z 约定判得出");
        let good =
            measure_stored_relative_error(&trad, 200, StorageTransform::OneMinusD).expect("同上");
        assert!(
            bad.p99 > MAX_STORED_RELATIVE_ERROR,
            "传统 Z 存 d 必须超线，实测 p99={:.2e}",
            bad.p99
        );
        assert!(
            good.p99 < 2.0 * FP16_RELATIVE_PRECISION,
            "存逆深度就该贴着 2⁻¹¹={:.2e}，实测 p99={:.2e}",
            FP16_RELATIVE_PRECISION,
            good.p99
        );
        assert!(bad.p99 / good.p99 > 20.0, "两者应差一到两个数量级");

        // reversed-Z 存 d 也是最优的——d 本身已是逆深度
        let rev: Vec<f32> = (0..pixels)
            .map(|i| {
                if i % 100 < 9 {
                    0.0
                } else {
                    0.1 / (1.0 + 199.0 * (i as f32 / pixels as f32))
                }
            })
            .collect();
        let r = measure_stored_relative_error(&rev, 200, StorageTransform::Identity).unwrap();
        assert!(r.p99 < 2.0 * FP16_RELATIVE_PRECISION);
    }

    /// 判不出 Z 约定时返回 `None`——**不猜**。
    #[test]
    fn no_clear_region_means_no_measurement_rather_than_a_guess() {
        // 纯几何铺满，两端都没有成片的清除值
        let ramp: Vec<f32> = (0..20_000)
            .map(|i| 0.2 + 0.5 * (i as f32 / 20_000.0))
            .collect();
        assert_eq!(
            measure_stored_relative_error(&ramp, 200, StorageTransform::Identity),
            None
        );
        // 整帧清除值：w ≤ 0，相对误差没有定义
        assert_eq!(
            measure_stored_relative_error(&[1.0; 512], 32, StorageTransform::OneMinusD),
            None
        );
    }

    #[test]
    fn observed_projection_convention_resolves_a_no_clear_frame() {
        // Full-screen traditional-Z geometry: rich f32 values packed near 1, but neither endpoint
        // appears often enough for infer_z_convention. This is the run18 failure shape.
        let pixels = 1280 * 720;
        let depth: Vec<f32> = (0..pixels)
            .map(|index| {
                let t = index as f32 / (pixels - 1) as f32;
                1.0 - (0.001 + 0.0005 * t)
            })
            .collect();
        assert_eq!(crate::capture_timing::infer_z_convention(&depth), None);

        let inferred_only = FrameHealthEvidence::inspect(&depth, 1280, StorageTransform::OneMinusD);
        assert!(matches!(
            inferred_only.defects.as_slice(),
            [FrameDefect::StoredQuantizationCollapse { .. }]
        ));

        let observed = FrameHealthEvidence::inspect_with_convention(
            &depth,
            1280,
            StorageTransform::OneMinusD,
            Some(crate::decode::ZConvention::Traditional),
        );
        assert!(observed.defects.is_empty(), "{observed:?}");
        assert!(
            observed
                .stored_relative_error
                .is_some_and(|error| error.p99 < 2.0 * FP16_RELATIVE_PRECISION),
            "{observed:?}"
        );

        let wrong_transform = FrameHealthEvidence::inspect_with_convention(
            &depth,
            1280,
            StorageTransform::Identity,
            Some(crate::decode::ZConvention::Traditional),
        );
        assert!(matches!(
            wrong_transform.defects.as_slice(),
            [FrameDefect::StoredQuantizationCollapse { .. }]
        ));
    }

    /// 决策逻辑单独测，不依赖任何能复现特定唯一值数的假帧。
    ///
    /// # 这里的四行就是本次改动的全部内容
    ///
    /// 实测：三帧真实 ETS2（reversed-Z 存 `d`，编码**正确**）是 raw 7 258 /
    /// stored 1 057——落在候选区里。只看唯一值数就会把唯一一份采对的数据判死。
    #[test]
    fn unique_count_only_nominates_a_candidate_and_relative_error_decides() {
        let raw = FrameHealthStats {
            pixels: 921_600,
            finite: 921_600,
            nan: 0,
            infinite: 0,
            uniform: false,
            min_finite_bits: Some(0.0f32.to_bits()),
            max_finite_bits: Some(0.92f32.to_bits()),
            finite_samples: 8_156,
            unique_sampled: 7_258, // 真实 ETS2 帧的数
        };
        let stored = FrameHealthStats {
            unique_sampled: 1_057, // 同上，低于 MIN_STORED_UNIQUE_SAMPLED
            ..raw
        };
        let tiny = StoredRelativeError {
            p50: FP16_RELATIVE_PRECISION,
            p99: FP16_RELATIVE_PRECISION,
            samples: 7_399,
        };
        let huge = StoredRelativeError {
            p50: 0.05,
            p99: 0.19,
            samples: 7_399,
        };

        assert_eq!(
            classify(&raw, &stored, Some(tiny)),
            Vec::new(),
            "编码正确（误差贴着 2⁻¹¹）却因为唯一值少被判死"
        );
        assert!(matches!(
            classify(&raw, &stored, Some(huge)).as_slice(),
            [FrameDefect::StoredQuantizationCollapse { .. }]
        ));
        // 判不出 Z 约定时退回唯一值数，即 #550 的原判据
        assert!(matches!(
            classify(&raw, &stored, None).as_slice(),
            [FrameDefect::StoredQuantizationCollapse { .. }]
        ));
        // 没进候选区就一律不判
        let plenty = FrameHealthStats {
            unique_sampled: 6_000,
            ..stored
        };
        assert_eq!(classify(&raw, &plenty, Some(huge)), Vec::new());
    }

    /// `validate()` 要把这个新字段纳入自洽校验，否则可以声明一份
    /// 「误差很小」却带着 collapse 的证据，反之亦然。
    #[test]
    fn declared_relative_error_must_agree_with_declared_defects() {
        let pixels = 1280 * 720;
        let frame: Vec<f32> = (0..pixels)
            .map(|i| {
                if i % 100 < 9 {
                    1.0
                } else {
                    1.0 - 0.2143 / (2.0 + 198.0 * (i as f32 / pixels as f32))
                }
            })
            .collect();
        let mut e = FrameHealthEvidence::inspect(&frame, 1280, StorageTransform::Identity);
        e.validate().expect("刚算出来的证据必须自洽");
        assert!(matches!(
            e.defects.as_slice(),
            [FrameDefect::StoredQuantizationCollapse { .. }]
        ));

        // 把误差改小但保留 collapse 声明 ⇒ 自相矛盾，必须拒
        e.stored_relative_error = Some(StoredRelativeError {
            p50: FP16_RELATIVE_PRECISION,
            p99: FP16_RELATIVE_PRECISION,
            samples: 7_399,
        });
        assert!(e.validate().is_err(), "误差与缺陷声明不一致却被放行");
    }

    #[test]
    fn the_518_blackout_aborts_at_300_instead_of_burning_to_the_end() {
        // #518 第二段 1149 帧连续空白一直烧到采集结束，有效数据在 seq 9623 就停了，
        // 之后 4 分钟全废。这条钉住「第 300 帧就该停」。
        let mut s = DefectStreak::default();
        let mut first_abort = None;
        for i in 1..=1149u32 {
            if let StreakVerdict::Abort(n) = s.record(true, 9_623 + u64::from(i)) {
                first_abort.get_or_insert((i, n));
            }
        }
        assert_eq!(first_abort, Some((300, 300)), "应当在第 300 帧首次中止");
        assert_eq!(
            s.first_defective_seq(),
            Some(9_624),
            "现场要以异常段的第一帧为中心，不是第 300 帧"
        );
    }

    #[test]
    fn alerts_come_before_the_abort_so_it_is_not_a_surprise() {
        // 中止线必须严格大于告警线——否则采集员在毫无预兆的情况下被停录。
        let mut s = DefectStreak::default();
        let mut alerts = 0;
        for seq in 1..=299u64 {
            if matches!(s.record(true, seq), StreakVerdict::Alert(_)) {
                alerts += 1;
            }
        }
        assert!(alerts >= 2, "中止前应当已经告警过若干次，实得 {alerts}");
        assert!(matches!(s.record(true, 300), StreakVerdict::Abort(300)));
    }

    #[test]
    fn a_degenerate_threshold_pair_still_alerts_first() {
        // 传入 abort <= alert 时不能让告警永远不发生。
        let mut s = DefectStreak::with_thresholds(60, 10);
        let mut saw_alert = false;
        for seq in 0..200 {
            match s.record(true, seq) {
                StreakVerdict::Alert(_) => saw_alert = true,
                StreakVerdict::Abort(_) => break,
                StreakVerdict::Continue => {}
            }
        }
        assert!(saw_alert, "中止线被抬到告警线之上后，仍应先告警");
    }

    #[test]
    fn recovery_resets_the_abort_countdown_too() {
        // 恢复后重新进入异常段，中止计数必须从头算——否则跨越多个正常段的
        // 零星异常会累积成一次莫名其妙的停录。
        let mut s = DefectStreak::default();
        for seq in 0..299 {
            s.record(true, seq);
        }
        s.record(false, 299);
        for seq in 300..599 {
            assert!(!matches!(s.record(true, seq), StreakVerdict::Abort(_)));
        }
        assert!(matches!(s.record(true, 599), StreakVerdict::Abort(300)));
        assert_eq!(
            s.first_defective_seq(),
            Some(300),
            "第二段的起点是它自己的第一帧，不是第一段的"
        );
    }
}

//! 深度缓冲原始值 → View-space linear depth（米），以及 Variant S / Variant R 两份产物的派生。
//!
//! 对应 [77 号 §5.5](../../../docs/77-depth-ground-truth-hook-experiment-plan.md)。

use crate::half::{f32_to_f16_bits, quantize_to_f16};

/// 投影参数。无论 reversed-Z 还是传统 Z，深度缓冲值 `d` 都满足 `d = a + b / z`。
///
/// - 传统 Z：`a = f/(f-n)`，`b = -n*f/(f-n)`
/// - reversed-Z + infinite far：`a = 0`，`b = n`
///
/// `metric_scale` 是游戏单位到米的比例（68 号交付物 2 里同名字段）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Projection {
    pub a: f64,
    pub b: f64,
    pub metric_scale: f64,
}

impl Projection {
    /// 由 near/far 构造传统 Z 的投影参数。
    #[must_use]
    pub fn traditional_z(near: f64, far: f64, metric_scale: f64) -> Self {
        let a = far / (far - near);
        Self {
            a,
            b: -near * far / (far - near),
            metric_scale,
        }
    }

    /// 由 near 构造 reversed-Z + 无穷远平面的投影参数。
    #[must_use]
    pub fn reversed_z_infinite(near: f64, metric_scale: f64) -> Self {
        Self {
            a: 0.0,
            b: near,
            metric_scale,
        }
    }

    /// 这组参数属于 reversed-Z 还是传统 Z。
    ///
    /// # 为什么是**推导**出来的，不是一个独立字段
    ///
    /// `d = A + B/z` ⇒ `dd/dz = −B/z²`。z 恒正，所以
    ///
    /// - `B > 0` ⇒ `d` 随距离**减小** ⇒ reversed-Z（近处高值）
    /// - `B < 0` ⇒ `d` 随距离**增大** ⇒ 传统 Z
    ///
    /// 这个判断唯一依赖已经在 profile 里的 `B`。若再存一个独立的 `reverse_z` 布尔，
    /// 两者就能互相矛盾——而矛盾时哪个说了算没有答案，降采样方向和线性化会分别
    /// 按不同的真值走。索引里仍然会写出这个布尔值供人核对，但它是**这里算出来的**。
    ///
    /// `B == 0` 是退化投影（`Projection` 的其他方法也会拒绝它），此处按传统 Z 返回——
    /// 调用方本就不该走到这里，profile 校验会先拦下。
    #[must_use]
    pub fn z_convention(&self) -> crate::decode::ZConvention {
        if self.b > 0.0 {
            crate::decode::ZConvention::Reversed
        } else {
            crate::decode::ZConvention::Traditional
        }
    }

    /// [`Self::z_convention`] 的布尔形式，写进索引供人核对。
    #[must_use]
    pub fn is_reversed_z(&self) -> bool {
        self.z_convention() == crate::decode::ZConvention::Reversed
    }

    /// 缓冲原始值 → 米制 view-space Z。
    ///
    /// 返回 `None` 表示该像素无有效深度（`d` 落在奇点上，或结果非有限）——
    /// 调用方应写入 NaN，这正是 68 号 §6.4 对无效像素的要求。
    #[must_use]
    pub fn linearize(&self, d: f64) -> Option<f64> {
        // A cleared depth pixel means that no scene geometry wrote this sample.  With a finite
        // far plane it is otherwise algebraically indistinguishable from a surface exactly on the
        // far plane, so checking only the projection singularity would turn Fallout 4 sky pixels
        // (`d == 1` for traditional Z) into a plausible-looking finite distance.  PR 405 / docs 68
        // requires invalid pixels to be NaN, and clipped geometry cannot legitimately survive on
        // the clear plane itself.
        let clear_depth = if self.is_reversed_z() { 0.0 } else { 1.0 };
        if d == clear_depth {
            return None;
        }
        let denom = d - self.a;
        if denom == 0.0 {
            return None;
        }
        let z = self.b / denom * self.metric_scale;
        if z.is_finite() && z > 0.0 {
            Some(z)
        } else {
            None
        }
    }
}

/// 交付产物的两种数值格式（[77 号 §5.5.4](../../../docs/77-depth-ground-truth-hook-experiment-plan.md)）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Variant {
    /// 完全符合 68 号 §6.4：fp16 + `trunc(depth×100)/100` 向零截断。
    Spec,
    /// 主档：fp32，不截断。
    Reference,
}

/// 68 号 §6.4 的数值处理：`trunc(depth×100)/100`，**向零截断，不是四舍五入**。
///
/// 规格给的例子是 `3.678 → 3.67`。
#[must_use]
pub fn truncate_two_decimals(z: f64) -> f64 {
    (z * 100.0).trunc() / 100.0
}

/// 把一个米制深度落成指定变体的**最终存储值**。
///
/// `None`（无效像素）在两个变体下都产出 NaN。
#[must_use]
pub fn encode(z: Option<f64>, variant: Variant) -> f32 {
    let Some(z) = z else { return f32::NAN };
    match variant {
        Variant::Reference => z as f32,
        Variant::Spec => quantize_to_f16(truncate_two_decimals(z) as f32),
    }
}

/// Variant S 的 fp16 位模式（写 EXR HALF 通道时用）。
#[must_use]
pub fn encode_spec_bits(z: Option<f64>) -> u16 {
    let Some(z) = z else {
        return f32_to_f16_bits(f32::NAN);
    };
    f32_to_f16_bits(truncate_two_decimals(z) as f32)
}

/// 逐帧格式损失统计（[77 号 §9.3E](../../../docs/77-depth-ground-truth-hook-experiment-plan.md)）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FormatLoss {
    /// 参与统计的有效像素数（NaN 不计）。
    pub valid_pixels: u64,
    /// `|S - R|` 的最大值，米。
    pub max_abs_error_m: f64,
    /// `|S - R| / R` 的平均值。
    pub mean_rel_error: f64,
    /// 深度 ≥ `f16_resolution_limit(0.01)` = 16 m 的像素占比。
    /// 这些像素上「1cm 精度」的声称不成立（§5.5.6）。
    pub beyond_1cm_limit_ratio: f64,
}

/// 对一帧（或一批）米制深度计算 Variant S 相对 Variant R 的格式损失。
#[must_use]
pub fn measure_format_loss(depths: &[Option<f64>]) -> FormatLoss {
    const LIMIT_M: f64 = 16.0; // crate::half::f16_resolution_limit(0.01)

    let mut valid = 0u64;
    let mut max_abs = 0.0f64;
    let mut rel_sum = 0.0f64;
    let mut beyond = 0u64;

    for &z in depths {
        let Some(z) = z else { continue };
        let r = f64::from(encode(Some(z), Variant::Reference));
        let s = f64::from(encode(Some(z), Variant::Spec));
        if !s.is_finite() || !r.is_finite() {
            continue;
        }
        valid += 1;
        let abs = (s - r).abs();
        if abs > max_abs {
            max_abs = abs;
        }
        if r != 0.0 {
            rel_sum += abs / r.abs();
        }
        if z >= LIMIT_M {
            beyond += 1;
        }
    }

    let n = valid.max(1) as f64;
    FormatLoss {
        valid_pixels: valid,
        max_abs_error_m: max_abs,
        mean_rel_error: rel_sum / n,
        beyond_1cm_limit_ratio: beyond as f64 / n,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_z_convention_follows_the_sign_of_b() {
        // reversed-Z + 无穷远：d = n/z，B = n > 0
        assert!(Projection::reversed_z_infinite(0.1, 1.0).is_reversed_z());
        // 传统 Z：B = −nf/(f−n) < 0
        assert!(!Projection::traditional_z(0.1, 1000.0, 1.0).is_reversed_z());
    }

    #[test]
    fn pooling_direction_matches_the_convention_for_both_families() {
        // 这条把「Z 约定」与「降采样取哪个」钉在一起。搞反的后果是降采样时
        // 系统性地选中最远的表面，在每一条物体边缘吃掉前景——而整帧仍然「像深度图」。
        use crate::decode::ZConvention;
        let rev = Projection::reversed_z_infinite(0.1, 1.0);
        // reversed-Z：近处 d 大。1 m 与 10 m 的缓冲值
        let (near_d, far_d) = (0.1 / 1.0, 0.1 / 10.0);
        assert!(near_d > far_d);
        assert_eq!(
            rev.z_convention().nearer(near_d as f32, far_d as f32),
            near_d as f32
        );

        let trad = Projection::traditional_z(0.1, 1000.0, 1.0);
        // 传统 Z：近处 d 小
        let near_d = (trad.a + trad.b / 1.0) as f32;
        let far_d = (trad.a + trad.b / 10.0) as f32;
        assert!(near_d < far_d);
        assert_eq!(trad.z_convention().nearer(near_d, far_d), near_d);
        assert_eq!(trad.z_convention(), ZConvention::Traditional);
    }

    #[test]
    fn reversed_z_infinite_inverts_correctly() {
        let p = Projection::reversed_z_infinite(0.1, 1.0);
        // d = n/z  →  z = n/d
        for z in [0.5f64, 1.0, 10.0, 100.0] {
            let d = 0.1 / z;
            let back = p.linearize(d).unwrap();
            assert!((back - z).abs() < 1e-9, "z={z} 回代得到 {back}");
        }
    }

    #[test]
    fn traditional_z_inverts_correctly() {
        let (near, far) = (0.1f64, 1000.0);
        let p = Projection::traditional_z(near, far, 1.0);
        for z in [0.5f64, 1.0, 10.0, 100.0] {
            let d = far / (far - near) * (1.0 - near / z);
            let back = p.linearize(d).unwrap();
            assert!((back - z).abs() < 1e-6, "z={z} 回代得到 {back}");
        }
    }

    #[test]
    fn traditional_z_clear_plane_is_invalid_even_with_a_finite_far_plane() {
        let p = Projection::traditional_z(0.1, 1000.0, 1.0);
        assert!(p.linearize(1.0).is_none());
        assert!(p.linearize(0.999).is_some());
    }

    #[test]
    fn metric_scale_is_applied() {
        // UE 惯例 1 uu ≈ 2 cm
        let p = Projection::reversed_z_infinite(10.0, 0.02);
        let z_units = 500.0;
        let d = 10.0 / z_units;
        let z_m = p.linearize(d).unwrap();
        assert!(
            (z_m - 10.0).abs() < 1e-9,
            "500 uu × 0.02 应为 10 m，实得 {z_m}"
        );
    }

    #[test]
    fn invalid_depth_becomes_nan_in_both_variants() {
        assert!(encode(None, Variant::Spec).is_nan());
        assert!(encode(None, Variant::Reference).is_nan());
        // 奇点
        let p = Projection::reversed_z_infinite(0.1, 1.0);
        assert!(p.linearize(0.0).is_none());
    }

    #[test]
    fn truncation_rounds_toward_zero_per_spec_example() {
        // 68 号 §6.4 给的例子：3.678 → 3.67（截断，非四舍五入）
        assert!((truncate_two_decimals(3.678) - 3.67).abs() < 1e-12);
        // 四舍五入会给 3.68，必须不是
        assert!((truncate_two_decimals(3.678) - 3.68).abs() > 1e-3);
        // 向零，不是向下：负值也截向零
        assert!((truncate_two_decimals(-3.678) + 3.67).abs() < 1e-12);
    }

    #[test]
    fn spec_variant_loses_one_centimetre_beyond_sixteen_metres() {
        // 近场：截断有效，误差在 1cm 量级以内
        let near = 8.0f64;
        let s_near = f64::from(encode(Some(near + 0.007), Variant::Spec));
        assert!((s_near - near).abs() < 0.02);

        // 远场：fp16 网格已经比 1cm 粗，截断变成空操作
        let far = 100.0f64;
        let a = f64::from(encode(Some(far + 0.01), Variant::Spec));
        let b = f64::from(encode(Some(far + 0.02), Variant::Spec));
        assert_eq!(
            a, b,
            "100 m 处相差 1cm 的两个深度在 fp16 下应落到同一个值（间隔 0.0625 m）"
        );
    }

    #[test]
    fn reference_variant_keeps_one_centimetre_everywhere_relevant() {
        for z in [1.0f64, 16.0, 100.0, 1000.0, 10_000.0] {
            let a = f64::from(encode(Some(z), Variant::Reference));
            let b = f64::from(encode(Some(z + 0.01), Variant::Reference));
            assert!(a != b, "fp32 在 {z} m 处应能分辨 1cm");
        }
    }

    #[test]
    fn format_loss_counts_the_far_field_share() {
        // 注意：整数深度在 fp16 下精确可表示（≤2048），本身测不出量化损失，
        // 所以这个用例只断言「越界占比」这一项。
        let depths: Vec<Option<f64>> = (1..=100).map(|i| Some(f64::from(i))).collect();
        let loss = measure_format_loss(&depths);
        assert_eq!(loss.valid_pixels, 100);
        // 1..=100 m 中 16..=100 共 85 个越过 1cm 界
        assert!((loss.beyond_1cm_limit_ratio - 0.85).abs() < 1e-9);
        assert_eq!(
            loss.max_abs_error_m, 0.0,
            "整数深度应无损，实测 {}",
            loss.max_abs_error_m
        );
    }

    #[test]
    fn format_loss_grows_with_depth() {
        // 这才是 §9.3E 真正要报的东西：同样的小数尾巴，远场损失显著大于近场。
        let near: Vec<Option<f64>> = (1..=50).map(|i| Some(1.0 + f64::from(i) * 0.017)).collect();
        let far: Vec<Option<f64>> = (1..=50)
            .map(|i| Some(100.0 + f64::from(i) * 0.017))
            .collect();

        let ln = measure_format_loss(&near);
        let lf = measure_format_loss(&far);

        // 近场：误差由截断主导，量级 ≤1cm
        assert!(
            ln.max_abs_error_m <= 0.011,
            "近场实测 {}",
            ln.max_abs_error_m
        );
        // 远场：fp16 在 100 m 处间隔 0.0625 m，误差应明显更大
        assert!(
            lf.max_abs_error_m > 0.015,
            "远场实测 {}",
            lf.max_abs_error_m
        );
        assert!(
            lf.max_abs_error_m > ln.max_abs_error_m * 2.0,
            "远场 {} 应远大于近场 {}",
            lf.max_abs_error_m,
            ln.max_abs_error_m
        );
        assert_eq!(ln.beyond_1cm_limit_ratio, 0.0);
        assert_eq!(lf.beyond_1cm_limit_ratio, 1.0);
    }

    #[test]
    fn encoding_is_deterministic() {
        // R-23：Variant S 必须能确定性再生
        let depths: Vec<Option<f64>> = (0..5000)
            .map(|i| {
                if i % 37 == 0 {
                    None
                } else {
                    Some(f64::from(i) * 0.031)
                }
            })
            .collect();
        let first: Vec<u16> = depths.iter().map(|&z| encode_spec_bits(z)).collect();
        let second: Vec<u16> = depths.iter().map(|&z| encode_spec_bits(z)).collect();
        assert_eq!(first, second);
    }
}

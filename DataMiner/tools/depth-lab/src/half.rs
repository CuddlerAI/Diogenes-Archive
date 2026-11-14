//! IEEE 754 binary16（fp16）与 f32 的互转，以及 fp16 网格的精度分析。
//!
//! 为什么自己写而不引依赖：本 crate 只需要转换与间隔分析两件事，都是几十行的位操作，
//! 且必须能**逐位断言**——[77 号 §5.5.6](../../../docs/77-depth-ground-truth-hook-experiment-plan.md)
//! 关于「fp16 装不下 1cm、界限在 16 m」的结论是要写进对外澄清的，
//! 不能建立在一个没读过源码的第三方 crate 上。

/// f32 → fp16 位模式，round-to-nearest-even，与 IEEE 754 binary16 一致。
///
/// NaN 保持 NaN（68 号规格用 NaN 标记无效像素，这条必须无损传递）。
/// 溢出饱和到 ±inf；下溢走非规格化数或 ±0。
#[must_use]
pub fn f32_to_f16_bits(value: f32) -> u16 {
    let bits = value.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exp = ((bits >> 23) & 0xff) as i32;
    let mantissa = bits & 0x007f_ffff;

    // NaN / Inf
    if exp == 0xff {
        return if mantissa != 0 {
            // 保留一个非零尾数位，确保仍是 NaN 而不是退化成 Inf
            sign | 0x7e00
        } else {
            sign | 0x7c00
        };
    }

    // 去掉 f32 偏置 127，加上 f16 偏置 15
    let unbiased = exp - 127;
    let new_exp = unbiased + 15;

    if new_exp >= 0x1f {
        // 上溢
        return sign | 0x7c00;
    }

    if new_exp <= 0 {
        // 非规格化数或零
        if new_exp < -10 {
            return sign;
        }
        // 补上隐含的前导 1，再右移到非规格化位置
        let m = mantissa | 0x0080_0000;
        let shift = (14 - new_exp) as u32;
        let half = 1u32 << (shift - 1);
        let rounded = m + half - u32::from((m >> shift) & 1 == 0 && m & (half - 1) == 0);
        return sign | ((rounded >> shift) as u16);
    }

    // 规格化：尾数 23 位 → 10 位，round-to-nearest-even
    let half = 1u32 << 12;
    let lsb = (mantissa >> 13) & 1;
    let rounded = mantissa + half - u32::from(lsb == 0 && mantissa & (half - 1) == 0);
    // 进位可能把尾数顶进指数，直接加上去即可（指数溢出会自然落到 0x7c00）
    let assembled = ((new_exp as u32) << 10) + (rounded >> 13);
    if assembled >= 0x7c00 {
        return sign | 0x7c00;
    }
    sign | (assembled as u16)
}

/// fp16 位模式 → f32。转换是精确的（f32 能表示所有 fp16 值）。
#[must_use]
pub fn f16_bits_to_f32(bits: u16) -> f32 {
    let sign = u32::from(bits & 0x8000) << 16;
    let exp = i32::from((bits >> 10) & 0x1f);
    let mantissa = u32::from(bits & 0x03ff);

    if exp == 0x1f {
        // Inf / NaN
        let m = if mantissa == 0 { 0 } else { mantissa << 13 | 1 };
        return f32::from_bits(sign | 0x7f80_0000 | m);
    }

    if exp == 0 {
        if mantissa == 0 {
            return f32::from_bits(sign);
        }
        // 非规格化：归一化到 f32 的规格化区间。
        // 10 位尾数字段内的前导零数 = leading_zeros() - 22（u32 高 22 位恒为零）。
        let leading = mantissa.leading_zeros() - 22;
        let e = 127 - 15 - leading;
        let m = (mantissa << (leading + 1)) & 0x03ff;
        return f32::from_bits(sign | (e << 23) | (m << 13));
    }

    let e = (exp - 15 + 127) as u32;
    f32::from_bits(sign | (e << 23) | (mantissa << 13))
}

/// 把一个 f32 量化到 fp16 网格上（转过去再转回来），用于评估量化损失。
#[must_use]
pub fn quantize_to_f16(value: f32) -> f32 {
    f16_bits_to_f32(f32_to_f16_bits(value))
}

/// fp16 在 `value` 所处的二进制区间 `[2^e, 2^(e+1))` 内的相邻可表示值间隔。
///
/// 这是 [77 号 §5.5.6](../../../docs/77-depth-ground-truth-hook-experiment-plan.md)
/// 那张表的计算内核：间隔恒为 `2^(e-10)`。
#[must_use]
pub fn f16_spacing_at(value: f32) -> f32 {
    let v = value.abs();
    if v == 0.0 || !v.is_finite() {
        return f32::NAN;
    }
    let e = v.log2().floor() as i32;
    // 非规格化区：间隔恒定
    if e < -14 {
        return 2f32.powi(-24);
    }
    2f32.powi(e - 10)
}

/// fp16 还能分辨 `resolution` 的最大深度（含）。
///
/// 解 `2^(e-10) > resolution` 得最小越界指数 `e`，阈值即 `2^e`。
/// `resolution = 0.01`（1 cm）时返回 **16.0** —— 这就是 68 号规格
/// 「float16」与「`trunc(depth×100)/100` 约 1cm 精度」两条要求的交集边界。
#[must_use]
pub fn f16_resolution_limit(resolution: f32) -> f32 {
    let e = (10.0 + resolution.log2()).ceil() as i32;
    2f32.powi(e)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_exactly_representable_values() {
        for v in [0.0f32, 1.0, 2.0, 0.5, -1.0, 1024.0, 65504.0, -65504.0] {
            assert_eq!(quantize_to_f16(v), v, "{v} 应当可被 fp16 精确表示");
        }
    }

    #[test]
    fn preserves_nan_because_the_spec_uses_it_for_invalid_pixels() {
        // 68 号 §6.4：无效像素统一写 NaN。这条不能在量化中丢失。
        assert!(quantize_to_f16(f32::NAN).is_nan());
        assert!(quantize_to_f16(-f32::NAN).is_nan());
    }

    #[test]
    fn saturates_beyond_f16_range() {
        assert_eq!(quantize_to_f16(1.0e30), f32::INFINITY);
        assert_eq!(quantize_to_f16(-1.0e30), f32::NEG_INFINITY);
    }

    #[test]
    fn spacing_matches_the_documented_table() {
        // 77 号 §5.5.6 的表，逐行断言
        assert!((f16_spacing_at(1.0) - 0.000_976_562_5).abs() < 1e-9);
        assert!((f16_spacing_at(8.0) - 0.007_812_5).abs() < 1e-9);
        assert!((f16_spacing_at(15.99) - 0.007_812_5).abs() < 1e-9);
        assert!((f16_spacing_at(16.0) - 0.015_625).abs() < 1e-9);
        assert!((f16_spacing_at(32.0) - 0.031_25).abs() < 1e-9);
        assert!((f16_spacing_at(100.0) - 0.062_5).abs() < 1e-9);
    }

    #[test]
    fn one_centimetre_limit_is_sixteen_metres() {
        // 本 crate 存在的理由之一：这个数要写进给需求方的澄清里，必须可复算。
        assert_eq!(f16_resolution_limit(0.01), 16.0);
        // 低于 16 m 仍可分辨 1cm，16 m 起不行
        assert!(f16_spacing_at(15.99) <= 0.01);
        assert!(f16_spacing_at(16.0) > 0.01);
    }

    #[test]
    fn two_decimal_values_stop_looking_like_two_decimals_early() {
        // §5.5.6 的第二层：0.01 不是二进制可表示数，
        // fp16 从很近的距离起就存不出「像两位小数」的值。
        let err_at_8m = (quantize_to_f16(8.01) - 8.01).abs();
        assert!(
            err_at_8m > 1e-3,
            "8.01 在 fp16 下的误差应显著大于 1e-3，实测 {err_at_8m}"
        );
        // 而近场没问题
        let err_at_1m = (quantize_to_f16(1.23) - 1.23).abs();
        assert!(
            err_at_1m < 1e-3,
            "1.23 在 fp16 下应基本保真，实测 {err_at_1m}"
        );
    }

    #[test]
    fn quantization_is_deterministic() {
        // R-23：Variant S 再生必须逐位可复现，量化是其中一环
        for i in 0..10_000 {
            let v = i as f32 * 0.017;
            assert_eq!(f32_to_f16_bits(v), f32_to_f16_bits(v));
        }
    }

    #[test]
    fn subnormals_and_zero_survive() {
        assert_eq!(f32_to_f16_bits(0.0), 0x0000);
        assert_eq!(f32_to_f16_bits(-0.0), 0x8000);
        // 最小非规格化 fp16 = 2^-24
        let tiny = 2f32.powi(-24);
        assert_eq!(quantize_to_f16(tiny), tiny);
    }
}

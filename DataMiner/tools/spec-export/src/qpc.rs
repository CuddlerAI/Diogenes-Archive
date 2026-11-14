//! QPC → 单调纳秒换算的唯一实现。
//!
//! 对齐方案（docs/17 §2.3）要求：`t_mono_ns = qpc × 1e9 / qpc_frequency`，
//! 128 位整数中间值，不假定 `qpc_frequency` 为 10MHz；帧/动作/心跳三条流
//! 共用同一份换算代码，禁止出现第二个实现。

use anyhow::{Context, ensure};

/// 把 QPC tick 换算为单调时钟纳秒值。
///
/// - 乘除全程走 `i128`，任何合法 `i64` 输入都不会溢出中间值；
/// - 整数除法向零取整；因为要求 `qpc >= 0`，等价于向下取整（floor），
///   保证同一频率下换算保持单调不减；
/// - `qpc_frequency` 来自 `run.json` 实测值（例如 10MHz、3.125MHz、
///   3.579545MHz），不允许假定常量。
pub fn qpc_to_mono_ns(qpc: i64, qpc_frequency: i64) -> anyhow::Result<i64> {
    ensure!(
        qpc_frequency > 0,
        "qpc_frequency must be positive, got {qpc_frequency}"
    );
    ensure!(qpc >= 0, "qpc must be non-negative, got {qpc}");
    let ns = i128::from(qpc) * 1_000_000_000_i128 / i128::from(qpc_frequency);
    i64::try_from(ns).with_context(|| {
        format!("t_mono_ns overflows i64: qpc={qpc}, qpc_frequency={qpc_frequency}")
    })
}

#[cfg(test)]
mod tests {
    use super::qpc_to_mono_ns;

    #[test]
    fn ten_mhz_is_exactly_times_one_hundred() {
        for qpc in [0_i64, 1, 99, 10_000_000, 918_273_645_512, 1 << 52] {
            assert_eq!(qpc_to_mono_ns(qpc, 10_000_000).unwrap(), qpc * 100);
        }
    }

    #[test]
    fn non_integral_frequency_matches_i128_reference() {
        // NTSC 色同步频率 3.579545MHz：1e9/freq 不是整数，必须整体乘再除。
        let freq = 3_579_545_i64;
        for qpc in [0_i64, 1, 7, 3_579_545, 3_579_546, 987_654_321_012] {
            let expected = (i128::from(qpc) * 1_000_000_000 / i128::from(freq)) as i64;
            assert_eq!(qpc_to_mono_ns(qpc, freq).unwrap(), expected);
        }
        // 一个手算锚点：qpc = freq → 恰好 1 秒。
        assert_eq!(qpc_to_mono_ns(freq, freq).unwrap(), 1_000_000_000);
        // qpc = 1 tick → floor(1e9 / 3579545) = 279 ns。
        assert_eq!(qpc_to_mono_ns(1, freq).unwrap(), 279);
    }

    #[test]
    fn doc08_sample_frequency() {
        // 08 文档 run.json 示例中的实测频率 3.125MHz：1 tick = 320ns 整。
        assert_eq!(
            qpc_to_mono_ns(918_120_243_402, 3_125_000).unwrap(),
            293_798_477_888_640
        );
    }

    #[test]
    fn large_qpc_does_not_overflow_intermediate() {
        // i64::MAX × 1e9 远超 i64，但 i128 中间值必须扛住；
        // 频率取 1GHz 时结果恰等于 qpc 本身，不溢出输出。
        assert_eq!(qpc_to_mono_ns(i64::MAX, 1_000_000_000).unwrap(), i64::MAX);
        // 多年量级的 10MHz QPC 值（约 29 年 ≈ 9.2e15 tick）换算正常。
        let qpc = 9_200_000_000_000_000_i64;
        assert_eq!(qpc_to_mono_ns(qpc, 10_000_000).unwrap(), qpc * 100);
    }

    #[test]
    fn output_overflow_is_an_error_not_wraparound() {
        // 低频大 qpc 会让纳秒值超出 i64，必须报错而不是回绕。
        assert!(qpc_to_mono_ns(i64::MAX, 10_000_000).is_err());
    }

    #[test]
    fn monotonicity_is_preserved() {
        let freq = 3_579_545_i64;
        let mut previous = None;
        let mut qpc = 0_i64;
        for step in [1_i64, 1, 2, 3, 5, 8, 1000, 999_983, 123_456_789] {
            qpc += step;
            let ns = qpc_to_mono_ns(qpc, freq).unwrap();
            if let Some(previous) = previous {
                assert!(ns >= previous, "monotonicity violated at qpc={qpc}");
            }
            previous = Some(ns);
        }
    }

    #[test]
    fn invalid_inputs_are_rejected() {
        assert!(qpc_to_mono_ns(1, 0).is_err());
        assert!(qpc_to_mono_ns(1, -1).is_err());
        assert!(qpc_to_mono_ns(-1, 10_000_000).is_err());
    }
}

//! 视角尺度标定：平台无关的测量与验收算法（docs/45 §4、docs/46 §3）。
//!
//! 职责边界：本 crate 只消费已经到达的 Raw Input 相对计数（`dx/dy`）与
//! QPC 时间戳，产出 `counts_per_360` 实测值、一致性验收判定与回报率估计。
//! 不触碰任何平台 API——事件采集、热键与向导流程由 Windows 客户端接线，
//! 因此算法可以在非 Windows 开发机与 CI 上直接回归。
//!
//! 判定语义是行为学的：不信任任何自报设置，只看"转满已知角度用了多少计数"
//! 这一游戏实际响应。所有阈值来自目录政策（`Policy`），本 crate 不内置数值。

use input_metrics::PollingRateEstimator;
use serde::{Deserialize, Serialize};

pub use input_metrics::POLLING_ESTIMATOR_VERSION;

pub mod evidence;
pub mod sim;

/// 测量算法版本，写入 `input-calibration.json` 的 `method.algorithm_version`。
pub const ALGORITHM_VERSION: &str = "multi-turn-2";

/// Raw Input 报告没有逐包硬件时间戳；同一次 Win32 混合读取拿到的报告共享
/// 一个投递完成 QPC。该口径写入证据，禁止下游把相同 QPC 当成丢包。
pub const INPUT_TIMESTAMP_SEMANTICS: &str = "delivery_batch_qpc";

/// 旋转方向。`Cw` = 向右整圈（Raw Input `dx` 累计为正），`Ccw` = 向左。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Cw,
    Ccw,
}

/// 测量转速档。快慢档结果不一致意味着游戏内平滑/加速开启（政策不合规）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Speed {
    Slow,
    Normal,
    Fast,
}

/// 验收结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Pass,
    Fail,
}

/// 不通过原因。序列化为 snake_case，与 `dm-input-calibration-2` 的
/// `fail_reasons` 字符串一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailReason {
    /// 没有可用测量，或存在 turns 非正的无效测量。
    InsufficientMeasurements,
    /// 单次测量事件数低于政策下限，回报率与求和不可信。
    InsufficientEvents,
    /// 单次测量可用于估算回报率的报告数不足，不能据此通过回报率硬门槛。
    InsufficientPollingSamples,
    /// 累计计数符号与声明的旋转方向相反（mapping 或操作错误）。
    DirectionMismatch,
    /// 实测中位值偏离目录常数超过容差。
    DeviationExceedsTolerance,
    /// 重复测量之间散布超过容差。
    RepeatInconsistent,
    /// 左转与右转结果不一致。
    LeftRightMismatch,
    /// 快慢档结果不一致——游戏内平滑/加速嫌疑，直接不通过。
    FastSlowMismatch,
    /// 实测回报率低于政策下限。
    PollingBelowFloor,
}

/// 一次测量段（起止标记之间）的累计器。调用方逐包喂入鼠标相对事件。
#[derive(Debug)]
pub struct Accumulator {
    first_qpc: Option<i64>,
    last_qpc: Option<i64>,
    total_dx: i64,
    total_dy: i64,
    event_count: u64,
    polling_rate: PollingRateEstimator,
}

/// 一次测量段的汇总事实，进入证据文件。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeasurementSummary {
    pub total_dx: i64,
    pub total_dy: i64,
    pub event_count: u64,
    pub duration_ms: f64,
    /// 由 delivery batch 内完整报告数修正后的回报率中位数。
    pub polling_hz_p50: Option<f64>,
    /// 对应最慢 5% 报告的回报率下界（历史字段名保留为 p95）。
    pub polling_hz_p95: Option<f64>,
    pub polling_estimated_report_count: u64,
    pub delivery_group_count: u64,
    pub max_delivery_batch_size: u64,
    /// QPC 回退的事件数；同一 delivery batch 的相等 QPC 是合法事实。
    pub non_monotonic_events: u64,
}

impl Accumulator {
    pub fn new(qpc_frequency: i64) -> Self {
        Self {
            first_qpc: None,
            last_qpc: None,
            total_dx: 0,
            total_dy: 0,
            event_count: 0,
            polling_rate: PollingRateEstimator::new(qpc_frequency),
        }
    }

    pub fn push(&mut self, qpc: i64, dx: i32, dy: i32) {
        if self.first_qpc.is_none() {
            self.first_qpc = Some(qpc);
        }
        if self.last_qpc.is_none_or(|last| qpc >= last) {
            self.last_qpc = Some(qpc);
        }
        self.polling_rate.push(qpc);
        self.total_dx += i64::from(dx);
        self.total_dy += i64::from(dy);
        self.event_count += 1;
    }

    /// 结束测量段并汇总；QPC 频率已在创建累计器时钉死。
    pub fn finish(self) -> MeasurementSummary {
        let duration_ms = match (self.first_qpc, self.last_qpc) {
            (Some(first), Some(last)) if last > first => {
                // PollingRateEstimator 在无效频率下不会给出 Hz；duration 同样
                // 保持 0，调用方的事件数/证据门会 fail-closed。
                let frequency = self.polling_rate.qpc_frequency();
                if frequency > 0 {
                    (last - first) as f64 * 1000.0 / frequency as f64
                } else {
                    0.0
                }
            }
            _ => 0.0,
        };
        let polling = self.polling_rate.finish(2);
        MeasurementSummary {
            total_dx: self.total_dx,
            total_dy: self.total_dy,
            event_count: self.event_count,
            duration_ms,
            polling_hz_p50: polling.p50_hz,
            polling_hz_p95: polling.p05_hz,
            polling_estimated_report_count: polling.estimated_report_count,
            delivery_group_count: polling.delivery_group_count,
            max_delivery_batch_size: polling.max_delivery_batch_size,
            non_monotonic_events: polling.non_monotonic_reports,
        }
    }
}

/// 一次完整测量：声明的方向/转速/圈数 + 累计事实。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeasurementRecord {
    pub direction: Direction,
    pub speed: Speed,
    /// 本次测量旋转的整圈数（多圈摊薄端点误差）。
    pub turns: f64,
    pub summary: MeasurementSummary,
}

/// 目录政策中与验收相关的参数（docs/46 §2.1 `view_scale` 块的子集）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Policy {
    /// 目录常数：转满 360° 的期望计数。
    pub expected_counts_per_360: f64,
    /// 偏差/重复散布/左右差的统一容差（百分比，如 0.5）。
    pub tolerance_pct: f64,
    /// 快慢档差异容差（百分比，如 1.0）。
    pub fast_slow_tolerance_pct: f64,
    /// 回报率硬下限（Hz，如 500）。
    pub min_polling_hz: f64,
    /// 单次测量的最小事件数，低于则该批不可信。
    pub min_events_per_measurement: u64,
}

/// 一组测量的验收结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Analysis {
    /// 各次测量 counts_per_360 的中位数；无有效测量时为 None。
    pub counts_per_360_measured: Option<f64>,
    /// 中位数相对目录常数的偏差（百分比）。
    pub deviation_pct: Option<f64>,
    /// 重复测量相对中位数的最大散布（百分比）。
    pub repeat_spread_pct: Option<f64>,
    /// 左右方向中位数差（相对两者均值，百分比）；只有单方向时为 None。
    pub left_right_delta_pct: Option<f64>,
    /// 快慢档中位数差（相对两者均值，百分比）；缺档时为 None。
    pub fast_slow_delta_pct: Option<f64>,
    /// 各测量段回报率 p50 的中位数。
    pub polling_hz_p50: Option<f64>,
    pub verdict: Verdict,
    pub fail_reasons: Vec<FailReason>,
}

/// 训练派生角度列的规范名（docs/45 §2.5）：内部弧度、+yaw 向右、+pitch 向上。
/// 数据集里凡声明角度列都必须携带这个字符串，换算语义变更时升号。
pub const VIEW_SCALE_NORM: &str = "view_scale_norm@1";

/// 每计数角度（度）。
pub fn degrees_per_count(counts_per_360: f64) -> f64 {
    360.0 / counts_per_360
}

/// 一段累计计数按目录常数换算成的角度（度，带符号）。常数缺失或非正时
/// 返回 `None`——测绘模式下只报计数，绝不编角度。
///
/// **符号是 Raw Input 原始口径**：dx 向右为正、**dy 向下为正**。这与训练
/// 派生层 `view_scale_norm@1` 的「+pitch 向上」相反，是故意的——这里报的是
/// 「你刚才往哪边推了多少」，给人对照动作，不是给模型的标签。谁要把它改成
/// 和 pitch 同号，就会让「从下往上扫」的读数变成正数而丢掉方向信息。
///
/// 这是自由测量段的读数（docs/60 §5.2）：
/// - 准星从画面左缘特征扫到右缘特征 → 绝对值即**水平视野**，用来定 `fov_axis`；
/// - 俯仰限位之间扫满（合计 180°）→ `vertical_scale_ratio = 180 / 该值的绝对值`，
///   因为这里按 ratio = 1 换算，读到 ±180 就说明各向同性。
pub fn swept_degrees(counts: i64, counts_per_360: f64) -> Option<f64> {
    (counts_per_360.is_finite() && counts_per_360 > 0.0)
        .then(|| counts as f64 * 360.0 / counts_per_360)
}

/// 训练派生层换算（`view_scale_norm@1`）：+yaw = 向右，弧度。
pub fn yaw_delta_rad(raw_dx: i64, counts_per_360: f64) -> f64 {
    raw_dx as f64 * (std::f64::consts::TAU / counts_per_360)
}

/// 训练派生层换算（`view_scale_norm@1`）：+pitch = 向上，弧度。
/// Raw Input 下移为正 `dy`，故取负。
pub fn pitch_delta_rad(raw_dy: i64, counts_per_360: f64, vertical_scale_ratio: f64) -> f64 {
    -(raw_dy as f64) * vertical_scale_ratio * (std::f64::consts::TAU / counts_per_360)
}

fn median(sorted: &[f64]) -> f64 {
    let n = sorted.len();
    debug_assert!(n > 0);
    if n % 2 == 1 {
        sorted[n / 2]
    } else {
        (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
    }
}

fn median_of(mut values: Vec<f64>) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(|a, b| a.partial_cmp(b).expect("scale values are finite"));
    Some(median(&values))
}

/// 两值相对差（相对两者均值，百分比）。
fn relative_delta_pct(a: f64, b: f64) -> f64 {
    let mean = (a + b) / 2.0;
    if mean == 0.0 {
        return 0.0;
    }
    ((a - b).abs() / mean) * 100.0
}

/// 对一组同状态（同腰射/同倍镜）的测量做验收。
///
/// 判定全部相对 `policy`，无隐藏默认值；`fail_reasons` 去重且顺序稳定。
pub fn analyze(records: &[MeasurementRecord], policy: &Policy) -> Analysis {
    let mut reasons: Vec<FailReason> = Vec::new();
    let push_unique = |reasons: &mut Vec<FailReason>, reason: FailReason| {
        if !reasons.contains(&reason) {
            reasons.push(reason);
        }
    };

    let mut values: Vec<(&MeasurementRecord, f64)> = Vec::new();
    for record in records {
        if record.turns <= 0.0 {
            push_unique(&mut reasons, FailReason::InsufficientMeasurements);
            continue;
        }
        if record.summary.event_count < policy.min_events_per_measurement {
            push_unique(&mut reasons, FailReason::InsufficientEvents);
        } else if record.summary.polling_estimated_report_count < policy.min_events_per_measurement
            || !record.summary.polling_hz_p50.is_some_and(f64::is_finite)
        {
            push_unique(&mut reasons, FailReason::InsufficientPollingSamples);
        }
        let dx = record.summary.total_dx;
        let sign_matches = match record.direction {
            Direction::Cw => dx > 0,
            Direction::Ccw => dx < 0,
        };
        if dx != 0 && !sign_matches {
            push_unique(&mut reasons, FailReason::DirectionMismatch);
        }
        values.push((record, dx.unsigned_abs() as f64 / record.turns));
    }

    if values.is_empty() {
        push_unique(&mut reasons, FailReason::InsufficientMeasurements);
        return Analysis {
            counts_per_360_measured: None,
            deviation_pct: None,
            repeat_spread_pct: None,
            left_right_delta_pct: None,
            fast_slow_delta_pct: None,
            polling_hz_p50: None,
            verdict: Verdict::Fail,
            fail_reasons: reasons,
        };
    }

    let measured =
        median_of(values.iter().map(|(_, v)| *v).collect()).expect("values is non-empty");

    let deviation_pct = if policy.expected_counts_per_360 > 0.0 {
        let deviation =
            ((measured - policy.expected_counts_per_360) / policy.expected_counts_per_360).abs()
                * 100.0;
        if deviation > policy.tolerance_pct {
            push_unique(&mut reasons, FailReason::DeviationExceedsTolerance);
        }
        Some(deviation)
    } else {
        None
    };

    let repeat_spread_pct = if measured > 0.0 {
        let spread = values
            .iter()
            .map(|(_, v)| ((v - measured) / measured).abs() * 100.0)
            .fold(0.0_f64, f64::max);
        if spread > policy.tolerance_pct {
            push_unique(&mut reasons, FailReason::RepeatInconsistent);
        }
        Some(spread)
    } else {
        None
    };

    let median_where = |predicate: &dyn Fn(&MeasurementRecord) -> bool| -> Option<f64> {
        median_of(
            values
                .iter()
                .filter(|(record, _)| predicate(record))
                .map(|(_, v)| *v)
                .collect(),
        )
    };

    let cw = median_where(&|r| r.direction == Direction::Cw);
    let ccw = median_where(&|r| r.direction == Direction::Ccw);
    let left_right_delta_pct = match (cw, ccw) {
        (Some(a), Some(b)) => {
            let delta = relative_delta_pct(a, b);
            if delta > policy.tolerance_pct {
                push_unique(&mut reasons, FailReason::LeftRightMismatch);
            }
            Some(delta)
        }
        _ => None,
    };

    let slow = median_where(&|r| r.speed == Speed::Slow);
    let fast = median_where(&|r| r.speed == Speed::Fast);
    let fast_slow_delta_pct = match (slow, fast) {
        (Some(a), Some(b)) => {
            let delta = relative_delta_pct(a, b);
            if delta > policy.fast_slow_tolerance_pct {
                push_unique(&mut reasons, FailReason::FastSlowMismatch);
            }
            Some(delta)
        }
        _ => None,
    };

    // 回报率是硬门槛，不能用 filter_map 静默丢掉不可估算的测量段。只有
    // 每段都提供了达到政策样本数的有限 p50，才允许生成跨段中位数。
    let polling_hz_p50 = values
        .iter()
        .map(|(record, _)| {
            (record.summary.polling_estimated_report_count >= policy.min_events_per_measurement)
                .then_some(record.summary.polling_hz_p50)
                .flatten()
                .filter(|hz| hz.is_finite())
        })
        .collect::<Option<Vec<_>>>()
        .and_then(median_of);
    match polling_hz_p50 {
        Some(hz) if hz < policy.min_polling_hz => {
            push_unique(&mut reasons, FailReason::PollingBelowFloor);
        }
        Some(_) => {}
        // 缺失或样本不足已由逐段 InsufficientPollingSamples 门覆盖。
        None => {}
    }

    let verdict = if reasons.is_empty() {
        Verdict::Pass
    } else {
        Verdict::Fail
    };
    Analysis {
        counts_per_360_measured: Some(measured),
        deviation_pct,
        repeat_spread_pct,
        left_right_delta_pct,
        fast_slow_delta_pct,
        polling_hz_p50,
        verdict,
        fail_reasons: reasons,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试用 QPC 频率：与 Windows 常见值同量级。
    const QPC_FREQ: i64 = 10_000_000;

    /// 构造均匀事件流：`hz` 回报率持续 `seconds` 秒，累计 `total_counts`。
    fn synth_summary(hz: i64, seconds: i64, total_counts: i64) -> MeasurementSummary {
        let events = hz * seconds;
        let step = QPC_FREQ / hz;
        let base = total_counts / events;
        let remainder = total_counts - base * events;
        let mut acc = Accumulator::new(QPC_FREQ);
        for i in 0..events {
            let dx = base
                + if i < remainder.abs() {
                    remainder.signum()
                } else {
                    0
                };
            acc.push(
                (i + 1) * step,
                i32::try_from(dx).expect("test dx fits i32"),
                0,
            );
        }
        acc.finish()
    }

    fn record(
        direction: Direction,
        speed: Speed,
        turns: f64,
        hz: i64,
        seconds: i64,
        total_counts: i64,
    ) -> MeasurementRecord {
        MeasurementRecord {
            direction,
            speed,
            turns,
            summary: synth_summary(hz, seconds, total_counts),
        }
    }

    fn policy() -> Policy {
        Policy {
            expected_counts_per_360: 18_000.0,
            tolerance_pct: 0.5,
            fast_slow_tolerance_pct: 1.0,
            min_polling_hz: 500.0,
            min_events_per_measurement: 100,
        }
    }

    /// 标准合格组：左右 × 快慢齐、1000Hz、三圈 54000 计数。
    fn passing_records() -> Vec<MeasurementRecord> {
        vec![
            record(Direction::Cw, Speed::Slow, 3.0, 1000, 9, 54_000),
            record(Direction::Cw, Speed::Normal, 3.0, 1000, 9, 54_000),
            record(Direction::Cw, Speed::Fast, 3.0, 1000, 9, 54_000),
            record(Direction::Ccw, Speed::Normal, 3.0, 1000, 9, -54_000),
        ]
    }

    #[test]
    fn exact_measurement_passes() {
        let analysis = analyze(&passing_records(), &policy());
        assert_eq!(
            analysis.verdict,
            Verdict::Pass,
            "reasons: {:?}",
            analysis.fail_reasons
        );
        assert_eq!(analysis.counts_per_360_measured, Some(18_000.0));
        assert_eq!(analysis.deviation_pct, Some(0.0));
        assert_eq!(analysis.repeat_spread_pct, Some(0.0));
        assert_eq!(analysis.left_right_delta_pct, Some(0.0));
        assert_eq!(analysis.fast_slow_delta_pct, Some(0.0));
    }

    #[test]
    fn polling_estimator_reads_stream_rate() {
        let s125 = synth_summary(125, 9, 54_000);
        let s1000 = synth_summary(1000, 9, 54_000);
        let hz125 = s125.polling_hz_p50.expect("has intervals");
        let hz1000 = s1000.polling_hz_p50.expect("has intervals");
        assert!((hz125 - 125.0).abs() < 1.0, "estimated {hz125}");
        assert!((hz1000 - 1000.0).abs() < 8.0, "estimated {hz1000}");
    }

    #[test]
    fn low_polling_fails_floor() {
        let records = vec![
            record(Direction::Cw, Speed::Normal, 3.0, 125, 9, 54_000),
            record(Direction::Ccw, Speed::Normal, 3.0, 125, 9, -54_000),
        ];
        let analysis = analyze(&records, &policy());
        assert_eq!(analysis.verdict, Verdict::Fail);
        assert!(
            analysis
                .fail_reasons
                .contains(&FailReason::PollingBelowFloor)
        );
        // 回报率不达标不应牵连其他判定项。
        assert!(
            !analysis
                .fail_reasons
                .contains(&FailReason::DeviationExceedsTolerance)
        );
    }

    #[test]
    fn missing_polling_in_every_measurement_fails_closed() {
        let mut records = passing_records();
        for record in &mut records {
            record.summary.polling_hz_p50 = None;
            record.summary.polling_estimated_report_count = 0;
        }

        let analysis = analyze(&records, &policy());
        assert_eq!(analysis.verdict, Verdict::Fail);
        assert_eq!(analysis.polling_hz_p50, None);
        assert!(
            analysis
                .fail_reasons
                .contains(&FailReason::InsufficientPollingSamples)
        );
    }

    #[test]
    fn one_high_polling_measurement_cannot_mask_missing_measurements() {
        let mut records = passing_records();
        for record in records.iter_mut().take(3) {
            record.summary.polling_hz_p50 = None;
            record.summary.polling_estimated_report_count = 0;
        }

        let analysis = analyze(&records, &policy());
        assert_eq!(analysis.verdict, Verdict::Fail);
        assert_eq!(analysis.polling_hz_p50, None);
        assert!(
            analysis
                .fail_reasons
                .contains(&FailReason::InsufficientPollingSamples)
        );
    }

    #[test]
    fn polling_estimate_with_too_few_reports_fails_closed() {
        let mut records = passing_records();
        records[0].summary.polling_estimated_report_count = policy().min_events_per_measurement - 1;

        let analysis = analyze(&records, &policy());
        assert_eq!(analysis.verdict, Verdict::Fail);
        assert_eq!(analysis.polling_hz_p50, None);
        assert!(
            analysis
                .fail_reasons
                .contains(&FailReason::InsufficientPollingSamples)
        );
    }

    #[test]
    fn doubled_sensitivity_fails_deviation() {
        // 灵敏度翻倍：转三圈只需一半计数。
        let records = vec![
            record(Direction::Cw, Speed::Normal, 3.0, 1000, 9, 27_000),
            record(Direction::Ccw, Speed::Normal, 3.0, 1000, 9, -27_000),
        ];
        let analysis = analyze(&records, &policy());
        assert_eq!(analysis.verdict, Verdict::Fail);
        assert!(
            analysis
                .fail_reasons
                .contains(&FailReason::DeviationExceedsTolerance)
        );
    }

    #[test]
    fn acceleration_shows_up_as_fast_slow_mismatch() {
        // 模拟加速：快速档每计数角度变大，同样三圈计数减少约 2.2%。
        let records = vec![
            record(Direction::Cw, Speed::Slow, 3.0, 1000, 9, 54_000),
            record(Direction::Cw, Speed::Fast, 3.0, 1000, 9, 52_800),
        ];
        let analysis = analyze(&records, &policy());
        assert_eq!(analysis.verdict, Verdict::Fail);
        assert!(
            analysis
                .fail_reasons
                .contains(&FailReason::FastSlowMismatch)
        );
    }

    #[test]
    fn left_right_asymmetry_detected() {
        let records = vec![
            record(Direction::Cw, Speed::Normal, 3.0, 1000, 9, 54_000),
            record(Direction::Ccw, Speed::Normal, 3.0, 1000, 9, -53_700),
        ];
        let analysis = analyze(&records, &policy());
        assert_eq!(analysis.verdict, Verdict::Fail);
        assert!(
            analysis
                .fail_reasons
                .contains(&FailReason::LeftRightMismatch)
        );
        assert!(
            !analysis
                .fail_reasons
                .contains(&FailReason::RepeatInconsistent)
        );
    }

    #[test]
    fn outlier_repeat_detected() {
        let records = vec![
            record(Direction::Cw, Speed::Normal, 3.0, 1000, 9, 54_000),
            record(Direction::Cw, Speed::Normal, 3.0, 1000, 9, 54_030),
            record(Direction::Cw, Speed::Normal, 3.0, 1000, 9, 55_500),
        ];
        let analysis = analyze(&records, &policy());
        assert_eq!(analysis.verdict, Verdict::Fail);
        assert!(
            analysis
                .fail_reasons
                .contains(&FailReason::RepeatInconsistent)
        );
        assert!(
            !analysis
                .fail_reasons
                .contains(&FailReason::DeviationExceedsTolerance)
        );
    }

    #[test]
    fn direction_mismatch_detected() {
        let records = vec![record(Direction::Ccw, Speed::Normal, 3.0, 1000, 9, 54_000)];
        let analysis = analyze(&records, &policy());
        assert_eq!(analysis.verdict, Verdict::Fail);
        assert!(
            analysis
                .fail_reasons
                .contains(&FailReason::DirectionMismatch)
        );
    }

    #[test]
    fn empty_and_undersized_inputs_fail_closed() {
        let empty = analyze(&[], &policy());
        assert_eq!(empty.verdict, Verdict::Fail);
        assert_eq!(
            empty.fail_reasons,
            vec![FailReason::InsufficientMeasurements]
        );
        assert_eq!(empty.counts_per_360_measured, None);

        let tiny = vec![record(Direction::Cw, Speed::Normal, 3.0, 10, 5, 54_000)];
        let analysis = analyze(&tiny, &policy());
        assert_eq!(analysis.verdict, Verdict::Fail);
        assert!(
            analysis
                .fail_reasons
                .contains(&FailReason::InsufficientEvents)
        );
    }

    #[test]
    fn equal_delivery_qpc_is_legal_but_regression_is_counted() {
        let mut acc = Accumulator::new(QPC_FREQ);
        acc.push(100, 1, 0);
        acc.push(100, 1, 0);
        acc.push(50, 1, 0);
        acc.push(200, 1, 0);
        let summary = acc.finish();
        assert_eq!(summary.non_monotonic_events, 1);
        assert_eq!(summary.event_count, 4);
        assert_eq!(summary.total_dx, 4);
    }

    #[test]
    fn batched_delivery_recovers_five_hundred_hz() {
        let mut acc = Accumulator::new(QPC_FREQ);
        for batch in 1..=250_i64 {
            let qpc = batch * (QPC_FREQ / 125);
            for _ in 0..4 {
                acc.push(qpc, 1, 0);
            }
        }
        let summary = acc.finish();
        assert_eq!(summary.polling_hz_p50, Some(500.0));
        assert_eq!(summary.max_delivery_batch_size, 4);
        assert_eq!(summary.delivery_group_count, 250);
    }

    #[test]
    fn norm_conversions_match_spec() {
        // 一整圈的计数换算恰为 2π；向下的 dy 是负 pitch（向上为正）。
        assert!((yaw_delta_rad(18_000, 18_000.0) - std::f64::consts::TAU).abs() < 1e-12);
        assert!((degrees_per_count(18_000.0) - 0.02).abs() < 1e-12);
        let pitch = pitch_delta_rad(100, 18_000.0, 1.0);
        assert!(pitch < 0.0);
        assert!((pitch.abs() - 100.0 * std::f64::consts::TAU / 18_000.0).abs() < 1e-12);
    }

    /// COD MW II 的目录常数（docs/60 §3，公式推算值）。
    const COD_COUNTS_PER_360: f64 = 6_818.18;

    #[test]
    fn swept_degrees_reads_horizontal_field_of_view() {
        // 画面左缘扫到右缘：半圈的计数即 180°，据此给 fov_axis 分档。
        let half_turn = (COD_COUNTS_PER_360 / 2.0).round() as i64;
        let swept = swept_degrees(half_turn, COD_COUNTS_PER_360).expect("constant is positive");
        assert!((swept - 180.0).abs() < 0.05, "swept {swept}");
        // 带符号：反向扫掠读负值，方向搞错一眼就看出来。
        assert!(swept_degrees(-half_turn, COD_COUNTS_PER_360).expect("some") < 0.0);
    }

    #[test]
    fn swept_degrees_derives_the_vertical_scale_ratio() {
        // 俯仰限位之间扫满 = 180°，读数按 ratio = 1 换算，
        // 所以 vertical_scale_ratio = 180 / |读数|。
        let isotropic = (COD_COUNTS_PER_360 / 2.0).round() as i64;
        let ratio = 180.0
            / swept_degrees(isotropic, COD_COUNTS_PER_360)
                .expect("some")
                .abs();
        assert!((ratio - 1.0).abs() < 0.001, "isotropic ratio {ratio}");

        // 垂直被压到 0.75 倍：同样 180° 要多推 1/0.75 倍计数。
        let squashed = (COD_COUNTS_PER_360 / 2.0 / 0.75).round() as i64;
        let ratio = 180.0
            / swept_degrees(squashed, COD_COUNTS_PER_360)
                .expect("some")
                .abs();
        assert!((ratio - 0.75).abs() < 0.001, "squashed ratio {ratio}");

        // 反向扫（低头→抬头，raw dy 为负）必须给出同一个 ratio。取绝对值
        // 之前它是 -1.0，标注员照着 UI 抄就会把 ratio 填成负数。
        let upward = -isotropic;
        let ratio = 180.0
            / swept_degrees(upward, COD_COUNTS_PER_360)
                .expect("some")
                .abs();
        assert!((ratio - 1.0).abs() < 0.001, "upward sweep ratio {ratio}");
    }

    #[test]
    fn swept_degrees_refuses_to_invent_angles_without_a_constant() {
        // 测绘模式（目录常数未定版 = 0.0）与非法常数一律只报计数。
        assert_eq!(swept_degrees(3_409, 0.0), None);
        assert_eq!(swept_degrees(3_409, -1.0), None);
        assert_eq!(swept_degrees(3_409, f64::NAN), None);
        assert_eq!(swept_degrees(3_409, f64::INFINITY), None);
    }

    #[test]
    fn fail_reasons_serialize_snake_case() {
        let json = serde_json::to_string(&vec![
            FailReason::InsufficientPollingSamples,
            FailReason::PollingBelowFloor,
            FailReason::FastSlowMismatch,
        ])
        .expect("serializes");
        assert_eq!(
            json,
            r#"["insufficient_polling_samples","polling_below_floor","fast_slow_mismatch"]"#
        );
        let verdict = serde_json::to_string(&Verdict::Pass).expect("serializes");
        assert_eq!(verdict, r#""pass""#);
    }
}

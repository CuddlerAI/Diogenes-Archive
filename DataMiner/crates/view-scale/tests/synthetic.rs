//! C1 合成回归（docs/46 §3）：在已知真值的合成相机上闭环跑完整对表流程，
//! 验证测量与验收算法本身是准的，并验证劣化检测器真能抓到目标劣化。
//! 全部无头、确定性，不依赖任何真机窗口。

use view_scale::sim::{CameraModel, OperatorProfile, measured_record};
use view_scale::{
    Analysis, Direction, FailReason, MeasurementRecord, Policy, Speed, Verdict, analyze,
};

const QPC_FREQ: i64 = 10_000_000;
const TRUTH_COUNTS_PER_360: f64 = 18_000.0;

fn policy() -> Policy {
    Policy {
        expected_counts_per_360: TRUTH_COUNTS_PER_360,
        tolerance_pct: 0.5,
        fast_slow_tolerance_pct: 1.0,
        min_polling_hz: 500.0,
        min_events_per_measurement: 100,
    }
}

/// 标准对表矩阵：三档速度 × 左右方向，各三圈；每次测量用独立相机实例
/// （对表关心的是相对旋转，不同测量之间无共享状态）。
fn run_matrix(camera_template: &CameraModel, polling_hz: Option<i64>) -> Vec<MeasurementRecord> {
    let speeds = [
        (Speed::Slow, 3_000.0),
        (Speed::Normal, 9_000.0),
        (Speed::Fast, 30_000.0),
    ];
    let mut records = Vec::new();
    let mut start_qpc = 1_000;
    for (speed, counts_per_s) in speeds {
        for direction in [Direction::Cw, Direction::Ccw] {
            let mut camera = camera_template.clone();
            let operator = OperatorProfile {
                counts_per_s,
                emit_hz: 1_000,
            };
            records.push(measured_record(
                &mut camera,
                operator,
                direction,
                speed,
                3.0,
                QPC_FREQ,
                polling_hz,
                start_qpc,
            ));
            start_qpc += 1_000_000_000;
        }
    }
    records
}

fn analyze_matrix(camera_template: &CameraModel, polling_hz: Option<i64>) -> Analysis {
    analyze(&run_matrix(camera_template, polling_hz), &policy())
}

#[test]
fn clean_pipeline_recovers_truth() {
    let analysis = analyze_matrix(&CameraModel::new(TRUTH_COUNTS_PER_360), None);
    assert_eq!(
        analysis.verdict,
        Verdict::Pass,
        "reasons: {:?}",
        analysis.fail_reasons
    );
    let measured = analysis.counts_per_360_measured.expect("has measurement");
    let deviation = analysis.deviation_pct.expect("has deviation");
    assert!(
        deviation <= 0.1,
        "measured {measured}, deviation {deviation}% exceeds 0.1%"
    );
}

#[test]
fn acceleration_trips_fast_slow_gate() {
    let camera = CameraModel::new(TRUTH_COUNTS_PER_360).with_acceleration(0.002);
    let analysis = analyze_matrix(&camera, None);
    assert_eq!(analysis.verdict, Verdict::Fail);
    assert!(
        analysis
            .fail_reasons
            .contains(&FailReason::FastSlowMismatch),
        "reasons: {:?}",
        analysis.fail_reasons
    );
    let delta = analysis.fast_slow_delta_pct.expect("both speeds measured");
    assert!(delta > 1.0, "fast/slow delta {delta}% should exceed policy");
}

#[test]
fn smoothing_is_scale_neutral_negative_control() {
    // 纯平滑不得触发任何尺度门：稳态增益为 1，闭环回修后计数守恒。
    let camera = CameraModel::new(TRUTH_COUNTS_PER_360).with_smoothing(0.2);
    let analysis = analyze_matrix(&camera, None);
    assert_eq!(
        analysis.verdict,
        Verdict::Pass,
        "reasons: {:?}",
        analysis.fail_reasons
    );
    let deviation = analysis.deviation_pct.expect("has deviation");
    assert!(deviation <= 0.15, "smoothing shifted scale by {deviation}%");
}

#[test]
fn low_polling_detected_without_losing_counts() {
    let analysis = analyze_matrix(&CameraModel::new(TRUTH_COUNTS_PER_360), Some(125));
    assert_eq!(analysis.verdict, Verdict::Fail);
    assert!(
        analysis
            .fail_reasons
            .contains(&FailReason::PollingBelowFloor),
        "reasons: {:?}",
        analysis.fail_reasons
    );
    let hz = analysis.polling_hz_p50.expect("has polling estimate");
    assert!((110.0..=140.0).contains(&hz), "estimated polling {hz}Hz");
    // 合批只粗化时间、不吞计数：尺度门不得误伤。
    assert!(
        !analysis
            .fail_reasons
            .contains(&FailReason::DeviationExceedsTolerance),
        "rebatching must preserve total counts; reasons: {:?}",
        analysis.fail_reasons
    );
    let deviation = analysis.deviation_pct.expect("has deviation");
    assert!(deviation <= 0.1, "deviation {deviation}% after rebatch");
}

#[test]
fn wrong_catalog_constant_detected() {
    // 灵敏度被调成双倍：游戏真值 9000，目录仍期望 18000。
    let analysis = analyze_matrix(&CameraModel::new(9_000.0), None);
    assert_eq!(analysis.verdict, Verdict::Fail);
    assert_eq!(
        analysis.fail_reasons,
        vec![FailReason::DeviationExceedsTolerance],
        "doubled sensitivity should fail exactly the deviation gate"
    );
    let measured = analysis.counts_per_360_measured.expect("has measurement");
    assert!(
        (measured - 9_000.0).abs() / 9_000.0 < 0.001,
        "measured {measured}"
    );
}

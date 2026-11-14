//! 性能认证的四档编排：状态机、降幅计算、可比性判据。
//!
//! [130 号 §3.3](../../../docs/130-depth-capture-inputs-auto-provisioning.md) 的 P3 判定层。
//! 采集员看到的是**一个按钮**；这里是按钮背后那台状态机。
//!
//! # 为什么是状态机而不是四次独立测量
//!
//! 四档数字只有在**同一台机器、同一个 probe、同一份 profile/binding、同一次会话**里
//! 测出来才可比。任何一项中途变了，四档数字放在一起就没有意义——
//! 而这件事从结果 JSON 上完全看不出来。
//!
//! 所以身份在 [`PerfRun::begin`] 时固化一次，之后每一档都对身份复核；
//! 不一致即整轮作废，不是「用新身份继续」。
//!
//! # 为什么必须四档而不是两档
//!
//! 仅水印档先把注入/水印成本拆出来；readback-only 档保留 selected-Present GPU 回读，
//! 但在 writer 前丢弃帧，再把回读成本与 depth 解码/EXR/落盘成本拆开。
//! 最终门限仍只比较基线与完整档；两个中间档只做归因，不会放宽认证门。

use serde::{Deserialize, Serialize};

use crate::integration::{
    MAX_AVG_FPS_DROP, MAX_AVG_FPS_GAIN, MAX_LOW1_FPS_DROP, MAX_LOW1_FPS_GAIN,
};
use crate::probe_control::WriterTimingSummary;

/// 四档。**顺序即枚举顺序**，`begin` 之后必须按序走完。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    /// 同一 probe 的 Present-hook-only 基线：不写水印、不回读深度。
    Baseline,
    /// 注入、写水印、**不回读**（需要 `watermark_only`）。
    WatermarkOnly,
    /// 完整回读，但在 writer 前丢弃；只用于拆分 readback 与编码/落盘开销。
    ReadbackOnly,
    /// 完整采集。
    Full,
}

impl Stage {
    pub const ORDER: [Stage; 4] = [
        Stage::Baseline,
        Stage::WatermarkOnly,
        Stage::ReadbackOnly,
        Stage::Full,
    ];

    #[must_use]
    pub fn next(self) -> Option<Stage> {
        match self {
            Stage::Baseline => Some(Stage::WatermarkOnly),
            Stage::WatermarkOnly => Some(Stage::ReadbackOnly),
            Stage::ReadbackOnly => Some(Stage::Full),
            Stage::Full => None,
        }
    }
}

/// 一档的实测结果。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct StageMeasurement {
    pub stage: Stage,
    pub avg_fps: f64,
    /// 1% low。
    pub low1_fps: f64,
    pub seconds: f64,
    /// 本档是否满足可比性前置条件。
    ///
    /// 字段名为兼容既有 v2 请求保留。selected-present 下视频 CFR 不是游戏
    /// Present 上限：基线只需证明 source 能供满视频车道；注入档的下降正是测量值，
    /// 不能因低于基线/CFR 再判废。
    pub frame_capped: bool,
    /// Probe Flushed 回显的 writer 旁路帧数；只应在 readback-only 档非零。
    #[serde(default)]
    pub writer_bypassed_frames: u64,
    /// Probe encode 环摘要；完整档必须有样本，readback-only 档必须为零样本。
    #[serde(default)]
    pub writer_timing: Option<WriterTimingSummary>,
}

/// 每档最短时长。
///
/// 5 分钟是 #449 T1 的口径。更短的样本会被场景切换与偶发抖动主导——
/// #518 那次 30 秒短测就没能暴露出后来 5 分钟里出现的问题。
pub const MIN_STAGE_SECONDS: f64 = 300.0;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "code")]
pub enum PerfRunError {
    /// 档位顺序不对。
    OutOfOrder { expected: Stage, got: Stage },
    /// 这一档时长不够。
    TooShort { stage: Stage, seconds: f64 },
    /// 本档不满足基线供帧/可比性条件。
    NotFrameCapped { stage: Stage },
    /// 基线 FPS 非正——测量本身坏了，不是"掉帧严重"。
    NonPositiveBaseline { avg_fps: f64, low1_fps: f64 },
    /// 完整档显著快于基线，说明场景/边界抖动已经大过被测开销。
    UnstableComparison {
        avg_fps_drop: f64,
        low1_fps_drop: f64,
    },
    /// 身份在中途变了：整轮作废。
    IdentityChanged { field: &'static str },
    /// 还没测完就要结论。
    Incomplete { missing: Vec<Stage> },
    /// readback-only 没证明 GPU 回读后的帧确实在 writer 前被旁路。
    MissingWriterBypassEvidence,
    /// 完整档没取得 writer 编码/落盘耗时样本。
    MissingWriterTimingEvidence,
}

/// 一轮认证。身份在 `begin` 时固化。
#[derive(Debug, Clone)]
pub struct PerfRun {
    identity: String,
    measurements: Vec<StageMeasurement>,
}

/// 四档跑完之后的结论。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PerfRunOutcome {
    pub avg_fps_drop: f64,
    pub low1_fps_drop: f64,
    /// 水印单独贡献的平均掉帧比例（归因用）。
    pub watermark_avg_drop: f64,
    /// GPU 回读/CPU 映射单独贡献的平均掉帧比例。
    pub readback_avg_drop: f64,
    /// writer 编码与落盘单独贡献的平均掉帧比例。
    pub writer_avg_drop: f64,
    /// 水印单独贡献的 1% low 降幅。
    pub watermark_low1_drop: f64,
    /// GPU 回读/CPU 映射单独贡献的 1% low 降幅。
    pub readback_low1_drop: f64,
    /// writer 编码与落盘单独贡献的 1% low 降幅。
    pub writer_low1_drop: f64,
    pub readback_only_bypassed_frames: u64,
    pub full_writer_timing: WriterTimingSummary,
    pub passes: bool,
}

impl PerfRun {
    #[must_use]
    pub fn begin(identity: impl Into<String>) -> Self {
        Self {
            identity: identity.into(),
            measurements: Vec::new(),
        }
    }

    /// 下一档该测哪个。`None` = 四档都测完了。
    #[must_use]
    pub fn next_stage(&self) -> Option<Stage> {
        Stage::ORDER.get(self.measurements.len()).copied()
    }

    /// 记一档。**identity 必须与 `begin` 时一致**——见模块文档。
    pub fn record(&mut self, identity: &str, m: StageMeasurement) -> Result<(), PerfRunError> {
        if identity != self.identity {
            return Err(PerfRunError::IdentityChanged {
                field: "certification_key",
            });
        }
        let expected = self.next_stage().ok_or(PerfRunError::OutOfOrder {
            expected: Stage::Full,
            got: m.stage,
        })?;
        if m.stage != expected {
            return Err(PerfRunError::OutOfOrder {
                expected,
                got: m.stage,
            });
        }
        if !m.frame_capped {
            return Err(PerfRunError::NotFrameCapped { stage: m.stage });
        }
        if m.seconds < MIN_STAGE_SECONDS {
            return Err(PerfRunError::TooShort {
                stage: m.stage,
                seconds: m.seconds,
            });
        }
        if m.stage == Stage::Baseline && (m.avg_fps <= 0.0 || m.low1_fps <= 0.0) {
            return Err(PerfRunError::NonPositiveBaseline {
                avg_fps: m.avg_fps,
                low1_fps: m.low1_fps,
            });
        }
        if m.stage == Stage::ReadbackOnly
            && (m.writer_bypassed_frames == 0
                || m.writer_timing
                    .is_some_and(|timing| timing.sample_count != 0))
        {
            return Err(PerfRunError::MissingWriterBypassEvidence);
        }
        if m.stage == Stage::Full
            && m.writer_timing
                .is_none_or(|timing| timing.sample_count == 0)
        {
            return Err(PerfRunError::MissingWriterTimingEvidence);
        }
        self.measurements.push(m);
        Ok(())
    }

    /// 四档都测完后算结论。
    ///
    /// 降幅取 `(baseline − stage) / baseline`。**基线更差时降幅为负**，
    /// 照实报——不夹到 0。掉帧为负说明测量噪声大于效应，
    /// 那本身就是「这轮不可信」的信号，抹掉它等于藏起证据。
    pub fn finish(&self) -> Result<PerfRunOutcome, PerfRunError> {
        if self.measurements.len() != Stage::ORDER.len() {
            let have: Vec<Stage> = self.measurements.iter().map(|m| m.stage).collect();
            return Err(PerfRunError::Incomplete {
                missing: Stage::ORDER
                    .iter()
                    .copied()
                    .filter(|s| !have.contains(s))
                    .collect(),
            });
        }
        let base = self.measurements[0];
        let wm = self.measurements[1];
        let readback = self.measurements[2];
        let full = self.measurements[3];
        let drop = |a: f64, b: f64| (a - b) / a;

        let avg_fps_drop = drop(base.avg_fps, full.avg_fps);
        let low1_fps_drop = drop(base.low1_fps, full.low1_fps);
        if avg_fps_drop < -MAX_AVG_FPS_GAIN || low1_fps_drop < -MAX_LOW1_FPS_GAIN {
            return Err(PerfRunError::UnstableComparison {
                avg_fps_drop,
                low1_fps_drop,
            });
        }
        let watermark_avg_drop = drop(base.avg_fps, wm.avg_fps);
        let readback_avg_drop = (wm.avg_fps - readback.avg_fps) / base.avg_fps;
        let writer_avg_drop = (readback.avg_fps - full.avg_fps) / base.avg_fps;
        let watermark_low1_drop = drop(base.low1_fps, wm.low1_fps);
        let readback_low1_drop = (wm.low1_fps - readback.low1_fps) / base.low1_fps;
        let writer_low1_drop = (readback.low1_fps - full.low1_fps) / base.low1_fps;
        let full_writer_timing = full
            .writer_timing
            .ok_or(PerfRunError::MissingWriterTimingEvidence)?;
        Ok(PerfRunOutcome {
            avg_fps_drop,
            low1_fps_drop,
            watermark_avg_drop,
            readback_avg_drop,
            writer_avg_drop,
            watermark_low1_drop,
            readback_low1_drop,
            writer_low1_drop,
            readback_only_bypassed_frames: readback.writer_bypassed_frames,
            full_writer_timing,
            passes: avg_fps_drop <= MAX_AVG_FPS_DROP && low1_fps_drop <= MAX_LOW1_FPS_DROP,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(stage: Stage, avg: f64, low1: f64) -> StageMeasurement {
        StageMeasurement {
            stage,
            avg_fps: avg,
            low1_fps: low1,
            seconds: 300.0,
            frame_capped: true,
            writer_bypassed_frames: u64::from(stage == Stage::ReadbackOnly) * 100,
            writer_timing: (stage == Stage::Full).then_some(WriterTimingSummary {
                sample_count: 100,
                total_committed: 100,
                ..WriterTimingSummary::default()
            }),
        }
    }

    fn run_four(a: [f64; 4], l: [f64; 4]) -> Result<PerfRunOutcome, PerfRunError> {
        let mut r = PerfRun::begin("k");
        for (i, s) in Stage::ORDER.iter().enumerate() {
            r.record("k", m(*s, a[i], l[i]))?;
        }
        r.finish()
    }

    #[test]
    fn the_461_measurements_pass() {
        // #461 原三档实测；新增 readback-only 用水印/完整档之间的无害值覆盖四档公式。
        let o = run_four(
            [59.823, 59.938, 59.900, 59.819],
            [36.283, 37.538, 37.500, 37.424],
        )
        .unwrap();
        assert!(o.passes, "{o:?}");
        assert!(
            o.avg_fps_drop.abs() < 0.001,
            "平均掉帧应≈0，实得 {}",
            o.avg_fps_drop
        );
        assert!(o.low1_fps_drop < 0.0, "1% low 比基线还好，应为负");
    }

    #[test]
    fn the_low1_gate_is_five_percent_while_the_average_gate_stays_at_two() {
        // 2026-09-18 owner 裁决只放宽了 1% low（3% → 5%），平均值门限没动。
        let passes = |avg_full: f64, low1_full: f64| {
            run_four([60.0, 60.0, 60.0, avg_full], [40.0, 40.0, 40.0, low1_full])
                .unwrap()
                .passes
        };
        assert!(passes(60.0, 38.4), "1% low 掉 4% 应通过");
        assert!(!passes(60.0, 37.6), "1% low 掉 6% 应拒绝");
        assert!(!passes(58.2, 40.0), "平均掉 3% 应拒绝");
    }

    #[test]
    fn the_443_measurements_attribute_the_drop() {
        // 四档把原先笼统的「回读后半」再拆成 readback 与 writer。
        let o = run_four([187.33, 161.60, 120.0, 96.05], [150.0, 130.0, 100.0, 80.0]).unwrap();
        assert!(!o.passes);
        assert!(
            (o.watermark_avg_drop - 0.1373).abs() < 0.001,
            "{}",
            o.watermark_avg_drop
        );
        assert!(
            o.readback_avg_drop > 0.2,
            "GPU 回读应有显著开销，实得 {}",
            o.readback_avg_drop
        );
        assert!(
            o.writer_avg_drop > 0.1,
            "writer 应有独立可见开销，实得 {}",
            o.writer_avg_drop
        );
        assert!(
            (o.watermark_avg_drop + o.readback_avg_drop + o.writer_avg_drop - o.avg_fps_drop).abs()
                < 1e-12
        );
        assert!(
            (o.watermark_low1_drop + o.readback_low1_drop + o.writer_low1_drop - o.low1_fps_drop)
                .abs()
                < 1e-12
        );
    }

    #[test]
    fn stages_must_run_in_order() {
        let mut r = PerfRun::begin("k");
        let e = r.record("k", m(Stage::Full, 60.0, 40.0)).unwrap_err();
        assert_eq!(
            e,
            PerfRunError::OutOfOrder {
                expected: Stage::Baseline,
                got: Stage::Full
            }
        );
    }

    #[test]
    fn a_stage_without_comparable_source_rate_is_refused_before_numbers_are_used() {
        let mut r = PerfRun::begin("k");
        let mut bad = m(Stage::Baseline, 187.0, 150.0);
        bad.frame_capped = false;
        assert_eq!(
            r.record("k", bad).unwrap_err(),
            PerfRunError::NotFrameCapped {
                stage: Stage::Baseline
            }
        );
    }

    #[test]
    fn a_short_stage_is_refused() {
        let mut r = PerfRun::begin("k");
        let mut short = m(Stage::Baseline, 60.0, 40.0);
        short.seconds = 30.0;
        assert!(matches!(
            r.record("k", short).unwrap_err(),
            PerfRunError::TooShort { .. }
        ));
    }

    #[test]
    fn an_identity_change_voids_the_whole_run() {
        // 四个数字只有在同一身份下才可比，而这件事从结果 JSON 上完全看不出来。
        let mut r = PerfRun::begin("k");
        r.record("k", m(Stage::Baseline, 60.0, 40.0)).unwrap();
        assert!(matches!(
            r.record("k2", m(Stage::WatermarkOnly, 60.0, 40.0))
                .unwrap_err(),
            PerfRunError::IdentityChanged { .. }
        ));
    }

    #[test]
    fn finishing_early_names_the_missing_stages() {
        let mut r = PerfRun::begin("k");
        r.record("k", m(Stage::Baseline, 60.0, 40.0)).unwrap();
        assert_eq!(
            r.finish().unwrap_err(),
            PerfRunError::Incomplete {
                missing: vec![Stage::WatermarkOnly, Stage::ReadbackOnly, Stage::Full]
            }
        );
    }

    #[test]
    fn a_dead_baseline_is_a_broken_measurement_not_a_huge_drop() {
        // baseline=0 会让降幅变成 inf/NaN，看起来像"掉帧极其严重"。
        // 真因是测量坏了，两者处置完全不同。
        let mut r = PerfRun::begin("k");
        assert!(matches!(
            r.record("k", m(Stage::Baseline, 0.0, 0.0)).unwrap_err(),
            PerfRunError::NonPositiveBaseline { .. }
        ));
    }

    #[test]
    fn a_negative_drop_is_reported_as_is() {
        // 完整档比基线还快 → 降幅为负。不夹到 0：那说明噪声大于效应，
        // 本身就是"这轮不可信"的信号，抹掉等于藏起证据。
        let o = run_four([59.0, 59.5, 59.8, 60.0], [40.0, 40.0, 40.0, 40.0]).unwrap();
        assert!(o.avg_fps_drop < 0.0, "实得 {}", o.avg_fps_drop);
        assert!(o.passes, "门限内的小幅负降幅仍按测量噪声容忍");
    }

    #[test]
    fn a_large_negative_drop_invalidates_the_comparison() {
        let error = run_four(
            [59.717, 59.950, 59.996, 59.988],
            [29.786, 35.736, 38.306, 36.865],
        )
        .unwrap_err();
        assert!(matches!(
            error,
            PerfRunError::UnstableComparison {
                low1_fps_drop,
                ..
            } if low1_fps_drop < -MAX_LOW1_FPS_GAIN
        ));
    }

    #[test]
    fn next_stage_walks_the_order_then_ends() {
        let mut r = PerfRun::begin("k");
        assert_eq!(r.next_stage(), Some(Stage::Baseline));
        r.record("k", m(Stage::Baseline, 60.0, 40.0)).unwrap();
        assert_eq!(r.next_stage(), Some(Stage::WatermarkOnly));
        r.record("k", m(Stage::WatermarkOnly, 60.0, 40.0)).unwrap();
        assert_eq!(r.next_stage(), Some(Stage::ReadbackOnly));
        r.record("k", m(Stage::ReadbackOnly, 60.0, 40.0)).unwrap();
        assert_eq!(r.next_stage(), Some(Stage::Full));
        r.record("k", m(Stage::Full, 60.0, 40.0)).unwrap();
        assert_eq!(r.next_stage(), None);
    }
}

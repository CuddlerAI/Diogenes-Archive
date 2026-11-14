//! 车道帧率假设的运行时闸（#801 修复清单第 2 项）。
//!
//! [`crate::profile`] L290「逐 Present 模式要求游戏锁在车道帧率」与
//! [`crate::cadence`]「游戏恰好跑车道帧率」此前只是注释——CP2077 v6 一单
//! 游戏实跑 ~32 fps，深度节拍与 15 fps 交付网格错开，50% 槽位错位 ±1~2 帧，
//! 直到接收方肉眼看出重影才被发现（#801）。本模块把假设变成逐 Present 的
//! 运行时观测和一份可落盘的裁决。
//!
//! # 判定为什么这么设计
//!
//! - **锚定车道帧率，不锚定 `target_depth_hz`**：配置错误（如 16 Hz）会把
//!   「预期」一起带偏——游戏 32 fps 配 16 Hz 反而显得「锁着」。车道（30/60）
//!   才是交付网格的真源，所以车道号必须由客户端显式传入（StartRun.lane_fps），
//!   probe 自己猜不出来。
//! - **容差 4%**：游戏内限帧器的抖动在 ±1–2%；而 32 vs 30 = +6.7% 必落网。
//!   界于两者之间，留一倍余量。
//! - **持续 1 秒（连续 `lane_fps` 次越界）才落锤**：单帧尖峰是瞬态；
//!   持续跑偏才是「没锁帧」。
//! - **加载卡顿不算跑偏**：单个间隔 > 4× 名义帧间隔视为停顿（加载/切场景），
//!   清空滑窗重新观测，不让一个大空洞污染其后整窗的观测值。代价是长期暂停的
//!   游戏测不出违规——但暂停也不产帧，交付侧的偶数帧全覆盖闸自然兜底。
//! - **裁决单向**（`violated` 置位不回落）：60 s 里哪怕只有 5 s 没锁，那 5 s
//!   的槽位已经错位，整条即不可交付。闸的意义是拦交付，不是描述平均状态。

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

/// 观测帧率相对车道帧率的允许偏差（%）。
pub const RATE_TOLERANCE_PCT: f64 = 4.0;

/// 裁决落盘文件名。probe 停止时写进采集输出目录；`deliver` 读到 `violated`
/// 即拒绝出件（拒绝出证的「证」就落在这里，线下重建也绕不开这个文件）。
pub const VERDICT_FILE_NAME: &str = "present-rate-verdict.json";

pub const VERDICT_SCHEMA: &str = "dm-present-rate-verdict/v1";

/// 判停顿的倍数：单间隔超过名义帧间隔的这个倍数 → 清窗重测。
const STALL_FACTOR: u64 = 4;

/// 滑窗长度（间隔数）= 2×lane_fps ≈ 2 s：短到能跟上帧率变化，长到平掉抖动。
fn window_intervals(lane_fps: u32) -> usize {
    (lane_fps as usize) * 2
}

/// 落盘的裁决。字段刻意少：回答「能不能交付、差多远、何时开始」三问即止。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PresentRateVerdict {
    pub schema: String,
    pub lane_fps: u32,
    pub tolerance_pct: f64,
    pub presents_observed: u64,
    pub violated: bool,
    /// 首次落锤时的 present 计数（1 起）。`None` = 未违规。
    pub first_violation_present: Option<u64>,
    /// 全程最大偏差（%）及彼时的观测帧率。未满过窗则均为 0。
    pub worst_deviation_pct: f64,
    pub observed_fps_at_worst: f64,
}

/// 逐 Present 喂 QPC，滑窗测观测帧率，越界持续 1 s 落锤。
#[derive(Debug)]
pub struct PresentRateGate {
    lane_fps: u32,
    qpc_frequency: u64,
    /// 最近的 present QPC 戳，容量 window_intervals+1。
    window: VecDeque<i64>,
    presents: u64,
    out_of_band_streak: u32,
    violated: bool,
    first_violation_present: Option<u64>,
    worst_deviation_pct: f64,
    observed_fps_at_worst: f64,
}

impl PresentRateGate {
    /// 零值直接拒绝——与 [`crate::cadence::Cadence::new`] 同口径：
    /// 兜底会让「配错了」表现成「跑起来了但判不出」，那是最难查的一类问题。
    pub fn new(lane_fps: u32, qpc_frequency: u64) -> Result<Self, String> {
        if lane_fps == 0 {
            return Err("lane_fps 为 0，帧率闸无法建立".into());
        }
        if qpc_frequency == 0 {
            return Err("QPC 频率为 0，帧率闸无法建立".into());
        }
        Ok(Self {
            lane_fps,
            qpc_frequency,
            window: VecDeque::with_capacity(window_intervals(lane_fps) + 1),
            presents: 0,
            out_of_band_streak: 0,
            violated: false,
            first_violation_present: None,
            worst_deviation_pct: 0.0,
            observed_fps_at_worst: 0.0,
        })
    }

    pub fn on_present(&mut self, qpc: i64) {
        self.presents += 1;

        if let Some(&last) = self.window.back() {
            let interval = qpc.saturating_sub(last);
            let nominal = (self.qpc_frequency / u64::from(self.lane_fps)).max(1) as i64;
            // 时间倒流（QPC 异常）或停顿：这段观测不可信，清窗重测。
            if interval <= 0 || interval as u64 > STALL_FACTOR * nominal as u64 {
                self.window.clear();
                self.out_of_band_streak = 0;
            }
        }

        // 先腾位再放入：len 永不超过 with_capacity 的预留量，保证热路径上
        // （Present 钩子里）永远不会发生哪怕一次堆扩容。
        let cap = window_intervals(self.lane_fps) + 1;
        if self.window.len() == cap {
            self.window.pop_front();
        }
        self.window.push_back(qpc);
        if self.window.len() < cap {
            return; // 窗没满，不评估——宁可少判，不可用短窗噪声判。
        }

        let span = self.window.back().unwrap() - self.window.front().unwrap();
        if span <= 0 {
            self.window.clear();
            self.out_of_band_streak = 0;
            return;
        }
        let intervals = (self.window.len() - 1) as f64;
        let observed_fps = intervals * self.qpc_frequency as f64 / span as f64;
        let deviation_pct =
            (observed_fps - f64::from(self.lane_fps)).abs() / f64::from(self.lane_fps) * 100.0;

        if deviation_pct > self.worst_deviation_pct {
            self.worst_deviation_pct = deviation_pct;
            self.observed_fps_at_worst = observed_fps;
        }

        if deviation_pct > RATE_TOLERANCE_PCT {
            self.out_of_band_streak += 1;
            if !self.violated && self.out_of_band_streak >= self.lane_fps {
                self.violated = true;
                self.first_violation_present = Some(self.presents);
            }
        } else {
            self.out_of_band_streak = 0;
        }
    }

    #[must_use]
    pub fn violated(&self) -> bool {
        self.violated
    }

    #[must_use]
    pub fn verdict(&self) -> PresentRateVerdict {
        PresentRateVerdict {
            schema: VERDICT_SCHEMA.into(),
            lane_fps: self.lane_fps,
            tolerance_pct: RATE_TOLERANCE_PCT,
            presents_observed: self.presents,
            violated: self.violated,
            first_violation_present: self.first_violation_present,
            worst_deviation_pct: self.worst_deviation_pct,
            observed_fps_at_worst: self.observed_fps_at_worst,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FREQ: u64 = 10_000_000; // 真机常见 QPC 频率
    const LANE: u32 = 30;

    /// 以固定 fps 连喂 n 个 present，附可选的确定性抖动（±jitter_ticks 交替）。
    fn feed(gate: &mut PresentRateGate, fps: f64, n: usize, jitter_ticks: i64, start: i64) -> i64 {
        let step = (FREQ as f64 / fps) as i64;
        let mut t = start;
        for i in 0..n {
            let j = if i % 2 == 0 {
                jitter_ticks
            } else {
                -jitter_ticks
            };
            gate.on_present(t + j);
            t += step;
        }
        t
    }

    #[test]
    fn zero_inputs_are_rejected() {
        assert!(PresentRateGate::new(0, FREQ).is_err());
        assert!(PresentRateGate::new(LANE, 0).is_err());
    }

    #[test]
    fn locked_lane_rate_with_jitter_never_violates() {
        let mut g = PresentRateGate::new(LANE, FREQ).unwrap();
        // ±0.5 ms 抖动：远超真机帧间抖动，仍不该误杀。
        feed(&mut g, 30.0, 600, 5_000, 0);
        assert!(!g.violated(), "{:?}", g.verdict());
        assert!(g.verdict().worst_deviation_pct < RATE_TOLERANCE_PCT);
    }

    #[test]
    fn thirty_two_fps_violates_within_two_seconds_of_full_window() {
        // v6/CP2077 事故的实际形态：车道 30，游戏实跑 32。
        let mut g = PresentRateGate::new(LANE, FREQ).unwrap();
        feed(&mut g, 32.0, 200, 0, 0);
        let v = g.verdict();
        assert!(v.violated);
        // 窗满（61）+ 持续 30 → 91 个 present 内必落锤。
        assert!(v.first_violation_present.unwrap() <= 91, "{v:?}");
        assert!((v.observed_fps_at_worst - 32.0).abs() < 0.5, "{v:?}");
        assert!(v.worst_deviation_pct > 6.0);
    }

    #[test]
    fn within_tolerance_rate_passes() {
        // −3%（29.1 fps）在容差内：限帧器就是这个量级的偏差。
        let mut g = PresentRateGate::new(LANE, FREQ).unwrap();
        feed(&mut g, 29.1, 600, 0, 0);
        assert!(!g.violated(), "{:?}", g.verdict());
    }

    #[test]
    fn loading_stall_does_not_violate() {
        // 锁 30 → 一次 500 ms 加载停顿 → 继续锁 30：停顿清窗，不落锤。
        let mut g = PresentRateGate::new(LANE, FREQ).unwrap();
        let t = feed(&mut g, 30.0, 120, 0, 0);
        let resume = t + (FREQ / 2) as i64; // 500 ms 空洞
        feed(&mut g, 30.0, 120, 0, resume);
        assert!(!g.violated(), "{:?}", g.verdict());
    }

    #[test]
    fn half_rate_violates() {
        // 车道 60 的游戏被跑成 30（挡在 vsync 半率上）：偏差 50%，必落锤。
        let mut g = PresentRateGate::new(60, FREQ).unwrap();
        feed(&mut g, 30.0, 400, 0, 0);
        assert!(g.violated(), "{:?}", g.verdict());
    }

    #[test]
    fn violation_latches_even_after_rate_recovers() {
        let mut g = PresentRateGate::new(LANE, FREQ).unwrap();
        let t = feed(&mut g, 32.0, 200, 0, 0);
        assert!(g.violated());
        feed(&mut g, 30.0, 300, 0, t);
        assert!(g.violated(), "违规单向：恢复锁帧不能洗掉已错位的槽位");
        assert!(g.verdict().first_violation_present.is_some());
    }

    #[test]
    fn verdict_roundtrips_through_json() {
        let mut g = PresentRateGate::new(LANE, FREQ).unwrap();
        feed(&mut g, 32.0, 200, 0, 0);
        let v = g.verdict();
        let s = serde_json::to_string(&v).unwrap();
        let back: PresentRateVerdict = serde_json::from_str(&s).unwrap();
        assert_eq!(v, back);
        assert_eq!(back.schema, VERDICT_SCHEMA);
    }
}

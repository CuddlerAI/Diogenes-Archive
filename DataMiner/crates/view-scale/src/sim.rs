//! 合成实验基建（docs/46 §3 / C1）：已知真值的相机模型、输入劣化模拟器
//! 与闭环"转圈对表"操作者。capture-lab 的 `--view-scale` / `--simulate-*`
//! 开关与 CI 合成回归共用此模块。
//!
//! 物理口径（实现期修正，已回写 docs/46 §3）：
//! - 游戏内**加速**让每计数角度随输入速率增长，造成快慢档 counts_per_360
//!   系统性分离——它才是 `FastSlowMismatch` 的真实靶子；
//! - 纯**平滑**（EMA 滤波）稳态增益为 1，闭环对表下不改变尺度，本模块把它
//!   用作阴性对照；它伤害的是输入→画面延迟，由训练管道的光流 QA（C7）捕获。
//!
//! 模块完全确定性：无随机数、无时钟，事件流可复现。

use crate::{Accumulator, Direction, MeasurementRecord, Speed};

/// 一条模拟 Raw Input 事件。
#[derive(Debug, Clone, Copy)]
pub struct SimEvent {
    pub qpc: i64,
    pub dx: i32,
    pub dy: i32,
}

/// 已知真值的合成相机。
#[derive(Debug, Clone)]
pub struct CameraModel {
    /// 真值：转满 360° 需要的计数。
    pub counts_per_360: f64,
    /// 加速度增益：每计数角度乘 `1 + accel_gain × (输入速率 / 1000 计数每秒)`；0 = 关。
    pub accel_gain: f64,
    /// EMA 平滑系数 α ∈ (0, 1]；None = 相机即时跟随输入。
    pub smoothing_alpha: Option<f64>,
    target_deg: f64,
    yaw_deg: f64,
}

impl CameraModel {
    pub fn new(counts_per_360: f64) -> Self {
        Self {
            counts_per_360,
            accel_gain: 0.0,
            smoothing_alpha: None,
            target_deg: 0.0,
            yaw_deg: 0.0,
        }
    }

    pub fn with_acceleration(mut self, gain: f64) -> Self {
        self.accel_gain = gain;
        self
    }

    pub fn with_smoothing(mut self, alpha: f64) -> Self {
        self.smoothing_alpha = Some(alpha);
        self
    }

    /// 当前实际相机水平角（度）。
    pub fn yaw_deg(&self) -> f64 {
        self.yaw_deg
    }

    /// 摄入一包计数并推进一步滤波。`rate_counts_per_s` 为当前输入速率。
    pub fn ingest(&mut self, dx: i32, rate_counts_per_s: f64) {
        let gain = 1.0 + self.accel_gain * (rate_counts_per_s / 1000.0);
        self.target_deg += f64::from(dx) * (360.0 / self.counts_per_360) * gain;
        self.step();
    }

    /// 按真实事件时间摄入（capture-lab 接真实鼠标时用）：由相邻事件间隔
    /// 推出瞬时速率供加速模型使用。`dt_seconds <= 0`（首个事件或时钟
    /// 异常）按零速率处理，即增益恒为 1。
    pub fn ingest_timed(&mut self, dx: i32, dt_seconds: f64) {
        let rate = if dt_seconds > 0.0 {
            f64::from(dx.unsigned_abs()) / dt_seconds
        } else {
            0.0
        };
        self.ingest(dx, rate);
    }

    fn step(&mut self) {
        match self.smoothing_alpha {
            Some(alpha) => self.yaw_deg += alpha * (self.target_deg - self.yaw_deg),
            None => self.yaw_deg = self.target_deg,
        }
    }

    /// 输入停止后让滤波收敛（模拟松手后画面走完剩余惯性）。
    pub fn settle(&mut self) {
        for _ in 0..4096 {
            self.step();
        }
    }
}

/// 模拟操作者的手速与事件发射频率（等价于满血回报率）。
#[derive(Debug, Clone, Copy)]
pub struct OperatorProfile {
    /// 主转动阶段的输入速率（计数/秒）。
    pub counts_per_s: f64,
    /// 事件发射频率（事件/秒）。
    pub emit_hz: i64,
}

fn reached(direction: Direction, yaw_deg: f64, target_deg: f64) -> bool {
    match direction {
        Direction::Cw => yaw_deg >= target_deg,
        Direction::Ccw => yaw_deg <= target_deg,
    }
}

/// 闭环模拟一次"对准参照物、转 `turns` 整圈、视觉回修对齐"的测量，
/// 返回操作者发出的原始事件流（可再经 [`rebatch`] 降回报率）。
///
/// 主阶段匀速甩到相机视觉到位；若滤波尾部使画面越过参照物，操作者以
/// 慢速逐计数反向修正，直到误差小于半个计数对应的角度——与真人按重影
/// 对齐的行为同构。
pub fn simulate_measurement(
    camera: &mut CameraModel,
    operator: OperatorProfile,
    direction: Direction,
    turns: f64,
    qpc_frequency: i64,
    start_qpc: i64,
) -> Vec<SimEvent> {
    let sign = match direction {
        Direction::Cw => 1.0,
        Direction::Ccw => -1.0,
    };
    let target_deg = camera.yaw_deg() + sign * turns * 360.0;
    let step_qpc = qpc_frequency / operator.emit_hz;
    let per_event = sign * operator.counts_per_s / operator.emit_hz as f64;

    let mut events = Vec::new();
    let mut qpc = start_qpc;
    let mut carry = 0.0_f64;
    let mut guard = 0u32;
    while !reached(direction, camera.yaw_deg(), target_deg) {
        guard += 1;
        assert!(guard < 5_000_000, "simulated turn does not converge");
        carry += per_event;
        let dx = carry.trunc() as i32;
        carry -= f64::from(dx);
        qpc += step_qpc;
        camera.ingest(dx, operator.counts_per_s);
        if dx != 0 {
            events.push(SimEvent { qpc, dx, dy: 0 });
        }
    }
    camera.settle();

    // 视觉回修：以慢速（125 计数/秒）逐计数修正滤波尾部造成的过冲。
    let half_count_deg = 360.0 / camera.counts_per_360 / 2.0;
    let correction_step_qpc = qpc_frequency / 125;
    let mut guard = 0u32;
    loop {
        let overshoot = camera.yaw_deg() - target_deg;
        if overshoot.abs() <= half_count_deg {
            break;
        }
        guard += 1;
        assert!(guard < 100_000, "alignment correction does not converge");
        let dx = if overshoot > 0.0 { -1 } else { 1 };
        qpc += correction_step_qpc;
        camera.ingest(dx, 125.0);
        camera.settle();
        events.push(SimEvent { qpc, dx, dy: 0 });
    }
    events
}

fn push_batched(out: &mut Vec<SimEvent>, qpc: i64, dx: i64, dy: i64) {
    if dx != 0 || dy != 0 {
        out.push(SimEvent {
            qpc,
            dx: i32::try_from(dx).expect("batched dx fits i32"),
            dy: i32::try_from(dy).expect("batched dy fits i32"),
        });
    }
}

/// 把满血事件流合批成低回报率包裹（模拟 125Hz 办公鼠标：8ms 一包）。
/// 计数总量守恒——降回报率只粗化时间，不吞移动量。
pub fn rebatch(events: &[SimEvent], qpc_frequency: i64, polling_hz: i64) -> Vec<SimEvent> {
    let period = qpc_frequency / polling_hz;
    let mut out: Vec<SimEvent> = Vec::new();
    let mut bucket_end: Option<i64> = None;
    let (mut dx, mut dy) = (0_i64, 0_i64);
    for event in events {
        if let Some(end) = bucket_end {
            if event.qpc >= end {
                push_batched(&mut out, end, dx, dy);
                let mut next_end = end;
                while event.qpc >= next_end {
                    next_end += period;
                }
                bucket_end = Some(next_end);
                dx = 0;
                dy = 0;
            }
        } else {
            bucket_end = Some(event.qpc + period);
        }
        dx += i64::from(event.dx);
        dy += i64::from(event.dy);
    }
    if let Some(end) = bucket_end {
        push_batched(&mut out, end, dx, dy);
    }
    out
}

/// 跑一次完整测量并汇总成验收记录。`polling_hz` 为 None 时保持满血事件流。
/// 测试/实验辅助入口，参数即实验矩阵的一行，不做成 builder。
#[allow(clippy::too_many_arguments)]
pub fn measured_record(
    camera: &mut CameraModel,
    operator: OperatorProfile,
    direction: Direction,
    speed: Speed,
    turns: f64,
    qpc_frequency: i64,
    polling_hz: Option<i64>,
    start_qpc: i64,
) -> MeasurementRecord {
    let events = simulate_measurement(camera, operator, direction, turns, qpc_frequency, start_qpc);
    let events = match polling_hz {
        Some(hz) => rebatch(&events, qpc_frequency, hz),
        None => events,
    };
    let mut accumulator = Accumulator::new(qpc_frequency);
    for event in &events {
        accumulator.push(event.qpc, event.dx, event.dy);
    }
    MeasurementRecord {
        direction,
        speed,
        turns,
        summary: accumulator.finish(),
    }
}

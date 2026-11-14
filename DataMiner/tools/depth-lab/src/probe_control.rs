//! Versioned, fail-closed lifecycle contract between the DataMiner client and
//! the injected depth probe.
//!
//! The transport is a replace-in-place command file plus immutable ACK files.
//! Writing a command is never an acknowledgement: only a validated ACK can
//! advance the client state machine. ACK files live below the run output so a
//! crash leaves an auditable record and two runs cannot consume each other's
//! replies.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::StreamIdentity;

pub const CONTROL_SCHEMA_V3: &str = "depth-probe-control/v3";
pub const ACK_SCHEMA_V3: &str = "depth-probe-ack/v3";
pub const CONTROL_RELATIVE_PATH: &str = r"DataMiner\depth-lab\probe-control.json";
pub const ACK_RELATIVE_DIR: &str = "probe-control/acks";

/// The bound CP2077 camera first admits 120 distinct Presents. Its startup
/// budget must also allow that history to accumulate below 12 Hz; this does
/// not relax the camera admission or the subsequent stable-Present checks.
pub fn capture_warmup_timeout(binding_schema: &str) -> Duration {
    Duration::from_secs(if binding_schema == crate::CAMERA_BINDING_SCHEMA_V3 {
        30
    } else {
        10
    })
}

/// Leave the probe time to publish its own warmup fault and cleanup evidence
/// before the client abandons Ready. Preserve a longer configured wait.
pub fn client_ready_timeout_ms(configured_ms: u64, binding_schema: &str) -> u64 {
    configured_ms.max(capture_warmup_timeout(binding_schema).as_millis() as u64 + 5_000)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlEnvelope {
    pub schema: String,
    pub command_id: String,
    pub identity: StreamIdentity,
    pub command: ControlCommand,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
// StartRun intentionally carries the immutable run contract while StopRun stays tiny. This is a
// low-frequency lifecycle file protocol, not a hot-path queue; boxing would only complicate it.
#[allow(clippy::large_enum_variant)]
pub enum ControlCommand {
    StartRun(StartRun),
    StopRun(StopRun),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartRun {
    pub target_process_id: u32,
    pub output_dir: String,
    pub unbind_from_end: usize,
    pub max_frames: Option<u64>,
    pub target_depth_hz: f64,
    /// 车道标称帧率（30/60）。present_rate 闸用它判「游戏是否锁在车道帧率」——
    /// **不能**从 `target_depth_hz` 推：配置一旦错（如 16），预期会跟着错，
    /// 游戏 32 fps 反而显得「锁着」（#801 的事故形态）。
    ///
    /// `#[serde(default)]`：旧客户端不发这个字段，probe 收到 `None` 即停用
    /// 帧率闸并留一行日志，协议版本不必因此上抬（先例见 `watermark_only`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lane_fps: Option<u32>,
    /// A signed device-profile reuse grant requires this live cadence check before Ready.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required_present_fps: Option<u32>,
    pub depth_profile_path: String,
    pub depth_profile_sha256: String,
    pub camera_binding_path: String,
    pub camera_binding_sha256: String,
    /// Diagnostic-only pass/draw identity trace. This remains off for normal capture runs.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pass_identity_trace: bool,
    /// Per-Present A3 timing. This is an explicit run contract, never inherited
    /// from the client, Steam, or game process environment.
    pub performance_metrics_enabled: bool,
    /// This run is permanently non-deliverable and may retain otherwise valid
    /// same-Present projection evidence outside the certified profile family.
    pub performance_validation_only: bool,
    /// Performance-certification baseline: keep the Present hook and A3 QPC
    /// timing, but do not write the watermark or materialize/read back depth.
    /// This must imply `watermark_only` so the depth path remains bypassed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub present_only: bool,
    /// **仅水印档**：写水印但不回读深度（性能认证三档里的中间档）。
    ///
    /// `#[serde(default)]`：旧客户端不发这个字段，probe 收到即视为 false，
    /// 协议版本不必因此上抬。
    #[serde(default)]
    pub watermark_only: bool,
    /// 性能 A/B 诊断档：保留 selected-Present、GPU 回读和 CPU 映射，但在进入
    /// EXR writer 前丢弃帧。仅允许 performance-validation-only，永不用于交付。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub writer_bypass: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_present_feedback: Option<SelectedPresentFeedbackBootstrap>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectedPresentFeedbackBootstrap {
    pub client_process_id: u32,
    pub nonce: String,
    pub resource_epoch: u64,
    pub queue_capacity: u32,
    pub ring_capacity: u32,
    pub max_feedback_lag_presents: u32,
    pub guard_slots: u32,
    pub width: u32,
    pub height: u32,
    pub bytes_per_pixel: u32,
    pub vram_limit_bytes: u64,
    #[serde(default = "default_selected_present_vram_reserve")]
    pub vram_reserve_bytes: u64,
}

const fn default_selected_present_vram_reserve() -> u64 {
    256 * 1024 * 1024
}

impl SelectedPresentFeedbackBootstrap {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.client_process_id == 0
            || self.resource_epoch == 0
            || self.queue_capacity == 0
            || self.queue_capacity > 4096
            || self.vram_reserve_bytes == 0
        {
            return Err(ProtocolError::Contract(
                "selected-Present bootstrap process/epoch/queue values are invalid".into(),
            ));
        }
        if self.nonce.is_empty()
            || self.nonce.len() > 64
            || !self
                .nonce
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return Err(ProtocolError::Contract(
                "selected-Present transport nonce is invalid".into(),
            ));
        }
        present_identity::feedback::RingBudget {
            capacity: self.ring_capacity,
            max_feedback_lag_presents: self.max_feedback_lag_presents,
            guard_slots: self.guard_slots,
            width: self.width,
            height: self.height,
            bytes_per_pixel: self.bytes_per_pixel,
            vram_limit_bytes: self.vram_limit_bytes,
        }
        .validate()
        .map_err(|error| ProtocolError::Contract(error.to_string()))?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StopRun {
    pub cutoff_qpc: i64,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DepthProfileIdentity {
    pub schema_version: String,
    pub game_id: String,
    pub lane: String,
    pub executable_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CameraBindingIdentity {
    pub schema: String,
    pub binding_id: String,
    pub binding_version: String,
    pub profile_id: String,
    pub profile_version: String,
    pub executable_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FlushSummary {
    pub writer_queue_depth: u64,
    pub last_present_seq: u64,
    pub cutoff_present_seq: u64,
    pub depth_row_count: u64,
    pub depth_file_count: u64,
    pub camera_pose_row_count: u64,
    pub camera_diagnostic_row_count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub player_pose_row_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_present: Option<SelectedPresentFlushSummary>,
    /// StartRun 的 writer A/B 旁路状态，由 Flushed 回显供客户端核对。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub writer_bypass: bool,
    /// 已完成 GPU 回读、但按旁路契约未进入 writer 的帧数。
    #[serde(default)]
    pub writer_bypassed_frames: u64,
    /// writer 编码/落盘耗时摘要；原始逐帧样本仍落在 perf-encode.jsonl。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub writer_timing: Option<WriterTimingSummary>,
    /// Actual probe state used for this generation. The client requires this
    /// to match StartRun before accepting Flushed evidence.
    pub performance_metrics_enabled: bool,
    /// Present cadence measured by the injected hook. This is the only FPS
    /// fact accepted by performance certification; WGC callback cadence is
    /// content-driven and changes when the watermark changes the backbuffer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub present_timing: Option<PresentTimingSummary>,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectedPresentFlushSummary {
    pub generation: u64,
    pub resource_epoch: u64,
    /// Exact boundary ACKed only after the first GPU candidate materialized.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_candidate_present_seq: Option<u64>,
    pub materialized_candidates: u64,
    pub selected_frame_count: u64,
    pub distinct_selected_depth_count: u64,
    pub source_identity_alias_count: u64,
    pub readback_submitted_count: u64,
    pub readback_completed_count: u64,
    pub unresolved_candidates: u64,
    pub ring_overwrite_count: u64,
    pub max_unresolved_candidates: u32,
    pub max_feedback_lag_presents: u64,
    pub feedback_latency: SelectedPresentLatencySummary,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu_allocation: Option<CandidateGpuAllocationSummary>,
    pub feedback_queue_depth: u64,
    pub ack_queue_depth: u64,
    pub feedback_queue_max_depth: u64,
    pub ack_queue_max_depth: u64,
    pub feedback_queue_full_count: u64,
    pub ack_queue_full_count: u64,
    pub transport_fault_reason: u32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateGpuAllocationSummary {
    pub capacity: u32,
    pub texture_bytes: u64,
    pub estimated_bytes: u64,
    pub configured_limit_bytes: u64,
    pub configured_reserve_bytes: u64,
    pub budget_before: u64,
    pub usage_before: u64,
    pub headroom_before: u64,
    pub budget_after: u64,
    pub usage_after: u64,
    pub headroom_after: u64,
    pub actual_usage_delta: u64,
    /// Run-end process budget snapshot. This is intentionally separate from the attributable
    /// post-priming allocation delta because the game may stream unrelated resources during the
    /// five-minute performance route.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_at_flush: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_at_flush: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headroom_at_flush: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LatencyQuantiles {
    pub p50: u64,
    pub p95: u64,
    pub p99: u64,
    pub max: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WriterTimingSummary {
    /// 当前有界环里参与分位数计算的样本数。
    pub sample_count: u64,
    /// 本档累计写入 encode 环的样本数，可能大于环容量。
    pub total_committed: u64,
    pub wrapped: bool,
    /// 超过 30 FPS 单帧预算（33.3ms）的样本数；按当前环保留窗口统计。
    pub over_budget_count: u64,
    pub duration_us: LatencyQuantiles,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PresentIntervalEvidence {
    pub start_present_seq: u64,
    pub end_present_seq: u64,
    pub start_qpc: i64,
    pub end_qpc: i64,
    pub duration_qpc: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PresentTimingSummary {
    /// Number of retained Present records in the certified measurement window.
    pub sample_count: u64,
    pub qpc_frequency: u64,
    pub qpc_start: i64,
    pub qpc_end: i64,
    /// Probe-scheduled certified window. It is fixed when Ready is emitted,
    /// independently of the later UI pause and StopRun teardown boundary.
    pub measurement_window_start_qpc: i64,
    pub measurement_cutoff_qpc: i64,
    /// Actual StopRun cutoff. A completed certification keeps this after the
    /// measurement cutoff so teardown cannot contaminate the result.
    pub stop_cutoff_qpc: i64,
    /// Integer milli-FPS keeps the lifecycle protocol total-orderable and
    /// avoids non-finite JSON values while retaining ample gate precision.
    pub avg_millifps: u64,
    pub low1_millifps: u64,
    /// Exact intervals participating in the slowest 1% tail, ordered slowest
    /// first. Persisting their QPC/Present identity makes boundary hitches
    /// distinguishable from stalls in the middle of the certified window.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub slowest_one_percent: Vec<PresentIntervalEvidence>,
    /// A wrapped timing ring is diagnostic-only: an incomplete five-minute
    /// window must never certify a device.
    pub wrapped: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectedPresentLatencySummary {
    pub sample_count: u64,
    pub selected_lag_presents: LatencyQuantiles,
    /// Samples contributing to `wgc_delivery_us`. Source-ahead samples are
    /// excluded from that distribution and counted separately.
    #[serde(default)]
    pub wgc_delivery_sample_count: u64,
    pub wgc_delivery_us: LatencyQuantiles,
    #[serde(default)]
    pub wgc_source_ahead_callback_ready_count: u64,
    #[serde(default)]
    pub wgc_source_ahead_callback_ready_us: LatencyQuantiles,
    pub callback_to_decision_us: LatencyQuantiles,
    /// Samples contributing to `source_to_probe_us`. The WGC clock can lead
    /// even the probe read, and those samples are counted separately rather
    /// than folded in as a wrapped or clamped delay.
    #[serde(default)]
    pub source_to_probe_sample_count: u64,
    pub source_to_probe_us: LatencyQuantiles,
    #[serde(default)]
    pub wgc_source_ahead_process_count: u64,
    #[serde(default)]
    pub wgc_source_ahead_process_us: LatencyQuantiles,
    pub decision_to_probe_us: LatencyQuantiles,
}

impl LatencyQuantiles {
    fn validate(self, name: &str) -> Result<(), String> {
        if self.p50 > self.p95 || self.p95 > self.p99 || self.p99 > self.max {
            return Err(format!(
                "{name} quantiles are not monotonic: p50={} p95={} p99={} max={}",
                self.p50, self.p95, self.p99, self.max
            ));
        }
        Ok(())
    }
}

impl WriterTimingSummary {
    fn validate(self) -> Result<(), String> {
        self.duration_us.validate("writer duration_us")?;
        if self.sample_count > self.total_committed {
            return Err("writer timing sample_count exceeds total_committed".into());
        }
        if self.over_budget_count > self.sample_count {
            return Err("writer timing over_budget_count exceeds sample_count".into());
        }
        if self.sample_count == 0 && self.duration_us != LatencyQuantiles::default() {
            return Err("writer timing quantiles exist without samples".into());
        }
        Ok(())
    }
}

impl PresentTimingSummary {
    fn validate(&self) -> Result<(), String> {
        if self.sample_count < 2 {
            return Err("present timing needs at least two samples".into());
        }
        if self.qpc_frequency == 0 || self.qpc_start <= 0 || self.qpc_end <= self.qpc_start {
            return Err("present timing QPC range is invalid".into());
        }
        if self.avg_millifps == 0 || self.low1_millifps == 0 {
            return Err("present timing FPS summaries must be positive".into());
        }
        if self.measurement_window_start_qpc <= 0
            || self.measurement_cutoff_qpc <= self.measurement_window_start_qpc
            || self.stop_cutoff_qpc <= 0
        {
            return Err("present timing measurement/stop cutoffs are invalid".into());
        }
        if self.qpc_end > self.stop_cutoff_qpc {
            return Err(format!(
                "present timing qpc_end {} exceeds stop cutoff {}",
                self.qpc_end, self.stop_cutoff_qpc
            ));
        }
        if self.stop_cutoff_qpc >= self.measurement_cutoff_qpc
            && self.qpc_end < self.measurement_cutoff_qpc
        {
            return Err(format!(
                "present timing qpc_end {} did not reach measurement cutoff {}",
                self.qpc_end, self.measurement_cutoff_qpc
            ));
        }
        if self.stop_cutoff_qpc >= self.measurement_cutoff_qpc
            && self.qpc_start < self.measurement_window_start_qpc
        {
            return Err(format!(
                "present timing qpc_start {} precedes scheduled window start {}",
                self.qpc_start, self.measurement_window_start_qpc
            ));
        }
        let expected_avg = u128::from(self.sample_count - 1)
            .checked_mul(u128::from(self.qpc_frequency))
            .and_then(|value| value.checked_mul(1_000))
            .ok_or_else(|| "present timing average calculation overflowed".to_string())?
            / u128::try_from(self.qpc_end - self.qpc_start)
                .map_err(|_| "present timing QPC duration is negative".to_string())?;
        if expected_avg != u128::from(self.avg_millifps) {
            return Err(format!(
                "present timing average does not match QPC evidence: expected {expected_avg}, got {}",
                self.avg_millifps
            ));
        }
        if self.low1_millifps > self.avg_millifps {
            return Err("present timing 1% low exceeds average FPS".into());
        }
        let interval_count = self.sample_count - 1;
        let expected_slow_count = interval_count.div_ceil(100);
        if u64::try_from(self.slowest_one_percent.len()).unwrap_or(u64::MAX) != expected_slow_count
        {
            return Err(format!(
                "present timing slowest-one-percent evidence count mismatch: expected {expected_slow_count}, got {}",
                self.slowest_one_percent.len()
            ));
        }
        let mut previous_duration = u64::MAX;
        for interval in &self.slowest_one_percent {
            if interval.start_present_seq >= interval.end_present_seq
                || interval.start_qpc < self.qpc_start
                || interval.end_qpc > self.qpc_end
                || interval.end_qpc <= interval.start_qpc
                || u64::try_from(interval.end_qpc - interval.start_qpc).ok()
                    != Some(interval.duration_qpc)
            {
                return Err("present timing slow interval evidence is invalid".into());
            }
            if interval.duration_qpc > previous_duration {
                return Err("present timing slow intervals are not ordered slowest first".into());
            }
            previous_duration = interval.duration_qpc;
        }
        let whole = usize::try_from(interval_count / 100)
            .map_err(|_| "present timing interval count does not fit usize".to_string())?;
        let remainder = interval_count % 100;
        let mut slow_duration_scaled = self.slowest_one_percent[..whole]
            .iter()
            .try_fold(0u128, |total, interval| {
                total.checked_add(u128::from(interval.duration_qpc).checked_mul(100)?)
            })
            .ok_or_else(|| "present timing 1% low evidence overflowed".to_string())?;
        if remainder != 0 {
            slow_duration_scaled = slow_duration_scaled
                .checked_add(
                    u128::from(self.slowest_one_percent[whole].duration_qpc)
                        .checked_mul(u128::from(remainder))
                        .ok_or_else(|| {
                            "present timing 1% low boundary evidence overflowed".to_string()
                        })?,
                )
                .ok_or_else(|| "present timing 1% low evidence overflowed".to_string())?;
        }
        if slow_duration_scaled == 0 {
            return Err("present timing 1% low evidence duration is zero".into());
        }
        let expected_low1 = u128::from(self.qpc_frequency)
            .checked_mul(1_000)
            .and_then(|value| value.checked_mul(u128::from(interval_count)))
            .ok_or_else(|| "present timing 1% low evidence calculation overflowed".to_string())?
            / slow_duration_scaled;
        if expected_low1 != u128::from(self.low1_millifps) {
            return Err(format!(
                "present timing 1% low does not match interval evidence: expected {expected_low1}, got {}",
                self.low1_millifps
            ));
        }
        Ok(())
    }
}

impl SelectedPresentLatencySummary {
    fn validate(&self) -> Result<(), String> {
        for (name, quantiles) in [
            ("selected_lag_presents", self.selected_lag_presents),
            ("wgc_delivery_us", self.wgc_delivery_us),
            (
                "wgc_source_ahead_callback_ready_us",
                self.wgc_source_ahead_callback_ready_us,
            ),
            ("callback_to_decision_us", self.callback_to_decision_us),
            ("source_to_probe_us", self.source_to_probe_us),
            (
                "wgc_source_ahead_process_us",
                self.wgc_source_ahead_process_us,
            ),
            ("decision_to_probe_us", self.decision_to_probe_us),
        ] {
            quantiles.validate(name)?;
        }
        for (count, quantiles, name) in [
            (
                self.wgc_delivery_sample_count,
                self.wgc_delivery_us,
                "WGC delivery",
            ),
            (
                self.wgc_source_ahead_callback_ready_count,
                self.wgc_source_ahead_callback_ready_us,
                "WGC source-ahead",
            ),
            (
                self.source_to_probe_sample_count,
                self.source_to_probe_us,
                "source-to-probe",
            ),
            (
                self.wgc_source_ahead_process_count,
                self.wgc_source_ahead_process_us,
                "WGC source-ahead-of-probe",
            ),
        ] {
            if count == 0 && quantiles != LatencyQuantiles::default() {
                return Err(format!("{name} quantiles exist without {name} samples"));
            }
        }
        if self.sample_count == 0
            && (self.selected_lag_presents != LatencyQuantiles::default()
                || self.callback_to_decision_us != LatencyQuantiles::default()
                || self.source_to_probe_us != LatencyQuantiles::default()
                || self.decision_to_probe_us != LatencyQuantiles::default())
        {
            return Err("selected feedback quantiles exist without samples".into());
        }
        Ok(())
    }
}

impl FlushSummary {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.writer_queue_depth != 0 {
            return Err(ProtocolError::NotFlushed(self.writer_queue_depth));
        }
        if self.last_present_seq != self.cutoff_present_seq {
            return Err(ProtocolError::Contract(format!(
                "last_present_seq {} does not equal cutoff_present_seq {}",
                self.last_present_seq, self.cutoff_present_seq
            )));
        }
        if self.depth_row_count != self.depth_file_count {
            return Err(ProtocolError::Contract(format!(
                "depth row/file count mismatch: {}/{}",
                self.depth_row_count, self.depth_file_count
            )));
        }
        match (
            self.performance_metrics_enabled,
            self.present_timing.as_ref(),
        ) {
            (true, Some(timing)) => timing.validate().map_err(ProtocolError::Contract)?,
            (true, None) => {
                return Err(ProtocolError::Contract(
                    "performance metrics enabled without Present timing evidence".into(),
                ));
            }
            (false, Some(_)) => {
                return Err(ProtocolError::Contract(
                    "Present timing evidence exists while performance metrics are disabled".into(),
                ));
            }
            (false, None) => {}
        }
        if self.writer_bypass {
            if self.depth_row_count != 0 || self.depth_file_count != 0 {
                return Err(ProtocolError::Contract(
                    "writer-bypass flush must not contain depth rows/files".into(),
                ));
            }
        } else if self.writer_bypassed_frames != 0 {
            return Err(ProtocolError::Contract(
                "writer_bypassed_frames exists while writer_bypass is false".into(),
            ));
        }
        if let Some(timing) = self.writer_timing {
            timing.validate().map_err(ProtocolError::Contract)?;
            if self.writer_bypass && timing.sample_count != 0 {
                return Err(ProtocolError::Contract(
                    "writer-bypass flush contains writer timing samples".into(),
                ));
            }
        }
        if let Some(selected) = &self.selected_present {
            if selected.unresolved_candidates != 0 || selected.ring_overwrite_count != 0 {
                return Err(ProtocolError::Contract(format!(
                    "selected-Present ring did not flush cleanly: unresolved={} overwrites={}",
                    selected.unresolved_candidates, selected.ring_overwrite_count
                )));
            }
            if selected.feedback_queue_depth != 0
                || selected.ack_queue_depth != 0
                || selected.feedback_queue_full_count != 0
                || selected.ack_queue_full_count != 0
                || selected.transport_fault_reason != 0
            {
                return Err(ProtocolError::Contract(format!(
                    "selected-Present transport did not flush cleanly: decision_depth={} ack_depth={} decision_full={} ack_full={} fatal={}",
                    selected.feedback_queue_depth,
                    selected.ack_queue_depth,
                    selected.feedback_queue_full_count,
                    selected.ack_queue_full_count,
                    selected.transport_fault_reason
                )));
            }
            if selected.selected_frame_count
                != selected
                    .distinct_selected_depth_count
                    .saturating_add(selected.source_identity_alias_count)
            {
                return Err(ProtocolError::Contract(
                    "selected-Present select/distinct/duplicate counts do not balance".into(),
                ));
            }
            let expected = selected.distinct_selected_depth_count;
            let writer_count_matches = if self.writer_bypass {
                self.writer_bypassed_frames == expected
                    && self.depth_row_count == 0
                    && self.depth_file_count == 0
            } else {
                self.depth_row_count == expected && self.depth_file_count == expected
            };
            if selected.readback_submitted_count != expected
                || selected.readback_completed_count != expected
                || !writer_count_matches
            {
                return Err(ProtocolError::Contract(format!(
                    "selected depth count mismatch: distinct={expected} submitted={} completed={} rows={} files={} writer_bypass={} bypassed={}",
                    selected.readback_submitted_count,
                    selected.readback_completed_count,
                    self.depth_row_count,
                    self.depth_file_count,
                    self.writer_bypass,
                    self.writer_bypassed_frames,
                )));
            }
            if selected.materialized_candidates < expected {
                return Err(ProtocolError::Contract(
                    "selected depth count exceeds materialized candidates".into(),
                ));
            }
            if (selected.materialized_candidates > 0)
                != selected.first_candidate_present_seq.is_some()
            {
                return Err(ProtocolError::Contract(
                    "selected-Present materialized candidate count and Arm boundary disagree"
                        .into(),
                ));
            }
            if selected
                .first_candidate_present_seq
                .is_some_and(|first| first > self.cutoff_present_seq)
            {
                return Err(ProtocolError::Contract(
                    "selected-Present Arm boundary is later than the cutoff".into(),
                ));
            }
            if selected.feedback_latency.sample_count != selected.selected_frame_count {
                return Err(ProtocolError::Contract(format!(
                    "selected feedback latency coverage mismatch: samples={} decisions={}",
                    selected.feedback_latency.sample_count, selected.selected_frame_count
                )));
            }
            // The WGC clock can lead ours at two points: our callback, and the
            // probe read past it. Each is split into a behind/ahead pair of
            // distributions, and every Select lands in exactly one side of
            // each. A pair that does not add up means a sample was dropped,
            // double-counted, or read from a summary that predates the split.
            let latency = &selected.feedback_latency;
            for (behind_name, behind, ahead_name, ahead) in [
                (
                    "delivery",
                    latency.wgc_delivery_sample_count,
                    "ahead",
                    latency.wgc_source_ahead_callback_ready_count,
                ),
                (
                    "source_to_probe",
                    latency.source_to_probe_sample_count,
                    "ahead_of_probe",
                    latency.wgc_source_ahead_process_count,
                ),
            ] {
                let classified = behind.checked_add(ahead).ok_or_else(|| {
                    ProtocolError::Contract("WGC latency sample counts overflowed".into())
                })?;
                if classified != latency.sample_count {
                    return Err(ProtocolError::Contract(format!(
                        "WGC latency sample coverage mismatch: {behind_name}={behind} \
                         {ahead_name}={ahead} samples={}",
                        latency.sample_count
                    )));
                }
            }
            latency.validate().map_err(ProtocolError::Contract)?;
            if selected.feedback_latency.selected_lag_presents.max
                < selected.max_feedback_lag_presents
            {
                return Err(ProtocolError::Contract(format!(
                    "selected feedback lag maximum is below distinct ring maximum: decisions={} ring={}",
                    selected.feedback_latency.selected_lag_presents.max,
                    selected.max_feedback_lag_presents
                )));
            }
            if expected > 0 {
                let allocation = selected.gpu_allocation.as_ref().ok_or_else(|| {
                    ProtocolError::Contract(
                        "selected-Present run has depth but no GPU allocation evidence".into(),
                    )
                })?;
                if allocation.capacity == 0 {
                    return Err(ProtocolError::Contract(
                        "selected-Present GPU allocation capacity is zero".into(),
                    ));
                }
                if allocation.estimated_bytes > allocation.configured_limit_bytes {
                    return Err(ProtocolError::Contract(format!(
                        "selected-Present GPU allocation estimate {} exceeds configured limit {}",
                        allocation.estimated_bytes, allocation.configured_limit_bytes
                    )));
                }
                if allocation.actual_usage_delta > allocation.configured_limit_bytes {
                    return Err(ProtocolError::Contract(format!(
                        "selected-Present attributable GPU allocation delta {} exceeds configured limit {} (usage_before={} usage_after={})",
                        allocation.actual_usage_delta,
                        allocation.configured_limit_bytes,
                        allocation.usage_before,
                        allocation.usage_after,
                    )));
                }
                let expected_allocation_headroom = allocation
                    .budget_after
                    .saturating_sub(allocation.usage_after);
                if allocation.headroom_after != expected_allocation_headroom {
                    return Err(ProtocolError::Contract(format!(
                        "selected-Present post-allocation GPU headroom is inconsistent: reported={} calculated={expected_allocation_headroom}",
                        allocation.headroom_after
                    )));
                }
                if allocation.headroom_after < allocation.configured_reserve_bytes {
                    return Err(ProtocolError::Contract(format!(
                        "selected-Present post-allocation GPU headroom {} is below configured reserve {}",
                        allocation.headroom_after, allocation.configured_reserve_bytes
                    )));
                }
                let (Some(budget_at_flush), Some(usage_at_flush), Some(headroom_at_flush)) = (
                    allocation.budget_at_flush,
                    allocation.usage_at_flush,
                    allocation.headroom_at_flush,
                ) else {
                    return Err(ProtocolError::Contract(
                        "selected-Present GPU allocation evidence has no complete run-end headroom snapshot"
                            .into(),
                    ));
                };
                let expected_flush_headroom = budget_at_flush.saturating_sub(usage_at_flush);
                if headroom_at_flush != expected_flush_headroom {
                    return Err(ProtocolError::Contract(format!(
                        "selected-Present run-end GPU headroom is inconsistent: reported={headroom_at_flush} calculated={expected_flush_headroom}"
                    )));
                }
                if headroom_at_flush < allocation.configured_reserve_bytes {
                    return Err(ProtocolError::Contract(format!(
                        "selected-Present run-end GPU headroom {headroom_at_flush} is below configured reserve {} (budget={budget_at_flush} usage={usage_at_flush})",
                        allocation.configured_reserve_bytes
                    )));
                }
            }
        }
        if !self.errors.is_empty() {
            return Err(ProtocolError::WriterErrors(self.errors.clone()));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProbeAck {
    pub schema: String,
    pub command_id: String,
    pub identity: StreamIdentity,
    pub ordinal: u32,
    pub ack: AckKind,
}

/// Adapter identity observed from the game's actual swapchain device.
///
/// The description is useful to humans, while the DXGI LUID is the strict
/// machine identity. Both the LUID and driver are bound into performance
/// certification so a hybrid or virtual adapter cannot reuse another GPU's
/// evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GpuAdapterIdentity {
    pub description: String,
    pub driver_version: String,
    pub luid_high: i32,
    pub luid_low: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum AckKind {
    Started,
    Ready {
        actual_profile: DepthProfileIdentity,
        actual_binding: CameraBindingIdentity,
        depth_profile_sha256: String,
        camera_binding_sha256: String,
        probe_version: String,
        actual_swapchain_adapter: GpuAdapterIdentity,
        first_present_seq: u64,
        resource_epoch: u64,
        performance_metrics_enabled: bool,
    },
    Stopped {
        cutoff_qpc: i64,
        cutoff_present_seq: u64,
    },
    Flushed {
        summary: FlushSummary,
    },
    Faulted {
        phase: String,
        code: String,
        message: String,
        summary: Option<FlushSummary>,
    },
}

impl ControlEnvelope {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        validate_schema(&self.schema, CONTROL_SCHEMA_V3)?;
        validate_identity(&self.identity)?;
        non_empty("command_id", &self.command_id)?;
        match &self.command {
            ControlCommand::StartRun(start) => {
                // Present cadence is the baseline's measured source rate, not the
                // selected video CFR. Source 60 -> video 30 is valid; source 30 ->
                // video 60 cannot satisfy this operating contract.
                if let Some(fps) = start.required_present_fps
                    && (!matches!(fps, 30 | 60)
                        || start
                            .lane_fps
                            .is_none_or(|lane| !matches!(lane, 30 | 60) || fps < lane)
                        || start.performance_validation_only)
                {
                    return Err(ProtocolError::Contract(
                        "invalid device-profile cadence requirement".into(),
                    ));
                }
                if start.target_process_id == 0 {
                    return Err(ProtocolError::Contract(
                        "target_process_id must be positive".into(),
                    ));
                }
                non_empty("output_dir", &start.output_dir)?;
                non_empty("depth_profile_path", &start.depth_profile_path)?;
                non_empty("camera_binding_path", &start.camera_binding_path)?;
                if !crate::camera_binding::is_sha256_digest(&start.depth_profile_sha256)
                    || !crate::camera_binding::is_sha256_digest(&start.camera_binding_sha256)
                {
                    return Err(ProtocolError::Contract(
                        "StartRun profile/binding hashes must be lowercase sha256:<64 hex>".into(),
                    ));
                }
                if start.present_only {
                    if !start.performance_validation_only {
                        return Err(ProtocolError::Contract(
                            "present-only mode is restricted to performance-validation-only runs"
                                .into(),
                        ));
                    }
                    if !start.performance_metrics_enabled {
                        return Err(ProtocolError::Contract(
                            "present-only mode requires per-Present performance metrics".into(),
                        ));
                    }
                    if !start.watermark_only {
                        return Err(ProtocolError::Contract(
                            "present-only mode must also bypass the depth path".into(),
                        ));
                    }
                    if start.writer_bypass || start.selected_present_feedback.is_some() {
                        return Err(ProtocolError::Contract(
                            "present-only mode cannot enable writer bypass or selected-Present feedback"
                                .into(),
                        ));
                    }
                }
                if start.watermark_only {
                    if !start.performance_validation_only {
                        return Err(ProtocolError::Contract(
                            "watermark-only mode is restricted to performance-validation-only runs"
                                .into(),
                        ));
                    }
                    if start.selected_present_feedback.is_some() {
                        return Err(ProtocolError::Contract(
                            "watermark-only mode cannot enable selected-Present feedback because it does not materialize depth candidates"
                                .into(),
                        ));
                    }
                }
                if start.writer_bypass {
                    if !start.performance_validation_only {
                        return Err(ProtocolError::Contract(
                            "writer-bypass mode is restricted to performance-validation-only runs"
                                .into(),
                        ));
                    }
                    if start.watermark_only {
                        return Err(ProtocolError::Contract(
                            "writer-bypass and watermark-only modes are mutually exclusive".into(),
                        ));
                    }
                    if start.selected_present_feedback.is_none() {
                        return Err(ProtocolError::Contract(
                            "writer-bypass mode requires selected-Present feedback/readback".into(),
                        ));
                    }
                }
                if let Some(feedback) = &start.selected_present_feedback {
                    feedback.validate()?;
                }
                for (field, value) in [
                    ("output_dir", start.output_dir.as_str()),
                    ("depth_profile_path", start.depth_profile_path.as_str()),
                    ("camera_binding_path", start.camera_binding_path.as_str()),
                ] {
                    if !Path::new(value).is_absolute() {
                        return Err(ProtocolError::Contract(format!(
                            "{field} must be absolute so client and injected process resolve the same file"
                        )));
                    }
                }
                if !start.target_depth_hz.is_finite() || start.target_depth_hz <= 0.0 {
                    return Err(ProtocolError::Contract(
                        "target_depth_hz must be finite and positive".into(),
                    ));
                }
            }
            ControlCommand::StopRun(stop) => {
                non_empty("reason", &stop.reason)?;
                if stop.cutoff_qpc <= 0 {
                    return Err(ProtocolError::Contract(
                        "cutoff_qpc must be positive".into(),
                    ));
                }
            }
        }
        Ok(())
    }
}

impl ProbeAck {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        validate_schema(&self.schema, ACK_SCHEMA_V3)?;
        validate_identity(&self.identity)?;
        non_empty("command_id", &self.command_id)?;
        if self.ordinal == 0 {
            return Err(ProtocolError::Contract(
                "ACK ordinal must be positive".into(),
            ));
        }
        match &self.ack {
            AckKind::Ready {
                actual_profile,
                actual_binding,
                depth_profile_sha256,
                camera_binding_sha256,
                probe_version,
                actual_swapchain_adapter,
                first_present_seq,
                resource_epoch,
                performance_metrics_enabled: _,
            } => {
                if !crate::camera_binding::is_sha256_digest(depth_profile_sha256)
                    || !crate::camera_binding::is_sha256_digest(camera_binding_sha256)
                {
                    return Err(ProtocolError::Contract(
                        "Ready profile/binding hashes must be lowercase sha256:<64 hex>".into(),
                    ));
                }
                for (field, value) in [
                    (
                        "actual_profile.schema_version",
                        actual_profile.schema_version.as_str(),
                    ),
                    ("actual_profile.game_id", actual_profile.game_id.as_str()),
                    ("actual_profile.lane", actual_profile.lane.as_str()),
                    (
                        "actual_profile.executable_sha256",
                        actual_profile.executable_sha256.as_str(),
                    ),
                    ("actual_binding.schema", actual_binding.schema.as_str()),
                    (
                        "actual_binding.binding_id",
                        actual_binding.binding_id.as_str(),
                    ),
                    (
                        "actual_binding.binding_version",
                        actual_binding.binding_version.as_str(),
                    ),
                    (
                        "actual_binding.profile_id",
                        actual_binding.profile_id.as_str(),
                    ),
                    (
                        "actual_binding.profile_version",
                        actual_binding.profile_version.as_str(),
                    ),
                    (
                        "actual_binding.executable_sha256",
                        actual_binding.executable_sha256.as_str(),
                    ),
                    ("probe_version", probe_version.as_str()),
                    (
                        "actual_swapchain_adapter.description",
                        actual_swapchain_adapter.description.as_str(),
                    ),
                    (
                        "actual_swapchain_adapter.driver_version",
                        actual_swapchain_adapter.driver_version.as_str(),
                    ),
                ] {
                    non_empty(field, value)?;
                }
                if actual_profile.game_id != actual_binding.profile_id
                    || actual_profile.schema_version != actual_binding.profile_version
                    || actual_profile.executable_sha256 != actual_binding.executable_sha256
                {
                    return Err(ProtocolError::Contract(
                        "Ready profile and CameraBinding identities disagree".into(),
                    ));
                }
                if !matches!(
                    (
                        actual_profile.schema_version.as_str(),
                        actual_binding.schema.as_str()
                    ),
                    ("dm-depth-profile-1", "camera-binding/v1")
                        | ("dm-depth-profile-2", "camera-binding/v2")
                        | ("dm-depth-profile-2", "camera-binding/cp2077-holstered-v3")
                        | ("dm-depth-profile-3", "camera-binding/cp2077-holstered-v3")
                        | ("dm-depth-profile-3", "camera-binding/kcd2-combined-v1")
                        | ("dm-depth-profile-3", "camera-binding/kcd2-combined-v2")
                ) {
                    return Err(ProtocolError::Contract(
                        "Ready depth/camera schema generations disagree".into(),
                    ));
                }
                if *first_present_seq == u64::MAX {
                    return Err(ProtocolError::Contract(
                        "Ready first_present_seq uses the reserved sentinel".into(),
                    ));
                }
                if *resource_epoch == 0 {
                    return Err(ProtocolError::Contract(
                        "Ready resource_epoch must be positive".into(),
                    ));
                }
            }
            AckKind::Flushed { summary } => summary.validate()?,
            AckKind::Faulted {
                phase,
                code,
                message,
                ..
            } => {
                non_empty("fault.phase", phase)?;
                non_empty("fault.code", code)?;
                non_empty("fault.message", message)?;
            }
            AckKind::Started | AckKind::Stopped { .. } => {}
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientLifecycleState {
    Idle,
    Starting,
    Started,
    Ready,
    Stopping,
    Stopped,
    Flushed,
    Faulted,
}

#[derive(Debug, Clone)]
pub struct ClientLifecycle {
    state: ClientLifecycleState,
    identity: Option<StreamIdentity>,
    start_command_id: Option<String>,
    stop_command_id: Option<String>,
    stop_cutoff_qpc: Option<i64>,
    stopped_cutoff_present_seq: Option<u64>,
    ready_first_present_seq: Option<u64>,
    ready_resource_epoch: Option<u64>,
    expected_performance_metrics_enabled: Option<bool>,
    expected_writer_bypass: Option<bool>,
    expected_presentation_only: bool,
    last_ordinal: u32,
    confirmed_flush: Option<FlushSummary>,
}

impl Default for ClientLifecycle {
    fn default() -> Self {
        Self {
            state: ClientLifecycleState::Idle,
            identity: None,
            start_command_id: None,
            stop_command_id: None,
            stop_cutoff_qpc: None,
            stopped_cutoff_present_seq: None,
            ready_first_present_seq: None,
            ready_resource_epoch: None,
            expected_performance_metrics_enabled: None,
            expected_writer_bypass: None,
            expected_presentation_only: false,
            last_ordinal: 0,
            confirmed_flush: None,
        }
    }
}

impl ClientLifecycle {
    #[must_use]
    pub fn state(&self) -> ClientLifecycleState {
        self.state
    }

    #[must_use]
    pub fn identity(&self) -> Option<&StreamIdentity> {
        self.identity.as_ref()
    }

    #[must_use]
    pub fn confirmed_flush(&self) -> Option<&FlushSummary> {
        self.confirmed_flush.as_ref()
    }

    #[must_use]
    pub fn last_ordinal(&self) -> u32 {
        self.last_ordinal
    }

    #[must_use]
    pub fn ready_first_present_seq(&self) -> Option<u64> {
        self.ready_first_present_seq
    }

    #[must_use]
    pub fn ready_resource_epoch(&self) -> Option<u64> {
        self.ready_resource_epoch
    }

    pub fn begin_start(&mut self, command: &ControlEnvelope) -> Result<(), ProtocolError> {
        command.validate()?;
        let ControlCommand::StartRun(start) = &command.command else {
            return Err(ProtocolError::Unexpected(
                "expected StartRun command".into(),
            ));
        };
        if !matches!(
            self.state,
            ClientLifecycleState::Idle
                | ClientLifecycleState::Flushed
                | ClientLifecycleState::Faulted
        ) {
            return Err(ProtocolError::Unexpected(format!(
                "cannot start from {:?}",
                self.state
            )));
        }
        if let Some(previous) = &self.identity
            && command.identity.generation <= previous.generation
        {
            return Err(ProtocolError::StaleGeneration {
                expected_after: previous.generation,
                actual: command.identity.generation,
            });
        }
        self.state = ClientLifecycleState::Starting;
        self.identity = Some(command.identity.clone());
        self.start_command_id = Some(command.command_id.clone());
        self.stop_command_id = None;
        self.stop_cutoff_qpc = None;
        self.stopped_cutoff_present_seq = None;
        self.ready_first_present_seq = None;
        self.ready_resource_epoch = None;
        self.expected_performance_metrics_enabled = Some(start.performance_metrics_enabled);
        self.expected_writer_bypass = Some(start.writer_bypass);
        self.expected_presentation_only = start.performance_validation_only
            && start.performance_metrics_enabled
            && start.watermark_only
            && !start.writer_bypass;
        self.last_ordinal = 0;
        self.confirmed_flush = None;
        Ok(())
    }

    pub fn begin_stop(&mut self, command: &ControlEnvelope) -> Result<(), ProtocolError> {
        command.validate()?;
        if !matches!(command.command, ControlCommand::StopRun(_)) {
            return Err(ProtocolError::Unexpected("expected StopRun command".into()));
        }
        if !matches!(
            self.state,
            ClientLifecycleState::Starting
                | ClientLifecycleState::Started
                | ClientLifecycleState::Ready
        ) {
            return Err(ProtocolError::Unexpected(format!(
                "cannot stop from {:?}",
                self.state
            )));
        }
        if self.identity.as_ref() != Some(&command.identity) {
            return Err(ProtocolError::IdentityMismatch);
        }
        let ControlCommand::StopRun(stop) = &command.command else {
            unreachable!("validated StopRun above")
        };
        self.stop_command_id = Some(command.command_id.clone());
        self.stop_cutoff_qpc = Some(stop.cutoff_qpc);
        self.stopped_cutoff_present_seq = None;
        self.state = ClientLifecycleState::Stopping;
        Ok(())
    }

    /// Consume a well-formed Ready ACK whose provenance was rejected by the
    /// client without transitioning to Ready.
    ///
    /// The ordinal must still advance: startup cleanup will otherwise see the
    /// rejected Ready again while already Stopping and fail before it can
    /// observe the probe's terminal Stop/Fault ACK.
    pub fn consume_rejected_ready(&mut self, ack: &ProbeAck) -> Result<(), ProtocolError> {
        ack.validate()?;
        if self.state != ClientLifecycleState::Started {
            return Err(ProtocolError::Unexpected(format!(
                "cannot reject Ready from {:?}",
                self.state
            )));
        }
        if self.identity.as_ref() != Some(&ack.identity) {
            return Err(ProtocolError::IdentityMismatch);
        }
        if ack.ordinal != self.last_ordinal.saturating_add(1) {
            return Err(ProtocolError::AckOrder {
                expected: self.last_ordinal.saturating_add(1),
                actual: ack.ordinal,
            });
        }
        if !matches!(ack.ack, AckKind::Ready { .. }) {
            return Err(ProtocolError::Unexpected(
                "consume_rejected_ready requires a Ready ACK".into(),
            ));
        }
        if self.start_command_id.as_deref() != Some(ack.command_id.as_str()) {
            return Err(ProtocolError::CommandMismatch);
        }
        self.last_ordinal = ack.ordinal;
        Ok(())
    }

    pub fn apply_ack(&mut self, ack: &ProbeAck) -> Result<(), ProtocolError> {
        ack.validate()?;
        if self.identity.as_ref() != Some(&ack.identity) {
            return Err(
                if self.identity.as_ref().is_some_and(|expected| {
                    expected.session_id == ack.identity.session_id
                        && ack.identity.generation < expected.generation
                }) {
                    ProtocolError::StaleGeneration {
                        expected_after: self.identity.as_ref().expect("checked").generation - 1,
                        actual: ack.identity.generation,
                    }
                } else {
                    ProtocolError::IdentityMismatch
                },
            );
        }
        if ack.ordinal != self.last_ordinal.saturating_add(1) {
            return Err(ProtocolError::AckOrder {
                expected: self.last_ordinal.saturating_add(1),
                actual: ack.ordinal,
            });
        }
        let command_matches = match ack.ack {
            AckKind::Started | AckKind::Ready { .. } => {
                self.start_command_id.as_deref() == Some(ack.command_id.as_str())
            }
            AckKind::Stopped { .. } | AckKind::Flushed { .. } => {
                self.stop_command_id.as_deref() == Some(ack.command_id.as_str())
            }
            AckKind::Faulted { .. } => {
                self.stop_command_id.as_deref() == Some(ack.command_id.as_str())
                    // The probe binds a terminal fault to Stop only after it has
                    // observed StopRun. Therefore a next-ordinal Fault still
                    // bound to the active Start while the client is Stopping
                    // proves the fault won the cross-process race. Identity and
                    // ordinal validation above remain mandatory.
                    || (self.state == ClientLifecycleState::Stopping
                        && self.start_command_id.as_deref() == Some(ack.command_id.as_str()))
                    || (self.stop_command_id.is_none()
                        && self.start_command_id.as_deref() == Some(ack.command_id.as_str()))
            }
        };
        if !command_matches {
            return Err(ProtocolError::CommandMismatch);
        }
        match &ack.ack {
            AckKind::Stopped {
                cutoff_qpc,
                cutoff_present_seq: _,
            } if Some(*cutoff_qpc) != self.stop_cutoff_qpc => {
                return Err(ProtocolError::Contract(format!(
                    "Stopped cutoff_qpc {cutoff_qpc} does not match requested {:?}",
                    self.stop_cutoff_qpc
                )));
            }
            AckKind::Stopped {
                cutoff_present_seq, ..
            } if self
                .ready_first_present_seq
                .is_some_and(|first| *cutoff_present_seq < first) =>
            {
                return Err(ProtocolError::Contract(format!(
                    "Stopped cutoff_present_seq {cutoff_present_seq} precedes Ready first_present_seq {:?}",
                    self.ready_first_present_seq
                )));
            }
            AckKind::Flushed { summary }
                if Some(summary.cutoff_present_seq) != self.stopped_cutoff_present_seq =>
            {
                return Err(ProtocolError::Contract(format!(
                    "Flushed cutoff_present_seq {} does not match Stopped {:?}",
                    summary.cutoff_present_seq, self.stopped_cutoff_present_seq
                )));
            }
            AckKind::Ready {
                performance_metrics_enabled,
                ..
            } if Some(*performance_metrics_enabled)
                != self.expected_performance_metrics_enabled =>
            {
                return Err(ProtocolError::Contract(format!(
                    "Ready performance_metrics_enabled={} does not match StartRun {:?}",
                    performance_metrics_enabled, self.expected_performance_metrics_enabled
                )));
            }
            AckKind::Flushed { summary }
                if Some(summary.performance_metrics_enabled)
                    != self.expected_performance_metrics_enabled =>
            {
                return Err(ProtocolError::Contract(format!(
                    "Flushed performance_metrics_enabled={} does not match StartRun {:?}",
                    summary.performance_metrics_enabled, self.expected_performance_metrics_enabled
                )));
            }
            AckKind::Flushed { summary }
                if Some(summary.writer_bypass) != self.expected_writer_bypass =>
            {
                return Err(ProtocolError::Contract(format!(
                    "Flushed writer_bypass={} does not match StartRun {:?}",
                    summary.writer_bypass, self.expected_writer_bypass
                )));
            }
            AckKind::Flushed { summary }
                if self.expected_presentation_only
                    && summary.camera_pose_row_count == 0
                    && summary.camera_diagnostic_row_count == 0
                    && summary.player_pose_row_count.is_none() =>
            {
                // D3D12 baseline/watermark have a Present timeline, not a pose
                // stream. Only an explicitly requested validation stage can
                // use this empty payload shape; timing still validates above.
                if summary.depth_row_count != 0
                    || summary.depth_file_count != 0
                    || summary.selected_present.is_some()
                    || summary.writer_bypassed_frames != 0
                    || summary
                        .writer_timing
                        .is_some_and(|timing| timing.sample_count != 0)
                {
                    return Err(ProtocolError::Contract(
                        "presentation-only stage contains depth capture payload".into(),
                    ));
                }
            }
            AckKind::Flushed { summary } if self.ready_first_present_seq.is_some() => {
                let first = self.ready_first_present_seq.expect("checked");
                let expected_camera_rows = summary
                    .cutoff_present_seq
                    .checked_sub(first)
                    .and_then(|distance| distance.checked_add(1))
                    .ok_or_else(|| {
                        ProtocolError::Contract(
                            "Camera present range overflowed or ran backwards".into(),
                        )
                    })?;
                let actual_camera_rows = summary
                    .camera_pose_row_count
                    .checked_add(summary.camera_diagnostic_row_count)
                    .ok_or_else(|| ProtocolError::Contract("Camera row count overflowed".into()))?;
                if actual_camera_rows != expected_camera_rows {
                    return Err(ProtocolError::Contract(format!(
                        "Camera pose/diagnostic rows {actual_camera_rows} do not cover Ready..cutoff range {first}..={} ({expected_camera_rows} rows)",
                        summary.cutoff_present_seq
                    )));
                }
                if let Some(player_rows) = summary.player_pose_row_count
                    && player_rows != expected_camera_rows
                {
                    return Err(ProtocolError::Contract(format!(
                        "player pose row count {player_rows} does not cover Present window {expected_camera_rows}"
                    )));
                }
            }
            _ => {}
        }

        self.state = match (&self.state, &ack.ack) {
            (ClientLifecycleState::Starting, AckKind::Started) => ClientLifecycleState::Started,
            (
                ClientLifecycleState::Started,
                AckKind::Ready {
                    first_present_seq,
                    resource_epoch,
                    ..
                },
            ) => {
                self.ready_first_present_seq = Some(*first_present_seq);
                self.ready_resource_epoch = Some(*resource_epoch);
                ClientLifecycleState::Ready
            }
            (
                ClientLifecycleState::Stopping,
                AckKind::Stopped {
                    cutoff_present_seq, ..
                },
            ) => {
                self.stopped_cutoff_present_seq = Some(*cutoff_present_seq);
                ClientLifecycleState::Stopped
            }
            (ClientLifecycleState::Stopped, AckKind::Flushed { summary }) => {
                self.confirmed_flush = Some(summary.clone());
                ClientLifecycleState::Flushed
            }
            (
                ClientLifecycleState::Starting
                | ClientLifecycleState::Started
                | ClientLifecycleState::Ready
                | ClientLifecycleState::Stopping
                | ClientLifecycleState::Stopped,
                AckKind::Faulted { .. },
            ) => ClientLifecycleState::Faulted,
            _ => {
                return Err(ProtocolError::Unexpected(format!(
                    "ACK {:?} cannot follow state {:?}",
                    ack.ack, self.state
                )));
            }
        };
        self.last_ordinal = ack.ordinal;
        Ok(())
    }

    /// Records a client-side transport/timeout failure without pretending that
    /// the probe emitted a `Faulted` ACK. The identity is retained so a later
    /// run must use a strictly newer generation and late replies stay stale.
    pub fn fail_local(&mut self, identity: &StreamIdentity) -> Result<(), ProtocolError> {
        if self.identity.as_ref() != Some(identity) {
            return Err(ProtocolError::IdentityMismatch);
        }
        if matches!(
            self.state,
            ClientLifecycleState::Idle | ClientLifecycleState::Flushed
        ) {
            return Err(ProtocolError::Unexpected(format!(
                "cannot record local fault from {:?}",
                self.state
            )));
        }
        self.state = ClientLifecycleState::Faulted;
        self.confirmed_flush = None;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolError {
    Contract(String),
    UnsupportedSchema {
        expected: &'static str,
        actual: String,
    },
    IdentityMismatch,
    StaleGeneration {
        expected_after: u64,
        actual: u64,
    },
    CommandMismatch,
    AckOrder {
        expected: u32,
        actual: u32,
    },
    Unexpected(String),
    NotFlushed(u64),
    WriterErrors(Vec<String>),
    Io(String),
}

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for ProtocolError {}

#[must_use]
pub fn control_path() -> PathBuf {
    #[cfg(windows)]
    let base = std::env::var_os("PROGRAMDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
    #[cfg(not(windows))]
    let base = std::env::temp_dir();
    base.join(CONTROL_RELATIVE_PATH.replace('\\', std::path::MAIN_SEPARATOR_STR))
}

#[must_use]
pub fn ack_dir(output_dir: impl AsRef<Path>) -> PathBuf {
    output_dir.as_ref().join(ACK_RELATIVE_DIR)
}

pub fn load_command_from(path: &Path) -> Result<ControlEnvelope, ProtocolError> {
    let text = fs::read_to_string(path).map_err(|error| ProtocolError::Io(error.to_string()))?;
    let command: ControlEnvelope =
        serde_json::from_str(&text).map_err(|error| ProtocolError::Contract(error.to_string()))?;
    command.validate()?;
    Ok(command)
}

pub fn write_command(path: &Path, command: &ControlEnvelope) -> Result<(), ProtocolError> {
    command.validate()?;
    write_replace(path, command)
}

pub fn write_ack(output_dir: &Path, ack: &ProbeAck) -> Result<PathBuf, ProtocolError> {
    ack.validate()?;
    let dir = ack_dir(output_dir);
    fs::create_dir_all(&dir).map_err(|error| ProtocolError::Io(error.to_string()))?;
    let path = dir.join(format!(
        "{:04}-{}.json",
        ack.ordinal,
        ack_kind_name(&ack.ack)
    ));
    let tmp = dir.join(format!(
        ".{:04}-{}.tmp",
        ack.ordinal,
        ack_kind_name(&ack.ack)
    ));
    let bytes = serde_json::to_vec_pretty(ack)
        .map_err(|error| ProtocolError::Contract(error.to_string()))?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .map_err(|error| ProtocolError::Io(error.to_string()))?;
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| ProtocolError::Io(error.to_string()))?;
    drop(file);
    fs::rename(&tmp, &path).map_err(|error| ProtocolError::Io(error.to_string()))?;
    Ok(path)
}

pub fn read_acks(output_dir: &Path) -> Result<Vec<ProbeAck>, ProtocolError> {
    let dir = ack_dir(output_dir);
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut paths = fs::read_dir(&dir)
        .map_err(|error| ProtocolError::Io(error.to_string()))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect::<Vec<_>>();
    paths.sort();
    paths
        .into_iter()
        .map(|path| {
            let text = fs::read_to_string(&path)
                .map_err(|error| ProtocolError::Io(format!("{}: {error}", path.display())))?;
            let ack: ProbeAck = serde_json::from_str(&text)
                .map_err(|error| ProtocolError::Contract(format!("{}: {error}", path.display())))?;
            ack.validate()?;
            Ok(ack)
        })
        .collect()
}

fn write_replace<T: Serialize>(path: &Path, value: &T) -> Result<(), ProtocolError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| ProtocolError::Io(error.to_string()))?;
    }
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|error| ProtocolError::Contract(error.to_string()))?;
    let mut file = File::create(&tmp).map_err(|error| ProtocolError::Io(error.to_string()))?;
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| ProtocolError::Io(error.to_string()))?;
    drop(file);
    replace_file(&tmp, path)
}

#[cfg(not(windows))]
fn replace_file(source: &Path, destination: &Path) -> Result<(), ProtocolError> {
    fs::rename(source, destination).map_err(|error| ProtocolError::Io(error.to_string()))
}

#[cfg(windows)]
fn replace_file(source: &Path, destination: &Path) -> Result<(), ProtocolError> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };
    use windows::core::PCWSTR;

    let source = source
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let destination = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    unsafe {
        MoveFileExW(
            PCWSTR(source.as_ptr()),
            PCWSTR(destination.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    }
    .map_err(|error| ProtocolError::Io(error.to_string()))
}

fn ack_kind_name(ack: &AckKind) -> &'static str {
    match ack {
        AckKind::Started => "started",
        AckKind::Ready { .. } => "ready",
        AckKind::Stopped { .. } => "stopped",
        AckKind::Flushed { .. } => "flushed",
        AckKind::Faulted { .. } => "faulted",
    }
}

fn validate_schema(actual: &str, expected: &'static str) -> Result<(), ProtocolError> {
    if actual == expected {
        Ok(())
    } else {
        Err(ProtocolError::UnsupportedSchema {
            expected,
            actual: actual.into(),
        })
    }
}

fn validate_identity(identity: &StreamIdentity) -> Result<(), ProtocolError> {
    non_empty("session_id", &identity.session_id)?;
    non_empty("run_id", &identity.run_id)?;
    if identity.generation == 0 {
        return Err(ProtocolError::Contract(
            "generation must be positive".into(),
        ));
    }
    Ok(())
}

fn non_empty(field: &str, value: &str) -> Result<(), ProtocolError> {
    if value.trim().is_empty() {
        Err(ProtocolError::Contract(format!(
            "{field} must not be empty"
        )))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ready_deadline_reserves_probe_cleanup_and_preserves_longer_waits() {
        for (schema, seconds) in [
            (crate::CAMERA_BINDING_SCHEMA_V2, 10),
            (crate::CAMERA_BINDING_SCHEMA_V3, 30),
        ] {
            assert_eq!(capture_warmup_timeout(schema), Duration::from_secs(seconds));
            assert_eq!(
                client_ready_timeout_ms(15_000, schema),
                seconds * 1_000 + 5_000
            );
            assert_eq!(client_ready_timeout_ms(60_000, schema), 60_000);
        }
    }

    #[test]
    fn baseline_present_cadence_is_independent_from_video_cfr() {
        let mut command = start(1);
        let ControlCommand::StartRun(ref mut value) = command.command else {
            unreachable!()
        };
        value.required_present_fps = Some(60);
        value.lane_fps = Some(30);
        value.performance_validation_only = false;
        assert!(command.validate().is_ok());
        for (source, video, valid) in [
            (30, Some(30), true),
            (60, Some(60), true),
            (30, Some(60), false),
            (60, None, false),
            (0, Some(30), false),
            (120, Some(30), false),
        ] {
            let ControlCommand::StartRun(ref mut value) = command.command else {
                unreachable!()
            };
            value.required_present_fps = Some(source);
            value.lane_fps = video;
            assert_eq!(
                command.validate().is_ok(),
                valid,
                "source={source}, video={video:?}"
            );
        }
        let ControlCommand::StartRun(ref mut value) = command.command else {
            unreachable!()
        };
        value.required_present_fps = Some(60);
        value.lane_fps = Some(30);
        value.performance_validation_only = true;
        assert!(command.validate().is_err());
    }

    fn identity(generation: u64) -> StreamIdentity {
        StreamIdentity {
            session_id: "s1".into(),
            run_id: format!("r{generation}"),
            generation,
        }
    }

    fn start(generation: u64) -> ControlEnvelope {
        let root = std::env::temp_dir().join(format!("dm-control-run-{generation}"));
        ControlEnvelope {
            schema: CONTROL_SCHEMA_V3.into(),
            command_id: format!("start-{generation}"),
            identity: identity(generation),
            command: ControlCommand::StartRun(StartRun {
                target_process_id: 4242,
                output_dir: root.to_string_lossy().into_owned(),
                unbind_from_end: 4,
                max_frames: None,
                target_depth_hz: 15.0,
                lane_fps: Some(30),
                required_present_fps: None,
                depth_profile_path: root.join("profile.json").to_string_lossy().into_owned(),
                depth_profile_sha256: format!("sha256:{}", "a".repeat(64)),
                camera_binding_path: root.join("binding.json").to_string_lossy().into_owned(),
                camera_binding_sha256: format!("sha256:{}", "b".repeat(64)),
                pass_identity_trace: false,
                performance_metrics_enabled: false,
                performance_validation_only: false,
                present_only: false,
                watermark_only: false,
                writer_bypass: false,
                selected_present_feedback: None,
            }),
        }
    }

    fn profile() -> DepthProfileIdentity {
        DepthProfileIdentity {
            schema_version: "dm-depth-profile-1".into(),
            game_id: "game".into(),
            lane: "lane30".into(),
            executable_sha256: "sha256:abc".into(),
        }
    }

    fn binding() -> CameraBindingIdentity {
        CameraBindingIdentity {
            schema: "camera-binding/v1".into(),
            binding_id: "binding".into(),
            binding_version: "1".into(),
            profile_id: "game".into(),
            profile_version: "dm-depth-profile-1".into(),
            executable_sha256: "sha256:abc".into(),
        }
    }

    fn adapter() -> GpuAdapterIdentity {
        GpuAdapterIdentity {
            description: "NVIDIA GeForce RTX 5070 Ti Laptop GPU".into(),
            driver_version: "32.0.15.9227".into(),
            luid_high: 7,
            luid_low: 42,
        }
    }

    fn ack(command: &str, generation: u64, ordinal: u32, kind: AckKind) -> ProbeAck {
        ProbeAck {
            schema: ACK_SCHEMA_V3.into(),
            command_id: command.into(),
            identity: identity(generation),
            ordinal,
            ack: kind,
        }
    }

    #[test]
    fn lifecycle_ack_v1_is_rejected_after_latency_evidence_schema_change() {
        let mut old = ack("start", 1, 1, AckKind::Started);
        old.schema = "depth-probe-ack/v1".into();
        assert!(matches!(
            old.validate(),
            Err(ProtocolError::UnsupportedSchema {
                expected: ACK_SCHEMA_V3,
                ..
            })
        ));
    }

    #[test]
    fn control_v2_is_rejected_without_the_validation_only_contract() {
        let mut old = start(1);
        old.schema = "depth-probe-control/v2".into();

        assert!(matches!(
            old.validate(),
            Err(ProtocolError::UnsupportedSchema {
                expected: CONTROL_SCHEMA_V3,
                ..
            })
        ));
    }

    #[test]
    fn pass_identity_trace_is_opt_in_and_old_start_commands_remain_compatible() {
        let normal = start(1);
        let normal_json = serde_json::to_string(&normal).unwrap();
        assert!(!normal_json.contains("pass_identity_trace"));

        let mut traced = start(2);
        let ControlCommand::StartRun(start) = &mut traced.command else {
            unreachable!()
        };
        start.pass_identity_trace = true;
        let traced_json = serde_json::to_string(&traced).unwrap();
        assert!(traced_json.contains("\"pass_identity_trace\":true"));

        let old_json = normal_json.replace(",\"pass_identity_trace\":false", "");
        let decoded: ControlEnvelope = serde_json::from_str(&old_json).unwrap();
        let ControlCommand::StartRun(start) = decoded.command else {
            unreachable!()
        };
        assert!(!start.pass_identity_trace);
    }

    #[test]
    fn watermark_only_rejects_selected_present_feedback_without_depth_candidates() {
        let mut command = start(3);
        let ControlCommand::StartRun(start) = &mut command.command else {
            unreachable!()
        };
        start.performance_validation_only = true;
        start.watermark_only = true;
        start.selected_present_feedback = Some(SelectedPresentFeedbackBootstrap {
            client_process_id: 7,
            nonce: "watermark-only".into(),
            resource_epoch: 1,
            queue_capacity: 96,
            ring_capacity: 96,
            max_feedback_lag_presents: 88,
            guard_slots: 8,
            width: 1280,
            height: 720,
            bytes_per_pixel: 4,
            vram_limit_bytes: 512 * 1024 * 1024,
            vram_reserve_bytes: 256 * 1024 * 1024,
        });

        let error = command.validate().unwrap_err();
        assert!(
            error
                .to_string()
                .contains("does not materialize depth candidates"),
            "{error}"
        );
    }

    #[test]
    fn watermark_only_is_always_non_deliverable_validation() {
        let mut command = start(4);
        let ControlCommand::StartRun(start) = &mut command.command else {
            unreachable!()
        };
        start.watermark_only = true;

        let error = command.validate().unwrap_err();
        assert!(
            error
                .to_string()
                .contains("restricted to performance-validation-only"),
            "{error}"
        );
    }

    #[test]
    fn present_only_requires_metrics_validation_and_depth_bypass() {
        let mut command = start(5);
        let ControlCommand::StartRun(start) = &mut command.command else {
            unreachable!()
        };
        start.present_only = true;
        let error = command.validate().unwrap_err();
        assert!(
            error.to_string().contains("performance-validation-only"),
            "{error}"
        );

        let ControlCommand::StartRun(start) = &mut command.command else {
            unreachable!()
        };
        start.performance_validation_only = true;
        start.performance_metrics_enabled = true;
        let error = command.validate().unwrap_err();
        assert!(
            error.to_string().contains("bypass the depth path"),
            "{error}"
        );

        let ControlCommand::StartRun(start) = &mut command.command else {
            unreachable!()
        };
        start.watermark_only = true;
        command.validate().unwrap();
    }

    #[test]
    fn writer_bypass_requires_validation_and_selected_present_readback() {
        let mut command = start(6);
        let ControlCommand::StartRun(start) = &mut command.command else {
            unreachable!()
        };
        start.writer_bypass = true;
        let error = command.validate().unwrap_err();
        assert!(
            error
                .to_string()
                .contains("restricted to performance-validation-only"),
            "{error}"
        );

        let ControlCommand::StartRun(start) = &mut command.command else {
            unreachable!()
        };
        start.performance_validation_only = true;
        let error = command.validate().unwrap_err();
        assert!(
            error.to_string().contains("requires selected-Present"),
            "{error}"
        );

        let ControlCommand::StartRun(start) = &mut command.command else {
            unreachable!()
        };
        start.selected_present_feedback = Some(SelectedPresentFeedbackBootstrap {
            client_process_id: 7,
            nonce: "writer-bypass".into(),
            resource_epoch: 1,
            queue_capacity: 96,
            ring_capacity: 96,
            max_feedback_lag_presents: 88,
            guard_slots: 8,
            width: 1280,
            height: 720,
            bytes_per_pixel: 4,
            vram_limit_bytes: 512 * 1024 * 1024,
            vram_reserve_bytes: 256 * 1024 * 1024,
        });
        command.validate().unwrap();
    }

    #[test]
    fn ready_accepts_v2_pair_but_rejects_mixed_schema_generations() {
        for (profile_schema, binding_schema, expected) in [
            ("dm-depth-profile-1", "camera-binding/v1", true),
            ("dm-depth-profile-2", "camera-binding/v2", true),
            (
                "dm-depth-profile-2",
                "camera-binding/cp2077-holstered-v3",
                true,
            ),
            (
                "dm-depth-profile-3",
                "camera-binding/cp2077-holstered-v3",
                true,
            ),
            (
                "dm-depth-profile-1",
                "camera-binding/cp2077-holstered-v3",
                false,
            ),
            (
                "dm-depth-profile-3",
                "camera-binding/kcd2-combined-v1",
                true,
            ),
            (
                "dm-depth-profile-3",
                "camera-binding/kcd2-combined-v2",
                true,
            ),
            (
                "dm-depth-profile-2",
                "camera-binding/kcd2-combined-v2",
                false,
            ),
            ("dm-depth-profile-3", "camera-binding/v2", false),
            (
                "dm-depth-profile-2",
                "camera-binding/kcd2-combined-v1",
                false,
            ),
            ("dm-depth-profile-2", "camera-binding/v1", false),
            ("dm-depth-profile-1", "camera-binding/v2", false),
        ] {
            let mut actual_profile = profile();
            actual_profile.schema_version = profile_schema.into();
            let mut actual_binding = binding();
            actual_binding.schema = binding_schema.into();
            actual_binding.profile_version = profile_schema.into();
            let ready = ack(
                "start-1",
                1,
                2,
                AckKind::Ready {
                    actual_profile,
                    actual_binding,
                    depth_profile_sha256: format!("sha256:{}", "a".repeat(64)),
                    camera_binding_sha256: format!("sha256:{}", "b".repeat(64)),
                    probe_version: "0.1.0".into(),
                    actual_swapchain_adapter: adapter(),
                    first_present_seq: 10,
                    resource_epoch: 1,
                    performance_metrics_enabled: false,
                },
            );
            assert_eq!(
                ready.validate().is_ok(),
                expected,
                "{profile_schema}/{binding_schema}"
            );
        }
    }

    #[test]
    fn kcd2_catalog_ready_survives_disk_ack_and_client_lifecycle() {
        let profile: serde_json::Value = serde_json::from_str(include_str!(
            "../../../config/catalogs/dev/kingdom-come-deliverance-2-steam/depth-profile.json"
        ))
        .unwrap();
        let binding: serde_json::Value = serde_json::from_str(include_str!(
            "../../../config/catalogs/dev/kingdom-come-deliverance-2-steam/camera-binding.json"
        ))
        .unwrap();
        let value =
            |document: &serde_json::Value, key: &str| document[key].as_str().unwrap().to_owned();
        let ready = ack(
            "start-1",
            1,
            2,
            AckKind::Ready {
                actual_profile: DepthProfileIdentity {
                    schema_version: value(&profile, "schema_version"),
                    game_id: value(&profile, "game_id"),
                    lane: value(&profile, "lane"),
                    executable_sha256: value(&profile, "executable_sha256"),
                },
                actual_binding: CameraBindingIdentity {
                    schema: value(&binding, "schema"),
                    binding_id: value(&binding, "binding_id"),
                    binding_version: value(&binding, "binding_version"),
                    profile_id: value(&binding, "profile_id"),
                    profile_version: value(&binding, "profile_version"),
                    executable_sha256: value(&binding, "executable_sha256"),
                },
                depth_profile_sha256: format!("sha256:{}", "a".repeat(64)),
                camera_binding_sha256: format!("sha256:{}", "b".repeat(64)),
                probe_version: "depth-2026.09.4".into(),
                actual_swapchain_adapter: adapter(),
                first_present_seq: 10,
                resource_epoch: 1,
                performance_metrics_enabled: false,
            },
        );
        let root = std::env::temp_dir().join(format!(
            "dm-kcd2-ready-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        write_ack(&root, &ack("start-1", 1, 1, AckKind::Started)).unwrap();
        write_ack(&root, &ready).unwrap();
        let mut client = ClientLifecycle::default();
        client.begin_start(&start(1)).unwrap();
        for persisted in read_acks(&root).unwrap() {
            client.apply_ack(&persisted).unwrap();
        }
        assert_eq!(client.state(), ClientLifecycleState::Ready);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn recording_is_not_ready_until_started_and_ready_are_confirmed() {
        let mut client = ClientLifecycle::default();
        client.begin_start(&start(1)).unwrap();
        assert_eq!(client.state(), ClientLifecycleState::Starting);
        assert!(
            client
                .apply_ack(&ack(
                    "start-1",
                    1,
                    1,
                    AckKind::Ready {
                        actual_profile: profile(),
                        actual_binding: binding(),
                        depth_profile_sha256: format!("sha256:{}", "a".repeat(64)),
                        camera_binding_sha256: format!("sha256:{}", "b".repeat(64)),
                        probe_version: "0.1.0".into(),
                        actual_swapchain_adapter: adapter(),
                        first_present_seq: 10,
                        resource_epoch: 1,
                        performance_metrics_enabled: false,
                    }
                ))
                .is_err()
        );
        client
            .apply_ack(&ack("start-1", 1, 1, AckKind::Started))
            .unwrap();
        client
            .apply_ack(&ack(
                "start-1",
                1,
                2,
                AckKind::Ready {
                    actual_profile: profile(),
                    actual_binding: binding(),
                    depth_profile_sha256: format!("sha256:{}", "a".repeat(64)),
                    camera_binding_sha256: format!("sha256:{}", "b".repeat(64)),
                    probe_version: "0.1.0".into(),
                    actual_swapchain_adapter: adapter(),
                    first_present_seq: 10,
                    resource_epoch: 1,
                    performance_metrics_enabled: false,
                },
            ))
            .unwrap();
        assert_eq!(client.state(), ClientLifecycleState::Ready);
    }

    #[test]
    fn rejected_ready_advances_ordinal_so_stop_cleanup_can_reach_terminal_fault() {
        let mut client = ClientLifecycle::default();
        client.begin_start(&start(1)).unwrap();
        client
            .apply_ack(&ack("start-1", 1, 1, AckKind::Started))
            .unwrap();
        let ready = ack(
            "start-1",
            1,
            2,
            AckKind::Ready {
                actual_profile: profile(),
                actual_binding: binding(),
                depth_profile_sha256: format!("sha256:{}", "a".repeat(64)),
                camera_binding_sha256: format!("sha256:{}", "b".repeat(64)),
                probe_version: "wrong-provenance".into(),
                actual_swapchain_adapter: adapter(),
                first_present_seq: 10,
                resource_epoch: 1,
                performance_metrics_enabled: false,
            },
        );
        client.consume_rejected_ready(&ready).unwrap();
        assert_eq!(client.state(), ClientLifecycleState::Started);
        assert_eq!(client.last_ordinal(), 2);

        let stop = ControlEnvelope {
            schema: CONTROL_SCHEMA_V3.into(),
            command_id: "stop-after-rejected-ready".into(),
            identity: identity(1),
            command: ControlCommand::StopRun(StopRun {
                cutoff_qpc: 99,
                reason: "ready_identity_mismatch".into(),
            }),
        };
        client.begin_stop(&stop).unwrap();
        client
            .apply_ack(&ack(
                "stop-after-rejected-ready",
                1,
                3,
                AckKind::Faulted {
                    phase: "lifecycle".into(),
                    code: "stop_before_generation_sealed".into(),
                    message: "startup cleanup complete".into(),
                    summary: None,
                },
            ))
            .unwrap();
        assert_eq!(client.state(), ClientLifecycleState::Faulted);
    }

    #[test]
    fn start_bound_terminal_fault_that_won_the_stop_race_is_consumed() {
        let mut client = ClientLifecycle::default();
        client.begin_start(&start(1)).unwrap();
        client
            .apply_ack(&ack("start-1", 1, 1, AckKind::Started))
            .unwrap();
        client
            .apply_ack(&ack(
                "start-1",
                1,
                2,
                AckKind::Ready {
                    actual_profile: profile(),
                    actual_binding: binding(),
                    depth_profile_sha256: format!("sha256:{}", "a".repeat(64)),
                    camera_binding_sha256: format!("sha256:{}", "b".repeat(64)),
                    probe_version: "0.1.0".into(),
                    actual_swapchain_adapter: adapter(),
                    first_present_seq: 10,
                    resource_epoch: 1,
                    performance_metrics_enabled: false,
                },
            ))
            .unwrap();
        let stop = ControlEnvelope {
            schema: CONTROL_SCHEMA_V3.into(),
            command_id: "stop-1".into(),
            identity: identity(1),
            command: ControlCommand::StopRun(StopRun {
                cutoff_qpc: 99,
                reason: "pause".into(),
            }),
        };
        client.begin_stop(&stop).unwrap();

        client
            .apply_ack(&ack(
                "start-1",
                1,
                3,
                AckKind::Faulted {
                    phase: "feedback".into(),
                    code: "ring_overwrite_unresolved".into(),
                    message: "terminal Start fault emitted before probe observed Stop".into(),
                    summary: None,
                },
            ))
            .unwrap();
        assert_eq!(client.state(), ClientLifecycleState::Faulted);
    }

    #[test]
    fn stopping_still_rejects_foreign_start_fault_and_bad_ordinal() {
        let mut client = ClientLifecycle::default();
        client.begin_start(&start(1)).unwrap();
        client
            .apply_ack(&ack("start-1", 1, 1, AckKind::Started))
            .unwrap();
        let stop = ControlEnvelope {
            schema: CONTROL_SCHEMA_V3.into(),
            command_id: "stop-1".into(),
            identity: identity(1),
            command: ControlCommand::StopRun(StopRun {
                cutoff_qpc: 99,
                reason: "pause".into(),
            }),
        };
        client.begin_stop(&stop).unwrap();
        let fault = |command: &str, generation, ordinal| {
            ack(
                command,
                generation,
                ordinal,
                AckKind::Faulted {
                    phase: "feedback".into(),
                    code: "ring_overwrite_unresolved".into(),
                    message: "fault".into(),
                    summary: None,
                },
            )
        };
        assert_eq!(
            client.apply_ack(&fault("foreign-start", 1, 2)),
            Err(ProtocolError::CommandMismatch)
        );
        assert!(matches!(
            client.apply_ack(&fault("start-1", 1, 3)),
            Err(ProtocolError::AckOrder { .. })
        ));
        assert!(matches!(
            client.apply_ack(&fault("start-1", 2, 2)),
            Err(ProtocolError::IdentityMismatch)
        ));
    }

    #[test]
    fn stop_requires_zero_queue_matching_cutoff_and_no_writer_errors() {
        let mut client = ClientLifecycle::default();
        client.begin_start(&start(1)).unwrap();
        client
            .apply_ack(&ack("start-1", 1, 1, AckKind::Started))
            .unwrap();
        client
            .apply_ack(&ack(
                "start-1",
                1,
                2,
                AckKind::Ready {
                    actual_profile: profile(),
                    actual_binding: binding(),
                    depth_profile_sha256: format!("sha256:{}", "a".repeat(64)),
                    camera_binding_sha256: format!("sha256:{}", "b".repeat(64)),
                    probe_version: "v".into(),
                    actual_swapchain_adapter: adapter(),
                    first_present_seq: 1,
                    resource_epoch: 1,
                    performance_metrics_enabled: false,
                },
            ))
            .unwrap();
        let stop = ControlEnvelope {
            schema: CONTROL_SCHEMA_V3.into(),
            command_id: "stop-1".into(),
            identity: identity(1),
            command: ControlCommand::StopRun(StopRun {
                cutoff_qpc: 99,
                reason: "pause".into(),
            }),
        };
        client.begin_stop(&stop).unwrap();
        assert!(
            client
                .apply_ack(&ack(
                    "stop-1",
                    1,
                    3,
                    AckKind::Stopped {
                        cutoff_qpc: 100,
                        cutoff_present_seq: 12,
                    },
                ))
                .is_err()
        );
        client
            .apply_ack(&ack(
                "stop-1",
                1,
                3,
                AckKind::Stopped {
                    cutoff_qpc: 99,
                    cutoff_present_seq: 12,
                },
            ))
            .unwrap();
        let mut bad = FlushSummary {
            writer_queue_depth: 1,
            last_present_seq: 12,
            cutoff_present_seq: 12,
            depth_row_count: 2,
            depth_file_count: 2,
            camera_pose_row_count: 1,
            camera_diagnostic_row_count: 11,
            player_pose_row_count: None,
            selected_present: None,
            writer_bypass: false,
            writer_bypassed_frames: 0,
            writer_timing: None,
            performance_metrics_enabled: false,
            present_timing: None,
            errors: Vec::new(),
        };
        assert!(
            client
                .apply_ack(&ack(
                    "stop-1",
                    1,
                    4,
                    AckKind::Flushed {
                        summary: bad.clone()
                    }
                ))
                .is_err()
        );
        bad.writer_queue_depth = 0;
        bad.errors.push("disk full".into());
        assert!(matches!(
            client.apply_ack(&ack(
                "stop-1",
                1,
                4,
                AckKind::Flushed {
                    summary: bad.clone()
                }
            )),
            Err(ProtocolError::WriterErrors(_))
        ));
        bad.errors.clear();
        bad.last_present_seq = 13;
        bad.cutoff_present_seq = 13;
        assert!(
            client
                .apply_ack(&ack(
                    "stop-1",
                    1,
                    4,
                    AckKind::Flushed {
                        summary: bad.clone()
                    }
                ))
                .is_err()
        );
        bad.last_present_seq = 12;
        bad.cutoff_present_seq = 12;
        bad.camera_diagnostic_row_count = 10;
        assert!(
            client
                .apply_ack(&ack(
                    "stop-1",
                    1,
                    4,
                    AckKind::Flushed {
                        summary: bad.clone()
                    }
                ))
                .is_err()
        );
        bad.camera_diagnostic_row_count = 11;
        // A generation with no player writer omits the optional count. An
        // enabled-but-empty or incomplete stream must not get that exemption.
        for player_rows in [0, 11, 13] {
            bad.player_pose_row_count = Some(player_rows);
            let error = client
                .apply_ack(&ack(
                    "stop-1",
                    1,
                    4,
                    AckKind::Flushed {
                        summary: bad.clone(),
                    },
                ))
                .unwrap_err();
            assert!(error.to_string().contains("player pose row count"));
        }
        let mut with_player = bad.clone();
        with_player.player_pose_row_count = Some(12);
        let mut player_client = client.clone();
        player_client
            .apply_ack(&ack(
                "stop-1",
                1,
                4,
                AckKind::Flushed {
                    summary: with_player,
                },
            ))
            .unwrap();
        bad.player_pose_row_count = None;
        client
            .apply_ack(&ack("stop-1", 1, 4, AckKind::Flushed { summary: bad }))
            .unwrap();
        assert_eq!(client.state(), ClientLifecycleState::Flushed);
        assert!(client.confirmed_flush().is_some());
        assert!(
            client
                .apply_ack(&ack(
                    "stop-1",
                    1,
                    5,
                    AckKind::Faulted {
                        phase: "late".into(),
                        code: "late_callback".into(),
                        message: "must not regress Flushed".into(),
                        summary: None,
                    },
                ))
                .is_err()
        );
        assert_eq!(client.state(), ClientLifecycleState::Flushed);
    }

    #[test]
    fn empty_pose_flush_is_only_accepted_for_explicit_presentation_validation() {
        for stage in 0..4 {
            let mut command = start(1);
            let ControlCommand::StartRun(ref mut config) = command.command else {
                unreachable!()
            };
            config.performance_metrics_enabled = true;
            config.performance_validation_only = true;
            config.present_only = stage == 0;
            config.watermark_only = stage < 2;
            config.writer_bypass = stage == 2;
            if stage >= 2 {
                config.selected_present_feedback = Some(SelectedPresentFeedbackBootstrap {
                    client_process_id: 7,
                    nonce: "stage-shape".into(),
                    resource_epoch: 1,
                    queue_capacity: 96,
                    ring_capacity: 96,
                    max_feedback_lag_presents: 88,
                    guard_slots: 8,
                    width: 1280,
                    height: 720,
                    bytes_per_pixel: 4,
                    vram_limit_bytes: 512 * 1024 * 1024,
                    vram_reserve_bytes: 256 * 1024 * 1024,
                });
            }
            let mut client = ClientLifecycle::default();
            client.begin_start(&command).unwrap();
            client
                .apply_ack(&ack("start-1", 1, 1, AckKind::Started))
                .unwrap();
            client
                .apply_ack(&ack(
                    "start-1",
                    1,
                    2,
                    AckKind::Ready {
                        actual_profile: profile(),
                        actual_binding: binding(),
                        depth_profile_sha256: format!("sha256:{}", "a".repeat(64)),
                        camera_binding_sha256: format!("sha256:{}", "b".repeat(64)),
                        probe_version: "test".into(),
                        actual_swapchain_adapter: adapter(),
                        first_present_seq: 1,
                        resource_epoch: 1,
                        performance_metrics_enabled: true,
                    },
                ))
                .unwrap();
            client
                .begin_stop(&ControlEnvelope {
                    schema: CONTROL_SCHEMA_V3.into(),
                    command_id: "stop-1".into(),
                    identity: identity(1),
                    command: ControlCommand::StopRun(StopRun {
                        cutoff_qpc: 2020,
                        reason: "pause".into(),
                    }),
                })
                .unwrap();
            client
                .apply_ack(&ack(
                    "stop-1",
                    1,
                    3,
                    AckKind::Stopped {
                        cutoff_qpc: 2020,
                        cutoff_present_seq: 101,
                    },
                ))
                .unwrap();
            let summary = FlushSummary {
                writer_queue_depth: 0,
                last_present_seq: 101,
                cutoff_present_seq: 101,
                depth_row_count: 0,
                depth_file_count: 0,
                camera_pose_row_count: 0,
                camera_diagnostic_row_count: 0,
                player_pose_row_count: None,
                selected_present: None,
                writer_bypass: stage == 2,
                writer_bypassed_frames: 0,
                writer_timing: None,
                performance_metrics_enabled: true,
                present_timing: Some(PresentTimingSummary {
                    sample_count: 101,
                    qpc_frequency: 1000,
                    qpc_start: 10,
                    qpc_end: 2010,
                    measurement_window_start_qpc: 10,
                    measurement_cutoff_qpc: 2010,
                    stop_cutoff_qpc: 2020,
                    avg_millifps: 50000,
                    low1_millifps: 50000,
                    slowest_one_percent: vec![PresentIntervalEvidence {
                        start_present_seq: 1,
                        end_present_seq: 2,
                        start_qpc: 10,
                        end_qpc: 30,
                        duration_qpc: 20,
                    }],
                    wrapped: false,
                }),
                errors: Vec::new(),
            };
            if stage < 2 {
                let mut missing_timing = summary.clone();
                missing_timing.present_timing = None;
                assert!(
                    client
                        .apply_ack(&ack(
                            "stop-1",
                            1,
                            4,
                            AckKind::Flushed {
                                summary: missing_timing
                            }
                        ))
                        .is_err()
                );
                let mut unexpected_depth = summary.clone();
                unexpected_depth.depth_row_count = 1;
                unexpected_depth.depth_file_count = 1;
                assert!(
                    client
                        .apply_ack(&ack(
                            "stop-1",
                            1,
                            4,
                            AckKind::Flushed {
                                summary: unexpected_depth
                            }
                        ))
                        .is_err()
                );
            }
            let result = client.apply_ack(&ack("stop-1", 1, 4, AckKind::Flushed { summary }));
            assert_eq!(result.is_ok(), stage < 2, "stage={stage}: {result:?}");
        }
    }

    #[test]
    fn selected_present_flush_requires_exact_end_to_end_counts() {
        let selected = SelectedPresentFlushSummary {
            generation: 7,
            resource_epoch: 2,
            first_candidate_present_seq: Some(100),
            materialized_candidates: 120,
            selected_frame_count: 31,
            distinct_selected_depth_count: 29,
            source_identity_alias_count: 2,
            readback_submitted_count: 29,
            readback_completed_count: 29,
            unresolved_candidates: 0,
            ring_overwrite_count: 0,
            max_unresolved_candidates: 80,
            max_feedback_lag_presents: 70,
            feedback_latency: SelectedPresentLatencySummary {
                sample_count: 31,
                selected_lag_presents: LatencyQuantiles {
                    p50: 20,
                    p95: 60,
                    p99: 70,
                    max: 70,
                },
                wgc_delivery_sample_count: 30,
                wgc_delivery_us: LatencyQuantiles {
                    p50: 1_000,
                    p95: 2_000,
                    p99: 3_000,
                    max: 4_000,
                },
                wgc_source_ahead_callback_ready_count: 1,
                wgc_source_ahead_callback_ready_us: LatencyQuantiles {
                    p50: 470,
                    p95: 470,
                    p99: 470,
                    max: 470,
                },
                callback_to_decision_us: LatencyQuantiles {
                    p50: 2_000,
                    p95: 5_000,
                    p99: 7_000,
                    max: 9_000,
                },
                source_to_probe_sample_count: 30,
                source_to_probe_us: LatencyQuantiles {
                    p50: 4_000,
                    p95: 8_000,
                    p99: 11_000,
                    max: 13_000,
                },
                // The one sample whose compositor clock still led the probe
                // read. This used to be no distribution at all: the run
                // stopped on it.
                wgc_source_ahead_process_count: 1,
                wgc_source_ahead_process_us: LatencyQuantiles {
                    p50: 105,
                    p95: 105,
                    p99: 105,
                    max: 105,
                },
                decision_to_probe_us: LatencyQuantiles {
                    p50: 1_000,
                    p95: 2_000,
                    p99: 3_000,
                    max: 4_000,
                },
            },
            gpu_allocation: Some(CandidateGpuAllocationSummary {
                capacity: 96,
                texture_bytes: 3_686_400,
                estimated_bytes: 357_580_800,
                configured_limit_bytes: 512 * 1024 * 1024,
                configured_reserve_bytes: 256 * 1024 * 1024,
                budget_before: 8 * 1024 * 1024 * 1024,
                usage_before: 2 * 1024 * 1024 * 1024,
                headroom_before: 6 * 1024 * 1024 * 1024,
                budget_after: 8 * 1024 * 1024 * 1024,
                usage_after: 2_500 * 1024 * 1024,
                headroom_after: 5_692 * 1024 * 1024,
                actual_usage_delta: 500 * 1024 * 1024,
                budget_at_flush: Some(8 * 1024 * 1024 * 1024),
                usage_at_flush: Some(2_750 * 1024 * 1024),
                headroom_at_flush: Some(5_442 * 1024 * 1024),
            }),
            feedback_queue_depth: 0,
            ack_queue_depth: 0,
            feedback_queue_max_depth: 12,
            ack_queue_max_depth: 4,
            feedback_queue_full_count: 0,
            ack_queue_full_count: 0,
            transport_fault_reason: 0,
        };
        let mut summary = FlushSummary {
            writer_queue_depth: 0,
            last_present_seq: 200,
            cutoff_present_seq: 200,
            depth_row_count: 29,
            depth_file_count: 29,
            camera_pose_row_count: 31,
            camera_diagnostic_row_count: 169,
            player_pose_row_count: Some(31),
            selected_present: Some(selected),
            writer_bypass: false,
            writer_bypassed_frames: 0,
            writer_timing: None,
            performance_metrics_enabled: false,
            present_timing: None,
            errors: Vec::new(),
        };
        summary.validate().unwrap();
        let mut unrelated_game_growth = summary.clone();
        let allocation = unrelated_game_growth
            .selected_present
            .as_mut()
            .unwrap()
            .gpu_allocation
            .as_mut()
            .unwrap();
        allocation.usage_at_flush = Some(7 * 1024 * 1024 * 1024);
        allocation.headroom_at_flush = Some(1024 * 1024 * 1024);
        unrelated_game_growth.validate().unwrap();

        let mut excessive_allocation = summary.clone();
        let allocation = excessive_allocation
            .selected_present
            .as_mut()
            .unwrap()
            .gpu_allocation
            .as_mut()
            .unwrap();
        allocation.actual_usage_delta = 513 * 1024 * 1024;
        allocation.usage_after = allocation.usage_before + allocation.actual_usage_delta;
        allocation.headroom_after = allocation.budget_after - allocation.usage_after;
        let error = excessive_allocation.validate().unwrap_err().to_string();
        assert!(
            error.contains("attributable GPU allocation delta"),
            "{error}"
        );
        assert!(error.contains("537919488"), "{error}");

        let mut missing_flush_snapshot = summary.clone();
        missing_flush_snapshot
            .selected_present
            .as_mut()
            .unwrap()
            .gpu_allocation
            .as_mut()
            .unwrap()
            .headroom_at_flush = None;
        let error = missing_flush_snapshot.validate().unwrap_err().to_string();
        assert!(
            error.contains("no complete run-end headroom snapshot"),
            "{error}"
        );

        let normal_summary = summary.clone();
        summary.writer_bypass = true;
        summary.writer_bypassed_frames = 29;
        summary.depth_row_count = 0;
        summary.depth_file_count = 0;
        summary.writer_timing = Some(WriterTimingSummary::default());
        summary.validate().unwrap();
        summary.writer_bypassed_frames = 28;
        assert!(summary.validate().is_err());
        summary = normal_summary;

        summary
            .selected_present
            .as_mut()
            .unwrap()
            .feedback_latency
            .wgc_source_ahead_callback_ready_count = 32;
        assert!(summary.validate().is_err());
        summary
            .selected_present
            .as_mut()
            .unwrap()
            .feedback_latency
            .wgc_source_ahead_callback_ready_count = 1;

        {
            let latency = &mut summary.selected_present.as_mut().unwrap().feedback_latency;
            latency.wgc_delivery_sample_count = 31;
            latency.wgc_source_ahead_callback_ready_count = 0;
        }
        assert!(summary.validate().is_err());
        {
            let latency = &mut summary.selected_present.as_mut().unwrap().feedback_latency;
            latency.wgc_delivery_sample_count = 30;
            latency.wgc_source_ahead_callback_ready_count = 1;
            // A summary written before the probe-read split -- or one that
            // dropped a leading sample instead of counting it -- leaves this
            // pair one short of the decisions it claims to cover. Clear the
            // quantiles too, so reconciliation is the only rule left to fail.
            latency.wgc_source_ahead_process_count = 0;
            latency.wgc_source_ahead_process_us = LatencyQuantiles::default();
        }
        let error = summary.validate().unwrap_err().to_string();
        assert!(error.contains("ahead_of_probe=0"), "{error}");
        {
            let latency = &mut summary.selected_present.as_mut().unwrap().feedback_latency;
            latency.wgc_source_ahead_process_count = 1;
            latency.wgc_source_ahead_process_us = LatencyQuantiles {
                p50: 105,
                p95: 105,
                p99: 105,
                max: 105,
            };
        }
        summary.validate().unwrap();
        {
            let latency = &mut summary.selected_present.as_mut().unwrap().feedback_latency;
            latency.wgc_delivery_sample_count = 30;
            latency.wgc_source_ahead_callback_ready_count = 1;
            latency.callback_to_decision_us.p50 = 6_000;
        }
        assert!(summary.validate().is_err());
        summary
            .selected_present
            .as_mut()
            .unwrap()
            .feedback_latency
            .callback_to_decision_us
            .p50 = 2_000;

        summary
            .selected_present
            .as_mut()
            .unwrap()
            .unresolved_candidates = 1;
        assert!(summary.validate().is_err());
        summary
            .selected_present
            .as_mut()
            .unwrap()
            .unresolved_candidates = 0;
        summary
            .selected_present
            .as_mut()
            .unwrap()
            .readback_completed_count = 28;
        assert!(summary.validate().is_err());
        summary
            .selected_present
            .as_mut()
            .unwrap()
            .readback_completed_count = 29;
        summary
            .selected_present
            .as_mut()
            .unwrap()
            .source_identity_alias_count = 1;
        assert!(summary.validate().is_err());
    }

    #[test]
    fn stale_generation_wrong_identity_and_late_ack_are_rejected() {
        let mut client = ClientLifecycle::default();
        client.begin_start(&start(1)).unwrap();
        client
            .apply_ack(&ack("start-1", 1, 1, AckKind::Started))
            .unwrap();
        let mut wrong = ack("start-1", 1, 2, AckKind::Started);
        wrong.identity.session_id = "other".into();
        assert_eq!(
            client.apply_ack(&wrong),
            Err(ProtocolError::IdentityMismatch)
        );

        client.state = ClientLifecycleState::Flushed;
        client.begin_start(&start(2)).unwrap();
        assert!(matches!(
            client.apply_ack(&ack("start-1", 1, 1, AckKind::Started)),
            Err(ProtocolError::StaleGeneration { .. })
        ));
        assert!(client.begin_start(&start(1)).is_err());
    }

    #[test]
    fn fault_allows_only_a_new_generation_and_old_callbacks_stay_rejected() {
        let mut client = ClientLifecycle::default();
        client.begin_start(&start(1)).unwrap();
        client
            .apply_ack(&ack(
                "start-1",
                1,
                1,
                AckKind::Faulted {
                    phase: "start".into(),
                    code: "writer_failed".into(),
                    message: "synthetic failure".into(),
                    summary: None,
                },
            ))
            .unwrap();
        assert_eq!(client.state(), ClientLifecycleState::Faulted);
        assert!(client.begin_start(&start(1)).is_err());

        client.begin_start(&start(2)).unwrap();
        assert!(matches!(
            client.apply_ack(&ack("start-1", 1, 2, AckKind::Started)),
            Err(ProtocolError::StaleGeneration { .. })
        ));
        client
            .apply_ack(&ack("start-2", 2, 1, AckKind::Started))
            .unwrap();
        assert_eq!(client.state(), ClientLifecycleState::Started);
    }

    #[test]
    fn local_timeout_retains_identity_and_requires_a_new_generation() {
        let mut client = ClientLifecycle::default();
        client.begin_start(&start(1)).unwrap();
        client.fail_local(&identity(1)).unwrap();
        assert_eq!(client.state(), ClientLifecycleState::Faulted);
        assert!(client.begin_start(&start(1)).is_err());
        client.begin_start(&start(2)).unwrap();
        assert!(matches!(
            client.apply_ack(&ack("start-1", 1, 1, AckKind::Started)),
            Err(ProtocolError::StaleGeneration { .. })
        ));
    }

    #[test]
    fn command_and_acks_round_trip_without_partial_json() {
        let root = std::env::temp_dir().join(format!("dm-probe-control-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let command_path = root.join("control.json");
        let command = start(1);
        write_command(&command_path, &command).unwrap();
        assert_eq!(load_command_from(&command_path).unwrap(), command);
        let first = ack("start-1", 1, 1, AckKind::Started);
        write_ack(&root, &first).unwrap();
        assert_eq!(read_acks(&root).unwrap(), vec![first]);
        assert!(!root.join("control.json.tmp").exists());
        let _ = fs::remove_dir_all(root);
    }
}

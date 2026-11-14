//! 逐帧状态机:把散落到各个通道回调里的读数,攒成一条帧记录。
//!
//! **本模块与 Windows 无关**,可以在任何平台上跑测试——这是刻意的:
//! 插件里唯一有判断的部分就在这里,把它做成平台无关的,就不必为了验一条语义去开机器。
//! FFI 外壳([`crate::ffi`])只负责搬数据,不做判断。
//!
//! # 三条纪律(与 `fallout-player-telemetry` 同源)
//!
//! 1. **暂停时绝不产出 player 载荷。** 主菜单/暂停里通道停更,而上一帧的值还留在内存里,
//!    照写就是凭空捏一帧。暂停期改为产出带原因的 skip 记录,让空档**显式**而不是消失。
//! 2. **「没上报过」不等于「没变化」。** 注册用的是 `FLAG_none`,值不变时回调不触发,
//!    所以必须单独记「这个通道到底有没有报过」,只用 `Option` 的存在性判断。
//! 3. **时间基回跳要记账。** `render_time` 会因 `timer_restart` 标志重置;
//!    下游若按单调递增去 join,回跳会让整段错配。

use ets2_player_telemetry::{EulerTurns, PlayerSampleV1, RawTruckSampleV1, RollConvention};
use serde::{Deserialize, Serialize};

use crate::ffi::{
    SCS_TELEMETRY_FRAME_START_FLAG_TIMER_RESTART, ScsDplacement, ScsFplacement, ScsFvector,
    ScsTelemetryFrameStart,
};

/// 帧记录的版本。
pub const FRAME_SCHEMA_VERSION_V3: &str = "ets2-telemetry-frame/v3";

/// 一帧产出的记录。**无论有没有有效样本都会产出一条**——空档必须可见。
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FrameRecordV3 {
    pub schema_version: String,
    /// 自插件初始化起的帧序号,从 1 开始,永不回退。缺号即丢帧。
    pub frame_seq: u64,
    /// `scs_telemetry_frame_start_t::render_time`,微秒。**与截帧对齐的键。**
    pub render_time_us: u64,
    /// 时间基代号:每次引擎置 `timer_restart` 就 +1。
    /// **只有同一个 epoch 内的 `render_time_us` 才可比。**
    pub timer_epoch: u64,
    /// 与 D3D11 probe 同源的 Windows QPC 区间；用于把 SDK 位姿精确关联到同一渲染帧。
    pub host_timing: HostTimingV1,
    /// 有效样本;`None` 时看 [`Self::skip_reason`]。
    pub player: Option<PlayerSampleV1>,
    /// 官方 SDK 的座舱/头部局部偏移，仅作为 D3D11 Camera hook 的独立交叉验证证据。
    /// 它不是最终相机位姿，也不用于替代 hook 交付。
    pub camera_cross_check: CameraCrossCheckV1,
    /// 无有效样本的原因。有 `player` 时为 `None`。
    pub skip_reason: Option<SkipReasonV1>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum HostTimingV1 {
    Available {
        frame_start_qpc: i64,
        frame_end_qpc: i64,
        qpc_frequency_hz: i64,
    },
    Unavailable,
    InvalidNonMonotonic,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CameraCrossCheckV1 {
    pub source: CameraCrossCheckSourceV1,
    pub cabin_default_position: CameraBasePositionSampleV1,
    pub head_default_position: CameraBasePositionSampleV1,
    pub cabin_offset: CameraOffsetSampleV1,
    pub head_offset: CameraOffsetSampleV1,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CameraCrossCheckSourceV1 {
    ScsTelemetrySdk114,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "status", content = "value", rename_all = "snake_case")]
pub enum CameraOffsetSampleV1 {
    Available(CameraOffsetV1),
    MissingSinceStartedOrPause,
    InvalidNonFinite,
    Paused,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "status", content = "position_m", rename_all = "snake_case")]
pub enum CameraBasePositionSampleV1 {
    Available([f32; 3]),
    ConfigurationNotSeen,
    MissingAttribute,
    InvalidNonFinite,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CameraOffsetV1 {
    /// SCS 官方约定：局部坐标 X 向右、Y 向上、Z 向后，单位米。
    pub position_m: [f32; 3],
    /// SCS `fplacement` 原始 heading/pitch/roll，单位均为圈。
    pub heading_pitch_roll_turns: [f32; 3],
}

/// 该帧没有产出有效样本的原因。**每一条都必须是可判读的事实,不能是「未知」兜底。**
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReasonV1 {
    /// 遥测处于暂停态(主菜单、暂停菜单、尚未 `started`)。
    Paused,
    /// `truck.world.placement` 从未上报过——位姿无从谈起。
    NoWorldPlacement,
    /// 速度的两个来源(`local.velocity.linear` 与 `truck.speed`)都没报过。
    /// 不允许用 0 顶替:静止与「不知道」是两件事。
    NoSpeedSource,
    /// 换算被拒(欧拉角越界、非有限值等)。原始读数不合法,不是我们算错。
    ConversionRejected,
    /// 该帧没收到 `frame_start`,没有对齐键。
    NoFrameStart,
}

/// 逐帧攒数据的状态机。
#[derive(Debug, Default)]
pub struct FrameAccumulator {
    running: bool,
    frame_seq: u64,
    timer_epoch: u64,
    render_time_us: Option<u64>,
    frame_start_qpc: Option<i64>,
    last_render_time_us: Option<u64>,
    world_placement: Option<ScsDplacement>,
    local_velocity: Option<ScsFvector>,
    speedometer_mps: Option<f32>,
    cabin_offset: Option<ScsFplacement>,
    head_offset: Option<ScsFplacement>,
    truck_configuration_seen: bool,
    cabin_default_position: Option<ScsFvector>,
    head_default_position: Option<ScsFvector>,
    /// 未置 `timer_restart` 却出现 `render_time` 回退的次数。
    /// 引擎不该这么干;真发生了要能看见,而不是被当成正常数据吃掉。
    unflagged_time_regressions: u64,
}

impl FrameAccumulator {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// `SCS_TELEMETRY_EVENT_started`。
    pub fn on_started(&mut self) {
        self.running = true;
    }

    /// `SCS_TELEMETRY_EVENT_paused`。
    ///
    /// **同时清掉所有通道缓存**:恢复之后,任何一个通道在再次上报之前都应算「没报过」,
    /// 否则暂停前的位姿会被当成恢复后的第一帧写出去。
    pub fn on_paused(&mut self) {
        self.running = false;
        self.world_placement = None;
        self.local_velocity = None;
        self.speedometer_mps = None;
        self.cabin_offset = None;
        self.head_offset = None;
    }

    /// `SCS_TELEMETRY_EVENT_frame_start`。
    pub fn on_frame_start(&mut self, frame_start: &ScsTelemetryFrameStart) {
        self.on_frame_start_with_qpc(frame_start, None);
    }

    pub fn on_frame_start_with_qpc(
        &mut self,
        frame_start: &ScsTelemetryFrameStart,
        sample_qpc: Option<i64>,
    ) {
        if frame_start.flags & SCS_TELEMETRY_FRAME_START_FLAG_TIMER_RESTART != 0 {
            self.timer_epoch += 1;
            self.last_render_time_us = None;
        } else if let Some(previous) = self.last_render_time_us
            && frame_start.render_time < previous
        {
            self.unflagged_time_regressions += 1;
        }
        self.last_render_time_us = Some(frame_start.render_time);
        self.render_time_us = Some(frame_start.render_time);
        self.frame_start_qpc = sample_qpc;
    }

    pub fn on_world_placement(&mut self, value: ScsDplacement) {
        self.world_placement = Some(value);
    }

    pub fn on_local_linear_velocity(&mut self, value: ScsFvector) {
        self.local_velocity = Some(value);
    }

    pub fn on_speedometer(&mut self, value: f32) {
        self.speedometer_mps = Some(value);
    }

    pub fn on_cabin_offset(&mut self, value: ScsFplacement) {
        self.cabin_offset = Some(value);
    }

    pub fn on_head_offset(&mut self, value: ScsFplacement) {
        self.head_offset = Some(value);
    }

    pub fn on_truck_configuration(
        &mut self,
        cabin_default_position: Option<ScsFvector>,
        head_default_position: Option<ScsFvector>,
    ) {
        self.truck_configuration_seen = true;
        self.cabin_default_position = cabin_default_position;
        self.head_default_position = head_default_position;
    }

    /// 未置标志却回退的次数,供关机时写进统计。
    #[must_use]
    pub const fn unflagged_time_regressions(&self) -> u64 {
        self.unflagged_time_regressions
    }

    /// `SCS_TELEMETRY_EVENT_frame_end`:产出这一帧的记录。
    ///
    /// 帧号在**任何**分支下都会 +1(包括 skip),所以下游看到的序号是连续的,
    /// 「这一帧没数据」与「这一帧丢了」可以区分开。
    pub fn on_frame_end(&mut self, roll_convention: RollConvention) -> FrameRecordV3 {
        self.on_frame_end_with_qpc(roll_convention, None, None)
    }

    pub fn on_frame_end_with_qpc(
        &mut self,
        roll_convention: RollConvention,
        frame_end_qpc: Option<i64>,
        qpc_frequency_hz: Option<i64>,
    ) -> FrameRecordV3 {
        self.frame_seq += 1;
        let frame_seq = self.frame_seq;
        let timer_epoch = self.timer_epoch;
        let camera_cross_check = self.camera_cross_check();
        let host_timing = match (self.frame_start_qpc.take(), frame_end_qpc, qpc_frequency_hz) {
            (Some(start), Some(end), Some(frequency))
                if start > 0 && end >= start && frequency > 0 =>
            {
                HostTimingV1::Available {
                    frame_start_qpc: start,
                    frame_end_qpc: end,
                    qpc_frequency_hz: frequency,
                }
            }
            (Some(start), Some(end), Some(frequency)) if start > 0 && end > 0 && frequency > 0 => {
                HostTimingV1::InvalidNonMonotonic
            }
            _ => HostTimingV1::Unavailable,
        };

        let Some(render_time_us) = self.render_time_us.take() else {
            return Self::skipped(
                frame_seq,
                0,
                timer_epoch,
                host_timing,
                camera_cross_check,
                SkipReasonV1::NoFrameStart,
            );
        };

        if !self.running {
            return Self::skipped(
                frame_seq,
                render_time_us,
                timer_epoch,
                host_timing,
                camera_cross_check,
                SkipReasonV1::Paused,
            );
        }
        let Some(placement) = self.world_placement else {
            return Self::skipped(
                frame_seq,
                render_time_us,
                timer_epoch,
                host_timing,
                camera_cross_check,
                SkipReasonV1::NoWorldPlacement,
            );
        };
        if self.local_velocity.is_none() && self.speedometer_mps.is_none() {
            return Self::skipped(
                frame_seq,
                render_time_us,
                timer_epoch,
                host_timing,
                camera_cross_check,
                SkipReasonV1::NoSpeedSource,
            );
        }

        let raw = RawTruckSampleV1 {
            world_position_m: [
                placement.position.x,
                placement.position.y,
                placement.position.z,
            ],
            world_orientation: EulerTurns {
                heading: f64::from(placement.orientation.heading),
                pitch: f64::from(placement.orientation.pitch),
                roll: f64::from(placement.orientation.roll),
            },
            local_linear_velocity_mps: self
                .local_velocity
                .map(|v| [f64::from(v.x), f64::from(v.y), f64::from(v.z)]),
            // 走到这里至少有一个速度源；velocity 缺失时下游会取这个值并标注降级。
            speedometer_mps: f64::from(self.speedometer_mps.unwrap_or(0.0)),
        };

        match raw.to_player_sample(roll_convention) {
            Ok(player) => FrameRecordV3 {
                schema_version: FRAME_SCHEMA_VERSION_V3.to_owned(),
                frame_seq,
                render_time_us,
                timer_epoch,
                host_timing,
                player: Some(player),
                camera_cross_check,
                skip_reason: None,
            },
            // 换算被拒的具体原因(欧拉越界/非有限/契约)对下游没有可操作性差别——
            // 它们都指向「这一帧的原始读数不能用」，统一记成一个可判读的 skip。
            Err(_) => Self::skipped(
                frame_seq,
                render_time_us,
                timer_epoch,
                host_timing,
                camera_cross_check,
                SkipReasonV1::ConversionRejected,
            ),
        }
    }

    fn skipped(
        frame_seq: u64,
        render_time_us: u64,
        timer_epoch: u64,
        host_timing: HostTimingV1,
        camera_cross_check: CameraCrossCheckV1,
        reason: SkipReasonV1,
    ) -> FrameRecordV3 {
        FrameRecordV3 {
            schema_version: FRAME_SCHEMA_VERSION_V3.to_owned(),
            frame_seq,
            render_time_us,
            timer_epoch,
            host_timing,
            player: None,
            camera_cross_check,
            skip_reason: Some(reason),
        }
    }

    fn camera_cross_check(&self) -> CameraCrossCheckV1 {
        let sample = |value: Option<ScsFplacement>| {
            if !self.running {
                return CameraOffsetSampleV1::Paused;
            }
            let Some(value) = value else {
                return CameraOffsetSampleV1::MissingSinceStartedOrPause;
            };
            let position_m = [value.position.x, value.position.y, value.position.z];
            let heading_pitch_roll_turns = [
                value.orientation.heading,
                value.orientation.pitch,
                value.orientation.roll,
            ];
            if position_m
                .into_iter()
                .chain(heading_pitch_roll_turns)
                .all(f32::is_finite)
            {
                CameraOffsetSampleV1::Available(CameraOffsetV1 {
                    position_m,
                    heading_pitch_roll_turns,
                })
            } else {
                CameraOffsetSampleV1::InvalidNonFinite
            }
        };
        let base_position = |value: Option<ScsFvector>| {
            if !self.truck_configuration_seen {
                return CameraBasePositionSampleV1::ConfigurationNotSeen;
            }
            let Some(value) = value else {
                return CameraBasePositionSampleV1::MissingAttribute;
            };
            let position_m = [value.x, value.y, value.z];
            if position_m.into_iter().all(f32::is_finite) {
                CameraBasePositionSampleV1::Available(position_m)
            } else {
                CameraBasePositionSampleV1::InvalidNonFinite
            }
        };

        CameraCrossCheckV1 {
            source: CameraCrossCheckSourceV1::ScsTelemetrySdk114,
            cabin_default_position: base_position(self.cabin_default_position),
            head_default_position: base_position(self.head_default_position),
            cabin_offset: sample(self.cabin_offset),
            head_offset: sample(self.head_offset),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ets2_player_telemetry::SpeedProvenanceV1;

    fn placement_facing_north() -> ScsDplacement {
        ScsDplacement {
            position: crate::ffi::ScsDvector {
                x: 100.0,
                y: 5.0,
                z: 200.0,
            },
            orientation: crate::ffi::ScsEuler {
                heading: 0.0,
                pitch: 0.0,
                roll: 0.0,
            },
            _padding: 0,
        }
    }

    fn local_offset(x: f32, y: f32, z: f32) -> crate::ffi::ScsFplacement {
        crate::ffi::ScsFplacement {
            position: crate::ffi::ScsFvector { x, y, z },
            orientation: crate::ffi::ScsEuler {
                heading: 0.125,
                pitch: -0.025,
                roll: 0.01,
            },
        }
    }

    fn frame_start(render_time: u64) -> ScsTelemetryFrameStart {
        ScsTelemetryFrameStart {
            flags: 0,
            _padding: 0,
            render_time,
            simulation_time: render_time,
            paused_simulation_time: 0,
        }
    }

    fn running_with_data() -> FrameAccumulator {
        let mut acc = FrameAccumulator::new();
        acc.on_started();
        acc.on_world_placement(placement_facing_north());
        acc.on_local_linear_velocity(ScsFvector {
            x: 0.0,
            y: 0.0,
            z: -20.0,
        });
        acc.on_speedometer(21.0);
        acc
    }

    /// 正常一帧:有对齐键、有位姿、有速度 ⇒ 产出有效样本。
    #[test]
    fn a_complete_frame_yields_a_sample() {
        let mut acc = running_with_data();
        acc.on_frame_start(&frame_start(1_000));
        let record = acc.on_frame_end(RollConvention::Unvalidated);
        assert_eq!(record.frame_seq, 1);
        assert_eq!(record.render_time_us, 1_000);
        assert_eq!(record.skip_reason, None);
        let player = record.player.expect("应有样本");
        assert_eq!(player.position_m, [100.0, 5.0, -200.0], "Z 应翻号");
        assert_eq!(
            player.speed_provenance,
            SpeedProvenanceV1::LocalLinearVelocityMagnitude
        );
    }

    /// **暂停期绝不产出 player 载荷。**
    ///
    /// 这是本模块最重要的一条:主菜单里通道停更,而上一帧的值还在内存里。
    /// 照写出去就是把菜单时段伪装成行驶时段——一批数据里混进这种帧,
    /// 事后无法从数值上分辨。
    #[test]
    fn paused_frames_never_carry_a_stale_player_payload() {
        let mut acc = running_with_data();
        acc.on_frame_start(&frame_start(1_000));
        assert!(
            acc.on_frame_end(RollConvention::Unvalidated)
                .player
                .is_some()
        );

        acc.on_paused();
        acc.on_frame_start(&frame_start(2_000));
        let record = acc.on_frame_end(RollConvention::Unvalidated);
        assert_eq!(record.player, None);
        assert_eq!(record.skip_reason, Some(SkipReasonV1::Paused));
        assert_eq!(record.frame_seq, 2, "跳过的帧同样占一个序号");
    }

    #[test]
    fn camera_offsets_are_raw_explicit_cross_check_evidence() {
        let mut acc = running_with_data();
        acc.on_truck_configuration(
            Some(crate::ffi::ScsFvector {
                x: 0.25,
                y: 1.5,
                z: -0.75,
            }),
            Some(crate::ffi::ScsFvector {
                x: 0.1,
                y: 0.7,
                z: -0.2,
            }),
        );
        acc.on_cabin_offset(local_offset(1.0, 2.0, 3.0));
        acc.on_head_offset(local_offset(4.0, 5.0, 6.0));
        acc.on_frame_start(&frame_start(1_000));

        let record = acc.on_frame_end(RollConvention::Unvalidated);
        assert_eq!(record.schema_version, FRAME_SCHEMA_VERSION_V3);
        assert_eq!(
            record.camera_cross_check.source,
            CameraCrossCheckSourceV1::ScsTelemetrySdk114
        );
        assert_eq!(
            record.camera_cross_check.cabin_default_position,
            CameraBasePositionSampleV1::Available([0.25, 1.5, -0.75])
        );
        assert_eq!(
            record.camera_cross_check.head_default_position,
            CameraBasePositionSampleV1::Available([0.1, 0.7, -0.2])
        );
        assert_eq!(
            record.camera_cross_check.cabin_offset,
            CameraOffsetSampleV1::Available(CameraOffsetV1 {
                position_m: [1.0, 2.0, 3.0],
                heading_pitch_roll_turns: [0.125, -0.025, 0.01],
            })
        );
        assert!(matches!(
            record.camera_cross_check.head_offset,
            CameraOffsetSampleV1::Available(_)
        ));
    }

    #[test]
    fn camera_offsets_are_cleared_on_pause_and_missing_after_resume() {
        let mut acc = running_with_data();
        acc.on_truck_configuration(
            Some(crate::ffi::ScsFvector {
                x: 0.25,
                y: 1.5,
                z: -0.75,
            }),
            Some(crate::ffi::ScsFvector {
                x: 0.1,
                y: 0.7,
                z: -0.2,
            }),
        );
        acc.on_cabin_offset(local_offset(1.0, 2.0, 3.0));
        acc.on_head_offset(local_offset(4.0, 5.0, 6.0));

        acc.on_paused();
        acc.on_frame_start(&frame_start(2_000));
        let paused = acc.on_frame_end(RollConvention::Unvalidated);
        assert_eq!(
            paused.camera_cross_check.cabin_offset,
            CameraOffsetSampleV1::Paused
        );
        assert_eq!(
            paused.camera_cross_check.head_offset,
            CameraOffsetSampleV1::Paused
        );
        assert!(matches!(
            paused.camera_cross_check.cabin_default_position,
            CameraBasePositionSampleV1::Available(_)
        ));

        acc.on_started();
        acc.on_frame_start(&frame_start(3_000));
        let resumed = acc.on_frame_end(RollConvention::Unvalidated);
        assert_eq!(
            resumed.camera_cross_check.cabin_offset,
            CameraOffsetSampleV1::MissingSinceStartedOrPause
        );
        assert_eq!(
            resumed.camera_cross_check.head_offset,
            CameraOffsetSampleV1::MissingSinceStartedOrPause
        );
        assert!(matches!(
            resumed.camera_cross_check.head_default_position,
            CameraBasePositionSampleV1::Available(_)
        ));
    }

    #[test]
    fn missing_camera_configuration_is_explicit() {
        let mut acc = running_with_data();
        acc.on_frame_start(&frame_start(1_000));
        let before_configuration = acc.on_frame_end(RollConvention::Unvalidated);
        assert_eq!(
            before_configuration
                .camera_cross_check
                .cabin_default_position,
            CameraBasePositionSampleV1::ConfigurationNotSeen
        );

        acc.on_truck_configuration(None, None);
        acc.on_frame_start(&frame_start(2_000));
        let missing_attributes = acc.on_frame_end(RollConvention::Unvalidated);
        assert_eq!(
            missing_attributes.camera_cross_check.cabin_default_position,
            CameraBasePositionSampleV1::MissingAttribute
        );
        assert_eq!(
            missing_attributes.camera_cross_check.head_default_position,
            CameraBasePositionSampleV1::MissingAttribute
        );
    }

    #[test]
    fn invalid_camera_offsets_do_not_drop_the_player_record() {
        let mut acc = running_with_data();
        acc.on_cabin_offset(local_offset(f32::NAN, 2.0, 3.0));
        acc.on_head_offset(local_offset(4.0, 5.0, 6.0));
        acc.on_frame_start(&frame_start(1_000));

        let record = acc.on_frame_end(RollConvention::Unvalidated);
        assert!(record.player.is_some());
        assert_eq!(
            record.camera_cross_check.cabin_offset,
            CameraOffsetSampleV1::InvalidNonFinite
        );
        assert!(serde_json::to_string(&record).is_ok());
    }

    /// 恢复后,通道再次上报之前不算「有值」——暂停会清空缓存。
    ///
    /// 若不清:恢复瞬间会写出一条用暂停前位姿、却带着恢复后时间戳的帧。
    /// 那种帧在时间轴上正好落在传送/读档之后,是最容易污染轨迹的一种。
    #[test]
    fn resuming_requires_channels_to_report_again() {
        let mut acc = running_with_data();
        acc.on_paused();
        acc.on_started();
        acc.on_frame_start(&frame_start(3_000));
        let record = acc.on_frame_end(RollConvention::Unvalidated);
        assert_eq!(record.skip_reason, Some(SkipReasonV1::NoWorldPlacement));

        acc.on_world_placement(placement_facing_north());
        acc.on_speedometer(0.0);
        acc.on_frame_start(&frame_start(3_020));
        assert!(
            acc.on_frame_end(RollConvention::Unvalidated)
                .player
                .is_some()
        );
    }

    /// 两个速度源都没报过 ⇒ 跳过,**不拿 0 顶替**。
    /// 静止(速度真的是 0)与「不知道」必须能分开。
    #[test]
    fn missing_every_speed_source_is_a_skip_not_a_zero() {
        let mut acc = FrameAccumulator::new();
        acc.on_started();
        acc.on_world_placement(placement_facing_north());
        acc.on_frame_start(&frame_start(1_000));
        let record = acc.on_frame_end(RollConvention::Unvalidated);
        assert_eq!(record.skip_reason, Some(SkipReasonV1::NoSpeedSource));

        // 仪表报了 0：这是「确实静止」，应当产出样本。
        acc.on_speedometer(0.0);
        acc.on_frame_start(&frame_start(1_020));
        let record = acc.on_frame_end(RollConvention::Unvalidated);
        let player = record.player.expect("仪表报 0 是有效读数");
        assert_eq!(player.speed_mps, 0.0);
        assert_eq!(player.speed_provenance, SpeedProvenanceV1::SpeedometerAbs);
    }

    /// 没有 `frame_start` 就没有对齐键 ⇒ 跳过。
    /// 68 §6.5 的验收硬闸是 depth 与 camera pose 严格对齐,没有键的帧对齐不了。
    #[test]
    fn a_frame_without_frame_start_has_no_alignment_key() {
        let mut acc = running_with_data();
        let record = acc.on_frame_end(RollConvention::Unvalidated);
        assert_eq!(record.skip_reason, Some(SkipReasonV1::NoFrameStart));
    }

    /// 对齐键**每帧消费一次**:上一帧的 `render_time` 不会被下一帧复用。
    /// 复用的后果是两帧带同一个键,下游 join 时二选一,静默丢一帧。
    #[test]
    fn the_alignment_key_is_consumed_once_per_frame() {
        let mut acc = running_with_data();
        acc.on_frame_start(&frame_start(5_000));
        assert_eq!(
            acc.on_frame_end(RollConvention::Unvalidated).render_time_us,
            5_000
        );
        let second = acc.on_frame_end(RollConvention::Unvalidated);
        assert_eq!(second.skip_reason, Some(SkipReasonV1::NoFrameStart));
    }

    /// `timer_restart` 会让 `render_time` 回跳,必须换 epoch。
    /// 只有同一 epoch 内的时间戳可比——否则下游按单调假设 join 会错配整段。
    #[test]
    fn a_timer_restart_starts_a_new_comparable_epoch() {
        let mut acc = running_with_data();
        acc.on_frame_start(&frame_start(9_000));
        let before = acc.on_frame_end(RollConvention::Unvalidated);
        assert_eq!(before.timer_epoch, 0);

        acc.on_world_placement(placement_facing_north());
        acc.on_frame_start(&ScsTelemetryFrameStart {
            flags: SCS_TELEMETRY_FRAME_START_FLAG_TIMER_RESTART,
            render_time: 10, // 回跳
            ..frame_start(0)
        });
        let after = acc.on_frame_end(RollConvention::Unvalidated);
        assert_eq!(after.timer_epoch, 1, "换了时间基就要换 epoch");
        assert!(after.render_time_us < before.render_time_us, "确实回跳了");
        assert_eq!(
            acc.unflagged_time_regressions(),
            0,
            "带标志的回跳是正常的，不该记进异常计数"
        );
    }

    /// **没置标志却回退**是引擎侧异常,要单独记账而不是当正常数据吃掉。
    #[test]
    fn an_unflagged_time_regression_is_counted_as_an_anomaly() {
        let mut acc = running_with_data();
        acc.on_frame_start(&frame_start(9_000));
        let _ = acc.on_frame_end(RollConvention::Unvalidated);
        acc.on_world_placement(placement_facing_north());
        acc.on_frame_start(&frame_start(8_000)); // 无标志的回退
        let _ = acc.on_frame_end(RollConvention::Unvalidated);
        assert_eq!(acc.unflagged_time_regressions(), 1);
    }

    /// 非法读数(欧拉角越界)一路冒泡成 skip,不产出错样本。
    #[test]
    fn illegal_readings_are_skipped_rather_than_converted() {
        let mut acc = running_with_data();
        acc.on_world_placement(ScsDplacement {
            orientation: crate::ffi::ScsEuler {
                heading: 90.0, // 度数误当圈数
                pitch: 0.0,
                roll: 0.0,
            },
            ..placement_facing_north()
        });
        acc.on_frame_start(&frame_start(1_000));
        let record = acc.on_frame_end(RollConvention::Unvalidated);
        assert_eq!(record.player, None);
        assert_eq!(record.skip_reason, Some(SkipReasonV1::ConversionRejected));
    }

    /// 帧号连续且从 1 起——缺号即丢帧,这条让「丢帧」可被下游发现。
    #[test]
    fn frame_numbers_are_dense_and_start_at_one() {
        let mut acc = running_with_data();
        let mut seen = Vec::new();
        for i in 0..5 {
            acc.on_world_placement(placement_facing_north());
            acc.on_frame_start(&frame_start(1_000 + i * 20));
            seen.push(acc.on_frame_end(RollConvention::Unvalidated).frame_seq);
        }
        assert_eq!(seen, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn host_qpc_brackets_are_preserved_for_probe_joining() {
        let mut acc = running_with_data();
        acc.on_frame_start_with_qpc(&frame_start(1_000), Some(50_000));
        let record =
            acc.on_frame_end_with_qpc(RollConvention::Unvalidated, Some(50_250), Some(10_000_000));
        assert_eq!(
            record.host_timing,
            HostTimingV1::Available {
                frame_start_qpc: 50_000,
                frame_end_qpc: 50_250,
                qpc_frequency_hz: 10_000_000,
            }
        );
    }

    /// 记录可 JSON 往返,且 skip 记录不带 player 字段值。
    #[test]
    fn records_round_trip_through_json() {
        let mut acc = running_with_data();
        acc.on_frame_start(&frame_start(1_000));
        let record = acc.on_frame_end(RollConvention::Unvalidated);
        let encoded = serde_json::to_string(&record).expect("可序列化");
        let decoded: FrameRecordV3 = serde_json::from_str(&encoded).expect("可反序列化");
        assert_eq!(decoded, record);
        assert!(encoded.contains("\"render_time_us\":1000"));
    }
}

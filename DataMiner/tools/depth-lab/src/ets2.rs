//! ETS2 交付桥：把 SCS 遥测流对齐到配对表，产出 `deliver --pose` 吃的位姿旁车。
//!
//! # 这一层解决的是「两个进程、两条时间轴」
//!
//! 遥测由**游戏进程**里的 SCS 插件写出（[`ets2_telemetry_plugin`] 那一侧），
//! 配对表由**采集端进程**的 recorder + `pairing` 产出。两者唯一的公共键是
//! `QueryPerformanceCounter`——它是系统级计数，跨进程可比。所以本模块做的事只有一件：
//! **按 QPC 就近取样，并把「近」的定义写死成一个可失败的硬闸。**
//!
//! # 三条纪律
//!
//! 1. **不插值。** `pairing` 那一层刻意保持笨（「按 `present_seq` 精确查表，
//!    查不到就是 `None`」）；这里的对应物是「取最近的一条，超出 `max_skew_us` 就是
//!    `None`」。插值会在暂停/读档/掉帧的边界上凭空造出一段平滑轨迹，而那正是
//!    最需要被发现的地方。
//! 2. **不外推。** 遥测流两端之外的视频帧一律拒绝，不拿端点值顶。
//! 3. **skip 记录不是位姿。** 插件对暂停、缺通道、换算被拒都写显式 skip；
//!    这里把它们当作「该时刻没有位姿」，而不是跳过后让邻近样本补位——
//!    补位会把主菜单时段伪装成行驶时段。
//!
//! # 与 Fallout 4 那条链的关系
//!
//! Fallout 4 走的是 `final_action_io` + probe 内的 `player-pose-index`
//! （见 [`crate::camera_action`]）：位姿在 Present 回调里与相机同帧采出，
//! 天然共 `present_seq`。ETS2 没有那个条件——遥测在游戏自己的帧循环里，
//! 我们不注入它——所以换成 QPC 就近对齐 + 硬闸。**代价是多了一项 skew 统计
//! 必须随交付带出**，见 [`Ets2PoseReport`]。

use std::io::{BufRead, BufReader, Write};
use std::path::Path;

use ets2_player_telemetry::{
    CameraSampleV1, InteriorCameraBaselineV1, OffsetComposition, PlayerSampleV1,
    RawCameraOffsetsV1, RollConvention, compose_interior_camera,
};
use serde::{Deserialize, Serialize};

use crate::camera::Intrinsics;
use crate::camera_action::{CameraPoseSource, FinalCameraIdentity};
use crate::deliver::PoseRow;
use crate::pairing::FramePair;

/// 插件写出的帧记录 schema。与 `ets2_telemetry_plugin::state::FRAME_SCHEMA_VERSION_V2`
/// 同一个字符串——刻意不跨 crate 引用常量，因为 depth-lab 不该为了一个字符串
/// 把插件 crate 拖进依赖树（那会让 probe DLL 也背上它）。
pub const TELEMETRY_FRAME_SCHEMA_V2: &str = "ets2-telemetry-frame/v2";
/// 本模块配置的 schema。
pub const POSE_CONFIG_SCHEMA_V1: &str = "ets2-pose-config/v1";
/// 本模块报告的 schema。
pub const POSE_REPORT_SCHEMA_V1: &str = "ets2-pose-report/v1";

/// 默认最大 skew：半个 30 fps 帧。
///
/// 取半帧而不是整帧，是因为「最近的一条」在最坏情况下也应当落在本帧内；
/// 超过半帧就说明遥测流与视频流之间存在成帧的错位，那时该查的是采集设置
/// （游戏帧率没锁 30？插件掉帧？），不是把闸放宽。
pub const DEFAULT_MAX_SKEW_US: u64 = 16_667;

/// 插件写出的一条帧记录。**只声明本模块要用的字段**，其余原样忽略——
/// 插件加字段不该让转换器整体失败。
#[derive(Debug, Clone, Deserialize)]
pub struct TelemetryFrame {
    pub schema_version: String,
    pub frame_seq: u64,
    pub timer_epoch: u64,
    pub host_qpc: Option<i64>,
    pub host_qpc_end: Option<i64>,
    pub host_qpc_frequency: Option<i64>,
    pub player: Option<PlayerSampleV1>,
    #[serde(default = "RawCameraOffsetsV1::unreported")]
    pub camera_offsets: RawCameraOffsetsV1,
    #[serde(default)]
    pub skip_reason: Option<String>,
}

/// 墙钟锚点：一次 `(QPC, UTC)` 配对读数。
///
/// 与 [44 号裁决](../../docs/44-run-wall-clock-anchor-decision.md) 同一个机制。
/// 墙钟**只用来填 `time` 字段**，绝不进入排序或对齐——那条纪律在 ETS2 这条链上
/// 同样成立。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WallClockAnchor {
    pub qpc: i64,
    /// Unix 纪元起的毫秒数（UTC）。用整数而不是字符串，免得转换器还要写一个日期解析器。
    pub unix_millis: i64,
}

/// 转换配置。每一项都必须显式给——没有「按经验取个默认」的余地。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Ets2PoseConfig {
    pub schema: String,
    pub baseline: InteriorCameraBaselineV1,
    pub roll_convention: RollConvention,
    pub offset_composition: OffsetComposition,
    /// 内参所依据的分辨率。必须是 `action_camera.json` 所描述的那一个（视频侧 1080p）。
    pub intrinsics_width: u32,
    pub intrinsics_height: u32,
    /// 视频帧 QPC 与最近遥测样本之间允许的最大间隔，微秒。
    #[serde(default = "default_max_skew_us")]
    pub max_skew_us: u64,
    pub wall_clock_anchor: WallClockAnchor,
    /// 采集机的 `QueryPerformanceFrequency`。必须与遥测记录里的逐条取值一致。
    pub qpc_frequency_hz: i64,
}

const fn default_max_skew_us() -> u64 {
    DEFAULT_MAX_SKEW_US
}

/// 一条视频帧为什么没能配上位姿。**每一条都要能指向一个可操作的现场原因。**
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PoseAbsentReason {
    /// 配对表这一行没有水印解出的 `present_seq`——旧版配对表才会这样。
    MissingPresentSeq,
    /// 视频帧的 QPC 落在遥测流之外。**不外推。**
    OutsideTelemetryWindow,
    /// 最近的遥测样本超出 `max_skew_us`。
    SkewExceeded,
    /// 最近的样本是一条 skip 记录（暂停、缺通道、换算被拒）。
    TelemetrySkipped,
    /// 该样本没有速度向量（仪表兜底那一路）。
    NoVelocityVector,
    /// 相机复合被拒（未标定次序下的非零偏移、越界欧拉角等）。
    CameraCompositionRejected,
}

/// 一条被拒的视频帧。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RejectedFrame {
    pub session_frame: u64,
    pub reason: PoseAbsentReason,
    /// 实测 skew，微秒。`None` 表示压根没找到候选样本。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skew_us: Option<i64>,
    /// 底层拒绝原因的原文。`reason` 只分到「相机复合被拒」这一层，
    /// 而那一层下面有五种成因，处置完全不同（换标定 / 改采集设置 / 上游数据坏）。
    /// 第一次真机跑就卡在这个粒度上：报告说了 5393 帧被拒，没说为什么。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// 转换报告。**skew 统计必须随交付带出**——它是这条链相对 Fallout 4 多出来的
/// 那一项不确定度，读交付的人有权看到它。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ets2PoseReport {
    pub schema: String,
    pub baseline_id: String,
    pub roll_convention: RollConvention,
    pub offset_composition: OffsetComposition,
    pub roll_calibrated: bool,
    pub offset_composition_calibrated: bool,
    pub video_frames: u64,
    pub pose_rows: u64,
    pub telemetry_frames: u64,
    pub telemetry_usable_samples: u64,
    pub max_skew_us: u64,
    pub skew_p50_us: i64,
    pub skew_p95_us: i64,
    pub skew_max_us: i64,
    /// 68 号 §6.3 的 `mouse_*` / `keyCode` 在本条链上的口径。
    /// ETS2 是**车载视角**轨迹，0722 变更后只有 Open 强制这些字段。
    pub mouse_and_keycode: String,
    pub rejected: Vec<RejectedFrame>,
}

impl Ets2PoseReport {
    /// 这份位姿能不能直接进正式交付。
    ///
    /// 缺一行就是「有 depth 没 pose」，68 号 §6.5 那道闸会在 `build_action_camera`
    /// 里把整条 clip 判掉——所以这里提前给出同一个结论，免得跑完一整轮转换
    /// 才在最后一步失败。
    #[must_use]
    pub fn is_deliverable(&self) -> bool {
        self.rejected.is_empty() && self.pose_rows == self.video_frames && self.pose_rows > 0
    }
}

/// 主入口：配对表 + 遥测流 → 位姿旁车。
pub fn build_pose_rows(
    pairs: &[FramePair],
    telemetry: &[TelemetryFrame],
    config: &Ets2PoseConfig,
) -> Result<(Vec<PoseRow>, Ets2PoseReport), String> {
    if config.schema != POSE_CONFIG_SCHEMA_V1 {
        return Err(format!("配置 schema 不是 {POSE_CONFIG_SCHEMA_V1}"));
    }
    if config.qpc_frequency_hz <= 0 {
        return Err("qpc_frequency_hz 必须为正".into());
    }
    config
        .baseline
        .validate()
        .map_err(|error| format!("相机基线不合法: {error:?}"))?;
    let intrinsics = config
        .baseline
        .intrinsics(config.intrinsics_width, config.intrinsics_height)
        .map_err(|error| format!("内参不可解: {error:?}"))?;
    let intrinsics = Intrinsics {
        fx: intrinsics.fx as f32,
        fy: intrinsics.fy as f32,
        cx: intrinsics.cx as f32,
        cy: intrinsics.cy as f32,
    };
    if pairs.is_empty() {
        return Err("配对表为空".into());
    }

    let samples = usable_samples(telemetry, config)?;
    let usable_count = samples.len() as u64;
    if samples.is_empty() {
        return Err("遥测流里没有任何可用样本".into());
    }

    let mut rows = Vec::with_capacity(pairs.len());
    let mut rejected = Vec::new();
    let mut skews = Vec::with_capacity(pairs.len());
    // 先按 session_frame 排一遍：配对表本应有序，但顺序错了会让相机速度的
    // 有限差分算出反向速度，而每一行看起来仍然完全正常。
    let mut ordered: Vec<&FramePair> = pairs.iter().collect();
    ordered.sort_by_key(|pair| pair.session_frame);
    for (index, pair) in ordered.iter().enumerate() {
        if index > 0 && ordered[index - 1].session_frame == pair.session_frame {
            return Err(format!("配对表 session_frame {} 重复", pair.session_frame));
        }
        let Some(present_seq) = pair.present_seq else {
            rejected.push(RejectedFrame {
                session_frame: pair.session_frame,
                reason: PoseAbsentReason::MissingPresentSeq,
                skew_us: None,
                detail: None,
            });
            continue;
        };
        match resolve(pair.source_qpc, &samples, config) {
            Ok((sample, skew_us)) => {
                let Some(player_velocity) = sample.player.velocity_mps else {
                    rejected.push(RejectedFrame {
                        session_frame: pair.session_frame,
                        reason: PoseAbsentReason::NoVelocityVector,
                        skew_us: Some(skew_us),
                        detail: None,
                    });
                    continue;
                };
                let camera = match compose_interior_camera(
                    sample.truck_world_position_m(),
                    sample.truck_world_orientation(),
                    &sample.offsets,
                    &config.baseline,
                    config.roll_convention,
                    config.offset_composition,
                ) {
                    Ok(camera) => camera,
                    Err(error) => {
                        rejected.push(RejectedFrame {
                            session_frame: pair.session_frame,
                            reason: PoseAbsentReason::CameraCompositionRejected,
                            skew_us: Some(skew_us),
                            detail: Some(format!("{error:?}")),
                        });
                        continue;
                    }
                };
                skews.push(skew_us.abs());
                rows.push(PendingRow {
                    present_seq,
                    session_frame: pair.session_frame,
                    source_qpc: pair.source_qpc,
                    game_fps: sample.game_fps,
                    camera,
                    player_position: sample.player.position_m,
                    player_rotation: sample.player.rotation_quaternion_xyzw,
                    player_velocity,
                });
            }
            Err(reason) => rejected.push(RejectedFrame {
                session_frame: pair.session_frame,
                reason: reason.0,
                skew_us: reason.1,
                detail: None,
            }),
        }
    }

    let pose_rows = finish_rows(&rows, intrinsics, config)?;
    skews.sort_unstable();
    let report = Ets2PoseReport {
        schema: POSE_REPORT_SCHEMA_V1.to_owned(),
        baseline_id: config.baseline.baseline_id.clone(),
        roll_convention: config.roll_convention,
        offset_composition: config.offset_composition,
        roll_calibrated: config.roll_convention.is_validated(),
        offset_composition_calibrated: config.offset_composition.is_validated(),
        video_frames: pairs.len() as u64,
        pose_rows: pose_rows.len() as u64,
        telemetry_frames: telemetry.len() as u64,
        telemetry_usable_samples: usable_count,
        max_skew_us: config.max_skew_us,
        skew_p50_us: percentile(&skews, 50),
        skew_p95_us: percentile(&skews, 95),
        skew_max_us: skews.last().copied().unwrap_or(0),
        mouse_and_keycode: "exempt_vehicle_lane".to_owned(),
        rejected,
    };
    Ok((pose_rows, report))
}

/// 一条已经通过全部校验、只差算速度与时间的行。
struct PendingRow {
    present_seq: u64,
    session_frame: u64,
    source_qpc: i64,
    /// 该样本所在时刻的游戏实时帧率，由遥测自己的 QPC 时间轴测得。
    /// 68 号 §6.3 的 `fps` 要的是「游戏当前运行时的实时帧率」，不是视频封装帧率。
    game_fps: f64,
    camera: CameraSampleV1,
    player_position: [f64; 3],
    player_rotation: [f64; 4],
    player_velocity: [f64; 3],
}

fn finish_rows(
    rows: &[PendingRow],
    intrinsics: Intrinsics,
    config: &Ets2PoseConfig,
) -> Result<Vec<PoseRow>, String> {
    if rows.len() < 2 {
        // 相机速度是逐帧有限差分，一行算不出来。与 `assemble_final_pose_rows`
        // 同一条判断——差别只在这里的键是 source_qpc 而不是 output_qpc_start。
        //
        // **返回空而不是 Err**：走到这里说明几乎所有帧都进了 `rejected`，
        // 而那份逐条原因正是排障唯一的入口。早退成 Err 会把报告一起吞掉，
        // 现场只剩一句「0 行」——第一次真机跑就是这么卡住的。
        // 交付判定不受影响：报告的 `is_deliverable()` 仍然是 false。
        return Ok(Vec::new());
    }
    let frequency = config.qpc_frequency_hz as f64;
    let mut out = Vec::with_capacity(rows.len());
    for (index, row) in rows.iter().enumerate() {
        // CFR pacer 的 duplicate 帧复用同一个 source frame，因此与前一帧**共用
        // 同一个 `source_qpc`**（`pairing::build` 里那条不变式 5）。拿相邻两行
        // 直接做差会得到 dt=0；所以邻居要往前找到第一个 QPC 真正不同的行，
        // 找不到再往回找。这与 `camera_action::dynamic_present_fps` 跳过同
        // `present_seq` 邻居是同一种处理。
        let neighbour = rows[index + 1..]
            .iter()
            .find(|other| other.source_qpc != row.source_qpc)
            .or_else(|| {
                rows[..index]
                    .iter()
                    .rev()
                    .find(|other| other.source_qpc != row.source_qpc)
            })
            .ok_or_else(|| {
                format!(
                    "全部 {} 行位姿共用同一个 source_qpc（{}）——整段没有时间跨度，算不出相机速度",
                    rows.len(),
                    row.source_qpc
                )
            })?;
        let (from, to) = if neighbour.source_qpc > row.source_qpc {
            (row, neighbour)
        } else {
            (neighbour, row)
        };
        let dt = (to.source_qpc - from.source_qpc) as f64 / frequency;
        let camera_speed = std::array::from_fn(|axis| {
            (to.camera.position_m[axis] - from.camera.position_m[axis]) / dt
        });
        out.push(PoseRow {
            present_seq: row.present_seq,
            session_frame: row.session_frame,
            camera_identity: FinalCameraIdentity::GameTelemetry {
                provider: "scs-sdk-telemetry/v2".into(),
            },
            camera_source: CameraPoseSource::GameTelemetry,
            fps: row.game_fps,
            time: format_delivery_time(row.source_qpc, config)
                .ok_or_else(|| format!("墙钟投影越界：source_qpc={}", row.source_qpc))?,
            camera_position: row.camera.position_m,
            camera_rotation_quaternion: row.camera.rotation_quaternion_xyzw,
            camera_intrinsics: intrinsics,
            camera_follow_offset: row.camera.follow_offset_m,
            camera_speed,
            player_position: row.player_position,
            player_rotation_quaternion: row.player_rotation,
            player_speed: row.player_velocity,
            // 68 号 §6.3：`mouse_*` / `keyCode` 只在 Open 轨迹强制（0722 变更）。
            // ETS2 是车载视角，落在豁免侧；字段按 schema 保留但不编造取值。
            mouse_x: 0.0,
            mouse_y: 0.0,
            mouse_dx: 0.0,
            mouse_dy: 0.0,
            key_code: Vec::new(),
        });
    }
    Ok(out)
}

/// 一条可用的遥测样本：有 QPC、有 player 载荷。
struct UsableSample {
    qpc: i64,
    /// 游戏实时帧率，由本样本与相邻样本的 QPC 间隔测得。
    game_fps: f64,
    player: PlayerSampleV1,
    offsets: RawCameraOffsetsV1,
}

impl UsableSample {
    /// `PlayerSampleV1` 存的是**交付系**位置，而 `compose_interior_camera`
    /// 吃的是 **SCS 世界系**。这里做一次逆变换（只翻 Z 号，自身即逆）。
    ///
    /// 反过来做——让复合器吃交付系——会让它内部的欧拉→旋转矩阵与轴变换错配，
    /// 而结果仍是一个合法的单位四元数，看不出来。
    fn truck_world_position_m(&self) -> [f64; 3] {
        let p = self.player.position_m;
        [p[0], p[1], -p[2]]
    }

    fn truck_world_orientation(&self) -> ets2_player_telemetry::EulerTurns {
        self.player.source_orientation
    }
}

fn usable_samples(
    telemetry: &[TelemetryFrame],
    config: &Ets2PoseConfig,
) -> Result<Vec<UsableSample>, String> {
    let mut out: Vec<UsableSample> = Vec::with_capacity(telemetry.len());
    let mut epoch: Option<u64> = None;
    for (index, frame) in telemetry.iter().enumerate() {
        if frame.schema_version != TELEMETRY_FRAME_SCHEMA_V2 {
            return Err(format!(
                "遥测第 {} 条的 schema 是 {}，本转换器只认 {TELEMETRY_FRAME_SCHEMA_V2}",
                index + 1,
                frame.schema_version
            ));
        }
        if let Some(frequency) = frame.host_qpc_frequency
            && frequency != config.qpc_frequency_hz
        {
            return Err(format!(
                "遥测第 {} 条的 QPC 频率 {frequency} 与配置的 {} 不符——两个进程读到不同频率\
                 说明它们不在同一台机器或同一次开机上，对齐结果不可信",
                index + 1,
                config.qpc_frequency_hz
            ));
        }
        // 换了时间基说明中途读档/重开了计时器。`host_qpc` 本身不受影响（QPC 是系统时钟），
        // 但整段素材的连续性已经断了，所以这里当场停而不是接着算。
        match epoch {
            None => epoch = Some(frame.timer_epoch),
            Some(previous) if previous != frame.timer_epoch => {
                return Err(format!(
                    "遥测第 {} 条换了 timer_epoch（{previous} -> {}）——这一段跨了引擎计时器重置，\
                     请按 epoch 切段后分别转换",
                    index + 1,
                    frame.timer_epoch
                ));
            }
            Some(_) => {}
        }
        let (Some(qpc), Some(player)) = (frame.host_qpc, frame.player.clone()) else {
            continue;
        };
        if qpc <= 0 {
            return Err(format!("遥测第 {} 条的 host_qpc 非正", index + 1));
        }
        if let Some(end) = frame.host_qpc_end
            && end < qpc
        {
            return Err(format!(
                "遥测第 {} 条的 frame_end QPC 早于 frame_start",
                index + 1
            ));
        }
        if let Some(previous) = out.last()
            && qpc <= previous.qpc
        {
            return Err(format!(
                "遥测第 {} 条的 host_qpc 没有递增（{} -> {qpc}）——QPC 是单调计数，\
                 回退说明这份文件被拼接过",
                index + 1,
                previous.qpc
            ));
        }
        out.push(UsableSample {
            qpc,
            // 先占位，全部收齐后统一按相邻间隔回填——测帧率需要下一条样本。
            game_fps: 0.0,
            player,
            offsets: frame.camera_offsets,
        });
    }
    fill_game_fps(&mut out, config.qpc_frequency_hz);
    Ok(out)
}

/// 按相邻样本的 QPC 间隔回填游戏实时帧率。
///
/// 用**遥测自己的**时间轴而不是视频帧间隔：后者被 CFR pacer 规整过，
/// 恒等于 30，写进 `fps` 就等于把「游戏跑了多快」这条事实抹掉——
/// 而 68 号 §7.1 的「游戏帧率保持在 30fps 以上」正是靠这一列验收的。
fn fill_game_fps(samples: &mut [UsableSample], frequency: i64) {
    let intervals: Vec<f64> = (0..samples.len())
        .map(|index| {
            let (from, to) = if index + 1 < samples.len() {
                (index, index + 1)
            } else if index > 0 {
                (index - 1, index)
            } else {
                return 0.0;
            };
            let ticks = samples[to].qpc - samples[from].qpc;
            if ticks <= 0 {
                0.0
            } else {
                frequency as f64 / ticks as f64
            }
        })
        .collect();
    for (sample, fps) in samples.iter_mut().zip(intervals) {
        sample.game_fps = fps;
    }
}

fn resolve<'a>(
    source_qpc: i64,
    samples: &'a [UsableSample],
    config: &Ets2PoseConfig,
) -> Result<(&'a UsableSample, i64), (PoseAbsentReason, Option<i64>)> {
    // **不外推**：两端之外一律拒绝。
    let (Some(first), Some(last)) = (samples.first(), samples.last()) else {
        return Err((PoseAbsentReason::OutsideTelemetryWindow, None));
    };
    if source_qpc < first.qpc || source_qpc > last.qpc {
        return Err((PoseAbsentReason::OutsideTelemetryWindow, None));
    }
    let index = match samples.binary_search_by_key(&source_qpc, |sample| sample.qpc) {
        Ok(index) => index,
        Err(insert) => {
            // insert 一定落在 (0, len)：两端已被上面的窗口闸挡掉。
            let before = &samples[insert - 1];
            let after = &samples[insert];
            if source_qpc - before.qpc <= after.qpc - source_qpc {
                insert - 1
            } else {
                insert
            }
        }
    };
    let sample = &samples[index];
    let skew_us = qpc_to_micros(source_qpc - sample.qpc, config.qpc_frequency_hz);
    if skew_us.unsigned_abs() > config.max_skew_us {
        return Err((PoseAbsentReason::SkewExceeded, Some(skew_us)));
    }
    Ok((sample, skew_us))
}

fn qpc_to_micros(ticks: i64, frequency: i64) -> i64 {
    (i128::from(ticks) * 1_000_000 / i128::from(frequency)) as i64
}

fn percentile(sorted: &[i64], p: u64) -> i64 {
    if sorted.is_empty() {
        return 0;
    }
    let index = ((sorted.len() as u64 - 1) * p / 100) as usize;
    sorted[index]
}

/// 68 号 §6.3 的 `time`：`YYYY-MM-DD HH:mm:ss.SSS`。
///
/// 注意与 `capture-core::wall_clock` 的 ISO-8601 输出**格式不同**（那边是
/// `T`/`Z` 形式）：68 号的样本用空格分隔且不带时区后缀，交付层逐字照办。
fn format_delivery_time(target_qpc: i64, config: &Ets2PoseConfig) -> Option<String> {
    let delta = i128::from(target_qpc) - i128::from(config.wall_clock_anchor.qpc);
    let offset_millis = delta.checked_mul(1_000)? / i128::from(config.qpc_frequency_hz);
    let millis =
        i64::try_from(i128::from(config.wall_clock_anchor.unix_millis) + offset_millis).ok()?;
    const MILLIS_PER_DAY: i64 = 86_400_000;
    let (year, month, day) = civil_from_days(millis.div_euclid(MILLIS_PER_DAY))?;
    let ms_of_day = millis.rem_euclid(MILLIS_PER_DAY);
    Some(format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}.{:03}",
        ms_of_day / 3_600_000,
        (ms_of_day / 60_000) % 60,
        (ms_of_day / 1_000) % 60,
        ms_of_day % 1_000,
    ))
}

/// Howard Hinnant 的 `civil_from_days`。与 `capture-core::wall_clock` 同一份算法
/// ——那个 crate 是 Windows-only 的，本 crate 刻意平台无关，所以不共享实现。
fn civil_from_days(days: i64) -> Option<(i64, i64, i64)> {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (1..=9999).contains(&year).then_some((year, month, day))
}

// ---------------------------------------------------------------------------
// 文件层
// ---------------------------------------------------------------------------

pub fn read_telemetry(path: &Path) -> Result<Vec<TelemetryFrame>, String> {
    let file = std::fs::File::open(path)
        .map_err(|error| format!("打开 {} 失败: {error}", path.display()))?;
    let mut rows = Vec::new();
    for (index, line) in BufReader::new(file).lines().enumerate() {
        let line = line
            .map_err(|error| format!("读 {} 第 {} 行失败: {error}", path.display(), index + 1))?;
        if line.trim().is_empty() {
            continue;
        }
        rows.push(
            serde_json::from_str(&line).map_err(|error| {
                format!("{} 第 {} 行解析失败: {error}", path.display(), index + 1)
            })?,
        );
    }
    if rows.is_empty() {
        return Err(format!("{} 里没有任何遥测记录", path.display()));
    }
    Ok(rows)
}

pub fn read_pairs(path: &Path) -> Result<Vec<FramePair>, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("读 {} 失败: {error}", path.display()))?;
    let mut rows = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        rows.push(
            serde_json::from_str(line).map_err(|error| {
                format!("{} 第 {} 行解析失败: {error}", path.display(), index + 1)
            })?,
        );
    }
    if rows.is_empty() {
        return Err(format!("{} 里没有任何配对行", path.display()));
    }
    Ok(rows)
}

pub fn read_config(path: &Path) -> Result<Ets2PoseConfig, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("读 {} 失败: {error}", path.display()))?;
    serde_json::from_str(&text).map_err(|error| format!("{} 解析失败: {error}", path.display()))
}

/// 把全速率位姿收敛成「每个 `present_seq` 一行」。
///
/// # 为什么需要两份文件
///
/// `deliver --pose`（15 fps，绑 depth）读的是 [`crate::deliver::read_pose_index`]，
/// 它按 `present_seq` 建表并**在重复键上直接报错**；而 `deliver --pose30` 读的是
/// `read_pose_rows`，要求 `session_frame` 逐帧连续。CFR pacer 的 duplicate 帧
/// 合法地共用一个 Present，于是同一份全速率文件不可能同时满足两边。
///
/// 收敛保留**第一条**，但先核对同键各行的位姿逐位一致——duplicate 帧本就来自
/// 同一次渲染、同一个 `source_qpc`，不一致说明上游配对出了问题，那时报错比
/// 「随便留一条」安全。
pub fn collapse_by_present_seq(rows: &[PoseRow]) -> Result<Vec<PoseRow>, String> {
    let mut seen: std::collections::BTreeMap<u64, &PoseRow> = std::collections::BTreeMap::new();
    let mut out = Vec::new();
    for row in rows {
        match seen.get(&row.present_seq) {
            None => {
                seen.insert(row.present_seq, row);
                out.push(row.clone());
            }
            Some(first) => {
                if first.camera_position != row.camera_position
                    || first.camera_rotation_quaternion != row.camera_rotation_quaternion
                    || first.player_position != row.player_position
                {
                    return Err(format!(
                        "present_seq {} 出现在 session_frame {} 与 {} 上却带着不同位姿——\
                         共用一个 Present 的帧必须来自同一次渲染",
                        row.present_seq, first.session_frame, row.session_frame
                    ));
                }
            }
        }
    }
    Ok(out)
}

pub fn write_pose_index(path: &Path, rows: &[PoseRow]) -> Result<(), String> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("建 {} 失败: {error}", parent.display()))?;
    }
    let temp = path.with_extension("jsonl.tmp");
    let mut file = std::fs::File::create(&temp)
        .map_err(|error| format!("建 {} 失败: {error}", temp.display()))?;
    for row in rows {
        serde_json::to_writer(&mut file, row)
            .map_err(|error| format!("序列化位姿失败: {error}"))?;
        file.write_all(b"\n")
            .map_err(|error| format!("写 {} 失败: {error}", temp.display()))?;
    }
    file.flush()
        .map_err(|error| format!("flush {} 失败: {error}", temp.display()))?;
    std::fs::rename(&temp, path).map_err(|error| format!("发布 {} 失败: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ets2_player_telemetry::{EulerTurns, FovSource, RawTruckSampleV1};

    use crate::pairing::CameraMode;

    const FREQ: i64 = 10_000_000; // 10 MHz，与常见 Windows QPC 同量级
    const TICKS_PER_FRAME: i64 = FREQ / 30;

    fn baseline() -> InteriorCameraBaselineV1 {
        InteriorCameraBaselineV1 {
            schema_version: ets2_player_telemetry::CAMERA_SCHEMA_VERSION_V1.to_owned(),
            baseline_id: "test-interior-v1".to_owned(),
            truck_id: "vehicle.scania.s_2016".to_owned(),
            evidence_path: "tools/pr405-ets2/evidence/example.json".to_owned(),
            default_head_position_m: [-0.45, 1.85, -1.20],
            default_head_orientation: EulerTurns {
                heading: 0.0,
                pitch: 0.0,
                roll: 0.0,
            },
            horizontal_fov_degrees: 71.0,
            fov_source: FovSource::UnpackedGameDefinition,
        }
    }

    fn config() -> Ets2PoseConfig {
        Ets2PoseConfig {
            schema: POSE_CONFIG_SCHEMA_V1.to_owned(),
            baseline: baseline(),
            roll_convention: RollConvention::Unvalidated,
            offset_composition: OffsetComposition::Unvalidated,
            intrinsics_width: 1920,
            intrinsics_height: 1080,
            max_skew_us: DEFAULT_MAX_SKEW_US,
            wall_clock_anchor: WallClockAnchor {
                qpc: 1_000_000,
                // 2026-08-07 10:00:00.000 UTC
                unix_millis: 1_786_096_800_000,
            },
            qpc_frequency_hz: FREQ,
        }
    }

    /// 第 `n` 帧（0 起）的遥测记录，车沿北向以 20 m/s 前进。
    fn telemetry_frame(n: i64) -> TelemetryFrame {
        let raw = RawTruckSampleV1 {
            // SCS 世界系：Z 指南，所以向北前进是 z 递减。
            world_position_m: [0.0, 0.0, -20.0 * n as f64 / 30.0],
            world_orientation: EulerTurns {
                heading: 0.0,
                pitch: 0.0,
                roll: 0.0,
            },
            local_linear_velocity_mps: Some([0.0, 0.0, -20.0]),
            speedometer_mps: 20.0,
        };
        TelemetryFrame {
            schema_version: TELEMETRY_FRAME_SCHEMA_V2.to_owned(),
            frame_seq: (n + 1) as u64,
            timer_epoch: 0,
            host_qpc: Some(1_000_000 + n * TICKS_PER_FRAME),
            host_qpc_end: Some(1_000_000 + n * TICKS_PER_FRAME + 1_000),
            host_qpc_frequency: Some(FREQ),
            player: Some(
                raw.to_player_sample(RollConvention::Unvalidated)
                    .expect("合法样本"),
            ),
            camera_offsets: RawCameraOffsetsV1::zero(),
            skip_reason: None,
        }
    }

    /// 第 `n` 个视频帧，QPC 刻意比遥测晚 1 ms（真实链路里两条流不会精确重合）。
    fn pair(n: i64) -> FramePair {
        FramePair {
            session_frame: n as u64,
            segment_index: 0,
            segment_frame: n as u32,
            source_qpc: 1_000_000 + n * TICKS_PER_FRAME + FREQ / 1_000,
            duplicate: false,
            camera_mode: CameraMode::Player,
            present_seq: Some(100 + n as u64),
            depth: None,
            depth_absent_reason: None,
        }
    }

    /// 遥测比视频**两端各多出一帧**：真实链路里插件先起、后停，
    /// 而「不外推」这条纪律要求视频帧必须落在遥测窗口内侧。
    fn run(frames: i64) -> (Vec<PoseRow>, Ets2PoseReport) {
        let pairs: Vec<_> = (0..frames).map(pair).collect();
        let telemetry: Vec<_> = (-1..=frames).map(telemetry_frame).collect();
        build_pose_rows(&pairs, &telemetry, &config()).expect("应当可转换")
    }

    /// 端到端：每个视频帧配上一行位姿，键是 `present_seq`（`deliver --pose` 按它查表）。
    #[test]
    fn every_video_frame_gets_one_pose_row_keyed_by_present_seq() {
        let (rows, report) = run(10);
        assert_eq!(rows.len(), 10);
        assert_eq!(
            rows.iter().map(|row| row.present_seq).collect::<Vec<_>>(),
            (100..110).collect::<Vec<_>>()
        );
        assert!(report.is_deliverable());
        assert!(report.rejected.is_empty());
    }

    /// **skew 必须被如实统计并带出。** 造数时视频帧比遥测晚 1 ms，报告应当看得见。
    #[test]
    fn the_measured_skew_is_reported_rather_than_hidden() {
        let (_, report) = run(10);
        assert_eq!(report.skew_p50_us, 1_000, "实得 {report:?}");
        assert_eq!(report.skew_max_us, 1_000);
        assert!(report.skew_max_us as u64 <= report.max_skew_us);
    }

    /// **超出闸门的帧被拒，而不是配一个远处的样本。**
    ///
    /// 这是本模块最重要的一条：一个偏了半帧以上的位姿在 SFM 里会被当成真实位移，
    /// 而每一行数据看起来都完全正常。
    #[test]
    fn a_frame_beyond_the_skew_gate_is_rejected_not_snapped() {
        let pairs: Vec<_> = (0..10).map(pair).collect();
        // 遥测掉了两帧（队列满、插件被抢占……）。对应的视频帧最近的样本已经在一帧之外，
        // 那不是「这一帧」的位姿——必须拒，不能贴过去。
        let telemetry: Vec<_> = (-1..=10)
            .filter(|n| !(5..=6).contains(n))
            .map(telemetry_frame)
            .collect();
        let (rows, report) =
            build_pose_rows(&pairs, &telemetry, &config()).expect("其余帧仍可转换");
        assert_eq!(rows.len(), 8);
        assert_eq!(
            report
                .rejected
                .iter()
                .map(|r| r.session_frame)
                .collect::<Vec<_>>(),
            vec![5, 6]
        );
        assert!(
            report
                .rejected
                .iter()
                .all(|r| r.reason == PoseAbsentReason::SkewExceeded)
        );
        assert!(
            !report.is_deliverable(),
            "缺一行就不能进正式交付——68 号 §6.5 会整条打回"
        );
    }

    /// **不外推。** 视频比遥测长出来的那一段没有位姿，不拿端点顶。
    #[test]
    fn frames_outside_the_telemetry_window_are_refused_rather_than_extrapolated() {
        let pairs: Vec<_> = (0..12).map(pair).collect();
        let telemetry: Vec<_> = (-1..=10).map(telemetry_frame).collect();
        let (rows, report) = build_pose_rows(&pairs, &telemetry, &config()).expect("前段可转换");
        assert_eq!(rows.len(), 10);
        assert_eq!(report.rejected.len(), 2);
        assert!(
            report
                .rejected
                .iter()
                .all(|r| r.reason == PoseAbsentReason::OutsideTelemetryWindow)
        );
    }

    /// **skip 记录不是位姿。** 暂停期的帧在遥测里是 `player: null`，
    /// 相邻样本不得替它补位——否则主菜单时段会被伪装成行驶时段。
    #[test]
    fn a_paused_telemetry_frame_is_not_covered_by_its_neighbour() {
        let mut telemetry: Vec<_> = (-1..=10).map(telemetry_frame).collect();
        for frame in &mut telemetry[5..8] {
            frame.player = None;
            frame.skip_reason = Some("paused".to_owned());
        }
        let pairs: Vec<_> = (0..10).map(pair).collect();
        let (_, report) = build_pose_rows(&pairs, &telemetry, &config()).expect("其余帧可转换");
        let rejected: Vec<_> = report.rejected.iter().map(|r| r.session_frame).collect();
        assert_eq!(
            rejected,
            vec![4, 5, 6],
            "暂停的三帧必须各自被拒，实得 {report:?}"
        );
        assert!(
            report
                .rejected
                .iter()
                .all(|r| r.reason == PoseAbsentReason::SkewExceeded),
            "最近的可用样本在一帧之外 ⇒ 超闸；它不该被静默采用"
        );
    }

    /// 旧版配对表没有 `present_seq`，无法定位 depth——显式拒绝而不是猜。
    #[test]
    fn pairs_without_a_watermarked_present_seq_are_rejected() {
        let mut pairs: Vec<_> = (0..10).map(pair).collect();
        pairs[3].present_seq = None;
        let telemetry: Vec<_> = (-1..=10).map(telemetry_frame).collect();
        let (_, report) = build_pose_rows(&pairs, &telemetry, &config()).expect("其余帧可转换");
        assert_eq!(
            report.rejected[0].reason,
            PoseAbsentReason::MissingPresentSeq
        );
    }

    /// 仪表兜底那一路没有速度向量 ⇒ 拒绝。68 号要的 `player_speed` 是 Vector3。
    #[test]
    fn samples_without_a_velocity_vector_are_rejected() {
        let mut telemetry: Vec<_> = (-1..=10).map(telemetry_frame).collect();
        let degraded = RawTruckSampleV1 {
            world_position_m: [0.0, 0.0, 0.0],
            world_orientation: EulerTurns {
                heading: 0.0,
                pitch: 0.0,
                roll: 0.0,
            },
            local_linear_velocity_mps: None,
            speedometer_mps: 20.0,
        };
        telemetry[3].player = Some(
            degraded
                .to_player_sample(RollConvention::Unvalidated)
                .expect("合法"),
        );
        let pairs: Vec<_> = (0..10).map(pair).collect();
        let (_, report) = build_pose_rows(&pairs, &telemetry, &config()).expect("其余帧可转换");
        assert_eq!(
            report.rejected[0].reason,
            PoseAbsentReason::NoVelocityVector
        );
    }

    /// **两个进程读到不同 QPC 频率 ⇒ 当场停。**
    /// 频率不符说明它们不在同一台机器/同一次开机上，对齐结果整体不可信。
    #[test]
    fn a_qpc_frequency_mismatch_stops_the_conversion() {
        let mut telemetry: Vec<_> = (-1..=10).map(telemetry_frame).collect();
        telemetry[0].host_qpc_frequency = Some(3_579_545);
        let pairs: Vec<_> = (0..10).map(pair).collect();
        let error = build_pose_rows(&pairs, &telemetry, &config()).expect_err("必须拒绝");
        assert!(error.contains("QPC 频率"), "实得 {error}");
    }

    /// 换了 `timer_epoch` 说明中途读档/重置了引擎计时器——整段停，不接着算。
    #[test]
    fn a_timer_epoch_change_stops_the_conversion() {
        let mut telemetry: Vec<_> = (-1..=10).map(telemetry_frame).collect();
        for frame in &mut telemetry[5..] {
            frame.timer_epoch = 1;
        }
        let pairs: Vec<_> = (0..10).map(pair).collect();
        let error = build_pose_rows(&pairs, &telemetry, &config()).expect_err("必须拒绝");
        assert!(error.contains("timer_epoch"), "实得 {error}");
    }

    /// QPC 回退说明这份遥测被拼接过——单调性是它作为 join 键的全部前提。
    #[test]
    fn a_non_monotonic_telemetry_qpc_stops_the_conversion() {
        let mut telemetry: Vec<_> = (-1..=10).map(telemetry_frame).collect();
        telemetry[6].host_qpc = Some(1);
        let pairs: Vec<_> = (0..10).map(pair).collect();
        let error = build_pose_rows(&pairs, &telemetry, &config()).expect_err("必须拒绝");
        assert!(error.contains("递增"), "实得 {error}");
    }

    /// 相机速度由**视频帧**时间轴的有限差分给出，且方向正确。
    /// 造数是沿北向 20 m/s，交付系前向是 +Z。
    #[test]
    fn camera_speed_is_differentiated_along_the_video_timeline() {
        let (rows, _) = run(10);
        for row in &rows {
            assert!(
                // 造数的帧间隔是 QPC 整数刻度（10 MHz / 30 不整除），所以有 ppm 级残差。
                (row.camera_speed[2] - 20.0).abs() < 1e-3,
                "实得 {:?}",
                row.camera_speed
            );
            assert!(row.camera_speed[0].abs() < 1e-9 && row.camera_speed[1].abs() < 1e-9);
        }
    }

    /// `time` 用 68 号的格式：空格分隔、毫秒精度、无时区后缀。
    /// 与 `capture-core` 的 ISO-8601 不是同一个形状，别「统一」它们。
    #[test]
    fn the_time_field_uses_the_requirement_document_format() {
        let (rows, _) = run(3);
        // 视频帧 0 在锚点之后 1 ms（造数刻意让两条流不精确重合），
        // 之后每帧 +1/30 s。
        assert_eq!(rows[0].time, "2026-08-07 10:00:00.001");
        assert_eq!(rows[1].time, "2026-08-07 10:00:00.034");
        assert!(
            !rows[0].time.contains('T') && !rows[0].time.contains('Z'),
            "68 号样本是 `YYYY-MM-DD HH:mm:ss.SSS`"
        );
    }

    /// 相机位置是「卡车位置 + 经车身旋转的头位」，不是卡车位置本身。
    /// `camera_follow_offset` 则是同系下的逐分量差。
    #[test]
    fn the_camera_sits_at_the_head_position_not_the_truck_origin() {
        let (rows, _) = run(3);
        let row = &rows[0];
        assert!((row.camera_position[1] - 1.85).abs() < 1e-9);
        assert!(
            (row.camera_follow_offset[1] - 1.85).abs() < 1e-9,
            "实得 {:?}",
            row.camera_follow_offset
        );
        assert!((row.player_position[1] - 0.0).abs() < 1e-9);
    }

    /// 未标定的 roll 与复合次序必须在报告里可见——交付层据此下调置信度。
    #[test]
    fn uncalibrated_conventions_are_visible_in_the_report() {
        let (_, report) = run(4);
        assert!(!report.roll_calibrated);
        assert!(!report.offset_composition_calibrated);
        assert_eq!(report.mouse_and_keycode, "exempt_vehicle_lane");
    }

    /// 配对表 session_frame 重复 ⇒ 停。重复意味着上游 pairing 出了问题，
    /// 而它会让相机速度的有限差分算出 0 或反向速度。
    #[test]
    fn duplicate_session_frames_stop_the_conversion() {
        let mut pairs: Vec<_> = (0..10).map(pair).collect();
        pairs[4].session_frame = pairs[3].session_frame;
        let telemetry: Vec<_> = (-1..=10).map(telemetry_frame).collect();
        let error = build_pose_rows(&pairs, &telemetry, &config()).expect_err("必须拒绝");
        assert!(error.contains("重复"), "实得 {error}");
    }

    /// **CFR duplicate 帧共用一个 Present，也共用一个 `source_qpc`。**
    ///
    /// 这是真实采集里必定出现的形状（`pairing` 不变式 5）。直接拿相邻两行做差
    /// 会得到 dt=0 → 速度 ±inf，而每一行看起来都完全正常。这条钉住「邻居要往前
    /// 找到第一个 QPC 真正不同的行」。
    #[test]
    fn duplicate_rgb_frames_sharing_one_present_do_not_break_camera_speed() {
        let mut pairs: Vec<_> = (0..10).map(pair).collect();
        // 第 4 帧是 WGC 复用槽：没有新 source frame，于是继承第 3 帧的 present_seq 与 QPC。
        pairs[4].duplicate = true;
        pairs[4].present_seq = pairs[3].present_seq;
        pairs[4].source_qpc = pairs[3].source_qpc;
        let telemetry: Vec<_> = (-1..=10).map(telemetry_frame).collect();
        let (rows, report) = build_pose_rows(&pairs, &telemetry, &config()).expect("应当可转换");
        assert_eq!(
            rows.len(),
            10,
            "duplicate 帧仍然是一个视频帧，要有自己的一行"
        );
        assert!(report.is_deliverable());
        for row in &rows {
            assert!(
                row.camera_speed.iter().all(|v| v.is_finite()),
                "session_frame {} 的速度是 {:?}",
                row.session_frame,
                row.camera_speed
            );
            assert!((row.camera_speed[2] - 20.0).abs() < 1e-3);
        }
    }

    /// 去重后每个 `present_seq` 恰好一行——这正是 `deliver --pose` 那一侧的要求。
    #[test]
    fn collapsing_by_present_seq_yields_exactly_one_row_per_present() {
        let mut pairs: Vec<_> = (0..10).map(pair).collect();
        pairs[4].duplicate = true;
        pairs[4].present_seq = pairs[3].present_seq;
        pairs[4].source_qpc = pairs[3].source_qpc;
        let telemetry: Vec<_> = (-1..=10).map(telemetry_frame).collect();
        let (rows, _) = build_pose_rows(&pairs, &telemetry, &config()).expect("应当可转换");
        let collapsed = collapse_by_present_seq(&rows).expect("同源帧应可收敛");
        assert_eq!(collapsed.len(), 9);
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join("pose.index.jsonl");
        write_pose_index(&path, &collapsed).expect("可写");
        // read_pose_index 在重复键上直接报错，所以这一步同时是收敛正确性的验收。
        let map = crate::deliver::read_pose_index(&path).expect("15fps 侧必须读得进去");
        assert_eq!(map.len(), 9);
    }

    /// 同一个 `present_seq` 却带着不同位姿 ⇒ 报错，而不是随便留一条。
    /// 这只可能来自上游配对出错，静默留一条会把错误藏进交付。
    #[test]
    fn collapsing_refuses_to_pick_between_contradictory_rows() {
        let (mut rows, _) = run(4);
        rows[1].present_seq = rows[0].present_seq;
        let error = collapse_by_present_seq(&rows).expect_err("必须拒绝");
        assert!(error.contains("不同位姿"), "实得 {error}");
    }

    /// 位姿旁车可被 `deliver::read_pose_index` 读回——这两端必须永远咬合。
    #[test]
    fn the_written_index_is_readable_by_the_delivery_layer() {
        let (rows, _) = run(4);
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join("pose.jsonl");
        write_pose_index(&path, &rows).expect("可写");
        let read_back = crate::deliver::read_pose_index(&path).expect("可读");
        assert_eq!(read_back.len(), rows.len());
        let first = read_back.get(&rows[0].present_seq).expect("键对得上");
        assert_eq!(first.time, rows[0].time);
        assert!((f64::from(first.camera_intrinsics.fx) - 1345.87).abs() < 0.01);
    }
}

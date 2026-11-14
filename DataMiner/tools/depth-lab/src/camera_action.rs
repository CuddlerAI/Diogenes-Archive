//! Camera / Action 的传输无关契约与严格逐帧合成。
//!
//! 中间 join 刻意不直接生成 68 号最终 `action_camera.json`：正式 schema 要求显式
//! `player_*` / `camera_follow_offset`，并非每条采集链都有可信玩家来源；Raw Input 也只提供
//! 原始计数，需要另行提供正式鼠标归一化口径。这里保留 rate-neutral 事实，最终层再严格
//! 接入玩家流，并在任一输入缺失、重复、乱序、绕回或身份不一致时 fail-closed。

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::camera::{
    Intrinsics, Mat4, MatrixKind, classify, intrinsics_from_projection, pose_from_view,
};
use crate::camera_binding::{
    GameCameraBinding, MatrixSourceBinding, ViewSourceBinding, fallout_render_rotation_from_basis,
    is_sha256_digest,
};
use crate::camera_track::projection_from_matrix;
use crate::pairing::{CameraMode, FramePair};
use crate::pipeline::DepthIndexRow;

pub const CAMERA_POSE_INDEX_SCHEMA_V2: &str = "camera-pose-index/v2";
pub const CAMERA_POSE_INDEX_SCHEMA_V1: &str = "camera-pose-index/v1";
/// Opt-in measurement evidence, never accepted by ordinary camera/Depth joins.
pub const PERFORMANCE_CAMERA_POSE_INDEX_SCHEMA_V1: &str = "performance-camera-pose-index/v1";
pub const PLAYER_POSE_INDEX_SCHEMA_V1: &str = "player-pose-index/v1";
/// v2: the actor root is associated with its Present by camera content
/// (`cp2077_pose_join`), not by the moment it was read. v1 rows carried whatever simulation
/// state memory held at Present time, which is usually one frame ahead of the rendered frame
/// and slips around stalls, so they are no longer production-authoritative.
pub const CP2077_PLAYER_POSE_SOURCE: &str = "cp2077-player-puppet-world-transform-exact-build/v2";

/// 一次采集 run 的身份。`generation` 用来隔离 stop/resume 后的新 run。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamIdentity {
    pub session_id: String,
    pub run_id: String,
    pub generation: u64,
}

/// Compact identity retained by the final pose sidecar after the large per-draw provenance has
/// been stripped. Probe-backed rows keep the exact run identity; telemetry lanes state their
/// provider explicitly instead of inventing a probe generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum FinalCameraIdentity {
    ProbeStream {
        session_id: String,
        run_id: String,
        generation: u64,
    },
    GameTelemetry {
        provider: String,
    },
}

impl From<&StreamIdentity> for FinalCameraIdentity {
    fn from(identity: &StreamIdentity) -> Self {
        Self::ProbeStream {
            session_id: identity.session_id.clone(),
            run_id: identity.run_id.clone(),
            generation: identity.generation,
        }
    }
}

/// 带 run 身份的一组行。身份属于整个旁车文件，不靠逐行猜测。
#[derive(Debug)]
pub struct IdentifiedRows<'a, T> {
    pub identity: &'a StreamIdentity,
    pub rows: &'a [T],
}

impl<'a, T> IdentifiedRows<'a, T> {
    #[must_use]
    pub const fn new(identity: &'a StreamIdentity, rows: &'a [T]) -> Self {
        Self { identity, rows }
    }
}

/// 能进入逐帧 pose 旁车的具体来源。
///
/// 这里没有 `camera_candidate` 变体：camera-track 的聚合候选必须先完成主相机选择，
/// 不能把“看起来像相机”的候选统计冒充逐帧 pose。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CameraPoseSource {
    ResolvedD3d12CameraHook,
    ResolvedMainCameraHook,
    ResolvedRenderCameraHybrid,
    ResolvedCombinedViewProjection,
    GameTelemetry,
}

/// Camera pose 的来源证据。production hook 必须逐行带全 binding/profile/build/DSV 与
/// 原始 view/projection；game telemetry 也必须写明 provider，不能只剩一组裸数。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CameraPoseProvenance {
    ResolvedD3d12CameraHook {
        binding: Box<crate::D3d12CameraBinding>,
        dsv_id: String,
        view_matrix: Box<Mat4>,
        projection_matrix: Box<Mat4>,
    },
    ResolvedMainCameraHook {
        binding_id: String,
        binding_version: String,
        profile_id: String,
        profile_version: String,
        executable_sha256: String,
        dsv_id: String,
        render_width: u32,
        render_height: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        view_metres_per_unit: Option<String>,
        view_source: ViewSourceBinding,
        projection_source: MatrixSourceBinding,
        view_matrix: Box<Mat4>,
        projection_matrix: Box<Mat4>,
    },
    GameTelemetry {
        provider: String,
        profile_id: String,
        profile_version: String,
        executable_sha256: String,
    },
    GameTelemetryMainCameraProjection {
        binding_id: String,
        binding_version: String,
        profile_id: String,
        profile_version: String,
        executable_sha256: String,
        dsv_id: String,
        render_width: u32,
        render_height: u32,
        game_camera: GameCameraBinding,
        projection_source: MatrixSourceBinding,
        projection_matrix: Box<Mat4>,
    },
    ResolvedRenderCameraHybrid {
        binding_id: String,
        binding_version: String,
        profile_id: String,
        profile_version: String,
        executable_sha256: String,
        dsv_id: String,
        render_width: u32,
        render_height: u32,
        game_camera: GameCameraBinding,
        render_basis_source: ViewSourceBinding,
        render_basis_matrix: Box<Mat4>,
        projection_source: MatrixSourceBinding,
        projection_matrix: Box<Mat4>,
    },
    ResolvedCombinedViewProjection {
        binding_id: String,
        binding_version: String,
        profile_id: String,
        profile_version: String,
        executable_sha256: String,
        dsv_id: String,
        render_width: u32,
        render_height: u32,
        view_metres_per_unit: String,
        axis_mapping: crate::D3d12AxisMapping,
        source_qpc: i64,
        root_signature_sha256: String,
        root_parameter_index: u32,
        pass_ordinal: u32,
        draw_ordinal: u32,
        projection_a: f64,
        projection_b: f64,
        projection_decomposition_residual: f64,
        held_out_depth_row_residual: f64,
    },
}

/// 仅含 Camera 事实的一行，以展开后的 `present_seq` 为主键。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CameraPoseRow {
    pub schema: String,
    pub identity: StreamIdentity,
    pub present_seq: u64,
    /// 读取该次 Present pose 时的 QPC。它与 WGC `source_qpc` 共钟，但不是同一事件。
    pub present_qpc: i64,
    pub source: CameraPoseSource,
    pub provenance: CameraPoseProvenance,
    pub camera_position: [f64; 3],
    /// `[x, y, z, w]`，w 在最后。
    pub camera_rotation_quaternion: [f64; 4],
    pub camera_intrinsics: Intrinsics,
    /// 没有可信速度时保持 `None`，不得用 `[0, 0, 0]` 伪造。
    pub camera_speed: Option<[f64; 3]>,
}

/// Why a CP2077 actor root belongs to this Present: the simulation state it was read with
/// carried an FPP camera equal to the camera this Present rendered.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cp2077CameraJoinReceipt {
    /// QPC span over which the sampler observed the joined simulation state in memory. An
    /// observation window, not the engine's publication time.
    pub state_first_qpc: i64,
    pub state_last_qpc: i64,
    pub state_reads: u32,
    /// Memory camera versus rendered camera. Bounded by the join tolerance, which is therefore
    /// also the bound on the residual frame misalignment.
    pub position_residual_metres: f64,
    pub rotation_residual_degrees: f64,
    pub candidates: u32,
    /// Same camera content seen with another actor root, excluded by the earliest-entry rule.
    pub torn_candidates: u32,
}

/// Exact-build, in-process Fallout player-root fact sampled at the same Present
/// boundary as the camera pose. Missing or unsafe memory is written to a separate
/// diagnostic stream by the probe; production rows never carry a stale pose.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlayerPoseRow {
    pub schema: String,
    pub identity: StreamIdentity,
    pub present_seq: u64,
    pub present_qpc: i64,
    pub executable_sha256: String,
    pub source: String,
    /// Observed KCD2 v3 draw identity; omitted for legacy and other games.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kcd2_root_source: Option<crate::Kcd2LivePoseSourceIdentity>,
    /// Required for CP2077 v2 and omitted for every other source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cp2077_camera_join: Option<Cp2077CameraJoinReceipt>,
    pub player_position: [f64; 3],
    /// `[x, y, z, w]`, w last.
    pub player_rotation_quaternion: [f64; 4],
    /// Actor-root displacement over Present QPC, in metres per second.
    pub player_speed: Option<[f64; 3]>,
}

pub fn validate_player_pose_row(row: &PlayerPoseRow) -> Result<(), &'static str> {
    if row.schema != PLAYER_POSE_INDEX_SCHEMA_V1 {
        return Err("player pose schema mismatch");
    }
    if row.identity.session_id.trim().is_empty() || row.identity.run_id.trim().is_empty() {
        return Err("player pose identity is incomplete");
    }
    if row.present_qpc <= 0 {
        return Err("player pose QPC is invalid");
    }
    if !is_sha256_digest(&row.executable_sha256) {
        return Err("player pose executable hash is invalid");
    }
    if !matches!(
        row.source.as_str(),
        "fallout4-player-root-exact-build/v1"
            | "bannerlord-main-party-shared-memory/v1"
            | CP2077_PLAYER_POSE_SOURCE
            | "kcd2-horizontal-player-root-exact-build/v1"
            | "kcd2-horizontal-player-root-exact-build/v2"
    ) {
        return Err("player pose source is not production-authoritative");
    }
    if row.source == CP2077_PLAYER_POSE_SOURCE {
        if row.executable_sha256 != crate::CP2077_BUILD_20383525_EXE_SHA256 {
            return Err("CP2077 player pose build is invalid");
        }
        let join = row
            .cp2077_camera_join
            .as_ref()
            .ok_or("CP2077 player pose lacks its camera join receipt")?;
        let tolerance = crate::cp2077_pose_join::CP2077_POSE_JOIN_TOLERANCE;
        // Negated comparisons so that NaN residuals are refused as well.
        if !(join.position_residual_metres >= 0.0
            && join.position_residual_metres <= tolerance.position_metres
            && join.rotation_residual_degrees >= 0.0
            && join.rotation_residual_degrees <= tolerance.rotation_degrees)
        {
            return Err("CP2077 camera join residual exceeds the join tolerance");
        }
        if join.state_first_qpc <= 0
            || join.state_last_qpc < join.state_first_qpc
            || join.state_first_qpc > row.present_qpc
            || join.state_reads == 0
            || join.candidates == 0
        {
            return Err("CP2077 camera join receipt is not causal or is empty");
        }
    } else if row.cp2077_camera_join.is_some() {
        return Err("only CP2077 player poses can carry a camera join receipt");
    }
    if row.source == "kcd2-horizontal-player-root-exact-build/v2" {
        let source = row
            .kcd2_root_source
            .as_ref()
            .ok_or("KCD2 v2 player pose lacks its observed root source")?;
        if row.executable_sha256 != crate::KCD2_EXECUTABLE_SHA256
            || crate::Kcd2LivePosePlan::pinned_v3().classify_source(
                &source.root_signature_sha256,
                source.root_parameter_index,
                source.pass_ordinal,
                source.draw_ordinal,
                0,
                256,
            ) != Some(crate::Kcd2LivePoseSourceKind::HorizontalPlayerRoot)
        {
            return Err("KCD2 v2 player pose root source/build is invalid");
        }
    } else if row.kcd2_root_source.is_some() {
        return Err("legacy or non-KCD2 player pose cannot carry a KCD2 root source");
    }
    if !row.player_position.iter().all(|value| value.is_finite()) {
        return Err("player position contains a non-finite value");
    }
    if !row
        .player_rotation_quaternion
        .iter()
        .all(|value| value.is_finite())
    {
        return Err("player rotation contains a non-finite value");
    }
    let norm_squared = row
        .player_rotation_quaternion
        .iter()
        .map(|value| value * value)
        .sum::<f64>();
    if (norm_squared - 1.0).abs() > 0.002 {
        return Err("player rotation quaternion is not normalized");
    }
    if row
        .player_speed
        .is_some_and(|speed| !speed.iter().all(|value| value.is_finite()))
    {
        return Err("player speed contains a non-finite value");
    }
    Ok(())
}

/// `frames.jsonl` 中保留下来的原始动作事件。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vk: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scan_code: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flags: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dx: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dy: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delta: Option<i16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub horizontal: Option<bool>,
}

/// 仅含 Action 与它所属时间窗的一行，可直接读取当前 `frames.jsonl` 行。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionFrameRow {
    pub frame: u64,
    pub session_frame: u64,
    pub run_id: String,
    pub video_t_us: u64,
    pub output_qpc_start: i64,
    pub output_qpc_end: i64,
    pub source_qpc: i64,
    pub source_seq: u64,
    pub duplicate: bool,
    pub source_superseded: u32,
    pub queue_drop: u64,
    pub keys: Vec<u16>,
    pub buttons: Vec<u16>,
    pub actions: Vec<String>,
    /// capture-core 为下游离散控制而夹到固定范围的值；严格合成不会读取它。
    #[serde(rename = "pointerDx")]
    pub clamped_pointer_dx: i32,
    /// capture-core 为下游离散控制而夹到固定范围的值；严格合成不会读取它。
    #[serde(rename = "pointerDy")]
    pub clamped_pointer_dy: i32,
    #[serde(rename = "pointerBins")]
    pub pointer_bins: [i32; 2],
    pub events: Vec<ActionEvent>,
    pub src: String,
}

/// 一帧内所有 `mouse_move` 原始事件的和，单位仍是 Raw Input count。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawMouseDelta {
    pub dx: i64,
    pub dy: i64,
}

impl ActionFrameRow {
    /// 只从原始事件求和；绝不读取已经 clamp 的 `pointerDx` / `pointerDy`。
    pub fn raw_mouse_delta(&self) -> Result<RawMouseDelta, CameraActionJoinError> {
        let mut raw = RawMouseDelta::default();
        for (event_index, event) in self.events.iter().enumerate() {
            if event.event_type != "mouse_move" {
                continue;
            }
            let (Some(dx), Some(dy)) = (event.dx, event.dy) else {
                return Err(CameraActionJoinError::MalformedMouseMove {
                    session_frame: self.session_frame,
                    event_index,
                });
            };
            raw.dx = raw.dx.checked_add(i64::from(dx)).ok_or(
                CameraActionJoinError::RawMouseOverflow {
                    session_frame: self.session_frame,
                    axis: "x",
                },
            )?;
            raw.dy = raw.dy.checked_add(i64::from(dy)).ok_or(
                CameraActionJoinError::RawMouseOverflow {
                    session_frame: self.session_frame,
                    axis: "y",
                },
            )?;
        }
        Ok(raw)
    }
}

/// 纯函数 join 的四路输入。输出频率完全由 `depth` 行决定，不内置 15/30fps 假设。
#[derive(Debug)]
pub struct CameraActionJoinInputs<'a> {
    pub depth: IdentifiedRows<'a, DepthIndexRow>,
    pub camera: IdentifiedRows<'a, CameraPoseRow>,
    pub actions: IdentifiedRows<'a, ActionFrameRow>,
    pub frame_pairs: IdentifiedRows<'a, FramePair>,
}

/// Full-rate RGB join. Unlike [`CameraActionJoinInputs`], its output is driven by
/// every frame pair, including odd RGB frames that intentionally have no depth.
#[derive(Debug)]
pub struct FullRateCameraActionJoinInputs<'a> {
    pub camera: IdentifiedRows<'a, CameraPoseRow>,
    pub actions: IdentifiedRows<'a, ActionFrameRow>,
    pub frame_pairs: IdentifiedRows<'a, FramePair>,
}

/// 严格 join 后的 rate-neutral 事实行。
///
/// 它故意没有 `player_*`、`camera_follow_offset`、归一化 mouse 或最终交付字段；
/// 未裁决的数据不能在这里“顺手”补成 0。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JoinedCameraActionRow {
    pub identity: StreamIdentity,
    pub present_seq: u64,
    pub session_frame: u64,
    pub video_t_us: u64,
    pub output_qpc_start: i64,
    pub output_qpc_end: i64,
    pub source_qpc: i64,
    pub camera_present_qpc: i64,
    pub camera_mode: CameraMode,
    pub camera_source: CameraPoseSource,
    pub camera_position: [f64; 3],
    pub camera_rotation_quaternion: [f64; 4],
    pub camera_intrinsics: Intrinsics,
    pub camera_speed: Option<[f64; 3]>,
    pub keys: Vec<u16>,
    pub buttons: Vec<u16>,
    pub actions: Vec<String>,
    pub raw_mouse_delta: RawMouseDelta,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FinalPoseRow {
    pub present_seq: u64,
    pub session_frame: u64,
    pub camera_identity: FinalCameraIdentity,
    pub camera_source: CameraPoseSource,
    pub time: String,
    /// Dynamic game Present rate measured from the camera pose QPC timeline.
    pub fps: f64,
    pub camera_position: [f64; 3],
    pub camera_rotation_quaternion: [f64; 4],
    pub camera_follow_offset: [f64; 3],
    pub camera_intrinsics: Intrinsics,
    pub camera_speed: [f64; 3],
    pub player_position: [f64; 3],
    pub player_rotation_quaternion: [f64; 4],
    pub player_speed: [f64; 3],
    pub mouse_x: f64,
    pub mouse_y: f64,
    pub mouse_dx: f64,
    pub mouse_dy: f64,
    pub key_code: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MouseNormalization {
    pub evidence_id: String,
    pub initial_x: f64,
    pub initial_y: f64,
    pub horizontal_counts_per_unit: f64,
    pub vertical_counts_per_unit: f64,
    pub delta_counts_full_scale: f64,
}

impl MouseNormalization {
    fn validate(&self) -> Result<(), &'static str> {
        if self.evidence_id.trim().is_empty() {
            return Err("mouse normalization evidence is missing");
        }
        if !(0.0..=1.0).contains(&self.initial_x) || !(0.0..=1.0).contains(&self.initial_y) {
            return Err("mouse normalization initial position is outside [0,1]");
        }
        if ![
            self.horizontal_counts_per_unit,
            self.vertical_counts_per_unit,
            self.delta_counts_full_scale,
        ]
        .into_iter()
        .all(|value| value.is_finite() && value > 0.0)
        {
            return Err("mouse normalization scale is invalid");
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct FinalCameraActionInputs<'a> {
    pub identity: &'a StreamIdentity,
    pub joined: &'a [JoinedCameraActionRow],
    pub player: IdentifiedRows<'a, PlayerPoseRow>,
    pub wallclock_by_session_frame: &'a BTreeMap<u64, String>,
    pub qpc_frequency_hz: u64,
    pub mouse: &'a MouseNormalization,
}

pub fn assemble_final_pose_rows(
    inputs: FinalCameraActionInputs<'_>,
) -> Result<Vec<FinalPoseRow>, String> {
    inputs.mouse.validate().map_err(str::to_owned)?;
    if inputs.qpc_frequency_hz == 0 {
        return Err("final action-camera QPC frequency is zero".into());
    }
    require_same_identity("player", inputs.identity, inputs.player.identity)
        .map_err(|error| error.to_string())?;
    if inputs.joined.is_empty() {
        return Err("final action-camera join is empty".into());
    }
    let mut player_by_present = BTreeMap::new();
    for row in inputs.player.rows {
        validate_player_pose_row(row).map_err(str::to_owned)?;
        if &row.identity != inputs.identity {
            return Err(format!("player row {} identity mismatch", row.present_seq));
        }
        if player_by_present.insert(row.present_seq, row).is_some() {
            return Err(format!("duplicate player pose {}", row.present_seq));
        }
    }
    let mut mouse_x = inputs.mouse.initial_x;
    let mut mouse_y = inputs.mouse.initial_y;
    let mut output = Vec::with_capacity(inputs.joined.len());
    if inputs.joined.len() < 2 {
        return Err("final action-camera needs at least two rows for camera velocity".into());
    }
    let mut camera_speeds = Vec::with_capacity(inputs.joined.len());
    for index in 0..inputs.joined.len() {
        let (from, to) = if index + 1 < inputs.joined.len() {
            (&inputs.joined[index], &inputs.joined[index + 1])
        } else {
            (&inputs.joined[index - 1], &inputs.joined[index])
        };
        if to.output_qpc_start <= from.output_qpc_start {
            return Err(format!(
                "video output QPC did not increase: {} -> {}",
                from.output_qpc_start, to.output_qpc_start
            ));
        }
        let dt =
            (to.output_qpc_start - from.output_qpc_start) as f64 / inputs.qpc_frequency_hz as f64;
        camera_speeds.push(std::array::from_fn(|axis| {
            (to.camera_position[axis] - from.camera_position[axis]) / dt
        }));
    }
    let max_skew_qpc = i64::try_from(inputs.qpc_frequency_hz / 30)
        .map_err(|_| "QPC frequency does not fit i64")?;
    for (row_index, joined) in inputs.joined.iter().enumerate() {
        if &joined.identity != inputs.identity {
            return Err(format!(
                "joined row {} identity mismatch",
                joined.present_seq
            ));
        }
        let player = player_by_present
            .get(&joined.present_seq)
            .copied()
            .ok_or_else(|| format!("missing player pose {}", joined.present_seq))?;
        let skew = joined.camera_present_qpc.abs_diff(player.present_qpc);
        if skew > max_skew_qpc as u64 {
            return Err(format!(
                "camera/player QPC skew at {} exceeds one 30fps frame: {skew}",
                joined.present_seq
            ));
        }
        let camera_speed = camera_speeds[row_index];
        let player_speed = player
            .player_speed
            .ok_or_else(|| format!("player speed missing at {}", joined.present_seq))?;
        let time = inputs
            .wallclock_by_session_frame
            .get(&joined.session_frame)
            .ok_or_else(|| format!("wall clock missing for frame {}", joined.session_frame))?
            .clone();
        if time.trim().is_empty() {
            return Err(format!(
                "wall clock is empty for frame {}",
                joined.session_frame
            ));
        }
        let mouse_dx = (joined.raw_mouse_delta.dx as f64 / inputs.mouse.delta_counts_full_scale)
            .clamp(-1.0, 1.0);
        let mouse_dy = (joined.raw_mouse_delta.dy as f64 / inputs.mouse.delta_counts_full_scale)
            .clamp(-1.0, 1.0);
        mouse_x = (mouse_x
            + joined.raw_mouse_delta.dx as f64 / inputs.mouse.horizontal_counts_per_unit)
            .clamp(0.0, 1.0);
        mouse_y = (mouse_y
            + joined.raw_mouse_delta.dy as f64 / inputs.mouse.vertical_counts_per_unit)
            .clamp(0.0, 1.0);
        output.push(FinalPoseRow {
            present_seq: joined.present_seq,
            session_frame: joined.session_frame,
            camera_identity: FinalCameraIdentity::from(&joined.identity),
            camera_source: joined.camera_source,
            time,
            fps: dynamic_present_fps(inputs.joined, row_index, inputs.qpc_frequency_hz)?,
            camera_position: joined.camera_position,
            camera_rotation_quaternion: joined.camera_rotation_quaternion,
            camera_follow_offset: std::array::from_fn(|axis| {
                joined.camera_position[axis] - player.player_position[axis]
            }),
            camera_intrinsics: joined.camera_intrinsics,
            camera_speed,
            player_position: player.player_position,
            player_rotation_quaternion: player.player_rotation_quaternion,
            player_speed,
            mouse_x,
            mouse_y,
            mouse_dx,
            mouse_dy,
            key_code: joined.keys.iter().copied().map(u32::from).collect(),
        });
    }
    Ok(output)
}

fn dynamic_present_fps(
    rows: &[JoinedCameraActionRow],
    index: usize,
    qpc_frequency_hz: u64,
) -> Result<f64, String> {
    let current = &rows[index];
    let neighbour = rows[index + 1..]
        .iter()
        .find(|row| row.present_seq != current.present_seq)
        .or_else(|| {
            rows[..index]
                .iter()
                .rev()
                .find(|row| row.present_seq != current.present_seq)
        })
        .ok_or_else(|| "dynamic fps needs at least two distinct Present samples".to_string())?;
    let seq_delta = current.present_seq.abs_diff(neighbour.present_seq);
    let qpc_delta = current
        .camera_present_qpc
        .abs_diff(neighbour.camera_present_qpc);
    if seq_delta == 0 || qpc_delta == 0 {
        return Err(format!(
            "dynamic fps has a zero delta at present {}",
            current.present_seq
        ));
    }
    let fps = qpc_frequency_hz as f64 * seq_delta as f64 / qpc_delta as f64;
    if !fps.is_finite() || fps <= 0.0 {
        return Err(format!(
            "dynamic fps is invalid at present {}",
            current.present_seq
        ));
    }
    Ok(fps)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CameraActionJoinError {
    EmptyStream {
        stream: &'static str,
    },
    IdentityMismatch {
        stream: &'static str,
        expected: Box<StreamIdentity>,
        actual: Box<StreamIdentity>,
    },
    InvalidIdentity {
        stream: &'static str,
        field: &'static str,
    },
    RowRunIdMismatch {
        session_frame: u64,
        expected: String,
        actual: String,
    },
    DuplicateKey {
        stream: &'static str,
        key: u64,
    },
    SequenceNotIncreasing {
        stream: &'static str,
        previous: u64,
        current: u64,
    },
    SequenceGap {
        stream: &'static str,
        previous: u64,
        current: u64,
    },
    QpcNotIncreasing {
        stream: &'static str,
        previous: i64,
        current: i64,
    },
    InvalidQpcWindow {
        session_frame: u64,
        start: i64,
        end: i64,
    },
    OverlappingQpcWindows {
        previous_session_frame: u64,
        previous_end: i64,
        session_frame: u64,
        start: i64,
    },
    InvalidCameraPose {
        present_seq: u64,
        reason: &'static str,
    },
    MalformedMouseMove {
        session_frame: u64,
        event_index: usize,
    },
    RawMouseOverflow {
        session_frame: u64,
        axis: &'static str,
    },
    MissingAction {
        session_frame: u64,
    },
    UnexpectedAction {
        session_frame: u64,
    },
    SourceQpcMismatch {
        session_frame: u64,
        pair_qpc: i64,
        action_qpc: i64,
    },
    DuplicateFlagMismatch {
        session_frame: u64,
        pair_duplicate: bool,
        action_duplicate: bool,
    },
    PairInheritanceMismatch {
        session_frame: u64,
        pair_duplicate: bool,
        depth_inherited: bool,
    },
    PairDepthNameMismatch {
        session_frame: u64,
        expected: String,
        actual: String,
    },
    MissingFramePair {
        present_seq: u64,
    },
    UnexpectedFramePair {
        present_seq: u64,
    },
    DsvIdentityMismatch {
        present_seq: u64,
        depth_index_dsv_id: String,
        frame_pair_dsv_id: String,
    },
    CameraDsvIdentityMismatch {
        present_seq: u64,
        depth_index_dsv_id: String,
        camera_dsv_id: String,
    },
    MissingDsvIdentity {
        stream: &'static str,
        present_seq: u64,
    },
    MissingCameraPose {
        present_seq: u64,
    },
    MissingRgbPresent {
        session_frame: u64,
    },
}

impl std::fmt::Display for CameraActionJoinError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Camera/Action 严格合成失败: {self:?}")
    }
}

impl std::error::Error for CameraActionJoinError {}

/// 将 depth-index、逐帧 camera pose、frames/actions 与 frame-pairs 严格合成。
///
/// 任一路输入有歧义就返回错误，不丢行、不覆盖重复 key，也不做计数器绕回猜测。
pub fn join_camera_action(
    inputs: CameraActionJoinInputs<'_>,
) -> Result<Vec<JoinedCameraActionRow>, CameraActionJoinError> {
    let expected_identity = inputs.depth.identity;
    validate_identity("depth", expected_identity)?;
    validate_identity("camera", inputs.camera.identity)?;
    validate_identity("actions", inputs.actions.identity)?;
    validate_identity("frame_pairs", inputs.frame_pairs.identity)?;
    require_same_identity("camera", expected_identity, inputs.camera.identity)?;
    require_same_identity("actions", expected_identity, inputs.actions.identity)?;
    require_same_identity(
        "frame_pairs",
        expected_identity,
        inputs.frame_pairs.identity,
    )?;

    require_non_empty("depth", inputs.depth.rows)?;
    require_non_empty("camera", inputs.camera.rows)?;
    require_non_empty("actions", inputs.actions.rows)?;
    require_non_empty("frame_pairs", inputs.frame_pairs.rows)?;

    validate_depth_rows(inputs.depth.rows)?;
    validate_camera_rows(inputs.camera.rows, expected_identity)?;
    validate_action_rows(inputs.actions.rows, expected_identity)?;
    validate_frame_pairs(inputs.frame_pairs.rows)?;

    let camera_by_present = inputs
        .camera
        .rows
        .iter()
        .map(|row| (row.present_seq, row))
        .collect::<BTreeMap<_, _>>();

    let mut action_by_frame = BTreeMap::new();
    for action in inputs.actions.rows {
        action_by_frame.insert(action.session_frame, (action, action.raw_mouse_delta()?));
    }

    let mut real_pair_by_present = BTreeMap::new();
    for pair in inputs.frame_pairs.rows {
        let Some((action, _)) = action_by_frame.get(&pair.session_frame) else {
            return Err(CameraActionJoinError::MissingAction {
                session_frame: pair.session_frame,
            });
        };
        if pair.source_qpc != action.source_qpc {
            return Err(CameraActionJoinError::SourceQpcMismatch {
                session_frame: pair.session_frame,
                pair_qpc: pair.source_qpc,
                action_qpc: action.source_qpc,
            });
        }
        if pair.duplicate != action.duplicate {
            return Err(CameraActionJoinError::DuplicateFlagMismatch {
                session_frame: pair.session_frame,
                pair_duplicate: pair.duplicate,
                action_duplicate: action.duplicate,
            });
        }

        if let Some(depth) = &pair.depth {
            if pair.duplicate != depth.inherited {
                return Err(CameraActionJoinError::PairInheritanceMismatch {
                    session_frame: pair.session_frame,
                    pair_duplicate: pair.duplicate,
                    depth_inherited: depth.inherited,
                });
            }
            if depth.dsv_id.trim().is_empty() {
                return Err(CameraActionJoinError::MissingDsvIdentity {
                    stream: "frame_pairs",
                    present_seq: depth.present_seq,
                });
            }
            // inherited 行复用的是上一真实帧的 depth_name；它不进入输出，也不能按当前
            // duplicate session_frame 改名。只对真实 depth 执行命名与唯一性检查。
            if !depth.inherited {
                let expected_name = format!("depth_{}", pair.session_frame);
                if depth.depth_name != expected_name {
                    return Err(CameraActionJoinError::PairDepthNameMismatch {
                        session_frame: pair.session_frame,
                        expected: expected_name,
                        actual: depth.depth_name.clone(),
                    });
                }
                if real_pair_by_present
                    .insert(depth.present_seq, pair)
                    .is_some()
                {
                    return Err(CameraActionJoinError::DuplicateKey {
                        stream: "frame_pairs.present_seq",
                        key: depth.present_seq,
                    });
                }
            }
        }
    }

    let pair_frames = inputs
        .frame_pairs
        .rows
        .iter()
        .map(|row| row.session_frame)
        .collect::<BTreeSet<_>>();
    for action in inputs.actions.rows {
        if !pair_frames.contains(&action.session_frame) {
            return Err(CameraActionJoinError::UnexpectedAction {
                session_frame: action.session_frame,
            });
        }
    }

    let depth_present = inputs
        .depth
        .rows
        .iter()
        .map(|row| row.present_seq)
        .collect::<BTreeSet<_>>();
    for present_seq in real_pair_by_present.keys() {
        if !depth_present.contains(present_seq) {
            return Err(CameraActionJoinError::UnexpectedFramePair {
                present_seq: *present_seq,
            });
        }
    }

    let mut joined = Vec::with_capacity(inputs.depth.rows.len());
    for depth in inputs.depth.rows {
        let pair = real_pair_by_present.get(&depth.present_seq).ok_or(
            CameraActionJoinError::MissingFramePair {
                present_seq: depth.present_seq,
            },
        )?;
        let Some(paired_depth) = &pair.depth else {
            return Err(CameraActionJoinError::MissingFramePair {
                present_seq: depth.present_seq,
            });
        };
        if paired_depth.dsv_id != depth.dsv_id {
            return Err(CameraActionJoinError::DsvIdentityMismatch {
                present_seq: depth.present_seq,
                depth_index_dsv_id: depth.dsv_id.clone(),
                frame_pair_dsv_id: paired_depth.dsv_id.clone(),
            });
        }
        let camera = camera_by_present.get(&depth.present_seq).ok_or(
            CameraActionJoinError::MissingCameraPose {
                present_seq: depth.present_seq,
            },
        )?;
        let camera_dsv_id = match &camera.provenance {
            CameraPoseProvenance::ResolvedD3d12CameraHook { dsv_id, .. }
            | CameraPoseProvenance::ResolvedMainCameraHook { dsv_id, .. }
            | CameraPoseProvenance::ResolvedRenderCameraHybrid { dsv_id, .. }
            | CameraPoseProvenance::ResolvedCombinedViewProjection { dsv_id, .. }
            | CameraPoseProvenance::GameTelemetryMainCameraProjection { dsv_id, .. } => {
                Some(dsv_id)
            }
            CameraPoseProvenance::GameTelemetry { .. } => None,
        };
        if camera_dsv_id.is_some_and(|dsv_id| dsv_id != &depth.dsv_id) {
            return Err(CameraActionJoinError::CameraDsvIdentityMismatch {
                present_seq: depth.present_seq,
                depth_index_dsv_id: depth.dsv_id.clone(),
                camera_dsv_id: camera_dsv_id.cloned().unwrap_or_default(),
            });
        }
        let Some((action, raw_mouse_delta)) = action_by_frame.get(&pair.session_frame) else {
            return Err(CameraActionJoinError::MissingAction {
                session_frame: pair.session_frame,
            });
        };

        joined.push(JoinedCameraActionRow {
            identity: expected_identity.clone(),
            present_seq: depth.present_seq,
            session_frame: pair.session_frame,
            video_t_us: action.video_t_us,
            output_qpc_start: action.output_qpc_start,
            output_qpc_end: action.output_qpc_end,
            source_qpc: action.source_qpc,
            camera_present_qpc: camera.present_qpc,
            camera_mode: pair.camera_mode,
            camera_source: camera.source,
            camera_position: camera.camera_position,
            camera_rotation_quaternion: camera.camera_rotation_quaternion,
            camera_intrinsics: camera.camera_intrinsics,
            camera_speed: camera.camera_speed,
            keys: action.keys.clone(),
            buttons: action.buttons.clone(),
            actions: action.actions.clone(),
            raw_mouse_delta: *raw_mouse_delta,
        });
    }
    Ok(joined)
}

/// Strictly joins every RGB frame to its watermark-selected camera pose and Raw
/// Input row. No nearest-frame or interpolation path exists: a missing odd-frame
/// watermark is just as fatal as a missing even-frame watermark.
pub fn join_camera_action_full_rate(
    inputs: FullRateCameraActionJoinInputs<'_>,
) -> Result<Vec<JoinedCameraActionRow>, CameraActionJoinError> {
    let expected_identity = inputs.camera.identity;
    validate_identity("camera", expected_identity)?;
    validate_identity("actions", inputs.actions.identity)?;
    validate_identity("frame_pairs", inputs.frame_pairs.identity)?;
    require_same_identity("actions", expected_identity, inputs.actions.identity)?;
    require_same_identity(
        "frame_pairs",
        expected_identity,
        inputs.frame_pairs.identity,
    )?;
    require_non_empty("camera", inputs.camera.rows)?;
    require_non_empty("actions", inputs.actions.rows)?;
    require_non_empty("frame_pairs", inputs.frame_pairs.rows)?;
    validate_camera_rows(inputs.camera.rows, expected_identity)?;
    validate_action_rows(inputs.actions.rows, expected_identity)?;
    validate_frame_pairs(inputs.frame_pairs.rows)?;

    let camera_by_present = inputs
        .camera
        .rows
        .iter()
        .map(|row| (row.present_seq, row))
        .collect::<BTreeMap<_, _>>();
    let mut action_by_frame = BTreeMap::new();
    for action in inputs.actions.rows {
        action_by_frame.insert(action.session_frame, (action, action.raw_mouse_delta()?));
    }
    let pair_frames = inputs
        .frame_pairs
        .rows
        .iter()
        .map(|row| row.session_frame)
        .collect::<BTreeSet<_>>();
    for action in inputs.actions.rows {
        if !pair_frames.contains(&action.session_frame) {
            return Err(CameraActionJoinError::UnexpectedAction {
                session_frame: action.session_frame,
            });
        }
    }

    let mut joined = Vec::with_capacity(inputs.frame_pairs.rows.len());
    for pair in inputs.frame_pairs.rows {
        let (action, raw_mouse_delta) = action_by_frame.get(&pair.session_frame).ok_or(
            CameraActionJoinError::MissingAction {
                session_frame: pair.session_frame,
            },
        )?;
        if pair.source_qpc != action.source_qpc {
            return Err(CameraActionJoinError::SourceQpcMismatch {
                session_frame: pair.session_frame,
                pair_qpc: pair.source_qpc,
                action_qpc: action.source_qpc,
            });
        }
        if pair.duplicate != action.duplicate {
            return Err(CameraActionJoinError::DuplicateFlagMismatch {
                session_frame: pair.session_frame,
                pair_duplicate: pair.duplicate,
                action_duplicate: action.duplicate,
            });
        }
        let present_seq = pair
            .present_seq
            .or_else(|| pair.depth.as_ref().map(|depth| depth.present_seq))
            .ok_or(CameraActionJoinError::MissingRgbPresent {
                session_frame: pair.session_frame,
            })?;
        let camera = camera_by_present
            .get(&present_seq)
            .ok_or(CameraActionJoinError::MissingCameraPose { present_seq })?;
        joined.push(JoinedCameraActionRow {
            identity: expected_identity.clone(),
            present_seq,
            session_frame: pair.session_frame,
            video_t_us: action.video_t_us,
            output_qpc_start: action.output_qpc_start,
            output_qpc_end: action.output_qpc_end,
            source_qpc: action.source_qpc,
            camera_present_qpc: camera.present_qpc,
            camera_mode: pair.camera_mode,
            camera_source: camera.source,
            camera_position: camera.camera_position,
            camera_rotation_quaternion: camera.camera_rotation_quaternion,
            camera_intrinsics: camera.camera_intrinsics,
            camera_speed: camera.camera_speed,
            keys: action.keys.clone(),
            buttons: action.buttons.clone(),
            actions: action.actions.clone(),
            raw_mouse_delta: *raw_mouse_delta,
        });
    }
    Ok(joined)
}

fn require_same_identity(
    stream: &'static str,
    expected: &StreamIdentity,
    actual: &StreamIdentity,
) -> Result<(), CameraActionJoinError> {
    if expected == actual {
        Ok(())
    } else {
        Err(CameraActionJoinError::IdentityMismatch {
            stream,
            expected: Box::new(expected.clone()),
            actual: Box::new(actual.clone()),
        })
    }
}

fn validate_identity(
    stream: &'static str,
    identity: &StreamIdentity,
) -> Result<(), CameraActionJoinError> {
    if identity.session_id.trim().is_empty() {
        return Err(CameraActionJoinError::InvalidIdentity {
            stream,
            field: "session_id",
        });
    }
    if identity.run_id.trim().is_empty() {
        return Err(CameraActionJoinError::InvalidIdentity {
            stream,
            field: "run_id",
        });
    }
    Ok(())
}

fn require_non_empty<T>(stream: &'static str, rows: &[T]) -> Result<(), CameraActionJoinError> {
    if rows.is_empty() {
        Err(CameraActionJoinError::EmptyStream { stream })
    } else {
        Ok(())
    }
}

fn validate_depth_rows(rows: &[DepthIndexRow]) -> Result<(), CameraActionJoinError> {
    validate_strictly_increasing(
        "depth.present_seq",
        rows.iter().map(|row| row.present_seq),
        false,
    )?;
    for row in rows {
        if row.dsv_id.trim().is_empty() {
            return Err(CameraActionJoinError::MissingDsvIdentity {
                stream: "depth",
                present_seq: row.present_seq,
            });
        }
    }
    Ok(())
}

fn validate_camera_rows(
    rows: &[CameraPoseRow],
    identity: &StreamIdentity,
) -> Result<(), CameraActionJoinError> {
    validate_strictly_increasing(
        "camera.present_seq",
        rows.iter().map(|row| row.present_seq),
        true,
    )?;
    for (index, row) in rows.iter().enumerate() {
        if &row.identity != identity {
            return Err(CameraActionJoinError::IdentityMismatch {
                stream: "camera row",
                expected: Box::new(identity.clone()),
                actual: Box::new(row.identity.clone()),
            });
        }
        if let Some(previous) = index.checked_sub(1).and_then(|index| rows.get(index))
            && row.present_qpc <= previous.present_qpc
        {
            return Err(CameraActionJoinError::QpcNotIncreasing {
                stream: "camera.present_qpc",
                previous: previous.present_qpc,
                current: row.present_qpc,
            });
        }
        validate_camera_pose(row)?;
    }
    Ok(())
}

fn validate_camera_pose(row: &CameraPoseRow) -> Result<(), CameraActionJoinError> {
    validate_camera_pose_row(row).map_err(|reason| CameraActionJoinError::InvalidCameraPose {
        present_seq: row.present_seq,
        reason,
    })
}

/// 单行 production writer 与 CA-1 join 共用的 Camera pose 校验。
pub fn validate_camera_pose_row(row: &CameraPoseRow) -> Result<(), &'static str> {
    validate_camera_pose_row_with_policy(row, false)
}

/// Validate explicitly non-deliverable KCD2 performance evidence. The caller
/// must select this path from its immutable performance-only run contract;
/// ordinary readers always use `validate_camera_pose_row` and reject its schema.
pub fn validate_performance_camera_pose_row(row: &CameraPoseRow) -> Result<(), &'static str> {
    if row.source != CameraPoseSource::ResolvedCombinedViewProjection {
        return Err("performance camera pose requires the combined view-projection source");
    }
    validate_camera_pose_row_with_policy(row, true)
}

fn validate_camera_pose_row_with_policy(
    row: &CameraPoseRow,
    performance_only: bool,
) -> Result<(), &'static str> {
    let expected_schema = if performance_only {
        PERFORMANCE_CAMERA_POSE_INDEX_SCHEMA_V1
    } else if row.source == CameraPoseSource::ResolvedD3d12CameraHook {
        CAMERA_POSE_INDEX_SCHEMA_V2
    } else {
        CAMERA_POSE_INDEX_SCHEMA_V1
    };
    if row.schema != expected_schema {
        return Err("unsupported camera pose index schema");
    }
    if row.identity.session_id.trim().is_empty() || row.identity.run_id.trim().is_empty() {
        return Err("camera stream identity is empty");
    }
    if row.present_qpc <= 0 {
        return Err("camera sample QPC is invalid");
    }
    match (&row.source, &row.provenance) {
        (
            CameraPoseSource::ResolvedD3d12CameraHook,
            CameraPoseProvenance::ResolvedD3d12CameraHook {
                binding,
                dsv_id,
                view_matrix,
                projection_matrix,
            },
        ) => {
            if dsv_id.trim().is_empty() || row.identity.generation == 0 {
                return Err("D3D12 camera DSV identity is empty");
            }
            let (position, rotation, intrinsics) =
                crate::d3d12_camera::resolve_pose(binding, view_matrix, projection_matrix)?;
            if !arrays_approximately_equal(&row.camera_position, &position)
                || !arrays_approximately_equal(&row.camera_rotation_quaternion, &rotation)
                || !intrinsics_approximately_equal(row.camera_intrinsics, intrinsics)
            {
                return Err("D3D12 camera pose disagrees with its bound matrices");
            }
        }
        (
            CameraPoseSource::ResolvedMainCameraHook,
            CameraPoseProvenance::ResolvedMainCameraHook {
                binding_id,
                binding_version,
                profile_id,
                profile_version,
                executable_sha256,
                dsv_id,
                render_width,
                render_height,
                view_metres_per_unit,
                view_source,
                projection_source,
                view_matrix,
                projection_matrix,
            },
        ) => {
            if binding_id.trim().is_empty()
                || binding_version.trim().is_empty()
                || profile_id.trim().is_empty()
                || profile_version.trim().is_empty()
                || dsv_id.trim().is_empty()
                || *render_width == 0
                || *render_height == 0
            {
                return Err("hook camera provenance identity is empty");
            }
            if !is_sha256_digest(executable_sha256) {
                return Err("hook camera executable hash is invalid");
            }
            let view_metres_per_unit = view_metres_per_unit
                .as_deref()
                .ok_or("hook camera metric scale is missing")?
                .parse::<f64>()
                .map_err(|_| "hook camera metric scale is not numeric")?;
            if !view_metres_per_unit.is_finite() || view_metres_per_unit <= 0.0 {
                return Err("hook camera metric scale is not positive and finite");
            }
            if view_source.source.role != crate::camera_binding::MatrixRole::View
                || projection_source.role != crate::camera_binding::MatrixRole::Projection
            {
                return Err("hook camera matrix role is invalid");
            }
            if view_source.source.pass_ordinal != projection_source.pass_ordinal
                || view_source.source.draw_ordinal != projection_source.draw_ordinal
            {
                return Err("hook camera sources do not share one draw window");
            }
            if same_physical_matrix_source(view_source.source, *projection_source) {
                return Err("hook view and projection resolve to the same matrix bytes");
            }
            if !matrix_source_is_valid(view_source.source)
                || !matrix_source_is_valid(*projection_source)
            {
                return Err("hook camera matrix source is invalid");
            }
            if !view_matrix
                .iter()
                .flatten()
                .chain(projection_matrix.iter().flatten())
                .all(|value| value.is_finite())
            {
                return Err("hook camera provenance matrix contains a non-finite value");
            }
            if classify(view_matrix)
                != (MatrixKind::View {
                    translation: view_source.translation,
                })
                || classify(projection_matrix) != MatrixKind::Projection
                || projection_from_matrix(projection_matrix).is_none()
            {
                return Err("hook camera provenance matrix is not invertible/usable");
            }
            let derived_pose = pose_from_view(view_matrix, view_source.translation);
            let derived_position = derived_pose
                .position
                .map(|value| f64::from(value) * view_metres_per_unit);
            if !arrays_approximately_equal(&row.camera_position, &derived_position)
                || !arrays_approximately_equal(
                    &row.camera_rotation_quaternion,
                    &derived_pose.rotation.map(f64::from),
                )
            {
                return Err("hook camera pose disagrees with its view matrix");
            }
            let derived_intrinsics = intrinsics_from_projection(
                projection_matrix,
                *render_width as f32,
                *render_height as f32,
            );
            if !intrinsics_approximately_equal(row.camera_intrinsics, derived_intrinsics) {
                return Err("hook camera intrinsics disagree with its projection matrix");
            }
        }
        (
            CameraPoseSource::ResolvedRenderCameraHybrid,
            CameraPoseProvenance::ResolvedRenderCameraHybrid {
                binding_id,
                binding_version,
                profile_id,
                profile_version,
                executable_sha256,
                dsv_id,
                render_width,
                render_height,
                game_camera,
                render_basis_source,
                render_basis_matrix,
                projection_source,
                projection_matrix,
            },
        ) => {
            if binding_id.trim().is_empty()
                || binding_version.trim().is_empty()
                || profile_id.trim().is_empty()
                || profile_version.trim().is_empty()
                || dsv_id.trim().is_empty()
                || game_camera.provider.trim().is_empty()
                || *render_width == 0
                || *render_height == 0
            {
                return Err("hybrid render camera provenance identity is empty");
            }
            if !is_sha256_digest(executable_sha256)
                || render_basis_source.source.role != crate::camera_binding::MatrixRole::View
                || projection_source.role != crate::camera_binding::MatrixRole::Projection
                || !matrix_source_is_valid(render_basis_source.source)
                || !matrix_source_is_valid(*projection_source)
                || render_basis_source.source.pass_ordinal != projection_source.pass_ordinal
                || render_basis_source.source.draw_ordinal != projection_source.draw_ordinal
                || same_physical_matrix_source(render_basis_source.source, *projection_source)
                || fallout_render_rotation_from_basis(
                    render_basis_matrix,
                    render_basis_source.translation,
                )
                .is_none()
                || classify(projection_matrix) != MatrixKind::Projection
                || projection_from_matrix(projection_matrix).is_none()
            {
                return Err("hybrid render camera provenance is invalid");
            }
            let derived_rotation = fallout_render_rotation_from_basis(
                render_basis_matrix,
                render_basis_source.translation,
            )
            .ok_or("hybrid render basis cannot produce a rotation")?
            .map(f64::from);
            if !arrays_approximately_equal(&row.camera_rotation_quaternion, &derived_rotation) {
                return Err("hybrid camera rotation disagrees with its render basis");
            }
            let derived_intrinsics = intrinsics_from_projection(
                projection_matrix,
                *render_width as f32,
                *render_height as f32,
            );
            if !intrinsics_approximately_equal(row.camera_intrinsics, derived_intrinsics) {
                return Err("hybrid camera intrinsics disagree with its projection matrix");
            }
        }
        (
            CameraPoseSource::ResolvedCombinedViewProjection,
            CameraPoseProvenance::ResolvedCombinedViewProjection {
                binding_id,
                binding_version,
                profile_id,
                profile_version,
                executable_sha256,
                dsv_id,
                render_width,
                render_height,
                view_metres_per_unit,
                axis_mapping,
                source_qpc,
                root_signature_sha256,
                root_parameter_index,
                pass_ordinal,
                draw_ordinal,
                projection_a,
                projection_b,
                projection_decomposition_residual,
                held_out_depth_row_residual,
            },
        ) => {
            let metric_scale = view_metres_per_unit
                .parse::<f64>()
                .map_err(|_| "combined view-projection metric scale is not numeric")?;
            let source_plan = match (binding_id.as_str(), binding_version.as_str()) {
                (crate::KCD2_COMBINED_BINDING_ID, crate::KCD2_COMBINED_BINDING_VERSION) => {
                    crate::Kcd2LivePosePlan::pinned()
                }
                (crate::KCD2_COMBINED_BINDING_ID_V2, crate::KCD2_COMBINED_BINDING_VERSION_V2) => {
                    crate::Kcd2LivePosePlan::pinned_v3()
                }
                _ => return Err("combined view-projection binding identity is invalid"),
            };
            if profile_id != crate::KCD2_MAINLINE_GAME_ID
                || profile_version != crate::DepthProfile::D3D12_STABLE_SCHEMA_VERSION
                || executable_sha256 != crate::KCD2_EXECUTABLE_SHA256
                || dsv_id != crate::KCD2_MAIN_SCENE_DSV_ID
                || *render_width != 1920
                || *render_height != 1080
                || !metric_scale.is_finite()
                || (metric_scale - 1.0).abs() > f64::EPSILON
                || axis_mapping.delivery_x_from != crate::D3d12SignedAxis::PositiveX
                || axis_mapping.delivery_y_from != crate::D3d12SignedAxis::PositiveZ
                || axis_mapping.delivery_z_from != crate::D3d12SignedAxis::PositiveY
                || *source_qpc <= 0
                || !root_signature_sha256.starts_with("sha256:")
                || source_plan.classify_source(
                    root_signature_sha256,
                    *root_parameter_index,
                    *pass_ordinal,
                    *draw_ordinal,
                    0,
                    256,
                ) != Some(crate::Kcd2LivePoseSourceKind::ViewProjection)
                || !projection_a.is_finite()
                || !projection_b.is_finite()
                || if performance_only {
                    !valid_performance_combined_projection(*projection_a, *projection_b)
                } else {
                    (*projection_a - crate::KCD2_PROJECTION_A).abs() > 1.0e-12
                        || (*projection_b - crate::KCD2_PROJECTION_B).abs() > 1.0e-12
                }
                || !projection_decomposition_residual.is_finite()
                || *projection_decomposition_residual < 0.0
                || *projection_decomposition_residual > 1.0e-3
                || !held_out_depth_row_residual.is_finite()
                || held_out_depth_row_residual.abs() > 1.0e-3
            {
                return Err("combined view-projection camera provenance is invalid");
            }
        }
        (
            CameraPoseSource::GameTelemetry,
            CameraPoseProvenance::GameTelemetryMainCameraProjection {
                binding_id,
                binding_version,
                profile_id,
                profile_version,
                executable_sha256,
                dsv_id,
                render_width,
                render_height,
                game_camera,
                projection_source,
                projection_matrix,
            },
        ) => {
            if binding_id.trim().is_empty()
                || binding_version.trim().is_empty()
                || profile_id.trim().is_empty()
                || profile_version.trim().is_empty()
                || dsv_id.trim().is_empty()
                || game_camera.provider.trim().is_empty()
                || *render_width == 0
                || *render_height == 0
            {
                return Err("game camera/projection provenance identity is empty");
            }
            if !is_sha256_digest(executable_sha256)
                || projection_source.role != crate::camera_binding::MatrixRole::Projection
                || !matrix_source_is_valid(*projection_source)
                || classify(projection_matrix) != MatrixKind::Projection
                || projection_from_matrix(projection_matrix).is_none()
            {
                return Err("game camera/projection provenance is invalid");
            }
            let derived_intrinsics = intrinsics_from_projection(
                projection_matrix,
                *render_width as f32,
                *render_height as f32,
            );
            if !intrinsics_approximately_equal(row.camera_intrinsics, derived_intrinsics) {
                return Err("game camera intrinsics disagree with its projection matrix");
            }
        }
        (
            CameraPoseSource::GameTelemetry,
            CameraPoseProvenance::GameTelemetry {
                provider,
                profile_id,
                profile_version,
                executable_sha256,
            },
        ) => {
            if provider.trim().is_empty()
                || profile_id.trim().is_empty()
                || profile_version.trim().is_empty()
            {
                return Err("game telemetry provenance identity is empty");
            }
            if !is_sha256_digest(executable_sha256) {
                return Err("game telemetry executable hash is invalid");
            }
        }
        _ => return Err("camera source and provenance disagree"),
    }
    if !row.camera_position.iter().all(|value| value.is_finite()) {
        return Err("camera_position contains a non-finite value");
    }
    if !row
        .camera_rotation_quaternion
        .iter()
        .all(|value| value.is_finite())
    {
        return Err("camera quaternion contains a non-finite value");
    }
    let norm_squared = row
        .camera_rotation_quaternion
        .iter()
        .map(|value| value * value)
        .sum::<f64>();
    if (norm_squared - 1.0).abs() > 0.002 {
        return Err("camera quaternion is not normalized");
    }
    let intrinsics = row.camera_intrinsics;
    if !intrinsics.fx.is_finite()
        || !intrinsics.fy.is_finite()
        || !intrinsics.cx.is_finite()
        || !intrinsics.cy.is_finite()
        || intrinsics.fx <= 0.0
        || intrinsics.fy <= 0.0
    {
        return Err("camera intrinsics are invalid");
    }
    if row
        .camera_speed
        .is_some_and(|speed| !speed.iter().all(|value| value.is_finite()))
    {
        return Err("camera_speed contains a non-finite value");
    }
    Ok(())
}

fn valid_performance_combined_projection(a: f64, b: f64) -> bool {
    // KCD2's observed 5 cm -> 2 cm transition remains finite-far reversed-Z.
    // d = a + b/z puts near at d=1 and far at d=0. Do not generalize this
    // measurement exception to traditional-Z, infinite far, or invalid planes.
    if !a.is_finite() || !b.is_finite() || a >= 0.0 || b <= 0.0 {
        return false;
    }
    let near = b / (1.0 - a);
    let far = -b / a;
    near.is_finite() && near > 0.0 && far.is_finite() && far > near
}

fn matrix_source_is_valid(source: MatrixSourceBinding) -> bool {
    source.buffer_bytes >= 64
        && source.offset.is_multiple_of(16)
        && source
            .offset
            .checked_add(64)
            .is_some_and(|end| end <= source.buffer_bytes)
}

fn same_physical_matrix_source(view: MatrixSourceBinding, projection: MatrixSourceBinding) -> bool {
    view.shader_stage == projection.shader_stage
        && view.bind_slot == projection.bind_slot
        && view.pass_ordinal == projection.pass_ordinal
        && view.draw_ordinal == projection.draw_ordinal
        && view.offset == projection.offset
}

fn arrays_approximately_equal<const N: usize>(left: &[f64; N], right: &[f64; N]) -> bool {
    left.iter()
        .zip(right)
        .all(|(left, right)| (left - right).abs() <= 1.0e-5)
}

fn intrinsics_approximately_equal(left: Intrinsics, right: Intrinsics) -> bool {
    [left.fx, left.fy, left.cx, left.cy]
        .into_iter()
        .zip([right.fx, right.fy, right.cx, right.cy])
        .all(|(left, right)| (left - right).abs() <= 1.0e-4)
}

fn validate_action_rows(
    rows: &[ActionFrameRow],
    identity: &StreamIdentity,
) -> Result<(), CameraActionJoinError> {
    validate_strictly_increasing(
        "actions.session_frame",
        rows.iter().map(|row| row.session_frame),
        true,
    )?;
    for (index, row) in rows.iter().enumerate() {
        if row.run_id != identity.run_id {
            return Err(CameraActionJoinError::RowRunIdMismatch {
                session_frame: row.session_frame,
                expected: identity.run_id.clone(),
                actual: row.run_id.clone(),
            });
        }
        if row.output_qpc_end <= row.output_qpc_start {
            return Err(CameraActionJoinError::InvalidQpcWindow {
                session_frame: row.session_frame,
                start: row.output_qpc_start,
                end: row.output_qpc_end,
            });
        }
        if let Some(previous) = index.checked_sub(1).and_then(|index| rows.get(index))
            && row.output_qpc_start < previous.output_qpc_end
        {
            return Err(CameraActionJoinError::OverlappingQpcWindows {
                previous_session_frame: previous.session_frame,
                previous_end: previous.output_qpc_end,
                session_frame: row.session_frame,
                start: row.output_qpc_start,
            });
        }
        row.raw_mouse_delta()?;
    }
    Ok(())
}

fn validate_frame_pairs(rows: &[FramePair]) -> Result<(), CameraActionJoinError> {
    validate_strictly_increasing(
        "frame_pairs.session_frame",
        rows.iter().map(|row| row.session_frame),
        true,
    )
}

fn validate_strictly_increasing(
    stream: &'static str,
    values: impl IntoIterator<Item = u64>,
    require_contiguous: bool,
) -> Result<(), CameraActionJoinError> {
    let mut previous = None;
    for current in values {
        if let Some(previous) = previous {
            if current == previous {
                return Err(CameraActionJoinError::DuplicateKey {
                    stream,
                    key: current,
                });
            }
            if current < previous {
                return Err(CameraActionJoinError::SequenceNotIncreasing {
                    stream,
                    previous,
                    current,
                });
            }
            if require_contiguous && current != previous + 1 {
                return Err(CameraActionJoinError::SequenceGap {
                    stream,
                    previous,
                    current,
                });
            }
        }
        previous = Some(current);
    }
    Ok(())
}

#[cfg(test)]
mod performance_camera_tests {
    use super::*;

    fn camera() -> CameraPoseRow {
        CameraPoseRow {
            schema: PERFORMANCE_CAMERA_POSE_INDEX_SCHEMA_V1.into(),
            identity: StreamIdentity {
                session_id: "session".into(),
                run_id: "r0003".into(),
                generation: 1,
            },
            present_seq: 107201,
            present_qpc: 2_000,
            source: CameraPoseSource::ResolvedCombinedViewProjection,
            provenance: CameraPoseProvenance::ResolvedCombinedViewProjection {
                binding_id: crate::KCD2_COMBINED_BINDING_ID_V2.into(),
                binding_version: crate::KCD2_COMBINED_BINDING_VERSION_V2.into(),
                profile_id: crate::KCD2_MAINLINE_GAME_ID.into(),
                profile_version: crate::DepthProfile::D3D12_STABLE_SCHEMA_VERSION.into(),
                executable_sha256: crate::KCD2_EXECUTABLE_SHA256.into(),
                dsv_id: crate::KCD2_MAIN_SCENE_DSV_ID.into(),
                render_width: 1920,
                render_height: 1080,
                view_metres_per_unit: "1.0".into(),
                axis_mapping: crate::D3d12AxisMapping {
                    delivery_x_from: crate::D3d12SignedAxis::PositiveX,
                    delivery_y_from: crate::D3d12SignedAxis::PositiveZ,
                    delivery_z_from: crate::D3d12SignedAxis::PositiveY,
                },
                source_qpc: 1_999,
                root_signature_sha256: format!(
                    "sha256:{}",
                    crate::KCD2_VIEW_PROJECTION_ROOT_SIGNATURE_SHA256
                ),
                root_parameter_index: 2,
                pass_ordinal: 0,
                draw_ordinal: 0,
                projection_a: -2.5000061997793486e-6,
                projection_b: 0.020000049844384193,
                projection_decomposition_residual: 1.0e-13,
                held_out_depth_row_residual: 1.0e-9,
            },
            camera_position: [1.0, 2.0, 3.0],
            camera_rotation_quaternion: [0.0, 0.0, 0.0, 1.0],
            camera_intrinsics: Intrinsics {
                fx: 960.0,
                fy: 960.0,
                cx: 960.0,
                cy: 540.0,
            },
            camera_speed: None,
        }
    }

    fn set_projection(row: &mut CameraPoseRow, a: f64, b: f64) {
        let CameraPoseProvenance::ResolvedCombinedViewProjection {
            projection_a,
            projection_b,
            ..
        } = &mut row.provenance
        else {
            unreachable!()
        };
        *projection_a = a;
        *projection_b = b;
    }

    #[test]
    fn performance_camera_accepts_observed_two_centimetre_projection_only_in_its_schema() {
        let mut row = camera();
        assert!(validate_performance_camera_pose_row(&row).is_ok());
        assert!(validate_camera_pose_row(&row).is_err());
        row.schema = CAMERA_POSE_INDEX_SCHEMA_V1.into();
        assert!(validate_performance_camera_pose_row(&row).is_err());
        assert!(validate_camera_pose_row(&row).is_err());
        set_projection(&mut row, crate::KCD2_PROJECTION_A, crate::KCD2_PROJECTION_B);
        assert!(validate_camera_pose_row(&row).is_ok());
        row.schema = PERFORMANCE_CAMERA_POSE_INDEX_SCHEMA_V1.into();
        assert!(validate_performance_camera_pose_row(&row).is_ok());
        assert!(validate_camera_pose_row(&row).is_err());
    }

    #[test]
    fn performance_camera_rejects_nonfinite_degenerate_and_unreviewed_projection_conventions() {
        for (a, b) in [
            (f64::NAN, 0.02),
            (-0.0000025, f64::NAN),
            (f64::NEG_INFINITY, 0.02),
            (-0.0000025, f64::INFINITY),
            (-0.0000025, 0.0),
            (-0.0000025, -0.02),
            (0.0, 0.02),
            (1.01, -0.02),
            (-f64::MIN_POSITIVE, f64::MAX),
            (-f64::MAX, f64::MIN_POSITIVE),
        ] {
            let mut row = camera();
            set_projection(&mut row, a, b);
            assert!(
                validate_performance_camera_pose_row(&row).is_err(),
                "{a}/{b}"
            );
            row.schema = CAMERA_POSE_INDEX_SCHEMA_V1.into();
            assert!(validate_camera_pose_row(&row).is_err(), "{a}/{b}");
        }
    }

    #[test]
    fn performance_camera_preserves_all_combined_metadata_and_pose_gates() {
        let mut wrong_source = camera();
        wrong_source.source = CameraPoseSource::ResolvedMainCameraHook;
        assert!(validate_performance_camera_pose_row(&wrong_source).is_err());
        for defect in 0..19 {
            let mut row = camera();
            let CameraPoseProvenance::ResolvedCombinedViewProjection {
                binding_id,
                binding_version,
                profile_id,
                profile_version,
                executable_sha256,
                dsv_id,
                render_width,
                render_height,
                view_metres_per_unit,
                axis_mapping,
                source_qpc,
                root_signature_sha256,
                root_parameter_index,
                draw_ordinal,
                projection_decomposition_residual,
                held_out_depth_row_residual,
                ..
            } = &mut row.provenance
            else {
                unreachable!()
            };
            match defect {
                0 => *binding_id = "wrong".into(),
                1 => *binding_version = "wrong".into(),
                2 => *profile_id = "wrong".into(),
                3 => *profile_version = "wrong".into(),
                4 => *executable_sha256 = "wrong".into(),
                5 => *dsv_id = "wrong".into(),
                6 => *render_width = 1280,
                7 => *render_height = 720,
                8 => *view_metres_per_unit = "NaN".into(),
                9 => axis_mapping.delivery_y_from = crate::D3d12SignedAxis::PositiveY,
                10 => *source_qpc = 0,
                11 => *root_signature_sha256 = "wrong".into(),
                12 => *root_parameter_index = 1,
                13 => *draw_ordinal = 1,
                14 => *projection_decomposition_residual = f64::NAN,
                15 => *held_out_depth_row_residual = 0.01,
                16 => row.camera_position[0] = f64::INFINITY,
                17 => row.camera_rotation_quaternion = [0.0; 4],
                18 => row.camera_intrinsics.fx = 0.0,
                _ => unreachable!(),
            }
            assert!(
                validate_performance_camera_pose_row(&row).is_err(),
                "defect {defect}"
            );
        }
    }
}

//! ETS2/ATS `camera_*` 事实：车内相机位姿的三级复合与内参声明。
//!
//! [98 号 §5.7.1](../../../docs/98-steam-wide-map-metric-actioncamera-source-survey.md)
//! 把 ETS2 的 `camera_*` 列为「D3D11 hook 取 view/projection 矩阵」，三级复合
//! `world_placement ∘ cabin ∘ head` 只作交叉校验。本模块反过来把三级复合做成**主路**，
//! 理由是 [95 号](../../../docs/95-camera-projection-binding-fieldguide.md) 的结论：
//! 投影矩阵在无变焦时恒定，认矩阵这件事在 Fallout 4 上卡了整整一轮，
//! 而 ETS2 的三个复合项**全部来自官方 MIT SDK 通道**，一个 hook 都不需要。
//!
//! 代价写在明处：内参不再是「从这一帧的渲染状态观测到的」，而是
//! 「采集前把 FOV 钉死并记录」——即 95 号 §3.3 的路线 D。ETS2 恰好满足该路线的
//! 全部前提（FOV 可用 `r_multimon_mode 4` + `horizontal_fov_override` 显式设死，
//! 或从解包的驾驶舱相机定义读出），所以它在这里不是退路而是可控输入。
//!
//! # 三条纪律
//!
//! 1. **偏移的复合次序没有官方裁定，本模块不猜。** 见 [`OffsetComposition`]，
//!    与 [`crate::RollConvention`] 同形：没有 `Default`，标定前的非零偏移一律拒绝。
//! 2. **`camera_*` 的置信度不高于 `player_*`。** 复合的第一项就是 `player_*` 的
//!    那个 `world.placement`，所以 roll 符号未标定时相机四元数同样未标定。
//! 3. **内参必须带来源。** [`FovSource`] 强制记账，且交付层据此判断能不能出门。

use serde::{Deserialize, Serialize};
use telemetry_geometry::{transform_point, transform_rotation};

use crate::contract::{ContractShapeErrorV1, ConversionErrorV1, CoordinateTransformV1};
use crate::orientation::{EulerTurns, RollConvention};

/// 本模块的消息版本。
pub const CAMERA_SCHEMA_VERSION_V1: &str = "ets2-camera-telemetry/v1";

/// 偏移小到可以当成零的位置阈值（米）。
///
/// 5 mm：比 1 cm 的交付深度精度还小一个量级，所以在这个量级下
/// 「复合次序选错」不可能改变任何一个会被交付出去的数值。
pub const NEGLIGIBLE_OFFSET_M: f64 = 0.005;

/// 偏移小到可以当成零的角度阈值（圈）。5e-4 圈 = 0.18°。
pub const NEGLIGIBLE_OFFSET_TURNS: f64 = 5e-4;

/// `truck.cabin.offset` / `truck.head.offset` 的复合次序。**没有默认值。**
///
/// # 为什么这一条必须由调用方决定
///
/// 官方头文件对两个通道的措辞是「a **vehicle space** position and orientation
/// **delta** of the cabin from its default position」与「a **cabin space** …
/// delta of the driver head …」。「delta from its default position, expressed in
/// 父空间」有两种都读得通的施加方式，且差别只在旋转非零时显形：
///
/// - [`Self::DeltaAppliedInChildFrame`]：`parent_from_child = default · Δ`
///   ——Δ 是在**默认位姿的局部系**里叠加（右乘）；
/// - [`Self::DeltaAppliedInParentFrame`]：`parent_from_child = Δ · default`
///   ——Δ 是在**父空间**里叠加（左乘）。
///
/// 措辞里的 "vehicle space" 更像在说后者，但它也可能只是在说 Δ 的**位置分量**
/// 用的是车身系的轴——那样旋转仍可能是右乘。两种读法在平移上一致、在旋转上不一致，
/// 而车内相机的默认头位相对车身原点有 1～2 米的偏移，**一个 5° 的读法差
/// 会让相机位置错出十几厘米**，正好落在 SFM 会当成真实位移的量级。
///
/// 官方文档没有第三句话可以裁定，所以本模块不猜：标定前用 [`Self::Unvalidated`]，
/// 此时**任何超过 [`NEGLIGIBLE_OFFSET_M`] / [`NEGLIGIBLE_OFFSET_TURNS`] 的偏移
/// 都会被拒绝**，而不是按某一种读法算出来。实机标定方法见 crate README。
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OffsetComposition {
    /// `parent_from_child = default · Δ`（右乘，Δ 在子/默认位姿的局部系里）。
    DeltaAppliedInChildFrame,
    /// `parent_from_child = Δ · default`（左乘，Δ 在父空间里）。
    DeltaAppliedInParentFrame,
    /// 尚未实机标定。可忽略量级的偏移按零处理并放行，其余一律拒绝。
    Unvalidated,
}

impl OffsetComposition {
    /// 是否已经过实机标定。`false` 时交付层应把它标进 `confidence`。
    #[must_use]
    pub const fn is_validated(self) -> bool {
        !matches!(self, Self::Unvalidated)
    }
}

/// 内参里那个 FOV 是怎么来的。**不允许「就用个常见值」。**
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FovSource {
    /// 从游戏本体解包出的驾驶舱相机定义读出（`scs_extractor` → `unit` 定义）。
    /// 它绑定到具体车型，换车必须重新取值。
    UnpackedGameDefinition,
    /// 采集前用控制台 `r_multimon_mode 4` + `horizontal_fov_override` 显式设死。
    /// 这是**确定角度值**，也是 95 号 §2.3 路 2 交叉验证的输入。
    ConsoleOverride,
}

/// 车内相机的标定基线：默认头位 + 钉死的 FOV。
///
/// 「默认位姿」不在 SDK 通道里——`cabin.offset` / `head.offset` 都只给相对默认位姿的
/// 增量。所以基线必须外接，且必须能指回证据（解包定义或控制台设定的记录）。
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InteriorCameraBaselineV1 {
    pub schema_version: String,
    /// 基线标识，例如 `scania-s-2016-interior-v1`。进交付元数据。
    pub baseline_id: String,
    /// 本基线适用的车型。**换车即作废**：驾驶舱几何与 FOV 都是逐车型的。
    pub truck_id: String,
    /// 取值证据的路径（解包产物或控制台设定记录）。空串不接受。
    pub evidence_path: String,
    /// 默认驾驶员头位在**车身系**（SCS local：X 右 / Y 上 / Z 后）下的位置，米。
    pub default_head_position_m: [f64; 3],
    /// 默认头部朝向相对车身的欧拉角（圈）。正常车型应接近全零。
    pub default_head_orientation: EulerTurns,
    /// 钉死的**水平** FOV，度。
    pub horizontal_fov_degrees: f64,
    pub fov_source: FovSource,
}

impl InteriorCameraBaselineV1 {
    pub fn validate(&self) -> Result<(), ContractShapeErrorV1> {
        if self.schema_version != CAMERA_SCHEMA_VERSION_V1 {
            return Err(ContractShapeErrorV1::InvalidField("schema_version"));
        }
        if self.baseline_id.trim().is_empty() || self.truck_id.trim().is_empty() {
            return Err(ContractShapeErrorV1::InvalidField("baseline identity"));
        }
        // 证据路径为空 = 这个基线说不清是哪来的，与 `metric_scale` 强制记账同一条纪律。
        if self.evidence_path.trim().is_empty() {
            return Err(ContractShapeErrorV1::InvalidField("evidence_path"));
        }
        if !self
            .default_head_position_m
            .iter()
            .all(|value| value.is_finite())
        {
            return Err(ContractShapeErrorV1::InvalidField(
                "default_head_position_m",
            ));
        }
        if self.default_head_orientation.validate().is_err() {
            return Err(ContractShapeErrorV1::InvalidField(
                "default_head_orientation",
            ));
        }
        // 上下界不是美观检查：<1° 会让 fx 溢出到不合理的量级，≥180° 根本不是透视投影。
        if !self.horizontal_fov_degrees.is_finite()
            || !(1.0..180.0).contains(&self.horizontal_fov_degrees)
        {
            return Err(ContractShapeErrorV1::InvalidField("horizontal_fov_degrees"));
        }
        Ok(())
    }

    /// 按交付分辨率算内参（方形像素）。
    ///
    /// 95 号 §2.3 路 2：`fx = (W/2)/tan(hFOV/2)`，方形像素下 `fy = fx`，
    /// 主点取几何中心。**分辨率必须是 `action_camera.json` 所描述的那一个**
    /// ——68 号 §6.3 的 `cx` 就写着「宽度一半，像素」，而 video 是 1080p、
    /// depth 是 720p，套错一个会让内参整体差 1.5 倍。
    pub fn intrinsics(
        &self,
        width: u32,
        height: u32,
    ) -> Result<CameraIntrinsicsV1, ContractShapeErrorV1> {
        self.validate()?;
        if width == 0 || height == 0 {
            return Err(ContractShapeErrorV1::InvalidField("render size"));
        }
        let half_fov = self.horizontal_fov_degrees.to_radians() / 2.0;
        let fx = (f64::from(width) / 2.0) / half_fov.tan();
        if !fx.is_finite() || fx <= 0.0 {
            return Err(ContractShapeErrorV1::InvalidField("horizontal_fov_degrees"));
        }
        Ok(CameraIntrinsicsV1 {
            fx,
            fy: fx,
            cx: f64::from(width) / 2.0,
            cy: f64::from(height) / 2.0,
        })
    }
}

/// 68 号 §6.3 的 `camera_intrinsics`。
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CameraIntrinsicsV1 {
    pub fx: f64,
    pub fy: f64,
    pub cx: f64,
    pub cy: f64,
}

/// SDK 侧一帧的相机相关原始读数。
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RawCameraOffsetsV1 {
    /// `truck.cabin.offset`（fplacement，车身系增量）。未上报时为 `None`。
    pub cabin_offset_m: Option<[f64; 3]>,
    pub cabin_offset_orientation: Option<EulerTurns>,
    /// `truck.head.offset`（fplacement，驾驶舱系增量）。未上报时为 `None`。
    pub head_offset_m: Option<[f64; 3]>,
    pub head_offset_orientation: Option<EulerTurns>,
}

impl RawCameraOffsetsV1 {
    /// 两级通道都「从未上报过」。**与 [`Self::zero`] 是两件事**:
    /// 前者是「不知道」,后者是「确实为零」。序列化时前者写 `null`。
    #[must_use]
    pub const fn unreported() -> Self {
        Self {
            cabin_offset_m: None,
            cabin_offset_orientation: None,
            head_offset_m: None,
            head_offset_orientation: None,
        }
    }

    /// 全零偏移。`g_cam_physics/steering/blinker` 置 0 且不做头部环视时的期望值。
    #[must_use]
    pub const fn zero() -> Self {
        const ZERO_EULER: EulerTurns = EulerTurns {
            heading: 0.0,
            pitch: 0.0,
            roll: 0.0,
        };
        Self {
            cabin_offset_m: Some([0.0; 3]),
            cabin_offset_orientation: Some(ZERO_EULER),
            head_offset_m: Some([0.0; 3]),
            head_offset_orientation: Some(ZERO_EULER),
        }
    }

    /// 两级偏移是否都小到可以当零。未上报的通道按零计。
    #[must_use]
    pub fn is_negligible(&self) -> bool {
        let position_ok = |value: Option<[f64; 3]>| {
            value.is_none_or(|v| v.iter().all(|x| x.abs() <= NEGLIGIBLE_OFFSET_M))
        };
        let angle_ok = |value: Option<EulerTurns>| {
            value.is_none_or(|e| {
                // heading 存在 `<0,1)` 的单位域里，所以「接近零」在两端都成立：
                // 0.9999 圈是 −0.0001 圈，不是 0.9999 圈。
                let heading = e.heading.min(1.0 - e.heading);
                heading.abs() <= NEGLIGIBLE_OFFSET_TURNS
                    && e.pitch.abs() <= NEGLIGIBLE_OFFSET_TURNS
                    && e.roll.abs() <= NEGLIGIBLE_OFFSET_TURNS
            })
        };
        position_ok(self.cabin_offset_m)
            && position_ok(self.head_offset_m)
            && angle_ok(self.cabin_offset_orientation)
            && angle_ok(self.head_offset_orientation)
    }
}

/// 换算后的交付侧 `camera_*`。
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CameraSampleV1 {
    pub schema_version: String,
    /// 68 号 §6.3 的 `camera_position`，交付世界系，单位米。
    pub position_m: [f64; 3],
    /// 68 号 §6.3 的 `camera_rotation_quaternion`，xyzw 序，已归一化且 `w ≥ 0`。
    pub rotation_quaternion_xyzw: [f64; 4],
    /// 68 号 §6.3 的 `camera_follow_offset`：相机相对被跟随目标（此处为卡车原点）
    /// 的偏移，**交付世界系**、米。
    pub follow_offset_m: [f64; 3],
    pub baseline_id: String,
    pub roll_convention: RollConvention,
    pub offset_composition: OffsetComposition,
}

/// 相机复合失败的原因。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CameraConversionErrorV1 {
    /// 基线本身不合法。
    Baseline(ContractShapeErrorV1),
    /// 卡车侧读数换算失败。
    Truck(ConversionErrorV1),
    /// 偏移欧拉角越界或非有限。
    Offset,
    /// 偏移非零，而复合次序尚未标定。**这不是数据坏了，是我们还不知道怎么算。**
    UncalibratedNonZeroOffset,
    /// 复合结果不再是纯旋转（理论上不该发生）。
    RotationNotProper,
    /// 位置非有限。
    NonFinitePosition,
}

/// 卡车世界位姿 + 两级偏移 + 基线 → 交付侧相机位姿。
///
/// 复合次序（SCS 局部系内）：
/// `world_from_camera = world_from_truck · truck_from_cabin · cabin_from_head`，
/// 其中 `cabin_from_head` 的默认项就是基线里的头位。两级增量按
/// [`OffsetComposition`] 施加，未标定时要求增量可忽略。
///
/// 位置随后与 `player_*` 走**同一个**轴变换（SCS 世界 → 68 号交付世界），
/// 所以 `camera_position` 与 `player_position` 天然同系——`camera_follow_offset`
/// 才是逐分量可减的。
pub fn compose_interior_camera(
    truck_world_position_m: [f64; 3],
    truck_world_orientation: EulerTurns,
    offsets: &RawCameraOffsetsV1,
    baseline: &InteriorCameraBaselineV1,
    roll_convention: RollConvention,
    offset_composition: OffsetComposition,
) -> Result<CameraSampleV1, CameraConversionErrorV1> {
    baseline
        .validate()
        .map_err(CameraConversionErrorV1::Baseline)?;
    if !offset_composition.is_validated() && !offsets.is_negligible() {
        return Err(CameraConversionErrorV1::UncalibratedNonZeroOffset);
    }

    let truck_rotation = truck_world_orientation
        .to_world_rotation(roll_convention)
        .map_err(|error| CameraConversionErrorV1::Truck(ConversionErrorV1::Euler(error)))?;

    // 车身系内：默认头位，再叠两级增量。cabin 的父空间是车身，head 的父空间是驾驶舱；
    // 未标定次序时两级增量都已被上面的闸判为可忽略，此处按零参与运算。
    let head_default_rotation = baseline
        .default_head_orientation
        .to_world_rotation(roll_convention)
        .map_err(|_| CameraConversionErrorV1::Offset)?;

    let cabin = placement_from_offset(
        offsets.cabin_offset_m,
        offsets.cabin_offset_orientation,
        roll_convention,
    )?;
    let head = placement_from_offset(
        offsets.head_offset_m,
        offsets.head_offset_orientation,
        roll_convention,
    )?;

    let head_default = Placement {
        position: baseline.default_head_position_m,
        rotation: head_default_rotation,
    };
    // truck_from_cabin 的「默认」是恒等（cabin.offset 本身就是相对默认位姿的增量，
    // 而默认驾驶舱位姿已经吸收进基线的头位里）。
    let truck_from_cabin = compose_delta(Placement::identity(), cabin, offset_composition);
    let cabin_from_head = compose_delta(head_default, head, offset_composition);
    let truck_from_camera = truck_from_cabin.then(&cabin_from_head);

    let world_from_truck = Placement {
        position: truck_world_position_m,
        rotation: truck_rotation,
    };
    let world_from_camera = world_from_truck.then(&truck_from_camera);

    let transform = CoordinateTransformV1::scs_to_dm_world();
    transform
        .validate_shape()
        .map_err(|error| CameraConversionErrorV1::Truck(ConversionErrorV1::Contract(error)))?;
    let axes = transform.target_from_source_axes;

    let position_m = transform_point(
        axes,
        transform.metres_per_game_unit,
        world_from_camera.position,
    )
    .ok_or(CameraConversionErrorV1::NonFinitePosition)?;
    // 交付契约存的是 **world→cam**，不是 cam→world。
    //
    // 104 号 §1.1 的 C1 把这条钉死并给了实测：反投影时不转置，跨帧深度一致性
    // median 误差 13.9%；转置后 0.71–0.92%。Fallout 那条链天然满足它——位姿是从
    // **view 矩阵**还原的，而 view 矩阵本身就是 world→cam。
    //
    // 本模块是从 `world.placement` 正向复合出 cam→world 的，所以必须显式取逆，
    // 否则整批四元数反向。**这一条在 0807 首次真机数据上抓到过**：不取逆时
    // 互投影 median 误差 66.8%，取逆后 40.1%。
    //
    // 剩余的 40.1% 与本函数无关，成因在别处：depth 的 `proj_source="engine_default"`
    // 参数与实际缓冲不符、相机基线（头位杠杆臂 / FOV）未标定、本批未按 #591 §3.3
    // 关闭驾驶舱物理。其中「相机基线取值」「roll 符号标定」「偏移复合次序标定」
    // 三项已列在 [103 号 §7](../../../docs/103-ets2-pr405-delivery-runbook.md) 的未完成清单里；
    // 完整拆解在随 0807 数据上传的 FINDINGS.md（不在本仓库内）。
    let world_from_camera_quat = transform_rotation(axes, world_from_camera.rotation)
        .ok_or(CameraConversionErrorV1::RotationNotProper)?;
    let rotation_quaternion_xyzw = invert_unit_quaternion(world_from_camera_quat);
    let truck_position_m =
        transform_point(axes, transform.metres_per_game_unit, truck_world_position_m)
            .ok_or(CameraConversionErrorV1::NonFinitePosition)?;

    Ok(CameraSampleV1 {
        schema_version: CAMERA_SCHEMA_VERSION_V1.to_owned(),
        position_m,
        rotation_quaternion_xyzw,
        follow_offset_m: std::array::from_fn(|axis| position_m[axis] - truck_position_m[axis]),
        baseline_id: baseline.baseline_id.clone(),
        roll_convention,
        offset_composition,
    })
}

/// 一个刚体位姿（SCS 局部/世界系，行主序旋转）。
#[derive(Clone, Copy, Debug)]
struct Placement {
    position: [f64; 3],
    rotation: [[f64; 3]; 3],
}

impl Placement {
    const fn identity() -> Self {
        Self {
            position: [0.0; 3],
            rotation: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
        }
    }

    /// `self ∘ child`：child 表达在 self 的局部系里。
    fn then(&self, child: &Self) -> Self {
        Self {
            position: std::array::from_fn(|i| {
                self.position[i]
                    + (0..3)
                        .map(|k| self.rotation[i][k] * child.position[k])
                        .sum::<f64>()
            }),
            rotation: mul3(self.rotation, child.rotation),
        }
    }
}

fn placement_from_offset(
    position: Option<[f64; 3]>,
    orientation: Option<EulerTurns>,
    roll_convention: RollConvention,
) -> Result<Placement, CameraConversionErrorV1> {
    let position = position.unwrap_or([0.0; 3]);
    if !position.iter().all(|value| value.is_finite()) {
        return Err(CameraConversionErrorV1::Offset);
    }
    let rotation = match orientation {
        Some(euler) => euler
            .to_world_rotation(roll_convention)
            .map_err(|_| CameraConversionErrorV1::Offset)?,
        None => Placement::identity().rotation,
    };
    Ok(Placement { position, rotation })
}

fn compose_delta(
    default: Placement,
    delta: Placement,
    composition: OffsetComposition,
) -> Placement {
    match composition {
        // 未标定时增量已被判为可忽略，走哪一支都一样；选右乘只是为了有个确定行为。
        OffsetComposition::DeltaAppliedInChildFrame | OffsetComposition::Unvalidated => {
            default.then(&delta)
        }
        OffsetComposition::DeltaAppliedInParentFrame => delta.then(&default),
    }
}

/// 单位四元数取逆 = 取共轭（虚部取负）。`w` 的符号不变，故 `w ≥ 0` 的约定仍然成立。
fn invert_unit_quaternion(q: [f64; 4]) -> [f64; 4] {
    [-q[0], -q[1], -q[2], q[3]]
}

fn mul3(left: [[f64; 3]; 3], right: [[f64; 3]; 3]) -> [[f64; 3]; 3] {
    let mut out = [[0.0; 3]; 3];
    for (i, row) in out.iter_mut().enumerate() {
        for (j, cell) in row.iter_mut().enumerate() {
            *cell = (0..3).map(|k| left[i][k] * right[k][j]).sum();
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn baseline() -> InteriorCameraBaselineV1 {
        InteriorCameraBaselineV1 {
            schema_version: CAMERA_SCHEMA_VERSION_V1.to_owned(),
            baseline_id: "test-interior-v1".to_owned(),
            truck_id: "vehicle.scania.s_2016".to_owned(),
            evidence_path: "tools/pr405-ets2/evidence/example.json".to_owned(),
            // 车身系：X 右 / Y 上 / Z 后 ⇒ 驾驶位在原点左前上方。
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

    fn close3(a: [f64; 3], b: [f64; 3]) -> bool {
        a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-9)
    }

    /// 采集端 0806 实测的那个数：71° FOV、1080p ⇒ fx = 960/tan(35.5°) ≈ 1345.87。
    /// 这条把「内参怎么算」钉在一个已经在真机数据上出现过的值上。
    #[test]
    fn intrinsics_match_the_field_measured_71_degree_value() {
        let k = baseline().intrinsics(1920, 1080).expect("合法基线");
        assert!((k.fx - 1345.87).abs() < 0.01, "实得 fx={}", k.fx);
        assert_eq!(k.fx, k.fy, "方形像素");
        assert_eq!((k.cx, k.cy), (960.0, 540.0));
    }

    /// **内参必须跟着交付分辨率走。** video 是 1080p、depth 是 720p，
    /// 套错一个整批内参差 1.5 倍——而画面看起来完全正常。
    #[test]
    fn intrinsics_scale_with_the_delivery_resolution() {
        let b = baseline();
        let hd = b.intrinsics(1920, 1080).expect("合法");
        let depth = b.intrinsics(1280, 720).expect("合法");
        assert!((hd.fx / depth.fx - 1.5).abs() < 1e-9);
        assert_eq!((depth.cx, depth.cy), (640.0, 360.0));
    }

    /// 没有证据路径的基线不许用——与 `metric_scale` 强制记账同一条纪律。
    #[test]
    fn a_baseline_without_evidence_is_rejected() {
        let mut b = baseline();
        b.evidence_path = "  ".to_owned();
        assert_eq!(
            b.validate(),
            Err(ContractShapeErrorV1::InvalidField("evidence_path"))
        );
    }

    /// 零偏移、车头朝北：相机应当落在「卡车位置 + 头位（经轴变换）」上。
    ///
    /// 车身系 Z 指后、交付系 Z 指前，所以车身系的 −1.20（前方 1.2 m）
    /// 在交付系里应当变成 +1.20。
    #[test]
    fn a_zero_offset_camera_sits_at_the_baseline_head_position() {
        let sample = compose_interior_camera(
            [100.0, 5.0, 200.0],
            EulerTurns {
                heading: 0.0,
                pitch: 0.0,
                roll: 0.0,
            },
            &RawCameraOffsetsV1::zero(),
            &baseline(),
            RollConvention::Unvalidated,
            OffsetComposition::Unvalidated,
        )
        .expect("零偏移应可复合");
        // player_position 侧：Z 翻号 ⇒ [100, 5, −200]。
        assert!(
            close3(sample.position_m, [100.0 - 0.45, 5.0 + 1.85, -200.0 + 1.20]),
            "实得 {:?}",
            sample.position_m
        );
        assert!(
            close3(sample.follow_offset_m, [-0.45, 1.85, 1.20]),
            "follow offset 应是同系下的逐分量差，实得 {:?}",
            sample.follow_offset_m
        );
    }

    /// **头位要跟着车身转。** 车头朝东（heading=0.75）时，车身系的「前方 1.2 m」
    /// 应当出现在世界的东侧，而不是继续留在北侧。
    ///
    /// 这条抓的是「忘了把头位过一遍车身旋转」——那种错误在直行朝北的第一段素材里
    /// 完全看不出来，一转弯整条轨迹就开始横向漂移。
    #[test]
    fn the_head_offset_rotates_with_the_truck() {
        let sample = compose_interior_camera(
            [0.0, 0.0, 0.0],
            EulerTurns {
                heading: 0.75, // 东
                pitch: 0.0,
                roll: 0.0,
            },
            &RawCameraOffsetsV1::zero(),
            &baseline(),
            RollConvention::Unvalidated,
            OffsetComposition::Unvalidated,
        )
        .expect("零偏移应可复合");
        // 车头朝东：车身前向 = 世界 +X。头位前方 1.20 ⇒ x=+1.20；
        // 车身右向（原 +X）朝南 = 交付系 −Z（南在 SCS 是 +Z，交付系翻号后是 −Z）。
        assert!(
            (sample.position_m[0] - 1.20).abs() < 1e-9,
            "前向分量应落到 +X，实得 {:?}",
            sample.position_m
        );
        assert!(
            (sample.position_m[1] - 1.85).abs() < 1e-9,
            "高度不受偏航影响"
        );
        assert!(
            (sample.position_m[2] - 0.45).abs() < 1e-9,
            "车身右向的 −0.45 应落到交付系 +Z，实得 {:?}",
            sample.position_m
        );
    }

    /// **未标定复合次序 + 非零偏移 ⇒ 拒绝，而不是按某一种读法算。**
    ///
    /// 这是本模块最重要的一条：两种读法都能算出「看起来合理」的相机位置，
    /// 而错了十几厘米的相机位置在 SFM 里会被当成真实位移。
    #[test]
    fn an_uncalibrated_non_zero_offset_is_refused_rather_than_guessed() {
        let mut offsets = RawCameraOffsetsV1::zero();
        offsets.head_offset_m = Some([0.10, 0.0, 0.0]);
        assert_eq!(
            compose_interior_camera(
                [0.0; 3],
                EulerTurns {
                    heading: 0.0,
                    pitch: 0.0,
                    roll: 0.0
                },
                &offsets,
                &baseline(),
                RollConvention::Unvalidated,
                OffsetComposition::Unvalidated,
            ),
            Err(CameraConversionErrorV1::UncalibratedNonZeroOffset)
        );
    }

    /// 标定之后同一份读数就能算出来了，且两种次序确实给出不同的位置
    /// ——否则这条标定就是多余的，那说明测试没有真的覆盖差异。
    #[test]
    fn the_two_compositions_disagree_once_the_offset_rotates() {
        // 差异只可能出现在**默认位姿非恒等**的那一级上：驾驶舱的默认位姿是恒等
        // （已吸收进基线头位），所以把转角放在 head 这一级才试得出两种读法。
        let offsets = RawCameraOffsetsV1 {
            head_offset_orientation: Some(EulerTurns {
                heading: 0.05, // 18°
                pitch: 0.0,
                roll: 0.0,
            }),
            ..RawCameraOffsetsV1::zero()
        };
        let run = |composition| {
            compose_interior_camera(
                [0.0; 3],
                EulerTurns {
                    heading: 0.0,
                    pitch: 0.0,
                    roll: 0.0,
                },
                &offsets,
                &baseline(),
                RollConvention::Unvalidated,
                composition,
            )
            .expect("已标定应可复合")
            .position_m
        };
        let child = run(OffsetComposition::DeltaAppliedInChildFrame);
        let parent = run(OffsetComposition::DeltaAppliedInParentFrame);
        let gap = (0..3)
            .map(|i| (child[i] - parent[i]).abs())
            .fold(0.0_f64, f64::max);
        assert!(
            gap > 0.05,
            "两种次序应当给出可区分的结果，实得差 {gap} m（{child:?} vs {parent:?}）"
        );
    }

    /// heading 存在 `<0,1)` 单位域里，所以「差一点点向右」是 0.9999 而不是 −0.0001。
    /// 可忽略判定必须认得出这个形状，否则每一帧都会被误判成大偏移而整段拒绝。
    #[test]
    fn a_negligible_heading_near_the_wrap_point_is_still_negligible() {
        let offsets = RawCameraOffsetsV1 {
            cabin_offset_orientation: Some(EulerTurns {
                heading: 0.9999,
                pitch: 0.0,
                roll: 0.0,
            }),
            ..RawCameraOffsetsV1::zero()
        };
        assert!(offsets.is_negligible());
    }

    /// 越界欧拉角（把度当成圈）在相机侧同样必须被拒。
    #[test]
    fn out_of_range_offset_angles_are_rejected() {
        let offsets = RawCameraOffsetsV1 {
            head_offset_orientation: Some(EulerTurns {
                heading: 30.0,
                pitch: 0.0,
                roll: 0.0,
            }),
            ..RawCameraOffsetsV1::zero()
        };
        assert_eq!(
            compose_interior_camera(
                [0.0; 3],
                EulerTurns {
                    heading: 0.0,
                    pitch: 0.0,
                    roll: 0.0
                },
                &offsets,
                &baseline(),
                RollConvention::Unvalidated,
                OffsetComposition::DeltaAppliedInChildFrame,
            ),
            Err(CameraConversionErrorV1::Offset)
        );
    }

    /// **交付契约存的是 world→cam，不是 cam→world。**
    ///
    /// 这是 0807 首次真机数据上抓到的那个 bug：不取逆时互投影 median 误差 66.8%，
    /// 取逆后 40.1%（104 号 §1.1 C1 在 FO4 上的同类实测是 13.9% vs 0.71%）。
    /// 原来的测试只验了模长与 `w ≥ 0`，那两条对方向反了完全无感——所以这里
    /// 直接把「把世界方向转进相机系」这件事本身钉死。
    #[test]
    fn the_quaternion_maps_world_into_camera_not_the_other_way() {
        // 车头朝东：交付系里东 = +X，而相机系的前向是 +Z。
        let sample = compose_interior_camera(
            [0.0; 3],
            EulerTurns {
                heading: 0.75,
                pitch: 0.0,
                roll: 0.0,
            },
            &RawCameraOffsetsV1::zero(),
            &baseline(),
            RollConvention::Unvalidated,
            OffsetComposition::DeltaAppliedInChildFrame,
        )
        .expect("合法");
        let [x, y, z, w] = sample.rotation_quaternion_xyzw;
        // 四元数 → 旋转矩阵（行主序，作用于列向量）。
        let r = [
            [
                1.0 - 2.0 * (y * y + z * z),
                2.0 * (x * y - z * w),
                2.0 * (x * z + y * w),
            ],
            [
                2.0 * (x * y + z * w),
                1.0 - 2.0 * (x * x + z * z),
                2.0 * (y * z - x * w),
            ],
            [
                2.0 * (x * z - y * w),
                2.0 * (y * z + x * w),
                1.0 - 2.0 * (x * x + y * y),
            ],
        ];
        let world_east = [1.0, 0.0, 0.0];
        let in_cam: [f64; 3] =
            std::array::from_fn(|i| (0..3).map(|k| r[i][k] * world_east[k]).sum());
        assert!(
            (in_cam[2] - 1.0).abs() < 1e-9,
            "世界东向经 R(q) 应落到相机前向 +Z（world→cam），实得 {in_cam:?}"
        );
    }

    /// 相机四元数应当是单位四元数且 `w ≥ 0`——与 `player_*` 同一约定。
    #[test]
    fn the_camera_quaternion_is_normalised_with_non_negative_w() {
        let sample = compose_interior_camera(
            [0.0; 3],
            EulerTurns {
                heading: 0.3,
                pitch: 0.1,
                roll: 0.05,
            },
            &RawCameraOffsetsV1::zero(),
            &baseline(),
            RollConvention::RollAxisIsForward,
            OffsetComposition::DeltaAppliedInChildFrame,
        )
        .expect("合法");
        let norm = sample
            .rotation_quaternion_xyzw
            .iter()
            .map(|v| v * v)
            .sum::<f64>()
            .sqrt();
        assert!((norm - 1.0).abs() < 1e-12, "实得模长 {norm}");
        assert!(sample.rotation_quaternion_xyzw[3] >= 0.0);
    }

    /// 契约可 JSON 往返。
    ///
    /// 浮点用容差比而不是逐位比：`serde_json` 默认的解析快路可以差 1 ULP
    /// （精确往返要开 `float_roundtrip` 特性），那是它的取舍，不是本模块的事实。
    /// 这条要证的是**字段没丢、枚举名没漂**。
    #[test]
    fn samples_round_trip_through_json() {
        let sample = compose_interior_camera(
            [1.0, 2.0, 3.0],
            EulerTurns {
                heading: 0.1,
                pitch: 0.0,
                roll: 0.0,
            },
            &RawCameraOffsetsV1::zero(),
            &baseline(),
            RollConvention::Unvalidated,
            OffsetComposition::Unvalidated,
        )
        .expect("合法");
        let encoded = serde_json::to_string(&sample).expect("可序列化");
        let decoded: CameraSampleV1 = serde_json::from_str(&encoded).expect("可反序列化");
        assert_eq!(decoded.schema_version, sample.schema_version);
        assert_eq!(decoded.baseline_id, sample.baseline_id);
        assert_eq!(decoded.roll_convention, sample.roll_convention);
        assert_eq!(decoded.offset_composition, sample.offset_composition);
        for axis in 0..3 {
            assert!((decoded.position_m[axis] - sample.position_m[axis]).abs() < 1e-12);
            assert!((decoded.follow_offset_m[axis] - sample.follow_offset_m[axis]).abs() < 1e-12);
        }
        for i in 0..4 {
            assert!(
                (decoded.rotation_quaternion_xyzw[i] - sample.rotation_quaternion_xyzw[i]).abs()
                    < 1e-12
            );
        }
        assert!(encoded.contains("\"offset_composition\":\"unvalidated\""));
    }
}

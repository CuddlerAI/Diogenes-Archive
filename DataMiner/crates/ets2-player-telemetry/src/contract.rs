//! ETS2/ATS `player_*` 事实的契约:坐标变换声明、速度来源记账、样本换算。

use serde::{Deserialize, Serialize};
use telemetry_geometry::{
    determinant3, matrix_is_orthonormal, transform_point, transform_rotation,
};

use crate::orientation::{EulerRangeError, EulerTurns, RollConvention};
use crate::{ETS2_METRES_PER_GAME_UNIT, SCHEMA_VERSION_V1};

/// SCS 世界系:X 东 / Y 上 / Z 南,右手系,单位米。
/// (官方头文件 `scssdk_value.h`:"In world space the X points to east, Y up and Z south.")
pub const ETS2_SOURCE_SPACE_V1: &str = "scs-world-x-east-y-up-z-south-rh-m/v1";
/// 交付侧世界系,与 `fallout-player-telemetry` 同一个目标空间(68 号 §6.1)。
pub const DM_WORLD_SPACE_V1: &str = "dm-world-x-right-y-up-z-forward-lh-m/v1";
/// 本 crate 的变换标识。
pub const ETS2_TO_DM_TRANSFORM_V1: &str = "scs-truck-root-to-dm-world/v1";

/// 坐标变换声明。逐字对应 `fallout-player-telemetry` 的同名结构,
/// 因为**目标空间是同一个**,两款游戏的差别只在 `target_from_source_axes` 与比例。
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CoordinateTransformV1 {
    pub transform_id: String,
    pub source_space_id: String,
    pub target_space_id: String,
    pub metres_per_game_unit: f64,
    /// 行主序,按 `target = axes · source · scale` 施加。
    pub target_from_source_axes: [[f64; 3]; 3],
}

impl CoordinateTransformV1 {
    /// SCS 世界系 → 交付世界系。
    ///
    /// SCS 是 X 东 / Y 上 / Z **南**(右手);68 号要的是 X 右 / Y 上 / Z **前**(左手)。
    /// 东对右、上对上都是恒等,**只有第三轴要翻号**:南的反向才是北 = 前。
    /// 于是 `det = −1`——右手系换左手系本来就该翻手性,这不是错误
    /// (`telemetry_geometry` 里 `handedness_flipping_axes_are_accepted` 钉的就是这条)。
    ///
    /// ⚠️ **`metres_per_game_unit` 恒为 1.0,不要填 1:20。**
    /// 那个 1:20 是 SCS 的**里程/时间压缩比**(SDK 里叫 `local.scale`,随分区取 3/15/19),
    /// 与几何比例无关。98 号 §4.2 陷阱 2 与 §6 争点 III 口径 2 都点了这条。
    #[must_use]
    pub fn scs_to_dm_world() -> Self {
        Self {
            transform_id: ETS2_TO_DM_TRANSFORM_V1.to_owned(),
            source_space_id: ETS2_SOURCE_SPACE_V1.to_owned(),
            target_space_id: DM_WORLD_SPACE_V1.to_owned(),
            metres_per_game_unit: ETS2_METRES_PER_GAME_UNIT,
            target_from_source_axes: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, -1.0]],
        }
    }

    pub fn validate_shape(&self) -> Result<(), ContractShapeErrorV1> {
        if self.transform_id.trim().is_empty()
            || self.source_space_id.trim().is_empty()
            || self.target_space_id.trim().is_empty()
        {
            return Err(ContractShapeErrorV1::InvalidField("transform identity"));
        }
        if !self.metres_per_game_unit.is_finite() || self.metres_per_game_unit <= 0.0 {
            return Err(ContractShapeErrorV1::InvalidField("metres_per_game_unit"));
        }
        if !matrix_is_orthonormal(self.target_from_source_axes, 1e-9)
            || (determinant3(self.target_from_source_axes).abs() - 1.0).abs() > 1e-9
        {
            return Err(ContractShapeErrorV1::InvalidField(
                "target_from_source_axes",
            ));
        }
        Ok(())
    }
}

/// 速度值的来源。**永远显式记录,不允许把两种来源混成一个数字。**
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SpeedProvenanceV1 {
    /// `truck.local.velocity.linear` 的模长。
    ///
    /// 官方逐字:"Represents vehicle space linear velocity of the truck measured
    /// in meters per second"。这是**真实运动速度**,也是本 crate 的首选来源。
    LocalLinearVelocityMagnitude,
    /// `truck.speed` 取绝对值。
    ///
    /// 官方逐字:"**Speedometer** speed in meters per second. Uses negative value
    /// to represent reverse movement."——它是**仪表读数**,不保证等于真实地速
    /// (仪表可建模为偏乐观,且轮胎打滑时与地速分离),且带符号表示倒车。
    /// 仅在 velocity 通道缺失时使用,并必须以本取值标注降级。
    SpeedometerAbs,
}

/// 契约形状错误。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContractShapeErrorV1 {
    InvalidField(&'static str),
}

/// 换算失败的原因。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConversionErrorV1 {
    /// 欧拉角越界或非有限。
    Euler(EulerRangeError),
    /// 位置非有限。
    NonFinitePosition,
    /// 速度非有限或为负模长。
    NonFiniteSpeed,
    /// 旋转经变换后不再是纯旋转(理论上不该发生,发生即上游给了坏数据)。
    RotationNotProper,
    /// 契约本身不合法。
    Contract(ContractShapeErrorV1),
}

fn multiply3(matrix: [[f64; 3]; 3], vector: [f64; 3]) -> [f64; 3] {
    matrix.map(|row| row.iter().zip(vector).map(|(a, b)| a * b).sum())
}

/// SDK 侧一帧的原始读数。字段名与官方通道一一对应。
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RawTruckSampleV1 {
    /// `truck.world.placement` 的 position(dvector,世界米)。
    pub world_position_m: [f64; 3],
    /// `truck.world.placement` 的 orientation(euler,圈)。
    pub world_orientation: EulerTurns,
    /// `truck.local.velocity.linear`(fvector,车身系 m/s)。缺失时为 `None`。
    pub local_linear_velocity_mps: Option<[f64; 3]>,
    /// `truck.speed`(float,仪表 m/s,负值表示倒车)。
    pub speedometer_mps: f64,
}

/// 换算后的交付侧 `player_*` 三件套。
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PlayerSampleV1 {
    pub schema_version: String,
    /// 68 号 §6.3 的 `player_position`,交付世界系,单位米。
    pub position_m: [f64; 3],
    /// 68 号 §6.3 的 `player_rotation_quaternion`,xyzw 序,已归一化且 `w ≥ 0`。
    pub rotation_quaternion_xyzw: [f64; 4],
    /// **原始**的 SCS 欧拉读数(圈),原样保留。
    ///
    /// [`RollConvention::Unvalidated`] 的整个承诺是「符号选错时数据仍可重算」,
    /// 而只留四元数是重算不了的——符号已经被吸收进去了。这一格就是那条承诺的
    /// 实际载体,同时也是 `camera` 侧做三级复合的输入。
    pub source_orientation: EulerTurns,
    /// 68 号 §6.3 的 `player_speed`,**非负**标量 m/s。
    pub speed_mps: f64,
    /// 交付世界系下的速度**向量**,m/s。68 号 §6.3 的 `player_speed` 要的是 Vector3,
    /// 标量给不出方向。
    ///
    /// **仅在 `truck.local.velocity.linear` 可用时有值。** 仪表兜底那一路是标量,
    /// 用「前向 × 仪表读数」凑一个向量在打滑与倒车时会指错方向,而那正是仪表与地速
    /// 分离的时刻——所以那一路留 `None`,由交付层拒绝,不在这里编。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub velocity_mps: Option<[f64; 3]>,
    /// 该 `speed_mps` 是怎么来的。
    pub speed_provenance: SpeedProvenanceV1,
    /// 本样本用的 roll 符号约定。
    /// [`RollConvention::Unvalidated`] 表示符号尚未实机标定——交付层据此下调置信度。
    pub roll_convention: RollConvention,
    /// 施加的变换,原样入交付元数据(纪律同 `fallout-player-telemetry`)。
    pub coordinate_transform: CoordinateTransformV1,
}

impl RawTruckSampleV1 {
    /// 把一帧 SDK 读数换算成交付侧样本。
    ///
    /// 语义要点:
    /// - **`player_*` 在 ETS2 语境下指「卡车」不是「人」**;98 号 §6 争点 III 反方论点 2
    ///   已经点明这个语义错位,交付说明里必须写清楚,不要让读者以为是人体位姿。
    /// - 速度优先取 `local.velocity.linear` 的模长;模长是标量,与坐标系无关,
    ///   所以不需要先做轴变换(测试 `speed_magnitude_is_frame_invariant` 钉了这条)。
    pub fn to_player_sample(
        &self,
        roll_convention: RollConvention,
    ) -> Result<PlayerSampleV1, ConversionErrorV1> {
        let transform = CoordinateTransformV1::scs_to_dm_world();
        transform
            .validate_shape()
            .map_err(ConversionErrorV1::Contract)?;
        let axes = transform.target_from_source_axes;

        let position_m =
            transform_point(axes, transform.metres_per_game_unit, self.world_position_m)
                .ok_or(ConversionErrorV1::NonFinitePosition)?;

        let source_rotation = self
            .world_orientation
            .to_world_rotation(roll_convention)
            .map_err(ConversionErrorV1::Euler)?;
        let rotation_quaternion_xyzw = transform_rotation(axes, source_rotation)
            .ok_or(ConversionErrorV1::RotationNotProper)?;

        let (speed_mps, velocity_mps, speed_provenance) = match self.local_linear_velocity_mps {
            Some(v) => {
                if !v.iter().all(|x| x.is_finite()) {
                    return Err(ConversionErrorV1::NonFiniteSpeed);
                }
                let magnitude = v.iter().map(|x| x * x).sum::<f64>().sqrt();
                // 通道给的是**车身系**速度，先转世界再换交付轴——顺序反了会在
                // 任何非正北的朝向上给出错误方向，而模长仍然正确，所以看不出来。
                let world = multiply3(source_rotation, v);
                let delivery = transform_point(axes, transform.metres_per_game_unit, world)
                    .ok_or(ConversionErrorV1::NonFiniteSpeed)?;
                (
                    magnitude,
                    Some(delivery),
                    SpeedProvenanceV1::LocalLinearVelocityMagnitude,
                )
            }
            None => (
                self.speedometer_mps.abs(),
                None,
                SpeedProvenanceV1::SpeedometerAbs,
            ),
        };
        if !speed_mps.is_finite() || speed_mps < 0.0 {
            return Err(ConversionErrorV1::NonFiniteSpeed);
        }

        Ok(PlayerSampleV1 {
            schema_version: SCHEMA_VERSION_V1.to_owned(),
            position_m,
            rotation_quaternion_xyzw,
            source_orientation: self.world_orientation,
            speed_mps,
            velocity_mps,
            speed_provenance,
            roll_convention,
            coordinate_transform: transform,
        })
    }
}

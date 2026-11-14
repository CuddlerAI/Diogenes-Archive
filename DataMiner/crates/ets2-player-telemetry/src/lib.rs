//! ETS2 / ATS `player_*` 契约与坐标换算(采集前置准备,**不含插件与传输**)。
//!
//! 定位与 `fallout-player-telemetry` 相同:本 crate 拥有**版本化的消息、坐标换算、
//! 速度来源记账**,不拥有 SCS 插件 DLL、共享内存布局、`action_camera.json` 组装与投递。
//! 前者可以在 macOS 上写完并测完,后者必须上 Windows——把两者分开,
//! 就不用为了验证一个符号去开一台 Windows 机器。
//!
//! # 它解决的是 68 号的哪一格
//!
//! [98 号 §3](../../../docs/98-steam-wide-map-metric-actioncamera-source-survey.md) 的结论是:
//! `player_*` 是渲染 hook **原理上拿不到**的一格([80 号](../../../docs/80-action-camera-deliverable-gap-analysis.md)
//! 已实证第三人称下不能从相机位姿反推),只能走游戏官方通道。
//! ETS2 的官方通道就是 SCS Telemetry SDK,而它的许可是 **MIT**
//! (`scs_sdk_1_14/sdk_license.txt`,Copyright (C) 2016 SCS Software,本轮一手核验)。
//!
//! # 数据面的三个已知陷阱(全部已钉测试)
//!
//! 1. **角度单位是「圈」不是弧度也不是度**,见 [`orientation::EulerTurns`];
//! 2. **局部 Z 指向后方**,前向是 −Z——照搬「Z 前」会让整批朝向反 180°;
//! 3. **`truck.speed` 是仪表读数且带符号**,真实速度应走 `truck.local.velocity.linear`,
//!    见 [`contract::SpeedProvenanceV1`]。
//!
//! # 一条尚未关闭的项
//!
//! roll 的符号在官方文档里**没有唯一解**,本 crate 拒绝替它猜:
//! [`orientation::RollConvention`] 没有 `Default`,调用方必须显式选,
//! 且标定前应选 [`orientation::RollConvention::Unvalidated`] 以便日后重算。
//! 标定方法见 README。

pub mod camera;
pub mod contract;
pub mod orientation;

pub use camera::{
    CAMERA_SCHEMA_VERSION_V1, CameraConversionErrorV1, CameraIntrinsicsV1, CameraSampleV1,
    FovSource, InteriorCameraBaselineV1, OffsetComposition, RawCameraOffsetsV1,
    compose_interior_camera,
};
pub use contract::{
    ContractShapeErrorV1, ConversionErrorV1, CoordinateTransformV1, DM_WORLD_SPACE_V1,
    ETS2_SOURCE_SPACE_V1, ETS2_TO_DM_TRANSFORM_V1, PlayerSampleV1, RawTruckSampleV1,
    SpeedProvenanceV1,
};
pub use orientation::{EulerRangeError, EulerTurns, RADIANS_PER_TURN, RollConvention, SPEC};

/// 本 crate 的消息版本。
pub const SCHEMA_VERSION_V1: &str = "ets2-player-telemetry/v1";

/// 本 crate 依据的官方 SDK 版本。
pub const SCS_SDK_VERSION: &str = "1.14";
/// `scs_sdk_1_14.zip` 的 SHA-256(2026-08-06 自
/// <https://download.eurotrucksimulator2.com/scs_sdk_1_14.zip> 取得)。
///
/// 钉哈希的理由同 [90 号](../../../docs/90-bannerlord-campaign-map-bounds-and-metric-evidence.md)
/// 第 4 条纪律:SDK 换版后本 crate 的全部字段语义结论**不自动延续**。
pub const SCS_SDK_1_14_ZIP_SHA256: &str =
    "sha256:c6c1f7376b7324994d9f9c567f3c4141fbbf305b6bf803bc4cfeef2437b2023a";

/// Prism3D 的世界单位就是米。
///
/// ⚠️ 这是 `metric_scale`。**不要把 SCS 宣传的 1:20 填进来**——那是里程/时间压缩比
/// (SDK 通道 `local.scale`,随分区取 3/15/19),不是几何比例。
pub const ETS2_METRES_PER_GAME_UNIT: f64 = 1.0;

/// 官方通道名,逐字取自 SDK 头文件,避免各处手写字符串写错。
pub mod channels {
    /// `include/common/scssdk_telemetry_truck_common_channels.h`
    /// "Represents world space position and orientation of the truck." (dplacement)
    pub const WORLD_PLACEMENT: &str = "truck.world.placement";
    /// "Represents vehicle space linear velocity of the truck measured in meters per second." (fvector)
    pub const LOCAL_LINEAR_VELOCITY: &str = "truck.local.velocity.linear";
    /// "**Speedometer** speed in meters per second. Uses negative value to represent reverse movement." (float)
    pub const SPEEDOMETER: &str = "truck.speed";
    /// "Represents a vehicle space position and orientation delta of the cabin from its default position." (fplacement)
    pub const CABIN_OFFSET: &str = "truck.cabin.offset";
    /// "...cabin space position and orientation delta of the driver head from its default position." (fplacement)
    pub const HEAD_OFFSET: &str = "truck.head.offset";
}

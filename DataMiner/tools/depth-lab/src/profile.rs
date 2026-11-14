//! `config/depth-profiles/<game_id>.lane{30,60}.json` 的 schema 与校验
//! （[77 号 §5.8/§5.10/§9.2](../../../docs/77-depth-ground-truth-hook-experiment-plan.md)）。
//!
//! 这份 profile 是 probe（Windows 侧）唯一的输入，也是 operator 人工校准结论的落点。
//! 它把方案里几条**必须由人确认、不能靠启发式**的东西固化下来：
//! 选中的是哪张 DSV、水印打在哪、`(A,B)` 从哪来、车道是哪条、build 钉在哪个哈希。
//!
//! **校验规则的设计原则：宁可拒绝启动，不可静默跑偏。**
//! 每一条 [`ProfileError`] 都对应方案里一个会导致「数据静默变质」的失效模式。

use serde::{Deserialize, Serialize};

use crate::{
    calibration::{MetricScaleCalibration, MetricScaleError},
    d3d12_selection::D3d12DepthSelection,
    probe_config::DiscoveryDepthFormat,
};

/// 车道（§5.8）。一个 session 只属于一条，且必须与 client 的 capture profile 帧率档一致（G4 门）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Lane {
    /// 1080p30 video + 15fps depth（偶数帧）——对齐 68 号的交付形态。
    Lane30,
    /// 1080p60 video + 30fps depth（偶数帧）——研究车道。
    Lane60,
}

/// 连续层采样节拍。旧 profile 缺省保持按目标 Hz 限流；新采集可显式选择逐 Present。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DepthCadence {
    /// 按车道一半频率定时采样（lane30=15Hz、lane60=30Hz）。
    #[default]
    TargetHz,
    /// 每个 Present 都采；解决录制重复帧导致的 present/video 相位漂移。
    EveryPresent,
}

impl Lane {
    #[must_use]
    pub fn video_fps(self) -> u32 {
        match self {
            Self::Lane30 => 30,
            Self::Lane60 => 60,
        }
    }

    /// 连续层的 depth 频率：**恒为视频帧率的一半**（68 号 §6.5 的偶数帧约定）。
    #[must_use]
    pub fn depth_hz(self) -> u32 {
        self.video_fps() / 2
    }
}

/// `(A, B)` 的来源（§5.5.2）。**必须显式声明**——它决定 §9.3 B 组指标能不能出结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectionSource {
    /// 路① 引擎已知默认值（near/far cvar + `metric_scale` 惯例）。
    EngineDefault,
    /// 路② 游戏内已知距离标定。
    Calibrated,
    /// 路③ 从渲染常量缓冲读投影矩阵（P4；需 §1.1④ 解除）。
    ConstantBuffer,
}

/// 回读路径（§5.3）。MSAA 会强制走 typeless。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadbackPath {
    /// `CopyResource → STAGING → Map`。hook 面小一半，但 **MSAA 下不可用**。
    CopyToStaging,
    /// typeless 升级 + `Texture2DMS` shader 解析 + GPU 侧降采样。
    TypelessShader,
    /// D3D12 resource-state/queue 路径；使用 `CopyTextureRegion` 写入 readback heap。
    ///
    /// 独立 wire 值确保旧客户端在反序列化时直接拒绝 v2 profile，不能误走
    /// D3D11 late-attach 回读路径。
    D3d12CopyTextureRegion,
}

/// operator 人工校准出的 DSV 选择结论（§5.10）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DsvSelection {
    /// `dump-dsv` 里 operator 目视选定的那张。
    pub dsv_id: String,
    pub expected_width: u32,
    pub expected_height: u32,
    /// `DXGI_SAMPLE_DESC::Count`。>1 时 [`ReadbackPath::CopyToStaging`] 非法。
    pub sample_count: u32,
    /// Exact formats accepted for this build. An empty legacy value keeps the historical
    /// all-supported-formats behavior.
    #[serde(default)]
    pub formats: Vec<DiscoveryDepthFormat>,
    /// 证据文件（缩略图 + 打分拆解）的相对路径。**没有证据不许上线。**
    pub evidence_path: String,
}

/// 水印矩形（§5.6.1）。坐标固定已知，无条件进掩码集（§9.2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatermarkRect {
    pub origin_x: u32,
    pub origin_y: u32,
}

/// 突发层配置（§5.5.4）。原生分辨率 + 原始精度 + **逐帧**。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BurstConfig {
    pub interval_seconds: u32,
    pub duration_seconds: u32,
}

/// Rendering backend that produced the depth resource. Older profiles omitted
/// this field because the only production probe was D3D11; serde therefore
/// preserves that meaning as the default instead of guessing from the game.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RendererBackend {
    #[default]
    D3d11,
    D3d12,
}

/// 完整的 per-game per-lane profile。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DepthProfile {
    pub schema_version: String,
    pub game_id: String,
    #[serde(default)]
    pub renderer_backend: RendererBackend,
    pub lane: Lane,
    /// 连续层采样节拍。缺省值保证旧 profile 行为不变。
    #[serde(default)]
    pub cadence: DepthCadence,
    /// Reference build 钉扣。默认不匹配即拒绝启动；Fallout 4 按 owner
    /// 2026-09-04 决策保留该值作证据与认证键，但不再执行运行时匹配。
    pub executable_sha256: String,
    pub dsv: DsvSelection,
    /// Stable D3D12 resource/view/transition identity. D3D11 must leave this absent;
    /// D3D12 must provide it so a process-local descriptor id is never treated as a
    /// cross-launch production selector.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub d3d12_selection: Option<D3d12DepthSelection>,
    pub readback_path: ReadbackPath,
    pub readback_slots: usize,
    pub watermark: WatermarkRect,
    /// 交付层分辨率。68 号 §6.4 固定 1280×720。
    pub delivery_width: u32,
    pub delivery_height: u32,
    pub burst: BurstConfig,
    pub projection_a: f64,
    pub projection_b: f64,
    /// 游戏单位 → 米。**刻意不是裸 `f64`**：§5.5.2 证明这个量无法由渲染观测单独定出，
    /// 必须来自管线之外的锚点或引擎惯例，因此它必须自带来源与证据
    /// （[`crate::calibration::MetricScaleCalibration`]）。
    pub metric_scale: MetricScaleCalibration,
    pub projection_source: ProjectionSource,
}

/// 每一条都对应方案里一个「会导致数据静默变质」的失效模式。
///
/// 不实现 `Eq`：`UnusableMetricScale` / `DegenerateProjection` 携带浮点。
#[derive(Debug, Clone, PartialEq)]
pub enum ProfileError {
    /// renderer、schema 必须成对演进，禁止按游戏名或未知字段猜后端。
    RendererSchemaMismatch {
        renderer: RendererBackend,
        expected: &'static str,
        actual: String,
    },
    /// renderer 与 readback wire 不匹配会把资源送入错误的 API 路径。
    RendererReadbackPathMismatch {
        renderer: RendererBackend,
        actual: ReadbackPath,
    },
    /// 当前 D3D12 readback 只证明了 non-MSAA texture。
    D3d12RequiresSingleSample { sample_count: u32 },
    /// D3D12 production must pin a cross-launch resource/view/transition identity.
    MissingD3d12Selection,
    /// D3D11 profiles cannot carry a D3D12 selector.
    UnexpectedD3d12Selection,
    /// The selector is structurally invalid or contradicts the profile DSV declaration.
    InvalidD3d12Selection { reason: String },
    /// v2 米制证据必须钉住 exact bytes，只有相对路径不足以防止内容漂移。
    MissingMetricScaleEvidenceHash,
    /// 哈希必须使用仓库统一的规范 wire 格式。
    InvalidMetricScaleEvidenceHash { value: String },
    /// §5.3.1：MSAA 下 `CopyResource → STAGING → Map` 在 API 层面不可用。
    MsaaRequiresTypeless { sample_count: u32 },
    /// §5.10：没有证据的 DSV 选择不许上线。
    MissingDsvEvidence,
    /// R-17：build 没钉住，补丁一来数据就静默变质。
    MissingExecutableHash,
    /// §5.4：环形池 <4 张在 60fps 下会频繁池满。
    ReadbackPoolTooSmall { slots: usize },
    /// 68 号 §6.4 指定 720p。偏离必须是显式决定，不能手滑。
    UnexpectedDeliveryResolution { width: u32, height: u32 },
    /// §5.5.1：`b == 0` 会让 `z = b/(d-a)` 恒为 0。
    DegenerateProjection,
    /// §5.5.2：`metric_scale` 非正、缺证据、或来源未经交叉验证。
    ///
    /// 具体哪一种见内层的 [`MetricScaleError`]——这里不摊平，是为了让
    /// 「比例值的合法性」始终只有一处定义（`calibration` 模块），profile 只做转发。
    UnusableMetricScale(MetricScaleError),
    /// 突发层时长 ≥ 间隔就不是「突发」，是全量——体量会失控（R-12）。
    BurstNotIntermittent { interval: u32, duration: u32 },
    /// 水印矩形超出交付/渲染画面。
    WatermarkOutOfBounds,
}

impl std::fmt::Display for ProfileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RendererSchemaMismatch {
                renderer,
                expected,
                actual,
            } => write!(
                f,
                "renderer_backend={renderer:?} 必须使用 schema_version={expected}，实际为 {actual}"
            ),
            Self::RendererReadbackPathMismatch { renderer, actual } => write!(
                f,
                "renderer_backend={renderer:?} 与 readback_path={actual:?} 不匹配"
            ),
            Self::D3d12RequiresSingleSample { sample_count } => write!(
                f,
                "D3D12 profile 当前只支持 non-MSAA sample_count=1，实际为 {sample_count}"
            ),
            Self::MissingD3d12Selection => write!(
                f,
                "D3D12 profile 缺跨启动稳定的 resource/view/transition selection"
            ),
            Self::UnexpectedD3d12Selection => {
                write!(f, "D3D11 profile 不得携带 D3D12 selection")
            }
            Self::InvalidD3d12Selection { reason } => {
                write!(f, "D3D12 selection 与 profile 不一致：{reason}")
            }
            Self::MissingMetricScaleEvidenceHash => write!(
                f,
                "D3D12 depth profile 的 metric_scale 缺 evidence_sha256，证据内容未钉住"
            ),
            Self::InvalidMetricScaleEvidenceHash { value } => write!(
                f,
                "metric_scale.evidence_sha256={value:?} 不是规范的 sha256:<64 lowercase hex>"
            ),
            Self::MsaaRequiresTypeless { sample_count } => write!(
                f,
                "DSV 是 MSAA ×{sample_count}，但 readback_path 声明为 copy_to_staging；\
                 多重采样纹理必须 USAGE_DEFAULT + CPUAccessFlags=0，不存在 MSAA 的 STAGING 纹理（§5.3.1）"
            ),
            Self::MissingDsvEvidence => {
                write!(
                    f,
                    "dsv.evidence_path 为空：没有人工校准证据的 DSV 选择不许上线（§5.10）"
                )
            }
            Self::MissingExecutableHash => write!(
                f,
                "executable_sha256 为空：build 未钉住，游戏补丁会让 DSV 选择静默失效（R-17）"
            ),
            Self::ReadbackPoolTooSmall { slots } => write!(
                f,
                "readback_slots={slots} 小于 §5.4 要求的 4 张，60fps 下会频繁池满"
            ),
            Self::UnexpectedDeliveryResolution { width, height } => write!(
                f,
                "交付分辨率 {width}×{height} 不是 68 号 §6.4 指定的 1280×720"
            ),
            Self::DegenerateProjection => {
                write!(f, "projection_b 为 0：z = b/(d−a) 将恒为 0（§5.5.1）")
            }
            Self::UnusableMetricScale(inner) => write!(f, "{inner}（§5.5.2）"),
            Self::BurstNotIntermittent { interval, duration } => write!(
                f,
                "突发层 duration={duration}s ≥ interval={interval}s，这不是突发是全量，体量会失控（R-12）"
            ),
            Self::WatermarkOutOfBounds => {
                write!(f, "水印矩形超出画面边界")
            }
        }
    }
}

impl std::error::Error for ProfileError {}

/// **只有投影参数**的候选文件——不是 profile。
///
/// # 为什么它是独立的一种东西
///
/// [`DepthProfile`] 要求 `executable_sha256`、DSV 证据、回读路径这些**只能在
/// 采集机上取到**的字段。而投影参数可以在别处先定出来（[91 号](../../../docs/91-fallout4-depth-projection-evidence.md) 的 Fallout 4
/// 就是从引擎 ini 默认值 + 公开单位换算推的）。
///
/// 把候选值硬塞进 `DepthProfile` 有两条路，都不行：编造那些字段是伪造证据；
/// 放宽 `validate()` 则是把整个 profile 的门一起拆了。所以它单独一个类型，
/// 走单独一条加载路径，落进索引时是 [`crate::capture_timing::IndexProjectionSource::Candidate`]。
///
/// # 它换来什么
///
/// 在标定落地（#485）之前，采集仍然只能得到 `proj_source=unknown`。而带上候选值：
///
/// - 索引里有数字，事后能和标定结果比对，看当初推得准不准；
/// - **`reverse_z` 有值了**，于是收口那道「传统 Z 却按 identity 落盘」的门
///   才会真正触发——`unknown` 时它根本不检查；
/// - 采集期的编码体检有了一个独立的对照（数据反推的约定 vs 候选声明的约定）。
///
/// 它**不**换来交付资格：[`crate::capture_timing::IndexProjection::is_metric_resolvable`]
/// 对 `Candidate` 返回 `false`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CandidateProjection {
    pub schema_version: String,
    pub game_id: String,
    pub projection_a: f64,
    pub projection_b: f64,
    pub metric_scale: f64,
    /// 还差什么才能升级成已标定。**空数组即非法**——
    /// 一个「什么都不缺」的候选就该是 profile，不该走这条路。
    #[serde(default)]
    pub blocking: Vec<String>,
}

impl CandidateProjection {
    pub const SCHEMA_VERSION: &'static str = "dm-depth-projection-candidate-1";

    /// 候选值本身能不能用来采集。**注意这不是「能不能交付」**——那永远是否。
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != Self::SCHEMA_VERSION {
            return Err(format!(
                "候选投影的 schema_version 是 `{}`，本版本只认 `{}`",
                self.schema_version,
                Self::SCHEMA_VERSION
            ));
        }
        if !self.projection_a.is_finite() || !self.projection_b.is_finite() {
            return Err("projection_a/b 必须是有限数".into());
        }
        if self.projection_b == 0.0 {
            return Err("projection_b == 0 会让 z = b/(d−a) 恒为 0".into());
        }
        if !self.metric_scale.is_finite() || self.metric_scale <= 0.0 {
            return Err(format!("metric_scale {} 不是正有限数", self.metric_scale));
        }
        if self.blocking.is_empty() {
            return Err(
                "候选文件的 blocking 是空的——什么都不缺就该做成正式 profile，\
                 走这条路会让「未验证」这件事在索引里消失"
                    .into(),
            );
        }
        Ok(())
    }

    #[must_use]
    pub fn projection(&self) -> crate::depth::Projection {
        crate::depth::Projection {
            a: self.projection_a,
            b: self.projection_b,
            metric_scale: self.metric_scale,
        }
    }
}

impl DepthProfile {
    pub const SCHEMA_VERSION: &'static str = "dm-depth-profile-1";
    pub const D3D12_SCHEMA_VERSION: &'static str = "dm-depth-profile-2";
    pub const D3D12_STABLE_SCHEMA_VERSION: &'static str = "dm-depth-profile-3";
    /// 68 号 §6.4 指定的交付分辨率。
    pub const DELIVERY_WIDTH: u32 = 1280;
    pub const DELIVERY_HEIGHT: u32 = 720;

    /// 磁盘门使用的标称采样频率。逐 Present 模式要求游戏锁在车道帧率。
    #[must_use]
    pub fn depth_hz(&self) -> u32 {
        match self.cadence {
            DepthCadence::TargetHz => self.lane.depth_hz(),
            DepthCadence::EveryPresent => self.lane.video_fps(),
        }
    }

    /// 全量校验。**返回全部问题而不是第一个**——让 operator 一次改完，而不是改一条跑一次。
    pub fn validate(&self, frame_width: u32, frame_height: u32) -> Vec<ProfileError> {
        let mut errs = Vec::new();

        let (schema_matches_renderer, expected_schema, readback_matches_renderer) =
            match self.renderer_backend {
                RendererBackend::D3d11 => (
                    self.schema_version == Self::SCHEMA_VERSION,
                    Self::SCHEMA_VERSION,
                    self.readback_path != ReadbackPath::D3d12CopyTextureRegion,
                ),
                RendererBackend::D3d12 => (
                    matches!(
                        self.schema_version.as_str(),
                        Self::D3D12_SCHEMA_VERSION | Self::D3D12_STABLE_SCHEMA_VERSION
                    ),
                    "dm-depth-profile-2 or dm-depth-profile-3",
                    self.readback_path == ReadbackPath::D3d12CopyTextureRegion,
                ),
            };
        if !schema_matches_renderer {
            errs.push(ProfileError::RendererSchemaMismatch {
                renderer: self.renderer_backend,
                expected: expected_schema,
                actual: self.schema_version.clone(),
            });
        }
        if !readback_matches_renderer {
            errs.push(ProfileError::RendererReadbackPathMismatch {
                renderer: self.renderer_backend,
                actual: self.readback_path,
            });
        }
        if self.renderer_backend == RendererBackend::D3d12 && self.dsv.sample_count != 1 {
            errs.push(ProfileError::D3d12RequiresSingleSample {
                sample_count: self.dsv.sample_count,
            });
        }
        match (
            self.renderer_backend,
            self.schema_version.as_str(),
            &self.d3d12_selection,
        ) {
            (RendererBackend::D3d11, _, Some(_))
            | (RendererBackend::D3d12, Self::D3D12_SCHEMA_VERSION, Some(_)) => {
                errs.push(ProfileError::UnexpectedD3d12Selection);
            }
            (RendererBackend::D3d12, Self::D3D12_STABLE_SCHEMA_VERSION, None) => {
                errs.push(ProfileError::MissingD3d12Selection);
            }
            (RendererBackend::D3d12, Self::D3D12_STABLE_SCHEMA_VERSION, Some(selection)) => {
                if let Err(reason) = selection.validate() {
                    errs.push(ProfileError::InvalidD3d12Selection { reason });
                } else if selection.resource.width != u64::from(self.dsv.expected_width)
                    || selection.resource.height != self.dsv.expected_height
                    || selection.resource.sample_count != self.dsv.sample_count
                    || !self.dsv.formats.contains(&selection.resource.format)
                    || !self.dsv.formats.contains(&selection.view.format)
                {
                    errs.push(ProfileError::InvalidD3d12Selection {
                        reason: "resource/view geometry or formats do not match dsv".into(),
                    });
                }
            }
            (RendererBackend::D3d11, _, None)
            | (RendererBackend::D3d12, Self::D3D12_SCHEMA_VERSION, None)
            | (RendererBackend::D3d12, _, _) => {}
        }
        match self.metric_scale.evidence_sha256.as_deref() {
            None if matches!(
                self.schema_version.as_str(),
                Self::D3D12_SCHEMA_VERSION | Self::D3D12_STABLE_SCHEMA_VERSION
            ) =>
            {
                errs.push(ProfileError::MissingMetricScaleEvidenceHash);
            }
            Some(value) if !crate::camera_binding::is_sha256_digest(value) => {
                errs.push(ProfileError::InvalidMetricScaleEvidenceHash {
                    value: value.to_owned(),
                });
            }
            None | Some(_) => {}
        }

        if self.dsv.sample_count > 1 && self.readback_path == ReadbackPath::CopyToStaging {
            errs.push(ProfileError::MsaaRequiresTypeless {
                sample_count: self.dsv.sample_count,
            });
        }
        if self.dsv.evidence_path.trim().is_empty() {
            errs.push(ProfileError::MissingDsvEvidence);
        }
        if self.executable_sha256.trim().is_empty() {
            errs.push(ProfileError::MissingExecutableHash);
        }
        if self.readback_slots < crate::readback::ReadbackPool::MIN_SLOTS {
            errs.push(ProfileError::ReadbackPoolTooSmall {
                slots: self.readback_slots,
            });
        }
        if self.delivery_width != Self::DELIVERY_WIDTH
            || self.delivery_height != Self::DELIVERY_HEIGHT
        {
            errs.push(ProfileError::UnexpectedDeliveryResolution {
                width: self.delivery_width,
                height: self.delivery_height,
            });
        }
        if self.projection_b == 0.0 {
            errs.push(ProfileError::DegenerateProjection);
        }
        if let Err(e) = self.metric_scale.validate() {
            errs.push(ProfileError::UnusableMetricScale(e));
        }
        if self.burst.duration_seconds >= self.burst.interval_seconds {
            errs.push(ProfileError::BurstNotIntermittent {
                interval: self.burst.interval_seconds,
                duration: self.burst.duration_seconds,
            });
        }
        if self
            .watermark
            .origin_x
            .checked_add(crate::watermark::RECT_W)
            .is_none_or(|right| right > frame_width)
            || self
                .watermark
                .origin_y
                .checked_add(crate::watermark::RECT_H)
                .is_none_or(|bottom| bottom > frame_height)
        {
            errs.push(ProfileError::WatermarkOutOfBounds);
        }
        errs
    }

    /// 该 profile 对应的 [`crate::depth::Projection`]。
    #[must_use]
    pub fn projection(&self) -> crate::depth::Projection {
        crate::depth::Projection {
            a: self.projection_a,
            b: self.projection_b,
            metric_scale: self.metric_scale.value,
        }
    }

    /// 某个 `session_frame` 是否应当有连续层 depth（偶数帧约定，68 号 §6.5）。
    #[must_use]
    pub fn expects_depth(&self, session_frame: u64) -> bool {
        session_frame.is_multiple_of(2)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calibration::{CrossCheck, DEFAULT_CROSS_CHECK_TOLERANCE};
    use crate::d3d12_selection::{
        D3d12DepthSelection, D3d12DsvIdentity, D3d12ResourceIdentity, D3d12TransitionIdentity,
    };

    fn valid() -> DepthProfile {
        DepthProfile {
            schema_version: DepthProfile::SCHEMA_VERSION.into(),
            game_id: "bioshock-infinite-steam-e2e".into(),
            renderer_backend: RendererBackend::D3d11,
            lane: Lane::Lane30,
            cadence: DepthCadence::TargetHz,
            executable_sha256: "sha256:c20a4252…".into(),
            dsv: DsvSelection {
                dsv_id: "main-scene-v1".into(),
                expected_width: 1920,
                expected_height: 1080,
                sample_count: 1,
                formats: Vec::new(),
                evidence_path: "session/depth-profile-evidence.json".into(),
            },
            d3d12_selection: None,
            readback_path: ReadbackPath::CopyToStaging,
            readback_slots: 4,
            watermark: WatermarkRect {
                origin_x: 0,
                origin_y: 0,
            },
            delivery_width: 1280,
            delivery_height: 720,
            burst: BurstConfig {
                interval_seconds: 300,
                duration_seconds: 3,
            },
            projection_a: 0.0,
            projection_b: 10.0,
            // 路①（UE3 惯例 1uu≈2cm）+ 一次锚点交叉验证 —— §5.5.2 要求的最小合规组合。
            metric_scale: MetricScaleCalibration::from_engine_convention(
                "ue3",
                0.02,
                "session/metric-scale-evidence.json",
            )
            .with_cross_check(CrossCheck::new(
                0.02,
                0.0203,
                DEFAULT_CROSS_CHECK_TOLERANCE,
            )),
            projection_source: ProjectionSource::EngineDefault,
        }
    }

    fn valid_d3d12() -> DepthProfile {
        let mut profile = valid();
        profile.schema_version = DepthProfile::D3D12_STABLE_SCHEMA_VERSION.into();
        profile.renderer_backend = RendererBackend::D3d12;
        profile.dsv.formats = vec![
            DiscoveryDepthFormat::R24G8Typeless,
            DiscoveryDepthFormat::D24UnormS8Uint,
        ];
        profile.d3d12_selection = Some(D3d12DepthSelection {
            resource: D3d12ResourceIdentity {
                dimension: 3,
                width: 1920,
                height: 1080,
                depth_or_array_size: 1,
                mip_levels: 1,
                format: DiscoveryDepthFormat::R24G8Typeless,
                sample_count: 1,
                sample_quality: 0,
                layout: 0,
                flags: 2,
            },
            view: D3d12DsvIdentity {
                format: DiscoveryDepthFormat::D24UnormS8Uint,
                dimension: 3,
                flags: 0,
            },
            transition: D3d12TransitionIdentity {
                subresource: 0,
                state_before: 16,
                state_after: 128,
                barrier_flags: 0,
            },
            fallback_draw: None,
        });
        profile.readback_path = ReadbackPath::D3d12CopyTextureRegion;
        profile.metric_scale = profile
            .metric_scale
            .with_evidence_sha256(format!("sha256:{}", "a".repeat(64)));
        profile
    }

    #[test]
    fn legacy_profile_defaults_to_d3d11_backend() {
        let mut json = serde_json::to_value(valid()).unwrap();
        json.as_object_mut().unwrap().remove("renderer_backend");
        assert!(json["metric_scale"].get("evidence_sha256").is_none());
        let profile: DepthProfile = serde_json::from_value(json).unwrap();
        assert_eq!(profile.renderer_backend, RendererBackend::D3d11);
        assert_eq!(profile.metric_scale.evidence_sha256, None);
        assert!(profile.validate(1920, 1080).is_empty());
    }

    #[test]
    fn d3d12_backend_is_explicit_in_profile_json() {
        let profile = valid_d3d12();
        let json = serde_json::to_value(&profile).unwrap();
        assert_eq!(json["renderer_backend"], "d3d12");
        assert_eq!(json["schema_version"], "dm-depth-profile-3");
        assert_eq!(json["readback_path"], "d3d12_copy_texture_region");
        assert_eq!(json["d3d12_selection"]["transition"]["state_after"], 128);
        assert_eq!(
            json["metric_scale"]["evidence_sha256"],
            format!("sha256:{}", "a".repeat(64))
        );
        let round_trip: DepthProfile = serde_json::from_value(json).unwrap();
        assert_eq!(round_trip, profile);
        assert!(round_trip.validate(1920, 1080).is_empty());
    }

    #[test]
    fn d3d12_requires_a_cross_launch_selection() {
        let mut profile = valid_d3d12();
        profile.d3d12_selection = None;
        assert!(
            profile
                .validate(1920, 1080)
                .contains(&ProfileError::MissingD3d12Selection)
        );

        let mut d3d11 = valid();
        d3d11.d3d12_selection = valid_d3d12().d3d12_selection;
        assert!(
            d3d11
                .validate(1920, 1080)
                .contains(&ProfileError::UnexpectedD3d12Selection)
        );
    }

    #[test]
    fn legacy_d3d12_v2_keeps_its_existing_dynamic_selector_contract() {
        let mut profile = valid_d3d12();
        profile.schema_version = DepthProfile::D3D12_SCHEMA_VERSION.into();
        profile.d3d12_selection = None;
        assert!(profile.validate(1920, 1080).is_empty());
    }

    #[test]
    fn d3d12_selection_must_match_the_profile_dsv() {
        let mut profile = valid_d3d12();
        profile.d3d12_selection.as_mut().unwrap().resource.width = 2560;
        assert!(
            profile
                .validate(1920, 1080)
                .iter()
                .any(|error| matches!(error, ProfileError::InvalidD3d12Selection { .. }))
        );
    }

    #[test]
    fn renderer_schema_and_readback_path_are_fail_closed_pairs() {
        let mut d3d12 = valid_d3d12();
        d3d12.schema_version = DepthProfile::SCHEMA_VERSION.into();
        assert!(d3d12.validate(1920, 1080).iter().any(|error| matches!(
            error,
            ProfileError::RendererSchemaMismatch {
                renderer: RendererBackend::D3d12,
                ..
            }
        )));

        let mut d3d12 = valid_d3d12();
        d3d12.readback_path = ReadbackPath::CopyToStaging;
        assert!(d3d12.validate(1920, 1080).iter().any(|error| matches!(
            error,
            ProfileError::RendererReadbackPathMismatch {
                renderer: RendererBackend::D3d12,
                ..
            }
        )));

        let mut d3d11 = valid();
        d3d11.schema_version = DepthProfile::D3D12_SCHEMA_VERSION.into();
        assert!(d3d11.validate(1920, 1080).iter().any(|error| matches!(
            error,
            ProfileError::RendererSchemaMismatch {
                renderer: RendererBackend::D3d11,
                ..
            }
        )));

        let mut d3d11 = valid();
        d3d11.readback_path = ReadbackPath::D3d12CopyTextureRegion;
        assert!(d3d11.validate(1920, 1080).iter().any(|error| matches!(
            error,
            ProfileError::RendererReadbackPathMismatch {
                renderer: RendererBackend::D3d11,
                ..
            }
        )));
    }

    #[test]
    fn d3d12_rejects_unproven_msaa_readback() {
        for sample_count in [0, 2, 4] {
            let mut profile = valid_d3d12();
            profile.dsv.sample_count = sample_count;
            assert!(
                profile
                    .validate(1920, 1080)
                    .contains(&ProfileError::D3d12RequiresSingleSample { sample_count })
            );
        }
    }

    #[test]
    fn profile_v2_requires_a_canonical_metric_evidence_hash() {
        let mut profile = valid_d3d12();
        profile.metric_scale.evidence_sha256 = None;
        assert!(
            profile
                .validate(1920, 1080)
                .contains(&ProfileError::MissingMetricScaleEvidenceHash)
        );

        for value in [
            String::new(),
            "a".repeat(64),
            format!("sha256:{}", "A".repeat(64)),
            format!("sha256:{}", "a".repeat(63)),
        ] {
            let mut profile = valid_d3d12();
            profile.metric_scale.evidence_sha256 = Some(value);
            assert!(
                profile.validate(1920, 1080).iter().any(|error| matches!(
                    error,
                    ProfileError::InvalidMetricScaleEvidenceHash { .. }
                ))
            );
        }
    }

    #[test]
    fn a_well_formed_profile_passes() {
        assert!(valid().validate(1920, 1080).is_empty());
    }

    #[test]
    fn shipped_fallout4_profile_pins_first_person_projection() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../config/catalogs/dev/fallout4-steam-release-test/depth-profile.json");
        let bytes =
            std::fs::read(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        let profile: DepthProfile = serde_json::from_slice(&bytes)
            .unwrap_or_else(|error| panic!("parse {}: {error}", path.display()));

        assert!(profile.validate(1920, 1080).is_empty());
        assert_eq!(profile.schema_version, DepthProfile::SCHEMA_VERSION);
        let projection = profile.projection();
        assert!((-projection.b / projection.a - 1.0).abs() < 1.0e-6);
    }

    #[test]
    fn lane_pins_the_even_frame_convention() {
        assert_eq!(Lane::Lane30.video_fps(), 30);
        assert_eq!(Lane::Lane30.depth_hz(), 15);
        assert_eq!(Lane::Lane60.video_fps(), 60);
        assert_eq!(Lane::Lane60.depth_hz(), 30);
    }

    #[test]
    fn legacy_profile_without_cadence_keeps_target_hz_behavior() {
        let mut json = serde_json::to_value(valid()).unwrap();
        json.as_object_mut().unwrap().remove("cadence");
        json["dsv"].as_object_mut().unwrap().remove("formats");
        let profile: DepthProfile = serde_json::from_value(json).unwrap();
        assert_eq!(profile.cadence, DepthCadence::TargetHz);
        assert_eq!(profile.depth_hz(), 15);
        assert!(profile.dsv.formats.is_empty());
    }

    #[test]
    fn profile_can_pin_an_exact_depth_format_family() {
        let mut profile = valid();
        profile.dsv.formats = vec![
            DiscoveryDepthFormat::R32G8X24Typeless,
            DiscoveryDepthFormat::D32FloatS8X24Uint,
        ];
        let json = serde_json::to_value(&profile).unwrap();
        let round_trip: DepthProfile = serde_json::from_value(json).unwrap();
        assert_eq!(round_trip.dsv.formats, profile.dsv.formats);
    }

    #[test]
    fn every_present_uses_the_lane_present_rate_for_budgeting() {
        let mut profile = valid();
        profile.cadence = DepthCadence::EveryPresent;
        assert_eq!(profile.depth_hz(), 30);
        assert_eq!(
            serde_json::to_value(profile).unwrap()["cadence"],
            "every_present"
        );
    }

    #[test]
    fn msaa_with_copy_to_staging_is_rejected() {
        // §5.3.1 是可行性前置条件，不是性能权衡——必须在启动前拦下
        let mut p = valid();
        p.dsv.sample_count = 4;
        let errs = p.validate(1920, 1080);
        assert!(errs.contains(&ProfileError::MsaaRequiresTypeless { sample_count: 4 }));
    }

    #[test]
    fn msaa_with_typeless_path_is_accepted() {
        let mut p = valid();
        p.dsv.sample_count = 4;
        p.readback_path = ReadbackPath::TypelessShader;
        assert!(p.validate(1920, 1080).is_empty());
    }

    #[test]
    fn a_dsv_without_evidence_cannot_ship() {
        // §5.10：「选中的是哪张、有图为证」才是可接受的验收标准
        let mut p = valid();
        p.dsv.evidence_path = "  ".into();
        assert!(
            p.validate(1920, 1080)
                .contains(&ProfileError::MissingDsvEvidence)
        );
    }

    #[test]
    fn an_unpinned_build_cannot_ship() {
        let mut p = valid();
        p.executable_sha256.clear();
        assert!(
            p.validate(1920, 1080)
                .contains(&ProfileError::MissingExecutableHash)
        );
    }

    #[test]
    fn burst_must_actually_be_intermittent() {
        // duration ≥ interval 就是全量，体量会失控（R-12）
        let mut p = valid();
        p.burst.duration_seconds = 300;
        assert!(
            p.validate(1920, 1080)
                .iter()
                .any(|e| matches!(e, ProfileError::BurstNotIntermittent { .. }))
        );
    }

    #[test]
    fn watermark_must_fit_the_frame() {
        let mut p = valid();
        p.watermark.origin_x = 1900; // 192 宽的矩形放不下
        assert!(
            p.validate(1920, 1080)
                .contains(&ProfileError::WatermarkOutOfBounds)
        );
    }

    #[test]
    fn watermark_coordinate_overflow_fails_closed() {
        let mut p = valid();
        p.watermark.origin_x = u32::MAX;
        p.watermark.origin_y = u32::MAX;
        assert!(
            p.validate(1920, 1080)
                .contains(&ProfileError::WatermarkOutOfBounds)
        );
    }

    #[test]
    fn delivery_resolution_deviation_must_be_deliberate() {
        let mut p = valid();
        p.delivery_width = 1920;
        p.delivery_height = 1080;
        assert!(
            p.validate(1920, 1080)
                .iter()
                .any(|e| matches!(e, ProfileError::UnexpectedDeliveryResolution { .. }))
        );
    }

    #[test]
    fn degenerate_projection_is_caught_before_it_produces_zeros() {
        let mut p = valid();
        p.projection_b = 0.0;
        assert!(
            p.validate(1920, 1080)
                .contains(&ProfileError::DegenerateProjection)
        );
    }

    #[test]
    fn a_metric_scale_without_provenance_cannot_ship() {
        // §5.5.2：这个量无法由渲染观测单独定出，所以「值 + 来源 + 证据」是一体的。
        // 只填惯例常数、不做交叉验证 = 把一个假设当成测量值发出去。
        let mut p = valid();
        p.metric_scale = MetricScaleCalibration::from_engine_convention(
            "ue3",
            0.02,
            "session/metric-scale-evidence.json",
        );
        assert!(p.validate(1920, 1080).iter().any(|e| matches!(
            e,
            ProfileError::UnusableMetricScale(MetricScaleError::ConventionNotCrossChecked { .. })
        )));
    }

    #[test]
    fn a_failed_metric_scale_cross_check_cannot_ship() {
        let mut p = valid();
        p.metric_scale = p.metric_scale.clone().with_cross_check(CrossCheck::new(
            0.02,
            0.01,
            DEFAULT_CROSS_CHECK_TOLERANCE,
        ));
        assert!(p.validate(1920, 1080).iter().any(|e| matches!(
            e,
            ProfileError::UnusableMetricScale(MetricScaleError::CrossCheckFailed { .. })
        )));
    }

    #[test]
    fn validate_reports_every_problem_at_once() {
        // 让 operator 一次改完，而不是改一条跑一次
        let mut p = valid();
        p.executable_sha256.clear();
        p.dsv.evidence_path.clear();
        p.readback_slots = 1;
        p.projection_b = 0.0;
        assert!(p.validate(1920, 1080).len() >= 4);
    }

    #[test]
    fn projection_round_trips_into_the_depth_module() {
        let p = valid();
        let proj = p.projection();
        // reversed-Z 无穷远：a=0, b=near；z = near/d × metric_scale
        let z = proj.linearize(10.0 / 500.0).unwrap();
        assert!((z - 10.0).abs() < 1e-9, "实得 {z}");
    }

    #[test]
    fn even_frame_convention_is_enforced_by_the_profile() {
        let p = valid();
        assert!(p.expects_depth(0));
        assert!(!p.expects_depth(1));
        assert!(p.expects_depth(12344));
    }

    #[test]
    fn serialises_round_trip() {
        let p = valid();
        let json = serde_json::to_string_pretty(&p).unwrap();
        let back: DepthProfile = serde_json::from_str(&json).unwrap();
        assert_eq!(p, back);
        // 车道与投影来源在 JSON 里是可读的 snake_case，不是数字
        assert!(json.contains("\"lane30\""));
        assert!(json.contains("\"engine_default\""));
        assert!(json.contains("\"copy_to_staging\""));
    }

    // --- 候选投影（#490 / 91 号）---

    /// 仓库里真实那份 Fallout 4 候选文件必须一直加载得了。
    /// **这个测试的价值在于它读的是发出去的那份文件**，不是夹具——
    /// 字段改名、schema 号改动都会在这里当场暴露。
    #[test]
    fn the_shipped_fallout4_candidate_file_stays_loadable() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../config/depth-profiles/fallout4-projection-candidate.json");
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("读 {} 失败: {e}", path.display()));
        let c: CandidateProjection = serde_json::from_str(&text).expect("候选文件解析失败");
        c.validate().expect("候选文件自校验失败");
        assert_eq!(c.game_id, "fallout-4");
        // 传统 Z ⇒ b 为负；这是 91 号取证的结论，改了要有人解释
        assert!(c.projection_b < 0.0);
        assert_eq!(
            c.projection().z_convention(),
            crate::decode::ZConvention::Traditional
        );
        assert!(
            !c.blocking.is_empty(),
            "它仍然缺锚点交叉验证，blocking 不该被清空"
        );
    }

    /// blocking 为空的候选是自相矛盾的：什么都不缺就该做成正式 profile。
    /// 放它过去会让「未验证」这件事在索引里消失。
    #[test]
    fn a_candidate_with_nothing_blocking_is_refused() {
        let c = CandidateProjection {
            schema_version: CandidateProjection::SCHEMA_VERSION.into(),
            game_id: "x".into(),
            projection_a: 1.0,
            projection_b: -15.0,
            metric_scale: 0.0142875,
            blocking: vec![],
        };
        assert!(c.validate().unwrap_err().contains("blocking"));
    }

    #[test]
    fn a_candidate_must_declare_its_own_schema() {
        let c = CandidateProjection {
            // 冒充正式 profile 的 schema 号
            schema_version: DepthProfile::SCHEMA_VERSION.into(),
            game_id: "x".into(),
            projection_a: 1.0,
            projection_b: -15.0,
            metric_scale: 0.0142875,
            blocking: vec!["缺标定".into()],
        };
        assert!(c.validate().unwrap_err().contains("schema_version"));
    }

    #[test]
    fn a_degenerate_candidate_is_refused_like_a_profile_would_be() {
        let base = CandidateProjection {
            schema_version: CandidateProjection::SCHEMA_VERSION.into(),
            game_id: "x".into(),
            projection_a: 1.0,
            projection_b: -15.0,
            metric_scale: 0.0142875,
            blocking: vec!["缺标定".into()],
        };
        let mut b0 = base.clone();
        b0.projection_b = 0.0;
        assert!(b0.validate().is_err(), "b=0 会让 z 恒为 0");
        let mut neg = base.clone();
        neg.metric_scale = -1.0;
        assert!(neg.validate().is_err(), "负 metric_scale 不能冒充标定");
        let mut nan = base;
        nan.projection_a = f64::NAN;
        assert!(nan.validate().is_err());
    }
}

//! 生产 Camera binding 与逐 Present pose 合成。
//!
//! discovery 的 `buffer_tag` 是进程内 COM 指针，跨游戏重启不稳定，因此 production
//! binding 不能钉它。生产来源钉的是 shader stage / bind slot / 主场景 pass ordinal /
//! draw ordinal / 常量缓冲字节数 / offset / 编码与矩阵布局。resource tag 只在同一个
//! Present + pass + draw 窗口内帮助判重，跨帧 ring-buffer 轮换是正常行为。

use serde::{Deserialize, Serialize};

use crate::camera::{
    Intrinsics, Mat4, MatrixKind, ORTHONORMAL_TOLERANCE, TranslationSlot, classify,
    intrinsics_from_projection, mat3_to_quat, pose_from_view,
};
use crate::camera_action::{
    CAMERA_POSE_INDEX_SCHEMA_V1, CameraPoseProvenance, CameraPoseRow, CameraPoseSource,
    StreamIdentity,
};
use crate::camera_track::projection_from_matrix;

pub const CAMERA_BINDING_SCHEMA_V1: &str = "camera-binding/v1";
pub const CAMERA_BINDING_SCHEMA_V2: &str = "camera-binding/v2";
pub const CAMERA_BINDING_SCHEMA_V3: &str = "camera-binding/cp2077-holstered-v3";
pub const KCD2_COMBINED_CAMERA_BINDING_SCHEMA_V1: &str = "camera-binding/kcd2-combined-v1";
pub const KCD2_COMBINED_CAMERA_BINDING_SCHEMA_V2: &str = "camera-binding/kcd2-combined-v2";
pub const CAMERA_POSE_DIAGNOSTIC_SCHEMA_V1: &str = "camera-pose-diagnostic/v1";

/// 没有 CameraPoseRow 的帧必须给出观测层原因；它不猜测具体游戏 UI/过场状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CameraPoseDiagnosticReason {
    NoMainSceneDepth,
    BindingSampleMissing,
    BindingAmbiguity,
    MatrixInvalid,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CameraPoseDiagnosticRow {
    pub schema: String,
    pub identity: StreamIdentity,
    pub present_seq: u64,
    pub reason: CameraPoseDiagnosticReason,
}

impl CameraPoseDiagnosticRow {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.schema != CAMERA_POSE_DIAGNOSTIC_SCHEMA_V1 {
            return Err("unsupported camera pose diagnostic schema");
        }
        if self.identity.session_id.trim().is_empty() || self.identity.run_id.trim().is_empty() {
            return Err("camera diagnostic stream identity is empty");
        }
        Ok(())
    }
}

/// 跨进程可复现的常量缓冲矩阵来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MatrixSourceBinding {
    pub shader_stage: ShaderStage,
    pub bind_slot: u32,
    pub pass_ordinal: u32,
    pub draw_ordinal: u32,
    pub buffer_bytes: u32,
    pub offset: u32,
    pub encoding: MatrixEncoding,
    pub layout: MatrixLayout,
    pub transpose: bool,
    pub role: MatrixRole,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShaderStage {
    Vertex,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatrixEncoding {
    F32LittleEndian,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatrixLayout {
    RowMajor,
    ColumnMajor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatrixRole {
    View,
    Projection,
}

/// view 来源还必须钉住平移布局，禁止每帧重新猜 row/column 约定。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewSourceBinding {
    #[serde(flatten)]
    pub source: MatrixSourceBinding,
    pub translation: TranslationSlot,
}

/// Stable D3D12 descriptor-table identity.  It deliberately contains no COM pointer, descriptor
/// handle, resource tag, process-local source tag, command-list generation or draw ordinal.
/// Those values are useful while debugging one launch, but none survives a fresh process.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct D3d12DescriptorTableBinding {
    pub root_signature_sha256: String,
    pub root_parameter_index: u32,
    pub descriptor_range_index: u32,
    pub descriptor_offset_in_range: u32,
    pub descriptor_count: u32,
    pub base_shader_register: u32,
    pub register_space: u32,
}

/// Semantic draw relation used by D3D12 camera evidence.
///
/// A numeric draw ordinal is intentionally not representable: CP2077 repeats the same camera CBV
/// on several selected-depth draws and the first observed ordinal changes between launches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum D3d12DrawRelation {
    ExactSelectedDepthDsvDraw,
}

/// Cross-launch D3D12 matrix source.  `cbv_resource_offset` is relative to the descriptor's
/// backing resource rather than a GPU virtual address. `declared_cbv_bytes` comes from the CBV
/// descriptor and therefore bounds every matrix read.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct D3d12MatrixSourceBinding {
    pub descriptor_table: D3d12DescriptorTableBinding,
    pub cbv_resource_offset: u64,
    pub declared_cbv_bytes: u32,
    pub matrix_offset: u32,
    pub encoding: MatrixEncoding,
    pub layout: MatrixLayout,
    pub transpose: bool,
    pub role: MatrixRole,
    pub draw_relation: D3d12DrawRelation,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct D3d12ViewSourceBinding {
    #[serde(flatten)]
    pub source: D3d12MatrixSourceBinding,
    pub translation: TranslationSlot,
}

/// One signed REDengine source axis. A D3D12 authoring candidate must spell out the complete
/// signed permutation instead of hiding it behind a free-form calibration label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum D3d12SignedAxis {
    PositiveX,
    NegativeX,
    PositiveY,
    NegativeY,
    PositiveZ,
    NegativeZ,
}

impl D3d12SignedAxis {
    const fn source_axis(self) -> usize {
        match self {
            Self::PositiveX | Self::NegativeX => 0,
            Self::PositiveY | Self::NegativeY => 1,
            Self::PositiveZ | Self::NegativeZ => 2,
        }
    }

    const fn sign(self) -> f64 {
        match self {
            Self::PositiveX | Self::PositiveY | Self::PositiveZ => 1.0,
            Self::NegativeX | Self::NegativeY | Self::NegativeZ => -1.0,
        }
    }
}

/// DataMiner delivery X/Y/Z expressed as a signed permutation of source X/Y/Z.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct D3d12AxisMapping {
    pub delivery_x_from: D3d12SignedAxis,
    pub delivery_y_from: D3d12SignedAxis,
    pub delivery_z_from: D3d12SignedAxis,
}

impl D3d12AxisMapping {
    pub fn validate(self) -> Result<(), D3d12CameraBindingError> {
        let mut axes = [
            self.delivery_x_from.source_axis(),
            self.delivery_y_from.source_axis(),
            self.delivery_z_from.source_axis(),
        ];
        axes.sort_unstable();
        if axes != [0, 1, 2] {
            return Err(D3d12CameraBindingError::InvalidAxisMapping);
        }
        Ok(())
    }

    #[must_use]
    pub fn source_to_delivery(self, source: [f64; 3]) -> [f64; 3] {
        [
            self.delivery_x_from.sign() * source[self.delivery_x_from.source_axis()],
            self.delivery_y_from.sign() * source[self.delivery_y_from.source_axis()],
            self.delivery_z_from.sign() * source[self.delivery_z_from.source_axis()],
        ]
    }
}

/// D3D12 CameraBinding contract.  This is versioned independently from the runtime v1 resolver so
/// old D3D11/Fallout catalog entries continue to deserialize byte-for-byte.  Authoring may emit a
/// v2 candidate only after two independent launches agree on these stable fields; this type does
/// not itself authorize catalog publication.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct D3d12CameraBinding {
    pub schema: String,
    pub binding_id: String,
    pub binding_version: String,
    pub profile_id: String,
    pub profile_version: String,
    pub executable_sha256: String,
    pub render_width: u32,
    pub render_height: u32,
    pub view_metres_per_unit: String,
    pub axis_mapping: D3d12AxisMapping,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub holstered_camera: Option<Cp2077HolsteredCamera>,
    pub view_sources: Vec<D3d12ViewSourceBinding>,
    pub projection_sources: Vec<D3d12MatrixSourceBinding>,
}

/// Explicit native-camera -> delivery-camera basis, independently of the world basis.
/// A reflected native view cannot be converted by conjugating both sides with one basis.
/// The policy schema also fixes source decoding: v1 reads a native V, while
/// `cp2077-bound-holstered-row-vp/v2` reads VP at the view slot and recovers
/// V = VP * inverse(P) from its paired same-CBV projection. No runtime guessing.
/// Publication and per-launch physical source admission remain separate gates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cp2077HolsteredCamera {
    pub schema: String,
    pub camera_axis_mapping: D3d12AxisMapping,
}

/// Exact-build KCD2 camera contract. The game exposes one combined
/// view-projection direct root-CBV rather than independently addressable view
/// and projection matrices, so forcing it into `D3d12CameraBinding` would invent
/// source identity. The embedded plan remains build-pinned and same-Present.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Kcd2CombinedCameraBinding {
    pub schema: String,
    pub binding_id: String,
    pub binding_version: String,
    pub profile_id: String,
    pub profile_version: String,
    pub executable_sha256: String,
    pub render_width: u32,
    pub render_height: u32,
    pub view_metres_per_unit: String,
    pub axis_mapping: D3d12AxisMapping,
    pub live_pose_plan: crate::Kcd2LivePosePlan,
}

/// Versioned production input. Backend selection is explicit and checked against DepthProfile;
/// a v2 or KCD2 payload never falls through to the D3D11 resolver.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CameraBindingDocument {
    V1(CameraBinding),
    V2(D3d12CameraBinding),
    Kcd2Combined(Kcd2CombinedCameraBinding),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum D3d12CameraBindingError {
    UnsupportedSchema {
        actual: String,
    },
    EmptyField {
        field: &'static str,
    },
    InvalidExecutableHash {
        actual: String,
    },
    ExecutableHashMismatch {
        expected: String,
        actual: String,
    },
    InvalidRenderSize {
        width: u32,
        height: u32,
    },
    InvalidMetricScale,
    InvalidAxisMapping,
    ViewSourceCount {
        count: usize,
    },
    ProjectionSourceCount {
        count: usize,
    },
    InvalidMatrixSource {
        kind: &'static str,
        reason: &'static str,
    },
    ViewProjectionSourceMismatch,
    SameViewAndProjectionOffset,
}

impl std::fmt::Display for D3d12CameraBindingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "D3D12 CameraBinding 不可用于 authoring: {self:?}")
    }
}

impl std::error::Error for D3d12CameraBindingError {}

/// per-build、per-profile 的生产相机绑定。
///
/// sources 用数组而不是单值，是为了让 authoring 产物能显式携带尚未裁决的候选；
/// production 解析时要求两边都**恰好一项**，不会悄悄取第一项。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CameraBinding {
    pub schema: String,
    pub binding_id: String,
    pub binding_version: String,
    pub profile_id: String,
    pub profile_version: String,
    pub executable_sha256: String,
    pub render_width: u32,
    pub render_height: u32,
    /// Metric scale for positions recovered from a GPU view matrix. Game-telemetry bindings carry
    /// their scale in [`GameCameraBinding`] instead and must leave this absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view_metres_per_unit: Option<String>,
    #[serde(default)]
    pub game_camera: Option<GameCameraBinding>,
    pub view_sources: Vec<ViewSourceBinding>,
    pub projection_sources: Vec<MatrixSourceBinding>,
}

/// Exact-build game camera source used instead of a GPU view-matrix guess.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GameCameraBinding {
    pub provider: String,
    pub camera_singleton_relocation: u64,
    pub camera_root_offset: u32,
    pub world_transform_offset: u32,
    pub metres_per_game_unit: String,
    pub axis_mapping: String,
}

impl GameCameraBinding {
    /// Validate an exact-build comparison source through the same checks used by a production
    /// [`CameraBinding`], without granting it production authority.
    pub fn validate(&self) -> Result<(), CameraBindingError> {
        validate_game_camera(self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CameraBindingError {
    UnsupportedSchema {
        actual: String,
    },
    EmptyField {
        field: &'static str,
    },
    InvalidExecutableHash {
        actual: String,
    },
    ExecutableHashMismatch {
        expected: String,
        actual: String,
    },
    ViewSourceCount {
        count: usize,
    },
    ConflictingCameraSources,
    InvalidGameCamera {
        reason: &'static str,
    },
    ProjectionSourceCount {
        count: usize,
    },
    InvalidRenderSize {
        width: u32,
        height: u32,
    },
    InvalidMatrixSource {
        kind: &'static str,
        reason: &'static str,
    },
    SameViewAndProjectionSource,
    ViewProjectionWindowMismatch,
}

impl std::fmt::Display for CameraBindingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "CameraBinding 不可用于生产: {self:?}")
    }
}

impl std::error::Error for CameraBindingError {}

#[derive(Debug, Clone, PartialEq)]
struct ResolvedCameraBinding {
    binding_id: String,
    binding_version: String,
    profile_id: String,
    profile_version: String,
    executable_sha256: String,
    render_width: u32,
    render_height: u32,
    view_metres_per_unit: Option<(String, f64)>,
    game_camera: Option<GameCameraBinding>,
    view: Option<ViewSourceBinding>,
    projection: MatrixSourceBinding,
}

impl CameraBinding {
    pub fn read(path: &std::path::Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("读 CameraBinding {} 失败: {e}", path.display()))?;
        serde_json::from_str(&text)
            .map_err(|e| format!("解析 CameraBinding {} 失败: {e}", path.display()))
    }

    fn resolve(
        &self,
        actual_executable_sha256: &str,
    ) -> Result<ResolvedCameraBinding, CameraBindingError> {
        if self.schema != CAMERA_BINDING_SCHEMA_V1 {
            return Err(CameraBindingError::UnsupportedSchema {
                actual: self.schema.clone(),
            });
        }
        for (field, value) in [
            ("binding_id", self.binding_id.as_str()),
            ("binding_version", self.binding_version.as_str()),
            ("profile_id", self.profile_id.as_str()),
            ("profile_version", self.profile_version.as_str()),
        ] {
            if value.trim().is_empty() {
                return Err(CameraBindingError::EmptyField { field });
            }
        }
        if !is_sha256_digest(&self.executable_sha256) {
            return Err(CameraBindingError::InvalidExecutableHash {
                actual: self.executable_sha256.clone(),
            });
        }
        if self.executable_sha256 != actual_executable_sha256 {
            return Err(CameraBindingError::ExecutableHashMismatch {
                expected: self.executable_sha256.clone(),
                actual: actual_executable_sha256.to_string(),
            });
        }
        if self.game_camera.is_some() && self.view_metres_per_unit.is_some() {
            return Err(CameraBindingError::ConflictingCameraSources);
        }
        let valid_view_count = if self.game_camera.is_some() {
            self.view_sources.len() <= 1
        } else {
            self.view_sources.len() == 1
        };
        if !valid_view_count {
            return Err(CameraBindingError::ViewSourceCount {
                count: self.view_sources.len(),
            });
        }
        if let Some(game_camera) = &self.game_camera {
            validate_game_camera(game_camera)?;
        }
        let view_metres_per_unit = match (&self.view_metres_per_unit, self.game_camera.is_none()) {
            (Some(raw), true) => {
                let scale = parse_positive_scale(raw).map_err(|reason| {
                    CameraBindingError::InvalidMatrixSource {
                        kind: "view",
                        reason,
                    }
                })?;
                Some((raw.clone(), scale))
            }
            (None, true) => {
                return Err(CameraBindingError::InvalidMatrixSource {
                    kind: "view",
                    reason: "view_metres_per_unit is missing",
                });
            }
            (None, false) => None,
            (Some(_), false) => unreachable!("conflicting sources were rejected above"),
        };
        if self.projection_sources.len() != 1 {
            return Err(CameraBindingError::ProjectionSourceCount {
                count: self.projection_sources.len(),
            });
        }
        if self.render_width == 0 || self.render_height == 0 {
            return Err(CameraBindingError::InvalidRenderSize {
                width: self.render_width,
                height: self.render_height,
            });
        }
        let view = self.view_sources.first().copied();
        let projection = self.projection_sources[0];
        if let Some(view) = view {
            validate_source("view", MatrixRole::View, view.source)?;
        }
        validate_source("projection", MatrixRole::Projection, projection)?;
        if let Some(view) = view {
            if same_physical_source(view.source, projection) {
                return Err(CameraBindingError::SameViewAndProjectionSource);
            }
            if view.source.pass_ordinal != projection.pass_ordinal
                || view.source.draw_ordinal != projection.draw_ordinal
            {
                return Err(CameraBindingError::ViewProjectionWindowMismatch);
            }
        }
        Ok(ResolvedCameraBinding {
            binding_id: self.binding_id.clone(),
            binding_version: self.binding_version.clone(),
            profile_id: self.profile_id.clone(),
            profile_version: self.profile_version.clone(),
            executable_sha256: actual_executable_sha256.to_string(),
            render_width: self.render_width,
            render_height: self.render_height,
            view_metres_per_unit,
            game_camera: self.game_camera.clone(),
            view,
            projection,
        })
    }

    /// Validate the authored binding through the same production resolver used
    /// by the pose assembler, without constructing a capture stream.
    pub fn validate_for_executable(
        &self,
        actual_executable_sha256: &str,
    ) -> Result<(), CameraBindingError> {
        self.resolve(actual_executable_sha256).map(|_| ())
    }
}

impl CameraBindingDocument {
    pub fn identity(&self) -> crate::probe_control::CameraBindingIdentity {
        match self {
            Self::V1(binding) => crate::probe_control::CameraBindingIdentity {
                schema: binding.schema.clone(),
                binding_id: binding.binding_id.clone(),
                binding_version: binding.binding_version.clone(),
                profile_id: binding.profile_id.clone(),
                profile_version: binding.profile_version.clone(),
                executable_sha256: binding.executable_sha256.clone(),
            },
            Self::V2(binding) => crate::probe_control::CameraBindingIdentity {
                schema: binding.schema.clone(),
                binding_id: binding.binding_id.clone(),
                binding_version: binding.binding_version.clone(),
                profile_id: binding.profile_id.clone(),
                profile_version: binding.profile_version.clone(),
                executable_sha256: binding.executable_sha256.clone(),
            },
            Self::Kcd2Combined(binding) => crate::probe_control::CameraBindingIdentity {
                schema: binding.schema.clone(),
                binding_id: binding.binding_id.clone(),
                binding_version: binding.binding_version.clone(),
                profile_id: binding.profile_id.clone(),
                profile_version: binding.profile_version.clone(),
                executable_sha256: binding.executable_sha256.clone(),
            },
        }
    }

    pub fn render_size(&self) -> (u32, u32) {
        match self {
            Self::V1(binding) => (binding.render_width, binding.render_height),
            Self::V2(binding) => (binding.render_width, binding.render_height),
            Self::Kcd2Combined(binding) => (binding.render_width, binding.render_height),
        }
    }

    /// Shared by catalog loading, client preflight and the receiving probe. This validates
    /// content, not publication authority: reviewed catalog and performance gates remain separate.
    pub fn validate_for_profile(
        &self,
        profile: &crate::DepthProfile,
        actual_executable_sha256: &str,
    ) -> Result<(), String> {
        let expected_backend = match self {
            Self::V1(_) => crate::RendererBackend::D3d11,
            Self::V2(_) => crate::RendererBackend::D3d12,
            Self::Kcd2Combined(_) => crate::RendererBackend::D3d12,
        };
        if profile.renderer_backend != expected_backend {
            return Err("CameraBinding schema and depth renderer backend disagree".into());
        }
        let identity = self.identity();
        if identity.profile_id != profile.game_id
            || identity.profile_version != profile.schema_version
            || identity.executable_sha256 != profile.executable_sha256
        {
            return Err("CameraBinding and DepthProfile identities disagree".into());
        }
        let (width, height) = self.render_size();
        if width != profile.dsv.expected_width || height != profile.dsv.expected_height {
            return Err("CameraBinding and depth resource render geometry disagree".into());
        }
        if matches!(self, Self::V2(_) | Self::Kcd2Combined(_))
            && profile.projection_source != crate::profile::ProjectionSource::ConstantBuffer
        {
            return Err(
                "D3D12 camera requires observed constant-buffer projection evidence".into(),
            );
        }
        let errors = profile.validate(width, height);
        if !errors.is_empty() {
            return Err(format!("depth profile failed validation: {errors:?}"));
        }
        self.validate_for_executable(actual_executable_sha256)
    }

    pub fn read(path: &std::path::Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|error| format!("读 CameraBinding {} 失败: {error}", path.display()))?;
        serde_json::from_str(&text)
            .map_err(|error| format!("解析 CameraBinding {} 失败: {error}", path.display()))
    }

    pub fn validate_for_executable(&self, actual_executable_sha256: &str) -> Result<(), String> {
        match self {
            Self::V1(binding) => binding
                .validate_for_executable(actual_executable_sha256)
                .map_err(|error| error.to_string()),
            Self::V2(binding) => binding
                .validate_for_executable(actual_executable_sha256)
                .map_err(|error| error.to_string()),
            Self::Kcd2Combined(binding) => {
                binding.validate_for_executable(actual_executable_sha256)
            }
        }
    }

    #[must_use]
    pub fn schema(&self) -> &str {
        match self {
            Self::V1(binding) => &binding.schema,
            Self::V2(binding) => &binding.schema,
            Self::Kcd2Combined(binding) => &binding.schema,
        }
    }

    #[must_use]
    pub fn binding_id(&self) -> &str {
        match self {
            Self::V1(binding) => &binding.binding_id,
            Self::V2(binding) => &binding.binding_id,
            Self::Kcd2Combined(binding) => &binding.binding_id,
        }
    }

    #[must_use]
    pub fn binding_version(&self) -> &str {
        match self {
            Self::V1(binding) => &binding.binding_version,
            Self::V2(binding) => &binding.binding_version,
            Self::Kcd2Combined(binding) => &binding.binding_version,
        }
    }

    #[must_use]
    pub fn profile_id(&self) -> &str {
        match self {
            Self::V1(binding) => &binding.profile_id,
            Self::V2(binding) => &binding.profile_id,
            Self::Kcd2Combined(binding) => &binding.profile_id,
        }
    }

    #[must_use]
    pub fn profile_version(&self) -> &str {
        match self {
            Self::V1(binding) => &binding.profile_version,
            Self::V2(binding) => &binding.profile_version,
            Self::Kcd2Combined(binding) => &binding.profile_version,
        }
    }

    #[must_use]
    pub fn executable_sha256(&self) -> &str {
        match self {
            Self::V1(binding) => &binding.executable_sha256,
            Self::V2(binding) => &binding.executable_sha256,
            Self::Kcd2Combined(binding) => &binding.executable_sha256,
        }
    }

    #[must_use]
    pub fn render_dimensions(&self) -> (u32, u32) {
        match self {
            Self::V1(binding) => (binding.render_width, binding.render_height),
            Self::V2(binding) => (binding.render_width, binding.render_height),
            Self::Kcd2Combined(binding) => (binding.render_width, binding.render_height),
        }
    }

    /// Whether the current probe has an explicit runtime for this document.
    #[must_use]
    pub const fn runtime_supported(&self) -> bool {
        matches!(self, Self::V1(_) | Self::V2(_) | Self::Kcd2Combined(_))
    }
}

impl Kcd2CombinedCameraBinding {
    pub fn validate_for_executable(&self, actual_executable_sha256: &str) -> Result<(), String> {
        let version_matches_plan = match self.schema.as_str() {
            KCD2_COMBINED_CAMERA_BINDING_SCHEMA_V1 => {
                self.binding_id == crate::KCD2_COMBINED_BINDING_ID
                    && self.binding_version == crate::KCD2_COMBINED_BINDING_VERSION
                    && matches!(
                        self.live_pose_plan.schema.as_str(),
                        crate::KCD2_LIVE_POSE_PLAN_SCHEMA_V1 | crate::KCD2_LIVE_POSE_PLAN_SCHEMA_V2
                    )
            }
            KCD2_COMBINED_CAMERA_BINDING_SCHEMA_V2 => {
                self.binding_id == crate::KCD2_COMBINED_BINDING_ID_V2
                    && self.binding_version == crate::KCD2_COMBINED_BINDING_VERSION_V2
                    && self.live_pose_plan.schema == crate::KCD2_LIVE_POSE_PLAN_SCHEMA_V3
            }
            _ => false,
        };
        if !version_matches_plan {
            return Err(
                "KCD2 combined CameraBinding schema, identity and source plan disagree".into(),
            );
        }
        for (field, value) in [
            ("binding_id", self.binding_id.as_str()),
            ("binding_version", self.binding_version.as_str()),
            ("profile_id", self.profile_id.as_str()),
            ("profile_version", self.profile_version.as_str()),
        ] {
            if value.trim().is_empty() {
                return Err(format!(
                    "KCD2 combined CameraBinding field {field} is empty"
                ));
            }
        }
        if self.profile_id != crate::KCD2_MAINLINE_GAME_ID
            || self.profile_version != crate::DepthProfile::D3D12_STABLE_SCHEMA_VERSION
            || self.executable_sha256 != crate::KCD2_EXECUTABLE_SHA256
            || !self
                .executable_sha256
                .eq_ignore_ascii_case(actual_executable_sha256)
            || self.render_width != 1920
            || self.render_height != 1080
            || parse_positive_scale(&self.view_metres_per_unit).ok() != Some(1.0)
        {
            return Err(
                "KCD2 combined CameraBinding build/profile/geometry/metric identity mismatch"
                    .into(),
            );
        }
        self.axis_mapping
            .validate()
            .map_err(|error| error.to_string())?;
        if self.axis_mapping.delivery_x_from != D3d12SignedAxis::PositiveX
            || self.axis_mapping.delivery_y_from != D3d12SignedAxis::PositiveZ
            || self.axis_mapping.delivery_z_from != D3d12SignedAxis::PositiveY
        {
            return Err("KCD2 combined CameraBinding axis mapping drifted".into());
        }
        self.live_pose_plan
            .validate(actual_executable_sha256)
            .map_err(|error| format!("KCD2 combined CameraBinding plan failed: {error}"))
    }
}

impl D3d12CameraBinding {
    pub fn validate_for_executable(
        &self,
        actual_executable_sha256: &str,
    ) -> Result<(), D3d12CameraBindingError> {
        if self.schema != CAMERA_BINDING_SCHEMA_V2 && self.schema != CAMERA_BINDING_SCHEMA_V3 {
            return Err(D3d12CameraBindingError::UnsupportedSchema {
                actual: self.schema.clone(),
            });
        }
        for (field, value) in [
            ("binding_id", self.binding_id.as_str()),
            ("binding_version", self.binding_version.as_str()),
            ("profile_id", self.profile_id.as_str()),
            ("profile_version", self.profile_version.as_str()),
        ] {
            if value.trim().is_empty() {
                return Err(D3d12CameraBindingError::EmptyField { field });
            }
        }
        if !is_sha256_digest(&self.executable_sha256) {
            return Err(D3d12CameraBindingError::InvalidExecutableHash {
                actual: self.executable_sha256.clone(),
            });
        }
        if self.executable_sha256 != actual_executable_sha256 {
            return Err(D3d12CameraBindingError::ExecutableHashMismatch {
                expected: self.executable_sha256.clone(),
                actual: actual_executable_sha256.to_owned(),
            });
        }
        if self.render_width == 0 || self.render_height == 0 {
            return Err(D3d12CameraBindingError::InvalidRenderSize {
                width: self.render_width,
                height: self.render_height,
            });
        }
        if parse_positive_scale(&self.view_metres_per_unit).is_err() {
            return Err(D3d12CameraBindingError::InvalidMetricScale);
        }
        self.axis_mapping.validate()?;
        if self.schema == CAMERA_BINDING_SCHEMA_V3 {
            return self.validate_holstered_sources();
        }
        if self.holstered_camera.is_some() {
            return Err(D3d12CameraBindingError::InvalidMatrixSource {
                kind: "view",
                reason: "v2 cannot carry holstered v3 semantics",
            });
        }
        if self.view_sources.len() != 1 {
            return Err(D3d12CameraBindingError::ViewSourceCount {
                count: self.view_sources.len(),
            });
        }
        if self.projection_sources.len() != 1 {
            return Err(D3d12CameraBindingError::ProjectionSourceCount {
                count: self.projection_sources.len(),
            });
        }
        let view = &self.view_sources[0].source;
        let projection = &self.projection_sources[0];
        validate_d3d12_source("view", MatrixRole::View, view)?;
        validate_d3d12_source("projection", MatrixRole::Projection, projection)?;
        if view.descriptor_table != projection.descriptor_table
            || view.cbv_resource_offset != projection.cbv_resource_offset
            || view.declared_cbv_bytes != projection.declared_cbv_bytes
            || view.draw_relation != projection.draw_relation
        {
            return Err(D3d12CameraBindingError::ViewProjectionSourceMismatch);
        }
        if view.matrix_offset == projection.matrix_offset {
            return Err(D3d12CameraBindingError::SameViewAndProjectionOffset);
        }
        Ok(())
    }
}

impl D3d12MatrixSourceBinding {
    /// Validate one standalone observation before it is used as a cross-launch grouping key.
    /// Full CameraBinding validation still has to compare the selected view and projection.
    pub fn validate_for_role(
        &self,
        expected_role: MatrixRole,
    ) -> Result<(), D3d12CameraBindingError> {
        let kind = match expected_role {
            MatrixRole::View => "view",
            MatrixRole::Projection => "projection",
        };
        validate_d3d12_source(kind, expected_role, self)
    }
}

fn validate_d3d12_source(
    kind: &'static str,
    expected_role: MatrixRole,
    source: &D3d12MatrixSourceBinding,
) -> Result<(), D3d12CameraBindingError> {
    if source.role != expected_role {
        return Err(D3d12CameraBindingError::InvalidMatrixSource {
            kind,
            reason: "matrix role does not match its binding list",
        });
    }
    let descriptor = &source.descriptor_table;
    if !is_sha256_digest(&descriptor.root_signature_sha256) {
        return Err(D3d12CameraBindingError::InvalidMatrixSource {
            kind,
            reason: "root signature fingerprint is not a SHA-256 digest",
        });
    }
    if descriptor.descriptor_count == 0
        || descriptor.descriptor_offset_in_range >= descriptor.descriptor_count
    {
        return Err(D3d12CameraBindingError::InvalidMatrixSource {
            kind,
            reason: "descriptor offset is outside its declared CBV range",
        });
    }
    if source.declared_cbv_bytes < 64 || !source.declared_cbv_bytes.is_multiple_of(256) {
        return Err(D3d12CameraBindingError::InvalidMatrixSource {
            kind,
            reason: "declared D3D12 CBV bytes are invalid",
        });
    }
    if !source.cbv_resource_offset.is_multiple_of(256) {
        return Err(D3d12CameraBindingError::InvalidMatrixSource {
            kind,
            reason: "CBV resource offset is not 256-byte aligned",
        });
    }
    if !source.matrix_offset.is_multiple_of(16) {
        return Err(D3d12CameraBindingError::InvalidMatrixSource {
            kind,
            reason: "matrix offset is not one 16-byte register aligned",
        });
    }
    if source
        .matrix_offset
        .checked_add(64)
        .is_none_or(|end| end > source.declared_cbv_bytes)
    {
        return Err(D3d12CameraBindingError::InvalidMatrixSource {
            kind,
            reason: "matrix exceeds the declared D3D12 CBV",
        });
    }
    Ok(())
}

fn validate_game_camera(binding: &GameCameraBinding) -> Result<(), CameraBindingError> {
    if binding.provider.trim().is_empty() {
        return Err(CameraBindingError::InvalidGameCamera {
            reason: "provider is empty",
        });
    }
    if binding.camera_singleton_relocation == 0
        || binding.camera_root_offset == 0
        || binding.world_transform_offset == 0
    {
        return Err(CameraBindingError::InvalidGameCamera {
            reason: "an exact-build object offset is zero",
        });
    }
    let scale = binding.metres_per_game_unit.parse::<f64>().map_err(|_| {
        CameraBindingError::InvalidGameCamera {
            reason: "metres_per_game_unit is not numeric",
        }
    })?;
    if !scale.is_finite() || scale <= 0.0 || binding.axis_mapping.trim().is_empty() {
        return Err(CameraBindingError::InvalidGameCamera {
            reason: "metric scale or axis mapping is invalid",
        });
    }
    Ok(())
}

fn parse_positive_scale(raw: &str) -> Result<f64, &'static str> {
    let scale = raw
        .parse::<f64>()
        .map_err(|_| "metric scale is not numeric")?;
    if !scale.is_finite() || scale <= 0.0 {
        return Err("metric scale is not positive and finite");
    }
    Ok(scale)
}

fn same_physical_source(view: MatrixSourceBinding, projection: MatrixSourceBinding) -> bool {
    view.shader_stage == projection.shader_stage
        && view.bind_slot == projection.bind_slot
        && view.pass_ordinal == projection.pass_ordinal
        && view.draw_ordinal == projection.draw_ordinal
        && view.offset == projection.offset
}

fn validate_source(
    kind: &'static str,
    expected_role: MatrixRole,
    source: MatrixSourceBinding,
) -> Result<(), CameraBindingError> {
    if source.role != expected_role {
        return Err(CameraBindingError::InvalidMatrixSource {
            kind,
            reason: "matrix role does not match its binding list",
        });
    }
    if source.buffer_bytes < 64 {
        return Err(CameraBindingError::InvalidMatrixSource {
            kind,
            reason: "buffer is smaller than one 4x4 f32 matrix",
        });
    }
    if !source.offset.is_multiple_of(16) {
        return Err(CameraBindingError::InvalidMatrixSource {
            kind,
            reason: "offset is not one 16-byte constant-buffer register aligned",
        });
    }
    if source
        .offset
        .checked_add(64)
        .is_none_or(|end| end > source.buffer_bytes)
    {
        return Err(CameraBindingError::InvalidMatrixSource {
            kind,
            reason: "matrix exceeds the constant buffer",
        });
    }
    Ok(())
}

#[must_use]
pub fn is_sha256_digest(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    })
}

/// 本 run 的动态身份；静态 profile/binding/build 身份来自 [`CameraBinding`]。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CameraPoseStreamContext {
    pub identity: StreamIdentity,
    pub dsv_id: String,
}

/// 一个 draw 窗口里某 shader stage / slot 实际绑定的缓冲快照。
#[derive(Debug, Clone, Copy)]
pub struct BoundBufferAtDraw<'a> {
    pub shader_stage: ShaderStage,
    pub bind_slot: u32,
    /// 仅用于同窗冲突诊断；绝不写入 production binding 或 pose index。
    pub buffer_tag: u64,
    pub bytes: &'a [u8],
}

/// 从 hook 送进纯逻辑的主场景 draw 窗口。
#[derive(Debug, Clone, Copy)]
pub struct CameraDrawSample<'a> {
    pub present_seq: u64,
    pub sample_qpc: i64,
    pub pass_ordinal: u32,
    pub draw_ordinal: u32,
    pub bound_buffers: &'a [BoundBufferAtDraw<'a>],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CameraPoseAssemblyError {
    Binding(CameraBindingError),
    InvalidStreamIdentity {
        field: &'static str,
    },
    DuplicatePresentSeq {
        present_seq: u64,
    },
    PresentSeqOutOfOrder {
        previous: u64,
        current: u64,
    },
    PresentSeqGap {
        previous: u64,
        current: u64,
    },
    MissingPresentBoundary {
        active: u64,
        incoming: u64,
    },
    BoundaryMismatch {
        active: u64,
        boundary: u64,
    },
    MissingView {
        present_seq: u64,
    },
    MissingProjection {
        present_seq: u64,
    },
    AmbiguousMatrixSource {
        present_seq: u64,
        kind: &'static str,
        first_buffer_tag: u64,
        second_buffer_tag: u64,
    },
    MatrixChangedWithinPresent {
        present_seq: u64,
        kind: &'static str,
        buffer_tag: u64,
    },
    InvalidViewMatrix {
        present_seq: u64,
    },
    InvalidProjectionMatrix {
        present_seq: u64,
    },
    InvalidSampleQpc {
        present_seq: u64,
        sample_qpc: i64,
    },
}

impl std::fmt::Display for CameraPoseAssemblyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "逐帧 Camera pose 生产失败: {self:?}")
    }
}

impl std::error::Error for CameraPoseAssemblyError {}

impl From<CameraBindingError> for CameraPoseAssemblyError {
    fn from(value: CameraBindingError) -> Self {
        Self::Binding(value)
    }
}

#[derive(Debug, Clone, Copy)]
struct BoundMatrix {
    buffer_tag: u64,
    sample_qpc: i64,
    matrix: Mat4,
}

#[derive(Debug)]
struct PendingPose {
    present_seq: u64,
    view: Option<BoundMatrix>,
    projection: Option<BoundMatrix>,
}

/// 按 Present boundary 驱动的严格合成器。没有 boundary 的半帧永远不会落盘。
#[derive(Debug)]
pub struct CameraPoseAssembler {
    binding: ResolvedCameraBinding,
    context: CameraPoseStreamContext,
    pending: Option<PendingPose>,
    last_accounted: Option<u64>,
    /// Fallout 的位姿来自逐 Present 的 exact-build 内存采样；投影只需在正式 depth
    /// 证据帧从钉死的 draw source 复核。两次证据帧之间复用最近一次已验证投影，避免为
    /// 每个游戏 Present 重跑 D3D 常量缓冲深路径。
    held_game_projection: Option<Mat4>,
}

impl CameraPoseAssembler {
    pub fn new(
        binding: &CameraBinding,
        context: CameraPoseStreamContext,
        actual_executable_sha256: &str,
    ) -> Result<Self, CameraPoseAssemblyError> {
        for (field, value) in [
            ("session_id", context.identity.session_id.as_str()),
            ("run_id", context.identity.run_id.as_str()),
            ("dsv_id", context.dsv_id.as_str()),
        ] {
            if value.trim().is_empty() {
                return Err(CameraPoseAssemblyError::InvalidStreamIdentity { field });
            }
        }
        Ok(Self {
            binding: binding.resolve(actual_executable_sha256)?,
            context,
            pending: None,
            last_accounted: None,
            held_game_projection: None,
        })
    }

    pub fn observe_draw(
        &mut self,
        sample: CameraDrawSample<'_>,
    ) -> Result<(), CameraPoseAssemblyError> {
        if sample.sample_qpc <= 0 {
            return Err(CameraPoseAssemblyError::InvalidSampleQpc {
                present_seq: sample.present_seq,
                sample_qpc: sample.sample_qpc,
            });
        }
        self.ensure_pending(sample.present_seq)?;
        if sample.pass_ordinal != self.binding.projection.pass_ordinal
            || sample.draw_ordinal != self.binding.projection.draw_ordinal
        {
            return Ok(());
        }

        let projection_buffer = unique_bound_buffer(sample, self.binding.projection, "projection")?;
        let projection_matrix = read_matrix(self.binding.projection, projection_buffer.bytes)
            .ok_or(CameraPoseAssemblyError::InvalidProjectionMatrix {
                present_seq: sample.present_seq,
            })?;
        if !valid_projection_matrix(
            &projection_matrix,
            self.binding.render_width,
            self.binding.render_height,
        ) {
            return Err(CameraPoseAssemblyError::InvalidProjectionMatrix {
                present_seq: sample.present_seq,
            });
        }
        let pending =
            self.pending
                .as_mut()
                .ok_or(CameraPoseAssemblyError::MissingPresentBoundary {
                    active: sample.present_seq,
                    incoming: sample.present_seq,
                })?;
        if let Some(view) = self.binding.view {
            let view_buffer = unique_bound_buffer(sample, view.source, "view")?;
            let view_matrix = read_matrix(view.source, view_buffer.bytes).ok_or(
                CameraPoseAssemblyError::InvalidViewMatrix {
                    present_seq: sample.present_seq,
                },
            )?;
            let valid = if self.binding.game_camera.is_some() {
                fallout_render_rotation_from_basis(&view_matrix, view.translation).is_some()
            } else {
                valid_view_matrix(&view_matrix, view.translation)
            };
            if !valid {
                return Err(CameraPoseAssemblyError::InvalidViewMatrix {
                    present_seq: sample.present_seq,
                });
            }
            record_matrix(
                pending,
                "view",
                view_buffer.buffer_tag,
                sample.sample_qpc,
                view_matrix,
            )?;
        }
        record_matrix(
            pending,
            "projection",
            projection_buffer.buffer_tag,
            sample.sample_qpc,
            projection_matrix,
        )?;
        Ok(())
    }

    /// `Present(seq)` 到达时完成且只完成这一帧。
    pub fn finish_present(
        &mut self,
        present_seq: u64,
    ) -> Result<Option<CameraPoseRow>, CameraPoseAssemblyError> {
        let Some(pending) = self.pending.take() else {
            // hook 刚装上时可能先看到一个 Present boundary，再看到第一份常量缓冲。
            let Some(previous) = self.last_accounted else {
                return Ok(None);
            };
            if present_seq == previous {
                return Err(CameraPoseAssemblyError::DuplicatePresentSeq { present_seq });
            }
            if present_seq < previous {
                return Err(CameraPoseAssemblyError::PresentSeqOutOfOrder {
                    previous,
                    current: present_seq,
                });
            }
            if previous.checked_add(1) != Some(present_seq) {
                return Err(CameraPoseAssemblyError::PresentSeqGap {
                    previous,
                    current: present_seq,
                });
            }
            return Err(CameraPoseAssemblyError::MissingView { present_seq });
        };
        if pending.present_seq != present_seq {
            let active = pending.present_seq;
            self.pending = Some(pending);
            return Err(CameraPoseAssemblyError::BoundaryMismatch {
                active,
                boundary: present_seq,
            });
        }
        let view = pending
            .view
            .ok_or(CameraPoseAssemblyError::MissingView { present_seq })?;
        let projection = pending
            .projection
            .ok_or(CameraPoseAssemblyError::MissingProjection { present_seq })?;
        let view_binding = self
            .binding
            .view
            .ok_or(CameraPoseAssemblyError::MissingView { present_seq })?;
        if !valid_view_matrix(&view.matrix, view_binding.translation) {
            return Err(CameraPoseAssemblyError::InvalidViewMatrix { present_seq });
        }
        if !valid_projection_matrix(
            &projection.matrix,
            self.binding.render_width,
            self.binding.render_height,
        ) {
            return Err(CameraPoseAssemblyError::InvalidProjectionMatrix { present_seq });
        }

        let pose = pose_from_view(&view.matrix, view_binding.translation);
        let (view_scale_text, view_scale) = self
            .binding
            .view_metres_per_unit
            .as_ref()
            .ok_or(CameraPoseAssemblyError::MissingView { present_seq })?;
        let intrinsics = intrinsics_from_projection(
            &projection.matrix,
            self.binding.render_width as f32,
            self.binding.render_height as f32,
        );
        let row = CameraPoseRow {
            schema: CAMERA_POSE_INDEX_SCHEMA_V1.to_string(),
            identity: self.context.identity.clone(),
            present_seq,
            present_qpc: view.sample_qpc.max(projection.sample_qpc),
            source: CameraPoseSource::ResolvedMainCameraHook,
            provenance: CameraPoseProvenance::ResolvedMainCameraHook {
                binding_id: self.binding.binding_id.clone(),
                binding_version: self.binding.binding_version.clone(),
                profile_id: self.binding.profile_id.clone(),
                profile_version: self.binding.profile_version.clone(),
                executable_sha256: self.binding.executable_sha256.clone(),
                dsv_id: self.context.dsv_id.clone(),
                render_width: self.binding.render_width,
                render_height: self.binding.render_height,
                view_metres_per_unit: Some(view_scale_text.clone()),
                view_source: view_binding,
                projection_source: self.binding.projection,
                view_matrix: Box::new(view.matrix),
                projection_matrix: Box::new(projection.matrix),
            },
            camera_position: pose.position.map(|value| f64::from(value) * *view_scale),
            camera_rotation_quaternion: pose.rotation.map(f64::from),
            camera_intrinsics: intrinsics,
            camera_speed: None,
        };
        self.last_accounted = Some(present_seq);
        Ok(Some(row))
    }

    /// Finish a Present using an exact-build game camera transform and the GPU
    /// projection captured from the pinned main-scene draw.
    pub fn finish_present_game_telemetry(
        &mut self,
        present_seq: u64,
        present_qpc: i64,
        camera_position: [f64; 3],
        camera_rotation_quaternion: [f64; 4],
    ) -> Result<Option<CameraPoseRow>, CameraPoseAssemblyError> {
        if present_qpc <= 0 {
            return Err(CameraPoseAssemblyError::InvalidSampleQpc {
                present_seq,
                sample_qpc: present_qpc,
            });
        }
        let Some(pending) = self.pending.take() else {
            let Some(previous) = self.last_accounted else {
                return Ok(None);
            };
            if previous.checked_add(1) != Some(present_seq) {
                return Err(if present_seq <= previous {
                    CameraPoseAssemblyError::PresentSeqOutOfOrder {
                        previous,
                        current: present_seq,
                    }
                } else {
                    CameraPoseAssemblyError::PresentSeqGap {
                        previous,
                        current: present_seq,
                    }
                });
            }
            return Err(CameraPoseAssemblyError::MissingProjection { present_seq });
        };
        if pending.present_seq != present_seq {
            let active = pending.present_seq;
            self.pending = Some(pending);
            return Err(CameraPoseAssemblyError::BoundaryMismatch {
                active,
                boundary: present_seq,
            });
        }
        let render_basis = match self.binding.view {
            Some(_) => Some(
                pending
                    .view
                    .ok_or(CameraPoseAssemblyError::MissingView { present_seq })?
                    .matrix,
            ),
            None => None,
        };
        let projection = pending
            .projection
            .ok_or(CameraPoseAssemblyError::MissingProjection { present_seq })?;
        if !valid_projection_matrix(
            &projection.matrix,
            self.binding.render_width,
            self.binding.render_height,
        ) {
            return Err(CameraPoseAssemblyError::InvalidProjectionMatrix { present_seq });
        }
        self.held_game_projection = Some(projection.matrix);
        let row = self.game_telemetry_row(
            present_seq,
            present_qpc,
            camera_position,
            camera_rotation_quaternion,
            projection.matrix,
            render_basis,
        )?;
        self.last_accounted = Some(present_seq);
        Ok(Some(row))
    }

    /// 完成一个没有 depth 证据采样的 Fallout Present。
    ///
    /// 位姿仍来自本 Present 的 exact-build sampler；内参复用最近一个正式 depth 证据帧
    /// 从 production binding 唯一 draw 观测到的投影。证据帧继续走
    /// [`Self::finish_present_game_telemetry`]，因此新的投影会替换缓存，缺首份证据仍
    /// fail-closed，depth-index 的 `observed_this_frame` 语义也不会被这条复用路径冒充。
    pub fn finish_present_game_telemetry_with_held_projection(
        &mut self,
        present_seq: u64,
        present_qpc: i64,
        camera_position: [f64; 3],
        camera_rotation_quaternion: [f64; 4],
    ) -> Result<Option<CameraPoseRow>, CameraPoseAssemblyError> {
        if self.binding.view.is_some() {
            return Err(CameraPoseAssemblyError::MissingView { present_seq });
        }
        if present_qpc <= 0 {
            return Err(CameraPoseAssemblyError::InvalidSampleQpc {
                present_seq,
                sample_qpc: present_qpc,
            });
        }
        if let Some(active) = self.pending.as_ref().map(|pending| pending.present_seq) {
            return Err(CameraPoseAssemblyError::BoundaryMismatch {
                active,
                boundary: present_seq,
            });
        }
        self.validate_next_present(present_seq)?;
        let projection = self
            .held_game_projection
            .ok_or(CameraPoseAssemblyError::MissingProjection { present_seq })?;
        if !valid_projection_matrix(
            &projection,
            self.binding.render_width,
            self.binding.render_height,
        ) {
            return Err(CameraPoseAssemblyError::InvalidProjectionMatrix { present_seq });
        }
        let row = self.game_telemetry_row(
            present_seq,
            present_qpc,
            camera_position,
            camera_rotation_quaternion,
            projection,
            None,
        )?;
        self.last_accounted = Some(present_seq);
        Ok(Some(row))
    }

    fn game_telemetry_row(
        &self,
        present_seq: u64,
        present_qpc: i64,
        camera_position: [f64; 3],
        camera_rotation_quaternion: [f64; 4],
        projection: Mat4,
        render_basis: Option<Mat4>,
    ) -> Result<CameraPoseRow, CameraPoseAssemblyError> {
        let intrinsics = intrinsics_from_projection(
            &projection,
            self.binding.render_width as f32,
            self.binding.render_height as f32,
        );
        let game_camera = self
            .binding
            .game_camera
            .clone()
            .ok_or(CameraPoseAssemblyError::MissingView { present_seq })?;
        let (source, provenance, rotation) = if let Some(render_basis) = render_basis {
            let render_basis_source = self
                .binding
                .view
                .ok_or(CameraPoseAssemblyError::MissingView { present_seq })?;
            let rotation =
                fallout_render_rotation_from_basis(&render_basis, render_basis_source.translation)
                    .ok_or(CameraPoseAssemblyError::InvalidViewMatrix { present_seq })?
                    .map(f64::from);
            (
                CameraPoseSource::ResolvedRenderCameraHybrid,
                CameraPoseProvenance::ResolvedRenderCameraHybrid {
                    binding_id: self.binding.binding_id.clone(),
                    binding_version: self.binding.binding_version.clone(),
                    profile_id: self.binding.profile_id.clone(),
                    profile_version: self.binding.profile_version.clone(),
                    executable_sha256: self.binding.executable_sha256.clone(),
                    dsv_id: self.context.dsv_id.clone(),
                    render_width: self.binding.render_width,
                    render_height: self.binding.render_height,
                    game_camera,
                    render_basis_source,
                    render_basis_matrix: Box::new(render_basis),
                    projection_source: self.binding.projection,
                    projection_matrix: Box::new(projection),
                },
                rotation,
            )
        } else {
            (
                CameraPoseSource::GameTelemetry,
                CameraPoseProvenance::GameTelemetryMainCameraProjection {
                    binding_id: self.binding.binding_id.clone(),
                    binding_version: self.binding.binding_version.clone(),
                    profile_id: self.binding.profile_id.clone(),
                    profile_version: self.binding.profile_version.clone(),
                    executable_sha256: self.binding.executable_sha256.clone(),
                    dsv_id: self.context.dsv_id.clone(),
                    render_width: self.binding.render_width,
                    render_height: self.binding.render_height,
                    game_camera,
                    projection_source: self.binding.projection,
                    projection_matrix: Box::new(projection),
                },
                camera_rotation_quaternion,
            )
        };
        Ok(CameraPoseRow {
            schema: CAMERA_POSE_INDEX_SCHEMA_V1.to_string(),
            identity: self.context.identity.clone(),
            present_seq,
            present_qpc,
            source,
            provenance,
            camera_position,
            camera_rotation_quaternion: rotation,
            camera_intrinsics: intrinsics,
            camera_speed: None,
        })
    }

    /// 明确记录一个不产 pose 的 Present。调用方必须同时写诊断行，不能静默跳号。
    pub fn skip_present(&mut self, present_seq: u64) -> Result<(), CameraPoseAssemblyError> {
        if let Some(active) = self.pending.as_ref().map(|pending| pending.present_seq)
            && active != present_seq
        {
            return Err(CameraPoseAssemblyError::BoundaryMismatch {
                active,
                boundary: present_seq,
            });
        }
        self.validate_next_present(present_seq)?;
        self.pending = None;
        self.last_accounted = Some(present_seq);
        Ok(())
    }

    fn ensure_pending(&mut self, present_seq: u64) -> Result<(), CameraPoseAssemblyError> {
        if let Some(active) = self.pending.as_ref().map(|pending| pending.present_seq) {
            if present_seq == active {
                return Ok(());
            }
            return Err(CameraPoseAssemblyError::MissingPresentBoundary {
                active,
                incoming: present_seq,
            });
        }
        self.validate_next_present(present_seq)?;
        self.pending = Some(PendingPose {
            present_seq,
            view: None,
            projection: None,
        });
        Ok(())
    }

    fn validate_next_present(&self, present_seq: u64) -> Result<(), CameraPoseAssemblyError> {
        if let Some(previous) = self.last_accounted {
            if present_seq == previous {
                return Err(CameraPoseAssemblyError::DuplicatePresentSeq { present_seq });
            }
            if present_seq < previous {
                return Err(CameraPoseAssemblyError::PresentSeqOutOfOrder {
                    previous,
                    current: present_seq,
                });
            }
            if previous.checked_add(1) != Some(present_seq) {
                return Err(CameraPoseAssemblyError::PresentSeqGap {
                    previous,
                    current: present_seq,
                });
            }
        }
        Ok(())
    }
}

fn record_matrix(
    pending: &mut PendingPose,
    kind: &'static str,
    buffer_tag: u64,
    sample_qpc: i64,
    matrix: Mat4,
) -> Result<(), CameraPoseAssemblyError> {
    let target = if kind == "view" {
        &mut pending.view
    } else {
        &mut pending.projection
    };
    match target {
        None => {
            *target = Some(BoundMatrix {
                buffer_tag,
                sample_qpc,
                matrix,
            });
        }
        Some(previous) if previous.buffer_tag != buffer_tag => {
            return Err(CameraPoseAssemblyError::AmbiguousMatrixSource {
                present_seq: pending.present_seq,
                kind,
                first_buffer_tag: previous.buffer_tag,
                second_buffer_tag: buffer_tag,
            });
        }
        Some(previous) if previous.matrix != matrix => {
            return Err(CameraPoseAssemblyError::MatrixChangedWithinPresent {
                present_seq: pending.present_seq,
                kind,
                buffer_tag,
            });
        }
        Some(previous) => previous.sample_qpc = previous.sample_qpc.max(sample_qpc),
    }
    Ok(())
}

fn unique_bound_buffer<'a>(
    sample: CameraDrawSample<'a>,
    source: MatrixSourceBinding,
    kind: &'static str,
) -> Result<&'a BoundBufferAtDraw<'a>, CameraPoseAssemblyError> {
    let mut matches = sample.bound_buffers.iter().filter(|buffer| {
        buffer.shader_stage == source.shader_stage && buffer.bind_slot == source.bind_slot
    });
    let first = matches.next().ok_or_else(|| {
        if kind == "view" {
            CameraPoseAssemblyError::MissingView {
                present_seq: sample.present_seq,
            }
        } else {
            CameraPoseAssemblyError::MissingProjection {
                present_seq: sample.present_seq,
            }
        }
    })?;
    if let Some(second) = matches.next() {
        return Err(CameraPoseAssemblyError::AmbiguousMatrixSource {
            present_seq: sample.present_seq,
            kind,
            first_buffer_tag: first.buffer_tag,
            second_buffer_tag: second.buffer_tag,
        });
    }
    if first.bytes.len() != source.buffer_bytes as usize {
        return Err(if kind == "view" {
            CameraPoseAssemblyError::InvalidViewMatrix {
                present_seq: sample.present_seq,
            }
        } else {
            CameraPoseAssemblyError::InvalidProjectionMatrix {
                present_seq: sample.present_seq,
            }
        });
    }
    Ok(first)
}

fn read_matrix(source: MatrixSourceBinding, bytes: &[u8]) -> Option<Mat4> {
    match source.encoding {
        MatrixEncoding::F32LittleEndian => {}
    }
    let start = source.offset as usize;
    let end = start.checked_add(64)?;
    let raw = bytes.get(start..end)?;
    let mut matrix = [[0.0; 4]; 4];
    for (index, chunk) in raw.as_chunks::<4>().0.iter().enumerate() {
        let value = f32::from_le_bytes(*chunk);
        if !value.is_finite() {
            return None;
        }
        let row = index / 4;
        let column = index % 4;
        match source.layout {
            MatrixLayout::RowMajor => matrix[row][column] = value,
            MatrixLayout::ColumnMajor => matrix[column][row] = value,
        }
    }
    if source.transpose {
        matrix = std::array::from_fn(|row| std::array::from_fn(|column| matrix[column][row]));
    }
    Some(matrix)
}

/// Decode and validate the projection carried by one binding-qualified constant buffer.
///
/// This is deliberately narrower than exposing [`read_matrix`]: callers that need per-frame
/// depth projection evidence must prove the exact production source, byte width and render
/// geometry before the resulting `(A, B)` can be attached to a depth-index row.
#[must_use]
pub fn projection_from_bound_buffer(
    source: MatrixSourceBinding,
    bytes: &[u8],
    render_width: u32,
    render_height: u32,
) -> Option<crate::depth::Projection> {
    if source.role != MatrixRole::Projection || bytes.len() != source.buffer_bytes as usize {
        return None;
    }
    let matrix = read_matrix(source, bytes)?;
    if !valid_projection_matrix(&matrix, render_width, render_height) {
        return None;
    }
    projection_from_matrix(&matrix)
}

/// Decode and validate the view pose carried by one binding-qualified constant buffer.
///
/// The returned position remains in the game's native view-matrix units. Production applies the
/// binding's explicit `view_metres_per_unit` only when it materializes the durable pose row.
#[must_use]
pub fn view_pose_from_bound_buffer(
    source: ViewSourceBinding,
    bytes: &[u8],
) -> Option<crate::camera::CameraPose> {
    if source.source.role != MatrixRole::View || bytes.len() != source.source.buffer_bytes as usize
    {
        return None;
    }
    let matrix = read_matrix(source.source, bytes)?;
    if !valid_view_matrix(&matrix, source.translation) {
        return None;
    }
    Some(pose_from_view(&matrix, source.translation))
}

/// Resolve Fallout 4's camera-relative render basis into the delivery coordinate system.
///
/// The audited VS12 basis is orthonormal with determinant -1 because the shader basis stores
/// delivery Y/Z in swapped columns. Right-multiplying that fixed swap yields the proper camera
/// rotation. Translation deliberately remains owned by the exact-build game-camera sampler.
pub fn fallout_render_rotation_from_bound_buffer(
    source: ViewSourceBinding,
    bytes: &[u8],
) -> Option<[f32; 4]> {
    if source.source.role != MatrixRole::View || bytes.len() != source.source.buffer_bytes as usize
    {
        return None;
    }
    let matrix = read_matrix(source.source, bytes)?;
    fallout_render_rotation_from_basis(&matrix, source.translation)
}

#[must_use]
pub fn fallout_render_rotation_from_basis(
    matrix: &Mat4,
    translation: TranslationSlot,
) -> Option<[f32; 4]> {
    if translation != TranslationSlot::Row3 || !valid_fallout_render_basis(matrix) {
        return None;
    }
    // The pinned Fallout payload is a world-to-camera basis.  Swapping its Y/Z
    // columns repairs the handedness, but the delivery quaternion is explicitly
    // camera-to-world, so the repaired basis must also be inverted.  It is
    // orthonormal, therefore inversion is an exact transpose.
    let world_to_camera =
        std::array::from_fn(|row| [matrix[row][0], matrix[row][2], matrix[row][1]]);
    let determinant = determinant3_rows(&world_to_camera);
    if !determinant.is_finite() || (determinant - 1.0).abs() > ORTHONORMAL_TOLERANCE * 4.0 {
        return None;
    }
    let camera_to_world =
        std::array::from_fn(|row| std::array::from_fn(|column| world_to_camera[column][row]));
    let quaternion = mat3_to_quat(&camera_to_world);
    let norm = quaternion.iter().map(|value| value * value).sum::<f32>();
    (norm.is_finite() && (norm - 1.0).abs() <= 0.002).then_some(quaternion)
}

fn valid_fallout_render_basis(matrix: &Mat4) -> bool {
    if !matrix.iter().flatten().all(|value| value.is_finite())
        || matrix[3][0].abs() > ORTHONORMAL_TOLERANCE
        || matrix[3][1].abs() > ORTHONORMAL_TOLERANCE
        || matrix[3][2].abs() > ORTHONORMAL_TOLERANCE
        || (matrix[3][3] - 1.0).abs() > ORTHONORMAL_TOLERANCE
    {
        return false;
    }
    let basis = std::array::from_fn(|row| [matrix[row][0], matrix[row][1], matrix[row][2]]);
    for row in &basis {
        let norm = row.iter().map(|value| value * value).sum::<f32>();
        if (norm - 1.0).abs() > ORTHONORMAL_TOLERANCE {
            return false;
        }
    }
    for left in 0..3 {
        for right in (left + 1)..3 {
            let dot = (0..3)
                .map(|column| basis[left][column] * basis[right][column])
                .sum::<f32>();
            if dot.abs() > ORTHONORMAL_TOLERANCE {
                return false;
            }
        }
    }
    let determinant = determinant3_rows(&basis);
    determinant.is_finite() && (determinant + 1.0).abs() <= ORTHONORMAL_TOLERANCE * 4.0
}

fn determinant3_rows(matrix: &[[f32; 3]; 3]) -> f32 {
    matrix[0][0] * (matrix[1][1] * matrix[2][2] - matrix[1][2] * matrix[2][1])
        - matrix[0][1] * (matrix[1][0] * matrix[2][2] - matrix[1][2] * matrix[2][0])
        + matrix[0][2] * (matrix[1][0] * matrix[2][1] - matrix[1][1] * matrix[2][0])
}

fn valid_view_matrix(matrix: &Mat4, translation: TranslationSlot) -> bool {
    if classify(matrix) != (MatrixKind::View { translation }) {
        return false;
    }
    let determinant = determinant3(matrix);
    // A constant-buffer payload is not necessarily a standalone homogeneous
    // transform. Fallout 4's exact-build main-camera source is a packed 3x4
    // rigid view followed by a shader-owned fourth row (observed as both
    // [0,0,0,0] and [.5,.5,.5,.5]). The row is not consumed by pose_from_view
    // for a Col3 binding, so requiring m[3][3] == 1 rejects the verified camera
    // while adding no geometric protection. The pinned source identity,
    // translation slot, orthonormal 3x3 and positive determinant remain strict.
    determinant.is_finite() && (determinant - 1.0).abs() <= ORTHONORMAL_TOLERANCE * 4.0
}

fn valid_projection_matrix(matrix: &Mat4, width: u32, height: u32) -> bool {
    if classify(matrix) != MatrixKind::Projection {
        return false;
    }
    if projection_from_matrix(matrix).is_none() {
        return false;
    }
    let intrinsics = intrinsics_from_projection(matrix, width as f32, height as f32);
    intrinsics_are_valid(intrinsics, width, height)
}

fn intrinsics_are_valid(intrinsics: Intrinsics, width: u32, height: u32) -> bool {
    intrinsics.fx.is_finite()
        && intrinsics.fy.is_finite()
        && intrinsics.cx.is_finite()
        && intrinsics.cy.is_finite()
        && intrinsics.fx > 0.0
        && intrinsics.fy > 0.0
        && (0.0..=width as f32).contains(&intrinsics.cx)
        && (0.0..=height as f32).contains(&intrinsics.cy)
}

fn determinant3(matrix: &Mat4) -> f32 {
    let a = matrix[0][0];
    let b = matrix[0][1];
    let c = matrix[0][2];
    let d = matrix[1][0];
    let e = matrix[1][1];
    let f = matrix[1][2];
    let g = matrix[2][0];
    let h = matrix[2][1];
    let i = matrix[2][2];
    a * (e * i - f * h) - b * (d * i - f * g) + c * (d * h - e * g)
}

#[cfg(test)]
mod versioned_binding_tests {
    use super::*;

    const HASH: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn v1_source(offset: u32, role: MatrixRole) -> MatrixSourceBinding {
        MatrixSourceBinding {
            shader_stage: ShaderStage::Vertex,
            bind_slot: 5,
            pass_ordinal: 0,
            draw_ordinal: 7,
            buffer_bytes: 256,
            offset,
            encoding: MatrixEncoding::F32LittleEndian,
            layout: MatrixLayout::RowMajor,
            transpose: false,
            role,
        }
    }

    fn d3d12_source(matrix_offset: u32, role: MatrixRole) -> D3d12MatrixSourceBinding {
        D3d12MatrixSourceBinding {
            descriptor_table: D3d12DescriptorTableBinding {
                root_signature_sha256: HASH.into(),
                root_parameter_index: 18,
                descriptor_range_index: 2,
                descriptor_offset_in_range: 0,
                descriptor_count: 1,
                base_shader_register: 7,
                register_space: 0,
            },
            cbv_resource_offset: 0,
            declared_cbv_bytes: 256,
            matrix_offset,
            encoding: MatrixEncoding::F32LittleEndian,
            layout: MatrixLayout::RowMajor,
            transpose: true,
            role,
            draw_relation: D3d12DrawRelation::ExactSelectedDepthDsvDraw,
        }
    }

    fn v2_binding() -> D3d12CameraBinding {
        D3d12CameraBinding {
            holstered_camera: None,
            schema: CAMERA_BINDING_SCHEMA_V2.into(),
            binding_id: "cp2077-d3d12-authoring-candidate".into(),
            binding_version: "authoring-candidate-1".into(),
            profile_id: "cp2077-depth".into(),
            profile_version: "dm-depth-profile-1".into(),
            executable_sha256: HASH.into(),
            render_width: 1920,
            render_height: 1080,
            view_metres_per_unit: "1.0".into(),
            axis_mapping: D3d12AxisMapping {
                delivery_x_from: D3d12SignedAxis::PositiveX,
                delivery_y_from: D3d12SignedAxis::PositiveY,
                delivery_z_from: D3d12SignedAxis::PositiveZ,
            },
            view_sources: vec![D3d12ViewSourceBinding {
                source: d3d12_source(0, MatrixRole::View),
                translation: TranslationSlot::Row3,
            }],
            projection_sources: vec![d3d12_source(128, MatrixRole::Projection)],
        }
    }

    fn kcd2_combined_binding() -> Kcd2CombinedCameraBinding {
        Kcd2CombinedCameraBinding {
            schema: KCD2_COMBINED_CAMERA_BINDING_SCHEMA_V1.into(),
            binding_id: "kcd2-steam-23914554-combined-vp-v1".into(),
            binding_version: "1".into(),
            profile_id: crate::KCD2_MAINLINE_GAME_ID.into(),
            profile_version: crate::DepthProfile::D3D12_STABLE_SCHEMA_VERSION.into(),
            executable_sha256: crate::KCD2_EXECUTABLE_SHA256.into(),
            render_width: 1920,
            render_height: 1080,
            view_metres_per_unit: "1.0".into(),
            axis_mapping: D3d12AxisMapping {
                delivery_x_from: D3d12SignedAxis::PositiveX,
                delivery_y_from: D3d12SignedAxis::PositiveZ,
                delivery_z_from: D3d12SignedAxis::PositiveY,
            },
            live_pose_plan: crate::Kcd2LivePosePlan::pinned(),
        }
    }

    #[test]
    fn versioned_document_keeps_v1_compatible() {
        let binding = CameraBinding {
            schema: CAMERA_BINDING_SCHEMA_V1.into(),
            binding_id: "fallout-v1".into(),
            binding_version: "1".into(),
            profile_id: "fallout-depth".into(),
            profile_version: "dm-depth-profile-1".into(),
            executable_sha256: HASH.into(),
            render_width: 1920,
            render_height: 1080,
            view_metres_per_unit: Some("1.0".into()),
            game_camera: None,
            view_sources: vec![ViewSourceBinding {
                source: v1_source(0, MatrixRole::View),
                translation: TranslationSlot::Row3,
            }],
            projection_sources: vec![v1_source(64, MatrixRole::Projection)],
        };
        let encoded = serde_json::to_string(&binding).unwrap();
        let document: CameraBindingDocument = serde_json::from_str(&encoded).unwrap();
        assert!(matches!(document, CameraBindingDocument::V1(_)));
        document.validate_for_executable(HASH).unwrap();
    }

    #[test]
    fn fallout4_binding_rejects_an_unverified_runtime_hash() {
        let actual = format!("sha256:{}", "a".repeat(64));
        let binding = CameraBinding {
            schema: CAMERA_BINDING_SCHEMA_V1.into(),
            binding_id: "fallout-v1".into(),
            binding_version: "1".into(),
            profile_id: "fallout4-steam-release-test".into(),
            profile_version: "dm-depth-profile-1".into(),
            executable_sha256: HASH.into(),
            render_width: 1920,
            render_height: 1080,
            view_metres_per_unit: Some("1.0".into()),
            game_camera: None,
            view_sources: vec![ViewSourceBinding {
                source: v1_source(0, MatrixRole::View),
                translation: TranslationSlot::Row3,
            }],
            projection_sources: vec![v1_source(64, MatrixRole::Projection)],
        };

        assert!(matches!(
            binding.resolve(&actual),
            Err(CameraBindingError::ExecutableHashMismatch { .. })
        ));
        let resolved = binding.resolve(HASH).unwrap();
        assert_eq!(resolved.executable_sha256, HASH);
    }

    #[test]
    fn d3d12_binding_has_only_cross_launch_source_identity() {
        let binding = v2_binding();
        binding.validate_for_executable(HASH).unwrap();
        let encoded = serde_json::to_string(&CameraBindingDocument::V2(binding)).unwrap();
        assert!(!encoded.contains("source_tag"));
        assert!(!encoded.contains("draw_ordinal"));
        assert!(!encoded.contains("resource_tag"));
        assert!(encoded.contains("\"axis_mapping\":{\"delivery_x_from\""));
        let decoded: CameraBindingDocument = serde_json::from_str(&encoded).unwrap();
        assert!(matches!(decoded, CameraBindingDocument::V2(_)));
    }

    #[test]
    fn d3d12_binding_rejects_unresolved_or_unbounded_evidence() {
        let mut binding = v2_binding();
        binding.axis_mapping.delivery_z_from = D3d12SignedAxis::NegativeY;
        assert_eq!(
            binding.validate_for_executable(HASH),
            Err(D3d12CameraBindingError::InvalidAxisMapping)
        );

        let mut binding = v2_binding();
        binding.view_sources[0].source.declared_cbv_bytes = 128;
        assert!(matches!(
            binding.validate_for_executable(HASH),
            Err(D3d12CameraBindingError::InvalidMatrixSource { kind: "view", .. })
        ));

        let mut binding = v2_binding();
        binding.projection_sources[0]
            .descriptor_table
            .root_parameter_index = 17;
        assert_eq!(
            binding.validate_for_executable(HASH),
            Err(D3d12CameraBindingError::ViewProjectionSourceMismatch)
        );
    }

    #[test]
    fn kcd2_combined_document_is_exact_build_and_runtime_supported() {
        let binding = kcd2_combined_binding();
        binding
            .validate_for_executable(crate::KCD2_EXECUTABLE_SHA256)
            .unwrap();
        let encoded = serde_json::to_vec(&binding).unwrap();
        let document: CameraBindingDocument = serde_json::from_slice(&encoded).unwrap();
        assert!(matches!(document, CameraBindingDocument::Kcd2Combined(_)));
        assert!(document.runtime_supported());
        assert_eq!(document.render_dimensions(), (1920, 1080));
        assert_eq!(document.profile_id(), crate::KCD2_MAINLINE_GAME_ID);

        let mut drifted = binding;
        drifted.axis_mapping.delivery_y_from = D3d12SignedAxis::NegativeZ;
        assert!(
            drifted
                .validate_for_executable(crate::KCD2_EXECUTABLE_SHA256)
                .is_err()
        );
    }

    #[test]
    fn kcd2_observed_sources_require_the_new_binding_identity() {
        let mut binding = kcd2_combined_binding();
        binding.live_pose_plan = crate::Kcd2LivePosePlan::pinned_v3();
        assert!(
            binding
                .validate_for_executable(crate::KCD2_EXECUTABLE_SHA256)
                .is_err()
        );
        binding.schema = KCD2_COMBINED_CAMERA_BINDING_SCHEMA_V2.into();
        binding.binding_id = crate::KCD2_COMBINED_BINDING_ID_V2.into();
        binding.binding_version = crate::KCD2_COMBINED_BINDING_VERSION_V2.into();
        binding
            .validate_for_executable(crate::KCD2_EXECUTABLE_SHA256)
            .unwrap();
        let document: CameraBindingDocument =
            serde_json::from_value(serde_json::to_value(&binding).unwrap()).unwrap();
        document
            .validate_for_executable(crate::KCD2_EXECUTABLE_SHA256)
            .unwrap();
        binding.live_pose_plan = crate::Kcd2LivePosePlan::pinned();
        assert!(
            binding
                .validate_for_executable(crate::KCD2_EXECUTABLE_SHA256)
                .is_err()
        );
    }
}

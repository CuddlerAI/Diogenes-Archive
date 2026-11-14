//! probe 的配置来源。
//!
//! # 为什么不能只用环境变量
//!
//! probe 是被注入进**已经在运行的游戏进程**的。它 `DllMain` 里读到的
//! `std::env::var` 是**游戏进程**的环境，不是注入器的——注入器上设的变量到不了它。
//!
//! 环境变量那套只对手工流程有效（operator 先 `set` 再从同一个 shell 启动游戏）。
//! 客户端驱动的注入必须换一条路：**双方约定一个固定路径的配置文件**，
//! 客户端注入前写，probe 启动时读。
//!
//! 这个坑很隐蔽——env 版本在手工测试里一路都对，接进客户端后
//! probe 会安静地拿到全部默认值（或按现在的实现直接拒绝启动），
//! 而注入本身是成功的。

use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
#[cfg(windows)]
use std::os::windows::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::camera_binding::GameCameraBinding;
use crate::dsv_discovery::{DsvDiscoveryConfig, DsvDiscoveryDescriptor};

/// 配置文件的约定位置：`%PROGRAMDATA%\DataMiner\depth-lab\probe-config.json`。
///
/// 选 `PROGRAMDATA` 而不是 `LOCALAPPDATA`：游戏进程与客户端可能以不同用户
/// 或不同完整性级别运行，`PROGRAMDATA` 是两边都能稳定读到的位置。
pub const CONFIG_RELATIVE_PATH: &str = r"DataMiner\depth-lab\probe-config.json";
/// Probe configuration is a small control document. A hard ceiling prevents a corrupted or
/// replaced ProgramData file from allocating unbounded memory inside the game process.
pub const MAX_PROBE_CONFIG_BYTES: u64 = 1024 * 1024;

/// 生产 Camera pose 输出的 run 级配置。
///
/// `binding_path` 指向按 build/profile 审核过的 [`crate::CameraBinding`]；它与
/// discovery 的 `camera_probe` 互斥，避免候选扫描结果被误当生产 pose。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CameraProductionConfig {
    pub binding_path: String,
    #[serde(default)]
    pub binding_sha256: Option<String>,
    pub run_id: String,
    pub generation: u64,
}

pub const CAMERA_DISCOVERY_SCHEMA_V1: &str = "camera-discovery-config/v1";
/// v2 keeps the v1 bounded draw scan and adds an explicitly untrusted, exact-build
/// camera reference sampled on the same Present as every matrix observation.
pub const CAMERA_DISCOVERY_SCHEMA_V2: &str = "camera-discovery-config/v2";
/// v3 replaces the lossy render tuple with a strictly replayed two-run D3D11 DSV selection
/// attestation. The attestation is still only a discovery bridge: it cannot arm recording or
/// create a production CameraBinding.
pub const CAMERA_DISCOVERY_SCHEMA_V3: &str = "camera-discovery-config/v3";
pub const MAX_DISCOVERY_DRAWS_PER_SAMPLED_PRESENT: u32 = 512;

/// Camera discovery 本次 run 的受审渲染身份。
///
/// 这里故意不接受“自动挑一个看起来像深度的纹理”：同尺寸资源不唯一时，probe 必须
/// fail-closed。格式也由 operator 明写，避免 1080p/D24 之类的历史样机常量悄悄变成契约。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiscoveryDepthFormat {
    R24G8Typeless,
    D24UnormS8Uint,
    R32Typeless,
    D32Float,
    R32G8X24Typeless,
    D32FloatS8X24Uint,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CameraDiscoveryRenderIdentity {
    pub width: u32,
    pub height: u32,
    pub sample_count: u32,
    pub formats: Vec<DiscoveryDepthFormat>,
}

/// Root-relative binding to the bridge-only D3D11 DSV selection attestation.
///
/// The injected runtime does not trust this document by hash alone. At activation and every phase
/// reset it replays all sources through `validate_dsv_selection_attestation_with_sources` and then
/// compares the returned executable and full 17-field descriptor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CameraDiscoveryDsvSelection {
    pub evidence_root: String,
    pub attestation_locator: String,
    pub attestation_sha256: String,
}

/// 一段可独立裁决的 Camera discovery phase。
///
/// `sample_every_n_presents` 控制采样频率，`draw_window_*` 控制每个主场景 pass 内
/// 真正进入后台扫描的 draw 区间。二者都是证据的一部分，不能在报告里隐去。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CameraDiscoveryConfig {
    pub schema: String,
    pub phase_id: String,
    pub executable_sha256: String,
    /// Legacy coarse identity, admitted only by v1/v2. v3 must omit it completely.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub render: Option<CameraDiscoveryRenderIdentity>,
    /// Strict two-run selection binding, admitted only by v3.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dsv_selection: Option<CameraDiscoveryDsvSelection>,
    pub sample_every_n_presents: u32,
    pub max_main_passes_per_present: u32,
    pub draw_window_start: u32,
    pub draw_window_count: u32,
    /// Optional legacy/singleton camera used only as a comparison signal during discovery.
    ///
    /// It is deliberately named `reference`, not `authority`: #636 exists because this exact
    /// stream may not be the render camera. v1 configs must leave it absent; v2 requires it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comparison_camera_reference: Option<GameCameraBinding>,
}

/// 解析约定路径。非 Windows 上退化到临时目录，仅为让测试能跑。
#[must_use]
pub fn config_path() -> PathBuf {
    #[cfg(windows)]
    let base = std::env::var_os("PROGRAMDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
    #[cfg(not(windows))]
    let base = std::env::temp_dir();
    base.join(CONFIG_RELATIVE_PATH.replace('\\', std::path::MAIN_SEPARATOR_STR))
}

/// probe 启动时需要知道的一切。
//
// 只 derive `PartialEq` 不 derive `Eq`：`target_depth_hz: Option<f64>` 让 `Eq`
// 无法成立（浮点没有自反的相等）。本类型只用于比较与序列化，不做 HashMap/BTreeSet
// 的键，所以失去 `Eq` 没有调用点受影响。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProbeConfig {
    /// 总开关。**文件存在但 `enabled=false` 时 probe 必须什么都不做**——
    /// 这让客户端可以留着文件只翻一个字段，而不必删文件。
    pub enabled: bool,
    /// 深度产物落盘根目录，必须在本次 session 目录之下。
    pub output_dir: String,
    /// 选用帧尾倒数第几次解绑的快照（#437 实测 BioShock 取 4）。
    pub unbind_from_end: usize,
    /// 采样帧数上限，`None` 表示不限。
    pub max_frames: Option<u64>,
    /// **只写水印、不回读深度**（性能认证的中间档）。
    ///
    /// # 为什么必须有这一档
    ///
    /// 68 号 / 01 号的性能门原本是三档对比；A/B 诊断在水印与完整之间新增
    /// readback-only，把 GPU 回读与 writer 编码/落盘拆开。
    /// 中间那档的作用是**归因**——掉帧时分清是水印开销还是回读开销。
    /// #443 排障正是靠它定位到「每帧重建 RTV」（仅水印一档就掉 13.73%）。
    ///
    /// 而水印写入被包在 `depth_capture_enabled()` 里（理由正确：关掉深度采集
    /// 不该继续改游戏画面），于是原本**没有**「注入了、写水印、但不回读」这个状态，
    /// 三档测法跑不出中间那档。本字段补上它。
    ///
    /// # 与 `enabled` 的关系
    ///
    /// 本字段只在 `enabled = true` 时有意义。`enabled=false` 仍然是「什么都不做」，
    /// 不会因为 `watermark_only=true` 就去写水印——**关掉就是关掉**。
    ///
    /// # 产物不可交付
    ///
    /// 这一档不产出任何深度帧，索引为空。它只用于性能测量，
    /// 与 `camera_discovery` 同属「此 session 不得用于交付」的模式。
    /// Present-hook-only certification baseline. It intentionally shares the
    /// depth bypass with `watermark_only`, but suppresses the watermark write.
    #[serde(default)]
    pub present_only: bool,
    #[serde(default)]
    pub watermark_only: bool,
    /// 完整 selected-Present 回读后，在 EXR writer 前丢弃帧的 A/B 诊断档。
    /// 仅由 performance-validation-only 的受控 StartRun 设置。
    #[serde(default)]
    pub writer_bypass: bool,
    /// **深度采样目标频率（Hz）**，按时间基定频，与游戏 Present 率无关。
    ///
    /// 旧实现是 `present_seq % 2`，含义是「Present 率的一半」——只有游戏恰好跑在
    /// 车道标称帧率时才等于期望频率。#443 那台不封顶跑 187 fps 时它实际是 93.5 Hz。
    ///
    /// `None` 表示按车道取默认值（[`crate::cadence::default_target_hz`]，即车道 fps ÷ 2），
    /// 与旧行为在「游戏跑车道帧率」时逐字一致。
    #[serde(default)]
    pub target_depth_hz: Option<f64>,
    /// 车道标称帧率（30/60），透传自 StartRun.lane_fps。`None` = 未带
    /// （旧客户端/手工 env 流程），present 帧率闸停用。见 #801。
    #[serde(default)]
    pub lane_fps: Option<u32>,
    /// **P4 相机位姿候选采集**（77 号 §5.11）。默认关。
    ///
    /// 单独一个开关而不是跟着 `enabled` 走：它要 hook `Map`/`Unmap`，
    /// 每帧扫描常量缓冲，是一条**全新的、未经真机性能验证的开销**。
    /// C5 刚刚才把 FPS 门压回 2% 以内，不能顺手把它再顶出去。
    ///
    /// 开着它跑的 session **不得用于交付**，只用于识别矩阵槽位。
    #[serde(default)]
    pub camera_probe: bool,
    /// discovery 的受审身份、phase 与有界 draw 采样窗。`camera_probe=true` 时必填。
    #[serde(default)]
    pub camera_discovery: Option<CameraDiscoveryConfig>,
    /// Exact bytes of the operator-authored representative-scene attestation. This is only a
    /// control-plane binding for generic D3D11 inventory v2; the probe does not validate or infer
    /// scene semantics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub representative_scene_attestation_sha256: Option<String>,
    /// Bounded D3D11 DSV inventory. This mode is discovery-only and mutually exclusive with every
    /// production/profile/camera/performance path.
    #[serde(default)]
    pub dsv_discovery: Option<DsvDiscoveryConfig>,
    /// 逐 Present 生产 Camera pose。`None` 表示不启用生产模式。
    #[serde(default)]
    pub camera_production: Option<CameraProductionConfig>,
    /// **depth profile 的绝对路径**（[`crate::profile::DepthProfile`] 的 JSON）。
    ///
    /// 探针从这里拿投影参数 `(a, b)`、`metric_scale` 与 Z 约定。
    ///
    /// `None` 时探针仍会采，但索引里每一行都是 `proj_source=unknown`——
    /// 那些深度**事后无法线性化成米**（[#490](https://github.com/DiogenesModel/Diogenes/issues/490)
    /// 的 61 GiB 就是这个状态）。而且降采样方向也只能退回保守默认，
    /// 在 reversed-Z 的游戏上会取到最远表面。
    ///
    /// 走文件路径而不是把参数内联进本结构：profile 是逐游戏的、带 `executable_sha256`
    /// 钉扣与 `metric_scale` 证据链的完整对象，抄一份进来就会有两份可以各自漂移。
    #[serde(default)]
    pub depth_profile_path: Option<String>,
    #[serde(default)]
    pub depth_profile_sha256: Option<String>,
    /// 写这份配置的 session id，仅用于排障时对得上号。
    pub session_id: String,
}

/// 配置读取失败的原因。**每一条都要能区分「没配」和「配错了」**——
/// 前者是客户端没写，后者是写了但内容不对，排障方向完全不同。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// 文件不存在。probe 应当安静退出，不 hook 任何东西。
    NotPresent,
    Unreadable(String),
    Malformed(String),
    /// 文件在、但 `enabled=false`。
    Disabled,
    /// `output_dir` 为空——写配置的一方漏了字段。
    EmptyOutputDir,
    CameraModeConflict,
    MissingCameraDiscovery,
    InvalidCameraDiscovery(String),
    InvalidDsvDiscovery(String),
    EmptyCameraBindingPath,
    EmptyCameraRunId,
    InvalidCameraGeneration,
    MissingCameraDepthProfile,
    InvalidProductionHash,
    InvalidPerformanceMode(String),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotPresent => write!(f, "配置文件不存在，probe 不启动"),
            Self::Unreadable(e) => write!(f, "配置文件读不了: {e}"),
            Self::Malformed(e) => write!(f, "配置文件格式不对: {e}"),
            Self::Disabled => write!(f, "配置里 enabled=false，probe 不启动"),
            Self::EmptyOutputDir => write!(f, "配置里 output_dir 为空"),
            Self::CameraModeConflict => {
                write!(
                    f,
                    "camera_probe discovery 与 camera_production 不能同时启用"
                )
            }
            Self::MissingCameraDiscovery => write!(
                f,
                "camera_probe discovery 缺 camera_discovery；禁止回退到硬编码 1920x1080"
            ),
            Self::InvalidCameraDiscovery(reason) => {
                write!(f, "camera_discovery 不可用: {reason}")
            }
            Self::InvalidDsvDiscovery(reason) => {
                write!(f, "D3D11 DSV discovery 不可用: {reason}")
            }
            Self::EmptyCameraBindingPath => write!(f, "camera_production.binding_path 为空"),
            Self::EmptyCameraRunId => write!(f, "camera_production.run_id 为空"),
            Self::InvalidCameraGeneration => {
                write!(f, "camera_production.generation 必须大于 0")
            }
            Self::MissingCameraDepthProfile => {
                write!(
                    f,
                    "camera_production 需要 depth_profile_path 来钉住 profile/build"
                )
            }
            Self::InvalidProductionHash => {
                write!(f, "camera_production profile/binding SHA-256 非法")
            }
            Self::InvalidPerformanceMode(reason) => {
                write!(f, "performance mode invalid: {reason}")
            }
        }
    }
}

/// 从约定路径读配置。
pub fn load() -> Result<ProbeConfig, ConfigError> {
    load_from(&config_path())
}

/// Load the exact bounded bytes consumed by the parser. Discovery activation evidence hashes
/// these same bytes, avoiding a second path read that could describe a different configuration.
pub fn load_with_bytes() -> Result<(ProbeConfig, Vec<u8>), ConfigError> {
    load_from_with_bytes(&config_path())
}

/// 从指定路径读（供测试与手工流程使用）。
pub fn load_from(path: &std::path::Path) -> Result<ProbeConfig, ConfigError> {
    load_from_with_bytes(path).map(|(config, _)| config)
}

/// Bounded exact-byte variant of [`load_from`].
pub fn load_from_with_bytes(path: &std::path::Path) -> Result<(ProbeConfig, Vec<u8>), ConfigError> {
    if !probe_config_leaf_exists_without_reparse(path)? {
        return Err(ConfigError::NotPresent);
    }
    reject_probe_config_reparse_chain(path)?;
    let canonical_before = path
        .canonicalize()
        .map_err(|error| ConfigError::Unreadable(error.to_string()))?;
    let path_metadata =
        fs::symlink_metadata(path).map_err(|error| ConfigError::Unreadable(error.to_string()))?;
    if probe_config_path_is_reparse(&path_metadata)
        || !path_metadata.is_file()
        || path_metadata.len() > MAX_PROBE_CONFIG_BYTES
    {
        return Err(ConfigError::Unreadable(
            "config must be a bounded, non-symlink regular file".into(),
        ));
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    options.share_mode(1); // FILE_SHARE_READ: deny new writers and delete/replace handles.
    let mut file = options
        .open(path)
        .map_err(|error| ConfigError::Unreadable(error.to_string()))?;
    let opened_metadata = file
        .metadata()
        .map_err(|error| ConfigError::Unreadable(error.to_string()))?;
    if !opened_metadata.is_file()
        || opened_metadata.len() > MAX_PROBE_CONFIG_BYTES
        || opened_metadata.len() != path_metadata.len()
    {
        return Err(ConfigError::Unreadable(
            "opened config exceeded its regular-file bound".into(),
        ));
    }
    let mut bytes = Vec::with_capacity(opened_metadata.len() as usize);
    (&mut file)
        .take(MAX_PROBE_CONFIG_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| ConfigError::Unreadable(error.to_string()))?;
    if bytes.len() as u64 != opened_metadata.len() || bytes.len() as u64 > MAX_PROBE_CONFIG_BYTES {
        return Err(ConfigError::Unreadable(
            "config length changed during its bounded read".into(),
        ));
    }
    file.seek(SeekFrom::Start(0))
        .map_err(|error| ConfigError::Unreadable(error.to_string()))?;
    let mut verified = Vec::with_capacity(bytes.len());
    (&mut file)
        .take(MAX_PROBE_CONFIG_BYTES + 1)
        .read_to_end(&mut verified)
        .map_err(|error| ConfigError::Unreadable(error.to_string()))?;
    if verified != bytes
        || file
            .metadata()
            .map_err(|error| ConfigError::Unreadable(error.to_string()))?
            .len()
            != bytes.len() as u64
    {
        return Err(ConfigError::Unreadable(
            "config bytes changed during its stable read".into(),
        ));
    }
    let canonical_after = path
        .canonicalize()
        .map_err(|error| ConfigError::Unreadable(error.to_string()))?;
    reject_probe_config_reparse_chain(path)?;
    if canonical_after != canonical_before {
        return Err(ConfigError::Unreadable(
            "config path changed during its stable read".into(),
        ));
    }
    if bytes.starts_with(&[0xef, 0xbb, 0xbf]) {
        return Err(ConfigError::Malformed(
            "UTF-8 BOM is not admitted in probe config".into(),
        ));
    }
    let cfg: ProbeConfig =
        serde_json::from_slice(&bytes).map_err(|e| ConfigError::Malformed(e.to_string()))?;
    if !cfg.enabled {
        return Err(ConfigError::Disabled);
    }
    if cfg.output_dir.trim().is_empty() {
        return Err(ConfigError::EmptyOutputDir);
    }
    cfg.validate()?;
    Ok((cfg, bytes))
}

fn probe_config_leaf_exists_without_reparse(path: &std::path::Path) -> Result<bool, ConfigError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if probe_config_path_is_reparse(&metadata) {
                return Err(ConfigError::Unreadable(
                    "probe config leaf is a reparse point".into(),
                ));
            }
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // A missing leaf is a legitimate NotPresent state only when every existing ancestor
            // is ordinary. This distinguishes absence from a dangling symlink/junction escape.
            if let Some(parent) = path.parent() {
                for ancestor in parent.ancestors() {
                    match fs::symlink_metadata(ancestor) {
                        Ok(metadata) if probe_config_path_is_reparse(&metadata) => {
                            return Err(ConfigError::Unreadable(
                                "missing probe config is below a reparse point".into(),
                            ));
                        }
                        Ok(_) => {}
                        Err(ancestor_error)
                            if ancestor_error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(ancestor_error) => {
                            return Err(ConfigError::Unreadable(ancestor_error.to_string()));
                        }
                    }
                }
            }
            Ok(false)
        }
        Err(error) => Err(ConfigError::Unreadable(error.to_string())),
    }
}

fn reject_probe_config_reparse_chain(path: &std::path::Path) -> Result<(), ConfigError> {
    for ancestor in path.ancestors() {
        let metadata = fs::symlink_metadata(ancestor)
            .map_err(|error| ConfigError::Unreadable(error.to_string()))?;
        if probe_config_path_is_reparse(&metadata) {
            return Err(ConfigError::Unreadable(
                "probe config path contains a reparse point".into(),
            ));
        }
    }
    Ok(())
}

fn probe_config_path_is_reparse(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        false
    }
}

impl ProbeConfig {
    /// 校验文件路径与环境变量两条入口共享的 discovery/production 契约。
    pub fn validate(&self) -> Result<(), ConfigError> {
        if !self.enabled {
            return Err(ConfigError::Disabled);
        }
        if self.output_dir.trim().is_empty() {
            return Err(ConfigError::EmptyOutputDir);
        }
        if self.present_only && (!self.watermark_only || self.writer_bypass) {
            return Err(ConfigError::InvalidPerformanceMode(
                "present_only requires watermark_only depth bypass and forbids writer_bypass"
                    .into(),
            ));
        }
        if self.camera_probe && self.camera_production.is_some() {
            return Err(ConfigError::CameraModeConflict);
        }
        if let Some(discovery) = &self.dsv_discovery {
            if discovery.schema == crate::dsv_discovery::DSV_DISCOVERY_CONFIG_SCHEMA_V2 {
                let scene_attestation_sha256 = self
                    .representative_scene_attestation_sha256
                    .as_deref()
                    .ok_or_else(|| {
                        ConfigError::InvalidDsvDiscovery(
                            "v2 requires representative_scene_attestation_sha256".into(),
                        )
                    })?;
                if !crate::camera_binding::is_sha256_digest(scene_attestation_sha256) {
                    return Err(ConfigError::InvalidDsvDiscovery(
                        "representative_scene_attestation_sha256 is not canonical SHA-256".into(),
                    ));
                }
            } else if self.representative_scene_attestation_sha256.is_some() {
                return Err(ConfigError::InvalidDsvDiscovery(
                    "representative_scene_attestation_sha256 is only admitted with D3D11 DSV discovery v2"
                        .into(),
                ));
            }
            discovery
                .validate()
                .map_err(ConfigError::InvalidDsvDiscovery)?;
            if self.session_id != format!("dsv-discovery-{}", discovery.discovery_id) {
                return Err(ConfigError::InvalidDsvDiscovery(
                    "session_id must equal dsv-discovery-<discovery_id>".into(),
                ));
            }
            if self.camera_probe
                || self.camera_discovery.is_some()
                || self.camera_production.is_some()
                || self.depth_profile_path.is_some()
                || self.depth_profile_sha256.is_some()
                || self.max_frames.is_some()
                || self.present_only
                || self.watermark_only
                || self.writer_bypass
                || self.target_depth_hz.is_some()
                || self.lane_fps.is_some()
            {
                return Err(ConfigError::InvalidDsvDiscovery(
                    "discovery_only 必须与 camera/profile/performance/formal capture 配置互斥"
                        .into(),
                ));
            }
            let output = PathBuf::from(&self.output_dir);
            if !output.is_absolute() {
                return Err(ConfigError::InvalidDsvDiscovery(
                    "output_dir 必须是 evidence_root 下的新绝对目录".into(),
                ));
            }
        } else if self.representative_scene_attestation_sha256.is_some() {
            return Err(ConfigError::InvalidDsvDiscovery(
                "representative_scene_attestation_sha256 is only admitted with D3D11 DSV discovery v2"
                    .into(),
            ));
        }
        if self.camera_probe {
            let discovery = self
                .camera_discovery
                .as_ref()
                .ok_or(ConfigError::MissingCameraDiscovery)?;
            validate_camera_discovery(discovery)?;
        } else if self.camera_discovery.is_some() {
            return Err(ConfigError::InvalidCameraDiscovery(
                "camera_probe=false 时不得携带 camera_discovery".into(),
            ));
        }
        if let Some(camera) = &self.camera_production {
            if camera.binding_path.trim().is_empty() {
                return Err(ConfigError::EmptyCameraBindingPath);
            }
            if camera.run_id.trim().is_empty() {
                return Err(ConfigError::EmptyCameraRunId);
            }
            if camera.generation == 0 {
                return Err(ConfigError::InvalidCameraGeneration);
            }
            if self
                .depth_profile_path
                .as_deref()
                .is_none_or(|path| path.trim().is_empty())
            {
                return Err(ConfigError::MissingCameraDepthProfile);
            }
            if self
                .depth_profile_sha256
                .as_deref()
                .is_some_and(|hash| !crate::camera_binding::is_sha256_digest(hash))
                || camera
                    .binding_sha256
                    .as_deref()
                    .is_some_and(|hash| !crate::camera_binding::is_sha256_digest(hash))
            {
                return Err(ConfigError::InvalidProductionHash);
            }
        }
        Ok(())
    }
}

fn validate_camera_discovery(config: &CameraDiscoveryConfig) -> Result<(), ConfigError> {
    let invalid = |reason: &str| ConfigError::InvalidCameraDiscovery(reason.into());
    if !matches!(
        config.schema.as_str(),
        CAMERA_DISCOVERY_SCHEMA_V1 | CAMERA_DISCOVERY_SCHEMA_V2 | CAMERA_DISCOVERY_SCHEMA_V3
    ) {
        return Err(invalid(
            "schema 必须为 camera-discovery-config/v1、v2 或 v3",
        ));
    }
    match (
        config.schema.as_str(),
        config.comparison_camera_reference.as_ref(),
    ) {
        (CAMERA_DISCOVERY_SCHEMA_V1, Some(_)) => {
            return Err(invalid("v1 不得携带 comparison_camera_reference"));
        }
        (CAMERA_DISCOVERY_SCHEMA_V2, None) => {
            return Err(invalid("v2 必须携带 comparison_camera_reference"));
        }
        _ => {}
    }
    let render = match (
        config.schema.as_str(),
        config.render.as_ref(),
        config.dsv_selection.as_ref(),
    ) {
        (CAMERA_DISCOVERY_SCHEMA_V1 | CAMERA_DISCOVERY_SCHEMA_V2, Some(render), None) => {
            Some(render)
        }
        (CAMERA_DISCOVERY_SCHEMA_V1 | CAMERA_DISCOVERY_SCHEMA_V2, None, _) => {
            return Err(invalid("v1/v2 必须携带 legacy render 身份"));
        }
        (CAMERA_DISCOVERY_SCHEMA_V1 | CAMERA_DISCOVERY_SCHEMA_V2, Some(_), Some(_)) => {
            return Err(invalid("v1/v2 不得携带 dsv_selection"));
        }
        (CAMERA_DISCOVERY_SCHEMA_V3, None, Some(selection)) => {
            validate_camera_discovery_dsv_selection(selection)?;
            None
        }
        (CAMERA_DISCOVERY_SCHEMA_V3, Some(_), _) => {
            return Err(invalid("v3 拒绝 legacy render；必须只使用 dsv_selection"));
        }
        (CAMERA_DISCOVERY_SCHEMA_V3, None, None) => {
            return Err(invalid("v3 必须携带 dsv_selection"));
        }
        _ => unreachable!("camera discovery schema checked above"),
    };
    if config.phase_id.is_empty()
        || !config
            .phase_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(invalid("phase_id 只能包含 ASCII 字母、数字、-、_"));
    }
    if !crate::camera_binding::is_sha256_digest(&config.executable_sha256) {
        return Err(invalid("executable_sha256 必须是小写 sha256:<64 hex>"));
    }
    if let Some(render) = render {
        if render.width == 0 || render.height == 0 {
            return Err(invalid("render width/height 必须大于 0"));
        }
        if render.sample_count == 0 {
            return Err(invalid("render sample_count 必须大于 0"));
        }
        if render.formats.is_empty() {
            return Err(invalid("render formats 不能为空"));
        }
        let mut formats = render.formats.clone();
        formats.sort_by_key(|format| *format as u8);
        formats.dedup();
        if formats.len() != render.formats.len() {
            return Err(invalid("render formats 不得重复"));
        }
    }
    if config.sample_every_n_presents == 0 {
        return Err(invalid("sample_every_n_presents 必须大于 0"));
    }
    if !(1..=16).contains(&config.max_main_passes_per_present) {
        return Err(invalid("max_main_passes_per_present 必须在 1..=16"));
    }
    if !(1..=256).contains(&config.draw_window_count) {
        return Err(invalid("draw_window_count 必须在 1..=256"));
    }
    if config
        .draw_window_start
        .checked_add(config.draw_window_count)
        .is_none()
    {
        return Err(invalid("draw window 溢出"));
    }
    if config
        .max_main_passes_per_present
        .checked_mul(config.draw_window_count)
        .is_none_or(|draws| draws > MAX_DISCOVERY_DRAWS_PER_SAMPLED_PRESENT)
    {
        return Err(invalid("每个采样 Present 的最大 draw 数不得超过 512"));
    }
    Ok(())
}

fn validate_camera_discovery_dsv_selection(
    selection: &CameraDiscoveryDsvSelection,
) -> Result<(), ConfigError> {
    let invalid = |reason: &str| ConfigError::InvalidCameraDiscovery(reason.into());
    if !std::path::Path::new(&selection.evidence_root).is_absolute() {
        return Err(invalid("dsv_selection.evidence_root 必须是绝对路径"));
    }
    if !crate::camera_binding::is_sha256_digest(&selection.attestation_sha256) {
        return Err(invalid(
            "dsv_selection.attestation_sha256 必须是小写 sha256:<64 hex>",
        ));
    }
    let locator = std::path::Path::new(&selection.attestation_locator);
    let components = locator.components().collect::<Vec<_>>();
    let safe_leaf = match components.as_slice() {
        [std::path::Component::Normal(leaf)] => leaf.to_str().is_some_and(|leaf| {
            !leaf.is_empty()
                && leaf.len() <= 128
                && leaf.ends_with(".json")
                && leaf.as_bytes()[0].is_ascii_alphanumeric()
                && leaf
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
        }),
        _ => false,
    };
    if selection.attestation_locator.contains('\\') || !safe_leaf {
        return Err(invalid(
            "dsv_selection.attestation_locator 必须是 evidence_root 下唯一一层的安全 ASCII .json leaf",
        ));
    }
    Ok(())
}

/// Convert a replay-validated exact descriptor to the legacy report geometry. This is a report
/// compatibility view only; v3 runtime selection must compare the full descriptor.
pub fn render_identity_from_dsv_descriptor(
    descriptor: DsvDiscoveryDescriptor,
) -> Result<CameraDiscoveryRenderIdentity, String> {
    let mut formats = Vec::new();
    for value in [descriptor.resource_format, descriptor.view_format] {
        let format = match value {
            44 => DiscoveryDepthFormat::R24G8Typeless,
            45 => DiscoveryDepthFormat::D24UnormS8Uint,
            39 => DiscoveryDepthFormat::R32Typeless,
            40 => DiscoveryDepthFormat::D32Float,
            19 => DiscoveryDepthFormat::R32G8X24Typeless,
            20 => DiscoveryDepthFormat::D32FloatS8X24Uint,
            other => return Err(format!("unsupported D3D11 depth format {other}")),
        };
        if !formats.contains(&format) {
            formats.push(format);
        }
    }
    Ok(CameraDiscoveryRenderIdentity {
        width: descriptor.width,
        height: descriptor.height,
        sample_count: descriptor.sample_count,
        formats,
    })
}

/// 客户端注入前写配置。**先写临时文件再原子改名**——
/// probe 可能在任意时刻读它，读到半截 JSON 会退化成「格式不对」而不是「没配」，
/// 排障方向就错了。
pub fn write(path: &std::path::Path, cfg: &ProbeConfig) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("建 {} 失败: {e}", dir.display()))?;
    }
    let tmp = path.with_extension("json.tmp");
    let text = serde_json::to_string_pretty(cfg).map_err(|e| e.to_string())?;
    std::fs::write(&tmp, text).map_err(|e| format!("写 {} 失败: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("改名 {} 失败: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("dm-probe-cfg-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d.join("probe-config.json")
    }

    fn sample() -> ProbeConfig {
        ProbeConfig {
            enabled: true,
            output_dir: r"D:\dm\sessions\s1\depth-lab\lane-30".into(),
            unbind_from_end: 4,
            max_frames: None,
            present_only: false,
            watermark_only: false,
            writer_bypass: false,
            target_depth_hz: None,
            lane_fps: None,
            camera_probe: false,
            camera_discovery: None,
            representative_scene_attestation_sha256: None,
            dsv_discovery: None,
            camera_production: None,
            depth_profile_path: None,
            depth_profile_sha256: None,
            session_id: "s1".into(),
        }
    }

    fn discovery(phase_id: &str) -> CameraDiscoveryConfig {
        CameraDiscoveryConfig {
            schema: CAMERA_DISCOVERY_SCHEMA_V1.into(),
            phase_id: phase_id.into(),
            executable_sha256: format!("sha256:{}", "0".repeat(64)),
            render: Some(CameraDiscoveryRenderIdentity {
                width: 2560,
                height: 1600,
                sample_count: 1,
                formats: vec![DiscoveryDepthFormat::R24G8Typeless],
            }),
            dsv_selection: None,
            sample_every_n_presents: 30,
            max_main_passes_per_present: 4,
            draw_window_start: 0,
            draw_window_count: 128,
            comparison_camera_reference: None,
        }
    }

    fn discovery_v3(phase_id: &str, evidence_root: &std::path::Path) -> CameraDiscoveryConfig {
        CameraDiscoveryConfig {
            schema: CAMERA_DISCOVERY_SCHEMA_V3.into(),
            phase_id: phase_id.into(),
            executable_sha256: format!("sha256:{}", "0".repeat(64)),
            render: None,
            dsv_selection: Some(CameraDiscoveryDsvSelection {
                evidence_root: evidence_root.to_string_lossy().into_owned(),
                attestation_locator: "selection-a.json".into(),
                attestation_sha256: format!("sha256:{}", "a".repeat(64)),
            }),
            sample_every_n_presents: 30,
            max_main_passes_per_present: 4,
            draw_window_start: 0,
            draw_window_count: 128,
            comparison_camera_reference: None,
        }
    }

    fn dsv_discovery(evidence_root: &std::path::Path) -> DsvDiscoveryConfig {
        DsvDiscoveryConfig {
            schema: crate::DSV_DISCOVERY_CONFIG_SCHEMA_V2.into(),
            discovery_id: "ac3-local-dsv-window".into(),
            executable_sha256: format!("sha256:{}", "a".repeat(64)),
            evidence_root: evidence_root.to_string_lossy().into_owned(),
            limits: crate::DsvDiscoveryLimits {
                max_frames: 120,
                max_candidates: 32,
                max_candidates_per_frame: 16,
            },
        }
    }

    #[test]
    fn watermark_only_defaults_to_false_and_survives_old_configs() {
        // 旧格式配置（客户端可能已在盘上留下）必须仍能读，且退化成"不是仅水印档"。
        let p = tmp("wmonly-legacy");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(
            &p,
            r#"{"enabled":true,"output_dir":"D:\\dm","unbind_from_end":4,"max_frames":null,"session_id":"s1"}"#,
        )
        .unwrap();
        assert!(!load_from(&p).unwrap().watermark_only);
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn watermark_only_is_meaningless_when_disabled() {
        // 「关掉就是关掉」——enabled=false 时不该因为 watermark_only 就去写水印。
        // 这条由 load() 的 Disabled 分支保证：它在读到 enabled=false 时直接返回
        // Err(Disabled)，调用方拿不到 config，自然也拿不到 watermark_only。
        let p = tmp("wmonly-disabled");
        let mut c = sample();
        c.enabled = false;
        c.watermark_only = true;
        write(&p, &c).unwrap();
        assert_eq!(load_from(&p), Err(ConfigError::Disabled));
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn round_trips() {
        let p = tmp("roundtrip");
        write(&p, &sample()).unwrap();
        assert_eq!(load_from(&p).unwrap(), sample());
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn exact_byte_loader_returns_the_bytes_it_parsed() {
        let p = tmp("exact-bytes");
        write(&p, &sample()).unwrap();
        let expected = std::fs::read(&p).unwrap();
        let (loaded, bytes) = load_from_with_bytes(&p).unwrap();
        assert_eq!(loaded, sample());
        assert_eq!(bytes, expected);
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn exact_byte_loader_rejects_bom_and_oversized_inputs() {
        let bom = tmp("bom");
        std::fs::create_dir_all(bom.parent().unwrap()).unwrap();
        let mut bytes = vec![0xef, 0xbb, 0xbf];
        bytes.extend_from_slice(br#"{"enabled":true}"#);
        std::fs::write(&bom, bytes).unwrap();
        assert!(matches!(
            load_from_with_bytes(&bom),
            Err(ConfigError::Malformed(_))
        ));
        let _ = std::fs::remove_dir_all(bom.parent().unwrap());

        let oversized = tmp("oversized");
        std::fs::create_dir_all(oversized.parent().unwrap()).unwrap();
        std::fs::write(&oversized, vec![b' '; MAX_PROBE_CONFIG_BYTES as usize + 1]).unwrap();
        assert!(matches!(
            load_from_with_bytes(&oversized),
            Err(ConfigError::Unreadable(_))
        ));
        let _ = std::fs::remove_dir_all(oversized.parent().unwrap());
    }

    #[test]
    fn a_missing_file_is_distinguishable_from_a_broken_one() {
        // 「没配」和「配错了」的排障方向完全不同，不能混成一个错误
        let p = tmp("missing");
        assert_eq!(load_from(&p), Err(ConfigError::NotPresent));

        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, "{ not json").unwrap();
        assert!(matches!(load_from(&p), Err(ConfigError::Malformed(_))));
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn missing_config_below_reparse_and_dangling_leaf_are_not_not_present() {
        use std::os::unix::fs::symlink;

        let dangling = tmp("dangling-leaf");
        std::fs::create_dir_all(dangling.parent().unwrap()).unwrap();
        symlink(dangling.with_file_name("absent-target.json"), &dangling).unwrap();
        assert!(matches!(
            load_from_with_bytes(&dangling),
            Err(ConfigError::Unreadable(_))
        ));
        let _ = std::fs::remove_dir_all(dangling.parent().unwrap());

        let below_link = tmp("missing-below-link");
        let container = below_link.parent().unwrap();
        std::fs::create_dir_all(container).unwrap();
        let real = container.join("real");
        let link = container.join("linked");
        std::fs::create_dir(&real).unwrap();
        symlink(&real, &link).unwrap();
        assert!(matches!(
            load_from_with_bytes(&link.join("missing.json")),
            Err(ConfigError::Unreadable(_))
        ));
        let _ = std::fs::remove_dir_all(container);
    }

    #[test]
    fn disabled_is_its_own_state_not_an_error_about_the_file() {
        // 客户端应当能只翻一个字段就关掉采集，而不必删文件
        let p = tmp("disabled");
        let mut c = sample();
        c.enabled = false;
        write(&p, &c).unwrap();
        assert_eq!(load_from(&p), Err(ConfigError::Disabled));
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn an_empty_output_dir_is_refused() {
        let p = tmp("emptydir");
        let mut c = sample();
        c.output_dir = "   ".into();
        write(&p, &c).unwrap();
        assert_eq!(load_from(&p), Err(ConfigError::EmptyOutputDir));
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn writing_leaves_no_temp_file_behind() {
        // probe 可能在任意时刻读；半截 JSON 会把「没配」误报成「配错了」
        let p = tmp("notmp");
        write(&p, &sample()).unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(p.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "残留: {leftovers:?}");
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn production_camera_requires_an_isolated_run_binding_and_profile() {
        let p = tmp("camera-production");
        let mut cfg = sample();
        cfg.depth_profile_path = Some(r"D:\profiles\synthetic.json".into());
        cfg.camera_production = Some(CameraProductionConfig {
            binding_path: r"D:\bindings\synthetic-camera.json".into(),
            binding_sha256: None,
            run_id: "r0001".into(),
            generation: 1,
        });
        write(&p, &cfg).unwrap();
        assert_eq!(load_from(&p).unwrap(), cfg);

        let mut conflict = cfg.clone();
        conflict.camera_probe = true;
        write(&p, &conflict).unwrap();
        assert_eq!(load_from(&p), Err(ConfigError::CameraModeConflict));

        let mut missing_profile = cfg;
        missing_profile.depth_profile_path = None;
        write(&p, &missing_profile).unwrap();
        assert_eq!(load_from(&p), Err(ConfigError::MissingCameraDepthProfile));
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn discovery_requires_explicit_non_1080_render_identity_and_bounded_window() {
        let p = tmp("camera-discovery");
        let mut cfg = sample();
        cfg.camera_probe = true;
        write(&p, &cfg).unwrap();
        assert_eq!(load_from(&p), Err(ConfigError::MissingCameraDiscovery));

        cfg.camera_discovery = Some(discovery("A1"));
        write(&p, &cfg).unwrap();
        let loaded = load_from(&p).unwrap();
        let discovery = loaded.camera_discovery.unwrap();
        let render = discovery.render.as_ref().unwrap();
        assert_eq!((render.width, render.height), (2560, 1600));

        cfg.camera_discovery.as_mut().unwrap().draw_window_count = 0;
        write(&p, &cfg).unwrap();
        assert!(matches!(
            load_from(&p),
            Err(ConfigError::InvalidCameraDiscovery(_))
        ));
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn discovery_v2_requires_an_explicit_untrusted_camera_reference() {
        let p = tmp("camera-discovery-v2");
        let mut cfg = sample();
        cfg.camera_probe = true;
        let mut v2 = discovery("POSE_STATIC");
        v2.schema = CAMERA_DISCOVERY_SCHEMA_V2.into();
        cfg.camera_discovery = Some(v2.clone());
        write(&p, &cfg).unwrap();
        assert!(matches!(
            load_from(&p),
            Err(ConfigError::InvalidCameraDiscovery(_))
        ));

        v2.comparison_camera_reference = Some(GameCameraBinding {
            provider: "fallout4-player-camera-root-exact-build/v1".into(),
            camera_singleton_relocation: 1,
            camera_root_offset: 2,
            world_transform_offset: 3,
            metres_per_game_unit: "1".into(),
            axis_mapping: "x_z_y".into(),
        });
        cfg.camera_discovery = Some(v2);
        write(&p, &cfg).unwrap();
        assert!(load_from(&p).is_ok());
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn discovery_v3_requires_only_one_canonical_attestation_leaf_and_rejects_legacy_fields() {
        let p = tmp("camera-discovery-v3");
        let mut cfg = sample();
        cfg.camera_probe = true;
        cfg.camera_discovery = Some(discovery_v3("POSE_STATIC", p.parent().unwrap()));
        write(&p, &cfg).unwrap();
        assert!(load_from(&p).is_ok());

        let mut legacy_render = cfg.clone();
        legacy_render.camera_discovery.as_mut().unwrap().render = discovery("A1").render;
        write(&p, &legacy_render).unwrap();
        assert!(matches!(
            load_from(&p),
            Err(ConfigError::InvalidCameraDiscovery(_))
        ));

        let mut missing_selection = cfg.clone();
        missing_selection
            .camera_discovery
            .as_mut()
            .unwrap()
            .dsv_selection = None;
        write(&p, &missing_selection).unwrap();
        assert!(matches!(
            load_from(&p),
            Err(ConfigError::InvalidCameraDiscovery(_))
        ));

        for locator in [
            "./selection.json",
            "nested/selection.json",
            "nested\\selection.json",
            "../selection.json",
            ".selection.json",
            "selection.txt",
        ] {
            let mut invalid = cfg.clone();
            invalid
                .camera_discovery
                .as_mut()
                .unwrap()
                .dsv_selection
                .as_mut()
                .unwrap()
                .attestation_locator = locator.into();
            write(&p, &invalid).unwrap();
            assert!(
                matches!(load_from(&p), Err(ConfigError::InvalidCameraDiscovery(_))),
                "locator unexpectedly admitted: {locator}"
            );
        }

        let mut legacy = cfg;
        legacy.camera_discovery.as_mut().unwrap().schema = CAMERA_DISCOVERY_SCHEMA_V1.into();
        legacy.camera_discovery.as_mut().unwrap().render = discovery("A1").render;
        write(&p, &legacy).unwrap();
        assert!(matches!(
            load_from(&p),
            Err(ConfigError::InvalidCameraDiscovery(_))
        ));
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn dsv_discovery_is_bounded_and_isolated_from_every_formal_capture_path() {
        let p = tmp("dsv-discovery");
        let evidence_root = p.parent().unwrap().join("dsv-evidence");
        let mut cfg = sample();
        cfg.output_dir = evidence_root
            .join("ac3-local-dsv-window")
            .to_string_lossy()
            .into_owned();
        cfg.representative_scene_attestation_sha256 = Some(format!("sha256:{}", "b".repeat(64)));
        cfg.dsv_discovery = Some(dsv_discovery(&evidence_root));
        cfg.session_id = "dsv-discovery-ac3-local-dsv-window".into();
        write(&p, &cfg).unwrap();
        assert_eq!(load_from(&p).unwrap(), cfg);

        let mut wrong_session = cfg.clone();
        wrong_session.session_id = "s1".into();
        write(&p, &wrong_session).unwrap();
        assert!(matches!(
            load_from(&p),
            Err(ConfigError::InvalidDsvDiscovery(_))
        ));

        let mut formal_frames = cfg.clone();
        formal_frames.max_frames = Some(1);
        write(&p, &formal_frames).unwrap();
        assert!(matches!(
            load_from(&p),
            Err(ConfigError::InvalidDsvDiscovery(_))
        ));

        let mut profile = cfg.clone();
        profile.depth_profile_path = Some(r"D:\profiles\guessed.json".into());
        write(&p, &profile).unwrap();
        assert!(matches!(
            load_from(&p),
            Err(ConfigError::InvalidDsvDiscovery(_))
        ));

        let mut camera = cfg.clone();
        camera.camera_probe = true;
        camera.camera_discovery = Some(discovery("A1"));
        write(&p, &camera).unwrap();
        assert!(matches!(
            load_from(&p),
            Err(ConfigError::InvalidDsvDiscovery(_))
        ));

        let mut unbounded = cfg.clone();
        unbounded
            .dsv_discovery
            .as_mut()
            .unwrap()
            .limits
            .max_candidates = 0;
        write(&p, &unbounded).unwrap();
        assert!(matches!(
            load_from(&p),
            Err(ConfigError::InvalidDsvDiscovery(_))
        ));

        let mut bad_hash = cfg.clone();
        bad_hash.dsv_discovery.as_mut().unwrap().executable_sha256 = "sha256:not-a-digest".into();
        write(&p, &bad_hash).unwrap();
        assert!(matches!(
            load_from(&p),
            Err(ConfigError::InvalidDsvDiscovery(_))
        ));

        let mut missing_scene_attestation = cfg.clone();
        missing_scene_attestation.representative_scene_attestation_sha256 = None;
        write(&p, &missing_scene_attestation).unwrap();
        assert!(matches!(
            load_from(&p),
            Err(ConfigError::InvalidDsvDiscovery(_))
        ));

        for invalid_hash in [
            "not-a-hash".to_string(),
            format!("sha256:{}", "B".repeat(64)),
            format!("sha256:{}", "b".repeat(63)),
        ] {
            let mut invalid_scene_attestation = cfg.clone();
            invalid_scene_attestation.representative_scene_attestation_sha256 = Some(invalid_hash);
            write(&p, &invalid_scene_attestation).unwrap();
            assert!(matches!(
                load_from(&p),
                Err(ConfigError::InvalidDsvDiscovery(_))
            ));
        }

        let mut legacy_schema_with_scene_hash = cfg.clone();
        legacy_schema_with_scene_hash
            .dsv_discovery
            .as_mut()
            .unwrap()
            .schema = crate::DSV_DISCOVERY_CONFIG_SCHEMA_V1.into();
        write(&p, &legacy_schema_with_scene_hash).unwrap();
        assert!(matches!(
            load_from(&p),
            Err(ConfigError::InvalidDsvDiscovery(_))
        ));

        let mut relative_output = cfg;
        relative_output.output_dir = "ac3-local-dsv-window".into();
        write(&p, &relative_output).unwrap();
        assert!(matches!(
            load_from(&p),
            Err(ConfigError::InvalidDsvDiscovery(_))
        ));
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn set_generic_d3d11_config_shape_loads_with_exact_scene_attestation_hash() {
        let p = tmp("set-generic-d3d11-shape");
        let evidence_root = p.parent().unwrap().join("evidence");
        let output = evidence_root.join("ac3-window-001");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let scene_sha256 = format!("sha256:{}", "b".repeat(64));
        let json = serde_json::json!({
            "enabled": true,
            "output_dir": output,
            "unbind_from_end": 4,
            "max_frames": null,
            "present_only": false,
            "watermark_only": false,
            "writer_bypass": false,
            "target_depth_hz": null,
            "lane_fps": null,
            "camera_probe": false,
            "camera_discovery": null,
            "representative_scene_attestation_sha256": scene_sha256,
            "dsv_discovery": {
                "schema": crate::DSV_DISCOVERY_CONFIG_SCHEMA_V2,
                "discovery_id": "ac3-window-001",
                "executable_sha256": format!("sha256:{}", "a".repeat(64)),
                "evidence_root": evidence_root,
                "limits": {
                    "max_frames": 600,
                    "max_candidates": 128,
                    "max_candidates_per_frame": 32
                }
            },
            "camera_production": null,
            "depth_profile_path": null,
            "depth_profile_sha256": null,
            "session_id": "dsv-discovery-ac3-window-001"
        });
        std::fs::write(&p, serde_json::to_vec_pretty(&json).unwrap()).unwrap();

        let loaded = load_from(&p).unwrap();
        assert_eq!(
            loaded.representative_scene_attestation_sha256.as_deref(),
            Some(scene_sha256.as_str())
        );
        assert_eq!(
            loaded.dsv_discovery.as_ref().unwrap().schema,
            crate::DSV_DISCOVERY_CONFIG_SCHEMA_V2
        );
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn scene_attestation_hash_is_rejected_outside_dsv_v2_but_disabled_stays_off() {
        let p = tmp("scene-hash-mode-boundary");
        let mut cfg = sample();
        cfg.representative_scene_attestation_sha256 = Some(format!("sha256:{}", "b".repeat(64)));
        write(&p, &cfg).unwrap();
        assert!(matches!(
            load_from(&p),
            Err(ConfigError::InvalidDsvDiscovery(_))
        ));

        cfg.enabled = false;
        write(&p, &cfg).unwrap();
        assert_eq!(load_from(&p), Err(ConfigError::Disabled));
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn dsv_discovery_nested_config_rejects_unknown_fields() {
        let p = tmp("dsv-discovery-unknown-field");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(
            &p,
            format!(
                r#"{{"enabled":true,"output_dir":"D:\\dm\\dsv-evidence\\run","unbind_from_end":4,"max_frames":null,"dsv_discovery":{{"schema":"{}","discovery_id":"run","executable_sha256":"sha256:{}","evidence_root":"D:\\dm\\dsv-evidence","limits":{{"max_frames":1,"max_candidates":1,"max_candidates_per_frame":1}},"selected_candidate":"dsv-0001"}},"session_id":"s1"}}"#,
                crate::DSV_DISCOVERY_CONFIG_SCHEMA_V2,
                "a".repeat(64)
            ),
        )
        .unwrap();
        assert!(matches!(load_from(&p), Err(ConfigError::Malformed(_))));
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn probe_config_rejects_unknown_top_level_fields() {
        let p = tmp("unknown-top-level-field");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(
            &p,
            r#"{"enabled":true,"output_dir":"D:\\dm","unbind_from_end":4,"max_frames":null,"session_id":"s1","unexpected":true}"#,
        )
        .unwrap();
        assert!(matches!(load_from(&p), Err(ConfigError::Malformed(_))));
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn the_agreed_path_is_stable() {
        // 客户端与 probe 靠这个常量对上号，改它等于改协议
        assert_eq!(
            CONFIG_RELATIVE_PATH,
            r"DataMiner\depth-lab\probe-config.json"
        );
    }
}

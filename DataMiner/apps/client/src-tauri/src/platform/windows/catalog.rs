use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::{Context, ensure};
use capture_core::ingest_contract::{CLIENT_CAPTURE_IS_ALWAYS_NON_TRAINABLE, SessionContract};
use capture_core::mapping::MappingSnapshot;
use capture_core::profile::{AcceptedEncoderRelaxations, CaptureAudioProfile, CaptureProfile};
use capture_core::recording::{
    CatalogProvenance, HardwarePreflightEvidence, RecordingCatalogContext, RecordingCatalogEntry,
};
use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::Path;

#[cfg(any(feature = "dev-catalog", all(test, feature = "depth-lab")))]
const SYNTHETIC_CATALOG_BYTES: &[u8] = include_bytes!(concat!(
    env!("DATAMINER_CONFIG_DIR"),
    "/catalogs/dev/synthetic/catalog.json"
));
#[cfg(any(feature = "dev-catalog", all(test, feature = "depth-lab")))]
const SYNTHETIC_DEVICE_BYTES: &[u8] = include_bytes!(concat!(
    env!("DATAMINER_CONFIG_DIR"),
    "/catalogs/dev/synthetic/device-profile.json"
));
#[cfg(any(feature = "dev-catalog", all(test, feature = "depth-lab")))]
const SYNTHETIC_RECORDING_BYTES: &[u8] = include_bytes!(concat!(
    env!("DATAMINER_CONFIG_DIR"),
    "/catalogs/dev/synthetic/recording-profile.json"
));
#[cfg(any(feature = "dev-catalog", all(test, feature = "depth-lab")))]
const SYNTHETIC_GAME_BYTES: &[u8] = include_bytes!(concat!(
    env!("DATAMINER_CONFIG_DIR"),
    "/catalogs/dev/synthetic/game-profile.json"
));
#[cfg(any(feature = "dev-catalog", all(test, feature = "depth-lab")))]
const SYNTHETIC_MAPPING_BYTES: &[u8] = include_bytes!(concat!(
    env!("DATAMINER_CONFIG_DIR"),
    "/catalogs/dev/synthetic/mapping.json"
));

// 由 build.rs 生成：`dev-catalog` 构建内嵌的全部游戏目录，逐条来自
// config/catalogs/shipped-games.json 的 catalogDir（五件套必嵌；目录下存在
// depth-profile.json / camera-binding.json 则一并内嵌）。游戏的增删只登记在
// manifest 一处——这里没有、也不许再有逐游戏的常量块或特性门。
#[cfg(feature = "dev-catalog")]
include!(concat!(env!("OUT_DIR"), "/embedded_dev_catalogs.rs"));

// 这里曾有 `enum CatalogMode { DevNonTrainable }`——一个单变体枚举，持有
// `dev_non_trainable` 这个同时表达「运行环境」「目录渠道」「能不能训练」三件事的词
// （issue #855 §2）。它随本 PR 退役：
//
// * **渠道**从来不是 manifest 说了算，而是「用哪把钉死的公钥验签」——见
//   [`pinned_delivery_anchor`]。certified 退役后（docs/100 §4.5）只剩一把，
//   所以按 mode 选锚早已是一个单臂 match。
// * **训练资格**不再由目录表达：中性 schema 把 `non_trainable` 整个删了
//   （issue #855 §A.4），客户端那一位改由硬约束常量
//   [`CLIENT_CAPTURE_IS_ALWAYS_NON_TRAINABLE`] 给出。
// * **这场采集按哪一代契约写字节**由 [`SessionContract`] 回答，而它是
//   (provenance, schema 世代) 的函数，见 [`CatalogGame::session_contract`]。
//
// 三件事各自有了自己的答案之后，那个词没有任何一处还需要它。

/// 目录 schema 的两代（issue #855 PR 1）。
///
/// legacy 一代把 `non_trainable` 定义成 manifest 与三本 profile 的**必填**字段；
/// 中性一代把它整个删掉——录制配置不再兼职表达训练资格与运行环境。
///
/// 这不是"该字段改成可选"。可选意味着一份中性 bundle 可以再次携带它，于是旧语义
/// 借尸还魂：authoring 侧继续用它表达训练资格，而读者早已不再消费。所以规则是
/// **带了就直接拒绝**，见 [`retired_training_field`]。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CatalogSchemaGeneration {
    Legacy,
    Neutral,
}

impl CatalogSchemaGeneration {
    /// 从 schema 版本串判代。未知版本不猜，直接 `None` → 调用方 fail-closed。
    fn of(schema_version: &str, legacy: &str, neutral: &str) -> Option<Self> {
        if schema_version == legacy {
            Some(Self::Legacy)
        } else if schema_version == neutral {
            Some(Self::Neutral)
        } else {
            None
        }
    }
}

/// 训练词表的代际闸，两个方向各挡一种错。
///
/// - legacy 一代**必须**显式带布尔：一份自称旧 schema 却不带它的载荷是损坏，不是断代；
/// - 中性一代**必须**不带。
///
/// 返回 legacy 一代 authored 的那个布尔；中性一代返回 `None`，意思是"这份载荷不再
/// 声明训练分类"，**不是**"声明为可训练"。
fn retired_training_field(
    generation: CatalogSchemaGeneration,
    declared: Option<bool>,
    payload: &str,
) -> anyhow::Result<Option<bool>> {
    match generation {
        CatalogSchemaGeneration::Legacy => {
            let value = declared.with_context(|| {
                format!("{payload} on the legacy schema must declare an explicit non_trainable")
            })?;
            Ok(Some(value))
        }
        CatalogSchemaGeneration::Neutral => {
            ensure!(
                declared.is_none(),
                "{payload} on the neutral catalog schema must not carry non_trainable; \
                 the neutral schema does not express training classification (issue #855)"
            );
            Ok(None)
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogIdentity {
    pub catalog_id: String,
    pub catalog_version: String,
    pub content_hash: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogEntryIdentity {
    pub id: String,
    pub version: String,
    pub content_hash: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogEntrySnapshot {
    #[serde(flatten)]
    pub identity: CatalogEntryIdentity,
    #[serde(skip)]
    source_bytes: Arc<Vec<u8>>,
}

impl CatalogEntrySnapshot {
    fn new(id: &str, version: &str, bytes: &[u8]) -> anyhow::Result<Self> {
        ensure!(!id.trim().is_empty(), "catalog entry id must not be empty");
        ensure!(
            !version.trim().is_empty(),
            "catalog entry version must not be empty"
        );
        Ok(Self {
            identity: CatalogEntryIdentity {
                id: id.into(),
                version: version.into(),
                content_hash: sha256(bytes),
            },
            source_bytes: Arc::new(bytes.to_vec()),
        })
    }

    #[must_use]
    pub fn source_bytes(&self) -> &[u8] {
        self.source_bytes.as_slice()
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogSelection {
    /// How the active catalog reached this client. Recorded, never gated on.
    pub provenance: CatalogProvenance,
    pub catalog: CatalogIdentity,
    /// 这场采集要按哪一代摄取契约写字节（issue #855 §B）。
    ///
    /// 逐游戏，不是逐 store：发布是每游戏一份 bundle、每游戏独立切 current
    /// （`catalog_releases` 的 published 唯一性是 (channel, catalog_id) 粒度），
    /// 所以 30 款断代的窗口期里下发集合**必然**混着两代。store 级的单一契约会
    /// 在那整段窗口里把整份集合判成不一致，等于一次全机群停录。
    pub session_contract: SessionContract,
    #[serde(skip)]
    catalog_source_bytes: Arc<Vec<u8>>,
    pub device: CatalogEntrySnapshot,
    pub recording: CatalogEntrySnapshot,
    pub game: CatalogEntrySnapshot,
    pub mapping: CatalogEntrySnapshot,
    /// Optional depth payloads selected with the game. Their presence is the
    /// capability signal; `depth_lab::catalog_depth` owns the paired refusal
    /// contract and executable pin checks.
    pub depth_profile: Option<CatalogEntrySnapshot>,
    pub camera_binding: Option<CatalogEntrySnapshot>,
}

impl CatalogSelection {
    #[cfg(not(feature = "local-catalog-only"))]
    #[must_use]
    pub(crate) fn same_profile_generation(&self, other: &Self) -> bool {
        other.session_contract == self.session_contract
            && other.device.identity == self.device.identity
            && other.recording.identity == self.recording.identity
            && other.game.identity == self.game.identity
            && other.mapping.identity == self.mapping.identity
            && optional_identity(&other.depth_profile) == optional_identity(&self.depth_profile)
            && optional_identity(&other.camera_binding) == optional_identity(&self.camera_binding)
    }

    #[must_use]
    pub fn recording_context(&self) -> RecordingCatalogContext {
        RecordingCatalogContext {
            contract: self.session_contract,
            // legacy 契约的两个字节由契约派生，不再由 manifest 声明——这是
            // `RecordingCatalogContext` 的唯一生产构造点，capture-core 开头的
            // `validate_catalog_contract_pair` 会把两者对不上的请求当场拒掉。
            mode: self
                .session_contract
                .legacy_catalog_mode()
                .map(str::to_owned),
            non_trainable: self
                .session_contract
                .declares_training_classification()
                .then_some(CLIENT_CAPTURE_IS_ALWAYS_NON_TRAINABLE),
            provenance: self.provenance,
            catalog_id: self.catalog.catalog_id.clone(),
            catalog_version: self.catalog.catalog_version.clone(),
            catalog_content_hash: self.catalog.content_hash.clone(),
            catalog_source_bytes: Arc::clone(&self.catalog_source_bytes),
            device: recording_entry(&self.device),
            recording: recording_entry(&self.recording),
            game: recording_entry(&self.game),
            mapping: recording_entry(&self.mapping),
        }
    }

    pub fn device_profile(&self) -> anyhow::Result<DeviceProfile> {
        serde_json::from_slice(self.device.source_bytes())
            .context("selected device profile is invalid")
    }

    pub fn recording_profile(&self) -> anyhow::Result<RecordingProfile> {
        serde_json::from_slice(self.recording.source_bytes())
            .context("selected recording profile is invalid")
    }

    #[cfg(test)]
    pub(crate) fn for_test(profile: &CaptureProfile, mapping: &MappingSnapshot) -> Self {
        let catalog_bytes = br#"{"schema_version":"dm-test-catalog-1"}"#.to_vec();
        // 这一份要长得**像客户端真会写下的那份**：会话快照里躺的是整册 recording
        // profile，不是引擎投影。从前这里直接序列化 `CaptureProfile`，于是任何一条
        // 「把快照按整册读回来」的路（recovery 的会话校验就是）在真机上能过、在测试
        // 里必炸——`RecordingProfile` 是 `deny_unknown_fields` 且这些字段都是必填。
        let recording_bytes = serde_json::to_vec(&serde_json::json!({
            "schema_version": "dm-recording-profile-1",
            "profile_id": profile.profile_id,
            "version": "1",
            "game_id": mapping.mapping().game.id,
            "non_trainable": profile.non_trainable,
            "capture_backend": profile.capture_backend,
            "encoder_backend": profile.encoder_backend,
            "width": profile.width,
            "height": profile.height,
            "fps_num": profile.fps_num,
            "fps_den": profile.fps_den,
            "bitrate_bps": profile.bitrate_bps,
            "max_bitrate_bps": profile.max_bitrate_bps,
            "gop_frames": profile.gop_frames,
            "segment_seconds": profile.segment_seconds,
            "test_segments": profile.test_segments,
            "accepted_encoder_relaxations": profile.accepted_encoder_relaxations,
            "window_mode": "windowed_or_borderless",
            "hdr": false,
            "cursor_capture": true,
            "in_game_mouse_sensitivity": serde_json::Value::Null,
            "field_of_view_degrees": serde_json::Value::Null,
            "view_scale": serde_json::Value::Null,
        }))
        .expect("test recording profile serializes");
        let make = |id: &str, version: &str, bytes: Vec<u8>| CatalogEntrySnapshot {
            identity: CatalogEntryIdentity {
                id: id.into(),
                version: version.into(),
                content_hash: sha256(&bytes),
            },
            source_bytes: Arc::new(bytes),
        };
        let mapped = mapping.mapping();
        Self {
            provenance: CatalogProvenance::Compiled,
            catalog: CatalogIdentity {
                catalog_id: "dataminer-test-dev".into(),
                catalog_version: "1".into(),
                content_hash: sha256(&catalog_bytes),
            },
            session_contract: SessionContract::LocalDevV1,
            catalog_source_bytes: Arc::new(catalog_bytes),
            device: make("test-device", "1", b"test-device".to_vec()),
            recording: make(&profile.profile_id, "1", recording_bytes),
            game: make(&mapped.game.id, "1", b"test-game".to_vec()),
            mapping: make(
                &mapped.mapping_id,
                &mapped.version,
                mapping.source_bytes().to_vec(),
            ),
            depth_profile: None,
            camera_binding: None,
        }
    }
}

/// 跟着唯一的调用方 `CatalogSelection::same_profile_generation` 一起开关。
/// 那个函数挂在 `not(local-catalog-only)` 上（本地目录模式不做代际比对），
/// 这里不同步就会在 all_games_dev 组合下变成 dead code，被 `-D warnings` 拦下。
#[cfg(not(feature = "local-catalog-only"))]
fn optional_identity(entry: &Option<CatalogEntrySnapshot>) -> Option<&CatalogEntryIdentity> {
    entry.as_ref().map(|entry| &entry.identity)
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogEntryCounts {
    pub devices: u32,
    pub recordings: u32,
    pub games: u32,
    pub mappings: u32,
    pub depth_profiles: u32,
    pub camera_bindings: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogStatus {
    pub schema_version: &'static str,
    /// How the active catalog reached this client (docs/54 §6.2). Surfaced so an
    /// operator can answer "where did this game list come from" from the UI,
    /// without pulling a session artifact.
    ///
    /// 断代前这里还有一个 store 级的 `mode`。它走了：摄取契约现在是**逐游戏**的
    /// （见 [`CatalogSelection::session_contract`]），一个 store 级的单值只能在
    /// 混代窗口期里说谎。这场采集按哪一代记录，看绑定摘要，那才是它成立的地方。
    pub provenance: CatalogProvenance,
    pub active_catalogs: Vec<CatalogIdentity>,
    pub entry_counts: CatalogEntryCounts,
    pub result: &'static str,
    pub active_generation: u64,
    pub cached_generation: u64,
}

impl Default for CatalogStatus {
    fn default() -> Self {
        Self {
            schema_version: "dm-catalog-status-4",
            provenance: CatalogProvenance::Compiled,
            active_catalogs: Vec::new(),
            entry_counts: CatalogEntryCounts {
                devices: 0,
                recordings: 0,
                games: 0,
                mappings: 0,
                depth_profiles: 0,
                camera_bindings: 0,
            },
            result: "catalog_not_loaded",
            active_generation: 0,
            cached_generation: 0,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogManifest {
    schema_version: String,
    catalog_id: String,
    catalog_version: String,
    /// legacy 一代：渠道派生（发布器按 channel 写），不是 authoring 声明；客户端拿它
    /// 与**通道推出的** mode 对账——这是旧 `validate_classification` 活下来的那一半。
    ///
    /// 中性一代：**必须缺席**（issue #855）。渠道由验签的那把钉死的公钥回答，那本来
    /// 就是更强的一份证据——声明可以撒谎，签名不能。
    non_trainable: Option<bool>,
    device_profiles: Vec<EntryReference>,
    recording_profiles: Vec<EntryReference>,
    game_profiles: Vec<EntryReference>,
    mappings: Vec<EntryReference>,
    #[serde(default)]
    depth_profiles: Vec<EntryReference>,
    #[serde(default)]
    camera_bindings: Vec<EntryReference>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EntryReference {
    id: String,
    version: String,
    content_hash: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GameProfile {
    schema_version: String,
    pub game_id: String,
    pub version: String,
    pub display_name: String,
    pub device_profile_id: String,
    pub recording_profile_id: String,
    /// 只按 id 引用：一份目录内每个 id 至多一个版本（docs/57 §3.2），所以
    /// 版本号进不了引用——改一本册子不再触发上游版本连环跳。
    pub mapping_id: String,
    pub executable_names: Vec<String>,
    // Optional identity refinements. Targeted non-trainable DEV profiles may
    // deliberately use process-name-only discovery to survive game updates and
    // window-class drift; trainability and downstream feature gates remain
    // separate fail-closed decisions.
    pub window_class: Option<String>,
    pub executable_sha256: Option<String>,
    pub input_contract: GameInputContract,
    /// legacy 必填、中性必须缺席（issue #855，见 [`retired_training_field`]）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub non_trainable: Option<bool>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GameInputContract {
    pub keyboard_vks: Vec<u16>,
    pub control_vks: Vec<u16>,
    pub mouse_buttons: Vec<u16>,
    pub capture_mouse_move: bool,
    pub capture_wheel: bool,
}

impl GameProfile {
    /// 这份 game profile 对应的采集分类。
    ///
    /// 中性一代不再声明它（issue #855 §A.4），所以取那条硬约束本身
    /// （[`CLIENT_CAPTURE_IS_ALWAYS_NON_TRAINABLE`]）；legacy 一代沿用 authored 值。
    /// 今天两条路算出同一个字节——UI 徽章与绑定摘要因此一字未变。
    pub(crate) fn is_non_trainable(&self) -> bool {
        self.non_trainable
            .unwrap_or(CLIENT_CAPTURE_IS_ALWAYS_NON_TRAINABLE)
    }
}

impl GameInputContract {
    pub(crate) fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            !self.keyboard_vks.is_empty(),
            "input contract has no keyboard controls"
        );
        ensure!(
            self.keyboard_vks.iter().all(|vk| *vk <= 255),
            "input contract contains an invalid keyboard VK"
        );
        ensure!(
            self.control_vks
                .iter()
                .all(|vk| self.keyboard_vks.contains(vk)),
            "application control keys must also be declared contract keys"
        );
        ensure!(
            self.mouse_buttons
                .iter()
                .all(|vk| matches!(vk, 1 | 2 | 4 | 5 | 6)),
            "input contract contains an invalid mouse button VK"
        );
        ensure!(
            unique(&self.keyboard_vks) && unique(&self.control_vks) && unique(&self.mouse_buttons),
            "input contract contains duplicate controls"
        );
        Ok(())
    }

    pub(crate) fn allows_key(&self, vk: u16) -> bool {
        self.keyboard_vks.contains(&vk)
    }

    pub(crate) fn is_control_key(&self, vk: u16) -> bool {
        self.control_vks.contains(&vk)
    }

    pub(crate) fn allows_mouse_button(&self, vk: u16) -> bool {
        self.mouse_buttons.contains(&vk)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceProfile {
    schema_version: String,
    pub profile_id: String,
    pub version: String,
    /// legacy 必填、中性必须缺席（issue #855，见 [`retired_training_field`]）。
    /// `skip_serializing_if` 让 legacy 的序列化字节一个不差，中性的不凭空写出 null。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub non_trainable: Option<bool>,
    pub platform: String,
    pub minimum_windows_build: u32,
    pub adapter_vendor_id: u32,
    /// 允许的显卡型号。空 = 不限型号——这是常态：新买的卡不该牵动目录发布。
    /// 填了就是**主动排除**（如笔记本省电芯），不是准入名单（57 §2.4）。
    pub adapter_device_ids: Vec<u32>,
    /// 驱动地板，四段版本串。地板而非白名单：白名单挡不住回归，只会在
    /// NVIDIA 自动更新那天让整个机群 409（#328 故障 B）。`None` = 不检查（dev 惯例）。
    pub minimum_driver_version: Option<String>,
    pub encoder_backend: String,
    pub input_backend: String,
    pub pointer_mode: String,
    pub evidence_class: String,
    /// 采集鼠标的硬件 DPI。鼠标 delta → 视角旋转的换算是 DPI × 游戏内灵敏度的
    /// 函数；该值无法从事件流回溯重建（device id 是会话盐化 hash），缺失即永久缺失。
    pub mouse_dpi: Option<u32>,
}

impl DeviceProfile {
    pub fn verify_hardware_evidence(
        &self,
        evidence: &HardwarePreflightEvidence,
    ) -> anyhow::Result<()> {
        ensure!(
            evidence.windows.major >= 10 && evidence.windows.build >= self.minimum_windows_build,
            "DEVICE_PROFILE_WINDOWS_MISMATCH: observed Windows {}.{}.{} is below profile floor {}",
            evidence.windows.major,
            evidence.windows.minor,
            evidence.windows.build,
            self.minimum_windows_build
        );
        ensure!(
            evidence.adapter_vendor_id == self.adapter_vendor_id,
            "DEVICE_PROFILE_VENDOR_MISMATCH: observed adapter vendor {} does not match {}",
            evidence.adapter_vendor_id,
            self.adapter_vendor_id
        );
        ensure!(
            self.adapter_device_ids.is_empty()
                || self
                    .adapter_device_ids
                    .contains(&evidence.adapter_device_id),
            "DEVICE_PROFILE_ADAPTER_MISMATCH: observed adapter device {} is not allowed",
            evidence.adapter_device_id
        );
        self.verify_driver_floor(evidence.adapter_driver_version.as_deref())
    }

    /// 驱动地板检查。与证据结构解耦，边界（相等 / 低于 / 进位 / 字符串序陷阱）可直接单测。
    fn verify_driver_floor(&self, observed_text: Option<&str>) -> anyhow::Result<()> {
        let Some(floor) = self.minimum_driver_version.as_deref() else {
            return Ok(());
        };
        let required = parse_driver_version(floor).with_context(|| {
            format!(
                "DEVICE_PROFILE_DRIVER_FLOOR_INVALID: profile floor {floor} is not four numeric segments"
            )
        })?;
        let observed_text = observed_text.context(
            "DEVICE_PROFILE_DRIVER_UNAVAILABLE: adapter driver version was not observed",
        )?;
        let observed = parse_driver_version(observed_text).with_context(|| {
            format!(
                "DEVICE_PROFILE_DRIVER_UNREADABLE: observed adapter driver version {observed_text} is not four numeric segments"
            )
        })?;
        ensure!(
            observed >= required,
            "DEVICE_PROFILE_DRIVER_TOO_OLD: observed adapter driver version {observed_text} is below profile floor {floor}"
        );
        Ok(())
    }
}

/// 四段无符号整数（`32.0.15.9227`）。数组的字典序即逐段数值序，所以解析出来直接比。
///
/// 必须解析后再比：字符串序按字符走，位数一变就翻。`32.0.15.10001` 是
/// `32.0.15.9227` 的后继，可字符串比时 `'1' < '9'`，它反被判成更旧——正好把一次
/// 正常的驱动更新变成 preflight 拒绝。
fn parse_driver_version(value: &str) -> Option<[u32; 4]> {
    let mut segments = value.trim().split('.');
    let mut parsed = [0u32; 4];
    for slot in parsed.iter_mut() {
        *slot = segments.next()?.parse().ok()?;
    }
    segments.next().is_none().then_some(parsed)
}

/// 一本册子：怎么录这款游戏（docs/57 §3.1）。
///
/// 它是 capture profile 与 settings profile 合并的结果。那两本本来就一一对应
/// （settings 内嵌 `capture_profile_id`，宽高帧率被交叉校验强制相等），分开只买来
/// 一条永远要对账的边和一次多余的版本级联。
///
/// **crate 边界不动**：capture-core 的引擎类型仍是 `CaptureProfile`，由
/// [`RecordingProfile::capture_profile`] 投影出去。FOV / view_scale / 灵敏度
/// 这些游戏侧配置不进引擎 crate。
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RecordingProfile {
    schema_version: String,
    pub profile_id: String,
    pub version: String,
    pub game_id: String,
    /// legacy 一代的 authored 声明（docs/57 §3.4 的第一个布尔），不与渠道对账。
    /// 中性一代必须缺席（issue #855，见 [`retired_training_field`]）：本地测试是否
    /// 永久 non-trainable 由显式的本地采集路径决定，不再借 profile 字段表达。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub non_trainable: Option<bool>,
    // ---- 引擎参数：只有这一段会投影进 capture-core ----
    pub capture_backend: String,
    pub encoder_backend: String,
    pub width: u32,
    pub height: u32,
    pub fps_num: u32,
    pub fps_den: u32,
    pub bitrate_bps: u32,
    pub max_bitrate_bps: u32,
    pub gop_frames: u32,
    pub segment_seconds: u32,
    pub test_segments: u32,
    pub accepted_encoder_relaxations: Option<AcceptedEncoderRelaxations>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio: Option<CaptureAudioProfile>,
    // ---- 游戏侧配置：引擎不看，preflight 与视角标定看 ----
    pub window_mode: String,
    pub hdr: bool,
    pub cursor_capture: bool,
    /// 游戏内鼠标灵敏度与视场角。与 device profile 的 mouse_dpi 一起构成
    /// 跨机器/跨标注员动作语义对齐所需的标定链，采集后无法回溯补齐。
    pub in_game_mouse_sensitivity: Option<f64>,
    pub field_of_view_degrees: Option<f64>,
    /// 视角尺度政策与目录常数（docs/46 §2.1）。None = 该 bundle 尚未
    /// authoring 此块；可训练审批的完备性要求由云端第一期门承担（C5），
    /// 客户端只做类型级自洽校验，不据此拦截录制（门禁在 C4 preflight）。
    pub view_scale: Option<ViewScalePolicy>,
}

impl RecordingProfile {
    /// 引擎需要的那一份投影。`CaptureProfile` 自己的 `validate()` 仍然是引擎参数的
    /// 唯一权威（GOP=round(fps×4s)、码率区间、1920×1080/30 地板等），所以这里只搬字段。
    pub fn capture_profile(&self) -> CaptureProfile {
        CaptureProfile {
            profile_id: self.profile_id.clone(),
            // 中性一代不再声明，取那条硬约束本身作为来源（见常量的文档）。legacy 一代
            // 沿用 authored 值——今天全部 authored 为 true，所以两条路投影出同一个字节。
            non_trainable: self
                .non_trainable
                .unwrap_or(CLIENT_CAPTURE_IS_ALWAYS_NON_TRAINABLE),
            capture_backend: self.capture_backend.clone(),
            encoder_backend: self.encoder_backend.clone(),
            width: self.width,
            height: self.height,
            fps_num: self.fps_num,
            fps_den: self.fps_den,
            bitrate_bps: self.bitrate_bps,
            max_bitrate_bps: self.max_bitrate_bps,
            gop_frames: self.gop_frames,
            segment_seconds: self.segment_seconds,
            test_segments: self.test_segments,
            accepted_encoder_relaxations: self.accepted_encoder_relaxations.clone(),
            audio: self.audio.clone(),
        }
    }
}

/// 视角尺度标定的目录政策（docs/45 §4.1、docs/46 §2.1）。
///
/// `applicable=false` 用于指针驱动型游戏（如 RTS）：显式声明"不适用"，
/// 与"忘了 authoring"可区分；此时不得携带任何标定常数。数值字段在
/// H-1 实验定版前保持 null——按 #225 先例，未知就是 null，不填猜测值。
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ViewScalePolicy {
    pub applicable: bool,
    /// 腰射转满 360° 的目录常数。
    pub counts_per_360: Option<f64>,
    /// FOV 记录轴（"horizontal_16_9" / "horizontal_4_3" / "vertical"），
    /// 与 `field_of_view_degrees` 配套；只记数字不记轴将来对不上。
    pub fov_axis: Option<String>,
    /// m_pitch / m_yaw 比值；1.0 = 垂直与水平各向同性（H-1 验证）。
    pub vertical_scale_ratio: Option<f64>,
    /// 会出现在训练数据中的开镜状态，各自单独标定，不合并。
    #[serde(default)]
    pub ads_states: Vec<ViewScaleAdsState>,
    /// 政策要求的游戏内开关（具体键名与档位由 H-1 对设置页逐项核对后定稿）。
    pub input_requirements: Option<ViewScaleInputRequirements>,
    /// 对表合格线：重复中位差 / 左右差（百分比）。
    pub tolerance_pct: Option<f64>,
    /// 快慢档差异合格线（百分比）；超线按加速/平滑嫌疑直接不通过。
    pub fast_slow_tolerance_pct: Option<f64>,
    /// 鼠标回报率硬下限（Hz）。
    pub min_polling_hz: Option<f64>,
    /// 相对游戏配置根的监视路径；空 = 监视缺席（如实记录，不假装在看）。
    #[serde(default)]
    pub config_watch_paths: Vec<String>,
    /// 全量标定有效期（天）。
    pub max_calibration_age_days: Option<u32>,
}

/// 单个开镜状态的政策条目。
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ViewScaleAdsState {
    pub state_id: String,
    pub counts_per_360: Option<f64>,
    pub fov_degrees: Option<f64>,
}

/// 政策要求的游戏内输入相关开关状态。None = 该项对本游戏未裁决。
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ViewScaleInputRequirements {
    pub hold_to_zoom: Option<bool>,
    pub mouse_smoothing: Option<bool>,
    pub mouse_acceleration: Option<bool>,
    pub minimum_frame_rate_off: Option<bool>,
    pub resolution_scale_100: Option<bool>,
}

#[derive(Clone)]
struct CatalogGame {
    profile: GameProfile,
    snapshot: CatalogEntrySnapshot,
    catalog: CatalogIdentity,
    /// 这一款所属 bundle 的 schema 世代。逐游戏保留，而不是 store 一份：
    /// 每游戏一份 bundle、每游戏独立切 current，混代窗口期是必然而不是异常。
    generation: CatalogSchemaGeneration,
    catalog_source_bytes: Arc<Vec<u8>>,
}

impl CatalogGame {
    /// 这一款游戏的采集要按哪一代摄取契约走（issue #855 §B / §D）。
    ///
    /// 两个条件**同时**满足才是中性 v2：
    ///
    /// * `provenance == Delivered` —— 目录是服务端签名下发的 managed 目录。编译内嵌
    ///   （`local-catalog-only` 与 All Games DEV 的内置册子）永远留在 legacy v1，
    ///   永久 `non_trainable`，不为本地 E2E 另造 v2 变体（§D 原话）。
    /// * `generation == Neutral` —— 这一款的 current release 已经是中性 schema。
    ///
    /// 第二个条件同时是**部署顺序的闸**：切 current 是人的动作，所以客户端先发布
    /// 也不会自己跑到 v2 去。旧客户端拉到中性 release 会在装载时就拒（它不认
    /// `dm-catalog-bundle-3`），新客户端拉到旧 release 则继续写 legacy v1 ——两个
    /// 方向都不会产出一份服务端还没准入的会话。
    fn session_contract(&self, provenance: CatalogProvenance) -> SessionContract {
        match (provenance, self.generation) {
            (CatalogProvenance::Delivered, CatalogSchemaGeneration::Neutral) => {
                SessionContract::IngestV2
            }
            _ => SessionContract::LocalDevV1,
        }
    }
}

#[derive(Clone)]
struct RecordingEntry {
    profile: RecordingProfile,
    /// 加载时算一次的引擎投影，避免每次开录再拼一遍。
    capture: CaptureProfile,
    snapshot: CatalogEntrySnapshot,
}

#[derive(Clone)]
struct DeviceEntry {
    profile: DeviceProfile,
    snapshot: CatalogEntrySnapshot,
}

#[derive(Clone)]
struct MappingEntry {
    mapping: MappingSnapshot,
    snapshot: CatalogEntrySnapshot,
}

#[derive(Clone)]
struct DepthProfileEntry {
    schema_version: String,
    executable_sha256: String,
    #[cfg(feature = "depth-lab")]
    profile: depth_lab::DepthProfile,
    snapshot: CatalogEntrySnapshot,
}

#[derive(Clone)]
struct CameraBindingEntry {
    profile_version: String,
    executable_sha256: String,
    #[cfg(feature = "depth-lab")]
    binding: depth_lab::CameraBindingDocument,
    snapshot: CatalogEntrySnapshot,
}

pub struct CatalogStore {
    /// How this store's bytes reached the client. `from_sources` is the
    /// `include_bytes!` path, so it defaults to `Compiled`; the signed-delivery
    /// entry point overwrites it. Recorded into every session's catalog context
    /// so a capture can be traced back to a source of truth (docs/54 §6.2).
    /// Never used as a gate.
    provenance: CatalogProvenance,
    catalogs: Vec<CatalogIdentity>,
    games: HashMap<String, CatalogGame>,
    devices: HashMap<String, DeviceEntry>,
    recordings: HashMap<String, RecordingEntry>,
    /// 按 id 建索引，不带版本（docs/57 §3.2）：一份目录内每个 id 至多一个版本，
    /// 一份 bundle 内重复 id 在 `from_sources` 直接报错；跨 bundle 的同 id 见
    /// `merge_same_provenance`（同字节去重、不同字节 fail-closed）。
    mappings: HashMap<String, MappingEntry>,
    depth_profiles: HashMap<String, DepthProfileEntry>,
    camera_bindings: HashMap<String, CameraBindingEntry>,
}

struct CatalogSources<'a> {
    manifest: &'a [u8],
    devices: &'a [&'a [u8]],
    recordings: &'a [&'a [u8]],
    games: &'a [&'a [u8]],
    mappings: &'a [&'a [u8]],
    depth_profiles: &'a [&'a [u8]],
    camera_bindings: &'a [&'a [u8]],
}

/// Validate an on-disk catalog bundle through the exact loader used by the
/// client. Release publishing uses the content-addressed layout
/// `catalog.json` + `payloads/sha256/<digest>.json`; the legacy six-file layout
/// went away with the `first_party_release` disk channel (docs/57 §3.3).
#[allow(dead_code)]
pub(crate) fn validate_disk_bundle(dir: &Path) -> anyhow::Result<()> {
    CatalogStore::from_content_addressed_disk_bundle(dir).map(|_| ())
}

/// Domain-separation tag for catalog manifest signatures. Versioned so the signing
/// format can evolve; the trailing NUL keeps it unambiguous as a prefix.
const CATALOG_SIGNATURE_DOMAIN: &[u8] = b"dm-signed-catalog-1\x00";

/// Byte payloads for a server-delivered, signed catalog bundle. `signature` is a
/// 64-byte Ed25519 signature over `CATALOG_SIGNATURE_DOMAIN || manifest`. Because
/// the manifest pins every profile's exact-byte `content_hash`, signing the
/// manifest transitively authenticates all bundled profiles (checked by the reused
/// `from_sources` validation). See `docs/16-signed-catalog-delivery.md`.
pub struct SignedCatalogSources<'a> {
    pub manifest: &'a [u8],
    pub signature: &'a [u8],
    pub devices: &'a [&'a [u8]],
    pub recordings: &'a [&'a [u8]],
    pub games: &'a [&'a [u8]],
    pub mappings: &'a [&'a [u8]],
    pub depth_profiles: &'a [&'a [u8]],
    pub camera_bindings: &'a [&'a [u8]],
}

fn read_regular_file_beneath(root: &Path, candidate: &Path) -> anyhow::Result<Vec<u8>> {
    let link_metadata = std::fs::symlink_metadata(candidate)
        .with_context(|| format!("failed to inspect catalog file {}", candidate.display()))?;
    ensure!(
        !link_metadata.file_type().is_symlink(),
        "catalog file must not be a symlink: {}",
        candidate.display()
    );
    let resolved = std::fs::canonicalize(candidate)
        .with_context(|| format!("failed to resolve catalog file {}", candidate.display()))?;
    ensure!(
        resolved.starts_with(root),
        "catalog file escapes bundle root: {}",
        candidate.display()
    );
    let metadata = std::fs::metadata(&resolved)
        .with_context(|| format!("failed to inspect catalog file {}", resolved.display()))?;
    ensure!(
        metadata.is_file(),
        "catalog payload is not a regular file: {}",
        resolved.display()
    );
    std::fs::read(&resolved)
        .with_context(|| format!("failed to read catalog file {}", resolved.display()))
}

/// Pinned Ed25519 trust anchor for signed catalogs: the public half of the delivery
/// key held only by the catalog signing tooling. Bundles served on this channel are
/// permanently `non_trainable`. Not a KMS key — it survived the certified lane's
/// retirement precisely because it never depended on one (docs/100 §4.5).
/// See `docs/16-signed-catalog-delivery.md`.
const DEV_CATALOG_ANCHOR: [u8; 32] = [
    0x58, 0x53, 0x16, 0xe5, 0xa8, 0xa6, 0xc4, 0x1c, 0x03, 0xd2, 0x80, 0xbf, 0xe1, 0x40, 0x9e, 0x41,
    0xa3, 0xd5, 0xf3, 0xc9, 0x08, 0x33, 0x37, 0xbb, 0x78, 0xf1, 0xe6, 0x26, 0x96, 0xc3, 0x9f, 0x82,
];

/// 签名下发的**唯一**信任锚。
///
/// 它曾被一层 `pinned_anchor_for_mode(mode)` 包着——那是 certified 时代"按渠道选锚"
/// 的残骸，车道退役后（docs/100 §4.5）早已是一个单臂 match。PR 1 先把这个函数抽了
/// 出来，正是为了 `CatalogMode` 退役时选锚逻辑不悬空（issue #855 §A.4）；现在那个词
/// 走了，这里一个字节都没动。
///
/// 锚从来不是"按 manifest 声称的东西选"的：一份被换了通道的 bundle 验不过就是验不过。
fn pinned_delivery_anchor() -> anyhow::Result<VerifyingKey> {
    VerifyingKey::from_bytes(&DEV_CATALOG_ANCHOR).context("pinned catalog trust anchor is invalid")
}

/// Verify another delivery-plane payload with the same pinned trust anchor while keeping a
/// distinct domain separator. Depth performance certifications use this rather than introducing
/// a second key distribution system or allowing callers to supply a key.
#[cfg(feature = "depth-lab")]
pub(crate) fn verify_delivery_payload_signature(
    domain: &[u8],
    payload: &[u8],
    signature: &[u8],
) -> anyhow::Result<()> {
    let signature = Signature::from_slice(signature)
        .context("delivery payload signature is not a valid Ed25519 signature")?;
    let mut signing_input = Vec::with_capacity(domain.len() + payload.len());
    signing_input.extend_from_slice(domain);
    signing_input.extend_from_slice(payload);
    pinned_delivery_anchor()?
        .verify_strict(&signing_input, &signature)
        .context("delivery payload signature verification failed")
}

impl CatalogStore {
    pub fn load() -> anyhow::Result<Self> {
        #[cfg(not(feature = "dev-catalog"))]
        {
            Ok(Self::empty())
        }

        #[cfg(feature = "dev-catalog")]
        {
            let mut store = Self::from_dev_sources(CatalogSources {
                manifest: SYNTHETIC_CATALOG_BYTES,
                devices: &[SYNTHETIC_DEVICE_BYTES],
                recordings: &[SYNTHETIC_RECORDING_BYTES],
                games: &[SYNTHETIC_GAME_BYTES],
                mappings: &[SYNTHETIC_MAPPING_BYTES],
                depth_profiles: &[],
                camera_bindings: &[],
            })?;
            // 内嵌游戏清单由 build.rs 从 shipped-games.json 派生（见文件头的
            // include!）。merge_same_provenance 保持同来源去重 / 冲突 fail-closed
            // 语义；哪些目录在场由 manifest 唯一决定，这里不做任何逐游戏判断。
            for sources in embedded_dev_catalogs() {
                store.merge_same_provenance(Self::from_dev_sources(sources)?)?;
            }
            Ok(store)
        }
    }

    /// 空基线：装机后、第一次成功下发前的状态。
    ///
    /// 从前这是一份编译内嵌的空 manifest（`config/catalogs/certified/catalog.json`）。
    /// 一份零条目的 manifest 不是 authored 内容，是硬编码基线；把它当文件放在
    /// bundle 目录下还会和真正的每游戏 bundle 混在一起（docs/57 §3.3）。
    ///
    /// 零条目意味着任何游戏都启动不了——空目录本身就是 fail-closed，与字节从哪来
    /// 无关。它自称 `Compiled`：这份基线确实是二进制自带的，没有任何一段验过签。
    // dev-catalog release 会把唯一生产调用点编译掉；测试仍保留 fail-closed 基线。
    #[cfg(any(not(feature = "dev-catalog"), test))]
    #[must_use]
    pub fn empty() -> Self {
        Self {
            provenance: CatalogProvenance::Compiled,
            catalogs: Vec::new(),
            games: HashMap::new(),
            devices: HashMap::new(),
            recordings: HashMap::new(),
            mappings: HashMap::new(),
            depth_profiles: HashMap::new(),
            camera_bindings: HashMap::new(),
        }
    }

    /// Verify an Ed25519 signature over the bundle manifest against a pinned public
    /// key, then reuse the exact-byte `from_sources` validation. Any failure returns
    /// `Err`, keeping callers fail-closed: a bundle that does not verify against the
    /// pinned key — or whose profile bytes do not match the signed manifest's content
    /// hashes — is never loaded.
    ///
    /// This is the P1 verify core from `docs/16-signed-catalog-delivery.md`.
    pub fn load_from_signed_bundle(
        pinned_key: &VerifyingKey,
        sources: SignedCatalogSources<'_>,
    ) -> anyhow::Result<Self> {
        let signature = Signature::from_slice(sources.signature)
            .context("catalog signature is not a valid Ed25519 signature")?;
        let mut signing_input =
            Vec::with_capacity(CATALOG_SIGNATURE_DOMAIN.len() + sources.manifest.len());
        signing_input.extend_from_slice(CATALOG_SIGNATURE_DOMAIN);
        signing_input.extend_from_slice(sources.manifest);
        pinned_key
            .verify_strict(&signing_input, &signature)
            .context("catalog manifest signature verification failed")?;
        // The signature covers the manifest, which pins every profile's exact-byte
        // content hash, so the reused content-hash checks transitively authenticate
        // all bundled profiles.
        let mut store = Self::from_sources(CatalogSources {
            manifest: sources.manifest,
            devices: sources.devices,
            recordings: sources.recordings,
            games: sources.games,
            mappings: sources.mappings,
            depth_profiles: sources.depth_profiles,
            camera_bindings: sources.camera_bindings,
        })?;
        // `from_sources` is shared with the `include_bytes!` path and defaults to
        // Compiled; these bytes came off the wire.
        store.provenance = CatalogProvenance::Delivered;
        Ok(store)
    }

    /// Verify a server-delivered signed bundle against the pinned trust anchor, then
    /// load it. A bundle signed by anything else simply fails verification
    /// (docs/16 §3, §6). The trust anchor lives in the binary, so callers do not
    /// choose keys — and there is no longer a channel argument to choose one with.
    pub fn load_signed_bundle(sources: SignedCatalogSources<'_>) -> anyhow::Result<Self> {
        let anchor = pinned_delivery_anchor()?;
        Self::load_from_signed_bundle(&anchor, sources)
    }

    fn from_content_addressed_disk_bundle(dir: &Path) -> anyhow::Result<Self> {
        let root = std::fs::canonicalize(dir)
            .with_context(|| format!("failed to resolve catalog bundle {}", dir.display()))?;
        let manifest_path = root.join("catalog.json");
        let manifest = read_regular_file_beneath(&root, &manifest_path)?;
        let parsed: CatalogManifest =
            serde_json::from_slice(&manifest).context("invalid catalog manifest")?;

        let read_entries = |entries: &[EntryReference]| -> anyhow::Result<Vec<Vec<u8>>> {
            entries
                .iter()
                .map(|reference| {
                    let digest = reference
                        .content_hash
                        .strip_prefix("sha256:")
                        .context("catalog entry content_hash must use sha256")?;
                    ensure!(
                        digest.len() == 64
                            && digest
                                .bytes()
                                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
                        "catalog entry content_hash must be lowercase sha256 hex"
                    );
                    let payload_path = root
                        .join("payloads")
                        .join("sha256")
                        .join(format!("{digest}.json"));
                    read_regular_file_beneath(&root, &payload_path)
                })
                .collect()
        };

        let devices = read_entries(&parsed.device_profiles)?;
        let recordings = read_entries(&parsed.recording_profiles)?;
        let games = read_entries(&parsed.game_profiles)?;
        let mappings = read_entries(&parsed.mappings)?;
        let depth_profiles = read_entries(&parsed.depth_profiles)?;
        let camera_bindings = read_entries(&parsed.camera_bindings)?;
        let device_slices = devices.iter().map(Vec::as_slice).collect::<Vec<_>>();
        let recording_slices = recordings.iter().map(Vec::as_slice).collect::<Vec<_>>();
        let game_slices = games.iter().map(Vec::as_slice).collect::<Vec<_>>();
        let mapping_slices = mappings.iter().map(Vec::as_slice).collect::<Vec<_>>();
        let depth_profile_slices = depth_profiles.iter().map(Vec::as_slice).collect::<Vec<_>>();
        let camera_binding_slices = camera_bindings
            .iter()
            .map(Vec::as_slice)
            .collect::<Vec<_>>();

        // 离线校验没有信任锚可用（还没签名呢），只能看 manifest 自称的分类。这里是
        // authoring 期的自洽检查，不是信任决定——信任决定在 `load_signed_bundle`。
        // certified 车道废除后（docs/100）`non_trainable: false` 没有下发路径可去了：
        // 这样一份 bundle 在任何通道上都验不过，authoring 期就该直说，而不是先放行
        // 再让它死在别处。
        //
        // 中性一代（`None`）不适用：它不声明分类，没有可自洽的自称。它是否合法由下面
        // `from_sources` 的代际闸判定——带了旧字段就在那里被拒。
        if let Some(declared) = parsed.non_trainable {
            ensure!(
                declared,
                "catalog bundle declares non_trainable=false, which no delivery channel accepts (docs/100)"
            );
        }
        Self::from_sources(CatalogSources {
            manifest: manifest.as_slice(),
            devices: &device_slices,
            recordings: &recording_slices,
            games: &game_slices,
            mappings: &mapping_slices,
            depth_profiles: &depth_profile_slices,
            camera_bindings: &camera_binding_slices,
        })
    }

    /// 装载一份**编译内嵌**的目录。内嵌字节不验签——它们本来就是这个二进制的一部分，
    /// 签名要防的是传输路上被换掉，而这条路上没有传输。
    #[cfg(feature = "dev-catalog")]
    fn from_dev_sources(sources: CatalogSources<'_>) -> anyhow::Result<Self> {
        Self::from_sources(sources)
    }

    /// 装载一份 bundle。`provenance` 由调用方按自己走的那条路覆盖：这里默认
    /// `Compiled`，`load_from_signed_bundle` 验签成功后改写成 `Delivered`。
    fn from_sources(sources: CatalogSources<'_>) -> anyhow::Result<Self> {
        let manifest: CatalogManifest =
            serde_json::from_slice(sources.manifest).context("invalid catalog manifest")?;
        let generation = validate_manifest(&manifest)?;
        let identity = CatalogIdentity {
            catalog_id: manifest.catalog_id.clone(),
            catalog_version: manifest.catalog_version.clone(),
            content_hash: sha256(sources.manifest),
        };
        let mut store = Self {
            provenance: CatalogProvenance::Compiled,
            catalogs: vec![identity.clone()],
            games: HashMap::new(),
            devices: HashMap::new(),
            recordings: HashMap::new(),
            mappings: HashMap::new(),
            depth_profiles: HashMap::new(),
            camera_bindings: HashMap::new(),
        };

        ensure!(
            sources.devices.len() == manifest.device_profiles.len()
                && sources.recordings.len() == manifest.recording_profiles.len()
                && sources.games.len() == manifest.game_profiles.len()
                && sources.mappings.len() == manifest.mappings.len()
                && sources.depth_profiles.len() == manifest.depth_profiles.len()
                && sources.camera_bindings.len() == manifest.camera_bindings.len(),
            "catalog manifest entry counts do not match bundled payloads"
        );

        for (reference, bytes) in manifest.device_profiles.iter().zip(sources.devices) {
            let profile: DeviceProfile =
                serde_json::from_slice(bytes).context("invalid device profile")?;
            validate_device(&profile, reference, generation)?;
            let snapshot = snapshot(reference, bytes)?;
            ensure!(
                store
                    .devices
                    .insert(reference.id.clone(), DeviceEntry { profile, snapshot })
                    .is_none(),
                "duplicate device profile {}",
                reference.id
            );
        }
        for (reference, bytes) in manifest.recording_profiles.iter().zip(sources.recordings) {
            let profile: RecordingProfile =
                serde_json::from_slice(bytes).context("invalid recording profile")?;
            validate_recording(&profile, reference, generation)?;
            // 引擎参数的权威仍是 capture-core 自己的 validate()（§3.1 的 crate 边界）。
            let capture = profile.capture_profile();
            capture.validate().with_context(|| {
                format!(
                    "recording profile {} carries invalid engine parameters",
                    reference.id
                )
            })?;
            let snapshot = snapshot(reference, bytes)?;
            ensure!(
                store
                    .recordings
                    .insert(
                        reference.id.clone(),
                        RecordingEntry {
                            profile,
                            capture,
                            snapshot,
                        },
                    )
                    .is_none(),
                "duplicate recording profile {}",
                reference.id
            );
        }
        for (reference, bytes) in manifest.mappings.iter().zip(sources.mappings) {
            let mapping = MappingSnapshot::from_slice(bytes)?;
            ensure!(
                mapping.mapping().mapping_id == reference.id
                    && mapping.mapping().version == reference.version,
                "mapping reference identity mismatch"
            );
            let snapshot = snapshot(reference, bytes)?;
            ensure!(
                store
                    .mappings
                    .insert(reference.id.clone(), MappingEntry { mapping, snapshot })
                    .is_none(),
                "duplicate mapping {}",
                reference.id
            );
        }
        for (reference, bytes) in manifest.game_profiles.iter().zip(sources.games) {
            let profile: GameProfile =
                serde_json::from_slice(bytes).context("invalid game profile")?;
            validate_game(&profile, reference, generation)?;
            let snapshot = snapshot(reference, bytes)?;
            ensure!(
                store
                    .games
                    .insert(
                        reference.id.clone(),
                        CatalogGame {
                            profile,
                            snapshot,
                            catalog: identity.clone(),
                            generation,
                            catalog_source_bytes: Arc::new(sources.manifest.to_vec()),
                        },
                    )
                    .is_none(),
                "duplicate game profile {}",
                reference.id
            );
        }
        for (reference, bytes) in manifest.depth_profiles.iter().zip(sources.depth_profiles) {
            let value: serde_json::Value =
                serde_json::from_slice(bytes).context("invalid depth profile")?;
            #[cfg(feature = "depth-lab")]
            let profile: depth_lab::DepthProfile =
                serde_json::from_slice(bytes).context("invalid depth profile")?;
            let schema_version = required_json_string(&value, "schema_version", "depth profile")?;
            ensure!(
                matches!(
                    schema_version.as_str(),
                    "dm-depth-profile-1" | "dm-depth-profile-2" | "dm-depth-profile-3"
                ),
                "unsupported depth profile schema {schema_version}"
            );
            let game_id = required_json_string(&value, "game_id", "depth profile")?;
            let executable_sha256 =
                required_json_string(&value, "executable_sha256", "depth profile")?;
            ensure!(
                canonical_sha256(&executable_sha256),
                "depth profile executable_sha256 must be lowercase sha256 hex"
            );
            ensure!(
                reference.id == game_id,
                "depth profile reference identity mismatch"
            );
            let snapshot = snapshot(reference, bytes)?;
            ensure!(
                store
                    .depth_profiles
                    .insert(
                        game_id,
                        DepthProfileEntry {
                            schema_version,
                            executable_sha256,
                            #[cfg(feature = "depth-lab")]
                            profile,
                            snapshot,
                        },
                    )
                    .is_none(),
                "duplicate depth profile {}",
                reference.id
            );
        }
        for (reference, bytes) in manifest.camera_bindings.iter().zip(sources.camera_bindings) {
            let value: serde_json::Value =
                serde_json::from_slice(bytes).context("invalid camera binding")?;
            #[cfg(feature = "depth-lab")]
            let binding: depth_lab::CameraBindingDocument =
                serde_json::from_slice(bytes).context("invalid camera binding")?;
            let schema = required_json_string(&value, "schema", "camera binding")?;
            ensure!(
                matches!(
                    schema.as_str(),
                    "camera-binding/v1"
                        | "camera-binding/v2"
                        | "camera-binding/cp2077-holstered-v3"
                        | "camera-binding/kcd2-combined-v1"
                        | "camera-binding/kcd2-combined-v2"
                ),
                "unsupported camera binding schema {schema}"
            );
            let binding_id = required_json_string(&value, "binding_id", "camera binding")?;
            let binding_version =
                required_json_string(&value, "binding_version", "camera binding")?;
            let profile_id = required_json_string(&value, "profile_id", "camera binding")?;
            let profile_version =
                required_json_string(&value, "profile_version", "camera binding")?;
            let executable_sha256 =
                required_json_string(&value, "executable_sha256", "camera binding")?;
            ensure!(
                canonical_sha256(&executable_sha256),
                "camera binding executable_sha256 must be lowercase sha256 hex"
            );
            ensure!(
                reference.id == binding_id && reference.version == binding_version,
                "camera binding reference identity mismatch"
            );
            #[cfg(feature = "depth-lab")]
            binding
                .validate_for_executable(&executable_sha256)
                .map_err(anyhow::Error::msg)
                .context("camera binding failed production validation")?;
            let snapshot = snapshot(reference, bytes)?;
            ensure!(
                store
                    .camera_bindings
                    .insert(
                        profile_id.clone(),
                        CameraBindingEntry {
                            profile_version,
                            executable_sha256,
                            #[cfg(feature = "depth-lab")]
                            binding,
                            snapshot,
                        },
                    )
                    .is_none(),
                "duplicate camera binding for depth profile {profile_id}"
            );
        }
        store.validate_cross_references()?;
        Ok(store)
    }

    /// 把另一份已加载 bundle 并进来。
    ///
    /// 两条路都用它：编译内嵌的 dev 目录一份一份叠起来，以及集合下发把全部已发布
    /// bundle 合成一个 store（docs/57 §3.3）。**来源必须相同**——provenance 是信任
    /// 决定的产物（验签过 / 没验签过），把两者混进一个索引等于让一款未验签的内嵌
    /// 目录搭上下发集合的信任。断代前这条不变量写在 `mode` 上，那个词同时兼职表达
    /// 环境与训练资格；换成 provenance 之后它表达的正好是原本要表达的那件事。
    ///
    /// **schema 世代不要求相同**：每游戏一份 bundle、每游戏独立切 current，所以
    /// 30 款断代的窗口期里集合必然混代。契约因此是逐游戏的
    /// （[`CatalogGame::session_contract`]），不是 store 级的。
    ///
    /// 两份 bundle 共用同一个条目是**正常**的，不是冲突：多份每游戏 bundle 引用同一
    /// 个机群 device profile 就是这样。判据是完整签名身份（id/version/content_hash）：
    /// 三者一致就去重，任一不同都说明两份 bundle 对同一个 id 有不同说法，fail-closed，
    /// 因为这种分歧没有哪一边更可信。
    pub(crate) fn merge_same_provenance(&mut self, extra: Self) -> anyhow::Result<()> {
        ensure!(
            self.provenance == extra.provenance,
            "catalog_provenance_mismatch: compiled and delivered catalogs cannot be merged"
        );
        for game_id in self.games.keys().filter(|id| extra.games.contains_key(*id)) {
            let held_depth = self
                .depth_profiles
                .get(game_id)
                .map(|entry| &entry.snapshot.identity);
            let incoming_depth = extra
                .depth_profiles
                .get(game_id)
                .map(|entry| &entry.snapshot.identity);
            let held_binding = self
                .camera_bindings
                .get(game_id)
                .map(|entry| &entry.snapshot.identity);
            let incoming_binding = extra
                .camera_bindings
                .get(game_id)
                .map(|entry| &entry.snapshot.identity);
            ensure!(
                held_depth == incoming_depth && held_binding == incoming_binding,
                "catalogs that share game {game_id} must bind the same depth pair identities"
            );
        }
        for identity in &extra.catalogs {
            ensure!(
                !self
                    .catalogs
                    .iter()
                    .any(|held| held.catalog_id == identity.catalog_id),
                "catalog {} is delivered twice",
                identity.catalog_id
            );
        }
        self.catalogs.extend(extra.catalogs);
        // 定序。`try_activate_catalog` 拿 `active_catalogs` 的相等性判断「这份是不是
        // 已经在跑的那份」，顺序参与比较——不定序的话下发顺序一变就会白激活一次。
        self.catalogs
            .sort_by(|left, right| left.catalog_id.cmp(&right.catalog_id));
        merge_entries(&mut self.devices, extra.devices, "device", |entry| {
            &entry.snapshot
        })?;
        merge_entries(
            &mut self.recordings,
            extra.recordings,
            "recording",
            |entry| &entry.snapshot,
        )?;
        merge_entries(&mut self.mappings, extra.mappings, "mapping", |entry| {
            &entry.snapshot
        })?;
        merge_entries(&mut self.games, extra.games, "game", |entry| {
            &entry.snapshot
        })?;
        merge_entries(
            &mut self.depth_profiles,
            extra.depth_profiles,
            "depth profile",
            |entry| &entry.snapshot,
        )?;
        merge_entries(
            &mut self.camera_bindings,
            extra.camera_bindings,
            "camera binding",
            |entry| &entry.snapshot,
        )?;
        self.validate_cross_references()
    }

    fn validate_cross_references(&self) -> anyhow::Result<()> {
        for game in self.games.values() {
            let profile = &game.profile;
            let device = self
                .devices
                .get(&profile.device_profile_id)
                .with_context(|| {
                    format!("game {} references no device profile", profile.game_id)
                })?;
            let recording = self
                .recordings
                .get(&profile.recording_profile_id)
                .with_context(|| {
                    format!("game {} references no recording profile", profile.game_id)
                })?;
            let mapping = self
                .mappings
                .get(&profile.mapping_id)
                .with_context(|| format!("game {} references no mapping", profile.game_id))?;
            // 合并一册之后，「宽高帧率两处必须相等」这条交叉校验连同它保护的那份重复
            // 一起消失了——它们现在本来就只有一份（docs/57 §3.1）。
            ensure!(
                device.profile.encoder_backend == recording.profile.encoder_backend
                    && recording.profile.game_id == profile.game_id
                    && mapping.mapping.mapping().game.id == profile.game_id
                    && mapping.mapping.mapping().recording_profile_id
                        == profile.recording_profile_id,
                "game {} catalog cross-reference validation failed",
                profile.game_id
            );
            validate_bindings_reach_frames(profile, mapping.mapping.mapping())?;

            let depth_profile = self.depth_profiles.get(&profile.game_id);
            let camera_binding = self.camera_bindings.get(&profile.game_id);
            ensure!(
                depth_profile.is_some() == camera_binding.is_some(),
                "game {} catalog depth pair must contain both depth profile and camera binding",
                profile.game_id
            );
            if let (Some(depth_profile), Some(camera_binding)) = (depth_profile, camera_binding) {
                ensure!(
                    camera_binding.profile_version == depth_profile.schema_version,
                    "game {} camera binding targets a different depth profile schema",
                    profile.game_id
                );
                ensure!(
                    camera_binding.executable_sha256 == depth_profile.executable_sha256,
                    "game {} depth pair pins different executable builds",
                    profile.game_id
                );
                if let Some(game_executable_sha256) = &profile.executable_sha256 {
                    ensure!(
                        game_executable_sha256 == &depth_profile.executable_sha256,
                        "game {} profile and depth pair pin different executable builds",
                        profile.game_id
                    );
                }
                #[cfg(feature = "depth-lab")]
                {
                    camera_binding
                        .binding
                        .validate_for_profile(
                            &depth_profile.profile,
                            &depth_profile.executable_sha256,
                        )
                        .map_err(anyhow::Error::msg)?;
                }
            }
        }
        for game_id in self.depth_profiles.keys() {
            ensure!(
                self.games.contains_key(game_id),
                "depth profile {game_id} references no game"
            );
            ensure!(
                self.camera_bindings.contains_key(game_id),
                "depth profile {game_id} has no camera binding"
            );
        }
        for profile_id in self.camera_bindings.keys() {
            ensure!(
                self.depth_profiles.contains_key(profile_id),
                "camera binding references no depth profile {profile_id}"
            );
        }
        Ok(())
    }

    #[must_use]
    pub fn status(&self) -> CatalogStatus {
        CatalogStatus {
            schema_version: "dm-catalog-status-4",
            provenance: self.provenance,
            active_catalogs: self.catalogs.clone(),
            entry_counts: CatalogEntryCounts {
                devices: count(self.devices.len()),
                recordings: count(self.recordings.len()),
                games: count(self.games.len()),
                mappings: count(self.mappings.len()),
                depth_profiles: count(self.depth_profiles.len()),
                camera_bindings: count(self.camera_bindings.len()),
            },
            // 空目录必须自称空。release 构建装机后、首次成功下发前就是这个状态，
            // 而那时编排器的 refresh_catalog_runtime_status 还没跑过——`status()`
            // 自己说不清就意味着**首个快照会谎报可用**。generation 两项仍是基线，
            // 它们的真实值确实只有编排器知道（docs/65 J-15）。
            result: if self.games.is_empty() {
                "catalog_empty"
            } else {
                // 旧值是 `dev_non_trainable_available`——一个把车道名写进"目录装上了
                // 没有"里的词（issue #855 §A）。它只回答可用性。
                "catalog_available"
            },
            active_generation: 0,
            cached_generation: 0,
        }
    }

    pub(crate) fn games(&self) -> impl Iterator<Item = &GameProfile> {
        self.games.values().map(|game| &game.profile)
    }

    pub(crate) fn game(&self, game_id: &str) -> Option<&GameProfile> {
        self.games.get(game_id).map(|game| &game.profile)
    }

    pub(crate) fn capture(&self, profile_id: &str) -> Option<&CaptureProfile> {
        self.recordings.get(profile_id).map(|entry| &entry.capture)
    }

    #[cfg(all(test, feature = "dev-catalog"))]
    pub(crate) fn recording(&self, profile_id: &str) -> Option<&RecordingProfile> {
        self.recordings.get(profile_id).map(|entry| &entry.profile)
    }

    pub(crate) fn mapping(&self, mapping_id: &str) -> Option<&MappingSnapshot> {
        self.mappings.get(mapping_id).map(|entry| &entry.mapping)
    }

    pub(crate) fn selection(&self, game_id: &str) -> anyhow::Result<CatalogSelection> {
        let game = self
            .games
            .get(game_id)
            .with_context(|| format!("game {game_id} is not in the active catalog"))?;
        let profile = &game.profile;
        Ok(CatalogSelection {
            provenance: self.provenance,
            catalog: game.catalog.clone(),
            session_contract: game.session_contract(self.provenance),
            catalog_source_bytes: Arc::clone(&game.catalog_source_bytes),
            device: self
                .devices
                .get(&profile.device_profile_id)
                .context("selected device profile disappeared")?
                .snapshot
                .clone(),
            recording: self
                .recordings
                .get(&profile.recording_profile_id)
                .context("selected recording profile disappeared")?
                .snapshot
                .clone(),
            game: game.snapshot.clone(),
            mapping: self
                .mappings
                .get(&profile.mapping_id)
                .context("selected mapping disappeared")?
                .snapshot
                .clone(),
            depth_profile: self
                .depth_profiles
                .get(game_id)
                .map(|entry| entry.snapshot.clone()),
            camera_binding: self
                .camera_bindings
                .get(game_id)
                .map(|entry| entry.snapshot.clone()),
        })
    }

    /// True only when the game and every profile/mapping exact-byte identity used by
    /// an active session are unchanged. Catalog release/version may advance because
    /// another game changed; that does not mutate this session's frozen snapshots.
    #[cfg(not(feature = "local-catalog-only"))]
    pub(crate) fn preserves_selection(&self, active: &CatalogSelection) -> bool {
        self.selection(&active.game.identity.id)
            .is_ok_and(|candidate| active.same_profile_generation(&candidate))
    }

    /// 启动门禁：这个游戏必须在当前已验签目录里有活跃条目。
    ///
    /// grant 车道废除后（docs/100）这里不再问任何 provider——「本机能录哪些游戏」由
    /// 目录本身回答，而目录是服务端签名下发、只发给已登记设备的。
    pub(crate) fn ensure_start_authorized(&self, game_id: &str) -> anyhow::Result<()> {
        ensure!(
            self.games.contains_key(game_id),
            "CATALOG_NO_MATCH: game has no active catalog entry"
        );
        Ok(())
    }
}

fn required_json_string(
    value: &serde_json::Value,
    field: &'static str,
    payload: &'static str,
) -> anyhow::Result<String> {
    value
        .get(field)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned)
        .with_context(|| format!("{payload} has no {field}"))
}

fn validate_manifest(manifest: &CatalogManifest) -> anyhow::Result<CatalogSchemaGeneration> {
    // 双读（issue #855 PR 1）：旧 `dm-catalog-bundle-2` 仍然可读——它是今天的 current、
    // 回滚路径与三份 superseded 研究快照的格式；中性 `dm-catalog-bundle-3` 新增为可读。
    let generation = CatalogSchemaGeneration::of(
        &manifest.schema_version,
        "dm-catalog-bundle-2",
        "dm-catalog-bundle-3",
    )
    .context("unsupported catalog manifest schema")?;
    ensure!(
        !manifest.catalog_id.trim().is_empty() && !manifest.catalog_version.trim().is_empty(),
        "catalog identity is incomplete"
    );
    let declared = retired_training_field(generation, manifest.non_trainable, "catalog manifest")?;
    // legacy：一份自称可训练的 manifest 没有任何下发路径能收它（docs/100 之后已无
    // 第二条渠道），authoring 期就该直说。断代前这里比的是 `declared == mode.non_trainable()`
    // ——`mode` 恒 `DevNonTrainable`、`non_trainable()` 恒 true，所以那条判据逐字等于
    // 下面这一条，只是绕道了一个把三件事粘在一起的词。
    //
    // 中性一代没有这一步，而且**不是把门拆了**：这份 manifest 根本不声明分类，没有
    // 可对账的自称；"从哪条通道来"改由 `pinned_delivery_anchor` 验签回答。
    if let Some(declared) = declared {
        ensure!(
            declared,
            "catalog manifest declares non_trainable=false, which no delivery path accepts (docs/100)"
        );
    }
    for entries in [
        &manifest.device_profiles,
        &manifest.recording_profiles,
        &manifest.game_profiles,
        &manifest.mappings,
        &manifest.depth_profiles,
        &manifest.camera_bindings,
    ] {
        let mut identities = HashSet::new();
        for entry in entries {
            ensure!(
                !entry.id.trim().is_empty()
                    && !entry.version.trim().is_empty()
                    && valid_sha256(&entry.content_hash),
                "catalog reference identity is incomplete"
            );
            // 按 id 去重，不带版本（docs/57 §3.2）：一份目录内每个 id 至多一个版本。
            ensure!(
                identities.insert(entry.id.as_str()),
                "catalog claims {} twice; one catalog holds at most one version per id",
                entry.id
            );
        }
    }
    Ok(generation)
}

/// 整份 bundle 必须同代。半迁移（manifest 已断代、profile 还带旧字段）是最坏的一种
/// 状态：部分读者放行、部分读者拒绝，而错误信息指不到真正的原因。
fn assert_same_generation(
    manifest: CatalogSchemaGeneration,
    payload: CatalogSchemaGeneration,
    label: &str,
) -> anyhow::Result<()> {
    ensure!(
        manifest == payload,
        "catalog bundle mixes schema generations: manifest is {manifest:?} but {label} is {payload:?}; \
         a bundle migrates as one unit (issue #855)"
    );
    Ok(())
}

fn validate_device(
    profile: &DeviceProfile,
    reference: &EntryReference,
    manifest_generation: CatalogSchemaGeneration,
) -> anyhow::Result<()> {
    let generation = CatalogSchemaGeneration::of(
        &profile.schema_version,
        "dm-device-profile-2",
        "dm-device-profile-3",
    )
    .context("device profile identity mismatch")?;
    assert_same_generation(manifest_generation, generation, "device profile")?;
    retired_training_field(generation, profile.non_trainable, "device profile")?;
    ensure!(
        profile.profile_id == reference.id && profile.version == reference.version,
        "device profile identity mismatch"
    );
    ensure!(
        profile.platform == "windows"
            && profile.minimum_windows_build > 0
            && profile.adapter_vendor_id > 0
            && profile.encoder_backend == "mf_nvidia_hardware_v1"
            && profile.input_backend == "raw_input_v1"
            && profile.pointer_mode == "relative"
            && !profile.evidence_class.trim().is_empty(),
        "device profile constraints are incomplete"
    );
    if let Some(floor) = profile.minimum_driver_version.as_deref() {
        ensure!(
            parse_driver_version(floor).is_some(),
            "device profile minimum_driver_version must be four numeric segments"
        );
    }
    Ok(())
}

fn validate_recording(
    profile: &RecordingProfile,
    reference: &EntryReference,
    manifest_generation: CatalogSchemaGeneration,
) -> anyhow::Result<()> {
    let generation = CatalogSchemaGeneration::of(
        &profile.schema_version,
        "dm-recording-profile-1",
        "dm-recording-profile-2",
    )
    .context("recording profile identity mismatch")?;
    assert_same_generation(manifest_generation, generation, "recording profile")?;
    retired_training_field(generation, profile.non_trainable, "recording profile")?;
    ensure!(
        profile.profile_id == reference.id && profile.version == reference.version,
        "recording profile identity mismatch"
    );
    // 引擎参数（backend / 几何 / 帧率 / 码率 / GOP / 归一化包络）由 capture-core 的
    // `CaptureProfile::validate` 判定，调用点在 `from_sources`。这里只管游戏侧那半段。
    ensure!(
        !profile.game_id.trim().is_empty()
            && !profile.window_mode.trim().is_empty()
            && !profile.hdr
            && profile.cursor_capture,
        "recording profile violates the local capture contract"
    );
    if let Some(view_scale) = &profile.view_scale {
        validate_view_scale(view_scale)?;
    }
    Ok(())
}

/// view_scale 块的类型级自洽校验（C2）：只拦"写了但自相矛盾"。
/// 完备性（常数是否定版、状态是否覆盖）属于云端可训练审批门（C5），
/// 录制门禁属于 preflight（C4），都不在这里判定。
fn validate_view_scale(policy: &ViewScalePolicy) -> anyhow::Result<()> {
    let positive = |value: Option<f64>| value.is_none_or(|v| v.is_finite() && v > 0.0);
    if !policy.applicable {
        ensure!(
            policy.counts_per_360.is_none() && policy.ads_states.is_empty(),
            "VIEW_SCALE_NOT_APPLICABLE_WITH_CONSTANTS: view_scale declares \
             not-applicable but carries scale constants"
        );
        return Ok(());
    }
    ensure!(
        positive(policy.counts_per_360)
            && positive(policy.vertical_scale_ratio)
            && positive(policy.tolerance_pct)
            && positive(policy.fast_slow_tolerance_pct)
            && positive(policy.min_polling_hz),
        "VIEW_SCALE_NON_POSITIVE_VALUE: view_scale numeric fields must be positive"
    );
    ensure!(
        policy.max_calibration_age_days.is_none_or(|days| days >= 1),
        "VIEW_SCALE_INVALID_AGE: max_calibration_age_days must be at least 1"
    );
    ensure!(
        policy
            .fov_axis
            .as_deref()
            .is_none_or(|axis| matches!(axis, "horizontal_16_9" | "horizontal_4_3" | "vertical")),
        "VIEW_SCALE_UNKNOWN_FOV_AXIS: fov_axis must be a documented axis"
    );
    let mut seen_states = std::collections::HashSet::new();
    for state in &policy.ads_states {
        ensure!(
            !state.state_id.trim().is_empty(),
            "VIEW_SCALE_EMPTY_ADS_STATE: ads state_id must not be empty"
        );
        ensure!(
            seen_states.insert(state.state_id.as_str()),
            "VIEW_SCALE_DUPLICATE_ADS_STATE: ads state_id must be unique"
        );
        ensure!(
            positive(state.counts_per_360) && positive(state.fov_degrees),
            "VIEW_SCALE_NON_POSITIVE_VALUE: ads state numeric fields must be positive"
        );
    }
    ensure!(
        policy
            .config_watch_paths
            .iter()
            .all(|path| !path.trim().is_empty()),
        "VIEW_SCALE_EMPTY_WATCH_PATH: config_watch_paths entries must not be empty"
    );
    Ok(())
}

/// PS/2 Set-1 make code -> virtual key, for the subset any contract can declare.
///
/// The two planes name the same physical key differently: `mapping.json` binds
/// scan codes, `input_contract` lists VKs. Cross-checking them needs this
/// correspondence, so it lives in shipping code rather than only in tests.
const SET1_TO_VK: &[(u16, u16)] = &[
    (1, 27),
    (2, 49),
    (3, 50),
    (4, 51),
    (5, 52),
    (6, 53),
    (7, 54),
    (8, 55),
    (9, 56),
    (10, 57),
    (11, 48),
    (12, 189),
    (13, 187),
    (14, 8),
    (15, 9),
    (16, 81),
    (17, 87),
    (18, 69),
    (19, 82),
    (20, 84),
    (21, 89),
    (22, 85),
    (23, 73),
    (24, 79),
    (25, 80),
    (26, 219),
    (27, 221),
    (28, 13),
    (29, 17),
    (30, 65),
    (31, 83),
    (32, 68),
    (33, 70),
    (34, 71),
    (35, 72),
    (36, 74),
    (37, 75),
    (38, 76),
    (39, 186),
    (40, 222),
    (41, 192),
    (42, 16),
    (43, 220),
    (44, 90),
    (45, 88),
    (46, 67),
    (47, 86),
    (48, 66),
    (49, 78),
    (50, 77),
    (51, 188),
    (52, 190),
    (53, 191),
    (54, 16),
    (55, 106),
    (56, 18),
    (57, 32),
    (58, 20),
    (59, 112),
    (60, 113),
    (61, 114),
    (62, 115),
    (63, 116),
    (64, 117),
    (65, 118),
    (66, 119),
    (67, 120),
    (68, 121),
    (87, 122),
    (88, 123),
];
const SET1_EXTENDED_TO_VK: &[(u16, u16)] = &[
    (28, 13),
    (29, 17),
    (53, 111),
    (56, 18),
    (71, 36),
    (72, 38),
    (73, 33),
    (75, 37),
    (77, 39),
    (79, 35),
    (80, 40),
    (81, 34),
    (82, 45),
    (83, 46),
];

#[must_use]
fn virtual_key_for_scan_code(scan_code: u16, extended: bool) -> Option<u16> {
    let table = if extended {
        SET1_EXTENDED_TO_VK
    } else {
        SET1_TO_VK
    };
    table
        .iter()
        .find(|(scan, _)| *scan == scan_code)
        .map(|(_, vk)| *vk)
}

/// Refuse a catalog whose mapping binds a key its own contract will swallow.
///
/// Two independent filters stand between a keypress and a label: Raw Input drops
/// anything outside `keyboard_vks`, and the projector drops anything inside
/// `control_vks`. A binding on either side of those gates can never reach a
/// frame, and nothing downstream can tell that apart from "the player never
/// pressed it" — the COD corpus lost `move.prone` this way for a full session
/// series, and it took a corpus audit to notice. PR #215 fixed the committed
/// bundles and locked them with a test; this makes the same invariant a load
/// -time gate, so a bad bundle cannot reach a recording at all.
fn validate_bindings_reach_frames(
    profile: &GameProfile,
    mapping: &capture_core::mapping::GameMapping,
) -> anyhow::Result<()> {
    let contract = &profile.input_contract;
    for binding in &mapping.bindings {
        let Some(scan_code) = binding.scan_code else {
            continue;
        };
        let extended = binding.extended.unwrap_or(false);
        let Some(vk) = virtual_key_for_scan_code(scan_code, extended) else {
            continue;
        };
        ensure!(
            contract.allows_key(vk),
            "game {}: mapping binds scan {} (VK {}) to {}, but the key is missing from \
             keyboard_vks, so Raw Input drops it and the action is never emitted",
            profile.game_id,
            scan_code,
            vk,
            binding.semantic
        );
        ensure!(
            !contract.is_control_key(vk),
            "game {}: mapping binds scan {} (VK {}) to {}, but the key is declared in \
             control_vks, so projection skips it and the action is never emitted",
            profile.game_id,
            scan_code,
            vk,
            binding.semantic
        );
    }
    for button in mapping.bindings.iter().filter_map(|binding| binding.button) {
        ensure!(
            contract.allows_mouse_button(button),
            "game {}: mapping binds mouse button {} but it is missing from mouse_buttons",
            profile.game_id,
            button
        );
    }
    Ok(())
}

fn validate_game(
    profile: &GameProfile,
    reference: &EntryReference,
    manifest_generation: CatalogSchemaGeneration,
) -> anyhow::Result<()> {
    // Identity pins (window class, executable SHA-256) are optional evidence in
    // every catalog mode (owner decision, 2026-07-28): a game may be recognized
    // by executable name alone. When a pin IS authored it must be well-formed,
    // and the runtime still enforces it during discovery/bind (window_tracker
    // rejects a class mismatch and executable_cache refuses a hash mismatch).
    let generation = CatalogSchemaGeneration::of(
        &profile.schema_version,
        "dm-game-profile-2",
        "dm-game-profile-3",
    )
    .context("game profile identity mismatch")?;
    assert_same_generation(manifest_generation, generation, "game profile")?;
    retired_training_field(generation, profile.non_trainable, "game profile")?;
    ensure!(
        profile.game_id == reference.id && profile.version == reference.version,
        "game profile identity mismatch"
    );
    ensure!(
        !profile.display_name.trim().is_empty()
            && !profile.device_profile_id.trim().is_empty()
            && !profile.recording_profile_id.trim().is_empty()
            && !profile.mapping_id.trim().is_empty()
            && !profile.executable_names.is_empty(),
        "game profile identity or references are incomplete"
    );
    ensure!(
        profile
            .window_class
            .as_deref()
            .is_none_or(|value| !value.trim().is_empty()),
        "game window class must be null or non-empty"
    );
    ensure!(
        profile
            .executable_sha256
            .as_deref()
            .is_none_or(valid_sha256),
        "game executable hash must be null or a SHA-256 identity"
    );
    profile.input_contract.validate()
}

/// 见 [`CatalogStore::merge_same_provenance`]：同 id 同完整签名身份去重，身份不同 fail-closed。
///
/// 不带 `dev-catalog` 门：从前只有编译内嵌的目录会合并，现在集合下发也要合并
/// （docs/57 §3.3），release 构建同样用得到。
///
/// 比较完整的签名身份，而不只比较精确字节 hash。大多数 payload 自己携带 version，
/// 但 depth profile 的 authored version 只存在于已签名 manifest；同字节、不同 version
/// 仍是两个不同身份，必须 fail-closed，不能让投递顺序决定保留哪一个。
fn merge_entries<T>(
    destination: &mut HashMap<String, T>,
    source: HashMap<String, T>,
    label: &str,
    snapshot: impl Fn(&T) -> &CatalogEntrySnapshot,
) -> anyhow::Result<()> {
    for (id, value) in source {
        match destination.get(&id) {
            Some(held) => ensure!(
                snapshot(held).identity == snapshot(&value).identity,
                "two catalogs disagree about {label} entry {id}"
            ),
            None => {
                destination.insert(id, value);
            }
        }
    }
    Ok(())
}

fn snapshot(reference: &EntryReference, bytes: &[u8]) -> anyhow::Result<CatalogEntrySnapshot> {
    let snapshot = CatalogEntrySnapshot::new(&reference.id, &reference.version, bytes)?;
    ensure!(
        snapshot.identity.content_hash == reference.content_hash,
        "catalog payload {} content hash mismatch",
        reference.id
    );
    Ok(snapshot)
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 71
        && value.starts_with("sha256:")
        && value[7..].bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn canonical_sha256(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|digest| {
        digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

fn unique(values: &[u16]) -> bool {
    values.iter().copied().collect::<HashSet<_>>().len() == values.len()
}

fn count(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

fn sha256(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn recording_entry(snapshot: &CatalogEntrySnapshot) -> RecordingCatalogEntry {
    RecordingCatalogEntry {
        id: snapshot.identity.id.clone(),
        version: snapshot.identity.version.clone(),
        content_hash: snapshot.identity.content_hash.clone(),
        source_bytes: Arc::clone(&snapshot.source_bytes),
    }
}

#[cfg(test)]
const TEST_DEPTH_PROFILE_TEMPLATE: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../config/catalogs/dev/fallout4-steam-release-test/depth-profile.json"
));

#[cfg(test)]
const TEST_CAMERA_BINDING_TEMPLATE: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../config/catalogs/dev/fallout4-steam-release-test/camera-binding.json"
));

#[cfg(test)]
fn test_depth_profile_bytes(game_id: &str, executable_sha256: &str) -> Vec<u8> {
    let mut value: serde_json::Value =
        serde_json::from_slice(TEST_DEPTH_PROFILE_TEMPLATE).expect("depth template is valid JSON");
    value["game_id"] = serde_json::json!(game_id);
    value["executable_sha256"] = serde_json::json!(executable_sha256);
    serde_json::to_vec(&value).expect("depth fixture reserializes")
}

#[cfg(test)]
fn test_camera_binding_bytes(
    profile_id: &str,
    binding_id: &str,
    binding_version: &str,
    executable_sha256: &str,
) -> Vec<u8> {
    let mut value: serde_json::Value = serde_json::from_slice(TEST_CAMERA_BINDING_TEMPLATE)
        .expect("camera binding template is valid JSON");
    value["profile_id"] = serde_json::json!(profile_id);
    value["binding_id"] = serde_json::json!(binding_id);
    value["binding_version"] = serde_json::json!(binding_version);
    value["executable_sha256"] = serde_json::json!(executable_sha256);
    serde_json::to_vec(&value).expect("camera binding fixture reserializes")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(feature = "local-catalog-only"))]
    fn selection_for_generation_test() -> CatalogSelection {
        let snapshot = |id: &str| CatalogEntrySnapshot::new(id, "1", id.as_bytes()).unwrap();
        let catalog_bytes = Arc::new(b"catalog-v1".to_vec());
        CatalogSelection {
            provenance: CatalogProvenance::Compiled,
            catalog: CatalogIdentity {
                catalog_id: TEST_CATALOG_ID.into(),
                catalog_version: "2026-07-28.1".into(),
                content_hash: sha256(&catalog_bytes),
            },
            session_contract: SessionContract::LocalDevV1,
            catalog_source_bytes: catalog_bytes,
            device: snapshot("device"),
            recording: snapshot("recording"),
            game: snapshot("game"),
            mapping: snapshot("mapping"),
            depth_profile: None,
            camera_binding: None,
        }
    }

    /// 随便一个每游戏形状的 catalog id；这个测试只关心 selection 的代际语义。
    #[cfg(not(feature = "local-catalog-only"))]
    const TEST_CATALOG_ID: &str = "dataminer-synthetic-dev";

    /// 零条目的基线拦得住启动：门禁的第一问是「这个游戏在不在活跃目录里」，
    /// 空目录对任何游戏都答否。这是 grant 车道废除后（docs/100）唯一还在的启动闸。
    #[test]
    fn empty_baseline_is_fail_closed() {
        let store = CatalogStore::empty();
        let status = store.status();
        assert_eq!(status.provenance, CatalogProvenance::Compiled);
        assert_eq!(status.entry_counts.games, 0);
        // 空目录必须自称空——这一句是首个快照的诚实性：编排器还没跑刷新时，
        // 消费 store 状态的路径拿到的就是这个值。
        assert_eq!(status.result, "catalog_empty");
        let error = store
            .ensure_start_authorized("synthetic-win32-dev")
            .expect_err("an empty catalog must not authorize any start");
        assert!(
            error.to_string().contains("CATALOG_NO_MATCH"),
            "got: {error}"
        );
    }

    #[cfg(not(feature = "local-catalog-only"))]
    #[test]
    fn session_profile_generation_ignores_other_game_release_changes_but_not_profile_bytes() {
        let active = selection_for_generation_test();
        let mut other_game_changed = active.clone();
        other_game_changed.catalog.catalog_version = "2026-07-28.2".into();
        other_game_changed.catalog.content_hash = "sha256:new-release".into();
        assert!(active.same_profile_generation(&other_game_changed));

        let mut active_mapping_changed = other_game_changed;
        active_mapping_changed.mapping.identity.content_hash = "sha256:tightened-mapping".into();
        assert!(!active.same_profile_generation(&active_mapping_changed));

        let mut active_depth_changed = active.clone();
        active_depth_changed.depth_profile =
            Some(CatalogEntrySnapshot::new("game", "1", b"depth-profile-v1").unwrap());
        assert!(!active.same_profile_generation(&active_depth_changed));

        let mut active_binding_changed = active;
        active_binding_changed.camera_binding =
            Some(CatalogEntrySnapshot::new("binding", "1", b"camera-binding-v1").unwrap());
        assert!(!active_binding_changed.same_profile_generation(&active_depth_changed));
    }

    #[cfg(feature = "dev-catalog")]
    #[test]
    fn dev_catalog_cross_references_and_hashes_are_closed() {
        let store = CatalogStore::load().unwrap();
        let selection = store.selection("synthetic-win32-dev").unwrap();
        // 编译内嵌 ⇒ 永远 legacy v1，哪怕 bundle 本身已经是中性 schema
        // （issue #855 §D：本地路径不另造 v2 变体）。
        assert_eq!(selection.session_contract, SessionContract::LocalDevV1);
        assert_eq!(selection.catalog.catalog_id, "dataminer-synthetic-dev");
        for hash in [
            &selection.catalog.content_hash,
            &selection.device.identity.content_hash,
            &selection.recording.identity.content_hash,
            &selection.game.identity.content_hash,
            &selection.mapping.identity.content_hash,
        ] {
            assert!(hash.starts_with("sha256:"));
            assert_eq!(hash.len(), 71);
        }
        store
            .ensure_start_authorized("synthetic-win32-dev")
            .unwrap();
    }

    #[cfg(feature = "depth-lab")]
    fn write_content_addressed_depth_fixture(include_binding: bool) -> tempfile::TempDir {
        write_content_addressed_depth_fixture_with(include_binding, |_| {})
    }

    #[cfg(feature = "depth-lab")]
    fn write_content_addressed_depth_fixture_with(
        include_binding: bool,
        mutate_depth: impl FnOnce(&mut serde_json::Value),
    ) -> tempfile::TempDir {
        write_content_addressed_depth_fixture_with_pair(include_binding, mutate_depth, |_| {})
    }

    #[cfg(feature = "depth-lab")]
    fn write_content_addressed_depth_fixture_with_pair(
        include_binding: bool,
        mutate_depth: impl FnOnce(&mut serde_json::Value),
        mutate_binding: impl FnOnce(&mut serde_json::Value),
    ) -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        let payload_directory = directory.path().join("payloads").join("sha256");
        std::fs::create_dir_all(&payload_directory).unwrap();

        let executable_sha256 =
            "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let mut depth_value: serde_json::Value = serde_json::from_slice(&test_depth_profile_bytes(
            "synthetic-win32-dev",
            executable_sha256,
        ))
        .unwrap();
        mutate_depth(&mut depth_value);
        let depth_bytes = serde_json::to_vec(&depth_value).unwrap();
        let mut binding_value: serde_json::Value =
            serde_json::from_slice(&test_camera_binding_bytes(
                "synthetic-win32-dev",
                "synthetic-camera",
                "3",
                executable_sha256,
            ))
            .unwrap();
        mutate_binding(&mut binding_value);
        let binding_bytes = serde_json::to_vec(&binding_value).unwrap();

        let payloads: [(&str, &[u8]); 6] = [
            ("device_profiles", SYNTHETIC_DEVICE_BYTES),
            ("recording_profiles", SYNTHETIC_RECORDING_BYTES),
            ("game_profiles", SYNTHETIC_GAME_BYTES),
            ("mappings", SYNTHETIC_MAPPING_BYTES),
            ("depth_profiles", depth_bytes.as_slice()),
            ("camera_bindings", binding_bytes.as_slice()),
        ];
        for (_, bytes) in &payloads {
            let digest = sha256(bytes).trim_start_matches("sha256:").to_owned();
            std::fs::write(payload_directory.join(format!("{digest}.json")), bytes).unwrap();
        }

        let mut manifest: serde_json::Value =
            serde_json::from_slice(SYNTHETIC_CATALOG_BYTES).unwrap();
        let depth_bytes = payloads[4].1;
        let binding_bytes = payloads[5].1;
        manifest["depth_profiles"] = serde_json::json!([{
            "id": "synthetic-win32-dev",
            "version": "1",
            "content_hash": sha256(depth_bytes),
        }]);
        manifest["camera_bindings"] = if include_binding {
            serde_json::json!([{
                "id": "synthetic-camera",
                "version": "3",
                "content_hash": sha256(binding_bytes),
            }])
        } else {
            serde_json::json!([])
        };
        std::fs::write(
            directory.path().join("catalog.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        directory
    }

    #[cfg(feature = "depth-lab")]
    #[test]
    fn content_addressed_filesystem_loads_depth_pair_and_rejects_half_pair() {
        let complete = write_content_addressed_depth_fixture(true);
        validate_disk_bundle(complete.path()).expect("complete filesystem pair loads");

        let half = write_content_addressed_depth_fixture(false);
        let error = validate_disk_bundle(half.path()).unwrap_err().to_string();
        assert!(error.contains("depth pair"), "got: {error}");
    }

    #[cfg(feature = "depth-lab")]
    #[test]
    fn content_addressed_holstered_binding_uses_normal_catalog_validation() {
        let directory = write_content_addressed_depth_fixture_with_pair(
            true,
            |depth| {
                depth["schema_version"] = serde_json::json!("dm-depth-profile-2");
                depth["renderer_backend"] = serde_json::json!("d3d12");
                depth["readback_path"] = serde_json::json!("d3d12_copy_texture_region");
                depth["executable_sha256"] =
                    serde_json::json!(depth_lab::CP2077_BUILD_20383525_EXE_SHA256);
                depth["metric_scale"]["evidence_sha256"] =
                    serde_json::json!(format!("sha256:{}", "a".repeat(64)));
            },
            |binding| {
                let old = binding.clone();
                *binding = serde_json::from_str(include_str!(
                    "../../../../../../tools/depth-lab/tests/fixtures/camera-binding-v2.json"
                ))
                .unwrap();
                for field in ["binding_id", "binding_version", "profile_id"] {
                    binding[field] = old[field].clone();
                }
                binding["schema"] = serde_json::json!(depth_lab::CAMERA_BINDING_SCHEMA_V3);
                binding["executable_sha256"] =
                    serde_json::json!(depth_lab::CP2077_BUILD_20383525_EXE_SHA256);
                binding["holstered_camera"] = serde_json::json!({
                    "schema": depth_lab::cp2077_bound_camera::POLICY,
                    "camera_axis_mapping": {"delivery_x_from":"positive_x", "delivery_y_from":"negative_y", "delivery_z_from":"positive_z"}
                });
            },
        );
        validate_disk_bundle(directory.path())
            .expect("normal catalog loader must validate v3 pair and hashes");
    }

    #[cfg(feature = "depth-lab")]
    #[test]
    fn content_addressed_filesystem_accepts_profile_v2_v3_and_rejects_unknown_schema() {
        let v2 = write_content_addressed_depth_fixture_with_pair(
            true,
            |depth| {
                depth["schema_version"] = serde_json::json!("dm-depth-profile-2");
                depth["renderer_backend"] = serde_json::json!("d3d12");
                depth["readback_path"] = serde_json::json!("d3d12_copy_texture_region");
                depth["metric_scale"]["evidence_sha256"] =
                    serde_json::json!(format!("sha256:{}", "a".repeat(64)));
            },
            |binding| {
                let old = binding.clone();
                *binding = serde_json::from_str(include_str!(
                    "../../../../../../tools/depth-lab/tests/fixtures/camera-binding-v2.json"
                ))
                .unwrap();
                for field in [
                    "binding_id",
                    "binding_version",
                    "profile_id",
                    "executable_sha256",
                    "render_width",
                    "render_height",
                ] {
                    binding[field] = old[field].clone();
                }
            },
        );
        validate_disk_bundle(v2.path()).expect("v2 filesystem pair loads");

        // Profile v3's real KCD2 contract has a combined D3D12 binding. Merely
        // relabeling the v1 Fallout fixture's profile_version leaves a D3D11
        // binding and must fail the shared backend check.
        let v3 = tempfile::tempdir().unwrap();
        let payload_directory = v3.path().join("payloads").join("sha256");
        std::fs::create_dir_all(&payload_directory).unwrap();
        let kcd2_catalog = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../config/catalogs/dev/kingdom-come-deliverance-2-steam");
        for name in [
            "device-profile.json",
            "recording-profile.json",
            "game-profile.json",
            "mapping.json",
            "depth-profile.json",
            "camera-binding.json",
        ] {
            let bytes = std::fs::read(kcd2_catalog.join(name)).unwrap();
            let digest = sha256(&bytes);
            std::fs::write(
                payload_directory.join(format!("{}.json", digest.trim_start_matches("sha256:"))),
                bytes,
            )
            .unwrap();
        }
        std::fs::copy(
            kcd2_catalog.join("catalog.json"),
            v3.path().join("catalog.json"),
        )
        .unwrap();
        validate_disk_bundle(v3.path()).expect("real KCD2 v3 filesystem pair loads");

        let unsupported = write_content_addressed_depth_fixture_with(true, |depth| {
            depth["schema_version"] = serde_json::json!("dm-depth-profile-4");
        });
        let error = validate_disk_bundle(unsupported.path())
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("unsupported depth profile schema"),
            "got: {error}"
        );
    }

    #[cfg(feature = "depth-lab")]
    #[test]
    fn content_addressed_filesystem_rejects_depth_payload_tampering() {
        let directory = write_content_addressed_depth_fixture(true);
        let manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(directory.path().join("catalog.json")).unwrap())
                .unwrap();
        let digest = manifest["depth_profiles"][0]["content_hash"]
            .as_str()
            .unwrap()
            .trim_start_matches("sha256:");
        let payload_path = directory
            .path()
            .join("payloads")
            .join("sha256")
            .join(format!("{digest}.json"));
        let mut tampered = std::fs::read(&payload_path).unwrap();
        tampered.push(b' '); // still valid JSON, but no longer the pinned exact bytes
        std::fs::write(payload_path, tampered).unwrap();

        let error = validate_disk_bundle(directory.path())
            .unwrap_err()
            .to_string();
        assert!(error.contains("content hash mismatch"), "got: {error}");
    }

    #[cfg(feature = "depth-lab")]
    #[test]
    fn content_addressed_filesystem_rejects_non_regular_depth_payload() {
        let directory = write_content_addressed_depth_fixture(true);
        let manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(directory.path().join("catalog.json")).unwrap())
                .unwrap();
        let digest = manifest["depth_profiles"][0]["content_hash"]
            .as_str()
            .unwrap()
            .trim_start_matches("sha256:");
        let payload_path = directory
            .path()
            .join("payloads")
            .join("sha256")
            .join(format!("{digest}.json"));
        std::fs::remove_file(&payload_path).unwrap();
        std::fs::create_dir(&payload_path).unwrap();

        let error = validate_disk_bundle(directory.path())
            .unwrap_err()
            .to_string();
        assert!(error.contains("not a regular file"), "got: {error}");
    }

    #[cfg(feature = "depth-lab")]
    #[test]
    fn content_addressed_filesystem_rejects_incomplete_and_invalid_depth_profiles() {
        let missing = write_content_addressed_depth_fixture_with(true, |depth| {
            depth.as_object_mut().unwrap().remove("readback_slots");
        });
        let error = validate_disk_bundle(missing.path())
            .unwrap_err()
            .to_string();
        assert!(error.contains("invalid depth profile"), "got: {error}");

        let too_small = write_content_addressed_depth_fixture_with(true, |depth| {
            depth["readback_slots"] = serde_json::json!(3);
        });
        let error = validate_disk_bundle(too_small.path())
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("depth profile failed validation"),
            "got: {error}"
        );
    }

    #[cfg(feature = "depth-lab")]
    #[test]
    fn content_addressed_filesystem_rejects_semantically_invalid_camera_binding() {
        let directory = write_content_addressed_depth_fixture(true);
        let manifest_path = directory.path().join("catalog.json");
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
        let old_digest = manifest["camera_bindings"][0]["content_hash"]
            .as_str()
            .unwrap()
            .trim_start_matches("sha256:");
        let payload_directory = directory.path().join("payloads").join("sha256");
        let mut binding: serde_json::Value = serde_json::from_slice(
            &std::fs::read(payload_directory.join(format!("{old_digest}.json"))).unwrap(),
        )
        .unwrap();
        binding["projection_sources"] = serde_json::json!([]);
        let binding = serde_json::to_vec(&binding).unwrap();
        let digest = sha256(&binding);
        std::fs::write(
            payload_directory.join(format!("{}.json", digest.trim_start_matches("sha256:"))),
            binding,
        )
        .unwrap();
        manifest["camera_bindings"][0]["content_hash"] = serde_json::json!(digest);
        std::fs::write(manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();

        let error = validate_disk_bundle(directory.path())
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("camera binding failed production validation"),
            "got: {error}"
        );
    }

    /// 合并一册之后引擎参数从哪来：recording profile 投影出 `CaptureProfile`，
    /// 而游戏侧字段（window_mode / view_scale / FOV）留在目录侧，进不了引擎类型。
    #[cfg(feature = "dev-catalog")]
    #[test]
    fn recording_profile_projects_the_engine_subset_and_keeps_the_rest_out() {
        let store = CatalogStore::load().unwrap();
        let game = store.game("synthetic-win32-dev").unwrap();
        let recording = store.recording(&game.recording_profile_id).unwrap();
        let capture = store.capture(&game.recording_profile_id).unwrap();

        assert_eq!(capture.profile_id, recording.profile_id);
        assert_eq!(capture.width, recording.width);
        assert_eq!(capture.height, recording.height);
        assert_eq!(capture.fps_num, recording.fps_num);
        assert_eq!(capture.gop_frames, recording.gop_frames);
        // 中性一代的 recording profile **不再声明**训练分类（issue #855 §A.4），所以
        // 投影出来的那一位来自 `CLIENT_CAPTURE_IS_ALWAYS_NON_TRAINABLE` 这条硬约束，
        // 不来自 authoring。断言写成"字段缺席 + 投影恒为 true"，而不是"两边相等"——
        // 后者在字段缺席时会退化成一句永真的废话。
        assert_eq!(recording.non_trainable, None);
        assert!(capture.non_trainable);
        // 游戏侧配置只住在目录侧；引擎类型上根本没有这些字段可读。
        assert_eq!(recording.window_mode, "windowed");
        assert!(recording.cursor_capture);
        capture
            .validate()
            .expect("the projection is a valid engine profile");
    }

    #[cfg(feature = "dev-catalog")]
    #[test]
    fn cod_steam_release_test_is_bundled_without_hash_or_window_class_constraints() {
        let store = CatalogStore::load().unwrap();
        let game = store.game("call-of-duty-steam-release-test").unwrap();
        assert_eq!(
            game.executable_names,
            [
                "cod.exe",
                "cod22-cod.exe",
                "cod23-cod.exe",
                "cod24-cod.exe",
                "sp22-cod.exe",
                "ModernWarfare.exe"
            ]
        );
        assert_eq!(game.window_class, None);
        assert_eq!(game.executable_sha256, None);
        assert!(game.is_non_trainable());
        assert_eq!(game.version, "12");
        let selection = store.selection("call-of-duty-steam-release-test").unwrap();
        let device = selection.device_profile().unwrap();
        assert_eq!(device.profile_id, "cod-steam-windows-nvidia-audio-dev-v1");
        assert_eq!(device.minimum_windows_build, 20348);
        let recording = store.recording(&game.recording_profile_id).unwrap();
        let capture = store.capture(&game.recording_profile_id).unwrap();
        assert_eq!(
            recording.profile_id,
            "cod-steam-release-test-recording-60-audio-v1"
        );
        assert_eq!((capture.width, capture.height), (1920, 1080));
        assert_eq!((capture.fps_num, capture.fps_den), (60, 1));
        assert_eq!(capture.gop_frames, 240);
        assert_eq!(
            (capture.bitrate_bps, capture.max_bitrate_bps),
            (16_000_000, 24_000_000)
        );
        assert_eq!(capture.segment_seconds, 10);
        assert_eq!(capture.test_segments, 30);
        assert_eq!(recording.in_game_mouse_sensitivity, Some(8.0));
        assert_eq!(recording.field_of_view_degrees, Some(100.0));
        assert_eq!(
            recording
                .view_scale
                .as_ref()
                .and_then(|view_scale| view_scale.counts_per_360),
            Some(6818.18)
        );
        let audio = capture
            .audio
            .as_ref()
            .expect("COD DEV profile must capture process audio");
        assert!(matches!(
            audio.mode,
            capture_core::profile::AudioCaptureMode::ProcessLoopback
        ));
        assert_eq!(audio.sample_rate_hz, 48_000);
        assert_eq!(audio.channels, 2);
        assert_eq!(audio.bits_per_sample, 16);
        assert_eq!(audio.sample_format, "pcm_s16le");
        assert_eq!(audio.channel_mask, 3);
        assert_eq!(audio.compression, "zstd");
        assert_eq!(
            game.input_contract.keyboard_vks,
            [
                9, 13, 16, 17, 18, 20, 27, 32, 49, 50, 51, 52, 53, 54, 65, 67, 68, 69, 70, 71, 75,
                77, 81, 82, 83, 84, 86, 87, 88, 89, 90, 119, 121, 192
            ]
        );
        // Only F8/F10. Profile version 5 also listed 17/18 here, which meant the
        // projector dropped every Left Control and Left Alt event as application
        // control — silently killing `move.prone` (scan 29) and
        // `communication.ping_wheel` (scan 56) in the two asserted bindings below.
        // Ctrl+Alt+F8/F10 keep working regardless: they arrive through
        // `RegisterHotKey`/`WM_HOTKEY`, never through Raw Input.
        assert_eq!(game.input_contract.control_vks, [119, 121]);
        assert_eq!(game.input_contract.mouse_buttons, [1, 2, 4, 5, 6]);

        let mapping = store.mapping(&game.mapping_id).unwrap().mapping();
        assert_eq!(mapping.bindings.len(), 39);
        // COD's preset uses F for interaction and E for lethal equipment. Keep
        // these assertions because the original release-test mapping had them
        // swapped, which silently produced plausible but incorrect actions.
        for (scan_code, semantic) in [
            (17, "move.forward"),
            (31, "move.backward"),
            (30, "move.left"),
            (32, "move.right"),
            (57, "move.jump_mantle"),
            (42, "move.sprint"),
            (29, "move.prone"),
            (46, "move.crouch_slide"),
            (33, "interact.use"),
            (19, "combat.reload"),
            (47, "combat.melee"),
            (20, "combat.mount"),
            (18, "combat.lethal_equipment"),
            (16, "combat.tactical_equipment"),
            (45, "combat.field_upgrade"),
            (34, "inventory.armor_plate_or_drop"),
            (44, "communication.push_to_talk"),
            (37, "combat.killstreak_wheel"),
            (2, "inventory.previous_weapon"),
            (3, "inventory.next_weapon"),
            (4, "combat.killstreak_slot_1"),
            (5, "combat.killstreak_slot_2"),
            (6, "combat.killstreak_slot_3"),
            (7, "combat.killstreak_slot_4"),
            (15, "ui.scoreboard_or_backpack"),
            (41, "ui.scoreboard"),
            (21, "ui.squad_missions"),
            (58, "ui.map"),
            (50, "ui.map"),
            (28, "communication.text_chat"),
            (56, "communication.ping_wheel"),
        ] {
            assert_eq!(
                mapping.keyboard_semantic(scan_code, false),
                Some(semantic),
                "unexpected COD semantic for scan code {scan_code}"
            );
        }
        assert_eq!(
            mapping.mouse_button_semantic(1),
            Some("combat.primary_attack")
        );
        assert_eq!(mapping.mouse_button_semantic(2), Some("combat.aim"));
        assert_eq!(mapping.mouse_button_semantic(4), Some("communication.ping"));
        assert_eq!(mapping.mouse_button_semantic(5), Some("combat.melee"));
        assert_eq!(mapping.mouse_button_semantic(6), Some("combat.mount"));
        assert_eq!(
            mapping.mouse_control_semantic(capture_core::mapping::MouseControl::Wheel),
            Some("inventory.cycle_weapon")
        );
        store
            .ensure_start_authorized("call-of-duty-steam-release-test")
            .unwrap();
    }

    #[cfg(feature = "dev-catalog")]
    #[test]
    fn euro_truck_simulator_2_e2e_pins_the_default_in_cab_controls() {
        let store = CatalogStore::load().unwrap();
        let game = store.game("euro-truck-simulator-2-steam").unwrap();
        assert_eq!(game.executable_names, ["eurotrucks2.exe"]);
        assert_eq!(game.window_class.as_deref(), Some("prism3d"));
        assert_eq!(
            game.executable_sha256.as_deref(),
            Some("sha256:b7dffe6b27402c7db6dfd52cf982cd5bf292584138b35e3eb8efb311814ab3f8")
        );
        assert!(game.is_non_trainable());
        assert_eq!(game.input_contract.control_vks, [119, 121]);
        assert!(!game.input_contract.keyboard_vks.contains(&120));
        assert_eq!(game.input_contract.mouse_buttons, [2, 4]);
        assert!(game.input_contract.capture_mouse_move);
        assert!(game.input_contract.capture_wheel);

        let recording = store.recording(&game.recording_profile_id).unwrap();
        let capture = store.capture(&game.recording_profile_id).unwrap();
        assert_eq!((capture.width, capture.height), (1920, 1080));
        assert_eq!((capture.fps_num, capture.fps_den), (30, 1));
        assert_eq!(recording.in_game_mouse_sensitivity, Some(0.33));
        assert!(!recording.hdr);

        let mapping = store.mapping(&game.mapping_id).unwrap().mapping();
        assert_eq!(mapping.bindings.len(), 73);
        assert_eq!(mapping.keyboard_semantic(72, true), Some("move.accelerate"));
        assert_eq!(
            mapping.keyboard_semantic(29, true),
            Some("transmission.gear_down")
        );
        assert_eq!(mapping.keyboard_semantic(67, false), None);
        assert_eq!(mapping.mouse_button_semantic(4), Some("camera.zoom_drag"));
        assert_eq!(
            mapping.mouse_control_semantic(capture_core::mapping::MouseControl::Wheel),
            Some("camera.zoom")
        );
        store
            .ensure_start_authorized("euro-truck-simulator-2-steam")
            .unwrap();
    }

    #[cfg(feature = "dev-catalog")]
    #[test]
    fn halo_infinite_discovery_is_process_name_only_and_non_trainable() {
        let store = CatalogStore::load().unwrap();
        let catalog = store
            .status()
            .active_catalogs
            .into_iter()
            .find(|catalog| catalog.catalog_id == "dataminer-halo-infinite-steam-release-test")
            .unwrap();
        assert_eq!(catalog.catalog_version, "2026-08-25.0");

        let game = store.game("halo-infinite-steam-release-test").unwrap();
        assert_eq!(game.version, "5");
        assert_eq!(game.executable_names, ["HaloInfinite.exe"]);
        assert_eq!(game.window_class, None);
        assert_eq!(game.executable_sha256, None);
        assert!(game.is_non_trainable());
    }

    #[cfg(feature = "dev-catalog")]
    #[test]
    fn fallout4_steam_discovery_is_name_only_while_depth_and_capture_stay_pinned() {
        let store = CatalogStore::load().unwrap();
        let catalog = store
            .status()
            .active_catalogs
            .into_iter()
            .find(|catalog| catalog.catalog_id == "dataminer-fallout4-steam-release-test")
            .unwrap();
        assert_eq!(catalog.catalog_version, "2026-08-25.0");
        let game = store.game("fallout4-steam-release-test").unwrap();
        assert_eq!(game.version, "6");
        assert_eq!(game.executable_names, ["Fallout4.exe"]);
        assert_eq!(game.window_class, None);
        assert_eq!(game.executable_sha256, None);
        assert_eq!(game.input_contract.control_vks, [119, 121]);
        assert!(!game.input_contract.keyboard_vks.contains(&120));
        assert!(!game.input_contract.keyboard_vks.contains(&86));
        assert!(game.is_non_trainable());

        let selection = store.selection("fallout4-steam-release-test").unwrap();
        assert!(selection.depth_profile.is_some());
        assert!(selection.camera_binding.is_some());
        let depth: serde_json::Value =
            serde_json::from_slice(selection.depth_profile.as_ref().unwrap().source_bytes())
                .unwrap();
        let binding: serde_json::Value =
            serde_json::from_slice(selection.camera_binding.as_ref().unwrap().source_bytes())
                .unwrap();
        let expected_depth_hash =
            "sha256:428f9996cc4248e26c0f62f9fdd3eaf0e5eb305834b67ee5996538e593218b61";
        assert_eq!(depth["executable_sha256"], expected_depth_hash);
        assert_eq!(binding["executable_sha256"], expected_depth_hash);

        let capture = store.capture(&game.recording_profile_id).unwrap();
        let recording = store.recording(&game.recording_profile_id).unwrap();
        let mapping = store.mapping(&game.mapping_id).unwrap().mapping();
        assert_eq!(recording.version, "5");
        // 60s 分段（#709 采集稳定性）：崩一次只丢一分钟，不丢五分钟。
        // 钉在这里是因为它是**目录里的值**——改 recording-profile.json 却
        // 忘了同步 catalog 的 content_hash 时，这条会连同签名校验一起红。
        assert_eq!(recording.segment_seconds, 60);
        assert_eq!((capture.width, capture.height), (1920, 1080));
        assert_eq!((recording.width, recording.height), (1920, 1080));
        let audio = capture
            .audio
            .as_ref()
            .expect("Fallout 4 DEV profile must capture process audio");
        assert!(matches!(
            audio.mode,
            capture_core::profile::AudioCaptureMode::ProcessLoopback
        ));
        assert_eq!(audio.sample_rate_hz, 48_000);
        assert_eq!(audio.channels, 2);
        assert_eq!(audio.bits_per_sample, 16);
        assert_eq!(audio.sample_format, "pcm_s16le");
        assert_eq!(audio.channel_mask, 3);
        assert_eq!(audio.compression, "zstd");
        assert_eq!(recording.window_mode, "borderless");
        for (scan_code, semantic) in [
            (17, "move.forward"),
            (31, "move.backward"),
            (30, "move.left"),
            (32, "move.right"),
            (56, "combat.bash_power_attack_or_grenade"),
            (18, "interact.activate"),
            (19, "combat.ready_reload_or_holster"),
            (15, "ui.pipboy_or_flashlight"),
            (57, "move.jump"),
            (42, "move.sprint"),
            (16, "combat.vats"),
            (29, "move.sneak"),
            (46, "move.run"),
            (58, "move.toggle_always_run"),
            (45, "move.auto_move"),
            (33, "ui.favorites"),
            (63, "system.quicksave"),
            (1, "ui.pause"),
            (37, "ui.quick_stats"),
            (23, "ui.quick_inventory"),
            (36, "ui.quick_data"),
            (50, "ui.quick_map"),
            (24, "ui.quick_radio"),
        ] {
            assert_eq!(
                mapping.keyboard_semantic(scan_code, false),
                Some(semantic),
                "missing screenshot-evidenced Fallout 4 binding for scan {scan_code}"
            );
        }
        assert_eq!(mapping.keyboard_semantic(67, false), None);
        assert_eq!(mapping.keyboard_semantic(47, false), None);
        assert_eq!(mapping.mouse_button_semantic(1), Some("combat.attack"));
        assert_eq!(
            mapping.mouse_button_semantic(2),
            Some("combat.aim_or_block")
        );
        assert_eq!(
            mapping.mouse_button_semantic(4),
            Some("camera.toggle_view_or_workshop")
        );
        let selection = store.selection("fallout4-steam-release-test").unwrap();
        assert_eq!(
            selection.depth_profile.unwrap().identity.content_hash,
            "sha256:12e564f0a2c9c7c49fc826df798799960c7eff60bdfffcfe066762cb9c1eaa83"
        );
        assert_eq!(
            selection.camera_binding.unwrap().identity.content_hash,
            "sha256:3026c8a8140b5f25bd899013daba52ea520bfaabe02913df78b6326cedeab247"
        );
    }

    #[cfg(all(feature = "dev-catalog", feature = "depth-lab"))]
    #[test]
    fn kcd2_bundle_loads_the_exact_d3d12_depth_camera_and_audio_contract() {
        let store = CatalogStore::load().unwrap();
        let game = store
            .game("kingdom-come-deliverance-2-steam")
            .expect("KCD2 must be present in All Games DEV");
        assert_eq!(game.version, "1");
        assert_eq!(game.executable_names, ["KingdomCome.exe"]);
        assert_eq!(
            game.executable_sha256.as_deref(),
            Some("sha256:d85b5355842d16290dc0d97e1d02856c6f2811a2021c8957aa24d7782a771b77")
        );

        let selection = store.selection(game.game_id.as_str()).unwrap();
        let depth_snapshot = selection
            .depth_profile
            .expect("KCD2 catalog must offer an exact DepthProfile");
        let binding_snapshot = selection
            .camera_binding
            .expect("KCD2 catalog must offer an exact CameraBinding");
        let depth: depth_lab::DepthProfile =
            serde_json::from_slice(depth_snapshot.source_bytes()).unwrap();
        let binding: depth_lab::CameraBindingDocument =
            serde_json::from_slice(binding_snapshot.source_bytes()).unwrap();
        assert_eq!(depth.schema_version, "dm-depth-profile-3");
        assert_eq!(depth.renderer_backend, depth_lab::RendererBackend::D3d12);
        assert_eq!(depth.readback_slots, 6);
        assert!(depth.d3d12_selection.is_some());
        assert!(matches!(
            binding,
            depth_lab::CameraBindingDocument::Kcd2Combined(_)
        ));

        let capture = store.capture(&game.recording_profile_id).unwrap();
        let audio = capture
            .audio
            .as_ref()
            .expect("KCD2 delivery profile must capture process audio");
        assert!(matches!(
            audio.mode,
            capture_core::profile::AudioCaptureMode::ProcessLoopback
        ));
        assert_eq!((capture.width, capture.height), (1920, 1080));
    }

    #[cfg(all(feature = "dev-catalog", feature = "depth-lab"))]
    #[test]
    fn kcd2_default_inputs_reach_raw_without_inventing_contextual_actions() {
        use capture_core::mapping::MouseControl;

        // Authored from IPL_GameData.pak!Libs/Config/keybindSuperactions.xml,
        // SHA-256 6d59be44430169cf2d464a659b34a90a669c2d4272dfcf58f353fff5e8479a45.
        // defaultProfile.xml resolves its keyboard=_keybinds_ref_ entries through
        // that table. The audited user profile has no keybind override.
        let store = CatalogStore::load().unwrap();
        let game = store.game("kingdom-come-deliverance-2-steam").unwrap();
        assert_eq!(
            game.input_contract.keyboard_vks,
            [
                8, 9, 13, 16, 17, 18, 20, 27, 32, 36, 37, 38, 39, 40, 46, 48, 49, 50, 51, 52, 53,
                54, 55, 56, 57, 65, 66, 67, 68, 69, 70, 71, 73, 74, 75, 76, 77, 78, 79, 80, 81, 82,
                83, 84, 86, 87, 88, 90, 96, 97, 98, 99, 100, 101, 102, 103, 104, 105, 112, 119,
                121
            ]
        );
        assert_eq!(game.input_contract.control_vks, [119, 121]);
        assert_eq!(game.input_contract.mouse_buttons, [1, 2, 4]);
        assert!(game.input_contract.capture_mouse_move);
        assert!(game.input_contract.capture_wheel);
        for vk in [16, 17, 18] {
            assert!(!game.input_contract.is_control_key(vk));
        }
        // Debug-only function keys are not authored from the debug/cheat maps.
        for vk in [114, 115, 116, 117, 118, 120, 122] {
            assert!(!game.input_contract.allows_key(vk));
        }

        let mapping = store.mapping(&game.mapping_id).unwrap().mapping();
        assert_eq!(mapping.bindings.len(), 14);
        for (scan, semantic) in [
            (17, "move.forward"),
            (31, "move.backward"),
            (30, "move.left"),
            (32, "move.right"),
            (57, "move.jump"),
            (42, "move.sprint"),
            (46, "move.toggle_crouch"),
            (58, "move.toggle_run"),
        ] {
            assert_eq!(mapping.keyboard_semantic(scan, false), Some(semantic));
        }
        for (scan, semantic) in [
            (72, "move.forward"),
            (80, "move.backward"),
            (75, "move.left"),
            (77, "move.right"),
        ] {
            assert_eq!(mapping.keyboard_semantic(scan, true), Some(semantic));
            assert_eq!(mapping.keyboard_semantic(scan, false), None);
        }
        assert_eq!(
            mapping.mouse_control_semantic(MouseControl::DeltaX),
            Some("camera.yaw")
        );
        assert_eq!(
            mapping.mouse_control_semantic(MouseControl::DeltaY),
            Some("camera.pitch")
        );
        // A click, wheel tick or E/F/Tab event cannot establish combat/menu context.
        for button in [1, 2, 4] {
            assert_eq!(mapping.mouse_button_semantic(button), None);
        }
        assert_eq!(mapping.mouse_control_semantic(MouseControl::Wheel), None);
        for scan in [18, 33, 15] {
            assert_eq!(mapping.keyboard_semantic(scan, false), None);
        }
        validate_bindings_reach_frames(game, mapping).unwrap();
    }

    #[cfg(all(feature = "dev-catalog", feature = "depth-lab"))]
    #[test]
    fn fallout4_depth_profile_exact_bytes_preserve_main_readback_slots() {
        let store = CatalogStore::load().unwrap();
        let selection = store.selection("fallout4-steam-release-test").unwrap();
        let snapshot = selection
            .depth_profile
            .expect("Fallout 4 catalog offers the depth pair");
        let profile: depth_lab::DepthProfile = serde_json::from_slice(snapshot.source_bytes())
            .expect("selected exact bytes are a depth profile");
        assert_eq!(profile.readback_slots, 6, "#762 must remain authoritative");
    }

    #[cfg(feature = "dev-catalog")]
    #[test]
    fn a_plague_tale_controls_match_the_local_keyboard_screenshots() {
        let store = CatalogStore::load().unwrap();
        let game = store
            .game("a-plague-tale-innocence-steam-release-test")
            .unwrap();
        assert_eq!(game.version, "4");
        let mapping = store.mapping(&game.mapping_id).unwrap().mapping();
        assert_eq!(mapping.version, "2026-08-25.0");
        for (scan_code, semantic) in [
            (46, "move.crouch"),
            (44, "companion.follow_or_stay"),
            (16, "companion.interact"),
            (33, "world.inspect"),
            (15, "inventory.ammo_selection"),
            (23, "inventory.craft"),
            (29, "combat.throw_aim"),
        ] {
            assert_eq!(
                mapping.keyboard_semantic(scan_code, false),
                Some(semantic),
                "missing screenshot-evidenced A Plague Tale binding for scan {scan_code}"
            );
        }
        assert_eq!(
            mapping.mouse_button_semantic(4),
            Some("interact.release_carried_object")
        );
    }

    #[cfg(feature = "dev-catalog")]
    #[test]
    fn total_war_controls_match_the_local_control_pages() {
        let store = CatalogStore::load().unwrap();
        let game = store
            .game("total-war-warhammer-iii-steam-release-test")
            .unwrap();
        assert_eq!(game.version, "4");
        let mapping = store.mapping(&game.mapping_id).unwrap().mapping();
        assert_eq!(mapping.version, "2026-08-25.0");
        assert_eq!(
            mapping.keyboard_semantic(25, false),
            Some("simulation.pause")
        );
        assert_eq!(
            mapping.keyboard_semantic(57, false),
            Some("ui.context_overlay")
        );
        assert_eq!(
            mapping.keyboard_semantic(19, false),
            Some("simulation.speed_toggle")
        );
        for (scan_code, semantic) in [
            (83, "camera.unit_view_toggle"),
            (82, "camera.third_person_view"),
            (73, "camera.speed_increase"),
            (81, "camera.speed_decrease"),
            (71, "camera.focus_capital"),
            (79, "camera.default_rotation"),
        ] {
            assert_eq!(mapping.keyboard_semantic(scan_code, true), Some(semantic));
            assert_eq!(mapping.keyboard_semantic(scan_code, false), None);
        }
        // WASD were guesses in the draft mapping and are not present on the
        // captured control pages, so they must not silently emit camera labels.
        for scan_code in [17, 31, 30, 32] {
            assert_eq!(mapping.keyboard_semantic(scan_code, false), None);
        }
    }

    #[cfg(feature = "dev-catalog")]
    #[test]
    fn hades_ii_steam_identity_and_native_geometry_are_exactly_pinned() {
        let store = CatalogStore::load().unwrap();
        let game = store.game("hades-ii-steam-release-test").unwrap();
        assert_eq!(game.version, "6");
        assert_eq!(game.executable_names, ["Hades2.exe"]);
        assert_eq!(game.window_class.as_deref(), Some("The Forge"));
        assert_eq!(
            game.executable_sha256.as_deref(),
            Some("sha256:df11c588771105d6e90885a949c512044268ba15b175220decb168da93763ecf")
        );
        assert_eq!(game.input_contract.control_vks, [119]);
        assert!(game.is_non_trainable());

        let capture = store.capture(&game.recording_profile_id).unwrap();
        let recording = store.recording(&game.recording_profile_id).unwrap();
        let mapping = store.mapping(&game.mapping_id).unwrap().mapping();
        assert_eq!((capture.width, capture.height), (2560, 1440));
        assert_eq!((recording.width, recording.height), (2560, 1440));
        assert_eq!(recording.window_mode, "borderless");
        for (scan_code, semantic) in [
            (16, "combat.cast"),
            (33, "combat.hex"),
            (34, "interact.gift_or_reforge"),
            (20, "social.salute"),
            (29, "combat.auto_fire"),
            (46, "ui.book_of_shadows"),
            (23, "ui.inventory"),
            (48, "ui.boon_details"),
            (51, "ui.select_left_column"),
            (52, "ui.select_right_column"),
            (68, "system.bug_report"),
            (87, "system.screenshot"),
        ] {
            assert_eq!(
                mapping.keyboard_semantic(scan_code, false),
                Some(semantic),
                "missing screenshot-evidenced Hades II binding for scan {scan_code}"
            );
        }
    }

    #[cfg(feature = "dev-catalog")]
    #[test]
    fn cyberpunk_2077_catalog_pins_build_geometry_mapping_and_depth_pair() {
        let store = CatalogStore::load().unwrap();
        let game = store.game("cyberpunk-2077-steam").unwrap();
        assert_eq!(game.version, "4");
        assert_eq!(game.display_name, "Cyberpunk 2077 (Steam)");
        assert_eq!(game.executable_names, ["Cyberpunk2077.exe"]);
        assert_eq!(game.window_class, None);
        assert_eq!(
            game.executable_sha256.as_deref(),
            Some("sha256:a7de82945c03e041fc7339fcf9066224d98db2f5d80fea50f7947bb350a60991")
        );
        assert_eq!(game.input_contract.control_vks, [119, 121]);
        assert!(game.is_non_trainable());

        let capture = store.capture(&game.recording_profile_id).unwrap();
        let recording = store.recording(&game.recording_profile_id).unwrap();
        let mapping = store.mapping(&game.mapping_id).unwrap().mapping();
        assert_eq!((capture.width, capture.height), (1920, 1080));
        assert_eq!((recording.width, recording.height), (1920, 1080));
        assert_eq!((recording.fps_num, recording.fps_den), (30, 1));
        assert_eq!(recording.window_mode, "fullscreen_or_borderless");
        assert_eq!(mapping.keyboard_semantic(17, false), Some("move.forward"));
        assert_eq!(mapping.keyboard_semantic(29, false), Some("move.dodge"));
        assert_eq!(mapping.keyboard_semantic(15, false), Some("ui.scanner"));
        assert_eq!(mapping.mouse_button_semantic(1), Some("combat.attack"));
        // The pair that passed the 2026-09-18 four-tier certification, byte for byte: its hashes
        // are part of the certification key, so any edit here voids that certificate.
        let selection = store.selection("cyberpunk-2077-steam").unwrap();
        assert_eq!(
            selection
                .depth_profile
                .as_ref()
                .map(|pinned| pinned.identity.content_hash.as_str()),
            Some("sha256:fa557a62a79c449ed138df7f2bbc56674c13482eea5db6e2608a7b0e9a42d151")
        );
        assert_eq!(
            selection
                .camera_binding
                .as_ref()
                .map(|pinned| pinned.identity.content_hash.as_str()),
            Some("sha256:edc26919102745259372c2089e6f48b72bfb97d239e8f3cf53a185ed43374b6c")
        );
    }

    #[cfg(feature = "dev-catalog")]
    #[test]
    fn gta5_legacy_pins_only_the_observed_executable_identity() {
        let store = CatalogStore::load().unwrap();
        let game = store.game("gta5-legacy-pc").unwrap();
        assert_eq!(game.version, "3");
        assert_eq!(game.executable_names, ["GTA5.exe"]);
        assert_eq!(game.window_class, None);
        assert_eq!(
            game.executable_sha256.as_deref(),
            Some("sha256:677e4e355cfbdb13273b1d992407e3c261b3a108dc4dd5c8a0c4c1da651802e5")
        );
        assert!(game.is_non_trainable());

        let selection = store.selection("gta5-legacy-pc").unwrap();
        assert_eq!(selection.catalog.catalog_version, "2026-09-02.0");
        assert!(selection.depth_profile.is_none());
        assert!(selection.camera_binding.is_none());
    }

    #[cfg(feature = "dev-catalog")]
    #[test]
    fn nioh2_e2e_is_exactly_pinned_and_matches_screenshot_evidence() {
        let store = CatalogStore::load().unwrap();
        let game = store.game("nioh2-steam").unwrap();
        assert_eq!(game.executable_names, ["nioh2.exe"]);
        assert_eq!(game.window_class.as_deref(), Some("NIOH"));
        assert_eq!(
            game.executable_sha256.as_deref(),
            Some("sha256:d8aedf1a5ba4d5bd5dc5cb84904c60988ad1574c06ed214d254fd0c673aa39a7")
        );
        assert!(game.is_non_trainable());
        assert_eq!(game.input_contract.control_vks, [119, 121]);

        let capture = store.capture(&game.recording_profile_id).unwrap();
        assert_eq!((capture.width, capture.height), (2048, 1152));
        assert_eq!(capture.fps_num, 60);
        assert_eq!(capture.segment_seconds, 10);
        assert_eq!(capture.test_segments, 3);

        let mapping = store.mapping(&game.mapping_id).unwrap().mapping();
        for (scan_code, semantic) in [
            (17, "move.forward"),
            (31, "move.backward"),
            (30, "move.left"),
            (32, "move.right"),
            (57, "move.dodge_sprint"),
            (18, "interact.execute"),
            (33, "camera.lock_on"),
            (1, "ui.status_menu"),
            (34, "ui.action_menu"),
            (2, "inventory.quick_item_1"),
            (3, "inventory.quick_item_2"),
        ] {
            assert_eq!(
                mapping.keyboard_semantic(scan_code, false),
                Some(semantic),
                "missing screenshot-evidenced Nioh 2 binding for scan {scan_code}"
            );
        }
    }

    #[cfg(feature = "dev-catalog")]
    #[test]
    fn nightreign_preflight_is_exactly_pinned_and_remains_non_trainable() {
        let store = CatalogStore::load().unwrap();
        let game = store
            .game("elden-ring-nightreign-steam-release-test")
            .unwrap();
        assert_eq!(game.version, "4");
        assert_eq!(game.executable_names, ["nightreign.exe"]);
        assert_eq!(game.window_class.as_deref(), Some("ELDEN RING NIGHTREIGN"));
        assert_eq!(
            game.executable_sha256.as_deref(),
            Some("sha256:9545e8732919bb6a63904cb73223641b3c4f9a67869be5722612acca413e8b3a")
        );
        assert!(game.is_non_trainable());

        let capture = store.capture(&game.recording_profile_id).unwrap();
        assert_eq!((capture.width, capture.height), (1920, 1080));
        assert_eq!(capture.fps_num, 30);
        assert_eq!(capture.segment_seconds, 10);
        assert_eq!(capture.test_segments, 30);
    }

    #[cfg(feature = "dev-catalog")]
    #[test]
    fn bannerlord_e2e_is_exactly_pinned_and_matches_combat_keybinding_evidence() {
        let store = CatalogStore::load().unwrap();
        let game = store.game("mount-and-blade-2-bannerlord-steam").unwrap();
        assert_eq!(game.executable_names, ["Bannerlord.exe"]);
        assert_eq!(game.window_class.as_deref(), Some("TWApplication"));
        assert_eq!(
            game.executable_sha256.as_deref(),
            Some("sha256:5fc59a9da65474a707fd3e868a24c6f0a782e9364edd4d17a8f8813171237de0")
        );
        assert!(game.is_non_trainable());
        assert_eq!(game.input_contract.control_vks, [119, 121]);
        assert!(game.input_contract.capture_mouse_move);
        assert!(game.input_contract.capture_wheel);

        let capture = store.capture(&game.recording_profile_id).unwrap();
        assert_eq!((capture.width, capture.height), (2048, 1152));
        assert_eq!(capture.fps_num, 30);
        assert_eq!(capture.segment_seconds, 10);
        assert_eq!(capture.test_segments, 3);

        let mapping = store.mapping(&game.mapping_id).unwrap().mapping();
        for (scan_code, semantic) in [
            (17, "move.forward"),
            (31, "move.backward"),
            (30, "move.left"),
            (32, "move.right"),
            (15, "ui.leave"),
            (56, "ui.show_indicators"),
            (33, "interact.use"),
            (57, "move.jump"),
            (44, "move.crouch"),
            (18, "combat.kick_or_bash"),
            (45, "combat.switch_weapon_usage"),
            (79, "inventory.equip_item_1"),
            (80, "inventory.equip_item_2"),
            (81, "inventory.equip_item_3"),
            (75, "inventory.equip_item_4"),
            (34, "inventory.drop_weapon"),
            (43, "inventory.sheathe_weapon"),
            (42, "camera.zoom"),
            (41, "ui.view_character"),
            (19, "camera.toggle"),
            (58, "move.toggle_walk_run"),
            (47, "communication.push_to_talk"),
            (22, "inventory.equipment_switch_menu"),
            (14, "command.view_orders"),
            (59, "command.select_order_1"),
            (65, "command.select_order_7"),
        ] {
            assert_eq!(
                mapping.keyboard_semantic(scan_code, false),
                Some(semantic),
                "missing screenshot-evidenced Bannerlord combat binding for scan {scan_code}"
            );
        }
        assert_eq!(mapping.keyboard_semantic(66, false), None);
        assert_eq!(mapping.keyboard_semantic(67, false), None);
        assert_eq!(mapping.mouse_button_semantic(1), Some("combat.attack"));
        assert_eq!(mapping.mouse_button_semantic(2), Some("combat.defend"));
        assert_eq!(mapping.mouse_button_semantic(4), Some("camera.lock_target"));
        assert_eq!(
            mapping.mouse_control_semantic(capture_core::mapping::MouseControl::Wheel),
            Some("inventory.cycle_weapon_or_shield")
        );
    }

    #[cfg(feature = "dev-catalog")]
    #[test]
    fn bioshock_infinite_e2e_is_exactly_pinned_and_sixty_fps() {
        let store = CatalogStore::load().unwrap();
        let game = store.game("bioshock-infinite-steam-e2e").unwrap();
        assert_eq!(game.executable_names, ["BioShockInfinite.exe"]);
        assert_eq!(
            game.window_class.as_deref(),
            Some("LaunchUnrealUWindowsClient")
        );
        assert_eq!(
            game.executable_sha256.as_deref(),
            Some("sha256:c20a42529c4181b1d10eed416d9847a823f9453dd267ca478efb3a10014a0e77")
        );
        assert!(game.is_non_trainable());
        assert_eq!(game.version, "5");
        // Version 1 declared `control_vks: [17]`, so Left Control arrived flagged
        // as application control and the projector dropped it — `move.crouch` was
        // only ever emitted through its second binding on C (scan 46). It also
        // bound Z to `camera.zoom` without listing VK 90, so that event was
        // discarded even earlier, in `handle_keyboard`. Both keys are ordinary
        // game input here; the safety hotkeys ride `WM_HOTKEY`, not Raw Input.
        assert_eq!(game.input_contract.control_vks, [119, 121]);
        for vk in [17, 90] {
            assert!(game.input_contract.allows_key(vk));
            assert!(!game.input_contract.is_control_key(vk));
        }

        let capture = store.capture(&game.recording_profile_id).unwrap();
        assert_eq!(capture.segment_seconds, 10);
        assert_eq!(capture.test_segments, 3);
        assert_eq!(capture.width, 1920);
        assert_eq!(capture.height, 1080);
        assert_eq!(capture.fps_num, 60);

        // The contract is a hard allowlist: `handle_keyboard` drops any VK it does
        // not list, so a mapping binding whose VK is missing here silently produces
        // no action at all. Pin both lists together — the earlier revision bound Z
        // to iron sights without declaring VK 90, and that binding never fired.
        assert_eq!(
            game.input_contract.keyboard_vks,
            [
                16, 17, 18, 32, 37, 38, 39, 40, 48, 49, 50, 51, 52, 53, 54, 55, 56, 65, 67, 68, 69,
                70, 74, 78, 81, 82, 83, 86, 87, 90, 119, 121
            ]
        );
        // Only the Ctrl+Alt+F8 / F10 safety controls. VK_CONTROL and VK_MENU must
        // stay out: projection skips control-flagged events wholesale, and in this
        // game Left Control is crouch / sky-line reverse and Left Alt is walk, so
        // listing them would delete both labels from every frame.
        assert_eq!(game.input_contract.control_vks, [119, 121]);
        assert_eq!(game.input_contract.mouse_buttons, [1, 2, 4]);

        let mapping = store.mapping(&game.mapping_id).unwrap().mapping();
        assert_eq!(mapping.bindings.len(), 36);
        for (scan_code, semantic) in [
            (17, "move.forward"),
            (31, "move.backward"),
            (30, "move.left"),
            (32, "move.right"),
            (56, "move.walk"),
            (33, "interact.use"),
            (19, "combat.reload"),
            (47, "combat.melee"),
            // Left Control is the crouch alternate, and must NOT be a control_vk
            // (see the assertion above) or projection would drop it.
            (29, "move.crouch"),
            // Iron sights, not a camera control: shares `combat.aim` with the
            // sibling catalogs and with the middle-mouse alternate below.
            (44, "combat.aim"),
            (2, "inventory.equip_vigor_1"),
            (9, "inventory.equip_vigor_8"),
            // The in-game list binds the gameplay menu to digit 0 (scan 11). The
            // earlier revision had it on M (scan 50), which is bound to nothing.
            (11, "ui.gameplay_menu"),
        ] {
            assert_eq!(mapping.keyboard_semantic(scan_code, false), Some(semantic));
        }
        assert_eq!(mapping.keyboard_semantic(50, false), None);
        // Arrow keys are the alternates for WASD and arrive with RI_KEY_E0 set.
        for (scan_code, semantic) in [
            (72, "move.forward"),
            (80, "move.backward"),
            (75, "move.left"),
            (77, "move.right"),
        ] {
            assert_eq!(mapping.keyboard_semantic(scan_code, true), Some(semantic));
            assert_eq!(mapping.keyboard_semantic(scan_code, false), None);
        }
        assert_eq!(
            mapping.mouse_button_semantic(1),
            Some("combat.primary_attack")
        );
        assert_eq!(mapping.mouse_button_semantic(2), Some("combat.fire_vigor"));
        assert_eq!(mapping.mouse_button_semantic(4), Some("combat.aim"));
        store
            .ensure_start_authorized("bioshock-infinite-steam-e2e")
            .unwrap();
    }

    #[cfg(feature = "dev-catalog")]
    #[test]
    fn wolfenstein2_e2e_keeps_the_60_fps_contract() {
        let store = CatalogStore::load().unwrap();
        let game = store.game("wolfenstein2-steam").unwrap();
        assert!(game.is_non_trainable());
        assert_eq!(game.version, "3");

        let selection = store.selection("wolfenstein2-steam").unwrap();
        let device = selection.device_profile().unwrap();
        assert_eq!(device.version, "5");
        assert!(device.adapter_device_ids.contains(&12_120));

        let recording = store.recording(&game.recording_profile_id).unwrap();
        assert_eq!(recording.version, "4");
        let capture = store.capture(&game.recording_profile_id).unwrap();
        assert_eq!((capture.width, capture.height), (1920, 1080));
        assert_eq!((capture.fps_num, capture.fps_den), (60, 1));
        assert_eq!(capture.gop_frames, 240);
        assert_eq!(capture.segment_seconds, 10);
        assert_eq!(capture.test_segments, 3);
    }

    #[cfg(feature = "dev-catalog")]
    #[test]
    fn all_games_dev_profiles_admit_the_required_nvidia_gpu_fleet() {
        // A product name is not a stable hardware key: NVIDIA publishes multiple
        // PCI device IDs for some identically named boards. Keep every official
        // ID for the four required product families in the admission contract.
        let required_device_ids = [
            0x1C03, // GeForce GTX 1060 6GB
            0x1C06, // GeForce GTX 1060 6GB
            0x2808, // GeForce RTX 4060
            0x2882, // GeForce RTX 4060
            0x2D04, // GeForce RTX 5060 Ti
            0x2F18, // GeForce RTX 5070 Ti Laptop GPU
            0x2F58, // GeForce RTX 5070 Ti Laptop GPU
        ];
        let store = CatalogStore::load().unwrap();

        for game_id in [
            "call-of-duty-steam-release-test",
            "bioshock-infinite-steam-e2e",
            "wolfenstein2-steam",
            "age-of-empires-4-steam",
            "wuchang-fallen-feathers-steam",
            "halo-infinite-steam-release-test",
        ] {
            let selection = store.selection(game_id).unwrap();
            let device = selection.device_profile().unwrap();
            assert!(
                device.adapter_device_ids.is_empty()
                    || required_device_ids
                        .iter()
                        .all(|id| device.adapter_device_ids.contains(id)),
                "{game_id} does not admit the complete required NVIDIA GPU fleet"
            );
        }
    }

    #[cfg(feature = "dev-catalog")]
    #[test]
    fn missing_identity_pins_are_accepted() {
        let mut game: GameProfile = serde_json::from_slice(SYNTHETIC_GAME_BYTES).unwrap();
        game.window_class = None;
        game.executable_sha256 = None;
        let reference = EntryReference {
            id: game.game_id.clone(),
            version: game.version.clone(),
            content_hash: "sha256:unused-by-game-validation".into(),
        };
        validate_game(&game, &reference, CatalogSchemaGeneration::Neutral).unwrap();

        // 中性一代**不得**携带 `non_trainable`。这条是双向闸的另一半：旧语义不能借
        // 尸还魂——authoring 侧继续用它表达训练资格，而读者早已不再消费。
        game.non_trainable = Some(false);
        let resurrected = validate_game(&game, &reference, CatalogSchemaGeneration::Neutral)
            .unwrap_err()
            .to_string();
        assert!(
            resurrected.contains("must not carry non_trainable"),
            "got: {resurrected}"
        );
    }

    #[cfg(feature = "dev-catalog")]
    #[test]
    fn present_identity_pins_must_still_be_well_formed() {
        let mut game: GameProfile = serde_json::from_slice(SYNTHETIC_GAME_BYTES).unwrap();
        let reference = EntryReference {
            id: game.game_id.clone(),
            version: game.version.clone(),
            content_hash: "sha256:unused-by-game-validation".into(),
        };

        game.window_class = Some("   ".into());
        let blank = validate_game(&game, &reference, CatalogSchemaGeneration::Neutral)
            .unwrap_err()
            .to_string();
        assert!(blank.contains("game window class must be null or non-empty"));
        game.window_class = None;

        // `name-only:<exe>` 是 docs/55 废除的伪指纹格式：指纹字段只装真哈希，
        // 「没测到」由 null + 缺席原因表达。这条守卫保证它进不了目录。
        game.executable_sha256 = Some("name-only:capture-lab.exe".into());
        let pseudo = validate_game(&game, &reference, CatalogSchemaGeneration::Neutral)
            .unwrap_err()
            .to_string();
        assert!(pseudo.contains("game executable hash must be null or a SHA-256 identity"));
    }

    /// 断代要断得干净：v1 的字段在 `deny_unknown_fields` 下必须是解析错误，
    /// 而不是被静默忽略后跑出一份"看起来能用"的目录。
    #[cfg(feature = "dev-catalog")]
    #[test]
    fn retired_v1_reference_fields_are_parse_errors() {
        let mut game: serde_json::Value = serde_json::from_slice(SYNTHETIC_GAME_BYTES).unwrap();
        game["mapping_version"] = serde_json::json!("2026-07-29.0");
        assert!(
            serde_json::from_value::<GameProfile>(game).is_err(),
            "mapping_version must not survive as a silently ignored field"
        );

        let mut recording: serde_json::Value =
            serde_json::from_slice(SYNTHETIC_RECORDING_BYTES).unwrap();
        recording["capture_profile_id"] = serde_json::json!("synthetic-win32-dev-v1");
        assert!(
            serde_json::from_value::<RecordingProfile>(recording).is_err(),
            "the settings->capture back-reference must not survive the merge"
        );
    }

    /// 同一个 id 在一份目录里出现两个版本 = 引用去版本化之后的歧义，必须当场报错
    /// 而不是留给 `HashMap::insert` 静默覆盖（docs/57 §3.2 的配套不变量）。
    #[cfg(feature = "dev-catalog")]
    #[test]
    fn one_catalog_holds_at_most_one_version_per_id() {
        let mut manifest: serde_json::Value =
            serde_json::from_slice(SYNTHETIC_CATALOG_BYTES).unwrap();
        let mut second = manifest["mappings"][0].clone();
        second["version"] = serde_json::json!("2026-07-29.9");
        manifest["mappings"].as_array_mut().unwrap().push(second);
        let bytes = serde_json::to_vec(&manifest).unwrap();
        let error = CatalogStore::from_sources(CatalogSources {
            manifest: &bytes,
            devices: &[SYNTHETIC_DEVICE_BYTES],
            recordings: &[SYNTHETIC_RECORDING_BYTES],
            games: &[SYNTHETIC_GAME_BYTES],
            mappings: &[SYNTHETIC_MAPPING_BYTES, SYNTHETIC_MAPPING_BYTES],
            depth_profiles: &[],
            camera_bindings: &[],
        })
        .map(|_| ())
        .unwrap_err()
        .to_string();
        assert!(error.contains("at most one version per id"), "got: {error}");
    }

    /// 把中性的 synthetic bundle 降级回 legacy 一代：四本载荷各自换回旧 schema 串并
    /// 补上 `non_trainable`，manifest 的 content_hash 随之重算。
    ///
    /// 三份带 `SUPERSEDED.md` 的研究快照仍然停在 legacy（issue #855 §A.9），所以
    /// `validate_manifest` 的 legacy 分支必须继续有测试覆盖——而仓库里已经没有活跃
    /// bundle 走那条路了。与其去读那三份冻结字节（它们各有自己的几何与身份钉，断言
    /// 会绑到与本用例无关的细节上），不如就地把 synthetic 降一代。
    #[cfg(feature = "dev-catalog")]
    fn legacy_synthetic_bundle(manifest_non_trainable: bool) -> (Vec<u8>, [Vec<u8>; 4]) {
        let downgrade = |bytes: &[u8], legacy_schema: &str| -> Vec<u8> {
            let mut value: serde_json::Value = serde_json::from_slice(bytes).unwrap();
            value["schema_version"] = serde_json::json!(legacy_schema);
            value["non_trainable"] = serde_json::json!(true);
            serde_json::to_vec(&value).unwrap()
        };
        let device = downgrade(SYNTHETIC_DEVICE_BYTES, "dm-device-profile-2");
        let recording = downgrade(SYNTHETIC_RECORDING_BYTES, "dm-recording-profile-1");
        let game = downgrade(SYNTHETIC_GAME_BYTES, "dm-game-profile-2");
        let mapping = SYNTHETIC_MAPPING_BYTES.to_vec();

        let mut manifest: serde_json::Value =
            serde_json::from_slice(SYNTHETIC_CATALOG_BYTES).unwrap();
        manifest["schema_version"] = serde_json::json!("dm-catalog-bundle-2");
        manifest["non_trainable"] = serde_json::json!(manifest_non_trainable);
        for (category, bytes) in [
            ("device_profiles", device.as_slice()),
            ("recording_profiles", recording.as_slice()),
            ("game_profiles", game.as_slice()),
            ("mappings", mapping.as_slice()),
        ] {
            manifest[category][0]["content_hash"] = serde_json::json!(sha256(bytes));
        }
        (
            serde_json::to_vec(&manifest).unwrap(),
            [device, recording, game, mapping],
        )
    }

    /// 渠道与 release 分类必须自洽。certified 车道废除后（docs/100）只剩一条渠道，
    /// 它要求 `non_trainable: true`——一份自称可训练的 legacy manifest 没有渠道能收它。
    /// 一份自称可训练的 legacy manifest 没有任何下发路径能收它。
    ///
    /// 断代前这条判据绕道 `declared == mode.non_trainable()`——`mode` 恒
    /// `DevNonTrainable`、`non_trainable()` 恒 true，所以它逐字等于"必须是 true"，
    /// 只是把答案藏在一个把三件事粘在一起的词后面（issue #855 §2）。命题一条没少，
    /// 报错换成直说。
    #[cfg(feature = "dev-catalog")]
    #[test]
    fn a_legacy_manifest_claiming_trainable_has_nowhere_to_go() {
        let (manifest, [device, recording, game, mapping]) = legacy_synthetic_bundle(false);
        let error = CatalogStore::from_sources(CatalogSources {
            manifest: &manifest,
            devices: &[&device],
            recordings: &[&recording],
            games: &[&game],
            mappings: &[&mapping],
            depth_profiles: &[],
            camera_bindings: &[],
        })
        .map(|_| ())
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("declares non_trainable=false, which no delivery path accepts"),
            "got: {error}"
        );
    }

    /// 同一份降级 bundle 在 `non_trainable: true` 时必须仍然装得进来——三份冻结研究
    /// 快照走的就是这条路。只测拒绝不测放行，legacy 分支可以整条坏掉而没人发现。
    #[cfg(feature = "dev-catalog")]
    #[test]
    fn a_legacy_bundle_still_loads_on_the_only_channel() {
        let (manifest, [device, recording, game, mapping]) = legacy_synthetic_bundle(true);
        let store = CatalogStore::from_sources(CatalogSources {
            manifest: &manifest,
            devices: &[&device],
            recordings: &[&recording],
            games: &[&game],
            mappings: &[&mapping],
            depth_profiles: &[],
            camera_bindings: &[],
        })
        .expect("a legacy bundle still loads");
        assert!(
            store
                .game("synthetic-win32-dev")
                .unwrap()
                .is_non_trainable()
        );
    }

    /// 中性一代复活旧字段就是拒绝——这是断代的另一半，方向与上面那条相反。
    #[cfg(feature = "dev-catalog")]
    #[test]
    fn a_neutral_manifest_must_not_resurrect_the_retired_field() {
        let mut manifest: serde_json::Value =
            serde_json::from_slice(SYNTHETIC_CATALOG_BYTES).unwrap();
        manifest["non_trainable"] = serde_json::json!(false);
        let bytes = serde_json::to_vec(&manifest).unwrap();
        let error = CatalogStore::from_sources(CatalogSources {
            manifest: &bytes,
            devices: &[SYNTHETIC_DEVICE_BYTES],
            recordings: &[SYNTHETIC_RECORDING_BYTES],
            games: &[SYNTHETIC_GAME_BYTES],
            mappings: &[SYNTHETIC_MAPPING_BYTES],
            depth_profiles: &[],
            camera_bindings: &[],
        })
        .map(|_| ())
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("must not carry non_trainable"),
            "got: {error}"
        );
    }

    #[cfg(not(feature = "dev-catalog"))]
    #[test]
    fn dev_payloads_are_not_available_without_explicit_feature() {
        let store = CatalogStore::load().unwrap();
        assert_eq!(store.status().provenance, CatalogProvenance::Compiled);
        assert_eq!(store.games().count(), 0);
    }
}

// 新旧 catalog schema 并存期的三条边界（issue #855 PR 1）。
//
//   ① v2 仍可读                        —— 今天的 current、回滚与三份 superseded 快照
//   ② 中性 schema 无字段可读            —— 断代之后 31 份活跃 bundle 走的那条路（PR 2）
//   ③ 中性 schema 带旧字段被拒绝        —— 防止旧语义借尸还魂
//
// 三条必须**同时**成立才叫兼容桥：只有①是没断代，只有②是硬切，缺③则新 schema 变成
// 「字段可选」，于是 authoring 侧会继续用它表达训练资格，而读者早已不再消费。
//
// 不加 feature 门、直接内嵌真实的已提交 COD bundle 字节：这条桥必须在 CI 默认的
// `cargo test -p dataminer-client --lib` 下就被验到，而不是只在 dev-catalog 组合里。
#[cfg(test)]
mod catalog_schema_bridge_tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    // 内嵌**真实的**已提交 legacy bundle 字节，而不是手搓一份 JSON：兼容桥要证明的是
    // 「今天线上这些字节仍然读得进来」，用一份自造 fixture 证明不了这件事。
    //
    // 路径写法与下面的 `signed_bundle_tests` 保持一致（`CARGO_MANIFEST_DIR` +
    // `/../../../config/...`）：`tools/verify-release-surface.ps1` 的手写目录内嵌闸按
    // `"/catalogs/dev/<dir>/` 这个字面量形状扫描 catalog.rs，那条闸守的是「谁都别再往
    // 源码里加回逐游戏 include_bytes!」，测试字节不该从它身上钻空子换个拼法绕过去。
    const LEGACY_MANIFEST: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../config/catalogs/dev/cod-steam-release-test/catalog.json"
    ));
    const LEGACY_DEVICE: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../config/catalogs/dev/cod-steam-release-test/device-profile.json"
    ));
    const LEGACY_RECORDING: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../config/catalogs/dev/cod-steam-release-test/recording-profile.json"
    ));
    const LEGACY_GAME: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../config/catalogs/dev/cod-steam-release-test/game-profile.json"
    ));
    const LEGACY_MAPPING: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../config/catalogs/dev/cod-steam-release-test/mapping.json"
    ));

    /// 哪份载荷上强行复活退役字段（③ 用）。
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Resurrect {
        None,
        Manifest,
        Device,
        Recording,
        Game,
    }

    struct Bundle {
        manifest: Vec<u8>,
        device: Vec<u8>,
        recording: Vec<u8>,
        game: Vec<u8>,
        mapping: Vec<u8>,
    }

    impl Bundle {
        fn load(&self) -> anyhow::Result<CatalogStore> {
            let devices: [&[u8]; 1] = [&self.device];
            let recordings: [&[u8]; 1] = [&self.recording];
            let games: [&[u8]; 1] = [&self.game];
            let mappings: [&[u8]; 1] = [&self.mapping];
            CatalogStore::from_sources(CatalogSources {
                manifest: &self.manifest,
                devices: &devices,
                recordings: &recordings,
                games: &games,
                mappings: &mappings,
                depth_profiles: &[],
                camera_bindings: &[],
            })
        }

        fn error(&self) -> String {
            // 不用 `expect_err`：它要求 Ok 分支实现 Debug，而 `CatalogStore` 没有
            // （它持有整册字节，Debug 输出没有意义）。let-else 拿到同一个错误。
            let Err(error) = self.load() else {
                panic!("bundle must be refused");
            };
            format!("{error:#}")
        }
    }

    /// 从已提交的 legacy COD bundle 造一份 bundle。
    ///
    /// `neutral_payloads` / `neutral_manifest` 分开传，是为了能造出"半迁移"的负例。
    /// 每份 payload 改完都重算 content hash 写回 manifest——否则拒绝会发生在 hash
    /// 对账那一步，测到的就不是代际闸了。
    fn build(neutral_manifest: bool, neutral_payloads: bool, resurrect: Resurrect) -> Bundle {
        fn retire(
            value: &mut serde_json::Value,
            neutral: bool,
            resurrected: bool,
            legacy_schema: &str,
            neutral_schema: &str,
        ) {
            let object = value.as_object_mut().expect("payload is a JSON object");
            object.insert(
                "schema_version".into(),
                serde_json::json!(if neutral {
                    neutral_schema
                } else {
                    legacy_schema
                }),
            );
            if neutral && !resurrected {
                object.remove("non_trainable");
            } else {
                object.insert("non_trainable".into(), serde_json::json!(true));
            }
        }

        let mut device: serde_json::Value = serde_json::from_slice(LEGACY_DEVICE).unwrap();
        retire(
            &mut device,
            neutral_payloads,
            resurrect == Resurrect::Device,
            "dm-device-profile-2",
            "dm-device-profile-3",
        );
        let mut recording: serde_json::Value = serde_json::from_slice(LEGACY_RECORDING).unwrap();
        retire(
            &mut recording,
            neutral_payloads,
            resurrect == Resurrect::Recording,
            "dm-recording-profile-1",
            "dm-recording-profile-2",
        );
        let mut game: serde_json::Value = serde_json::from_slice(LEGACY_GAME).unwrap();
        retire(
            &mut game,
            neutral_payloads,
            resurrect == Resurrect::Game,
            "dm-game-profile-2",
            "dm-game-profile-3",
        );

        let device = serde_json::to_vec(&device).unwrap();
        let recording = serde_json::to_vec(&recording).unwrap();
        let game = serde_json::to_vec(&game).unwrap();
        let mapping = LEGACY_MAPPING.to_vec();

        let mut manifest: serde_json::Value = serde_json::from_slice(LEGACY_MANIFEST).unwrap();
        retire(
            &mut manifest,
            neutral_manifest,
            resurrect == Resurrect::Manifest,
            "dm-catalog-bundle-2",
            "dm-catalog-bundle-3",
        );
        manifest["device_profiles"][0]["content_hash"] = serde_json::json!(sha256(&device));
        manifest["recording_profiles"][0]["content_hash"] = serde_json::json!(sha256(&recording));
        manifest["game_profiles"][0]["content_hash"] = serde_json::json!(sha256(&game));
        manifest["mappings"][0]["content_hash"] = serde_json::json!(sha256(&mapping));

        Bundle {
            manifest: serde_json::to_vec(&manifest).unwrap(),
            device,
            recording,
            game,
            mapping,
        }
    }

    fn legacy() -> Bundle {
        build(false, false, Resurrect::None)
    }

    fn neutral() -> Bundle {
        build(true, true, Resurrect::None)
    }

    // ── ① v2 仍可读 ────────────────────────────────────────────────────────
    #[test]
    fn the_current_v2_schema_still_loads() {
        let store = legacy().load().expect("dm-catalog-bundle-2 still loads");
        assert_eq!(store.games().count(), 1);
        assert_eq!(store.status().result, "catalog_available");
    }

    #[test]
    fn the_legacy_schema_still_requires_the_field() {
        // 断代不是"这个字段以后可以不写"。一份自称 dm-catalog-bundle-2 却不带它的
        // manifest 是损坏，不是中性。
        let mut broken = legacy();
        let mut manifest: serde_json::Value = serde_json::from_slice(&broken.manifest).unwrap();
        manifest
            .as_object_mut()
            .unwrap()
            .remove("non_trainable")
            .expect("the legacy fixture carries the field");
        broken.manifest = serde_json::to_vec(&manifest).unwrap();
        assert!(
            broken
                .error()
                .contains("must declare an explicit non_trainable"),
            "got: {}",
            broken.error()
        );
    }

    // ── ② 中性 schema 无字段可读 ───────────────────────────────────────────
    #[test]
    fn the_neutral_schema_loads_without_the_field() {
        let store = neutral().load().expect("dm-catalog-bundle-3 loads");
        assert_eq!(store.games().count(), 1);

        let game_id = store
            .games()
            .next()
            .expect("the neutral bundle carries one game")
            .game_id
            .clone();
        let selection = store.selection(&game_id).expect("selection resolves");
        // 这一份是**离线装载**（provenance = Compiled），所以即使 schema 已断代，
        // 契约仍是 legacy v1：中性 v2 要求"managed 且中性"两个条件同时成立。
        assert_eq!(selection.session_contract, SessionContract::LocalDevV1);
        // 运行时那一位不再来自 bundle，而是来自那条硬约束本身。它仍然是 true——
        // 客户端与录制链路的任何位置都不得主张可训练性（AGENTS.md、docs/100）。
        assert_eq!(
            selection.recording_context().non_trainable,
            Some(CLIENT_CAPTURE_IS_ALWAYS_NON_TRAINABLE)
        );
        // 中性 profile 不再声明，但投影进引擎的那一位仍然是 true——录制侧的行为
        // 一个字节没变。
        assert_eq!(selection.recording_profile().unwrap().non_trainable, None);
        assert!(
            selection
                .recording_profile()
                .unwrap()
                .capture_profile()
                .non_trainable
        );
        assert_eq!(selection.device_profile().unwrap().non_trainable, None);
    }

    #[test]
    fn a_neutral_bundle_verifies_through_the_signed_delivery_path() {
        // 中性一代删掉了 manifest 的自称分类，所以"这份 bundle 从哪条渠道来"完全
        // 由验签回答。这条测试证明那条路是通的，而不是只有离线装载能读新 schema。
        let bundle = neutral();
        let key = SigningKey::from_bytes(&[7u8; 32]);
        let mut input = CATALOG_SIGNATURE_DOMAIN.to_vec();
        input.extend_from_slice(&bundle.manifest);
        let signature = key.sign(&input).to_bytes();
        let devices: [&[u8]; 1] = [&bundle.device];
        let recordings: [&[u8]; 1] = [&bundle.recording];
        let games: [&[u8]; 1] = [&bundle.game];
        let mappings: [&[u8]; 1] = [&bundle.mapping];
        let store = CatalogStore::load_from_signed_bundle(
            &key.verifying_key(),
            SignedCatalogSources {
                manifest: &bundle.manifest,
                signature: &signature,
                devices: &devices,
                recordings: &recordings,
                games: &games,
                mappings: &mappings,
                depth_profiles: &[],
                camera_bindings: &[],
            },
        )
        .expect("a neutral bundle verifies under the pinned-anchor path");
        assert_eq!(store.provenance, CatalogProvenance::Delivered);
        // managed（已验签下发）+ 中性 schema ⇒ 这一款走 v2 candidate 契约。
        // 这是 issue #855 PR 4 的那条断代线，两个条件缺一不可。
        let game_id = store
            .games()
            .next()
            .expect("the neutral bundle carries one game")
            .game_id
            .clone();
        let selection = store.selection(&game_id).expect("selection resolves");
        assert_eq!(selection.session_contract, SessionContract::IngestV2);
        // 中性契约的目录身份不再自称车道，也不再申报训练分类。
        let context = selection.recording_context();
        assert_eq!(context.mode, None);
        assert_eq!(context.non_trainable, None);
    }

    /// 同一份**已验签下发**的 bundle，schema 还停在 legacy 时必须仍走 legacy v1。
    ///
    /// 这是部署顺序闸的另一半：客户端可以先发布、先铺开，只要生产 current 还是
    /// 旧 v2 release，它写下的就仍是 legacy v1 会话。切 current 是人的动作，
    /// 而不是"部署了新客户端就自己跑到 v2 去"。
    #[test]
    fn a_delivered_bundle_still_on_the_legacy_schema_keeps_writing_v1() {
        let bundle = legacy();
        let key = SigningKey::from_bytes(&[7u8; 32]);
        let mut input = CATALOG_SIGNATURE_DOMAIN.to_vec();
        input.extend_from_slice(&bundle.manifest);
        let signature = key.sign(&input).to_bytes();
        let devices: [&[u8]; 1] = [&bundle.device];
        let recordings: [&[u8]; 1] = [&bundle.recording];
        let games: [&[u8]; 1] = [&bundle.game];
        let mappings: [&[u8]; 1] = [&bundle.mapping];
        let store = CatalogStore::load_from_signed_bundle(
            &key.verifying_key(),
            SignedCatalogSources {
                manifest: &bundle.manifest,
                signature: &signature,
                devices: &devices,
                recordings: &recordings,
                games: &games,
                mappings: &mappings,
                depth_profiles: &[],
                camera_bindings: &[],
            },
        )
        .expect("a legacy bundle still verifies through the signed path");
        assert_eq!(store.provenance, CatalogProvenance::Delivered);
        let game_id = store
            .games()
            .next()
            .expect("the legacy bundle carries one game")
            .game_id
            .clone();
        let selection = store.selection(&game_id).expect("selection resolves");
        assert_eq!(selection.session_contract, SessionContract::LocalDevV1);
        assert_eq!(
            selection.recording_context().mode.as_deref(),
            Some("dev_non_trainable")
        );
    }

    // ── ③ 中性 schema 带旧字段被拒绝 ───────────────────────────────────────
    #[test]
    fn the_neutral_schema_refuses_a_resurrected_field() {
        for resurrect in [
            Resurrect::Manifest,
            Resurrect::Device,
            Resurrect::Recording,
            Resurrect::Game,
        ] {
            let error = build(true, true, resurrect).error();
            assert!(
                error.contains("must not carry non_trainable"),
                "got: {error}"
            );
        }
    }

    #[test]
    fn a_half_migrated_bundle_is_refused() {
        // 半迁移是最坏状态：部分读者放行、部分读者拒绝，而错误信息指不到真正的原因。
        let neutral_manifest_only = build(true, false, Resurrect::None).error();
        assert!(
            neutral_manifest_only.contains("mixes schema generations"),
            "got: {neutral_manifest_only}"
        );
        let neutral_payloads_only = build(false, true, Resurrect::None).error();
        assert!(
            neutral_payloads_only.contains("mixes schema generations"),
            "got: {neutral_payloads_only}"
        );
    }

    #[test]
    fn an_unknown_schema_version_is_refused_rather_than_guessed() {
        let mut unknown = neutral();
        let mut manifest: serde_json::Value = serde_json::from_slice(&unknown.manifest).unwrap();
        manifest["schema_version"] = serde_json::json!("dm-catalog-bundle-9");
        unknown.manifest = serde_json::to_vec(&manifest).unwrap();
        assert!(
            unknown
                .error()
                .contains("unsupported catalog manifest schema"),
            "got: {}",
            unknown.error()
        );
    }

    #[test]
    fn the_pinned_delivery_anchor_survived_the_retired_mode_word() {
        // `CatalogMode` 随 issue #855 §A.4 退役了，选锚不能跟着悬空：锚是钉死在
        // 二进制里的那 32 个字节，与任何车道词无关。PR 1 抽出这个函数正是为了
        // 这一刻；这条断言钉住的是「退役之后锚一个字节没变」。
        assert_eq!(
            pinned_delivery_anchor().unwrap().to_bytes(),
            DEV_CATALOG_ANCHOR
        );
    }
}

// 驱动地板与型号清单（docs/57 §2.3/§2.4）。硬件无关、feature 无关：只走 JSON →
// DeviceProfile → 地板比较 / authoring 校验这条纯函数链。
#[cfg(test)]
mod device_profile_tests {
    use super::*;

    fn profile_json(floor: serde_json::Value, device_ids: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "schema_version": "dm-device-profile-2",
            "profile_id": "fleet-test-v1",
            "version": "1",
            "non_trainable": false,
            "platform": "windows",
            "minimum_windows_build": 26200,
            "adapter_vendor_id": 4318,
            "adapter_device_ids": device_ids,
            "minimum_driver_version": floor,
            "encoder_backend": "mf_nvidia_hardware_v1",
            "input_backend": "raw_input_v1",
            "pointer_mode": "relative",
            "evidence_class": "test_fleet",
            "mouse_dpi": null
        })
    }

    fn profile(floor: serde_json::Value) -> DeviceProfile {
        serde_json::from_value(profile_json(floor, serde_json::json!([])))
            .expect("dm-device-profile-2 parses")
    }

    fn floor_error(floor: &str, observed: Option<&str>) -> String {
        let error = profile(serde_json::json!(floor))
            .verify_driver_floor(observed)
            .expect_err("driver floor check must reject");
        format!("{error:#}")
    }

    #[test]
    fn driver_floor_accepts_equal_and_newer_and_rejects_older() {
        let profile = profile(serde_json::json!("32.0.15.9227"));
        profile
            .verify_driver_floor(Some("32.0.15.9227"))
            .expect("exactly the floor is allowed");
        profile
            .verify_driver_floor(Some("32.0.15.9228"))
            .expect("one patch above the floor is allowed");
        assert!(
            floor_error("32.0.15.9227", Some("32.0.15.9226"))
                .contains("DEVICE_PROFILE_DRIVER_TOO_OLD")
        );
    }

    // 机群现装的两点：新分支的低 build 号仍然新过旧分支的高 build 号。
    #[test]
    fn driver_floor_carries_across_segments() {
        profile(serde_json::json!("32.0.15.9227"))
            .verify_driver_floor(Some("32.0.16.1074"))
            .expect("a newer branch outranks a higher build number in the older branch");
    }

    // 字符串序的翻车点是位数变化，不是分支号。build 从四位涨到五位时字符串比会把
    // 新驱动判成更旧（'1' < '9'），一次正常的自动更新就变成 preflight 拒绝
    // ——#328 故障 B 里自己扣扳机的那个机制。
    #[test]
    fn driver_floor_survives_a_build_number_gaining_a_digit() {
        assert!("32.0.15.10001" < "32.0.15.9227", "string order is the trap");
        assert!(
            parse_driver_version("32.0.15.10001") > parse_driver_version("32.0.15.9227"),
            "numeric order is the fix"
        );
        profile(serde_json::json!("32.0.15.9227"))
            .verify_driver_floor(Some("32.0.15.10001"))
            .expect("a five digit build must not read as older than a four digit one");
        assert!(
            floor_error("32.0.15.10001", Some("32.0.15.9227"))
                .contains("DEVICE_PROFILE_DRIVER_TOO_OLD"),
            "and the reverse still has to be refused"
        );
    }

    #[test]
    fn absent_floor_skips_the_check_entirely() {
        let profile = profile(serde_json::Value::Null);
        profile
            .verify_driver_floor(None)
            .expect("dev profiles without a floor do not need an observed version");
        profile
            .verify_driver_floor(Some("nonsense"))
            .expect("no floor means nothing to compare against");
    }

    #[test]
    fn unobservable_and_unparseable_versions_fail_closed_and_distinctly() {
        assert!(floor_error("32.0.15.9227", None).contains("DEVICE_PROFILE_DRIVER_UNAVAILABLE"));
        assert!(
            floor_error("32.0.15.9227", Some("32.0.15"))
                .contains("DEVICE_PROFILE_DRIVER_UNREADABLE")
        );
        assert!(
            floor_error("32.0.15.9227", Some("32.0.15.9227.1"))
                .contains("DEVICE_PROFILE_DRIVER_UNREADABLE")
        );
        assert!(
            floor_error("566.36", Some("32.0.15.9227"))
                .contains("DEVICE_PROFILE_DRIVER_FLOOR_INVALID")
        );
    }

    fn validate(floor: serde_json::Value, device_ids: serde_json::Value) -> anyhow::Result<()> {
        let json = profile_json(floor, device_ids);
        let profile: DeviceProfile = serde_json::from_value(json)?;
        let reference = EntryReference {
            id: profile.profile_id.clone(),
            version: profile.version.clone(),
            content_hash: "sha256:unused".into(),
        };
        validate_device(&profile, &reference, CatalogSchemaGeneration::Legacy)
    }

    /// 驱动地板是可选的，型号清单为空表示「不按型号筛」。从前 certified 额外要求必须
    /// 声明地板；那条渠道条件随车道退役（docs/100），地板的**格式**校验仍在。
    #[test]
    fn an_absent_floor_and_an_empty_adapter_list_are_both_accepted() {
        validate(serde_json::json!("32.0.15.9227"), serde_json::json!([]))
            .expect("an empty adapter list is how a new GPU model ships with zero catalog changes");
        validate(serde_json::Value::Null, serde_json::json!([11524]))
            .expect("a profile may filter by adapter id without declaring a driver floor");
        validate(serde_json::Value::Null, serde_json::json!([]))
            .expect("empty-means-skip is the remaining convention");
    }

    #[test]
    fn malformed_floors_and_retired_fields_are_refused_at_authoring_time() {
        let error = validate(serde_json::json!("32.0.15"), serde_json::json!([]))
            .expect_err("a floor that cannot be parsed must not reach the preflight");
        assert!(error.to_string().contains("four numeric segments"));

        let mut retired = profile_json(serde_json::json!("32.0.15.9227"), serde_json::json!([]));
        retired["schema_version"] = serde_json::json!("dm-device-profile-1");
        retired["driver_versions"] = serde_json::json!(["32.0.15.9227"]);
        assert!(
            serde_json::from_value::<DeviceProfile>(retired).is_err(),
            "deny_unknown_fields makes the v1 whitelist a parse error, not a silent no-op"
        );
    }
}

// P1 verify core for server-delivered catalogs (docs/16-signed-catalog-delivery.md).
// These are hardware-free, feature-independent unit tests: they sign the *real*
// committed COD bundle bytes with an ephemeral, deterministic in-process key (no
// committed secret, no pinned trust anchor) and exercise verify / tamper / wrong-key
// / malformed-signature / fail-closed. The bytes are included directly so the tests
// run under the default CI job regardless of the dev-catalog / cod feature flags.
#[cfg(test)]
mod signed_bundle_tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    const COD_MANIFEST: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../config/catalogs/dev/cod-steam-release-test/catalog.json"
    ));
    const COD_DEVICE: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../config/catalogs/dev/cod-steam-release-test/device-profile.json"
    ));
    const COD_RECORDING: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../config/catalogs/dev/cod-steam-release-test/recording-profile.json"
    ));
    const COD_GAME: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../config/catalogs/dev/cod-steam-release-test/game-profile.json"
    ));
    const COD_MAPPING: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../config/catalogs/dev/cod-steam-release-test/mapping.json"
    ));

    const DEVICE_REFS: &[&[u8]] = &[COD_DEVICE];
    const RECORDING_REFS: &[&[u8]] = &[COD_RECORDING];
    const GAME_REFS: &[&[u8]] = &[COD_GAME];
    const MAPPING_REFS: &[&[u8]] = &[COD_MAPPING];
    const TEST_EXECUTABLE_SHA256: &str =
        "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn depth_pair_bytes() -> (Vec<u8>, Vec<u8>) {
        (
            test_depth_profile_bytes("call-of-duty-steam-release-test", TEST_EXECUTABLE_SHA256),
            test_camera_binding_bytes(
                "call-of-duty-steam-release-test",
                "cod-camera-fixture",
                "3",
                TEST_EXECUTABLE_SHA256,
            ),
        )
    }

    // Ephemeral, deterministic dev key: proves the mechanism without committing a
    // secret or a fake trust anchor. Real trust-anchor pinning is a later phase.
    fn dev_signing_key() -> SigningKey {
        SigningKey::from_bytes(&[7u8; 32])
    }

    fn sign_manifest(manifest: &[u8]) -> [u8; 64] {
        let mut input = CATALOG_SIGNATURE_DOMAIN.to_vec();
        input.extend_from_slice(manifest);
        dev_signing_key().sign(&input).to_bytes()
    }

    fn bundle<'a>(
        manifest: &'a [u8],
        signature: &'a [u8],
        device_refs: &'a [&'a [u8]],
    ) -> SignedCatalogSources<'a> {
        SignedCatalogSources {
            manifest,
            signature,
            devices: device_refs,
            recordings: RECORDING_REFS,
            games: GAME_REFS,
            mappings: MAPPING_REFS,
            depth_profiles: &[],
            camera_bindings: &[],
        }
    }

    fn manifest_with_depth_pair(
        include_binding: bool,
        depth_version: &str,
        depth_profile: &[u8],
        camera_binding: &[u8],
    ) -> Vec<u8> {
        let mut manifest: serde_json::Value = serde_json::from_slice(COD_MANIFEST).unwrap();
        manifest["depth_profiles"] = serde_json::json!([{
            "id": "call-of-duty-steam-release-test",
            "version": depth_version,
            "content_hash": sha256(depth_profile),
        }]);
        manifest["camera_bindings"] = if include_binding {
            serde_json::json!([{
                "id": "cod-camera-fixture",
                "version": "3",
                "content_hash": sha256(camera_binding),
            }])
        } else {
            serde_json::json!([])
        };
        serde_json::to_vec(&manifest).unwrap()
    }

    fn depth_bundle<'a>(
        manifest: &'a [u8],
        signature: &'a [u8],
        profiles: &'a [&'a [u8]],
        binding: &'a [&'a [u8]],
    ) -> SignedCatalogSources<'a> {
        SignedCatalogSources {
            manifest,
            signature,
            devices: DEVICE_REFS,
            recordings: RECORDING_REFS,
            games: GAME_REFS,
            mappings: MAPPING_REFS,
            depth_profiles: profiles,
            camera_bindings: binding,
        }
    }

    #[cfg(feature = "depth-lab")]
    #[test]
    fn signed_depth_pair_is_bound_to_exact_payload_bytes() {
        let (depth_profile, camera_binding) = depth_pair_bytes();
        let manifest = manifest_with_depth_pair(true, "1", &depth_profile, &camera_binding);
        let signature = sign_manifest(&manifest);
        let store = CatalogStore::load_from_signed_bundle(
            &dev_signing_key().verifying_key(),
            depth_bundle(
                &manifest,
                &signature,
                &[depth_profile.as_slice()],
                &[camera_binding.as_slice()],
            ),
        )
        .expect("signed depth pair loads");
        let selection = store.selection("call-of-duty-steam-release-test").unwrap();
        assert_eq!(
            selection.depth_profile.unwrap().source_bytes(),
            depth_profile
        );
        assert_eq!(
            selection.camera_binding.unwrap().source_bytes(),
            camera_binding
        );

        let mut tampered_binding = camera_binding.clone();
        tampered_binding.push(b' ');
        let error = expect_rejected(CatalogStore::load_from_signed_bundle(
            &dev_signing_key().verifying_key(),
            depth_bundle(
                &manifest,
                &signature,
                &[depth_profile.as_slice()],
                &[tampered_binding.as_slice()],
            ),
        ));
        assert!(error.contains("content hash mismatch"), "got: {error}");
    }

    #[cfg(feature = "depth-lab")]
    #[test]
    fn signed_depth_half_pair_is_rejected() {
        let (depth_profile, camera_binding) = depth_pair_bytes();
        let manifest = manifest_with_depth_pair(false, "1", &depth_profile, &camera_binding);
        let signature = sign_manifest(&manifest);
        let error = expect_rejected(CatalogStore::load_from_signed_bundle(
            &dev_signing_key().verifying_key(),
            depth_bundle(&manifest, &signature, &[depth_profile.as_slice()], &[]),
        ));
        assert!(error.contains("depth pair"), "got: {error}");
    }

    #[test]
    fn merging_same_bytes_with_different_manifest_version_fails_closed() {
        let bytes = b"same authenticated bytes";
        let entry = |version: &str| {
            CatalogEntrySnapshot::new("shared", version, bytes).expect("test entry is valid")
        };
        let mut destination = HashMap::from([("shared".to_owned(), entry("1"))]);
        let source = HashMap::from([("shared".to_owned(), entry("2"))]);
        let error = merge_entries(&mut destination, source, "depth profile", |snapshot| {
            snapshot
        })
        .unwrap_err()
        .to_string();
        assert!(error.contains("disagree"), "got: {error}");
    }

    // Avoids requiring `CatalogStore: Debug` (which `unwrap_err`/`expect_err` need).
    fn expect_rejected(result: anyhow::Result<CatalogStore>) -> String {
        match result {
            Ok(_) => panic!("expected signed-bundle rejection but it loaded"),
            Err(error) => error.to_string(),
        }
    }

    /// 只改 manifest 的 `catalog_id`，条目引用一字不动——造出「同一渠道下两份 bundle
    /// 引用同一批条目」这个形状。机群 device profile 就是这样：每份每游戏 bundle 都
    /// 引用同一个 `fleet-windows-nvidia-v1`。
    fn manifest_as_another_catalog(catalog_id: &str, device_hash: Option<&str>) -> Vec<u8> {
        let mut value: serde_json::Value =
            serde_json::from_slice(COD_MANIFEST).expect("manifest is valid JSON");
        value["catalog_id"] = serde_json::Value::String(catalog_id.to_owned());
        if let Some(hash) = device_hash {
            value["device_profiles"][0]["content_hash"] =
                serde_json::Value::String(hash.to_owned());
        }
        serde_json::to_vec(&value).expect("manifest reserializes")
    }

    fn load_dev_bundle(manifest: &[u8], devices: &[&[u8]]) -> anyhow::Result<CatalogStore> {
        CatalogStore::load_from_signed_bundle(
            &dev_signing_key().verifying_key(),
            bundle(manifest, &sign_manifest(manifest), devices),
        )
    }

    fn load_depth_dev_bundle(catalog_id: &str) -> CatalogStore {
        let (depth_profile, camera_binding) = depth_pair_bytes();
        let mut manifest: serde_json::Value = serde_json::from_slice(&manifest_with_depth_pair(
            true,
            "1",
            &depth_profile,
            &camera_binding,
        ))
        .unwrap();
        manifest["catalog_id"] = serde_json::json!(catalog_id);
        let manifest = serde_json::to_vec(&manifest).unwrap();
        let signature = sign_manifest(&manifest);
        CatalogStore::load_from_signed_bundle(
            &dev_signing_key().verifying_key(),
            depth_bundle(
                &manifest,
                &signature,
                &[depth_profile.as_slice()],
                &[camera_binding.as_slice()],
            ),
        )
        .expect("depth bundle is internally valid")
    }

    fn merged_catalog_ids(store: &CatalogStore) -> Vec<String> {
        store
            .status()
            .active_catalogs
            .into_iter()
            .map(|identity| identity.catalog_id)
            .collect()
    }

    const SECOND_CATALOG_ID: &str = "dataminer-another-game-dev";

    #[test]
    fn merging_shared_game_with_different_depth_presence_fails_in_both_orders() {
        let mut plain_then_depth =
            load_dev_bundle(COD_MANIFEST, DEVICE_REFS).expect("plain bundle loads");
        let error = plain_then_depth
            .merge_same_provenance(load_depth_dev_bundle(SECOND_CATALOG_ID))
            .expect_err("a shared game cannot acquire depth bytes from another manifest");
        assert!(
            error.to_string().contains("same depth pair"),
            "got: {error}"
        );

        let mut depth_then_plain = load_depth_dev_bundle(SECOND_CATALOG_ID);
        let error = depth_then_plain
            .merge_same_provenance(
                load_dev_bundle(COD_MANIFEST, DEVICE_REFS).expect("plain bundle loads"),
            )
            .expect_err("delivery order must not change the shared game's depth provenance");
        assert!(
            error.to_string().contains("same depth pair"),
            "got: {error}"
        );
    }

    /// 集合下发把一条渠道的 N 份 bundle 合成一个 store。两份共用同一个条目是正常的，
    /// 判据是完整签名身份：id/version/content_hash 都相同就去重，不报「重复条目」。
    ///
    /// 这里两份 bundle 引用的是**同一批**条目（只有 catalog_id 不同），所以四类都走到
    /// 了去重那一支。
    #[test]
    fn merging_two_catalogs_that_share_entries_dedupes_by_identity() {
        let mut merged = load_dev_bundle(COD_MANIFEST, DEVICE_REFS).expect("first bundle loads");
        let second_manifest = manifest_as_another_catalog(SECOND_CATALOG_ID, None);
        let second = load_dev_bundle(&second_manifest, DEVICE_REFS).expect("second bundle loads");
        merged
            .merge_same_provenance(second)
            .expect("identity-equal shared entries must dedupe, not conflict");
        let status = merged.status();
        assert_eq!(status.active_catalogs.len(), 2);
        assert_eq!(
            status.entry_counts.devices, 1,
            "a shared device profile must not be duplicated"
        );
        assert_eq!(status.entry_counts.recordings, 1);
        assert_eq!(status.entry_counts.mappings, 1);
    }

    /// `active_catalogs` 的顺序只由 catalog_id 决定，与下发顺序无关。
    /// `try_activate_catalog` 拿这个 `Vec` 的相等性判断「是不是已经在跑的那份」——
    /// 顺序跟着下发飘的话，每一轮刷新都会白激活一次。
    #[test]
    fn merged_catalog_order_does_not_depend_on_delivery_order() {
        let second_manifest = manifest_as_another_catalog(SECOND_CATALOG_ID, None);
        let mut forward = load_dev_bundle(COD_MANIFEST, DEVICE_REFS).unwrap();
        forward
            .merge_same_provenance(load_dev_bundle(&second_manifest, DEVICE_REFS).unwrap())
            .unwrap();
        let mut reverse = load_dev_bundle(&second_manifest, DEVICE_REFS).unwrap();
        reverse
            .merge_same_provenance(load_dev_bundle(COD_MANIFEST, DEVICE_REFS).unwrap())
            .unwrap();
        assert_eq!(merged_catalog_ids(&forward), merged_catalog_ids(&reverse));
        // 定序只按 catalog_id 的字典序，与投递顺序无关：
        // "dataminer-another-game-dev" < "dataminer-call-of-duty-steam-release-test"。
        assert_eq!(
            merged_catalog_ids(&forward),
            vec![
                SECOND_CATALOG_ID.to_owned(),
                "dataminer-call-of-duty-steam-release-test".to_owned(),
            ]
        );
    }

    /// 同一个 id 两份不同字节：没有哪一边更可信，fail-closed。
    #[test]
    fn merging_catalogs_that_disagree_about_one_entry_fails_closed() {
        let mut device: serde_json::Value =
            serde_json::from_slice(COD_DEVICE).expect("device profile is valid JSON");
        let build = device["minimum_windows_build"]
            .as_u64()
            .expect("device profile declares minimum_windows_build");
        device["minimum_windows_build"] = serde_json::json!(build + 1);
        let device = serde_json::to_vec(&device).expect("device profile reserializes");
        let second_manifest =
            manifest_as_another_catalog(SECOND_CATALOG_ID, Some(&sha256(&device)));

        let mut merged = load_dev_bundle(COD_MANIFEST, DEVICE_REFS).expect("first bundle loads");
        let second = load_dev_bundle(&second_manifest, &[device.as_slice()])
            .expect("the second bundle is internally consistent on its own");
        let error = merged
            .merge_same_provenance(second)
            .expect_err("two different bytes under one id must fail closed");
        assert!(
            error.to_string().contains("disagree about device entry"),
            "got: {error}"
        );
    }

    /// 同一个 catalog 在一个集合里出现两次是对端在自相矛盾，不是共用条目。
    #[test]
    fn merging_the_same_catalog_twice_is_rejected() {
        let mut merged = load_dev_bundle(COD_MANIFEST, DEVICE_REFS).unwrap();
        let error = merged
            .merge_same_provenance(load_dev_bundle(COD_MANIFEST, DEVICE_REFS).unwrap())
            .expect_err("the same catalog_id twice must be rejected");
        assert!(
            error.to_string().contains("delivered twice"),
            "got: {error}"
        );
    }

    #[test]
    fn valid_signed_bundle_verifies_and_reuses_validation() {
        let key = dev_signing_key().verifying_key();
        let signature = sign_manifest(COD_MANIFEST);
        let store = CatalogStore::load_from_signed_bundle(
            &key,
            bundle(COD_MANIFEST, &signature, DEVICE_REFS),
        )
        .expect("valid signed COD bundle must load");
        assert_eq!(store.provenance, CatalogProvenance::Delivered);
        assert!(store.games.contains_key("call-of-duty-steam-release-test"));
        // The reused cross-reference resolution still works end to end.
        let selection = store
            .selection("call-of-duty-steam-release-test")
            .expect("signed bundle cross-references resolve");
        assert_eq!(
            selection.catalog.catalog_id,
            "dataminer-call-of-duty-steam-release-test"
        );
    }

    #[test]
    fn tampered_manifest_fails_signature_verification() {
        let key = dev_signing_key().verifying_key();
        let signature = sign_manifest(COD_MANIFEST); // signed over the pristine manifest
        let mut manifest = COD_MANIFEST.to_vec();
        manifest.insert(0, b' '); // still valid JSON, different bytes
        let error = expect_rejected(CatalogStore::load_from_signed_bundle(
            &key,
            bundle(&manifest, &signature, DEVICE_REFS),
        ));
        assert!(
            error.contains("signature verification failed"),
            "expected signature failure, got: {error}"
        );
    }

    #[test]
    fn tampered_profile_fails_content_hash_despite_valid_signature() {
        let key = dev_signing_key().verifying_key();
        let signature = sign_manifest(COD_MANIFEST);
        // The signature still verifies (it covers only the manifest), but the swapped
        // profile no longer matches the content hash the signed manifest pins.
        let mut device = COD_DEVICE.to_vec();
        device.insert(0, b' ');
        let device_refs: [&[u8]; 1] = [device.as_slice()];
        let error = expect_rejected(CatalogStore::load_from_signed_bundle(
            &key,
            bundle(COD_MANIFEST, &signature, &device_refs),
        ));
        assert!(
            error.contains("content hash mismatch"),
            "expected content hash mismatch, got: {error}"
        );
    }

    #[test]
    fn wrong_pinned_key_is_rejected() {
        let wrong_key = SigningKey::from_bytes(&[9u8; 32]).verifying_key();
        let signature = sign_manifest(COD_MANIFEST); // signed by the [7u8; 32] key
        let error = expect_rejected(CatalogStore::load_from_signed_bundle(
            &wrong_key,
            bundle(COD_MANIFEST, &signature, DEVICE_REFS),
        ));
        assert!(
            error.contains("signature verification failed"),
            "expected signature failure, got: {error}"
        );
    }

    #[test]
    fn malformed_signature_is_rejected() {
        let key = dev_signing_key().verifying_key();
        let signature = sign_manifest(COD_MANIFEST);
        let error = expect_rejected(CatalogStore::load_from_signed_bundle(
            &key,
            bundle(COD_MANIFEST, &signature[..63], DEVICE_REFS),
        ));
        assert!(
            error.contains("not a valid Ed25519 signature"),
            "expected signature parse failure, got: {error}"
        );
    }

    // ---- P2: pinned trust anchors + channel-based key selection ---------------

    // The dev delivery key whose public half is the pinned DEV_CATALOG_ANCHOR. Its
    // 32-byte seed is dev-only, non-production; the real signer tool holds the same
    // seed (kept out of KMS on purpose, since dev bundles are always non-trainable).
    fn dev_delivery_key() -> SigningKey {
        SigningKey::from_bytes(b"dm-dev-catalog-delivery-seed-v1\0")
    }

    fn dev_delivery_sign(manifest: &[u8]) -> [u8; 64] {
        let mut input = CATALOG_SIGNATURE_DOMAIN.to_vec();
        input.extend_from_slice(manifest);
        dev_delivery_key().sign(&input).to_bytes()
    }

    /// 一个 store 只能有**一个**来源。断代前这条不变量写在渠道上（`mode` 相等），
    /// 而那个词同时兼职表达环境与训练资格（issue #855 §2）；换成 provenance 之后它
    /// 表达的正好是原本要表达的那件事：验签过的字节不得与没验签过的混进同一个索引，
    /// 否则一款内嵌目录就搭上了下发集合的信任。
    ///
    /// **schema 世代不在此列**——每游戏独立切 current，混代是断代窗口期的常态，
    /// 契约因此逐游戏派生（`CatalogGame::session_contract`）。
    #[test]
    fn compiled_and_delivered_catalogs_do_not_merge() {
        let signature = dev_delivery_sign(COD_MANIFEST);
        let mut delivered =
            CatalogStore::load_signed_bundle(bundle(COD_MANIFEST, &signature, DEVICE_REFS))
                .expect("the delivered COD bundle must verify");
        let error = delivered
            .merge_same_provenance(CatalogStore::empty())
            .unwrap_err();
        assert!(
            error.to_string().contains("catalog_provenance_mismatch"),
            "got: {error}"
        );
    }

    #[test]
    fn dev_signed_bundle_loads_under_pinned_anchor() {
        let signature = dev_delivery_sign(COD_MANIFEST);
        let store = CatalogStore::load_signed_bundle(bundle(COD_MANIFEST, &signature, DEVICE_REFS))
            .expect("dev-delivery-signed bundle must load under the pinned anchor");
        assert_eq!(store.provenance, CatalogProvenance::Delivered);
        assert!(store.games.contains_key("call-of-duty-steam-release-test"));
    }

    #[test]
    fn bundle_signed_by_unpinned_key_is_rejected() {
        // Signed by the P1 ephemeral test key, which is NOT the pinned dev anchor.
        let signature = sign_manifest(COD_MANIFEST);
        let error = expect_rejected(CatalogStore::load_signed_bundle(bundle(
            COD_MANIFEST,
            &signature,
            DEVICE_REFS,
        )));
        assert!(
            error.contains("signature verification failed"),
            "expected signature failure under the pinned anchor, got: {error}"
        );
    }
}

// 启动门禁（docs/100）。grant 车道废除后这条闸只剩一问：游戏在不在活跃目录里。
// 原神那条「未授权真实游戏开发目录要显式 debug-only 开关」的分支随该 bundle 一起
// 删除（issue #855 PR 2）——目录里没有它，就没有需要单独关起来的条目了。「哪台机器
// 能录哪些游戏」由签名目录 + 设备鉴权 + 运营 `--allow-capture` 三重锁在服务端回答，
// 客户端不再自持任何凭证（docs/100 §2 第 2 条）。
//
// 夹具用编译内嵌的 synthetic bundle，因此模块 cfg 必须与 `SYNTHETIC_*_BYTES` 的
// 可用条件对齐（`any(dev-catalog, all(test, depth-lab))`）；并且走**未加特性门**的
// `from_sources` 而不是 `from_dev_sources`，好让这条闸在两种特性组合下都被编译到。
#[cfg(all(test, any(feature = "dev-catalog", feature = "depth-lab")))]
mod start_gate_tests {
    use super::*;

    fn synthetic_store() -> CatalogStore {
        CatalogStore::from_sources(CatalogSources {
            manifest: SYNTHETIC_CATALOG_BYTES,
            devices: &[SYNTHETIC_DEVICE_BYTES],
            recordings: &[SYNTHETIC_RECORDING_BYTES],
            games: &[SYNTHETIC_GAME_BYTES],
            mappings: &[SYNTHETIC_MAPPING_BYTES],
            depth_profiles: &[],
            camera_bindings: &[],
        })
        .expect("the synthetic dev bundle loads")
    }

    /// 目录里有这个游戏就放行，不问任何凭证。
    #[test]
    fn a_catalog_game_starts_without_consulting_any_credential() {
        synthetic_store()
            .ensure_start_authorized("synthetic-win32-dev")
            .unwrap();
    }

    /// 不在目录里的游戏仍然 fail-closed——这是这条闸剩下的全部内容。
    #[test]
    fn a_game_outside_the_catalog_cannot_start() {
        let error = synthetic_store()
            .ensure_start_authorized("a-game-with-no-catalog-entry")
            .expect_err("a game with no catalog entry must not start");
        assert!(
            error.to_string().contains("CATALOG_NO_MATCH"),
            "got: {error}"
        );
    }
}

// Cross-check every committed bundle's mapping against its own input contract.
// A binding can be dead in two ways, and neither is visible at runtime — capture
// succeeds, frames look healthy, the label is simply never emitted:
//
//   * the VK is absent from `keyboard_vks`, so `handle_keyboard` returns before
//     the event is ever recorded (window_tracker.rs);
//   * the VK is listed in `control_vks`, so the recorded event carries
//     `control: true` and `FrameProjector::project_segment` skips it before
//     `apply_event` (capture-core projection.rs) — the key reaches neither
//     `keys` nor `actions`.
//
// `release/cod-steam` shipped five profile versions with `move.prone` (Left
// Control) and `communication.ping_wheel` (Left Alt) dead the second way,
// because `control_vks` carried 17/18 alongside the F8/F10 safety keys. The
// safety hotkeys do not need them: Ctrl+Alt+F8/F9/F10 are registered through
// `RegisterHotKey`/`WM_HOTKEY` in global_hotkeys.rs, which never touches Raw
// Input. What 17/18 did buy was an exemption from the arming neutrality gate
// (`neutral_at` skips application-control keys, and a non-neutral anchor fails
// the run with `INPUT_NOT_NEUTRAL`); every other catalog that binds Ctrl or Alt
// to a game action — Elden Ring, both Halos, Wolfenstein II, BioShock — already
// accepts that cost rather than losing the label, so `control_vks` is
// `[119, 121]` across the board.
//
// This check is deliberately not feature-gated and reads the bundles off disk
// rather than through `include_bytes!`, so it covers every committed catalog
// under CI's plain `cargo test -p dataminer-client --lib`.
#[cfg(test)]
mod input_contract_coverage_tests {
    use super::*;
    use capture_core::mapping::{GameMapping, InputDevice};
    use std::fs;
    use std::path::{Path, PathBuf};

    // Set 1 ("XT") scan codes as Raw Input reports them in `RAWKEYBOARD.MakeCode`,
    // paired with the `VKey` the same packet carries. Raw Input reports the
    // *generic* modifier VKs — VK_SHIFT/VK_CONTROL/VK_MENU rather than the
    // side-specific VK_L*/VK_R* — which is why 42 and 54 both map to 16, and why
    // Left Control (29) collides with the 17 that used to sit in `control_vks`.
    const SET1: &[(u16, u16)] = &[
        (1, 27),   // Escape
        (2, 49),   // 1
        (3, 50),   // 2
        (4, 51),   // 3
        (5, 52),   // 4
        (6, 53),   // 5
        (7, 54),   // 6
        (8, 55),   // 7
        (9, 56),   // 8
        (10, 57),  // 9
        (11, 48),  // 0
        (12, 189), // -
        (13, 187), // =
        (14, 8),   // Backspace
        (15, 9),   // Tab
        (16, 81),  // Q
        (17, 87),  // W
        (18, 69),  // E
        (19, 82),  // R
        (20, 84),  // T
        (21, 89),  // Y
        (22, 85),  // U
        (23, 73),  // I
        (24, 79),  // O
        (25, 80),  // P
        (26, 219), // [
        (27, 221), // ]
        (28, 13),  // Enter
        (29, 17),  // Left Control
        (30, 65),  // A
        (31, 83),  // S
        (32, 68),  // D
        (33, 70),  // F
        (34, 71),  // G
        (35, 72),  // H
        (36, 74),  // J
        (37, 75),  // K
        (38, 76),  // L
        (39, 186), // ;
        (40, 222), // '
        (41, 192), // `
        (42, 16),  // Left Shift
        (43, 220), // \
        (44, 90),  // Z
        (45, 88),  // X
        (46, 67),  // C
        (47, 86),  // V
        (48, 66),  // B
        (49, 78),  // N
        (50, 77),  // M
        (51, 188), // ,
        (52, 190), // .
        (53, 191), // /
        (54, 16),  // Right Shift
        (55, 106), // Numpad *
        (56, 18),  // Left Alt
        (57, 32),  // Space
        (58, 20),  // Caps Lock
        (59, 112), // F1
        (60, 113), // F2
        (61, 114), // F3
        (62, 115), // F4
        (63, 116), // F5
        (64, 117), // F6
        (65, 118), // F7
        (66, 119), // F8
        (67, 120), // F9
        (68, 121), // F10
        (69, 144), // Num Lock
        (70, 145), // Scroll Lock
        (71, 103), // Numpad 7
        (72, 104), // Numpad 8
        (73, 105), // Numpad 9
        (74, 109), // Numpad -
        (75, 100), // Numpad 4
        (76, 101), // Numpad 5
        (77, 102), // Numpad 6
        (78, 107), // Numpad +
        (79, 97),  // Numpad 1
        (80, 98),  // Numpad 2
        (81, 99),  // Numpad 3
        (82, 96),  // Numpad 0
        (83, 110), // Numpad .
        (87, 122), // F11
        (88, 123), // F12
    ];

    // Same table for packets carrying the E0 prefix (`RI_KEY_E0`).
    const SET1_EXTENDED: &[(u16, u16)] = &[
        (28, 13),  // Numpad Enter
        (29, 17),  // Right Control
        (53, 111), // Numpad /
        (56, 18),  // Right Alt
        (71, 36),  // Home
        (72, 38),  // Up
        (73, 33),  // Page Up
        (75, 37),  // Left
        (77, 39),  // Right
        (79, 35),  // End
        (80, 40),  // Down
        (81, 34),  // Page Down
        (82, 45),  // Insert
        (83, 46),  // Delete
        (91, 91),  // Left Windows
        (92, 92),  // Right Windows
        (93, 93),  // Menu
    ];

    fn virtual_key(scan_code: u16, extended: bool) -> Option<u16> {
        let table = if extended { SET1_EXTENDED } else { SET1 };
        table
            .iter()
            .find(|(scan, _)| *scan == scan_code)
            .map(|(_, vk)| *vk)
    }

    fn catalog_bundles() -> Vec<PathBuf> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../config/catalogs");
        let mut bundles = Vec::new();
        for channel in fs::read_dir(root).expect("config/catalogs is readable") {
            let channel = channel.expect("catalog channel entry is readable").path();
            if !channel.is_dir() {
                continue;
            }
            for bundle in fs::read_dir(&channel).expect("catalog channel is readable") {
                let bundle = bundle.expect("catalog bundle entry is readable").path();
                if bundle.join("game-profile.json").is_file()
                    && bundle.join("mapping.json").is_file()
                {
                    bundles.push(bundle);
                }
            }
        }
        bundles.sort();
        assert!(
            bundles.len() >= 8,
            "expected the committed catalog bundles to be discovered, found {}",
            bundles.len()
        );
        bundles
    }

    fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> T {
        let bytes = fs::read(path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        serde_json::from_slice(&bytes).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
    }

    #[test]
    fn every_mapping_binding_survives_its_own_input_contract() {
        let mut dead = Vec::new();
        for bundle in catalog_bundles() {
            let label = bundle
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("<unnamed>")
                .to_owned();
            let game: GameProfile = read_json(&bundle.join("game-profile.json"));
            let mapping: GameMapping = read_json(&bundle.join("mapping.json"));
            let contract = &game.input_contract;

            for binding in &mapping.bindings {
                let semantic = &binding.semantic;
                match binding.device {
                    InputDevice::Keyboard => {
                        let scan_code =
                            binding.scan_code.expect("keyboard binding has a scan code");
                        let extended = binding
                            .extended
                            .expect("keyboard binding has an extended flag");
                        let Some(vk) = virtual_key(scan_code, extended) else {
                            dead.push(format!(
                                "{label}: scan {scan_code} (extended={extended}) -> {semantic} \
                                 is not in the Set 1 table above; add it so the binding can be checked"
                            ));
                            continue;
                        };
                        if !contract.allows_key(vk) {
                            dead.push(format!(
                                "{label}: scan {scan_code} (VK {vk}) -> {semantic} is missing from \
                                 keyboard_vks, so Raw Input drops the event and the action is never emitted"
                            ));
                        } else if contract.is_control_key(vk) {
                            dead.push(format!(
                                "{label}: scan {scan_code} (VK {vk}) -> {semantic} is in control_vks, \
                                 so projection skips the event and the action is never emitted"
                            ));
                        }
                    }
                    InputDevice::Mouse => {
                        if let Some(button) = binding.button
                            && !contract.allows_mouse_button(button)
                        {
                            dead.push(format!(
                                "{label}: mouse button {button} -> {semantic} is missing from \
                                 mouse_buttons, so the action is never emitted"
                            ));
                        }
                    }
                }
            }
        }
        assert!(
            dead.is_empty(),
            "catalog bindings that can never reach a frame:\n  {}",
            dead.join("\n  ")
        );
    }

    #[test]
    fn the_safety_hotkey_keys_are_the_only_declared_application_controls() {
        // F8/F10 are the safety controls; F9 (120) is the marker hotkey and must
        // stay out of every contract entirely so it can never masquerade as game
        // input. Anything else in `control_vks` silently costs a label the moment
        // a mapping binds it, which is exactly how COD lost prone and ping wheel.
        for bundle in catalog_bundles() {
            let game: GameProfile = read_json(&bundle.join("game-profile.json"));
            let contract = &game.input_contract;
            let label = bundle.display();
            for vk in &contract.control_vks {
                assert!(
                    matches!(vk, 17 | 18 | 119 | 121),
                    "{label}: unexpected application control VK {vk}"
                );
            }
            assert!(
                !contract.allows_key(120) && !contract.is_control_key(120),
                "{label}: the F9 marker hotkey must stay outside the Raw Input contract"
            );
        }
    }
}

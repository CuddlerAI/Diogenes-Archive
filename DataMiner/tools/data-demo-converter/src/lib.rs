use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, ensure};
use capture_core::audio_contract::{
    AUDIO_ARTIFACT_NAME, AUDIO_BITS_PER_SAMPLE, AUDIO_CHANNELS, AUDIO_SAMPLE_FORMAT,
    AUDIO_SAMPLE_RATE_HZ, SegmentAudioMetadata, SegmentAudioState, expected_manifest_payloads,
};
use capture_core::ingest_contract::{
    INGEST_V2_RECORDING_SCHEMA, INGEST_V2_SESSION_SCHEMA, RECORDING_SCHEMA_V1, RECORDING_SCHEMA_V2,
    SESSION_SCHEMA_V2, SESSION_SCHEMA_V3, SessionContract,
};
use capture_core::input::RawInputEvent;
use capture_core::mapping::MappingSnapshot;
use capture_core::profile::{AcceptedEncoderRelaxations, CaptureAudioProfile, CaptureProfile};
use capture_core::projection::{FrameProjector, FrameTiming, foveated_bin};
use flacenc::component::BitRepr;
use flacenc::error::Verify;
use mp4::{
    AvcConfig, AvcProfile, MediaConfig, MediaType, Mp4Config, Mp4Reader, Mp4Sample, Mp4Writer,
    TrackConfig, TrackType,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

mod incremental;

pub use incremental::{
    IncrementalReport, IncrementalWallClockContext, ReadySegmentRef, advance_incremental,
    finalize_incremental,
};

/// v1 RAW 会话（schema-1：写 `capture-profile.json`、**不带 catalog** 的最早一代）。
///
/// **真实的 schema-1 会话不可达**（2026-07-30 复核）：它带的是 `dm-mapping-1` 的
/// mapping.json，而无 catalog 的会话走 `MappingSnapshot::from_slice`，只接受
/// `dm-mapping-2`——于是在读 mapping 那一步以 `unsupported mapping schema` 失败。
/// 这个分支只有夹具能走到，而夹具是「schema-1 会话 + v2 mapping」这种**现场不存在
/// 的组合**（`fixture_with` 写的就是这个常量）。没有在入口处硬拒：收益是零，代价是
/// 整套用例连同夹具体系一起重排。要真退役，连夹具一起做，别只在入口加一句 ensure。
///
/// 不要把它与 **bundle-1 遗留世代**混为一谈（issue #811 说的「v1 旧 schema」是
/// 后者）：那一代是 session-2/3 的 schema 字符串 + `capture`/`settings` 两册
/// catalog + `dm-mapping-1`，**可达且被支持**——由 catalog 形状判代（
/// `RawCatalogIdentity::shape`），mapping 走 `from_legacy_v1_slice`，只对
/// local-debug 与 attested-trainable 开放。
const RAW_SESSION_SCHEMA_V1: &str = "dm-local-dev-session-1";
const RAW_SESSION_SCHEMA_V2: &str = SESSION_SCHEMA_V2;
const RAW_SESSION_SCHEMA_V3: &str = SESSION_SCHEMA_V3;
const RAW_INGEST_V2_SESSION_SCHEMA: &str = INGEST_V2_SESSION_SCHEMA;
const RAW_SEGMENT_SCHEMA: &str = RECORDING_SCHEMA_V1;
const RAW_AUDIO_SEGMENT_SCHEMA: &str = RECORDING_SCHEMA_V2;
const RAW_INGEST_V2_SEGMENT_SCHEMA: &str = INGEST_V2_RECORDING_SCHEMA;
const RAW_READY_STATE: &str = "non_trainable_ready";
/// 中性 v2 的规范 ready 口径，也是具结车道（docs/141 §3 路线 B）早期 manual
/// 镜像段使用的口径。中性 session 必须用它；legacy local-debug 仍只接受
/// `non_trainable_ready`，不会因为新世代上线而放宽存量契约。
const RAW_PLAIN_READY_STATE: &str = "ready";
const RAW_CONSENTED_READY_STATE: &str = "consented_ready";
const RAW_RUN_COMPLETE_STATE: &str = "non_trainable_complete";
const RAW_INGEST_V2_RUN_COMPLETE_STATE: &str = "complete";
const RAW_CONSENTED_RUN_COMPLETE_STATE: &str = "consented_complete";
const CATALOG_MODE_DEV: &str = "dev_non_trainable";
const CATALOG_MODE_RELEASE: &str = "first_party_release";
const RAW_CONTRACT_VERSION: &str = "dm-1";
const LEGACY_DATA_MODEL_SCHEMA_VERSION: &str = "dm-1";
const DATA_MODEL_SCHEMA_VERSION: &str = "dm-2";
const WALL_CLOCK_PROJECTION_RANGE_ERROR: &str = "wall-clock projection exceeds supported i64 range";
const INVALID_RECORDED_WALL_CLOCK_ERROR: &str = "recorded wall-clock UTC anchor is invalid";
#[cfg(test)]
const TARGET_WIDTH: u32 = 1920;
#[cfg(test)]
const TARGET_HEIGHT: u32 = 1080;
#[cfg(test)]
const TARGET_FPS: u32 = 30;
#[cfg(test)]
const CHUNK_FRAMES: usize = 120;
const FRAMEPAIR_GAP: u64 = 8;
const FRAMEPAIR_STRIDE: u64 = 4;

// Aggregation semantics disclosed on every data card (docs/148 A1). These are
// facts about how p0.jsonl rows were produced — normative sources are
// docs/02 §3-§4 (frame-window projection) and docs/58 §1.4 (downsampling).
// They are duplicated as machine-readable strings so a consumer can assert on
// them without parsing prose; validate_stats accepts empty (pre-disclosure
// artifacts) or exactly these values.
const AGG_WINDOW_RULE: &str = "event_qpc_half_open[start,end)";
const AGG_KEY_SEMANTICS: &str = "held_at_window_end";
const AGG_BUTTON_SEMANTICS: &str = "held_at_window_end";
const AGG_POINTER_AGGREGATION: &str = "sum_raw_per_window_then_clamp_pm200_then_mulaw_bin_once";
const AGG_MULAW_SCHEME: &str = "dm1-v2(mu=8,ref=200,max_bin=10)";
const AGG_DOWNSAMPLE_RULE: &str = "windows_on_time;pointer=sum_per_window_never_sample;keys_buttons=coverage_ratio_not_or;never_reclamp;no_upsample";
const AGG_DUPLICATE_SEMANTICS: &str =
    "cfr_pacer_fact_recorded_at_capture_never_inferred_from_pixels";

#[cfg(test)]
thread_local! {
    static SEGMENT_VALIDATION_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn reset_segment_validation_count() {
    SEGMENT_VALIDATION_COUNT.set(0);
}

#[cfg(test)]
fn segment_validation_count() -> usize {
    SEGMENT_VALIDATION_COUNT.get()
}

/// Compliance stamping/validation profile for a conversion or verification.
///
/// `LocalDebugNonTrainable` is the historical default: it accepts only
/// non-trainable RAW chains and produces non-trainable, non-certified
/// datasets. `FirstPartyRelease` is the delivery path: it accepts only
/// consent-stamped RAW chains (`consented_ready` segments carrying the same
/// consent id) and produces `first_party`/`is_trainable`/`production_certified`
/// datasets. `AttestedTrainable` is the attested lane (docs/141 §3 route B): it
/// accepts truthful non-trainable RAW and produces `first_party_attested`/
/// `is_trainable` datasets whose eligibility is carried by a WAIVER reference,
/// never `production_certified`. There is deliberately no way to mix profiles:
/// every gate on both the input and output side derives from this one value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ComplianceProfile {
    LocalDebugNonTrainable,
    /// **当前不可达**（2026-07-30 复核，docs/57 §3.3 的通道退役之后）。
    ///
    /// 这条车道要求会话的 `catalog.mode == "first_party_release"`（见
    /// `validate_v2_catalog_snapshots`），而客户端的 `CatalogMode` 现在只剩
    /// `dev_non_trainable` 一个取值（`certified` 随平行授权车道退役，docs/100）
    /// ——没有任何客户端能再产出这种会话。
    /// v1 会话不带 catalog、绕得过那道门，但 v1 自己也不可达（见
    /// `RAW_SESSION_SCHEMA_V1`）。CLI 仍然暴露 `--first-party-release`，它现在只能
    /// 失败，失败在那句 catalog mode 的 ensure 上。
    ///
    /// 没有改成入口处 fail-fast：收益是零（这条路本来就走不到），代价是覆盖
    /// `stamp()` 与 consent 校验的两个用例一起失效，而那两段逻辑本身并没有退役。
    /// 真要退役是一次**带政策含义**的清理（"我们不再支持产出 first-party 可训练
    /// 数据集"），应当连同 CLI 参数、`stamp()` 的 release 分支与相关用例一起做，
    /// 由 owner 单独决定。
    FirstPartyRelease {
        consent_id: String,
    },
    /// 具结车道（docs/141 §3 路线 B）：如实接受 non-trainable RAW（session/segment
    /// `non_trainable=true`、`consent_id=null`、catalog `dev_non_trainable`），取数
    /// 资格不由采集时点 consent 承载，而由 owner 具结 WAIVER 的引用（`waiver_ref`）
    /// 承载。输出印章 `rights="first_party_attested"`——刻意区别于
    /// first-party-release 的 `"first_party"`，让下游能分辨证据强度——
    /// `is_trainable=true` 且 `production_certified` 恒为 false（具结≠certified）。
    /// 完整性类校验（六份 snapshot 逐字节 SHA-256、sha256.json 对账、segment
    /// 连续性、schema 白名单）与其它两档完全一致，一条不放松；本档不存在也不允许
    /// 任何「把 RAW 头改写成 consented 形状」的逻辑。
    AttestedTrainable {
        waiver_ref: String,
    },
}

impl ComplianceProfile {
    fn is_release(&self) -> bool {
        matches!(self, Self::FirstPartyRelease { .. })
    }

    fn is_attested(&self) -> bool {
        matches!(self, Self::AttestedTrainable { .. })
    }

    fn consent_id(&self) -> Option<&str> {
        match self {
            Self::LocalDebugNonTrainable | Self::AttestedTrainable { .. } => None,
            Self::FirstPartyRelease { consent_id } => Some(consent_id),
        }
    }

    fn validate(&self) -> anyhow::Result<()> {
        if let Self::FirstPartyRelease { consent_id } = self {
            ensure!(
                !consent_id.trim().is_empty(),
                "first-party-release profile requires a non-empty consent id"
            );
        }
        if let Self::AttestedTrainable { waiver_ref } = self {
            ensure!(
                !waiver_ref.trim().is_empty(),
                "attested-trainable profile requires a non-empty waiver ref"
            );
        }
        Ok(())
    }

    fn stamp(&self) -> ComplianceStamp {
        match self {
            Self::LocalDebugNonTrainable => ComplianceStamp {
                rights: "local_debug_non_trainable",
                first_party: false,
                consent_id: None,
                non_trainable: true,
                is_trainable: false,
                production_certified: false,
                annotation_source: "raw_input_recorded",
                annotation_note: "Local debug dm-1 Raw Input projection with frozen-mapping semantic actions; not production-certified or trainable.",
                tags: &["format_demo", "local_debug", "non_trainable"],
                attestation: None,
            },
            Self::FirstPartyRelease { consent_id } => ComplianceStamp {
                rights: "first_party",
                first_party: true,
                consent_id: Some(consent_id.clone()),
                non_trainable: false,
                is_trainable: true,
                production_certified: true,
                annotation_source: "human_recorded",
                annotation_note: "First-party dm-1 Raw Input projection with frozen-mapping semantic actions; consent-gated, production-certified, trainable.",
                tags: &["hf_teaser", "first_party", "trainable"],
                attestation: None,
            },
            Self::AttestedTrainable { waiver_ref } => ComplianceStamp {
                rights: "first_party_attested",
                first_party: true,
                consent_id: None,
                non_trainable: false,
                is_trainable: true,
                // 恒 false：具结承载的是取数资格，不是生产认证。
                production_certified: false,
                annotation_source: "human_recorded",
                annotation_note: "Attested-trainable dm-1 Raw Input projection with frozen-mapping semantic actions; eligibility carried by an owner attestation WAIVER reference (not a capture-time consent id); trainable, NOT production-certified.",
                tags: &["attested_trainable", "first_party"],
                attestation: Some(Attestation {
                    waiver_ref: waiver_ref.clone(),
                }),
            },
        }
    }
}

/// The single source of truth for every compliance field written into (and
/// expected back from) a dataset under a given profile.
struct ComplianceStamp {
    rights: &'static str,
    first_party: bool,
    consent_id: Option<String>,
    non_trainable: bool,
    is_trainable: bool,
    production_certified: bool,
    annotation_source: &'static str,
    annotation_note: &'static str,
    tags: &'static [&'static str],
    /// attested-trainable 专属：owner 具结 WAIVER 的引用。另两档恒为 `None`，
    /// 且序列化时按缺席处理（`skip_serializing_if`），保证 local-debug 与
    /// first-party-release 的输出字节零变化。
    attestation: Option<Attestation>,
}

/// 具结车道的治理指针（docs/141 §2）：`waiver_ref` 指向承载取数资格的 WAIVER
/// 对象（含日期），例如
/// `r2://diogenes-model-training/raw/v1/attested-trainable/WAIVER.json@2026-08-10`。
/// 写进 chunk meta / session metadata / _dataset_stats 三处，四处一致性的
/// 机器可读落点。
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct Attestation {
    waiver_ref: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConversionReport {
    pub session_id: String,
    pub frame_count: u64,
    pub chunk_count: u32,
    pub output: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerificationReport {
    pub session_id: String,
    pub frame_count: u64,
    pub chunk_count: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Mp4SyncReport {
    pub sample_count: u32,
    pub sync_samples: Vec<u32>,
    pub width: u16,
    pub height: u16,
    pub track_timescale: u32,
}

#[derive(Clone, Debug, Deserialize)]
struct RawSession {
    schema_version: String,
    session_id: String,
    #[serde(default)]
    non_trainable: Option<bool>,
    #[serde(default)]
    production_certified: Option<bool>,
    #[serde(default)]
    consent_id: Option<String>,
    game: RawGame,
    mapping: RawMappingIdentity,
    #[serde(default)]
    catalog: Option<RawCatalogIdentity>,
    capture_policy: RawCapturePolicy,
    contract: RawContract,
}

#[derive(Clone, Debug, Deserialize)]
struct RawGame {
    id: String,
}

#[derive(Clone, Debug, Deserialize)]
struct RawMappingIdentity {
    id: String,
    version: String,
    content_hash: String,
    #[serde(default)]
    recording_profile_id: Option<String>,
    /// 断代前的键名（docs/57 之前 mapping 引用 settings profile）。两个键按世代
    /// 互斥：bundle-1 会话只许写这个，其余世代只许写 `recording_profile_id`
    /// （`validate_session_header` 强制）。刻意不做 serde alias 合并——合并会让
    /// 「新世代却写旧键」这种杂交形态静默通过。
    #[serde(default)]
    settings_profile_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct RawCatalogIdentity {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mode: Option<String>,
    catalog_id: String,
    catalog_version: String,
    catalog_content_hash: String,
    /// How this exact catalog reached the recorder (`compiled` / `delivered`).
    /// Optional because archived RAW predates this audit fact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    provenance: Option<String>,
    device: RawCatalogEntryIdentity,
    /// capture + settings 两本册子并成一本（docs/57 §3.1）。这是 session.json 里
    /// catalog 段的读侧镜像，字段名必须与写侧（`RecordingCatalogContext`）一致。
    ///
    /// `Option` 不代表可缺席：断代前的 bundle-1 会话写的是 `capture` + `settings`
    /// 两条（issue #811 路线 A），断代后写 `recording` 一条。到底是哪一代由
    /// [`Self::shape`] 裁决，混装与全缺都在那里被拒——三个字段没有任何一个
    /// 允许独立缺席。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    recording: Option<RawCatalogEntryIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    capture: Option<RawCatalogEntryIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    settings: Option<RawCatalogEntryIdentity>,
    game: RawCatalogEntryIdentity,
    mapping: RawCatalogEntryIdentity,
}

/// session.json catalog 段的世代形状（docs/57 断代线两侧）。
enum RawCatalogShape<'a> {
    /// 断代后：单册 recording profile。
    Merged {
        recording: &'a RawCatalogEntryIdentity,
    },
    /// 断代前（bundle-1）：capture + settings 两册。只有 local-debug 与
    /// attested-trainable 两档走得进这一代（见 `validate_session_header`）。
    LegacyCaptureSettings {
        capture: &'a RawCatalogEntryIdentity,
        settings: &'a RawCatalogEntryIdentity,
    },
}

impl RawCatalogIdentity {
    fn shape(&self) -> anyhow::Result<RawCatalogShape<'_>> {
        match (&self.recording, &self.capture, &self.settings) {
            (Some(recording), None, None) => Ok(RawCatalogShape::Merged { recording }),
            (None, Some(capture), Some(settings)) => {
                Ok(RawCatalogShape::LegacyCaptureSettings { capture, settings })
            }
            _ => anyhow::bail!(
                "session catalog identity mixes generations: it must carry either a \
                 `recording` entry (post-docs/57) or `capture` + `settings` entries \
                 (pre-docs/57 bundle-1), never a hybrid"
            ),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct DataModelCatalog {
    provenance: String,
}

fn data_model_catalog(catalog: Option<&RawCatalogIdentity>) -> Option<DataModelCatalog> {
    catalog
        .and_then(|catalog| catalog.provenance.as_ref())
        .map(|provenance| DataModelCatalog {
            provenance: provenance.clone(),
        })
}

fn validate_data_model_catalog(catalog: Option<&DataModelCatalog>) -> anyhow::Result<()> {
    if let Some(catalog) = catalog {
        ensure!(
            matches!(catalog.provenance.as_str(), "compiled" | "delivered"),
            "unsupported catalog provenance"
        );
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct RawCatalogEntryIdentity {
    id: String,
    version: String,
    content_hash: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCatalogManifest {
    schema_version: String,
    catalog_id: String,
    catalog_version: String,
    /// manifest 不再自称 `catalog_mode`（docs/57 §3.3）：mode 由下发渠道决定，
    /// 会话侧的权威是 session.json 的 `catalog.mode`。
    #[serde(default)]
    non_trainable: Option<bool>,
    device_profiles: Vec<RawCatalogEntryIdentity>,
    recording_profiles: Vec<RawCatalogEntryIdentity>,
    game_profiles: Vec<RawCatalogEntryIdentity>,
    mappings: Vec<RawCatalogEntryIdentity>,
    #[serde(default, rename = "depth_profiles")]
    _depth_profiles: Vec<RawCatalogEntryIdentity>,
    #[serde(default, rename = "camera_bindings")]
    _camera_bindings: Vec<RawCatalogEntryIdentity>,
}

/// bundle-1 目录 manifest（docs/57 断代前）：五本册子、manifest 自带
/// `catalog_mode`。只被 bundle-1 遗留车道消费（issue #811 路线 A）；断代后的
/// manifest 走 `RawCatalogManifest`。`deny_unknown_fields` 与当代同纪律。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLegacyCatalogManifestV1 {
    schema_version: String,
    catalog_id: String,
    catalog_version: String,
    catalog_mode: String,
    non_trainable: bool,
    device_profiles: Vec<RawCatalogEntryIdentity>,
    capture_profiles: Vec<RawCatalogEntryIdentity>,
    game_profiles: Vec<RawCatalogEntryIdentity>,
    settings_profiles: Vec<RawCatalogEntryIdentity>,
    mappings: Vec<RawCatalogEntryIdentity>,
}

#[derive(Debug, Deserialize)]
struct RawDeviceProfileIdentity {
    schema_version: String,
    profile_id: String,
    version: String,
    #[serde(default)]
    non_trainable: Option<bool>,
}

/// bundle-1 的 game profile（`dm-game-profile-1`）：引用 capture/settings 两册，
/// mapping 引用还带版本（去版本化是 docs/57 §3.2 才发生的）。
#[derive(Debug, Deserialize)]
struct RawLegacyGameProfileIdentity {
    schema_version: String,
    game_id: String,
    version: String,
    device_profile_id: String,
    capture_profile_id: String,
    settings_profile_id: String,
    mapping_id: String,
    mapping_version: String,
    non_trainable: bool,
}

/// bundle-1 的 settings profile（`dm-settings-profile-1`）。converter 只消费
/// 身份、交叉引用与 view_scale 块；窗口模式等政策字段属客户端门禁，刻意不解析
/// （与 `RawRecordingProfile` 同一纪律，故不 `deny_unknown_fields`）。
#[derive(Debug, Deserialize)]
struct RawLegacySettingsProfile {
    schema_version: String,
    profile_id: String,
    version: String,
    game_id: String,
    capture_profile_id: String,
    non_trainable: bool,
    #[serde(default)]
    view_scale: Option<RawViewScaleBlock>,
}

#[derive(Debug, Deserialize)]
struct RawGameProfileIdentity {
    schema_version: String,
    game_id: String,
    version: String,
    device_profile_id: String,
    recording_profile_id: String,
    /// 引用只按 id、不按版本（docs/57 §3.2）：一份目录内每个 id 至多一个版本，
    /// 所以 `mapping_version` 这个字段整个消失了，而不是变成可选。
    mapping_id: String,
    #[serde(default)]
    non_trainable: Option<bool>,
}

/// 合并后的单册 recording profile（docs/57 §3.1）：引擎参数与游戏侧配置同一本。
///
/// 只声明 converter 真正消费的字段，不 `deny_unknown_fields`——政策字段
/// （ads_states、input_requirements 等）属客户端门禁，这里刻意不解析。
#[derive(Debug, Deserialize)]
struct RawRecordingProfile {
    schema_version: String,
    profile_id: String,
    version: String,
    game_id: String,
    #[serde(default)]
    non_trainable: Option<bool>,
    // ---- 引擎参数：投影进 capture-core 的那一段 ----
    capture_backend: String,
    encoder_backend: String,
    width: u32,
    height: u32,
    fps_num: u32,
    fps_den: u32,
    bitrate_bps: u32,
    max_bitrate_bps: u32,
    gop_frames: u32,
    segment_seconds: u32,
    test_segments: u32,
    #[serde(default)]
    accepted_encoder_relaxations: Option<AcceptedEncoderRelaxations>,
    #[serde(default)]
    audio: Option<CaptureAudioProfile>,
    // 视角尺度块（docs/45 §2.2）。converter 只消费换算需要的子集；缺省 = 老
    // profile，走"未标定"分支。
    #[serde(default)]
    view_scale: Option<RawViewScaleBlock>,
}

impl RawRecordingProfile {
    /// 引擎需要的那一份投影，与客户端 `RecordingProfile::capture_profile` 同一批字段。
    ///
    /// 这里只搬字段，判定仍交给 `CaptureProfile::validate`——引擎参数的权威（GOP =
    /// round(fps×4s)、码率区间、1920×1080/30 地板）在 capture-core，不在 converter。
    /// 两侧投影万一漂了也不会静默：`validate_capture_policy` 拿它跟 session.json 里
    /// 客户端自己写下的 capture_policy 对账。
    fn capture_profile(&self, expected_non_trainable: bool) -> anyhow::Result<CaptureProfile> {
        let profile = CaptureProfile {
            profile_id: self.profile_id.clone(),
            non_trainable: expected_non_trainable,
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
        };
        profile
            .validate()
            .context("recording-profile.json engine parameters are invalid")?;
        Ok(profile)
    }
}

#[derive(Debug, Deserialize)]
struct RawViewScaleBlock {
    #[serde(default)]
    applicable: Option<bool>,
    #[serde(default)]
    counts_per_360: Option<f64>,
    #[serde(default)]
    vertical_scale_ratio: Option<f64>,
    /// 只判"是否声明过"（docs/60 Q2 护栏）：逐档常数的语义与校验属客户端门禁，
    /// converter 不解析条目内容——逐档换算要等向导能实测 ADS 档位后才产品化。
    #[serde(default)]
    ads_states: Option<Vec<serde_json::Value>>,
}

/// 会话 recording 快照派生的角度换算常数（docs/46 §6）。
///
/// `None` = 该会话无可用标定：块缺失、`applicable != true`，或常数还没定版
/// （H-1 前目录里全是 null）。这是合法状态，角度列落 null；相反，声明了
/// applicable 却带非法数值（非有限/非正）是目录数据谎言，直接拒绝转换——
/// 与 register-catalog 审批门（C5）同一判据，防止未审批的 dev 目录静默产出
/// 错误角度。
#[derive(Clone, Copy, Debug, PartialEq)]
struct ViewScaleConstants {
    counts_per_360: f64,
    vertical_scale_ratio: f64,
}

fn view_scale_constants(
    recording: &RawRecordingProfile,
) -> anyhow::Result<Option<ViewScaleConstants>> {
    view_scale_constants_in("recording-profile.json", recording.view_scale.as_ref())
}

/// `source` 是块所在的快照文件名：断代后住 recording-profile.json，bundle-1 世代
/// 住 settings-profile.json（docs/45 §2.2 的块早于 docs/57 合册）。判定逻辑两代
/// 完全同一，只有拒绝信息里的出处不同。
fn view_scale_constants_in(
    source: &str,
    view_scale: Option<&RawViewScaleBlock>,
) -> anyhow::Result<Option<ViewScaleConstants>> {
    let Some(block) = view_scale else {
        return Ok(None);
    };
    if block.applicable != Some(true) {
        return Ok(None);
    }
    let validated = |label: &str, value: Option<f64>| -> anyhow::Result<Option<f64>> {
        match value {
            None => Ok(None),
            Some(number) if number.is_finite() && number > 0.0 => Ok(Some(number)),
            Some(number) => anyhow::bail!(
                "{source} view_scale.{label} is not a positive finite number: {number}"
            ),
        }
    };
    let counts_per_360 = validated("counts_per_360", block.counts_per_360)?;
    let vertical_scale_ratio = validated("vertical_scale_ratio", block.vertical_scale_ratio)?;
    // ads_states 护栏（docs/60 Q2 / docs/64 §3.2）：applicable=true 而 ads_states
    // 未声明时，converter 只有腰射常数——开镜时段游戏用的是另一套尺度，拿腰射
    // 常数换算全部帧会让 ADS 时段的角度标签系统性偏离，且此前没有任何门会发现。
    // 缺数据（null）模型知道没有，错数据模型会去学，所以这里判为未标定、角度列
    // 整列留白。真没有开镜机制的游戏也必须显式声明（如单条 hip 档），不许留空。
    if block.ads_states.as_ref().is_none_or(Vec::is_empty) {
        return Ok(None);
    }
    Ok(match (counts_per_360, vertical_scale_ratio) {
        (Some(counts_per_360), Some(vertical_scale_ratio)) => Some(ViewScaleConstants {
            counts_per_360,
            vertical_scale_ratio,
        }),
        // 常数未定版（测绘模式）：不猜、不补默认值。
        _ => None,
    })
}

#[derive(Clone, Debug, Deserialize)]
struct RawCapturePolicy {
    source: String,
    width: u32,
    height: u32,
    scale: bool,
    #[serde(default)]
    minimum_source_width: Option<u32>,
    #[serde(default)]
    minimum_source_height: Option<u32>,
    fps_num: u32,
    fps_den: u32,
    gop_frames: u32,
    audio: serde_json::Value,
}

#[derive(Clone, Debug, Deserialize)]
struct RawContract {
    version: String,
    pointer_clamp: i32,
    pointer_mu: f64,
    pointer_ref: i32,
    pointer_max_bin: i32,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawRun {
    schema_version: String,
    state: String,
    #[serde(default)]
    non_trainable: Option<bool>,
    #[serde(default)]
    production_certified: Option<bool>,
    session_id: String,
    run_id: String,
    mapping_id: String,
    mapping_version: String,
    mapping_content_hash: String,
    session_frame_start: u64,
    session_frame_end: u64,
    frame_count: u64,
    segment_count: u32,
    input_event_count: u64,
    input_queue_overflows: u64,
    // Timebase identity, and the wall clock the recorder paired with it. All
    // `default` because RAW written before the recorder emitted them parses as
    // absent, and absent only costs the wall-clock estimate — it must not fail
    // an otherwise valid conversion.
    #[serde(default)]
    qpc_frequency: Option<i64>,
    #[serde(default)]
    anchor_qpc: Option<i64>,
    #[serde(default)]
    anchor_at_utc: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawSegmentMeta {
    schema_version: String,
    session_id: String,
    run_id: String,
    segment_index: u32,
    session_frame_start: Option<u64>,
    n_frames: u64,
    state: String,
    #[serde(default)]
    non_trainable: Option<bool>,
    #[serde(default)]
    production_certified: Option<bool>,
    #[serde(default)]
    consent_id: Option<String>,
    input_status: String,
    qpc_start: i64,
    qpc_end: i64,
    fps_num: u32,
    fps_den: u32,
    width: u32,
    height: u32,
    mapping: RawMappingIdentity,
    #[serde(default)]
    catalog: Option<RawCatalogIdentity>,
    contract: RawContract,
    video: RawFileRef,
    frames: RawJsonlRef,
    events: RawJsonlRef,
    effective_encoder: RawEffectiveEncoder,
    probe: RawProbe,
    health: RawHealth,
    #[serde(default)]
    audio: Option<SegmentAudioMetadata>,
}

#[derive(Debug, Deserialize)]
struct RawFileRef {
    path: String,
    bytes: u64,
    sha256: String,
}

#[derive(Debug, Deserialize)]
struct RawJsonlRef {
    path: String,
    lines: u64,
    sha256: String,
}

#[derive(Debug, Deserialize)]
struct RawProbe {
    codec: String,
    h264_profile: u32,
    width: u32,
    height: u32,
    fps_num: u32,
    fps_den: u32,
    sample_count: u64,
    first_sample_clean_point: bool,
    pts_monotonic: bool,
}

#[derive(Debug, Deserialize)]
struct RawEffectiveEncoder {
    transform_clsid: Option<String>,
    friendly_name: Option<String>,
    hardware_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawHealth {
    queue_drops: u64,
    input_overflow: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct VideoDescriptor {
    movie_timescale: u32,
    track_timescale: u32,
    language: String,
    width: u16,
    height: u16,
    sequence_parameter_set: Vec<u8>,
    picture_parameter_set: Vec<u8>,
    sample_count: u32,
    sync_samples: Vec<u32>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct NativeFormat {
    width: u32,
    height: u32,
    fps_num: u32,
    fps_den: u32,
    chunk_frames: usize,
}

impl NativeFormat {
    fn from_profile(
        profile: &CaptureProfile,
        compliance: &ComplianceProfile,
    ) -> anyhow::Result<Self> {
        ensure!(
            profile.non_trainable != compliance.is_release(),
            "capture profile classification does not match the {} compliance profile",
            match compliance {
                ComplianceProfile::FirstPartyRelease { .. } => "first-party-release",
                ComplianceProfile::LocalDebugNonTrainable => "local-debug non-trainable",
                // 具结档同样只接受 non-trainable 的采集 profile（如实 RAW）。
                ComplianceProfile::AttestedTrainable { .. } => "attested-trainable non-trainable",
            }
        );
        Self::new(
            profile.width,
            profile.height,
            profile.fps_num,
            profile.fps_den,
            profile.gop_frames,
        )
    }

    fn from_capture(capture: &DatasetCapture) -> anyhow::Result<Self> {
        let native = Self::new(
            capture.width,
            capture.height,
            capture.fps_num,
            capture.fps_den,
            capture.chunk_frames,
        )?;
        ensure!(
            capture.fps == native.fps(),
            "dataset capture fps does not match its rational rate"
        );
        Ok(native)
    }

    fn new(
        width: u32,
        height: u32,
        fps_num: u32,
        fps_den: u32,
        chunk_frames: u32,
    ) -> anyhow::Result<Self> {
        ensure!(
            width > 0 && height > 0,
            "native dimensions must be positive"
        );
        ensure!(
            fps_num > 0 && fps_den > 0,
            "native frame rate must be positive"
        );
        ensure!(chunk_frames > 0, "native GOP must contain frames");
        let expected_chunk_frames =
            (u64::from(fps_num) * 4 + u64::from(fps_den) / 2) / u64::from(fps_den);
        ensure!(
            u64::from(chunk_frames) == expected_chunk_frames,
            "native GOP must equal round(fps * 4s)"
        );
        Ok(Self {
            width,
            height,
            fps_num,
            fps_den,
            chunk_frames: usize::try_from(chunk_frames)?,
        })
    }

    fn fps(self) -> u32 {
        u32::try_from(
            (u64::from(self.fps_num) + u64::from(self.fps_den) / 2) / u64::from(self.fps_den),
        )
        .unwrap_or(u32::MAX)
    }

    fn rate_label(self) -> String {
        format!("{}/{}", self.fps_num, self.fps_den)
    }

    fn video_time_ms(self, frame: u64) -> u64 {
        let denominator = u128::from(self.fps_num);
        let numerator = u128::from(frame) * 1_000 * u128::from(self.fps_den);
        ((numerator + denominator / 2) / denominator) as u64
    }
}

#[derive(Debug)]
struct ValidatedSegment {
    path: PathBuf,
    run_id: String,
    segment_index: u32,
    rows: Vec<RawFrameProjection>,
    events: Vec<RawInputEvent>,
    video: VideoDescriptor,
    source_hashes: BTreeMap<String, String>,
    audio: Option<SegmentAudioMetadata>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct RawFrameProjection {
    frame: u64,
    session_frame: u64,
    run_id: String,
    video_t_us: u64,
    output_qpc_start: i64,
    output_qpc_end: i64,
    source_qpc: i64,
    source_seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    present_seq: Option<u64>,
    duplicate: bool,
    source_superseded: u32,
    queue_drop: u64,
    keys: Vec<u16>,
    // Held mouse buttons at end of frame (#59 item 1).
    //
    // `Option`, not a defaulted `Vec`, because absent and empty must stay
    // distinguishable. RAW recorded before capture-core emitted this field has no
    // such key; re-projecting those events with today's projector *does* produce
    // buttons, so the frames-vs-events equality check below would fail on every
    // archived session that held a mouse button. Keeping absence visible lets
    // that check skip the field for exactly those rows. Serialization omits it
    // when absent so a re-serialized old row stays byte-honest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    buttons: Option<Vec<u16>>,
    actions: Vec<String>,
    #[serde(rename = "pointerDx")]
    pointer_dx: i32,
    #[serde(rename = "pointerDy")]
    pointer_dy: i32,
    #[serde(rename = "pointerBins")]
    pointer_bins: [i32; 2],
    events: Vec<RawFrameEvent>,
    src: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct RawFrameEvent {
    #[serde(rename = "type")]
    event_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    vk: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    scan_code: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    flags: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dx: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dy: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    delta: Option<i16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    horizontal: Option<bool>,
}

#[derive(Debug)]
struct ValidatedSession {
    session: RawSession,
    mapping: MappingSnapshot,
    native_format: NativeFormat,
    audio_enabled: bool,
    segments: Vec<ValidatedSegment>,
    compliance: ComplianceProfile,
    /// `None` when the session id carries no epoch or RAW recorded no tick rate;
    /// the conversion then simply ships without the wall-clock estimate.
    wall_clock: Option<WallClockAnchor>,
    // 来自 hash 复核过的 settings 快照（v1 会话恒 None）。
    view_scale: Option<ViewScaleConstants>,
}

/// Pairs the one wall-clock instant RAW implies with the tick it belongs to.
///
/// RAW records no wall clock anywhere, so every absolute time this crate emits
/// traces back to this single pair per session. Relative accuracy is the tick
/// rate; absolute accuracy is bounded by an unknown constant — how long after
/// session creation the first frame was captured.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct WallClockAnchor {
    epoch_us: i64,
    anchor_qpc: i64,
    qpc_frequency: i64,
}

impl WallClockAnchor {
    /// Prefers the instant the recorder actually measured and wrote into
    /// `run.json`. Only when RAW predates that field does this fall back to the
    /// epoch embedded in the session id — a value that tracks the tick well
    /// (±0.06 s across this corpus) but is offset by an unmeasurable constant,
    /// the delay from session creation to first captured frame.
    fn new(
        session_id: &str,
        first_frame_qpc: i64,
        recorded: Option<(&str, i64)>,
        qpc_frequency: Option<i64>,
    ) -> anyhow::Result<Option<Self>> {
        let measured = recorded
            .map(|(utc, qpc)| -> anyhow::Result<_> {
                let epoch_ms =
                    parse_iso8601_millis_utc(utc).context(INVALID_RECORDED_WALL_CLOCK_ERROR)?;
                let epoch_us = epoch_ms
                    .checked_mul(1_000)
                    .context(INVALID_RECORDED_WALL_CLOCK_ERROR)?;
                Ok((epoch_us, qpc))
            })
            .transpose()?;
        let Some(qpc_frequency) = qpc_frequency.filter(|frequency| *frequency > 0) else {
            return Ok(None);
        };
        let (epoch_us, anchor_qpc) = match measured {
            Some(pair) => pair,
            None => {
                let Some(epoch_us) =
                    session_epoch_ms(session_id).and_then(|epoch_ms| epoch_ms.checked_mul(1_000))
                else {
                    return Ok(None);
                };
                (epoch_us, first_frame_qpc)
            }
        };
        Ok(Some(Self {
            epoch_us,
            anchor_qpc,
            qpc_frequency,
        }))
    }

    fn estimate_us(&self, qpc: i64) -> anyhow::Result<i64> {
        ensure!(
            self.qpc_frequency > 0,
            "wall-clock QPC frequency must be positive"
        );
        let ticks = qpc
            .checked_sub(self.anchor_qpc)
            .context(WALL_CLOCK_PROJECTION_RANGE_ERROR)?;
        let delta_us = i128::from(ticks)
            .checked_mul(1_000_000)
            .context(WALL_CLOCK_PROJECTION_RANGE_ERROR)?
            / i128::from(self.qpc_frequency);
        let estimate = i128::from(self.epoch_us)
            .checked_add(delta_us)
            .context(WALL_CLOCK_PROJECTION_RANGE_ERROR)?;
        i64::try_from(estimate).map_err(|_| anyhow::anyhow!(WALL_CLOCK_PROJECTION_RANGE_ERROR))
    }
}

#[derive(Clone, Debug)]
struct ValidatedSessionHeader {
    session: RawSession,
    mapping: MappingSnapshot,
    profile: CaptureProfile,
    native_format: NativeFormat,
    view_scale: Option<ViewScaleConstants>,
}

// `Eq` 止步于此：角度列是 f64。行等值比较全部走 `PartialEq`（framepairs 重建
// 对账依赖它），NaN 不可能出现——常数经 `view_scale_constants` 拒绝了非有限值。
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct DataDemoFrame {
    frame: u64,
    gframe: u64,
    video_t_ms: u64,
    // The capture timebase, carried through so a consumer of the packed dataset
    // can time-align without reopening RAW. `video_t_ms` is derived from the CFR
    // grid (gframe), which says when a frame *should* sit; these two say when the
    // capture actually aggregated input for it, and they are what event
    // attribution is defined on: an event belongs to this frame iff its tick is
    // in [qpc, qpc_end). `default` keeps p0.jsonl written before this field
    // readable, same precedent as pointerRawDx/Dy above.
    #[serde(default)]
    qpc: i64,
    #[serde(default)]
    qpc_end: i64,
    // Estimated wall clock, µs since the Unix epoch. Nothing in RAW records a
    // wall clock, so this is reconstructed from the epoch embedded in the session
    // id, anchored at the session's first frame tick. It carries an unknown
    // constant bias — the delay from session creation to first captured frame —
    // and is absent entirely when the session id carries no parseable epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    t_utc_est_us: Option<i64>,
    keys: Vec<u16>,
    // Held mouse buttons at end of frame (#59 item 1), the other half of the
    // physical input state alongside `keys`. Carried through from RAW rather
    // than rebuilt here: a converter-side replay of `events` would read empty
    // after an incremental resume, because the projector's warm frame is
    // discarded and its synthetic `mouse_down` never reaches an emitted frame.
    #[serde(default)]
    buttons: Vec<u16>,
    // `actions` is the immutable semantic projection produced from this
    // session's frozen mapping. Keep the physical keys/events alongside it so
    // downstream users can audit or rebuild the derived labels.
    #[serde(default)]
    actions: Vec<String>,
    #[serde(rename = "pointerDx")]
    pointer_dx: i32,
    #[serde(rename = "pointerDy")]
    pointer_dy: i32,
    // Untruncated per-frame pointer sum (#59 item 2). `pointerDx/Dy` clamp to
    // ±200, which saturates on fast flicks and loses angular velocity; these are
    // the raw sum of this frame's `mouse_move` deltas, no clamp, so downstream
    // does not have to reopen `events` to recover it. `#[serde(default)]` keeps
    // older p0.jsonl (written before this field) readable — they parse as 0, and
    // verify only recomputes the field for frames that carry it.
    #[serde(rename = "pointerRawDx", default)]
    pointer_raw_dx: i32,
    #[serde(rename = "pointerRawDy", default)]
    pointer_raw_dy: i32,
    // 视角角度真值（docs/46 §6，`view_scale_norm@1`）：本帧 raw 计数和 × 会话
    // settings 快照的换算常数，弧度，+yaw 向右 / +pitch 向上。null = 该会话无
    // 有效标定（存量语料、applicable=false、常数未定版），mask 用
    // `view_scale_valid`；沿用 `input_render_latency_frames` 的"null=未标定、
    // 绝不编造"范式。显式序列化 null（不省略），让"没有"可见。`default` 保旧
    // p0.jsonl 可读：解析为 null + false，verify 按未标定分支放行。
    #[serde(default)]
    look_yaw_delta_rad: Option<f64>,
    #[serde(default)]
    look_pitch_delta_rad: Option<f64>,
    #[serde(default)]
    view_scale_valid: bool,
    #[serde(rename = "pointerBins")]
    pointer_bins: [i32; 2],
    events: Vec<RawFrameEvent>,
    src: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
struct FramePair {
    #[serde(rename = "clipId")]
    clip_id: String,
    #[serde(rename = "consentId")]
    consent_id: Option<String>,
    #[serde(rename = "frameT")]
    frame_t: u64,
    #[serde(rename = "gapH")]
    gap_h: u64,
    #[serde(rename = "chunkT")]
    chunk_t: u32,
    #[serde(rename = "chunkTH")]
    chunk_th: u32,
    #[serde(rename = "actionT")]
    action_t: FramePairAction,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
struct FramePairAction {
    keys: Vec<u16>,
    // #59 item 1: a click landing between stride samples is invisible to pair
    // supervision without this, and firing/aiming are the strongest visual
    // causes in an FPS.
    #[serde(default)]
    buttons: Vec<u16>,
    // FramePair consumers should not need to reopen p0.jsonl just to recover
    // the semantic action target for frameT. The default keeps older local
    // non-trainable DataModels readable while new outputs always materialize it.
    #[serde(default)]
    actions: Vec<String>,
    #[serde(rename = "pointerDx")]
    pointer_dx: i32,
    #[serde(rename = "pointerDy")]
    pointer_dy: i32,
    // Mirror of the frame's untruncated pointer sum (#59 item 2), so a FramePair
    // consumer training on angular velocity need not reopen p0.jsonl. `default`
    // keeps older framepairs.jsonl readable.
    #[serde(rename = "pointerRawDx", default)]
    pointer_raw_dx: i32,
    #[serde(rename = "pointerRawDy", default)]
    pointer_raw_dy: i32,
    // 帧行角度列的镜像（同 pointerRawDx 的理由：FramePair 消费方不该为了
    // 角度真值重开 p0.jsonl）。`default` 保旧 framepairs.jsonl 可读。
    #[serde(default)]
    look_yaw_delta_rad: Option<f64>,
    #[serde(default)]
    look_pitch_delta_rad: Option<f64>,
    #[serde(default)]
    view_scale_valid: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
struct ChunkMeta {
    schema_version: String,
    sample_key: String,
    session: String,
    chunk: u32,
    game: String,
    category: String,
    platform: String,
    video: DataDemoVideo,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    audio: Option<DataDemoAudio>,
    annotation_source: String,
    key_schema: String,
    rights: String,
    first_party: bool,
    consent_id: Option<String>,
    // 具结车道治理指针；仅 attested-trainable 输出携带。`skip_serializing_if`
    // 保证另两档输出字节零变化，`default` 保旧 DataModel 可读。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    attestation: Option<Attestation>,
    non_trainable: bool,
    is_trainable: bool,
    production_certified: bool,
    is_cutscene: bool,
    #[serde(default)]
    catalog: Option<DataModelCatalog>,
    provenance: ChunkProvenance,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
struct DataDemoVideo {
    file: String,
    codec: String,
    width: u32,
    height: u32,
    fps: u32,
    fps_num: u32,
    fps_den: u32,
    frames: u64,
    audio: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
struct DataDemoAudio {
    state: SegmentAudioState,
    source: String,
    present: bool,
    sample_rate: u32,
    channels: u16,
    bits: u16,
    sample_format: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    codec: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    frames: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sha256: Option<String>,
    failure_codes: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
struct ChunkProvenance {
    raw_session_id: String,
    raw_segments: Vec<String>,
    mapping_id: String,
    mapping_version: String,
    mapping_content_hash: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
struct SessionMetadata {
    schema_version: String,
    workflow_id: String,
    game: String,
    category: String,
    platform: String,
    source_video: String,
    org: String,
    capture: DatasetCapture,
    duration_ms: u64,
    frame_pairs: FramePairSummary,
    annotation: AnnotationSummary,
    rights: String,
    first_party: bool,
    consent_id: Option<String>,
    // 同 `ChunkMeta::attestation`：具结车道治理指针，仅 attested-trainable 携带。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    attestation: Option<Attestation>,
    non_trainable: bool,
    is_trainable: bool,
    production_certified: bool,
    is_cutscene: bool,
    tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    audio: Option<DatasetAudioSummary>,
    // 会话级视角标定声明（docs/46 §6）。null = 本会话角度列全空（未标定）。
    // 序列化为显式 null 不省略——同 `input_render_latency_frames` 的"缺席可见"
    // 纪律；`default` 保旧 metadata 可读。verify 拿它当逐帧角度重算的常数源，
    // 并由 RAW 绑定校验与会话 settings 快照对账。
    #[serde(default)]
    view_scale: Option<ViewScaleRef>,
    /// Catalog delivery provenance projected from the hash-validated RAW catalog
    /// identity. This is distinct from derived-data lineage in `provenance` below.
    #[serde(default)]
    catalog: Option<DataModelCatalog>,
    provenance: SessionProvenance,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
struct ViewScaleRef {
    /// 角度列语义版本，恒为 `view_scale::VIEW_SCALE_NORM`。
    norm: String,
    counts_per_360: f64,
    vertical_scale_ratio: f64,
}

impl ViewScaleRef {
    fn from_constants(constants: &ViewScaleConstants) -> Self {
        Self {
            norm: view_scale::VIEW_SCALE_NORM.into(),
            counts_per_360: constants.counts_per_360,
            vertical_scale_ratio: constants.vertical_scale_ratio,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
struct DatasetCapture {
    fps: u32,
    fps_num: u32,
    fps_den: u32,
    width: u32,
    height: u32,
    codec: String,
    frame_count: u64,
    chunks: u32,
    chunk_frames: u32,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
struct DatasetAudioSummary {
    source: String,
    codec: String,
    sample_rate: u32,
    channels: u16,
    bits: u16,
    sample_format: String,
    ready_chunks: u32,
    degraded_chunks: u32,
    absent_chunks: u32,
    all_publishable: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
struct FramePairSummary {
    count: u64,
    gap_h: u64,
    stride: u64,
    overlapping: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
struct AnnotationSummary {
    source: String,
    labeled_frames: u64,
    note: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
struct SessionProvenance {
    raw_schema_version: String,
    raw_session_id: String,
    mapping_id: String,
    mapping_version: String,
    mapping_content_hash: String,
    source_segments: Vec<SourceSegment>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    run_frame_ranges: Vec<RunFrameRange>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
struct SourceSegment {
    run_id: String,
    segment_index: u32,
    hashes: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct RunFrameRange {
    run_id: String,
    frame_start: u64,
    frame_end: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
struct DatasetStats {
    sessions: u32,
    games: Vec<String>,
    total_hours: f64,
    total_frames: u64,
    total_chunks: u32,
    total_frame_pairs: u64,
    datacard: DatasetCard,
    compliance: DatasetCompliance,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
struct DatasetCard {
    fps: u32,
    fps_num: u32,
    fps_den: u32,
    resolution: String,
    carrier: String,
    chunk_s: u32,
    gap_h: u64,
    stride: u64,
    overlapping: bool,
    org: String,
    merged_jsonl: bool,
    // Fixed disclosures (#59 item 3). These say what the DataModel does NOT carry, so a
    // downstream reader does not have to infer it from the absence of a field.
    #[serde(default)]
    disclosures: DatasetDisclosures,
}

/// What the DataModel structurally does not carry, plus the one number the converter
/// cannot know (#59 item 3).
///
/// The three `has_*` flags are facts about the format itself, not about this
/// session: dm-1/dm-2 record video, input events, and their frame projection — they have
/// no reward signal, no episode-termination flag, and no game-internal state
/// channel. MBRL-style paradigms can therefore only pretrain dynamics from it;
/// a reward channel (HUD OCR or similar) is an evaluation item for the full
/// schema and deliberately out of scope here.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
struct DatasetDisclosures {
    /// Always false: there is no reward channel in dm-1.
    has_reward_channel: bool,
    /// Always false: there is no episode-termination channel in dm-1.
    has_termination_channel: bool,
    /// Always false: no game-internal state (health, score, position) is read.
    has_game_state_channel: bool,
    /// Measured input→render latency for the capture machine, in frames.
    ///
    /// The converter **cannot** derive this: it is a per-capture-machine
    /// calibration made by inspecting footage, not a fact recorded in RAW. It is
    /// therefore `None` unless a measurement is supplied, and serializes as an
    /// explicit `null` rather than being omitted — an absent disclosure reads as
    /// "no latency issue", whereas `null` reads as "not calibrated", which is the
    /// truth. Fabricating a number here (for example reusing another machine's
    /// measurement) would put an unmeasured claim on a published data card.
    input_render_latency_frames: Option<u32>,
    /// Aggregation semantics of every p0.jsonl row (docs/148 A1). Machine-readable
    /// mirrors of the DATASHEET "Action aggregation & downsampling" section, so a
    /// consumer can assert on them instead of parsing prose. Normative sources:
    /// docs/02 §3-§4, docs/58 §1.4. Empty on artifacts that predate the
    /// disclosure (`serde(default)`); the writer always fills the canonical
    /// constants and `validate_stats` rejects any other non-empty value.
    #[serde(default)]
    window_rule: String,
    #[serde(default)]
    key_semantics: String,
    #[serde(default)]
    button_semantics: String,
    #[serde(default)]
    pointer_aggregation: String,
    #[serde(default)]
    mulaw_scheme: String,
    #[serde(default)]
    downsample_rule: String,
    #[serde(default)]
    duplicate_semantics: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
struct DatasetCompliance {
    all_non_trainable: bool,
    trainable_sessions: u32,
    production_certified_sessions: u32,
    // 同 `ChunkMeta::attestation`：具结车道治理指针，仅 attested-trainable 携带。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    attestation: Option<Attestation>,
    note: String,
}

pub fn inspect_sync_samples(path: &Path) -> anyhow::Result<Mp4SyncReport> {
    let descriptor = inspect_video(path, None, None)?;
    Ok(Mp4SyncReport {
        sample_count: descriptor.sample_count,
        sync_samples: descriptor.sync_samples,
        width: descriptor.width,
        height: descriptor.height,
        track_timescale: descriptor.track_timescale,
    })
}

/// Control-key re-arbitration policy for a whole conversion run.
///
/// `None` (the default) trusts the `control` flag persisted at capture time.
/// `Some(set)` recomputes it as "vk is in this set", which is what a corpus
/// captured under a wrong `control_vks` needs — see [`reproject_session`].
/// Set once from the CLI so `reproject` and `convert` cannot disagree: they
/// must replay the events identically or the cross-check in `validate_segment`
/// fails, which is exactly the guard that caught this being threaded through
/// only half the pipeline.
static CONTROL_POLICY: std::sync::OnceLock<BTreeSet<u16>> = std::sync::OnceLock::new();

/// Install the process-wide control policy. Idempotent; later calls are ignored.
pub fn set_control_policy(control_vks: &[u16]) {
    let _ = CONTROL_POLICY.set(control_vks.iter().copied().collect());
}

fn apply_control_policy(events: &mut [RawInputEvent]) -> u64 {
    let Some(control) = CONTROL_POLICY.get() else {
        return 0;
    };
    let mut freed = 0;
    for event in events {
        if let Some(vk) = event.vk {
            let now = control.contains(&vk);
            if event.control && !now {
                freed += 1;
            }
            event.control = now;
        }
    }
    freed
}

/// Per-segment record of what `reproject_session` changed.
#[derive(Debug, Serialize)]
pub struct ReprojectedSegment {
    pub run_id: String,
    pub segment: u32,
    pub frames_sha256_before: String,
    pub frames_sha256_after: String,
    /// Events whose persisted `control` flag the policy flipped to false, i.e.
    /// input the original projection dropped and this one keeps.
    pub freed_events: u64,
    pub rows_changed: u64,
}

#[derive(Debug, Serialize)]
pub struct ReprojectionReport {
    pub session_id: String,
    pub control_vks: Vec<u16>,
    pub segments: Vec<ReprojectedSegment>,
    pub freed_events_total: u64,
    pub rows_changed_total: u64,
}

/// Re-derive `frames.jsonl.zst` from `events.jsonl.zst` under an explicit
/// control-key policy, rewriting the RAW working copy in place.
///
/// The persisted `control` flag is the capture-time verdict of the game
/// profile's `control_vks`. When that profile was wrong — the COD corpus was
/// recorded with `control_vks: [17, 18, 119, 121]`, so Ctrl (a real
/// `move.prone` binding) was stamped as a safety-hotkey modifier and dropped by
/// the projector — every downstream plane inherits the loss. Passing the
/// corrected set here re-arbitrates the flag and replays the projection.
///
/// Caller must operate on a working copy: this rewrites frames, `meta.json`
/// and `sha256.json`, so the result no longer matches the canonical RAW bytes.
/// Record the substitution in the dataset's provenance — the session still
/// declares the profile it was captured under.
pub fn reproject_session(
    session_dir: &Path,
    control_vks: &[u16],
) -> anyhow::Result<ReprojectionReport> {
    // Header only: the full `validate_ready_session` cross-checks the stored
    // frames against a replay of the events, which is exactly what this
    // function is about to change.
    let header = validate_session_header(session_dir, &ComplianceProfile::LocalDebugNonTrainable)?;
    set_control_policy(control_vks);
    let control: BTreeSet<u16> = CONTROL_POLICY
        .get()
        .cloned()
        .unwrap_or_else(|| control_vks.iter().copied().collect());
    let mut report = ReprojectionReport {
        session_id: header.session.session_id.clone(),
        control_vks: control.iter().copied().collect(),
        segments: Vec::new(),
        freed_events_total: 0,
        rows_changed_total: 0,
    };

    for run_dir in sorted_directories(&session_dir.join("runs"), "r")? {
        let run: RawRun = read_json(&run_dir.join("run.json"))?;
        // One projector per run, matching how `convert_session` replays: it
        // carries pressed-key state and previous timing across segments.
        let mut projector = FrameProjector::default();
        for segment_dir in sorted_numeric_directories(&run_dir.join("segments"))? {
            let frames_path = segment_dir.join("frames.jsonl.zst");
            let events_path = segment_dir.join("events.jsonl.zst");
            let stored: Vec<RawFrameProjection> = read_zstd_jsonl(&frames_path)?;
            let mut events: Vec<RawInputEvent> = read_zstd_jsonl(&events_path)?;

            let freed = apply_control_policy(&mut events);

            let timing = stored.iter().map(frame_timing).collect::<Vec<_>>();
            let rows = projector.project_segment(
                &timing,
                &events,
                &run.run_id,
                &header.profile,
                header.mapping.mapping(),
            )?;
            ensure!(
                rows.len() == stored.len(),
                "reprojection changed the frame count of {}",
                segment_dir.display()
            );
            // Only count rows whose input semantics moved. Re-projecting
            // pre-#59 RAW also materialises `buttons`, which would otherwise
            // report every row as changed and drown the signal.
            let changed = rows
                .iter()
                .zip(&stored)
                .filter(|(new, old)| new.keys != old.keys || new.actions != old.actions)
                .count() as u64;

            let before = sha256_file(&frames_path)?;
            write_zstd_jsonl(&frames_path, &rows)?;
            let after = sha256_file(&frames_path)?;
            rewrite_segment_hashes(&segment_dir, &after)?;

            report.freed_events_total += freed;
            report.rows_changed_total += changed;
            report.segments.push(ReprojectedSegment {
                run_id: run.run_id.clone(),
                segment: numeric_name(&segment_dir)?,
                frames_sha256_before: before,
                frames_sha256_after: after,
                freed_events: freed,
                rows_changed: changed,
            });
        }
    }
    Ok(report)
}

/// Point `meta.json` at the new frames digest, then `sha256.json` at both.
/// Both files are edited as raw JSON so unrelated fields survive untouched.
fn rewrite_segment_hashes(segment_dir: &Path, frames_sha256: &str) -> anyhow::Result<()> {
    let meta_path = segment_dir.join("meta.json");
    let mut meta: serde_json::Value = read_json(&meta_path)?;
    meta.get_mut("frames")
        .and_then(|frames| frames.get_mut("sha256"))
        .map(|slot| *slot = serde_json::Value::from(frames_sha256))
        .context("meta.json has no frames.sha256")?;
    write_json(&meta_path, &meta)?;

    let manifest_path = segment_dir.join("sha256.json");
    let mut manifest: serde_json::Value = read_json(&manifest_path)?;
    let entries = manifest
        .as_object_mut()
        .context("sha256.json is not an object")?;
    entries.insert(
        "frames.jsonl.zst".into(),
        serde_json::Value::from(frames_sha256),
    );
    entries.insert(
        "meta.json".into(),
        serde_json::Value::from(sha256_file(&meta_path)?),
    );
    write_json(&manifest_path, &manifest)
}

fn write_zstd_jsonl(path: &Path, rows: &[impl Serialize]) -> anyhow::Result<()> {
    let mut encoder = zstd::Encoder::new(BufWriter::new(File::create(path)?), 3)?;
    for row in rows {
        serde_json::to_writer(&mut encoder, row)?;
        encoder.write_all(b"\n")?;
    }
    encoder.finish()?.flush()?;
    Ok(())
}

pub fn convert_session(session_dir: &Path, output: &Path) -> anyhow::Result<ConversionReport> {
    convert_session_with_profile(
        session_dir,
        output,
        &ComplianceProfile::LocalDebugNonTrainable,
    )
}

pub fn convert_session_with_profile(
    session_dir: &Path,
    output: &Path,
    compliance: &ComplianceProfile,
) -> anyhow::Result<ConversionReport> {
    compliance.validate()?;
    ensure!(
        !output.exists(),
        "refusing to overwrite output {}",
        output.display()
    );
    ensure_output_outside_source(session_dir, output)?;
    let validated = validate_ready_session(session_dir, compliance)?;
    validate_chunk_boundaries(&validated)?;

    let output_parent = output
        .parent()
        .context("output dataset path has no parent")?;
    fs::create_dir_all(output_parent)
        .with_context(|| format!("failed to create {}", output_parent.display()))?;
    ensure_output_outside_source(session_dir, output)?;
    let output_name = output
        .file_name()
        .and_then(|value| value.to_str())
        .context("output dataset path has no UTF-8 file name")?;
    let staging = output.with_file_name(format!(".{output_name}.staging-{}", std::process::id()));
    ensure!(
        !staging.exists(),
        "refusing to overwrite staging output {}",
        staging.display()
    );
    fs::create_dir(&staging)
        .with_context(|| format!("failed to create staging output {}", staging.display()))?;
    let mut guard = StagingGuard::new(staging.clone());

    let report = materialize_dataset(&validated, &staging)?;
    verify_dataset_with_profile(&staging, compliance)
        .context("generated data_demo dataset failed verification")?;
    fs::rename(&staging, output).with_context(|| {
        format!(
            "failed to publish dataset {} -> {}",
            staging.display(),
            output.display()
        )
    })?;
    guard.disarm();
    Ok(ConversionReport {
        output: output.to_path_buf(),
        ..report
    })
}

pub fn verify_dataset(dataset: &Path) -> anyhow::Result<VerificationReport> {
    verify_dataset_with_profile(dataset, &ComplianceProfile::LocalDebugNonTrainable)
}

fn validate_data_model_schema_member(
    value: &serde_json::Value,
    member: &str,
) -> anyhow::Result<()> {
    let object = value
        .as_object()
        .with_context(|| format!("{member} must be a JSON object"))?;
    let schema_version = object
        .get("schema_version")
        .and_then(serde_json::Value::as_str)
        .with_context(|| format!("{member} has no string schema_version"))?;
    match schema_version {
        LEGACY_DATA_MODEL_SCHEMA_VERSION => ensure!(
            !object.contains_key("catalog"),
            "{LEGACY_DATA_MODEL_SCHEMA_VERSION} {member} must not contain top-level catalog"
        ),
        DATA_MODEL_SCHEMA_VERSION => ensure!(
            object.contains_key("catalog"),
            "{DATA_MODEL_SCHEMA_VERSION} {member} must contain top-level catalog"
        ),
        _ => anyhow::bail!("unsupported dataset schema {schema_version}"),
    }
    Ok(())
}

pub fn verify_dataset_with_profile(
    dataset: &Path,
    compliance: &ComplianceProfile,
) -> anyhow::Result<VerificationReport> {
    compliance.validate()?;
    ensure!(dataset.is_dir(), "dataset directory does not exist");
    let metadata_lines: Vec<serde_json::Value> = read_jsonl(&dataset.join("metadata.jsonl"))?;
    ensure!(
        metadata_lines.len() == 1,
        "converter dataset must contain exactly one metadata.jsonl row"
    );
    let metadata_value = metadata_lines.into_iter().next().unwrap();
    validate_data_model_schema_member(&metadata_value, "dataset metadata")?;
    let metadata: SessionMetadata = serde_json::from_value(metadata_value.clone())
        .context("failed to parse dataset metadata")?;
    ensure_compliance_metadata(&metadata, compliance)?;
    let native_format = NativeFormat::from_capture(&metadata.capture)?;
    let game_slug = slug(&metadata.provenance.mapping_id.replace("game.", ""));
    let preferred = dataset.join(&game_slug).join(&metadata.workflow_id);
    let session_dir = if preferred.is_dir() {
        preferred
    } else {
        find_dataset_session(dataset, &metadata.workflow_id)?
    };
    validate_dataset_tree(dataset, &session_dir)?;
    let session_metadata_value: serde_json::Value = read_json(&session_dir.join("metadata.json"))?;
    validate_data_model_schema_member(&session_metadata_value, "session metadata")?;
    ensure!(
        session_metadata_value == metadata_value,
        "metadata.json does not match metadata.jsonl"
    );
    // 角度重算的常数源必须自身合法，否则下面的逐帧对账会算出 NaN/Inf，
    // 让"重算一致"退化成永假谜题。
    if let Some(reference) = &metadata.view_scale {
        ensure!(
            reference.norm == view_scale::VIEW_SCALE_NORM
                && reference.counts_per_360.is_finite()
                && reference.counts_per_360 > 0.0
                && reference.vertical_scale_ratio.is_finite()
                && reference.vertical_scale_ratio > 0.0,
            "metadata view_scale reference is not a valid {} declaration",
            view_scale::VIEW_SCALE_NORM
        );
    }

    let mut all_rows = Vec::new();
    let chunk_dirs = dataset_chunk_directories(&session_dir)?;
    ensure!(
        chunk_dirs.len() as u32 == metadata.capture.chunks,
        "dataset chunk directory count mismatch"
    );
    let mut video_template = None;
    let mut previous_qpc_end: Option<i64> = None;
    let mut previous_estimate: Option<i64> = None;
    let mut wall_clock_probe: Option<(u64, i64)> = None;
    let mut verified_audio = Vec::new();
    for (chunk_index, chunk_dir) in chunk_dirs.iter().enumerate() {
        let chunk_count = u32::try_from(chunk_index)?;
        let chunk_meta_value: serde_json::Value = read_json(&chunk_dir.join("meta.json"))?;
        validate_data_model_schema_member(&chunk_meta_value, "chunk metadata")?;
        let chunk_meta: ChunkMeta = serde_json::from_value(chunk_meta_value)
            .with_context(|| format!("failed to parse chunk {chunk_count} metadata"))?;
        validate_chunk_meta(
            &chunk_meta,
            &metadata,
            chunk_count,
            native_format,
            compliance,
        )?;
        validate_chunk_audio_files(chunk_dir, &chunk_meta, metadata.audio.is_some())?;
        verified_audio.push(chunk_meta.audio.clone());
        let rows: Vec<DataDemoFrame> = read_jsonl(&chunk_dir.join("p0.jsonl"))?;
        ensure!(!rows.is_empty(), "chunk {chunk_count} is empty");
        ensure!(
            rows.len() <= native_format.chunk_frames,
            "chunk {chunk_count} exceeds {} frames",
            native_format.chunk_frames
        );
        ensure!(
            chunk_meta.video.frames == rows.len() as u64,
            "chunk {chunk_count} meta frame count mismatch"
        );
        let video = inspect_video(
            &chunk_dir.join("p0.mp4"),
            Some(rows.len() as u64),
            Some(native_format),
        )?;
        if let Some(template) = &video_template {
            ensure!(
                same_codec_config(template, &video),
                "output chunk codec configuration changed"
            );
        } else {
            video_template = Some(video.clone());
        }
        ensure!(
            video.sync_samples.first() == Some(&0),
            "chunk {chunk_count} does not start with a sync sample"
        );
        for (local, row) in rows.iter().enumerate() {
            let expected_gframe = (all_rows.len() + local) as u64;
            ensure!(row.frame == local as u64, "chunk frame index is not local");
            ensure!(row.gframe == expected_gframe, "gframe is not continuous");
            ensure!(
                row.video_t_ms == native_format.video_time_ms(expected_gframe),
                "video_t_ms does not match gframe"
            );
            // Timebase columns: only checkable for output that carries them
            // (0 means "written before this field existed"), and only for
            // internal consistency — the tick values themselves come from RAW
            // and are already verified against it upstream.
            if row.qpc != 0 || row.qpc_end != 0 {
                ensure!(
                    row.qpc_end > row.qpc,
                    "frame aggregation window is empty or inverted"
                );
                if let Some(previous) = previous_qpc_end {
                    // Windows tile the stream, but a pause/resume boundary is a
                    // real jump forward — so require no overlap, not adjacency.
                    ensure!(
                        row.qpc >= previous,
                        "frame aggregation windows overlap or move backwards"
                    );
                }
                previous_qpc_end = Some(row.qpc_end);
            }
            if let Some(estimate) = row.t_utc_est_us {
                // `video_t_ms` counts *recorded* frames; the estimate rides the
                // capture tick, which keeps running while a run is paused. So
                // the two agree only within a run, and across a pause/resume
                // the estimate legitimately jumps ahead by the wall-clock length
                // of the gap. That makes the upper side uncheckable here — the
                // dataset does not carry run boundaries — but the lower side is
                // still a real invariant: real time never elapses more slowly
                // than the frames it produced. The allowance covers the sub-tick
                // rounding in each frame window accumulated over a session.
                let anchor = *wall_clock_probe.get_or_insert((row.video_t_ms, estimate));
                let expected = i128::from(anchor.1) + i128::from(row.video_t_ms - anchor.0) * 1_000;
                ensure!(
                    i128::from(estimate) + 10_000 >= expected,
                    "t_utc_est_us runs behind the frame timeline"
                );
                if let Some(previous) = previous_estimate {
                    ensure!(
                        estimate >= previous,
                        "t_utc_est_us moves backwards between frames"
                    );
                }
                previous_estimate = Some(estimate);
            }
            ensure!(
                is_sorted_unique(&row.keys),
                "frame keys are not sorted/unique"
            );
            // #59 item 1: same invariant as `keys`. Held state, so it cannot be
            // recomputed from this frame's events alone — only its shape is
            // checkable here.
            ensure!(
                is_sorted_unique(&row.buttons),
                "frame buttons are not sorted/unique"
            );
            ensure!(
                is_sorted_unique(&row.actions),
                "frame actions are not sorted/unique"
            );
            ensure!(
                row.actions
                    .iter()
                    .all(|action| valid_action_semantic(action)),
                "frame contains an invalid action semantic"
            );
            ensure!(row.src == "rec", "frame source is not recorded Raw Input");
            ensure!(
                row.pointer_bins == [foveated_bin(row.pointer_dx), foveated_bin(row.pointer_dy)],
                "pointerBins do not match the shared dm-1 projection"
            );
            // #59 item 2: the untruncated pointer sum must equal this frame's
            // `mouse_move` deltas recomputed from `events`. Verified only when the
            // field is present (older pre-#59 outputs carry 0 and no events sum),
            // so an archived non-#59 DataModel still verifies.
            let (raw_dx, raw_dy) = pointer_raw_sum(&row.events);
            let has_raw = row.pointer_raw_dx != 0 || row.pointer_raw_dy != 0;
            ensure!(
                !has_raw || (row.pointer_raw_dx == raw_dx && row.pointer_raw_dy == raw_dy),
                "pointerRawDx/Dy do not match the sum of the frame's mouse_move events"
            );
            // docs/46 §6 对账项：角度列与会话级标定声明互证。重算走 events
            // （与 pointerRaw 先例同源，不用行内字段自证），同一纯函数、同一
            // 常数，IEEE-754 下确定性可复现，故逐位相等；常数合法性已在上方
            // 把关，NaN 不可达。未标定数据集（含 C7 之前的存量）必须整行留白。
            match &metadata.view_scale {
                Some(reference) => {
                    let yaw =
                        view_scale::yaw_delta_rad(i64::from(raw_dx), reference.counts_per_360);
                    let pitch = view_scale::pitch_delta_rad(
                        i64::from(raw_dy),
                        reference.counts_per_360,
                        reference.vertical_scale_ratio,
                    );
                    ensure!(
                        row.view_scale_valid
                            && row.look_yaw_delta_rad == Some(yaw)
                            && row.look_pitch_delta_rad == Some(pitch),
                        "look_yaw/pitch_delta_rad do not match recomputation from events under the declared view_scale"
                    );
                }
                None => {
                    ensure!(
                        !row.view_scale_valid
                            && row.look_yaw_delta_rad.is_none()
                            && row.look_pitch_delta_rad.is_none(),
                        "uncalibrated dataset carries view-scale angle claims"
                    );
                }
            }
        }
        all_rows.extend(rows);
    }
    let chunk_count = u32::try_from(chunk_dirs.len())?;
    ensure!(chunk_count > 0, "dataset has no chunks");
    ensure!(
        u64::try_from(all_rows.len())? == metadata.capture.frame_count,
        "dataset frame count mismatch"
    );
    ensure!(
        chunk_count == metadata.capture.chunks,
        "dataset chunk count mismatch"
    );
    ensure!(
        summarize_audio(verified_audio.iter().map(Option::as_ref)) == metadata.audio,
        "dataset audio summary does not close over chunk audio states"
    );
    let labeled_frames = all_rows.iter().filter(|row| is_labeled_frame(row)).count() as u64;
    ensure!(
        labeled_frames == metadata.annotation.labeled_frames,
        "dataset labeled frame count mismatch"
    );
    for index in 0..chunk_count.saturating_sub(1) {
        let rows: Vec<DataDemoFrame> =
            read_jsonl(&session_dir.join(format!("c{index:04}/p0.jsonl")))?;
        ensure!(
            rows.len() == native_format.chunk_frames,
            "non-final chunk {index} is not one native four-second GOP"
        );
    }

    let pairs: Vec<FramePair> = read_jsonl(&session_dir.join("framepairs.jsonl"))?;
    let run_frame_ranges = validated_run_frame_ranges(
        &metadata.provenance.run_frame_ranges,
        metadata.capture.frame_count,
    )?;
    let expected_pairs = build_framepairs(
        &metadata.workflow_id,
        metadata.consent_id.as_deref(),
        &all_rows,
        native_format.chunk_frames,
        &run_frame_ranges,
    );
    ensure!(
        pairs == expected_pairs,
        "framepairs.jsonl does not close over frames"
    );
    ensure!(
        pairs.len() as u64 == metadata.frame_pairs.count,
        "frame pair count mismatch"
    );
    let stats: DatasetStats = read_json(&dataset.join("_dataset_stats.json"))?;
    validate_stats(&stats, &metadata, compliance)?;
    ensure!(
        dataset.join("DATASHEET.md").is_file(),
        "DATASHEET.md is missing"
    );

    Ok(VerificationReport {
        session_id: metadata.workflow_id,
        frame_count: all_rows.len() as u64,
        chunk_count,
    })
}

/// Verifies both the standalone data_demo contract and its exact provenance
/// binding to one immutable, completed RAW session.
pub fn verify_dataset_against_raw_session(
    session_dir: &Path,
    dataset: &Path,
) -> anyhow::Result<VerificationReport> {
    ensure_output_outside_source(session_dir, dataset)?;
    let validated =
        validate_ready_session(session_dir, &ComplianceProfile::LocalDebugNonTrainable)?;
    validate_chunk_boundaries(&validated)?;
    incremental::verify_dataset_binding(dataset, &validated)
}

fn validate_ready_session(
    session_dir: &Path,
    compliance: &ComplianceProfile,
) -> anyhow::Result<ValidatedSession> {
    let header = validate_session_header(session_dir, compliance)?;
    let ValidatedSessionHeader {
        session,
        mapping,
        profile,
        native_format,
        view_scale,
    } = header;

    let runs_dir = session_dir.join("runs");
    ensure!(runs_dir.is_dir(), "ready session runs directory is missing");
    let run_dirs = sorted_directories(&runs_dir, "r")?;
    ensure!(!run_dirs.is_empty(), "ready session has no runs");

    let mut segments = Vec::new();
    let mut previous_session_frame = None;
    let mut previous_run_qpc_end = None;
    let mut expected_run_frame_start = 0_u64;
    let mut qpc_frequency = None;
    let mut recorded_anchor: Option<(String, i64)> = None;
    let mut wall_clock_run_selected = false;
    for (run_index, run_dir) in run_dirs.into_iter().enumerate() {
        let expected_run_id = format!("r{run_index:04}");
        ensure!(
            run_dir.file_name().and_then(|value| value.to_str()) == Some(expected_run_id.as_str()),
            "run directory and run.json identity mismatch"
        );
        // 客户端中止的 run 只封 failure.json、永远等不到 run.json——那不是 ready
        // 内容，跳过整个目录（它的 segments 若存在也一并不进数据集：run 级
        // 帧/事件计数从未定稿）。目录密实编号仍按全部 run 目录核（客户端连失败
        // run 也按序编号）。安全性由帧连续性检查兜底：failed run 真录了帧且其后
        // 还有完结 run 时，session_frame 链在下一个 run 处断裂、整个会话照旧被
        // 拒。两个文件都缺的目录仍然是错误——那不是客户端会留下的形状。
        let run_path = run_dir.join("run.json");
        if !run_path.is_file() {
            ensure!(
                run_dir.join("failure.json").is_file(),
                "run {expected_run_id} has neither run.json nor failure.json"
            );
            continue;
        }
        let run: RawRun = read_json(&run_path)?;
        if !wall_clock_run_selected && run.anchor_qpc.is_some() && run.qpc_frequency.is_some() {
            recorded_anchor = run.anchor_at_utc.clone().zip(run.anchor_qpc);
            wall_clock_run_selected = true;
        }
        // Same machine, same boot, so every run in a session reports the same
        // rate; take the first and require the rest to agree.
        match (qpc_frequency, run.qpc_frequency) {
            (None, rate) => qpc_frequency = rate,
            (Some(known), Some(rate)) => ensure!(
                known == rate,
                "runs disagree on the capture tick rate: {known} vs {rate}"
            ),
            (Some(_), None) => {}
        }
        validate_run(&run, &session, &mapping, compliance)?;
        ensure!(
            run.run_id == expected_run_id,
            "run directory and run.json identity mismatch"
        );
        ensure!(
            run.session_frame_start == expected_run_frame_start,
            "run {} session frame start is not continuous: expected {}, got {}",
            run.run_id,
            expected_run_frame_start,
            run.session_frame_start
        );
        let segment_dirs = sorted_numeric_directories(&run_dir.join("segments"))?;
        ensure!(
            segment_dirs.len() as u32 == run.segment_count,
            "run {} segment count mismatch",
            run.run_id
        );
        let mut projector = FrameProjector::default();
        let mut run_frames = 0_u64;
        let mut run_events = 0_u64;
        let mut run_first_session_frame = None;
        let mut run_last_session_frame = None;
        let mut run_first_qpc = None;
        let mut run_last_qpc = None;
        for segment_dir in segment_dirs {
            let validated = validate_segment(
                &segment_dir,
                &session,
                &profile,
                &mapping,
                &run.run_id,
                run.qpc_frequency,
                native_format,
                &mut projector,
                compliance,
            )?;
            if let Some(previous) = previous_session_frame {
                ensure!(
                    validated.rows.first().unwrap().session_frame == previous + 1,
                    "session_frame is not continuous across ready segments"
                );
            }
            previous_session_frame = validated.rows.last().map(|row| row.session_frame);
            run_first_session_frame.get_or_insert(validated.rows.first().unwrap().session_frame);
            run_last_session_frame = validated.rows.last().map(|row| row.session_frame);
            run_first_qpc.get_or_insert(validated.rows.first().unwrap().output_qpc_start);
            run_last_qpc = validated.rows.last().map(|row| row.output_qpc_end);
            run_frames += validated.rows.len() as u64;
            let meta: RawSegmentMeta = read_json(&segment_dir.join("meta.json"))?;
            run_events += meta.events.lines;
            segments.push(validated);
        }
        ensure!(run_frames == run.frame_count, "run frame count mismatch");
        if run_frames == 0 {
            ensure!(
                run_first_session_frame.is_none()
                    && run_last_session_frame.is_none()
                    && run.session_frame_start == run.session_frame_end,
                "empty run frame interval does not close at its start"
            );
        } else {
            ensure!(
                run_first_session_frame == Some(run.session_frame_start)
                    && run_last_session_frame.and_then(|value| value.checked_add(1))
                        == Some(run.session_frame_end),
                "run frame interval does not match frame payload"
            );
        }
        if let (Some(previous_end), Some(current_start)) = (previous_run_qpc_end, run_first_qpc) {
            ensure!(
                current_start >= previous_end,
                "run QPC timeline moved backwards"
            );
        }
        if run_last_qpc.is_some() {
            previous_run_qpc_end = run_last_qpc;
        }
        ensure!(
            run_events == run.input_event_count,
            "run event count mismatch"
        );
        ensure!(run.input_queue_overflows == 0, "run records input overflow");
        let expected_run_frame_end = run
            .session_frame_start
            .checked_add(run.frame_count)
            .context("run session frame interval overflow")?;
        ensure!(
            run.session_frame_end == expected_run_frame_end,
            "run session frame interval mismatch"
        );
        expected_run_frame_start = expected_run_frame_end;
    }
    ensure!(!segments.is_empty(), "ready session has no ready segments");

    // Anchor the session's wall clock at its very first frame, which is also the
    // tick `run.json` reports as the first run's anchor.
    let wall_clock = segments
        .first()
        .and_then(|segment| segment.rows.first())
        .map(|first| {
            WallClockAnchor::new(
                &session.session_id,
                first.output_qpc_start,
                recorded_anchor
                    .as_ref()
                    .map(|(utc, qpc)| (utc.as_str(), *qpc)),
                qpc_frequency,
            )
        })
        .transpose()?
        .flatten();

    Ok(ValidatedSession {
        session,
        mapping,
        native_format,
        audio_enabled: profile.audio.is_some(),
        segments,
        compliance: compliance.clone(),
        wall_clock,
        view_scale,
    })
}

fn validate_session_header(
    session_dir: &Path,
    compliance: &ComplianceProfile,
) -> anyhow::Result<ValidatedSessionHeader> {
    ensure!(
        session_dir.is_dir(),
        "ready session directory does not exist"
    );
    let session: RawSession = read_json(&session_dir.join("session.json"))?;
    ensure!(
        supported_raw_session_schema(&session.schema_version),
        "unsupported RAW session schema {}",
        session.schema_version
    );
    let expected_claims = expected_raw_training_claims(&session.schema_version, compliance)?;
    ensure!(
        (session.non_trainable, session.production_certified) == expected_claims,
        "RAW session training claims do not match its session generation \
         (non-trainable/consented classification mismatch)"
    );
    if compliance.is_release() {
        ensure!(
            session.consent_id.as_deref() == compliance.consent_id(),
            "RAW session consent id does not match the release consent id"
        );
    } else {
        // local-debug 与 attested-trainable 都只接受如实的采集 RAW。legacy 用
        // `non_trainable=true` 表达；中性 v2 刻意不携带任何训练声明，上面的
        // generation 对账同时拦住「缺了旧字段」和「新格式又把旧字段塞回来」。
        if compliance.is_attested() {
            ensure!(
                session.consent_id.is_none(),
                "attested-trainable refuses a RAW session carrying a consent id; \
                 consent-stamped RAW belongs to the first-party-release profile"
            );
        }
    }
    ensure_safe_component(&session.session_id, "session id")?;
    ensure_contract(&session.contract)?;

    // bundle-1 世代（断代前的 capture+settings 两册目录，issue #811 路线 A）
    // 由 catalog 段的形状裁决——那一代与当代共用 session schema 字符串，schema
    // 本身分不出来。只有 local-debug 与 attested-trainable 走得进去：
    // first-party-release 维持 docs/57 的 v2+ 断代，中性 v2 晚于断代、不存在
    // bundle-1 形态。
    let legacy_bundle1 = if uses_catalog_snapshots(&session.schema_version) {
        let catalog = session
            .catalog
            .as_ref()
            .context("catalog-backed session requires catalog identity in session.json")?;
        let legacy = matches!(
            catalog.shape()?,
            RawCatalogShape::LegacyCaptureSettings { .. }
        );
        if legacy {
            ensure!(
                !compliance.is_release(),
                "first-party-release refuses the pre-docs/57 bundle-1 catalog generation; \
                 only local-debug and attested-trainable read archived bundle-1 RAW"
            );
            ensure!(
                !is_neutral_ingest_session(&session.schema_version),
                "neutral ingest sessions postdate the docs/57 catalog merge and cannot \
                 carry a bundle-1 capture+settings catalog"
            );
        }
        legacy
    } else {
        false
    };

    // bundle-1 与更早的 v1 会话写的都是 capture-profile.json；断代后客户端只写
    // 一份 recording-profile.json（docs/57 §3.1）。引擎参数在两种形状里是同一批
    // 字段，投影出同一个 `CaptureProfile`，下游一行不用分叉。
    let profile = if uses_catalog_snapshots(&session.schema_version) && !legacy_bundle1 {
        read_json::<RawRecordingProfile>(&session_dir.join("recording-profile.json"))?
            .capture_profile(!compliance.is_release())?
    } else {
        CaptureProfile::load(session_dir.join("capture-profile.json"))?
    };
    if let Some(audio) = &profile.audio {
        audio.validate()?;
    }
    ensure!(
        expected_audio_metadata(&session.schema_version, profile.audio.is_some())?
            == profile.audio.is_some(),
        "RAW session generation and recording profile disagree about audio"
    );
    let native_format = NativeFormat::from_profile(&profile, compliance)?;
    validate_capture_policy(&session.capture_policy, &profile)?;

    let mapping_bytes =
        fs::read(session_dir.join("mapping.json")).context("failed to read mapping.json")?;
    // bundle-1 会话钉的是 dm-mapping-1（settings_profile_id 键）；legacy 读取器
    // 在内存里规范化、哈希仍取原字节，所以下面全部身份对账原样成立。世代不许
    // 杂交：bundle-1 配 dm-mapping-2 或反之都在各自读取器里被拒。
    let mapping = if legacy_bundle1 {
        MappingSnapshot::from_legacy_v1_slice(&mapping_bytes)?
    } else {
        MappingSnapshot::from_slice(&mapping_bytes)?
    };
    validate_mapping_identity(&session.mapping, &mapping, "session.json")?;
    // schema-1 的 v1 会话没有目录快照，天然属"未标定"分支。
    let view_scale = if uses_catalog_snapshots(&session.schema_version) {
        validate_v2_catalog_snapshots(session_dir, &session, &profile, &mapping, compliance)?
    } else {
        None
    };
    ensure!(
        session.game.id == mapping.mapping().game.id,
        "session game id does not match mapping"
    );
    // mapping 的 profile 引用键按世代互斥（bundle-1 写 settings_profile_id，其余
    // 写 recording_profile_id）；legacy 规范化后两个世代都收敛到
    // `mapping().recording_profile_id` 这一个口径上对账。
    let (generation_profile_id, foreign_generation_key) = if legacy_bundle1 {
        (
            session.mapping.settings_profile_id.as_deref(),
            session.mapping.recording_profile_id.as_deref(),
        )
    } else {
        (
            session.mapping.recording_profile_id.as_deref(),
            session.mapping.settings_profile_id.as_deref(),
        )
    };
    ensure!(
        foreign_generation_key.is_none(),
        "session mapping identity carries the profile-reference key of the wrong \
         catalog generation"
    );
    ensure!(
        generation_profile_id == Some(mapping.mapping().recording_profile_id.as_str()),
        "session recording profile does not match mapping"
    );

    Ok(ValidatedSessionHeader {
        session,
        mapping,
        profile,
        native_format,
        view_scale,
    })
}

#[allow(clippy::too_many_arguments)]
fn validate_segment(
    segment_dir: &Path,
    session: &RawSession,
    profile: &CaptureProfile,
    mapping: &MappingSnapshot,
    run_id: &str,
    run_qpc_frequency: Option<i64>,
    native_format: NativeFormat,
    projector: &mut FrameProjector,
    compliance: &ComplianceProfile,
) -> anyhow::Result<ValidatedSegment> {
    #[cfg(test)]
    SEGMENT_VALIDATION_COUNT.set(SEGMENT_VALIDATION_COUNT.get() + 1);
    for name in [
        "video.mp4",
        "frames.jsonl.zst",
        "events.jsonl.zst",
        "meta.json",
        "sha256.json",
    ] {
        ensure!(
            segment_dir.join(name).is_file(),
            "ready segment is missing {name}: {}",
            segment_dir.display()
        );
    }
    let meta: RawSegmentMeta = read_json(&segment_dir.join("meta.json"))?;
    let expected_index = numeric_name(segment_dir)?;
    let expected_schema = expected_recording_schema(&session.schema_version)?;
    ensure!(
        meta.schema_version == expected_schema,
        "unsupported segment schema"
    );
    ensure!(
        meta.session_id == session.session_id,
        "segment session id mismatch"
    );
    ensure!(meta.run_id == run_id, "segment run id mismatch");
    ensure!(
        meta.segment_index == expected_index,
        "segment index mismatch"
    );
    let expected_claims = expected_raw_training_claims(&session.schema_version, compliance)?;
    ensure!(
        (meta.non_trainable, meta.production_certified) == expected_claims,
        "segment training claims do not match its session generation"
    );
    if compliance.is_release() {
        ensure!(
            meta.state == RAW_CONSENTED_READY_STATE,
            "segment is not consented-ready"
        );
        ensure!(
            meta.consent_id.as_deref() == compliance.consent_id(),
            "segment consent id does not match the release consent id"
        );
    } else if is_neutral_ingest_session(&session.schema_version) {
        ensure!(meta.state == RAW_PLAIN_READY_STATE, "segment is not ready");
        if compliance.is_attested() {
            ensure!(
                meta.consent_id.is_none(),
                "attested-trainable refuses a segment carrying a consent id; \
                 consent-stamped RAW belongs to the first-party-release profile"
            );
        }
    } else if compliance.is_attested() {
        // 具结车道的如实 RAW：早期 manual 镜像段写裸 `ready`，之后的段写
        // `non_trainable_ready`，两者都接受；non_trainable/consent 保持如实，
        // 不改写、不放松其余任何完整性校验。
        ensure!(
            meta.state == RAW_READY_STATE || meta.state == RAW_PLAIN_READY_STATE,
            "segment is not ready (attested-trainable accepts state `{RAW_READY_STATE}` or `{RAW_PLAIN_READY_STATE}`)"
        );
        ensure!(
            meta.consent_id.is_none(),
            "attested-trainable refuses a segment carrying a consent id; \
             consent-stamped RAW belongs to the first-party-release profile"
        );
    } else {
        ensure!(meta.state == RAW_READY_STATE, "segment is not ready");
    }
    ensure!(
        meta.input_status == "raw_input_v1",
        "unsupported input status"
    );
    ensure!(
        meta.qpc_end > meta.qpc_start,
        "segment QPC interval is empty"
    );
    ensure!(
        meta.fps_num == profile.fps_num && meta.fps_den == profile.fps_den,
        "segment fps mismatch"
    );
    ensure!(
        meta.width == profile.width && meta.height == profile.height,
        "segment dimensions mismatch"
    );
    validate_mapping_identity(&meta.mapping, mapping, "segment meta")?;
    if uses_catalog_snapshots(&session.schema_version) {
        ensure!(
            meta.catalog.as_ref() == session.catalog.as_ref(),
            "segment meta catalog identity does not match session.json"
        );
    }
    ensure_contract(&meta.contract)?;
    ensure!(
        meta.health.queue_drops == 0,
        "segment records a source queue drop"
    );
    ensure!(
        meta.health.input_overflow == 0,
        "segment records input overflow"
    );
    validate_effective_encoder(&meta.effective_encoder, profile)?;
    validate_probe(&meta.probe, &meta)?;
    ensure!(
        meta.video.path == "video.mp4",
        "unsafe video path in segment meta"
    );
    ensure!(
        meta.frames.path == "frames.jsonl.zst",
        "unsafe frames path in segment meta"
    );
    ensure!(
        meta.events.path == "events.jsonl.zst",
        "unsafe events path in segment meta"
    );

    ensure!(
        expected_audio_metadata(&session.schema_version, profile.audio.is_some())?
            == meta.audio.is_some(),
        "segment audio metadata does not match the session generation"
    );
    if let Some(audio) = &meta.audio {
        audio.validate()?;
        if let Some(run_frequency) = run_qpc_frequency {
            ensure!(
                audio.qpc_frequency == run_frequency,
                "segment audio QPC frequency differs from run.json"
            );
        }
        if audio.has_artifact() {
            ensure!(
                audio.first_sample_qpc == Some(meta.qpc_start),
                "segment audio does not start at the video QPC boundary"
            );
            ensure!(
                audio.frames
                    == Some(audio_frames_for_qpc_span(
                        meta.qpc_start,
                        meta.qpc_end,
                        audio.qpc_frequency,
                    )?),
                "segment audio frame count does not close over the video QPC interval"
            );
        }
    }

    let manifest: BTreeMap<String, String> = read_json(&segment_dir.join("sha256.json"))?;
    let expected_names = expected_manifest_payloads(meta.audio.as_ref())
        .into_iter()
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    ensure!(
        manifest.keys().cloned().collect::<BTreeSet<_>>() == expected_names,
        "sha256.json payload set does not match segment audio state"
    );
    for (name, expected) in &manifest {
        let actual = sha256_file(&segment_dir.join(name))?;
        ensure!(actual == *expected, "SHA-256 mismatch for {name}");
    }
    ensure!(
        manifest["video.mp4"] == meta.video.sha256,
        "video hash differs from meta"
    );
    ensure!(
        manifest["frames.jsonl.zst"] == meta.frames.sha256,
        "frames hash differs from meta"
    );
    ensure!(
        manifest["events.jsonl.zst"] == meta.events.sha256,
        "events hash differs from meta"
    );
    ensure!(
        fs::metadata(segment_dir.join("video.mp4"))?.len() == meta.video.bytes,
        "video byte count mismatch"
    );
    if let Some(audio) = &meta.audio
        && audio.has_artifact()
    {
        ensure!(
            manifest[AUDIO_ARTIFACT_NAME]
                == audio
                    .sha256
                    .as_deref()
                    .context("present audio has no SHA-256")?,
            "audio hash differs from meta"
        );
        let pcm = read_segment_pcm(segment_dir, audio)?;
        ensure!(
            pcm.len() as u64
                == audio
                    .uncompressed_bytes()?
                    .context("present audio has no PCM byte count")?,
            "audio PCM byte count differs from meta"
        );
    }

    let rows: Vec<RawFrameProjection> = read_zstd_jsonl(&segment_dir.join("frames.jsonl.zst"))?;
    let mut events: Vec<RawInputEvent> = read_zstd_jsonl(&segment_dir.join("events.jsonl.zst"))?;
    // events.jsonl stays byte-identical to the canonical RAW; only the derived
    // frames plane is re-arbitrated, so the replay here must use the same policy.
    apply_control_policy(&mut events);
    ensure!(
        rows.len() as u64 == meta.n_frames,
        "frames JSONL line count mismatch"
    );
    ensure!(
        rows.len() as u64 == meta.frames.lines,
        "frames meta line count mismatch"
    );
    ensure!(
        events.len() as u64 == meta.events.lines,
        "events meta line count mismatch"
    );
    ensure!(!rows.is_empty(), "ready segment has no frame rows");
    ensure!(
        meta.session_frame_start == rows.first().map(|row| row.session_frame),
        "segment session_frame_start mismatch"
    );
    ensure!(
        rows.first().map(|row| row.output_qpc_start) == Some(meta.qpc_start)
            && rows.last().map(|row| row.output_qpc_end) == Some(meta.qpc_end),
        "segment QPC interval does not match frame payload"
    );
    for event in &events {
        ensure!(event.run_id == run_id, "event run id mismatch");
        ensure!(
            event.qpc >= meta.qpc_start && event.qpc < meta.qpc_end,
            "event QPC outside segment"
        );
    }
    for row in &rows {
        ensure!(row.run_id == run_id, "frame row run id mismatch");
    }
    let timing = rows.iter().map(frame_timing).collect::<Vec<_>>();
    let expected =
        projector.project_segment(&timing, &events, run_id, profile, mapping.mapping())?;
    let mut expected_value = serde_json::to_value(&expected)?;
    let stored_value = serde_json::to_value(&rows)?;
    // RAW written before capture-core emitted `buttons` has no such key, while
    // re-projecting its events today does produce one. Drop the field from the
    // expectation for exactly those rows, so an archived session still converts;
    // rows that do carry it are compared in full (#59 item 1).
    if let (Some(expected_rows), Some(stored_rows)) =
        (expected_value.as_array_mut(), stored_value.as_array())
    {
        for (expected_row, stored_row) in expected_rows.iter_mut().zip(stored_rows) {
            if stored_row.get("buttons").is_none()
                && let Some(object) = expected_row.as_object_mut()
            {
                object.remove("buttons");
            }
        }
    }
    ensure!(
        expected_value == stored_value,
        "frames.jsonl projection does not match events.jsonl and mapping.json"
    );

    let video = inspect_video(
        &segment_dir.join("video.mp4"),
        Some(meta.n_frames),
        Some(native_format),
    )?;
    Ok(ValidatedSegment {
        path: segment_dir.to_path_buf(),
        run_id: run_id.to_owned(),
        segment_index: meta.segment_index,
        rows,
        events,
        video,
        source_hashes: manifest,
        audio: meta.audio,
    })
}

fn audio_frames_for_qpc_span(start: i64, end: i64, frequency: i64) -> anyhow::Result<u64> {
    ensure!(end >= start, "audio QPC interval must not move backwards");
    ensure!(frequency > 0, "audio QPC frequency must be positive");
    let ticks = i128::from(
        end.checked_sub(start)
            .context("audio QPC interval overflow")?,
    );
    let frequency = i128::from(frequency);
    let frames = (ticks
        .checked_mul(i128::from(AUDIO_SAMPLE_RATE_HZ))
        .context("audio QPC conversion overflow")?
        + frequency / 2)
        / frequency;
    u64::try_from(frames).context("audio frame count overflow")
}

fn read_segment_pcm(segment_dir: &Path, audio: &SegmentAudioMetadata) -> anyhow::Result<Vec<u8>> {
    ensure!(audio.has_artifact(), "absent audio has no PCM payload");
    let path = segment_dir.join(AUDIO_ARTIFACT_NAME);
    ensure!(
        fs::metadata(&path)?.len()
            == audio
                .bytes
                .context("present audio has no compressed byte count")?,
        "audio compressed byte count differs from meta"
    );
    let file = File::open(&path).with_context(|| format!("failed to open {}", path.display()))?;
    zstd::decode_all(BufReader::new(file))
        .with_context(|| format!("invalid zstd audio stream {}", path.display()))
}

fn write_flac(path: &Path, pcm: &[u8], frames: u64) -> anyhow::Result<()> {
    ensure!(
        pcm.len() as u64 == frames.checked_mul(4).context("FLAC PCM size overflow")?,
        "FLAC PCM does not contain stereo 16-bit frames"
    );
    let samples = pcm
        .as_chunks::<2>()
        .0
        .iter()
        .map(|bytes| i32::from(i16::from_le_bytes([bytes[0], bytes[1]])))
        .collect::<Vec<_>>();
    let mut config = flacenc::config::Encoder::default();
    config.multithread = false;
    let config = config.into_verified().map_err(|(_, error)| {
        anyhow::anyhow!("invalid deterministic FLAC encoder configuration: {error:?}")
    })?;
    let source = flacenc::source::MemSource::from_samples(
        &samples,
        usize::from(AUDIO_CHANNELS),
        usize::from(AUDIO_BITS_PER_SAMPLE),
        usize::try_from(AUDIO_SAMPLE_RATE_HZ)?,
    );
    let stream = flacenc::encode_with_fixed_block_size(&config, source, config.block_size)
        .map_err(|error| anyhow::anyhow!("FLAC encoding failed: {error:?}"))?;
    let mut sink = flacenc::bitsink::ByteSink::new();
    stream
        .write(&mut sink)
        .context("FLAC serialization failed")?;
    fs::write(path, sink.as_slice())?;
    verify_flac(path, frames, Some(pcm))
}

fn verify_flac(path: &Path, frames: u64, expected_pcm: Option<&[u8]>) -> anyhow::Result<()> {
    let mut reader = claxon::FlacReader::open(path)
        .with_context(|| format!("invalid FLAC {}", path.display()))?;
    let info = reader.streaminfo();
    ensure!(
        info.sample_rate == AUDIO_SAMPLE_RATE_HZ
            && info.channels == u32::from(AUDIO_CHANNELS)
            && info.bits_per_sample == u32::from(AUDIO_BITS_PER_SAMPLE)
            && info.samples == Some(frames),
        "FLAC stream format or frame count mismatch"
    );
    let mut decoded_samples = 0_u64;
    for sample in reader.samples() {
        let sample = sample.context("FLAC sample decode failed")?;
        if let Some(expected) = expected_pcm {
            let offset = usize::try_from(
                decoded_samples
                    .checked_mul(2)
                    .context("decoded FLAC sample offset overflow")?,
            )?;
            ensure!(
                offset + 2 <= expected.len(),
                "FLAC decoded too many samples"
            );
            let expected = i32::from(i16::from_le_bytes([expected[offset], expected[offset + 1]]));
            ensure!(sample == expected, "FLAC round-trip changed a PCM sample");
        }
        decoded_samples += 1;
    }
    ensure!(
        decoded_samples
            == frames
                .checked_mul(u64::from(AUDIO_CHANNELS))
                .context("FLAC decoded sample count overflow",)?,
        "FLAC decoded sample count mismatch"
    );
    Ok(())
}

fn inspect_video(
    path: &Path,
    expected_samples: Option<u64>,
    native_format: Option<NativeFormat>,
) -> anyhow::Result<VideoDescriptor> {
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let size = file.metadata()?.len();
    let mut reader = Mp4Reader::read_header(BufReader::new(file), size)
        .with_context(|| format!("invalid MP4 {}", path.display()))?;
    ensure!(!reader.is_fragmented(), "fragmented MP4 is not supported");
    ensure!(
        reader.tracks().len() == 1,
        "MP4 must contain exactly one track"
    );
    let track = reader.tracks().values().next().unwrap();
    ensure!(
        track.track_type()? == TrackType::Video,
        "MP4 track is not video"
    );
    ensure!(
        track.media_type()? == MediaType::H264,
        "MP4 video is not H.264"
    );
    ensure!(
        track.video_profile()? == AvcProfile::AvcHigh,
        "MP4 video is not H.264 High profile"
    );
    let track_id = track.track_id();
    let descriptor_fields = (
        track.timescale(),
        track.language().to_owned(),
        track.width(),
        track.height(),
        track.sequence_parameter_set()?.to_vec(),
        track.picture_parameter_set()?.to_vec(),
        track.sample_count(),
    );
    let (
        track_timescale,
        language,
        width,
        height,
        sequence_parameter_set,
        picture_parameter_set,
        sample_count,
    ) = descriptor_fields;
    if let Some(expected) = expected_samples {
        ensure!(
            u64::from(sample_count) == expected,
            "MP4 sample count mismatch"
        );
    }
    if let Some(native_format) = native_format {
        ensure!(
            u32::from(width) == native_format.width && u32::from(height) == native_format.height,
            "MP4 dimensions do not match the native capture profile"
        );
    }
    ensure!(sample_count > 0, "MP4 has no samples");
    let mut sync_samples = Vec::new();
    let mut expected_start = 0_u64;
    for sample_id in 1..=sample_count {
        let sample = reader
            .read_sample(track_id, sample_id)?
            .with_context(|| format!("MP4 sample {sample_id} is missing"))?;
        ensure!(sample.duration > 0, "MP4 sample duration is zero");
        if let Some(native_format) = native_format {
            ensure!(
                sample_duration_matches_rate(sample.duration, track_timescale, native_format),
                "MP4 sample duration does not match native {}fps",
                native_format.rate_label()
            );
        }
        ensure!(
            sample.rendering_offset == 0,
            "MP4 contains reordered/B-frame samples"
        );
        ensure!(
            sample.start_time == expected_start,
            "MP4 sample timeline is not contiguous"
        );
        expected_start = expected_start
            .checked_add(u64::from(sample.duration))
            .context("MP4 sample timeline overflow")?;
        let contains_idr = h264_sample_contains_idr(&sample.bytes)?;
        ensure!(
            sample.is_sync == contains_idr,
            "MP4 sync table and H.264 IDR samples disagree"
        );
        if sample.is_sync {
            sync_samples.push(sample_id - 1);
        }
    }
    ensure!(
        sync_samples.first() == Some(&0),
        "MP4 does not start with a sync sample"
    );
    Ok(VideoDescriptor {
        movie_timescale: reader.timescale(),
        track_timescale,
        language,
        width,
        height,
        sequence_parameter_set,
        picture_parameter_set,
        sample_count,
        sync_samples,
    })
}

fn h264_sample_contains_idr(bytes: &[u8]) -> anyhow::Result<bool> {
    let mut offset = 0_usize;
    let mut contains_idr = false;
    while offset < bytes.len() {
        ensure!(
            bytes.len() - offset >= 4,
            "H.264 AVCC sample has a truncated NAL length"
        );
        let length = u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
        offset += 4;
        ensure!(length > 0, "H.264 AVCC sample contains an empty NAL unit");
        let end = offset
            .checked_add(length)
            .context("H.264 NAL length overflow")?;
        ensure!(
            end <= bytes.len(),
            "H.264 AVCC sample has a truncated NAL unit"
        );
        contains_idr |= bytes[offset] & 0x1f == 5;
        offset = end;
    }
    Ok(contains_idr)
}

fn sample_duration_matches_rate(
    duration: u32,
    track_timescale: u32,
    native_format: NativeFormat,
) -> bool {
    let actual = u64::from(duration) * u64::from(native_format.fps_num);
    let expected = u64::from(track_timescale) * u64::from(native_format.fps_den);
    actual.abs_diff(expected) <= u64::from(native_format.fps_num)
}

fn validate_chunk_boundaries(session: &ValidatedSession) -> anyhow::Result<()> {
    let first = &session.segments[0].video;
    let chunk_frames = session.native_format.chunk_frames;
    let mut gframe = 0_usize;
    for segment in &session.segments {
        ensure!(
            same_codec_config(first, &segment.video),
            "source MP4 codec configuration changes at {}/{}",
            segment.run_id,
            segment.segment_index
        );
        let sync = segment
            .video
            .sync_samples
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        for local in 0..segment.video.sample_count {
            if gframe.is_multiple_of(chunk_frames) {
                ensure!(
                    sync.contains(&local),
                    "data_demo native GOP boundary gframe {gframe} is not an H.264 sync sample (source {}/{}, local frame {local}); refusing lossy or fake conversion",
                    segment.run_id,
                    segment.segment_index
                );
            }
            gframe += 1;
        }
    }
    Ok(())
}

fn materialize_dataset(
    session: &ValidatedSession,
    staging: &Path,
) -> anyhow::Result<ConversionReport> {
    let mapping = session.mapping.mapping();
    let game_slug = slug(&mapping.game.id);
    let session_dir = staging.join(&game_slug).join(&session.session.session_id);
    fs::create_dir_all(&session_dir)?;
    let video_template = session.segments[0].video.clone();
    let mut open_chunk: Option<OpenChunk> = None;
    let mut all_rows = Vec::new();
    let mut chunk_summaries = Vec::new();

    for segment in &session.segments {
        let video_path = segment.path.join("video.mp4");
        ensure!(
            sha256_file(&video_path)? == segment.source_hashes["video.mp4"],
            "source video changed after ready validation"
        );
        let file = File::open(&video_path)?;
        let size = file.metadata()?.len();
        let mut reader = Mp4Reader::read_header(BufReader::new(file), size)?;
        let track_id = reader.tracks().values().next().unwrap().track_id();
        let segment_audio = SegmentAudioData::load(segment)?;
        for (local, raw_row) in segment.rows.iter().enumerate() {
            let sample = reader
                .read_sample(track_id, u32::try_from(local + 1)?)?
                .context("source MP4 sample disappeared after validation")?;
            let gframe = all_rows.len() as u64;
            if open_chunk.is_none() {
                ensure!(
                    sample.is_sync,
                    "output chunk would not start with a sync sample"
                );
                open_chunk = Some(OpenChunk::create(
                    &session_dir,
                    u32::try_from(chunk_summaries.len())?,
                    &video_template,
                    session.native_format.chunk_frames,
                    session.audio_enabled,
                )?);
            }
            let row = DataDemoFrame::from_raw(
                raw_row,
                gframe,
                session.native_format,
                session.wall_clock.as_ref(),
                session.view_scale.as_ref(),
            )?;
            let source = format!("{}/{:05}", segment.run_id, segment.segment_index);
            let chunk = open_chunk.as_mut().unwrap();
            let audio = segment_audio
                .as_ref()
                .map(|audio| audio.slice(raw_row))
                .transpose()?;
            chunk.write_sample(sample, row.clone(), source, audio)?;
            all_rows.push(row);
            if chunk.rows.len() == session.native_format.chunk_frames {
                let finished = open_chunk.take().unwrap().finish(session)?;
                chunk_summaries.push(finished);
            }
        }
    }
    if let Some(chunk) = open_chunk.take() {
        chunk_summaries.push(chunk.finish(session)?);
    }
    ensure!(!chunk_summaries.is_empty(), "conversion produced no chunks");

    let run_frame_ranges = build_run_frame_ranges(session);
    let pairs = build_framepairs(
        &session.session.session_id,
        session.compliance.consent_id(),
        &all_rows,
        session.native_format.chunk_frames,
        &run_frame_ranges,
    );
    write_jsonl(&session_dir.join("framepairs.jsonl"), &pairs)?;
    let metadata = build_session_metadata(session, &all_rows, &pairs, &chunk_summaries);
    write_json(&session_dir.join("metadata.json"), &metadata)?;
    write_jsonl(
        &staging.join("metadata.jsonl"),
        std::slice::from_ref(&metadata),
    )?;
    let stats = build_stats(&metadata);
    write_json(&staging.join("_dataset_stats.json"), &stats)?;
    fs::write(staging.join("DATASHEET.md"), datasheet(&metadata))?;

    Ok(ConversionReport {
        session_id: session.session.session_id.clone(),
        frame_count: all_rows.len() as u64,
        chunk_count: chunk_summaries.len() as u32,
        output: staging.to_path_buf(),
    })
}

struct SegmentAudioData {
    metadata: SegmentAudioMetadata,
    pcm: Option<Vec<u8>>,
}

impl SegmentAudioData {
    fn load(segment: &ValidatedSegment) -> anyhow::Result<Option<Self>> {
        let Some(metadata) = &segment.audio else {
            return Ok(None);
        };
        Self::load_from(
            &segment.path,
            metadata,
            segment
                .source_hashes
                .get(AUDIO_ARTIFACT_NAME)
                .map(String::as_str),
        )
        .map(Some)
    }

    fn load_from(
        segment_dir: &Path,
        metadata: &SegmentAudioMetadata,
        expected_hash: Option<&str>,
    ) -> anyhow::Result<Self> {
        let pcm = if metadata.has_artifact() {
            let path = segment_dir.join(AUDIO_ARTIFACT_NAME);
            let expected_hash = expected_hash.context("present audio has no source hash")?;
            ensure!(
                sha256_file(&path)? == expected_hash,
                "source audio changed after ready validation"
            );
            Some(read_segment_pcm(segment_dir, metadata)?)
        } else {
            ensure!(
                expected_hash.is_none(),
                "absent audio unexpectedly carries a source hash"
            );
            None
        };
        Ok(Self {
            metadata: metadata.clone(),
            pcm,
        })
    }

    fn slice<'a>(&'a self, row: &RawFrameProjection) -> anyhow::Result<AudioFrameSlice<'a>> {
        let failure_code = self.metadata.failure_code.as_deref();
        if !self.metadata.is_publicly_publishable() {
            return Ok(AudioFrameSlice {
                state: self.metadata.state,
                pcm: &[],
                failure_code,
            });
        }
        let first_qpc = self
            .metadata
            .first_sample_qpc
            .context("ready audio has no first sample QPC")?;
        ensure!(
            row.output_qpc_start >= first_qpc && row.output_qpc_end >= row.output_qpc_start,
            "video row precedes its audio sample range"
        );
        let start = audio_frames_for_qpc_span(
            first_qpc,
            row.output_qpc_start,
            self.metadata.qpc_frequency,
        )?;
        let end =
            audio_frames_for_qpc_span(first_qpc, row.output_qpc_end, self.metadata.qpc_frequency)?;
        let bytes_per_frame = self.metadata.format.bytes_per_frame();
        let byte_start = usize::try_from(
            start
                .checked_mul(bytes_per_frame)
                .context("audio slice start overflow")?,
        )?;
        let byte_end = usize::try_from(
            end.checked_mul(bytes_per_frame)
                .context("audio slice end overflow")?,
        )?;
        let pcm = self.pcm.as_ref().context("ready audio PCM is missing")?;
        ensure!(byte_end <= pcm.len(), "audio slice exceeds segment PCM");
        Ok(AudioFrameSlice {
            state: SegmentAudioState::Ready,
            pcm: &pcm[byte_start..byte_end],
            failure_code: None,
        })
    }
}

struct AudioFrameSlice<'a> {
    state: SegmentAudioState,
    pcm: &'a [u8],
    failure_code: Option<&'a str>,
}

struct ChunkAudioAccumulator {
    state: SegmentAudioState,
    pcm: Vec<u8>,
    failure_codes: BTreeSet<String>,
}

impl ChunkAudioAccumulator {
    fn new(chunk_frames: usize) -> Self {
        Self {
            state: SegmentAudioState::Ready,
            pcm: Vec::with_capacity(chunk_frames * 6_400),
            failure_codes: BTreeSet::new(),
        }
    }

    fn ingest(&mut self, slice: AudioFrameSlice<'_>) {
        match slice.state {
            SegmentAudioState::Ready if self.state == SegmentAudioState::Ready => {
                self.pcm.extend_from_slice(slice.pcm);
            }
            SegmentAudioState::Ready => {}
            SegmentAudioState::Degraded => {
                if self.state == SegmentAudioState::Ready {
                    self.state = SegmentAudioState::Degraded;
                    self.pcm.clear();
                }
                self.failure_codes
                    .insert("AUDIO_DEGRADED_NOT_PUBLIC".into());
                if let Some(code) = slice.failure_code {
                    self.failure_codes.insert(code.into());
                }
            }
            SegmentAudioState::Absent => {
                self.state = SegmentAudioState::Absent;
                self.pcm.clear();
                self.failure_codes
                    .insert(slice.failure_code.unwrap_or("AUDIO_UNAVAILABLE").to_owned());
            }
        }
    }

    fn finish(self, directory: &Path) -> anyhow::Result<DataDemoAudio> {
        let mut audio = DataDemoAudio {
            state: self.state,
            source: "process_loopback".into(),
            present: false,
            sample_rate: AUDIO_SAMPLE_RATE_HZ,
            channels: AUDIO_CHANNELS,
            bits: AUDIO_BITS_PER_SAMPLE,
            sample_format: AUDIO_SAMPLE_FORMAT.into(),
            file: None,
            codec: None,
            frames: None,
            sha256: None,
            failure_codes: self.failure_codes.into_iter().collect(),
        };
        if self.state == SegmentAudioState::Ready {
            let frames = u64::try_from(self.pcm.len())? / 4;
            ensure!(frames > 0, "ready chunk audio is empty");
            let path = directory.join("p0.flac");
            write_flac(&path, &self.pcm, frames)?;
            audio.present = true;
            audio.file = Some("p0.flac".into());
            audio.codec = Some("flac".into());
            audio.frames = Some(frames);
            audio.sha256 = Some(sha256_file(&path)?);
        }
        Ok(audio)
    }
}

struct OpenChunk {
    index: u32,
    directory: PathBuf,
    writer: Mp4Writer<BufWriter<File>>,
    next_start_time: u64,
    rows: Vec<DataDemoFrame>,
    source_segments: BTreeSet<String>,
    audio: Option<ChunkAudioAccumulator>,
}

impl OpenChunk {
    fn create(
        session_dir: &Path,
        index: u32,
        video: &VideoDescriptor,
        chunk_frames: usize,
        audio_enabled: bool,
    ) -> anyhow::Result<Self> {
        let directory = session_dir.join(format!("c{index:04}"));
        Self::create_at(directory, index, video, chunk_frames, audio_enabled)
    }

    fn create_at(
        directory: PathBuf,
        index: u32,
        video: &VideoDescriptor,
        chunk_frames: usize,
        audio_enabled: bool,
    ) -> anyhow::Result<Self> {
        fs::create_dir(&directory)?;
        let file = BufWriter::new(File::create(directory.join("p0.mp4"))?);
        let mp4_config = Mp4Config {
            major_brand: "isom".parse()?,
            minor_version: 512,
            compatible_brands: vec![
                "isom".parse()?,
                "iso2".parse()?,
                "avc1".parse()?,
                "mp41".parse()?,
            ],
            timescale: video.movie_timescale,
        };
        let mut writer = Mp4Writer::write_start(file, &mp4_config)?;
        writer.add_track(&TrackConfig {
            track_type: TrackType::Video,
            timescale: video.track_timescale,
            language: video.language.clone(),
            media_conf: MediaConfig::AvcConfig(AvcConfig {
                width: video.width,
                height: video.height,
                seq_param_set: video.sequence_parameter_set.clone(),
                pic_param_set: video.picture_parameter_set.clone(),
            }),
        })?;
        Ok(Self {
            index,
            directory,
            writer,
            next_start_time: 0,
            rows: Vec::with_capacity(chunk_frames),
            source_segments: BTreeSet::new(),
            audio: audio_enabled.then(|| ChunkAudioAccumulator::new(chunk_frames)),
        })
    }

    fn write_sample(
        &mut self,
        mut sample: Mp4Sample,
        row: DataDemoFrame,
        source: String,
        audio: Option<AudioFrameSlice<'_>>,
    ) -> anyhow::Result<()> {
        sample.start_time = self.next_start_time;
        self.next_start_time = self
            .next_start_time
            .checked_add(u64::from(sample.duration))
            .context("output MP4 timeline overflow")?;
        self.writer.write_sample(1, &sample)?;
        self.rows.push(row);
        self.source_segments.insert(source);
        match (&mut self.audio, audio) {
            (Some(accumulator), Some(slice)) => accumulator.ingest(slice),
            (None, None) => {}
            _ => anyhow::bail!("chunk audio state does not match the session generation"),
        }
        Ok(())
    }

    fn finish(mut self, session: &ValidatedSession) -> anyhow::Result<ChunkSummary> {
        self.writer.write_end()?;
        let mut output = self.writer.into_writer();
        output.flush()?;
        output.get_ref().sync_all()?;
        write_jsonl(&self.directory.join("p0.jsonl"), &self.rows)?;
        let audio = self
            .audio
            .take()
            .map(|audio| audio.finish(&self.directory))
            .transpose()?;
        let mapping = session.mapping.mapping();
        let stamp = session.compliance.stamp();
        let meta = ChunkMeta {
            schema_version: DATA_MODEL_SCHEMA_VERSION.into(),
            sample_key: format!("{}_c{:04}", session.session.session_id, self.index),
            session: session.session.session_id.clone(),
            chunk: self.index,
            game: mapping.game.name.clone(),
            category: mapping.taxonomy.content.clone(),
            platform: mapping.game.platform.clone(),
            video: DataDemoVideo {
                file: "p0.mp4".into(),
                codec: "h264".into(),
                width: session.native_format.width,
                height: session.native_format.height,
                fps: session.native_format.fps(),
                fps_num: session.native_format.fps_num,
                fps_den: session.native_format.fps_den,
                frames: self.rows.len() as u64,
                audio: false,
            },
            audio: audio.clone(),
            annotation_source: stamp.annotation_source.into(),
            key_schema: format!(
                "VK codes + dm-1 mapping {}@{}",
                mapping.mapping_id, mapping.version
            ),
            rights: stamp.rights.into(),
            first_party: stamp.first_party,
            consent_id: stamp.consent_id,
            attestation: stamp.attestation,
            non_trainable: stamp.non_trainable,
            is_trainable: stamp.is_trainable,
            production_certified: stamp.production_certified,
            is_cutscene: false,
            catalog: data_model_catalog(session.session.catalog.as_ref()),
            provenance: ChunkProvenance {
                raw_session_id: session.session.session_id.clone(),
                raw_segments: self.source_segments.into_iter().collect(),
                mapping_id: mapping.mapping_id.clone(),
                mapping_version: mapping.version.clone(),
                mapping_content_hash: session.mapping.content_hash().into(),
            },
        };
        write_json(&self.directory.join("meta.json"), &meta)?;
        Ok(ChunkSummary {
            frame_count: self.rows.len() as u64,
            audio,
        })
    }
}

struct ChunkSummary {
    frame_count: u64,
    audio: Option<DataDemoAudio>,
}

/// Untruncated per-frame pointer sum (#59 item 2): the signed sum of every
/// `mouse_move` delta in the frame, with no ±200 clamp. Saturating i32 add so a
/// pathological frame cannot panic in debug; real frames are far from the bound.
fn pointer_raw_sum(events: &[RawFrameEvent]) -> (i32, i32) {
    let mut dx = 0_i32;
    let mut dy = 0_i32;
    for event in events {
        if event.event_type == "mouse_move" {
            dx = dx.saturating_add(event.dx.unwrap_or(0));
            dy = dy.saturating_add(event.dy.unwrap_or(0));
        }
    }
    (dx, dy)
}

/// Epoch milliseconds carried by a session id, when it carries one.
///
/// The local dev recorder names sessions `dev-<epoch_ms>-<n>`, which is the only
/// bridge to wall clock that exists anywhere in RAW. Any other id shape (a ULID,
/// for instance) yields `None` rather than a guess, so a session whose naming we
/// do not understand simply ships without the estimate.
/// Parses the exact shape the recorder writes — `YYYY-MM-DDTHH:MM:SS.mmmZ` —
/// into epoch milliseconds. Deliberately strict: anything else is a value this
/// crate did not produce, and guessing at it would launder an unknown format
/// into an authoritative-looking timestamp.
fn parse_iso8601_millis_utc(value: &str) -> Option<i64> {
    let bytes = value.as_bytes();
    if bytes.len() != 24 || bytes[23] != b'Z' {
        return None;
    }
    let field = |start: usize, end: usize, expect: u8| -> Option<i64> {
        if end < bytes.len() && bytes[end] != expect {
            return None;
        }
        let field = value.get(start..end)?;
        (!field.is_empty() && field.bytes().all(|byte| byte.is_ascii_digit()))
            .then(|| field.parse().ok())?
    };
    let year = field(0, 4, b'-')?;
    let month = field(5, 7, b'-')?;
    let day = field(8, 10, b'T')?;
    let hour = field(11, 13, b':')?;
    let minute = field(14, 16, b':')?;
    let second = field(17, 19, b'.')?;
    let milli = field(20, 23, b'Z')?;
    if !(1..=9999).contains(&year)
        || !(1..=12).contains(&month)
        || !(1..=days_in_month(year, month)?).contains(&day)
        || !(0..=23).contains(&hour)
        || !(0..=59).contains(&minute)
        || !(0..=59).contains(&second)
        || !(0..=999).contains(&milli)
    {
        return None;
    }
    let days = days_from_civil(year, month, day)?;
    Some(((days * 86_400 + hour * 3_600 + minute * 60 + second) * 1_000) + milli)
}

fn days_in_month(year: i64, month: i64) -> Option<i64> {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => Some(31),
        4 | 6 | 9 | 11 => Some(30),
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => Some(29),
        2 => Some(28),
        _ => None,
    }
}

/// Howard Hinnant's `days_from_civil`: proleptic Gregorian date to days since
/// the Unix epoch. The inverse of the `civil_from_days` the recorder formats
/// with, so a round trip through `run.json` is lossless.
fn days_from_civil(year: i64, month: i64, day: i64) -> Option<i64> {
    let year = year - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let year_of_era = year.rem_euclid(400);
    let day_of_year = (153 * if month > 2 { month - 3 } else { month + 9 } + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era.checked_mul(146_097)?.checked_add(day_of_era - 719_468)
}

fn session_epoch_ms(session_id: &str) -> Option<i64> {
    let mut parts = session_id.split('-');
    if parts.next()? != "dev" {
        return None;
    }
    let epoch: i64 = parts.next()?.parse().ok()?;
    // A plausible-instant guard: reject anything outside 2001-09-09..2286-11-20,
    // so a numeric field that is not a timestamp cannot masquerade as one.
    (1_000_000_000_000..10_000_000_000_000)
        .contains(&epoch)
        .then_some(epoch)
}

impl DataDemoFrame {
    fn from_raw(
        row: &RawFrameProjection,
        gframe: u64,
        native_format: NativeFormat,
        wall_clock: Option<&WallClockAnchor>,
        view_scale: Option<&ViewScaleConstants>,
    ) -> anyhow::Result<Self> {
        let (pointer_raw_dx, pointer_raw_dy) = pointer_raw_sum(&row.events);
        // 角度真值从 raw 通路派生（docs/46 §2.5）：pointerDx/Dy 带 ±200 clamp，
        // 快速甩枪会截断角速度，绝不能作角度源。
        let (look_yaw_delta_rad, look_pitch_delta_rad) = match view_scale {
            Some(constants) => (
                Some(view_scale::yaw_delta_rad(
                    i64::from(pointer_raw_dx),
                    constants.counts_per_360,
                )),
                Some(view_scale::pitch_delta_rad(
                    i64::from(pointer_raw_dy),
                    constants.counts_per_360,
                    constants.vertical_scale_ratio,
                )),
            ),
            None => (None, None),
        };
        Ok(Self {
            frame: gframe % native_format.chunk_frames as u64,
            gframe,
            video_t_ms: native_format.video_time_ms(gframe),
            qpc: row.output_qpc_start,
            qpc_end: row.output_qpc_end,
            t_utc_est_us: wall_clock
                .map(|anchor| anchor.estimate_us(row.output_qpc_start))
                .transpose()?,
            keys: row.keys.clone(),
            // Absent in pre-#59 RAW; an empty held set is the truthful
            // projection of "this RAW never recorded button state".
            buttons: row.buttons.clone().unwrap_or_default(),
            actions: row.actions.clone(),
            pointer_dx: row.pointer_dx,
            pointer_dy: row.pointer_dy,
            pointer_raw_dx,
            pointer_raw_dy,
            look_yaw_delta_rad,
            look_pitch_delta_rad,
            view_scale_valid: view_scale.is_some(),
            pointer_bins: row.pointer_bins,
            events: row.events.clone(),
            src: row.src.clone(),
        })
    }
}

fn build_framepairs(
    session_id: &str,
    consent_id: Option<&str>,
    rows: &[DataDemoFrame],
    chunk_frames: usize,
    run_frame_ranges: &[RunFrameRange],
) -> Vec<FramePair> {
    let mut pairs = Vec::new();
    let frame_count = rows.len() as u64;
    let Some(limit) = frame_count.checked_sub(FRAMEPAIR_GAP) else {
        return pairs;
    };
    for frame_t in (0..limit).step_by(FRAMEPAIR_STRIDE as usize) {
        let frame_th = frame_t + FRAMEPAIR_GAP;
        if !run_frame_ranges
            .iter()
            .any(|range| frame_t >= range.frame_start && frame_th < range.frame_end)
        {
            continue;
        }
        let row = &rows[frame_t as usize];
        pairs.push(FramePair {
            clip_id: session_id.into(),
            consent_id: consent_id.map(str::to_owned),
            frame_t,
            gap_h: FRAMEPAIR_GAP,
            chunk_t: (frame_t / chunk_frames as u64) as u32,
            chunk_th: ((frame_t + FRAMEPAIR_GAP) / chunk_frames as u64) as u32,
            action_t: FramePairAction {
                keys: row.keys.clone(),
                buttons: row.buttons.clone(),
                actions: row.actions.clone(),
                pointer_dx: row.pointer_dx,
                pointer_dy: row.pointer_dy,
                pointer_raw_dx: row.pointer_raw_dx,
                pointer_raw_dy: row.pointer_raw_dy,
                look_yaw_delta_rad: row.look_yaw_delta_rad,
                look_pitch_delta_rad: row.look_pitch_delta_rad,
                view_scale_valid: row.view_scale_valid,
            },
        });
    }
    pairs
}

fn build_run_frame_ranges(session: &ValidatedSession) -> Vec<RunFrameRange> {
    let mut ranges = Vec::<RunFrameRange>::new();
    let mut next_frame = 0_u64;
    for segment in &session.segments {
        let segment_end = next_frame + segment.rows.len() as u64;
        if let Some(current) = ranges.last_mut()
            && current.run_id == segment.run_id
        {
            current.frame_end = segment_end;
        } else {
            ranges.push(RunFrameRange {
                run_id: segment.run_id.clone(),
                frame_start: next_frame,
                frame_end: segment_end,
            });
        }
        next_frame = segment_end;
    }
    ranges
}

fn validated_run_frame_ranges(
    ranges: &[RunFrameRange],
    frame_count: u64,
) -> anyhow::Result<Vec<RunFrameRange>> {
    if ranges.is_empty() {
        return Ok(vec![RunFrameRange {
            run_id: "legacy".into(),
            frame_start: 0,
            frame_end: frame_count,
        }]);
    }
    let mut next_frame = 0_u64;
    let mut run_ids = BTreeSet::new();
    for range in ranges {
        ensure_safe_component(&range.run_id, "run frame range id")?;
        ensure!(
            run_ids.insert(range.run_id.clone()),
            "run frame range ids are not unique"
        );
        ensure!(
            range.frame_start == next_frame && range.frame_end > range.frame_start,
            "run frame ranges are not contiguous"
        );
        next_frame = range.frame_end;
    }
    ensure!(
        next_frame == frame_count,
        "run frame ranges do not cover dataset frames"
    );
    Ok(ranges.to_vec())
}

fn build_session_metadata(
    session: &ValidatedSession,
    rows: &[DataDemoFrame],
    pairs: &[FramePair],
    chunks: &[ChunkSummary],
) -> SessionMetadata {
    let mapping = session.mapping.mapping();
    debug_assert_eq!(
        chunks.iter().map(|chunk| chunk.frame_count).sum::<u64>(),
        rows.len() as u64
    );
    let labeled_frames = rows.iter().filter(|row| is_labeled_frame(row)).count() as u64;
    let stamp = session.compliance.stamp();
    SessionMetadata {
        schema_version: DATA_MODEL_SCHEMA_VERSION.into(),
        workflow_id: session.session.session_id.clone(),
        game: mapping.game.name.clone(),
        category: mapping.taxonomy.content.clone(),
        platform: mapping.game.platform.clone(),
        source_video: format!("ready-session:{}", session.session.session_id),
        org: format!(
            "data_demo-compatible: native 4s GOP/chunk x {}, key {}_cNNNN, p0.mp4+p0.jsonl+meta.json",
            chunks.len(),
            session.session.session_id
        ),
        capture: DatasetCapture {
            fps: session.native_format.fps(),
            fps_num: session.native_format.fps_num,
            fps_den: session.native_format.fps_den,
            width: session.native_format.width,
            height: session.native_format.height,
            codec: "h264".into(),
            frame_count: rows.len() as u64,
            chunks: chunks.len() as u32,
            chunk_frames: session.native_format.chunk_frames as u32,
        },
        duration_ms: session.native_format.video_time_ms(rows.len() as u64),
        frame_pairs: FramePairSummary {
            count: pairs.len() as u64,
            gap_h: FRAMEPAIR_GAP,
            stride: FRAMEPAIR_STRIDE,
            overlapping: true,
        },
        annotation: AnnotationSummary {
            source: stamp.annotation_source.into(),
            labeled_frames,
            note: stamp.annotation_note.into(),
        },
        rights: stamp.rights.into(),
        first_party: stamp.first_party,
        consent_id: stamp.consent_id,
        attestation: stamp.attestation,
        non_trainable: stamp.non_trainable,
        is_trainable: stamp.is_trainable,
        production_certified: stamp.production_certified,
        is_cutscene: false,
        tags: stamp.tags.iter().map(|tag| (*tag).to_owned()).collect(),
        audio: build_audio_summary(chunks),
        view_scale: session
            .view_scale
            .as_ref()
            .map(ViewScaleRef::from_constants),
        catalog: data_model_catalog(session.session.catalog.as_ref()),
        provenance: SessionProvenance {
            raw_schema_version: session.session.schema_version.clone(),
            raw_session_id: session.session.session_id.clone(),
            mapping_id: mapping.mapping_id.clone(),
            mapping_version: mapping.version.clone(),
            mapping_content_hash: session.mapping.content_hash().into(),
            source_segments: session
                .segments
                .iter()
                .map(|segment| SourceSegment {
                    run_id: segment.run_id.clone(),
                    segment_index: segment.segment_index,
                    hashes: segment.source_hashes.clone(),
                })
                .collect(),
            run_frame_ranges: build_run_frame_ranges(session),
        },
    }
}

fn build_audio_summary(chunks: &[ChunkSummary]) -> Option<DatasetAudioSummary> {
    summarize_audio(chunks.iter().map(|chunk| chunk.audio.as_ref()))
}

fn summarize_audio<'a>(
    chunks: impl Iterator<Item = Option<&'a DataDemoAudio>>,
) -> Option<DatasetAudioSummary> {
    let chunks = chunks.collect::<Vec<_>>();
    if chunks.iter().all(|audio| audio.is_none()) {
        return None;
    }
    let mut ready_chunks = 0_u32;
    let mut degraded_chunks = 0_u32;
    let mut absent_chunks = 0_u32;
    for audio in chunks.into_iter().flatten() {
        match audio.state {
            SegmentAudioState::Ready => ready_chunks += 1,
            SegmentAudioState::Degraded => degraded_chunks += 1,
            SegmentAudioState::Absent => absent_chunks += 1,
        }
    }
    Some(DatasetAudioSummary {
        source: "process_loopback".into(),
        codec: "flac".into(),
        sample_rate: AUDIO_SAMPLE_RATE_HZ,
        channels: AUDIO_CHANNELS,
        bits: AUDIO_BITS_PER_SAMPLE,
        sample_format: AUDIO_SAMPLE_FORMAT.into(),
        ready_chunks,
        degraded_chunks,
        absent_chunks,
        all_publishable: degraded_chunks == 0 && absent_chunks == 0,
    })
}

fn build_stats(metadata: &SessionMetadata) -> DatasetStats {
    DatasetStats {
        sessions: 1,
        games: vec![metadata.game.clone()],
        total_hours: (metadata.duration_ms as f64 / 3_600_000.0 * 1_000.0).round() / 1_000.0,
        total_frames: metadata.capture.frame_count,
        total_chunks: metadata.capture.chunks,
        total_frame_pairs: metadata.frame_pairs.count,
        datacard: DatasetCard {
            fps: metadata.capture.fps,
            fps_num: metadata.capture.fps_num,
            fps_den: metadata.capture.fps_den,
            resolution: format!("{}x{}", metadata.capture.width, metadata.capture.height),
            carrier: "h264_mp4_chunks".into(),
            chunk_s: 4,
            gap_h: FRAMEPAIR_GAP,
            stride: FRAMEPAIR_STRIDE,
            overlapping: true,
            org: "data_demo-compatible".into(),
            merged_jsonl: true,
            disclosures: DatasetDisclosures {
                // Structural facts shared by dm-1 and dm-2, true for every conversion.
                has_reward_channel: false,
                has_termination_channel: false,
                has_game_state_channel: false,
                // Not measurable from RAW; stays null until a per-machine
                // calibration is supplied. See the field docs.
                input_render_latency_frames: None,
                window_rule: AGG_WINDOW_RULE.into(),
                key_semantics: AGG_KEY_SEMANTICS.into(),
                button_semantics: AGG_BUTTON_SEMANTICS.into(),
                pointer_aggregation: AGG_POINTER_AGGREGATION.into(),
                mulaw_scheme: AGG_MULAW_SCHEME.into(),
                downsample_rule: AGG_DOWNSAMPLE_RULE.into(),
                duplicate_semantics: AGG_DUPLICATE_SEMANTICS.into(),
            },
        },
        compliance: DatasetCompliance {
            all_non_trainable: metadata.non_trainable,
            trainable_sessions: u32::from(metadata.is_trainable),
            production_certified_sessions: u32::from(metadata.production_certified),
            attestation: metadata.attestation.clone(),
            note: if metadata.attestation.is_some() {
                "Attested-trainable conversion; eligibility carried by the attestation.waiver_ref WAIVER; is_trainable=true; production_certified=false.".into()
            } else if metadata.is_trainable {
                "First-party release conversion; consent-gated; is_trainable=true; production_certified=true.".into()
            } else {
                "Local debug conversion; non_trainable=true; production_certified=false.".into()
            },
        },
    }
}

fn datasheet(metadata: &SessionMetadata) -> String {
    let view_scale_note = match &metadata.view_scale {
        Some(reference) => format!(
            "- view-scale angles: **calibrated** (`{}`). `look_yaw_delta_rad`/`look_pitch_delta_rad` \
             are derived per frame from raw mouse counts with `counts_per_360={}` and \
             `vertical_scale_ratio={}` taken from this session's hash-verified settings snapshot; \
             `view_scale_valid=true` on every frame.",
            reference.norm, reference.counts_per_360, reference.vertical_scale_ratio
        ),
        None => "- view-scale angles: **not calibrated for this session**. \
                 `look_yaw_delta_rad`/`look_pitch_delta_rad` are null and `view_scale_valid=false`: \
                 the session predates the calibration contract, the game is not aim-driven, the \
                 catalog constants are not yet versioned, or the catalog does not declare its \
                 aim-down-sights states (hip-fire-only constants would mislabel ADS segments). \
                 The converter never substitutes another setup's constants."
            .to_owned(),
    };
    let compliance_note = if let Some(attestation) = &metadata.attestation {
        format!(
            "This attested-trainable artifact carries no capture-time consent id: its training \
             eligibility is carried by the owner attestation WAIVER `{}`. `is_trainable=true`, \
             `production_certified=false` (attestation is not certification). See the \
             attested-lane disclosures below.",
            attestation.waiver_ref
        )
    } else if metadata.is_trainable {
        format!(
            "This first-party release artifact is consent-gated (consent id `{}`), \
             `is_trainable=true`, and `production_certified=true`.",
            metadata.consent_id.as_deref().unwrap_or("<missing>")
        )
    } else {
        "This local debug artifact remains `non_trainable=true` and `production_certified=false`."
            .to_owned()
    };
    // WAIVER `restrictions`/`quality_disclaimer` 的机器落地（docs/141 §2）：具结档
    // 固定追加三点披露，S5 验证器据此硬校验。另两档为空串，输出字节零变化。
    let attested_disclosures = match &metadata.attestation {
        Some(attestation) => format!(
            "\n## 具结车道披露（attested-trainable）\n\n\
             - **具结车道属性**：本数据集的取数资格由 owner 具结 WAIVER 承载\
             （`{}`），RAW 侧 `non_trainable=true`、`consent_id=null` 保持如实；\
             证据强度低于采集时点 consent，因此 `rights=first_party_attested`\
             （区别于 consent 车道的 `first_party`），`production_certified` 恒为 false。\n\
             - **`combat.aim` 坏标签**：该动作在本语料由 toggle-zoom 映射产生，是已知\
             坏标签；训练必须 mask 该通道，不得作为监督真值。\n\
             - **view_scale 未标定**：本车道会话不携带视角标定常数，camera 头只有 \
             μ-law 计数箱（`pointerBins`），`look_yaw_delta_rad`/`look_pitch_delta_rad` \
             为 null 且 `view_scale_valid=false`。\n",
            attestation.waiver_ref
        ),
        None => String::new(),
    };
    format!(
        "# DataMiner data_demo conversion\n\n\
         - session: `{}`\n\
         - game: `{}`\n\
         - format: H.264 {}x{}@{}fps ({}/{}), {} frames per native 4-second GOP chunk\n\
         - frame pairs: gapH={}, stride={}\n\
         - source: validated immutable RAW ready artifacts; MP4 samples were remuxed without decoding or re-encoding\n\n\
         ## What this dataset does not contain\n\n\
         dm-1 records video, raw input events, and their per-frame projection. It carries:\n\n\
         - **no reward channel**\n\
         - **no episode-termination channel**\n\
         - **no game-internal state** (health, score, position are never read)\n\n\
         Reinforcement-learning and model-based paradigms can therefore pretrain dynamics from \
         this data, but cannot learn a reward from it. Recovering a reward signal (HUD OCR or \
         similar) is an offline step outside this format.\n\n\
         - input-to-render latency: **not calibrated for this artifact**. It is measured per \
         capture machine by inspecting footage and is not recorded in RAW, so the converter \
         leaves it null rather than repeating another machine's figure.\n\n\
         {view_scale_note}\n\n\
         ## Action aggregation & downsampling\n\n\
         How every `p0.jsonl` row was aggregated (normative: docs/02 §3-§4; docs/58 §1.4). \
         Machine-readable mirrors live in `_dataset_stats.json` under `datacard.disclosures`.\n\n\
         - **window**: each input event lands in exactly one frame window by \
         `qpc ∈ [output_qpc_start, output_qpc_end)` (half-open).\n\
         - **`keys` / `buttons`**: the sets still **held at window end**, sorted ascending — \
         not \"pressed during the window\". A fast press-and-release inside one window appears \
         only in `events`.\n\
         - **`pointerDx/Dy`**: raw relative counts **summed over the window, then clamped once \
         to ±200**. Clamped values are unsuitable as regression targets (the clamp hits exactly \
         the fast-flick frames); regress on the unclamped per-window sums (`pointerRawDx/Dy` \
         where present, else re-derive by summing `events` mouse deltas per window).\n\
         - **`pointerBins`**: mu-law bins of the clamped sums, binned exactly once \
         (scheme `dm1-v2`: mu=8, ref=200, max_bin=10). Bin schemes are generational \
         (`dm1-v2` vs `legacy`) and **not interchangeable**: cross-corpus consumers drop \
         stored bins and re-derive from raw deltas.\n\
         - **`duplicate`**: a CFR pacer fact recorded at capture time (the slot had no fresh \
         source frame); never inferred from pixels.\n\
         - **Downsampling to a lower fps**: build target windows on time; **sum** pointer \
         displacement per window (sampling narrows the delta distribution); key/button \
         channels become the in-window **coverage ratio in [0,1]** (OR would stretch a tap \
         to the whole window — binarize with `>0` if needed); never re-clamp already-clamped \
         values; cut video and labels with the same window plan; upsampling is not defined \
         for this data.\n\n\
         ## Compliance\n\n\
         {compliance_note}\n\
         {attested_disclosures}",
        metadata.workflow_id,
        metadata.game,
        metadata.capture.width,
        metadata.capture.height,
        metadata.capture.fps,
        metadata.capture.fps_num,
        metadata.capture.fps_den,
        metadata.capture.chunk_frames,
        FRAMEPAIR_GAP,
        FRAMEPAIR_STRIDE
    )
}

fn validate_run(
    run: &RawRun,
    session: &RawSession,
    mapping: &MappingSnapshot,
    compliance: &ComplianceProfile,
) -> anyhow::Result<()> {
    let expected_schema = expected_recording_schema(&session.schema_version)?;
    ensure!(
        run.schema_version == expected_schema,
        "unsupported run schema"
    );
    let expected_claims = expected_raw_training_claims(&session.schema_version, compliance)?;
    ensure!(
        (run.non_trainable, run.production_certified) == expected_claims,
        "run training claims do not match its session generation"
    );
    if compliance.is_release() {
        ensure!(
            run.state == RAW_CONSENTED_RUN_COMPLETE_STATE,
            "run is not consented-complete"
        );
    } else {
        let expected_state = if is_neutral_ingest_session(&session.schema_version) {
            RAW_INGEST_V2_RUN_COMPLETE_STATE
        } else {
            RAW_RUN_COMPLETE_STATE
        };
        ensure!(run.state == expected_state, "run is not complete");
    }
    ensure!(
        run.session_id == session.session_id,
        "run session id mismatch"
    );
    ensure!(
        run.run_id.starts_with('r') && run.run_id[1..].bytes().all(|byte| byte.is_ascii_digit()),
        "invalid run id"
    );
    ensure!(
        run.mapping_id == mapping.mapping().mapping_id,
        "run mapping id mismatch"
    );
    ensure!(
        run.mapping_version == mapping.mapping().version,
        "run mapping version mismatch"
    );
    ensure!(
        run.mapping_content_hash == mapping.content_hash(),
        "run mapping hash mismatch"
    );
    Ok(())
}

fn validate_mapping_identity(
    identity: &RawMappingIdentity,
    mapping: &MappingSnapshot,
    source: &str,
) -> anyhow::Result<()> {
    ensure!(
        identity.id == mapping.mapping().mapping_id,
        "{source} mapping id mismatch"
    );
    ensure!(
        identity.version == mapping.mapping().version,
        "{source} mapping version mismatch"
    );
    ensure!(
        identity.content_hash == mapping.content_hash(),
        "{source} mapping hash mismatch"
    );
    Ok(())
}

fn supported_raw_session_schema(schema: &str) -> bool {
    matches!(
        schema,
        RAW_SESSION_SCHEMA_V1
            | RAW_SESSION_SCHEMA_V2
            | RAW_SESSION_SCHEMA_V3
            | RAW_INGEST_V2_SESSION_SCHEMA
    )
}

fn is_neutral_ingest_session(schema: &str) -> bool {
    schema == RAW_INGEST_V2_SESSION_SCHEMA
}

fn uses_catalog_snapshots(schema: &str) -> bool {
    matches!(
        schema,
        RAW_SESSION_SCHEMA_V2 | RAW_SESSION_SCHEMA_V3 | RAW_INGEST_V2_SESSION_SCHEMA
    )
}

fn expected_raw_training_claims(
    schema: &str,
    compliance: &ComplianceProfile,
) -> anyhow::Result<(Option<bool>, Option<bool>)> {
    if is_neutral_ingest_session(schema) {
        ensure!(
            !compliance.is_release(),
            "first-party-release refuses a neutral RAW session with no capture-time training claims"
        );
        Ok((None, None))
    } else {
        Ok((Some(!compliance.is_release()), Some(false)))
    }
}

fn expected_recording_schema(schema: &str) -> anyhow::Result<&'static str> {
    match schema {
        RAW_SESSION_SCHEMA_V1 | RAW_SESSION_SCHEMA_V2 => Ok(RAW_SEGMENT_SCHEMA),
        RAW_SESSION_SCHEMA_V3 => Ok(RAW_AUDIO_SEGMENT_SCHEMA),
        RAW_INGEST_V2_SESSION_SCHEMA => Ok(RAW_INGEST_V2_SEGMENT_SCHEMA),
        other => anyhow::bail!("unsupported RAW session schema {other}"),
    }
}

fn expected_audio_metadata(schema: &str, profile_has_audio: bool) -> anyhow::Result<bool> {
    match SessionContract::for_session_schema_version(schema) {
        Some(SessionContract::LocalDevV1) => Ok(schema == RAW_SESSION_SCHEMA_V3),
        Some(SessionContract::IngestV2) => Ok(profile_has_audio),
        None if schema == RAW_SESSION_SCHEMA_V1 => Ok(false),
        None => anyhow::bail!("unsupported RAW session schema {schema}"),
    }
}

fn raw_schema_audio_declaration(schema: &str) -> Option<bool> {
    match schema {
        RAW_SESSION_SCHEMA_V3 => Some(true),
        RAW_SESSION_SCHEMA_V1 | RAW_SESSION_SCHEMA_V2 => Some(false),
        RAW_INGEST_V2_SESSION_SCHEMA => None,
        _ => Some(false),
    }
}

/// 校验 v2 会话的目录快照 + manifest，并返回快照派生的视角换算常数
/// （快照字节已对 session.json 声明的 content hash 复核，常数因此可信）。
///
/// 断代后（bundle-2/3）走四件套 + 单册 recording profile；bundle-1 遗留世代
/// （issue #811 路线 A）走五件套 + capture/settings 两册，哈希对账纪律一条不少。
fn validate_v2_catalog_snapshots(
    session_dir: &Path,
    session: &RawSession,
    profile: &CaptureProfile,
    mapping: &MappingSnapshot,
    compliance: &ComplianceProfile,
) -> anyhow::Result<Option<ViewScaleConstants>> {
    let catalog = session
        .catalog
        .as_ref()
        .context("catalog-backed session requires catalog identity in session.json")?;
    match catalog.shape()? {
        RawCatalogShape::Merged { recording } => validate_merged_catalog_snapshots(
            session_dir,
            session,
            catalog,
            recording,
            profile,
            mapping,
            compliance,
        ),
        RawCatalogShape::LegacyCaptureSettings { capture, settings } => {
            validate_legacy_bundle1_catalog_snapshots(
                session_dir,
                session,
                catalog,
                capture,
                settings,
                profile,
                mapping,
                compliance,
            )
        }
    }
}

fn validate_merged_catalog_snapshots(
    session_dir: &Path,
    session: &RawSession,
    catalog: &RawCatalogIdentity,
    catalog_recording: &RawCatalogEntryIdentity,
    profile: &CaptureProfile,
    mapping: &MappingSnapshot,
    compliance: &ComplianceProfile,
) -> anyhow::Result<Option<ViewScaleConstants>> {
    let neutral = is_neutral_ingest_session(&session.schema_version);
    let expected_mode = if neutral {
        None
    } else if compliance.is_release() {
        Some(CATALOG_MODE_RELEASE)
    } else {
        Some(CATALOG_MODE_DEV)
    };
    ensure!(
        catalog.mode.as_deref() == expected_mode,
        "session catalog mode does not match its session generation"
    );
    validate_data_model_catalog(data_model_catalog(Some(catalog)).as_ref())?;
    for (label, identity) in [
        ("device", &catalog.device),
        ("recording", catalog_recording),
        ("game", &catalog.game),
        ("mapping", &catalog.mapping),
    ] {
        ensure_catalog_identity(identity, label)?;
    }
    ensure!(
        !catalog.catalog_id.trim().is_empty()
            && !catalog.catalog_version.trim().is_empty()
            && valid_sha256(&catalog.catalog_content_hash),
        "v2 session catalog identity is incomplete"
    );

    let catalog_bytes =
        read_exact_catalog_snapshot(session_dir, "catalog.json", &catalog.catalog_content_hash)?;
    let device_bytes = read_exact_catalog_snapshot(
        session_dir,
        "device-profile.json",
        &catalog.device.content_hash,
    )?;
    let recording_bytes = read_exact_catalog_snapshot(
        session_dir,
        "recording-profile.json",
        &catalog_recording.content_hash,
    )?;
    let game_bytes =
        read_exact_catalog_snapshot(session_dir, "game-profile.json", &catalog.game.content_hash)?;
    let mapping_bytes =
        read_exact_catalog_snapshot(session_dir, "mapping.json", &catalog.mapping.content_hash)?;

    let manifest: RawCatalogManifest = serde_json::from_slice(&catalog_bytes)
        .context("invalid v2 catalog snapshot catalog.json")?;
    // All Games DEV uses the same current bundle-3 snapshots as the delivered
    // client while retaining its explicitly non-trainable local session schema.
    // Snapshot generation comes from those hash-verified bytes, not an invented
    // bundle-2 reclassification of the source. This compatibility is local-only.
    let local_bundle3 = !neutral
        && matches!(compliance, ComplianceProfile::LocalDebugNonTrainable)
        && catalog.mode.as_deref() == Some(CATALOG_MODE_DEV)
        && session.non_trainable == Some(true)
        && session.production_certified == Some(false)
        && manifest.schema_version == "dm-catalog-bundle-3";
    let neutral_catalog = neutral || local_bundle3;
    let expected_non_trainable = if neutral_catalog {
        None
    } else {
        Some(!compliance.is_release())
    };
    let expected_manifest_schema = if neutral_catalog {
        "dm-catalog-bundle-3"
    } else {
        "dm-catalog-bundle-2"
    };
    ensure!(
        manifest.schema_version == expected_manifest_schema
            && manifest.catalog_id == catalog.catalog_id
            && manifest.catalog_version == catalog.catalog_version
            && manifest.non_trainable == expected_non_trainable,
        "catalog.json identity or classification does not match session.json"
    );
    ensure_manifest_selects(&manifest.device_profiles, &catalog.device, "device")?;
    ensure_manifest_selects(&manifest.recording_profiles, catalog_recording, "recording")?;
    ensure_manifest_selects(&manifest.game_profiles, &catalog.game, "game")?;
    ensure_manifest_selects(&manifest.mappings, &catalog.mapping, "mapping")?;

    let device: RawDeviceProfileIdentity = serde_json::from_slice(&device_bytes)
        .context("invalid v2 catalog snapshot device-profile.json")?;
    let expected_device_schema = if neutral_catalog {
        "dm-device-profile-3"
    } else {
        "dm-device-profile-2"
    };
    ensure!(
        device.schema_version == expected_device_schema
            && device.profile_id == catalog.device.id
            && device.version == catalog.device.version
            && device.non_trainable == expected_non_trainable,
        "device-profile.json identity or classification does not match session.json"
    );
    // `profile` 是本会话真正用来解码的那份引擎投影（见 `validate_session_header`），
    // 所以这里核的是「投影的来源就是 session.json 钉的那份 recording profile」。
    ensure!(
        profile.profile_id == catalog_recording.id
            && profile.non_trainable == expected_non_trainable.unwrap_or(true),
        "the engine profile in use does not match the recording profile session.json pins"
    );
    let game: RawGameProfileIdentity = serde_json::from_slice(&game_bytes)
        .context("invalid v2 catalog snapshot game-profile.json")?;
    let expected_game_schema = if neutral_catalog {
        "dm-game-profile-3"
    } else {
        "dm-game-profile-2"
    };
    ensure!(
        game.schema_version == expected_game_schema
            && game.game_id == catalog.game.id
            && game.version == catalog.game.version
            && game.non_trainable == expected_non_trainable
            && game.game_id == session.game.id
            && game.device_profile_id == catalog.device.id
            && game.recording_profile_id == catalog_recording.id
            && game.mapping_id == catalog.mapping.id,
        "game-profile.json identity or cross-references do not match session.json"
    );
    let recording: RawRecordingProfile = serde_json::from_slice(&recording_bytes)
        .context("invalid v2 catalog snapshot recording-profile.json")?;
    let expected_recording_profile_schema = if neutral_catalog {
        "dm-recording-profile-2"
    } else {
        "dm-recording-profile-1"
    };
    ensure!(
        recording.schema_version == expected_recording_profile_schema
            && recording.profile_id == catalog_recording.id
            && recording.version == catalog_recording.version
            && recording.non_trainable == expected_non_trainable
            && recording.game_id == catalog.game.id,
        "recording-profile.json identity or cross-references do not match session.json"
    );
    ensure!(
        sha256_bytes(&mapping_bytes) == catalog.mapping.content_hash
            && mapping.mapping().mapping_id == catalog.mapping.id
            && mapping.mapping().version == catalog.mapping.version
            && mapping.mapping().game.id == catalog.game.id
            && mapping.mapping().recording_profile_id == catalog_recording.id,
        "mapping.json identity or cross-references do not match session.json"
    );
    ensure!(
        session.mapping.id == catalog.mapping.id
            && session.mapping.version == catalog.mapping.version
            && session.mapping.content_hash == catalog.mapping.content_hash
            && session.mapping.recording_profile_id.as_deref()
                == Some(catalog_recording.id.as_str()),
        "session.json mapping identity does not match its v2 catalog selection"
    );
    view_scale_constants(&recording)
}

/// bundle-1 遗留世代的目录快照五件套（issue #811 路线 A，恢复自 docs/57 断代前的
/// 校验，a0dd7db6^）。只有 local-debug 与 attested-trainable 走得到这里
/// （`validate_session_header` 的世代门），所以本档恒为 non-trainable 口径；
/// 逐字节 SHA-256 对账、manifest 选中性、交叉引用与当代同纪律，一条不放松。
#[allow(clippy::too_many_arguments)]
fn validate_legacy_bundle1_catalog_snapshots(
    session_dir: &Path,
    session: &RawSession,
    catalog: &RawCatalogIdentity,
    catalog_capture: &RawCatalogEntryIdentity,
    catalog_settings: &RawCatalogEntryIdentity,
    profile: &CaptureProfile,
    mapping: &MappingSnapshot,
    compliance: &ComplianceProfile,
) -> anyhow::Result<Option<ViewScaleConstants>> {
    ensure!(
        !compliance.is_release() && !is_neutral_ingest_session(&session.schema_version),
        "bundle-1 catalog generation is only readable under local-debug or attested-trainable"
    );
    ensure!(
        catalog.mode.as_deref() == Some(CATALOG_MODE_DEV),
        "session catalog mode does not match its session generation"
    );
    validate_data_model_catalog(data_model_catalog(Some(catalog)).as_ref())?;
    for (label, identity) in [
        ("device", &catalog.device),
        ("capture", catalog_capture),
        ("settings", catalog_settings),
        ("game", &catalog.game),
        ("mapping", &catalog.mapping),
    ] {
        ensure_catalog_identity(identity, label)?;
    }
    ensure!(
        !catalog.catalog_id.trim().is_empty()
            && !catalog.catalog_version.trim().is_empty()
            && valid_sha256(&catalog.catalog_content_hash),
        "v2 session catalog identity is incomplete"
    );

    let catalog_bytes =
        read_exact_catalog_snapshot(session_dir, "catalog.json", &catalog.catalog_content_hash)?;
    let device_bytes = read_exact_catalog_snapshot(
        session_dir,
        "device-profile.json",
        &catalog.device.content_hash,
    )?;
    // 引擎投影已在 `validate_session_header` 从 capture-profile.json 读出；这里
    // 补上「那份字节就是 session.json 钉的 capture 快照」的逐字节复核。
    let _capture_bytes = read_exact_catalog_snapshot(
        session_dir,
        "capture-profile.json",
        &catalog_capture.content_hash,
    )?;
    let settings_bytes = read_exact_catalog_snapshot(
        session_dir,
        "settings-profile.json",
        &catalog_settings.content_hash,
    )?;
    let game_bytes =
        read_exact_catalog_snapshot(session_dir, "game-profile.json", &catalog.game.content_hash)?;
    let mapping_bytes =
        read_exact_catalog_snapshot(session_dir, "mapping.json", &catalog.mapping.content_hash)?;

    let manifest: RawLegacyCatalogManifestV1 = serde_json::from_slice(&catalog_bytes)
        .context("invalid bundle-1 catalog snapshot catalog.json")?;
    ensure!(
        manifest.schema_version == "dm-catalog-bundle-1"
            && manifest.catalog_id == catalog.catalog_id
            && manifest.catalog_version == catalog.catalog_version
            && manifest.catalog_mode == CATALOG_MODE_DEV
            && manifest.non_trainable,
        "catalog.json identity or classification does not match session.json"
    );
    ensure_manifest_selects(&manifest.device_profiles, &catalog.device, "device")?;
    ensure_manifest_selects(&manifest.capture_profiles, catalog_capture, "capture")?;
    ensure_manifest_selects(&manifest.settings_profiles, catalog_settings, "settings")?;
    ensure_manifest_selects(&manifest.game_profiles, &catalog.game, "game")?;
    ensure_manifest_selects(&manifest.mappings, &catalog.mapping, "mapping")?;

    let device: RawDeviceProfileIdentity = serde_json::from_slice(&device_bytes)
        .context("invalid bundle-1 catalog snapshot device-profile.json")?;
    // 封存车道里 device profile 横跨 -1 与 -2 两个 schema（07-31 音频代在断代
    // 合入前就把 device 升到了 -2，R2 实测两种都在）；两者身份字段同形。
    ensure!(
        matches!(
            device.schema_version.as_str(),
            "dm-device-profile-1" | "dm-device-profile-2"
        ) && device.profile_id == catalog.device.id
            && device.version == catalog.device.version
            && device.non_trainable == Some(true),
        "device-profile.json identity or classification does not match session.json"
    );
    ensure!(
        profile.profile_id == catalog_capture.id && profile.non_trainable,
        "the engine profile in use does not match the capture profile session.json pins"
    );
    let game: RawLegacyGameProfileIdentity = serde_json::from_slice(&game_bytes)
        .context("invalid bundle-1 catalog snapshot game-profile.json")?;
    ensure!(
        game.schema_version == "dm-game-profile-1"
            && game.game_id == catalog.game.id
            && game.version == catalog.game.version
            && game.non_trainable
            && game.game_id == session.game.id
            && game.device_profile_id == catalog.device.id
            && game.capture_profile_id == catalog_capture.id
            && game.settings_profile_id == catalog_settings.id
            && game.mapping_id == catalog.mapping.id
            && game.mapping_version == catalog.mapping.version,
        "game-profile.json identity or cross-references do not match session.json"
    );
    let settings: RawLegacySettingsProfile = serde_json::from_slice(&settings_bytes)
        .context("invalid bundle-1 catalog snapshot settings-profile.json")?;
    ensure!(
        settings.schema_version == "dm-settings-profile-1"
            && settings.profile_id == catalog_settings.id
            && settings.version == catalog_settings.version
            && settings.non_trainable
            && settings.game_id == catalog.game.id
            && settings.capture_profile_id == catalog_capture.id,
        "settings-profile.json identity or cross-references do not match session.json"
    );
    // legacy mapping 规范化后 `recording_profile_id` 承载的就是 settings 引用
    // （dm-mapping-1 的 settings_profile_id，见 `MappingSnapshot::from_legacy_v1_slice`）。
    ensure!(
        sha256_bytes(&mapping_bytes) == catalog.mapping.content_hash
            && mapping.mapping().mapping_id == catalog.mapping.id
            && mapping.mapping().version == catalog.mapping.version
            && mapping.mapping().game.id == catalog.game.id
            && mapping.mapping().recording_profile_id == catalog_settings.id,
        "mapping.json identity or cross-references do not match session.json"
    );
    ensure!(
        session.mapping.id == catalog.mapping.id
            && session.mapping.version == catalog.mapping.version
            && session.mapping.content_hash == catalog.mapping.content_hash
            && session.mapping.settings_profile_id.as_deref() == Some(catalog_settings.id.as_str()),
        "session.json mapping identity does not match its v2 catalog selection"
    );
    // bundle-1 世代 view_scale 块住在 settings profile（docs/45 §2.2 早于合册）；
    // COD 封存车道整层未标定，这里照旧走"不猜、不补默认值"的判定。
    view_scale_constants_in("settings-profile.json", settings.view_scale.as_ref())
}

fn ensure_catalog_identity(identity: &RawCatalogEntryIdentity, label: &str) -> anyhow::Result<()> {
    ensure!(
        !identity.id.trim().is_empty()
            && !identity.version.trim().is_empty()
            && valid_sha256(&identity.content_hash),
        "v2 {label} catalog identity is incomplete"
    );
    Ok(())
}

fn ensure_manifest_selects(
    references: &[RawCatalogEntryIdentity],
    expected: &RawCatalogEntryIdentity,
    label: &str,
) -> anyhow::Result<()> {
    let matches = references
        .iter()
        .filter(|reference| reference.id == expected.id && reference.version == expected.version)
        .collect::<Vec<_>>();
    ensure!(
        matches.len() == 1 && matches[0].content_hash == expected.content_hash,
        "catalog.json {label} identity does not match session.json"
    );
    Ok(())
}

fn read_exact_catalog_snapshot(
    session_dir: &Path,
    name: &str,
    expected_hash: &str,
) -> anyhow::Result<Vec<u8>> {
    let path = session_dir.join(name);
    ensure!(path.is_file(), "v2 catalog snapshot is missing {name}");
    let bytes = fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?;
    ensure!(
        sha256_bytes(&bytes) == expected_hash,
        "catalog snapshot {name} exact-byte SHA-256 mismatch"
    );
    Ok(bytes)
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 71
        && value.starts_with("sha256:")
        && value[7..].bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn ensure_contract(contract: &RawContract) -> anyhow::Result<()> {
    ensure!(
        contract.version == RAW_CONTRACT_VERSION,
        "unsupported dm contract"
    );
    ensure!(contract.pointer_clamp == 200, "pointer clamp mismatch");
    ensure!(contract.pointer_mu == 8.0, "pointer mu mismatch");
    ensure!(contract.pointer_ref == 200, "pointer reference mismatch");
    ensure!(contract.pointer_max_bin == 10, "pointer max bin mismatch");
    Ok(())
}

fn ensure_safe_component(value: &str, label: &str) -> anyhow::Result<()> {
    let mut components = Path::new(value).components();
    let is_single_normal = matches!(
        components.next(),
        Some(Component::Normal(component)) if component.to_str() == Some(value)
    ) && components.next().is_none();
    ensure!(
        is_single_normal
            && !value.is_empty()
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')),
        "{label} is not a safe path component"
    );
    Ok(())
}

fn ensure_output_outside_source(session_dir: &Path, output: &Path) -> anyhow::Result<()> {
    let source = session_dir
        .canonicalize()
        .with_context(|| format!("failed to resolve RAW session {}", session_dir.display()))?;
    let output_absolute = std::path::absolute(output)
        .with_context(|| format!("failed to resolve output {}", output.display()))?;
    ensure!(
        !output_absolute.starts_with(&source) && !source.starts_with(&output_absolute),
        "output and the read-only RAW session must not overlap"
    );
    match (output.parent(), output.file_name()) {
        (Some(parent), Some(name)) if parent.exists() => {
            let resolved = parent.canonicalize()?.join(name);
            ensure!(
                !resolved.starts_with(&source) && !source.starts_with(&resolved),
                "output resolves to a path overlapping the read-only RAW session"
            );
        }
        _ => {}
    }
    Ok(())
}

fn validate_capture_policy(
    policy: &RawCapturePolicy,
    profile: &CaptureProfile,
) -> anyhow::Result<()> {
    ensure!(
        policy.source == "game_window_only",
        "session source is not game-window-only"
    );
    let source_policy_matches = match (policy.minimum_source_width, policy.minimum_source_height) {
        (None, None) => !policy.scale,
        (Some(width), Some(height)) => {
            (width, height) == profile.source_geometry_rule().dimensions()
                && policy.scale == profile.allows_source_scaling()
        }
        _ => false,
    };
    ensure!(
        source_policy_matches,
        "session source geometry policy is invalid"
    );
    let expected_audio = match &profile.audio {
        Some(audio) => serde_json::to_value(audio)?,
        None => serde_json::Value::Bool(false),
    };
    ensure!(
        policy.audio == expected_audio,
        "session capture policy audio does not match capture-profile.json"
    );
    ensure!(
        policy.width == profile.width && policy.height == profile.height,
        "session/profile dimensions mismatch"
    );
    ensure!(
        policy.fps_num == profile.fps_num && policy.fps_den == profile.fps_den,
        "session/profile fps mismatch"
    );
    ensure!(
        policy.gop_frames == profile.gop_frames,
        "session/profile GOP mismatch"
    );
    Ok(())
}

fn validate_probe(probe: &RawProbe, meta: &RawSegmentMeta) -> anyhow::Result<()> {
    ensure!(probe.codec == "H.264", "segment probe codec mismatch");
    ensure!(
        probe.h264_profile == 100,
        "segment is not H.264 High profile"
    );
    ensure!(
        probe.width == meta.width && probe.height == meta.height,
        "probe dimensions mismatch"
    );
    ensure!(
        probe.fps_num == meta.fps_num && probe.fps_den == meta.fps_den,
        "probe fps mismatch"
    );
    ensure!(
        probe.sample_count == meta.n_frames,
        "probe sample count mismatch"
    );
    ensure!(
        probe.first_sample_clean_point,
        "segment does not start with an IDR"
    );
    ensure!(probe.pts_monotonic, "segment PTS is not monotonic");
    Ok(())
}

fn validate_effective_encoder(
    encoder: &RawEffectiveEncoder,
    _profile: &CaptureProfile,
) -> anyhow::Result<()> {
    let has_identity = [
        encoder.transform_clsid.as_deref(),
        encoder.friendly_name.as_deref(),
        encoder.hardware_url.as_deref(),
    ]
    .into_iter()
    .flatten()
    .any(|value| !value.trim().is_empty());
    ensure!(has_identity, "segment encoder identity is missing");
    // Codec controls are driver preferences, not proof of capture ability.
    // The raw MP4 validation above already enforces the actual downstream
    // contract: H.264, expected geometry/rate/sample count, monotonic PTS and
    // a decodable IDR at the start of every segment. Do not reject otherwise
    // valid capture merely because a driver normalized bitrate/GOP/B-frame or
    // low-latency controls differently.
    Ok(())
}

fn validate_chunk_audio_files(
    chunk_dir: &Path,
    meta: &ChunkMeta,
    audio_enabled: bool,
) -> anyhow::Result<()> {
    let mut expected_files = BTreeSet::from([
        "meta.json".to_owned(),
        "p0.jsonl".to_owned(),
        "p0.mp4".to_owned(),
    ]);
    match (audio_enabled, &meta.audio) {
        (false, None) => {}
        (true, Some(audio)) => {
            ensure!(
                audio.source == "process_loopback"
                    && audio.sample_rate == AUDIO_SAMPLE_RATE_HZ
                    && audio.channels == AUDIO_CHANNELS
                    && audio.bits == AUDIO_BITS_PER_SAMPLE
                    && audio.sample_format == AUDIO_SAMPLE_FORMAT,
                "chunk audio format mismatch"
            );
            match audio.state {
                SegmentAudioState::Ready => {
                    ensure!(
                        audio.present
                            && audio.file.as_deref() == Some("p0.flac")
                            && audio.codec.as_deref() == Some("flac")
                            && audio.frames.is_some_and(|frames| frames > 0)
                            && audio.sha256.as_deref().is_some_and(is_sha256_identifier)
                            && audio.failure_codes.is_empty(),
                        "ready chunk audio metadata is incomplete"
                    );
                    let path = chunk_dir.join("p0.flac");
                    let expected_hash = audio
                        .sha256
                        .as_deref()
                        .context("ready chunk audio has no SHA-256")?;
                    ensure!(
                        sha256_file(&path)? == expected_hash,
                        "chunk FLAC hash mismatch"
                    );
                    verify_flac(
                        &path,
                        audio
                            .frames
                            .context("ready chunk audio has no frame count")?,
                        None,
                    )?;
                    expected_files.insert("p0.flac".into());
                }
                SegmentAudioState::Degraded | SegmentAudioState::Absent => {
                    ensure!(
                        !audio.present
                            && audio.file.is_none()
                            && audio.codec.is_none()
                            && audio.frames.is_none()
                            && audio.sha256.is_none()
                            && !audio.failure_codes.is_empty(),
                        "unpublishable chunk audio must omit FLAC and carry failure codes"
                    );
                }
            }
        }
        _ => anyhow::bail!("chunk audio generation does not match session metadata"),
    }
    let actual_files = fs::read_dir(chunk_dir)?
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_file())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect::<BTreeSet<_>>();
    ensure!(
        actual_files == expected_files,
        "chunk directory members do not match audio metadata"
    );
    Ok(())
}

fn validate_chunk_meta(
    meta: &ChunkMeta,
    session: &SessionMetadata,
    index: u32,
    native_format: NativeFormat,
    compliance: &ComplianceProfile,
) -> anyhow::Result<()> {
    validate_data_model_catalog(meta.catalog.as_ref())?;
    ensure!(
        meta.schema_version == session.schema_version,
        "chunk schema does not match session metadata"
    );
    ensure!(
        meta.session == session.workflow_id && meta.chunk == index,
        "chunk identity mismatch"
    );
    ensure!(
        meta.sample_key == format!("{}_c{index:04}", session.workflow_id),
        "chunk sample key mismatch"
    );
    ensure!(
        meta.game == session.game
            && meta.category == session.category
            && meta.platform == session.platform,
        "chunk catalog fields do not match session metadata"
    );
    ensure!(meta.video.file == "p0.mp4", "chunk video path mismatch");
    ensure!(meta.video.codec == "h264", "chunk codec mismatch");
    ensure!(
        meta.video.width == native_format.width
            && meta.video.height == native_format.height
            && meta.video.fps == native_format.fps()
            && meta.video.fps_num == native_format.fps_num
            && meta.video.fps_den == native_format.fps_den
            && !meta.video.audio,
        "chunk video does not preserve the native capture format"
    );
    let stamp = compliance.stamp();
    ensure!(
        meta.non_trainable == stamp.non_trainable
            && meta.is_trainable == stamp.is_trainable
            && meta.production_certified == stamp.production_certified,
        "chunk compliance flags do not match the compliance profile"
    );
    ensure!(
        meta.rights == session.rights
            && meta.first_party == session.first_party
            && meta.consent_id == session.consent_id
            && meta.attestation == session.attestation
            && meta.is_cutscene == session.is_cutscene,
        "chunk compliance fields do not match session metadata"
    );
    ensure!(
        meta.annotation_source == session.annotation.source && !meta.key_schema.trim().is_empty(),
        "chunk annotation contract mismatch"
    );
    ensure!(
        meta.catalog == session.catalog,
        "chunk catalog identity does not match session metadata"
    );
    ensure!(
        meta.provenance.raw_session_id == session.provenance.raw_session_id
            && meta.provenance.mapping_id == session.provenance.mapping_id
            && meta.provenance.mapping_version == session.provenance.mapping_version
            && meta.provenance.mapping_content_hash == session.provenance.mapping_content_hash,
        "chunk provenance identity mismatch"
    );
    let allowed_segments = session
        .provenance
        .source_segments
        .iter()
        .map(|segment| format!("{}/{:05}", segment.run_id, segment.segment_index))
        .collect::<BTreeSet<_>>();
    ensure!(
        !meta.provenance.raw_segments.is_empty()
            && meta
                .provenance
                .raw_segments
                .iter()
                .all(|segment| allowed_segments.contains(segment)),
        "chunk provenance references an unknown RAW segment"
    );
    Ok(())
}

fn ensure_compliance_metadata(
    metadata: &SessionMetadata,
    compliance: &ComplianceProfile,
) -> anyhow::Result<()> {
    validate_data_model_catalog(metadata.catalog.as_ref())?;
    let stamp = compliance.stamp();
    ensure!(
        matches!(
            metadata.schema_version.as_str(),
            LEGACY_DATA_MODEL_SCHEMA_VERSION | DATA_MODEL_SCHEMA_VERSION
        ),
        "unsupported dataset schema"
    );
    ensure_safe_component(&metadata.workflow_id, "workflow id")?;
    ensure!(
        metadata.non_trainable == stamp.non_trainable
            && metadata.is_trainable == stamp.is_trainable,
        "dataset trainability does not match the compliance profile"
    );
    ensure!(
        metadata.production_certified == stamp.production_certified,
        "dataset certification does not match the compliance profile"
    );
    ensure!(
        metadata.rights == stamp.rights
            && metadata.first_party == stamp.first_party
            && metadata.consent_id == stamp.consent_id
            && !metadata.is_cutscene,
        "dataset compliance fields do not match the compliance profile"
    );
    // 与 consent id 同一纪律：verify attested 数据集必须显式给出 --waiver-ref，
    // 且与印章逐字一致——操作者被强制对照 WAIVER 登记。
    ensure!(
        metadata.attestation == stamp.attestation,
        "dataset attestation waiver ref does not match the compliance profile"
    );
    let native_format = NativeFormat::from_capture(&metadata.capture)?;
    ensure!(
        metadata.capture.codec == "h264",
        "dataset capture codec mismatch"
    );
    ensure!(
        metadata.capture.frame_count > 0
            && metadata.capture.chunks
                == u32::try_from(
                    metadata
                        .capture
                        .frame_count
                        .div_ceil(native_format.chunk_frames as u64),
                )?,
        "dataset chunk count does not close over frame count"
    );
    ensure!(
        metadata.duration_ms == native_format.video_time_ms(metadata.capture.frame_count),
        "dataset duration does not match frame count"
    );
    ensure!(
        metadata.frame_pairs.gap_h == FRAMEPAIR_GAP
            && metadata.frame_pairs.stride == FRAMEPAIR_STRIDE
            && metadata.frame_pairs.overlapping,
        "dataset frame-pair contract mismatch"
    );
    ensure!(
        metadata.annotation.source == stamp.annotation_source
            && metadata.source_video == format!("ready-session:{}", metadata.workflow_id)
            && supported_raw_session_schema(&metadata.provenance.raw_schema_version)
            && metadata.provenance.raw_session_id == metadata.workflow_id,
        "dataset source provenance mismatch"
    );
    if let Some(expected_audio) =
        raw_schema_audio_declaration(&metadata.provenance.raw_schema_version)
    {
        ensure!(
            expected_audio == metadata.audio.is_some(),
            "dataset audio declaration does not match RAW session generation"
        );
    }
    if let Some(audio) = &metadata.audio {
        ensure!(
            audio.source == "process_loopback"
                && audio.codec == "flac"
                && audio.sample_rate == AUDIO_SAMPLE_RATE_HZ
                && audio.channels == AUDIO_CHANNELS
                && audio.bits == AUDIO_BITS_PER_SAMPLE
                && audio.sample_format == AUDIO_SAMPLE_FORMAT
                && audio.ready_chunks + audio.degraded_chunks + audio.absent_chunks
                    == metadata.capture.chunks
                && audio.all_publishable
                    == (audio.degraded_chunks == 0 && audio.absent_chunks == 0),
            "dataset audio summary is invalid"
        );
    }
    ensure!(
        metadata
            .tags
            .iter()
            .map(String::as_str)
            .eq(stamp.tags.iter().copied()),
        "dataset tags mismatch"
    );
    ensure!(
        !metadata.provenance.source_segments.is_empty(),
        "dataset has no RAW source provenance"
    );
    let base_hash_names = BTreeSet::from([
        "events.jsonl.zst".to_owned(),
        "frames.jsonl.zst".to_owned(),
        "meta.json".to_owned(),
        "video.mp4".to_owned(),
    ]);
    let mut source_ids = BTreeSet::new();
    for segment in &metadata.provenance.source_segments {
        ensure_safe_component(&segment.run_id, "provenance run id")?;
        ensure!(
            source_ids.insert((segment.run_id.clone(), segment.segment_index)),
            "dataset repeats a RAW source segment"
        );
        let names = segment.hashes.keys().cloned().collect::<BTreeSet<_>>();
        let valid_names = if metadata.audio.is_some() {
            names == base_hash_names
                || names
                    == base_hash_names
                        .iter()
                        .cloned()
                        .chain([AUDIO_ARTIFACT_NAME.to_owned()])
                        .collect()
        } else {
            names == base_hash_names
        };
        ensure!(
            valid_names
                && segment
                    .hashes
                    .values()
                    .all(|hash| is_sha256_identifier(hash)),
            "dataset RAW source hashes are incomplete"
        );
    }
    Ok(())
}

fn validate_stats(
    stats: &DatasetStats,
    metadata: &SessionMetadata,
    compliance: &ComplianceProfile,
) -> anyhow::Result<()> {
    ensure!(stats.sessions == 1, "dataset stats session count mismatch");
    ensure!(
        stats.games == [metadata.game.clone()],
        "dataset stats games mismatch"
    );
    ensure!(
        stats.total_frames == metadata.capture.frame_count,
        "dataset stats frame count mismatch"
    );
    ensure!(
        stats.total_chunks == metadata.capture.chunks,
        "dataset stats chunk count mismatch"
    );
    ensure!(
        stats.total_frame_pairs == metadata.frame_pairs.count,
        "dataset stats framepair count mismatch"
    );
    ensure!(
        stats.total_hours
            == (metadata.duration_ms as f64 / 3_600_000.0 * 1_000.0).round() / 1_000.0,
        "dataset stats duration mismatch"
    );
    ensure!(
        stats.datacard.fps == metadata.capture.fps
            && stats.datacard.fps_num == metadata.capture.fps_num
            && stats.datacard.fps_den == metadata.capture.fps_den
            && stats.datacard.resolution
                == format!("{}x{}", metadata.capture.width, metadata.capture.height)
            && stats.datacard.carrier == "h264_mp4_chunks"
            && stats.datacard.chunk_s == 4
            && stats.datacard.gap_h == FRAMEPAIR_GAP
            && stats.datacard.stride == FRAMEPAIR_STRIDE
            && stats.datacard.overlapping
            && stats.datacard.merged_jsonl,
        "dataset stats datacard mismatch"
    );
    // Aggregation disclosures (docs/148 A1): empty means the artifact predates
    // the disclosure; any non-empty value must be the canonical constant, so a
    // card can never carry a stale or hand-edited semantics claim.
    for (value, expected, field) in [
        (
            &stats.datacard.disclosures.window_rule,
            AGG_WINDOW_RULE,
            "window_rule",
        ),
        (
            &stats.datacard.disclosures.key_semantics,
            AGG_KEY_SEMANTICS,
            "key_semantics",
        ),
        (
            &stats.datacard.disclosures.button_semantics,
            AGG_BUTTON_SEMANTICS,
            "button_semantics",
        ),
        (
            &stats.datacard.disclosures.pointer_aggregation,
            AGG_POINTER_AGGREGATION,
            "pointer_aggregation",
        ),
        (
            &stats.datacard.disclosures.mulaw_scheme,
            AGG_MULAW_SCHEME,
            "mulaw_scheme",
        ),
        (
            &stats.datacard.disclosures.downsample_rule,
            AGG_DOWNSAMPLE_RULE,
            "downsample_rule",
        ),
        (
            &stats.datacard.disclosures.duplicate_semantics,
            AGG_DUPLICATE_SEMANTICS,
            "duplicate_semantics",
        ),
    ] {
        ensure!(
            value.is_empty() || value == expected,
            "dataset stats aggregation disclosure `{field}` is neither empty nor the canonical value"
        );
    }
    let stamp = compliance.stamp();
    ensure!(
        stats.compliance.all_non_trainable == stamp.non_trainable,
        "dataset stats trainability boundary does not match the compliance profile"
    );
    ensure!(
        stats.compliance.trainable_sessions == u32::from(stamp.is_trainable),
        "dataset stats trainable session count does not match the compliance profile"
    );
    ensure!(
        stats.compliance.production_certified_sessions == u32::from(stamp.production_certified),
        "dataset stats certified session count does not match the compliance profile"
    );
    ensure!(
        stats.compliance.attestation == stamp.attestation,
        "dataset stats attestation waiver ref does not match the compliance profile"
    );
    Ok(())
}

fn is_sha256_identifier(value: &str) -> bool {
    value.len() == 71
        && value.starts_with("sha256:")
        && value[7..].bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn same_codec_config(left: &VideoDescriptor, right: &VideoDescriptor) -> bool {
    left.movie_timescale == right.movie_timescale
        && left.track_timescale == right.track_timescale
        && left.language == right.language
        && left.width == right.width
        && left.height == right.height
        && left.sequence_parameter_set == right.sequence_parameter_set
        && left.picture_parameter_set == right.picture_parameter_set
}

fn frame_timing(row: &RawFrameProjection) -> FrameTiming {
    FrameTiming {
        frame: row.frame,
        session_frame: row.session_frame,
        output_qpc_start: row.output_qpc_start,
        output_qpc_end: row.output_qpc_end,
        source_qpc: row.source_qpc,
        source_seq: row.source_seq,
        present_seq: row.present_seq,
        duplicate: row.duplicate,
        source_superseded: row.source_superseded,
        queue_drop: row.queue_drop,
    }
}

fn read_json<T: DeserializeOwned>(path: &Path) -> anyhow::Result<T> {
    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("invalid JSON {}", path.display()))
}

fn read_jsonl<T: DeserializeOwned>(path: &Path) -> anyhow::Result<Vec<T>> {
    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    parse_jsonl(&bytes, path)
}

fn read_zstd_jsonl<T: DeserializeOwned>(path: &Path) -> anyhow::Result<Vec<T>> {
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let bytes = zstd::decode_all(BufReader::new(file))
        .with_context(|| format!("invalid zstd stream {}", path.display()))?;
    parse_jsonl(&bytes, path)
}

fn parse_jsonl<T: DeserializeOwned>(bytes: &[u8], path: &Path) -> anyhow::Result<Vec<T>> {
    if !bytes.is_empty() {
        ensure!(
            bytes.ends_with(b"\n"),
            "JSONL must end with a newline: {}",
            path.display()
        );
    }
    let text = std::str::from_utf8(bytes)
        .with_context(|| format!("JSONL is not UTF-8: {}", path.display()))?;
    text.lines()
        .enumerate()
        .map(|(index, line)| {
            ensure!(!line.is_empty(), "JSONL contains an empty row");
            serde_json::from_str(line)
                .with_context(|| format!("invalid JSONL row {} in {}", index + 1, path.display()))
        })
        .collect()
}

fn write_json(path: &Path, value: &impl Serialize) -> anyhow::Result<()> {
    let mut file = BufWriter::new(File::create(path)?);
    serde_json::to_writer_pretty(&mut file, value)?;
    file.write_all(b"\n")?;
    file.flush()?;
    Ok(())
}

fn write_jsonl(path: &Path, rows: &[impl Serialize]) -> anyhow::Result<()> {
    let mut file = BufWriter::new(File::create(path)?);
    for row in rows {
        serde_json::to_writer(&mut file, row)?;
        file.write_all(b"\n")?;
    }
    file.flush()?;
    Ok(())
}

fn sha256_file(path: &Path) -> anyhow::Result<String> {
    let mut reader = BufReader::new(
        File::open(path).with_context(|| format!("failed to open {}", path.display()))?,
    );
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("sha256:{:x}", hasher.finalize()))
}

fn sha256_bytes(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn sorted_directories(root: &Path, prefix: &str) -> anyhow::Result<Vec<PathBuf>> {
    ensure!(root.is_dir(), "directory is missing: {}", root.display());
    let mut directories = fs::read_dir(root)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();
    directories.sort();
    for path in &directories {
        let name = path
            .file_name()
            .and_then(|value| value.to_str())
            .context("non-UTF-8 directory name")?;
        ensure!(
            name.starts_with(prefix)
                && name[prefix.len()..]
                    .bytes()
                    .all(|byte| byte.is_ascii_digit()),
            "unexpected directory {}",
            path.display()
        );
    }
    Ok(directories)
}

fn sorted_numeric_directories(root: &Path) -> anyhow::Result<Vec<PathBuf>> {
    ensure!(
        root.is_dir(),
        "segments directory is missing: {}",
        root.display()
    );
    let mut directories = fs::read_dir(root)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();
    directories.sort();
    for (index, path) in directories.iter().enumerate() {
        ensure!(
            numeric_name(path)? == index as u32,
            "segment directories are not contiguous"
        );
    }
    Ok(directories)
}

fn numeric_name(path: &Path) -> anyhow::Result<u32> {
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .context("non-UTF-8 numeric directory")?;
    ensure!(
        !name.is_empty() && name.bytes().all(|byte| byte.is_ascii_digit()),
        "invalid numeric directory {}",
        path.display()
    );
    name.parse().context("numeric directory index overflow")
}

fn find_dataset_session(dataset: &Path, session_id: &str) -> anyhow::Result<PathBuf> {
    let mut matches = Vec::new();
    for game in fs::read_dir(dataset)? {
        let game = game?.path();
        if game.is_dir() {
            let candidate = game.join(session_id);
            if candidate.is_dir() {
                matches.push(candidate);
            }
        }
    }
    ensure!(
        matches.len() == 1,
        "dataset session directory is missing or ambiguous"
    );
    Ok(matches.remove(0))
}

fn validate_dataset_tree(dataset: &Path, session_dir: &Path) -> anyhow::Result<()> {
    let game_dirs = fs::read_dir(dataset)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();
    ensure!(
        game_dirs.len() == 1,
        "converter dataset must contain exactly one game directory"
    );
    let session_dirs = fs::read_dir(&game_dirs[0])?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();
    ensure!(
        session_dirs.len() == 1 && session_dirs[0] == session_dir,
        "converter dataset must contain exactly one session directory"
    );
    Ok(())
}

fn dataset_chunk_directories(session_dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut directories = fs::read_dir(session_dir)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();
    directories.sort();
    for (index, directory) in directories.iter().enumerate() {
        ensure!(
            directory.file_name().and_then(|value| value.to_str())
                == Some(format!("c{index:04}").as_str()),
            "chunk directories are not contiguous"
        );
    }
    ensure!(!directories.is_empty(), "dataset has no chunks");
    Ok(directories)
}

fn slug(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect::<String>()
        .trim_matches('_')
        .to_owned()
}

fn is_sorted_unique<T: Ord>(values: &[T]) -> bool {
    values.windows(2).all(|window| window[0] < window[1])
}

fn valid_action_semantic(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._-".contains(&byte)
        })
}

fn is_labeled_frame(row: &DataDemoFrame) -> bool {
    !row.keys.is_empty() || !row.actions.is_empty() || row.pointer_dx != 0 || row.pointer_dy != 0
}

struct StagingGuard {
    path: PathBuf,
    armed: bool,
}

impl StagingGuard {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for StagingGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs::OpenOptions;

    use capture_core::audio_contract::{AudioPcmFormat, AudioSource, SegmentAudioHealth};
    use capture_core::mapping::MappingSnapshot;
    use capture_core::profile::CaptureAudioProfile;
    use capture_core::projection::{FrameProjector, FrameTiming};
    use mp4::{AvcConfig, MediaConfig, Mp4Config, Mp4Sample, Mp4Writer, TrackConfig, TrackType};
    use serde::Serialize;
    use serde_json::json;
    use tempfile::TempDir;

    use super::*;

    /// 夹具里 mapping / game profile / catalog 三处必须指同一个 recording profile
    /// （断代后 mapping 的 `recording_profile_id` 就是它）。
    const RECORDING_PROFILE_ID: &str = "synthetic-fixture-recording-v1";

    const MAPPING: &[u8] = br#"{
  "schema_version": "dm-mapping-2",
  "mapping_id": "game.synthetic-fixture.pc",
  "version": "2026-07-23.1",
  "game": {"id": "synthetic-fixture", "name": "Synthetic Fixture", "platform": "pc"},
  "taxonomy": {"content": "sandbox", "view": "first_person", "material": "3d"},
  "recording_profile_id": "synthetic-fixture-recording-v1",
  "bindings": [
    {"device": "keyboard", "scan_code": 17, "extended": false, "semantic": "move.forward"},
    {"device": "mouse", "control": "delta_x", "semantic": "camera.yaw"},
    {"device": "mouse", "control": "delta_y", "semantic": "camera.pitch"}
  ]
}
"#;

    struct Fixture {
        _temp: TempDir,
        session: PathBuf,
        output: PathBuf,
    }

    #[derive(Clone)]
    struct SegmentSpec {
        frames: usize,
        sync_samples: Vec<u32>,
    }

    #[derive(Serialize)]
    struct GoldenSummary {
        frame_count: u64,
        chunk_count: u32,
        frame_pair_count: u64,
        metadata_keys: Vec<String>,
        chunk_meta_keys: Vec<String>,
        stats_keys: Vec<String>,
        schema_version: String,
        metadata_catalog: Option<DataModelCatalog>,
        chunk0_catalog: Option<DataModelCatalog>,
        non_trainable: bool,
        production_certified: bool,
        chunk0_source_segments: Vec<String>,
        chunk1_source_segments: Vec<String>,
        frame_pairs: Vec<FramePair>,
        rows: Vec<DataDemoFrame>,
    }

    #[test]
    fn scalable_capture_policy_requires_the_declared_development_floor() {
        let profile = CaptureProfile {
            profile_id: "scalable-policy-fixture-v1".into(),
            non_trainable: true,
            capture_backend: "native_wgc_window_v1".into(),
            encoder_backend: "mf_nvidia_hardware_v1".into(),
            width: TARGET_WIDTH,
            height: TARGET_HEIGHT,
            fps_num: TARGET_FPS,
            fps_den: 1,
            bitrate_bps: 1_000_000,
            max_bitrate_bps: 2_000_000,
            gop_frames: CHUNK_FRAMES as u32,
            segment_seconds: 10,
            test_segments: 1,
            accepted_encoder_relaxations: None,
            audio: None,
        };
        let (minimum_source_width, minimum_source_height) =
            profile.source_geometry_rule().dimensions();
        let policy = RawCapturePolicy {
            source: "game_window_only".into(),
            width: profile.width,
            height: profile.height,
            scale: true,
            minimum_source_width: Some(minimum_source_width),
            minimum_source_height: Some(minimum_source_height),
            fps_num: profile.fps_num,
            fps_den: profile.fps_den,
            gop_frames: profile.gop_frames,
            audio: serde_json::Value::Bool(false),
        };

        validate_capture_policy(&policy, &profile).unwrap();

        let mut incomplete = policy;
        incomplete.minimum_source_height = None;
        assert!(validate_capture_policy(&incomplete, &profile).is_err());
    }

    #[test]
    fn encoder_validation_uses_output_capability_not_exact_driver_controls() {
        let profile = CaptureProfile {
            profile_id: "capability-policy-fixture-v1".into(),
            non_trainable: true,
            capture_backend: "native_wgc_window_v1".into(),
            encoder_backend: "mf_nvidia_hardware_v1".into(),
            width: TARGET_WIDTH,
            height: TARGET_HEIGHT,
            fps_num: TARGET_FPS,
            fps_den: 1,
            bitrate_bps: 1_000_000,
            max_bitrate_bps: 2_000_000,
            gop_frames: CHUNK_FRAMES as u32,
            segment_seconds: 10,
            test_segments: 1,
            accepted_encoder_relaxations: None,
            audio: None,
        };
        let encoder: RawEffectiveEncoder = serde_json::from_value(serde_json::json!({
            "transform_clsid": "{fixture-hardware-encoder}",
            "friendly_name": null,
            "hardware_url": null,
            "control_policy": "hardware_capability_validated_best_effort_controls",
            "relaxed_controls": ["maximum bitrate normalized by driver"],
            "rate_control_mode": 99,
            "mean_bitrate_bps": 750000,
            "max_bitrate_bps": 0,
            "gop_frames": 300,
            "b_frames": 2,
            "low_latency": false
        }))
        .unwrap();

        validate_effective_encoder(&encoder, &profile).unwrap();
    }

    #[test]
    fn raw_recorded_before_buttons_existed_still_converts() {
        // #59 item 1 backward compatibility. RAW written before capture-core
        // emitted `buttons` has no such key, but re-projecting its events with
        // today's projector produces one. Without the absent-vs-empty
        // distinction the frames-vs-events equality check would reject every
        // archived session, which is the one thing the immutable-RAW design must
        // never do.
        let fixture = fixture(&[
            SegmentSpec {
                frames: 100,
                sync_samples: vec![0],
            },
            SegmentSpec {
                frames: 140,
                sync_samples: vec![0, 20],
            },
        ]);
        for index in 0..2 {
            let segment = fixture
                .session
                .join(format!("runs/r0000/segments/{index:05}"));
            let frames_path = segment.join("frames.jsonl.zst");
            // Strip the key the way an older recorder would have left it out.
            let mut frames: Vec<serde_json::Value> = read_zstd_jsonl(&frames_path).unwrap();
            for frame in &mut frames {
                frame.as_object_mut().unwrap().remove("buttons");
            }
            write_zstd_jsonl(&frames_path, &frames);
            let meta_path = segment.join("meta.json");
            let mut meta: serde_json::Value = read_json(&meta_path).unwrap();
            meta["frames"]["sha256"] = json!(sha256_file(&frames_path).unwrap());
            write_json(&meta_path, &meta).unwrap();
            rewrite_segment_manifest(&segment);
        }

        let report = convert_session(&fixture.session, &fixture.output).unwrap();
        assert_eq!(report.frame_count, 240);
        verify_dataset(&fixture.output).unwrap();

        // The absent field converts to an empty held set, not to a failure.
        let session_dir = fixture
            .output
            .join("synthetic_fixture")
            .join("fixture-session");
        let chunk0: Vec<DataDemoFrame> = read_jsonl(&session_dir.join("c0000/p0.jsonl")).unwrap();
        assert!(chunk0.iter().all(|frame| frame.buttons.is_empty()));
    }

    #[test]
    fn pointer_raw_sum_ignores_non_moves_and_does_not_clamp() {
        let event = |ty: &str, dx: Option<i32>, dy: Option<i32>| RawFrameEvent {
            event_type: ty.into(),
            vk: None,
            scan_code: None,
            flags: None,
            dx,
            dy,
            delta: None,
            horizontal: None,
        };
        let events = vec![
            event("mouse_move", Some(150), Some(-10)),
            // A fast flick in the same frame pushes the sum past the ±200 clamp
            // that pointerDx/Dy saturate at — the exact truncation #59 removes.
            event("mouse_move", Some(180), Some(-5)),
            // Non-move events contribute nothing, even if they carried dx/dy.
            event("mouse_down", None, None),
            event("key_down", None, None),
        ];
        assert_eq!(pointer_raw_sum(&events), (330, -15));
        // 330 is well beyond the clamp, so the clamped pointerDx would read 200
        // while the raw sum preserves the true angular velocity.
        assert!(pointer_raw_sum(&events).0 > 200);
        assert_eq!(pointer_raw_sum(&[]), (0, 0));
    }

    #[test]
    fn golden_ready_session_converts_across_segment_and_verifies() {
        let fixture = fixture(&[
            SegmentSpec {
                frames: 100,
                sync_samples: vec![0],
            },
            SegmentSpec {
                frames: 140,
                sync_samples: vec![0, 20],
            },
        ]);
        let converted = convert_session(&fixture.session, &fixture.output).unwrap();
        assert_eq!((converted.frame_count, converted.chunk_count), (240, 2));
        let verified = verify_dataset(&fixture.output).unwrap();
        assert_eq!(verified.frame_count, 240);

        let session_dir = fixture
            .output
            .join("synthetic_fixture")
            .join("fixture-session");
        let chunk0: Vec<DataDemoFrame> = read_jsonl(&session_dir.join("c0000/p0.jsonl")).unwrap();
        let chunk1: Vec<DataDemoFrame> = read_jsonl(&session_dir.join("c0001/p0.jsonl")).unwrap();
        let chunk0_meta: ChunkMeta = read_json(&session_dir.join("c0000/meta.json")).unwrap();
        let chunk1_meta: ChunkMeta = read_json(&session_dir.join("c0001/meta.json")).unwrap();
        let metadata: SessionMetadata = read_json(&session_dir.join("metadata.json")).unwrap();
        let stats: DatasetStats = read_json(&fixture.output.join("_dataset_stats.json")).unwrap();
        let pairs: Vec<FramePair> = read_jsonl(&session_dir.join("framepairs.jsonl")).unwrap();
        assert_eq!(chunk0[0].actions, vec!["move.forward"]);
        assert_eq!(
            chunk0[1].actions,
            vec!["camera.pitch", "camera.yaw", "move.forward"]
        );
        assert_eq!(pairs[0].action_t.actions, vec!["move.forward"]);
        // #59 item 3: the data card states what dm-1 does not carry. The three
        // flags are facts about the format; the latency stays null because nothing
        // in RAW measures it and the converter must not repeat another machine's
        // figure.
        let disclosures = &stats.datacard.disclosures;
        assert!(!disclosures.has_reward_channel);
        assert!(!disclosures.has_termination_channel);
        assert!(!disclosures.has_game_state_channel);
        assert_eq!(disclosures.input_render_latency_frames, None);
        // Serialized as an explicit null, not omitted: an absent key reads as
        // "no issue", a null reads as "not calibrated".
        let stats_json: serde_json::Value =
            read_json(&fixture.output.join("_dataset_stats.json")).unwrap();
        let card = &stats_json["datacard"]["disclosures"];
        assert!(card.get("input_render_latency_frames").is_some());
        assert!(card["input_render_latency_frames"].is_null());
        // The human-readable card must carry the same disclosure as the JSON.
        let datasheet_text = fs::read_to_string(fixture.output.join("DATASHEET.md")).unwrap();
        assert!(datasheet_text.contains("no reward channel"));
        assert!(datasheet_text.contains("not calibrated for this artifact"));
        // docs/148 A1: aggregation semantics are disclosed twice — as machine
        // fields and as a card section — and the two must ship together.
        assert_eq!(disclosures.window_rule, AGG_WINDOW_RULE);
        assert_eq!(disclosures.key_semantics, AGG_KEY_SEMANTICS);
        assert_eq!(disclosures.button_semantics, AGG_BUTTON_SEMANTICS);
        assert_eq!(disclosures.pointer_aggregation, AGG_POINTER_AGGREGATION);
        assert_eq!(disclosures.mulaw_scheme, AGG_MULAW_SCHEME);
        assert_eq!(disclosures.downsample_rule, AGG_DOWNSAMPLE_RULE);
        assert_eq!(disclosures.duplicate_semantics, AGG_DUPLICATE_SEMANTICS);
        assert!(datasheet_text.contains("## Action aggregation & downsampling"));
        assert!(datasheet_text.contains("held at window end"));
        assert!(datasheet_text.contains("summed over the window, then clamped once"));
        assert!(datasheet_text.contains("dm1-v2"));
        assert!(datasheet_text.contains("coverage ratio in [0,1]"));
        // #59 item 1: buttons carries through from RAW to the frame and to the
        // framepair action, with the same sorted/unique shape as keys.
        for frame in chunk0.iter().chain(chunk1.iter()) {
            assert!(is_sorted_unique(&frame.buttons));
        }
        for pair in &pairs {
            let frame = chunk0
                .iter()
                .chain(chunk1.iter())
                .find(|f| f.gframe == pair.frame_t)
                .expect("framepair frameT has a frame");
            assert_eq!(pair.action_t.buttons, frame.buttons);
        }
        // #59 item 2: every frame's untruncated pointer sum equals the sum of its
        // own mouse_move deltas, in both p0.jsonl and the framepair mirror.
        for frame in chunk0.iter().chain(chunk1.iter()) {
            let (dx, dy) = pointer_raw_sum(&frame.events);
            assert_eq!((frame.pointer_raw_dx, frame.pointer_raw_dy), (dx, dy));
        }
        for pair in &pairs {
            let frame = chunk0
                .iter()
                .chain(chunk1.iter())
                .find(|f| f.gframe == pair.frame_t)
                .expect("framepair frameT has a frame");
            assert_eq!(
                (pair.action_t.pointer_raw_dx, pair.action_t.pointer_raw_dy),
                (frame.pointer_raw_dx, frame.pointer_raw_dy),
            );
        }
        // The key-up transition remains semantically labeled for its frame even
        // though the end-of-frame physical key state is empty.
        assert_eq!(metadata.annotation.labeled_frames, 4);
        let source_samples = [
            fixture.session.join("runs/r0000/segments/00000/video.mp4"),
            fixture.session.join("runs/r0000/segments/00001/video.mp4"),
        ]
        .into_iter()
        .flat_map(|path| sample_fingerprints(&path))
        .collect::<Vec<_>>();
        let output_samples = [
            session_dir.join("c0000/p0.mp4"),
            session_dir.join("c0001/p0.mp4"),
        ]
        .into_iter()
        .flat_map(|path| sample_fingerprints(&path))
        .collect::<Vec<_>>();
        assert_eq!(source_samples, output_samples);
        let actual = GoldenSummary {
            frame_count: converted.frame_count,
            chunk_count: converted.chunk_count,
            frame_pair_count: pairs.len() as u64,
            metadata_keys: object_keys(&metadata),
            chunk_meta_keys: object_keys(&chunk0_meta),
            stats_keys: object_keys(&stats),
            schema_version: metadata.schema_version,
            metadata_catalog: metadata.catalog.clone(),
            chunk0_catalog: chunk0_meta.catalog.clone(),
            non_trainable: metadata.non_trainable,
            production_certified: metadata.production_certified,
            chunk0_source_segments: chunk0_meta.provenance.raw_segments,
            chunk1_source_segments: chunk1_meta.provenance.raw_segments,
            frame_pairs: vec![pairs[0].clone(), pairs[28].clone(), pairs[57].clone()],
            rows: vec![
                chunk0[0].clone(),
                chunk0[1].clone(),
                chunk1[0].clone(),
                chunk1[119].clone(),
            ],
        };
        let expected: serde_json::Value =
            serde_json::from_str(include_str!("../tests/golden/data-demo-summary.json")).unwrap();
        assert_eq!(serde_json::to_value(actual).unwrap(), expected);
    }

    #[test]
    fn release_profile_converts_stamps_and_verifies_round_trip() {
        let compliance = ComplianceProfile::FirstPartyRelease {
            consent_id: "CNS-20260724-001".into(),
        };
        let fixture = fixture_with(
            &[
                SegmentSpec {
                    frames: 100,
                    sync_samples: vec![0],
                },
                SegmentSpec {
                    frames: 140,
                    sync_samples: vec![0, 20],
                },
            ],
            &compliance,
        );
        let converted =
            convert_session_with_profile(&fixture.session, &fixture.output, &compliance).unwrap();
        assert_eq!((converted.frame_count, converted.chunk_count), (240, 2));
        verify_dataset_with_profile(&fixture.output, &compliance).unwrap();

        let session_dir = fixture
            .output
            .join("synthetic_fixture")
            .join("fixture-session");
        let metadata: SessionMetadata = read_json(&session_dir.join("metadata.json")).unwrap();
        assert_eq!(metadata.rights, "first_party");
        assert!(metadata.first_party);
        assert_eq!(metadata.consent_id.as_deref(), Some("CNS-20260724-001"));
        assert!(!metadata.non_trainable);
        assert!(metadata.is_trainable);
        assert!(metadata.production_certified);
        assert_eq!(metadata.annotation.source, "human_recorded");
        assert_eq!(metadata.tags, ["hf_teaser", "first_party", "trainable"]);
        let chunk_meta: ChunkMeta = read_json(&session_dir.join("c0000/meta.json")).unwrap();
        assert!(chunk_meta.is_trainable && chunk_meta.production_certified);
        assert_eq!(chunk_meta.consent_id.as_deref(), Some("CNS-20260724-001"));
        let pairs: Vec<FramePair> = read_jsonl(&session_dir.join("framepairs.jsonl")).unwrap();
        assert!(
            pairs
                .iter()
                .all(|pair| pair.consent_id.as_deref() == Some("CNS-20260724-001"))
        );
        let stats: DatasetStats = read_json(&fixture.output.join("_dataset_stats.json")).unwrap();
        assert!(!stats.compliance.all_non_trainable);
        assert_eq!(stats.compliance.trainable_sessions, 1);
        assert_eq!(stats.compliance.production_certified_sessions, 1);

        // A release dataset must not pass under the local-debug expectations,
        // nor under a mismatched consent id.
        assert!(verify_dataset(&fixture.output).is_err());
        assert!(
            verify_dataset_with_profile(
                &fixture.output,
                &ComplianceProfile::FirstPartyRelease {
                    consent_id: "CNS-20260724-999".into(),
                },
            )
            .is_err()
        );
    }

    #[test]
    fn release_profile_refuses_non_consented_raw_session() {
        let fixture = fixture(&[SegmentSpec {
            frames: 120,
            sync_samples: vec![0],
        }]);
        let error = convert_session_with_profile(
            &fixture.session,
            &fixture.output,
            &ComplianceProfile::FirstPartyRelease {
                consent_id: "CNS-20260724-001".into(),
            },
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("non-trainable"));
    }

    #[test]
    fn local_debug_profile_refuses_consented_raw_session() {
        let compliance = ComplianceProfile::FirstPartyRelease {
            consent_id: "CNS-20260724-001".into(),
        };
        let fixture = fixture_with(
            &[SegmentSpec {
                frames: 120,
                sync_samples: vec![0],
            }],
            &compliance,
        );
        let error = convert_session(&fixture.session, &fixture.output).unwrap_err();
        assert!(format!("{error:#}").contains("non-trainable"));
    }

    #[test]
    fn release_profile_refuses_consent_mismatch_in_raw_session() {
        let compliance = ComplianceProfile::FirstPartyRelease {
            consent_id: "CNS-20260724-001".into(),
        };
        let fixture = fixture_with(
            &[SegmentSpec {
                frames: 120,
                sync_samples: vec![0],
            }],
            &compliance,
        );
        let error = convert_session_with_profile(
            &fixture.session,
            &fixture.output,
            &ComplianceProfile::FirstPartyRelease {
                consent_id: "CNS-20260724-002".into(),
            },
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("consent id"));
    }

    #[test]
    fn release_profile_requires_non_empty_consent_id() {
        let fixture = fixture(&[SegmentSpec {
            frames: 120,
            sync_samples: vec![0],
        }]);
        let error = convert_session_with_profile(
            &fixture.session,
            &fixture.output,
            &ComplianceProfile::FirstPartyRelease {
                consent_id: "  ".into(),
            },
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("non-empty consent id"));
    }

    const FIXTURE_WAIVER_REF: &str =
        "r2://diogenes-model-training/raw/v1/attested-trainable/WAIVER.json@2026-08-10";

    fn attested_compliance() -> ComplianceProfile {
        ComplianceProfile::AttestedTrainable {
            waiver_ref: FIXTURE_WAIVER_REF.into(),
        }
    }

    #[test]
    fn attested_profile_converts_stamps_and_verifies_round_trip() {
        let compliance = attested_compliance();
        // attested 车道的如实 RAW 与 local-debug 同形（non_trainable=true、
        // consent_id=null、non_trainable_ready），夹具直接复用。
        let fixture = fixture_with(
            &[
                SegmentSpec {
                    frames: 100,
                    sync_samples: vec![0],
                },
                SegmentSpec {
                    frames: 140,
                    sync_samples: vec![0, 20],
                },
            ],
            &compliance,
        );
        let converted =
            convert_session_with_profile(&fixture.session, &fixture.output, &compliance).unwrap();
        assert_eq!((converted.frame_count, converted.chunk_count), (240, 2));
        verify_dataset_with_profile(&fixture.output, &compliance).unwrap();

        let session_dir = fixture
            .output
            .join("synthetic_fixture")
            .join("fixture-session");
        let metadata: SessionMetadata = read_json(&session_dir.join("metadata.json")).unwrap();
        assert_eq!(metadata.rights, "first_party_attested");
        assert!(metadata.first_party);
        assert_eq!(metadata.consent_id, None);
        assert_eq!(
            metadata.attestation.as_ref().map(|a| a.waiver_ref.as_str()),
            Some(FIXTURE_WAIVER_REF)
        );
        assert!(!metadata.non_trainable);
        assert!(metadata.is_trainable);
        assert!(!metadata.production_certified);
        assert_eq!(metadata.annotation.source, "human_recorded");
        assert_eq!(metadata.tags, ["attested_trainable", "first_party"]);
        let chunk_meta: ChunkMeta = read_json(&session_dir.join("c0000/meta.json")).unwrap();
        assert!(chunk_meta.is_trainable && !chunk_meta.production_certified);
        assert_eq!(chunk_meta.consent_id, None);
        assert_eq!(chunk_meta.attestation, metadata.attestation);
        let pairs: Vec<FramePair> = read_jsonl(&session_dir.join("framepairs.jsonl")).unwrap();
        assert!(pairs.iter().all(|pair| pair.consent_id.is_none()));
        let stats: DatasetStats = read_json(&fixture.output.join("_dataset_stats.json")).unwrap();
        assert!(!stats.compliance.all_non_trainable);
        assert_eq!(stats.compliance.trainable_sessions, 1);
        assert_eq!(stats.compliance.production_certified_sessions, 0);
        assert_eq!(stats.compliance.attestation, metadata.attestation);
        // WAIVER restrictions 的固定披露三点必须落进 DATASHEET。
        let datasheet_text = fs::read_to_string(fixture.output.join("DATASHEET.md")).unwrap();
        assert!(datasheet_text.contains("具结车道披露（attested-trainable）"));
        assert!(datasheet_text.contains(FIXTURE_WAIVER_REF));
        assert!(datasheet_text.contains("`combat.aim` 坏标签"));
        assert!(datasheet_text.contains("view_scale 未标定"));

        // An attested dataset must not pass under local-debug or release
        // expectations, nor under a mismatched waiver ref.
        assert!(verify_dataset(&fixture.output).is_err());
        assert!(
            verify_dataset_with_profile(
                &fixture.output,
                &ComplianceProfile::FirstPartyRelease {
                    consent_id: "CNS-20260724-001".into(),
                },
            )
            .is_err()
        );
        assert!(
            verify_dataset_with_profile(
                &fixture.output,
                &ComplianceProfile::AttestedTrainable {
                    waiver_ref: "r2://other/WAIVER.json@2026-01-01".into(),
                },
            )
            .is_err()
        );
    }

    #[test]
    fn attested_profile_accepts_v2_dev_non_trainable_catalog() {
        // v2 目录门对 attested 与 local-debug 同口径：mode=dev_non_trainable、
        // catalog.json non_trainable=true，六份 snapshot 逐字节 SHA-256 照旧。
        let fixture = fixture_v2(&[SegmentSpec {
            frames: 120,
            sync_samples: vec![0],
        }]);
        let compliance = attested_compliance();
        let converted =
            convert_session_with_profile(&fixture.session, &fixture.output, &compliance).unwrap();
        assert_eq!((converted.frame_count, converted.chunk_count), (120, 1));
        verify_dataset_with_profile(&fixture.output, &compliance).unwrap();
        let metadata: SessionMetadata = read_json(
            &fixture
                .output
                .join("synthetic_fixture/fixture-session/metadata.json"),
        )
        .unwrap();
        assert_eq!(metadata.rights, "first_party_attested");
        assert_eq!(metadata.schema_version, DATA_MODEL_SCHEMA_VERSION);
    }

    #[test]
    fn neutral_ingest_v2_converts_in_full_and_incremental_modes() {
        let fixture = fixture_ingest_v2(&[SegmentSpec {
            frames: 120,
            sync_samples: vec![0],
        }]);

        let converted = convert_session(&fixture.session, &fixture.output).unwrap();
        assert_eq!((converted.frame_count, converted.chunk_count), (120, 1));
        verify_dataset(&fixture.output).unwrap();

        let incremental_output = fixture._temp.path().join("incremental-neutral-v2");
        let refs = ready_refs(&[("r0000", 0)]);
        let report =
            advance_incremental(&fixture.session, &incremental_output, &refs, None).unwrap();
        assert_eq!((report.published_chunks, report.pending_frames), (1, 0));
        finalize_incremental(&fixture.session, &incremental_output, &refs).unwrap();
        verify_dataset(&incremental_output).unwrap();
    }

    #[test]
    fn neutral_ingest_v2_rejects_reintroduced_training_claims() {
        let fixture = fixture_ingest_v2(&[SegmentSpec {
            frames: 120,
            sync_samples: vec![0],
        }]);
        let session_path = fixture.session.join("session.json");
        let mut session: serde_json::Value = read_json(&session_path).unwrap();
        session["non_trainable"] = json!(true);
        write_json(&session_path, &session).unwrap();

        let error = convert_session(&fixture.session, &fixture.output).unwrap_err();
        assert!(
            format!("{error:#}").contains("training claims do not match its session generation")
        );
    }

    fn local_debug_bundle3_fixture() -> Fixture {
        let fixture = fixture_ingest_v2(&[SegmentSpec {
            frames: 120,
            sync_samples: vec![0],
        }]);
        let session_path = fixture.session.join("session.json");
        let mut session: serde_json::Value = read_json(&session_path).unwrap();
        session["schema_version"] = json!(RAW_SESSION_SCHEMA_V2);
        session["non_trainable"] = json!(true);
        session["production_certified"] = json!(false);
        session["catalog"]["mode"] = json!(CATALOG_MODE_DEV);
        session["catalog"]["provenance"] = json!("compiled");
        write_json(&session_path, &session).unwrap();
        let segment = fixture.session.join("runs/r0000/segments/00000");
        let mut meta: serde_json::Value = read_json(&segment.join("meta.json")).unwrap();
        meta["schema_version"] = json!(RAW_SEGMENT_SCHEMA);
        meta["state"] = json!(RAW_READY_STATE);
        meta["non_trainable"] = json!(true);
        meta["production_certified"] = json!(false);
        meta["catalog"] = session["catalog"].clone();
        write_json(&segment.join("meta.json"), &meta).unwrap();
        rewrite_segment_meta_hash(&segment);
        let run_path = fixture.session.join("runs/r0000/run.json");
        let mut run: serde_json::Value = read_json(&run_path).unwrap();
        run["schemaVersion"] = json!(RAW_SEGMENT_SCHEMA);
        run["state"] = json!(RAW_RUN_COMPLETE_STATE);
        run["nonTrainable"] = json!(true);
        run["productionCertified"] = json!(false);
        write_json(&run_path, &run).unwrap();
        fixture
    }

    #[test]
    fn local_debug_with_current_bundle3_converts_without_reclassifying_raw() {
        let fixture = local_debug_bundle3_fixture();
        let source_before = sha256_file(&fixture.session.join("catalog.json")).unwrap();
        let header_before = fs::read(fixture.session.join("session.json")).unwrap();
        let converted = convert_session(&fixture.session, &fixture.output).unwrap();
        assert_eq!(converted.frame_count, 120);
        verify_dataset(&fixture.output).unwrap();
        let report = advance_incremental(
            &fixture.session,
            &fixture.output.with_extension("incremental"),
            &[ReadySegmentRef {
                run_id: "r0000".into(),
                segment_index: 0,
            }],
            None,
        )
        .unwrap();
        assert_eq!(report.published_frames, 120);
        assert_eq!(
            source_before,
            sha256_file(&fixture.session.join("catalog.json")).unwrap()
        );
        assert_eq!(
            header_before,
            fs::read(fixture.session.join("session.json")).unwrap()
        );
        let metadata: SessionMetadata = read_json(
            &fixture
                .output
                .join("synthetic_fixture/fixture-session/metadata.json"),
        )
        .unwrap();
        assert!(metadata.non_trainable);
        assert!(!metadata.is_trainable && !metadata.production_certified);
    }

    #[test]
    fn local_bundle3_compatibility_keeps_hash_and_classification_checks() {
        let fixture = local_debug_bundle3_fixture();
        let device = fixture.session.join("device-profile.json");
        let bytes = fs::read(&device).unwrap();
        let mut changed = bytes.clone();
        changed.push(b' ');
        fs::write(&device, changed).unwrap();
        let error = convert_session(&fixture.session, &fixture.output).unwrap_err();
        assert!(format!("{error:#}").contains("exact-byte SHA-256 mismatch"));
        fs::write(&device, bytes).unwrap();
        let mut session: serde_json::Value =
            read_json(&fixture.session.join("session.json")).unwrap();
        session["production_certified"] = json!(true);
        write_json(&fixture.session.join("session.json"), &session).unwrap();
        let error = convert_session(&fixture.session, &fixture.output).unwrap_err();
        assert!(format!("{error:#}").contains("training claims"));
    }

    #[test]
    fn attested_profile_accepts_plain_ready_segment_state() {
        let fixture = fixture_with(
            &[SegmentSpec {
                frames: 120,
                sync_samples: vec![0],
            }],
            &attested_compliance(),
        );
        let segment_dir = fixture.session.join("runs/r0000/segments/00000");
        let meta_path = segment_dir.join("meta.json");
        let mut meta: serde_json::Value = read_json(&meta_path).unwrap();
        meta["state"] = json!(RAW_PLAIN_READY_STATE);
        write_json(&meta_path, &meta).unwrap();
        rewrite_segment_meta_hash(&segment_dir);

        // local-debug 只认 non_trainable_ready，裸 ready 仍被拒。
        let debug_output = fixture.output.with_file_name("dataset-debug");
        let error = convert_session(&fixture.session, &debug_output).unwrap_err();
        assert!(format!("{error:#}").contains("segment is not ready"));

        let compliance = attested_compliance();
        convert_session_with_profile(&fixture.session, &fixture.output, &compliance).unwrap();
        verify_dataset_with_profile(&fixture.output, &compliance).unwrap();
    }

    #[test]
    fn attested_profile_refuses_consented_raw_session() {
        // 具结档如实接受 non-trainable RAW；consent 形状的 RAW 属于
        // first-party-release，不属于这条车道。
        let fixture = fixture_with(
            &[SegmentSpec {
                frames: 120,
                sync_samples: vec![0],
            }],
            &ComplianceProfile::FirstPartyRelease {
                consent_id: "CNS-20260724-001".into(),
            },
        );
        let error =
            convert_session_with_profile(&fixture.session, &fixture.output, &attested_compliance())
                .unwrap_err();
        assert!(format!("{error:#}").contains("non-trainable"));
    }

    #[test]
    fn attested_profile_refuses_raw_session_carrying_consent_id() {
        let fixture = fixture_with(
            &[SegmentSpec {
                frames: 120,
                sync_samples: vec![0],
            }],
            &attested_compliance(),
        );
        let session_path = fixture.session.join("session.json");
        let mut session: serde_json::Value = read_json(&session_path).unwrap();
        session["consent_id"] = json!("CNS-20260724-001");
        write_json(&session_path, &session).unwrap();

        let error =
            convert_session_with_profile(&fixture.session, &fixture.output, &attested_compliance())
                .unwrap_err();
        assert!(format!("{error:#}").contains("carrying a consent id"));
    }

    #[test]
    fn attested_profile_requires_non_empty_waiver_ref() {
        let fixture = fixture_with(
            &[SegmentSpec {
                frames: 120,
                sync_samples: vec![0],
            }],
            &attested_compliance(),
        );
        let error = convert_session_with_profile(
            &fixture.session,
            &fixture.output,
            &ComplianceProfile::AttestedTrainable {
                waiver_ref: "  ".into(),
            },
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("non-empty waiver ref"));
    }

    #[test]
    fn local_debug_output_carries_no_attestation_key() {
        // local-debug 与 first-party-release 两档输出字节零变化：attestation
        // 字段按缺席序列化，metadata/chunk/stats 三处都不得出现该键。
        let fixture = fixture(&[SegmentSpec {
            frames: 120,
            sync_samples: vec![0],
        }]);
        convert_session(&fixture.session, &fixture.output).unwrap();
        let session_dir = fixture.output.join("synthetic_fixture/fixture-session");
        for path in [
            fixture.output.join("_dataset_stats.json"),
            session_dir.join("metadata.json"),
            session_dir.join("c0000/meta.json"),
        ] {
            let text = fs::read_to_string(&path).unwrap();
            assert!(
                !text.contains("attestation"),
                "{} must not mention attestation",
                path.display()
            );
        }
    }

    #[test]
    fn legacy_bundle1_session_converts_under_attested_profile_with_full_stamp() {
        // issue #811 路线 A 的主验收：bundle-1 世代（capture+settings 两册 +
        // dm-mapping-1）在具结档下走完 convert + verify，印章与 Halo 首批同形。
        let fixture = fixture_legacy_bundle1(&[
            SegmentSpec {
                frames: 100,
                sync_samples: vec![0],
            },
            SegmentSpec {
                frames: 140,
                sync_samples: vec![0, 20],
            },
        ]);
        let compliance = attested_compliance();
        let converted =
            convert_session_with_profile(&fixture.session, &fixture.output, &compliance).unwrap();
        assert_eq!((converted.frame_count, converted.chunk_count), (240, 2));
        verify_dataset_with_profile(&fixture.output, &compliance).unwrap();

        let session_dir = fixture
            .output
            .join("synthetic_fixture")
            .join("fixture-session");
        let metadata: SessionMetadata = read_json(&session_dir.join("metadata.json")).unwrap();
        assert_eq!(metadata.rights, "first_party_attested");
        assert!(metadata.first_party);
        assert_eq!(metadata.consent_id, None);
        assert_eq!(
            metadata.attestation.as_ref().map(|a| a.waiver_ref.as_str()),
            Some(FIXTURE_WAIVER_REF)
        );
        assert!(metadata.is_trainable);
        assert!(!metadata.production_certified);
        // bundle-1 目录没有 provenance 审计事实，输出的 catalog 块必须整个缺席，
        // 而不是编一个出来。
        assert_eq!(metadata.catalog, None);
        let datasheet_text = fs::read_to_string(fixture.output.join("DATASHEET.md")).unwrap();
        assert!(datasheet_text.contains("具结车道披露（attested-trainable）"));
        assert!(datasheet_text.contains(FIXTURE_WAIVER_REF));
        // COD 封存车道无 view_scale 标定：角度列整列留白 + 卡面披露。
        assert!(datasheet_text.contains("view_scale 未标定"));
        assert!(datasheet_text.contains("not calibrated for this artifact"));

        // local-debug 同样读得动这一代（同一非放行口径）。
        let debug_output = fixture.output.with_file_name("dataset-legacy-debug");
        convert_session(&fixture.session, &debug_output).unwrap();
        verify_dataset(&debug_output).unwrap();
    }

    #[test]
    fn legacy_bundle1_audio_session_converts_under_attested_profile() {
        // 封存 COD 车道的 07-31 形态：session-3 + 音频 + bundle-1 目录。
        let fixture = fixture_legacy_bundle1_audio(&[SegmentSpec {
            frames: 120,
            sync_samples: vec![0],
        }]);
        let compliance = attested_compliance();
        let converted =
            convert_session_with_profile(&fixture.session, &fixture.output, &compliance).unwrap();
        assert_eq!((converted.frame_count, converted.chunk_count), (120, 1));
        verify_dataset_with_profile(&fixture.output, &compliance).unwrap();
        let session_dir = fixture.output.join("synthetic_fixture/fixture-session");
        let metadata: SessionMetadata = read_json(&session_dir.join("metadata.json")).unwrap();
        assert_eq!(metadata.rights, "first_party_attested");
        // 音频链路整条走通：chunk 侧必须真的带出 FLAC。
        assert!(session_dir.join("c0000/p0.flac").is_file());
    }

    #[test]
    fn legacy_bundle1_accepts_device_profile_2_generation() {
        // R2 实测封存车道 device profile 横跨 -1/-2 两个 schema（07-31 音频代
        // 先于断代把 device 升到了 -2）；两种都必须走得通。
        let fixture = fixture_legacy_bundle1_with(
            &[SegmentSpec {
                frames: 120,
                sync_samples: vec![0],
            }],
            None,
            "dm-device-profile-2",
        );
        let compliance = attested_compliance();
        convert_session_with_profile(&fixture.session, &fixture.output, &compliance).unwrap();
        verify_dataset_with_profile(&fixture.output, &compliance).unwrap();
    }

    #[test]
    fn legacy_bundle1_refused_under_first_party_release() {
        // docs/57 的断代对 first-party-release 不放松。如实的 bundle-1 RAW 在更早的
        // claims 检查就倒下；这里把 claims/consent 伪造成 release 形状，验证的是
        // 即便前面的门全被骗过，世代门本身也把 bundle-1 拒在外面。
        let fixture = fixture_legacy_bundle1(&[SegmentSpec {
            frames: 120,
            sync_samples: vec![0],
        }]);
        let session_path = fixture.session.join("session.json");
        let mut session: serde_json::Value = read_json(&session_path).unwrap();
        session["non_trainable"] = json!(false);
        session["consent_id"] = json!("CNS-20260724-001");
        write_json(&session_path, &session).unwrap();
        let error = convert_session_with_profile(
            &fixture.session,
            &fixture.output,
            &ComplianceProfile::FirstPartyRelease {
                consent_id: "CNS-20260724-001".into(),
            },
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("bundle-1"));
    }

    #[test]
    fn legacy_bundle1_rejects_catalog_generation_hybrids() {
        let fixture = fixture_legacy_bundle1(&[SegmentSpec {
            frames: 120,
            sync_samples: vec![0],
        }]);
        let session_path = fixture.session.join("session.json");
        let baseline: serde_json::Value = read_json(&session_path).unwrap();

        // capture+settings 之上再塞一条 recording ⇒ 世代杂交，直接拒。
        let mut hybrid = baseline.clone();
        hybrid["catalog"]["recording"] = hybrid["catalog"]["capture"].clone();
        write_json(&session_path, &hybrid).unwrap();
        let error =
            convert_session_with_profile(&fixture.session, &fixture.output, &attested_compliance())
                .unwrap_err();
        assert!(format!("{error:#}").contains("mixes generations"));

        // 只剩 capture、缺 settings ⇒ 同样不是任何真实世代的形状。
        let mut partial = baseline;
        partial["catalog"]
            .as_object_mut()
            .unwrap()
            .remove("settings");
        write_json(&session_path, &partial).unwrap();
        let error =
            convert_session_with_profile(&fixture.session, &fixture.output, &attested_compliance())
                .unwrap_err();
        assert!(format!("{error:#}").contains("mixes generations"));
    }

    #[test]
    fn legacy_bundle1_rejects_wrong_mapping_generation() {
        // bundle-1 会话钉 dm-mapping-1；把 dm-mapping-2 塞进去（哈希也跟着改齐）
        // 必须在 legacy mapping 读取器处被拒，而不是静默换代。
        let fixture = fixture_legacy_bundle1(&[SegmentSpec {
            frames: 120,
            sync_samples: vec![0],
        }]);
        fs::write(fixture.session.join("mapping.json"), MAPPING).unwrap();
        let error =
            convert_session_with_profile(&fixture.session, &fixture.output, &attested_compliance())
                .unwrap_err();
        assert!(format!("{error:#}").contains("invalid legacy v1 game mapping"));
    }

    #[test]
    fn legacy_bundle1_rejects_tampered_snapshot_bytes() {
        // 五件套逐字节 SHA-256 对账与当代同纪律：settings 快照动一个字节即拒。
        let fixture = fixture_legacy_bundle1(&[SegmentSpec {
            frames: 120,
            sync_samples: vec![0],
        }]);
        let settings_path = fixture.session.join("settings-profile.json");
        let mut settings: serde_json::Value = read_json(&settings_path).unwrap();
        settings["window_mode"] = json!("tampered");
        write_json(&settings_path, &settings).unwrap();
        let error =
            convert_session_with_profile(&fixture.session, &fixture.output, &attested_compliance())
                .unwrap_err();
        assert!(format!("{error:#}").contains("settings-profile.json exact-byte SHA-256 mismatch"));
    }

    #[test]
    fn legacy_bundle1_rejects_mapping_reference_key_of_wrong_generation() {
        // session.mapping 的 profile 引用键按世代互斥：bundle-1 会话写
        // recording_profile_id 属于杂交形态。目录对账先一步把它拒掉（settings
        // 引用键缺席 = 与 catalog 选择对不上）。
        let fixture = fixture_legacy_bundle1(&[SegmentSpec {
            frames: 120,
            sync_samples: vec![0],
        }]);
        let session_path = fixture.session.join("session.json");
        let mut session: serde_json::Value = read_json(&session_path).unwrap();
        session["mapping"]["recording_profile_id"] = json!(LEGACY_SETTINGS_PROFILE_ID);
        session["mapping"]
            .as_object_mut()
            .unwrap()
            .remove("settings_profile_id");
        write_json(&session_path, &session).unwrap();
        let error =
            convert_session_with_profile(&fixture.session, &fixture.output, &attested_compliance())
                .unwrap_err();
        assert!(
            format!("{error:#}")
                .contains("session.json mapping identity does not match its v2 catalog selection")
        );
    }

    #[test]
    fn legacy_bundle1_view_scale_lives_in_settings_snapshot() {
        // bundle-1 世代 view_scale 块住 settings profile；判定逻辑与当代同一：
        // 声明 applicable 且常数/ads_states 齐全 ⇒ 标定成立。
        let fixture = fixture_legacy_bundle1_with(
            &[SegmentSpec {
                frames: 120,
                sync_samples: vec![0],
            }],
            Some(json!({
                "applicable": true,
                "counts_per_360": 4096.0,
                "vertical_scale_ratio": 1.0,
                "ads_states": [{"state": "hip"}]
            })),
            "dm-device-profile-1",
        );
        let header = validate_session_header(&fixture.session, &attested_compliance()).unwrap();
        assert_eq!(
            header.view_scale,
            Some(ViewScaleConstants {
                counts_per_360: 4096.0,
                vertical_scale_ratio: 1.0
            })
        );

        // 声明了 applicable 却带非法数值 = 目录数据谎言，照旧拒绝，且出处指向
        // settings-profile.json。
        let bad = fixture_legacy_bundle1_with(
            &[SegmentSpec {
                frames: 120,
                sync_samples: vec![0],
            }],
            Some(json!({
                "applicable": true,
                "counts_per_360": -1.0,
                "vertical_scale_ratio": 1.0,
                "ads_states": [{"state": "hip"}]
            })),
            "dm-device-profile-1",
        );
        let error = validate_session_header(&bad.session, &attested_compliance()).unwrap_err();
        assert!(format!("{error:#}").contains("settings-profile.json view_scale.counts_per_360"));
    }

    #[test]
    fn trailing_failed_run_directory_is_skipped() {
        // 客户端崩溃后留下的「完整 r0000 + 尾随 failed r0001（仅 failure.json、零
        // segment）」形态（真实 COD 具结车道 dev-1785045787023-0005）：死目录不该
        // 拖垮整个会话。
        let fixture = fixture(&[SegmentSpec {
            frames: 120,
            sync_samples: vec![0],
        }]);
        let failed = fixture.session.join("runs/r0001");
        fs::create_dir_all(&failed).unwrap();
        write_json(
            &failed.join("failure.json"),
            &json!({
                "schema_version": RAW_SEGMENT_SCHEMA,
                "state": "failed",
                "session_id": "fixture-session",
                "run_id": "r0001",
                "error": "WGC callback queue dropped 1 frame(s)"
            }),
        )
        .unwrap();
        let report = convert_session(&fixture.session, &fixture.output).unwrap();
        assert_eq!(report.frame_count, 120);
        verify_dataset(&fixture.output).unwrap();
        let attested_output = fixture.output.with_file_name("dataset-attested");
        let compliance = attested_compliance();
        convert_session_with_profile(&fixture.session, &attested_output, &compliance).unwrap();
        verify_dataset_with_profile(&attested_output, &compliance).unwrap();
    }

    #[test]
    fn session_whose_only_run_failed_has_no_ready_content() {
        // run 未完结即非 ready：failed run 的 segments（若有）也不进数据集——run 级
        // 帧/事件计数从未定稿。整个会话因此没有可交付内容。
        let fixture = fixture(&[SegmentSpec {
            frames: 120,
            sync_samples: vec![0],
        }]);
        let run_dir = fixture.session.join("runs/r0000");
        fs::rename(run_dir.join("run.json"), run_dir.join("failure.json")).unwrap();
        let error = convert_session(&fixture.session, &fixture.output).unwrap_err();
        assert!(format!("{error:#}").contains("no ready segments"));
    }

    #[test]
    fn run_directory_with_neither_run_nor_failure_record_is_rejected() {
        // 跳过只给「客户端明确封了 failure.json」的目录；两个文件都缺不是客户端
        // 会留下的形状，仍然报错而不是静默略过。
        let fixture = fixture(&[SegmentSpec {
            frames: 120,
            sync_samples: vec![0],
        }]);
        fs::remove_file(fixture.session.join("runs/r0000/run.json")).unwrap();
        let error = convert_session(&fixture.session, &fixture.output).unwrap_err();
        assert!(format!("{error:#}").contains("neither run.json nor failure.json"));
    }

    #[test]
    fn dataset_verifier_rejects_duplicate_semantic_actions() {
        let fixture = fixture(&[SegmentSpec {
            frames: 12,
            sync_samples: vec![0],
        }]);
        convert_session(&fixture.session, &fixture.output).unwrap();
        let frames_path = fixture
            .output
            .join("synthetic_fixture/fixture-session/c0000/p0.jsonl");
        let mut rows: Vec<DataDemoFrame> = read_jsonl(&frames_path).unwrap();
        rows[0].actions.push("move.forward".into());
        write_jsonl(&frames_path, &rows).unwrap();

        let error = verify_dataset(&fixture.output).unwrap_err();
        assert!(format!("{error:#}").contains("frame actions are not sorted/unique"));
    }

    #[test]
    fn legacy_data_demo_actions_default_to_empty() {
        let row: DataDemoFrame = serde_json::from_value(json!({
            "frame": 0,
            "gframe": 0,
            "video_t_ms": 0,
            "keys": [87],
            "pointerDx": 0,
            "pointerDy": 0,
            "pointerBins": [0, 0],
            "events": [],
            "src": "rec"
        }))
        .unwrap();
        let action: FramePairAction = serde_json::from_value(json!({
            "keys": [87],
            "pointerDx": 0,
            "pointerDy": 0
        }))
        .unwrap();

        assert!(row.actions.is_empty());
        assert!(action.actions.is_empty());
    }

    #[test]
    fn incremental_same_run_crosses_segments_and_publishes_only_full_chunks() {
        let fixture = fixture(&[
            SegmentSpec {
                frames: 100,
                sync_samples: vec![0],
            },
            SegmentSpec {
                frames: 50,
                sync_samples: vec![0, 20],
            },
        ]);
        fs::remove_file(fixture.session.join("runs/r0000/run.json")).unwrap();
        let refs = ready_refs(&[("r0000", 0), ("r0000", 1)]);

        let report = advance_incremental(&fixture.session, &fixture.output, &refs, None).unwrap();
        assert_eq!(report.ready_segments, 2);
        assert_eq!(report.ready_frames, 150);
        assert_eq!(report.published_chunks, 1);
        assert_eq!(report.published_frames, 120);
        assert_eq!(report.pending_frames, 30);

        let session_dir = fixture.output.join("synthetic_fixture/fixture-session");
        assert!(session_dir.join("c0000/p0.mp4").is_file());
        assert!(!session_dir.join("c0001").exists());
        assert!(!fixture.output.join("metadata.jsonl").exists());
        assert!(!session_dir.join("framepairs.jsonl").exists());
        let meta: ChunkMeta = read_json(&session_dir.join("c0000/meta.json")).unwrap();
        assert_eq!(
            meta.provenance.raw_segments,
            vec!["r0000/00000", "r0000/00001"]
        );
    }

    #[test]
    fn incremental_replay_is_idempotent_and_ignores_unreferenced_tmp_directories() {
        let fixture = fixture(&[
            SegmentSpec {
                frames: 100,
                sync_samples: vec![0],
            },
            SegmentSpec {
                frames: 140,
                sync_samples: vec![0, 20],
            },
        ]);
        fs::remove_file(fixture.session.join("runs/r0000/run.json")).unwrap();
        fs::create_dir(fixture.session.join("runs/r0000/segments/.00002.tmp")).unwrap();
        let refs = ready_refs(&[("r0000", 0), ("r0000", 1)]);

        let first = advance_incremental(&fixture.session, &fixture.output, &refs, None).unwrap();
        let session_dir = fixture.output.join("synthetic_fixture/fixture-session");
        let before = [
            sha256_file(&session_dir.join("c0000/p0.mp4")).unwrap(),
            sha256_file(&session_dir.join("c0000/p0.jsonl")).unwrap(),
            sha256_file(&session_dir.join("c0001/p0.mp4")).unwrap(),
            sha256_file(&session_dir.join("c0001/p0.jsonl")).unwrap(),
        ];
        let second = advance_incremental(&fixture.session, &fixture.output, &refs, None).unwrap();
        let after = [
            sha256_file(&session_dir.join("c0000/p0.mp4")).unwrap(),
            sha256_file(&session_dir.join("c0000/p0.jsonl")).unwrap(),
            sha256_file(&session_dir.join("c0001/p0.mp4")).unwrap(),
            sha256_file(&session_dir.join("c0001/p0.jsonl")).unwrap(),
        ];

        assert_eq!(first, second);
        assert_eq!(before, after);
        assert_eq!((second.published_chunks, second.pending_frames), (2, 0));
    }

    #[test]
    fn incremental_cursor_validates_only_new_refs_and_replays_only_pending_tail() {
        let fixture = fixture(&[
            SegmentSpec {
                frames: 120,
                sync_samples: vec![0],
            },
            SegmentSpec {
                frames: 10,
                sync_samples: vec![0],
            },
            SegmentSpec {
                frames: 110,
                sync_samples: vec![0],
            },
        ]);
        let first_refs = ready_refs(&[("r0000", 0), ("r0000", 1)]);
        reset_segment_validation_count();
        let first =
            advance_incremental(&fixture.session, &fixture.output, &first_refs, None).unwrap();
        assert_eq!(segment_validation_count(), 2);
        assert_eq!((first.published_chunks, first.pending_frames), (1, 10));
        let fully_published_source = fixture.session.join("runs/r0000/segments/00000");
        fs::remove_file(fully_published_source.join("video.mp4")).unwrap();
        fs::remove_file(fully_published_source.join("frames.jsonl.zst")).unwrap();

        reset_segment_validation_count();
        let second = advance_incremental(
            &fixture.session,
            &fixture.output,
            &ready_refs(&[("r0000", 0), ("r0000", 1), ("r0000", 2)]),
            None,
        )
        .unwrap();
        assert_eq!(
            segment_validation_count(),
            1,
            "a healthy cursor must validate only the appended ready ref"
        );
        assert_eq!((second.published_chunks, second.pending_frames), (2, 0));
        let chunk1: Vec<DataDemoFrame> = read_jsonl(
            &fixture
                .output
                .join("synthetic_fixture/fixture-session/c0001/p0.jsonl"),
        )
        .unwrap();
        assert_eq!(chunk1.len(), 120);
    }

    #[test]
    fn missing_incremental_cursor_forces_full_recovery() {
        let fixture = fixture(&[
            SegmentSpec {
                frames: 100,
                sync_samples: vec![0],
            },
            SegmentSpec {
                frames: 140,
                sync_samples: vec![0, 20],
            },
        ]);
        let refs = ready_refs(&[("r0000", 0), ("r0000", 1)]);
        let first = advance_incremental(&fixture.session, &fixture.output, &refs, None).unwrap();
        fs::remove_file(fixture.output.join(".incremental-state.json")).unwrap();

        reset_segment_validation_count();
        let recovered =
            advance_incremental(&fixture.session, &fixture.output, &refs, None).unwrap();
        assert_eq!(segment_validation_count(), 2);
        assert_eq!(recovered, first);
        assert!(fixture.output.join(".incremental-state.json").is_file());
    }

    #[test]
    fn missing_or_corrupt_cursor_upgrades_only_exact_legacy_chunks() {
        for corrupt_cursor in [false, true] {
            let fixture = fixture_v2(&[SegmentSpec {
                frames: 130,
                sync_samples: vec![0, 120],
            }]);
            stamp_catalog_provenance(&fixture, 1, "compiled");
            stamp_run_wall_clock(&fixture);
            let refs = ready_refs(&[("r0000", 0)]);
            let expected = advance_incremental(
                &fixture.session,
                &fixture.output,
                &refs,
                Some(&fixture_wall_clock_context()),
            )
            .unwrap();
            assert_eq!(expected.published_chunks, 1);

            let cursor_path = fixture.output.join(".incremental-state.json");
            if corrupt_cursor {
                fs::write(&cursor_path, b"{not-json").unwrap();
            } else {
                fs::remove_file(&cursor_path).unwrap();
            }
            let chunk = fixture
                .output
                .join("synthetic_fixture/fixture-session/c0000");
            downgrade_data_model_json_to_dm_1(&chunk.join("meta.json"));
            let frames_path = chunk.join("p0.jsonl");
            let mut rows: Vec<DataDemoFrame> = read_jsonl(&frames_path).unwrap();
            for row in &mut rows {
                row.t_utc_est_us = None;
            }
            write_jsonl(&frames_path, &rows).unwrap();

            let recovered = advance_incremental(
                &fixture.session,
                &fixture.output,
                &refs,
                Some(&fixture_wall_clock_context()),
            )
            .unwrap();
            assert_eq!(recovered, expected);
            let cursor: serde_json::Value = read_json(&cursor_path).unwrap();
            assert_eq!(
                cursor["schema_version"],
                json!("dm-data-demo-incremental-cursor-3")
            );
            let meta: serde_json::Value = read_json(&chunk.join("meta.json")).unwrap();
            assert_eq!(meta["catalog"], json!({ "provenance": "compiled" }));
            let rows: Vec<DataDemoFrame> = read_jsonl(&frames_path).unwrap();
            assert!(rows.iter().all(|row| row.t_utc_est_us.is_some()));
        }
    }

    #[test]
    fn missing_cursor_preserves_legacy_chunks_with_other_raw_drift() {
        let fixture = fixture_v2(&[SegmentSpec {
            frames: 130,
            sync_samples: vec![0, 120],
        }]);
        stamp_catalog_provenance(&fixture, 1, "compiled");
        stamp_run_wall_clock(&fixture);
        let refs = ready_refs(&[("r0000", 0)]);
        advance_incremental(
            &fixture.session,
            &fixture.output,
            &refs,
            Some(&fixture_wall_clock_context()),
        )
        .unwrap();
        fs::remove_file(fixture.output.join(".incremental-state.json")).unwrap();

        let chunk = fixture
            .output
            .join("synthetic_fixture/fixture-session/c0000");
        let meta_path = chunk.join("meta.json");
        downgrade_data_model_json_to_dm_1(&meta_path);
        let frames_path = chunk.join("p0.jsonl");
        let mut rows: Vec<DataDemoFrame> = read_jsonl(&frames_path).unwrap();
        for row in &mut rows {
            row.t_utc_est_us = None;
        }
        rows[0].qpc += 1;
        rows[0].qpc_end += 1;
        write_jsonl(&frames_path, &rows).unwrap();
        let before_meta = sha256_file(&meta_path).unwrap();
        let before_rows = sha256_file(&frames_path).unwrap();

        advance_incremental(
            &fixture.session,
            &fixture.output,
            &refs,
            Some(&fixture_wall_clock_context()),
        )
        .unwrap_err();
        assert_eq!(sha256_file(&meta_path).unwrap(), before_meta);
        assert_eq!(sha256_file(&frames_path).unwrap(), before_rows);
        assert!(
            !fixture.output.join(".incremental-state.json").exists(),
            "a rejected recovery must not publish a replacement cursor"
        );
    }

    #[test]
    fn valid_cursor_rejects_changed_published_chunk_bytes() {
        let fixture = fixture(&[SegmentSpec {
            frames: 130,
            sync_samples: vec![0, 120],
        }]);
        stamp_run_wall_clock(&fixture);
        let refs = ready_refs(&[("r0000", 0)]);
        advance_incremental(
            &fixture.session,
            &fixture.output,
            &refs,
            Some(&fixture_wall_clock_context()),
        )
        .unwrap();
        let frames_path = fixture
            .output
            .join("synthetic_fixture/fixture-session/c0000/p0.jsonl");
        let mut rows: Vec<DataDemoFrame> = read_jsonl(&frames_path).unwrap();
        for row in &mut rows {
            row.qpc += 1;
            row.qpc_end += 1;
            row.t_utc_est_us = row.t_utc_est_us.map(|value| value + 33);
        }
        write_jsonl(&frames_path, &rows).unwrap();
        let before = sha256_file(&frames_path).unwrap();

        advance_incremental(
            &fixture.session,
            &fixture.output,
            &refs,
            Some(&fixture_wall_clock_context()),
        )
        .unwrap_err();
        assert_eq!(sha256_file(&frames_path).unwrap(), before);
    }

    #[test]
    fn refreshed_cursor_hash_cannot_authorize_dm_2_without_catalog_key() {
        let fixture = fixture_v2(&[SegmentSpec {
            frames: 130,
            sync_samples: vec![0, 120],
        }]);
        let refs = ready_refs(&[("r0000", 0)]);
        advance_incremental(&fixture.session, &fixture.output, &refs, None).unwrap();
        let meta_path = fixture
            .output
            .join("synthetic_fixture/fixture-session/c0000/meta.json");
        let mut meta: serde_json::Value = read_json(&meta_path).unwrap();
        meta.as_object_mut().unwrap().remove("catalog");
        write_json(&meta_path, &meta).unwrap();
        let cursor_path = fixture.output.join(".incremental-state.json");
        let mut cursor: serde_json::Value = read_json(&cursor_path).unwrap();
        cursor["published_chunk_hashes"][0]["meta.json"] = json!(sha256_file(&meta_path).unwrap());
        write_json(&cursor_path, &cursor).unwrap();
        let before = sha256_file(&meta_path).unwrap();

        advance_incremental(&fixture.session, &fixture.output, &refs, None).unwrap_err();
        assert_eq!(sha256_file(&meta_path).unwrap(), before);
    }

    #[test]
    fn cursorless_recovery_rejects_partially_missing_wall_clock_rows() {
        let fixture = fixture(&[SegmentSpec {
            frames: 130,
            sync_samples: vec![0, 120],
        }]);
        stamp_run_wall_clock(&fixture);
        let refs = ready_refs(&[("r0000", 0)]);
        advance_incremental(
            &fixture.session,
            &fixture.output,
            &refs,
            Some(&fixture_wall_clock_context()),
        )
        .unwrap();
        fs::remove_file(fixture.output.join(".incremental-state.json")).unwrap();
        let frames_path = fixture
            .output
            .join("synthetic_fixture/fixture-session/c0000/p0.jsonl");
        let mut rows: Vec<serde_json::Value> = read_jsonl(&frames_path).unwrap();
        rows[0].as_object_mut().unwrap().remove("t_utc_est_us");
        write_jsonl(&frames_path, &rows).unwrap();
        let before = sha256_file(&frames_path).unwrap();

        advance_incremental(
            &fixture.session,
            &fixture.output,
            &refs,
            Some(&fixture_wall_clock_context()),
        )
        .unwrap_err();
        assert_eq!(sha256_file(&frames_path).unwrap(), before);
    }

    #[test]
    fn cursorless_recovery_rejects_dm_2_chunk_missing_all_wall_clock_rows() {
        let fixture = fixture(&[SegmentSpec {
            frames: 130,
            sync_samples: vec![0, 120],
        }]);
        stamp_run_wall_clock(&fixture);
        let refs = ready_refs(&[("r0000", 0)]);
        advance_incremental(
            &fixture.session,
            &fixture.output,
            &refs,
            Some(&fixture_wall_clock_context()),
        )
        .unwrap();
        fs::remove_file(fixture.output.join(".incremental-state.json")).unwrap();
        let frames_path = fixture
            .output
            .join("synthetic_fixture/fixture-session/c0000/p0.jsonl");
        let mut rows: Vec<serde_json::Value> = read_jsonl(&frames_path).unwrap();
        for row in &mut rows {
            row.as_object_mut().unwrap().remove("t_utc_est_us");
        }
        write_jsonl(&frames_path, &rows).unwrap();
        let before = sha256_file(&frames_path).unwrap();

        advance_incremental(
            &fixture.session,
            &fixture.output,
            &refs,
            Some(&fixture_wall_clock_context()),
        )
        .unwrap_err();
        assert_eq!(sha256_file(&frames_path).unwrap(), before);
    }

    #[test]
    fn incremental_cursor_restores_pressed_input_state_across_calls() {
        let fixture = fixture(&[
            SegmentSpec {
                frames: 2,
                sync_samples: vec![0],
            },
            SegmentSpec {
                frames: 2,
                sync_samples: vec![0],
            },
        ]);
        advance_incremental(
            &fixture.session,
            &fixture.output,
            &ready_refs(&[("r0000", 0)]),
            None,
        )
        .unwrap();
        reset_segment_validation_count();
        let report = advance_incremental(
            &fixture.session,
            &fixture.output,
            &ready_refs(&[("r0000", 0), ("r0000", 1)]),
            None,
        )
        .unwrap();
        assert_eq!(segment_validation_count(), 1);
        assert_eq!((report.ready_frames, report.pending_frames), (4, 4));
    }

    #[test]
    fn incremental_finalize_publishes_tail_and_complete_verified_dataset() {
        let fixture = fixture(&[
            SegmentSpec {
                frames: 100,
                sync_samples: vec![0],
            },
            SegmentSpec {
                frames: 150,
                sync_samples: vec![0, 20, 140],
            },
        ]);
        let refs = ready_refs(&[("r0000", 0), ("r0000", 1)]);

        let live = advance_incremental(&fixture.session, &fixture.output, &refs, None).unwrap();
        assert_eq!((live.published_chunks, live.pending_frames), (2, 10));
        let finalized = finalize_incremental(&fixture.session, &fixture.output, &refs).unwrap();
        assert_eq!((finalized.frame_count, finalized.chunk_count), (250, 3));
        let verified = verify_dataset(&fixture.output).unwrap();
        assert_eq!((verified.frame_count, verified.chunk_count), (250, 3));
        let tail: Vec<DataDemoFrame> = read_jsonl(
            &fixture
                .output
                .join("synthetic_fixture/fixture-session/c0002/p0.jsonl"),
        )
        .unwrap();
        assert_eq!(tail.len(), 10);

        let repeated = finalize_incremental(&fixture.session, &fixture.output, &refs).unwrap();
        assert_eq!(repeated, finalized);
        assert!(!fixture.output.join(".incremental-state.json").exists());
    }

    #[test]
    fn incremental_finalize_with_wall_clock_anchor_matches_full_replay_bytes() {
        let fixture = fixture(&[
            SegmentSpec {
                frames: 100,
                sync_samples: vec![0],
            },
            SegmentSpec {
                frames: 150,
                sync_samples: vec![0, 20, 140],
            },
        ]);
        let run_path = fixture.session.join("runs/r0000/run.json");
        let mut run: serde_json::Value = read_json(&run_path).unwrap();
        run["qpcFrequency"] = json!(30_000);
        run["anchorQpc"] = json!(10_010);
        run["anchorAtUtc"] = json!("2026-08-04T09:08:56.123Z");
        write_json(&run_path, &run).unwrap();
        let refs = ready_refs(&[("r0000", 0), ("r0000", 1)]);

        let wall_clock = IncrementalWallClockContext {
            anchor_at_utc: Some("2026-08-04T09:08:56.123Z".into()),
            anchor_qpc: 10_010,
            qpc_frequency: 30_000,
        };
        let live = advance_incremental(&fixture.session, &fixture.output, &refs, Some(&wall_clock))
            .unwrap();
        assert_eq!((live.published_chunks, live.pending_frames), (2, 10));
        let session_rel = Path::new("synthetic_fixture/fixture-session");
        let incremental_bytes = (0..2)
            .map(|chunk| {
                fs::read(
                    fixture
                        .output
                        .join(session_rel)
                        .join(format!("c{chunk:04}/p0.jsonl")),
                )
                .unwrap()
            })
            .collect::<Vec<_>>();
        let full_replay = fixture.output.with_file_name("full-replay");
        convert_session(&fixture.session, &full_replay).unwrap();
        let replay_bytes = (0..2)
            .map(|chunk| {
                fs::read(
                    full_replay
                        .join(session_rel)
                        .join(format!("c{chunk:04}/p0.jsonl")),
                )
                .unwrap()
            })
            .collect::<Vec<_>>();

        let finalized = finalize_incremental(&fixture.session, &fixture.output, &refs).unwrap();
        assert_eq!((finalized.frame_count, finalized.chunk_count), (250, 3));
        assert_eq!(incremental_bytes, replay_bytes);
        for (chunk, before) in incremental_bytes.iter().enumerate() {
            assert_eq!(
                fs::read(
                    fixture
                        .output
                        .join(session_rel)
                        .join(format!("c{chunk:04}/p0.jsonl")),
                )
                .unwrap(),
                *before,
                "finalize must preserve an already-published chunk byte-for-byte"
            );
        }

        let mut rows = Vec::new();
        for chunk in 0..3 {
            rows.extend(
                read_jsonl::<DataDemoFrame>(
                    &fixture
                        .output
                        .join(session_rel)
                        .join(format!("c{chunk:04}/p0.jsonl")),
                )
                .unwrap(),
            );
        }
        assert_eq!(rows.len(), 250);
        let wall_clock = rows
            .iter()
            .map(|row| {
                row.t_utc_est_us
                    .expect("a recorded wall-clock anchor must reach every output frame")
            })
            .collect::<Vec<_>>();
        assert!(wall_clock.windows(2).all(|pair| pair[0] <= pair[1]));

        let repeated = finalize_incremental(&fixture.session, &fixture.output, &refs).unwrap();
        assert_eq!(repeated, finalized);
    }

    #[test]
    fn raw_wall_clock_extreme_qpc_returns_a_stable_error_without_panicking() {
        let fixture = fixture(&[SegmentSpec {
            frames: 120,
            sync_samples: vec![0],
        }]);
        stamp_run_wall_clock_values(&fixture, i64::MIN, 30_000);

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            convert_session(&fixture.session, &fixture.output)
        }));
        let error = result
            .expect("malicious RAW wall-clock values must not panic")
            .expect_err("an unrepresentable RAW wall-clock projection must be rejected");
        assert_eq!(
            error.root_cause().to_string(),
            "wall-clock projection exceeds supported i64 range"
        );
    }

    #[test]
    fn journal_wall_clock_extreme_projection_returns_the_same_stable_error() {
        let fixture = fixture(&[SegmentSpec {
            frames: 120,
            sync_samples: vec![0],
        }]);
        let context = IncrementalWallClockContext {
            anchor_at_utc: Some("2026-08-04T09:08:56.123Z".into()),
            // The QPC subtraction is representable, but converting the resulting
            // microsecond projection back to i64 is not.
            anchor_qpc: -9_223_372_036_000,
            qpc_frequency: 1,
        };

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            advance_incremental(
                &fixture.session,
                &fixture.output,
                &ready_refs(&[("r0000", 0)]),
                Some(&context),
            )
        }));
        let error = result
            .expect("malicious journal wall-clock values must not panic")
            .expect_err("an unrepresentable journal wall-clock projection must be rejected");
        assert_eq!(
            error.root_cause().to_string(),
            "wall-clock projection exceeds supported i64 range"
        );
    }

    #[test]
    fn raw_binding_wall_clock_extreme_returns_the_same_stable_error() {
        let fixture = fixture(&[SegmentSpec {
            frames: 120,
            sync_samples: vec![0],
        }]);
        stamp_run_wall_clock(&fixture);
        convert_session(&fixture.session, &fixture.output).unwrap();
        stamp_run_wall_clock_values(&fixture, i64::MIN, 30_000);

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            verify_dataset_against_raw_session(&fixture.session, &fixture.output)
        }));
        let error = result
            .expect("malicious RAW wall-clock values must not panic during binding")
            .expect_err("an unrepresentable RAW binding projection must be rejected");
        assert_eq!(
            error.root_cause().to_string(),
            "wall-clock projection exceeds supported i64 range"
        );
    }

    #[test]
    fn invalid_recorded_wall_clock_is_rejected_by_full_conversion() {
        let fixture = fixture(&[SegmentSpec {
            frames: 120,
            sync_samples: vec![0],
        }]);
        stamp_run_wall_clock_utc(&fixture, "2026-01-01T-1:00:00.000Z");

        let error = convert_session(&fixture.session, &fixture.output).unwrap_err();
        assert_eq!(
            error.root_cause().to_string(),
            "recorded wall-clock UTC anchor is invalid"
        );
    }

    #[test]
    fn invalid_journal_wall_clock_is_rejected_before_incremental_publish() {
        let fixture = fixture(&[SegmentSpec {
            frames: 120,
            sync_samples: vec![0],
        }]);
        let context = IncrementalWallClockContext {
            anchor_at_utc: Some("0000-01-01T00:00:00.000Z".into()),
            anchor_qpc: 10_010,
            qpc_frequency: 30_000,
        };

        let error = advance_incremental(
            &fixture.session,
            &fixture.output,
            &ready_refs(&[("r0000", 0)]),
            Some(&context),
        )
        .unwrap_err();
        assert_eq!(
            error.root_cause().to_string(),
            "recorded wall-clock UTC anchor is invalid"
        );
        assert!(
            !fixture
                .output
                .join("synthetic_fixture/fixture-session/c0000")
                .exists()
        );
    }

    #[test]
    fn invalid_raw_wall_clock_is_rejected_during_dataset_binding() {
        let fixture = fixture(&[SegmentSpec {
            frames: 120,
            sync_samples: vec![0],
        }]);
        stamp_run_wall_clock(&fixture);
        convert_session(&fixture.session, &fixture.output).unwrap();
        stamp_run_wall_clock_utc(&fixture, "2026-08-04T09:08:60.000Z");

        let error =
            verify_dataset_against_raw_session(&fixture.session, &fixture.output).unwrap_err();
        assert_eq!(
            error.root_cause().to_string(),
            "recorded wall-clock UTC anchor is invalid"
        );
    }

    #[test]
    fn recorded_wall_clock_parser_rejects_nonexistent_civil_dates() {
        assert!(parse_iso8601_millis_utc("2024-02-29T23:59:59.999Z").is_some());
        for invalid in [
            "0000-01-01T00:00:00.000Z",
            "2023-02-29T00:00:00.000Z",
            "2026-02-31T00:00:00.000Z",
            "2026-04-31T00:00:00.000Z",
            "2026-08-04T09:08:60.000Z",
            "+001-01-01T00:00:00.000Z",
            "2026-+1-01T00:00:00.000Z",
            "2026-01-01T-1:00:00.000Z",
            "2026-01-01T00:-1:00.000Z",
            "2026-01-01T00:00:-1.000Z",
            "2026-01-01T00:00:00.-01Z",
        ] {
            assert_eq!(
                parse_iso8601_millis_utc(invalid),
                None,
                "accepted invalid recorder timestamp {invalid}"
            );
        }
    }

    #[test]
    fn finalized_output_must_bind_to_the_current_raw_session() {
        let stale = fixture(&[SegmentSpec {
            frames: 120,
            sync_samples: vec![0],
        }]);
        convert_session(&stale.session, &stale.output).unwrap();
        let current = fixture(&[SegmentSpec {
            frames: 240,
            sync_samples: vec![0, 120],
        }]);

        let error = finalize_incremental(
            &current.session,
            &stale.output,
            &ready_refs(&[("r0000", 0)]),
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("does not bind to current RAW")
                || message.contains("does not match current RAW")
                || message.contains("differs from final replay")
        );
        assert!(
            !stale
                .output
                .join("synthetic_fixture/fixture-session/c0001")
                .exists(),
            "binding rejection must not mutate the stale finalized dataset"
        );
    }

    #[test]
    fn framepairs_do_not_cross_pause_run_boundaries() {
        let fixture = fixture(&[
            SegmentSpec {
                frames: 64,
                sync_samples: vec![0],
            },
            SegmentSpec {
                frames: 64,
                sync_samples: vec![0, 56],
            },
        ]);
        split_second_segment_into_new_run(&fixture);

        convert_session(&fixture.session, &fixture.output).unwrap();
        verify_dataset(&fixture.output).unwrap();
        let session_dir = fixture.output.join("synthetic_fixture/fixture-session");
        let metadata: SessionMetadata = read_json(&session_dir.join("metadata.json")).unwrap();
        assert_eq!(
            metadata.provenance.run_frame_ranges,
            vec![
                RunFrameRange {
                    run_id: "r0000".into(),
                    frame_start: 0,
                    frame_end: 64,
                },
                RunFrameRange {
                    run_id: "r0001".into(),
                    frame_start: 64,
                    frame_end: 128,
                },
            ]
        );
        let pairs: Vec<FramePair> = read_jsonl(&session_dir.join("framepairs.jsonl")).unwrap();
        assert_eq!(pairs.len(), 28);
        assert!(
            !pairs
                .iter()
                .any(|pair| pair.frame_t == 56 || pair.frame_t == 60)
        );
    }

    #[test]
    fn zero_frame_run_must_preserve_the_session_frame_cursor() {
        let valid = fixture(&[SegmentSpec {
            frames: 12,
            sync_samples: vec![0],
        }]);
        append_empty_run(&valid, 12);
        convert_session(&valid.session, &valid.output).unwrap();
        verify_dataset_against_raw_session(&valid.session, &valid.output).unwrap();

        let invalid = fixture(&[SegmentSpec {
            frames: 12,
            sync_samples: vec![0],
        }]);
        append_empty_run(&invalid, 999);
        let error = convert_session(&invalid.session, &invalid.output).unwrap_err();
        assert!(format!("{error:#}").contains("session frame start is not continuous"));
    }

    fn object_keys(value: &impl Serialize) -> Vec<String> {
        let mut keys = serde_json::to_value(value)
            .unwrap()
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        keys.sort();
        keys
    }

    fn ready_refs(values: &[(&str, u32)]) -> Vec<ReadySegmentRef> {
        values
            .iter()
            .map(|(run_id, segment_index)| ReadySegmentRef {
                run_id: (*run_id).into(),
                segment_index: *segment_index,
            })
            .collect()
    }

    fn append_empty_run(fixture: &Fixture, session_frame: u64) {
        let run_dir = fixture.session.join("runs/r0001");
        fs::create_dir_all(run_dir.join("segments")).unwrap();
        let mut run: serde_json::Value =
            read_json(&fixture.session.join("runs/r0000/run.json")).unwrap();
        run["runId"] = json!("r0001");
        run["sessionFrameStart"] = json!(session_frame);
        run["sessionFrameEnd"] = json!(session_frame);
        run["frameCount"] = json!(0);
        run["segmentCount"] = json!(0);
        run["inputEventCount"] = json!(0);
        write_json(&run_dir.join("run.json"), &run).unwrap();
    }

    fn split_second_segment_into_new_run(fixture: &Fixture) {
        let original = fixture.session.join("runs/r0000/segments/00001");
        let new_run = fixture.session.join("runs/r0001");
        fs::create_dir_all(new_run.join("segments")).unwrap();
        let moved = new_run.join("segments/00000");
        fs::rename(&original, &moved).unwrap();

        let frames_path = moved.join("frames.jsonl.zst");
        let mut frames: Vec<RawFrameProjection> = read_zstd_jsonl(&frames_path).unwrap();
        for frame in &mut frames {
            frame.run_id = "r0001".into();
        }
        write_zstd_jsonl(&frames_path, &frames);

        let events_path = moved.join("events.jsonl.zst");
        let mut events: Vec<RawInputEvent> = read_zstd_jsonl(&events_path).unwrap();
        for event in &mut events {
            event.run_id = "r0001".into();
        }
        write_zstd_jsonl(&events_path, &events);

        let meta_path = moved.join("meta.json");
        let mut meta: serde_json::Value = read_json(&meta_path).unwrap();
        meta["run_id"] = json!("r0001");
        meta["segment_index"] = json!(0);
        meta["frames"]["sha256"] = json!(sha256_file(&frames_path).unwrap());
        meta["events"]["sha256"] = json!(sha256_file(&events_path).unwrap());
        write_json(&meta_path, &meta).unwrap();
        rewrite_segment_manifest(&moved);

        let mapping = MappingSnapshot::from_slice(MAPPING).unwrap();
        let run0_meta: RawSegmentMeta =
            read_json(&fixture.session.join("runs/r0000/segments/00000/meta.json")).unwrap();
        write_json(
            &fixture.session.join("runs/r0000/run.json"),
            &json!({
                "schemaVersion": RAW_SEGMENT_SCHEMA,
                "state": RAW_RUN_COMPLETE_STATE,
                "nonTrainable": true,
                "productionCertified": false,
                "sessionId": "fixture-session",
                "runId": "r0000",
                "mappingId": mapping.mapping().mapping_id,
                "mappingVersion": mapping.mapping().version,
                "mappingContentHash": mapping.content_hash(),
                "sessionFrameStart": 0,
                "sessionFrameEnd": 64,
                "frameCount": 64,
                "segmentCount": 1,
                "inputEventCount": run0_meta.events.lines,
                "inputQueueOverflows": 0
            }),
        )
        .unwrap();
        write_json(
            &new_run.join("run.json"),
            &json!({
                "schemaVersion": RAW_SEGMENT_SCHEMA,
                "state": RAW_RUN_COMPLETE_STATE,
                "nonTrainable": true,
                "productionCertified": false,
                "sessionId": "fixture-session",
                "runId": "r0001",
                "mappingId": mapping.mapping().mapping_id,
                "mappingVersion": mapping.mapping().version,
                "mappingContentHash": mapping.content_hash(),
                "sessionFrameStart": 64,
                "sessionFrameEnd": 128,
                "frameCount": 64,
                "segmentCount": 1,
                "inputEventCount": events.len(),
                "inputQueueOverflows": 0
            }),
        )
        .unwrap();
    }

    fn rewrite_segment_manifest(segment_dir: &Path) {
        write_json(
            &segment_dir.join("sha256.json"),
            &BTreeMap::from([
                (
                    "video.mp4",
                    sha256_file(&segment_dir.join("video.mp4")).unwrap(),
                ),
                (
                    "frames.jsonl.zst",
                    sha256_file(&segment_dir.join("frames.jsonl.zst")).unwrap(),
                ),
                (
                    "events.jsonl.zst",
                    sha256_file(&segment_dir.join("events.jsonl.zst")).unwrap(),
                ),
                (
                    "meta.json",
                    sha256_file(&segment_dir.join("meta.json")).unwrap(),
                ),
            ]),
        )
        .unwrap();
    }

    fn sample_fingerprints(path: &Path) -> Vec<(Vec<u8>, u32, i32, bool)> {
        let file = File::open(path).unwrap();
        let size = file.metadata().unwrap().len();
        let mut reader = Mp4Reader::read_header(BufReader::new(file), size).unwrap();
        let track_id = reader.tracks().values().next().unwrap().track_id();
        (1..=reader.tracks()[&track_id].sample_count())
            .map(|sample_id| {
                let sample = reader.read_sample(track_id, sample_id).unwrap().unwrap();
                (
                    sample.bytes.to_vec(),
                    sample.duration,
                    sample.rendering_offset,
                    sample.is_sync,
                )
            })
            .collect()
    }

    #[test]
    fn missing_ready_file_is_rejected() {
        let fixture = fixture(&[SegmentSpec {
            frames: 12,
            sync_samples: vec![0],
        }]);
        fs::remove_file(
            fixture
                .session
                .join("runs/r0000/segments/00000/events.jsonl.zst"),
        )
        .unwrap();
        let error = convert_session(&fixture.session, &fixture.output).unwrap_err();
        assert!(format!("{error:#}").contains("missing events.jsonl.zst"));
        assert!(!fixture.output.exists());
    }

    #[test]
    fn sha256_tampering_is_rejected() {
        let fixture = fixture(&[SegmentSpec {
            frames: 12,
            sync_samples: vec![0],
        }]);
        OpenOptions::new()
            .append(true)
            .open(
                fixture
                    .session
                    .join("runs/r0000/segments/00000/frames.jsonl.zst"),
            )
            .unwrap()
            .write_all(b"tampered")
            .unwrap();
        let error = convert_session(&fixture.session, &fixture.output).unwrap_err();
        assert!(format!("{error:#}").contains("SHA-256 mismatch for frames.jsonl.zst"));
        assert!(!fixture.output.exists());
    }

    #[test]
    fn mapping_identity_mismatch_is_rejected() {
        let fixture = fixture(&[SegmentSpec {
            frames: 12,
            sync_samples: vec![0],
        }]);
        let session_path = fixture.session.join("session.json");
        let mut session: serde_json::Value = read_json(&session_path).unwrap();
        session["mapping"]["version"] = json!("wrong-version");
        write_json(&session_path, &session).unwrap();
        let error = convert_session(&fixture.session, &fixture.output).unwrap_err();
        assert!(format!("{error:#}").contains("session.json mapping version mismatch"));
        assert!(!fixture.output.exists());
    }

    #[test]
    fn unsafe_session_id_and_output_inside_source_are_rejected() {
        let unsafe_id_fixture = fixture(&[SegmentSpec {
            frames: 12,
            sync_samples: vec![0],
        }]);
        let session_path = unsafe_id_fixture.session.join("session.json");
        let mut session: serde_json::Value = read_json(&session_path).unwrap();
        session["session_id"] = json!("..\\escaped");
        write_json(&session_path, &session).unwrap();
        let error =
            convert_session(&unsafe_id_fixture.session, &unsafe_id_fixture.output).unwrap_err();
        assert!(format!("{error:#}").contains("safe path component"));
        assert!(!unsafe_id_fixture.output.exists());

        let nested_fixture = fixture(&[SegmentSpec {
            frames: 12,
            sync_samples: vec![0],
        }]);
        let nested_output = nested_fixture.session.join("derived-dataset");
        let error = convert_session(&nested_fixture.session, &nested_output).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("overlap"), "{message}");
        assert!(!nested_output.exists());

        let ancestor_fixture = fixture(&[SegmentSpec {
            frames: 12,
            sync_samples: vec![0],
        }]);
        let ancestor_output = ancestor_fixture.session.parent().unwrap();
        let error = advance_incremental(
            &ancestor_fixture.session,
            ancestor_output,
            &ready_refs(&[("r0000", 0)]),
            None,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("overlap"));
    }

    #[test]
    fn non_sync_four_second_boundary_is_rejected_without_output() {
        let fixture = fixture(&[SegmentSpec {
            frames: 121,
            sync_samples: vec![0],
        }]);
        let error = convert_session(&fixture.session, &fixture.output).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("gframe 120 is not an H.264 sync sample"));
        assert!(message.contains("refusing lossy or fake conversion"));
        assert!(!fixture.output.exists());
    }

    #[test]
    fn v2_ready_session_validates_exact_catalog_snapshots() {
        let fixture = fixture_v2(&[SegmentSpec {
            frames: 12,
            sync_samples: vec![0],
        }]);

        let raw_session: serde_json::Value =
            read_json(&fixture.session.join("session.json")).unwrap();
        let raw_segment: serde_json::Value =
            read_json(&fixture.session.join("runs/r0000/segments/00000/meta.json")).unwrap();
        assert!(raw_session["catalog"].get("provenance").is_none());
        assert!(raw_segment["catalog"].get("provenance").is_none());

        let converted = convert_session(&fixture.session, &fixture.output).unwrap();
        assert_eq!((converted.frame_count, converted.chunk_count), (12, 1));
        let session_output = fixture.output.join("synthetic_fixture/fixture-session");
        let metadata: SessionMetadata = read_json(&session_output.join("metadata.json")).unwrap();
        let metadata_json: serde_json::Value =
            read_json(&session_output.join("metadata.json")).unwrap();
        let chunk_json: serde_json::Value =
            read_json(&session_output.join("c0000/meta.json")).unwrap();
        assert_eq!(
            metadata.provenance.raw_schema_version,
            RAW_SESSION_SCHEMA_V2
        );
        assert!(metadata.catalog.is_none());
        assert!(metadata_json["catalog"].is_null());
        assert!(chunk_json["catalog"].is_null());
        verify_dataset(&fixture.output).unwrap();
        verify_dataset_against_raw_session(&fixture.session, &fixture.output).unwrap();
    }

    #[test]
    fn new_outputs_use_dm_2_while_catalogless_dm_1_remains_readable() {
        let fixture = fixture_v2(&[SegmentSpec {
            frames: 12,
            sync_samples: vec![0],
        }]);
        convert_session(&fixture.session, &fixture.output).unwrap();
        let session_output = fixture.output.join("synthetic_fixture/fixture-session");
        let metadata: serde_json::Value = read_json(&session_output.join("metadata.json")).unwrap();
        let chunk: serde_json::Value = read_json(&session_output.join("c0000/meta.json")).unwrap();
        assert_eq!(metadata["schema_version"], json!("dm-2"));
        assert_eq!(chunk["schema_version"], json!("dm-2"));
        assert!(metadata.get("catalog").is_some());
        assert!(chunk.get("catalog").is_some());

        rewrite_output_schema_and_catalog(&fixture, 1, "dm-1", None);
        verify_dataset(&fixture.output).expect("a pre-catalog dm-1 dataset must remain readable");
    }

    #[test]
    fn dataset_schema_version_strictly_arbitrates_the_catalog_key() {
        let fixture = fixture_v2(&[SegmentSpec {
            frames: 12,
            sync_samples: vec![0],
        }]);
        stamp_catalog_provenance(&fixture, 1, "compiled");
        convert_session(&fixture.session, &fixture.output).unwrap();

        rewrite_output_schema_and_catalog(&fixture, 1, "dm-2", None);
        let error = verify_dataset(&fixture.output).unwrap_err();
        assert!(
            format!("{error:#}").contains("dm-2 dataset metadata must contain top-level catalog")
        );

        rewrite_output_schema_and_catalog(
            &fixture,
            1,
            "dm-1",
            Some(json!({ "provenance": "compiled" })),
        );
        let error = verify_dataset(&fixture.output).unwrap_err();
        assert!(
            format!("{error:#}")
                .contains("dm-1 dataset metadata must not contain top-level catalog")
        );

        rewrite_output_schema_and_catalog(&fixture, 1, "dm-99", None);
        let error = verify_dataset(&fixture.output).unwrap_err();
        assert!(format!("{error:#}").contains("unsupported dataset schema dm-99"));

        rewrite_output_schema_and_catalog(
            &fixture,
            1,
            "dm-2",
            Some(json!({ "provenance": "compiled" })),
        );
        downgrade_data_model_json_to_dm_1(
            &fixture
                .output
                .join("synthetic_fixture/fixture-session/c0000/meta.json"),
        );
        let error = verify_dataset(&fixture.output).unwrap_err();
        assert!(format!("{error:#}").contains("chunk schema does not match session metadata"));

        rewrite_output_schema_and_catalog(
            &fixture,
            1,
            "dm-2",
            Some(json!({ "provenance": "compiled", "extra": true })),
        );
        let error = verify_dataset(&fixture.output).unwrap_err();
        assert!(format!("{error:#}").contains("unknown field `extra`"));
    }

    #[test]
    fn catalog_provenance_round_trips_to_session_and_chunk_outputs() {
        let fixtures = [
            (
                fixture_v2(&[SegmentSpec {
                    frames: 12,
                    sync_samples: vec![0],
                }]),
                "compiled",
            ),
            (
                fixture_audio(&[SegmentSpec {
                    frames: 12,
                    sync_samples: vec![0],
                }]),
                "delivered",
            ),
        ];

        for (fixture, provenance) in fixtures {
            stamp_catalog_provenance(&fixture, 1, provenance);
            let expected_catalog = json!({ "provenance": provenance });

            convert_session(&fixture.session, &fixture.output).unwrap();
            let session_output = fixture.output.join("synthetic_fixture/fixture-session");
            let metadata: serde_json::Value =
                read_json(&session_output.join("metadata.json")).unwrap();
            let chunk: serde_json::Value =
                read_json(&session_output.join("c0000/meta.json")).unwrap();
            assert_eq!(metadata["catalog"], expected_catalog);
            assert_eq!(chunk["catalog"], expected_catalog);
            verify_dataset(&fixture.output).unwrap();
            verify_dataset_against_raw_session(&fixture.session, &fixture.output).unwrap();
        }
    }

    #[test]
    fn raw_binding_rejects_changed_catalog_provenance_after_conversion() {
        let fixture = fixture_v2(&[SegmentSpec {
            frames: 12,
            sync_samples: vec![0],
        }]);
        stamp_catalog_provenance(&fixture, 1, "compiled");
        convert_session(&fixture.session, &fixture.output).unwrap();
        verify_dataset(&fixture.output).unwrap();

        stamp_catalog_provenance(&fixture, 1, "delivered");
        let error =
            verify_dataset_against_raw_session(&fixture.session, &fixture.output).unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("finalized DataModel output catalog does not match current RAW"),
            "{message}"
        );
    }

    #[test]
    fn incremental_cursor_rejects_changed_catalog_provenance() {
        let fixture = fixture_v2(&[
            SegmentSpec {
                frames: 120,
                sync_samples: vec![0],
            },
            SegmentSpec {
                frames: 10,
                sync_samples: vec![0],
            },
        ]);
        stamp_catalog_provenance(&fixture, 2, "compiled");
        let refs = ready_refs(&[("r0000", 0)]);
        let first = advance_incremental(&fixture.session, &fixture.output, &refs, None).unwrap();
        assert_eq!(first.published_chunks, 1);

        stamp_catalog_provenance(&fixture, 2, "delivered");
        let error =
            advance_incremental(&fixture.session, &fixture.output, &refs, None).unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("incremental cursor source identity mismatch"),
            "{message}"
        );
    }

    #[test]
    fn legacy_cursor_rebuilds_wall_clock_chunks_from_raw() {
        let fixture = fixture(&[SegmentSpec {
            frames: 130,
            sync_samples: vec![0, 120],
        }]);
        stamp_run_wall_clock(&fixture);
        let mut wall_clock = fixture_wall_clock_context();
        // Mirrors a v28 journal row after v29 adds nullable anchor_at_utc:
        // the sealed run manifest is allowed to supply the previously unknown UTC.
        wall_clock.anchor_at_utc = None;
        let refs = ready_refs(&[("r0000", 0)]);
        let first =
            advance_incremental(&fixture.session, &fixture.output, &refs, Some(&wall_clock))
                .unwrap();
        assert_eq!(first.published_chunks, 1);

        let cursor_path = fixture.output.join(".incremental-state.json");
        let mut cursor: serde_json::Value = read_json(&cursor_path).unwrap();
        cursor["schema_version"] = json!("dm-data-demo-incremental-cursor-2");
        write_json(&cursor_path, &cursor).unwrap();
        let chunk = fixture
            .output
            .join("synthetic_fixture/fixture-session/c0000");
        downgrade_data_model_json_to_dm_1(&chunk.join("meta.json"));
        let frames_path = chunk.join("p0.jsonl");
        let mut rows: Vec<DataDemoFrame> = read_jsonl(&frames_path).unwrap();
        for row in &mut rows {
            row.t_utc_est_us = None;
        }
        write_jsonl(&frames_path, &rows).unwrap();

        let rebuilt =
            advance_incremental(&fixture.session, &fixture.output, &refs, Some(&wall_clock))
                .unwrap();
        assert_eq!(rebuilt, first);
        let rows: Vec<DataDemoFrame> = read_jsonl(&frames_path).unwrap();
        assert!(rows.iter().all(|row| row.t_utc_est_us.is_some()));
    }

    #[test]
    fn finalize_rebuilds_legacy_cursor_chunks_without_an_advance() {
        let fixture = fixture_v2(&[SegmentSpec {
            frames: 130,
            sync_samples: vec![0, 120],
        }]);
        stamp_catalog_provenance(&fixture, 1, "compiled");
        stamp_run_wall_clock(&fixture);
        let refs = ready_refs(&[("r0000", 0)]);
        let first = advance_incremental(
            &fixture.session,
            &fixture.output,
            &refs,
            Some(&fixture_wall_clock_context()),
        )
        .unwrap();
        assert_eq!(first.published_chunks, 1);

        let cursor_path = fixture.output.join(".incremental-state.json");
        let mut cursor: serde_json::Value = read_json(&cursor_path).unwrap();
        cursor["schema_version"] = json!("dm-data-demo-incremental-cursor-2");
        write_json(&cursor_path, &cursor).unwrap();
        let session_output = fixture.output.join("synthetic_fixture/fixture-session");
        let frames_path = session_output.join("c0000/p0.jsonl");
        let mut rows: Vec<DataDemoFrame> = read_jsonl(&frames_path).unwrap();
        for row in &mut rows {
            row.t_utc_est_us = None;
        }
        write_jsonl(&frames_path, &rows).unwrap();
        downgrade_data_model_json_to_dm_1(&session_output.join("c0000/meta.json"));

        let finalized = finalize_incremental(&fixture.session, &fixture.output, &refs).unwrap();
        assert_eq!((finalized.frame_count, finalized.chunk_count), (130, 2));
        assert!(!cursor_path.exists());
        verify_dataset_against_raw_session(&fixture.session, &fixture.output).unwrap();
        let rows: Vec<DataDemoFrame> = read_jsonl(&frames_path).unwrap();
        assert!(rows.iter().all(|row| row.t_utc_est_us.is_some()));
        let chunk: serde_json::Value = read_json(&session_output.join("c0000/meta.json")).unwrap();
        assert_eq!(chunk["catalog"], json!({ "provenance": "compiled" }));

        let repeated = finalize_incremental(&fixture.session, &fixture.output, &refs).unwrap();
        assert_eq!(repeated, finalized);
    }

    #[test]
    fn finalize_upgrades_exact_legacy_chunks_without_a_usable_cursor() {
        for corrupt_cursor in [false, true] {
            let fixture = fixture_v2(&[SegmentSpec {
                frames: 130,
                sync_samples: vec![0, 120],
            }]);
            stamp_catalog_provenance(&fixture, 1, "compiled");
            stamp_run_wall_clock(&fixture);
            let refs = ready_refs(&[("r0000", 0)]);
            advance_incremental(
                &fixture.session,
                &fixture.output,
                &refs,
                Some(&fixture_wall_clock_context()),
            )
            .unwrap();
            let cursor_path = fixture.output.join(".incremental-state.json");
            if corrupt_cursor {
                fs::write(&cursor_path, b"{not-json").unwrap();
            } else {
                fs::remove_file(&cursor_path).unwrap();
            }
            let chunk = fixture
                .output
                .join("synthetic_fixture/fixture-session/c0000");
            downgrade_data_model_json_to_dm_1(&chunk.join("meta.json"));
            let frames_path = chunk.join("p0.jsonl");
            let mut rows: Vec<DataDemoFrame> = read_jsonl(&frames_path).unwrap();
            for row in &mut rows {
                row.t_utc_est_us = None;
            }
            write_jsonl(&frames_path, &rows).unwrap();

            let finalized = finalize_incremental(&fixture.session, &fixture.output, &refs).unwrap();
            assert_eq!((finalized.frame_count, finalized.chunk_count), (130, 2));
            verify_dataset_against_raw_session(&fixture.session, &fixture.output).unwrap();
        }
    }

    #[test]
    fn v2_cursor_does_not_authorize_discarding_other_chunk_drift() {
        let fixture = fixture_v2(&[SegmentSpec {
            frames: 130,
            sync_samples: vec![0, 120],
        }]);
        stamp_catalog_provenance(&fixture, 1, "compiled");
        stamp_run_wall_clock(&fixture);
        let refs = ready_refs(&[("r0000", 0)]);
        advance_incremental(
            &fixture.session,
            &fixture.output,
            &refs,
            Some(&fixture_wall_clock_context()),
        )
        .unwrap();
        let cursor_path = fixture.output.join(".incremental-state.json");
        let mut cursor: serde_json::Value = read_json(&cursor_path).unwrap();
        cursor["schema_version"] = json!("dm-data-demo-incremental-cursor-2");
        write_json(&cursor_path, &cursor).unwrap();
        let chunk = fixture
            .output
            .join("synthetic_fixture/fixture-session/c0000");
        downgrade_data_model_json_to_dm_1(&chunk.join("meta.json"));
        let frames_path = chunk.join("p0.jsonl");
        let mut rows: Vec<DataDemoFrame> = read_jsonl(&frames_path).unwrap();
        for row in &mut rows {
            row.t_utc_est_us = None;
        }
        rows[0].qpc += 1;
        rows[0].qpc_end += 1;
        write_jsonl(&frames_path, &rows).unwrap();
        let before = sha256_file(&frames_path).unwrap();

        finalize_incremental(&fixture.session, &fixture.output, &refs).unwrap_err();
        assert_eq!(sha256_file(&frames_path).unwrap(), before);
    }

    #[test]
    fn damaged_v2_cursor_identity_does_not_block_exact_chunk_recovery() {
        let fixture = fixture_v2(&[SegmentSpec {
            frames: 130,
            sync_samples: vec![0, 120],
        }]);
        stamp_catalog_provenance(&fixture, 1, "compiled");
        stamp_run_wall_clock(&fixture);
        let refs = ready_refs(&[("r0000", 0)]);
        advance_incremental(
            &fixture.session,
            &fixture.output,
            &refs,
            Some(&fixture_wall_clock_context()),
        )
        .unwrap();
        let cursor_path = fixture.output.join(".incremental-state.json");
        let mut cursor: serde_json::Value = read_json(&cursor_path).unwrap();
        cursor["schema_version"] = json!("dm-data-demo-incremental-cursor-2");
        cursor["session_id"] = json!("damaged-session");
        cursor["mapping_content_hash"] = json!("damaged-mapping");
        write_json(&cursor_path, &cursor).unwrap();
        let chunk = fixture
            .output
            .join("synthetic_fixture/fixture-session/c0000");
        downgrade_data_model_json_to_dm_1(&chunk.join("meta.json"));
        let frames_path = chunk.join("p0.jsonl");
        let mut rows: Vec<DataDemoFrame> = read_jsonl(&frames_path).unwrap();
        for row in &mut rows {
            row.t_utc_est_us = None;
        }
        write_jsonl(&frames_path, &rows).unwrap();

        let recovered = advance_incremental(
            &fixture.session,
            &fixture.output,
            &refs,
            Some(&fixture_wall_clock_context()),
        )
        .unwrap();
        assert_eq!(recovered.published_chunks, 1);
        let cursor: serde_json::Value = read_json(&cursor_path).unwrap();
        assert_eq!(
            cursor["schema_version"],
            json!("dm-data-demo-incremental-cursor-3")
        );
    }

    #[test]
    fn finalize_rebuilds_an_exact_legacy_completed_catalog_output() {
        let fixture = fixture_v2(&[SegmentSpec {
            frames: 130,
            sync_samples: vec![0, 120],
        }]);
        stamp_catalog_provenance(&fixture, 1, "delivered");
        stamp_run_wall_clock(&fixture);
        let refs = ready_refs(&[("r0000", 0)]);
        convert_session(&fixture.session, &fixture.output).unwrap();
        let session_output = fixture.output.join("synthetic_fixture/fixture-session");
        downgrade_data_model_jsonl_to_dm_1(&fixture.output.join("metadata.jsonl"));
        downgrade_data_model_json_to_dm_1(&session_output.join("metadata.json"));
        for chunk in 0..2 {
            downgrade_data_model_json_to_dm_1(
                &session_output.join(format!("c{chunk:04}/meta.json")),
            );
        }
        verify_dataset(&fixture.output).unwrap();

        let finalized = finalize_incremental(&fixture.session, &fixture.output, &refs).unwrap();
        assert_eq!((finalized.frame_count, finalized.chunk_count), (130, 2));
        verify_dataset_against_raw_session(&fixture.session, &fixture.output).unwrap();
        let metadata: serde_json::Value = read_json(&session_output.join("metadata.json")).unwrap();
        assert_eq!(metadata["catalog"], json!({ "provenance": "delivered" }));

        let repeated = finalize_incremental(&fixture.session, &fixture.output, &refs).unwrap();
        assert_eq!(repeated, finalized);
    }

    #[test]
    fn finalize_upgrades_a_preversioned_dm_1_catalog_null_output() {
        let fixture = fixture_v2(&[SegmentSpec {
            frames: 12,
            sync_samples: vec![0],
        }]);
        let refs = ready_refs(&[("r0000", 0)]);
        convert_session(&fixture.session, &fixture.output).unwrap();
        rewrite_output_schema_and_catalog(&fixture, 1, "dm-1", Some(json!(null)));
        assert!(
            format!("{:#}", verify_dataset(&fixture.output).unwrap_err())
                .contains("dm-1 dataset metadata must not contain top-level catalog")
        );

        finalize_incremental(&fixture.session, &fixture.output, &refs).unwrap();
        verify_dataset_against_raw_session(&fixture.session, &fixture.output).unwrap();
        let metadata: serde_json::Value = read_json(
            &fixture
                .output
                .join("synthetic_fixture/fixture-session/metadata.json"),
        )
        .unwrap();
        assert_eq!(metadata["schema_version"], json!("dm-2"));
        assert!(metadata["catalog"].is_null());
    }

    #[test]
    fn finalize_upgrades_a_true_catalogless_dm_1_completed_output() {
        let fixture = fixture_v2(&[SegmentSpec {
            frames: 12,
            sync_samples: vec![0],
        }]);
        let refs = ready_refs(&[("r0000", 0)]);
        convert_session(&fixture.session, &fixture.output).unwrap();
        rewrite_output_schema_and_catalog(&fixture, 1, "dm-1", None);
        verify_dataset(&fixture.output).unwrap();

        finalize_incremental(&fixture.session, &fixture.output, &refs).unwrap();
        verify_dataset_against_raw_session(&fixture.session, &fixture.output).unwrap();
        let metadata: serde_json::Value = read_json(
            &fixture
                .output
                .join("synthetic_fixture/fixture-session/metadata.json"),
        )
        .unwrap();
        assert_eq!(metadata["schema_version"], json!("dm-2"));
        assert!(metadata["catalog"].is_null());
    }

    #[test]
    fn finalize_preserves_a_legacy_catalog_output_with_other_raw_drift() {
        let fixture = fixture_v2(&[SegmentSpec {
            frames: 130,
            sync_samples: vec![0, 120],
        }]);
        stamp_catalog_provenance(&fixture, 1, "compiled");
        stamp_run_wall_clock(&fixture);
        let refs = ready_refs(&[("r0000", 0)]);
        convert_session(&fixture.session, &fixture.output).unwrap();
        let session_output = fixture.output.join("synthetic_fixture/fixture-session");
        downgrade_data_model_jsonl_to_dm_1(&fixture.output.join("metadata.jsonl"));
        downgrade_data_model_json_to_dm_1(&session_output.join("metadata.json"));
        for chunk in 0..2 {
            downgrade_data_model_json_to_dm_1(
                &session_output.join(format!("c{chunk:04}/meta.json")),
            );
            let frames_path = session_output.join(format!("c{chunk:04}/p0.jsonl"));
            let mut rows: Vec<DataDemoFrame> = read_jsonl(&frames_path).unwrap();
            for row in &mut rows {
                row.t_utc_est_us = row.t_utc_est_us.map(|value| value + 1);
            }
            write_jsonl(&frames_path, &rows).unwrap();
        }
        verify_dataset(&fixture.output).unwrap();
        let before = sha256_file(&session_output.join("c0000/p0.jsonl")).unwrap();

        finalize_incremental(&fixture.session, &fixture.output, &refs).unwrap_err();
        assert_eq!(
            sha256_file(&session_output.join("c0000/p0.jsonl")).unwrap(),
            before,
            "a rejected legacy migration must preserve the original output"
        );
    }

    #[test]
    fn incremental_rejects_unknown_catalog_provenance_before_publishing() {
        let fixture = fixture_v2(&[SegmentSpec {
            frames: 120,
            sync_samples: vec![0],
        }]);
        stamp_catalog_provenance(&fixture, 1, "future");

        let error = advance_incremental(
            &fixture.session,
            &fixture.output,
            &ready_refs(&[("r0000", 0)]),
            None,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("unsupported catalog provenance"));
        assert!(!fixture.output.exists());
    }

    #[test]
    fn raw_binding_rejects_a_tampered_wall_clock_projection() {
        let fixture = fixture(&[SegmentSpec {
            frames: 120,
            sync_samples: vec![0],
        }]);
        stamp_run_wall_clock(&fixture);
        convert_session(&fixture.session, &fixture.output).unwrap();
        let frames_path = fixture
            .output
            .join("synthetic_fixture/fixture-session/c0000/p0.jsonl");
        let mut rows: Vec<DataDemoFrame> = read_jsonl(&frames_path).unwrap();
        for row in &mut rows {
            row.t_utc_est_us = None;
        }
        write_jsonl(&frames_path, &rows).unwrap();
        verify_dataset(&fixture.output).unwrap();

        let error =
            verify_dataset_against_raw_session(&fixture.session, &fixture.output).unwrap_err();
        assert!(format!("{error:#}").contains("wall-clock projection"));
    }

    #[test]
    fn v2_exact_catalog_snapshot_tampering_is_rejected() {
        let fixture = fixture_v2(&[SegmentSpec {
            frames: 12,
            sync_samples: vec![0],
        }]);
        OpenOptions::new()
            .append(true)
            .open(fixture.session.join("device-profile.json"))
            .unwrap()
            .write_all(b"tampered")
            .unwrap();

        let error = convert_session(&fixture.session, &fixture.output).unwrap_err();
        assert!(
            format!("{error:#}")
                .contains("catalog snapshot device-profile.json exact-byte SHA-256 mismatch")
        );
        assert!(!fixture.output.exists());
    }

    #[test]
    fn v2_session_catalog_identity_tampering_is_rejected() {
        let fixture = fixture_v2(&[SegmentSpec {
            frames: 12,
            sync_samples: vec![0],
        }]);
        let session_path = fixture.session.join("session.json");
        let mut session: serde_json::Value = read_json(&session_path).unwrap();
        session["catalog"]["device"]["version"] = json!("wrong-version");
        write_json(&session_path, &session).unwrap();

        let error = convert_session(&fixture.session, &fixture.output).unwrap_err();
        assert!(
            format!("{error:#}")
                .contains("catalog.json device identity does not match session.json")
        );
        assert!(!fixture.output.exists());
    }

    #[test]
    fn v2_segment_catalog_identity_tampering_is_rejected() {
        let fixture = fixture_v2(&[SegmentSpec {
            frames: 12,
            sync_samples: vec![0],
        }]);
        let segment_dir = fixture.session.join("runs/r0000/segments/00000");
        let meta_path = segment_dir.join("meta.json");
        let mut meta: serde_json::Value = read_json(&meta_path).unwrap();
        meta["catalog"]["mapping"]["version"] = json!("wrong-version");
        write_json(&meta_path, &meta).unwrap();
        rewrite_segment_meta_hash(&segment_dir);

        let error = convert_session(&fixture.session, &fixture.output).unwrap_err();
        assert!(
            format!("{error:#}")
                .contains("segment meta catalog identity does not match session.json")
        );
        assert!(!fixture.output.exists());
    }

    #[test]
    fn v2_view_scale_constants_derive_angle_columns_and_verify() {
        let fixture = fixture_v2_with_view_scale(
            &[SegmentSpec {
                frames: 12,
                sync_samples: vec![0],
            }],
            Some(json!({
                "applicable": true,
                "counts_per_360": 16364.0,
                "vertical_scale_ratio": 1.0,
                // 客户端政策字段必须被 converter 忽略而不是拒绝。
                "ads_states": [{"state_id": "hip", "zoom_scale_ratio": null}]
            })),
        );
        let converted = convert_session(&fixture.session, &fixture.output).unwrap();
        assert_eq!((converted.frame_count, converted.chunk_count), (12, 1));
        let session_dir = fixture
            .output
            .join("synthetic_fixture")
            .join("fixture-session");
        let metadata: SessionMetadata = read_json(&session_dir.join("metadata.json")).unwrap();
        assert_eq!(
            metadata.view_scale,
            Some(ViewScaleRef {
                norm: view_scale::VIEW_SCALE_NORM.into(),
                counts_per_360: 16364.0,
                vertical_scale_ratio: 1.0,
            })
        );
        let rows: Vec<DataDemoFrame> = read_jsonl(&session_dir.join("c0000/p0.jsonl")).unwrap();
        let mut moved = 0;
        for row in &rows {
            assert!(row.view_scale_valid);
            let (raw_dx, raw_dy) = pointer_raw_sum(&row.events);
            assert_eq!(
                row.look_yaw_delta_rad,
                Some(view_scale::yaw_delta_rad(i64::from(raw_dx), 16364.0))
            );
            assert_eq!(
                row.look_pitch_delta_rad,
                Some(view_scale::pitch_delta_rad(i64::from(raw_dy), 16364.0, 1.0))
            );
            if raw_dx != 0 || raw_dy != 0 {
                moved += 1;
            }
        }
        // 夹具帧 1 携带 mouse_move(120, -14)：断言不是在全零输入上空转，
        // 且符号语义正确（+yaw = 右移，dy 负 = 上移 = +pitch）。
        assert!(moved >= 1);
        let sample = rows
            .iter()
            .find(|row| row.look_yaw_delta_rad != Some(0.0))
            .expect("fixture has a frame with pointer motion");
        assert!(sample.look_yaw_delta_rad.unwrap() > 0.0);
        assert!(sample.look_pitch_delta_rad.unwrap() > 0.0);
        let pairs: Vec<FramePair> = read_jsonl(&session_dir.join("framepairs.jsonl")).unwrap();
        assert!(!pairs.is_empty());
        for pair in &pairs {
            let frame = rows
                .iter()
                .find(|frame| frame.gframe == pair.frame_t)
                .expect("framepair frameT has a frame");
            assert!(pair.action_t.view_scale_valid);
            assert_eq!(pair.action_t.look_yaw_delta_rad, frame.look_yaw_delta_rad);
            assert_eq!(
                pair.action_t.look_pitch_delta_rad,
                frame.look_pitch_delta_rad
            );
        }
        let datasheet_text = fs::read_to_string(fixture.output.join("DATASHEET.md")).unwrap();
        assert!(datasheet_text.contains(view_scale::VIEW_SCALE_NORM));
        verify_dataset(&fixture.output).unwrap();
        verify_dataset_against_raw_session(&fixture.session, &fixture.output).unwrap();
    }

    #[test]
    fn v2_view_scale_mapping_mode_null_constants_stay_uncalibrated() {
        // applicable=true 但常数未定版（H-1 前所有 dev 目录的形态）：合法，
        // 角度整列留白且显式序列化 null。
        let fixture = fixture_v2_with_view_scale(
            &[SegmentSpec {
                frames: 12,
                sync_samples: vec![0],
            }],
            Some(json!({
                "applicable": true,
                "counts_per_360": null,
                "vertical_scale_ratio": null
            })),
        );
        convert_session(&fixture.session, &fixture.output).unwrap();
        let session_dir = fixture
            .output
            .join("synthetic_fixture")
            .join("fixture-session");
        let metadata_json: serde_json::Value =
            read_json(&session_dir.join("metadata.json")).unwrap();
        assert!(metadata_json.get("view_scale").is_some());
        assert!(metadata_json["view_scale"].is_null());
        let rows: Vec<DataDemoFrame> = read_jsonl(&session_dir.join("c0000/p0.jsonl")).unwrap();
        assert!(rows.iter().all(|row| {
            !row.view_scale_valid
                && row.look_yaw_delta_rad.is_none()
                && row.look_pitch_delta_rad.is_none()
        }));
        // 行内也必须是显式 null（缺席可见），不是省略键。
        let first_line = fs::read_to_string(session_dir.join("c0000/p0.jsonl")).unwrap();
        let first_row: serde_json::Value =
            serde_json::from_str(first_line.lines().next().unwrap()).unwrap();
        for key in ["look_yaw_delta_rad", "look_pitch_delta_rad"] {
            assert!(first_row.get(key).is_some());
            assert!(first_row[key].is_null());
        }
        assert_eq!(first_row["view_scale_valid"], json!(false));
        let datasheet_text = fs::read_to_string(fixture.output.join("DATASHEET.md")).unwrap();
        assert!(datasheet_text.contains("not calibrated for this session"));
        verify_dataset(&fixture.output).unwrap();
        verify_dataset_against_raw_session(&fixture.session, &fixture.output).unwrap();
    }

    #[test]
    fn v2_view_scale_empty_ads_states_blank_angle_columns() {
        // ads_states 护栏（docs/60 Q2 / docs/64 §3.2）：常数齐全但 ads_states 为空
        // （或缺席）时角度列必须整列留白——腰射常数换算 ADS 时段是系统性错误，
        // 此前无门可拦。护栏把"错数据"降级为"缺数据"。
        for ads_states in [json!(null), json!([])] {
            let mut view_scale = json!({
                "applicable": true,
                "counts_per_360": 16364.0,
                "vertical_scale_ratio": 1.0
            });
            if !ads_states.is_null() {
                view_scale["ads_states"] = ads_states;
            }
            let fixture = fixture_v2_with_view_scale(
                &[SegmentSpec {
                    frames: 12,
                    sync_samples: vec![0],
                }],
                Some(view_scale),
            );
            convert_session(&fixture.session, &fixture.output).unwrap();
            let session_dir = fixture
                .output
                .join("synthetic_fixture")
                .join("fixture-session");
            let metadata_json: serde_json::Value =
                read_json(&session_dir.join("metadata.json")).unwrap();
            assert!(metadata_json["view_scale"].is_null());
            let rows: Vec<DataDemoFrame> = read_jsonl(&session_dir.join("c0000/p0.jsonl")).unwrap();
            assert!(rows.iter().all(|row| {
                !row.view_scale_valid
                    && row.look_yaw_delta_rad.is_none()
                    && row.look_pitch_delta_rad.is_none()
            }));
            let datasheet_text = fs::read_to_string(fixture.output.join("DATASHEET.md")).unwrap();
            assert!(datasheet_text.contains("not calibrated for this session"));
            verify_dataset(&fixture.output).unwrap();
            verify_dataset_against_raw_session(&fixture.session, &fixture.output).unwrap();
        }
    }

    #[test]
    fn v2_view_scale_invalid_constants_are_rejected() {
        let fixture = fixture_v2_with_view_scale(
            &[SegmentSpec {
                frames: 12,
                sync_samples: vec![0],
            }],
            Some(json!({
                "applicable": true,
                "counts_per_360": -5.0,
                "vertical_scale_ratio": 1.0
            })),
        );
        let error = convert_session(&fixture.session, &fixture.output).unwrap_err();
        assert!(
            format!("{error:#}")
                .contains("view_scale.counts_per_360 is not a positive finite number")
        );
        assert!(!fixture.output.exists());
    }

    #[test]
    fn verify_rejects_tampered_angle_column() {
        let fixture = fixture_v2_with_view_scale(
            &[SegmentSpec {
                frames: 12,
                sync_samples: vec![0],
            }],
            Some(json!({
                "applicable": true,
                "counts_per_360": 16364.0,
                "vertical_scale_ratio": 1.0,
                "ads_states": [{"state_id": "hip", "zoom_scale_ratio": null}]
            })),
        );
        convert_session(&fixture.session, &fixture.output).unwrap();
        let chunk_path = fixture
            .output
            .join("synthetic_fixture/fixture-session/c0000/p0.jsonl");
        let tampered = fs::read_to_string(&chunk_path)
            .unwrap()
            .lines()
            .enumerate()
            .map(|(index, line)| {
                if index == 0 {
                    let mut row: serde_json::Value = serde_json::from_str(line).unwrap();
                    row["look_yaw_delta_rad"] = json!(1.0);
                    serde_json::to_string(&row).unwrap()
                } else {
                    line.to_owned()
                }
            })
            .map(|line| line + "\n")
            .collect::<String>();
        fs::write(&chunk_path, tampered).unwrap();
        let error = verify_dataset(&fixture.output).unwrap_err();
        assert!(
            format!("{error:#}").contains(
                "look_yaw/pitch_delta_rad do not match recomputation from events under the declared view_scale"
            )
        );
    }

    #[test]
    fn audio_ready_converts_to_lossless_flac_and_incremental_replays_identically() {
        let specs = [
            SegmentSpec {
                frames: 100,
                sync_samples: vec![0],
            },
            SegmentSpec {
                frames: 140,
                sync_samples: vec![0, 20],
            },
        ];
        let fixture = fixture_audio(&specs);
        convert_session(&fixture.session, &fixture.output).unwrap();
        verify_dataset(&fixture.output).unwrap();
        verify_dataset_against_raw_session(&fixture.session, &fixture.output).unwrap();

        let session_dir = fixture.output.join("synthetic_fixture/fixture-session");
        for index in 0..2 {
            let chunk = session_dir.join(format!("c{index:04}"));
            let meta: ChunkMeta = read_json(&chunk.join("meta.json")).unwrap();
            let audio = meta.audio.unwrap();
            assert_eq!(audio.state, SegmentAudioState::Ready);
            assert!(audio.present);
            assert_eq!(audio.frames, Some(120 * 1_600));
            verify_flac(&chunk.join("p0.flac"), 120 * 1_600, None).unwrap();
        }
        let metadata: SessionMetadata = read_json(&session_dir.join("metadata.json")).unwrap();
        assert_eq!(metadata.audio.unwrap().ready_chunks, 2);

        let incremental_output = fixture._temp.path().join("incremental-dataset");
        let refs = ready_refs(&[("r0000", 0), ("r0000", 1)]);
        let report =
            advance_incremental(&fixture.session, &incremental_output, &refs, None).unwrap();
        assert_eq!((report.published_chunks, report.pending_frames), (2, 0));
        assert_eq!(
            advance_incremental(&fixture.session, &incremental_output, &refs, None).unwrap(),
            report
        );
        let incremental_session = incremental_output.join("synthetic_fixture/fixture-session");
        for index in 0..2 {
            assert_eq!(
                sha256_file(&session_dir.join(format!("c{index:04}/p0.flac"))).unwrap(),
                sha256_file(&incremental_session.join(format!("c{index:04}/p0.flac"))).unwrap(),
            );
        }
    }

    #[test]
    fn degraded_audio_keeps_video_and_marks_flac_unpublishable() {
        let fixture = fixture_audio(&[SegmentSpec {
            frames: 12,
            sync_samples: vec![0],
        }]);
        let segment = fixture.session.join("runs/r0000/segments/00000");
        let meta_path = segment.join("meta.json");
        let mut meta: serde_json::Value = read_json(&meta_path).unwrap();
        meta["audio"]["state"] = json!("degraded");
        meta["audio"]["failure_code"] = json!("AUDIO_BOUNDED_GAP_FILLED");
        meta["audio"]["health"]["silence_filled_spans"] = json!([{"start_frame": 0, "frames": 1}]);
        write_json(&meta_path, &meta).unwrap();
        rewrite_segment_meta_hash(&segment);

        convert_session(&fixture.session, &fixture.output).unwrap();
        let chunk = fixture
            .output
            .join("synthetic_fixture/fixture-session/c0000");
        assert!(chunk.join("p0.mp4").is_file());
        assert!(!chunk.join("p0.flac").exists());
        let meta: ChunkMeta = read_json(&chunk.join("meta.json")).unwrap();
        let audio = meta.audio.unwrap();
        assert_eq!(audio.state, SegmentAudioState::Degraded);
        assert!(!audio.present);
        assert!(
            audio
                .failure_codes
                .contains(&"AUDIO_DEGRADED_NOT_PUBLIC".to_owned())
        );
        verify_dataset(&fixture.output).unwrap();
    }

    fn fixture_audio(specs: &[SegmentSpec]) -> Fixture {
        let fixture = fixture_v2(specs);
        let profile_path = fixture.session.join("recording-profile.json");
        let mut profile: serde_json::Value = read_json(&profile_path).unwrap();
        let audio_profile = CaptureAudioProfile::process_loopback();
        profile["audio"] = serde_json::to_value(&audio_profile).unwrap();
        write_json(&profile_path, &profile).unwrap();

        let recording_hash = sha256_file(&profile_path).unwrap();
        let catalog_path = fixture.session.join("catalog.json");
        let mut catalog: serde_json::Value = read_json(&catalog_path).unwrap();
        catalog["recording_profiles"][0]["content_hash"] = json!(recording_hash);
        write_json(&catalog_path, &catalog).unwrap();
        let catalog_hash = sha256_file(&catalog_path).unwrap();

        let session_path = fixture.session.join("session.json");
        let mut session: serde_json::Value = read_json(&session_path).unwrap();
        session["schema_version"] = json!(RAW_SESSION_SCHEMA_V3);
        session["capture_policy"]["audio"] = serde_json::to_value(&audio_profile).unwrap();
        session["catalog"]["recording"]["content_hash"] = json!(recording_hash);
        session["catalog"]["catalog_content_hash"] = json!(catalog_hash);
        write_json(&session_path, &session).unwrap();
        let catalog_identity = session["catalog"].clone();

        stamp_audio_generation_segments(&fixture, specs, &catalog_identity);
        fixture
    }

    /// 把夹具的段与 run 升到音频世代（segment schema-2 + PCM 工件 + manifest），
    /// meta 的 catalog 段统一改写成 `catalog_identity`。v2（合册）与 bundle-1
    /// （遗留）两种目录形态的音频夹具共用这一段——音频契约与目录世代无关。
    fn stamp_audio_generation_segments(
        fixture: &Fixture,
        specs: &[SegmentSpec],
        catalog_identity: &serde_json::Value,
    ) {
        for segment_index in 0..specs.len() {
            let segment = fixture
                .session
                .join(format!("runs/r0000/segments/{segment_index:05}"));
            let meta_path = segment.join("meta.json");
            let mut meta: serde_json::Value = read_json(&meta_path).unwrap();
            let qpc_start = meta["qpc_start"].as_i64().unwrap();
            let qpc_end = meta["qpc_end"].as_i64().unwrap();
            let frames = audio_frames_for_qpc_span(qpc_start, qpc_end, 30_000).unwrap();
            let mut pcm = Vec::with_capacity(usize::try_from(frames * 4).unwrap());
            for frame in 0..frames {
                let left = i16::try_from(i64::try_from(frame % 2_001).unwrap() - 1_000).unwrap();
                pcm.extend_from_slice(&left.to_le_bytes());
                pcm.extend_from_slice(&left.saturating_neg().to_le_bytes());
            }
            let audio_path = segment.join(AUDIO_ARTIFACT_NAME);
            let mut encoder = zstd::Encoder::new(File::create(&audio_path).unwrap(), 3).unwrap();
            encoder.write_all(&pcm).unwrap();
            encoder.finish().unwrap();
            let audio = SegmentAudioMetadata {
                state: SegmentAudioState::Ready,
                source: AudioSource::ProcessLoopback,
                path: Some(AUDIO_ARTIFACT_NAME.into()),
                sha256: Some(sha256_file(&audio_path).unwrap()),
                bytes: Some(fs::metadata(&audio_path).unwrap().len()),
                format: AudioPcmFormat::default(),
                qpc_frequency: 30_000,
                first_sample_qpc: Some(qpc_start),
                frames: Some(frames),
                failure_code: None,
                health: SegmentAudioHealth::default(),
            };
            meta["schema_version"] = json!(RAW_AUDIO_SEGMENT_SCHEMA);
            meta["catalog"] = catalog_identity.clone();
            meta["audio"] = serde_json::to_value(&audio).unwrap();
            write_json(&meta_path, &meta).unwrap();
            let manifest_path = segment.join("sha256.json");
            let mut manifest: BTreeMap<String, String> = read_json(&manifest_path).unwrap();
            manifest.insert(AUDIO_ARTIFACT_NAME.into(), audio.sha256.unwrap());
            manifest.insert("meta.json".into(), sha256_file(&meta_path).unwrap());
            write_json(&manifest_path, &manifest).unwrap();
        }
        let run_path = fixture.session.join("runs/r0000/run.json");
        let mut run: serde_json::Value = read_json(&run_path).unwrap();
        run["schemaVersion"] = json!(RAW_AUDIO_SEGMENT_SCHEMA);
        run["qpcFrequency"] = json!(30_000);
        write_json(&run_path, &run).unwrap();
    }

    fn fixture_v2(specs: &[SegmentSpec]) -> Fixture {
        fixture_v2_with_view_scale(specs, None)
    }

    fn fixture_ingest_v2(specs: &[SegmentSpec]) -> Fixture {
        let fixture = fixture_v2(specs);

        let device_path = fixture.session.join("device-profile.json");
        let mut device: serde_json::Value = read_json(&device_path).unwrap();
        device["schema_version"] = json!("dm-device-profile-3");
        device.as_object_mut().unwrap().remove("non_trainable");
        write_json(&device_path, &device).unwrap();

        let recording_path = fixture.session.join("recording-profile.json");
        let mut recording: serde_json::Value = read_json(&recording_path).unwrap();
        recording["schema_version"] = json!("dm-recording-profile-2");
        recording.as_object_mut().unwrap().remove("non_trainable");
        write_json(&recording_path, &recording).unwrap();

        let game_path = fixture.session.join("game-profile.json");
        let mut game: serde_json::Value = read_json(&game_path).unwrap();
        game["schema_version"] = json!("dm-game-profile-3");
        game.as_object_mut().unwrap().remove("non_trainable");
        write_json(&game_path, &game).unwrap();

        let device_identity = RawCatalogEntryIdentity {
            id: "synthetic-fixture-device-v1".into(),
            version: "1".into(),
            content_hash: sha256_file(&device_path).unwrap(),
        };
        let recording_identity = RawCatalogEntryIdentity {
            id: RECORDING_PROFILE_ID.into(),
            version: "1".into(),
            content_hash: sha256_file(&recording_path).unwrap(),
        };
        let game_identity = RawCatalogEntryIdentity {
            id: "synthetic-fixture".into(),
            version: "1".into(),
            content_hash: sha256_file(&game_path).unwrap(),
        };
        let mapping_identity = RawCatalogEntryIdentity {
            id: "game.synthetic-fixture.pc".into(),
            version: "2026-07-23.1".into(),
            content_hash: sha256_file(&fixture.session.join("mapping.json")).unwrap(),
        };
        let catalog_path = fixture.session.join("catalog.json");
        write_json(
            &catalog_path,
            &json!({
                "schema_version": "dm-catalog-bundle-3",
                "catalog_id": "dataminer-synthetic-fixture-neutral",
                "catalog_version": "2",
                "device_profiles": [&device_identity],
                "recording_profiles": [&recording_identity],
                "game_profiles": [&game_identity],
                "mappings": [&mapping_identity]
            }),
        )
        .unwrap();
        let catalog = RawCatalogIdentity {
            mode: None,
            catalog_id: "dataminer-synthetic-fixture-neutral".into(),
            catalog_version: "2".into(),
            catalog_content_hash: sha256_file(&catalog_path).unwrap(),
            provenance: Some("delivered".into()),
            device: device_identity,
            recording: Some(recording_identity),
            capture: None,
            settings: None,
            game: game_identity,
            mapping: mapping_identity,
        };
        let catalog_json = serde_json::to_value(&catalog).unwrap();

        let session_path = fixture.session.join("session.json");
        let mut session: serde_json::Value = read_json(&session_path).unwrap();
        session["schema_version"] = json!(RAW_INGEST_V2_SESSION_SCHEMA);
        session["catalog"] = catalog_json.clone();
        let session_object = session.as_object_mut().unwrap();
        session_object.remove("non_trainable");
        session_object.remove("production_certified");
        session_object.remove("consent_id");
        write_json(&session_path, &session).unwrap();

        for segment_index in 0..specs.len() {
            let segment_dir = fixture
                .session
                .join(format!("runs/r0000/segments/{segment_index:05}"));
            let meta_path = segment_dir.join("meta.json");
            let mut meta: serde_json::Value = read_json(&meta_path).unwrap();
            meta["schema_version"] = json!(RAW_INGEST_V2_SEGMENT_SCHEMA);
            meta["state"] = json!(RAW_PLAIN_READY_STATE);
            meta["catalog"] = catalog_json.clone();
            let meta_object = meta.as_object_mut().unwrap();
            meta_object.remove("non_trainable");
            meta_object.remove("production_certified");
            meta_object.remove("consent_id");
            write_json(&meta_path, &meta).unwrap();
            rewrite_segment_meta_hash(&segment_dir);
        }

        let run_path = fixture.session.join("runs/r0000/run.json");
        let mut run: serde_json::Value = read_json(&run_path).unwrap();
        run["schemaVersion"] = json!(RAW_INGEST_V2_SEGMENT_SCHEMA);
        run["state"] = json!(RAW_INGEST_V2_RUN_COMPLETE_STATE);
        let run_object = run.as_object_mut().unwrap();
        run_object.remove("nonTrainable");
        run_object.remove("productionCertified");
        write_json(&run_path, &run).unwrap();

        fixture
    }

    fn stamp_catalog_provenance(fixture: &Fixture, segment_count: usize, provenance: &str) {
        let session_path = fixture.session.join("session.json");
        let mut session: serde_json::Value = read_json(&session_path).unwrap();
        session["catalog"]["provenance"] = json!(provenance);
        write_json(&session_path, &session).unwrap();

        for segment_index in 0..segment_count {
            let segment_dir = fixture
                .session
                .join(format!("runs/r0000/segments/{segment_index:05}"));
            let meta_path = segment_dir.join("meta.json");
            let mut meta: serde_json::Value = read_json(&meta_path).unwrap();
            meta["catalog"]["provenance"] = json!(provenance);
            write_json(&meta_path, &meta).unwrap();
            rewrite_segment_meta_hash(&segment_dir);
        }
    }

    fn stamp_run_wall_clock(fixture: &Fixture) {
        stamp_run_wall_clock_values(fixture, 10_010, 30_000);
    }

    fn stamp_run_wall_clock_values(fixture: &Fixture, anchor_qpc: i64, qpc_frequency: i64) {
        let run_path = fixture.session.join("runs/r0000/run.json");
        let mut run: serde_json::Value = read_json(&run_path).unwrap();
        run["qpcFrequency"] = json!(qpc_frequency);
        run["anchorQpc"] = json!(anchor_qpc);
        run["anchorAtUtc"] = json!("2026-08-04T09:08:56.123Z");
        write_json(&run_path, &run).unwrap();
    }

    fn stamp_run_wall_clock_utc(fixture: &Fixture, anchor_at_utc: &str) {
        stamp_run_wall_clock(fixture);
        let run_path = fixture.session.join("runs/r0000/run.json");
        let mut run: serde_json::Value = read_json(&run_path).unwrap();
        run["anchorAtUtc"] = json!(anchor_at_utc);
        write_json(&run_path, &run).unwrap();
    }

    fn downgrade_data_model_json_to_dm_1(path: &Path) {
        let mut value: serde_json::Value = read_json(path).unwrap();
        let object = value
            .as_object_mut()
            .expect("fixture JSON must be an object");
        object.insert("schema_version".into(), json!("dm-1"));
        object.remove("catalog");
        write_json(path, &value).unwrap();
    }

    fn downgrade_data_model_jsonl_to_dm_1(path: &Path) {
        let mut rows: Vec<serde_json::Value> = read_jsonl(path).unwrap();
        for row in &mut rows {
            let object = row
                .as_object_mut()
                .expect("fixture JSONL row must be an object");
            object.insert("schema_version".into(), json!("dm-1"));
            object.remove("catalog");
        }
        write_jsonl(path, &rows).unwrap();
    }

    fn rewrite_output_schema_and_catalog(
        fixture: &Fixture,
        chunk_count: u32,
        schema_version: &str,
        catalog: Option<serde_json::Value>,
    ) {
        let rewrite = |value: &mut serde_json::Value| {
            let object = value
                .as_object_mut()
                .expect("DataModel metadata must be a JSON object");
            object.insert("schema_version".into(), json!(schema_version));
            match &catalog {
                Some(catalog) => {
                    object.insert("catalog".into(), catalog.clone());
                }
                None => {
                    object.remove("catalog");
                }
            }
        };

        let metadata_jsonl = fixture.output.join("metadata.jsonl");
        let mut rows: Vec<serde_json::Value> = read_jsonl(&metadata_jsonl).unwrap();
        for row in &mut rows {
            rewrite(row);
        }
        write_jsonl(&metadata_jsonl, &rows).unwrap();

        let session_output = fixture.output.join("synthetic_fixture/fixture-session");
        let metadata_json = session_output.join("metadata.json");
        let mut metadata: serde_json::Value = read_json(&metadata_json).unwrap();
        rewrite(&mut metadata);
        write_json(&metadata_json, &metadata).unwrap();
        for chunk in 0..chunk_count {
            let chunk_json = session_output.join(format!("c{chunk:04}/meta.json"));
            let mut metadata: serde_json::Value = read_json(&chunk_json).unwrap();
            rewrite(&mut metadata);
            write_json(&chunk_json, &metadata).unwrap();
        }
    }

    fn fixture_wall_clock_context() -> IncrementalWallClockContext {
        IncrementalWallClockContext {
            anchor_at_utc: Some("2026-08-04T09:08:56.123Z".into()),
            anchor_qpc: 10_010,
            qpc_frequency: 30_000,
        }
    }

    fn fixture_v2_with_view_scale(
        specs: &[SegmentSpec],
        view_scale: Option<serde_json::Value>,
    ) -> Fixture {
        let fixture = fixture(specs);
        let device = json!({
            "schema_version": "dm-device-profile-2",
            "profile_id": "synthetic-fixture-device-v1",
            "version": "1",
            "non_trainable": true
        });
        let game = json!({
            "schema_version": "dm-game-profile-2",
            "game_id": "synthetic-fixture",
            "version": "1",
            "device_profile_id": "synthetic-fixture-device-v1",
            "recording_profile_id": RECORDING_PROFILE_ID,
            "mapping_id": "game.synthetic-fixture.pc",
            "non_trainable": true
        });
        write_json(&fixture.session.join("device-profile.json"), &device).unwrap();
        write_json(&fixture.session.join("game-profile.json"), &game).unwrap();

        // 单册 recording profile（docs/57 §3.1）= 基础夹具那份引擎参数 + 断代后归到
        // 同一本里的身份与游戏侧字段。从 capture-profile.json 读回来再加字段，引擎参数
        // 就不可能和 v1 夹具漂开——session.json 的 capture_policy 是拿它对账的。
        let mut recording: serde_json::Value =
            read_json(&fixture.session.join("capture-profile.json")).unwrap();
        recording["schema_version"] = json!("dm-recording-profile-1");
        recording["profile_id"] = json!(RECORDING_PROFILE_ID);
        recording["version"] = json!("1");
        recording["game_id"] = json!("synthetic-fixture");
        if let Some(view_scale) = view_scale {
            recording["view_scale"] = view_scale;
        }
        write_json(&fixture.session.join("recording-profile.json"), &recording).unwrap();
        // v2 会话不再写 capture-profile.json。删掉它，任何一处误读旧文件名会当场炸，
        // 而不是悄悄拿 v1 夹具留下的那份字节顶上。
        fs::remove_file(fixture.session.join("capture-profile.json")).unwrap();

        let device_identity = RawCatalogEntryIdentity {
            id: "synthetic-fixture-device-v1".into(),
            version: "1".into(),
            content_hash: sha256_file(&fixture.session.join("device-profile.json")).unwrap(),
        };
        let recording_identity = RawCatalogEntryIdentity {
            id: RECORDING_PROFILE_ID.into(),
            version: "1".into(),
            content_hash: sha256_file(&fixture.session.join("recording-profile.json")).unwrap(),
        };
        let game_identity = RawCatalogEntryIdentity {
            id: "synthetic-fixture".into(),
            version: "1".into(),
            content_hash: sha256_file(&fixture.session.join("game-profile.json")).unwrap(),
        };
        let mapping_identity = RawCatalogEntryIdentity {
            id: "game.synthetic-fixture.pc".into(),
            version: "2026-07-23.1".into(),
            content_hash: sha256_file(&fixture.session.join("mapping.json")).unwrap(),
        };
        write_json(
            &fixture.session.join("catalog.json"),
            &json!({
                "schema_version": "dm-catalog-bundle-2",
                "catalog_id": "dataminer-synthetic-fixture-dev",
                "catalog_version": "1",
                "non_trainable": true,
                "device_profiles": [&device_identity],
                "recording_profiles": [&recording_identity],
                "game_profiles": [&game_identity],
                "mappings": [&mapping_identity]
            }),
        )
        .unwrap();
        let catalog = RawCatalogIdentity {
            mode: Some("dev_non_trainable".into()),
            catalog_id: "dataminer-synthetic-fixture-dev".into(),
            catalog_version: "1".into(),
            catalog_content_hash: sha256_file(&fixture.session.join("catalog.json")).unwrap(),
            provenance: None,
            device: device_identity,
            recording: Some(recording_identity),
            capture: None,
            settings: None,
            game: game_identity,
            mapping: mapping_identity,
        };

        let session_path = fixture.session.join("session.json");
        let mut session: serde_json::Value = read_json(&session_path).unwrap();
        session["schema_version"] = json!(RAW_SESSION_SCHEMA_V2);
        session["catalog"] = serde_json::to_value(&catalog).unwrap();
        session["catalog"]["non_trainable"] = json!(true);
        write_json(&session_path, &session).unwrap();

        for segment_index in 0..specs.len() {
            let segment_dir = fixture
                .session
                .join(format!("runs/r0000/segments/{segment_index:05}"));
            let meta_path = segment_dir.join("meta.json");
            let mut meta: serde_json::Value = read_json(&meta_path).unwrap();
            meta["catalog"] = serde_json::to_value(&catalog).unwrap();
            meta["catalog"]["non_trainable"] = json!(true);
            write_json(&meta_path, &meta).unwrap();
            rewrite_segment_meta_hash(&segment_dir);
        }
        fixture
    }

    /// bundle-1 遗留夹具的 settings profile id（dm-mapping-1 的
    /// `settings_profile_id` 与 catalog/settings/game 三处必须同指它）。
    const LEGACY_SETTINGS_PROFILE_ID: &str = "synthetic-fixture-settings-v1";

    /// 把 v2 夹具 mapping 降回 dm-mapping-1：schema 字符串 + profile 引用键改名，
    /// 与 e433de36 的断代差异严格互逆——两代 mapping 的其余字节完全同形。
    fn legacy_mapping_bytes() -> Vec<u8> {
        String::from_utf8(MAPPING.to_vec())
            .unwrap()
            .replace("dm-mapping-2", "dm-mapping-1")
            .replace(
                "\"recording_profile_id\": \"synthetic-fixture-recording-v1\"",
                &format!("\"settings_profile_id\": \"{LEGACY_SETTINGS_PROFILE_ID}\""),
            )
            .into_bytes()
    }

    fn fixture_legacy_bundle1(specs: &[SegmentSpec]) -> Fixture {
        fixture_legacy_bundle1_with(specs, None, "dm-device-profile-1")
    }

    /// docs/57 断代前的 bundle-1 会话夹具（issue #811 路线 A 的被测对象）：
    /// session-2 schema + capture/settings 两册目录 + dm-mapping-1。与 R2 封存
    /// COD 车道同形（30fps 无音频代；音频代见 `fixture_legacy_bundle1_audio`）。
    fn fixture_legacy_bundle1_with(
        specs: &[SegmentSpec],
        view_scale: Option<serde_json::Value>,
        device_schema: &str,
    ) -> Fixture {
        let fixture = fixture(specs);
        let legacy_mapping = legacy_mapping_bytes();
        fs::write(fixture.session.join("mapping.json"), &legacy_mapping).unwrap();
        let mapping_hash = sha256_bytes(&legacy_mapping);

        let device = json!({
            "schema_version": device_schema,
            "profile_id": "synthetic-fixture-device-v1",
            "version": "1",
            "non_trainable": true
        });
        let mut settings = json!({
            "schema_version": "dm-settings-profile-1",
            "profile_id": LEGACY_SETTINGS_PROFILE_ID,
            "version": "1",
            "game_id": "synthetic-fixture",
            "capture_profile_id": "synthetic-fixture-v1",
            "non_trainable": true
        });
        if let Some(view_scale) = view_scale {
            settings["view_scale"] = view_scale;
        }
        let game = json!({
            "schema_version": "dm-game-profile-1",
            "game_id": "synthetic-fixture",
            "version": "1",
            "device_profile_id": "synthetic-fixture-device-v1",
            "capture_profile_id": "synthetic-fixture-v1",
            "settings_profile_id": LEGACY_SETTINGS_PROFILE_ID,
            "mapping_id": "game.synthetic-fixture.pc",
            "mapping_version": "2026-07-23.1",
            "non_trainable": true
        });
        write_json(&fixture.session.join("device-profile.json"), &device).unwrap();
        write_json(&fixture.session.join("settings-profile.json"), &settings).unwrap();
        write_json(&fixture.session.join("game-profile.json"), &game).unwrap();

        let entry = |id: &str, version: &str, content_hash: String| json!({"id": id, "version": version, "content_hash": content_hash});
        let device_entry = entry(
            "synthetic-fixture-device-v1",
            "1",
            sha256_file(&fixture.session.join("device-profile.json")).unwrap(),
        );
        let capture_entry = entry(
            "synthetic-fixture-v1",
            "1",
            sha256_file(&fixture.session.join("capture-profile.json")).unwrap(),
        );
        let settings_entry = entry(
            LEGACY_SETTINGS_PROFILE_ID,
            "1",
            sha256_file(&fixture.session.join("settings-profile.json")).unwrap(),
        );
        let game_entry = entry(
            "synthetic-fixture",
            "1",
            sha256_file(&fixture.session.join("game-profile.json")).unwrap(),
        );
        let mapping_entry = entry("game.synthetic-fixture.pc", "2026-07-23.1", mapping_hash);

        let catalog_path = fixture.session.join("catalog.json");
        write_json(
            &catalog_path,
            &json!({
                "schema_version": "dm-catalog-bundle-1",
                "catalog_id": "dataminer-synthetic-fixture-legacy",
                "catalog_version": "1",
                "catalog_mode": "dev_non_trainable",
                "non_trainable": true,
                "device_profiles": [&device_entry],
                "capture_profiles": [&capture_entry],
                "game_profiles": [&game_entry],
                "settings_profiles": [&settings_entry],
                "mappings": [&mapping_entry]
            }),
        )
        .unwrap();
        let catalog_identity = json!({
            "mode": "dev_non_trainable",
            "catalog_id": "dataminer-synthetic-fixture-legacy",
            "catalog_version": "1",
            "catalog_content_hash": sha256_file(&catalog_path).unwrap(),
            "device": device_entry,
            "capture": capture_entry,
            "settings": settings_entry,
            "game": game_entry,
            "mapping": mapping_entry
        });

        let session_path = fixture.session.join("session.json");
        let mut session: serde_json::Value = read_json(&session_path).unwrap();
        session["schema_version"] = json!(RAW_SESSION_SCHEMA_V2);
        session["catalog"] = catalog_identity.clone();
        session["mapping"] = json!({
            "id": "game.synthetic-fixture.pc",
            "version": "2026-07-23.1",
            "content_hash": mapping_entry["content_hash"],
            "settings_profile_id": LEGACY_SETTINGS_PROFILE_ID
        });
        write_json(&session_path, &session).unwrap();

        for segment_index in 0..specs.len() {
            let segment_dir = fixture
                .session
                .join(format!("runs/r0000/segments/{segment_index:05}"));
            let meta_path = segment_dir.join("meta.json");
            let mut meta: serde_json::Value = read_json(&meta_path).unwrap();
            meta["catalog"] = catalog_identity.clone();
            meta["mapping"]["content_hash"] = mapping_entry["content_hash"].clone();
            write_json(&meta_path, &meta).unwrap();
            rewrite_segment_meta_hash(&segment_dir);
        }

        let run_path = fixture.session.join("runs/r0000/run.json");
        let mut run: serde_json::Value = read_json(&run_path).unwrap();
        run["mappingContentHash"] = mapping_entry["content_hash"].clone();
        write_json(&run_path, &run).unwrap();
        fixture
    }

    /// bundle-1 遗留世代的音频代（R2 封存 COD 车道的 07-31 形态：session-3 +
    /// 音频 capture profile + bundle-1 目录 + dm-mapping-1）。
    fn fixture_legacy_bundle1_audio(specs: &[SegmentSpec]) -> Fixture {
        let fixture = fixture_legacy_bundle1(specs);
        let profile_path = fixture.session.join("capture-profile.json");
        let mut profile: serde_json::Value = read_json(&profile_path).unwrap();
        let audio_profile = CaptureAudioProfile::process_loopback();
        profile["audio"] = serde_json::to_value(&audio_profile).unwrap();
        write_json(&profile_path, &profile).unwrap();
        let capture_hash = sha256_file(&profile_path).unwrap();

        let catalog_path = fixture.session.join("catalog.json");
        let mut catalog: serde_json::Value = read_json(&catalog_path).unwrap();
        catalog["capture_profiles"][0]["content_hash"] = json!(capture_hash);
        write_json(&catalog_path, &catalog).unwrap();
        let catalog_hash = sha256_file(&catalog_path).unwrap();

        let session_path = fixture.session.join("session.json");
        let mut session: serde_json::Value = read_json(&session_path).unwrap();
        session["schema_version"] = json!(RAW_SESSION_SCHEMA_V3);
        session["capture_policy"]["audio"] = serde_json::to_value(&audio_profile).unwrap();
        session["catalog"]["capture"]["content_hash"] = json!(capture_hash);
        session["catalog"]["catalog_content_hash"] = json!(catalog_hash);
        write_json(&session_path, &session).unwrap();
        let catalog_identity = session["catalog"].clone();

        stamp_audio_generation_segments(&fixture, specs, &catalog_identity);
        fixture
    }

    fn rewrite_segment_meta_hash(segment_dir: &Path) {
        let manifest_path = segment_dir.join("sha256.json");
        let mut manifest: BTreeMap<String, String> = read_json(&manifest_path).unwrap();
        manifest.insert(
            "meta.json".into(),
            sha256_file(&segment_dir.join("meta.json")).unwrap(),
        );
        write_json(&manifest_path, &manifest).unwrap();
    }

    fn fixture(specs: &[SegmentSpec]) -> Fixture {
        fixture_with(specs, &ComplianceProfile::LocalDebugNonTrainable)
    }

    fn fixture_with(specs: &[SegmentSpec], compliance: &ComplianceProfile) -> Fixture {
        let release = compliance.is_release();
        let consent = compliance.consent_id();
        let temp = tempfile::tempdir().unwrap();
        let session = temp.path().join("ready-session");
        let output = temp.path().join("dataset");
        fs::create_dir_all(session.join("runs/r0000/segments")).unwrap();
        let mapping = MappingSnapshot::from_slice(MAPPING).unwrap();
        fs::write(session.join("mapping.json"), MAPPING).unwrap();
        let profile = CaptureProfile {
            profile_id: "synthetic-fixture-v1".into(),
            non_trainable: !release,
            capture_backend: "native_wgc_window_v1".into(),
            encoder_backend: "mf_nvidia_hardware_v1".into(),
            width: TARGET_WIDTH,
            height: TARGET_HEIGHT,
            fps_num: TARGET_FPS,
            fps_den: 1,
            bitrate_bps: 1_000_000,
            max_bitrate_bps: 2_000_000,
            gop_frames: CHUNK_FRAMES as u32,
            segment_seconds: 10,
            test_segments: 1,
            accepted_encoder_relaxations: None,
            audio: None,
        };
        write_json(&session.join("capture-profile.json"), &profile).unwrap();
        write_json(
            &session.join("session.json"),
            &json!({
                "schema_version": RAW_SESSION_SCHEMA_V1,
                "session_id": "fixture-session",
                "non_trainable": !release,
                "production_certified": false,
                "consent_id": consent,
                "game": {"id": "synthetic-fixture"},
                "mapping": {
                    "id": mapping.mapping().mapping_id,
                    "version": mapping.mapping().version,
                    "content_hash": mapping.content_hash(),
                    "recording_profile_id": mapping.mapping().recording_profile_id
                },
                "capture_policy": {
                    "source": "game_window_only",
                    "width": TARGET_WIDTH,
                    "height": TARGET_HEIGHT,
                    "scale": false,
                    "fps_num": TARGET_FPS,
                    "fps_den": 1,
                    "gop_frames": CHUNK_FRAMES,
                    "audio": false
                },
                "contract": contract_json()
            }),
        )
        .unwrap();

        let total_frames = specs.iter().map(|spec| spec.frames as u64).sum::<u64>();
        let all_events = fixture_events();
        let mut projector = FrameProjector::default();
        let mut session_frame = 0_u64;
        let mut total_events = 0_u64;
        for (segment_index, spec) in specs.iter().enumerate() {
            let segment_dir = session.join(format!("runs/r0000/segments/{segment_index:05}"));
            fs::create_dir(&segment_dir).unwrap();
            let qpc_start = 10_000 + i64::try_from(session_frame).unwrap() * 1_000;
            let qpc_end = qpc_start + i64::try_from(spec.frames).unwrap() * 1_000;
            let timing = (0..spec.frames)
                .map(|local| {
                    let global = session_frame + local as u64;
                    FrameTiming {
                        frame: local as u64,
                        session_frame: global,
                        output_qpc_start: 10_000 + global as i64 * 1_000,
                        output_qpc_end: 10_000 + (global as i64 + 1) * 1_000,
                        source_qpc: 10_010 + global as i64 * 1_000,
                        source_seq: global + 1,
                        present_seq: None,
                        duplicate: false,
                        source_superseded: 0,
                        queue_drop: 0,
                    }
                })
                .collect::<Vec<_>>();
            let events = all_events
                .iter()
                .filter(|event| event.qpc >= qpc_start && event.qpc < qpc_end)
                .cloned()
                .collect::<Vec<_>>();
            total_events += events.len() as u64;
            let rows = projector
                .project_segment(&timing, &events, "r0000", &profile, mapping.mapping())
                .unwrap();
            write_zstd_jsonl(&segment_dir.join("frames.jsonl.zst"), &rows);
            write_zstd_jsonl(&segment_dir.join("events.jsonl.zst"), &events);
            write_fixture_mp4(
                &segment_dir.join("video.mp4"),
                spec.frames,
                &spec.sync_samples,
            );
            let video_hash = sha256_file(&segment_dir.join("video.mp4")).unwrap();
            let frames_hash = sha256_file(&segment_dir.join("frames.jsonl.zst")).unwrap();
            let events_hash = sha256_file(&segment_dir.join("events.jsonl.zst")).unwrap();
            let video_bytes = fs::metadata(segment_dir.join("video.mp4")).unwrap().len();
            let meta = json!({
                "schema_version": RAW_SEGMENT_SCHEMA,
                "session_id": "fixture-session",
                "run_id": "r0000",
                "segment_index": segment_index,
                "session_frame_start": session_frame,
                "n_frames": spec.frames,
                "state": if release { RAW_CONSENTED_READY_STATE } else { RAW_READY_STATE },
                "non_trainable": !release,
                "production_certified": false,
                "consent_id": consent,
                "input_status": "raw_input_v1",
                "qpc_start": qpc_start,
                "qpc_end": qpc_end,
                "fps_num": TARGET_FPS,
                "fps_den": 1,
                "width": TARGET_WIDTH,
                "height": TARGET_HEIGHT,
                "mapping": {
                    "id": mapping.mapping().mapping_id,
                    "version": mapping.mapping().version,
                    "content_hash": mapping.content_hash()
                },
                "contract": contract_json(),
                "video": {"path": "video.mp4", "bytes": video_bytes, "sha256": video_hash},
                "frames": {"path": "frames.jsonl.zst", "lines": rows.len(), "sha256": frames_hash},
                "events": {"path": "events.jsonl.zst", "lines": events.len(), "sha256": events_hash},
                "effective_encoder": {
                    "transform_clsid": null,
                    "friendly_name": "synthetic hardware fixture",
                    "hardware_url": "synthetic hardware fixture",
                    "rate_control_mode": 1,
                    "mean_bitrate_bps": profile.bitrate_bps,
                    "max_bitrate_bps": profile.max_bitrate_bps,
                    "gop_frames": profile.gop_frames,
                    "b_frames": 0,
                    "low_latency": true
                },
                "probe": {
                    "codec": "H.264",
                    "h264_profile": 100,
                    "width": TARGET_WIDTH,
                    "height": TARGET_HEIGHT,
                    "fps_num": TARGET_FPS,
                    "fps_den": 1,
                    "sample_count": spec.frames,
                    "first_sample_clean_point": true,
                    "pts_monotonic": true
                },
                "health": {"queue_drops": 0, "input_overflow": 0}
            });
            write_json(&segment_dir.join("meta.json"), &meta).unwrap();
            let meta_hash = sha256_file(&segment_dir.join("meta.json")).unwrap();
            write_json(
                &segment_dir.join("sha256.json"),
                &BTreeMap::from([
                    ("video.mp4", video_hash),
                    ("frames.jsonl.zst", frames_hash),
                    ("events.jsonl.zst", events_hash),
                    ("meta.json", meta_hash),
                ]),
            )
            .unwrap();
            session_frame += spec.frames as u64;
        }
        write_json(
            &session.join("runs/r0000/run.json"),
            &json!({
                "schemaVersion": RAW_SEGMENT_SCHEMA,
                "state": if release {
                    RAW_CONSENTED_RUN_COMPLETE_STATE
                } else {
                    RAW_RUN_COMPLETE_STATE
                },
                "nonTrainable": !release,
                "productionCertified": false,
                "sessionId": "fixture-session",
                "runId": "r0000",
                "mappingId": mapping.mapping().mapping_id,
                "mappingVersion": mapping.mapping().version,
                "mappingContentHash": mapping.content_hash(),
                "sessionFrameStart": 0,
                "sessionFrameEnd": total_frames,
                "frameCount": total_frames,
                "segmentCount": specs.len(),
                "inputEventCount": total_events,
                "inputQueueOverflows": 0
            }),
        )
        .unwrap();
        Fixture {
            _temp: temp,
            session,
            output,
        }
    }

    fn fixture_events() -> Vec<RawInputEvent> {
        vec![
            RawInputEvent {
                seq: 0,
                run_id: "r0000".into(),
                qpc: 10_100,
                event_type: "key_down".into(),
                device: "kbd:fixture".into(),
                vk: Some(87),
                scan_code: Some(17),
                flags: Some(0),
                repeat: None,
                dx: None,
                dy: None,
                absolute: None,
                delta: None,
                horizontal: None,
                control: false,
            },
            RawInputEvent {
                seq: 1,
                run_id: "r0000".into(),
                qpc: 11_100,
                event_type: "mouse_move".into(),
                device: "mouse:fixture".into(),
                vk: None,
                scan_code: None,
                flags: None,
                repeat: None,
                dx: Some(120),
                dy: Some(-14),
                absolute: Some(false),
                delta: None,
                horizontal: None,
                control: false,
            },
            RawInputEvent {
                seq: 2,
                run_id: "r0000".into(),
                qpc: 13_100,
                event_type: "key_up".into(),
                device: "kbd:fixture".into(),
                vk: Some(87),
                scan_code: Some(17),
                flags: Some(0),
                repeat: None,
                dx: None,
                dy: None,
                absolute: None,
                delta: None,
                horizontal: None,
                control: false,
            },
        ]
    }

    fn write_fixture_mp4(path: &Path, frames: usize, sync_samples: &[u32]) {
        let output = BufWriter::new(File::create(path).unwrap());
        let mut writer = Mp4Writer::write_start(
            output,
            &Mp4Config {
                major_brand: "isom".parse().unwrap(),
                minor_version: 512,
                compatible_brands: vec!["isom".parse().unwrap(), "avc1".parse().unwrap()],
                timescale: 1_000,
            },
        )
        .unwrap();
        writer
            .add_track(&TrackConfig {
                track_type: TrackType::Video,
                timescale: 30_000,
                language: "und".into(),
                media_conf: MediaConfig::AvcConfig(AvcConfig {
                    width: TARGET_WIDTH as u16,
                    height: TARGET_HEIGHT as u16,
                    seq_param_set: vec![0x67, 100, 0, 31, 0xac, 0xd9, 0x40],
                    pic_param_set: vec![0x68, 0xee, 0x3c, 0x80],
                }),
            })
            .unwrap();
        for frame in 0..frames {
            writer
                .write_sample(
                    1,
                    &Mp4Sample {
                        start_time: frame as u64 * 1_000,
                        duration: 1_000,
                        rendering_offset: 0,
                        is_sync: sync_samples.contains(&(frame as u32)),
                        bytes: vec![
                            0,
                            0,
                            0,
                            1,
                            if sync_samples.contains(&(frame as u32)) {
                                0x65
                            } else {
                                0x41
                            },
                        ]
                        .into(),
                    },
                )
                .unwrap();
        }
        writer.write_end().unwrap();
        let mut output = writer.into_writer();
        output.flush().unwrap();
    }

    fn write_zstd_jsonl(path: &Path, rows: &[impl Serialize]) {
        let file = BufWriter::new(File::create(path).unwrap());
        let mut encoder = zstd::Encoder::new(file, 3).unwrap();
        for row in rows {
            serde_json::to_writer(&mut encoder, row).unwrap();
            encoder.write_all(b"\n").unwrap();
        }
        encoder.finish().unwrap().flush().unwrap();
    }

    fn contract_json() -> serde_json::Value {
        json!({
            "version": RAW_CONTRACT_VERSION,
            "pointer_clamp": 200,
            "pointer_mu": 8,
            "pointer_ref": 200,
            "pointer_max_bin": 10
        })
    }
}

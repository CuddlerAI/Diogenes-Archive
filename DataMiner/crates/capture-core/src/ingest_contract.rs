//! 摄取契约世代（issue #855 §B）。
//!
//! 一场采集从头到尾只属于**一个**契约。它在 session 创建时定死、之后不可变，
//! 并且一处决定四件事：session schema、clip artifact schema、上传信封的
//! source family、preflight 终态词。
//!
//! 断代前这四件事各自散在 capture-core、journal、uploader 与 preflight 里，靠
//! `dev_non_trainable` 这一个词串起来——而那个词同时表达"运行环境"、"目录渠道"
//! 和"这批数据能不能训练"三件互不相干的事（issue #855 §2）。把它们并进一个类型，
//! 是为了让"这场采集按哪一代写字节"只有一个答案，而不是四处各自推断一遍。
//!
//! **谁决定用哪一代不在这里回答。** 那是目录选择的事：managed（服务端签名下发）
//! 且已是中性 schema 的目录走 [`SessionContract::IngestV2`]，其余一律
//! [`SessionContract::LocalDevV1`]。这里只回答"选定之后各处该写哪些字节"。

use serde::{Deserialize, Serialize};

pub const RECORDING_SCHEMA_V1: &str = "dm-local-dev-recording-1";
pub const RECORDING_SCHEMA_V2: &str = "dm-local-dev-recording-2";
pub const SESSION_SCHEMA_V2: &str = "dm-local-dev-session-2";
pub const SESSION_SCHEMA_V3: &str = "dm-local-dev-session-3";

/// 中性摄取契约的 session schema（issue #855 §B）。
///
/// v2 **只有这一个值**，不随音频分世代：legacy 用 `-2` / `-3` 表达"有没有音频"，
/// 而 §B 明确 `sessions.schema_version` 是 v1/v2 并存期的唯一存储路由键——一个
/// 兼职表达音频的路由键会在每加一件可选产物时翻倍。v2 上"这场采集有没有音频"
/// 是 clip 的 artifact 事实（服务端 `audioDeclaration: "per_clip"`）。
pub const INGEST_V2_SESSION_SCHEMA: &str = "dm-session-ingest-2";
/// 中性摄取契约的 clip artifact schema，对应服务端家族的 `artifactSchemaVersion`。
/// 与 object class `dataminer-raw`、前缀 `raw/v2` 同一个词根，不含环境/训练词。
pub const INGEST_V2_RECORDING_SCHEMA: &str = "dm-raw-recording-1";

/// legacy 车道的上传信封 source family。名字里的 `local-dev` 正是本 issue 要拆掉
/// 的写法，但它是**存量契约**：服务端 legacy 家族按字面量校验，改它等于把所有
/// 历史 session 的上传打断。中性车道用下面那个词。
pub const LOCAL_DEV_SOURCE_FAMILY: &str = "dm-local-dev";
/// 中性车道的 source family，由契约名派生（`dm-session-ingest-2` → `dm-session-ingest`）。
pub const INGEST_V2_SOURCE_FAMILY: &str = "dm-session-ingest";

/// legacy 车道 preflight 的唯一终态。词里带着车道名，这正是它只留在 legacy 的原因。
pub const DEV_NON_TRAINABLE_PASS: &str = "dev_non_trainable_pass";
/// 中性车道 preflight 的唯一终态。它只回答"preflight 过了没有"，不兼职表达车道。
pub const PREFLIGHT_PASS: &str = "preflight_pass";

/// legacy 目录身份里的 `mode` 字面量。
///
/// 它仍然出现在**已经落盘**的 legacy session.json / meta.json 里，recovery 要按它
/// 校验存量产物，所以这个字节不能消失。新写入只在 legacy 契约上产生它；中性契约
/// 的目录身份只有 id / version / contentHash（issue #855 §A.4）。
pub const LEGACY_CATALOG_MODE: &str = "dev_non_trainable";

/// 客户端录制**永远** non-trainable。
///
/// 中性 schema 删掉了 `non_trainable`，所以运行时这一位不能再从 bundle 里读。它的
/// 替代来源不是另一个字段，而是这条硬约束本身：可训练性只能由发布侧的人工放行开启，
/// 客户端与录制链路的任何位置都不得主张它（AGENTS.md、docs/100）。因此这里是常量，
/// 不是配置——一个能被 authoring 改成 false 的开关，正是 issue #855 要拆掉的东西。
pub const CLIENT_CAPTURE_IS_ALWAYS_NON_TRAINABLE: bool = true;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SessionContract {
    /// legacy v1。编译内嵌目录与 `local-catalog-only` 构建的**永久**归宿
    /// （issue #855 §D）：`raw/v1/non-trainable/...`、legacy object class、分类永久
    /// `non_trainable`，绝不产生 candidate。本地 E2E 不另造 v2 变体。
    /// `alias`：v33 之前落盘的 preflight 证据把这一代写作 `dev_non_trainable`。
    /// 那份 JSON 已经在用户机器上了，而它表达的**就是**这一代——按别名收下，而不是
    /// 让一份历史证据在反序列化时炸掉。新写入一律用上面那个名字。
    #[serde(rename = "dm-local-dev", alias = "dev_non_trainable")]
    LocalDevV1,
    /// 中性 v2。服务端按已登记的 Catalog 身份、capture admission 与采集时既存事实
    /// 派生 `training_candidate`——**客户端一个字都不申报训练资格**，candidate 也
    /// 不等于通过（issue #855 §B / §C）。
    #[serde(rename = "dm-session-ingest")]
    IngestV2,
}

impl SessionContract {
    /// 上传信封 `classification.sourceFamily`，同时是本地 journal 里持久化的值。
    #[must_use]
    pub const fn source_family(self) -> &'static str {
        match self {
            Self::LocalDevV1 => LOCAL_DEV_SOURCE_FAMILY,
            Self::IngestV2 => INGEST_V2_SOURCE_FAMILY,
        }
    }

    /// 从持久化的 source family 还原。未知值返回 `None`——调用方 fail-closed，
    /// 绝不落到一个默认车道（journal 里 v33 之前的 `legacy` 行走的就是这条路）。
    #[must_use]
    pub fn from_source_family(value: &str) -> Option<Self> {
        match value {
            LOCAL_DEV_SOURCE_FAMILY => Some(Self::LocalDevV1),
            INGEST_V2_SOURCE_FAMILY => Some(Self::IngestV2),
            _ => None,
        }
    }

    /// `session.json` 与服务端注册体的 `sessionSchemaVersion`。
    #[must_use]
    pub const fn session_schema_version(self, audio_enabled: bool) -> &'static str {
        match (self, audio_enabled) {
            (Self::LocalDevV1, true) => SESSION_SCHEMA_V3,
            (Self::LocalDevV1, false) => SESSION_SCHEMA_V2,
            (Self::IngestV2, _) => INGEST_V2_SESSION_SCHEMA,
        }
    }

    /// clip 侧 `meta.json` / manifest 的 `schema_version`，也是信封的
    /// `classification.artifactSchemaVersion`。两者必须同源：申报一代、落盘另一代
    /// 是这条链上最难查的一种不一致。
    #[must_use]
    pub const fn recording_schema_version(self, audio_enabled: bool) -> &'static str {
        match (self, audio_enabled) {
            (Self::LocalDevV1, true) => RECORDING_SCHEMA_V2,
            (Self::LocalDevV1, false) => RECORDING_SCHEMA_V1,
            (Self::IngestV2, _) => INGEST_V2_RECORDING_SCHEMA,
        }
    }

    /// 该契约 preflight 的唯一通过终态。
    #[must_use]
    pub const fn preflight_pass_result(self) -> &'static str {
        match self {
            Self::LocalDevV1 => DEV_NON_TRAINABLE_PASS,
            Self::IngestV2 => PREFLIGHT_PASS,
        }
    }

    /// 该契约的目录身份是否携带 legacy 的 `mode` 字面量。
    ///
    /// 中性契约返回 `None`，意思是"目录身份不再自称车道"，**不是**"车道未知"：
    /// 目录从哪条通道来由验签用的那把钉死的公钥回答，那本来就是更强的证据。
    #[must_use]
    pub const fn legacy_catalog_mode(self) -> Option<&'static str> {
        match self {
            Self::LocalDevV1 => Some(LEGACY_CATALOG_MODE),
            Self::IngestV2 => None,
        }
    }

    /// ready 段在 `meta.json` 里的状态词。
    ///
    /// legacy 把车道名写进了状态里（`non_trainable_ready`）；中性契约只回答"这一段
    /// 封好了没有"。data-demo-converter 早已同时接受裸 `ready`（镜像段走的就是它），
    /// 所以中性一代不需要下游先改。
    #[must_use]
    pub const fn segment_ready_state(self) -> &'static str {
        match self {
            Self::LocalDevV1 => "non_trainable_ready",
            Self::IngestV2 => "ready",
        }
    }

    /// run 正常收尾时 `run.json` 里的状态词。理由同 [`Self::segment_ready_state`]。
    #[must_use]
    pub const fn run_complete_state(self) -> &'static str {
        match self {
            Self::LocalDevV1 => "non_trainable_complete",
            Self::IngestV2 => "complete",
        }
    }

    /// 落盘产物与上传信封是否携带 `non_trainable` / `production_certified` 这类
    /// 训练声明。
    ///
    /// legacy 携带（那些字节就是它的契约，且已经落在存量产物里）；中性一律不带
    /// ——§B 明确新契约不重复携带互相可能打架的字段，服务端的
    /// `retiredClassificationKeys` 会把带了的请求当场 403。
    #[must_use]
    pub const fn declares_training_classification(self) -> bool {
        matches!(self, Self::LocalDevV1)
    }

    /// 从持久化的 `sessions.schema_version` 反解契约。
    ///
    /// 这是**存量 session 的路由方向**：一场采集属于哪一代是创建时写死的历史事实，
    /// 恢复、上传、封存都必须回到它当初那一代，不能按今天的目录重新推断。未知版本
    /// 返回 `None`（`dm-local-dev-session-1` 就在此列：那一代没有目录证据）。
    #[must_use]
    pub fn for_session_schema_version(value: &str) -> Option<Self> {
        match value {
            SESSION_SCHEMA_V2 | SESSION_SCHEMA_V3 => Some(Self::LocalDevV1),
            INGEST_V2_SESSION_SCHEMA => Some(Self::IngestV2),
            _ => None,
        }
    }

    /// 从 clip 侧 `meta.json` 的 `schema_version` 反解契约。
    ///
    /// 与 [`Self::for_session_schema_version`] 成对：段是后来落的，它自己必须说得清
    /// 属于哪一代；与**会话**那一代是否一致由拿得到 journal 行的调用方对账。
    #[must_use]
    pub fn for_recording_schema_version(value: &str) -> Option<Self> {
        match value {
            RECORDING_SCHEMA_V1 | RECORDING_SCHEMA_V2 => Some(Self::LocalDevV1),
            INGEST_V2_RECORDING_SCHEMA => Some(Self::IngestV2),
            _ => None,
        }
    }

    /// 该 session schema 版本是否声明了音频世代。
    ///
    /// legacy 用 `-3` 表达"有音频"；中性只有一个值，音频与否不在 session schema 里。
    #[must_use]
    pub fn session_schema_declares_audio(value: &str) -> Option<bool> {
        match value {
            SESSION_SCHEMA_V2 => Some(false),
            SESSION_SCHEMA_V3 => Some(true),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_generation_vocabulary_is_unchanged_byte_for_byte() {
        // 断代不得动 legacy 的任何一个字节：存量产物已经落在磁盘和 R2 上，
        // 服务端 legacy 家族也按字面量校验。
        let legacy = SessionContract::LocalDevV1;
        assert_eq!(
            legacy.session_schema_version(false),
            "dm-local-dev-session-2"
        );
        assert_eq!(
            legacy.session_schema_version(true),
            "dm-local-dev-session-3"
        );
        assert_eq!(
            legacy.recording_schema_version(false),
            "dm-local-dev-recording-1"
        );
        assert_eq!(
            legacy.recording_schema_version(true),
            "dm-local-dev-recording-2"
        );
        assert_eq!(legacy.source_family(), "dm-local-dev");
        assert_eq!(legacy.preflight_pass_result(), "dev_non_trainable_pass");
        assert_eq!(legacy.legacy_catalog_mode(), Some("dev_non_trainable"));
        assert_eq!(legacy.segment_ready_state(), "non_trainable_ready");
        assert_eq!(legacy.run_complete_state(), "non_trainable_complete");
        assert!(legacy.declares_training_classification());
    }

    #[test]
    fn the_neutral_generation_has_one_session_schema_and_no_training_words() {
        let neutral = SessionContract::IngestV2;
        // 音频不再分世代：唯一的路由键必须只有一个值（§B）。
        assert_eq!(
            neutral.session_schema_version(false),
            neutral.session_schema_version(true)
        );
        assert_eq!(neutral.session_schema_version(false), "dm-session-ingest-2");
        assert_eq!(
            neutral.recording_schema_version(false),
            neutral.recording_schema_version(true)
        );
        assert_eq!(neutral.recording_schema_version(true), "dm-raw-recording-1");
        assert_eq!(neutral.source_family(), "dm-session-ingest");
        assert_eq!(neutral.preflight_pass_result(), "preflight_pass");
        assert_eq!(neutral.legacy_catalog_mode(), None);
        assert!(!neutral.declares_training_classification());
        // 验收标准逐字：新公共契约里不存在这四个车道词。
        for word in ["dev", "production", "certified", "non_trainable"] {
            for value in [
                neutral.session_schema_version(true),
                neutral.recording_schema_version(true),
                neutral.source_family(),
                neutral.preflight_pass_result(),
                neutral.segment_ready_state(),
                neutral.run_complete_state(),
            ] {
                assert!(
                    !value.contains(word),
                    "neutral contract value {value} still carries the retired word {word}"
                );
            }
        }
    }

    #[test]
    fn session_schema_versions_route_back_to_their_own_generation() {
        for (version, expected) in [
            ("dm-local-dev-session-2", SessionContract::LocalDevV1),
            ("dm-local-dev-session-3", SessionContract::LocalDevV1),
            ("dm-session-ingest-2", SessionContract::IngestV2),
        ] {
            assert_eq!(
                SessionContract::for_session_schema_version(version),
                Some(expected)
            );
        }
        // 未知版本不落任何默认车道——包括没有目录证据的第一代。
        for version in ["dm-local-dev-session-1", "dm-certified-session-2", ""] {
            assert_eq!(SessionContract::for_session_schema_version(version), None);
        }
    }

    #[test]
    fn source_families_round_trip_and_unknown_values_fail_closed() {
        for contract in [SessionContract::LocalDevV1, SessionContract::IngestV2] {
            assert_eq!(
                SessionContract::from_source_family(contract.source_family()),
                Some(contract)
            );
        }
        // journal v33 之前建的行会带 `legacy`（迁移前那一列还没有目录证据）。
        // 它必须解析失败，让调用方按"无法确定契约"处理，而不是被当成 legacy 车道。
        for value in [
            "legacy",
            "dev_non_trainable",
            "legacy_dev_non_trainable",
            "",
        ] {
            assert_eq!(SessionContract::from_source_family(value), None);
        }
    }

    /// 存量 preflight 证据 JSON 的别名。`from_source_family` 刻意**不**认这个词
    /// （那是 journal 的列，v33 已经断代过），serde 认——两者读的是不同的字节，
    /// 而用户机器上那份历史 JSON 必须继续读得回来。
    #[test]
    fn persisted_preflight_evidence_still_deserializes_under_its_legacy_spelling() {
        let legacy: SessionContract = serde_json::from_str("\"dev_non_trainable\"").unwrap();
        assert_eq!(legacy, SessionContract::LocalDevV1);
        let neutral: SessionContract = serde_json::from_str("\"dm-session-ingest\"").unwrap();
        assert_eq!(neutral, SessionContract::IngestV2);
        // 新写入只用中性拼写。
        assert_eq!(
            serde_json::to_string(&SessionContract::LocalDevV1).unwrap(),
            "\"dm-local-dev\""
        );
    }
}

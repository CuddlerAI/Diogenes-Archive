//! `dm-input-calibration-2` 证据文件的类型与序列化（docs/46 §2.2）。
//!
//! 职责边界：本模块只定义 schema 与确定性序列化，保证客户端、云端与
//! 训练管道跨端一致解析。字节层不可变性（临时文件 + 原子 rename +
//! 全文件 SHA-256）由客户端 journal 侧负责，不在本模块。
//!
//! 序列化约定：`to_canonical_json` 输出 pretty JSON + 末尾换行，字段顺序
//! 即结构体声明顺序——同一结构永远得到相同字节，供 hash 承诺使用。

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::{Analysis, Direction, FailReason, Speed, Verdict};

/// 证据文件 schema 版本。
pub const SCHEMA_VERSION: &str = "dm-input-calibration-2";

/// 标定种类：全量对表、会话前单圈抽检，或自由测量段。
///
/// `Free` 是测绘工具而不是标定（docs/60 §6 Q4）：不预设方向、转速与圈数，
/// 只累计计数并如实报告，供人工推算 `vertical_scale_ratio` / `fov_axis` /
/// 开镜常数。它**没有验收裁决**——`verdict` 为 `None`，且永远不进
/// "有效标定"池（journal 侧按 kind 过滤，见 `latest_valid_view_calibration`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CalibrationKind {
    Full,
    SpotCheck,
    Free,
}

impl CalibrationKind {
    /// 自由测量段没有计划也没有裁决；`Method`/`EvidenceMeasurement` 的
    /// 计划类字段与 `verdict` 因此必须为 `None`。
    pub const fn is_free(self) -> bool {
        matches!(self, Self::Free)
    }
}

/// 标定发生时引用的目录身份（与 session contract 同规格）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatalogRef {
    pub catalog_version: String,
    pub recording_profile_id: String,
    pub recording_profile_hash: String,
    pub device_profile_id: String,
    pub device_profile_hash: String,
}

/// 判定时使用的政策快照（来自 recording profile 的 `view_scale` 块）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PolicyRef {
    pub expected_counts_per_360: f64,
    pub tolerance_pct: f64,
    pub fast_slow_tolerance_pct: f64,
    pub min_polling_hz: f64,
}

/// 测量方法与版本。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Method {
    pub algorithm_version: String,
    pub polling_estimator_version: String,
    pub input_timestamp_semantics: String,
    /// 计划每段的整圈数；自由测量段不预设圈数，为 `None`。
    pub turns_per_measurement: Option<f64>,
}

/// 单次测量的原始计数轨迹引用（分段事件文件 + hash），可复算 derived。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RawTraceRef {
    pub file: String,
    pub sha256: String,
}

/// 单次测量的证据条目。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvidenceMeasurement {
    /// 腰射为 "hipfire"，开镜状态按目录 `ads_states[].state_id`。
    ///
    /// `None` = **未声明**：自由测量段做的是什么状态由标注员现场决定，程序
    /// 无从得知（按住开镜转几圈和腰射转几圈，采集侧看到的都只是计数）。
    /// 这里绝不能默认填 "hipfire"——那会把一段 8 倍镜的测量记成腰射，
    /// 是本 schema 最不该产出的那种「看起来正常的错数据」。
    pub state_id: Option<String>,
    /// 计划声明的方向；自由测量段无声明，为 `None`（不写 0/占位值，
    /// 「没有声明」与「声明了某个方向」必须可区分）。
    pub direction: Option<Direction>,
    /// 计划声明的转速档；自由测量段为 `None`。
    pub speed: Option<Speed>,
    /// 本段旋转的整圈数；自由测量段不预设圈数，为 `None`。
    pub turns: Option<f64>,
    pub total_counts_x: i64,
    pub total_counts_y: i64,
    pub event_count: u64,
    pub duration_ms: f64,
    pub polling_hz_p50: Option<f64>,
    pub polling_hz_p05: Option<f64>,
    pub polling_estimated_report_count: u64,
    pub delivery_group_count: u64,
    pub max_delivery_batch_size: u64,
    pub buffered_report_count: u64,
    pub start_marker_qpc: i64,
    pub end_marker_qpc: i64,
    pub raw_trace: RawTraceRef,
}

/// 某个开镜状态的实测尺度。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PerStateScale {
    pub state_id: String,
    pub counts_per_360_measured: f64,
}

/// 由测量派生的汇总（可由 `measurements` + 算法版本重算）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Derived {
    pub counts_per_360_measured: Option<f64>,
    pub deviation_pct: Option<f64>,
    pub repeat_spread_pct: Option<f64>,
    pub left_right_delta_pct: Option<f64>,
    pub fast_slow_delta_pct: Option<f64>,
    pub polling_hz_p50: Option<f64>,
    pub per_state: Vec<PerStateScale>,
}

impl Derived {
    /// 自由测量段没有可派生的汇总：圈数未声明，`counts_per_360` 无从算起。
    /// 事实全部留在 `measurements[]`（总计数、时长、回报率），由人工按
    /// docs/59 的步骤换算——这里不猜、不填占位值。
    pub fn unanalyzed() -> Self {
        Self {
            counts_per_360_measured: None,
            deviation_pct: None,
            repeat_spread_pct: None,
            left_right_delta_pct: None,
            fast_slow_delta_pct: None,
            polling_hz_p50: None,
            per_state: Vec::new(),
        }
    }

    /// 从主状态（腰射）的验收结果构造；开镜状态另行追加 `per_state`。
    pub fn from_analysis(analysis: &Analysis) -> Self {
        Self {
            counts_per_360_measured: analysis.counts_per_360_measured,
            deviation_pct: analysis.deviation_pct,
            repeat_spread_pct: analysis.repeat_spread_pct,
            left_right_delta_pct: analysis.left_right_delta_pct,
            fast_slow_delta_pct: analysis.fast_slow_delta_pct,
            polling_hz_p50: analysis.polling_hz_p50,
            per_state: Vec::new(),
        }
    }
}

/// `input-calibration.json` 的完整结构。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CalibrationEvidence {
    pub schema_version: String,
    pub calibration_id: String,
    pub kind: CalibrationKind,
    pub parent_calibration_id: Option<String>,
    pub created_utc: String,
    pub qpc_anchor: i64,
    pub qpc_frequency: i64,
    pub game_id: String,
    pub catalog_ref: CatalogRef,
    pub policy: PolicyRef,
    pub method: Method,
    pub measurements: Vec<EvidenceMeasurement>,
    pub derived: Derived,
    /// 验收裁决；自由测量段没有裁决，为 `None`。两头都锁死（见
    /// `from_json` 的 `KindVerdictMismatch`）：free 必无裁决、正式标定必有，
    /// 不给"拿 fail 当占位"这种说谎留门。
    pub verdict: Option<Verdict>,
    pub fail_reasons: Vec<FailReason>,
}

/// 证据解析错误。
#[derive(Debug)]
pub enum EvidenceError {
    Json(serde_json::Error),
    SchemaVersionMismatch {
        found: String,
    },
    MethodVersionMismatch {
        algorithm: String,
        polling_estimator: String,
        timestamp_semantics: String,
    },
    /// `kind` 与 `verdict` 的存在性不成对：free 必无裁决、其余必有。
    KindVerdictMismatch {
        kind: CalibrationKind,
        has_verdict: bool,
    },
}

impl fmt::Display for EvidenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Json(err) => write!(f, "invalid calibration evidence json: {err}"),
            Self::SchemaVersionMismatch { found } => write!(
                f,
                "calibration evidence schema mismatch: found {found:?}, expected {SCHEMA_VERSION:?}"
            ),
            Self::MethodVersionMismatch {
                algorithm,
                polling_estimator,
                timestamp_semantics,
            } => write!(
                f,
                "calibration evidence method mismatch: algorithm={algorithm:?}, polling_estimator={polling_estimator:?}, timestamp_semantics={timestamp_semantics:?}"
            ),
            Self::KindVerdictMismatch { kind, has_verdict } => write!(
                f,
                "calibration evidence kind/verdict mismatch: kind={kind:?} carries verdict={has_verdict} (free measurements have no verdict; every other kind must have one)"
            ),
        }
    }
}

impl std::error::Error for EvidenceError {}

impl CalibrationEvidence {
    /// 确定性序列化：pretty JSON + 末尾换行，逐字节稳定，供 SHA-256 承诺。
    pub fn to_canonical_json(&self) -> serde_json::Result<String> {
        serde_json::to_string_pretty(self).map(|mut text| {
            text.push('\n');
            text
        })
    }

    /// 解析并校验 schema 版本；版本不符按 fail-closed 拒绝而不是猜测兼容。
    pub fn from_json(text: &str) -> Result<Self, EvidenceError> {
        // 先看 envelope 版本，再反序列化完整结构。这样 v1 缺少 v2 字段时
        // 仍能得到明确的版本不匹配，而不是误报普通 JSON 损坏。
        let value: serde_json::Value = serde_json::from_str(text).map_err(EvidenceError::Json)?;
        let found = value
            .get("schema_version")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        if found != SCHEMA_VERSION {
            return Err(EvidenceError::SchemaVersionMismatch {
                found: found.to_string(),
            });
        }
        let evidence: Self = serde_json::from_value(value).map_err(EvidenceError::Json)?;
        if evidence.method.algorithm_version != crate::ALGORITHM_VERSION
            || evidence.method.polling_estimator_version != crate::POLLING_ESTIMATOR_VERSION
            || evidence.method.input_timestamp_semantics != crate::INPUT_TIMESTAMP_SEMANTICS
        {
            return Err(EvidenceError::MethodVersionMismatch {
                algorithm: evidence.method.algorithm_version,
                polling_estimator: evidence.method.polling_estimator_version,
                timestamp_semantics: evidence.method.input_timestamp_semantics,
            });
        }
        // kind 与 verdict 的存在性必须成对。自由测量段没有验收，正式标定
        // 必须有；两个方向都拦住，读侧就不必再猜"verdict 缺了是自由段还是
        // 写坏了"。
        if evidence.kind.is_free() != evidence.verdict.is_none() {
            return Err(EvidenceError::KindVerdictMismatch {
                kind: evidence.kind,
                has_verdict: evidence.verdict.is_some(),
            });
        }
        Ok(evidence)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> CalibrationEvidence {
        CalibrationEvidence {
            schema_version: SCHEMA_VERSION.to_string(),
            calibration_id: "11111111-2222-3333-4444-555555555555".to_string(),
            kind: CalibrationKind::Full,
            parent_calibration_id: None,
            created_utc: "2026-07-28T12:00:00Z".to_string(),
            qpc_anchor: 1_000,
            qpc_frequency: 10_000_000,
            game_id: "halo-infinite-steam-release-test".to_string(),
            catalog_ref: CatalogRef {
                catalog_version: "dev-1".to_string(),
                recording_profile_id: "halo-infinite-steam-release-test-recording-v1".to_string(),
                recording_profile_hash: "sha256:aa".to_string(),
                device_profile_id: "dev-device-v1".to_string(),
                device_profile_hash: "sha256:bb".to_string(),
            },
            policy: PolicyRef {
                expected_counts_per_360: 18_000.0,
                tolerance_pct: 0.5,
                fast_slow_tolerance_pct: 1.0,
                min_polling_hz: 500.0,
            },
            method: Method {
                algorithm_version: crate::ALGORITHM_VERSION.to_string(),
                polling_estimator_version: crate::POLLING_ESTIMATOR_VERSION.to_string(),
                input_timestamp_semantics: crate::INPUT_TIMESTAMP_SEMANTICS.to_string(),
                turns_per_measurement: Some(3.0),
            },
            measurements: vec![EvidenceMeasurement {
                state_id: Some("hipfire".to_string()),
                direction: Some(Direction::Cw),
                speed: Some(Speed::Normal),
                turns: Some(3.0),
                total_counts_x: 54_000,
                total_counts_y: -12,
                event_count: 6_000,
                duration_ms: 6_000.0,
                polling_hz_p50: Some(1_000.0),
                polling_hz_p05: Some(1_000.0),
                polling_estimated_report_count: 5_999,
                delivery_group_count: 6_000,
                max_delivery_batch_size: 1,
                buffered_report_count: 0,
                start_marker_qpc: 1_000,
                end_marker_qpc: 60_001_000,
                raw_trace: RawTraceRef {
                    file: "calibration/1111/m00.events.jsonl.zst".to_string(),
                    sha256: "sha256:cc".to_string(),
                },
            }],
            derived: Derived {
                counts_per_360_measured: Some(18_000.0),
                deviation_pct: Some(0.0),
                repeat_spread_pct: Some(0.0),
                left_right_delta_pct: None,
                fast_slow_delta_pct: None,
                polling_hz_p50: Some(1_000.0),
                per_state: vec![PerStateScale {
                    state_id: "zoom_1".to_string(),
                    counts_per_360_measured: 27_000.0,
                }],
            },
            verdict: Some(Verdict::Pass),
            fail_reasons: Vec::new(),
        }
    }

    /// 自由测量段样本：无计划字段、无裁决，事实只在 measurements 里。
    fn free_sample() -> CalibrationEvidence {
        let mut evidence = sample();
        evidence.kind = CalibrationKind::Free;
        evidence.parent_calibration_id = None;
        evidence.method.turns_per_measurement = None;
        evidence.measurements[0].state_id = None;
        evidence.measurements[0].direction = None;
        evidence.measurements[0].speed = None;
        evidence.measurements[0].turns = None;
        evidence.derived = Derived::unanalyzed();
        evidence.verdict = None;
        evidence.fail_reasons = Vec::new();
        evidence
    }

    #[test]
    fn canonical_json_roundtrips_and_is_stable() {
        let evidence = sample();
        let first = evidence.to_canonical_json().expect("serializes");
        let second = evidence.to_canonical_json().expect("serializes");
        assert_eq!(first, second, "canonical serialization must be byte-stable");
        assert!(first.ends_with('\n'));
        let parsed = CalibrationEvidence::from_json(&first).expect("parses");
        assert_eq!(parsed, evidence);
    }

    #[test]
    fn schema_version_is_fail_closed() {
        let mut evidence = sample();
        evidence.schema_version = "dm-input-calibration-99".to_string();
        let text = evidence.to_canonical_json().expect("serializes");
        match CalibrationEvidence::from_json(&text) {
            Err(EvidenceError::SchemaVersionMismatch { found }) => {
                assert_eq!(found, "dm-input-calibration-99");
            }
            other => panic!("expected schema mismatch, got {other:?}"),
        }
    }

    #[test]
    fn legacy_schema_is_rejected_before_missing_v2_fields_are_decoded() {
        let mut value = serde_json::to_value(sample()).expect("serializes");
        value["schema_version"] = "dm-input-calibration-1".into();
        value["method"]
            .as_object_mut()
            .expect("method object")
            .remove("polling_estimator_version");
        let text = serde_json::to_string(&value).expect("serializes");
        assert!(matches!(
            CalibrationEvidence::from_json(&text),
            Err(EvidenceError::SchemaVersionMismatch { found })
                if found == "dm-input-calibration-1"
        ));
    }

    #[test]
    fn obsolete_polling_method_is_fail_closed() {
        let mut evidence = sample();
        evidence.method.polling_estimator_version = "message-loop-interval-1".to_string();
        let text = evidence.to_canonical_json().expect("serializes");
        assert!(matches!(
            CalibrationEvidence::from_json(&text),
            Err(EvidenceError::MethodVersionMismatch { .. })
        ));
    }

    #[test]
    fn free_measurement_roundtrips_without_plan_or_verdict() {
        let evidence = free_sample();
        let text = evidence.to_canonical_json().expect("serializes");
        // 「没有声明」必须以 null 显式出现，不能省字段——省了就分不清
        // 「自由段」与「写漏了」。
        assert!(text.contains(r#""kind": "free""#));
        assert!(text.contains(r#""verdict": null"#));
        assert!(text.contains(r#""turns_per_measurement": null"#));
        // 状态未声明必须是 null，绝不能默认成 "hipfire"——一段开镜测量被
        // 记成腰射，就是这条线一直在防的那种静默错数据。
        assert!(text.contains(r#""state_id": null"#));
        assert!(!text.contains("hipfire"));
        assert!(text.contains(r#""direction": null"#));
        assert!(text.contains(r#""speed": null"#));
        assert!(text.contains(r#""turns": null"#));
        // 事实仍然落盘：总计数与轨迹引用是自由段的全部产出。
        assert!(text.contains(r#""total_counts_x": 54000"#));
        assert!(text.contains(r#""total_counts_y": -12"#));
        let parsed = CalibrationEvidence::from_json(&text).expect("parses");
        assert_eq!(parsed, evidence);
        assert_eq!(parsed.derived.counts_per_360_measured, None);
    }

    #[test]
    fn kind_and_verdict_presence_must_pair_up() {
        // free 带裁决 → 拒。
        let mut lying_free = free_sample();
        lying_free.verdict = Some(Verdict::Pass);
        let text = lying_free.to_canonical_json().expect("serializes");
        assert!(matches!(
            CalibrationEvidence::from_json(&text),
            Err(EvidenceError::KindVerdictMismatch {
                kind: CalibrationKind::Free,
                has_verdict: true
            })
        ));

        // 正式标定缺裁决 → 也拒（不许拿 free 的形状混进有效标定池）。
        let mut verdictless_full = sample();
        verdictless_full.verdict = None;
        let text = verdictless_full.to_canonical_json().expect("serializes");
        assert!(matches!(
            CalibrationEvidence::from_json(&text),
            Err(EvidenceError::KindVerdictMismatch {
                kind: CalibrationKind::Full,
                has_verdict: false
            })
        ));
    }

    #[test]
    fn wire_format_uses_snake_case_strings() {
        let mut evidence = sample();
        evidence.kind = CalibrationKind::SpotCheck;
        evidence.verdict = Some(Verdict::Fail);
        evidence.fail_reasons = vec![FailReason::PollingBelowFloor];
        let text = evidence.to_canonical_json().expect("serializes");
        assert!(text.contains(r#""kind": "spot_check""#));
        assert!(text.contains(r#""verdict": "fail""#));
        assert!(text.contains(r#""polling_below_floor""#));
        assert!(text.contains(r#""schema_version": "dm-input-calibration-2""#));
    }

    #[test]
    fn derived_from_analysis_copies_summary() {
        use crate::{Policy, analyze};
        let records = vec![];
        let policy = Policy {
            expected_counts_per_360: 18_000.0,
            tolerance_pct: 0.5,
            fast_slow_tolerance_pct: 1.0,
            min_polling_hz: 500.0,
            min_events_per_measurement: 100,
        };
        let analysis = analyze(&records, &policy);
        let derived = Derived::from_analysis(&analysis);
        assert_eq!(derived.counts_per_360_measured, None);
        assert!(derived.per_state.is_empty());
    }
}

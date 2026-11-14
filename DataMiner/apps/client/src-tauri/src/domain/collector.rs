use serde::{Deserialize, Serialize};
use serde_json::Value;

use capture_core::ingest_contract::SessionContract;

use crate::platform::windows::catalog::CatalogStatus;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CollectorState {
    #[default]
    Idle,
    Discovering,
    Bound,
    Preflighting,
    Starting,
    Recording,
    Pausing,
    Paused,
    Stopping,
    Completed,
    Faulted,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GameSummary {
    pub game_id: String,
    pub display_name: String,
    pub recording_profile_id: String,
    pub recording_profile_version: String,
    pub recording_profile_hash: String,
    pub device_profile_id: String,
    pub mapping_id: String,
    pub mapping_version: String,
    pub width: u32,
    pub height: u32,
    pub fps_num: u32,
    pub fps_den: u32,
    pub non_trainable: bool,
    /// 这一款录起来会写成哪一代摄取契约（issue #855 §B）。逐游戏，不是逐目录：
    /// 每游戏一份 bundle、每游戏独立切 current。
    pub session_contract: SessionContract,
    pub admission: GameAdmission,
    pub admission_reason: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GameAdmission {
    Admitted,
    #[cfg_attr(feature = "local-catalog-only", allow(dead_code))]
    Onboarding,
    #[cfg_attr(feature = "local-catalog-only", allow(dead_code))]
    Unknown,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowCandidate {
    pub candidate_id: String,
    pub game_id: String,
    pub display_name: String,
    pub executable_name: String,
    pub client_width: u32,
    pub client_height: u32,
    pub dpi: u32,
    pub foreground: bool,
    pub non_trainable: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BoundTargetSummary {
    pub target_id: String,
    pub game_id: String,
    pub display_name: String,
    pub executable_name: String,
    /// 实测 build 指纹；受保护进程拒绝观察时为 `None`，原因见下一字段
    /// （docs/55 §3.1）。前端据此展示"已测到指纹 / 仅进程名"两种身份强度。
    pub executable_sha256: Option<String>,
    pub executable_sha256_missing_reason: Option<String>,
    pub process_id: u32,
    pub client_width: u32,
    pub client_height: u32,
    pub dpi: u32,
    pub recording_profile_id: String,
    pub recording_profile_version: String,
    pub recording_profile_hash: String,
    pub device_profile_id: String,
    pub device_profile_version: String,
    pub device_profile_hash: String,
    pub game_profile_version: String,
    pub game_profile_hash: String,
    pub mapping_id: String,
    pub mapping_version: String,
    pub mapping_hash: String,
    /// 绑定之后这场采集会按哪一代契约落盘与上传。断代前这里是 `catalogMode`
    /// （`dev_non_trainable`），一个同时表达环境、渠道与训练资格的词（issue #855 §2）。
    pub session_contract: SessionContract,
    pub catalog_id: String,
    pub catalog_version: String,
    pub catalog_hash: String,
    pub non_trainable: bool,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureStatus {
    pub session_id: String,
    pub run_id: String,
    pub run_seq: u32,
    pub frame_count: u64,
    pub wgc_callback_count: u64,
    pub active_segment: u32,
    pub ready_segments: u32,
    pub duplicate_frames: u64,
    pub source_superseded: u64,
    pub capture_queue_depth: usize,
    pub capture_queue_drops: u64,
    pub input_event_count: u64,
    pub input_queue_depth: usize,
    pub input_queue_overflows: u64,
    pub pressed_contract_keys: Vec<u16>,
    pub mouse_buttons: Vec<u8>,
    pub elapsed_ms: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageState {
    #[default]
    Unknown,
    Ready,
    Warning,
    Critical,
    Unavailable,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageStatus {
    pub state: StorageState,
    pub free_bytes: Option<u64>,
    pub required_start_bytes: u64,
    pub reserve_bytes: u64,
    pub estimated_remaining_seconds: Option<u64>,
    pub start_allowed: bool,
    pub stop_requested: bool,
}

/// 上传面的运行时快照（docs/65 J-29/J-30）。
///
/// 事实源是 journal 的 `upload_jobs` / `session_artifact_uploads` 两张表加上
/// uploader worker 自己的存活与凭据状态；由 uploader 每轮循环推给编排器，
/// **只描述不决策**——上传状态永远不改 RAW 事实，也不影响采集。
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UploadStatus {
    /// worker 线程活着并在循环。默认 `false`：uploader 启动失败时没有任何人
    /// 会把它翻成 true，这正是「上传器未启动」常驻徽章的判据（J-30）。
    pub uploader_running: bool,
    /// 设备凭据缺席或读不出来——worker 每 3s 空转的那种降级（`uploader.rs`
    /// 凭据分支）。绑定完成后自动翻回 false。
    pub credentials_missing: bool,
    /// `pending` + `retry_wait`：排队等待上传的分段数。
    pub pending_segments: u32,
    /// `running`：正在传输的分段数。
    pub inflight_segments: u32,
    /// `stored`：字节已进 R2、等服务端 SHA-256 复验的分段数。
    pub awaiting_verification_segments: u32,
    /// `rejected`：被服务端终态拒收的分段数。逐段原因在历史页展开。
    pub rejected_segments: u32,
    /// 尚未到达终态（verified/rejected）的会话级 artifact 上传任务数。
    pub pending_session_artifacts: u32,
    /// 最近一次上传失败的稳定错误码（segment 与 session artifact 两队列取最新）。
    pub last_error_code: Option<String>,
    /// 最早的下一次重试时刻（Unix ms）；没有退避中的任务时为 None。
    pub next_retry_unix_ms: Option<i64>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CollectorFault {
    pub code: String,
    pub message: String,
    pub action: String,
    pub retryable: bool,
    pub details_id: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreflightCheckEvidence {
    pub check_id: String,
    pub result: String,
    pub facts: Value,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreflightStatus {
    pub schema_version: String,
    pub preflight_id: String,
    pub request_id: String,
    pub phase: String,
    pub result: String,
    /// 这次预检属于哪一代契约。它决定"通过"写作什么词
    /// （`dev_non_trainable_pass` / `preflight_pass`），服务端迁移 0036 的配对
    /// 触发器逐字镜像同一份映射。
    ///
    /// `alias`：v33 之前落盘的证据 JSON 用 `catalogMode` 这个键，值是
    /// `dev_non_trainable`；两个别名一起让那份历史 JSON 继续读得回来
    /// （`SessionContract` 的值别名见 capture-core）。
    #[serde(alias = "catalogMode")]
    pub session_contract: SessionContract,
    pub session_id: Option<String>,
    pub run_id: Option<String>,
    pub failure_code: Option<String>,
    pub details_id: Option<String>,
    pub evidence_hash: String,
    pub checks: Vec<PreflightCheckEvidence>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CollectorSnapshot {
    pub schema_version: &'static str,
    pub revision: u64,
    pub state: CollectorState,
    /// 车道分类，随目录渠道/绑定目标派生（docs/65 J-26），不再是常量。
    /// 派生规则见 [`Self::derive_lane_classification`]。
    pub non_trainable: bool,
    pub production_certified: bool,
    pub catalog: CatalogStatus,
    pub preflight: Option<PreflightStatus>,
    pub bound_target: Option<BoundTargetSummary>,
    pub capture: Option<CaptureStatus>,
    pub storage: StorageStatus,
    pub rebind_required: bool,
    pub rebind_game_id: Option<String>,
    pub fault: Option<CollectorFault>,
    /// v5 新增：上传面（docs/65 J-29/J-30）。
    pub upload: UploadStatus,
}

impl Default for CollectorSnapshot {
    fn default() -> Self {
        Self {
            schema_version: "dm-collector-snapshot-5",
            revision: 0,
            state: CollectorState::Idle,
            non_trainable: true,
            production_certified: false,
            catalog: CatalogStatus::default(),
            preflight: None,
            bound_target: None,
            capture: None,
            storage: StorageStatus::default(),
            rebind_required: false,
            rebind_game_id: None,
            fault: None,
            upload: UploadStatus::default(),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DiscoverWindowsRequest {
    pub game_id: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BindWindowRequest {
    pub candidate_id: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StartSessionRequest {
    pub request_id: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StopSessionRequest {
    pub reason: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MarkerKind {
    Manual,
}

impl MarkerKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AddMarkerRequest {
    pub request_id: String,
    pub kind: MarkerKind,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MarkerAudit {
    pub marker_id: String,
    pub session_id: String,
    pub run_id: String,
    pub seq: u32,
    pub kind: String,
    pub qpc: i64,
    pub qpc_frequency: i64,
    pub run_frame: u64,
    pub session_frame: u64,
    pub video_time_us: u64,
    pub created_at_utc: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HeartbeatAudit {
    pub heartbeat_id: String,
    pub session_id: String,
    pub run_id: String,
    pub seq: u32,
    pub qpc: i64,
    pub qpc_frequency: i64,
    pub collector: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionIdRequest {
    pub session_id: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeleteSessionRequest {
    pub session_id: String,
    pub confirmation: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OverlaySnapshot {
    pub recording_state: CollectorState,
    /// 当前会话的车道分类（docs/65 J-26）。HUD 在录制期常驻显示。certified 车道
    /// 废除后（docs/100）只剩一条车道，这两项因此恒 `true` / `false`；保留派生而不
    /// 是写死常量，是因为它们的事实源（绑定目标的 selection）没变。
    pub non_trainable: bool,
    pub production_certified: bool,
    pub elapsed_ms: u64,
    pub segment_seq: u32,
    pub capture_health: &'static str,
    pub storage_state: StorageState,
    pub storage_free_bytes: Option<u64>,
    pub storage_remaining_seconds: Option<u64>,
    pub storage_stop_requested: bool,
    pub pressed_contract_keys: Vec<u16>,
    pub mouse_buttons: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OverlayAction {
    PauseResume,
    AddMarker,
    Stop,
    ShowMainWindow,
}

impl CollectorSnapshot {
    /// 把车道分类从常量改成派生事实（docs/65 J-26）。
    ///
    /// 绑定目标在场时以它为准——那一项来自已验签 catalog 的 selection；未绑定时
    /// 取那条硬约束本身：客户端与录制链路的任何位置都不得主张可训练性
    /// （AGENTS.md、docs/100）。断代前这里读的是目录级的 `catalog.mode.non_trainable()`
    /// ——那个词随 issue #855 §A.4 退役，而它在这里恒 true，所以逐字等价。
    /// `production_certified` 是 first_party 时代的遗留声明，恒 false
    /// （docs/100 §6：归尸体清理刀）。
    /// 仅描述展示面：journal 与上传信封的分类另有独立事实源（`CatalogSelection`），
    /// 服务端分类主权不受这里影响。
    pub fn derive_lane_classification(&mut self) {
        self.non_trainable = match &self.bound_target {
            Some(bound) => bound.non_trainable,
            None => capture_core::ingest_contract::CLIENT_CAPTURE_IS_ALWAYS_NON_TRAINABLE,
        };
        self.production_certified = false;
    }

    #[must_use]
    pub fn overlay_snapshot(&self) -> OverlaySnapshot {
        let capture = self.capture.as_ref();
        OverlaySnapshot {
            recording_state: self.state,
            non_trainable: self.non_trainable,
            production_certified: self.production_certified,
            elapsed_ms: capture.map_or(0, |status| status.elapsed_ms),
            segment_seq: capture.map_or(0, |status| status.active_segment),
            capture_health: if self.fault.is_some() {
                "fault"
            } else {
                "healthy"
            },
            storage_state: self.storage.state,
            storage_free_bytes: self.storage.free_bytes,
            storage_remaining_seconds: self.storage.estimated_remaining_seconds,
            storage_stop_requested: self.storage.stop_requested,
            pressed_contract_keys: Vec::new(),
            mouse_buttons: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_contract_uses_camel_case_and_hides_native_handles() {
        let json = serde_json::to_value(CollectorSnapshot::default()).unwrap();
        assert_eq!(json["schemaVersion"], "dm-collector-snapshot-5");
        assert_eq!(json["rebindRequired"], false);
        assert_eq!(json["rebindGameId"], serde_json::Value::Null);
        assert_eq!(json["state"], "idle");
        assert_eq!(json["storage"]["state"], "unknown");
        // v5 上传面：默认即「上传器未启动」的 fail-visible 形状（docs/65 J-30）。
        assert_eq!(json["upload"]["uploaderRunning"], false);
        assert_eq!(json["upload"]["pendingSegments"], 0);
        assert_eq!(json["upload"]["lastErrorCode"], serde_json::Value::Null);
        assert!(json.get("hwnd").is_none());
    }

    #[test]
    fn overlay_contract_starts_empty_without_an_input_service() {
        let overlay = CollectorSnapshot::default().overlay_snapshot();
        assert_eq!(overlay.recording_state, CollectorState::Idle);
        assert!(overlay.non_trainable);
        assert!(!overlay.production_certified);
        assert!(overlay.pressed_contract_keys.is_empty());
        assert!(overlay.mouse_buttons.is_empty());
    }

    fn bound_summary(session_contract: SessionContract, non_trainable: bool) -> BoundTargetSummary {
        BoundTargetSummary {
            target_id: "t".into(),
            game_id: "game".into(),
            display_name: "Game".into(),
            executable_name: "game.exe".into(),
            executable_sha256: None,
            executable_sha256_missing_reason: None,
            process_id: 1,
            client_width: 1920,
            client_height: 1080,
            dpi: 96,
            recording_profile_id: "rp".into(),
            recording_profile_version: "1".into(),
            recording_profile_hash: "h".into(),
            device_profile_id: "dp".into(),
            device_profile_version: "1".into(),
            device_profile_hash: "h".into(),
            game_profile_version: "1".into(),
            game_profile_hash: "h".into(),
            mapping_id: "m".into(),
            mapping_version: "1".into(),
            mapping_hash: "h".into(),
            session_contract,
            catalog_id: "c".into(),
            catalog_version: "1".into(),
            catalog_hash: "h".into(),
            non_trainable,
        }
    }

    // docs/65 J-26：车道分类随目录渠道/绑定目标派生。certified 车道废除后
    // （docs/100）只剩一条车道，HUD 因此在任何状态下都不该说出「可训练」。
    #[test]
    fn the_hud_never_claims_a_trainable_lane() {
        let mut snapshot = CollectorSnapshot::default();

        // 空目录（release 默认）：什么都录不了。
        snapshot.derive_lane_classification();
        assert!(snapshot.non_trainable);
        assert!(!snapshot.production_certified);

        // 有可录条目，分类不变。
        snapshot.catalog.entry_counts.games = 1;
        snapshot.derive_lane_classification();
        assert!(snapshot.non_trainable);
        assert!(!snapshot.production_certified);

        // 绑定目标在场时以 selection 的分类为准，覆盖目录级派生。
        snapshot.bound_target = Some(bound_summary(SessionContract::LocalDevV1, true));
        snapshot.derive_lane_classification();
        assert!(snapshot.non_trainable);
        assert!(!snapshot.production_certified);
        assert!(snapshot.overlay_snapshot().non_trainable);
        assert!(!snapshot.overlay_snapshot().production_certified);
    }
}

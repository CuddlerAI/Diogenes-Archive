//! 客户端行为遥测：把本地 `client_event_outbox` 排空到 console-api 的
//! `POST /app/v1/client-events`（docs/23 §4）。
//!
//! 这条路是**旁路**（11 §8.2 的硬纪律）：本地缓冲、失败退避、满了丢最旧、
//! 永不阻塞、永不 panic、绝不影响录制主链路。上报故障的最坏后果是「仓库里
//! 少几条遥测」，绝不能变成录制故障。
//!
//! 结构照抄 mirror.rs（排队/游标/退避语义已被它的单测验证过形状）：
//!   - `client_event_outbox` + `client_event_cursor`（journal v11，v31 起按账号环境分区）；
//!   - seq AUTOINCREMENT 不复用，游标推进即删行；
//!   - 退避档 [15, 60, 300, 900, 3600] 秒。
//!
//! 差异有三处，都来自 client-events 契约本身：
//!   - `event_id` 是客户端生成的 uuid v4——全链幂等键（服务端
//!     `ON CONFLICT (event_id) DO NOTHING`，BQ MERGE 再去重一次）；
//!   - 批上限 100（`clientEventBatchSchema.max(100)`），不是 mirror 的 500；
//!   - 服务端 Zod `.strict()` 拒收（非鉴权类 4xx）是**毒批**：重试永远不会
//!     变好，丢弃该批并推进游标，把丢弃计入 `dropped_events` 让它可观测。
//!
//! 事件从两处进来：
//!   - [`ClientEventRecorder::on_diagnostic_event`]——挂在 DiagnosticService
//!     的 sink 上，吃**已脱敏**的诊断事件 JSON，映射成契约 kinds（A 类 +
//!     capture_rollup 聚合）；
//!   - [`ClientEventRecorder::record`]——orchestrator 的 preflight 直报点
//!     （B 类，带 windowsBuild/驱动字段）。

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::Context;
use crossbeam_channel::{Receiver, Sender, bounded};
use parking_lot::Mutex;
use rusqlite::{Connection, params};
use serde_json::{Value, json};

use crate::services::account::{
    AccountEnvironment, CANONICAL_PRODUCTION_ACCOUNT_API_BASE_URL, ResolvedAccountConfiguration,
    targets_production_account_host,
};
use crate::services::credentials::{self, CredentialKind, CredentialStore, DeviceCredential};
use crate::services::diagnostics::{DiagnosticCorrelation, DiagnosticPayload, DiagnosticService};
use crate::services::journal::SessionJournal;

/// 单批上限，与 console-api `clientEventBatchSchema` 的 `.max(100)` 一致。
pub const MAX_BATCH: usize = 100;

/// 单环境环形缓冲上限（11 §8.3：永远不因上报积压占满用户磁盘）。按每行 ≤1KB
/// 估算每个 staging/production/unknown 分区约 10MB，三者合计最多约 30MB。
pub const MAX_OUTBOX_ROWS: i64 = 10_000;

const BACKOFF_SECONDS: [i64; 5] = [15, 60, 300, 900, 3_600];
const WORKER_POLL_INTERVAL: Duration = Duration::from_secs(3);

/// console-api 基址的**专用覆盖**（`/app/v1/client-events`）。
///
/// # 为什么它不再自己兜底
///
/// 这条原本的语义是「未配置就待机」，注释写着「不伪造一个不存在的默认生产地址」。
/// 那句话在写下时成立——console-api 当时确实没有自定义域名（docs/25 开放项 7）。
/// 但域名早已在服务，而账号面与事件面打的是同一个服务的同一个 `/app/v1` 面。
///
/// 于是留下来的不是「安全的待机」，是一个看不见的黑洞：生产库 `client_events`
/// 至今 0 行，采集员机器上连续弹 31 次失败而服务端一无所知（docs/101 §3.4）。
/// 因此它必须复用账号面已通过 fail-closed 校验的环境和 base，而不能独自选择 production。
///
/// 所以本变量降级为**覆盖**：设了就用它（`tools/run-upload-intercept-lab.ps1` 靠它
/// 把上报打到本地拦截器），没设就使用账号面正在用的那个基址。**显式空串仍然表示
/// 关掉上报**。
///
/// 刻意不新增第二个「生产默认地址」常量：两处各存一份，换环境时必然给出两个不同的
/// 目标，而那种错会表现成「登录成功但事件不上报」，指向完全错误的方向。
const CONSOLE_API_BASE_URL_ENV: &str = "DATAMINER_CONSOLE_API_BASE_URL";

const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

// details 的 schema_version 字面量——与 console-api app-contracts.ts 一字不差。
pub const APP_STARTED_SCHEMA: &str = "dm-client-app-started-1";
pub const RECOVERY_SCHEMA: &str = "dm-client-recovery-1";
pub const TARGET_BOUND_SCHEMA: &str = "dm-client-target-bound-1";
pub const RUN_STARTED_SCHEMA: &str = "dm-client-run-started-1";
pub const RUN_COMPLETED_SCHEMA: &str = "dm-client-run-completed-1";
pub const FAULT_SCHEMA: &str = "dm-client-fault-1";
pub const ENROLLED_SCHEMA: &str = "dm-client-enrolled-1";
pub const PREFLIGHT_SCHEMA: &str = "dm-client-preflight-1";
pub const UPDATE_CHECK_SCHEMA: &str = "dm-client-update-check-1";
pub const CAPTURE_ROLLUP_SCHEMA: &str = "dm-client-capture-rollup-1";

fn account_environment_key(environment: Option<AccountEnvironment>) -> &'static str {
    match environment {
        Some(AccountEnvironment::Staging) => "staging",
        Some(AccountEnvironment::Production) => "production",
        None => "unknown",
    }
}

/// 一条待投递事件，字段对应 `clientEventSchema`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxEvent {
    pub seq: i64,
    pub event_id: String,
    pub occurred_at_utc: String,
    pub kind: String,
    pub severity: String,
    pub session_id: Option<String>,
    pub payload_json: String,
}

/// 一次投递请求；`to_body` 产出服务端契约的请求体。
#[derive(Debug, Clone)]
pub struct OutboxBatch {
    pub events: Vec<OutboxEvent>,
}

impl OutboxBatch {
    pub fn to_body(&self) -> Value {
        let events: Vec<Value> = self
            .events
            .iter()
            .map(|event| {
                let details: Value =
                    serde_json::from_str(&event.payload_json).unwrap_or_else(|_| json!({}));
                let mut body = json!({
                    "eventId": event.event_id,
                    "occurredAt": event.occurred_at_utc,
                    "kind": event.kind,
                    "severity": event.severity,
                    "clientVersion": CLIENT_VERSION,
                    "details": details,
                });
                if let Some(session_id) = &event.session_id {
                    body["sessionId"] = json!(session_id);
                }
                body
            })
            .collect();
        json!({ "events": events })
    }
}

/// 服务端如何处置了这一批。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendDisposition {
    /// 2xx：整批已被接收（重复 event_id 在服务端静默去重）。
    Accepted,
    /// 非鉴权类 4xx：契约拒收。重试不会变好——毒批，调用方丢弃并推进游标。
    /// 鉴权失败（401/403）**不是**这个值：凭据可能轮换恢复，走 Err 退避。
    RejectedByContract,
}

pub trait ClientEventTransport: Send + Sync {
    fn send(&self, batch: &OutboxBatch) -> anyhow::Result<SendDisposition>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DrainOutcome {
    pub sent: usize,
    pub dropped: usize,
    pub deferred: bool,
}

/// 入队一条事件。事件在**入队前**已脱敏（诊断 sink 的输出，或 record 内过
/// sanitize）；这里只负责持久化 + 环形缓冲上限。
pub fn enqueue(
    connection: &Connection,
    account_environment: Option<AccountEnvironment>,
    kind: &str,
    severity: &str,
    session_id: Option<&str>,
    payload_json: &str,
) -> anyhow::Result<()> {
    let environment = account_environment_key(account_environment);
    let event_id = uuid::Uuid::new_v4().to_string();
    let tx = connection.unchecked_transaction()?;
    tx.execute(
        "INSERT INTO client_event_outbox(
            event_id, occurred_at_utc, kind, severity, session_id, payload_json,
            created_at_utc, account_environment
         ) VALUES (?1, strftime('%Y-%m-%dT%H:%M:%fZ','now'), ?2, ?3, ?4, ?5,
                   strftime('%Y-%m-%dT%H:%M:%fZ','now'), ?6)",
        params![
            event_id,
            kind,
            severity,
            session_id,
            payload_json,
            environment
        ],
    )?;
    // 每个环境独立限额；一个长期离线的环境不能挤掉另一个环境的证据。
    let overflow: i64 = tx.query_row(
        "SELECT MAX(COUNT(*) - ?1, 0) FROM client_event_outbox
          WHERE account_environment = ?2",
        params![MAX_OUTBOX_ROWS, environment],
        |row| row.get(0),
    )?;
    if overflow > 0 {
        tx.execute(
            "DELETE FROM client_event_outbox WHERE seq IN (
                SELECT seq FROM client_event_outbox
                 WHERE account_environment = ?2 ORDER BY seq LIMIT ?1
             )",
            params![overflow, environment],
        )?;
        tx.execute(
            "UPDATE client_event_cursor
             SET dropped_events = dropped_events + ?1,
                 updated_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now')
             WHERE account_environment = ?2",
            params![overflow, environment],
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// 取走并清零丢弃计数（app_started 的 `droppedEvents` 字段）。
pub fn take_dropped_events(
    connection: &Connection,
    account_environment: Option<AccountEnvironment>,
) -> anyhow::Result<i64> {
    let environment = account_environment_key(account_environment);
    let dropped: i64 = connection.query_row(
        "SELECT dropped_events FROM client_event_cursor WHERE account_environment = ?1",
        params![environment],
        |row| row.get(0),
    )?;
    if dropped > 0 {
        connection.execute(
            "UPDATE client_event_cursor
             SET dropped_events = 0,
                 updated_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now')
             WHERE account_environment = ?1",
            params![environment],
        )?;
    }
    Ok(dropped)
}

/// 一批已从 outbox 冻结下来、等待发送的事件。
///
/// `last_seq` 是这一批的 seq 上界，也是提交阶段唯一的栅栏：`client_event_outbox.seq`
/// 是 AUTOINCREMENT，发送期间并发入队的事件 seq 必然更大，因此按 `<= last_seq`
/// 推进游标和删行，绝不会误 ack 掉一条还没发出去的事件。
#[derive(Debug, Clone)]
struct FrozenBatch {
    batch: OutboxBatch,
    last_seq: i64,
}

/// 阶段一：短暂持锁，冻结一批待发事件；`None` 表示无待发。
///
/// 只读不写，也不做任何网络 I/O——持锁时长就是一次 `SELECT ... LIMIT 100`。
fn freeze_batch(
    connection: &Connection,
    account_environment: AccountEnvironment,
) -> anyhow::Result<Option<FrozenBatch>> {
    let acked_seq = current_acked_seq(connection, account_environment)?;
    let events = pending_after(connection, account_environment, acked_seq)?;
    if events.is_empty() {
        return Ok(None);
    }
    let last_seq = events.last().expect("batch is non-empty").seq;
    Ok(Some(FrozenBatch {
        batch: OutboxBatch { events },
        last_seq,
    }))
}

/// 阶段三：重新持锁，按 seq 栅栏原子提交这一批的结局。
///
/// 毒批处置：契约拒收（非鉴权 4xx）时丢弃该批并推进游标——留着它只会永远堵住队头。
/// 传输失败只更新遥测自己的 retry/backoff，不碰任何采集事实。
fn commit_send(
    connection: &Connection,
    account_environment: AccountEnvironment,
    frozen: &FrozenBatch,
    sent: anyhow::Result<SendDisposition>,
) -> anyhow::Result<DrainOutcome> {
    let last_seq = frozen.last_seq;
    match sent {
        Ok(SendDisposition::Accepted) => {
            advance_cursor(connection, account_environment, last_seq)?;
            Ok(DrainOutcome {
                sent: frozen.batch.events.len(),
                dropped: 0,
                deferred: false,
            })
        }
        Ok(SendDisposition::RejectedByContract) => {
            let dropped = frozen.batch.events.len() as i64;
            let tx = connection.unchecked_transaction()?;
            tx.execute(
                "UPDATE client_event_cursor
                 SET acked_seq = MAX(acked_seq, ?1),
                     attempt_count = 0,
                     dropped_events = dropped_events + ?2,
                     last_error = 'CLIENT_EVENTS_REJECTED',
                     last_attempt_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now'),
                     updated_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now')
                 WHERE account_environment = ?3",
                params![
                    last_seq,
                    dropped,
                    account_environment_key(Some(account_environment))
                ],
            )?;
            tx.execute(
                "DELETE FROM client_event_outbox
                  WHERE account_environment = ?2 AND seq <= ?1",
                params![last_seq, account_environment_key(Some(account_environment))],
            )?;
            tx.commit()?;
            Ok(DrainOutcome {
                sent: 0,
                dropped: dropped as usize,
                deferred: false,
            })
        }
        Err(_) => {
            // 失败不影响录制：只留稳定错误码并退避。error 可能含 URL、响应体
            // 甚至 header，不能原样落库。
            record_failure(
                connection,
                account_environment,
                "CLIENT_EVENTS_TRANSPORT_FAILED",
            )?;
            Ok(DrainOutcome {
                sent: 0,
                dropped: 0,
                deferred: true,
            })
        }
    }
}

/// 排空一次 outbox：**冻结 → 放锁发送 → 重新持锁提交**三段。语义同
/// mirror::MirrorDeliveryHandle::drain_once，外加毒批处置。
///
/// 中间那段是这个函数存在的全部理由（#846）。journal 只有一个 SQLite 连接、一把
/// 锁，录制心跳也要走它；一旦 HTTP 的 DNS/连接/TLS/请求/响应等待落在锁内，断网时
/// 15 秒 connect timeout 就会把心跳挡在锁外，recorder 事件队列（容量 16、每秒一
/// 个）填满后 fail-closed 停录。所以 `transport.send` 只能发生在两次
/// `with_connection` **之间**，绝不能发生在任何一次里面。
///
/// 因此这个函数拿的是 `&SessionJournal` 而不是 `&Connection`：借到裸连接就意味着
/// 锁已经在调用方手里，那时再发请求，#846 就原样复活了。
pub fn drain_once(
    journal: &SessionJournal,
    account_environment: AccountEnvironment,
    transport: &dyn ClientEventTransport,
) -> anyhow::Result<DrainOutcome> {
    let Some(frozen) =
        journal.with_connection(|connection| freeze_batch(connection, account_environment))?
    else {
        return Ok(DrainOutcome {
            sent: 0,
            dropped: 0,
            deferred: false,
        });
    };
    // 不持锁。这一行可能一直阻塞到 60 秒 request timeout，期间录制照常写 journal。
    let sent = transport.send(&frozen.batch);
    journal
        .with_connection(|connection| commit_send(connection, account_environment, &frozen, sent))
}

/// 距下一次可尝试的秒数；无待发返回 i64::MAX，可发返回 0。
pub fn seconds_until_ready(
    connection: &Connection,
    account_environment: AccountEnvironment,
    now_unix: i64,
) -> anyhow::Result<i64> {
    let environment = account_environment_key(Some(account_environment));
    let acked_seq = current_acked_seq(connection, account_environment)?;
    let has_pending: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM client_event_outbox
                        WHERE account_environment = ?2 AND seq > ?1)",
        params![acked_seq, environment],
        |row| row.get::<_, i64>(0),
    )? == 1;
    if !has_pending {
        return Ok(i64::MAX);
    }
    let (attempt, last_attempt_unix): (i64, Option<i64>) = connection.query_row(
        "SELECT attempt_count,
                CAST(strftime('%s', last_attempt_at_utc) AS INTEGER)
         FROM client_event_cursor WHERE account_environment = ?1",
        params![environment],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let Some(last) = last_attempt_unix else {
        return Ok(0);
    };
    let wait = backoff_for(attempt);
    Ok((last + wait - now_unix).max(0))
}

/// 只读投影（不含凭据、不含 payload），供 UI/诊断。
/// 生产读口尚未接线（与 mirror::cursor_snapshot 同期挂 UI），先保留编译检查。
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorSnapshot {
    pub acked_seq: i64,
    pub pending: i64,
    pub attempt: i64,
    pub dropped_events: i64,
    pub last_error: Option<String>,
}

#[allow(dead_code)]
pub fn cursor_snapshot(
    connection: &Connection,
    account_environment: AccountEnvironment,
) -> anyhow::Result<CursorSnapshot> {
    let environment = account_environment_key(Some(account_environment));
    let acked_seq = current_acked_seq(connection, account_environment)?;
    let pending: i64 = connection.query_row(
        "SELECT COUNT(*) FROM client_event_outbox
          WHERE account_environment = ?2 AND seq > ?1",
        params![acked_seq, environment],
        |row| row.get(0),
    )?;
    let (attempt, dropped_events, last_error): (i64, i64, Option<String>) = connection.query_row(
        "SELECT attempt_count, dropped_events, last_error
         FROM client_event_cursor WHERE account_environment = ?1",
        params![environment],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    Ok(CursorSnapshot {
        acked_seq,
        pending,
        attempt,
        dropped_events,
        last_error,
    })
}

fn current_acked_seq(
    connection: &Connection,
    account_environment: AccountEnvironment,
) -> anyhow::Result<i64> {
    connection
        .query_row(
            "SELECT acked_seq FROM client_event_cursor WHERE account_environment = ?1",
            params![account_environment_key(Some(account_environment))],
            |row| row.get(0),
        )
        .context("client event cursor row is missing")
}

fn pending_after(
    connection: &Connection,
    account_environment: AccountEnvironment,
    acked_seq: i64,
) -> anyhow::Result<Vec<OutboxEvent>> {
    let mut statement = connection.prepare(
        "SELECT seq, event_id, occurred_at_utc, kind, severity, session_id, payload_json
         FROM client_event_outbox
         WHERE account_environment = ?3 AND seq > ?1
         ORDER BY seq
         LIMIT ?2",
    )?;
    let rows = statement.query_map(
        params![
            acked_seq,
            MAX_BATCH as i64,
            account_environment_key(Some(account_environment))
        ],
        |row| {
            Ok(OutboxEvent {
                seq: row.get(0)?,
                event_id: row.get(1)?,
                occurred_at_utc: row.get(2)?,
                kind: row.get(3)?,
                severity: row.get(4)?,
                session_id: row.get(5)?,
                payload_json: row.get(6)?,
            })
        },
    )?;
    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}

fn advance_cursor(
    connection: &Connection,
    account_environment: AccountEnvironment,
    acked_seq: i64,
) -> anyhow::Result<()> {
    let environment = account_environment_key(Some(account_environment));
    let tx = connection.unchecked_transaction()?;
    tx.execute(
        "UPDATE client_event_cursor
         SET acked_seq = MAX(acked_seq, ?1),
             attempt_count = 0,
             last_error = NULL,
             updated_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now')
         WHERE account_environment = ?2",
        params![acked_seq, environment],
    )?;
    tx.execute(
        "DELETE FROM client_event_outbox
          WHERE account_environment = ?2 AND seq <= ?1",
        params![acked_seq, environment],
    )?;
    tx.commit()?;
    Ok(())
}

fn record_failure(
    connection: &Connection,
    account_environment: AccountEnvironment,
    code: &str,
) -> anyhow::Result<()> {
    connection.execute(
        "UPDATE client_event_cursor
         SET attempt_count = attempt_count + 1,
             last_error = ?1,
             last_attempt_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now'),
             updated_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now')
         WHERE account_environment = ?2",
        params![code, account_environment_key(Some(account_environment))],
    )?;
    Ok(())
}

fn backoff_for(attempt: i64) -> i64 {
    let index = (attempt.max(1) - 1).min((BACKOFF_SECONDS.len() - 1) as i64) as usize;
    BACKOFF_SECONDS[index]
}

/// 客户端声明的运行上下文（docs/23 §4.3）：只是 details 里的盖章，
/// 不是权威归属——身份列永远由服务端从设备凭据解析。
#[derive(Debug, Clone, Default)]
pub struct StampContext {
    /// 这场采集的摄取契约 source family（`dm-local-dev` / `dm-session-ingest`）。
    /// 断代前这里盖的是 `catalogMode`，一个同时表达环境、渠道与训练资格的词
    /// （issue #855 §2）；console-api 的 `.strict()` 详情契约两个键都收，
    /// 老客户端的存量 outbox 行因此不会在服务端被拒。
    pub session_contract: Option<String>,
    pub consent_present: Option<bool>,
}

/// 每个 run 的段级聚合状态（capture_rollup 的原料）。
#[derive(Debug, Clone, Default)]
struct RunRollup {
    run_name: Option<String>,
    segment_seconds: Option<u64>,
    queue_depth_max: u64,
    storage_state: Option<String>,
    segments_emitted: u64,
}

/// 事件产点句柄。所有方法**吞掉错误**（tracing 留痕）：旁路纪律，遥测失败
/// 绝不能变成调用方的失败。
pub struct ClientEventRecorder {
    journal: Arc<SessionJournal>,
    account_environment: Option<AccountEnvironment>,
    context: Mutex<StampContext>,
    rollups: Mutex<HashMap<String, RunRollup>>,
    wake: Mutex<Option<Sender<()>>>,
}

impl ClientEventRecorder {
    pub fn new(
        journal: Arc<SessionJournal>,
        account_environment: Option<AccountEnvironment>,
    ) -> Self {
        Self {
            journal,
            account_environment,
            context: Mutex::new(StampContext::default()),
            rollups: Mutex::new(HashMap::new()),
            wake: Mutex::new(None),
        }
    }

    fn set_wake(&self, sender: Sender<()>) {
        self.wake.lock().replace(sender);
    }

    pub fn set_context(&self, context: StampContext) {
        *self.context.lock() = context;
    }

    /// 直报一条事件（orchestrator preflight 等 B 类产点）。details 必须已由
    /// 调用方保证只含白名单字段；上下文盖章在这里统一附加。
    pub fn record(&self, kind: &str, severity: &str, mut details: Value) {
        self.stamp(&mut details);
        if kind == "app_started" {
            // 丢弃计数搭 app_started 出境（§4.1 的可观测性约定）。
            let dropped = self
                .journal
                .with_connection(|connection| {
                    take_dropped_events(connection, self.account_environment)
                })
                .unwrap_or(0);
            if dropped > 0 {
                details["droppedEvents"] = json!(dropped);
            }
        }
        let payload = details.to_string();
        let result = self.journal.with_connection(|connection| {
            enqueue(
                connection,
                self.account_environment,
                kind,
                severity,
                None,
                &payload,
            )
        });
        if let Err(error) = result {
            tracing::debug!(error = %error, kind, "client event enqueue failed (telemetry is best-effort)");
            return;
        }
        if let Some(wake) = self.wake.lock().as_ref() {
            let _ = wake.try_send(());
        }
    }

    /// DiagnosticService sink 入口：吃**已脱敏**的诊断事件 JSON，映射成契约
    /// kinds。不认识的 kind 直接忽略——诊断面可以自由演化，上云白名单不跟着走。
    pub fn on_diagnostic_event(&self, event: &Value) {
        let Some(kind) = event.get("kind").and_then(Value::as_str) else {
            return;
        };
        let severity = match event.get("level").and_then(Value::as_str) {
            Some("warning") => "warn",
            Some("error") => "error",
            _ => "info",
        };
        let data = event.get("data").cloned().unwrap_or_else(|| json!({}));
        let run_id = event.get("run_id").and_then(Value::as_str);

        let mapped: Option<(&str, Value)> = match kind {
            "app_started" => Some((
                "app_started",
                json!({
                    "schema_version": APP_STARTED_SCHEMA,
                    "journalSchemaVersion": data.get("journal_schema_version"),
                }),
            )),
            "recovery_completed" => Some((
                "recovery_completed",
                json!({
                    "schema_version": RECOVERY_SCHEMA,
                    "recoveredRuns": data.get("verified_ready_segments"),
                    "findings": sum_u64(&data, &["quarantined_items", "corrupt_segments", "orphan_items"]),
                }),
            )),
            "target_bound" => Some((
                "target_bound",
                json!({ "schema_version": TARGET_BOUND_SCHEMA }),
            )),
            "run_started" => {
                if let (Some(run_id), Some(run_seq)) =
                    (run_id, data.get("run_seq").and_then(Value::as_u64))
                {
                    let mut rollups = self.rollups.lock();
                    let entry = rollups.entry(run_id.to_string()).or_default();
                    entry.run_name = Some(format!("r{run_seq:04}"));
                    entry.segment_seconds = data.get("segment_seconds").and_then(Value::as_u64);
                }
                let fps = match (
                    data.get("fps_num").and_then(Value::as_u64),
                    data.get("fps_den").and_then(Value::as_u64),
                ) {
                    // 四舍五入而非截断:60000/1001 应上报 60,截断会得 59
                    // 并撞上服务端 run_started.fps 的整数语义(docs/50 P-5d)。
                    (Some(num), Some(den)) if den > 0 => Some((num + den / 2) / den),
                    _ => None,
                };
                Some((
                    "run_started",
                    json!({
                        "schema_version": RUN_STARTED_SCHEMA,
                        "runName": data.get("run_seq").and_then(Value::as_u64).map(|seq| format!("r{seq:04}")),
                        "encoderBackend": data.get("encoder_backend"),
                        "fps": fps,
                        "width": data.get("width"),
                        "height": data.get("height"),
                        "segmentSeconds": data.get("segment_seconds"),
                    }),
                ))
            }
            "capture_progress" => {
                if let Some(run_id) = run_id {
                    let mut rollups = self.rollups.lock();
                    let entry = rollups.entry(run_id.to_string()).or_default();
                    let depth = data
                        .get("capture_queue_depth")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    entry.queue_depth_max = entry.queue_depth_max.max(depth);
                    if let Some(state) = data.get("storage_state").and_then(Value::as_str) {
                        entry.storage_state = Some(state.to_string());
                    }
                }
                None
            }
            "segment_ready" => run_id.map(|run_id| {
                let mut rollups = self.rollups.lock();
                let entry = rollups.entry(run_id.to_string()).or_default();
                let segment_seq = entry.segments_emitted;
                entry.segments_emitted += 1;
                let queue_depth_max = std::mem::take(&mut entry.queue_depth_max);
                (
                    "capture_rollup",
                    json!({
                        "schema_version": CAPTURE_ROLLUP_SCHEMA,
                        "runName": entry.run_name.clone().unwrap_or_else(|| "unknown".to_string()),
                        "segmentSeq": segment_seq,
                        "durationSeconds": entry.segment_seconds,
                        "frameCount": data.get("frame_count"),
                        "queueDepthMax": queue_depth_max,
                        "storageState": entry.storage_state.clone(),
                    }),
                )
            }),
            "run_stopped" => {
                let run_name = run_id.and_then(|run_id| {
                    let mut rollups = self.rollups.lock();
                    rollups.remove(run_id).and_then(|state| state.run_name)
                });
                Some((
                    "run_completed",
                    json!({
                        "schema_version": RUN_COMPLETED_SCHEMA,
                        "runName": run_name,
                        "reason": data.get("stop_reason"),
                        "segments": data.get("ready_segments"),
                    }),
                ))
            }
            "fault" => Some((
                "fault",
                json!({
                    "schema_version": FAULT_SCHEMA,
                    // `diagnostic` 字段刻意不带：它是自由文本调试痕，不在
                    // 上云白名单里（docs/23 §3.2）。
                    "code": data.get("code").and_then(Value::as_str).unwrap_or("UNKNOWN"),
                    "message": data.get("message"),
                    "action": data.get("action"),
                    "retryable": data.get("retryable"),
                }),
            )),
            "update_check" => Some((
                "update_check",
                json!({
                    "schema_version": UPDATE_CHECK_SCHEMA,
                    "trigger": data.get("trigger").and_then(Value::as_str).unwrap_or("unknown"),
                    "currentVersion": data.get("current_version").and_then(Value::as_str).unwrap_or(CLIENT_VERSION),
                    "latestVersion": data.get("latest_version"),
                    "updateAvailable": data.get("update_available"),
                }),
            )),
            "update_check_failed" => Some((
                "update_check",
                json!({
                    "schema_version": UPDATE_CHECK_SCHEMA,
                    "trigger": data.get("trigger").and_then(Value::as_str).unwrap_or("unknown"),
                    "currentVersion": CLIENT_VERSION,
                    "error": data.get("error"),
                }),
            )),
            // 不上云的诊断面：进度 1/s（heartbeat 面职责）、标注语义、导出、
            // 控制阶段耗时、会话删除。白名单收紧在这里，一目了然。
            _ => None,
        };

        if let Some((kind, mut details)) = mapped {
            prune_nulls(&mut details);
            self.record(kind, severity, details);
        }
    }

    fn stamp(&self, details: &mut Value) {
        let context = self.context.lock();
        if let Value::Object(map) = details {
            if let Some(contract) = &context.session_contract {
                map.insert("sessionContract".into(), json!(contract));
            }
            if let Some(consent) = context.consent_present {
                map.insert("consentPresent".into(), json!(consent));
            }
        }
    }
}

/// serde_json 的 `Value::Null` 会被服务端 `.strict()` 的 optional 字段拒收
/// （null ≠ 缺席），出境前剪掉。
fn prune_nulls(details: &mut Value) {
    if let Value::Object(map) = details {
        map.retain(|_, value| !value.is_null());
    }
}

fn sum_u64(data: &Value, keys: &[&str]) -> u64 {
    keys.iter()
        .filter_map(|key| data.get(*key).and_then(Value::as_u64))
        .sum()
}

/// reqwest 实现：console-api 直连（`/app/v1` 不经 console-web Worker）。
pub struct HttpClientEventTransport {
    client: reqwest::blocking::Client,
    endpoint: reqwest::Url,
    credential: DeviceCredential,
}

#[derive(Debug)]
enum ClientEventsOverrideValue {
    NotPresent,
    NotUnicode,
    Value(String),
}

#[derive(Clone, Debug)]
pub struct ResolvedClientEventsConfiguration {
    account_environment: AccountEnvironment,
    endpoint: reqwest::Url,
    client: reqwest::blocking::Client,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientEventsConfigurationFailure {
    pub code: String,
    pub reason: String,
}

impl ClientEventsConfigurationFailure {
    fn new(code: &str, reason: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            reason: reason.into(),
        }
    }
}

/// 在启动线程上读取一次 secondary override；worker 只接收解析后的不可变结果，
/// 不再观察运行期间发生变化的进程环境。
pub fn client_events_configuration_from_env(
    account_configuration: Option<&ResolvedAccountConfiguration>,
) -> Result<Option<ResolvedClientEventsConfiguration>, ClientEventsConfigurationFailure> {
    let override_value = match std::env::var(CONSOLE_API_BASE_URL_ENV) {
        Ok(value) => ClientEventsOverrideValue::Value(value),
        Err(std::env::VarError::NotPresent) => ClientEventsOverrideValue::NotPresent,
        Err(std::env::VarError::NotUnicode(_)) => ClientEventsOverrideValue::NotUnicode,
    };
    resolve_client_events_configuration(override_value, account_configuration)
}

fn resolve_client_events_configuration(
    override_value: ClientEventsOverrideValue,
    account_configuration: Option<&ResolvedAccountConfiguration>,
) -> Result<Option<ResolvedClientEventsConfiguration>, ClientEventsConfigurationFailure> {
    if matches!(override_value, ClientEventsOverrideValue::NotUnicode) {
        return Err(ClientEventsConfigurationFailure::new(
            "client_events_base_url_invalid",
            "DATAMINER_CONSOLE_API_BASE_URL is not valid Unicode",
        ));
    }
    if matches!(&override_value, ClientEventsOverrideValue::Value(raw) if raw.trim().is_empty()) {
        return Ok(None);
    }
    let Some(account_configuration) = account_configuration else {
        return match override_value {
            ClientEventsOverrideValue::NotPresent => Ok(None),
            ClientEventsOverrideValue::Value(_) => Err(ClientEventsConfigurationFailure::new(
                "client_events_account_configuration_missing",
                "DATAMINER_CONSOLE_API_BASE_URL is set but account configuration is not ready",
            )),
            ClientEventsOverrideValue::NotUnicode => unreachable!(),
        };
    };
    let configured = match override_value {
        ClientEventsOverrideValue::NotPresent => account_configuration.base_url.as_str(),
        ClientEventsOverrideValue::Value(ref raw) => raw.as_str(),
        ClientEventsOverrideValue::NotUnicode => unreachable!(),
    };
    let endpoint = client_events_endpoint(configured, account_configuration).map_err(|error| {
        let summary = error.to_string();
        let code =
            if summary == "console API base URL conflicts with the selected account environment" {
                "client_events_environment_conflict"
            } else {
                "client_events_base_url_invalid"
            };
        // anyhow 的 Display 只给最外层 context；`{:#}` 同时保留 URL parser 的
        // 静态根因（例如 relative URL without a base），且 parser 不回显原始配置值。
        ClientEventsConfigurationFailure::new(code, format!("{error:#}"))
    })?;
    let client = reqwest::blocking::Client::builder()
        // The selected account environment is an origin boundary, not merely a first-hop hint.
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(60))
        .user_agent(concat!("DataMiner/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|error| {
            ClientEventsConfigurationFailure::new(
                "client_events_transport_start_failed",
                format!("failed to build client events HTTP client: {error}"),
            )
        })?;
    Ok(Some(ResolvedClientEventsConfiguration {
        account_environment: account_configuration.environment,
        endpoint,
        client,
    }))
}

pub fn record_client_events_startup_failure(
    diagnostics: &DiagnosticService,
    failure: &ClientEventsConfigurationFailure,
) {
    if let Err(error) = diagnostics.record(
        "error",
        None,
        DiagnosticCorrelation::default(),
        DiagnosticPayload::ServiceStartupDegraded {
            service: "client_events".into(),
            code: failure.code.clone(),
            reason: failure.reason.clone(),
        },
        true,
    ) {
        tracing::error!(error = %error, "failed to record client events startup diagnostics");
    }
}

fn client_events_endpoint(
    configured: &str,
    account_configuration: &ResolvedAccountConfiguration,
) -> anyhow::Result<reqwest::Url> {
    let mut base_url =
        reqwest::Url::parse(configured).context("invalid DATAMINER_CONSOLE_API_BASE_URL")?;
    anyhow::ensure!(
        base_url.scheme() == "https"
            || (cfg!(debug_assertions)
                && base_url.scheme() == "http"
                && matches!(base_url.host_str(), Some("127.0.0.1" | "localhost"))),
        "console API base URL must use HTTPS"
    );
    anyhow::ensure!(
        base_url.query().is_none() && base_url.fragment().is_none(),
        "console API base URL cannot contain query or fragment"
    );
    anyhow::ensure!(
        base_url.username().is_empty() && base_url.password().is_none(),
        "console API base URL cannot contain user info"
    );
    anyhow::ensure!(
        !base_url.path().contains("/app/v1"),
        "console API base URL must not include the /app/v1 prefix; the client appends it"
    );
    if !base_url.path().ends_with('/') {
        let normalized = format!("{}/", base_url.path());
        base_url.set_path(&normalized);
    }
    let production = reqwest::Url::parse(CANONICAL_PRODUCTION_ACCOUNT_API_BASE_URL)?;
    let points_at_production = base_url == production;
    let targets_production_host = targets_production_account_host(&base_url);
    anyhow::ensure!(
        match account_configuration.environment {
            AccountEnvironment::Production => points_at_production,
            AccountEnvironment::Staging => !targets_production_host,
        },
        "console API base URL conflicts with the selected account environment"
    );
    Ok(base_url.join("app/v1/client-events")?)
}

impl HttpClientEventTransport {
    fn from_configuration(
        credential: DeviceCredential,
        configuration: &ResolvedClientEventsConfiguration,
    ) -> Self {
        Self {
            client: configuration.client.clone(),
            endpoint: configuration.endpoint.clone(),
            credential,
        }
    }
}

impl ClientEventTransport for HttpClientEventTransport {
    fn send(&self, batch: &OutboxBatch) -> anyhow::Result<SendDisposition> {
        let idempotency_key = batch
            .events
            .first()
            .map(|event| event.event_id.clone())
            .unwrap_or_default();
        let response = self
            .client
            .post(self.endpoint.clone())
            .bearer_auth(self.credential.expose())
            .header("Idempotency-Key", idempotency_key)
            .json(&batch.to_body())
            .send()
            .map_err(|_| anyhow::anyhow!("CLIENT_EVENTS_API_UNAVAILABLE"))?;
        let status = response.status();
        if status.is_success() {
            return Ok(SendDisposition::Accepted);
        }
        // 401/403 走 Err：凭据可能轮换后恢复，不是毒批。响应体不读——错误里
        // 不携带任何可能含敏感内容的服务端文本。
        if status.as_u16() == 401 || status.as_u16() == 403 {
            anyhow::bail!("CLIENT_EVENTS_UNAUTHORIZED");
        }
        if status.is_client_error() {
            return Ok(SendDisposition::RejectedByContract);
        }
        anyhow::bail!("CLIENT_EVENTS_SERVER_ERROR");
    }
}

/// 后台发送器（仿 UploaderService：专用线程 + wake channel + 优雅退出）。
pub struct ClientEventsService {
    recorder: Arc<ClientEventRecorder>,
    wake_tx: Sender<()>,
    stopping: Arc<AtomicBool>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl ClientEventsService {
    pub fn start(
        journal: Arc<SessionJournal>,
        credential_store: Arc<dyn CredentialStore>,
        account_environment: Option<AccountEnvironment>,
        configuration: Option<ResolvedClientEventsConfiguration>,
    ) -> anyhow::Result<Arc<Self>> {
        anyhow::ensure!(
            configuration
                .as_ref()
                .is_none_or(|resolved| Some(resolved.account_environment) == account_environment),
            "client events send environment differs from its durable outbox environment"
        );
        let recorder = Arc::new(ClientEventRecorder::new(
            Arc::clone(&journal),
            account_environment,
        ));
        let (wake_tx, wake_rx) = bounded::<()>(1);
        recorder.set_wake(wake_tx.clone());
        let stopping = Arc::new(AtomicBool::new(false));
        let worker = {
            let journal = Arc::clone(&journal);
            let stopping = Arc::clone(&stopping);
            std::thread::Builder::new()
                .name("dm-client-events".into())
                .spawn(move || {
                    worker_loop(journal, credential_store, configuration, wake_rx, stopping);
                })
                .context("failed to spawn client events sender")?
        };
        Ok(Arc::new(Self {
            recorder,
            wake_tx,
            stopping,
            worker: Mutex::new(Some(worker)),
        }))
    }

    pub fn recorder(&self) -> Arc<ClientEventRecorder> {
        Arc::clone(&self.recorder)
    }

    pub fn wake(&self) {
        let _ = self.wake_tx.try_send(());
    }

    pub fn begin_shutdown(&self) {
        self.stopping.store(true, Ordering::Release);
        let _ = self.wake_tx.try_send(());
    }

    #[must_use]
    pub fn shutdown_complete(&self) -> bool {
        self.worker
            .lock()
            .as_ref()
            .is_none_or(JoinHandle::is_finished)
    }
}

fn worker_loop(
    journal: Arc<SessionJournal>,
    credential_store: Arc<dyn CredentialStore>,
    configuration: Option<ResolvedClientEventsConfiguration>,
    wake_rx: Receiver<()>,
    stopping: Arc<AtomicBool>,
) {
    while !stopping.load(Ordering::Acquire) {
        let _ = wake_rx.recv_timeout(WORKER_POLL_INTERVAL);
        if stopping.load(Ordering::Acquire) {
            break;
        }
        // 账号环境未就绪、secondary override 非法或显式关闭上报时，事件仍按
        // 启动时账号环境落 durable outbox，但本次进程绝不发送。
        let Some(configuration) = configuration.as_ref() else {
            continue;
        };
        let now_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs() as i64)
            .unwrap_or(0);
        let ready = journal
            .with_connection(|connection| {
                seconds_until_ready(connection, configuration.account_environment, now_unix)
            })
            .unwrap_or(i64::MAX);
        if ready != 0 {
            continue;
        }
        // 未 enroll（无该环境凭据）时静默待机。
        let Ok(Some(credential)) = credentials::load(
            credential_store.as_ref(),
            CredentialKind::Device,
            configuration.account_environment.credential_scope(),
        ) else {
            continue;
        };
        let transport = HttpClientEventTransport::from_configuration(credential, configuration);
        // 积压时连续排空，但**每个完整批次之后**重新观察 shutdown。已经被服务端
        // 接受的批次先原子推进游标；尚未开始的批次继续留在 durable outbox。
        // 环境隔离沿用 main 的 `account_environment` 口径，不因这次改动回退。
        if let Err(error) = drain_backlog_until_stopping(stopping.as_ref(), || {
            drain_once(&journal, configuration.account_environment, &transport)
        }) {
            tracing::debug!(error = %error, "client events drain failed (telemetry is best-effort)");
        }
    }
}

fn drain_backlog_until_stopping(
    stopping: &AtomicBool,
    mut drain: impl FnMut() -> anyhow::Result<DrainOutcome>,
) -> anyhow::Result<()> {
    while !stopping.load(Ordering::Acquire) {
        let outcome = drain()?;
        if outcome.sent == 0 || outcome.deferred {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    use capture_core::mapping::MappingSnapshot;
    use capture_core::profile::CaptureProfile;

    use crate::services::journal::{HeartbeatWrite, TestRunStart};

    fn production_account_configuration() -> ResolvedAccountConfiguration {
        ResolvedAccountConfiguration {
            environment: AccountEnvironment::Production,
            base_url: CANONICAL_PRODUCTION_ACCOUNT_API_BASE_URL.into(),
        }
    }

    const TEST_ENVIRONMENT: AccountEnvironment = AccountEnvironment::Staging;

    fn enqueue(
        connection: &Connection,
        kind: &str,
        severity: &str,
        session_id: Option<&str>,
        payload_json: &str,
    ) -> anyhow::Result<()> {
        super::enqueue(
            connection,
            Some(TEST_ENVIRONMENT),
            kind,
            severity,
            session_id,
            payload_json,
        )
    }

    /// 单连接下的三段合成，供 outbox 语义测试复用。
    ///
    /// 生产的 [`super::drain_once`] 走的是同样三段，区别只在中间那段之外会放开
    /// journal 锁；语义断言不需要真锁，所以这里直接串起来。锁本身由
    /// `a_blocked_transport_never_holds_the_journal_lock` 用真 journal 守。
    fn drain_once_for(
        connection: &Connection,
        account_environment: AccountEnvironment,
        transport: &dyn ClientEventTransport,
    ) -> anyhow::Result<DrainOutcome> {
        let Some(frozen) = freeze_batch(connection, account_environment)? else {
            return Ok(DrainOutcome {
                sent: 0,
                dropped: 0,
                deferred: false,
            });
        };
        let sent = transport.send(&frozen.batch);
        commit_send(connection, account_environment, &frozen, sent)
    }

    fn drain_once(
        connection: &Connection,
        transport: &dyn ClientEventTransport,
    ) -> anyhow::Result<DrainOutcome> {
        drain_once_for(connection, TEST_ENVIRONMENT, transport)
    }

    fn cursor_snapshot(connection: &Connection) -> anyhow::Result<CursorSnapshot> {
        super::cursor_snapshot(connection, TEST_ENVIRONMENT)
    }

    fn pending_after(connection: &Connection, acked_seq: i64) -> anyhow::Result<Vec<OutboxEvent>> {
        super::pending_after(connection, TEST_ENVIRONMENT, acked_seq)
    }

    fn take_dropped_events(connection: &Connection) -> anyhow::Result<i64> {
        super::take_dropped_events(connection, Some(TEST_ENVIRONMENT))
    }

    fn seconds_until_ready(connection: &Connection, now_unix: i64) -> anyhow::Result<i64> {
        super::seconds_until_ready(connection, TEST_ENVIRONMENT, now_unix)
    }

    #[test]
    fn non_unicode_override_is_invalid_instead_of_falling_back() {
        let failure = resolve_client_events_configuration(
            ClientEventsOverrideValue::NotUnicode,
            Some(&production_account_configuration()),
        )
        .expect_err("an explicitly unreadable override must fail closed");

        assert_eq!(failure.code, "client_events_base_url_invalid");
        assert_eq!(
            failure.reason,
            "DATAMINER_CONSOLE_API_BASE_URL is not valid Unicode"
        );
    }

    #[test]
    fn invalid_override_returns_a_stable_code_and_a_safe_real_reason() {
        let failure = resolve_client_events_configuration(
            ClientEventsOverrideValue::Value(
                "https://user:secret@console.diogenesmodel.com/".into(),
            ),
            Some(&production_account_configuration()),
        )
        .expect_err("credentials in an override must fail closed");

        assert_eq!(failure.code, "client_events_base_url_invalid");
        assert_eq!(
            failure.reason,
            "console API base URL cannot contain user info"
        );
        assert!(!failure.reason.contains("secret"));
    }

    #[test]
    fn client_events_user_info_failure_keeps_its_safe_reason_in_diagnostics() {
        let temp = tempfile::tempdir().unwrap();
        let diagnostics =
            crate::services::diagnostics::DiagnosticService::open(temp.path()).unwrap();
        let failure = resolve_client_events_configuration(
            ClientEventsOverrideValue::Value(
                "https://alice:secret@console.diogenesmodel.com/".into(),
            ),
            Some(&production_account_configuration()),
        )
        .expect_err("console URL user info must fail closed");

        record_client_events_startup_failure(&diagnostics, &failure);

        let text = std::fs::read_to_string(
            temp.path()
                .join("diagnostics")
                .join("logs")
                .join("events.jsonl"),
        )
        .unwrap();
        let event: Value = serde_json::from_str(text.trim()).unwrap();
        assert_eq!(
            event["data"]["reason"],
            "console API base URL cannot contain user info"
        );
        assert!(!text.contains("alice"));
        assert!(!text.contains("secret"));
        assert!(!text.contains("redacted-sensitive-diagnostic"));
    }

    #[test]
    fn malformed_override_keeps_the_parser_root_cause_without_echoing_the_value() {
        let raw = "definitely-not-a-url-secret-marker";
        let failure = resolve_client_events_configuration(
            ClientEventsOverrideValue::Value(raw.into()),
            Some(&production_account_configuration()),
        )
        .expect_err("a malformed override must fail closed at startup");

        assert_eq!(failure.code, "client_events_base_url_invalid");
        assert!(failure.reason.contains("relative URL without a base"));
        assert!(!failure.reason.contains(raw));
    }

    #[test]
    fn startup_failure_is_written_to_diagnostics_with_stable_fields() {
        let temp = tempfile::tempdir().unwrap();
        let diagnostics =
            crate::services::diagnostics::DiagnosticService::open(temp.path()).unwrap();
        let failure = ClientEventsConfigurationFailure {
            code: "client_events_base_url_invalid".into(),
            reason: "DATAMINER_CONSOLE_API_BASE_URL is not valid Unicode".into(),
        };

        record_client_events_startup_failure(&diagnostics, &failure);

        let text = std::fs::read_to_string(
            temp.path()
                .join("diagnostics")
                .join("logs")
                .join("events.jsonl"),
        )
        .unwrap();
        let event: Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
        assert_eq!(event["kind"], "service_startup_degraded");
        assert_eq!(event["data"]["service"], "client_events");
        assert_eq!(event["data"]["code"], failure.code);
        assert_eq!(event["data"]["reason"], failure.reason);
    }

    #[test]
    fn begin_shutdown_does_not_join_a_running_worker() {
        let (recorder, _journal, _dir) = recorder_with_temp_journal();
        let (wake_tx, _wake_rx) = bounded::<()>(1);
        let stopping = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let worker_release = Arc::clone(&release);
        let worker = std::thread::spawn(move || {
            while !worker_release.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(5));
            }
        });
        let service = Arc::new(ClientEventsService {
            recorder,
            wake_tx,
            stopping,
            worker: Mutex::new(Some(worker)),
        });
        let (returned_tx, returned_rx) = bounded::<()>(1);
        let shutdown_service = Arc::clone(&service);
        std::thread::spawn(move || {
            shutdown_service.begin_shutdown();
            let _ = returned_tx.send(());
        });

        let returned_without_join = returned_rx.recv_timeout(Duration::from_millis(100)).is_ok();
        release.store(true, Ordering::Release);
        let _ = returned_rx.recv_timeout(Duration::from_secs(1));

        assert!(
            returned_without_join,
            "begin_shutdown blocked on the client-events worker instead of only signalling it"
        );
    }

    #[test]
    fn override_wins_over_the_account_base_url() {
        let staging = ResolvedAccountConfiguration {
            environment: AccountEnvironment::Staging,
            base_url: "https://staging-console.example.com/".into(),
        };
        let resolved = resolve_client_events_configuration(
            ClientEventsOverrideValue::Value("http://127.0.0.1:9/".into()),
            Some(&staging),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            resolved.endpoint.as_str(),
            "http://127.0.0.1:9/app/v1/client-events"
        );
    }

    #[test]
    fn override_with_api_prefix_fails_closed_before_transport_start() {
        let staging = ResolvedAccountConfiguration {
            environment: AccountEnvironment::Staging,
            base_url: "https://staging-console.example.com/".into(),
        };
        let failure = resolve_client_events_configuration(
            ClientEventsOverrideValue::Value("https://staging-console.example.com/app/v1/".into()),
            Some(&staging),
        )
        .expect_err("a pre-appended API prefix must fail at startup");

        assert_eq!(failure.code, "client_events_base_url_invalid");
        assert_eq!(
            failure.reason,
            "console API base URL must not include the /app/v1 prefix; the client appends it"
        );
    }

    #[test]
    fn client_events_http_client_does_not_follow_redirects() {
        let probe = crate::services::test_http::redirect_probe();
        let staging = ResolvedAccountConfiguration {
            environment: AccountEnvironment::Staging,
            base_url: "https://staging-console.example.com/".into(),
        };
        let configuration = resolve_client_events_configuration(
            ClientEventsOverrideValue::Value(probe.source_url.clone()),
            Some(&staging),
        )
        .unwrap()
        .unwrap();
        let transport = HttpClientEventTransport::from_configuration(
            DeviceCredential::new("redirect-test-token").unwrap(),
            &configuration,
        );
        let batch = OutboxBatch {
            events: vec![OutboxEvent {
                seq: 1,
                event_id: "00000000-0000-4000-8000-000000000001".into(),
                occurred_at_utc: "2026-08-13T00:00:00Z".into(),
                kind: "app_started".into(),
                severity: "info".into(),
                session_id: None,
                payload_json: "{}".into(),
            }],
        };

        assert!(
            transport.send(&batch).is_err(),
            "a redirect must not be accepted as delivery"
        );
        assert!(
            !probe.finish(),
            "client events must not reach a redirect target"
        );
    }

    #[test]
    fn explicit_empty_override_disables_reporting() {
        // 「关掉」必须仍然可表达——`run-upload-intercept-lab.ps1` 之外，
        // 离线演示与测试也靠这条。空白串按空处理，不然一个尾随空格就变成了
        // 「解析失败」而不是「关掉」。
        for raw in ["", "   "] {
            let resolved = resolve_client_events_configuration(
                ClientEventsOverrideValue::Value(raw.to_owned()),
                Some(&production_account_configuration()),
            )
            .unwrap();
            assert!(resolved.is_none(), "raw = {raw:?}");
        }
    }

    #[test]
    fn unset_override_falls_back_instead_of_standing_by_forever() {
        // 这条是本次修复本身：此前未设变量 = 永远不上报，于是生产库 client_events
        // 至今 0 行（docs/101 §3.4）。回落到账号面基址之后，发布包才会真的上报。
        let resolved = resolve_client_events_configuration(
            ClientEventsOverrideValue::NotPresent,
            Some(&production_account_configuration()),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            resolved.endpoint.as_str(),
            "https://console.diogenesmodel.com/app/v1/client-events"
        );
    }

    #[test]
    fn account_surface_turned_off_turns_reporting_off_too() {
        // 账号面被显式关掉时上报器一并关掉，不出现「账号面关了、上报还在打」。
        assert!(
            resolve_client_events_configuration(ClientEventsOverrideValue::NotPresent, None)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn client_events_endpoint_rejects_cross_environment_overrides() {
        let staging = ResolvedAccountConfiguration {
            environment: AccountEnvironment::Staging,
            base_url: "https://staging-console.example.com/".into(),
        };
        let production = ResolvedAccountConfiguration {
            environment: AccountEnvironment::Production,
            base_url: CANONICAL_PRODUCTION_ACCOUNT_API_BASE_URL.into(),
        };

        assert!(
            client_events_endpoint(CANONICAL_PRODUCTION_ACCOUNT_API_BASE_URL, &staging).is_err()
        );
        assert!(
            client_events_endpoint("https://staging-console.example.com/", &production).is_err()
        );
        assert!(
            client_events_endpoint("https://console.diogenesmodel.com./", &staging).is_err(),
            "a trailing DNS dot must not bypass the staging-to-production guard"
        );
        assert!(
            client_events_endpoint("https://user:password@staging.example.com/", &staging).is_err()
        );
        assert!(
            client_events_endpoint("https://staging.example.com/?token=secret", &staging).is_err()
        );
        assert_eq!(
            client_events_endpoint("https://staging-console.example.com", &staging)
                .unwrap()
                .as_str(),
            "https://staging-console.example.com/app/v1/client-events"
        );
        assert_eq!(
            client_events_endpoint(CANONICAL_PRODUCTION_ACCOUNT_API_BASE_URL, &production)
                .unwrap()
                .as_str(),
            "https://console.diogenesmodel.com/app/v1/client-events"
        );
    }

    /// 只建 outbox 两表的库（同 mirror.rs 的单测策略），避免拖整套 journal。
    fn outbox_db() -> Connection {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE client_event_outbox (
                    seq INTEGER PRIMARY KEY AUTOINCREMENT,
                    event_id TEXT NOT NULL UNIQUE,
                    occurred_at_utc TEXT NOT NULL,
                    kind TEXT NOT NULL,
                    severity TEXT NOT NULL,
                    session_id TEXT,
                    payload_json TEXT NOT NULL,
                    created_at_utc TEXT NOT NULL,
                    account_environment TEXT NOT NULL
                        CHECK(account_environment IN ('unknown', 'staging', 'production'))
                 );
                 CREATE TABLE client_event_cursor (
                    account_environment TEXT PRIMARY KEY
                        CHECK(account_environment IN ('unknown', 'staging', 'production')),
                    acked_seq INTEGER NOT NULL DEFAULT 0,
                    attempt_count INTEGER NOT NULL DEFAULT 0,
                    dropped_events INTEGER NOT NULL DEFAULT 0,
                    last_error TEXT,
                    last_attempt_at_utc TEXT,
                    updated_at_utc TEXT NOT NULL
                 );
                 INSERT INTO client_event_cursor(account_environment, updated_at_utc)
                 VALUES
                    ('unknown', strftime('%Y-%m-%dT%H:%M:%fZ','now')),
                    ('staging', strftime('%Y-%m-%dT%H:%M:%fZ','now')),
                    ('production', strftime('%Y-%m-%dT%H:%M:%fZ','now'));",
            )
            .unwrap();
        connection
    }

    struct OkTransport {
        calls: AtomicUsize,
    }
    impl ClientEventTransport for OkTransport {
        fn send(&self, _batch: &OutboxBatch) -> anyhow::Result<SendDisposition> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(SendDisposition::Accepted)
        }
    }

    struct RejectTransport;
    impl ClientEventTransport for RejectTransport {
        fn send(&self, _batch: &OutboxBatch) -> anyhow::Result<SendDisposition> {
            Ok(SendDisposition::RejectedByContract)
        }
    }

    struct FailTransport;
    impl ClientEventTransport for FailTransport {
        fn send(&self, _batch: &OutboxBatch) -> anyhow::Result<SendDisposition> {
            anyhow::bail!("network unreachable: secret-looking-url")
        }
    }

    struct CollectTransport {
        kinds: Mutex<Vec<String>>,
    }

    impl ClientEventTransport for CollectTransport {
        fn send(&self, batch: &OutboxBatch) -> anyhow::Result<SendDisposition> {
            self.kinds
                .lock()
                .extend(batch.events.iter().map(|event| event.kind.clone()));
            Ok(SendDisposition::Accepted)
        }
    }

    struct StopAfterFirstBatchTransport {
        stopping: Arc<AtomicBool>,
        calls: AtomicUsize,
    }
    impl ClientEventTransport for StopAfterFirstBatchTransport {
        fn send(&self, _batch: &OutboxBatch) -> anyhow::Result<SendDisposition> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.stopping.store(true, Ordering::Release);
            Ok(SendDisposition::Accepted)
        }
    }

    #[test]
    fn environment_switch_keeps_outbox_rows_and_cursors_isolated() {
        let connection = outbox_db();
        super::enqueue(
            &connection,
            Some(AccountEnvironment::Production),
            "production_event",
            "info",
            None,
            "{}",
        )
        .unwrap();
        super::enqueue(
            &connection,
            Some(AccountEnvironment::Staging),
            "staging_event",
            "info",
            None,
            "{}",
        )
        .unwrap();
        super::enqueue(&connection, None, "legacy_unknown", "info", None, "{}").unwrap();
        let transport = CollectTransport {
            kinds: Mutex::new(Vec::new()),
        };

        let staging = drain_once_for(&connection, AccountEnvironment::Staging, &transport).unwrap();

        assert_eq!(staging.sent, 1);
        assert_eq!(&*transport.kinds.lock(), &["staging_event"]);
        assert_eq!(
            super::cursor_snapshot(&connection, AccountEnvironment::Staging)
                .unwrap()
                .pending,
            0
        );
        assert_eq!(
            super::cursor_snapshot(&connection, AccountEnvironment::Production)
                .unwrap()
                .pending,
            1
        );
        let unknown_rows: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM client_event_outbox
                  WHERE account_environment = 'unknown'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(unknown_rows, 1, "unknown rows must remain quarantined");

        transport.kinds.lock().clear();
        let production =
            drain_once_for(&connection, AccountEnvironment::Production, &transport).unwrap();
        assert_eq!(production.sent, 1);
        assert_eq!(&*transport.kinds.lock(), &["production_event"]);
        assert_eq!(
            connection
                .query_row(
                    "SELECT COUNT(*) FROM client_event_outbox
                      WHERE account_environment = 'unknown'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );
    }

    #[test]
    fn backlog_drain_observes_stopping_between_batches() {
        let connection = outbox_db();
        for index in 0..=MAX_BATCH {
            enqueue(
                &connection,
                "fault",
                "error",
                None,
                &format!("{{\"i\":{index}}}"),
            )
            .unwrap();
        }
        let stopping = Arc::new(AtomicBool::new(false));
        let transport = StopAfterFirstBatchTransport {
            stopping: Arc::clone(&stopping),
            calls: AtomicUsize::new(0),
        };

        drain_backlog_until_stopping(stopping.as_ref(), || drain_once(&connection, &transport))
            .unwrap();

        assert_eq!(transport.calls.load(Ordering::SeqCst), 1);
        let snapshot = cursor_snapshot(&connection).unwrap();
        assert_eq!(snapshot.acked_seq, MAX_BATCH as i64);
        assert_eq!(snapshot.pending, 1);
    }

    #[test]
    fn backlog_drain_does_not_start_when_already_stopping() {
        let stopping = AtomicBool::new(true);
        let calls = AtomicUsize::new(0);

        drain_backlog_until_stopping(&stopping, || {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(DrainOutcome {
                sent: 1,
                dropped: 0,
                deferred: false,
            })
        })
        .unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn backlog_drain_continues_until_a_later_batch_is_empty() {
        let stopping = AtomicBool::new(false);
        let calls = AtomicUsize::new(0);

        drain_backlog_until_stopping(&stopping, || {
            let call = calls.fetch_add(1, Ordering::SeqCst);
            Ok(DrainOutcome {
                sent: usize::from(call == 0),
                dropped: 0,
                deferred: false,
            })
        })
        .unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn enqueue_assigns_uuid_event_ids_and_drain_advances() {
        let connection = outbox_db();
        enqueue(
            &connection,
            "app_started",
            "info",
            None,
            "{\"schema_version\":\"dm-client-app-started-1\"}",
        )
        .unwrap();
        enqueue(
            &connection,
            "fault",
            "error",
            None,
            "{\"schema_version\":\"dm-client-fault-1\",\"code\":\"X\"}",
        )
        .unwrap();

        let transport = OkTransport {
            calls: AtomicUsize::new(0),
        };
        let outcome = drain_once(&connection, &transport).unwrap();

        assert_eq!(outcome.sent, 2);
        assert!(!outcome.deferred);
        let snapshot = cursor_snapshot(&connection).unwrap();
        assert_eq!(snapshot.pending, 0);
        assert_eq!(snapshot.acked_seq, 2);
    }

    #[test]
    fn batch_body_matches_the_server_contract_shape() {
        let connection = outbox_db();
        enqueue(&connection, "preflight_failed", "error", None,
            "{\"schema_version\":\"dm-client-preflight-1\",\"failureCode\":\"HARDWARE_PATH_PREFLIGHT_FAILED\",\"windowsBuild\":\"22631\"}").unwrap();
        let events = pending_after(&connection, 0).unwrap();
        let batch = OutboxBatch { events };
        let body = batch.to_body();

        let event = &body["events"][0];
        assert_eq!(event["kind"], "preflight_failed");
        assert_eq!(event["severity"], "error");
        assert_eq!(
            event["details"]["failureCode"],
            "HARDWARE_PATH_PREFLIGHT_FAILED"
        );
        assert_eq!(event["details"]["windowsBuild"], "22631");
        // uuid v4 形状（服务端 z.string().uuid()）。
        let event_id = event["eventId"].as_str().unwrap();
        assert_eq!(event_id.len(), 36);
        assert!(event.get("sessionId").is_none());
        assert!(!event["clientVersion"].as_str().unwrap().is_empty());
    }

    #[test]
    fn transport_failure_defers_without_dropping() {
        let connection = outbox_db();
        enqueue(&connection, "fault", "error", None, "{}").unwrap();

        let outcome = drain_once(&connection, &FailTransport).unwrap();

        assert!(outcome.deferred);
        let snapshot = cursor_snapshot(&connection).unwrap();
        assert_eq!(snapshot.pending, 1);
        assert_eq!(snapshot.attempt, 1);
        assert_eq!(
            snapshot.last_error.as_deref(),
            Some("CLIENT_EVENTS_TRANSPORT_FAILED")
        );
    }

    #[test]
    fn contract_rejection_drops_the_poison_batch_and_counts_it() {
        let connection = outbox_db();
        enqueue(&connection, "fault", "error", None, "{}").unwrap();
        enqueue(&connection, "fault", "error", None, "{}").unwrap();

        let outcome = drain_once(&connection, &RejectTransport).unwrap();

        assert_eq!(outcome.dropped, 2);
        let snapshot = cursor_snapshot(&connection).unwrap();
        // 队头不再被毒批堵住，丢弃可观测。
        assert_eq!(snapshot.pending, 0);
        assert_eq!(snapshot.dropped_events, 2);
        assert_eq!(
            snapshot.last_error.as_deref(),
            Some("CLIENT_EVENTS_REJECTED")
        );
        // 后续事件继续正常投递。
        enqueue(&connection, "app_started", "info", None, "{}").unwrap();
        let transport = OkTransport {
            calls: AtomicUsize::new(0),
        };
        assert_eq!(drain_once(&connection, &transport).unwrap().sent, 1);
    }

    #[test]
    fn the_ring_buffer_drops_oldest_beyond_the_cap() {
        let connection = outbox_db();
        for index in 0..(MAX_OUTBOX_ROWS + 5) {
            enqueue(
                &connection,
                "fault",
                "error",
                None,
                &format!("{{\"i\":{index}}}"),
            )
            .unwrap();
        }
        let snapshot = cursor_snapshot(&connection).unwrap();
        assert_eq!(snapshot.pending, MAX_OUTBOX_ROWS);
        assert_eq!(snapshot.dropped_events, 5);
        // 最旧的被丢：队头现在从 seq 6 开始。
        let events = pending_after(&connection, 0).unwrap();
        assert_eq!(events.first().unwrap().seq, 6);
        assert_eq!(take_dropped_events(&connection).unwrap(), 5);
        assert_eq!(take_dropped_events(&connection).unwrap(), 0);
    }

    #[test]
    fn backoff_grows_and_saturates() {
        assert_eq!(backoff_for(0), 15);
        assert_eq!(backoff_for(1), 15);
        assert_eq!(backoff_for(2), 60);
        assert_eq!(backoff_for(5), 3_600);
        assert_eq!(backoff_for(99), 3_600);
    }

    #[test]
    fn ready_signal_reports_no_pending_as_never() {
        let connection = outbox_db();
        assert_eq!(seconds_until_ready(&connection, 1_000).unwrap(), i64::MAX);
        enqueue(&connection, "fault", "error", None, "{}").unwrap();
        assert_eq!(seconds_until_ready(&connection, 1_000).unwrap(), 0);
    }

    fn recorder_with_temp_journal() -> (
        Arc<ClientEventRecorder>,
        Arc<SessionJournal>,
        tempfile::TempDir,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(SessionJournal::open(dir.path().join("journal.sqlite3")).unwrap());
        (
            Arc::new(ClientEventRecorder::new(
                Arc::clone(&journal),
                Some(TEST_ENVIRONMENT),
            )),
            journal,
            dir,
        )
    }

    #[test]
    fn diagnostic_bridge_maps_run_lifecycle_and_rolls_up_segments() {
        let (recorder, journal, _dir) = recorder_with_temp_journal();
        recorder.set_context(StampContext {
            session_contract: Some("dm-local-dev".into()),
            consent_present: Some(false),
        });

        let run = |payload: Value| recorder.on_diagnostic_event(&payload);
        run(json!({
            "kind": "run_started", "level": "info", "run_id": "run-1",
            "data": {"run_seq": 3, "encoder_backend": "nvenc", "fps_num": 60, "fps_den": 1,
                      "width": 1920, "height": 1080, "max_bitrate_bps": 1, "segment_seconds": 10}
        }));
        run(json!({
            "kind": "capture_progress", "level": "info", "run_id": "run-1",
            "data": {"capture_queue_depth": 7, "storage_state": "ok"}
        }));
        run(json!({
            "kind": "segment_ready", "level": "info", "run_id": "run-1",
            "data": {"frame_count": 600, "event_count": 10, "qpc_start": 0, "qpc_end": 1, "video_bytes": 5}
        }));
        run(json!({
            "kind": "run_stopped", "level": "info", "run_id": "run-1",
            "data": {"stop_reason": "user", "frame_count": 600, "ready_segments": 1,
                      "input_event_count": 2, "input_queue_overflows": 0}
        }));
        // 1/s 进度与未知诊断种类不产生事件。
        run(json!({"kind": "marker_added", "level": "info", "data": {}}));

        let events = journal
            .with_connection(|connection| pending_after(connection, 0))
            .unwrap();
        let kinds: Vec<&str> = events.iter().map(|event| event.kind.as_str()).collect();
        assert_eq!(
            kinds,
            vec!["run_started", "capture_rollup", "run_completed"]
        );

        let started: Value = serde_json::from_str(&events[0].payload_json).unwrap();
        assert_eq!(started["fps"], 60);

        let rollup: Value = serde_json::from_str(&events[1].payload_json).unwrap();
        assert_eq!(rollup["schema_version"], CAPTURE_ROLLUP_SCHEMA);
        assert_eq!(rollup["runName"], "r0003");
        assert_eq!(rollup["segmentSeq"], 0);
        assert_eq!(rollup["frameCount"], 600);
        assert_eq!(rollup["queueDepthMax"], 7);
        assert_eq!(rollup["storageState"], "ok");
        assert_eq!(rollup["sessionContract"], "dm-local-dev");
        assert_eq!(rollup["consentPresent"], false);

        let completed: Value = serde_json::from_str(&events[2].payload_json).unwrap();
        assert_eq!(completed["runName"], "r0003");
        assert_eq!(completed["reason"], "user");
        // null 字段出境前剪掉——服务端 .strict() 的 optional 拒收 null。
        assert!(completed.get("durationSeconds").is_none());
    }

    #[test]
    fn diagnostic_bridge_rounds_rational_fps_instead_of_truncating() {
        let (recorder, journal, _dir) = recorder_with_temp_journal();
        recorder.on_diagnostic_event(&json!({
            "kind": "run_started", "level": "info", "run_id": "run-ntsc",
            "data": {"run_seq": 1, "encoder_backend": "nvenc",
                      "fps_num": 60_000, "fps_den": 1_001,
                      "width": 1920, "height": 1080, "segment_seconds": 10}
        }));
        let events = journal
            .with_connection(|connection| pending_after(connection, 0))
            .unwrap();
        let started: Value = serde_json::from_str(&events[0].payload_json).unwrap();
        // 截断整除会得 59;名义帧率 59.94 的正确整数上报是 60(docs/50 P-5d)。
        assert_eq!(started["fps"], 60);
    }

    #[test]
    fn diagnostic_bridge_drops_free_text_diagnostic_from_faults() {
        let (recorder, journal, _dir) = recorder_with_temp_journal();
        recorder.on_diagnostic_event(&json!({
            "kind": "fault", "level": "error",
            "data": {"code": "X", "message": "m", "action": "a", "retryable": true,
                      "diagnostic": "C:/secret/path"}
        }));
        let events = journal
            .with_connection(|connection| pending_after(connection, 0))
            .unwrap();
        assert_eq!(events.len(), 1);
        let details: Value = serde_json::from_str(&events[0].payload_json).unwrap();
        assert_eq!(details["schema_version"], FAULT_SCHEMA);
        assert_eq!(details["code"], "X");
        assert!(details.get("diagnostic").is_none());
    }

    #[test]
    fn update_check_failure_maps_with_the_local_client_version() {
        let (recorder, journal, _dir) = recorder_with_temp_journal();
        recorder.on_diagnostic_event(&json!({
            "kind": "update_check_failed", "level": "warning",
            "data": {"trigger": "startup", "error": "offline"}
        }));
        let events = journal
            .with_connection(|connection| pending_after(connection, 0))
            .unwrap();
        assert_eq!(events[0].kind, "update_check");
        assert_eq!(events[0].severity, "warn");
        let details: Value = serde_json::from_str(&events[0].payload_json).unwrap();
        assert_eq!(details["currentVersion"], CLIENT_VERSION);
        assert_eq!(details["error"], "offline");
    }

    /// 阻塞在发送里的 transport：`entered` 通知发送已经真正开始，`release` 之前不返回。
    struct BlockingTransport {
        entered: Sender<()>,
        release: Receiver<()>,
    }

    impl ClientEventTransport for BlockingTransport {
        fn send(&self, _batch: &OutboxBatch) -> anyhow::Result<SendDisposition> {
            self.entered
                .send(())
                .expect("test harness dropped the receiver");
            self.release
                .recv()
                .expect("test harness dropped the sender");
            Ok(SendDisposition::Accepted)
        }
    }

    /// 一个真 journal，且 run 已经处于 recording——`record_heartbeat` 只在这个状态下成立。
    fn recording_journal(dir: &tempfile::TempDir) -> Arc<SessionJournal> {
        const MAPPING: &[u8] = br#"{
          "schema_version":"dm-mapping-2",
          "mapping_id":"game.test.pc",
          "version":"1",
          "game":{"id":"test","name":"Test","platform":"pc"},
          "taxonomy":{"content":"act","view":"first_person","material":"3d"},
          "recording_profile_id":"test-profile",
          "bindings":[{"device":"keyboard","scan_code":17,"extended":false,"semantic":"move.forward"}]
        }"#;
        let profile = CaptureProfile {
            profile_id: "test-profile".into(),
            non_trainable: true,
            capture_backend: "native_wgc_window_v1".into(),
            encoder_backend: "mf_nvidia_hardware_v1".into(),
            width: 1920,
            height: 1080,
            fps_num: 30,
            fps_den: 1,
            bitrate_bps: 8_000_000,
            max_bitrate_bps: 12_000_000,
            gop_frames: 120,
            segment_seconds: 10,
            test_segments: 1,
            accepted_encoder_relaxations: None,
            audio: None,
        };
        let mapping = MappingSnapshot::from_slice(MAPPING).unwrap();
        let journal = SessionJournal::open(dir.path().join("journal.sqlite3")).unwrap();
        journal
            .begin_test_run(&TestRunStart {
                request_id: "request-1",
                session_id: "session-1",
                run_id: "r0000",
                run_seq: 0,
                session_frame_start: 0,
                process_id: 42,
                target_window_id: "window-digest",
                executable_sha256: Some("sha256:exe"),
                executable_sha256_missing_reason: None,
                profile: &profile,
                mapping: &mapping,
                view_calibration: None,
            })
            .unwrap();
        journal
            .mark_run_started("session-1", "r0000", 100, 10_000_000)
            .unwrap();
        Arc::new(journal)
    }

    /// #846 的核心回归：遥测发送期间 journal 锁必须是放开的。
    ///
    /// 回归时 `record_heartbeat` 会一路等到 transport 被释放，而释放发生在断言之后
    /// ——那是死锁。所以心跳放在另一条线程上用有界等待去接，把死锁变成一次可读的
    /// 失败，而不是挂住 CI。
    #[test]
    fn a_blocked_transport_never_holds_the_journal_lock() {
        let dir = tempfile::tempdir().unwrap();
        let journal = recording_journal(&dir);
        journal
            .with_connection(|connection| {
                super::enqueue(
                    connection,
                    Some(TEST_ENVIRONMENT),
                    "app_started",
                    "info",
                    None,
                    "{}",
                )
            })
            .unwrap();

        let (entered_tx, entered_rx) = bounded::<()>(1);
        let (release_tx, release_rx) = bounded::<()>(1);
        let drainer = {
            let journal = Arc::clone(&journal);
            std::thread::spawn(move || {
                let transport = BlockingTransport {
                    entered: entered_tx,
                    release: release_rx,
                };
                super::drain_once(&journal, TEST_ENVIRONMENT, &transport).unwrap()
            })
        };

        // 等到发送真正开始：此刻若锁还在遥测线程手里，心跳就永远拿不到。
        entered_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the transport should have been called");

        let (beat_tx, beat_rx) = bounded::<()>(1);
        let beater = {
            let journal = Arc::clone(&journal);
            std::thread::spawn(move || {
                journal
                    .record_heartbeat(&HeartbeatWrite {
                        session_id: "session-1",
                        run_id: "r0000",
                        qpc: 100,
                    })
                    .unwrap();
                let _ = beat_tx.send(());
            })
        };

        // transport 还阻塞着，一步都没放。心跳必须现在就写完。
        beat_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("record_heartbeat must not wait for the client events transport (#846)");

        release_tx.send(()).unwrap();
        beater.join().unwrap();
        assert_eq!(drainer.join().unwrap().sent, 1);
    }

    /// 发送期间入队的事件 seq 更大，提交只能 ack 冻结的那一批。
    #[test]
    fn events_enqueued_while_the_transport_is_in_flight_are_not_acked() {
        let connection = outbox_db();
        enqueue(&connection, "in_flight", "info", None, "{}").unwrap();

        // 阶段一：冻结。生产里锁从这里之后就放开了。
        let frozen = freeze_batch(&connection, TEST_ENVIRONMENT)
            .unwrap()
            .unwrap();
        assert_eq!(frozen.batch.events.len(), 1);

        // 阶段二期间：录制线程照常入队自己的事件。
        enqueue(&connection, "during_send_a", "info", None, "{}").unwrap();
        enqueue(&connection, "during_send_b", "warn", None, "{}").unwrap();

        // 阶段三：只提交冻结的那一批。
        let outcome = commit_send(
            &connection,
            TEST_ENVIRONMENT,
            &frozen,
            Ok(SendDisposition::Accepted),
        )
        .unwrap();

        assert_eq!(outcome.sent, 1);
        let snapshot = cursor_snapshot(&connection).unwrap();
        assert_eq!(snapshot.acked_seq, frozen.last_seq);
        assert_eq!(
            snapshot.pending, 2,
            "events enqueued during the send must stay pending"
        );

        // 恢复后按 seq 继续排空，两条都在、顺序不变。
        let transport = CollectTransport {
            kinds: Mutex::new(Vec::new()),
        };
        assert_eq!(drain_once(&connection, &transport).unwrap().sent, 2);
        assert_eq!(
            &*transport.kinds.lock(),
            &["during_send_a", "during_send_b"]
        );
        assert_eq!(cursor_snapshot(&connection).unwrap().pending, 0);
    }

    /// 毒批丢弃同样只按栅栏删行：发送期间入队的事件不被连坐删掉。
    #[test]
    fn a_poison_batch_discard_spares_events_enqueued_during_the_send() {
        let connection = outbox_db();
        enqueue(&connection, "poison", "info", None, "{}").unwrap();
        let frozen = freeze_batch(&connection, TEST_ENVIRONMENT)
            .unwrap()
            .unwrap();
        enqueue(&connection, "survivor", "info", None, "{}").unwrap();

        let outcome = commit_send(
            &connection,
            TEST_ENVIRONMENT,
            &frozen,
            Ok(SendDisposition::RejectedByContract),
        )
        .unwrap();

        assert_eq!(outcome.dropped, 1);
        let remaining = pending_after(&connection, 0).unwrap();
        assert_eq!(
            remaining
                .iter()
                .map(|event| event.kind.as_str())
                .collect::<Vec<_>>(),
            ["survivor"]
        );
    }

    /// 传输失败只动遥测自己的退避字段，不碰采集事实。
    #[test]
    fn a_transport_failure_only_touches_the_telemetry_cursor() {
        let connection = outbox_db();
        enqueue(&connection, "in_flight", "info", None, "{}").unwrap();
        let frozen = freeze_batch(&connection, TEST_ENVIRONMENT)
            .unwrap()
            .unwrap();
        enqueue(&connection, "during_send", "info", None, "{}").unwrap();

        let outcome = commit_send(
            &connection,
            TEST_ENVIRONMENT,
            &frozen,
            Err(anyhow::anyhow!("network unreachable: secret-looking-url")),
        )
        .unwrap();

        assert!(outcome.deferred);
        let snapshot = cursor_snapshot(&connection).unwrap();
        assert_eq!(
            snapshot.acked_seq, 0,
            "a failed send must not advance the cursor"
        );
        assert_eq!(
            snapshot.pending, 2,
            "nothing may be dropped on a failed send"
        );
        assert_eq!(snapshot.attempt, 1);
        assert_eq!(
            snapshot.last_error.as_deref(),
            Some("CLIENT_EVENTS_TRANSPORT_FAILED"),
            "the raw error must never reach the journal"
        );
    }
}

//! 镜像投递：把本地 `mirror_outbox` 排空到 dataminer-mirror（docs/13）。
//!
//! 这条路是**单向、可丢、可重放**的旁路。它绝不：
//!   - 阻塞或暂停录制（失败只更新游标上的 last_error）；
//!   - 改写任何采集事实（不碰 segments/sessions/data_model_jobs）；
//!   - 持有 artifact 的可写句柄。
//!
//! 传输被抽象成 [`MirrorTransport`]，好让投递逻辑（取批、发送、推进游标、退避）
//! 能在没有网络、没有 Windows 的情况下被单测覆盖。真实实现用一个注入的
//! HTTP 客户端；构造它、选 TLS 后端、连 Credential Manager 取凭据是 wiring 的事，
//! 不在这个模块里，这样投递语义本身可以被确定性地测。

use std::sync::Arc;

use anyhow::Context;
use rusqlite::{Connection, params};

/// 单批上限。与服务端契约的 500 一致（`apps/mirror` 的 `MAX_EVENTS_PER_BATCH`）。
/// 一次取太多会让一次失败拖住一大批；太少则空跑网络。
pub const MAX_BATCH: usize = 500;

/// 退避档，秒。镜像只是看板与上传前普查的输入，不值得把重试打成忙轮询；
/// 但停用/积压状态最终要能送达，所以封顶而不是无限退。
const BACKOFF_SECONDS: [i64; 5] = [15, 60, 300, 900, 3_600];

/// 一条待投递的镜像事件，逐字段对应服务端契约。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MirrorEvent {
    pub seq: i64,
    pub entity: String,
    pub entity_id: String,
    pub payload_json: String,
    pub observed_at_ms: i64,
}

/// 一次投递请求：设备 id + 一批按 seq 升序的事件。
#[derive(Debug, Clone)]
pub struct MirrorBatch {
    pub device_id: String,
    pub events: Vec<MirrorEvent>,
}

/// 服务端对一次投递的回执。`acked_seq` 是该设备当前已确认的最大 seq。
#[derive(Debug, Clone, Copy)]
pub struct MirrorAck {
    pub acked_seq: i64,
}

/// 发送一批事件到 dataminer-mirror。实现负责认证与 HTTP；投递逻辑负责其余。
pub trait MirrorTransport: Send + Sync {
    fn send(&self, batch: &MirrorBatch) -> anyhow::Result<MirrorAck>;
}

/// 一次 drain 的结果，用于日志与测试断言。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DrainOutcome {
    pub sent: usize,
    pub acked_seq: i64,
    pub deferred: bool,
}

/// 冻结阶段的结果：要么无待发，要么一批已经定好边界的事件。
#[derive(Debug, Clone)]
enum Frozen {
    /// 无待发；`acked_seq` 就是这次 drain 的全部结果。
    Idle {
        acked_seq: i64,
    },
    Ready(FrozenBatch),
}

/// 一批已从 outbox 冻结下来、等待发送的事件。
///
/// `last_seq` 是这一批的 seq 上界，也是提交阶段唯一的栅栏：`mirror_outbox.seq`
/// 是 AUTOINCREMENT，发送期间并发入队的事件 seq 必然更大，因此按 `<= last_seq`
/// 推进游标和删行，绝不会误删一条还没投递的事件。
#[derive(Debug, Clone)]
struct FrozenBatch {
    batch: MirrorBatch,
    /// 冻结瞬间的游标；未推进时如实回报它，不假装前进。
    acked_seq: i64,
    last_seq: i64,
}

/// 阶段一：短暂持锁，冻结一批待发事件。
///
/// 只读不写，也不做任何网络 I/O——持锁时长就是一次 `SELECT ... LIMIT 500`。
fn freeze_batch(connection: &Connection, device_id: &str) -> anyhow::Result<Frozen> {
    anyhow::ensure!(
        is_lowercase_uuid(device_id),
        "mirror device id must be the lowercase uuid issued by the console"
    );
    let acked_seq = current_acked_seq(connection)?;
    let events = pending_after(connection, acked_seq)?;
    if events.is_empty() {
        return Ok(Frozen::Idle { acked_seq });
    }
    let last_seq = events.last().expect("batch is non-empty").seq;
    Ok(Frozen::Ready(FrozenBatch {
        batch: MirrorBatch {
            device_id: device_id.to_string(),
            events,
        },
        acked_seq,
        last_seq,
    }))
}

/// 阶段三：重新持锁，按 seq 栅栏原子提交这一批的结局。
///
/// 成功则把游标推进到回执的 `acked_seq` 并删除已确认行、清零重试计数；失败则
/// 记录 last_error 并按尝试次数退避，**不**推进游标、**不**删行。
///
/// 幂等来自 seq：重发同一批在服务端是 no-op（`(device_id, seq)` 唯一），因此进程
/// 在「发送成功但游标未推进」之间崩溃只会导致一次无害的重发。
fn commit_send(
    connection: &Connection,
    frozen: &FrozenBatch,
    sent: anyhow::Result<MirrorAck>,
) -> anyhow::Result<DrainOutcome> {
    match sent {
        Ok(ack) if ack.acked_seq == frozen.last_seq => {
            advance_cursor(connection, ack.acked_seq)?;
            Ok(DrainOutcome {
                sent: frozen.batch.events.len(),
                acked_seq: ack.acked_seq,
                deferred: false,
            })
        }
        Ok(ack) => {
            // 服务端正常回执必定等于本批最后一个 seq。更大通常意味着 journal 被
            // 重建但设备凭据未轮换；如果照单全收，会把新事件当成旧 seq 静默删除。
            let failure = if ack.acked_seq > frozen.last_seq {
                MirrorFailureCode::RemoteCursorAhead
            } else {
                MirrorFailureCode::IncompleteAck
            };
            record_failure(connection, failure)?;
            Ok(DrainOutcome {
                sent: 0,
                acked_seq: frozen.acked_seq,
                deferred: true,
            })
        }
        Err(_) => {
            // 失败不影响录制：只留稳定错误码并退避。error 可能含 URL、响应体甚至
            // header，不能把它原样写进 journal 或诊断导出。
            record_failure(connection, MirrorFailureCode::TransportFailed)?;
            Ok(DrainOutcome {
                sent: 0,
                acked_seq: frozen.acked_seq,
                deferred: true,
            })
        }
    }
}

/// 距离下一次可尝试还有多少秒；无待发或已到期返回 0。
///
/// 供后台循环决定睡多久，避免在退避窗口内空转。
pub fn seconds_until_ready(connection: &Connection, now_unix: i64) -> anyhow::Result<i64> {
    let acked_seq = current_acked_seq(connection)?;
    let has_pending: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM mirror_outbox WHERE seq > ?1)",
        params![acked_seq],
        |row| row.get::<_, i64>(0),
    )? == 1;
    if !has_pending {
        return Ok(i64::MAX);
    }
    let (attempt, last_attempt_unix): (i64, Option<i64>) = connection.query_row(
        "SELECT attempt_count,
                CAST(strftime('%s', last_attempt_at_utc) AS INTEGER)
         FROM mirror_cursor WHERE singleton = 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let Some(last) = last_attempt_unix else {
        return Ok(0);
    };
    let wait = backoff_for(attempt);
    Ok((last + wait - now_unix).max(0))
}

fn current_acked_seq(connection: &Connection) -> anyhow::Result<i64> {
    connection
        .query_row(
            "SELECT acked_seq FROM mirror_cursor WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .context("mirror cursor row is missing")
}

fn pending_after(connection: &Connection, acked_seq: i64) -> anyhow::Result<Vec<MirrorEvent>> {
    let mut statement = connection.prepare(
        "SELECT seq, entity, entity_id, payload_json, observed_at_ms
         FROM mirror_outbox
         WHERE seq > ?1
         ORDER BY seq
         LIMIT ?2",
    )?;
    let rows = statement.query_map(params![acked_seq, MAX_BATCH as i64], |row| {
        Ok(MirrorEvent {
            seq: row.get(0)?,
            entity: row.get(1)?,
            entity_id: row.get(2)?,
            payload_json: row.get(3)?,
            observed_at_ms: row.get(4)?,
        })
    })?;
    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}

fn advance_cursor(connection: &Connection, acked_seq: i64) -> anyhow::Result<()> {
    let tx = connection.unchecked_transaction()?;
    // acked_seq 用 MAX 保护：一个迟到的、更旧回执不能让游标倒退。
    tx.execute(
        "UPDATE mirror_cursor
         SET acked_seq = MAX(acked_seq, ?1),
             attempt_count = 0,
             last_error = NULL,
             updated_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now')
         WHERE singleton = 1",
        params![acked_seq],
    )?;
    // 已确认的行删掉：AUTOINCREMENT 保证 seq 不复用，因此删除不会让下一条
    // 撞上一个云端已经见过的 seq。
    tx.execute(
        "DELETE FROM mirror_outbox WHERE seq <= ?1",
        params![acked_seq],
    )?;
    tx.commit()?;
    Ok(())
}

fn record_failure(connection: &Connection, failure: MirrorFailureCode) -> anyhow::Result<()> {
    connection.execute(
        "UPDATE mirror_cursor
         SET attempt_count = attempt_count + 1,
             last_error = ?1,
             last_attempt_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now'),
             updated_at_utc = strftime('%Y-%m-%dT%H:%M:%fZ','now')
         WHERE singleton = 1",
        params![failure.as_str()],
    )?;
    Ok(())
}

fn backoff_for(attempt: i64) -> i64 {
    // attempt_count 在失败落库后从 1 开始：第一次失败等 15 秒，第二次 60 秒。
    let index = (attempt.max(1) - 1).min((BACKOFF_SECONDS.len() - 1) as i64) as usize;
    BACKOFF_SECONDS[index]
}

#[derive(Debug, Clone, Copy)]
enum MirrorFailureCode {
    TransportFailed,
    IncompleteAck,
    RemoteCursorAhead,
}

impl MirrorFailureCode {
    fn as_str(self) -> &'static str {
        match self {
            Self::TransportFailed => "MIRROR_TRANSPORT_FAILED",
            Self::IncompleteAck => "MIRROR_ACK_INCOMPLETE",
            Self::RemoteCursorAhead => "MIRROR_REMOTE_CURSOR_AHEAD",
        }
    }
}

fn is_lowercase_uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => byte == b'-',
            _ => byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte),
        })
}

/// 供 wiring 层持有的服务句柄。当前只暴露一次 drain 和调度间隔计算；
/// 真正的后台循环与 HTTP transport 在凭据存储与 base URL 配置接好后再落，
/// 那时它调用的就是本模块这些已被测过的函数。
pub struct MirrorDeliveryHandle {
    connection: Arc<parking_lot::Mutex<Connection>>,
}

impl MirrorDeliveryHandle {
    pub fn new(connection: Arc<parking_lot::Mutex<Connection>>) -> Self {
        Self { connection }
    }

    /// 排空一次 outbox：**冻结 → 放锁发送 → 重新持锁提交**三段。
    ///
    /// 中间那段是这个方法这么写的全部理由（#846）。这把锁和录制写 journal 用的是
    /// 同一把；一旦 HTTP 的 DNS/连接/TLS/请求/响应等待落在锁内，断网时录制心跳就
    /// 会被挡在锁外，recorder 事件队列填满后 fail-closed 停录。所以
    /// `transport.send` 只能发生在两次上锁**之间**，绝不能发生在任何一次里面。
    pub fn drain_once(
        &self,
        device_id: &str,
        transport: &dyn MirrorTransport,
    ) -> anyhow::Result<DrainOutcome> {
        let frozen = {
            let connection = self.connection.lock();
            freeze_batch(&connection, device_id)?
        };
        let frozen = match frozen {
            Frozen::Idle { acked_seq } => {
                return Ok(DrainOutcome {
                    sent: 0,
                    acked_seq,
                    deferred: false,
                });
            }
            Frozen::Ready(frozen) => frozen,
        };
        // 不持锁。这一行可能一直阻塞到传输层的 request timeout。
        let sent = transport.send(&frozen.batch);
        let connection = self.connection.lock();
        commit_send(&connection, &frozen, sent)
    }
}

/// 读取当前游标状态，用于只读 UI 投影与诊断（不含凭据、不含 payload）。
pub fn cursor_snapshot(connection: &Connection) -> anyhow::Result<CursorSnapshot> {
    let acked_seq = current_acked_seq(connection)?;
    let pending: i64 = connection.query_row(
        "SELECT COUNT(*) FROM mirror_outbox WHERE seq > ?1",
        params![acked_seq],
        |row| row.get(0),
    )?;
    let (attempt, last_error): (i64, Option<String>) = connection.query_row(
        "SELECT attempt_count, last_error FROM mirror_cursor WHERE singleton = 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    Ok(CursorSnapshot {
        acked_seq,
        pending,
        attempt,
        last_error,
    })
}

/// 只读投影。`last_error` 是脱敏后的短串，可以进 UI 与诊断。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorSnapshot {
    pub acked_seq: i64,
    pub pending: i64,
    pub attempt: i64,
    pub last_error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    const DEVICE_ID: &str = "11111111-1111-4111-8111-111111111111";

    /// 建一个只含镜像两表的库，避免把整套 journal schema 拖进单测。
    fn mirror_db() -> Connection {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE mirror_outbox (
                    seq INTEGER PRIMARY KEY AUTOINCREMENT,
                    entity TEXT NOT NULL,
                    entity_id TEXT NOT NULL,
                    payload_json TEXT NOT NULL,
                    observed_at_ms INTEGER NOT NULL,
                    created_at_utc TEXT NOT NULL
                 );
                 CREATE TABLE mirror_cursor (
                    singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
                    acked_seq INTEGER NOT NULL DEFAULT 0,
                    attempt_count INTEGER NOT NULL DEFAULT 0,
                    last_error TEXT,
                    last_attempt_at_utc TEXT,
                    updated_at_utc TEXT NOT NULL
                 );
                 INSERT INTO mirror_cursor(singleton, updated_at_utc)
                 VALUES (1, strftime('%Y-%m-%dT%H:%M:%fZ','now'));",
            )
            .unwrap();
        connection
    }

    fn enqueue(connection: &Connection, entity_id: &str) {
        connection
            .execute(
                "INSERT INTO mirror_outbox(entity, entity_id, payload_json, observed_at_ms, created_at_utc)
                 VALUES ('segment', ?1, '{\"clientUploadState\":\"unknown\"}', 1, 't')",
                params![entity_id],
            )
            .unwrap();
    }

    /// 单连接下的三段合成，供投递语义测试复用。
    ///
    /// 生产的 [`MirrorDeliveryHandle::drain_once`] 走的是同样三段，区别只在中间那段
    /// 之外会放开连接锁；语义断言不需要真锁，所以这里直接串起来。锁本身由
    /// `a_blocked_transport_never_holds_the_connection_lock` 守。
    fn drain_once(
        connection: &Connection,
        device_id: &str,
        transport: &dyn MirrorTransport,
    ) -> anyhow::Result<DrainOutcome> {
        let frozen = match freeze_batch(connection, device_id)? {
            Frozen::Idle { acked_seq } => {
                return Ok(DrainOutcome {
                    sent: 0,
                    acked_seq,
                    deferred: false,
                });
            }
            Frozen::Ready(frozen) => frozen,
        };
        let sent = transport.send(&frozen.batch);
        commit_send(connection, &frozen, sent)
    }

    struct OkTransport {
        calls: AtomicUsize,
    }
    impl MirrorTransport for OkTransport {
        fn send(&self, batch: &MirrorBatch) -> anyhow::Result<MirrorAck> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(MirrorAck {
                acked_seq: batch.events.last().unwrap().seq,
            })
        }
    }

    struct FailTransport;
    impl MirrorTransport for FailTransport {
        fn send(&self, _batch: &MirrorBatch) -> anyhow::Result<MirrorAck> {
            anyhow::bail!("network unreachable: secret-looking-url")
        }
    }

    #[test]
    fn drains_pending_and_advances_the_cursor() {
        let connection = mirror_db();
        enqueue(&connection, "a");
        enqueue(&connection, "b");
        let transport = OkTransport {
            calls: AtomicUsize::new(0),
        };

        let outcome = drain_once(&connection, DEVICE_ID, &transport).unwrap();

        assert_eq!(outcome.sent, 2);
        assert_eq!(outcome.acked_seq, 2);
        assert!(!outcome.deferred);
        let snapshot = cursor_snapshot(&connection).unwrap();
        assert_eq!(snapshot.pending, 0);
        assert_eq!(snapshot.acked_seq, 2);
    }

    #[test]
    fn a_drain_with_nothing_pending_sends_no_batch() {
        let connection = mirror_db();
        let transport = OkTransport {
            calls: AtomicUsize::new(0),
        };

        let outcome = drain_once(&connection, DEVICE_ID, &transport).unwrap();

        assert_eq!(
            outcome,
            DrainOutcome {
                sent: 0,
                acked_seq: 0,
                deferred: false
            }
        );
        assert_eq!(transport.calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn failure_defers_without_dropping_or_advancing() {
        let connection = mirror_db();
        enqueue(&connection, "a");

        let outcome = drain_once(&connection, DEVICE_ID, &FailTransport).unwrap();

        assert!(outcome.deferred);
        assert_eq!(outcome.sent, 0);
        let snapshot = cursor_snapshot(&connection).unwrap();
        // 事件仍在，游标没动，重试计数前进。
        assert_eq!(snapshot.pending, 1);
        assert_eq!(snapshot.acked_seq, 0);
        assert_eq!(snapshot.attempt, 1);
        // last_error 是脱敏短串——但绝不能把疑似 URL/密钥原样留在里面超过截断长度。
        assert_eq!(
            snapshot.last_error.as_deref(),
            Some("MIRROR_TRANSPORT_FAILED")
        );
    }

    #[test]
    fn a_successful_drain_after_failures_clears_the_error_and_backoff() {
        let connection = mirror_db();
        enqueue(&connection, "a");
        drain_once(&connection, DEVICE_ID, &FailTransport).unwrap();
        drain_once(&connection, DEVICE_ID, &FailTransport).unwrap();
        assert_eq!(cursor_snapshot(&connection).unwrap().attempt, 2);

        let outcome = drain_once(
            &connection,
            DEVICE_ID,
            &OkTransport {
                calls: AtomicUsize::new(0),
            },
        )
        .unwrap();

        assert_eq!(outcome.sent, 1);
        let snapshot = cursor_snapshot(&connection).unwrap();
        assert_eq!(snapshot.attempt, 0);
        assert!(snapshot.last_error.is_none());
    }

    #[test]
    fn the_cursor_never_moves_backward_on_a_stale_ack() {
        let connection = mirror_db();
        enqueue(&connection, "a");
        enqueue(&connection, "b");
        enqueue(&connection, "c");

        // 第一批全部确认。
        drain_once(
            &connection,
            DEVICE_ID,
            &OkTransport {
                calls: AtomicUsize::new(0),
            },
        )
        .unwrap();
        assert_eq!(cursor_snapshot(&connection).unwrap().acked_seq, 3);

        // 一个假装只确认到 seq 1 的迟到回执不能让游标倒退。
        struct StaleTransport;
        impl MirrorTransport for StaleTransport {
            fn send(&self, _batch: &MirrorBatch) -> anyhow::Result<MirrorAck> {
                Ok(MirrorAck { acked_seq: 1 })
            }
        }
        // 没有待发事件了，所以这次 drain 根本不会调用 transport；直接调 advance
        // 验证保护本身。
        advance_cursor(&connection, 1).unwrap();
        assert_eq!(cursor_snapshot(&connection).unwrap().acked_seq, 3);
        let _ = StaleTransport;
    }

    #[test]
    fn a_deleted_seq_is_never_reissued() {
        let connection = mirror_db();
        enqueue(&connection, "a");
        drain_once(
            &connection,
            DEVICE_ID,
            &OkTransport {
                calls: AtomicUsize::new(0),
            },
        )
        .unwrap();
        // 已确认行被删除；下一条必须拿到 seq 2，而不是复用 1。
        enqueue(&connection, "b");
        let next: i64 = connection
            .query_row("SELECT seq FROM mirror_outbox", [], |row| row.get(0))
            .unwrap();
        assert_eq!(next, 2);
    }

    #[test]
    fn backoff_grows_and_saturates() {
        assert_eq!(backoff_for(0), 15);
        assert_eq!(backoff_for(1), 15);
        assert_eq!(backoff_for(2), 60);
        assert_eq!(backoff_for(5), 3_600);
        // 超过档位的尝试次数停在最大值，不 panic、不无限退。
        assert_eq!(backoff_for(99), 3_600);
    }

    #[test]
    fn ready_signal_reports_no_pending_as_never() {
        let connection = mirror_db();
        assert_eq!(seconds_until_ready(&connection, 1_000).unwrap(), i64::MAX);

        enqueue(&connection, "a");
        // 从未尝试过：立刻可发。
        assert_eq!(seconds_until_ready(&connection, 1_000).unwrap(), 0);
    }

    #[test]
    fn rejects_a_remote_cursor_from_an_old_journal_generation_without_deleting() {
        let connection = mirror_db();
        enqueue(&connection, "new-journal-event");
        struct AheadTransport;
        impl MirrorTransport for AheadTransport {
            fn send(&self, _batch: &MirrorBatch) -> anyhow::Result<MirrorAck> {
                Ok(MirrorAck { acked_seq: 42 })
            }
        }

        let outcome = drain_once(&connection, DEVICE_ID, &AheadTransport).unwrap();

        assert!(outcome.deferred);
        let snapshot = cursor_snapshot(&connection).unwrap();
        assert_eq!(snapshot.acked_seq, 0);
        assert_eq!(snapshot.pending, 1);
        assert_eq!(
            snapshot.last_error.as_deref(),
            Some("MIRROR_REMOTE_CURSOR_AHEAD")
        );
    }

    #[test]
    fn rejects_non_console_device_ids_before_transport() {
        let connection = mirror_db();
        enqueue(&connection, "a");
        let transport = OkTransport {
            calls: AtomicUsize::new(0),
        };

        assert!(drain_once(&connection, "MY-LAPTOP", &transport).is_err());
        assert_eq!(transport.calls.load(Ordering::SeqCst), 0);
        assert_eq!(cursor_snapshot(&connection).unwrap().pending, 1);
    }

    /// 阻塞在发送里的 transport：`entered` 通知发送已经真正开始，`release` 之前不返回。
    struct BlockingTransport {
        entered: crossbeam_channel::Sender<()>,
        release: crossbeam_channel::Receiver<()>,
    }

    impl MirrorTransport for BlockingTransport {
        fn send(&self, batch: &MirrorBatch) -> anyhow::Result<MirrorAck> {
            self.entered
                .send(())
                .expect("test harness dropped the receiver");
            self.release
                .recv()
                .expect("test harness dropped the sender");
            Ok(MirrorAck {
                acked_seq: batch.events.last().expect("batch is non-empty").seq,
            })
        }
    }

    /// #846 的同款回归：投递发送期间连接锁必须是放开的。
    ///
    /// 回归时下面那次 `lock()` 会一路等到 transport 被释放，而释放发生在断言之后
    /// ——那是死锁。所以取锁放在另一条线程上用有界等待去接，把死锁变成一次可读的
    /// 失败，而不是挂住 CI。
    #[test]
    fn a_blocked_transport_never_holds_the_connection_lock() {
        let connection = mirror_db();
        enqueue(&connection, "segment-1");
        let connection = Arc::new(parking_lot::Mutex::new(connection));
        let handle = MirrorDeliveryHandle::new(Arc::clone(&connection));

        let (entered_tx, entered_rx) = crossbeam_channel::bounded::<()>(1);
        let (release_tx, release_rx) = crossbeam_channel::bounded::<()>(1);
        let drainer = std::thread::spawn(move || {
            let transport = BlockingTransport {
                entered: entered_tx,
                release: release_rx,
            };
            handle.drain_once(DEVICE_ID, &transport).unwrap()
        });

        // 等到发送真正开始：此刻若锁还在投递线程手里，下面就永远拿不到。
        entered_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the transport should have been called");

        let (locked_tx, locked_rx) = crossbeam_channel::bounded::<()>(1);
        let locker = std::thread::spawn(move || {
            let guard = connection.lock();
            let pending: i64 = guard
                .query_row("SELECT COUNT(*) FROM mirror_outbox", [], |row| row.get(0))
                .unwrap();
            assert_eq!(pending, 1);
            drop(guard);
            let _ = locked_tx.send(());
        });

        // transport 还阻塞着，一步都没放。别的写者必须现在就能拿到锁。
        locked_rx.recv_timeout(Duration::from_secs(10)).expect(
            "the journal connection must not stay locked across the mirror transport (#846)",
        );

        release_tx.send(()).unwrap();
        locker.join().unwrap();
        assert_eq!(drainer.join().unwrap().sent, 1);
    }
}

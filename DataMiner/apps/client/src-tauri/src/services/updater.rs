//! M-A 版本检查（docs/15 §2.3–§2.4）：只检查、只提示。
//!
//! 本模块永远不下载、不验签、不执行任何字节——那些属于 M-B，且以 minisign
//! 公钥仪式完成为前置门（docs/15 裁决 D2）。检查是对渠道静态 `latest.json`
//! 的匿名 GET：不携带设备身份、无查询参数（docs/15 边界 4 / 裁决 D5）。
//! 检查失败对用户静默，只写诊断事件；版本检查永远不影响采集可用性。

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, Wry};
use tauri_plugin_updater::UpdaterExt;

use crate::app_state::AppState;
use crate::services::diagnostics::{DiagnosticCorrelation, DiagnosticPayload, DiagnosticService};

/// 启动后首次自动检查的延迟：恢复门与采集路径优先（docs/15 §2.3）。
pub const STARTUP_CHECK_DELAY: Duration = Duration::from_secs(30);
/// 常驻托盘进程的自动检查节流间隔；手动检查不受节流。
pub const AUTO_CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
/// 渠道是 CDN 缓存的静态文件，5s 拿不到就按本次失败处理。
const CHECK_TIMEOUT: Duration = Duration::from_secs(5);
/// 发布说明来自 tag message，防御性截断以约束快照与事件体积。
const NOTES_MAX_CHARS: usize = 2_000;

pub const STATUS_EVENT: &str = "updater://status";

/// 策略的三个编译期输入，顺序与 `checks_enabled_for` 的形参一致。
///
/// 单独暴露成元组，是为了让 CI 能在**一次编译**里核对每根线接得对不对——
/// 只看 `checks_enabled()` 那一个 bool 是分辨不出「哪根线错了」的，
/// 从前只能靠为每种组合各链一个可执行文件再跑测试来反推（见 dataminer-ci.yml）。
pub const fn policy_inputs() -> (bool, bool, bool) {
    (
        crate::build_flavor::uses_delivered_catalog(),
        cfg!(debug_assertions),
        cfg!(feature = "dev-catalog"),
    )
}

/// 只有不携开发 catalog 的 Delivered Catalog release 才允许检查
/// （docs/15 §2.3）。这包括已签名 production 及标记清楚的 unsigned-delivered；
/// debug 或 dev-catalog 任一条件命中仍 fail closed，避免本地开发包误访问发布渠道。
pub const fn checks_enabled() -> bool {
    let (delivered_release, debug_assertions, dev_catalog) = policy_inputs();
    checks_enabled_for(delivered_release, debug_assertions, dev_catalog)
}

const fn checks_enabled_for(
    delivered_release: bool,
    debug_assertions: bool,
    dev_catalog: bool,
) -> bool {
    delivered_release && !debug_assertions && !dev_catalog
}

/// 编译期核对：这次编译出来的策略必须等于 CI 显式声明的期望值。
///
/// 为什么是编译期而不是测试：这四个值全是编译期常量，问它们等于几不需要产物跑
/// 起来。从前的做法是为五种组合各链一个 Tauri 测试可执行文件再运行，CI 上要
/// 38 分钟，其中真正执行测试的时间是 0.08 秒。写成 const 断言后 `cargo check`
/// 和 `cargo clippy` 就能判定，不必代码生成、不必链接。
///
/// 期望值必须由外部显式给入、不能从编译出的风味反推，否则就是让被测对象自己
/// 出题自己判卷。分工是：本断言只管**接线**（三个输入有没有真的接进策略），
/// 策略逻辑本身的八行真值表由 `update_policy_truth_table_is_fail_closed` 覆盖。
///
/// 一旦 `DATAMINER_EXPECT_UPDATER_ENABLED` 在场即视为武装，此时其余三个期望值
/// 缺一个就编译失败——从前那个运行时版本遇到变量缺失是静默 return 然后报「通过」。
/// 未设任何期望值时（开发者日常构建）本块不做断言。
const _: () = match option_env!("DATAMINER_EXPECT_UPDATER_ENABLED") {
    None => (),
    Some(_) => {
        let (delivered_release, debug_assertions, dev_catalog) = policy_inputs();
        let expected_delivered_release = match option_env!("DATAMINER_EXPECT_BUILD_FLAVOR") {
            Some(raw) => matches!(raw.as_bytes(), b"production" | b"unsigned-delivered"),
            None => {
                panic!("DATAMINER_EXPECT_BUILD_FLAVOR is required once the policy check is armed")
            }
        };
        let expected_debug = expect_policy_flag(
            option_env!("DATAMINER_EXPECT_DEBUG_ASSERTIONS"),
            "DATAMINER_EXPECT_DEBUG_ASSERTIONS is required once the policy check is armed",
        );
        let expected_dev_catalog = expect_policy_flag(
            option_env!("DATAMINER_EXPECT_DEV_CATALOG"),
            "DATAMINER_EXPECT_DEV_CATALOG is required once the policy check is armed",
        );
        let expected_enabled = expect_policy_flag(
            option_env!("DATAMINER_EXPECT_UPDATER_ENABLED"),
            "DATAMINER_EXPECT_UPDATER_ENABLED is required once the policy check is armed",
        );
        assert!(
            delivered_release == expected_delivered_release,
            "compiled DATAMINER_BUILD_FLAVOR did not match the explicit expectation"
        );
        assert!(
            debug_assertions == expected_debug,
            "compiled debug_assertions did not match the explicit expectation"
        );
        assert!(
            dev_catalog == expected_dev_catalog,
            "compiled dev-catalog feature did not match the explicit expectation"
        );
        assert!(
            checks_enabled() == expected_enabled,
            "compiled updater policy did not match the explicit expectation"
        );
    }
};

const fn expect_policy_flag(value: Option<&str>, missing: &'static str) -> bool {
    match value {
        Some(raw) => match raw.as_bytes() {
            b"1" => true,
            b"0" => false,
            _ => panic!("updater policy expectations must be exactly \"0\" or \"1\""),
        },
        None => panic!("{}", missing),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CheckTrigger {
    Startup,
    Scheduled,
    Manual,
}

impl CheckTrigger {
    fn as_str(self) -> &'static str {
        match self {
            CheckTrigger::Startup => "startup",
            CheckTrigger::Scheduled => "scheduled",
            CheckTrigger::Manual => "manual",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum UpdateCheckState {
    Disabled,
    Idle,
    Checking,
    UpToDate,
    UpdateAvailable,
    Failed,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateStatusSnapshot {
    pub current_version: String,
    pub state: UpdateCheckState,
    pub latest_version: Option<String>,
    pub notes: Option<String>,
    pub checked_at_unix_ms: Option<u64>,
    pub last_error: Option<String>,
}

#[derive(Clone, Debug)]
enum CheckOutcome {
    UpToDate,
    UpdateAvailable {
        version: String,
        notes: Option<String>,
    },
    Failed {
        error: String,
    },
}

struct Inner {
    snapshot: UpdateStatusSnapshot,
    checking: bool,
}

pub struct UpdateChecker {
    diagnostics: Arc<DiagnosticService>,
    inner: Mutex<Inner>,
}

impl UpdateChecker {
    pub fn new(current_version: String, diagnostics: Arc<DiagnosticService>) -> Self {
        let state = if checks_enabled() {
            UpdateCheckState::Idle
        } else {
            UpdateCheckState::Disabled
        };
        Self {
            diagnostics,
            inner: Mutex::new(Inner {
                snapshot: UpdateStatusSnapshot {
                    current_version,
                    state,
                    latest_version: None,
                    notes: None,
                    checked_at_unix_ms: None,
                    last_error: None,
                },
                checking: false,
            }),
        }
    }

    pub fn snapshot(&self) -> UpdateStatusSnapshot {
        self.inner.lock().snapshot.clone()
    }

    fn begin_check(&self) -> Option<UpdateStatusSnapshot> {
        let mut inner = self.inner.lock();
        if inner.snapshot.state == UpdateCheckState::Disabled || inner.checking {
            return Some(inner.snapshot.clone());
        }
        inner.checking = true;
        inner.snapshot.state = UpdateCheckState::Checking;
        None
    }

    /// 执行一次检查并返回最新快照。disabled 或已有检查在途时不发起新请求，
    /// 直接返回当前快照。
    pub async fn check(&self, app: &AppHandle<Wry>, trigger: CheckTrigger) -> UpdateStatusSnapshot {
        if let Some(snapshot) = self.begin_check() {
            return snapshot;
        }
        emit_status(app, &self.snapshot());

        let outcome = run_check(app).await;
        let snapshot = {
            let mut inner = self.inner.lock();
            inner.checking = false;
            apply_outcome(&mut inner.snapshot, &outcome, unix_ms());
            inner.snapshot.clone()
        };
        self.record_diagnostics(trigger, &outcome, &snapshot);
        emit_status(app, &snapshot);
        snapshot
    }

    fn record_diagnostics(
        &self,
        trigger: CheckTrigger,
        outcome: &CheckOutcome,
        snapshot: &UpdateStatusSnapshot,
    ) {
        let (level, payload) = match outcome {
            CheckOutcome::Failed { error } => (
                "warning",
                DiagnosticPayload::UpdateCheckFailed {
                    trigger: trigger.as_str().into(),
                    error: error.clone(),
                },
            ),
            _ => (
                "info",
                DiagnosticPayload::UpdateCheck {
                    trigger: trigger.as_str().into(),
                    current_version: snapshot.current_version.clone(),
                    latest_version: snapshot.latest_version.clone(),
                    update_available: snapshot.latest_version.is_some(),
                },
            ),
        };
        if let Err(error) = self.diagnostics.record(
            level,
            None,
            DiagnosticCorrelation::default(),
            payload,
            false,
        ) {
            tracing::warn!(error = %error, "failed to record update-check diagnostic event");
        }
    }
}

/// 自动检查循环：启动 30s 后一次，此后每 24h 一次。只在 release 构建被 spawn。
pub async fn auto_check_loop(app: AppHandle<Wry>) {
    tokio::time::sleep(STARTUP_CHECK_DELAY).await;
    let checker = Arc::clone(&app.state::<AppState>().updater);
    checker.check(&app, CheckTrigger::Startup).await;
    let mut ticker = tokio::time::interval(AUTO_CHECK_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // interval 的首个 tick 立即完成；先消费掉，避免启动检查后立刻再查一次。
    ticker.tick().await;
    loop {
        ticker.tick().await;
        checker.check(&app, CheckTrigger::Scheduled).await;
    }
}

async fn run_check(app: &AppHandle<Wry>) -> CheckOutcome {
    // tauri-plugin-updater 的 check() 只做 GET + semver 比较；配置里的占位
    // pubkey 只会在 download/验签路径被解码，而 M-A 不存在那条路径。
    let updater = match app.updater_builder().timeout(CHECK_TIMEOUT).build() {
        Ok(updater) => updater,
        Err(error) => {
            return CheckOutcome::Failed {
                error: error.to_string(),
            };
        }
    };
    match updater.check().await {
        Ok(Some(update)) => CheckOutcome::UpdateAvailable {
            version: update.version.clone(),
            notes: update.body.clone(),
        },
        Ok(None) => CheckOutcome::UpToDate,
        Err(error) => CheckOutcome::Failed {
            error: error.to_string(),
        },
    }
}

fn apply_outcome(snapshot: &mut UpdateStatusSnapshot, outcome: &CheckOutcome, now_unix_ms: u64) {
    snapshot.checked_at_unix_ms = Some(now_unix_ms);
    match outcome {
        CheckOutcome::UpToDate => {
            snapshot.state = UpdateCheckState::UpToDate;
            snapshot.latest_version = None;
            snapshot.notes = None;
            snapshot.last_error = None;
        }
        CheckOutcome::UpdateAvailable { version, notes } => {
            snapshot.state = UpdateCheckState::UpdateAvailable;
            snapshot.latest_version = Some(version.clone());
            snapshot.notes = notes.as_deref().map(truncate_notes);
            snapshot.last_error = None;
        }
        CheckOutcome::Failed { error } => {
            snapshot.last_error = Some(error.clone());
            // 检查失败不清除已发现的新版本：一次网络抖动不该让提示消失
            // （docs/15 §2.4）。从未发现过更新时才进入 failed 态。
            snapshot.state = if snapshot.latest_version.is_some() {
                UpdateCheckState::UpdateAvailable
            } else {
                UpdateCheckState::Failed
            };
        }
    }
}

fn truncate_notes(notes: &str) -> String {
    let trimmed = notes.trim();
    if trimmed.chars().count() <= NOTES_MAX_CHARS {
        return trimmed.to_owned();
    }
    let mut truncated: String = trimmed.chars().take(NOTES_MAX_CHARS).collect();
    truncated.push('…');
    truncated
}

fn emit_status(app: &AppHandle<Wry>, snapshot: &UpdateStatusSnapshot) {
    if let Err(error) = app.emit(STATUS_EVENT, snapshot) {
        tracing::warn!(error = %error, "failed to emit updater status");
    }
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> UpdateStatusSnapshot {
        UpdateStatusSnapshot {
            current_version: "0.1.0".into(),
            state: UpdateCheckState::Idle,
            latest_version: None,
            notes: None,
            checked_at_unix_ms: None,
            last_error: None,
        }
    }

    #[test]
    fn up_to_date_clears_prior_update_and_error() {
        let mut status = snapshot();
        apply_outcome(
            &mut status,
            &CheckOutcome::UpdateAvailable {
                version: "0.2.0".into(),
                notes: Some("notes".into()),
            },
            1,
        );
        apply_outcome(&mut status, &CheckOutcome::UpToDate, 2);
        assert_eq!(status.state, UpdateCheckState::UpToDate);
        assert_eq!(status.latest_version, None);
        assert_eq!(status.notes, None);
        assert_eq!(status.checked_at_unix_ms, Some(2));
    }

    #[test]
    fn failure_never_clears_a_discovered_update() {
        let mut status = snapshot();
        apply_outcome(
            &mut status,
            &CheckOutcome::UpdateAvailable {
                version: "0.2.0".into(),
                notes: None,
            },
            1,
        );
        apply_outcome(
            &mut status,
            &CheckOutcome::Failed {
                error: "timeout".into(),
            },
            2,
        );
        assert_eq!(status.state, UpdateCheckState::UpdateAvailable);
        assert_eq!(status.latest_version.as_deref(), Some("0.2.0"));
        assert_eq!(status.last_error.as_deref(), Some("timeout"));
    }

    #[test]
    fn failure_without_prior_update_is_failed_state() {
        let mut status = snapshot();
        apply_outcome(
            &mut status,
            &CheckOutcome::Failed {
                error: "dns".into(),
            },
            1,
        );
        assert_eq!(status.state, UpdateCheckState::Failed);
        assert_eq!(status.latest_version, None);
    }

    #[test]
    fn notes_are_trimmed_and_bounded() {
        let long = "x".repeat(NOTES_MAX_CHARS + 10);
        let mut status = snapshot();
        apply_outcome(
            &mut status,
            &CheckOutcome::UpdateAvailable {
                version: "0.2.0".into(),
                notes: Some(format!("  {long}  ")),
            },
            1,
        );
        let notes = status.notes.expect("notes retained");
        assert_eq!(notes.chars().count(), NOTES_MAX_CHARS + 1);
        assert!(notes.ends_with('…'));
    }

    #[test]
    fn update_policy_truth_table_is_fail_closed() {
        for (delivered_release, debug_assertions, dev_catalog, expected) in [
            (false, false, false, false),
            (false, false, true, false),
            (false, true, false, false),
            (false, true, true, false),
            (true, false, false, true),
            (true, false, true, false),
            (true, true, false, false),
            (true, true, true, false),
        ] {
            assert_eq!(
                checks_enabled_for(delivered_release, debug_assertions, dev_catalog),
                expected,
                "delivered_release={delivered_release}, debug={debug_assertions}, dev_catalog={dev_catalog}"
            );
        }
    }

    #[cfg(debug_assertions)]
    #[test]
    fn compiled_debug_profile_disables_updater() {
        if cfg!(feature = "dev-catalog") {
            assert!(!checks_enabled(), "debug + dev-catalog must be disabled");
            return;
        }

        assert!(!checks_enabled());
    }

    #[cfg(all(not(debug_assertions), feature = "dev-catalog"))]
    #[test]
    fn compiled_release_with_dev_catalog_disables_updater() {
        assert!(!checks_enabled());
    }

    #[cfg(all(not(debug_assertions), not(feature = "dev-catalog")))]
    #[test]
    fn compiled_release_without_dev_catalog_follows_delivered_flavor() {
        if crate::build_flavor::uses_delivered_catalog() {
            assert!(checks_enabled(), "delivered release must remain enabled");
        } else {
            assert!(
                !checks_enabled(),
                "local development release must be disabled"
            );
        }
    }

    #[tokio::test]
    async fn disabled_manual_check_returns_before_updater_access() {
        if checks_enabled() {
            return;
        }

        let temp = tempfile::tempdir().unwrap();
        let diagnostics = Arc::new(DiagnosticService::open(temp.path()).unwrap());
        let checker = UpdateChecker::new("0.1.0".into(), diagnostics);

        assert_eq!(checker.snapshot().state, UpdateCheckState::Disabled);

        // Deliberately omit the updater plugin. If check() touches the Tauri
        // updater before the Disabled guard, this call cannot complete normally.
        let app = tauri::Builder::<Wry>::default()
            .any_thread()
            .build(tauri::test::mock_context(tauri::test::noop_assets()))
            .unwrap();
        let status = checker.check(app.handle(), CheckTrigger::Manual).await;

        assert_eq!(status.state, UpdateCheckState::Disabled);
        assert_eq!(status.checked_at_unix_ms, None);
    }
}

//! 条款变更观察器（docs/108 §3.6）：服务端换了同意书，本人要在客户端里知道。
//!
//! 在这之前，consent 状态只有账号页 overview 会拉一次（`Account.tsx:158`）。采集员
//! 不点开账号页，就永远不知道条款变了——docs/105 §4 的就地修订之所以只能靠「内部
//! 员工线下逐一告知」，缺的正是这条路径。
//!
//! 形状照抄 [`crate::services::updater`] 的自动检查：启动后延迟首检、之后按间隔轮询、
//! 快照 + 事件推给前端。三条纪律与它一致：
//!
//! 1. **只读、只提示。** 本模块永远不签署任何东西——补签是本人在界面上的动作。
//! 2. **失败静默。** 未登录、未配置账号服务、网络不通，都不产生任何用户可见的噪声，
//!    也绝不触达采集路径。条款检查失败不该让任何人少录一帧。
//! 3. **不是采集事实源。** orchestrator 不读它；它唯一的产物是界面上的一条提示。

use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, Wry};

use crate::app_state::AppState;
use crate::services::account::ConsentState;

/// 启动后首检的延迟。恢复门与采集路径优先，与版本检查同一条理由（docs/15 §2.3）。
pub const STARTUP_CHECK_DELAY: Duration = Duration::from_secs(20);
/// 常驻托盘进程的轮询间隔。条款是**人**在服务端换的，按天计的事件按小时轮询已经
/// 远快于它发生的速度；再密只是给一个不会变的东西加请求。
pub const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

pub const STATUS_EVENT: &str = "consent://status";
/// 通知码。前端据此认出这条通知要配「去签署」的动作，而不是靠匹配文案。
pub const RESIGN_NOTIFICATION_CODE: &str = "consent_resign_required";

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsentNoticeSnapshot {
    /// 服务端在服务的条款与本人已签的那份不是同一份文书（比对 hash，不是版本串）。
    pub resign_required: bool,
    /// 服务端当前条款的版本串，仅用于显示。
    pub current_policy_version: Option<String>,
    /// 补签会新增的授权范围。**空集不等于不用补签**——文书可能只是措辞修订，
    /// 范围没变；那时仍要补签（签的是文书，不是范围），只是提示里没有"新增"可说。
    pub pending_scopes: Vec<String>,
    /// 本人已签那份文书声明的范围。
    pub signed_scopes: Vec<String>,
    /// 是否已经至少成功查到过一次。前端据此区分"没有待办"与"还不知道"——
    /// 把后者渲染成前者，等于对采集员说了一件我们并不知道的事。
    pub checked: bool,
}

pub struct ConsentWatcher {
    snapshot: Mutex<ConsentNoticeSnapshot>,
    /// 上一次已经通知过的文书。用它做边沿判定：横幅常驻，弹一次就够——
    /// 每 6 小时重弹一次同一条通知只会训练人把通知当背景噪声。
    notified_policy_version: Mutex<Option<String>>,
}

impl ConsentWatcher {
    pub fn new() -> Self {
        Self {
            snapshot: Mutex::new(ConsentNoticeSnapshot::default()),
            notified_policy_version: Mutex::new(None),
        }
    }

    pub fn snapshot(&self) -> ConsentNoticeSnapshot {
        self.snapshot.lock().clone()
    }

    /// 查一次并按需推送。返回是否查成功，供调用方决定退避。
    pub fn check(&self, app: &AppHandle<Wry>) -> bool {
        let state = app.state::<AppState>();
        let Some(account) = state.account.clone() else {
            // 账号面未配置、配置非法或启动降级时都没有可查询的服务；具体原因由
            // account availability 与启动诊断呈现，观察器不再重复制造提示。
            return false;
        };
        match account.consent_state() {
            Ok(consent) => {
                self.apply(app, &consent);
                true
            }
            Err(error) => {
                // 未登录是最常见的一种，且完全正常——离线启动、还没登录、会话过期
                // 都会走到这里，所以它连一条 debug 日志都不该产生。不清空既有快照：
                // 上一次查到的事实并没有因为这次没查成而变得不真。
                if error.code != "session_required" {
                    tracing::debug!(
                        code = %error.code,
                        "consent check failed; keeping the previous snapshot"
                    );
                }
                false
            }
        }
    }

    fn apply(&self, app: &AppHandle<Wry>, consent: &ConsentState) {
        let next = ConsentNoticeSnapshot {
            resign_required: consent.resign_required || !consent.matches_current_terms,
            current_policy_version: Some(consent.current_policy_version.clone()),
            pending_scopes: consent.pending_scopes.clone(),
            signed_scopes: consent
                .consent
                .as_ref()
                .map(|signed| signed.scopes.clone())
                .unwrap_or_default(),
            checked: true,
        };
        let changed = {
            let mut held = self.snapshot.lock();
            let changed = *held != next;
            *held = next.clone();
            changed
        };
        if changed && let Err(error) = app.emit(STATUS_EVENT, &next) {
            tracing::warn!(error = %error, "failed to emit consent status");
        }
        if next.resign_required {
            self.notify_once(app, &next);
        } else {
            // 签完了就把边沿放开：下一次条款再变还要再提醒一次。
            *self.notified_policy_version.lock() = None;
        }
    }

    fn notify_once(&self, app: &AppHandle<Wry>, snapshot: &ConsentNoticeSnapshot) {
        let version = snapshot.current_policy_version.clone();
        {
            let mut notified = self.notified_policy_version.lock();
            if *notified == version {
                return;
            }
            *notified = version;
        }
        let state = app.state::<AppState>();
        let notification = state.orchestrator.diagnostics().create_notification(
            RESIGN_NOTIFICATION_CODE,
            resign_message(snapshot),
            "打开「账号」页，读完条款后勾选并签署。采集不受影响，可以先继续录。",
            true,
            crate::services::diagnostics::DiagnosticCorrelation::default(),
            None,
        );
        if let Err(error) = app.emit("collector://notification", &notification) {
            tracing::warn!(error = %error, "failed to emit consent notification");
        }
    }
}

impl Default for ConsentWatcher {
    fn default() -> Self {
        Self::new()
    }
}

/// 提示正文。差集在前——「新增了什么」才是采集员用来判断的信息，版本串不是。
pub fn resign_message(snapshot: &ConsentNoticeSnapshot) -> String {
    if snapshot.pending_scopes.is_empty() {
        return "采集条款已更新，需要你重新签署。".to_string();
    }
    let added = snapshot
        .pending_scopes
        .iter()
        .map(|scope| scope_label(scope))
        .collect::<Vec<_>>()
        .join("、");
    format!("采集条款已更新，需要你补充签署以扩大授权范围。新增：{added}。")
}

/// scope 词的中文说法。未知词原样回显而不是吞掉——服务端加了新词而客户端还没更新
/// 时，显示一个英文词远好过显示一句漏掉了它的话。
fn scope_label(scope: &str) -> String {
    match scope {
        "input" => "键鼠输入".to_string(),
        "video" => "游戏画面".to_string(),
        "audio" => "游戏进程音频".to_string(),
        "depth" => "深度与相机参数".to_string(),
        "public_release" => "公开发布".to_string(),
        "trainable_research" => "模型训练研究".to_string(),
        other => other.to_string(),
    }
}

/// 后台轮询。与 `updater::auto_check_loop` 同构。
pub async fn watch_loop(app: AppHandle<Wry>) {
    tokio::time::sleep(STARTUP_CHECK_DELAY).await;
    let watcher = Arc::clone(&app.state::<AppState>().consent_watcher);
    let handle = app.clone();
    let checker = Arc::clone(&watcher);
    let _ = tauri::async_runtime::spawn_blocking(move || checker.check(&handle)).await;
    let mut ticker = tokio::time::interval(CHECK_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // interval 的首个 tick 立即完成；先消费掉，避免刚查完又查一次。
    ticker.tick().await;
    loop {
        ticker.tick().await;
        let handle = app.clone();
        let checker = Arc::clone(&watcher);
        let _ = tauri::async_runtime::spawn_blocking(move || checker.check(&handle)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(pending: &[&str]) -> ConsentNoticeSnapshot {
        ConsentNoticeSnapshot {
            resign_required: true,
            current_policy_version: Some("2026-07-25-teaser".to_string()),
            pending_scopes: pending.iter().map(|scope| scope.to_string()).collect(),
            signed_scopes: vec!["input".to_string(), "video".to_string()],
            checked: true,
        }
    }

    #[test]
    fn message_names_what_re_signing_adds() {
        assert_eq!(
            resign_message(&snapshot(&["audio", "depth"])),
            "采集条款已更新，需要你补充签署以扩大授权范围。新增：游戏进程音频、深度与相机参数。"
        );
    }

    #[test]
    fn message_still_asks_when_the_scope_set_is_unchanged() {
        // 措辞修订也要重签——签的是文书，不是范围。没有"新增"可说时不能因此
        // 静默，否则一次纯措辞的改版就无人补签。
        assert_eq!(
            resign_message(&snapshot(&[])),
            "采集条款已更新，需要你重新签署。"
        );
    }

    #[test]
    fn unknown_scope_words_survive_instead_of_vanishing() {
        // 服务端先于客户端加了新 scope 词时，宁可显示英文原词，也不能显示一句
        // 漏掉它的提示——后者会让采集员以为自己知道全部新增内容。
        assert!(resign_message(&snapshot(&["haptics"])).contains("haptics"));
    }

    #[test]
    fn default_snapshot_is_not_yet_checked() {
        // 「还没查」与「没有待办」必须分得开：把前者渲染成后者，等于对采集员
        // 说了一件我们并不知道的事。
        let fresh = ConsentNoticeSnapshot::default();
        assert!(!fresh.checked);
        assert!(!fresh.resign_required);
    }
}

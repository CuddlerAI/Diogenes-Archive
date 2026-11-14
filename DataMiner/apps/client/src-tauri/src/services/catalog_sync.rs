//! 签名 catalog 的运行时同步与激活。
//!
//! Tauri async 循环只负责调度；blocking reqwest、Credential Manager 与 SQLite 全部在
//! `spawn_blocking` 中执行。活动 catalog 每次都从 SQLite 原始封套重新验签得到，网络失败
//! 保留上一份有效缓存，采集活动期间则只落缓存、延迟到安全窗口再激活。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, bail};
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use tokio::sync::Notify;

use crate::services::account::{
    AccountEnvironment, ResolvedAccountConfiguration, load_or_create_client_device_id,
};
use crate::services::api_endpoint::production_api_base_url;
use crate::services::catalog_delivery::{
    AcceptedCatalog, CatalogDelivery, CatalogFetchResult, CatalogFetcher, CatalogStateEnvironment,
    ReqwestCatalogHttpTransport, RevocationList, SqliteCatalogDeliveryState, SyncOutcome,
    fetch_bootstrap_catalog_over_http, fetch_catalog_over_http, stable_catalog_error_code,
};
use crate::services::credentials::{
    self, CredentialKind, CredentialStore, PRODUCTION_CREDENTIAL_SCOPE,
};
use crate::services::diagnostics::{DiagnosticCorrelation, DiagnosticPayload};
use crate::services::orchestrator::SessionOrchestrator;

const SYNC_INTERVAL_SECONDS: i64 = 10 * 60;
const MAX_JITTER_SECONDS: i64 = 60;

/// 按本机当前**已有的凭据**选择拉取通道。
///
/// 绑定后走摄取面的 `GET /v1/catalog`，带设备凭据——这是稳态，也是唯一一条在采集
/// 期间会跑的路。绑定前设备凭据还不存在，改走 console 的
/// `GET /app/v1/catalog/bootstrap`，带标注员会话。
///
/// 为什么需要后者：设备绑定要提交一个服务端已批准的设备配置身份，而客户端只能从
/// 签名 catalog 里知道有哪些；拉 catalog 又要设备凭据；设备凭据只有绑定成功才签发。
/// 一台新机器因此永远进不来（issue #323）。
///
/// 选择顺序不能反过来：只要设备凭据在，就必须用它。设备凭据是长期身份，会话是会
/// 过期的短期身份；绑定完还继续用会话拉目录，等于让稳态依赖一个随时会失效的东西。
struct AuthenticatedCatalogFetcher {
    transport: ReqwestCatalogHttpTransport,
    base_url: String,
    /// 启动时唯一一次解析出的账号环境与 base；`None` 时没有绑定前通道。
    account_configuration: Option<ResolvedAccountConfiguration>,
    credential_store: Arc<dyn CredentialStore>,
    client_version: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CatalogAuthChannel {
    DeviceCredential,
    AnnotatorSession,
}

/// 本机现在能用哪条通道；`None` 表示没有显式 ready 配置或没有该环境凭据，不该发请求。
///
/// 调度器用它决定要不要发起这一轮，fetcher 用同一个判断决定带哪把钥匙。两边共用
/// 一个函数是有意的：如果调度器认为"可以发"而 fetcher 认为"没钥匙"，表现出来就是
/// 每 30 秒一次必败的请求外加一条误导性的错误码。
fn available_auth_channel(
    store: &dyn CredentialStore,
    account_configuration: Option<&ResolvedAccountConfiguration>,
) -> anyhow::Result<Option<CatalogAuthChannel>> {
    // catalog 的 device 稳态通道仍属于 production ingest（#144 不在本修复范围）。
    // 只有显式 ready production 才能启用；staging、缺省、非法或账号启动降级都绝不能
    // 让一把遗留 production device 凭据把“未选择”重新解释成 production。
    let production_device_allowed = account_configuration
        .is_some_and(|configuration| configuration.environment == AccountEnvironment::Production);
    if production_device_allowed
        && credentials::load(store, CredentialKind::Device, PRODUCTION_CREDENTIAL_SCOPE)?.is_some()
    {
        return Ok(Some(CatalogAuthChannel::DeviceCredential));
    }
    if let Some(configuration) = account_configuration
        && credentials::load(
            store,
            CredentialKind::AnnotatorSession,
            configuration.environment.credential_scope(),
        )?
        .is_some()
    {
        return Ok(Some(CatalogAuthChannel::AnnotatorSession));
    }
    Ok(None)
}

fn selected_catalog_state_environment(
    account_configuration: Option<&ResolvedAccountConfiguration>,
) -> Option<CatalogStateEnvironment> {
    account_configuration.map(|configuration| match configuration.environment {
        AccountEnvironment::Staging => CatalogStateEnvironment::Staging,
        AccountEnvironment::Production => CatalogStateEnvironment::Production,
    })
}

impl CatalogFetcher for AuthenticatedCatalogFetcher {
    fn fetch(&self, etag: Option<&str>) -> anyhow::Result<CatalogFetchResult> {
        let store = self.credential_store.as_ref();
        // 凭据在真正要发请求时才从 Credential Manager 展开，不驻留在 fetcher 里。
        match available_auth_channel(store, self.account_configuration.as_ref())? {
            Some(CatalogAuthChannel::DeviceCredential) => {
                let credential =
                    credentials::load(store, CredentialKind::Device, PRODUCTION_CREDENTIAL_SCOPE)?
                        .context("catalog delivery requires an enrolled production device")?;
                fetch_catalog_over_http(
                    &self.transport,
                    &self.base_url,
                    credential.expose(),
                    etag,
                    &self.client_version,
                )
            }
            Some(CatalogAuthChannel::AnnotatorSession) => {
                let account_configuration = self
                    .account_configuration
                    .as_ref()
                    .context("catalog bootstrap requires the account surface")?;
                let session = credentials::load(
                    store,
                    CredentialKind::AnnotatorSession,
                    account_configuration.environment.credential_scope(),
                )?
                .context("catalog bootstrap requires a signed-in annotator")?;
                fetch_bootstrap_catalog_over_http(
                    &self.transport,
                    &account_configuration.base_url,
                    session.expose(),
                    etag,
                    &self.client_version,
                )
            }
            None => bail!(
                "catalog_auth_missing: catalog delivery needs an enrolled device or a signed-in annotator"
            ),
        }
    }
}

struct CatalogSyncInner {
    delivery: Mutex<CatalogDelivery>,
    credential_store: Arc<dyn CredentialStore>,
    /// 与 fetcher 里那份同源，调度器用它判断绑定前通道是否可用。
    account_configuration: Option<ResolvedAccountConfiguration>,
    orchestrator: Arc<SessionOrchestrator>,
    wake: Notify,
    status: Mutex<CatalogSyncStatus>,
    refreshing: AtomicBool,
    stopping: AtomicBool,
}

pub struct CatalogSyncService {
    inner: Arc<CatalogSyncInner>,
}

struct RefreshReport {
    outcome: Option<SyncOutcome>,
    activated: bool,
}

/// 最近一次下发尝试的结果，供界面把「不能绑定」说成具体哪一种（issue #323）。
///
/// 只带稳定错误码与一个粗粒度状态，**不带** URL、响应体、Authorization 或任何
/// 凭据；这份快照会被写进 `events.jsonl`，那里不允许出现敏感值。
#[derive(Clone, Debug, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogSyncStatus {
    /// 是否已经完成过至少一次拉取尝试。false = 还没轮到（刚启动/未登录）。
    pub attempted: bool,
    /// 最近一次尝试的稳定错误码；`None` 表示上一次成功。
    pub last_error_code: Option<String>,
    /// 最近一次采纳的集合里每一份 `catalog_id@catalog_version`，全部来自**已验签**的
    /// manifest。集合下发之后没有「那一个版本」可言（docs/57 §3.3），所以这里是列表。
    pub last_accepted_catalogs: Vec<String>,
}

struct SingleFlightGuard<'a> {
    flag: &'a AtomicBool,
}

impl<'a> SingleFlightGuard<'a> {
    fn try_enter(flag: &'a AtomicBool) -> Option<Self> {
        flag.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| Self { flag })
    }
}

impl Drop for SingleFlightGuard<'_> {
    fn drop(&mut self) {
        self.flag.store(false, Ordering::Release);
    }
}

struct RefreshSchedule {
    jitter_seconds: i64,
    consecutive_failures: u32,
}

impl RefreshSchedule {
    fn new(installation_id: &str) -> Self {
        let digest = Sha256::digest(installation_id.as_bytes());
        let sample = u64::from_be_bytes(digest[..8].try_into().expect("sha256 prefix"));
        let width = (MAX_JITTER_SECONDS * 2 + 1) as u64;
        Self {
            jitter_seconds: (sample % width) as i64 - MAX_JITTER_SECONDS,
            consecutive_failures: 0,
        }
    }

    fn success_delay(&mut self) -> Duration {
        self.consecutive_failures = 0;
        Duration::from_secs((SYNC_INTERVAL_SECONDS + self.jitter_seconds) as u64)
    }

    fn failure_delay(&mut self) -> Duration {
        let exponent = self.consecutive_failures.min(5);
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        let base = (30_i64 * (1_i64 << exponent)).min(SYNC_INTERVAL_SECONDS);
        let scaled_jitter = self.jitter_seconds * base / SYNC_INTERVAL_SECONDS;
        Duration::from_secs((base + scaled_jitter).max(1) as u64)
    }
}

impl CatalogSyncService {
    pub fn start(
        orchestrator: Arc<SessionOrchestrator>,
        credential_store: Arc<dyn CredentialStore>,
        client_version: &str,
        account_configuration: Option<ResolvedAccountConfiguration>,
    ) -> anyhow::Result<Option<Arc<Self>>> {
        let Some(state_environment) =
            selected_catalog_state_environment(account_configuration.as_ref())
        else {
            tracing::info!(
                "signed catalog sync is disabled until an explicit ready account environment is available"
            );
            return Ok(None);
        };
        let base_url = production_api_base_url()?;
        let installation_id = load_or_create_client_device_id(&orchestrator.app_data_root())?;
        let fetcher = AuthenticatedCatalogFetcher {
            transport: ReqwestCatalogHttpTransport::new()?,
            base_url,
            account_configuration: account_configuration.clone(),
            credential_store: Arc::clone(&credential_store),
            client_version: client_version.to_string(),
        };
        let state = SqliteCatalogDeliveryState::open_for_environment(
            &orchestrator.app_data_root(),
            state_environment,
        )?;
        let inner = Arc::new(CatalogSyncInner {
            delivery: Mutex::new(CatalogDelivery::new(
                Box::new(fetcher),
                Box::new(state),
                RevocationList::default(),
            )),
            credential_store,
            account_configuration,
            orchestrator,
            wake: Notify::new(),
            status: Mutex::new(CatalogSyncStatus::default()),
            refreshing: AtomicBool::new(false),
            stopping: AtomicBool::new(false),
        });
        let service = Arc::new(Self {
            inner: Arc::clone(&inner),
        });

        // 启动网络请求前先重验离线缓存。坏缓存只会保持 fail-closed；worker 仍会启动，
        // 以便后续更高版本的合法 bundle 修复本地状态。
        if let Err(error) = activate_cached(&inner) {
            tracing::error!(error = %error, "cached catalog failed re-verification; runtime catalog remains fail-closed");
        }
        tauri::async_runtime::spawn(sync_loop(inner, RefreshSchedule::new(&installation_id)));
        Ok(Some(service))
    }

    pub fn wake(&self) {
        self.inner.wake.notify_one();
    }

    pub fn begin_shutdown(&self) {
        self.inner.stopping.store(true, Ordering::Release);
        self.inner.wake.notify_one();
    }
}

impl Drop for CatalogSyncService {
    fn drop(&mut self) {
        self.begin_shutdown();
    }
}

async fn sync_loop(inner: Arc<CatalogSyncInner>, mut schedule: RefreshSchedule) {
    loop {
        if inner.stopping.load(Ordering::Acquire) {
            break;
        }
        let blocking_inner = Arc::clone(&inner);
        let refresh_succeeded = match tauri::async_runtime::spawn_blocking(move || {
            refresh_once(&blocking_inner)
        })
        .await
        {
            Ok(Ok(report)) => {
                let succeeded = !matches!(report.outcome, Some(SyncOutcome::Rejected(_)));
                if let Some(outcome) = report.outcome {
                    match outcome {
                        SyncOutcome::Accepted { catalogs } => tracing::info!(
                            catalog_count = catalogs.len(),
                            catalogs = summarize_catalogs(&catalogs).join(", "),
                            activated = report.activated,
                            "signed catalog refresh accepted"
                        ),
                        SyncOutcome::NotModified => tracing::debug!(
                            activated = report.activated,
                            "signed catalog refresh not modified"
                        ),
                        SyncOutcome::Rejected(reason) => tracing::debug!(
                            reason,
                            activated = report.activated,
                            "signed catalog refresh kept the previously accepted cache"
                        ),
                    }
                }
                succeeded
            }
            Ok(Err(error)) => {
                tracing::warn!(error = %error, "signed catalog refresh failed; previously verified runtime state is unchanged");
                false
            }
            Err(error) => {
                tracing::error!(error = %error, "signed catalog blocking task failed");
                false
            }
        };
        if inner.stopping.load(Ordering::Acquire) {
            break;
        }
        let delay = if refresh_succeeded {
            schedule.success_delay()
        } else {
            schedule.failure_delay()
        };
        wait_for_wake_or_delay(&inner.wake, delay).await;
    }
}

async fn wait_for_wake_or_delay(wake: &Notify, delay: Duration) {
    let _ = tokio::time::timeout(delay, wake.notified()).await;
}

fn refresh_once(inner: &CatalogSyncInner) -> anyhow::Result<RefreshReport> {
    let _flight = SingleFlightGuard::try_enter(&inner.refreshing)
        .context("catalog refresh already in flight")?;
    // 完全没登录是正常的首启状态：仍重验/激活已有离线缓存，但不制造一次必败的网络
    // 请求。登录之后即使还没绑定也要发——绑定前通道正是为这个窗口存在的（#323）。
    let channel = available_auth_channel(
        inner.credential_store.as_ref(),
        inner.account_configuration.as_ref(),
    )?;
    let (outcome, active) = {
        let mut delivery = inner.delivery.lock();
        let outcome = channel.map(|_| delivery.refresh()).transpose();
        let active = delivery.active_catalog();
        (outcome, active)
    };
    // 归因在这里做，因为只有这里知道失败的是**哪一步**。放到调用方去看 anyhow
    // 链，一次凭据库读失败会被归成 `catalog_validation_failed`，把排障引向验签。
    record_status(inner, channel.is_some(), &outcome);
    let activated = match active {
        Ok(Some(catalog)) => inner.orchestrator.try_activate_catalog(catalog),
        Ok(None) => Ok(false),
        Err(error) => Err(error),
    };
    // 落盘在 `?` 之前：失败那次恰恰是最需要留痕的一次。
    if let Some(channel) = channel {
        record_catalog_diagnostic(
            inner,
            channel,
            &outcome,
            activated.as_ref().ok().copied().unwrap_or(false),
        );
    }
    // grant 运行时探测（docs/65 J-15）挂在同一个 blocking 循环里：启动即探、
    // 每轮下发后再探、设备绑定成功的 wake 也会带一次。目录本轮失败不拦探测——
    // 缓存里验签通过的 grant 离线也照亮，探测失败自会以 check_failed 展示。
    let activated = activated?;
    let outcome = outcome?;
    Ok(RefreshReport { outcome, activated })
}

/// 把一次下发尝试写进 `diagnostics/logs/events.jsonl`。
///
/// 记录失败只 warn，不改变刷新结果：诊断留痕不该有能力让一次成功的刷新变成失败。
fn record_catalog_diagnostic(
    inner: &CatalogSyncInner,
    channel: CatalogAuthChannel,
    outcome: &anyhow::Result<Option<SyncOutcome>>,
    activated: bool,
) {
    let auth_channel = match channel {
        CatalogAuthChannel::DeviceCredential => "device_credential",
        CatalogAuthChannel::AnnotatorSession => "annotator_session",
    };
    let (result, error_code, catalogs) = match outcome {
        Ok(Some(SyncOutcome::Accepted { catalogs })) => {
            ("accepted", None, summarize_catalogs(catalogs))
        }
        Ok(Some(SyncOutcome::NotModified)) => ("not_modified", None, Vec::new()),
        // Rejected 的 reason 是我们自己的固定字面量，没有外部输入，可以落盘。
        Ok(Some(SyncOutcome::Rejected(reason))) => {
            ("rejected", Some((*reason).to_owned()), Vec::new())
        }
        Ok(None) => return,
        Err(error) => (
            "failed",
            Some(stable_catalog_error_code(error).to_owned()),
            Vec::new(),
        ),
    };
    let level = if matches!(result, "accepted" | "not_modified") {
        "info"
    } else {
        "warning"
    };
    if let Err(error) = inner.orchestrator.diagnostics().record(
        level,
        None,
        DiagnosticCorrelation::default(),
        DiagnosticPayload::CatalogSyncCompleted {
            auth_channel,
            outcome: result,
            error_code,
            catalogs,
            activated,
        },
        false,
    ) {
        tracing::warn!(error = %error, "failed to persist catalog sync diagnostic");
    }
}

/// 把采纳的集合压成 `catalog_id@catalog_version` 一行一份。
///
/// 两处共用：日志与 `events.jsonl`。取值全部来自**已验签**的 manifest，所以可以落盘
/// ——集合外层那两个未签名的自称字段从来不到这里（见 `catalog_delivery::verify_set_entry`）。
fn summarize_catalogs(catalogs: &[AcceptedCatalog]) -> Vec<String> {
    catalogs
        .iter()
        .map(|catalog| format!("{}@{}", catalog.catalog_id, catalog.catalog_version))
        .collect()
}

fn record_status(
    inner: &CatalogSyncInner,
    attempted: bool,
    outcome: &anyhow::Result<Option<SyncOutcome>>,
) {
    let mut status = inner.status.lock();
    if !attempted {
        // 没登录不是错误状态，是还没轮到。保留上一次的错误码没有意义——它描述的是
        // 另一个凭据状态下的世界。
        status.attempted = false;
        status.last_error_code = None;
        return;
    }
    status.attempted = true;
    match outcome {
        Ok(Some(SyncOutcome::Accepted { catalogs })) => {
            status.last_error_code = None;
            status.last_accepted_catalogs = summarize_catalogs(catalogs);
        }
        Ok(Some(SyncOutcome::NotModified)) => status.last_error_code = None,
        Ok(Some(SyncOutcome::Rejected(_))) => {
            status.last_error_code = Some("catalog_rejected".to_owned());
        }
        // `then(..)` 在 can_fetch 为真时一定产出 Some，这个分支不可达；按未尝试处理
        // 而不是 unwrap，免得一个不变量的变化变成 panic。
        Ok(None) => status.attempted = false,
        Err(error) => {
            status.last_error_code = Some(stable_catalog_error_code(error).to_owned());
        }
    }
}

fn activate_cached(inner: &CatalogSyncInner) -> anyhow::Result<()> {
    let active = inner.delivery.lock().active_catalog()?;
    if let Some(catalog) = active {
        let _ = inner.orchestrator.try_activate_catalog(catalog)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::credentials::{InMemoryCredentialStore, SecretToken};

    const ACCOUNT_BASE: &str = "https://console.diogenesmodel.com";

    fn production_account_configuration() -> ResolvedAccountConfiguration {
        ResolvedAccountConfiguration {
            environment: AccountEnvironment::Production,
            base_url: format!("{ACCOUNT_BASE}/"),
        }
    }

    fn staging_account_configuration() -> ResolvedAccountConfiguration {
        ResolvedAccountConfiguration {
            environment: AccountEnvironment::Staging,
            base_url: "https://staging-console.example.com/".into(),
        }
    }

    #[test]
    fn only_a_ready_account_configuration_selects_an_activatable_catalog_partition() {
        assert_eq!(selected_catalog_state_environment(None), None);
        assert_eq!(
            selected_catalog_state_environment(Some(&staging_account_configuration())),
            Some(CatalogStateEnvironment::Staging)
        );
        assert_eq!(
            selected_catalog_state_environment(Some(&production_account_configuration())),
            Some(CatalogStateEnvironment::Production)
        );
    }

    fn store_with(device: bool, session: bool) -> InMemoryCredentialStore {
        let store = InMemoryCredentialStore::default();
        if device {
            credentials::store(
                &store,
                CredentialKind::Device,
                PRODUCTION_CREDENTIAL_SCOPE,
                &SecretToken::new("device-token".to_owned()).unwrap(),
            )
            .unwrap();
        }
        if session {
            credentials::store(
                &store,
                CredentialKind::AnnotatorSession,
                PRODUCTION_CREDENTIAL_SCOPE,
                &SecretToken::new("session-token".to_owned()).unwrap(),
            )
            .unwrap();
        }
        store
    }

    /// #323 的核心：登录之后、绑定之前必须有一条能用的通道，否则新机器永远进不来。
    #[test]
    fn a_signed_in_annotator_without_a_device_can_still_reach_the_catalog() {
        let store = store_with(false, true);
        let configuration = production_account_configuration();
        assert_eq!(
            available_auth_channel(&store, Some(&configuration)).unwrap(),
            Some(CatalogAuthChannel::AnnotatorSession)
        );
    }

    /// 绑定完成后必须切回设备凭据：会话会过期，稳态不能挂在一个短期身份上。
    #[test]
    fn the_device_credential_wins_once_enrollment_has_happened() {
        let store = store_with(true, true);
        let configuration = production_account_configuration();
        assert_eq!(
            available_auth_channel(&store, Some(&configuration)).unwrap(),
            Some(CatalogAuthChannel::DeviceCredential)
        );
        // 缺省、非法或启动降级的账号配置都必须 fail closed；遗留 production
        // device 凭据不能把“没有显式选择”重新解释成 production。
        assert_eq!(available_auth_channel(&store, None).unwrap(), None);
    }

    #[test]
    fn no_credentials_means_no_request_rather_than_a_guaranteed_failure() {
        assert_eq!(
            available_auth_channel(
                &store_with(false, false),
                Some(&production_account_configuration())
            )
            .unwrap(),
            None
        );
        // 有会话但账号面被显式关掉：没有可打的绑定前地址，同样不发请求。
        assert_eq!(
            available_auth_channel(&store_with(false, true), None).unwrap(),
            None
        );
    }

    #[test]
    fn staging_account_configuration_never_uses_a_production_device_credential() {
        let store = store_with(true, false);
        let configuration = staging_account_configuration();
        assert_eq!(
            available_auth_channel(&store, Some(&configuration)).unwrap(),
            None
        );

        credentials::store(
            &store,
            CredentialKind::AnnotatorSession,
            configuration.environment.credential_scope(),
            &SecretToken::new("staging-session-token".to_owned()).unwrap(),
        )
        .unwrap();
        assert_eq!(
            available_auth_channel(&store, Some(&configuration)).unwrap(),
            Some(CatalogAuthChannel::AnnotatorSession)
        );
    }

    #[test]
    fn installation_jitter_is_stable_and_bounded() {
        let mut first = RefreshSchedule::new("installation-a");
        let mut second = RefreshSchedule::new("installation-a");
        let delay = first.success_delay();
        assert_eq!(delay, second.success_delay());
        assert!((540..=660).contains(&delay.as_secs()));
    }

    #[test]
    fn failures_back_off_from_thirty_seconds_to_ten_minutes() {
        let mut schedule = RefreshSchedule {
            jitter_seconds: 0,
            consecutive_failures: 0,
        };
        let delays = (0..7)
            .map(|_| schedule.failure_delay().as_secs())
            .collect::<Vec<_>>();
        assert_eq!(delays, vec![30, 60, 120, 240, 480, 600, 600]);
        assert_eq!(schedule.success_delay(), Duration::from_secs(600));
        assert_eq!(schedule.failure_delay(), Duration::from_secs(30));
    }

    #[test]
    fn refresh_guard_is_single_flight_and_releases_on_drop() {
        let flag = AtomicBool::new(false);
        let first = SingleFlightGuard::try_enter(&flag).expect("first refresh enters");
        assert!(SingleFlightGuard::try_enter(&flag).is_none());
        drop(first);
        assert!(SingleFlightGuard::try_enter(&flag).is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn fake_time_waits_for_schedule_or_immediate_wake() {
        let wake = Arc::new(Notify::new());
        let waiting_wake = Arc::clone(&wake);
        let scheduled = tokio::spawn(async move {
            wait_for_wake_or_delay(&waiting_wake, Duration::from_secs(600)).await;
        });
        tokio::task::yield_now().await;
        assert!(!scheduled.is_finished());
        tokio::time::advance(Duration::from_secs(599)).await;
        tokio::task::yield_now().await;
        assert!(!scheduled.is_finished());
        tokio::time::advance(Duration::from_secs(1)).await;
        scheduled.await.unwrap();

        let waiting_wake = Arc::clone(&wake);
        let immediate = tokio::spawn(async move {
            wait_for_wake_or_delay(&waiting_wake, Duration::from_secs(600)).await;
        });
        tokio::task::yield_now().await;
        wake.notify_one();
        immediate.await.unwrap();
    }
}

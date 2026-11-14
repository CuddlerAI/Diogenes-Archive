use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::Mutex;
use tauri::tray::TrayIcon;
use tauri::{AppHandle, Wry};

use crate::services::account::{
    AccountAvailability, AccountConfigurationFailure, AccountConfigurationResolution,
    AccountService, account_configuration_from_env, record_account_startup_failure,
};
#[cfg(not(feature = "local-catalog-only"))]
use crate::services::catalog_sync::CatalogSyncService;
use crate::services::client_events::{
    ClientEventsConfigurationFailure, ClientEventsService, client_events_configuration_from_env,
    record_client_events_startup_failure,
};
use crate::services::consent_watch::ConsentWatcher;
use crate::services::credentials::{self, CredentialStore};
use crate::services::orchestrator::SessionOrchestrator;
use crate::services::updater::UpdateChecker;
use crate::services::uploader::UploaderService;
use crate::services::view_scale::ViewScaleService;

pub struct AppState {
    pub orchestrator: Arc<SessionOrchestrator>,
    pub view_scale: Arc<ViewScaleService>,
    pub credential_store: Arc<dyn CredentialStore>,
    pub updater: Arc<UpdateChecker>,
    /// 条款变更观察器（docs/108 §3.6）。只读、只提示，采集路径从不读它。
    pub consent_watcher: Arc<ConsentWatcher>,
    /// 账号服务初始化失败时为 None——账号面不可用不能影响本地采集。
    pub account: Option<Arc<AccountService>>,
    /// 与 `account` 分开保存，才能让 WebView 区分真正未配置与配置非法。
    pub account_availability: AccountAvailability,
    uploader: Option<Arc<UploaderService>>,
    client_events: Option<Arc<ClientEventsService>>,
    #[cfg(not(feature = "local-catalog-only"))]
    catalog_sync: Option<Arc<CatalogSyncService>>,
    tray: Mutex<Option<TrayIcon<Wry>>>,
    quit_requested: AtomicBool,
}

impl AppState {
    pub fn new(app: AppHandle) -> anyhow::Result<Self> {
        let current_version = app.package_info().version.to_string();
        let orchestrator = Arc::new(SessionOrchestrator::new(app.clone())?);
        // 视角标定服务（docs/45/46）：无后台线程，探针只在对表会话内存活。
        let view_scale = Arc::new(ViewScaleService::new(app, Arc::clone(&orchestrator)));
        let credential_store = credentials::platform_store();
        let updater = Arc::new(UpdateChecker::new(
            current_version.clone(),
            orchestrator.diagnostics(),
        ));
        let consent_watcher = Arc::new(ConsentWatcher::new());
        // 账号环境与 base 只在这里解析一次，随后把同一份规范化配置传给所有账号同源
        // 消费者。任何缺省、非法或矛盾值都只关闭账号面，绝不改写为 production。
        let account_resolution = account_configuration_from_env();
        let mut account_availability = account_resolution.availability();
        let mut active_account_configuration = None;
        let account = match account_resolution {
            AccountConfigurationResolution::Unconfigured => None,
            AccountConfigurationResolution::Invalid(failure) => {
                record_account_startup_failure(orchestrator.diagnostics().as_ref(), &failure);
                tracing::error!(code = %failure.code, reason = %failure.reason, "account configuration is invalid; sign-in is disabled");
                None
            }
            AccountConfigurationResolution::Ready(configuration) => {
                match AccountService::from_configuration(
                    Arc::clone(&credential_store),
                    &configuration,
                    &current_version,
                    orchestrator.diagnostics(),
                ) {
                    Ok(service) => {
                        active_account_configuration = Some(configuration);
                        Some(Arc::new(service))
                    }
                    Err(error) => {
                        let failure = AccountConfigurationFailure::service_start(&error);
                        account_availability = failure.availability();
                        record_account_startup_failure(
                            orchestrator.diagnostics().as_ref(),
                            &failure,
                        );
                        tracing::error!(code = %failure.code, reason = %failure.reason, "account service is unavailable; sign-in is disabled");
                        None
                    }
                }
            }
        };
        // worker 每轮把上传面推进 CollectorSnapshot（docs/65 J-29）。启动失败时
        // 没有任何人把 uploaderRunning 翻成 true，快照因此保持默认的
        // 「上传器未启动」——采集页的常驻降级徽章正是读它（J-30）。
        let upload_status_orchestrator = Arc::clone(&orchestrator);
        let uploader = match UploaderService::start(
            orchestrator.upload_journal(),
            orchestrator.raw_output_root(),
            orchestrator.app_data_root(),
            Arc::clone(&credential_store),
            orchestrator.diagnostics(),
            Arc::new(move |status| upload_status_orchestrator.set_upload_status(status)),
        ) {
            Ok(service) => Some(service),
            Err(error) => {
                // Upload is an eventually-consistent side effect. A bad endpoint
                // override or worker spawn failure must never disable capture.
                tracing::error!(error = %error, "R2 uploader is disabled; durable jobs remain local");
                None
            }
        };
        // 行为遥测发送器（docs/23 §4.2）。与 uploader 同一纪律：启动失败只
        // log，绝不影响采集。
        let client_events_environment = active_account_configuration
            .as_ref()
            .map(|configuration| configuration.environment);
        let client_events_configuration = match client_events_configuration_from_env(
            active_account_configuration.as_ref(),
        ) {
            Ok(configuration) => configuration,
            Err(failure) => {
                record_client_events_startup_failure(orchestrator.diagnostics().as_ref(), &failure);
                tracing::error!(code = %failure.code, reason = %failure.reason, "client events configuration is invalid; telemetry stays local");
                None
            }
        };
        let client_events = match ClientEventsService::start(
            orchestrator.upload_journal(),
            Arc::clone(&credential_store),
            client_events_environment,
            client_events_configuration,
        ) {
            Ok(service) => {
                let recorder = service.recorder();
                // A 类事件走诊断 sink（record() 已脱敏），B 类 preflight 走
                // orchestrator 的直报点。
                let sink_recorder = Arc::clone(&recorder);
                orchestrator
                    .diagnostics()
                    .set_event_sink(Arc::new(move |event| {
                        sink_recorder.on_diagnostic_event(event)
                    }));
                orchestrator.set_client_event_recorder(recorder);
                Some(service)
            }
            Err(error) => {
                let failure = ClientEventsConfigurationFailure {
                    code: "client_events_service_start_failed".into(),
                    reason: format!("{error:#}"),
                };
                record_client_events_startup_failure(orchestrator.diagnostics().as_ref(), &failure);
                tracing::error!(error = %error, "client events sender is disabled; telemetry stays local");
                None
            }
        };
        #[cfg(feature = "local-catalog-only")]
        tracing::info!(
            "signed catalog sync is disabled by the local-catalog-only build feature; bundled DEV catalog remains authoritative"
        );
        #[cfg(not(feature = "local-catalog-only"))]
        let catalog_sync = match CatalogSyncService::start(
            Arc::clone(&orchestrator),
            Arc::clone(&credential_store),
            &current_version,
            active_account_configuration,
        ) {
            Ok(service) => service,
            Err(error) => {
                // release 的内置 catalog 为空，所以 wiring 失败仍保持不能录制；
                // debug dev catalog 则继续服务本地 non-trainable 测试。
                tracing::error!(error = %error, "signed catalog sync is disabled; bundled catalog remains authoritative");
                None
            }
        };
        Ok(Self {
            orchestrator,
            view_scale,
            credential_store,
            updater,
            consent_watcher,
            account,
            account_availability,
            uploader,
            client_events,
            #[cfg(not(feature = "local-catalog-only"))]
            catalog_sync,
            tray: Mutex::new(None),
            quit_requested: AtomicBool::new(false),
        })
    }

    pub fn wake_client_events(&self) {
        if let Some(client_events) = &self.client_events {
            client_events.wake();
        }
    }

    pub fn begin_client_events_shutdown(&self) {
        if let Some(client_events) = &self.client_events {
            client_events.begin_shutdown();
        }
    }

    pub fn client_events_shutdown_complete(&self) -> bool {
        self.client_events
            .as_ref()
            .is_none_or(|client_events| client_events.shutdown_complete())
    }

    pub fn wake_catalog_sync(&self) {
        #[cfg(not(feature = "local-catalog-only"))]
        if let Some(catalog_sync) = &self.catalog_sync {
            catalog_sync.wake();
        }
    }

    pub fn begin_catalog_sync_shutdown(&self) {
        #[cfg(not(feature = "local-catalog-only"))]
        if let Some(catalog_sync) = &self.catalog_sync {
            catalog_sync.begin_shutdown();
        }
    }

    pub fn set_tray(&self, tray: TrayIcon<Wry>) {
        self.tray.lock().replace(tray);
    }

    pub fn wake_uploader(&self) {
        if let Some(uploader) = &self.uploader {
            uploader.wake();
        }
    }

    pub fn begin_uploader_shutdown(&self) {
        if let Some(uploader) = &self.uploader {
            uploader.begin_shutdown();
        }
    }

    pub fn uploader_shutdown_complete(&self) -> bool {
        self.uploader
            .as_ref()
            .is_none_or(|uploader| uploader.shutdown_complete())
    }

    pub fn request_quit(&self) -> bool {
        !self.quit_requested.swap(true, Ordering::AcqRel)
    }

    pub fn quit_requested(&self) -> bool {
        self.quit_requested.load(Ordering::Acquire)
    }

    pub fn cancel_quit(&self) {
        self.quit_requested.store(false, Ordering::Release);
    }
}

//! 标注员账号：注册、邮箱验证、登录、登出与设备 enroll（`docs/11` §7，服务端 `/app/v1`）。
//!
//! 与 [`crate::services::uploader`] 的 `HttpTransport` **刻意分开**，不是重复：
//!
//! - **不同主机**。uploader 打的是摄取面（`api.diogenesmodel.com`）；`/app/v1` 在
//!   console-api 上，显式 production 配置经 `console.diogenesmodel.com`。（此处原先写的是「必须
//!   Cloud Run 直连，因为过不去 Cloudflare Access」——生产是 `password_only`
//!   登录模式，console 域名前面没有 Access 拦截，`/app/v1/*` 实测可直接访问。）
//! - **不同凭据**。uploader 每个请求都带 device token；这里注册/验证/登录**没有**任何
//!   凭据，登出与设备 enroll/list 带的是 annotator 会话 token。
//! - **不同失败语义**。uploader 的错误驱动的是 durable 重试队列（可重试/不可重试
//!   二元）；这里的错误要变成给人看的一句话。
//!
//! 复用的是**做法**而不是代码：base URL 的成形规则（强制 HTTPS、拒绝 query/fragment、
//! 末尾补斜杠后 join）与 uploader 逐条一致，因为那些规则防的是同一类错误。
//!
//! 本模块不碰采集链路。账号配置缺失或非法时账号面 fail closed，也不改写已有本地 artifact；
//! 新采集能否开始仍由当前显式环境下已验签的目录决定，不能为了“可用”猜成 production。

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::platform::windows::input_devices::InputDevicesSnapshot;
use crate::services::credentials::{self, CredentialKind, CredentialStore, SecretToken};
use crate::services::diagnostics::{DiagnosticCorrelation, DiagnosticPayload, DiagnosticService};
use capture_core::device_snapshot::{DisplaySnapshot, GpuSnapshot};

/// 账号面必须同时显式选择环境和 base URL。只有两项都缺席才是「未配置」；只出现
/// 其中一项、值非法或环境与地址冲突都属于「配置无效」，绝不回落 production。
pub const ACCOUNT_ENVIRONMENT_ENV: &str = "DATAMINER_ACCOUNT_ENVIRONMENT";
const ACCOUNT_API_BASE_URL_ENV: &str = "DATAMINER_ACCOUNT_API_BASE_URL";
const OBSOLETE_ACCOUNT_BASE_URL_ENV: &str = "DATAMINER_ACCOUNT_BASE_URL";
const EMBEDDED_ACCOUNT_ENVIRONMENT: Option<&str> =
    option_env!("DATAMINER_EMBEDDED_ACCOUNT_ENVIRONMENT");
const EMBEDDED_ACCOUNT_API_BASE_URL: Option<&str> =
    option_env!("DATAMINER_EMBEDDED_ACCOUNT_API_BASE_URL");

/// production 的唯一合法账号 origin。它只用于校验显式 production 配置，绝不是默认值。
pub(crate) const CANONICAL_PRODUCTION_ACCOUNT_API_BASE_URL: &str =
    "https://console.diogenesmodel.com/";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AccountEnvironment {
    Staging,
    Production,
}

impl AccountEnvironment {
    #[must_use]
    pub const fn credential_scope(self) -> &'static str {
        match self {
            Self::Staging => "staging",
            Self::Production => "production",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedAccountConfiguration {
    pub environment: AccountEnvironment,
    pub base_url: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountConfigurationFailure {
    pub code: String,
    /// 给账号页看的安全、稳定文案；不回显原始配置值。
    pub message: String,
    /// 给本地诊断导出的真实安全原因；同样不包含原始 URL。
    pub reason: String,
}

impl AccountConfigurationFailure {
    fn new(code: &str, message: &str, reason: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            reason: reason.into(),
        }
    }

    pub fn service_start(error: &anyhow::Error) -> Self {
        Self::new(
            "account_service_start_failed",
            "账号服务启动失败，请导出诊断并联系管理员。账号连接与账号环境目录均已停止。",
            format!("{error:#}"),
        )
    }

    #[must_use]
    pub fn availability(&self) -> AccountAvailability {
        AccountAvailability::Invalid {
            code: self.code.clone(),
            message: self.message.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AccountConfigurationResolution {
    Unconfigured,
    Invalid(AccountConfigurationFailure),
    Ready(ResolvedAccountConfiguration),
}

impl AccountConfigurationResolution {
    #[must_use]
    pub fn availability(&self) -> AccountAvailability {
        match self {
            Self::Unconfigured => AccountAvailability::Unconfigured,
            Self::Invalid(failure) => failure.availability(),
            Self::Ready(configuration) => AccountAvailability::Ready {
                environment: configuration.environment,
            },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum AccountAvailability {
    Unconfigured,
    Invalid { code: String, message: String },
    Ready { environment: AccountEnvironment },
}

/// 比上传短：这些请求都在人等着的界面后面。argon2 校验在服务端要几百毫秒，
/// 30 秒足够，再长只是让卡住的界面卡得更久。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const CLIENT_DEVICE_ID_FILE: &str = "client-device-id";

/// 服务端当前生效的条款。客户端**不能**自己编版本号：注册时
/// `acceptedPolicyVersion` 与服务端不符会被直接拒绝（§7.1），而正文要展示给用户看
/// ——对一份读不到的文档表示同意，不算同意。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Terms {
    pub policy_version: String,
    pub purpose: String,
    pub terms_hash: String,
    pub body: String,
}

/// 当前 consent 与服务端在服务的条款是否一致（服务端 `GET /app/v1/consent`）。
///
/// `matches_current_terms` 比对的是 hash 而非版本串：同一个版本号配不同字节就是
/// 另一份文书，而那正是最值得发现的漂移。
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsentState {
    pub consent: Option<SignedConsent>,
    pub current_policy_version: String,
    pub current_allows_trainable_use: bool,
    pub matches_current_terms: bool,
    /// 补签会**新增**哪些授权范围（docs/108 §3.3）。这是提示里唯一有信息量的东西：
    /// 「你签的是 A、现在是 B」逼采集员自己去 diff 两份英文法律文本，而
    /// 「补签后新增：游戏进程音频」是一句话就能判断要不要签的。
    ///
    /// 老服务端不返回这两个字段，缺省即空/假——客户端因此可以先于服务端发版，
    /// 只是横幅不会亮。
    #[serde(default)]
    pub pending_scopes: Vec<String>,
    #[serde(default)]
    pub current_scopes: Vec<String>,
    #[serde(default)]
    pub resign_required: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SignedConsent {
    pub policy_version: String,
    pub allows_trainable_use: bool,
    pub accepted_at: Option<String>,
    #[serde(default)]
    pub scopes: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SignupChallenge {
    pub challenge_id: String,
    pub expires_in_seconds: u64,
}

/// 登录态。**没有 `Unknown` 之外的持久化**：token 在 Credential Manager 里，
/// 谁在登录由服务端说了算。
///
/// `Unknown` 不是「未登录」——它是「本地有 token 但还没和服务端对过账」。两者必须
/// 分开：把离线启动显示成「未登录」会诱导用户重新注册，而他其实已经有账号了。
#[derive(Clone, Debug, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum AccountIdentity {
    Unknown,
    SignedIn { annotator_id: String },
    SignedOut,
}

/// 给前端看的错误。稳定的机读码 + 一句中文，**不透传服务端文案**；只保留
/// requestId 作为无敏感内容的关联编号，用户截图后可以与服务端日志对账。
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountError {
    pub code: String,
    pub message: String,
    pub retryable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_seconds: Option<u64>,
    /// 保留服务端原始稳定码供本地诊断使用；WebView 只消费映射后的本地 `code`。
    #[serde(skip_serializing)]
    server_code: Option<String>,
}

impl AccountError {
    fn new(code: &str, message: &str, retryable: bool) -> Self {
        Self {
            code: code.to_string(),
            message: message.to_string(),
            retryable,
            http_status: None,
            request_id: None,
            retry_after_seconds: None,
            server_code: None,
        }
    }

    /// 服务端错误码 → 用户看得懂的一句话。未知码统一收口，绝不回显服务端文本。
    fn from_server(code: &str) -> Self {
        match code {
            "invite_invalid" => {
                Self::new(code, "邀请码无效或已被使用。请向管理员确认后重试。", false)
            }
            "terms_out_of_date" => {
                Self::new(code, "条款已更新，请重新打开注册页阅读最新版本。", false)
            }
            "invalid_credentials" => {
                Self::new(code, "邮箱或密码不正确，也可能是验证码已失效。", false)
            }
            "signup_verification_failed" => Self::new(
                code,
                "验证码不正确、已过期或已被新验证码替代，请使用最新邮件中的验证码。",
                false,
            ),
            "session_required" => Self::new(code, "登录已过期，请重新登录。", false),
            "rate_limited" => Self::new(code, "尝试过于频繁，请稍后再试。", true),
            "device_unavailable" => Self::new(code, "这台设备不属于当前账号，无法操作。", false),
            "invalid_request" => Self::new(code, "填写的内容不符合要求，请检查后重试。", false),
            "weak_password" => Self::new(code, "密码不能包含你的邮箱地址。", false),
            "internal_error" => Self::new(code, "账号服务暂时异常，请稍后重试。", true),
            _ => Self::new("account_request_failed", "请求失败，请稍后重试。", true),
        }
    }

    fn from_response(
        code: &str,
        http_status: u16,
        request_id: Option<String>,
        retryable: Option<bool>,
        retry_after_seconds: Option<u64>,
    ) -> Self {
        let mut error = if code.is_empty() && http_status >= 500 {
            Self::unavailable()
        } else {
            Self::from_server(code)
        };
        error.server_code = (!code.is_empty()).then(|| code.to_owned());
        error = error.with_response_metadata(http_status, request_id, retry_after_seconds);
        if let Some(retryable) = retryable {
            error.retryable = retryable;
        }
        error
    }

    /// 兼容旧版服务端：注册验证码失败曾与登录失败共用 `invalid_credentials`。
    /// 只改给界面看的本地错误码和文案，诊断日志仍保留服务端原始码。
    fn for_signup_verification(mut self) -> Self {
        if self.code == "invalid_credentials" {
            self.code = "signup_verification_failed".into();
            self.message =
                "验证码不正确、已过期或已被新验证码替代，请使用最新邮件中的验证码。".into();
        }
        self
    }

    fn with_response_metadata(
        mut self,
        http_status: u16,
        request_id: Option<String>,
        retry_after_seconds: Option<u64>,
    ) -> Self {
        self.http_status = Some(http_status);
        self.request_id = request_id;
        self.retry_after_seconds = retry_after_seconds;
        self
    }

    fn unavailable() -> Self {
        Self::new(
            "account_api_unavailable",
            "连接不上账号服务，请检查网络后重试。",
            true,
        )
    }

    /// 地址通了、但对面不是账号服务。
    ///
    /// 真实踩到过：console 的 Cloudflare Worker 没有把 `/app/` 转发到 API，于是
    /// 每个账号请求都拿到 200 + 控制台 SPA 的 HTML。旧代码把它当成「服务端返回的
    /// 内容无法解析」，读起来像是服务端出了个偶发的 bug，而实际是**地址不对**——
    /// 两者的下一步完全不同，所以它值一个自己的错误码。
    fn not_account_service() -> Self {
        Self::new(
            "account_api_not_account_service",
            "这个地址返回的不是账号服务的响应（多半是网页而不是接口），请联系管理员确认账号服务地址。",
            false,
        )
    }

    pub fn unconfigured() -> Self {
        Self::new(
            "account_api_unconfigured",
            "这个构建没有配置账号服务，账号功能不可用。",
            false,
        )
    }

    pub fn configuration_invalid(code: &str, message: &str) -> Self {
        Self::new(code, message, false)
    }

    pub(crate) fn local(code: &str, message: &str, retryable: bool) -> Self {
        Self::new(code, message, retryable)
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceEnrollmentRequest {
    pub client_device_id: String,
    pub windows_build: String,
    pub gpu_pci_id: String,
    pub gpu_driver: String,
    pub client_version: String,
    pub display_snapshot: DisplaySnapshot,
    pub input_devices_snapshot: InputDevicesSnapshot,
}

impl DeviceEnrollmentRequest {
    pub fn new(
        client_device_id: String,
        windows_build: String,
        gpu: GpuSnapshot,
        client_version: String,
        display_snapshot: DisplaySnapshot,
        input_devices_snapshot: InputDevicesSnapshot,
    ) -> Self {
        Self {
            client_device_id,
            windows_build,
            gpu_pci_id: gpu.pci_id,
            gpu_driver: gpu.driver_version,
            client_version,
            display_snapshot,
            input_devices_snapshot,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnrollGrant {
    pub device_id: String,
    pub client_device_id: String,
    pub token: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EnrollOutcome {
    pub device_id: String,
    pub client_device_id: String,
    pub credential_issued: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountDevice {
    pub device_id: String,
    pub client_device_id: String,
    pub status: String,
    pub client_version: String,
    pub last_seen_at: Option<String>,
    pub created_at: String,
}

/// 联络资料（docs/34 §5.4）。全部可空：资料是运营协调用的联络事实，
/// 不是身份，没有资料是合法状态。
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountProfile {
    pub display_name: Option<String>,
    pub phone: Option<String>,
    pub im_kind: Option<String>,
    pub im_handle: Option<String>,
    pub region: Option<String>,
    pub updated_at: String,
}

/// 资料更新。服务端 PUT 是整体替换：**缺席的字段会被清空**，所以序列化时
/// 跳过 `None` 恰好就是「清掉它」的表达，而不是「保持不动」。IM 渠道与帐号
/// 必须成对（服务端约束）；命令层在提交前收拢这一点。
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileUpdate {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phone: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub im_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub im_handle: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
}

/// 传输缝。存在的唯一理由是让编排逻辑（状态转移、凭据落盘、错误映射）能被单测覆盖
/// 而不需要真网络——与 `catalog_delivery` 的做法一致。
pub trait AccountApi: Send + Sync {
    fn terms(&self) -> Result<Terms, AccountError>;
    fn consent_state(&self, session: &SecretToken) -> Result<ConsentState, AccountError>;
    fn renew_consent(
        &self,
        session: &SecretToken,
        body: &serde_json::Value,
    ) -> Result<(), AccountError>;
    fn signup(&self, body: &serde_json::Value) -> Result<SignupChallenge, AccountError>;
    fn verify_signup(&self, body: &serde_json::Value) -> Result<(), AccountError>;
    fn resend_signup_code(&self, body: &serde_json::Value)
    -> Result<SignupChallenge, AccountError>;
    fn request_password_reset(
        &self,
        body: &serde_json::Value,
    ) -> Result<SignupChallenge, AccountError>;
    fn confirm_password_reset(&self, body: &serde_json::Value) -> Result<(), AccountError>;
    fn login(&self, body: &serde_json::Value) -> Result<SessionGrant, AccountError>;
    fn logout(&self, session: &SecretToken) -> Result<(), AccountError>;
    fn session(&self, session: &SecretToken) -> Result<String, AccountError>;
    fn enroll_device(
        &self,
        session: &SecretToken,
        body: &DeviceEnrollmentRequest,
    ) -> Result<EnrollGrant, AccountError>;
    fn list_devices(&self, session: &SecretToken) -> Result<Vec<AccountDevice>, AccountError>;
    fn change_password(
        &self,
        session: &SecretToken,
        body: &serde_json::Value,
    ) -> Result<(), AccountError>;
    fn get_profile(&self, session: &SecretToken) -> Result<Option<AccountProfile>, AccountError>;
    fn update_profile(
        &self,
        session: &SecretToken,
        body: &ProfileUpdate,
    ) -> Result<Option<AccountProfile>, AccountError>;
}

/// 登录响应。服务端还会返回 `expiresInSeconds`，这里刻意不解析——目前没有任何东西
/// 消费它，而会话是否有效由服务端在下一次请求时说了算（401 即清）。等到界面真的要
/// 提前提醒过期时再加，不留一个没人读的字段冒充能力。
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionGrant {
    pub token: String,
}

/// 账号编排：把 HTTP 结果落到凭据存储与内存态上。
pub struct AccountService {
    api: Arc<dyn AccountApi>,
    store: Arc<dyn CredentialStore>,
    environment: AccountEnvironment,
    identity: Mutex<AccountIdentity>,
    diagnostics: Option<Arc<DiagnosticService>>,
}

impl AccountService {
    pub fn new(
        api: Arc<dyn AccountApi>,
        store: Arc<dyn CredentialStore>,
        environment: AccountEnvironment,
    ) -> Self {
        Self {
            api,
            store,
            environment,
            identity: Mutex::new(AccountIdentity::Unknown),
            diagnostics: None,
        }
    }

    pub fn from_configuration(
        store: Arc<dyn CredentialStore>,
        configuration: &ResolvedAccountConfiguration,
        client_version: &str,
        diagnostics: Arc<DiagnosticService>,
    ) -> anyhow::Result<Self> {
        let api = HttpAccountApi::new(&configuration.base_url, client_version)?;
        let mut service = Self::new(Arc::new(api), store, configuration.environment);
        service.diagnostics = Some(diagnostics);
        Ok(service)
    }

    #[must_use]
    pub const fn credential_scope(&self) -> &'static str {
        self.environment.credential_scope()
    }

    #[must_use]
    pub const fn environment(&self) -> AccountEnvironment {
        self.environment
    }

    fn observe_api<T>(
        &self,
        operation: &str,
        method: &str,
        path: &str,
        result: Result<T, AccountError>,
    ) -> Result<T, AccountError> {
        if let (Some(diagnostics), Err(error)) = (&self.diagnostics, &result) {
            let server_code = error
                .server_code
                .clone()
                .unwrap_or_else(|| error.code.clone());
            if let Err(record_error) = diagnostics.record(
                "warning",
                None,
                DiagnosticCorrelation::default(),
                DiagnosticPayload::AccountApiRequestFailed {
                    operation: operation.into(),
                    method: method.into(),
                    path: path.into(),
                    http_status: error.http_status,
                    server_code,
                    request_id: error.request_id.clone(),
                    retryable: error.retryable,
                    environment: self.environment.credential_scope().into(),
                },
                false,
            ) {
                tracing::warn!(error = %record_error, operation, "failed to persist account API diagnostic");
            }
        }
        result
    }

    pub fn identity(&self) -> AccountIdentity {
        self.identity.lock().clone()
    }

    pub fn terms(&self) -> Result<Terms, AccountError> {
        self.observe_api("terms", "GET", "/app/v1/terms", self.api.terms())
    }

    pub fn signup(
        &self,
        invite_code: &str,
        email: &str,
        password: &str,
        alias: &str,
        accepted_policy_version: &str,
    ) -> Result<SignupChallenge, AccountError> {
        let result = self.api.signup(&serde_json::json!({
            "inviteCode": invite_code,
            "email": email,
            "password": password,
            "alias": alias,
            "acceptedPolicyVersion": accepted_policy_version,
        }));
        self.observe_api("signup", "POST", "/app/v1/signup", result)
    }

    pub fn verify_signup(&self, challenge_id: &str, code: &str) -> Result<(), AccountError> {
        let result = self
            .api
            .verify_signup(&serde_json::json!({
                "challengeId": challenge_id,
                "code": code,
            }))
            .map_err(AccountError::for_signup_verification);
        self.observe_api("signup_verify", "POST", "/app/v1/signup/verify", result)
    }

    pub fn resend_signup_code(&self, challenge_id: &str) -> Result<SignupChallenge, AccountError> {
        let result = self
            .api
            .resend_signup_code(&serde_json::json!({ "challengeId": challenge_id }));
        self.observe_api("signup_resend", "POST", "/app/v1/signup/resend", result)
    }

    /// 找回密码第一步。服务端对「有账号」与「没账号」返回同一个形状，
    /// 所以这里也**不能**把结果解释成「该邮箱已注册」——界面只说“如果这个邮箱
    /// 有账号，验证码已发出”。
    pub fn request_password_reset(&self, email: &str) -> Result<SignupChallenge, AccountError> {
        let result = self
            .api
            .request_password_reset(&serde_json::json!({ "email": email }));
        self.observe_api(
            "password_reset_request",
            "POST",
            "/app/v1/password-reset/request",
            result,
        )
    }

    /// 找回密码第二步。成功后服务端会吊销该账号的全部会话，因此本地那份
    /// annotator 会话凭据必然已失效——顺手清掉，避免界面停在「已登录」而
    /// 下一个请求才 401。设备凭据是另一把钥匙，服务端没动，这里也不动。
    pub fn confirm_password_reset(
        &self,
        challenge_id: &str,
        code: &str,
        password: &str,
    ) -> Result<(), AccountError> {
        let result = self.api.confirm_password_reset(&serde_json::json!({
            "challengeId": challenge_id,
            "code": code,
            "password": password,
        }));
        self.observe_api(
            "password_reset_confirm",
            "POST",
            "/app/v1/password-reset/confirm",
            result,
        )?;
        // 只清本地：服务端已经吊销了全部会话，再打一次 /logout 只会拿 401。
        let _ = credentials::remove(
            self.store.as_ref(),
            CredentialKind::AnnotatorSession,
            self.environment.credential_scope(),
        );
        *self.identity.lock() = AccountIdentity::SignedOut;
        Ok(())
    }

    /// 登录成功后把 token 写进 Credential Manager。写失败视为登录失败：
    /// 一个存不下来的会话下次启动就没了，报成功等于骗人。
    pub fn login(&self, email: &str, password: &str) -> Result<AccountIdentity, AccountError> {
        let result = self.api.login(&serde_json::json!({
            "email": email,
            "password": password,
        }));
        let grant = self.observe_api("login", "POST", "/app/v1/login", result)?;
        let token = SecretToken::new(grant.token).map_err(|_| {
            AccountError::new(
                "account_response_invalid",
                "服务端返回的会话凭据格式异常。",
                false,
            )
        })?;
        credentials::store(
            self.store.as_ref(),
            CredentialKind::AnnotatorSession,
            self.environment.credential_scope(),
            &token,
        )
        .map_err(|_| {
            AccountError::new(
                "credential_store_failed",
                "无法安全保存登录状态，请重试。",
                true,
            )
        })?;
        self.refresh_identity()
    }

    /// 拿本地 token 问服务端「我是谁」。
    ///
    /// 三态转移都在这里：401 说明 token 死了，**清掉它**并转 `SignedOut`；网络失败
    /// 停在 `Unknown` 并**保留 token**——离线不是登出，把用户的会话在断网时丢掉是
    /// 最容易惹恼人的一种 bug。
    pub fn refresh_identity(&self) -> Result<AccountIdentity, AccountError> {
        let Some(token) = credentials::load(
            self.store.as_ref(),
            CredentialKind::AnnotatorSession,
            self.environment.credential_scope(),
        )
        .ok()
        .flatten() else {
            *self.identity.lock() = AccountIdentity::SignedOut;
            return Ok(AccountIdentity::SignedOut);
        };

        let result = self.observe_api(
            "session",
            "GET",
            "/app/v1/session",
            self.api.session(&token),
        );
        match result {
            Ok(annotator_id) => {
                let identity = AccountIdentity::SignedIn { annotator_id };
                *self.identity.lock() = identity.clone();
                Ok(identity)
            }
            Err(error) if error.code == "session_required" => {
                let _ = credentials::remove(
                    self.store.as_ref(),
                    CredentialKind::AnnotatorSession,
                    self.environment.credential_scope(),
                );
                *self.identity.lock() = AccountIdentity::SignedOut;
                Ok(AccountIdentity::SignedOut)
            }
            Err(_) => {
                *self.identity.lock() = AccountIdentity::Unknown;
                Ok(AccountIdentity::Unknown)
            }
        }
    }

    /// 登出。**无论服务端怎么回都删本地 token**：网络故障不能让用户卡在
    /// 「已登录」上——那会让共用电脑的场景下没人能真正退出。
    ///
    /// 刻意**不动设备凭据**：那是这台机器的身份，不是这个人的。删掉它会让
    /// 排队中的上传全部停摆，而用户只是想退出账号。
    pub fn logout(&self) -> AccountIdentity {
        if let Some(token) = credentials::load(
            self.store.as_ref(),
            CredentialKind::AnnotatorSession,
            self.environment.credential_scope(),
        )
        .ok()
        .flatten()
        {
            let result = self.api.logout(&token);
            let _ = self.observe_api("logout", "POST", "/app/v1/logout", result);
        }
        let _ = credentials::remove(
            self.store.as_ref(),
            CredentialKind::AnnotatorSession,
            self.environment.credential_scope(),
        );
        *self.identity.lock() = AccountIdentity::SignedOut;
        AccountIdentity::SignedOut
    }

    pub fn enroll_device(
        &self,
        snapshot: DeviceEnrollmentRequest,
    ) -> Result<EnrollOutcome, AccountError> {
        let session = self.session_token()?;
        let result = self.api.enroll_device(&session, &snapshot);
        let grant = self.observe_api("device_enroll", "POST", "/app/v1/devices/enroll", result)?;
        let credential_issued = grant.token.is_some();
        if let Some(raw_token) = grant.token {
            let token = SecretToken::new(raw_token).map_err(|_| {
                AccountError::new(
                    "account_response_invalid",
                    "服务端返回的设备凭据格式异常。",
                    false,
                )
            })?;
            credentials::store(
                self.store.as_ref(),
                CredentialKind::Device,
                self.environment.credential_scope(),
                &token,
            )
            .map_err(|_| {
                AccountError::new(
                    "credential_store_failed",
                    "无法安全保存设备凭据，请重试。",
                    true,
                )
            })?;
        }
        Ok(EnrollOutcome {
            device_id: grant.device_id,
            client_device_id: grant.client_device_id,
            credential_issued,
        })
    }

    pub fn list_devices(&self) -> Result<Vec<AccountDevice>, AccountError> {
        let session = self.session_token()?;
        let result = self.api.list_devices(&session);
        self.observe_api("devices_list", "GET", "/app/v1/devices", result)
    }

    /// 登录态改密（docs/34 §6.3）。成功后服务端撤销**其他**会话、保留当前——
    /// 本地这份 token 依旧有效，所以既不清凭据也不改身份态；设备凭据同样不动。
    /// 与 [`Self::confirm_password_reset`]（撤全部、清本地）刻意不同。
    pub fn change_password(
        &self,
        current_password: &str,
        new_password: &str,
    ) -> Result<(), AccountError> {
        let session = self.session_token()?;
        let result = self.api.change_password(
            &session,
            &serde_json::json!({
                "currentPassword": current_password,
                "newPassword": new_password,
            }),
        );
        self.observe_api("password_change", "POST", "/app/v1/password", result)
    }

    pub fn consent_state(&self) -> Result<ConsentState, AccountError> {
        let session = self.session_token()?;
        let result = self.api.consent_state(&session);
        self.observe_api("consent_get", "GET", "/app/v1/consent", result)
    }

    /// 补签当前条款。`accepted_policy_version` 由调用方从刚展示给用户的那份
    /// 条款里取——服务端会比对，展示的是旧版就拒。
    pub fn renew_consent(&self, accepted_policy_version: &str) -> Result<(), AccountError> {
        let session = self.session_token()?;
        let result = self.api.renew_consent(
            &session,
            &serde_json::json!({ "acceptedPolicyVersion": accepted_policy_version }),
        );
        self.observe_api("consent_renew", "POST", "/app/v1/consent", result)
    }

    pub fn get_profile(&self) -> Result<Option<AccountProfile>, AccountError> {
        let session = self.session_token()?;
        let result = self.api.get_profile(&session);
        self.observe_api("profile_get", "GET", "/app/v1/profile", result)
    }

    pub fn update_profile(
        &self,
        update: &ProfileUpdate,
    ) -> Result<Option<AccountProfile>, AccountError> {
        let session = self.session_token()?;
        let result = self.api.update_profile(&session, update);
        self.observe_api("profile_update", "PUT", "/app/v1/profile", result)
    }

    fn session_token(&self) -> Result<SecretToken, AccountError> {
        credentials::load(
            self.store.as_ref(),
            CredentialKind::AnnotatorSession,
            self.environment.credential_scope(),
        )
        .map_err(|_| {
            AccountError::new(
                "credential_store_failed",
                "无法读取登录状态，请重试。",
                true,
            )
        })?
        .ok_or_else(|| AccountError::from_server("session_required"))
    }
}

pub fn account_configuration_from_env() -> AccountConfigurationResolution {
    let obsolete_base_url = match read_account_environment_value(OBSOLETE_ACCOUNT_BASE_URL_ENV) {
        Ok(value) => value,
        Err(failure) => return AccountConfigurationResolution::Invalid(failure),
    };
    let environment = match read_account_environment_value(ACCOUNT_ENVIRONMENT_ENV) {
        Ok(value) => value,
        Err(failure) => return AccountConfigurationResolution::Invalid(failure),
    };
    let base_url = match read_account_environment_value(ACCOUNT_API_BASE_URL_ENV) {
        Ok(value) => value,
        Err(failure) => return AccountConfigurationResolution::Invalid(failure),
    };
    if obsolete_base_url.is_some() {
        return resolve_account_configuration_values(
            environment.as_deref(),
            base_url.as_deref(),
            obsolete_base_url.as_deref(),
        );
    }

    // A packaged operator build may carry an explicit, public account endpoint so it works on a
    // clean machine without mutating global environment variables. Runtime values still win as a
    // pair; supplying only one remains an error instead of borrowing the other from the bundle.
    if environment.is_none() && base_url.is_none() {
        return resolve_account_configuration(
            EMBEDDED_ACCOUNT_ENVIRONMENT,
            EMBEDDED_ACCOUNT_API_BASE_URL,
        );
    }
    resolve_account_configuration(environment.as_deref(), base_url.as_deref())
}

pub fn record_account_startup_failure(
    diagnostics: &DiagnosticService,
    failure: &AccountConfigurationFailure,
) {
    if let Err(error) = diagnostics.record(
        "error",
        None,
        DiagnosticCorrelation::default(),
        DiagnosticPayload::ServiceStartupDegraded {
            service: "account".into(),
            code: failure.code.clone(),
            reason: failure.reason.clone(),
        },
        true,
    ) {
        // 诊断是 best-effort，写盘失败不能反过来拖垮本地采集。
        tracing::error!(error = %error, "failed to record account startup diagnostics");
    }
}

fn read_account_environment_value(
    name: &'static str,
) -> Result<Option<String>, AccountConfigurationFailure> {
    match std::env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(AccountConfigurationFailure::new(
            "account_configuration_invalid",
            "账号服务配置包含无法识别的字符，请联系管理员。账号连接已停止。",
            format!("{name} is not valid Unicode"),
        )),
    }
}

#[must_use]
pub fn resolve_account_configuration(
    environment: Option<&str>,
    base_url: Option<&str>,
) -> AccountConfigurationResolution {
    resolve_account_configuration_values(environment, base_url, None)
}

#[must_use]
fn resolve_account_configuration_values(
    environment: Option<&str>,
    base_url: Option<&str>,
    obsolete_base_url: Option<&str>,
) -> AccountConfigurationResolution {
    if obsolete_base_url.is_some() {
        return AccountConfigurationResolution::Invalid(AccountConfigurationFailure::new(
            "account_configuration_obsolete_variable",
            "检测到旧账号地址变量 DATAMINER_ACCOUNT_BASE_URL；请删除它并使用当前两项配置。",
            "obsolete DATAMINER_ACCOUNT_BASE_URL is set; use DATAMINER_ACCOUNT_ENVIRONMENT and DATAMINER_ACCOUNT_API_BASE_URL",
        ));
    }
    let (environment, base_url) = match (environment, base_url) {
        (None, None) => return AccountConfigurationResolution::Unconfigured,
        (None, Some(_)) => {
            return AccountConfigurationResolution::Invalid(AccountConfigurationFailure::new(
                "account_configuration_incomplete",
                "已配置账号服务地址，但缺少账号环境；请同时设置 staging 或 production。",
                "DATAMINER_ACCOUNT_ENVIRONMENT is required when DATAMINER_ACCOUNT_API_BASE_URL is set",
            ));
        }
        (Some(_), None) => {
            return AccountConfigurationResolution::Invalid(AccountConfigurationFailure::new(
                "account_configuration_incomplete",
                "已选择账号环境，但缺少账号服务地址；请补全配置。",
                "DATAMINER_ACCOUNT_API_BASE_URL is required when DATAMINER_ACCOUNT_ENVIRONMENT is set",
            ));
        }
        (Some(environment), Some(base_url)) => (environment, base_url),
    };

    if environment.is_empty() || base_url.trim().is_empty() {
        return AccountConfigurationResolution::Invalid(AccountConfigurationFailure::new(
            "account_configuration_invalid",
            "账号环境和账号服务地址都不能为空；请修正配置。账号连接已停止。",
            "account environment and base URL must not be empty",
        ));
    }
    if base_url != base_url.trim() {
        return AccountConfigurationResolution::Invalid(AccountConfigurationFailure::new(
            "account_base_url_invalid",
            "账号服务地址前后不能有空白字符。",
            "account base URL must not have surrounding whitespace",
        ));
    }

    let environment = match environment {
        "staging" => AccountEnvironment::Staging,
        "production" => AccountEnvironment::Production,
        _ => {
            return AccountConfigurationResolution::Invalid(AccountConfigurationFailure::new(
                "account_environment_invalid",
                "账号环境只能是 staging 或 production（区分大小写）。",
                "DATAMINER_ACCOUNT_ENVIRONMENT must be exactly staging or production",
            ));
        }
    };
    let parsed = match parse_account_base_url(base_url) {
        Ok(parsed) => parsed,
        Err(error) => {
            let reason = error.to_string();
            let message = match reason.as_str() {
                "account base URL must be https" => "账号服务地址必须使用 https。",
                "account base URL must not carry a query or fragment" => {
                    "账号服务地址不能包含 query 或 fragment。"
                }
                "account base URL must not contain user info" => {
                    "账号服务地址不能包含用户名或密码。"
                }
                "account base URL must not include the /app/v1 prefix; the client appends it" => {
                    "账号服务地址必须是根地址，不能包含 /app/v1。"
                }
                _ => "账号服务地址不是有效的 URL。",
            };
            return AccountConfigurationResolution::Invalid(AccountConfigurationFailure::new(
                "account_base_url_invalid",
                message,
                reason,
            ));
        }
    };
    let normalized = parsed.to_string();
    let is_production_origin = normalized == CANONICAL_PRODUCTION_ACCOUNT_API_BASE_URL;
    let targets_production_host = targets_production_account_host(&parsed);
    if (environment == AccountEnvironment::Production && !is_production_origin)
        || (environment == AccountEnvironment::Staging && targets_production_host)
    {
        return AccountConfigurationResolution::Invalid(AccountConfigurationFailure::new(
            "account_environment_conflict",
            "账号环境与服务地址矛盾，已停止账号连接与账号环境目录。",
            "account environment conflicts with the configured account base URL",
        ));
    }

    AccountConfigurationResolution::Ready(ResolvedAccountConfiguration {
        environment,
        base_url: normalized,
    })
}

/// DNS 绝对域名允许以终止点结尾；比较安全边界时必须把该写法与普通主机名视为同一目标。
///
/// 这个 helper 同时供账号 API 和账号同源的 client-events 通道使用，避免两条路径的
/// production 防误打规则发生漂移。
#[must_use]
pub(crate) fn targets_production_account_host(url: &reqwest::Url) -> bool {
    url.host_str().is_some_and(|host| {
        host.trim_end_matches('.')
            .eq_ignore_ascii_case("console.diogenesmodel.com")
    })
}

pub fn load_or_create_client_device_id(app_data_root: &Path) -> anyhow::Result<String> {
    fs::create_dir_all(app_data_root).context("failed to create app data directory")?;
    let path = app_data_root.join(CLIENT_DEVICE_ID_FILE);
    if path.exists() {
        return read_client_device_id(&path);
    }

    let id = format!("dmc-{}", uuid::Uuid::new_v4().simple());
    let staging = app_data_root.join(format!(
        ".{CLIENT_DEVICE_ID_FILE}.{}.tmp",
        uuid::Uuid::new_v4().simple()
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&staging)
        .context("failed to create client device id staging file")?;
    file.write_all(id.as_bytes())
        .context("failed to write client device id")?;
    file.sync_all().context("failed to sync client device id")?;
    drop(file);

    match fs::rename(&staging, &path) {
        Ok(()) => Ok(id),
        Err(_) if path.is_file() => {
            let _ = fs::remove_file(&staging);
            read_client_device_id(&path)
        }
        Err(error) => {
            let _ = fs::remove_file(&staging);
            Err(error).context("failed to publish client device id")
        }
    }
}

fn read_client_device_id(path: &Path) -> anyhow::Result<String> {
    let value = fs::read_to_string(path).context("failed to read client device id")?;
    let value = value.trim();
    let suffix = value
        .strip_prefix("dmc-")
        .context("client device id has an invalid prefix")?;
    anyhow::ensure!(
        suffix.len() == 32
            && suffix
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "client device id has an invalid format"
    );
    Ok(value.to_string())
}

fn parse_account_base_url(base_url: &str) -> anyhow::Result<reqwest::Url> {
    let mut parsed = reqwest::Url::parse(base_url).context("account base URL is not a URL")?;
    // 明文 HTTP 会把邀请码、密码和会话 token 暴露。debug 只为本机拦截器保留回环口。
    let is_local = matches!(parsed.host_str(), Some("127.0.0.1") | Some("localhost"));
    anyhow::ensure!(
        parsed.scheme() == "https"
            || (cfg!(debug_assertions) && parsed.scheme() == "http" && is_local),
        "account base URL must be https"
    );
    anyhow::ensure!(
        parsed.query().is_none() && parsed.fragment().is_none(),
        "account base URL must not carry a query or fragment"
    );
    anyhow::ensure!(
        parsed.username().is_empty() && parsed.password().is_none(),
        "account base URL must not contain user info"
    );
    // 每个方法都自己拼 `app/v1/…`；base 再带一次会得到不存在的重复路径。
    anyhow::ensure!(
        !parsed.path().contains("/app/v1"),
        "account base URL must not include the /app/v1 prefix; the client appends it"
    );
    if !parsed.path().ends_with('/') {
        let path = format!("{}/", parsed.path());
        parsed.set_path(&path);
    }
    Ok(parsed)
}

/// reqwest blocking 实现。
///
/// blocking 而不是 async：客户端里唯一的 async 运行时是 Tauri 的，而这些调用都由
/// 命令触发、在 `spawn_blocking` 里跑（`catalog_delivery` 已经踩过「阻塞 client 在
/// async 上下文 panic」这个坑）。引入第二套 async 栈只会增大发布面。
pub struct HttpAccountApi {
    client: reqwest::blocking::Client,
    base: reqwest::Url,
}

impl HttpAccountApi {
    pub fn new(base_url: &str, client_version: &str) -> anyhow::Result<Self> {
        let parsed = parse_account_base_url(base_url)?;
        let client = reqwest::blocking::Client::builder()
            // URL/environment checks apply to the actual credential recipient. Following a
            // redirect would let the first hop move passwords or bearer tokens elsewhere.
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .user_agent(format!("DataMiner/{client_version}"))
            .build()
            .context("failed to build the account HTTP client")?;
        Ok(Self {
            client,
            base: parsed,
        })
    }

    fn url(&self, path: &str) -> Result<reqwest::Url, AccountError> {
        self.base.join(path).map_err(|_| {
            AccountError::new("account_request_invalid", "内部错误：请求地址异常。", false)
        })
    }

    /// 账号服务的每个响应都是 JSON——成功和失败都是。别的 content-type 意味着
    /// 应答的根本不是它，而不是它答错了。
    fn carries_json(response: &reqwest::blocking::Response) -> bool {
        response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.to_ascii_lowercase().contains("json"))
    }

    /// `post_no_content` 的成功形态是 204 空体，往往连 content-type 头都没有——
    /// 所以只把「明确声称自己不是 JSON」的响应当作打错了地方，缺头不算。
    fn carries_non_json(response: &reqwest::blocking::Response) -> bool {
        response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| !value.to_ascii_lowercase().contains("json"))
    }

    /// 把响应变成结果。**不回显服务端 message**——只取 `code` 走本地映射表。
    fn interpret<T: for<'de> Deserialize<'de>>(
        response: reqwest::blocking::Response,
    ) -> Result<T, AccountError> {
        let status = response.status();
        if status.is_success() {
            if !Self::carries_json(&response) {
                return Err(AccountError::not_account_service());
            }
            return response.json::<T>().map_err(|_| {
                AccountError::new(
                    "account_response_invalid",
                    "服务端返回的内容无法解析。",
                    false,
                )
            });
        }
        Err(Self::error_for(status, response))
    }

    fn error_for(
        status: reqwest::StatusCode,
        response: reqwest::blocking::Response,
    ) -> AccountError {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Body {
            code: Option<String>,
            request_id: Option<String>,
            retryable: Option<bool>,
        }
        let request_id_header = response
            .headers()
            .get("x-request-id")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let retry_after_seconds = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok());
        let status_code = status.as_u16();
        // 4xx 也一样：一个 HTML 的 404 是「这个地址上没有账号服务」，
        // 不是「你的请求有问题」。
        if !Self::carries_json(&response) {
            return AccountError::not_account_service().with_response_metadata(
                status_code,
                request_id_header,
                retry_after_seconds,
            );
        }
        match response.json::<Body>() {
            Ok(body) => AccountError::from_response(
                body.code.as_deref().unwrap_or(""),
                status_code,
                body.request_id.or(request_id_header),
                body.retryable,
                retry_after_seconds,
            ),
            Err(_) => AccountError::from_response(
                "",
                status_code,
                request_id_header,
                None,
                retry_after_seconds,
            ),
        }
    }

    fn post_json<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        body: &serde_json::Value,
    ) -> Result<T, AccountError> {
        let response = self
            .client
            .post(self.url(path)?)
            .json(body)
            .send()
            // reqwest 的错误会带上完整 URL；整个丢掉，只留一个稳定码。
            .map_err(|_| AccountError::unavailable())?;
        Self::interpret(response)
    }

    fn post_authenticated<T: for<'de> Deserialize<'de>, B: Serialize>(
        &self,
        path: &str,
        body: &B,
        session: &SecretToken,
    ) -> Result<T, AccountError> {
        let response = self
            .client
            .post(self.url(path)?)
            .bearer_auth(session.expose())
            .json(body)
            .send()
            .map_err(|_| AccountError::unavailable())?;
        Self::interpret(response)
    }

    fn put_authenticated<T: for<'de> Deserialize<'de>, B: Serialize>(
        &self,
        path: &str,
        body: &B,
        session: &SecretToken,
    ) -> Result<T, AccountError> {
        let response = self
            .client
            .put(self.url(path)?)
            .bearer_auth(session.expose())
            .json(body)
            .send()
            .map_err(|_| AccountError::unavailable())?;
        Self::interpret(response)
    }

    /// 204 专用：空 body 上调 `json()` 会失败。
    fn post_no_content(
        &self,
        path: &str,
        body: &serde_json::Value,
        session: Option<&SecretToken>,
    ) -> Result<(), AccountError> {
        let mut request = self.client.post(self.url(path)?).json(body);
        if let Some(token) = session {
            request = request.bearer_auth(token.expose());
        }
        let response = request.send().map_err(|_| AccountError::unavailable())?;
        let status = response.status();
        // 204 的语义已经是「成功且没有响应正文」。生产链路曾出现源站正确返回
        // 204、边缘层却附带 `Content-Type: text/html` 的情况；按这个无正文响应头
        // 反推「拿到的是网页」会把已经完成的注册误报成失败，用户重试时验证码又
        // 已被消费。先认协议状态，再对其余 2xx 保留网页误路由检测。
        if status == reqwest::StatusCode::NO_CONTENT {
            return Ok(());
        }
        if status.is_success() {
            // 走到这的三个动作（验证注册、确认重置密码、登出）都是「做了就算数」的，
            // 而 Worker 没转发 /app/ 时它们照样拿到 200——只是一张 text/html 的网页。
            // 「密码已重置」不能建立在一张网页上；对明确非 JSON 的成功照样报
            // not_account_service。正常的 204 空体没有 content-type，不受影响。
            if Self::carries_non_json(&response) {
                return Err(AccountError::not_account_service());
            }
            return Ok(());
        }
        Err(Self::error_for(status, response))
    }
}

impl AccountApi for HttpAccountApi {
    fn terms(&self) -> Result<Terms, AccountError> {
        let response = self
            .client
            .get(self.url("app/v1/terms")?)
            .send()
            .map_err(|_| AccountError::unavailable())?;
        Self::interpret(response)
    }

    fn signup(&self, body: &serde_json::Value) -> Result<SignupChallenge, AccountError> {
        self.post_json("app/v1/signup", body)
    }

    fn verify_signup(&self, body: &serde_json::Value) -> Result<(), AccountError> {
        self.post_no_content("app/v1/signup/verify", body, None)
    }

    fn resend_signup_code(
        &self,
        body: &serde_json::Value,
    ) -> Result<SignupChallenge, AccountError> {
        self.post_json("app/v1/signup/resend", body)
    }

    fn request_password_reset(
        &self,
        body: &serde_json::Value,
    ) -> Result<SignupChallenge, AccountError> {
        self.post_json("app/v1/password-reset/request", body)
    }

    fn confirm_password_reset(&self, body: &serde_json::Value) -> Result<(), AccountError> {
        self.post_no_content("app/v1/password-reset/confirm", body, None)
    }

    fn login(&self, body: &serde_json::Value) -> Result<SessionGrant, AccountError> {
        self.post_json("app/v1/login", body)
    }

    fn logout(&self, session: &SecretToken) -> Result<(), AccountError> {
        self.post_no_content("app/v1/logout", &serde_json::json!({}), Some(session))
    }

    fn session(&self, session: &SecretToken) -> Result<String, AccountError> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Body {
            annotator_id: String,
        }
        let response = self
            .client
            .get(self.url("app/v1/session")?)
            .bearer_auth(session.expose())
            .send()
            .map_err(|_| AccountError::unavailable())?;
        let body: Body = Self::interpret(response)?;
        Ok(body.annotator_id)
    }

    fn enroll_device(
        &self,
        session: &SecretToken,
        body: &DeviceEnrollmentRequest,
    ) -> Result<EnrollGrant, AccountError> {
        self.post_authenticated("app/v1/devices/enroll", body, session)
    }

    fn list_devices(&self, session: &SecretToken) -> Result<Vec<AccountDevice>, AccountError> {
        #[derive(Deserialize)]
        struct Body {
            devices: Vec<AccountDevice>,
        }
        let response = self
            .client
            .get(self.url("app/v1/devices")?)
            .bearer_auth(session.expose())
            .send()
            .map_err(|_| AccountError::unavailable())?;
        Ok(Self::interpret::<Body>(response)?.devices)
    }

    fn change_password(
        &self,
        session: &SecretToken,
        body: &serde_json::Value,
    ) -> Result<(), AccountError> {
        self.post_no_content("app/v1/password", body, Some(session))
    }

    fn consent_state(&self, session: &SecretToken) -> Result<ConsentState, AccountError> {
        let response = self
            .client
            .get(self.url("app/v1/consent")?)
            .bearer_auth(session.expose())
            .send()
            .map_err(|_| AccountError::unavailable())?;
        Self::interpret(response)
    }

    fn renew_consent(
        &self,
        session: &SecretToken,
        body: &serde_json::Value,
    ) -> Result<(), AccountError> {
        self.post_no_content("app/v1/consent", body, Some(session))
    }

    fn get_profile(&self, session: &SecretToken) -> Result<Option<AccountProfile>, AccountError> {
        #[derive(Deserialize)]
        struct Body {
            profile: Option<AccountProfile>,
        }
        let response = self
            .client
            .get(self.url("app/v1/profile")?)
            .bearer_auth(session.expose())
            .send()
            .map_err(|_| AccountError::unavailable())?;
        Ok(Self::interpret::<Body>(response)?.profile)
    }

    fn update_profile(
        &self,
        session: &SecretToken,
        body: &ProfileUpdate,
    ) -> Result<Option<AccountProfile>, AccountError> {
        #[derive(Deserialize)]
        struct Body {
            profile: Option<AccountProfile>,
        }
        Ok(self
            .put_authenticated::<Body, _>("app/v1/profile", body, session)?
            .profile)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::credentials::InMemoryCredentialStore;
    use std::io::Read;
    use std::net::TcpListener;
    use std::thread;

    /// 起一个只回答一次的 loopback HTTP 服务，给 `reqwest` 的真实响应解析路径做回归。
    /// 请求只为避免 Windows 提前关连接时发 RST 而读完，不解析、不记录正文里的测试值。
    fn serve_once(response: &'static str) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            // Windows 在服务端带着未读请求数据直接关连接时可能发 RST，reqwest 看到的
            // 就会是传输失败而不是下面的响应。读完 header 与 Content-Length 指定的
            // JSON 正文再应答，夹具才和真正的 HTTP 服务一样。
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            let (header_bytes, content_length) = loop {
                let read = stream.read(&mut buffer).unwrap();
                assert!(read > 0, "client closed before sending the request headers");
                request.extend_from_slice(&buffer[..read]);
                if let Some(header_end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n")
                {
                    let header_bytes = header_end + 4;
                    let headers = String::from_utf8_lossy(&request[..header_end]);
                    let content_length = headers
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    break (header_bytes, content_length);
                }
            };
            while request.len() < header_bytes + content_length {
                let read = stream.read(&mut buffer).unwrap();
                assert!(read > 0, "client closed before sending the request body");
                request.extend_from_slice(&buffer[..read]);
            }
            stream.write_all(response.as_bytes()).unwrap();
        });
        (format!("http://{address}/"), handle)
    }

    /// release profile 会正确拒绝 loopback 明文 HTTP；测试模块直接构造内部传输，
    /// 只绕过 URL 策略门，不改变生产构造器的 HTTPS 约束。
    fn loopback_api(base: &str) -> HttpAccountApi {
        HttpAccountApi {
            client: reqwest::blocking::Client::builder().build().unwrap(),
            base: reqwest::Url::parse(base).unwrap(),
        }
    }

    /// 可编程的假传输：每个方法返回预设结果，并记下被调用过没有。
    #[derive(Default)]
    struct FakeApi {
        session_result: Mutex<Option<Result<String, AccountError>>>,
        login_result: Mutex<Option<Result<SessionGrant, AccountError>>>,
        verify_result: Mutex<Option<Result<(), AccountError>>>,
        enroll_result: Mutex<Option<Result<EnrollGrant, AccountError>>>,
        listed_devices: Mutex<Option<Result<Vec<AccountDevice>, AccountError>>>,
        logout_calls: Mutex<u32>,
        reset_confirm_result: Mutex<Option<Result<(), AccountError>>>,
        change_password_result: Mutex<Option<Result<(), AccountError>>>,
    }

    impl AccountApi for FakeApi {
        fn consent_state(&self, _session: &SecretToken) -> Result<ConsentState, AccountError> {
            unimplemented!()
        }
        fn renew_consent(
            &self,
            _session: &SecretToken,
            _body: &serde_json::Value,
        ) -> Result<(), AccountError> {
            unimplemented!()
        }
        fn terms(&self) -> Result<Terms, AccountError> {
            unimplemented!()
        }
        fn signup(&self, _body: &serde_json::Value) -> Result<SignupChallenge, AccountError> {
            unimplemented!()
        }
        fn verify_signup(&self, _body: &serde_json::Value) -> Result<(), AccountError> {
            self.verify_result
                .lock()
                .take()
                .unwrap_or_else(|| Err(AccountError::unavailable()))
        }
        fn resend_signup_code(
            &self,
            _body: &serde_json::Value,
        ) -> Result<SignupChallenge, AccountError> {
            unimplemented!()
        }
        fn request_password_reset(
            &self,
            _body: &serde_json::Value,
        ) -> Result<SignupChallenge, AccountError> {
            unimplemented!()
        }
        fn confirm_password_reset(&self, _body: &serde_json::Value) -> Result<(), AccountError> {
            self.reset_confirm_result
                .lock()
                .take()
                .unwrap_or_else(|| Err(AccountError::unavailable()))
        }
        fn login(&self, _body: &serde_json::Value) -> Result<SessionGrant, AccountError> {
            self.login_result
                .lock()
                .take()
                .unwrap_or_else(|| Err(AccountError::unavailable()))
        }
        fn logout(&self, _session: &SecretToken) -> Result<(), AccountError> {
            *self.logout_calls.lock() += 1;
            Ok(())
        }
        fn session(&self, _session: &SecretToken) -> Result<String, AccountError> {
            self.session_result
                .lock()
                .take()
                .unwrap_or_else(|| Err(AccountError::unavailable()))
        }
        fn enroll_device(
            &self,
            _session: &SecretToken,
            _body: &DeviceEnrollmentRequest,
        ) -> Result<EnrollGrant, AccountError> {
            self.enroll_result
                .lock()
                .take()
                .unwrap_or_else(|| Err(AccountError::unavailable()))
        }
        fn list_devices(&self, _session: &SecretToken) -> Result<Vec<AccountDevice>, AccountError> {
            self.listed_devices
                .lock()
                .take()
                .unwrap_or_else(|| Err(AccountError::unavailable()))
        }
        fn change_password(
            &self,
            _session: &SecretToken,
            _body: &serde_json::Value,
        ) -> Result<(), AccountError> {
            self.change_password_result
                .lock()
                .take()
                .unwrap_or_else(|| Err(AccountError::unavailable()))
        }
        fn get_profile(
            &self,
            _session: &SecretToken,
        ) -> Result<Option<AccountProfile>, AccountError> {
            unimplemented!()
        }
        fn update_profile(
            &self,
            _session: &SecretToken,
            _body: &ProfileUpdate,
        ) -> Result<Option<AccountProfile>, AccountError> {
            unimplemented!()
        }
    }

    fn service(api: Arc<FakeApi>) -> (AccountService, Arc<InMemoryCredentialStore>) {
        service_for_environment(api, AccountEnvironment::Production)
    }

    fn service_for_environment(
        api: Arc<FakeApi>,
        environment: AccountEnvironment,
    ) -> (AccountService, Arc<InMemoryCredentialStore>) {
        let store = Arc::new(InMemoryCredentialStore::default());
        let service = AccountService::new(api, store.clone(), environment);
        (service, store)
    }

    fn enrollment_request() -> DeviceEnrollmentRequest {
        DeviceEnrollmentRequest::new(
            "dmc-00000000000000000000000000000000".into(),
            "10.0.26100".into(),
            GpuSnapshot {
                pci_id: "PCI\\VEN_10DE&DEV_1C03".into(),
                driver_version: "32.0.15.6094".into(),
            },
            "0.1.0".into(),
            DisplaySnapshot {
                schema_version: "dm-display-snapshot-1",
                monitors: Vec::new(),
            },
            InputDevicesSnapshot {
                schema_version: "dm-input-devices-1",
                keyboards: 1,
                mice: 1,
                hids: 0,
            },
        )
    }

    #[test]
    fn enrollment_request_matches_the_server_contract_and_privacy_boundary() {
        let value = serde_json::to_value(enrollment_request()).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "clientDeviceId": "dmc-00000000000000000000000000000000",
                "windowsBuild": "10.0.26100",
                "gpuPciId": "PCI\\VEN_10DE&DEV_1C03",
                "gpuDriver": "32.0.15.6094",
                "clientVersion": "0.1.0",
                "displaySnapshot": {
                    "schemaVersion": "dm-display-snapshot-1",
                    "monitors": [],
                },
                "inputDevicesSnapshot": {
                    "schemaVersion": "dm-input-devices-1",
                    "keyboards": 1,
                    "mice": 1,
                    "hids": 0,
                },
            })
        );
        let serialized = serde_json::to_string(&value).unwrap().to_ascii_lowercase();
        for forbidden in ["edid", "devicename", "devicepath", "serial"] {
            assert!(!serialized.contains(forbidden));
        }
    }

    #[test]
    fn account_identity_wire_contract_matches_the_frontend() {
        assert_eq!(
            serde_json::to_value(AccountIdentity::Unknown).unwrap(),
            serde_json::json!({ "kind": "unknown" })
        );
        assert_eq!(
            serde_json::to_value(AccountIdentity::SignedIn {
                annotator_id: "annotator-1".into(),
            })
            .unwrap(),
            serde_json::json!({
                "kind": "signedIn",
                "annotatorId": "annotator-1",
            })
        );
        assert_eq!(
            serde_json::to_value(AccountIdentity::SignedOut).unwrap(),
            serde_json::json!({ "kind": "signedOut" })
        );
    }

    #[test]
    fn login_stores_the_session_and_resolves_who_it_is() {
        let api = Arc::new(FakeApi::default());
        *api.login_result.lock() = Some(Ok(SessionGrant {
            token: "session-token".to_string(),
        }));
        *api.session_result.lock() = Some(Ok("annotator-1".to_string()));
        let (service, store) = service(api);

        let identity = service
            .login("a@example.com", "correct horse battery")
            .unwrap();

        assert!(matches!(
            identity,
            AccountIdentity::SignedIn { ref annotator_id } if annotator_id == "annotator-1"
        ));
        assert!(
            credentials::load(
                store.as_ref(),
                CredentialKind::AnnotatorSession,
                "production"
            )
            .unwrap()
            .is_some()
        );
    }

    /// 断网不是登出。把用户的会话在网络抖动时丢掉，会让他以为自己被踢了。
    #[test]
    fn a_transport_failure_leaves_the_session_alone() {
        let api = Arc::new(FakeApi::default());
        let (service, store) = service(api);
        credentials::store(
            store.as_ref(),
            CredentialKind::AnnotatorSession,
            "production",
            &SecretToken::new("stored-token").unwrap(),
        )
        .unwrap();

        let identity = service.refresh_identity().unwrap();

        assert!(matches!(identity, AccountIdentity::Unknown));
        assert!(
            credentials::load(
                store.as_ref(),
                CredentialKind::AnnotatorSession,
                "production"
            )
            .unwrap()
            .is_some(),
            "an offline start must not discard the session"
        );
    }

    /// 服务端说这把钥匙不认了，才清掉它。
    #[test]
    fn a_rejected_session_is_cleared() {
        let api = Arc::new(FakeApi::default());
        *api.session_result.lock() = Some(Err(AccountError::from_server("session_required")));
        let (service, store) = service(api);
        credentials::store(
            store.as_ref(),
            CredentialKind::AnnotatorSession,
            "production",
            &SecretToken::new("stale-token").unwrap(),
        )
        .unwrap();

        let identity = service.refresh_identity().unwrap();

        assert!(matches!(identity, AccountIdentity::SignedOut));
        assert!(
            credentials::load(
                store.as_ref(),
                CredentialKind::AnnotatorSession,
                "production"
            )
            .unwrap()
            .is_none()
        );
    }

    /// 登出不能碰设备凭据：那是机器的身份，删了会让排队中的上传全部停摆。
    #[test]
    fn logout_clears_the_session_but_not_the_device_credential() {
        let api = Arc::new(FakeApi::default());
        let (service, store) = service(api.clone());
        credentials::store(
            store.as_ref(),
            CredentialKind::AnnotatorSession,
            "production",
            &SecretToken::new("session-token").unwrap(),
        )
        .unwrap();
        credentials::store(
            store.as_ref(),
            CredentialKind::Device,
            "production",
            &SecretToken::new("device-token").unwrap(),
        )
        .unwrap();

        service.logout();

        assert_eq!(*api.logout_calls.lock(), 1);
        assert!(
            credentials::load(
                store.as_ref(),
                CredentialKind::AnnotatorSession,
                "production"
            )
            .unwrap()
            .is_none()
        );
        assert!(
            credentials::load(store.as_ref(), CredentialKind::Device, "production")
                .unwrap()
                .is_some(),
            "the device identity outlives the person's sign-in"
        );
    }

    #[test]
    fn enrollment_stores_the_one_time_device_credential() {
        let api = Arc::new(FakeApi::default());
        *api.enroll_result.lock() = Some(Ok(EnrollGrant {
            device_id: "device-1".into(),
            client_device_id: "dmc-00000000000000000000000000000000".into(),
            token: Some("device-token".into()),
        }));
        let (service, store) = service(api);
        credentials::store(
            store.as_ref(),
            CredentialKind::AnnotatorSession,
            "production",
            &SecretToken::new("session-token").unwrap(),
        )
        .unwrap();

        let outcome = service.enroll_device(enrollment_request()).unwrap();

        assert!(outcome.credential_issued);
        assert_eq!(outcome.device_id, "device-1");
        assert!(
            credentials::load(store.as_ref(), CredentialKind::Device, "production")
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn staging_login_and_enrollment_never_touch_production_credentials() {
        let api = Arc::new(FakeApi::default());
        *api.login_result.lock() = Some(Ok(SessionGrant {
            token: "staging-session-token".into(),
        }));
        *api.session_result.lock() = Some(Ok("annotator-staging".into()));
        *api.enroll_result.lock() = Some(Ok(EnrollGrant {
            device_id: "device-staging".into(),
            client_device_id: "dmc-00000000000000000000000000000000".into(),
            token: Some("staging-device-token".into()),
        }));
        let (service, store) = service_for_environment(api, AccountEnvironment::Staging);
        credentials::store(
            store.as_ref(),
            CredentialKind::AnnotatorSession,
            "production",
            &SecretToken::new("production-session-token").unwrap(),
        )
        .unwrap();
        credentials::store(
            store.as_ref(),
            CredentialKind::Device,
            "production",
            &SecretToken::new("production-device-token").unwrap(),
        )
        .unwrap();

        service
            .login("staging@example.com", "correct horse battery")
            .unwrap();
        service.enroll_device(enrollment_request()).unwrap();

        assert_eq!(
            credentials::load(store.as_ref(), CredentialKind::AnnotatorSession, "staging")
                .unwrap()
                .unwrap()
                .expose(),
            "staging-session-token"
        );
        assert_eq!(
            credentials::load(store.as_ref(), CredentialKind::Device, "staging")
                .unwrap()
                .unwrap()
                .expose(),
            "staging-device-token"
        );
        assert_eq!(
            credentials::load(
                store.as_ref(),
                CredentialKind::AnnotatorSession,
                "production"
            )
            .unwrap()
            .unwrap()
            .expose(),
            "production-session-token"
        );
        assert_eq!(
            credentials::load(store.as_ref(), CredentialKind::Device, "production")
                .unwrap()
                .unwrap()
                .expose(),
            "production-device-token"
        );
    }

    #[test]
    fn idempotent_enrollment_does_not_replace_an_existing_device_credential() {
        let api = Arc::new(FakeApi::default());
        *api.enroll_result.lock() = Some(Ok(EnrollGrant {
            device_id: "device-1".into(),
            client_device_id: "dmc-00000000000000000000000000000000".into(),
            token: None,
        }));
        let (service, store) = service(api);
        credentials::store(
            store.as_ref(),
            CredentialKind::AnnotatorSession,
            "production",
            &SecretToken::new("session-token").unwrap(),
        )
        .unwrap();
        credentials::store(
            store.as_ref(),
            CredentialKind::Device,
            "production",
            &SecretToken::new("existing-device-token").unwrap(),
        )
        .unwrap();

        let outcome = service.enroll_device(enrollment_request()).unwrap();

        assert!(!outcome.credential_issued);
        let stored = credentials::load(store.as_ref(), CredentialKind::Device, "production")
            .unwrap()
            .unwrap();
        assert_eq!(stored.expose(), "existing-device-token");
    }

    #[test]
    fn client_device_id_is_stable_and_rejects_corrupt_state() {
        let temp = tempfile::tempdir().unwrap();
        let first = load_or_create_client_device_id(temp.path()).unwrap();
        let second = load_or_create_client_device_id(temp.path()).unwrap();
        assert_eq!(first, second);
        assert!(first.starts_with("dmc-"));
        assert_eq!(first.len(), 36);

        fs::write(temp.path().join(CLIENT_DEVICE_ID_FILE), "hardware-serial").unwrap();
        assert!(load_or_create_client_device_id(temp.path()).is_err());
    }

    /// production canonical 地址必须自身合法；它只校验显式选择，绝不是缺省 fallback。
    #[test]
    fn the_canonical_production_account_base_url_is_itself_valid() {
        assert!(CANONICAL_PRODUCTION_ACCOUNT_API_BASE_URL.starts_with("https://"));
        assert!(HttpAccountApi::new(CANONICAL_PRODUCTION_ACCOUNT_API_BASE_URL, "0.4.0").is_ok());
    }

    /// 路径由 base 加上方法自带的 `app/v1/…` 组成。base 自己再带一次前缀会把每个
    /// 请求送到 `/app/v1/app/v1/…`——服务端答 session_required，界面说「登录已过期」，
    /// 而实际上没有任何会话过期。这种配置在构造时就该被拒。
    #[test]
    fn account_paths_are_joined_onto_the_base_and_the_prefix_is_never_doubled() {
        let api = HttpAccountApi::new("https://console.diogenesmodel.com", "0.5.0").unwrap();
        assert_eq!(
            api.url("app/v1/terms").unwrap().as_str(),
            "https://console.diogenesmodel.com/app/v1/terms"
        );
        assert!(HttpAccountApi::new("https://console.diogenesmodel.com/app/v1", "0.5.0").is_err());
        assert!(HttpAccountApi::new("https://console.diogenesmodel.com/app/v1/", "0.5.0").is_err());
    }

    /// 2026-08-03 生产现场：源站完成注册并返回 204，但边缘链路给这个空响应附了
    /// `text/html`，旧客户端因此报「不是账号服务」，随后重试又因验证码已消费而 401。
    #[test]
    fn no_content_success_ignores_a_spurious_non_json_content_type() {
        let (base, server) = serve_once(
            "HTTP/1.1 204 No Content\r\nContent-Type: text/html; charset=utf-8\r\nConnection: close\r\n\r\n",
        );
        let api = loopback_api(&base);

        assert!(
            api.verify_signup(&serde_json::json!({
                "challengeId": "challenge-must-not-be-logged",
                "code": "654321"
            }))
            .is_ok()
        );
        server.join().unwrap();
    }

    /// 放宽的只有协议明确的 204；Worker 把 `/app/` 错送到 SPA 时仍会得到
    /// `200 + text/html`，这张网页绝不能被当成验证成功。
    #[test]
    fn html_ok_response_is_still_rejected_for_no_content_operations() {
        let (base, server) = serve_once(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: 15\r\nConnection: close\r\n\r\n<!doctype html>",
        );
        let api = loopback_api(&base);

        let error = api
            .verify_signup(&serde_json::json!({
                "challengeId": "challenge-must-not-be-logged",
                "code": "654321"
            }))
            .unwrap_err();
        assert_eq!(error.code, "account_api_not_account_service");
        server.join().unwrap();
    }

    /// 重置成功后服务端吊销全部会话，本地那份就是废的。留着它会让界面停在
    /// 「已登录」，直到下一个请求 401 才露馅——用户看到的是"密码改好了但一操作
    /// 就被踢"，而不是"请用新密码重新登录"。
    #[test]
    fn a_confirmed_password_reset_drops_the_local_session() {
        let api = Arc::new(FakeApi::default());
        *api.reset_confirm_result.lock() = Some(Ok(()));
        let (service, store) = service(api);
        credentials::store(
            store.as_ref(),
            CredentialKind::AnnotatorSession,
            "production",
            &SecretToken::new("stale-session-token").unwrap(),
        )
        .unwrap();

        service
            .confirm_password_reset("challenge-1", "123456", "a whole new correct horse")
            .unwrap();

        assert!(
            credentials::load(
                store.as_ref(),
                CredentialKind::AnnotatorSession,
                "production"
            )
            .unwrap()
            .is_none(),
            "the revoked session token must not survive a reset"
        );
        assert!(matches!(service.identity(), AccountIdentity::SignedOut));
    }

    /// 失败时**不能**清会话：一个打错的验证码不该把已登录的人踢下线。
    #[test]
    fn a_failed_password_reset_leaves_the_session_alone() {
        let api = Arc::new(FakeApi::default());
        *api.reset_confirm_result.lock() =
            Some(Err(AccountError::from_server("invalid_credentials")));
        let (service, store) = service(api);
        credentials::store(
            store.as_ref(),
            CredentialKind::AnnotatorSession,
            "production",
            &SecretToken::new("live-session-token").unwrap(),
        )
        .unwrap();

        assert!(
            service
                .confirm_password_reset("challenge-1", "000000", "a whole new correct horse")
                .is_err()
        );
        assert!(
            credentials::load(
                store.as_ref(),
                CredentialKind::AnnotatorSession,
                "production"
            )
            .unwrap()
            .is_some()
        );
    }

    /// 改密与找回的会话语义刻意不同（docs/34 §6.3/§6.6）：改密撤的是**其他**
    /// 会话，服务端保留当前这份，所以本地 token 与登录态都必须原样留下——
    /// 清掉它等于把刚证明过自己的人踢下线。
    #[test]
    fn a_password_change_keeps_the_local_session() {
        let api = Arc::new(FakeApi::default());
        *api.change_password_result.lock() = Some(Ok(()));
        let (service, store) = service(api);
        credentials::store(
            store.as_ref(),
            CredentialKind::AnnotatorSession,
            "production",
            &SecretToken::new("live-session-token").unwrap(),
        )
        .unwrap();

        service
            .change_password("the old correct horse", "a whole new correct horse")
            .unwrap();

        assert!(
            credentials::load(
                store.as_ref(),
                CredentialKind::AnnotatorSession,
                "production"
            )
            .unwrap()
            .is_some(),
            "the caller's own session survives a password change"
        );
    }

    /// 没有本地会话时改密必须直接报 `session_required`，而不是发一个注定 401 的请求。
    #[test]
    fn a_password_change_without_a_session_fails_locally() {
        let api = Arc::new(FakeApi::default());
        let (service, _store) = service(api);
        let error = service
            .change_password("anything at all here", "a whole new correct horse")
            .unwrap_err();
        assert_eq!(error.code, "session_required");
    }

    #[test]
    fn account_configuration_is_explicit_and_never_falls_back_to_production() {
        assert!(matches!(
            resolve_account_configuration(None, None),
            AccountConfigurationResolution::Unconfigured
        ));
        let AccountConfigurationResolution::Invalid(obsolete) =
            resolve_account_configuration_values(
                None,
                None,
                Some("https://staging-console.example.com"),
            )
        else {
            panic!("the obsolete account base variable must be invalid, not unconfigured");
        };
        assert_eq!(obsolete.code, "account_configuration_obsolete_variable");

        for (environment, base_url, expected_code) in [
            (
                None,
                Some("https://staging-console.example.com"),
                "account_configuration_incomplete",
            ),
            (Some("staging"), None, "account_configuration_incomplete"),
            (Some(""), Some(""), "account_configuration_invalid"),
            (
                Some("stagin"),
                Some("https://staging-console.example.com"),
                "account_environment_invalid",
            ),
            (
                Some("STAGING"),
                Some("https://staging-console.example.com"),
                "account_environment_invalid",
            ),
            (
                Some("staging"),
                Some("https://console.diogenesmodel.com/"),
                "account_environment_conflict",
            ),
            (
                Some("staging"),
                Some("https://console.diogenesmodel.com./"),
                "account_environment_conflict",
            ),
            (
                Some("staging"),
                Some("https://console.diogenesmodel.com/staging-lookalike"),
                "account_environment_conflict",
            ),
            (
                Some("production"),
                Some("https://staging-console.example.com"),
                "account_environment_conflict",
            ),
        ] {
            let AccountConfigurationResolution::Invalid(failure) =
                resolve_account_configuration(environment, base_url)
            else {
                panic!("{environment:?} + {base_url:?} must fail closed");
            };
            assert_eq!(failure.code, expected_code);
        }

        let AccountConfigurationResolution::Ready(production) = resolve_account_configuration(
            Some("production"),
            Some("https://console.diogenesmodel.com/"),
        ) else {
            panic!("an explicit canonical production configuration must be ready");
        };
        assert_eq!(production.environment, AccountEnvironment::Production);
        assert_eq!(production.environment.credential_scope(), "production");
        assert_eq!(production.base_url, "https://console.diogenesmodel.com/");

        let AccountConfigurationResolution::Ready(staging) = resolve_account_configuration(
            Some("staging"),
            Some("https://staging-console.example.com"),
        ) else {
            panic!("an explicit safe staging configuration must be ready");
        };
        assert_eq!(staging.environment, AccountEnvironment::Staging);
        assert_eq!(staging.environment.credential_scope(), "staging");
        assert_eq!(staging.base_url, "https://staging-console.example.com/");
    }

    #[test]
    fn invalid_account_url_keeps_the_real_safe_startup_reason() {
        let AccountConfigurationResolution::Invalid(failure) = resolve_account_configuration(
            Some("staging"),
            Some("http://example.com?token=must-not-be-exported"),
        ) else {
            panic!("plaintext account configuration must fail closed");
        };

        assert_eq!(failure.code, "account_base_url_invalid");
        assert_eq!(failure.reason, "account base URL must be https");
        assert!(failure.message.contains("https"));
        assert!(!failure.reason.contains("must-not-be-exported"));
        assert!(!failure.message.contains("must-not-be-exported"));
    }

    #[test]
    fn invalid_account_startup_records_the_real_safe_reason() {
        let temp = tempfile::tempdir().unwrap();
        let diagnostics = DiagnosticService::open(temp.path()).unwrap();
        let AccountConfigurationResolution::Invalid(failure) = resolve_account_configuration(
            Some("staging"),
            Some("http://example.com?token=must-not-be-exported"),
        ) else {
            panic!("plaintext account configuration must fail closed");
        };

        record_account_startup_failure(&diagnostics, &failure);
        let log = fs::read_to_string(temp.path().join("diagnostics/logs/events.jsonl")).unwrap();
        let event: serde_json::Value = serde_json::from_str(log.trim()).unwrap();
        assert_eq!(event["kind"], "service_startup_degraded");
        assert_eq!(event["data"]["service"], "account");
        assert_eq!(event["data"]["code"], "account_base_url_invalid");
        assert_eq!(event["data"]["reason"], "account base URL must be https");
        assert!(!log.contains("must-not-be-exported"));
        assert!(!log.contains("http://example.com"));
    }

    #[test]
    fn account_user_info_failure_keeps_its_safe_reason_in_diagnostics() {
        let temp = tempfile::tempdir().unwrap();
        let diagnostics = DiagnosticService::open(temp.path()).unwrap();
        let AccountConfigurationResolution::Invalid(failure) = resolve_account_configuration(
            Some("staging"),
            Some("https://alice:secret@staging-console.example.com/"),
        ) else {
            panic!("account URL user info must fail closed");
        };

        assert_eq!(failure.code, "account_base_url_invalid");
        assert_eq!(
            failure.reason,
            "account base URL must not contain user info"
        );
        record_account_startup_failure(&diagnostics, &failure);

        let log = fs::read_to_string(temp.path().join("diagnostics/logs/events.jsonl")).unwrap();
        let event: serde_json::Value = serde_json::from_str(log.trim()).unwrap();
        assert_eq!(
            event["data"]["reason"],
            "account base URL must not contain user info"
        );
        assert!(!log.contains("alice"));
        assert!(!log.contains("secret"));
        assert!(!log.contains("redacted-sensitive-diagnostic"));
    }

    #[test]
    fn a_base_url_must_be_https_and_carry_no_query() {
        assert!(HttpAccountApi::new("https://console.example.com", "0.1.0").is_ok());
        assert!(HttpAccountApi::new("http://console.example.com", "0.1.0").is_err());
        assert!(HttpAccountApi::new("https://console.example.com/?a=1", "0.1.0").is_err());
        assert!(HttpAccountApi::new("https://user:pass@console.example.com", "0.1.0").is_err());
    }

    #[test]
    fn account_http_client_does_not_follow_redirects() {
        let probe = crate::services::test_http::redirect_probe();
        let api = HttpAccountApi::new(&probe.source_url, "0.1.0").unwrap();

        assert!(
            api.terms().is_err(),
            "a redirect must be surfaced as a failure"
        );
        assert!(
            !probe.finish(),
            "account requests must not reach a redirect target"
        );
    }

    /// 服务端的英文运维文案与 requestId 不该出现在界面上，未知码也必须收口。
    #[test]
    fn server_codes_map_to_local_copy() {
        assert_eq!(
            AccountError::from_server("invite_invalid").code,
            "invite_invalid"
        );
        assert_eq!(
            AccountError::from_server("something_new").code,
            "account_request_failed"
        );
        assert!(AccountError::from_server("rate_limited").retryable);
        assert!(!AccountError::from_server("invite_invalid").retryable);
    }

    #[test]
    fn signup_verification_error_keeps_correlation_and_writes_only_safe_diagnostics() {
        let temp = tempfile::tempdir().unwrap();
        let diagnostics = Arc::new(DiagnosticService::open(temp.path()).unwrap());
        let api = Arc::new(FakeApi::default());
        *api.verify_result.lock() = Some(Err(AccountError::from_response(
            "invalid_credentials",
            401,
            Some("req_safe-correlation".into()),
            Some(false),
            None,
        )));
        let store = Arc::new(InMemoryCredentialStore::default());
        let mut service = AccountService::new(api, store, AccountEnvironment::Production);
        service.diagnostics = Some(diagnostics);

        let error = service
            .verify_signup("challenge-must-not-be-logged", "654321")
            .unwrap_err();
        assert_eq!(error.code, "signup_verification_failed");
        assert_eq!(error.http_status, Some(401));
        assert_eq!(error.request_id.as_deref(), Some("req_safe-correlation"));
        assert!(error.message.starts_with("验证码不正确"));

        let log = fs::read_to_string(temp.path().join("diagnostics/logs/events.jsonl")).unwrap();
        let event: serde_json::Value = serde_json::from_str(log.trim()).unwrap();
        assert_eq!(event["kind"], "account_api_request_failed");
        assert_eq!(event["data"]["operation"], "signup_verify");
        assert_eq!(event["data"]["method"], "POST");
        assert_eq!(event["data"]["path"], "/app/v1/signup/verify");
        assert_eq!(event["data"]["http_status"], 401);
        assert_eq!(event["data"]["server_code"], "invalid_credentials");
        assert_eq!(event["data"]["request_id"], "req_safe-correlation");
        assert!(!log.contains("challenge-must-not-be-logged"));
        assert!(!log.contains("654321"));
    }
}

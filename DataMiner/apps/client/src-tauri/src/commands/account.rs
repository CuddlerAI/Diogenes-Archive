//! 账号与设备 enroll 命令面（`docs/11` §7）。
//!
//! 每个命令都是 `async fn` + `spawn_blocking`：账号服务用的是 blocking reqwest，
//! 在 Tauri 的 async 上下文里直接调会 panic（`catalog_delivery` 已经踩过这个坑）。
//!
//! 两条纪律与 [`super::credentials`] 一致：
//! - 含明文的请求结构体**不** derive `Debug`/`Serialize`，让密码只能单向流向 HTTP body；
//! - 返回给前端的东西**永远不含 token**，只含身份与状态。

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, State};

use crate::app_state::AppState;
use crate::platform::windows::input_devices::collect_input_devices_snapshot;
use crate::services::account::{
    AccountAvailability, AccountDevice, AccountEnvironment, AccountError, AccountIdentity,
    AccountProfile, ConsentState, DeviceEnrollmentRequest, EnrollOutcome, ProfileUpdate,
    SignupChallenge, Terms, load_or_create_client_device_id,
};
use crate::services::consent_watch::ConsentNoticeSnapshot;
use crate::services::credentials::{self, CredentialKind};

/// 注册请求。明文密码与邀请码都在这里，所以整个结构体不可打印、不可序列化。
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SignupRequest {
    pub invite_code: String,
    pub email: String,
    pub password: String,
    pub alias: String,
    pub accepted_policy_version: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LoginRequest {
    pub email: String,
    pub password: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct VerifyRequest {
    pub challenge_id: String,
    pub code: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResendRequest {
    pub challenge_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConsentRenewalRequest {
    pub accepted_policy_version: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PasswordResetRequest {
    pub email: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PasswordResetConfirmRequest {
    pub challenge_id: String,
    pub code: String,
    pub password: String,
}

/// 登录态改密（docs/34 §6.3）。两个明文密码都在这里，所以与其余含密结构体
/// 同一纪律：不 derive `Debug`/`Serialize`，只能单向流向 HTTP body。
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ChangePasswordRequest {
    pub current_password: String,
    pub new_password: String,
}

/// 资料更新。字段缺席 = 清空（服务端整体替换语义）；IM 渠道与帐号必须成对，
/// 不成对的提交在这里收拢成「都不带」，与服务端约束一致而不是撞它的 400。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UpdateProfileRequest {
    pub display_name: Option<String>,
    pub phone: Option<String>,
    pub im_kind: Option<String>,
    pub im_handle: Option<String>,
    pub region: Option<String>,
}

impl UpdateProfileRequest {
    fn into_update(self) -> ProfileUpdate {
        let clean = |value: Option<String>| {
            value
                .map(|raw| raw.trim().to_string())
                .filter(|trimmed| !trimmed.is_empty())
        };
        let im_kind = clean(self.im_kind);
        let im_handle = clean(self.im_handle);
        let im_pair = im_kind.zip(im_handle);
        ProfileUpdate {
            display_name: clean(self.display_name),
            phone: clean(self.phone),
            im_kind: im_pair.as_ref().map(|(kind, _)| kind.clone()),
            im_handle: im_pair.map(|(_, handle)| handle),
            region: clean(self.region),
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceEnrollmentStatus {
    pub schema_version: &'static str,
    pub client_device_id: String,
    pub device_credential_present: bool,
    pub devices: Vec<AccountDevice>,
}

// -3：设备登记状态不再暴露 catalog/profile 选择面，只描述本机身份与凭据。
const DEVICE_ENROLLMENT_STATUS_SCHEMA: &str = "dm-device-enrollment-status-3";

/// 前端渲染账号面所需的全部状态。刻意不含 token，也不含邮箱——
/// 邮箱由服务端持有，界面上要显示时再单独取。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountStatus {
    pub schema_version: &'static str,
    pub availability: AccountAvailability,
    pub identity: AccountIdentity,
}

const ACCOUNT_STATUS_SCHEMA: &str = "dm-account-status-2";

impl AccountStatus {
    fn new(availability: AccountAvailability, identity: AccountIdentity) -> Self {
        Self {
            schema_version: ACCOUNT_STATUS_SCHEMA,
            availability,
            identity,
        }
    }
}

fn account_status(state: &AppState, identity: AccountIdentity) -> AccountStatus {
    AccountStatus::new(state.account_availability.clone(), identity)
}

fn account_unavailable_error(state: &AppState) -> AccountError {
    match &state.account_availability {
        AccountAvailability::Invalid { code, message } => {
            AccountError::configuration_invalid(code, message)
        }
        AccountAvailability::Unconfigured | AccountAvailability::Ready { .. } => {
            AccountError::unconfigured()
        }
    }
}

/// 命令层的错误。`AccountError` 已经是给人看的形状，直接透传。
fn to_error(error: AccountError) -> AccountError {
    error
}

/// 纯本地读，没有网络：供界面首帧立刻拿到状态。
#[tauri::command]
pub fn account_get_status(state: State<'_, AppState>) -> AccountStatus {
    let identity = state
        .account
        .as_ref()
        .map_or(AccountIdentity::SignedOut, |account| account.identity());
    account_status(&state, identity)
}

#[tauri::command]
pub async fn account_refresh_session(
    state: State<'_, AppState>,
) -> Result<AccountStatus, AccountError> {
    let Some(account) = state.account.clone() else {
        return Ok(account_status(&state, AccountIdentity::SignedOut));
    };
    let identity = tauri::async_runtime::spawn_blocking(move || account.refresh_identity())
        .await
        .map_err(|_| AccountError::unconfigured())?
        .map_err(to_error)?;
    Ok(account_status(&state, identity))
}

/// 当前 consent 状态。界面据此判断是否要提示补签。
#[tauri::command]
pub async fn account_get_consent_state(
    state: State<'_, AppState>,
) -> Result<ConsentState, AccountError> {
    let Some(account) = state.account.clone() else {
        return Err(account_unavailable_error(&state));
    };
    tauri::async_runtime::spawn_blocking(move || account.consent_state())
        .await
        .map_err(|_| AccountError::unconfigured())?
        .map_err(to_error)
}

/// 观察器最近一次查到的条款状态（docs/108 §3.6）。
///
/// 与 `account_get_consent_state` 的区别是它**不发请求**：主界面在每次渲染时都要
/// 知道该不该挂横幅，而那不该变成一串网络请求。真正的查询由后台轮询做，这里只读
/// 它的快照。
#[tauri::command]
pub fn account_get_consent_notice(state: State<'_, AppState>) -> ConsentNoticeSnapshot {
    state.consent_watcher.snapshot()
}

/// 立即查一次并刷新快照。登录成功、补签完成之后调用——那两个时刻的状态一定变了，
/// 等下一次 6 小时轮询会让界面停在一个已经不真的提示上。
#[tauri::command]
pub async fn account_refresh_consent_notice(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<ConsentNoticeSnapshot, AccountError> {
    // 先把 Arc 取出来：`State` 不是 Send，跨 await 持有它编译不过，而 await 之后
    // 还要读一次快照。
    let watcher = Arc::clone(&state.consent_watcher);
    let checker = Arc::clone(&watcher);
    let handle = app.clone();
    tauri::async_runtime::spawn_blocking(move || checker.check(&handle))
        .await
        .map_err(|_| AccountError::unconfigured())?;
    Ok(watcher.snapshot())
}

#[tauri::command]
pub async fn account_renew_consent(
    state: State<'_, AppState>,
    request: ConsentRenewalRequest,
) -> Result<(), AccountError> {
    let Some(account) = state.account.clone() else {
        return Err(account_unavailable_error(&state));
    };
    tauri::async_runtime::spawn_blocking(move || {
        account.renew_consent(&request.accepted_policy_version)
    })
    .await
    .map_err(|_| AccountError::unconfigured())?
    .map_err(to_error)
}

#[tauri::command]
pub async fn account_get_terms(state: State<'_, AppState>) -> Result<Terms, AccountError> {
    let Some(account) = state.account.clone() else {
        return Err(account_unavailable_error(&state));
    };
    tauri::async_runtime::spawn_blocking(move || account.terms())
        .await
        .map_err(|_| AccountError::unconfigured())?
        .map_err(to_error)
}

#[tauri::command]
pub async fn account_signup(
    state: State<'_, AppState>,
    request: SignupRequest,
) -> Result<SignupChallenge, AccountError> {
    let Some(account) = state.account.clone() else {
        return Err(account_unavailable_error(&state));
    };
    tauri::async_runtime::spawn_blocking(move || {
        account.signup(
            &request.invite_code,
            &request.email,
            &request.password,
            &request.alias,
            &request.accepted_policy_version,
        )
    })
    .await
    .map_err(|_| AccountError::unconfigured())?
    .map_err(to_error)
}

#[tauri::command]
pub async fn account_verify_signup(
    state: State<'_, AppState>,
    request: VerifyRequest,
) -> Result<(), AccountError> {
    let Some(account) = state.account.clone() else {
        return Err(account_unavailable_error(&state));
    };
    tauri::async_runtime::spawn_blocking(move || {
        account.verify_signup(&request.challenge_id, &request.code)
    })
    .await
    .map_err(|_| AccountError::unconfigured())?
    .map_err(to_error)
}

#[tauri::command]
pub async fn account_resend_signup_code(
    state: State<'_, AppState>,
    request: ResendRequest,
) -> Result<SignupChallenge, AccountError> {
    let Some(account) = state.account.clone() else {
        return Err(account_unavailable_error(&state));
    };
    tauri::async_runtime::spawn_blocking(move || account.resend_signup_code(&request.challenge_id))
        .await
        .map_err(|_| AccountError::unconfigured())?
        .map_err(to_error)
}

/// 找回密码第一步。返回的 challenge 对「有账号」和「没账号」是同一个形状——
/// 界面必须照此措辞，不能说成「验证码已发送到该账号」。
#[tauri::command]
pub async fn account_request_password_reset(
    state: State<'_, AppState>,
    request: PasswordResetRequest,
) -> Result<SignupChallenge, AccountError> {
    let Some(account) = state.account.clone() else {
        return Err(account_unavailable_error(&state));
    };
    tauri::async_runtime::spawn_blocking(move || account.request_password_reset(&request.email))
        .await
        .map_err(|_| AccountError::unconfigured())?
        .map_err(to_error)
}

#[tauri::command]
pub async fn account_confirm_password_reset(
    state: State<'_, AppState>,
    request: PasswordResetConfirmRequest,
) -> Result<(), AccountError> {
    let Some(account) = state.account.clone() else {
        return Err(account_unavailable_error(&state));
    };
    tauri::async_runtime::spawn_blocking(move || {
        account.confirm_password_reset(&request.challenge_id, &request.code, &request.password)
    })
    .await
    .map_err(|_| AccountError::unconfigured())?
    .map_err(to_error)
}

#[tauri::command]
pub async fn account_login(
    state: State<'_, AppState>,
    request: LoginRequest,
) -> Result<AccountStatus, AccountError> {
    let Some(account) = state.account.clone() else {
        return Err(account_unavailable_error(&state));
    };
    let identity = tauri::async_runtime::spawn_blocking(move || {
        account.login(&request.email, &request.password)
    })
    .await
    .map_err(|_| AccountError::unconfigured())?
    .map_err(to_error)?;
    // 登录本身就解锁了一条新的下发通道：选择游戏前的 catalog 启动用的正是这个会话
    // （#323）。不在这里唤醒，用户就要等到下一个
    // 十分钟周期——而屏幕上没有任何东西告诉他该等。
    state.wake_catalog_sync();
    Ok(account_status(&state, identity))
}

#[tauri::command]
pub async fn account_logout(state: State<'_, AppState>) -> Result<AccountStatus, AccountError> {
    let Some(account) = state.account.clone() else {
        return Ok(account_status(&state, AccountIdentity::SignedOut));
    };
    let identity = tauri::async_runtime::spawn_blocking(move || account.logout())
        .await
        .map_err(|_| AccountError::unconfigured())?;
    Ok(account_status(&state, identity))
}

#[tauri::command]
pub async fn account_change_password(
    state: State<'_, AppState>,
    request: ChangePasswordRequest,
) -> Result<(), AccountError> {
    let Some(account) = state.account.clone() else {
        return Err(account_unavailable_error(&state));
    };
    tauri::async_runtime::spawn_blocking(move || {
        account.change_password(&request.current_password, &request.new_password)
    })
    .await
    .map_err(|_| AccountError::unconfigured())?
    .map_err(to_error)
}

#[tauri::command]
pub async fn account_get_profile(
    state: State<'_, AppState>,
) -> Result<Option<AccountProfile>, AccountError> {
    let Some(account) = state.account.clone() else {
        return Err(account_unavailable_error(&state));
    };
    tauri::async_runtime::spawn_blocking(move || account.get_profile())
        .await
        .map_err(|_| AccountError::unconfigured())?
        .map_err(to_error)
}

#[tauri::command]
pub async fn account_update_profile(
    state: State<'_, AppState>,
    request: UpdateProfileRequest,
) -> Result<Option<AccountProfile>, AccountError> {
    let Some(account) = state.account.clone() else {
        return Err(account_unavailable_error(&state));
    };
    let update = request.into_update();
    tauri::async_runtime::spawn_blocking(move || account.update_profile(&update))
        .await
        .map_err(|_| AccountError::unconfigured())?
        .map_err(to_error)
}

#[tauri::command]
pub async fn account_get_device_enrollment_status(
    state: State<'_, AppState>,
) -> Result<DeviceEnrollmentStatus, AccountError> {
    let Some(account) = state.account.clone() else {
        return Err(account_unavailable_error(&state));
    };
    let orchestrator = state.orchestrator.clone();
    let store = state.credential_store.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let client_device_id = load_or_create_client_device_id(&orchestrator.app_data_root())
            .map_err(|_| {
                AccountError::local(
                    "ACCOUNT_ENROLL_CLIENT_DEVICE_ID_FAILED",
                    "无法读取或创建设备安装标识。",
                    false,
                )
            })?;
        let device_credential_present = credentials::load(
            store.as_ref(),
            CredentialKind::Device,
            account.credential_scope(),
        )
        .map_err(|_| {
            AccountError::local(
                "credential_store_failed",
                "无法读取设备凭据状态，请重试。",
                true,
            )
        })?
        .is_some();
        Ok(DeviceEnrollmentStatus {
            schema_version: DEVICE_ENROLLMENT_STATUS_SCHEMA,
            client_device_id,
            device_credential_present,
            devices: account.list_devices()?,
        })
    })
    .await
    .map_err(|_| {
        AccountError::local(
            "ACCOUNT_ENROLL_TASK_FAILED",
            "读取设备登记状态失败，请重试。",
            true,
        )
    })?
}

#[tauri::command]
pub async fn account_enroll_device(
    state: State<'_, AppState>,
) -> Result<EnrollOutcome, AccountError> {
    let Some(account) = state.account.clone() else {
        return Err(account_unavailable_error(&state));
    };
    let account_environment = account.environment();
    let orchestrator = state.orchestrator.clone();
    let outcome = tauri::async_runtime::spawn_blocking(move || {
        let client_device_id = load_or_create_client_device_id(&orchestrator.app_data_root())
            .map_err(|_| {
                AccountError::local(
                    "ACCOUNT_ENROLL_CLIENT_DEVICE_ID_FAILED",
                    "无法读取或创建设备安装标识。",
                    false,
                )
            })?;
        let windows = capture_core::platform::WindowsVersion::current().map_err(|_| {
            AccountError::local(
                "ACCOUNT_ENROLL_WINDOWS_SNAPSHOT_FAILED",
                "无法读取 Windows 版本，设备登记已停止。",
                false,
            )
        })?;
        let gpu = capture_core::device_snapshot::collect_nvidia_gpu_snapshot().map_err(|_| {
            AccountError::local(
                "ACCOUNT_ENROLL_GPU_SNAPSHOT_FAILED",
                "无法读取 NVIDIA GPU 与驱动版本，设备登记已停止。",
                false,
            )
        })?;
        let display_snapshot =
            capture_core::device_snapshot::collect_display_snapshot().map_err(|_| {
                AccountError::local(
                    "ACCOUNT_ENROLL_DISPLAY_SNAPSHOT_FAILED",
                    "无法读取显示器几何信息，设备登记已停止。",
                    false,
                )
            })?;
        let input_devices_snapshot = collect_input_devices_snapshot().map_err(|_| {
            AccountError::local(
                "ACCOUNT_ENROLL_INPUT_SNAPSHOT_FAILED",
                "无法读取输入设备数量，设备登记已停止。",
                false,
            )
        })?;
        let outcome = account.enroll_device(DeviceEnrollmentRequest::new(
            client_device_id,
            format!("{}.{}.{}", windows.major, windows.minor, windows.build),
            gpu,
            env!("CARGO_PKG_VERSION").to_string(),
            display_snapshot,
            input_devices_snapshot,
        ))?;
        Ok(outcome)
    })
    .await
    .map_err(|_| {
        AccountError::local(
            "ACCOUNT_ENROLL_TASK_FAILED",
            "设备登记任务异常终止，请重试。",
            true,
        )
    })??;
    if outcome.credential_issued {
        // uploader 属于尚未纳入本次账号环境选择的生产摄取链。STAGING 登记绝不能
        // 借机唤醒它；只唤醒已经按账号环境冻结配置和凭据槽位的两个消费者。
        if should_wake_uploader_after_enrollment(account_environment) {
            state.wake_uploader();
        }
        state.wake_catalog_sync();
        state.wake_client_events();
    }
    Ok(outcome)
}

fn should_wake_uploader_after_enrollment(environment: AccountEnvironment) -> bool {
    environment == AccountEnvironment::Production
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::account::AccountEnvironment;

    #[test]
    fn account_status_distinguishes_unconfigured_invalid_and_ready() {
        let unconfigured = serde_json::to_value(AccountStatus::new(
            AccountAvailability::Unconfigured,
            AccountIdentity::SignedOut,
        ))
        .unwrap();
        let invalid = serde_json::to_value(AccountStatus::new(
            AccountAvailability::Invalid {
                code: "account_base_url_invalid".into(),
                message: "账号服务地址必须使用 https。".into(),
            },
            AccountIdentity::SignedOut,
        ))
        .unwrap();
        let ready = serde_json::to_value(AccountStatus::new(
            AccountAvailability::Ready {
                environment: AccountEnvironment::Staging,
            },
            AccountIdentity::SignedOut,
        ))
        .unwrap();

        assert_eq!(unconfigured["schemaVersion"], "dm-account-status-2");
        assert_eq!(unconfigured["availability"]["kind"], "unconfigured");
        assert_eq!(invalid["availability"]["kind"], "invalid");
        assert_eq!(invalid["availability"]["code"], "account_base_url_invalid");
        assert_eq!(
            invalid["availability"]["message"],
            "账号服务地址必须使用 https。"
        );
        assert_eq!(ready["availability"]["kind"], "ready");
        assert_eq!(ready["availability"]["environment"], "staging");
        assert!(unconfigured.get("configured").is_none());
        assert!(invalid.get("configured").is_none());
        assert!(ready.get("configured").is_none());
    }

    #[test]
    fn only_production_enrollment_wakes_the_production_uploader() {
        assert!(!should_wake_uploader_after_enrollment(
            AccountEnvironment::Staging
        ));
        assert!(should_wake_uploader_after_enrollment(
            AccountEnvironment::Production
        ));
    }

    #[test]
    fn device_enrollment_status_has_no_profile_selection_contract() {
        let value = serde_json::to_value(DeviceEnrollmentStatus {
            schema_version: DEVICE_ENROLLMENT_STATUS_SCHEMA,
            client_device_id: "dmc-test".into(),
            device_credential_present: false,
            devices: Vec::new(),
        })
        .unwrap();

        assert_eq!(value["schemaVersion"], "dm-device-enrollment-status-3");
        let object = value.as_object().unwrap();
        assert!(!object.contains_key("deviceProfiles"));
        assert!(!object.contains_key("catalogSync"));
    }
}

use serde::{Deserialize, Serialize};
use tauri::State;

use crate::app_state::AppState;
use crate::services::client_events;
use crate::services::credentials::{self, CredentialKind, DeviceCredential};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CredentialScopeRequest {
    pub environment: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StoreDeviceCredentialRequest {
    pub environment: String,
    // 故意不 derive Debug/Serialize：这段明文只允许向 Credential Manager 单向移动。
    pub credential: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceCredentialStatus {
    pub environment: String,
    pub configured: bool,
}

#[tauri::command]
pub fn client_get_device_credential_status(
    state: State<'_, AppState>,
    request: CredentialScopeRequest,
) -> Result<DeviceCredentialStatus, String> {
    ensure_production_scope(&request.environment)?;
    let configured = credentials::load(
        state.credential_store.as_ref(),
        CredentialKind::Device,
        &request.environment,
    )
    .map_err(command_error)?
    .is_some();
    Ok(DeviceCredentialStatus {
        environment: request.environment,
        configured,
    })
}

#[tauri::command]
pub fn client_store_device_credential(
    state: State<'_, AppState>,
    request: StoreDeviceCredentialRequest,
) -> Result<DeviceCredentialStatus, String> {
    ensure_production_scope(&request.environment)?;
    let credential = DeviceCredential::new(request.credential).map_err(command_error)?;
    credentials::store(
        state.credential_store.as_ref(),
        CredentialKind::Device,
        &request.environment,
        &credential,
    )
    .map_err(command_error)?;
    state.wake_uploader();
    // 凭据落位即视为完成 enroll：给仓库记一条 enrolled，并唤醒发送器把
    // 攒下的事件（app_started 等）第一时间送出去。
    if let Some(recorder) = state.orchestrator.client_event_recorder() {
        recorder.record(
            "enrolled",
            "info",
            serde_json::json!({ "schema_version": client_events::ENROLLED_SCHEMA }),
        );
    }
    state.wake_client_events();
    Ok(DeviceCredentialStatus {
        environment: request.environment,
        configured: true,
    })
}

#[tauri::command]
pub fn client_delete_device_credential(
    state: State<'_, AppState>,
    request: CredentialScopeRequest,
) -> Result<DeviceCredentialStatus, String> {
    ensure_production_scope(&request.environment)?;
    credentials::remove(
        state.credential_store.as_ref(),
        CredentialKind::Device,
        &request.environment,
    )
    .map_err(command_error)?;
    Ok(DeviceCredentialStatus {
        environment: request.environment,
        configured: false,
    })
}

fn command_error(error: anyhow::Error) -> String {
    error.to_string()
}

fn ensure_production_scope(environment: &str) -> Result<(), String> {
    if environment != credentials::PRODUCTION_CREDENTIAL_SCOPE {
        return Err("only the production device credential is supported".into());
    }
    Ok(())
}

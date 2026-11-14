use std::sync::Arc;

use serde::Deserialize;
use tauri::State;

use super::depth_lab::LOCAL_CP2077_GAME_ID;
use crate::app_state::AppState;
use crate::cp2077_raw_demo::MainCollectorPurpose;
use crate::services::cp2077_trial::{
    TrialStatus, open_output, prepare_and_launch, start_recording, status, stop_recording,
};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TrialPrepareRequest {
    risk_acknowledged: bool,
    #[serde(default)]
    purpose: MainCollectorPurpose,
}

#[tauri::command]
pub fn cp2077_trial_get_status(game_id: String) -> TrialStatus {
    let mut current = status();
    current.available &= game_id == LOCAL_CP2077_GAME_ID;
    current
}

#[tauri::command]
pub async fn cp2077_trial_prepare_and_launch(
    state: State<'_, AppState>,
    request: TrialPrepareRequest,
) -> Result<TrialStatus, String> {
    let orchestrator = Arc::clone(&state.orchestrator);
    tauri::async_runtime::spawn_blocking(move || {
        let collector_state = orchestrator.snapshot().state;
        prepare_and_launch(request.risk_acknowledged, collector_state, request.purpose)
    })
    .await
    .map_err(|error| format!("CP2077 trial preparation task failed: {error}"))?
}

#[tauri::command]
pub fn cp2077_trial_start_recording() -> Result<TrialStatus, String> {
    start_recording()
}

#[tauri::command]
pub fn cp2077_trial_stop_recording() -> Result<TrialStatus, String> {
    stop_recording()
}

#[tauri::command]
pub fn cp2077_trial_open_output() -> Result<(), String> {
    open_output()
}

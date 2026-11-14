//! 视角标定命令面（docs/46 §4.1）：热键与 UI 按钮共用服务入口，命令层
//! 只做转发与错误字符串化，不携带任何业务判断。

use tauri::State;

use crate::app_state::AppState;
use crate::services::view_scale::ViewScaleStatus;

fn command_error(error: anyhow::Error) -> String {
    format!("{error:#}")
}

#[tauri::command]
pub fn view_scale_get_status(state: State<'_, AppState>) -> ViewScaleStatus {
    state.view_scale.status()
}

#[tauri::command]
pub fn view_scale_begin_session(
    state: State<'_, AppState>,
    kind: String,
) -> Result<ViewScaleStatus, String> {
    state.view_scale.begin_session(&kind).map_err(command_error)
}

#[tauri::command]
pub fn view_scale_toggle_measurement(
    state: State<'_, AppState>,
) -> Result<ViewScaleStatus, String> {
    state.view_scale.toggle_measurement().map_err(command_error)
}

#[tauri::command]
pub fn view_scale_abort_measurement(state: State<'_, AppState>) -> Result<ViewScaleStatus, String> {
    state.view_scale.abort_measurement().map_err(command_error)
}

#[tauri::command]
pub fn view_scale_cancel_session(state: State<'_, AppState>) -> Result<ViewScaleStatus, String> {
    state.view_scale.cancel_session().map_err(command_error)
}

#[tauri::command]
pub fn view_scale_finish_session(state: State<'_, AppState>) -> Result<ViewScaleStatus, String> {
    state.view_scale.finish_session().map_err(command_error)
}

#[tauri::command]
pub fn view_scale_list_history(
    state: State<'_, AppState>,
) -> Result<Vec<crate::services::journal::ViewCalibrationAudit>, String> {
    state.view_scale.history(50).map_err(command_error)
}

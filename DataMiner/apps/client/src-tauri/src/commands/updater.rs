// 前端不直连 tauri-plugin-updater 的原始 API（docs/15 裁决 D3）：capabilities
// 不授予任何 updater:* 权限，所有更新交互只经过这里的包装命令，节流、状态机
// 与诊断留痕都在 Rust 侧强制。
use std::sync::Arc;

use tauri::{AppHandle, Manager, Wry};

use crate::app_state::AppState;
use crate::services::updater::{CheckTrigger, UpdateStatusSnapshot};

#[tauri::command]
pub fn client_get_update_status(state: tauri::State<'_, AppState>) -> UpdateStatusSnapshot {
    state.updater.snapshot()
}

#[tauri::command]
pub async fn client_check_for_update(app: AppHandle<Wry>) -> UpdateStatusSnapshot {
    let checker = Arc::clone(&app.state::<AppState>().updater);
    checker.check(&app, CheckTrigger::Manual).await
}

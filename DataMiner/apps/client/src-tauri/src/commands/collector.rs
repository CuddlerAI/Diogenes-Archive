use std::sync::Arc;
use std::time::Instant;

use capture_core::P0Status;
use tauri::State;

use crate::app_state::AppState;
use crate::domain::collector::{
    AddMarkerRequest, BindWindowRequest, CollectorFault, CollectorSnapshot, DeleteSessionRequest,
    DiscoverWindowsRequest, GameSummary, MarkerAudit, OverlayAction, OverlaySnapshot,
    SessionIdRequest, StartSessionRequest, StopSessionRequest, WindowCandidate,
};
use crate::services::capture_storage::CaptureStorageStatus;
use crate::services::diagnostics::DiagnosticExportResult;
use crate::services::journal::{DataModelJobStatus, SessionHistoryDetail, SessionHistorySummary};
use crate::services::session_history::SessionDeletionResult;

#[tauri::command]
pub fn collector_get_p0_status() -> P0Status {
    P0Status::current()
}

#[tauri::command]
pub fn collector_get_snapshot(state: State<'_, AppState>) -> CollectorSnapshot {
    // 启动期通知只在这里代发：前端先挂监听再发首次 snapshot（main.tsx 保证顺序），
    // 这条命令到来即证明监听已就位。
    state.orchestrator.deliver_startup_notifications();
    state.orchestrator.snapshot()
}

#[tauri::command]
pub fn collector_get_capture_storage_status(
    state: State<'_, AppState>,
) -> Result<CaptureStorageStatus, String> {
    state
        .orchestrator
        .capture_storage_status()
        .map_err(command_error)
}

#[tauri::command]
pub async fn collector_choose_capture_storage_location(
    state: State<'_, AppState>,
) -> Result<CaptureStorageStatus, String> {
    let orchestrator = Arc::clone(&state.orchestrator);
    run_blocking_command("choose_capture_storage", move || {
        orchestrator.choose_capture_storage_location()
    })
    .await
}

// local-catalog-only 构建的准入在 window_tracker.rs 的 games() 里一次成型：
// 该口味离线自权威，每个内嵌条目构造时即 Admitted。此前这里还有一层按名单
// 二次直批的覆盖（LOCAL_NON_TRAINABLE_GAME_IDS），但它面对的输入永远没有
// Unknown 条目——自 games() 打上口味级 Admitted 起就是运行时死代码，随
// 逐游戏特性断代一并删除。
#[cfg(feature = "local-catalog-only")]
#[tauri::command]
pub async fn collector_list_games(state: State<'_, AppState>) -> Result<Vec<GameSummary>, String> {
    Ok(state.orchestrator.games())
}

#[cfg(not(feature = "local-catalog-only"))]
#[tauri::command]
pub async fn collector_list_games(state: State<'_, AppState>) -> Result<Vec<GameSummary>, String> {
    let fallback = state.orchestrator.games();
    #[cfg(feature = "local-catalog-only")]
    {
        // This build flavor is deliberately offline and non-production. The
        // compiled catalog is the only authority for local selection; querying
        // production admission would both violate that isolation and turn a
        // local D7 entry into `game_not_registered`.
        Ok(fallback)
    }
    #[cfg(not(feature = "local-catalog-only"))]
    {
        let pending = fallback.clone();
        let credential_store = Arc::clone(&state.credential_store);
        match tauri::async_runtime::spawn_blocking(move || {
        let mut games = pending;
        if let Err(error) = crate::services::game_admission::overlay_server_admission(
            &mut games,
            credential_store.as_ref(),
        ) {
            // Admission is an operator aid, not an authority boundary.  The API
            // still fails closed and the frontend has the shipped fallback.
            tracing::warn!(error = %error, "live game admission is unavailable; using fallback state");
        }
        games
    })
    .await
    {
        Ok(games) => Ok(games),
        Err(error) => {
            tracing::warn!(error = %error, "game admission task failed; using catalog roster");
            Ok(fallback)
        }
    }
    }
}

#[tauri::command]
pub async fn collector_list_window_candidates(
    state: State<'_, AppState>,
    request: DiscoverWindowsRequest,
) -> Result<Vec<WindowCandidate>, String> {
    let orchestrator = Arc::clone(&state.orchestrator);
    let game_id = request.game_id;
    run_blocking_command("discover", move || orchestrator.discover(&game_id)).await
}

#[tauri::command]
pub async fn collector_bind_window(
    state: State<'_, AppState>,
    request: BindWindowRequest,
) -> Result<CollectorSnapshot, String> {
    let orchestrator = Arc::clone(&state.orchestrator);
    let candidate_id = request.candidate_id;
    let snapshot = run_blocking_command("bind", move || orchestrator.bind(&candidate_id)).await?;
    #[cfg(feature = "depth-lab")]
    if let Some(target) = snapshot.bound_target.as_ref() {
        crate::services::depth_lab::observe_cp2077_discovery_binding(
            &target.game_id,
            target.process_id,
            target.executable_sha256.as_deref(),
        )?;
    }
    Ok(snapshot)
}

#[tauri::command]
pub async fn collector_start_session(
    state: State<'_, AppState>,
    request: StartSessionRequest,
) -> Result<CollectorSnapshot, String> {
    let orchestrator = Arc::clone(&state.orchestrator);
    let request_id = request.request_id;
    run_blocking_command("start", move || orchestrator.start(&request_id)).await
}

#[tauri::command]
pub fn collector_stop(
    state: State<'_, AppState>,
    request: StopSessionRequest,
) -> Result<CollectorSnapshot, String> {
    state
        .orchestrator
        .stop(&request.reason)
        .map_err(command_error)
}

#[tauri::command]
pub fn collector_pause(
    state: State<'_, AppState>,
    request: StopSessionRequest,
) -> Result<CollectorSnapshot, String> {
    let _ = request.reason;
    state.orchestrator.pause(None).map_err(command_error)
}

#[tauri::command]
pub async fn collector_resume(
    state: State<'_, AppState>,
    request: StartSessionRequest,
) -> Result<CollectorSnapshot, String> {
    let orchestrator = Arc::clone(&state.orchestrator);
    let request_id = request.request_id;
    run_blocking_command("resume", move || orchestrator.resume(&request_id)).await
}

#[tauri::command]
pub fn collector_add_marker(
    state: State<'_, AppState>,
    request: AddMarkerRequest,
) -> Result<MarkerAudit, CollectorFault> {
    state
        .orchestrator
        .add_marker(request.kind, &request.request_id, None)
}

#[tauri::command]
pub fn collector_get_overlay_snapshot(state: State<'_, AppState>) -> OverlaySnapshot {
    state.orchestrator.overlay_snapshot()
}

#[tauri::command]
pub fn collector_export_diagnostics(
    state: State<'_, AppState>,
    session_id: Option<String>,
) -> Result<DiagnosticExportResult, CollectorFault> {
    state.orchestrator.export_diagnostics(session_id.as_deref())
}

#[tauri::command]
pub fn collector_list_session_history(
    state: State<'_, AppState>,
) -> Result<Vec<SessionHistorySummary>, CollectorFault> {
    state.orchestrator.session_history()
}

#[tauri::command]
pub fn collector_request_session_upload(
    state: State<'_, AppState>,
    request: SessionIdRequest,
) -> Result<SessionHistorySummary, CollectorFault> {
    let summary = state
        .orchestrator
        .request_session_upload(&request.session_id)?;
    state.wake_uploader();
    Ok(summary)
}

#[tauri::command]
pub fn collector_get_session_history_detail(
    state: State<'_, AppState>,
    request: SessionIdRequest,
) -> Result<SessionHistoryDetail, CollectorFault> {
    state
        .orchestrator
        .session_history_detail(&request.session_id)
}

#[tauri::command]
pub fn collector_get_data_model_status(
    state: State<'_, AppState>,
    request: SessionIdRequest,
) -> Result<Option<DataModelJobStatus>, CollectorFault> {
    state.orchestrator.data_model_status(&request.session_id)
}

#[tauri::command]
pub fn collector_open_session_directory(
    state: State<'_, AppState>,
    request: SessionIdRequest,
) -> Result<(), CollectorFault> {
    state
        .orchestrator
        .open_session_directory(&request.session_id)
}

#[tauri::command]
pub fn collector_open_data_model_directory(
    state: State<'_, AppState>,
    request: SessionIdRequest,
) -> Result<(), CollectorFault> {
    state
        .orchestrator
        .open_data_model_directory(&request.session_id)
}

#[tauri::command]
pub fn collector_delete_session(
    state: State<'_, AppState>,
    request: DeleteSessionRequest,
) -> Result<SessionDeletionResult, CollectorFault> {
    state
        .orchestrator
        .delete_session(&request.session_id, &request.confirmation)
}

#[tauri::command]
pub fn collector_set_overlay_visible(
    state: State<'_, AppState>,
    visible: bool,
) -> CollectorSnapshot {
    state.orchestrator.set_overlay_visible(visible)
}

#[tauri::command]
pub fn collector_overlay_ready(state: State<'_, AppState>) -> Result<OverlaySnapshot, String> {
    state
        .orchestrator
        .overlay_webview_ready()
        .map_err(command_error)
}

#[tauri::command]
pub fn collector_overlay_heartbeat(state: State<'_, AppState>) {
    state.orchestrator.overlay_heartbeat();
}

#[tauri::command]
pub fn collector_overlay_report_fault(state: State<'_, AppState>) {
    state.orchestrator.report_overlay_client_fault();
}

#[tauri::command]
pub async fn collector_route_overlay_action(
    state: State<'_, AppState>,
    action: OverlayAction,
) -> Result<CollectorSnapshot, String> {
    let orchestrator = Arc::clone(&state.orchestrator);
    run_blocking_command("overlay_action", move || {
        orchestrator.route_overlay_action(action)
    })
    .await
}

pub(super) async fn run_blocking_command<T>(
    operation: &'static str,
    task: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) -> Result<T, String>
where
    T: Send + 'static,
{
    let queued_at = Instant::now();
    tauri::async_runtime::spawn_blocking(move || {
        let queue_wait_ms = elapsed_ms(queued_at);
        let started_at = Instant::now();
        let result = task();
        tracing::info!(
            operation,
            queue_wait_ms,
            execution_ms = elapsed_ms(started_at),
            success = result.is_ok(),
            "collector blocking command completed"
        );
        result
    })
    .await
    .map_err(|error| format!("COLLECTOR_BLOCKING_TASK_FAILED:{operation}:{error}"))?
    .map_err(command_error)
}

fn elapsed_ms(started_at: Instant) -> u64 {
    u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn command_error(error: anyhow::Error) -> String {
    error.to_string()
}

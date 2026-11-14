use std::sync::Arc;

use crate::app_state::AppState;
use crate::domain::collector::{CollectorSnapshot, StartSessionRequest};
use crate::services::depth_lab::{
    DepthProbeRuntimeStatus, DepthProbeSettings, EarlyLaunchPreflight, EarlyLaunchResult,
    PerfCertificationGateStatus, PerfCertificationStatus, PerfStageMeasurementRequest,
    begin_perf_certification, cancel_perf_certification, configure, launch_configured_cp2077,
    perf_certification_status, preflight_configured_cp2077, record_perf_stage, runtime_status,
};
#[cfg(any(feature = "dev-catalog", test))]
use crate::services::depth_lab::{
    DiscoverySmokeResult, DiscoverySmokeStartRequest, preflight_cp2077_discovery,
    start_cp2077_discovery, stop_cp2077_discovery,
};
use tauri::State;

use super::collector::run_blocking_command;

#[cfg(any(feature = "dev-catalog", test))]
pub(super) const LOCAL_CP2077_GAME_ID: &str = "cyberpunk-2077-steam";

/// CA-3 only exposes the stable backend boundary. CA-4 may add a UI later.
#[tauri::command]
pub fn depth_lab_configure(
    state: State<'_, AppState>,
    request: DepthProbeSettings,
    game_id: Option<String>,
) -> Result<DepthProbeRuntimeStatus, String> {
    let selection = if request.enabled {
        let game_id = game_id
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| "请先选择要采集的游戏，再开启深度采集".to_string())?;
        Some(
            state
                .orchestrator
                .depth_catalog_selection(game_id)
                .map_err(|error| format!("读取游戏 depth catalog 失败: {error:#}"))?,
        )
    } else {
        None
    };
    configure(request, selection.as_ref())
}

#[tauri::command]
pub fn depth_lab_get_status() -> DepthProbeRuntimeStatus {
    runtime_status()
}

#[tauri::command]
pub async fn depth_lab_check_performance_certification(
    state: State<'_, AppState>,
    game_id: String,
) -> Result<PerfCertificationGateStatus, String> {
    let orchestrator = Arc::clone(&state.orchestrator);
    run_blocking_command("depth_performance_certification_check", move || {
        orchestrator.check_depth_performance_certification(&game_id)
    })
    .await
}

#[tauri::command]
pub fn depth_lab_launch_cp2077() -> Result<EarlyLaunchResult, String> {
    launch_configured_cp2077()
}

#[tauri::command]
pub fn depth_lab_preflight_cp2077() -> Result<EarlyLaunchPreflight, String> {
    preflight_configured_cp2077()
}

#[cfg(any(feature = "dev-catalog", test))]
#[tauri::command]
pub fn depth_lab_preflight_cp2077_discovery(game_id: String) -> EarlyLaunchPreflight {
    if game_id != LOCAL_CP2077_GAME_ID {
        return EarlyLaunchPreflight {
            eligible: false,
            ready: false,
            executable_hash_verified: false,
            launcher_present: false,
            launcher_sha256: None,
            probe_present: false,
            probe_sha256: None,
            blockers: vec!["当前游戏没有本地 CP2077 discovery 入口".into()],
        };
    }
    preflight_cp2077_discovery()
}

#[cfg(any(feature = "dev-catalog", test))]
#[tauri::command]
pub async fn depth_lab_start_cp2077_discovery(
    request: DiscoverySmokeStartRequest,
) -> Result<DiscoverySmokeResult, String> {
    tauri::async_runtime::spawn_blocking(move || start_cp2077_discovery(request))
        .await
        .map_err(|error| format!("CP2077 discovery launch task failed: {error}"))?
}

#[cfg(any(feature = "dev-catalog", test))]
#[tauri::command]
pub async fn depth_lab_stop_cp2077_discovery() -> Result<DiscoverySmokeResult, String> {
    tauri::async_runtime::spawn_blocking(stop_cp2077_discovery)
        .await
        .map_err(|error| format!("CP2077 discovery seal task failed: {error}"))?
}

#[tauri::command]
pub fn depth_lab_begin_perf_certification() -> Result<PerfCertificationStatus, String> {
    begin_perf_certification()
}

#[tauri::command]
pub fn depth_lab_get_perf_certification() -> Result<PerfCertificationStatus, String> {
    perf_certification_status()
}

#[tauri::command]
pub fn depth_lab_record_perf_stage(
    request: PerfStageMeasurementRequest,
) -> Result<PerfCertificationStatus, String> {
    record_perf_stage(request)
}

#[tauri::command]
pub fn depth_lab_cancel_perf_certification() -> Result<PerfCertificationStatus, String> {
    cancel_perf_certification()
}

#[tauri::command]
pub async fn depth_lab_start_perf_stage(
    state: State<'_, AppState>,
    request: StartSessionRequest,
) -> Result<CollectorSnapshot, String> {
    let orchestrator = Arc::clone(&state.orchestrator);
    let request_id = request.request_id;
    run_blocking_command("depth_perf_stage", move || {
        orchestrator.start_perf_certification_stage(&request_id)
    })
    .await
}

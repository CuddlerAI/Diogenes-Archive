mod app_state;
mod build_flavor;
mod commands;
mod domain;
mod platform;
mod services;

#[cfg(all(feature = "depth-lab", any(feature = "dev-catalog", test)))]
pub mod cp2077_raw_demo;
#[cfg(feature = "depth-lab")]
pub mod depth_finalize_recovery;
#[cfg(feature = "depth-lab")]
pub mod issue504_acceptance;

use app_state::AppState;
use commands::account::{
    account_change_password, account_confirm_password_reset, account_enroll_device,
    account_get_consent_notice, account_get_consent_state, account_get_device_enrollment_status,
    account_get_profile, account_get_status, account_get_terms, account_login, account_logout,
    account_refresh_consent_notice, account_refresh_session, account_renew_consent,
    account_request_password_reset, account_resend_signup_code, account_signup,
    account_update_profile, account_verify_signup,
};
use commands::collector::{
    collector_add_marker, collector_bind_window, collector_choose_capture_storage_location,
    collector_delete_session, collector_export_diagnostics, collector_get_capture_storage_status,
    collector_get_data_model_status, collector_get_overlay_snapshot, collector_get_p0_status,
    collector_get_session_history_detail, collector_get_snapshot, collector_list_games,
    collector_list_session_history, collector_list_window_candidates,
    collector_open_data_model_directory, collector_open_session_directory,
    collector_overlay_heartbeat, collector_overlay_ready, collector_overlay_report_fault,
    collector_pause, collector_request_session_upload, collector_resume,
    collector_route_overlay_action, collector_set_overlay_visible, collector_start_session,
    collector_stop,
};
#[cfg(all(feature = "depth-lab", any(feature = "dev-catalog", test)))]
use commands::cp2077_trial::{
    cp2077_trial_get_status, cp2077_trial_open_output, cp2077_trial_prepare_and_launch,
    cp2077_trial_start_recording, cp2077_trial_stop_recording,
};
use commands::credentials::{
    client_delete_device_credential, client_get_device_credential_status,
    client_store_device_credential,
};
#[cfg(feature = "depth-lab")]
use commands::depth_lab::{
    depth_lab_begin_perf_certification, depth_lab_cancel_perf_certification,
    depth_lab_check_performance_certification, depth_lab_configure,
    depth_lab_get_perf_certification, depth_lab_get_status, depth_lab_launch_cp2077,
    depth_lab_preflight_cp2077, depth_lab_record_perf_stage, depth_lab_start_perf_stage,
};
#[cfg(all(feature = "depth-lab", any(feature = "dev-catalog", test)))]
use commands::depth_lab::{
    depth_lab_preflight_cp2077_discovery, depth_lab_start_cp2077_discovery,
    depth_lab_stop_cp2077_discovery,
};
use commands::updater::{client_check_for_update, client_get_update_status};
use commands::view_scale::{
    view_scale_abort_measurement, view_scale_begin_session, view_scale_cancel_session,
    view_scale_finish_session, view_scale_get_status, view_scale_list_history,
    view_scale_toggle_measurement,
};
use platform::windows::global_hotkeys;
use services::lifecycle;
use std::time::Duration;
use tauri::{Manager, WindowEvent};

const DEV_INSTALLER_SMOKE_EXIT_PREFIX: &str = "--dev-installer-smoke-exit-ms=";
const DEV_INSTALLER_SMOKE_QUIESCENCE_TIMEOUT: Duration = Duration::from_secs(5);

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let dev_installer_smoke_exit = dev_installer_smoke_exit_delay(std::env::args());
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .setup(|app| {
            #[cfg(feature = "depth-lab")]
            if let Err(error) =
                services::depth_certification_delivery::remove_retired_certification_stores()
            {
                tracing::warn!(%error, "could not remove retired Depth certification stores");
            }
            let state = AppState::new(app.handle().clone())?;
            let orchestrator = state.orchestrator.clone();
            let view_scale = state.view_scale.clone();
            app.manage(state);

            let tray = lifecycle::install_tray(app)?;
            app.state::<AppState>().set_tray(tray);

            // docs/65 J-13：窗口标题写死在 tauri.conf.json 且生产配置不覆盖
            // app.windows，这里以 build_flavor 为唯一事实源统一改写（dev 风味
            // 写回的值与配置相同，无可见变化）。
            for (label, title) in [
                ("main", build_flavor::main_window_title()),
                ("overlay", build_flavor::overlay_window_title()),
            ] {
                if let Some(window) = app.get_webview_window(label) {
                    let _ = window.set_title(title);
                }
            }

            if let Some(main) = app.get_webview_window("main")
                && let Err(error) = global_hotkeys::install(&main, orchestrator.clone(), view_scale)
            {
                // docs/65 J-24：此前只写日志，而采集页仍写死「Ctrl+Alt+F8 暂停 ·
                // F10 结束」——全屏里按键没反应且无从得知。通知入启动队列，由首次
                // snapshot 轮询送达前端。
                orchestrator.report_control_hotkeys_unavailable(&format!("{error:#}"));
                tracing::warn!(error = %error, "global hotkeys unavailable; main and tray controls remain available");
            }

            // M-A 版本检查（docs/15 §2.3）：只在 release 构建自动检查；
            // 检查只提示、不下载不执行，失败对采集路径完全无影响。
            if services::updater::checks_enabled() {
                tauri::async_runtime::spawn(services::updater::auto_check_loop(
                    app.handle().clone(),
                ));
            }
            // 条款变更观察（docs/108 §3.6）：只读、只提示。未登录或账号面未配置时
            // 整条通路静默，采集路径不受任何影响——与版本检查同一条纪律。
            tauri::async_runtime::spawn(services::consent_watch::watch_loop(
                app.handle().clone(),
            ));
            Ok(())
        })
        .on_window_event(|window, event| {
            if window.label() == "main"
                && let WindowEvent::CloseRequested { api, .. } = event
                && !window.state::<AppState>().quit_requested()
            {
                api.prevent_close();
                if let Err(error) = window.hide() {
                    tracing::warn!(error = %error, "failed to hide main window to tray");
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            collector_get_p0_status,
            collector_get_snapshot,
            collector_get_capture_storage_status,
            collector_choose_capture_storage_location,
            collector_list_games,
            collector_list_window_candidates,
            collector_bind_window,
            collector_start_session,
            collector_pause,
            collector_resume,
            collector_add_marker,
            collector_stop,
            collector_export_diagnostics,
            collector_list_session_history,
            collector_request_session_upload,
            collector_get_session_history_detail,
            collector_get_data_model_status,
            collector_open_data_model_directory,
            collector_open_session_directory,
            collector_delete_session,
            collector_get_overlay_snapshot,
            collector_set_overlay_visible,
            collector_overlay_ready,
            collector_overlay_heartbeat,
            collector_overlay_report_fault,
            collector_route_overlay_action,
            account_get_status,
            account_refresh_session,
            account_get_terms,
            account_get_consent_state,
            account_get_consent_notice,
            account_refresh_consent_notice,
            account_renew_consent,
            account_signup,
            account_verify_signup,
            account_resend_signup_code,
            account_request_password_reset,
            account_confirm_password_reset,
            account_login,
            account_logout,
            account_change_password,
            account_get_profile,
            account_update_profile,
            account_get_device_enrollment_status,
            account_enroll_device,
            client_get_device_credential_status,
            client_store_device_credential,
            client_delete_device_credential,
            client_get_update_status,
            client_check_for_update,
            view_scale_get_status,
            view_scale_begin_session,
            view_scale_toggle_measurement,
            view_scale_abort_measurement,
            view_scale_cancel_session,
            view_scale_finish_session,
            view_scale_list_history,
            #[cfg(feature = "depth-lab")]
            depth_lab_configure,
            #[cfg(feature = "depth-lab")]
            depth_lab_get_status,
            #[cfg(feature = "depth-lab")]
            depth_lab_check_performance_certification,
            #[cfg(feature = "depth-lab")]
            depth_lab_launch_cp2077,
            #[cfg(feature = "depth-lab")]
            depth_lab_preflight_cp2077,
            #[cfg(all(feature = "depth-lab", any(feature = "dev-catalog", test)))]
            depth_lab_preflight_cp2077_discovery,
            #[cfg(all(feature = "depth-lab", any(feature = "dev-catalog", test)))]
            depth_lab_start_cp2077_discovery,
            #[cfg(all(feature = "depth-lab", any(feature = "dev-catalog", test)))]
            depth_lab_stop_cp2077_discovery,
            #[cfg(all(feature = "depth-lab", any(feature = "dev-catalog", test)))]
            cp2077_trial_get_status,
            #[cfg(all(feature = "depth-lab", any(feature = "dev-catalog", test)))]
            cp2077_trial_prepare_and_launch,
            #[cfg(all(feature = "depth-lab", any(feature = "dev-catalog", test)))]
            cp2077_trial_start_recording,
            #[cfg(all(feature = "depth-lab", any(feature = "dev-catalog", test)))]
            cp2077_trial_stop_recording,
            #[cfg(all(feature = "depth-lab", any(feature = "dev-catalog", test)))]
            cp2077_trial_open_output,
            #[cfg(feature = "depth-lab")]
            depth_lab_begin_perf_certification,
            #[cfg(feature = "depth-lab")]
            depth_lab_get_perf_certification,
            #[cfg(feature = "depth-lab")]
            depth_lab_record_perf_stage,
            #[cfg(feature = "depth-lab")]
            depth_lab_cancel_perf_certification,
            #[cfg(feature = "depth-lab")]
            depth_lab_start_perf_stage,
        ])
        .build(tauri::generate_context!())
        .expect("DataMiner Tauri runtime failed");
    if let Some(delay) = dev_installer_smoke_exit {
        let handle = app.handle().clone();
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(delay).await;
            lifecycle::request_bounded_safe_quit(&handle, DEV_INSTALLER_SMOKE_QUIESCENCE_TIMEOUT);
        });
    }
    app.run(|app, event| {
        if let tauri::RunEvent::ExitRequested { api, .. } = event
            && !app.state::<AppState>().quit_requested()
        {
            api.prevent_exit();
        }
    });
}

fn dev_installer_smoke_exit_delay(args: impl IntoIterator<Item = String>) -> Option<Duration> {
    args.into_iter().find_map(|argument| {
        let milliseconds = argument
            .strip_prefix(DEV_INSTALLER_SMOKE_EXIT_PREFIX)?
            .parse::<u64>()
            .ok()?;
        (1_000..=10_000)
            .contains(&milliseconds)
            .then(|| Duration::from_millis(milliseconds))
    })
}

#[cfg(test)]
mod installer_smoke_tests {
    use super::*;

    #[test]
    fn installer_smoke_exit_is_explicit_and_bounded() {
        assert_eq!(
            dev_installer_smoke_exit_delay([
                "app".into(),
                "--dev-installer-smoke-exit-ms=2500".into()
            ]),
            Some(Duration::from_millis(2_500))
        );
        assert_eq!(
            dev_installer_smoke_exit_delay(["--dev-installer-smoke-exit-ms=999".into()]),
            None
        );
        assert_eq!(dev_installer_smoke_exit_delay(["app".into()]), None);
        assert!(
            Duration::from_millis(3_000) + DEV_INSTALLER_SMOKE_QUIESCENCE_TIMEOUT
                < Duration::from_millis(15_000),
            "the installer smoke must leave time for Tauri/WebView teardown"
        );
    }
}

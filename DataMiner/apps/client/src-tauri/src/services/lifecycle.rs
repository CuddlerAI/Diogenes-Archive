use std::time::Duration;

use tauri::image::Image;
use tauri::menu::MenuBuilder;
use tauri::tray::{MouseButton, TrayIcon, TrayIconBuilder, TrayIconEvent};
use tauri::{App, AppHandle, Manager, Wry};

use crate::app_state::AppState;
use crate::build_flavor;
use crate::domain::collector::{CollectorState, OverlayAction};
use crate::services::diagnostics::{DiagnosticCorrelation, DiagnosticPayload};

const TRAY_ID: &str = "dataminer-tray";
const SHOW_MAIN_ID: &str = "show-main";
const TOGGLE_PAUSE_RESUME_ID: &str = "toggle-pause-resume";
const STOP_ID: &str = "stop-recording";
const QUIT_ID: &str = "quit";
const SAFE_QUIT_POLL_INTERVAL: Duration = Duration::from_millis(50);
const SAFE_QUIT_TIMEOUT_EXIT_CODE: i32 = 70;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SafeQuitGates {
    data_model: bool,
    sentry: bool,
    uploader_worker: bool,
    client_events_worker: bool,
}

impl SafeQuitGates {
    fn all_ready(self) -> bool {
        self.data_model && self.sentry && self.uploader_worker && self.client_events_worker
    }

    fn blocked_names(self) -> Vec<String> {
        [
            ("data_model", self.data_model),
            ("sentry", self.sentry),
            ("uploader_worker", self.uploader_worker),
            ("client_events_worker", self.client_events_worker),
        ]
        .into_iter()
        .filter(|(_, ready)| !ready)
        .map(|(name, _)| name.to_owned())
        .collect()
    }
}

trait SafeQuitContext: Clone + Send + Sync + 'static {
    fn collector_state(&self) -> CollectorState;
    fn begin_data_model_shutdown(&self);
    fn gates(&self) -> SafeQuitGates;
    fn record_diagnostic(&self, payload: DiagnosticPayload);
    fn exit(&self, code: i32);
}

#[derive(Clone)]
struct AppSafeQuitContext {
    app: AppHandle<Wry>,
}

impl SafeQuitContext for AppSafeQuitContext {
    fn collector_state(&self) -> CollectorState {
        self.app.state::<AppState>().orchestrator.snapshot().state
    }

    fn begin_data_model_shutdown(&self) {
        self.app
            .state::<AppState>()
            .orchestrator
            .begin_data_model_shutdown();
    }

    fn gates(&self) -> SafeQuitGates {
        let state = self.app.state::<AppState>();
        SafeQuitGates {
            data_model: state.orchestrator.data_model_quiescent(),
            sentry: state.orchestrator.sentry_quiescent(),
            uploader_worker: state.uploader_shutdown_complete(),
            client_events_worker: state.client_events_shutdown_complete(),
        }
    }

    fn record_diagnostic(&self, payload: DiagnosticPayload) {
        let level = if matches!(&payload, DiagnosticPayload::SafeQuitTimedOut { .. }) {
            "error"
        } else {
            "info"
        };
        if let Err(error) = self
            .app
            .state::<AppState>()
            .orchestrator
            .diagnostics()
            .record(level, None, DiagnosticCorrelation::default(), payload, true)
        {
            tracing::error!(error = %error, "failed to persist safe-quit diagnostic");
        }
    }

    fn exit(&self, code: i32) {
        self.app.exit(code);
    }
}

pub fn install_tray(app: &App<Wry>) -> anyhow::Result<TrayIcon<Wry>> {
    let menu = MenuBuilder::new(app)
        .text(SHOW_MAIN_ID, build_flavor::tray_open_label())
        .text(TOGGLE_PAUSE_RESUME_ID, "暂停 / 继续采集 (Ctrl+Alt+F8)")
        .text(STOP_ID, "结束当前会话 (Ctrl+Alt+F10)")
        .separator()
        .text(QUIT_ID, "安全退出")
        .build()?;

    let tray = TrayIconBuilder::with_id(TRAY_ID)
        .menu(&menu)
        .icon(tray_icon())
        .tooltip(build_flavor::tray_tooltip())
        .on_tray_icon_event(|tray, event| {
            if matches!(
                event,
                TrayIconEvent::DoubleClick {
                    button: MouseButton::Left,
                    ..
                }
            ) {
                route(tray.app_handle(), OverlayAction::ShowMainWindow);
            }
        })
        .on_menu_event(|app, event| match event.id().as_ref() {
            SHOW_MAIN_ID => route(app, OverlayAction::ShowMainWindow),
            TOGGLE_PAUSE_RESUME_ID => {
                if let Err(error) = app
                    .state::<AppState>()
                    .orchestrator
                    .toggle_pause_resume(None)
                {
                    tracing::warn!(error = %error, "tray pause/resume action failed");
                }
            }
            STOP_ID => {
                if let Err(error) = app.state::<AppState>().orchestrator.stop("tray") {
                    tracing::warn!(error = %error, "tray stop action failed");
                }
            }
            QUIT_ID => request_safe_quit(app),
            _ => {}
        })
        .build(app)?;
    Ok(tray)
}

pub fn request_safe_quit(app: &AppHandle<Wry>) {
    request_safe_quit_with_timeout(app, None);
}

pub fn request_bounded_safe_quit(app: &AppHandle<Wry>, quiescence_timeout: Duration) {
    request_safe_quit_with_timeout(app, Some(quiescence_timeout));
}

fn request_safe_quit_with_timeout(app: &AppHandle<Wry>, quiescence_timeout: Option<Duration>) {
    let state = app.state::<AppState>();
    if !state.request_quit() {
        return;
    }
    let orchestrator = state.orchestrator.clone();
    let current = orchestrator.begin_safe_quit();
    let context = AppSafeQuitContext { app: app.clone() };
    context.record_diagnostic(DiagnosticPayload::SafeQuitRequested {
        quiescence_timeout_ms: quiescence_timeout.map(duration_ms),
    });
    if let Err(error) = stop_capture_before_background_shutdown(
        current,
        || orchestrator.stop("application_exit").map(|_| ()),
        || {
            state.begin_uploader_shutdown();
            state.begin_client_events_shutdown();
            state.begin_catalog_sync_shutdown();
        },
    ) {
        tracing::error!(error = %error, "safe exit could not stop capture");
        orchestrator.cancel_safe_quit();
        state.cancel_quit();
        return;
    }

    tauri::async_runtime::spawn(coordinate_safe_quit(context, quiescence_timeout));
}

fn stop_capture_before_background_shutdown(
    current: CollectorState,
    stop_capture: impl FnOnce() -> anyhow::Result<()>,
    begin_background_shutdown: impl FnOnce(),
) -> anyhow::Result<()> {
    if should_stop_before_exit(current) {
        stop_capture()?;
    }
    begin_background_shutdown();
    Ok(())
}

async fn coordinate_safe_quit<C: SafeQuitContext>(
    context: C,
    quiescence_timeout: Option<Duration>,
) {
    let started_at = tokio::time::Instant::now();
    let mut data_model_shutdown_started = false;
    let mut deadline = None;

    loop {
        if !should_stop_before_exit(context.collector_state()) {
            if !data_model_shutdown_started {
                // Stop future claims before observing quiescence; otherwise a sleeping
                // worker could wake between the check and app.exit(). The smoke deadline
                // begins only after capture has reached a terminal state.
                context.begin_data_model_shutdown();
                data_model_shutdown_started = true;
                deadline = quiescence_timeout.map(|timeout| tokio::time::Instant::now() + timeout);
            }

            let gates = context.gates();
            if gates.all_ready() {
                context.record_diagnostic(DiagnosticPayload::SafeQuitCompleted {
                    elapsed_ms: duration_ms(started_at.elapsed()),
                });
                context.exit(0);
                return;
            }

            if deadline.is_some_and(|limit| tokio::time::Instant::now() >= limit) {
                let timeout = quiescence_timeout.expect("deadline requires a timeout");
                let blocked_gates = gates.blocked_names();
                tracing::error!(
                    ?blocked_gates,
                    timeout_ms = duration_ms(timeout),
                    "bounded safe quit reached its quiescence deadline"
                );
                context.record_diagnostic(DiagnosticPayload::SafeQuitTimedOut {
                    timeout_ms: duration_ms(timeout),
                    blocked_gates,
                });
                // A blocked smoke must fail closed. This is a normal non-zero Tauri
                // exit request, never an exit-0 process kill. Recording shutdown is
                // excluded because the deadline is not armed until capture is terminal.
                context.exit(SAFE_QUIT_TIMEOUT_EXIT_CODE);
                return;
            }
        }

        let wake_at = deadline
            .map(|limit| {
                std::cmp::min(limit, tokio::time::Instant::now() + SAFE_QUIT_POLL_INTERVAL)
            })
            .unwrap_or_else(|| tokio::time::Instant::now() + SAFE_QUIT_POLL_INTERVAL);
        tokio::time::sleep_until(wake_at).await;
    }
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn route(app: &AppHandle<Wry>, action: OverlayAction) {
    if let Err(error) = app
        .state::<AppState>()
        .orchestrator
        .route_overlay_action(action)
    {
        tracing::warn!(error = %error, "tray action failed");
    }
}

fn capture_active(state: CollectorState) -> bool {
    matches!(
        state,
        CollectorState::Starting
            | CollectorState::Recording
            | CollectorState::Pausing
            | CollectorState::Stopping
    )
}

fn should_stop_before_exit(state: CollectorState) -> bool {
    capture_active(state) || state == CollectorState::Paused
}

fn tray_icon() -> Image<'static> {
    const SIZE: u32 = 32;
    let mut rgba = vec![0_u8; (SIZE * SIZE * 4) as usize];
    for y in 0..SIZE {
        for x in 0..SIZE {
            let index = ((y * SIZE + x) * 4) as usize;
            let in_disc = (x as i32 - 16).pow(2) + (y as i32 - 16).pow(2) <= 14_i32.pow(2);
            let in_d = (9..=21).contains(&x)
                && (8..=24).contains(&y)
                && (x <= 12 || y <= 11 || y >= 21 || x >= 19);
            let pixel = if in_d {
                [8, 21, 16, 255]
            } else if in_disc {
                [101, 221, 179, 255]
            } else {
                [0, 0, 0, 0]
            };
            rgba[index..index + 4].copy_from_slice(&pixel);
        }
    }
    Image::new_owned(rgba, SIZE, SIZE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use parking_lot::Mutex;
    use serde_json::Value;

    #[derive(Clone)]
    struct FakeSafeQuitContext {
        collector_state: Arc<Mutex<CollectorState>>,
        gates: Arc<Mutex<SafeQuitGates>>,
        data_model_shutdowns: Arc<AtomicUsize>,
        diagnostics: Arc<Mutex<Vec<Value>>>,
        exits: Arc<Mutex<Vec<i32>>>,
    }

    impl FakeSafeQuitContext {
        fn new(collector_state: CollectorState, gates: SafeQuitGates) -> Self {
            Self {
                collector_state: Arc::new(Mutex::new(collector_state)),
                gates: Arc::new(Mutex::new(gates)),
                data_model_shutdowns: Arc::new(AtomicUsize::new(0)),
                diagnostics: Arc::new(Mutex::new(Vec::new())),
                exits: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    impl SafeQuitContext for FakeSafeQuitContext {
        fn collector_state(&self) -> CollectorState {
            *self.collector_state.lock()
        }

        fn begin_data_model_shutdown(&self) {
            self.data_model_shutdowns.fetch_add(1, Ordering::AcqRel);
        }

        fn gates(&self) -> SafeQuitGates {
            *self.gates.lock()
        }

        fn record_diagnostic(&self, payload: crate::services::diagnostics::DiagnosticPayload) {
            self.diagnostics
                .lock()
                .push(serde_json::to_value(payload).unwrap());
        }

        fn exit(&self, code: i32) {
            self.exits.lock().push(code);
        }
    }

    fn ready_gates() -> SafeQuitGates {
        SafeQuitGates {
            data_model: true,
            sentry: true,
            uploader_worker: true,
            client_events_worker: true,
        }
    }

    #[test]
    fn background_shutdown_waits_for_capture_stop_success() {
        let background_shutdowns = AtomicUsize::new(0);

        let result = stop_capture_before_background_shutdown(
            CollectorState::Recording,
            || anyhow::bail!("synthetic capture stop failure"),
            || {
                background_shutdowns.fetch_add(1, Ordering::AcqRel);
            },
        );

        assert!(result.is_err());
        assert_eq!(background_shutdowns.load(Ordering::Acquire), 0);
    }

    #[test]
    fn background_shutdown_follows_successful_capture_stop() {
        let calls = Mutex::new(Vec::new());

        stop_capture_before_background_shutdown(
            CollectorState::Recording,
            || {
                calls.lock().push("capture_stop");
                Ok::<(), anyhow::Error>(())
            },
            || calls.lock().push("background_shutdown"),
        )
        .unwrap();

        assert_eq!(*calls.lock(), vec!["capture_stop", "background_shutdown"]);
    }

    #[test]
    fn idle_safe_quit_still_starts_background_shutdown() {
        let capture_stops = AtomicUsize::new(0);
        let background_shutdowns = AtomicUsize::new(0);

        stop_capture_before_background_shutdown(
            CollectorState::Completed,
            || {
                capture_stops.fetch_add(1, Ordering::AcqRel);
                Ok::<(), anyhow::Error>(())
            },
            || {
                background_shutdowns.fetch_add(1, Ordering::AcqRel);
            },
        )
        .unwrap();

        assert_eq!(capture_stops.load(Ordering::Acquire), 0);
        assert_eq!(background_shutdowns.load(Ordering::Acquire), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn bounded_safe_exit_all_quiescent_reaches_exit_zero() {
        let context = FakeSafeQuitContext::new(CollectorState::Completed, ready_gates());

        coordinate_safe_quit(context.clone(), Some(Duration::from_secs(8))).await;

        assert_eq!(*context.exits.lock(), vec![0]);
        assert_eq!(context.data_model_shutdowns.load(Ordering::Acquire), 1);
        assert_eq!(context.diagnostics.lock().len(), 1);
        assert_eq!(context.diagnostics.lock()[0]["kind"], "safe_quit_completed");
    }

    #[tokio::test(start_paused = true)]
    async fn bounded_safe_exit_names_blocked_gate_and_is_time_bounded() {
        for (blocked_gate, gates) in [
            (
                "data_model",
                SafeQuitGates {
                    data_model: false,
                    ..ready_gates()
                },
            ),
            (
                "sentry",
                SafeQuitGates {
                    sentry: false,
                    ..ready_gates()
                },
            ),
            (
                "uploader_worker",
                SafeQuitGates {
                    uploader_worker: false,
                    ..ready_gates()
                },
            ),
            (
                "client_events_worker",
                SafeQuitGates {
                    client_events_worker: false,
                    ..ready_gates()
                },
            ),
        ] {
            let context = FakeSafeQuitContext::new(CollectorState::Completed, gates);
            let task = tokio::spawn(coordinate_safe_quit(
                context.clone(),
                Some(Duration::from_millis(400)),
            ));
            tokio::task::yield_now().await;

            tokio::time::advance(Duration::from_millis(399)).await;
            tokio::task::yield_now().await;
            assert!(context.exits.lock().is_empty(), "gate={blocked_gate}");
            assert!(!task.is_finished(), "gate={blocked_gate}");

            tokio::time::advance(Duration::from_millis(1)).await;
            task.await.unwrap();

            assert_eq!(
                *context.exits.lock(),
                vec![SAFE_QUIT_TIMEOUT_EXIT_CODE],
                "gate={blocked_gate}"
            );
            assert_ne!(context.exits.lock()[0], 0, "gate={blocked_gate}");
            let diagnostics = context.diagnostics.lock();
            assert_eq!(diagnostics.len(), 1, "gate={blocked_gate}");
            assert_eq!(
                diagnostics[0]["kind"], "safe_quit_timed_out",
                "gate={blocked_gate}"
            );
            assert_eq!(
                diagnostics[0]["data"]["timeout_ms"], 400,
                "gate={blocked_gate}"
            );
            assert_eq!(
                diagnostics[0]["data"]["blocked_gates"],
                serde_json::json!([blocked_gate]),
                "gate={blocked_gate}"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn bounded_safe_exit_does_not_cut_off_live_capture() {
        let context = FakeSafeQuitContext::new(CollectorState::Recording, ready_gates());
        let task = tokio::spawn(coordinate_safe_quit(
            context.clone(),
            Some(Duration::from_secs(8)),
        ));
        tokio::task::yield_now().await;

        tokio::time::advance(Duration::from_secs(24)).await;
        tokio::task::yield_now().await;
        assert!(context.exits.lock().is_empty());
        assert_eq!(context.data_model_shutdowns.load(Ordering::Acquire), 0);
        assert!(!task.is_finished());

        *context.collector_state.lock() = CollectorState::Completed;
        tokio::time::advance(Duration::from_millis(50)).await;
        task.await.unwrap();

        assert_eq!(*context.exits.lock(), vec![0]);
        assert_eq!(context.data_model_shutdowns.load(Ordering::Acquire), 1);
    }

    #[test]
    fn safe_exit_waits_only_for_live_capture_states() {
        assert!(capture_active(CollectorState::Recording));
        assert!(capture_active(CollectorState::Pausing));
        assert!(!capture_active(CollectorState::Paused));
        assert!(!capture_active(CollectorState::Completed));
    }

    #[test]
    fn safe_exit_completes_paused_session_before_leaving() {
        assert!(should_stop_before_exit(CollectorState::Paused));
        assert!(should_stop_before_exit(CollectorState::Recording));
        assert!(!should_stop_before_exit(CollectorState::Completed));
        assert!(!should_stop_before_exit(CollectorState::Faulted));
    }

    #[test]
    fn generated_tray_icon_has_expected_rgba_shape() {
        let icon = tray_icon();
        assert_eq!(icon.width(), 32);
        assert_eq!(icon.height(), 32);
        assert_eq!(icon.rgba().len(), 32 * 32 * 4);
    }
}

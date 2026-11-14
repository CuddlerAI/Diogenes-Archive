use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::Context;
use capture_core::input::RawInputStatus;
use parking_lot::Mutex;
use tauri::{AppHandle, Emitter, Manager, WebviewWindow, WindowEvent};

use crate::domain::collector::{CollectorSnapshot, CollectorState, OverlaySnapshot};
use crate::platform::windows::overlay_window::{
    OVERLAY_LABEL, OverlayTarget, configure_hud, hide_hud, place_hud,
};
use crate::platform::windows::window_tracker::BoundTarget;
use crate::services::diagnostics::{DiagnosticCorrelation, DiagnosticService};

const OVERLAY_TICK: Duration = Duration::from_millis(16);
const OVERLAY_HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(5);

struct OverlayState {
    collector: CollectorSnapshot,
    input_status: Option<RawInputStatus>,
    target: Option<OverlayTarget>,
    visible_preference: bool,
    webview_ready: bool,
    last_heartbeat: Option<Instant>,
    active_since: Option<Instant>,
    shown: bool,
    warning_emitted: bool,
    last_payload: Option<OverlaySnapshot>,
    last_ping_sent: Option<Instant>,
}

pub struct OverlayWindowController {
    app: AppHandle,
    window: WebviewWindow,
    overlay_hwnd_value: usize,
    placement_dirty: Arc<AtomicBool>,
    diagnostics: Arc<DiagnosticService>,
    state: Mutex<OverlayState>,
}

impl OverlayWindowController {
    pub fn new(app: AppHandle, diagnostics: Arc<DiagnosticService>) -> anyhow::Result<Arc<Self>> {
        let window = app
            .get_webview_window(OVERLAY_LABEL)
            .context("configured Overlay window is missing")?;
        let overlay_hwnd_value = configure_hud(&window)?;
        hide_hud(overlay_hwnd_value);
        let controller = Arc::new(Self {
            app,
            window,
            overlay_hwnd_value,
            placement_dirty: Arc::new(AtomicBool::new(true)),
            diagnostics,
            state: Mutex::new(OverlayState {
                collector: CollectorSnapshot::default(),
                input_status: None,
                target: None,
                visible_preference: true,
                webview_ready: false,
                last_heartbeat: None,
                active_since: None,
                shown: false,
                warning_emitted: false,
                last_payload: None,
                last_ping_sent: None,
            }),
        });
        controller.install_window_event_isolation();
        controller.start_publisher();
        Ok(controller)
    }

    pub fn set_target(&self, target: &BoundTarget) {
        self.state.lock().target = Some(OverlayTarget {
            hwnd_value: target.hwnd_value,
            root_owner_value: target.root_owner_value,
            expected_width: target.client_width,
            expected_height: target.client_height,
            expected_dpi: target.dpi,
        });
        self.placement_dirty.store(true, Ordering::Release);
    }

    pub fn clear_target(&self) {
        {
            let mut state = self.state.lock();
            state.target = None;
            state.input_status = None;
            state.shown = false;
            state.last_payload = None;
        }
        hide_hud(self.overlay_hwnd_value);
        self.placement_dirty.store(true, Ordering::Release);
    }

    pub fn attach_input_status(&self, status: RawInputStatus) {
        self.state.lock().input_status = Some(status);
    }

    pub fn detach_input_status(&self) {
        self.state.lock().input_status = None;
    }

    pub fn update_collector_snapshot(&self, snapshot: CollectorSnapshot) {
        let mut state = self.state.lock();
        let was_active = overlay_active(state.collector.state);
        let is_active = overlay_active(snapshot.state);
        if is_active && !was_active {
            let now = Instant::now();
            state.active_since = Some(now);
            state.last_heartbeat = Some(now);
            state.last_ping_sent = None;
        } else if !is_active {
            state.active_since = None;
        }
        state.collector = snapshot;
    }

    #[must_use]
    pub fn placement_dirty_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.placement_dirty)
    }

    pub fn set_visible(&self, visible: bool) {
        let hide_now = {
            let mut state = self.state.lock();
            state.visible_preference = visible;
            if !visible {
                state.shown = false;
                true
            } else {
                false
            }
        };
        if hide_now {
            hide_hud(self.overlay_hwnd_value);
        }
        self.placement_dirty.store(true, Ordering::Release);
    }

    pub fn webview_ready(&self) -> anyhow::Result<()> {
        configure_hud(&self.window)?;
        let mut state = self.state.lock();
        state.webview_ready = true;
        state.last_heartbeat = Some(Instant::now());
        state.warning_emitted = false;
        state.last_payload = None;
        state.last_ping_sent = None;
        drop(state);
        self.placement_dirty.store(true, Ordering::Release);
        Ok(())
    }

    pub fn heartbeat(&self) {
        self.state.lock().last_heartbeat = Some(Instant::now());
    }

    pub fn report_client_fault(&self) {
        self.mark_unavailable("overlay_javascript_fault");
    }

    fn install_window_event_isolation(self: &Arc<Self>) {
        let controller = Arc::downgrade(self);
        self.window.on_window_event(move |event| {
            let Some(controller) = controller.upgrade() else {
                return;
            };
            match event {
                WindowEvent::CloseRequested { api, .. } => {
                    api.prevent_close();
                    controller.set_visible(false);
                    tracing::warn!("Overlay close request was isolated from the recording session");
                }
                WindowEvent::Destroyed => controller.mark_unavailable("overlay_window_destroyed"),
                WindowEvent::Focused(true) => {
                    tracing::warn!(
                        "Overlay WebView reported focus; Win32 no-activate and foreground guards remain authoritative"
                    );
                }
                _ => {}
            }
        });
    }

    fn start_publisher(self: &Arc<Self>) {
        let controller = Arc::downgrade(self);
        tauri::async_runtime::spawn(async move {
            let mut ticker = tokio::time::interval(OVERLAY_TICK);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                ticker.tick().await;
                let Some(controller) = controller.upgrade() else {
                    break;
                };
                controller.tick();
            }
        });
    }

    fn tick(&self) {
        let now = Instant::now();
        let (payload, emit_payload, ping_due, target, should_show, heartbeat_expired, was_shown) = {
            let mut state = self.state.lock();
            let mut payload = state.collector.overlay_snapshot();
            if matches!(
                state.collector.state,
                CollectorState::Starting | CollectorState::Recording
            ) && let Some(input_status) = &state.input_status
            {
                let input = input_status.snapshot();
                payload.pressed_contract_keys = input.pressed_contract_keys;
                payload.mouse_buttons = input.mouse_buttons;
            }
            let emit_payload = state.last_payload.as_ref() != Some(&payload);
            if emit_payload {
                state.last_payload = Some(payload.clone());
            }
            let active = overlay_active(state.collector.state);
            let overlay_expected = active && state.visible_preference;
            let should_show = overlay_expected && state.webview_ready;
            let readiness_expired = overlay_expected
                && !state.webview_ready
                && state
                    .active_since
                    .is_some_and(|started| now.duration_since(started) > OVERLAY_HEARTBEAT_TIMEOUT);
            let heartbeat_expired = readiness_expired
                || (should_show
                    && state
                        .last_heartbeat
                        .is_none_or(|last| now.duration_since(last) > OVERLAY_HEARTBEAT_TIMEOUT));
            let ping_due = should_show
                && state
                    .last_ping_sent
                    .is_none_or(|last| now.duration_since(last) >= Duration::from_secs(1));
            if ping_due {
                state.last_ping_sent = Some(now);
            }
            (
                payload,
                emit_payload,
                ping_due,
                state.target,
                should_show,
                heartbeat_expired,
                state.shown,
            )
        };

        if heartbeat_expired {
            self.mark_unavailable("overlay_heartbeat_timeout");
            return;
        }
        if emit_payload
            && let Err(error) = self
                .app
                .emit_to(OVERLAY_LABEL, "overlay://snapshot", &payload)
        {
            tracing::warn!(error = %error, "failed to publish Overlay snapshot");
            self.mark_unavailable("overlay_snapshot_emit_failed");
            return;
        }
        if ping_due && let Err(error) = self.app.emit_to(OVERLAY_LABEL, "overlay://ping", ()) {
            tracing::warn!(error = %error, "failed to ping Overlay WebView");
        }
        if !should_show {
            if was_shown {
                hide_hud(self.overlay_hwnd_value);
                self.state.lock().shown = false;
            }
            return;
        }

        let dirty = self.placement_dirty.swap(false, Ordering::AcqRel);
        if !dirty && was_shown {
            return;
        }
        let Some(target) = target else {
            hide_hud(self.overlay_hwnd_value);
            self.state.lock().shown = false;
            return;
        };
        match place_hud(self.overlay_hwnd_value, target) {
            Ok(shown) => self.state.lock().shown = shown,
            Err(error) => {
                tracing::warn!(error = %error, "Overlay placement failed; recording continues");
                self.mark_unavailable("overlay_placement_failed");
            }
        }
    }

    fn mark_unavailable(&self, reason: &'static str) {
        hide_hud(self.overlay_hwnd_value);
        let (emit_warning, correlation) = {
            let mut state = self.state.lock();
            state.webview_ready = false;
            state.shown = false;
            let emit_warning = if state.warning_emitted {
                false
            } else {
                state.warning_emitted = true;
                true
            };
            let correlation = state
                .collector
                .capture
                .as_ref()
                .map_or_else(DiagnosticCorrelation::default, |capture| {
                    DiagnosticCorrelation::run(&capture.session_id, &capture.run_id)
                });
            (emit_warning, correlation)
        };
        if emit_warning {
            tracing::warn!(
                reason,
                "Overlay unavailable; recording state was not changed"
            );
            emit_unavailable_warning(&self.app, &self.diagnostics, correlation);
        }
    }
}

pub fn emit_unavailable_warning(
    app: &AppHandle,
    diagnostics: &DiagnosticService,
    correlation: DiagnosticCorrelation,
) {
    let notification = diagnostics.create_notification(
        "OVERLAY_UNAVAILABLE",
        "采集状态浮层不可用，录制仍在继续。",
        "录制控制与本地产物不受影响；本次录制结束后重启客户端恢复浮层。",
        true,
        correlation,
        None,
    );
    if let Err(error) = app.emit("collector://notification", &notification) {
        tracing::warn!(error = %error, "failed to emit Overlay warning");
    }
}

fn overlay_active(state: CollectorState) -> bool {
    matches!(
        state,
        CollectorState::Starting
            | CollectorState::Recording
            | CollectorState::Pausing
            | CollectorState::Paused
            | CollectorState::Stopping
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heartbeat_budget_is_wider_than_the_sixty_hertz_publish_tick() {
        assert!(OVERLAY_HEARTBEAT_TIMEOUT > OVERLAY_TICK * 100);
    }
}

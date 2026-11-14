use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use anyhow::{Context, ensure};
use capture_core::qpc::QpcClock;
use tauri::WebviewWindow;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, RegisterHotKey, UnregisterHotKey, VK_F8, VK_F9, VK_F10,
    VK_F11,
};
use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{WM_HOTKEY, WM_NCDESTROY};

use crate::services::orchestrator::SessionOrchestrator;
use crate::services::view_scale::ViewScaleService;

const SUBCLASS_ID: usize = 0x444d_484b;
const TOGGLE_PAUSE_RESUME_ID: i32 = 0x0044_4d01;
const STOP_ID: i32 = 0x0044_4d02;
const ADD_MARKER_ID: i32 = 0x0044_4d03;
const VIEW_SCALE_TOGGLE_ID: i32 = 0x0044_4d04;

struct HotkeyContext {
    orchestrator: Arc<SessionOrchestrator>,
    view_scale: Arc<ViewScaleService>,
    marker_registered: bool,
    view_scale_registered: bool,
}

pub fn install(
    window: &WebviewWindow,
    orchestrator: Arc<SessionOrchestrator>,
    view_scale: Arc<ViewScaleService>,
) -> anyhow::Result<()> {
    let hwnd = HWND(window.hwnd().context("main window HWND is unavailable")?.0);
    let modifiers = MOD_CONTROL | MOD_ALT | MOD_NOREPEAT;

    unsafe {
        RegisterHotKey(
            Some(hwnd),
            TOGGLE_PAUSE_RESUME_ID,
            modifiers,
            u32::from(VK_F8.0),
        )
    }
    .context("failed to register Ctrl+Alt+F8")?;
    if let Err(error) =
        unsafe { RegisterHotKey(Some(hwnd), STOP_ID, modifiers, u32::from(VK_F10.0)) }
    {
        let _ = unsafe { UnregisterHotKey(Some(hwnd), TOGGLE_PAUSE_RESUME_ID) };
        return Err(error).context("failed to register Ctrl+Alt+F10");
    }

    // F9 intentionally stays outside every game Raw Input allowlist. WM_HOTKEY
    // becomes a separate marker fact; it must never masquerade as game input.
    let marker_registered =
        match unsafe { RegisterHotKey(Some(hwnd), ADD_MARKER_ID, modifiers, u32::from(VK_F9.0)) } {
            Ok(()) => true,
            Err(error) => {
                orchestrator.report_marker_hotkey_unavailable(&error);
                false
            }
        };

    // F11 与 F9 同纪律：留在所有游戏 Raw Input allowlist 之外，WM_HOTKEY
    // 是独立的对表标记事实，绝不冒充游戏输入。注册失败软降级——标定
    // 仍可用 UI 按钮起止。
    let view_scale_registered = match unsafe {
        RegisterHotKey(
            Some(hwnd),
            VIEW_SCALE_TOGGLE_ID,
            modifiers,
            u32::from(VK_F11.0),
        )
    } {
        Ok(()) => true,
        Err(error) => {
            tracing::warn!(error = %error, "Ctrl+Alt+F11 unavailable; view-scale measurements fall back to UI buttons");
            false
        }
    };

    let context = Box::into_raw(Box::new(HotkeyContext {
        orchestrator,
        view_scale,
        marker_registered,
        view_scale_registered,
    }));
    let installed = unsafe {
        SetWindowSubclass(
            hwnd,
            Some(hotkey_subclass_proc),
            SUBCLASS_ID,
            context as usize,
        )
    };
    if !installed.as_bool() {
        let _ = unsafe { UnregisterHotKey(Some(hwnd), TOGGLE_PAUSE_RESUME_ID) };
        let _ = unsafe { UnregisterHotKey(Some(hwnd), STOP_ID) };
        if marker_registered {
            let _ = unsafe { UnregisterHotKey(Some(hwnd), ADD_MARKER_ID) };
        }
        drop(unsafe { Box::from_raw(context) });
    }
    ensure!(
        installed.as_bool(),
        "failed to install global-hotkey subclass"
    );
    Ok(())
}

unsafe extern "system" fn hotkey_subclass_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _subclass_id: usize,
    ref_data: usize,
) -> LRESULT {
    if message == WM_HOTKEY && ref_data != 0 {
        let handled = catch_unwind(AssertUnwindSafe(|| {
            let context = unsafe { &*(ref_data as *const HotkeyContext) };
            let result = match wparam.0 as i32 {
                TOGGLE_PAUSE_RESUME_ID => QpcClock::new()
                    .and_then(|clock| clock.now())
                    .and_then(|qpc| context.orchestrator.toggle_pause_resume(Some(qpc)))
                    .map(|_| ()),
                STOP_ID => context.orchestrator.stop("global_hotkey").map(|_| ()),
                VIEW_SCALE_TOGGLE_ID => context.view_scale.toggle_measurement().map(|_| ()),
                ADD_MARKER_ID => QpcClock::new()
                    .and_then(|clock| clock.now())
                    .and_then(|qpc| {
                        context
                            .orchestrator
                            .add_hotkey_marker(qpc)
                            .map(|_| {
                                #[cfg(all(
                                    feature = "depth-lab",
                                    any(feature = "dev-catalog", test)
                                ))]
                                crate::services::cp2077_trial::note_metric_marker();
                            })
                            .map_err(|fault| anyhow::anyhow!("{}: {}", fault.code, fault.message))
                    }),
                _ => return false,
            };
            if let Err(error) = result {
                tracing::warn!(error = %error, "global hotkey action failed");
            }
            true
        }))
        .unwrap_or_else(|_| {
            tracing::error!("global hotkey handler panicked");
            true
        });
        if handled {
            return LRESULT(0);
        }
    }

    if message == WM_NCDESTROY {
        let _ = unsafe { UnregisterHotKey(Some(window), TOGGLE_PAUSE_RESUME_ID) };
        let _ = unsafe { UnregisterHotKey(Some(window), STOP_ID) };
        let context = (ref_data != 0).then(|| unsafe { &*(ref_data as *const HotkeyContext) });
        if context.is_some_and(|context| context.marker_registered) {
            let _ = unsafe { UnregisterHotKey(Some(window), ADD_MARKER_ID) };
        }
        if context.is_some_and(|context| context.view_scale_registered) {
            let _ = unsafe { UnregisterHotKey(Some(window), VIEW_SCALE_TOGGLE_ID) };
        }
        let _ = unsafe { RemoveWindowSubclass(window, Some(hotkey_subclass_proc), SUBCLASS_ID) };
        if ref_data != 0 {
            drop(unsafe { Box::from_raw(ref_data as *mut HotkeyContext) });
        }
    }
    unsafe { DefSubclassProc(window, message, wparam, lparam) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_hotkey_is_non_repeating_and_does_not_overlap_safety_controls() {
        assert_ne!(ADD_MARKER_ID, TOGGLE_PAUSE_RESUME_ID);
        assert_ne!(ADD_MARKER_ID, STOP_ID);
        assert_eq!(VK_F9.0, 120);
        assert_ne!(VK_F9.0, VK_F8.0);
        assert_ne!(VK_F9.0, VK_F10.0);
        assert_ne!((MOD_CONTROL | MOD_ALT | MOD_NOREPEAT).0 & MOD_NOREPEAT.0, 0);
    }
}

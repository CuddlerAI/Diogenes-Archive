use std::ffi::c_void;

use anyhow::{Context, ensure};
use tauri::WebviewWindow;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{
    GA_ROOTOWNER, GWL_EXSTYLE, GetAncestor, GetClientRect, GetForegroundWindow, GetWindowLongPtrW,
    HTTRANSPARENT, HWND_TOPMOST, IsIconic, IsWindow, IsWindowVisible, MA_NOACTIVATE, SW_HIDE,
    SW_SHOWNOACTIVATE, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOOWNERZORDER, SWP_NOSIZE,
    SWP_NOZORDER, SWP_SHOWWINDOW, SetWindowLongPtrW, SetWindowPos, ShowWindow, WM_MOUSEACTIVATE,
    WM_NCDESTROY, WM_NCHITTEST, WS_EX_APPWINDOW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_EX_TRANSPARENT,
};

pub const OVERLAY_LABEL: &str = "overlay";

const SUBCLASS_ID: usize = 0x444d_4f56;
const BASE_WIDTH_DIP: i32 = 420;
const BASE_HEIGHT_DIP: i32 = 96;
const MARGIN_DIP: i32 = 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OverlayTarget {
    pub hwnd_value: usize,
    pub root_owner_value: usize,
    pub expected_width: u32,
    pub expected_height: u32,
    pub expected_dpi: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OverlayBounds {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

pub fn configure_hud(window: &WebviewWindow) -> anyhow::Result<usize> {
    window
        .set_always_on_top(true)
        .context("failed to make Overlay topmost")?;
    window
        .set_ignore_cursor_events(true)
        .context("failed to make Overlay ignore cursor events")?;

    let tauri_hwnd = window.hwnd().context("Overlay HWND is unavailable")?;
    let hwnd_value = tauri_hwnd.0 as usize;
    let hwnd = hwnd(hwnd_value);
    ensure!(
        unsafe { IsWindow(Some(hwnd)) }.as_bool(),
        "Overlay HWND is invalid"
    );

    apply_click_through(hwnd_value)?;

    let installed = unsafe { SetWindowSubclass(hwnd, Some(overlay_subclass_proc), SUBCLASS_ID, 0) };
    ensure!(
        installed.as_bool(),
        "failed to install Overlay no-activate subclass"
    );
    Ok(hwnd_value)
}

fn apply_click_through(overlay_hwnd_value: usize) -> anyhow::Result<()> {
    let overlay = hwnd(overlay_hwnd_value);
    let current = unsafe { GetWindowLongPtrW(overlay, GWL_EXSTYLE) } as u32;
    let required = WS_EX_TOOLWINDOW.0 | WS_EX_NOACTIVATE.0 | WS_EX_TRANSPARENT.0;
    let next = (current | required) & !WS_EX_APPWINDOW.0;
    unsafe { SetWindowLongPtrW(overlay, GWL_EXSTYLE, next as isize) };
    unsafe {
        SetWindowPos(
            overlay,
            None,
            0,
            0,
            0,
            0,
            SWP_FRAMECHANGED
                | SWP_NOMOVE
                | SWP_NOSIZE
                | SWP_NOZORDER
                | SWP_NOACTIVATE
                | SWP_NOOWNERZORDER,
        )
    }
    .context("failed to apply Overlay interaction styles")?;
    let applied = unsafe { GetWindowLongPtrW(overlay, GWL_EXSTYLE) } as u32;
    ensure!(
        applied & required == required && applied & WS_EX_APPWINDOW.0 == 0,
        "Windows rejected required click-through Overlay styles: current=0x{current:08x}, requested=0x{next:08x}, applied=0x{applied:08x}"
    );
    Ok(())
}

pub fn place_hud(overlay_hwnd_value: usize, target: OverlayTarget) -> anyhow::Result<bool> {
    let overlay = hwnd(overlay_hwnd_value);
    ensure!(
        unsafe { IsWindow(Some(overlay)) }.as_bool(),
        "Overlay HWND was destroyed"
    );

    let Some(bounds) = target_bounds(target)? else {
        hide_hud(overlay_hwnd_value);
        return Ok(false);
    };
    unsafe {
        SetWindowPos(
            overlay,
            Some(HWND_TOPMOST),
            bounds.x,
            bounds.y,
            bounds.width,
            bounds.height,
            SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_SHOWWINDOW,
        )
    }
    .context("SetWindowPos failed for Overlay")?;
    let _ = unsafe { ShowWindow(overlay, SW_SHOWNOACTIVATE) };
    Ok(true)
}

pub fn hide_hud(overlay_hwnd_value: usize) {
    if overlay_hwnd_value == 0 {
        return;
    }
    let overlay = hwnd(overlay_hwnd_value);
    if unsafe { IsWindow(Some(overlay)) }.as_bool() {
        let _ = unsafe { ShowWindow(overlay, SW_HIDE) };
    }
}

pub fn target_bounds(target: OverlayTarget) -> anyhow::Result<Option<OverlayBounds>> {
    let target_hwnd = hwnd(target.hwnd_value);
    // Target lifecycle is owned by TargetWindowMonitor. During a normal close
    // the Overlay can observe the destroyed HWND before the recorder publishes
    // its Paused/rebind snapshot; hide for that tick instead of reporting an
    // Overlay subsystem failure that misleadingly says recording continues.
    if !unsafe { IsWindow(Some(target_hwnd)) }.as_bool() {
        return Ok(None);
    }
    if !unsafe { IsWindowVisible(target_hwnd) }.as_bool()
        || unsafe { IsIconic(target_hwnd) }.as_bool()
        || root_owner_value(unsafe { GetForegroundWindow() }) != target.root_owner_value
    {
        return Ok(None);
    }

    let mut client = RECT::default();
    unsafe { GetClientRect(target_hwnd, &mut client) }
        .context("GetClientRect failed for Overlay target")?;
    let width = (client.right - client.left).max(0) as u32;
    let height = (client.bottom - client.top).max(0) as u32;
    let dpi = unsafe { GetDpiForWindow(target_hwnd) };
    ensure!(
        width == target.expected_width
            && height == target.expected_height
            && dpi == target.expected_dpi,
        "bound target geometry or DPI changed before Overlay placement"
    );

    let mut top_left = POINT {
        x: client.left,
        y: client.top,
    };
    let mut bottom_right = POINT {
        x: client.right,
        y: client.bottom,
    };
    ensure!(
        unsafe { ClientToScreen(target_hwnd, &mut top_left) }.as_bool()
            && unsafe { ClientToScreen(target_hwnd, &mut bottom_right) }.as_bool(),
        "ClientToScreen failed for Overlay target"
    );
    Ok(Some(compute_bounds(
        top_left.x,
        top_left.y,
        bottom_right.x - top_left.x,
        bottom_right.y - top_left.y,
        dpi,
    )?))
}

fn compute_bounds(
    client_x: i32,
    client_y: i32,
    client_width: i32,
    client_height: i32,
    dpi: u32,
) -> anyhow::Result<OverlayBounds> {
    ensure!(
        client_width > 0 && client_height > 0,
        "target client area is empty"
    );
    ensure!(dpi > 0, "target DPI is invalid");
    let scale = |dip: i32| -> i32 { ((i64::from(dip) * i64::from(dpi) + 48) / 96) as i32 };
    let margin = scale(MARGIN_DIP);
    let width = scale(BASE_WIDTH_DIP);
    let height = scale(BASE_HEIGHT_DIP);
    ensure!(
        client_width >= width + margin * 2 && client_height >= height + margin * 2,
        "target client area is too small for the Overlay HUD"
    );
    Ok(OverlayBounds {
        x: client_x + client_width - width - margin,
        y: client_y + margin,
        width,
        height,
    })
}

fn hwnd(value: usize) -> HWND {
    HWND(value as *mut c_void)
}

fn root_owner_value(window: HWND) -> usize {
    if window.0.is_null() {
        return 0;
    }
    let root = unsafe { GetAncestor(window, GA_ROOTOWNER) };
    if root.0.is_null() {
        window.0 as usize
    } else {
        root.0 as usize
    }
}

unsafe extern "system" fn overlay_subclass_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _subclass_id: usize,
    _ref_data: usize,
) -> LRESULT {
    match message {
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
        WM_NCDESTROY => {
            let _ =
                unsafe { RemoveWindowSubclass(window, Some(overlay_subclass_proc), SUBCLASS_ID) };
            unsafe { DefSubclassProc(window, message, wparam, lparam) }
        }
        _ => unsafe { DefSubclassProc(window, message, wparam, lparam) },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlay_is_anchored_to_physical_client_top_right() {
        let bounds = compute_bounds(100, 50, 1920, 1080, 96).unwrap();
        assert_eq!(
            bounds,
            OverlayBounds {
                x: 1584,
                y: 66,
                width: 420,
                height: 96,
            }
        );
    }

    #[test]
    fn overlay_scales_with_target_dpi_without_changing_anchor() {
        let bounds = compute_bounds(-2560, 0, 2560, 1440, 144).unwrap();
        assert_eq!(bounds.width, 630);
        assert_eq!(bounds.height, 144);
        assert_eq!(bounds.x, -3214 + 2560);
        assert_eq!(bounds.y, 24);
    }

    #[test]
    fn overlay_rejects_a_client_area_that_cannot_contain_it() {
        assert!(compute_bounds(0, 0, 320, 200, 96).is_err());
    }
}

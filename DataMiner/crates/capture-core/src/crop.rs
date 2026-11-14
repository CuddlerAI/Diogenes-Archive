use anyhow::{Context, ensure};
use serde::Serialize;
use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::Graphics::Dwm::{DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute};
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::UI::WindowsAndMessaging::GetClientRect;

#[derive(Clone, Copy, Debug, Serialize)]
pub struct CropRect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl CropRect {
    #[must_use]
    pub fn width(self) -> u32 {
        (self.right - self.left) as u32
    }

    #[must_use]
    pub fn height(self) -> u32 {
        (self.bottom - self.top) as u32
    }

    pub fn validate_for_surface(
        self,
        surface_width: u32,
        surface_height: u32,
    ) -> anyhow::Result<()> {
        ensure!(
            self.left >= 0 && self.top >= 0,
            "client crop starts outside the capture surface"
        );
        ensure!(
            self.right > self.left && self.bottom > self.top,
            "client crop is empty"
        );
        ensure!(
            self.right as u32 <= surface_width && self.bottom as u32 <= surface_height,
            "client crop exceeds the capture surface"
        );
        Ok(())
    }
}

pub fn client_area_crop(hwnd: HWND) -> anyhow::Result<CropRect> {
    let mut client = RECT::default();
    unsafe { GetClientRect(hwnd, &mut client) }.context("GetClientRect failed")?;
    let mut origin = POINT {
        x: client.left,
        y: client.top,
    };
    ensure!(
        unsafe { ClientToScreen(hwnd, &mut origin) }.as_bool(),
        "ClientToScreen failed"
    );

    let extended = extended_frame_bounds(hwnd)?;

    Ok(CropRect {
        left: origin.x - extended.left,
        top: origin.y - extended.top,
        right: origin.x - extended.left + (client.right - client.left),
        bottom: origin.y - extended.top + (client.bottom - client.top),
    })
}

pub fn capture_surface_size(hwnd: HWND) -> anyhow::Result<(u32, u32)> {
    let bounds = extended_frame_bounds(hwnd)?;
    ensure!(
        bounds.right > bounds.left && bounds.bottom > bounds.top,
        "window capture bounds are empty"
    );
    Ok((
        (bounds.right - bounds.left) as u32,
        (bounds.bottom - bounds.top) as u32,
    ))
}

fn extended_frame_bounds(hwnd: HWND) -> anyhow::Result<RECT> {
    let mut extended = RECT::default();
    unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            (&mut extended as *mut RECT).cast(),
            std::mem::size_of::<RECT>() as u32,
        )
    }
    .context("DwmGetWindowAttribute(DWMWA_EXTENDED_FRAME_BOUNDS) failed")?;
    Ok(extended)
}

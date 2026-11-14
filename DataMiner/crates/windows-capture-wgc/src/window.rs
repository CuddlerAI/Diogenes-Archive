use windows::Graphics::Capture::GraphicsCaptureItem;
use windows::Win32::Foundation::HWND;
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;

/// Explicit window capture target. This fork has no other source type.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Window(HWND);

unsafe impl Send for Window {}

impl Window {
    /// Wrap a previously validated Win32 `HWND`.
    #[must_use]
    pub const fn from_raw_hwnd(hwnd: *mut core::ffi::c_void) -> Self {
        Self(HWND(hwnd))
    }

    #[must_use]
    pub const fn as_raw_hwnd(&self) -> *mut core::ffi::c_void {
        self.0.0
    }

    pub(crate) fn create_item(self) -> windows::core::Result<GraphicsCaptureItem> {
        let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        unsafe { interop.CreateForWindow(self.0) }
    }
}

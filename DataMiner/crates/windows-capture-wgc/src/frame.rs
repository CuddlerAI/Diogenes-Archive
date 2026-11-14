use windows::Foundation::TimeSpan;
use windows::Graphics::Capture::Direct3D11CaptureFrame;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_TEXTURE2D_DESC, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};

/// Borrowed WGC frame. It cannot outlive the frame-arrived callback.
pub struct Frame<'a> {
    capture_frame: Direct3D11CaptureFrame,
    device: &'a ID3D11Device,
    context: &'a ID3D11DeviceContext,
    texture: ID3D11Texture2D,
    desc: D3D11_TEXTURE2D_DESC,
    content_width: u32,
    content_height: u32,
}

impl<'a> Frame<'a> {
    pub(crate) const fn new(
        capture_frame: Direct3D11CaptureFrame,
        device: &'a ID3D11Device,
        context: &'a ID3D11DeviceContext,
        texture: ID3D11Texture2D,
        desc: D3D11_TEXTURE2D_DESC,
        content_width: u32,
        content_height: u32,
    ) -> Self {
        Self {
            capture_frame,
            device,
            context,
            texture,
            desc,
            content_width,
            content_height,
        }
    }

    #[must_use]
    pub const fn width(&self) -> u32 {
        self.desc.Width
    }

    #[must_use]
    pub const fn height(&self) -> u32 {
        self.desc.Height
    }

    #[must_use]
    pub const fn content_size(&self) -> (u32, u32) {
        (self.content_width, self.content_height)
    }

    pub fn timestamp(&self) -> windows::core::Result<TimeSpan> {
        self.capture_frame.SystemRelativeTime()
    }

    #[must_use]
    pub const fn as_raw_texture(&self) -> &ID3D11Texture2D {
        &self.texture
    }

    #[must_use]
    pub const fn device(&self) -> &ID3D11Device {
        self.device
    }

    #[must_use]
    pub const fn device_context(&self) -> &ID3D11DeviceContext {
        self.context
    }

    #[must_use]
    pub const fn desc(&self) -> &D3D11_TEXTURE2D_DESC {
        &self.desc
    }
}

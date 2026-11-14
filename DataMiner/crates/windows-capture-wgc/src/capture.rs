use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use parking_lot::Mutex;
use windows::Foundation::{Metadata::ApiInformation, TypedEventHandler};
use windows::Graphics::Capture::{
    Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_TEXTURE2D_DESC, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::System::Com::{CO_MTA_USAGE_COOKIE, CoDecrementMTAUsage, CoIncrementMTAUsage};
use windows::Win32::System::WinRT::Direct3D11::IDirect3DDxgiInterfaceAccess;
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize};
use windows::core::{HSTRING, IInspectable, Interface};

use crate::d3d11::create_direct3d_device;
use crate::frame::Frame;
use crate::window::Window;

/// Number of compositor frames WinRT may retain for one capture session.
///
/// Callers sizing a downstream queue need this value as burst headroom: the
/// minimum update interval controls cadence, but it does not prevent the full
/// frame pool from becoming observable after a scheduling delay.
pub const WGC_FRAME_POOL_BUFFER_COUNT: i32 = 3;

/// Settings for the only supported source: one explicit window.
pub struct WgcSettings<Flags> {
    pub window: Window,
    pub device: ID3D11Device,
    pub device_context: ID3D11DeviceContext,
    pub cursor_capture: bool,
    pub draw_border: bool,
    pub include_secondary_windows: bool,
    pub minimum_update_interval: Duration,
    pub flags: Flags,
}

unsafe impl<Flags: Send> Send for WgcSettings<Flags> {}

/// D3D objects and caller flags provided when the handler is built.
pub struct CaptureContext<Flags> {
    pub flags: Flags,
    pub device: ID3D11Device,
    pub device_context: ID3D11DeviceContext,
    pub surface_width: u32,
    pub surface_height: u32,
}

/// Callback-local stop handle.
#[derive(Clone)]
pub struct FrameControl {
    stop: SyncSender<()>,
}

impl FrameControl {
    pub fn request_stop(&self) {
        let _ = self.stop.try_send(());
    }
}

/// Consumer implemented by DataMiner's GPU-copy callback.
pub trait GraphicsCaptureHandler: Sized {
    type Flags: Send + 'static;
    type Error: Send + Sync + 'static;

    fn new(context: CaptureContext<Self::Flags>) -> Result<Self, Self::Error>;

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame<'_>,
        control: FrameControl,
    ) -> Result<(), Self::Error>;

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

enum Termination<E> {
    Handler(E),
    GeometryChanged {
        expected: (i32, i32),
        actual: (i32, i32),
    },
}

#[derive(Debug, thiserror::Error)]
pub enum CaptureError<E> {
    #[error("Windows Graphics Capture is unavailable")]
    Unsupported,
    #[error("failed to initialize WinRT: {0}")]
    WinRt(windows::core::Error),
    #[error("Windows capture API failed: {0}")]
    Windows(#[from] windows::core::Error),
    #[error("capture handler initialization failed")]
    HandlerInit(E),
    #[error("capture handler failed")]
    Handler(E),
    #[error("source geometry changed from {expected:?} to {actual:?}")]
    GeometryChanged {
        expected: (i32, i32),
        actual: (i32, i32),
    },
    #[error("capture control thread panicked")]
    ThreadPanicked,
    #[error("capture control thread exited before startup completed")]
    StartupChannelClosed,
}

/// Joinable lifetime handle for the single WGC control thread.
pub struct CaptureControl<H: GraphicsCaptureHandler> {
    join: Option<JoinHandle<Result<(), CaptureError<H::Error>>>>,
    stop: SyncSender<()>,
    callback: Arc<Mutex<H>>,
    halted: Arc<AtomicBool>,
}

impl<H: GraphicsCaptureHandler> CaptureControl<H> {
    #[must_use]
    pub fn callback(&self) -> Arc<Mutex<H>> {
        Arc::clone(&self.callback)
    }

    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.join.as_ref().is_none_or(JoinHandle::is_finished)
    }

    pub fn request_stop(&self) {
        self.halted.store(true, Ordering::Release);
        let _ = self.stop.try_send(());
    }

    pub fn wait(mut self) -> Result<(), CaptureError<H::Error>> {
        let Some(join) = self.join.take() else {
            return Ok(());
        };
        join.join().map_err(|_| CaptureError::ThreadPanicked)?
    }

    pub fn stop(self) -> Result<(), CaptureError<H::Error>> {
        self.request_stop();
        self.wait()
    }
}

impl<H: GraphicsCaptureHandler> Drop for CaptureControl<H> {
    fn drop(&mut self) {
        self.halted.store(true, Ordering::Release);
        let _ = self.stop.try_send(());
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

struct WinRtGuard {
    cookie: CO_MTA_USAGE_COOKIE,
}

impl WinRtGuard {
    fn new() -> Result<Self, windows::core::Error> {
        let cookie = unsafe { CoIncrementMTAUsage()? };
        let result = unsafe { RoInitialize(RO_INIT_MULTITHREADED) };
        if let Err(error) = result
            && error.code() != windows::Win32::Foundation::S_FALSE
        {
            let _ = unsafe { CoDecrementMTAUsage(cookie) };
            return Err(error);
        }
        Ok(Self { cookie })
    }
}

impl Drop for WinRtGuard {
    fn drop(&mut self) {
        unsafe { RoUninitialize() };
        let _ = unsafe { CoDecrementMTAUsage(self.cookie) };
    }
}

impl<H> CaptureControl<H>
where
    H: GraphicsCaptureHandler + Send + 'static,
{
    pub fn start(settings: WgcSettings<H::Flags>) -> Result<Self, CaptureError<H::Error>> {
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let (stop_tx, stop_rx) = mpsc::sync_channel(1);
        let stop_for_thread = stop_tx.clone();
        let halted = Arc::new(AtomicBool::new(false));
        let halted_for_thread = Arc::clone(&halted);

        let join = thread::Builder::new()
            .name("dm-wgc-control".into())
            .spawn(move || {
                let _winrt = WinRtGuard::new().map_err(CaptureError::WinRt)?;
                if !GraphicsCaptureSession::IsSupported()? {
                    return Err(CaptureError::Unsupported);
                }

                let item = settings.window.create_item()?;
                let initial_size = item.Size()?;
                let direct3d_device: IDirect3DDevice = create_direct3d_device(&settings.device)?;
                let frame_pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
                    &direct3d_device,
                    DirectXPixelFormat::B8G8R8A8UIntNormalized,
                    WGC_FRAME_POOL_BUFFER_COUNT,
                    initial_size,
                )?;
                let session = frame_pool.CreateCaptureSession(&item)?;

                if ApiInformation::IsPropertyPresent(
                    &HSTRING::from("Windows.Graphics.Capture.GraphicsCaptureSession"),
                    &HSTRING::from("IsCursorCaptureEnabled"),
                )? {
                    session.SetIsCursorCaptureEnabled(settings.cursor_capture)?;
                }
                if ApiInformation::IsPropertyPresent(
                    &HSTRING::from("Windows.Graphics.Capture.GraphicsCaptureSession"),
                    &HSTRING::from("IsBorderRequired"),
                )? {
                    session.SetIsBorderRequired(settings.draw_border)?;
                }
                if ApiInformation::IsPropertyPresent(
                    &HSTRING::from("Windows.Graphics.Capture.GraphicsCaptureSession"),
                    &HSTRING::from("IncludeSecondaryWindows"),
                )? {
                    session.SetIncludeSecondaryWindows(settings.include_secondary_windows)?;
                }
                if ApiInformation::IsPropertyPresent(
                    &HSTRING::from("Windows.Graphics.Capture.GraphicsCaptureSession"),
                    &HSTRING::from("MinUpdateInterval"),
                )? {
                    session.SetMinUpdateInterval(settings.minimum_update_interval.into())?;
                }

                let handler = H::new(CaptureContext {
                    flags: settings.flags,
                    device: settings.device.clone(),
                    device_context: settings.device_context.clone(),
                    surface_width: initial_size.Width as u32,
                    surface_height: initial_size.Height as u32,
                })
                .map_err(CaptureError::HandlerInit)?;
                let callback = Arc::new(Mutex::new(handler));
                let termination = Arc::new(Mutex::new(None));

                let closed_token = item.Closed(&TypedEventHandler::<
                    GraphicsCaptureItem,
                    IInspectable,
                >::new({
                    let callback = Arc::clone(&callback);
                    let termination = Arc::clone(&termination);
                    let stop = stop_for_thread.clone();
                    let halted = Arc::clone(&halted_for_thread);
                    move |_, _| {
                        halted.store(true, Ordering::Release);
                        if let Err(error) = callback.lock().on_closed() {
                            *termination.lock() = Some(Termination::Handler(error));
                        }
                        let _ = stop.try_send(());
                        Ok(())
                    }
                }))?;

                let frame_token = frame_pool.FrameArrived(&TypedEventHandler::<
                    Direct3D11CaptureFramePool,
                    IInspectable,
                >::new({
                    let callback = Arc::clone(&callback);
                    let termination = Arc::clone(&termination);
                    let stop = stop_for_thread.clone();
                    let halted = Arc::clone(&halted_for_thread);
                    let device = settings.device.clone();
                    let context = settings.device_context.clone();
                    move |sender, _| {
                        if halted.load(Ordering::Acquire) {
                            return Ok(());
                        }
                        let Some(sender) = sender.as_ref() else {
                            return Ok(());
                        };
                        let capture_frame = sender.TryGetNextFrame()?;
                        let content = capture_frame.ContentSize()?;
                        if content.Width != initial_size.Width
                            || content.Height != initial_size.Height
                        {
                            *termination.lock() = Some(Termination::GeometryChanged {
                                expected: (initial_size.Width, initial_size.Height),
                                actual: (content.Width, content.Height),
                            });
                            halted.store(true, Ordering::Release);
                            let _ = stop.try_send(());
                            return Ok(());
                        }

                        let surface = capture_frame.Surface()?;
                        let access = surface.cast::<IDirect3DDxgiInterfaceAccess>()?;
                        let texture = unsafe { access.GetInterface::<ID3D11Texture2D>()? };
                        let mut desc = D3D11_TEXTURE2D_DESC::default();
                        unsafe { texture.GetDesc(&mut desc) };
                        let mut frame = Frame::new(
                            capture_frame,
                            &device,
                            &context,
                            texture,
                            desc,
                            content.Width as u32,
                            content.Height as u32,
                        );
                        let control = FrameControl { stop: stop.clone() };
                        if let Err(error) = callback.lock().on_frame_arrived(&mut frame, control) {
                            *termination.lock() = Some(Termination::Handler(error));
                            halted.store(true, Ordering::Release);
                            let _ = stop.try_send(());
                        }
                        Ok(())
                    }
                }))?;

                session.StartCapture()?;
                ready_tx
                    .send(Arc::clone(&callback))
                    .map_err(|_| CaptureError::StartupChannelClosed)?;

                let _ = stop_rx.recv();
                halted_for_thread.store(true, Ordering::Release);
                let _ = frame_pool.RemoveFrameArrived(frame_token);
                let _ = item.RemoveClosed(closed_token);
                let _ = session.Close();
                let _ = frame_pool.Close();

                match termination.lock().take() {
                    Some(Termination::Handler(error)) => Err(CaptureError::Handler(error)),
                    Some(Termination::GeometryChanged { expected, actual }) => {
                        Err(CaptureError::GeometryChanged { expected, actual })
                    }
                    None => Ok(()),
                }
            })
            .map_err(|_| CaptureError::ThreadPanicked)?;

        let callback = match ready_rx.recv() {
            Ok(callback) => callback,
            Err(_) => {
                return match join.join() {
                    Ok(result) => Err(result.err().unwrap_or(CaptureError::StartupChannelClosed)),
                    Err(_) => Err(CaptureError::ThreadPanicked),
                };
            }
        };

        Ok(Self {
            join: Some(join),
            stop: stop_tx,
            callback,
            halted,
        })
    }
}

use std::ffi::c_void;
use std::fs::{self, File};
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, SyncSender};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, bail, ensure};
use capture_core::crop::CropRect;
use capture_core::gpu::{
    BgraToNv12, VideoConversionSpec, create_bgra_copy_texture, create_nv12_texture,
};
use capture_core::mf::{HardwareH264Encoder, MfPlatform};
use capture_core::profile::CaptureProfile;
use serde::Serialize;
use sha2::{Digest, Sha256};
use windows::Win32::Foundation::{HMODULE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ,
    D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
    D3D11CreateDeviceAndSwapChain, ID3D11Device, ID3D11DeviceContext, ID3D11DeviceContext1,
    ID3D11RenderTargetView, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_MODE_DESC, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    DXGI_SWAP_CHAIN_DESC, DXGI_USAGE_RENDER_TARGET_OUTPUT, IDXGISwapChain,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CS_HREDRAW, CS_OWNDC, CS_VREDRAW, CreateWindowExW, DefWindowProcW, DispatchMessageW,
    GetClientRect, GetSystemMetrics, MSG, PM_REMOVE, PeekMessageW, RegisterClassW, SM_CXSCREEN,
    SM_CYSCREEN, SW_SHOW, ShowWindow, TranslateMessage, WM_DESTROY, WM_QUIT, WNDCLASSW,
    WS_EX_TOPMOST, WS_POPUP, WS_VISIBLE,
};
use windows::core::{Interface, w};
use windows_capture_wgc::{
    CaptureContext, CaptureControl, Frame, FrameControl, GraphicsCaptureHandler, WgcSettings,
    Window,
};

const WIDTH: u32 = 1920;
const HEIGHT: u32 = 1088;
const ROI_HEIGHT: u32 = 8;
const ENCODE_HEIGHT: u32 = 1080;
type Pixel = [u8; 4];
type SourceSamples = (Pixel, Pixel, Pixel, Pixel);

struct TestWindow {
    hwnd: HWND,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    context1: ID3D11DeviceContext1,
    swapchain: IDXGISwapChain,
    backbuffer: ID3D11Texture2D,
    rtv: ID3D11RenderTargetView,
}

impl TestWindow {
    fn create() -> anyhow::Result<Self> {
        unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) }
            .context("SetProcessDpiAwarenessContext failed")?;
        let module = unsafe { GetModuleHandleW(None) }.context("GetModuleHandleW failed")?;
        let class = w!("DiogenesIssue750WgcProbe");
        let wnd_class = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW | CS_OWNDC,
            lpfnWndProc: Some(window_proc),
            hInstance: module.into(),
            lpszClassName: class,
            ..Default::default()
        };
        ensure!(
            unsafe { RegisterClassW(&wnd_class) } != 0,
            "RegisterClassW failed"
        );
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_TOPMOST,
                class,
                w!("Diogenes issue 750 WGC evidence - NON-TRAINABLE"),
                WS_POPUP | WS_VISIBLE,
                0,
                0,
                WIDTH as i32,
                HEIGHT as i32,
                None,
                None,
                Some(module.into()),
                None,
            )
        }
        .context("CreateWindowExW failed")?;

        let desc = DXGI_SWAP_CHAIN_DESC {
            BufferDesc: DXGI_MODE_DESC {
                Width: WIDTH,
                Height: HEIGHT,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                ..Default::default()
            },
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            OutputWindow: hwnd,
            Windowed: true.into(),
            ..Default::default()
        };
        let mut swapchain = None;
        let mut device = None;
        let mut context = None;
        unsafe {
            D3D11CreateDeviceAndSwapChain(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&desc),
                Some(&mut swapchain),
                Some(&mut device),
                None,
                Some(&mut context),
            )
        }
        .context("D3D11CreateDeviceAndSwapChain failed")?;
        let swapchain = swapchain.context("D3D11 returned no swapchain")?;
        let device = device.context("D3D11 returned no device")?;
        let context = context.context("D3D11 returned no immediate context")?;
        let context1 = context.cast().context("ID3D11DeviceContext1 unavailable")?;
        let backbuffer: ID3D11Texture2D =
            unsafe { swapchain.GetBuffer(0) }.context("IDXGISwapChain::GetBuffer failed")?;
        let mut rtv = None;
        unsafe { device.CreateRenderTargetView(&backbuffer, None, Some(&mut rtv)) }
            .context("CreateRenderTargetView failed")?;
        let rtv = rtv.context("CreateRenderTargetView returned null")?;
        let _ = unsafe { ShowWindow(hwnd, SW_SHOW) };
        let window = Self {
            hwnd,
            device,
            context,
            context1,
            swapchain,
            backbuffer,
            rtv,
        };
        window.render()?;
        Ok(window)
    }

    fn render(&self) -> anyhow::Result<()> {
        self.draw_pattern();
        unsafe { self.swapchain.Present(1, Default::default()).ok()? };
        Ok(())
    }

    fn draw_pattern(&self) {
        let full = [0.25, 0.50, 0.75, 1.0];
        unsafe { self.context.ClearRenderTargetView(&self.rtv, &full) };
        let alpha_rects = [
            (
                RECT {
                    left: 0,
                    top: 0,
                    right: 64,
                    bottom: ROI_HEIGHT as i32,
                },
                0.0,
            ),
            (
                RECT {
                    left: 64,
                    top: 0,
                    right: 128,
                    bottom: ROI_HEIGHT as i32,
                },
                247.0 / 255.0,
            ),
            (
                RECT {
                    left: 128,
                    top: 0,
                    right: 192,
                    bottom: ROI_HEIGHT as i32,
                },
                1.0,
            ),
        ];
        for (rect, alpha) in alpha_rects {
            unsafe {
                self.context1
                    .ClearView(&self.rtv, &[0.25, 0.50, 0.75, alpha], Some(&[rect]));
            }
        }
        let bottom = RECT {
            left: 0,
            top: 1080,
            right: WIDTH as i32,
            bottom: HEIGHT as i32,
        };
        unsafe {
            self.context1
                .ClearView(&self.rtv, &[1.0, 0.0, 1.0, 1.0], Some(&[bottom]));
        }
    }

    fn source_samples(&self) -> anyhow::Result<SourceSamples> {
        self.draw_pattern();
        let desc = D3D11_TEXTURE2D_DESC {
            Width: WIDTH,
            Height: HEIGHT,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut staging = None;
        unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut staging)) }?;
        let staging = staging.context("backbuffer staging texture is null")?;
        unsafe { self.context.CopyResource(&staging, &self.backbuffer) };
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe {
            self.context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
        }?;
        let read = |x: usize, y: usize| {
            let offset = y * mapped.RowPitch as usize + x * 4;
            unsafe {
                std::slice::from_raw_parts(mapped.pData.cast::<u8>().add(offset), 4)
                    .try_into()
                    .unwrap_or([0; 4])
            }
        };
        let samples = (read(16, 4), read(80, 4), read(144, 4), read(960, 1084));
        unsafe { self.context.Unmap(&staging, 0) };
        unsafe { self.swapchain.Present(1, Default::default()).ok()? };
        Ok(samples)
    }
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_DESTROY {
        return LRESULT(0);
    }
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

#[derive(Debug)]
struct CapturedFrame {
    width: u32,
    height: u32,
    content_width: u32,
    content_height: u32,
    row_pitch: u32,
    bgra: Vec<u8>,
}

struct CaptureHandler {
    tx: SyncSender<CapturedFrame>,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    sent: bool,
}

impl GraphicsCaptureHandler for CaptureHandler {
    type Flags = SyncSender<CapturedFrame>;
    type Error = anyhow::Error;

    fn new(context: CaptureContext<Self::Flags>) -> Result<Self, Self::Error> {
        Ok(Self {
            tx: context.flags,
            device: context.device,
            context: context.device_context,
            sent: false,
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame<'_>,
        control: FrameControl,
    ) -> Result<(), Self::Error> {
        if self.sent {
            control.request_stop();
            return Ok(());
        }
        let desc = D3D11_TEXTURE2D_DESC {
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
            ..*frame.desc()
        };
        let mut staging = None;
        unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut staging)) }
            .context("create WGC staging texture")?;
        let staging = staging.context("WGC staging texture is null")?;
        unsafe { self.context.CopyResource(&staging, frame.as_raw_texture()) };
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe {
            self.context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
        }
        .context("map WGC staging texture")?;
        let bytes = mapped.RowPitch as usize * desc.Height as usize;
        let bgra = unsafe { std::slice::from_raw_parts(mapped.pData.cast::<u8>(), bytes) }.to_vec();
        unsafe { self.context.Unmap(&staging, 0) };
        let (content_width, content_height) = frame.content_size();
        self.tx
            .send(CapturedFrame {
                width: desc.Width,
                height: desc.Height,
                content_width,
                content_height,
                row_pitch: mapped.RowPitch,
                bgra,
            })
            .context("send captured frame")?;
        self.sent = true;
        control.request_stop();
        Ok(())
    }
}

#[derive(Debug, Serialize)]
struct AlphaSample {
    x: u32,
    expected_backbuffer_alpha: u8,
    captured_bgra: [u8; 4],
}

#[derive(Debug, Serialize)]
struct Nv12Result {
    alpha_zero_sha256: String,
    alpha_255_sha256: String,
    different_bytes: usize,
    identical: bool,
}

#[derive(Debug, Serialize)]
struct H264Result {
    encoded_samples: usize,
    encoded_bytes: usize,
    clean_point_samples: usize,
    retained_input_slots: Vec<u32>,
}

#[derive(Debug, Serialize)]
struct Summary {
    schema: &'static str,
    desktop_primary: [i32; 2],
    requested_client: [u32; 2],
    actual_client: [i32; 2],
    wgc_surface: [u32; 2],
    wgc_content_size: [u32; 2],
    offscreen_bottom_rows_present: bool,
    bottom_row_bgra: [u8; 4],
    source_alpha_bgra: [[u8; 4]; 3],
    source_bottom_row_bgra: [u8; 4],
    alpha_samples: Vec<AlphaSample>,
    all_captured_alpha_opaque: bool,
    alpha_zero_rgb_preserved: bool,
    nv12_alpha_comparison: Nv12Result,
    h264: H264Result,
}

fn main() -> anyhow::Result<()> {
    let output = parse_output()?;
    fs::create_dir_all(&output)?;
    let window = TestWindow::create()?;
    let (source_alpha_0, source_alpha_247, source_alpha_255, source_bottom) =
        window.source_samples()?;
    let mut client = RECT::default();
    unsafe { GetClientRect(window.hwnd, &mut client) }.context("GetClientRect failed")?;

    let (tx, rx) = mpsc::sync_channel(1);
    let settings = WgcSettings {
        window: Window::from_raw_hwnd(window.hwnd.0),
        device: window.device.clone(),
        device_context: window.context.clone(),
        cursor_capture: false,
        draw_border: false,
        include_secondary_windows: false,
        minimum_update_interval: Duration::from_millis(16),
        flags: tx,
    };
    let control = CaptureControl::<CaptureHandler>::start(settings)
        .map_err(|error| anyhow::anyhow!("start WGC: {error}"))?;
    let deadline = Instant::now() + Duration::from_secs(10);
    let captured = loop {
        window.render()?;
        pump_messages();
        match rx.try_recv() {
            Ok(frame) => break frame,
            Err(mpsc::TryRecvError::Empty) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(16));
            }
            Err(mpsc::TryRecvError::Empty) => bail!("WGC frame timeout"),
            Err(mpsc::TryRecvError::Disconnected) => bail!("WGC handler disconnected"),
        }
    };
    control
        .stop()
        .map_err(|error| anyhow::anyhow!("stop WGC: {error}"))?;

    write_png(&output.join("captured.png"), &captured)?;
    let alpha_samples = [16_u32, 80, 144]
        .into_iter()
        .zip([0_u8, 247, 255])
        .map(|(x, expected)| AlphaSample {
            x,
            expected_backbuffer_alpha: expected,
            captured_bgra: pixel(&captured, x, 4),
        })
        .collect::<Vec<_>>();
    let bottom_row_bgra = if captured.height > 1080 {
        pixel(&captured, WIDTH / 2, 1084)
    } else {
        [0; 4]
    };
    let (nv12, h264) = compare_and_encode_alpha(&window.device, &window.context)?;
    let summary = Summary {
        schema: "diogenes.issue750.wgc-alpha-oversize/v1",
        desktop_primary: unsafe { [GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)] },
        requested_client: [WIDTH, HEIGHT],
        actual_client: [client.right - client.left, client.bottom - client.top],
        wgc_surface: [captured.width, captured.height],
        wgc_content_size: [captured.content_width, captured.content_height],
        offscreen_bottom_rows_present: captured.height >= HEIGHT
            && bottom_row_bgra[0] >= 240
            && bottom_row_bgra[1] <= 15
            && bottom_row_bgra[2] >= 240,
        bottom_row_bgra,
        source_alpha_bgra: [source_alpha_0, source_alpha_247, source_alpha_255],
        source_bottom_row_bgra: source_bottom,
        all_captured_alpha_opaque: alpha_samples
            .iter()
            .all(|sample| sample.captured_bgra[3] == 255),
        alpha_zero_rgb_preserved: {
            let sample = alpha_samples[0].captured_bgra;
            sample[0] >= 180 && sample[1] >= 120 && sample[2] >= 55
        },
        alpha_samples,
        nv12_alpha_comparison: nv12,
        h264,
    };
    let path = output.join("summary.json");
    serde_json::to_writer_pretty(BufWriter::new(File::create(&path)?), &summary)?;
    println!("{}", serde_json::to_string_pretty(&summary)?);
    Ok(())
}

fn parse_output() -> anyhow::Result<PathBuf> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() == 1 {
        return Ok(PathBuf::from("evidence/wgc-alpha-oversize"));
    }
    ensure!(
        args.len() == 3 && args[1] == "--output",
        "usage: issue750-wgc-probe [--output PATH]"
    );
    Ok(PathBuf::from(&args[2]))
}

fn pump_messages() {
    let mut message = MSG::default();
    while unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
        if message.message == WM_QUIT {
            break;
        }
        unsafe {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

fn pixel(frame: &CapturedFrame, x: u32, y: u32) -> [u8; 4] {
    if x >= frame.width || y >= frame.height {
        return [0; 4];
    }
    let offset = y as usize * frame.row_pitch as usize + x as usize * 4;
    frame.bgra[offset..offset + 4].try_into().unwrap_or([0; 4])
}

fn write_png(path: &Path, frame: &CapturedFrame) -> anyhow::Result<()> {
    let mut rgba = Vec::with_capacity(frame.width as usize * frame.height as usize * 4);
    for y in 0..frame.height {
        for x in 0..frame.width {
            let [b, g, r, a] = pixel(frame, x, y);
            rgba.extend_from_slice(&[r, g, b, a]);
        }
    }
    let mut encoder = png::Encoder::new(
        BufWriter::new(File::create(path)?),
        frame.width,
        frame.height,
    );
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.write_header()?.write_image_data(&rgba)?;
    Ok(())
}

fn compare_and_encode_alpha(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
) -> anyhow::Result<(Nv12Result, H264Result)> {
    let inputs = [
        create_bgra_copy_texture(device, WIDTH, ENCODE_HEIGHT)?,
        create_bgra_copy_texture(device, WIDTH, ENCODE_HEIGHT)?,
    ];
    for (index, alpha) in [0_u8, 255].into_iter().enumerate() {
        let mut pixels = vec![0_u8; WIDTH as usize * ENCODE_HEIGHT as usize * 4];
        for pixel in pixels.as_chunks_mut::<4>().0 {
            pixel.copy_from_slice(&[191, 128, 64, alpha]);
        }
        unsafe {
            context.UpdateSubresource(
                &inputs[index],
                0,
                None,
                pixels.as_ptr().cast::<c_void>(),
                WIDTH * 4,
                WIDTH * ENCODE_HEIGHT * 4,
            );
        }
    }
    let outputs = [
        create_nv12_texture(device, WIDTH, ENCODE_HEIGHT)?,
        create_nv12_texture(device, WIDTH, ENCODE_HEIGHT)?,
    ];
    let converter = BgraToNv12::new(
        device,
        context,
        VideoConversionSpec {
            surface_width: WIDTH,
            surface_height: ENCODE_HEIGHT,
            crop: CropRect {
                left: 0,
                top: 0,
                right: WIDTH as i32,
                bottom: ENCODE_HEIGHT as i32,
            },
            output_width: WIDTH,
            output_height: ENCODE_HEIGHT,
            fps_num: 60,
            fps_den: 1,
        },
    )?;
    let surfaces = converter.prepare_surfaces(&inputs, &outputs)?;
    converter.convert(&surfaces, 0, 0, 0)?;
    converter.convert(&surfaces, 1, 1, 1)?;
    unsafe { context.Flush() };
    let zero = read_nv12(device, context, &outputs[0])?;
    let opaque = read_nv12(device, context, &outputs[1])?;
    let different_bytes = zero.iter().zip(&opaque).filter(|(a, b)| a != b).count();
    let nv12 = Nv12Result {
        alpha_zero_sha256: sha256(&zero),
        alpha_255_sha256: sha256(&opaque),
        different_bytes,
        identical: zero == opaque,
    };

    let _mf = MfPlatform::startup()?;
    let profile = CaptureProfile {
        profile_id: "issue750-alpha-h264-non-trainable".into(),
        non_trainable: true,
        capture_backend: "native_wgc_window_v1".into(),
        encoder_backend: "mf_nvidia_hardware_v1".into(),
        width: WIDTH,
        height: ENCODE_HEIGHT,
        fps_num: 60,
        fps_den: 1,
        bitrate_bps: 12_000_000,
        max_bitrate_bps: 18_000_000,
        gop_frames: 240,
        segment_seconds: 10,
        test_segments: 1,
        accepted_encoder_relaxations: None,
        audio: None,
    };
    let mut encoder = HardwareH264Encoder::create(device, &profile)?;
    encoder.write_texture(&outputs[0], 0, 0, true)?;
    encoder.write_texture(&outputs[1], 1, 1, false)?;
    let finished = encoder.finish()?;
    let h264 = H264Result {
        encoded_samples: finished.encoded_samples.len(),
        encoded_bytes: finished
            .encoded_samples
            .iter()
            .map(|sample| sample.data.len())
            .sum(),
        clean_point_samples: finished
            .encoded_samples
            .iter()
            .filter(|sample| sample.clean_point)
            .count(),
        retained_input_slots: finished.retained_slots,
    };
    ensure!(
        h264.encoded_samples == 2,
        "hardware H.264 did not return two samples"
    );
    ensure!(h264.encoded_bytes > 0, "hardware H.264 output is empty");
    ensure!(
        h264.retained_input_slots.is_empty(),
        "hardware H.264 retained input slots"
    );
    Ok((nv12, h264))
}

fn read_nv12(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    source: &ID3D11Texture2D,
) -> anyhow::Result<Vec<u8>> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: WIDTH,
        Height: ENCODE_HEIGHT,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_NV12,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };
    let mut staging = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut staging)) }?;
    let staging = staging.context("NV12 staging texture is null")?;
    unsafe { context.CopyResource(&staging, source) };
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    unsafe { context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)) }?;
    let bytes = mapped.RowPitch as usize * (ENCODE_HEIGHT as usize + ENCODE_HEIGHT as usize / 2);
    let output = unsafe { std::slice::from_raw_parts(mapped.pData.cast::<u8>(), bytes) }.to_vec();
    unsafe { context.Unmap(&staging, 0) };
    Ok(output)
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

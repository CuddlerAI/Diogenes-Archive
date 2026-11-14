//! Bisect where the collector's per-frame handle growth comes from, without the game.
//!
//! Capture 24 recorded the collector's handle count climbing 1264 -> 10476 over
//! 307.6 s (29.96 handles/s) while thread count stayed bounded and private bytes
//! grew much more slowly. That rate is close to one handle per recording frame,
//! but the sampler only recorded a count, so it cannot say which kernel object
//! type accumulates, and it cannot say which pipeline stage creates it.
//!
//! This probe narrows both questions on the same machine without Fallout4, the
//! depth probe, present-identity, audio, or input: it renders its own window and
//! runs the production capture path against it in three cumulative stages.
//!
//!   wgc     WGC frame arrives -> CopyResource into the BGRA ring -> D3D11 event
//!           query -> recycle. Mirrors `capture_core::capture`'s ingress.
//!   nv12    ... plus `BgraToNv12` into the NV12 pool, exactly as `record_inner`
//!           does (convert into the canonical slot, CopyResource into an output
//!           slot).
//!   encode  ... plus `HardwareH264Encoder::write_texture` and the tracked-sample
//!           release/encoded drains.
//!
//! Every sample can record `GetProcessHandleCount` and a non-atomic type census.
//! A stage-dependent increase narrows the investigation; identifying ownership
//! requires lifetime/stack evidence. Neither similar slopes nor non-reproduction
//! prove whether a synthetic result has the same cause as the original game run.
//!
//! This is a diagnostic tool, not a capture backend. It captures only its own
//! window, writes no session, run, manifest or artifact, and produces nothing
//! that could be mistaken for recorded data.

mod census;

use std::collections::{BTreeMap, VecDeque};
use std::ffi::c_void;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail, ensure};
use capture_core::capture::BGRA_RING_SIZE;
use capture_core::crop::CropRect;
use capture_core::d3d::D3dContext;
use capture_core::gpu::{
    BgraToNv12, VideoConversionSpec, create_bgra_copy_texture, create_nv12_texture,
};
use capture_core::mf::{HardwareH264Encoder, MfPlatform};
use capture_core::profile::CaptureProfile;
use serde::Serialize;
use serde_json::json;
use windows::Win32::Foundation::{HMODULE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_QUERY_DESC, D3D11_QUERY_EVENT, D3D11_SDK_VERSION,
    D3D11CreateDeviceAndSwapChain, ID3D11Device, ID3D11DeviceContext, ID3D11Query,
    ID3D11RenderTargetView, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_MODE_DESC, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    DXGI_SWAP_CHAIN_DESC, DXGI_USAGE_RENDER_TARGET_OUTPUT, IDXGISwapChain,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CS_HREDRAW, CS_OWNDC, CS_VREDRAW, CreateWindowExW, DefWindowProcW, DestroyWindow,
    DispatchMessageW, MSG, PM_REMOVE, PeekMessageW, RegisterClassW, SW_SHOWNA, ShowWindow,
    TranslateMessage, WM_DESTROY, WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP,
};
use windows::core::{BOOL, w};

use crate::census::{HandleCensus, diff_by_type};

const SCHEMA: &str = "diogenes.handle-growth-probe/v2";
/// Capture 24 ran 1920x1080; the ring and NV12 pool sizes are production's.
const WIDTH: u32 = 1920;
const HEIGHT: u32 = 1080;
/// `record_inner` keeps NV12 slot 0 canonical and encodes from slots 1..N.
const NV12_POOL_SIZE: usize = 6;
const NV12_CANONICAL_SLOT: usize = 0;
const GPU_COPY_WAIT: Duration = Duration::from_millis(500);
const NV12_SLOT_WAIT: Duration = Duration::from_millis(500);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Stage {
    Wgc,
    Nv12,
    Encode,
}

impl Stage {
    const fn name(self) -> &'static str {
        match self {
            Self::Wgc => "wgc",
            Self::Nv12 => "nv12",
            Self::Encode => "encode",
        }
    }

    fn parse(value: &str) -> anyhow::Result<Self> {
        match value {
            "wgc" => Ok(Self::Wgc),
            "nv12" => Ok(Self::Nv12),
            "encode" => Ok(Self::Encode),
            other => bail!("unknown stage `{other}`; expected wgc, nv12 or encode"),
        }
    }
}

struct Options {
    output: PathBuf,
    stages: Vec<Stage>,
    stage_seconds: u64,
    settle_seconds: u64,
    sample_seconds: u64,
    fps: u32,
    census: bool,
    max_handle_value: u32,
}

fn parse_options() -> anyhow::Result<Options> {
    let mut output = None;
    let mut stages = vec![Stage::Wgc, Stage::Nv12, Stage::Encode];
    let mut stage_seconds = 60_u64;
    let mut settle_seconds = 8_u64;
    let mut sample_seconds = 3_u64;
    let mut fps = 30_u32;
    let mut census = true;
    let mut max_handle_value = census::DEFAULT_MAX_HANDLE_VALUE;

    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut index = 0;
    while index < args.len() {
        let flag = args[index].as_str();
        let mut value = || -> anyhow::Result<String> {
            index += 1;
            args.get(index)
                .cloned()
                .with_context(|| format!("{flag} needs a value"))
        };
        match flag {
            "--output" => output = Some(PathBuf::from(value()?)),
            "--stages" => {
                stages = value()?
                    .split(',')
                    .map(str::trim)
                    .filter(|part| !part.is_empty())
                    .map(Stage::parse)
                    .collect::<anyhow::Result<Vec<_>>>()?;
            }
            "--stage-seconds" => stage_seconds = value()?.parse().context("--stage-seconds")?,
            "--settle-seconds" => settle_seconds = value()?.parse().context("--settle-seconds")?,
            "--sample-seconds" => sample_seconds = value()?.parse().context("--sample-seconds")?,
            "--fps" => fps = value()?.parse().context("--fps")?,
            "--no-census" => census = false,
            "--max-handle-value" => {
                max_handle_value = value()?.parse().context("--max-handle-value")?;
            }
            "--help" | "-h" => {
                println!("{}", usage());
                std::process::exit(0);
            }
            other => bail!("unknown argument `{other}`\n\n{}", usage()),
        }
        index += 1;
    }

    let output = output.context(usage())?;
    ensure!(!stages.is_empty(), "--stages selected no stage");
    ensure!(fps > 0, "--fps must be positive");
    ensure!(
        stage_seconds > settle_seconds,
        "--stage-seconds must exceed --settle-seconds so a slope window remains"
    );
    ensure!(sample_seconds > 0, "--sample-seconds must be positive");
    Ok(Options {
        output,
        stages,
        stage_seconds,
        settle_seconds,
        sample_seconds,
        fps,
        census,
        max_handle_value,
    })
}

fn usage() -> String {
    "usage: handle-growth-probe --output <new directory> \
     [--stages wgc,nv12,encode] [--stage-seconds 60] [--settle-seconds 8] \
     [--sample-seconds 3] [--fps 30] [--no-census] [--max-handle-value N]"
        .to_owned()
}

// ---------------------------------------------------------------------------
// Evidence journal
// ---------------------------------------------------------------------------

struct Journal {
    writer: Mutex<BufWriter<File>>,
    started: Instant,
}

impl Journal {
    fn create(path: &Path) -> anyhow::Result<Self> {
        let file = File::create_new(path)
            .with_context(|| format!("failed to create {}", path.display()))?;
        Ok(Self {
            writer: Mutex::new(BufWriter::new(file)),
            started: Instant::now(),
        })
    }

    fn emit(&self, kind: &str, value: serde_json::Value) -> anyhow::Result<()> {
        let record = json!({
            "schema": SCHEMA,
            "kind": kind,
            "utc_unix_ms": unix_millis(),
            "elapsed_ms": self.started.elapsed().as_millis() as u64,
            "value": value,
        });
        let mut writer = self
            .writer
            .lock()
            .map_err(|_| anyhow::anyhow!("evidence journal mutex was poisoned"))?;
        serde_json::to_writer(&mut *writer, &record)?;
        writer.write_all(b"\n")?;
        writer.flush()?;
        Ok(())
    }
}

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Self-rendered capture source
// ---------------------------------------------------------------------------

struct RenderWindow {
    hwnd: HWND,
    context: ID3D11DeviceContext,
    swapchain: IDXGISwapChain,
    rtv: ID3D11RenderTargetView,
    _device: ID3D11Device,
}

impl RenderWindow {
    fn create() -> anyhow::Result<Self> {
        let _ =
            unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        let instance = unsafe { GetModuleHandleW(None) }.context("GetModuleHandleW failed")?;
        let class = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW | CS_OWNDC,
            lpfnWndProc: Some(window_proc),
            hInstance: instance.into(),
            lpszClassName: w!("DiogenesHandleGrowthProbe"),
            ..Default::default()
        };
        ensure!(
            unsafe { RegisterClassW(&raw const class) } != 0,
            "RegisterClassW failed"
        );
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                w!("DiogenesHandleGrowthProbe"),
                w!("Diogenes handle-growth probe"),
                WS_POPUP,
                0,
                0,
                WIDTH as i32,
                HEIGHT as i32,
                None,
                None,
                Some(instance.into()),
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
        .context("D3D11CreateDeviceAndSwapChain failed for the probe source window")?;
        let swapchain = swapchain.context("D3D11 returned no swapchain")?;
        let device = device.context("D3D11 returned no source device")?;
        let context = context.context("D3D11 returned no source context")?;
        let backbuffer: ID3D11Texture2D =
            unsafe { swapchain.GetBuffer(0) }.context("IDXGISwapChain::GetBuffer failed")?;
        let mut rtv = None;
        unsafe { device.CreateRenderTargetView(&backbuffer, None, Some(&mut rtv)) }
            .context("CreateRenderTargetView failed")?;
        let rtv = rtv.context("CreateRenderTargetView returned null")?;
        // SW_SHOWNA keeps the operator's focus where it is; WGC captures the
        // window's own composed content whether or not it is foreground.
        let _ = unsafe { ShowWindow(hwnd, SW_SHOWNA) };
        Ok(Self {
            hwnd,
            context,
            swapchain,
            rtv,
            _device: device,
        })
    }

    /// Present a frame whose content changes every time, so the compositor
    /// always has a new frame to hand WGC.
    fn render(&self, index: u64) -> anyhow::Result<()> {
        let phase = (index % 60) as f32 / 60.0;
        unsafe {
            self.context
                .ClearRenderTargetView(&self.rtv, &[phase, 1.0 - phase, 0.5, 1.0]);
            self.swapchain.Present(0, Default::default()).ok()?;
        }
        Ok(())
    }
}

impl Drop for RenderWindow {
    fn drop(&mut self) {
        // Called on the thread that created this diagnostic-only window.
        let _ = unsafe { DestroyWindow(self.hwnd) };
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

fn pump_messages() {
    let mut message = MSG::default();
    while unsafe { PeekMessageW(&raw mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
        unsafe {
            let _ = TranslateMessage(&raw const message);
            DispatchMessageW(&raw const message);
        }
    }
}

// ---------------------------------------------------------------------------
// WGC ingress, mirroring capture_core::capture
// ---------------------------------------------------------------------------

struct RingFlags {
    textures: Vec<ID3D11Texture2D>,
    queries: Vec<ID3D11Query>,
    frame_tx: SyncSender<usize>,
    recycle_rx: Receiver<usize>,
    arrived: Arc<AtomicU64>,
    starved: Arc<AtomicU64>,
}

struct RingHandler {
    context: ID3D11DeviceContext,
    textures: Vec<ID3D11Texture2D>,
    queries: Vec<ID3D11Query>,
    frame_tx: SyncSender<usize>,
    recycle_rx: Receiver<usize>,
    arrived: Arc<AtomicU64>,
    starved: Arc<AtomicU64>,
    free: VecDeque<usize>,
}

impl windows_capture_wgc::GraphicsCaptureHandler for RingHandler {
    type Flags = RingFlags;
    type Error = anyhow::Error;

    fn new(context: windows_capture_wgc::CaptureContext<Self::Flags>) -> Result<Self, Self::Error> {
        let RingFlags {
            textures,
            queries,
            frame_tx,
            recycle_rx,
            arrived,
            starved,
        } = context.flags;
        let free = (0..textures.len()).collect();
        Ok(Self {
            context: context.device_context,
            textures,
            queries,
            frame_tx,
            recycle_rx,
            arrived,
            starved,
            free,
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut windows_capture_wgc::Frame<'_>,
        _control: windows_capture_wgc::FrameControl,
    ) -> Result<(), Self::Error> {
        self.arrived.fetch_add(1, Ordering::Relaxed);
        while let Ok(slot) = self.recycle_rx.try_recv() {
            self.free.push_back(slot);
        }
        // Production stops the run on ring starvation. A diagnostic must keep
        // sampling instead, so starvation is counted and the frame dropped.
        let Some(slot) = self.free.pop_front() else {
            self.starved.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        };
        unsafe {
            self.context
                .CopyResource(&self.textures[slot], frame.as_raw_texture());
            self.context.End(&self.queries[slot]);
        }
        if self.frame_tx.try_send(slot).is_err() {
            self.starved.fetch_add(1, Ordering::Relaxed);
            self.free.push_back(slot);
        }
        Ok(())
    }
}

/// Poll the ring slot's event query the same way `wait_for_gpu_copy_completion`
/// does: read the BOOL out-parameter, because `GetData` reports "not ready" as
/// S_FALSE, which is a success HRESULT.
fn wait_for_copy(
    context: &ID3D11DeviceContext,
    query: &ID3D11Query,
    limit: Duration,
) -> anyhow::Result<bool> {
    let deadline = Instant::now() + limit;
    loop {
        let mut completed = BOOL(0);
        unsafe {
            context.GetData(
                query,
                Some((&raw mut completed).cast::<c_void>()),
                std::mem::size_of::<BOOL>() as u32,
                0,
            )
        }
        .context("D3D11 GPU-copy completion query failed")?;
        if completed.as_bool() {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        thread::yield_now();
    }
}

// ---------------------------------------------------------------------------
// Stage runner
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct StageReport {
    stage: &'static str,
    seconds: f64,
    frames_arrived: u64,
    frames_processed: u64,
    frames_submitted: u64,
    frames_output: u64,
    /// Frames processed inside the slope window, i.e. after the settle sample.
    window_frames_processed: u64,
    ring_starved: u64,
    nv12_starved: u64,
    copy_timeouts: u64,
    settle_handles: u32,
    end_handles: u32,
    handle_delta: i64,
    handles_per_second: f64,
    handles_per_processed_frame: f64,
    type_delta: BTreeMap<String, i64>,
    type_delta_complete: bool,
    measurement_healthy: bool,
}

struct StageOutcome {
    report: StageReport,
    settle_census: Option<HandleCensus>,
    end_census: Option<HandleCensus>,
}

#[allow(clippy::too_many_lines)]
fn run_stage(
    stage: Stage,
    options: &Options,
    journal: &Journal,
    d3d: &D3dContext,
    profile: &CaptureProfile,
    hwnd_value: usize,
) -> anyhow::Result<StageOutcome> {
    let mut textures = Vec::with_capacity(BGRA_RING_SIZE);
    let mut queries = Vec::with_capacity(BGRA_RING_SIZE);
    for _ in 0..BGRA_RING_SIZE {
        textures.push(create_bgra_copy_texture(&d3d.device, WIDTH, HEIGHT)?);
        let query_desc = D3D11_QUERY_DESC {
            Query: D3D11_QUERY_EVENT,
            MiscFlags: 0,
        };
        let mut query = None;
        unsafe {
            d3d.device
                .CreateQuery(&raw const query_desc, Some(&mut query))
        }
        .context("failed to create a GPU-copy completion query")?;
        queries.push(query.context("D3D11 returned no GPU-copy completion query")?);
    }

    let nv12_textures = (0..NV12_POOL_SIZE)
        .map(|_| create_nv12_texture(&d3d.device, WIDTH, HEIGHT))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let converter = BgraToNv12::new(
        &d3d.device,
        &d3d.immediate_context,
        VideoConversionSpec {
            surface_width: WIDTH,
            surface_height: HEIGHT,
            crop: CropRect {
                left: 0,
                top: 0,
                right: WIDTH as i32,
                bottom: HEIGHT as i32,
            },
            output_width: WIDTH,
            output_height: HEIGHT,
            fps_num: profile.fps_num,
            fps_den: profile.fps_den,
        },
    )?;
    let surfaces = converter.prepare_surfaces(&textures, &nv12_textures)?;

    let mut encoder = match stage {
        Stage::Encode => Some(HardwareH264Encoder::create(&d3d.device, profile)?),
        Stage::Wgc | Stage::Nv12 => None,
    };

    let (frame_tx, frame_rx) = mpsc::sync_channel::<usize>(BGRA_RING_SIZE);
    let (recycle_tx, recycle_rx) = mpsc::channel::<usize>();
    let arrived = Arc::new(AtomicU64::new(0));
    let starved = Arc::new(AtomicU64::new(0));

    let settings = windows_capture_wgc::WgcSettings {
        window: windows_capture_wgc::Window::from_raw_hwnd(hwnd_value as *mut c_void),
        device: d3d.device.clone(),
        device_context: d3d.immediate_context.clone(),
        cursor_capture: false,
        draw_border: false,
        include_secondary_windows: false,
        minimum_update_interval: Duration::from_micros(
            1_000_000 * u64::from(profile.fps_den) / u64::from(profile.fps_num.max(1)),
        ),
        flags: RingFlags {
            textures: textures.clone(),
            queries: queries.clone(),
            frame_tx,
            recycle_rx,
            arrived: Arc::clone(&arrived),
            starved: Arc::clone(&starved),
        },
    };
    let control = windows_capture_wgc::CaptureControl::<RingHandler>::start(settings)
        .map_err(|error| anyhow::anyhow!("WGC start failed: {error}"))?;

    let stage_started = Instant::now();
    let settle_at = stage_started + Duration::from_secs(options.settle_seconds);
    let end_at = stage_started + Duration::from_secs(options.stage_seconds);

    let mut free_nv12: VecDeque<usize> = (1..NV12_POOL_SIZE).collect();
    let mut processed = 0_u64;
    let mut encoded_frames = 0_u64;
    let mut frames_output = 0_u64;
    let mut copy_timeouts = 0_u64;
    let mut nv12_starved = 0_u64;
    let mut session_frame = 0_u64;

    let mut settle_census = None;
    let mut settle_handles = 0_u32;
    let mut settle_instant = stage_started;
    let mut processed_at_settle = 0_u64;
    let mut next_sample = stage_started;
    let mut settled = false;

    loop {
        let now = Instant::now();
        if now >= end_at {
            break;
        }
        if !settled && now >= settle_at {
            settled = true;
            settle_instant = now;
            processed_at_settle = processed;
            let sample = sample_now(options, journal, stage, "settle", &arrived, processed)?;
            settle_handles = sample.0;
            settle_census = sample.1;
            // The slope window starts here; do not immediately census again.
            next_sample = now + Duration::from_secs(options.sample_seconds);
            continue;
        }
        if now >= next_sample {
            let sample = sample_now(options, journal, stage, "sample", &arrived, processed)?;
            if !settled {
                settle_handles = sample.0;
            }
            next_sample = now + Duration::from_secs(options.sample_seconds);
        }

        let slot = match frame_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(slot) => slot,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                bail!("WGC ingress channel disconnected before the stage ended")
            }
        };
        if !wait_for_copy(&d3d.immediate_context, &queries[slot], GPU_COPY_WAIT)? {
            copy_timeouts += 1;
            // Never recycle or encode a surface whose GPU copy is unfinished.
            break;
        }
        if stage != Stage::Wgc {
            converter.convert(&surfaces, slot, NV12_CANONICAL_SLOT, session_frame as u32)?;
            if let Some(encoder) = encoder.as_mut() {
                for released in encoder.take_released()? {
                    free_nv12.push_back(released as usize);
                }
                let output_slot = match free_nv12.pop_front() {
                    Some(output_slot) => Some(output_slot),
                    None => wait_for_nv12_slot(encoder, &mut free_nv12)?,
                };
                match output_slot {
                    Some(output_slot) => {
                        unsafe {
                            d3d.immediate_context.CopyResource(
                                &nv12_textures[output_slot],
                                &nv12_textures[NV12_CANONICAL_SLOT],
                            );
                        }
                        let force_keyframe =
                            session_frame.is_multiple_of(u64::from(profile.gop_frames));
                        encoder.write_texture(
                            &nv12_textures[output_slot],
                            output_slot as u32,
                            session_frame,
                            force_keyframe,
                        )?;
                        session_frame += 1;
                        encoded_frames += 1;
                        // Drain the grabbed output so encoded samples cannot be
                        // mistaken for the thing that accumulates.
                        frames_output += encoder.take_encoded()?.len() as u64;
                    }
                    None => nv12_starved += 1,
                }
            } else {
                // Without an encoder there is no release callback to return
                // slots, so round-robin the same non-canonical slots.
                let output_slot = 1 + (processed as usize % (NV12_POOL_SIZE - 1));
                unsafe {
                    d3d.immediate_context.CopyResource(
                        &nv12_textures[output_slot],
                        &nv12_textures[NV12_CANONICAL_SLOT],
                    );
                }
            }
        }
        processed += 1;
        recycle_tx
            .send(slot)
            .context("WGC recycle channel disconnected")?;
    }

    let ended = Instant::now();
    let (end_handles, end_census) =
        sample_now(options, journal, stage, "stage_end", &arrived, processed)?;

    control
        .stop()
        .map_err(|error| anyhow::anyhow!("WGC stop failed: {error}"))?;
    lifecycle_sample(options, journal, "after_wgc_stop")?;
    if let Some(encoder) = encoder.take() {
        let finished = encoder.finish()?;
        frames_output += finished.encoded_samples.len() as u64;
        journal.emit(
            "encoder_finished",
            json!({
                "stage": stage.name(),
                "encoded_samples": finished.encoded_samples.len(),
                "frames_submitted": encoded_frames,
                "frames_output": frames_output,
                "effective_encoder": finished.effective_encoder,
                "retained_input_slots": finished.retained_slots,
            }),
        )?;
        ensure!(
            finished.retained_slots.is_empty(),
            "encoder retained input slots at shutdown"
        );
        ensure!(
            frames_output == encoded_frames,
            "encoder output count {frames_output} differs from submitted count {encoded_frames}"
        );
    }
    drop(surfaces);
    drop(converter);
    drop(nv12_textures);
    drop(textures);
    drop(queries);
    drop(frame_rx);
    drop(recycle_tx);
    lifecycle_sample(options, journal, "after_stage_resources_dropped")?;
    thread::sleep(Duration::from_secs(3));
    lifecycle_sample(options, journal, "after_stage_settle")?;

    // Measure the slope over the window that was actually observed, not the
    // nominal one: a stage that ends early must not report an inflated rate.
    let seconds = ended
        .saturating_duration_since(settle_instant)
        .as_secs_f64();
    let window_frames_processed = processed.saturating_sub(processed_at_settle);
    let handle_delta = i64::from(end_handles) - i64::from(settle_handles);
    let type_delta = match (settle_census.as_ref(), end_census.as_ref()) {
        (Some(before), Some(after)) => diff_by_type(&before.by_type, &after.by_type),
        _ => BTreeMap::new(),
    };
    let report = StageReport {
        stage: stage.name(),
        seconds,
        frames_arrived: arrived.load(Ordering::Relaxed),
        frames_processed: processed,
        frames_submitted: encoded_frames,
        frames_output,
        window_frames_processed,
        ring_starved: starved.load(Ordering::Relaxed),
        nv12_starved,
        copy_timeouts,
        settle_handles,
        end_handles,
        handle_delta,
        handles_per_second: if seconds > 0.0 {
            handle_delta as f64 / seconds
        } else {
            0.0
        },
        handles_per_processed_frame: if window_frames_processed > 0 {
            handle_delta as f64 / window_frames_processed as f64
        } else {
            0.0
        },
        type_delta,
        type_delta_complete: matches!((&settle_census, &end_census),
            (Some(before), Some(after)) if !before.incomplete && !after.incomplete),
        measurement_healthy: settled
            && window_frames_processed > 0
            && starved.load(Ordering::Relaxed) == 0
            && nv12_starved == 0
            && copy_timeouts == 0,
    };
    journal.emit("stage_report", serde_json::to_value(&report)?)?;
    Ok(StageOutcome {
        report,
        settle_census,
        end_census,
    })
}

/// Bounded wait for Media Foundation to release an NV12 pool slot.
fn wait_for_nv12_slot(
    encoder: &mut HardwareH264Encoder,
    free_nv12: &mut VecDeque<usize>,
) -> anyhow::Result<Option<usize>> {
    let deadline = Instant::now() + NV12_SLOT_WAIT;
    while Instant::now() < deadline {
        for released in encoder.take_released()? {
            free_nv12.push_back(released as usize);
        }
        if let Some(slot) = free_nv12.pop_front() {
            return Ok(Some(slot));
        }
        thread::sleep(Duration::from_millis(1));
    }
    Ok(None)
}

fn sample_now(
    options: &Options,
    journal: &Journal,
    stage: Stage,
    kind: &str,
    arrived: &AtomicU64,
    processed: u64,
) -> anyhow::Result<(u32, Option<HandleCensus>)> {
    if options.census {
        let census = census::census(options.max_handle_value)?;
        journal.emit(
            kind,
            json!({
                "stage": stage.name(),
                "frames_arrived": arrived.load(Ordering::Relaxed),
                "frames_processed": processed,
                "census": census,
            }),
        )?;
        Ok((census.total, Some(census)))
    } else {
        let total = census::process_handle_count()?;
        journal.emit(
            kind,
            json!({
                "stage": stage.name(),
                "frames_arrived": arrived.load(Ordering::Relaxed),
                "frames_processed": processed,
                "handles": total,
            }),
        )?;
        Ok((total, None))
    }
}

// ---------------------------------------------------------------------------

fn lab_profile(fps: u32) -> CaptureProfile {
    CaptureProfile {
        profile_id: "handle-growth-probe-lab-non-trainable".into(),
        non_trainable: true,
        capture_backend: "native_wgc_window_v1".into(),
        encoder_backend: "mf_nvidia_hardware_v1".into(),
        width: WIDTH,
        height: HEIGHT,
        fps_num: fps,
        fps_den: 1,
        bitrate_bps: 12_000_000,
        max_bitrate_bps: 18_000_000,
        gop_frames: 120,
        segment_seconds: 60,
        test_segments: 1,
        accepted_encoder_relaxations: None,
        audio: None,
    }
}

fn lifecycle_sample(options: &Options, journal: &Journal, phase: &str) -> anyhow::Result<()> {
    if options.census {
        journal.emit(
            "lifecycle",
            json!({"phase":phase,
            "census":census::census(options.max_handle_value)?}),
        )
    } else {
        journal.emit(
            "lifecycle",
            json!({"phase":phase,
            "handles":census::process_handle_count()?}),
        )
    }
}

fn prepare_output(output: &Path) -> anyhow::Result<()> {
    if output.exists() {
        let mut entries =
            fs::read_dir(output).with_context(|| format!("failed to read {}", output.display()))?;
        ensure!(
            entries.next().is_none(),
            "output directory {} already has content; point --output at a new path so \
             earlier evidence is never overwritten",
            output.display()
        );
    }
    fs::create_dir_all(output).with_context(|| format!("failed to create {}", output.display()))?;
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let options = parse_options()?;
    prepare_output(&options.output)?;
    let journal = Arc::new(Journal::create(
        &options.output.join("handle-growth.jsonl"),
    )?);

    // Production recording initializes the calling thread as MTA before MF.
    struct ComGuard;
    impl Drop for ComGuard {
        fn drop(&mut self) {
            unsafe { CoUninitialize() };
        }
    }
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
        .ok()
        .context("CoInitializeEx(MTA) failed")?;
    let _com = ComGuard;
    let profile = lab_profile(options.fps);
    let d3d = D3dContext::create_nvidia_hardware_only()?;
    let _mf = MfPlatform::startup()?;

    journal.emit(
        "header",
        json!({
            "pid": std::process::id(),
            "adapter": d3d.adapter,
            "width": WIDTH,
            "height": HEIGHT,
            "bgra_ring_size": BGRA_RING_SIZE,
            "nv12_pool_size": NV12_POOL_SIZE,
            "stages": options.stages.iter().map(|stage| stage.name()).collect::<Vec<_>>(),
            "stage_seconds": options.stage_seconds,
            "settle_seconds": options.settle_seconds,
            "sample_seconds": options.sample_seconds,
            "fps": options.fps,
            "census": options.census,
        }),
    )?;

    // The window is created on, and pumped by, the thread that owns it.
    let (hwnd_tx, hwnd_rx) = mpsc::channel::<anyhow::Result<usize>>();
    let stop_render = Arc::new(AtomicBool::new(false));
    let render_stop = Arc::clone(&stop_render);
    let render_fps = options.fps;
    let render = thread::Builder::new()
        .name("probe-source".into())
        .spawn(move || -> anyhow::Result<u64> {
            let window = match RenderWindow::create() {
                Ok(window) => {
                    let _ = hwnd_tx.send(Ok(window.hwnd.0 as usize));
                    window
                }
                Err(error) => {
                    let _ = hwnd_tx.send(Err(anyhow::anyhow!("{error:#}")));
                    return Err(error);
                }
            };
            let period = Duration::from_secs_f64(1.0 / f64::from(render_fps));
            let mut next = Instant::now();
            let mut presented = 0_u64;
            while !render_stop.load(Ordering::Acquire) {
                pump_messages();
                window.render(presented)?;
                presented += 1;
                next += period;
                let now = Instant::now();
                if next > now {
                    thread::sleep(next - now);
                } else {
                    next = now;
                }
            }
            Ok(presented)
        })
        .context("failed to start the probe source thread")?;

    let hwnd_value = hwnd_rx
        .recv()
        .context("probe source thread ended before reporting its window")??;

    lifecycle_sample(&options, &journal, "source_only_start")?;
    thread::sleep(Duration::from_secs(8));
    lifecycle_sample(&options, &journal, "source_only_end")?;

    let mut reports = Vec::new();
    let mut censuses = Vec::new();
    let mut failure = None;
    for stage in &options.stages {
        println!(
            "stage {} running for {}s ...",
            stage.name(),
            options.stage_seconds
        );
        match run_stage(*stage, &options, &journal, &d3d, &profile, hwnd_value) {
            Ok(outcome) => {
                println!(
                    "  handles {} -> {} ({:+}) = {:.3}/s, {:.3}/frame",
                    outcome.report.settle_handles,
                    outcome.report.end_handles,
                    outcome.report.handle_delta,
                    outcome.report.handles_per_second,
                    outcome.report.handles_per_processed_frame
                );
                for (name, delta) in &outcome.report.type_delta {
                    println!("    {name:<24} {delta:+}");
                }
                censuses.push(json!({
                    "stage": stage.name(),
                    "settle": outcome.settle_census,
                    "end": outcome.end_census,
                }));
                reports.push(outcome.report);
            }
            Err(error) => {
                journal.emit(
                    "stage_failed",
                    json!({ "stage": stage.name(), "error": format!("{error:#}") }),
                )?;
                failure = Some(error);
                break;
            }
        }
    }

    stop_render.store(true, Ordering::Release);
    let presented = match render.join() {
        Ok(Ok(presented)) => presented,
        Ok(Err(error)) => return Err(error.context("probe source thread failed")),
        Err(_) => bail!("probe source thread panicked"),
    };

    drop(_mf);
    let adapter = d3d.adapter.clone();
    drop(d3d);
    lifecycle_sample(&options, &journal, "after_source_mf_device_shutdown")?;
    thread::sleep(Duration::from_secs(3));
    lifecycle_sample(&options, &journal, "after_shutdown_settle")?;

    let summary = json!({
        "schema": SCHEMA,
        "pid": std::process::id(),
        "adapter": adapter,
        "failure": failure.as_ref().map(|error| format!("{error:#}")),
        "frames_presented": presented,
        "stages": reports,
        "censuses": censuses,
    });
    let summary_path = options.output.join("summary.json");
    let mut summary_file = File::create_new(&summary_path)?;
    summary_file.write_all(&serde_json::to_vec_pretty(&summary)?)?;
    summary_file.sync_all()?;
    journal.emit("footer", json!({ "frames_presented": presented }))?;
    println!("evidence written to {}", options.output.display());

    match failure {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

use std::ffi::c_void;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, ensure};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateSolidBrush, DeleteObject, EndPaint, FillRect, HDC, HGDIOBJ, InvalidateRect,
    PAINTSTRUCT, SetBkMode, SetTextColor, TRANSPARENT, TextOutW, UpdateWindow,
};
use windows::Win32::Media::{timeBeginPeriod, timeEndPeriod};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::Threading::{
    GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_HIGHEST, THREAD_PRIORITY_NORMAL,
};
use windows::Win32::UI::HiDpi::{
    AdjustWindowRectExForDpi, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForSystem,
    SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::Input::{
    GetRawInputData, HRAWINPUT, MOUSE_MOVE_ABSOLUTE, RAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER,
    RID_INPUT, RIM_TYPEMOUSE, RegisterRawInputDevices,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CREATESTRUCTW, CS_HREDRAW, CS_OWNDC, CS_VREDRAW, CreateWindowExW, DefWindowProcW,
    DispatchMessageW, GWLP_USERDATA, GetClientRect, HWND_NOTOPMOST, IDC_ARROW, LoadCursorW, MSG,
    PM_REMOVE, PeekMessageW, PostQuitMessage, RegisterClassW, SW_SHOW, SWP_NOACTIVATE, SWP_NOMOVE,
    SWP_NOSIZE, SetWindowLongPtrW, SetWindowPos, ShowWindow, TranslateMessage, WM_CLOSE,
    WM_DESTROY, WM_INPUT, WM_NCCREATE, WM_PAINT, WM_QUIT, WNDCLASSW, WS_EX_TOPMOST, WS_POPUP,
    WS_VISIBLE,
};
use windows::core::w;

use crate::audio_calibration::ToneSchedule;
use crate::viewscale_scene::{ViewScaleConfig, ViewScaleScene, pillar_screen_positions};

pub struct RenderTelemetry {
    phase: AtomicU8,
    sample_stride: u64,
    baseline_qpc_ticks: Mutex<Vec<i64>>,
    capture_qpc_ticks: Mutex<Vec<i64>>,
    audio_schedule: Mutex<Option<ToneSchedule>>,
    audio_paint_states: Mutex<Vec<RenderedAudioState>>,
    audio_flash_qpc_ticks: Mutex<Vec<i64>>,
}

#[derive(Clone, Copy, Debug)]
struct RenderedAudioState {
    completed_qpc: i64,
    burst_index: Option<u32>,
}

impl RenderTelemetry {
    pub fn begin_preflight(&self) {
        self.phase.store(PHASE_PREFLIGHT, Ordering::Release);
    }

    pub fn begin_capture(&self) -> CapturePhaseGuard<'_> {
        self.phase.store(PHASE_CAPTURE, Ordering::Release);
        CapturePhaseGuard { telemetry: self }
    }

    pub fn snapshot(&self) -> RenderTelemetrySnapshot {
        RenderTelemetrySnapshot {
            baseline_qpc_ticks: self
                .baseline_qpc_ticks
                .lock()
                .expect("baseline render telemetry poisoned")
                .clone(),
            capture_qpc_ticks: self
                .capture_qpc_ticks
                .lock()
                .expect("capture render telemetry poisoned")
                .clone(),
            audio_flash_qpc_ticks: self
                .audio_flash_qpc_ticks
                .lock()
                .expect("audio flash telemetry poisoned")
                .clone(),
        }
    }

    pub fn configure_audio_schedule(&self, schedule: ToneSchedule) {
        *self
            .audio_schedule
            .lock()
            .expect("audio schedule telemetry poisoned") = Some(schedule);
        self.audio_paint_states
            .lock()
            .expect("audio paint telemetry poisoned")
            .clear();
        self.audio_flash_qpc_ticks
            .lock()
            .expect("audio flash telemetry poisoned")
            .clear();
    }

    fn scheduled_audio_burst(&self, qpc: i64) -> Option<u32> {
        self.audio_schedule
            .lock()
            .expect("audio schedule telemetry poisoned")
            .and_then(|schedule| schedule.burst_index_at(qpc))
    }

    fn record_audio_paint(&self, completed_qpc: i64, burst_index: Option<u32>) {
        if self
            .audio_schedule
            .lock()
            .expect("audio schedule telemetry poisoned")
            .is_none()
        {
            return;
        }
        let mut states = self
            .audio_paint_states
            .lock()
            .expect("audio paint telemetry poisoned");
        if states.last().and_then(|state| state.burst_index) == burst_index {
            return;
        }
        states.push(RenderedAudioState {
            completed_qpc,
            burst_index,
        });
        drop(states);
        if burst_index.is_some() {
            self.audio_flash_qpc_ticks
                .lock()
                .expect("audio flash telemetry poisoned")
                .push(completed_qpc);
        }
    }

    pub fn painted_audio_burst_at(&self, source_qpc: i64) -> Option<u32> {
        self.audio_paint_states
            .lock()
            .expect("audio paint telemetry poisoned")
            .iter()
            .rev()
            .find(|state| state.completed_qpc <= source_qpc)
            .and_then(|state| state.burst_index)
    }

    fn record(&self, qpc: i64, source_frame: u64) {
        if !source_frame.is_multiple_of(self.sample_stride) {
            return;
        }
        let target = match self.phase.load(Ordering::Acquire) {
            PHASE_BASELINE => &self.baseline_qpc_ticks,
            PHASE_CAPTURE => &self.capture_qpc_ticks,
            _ => return,
        };
        target.lock().expect("render telemetry poisoned").push(qpc);
    }
}

impl Default for RenderTelemetry {
    fn default() -> Self {
        Self {
            phase: AtomicU8::new(PHASE_BASELINE),
            sample_stride: 1,
            baseline_qpc_ticks: Mutex::new(Vec::new()),
            capture_qpc_ticks: Mutex::new(Vec::new()),
            audio_schedule: Mutex::new(None),
            audio_paint_states: Mutex::new(Vec::new()),
            audio_flash_qpc_ticks: Mutex::new(Vec::new()),
        }
    }
}

pub struct RenderTelemetrySnapshot {
    pub baseline_qpc_ticks: Vec<i64>,
    pub capture_qpc_ticks: Vec<i64>,
    pub audio_flash_qpc_ticks: Vec<i64>,
}

pub struct CapturePhaseGuard<'a> {
    telemetry: &'a RenderTelemetry,
}

impl Drop for CapturePhaseGuard<'_> {
    fn drop(&mut self) {
        self.telemetry.phase.store(PHASE_DONE, Ordering::Release);
    }
}

const PHASE_BASELINE: u8 = 0;
const PHASE_PREFLIGHT: u8 = 1;
const PHASE_CAPTURE: u8 = 2;
const PHASE_DONE: u8 = 3;

struct WindowState {
    frame: u64,
    telemetry: Arc<RenderTelemetry>,
    /// `--view-scale` 模式的合成相机场景；仅消息循环线程访问。
    view_scale: Option<ViewScaleScene>,
}

pub struct SyntheticWindow {
    pub hwnd: HWND,
    pub telemetry: Arc<RenderTelemetry>,
    profile_frame_period_qpc: i64,
    stress_frame_period_qpc: i64,
    qpc_frequency: i64,
}

struct SyntheticRenderPriority;

impl SyntheticRenderPriority {
    fn enter() -> anyhow::Result<Self> {
        // This raises only the lab's synthetic game/UI thread, not any
        // collector thread. Real games commonly protect their render loop from
        // ordinary desktop scheduling jitter; the P0 source must not invent a
        // missing frame and then blame the collector for duplicating it.
        unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST) }
            .context("failed to raise synthetic render thread priority")?;
        Ok(Self)
    }
}

impl Drop for SyntheticRenderPriority {
    fn drop(&mut self) {
        let _ = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_NORMAL) };
    }
}

impl SyntheticWindow {
    pub fn create(
        width: u32,
        height: u32,
        fps_num: u32,
        fps_den: u32,
        stress_rate_multiplier: u32,
        view_scale: Option<ViewScaleConfig>,
    ) -> anyhow::Result<Self> {
        ensure!(
            stress_rate_multiplier > 0,
            "synthetic stress rate must be positive"
        );
        unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) }
            .context("failed to enable per-monitor-v2 DPI awareness")?;
        ensure!(
            unsafe { timeBeginPeriod(1) } == 0,
            "timeBeginPeriod(1) failed"
        );

        let module = unsafe { GetModuleHandleW(None) }.context("GetModuleHandleW failed")?;
        let instance = HINSTANCE(module.0);
        let class = w!("DataMinerCaptureLabSyntheticWindow");
        let cursor = unsafe { LoadCursorW(None, IDC_ARROW) }.context("LoadCursorW failed")?;
        let wnd_class = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW | CS_OWNDC,
            lpfnWndProc: Some(window_proc),
            hInstance: instance,
            hCursor: cursor,
            lpszClassName: class,
            ..Default::default()
        };
        ensure!(
            unsafe { RegisterClassW(&wnd_class) } != 0,
            "RegisterClassW failed"
        );

        // A decorated window is capped to the monitor work area on a
        // 1920x1080 desktop, shrinking its client area by the taskbar height.
        // A borderless popup matches a fullscreen game and preserves the exact
        // physical client geometry required by the P0 profile.
        let style = WS_POPUP | WS_VISIBLE;
        let ex_style = WS_EX_TOPMOST;
        let mut outer = RECT {
            left: 0,
            top: 0,
            right: width as i32,
            bottom: height as i32,
        };
        unsafe { AdjustWindowRectExForDpi(&mut outer, style, false, ex_style, GetDpiForSystem()) }
            .context("AdjustWindowRectExForDpi failed")?;

        let mut qpc_frequency = 0_i64;
        unsafe { QueryPerformanceFrequency(&mut qpc_frequency) }
            .context("QueryPerformanceFrequency failed")?;

        let telemetry = Arc::new(RenderTelemetry {
            sample_stride: u64::from(stress_rate_multiplier),
            ..RenderTelemetry::default()
        });
        let wants_raw_input = view_scale.is_some();
        let state = Box::new(WindowState {
            frame: 0,
            telemetry: Arc::clone(&telemetry),
            view_scale: view_scale.map(|config| ViewScaleScene::new(config, qpc_frequency)),
        });
        let state_ptr = Box::into_raw(state);
        let hwnd = unsafe {
            CreateWindowExW(
                ex_style,
                class,
                w!("DataMiner P0 Synthetic Game - NON-TRAINABLE"),
                style,
                0,
                0,
                outer.right - outer.left,
                outer.bottom - outer.top,
                None,
                None,
                Some(instance),
                Some(state_ptr.cast::<c_void>()),
            )
        };
        let hwnd = match hwnd {
            Ok(hwnd) => hwnd,
            Err(error) => {
                unsafe { drop(Box::from_raw(state_ptr)) };
                return Err(error).context("CreateWindowExW failed");
            }
        };
        let _ = unsafe { ShowWindow(hwnd, SW_SHOW) };
        if wants_raw_input {
            // 与真实游戏同构：普通（非 INPUTSINK）注册，仅前台时接收——
            // 标定/对表时该窗口本来就必须在前台被瞄准。
            let devices = [RAWINPUTDEVICE {
                usUsagePage: 0x01,
                usUsage: 0x02,
                dwFlags: Default::default(),
                hwndTarget: hwnd,
            }];
            unsafe {
                RegisterRawInputDevices(&devices, std::mem::size_of::<RAWINPUTDEVICE>() as u32)
            }
            .context("RegisterRawInputDevices(mouse) failed for --view-scale")?;
        }
        let profile_frame_period_qpc =
            (i128::from(qpc_frequency) * i128::from(fps_den) / i128::from(fps_num)) as i64;
        let stress_frame_period_qpc = profile_frame_period_qpc / i64::from(stress_rate_multiplier);
        ensure!(
            stress_frame_period_qpc > 0,
            "synthetic stress frame period rounded to zero QPC ticks"
        );
        ensure!(
            unsafe { InvalidateRect(Some(hwnd), None, false) }.as_bool(),
            "initial InvalidateRect failed"
        );
        Ok(Self {
            hwnd,
            telemetry,
            profile_frame_period_qpc,
            stress_frame_period_qpc,
            qpc_frequency,
        })
    }

    pub fn allow_normal_z_order(&self) -> anyhow::Result<()> {
        unsafe {
            SetWindowPos(
                self.hwnd,
                Some(HWND_NOTOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            )
        }
        .context("failed to remove synthetic window topmost style")
    }

    pub fn run_message_loop(self) -> anyhow::Result<Arc<RenderTelemetry>> {
        let _priority = SyntheticRenderPriority::enter()?;
        let mut message = MSG::default();
        let mut next_frame = 0_i64;
        unsafe { QueryPerformanceCounter(&mut next_frame) }
            .context("QueryPerformanceCounter failed")?;
        let mut observed_phase = self.telemetry.phase.load(Ordering::Acquire);
        loop {
            while unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
                if message.message == WM_QUIT {
                    let _ = unsafe { timeEndPeriod(1) };
                    return Ok(self.telemetry);
                }
                unsafe {
                    let _ = TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            }

            let mut now = 0_i64;
            unsafe { QueryPerformanceCounter(&mut now) }
                .context("QueryPerformanceCounter failed")?;
            let phase = self.telemetry.phase.load(Ordering::Acquire);
            let frame_period_qpc = if matches!(phase, PHASE_BASELINE | PHASE_CAPTURE) {
                self.stress_frame_period_qpc
            } else {
                self.profile_frame_period_qpc
            };
            if phase != observed_phase {
                // Preflight deliberately returns to profile cadence so WGC can
                // establish its texture/query barrier before the bounded queue
                // receives the stress source. Baseline and capture both use
                // the same stress cadence for a fair performance comparison.
                next_frame = now + frame_period_qpc;
                observed_phase = phase;
            }
            if now >= next_frame {
                let _ = unsafe { InvalidateRect(Some(self.hwnd), None, false) };
                let _ = unsafe { UpdateWindow(self.hwnd) };
                next_frame += frame_period_qpc;
                if next_frame <= now {
                    next_frame = now + frame_period_qpc;
                }
            } else {
                let seconds = (next_frame - now) as f64 / self.qpc_frequency as f64;
                std::thread::sleep(Duration::from_secs_f64(seconds.min(0.002)));
            }
        }
    }
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_NCCREATE {
        let create = unsafe { &*(lparam.0 as *const CREATESTRUCTW) };
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize) };
        return LRESULT(1);
    }
    let state_ptr =
        unsafe { windows::Win32::UI::WindowsAndMessaging::GetWindowLongPtrW(hwnd, GWLP_USERDATA) }
            as *mut WindowState;

    match message {
        WM_INPUT if !state_ptr.is_null() => {
            let state = unsafe { &mut *state_ptr };
            // 未注册（无场景）时 WM_INPUT 根本不会到达，这里的解析不空跑。
            if let (Some(scene), Some(dx)) =
                (state.view_scale.as_mut(), read_relative_mouse_dx(lparam))
            {
                let mut qpc = 0_i64;
                if unsafe { QueryPerformanceCounter(&mut qpc) }.is_ok() {
                    scene.ingest_relative(qpc, dx);
                    let _ = unsafe { InvalidateRect(Some(hwnd), None, false) };
                }
            }
            // WM_INPUT 要求转交 DefWindowProc 完成系统侧清理。
            unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
        }
        WM_PAINT if !state_ptr.is_null() => {
            let state = unsafe { &mut *state_ptr };
            state.frame += 1;
            let mut qpc = 0_i64;
            let audio_burst = if unsafe { QueryPerformanceCounter(&mut qpc) }.is_ok() {
                state.telemetry.record(qpc, state.frame);
                state.telemetry.scheduled_audio_burst(qpc)
            } else {
                None
            };
            let painted = unsafe {
                paint(
                    hwnd,
                    state.frame,
                    state.view_scale.as_ref(),
                    audio_burst.is_some(),
                )
            };
            if painted && unsafe { QueryPerformanceCounter(&mut qpc) }.is_ok() {
                state.telemetry.record_audio_paint(qpc, audio_burst);
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            let _ = unsafe { windows::Win32::UI::WindowsAndMessaging::DestroyWindow(hwnd) };
            LRESULT(0)
        }
        WM_DESTROY => {
            if !state_ptr.is_null() {
                unsafe {
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                    drop(Box::from_raw(state_ptr));
                }
            }
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

/// 从 WM_INPUT 提取相对鼠标位移；绝对模式（触摸板/远程桌面注入）拒收，
/// 与采集端的 Raw Input 纪律一致。
fn read_relative_mouse_dx(lparam: LPARAM) -> Option<i32> {
    let mut size = 0_u32;
    let header_size = std::mem::size_of::<RAWINPUTHEADER>() as u32;
    unsafe {
        GetRawInputData(
            HRAWINPUT(lparam.0 as *mut c_void),
            RID_INPUT,
            None,
            &mut size,
            header_size,
        )
    };
    if size == 0 || size as usize > std::mem::size_of::<RAWINPUT>() {
        return None;
    }
    let mut raw = RAWINPUT::default();
    let copied = unsafe {
        GetRawInputData(
            HRAWINPUT(lparam.0 as *mut c_void),
            RID_INPUT,
            Some((&mut raw as *mut RAWINPUT).cast::<c_void>()),
            &mut size,
            header_size,
        )
    };
    if copied == u32::MAX || copied == 0 || raw.header.dwType != RIM_TYPEMOUSE.0 {
        return None;
    }
    let mouse = unsafe { raw.data.mouse };
    if (mouse.usFlags.0 & MOUSE_MOVE_ABSOLUTE.0) != 0 {
        return None;
    }
    Some(mouse.lLastX)
}

unsafe fn paint(
    hwnd: HWND,
    frame: u64,
    view_scale: Option<&ViewScaleScene>,
    audio_flash: bool,
) -> bool {
    let mut paint = PAINTSTRUCT::default();
    let dc = unsafe { BeginPaint(hwnd, &mut paint) };
    let mut client = RECT::default();
    let _ = unsafe { GetClientRect(hwnd, &mut client) };

    if let Some(scene) = view_scale {
        unsafe { paint_view_scale(dc, &client, scene) };
        return unsafe { EndPaint(hwnd, &paint) }.as_bool();
    }

    if audio_flash {
        let flash = unsafe { CreateSolidBrush(rgb(255, 255, 255)) };
        let filled = unsafe { FillRect(dc, &client, flash) } != 0;
        let _ = unsafe { DeleteObject(HGDIOBJ(flash.0)) };
        let ended = unsafe { EndPaint(hwnd, &paint) }.as_bool();
        return filled && ended;
    }

    let phase = (frame % 255) as u8;
    let background = unsafe { CreateSolidBrush(rgb(8, 12 + phase / 16, 28)) };
    unsafe { FillRect(dc, &client, background) };
    let _ = unsafe { DeleteObject(HGDIOBJ(background.0)) };

    let band_width = ((client.right - client.left) / 8).max(1);
    for x in 0..8 {
        let rect = RECT {
            left: x * band_width,
            top: client.bottom / 3,
            right: (x + 1) * band_width,
            bottom: client.bottom * 2 / 3,
        };
        let brush = unsafe {
            CreateSolidBrush(rgb(
                (20 + x * 25) as u8,
                (180 - x * 13) as u8,
                phase.wrapping_add((x * 19) as u8),
            ))
        };
        unsafe { FillRect(dc, &rect, brush) };
        let _ = unsafe { DeleteObject(HGDIOBJ(brush.0)) };
    }

    let marker_size = 128;
    let marker = RECT {
        left: 32,
        top: 32,
        right: 32 + marker_size,
        bottom: 32 + marker_size,
    };
    let marker_brush = unsafe {
        CreateSolidBrush(if frame.is_multiple_of(2) {
            rgb(255, 255, 255)
        } else {
            rgb(255, 32, 32)
        })
    };
    unsafe { FillRect(dc, &marker, marker_brush) };
    let _ = unsafe { DeleteObject(HGDIOBJ(marker_brush.0)) };

    let label: Vec<u16> = format!("DataMiner P0  frame={frame:08}  NON-TRAINABLE")
        .encode_utf16()
        .collect();
    let _ = unsafe { SetBkMode(dc, TRANSPARENT) };
    let _ = unsafe { SetTextColor(dc, rgb(255, 235, 80)) };
    let _ = unsafe { TextOutW(dc, 190, 64, &label) };
    unsafe { EndPaint(hwnd, &paint) }.as_bool()
}

/// 全景标定场景：深色背景 + 地平线 + 每 30° 参照柱（标方位角，0° 柱
/// 高亮作对表起点）+ 中心准星 + HUD（真值/劣化开关自报 + 当前方位）。
/// 全部几何来自 `viewscale_scene` 的纯函数，本函数只负责 GDI 落笔。
unsafe fn paint_view_scale(dc: HDC, client: &RECT, scene: &ViewScaleScene) {
    let width = client.right - client.left;
    let height = client.bottom - client.top;

    let background = unsafe { CreateSolidBrush(rgb(10, 14, 22)) };
    unsafe { FillRect(dc, client, background) };
    let _ = unsafe { DeleteObject(HGDIOBJ(background.0)) };

    let horizon = RECT {
        left: 0,
        top: height / 2 - 1,
        right: width,
        bottom: height / 2 + 1,
    };
    let horizon_brush = unsafe { CreateSolidBrush(rgb(70, 80, 100)) };
    unsafe { FillRect(dc, &horizon, horizon_brush) };
    let _ = unsafe { DeleteObject(HGDIOBJ(horizon_brush.0)) };

    let _ = unsafe { SetBkMode(dc, TRANSPARENT) };
    for (x, azimuth) in pillar_screen_positions(scene.yaw_deg(), scene.hfov_deg(), width, 30.0) {
        let color = if azimuth == 0 {
            rgb(255, 200, 40)
        } else {
            rgb(120, 190, 230)
        };
        let pillar = RECT {
            left: (x - 3).max(0),
            top: height / 4,
            right: (x + 3).min(width),
            bottom: height * 3 / 4,
        };
        let brush = unsafe { CreateSolidBrush(color) };
        unsafe { FillRect(dc, &pillar, brush) };
        let _ = unsafe { DeleteObject(HGDIOBJ(brush.0)) };
        let label: Vec<u16> = format!("{azimuth}").encode_utf16().collect();
        let _ = unsafe { SetTextColor(dc, color) };
        let _ = unsafe { TextOutW(dc, (x - 12).max(0), height / 4 - 24, &label) };
    }

    let cross_brush = unsafe { CreateSolidBrush(rgb(255, 64, 64)) };
    let cross_v = RECT {
        left: width / 2 - 1,
        top: height / 2 - 12,
        right: width / 2 + 1,
        bottom: height / 2 + 12,
    };
    let cross_h = RECT {
        left: width / 2 - 12,
        top: height / 2 - 1,
        right: width / 2 + 12,
        bottom: height / 2 + 1,
    };
    unsafe { FillRect(dc, &cross_v, cross_brush) };
    unsafe { FillRect(dc, &cross_h, cross_brush) };
    let _ = unsafe { DeleteObject(HGDIOBJ(cross_brush.0)) };

    let _ = unsafe { SetTextColor(dc, rgb(255, 235, 80)) };
    let config_line: Vec<u16> = scene.hud_config_line().encode_utf16().collect();
    let _ = unsafe { TextOutW(dc, 16, 16, &config_line) };
    let status_line: Vec<u16> = scene.hud_status_line().encode_utf16().collect();
    let _ = unsafe { TextOutW(dc, 16, 40, &status_line) };
    let _ = unsafe { SetTextColor(dc, rgb(255, 120, 120)) };
    let badge: Vec<u16> = "SYNTHETIC VIEW-SCALE SCENE - NON-TRAINABLE"
        .encode_utf16()
        .collect();
    let _ = unsafe { TextOutW(dc, 16, height - 32, &badge) };
}

const fn rgb(red: u8, green: u8, blue: u8) -> COLORREF {
    COLORREF(red as u32 | ((green as u32) << 8) | ((blue as u32) << 16))
}

#[cfg(test)]
mod audio_tests {
    use super::*;

    #[test]
    fn painted_burst_lookup_requires_a_completed_paint_transition() {
        let telemetry = RenderTelemetry::default();
        telemetry.configure_audio_schedule(ToneSchedule {
            submit_qpc: 0,
            first_burst_qpc: 100,
            period_qpc: 1_000,
            burst_duration_qpc: 50,
            burst_count: 1,
        });

        assert_eq!(telemetry.painted_audio_burst_at(120), None);
        telemetry.record_audio_paint(125, Some(0));
        assert_eq!(telemetry.painted_audio_burst_at(124), None);
        assert_eq!(telemetry.painted_audio_burst_at(125), Some(0));
        telemetry.record_audio_paint(151, None);
        assert_eq!(telemetry.painted_audio_burst_at(150), Some(0));
        assert_eq!(telemetry.painted_audio_burst_at(151), None);
        assert_eq!(telemetry.snapshot().audio_flash_qpc_ticks, vec![125]);
    }
}

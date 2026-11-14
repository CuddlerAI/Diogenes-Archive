//! 标定用 Raw Input 探针（docs/46 §4.1 / C3）：录制之外的独立鼠标相对
//! 计数通道，供视角标定服务在对表期间逐事件累计 `dx/dy + QPC`。
//!
//! 与录制链路的互斥（务必遵守）：`RegisterRawInputDevices` 是进程级、
//! 按 usage 的注册，后注册者会顶掉先注册者的投递目标。因此本探针绝不
//! 允许与录制监视（window_tracker）同时活跃——服务层在编排器非
//! Idle/Bound 状态时拒绝启动探针，C4 的 preflight 再从录制侧加反向硬门。
//! 探针只注册鼠标（usage 0x02）、不碰键盘、不写任何录制事实；队列溢出
//! 只计数并由服务把当次测量作废，与录制的 fail-closed 语义对齐。

use std::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::JoinHandle;

use anyhow::Context;
use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Performance::QueryPerformanceCounter;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::{
    HRAWINPUT, MOUSE_MOVE_ABSOLUTE, RAWINPUT, RAWINPUTDEVICE, RIDEV_INPUTSINK, RIDEV_REMOVE,
    RIM_TYPEMOUSE, RegisterRawInputDevices,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, HWND_MESSAGE,
    MSG, PostThreadMessageW, TranslateMessage, WINDOW_EX_STYLE, WINDOW_STYLE, WM_INPUT, WM_QUIT,
};
use windows::core::w;

use super::raw_input_reader::{CapturedRawInput, RawInputReader};

/// 探针队列容量。对表测量的事件率上限约为回报率（≤8kHz 级），65536
/// 提供数秒级缓冲；溢出即计数并由服务作废当次测量，不静默丢失。
const PROBE_QUEUE_CAPACITY: usize = 65_536;

/// 一条探针事件：相对计数 + 到达时刻 QPC + 设备句柄值（仅用于同一次
/// 对表内的多鼠标检测，不落盘、不跨会话追踪）。
#[derive(Debug, Clone, Copy)]
pub struct ProbeEvent {
    pub qpc: i64,
    pub dx: i32,
    pub dy: i32,
    pub device: isize,
    pub buffered: bool,
    pub delivery_batch_size: u32,
}

/// 独立线程上的鼠标 Raw Input 探针；Drop 时停线程并反注册。
pub struct RawInputProbe {
    thread: Option<JoinHandle<()>>,
    thread_id: u32,
    receiver: Receiver<ProbeEvent>,
    overflows: Arc<AtomicU64>,
}

impl RawInputProbe {
    /// 启动探针线程并完成注册；注册失败原样返回错误（fail-closed）。
    pub fn start() -> anyhow::Result<Self> {
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<anyhow::Result<u32>>();
        let (event_tx, event_rx) = bounded::<ProbeEvent>(PROBE_QUEUE_CAPACITY);
        let overflows = Arc::new(AtomicU64::new(0));
        let thread_overflows = Arc::clone(&overflows);
        let thread = std::thread::Builder::new()
            .name("dm-view-scale-probe".into())
            .spawn(move || probe_thread(&ready_tx, &event_tx, &thread_overflows))
            .context("failed to spawn the view-scale raw input probe thread")?;
        let thread_id = ready_rx
            .recv()
            .context("view-scale probe thread exited before reporting readiness")??;
        Ok(Self {
            thread: Some(thread),
            thread_id,
            receiver: event_rx,
            overflows,
        })
    }

    #[must_use]
    pub fn receiver(&self) -> &Receiver<ProbeEvent> {
        &self.receiver
    }

    /// 自启动以来的队列溢出计数；非零意味着期间的测量不可信。
    #[must_use]
    pub fn overflow_count(&self) -> u64 {
        self.overflows.load(Ordering::Acquire)
    }
}

impl Drop for RawInputProbe {
    fn drop(&mut self) {
        let _ = unsafe {
            PostThreadMessageW(
                self.thread_id,
                WM_QUIT,
                Default::default(),
                Default::default(),
            )
        };
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn probe_thread(
    ready: &std::sync::mpsc::Sender<anyhow::Result<u32>>,
    events: &Sender<ProbeEvent>,
    overflows: &AtomicU64,
) {
    let sink = match create_probe_sink().and_then(|sink| {
        register_probe_mouse(sink)?;
        Ok(sink)
    }) {
        Ok(sink) => sink,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    let _ = ready.send(Ok(unsafe { GetCurrentThreadId() }));

    let mut reader = RawInputReader::new();
    let mut packets = Vec::new();
    let mut message = MSG::default();
    while unsafe { GetMessageW(&mut message, None, 0, 0) }.as_bool() {
        if message.message == WM_INPUT {
            let result =
                reader.read_available(HRAWINPUT(message.lParam.0 as *mut c_void), &mut packets);
            let mut qpc = 0_i64;
            if let Err(error) = result {
                tracing::warn!(error = %error, "view-scale Raw Input batch read failed");
                overflows.fetch_add(1, Ordering::AcqRel);
            } else if unsafe { QueryPerformanceCounter(&mut qpc) }.is_err() {
                overflows.fetch_add(1, Ordering::AcqRel);
            } else {
                let batch_size = packets
                    .iter()
                    .filter(|packet| relative_mouse_movement(&packet.raw).is_some())
                    .count();
                let batch_size = u32::try_from(batch_size).unwrap_or(u32::MAX);
                for packet in &packets {
                    if let Some(event) = read_probe_event(*packet, qpc, batch_size)
                        && let Err(TrySendError::Full(_)) = events.try_send(event)
                    {
                        overflows.fetch_add(1, Ordering::AcqRel);
                    }
                }
            }
            let _ = unsafe {
                DefWindowProcW(
                    message.hwnd,
                    message.message,
                    message.wParam,
                    message.lParam,
                )
            };
            continue;
        }
        unsafe {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
    unregister_probe_mouse();
    let _ = unsafe { DestroyWindow(sink) };
}

fn create_probe_sink() -> anyhow::Result<HWND> {
    unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("STATIC"),
            w!("DataMinerViewScaleProbeSink"),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            None,
            None,
        )
    }
    .context("failed to create the view-scale probe message-only window")
}

fn register_probe_mouse(sink: HWND) -> anyhow::Result<()> {
    let devices = [RAWINPUTDEVICE {
        usUsagePage: 0x01,
        usUsage: 0x02,
        dwFlags: RIDEV_INPUTSINK,
        hwndTarget: sink,
    }];
    unsafe { RegisterRawInputDevices(&devices, std::mem::size_of::<RAWINPUTDEVICE>() as u32) }
        .context("RegisterRawInputDevices(mouse) failed for the view-scale probe")?;
    Ok(())
}

fn unregister_probe_mouse() {
    let devices = [RAWINPUTDEVICE {
        usUsagePage: 0x01,
        usUsage: 0x02,
        dwFlags: RIDEV_REMOVE,
        hwndTarget: HWND::default(),
    }];
    let _ =
        unsafe { RegisterRawInputDevices(&devices, std::mem::size_of::<RAWINPUTDEVICE>() as u32) };
}

fn relative_mouse_movement(raw: &RAWINPUT) -> Option<(i32, i32)> {
    if raw.header.dwType != RIM_TYPEMOUSE.0 {
        return None;
    }
    let mouse = unsafe { raw.data.mouse };
    if (mouse.usFlags.0 & MOUSE_MOVE_ABSOLUTE.0) != 0 {
        return None;
    }
    if mouse.lLastX == 0 && mouse.lLastY == 0 {
        return None;
    }
    Some((mouse.lLastX, mouse.lLastY))
}

/// 解析已读取的 Raw Input：仅接受相对鼠标移动；绝对模式（触摸板绝对
/// 注入/远程桌面）和纯按键/滚轮包不参与尺度或回报率测量。
fn read_probe_event(
    packet: CapturedRawInput,
    qpc: i64,
    delivery_batch_size: u32,
) -> Option<ProbeEvent> {
    let (dx, dy) = relative_mouse_movement(&packet.raw)?;
    Some(ProbeEvent {
        qpc,
        dx,
        dy,
        device: packet.raw.header.hDevice.0 as isize,
        buffered: packet.buffered,
        delivery_batch_size,
    })
}

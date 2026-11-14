//! 与截帧共钟的宿主时间戳。
//!
//! # 为什么必须是 QPC，而不是 `render_time` 或 `Instant`
//!
//! `render_time_us` 是**引擎自己的**渲染时钟：它会被 `timer_restart` 重置
//! （见 [`crate::state::FrameRecordV2::timer_epoch`]），也与我们截帧管线的
//! `source_qpc` 没有任何已知的固定关系。拿它去 join 视频帧，只能得到
//! 「看起来对上了」的结果。
//!
//! `std::time::Instant` 在 Windows 上确实建在 QPC 上，但它的原点是**不透明的**，
//! 跨进程不可比——而遥测在游戏进程里、配对表在采集端进程里。
//!
//! 所以这里直接读原始 `QueryPerformanceCounter`：它是系统级单调计数，
//! 两个进程读到的是同一条时间轴，这正是 68 号 §6.5「depth 与 RGB 帧、camera pose
//! 严格对齐」那道硬闸唯一可用的公共键。
//!
//! # 关于「本 crate 一行 Windows API 都没用」
//!
//! 那条纪律指的是**判断逻辑**不碰平台 API（[`crate::state`] 至今仍然如此）。
//! 本模块是明确的例外，且被压到最小：两个函数、只读、无状态、非 Windows 上返回
//! `None`。下游看到 `host_qpc: null` 会拒绝该条记录，**不会**退化成用 `render_time`
//! 硬凑——退不回去正是这里返回 `Option` 而不是 0 的原因。

/// 宿主时钟的一次读数。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostTick {
    /// `QueryPerformanceCounter` 的原始计数。
    pub qpc: i64,
    /// `QueryPerformanceFrequency`，Hz。同一次开机内恒定。
    pub frequency: i64,
}

#[cfg(windows)]
#[must_use]
pub fn read() -> Option<HostTick> {
    use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
    let mut qpc = 0i64;
    let mut frequency = 0i64;
    // SAFETY: 两个 out 参数都是本栈帧上的合法可写 i64。
    unsafe {
        QueryPerformanceCounter(&raw mut qpc).ok()?;
        QueryPerformanceFrequency(&raw mut frequency).ok()?;
    }
    // 0 或负的频率意味着这台机器没有可用的高精度计数器；那种情况下时间戳没有意义，
    // 与其交一个不能 join 的数，不如让下游看见空档。
    if qpc <= 0 || frequency <= 0 {
        return None;
    }
    Some(HostTick { qpc, frequency })
}

/// 非 Windows 上没有 QPC。返回 `None` 而不是伪造一个单调计数器——
/// 一个只在本平台单调的数字无法与采集端的 `source_qpc` 对齐，交出去只会误导。
#[cfg(not(windows))]
#[must_use]
pub const fn read() -> Option<HostTick> {
    None
}

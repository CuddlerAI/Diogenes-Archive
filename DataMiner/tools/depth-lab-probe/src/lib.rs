//! `dm-depth-probe` —— 注入进游戏进程的 DLL。
//!
//! 设计见 [77 号 §5](../../../docs/77-depth-ground-truth-hook-experiment-plan.md)。
//!
//! # 本 crate 刻意保持薄
//!
//! 所有**不需要 D3D 的逻辑**都在 [`depth_lab`] 里，并已在任意平台用测试锁死：
//! DSV 打分（`dsv`）、回读环形池（`readback`）、水印编解码（`watermark`）、
//! profile 校验（`profile`）、线性化与变体派生（`depth`）、逐帧配对（`pairing`）。
//!
//! 这里只剩三类东西：
//!
//! 1. **vtable hook**（[`vtable`]）——唯一必须 `unsafe` 的地方；
//! 2. **把 D3D 的观察喂给 depth-lab 的纯逻辑**；
//! 3. **把 depth-lab 算出的矩形/决定翻译成 D3D 调用**。
//!
//! 这样切分的目的是**归因**：真机上出问题时，能立刻分清是「逻辑错」（本机测试早该抓到）
//! 还是「D3D 交互错」（只能在真机上暴露）。
//!
//! # 当前状态
//!
//! **P1a 已在 BioShock Infinite 真机验证：注入 + Present hook + 动态水印写入。**
//! 设置 `DM_DEPTH_PROBE_DSV_AUDIT=1` 会在游戏的实际 immediate context 上安装
//! `OMSetRenderTargets` / `ClearDepthStencilView` hook，去重记录 DSV 纹理描述，并启用
//! P1c/P1d 的解绑快照、非阻塞回读与异步落盘。Draw 证据与自动排序仍待 P1b 接线。

#![cfg_attr(not(windows), allow(dead_code))]

pub mod vtable;

#[cfg(all(windows, feature = "cp2077-diagnostics", debug_assertions))]
pub(crate) mod cp2077_camera_diagnostics;
#[cfg(windows)]
pub mod cp2077_player_probe;
pub mod cp2077_pose_abi;
/// CP2077 2.31 exact-build camera/player pose discovery admission. This is pure validation and
/// never emits a production binding by itself.
pub mod cp2077_pose_probe;
pub mod cp2077_pose_runtime;
#[cfg(windows)]
pub mod cp2077_pose_wiring;

#[cfg(windows)]
pub mod d3d12_abi;
#[cfg(windows)]
pub mod d3d12_backend;
#[cfg(windows)]
pub mod d3d12_candidate;
#[cfg(windows)]
pub mod d3d12_capture;
#[cfg(windows)]
pub mod d3d12_render_camera;
#[cfg(windows)]
pub mod d3d12_watermark;
#[cfg(windows)]
pub mod descriptor_timing;
#[cfg(windows)]
pub mod dsv_discovery;

/// 落盘。纯文件 IO，与平台无关——因此可以在任意机器上测。
pub mod writer;

/// Camera-only production pose 索引。纯文件 IO，跨平台测试。
pub mod camera_writer;
pub mod player_writer;

#[cfg(windows)]
pub mod bannerlord_player_probe;
#[cfg(windows)]
pub mod camera_probe;
#[cfg(windows)]
pub mod execute_prepare;
pub mod fallout_build;
pub mod fallout_camera_probe;
#[cfg(windows)]
pub mod fallout_player_probe;
#[cfg(windows)]
mod hook_timing;
#[cfg(windows)]
pub mod kcd2_live_pose;
#[cfg(windows)]
pub mod perf;
#[cfg(windows)]
pub mod probe;
#[cfg(windows)]
pub mod raw_depth_demo;
#[cfg(windows)]
mod raw_depth_writer_io;
#[cfg(windows)]
mod startup_timing;
#[cfg(windows)]
pub mod watermark_gpu;

#[cfg(windows)]
pub mod readback;

#[cfg(windows)]
mod dllmain {
    use windows::Win32::Foundation::{HINSTANCE, TRUE};
    use windows::Win32::System::SystemServices::DLL_PROCESS_ATTACH;

    /// DLL 入口。
    ///
    /// **在 `DllMain` 里只做最少的事**：Windows 的 loader lock 在这里是持有的，
    /// 任何加载其他 DLL、创建 D3D 设备、等待线程的操作都可能死锁。
    /// 因此这里只起一个线程，真正的初始化在那个线程里做。
    #[unsafe(no_mangle)]
    pub extern "system" fn DllMain(_dll: HINSTANCE, reason: u32, _reserved: *mut ()) -> i32 {
        if reason == DLL_PROCESS_ATTACH {
            std::thread::spawn(|| {
                if let Err(e) = crate::probe::init() {
                    crate::probe::log(&format!("probe 初始化失败: {e}"));
                }
            });
        }
        TRUE.0
    }
}

#[cfg(not(windows))]
mod non_windows_stub {
    //! 非 Windows 平台上本 crate 是空壳，只保留 [`crate::vtable`] 里那些
    //! 平台无关的索引常量与它们的测试——这样 `cargo test --workspace`
    //! 在 mac/Linux 上仍然可用。
}
